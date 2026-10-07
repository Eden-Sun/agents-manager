
    use super::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn the_revision_changes_with_every_launch_relevant_field_and_ignores_the_rest() {
        let e = tt::env().await;
        let base = tt::claude_bot(&e.app, &e.project_id, "rev").await;
        let rev = of(&base);
        assert_eq!(rev, of(&base.clone()), "同樣的設定同一個版本");
        let mut b = base.clone();
        b.persona = Some("x".into());
        assert_ne!(of(&b), rev);
        let mut b = base.clone();
        b.env_json = r#"{"A":"1"}"#.into();
        assert_ne!(of(&b), rev);
        let mut b = base.clone();
        b.args_json = r#"["--x"]"#.into();
        assert_ne!(of(&b), rev);
        let mut b = base.clone();
        b.name = "renamed".into();
        b.autostart = 1;
        b.is_primary = 1;
        assert_eq!(of(&b), rev, "名字、autostart、釘選不需要重啟");
    }

    /// 升級 daemon 不能讓所有現役 run 變成過期：`instruction_files` 拿掉後，沒設過它的 bot 版本必須跟移除前（第五格 null）逐字相同。
    #[tokio::test]
    async fn dropping_instruction_files_keeps_the_revision_of_every_bot_that_never_set_it() {
        let e = tt::env().await;
        let b = tt::claude_bot(&e.app, &e.project_id, "rev").await;
        let before = json!([b.model, b.effort, b.fast, b.persona, null, b.args_json, b.identity, b.env(), b.inject_hooks, b.auto_approve]);
        assert_eq!(of(&b), format!("{:016x}", fnv1a64(&before.to_string())));
    }

    #[tokio::test]
    async fn stamp_reports_a_missing_run_instead_of_claiming_success() {
        let e = tt::env().await;
        let result = stamp(&e.app.db, "missing-run-for-launch-stamp", "0123456789abcdef").await;
        assert!(matches!(result, Err(sqlx::Error::RowNotFound)), "missing target rows must not be reported as a successful stamp: {result:?}");
    }
