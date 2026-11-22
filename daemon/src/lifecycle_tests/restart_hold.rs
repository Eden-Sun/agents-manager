#![allow(unused_imports)]

use crate::lifecycle_tests::prelude::*;
use crate::db;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::state::App;
use crate::lifecycle::*;
use crate::lifecycle::start::{restart_bot_with, StartOpts};
use crate::lifecycle::restart_hold::*;
use crate::lifecycle::restart_hold::test_seams::*;

mod tests {
    use super::*;
    use crate::lifecycle::*;
    use crate::lifecycle::restart_hold::{begin, in_progress};
    use crate::testing as tt;

    async fn queued_turn(app: &Arc<App>, bot_id: &str) -> String {
        let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
        let id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&id)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    async fn status(app: &Arc<App>, turn_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn_id).fetch_one(&app.db).await.unwrap()
    }

    /// 不拿 bot 鎖的兩條路（定時 sweeper、pane-exit 事件的 `mark_run_exited`）在重啟那一段都不撤；
    /// 重啟結束（憑證 drop）之後，沒有 run 的照舊是孤兒、照舊撤。
    #[tokio::test]
    async fn nothing_revokes_a_queued_prompt_while_its_bot_is_restarting() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "restarting").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let queued = queued_turn(&app, &bot.id).await;

        let hold = begin(&bot.id);
        assert!(in_progress(&bot.id));
        mark_run_exited(&app, &run, "pane exited").await;
        assert_eq!(status(&app, &queued).await, "queued", "pane-exit 事件在重啟途中到：不撤");
        assert!(revoke_all_orphaned_queued_turns(&app).await.is_empty(), "定時 sweeper 也不撤");
        assert_eq!(status(&app, &queued).await, "queued");

        drop(hold);
        assert!(!in_progress(&bot.id));
        assert_eq!(revoke_all_orphaned_queued_turns(&app).await, vec![queued.clone()], "重啟結束還是沒有 run：真的孤兒，撤");
        assert_eq!(status(&app, &queued).await, "failed");
    }

    /// 重啟沒能把 bot 開回來：這時候排著的才是真的孤兒，撤掉並說明原因（issue #106 驗收第二條）。
    #[tokio::test]
    async fn a_restart_that_cannot_bring_the_bot_back_revokes_what_was_queued() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "left-down").await;
        start_bot(&app, &bot.id).await.unwrap();
        let queued = queued_turn(&app, &bot.id).await;
        sqlx::query("UPDATE bots SET identity='ghost' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        assert!(restart_bot_with(&app, &bot.id, StartOpts::default()).await.is_err());
        assert!(db::active_run(&app.db, &bot.id).await.unwrap().is_none(), "bot 沒回來");
        assert!(!in_progress(&bot.id), "重啟結束了");
        assert_eq!(status(&app, &queued).await, "failed");
        let why: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&queued)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(why.contains("重啟之後沒能把 bot 開回來"), "{why}");
    }

    /// 一般的 stop（不是重啟）照舊撤：使用者要它停，就沒有人會送。
    #[tokio::test]
    async fn a_plain_stop_still_revokes_what_was_queued() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "stopped").await;
        start_bot(&app, &bot.id).await.unwrap();
        let queued = queued_turn(&app, &bot.id).await;
        stop_bot(&app, &bot.id).await.unwrap();
        assert_eq!(status(&app, &queued).await, "failed");
    }

    /// 巢狀的憑證：先結束的那一個不會把還在進行的清掉。
    #[test]
    fn a_hold_ends_only_when_the_last_one_is_dropped() {
        let id = format!("b-{}", crate::db::ulid());
        let a = begin(&id);
        let b = begin(&id);
        drop(a);
        assert!(in_progress(&id));
        drop(b);
        assert!(!in_progress(&id));
    }
}
