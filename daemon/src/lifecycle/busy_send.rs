//! Persist user prompts accepted while a running agent is busy (#733).

use super::{prompt, *};
use crate::attach::Attachment;

/// 409 `queue_slot_taken`：唯一的 queued 槽被 `turn_id` 那一筆佔著，`holder` 說是誰（web 才講得出人話）。
async fn slot_taken(app: &Arc<App>, turn_id: &str) -> LcError {
    LcError::conflict("queue_slot_taken", json!({"turn_id": turn_id, "holder": slot_holder(app, turn_id).await}))
}

/// 佔槽的是誰：`{"kind": "user"｜"start"｜"daemon"｜"agm"｜"bot"｜"unknown", "bot_id"?, "bot_name"?}`。
/// `user`＝使用者自己（可能是另一個分頁）排的 `awaits_idle`；`start`＝在等 bot 起來的那一則（#122）；`daemon`＝daemon 的通知；
/// `agm`／`bot`＝別人派來的（`relay_from` 是 bot id；是 AGM 的 bot 算 `agm`；**自稱**的 `relay_from`〔沒帶 bot token，`relay_unverified`〕只說 `bot`，不帶名字與 id）。讀不到就是 `unknown`，不猜。
async fn slot_holder(app: &Arc<App>, turn_id: &str) -> Value {
    let row: Option<(i64, i64, Option<String>, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT t.awaits_idle, t.awaits_start, t.client_request_id,
                (SELECT m.relay_from FROM messages m WHERE m.turn_id = t.id AND m.role = 'user' ORDER BY m.created_at, m.rowid LIMIT 1),
                (SELECT m.relay_unverified FROM messages m WHERE m.turn_id = t.id AND m.role = 'user' ORDER BY m.created_at, m.rowid LIMIT 1)
           FROM turns t WHERE t.id = ?",
    )
    .bind(turn_id)
    .fetch_optional(&app.db)
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
        if let Ok(Some(bot)) = db::bot(&app.db, &from).await {
            let agm = match crate::supervisor_owned::load(&app.db).await {
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
pub(super) async fn refuse_if_slot_taken(app: &Arc<App>, conversation_id: &str) -> LcResult<()> {
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM turns WHERE conversation_id=? AND status='queued' AND awaits_idle=1 ORDER BY created_at, id LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(&app.db)
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
    app: &Arc<App>,
    conversation_id: &str,
    bot_id: &str,
    text: &str,
    deliver: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    relay: prompt::RelaySrc<'_>,
    files: &[Attachment],
) -> LcResult<prompt::PromptOut> {
    let mut tx = app.db.begin().await.map_err(up)?;
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM turns WHERE conversation_id=? AND status='queued' AND awaits_idle=1 ORDER BY created_at, id LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(up)?;
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
                .fetch_optional(&app.db)
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
    crate::attach::bind_tx(&mut tx, &message_id, files)
        .await
        .map_err(up)?;

    tx.commit().await.map_err(up)?;
    prompt::emit_prompt_message(app, bot_id, &message_id).await;
    super::messages::emit_turn(app, &turn_id).await;
    tracing::info!(bot = %bot_id, turn = %turn_id, "使用者 prompt 已持久排入忙碌 bot 的佇列");
    Ok(prompt::PromptOut {
        turn_id,
        message_id,
        delivery: "queued".into(),
        send_now: None,
    })
}
