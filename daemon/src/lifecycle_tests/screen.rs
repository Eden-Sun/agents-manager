#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod limit_hit_quota_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::screen::*;
    use crate::testing as tt;
    const NOTICE: &str = "ERROR: You've hit your usage limit, or try again at 10:15 PM.";
    async fn apply(app: &Arc<App>) {
        apply_codex_limit_hit_quota(app, LOCAL_HOST, "codex", codex_limit_hit(NOTICE, crate::db::now())).await;
    }
    /// review3 c2 M1：`until` 到了、交辦重送、CLI 回同一句橫幅——這是真的又被擋一次，不是重掃舊字。
    /// 以前同文就直接略過，記憶體裡只剩那張過期的，`on_turn_done` 查不到撞限，交辦被結成 failed 送去驗收。
    #[tokio::test]
    async fn the_same_banner_after_the_old_one_expired_is_a_fresh_hit() {
        let env = tt::env().await;
        let app = env.app.clone();
        apply(&app).await;
        // 改成已經過期，模擬「等到重置時間、重送一次，又撞到同一句」。
        {
            let mut q = app.quotas.lock().await;
            let hit = q.get_mut("codex").unwrap().limit_hit.as_mut().unwrap();
            hit.until = Some("2020-01-01T00:00:00Z".into());
            hit.at = "2020-01-01T00:00:00Z".into();
        }

        apply(&app).await;

        let hit = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().expect("又被擋一次要重新記上");
        assert!(!crate::quota::limit_hit_expired(Some(&hit)), "新的那張不該是過期的：{hit:?}");
        assert_ne!(hit.at, "2020-01-01T00:00:00Z", "時間戳要更新成這一次");
    }
    const FAR_LIMIT: &str = "■ You've hit your usage limit. Upgrade to Pro, or try again at Sep 19th, 2099 6:43 PM.";
    /// 一顆在跑的本機 codex bot，畫面上是 `screen`。
    async fn codex_run(env: &tt::Env, identity: Option<&str>, screen: &str) -> (crate::db::Bot, String) {
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "cx").await;
        sqlx::query("UPDATE bots SET kind='codex', identity=? WHERE id=?").bind(identity).bind(&bot.id).execute(app.db()).await.unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        env.herdr.set_screen(&format!("pane-{}", bot.id), screen);
        (crate::db::bot(app.db(), &bot.id).await.unwrap().unwrap(), run)
    }
    fn banner_screen() -> String {
        format!("› Reply with PONG\n\n{FAR_LIMIT}\n\n› \n")
    }
    async fn limit_hits(app: &Arc<App>) -> Vec<String> {
        app.quotas.lock().await.iter().filter(|(_, q)| q.limit_hit.is_some()).map(|(k, _)| k.clone()).collect()
    }
    /// #198：畫面上的撞限橫幅記不進正確那一格（身份表還沒偵測完，算不出 `cx0` 是不是預設帳號）就欠著，不猜一格
    /// `codex:cx0`；回合照樣收成 failed，派送前照欠著的那一筆擋。
    #[tokio::test]
    async fn a_limit_notice_whose_key_is_unknown_is_owed_not_guessed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = codex_run(&env, Some("cx0"), &banner_screen()).await;
        let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;

        assert!(capture_codex_usage_notices(&app, &bot.id, &run).await.is_err(), "記不進去要回錯");
        assert_eq!(limit_hits(&app).await, Vec::<String>::new(), "沒有猜一格寫下去");
        assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "failed", "回合照樣收");
        assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "欠著照擋");
    }
    /// #198 的留言：通知訊息先寫，之後這一則就不是新的。以前寫完訊息才讀「有沒有回合在飛」，那一下讀錯就回錯——沒有在飛
    /// 回合的話，下一次被當成看過的舊橫幅跳過，撞限永遠不記。現在先讀再寫；訊息寫下之後的失敗（這裡是蓋憑據）由欠帳接手。
    #[tokio::test]
    async fn a_limit_notice_is_never_lost_to_a_read_error_after_it_was_written() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = codex_run(&env, None, &banner_screen()).await;
        // 這個 run 已經讀過一次畫面（那時沒有橫幅），這張是新的。
        crate::lifecycle::limit_banner::sighting(&run, "", FAR_LIMIT, &[]);
        let a = app.clone();
        crate::lifecycle::race_point::arm("codex_notice_after_insert", &bot.id, move || async move {
            sqlx::query("ALTER TABLE turns RENAME TO turns_unreadable").execute(a.db()).await.unwrap();
        });
        let first = capture_codex_usage_notices(&app, &bot.id, &run).await;
        sqlx::query("ALTER TABLE turns_unreadable RENAME TO turns").execute(app.db()).await.unwrap();
        assert!(first.is_err(), "後面寫不進去要回錯");
        capture_codex_usage_notices(&app, &bot.id, &run).await.unwrap();

        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='system' AND content LIKE '%hit your usage limit%'").fetch_one(app.db()).await.unwrap();
        assert_eq!(notes, 1, "通知只寫一次");
        assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "撞限記下了（或欠著照擋），沒有因為讀錯就丟掉");
        assert_eq!(limit_hits(&app).await, vec!["codex".to_string()]);
    }
    /// 還沒過期的同一張橫幅照舊不算新證據（2026-09-13：22:21 掃到 22:15 的舊橫幅，把 `at` 蓋成現在）。
    #[tokio::test]
    async fn an_unexpired_banner_seen_again_is_still_not_new_evidence() {
        let env = tt::env().await;
        let app = env.app.clone();
        apply(&app).await;
        let first = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().unwrap();

        apply(&app).await;

        let again = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().unwrap();
        assert_eq!((first.at, first.until), (again.at, again.until));
    }
}
