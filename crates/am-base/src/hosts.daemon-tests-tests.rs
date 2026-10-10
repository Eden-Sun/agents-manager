
    use super::*;

    #[tokio::test]
    async fn a_remote_autostart_pass_waits_for_the_api_readiness_transition() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = "remote-autostart-readiness-test";
        app.set_startup_ready(false);
        let pass_app = app.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let pass = tokio::spawn(async move {
            let _ = started_tx.send(());
            pass_app.wait_until_startup_ready().await;
            crate::runners::reconcile::autostart_after_reconcile(&pass_app, host, true).await
        });

        started_rx.await.unwrap();
        assert!(!pass.is_finished(), "remote autostart must wait while API-facing startup work is unavailable");
        assert!(!app.autostart_hosts.lock().unwrap().contains_key(host), "pass must not claim before ready");

        app.set_startup_ready(true);
        assert!(pass.await.unwrap(), "once ready, a successful reconcile should run the host pass");
        assert_eq!(app.autostart_hosts.lock().unwrap().get(host), Some(&crate::state::AutostartHostStatus::Done));
    }

    /// A captured old host must not publish its state or combine it with the replacement's tools cache.
    #[tokio::test]
    async fn a_repointed_host_cannot_publish_the_old_connection_snapshot() {
        let env = crate::testing::env().await;
        let host = "host-changed-fence-test";
        let cfg = |ssh: &str| HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
        };
        let old = env.app.hosts.insert_remote_for_test(cfg("target-a")).await;
        *old.error.lock().await = Some("server A error".into());
        let old_fence = env.app.hosts.fence(host).await.unwrap();
        let app = env.app.clone();
        let stale_fence = old_fence.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (publish_tx, publish_rx) = tokio::sync::oneshot::channel();
        let stale_emit = tokio::spawn(async move {
            let _ = ready_tx.send(());
            let _ = publish_rx.await;
            crate::state::emit_host_changed(&app, &stale_fence).await;
        });
        ready_rx.await.unwrap();

        let new = env.app.hosts.replace_remote_for_test(&env.app, cfg("target-b")).await;
        new.connected.store(true, Ordering::SeqCst);
        env.app.tools.lock().await.insert(
            host.into(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![],
                utc_offset_secs: None,
                herdr_cli: Some("herdr 0.9.7".into()),
                checked_at: crate::db::now(),
            },
        );
        let mut events = env.app.subscribe();

        // Replacement wins before publish; A's connected/error must not be paired with B's tools.
        publish_tx.send(()).unwrap();
        stale_emit.await.unwrap();
        assert!(
            matches!(events.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
            "the superseded A connection must publish no host_changed or daemon_status event"
        );

        let new_fence = env.app.hosts.fence(host).await.unwrap();
        crate::state::emit_host_changed(&env.app, &new_fence).await;
        let current = events.try_recv().unwrap();
        assert_eq!(current.kind, "host_changed");
        assert_eq!(current.data["name"], host);
        assert_eq!(current.data["connected"], true);
        assert_eq!(current.data["error"], serde_json::Value::Null);
        assert_eq!(current.data["herdr"]["cli_version"], "0.9.7");
    }

    /// 主機移除／換連線時 `forget_host_observations` 丟掉 `<host>/…` 的額度，但沒告訴前端：額度條在下一次 5 分鐘輪詢以前
    /// 還顯示著那台機器的數字（換連線時甚至是另一台機器的）。每個被丟掉的 key 要發一則 `quota_updated`（`quota:null`），
    /// 跟 `identity_kind::cleanup_host` 同一個形狀。
    #[tokio::test]
    async fn forgetting_a_hosts_quota_tells_the_clients() {
        let env = crate::testing::env().await;
        let host = "quota-forget-test";
        let key = format!("{host}/claude");
        env.app.quotas.lock().await.insert(
            key.clone(),
            crate::quota::Quota {
                five_hour: None,
                seven_day: None,
                fable: None,
                reset_credits: None,
                limit_hit: None,
                plan: None,
                updated_at: crate::db::now(),
                source: "test".into(),
                account: None,
                host: host.into(),
            },
        );
        let mut events = env.app.subscribe();
        <crate::state::App as HostHooks>::forget_host_observations(&env.app, host).await;
        assert!(env.app.quotas.lock().await.get(&key).is_none(), "前提：額度已丟掉");
        let mut seen = None;
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "quota_updated" && ev.data["kind"] == key.as_str() {
                seen = Some(ev);
            }
        }
        let ev = seen.expect("丟掉的額度 key 要發 quota_updated");
        assert!(ev.data["quota"].is_null(), "{}", ev.data);
        assert_eq!(ev.data["host"], host);
    }

    #[tokio::test]
    async fn xreview_forgetting_a_host_also_drops_its_baseline_and_stale_shim_incident() {
        let env = crate::testing::env().await;
        let host = "forget-observations-test";
        env.app.host_baseline.lock().await.insert(
            host.into(),
            crate::host_baseline::BaselineReport {
                os: Some("Linux".into()),
                issues: Some(vec![]),
                checked_at: crate::db::now(),
                failed_at: None,
                error: None,
                stale: false,
            },
        );
        env.app
            .remote_shim_stale
            .lock()
            .await
            .insert(host.into(), "old host shim could not be refreshed".into());
        env.app
            .remote_shim_stale
            .lock()
            .await
            .insert("other-host".into(), "keep this other host".into());

        <crate::state::App as HostHooks>::forget_host_observations(&env.app, host).await;

        assert!(
            !env.app.host_baseline.lock().await.contains_key(host),
            "a same-name replacement must not inherit another machine's baseline"
        );
        let stale = env.app.remote_shim_stale.lock().await;
        assert!(
            !stale.contains_key(host),
            "removed hosts must not keep reporting stale shim incidents"
        );
        assert_eq!(
            stale.get("other-host").map(String::as_str),
            Some("keep this other host")
        );
    }

    #[tokio::test]
    async fn a_fenced_host_operation_holds_its_authority_until_completion() {
        let env = crate::testing::env().await;
        let host = "authority-gate-test";
        let cfg = HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: "target-a".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
        };
        env.app.hosts.insert_remote_for_test(cfg).await;
        let fence = env.app.hosts.fence(host).await.unwrap();
        let app = env.app.clone();
        let fence_for_task = fence.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            app.hosts
                .run_if_current(&fence_for_task, async move {
                    let _ = entered_tx.send(());
                    let _ = release_rx.await;
                    "original authority"
                })
                .await
        });
        entered_rx.await.unwrap();
        assert!(fence.authority_gate_for_test().try_write().is_err(), "repoint/reconnect must wait while the RPC is in flight");
        release_tx.send(()).unwrap();
        assert_eq!(task.await.unwrap(), Some("original authority"));
        assert!(fence.authority_gate_for_test().try_write().is_ok(), "the authority gate releases after the operation");
    }

    #[test]
    fn remote_path_is_one_quoted_path_entry() {
        assert_eq!(remote_path_prefix(""), "");
        assert_eq!(remote_path_prefix("  "), "");
        assert_eq!(remote_path_prefix("/opt/x/bin"), "export PATH='/opt/x/bin':\"$PATH\"\n");
        // A space no longer splits the export; a `;` or `$(…)` is data, not a command.
        let p = remote_path_prefix("/Users/me/my tools/bin;$(touch /tmp/pwned)");
        assert_eq!(p, "export PATH='/Users/me/my tools/bin;$(touch /tmp/pwned)':\"$PATH\"\n");
    }

    /// #241：`$HOME/.local/bin` 以前整串單引號、字面 `$HOME` 進 PATH，遠端找不到 herdr。
    #[test]
    fn remote_path_dirs_expand_only_a_leading_home() {
        assert!(remote_path_dirs("", "/home/u").is_empty());
        assert_eq!(
            remote_path_dirs("/opt/homebrew/bin:$HOME/.local/bin:${HOME}/b:~/c:~", "/home/u"),
            ["/opt/homebrew/bin", "/home/u/.local/bin", "/home/u/b", "/home/u/c", "/home/u"]
        );
        assert_eq!(remote_path_dirs("/x/$HOME/bin:$HOMEX/bin:~u/bin", "/home/u"), ["/x/$HOME/bin", "$HOMEX/bin", "~u/bin"]);
    }

    #[test]
    fn remote_path_expands_a_leading_home_and_splits_on_colon() {
        assert_eq!(remote_path_prefix("$HOME/.local/bin"), "export PATH=\"$HOME\"'/.local/bin':\"$PATH\"\n");
        assert_eq!(remote_path_prefix("${HOME}/bin"), "export PATH=\"$HOME\"'/bin':\"$PATH\"\n");
        assert_eq!(remote_path_prefix("~/bin"), "export PATH=\"$HOME\"'/bin':\"$PATH\"\n");
        assert_eq!(
            remote_path_prefix("/opt/homebrew/bin:$HOME/.local/bin"),
            "export PATH='/opt/homebrew/bin':\"$HOME\"'/.local/bin':\"$PATH\"\n"
        );
        // `$HOME` 只在項目開頭展開；其他 `$` 仍是資料。
        assert_eq!(remote_path_prefix("/x/$HOME/bin"), "export PATH='/x/$HOME/bin':\"$PATH\"\n");
        assert_eq!(remote_path_prefix("$HOMEX/bin"), "export PATH='$HOMEX/bin':\"$PATH\"\n");
        let p = remote_path_prefix("$HOME/a;$(touch /tmp/pwned)");
        assert_eq!(p, "export PATH=\"$HOME\"'/a;$(touch /tmp/pwned)':\"$PATH\"\n");
    }

    /// 兩個 daemon 實例（正式＋隔離，或兩個資料目錄不同的隔離實例）以前共用同一個
    /// `/tmp/agents-manager-<uid>`：管同一台遠端主機時會撞同一個 ctl／sock，一邊
    /// `kill_master`／explicit reconnect 會把另一邊的隧道也斷掉（issue #85）。
    #[test]
    fn short_dir_is_namespaced_by_instance() {
        let production = short_dir(None);
        let iso_a = short_dir(Some("a1b2c3d4e5f6a7b8"));
        let iso_b = short_dir(Some("00112233445566ff"));
        assert_ne!(production, iso_a, "正式實例跟隔離實例不能共用同一個目錄");
        assert_ne!(iso_a, iso_b, "兩個不同的隔離實例不能撞同一個目錄");
        assert_eq!(short_dir(Some("a1b2c3d4e5f6a7b8")), iso_a, "同一個實例重複呼叫要拿到同一條路徑");
    }

    /// `instance_slug()` 固定 16 個 hex 字元、host name 上限 32（`config::valid_host_name`）；
    /// 兩者疊到 `short_dir` 之後仍要留在 macOS AF_UNIX 的長度上限內——`start_master` 對
    /// forwarded socket 路徑超過 100 bytes 會直接 bail，不是等 ssh 自己失敗。
    #[test]
    fn a_max_length_host_name_with_an_instance_slug_still_fits_af_unix() {
        let slug = "0123456789abcdef";
        let host_name = "a".repeat(32);
        let dir = short_dir(Some(slug));
        for suffix in ["ctl", "sock"] {
            let p = dir.join(format!("{host_name}.{suffix}"));
            assert!(p.to_string_lossy().len() <= 100, "{suffix} 路徑超過 AF_UNIX 上限：{}", p.display());
        }
    }

    /// #282：對端活著但不讀 stdin（`sleep`），資料又大於 pipe buffer——寫入卡住時整個呼叫要在 timeout 內回「逾時」。
    #[tokio::test]
    async fn a_stalled_stdin_write_is_bounded_by_the_timeout() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30");
        let data = vec![7u8; 8 * 1024 * 1024];
        let started = std::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(10), run_with_stdin(cmd, &data, Duration::from_millis(500)))
            .await
            .expect("寫入卡住時 timeout 必須生效，不能整個呼叫掛住");
        assert!(matches!(r, Ok(None)), "要回逾時：{r:?}");
        // 對端 `sleep 30`：真的卡住會等滿 30 秒；上限只要低於它就分得出來，不必貼著名義時間（慢 runner 才不會翻紅）。
        assert!(started.elapsed() < Duration::from_secs(25));
    }

    /// ssh_exec 的 script 走 stdin：遠端不讀、script 又大於 pipe buffer 時，寫入卡住也要在 timeout 內回「逾時」（同 #282）。
    #[tokio::test]
    async fn a_stalled_script_write_is_bounded_by_the_exec_timeout() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30");
        let script = format!("# {}\n", "x".repeat(8 * 1024 * 1024));
        let started = std::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(10), run_script_over_stdin(cmd, &script, Duration::from_millis(500), "h"))
            .await
            .expect("寫入卡住時 timeout 必須生效，不能整個呼叫掛住");
        let e = r.expect_err("要回逾時");
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(25));
    }

    /// 正常路徑：script 完整送到、stdout 照拿；非 0 退出帶 stderr。
    #[tokio::test]
    async fn the_script_reaches_sh_and_failures_carry_stderr() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-s");
        assert_eq!(run_script_over_stdin(cmd, "printf hi", Duration::from_secs(10), "h").await.unwrap(), "hi");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-s");
        let e = run_script_over_stdin(cmd, "echo boom >&2; exit 3", Duration::from_secs(10), "h").await.unwrap_err();
        assert!(e.to_string().contains("boom"), "{e}");
    }

    /// 正常路徑：資料完整送到、輸出照拿。
    #[tokio::test]
    async fn stdin_data_reaches_the_child_and_its_output_comes_back() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("wc -c");
        let data = vec![1u8; 300_000];
        let (out, wrote) = run_with_stdin(cmd, &data, Duration::from_secs(10)).await.unwrap().unwrap();
        wrote.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "300000");
    }

    /// 本機探測逾時：子行程（含孫行程）要被收掉，不能留著等下一輪再多留一個。
    #[tokio::test]
    async fn a_timed_out_local_probe_leaves_nothing_running() {
        let marker = format!("am-local-probe-{}", crate::db::ulid());
        let script = format!("sleep 30 # {marker}\nsleep 30 # {marker}");
        let t0 = std::time::Instant::now();
        let e = sh_local_stdout(&script, Duration::from_millis(300), "probe").await.unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(25));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ps = std::process::Command::new("/bin/ps").args(["-axo", "command"]).output().unwrap();
        let alive: Vec<&str> = std::str::from_utf8(&ps.stdout).unwrap().lines().filter(|l| l.contains(&marker) && !l.contains("ps ")).collect();
        assert!(alive.is_empty(), "children survived the timeout: {alive:?}");
    }

    #[tokio::test]
    async fn a_local_probe_returns_its_stdout() {
        assert_eq!(sh_local_stdout("printf ok", Duration::from_secs(10), "probe").await.unwrap(), "ok");
    }

    /// 逾時前已經印出的輸出要留著：agy 沒憑證時印 `Authentication required` 然後卡住等人（issue #870）。
    #[tokio::test]
    async fn a_timed_out_local_run_keeps_what_it_printed_before_the_timeout() {
        let marker = format!("am-partial-{}", crate::db::ulid());
        let script = format!("echo 'Authentication required'; echo warn >&2; sleep 30 # {marker}");
        let t0 = std::time::Instant::now();
        let run = sh_local_capture(&script, Duration::from_millis(500)).await.unwrap();
        assert!(run.timed_out && run.status.is_none());
        assert!(String::from_utf8_lossy(&run.stdout).contains("Authentication required"), "{:?}", String::from_utf8_lossy(&run.stdout));
        assert!(String::from_utf8_lossy(&run.stderr).contains("warn"));
        assert!(t0.elapsed() < Duration::from_secs(25));
        // 沒逾時的照舊：有退出狀態、完整輸出。
        let ok = sh_local_capture("printf out; printf err >&2; exit 3", Duration::from_secs(10)).await.unwrap();
        assert!(!ok.timed_out);
        assert_eq!(ok.status.and_then(|s| s.code()), Some(3));
        assert_eq!((String::from_utf8_lossy(&ok.stdout).as_ref(), String::from_utf8_lossy(&ok.stderr).as_ref()), ("out", "err"));
        // `sh_local` 的契約不變：逾時回 `None`。
        assert!(sh_local("sleep 30", Duration::from_millis(200)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sh_local_timeout_kills_the_child() {
        let marker = format!("am-sh-local-{}", crate::db::ulid());
        // `sh -c` may fork rather than exec the last command; the group kill has to reach it.
        let script = format!("sleep 30 # {marker}\nsleep 30 # {marker}");
        let t0 = std::time::Instant::now();
        let r = sh_local(&script, Duration::from_millis(300)).await.unwrap();
        assert!(r.is_none(), "expected a timeout");
        assert!(t0.elapsed() < Duration::from_secs(25), "子行程 `sleep 30`：沒被逾時砍掉才會等滿");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ps = std::process::Command::new("/bin/ps").args(["-axo", "command"]).output().unwrap();
        let alive: Vec<&str> = std::str::from_utf8(&ps.stdout).unwrap().lines().filter(|l| l.contains(&marker) && !l.contains("ps ")).collect();
        assert!(alive.is_empty(), "children survived the timeout: {alive:?}");
        let ok = sh_local("printf hi; exit 3", Duration::from_secs(5)).await.unwrap().unwrap();
        assert_eq!(ok.status.code(), Some(3));
        assert_eq!(ok.stdout, b"hi");
    }

    #[tokio::test]
    async fn macos_local_sh_local_drains_stdout_and_stderr_concurrently() {
        let output = sh_local(
            r"head -c 200000 /dev/zero | tr '\0' x >&2; printf done",
            Duration::from_secs(3),
        )
        .await
        .unwrap()
        .expect("large stderr output must not deadlock until timeout");

        assert!(output.status.success());
        assert_eq!(output.stdout, b"done");
        assert_eq!(output.stderr.len(), 200_000);
        assert!(output.stderr.iter().all(|byte| *byte == b'x'));
    }

    fn cfg() -> HostCfg {
        HostCfg {
            shared_session: false,
            name: "m4p".into(),
            ssh: "m4p@100.112.229.82".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }
    }

    /// 主機睡著／tailscale 斷線時 ssh 連不出去：沒有 ConnectTimeout 就只能等系統的 TCP 逾時（Linux 約 2 分鐘），
    /// 所以每條 ssh 都帶 ConnectTimeout（跟 remote_cargo 的 SSH_LIVENESS_OPTS 同一個數字）。放在使用者的 `ssh_opts` **後面**：
    /// ssh 取第一個拿到的值，使用者為慢線路明寫的 `ConnectTimeout=60` 才蓋得過去。
    #[test]
    fn every_ssh_leg_has_a_connect_timeout_the_users_opts_can_override() {
        let mut c = cfg();
        c.ssh_opts = vec!["-o".into(), "ConnectTimeout=60".into()];
        let args = HostConn::remote(c, None).ssh_args();
        let at = |needle: &str| args.iter().position(|a| a.starts_with(needle));
        let ours = at("ConnectTimeout=15").expect("沒有預設的 ConnectTimeout");
        let theirs = at("ConnectTimeout=60").expect("使用者的選項不見了");
        assert!(theirs < ours, "使用者的要排在前面才蓋得過預設：{args:?}");
        assert!(HostConn::remote(cfg(), None).ssh_args().iter().any(|a| a == "ConnectTimeout=15"));
    }

    /// 離線警示條的「離線多久」：斷線起點只在第一次斷時記下，重試失敗不往後推；連上就清掉。
    #[test]
    fn disconnected_since_keeps_the_first_drop_and_clears_on_connect() {
        let conn = HostConn::remote(cfg(), None);
        let first = conn.disconnected_since().expect("還沒連上過就算離線");
        conn.mark_down();
        assert_eq!(conn.disconnected_since().as_deref(), Some(first.as_str()));
        conn.mark_up();
        conn.connected.store(true, Ordering::SeqCst);
        assert_eq!(conn.disconnected_since(), None);
        conn.mark_down();
        conn.connected.store(false, Ordering::SeqCst);
        assert!(conn.disconnected_since().is_some_and(|t| t >= first));
    }

    /// issue #506：`apply_config` 的套用迴圈以前沒像上面的移除迴圈那樣跳過 local，
    /// 一列 `name = "local"` 就會把本機那顆 `HostConn` 換成 `HostConn::remote`——
    /// client socket 變成沒人在聽的 `<instance>/local.sock`、`is_local()` 翻成 false，
    /// 之後所有「本機走直接路徑、遠端走 ssh」的分支整批翻面。
    #[tokio::test]
    async fn a_hosts_entry_named_local_never_replaces_the_local_connection() {
        let env = crate::testing::env().await;
        let before = env.app.hosts.get(LOCAL_HOST).await.expect("本機那顆一開始就在");
        assert!(before.is_local());

        let mut bad = cfg();
        bad.name = LOCAL_HOST.into();
        let changed = env.app.hosts.apply_config(&env.app, &[bad]).await;

        assert!(!changed.contains(LOCAL_HOST), "不該把 local 當成「設定變了」的主機");
        let after = env.app.hosts.get(LOCAL_HOST).await.expect("本機那顆還要在");
        assert!(after.is_local(), "local 不可以變成 ssh 遠端");
        assert!(Arc::ptr_eq(&before, &after), "連那顆 HostConn 都不該被換掉（supervisor、master 都還在原位）");
    }

    /// #347：同名主機改設定（可能已經指到另一台）時，以主機名為鍵的觀測快取要當場作廢，
    /// 不然新連線的偵測／探測回來之前，舊機器的身分、撞限、模型清單會被當成新機器的事實。
    #[tokio::test]
    async fn replacing_a_host_forgets_what_was_observed_through_the_old_connection() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let mk = |name: &str, ssh: &str| HostCfg { name: name.into(), ssh: ssh.into(), ..cfg() };
        app.hosts.insert_remote_for_test(mk("inv-347", "target-a")).await;
        app.hosts.insert_remote_for_test(mk("other-347", "target-o")).await;
        let reading = || crate::quota::Quota {
            five_hour: None, seven_day: None, fable: None, reset_credits: None, limit_hit: None, plan: None,
            updated_at: crate::db::now(), source: "test".into(), account: None, host: String::new(),
        };
        let tools = || crate::tools::HostTools {
            tools: Default::default(), identities: Default::default(), shell_identities: vec![], utc_offset_secs: None, herdr_cli: None,
            checked_at: crate::db::now(),
        };
        let shell = |host: &str| crate::api::shell::HostShell {
            host: host.into(), herdr_session: "agents-manager".into(), workspace_id: "w1".into(), tab_id: "w1:t1".into(),
            pane_id: "w1:p1".into(), cwd: "/".into(), created_at: crate::db::now(),
        };
        for h in ["inv-347", "other-347"] {
            app.tools.lock().await.insert(h.into(), tools());
            crate::quota::set(app, h, "codex", reading()).await;
            app.models_cache.lock().await.insert(format!("{h}/codex/"), (std::time::Instant::now(), json!({})));
            app.host_shells.lock().await.push(shell(h));
            if h == "inv-347" {
                crate::runners::login_assist::reserve(app, h, "cc9").unwrap().register("w1:p1", &shell(h).created_at);
            }
        }
        let cached_rows = |key: &'static str| async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM quota_cache WHERE key = ?").bind(key).fetch_one(&app.db).await.unwrap()
        };
        assert_eq!(cached_rows("inv-347/codex").await, 1);

        app.hosts.replace_remote_for_test(app, mk("inv-347", "target-b")).await;

        assert!(!app.tools.lock().await.contains_key("inv-347"), "舊機器的偵測結果（身分、登入）要丟");
        assert!(!app.quotas.lock().await.contains_key("inv-347/codex"), "舊機器的額度不能擋新機器");
        assert_eq!(cached_rows("inv-347/codex").await, 0, "重啟快取列也要清，不然下次開機又種回來");
        assert!(!app.models_cache.lock().await.contains_key("inv-347/codex/"), "舊機器的模型清單要丟");
        assert!(!app.host_shells.lock().await.iter().any(|s| s.host == "inv-347"), "舊機器上的 shell 不能拿來對新機器的 pane");
        assert!(!crate::login_assist::is_registered(app, "inv-347", "w1:p1"), "舊機器上的登入 pane 也不能阻擋或指向新機器");
        assert!(app.tools.lock().await.contains_key("other-347"), "別台不動");
        assert!(app.quotas.lock().await.contains_key("other-347/codex"));
        assert!(app.models_cache.lock().await.contains_key("other-347/codex/"));
        assert!(app.host_shells.lock().await.iter().any(|s| s.host == "other-347"));
    }

    #[test]
    fn only_forward_shaping_fields_reconnect() {
        let a = cfg();
        let mut b = cfg();
        assert!(!cfg_differs(&a, &b));
        b.ssh_port = 2222;
        assert!(cfg_differs(&a, &b));
    }

    #[tokio::test]
    async fn reconnect_does_not_return_a_stale_error() {
        let env = crate::testing::env().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut host_cfg = cfg();
        host_cfg.name = "reconnect-stale-error-test".into();
        // Never accept: ssh stays in handshake past the first 250 ms poll, exposing a leaked stale error.
        host_cfg.ssh = "127.0.0.1".into();
        host_cfg.ssh_port = port;
        host_cfg.ssh_opts = vec!["-o".into(), "ConnectTimeout=2".into()];
        let conn = HostConn::remote(host_cfg, env.app.instance());
        *conn.error.lock().await = Some("previous error".into());
        env.app.hosts.conns.lock().await.insert(conn.name.clone(), conn.clone());

        let result = tokio::time::timeout(Duration::from_secs(5), env.app.hosts.reconnect(&env.app, &conn.name))
            .await
            .expect("reconnect should not wait for the full timeout")
            .expect("host should exist");
        assert!(!result.0);
        assert_ne!(result.1.as_deref(), Some("previous error"));

        let task = conn.supervisor.lock().await.take();
        if let Some(task) = task {
            task.abort();
        }
    }

    // ---- #887：ssh 控制目錄與遠端腳本不再用 /tmp 底下可預測的路徑 ----

    #[test]
    fn ensure_private_dir_creates_0700_and_rejects_loose_or_symlinked_dirs() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let base = std::env::temp_dir().join(format!("am-test-{}", crate::db::ulid()));
        std::fs::create_dir_all(&base).unwrap();
        let mode = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;

        // 不存在 → 建立，0700。
        let fresh = base.join("fresh");
        ensure_private_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        // 已存在 0700 → 照樣 Ok（重複呼叫）。
        ensure_private_dir(&fresh).unwrap();

        // 已存在但權限太鬆 → Err，而且不去 chmod 它。
        let loose = base.join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = ensure_private_dir(&loose).unwrap_err().to_string();
        assert!(err.contains("不安全的 ssh 控制目錄"), "{err}");
        assert_eq!(mode(&loose), 0o777, "別人預先佔的目錄不動它");

        // 符號連結（就算指向一個 0700 目錄）→ Err。
        let link = base.join("link");
        symlink(&fresh, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());

        // 不是目錄 → Err。
        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(ensure_private_dir(&file).is_err());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_remote_session_script_has_no_predictable_tmp_paths_and_still_parses() {
        let script = HostConn::remote_session_script(&cfg(), false);
        for old in ["/tmp/am-systemctl.err", "/tmp/am-launchctl.err", "/tmp/herdr-"] {
            assert!(!script.contains(old), "腳本不該再用 {old}：{script}");
        }
        assert!(script.contains("mktemp"), "錯誤輸出暫存要用 mktemp");
        assert!(script.contains(".config/agents-manager/herdr-"), "server log 放在使用者自己的目錄");
        assert!(script.contains("umask 077"));
        let parsed = std::process::Command::new("/bin/sh").arg("-n").arg("-c").arg(&script).output().unwrap();
        assert!(parsed.status.success(), "sh -n: {}", String::from_utf8_lossy(&parsed.stderr));
    }

    // ---- #888：post-connect 卡住不能讓 supervisor 發現不了斷線 ----

    struct TestHooks {
        hosts: HostManager,
        entered: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl HostsAccess for TestHooks {
        fn hosts(&self) -> &HostManager {
            &self.hosts
        }
    }

    impl HostInstance for TestHooks {
        fn instance(&self) -> Option<String> {
            None
        }
    }

    impl HostHooks for TestHooks {
        async fn is_shared_host(_app: &Arc<Self>, _host: &str) -> bool {
            false
        }
        async fn host_changed(_app: &Arc<Self>, _fence: &HostFence) {}
        fn set_local_herdr_connected(&self, _ok: bool) {}
        /// 永遠不回的 post-connect（像卡在遠端不回話的對帳）；被 abort 時 future 被丟掉，flag 會翻。
        async fn host_connected(app: Arc<Self>, _host: String) {
            struct OnDrop(Arc<AtomicBool>);
            impl Drop for OnDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _flag = OnDrop(app.dropped.clone());
            app.entered.store(true, Ordering::SeqCst);
            futures::future::pending::<()>().await;
        }
        async fn forget_host_observations(_app: &Arc<Self>, _host: &str) {}
        async fn host_removed(_app: &Arc<Self>, _host: &str) {}
    }

    #[tokio::test]
    async fn a_hung_post_connect_does_not_stop_the_supervisor_from_noticing_a_dead_host() {
        let dir = std::env::temp_dir().join(format!("am-test-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let herdr = crate::testing::MockHerdr::start(dir.join("remote.sock"));
        let hooks = Arc::new(TestHooks {
            hosts: HostManager::new(HerdrClient::new(dir.join("local.sock"))),
            entered: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicBool::new(false)),
        });
        let mut host_cfg = cfg();
        host_cfg.name = "post-connect-hang-test".into();
        let conn = hooks.hosts.insert_remote_with_client_for_test(host_cfg, HerdrClient::new(dir.join("remote.sock"))).await;
        // 一個活著的假 ssh master：`run_connected` 只看它還在不在。
        let master = tokio::process::Command::new("sleep").arg("60").kill_on_drop(true).spawn().unwrap();
        conn.set_master_for_test(master).await;
        set_ping_interval_for_test(Some(Duration::from_millis(100)));
        let fence = hooks.hosts.fence(&conn.name).await.unwrap();
        let generation = hooks.hosts.current_generation(&conn.name).await.unwrap();

        let run = tokio::spawn({
            let (hooks, conn) = (hooks.clone(), conn.clone());
            async move { run_connected(&hooks, &conn, &fence, generation).await }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !(hooks.entered.load(Ordering::SeqCst) && conn.is_connected()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("連上之後要標成連線中，而且 post-connect 已經開始（卡住）");

        // herdr 不回 ping：post-connect 還卡著，也要在幾個 ping 間隔內發現、標成斷線。
        for _ in 0..4 {
            herdr.fail_next("ping", crate::testing::Fault::Refuse);
        }
        let superseded = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("post-connect 卡住時 supervisor 仍要發現斷線")
            .unwrap();
        assert!(!superseded, "斷線不是世代變了");
        assert!(!conn.is_connected());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !hooks.dropped.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("斷線時 post-connect task 要一起收掉");

        set_ping_interval_for_test(None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_remote_dir_scripts_refuse_the_agy_credential_directory() {
        let home = crate::testing::track(std::env::temp_dir().join(format!("am-test-gemini-{}", crate::db::ulid())));
        std::fs::create_dir_all(home.join(".gemini/antigravity-cli")).unwrap();
        std::fs::create_dir_all(home.join("project")).unwrap();

        let run = |script: &str| -> String {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .env("HOME", &home)
                .output()
                .expect("run sh script");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        let err1 = parse_dir_listing(&run(&dir_list_script(Some("~/.gemini"), true)), true)
            .unwrap_err()
            .to_string();
        assert!(err1.contains("forbidden"), "dir_list ~/.gemini err: {err1}");

        let err2 = parse_dir_listing(&run(&dir_list_script(Some("~/.gemini/antigravity-cli"), true)), true)
            .unwrap_err()
            .to_string();
        assert!(err2.contains("forbidden"), "dir_list ~/.gemini/antigravity-cli err: {err2}");

        let err3 = parse_make_dir(&run(&make_dir_script("~/.gemini", "x")))
            .unwrap_err()
            .to_string();
        assert!(err3.contains("forbidden"), "make_dir under ~/.gemini err: {err3}");
        assert!(!home.join(".gemini/x").exists(), "home/.gemini/x must not exist");

        let err4 = parse_make_dir(&run(&make_dir_script("~", ".gemini")))
            .unwrap_err()
            .to_string();
        assert!(err4.contains("forbidden"), "make_dir ~ .gemini err: {err4}");

        let ok = parse_dir_listing(&run(&dir_list_script(Some("~/project"), true)), true);
        assert!(ok.is_ok(), "dir_list ~/project should succeed: {:?}", ok.err());
    }


    /// B7（#1145）：ssh 上限要比腳本自己等的時間長，「起不來」的診斷才送得回來；腳本的迴圈也用同一個常數。
    #[test]
    fn the_remote_session_probe_gets_more_time_than_its_own_script_waits() {
        assert!(REMOTE_SESSION_TIMEOUT.as_secs() >= 2 * (REMOTE_SESSION_WAIT_ROUNDS + 2));
        let script = HostConn::remote_session_script(&cfg(), false);
        assert!(script.contains(&format!("while [ $i -lt {} ]", REMOTE_SESSION_WAIT_ROUNDS)), "腳本要用同一個等待圈數");
    }
