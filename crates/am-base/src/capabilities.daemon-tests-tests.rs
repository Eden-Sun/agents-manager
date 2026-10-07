
    use super::*;

    async fn uses_only_narrow_capabilities(app: &(impl Db + DataDir + Emit)) -> (bool, std::path::PathBuf) {
        app.emit("capability_probe", serde_json::json!({"ok": true})).await;
        let ping: i64 = sqlx::query_scalar("SELECT 1").fetch_one(app.db()).await.unwrap();
        (ping == 1, app.data_dir().to_path_buf())
    }

    /// `Arc<App>` 與 `App` 都能當窄能力傳進去，而且指到同一份資源（既有呼叫端不用改）。
    #[tokio::test]
    async fn app_and_arc_app_satisfy_the_same_narrow_capabilities() {
        let e = crate::testing::env().await;
        let mut rx = e.app.subscribe();
        let (ok, dir) = uses_only_narrow_capabilities(&e.app).await;
        assert!(ok);
        assert_eq!(dir, e.app.data_dir);
        let (ok2, _) = uses_only_narrow_capabilities(&*e.app).await;
        assert!(ok2);
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, "capability_probe");
    }
