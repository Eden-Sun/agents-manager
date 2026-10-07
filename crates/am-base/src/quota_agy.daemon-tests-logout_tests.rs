
    use crate::runners::quota_agy::*;
    use crate::state::App;
    use std::sync::Arc;
    use super::*;
    use crate::testing as tt;

    fn token_path() -> std::path::PathBuf {
        crate::home::dir().unwrap().join(TOKEN_FILE)
    }

    async fn seed(app: &Arc<App>, host: &str) {
        for (key, q) in parse_usage(&json!({"response": "Gemini Models\tWeekly Limit Remaining\t98%\t2026-10-11T15:39:29Z\nClaude and GPT models\tWeekly Limit Remaining\t100%\t2026-10-11T15:56:55Z\n"}).to_string()).unwrap() {
            crate::quota::set(app, host, key, q).await;
        }
    }

    use serde_json::json;

    /// 本機：刪假 HOME 底下的憑證檔、清 Gemini 額度 key（連重啟快取）並廣播 `quota:null`；別的檔不動。
    #[tokio::test]
    async fn logging_out_removes_only_the_token_clears_gemini_quota_and_broadcasts() {
        let _lock = token_test_lock().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let tok = token_path();
        let other = tok.with_file_name("settings.json");
        std::fs::create_dir_all(tok.parent().unwrap()).unwrap();
        std::fs::write(&tok, "secret").unwrap();
        std::fs::write(&other, "{}").unwrap();
        seed(&app, LOCAL_HOST).await;
        let mut rx = app.subscribe();

        assert!(logout(&app, LOCAL_HOST).await.unwrap(), "檔案在 → removed");
        assert!(!tok.exists(), "憑證檔被刪");
        assert!(other.exists(), "不動別的檔");
        let q = app.quotas.lock().await;
        assert!(!q.contains_key("agy"), "Gemini 額度快照清掉");
        drop(q);
        let cached: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quota_cache WHERE key = 'agy'").fetch_one(&app.db).await.unwrap();
        assert_eq!(cached, 0, "重啟快取也清掉，不然重開機又顯示舊數字");
        let mut cleared = std::collections::BTreeSet::new();
        while let Ok(ev) = rx.try_recv() {
            if ev.kind == "quota_updated" && ev.data["quota"].is_null() {
                cleared.insert(ev.data["kind"].as_str().unwrap().to_string());
            }
        }
        assert_eq!(cleared, ["agy".to_string()].into(), "推一則 quota:null");

        // 檔案原本不在：false，不算錯。
        assert!(!logout(&app, LOCAL_HOST).await.unwrap());
        std::fs::remove_file(&other).unwrap();
    }

    #[tokio::test]
    async fn an_unknown_host_is_unknown_and_other_hosts_quota_is_untouched() {
        let _lock = token_test_lock().await;
        let e = tt::env().await;
        let app = e.app.clone();
        assert!(matches!(logout(&app, "no-such-host").await, Err(LogoutError::UnknownHost)));
        // 遠端那台的 agy 額度不受本機登出影響。
        let host = format!("agy-lo-{}", crate::db::ulid().to_ascii_lowercase());
        app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        seed(&app, &host).await;
        seed(&app, LOCAL_HOST).await;
        logout(&app, LOCAL_HOST).await.unwrap();
        assert!(app.quotas.lock().await.contains_key(&format!("{host}/agy")), "別台的快照還在");
    }

    /// 遠端：走 ssh（同探測那條），只送一段刪檔的 script；回 `AM_REMOVED`／`AM_ABSENT`／其他（失敗、502）。
    #[tokio::test]
    async fn a_remote_logout_runs_one_rm_script_over_ssh_and_clears_that_hosts_keys() {
        let e = tt::env().await;
        let app = e.app.clone();
        let host = format!("agy-lo-r-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        let replies = Arc::new(std::sync::Mutex::new(vec!["AM_REMOVED\n".to_string(), "AM_ABSENT\n".to_string(), "boom\n".to_string()]));
        let scripts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (r2, s2) = (replies.clone(), scripts.clone());
        crate::hosts::set_ssh_fake(&host, move |script| {
            s2.lock().unwrap().push(script.to_string());
            Ok(r2.lock().unwrap().remove(0))
        });
        // 沒連線：不送 ssh，回錯。
        conn.connected.store(false, std::sync::atomic::Ordering::SeqCst);
        seed(&app, &host).await;
        assert!(matches!(logout(&app, &host).await, Err(LogoutError::Failed(m)) if m.contains("not connected")));
        assert!(scripts.lock().unwrap().is_empty());
        assert!(app.quotas.lock().await.contains_key(&format!("{host}/agy")), "失敗不清快照");

        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(logout(&app, &host).await.unwrap());
        let sent = scripts.lock().unwrap()[0].clone();
        assert!(sent.contains(".gemini/antigravity-cli/antigravity-oauth-token") && sent.contains("rm -f") && !sent.contains("agy -p"), "{sent}");
        assert!(!app.quotas.lock().await.contains_key(&format!("{host}/agy")));

        assert!(!logout(&app, &host).await.unwrap(), "AM_ABSENT → false");
        seed(&app, &host).await;
        assert!(matches!(logout(&app, &host).await, Err(LogoutError::Failed(m)) if m.contains("did not confirm")), "認不得的回覆＝失敗，快照留著");
        assert!(app.quotas.lock().await.contains_key(&format!("{host}/agy")));
    }
