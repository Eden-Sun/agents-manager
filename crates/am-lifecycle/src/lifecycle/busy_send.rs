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
/// `user`＝使用者自己（可能是另一個分頁）排的 `awaits_idle`；`start`＝在等 bot 起來的那一則（#122）；`daemon`＝daemon 的通知；
/// `agm`／`bot`＝別人派來的（`relay_from` 是 bot id；是 AGM 的 bot 算 `agm`；**自稱**的 `relay_from`〔沒帶 bot token，`relay_unverified`〕只說 `bot`，不帶名字與 id）。讀不到就是 `unknown`，不猜。
async fn slot_holder(app: &(impl crate::capabilities::Db + SupervisorSendRepo), turn_id: &str) -> Value {
    let row: Option<(i64, i64, Option<String>, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT t.awaits_idle, t.awaits_start, t.client_request_id,
                (SELECT m.relay_from FROM messages m WHERE m.turn_id = t.id AND m.role = 'user' ORDER BY m.created_at, m.rowid LIMIT 1),
                (SELECT m.relay_unverified FROM messages m WHERE m.turn_id = t.id AND m.role = 'user' ORDER BY m.created_at, m.rowid LIMIT 1)
           FROM turns t WHERE t.id = ?",
    )
    .bind(turn_id)
    .fetch_optional(app.db())
    .await
    .ok()
    .flatten();
    let Some((awaits_idle, awaits_start, crid, relay_from, relay_unverified)) = row else { return json!({"kind": "unknown"}) };
    if awaits_start == 1 {
        return json!({"kind": "start"});
    }
    if awaits_idle == 1 && relay_from.is_none() {
        return json!({"kind": "user"});
    }
    if relay_from.as_deref() == Some("daemon") || super::daemon_notice::is_daemon_notice(crid.as_deref()) {
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
pub(super) async fn refuse_if_slot_taken(app: &(impl crate::capabilities::Db + SupervisorSendRepo), conversation_id: &str) -> LcResult<()> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM turns WHERE conversation_id=? AND status='queued' AND awaits_idle=1 ORDER BY created_at, id LIMIT 1",
    )
    .bind(conversation_id)
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
        "SELECT id FROM turns WHERE conversation_id=? AND status='queued' AND awaits_idle=1 ORDER BY created_at, id LIMIT 1",
    )
    .bind(conversation_id)
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
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, prompt_text, awaits_idle)
         VALUES (?, ?, NULL, 'web', 'queued', 'pending', ?, ?, ?, 1)",
    )
    .bind(&turn_id)
    .bind(conversation_id)
    .bind(client_request_id)
    .bind(&now)
    .bind(deliver)
    .execute(&mut *tx)
    .await;
    if let Err(error) = inserted {
        // 唯一索引（每個對話最多一筆 queued）擋下來＝槽被 AGM 派工、別的 bot、啟動等待佔著：說是誰，不要只回一句英文。
        if error.as_database_error().is_some_and(|d| d.is_unique_violation()) {
            drop(tx);
            let occupant: Option<String> = sqlx::query_scalar("SELECT id FROM turns WHERE conversation_id=? AND status='queued' ORDER BY created_at, id LIMIT 1")
                .bind(conversation_id)
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
