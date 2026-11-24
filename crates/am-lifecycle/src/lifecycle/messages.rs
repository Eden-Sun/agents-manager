//! Message and turn rows: insert, group, and the events they emit.

use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

pub async fn insert_message(
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit),
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
) -> anyhow::Result<db::Message> {
    insert_message_full(app, conversation_id, turn_id, role, content, source, incomplete, snapshot, None).await
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_message_tx(
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
pub async fn insert_message_relayed_tx(
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
    Ok(sqlx::query_as::<_, db::Message>("SELECT *, rowid AS seq FROM messages WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut **tx)
        .await?)
}

pub async fn emit_message_added(app: &impl crate::capabilities::Emit, bot_id: &str, message: db::Message) {
    app.emit("message_added", json!({ "bot_id": bot_id, "message": message })).await;
}

/// 別的 bot 送來的訊息記下寄件 bot 送它當下正在跑的回合（`relay_turn_id`，#927）。跟插入同一個交易，寫在 [`insert_message_relayed_tx`] 之後。
pub async fn mark_relay_turn(conn: &mut sqlx::SqliteConnection, message_id: &str, turn: Option<&str>) -> anyhow::Result<()> {
    let Some(turn) = turn else { return Ok(()) };
    sqlx::query("UPDATE messages SET relay_turn_id = ? WHERE id = ?").bind(turn).bind(message_id).execute(conn).await?;
    Ok(())
}

/// 同上，外加 `relay_from`（別的 bot 送進來的，SPEC §6.5d）。INSERT 時就寫：`message_added`
/// 當下就推出去，事後 UPDATE 的話泡泡要重新載入才會變「AGM →」。
#[allow(clippy::too_many_arguments)]
pub async fn insert_message_full(
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit),
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
    relay_from: Option<&str>,
) -> anyhow::Result<db::Message> {
    let mut tx = db::begin_write(app.db()).await?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
        .bind(conversation_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("conversation {conversation_id} has no owner"))?;
    #[cfg(feature = "test-hooks")]
    super::race_point::hit("insert_message_after_owner_read", conversation_id).await;
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
    .execute(&mut *tx)
    .await?;
    let m = sqlx::query_as::<_, db::Message>("SELECT *, rowid AS seq FROM messages WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    app.emit("message_added", json!({ "bot_id": bot_id, "message": m })).await;
    Ok(m)
}

/// 帶有自訂 id（冪等鍵）的訊息插入。如果該 id 已存在，直接回傳已存在的訊息（不發出重複的 `message_added` 事件）。
pub async fn insert_message_idempotent(
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit),
    conversation_id: &str,
    role: &str,
    content: &str,
    source: &str,
    id: &str,
) -> anyhow::Result<db::Message> {
    let mut tx = db::begin_write(app.db()).await?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
        .bind(conversation_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("conversation {conversation_id} has no owner"))?;
    if let Some(existing) = sqlx::query_as::<_, db::Message>("SELECT *, rowid AS seq FROM messages WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
    {
        return Ok(existing);
    }
    let now = db::now();
    let n = sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, incomplete, created_at)
         VALUES (?,?,NULL,?,?,?,0,?) ON CONFLICT(id) DO NOTHING",
    )
    .bind(id)
    .bind(conversation_id)
    .bind(role)
    .bind(content)
    .bind(source)
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let m = sqlx::query_as::<_, db::Message>("SELECT *, rowid AS seq FROM messages WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    if n > 0 {
        app.emit("message_added", json!({ "bot_id": bot_id, "message": m })).await;
    }
    Ok(m)
}

pub async fn emit_turn(app: &impl LcHost, turn_id: &str) {
    let row = sqlx::query_as::<_, TurnEventRow>(
        "SELECT t.*, c.bot_id AS bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id = ?",
    )
        .bind(turn_id)
        .fetch_optional(app.db())
        .await;
    let row = match row {
        Ok(Some(row)) => {
            clear_turn_event_retry(turn_id);
            row
        }
        Ok(None) => {
            clear_turn_event_retry(turn_id);
            return;
        }
        Err(error) => {
            tracing::warn!(turn = turn_id, error = %error, "could not read turn owner; event and queue wake-up will be retried");
            schedule_turn_event_retry(app, turn_id);
            return;
        }
    };
    let t = row.turn;
    let bot_id = row.bot_id;
    let should_flush_queue = t.status != "in_flight" && t.status != "queued";
    if matches!(t.status.as_str(), "completed" | "completed_fallback") {
        crate::lifecycle::s6_ports::TurnEventHostServices::child_done_after_completed_turn(app, &t.id);
    }
    app.emit("turn_updated", json!({ "bot_id": bot_id, "turn": t })).await;
    crate::lifecycle::s6_ports::TurnEventHostServices::publish_lifecycle_turn(
        app,
        &bot_id,
        &t.id,
        &t.status,
        &t.delivery,
    );
    if should_flush_queue {
        if t.completed_at.is_some() {
            app.emit_bot_status(&bot_id).await;
        }
        schedule_flush_queued(app, &bot_id);
        super::schedule_deferred_live(app, &bot_id);
    }
}

#[derive(sqlx::FromRow)]
struct TurnEventRow {
    #[sqlx(flatten)]
    turn: db::Turn,
    bot_id: String,
}

struct TurnEventRetry {
    attempt: usize,
    pending: Option<u64>,
}

static TURN_EVENT_RETRIES: OnceLock<Mutex<HashMap<String, TurnEventRetry>>> = OnceLock::new();
static TURN_EVENT_RETRY_SEQ: AtomicU64 = AtomicU64::new(1);

fn clear_turn_event_retry(turn_id: &str) {
    if let Ok(mut retries) = TURN_EVENT_RETRIES.get_or_init(Default::default).lock() {
        retries.remove(turn_id);
    }
}

#[cfg(feature = "test-hooks")]
fn forget_turn_event_retry(turn_id: &str) {
    clear_turn_event_retry(turn_id);
}

fn schedule_turn_event_retry(app: &impl LcHost, turn_id: &str) {
    const BACKOFF_SECONDS: [u64; 5] = [2, 5, 15, 30, 60];
    const TEST_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

    let (generation, delay) = {
        let Ok(mut retries) = TURN_EVENT_RETRIES.get_or_init(Default::default).lock() else { return };
        let retry = retries.entry(turn_id.to_string()).or_insert(TurnEventRetry { attempt: 0, pending: None });
        if retry.pending.is_some() {
            return;
        }
        let delay = if cfg!(any(test, feature = "test-hooks")) {
            TEST_RETRY_DELAY
        } else {
            std::time::Duration::from_secs(BACKOFF_SECONDS[retry.attempt.min(BACKOFF_SECONDS.len() - 1)])
        };
        retry.attempt += 1;
        let generation = TURN_EVENT_RETRY_SEQ.fetch_add(1, Ordering::Relaxed);
        retry.pending = Some(generation);
        (generation, delay)
    };

    let (app, turn_id) = ((*app).clone(), turn_id.to_string());
    tokio::spawn(Box::pin(async move {
        tokio::time::sleep(delay).await;
        let should_retry = TURN_EVENT_RETRIES.get_or_init(Default::default).lock().map(|mut retries| {
            retries.get_mut(&turn_id).filter(|retry| retry.pending == Some(generation)).map(|retry| {
                retry.pending = None;
            }).is_some()
        }).unwrap_or(false);
        if should_retry {
            emit_turn(&app, &turn_id).await;
        }
    }));
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    use super::*;
    #[cfg(feature = "test-hooks")]
    pub fn forget_turn_event_retry(turn_id: &str) { super::forget_turn_event_retry(turn_id) }
}
