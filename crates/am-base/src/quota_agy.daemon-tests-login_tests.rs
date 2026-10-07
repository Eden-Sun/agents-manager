
    use crate::runners::quota_agy::*;
    use crate::state::App;
    use std::sync::Arc;
    use super::*;
    use crate::testing as tt;

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

    /// 登入之後不用重啟：憑證檔出現，下一輪 watcher 就把旗標翻成已登入並探測一次額度（假 agy 路徑讀不到輸出＝探測失敗，旗標仍已翻）。
    /// 憑證檔還沒出現就什麼都不動；沒問出登入與否（`None`）也不猜。
    #[tokio::test]
    async fn the_watcher_flips_to_logged_in_once_the_token_file_appears() {
        let _lock = token_test_lock().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let tok = token_path();
        let _ = std::fs::remove_file(&tok);
        install_agy(&app, Some(false)).await;

        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, Some(false), "檔案還沒出現：不動");

        install_agy(&app, None).await;
        std::fs::create_dir_all(tok.parent().unwrap()).unwrap();
        std::fs::write(&tok, "secret").unwrap();
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, None, "沒問出登入與否：不猜");

        install_agy(&app, Some(false)).await;
        let mut rx = app.subscribe();
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, Some(true), "憑證檔出現：翻成已登入");
        let mut pushed = false;
        while let Ok(ev) = rx.try_recv() {
            pushed |= ev.kind == "host_changed" && ev.data["tools"]["agy"]["logged_in"] == true;
        }
        assert!(pushed, "要推 host_changed");
        std::fs::remove_file(&tok).unwrap();
    }

    /// 已知未登入的主機，定期輪詢不去跑 `agy -p /usage`（那會停在登入畫面等到逾時）。
    #[tokio::test]
    async fn a_logged_out_host_is_not_probed_by_the_poller() {
        let e = tt::env().await;
        install_agy(&e.app, Some(false)).await;
        assert!(matches!(refresh_agy_if_due(&e.app, LOCAL_HOST).await, Ok(None)));
    }
