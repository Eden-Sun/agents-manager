
    use super::*;
    use crate::runners::models::list;

    /// 一台設定了、但連不上的遠端主機（沒有人在聽那個 port，connection refused，快速失敗）。
    async fn unreachable_host(e: &crate::testing::Env) -> &'static str {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let cfg = crate::config::HostCfg {
            shared_session: false,
            name: "unreachable-box".into(),
            ssh: "127.0.0.1".into(),
            ssh_port: port,
            ssh_opts: vec!["-o".into(), "ConnectTimeout=2".into()],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        e.app.hosts.apply_config(&e.app, &[cfg]).await;
        "unreachable-box"
    }

    #[tokio::test]
    async fn a_remote_model_identity_does_not_probe_without_home_and_recovers() {
        let e = crate::testing::env().await;
        let host = format!("model-home-616-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = e.app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        e.app.cfg.update(|cfg| {
            cfg.identities.push(crate::config::IdentityCfg {
                name: "cc1".into(),
                kind: "claude".into(),
                host: Some(host.clone()),
                env: [("CLAUDE_CONFIG_DIR".into(), "~/.claude-cc1".into())].into(),
                args: vec![],
            });
            Ok(())
        }).await.unwrap();
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Err(anyhow!("injected remote HOME read failure"))
        });

        let err = claude_default_effort(&e.app, &host, Some("cc1"), "opus").await.expect_err("do not read an identity's model settings without its HOME");
        assert!(err.to_string().contains("HOME"), "preserve the HOME failure reason: {err:#}");
        assert_eq!(calls.lock().unwrap().len(), 1, "settings-file probe must not follow failed HOME resolution");

        *conn.remote_home.lock().await = Some("/home/remote-model".into());
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Ok(r#"{"effortLevel":"medium"}"#.into())
        });
        assert_eq!(claude_default_effort(&e.app, &host, Some("cc1"), "opus").await.unwrap(), "medium");
        assert!(calls.lock().unwrap().last().unwrap().contains("'/home/remote-model/.claude-cc1'/settings.json"), "recovery uses the remote identity path: {:?}", calls.lock().unwrap());
    }

    /// #880：沒帶 `--effort`、settings 沒有 `effortLevel` 時，內建預設按模型分（haiku medium、sonnet high）；有設定就照設定。
    #[tokio::test]
    async fn the_builtin_default_effort_depends_on_the_model_alias() {
        let e = crate::testing::env().await;
        let host = format!("model-eff-880-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = e.app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        *conn.remote_home.lock().await = Some("/home/remote-model".into());
        crate::hosts::set_ssh_fake(&host, |_| Ok(String::new()));
        let of = |alias: &'static str| {
            let (app, host) = (e.app.clone(), host.clone());
            async move { claude_default_effort(&app, &host, None, alias).await.unwrap() }
        };
        assert_eq!(of("haiku").await, "medium");
        assert_eq!(of("claude-haiku-5-5").await, "medium", "完整 id 也認得");
        assert_eq!(of("sonnet").await, "medium", "廠推薦一律 medium");
        assert_eq!(of("claude-sonnet-5-5").await, "medium");
        assert_eq!(of("opus").await, "medium");
        assert_eq!(of("fable").await, "medium", "廠推薦一律 medium，不照模型分");
        assert_eq!(of("claude-opus-5").await, "medium");

        crate::hosts::set_ssh_fake(&host, |_| Ok(r#"{"effortLevel":"low"}"#.into()));
        assert_eq!(of("haiku").await, "low", "settings 的 effortLevel 蓋過內建預設");
        assert_eq!(of("sonnet").await, "low");
        assert_eq!(of("opus").await, "medium", "文件：頂層 effortLevel 對 Opus 5.5 不算數");
        assert_eq!(of("claude-opus-5").await, "low", "Opus 5 照舊吃頂層那個");

        // #1679：檔案只有舊版那一列（claude-opus-5）→ 目前版本沒有列，用內建預設，不拿舊版頂替。
        crate::hosts::set_ssh_fake(&host, |_| Ok(r#"{"modelSettings":{"claude-opus-5":{"effortLevel":"xhigh"}}}"#.into()));
        assert_eq!(of("opus").await, "medium");
        assert_eq!(of("claude-opus-5").await, "xhigh", "完整 id 照完全相等的那一列");
        crate::hosts::set_ssh_fake(&host, |_| Ok(r#"{"modelSettings":{"claude-opus-5-5":{"effortLevel":"low"}}}"#.into()));
        assert_eq!(of("opus").await, "low");
    }

    /// #268：遠端 settings.json 讀不到（ssh 失敗）不是「沒設定」——以前讀成 `""`，預設 effort 變成內建的 `high`，
    /// 對帳把它記進子 bot，之後沒有人會再讀一次。
    #[tokio::test]
    async fn an_unreadable_remote_settings_file_is_an_error_not_the_builtin_default() {
        let e = crate::testing::env().await;
        let host = unreachable_host(&e).await;
        let r = tokio::time::timeout(Duration::from_secs(20), claude_default_effort(&e.app, host, None, "opus"))
            .await
            .expect("連不上要快速失敗");
        assert!(r.is_err(), "讀不到設定檔不能回內建預設：{r:?}");
        let listed = tokio::time::timeout(Duration::from_secs(20), list(&e.app, host, "claude", None, true)).await.unwrap();
        assert!(listed.is_err(), "模型清單不能拿內建預設充數、還快取十分鐘");
        assert!(e.app.models_cache.lock().await.is_empty(), "失敗的結果不能進快取");
        e.app.hosts.remove(&e.app, host).await;
    }

    /// #347：A 機的模型探測還在路上時同名主機改指到 B、B 的清單先進了快取；A 晚到的結果不能蓋掉它，也不能回給呼叫端當答案。
    #[tokio::test]
    async fn a_model_list_from_a_superseded_host_does_not_reach_the_cache() {
        let e = crate::testing::env().await;
        let host = "models-347";
        let cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        let answer = Arc::new(std::sync::Mutex::new(r#"{"effortLevel":"low"}"#.to_string()));
        let a2 = answer.clone();
        crate::hosts::set_ssh_fake(host, move |_| Ok(a2.lock().unwrap().clone()));
        crate::hosts::set_ssh_delay(host, Duration::from_millis(400));
        e.app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let app = e.app.clone();
        let stale = tokio::spawn(async move { list(&app, host, "claude", None, true).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        e.app.hosts.replace_remote_for_test(&e.app, cfg("target-b")).await;
        *answer.lock().unwrap() = r#"{"effortLevel":"max"}"#.into();
        crate::hosts::set_ssh_delay(host, Duration::ZERO);
        let fresh = list(&e.app, host, "claude", None, true).await.expect("B 自己的清單");
        assert_eq!(fresh["models"][1]["account_default_effort"], "max");
        assert_eq!(fresh["models"][1]["default_effort"], "medium", "廠推薦不跟帳號設定走");
        *answer.lock().unwrap() = r#"{"effortLevel":"low"}"#.into();

        let r = stale.await.unwrap();
        assert!(r.is_err(), "A 的結果作廢，不能當答案回：{r:?}");
        let cached = e.app.models_cache.lock().await.get(&format!("{host}/claude/")).map(|(_, v)| v.clone()).expect("B 的還在");
        assert_eq!(cached["models"][1]["account_default_effort"], "max", "快取要留著 B 的清單");
    }

    /// 缺檔是答案不是錯誤：本機回 `""`，遠端腳本要 exit 0（`ssh_exec` 看 exit code）。
    #[tokio::test]
    async fn a_missing_settings_file_is_an_empty_answer_not_a_failure() {
        let e = crate::testing::env().await;
        let local = read_optional_text(&e.app, LOCAL_HOST, "/nonexistent-am-dir/settings.json").await.unwrap();
        assert_eq!(local, "");
        let st = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(optional_cat_script("/nonexistent-am-dir/settings.json"))
            .status()
            .await
            .unwrap();
        assert!(st.success(), "缺檔時腳本要 exit 0，不然遠端缺檔會被 ssh_exec 當成失敗");
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("am-models-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 設定檔是 FIFO（沒有寫入端）：以前本機的 `cat` 永遠等，`GET /api/models` 與生 child 一起卡住（#1074）。
    /// 非一般檔案現在當成缺檔（空字串）、立刻回來。
    #[tokio::test]
    async fn a_fifo_config_does_not_hang_the_local_read() {
        let e = crate::testing::env().await;
        let dir = scratch_dir("fifo");
        let fifo = dir.join("config.toml");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            read_optional_text(&e.app, LOCAL_HOST, &fifo.display().to_string()),
        )
        .await;
        match r {
            Ok(Ok(s)) => assert_eq!(s, "", "FIFO 當成缺檔，立刻回空字串"),
            _ => {
                // 還是卡住：寫端開一下把 `cat` 放掉，免得留下行程。
                let f = fifo.clone();
                std::thread::spawn(move || drop(std::fs::OpenOptions::new().write(true).open(f)));
                panic!("讀 FIFO 卡住（超過 20 秒）：{r:?}");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 一般檔超過 1 MiB 只讀前 1 MiB（設定檔不會這麼大；指到大檔或裝置時不吃光記憶體）。
    #[tokio::test]
    async fn a_huge_config_is_read_only_up_to_one_mib() {
        let e = crate::testing::env().await;
        let dir = scratch_dir("huge");
        let path = dir.join("config.toml");
        std::fs::write(&path, vec![b'x'; 2 * 1024 * 1024]).unwrap();
        let s = read_optional_text(&e.app, LOCAL_HOST, &path.display().to_string()).await.unwrap();
        assert_eq!(s.len(), 1024 * 1024);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The argv shapes `herdr pane process-info` actually reported on this machine,
    /// 2026-09-07 (a claude child pane and a grok one).
    #[test]
    fn model_and_effort_are_read_off_a_running_cli() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--dangerously-skip-permissions", "--model", "opus"])),
            (Some("claude-opus-5-5".into()), None)
        );
        assert_eq!(
            model_effort_from_argv("grok", &argv(&["grok", "--always-approve", "-m", "grok-4.6", "--reasoning-effort", "high"])),
            (Some("grok-4.6".into()), Some("high".into()))
        );
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--model=sonnet", "--effort=xhigh"])),
            (Some("sonnet".into()), Some("xhigh".into()))
        );
        assert_eq!(
            model_effort_from_argv(
                "codex",
                &argv(&["codex", "-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=\"max\"", "-c", "service_tier=\"priority\""])
            ),
            (Some("gpt-6.1-sol".into()), Some("max".into()))
        );
        assert_eq!(
            fast_from_argv("codex", &argv(&["codex", "-c", "service_tier=\"priority\""])),
            Some(true)
        );
        assert_eq!(fast_from_argv("codex", &argv(&["codex", "-c", "service_tier=\"\""])), Some(false));
        assert_eq!(fast_from_argv("claude", &argv(&["claude", "-c", "service_tier=\"priority\""])), None);
    }

    #[test]
    fn argv_model_effort_and_fast_scanning_stops_at_double_dash() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--model", "sonnet", "--", "--model=haiku"])),
            (Some("sonnet".into()), None)
        );
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--effort", "high", "--", "--effort=low"])),
            (None, Some("high".into()))
        );
        assert_eq!(fast_from_argv("codex", &argv(&["codex", "--", "--config=service_tier=priority"])), None);
    }

    /// Nothing is guessed: a bare CLI stays unset, and an effort the kind does not accept
    /// (`max` is claude/codex only) is dropped rather than stored for grok.
    #[test]
    fn unparseable_argv_leaves_the_fields_unset() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(model_effort_from_argv("claude", &argv(&["claude", "--dangerously-skip-permissions"])), (None, None));
        assert_eq!(model_effort_from_argv("grok", &argv(&["grok", "--reasoning-effort", "max"])), (None, None));
        // A flag whose value is missing must not swallow the next flag.
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--model", "--effort", "high"])),
            (None, Some("high".into()))
        );
    }

    #[test]
    fn grok_falls_back_to_its_terminal_title() {
        assert_eq!(grok_title_model_effort("Grok 4.6 (xhigh)"), (Some("grok-4.6".into()), Some("xhigh".into())));
        assert_eq!(grok_title_model_effort("Grok 4.6"), (Some("grok-4.6".into()), None));
        // A title the agent has renamed to its task says nothing about the model.
        assert_eq!(grok_title_model_effort("遠端主機 gh 登入 API 與 UI - grok"), (None, None));
        assert_eq!(grok_title_model_effort("Grok Code Fast"), (None, None));
    }

    #[test]
    fn grok_effort_is_read_off_the_tui_footer() {
        let high = "  │ ❯                                        │\n  ╰──────────────── Grok 4.6 (high) · always-approve ─╯\n";
        assert_eq!(grok_effort_from_screen(high).as_deref(), Some("high"));
        let medium = "  ╰──────────────────────────────── Grok 4.6 (medium) · always-approve ─╯\n";
        assert_eq!(grok_effort_from_screen(medium).as_deref(), Some("medium"));
        assert_eq!(grok_effort_from_screen("claude composer, no grok footer"), None);
    }

    /// agy 的清單是寫死的 slug，effort 已在 slug 裡：每個模型的 `efforts` 都是空的（`--effort` 留給第二階段），有且只有一個預設。
    #[test]
    fn agy_models_are_static_slugs_without_separate_efforts() {
        let models = agy_static_models();
        assert_eq!(models.iter().filter(|m| m["is_default"] == true).count(), 1);
        for m in &models {
            assert_eq!(m["efforts"], json!([]));
        }
        assert_eq!(model_effort_from_argv("agy", &["agy".into(), "--model".into(), "gemini-3.8-flash-high".into()]).0.as_deref(), Some("gemini-3.8-flash-high"));
    }

    /// 2026-10-14：agy 只收 Gemini 3.8 含以上。判斷式（daemon 的 `agy_model_allowed`，web 的 `agyModelAllowed` 同一條）：
    /// 3.7 以下、非 Gemini、沒有版號的都不收；4、4-argon、3.10 收。
    #[test]
    fn agy_model_rule_is_gemini_38_and_up() {
        for ok in ["gemini-3.8-flash-medium", "gemini-3.8-flash-high", "gemini-3.8-flash-low", "gemini-4-argon", "gemini-4.1-argon-high", "gemini-3.10-flash-low", "gemini-5-whatever"] {
            assert!(agy_model_allowed(ok), "{ok} 該收");
        }
        for no in [
            "gemini-3.7-flash-high",
            "gemini-3.6-flash-medium",
            "gemini-3.1-pro-high",
            "gemini-3-flash",
            "gemini-3.8.1-flash",
            "gemini-",
            "gemini-argon",
            "gemini3.8-flash",
            "claude-sonnet-4-6",
            "claude-opus-4-6-thinking",
            "gpt-oss-120b-medium",
            "",
        ] {
            assert!(!agy_model_allowed(no), "{no} 不該收");
        }
    }

    /// 不收的 agy 模型一律換成預設（啟動、寫設定、adopted 的 argv 都是同一支 `canonical_model`）；收的原樣。
    #[test]
    fn agy_models_outside_the_rule_fall_back_to_the_default() {
        for retired in [
            "gemini-3.7-flash-high",
            "gemini-3.7-flash-medium",
            "gemini-3.7-flash-low",
            "gemini-3.6-flash-high",
            "gemini-3.6-flash-medium",
            "gemini-3.6-flash-low",
            "gemini-3.1-pro-high",
            "gemini-3.1-pro-low",
            "claude-sonnet-4-6",
            "claude-opus-4-6-thinking",
            "gpt-oss-120b-medium",
        ] {
            assert_eq!(canonical_model("agy", retired), AGY_DEFAULT_MODEL, "{retired}");
            assert_eq!(remap_deprecated_model("agy", retired), Some(AGY_DEFAULT_MODEL));
        }
        assert_eq!(AGY_DEFAULT_MODEL, "gemini-3.8-flash-medium", "預設維持 3.8 medium");
        for kept in ["gemini-3.8-flash-high", "gemini-3.8-flash-low", "gemini-4-argon", "gemini-3.9-flash-high"] {
            assert_eq!(canonical_model("agy", kept), kept, "{kept} 使用者自己的選擇不動");
            assert_eq!(remap_deprecated_model("agy", kept), None, "{kept}");
        }
        assert_eq!(remap_deprecated_model("claude", "claude-sonnet-4-6"), None, "只對 agy");
        assert_eq!(model_effort_from_argv("agy", &["agy".into(), "--model".into(), "gemini-3.1-pro-high".into()]).0.as_deref(), Some("gemini-3.8-flash-medium"));
    }

    /// `agy models` 的輸出：`slug<TAB>顯示名` 一行一個，只留規則內的，預設排第一、其餘保持 CLI 順序。
    #[test]
    fn agy_models_output_is_filtered_and_default_first() {
        let text = "claude-sonnet-4-6\tClaude Sonnet 4.6\n\
gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
gemini-3.7-flash-high\tGemini 3.7 Flash (High)\n\
gemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)\n\
gpt-oss-120b-medium\tGPT-OSS 120B\n\
gemini-4-argon\n";
        let models = agy_models_from_text(text);
        let ids: Vec<&str> = models.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["gemini-3.8-flash-medium", "gemini-3.8-flash-low", "gemini-4-argon"]);
        assert_eq!(models.iter().filter(|m| m["is_default"] == true).count(), 1);
        assert_eq!(models[0]["is_default"], true);
        assert_eq!(models[0]["display_name"], "Gemini 3.8 Flash (Medium)");
        assert_eq!(models[2]["display_name"], "gemini-4-argon", "沒有顯示名就用 slug");
        for m in &models {
            assert_eq!(m["efforts"], json!([]));
        }
    }

    /// 未登入或沒有可用的輸出：解出來是空的，呼叫端就退回寫死的清單。
    #[test]
    fn agy_models_output_without_a_usable_line_is_empty() {
        assert!(agy_models_from_text("").is_empty());
        assert!(agy_models_from_text("Please sign in to Antigravity to continue.\n").is_empty());
        assert!(agy_models_from_text("claude-sonnet-4-6\tClaude\ngpt-oss-120b-medium\tGPT-OSS\n").is_empty());
    }

    /// 本機沒有偵測到 agy 就不猜 PATH，直接是寫死的 3.8 Flash 三檔（測試環境不會去起真的 agy）。
    #[tokio::test]
    async fn agy_list_without_a_detected_binary_is_the_static_fallback() {
        let env = crate::testing::env().await;
        let v = list(&env.app, "local", "agy", None, true).await.unwrap();
        assert_eq!(v["source"], "static");
        let ids: Vec<&str> = v["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["gemini-3.8-flash-medium", "gemini-3.8-flash-high", "gemini-3.8-flash-low"]);
    }

    /// `/effort` 之後對話裡還留著上一則 `Switched to … (low effort)`。回讀必須用框底，不能用上面那則。
    #[test]
    fn grok_effort_readback_uses_the_composer_not_an_earlier_switch_line() {
        let screen = "\
  ⏺ Switched to Grok 4.7 (low effort)

  ╭────────────────────────────────────────────────╮
  │ ❯                                              │
  ╰────────────── Grok 4.7 (high) · always-approve ─╯
";
        assert_eq!(grok_effort_from_screen(screen).as_deref(), Some("high"));
    }

    /// Real `settings.json` shapes seen on this machine and on m4p. 「廠推薦」（`default_effort`）只看模型、不看帳號；
    /// 帳號 settings.json 記住的值另放 `account_default_effort`，且只在跟內建不同時才有（#1679）。
    #[test]
    fn claude_vendor_default_ignores_the_account_and_the_account_value_is_its_own_field() {
        let (global, per_model) = parse_claude_effort_settings(
            r#"{"effortLevel":"high","modelSettings":{"claude-opus-5-5":{"effortLevel":"low"}}}"#,
        );
        assert_eq!(global.as_deref(), Some("high"));
        assert_eq!(per_model.get("claude-opus-5-5").map(String::as_str), Some("low"));

        let models = claude_static_models(global.as_deref(), &per_model, None);
        let field = |models: &[Value], id: &str, f: &str| models.iter().find(|m| m["id"] == id).unwrap()[f].clone();
        assert_eq!(field(&models, "opus", "default_effort"), json!("medium"), "廠推薦不是帳號記住的 low");
        assert_eq!(field(&models, "opus", "account_default_effort"), json!("low"), "帳號對目前版本那一列的值");
        assert_eq!(field(&models, "sonnet", "default_effort"), json!("medium"));
        assert_eq!(field(&models, "sonnet", "account_default_effort"), json!("high"), "帳號全域 high 跟 sonnet 內建不同：另標");
        assert_eq!(field(&models, "fable", "default_effort"), json!("medium"));
        assert_eq!(field(&models, "fable", "account_default_effort"), json!("high"));
        assert_eq!(field(&models, "haiku", "default_effort"), json!("medium"));
        assert_eq!(field(&models, "haiku", "account_default_effort"), json!("high"), "帳號全域 high 跟 haiku 內建不同：另標");
        // 文件：頂層 effortLevel 對 Opus 5.5 不算數——沒有 opus-5-5 那一列時，全域 high 不會變成它的帳號值。
        let only_global = parse_claude_effort_settings(r#"{"effortLevel":"high"}"#);
        let m = claude_static_models(only_global.0.as_deref(), &only_global.1, None);
        assert_eq!(field(&m, "opus", "account_default_effort"), Value::Null);

        // m4p：沒有頂層 `effortLevel`，只有 fable 的列。
        let (global2, per_model2) =
            parse_claude_effort_settings(r#"{"modelSettings":{"claude-fable-5-1":{"effortLevel":"low"}}}"#);
        assert_eq!(global2, None);
        let models2 = claude_static_models(global2.as_deref(), &per_model2, None);
        assert_eq!(field(&models2, "fable", "default_effort"), json!("medium"));
        assert_eq!(field(&models2, "fable", "account_default_effort"), json!("low"));
        assert_eq!(field(&models2, "opus", "account_default_effort"), Value::Null);

        // 讀不到／不是 JSON：沒有帳號值，廠推薦照舊。
        let (g3, m3) = parse_claude_effort_settings("");
        assert_eq!(g3, None);
        assert!(m3.is_empty());
        let models3 = claude_static_models(g3.as_deref(), &m3, None);
        for m in &models3 {
            assert_eq!(m["account_default_effort"], Value::Null, "{m}");
        }
        assert_eq!(field(&models3, "opus", "default_effort"), json!("medium"));
        assert_eq!(field(&models3, "haiku", "default_effort"), json!("medium"));
        assert_eq!(field(&models3, "sonnet", "default_effort"), json!("medium"));
        assert_eq!(field(&models3, "fable", "default_effort"), json!("medium"));
    }

    /// #1679：同一個 opus，cc0（目前版本有列）與 cc1（只有舊版 opus-5 的列）的「廠推薦」一樣；
    /// cc1 沒有目前版本那一列，就沒有帳號值，更不能拿舊版的 xhigh 頂替。
    #[test]
    fn claude_vendor_default_is_the_same_on_every_account() {
        let cc0 = parse_claude_effort_settings(
            r#"{"modelSettings":{"claude-opus-5-5":{"effortLevel":"medium"},"claude-opus-5":{"effortLevel":"high"}}}"#,
        );
        let cc1 = parse_claude_effort_settings(
            r#"{"modelSettings":{"claude-opus-5":{"effortLevel":"xhigh"},"claude-fable-5-1":{"effortLevel":"low"}}}"#,
        );
        let list = |(g, m): &(Option<String>, BTreeMap<String, String>)| claude_static_models(g.as_deref(), m, None);
        let (m0, m1) = (list(&cc0), list(&cc1));
        let field = |models: &[Value], id: &str, f: &str| models.iter().find(|m| m["id"] == id).unwrap()[f].clone();
        for id in ["opus", "sonnet", "haiku", "fable"] {
            assert_eq!(field(&m0, id, "default_effort"), field(&m1, id, "default_effort"), "{id} 的廠推薦不能跟帳號走");
        }
        assert_eq!(field(&m0, "opus", "account_default_effort"), Value::Null, "cc0 記的 medium 跟內建一樣：不另外標");
        assert_eq!(field(&m1, "opus", "account_default_effort"), Value::Null, "cc1 沒有 claude-opus-5-5 這一列");
        assert_eq!(claude_effort_override(&cc1.1, "opus"), None, "不帶 --effort 實際跑的是內建預設，不是舊版的 xhigh");
        assert_eq!(claude_effort_override(&cc1.1, "claude-opus-5-5"), None);
    }

    /// issue #937／#1679：裸別名只對到「目前版本」那一列（`claude_current_model_id`），舊版的列不頂替；
    /// 完整 id 只認完全相等，不做子字串比對。CLI 自己也是照完整 model id 查 `modelSettings`。
    #[test]
    fn claude_effort_override_looks_up_only_the_current_version_of_an_alias() {
        let (global, per_model) = parse_claude_effort_settings(
            r#"{"modelSettings":{
                "claude-fable-5-1":{"effortLevel":"medium"},
                "claude-haiku-4-5":{"effortLevel":"medium"},
                "claude-opus-5":{"effortLevel":"high"},
                "claude-opus-5-5":{"effortLevel":"medium"},
                "claude-sonnet-5":{"effortLevel":"low"},
                "claude-sonnet-5-5":{"effortLevel":"high"}}}"#,
        );
        assert_eq!(global, None);
        let of = |m: &str| claude_effort_override(&per_model, m);
        assert_eq!(of("sonnet").as_deref(), Some("high"), "sonnet → claude-sonnet-5-5");
        assert_eq!(of("opus").as_deref(), Some("medium"));
        assert_eq!(of("claude-sonnet-5").as_deref(), Some("low"), "完整 id 只認完全相等，不會 contains 到 -5-5");
        assert_eq!(of("claude-sonnet-5-5").as_deref(), Some("high"));
        assert_eq!(of("haiku"), None, "目前版本是 claude-haiku-5-5；檔案只有舊的 4-5：不頂替");
        assert_eq!(of("claude-haiku-5-5"), None, "沒有這個 key：不做子字串比對");
        assert_eq!(of("fable").as_deref(), Some("medium"));
        assert_eq!(of("Claude-Sonnet-5-5").as_deref(), Some("high"), "大小寫不同仍算相等");
        assert_eq!(of("SONNET").as_deref(), Some("high"));
        assert_eq!(of("gpt-5"), None);

        let models = claude_static_models(global.as_deref(), &per_model, None);
        let eff = |id: &str| models.iter().find(|m| m["id"] == id).unwrap()["account_default_effort"].clone();
        assert_eq!(eff("sonnet"), json!("high"), "跟廠推薦 medium 不同：另標");
        assert_eq!(eff("opus"), Value::Null, "medium 跟廠推薦一樣：不另外標");
        assert_eq!(eff("haiku"), Value::Null);
    }

    /// 別名對到哪個完整 id 要跟啟動時一致：opus 就是 `remap_deprecated_model` 換成的那個。
    #[test]
    fn claude_current_model_ids_match_the_alias_remap() {
        assert_eq!(claude_current_model_id("opus"), Some("claude-opus-5-5"));
        assert_eq!(Some(claude_current_model_id("opus").unwrap()), remap_deprecated_model("claude", "opus"));
        for alias in ["opus", "sonnet", "haiku", "fable"] {
            let id = claude_current_model_id(alias).unwrap();
            assert_eq!(claude_alias_of(id), Some(alias), "{id} 要對回 {alias}");
        }
        assert_eq!(claude_current_model_id("claude-opus-5-5"), None, "只有裸別名才要解析");
    }

    /// #941：claude 的預設標記跟著帳號 settings.json 的頂層 `model`，不是寫死 opus。
    #[test]
    fn claude_default_mark_follows_settings_model() {
        let marks = |default: Option<&str>| -> Vec<String> {
            claude_static_models(None, &BTreeMap::new(), default)
                .iter()
                .filter(|m| m["is_default"] == json!(true))
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(marks(Some("sonnet")), ["sonnet"]);
        assert_eq!(marks(Some("claude-sonnet-5-5")), ["sonnet"]);
        assert_eq!(marks(Some("opus[1m]")), ["opus"]);
        assert_eq!(marks(Some("claude-opus-5-5")), ["opus"]);
        assert_eq!(marks(Some("claude-fable-5-1")), ["fable"]);
        // 沒設、或對不到任何系列：一顆都不標，不猜 opus。
        assert!(marks(None).is_empty());
        assert!(marks(Some("gpt-6-luna")).is_empty());
    }

    #[test]
    fn claude_alias_of_takes_whole_names_and_family_prefixes_only() {
        assert_eq!(claude_alias_of("sonnet"), Some("sonnet"));
        assert_eq!(claude_alias_of("Opus[1m]"), Some("opus"));
        assert_eq!(claude_alias_of("claude-haiku-5-5"), Some("haiku"));
        assert_eq!(claude_alias_of("claude-sonnet-5"), Some("sonnet"));
        assert_eq!(claude_alias_of("claude-opus"), Some("opus"));
        // 子字串不算：名字裡有家族名但不是那個家族的，對不到。
        assert_eq!(claude_alias_of("my-sonnet-experiment"), None);
        assert_eq!(claude_alias_of(""), None);
    }

    #[test]
    fn claude_settings_model_is_read_from_the_same_file() {
        assert_eq!(parse_claude_default_model(r#"{"model":"sonnet","effortLevel":"low"}"#).as_deref(), Some("sonnet"));
        assert_eq!(parse_claude_default_model(r#"{"model":"  "}"#), None);
        assert_eq!(parse_claude_default_model(r#"{"effortLevel":"high"}"#), None);
        assert_eq!(parse_claude_default_model(""), None);
    }

    /// #941：codex 身份 `config.toml` 頂層的 model／effort 才是它實際會跑的；只有 app-server 的 isDefault 不夠。
    #[test]
    fn codex_identity_config_sets_default_model_and_effort() {
        let models = || {
            vec![
                json!({"id": "gpt-6-sol", "is_default": true, "default_effort": "low"}),
                json!({"id": "gpt-6-luna", "is_default": false, "default_effort": "low"}),
            ]
        };
        // 模型與強度都有設：luna 成為預設，強度覆寫在 luna 上，sol 不動。
        let m = codex_apply_identity_config(models(), "model = \"gpt-6-luna\"\nmodel_reasoning_effort = \"high\"\n");
        assert_eq!(m[0]["is_default"], json!(false));
        assert_eq!(m[1]["is_default"], json!(true));
        assert_eq!(m[1]["default_effort"], json!("high"));
        assert_eq!(m[0]["default_effort"], json!("low"));
        // 只有強度：覆寫 app-server 回的預設模型。
        let m = codex_apply_identity_config(models(), "model_reasoning_effort = 'medium'\n");
        assert_eq!(m[0]["default_effort"], json!("medium"));
        assert_eq!(m[1]["default_effort"], json!("low"));
        // 設的模型不在清單裡：一顆都不標，不猜。
        let m = codex_apply_identity_config(models(), "model = \"gpt-9\"\n");
        assert!(m.iter().all(|v| v["is_default"] == json!(false)));
        // 沒設、讀不懂、或只在 [profiles] 裡：照 app-server 原樣。
        assert_eq!(codex_apply_identity_config(models(), ""), models());
        assert_eq!(codex_apply_identity_config(models(), "not toml = = ="), models());
        assert_eq!(codex_apply_identity_config(models(), "[profiles.x]\nmodel = \"gpt-6-luna\"\n"), models());
    }

    /// #941：身份的家目錄要真的帶進 `codex app-server`／`grok models` 的環境，讀檔也讀同一個目錄。
    #[test]
    fn identity_home_reaches_the_cli_env_and_its_files() {
        assert_eq!(home_env_prefix("CODEX_HOME", "/home/u/.codex-cx2"), "CODEX_HOME='/home/u/.codex-cx2'; export CODEX_HOME; ");
        assert_eq!(home_file_expr(Some("/home/u/.grok-g2"), ".grok", "config.toml"), "'/home/u/.grok-g2'/config.toml");
        // 沒有身份：照 CLI 預設讀 `$HOME/<預設目錄>`。
        assert_eq!(home_file_expr(None, ".grok", "models_cache.json"), "\"$HOME/.grok/models_cache.json\"");
    }

    #[test]
    fn grok_text_parses() {
        let t = "You are logged in with grok.com.\n\nDefault model: grok-4.5\n\nAvailable models:\n  - grok-4.6\n  * grok-4.5 (default)\n";
        let m = grok_models_from_text(t);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["id"], "grok-4.6");
        assert_eq!(m[0]["is_default"], false);
        assert_eq!(m[1]["id"], "grok-4.5");
        assert_eq!(m[1]["is_default"], true);
        assert_eq!(m[1]["efforts"][2], "high");
    }

    #[test]
    fn grok_cache_enriches_per_model_efforts() {
        let t = "Default model: grok-4.5\n\nAvailable models:\n  - grok-4.6\n  * grok-4.5 (default)\n";
        let m = grok_models_from_text(t);
        let cache = json!({
            "models": {
                "grok-4.6": {"info": {"reasoning_efforts": [
                    {"id": "xhigh", "value": "xhigh", "default": false},
                    {"id": "high", "value": "high", "default": true},
                    {"id": "medium", "value": "medium", "default": false},
                    {"id": "low", "value": "low", "default": false}
                ]}},
                "grok-4.5": {"info": {"reasoning_efforts": [
                    {"id": "high", "value": "high", "default": true},
                    {"id": "medium", "value": "medium", "default": false},
                    {"id": "low", "value": "low", "default": false}
                ]}}
            }
        });
        let m = enrich_grok_models(m, &cache.to_string(), "default_reasoning_effort = \"medium\"\n");
        assert_eq!(m[0]["efforts"], json!(["low", "medium", "high", "xhigh"]));
        assert_eq!(m[0]["default_effort"], "high");
        assert_eq!(m[1]["efforts"], json!(["low", "medium", "high"]));
        assert_eq!(m[1]["default_effort"], "high");
    }

    #[test]
    fn grok_config_effort_top_level_with_trailing_comment() {
        let cfg = "default_reasoning_effort = \"high\"  # 2026-09\n";
        assert_eq!(grok_default_effort_from_config(cfg).as_deref(), Some("high"));
    }

    #[test]
    fn grok_config_effort_models_table_and_single_quotes() {
        let cfg = "[models]\ndefault_reasoning_effort = 'medium' # note\n";
        assert_eq!(grok_default_effort_from_config(cfg).as_deref(), Some("medium"));
    }

    #[test]
    fn grok_config_effort_top_level_wins_over_models_table() {
        let cfg = "default_reasoning_effort = \"high\"\n[models]\ndefault_reasoning_effort = \"low\"\n";
        assert_eq!(grok_default_effort_from_config(cfg).as_deref(), Some("high"));
    }

    #[test]
    fn grok_config_effort_ignores_other_tables_and_junk() {
        // A same-named key under an unrelated table is not the global default.
        let cfg = "[models.\"grok-4.5\"]\ndefault_reasoning_effort = \"low\"\n";
        assert_eq!(grok_default_effort_from_config(cfg), None);
        assert_eq!(grok_default_effort_from_config("default_reasoning_effort = \"\"\n"), None);
        assert_eq!(grok_default_effort_from_config("default_reasoning_effort = 3\n"), None);
        assert_eq!(grok_default_effort_from_config("not toml = = =\n"), None);
        assert_eq!(grok_default_effort_from_config(""), None);
    }

    #[test]
    fn codex_rpc_result_maps() {
        let r = json!({"data": [{
            "id": "gpt-6-astra", "displayName": "GPT-6-Astra", "description": "d", "isDefault": true,
            "defaultReasoningEffort": "low",
            "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": ""}, {"reasoningEffort": "ultra", "description": ""}],
            "serviceTiers": [{"id": "priority", "name": "Fast", "description": "2x speed, increased usage"}]
        }]});
        let m = codex_models_from_rpc(&r);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["id"], "gpt-6-astra");
        assert_eq!(m[0]["default_effort"], "low");
        assert_eq!(m[0]["efforts"], json!(["low", "ultra"]));
        assert_eq!(m[0]["service_tiers"][0]["id"], "priority");
    }

    #[test]
    fn codex_previous_sol_is_remapped_exactly() {
        assert_eq!(remap_deprecated_model("codex", "gpt-6-sol"), Some("gpt-6.1-sol"));
        for model in ["gpt-6.1-sol", "gpt-6-sol-preview", "gpt-6-sol-extra", "gpt-6-astra", "gpt-6-luna"] {
            assert_eq!(remap_deprecated_model("codex", model), None, "{model}");
        }
        assert_eq!(remap_deprecated_model("claude", "gpt-6-sol"), None);
    }

    #[test]
    fn codex_previous_sol_catalog_transfers_default_and_identity_override() {
        let rpc = |old_default, new_default| json!({"data": [
            {"id":"gpt-6-sol", "isDefault":old_default},
            {"id":"gpt-6.1-sol", "isDefault":new_default, "defaultReasoningEffort":"medium"},
            {"id":"gpt-6-luna", "isDefault":false},
            {"id":"gpt-6-sol-preview", "isDefault":false}
        ]});
        for (old, new) in [(true, false), (false, true), (false, false)] {
            let models = codex_models_from_rpc(&rpc(old, new));
            assert_eq!(models.iter().map(|m| m["id"].as_str().unwrap()).collect::<Vec<_>>(),
                ["gpt-6.1-sol", "gpt-6-luna", "gpt-6-sol-preview"]);
            assert_eq!(models[0]["is_default"], json!(old || new));
            assert!(models[1..].iter().all(|m| m["is_default"] == false));
        }
        let models = codex_apply_identity_config(codex_models_from_rpc(&rpc(false, false)),
            "model = 'gpt-6-sol'\nmodel_reasoning_effort = 'high'");
        assert_eq!(models[0]["is_default"], true);
        assert_eq!(models[0]["default_effort"], "high");
        let models = codex_apply_identity_config(models, "model = 'gpt-6-luna'");
        assert_eq!(models[0]["is_default"], false);
        assert_eq!(models[1]["is_default"], true);
        assert!(codex_models_from_rpc(&json!({"data":[{"id":"gpt-6-sol", "isDefault":true}]})).is_empty());
        let missing_target = codex_models_from_rpc(&json!({"data":[
            {"id":"gpt-6-sol", "isDefault":true}, {"id":"gpt-6-luna", "isDefault":false}
        ]}));
        assert_eq!(missing_target.len(), 1);
        assert_eq!(missing_target[0]["is_default"], false);

    }

    #[test]
    fn deprecated_models_are_remapped_only_on_exact_matches() {
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-sol"), Some("gpt-6.1-sol"));
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-terra"), Some("gpt-6.1-sol"));
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-luna"), Some("gpt-6-luna"));
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-sol-preview"), None);
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-terra-extra"), None);
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-luna-extra"), None);
        assert_eq!(remap_deprecated_model("claude", "opus"), Some("claude-opus-5-5"));
        assert_eq!(remap_deprecated_model("claude", "claude-opus-4-1"), None);
        assert_eq!(remap_deprecated_model("claude", "claude-opus-5-5"), None);
    }

    #[test]
    fn adopted_child_argv_model_is_canonicalized_only_for_retired_exact_names() {
        let argv = |model: &str| vec!["codex".into(), "-m".into(), model.into()];
        assert_eq!(model_effort_from_argv("codex", &argv("gpt-5.6-sol")).0.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(model_effort_from_argv("codex", &argv("gpt-5.6-terra")).0.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(model_effort_from_argv("codex", &argv("gpt-5.6-luna")).0.as_deref(), Some("gpt-6-luna"));
        let argv = vec!["claude".into(), "--model".into(), "opus".into()];
        assert_eq!(model_effort_from_argv("claude", &argv).0.as_deref(), Some("claude-opus-5-5"));
        let argv = vec!["claude".into(), "--model".into(), "claude-opus-4-1".into()];
        assert_eq!(model_effort_from_argv("claude", &argv).0.as_deref(), Some("claude-opus-4-1"));
    }

    #[test]
    fn codex_model_catalog_hides_all_deprecated_56_models_exactly() {
        let r = json!({"data": [
            {"id": "gpt-5.6-sol", "displayName": "Old Sol"},
            {"id": "gpt-5.6-terra", "displayName": "Old Terra"},
            {"id": "gpt-5.6-luna", "displayName": "Old Luna"},
            {"id": "gpt-5.6-sol-preview", "displayName": "Preview"},
            {"id": "gpt-5.6-luna-preview", "displayName": "Other"},
            {"id": "gpt-6.1-sol", "displayName": "New Sol"},
            {"id": "gpt-6-luna", "displayName": "New"}
        ]});
        let mapped = codex_models_from_rpc(&r);
        let ids: Vec<_> = mapped.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["gpt-5.6-sol-preview", "gpt-5.6-luna-preview", "gpt-6.1-sol", "gpt-6-luna"]);
    }

    #[test]
    fn find_response_skips_notifications() {
        let text = "{\"method\":\"remoteControl/status/changed\",\"params\":{}}\n{\"id\":1,\"result\":{}}\n{\"id\":2,\"result\":{\"data\":[]}}\n";
        let r = find_response(text, 2).unwrap().unwrap();
        assert_eq!(r["data"], json!([]));
        assert!(find_response(text, 9).is_none());
        let err = "{\"id\":2,\"error\":{\"code\":-1,\"message\":\"nope\"}}";
        assert!(find_response(err, 2).unwrap().is_err());
    }
