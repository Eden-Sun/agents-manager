//! Message and turn rows: insert, group, and the events they emit.

use super::*;



pub async fn insert_message(
    app: &Arc<App>,
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
) -> anyhow::Result<db::Message> {
    insert_message_grouped(app, conversation_id, turn_id, role, content, source, incomplete, snapshot, None).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_message_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
) -> anyhow::Result<db::Message> {
    insert_message_relayed_tx(tx, conversation_id, turn_id, role, content, source, incomplete, snapshot, None).await
}

/// [`insert_message_tx`] 外加 `relay_from`（見 [`insert_message_full`]）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_message_relayed_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
    relay_from: Option<&str>,
) -> anyhow::Result<db::Message> {
    let id = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, incomplete, terminal_snapshot, relay_from, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(conversation_id)
    .bind(turn_id)
    .bind(role)
    .bind(content)
    .bind(source)
    .bind(incomplete as i64)
    .bind(snapshot)
    .bind(relay_from)
    .bind(&now)
    .execute(&mut **tx)
    .await?;
    Ok(sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut **tx)
        .await?)
}

pub(crate) async fn emit_message_added(app: &Arc<App>, bot_id: &str, message: db::Message) {
    app.emit("message_added", json!({ "bot_id": bot_id, "message": message })).await;
}

/// `insert_message` with a SPEC §13 `group_id` (project group chat).
#[allow(clippy::too_many_arguments)]
pub async fn insert_message_grouped(
    app: &Arc<App>,
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
    group_id: Option<&str>,
) -> anyhow::Result<db::Message> {
    insert_message_full(app, conversation_id, turn_id, role, content, source, incomplete, snapshot, group_id, None).await
}

/// 同上，外加 `relay_from`（別的 bot 送進來的，SPEC §6.5d）。INSERT 時就寫：`message_added`
/// 當下就推出去，事後 UPDATE 的話泡泡要重新載入才會變「AGM →」。
#[allow(clippy::too_many_arguments)]
pub async fn insert_message_full(
    app: &Arc<App>,
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
    group_id: Option<&str>,
    relay_from: Option<&str>,
) -> anyhow::Result<db::Message> {
    let id = db::ulid();

    let now = db::now();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, incomplete, terminal_snapshot, group_id, relay_from, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(conversation_id)
    .bind(turn_id)
    .bind(role)
    .bind(content)
    .bind(source)
    .bind(incomplete as i64)
    .bind(snapshot)
    .bind(group_id)
    .bind(relay_from)
    .bind(&now)
    .execute(&app.db)
    .await?;
    let m = sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE id = ?")
        .bind(&id)
        .fetch_one(&app.db)
        .await?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
        .bind(conversation_id)
        .fetch_one(&app.db)
        .await
        .unwrap_or_default();
    app.emit("message_added", json!({ "bot_id": bot_id, "message": m })).await;
    Ok(m)
}

pub async fn emit_turn(app: &Arc<App>, turn_id: &str) {
    if let Ok(Some(t)) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
    {
        let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
            .bind(&t.conversation_id)
            .fetch_one(&app.db)
            .await
            .unwrap_or_default();
        let should_flush_queue = t.status != "in_flight" && t.status != "queued";
        app.emit("turn_updated", json!({ "bot_id": bot_id, "turn": t })).await;
        // Every path that takes a turn out of `in_flight` funnels through here: one subscription suffices.
        app.publish_turn(crate::state::TurnEvent {
            bot_id: bot_id.clone(),
            turn_id: t.id.clone(),
            status: t.status.clone(),
            delivery: t.delivery.clone(),
        });
        // Schedule after publishing so the next prompt cannot race the completion event.
        if should_flush_queue {
            schedule_flush_queued(app, &bot_id);
        }
    }
}

