    use crate::runners::quota_agy::*;
    use crate::state::App;
    use std::sync::Arc;
    use super::*;
    use crate::testing as tt;

    fn remote_cfg(host: &str) -> crate::config::HostCfg {
        crate::config::HostCfg {
            shared_session: false,
            name: host.to_string(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }
    }

    /// 全域 map 以 host 名為 key，測試各用自己的名字避免互相干擾。
    fn fresh_host() -> String {
        format!("agy-bo-{}", crate::db::ulid().to_ascii_lowercase())
    }

    async fn add_host(app: &Arc<App>, host: &str) -> Arc<crate::hosts::HostConn> {
        // 不連線：探測在 `client_for_fence` 就失敗，不會真的開 ssh。
        let conn = app.hosts.insert_remote_for_test(remote_cfg(host)).await;
        conn.connected.store(false, std::sync::atomic::Ordering::SeqCst);
        conn
    }

    /// 種一份「agy 已裝、已登入、path 指到不存在的檔」，讓 `refresh_agy_if_due` 會真的去探測（失敗）而不是 `Ok(None)`／`tools::detect`。
    async fn install_agy(app: &Arc<App>, host: &str) {
        let ht = crate::tools::HostTools {
            tools: [("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/nonexistent/agy".into()), version: None, logged_in: Some(true) })].into(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert(host.to_string(), ht);
    }

    fn key_of(host: &str) -> String {
        crate::quota::quota_key(host, "agy")
    }

    /// 舊主機（A）的失敗不能讓同名改指後的新主機（B）進入冷卻。
    #[tokio::test]
    async fn an_old_hosts_failure_does_not_back_off_the_new_host() {
        let e = tt::env().await;
        let app = e.app.clone();
        let host = fresh_host();
        add_host(&app, &host).await;
        let a = app.hosts.fence(&host).await.unwrap();
        // H 改指到 B。
        add_host(&app, &host).await;
        let b = app.hosts.fence(&host).await.unwrap();

        assert!(!note_failure(&app, &host, &a).await, "A 的世代已經不是權威，不寫冷卻");
        assert!(!backoff_active(&key_of(&host), &b));

        install_agy(&app, &host).await;
        let r = refresh_agy_if_due(&app, &host).await;
        assert!(r.is_err(), "B 沒有被冷卻：有去探測（連不上 pane 而失敗），不是 Ok(None)：{r:?}");
    }

    /// 舊世代的登入 watcher 不能清掉新主機的冷卻。
    #[tokio::test]
    async fn an_old_watcher_does_not_clear_the_new_hosts_backoff() {
        let e = tt::env().await;
        let app = e.app.clone();
        let host = fresh_host();
        add_host(&app, &host).await;
        let a = app.hosts.fence(&host).await.unwrap();
        add_host(&app, &host).await;
        let b = app.hosts.fence(&host).await.unwrap();

        assert!(note_failure(&app, &host, &b).await);
        assert!(backoff_active(&key_of(&host), &b));
        clear_backoff(&app, &host, &a).await;
        assert!(backoff_active(&key_of(&host), &b), "A 的清除不動 B 的冷卻");
        clear_backoff(&app, &host, &b).await;
        assert!(!backoff_active(&key_of(&host), &b), "同世代的清除照常生效");
    }

    /// 換代造成的失敗（`superseded`）不開始冷卻；其他失敗照常。
    #[tokio::test]
    async fn a_superseded_probe_does_not_start_a_backoff() {
        let superseded = RefreshFail { error: anyhow::anyhow!("host changed"), superseded: true };
        let other = RefreshFail::from(anyhow::anyhow!("pane failed"));
        assert!(!starts_backoff(&superseded));
        assert!(starts_backoff(&other));
    }

    /// 同一世代寫入的冷卻照常生效（沒把正常的 backoff 弄壞），並且 `refresh_agy_if_due` 跳過探測。
    #[tokio::test]
    async fn backoff_from_the_same_generation_still_applies() {
        let e = tt::env().await;
        let app = e.app.clone();
        let host = fresh_host();
        add_host(&app, &host).await;
        install_agy(&app, &host).await;
        let fence = app.hosts.fence(&host).await.unwrap();
        assert!(!backoff_active(&key_of(&host), &fence));
        assert!(note_failure(&app, &host, &fence).await);
        assert!(backoff_active(&key_of(&host), &fence));
        // 同世代的新 fence（ticket 不同）也認得。
        let again = app.hosts.fence(&host).await.unwrap();
        assert!(backoff_active(&key_of(&host), &again));
        assert!(matches!(refresh_agy_if_due(&app, &host).await, Ok(None)), "冷卻中：這一輪跳過");
    }

    /// reconnect（同一個連線物件、世代 +1）也算換代：舊世代的冷卻不壓新世代。
    #[tokio::test]
    async fn a_reconnect_generation_bump_drops_the_old_backoff() {
        let e = tt::env().await;
        let app = e.app.clone();
        let host = fresh_host();
        let conn = add_host(&app, &host).await;
        let old = app.hosts.fence(&host).await.unwrap();
        assert!(note_failure(&app, &host, &old).await);
        conn.bump_generation_for_test();
        let new = app.hosts.fence(&host).await.unwrap();
        assert!(!backoff_active(&key_of(&host), &new), "舊世代的冷卻不屬於重連後的新世代");
        assert!(!note_failure(&app, &host, &old).await, "舊 fence 寫不進去");
    }
