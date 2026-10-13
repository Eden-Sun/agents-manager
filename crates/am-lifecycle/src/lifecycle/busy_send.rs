//! Persist user prompts accepted while a running agent is busy (#733).

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::capabilities::Db;
use super::{prompt, *};
use super::send_now::ports::{AttachTxPort, SupervisorSendRepo};
use super::s6_ports::BusySendServices;
use crate::attach::Attachment;

/// 409 `queue_slot_taken`：唯一的 queued 槽被 `turn_id` 那一筆佔著，`holder` 說是誰（web 才講得出人話）。
async fn slot_taken(app: &(impl crate::capabilities::Db + SupervisorSendRepo), turn_id: &str) -> LcError {
    LcError::conflict("queue_slot_taken", json!({"turn_id": turn_id, "holder": slot_holder(app, turn_id).await}))
}

/// 佔槽的是誰：`{"kind": "user"｜"start"｜"daemon"｜"agm"｜"bot"｜"unknown", "bot_id"?, "bot_name"?}`。
/// `user`＝使用者自己排的；`start`＝在等 bot 起來的那一則（#122）；`daemon`＝daemon 的通知；
/// `agm`／`bot`＝別人派來的（`relay_from` 是 bot id；是 AGM 的 bot 算 `agm`；**自稱**的 `relay_from`〔沒帶 bot token，`relay_unverified`〕只說 `bot`，不帶名字與 id）。讀不到就是 `unknown`，不猜。
async fn slot_holder(app: &(impl crate::capabilities::Db + SupervisorSendRepo), turn_id: &str) -> Value {
    let row: Option<(String, i64, i64, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT t.queue_class, t.awaits_idle, t.awaits_start,
                (SELECT m.relay_from FROM messages m WHERE m.turn_id = t.id AND m.role = 'user' ORDER BY m.created_at, m.rowid LIMIT 1),
                (SELECT m.relay_unverified FROM messages m WHERE m.turn_id = t.id AND m.role = 'user' ORDER BY m.created_at, m.rowid LIMIT 1)
           FROM turns t WHERE t.id = ?",
    )
    .bind(turn_id)
    .fetch_optional(app.db())
    .await
    .ok()
    .flatten();
    let Some((queue_class, awaits_idle, awaits_start, relay_from, relay_unverified)) = row else { return json!({"kind": "unknown"}) };
    if awaits_start == 1 {
        return json!({"kind": "start"});
    }
    if queue_class == prompt::QueueClass::User.as_str() && awaits_idle == 1 {
        return json!({"kind": "user"});
    }
    if queue_class == prompt::QueueClass::DaemonNotice.as_str() {
        return json!({"kind": "daemon"});
    }
    if let Some(from) = relay_from {
        // 沒帶 bot token 的 `relay_from` 只是自稱（`relay_unverified`）：不當事實講成 AGM、也不替呼叫端把任意 id 解析成名字。
        if relay_unverified.unwrap_or(1) != 0 {
            return json!({"kind": "bot"});
        }
        if let Ok(Some(bot)) = db::bot(app.db(), &from).await {
            let agm = match app.load_owned().await {
                Ok(owned) => owned.owns(&bot),
                Err(_) => false,
            };
            return json!({"kind": if agm { "agm" } else { "bot" }, "bot_id": bot.id, "bot_name": bot.name});
        }
        return json!({"kind": "bot", "bot_id": from});
    }
    json!({"kind": "unknown"})
}

/// Enforce the single `awaits_idle` slot before taking the direct-send path too. A queued send
/// can still be waiting for the queue worker after its predecessor has ended.
///
/// User, relay, and daemon notices each keep their own single queued slot (#1685).
pub(super) async fn refuse_if_slot_taken(app: &(impl crate::capabilities::Db + SupervisorSendRepo), conversation_id: &str, queue_class: prompt::QueueClass) -> LcResult<()> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM turns WHERE conversation_id=? AND status='queued' AND awaits_idle=1 AND queue_class=? ORDER BY created_at, id LIMIT 1",
    )
    .bind(conversation_id)
    .bind(queue_class.as_str())
    .fetch_optional(app.db())
    .await
    .map_err(up)?;
    match existing {
        Some(turn_id) => Err(slot_taken(app, &turn_id).await),
        None => Ok(()),
    }
}

/// Insert the turn, user message, and attachment bindings as one durable queue admission.
/// The caller holds the per-bot lock and has already resolved/validated the attachments.
#[allow(clippy::too_many_arguments)]
pub(super) async fn queue_awaiting_idle(
    app: &(impl crate::capabilities::Db + BusySendServices + SupervisorSendRepo),
    conversation_id: &str,
    bot_id: &str,
    text: &str,
    deliver: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    relay: prompt::RelaySrc<'_>,
    files: &[Attachment],
) -> LcResult<prompt::PromptOut> {
    // 先讀槽再寫：bot 鎖只排得住這顆 bot 自己，擋不住別的 writer（對帳、收件匣）在讀與寫之間 commit；deferred 交易
    // 這時升級寫鎖直接 517，空著的槽也把使用者的 prompt 拒掉（#821）。
    let mut tx = db::begin_write(app.db()).await.map_err(up)?;
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM turns WHERE conversation_id=? AND status='queued' AND awaits_idle=1 AND queue_class=? ORDER BY created_at, id LIMIT 1",
    )
    .bind(conversation_id)
    .bind(relay.queue_class.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(up)?;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("busy_queue_after_slot_read", conversation_id).await;
    if let Some(turn_id) = existing {
        drop(tx);
        return Err(slot_taken(app, &turn_id).await);
    }

    let turn_id = db::ulid();
    let message_id = db::ulid();
    let now = db::now();
    let inserted = sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, queue_class, created_at, prompt_text, awaits_idle)
         VALUES (?, ?, NULL, 'web', 'queued', 'pending', ?, ?, ?, ?, 1)",
    )
    .bind(&turn_id)
    .bind(conversation_id)
    .bind(client_request_id)
    .bind(relay.queue_class.as_str())
    .bind(&now)
    .bind(deliver)
    .execute(&mut *tx)
    .await;
    if let Err(error) = inserted {
        // 唯一索引（每個對話最多一筆 queued）擋下來＝槽被 AGM 派工、別的 bot、啟動等待佔著：說是誰，不要只回一句英文。
        if error.as_database_error().is_some_and(|d| d.is_unique_violation()) {
            drop(tx);
            let occupant: Option<String> = sqlx::query_scalar(
                "SELECT id FROM turns WHERE conversation_id=? AND status='queued' AND queue_class=? ORDER BY created_at, id LIMIT 1",
            )
            .bind(conversation_id)
            .bind(relay.queue_class.as_str())
            .fetch_optional(app.db())
                .await
                .ok()
                .flatten();
            if let Some(turn_id) = occupant {
                return Err(slot_taken(app, &turn_id).await);
            }
            return Err(prompt::queue_insert_error(bot_id, conversation_id, error));
        }
        return Err(prompt::queue_insert_error(bot_id, conversation_id, error));
    }

    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, relay_from, relay_unverified, created_at)
         VALUES (?, ?, ?, 'user', ?, 'web', ?, ?, ?, ?)",
    )
    .bind(&message_id)
    .bind(conversation_id)
    .bind(&turn_id)
    .bind(text)
    .bind(group_id)
    .bind(relay.from)
    .bind(relay.unverified)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.bind_attachments_tx(&message_id, files)
        .await
        .map_err(up)?;

    tx.commit().await.map_err(up)?;
    app.prompt_message_added(bot_id, &message_id).await;
    app.turn_changed(&turn_id).await;
    tracing::info!(bot = %bot_id, turn = %turn_id, "使用者 prompt 已持久排入忙碌 bot 的佇列");
    Ok(prompt::PromptOut {
        turn_id,
        message_id,
        delivery: "queued".into(),
        send_now: None,
    })
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;
    use crate::testing as tt;
    use std::sync::Mutex as StdMutex;

    async fn admit(app: &Arc<App>, conv: &str, bot_id: &str, text: &str, crid: &str) -> LcResult<prompt::PromptOut> {
        queue_awaiting_idle(app, conv, bot_id, text, text, crid, None, prompt::RelaySrc::trusted(None), &[]).await
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
        queue_awaiting_idle(app, conv, bot_id, text, text, crid, None, prompt::RelaySrc::daemon_notice(Some("daemon")), &[]).await
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
        admit_notice(&app, &conv, &bot.id, "子 agent 完成了", "child-done:c5:t5").await.unwrap();
        admit(&app, &conv, &bot.id, "使用者的一句", "busy-1685-critical-u").await.unwrap();
        tt::fake_run(&app, &bot.id).await;

        assert!(crate::supervisor::store::delivery_critical_anywhere(&app.db).await.unwrap(), "a queued row on a live idle run is delivery-critical");
        let lease = crate::supervisor::store::acquire_lease(&app.db, "restart", "runner", None, None, &db::iso_in(600), true, None, &json!({}))
            .await
            .unwrap();
        assert!(lease.is_none(), "the quiet lease is refused while either lane has a queued row");
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
