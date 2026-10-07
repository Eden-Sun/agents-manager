//! Message and turn rows: insert, group, and the events they emit.

use crate::state::App;
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
    Ok(sqlx::query_as::<_, db::Message>("SELECT *, rowid AS seq FROM messages WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut **tx)
        .await?)
}

pub(crate) async fn emit_message_added(app: &impl crate::capabilities::Emit, bot_id: &str, message: db::Message) {
    app.emit("message_added", json!({ "bot_id": bot_id, "message": message })).await;
}

/// `insert_message` with a SPEC §13 `group_id` (project group chat).
#[allow(clippy::too_many_arguments)]
pub async fn insert_message_grouped(
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit),
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
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit),
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
    // 先讀 owner 再寫訊息（#613 要它們同一個交易）：deferred 的話讀完之後別的 writer 一 commit，INSERT 就 517，
    // `let _ =` 的呼叫端連說明訊息都默默丟了（#822）。
    let mut tx = db::begin_write(app.db()).await?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
        .bind(conversation_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("conversation {conversation_id} has no owner"))?;
    #[cfg(test)]
    super::race_point::hit("insert_message_after_owner_read", conversation_id).await;
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

pub async fn emit_turn(app: &Arc<App>, turn_id: &str) {
    let row = sqlx::query_as::<_, TurnEventRow>(
        "SELECT t.*, c.bot_id AS bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id = ?",
    )
        .bind(turn_id)
        .fetch_optional(&app.db)
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
        // Hook and terminal-fallback completion both publish here, after their reply is committed.
        crate::runners::child_done::on_completed_turn(app, &t.id);
    }
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
        // 快取倒數（`cache_clock`）：`run.last_api_at` 取自回合的 `completed_at`，agent 先 idle、回合後收時要再推一次。
        if t.completed_at.is_some() {
            app.emit_bot_status(&bot_id).await;
        }
        schedule_flush_queued(app, &bot_id);
        // 回合收掉是「回合在飛」那種延後在等的邊：agent 早已 idle 時不會再有 idle 邊（#712）。
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

#[cfg(test)]
fn forget_turn_event_retry(turn_id: &str) {
    clear_turn_event_retry(turn_id);
}

fn schedule_turn_event_retry(app: &Arc<App>, turn_id: &str) {
    const BACKOFF_SECONDS: [u64; 5] = [2, 5, 15, 30, 60];
    const TEST_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

    let (generation, delay) = {
        let Ok(mut retries) = TURN_EVENT_RETRIES.get_or_init(Default::default).lock() else { return };
        let retry = retries.entry(turn_id.to_string()).or_insert(TurnEventRetry { attempt: 0, pending: None });
        if retry.pending.is_some() {
            return;
        }
        let delay = if cfg!(test) {
            TEST_RETRY_DELAY
        } else {
            std::time::Duration::from_secs(BACKOFF_SECONDS[retry.attempt.min(BACKOFF_SECONDS.len() - 1)])
        };
        retry.attempt += 1;
        let generation = TURN_EVENT_RETRY_SEQ.fetch_add(1, Ordering::Relaxed);
        retry.pending = Some(generation);
        (generation, delay)
    };

    let (app, turn_id) = (app.clone(), turn_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let should_retry = TURN_EVENT_RETRIES.get_or_init(Default::default).lock().map(|mut retries| {
            retries.get_mut(&turn_id).filter(|retry| retry.pending == Some(generation)).map(|retry| {
                retry.pending = None;
            }).is_some()
        }).unwrap_or(false);
        if should_retry {
            emit_turn(&app, &turn_id).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use tokio::sync::broadcast::error::TryRecvError;

    #[tokio::test]
    async fn unreadable_message_owner_does_not_persist_or_publish_until_retry() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "message-owner-retry").await;
        let conversation_id = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let mut events = app.subscribe();

        tt::make_table_unreadable(&app, "conversations").await;
        let first = insert_message(&app, &conversation_id, None, "assistant", "reply", "hook", false, None).await;
        tt::make_table_readable(&app, "conversations").await;

        assert!(first.is_err(), "owner lookup failure must be returned for retry: {first:?}");
        let failed_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?")
            .bind(&conversation_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(failed_rows, 0, "the failed attempt must not leave an unrouteable durable message");
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)), "an unverified owner cannot be published");

        let retried = insert_message(&app, &conversation_id, None, "assistant", "reply", "hook", false, None)
            .await
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match events.recv().await {
                    Ok(event) if event.kind == "message_added" && event.data["message"]["id"] == retried.id => break event,
                    Ok(_) => continue,
                    Err(error) => panic!("event bus closed before message retry: {error}"),
                }
            }
        })
        .await
        .expect("the retry should publish its message");
        assert_eq!(event.data["bot_id"], bot.id);
        assert_eq!(event.data["message"]["id"], retried.id);
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)), "the retry emits exactly one message event");
    }

    /// #822：讀完 owner、還沒 INSERT 訊息的那一瞬，一個不相干的 writer commit 了一筆。deferred 交易這時 INSERT 直接 517，
    /// `let _ = insert_message(..)` 的呼叫端連說明訊息都默默丟了。寫鎖從讀之前就拿著：插進來的那一筆等，訊息寫進去一次、
    /// 事件帶對的 owner。
    #[tokio::test]
    async fn an_unrelated_writer_between_the_owner_read_and_the_insert_does_not_lose_the_message() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "message-owner-831").await;
        let conversation_id = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let mut events = app.subscribe();
        let other = tt::arm_app_foreign_writer(&app, "insert_message_after_owner_read", &conversation_id);

        let m = insert_message(&app, &conversation_id, None, "system", "說明", "system", false, None)
            .await
            .expect("an unrelated writer must not make the message insert fail");
        assert_eq!(*other.lock().unwrap(), Some(false), "the insert holds the write lock from its owner read on; the other writer waits");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?").bind(&conversation_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(rows, 1);
        let event = loop {
            let ev = events.recv().await.unwrap();
            if ev.kind == "message_added" {
                break ev;
            }
        };
        assert_eq!(event.data["bot_id"], bot.id, "the event carries the owner read in the same transaction");
        assert_eq!(event.data["message"]["id"], m.id);
    }

    #[tokio::test]
    async fn unreadable_turn_owner_does_not_publish_an_empty_owner_and_recovers() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "emit-turn-owner").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let conversation = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let terminal = db::ulid();
        let queued = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'failed', 'unknown', ?)")
            .bind(&terminal).bind(&conversation).bind(&run_id).bind(db::now()).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, prompt_text) VALUES (?, ?, 'web', 'queued', 'pending', ?, 'wake after terminal turn')")
            .bind(&queued).bind(&conversation).bind(db::now()).execute(&app.db).await.unwrap();

        let mut turn_events = app.subscribe_turns();
        let mut ws_events = app.subscribe();
        let mut conn = app.db.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await.unwrap();
        sqlx::query("UPDATE conversations SET bot_id=CAST(bot_id AS BLOB) WHERE id=?").bind(&conversation).execute(&mut *conn).await.unwrap();
        drop(conn);

        emit_turn(&app, &terminal).await;

        assert!(matches!(turn_events.try_recv(), Err(TryRecvError::Empty)), "a terminal event needs a readable owner");
        assert!(matches!(ws_events.try_recv(), Err(TryRecvError::Empty)), "do not publish turn_updated with an empty bot id");

        let mut conn = app.db.acquire().await.unwrap();
        sqlx::query("UPDATE conversations SET bot_id=? WHERE id=?").bind(&bot.id).bind(&conversation).execute(&mut *conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.unwrap();
        drop(conn);

        let recovered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(event) = turn_events.recv().await {
                    if event.turn_id == terminal { break event; }
                }
            }
        }).await.expect("owner lookup should retry after SQLite recovers");
        assert_eq!(recovered.bot_id, bot.id);
        assert_eq!(super::super::take_scheduled_flush_count(&bot.id), 1, "the real owner receives the queue wake-up");
        assert_eq!(sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?").bind(&queued).fetch_one(&app.db).await.unwrap(), "queued", "the test scheduler is intentionally inert");
    }

    #[tokio::test]
    async fn queued_turn_wake_is_rearmed_after_restart_when_owner_read_recovers() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "emit-turn-restart").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let conversation = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let terminal = db::ulid();
        let queued = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'failed', 'unknown', ?)")
            .bind(&terminal).bind(&conversation).bind(&run_id).bind(db::now()).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, prompt_text) VALUES (?, ?, 'web', 'queued', 'pending', ?, 'wake after daemon restart')")
            .bind(&queued).bind(&conversation).bind(db::now()).execute(&app.db).await.unwrap();

        let mut conn = app.db.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await.unwrap();
        sqlx::query("UPDATE conversations SET bot_id=CAST(bot_id AS BLOB) WHERE id=?").bind(&conversation).execute(&mut *conn).await.unwrap();
        drop(conn);
        emit_turn(&app, &terminal).await;

        let restarted = tt::restart_app(&env).await;
        forget_turn_event_retry(&terminal);
        assert!(rearm_queue_retries(&restarted).await.is_err(), "startup retains its queue-recovery debt while ownership is unreadable");

        let mut conn = restarted.db.acquire().await.unwrap();
        sqlx::query("UPDATE conversations SET bot_id=? WHERE id=?").bind(&bot.id).bind(&conversation).execute(&mut *conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.unwrap();
        drop(conn);

        let pane = format!("pane-{}", bot.id);
        env.herdr.set_agent("agent", &pane, true);
        env.herdr.live_pane(&pane, tt::LivePane { width: Some(120), ..Default::default() });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let app_for_fire = restarted.clone();
        let fire = move |bot_id: String| {
            let (app, tx) = (app_for_fire.clone(), tx.clone());
            tokio::spawn(async move {
                let ok = flush_queued_locked(&app, &bot_id).await.is_ok();
                let _ = tx.send((bot_id, ok));
            });
        };
        assert_eq!(rearm_queue_retries_with(&restarted, fire).await.unwrap(), 1);
        let (woken, flushed) = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await
            .expect("a durable queued row is retried after restart")
            .expect("the retry callback reports the bot");
        assert_eq!(woken, bot.id, "the owner is resolved from the recovered DB row");
        assert!(flushed, "the recovered bot's queued prompt is flushed");
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&queued).fetch_one(&restarted.db).await.unwrap();
        assert_eq!(status, "in_flight", "the queued prompt is driven after restart");
    }
}
