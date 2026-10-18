//! daemon 自己產生、排進佇列的通知（#562）：子 agent 停在 blocked（`child_alerts`）、重啟後的續行提示（`resume_nudge`）。
//!
//! 它們不是使用者送的、也不是 AGM 派工，寫入時由 daemon 指定獨立 lane（#1685：使用者、relay、通知各自最多一筆 queued）。一則通知送不進去時，
//! 照一般 prompt 的重試上限（12 次）要擋約 40 分鐘，後面的使用者訊息一直排不到。所以：
//! - 放回佇列的上限短得多（[`RETRY_LIMIT`]），用完就收成 failed＋一則 system 說明，讓佇列往下走；
//! - `POST /api/turns/{id}/withdraw` 可以撤回還在排的這一種（使用者訊息、AGM 派工照舊 409）。

/// daemon 通知最多放回佇列幾次：退避 15＋30＋60 秒，大約兩分鐘就讓路（一般 prompt 是 12 次、約 40 分鐘）。
pub const RETRY_LIMIT: i64 = 3;

/// 上限用完、收成 failed 時寫進對話的說明。
pub fn gave_up_hint(reason: &str) -> String {
    format!(
        "daemon 自動通知沒有送出：試了 {RETRY_LIMIT} 次都沒辦法打字（最後一次是 {reason}），已放棄，讓後面排著的訊息先送。通知的內容仍在上面，需要的話自己轉告 agent。"
    )
}

/// 使用者撤回（`POST /api/turns/{id}/withdraw`）時寫進對話的說明。
pub const WITHDRAWN_WHY: &str = "使用者撤回了這則 daemon 自動通知：沒有送出，不會再送。";

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;
    use crate::db;
    use crate::lc_error::LcError;
    use crate::lifecycle::{flush_queued_locked, withdraw_turn, QueueClass};
    use crate::state::App;
    use crate::testing as tt;
    use std::sync::Arc;

    /// 一顆閒著的 grok，框裡一直有使用者的草稿（打不進去＝`composer_busy`），佇列頭排著一則 `crid` 的 turn。
    async fn stuck_head(crid: Option<&str>, queue_class: QueueClass) -> (tt::Env, String, String, String) {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "notice").await;
        sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run)
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        db::set_pane_typed(&app.db, &run).await.unwrap();
        env.herdr.live_pane("pane-1", tt::LivePane { width: Some(120), boxed: true, composer: vec!["草稿".into()], ..Default::default() });
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn = queue_turn(&app, &conv, crid, queue_class).await.unwrap();
        (env, bot.id, conv, turn)
    }

    async fn queue_turn(app: &Arc<App>, conv: &str, crid: Option<&str>, queue_class: QueueClass) -> Result<String, sqlx::Error> {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, client_request_id, queue_class, created_at)
             VALUES (?,?,'web','queued','pending','通知',?,?,?)",
        )
        .bind(&id)
        .bind(conv)
        .bind(crid)
        .bind(queue_class.as_str())
        .bind(db::now())
        .execute(&app.db)
        .await?;
        Ok(id)
    }

    async fn status(app: &Arc<App>, turn: &str) -> (String, String) {
        sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap()
    }

    async fn system_notes(app: &Arc<App>, turn: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(turn).fetch_all(&app.db).await.unwrap()
    }

    /// 把 `n` 次放回走完（每次都把退避清掉，不等真實時間）。
    async fn flush_times(app: &Arc<App>, bot: &str, turn: &str, n: i64) {
        for _ in 0..n {
            flush_queued_locked(app, bot).await.unwrap();
            sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(turn).execute(&app.db).await.unwrap();
        }
    }

    /// #562 的毒丸：通知一直打不進去。用完短上限就收成 failed＋說明，佇列空出來給後面的訊息；
    /// 同樣卡住的使用者訊息照舊用一般的上限，不會被這條提早放棄。
    #[tokio::test]
    async fn a_notice_that_cannot_be_typed_gives_way_after_its_short_limit() {
        let (env, bot, conv, turn) = stuck_head(Some("child-blocked:01CHILD:1:ff"), QueueClass::DaemonNotice).await;
        let app = env.app.clone();
        let mut events = app.subscribe();
        flush_times(&app, &bot, &turn, RETRY_LIMIT).await;
        assert_eq!(status(&app, &turn).await.0, "queued", "上限之內照常放回");
        assert!(std::iter::from_fn(|| events.try_recv().ok()).all(|event| {
            event.kind != "message_added" || event.data["message"]["turn_id"] != turn
        }), "daemon 通知上限之前不會推失敗說明");
        flush_times(&app, &bot, &turn, 1).await;
        assert_eq!(status(&app, &turn).await, ("failed".into(), "failed".into()));
        let notes = system_notes(&app, &turn).await;
        assert!(notes.len() == 1 && notes[0].contains("daemon 自動通知") && notes[0].contains("composer_busy"), "{notes:?}");
        let message: db::Message = sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE turn_id=? AND role='system'")
            .bind(&turn)
            .fetch_one(&app.db)
            .await
            .unwrap();
        let message_events: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| event.kind == "message_added" && event.data["message"]["turn_id"] == turn)
            .collect();
        assert_eq!(message_events.len(), 1, "daemon 重試用盡時只推一次說明");
        assert_eq!(message_events[0].data["bot_id"], bot);
        assert_eq!(message_events[0].data["message"], serde_json::to_value(message).unwrap(), "event carries the committed DB message");
        assert!(
            queue_turn(&app, &conv, Some("web-behind"), QueueClass::User).await.is_ok(),
            "讓路：後面的使用者訊息排得進自己的 lane",
        );
        assert_eq!(env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0, "從頭到尾沒打過字");

        let (env, bot, _, turn) = stuck_head(Some("web-1234"), QueueClass::User).await;
        flush_times(&env.app, &bot, &turn, RETRY_LIMIT + 1).await;
        assert_eq!(status(&env.app, &turn).await.0, "queued", "使用者的訊息不吃這個短上限");
    }

    /// `POST /api/turns/{id}/withdraw`：還在排的 daemon 通知撤得掉（failed＋說明）；使用者訊息、已經在送的通知照舊 409、原樣不動。
    #[tokio::test]
    async fn withdraw_takes_a_queued_notice_but_not_a_users_message() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "withdraw-notice").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();

        let user = queue_turn(&app, &conv, Some("web-1234"), QueueClass::User).await.unwrap();
        assert!(matches!(withdraw_turn(&app, &user).await, Err(LcError::Conflict(_))), "使用者排的不歸這裡撤");
        assert_eq!(status(&app, &user).await.0, "queued", "原樣不動");
        sqlx::query("DELETE FROM turns WHERE id=?").bind(&user).execute(&app.db).await.unwrap();

        let notice = queue_turn(&app, &conv, Some("child-blocked:01CHILD:1:ff"), QueueClass::DaemonNotice).await.unwrap();
        withdraw_turn(&app, &notice).await.unwrap();
        assert_eq!(status(&app, &notice).await, ("failed".into(), "failed".into()));
        assert_eq!(system_notes(&app, &notice).await, vec![super::WITHDRAWN_WHY.to_string()]);
        assert!(matches!(withdraw_turn(&app, &notice).await, Err(LcError::Conflict(_))), "撤過的不能再撤");

        let sending = queue_turn(&app, &conv, Some("resume-nudge:01RUN"), QueueClass::DaemonNotice).await.unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE turns SET status='in_flight', run_id=? WHERE id=?").bind(&run).bind(&sending).execute(&app.db).await.unwrap();
        assert!(matches!(withdraw_turn(&app, &sending).await, Err(LcError::Conflict(_))), "已經在送的撤不回來");
        assert_eq!(status(&app, &sending).await.0, "in_flight");
    }

}
