#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::busy_send::test_seams::*;
    use crate::lifecycle::*;
    use crate::capabilities::Db;
    use crate::lifecycle::{prompt, *};
    use crate::lifecycle::send_now::ports::{AttachTxPort, SupervisorSendRepo};
    use crate::lifecycle::s6_ports::BusySendServices;
    use crate::attach::Attachment;
    use crate::lifecycle::busy_send::*;
    use crate::testing as tt;
    use std::sync::Mutex as StdMutex;
    async fn admit(app: &Arc<App>, conv: &str, bot_id: &str, text: &str, crid: &str) -> LcResult<prompt::PromptOut> {
        queue_awaiting_idle(app, conv, bot_id, text, text, crid, prompt::RelaySrc::trusted(None), &[]).await
    }
    async fn queued(app: &Arc<App>, conv: &str) -> (i64, i64) {
        sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status='queued' AND awaits_idle=1),
                    (SELECT COUNT(*) FROM messages m JOIN turns t ON t.id = m.turn_id WHERE t.conversation_id=? AND t.awaits_idle=1 AND m.role='user')",
        )
        .bind(conv)
        .bind(conv)
        .fetch_one(app.db())
        .await
        .unwrap()
    }
    /// #821：讀完「槽是空的」、還沒 INSERT 的那一瞬，一個不相干的 writer（對帳、收件匣）commit 了一筆。deferred 交易這時升級
    /// 寫鎖直接 517 BUSY_SNAPSHOT，空著的槽也把 prompt 拒掉；寫鎖要從讀之前就拿著，插進來的那一筆等，prompt 照收、只收一次。
    #[tokio::test]
    async fn an_unrelated_writer_between_the_slot_read_and_the_insert_does_not_refuse_the_prompt() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "busy-831").await;
        let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
        let other = tt::arm_app_foreign_writer(&e.app, "busy_queue_after_slot_read", &conv);

        let out = admit(&e.app, &conv, &bot.id, "稍後送出", "busy-831-1").await.expect("a free slot must not be refused because another writer committed");
        assert_eq!(*other.lock().unwrap(), Some(false), "the admission holds the write lock from its read on; the other writer waits");
        assert_eq!(out.delivery, "queued");
        assert_eq!(queued(&e.app, &conv).await, (1, 1), "the queued turn and its user message commit exactly once");
    }
    /// daemon 自己排的通知（lane 由受信任的 caller 指定，來源標記只供 UI 顯示）。
    async fn admit_notice(app: &Arc<App>, conv: &str, bot_id: &str, text: &str, crid: &str) -> LcResult<prompt::PromptOut> {
        queue_awaiting_idle(app, conv, bot_id, text, text, crid, prompt::RelaySrc::daemon_notice(Some("daemon")), &[]).await
    }
    async fn queued_rows(app: &Arc<App>, conv: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status='queued'")
            .bind(conv)
            .fetch_one(app.db())
            .await
            .unwrap()
    }
    /// #1685：排著一則 daemon 通知（`child-done:`），使用者自己打的那一句照樣排得進去，不被通知擋在門外；
    /// 使用者那一句排在通知前面送。使用者的槽照舊只收一筆（第二句回 `queue_slot_taken`，`holder` 是使用者自己）。
    #[tokio::test]
    async fn a_user_prompt_queues_behind_nothing_when_a_daemon_notice_holds_the_queue() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "busy-1685-notice-first").await;
        let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
        let notice = admit_notice(&e.app, &conv, &bot.id, "子 agent 完成了", "child-done:c1:t1").await.expect("the notice takes the daemon slot");

        let user = admit(&e.app, &conv, &bot.id, "使用者的一句", "busy-1685-user")
            .await
            .expect("a user prompt must not be refused because a daemon notice holds the queue");
        assert_eq!(queued_rows(&e.app, &conv).await, 2, "the notice and the user's line both stay queued");
        let head = db::queued_turn(e.app.db(), &conv).await.unwrap().expect("something is queued");
        assert_eq!(head.id, user.turn_id, "the user's line goes first, the notice follows");

        match admit(&e.app, &conv, &bot.id, "再一句", "busy-1685-user-2").await {
            Err(LcError::Conflict(body)) => {
                assert_eq!(body["reason"], "queue_slot_taken", "{body}");
                assert_eq!(body["turn_id"], user.turn_id.as_str(), "{body}");
                assert_eq!(body["holder"]["kind"], "user", "{body}");
            }
            other => panic!("a second user prompt keeps the one user slot, got {:?}", other.map(|o| o.turn_id)),
        }
        assert_eq!(queued_rows(&e.app, &conv).await, 2);
        assert_eq!(notice.delivery, "queued");
    }
    /// #1685：使用者的一句已經排著，子 agent 的 `child-blocked:` 通知照樣排得進去；通知之間仍只收一則（daemon 的槽照舊一筆）。
    #[tokio::test]
    async fn a_daemon_notice_queues_behind_a_user_prompt_and_the_daemon_slot_still_takes_one() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "busy-1685-user-first").await;
        let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
        let user = admit(&e.app, &conv, &bot.id, "先排的使用者", "busy-1685-u").await.expect("the user prompt takes the user slot");

        let alert = admit_notice(&e.app, &conv, &bot.id, "子 agent 卡住了", "child-blocked:c2:q1")
            .await
            .expect("a daemon notice must not be refused because a user prompt holds the queue");
        assert_eq!(queued_rows(&e.app, &conv).await, 2);
        assert_eq!(db::queued_turn(e.app.db(), &conv).await.unwrap().expect("queued").id, user.turn_id);

        match admit_notice(&e.app, &conv, &bot.id, "又一則通知", "resume-nudge:01RUN").await {
            Err(LcError::Conflict(body)) => {
                assert_eq!(body["reason"], "queue_slot_taken", "{body}");
                assert_eq!(body["turn_id"], alert.turn_id.as_str(), "{body}");
                assert_eq!(body["holder"]["kind"], "daemon", "{body}");
            }
            other => panic!("the daemon slot keeps one notice, got {:?}", other.map(|o| o.turn_id)),
        }
    }
    /// #1685（驗收 2、3）：使用者的一句被 flush 認領（`in_flight`）之後，原本排著的 daemon 通知還在佇列裡、是下一則；
    /// 投影給 web 的 `queued_turn`（`queued_turns_by_bot`）在使用者那一句還排著的時候，就是使用者自己的那一句。
    #[tokio::test]
    async fn a_queued_daemon_notice_survives_the_user_line_being_claimed_and_is_next_in_line() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "busy-1685-survive").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let notice = admit_notice(&app, &conv, &bot.id, "子 agent 完成了", "child-done:c3:t3").await.unwrap();
        let user = admit(&app, &conv, &bot.id, "使用者的一句", "busy-1685-survive-u").await.unwrap();

        let projected = db::queued_turns_by_bot(&app.db).await.unwrap();
        assert_eq!(projected.get(&bot.id).map(|t| t.id.as_str()), Some(user.turn_id.as_str()), "web 的 queued_turn 是使用者自己那一句");

        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE turns SET status='in_flight', run_id=? WHERE id=?").bind(&run).bind(&user.turn_id).execute(&app.db).await.unwrap();
        let head = db::queued_turn(app.db(), &conv).await.unwrap().expect("the notice is still queued");
        assert_eq!(head.id, notice.turn_id, "the notice is next once the user line is claimed");
        assert_eq!(queued_rows(&app, &conv).await, 1, "exactly the notice remains queued");
    }
    /// #1685（驗收 4，撤回）：撤回排著的 daemon 通知只撤它一則，使用者那一句照舊排著、仍是佇列頭。
    #[tokio::test]
    async fn withdrawing_the_notice_leaves_the_user_line_queued_at_the_head() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "busy-1685-withdraw").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let notice = admit_notice(&app, &conv, &bot.id, "子 agent 完成了", "child-done:c4:t4").await.unwrap();
        let user = admit(&app, &conv, &bot.id, "使用者的一句", "busy-1685-withdraw-u").await.unwrap();

        crate::lifecycle::withdraw_turn(&app, &notice.turn_id).await.expect("a queued daemon notice can be withdrawn");
        assert_eq!(queued_rows(&app, &conv).await, 1, "only the withdrawn notice leaves the queue");
        let head = db::queued_turn(app.db(), &conv).await.unwrap().expect("the user line is still queued");
        assert_eq!(head.id, user.turn_id);
    }
    /// #1685（驗收 4，送達臨界區）：兩條槽各排一則時，`DELIVERY_CRITICAL_PREDICATE` 照舊判成臨界區，`acquire_lease`（quiet）照舊拿不到。
    #[tokio::test]
    async fn the_delivery_critical_predicate_and_quiet_lease_hold_with_two_queued_rows() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "busy-1685-critical").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let notice = admit_notice(&app, &conv, &bot.id, "子 agent 完成了", "child-done:c5:t5").await.unwrap();
        let user = admit(&app, &conv, &bot.id, "使用者的一句", "busy-1685-critical-u").await.unwrap();
        tt::fake_run(&app, &bot.id).await;

        assert!(crate::supervisor::store::delivery_critical_anywhere(&app.db).await.unwrap(), "a queued row on a live idle run is delivery-critical");
        let lease = crate::supervisor::store::acquire_lease(&app.db, "restart", "runner", None, None, &db::iso_in(600), true, None, &json!({}))
            .await
            .unwrap();
        assert!(lease.is_none(), "the quiet lease is refused while either lane has a queued row");

        sqlx::query("UPDATE turns SET status='failed', delivery='failed', completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&notice.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        assert!(crate::supervisor::store::delivery_critical_anywhere(&app.db).await.unwrap(), "the user row remains delivery-critical after the notice is withdrawn");
        assert!(
            crate::supervisor::store::acquire_lease(&app.db, "restart", "runner", None, None, &db::iso_in(600), true, None, &json!({}))
                .await
                .unwrap()
                .is_none(),
            "the user lane alone still refuses the quiet lease",
        );

        sqlx::query("UPDATE turns SET status='failed', delivery='failed', completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&user.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        assert!(!crate::supervisor::store::delivery_critical_anywhere(&app.db).await.unwrap(), "no queued lane remains");
        assert!(
            crate::supervisor::store::acquire_lease(&app.db, "restart", "runner", None, None, &db::iso_in(600), true, None, &json!({}))
                .await
                .unwrap()
                .is_some(),
            "the quiet lease becomes available after both rows leave the queue",
        );
    }
    /// #821：真的有第二個排隊請求在同一瞬搶同一個槽時，它照舊拿到 `queue_slot_taken`（不是 `database is locked`），
    /// 槽裡只有先到的那一筆。不拿 bot 鎖，兩個 admission 直接在 DB 上碰。
    #[tokio::test]
    async fn a_second_request_racing_for_the_slot_still_gets_queue_slot_taken() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "busy-831-race").await;
        let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
        let second = Arc::new(StdMutex::new(None));
        let (app, c, b, slot) = (e.app.clone(), conv.clone(), bot.id.clone(), second.clone());
        race_point::arm("busy_queue_after_slot_read", &conv, move || async move {
            let h = tokio::spawn(async move { admit(&app, &c, &b, "再一則", "busy-831-b").await });
            *slot.lock().unwrap() = Some(h);
        });

        let first = admit(&e.app, &conv, &bot.id, "先到的", "busy-831-a").await.expect("the first admission takes the slot");
        let h = second.lock().unwrap().take().expect("the race point ran");
        match h.await.unwrap() {
            Err(LcError::Conflict(body)) => {
                assert_eq!(body["reason"], "queue_slot_taken", "{body}");
                assert_eq!(body["turn_id"], first.turn_id.as_str(), "{body}");
            }
            other => panic!("the racing request must be refused as queue_slot_taken, got {:?}", other.map(|o| o.turn_id)),
        }
        assert_eq!(queued(&e.app, &conv).await, (1, 1));
    }
}
