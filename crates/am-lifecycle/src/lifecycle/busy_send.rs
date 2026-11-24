//! Persist user prompts accepted while a running agent is busy (#733).

#[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
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
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::Cfg + BusySendServices + SupervisorSendRepo),
    conversation_id: &str,
    bot_id: &str,
    text: &str,
    deliver: &str,
    client_request_id: &str,
    relay: prompt::RelaySrc<'_>,
    files: &[Attachment],
) -> LcResult<prompt::PromptOut> {
    let mut tx = db::begin_write(app.db()).await.map_err(up)?;
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM turns WHERE conversation_id=? AND status='queued' AND awaits_idle=1 AND queue_class=? ORDER BY created_at, id LIMIT 1",
    )
    .bind(conversation_id)
    .bind(relay.queue_class.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(up)?;
    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
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
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, relay_unverified, relay_auth, created_at)
         VALUES (?, ?, ?, 'user', ?, 'web', ?, ?, ?, ?)",
    )
    .bind(&message_id)
    .bind(conversation_id)
    .bind(&turn_id)
    .bind(text)
    .bind(relay.from)
    .bind(relay.unverified)
    .bind(relay.auth)
    .bind(&now)
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.bind_attachments_tx(&message_id, files)
        .await
        .map_err(up)?;

    tx.commit().await.map_err(up)?;
    app.prompt_message_added(bot_id, &message_id).await;
    super::ledger_ingest::ingest_user_prompt(app, relay, bot_id, &message_id, &turn_id, text).await;
    app.turn_changed(&turn_id).await;
    tracing::info!(bot = %bot_id, turn = %turn_id, "使用者 prompt 已持久排入忙碌 bot 的佇列");
    Ok(prompt::PromptOut {
        turn_id,
        message_id,
        delivery: "queued".into(),
        send_now: None,
    })
}

#[cfg(any(test, feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
#[doc(hidden)]
pub mod test_seams {
    //! #1862 L2：daemon 測試要碰的私有項，窄轉呼叫（只在測試建置存在）。
    use super::*;
    pub async fn queue_awaiting_idle(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::Cfg + BusySendServices + SupervisorSendRepo), conversation_id: &str, bot_id: &str, text: &str, deliver: &str, client_request_id: &str, relay: prompt::RelaySrc<'_>, files: &[Attachment]) -> LcResult<prompt::PromptOut> { super::queue_awaiting_idle(app, conversation_id, bot_id, text, deliver, client_request_id, relay, files).await }
}
