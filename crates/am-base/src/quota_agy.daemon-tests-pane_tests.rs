
    use crate::runners::quota_agy::*;
    use crate::state::App;
    use std::sync::Arc;
    use super::*;
    use crate::herdr::HerdrClient;
    use crate::testing::MockHerdr;
    use serde_json::json;

    struct Remote {
        host: String,
        herdr: MockHerdr,
        ssh_calls: Arc<std::sync::Mutex<Vec<String>>>,
        _dir: std::path::PathBuf,
    }

    async fn remote(app: &Arc<App>, logged_in: Option<bool>) -> Remote {
        let host = format!("agy-pane-{}", crate::db::ulid().to_ascii_lowercase());
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-agy-pane-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let herdr = MockHerdr::start(sock.clone());
        let conn = app
            .hosts
            .insert_remote_with_client_for_test(
                crate::config::HostCfg {
                    shared_session: false,
                    name: host.clone(),
                    ssh: "unused".into(),
                    ssh_port: 22,
                    ssh_opts: vec![],
                    herdr_session: "agents-manager".into(),
                    remote_path: String::new(),
                },
                HerdrClient::new(sock),
            )
            .await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        *conn.remote_home.lock().await = Some("/Users/m4p".into());
        // 純 ssh 讀不到 Keychain：探測**不能**走 ssh（記下任何 ssh 呼叫）。
        let ssh_calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls = ssh_calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls.lock().unwrap().push(script.to_string());
            Err(anyhow!("ssh must not be used for the agy quota probe"))
        });
        let ht = crate::tools::HostTools {
            tools: [("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/Users/m4p/.local/bin/agy".into()), version: None, logged_in })].into(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert(host.clone(), ht);
        Remote { host, herdr, ssh_calls, _dir: dir }
    }

    fn screen(body: &str, rc: i32) -> String {
        format!("{PANE_BEGIN}\n{body}\n{PANE_DONE}{rc}\n")
    }

    fn usage_json() -> String {
        json!({"status": "SUCCESS", "command": {"data": {"groups": [{"name": "Gemini Models", "buckets": [
            {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.98, "reset_time": "2026-10-11T15:39:29Z"},
            {"id": "gemini-5h", "window": "five_hour", "remaining_fraction": 0.23, "reset_time": "2026-10-05T15:00:00Z"}
        ]}]}}})
        .to_string()
    }

    async fn agy_flag(app: &Arc<App>, host: &str) -> Option<bool> {
        app.tools.lock().await[host].tools["agy"].logged_in
    }

    async fn open_workspaces(h: &MockHerdr) -> usize {
        for _ in 0..40 {
            if h.workspaces.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        h.workspaces.lock().unwrap().len()
    }

    fn quota_error_of(app_tools: &crate::tools::HostTools, host: &str) -> serde_json::Value {
        tools_json(host, &app_tools.tools)["agy"]["quota_error"].clone()
    }

    #[tokio::test]
    async fn a_remote_probe_runs_in_a_pane_not_over_ssh_and_stores_both_gemini_windows() {
        let app = crate::testing::env().await.app.clone();
        let r = remote(&app, Some(true)).await;
        r.herdr.set_screen("*", &screen(&usage_json(), 0));
        assert!(refresh_agy(&app, &r.host).await.unwrap());
        let q = app.quotas.lock().await;
        let got = &q[&format!("{}/agy", r.host)];
        assert!(got.five_hour.is_some() && got.seven_day.is_some(), "{got:?}");
        assert!(!q.contains_key("agy"), "遠端的讀數不能落在本機那格");
        drop(q);
        assert_eq!(r.ssh_calls.lock().unwrap().len(), 0, "不走 ssh（macOS 的 Keychain 純 ssh 讀不到）");
        let creates = r.herdr.calls_to("workspace.create");
        assert_eq!(creates.len(), 1, "{creates:?}");
        assert_eq!(creates[0]["cwd"], "/Users/m4p", "在遠端 home 開 pane");
        let typed: String = r.herdr.calls_to("pane.send_text").iter().filter_map(|c| c["text"].as_str().map(String::from)).collect();
        assert!(typed.contains("'/Users/m4p/.local/bin/agy' -p /usage --output-format json"), "{typed}");
        assert_eq!(open_workspaces(&r.herdr).await, 0, "探測完 workspace 關掉");
        assert_eq!(agy_flag(&app, &r.host).await, Some(true));
        assert!(quota_error_of(&app.tools.lock().await[&r.host], &r.host).is_null());
    }

    /// agy 在 pane 裡明說沒憑證：不等到逾時，旗標翻成未登入（網頁出現登入鈕），不留「額度暫時拿不到」的錯誤，
    /// 而且登入偵測冷卻期內不把旗標又翻回去。
    #[tokio::test]
    async fn authentication_required_in_the_pane_marks_logged_out_at_once() {
        let app = crate::testing::env().await.app.clone();
        let r = remote(&app, Some(true)).await;
        r.herdr.set_screen("*", &format!("{PANE_BEGIN}\nAuthentication required. Please visit the URL to log in\nhttps://accounts.example/x\n"));
        let started = std::time::Instant::now();
        let err = refresh_agy(&app, &r.host).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2), "不必等逾時：{:?}", started.elapsed());
        assert!(err.to_string().contains("not logged in"), "{err:#}");
        assert_eq!(agy_flag(&app, &r.host).await, Some(false));
        assert!(quota_error_of(&app.tools.lock().await[&r.host], &r.host).is_null(), "未登入由旗標表達，不另記額度錯誤");
        assert_eq!(open_workspaces(&r.herdr).await, 0, "卡在登入畫面的 pane 也關掉");
        assert_eq!(r.ssh_calls.lock().unwrap().len(), 0);

        // 冷卻期內：Keychain 裡有項目也不翻回去、不再開 pane。
        let before = r.herdr.calls_to("workspace.create").len();
        login_watch_once(&app, &r.host).await;
        assert_eq!(agy_flag(&app, &r.host).await, Some(false));
        assert_eq!(r.herdr.calls_to("workspace.create").len(), before);
    }

    /// 已登入但這次拿不到額度：原因記起來、跟 `tools.agy` 一起送出去（網頁寫「已登入，額度暫時拿不到」），旗標不動；下一次成功就清掉。
    #[tokio::test]
    async fn a_probe_that_times_out_keeps_logged_in_and_reports_why_until_it_recovers() {
        let app = crate::testing::env().await.app.clone();
        let r = remote(&app, Some(true)).await;
        let mut rx = app.subscribe();
        r.herdr.set_screen("*", &format!("{PANE_BEGIN}\n"));
        let err = refresh_agy(&app, &r.host).await.unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err:#}");
        assert_eq!(agy_flag(&app, &r.host).await, Some(true), "逾時不等於未登入");
        let e = quota_error_of(&app.tools.lock().await[&r.host], &r.host);
        assert_eq!(e["reason"], "timeout", "{e}");
        assert!(e["message"].as_str().unwrap().contains(&r.host) && e["at"].is_string(), "{e}");
        let mut pushed = false;
        while let Ok(ev) = rx.try_recv() {
            pushed |= ev.kind == "host_changed" && ev.data["name"] == r.host.as_str() && ev.data["tools"]["agy"]["quota_error"]["reason"] == "timeout";
        }
        assert!(pushed, "失敗原因要推 host_changed，網頁才換掉「背景查詢中」");

        // 恢復：下一次讀得到，錯誤清掉並再推一次。
        r.herdr.set_screen("*", &screen(&usage_json(), 0));
        assert!(refresh_agy(&app, &r.host).await.unwrap());
        assert!(quota_error_of(&app.tools.lock().await[&r.host], &r.host).is_null());
        let mut cleared = false;
        while let Ok(ev) = rx.try_recv() {
            cleared |= ev.kind == "host_changed" && ev.data["tools"]["agy"].get("quota_error").is_none();
        }
        assert!(cleared, "成功後清掉並推出去");
    }

    #[tokio::test]
    async fn an_unreadable_or_failing_agy_names_its_reason() {
        let app = crate::testing::env().await.app.clone();
        let r = remote(&app, Some(true)).await;
        r.herdr.set_screen("*", &screen("{\"status\":\"SUCCESS\"}", 0));
        refresh_agy(&app, &r.host).await.unwrap_err();
        assert_eq!(quota_error_of(&app.tools.lock().await[&r.host], &r.host)["reason"], "unreadable");
        r.herdr.set_screen("*", &screen("agy: command not found", 127));
        let err = refresh_agy(&app, &r.host).await.unwrap_err();
        assert!(err.to_string().contains("exited with 127"), "{err:#}");
        assert_eq!(quota_error_of(&app.tools.lock().await[&r.host], &r.host)["reason"], "exit");
        assert_eq!(agy_flag(&app, &r.host).await, Some(true));
    }

    #[test]
    fn the_failure_record_does_not_touch_other_hosts_or_other_tools() {
        let tools: std::collections::BTreeMap<String, crate::tools::ToolInfo> =
            [("agy".to_string(), Default::default()), ("claude".to_string(), Default::default())].into();
        let host = format!("agy-json-{}", crate::db::ulid().to_ascii_lowercase());
        assert_eq!(tools_json(&host, &tools), json!(tools), "沒有失敗記錄：跟原本的序列化一模一樣");
        assert!(set_probe_error(&host, Some(ProbeError { reason: "timeout", message: "m".into(), at: "t".into() })));
        assert!(!set_probe_error(&host, Some(ProbeError { reason: "timeout", message: "m".into(), at: "later".into() })), "同樣的原因不重複推");
        let v = tools_json(&host, &tools);
        assert_eq!(v["agy"]["quota_error"]["reason"], "timeout");
        assert!(v["claude"].get("quota_error").is_none());
        assert_eq!(tools_json("other-host", &tools), json!(tools));
        assert!(set_probe_error(&host, None));
        assert_eq!(tools_json(&host, &tools), json!(tools));
    }

    #[test]
    fn the_pane_output_parsers_read_the_last_complete_run_and_spot_the_login_wall() {
        // 指令重打過：捲動緩衝區有兩輪，只認最後那輪完整的。
        let two = format!("{PANE_BEGIN}\nold\n{PANE_BEGIN}\n{{\"a\":1}}\n{PANE_DONE}0\n");
        assert_eq!(pane_done(&two), Some((0, "{\"a\":1}".to_string())));
        assert_eq!(pane_done(&format!("{PANE_BEGIN}\nstill running")), None, "沒有 DONE＝還沒結束");
        assert_eq!(pane_done(&format!("{PANE_DONE}1\n")), Some((1, String::new())), "mktemp 失敗：只有 DONE");
        assert_eq!(pane_done(&format!("{PANE_BEGIN}\nx\n{PANE_DONE}zz\n")).map(|x| x.0), Some(1), "結束碼讀不懂當失敗");
        assert!(auth_required("Authentication required. Please visit the URL to log in"));
        assert!(auth_required("AUTHENTICATION REQUIRED"));
        assert!(!auth_required("{\"status\":\"SUCCESS\"}"));
        assert_eq!(json_line("warning: x\n  {\"a\":1}  \ntrailer"), Some("{\"a\":1}"));
        assert_eq!(json_line("no json here"), None);
        // 登入牆只看最後一次 BEGIN 之後：之前畫面上的字不算。
        assert!(!auth_required(after_begin(&format!("Authentication required\n{PANE_BEGIN}\nok\n"))));
    }

    /// 真的把 pane 那一行丟給 sh：假 agy 在別的目錄跑、目錄用完就刪、標記與結束碼印得出來；壞掉的 agy 結束碼傳得出來。
    #[test]
    fn the_pane_command_runs_in_a_throwaway_dir_and_prints_the_markers() {
        let dir = crate::testing::scratch_dir("am-agy-pane-cmd");
        let seen = dir.join("seen.txt");
        let fake = dir.join("agy");
        crate::testing::write_exec(&fake, format!("#!/bin/sh\npwd > {s}\necho \"$AGY_CLI_DISABLE_AUTO_UPDATE $*\" >> {s}\nprintf '%s\\n' '{j}'\nexit 3\n", s = seen.display(), j = usage_json()));
        let out = crate::exec_retry::output(std::process::Command::new("/bin/sh").arg("-c").arg(pane_probe_command(fake.to_str()))).unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let (rc, body) = pane_done(&text).expect(&text);
        assert_eq!(rc, 3, "agy 的結束碼傳得出來");
        assert!(read_usage("h", Some(rc), &body).is_ok(), "JSON 那行照樣讀得到");
        let seen = std::fs::read_to_string(&seen).unwrap();
        let mut lines = seen.lines();
        let cwd = lines.next().unwrap();
        assert!(!cwd.starts_with(dir.to_str().unwrap()) && !std::path::Path::new(cwd).exists(), "拋棄式 cwd 用完刪掉：{cwd}");
        assert_eq!(lines.next().unwrap(), "true -p /usage --output-format json");
        assert!(!pane_probe_command(None).contains(PANE_BEGIN) && !pane_probe_command(None).contains(PANE_DONE), "回顯的指令不能長得像標記");
    }

    /// claude 與 agy 的探測 workspace 各清各的殘留：同台主機同時跑時，不能互相收掉對方正在跑的。
    #[test]
    fn claude_and_agy_probe_labels_do_not_sweep_each_other() {
        assert!(is_probe_label("am-quota-agy") && !is_probe_label("am-quota-claude") && !is_probe_label("am-quota-claude-cc1") && !is_probe_label("am-quota-grok"));
        assert!(crate::shared_host::sweepable("am-quota-agy@t", Some("t"), is_probe_label));
        assert!(!crate::shared_host::sweepable("am-quota-agy@other", Some("t"), is_probe_label));
    }
