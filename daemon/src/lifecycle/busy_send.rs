//! Persist user prompts accepted while a running agent is busy (#733).

use super::{prompt, *};
use crate::attach::Attachment;

fn slot_taken(turn_id: &str) -> LcError {
    LcError::conflict("queue_slot_taken", json!({"turn_id": turn_id}))
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
        Some(turn_id) => Err(slot_taken(&turn_id)),
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
        return Err(slot_taken(&turn_id));
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
