
    use crate::runners::quota_agy::*;
    use crate::state::App;
    use std::sync::Arc;
    use super::*;
    use crate::testing as tt;
    use serde_json::json;

    fn token_path() -> std::path::PathBuf {
        crate::home::dir().unwrap().join(TOKEN_FILE)
    }

    async fn install_agy(app: &Arc<App>, logged_in: Option<bool>) {
        let ht = crate::tools::HostTools {
            tools: [("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/nonexistent/agy".into()), version: None, logged_in })].into(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert(LOCAL_HOST.into(), ht);
    }

    async fn agy_logged_in(app: &Arc<App>) -> Option<bool> {
        app.tools.lock().await[LOCAL_HOST].tools["agy"].logged_in
    }

    /// 登出：憑證檔刪了、`tools.agy.logged_in` 立刻是 false 並推 `host_changed`（那一格馬上變「未登入」＋出現登入鈕）。
    #[tokio::test]
    async fn logging_out_flips_the_login_flag_at_once_and_pushes_host_changed() {
        let _lock = token_test_lock().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let tok = token_path();
        std::fs::create_dir_all(tok.parent().unwrap()).unwrap();
        std::fs::write(&tok, "secret").unwrap();
        install_agy(&app, Some(true)).await;
        let mut rx = app.subscribe();
        logout(&app, LOCAL_HOST).await.unwrap();
        assert_eq!(agy_logged_in(&app).await, Some(false));
        let mut pushed = false;
        while let Ok(ev) = rx.try_recv() {
            pushed |= ev.kind == "host_changed" && ev.data["tools"]["agy"]["logged_in"] == false;
        }
        assert!(pushed, "要推 host_changed，網頁才會翻成未登入");
    }

    /// 行程全域的冷卻與探測錯誤（key 是本機）：這幾條測試持有 `token_test_lock`，進出場都清乾淨。
    struct Fresh {
        tok: std::path::PathBuf,
    }

    impl Fresh {
        fn new() -> Self {
            Self::reset();
            Self { tok: token_path() }
        }

        fn reset() {
            let key = crate::quota::quota_key(LOCAL_HOST, "agy");
            auth_denied().lock().unwrap().remove(&key);
            backoff().lock().unwrap().remove(&key);
            set_probe_error(LOCAL_HOST, None);
            let _ = std::fs::remove_file(token_path());
        }

        fn write_token(&self) {
            std::fs::create_dir_all(self.tok.parent().unwrap()).unwrap();
            std::fs::write(&self.tok, "secret").unwrap();
        }
    }

    impl Drop for Fresh {
        fn drop(&mut self) {
            Self::reset();
        }
    }

    /// 假的 agy：每次被呼叫在 `calls` 記一行，再執行 `body`。回傳（可執行檔路徑、呼叫記錄）。
    fn fake_agy(dir: &std::path::Path, name: &str, body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(dir).unwrap();
        let calls = dir.join(format!("{name}.calls"));
        let exe = dir.join(name);
        std::fs::write(&exe, format!("#!/bin/sh\necho call >> '{}'\n{body}\n", calls.display())).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        (exe, calls)
    }

    fn call_count(calls: &std::path::Path) -> usize {
        std::fs::read_to_string(calls).map(|t| t.lines().count()).unwrap_or(0)
    }

    async fn install_agy_at(app: &Arc<App>, logged_in: Option<bool>, path: &std::path::Path) {
        let ht = crate::tools::HostTools {
            tools: [("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some(path.display().to_string()), version: None, logged_in })].into(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert(LOCAL_HOST.into(), ht);
    }

    fn usage_json_line() -> String {
        json!({"status": "SUCCESS", "command": {"data": {"groups": [{"name": "Gemini Models", "buckets": [
            {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.98, "reset_time": "2026-10-11T15:39:29Z"},
            {"id": "gemini-5h", "window": "five_hour", "remaining_fraction": 0.23, "reset_time": "2026-10-05T15:00:00Z"}
        ]}]}}})
        .to_string()
    }

    /// 網頁看到的 `tools.agy.quota_error`（`Null` ＝沒有探測錯誤）。
    async fn quota_error(app: &Arc<App>) -> serde_json::Value {
        let all = app.tools.lock().await;
        tools_json(LOCAL_HOST, &all[LOCAL_HOST].tools)["agy"].get("quota_error").cloned().unwrap_or(serde_json::Value::Null)
    }

    fn drain_host_changed(rx: &mut tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> Vec<Option<bool>> {
        std::iter::from_fn(|| rx.try_recv().ok()).filter(|ev| ev.kind == "host_changed").map(|ev| ev.data["tools"]["agy"]["logged_in"].as_bool()).collect()
    }

    /// 憑證檔還沒出現就什麼都不動；沒問出登入與否（`None`）也不猜。
    #[tokio::test]
    async fn the_watcher_does_nothing_without_a_token_file_or_a_known_login_state() {
        let _lock = token_test_lock().await;
        // 登入冷卻是行程全域的（key 是本機）：別條測試（例如 agy 回合授權失敗）可能已經留下一個，watcher 就不會翻旗標。
        // `Fresh::new()` 進場先清掉（冷卻、退避、探測錯誤、憑證檔），離場也清。
        let fresh = Fresh::new();
        let e = tt::env().await;
        let app = e.app.clone();
        let (exe, calls) = fake_agy(&e.dir.join("fake-agy"), "agy", &format!("echo '{}'", usage_json_line()));
        install_agy_at(&app, Some(false), &exe).await;
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, Some(false), "檔案還沒出現：不動");
        assert_eq!(call_count(&calls), 0, "沒有憑證檔：不探測");

        install_agy_at(&app, None, &exe).await;
        fresh.write_token();
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, None, "沒問出登入與否：不猜");
        assert_eq!(call_count(&calls), 0);
    }

    /// 登入之後不用重啟（issue #870）：憑證檔出現 → 先探測 `/usage`，**成功才**翻成已登入並推 `host_changed`，額度同時回到那一格。
    #[tokio::test]
    async fn the_watcher_flips_to_logged_in_only_after_the_usage_probe_succeeds() {
        let _lock = token_test_lock().await;
        let fresh = Fresh::new();
        let e = tt::env().await;
        let app = e.app.clone();
        let (exe, calls) = fake_agy(&e.dir.join("fake-agy"), "agy", &format!("echo '{}'", usage_json_line()));
        install_agy_at(&app, Some(false), &exe).await;
        set_probe_error(LOCAL_HOST, Some(ProbeError { reason: "timeout", message: "old".into(), at: "t".into() }));
        fresh.write_token();
        let mut rx = app.subscribe();
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(call_count(&calls), 1, "探測了一次");
        assert_eq!(agy_logged_in(&app).await, Some(true), "探測成功：翻成已登入");
        assert!(drain_host_changed(&mut rx).contains(&Some(true)), "要推 host_changed");
        assert!(app.quotas.lock().await.contains_key("agy"), "兩條額度回到那一格");
        assert!(quota_error(&app).await.is_null(), "舊的探測錯誤清掉");
    }

    /// 憑證檔在、但 agy 明說要登入（token 過期／被撤銷）：旗標維持未登入，記 5 分鐘冷卻，之後不再因憑證檔還在而探測。
    #[tokio::test]
    async fn an_explicit_auth_failure_keeps_the_host_logged_out_and_starts_the_cooldown() {
        let _lock = token_test_lock().await;
        let fresh = Fresh::new();
        let e = tt::env().await;
        let app = e.app.clone();
        let (exe, calls) = fake_agy(&e.dir.join("fake-agy"), "agy", "echo 'Authentication required' >&2; exit 1");
        install_agy_at(&app, Some(false), &exe).await;
        fresh.write_token();
        let mut rx = app.subscribe();
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, Some(false), "auth 失敗：維持未登入");
        assert!(!drain_host_changed(&mut rx).contains(&Some(true)), "從沒翻成已登入過");
        assert!(auth_denied_active(&crate::quota::quota_key(LOCAL_HOST, "agy")), "記下 auth_denied 冷卻");
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(call_count(&calls), 1, "冷卻期內不再探測");
    }

    /// agy 印了 `Authentication required` 就卡住等人去開網址：逾時前已讀到的輸出要認得出，不是單純的逾時（issue #870）。
    #[tokio::test]
    async fn a_login_wall_printed_before_a_timeout_still_counts_as_auth_failure() {
        let _lock = token_test_lock().await;
        let fresh = Fresh::new();
        let e = tt::env().await;
        let app = e.app.clone();
        let (exe, _calls) = fake_agy(&e.dir.join("fake-agy"), "agy", "echo 'Authentication required'; echo 'Please visit the URL to log in'; sleep 30");
        install_agy_at(&app, Some(false), &exe).await;
        fresh.write_token();
        let t0 = std::time::Instant::now();
        login_watch_once(&app, LOCAL_HOST).await;
        assert!(t0.elapsed() < std::time::Duration::from_secs(25), "逾時後行程群組被收掉");
        assert_eq!(agy_logged_in(&app).await, Some(false));
        assert!(auth_denied_active(&crate::quota::quota_key(LOCAL_HOST, "agy")), "認得出是 auth 失敗，不是逾時：記 auth_denied 冷卻");
        assert!(quota_error(&app).await.is_null(), "不是探測錯誤");
    }

    /// 其他失敗（網路、逾時、讀不懂）：旗標保持現值、記探測錯誤，15 分鐘冷卻內不再探測。
    #[tokio::test]
    async fn a_non_auth_probe_failure_keeps_the_flag_records_the_error_and_backs_off() {
        let _lock = token_test_lock().await;
        let fresh = Fresh::new();
        let e = tt::env().await;
        let app = e.app.clone();
        let (exe, calls) = fake_agy(&e.dir.join("fake-agy"), "agy", "echo 'dial tcp: connection refused' >&2; exit 1");
        install_agy_at(&app, Some(false), &exe).await;
        fresh.write_token();
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, Some(false), "網路類失敗：保持現值，不翻");
        assert_eq!(quota_error(&app).await["reason"], "unreadable", "記下探測錯誤");
        assert!(!auth_denied_active(&crate::quota::quota_key(LOCAL_HOST, "agy")), "不是 auth 失敗：不記 auth_denied");
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(call_count(&calls), 1, "失敗冷卻內不再探測（不然每 20 秒起一次 200 MB 的執行檔）");

        // 保持現值也包括「已登入」：已登入的主機遇到網路類失敗不會被翻成未登入（那是 refresh 的老行為）。
        install_agy_at(&app, Some(true), &exe).await;
        backoff().lock().unwrap().remove(&crate::quota::quota_key(LOCAL_HOST, "agy"));
        let _ = refresh_agy(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, Some(true));
    }

    /// 已知未登入的主機，定期輪詢不去跑 `agy -p /usage`（那會停在登入畫面等到逾時）。
    #[tokio::test]
    async fn a_logged_out_host_is_not_probed_by_the_poller() {
        let e = tt::env().await;
        install_agy(&e.app, Some(false)).await;
        assert!(matches!(refresh_agy_if_due(&e.app, LOCAL_HOST).await, Ok(None)));
    }
