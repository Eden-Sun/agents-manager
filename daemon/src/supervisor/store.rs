//! Supervisor (AGM) persistence — the authoritative record behind the manager LLM.
//!
//! The manager model only understands and decides; every fact it is allowed to rely on lives
//! in these tables, so a daemon restart, a model switch or a lost session never loses an
//! assignment or replays one twice.

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{FromRow, SqlitePool};

/// There is exactly one supervisor, and its id is stable across restarts and model switches.
pub const SUPERVISOR_ID: &str = "AGM";

pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS supervisors (
  id TEXT PRIMARY KEY,
  bot_id TEXT,
  project_id TEXT,
  cwd TEXT,
  identity TEXT NOT NULL DEFAULT 'cc0',
  effort TEXT NOT NULL DEFAULT 'low',
  -- `fable` | `opus`: the candidate currently in use, updated only *after* the switch applied.
  active_model TEXT NOT NULL DEFAULT 'fable',
  -- Bumped on every model switch / re-setup; an older controller sees the mismatch and stops.
  generation INTEGER NOT NULL DEFAULT 0,
  -- Sticky override: 'waiting_quota' | 'failed' | '' (derive from the run).
  status TEXT NOT NULL DEFAULT '',
  status_detail TEXT,
  -- How many automatic candidate switches happened inside the current cooldown window.
  fallback_tries INTEGER NOT NULL DEFAULT 0,
  cooldown_until TEXT,
  quota_reset_at TEXT,
  -- Never asserted from argv: only a verified Remote Control session sets 'active'.
  remote_status TEXT NOT NULL DEFAULT 'unknown',
  remote_url TEXT,
  summary TEXT,
  summary_version INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS supervisor_requests (
  id TEXT PRIMARY KEY,
  supervisor_id TEXT NOT NULL,
  -- 'web' | 'native' | 'api': where the user's words came in from.
  source TEXT NOT NULL DEFAULT 'api',
  -- Stable id of that source (turn / message id). NULL when the caller had none — never a
  -- hash of the text, because two identical asks are two asks.
  source_key TEXT,
  text TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'open',
  created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS supervisor_requests_source
  ON supervisor_requests(supervisor_id, source_key) WHERE source_key IS NOT NULL;
CREATE TABLE IF NOT EXISTS supervisor_assignments (
  id TEXT PRIMARY KEY,
  supervisor_id TEXT NOT NULL,
  request_id TEXT,
  target_bot_id TEXT NOT NULL,
  -- The one id every retry of this assignment reuses, so `lifecycle::prompt` dedupes for us.
  client_request_id TEXT NOT NULL,
  turn_id TEXT,
  text TEXT NOT NULL,
  -- queued | delivered | unknown | completed | failed | cancelled
  status TEXT NOT NULL DEFAULT 'queued',
  delivery TEXT,
  result TEXT,
  error TEXT,
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  completed_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS supervisor_assignments_crid
  ON supervisor_assignments(supervisor_id, client_request_id);
CREATE INDEX IF NOT EXISTS supervisor_assignments_open
  ON supervisor_assignments(status) WHERE status IN ('queued','delivered','unknown');
CREATE INDEX IF NOT EXISTS supervisor_assignments_turn ON supervisor_assignments(turn_id);
CREATE TABLE IF NOT EXISTS supervisor_inbox (
  id TEXT PRIMARY KEY,
  supervisor_id TEXT NOT NULL,
  -- Dedupe key for a replayed / re-scanned event, e.g. `turn:<turn_id>:completed`.
  event_key TEXT NOT NULL,
  assignment_id TEXT,
  bot_id TEXT,
  turn_id TEXT,
  kind TEXT NOT NULL,
  payload_json TEXT NOT NULL DEFAULT '{}',
  -- pending (not shown to the manager yet) | delivered (prompted) | handled (acked)
  state TEXT NOT NULL DEFAULT 'pending',
  notify_turn_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS supervisor_inbox_key ON supervisor_inbox(supervisor_id, event_key);
CREATE INDEX IF NOT EXISTS supervisor_inbox_open ON supervisor_inbox(state) WHERE state != 'handled';
CREATE TABLE IF NOT EXISTS supervisor_notes (
  id TEXT PRIMARY KEY,
  supervisor_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  body TEXT NOT NULL,
  version INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);
"#;

/// Called from `db::migrate` on the same single-connection pool as the rest of the schema.
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    for stmt in DDL.split(";\n") {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        sqlx::query(s).execute(pool).await?;
    }
    // Additive columns for databases created before they existed (same pattern as `db::migrate`).
    for (col, ddl) in [
        // 1 once the user has started the manager, 0 after `supervisor-stop`. The watchdog only
        // brings back a manager that is *supposed* to be running: a fresh `setup` stays down
        // until a human starts it once, and an explicit stop stays stopped.
        ("desired_running", "ALTER TABLE supervisors ADD COLUMN desired_running INTEGER NOT NULL DEFAULT 0"),
        // Consecutive automatic starts that did not bring the manager back, and when the next
        // one is due. Shared through the row so a second controller loop sees the same count.
        ("watchdog_attempts", "ALTER TABLE supervisors ADD COLUMN watchdog_attempts INTEGER NOT NULL DEFAULT 0"),
        ("watchdog_next_at", "ALTER TABLE supervisors ADD COLUMN watchdog_next_at TEXT"),
        // When the manager was last woken with its inbox. The notify throttle reads it, so it
        // has to survive a restart: a reboot must not turn into an extra wake-up.
        ("last_notify_at", "ALTER TABLE supervisors ADD COLUMN last_notify_at TEXT"),
    ] {
        if !has_column(pool, "supervisors", col).await? {
            sqlx::query(ddl).execute(pool).await?;
            if col == "desired_running" {
                // A manager that was started before this column existed (generation > 0 is
                // only ever bumped by `start`) was never told to stop, so it is still wanted.
                sqlx::query("UPDATE supervisors SET desired_running=1 WHERE bot_id IS NOT NULL AND generation>0")
                    .execute(pool)
                    .await?;
            }
        }
    }
    Ok(())
}

async fn has_column(pool: &SqlitePool, table: &str, col: &str) -> Result<bool> {
    let cols: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as(&format!("PRAGMA table_info({table})")).fetch_all(pool).await?;
    Ok(cols.iter().any(|c| c.1 == col))
}

/// Mirrors the row: `FromRow` needs every column, and not all of them have a
/// reader yet (the front end reads several straight out of the JSON).
#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct Supervisor {
    pub id: String,
    pub bot_id: Option<String>,
    pub project_id: Option<String>,
    pub cwd: Option<String>,
    pub identity: String,
    pub effort: String,
    pub active_model: String,
    pub generation: i64,
    pub status: String,
    pub status_detail: Option<String>,
    pub fallback_tries: i64,
    pub cooldown_until: Option<String>,
    pub quota_reset_at: Option<String>,
    pub remote_status: String,
    pub remote_url: Option<String>,
    pub summary: Option<String>,
    pub summary_version: i64,
    pub desired_running: i64,
    pub watchdog_attempts: i64,
    pub watchdog_next_at: Option<String>,
    pub last_notify_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Supervisor {
    pub fn wants_running(&self) -> bool {
        self.desired_running != 0
    }
}

/// Mirrors the row: `FromRow` needs every column, and not all of them have a
/// reader yet (the front end reads several straight out of the JSON).
#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct Assignment {
    pub id: String,
    pub supervisor_id: String,
    pub request_id: Option<String>,
    pub target_bot_id: String,
    pub client_request_id: String,
    pub turn_id: Option<String>,
    pub text: String,
    pub status: String,
    pub delivery: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub attempts: i64,
    pub next_attempt_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

impl Assignment {
    /// The wire shape the front end and the `agm` CLI agreed on.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "target_bot_id": self.target_bot_id,
            "client_request_id": self.client_request_id,
            "turn_id": self.turn_id,
            "status": self.status,
            "text": self.text,
            "delivery": self.delivery,
            "result": self.result,
            "error": self.error,
            "attempts": self.attempts,
            "request_id": self.request_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "completed_at": self.completed_at,
        })
    }

    pub fn is_open(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "delivered" | "unknown")
    }
}

/// Mirrors the row: `FromRow` needs every column, and not all of them have a
/// reader yet (the front end reads several straight out of the JSON).
#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct InboxEvent {
    pub id: String,
    pub event_key: String,
    pub assignment_id: Option<String>,
    pub bot_id: Option<String>,
    pub turn_id: Option<String>,
    pub kind: String,
    pub payload_json: String,
    pub state: String,
    pub notify_turn_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl InboxEvent {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "event_key": self.event_key,
            "assignment_id": self.assignment_id,
            "bot_id": self.bot_id,
            "turn_id": self.turn_id,
            "kind": self.kind,
            "payload": serde_json::from_str::<Value>(&self.payload_json).unwrap_or_else(|_| json!({})),
            "state": self.state,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }
}

// ---------------------------------------------------------------- supervisor row

/// Read the one supervisor row, creating the "nothing set up yet" default on first call.
pub async fn get_or_init(pool: &SqlitePool) -> Result<Supervisor> {
    if let Some(s) = sqlx::query_as::<_, Supervisor>("SELECT * FROM supervisors WHERE id=?")
        .bind(SUPERVISOR_ID)
        .fetch_optional(pool)
        .await?
    {
        return Ok(s);
    }
    let now = crate::db::now();
    sqlx::query(
        "INSERT OR IGNORE INTO supervisors (id, identity, effort, active_model, created_at, updated_at)
         VALUES (?, 'cc0', 'low', 'fable', ?, ?)",
    )
    .bind(SUPERVISOR_ID)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(sqlx::query_as::<_, Supervisor>("SELECT * FROM supervisors WHERE id=?")
        .bind(SUPERVISOR_ID)
        .fetch_one(pool)
        .await?)
}

pub async fn set_env(pool: &SqlitePool, bot_id: &str, project_id: &str, cwd: &str) -> Result<()> {
    sqlx::query("UPDATE supervisors SET bot_id=?, project_id=?, cwd=?, updated_at=? WHERE id=?")
        .bind(bot_id)
        .bind(project_id)
        .bind(cwd)
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_status(pool: &SqlitePool, status: &str, detail: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE supervisors SET status=?, status_detail=?, updated_at=? WHERE id=?")
        .bind(status)
        .bind(detail)
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .execute(pool)
        .await?;
    Ok(())
}

/// Change only the human-readable detail; the sticky `status` stays what it is.
pub async fn set_status_detail(pool: &SqlitePool, detail: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE supervisors SET status_detail=?, updated_at=? WHERE id=?")
        .bind(detail)
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .execute(pool)
        .await?;
    Ok(())
}

/// `start` → true, `stop` → false. Either one is a fresh decision by the user, so the
/// watchdog's failure count starts over with it.
pub async fn set_desired_running(pool: &SqlitePool, wanted: bool) -> Result<()> {
    sqlx::query(
        "UPDATE supervisors SET desired_running=?, watchdog_attempts=0, watchdog_next_at=NULL, updated_at=? WHERE id=?",
    )
    .bind(i64::from(wanted))
    .bind(crate::db::now())
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_watchdog(pool: &SqlitePool, attempts: i64, next_at: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE supervisors SET watchdog_attempts=?, watchdog_next_at=?, updated_at=? WHERE id=?")
        .bind(attempts)
        .bind(next_at)
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record that the manager was just woken. Only the successful notify path calls this: a
/// failed prompt must not start the next window, or a flaky send silently drops a window's
/// worth of events.
pub async fn set_last_notify(pool: &SqlitePool, at: &str) -> Result<()> {
    sqlx::query("UPDATE supervisors SET last_notify_at=?, updated_at=? WHERE id=?")
        .bind(at)
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_remote(pool: &SqlitePool, status: &str, url: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE supervisors SET remote_status=?, remote_url=?, updated_at=? WHERE id=?")
        .bind(status)
        .bind(url)
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .execute(pool)
        .await?;
    Ok(())
}

/// Applying a candidate switch: the new model is recorded only once it is actually in effect,
/// and the generation bump retires whatever controller was running for the old one.
pub async fn set_active_model(pool: &SqlitePool, model: &str, cooldown_until: Option<&str>) -> Result<i64> {
    sqlx::query(
        "UPDATE supervisors SET active_model=?, generation=generation+1, fallback_tries=fallback_tries+1,
           cooldown_until=?, updated_at=? WHERE id=?",
    )
    .bind(model)
    .bind(cooldown_until)
    .bind(crate::db::now())
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?;
    Ok(sqlx::query_scalar::<_, i64>("SELECT generation FROM supervisors WHERE id=?")
        .bind(SUPERVISOR_ID)
        .fetch_one(pool)
        .await?)
}

pub async fn bump_generation(pool: &SqlitePool) -> Result<i64> {
    sqlx::query("UPDATE supervisors SET generation=generation+1, updated_at=? WHERE id=?")
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .execute(pool)
        .await?;
    Ok(sqlx::query_scalar::<_, i64>("SELECT generation FROM supervisors WHERE id=?")
        .bind(SUPERVISOR_ID)
        .fetch_one(pool)
        .await?)
}

/// Clear the bounded-retry budget once the cooldown has passed, so a later, genuinely new
/// failure gets its own switch instead of being stuck at "already tried".
pub async fn clear_fallback_budget(pool: &SqlitePool) -> Result<()> {
    sqlx::query("UPDATE supervisors SET fallback_tries=0, cooldown_until=NULL, updated_at=? WHERE id=?")
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_quota_reset(pool: &SqlitePool, reset_at: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE supervisors SET quota_reset_at=?, updated_at=? WHERE id=?")
        .bind(reset_at)
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_summary(pool: &SqlitePool, summary: &str) -> Result<i64> {
    sqlx::query(
        "UPDATE supervisors SET summary=?, summary_version=summary_version+1, updated_at=? WHERE id=?",
    )
    .bind(summary)
    .bind(crate::db::now())
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?;
    let v: i64 = sqlx::query_scalar("SELECT summary_version FROM supervisors WHERE id=?")
        .bind(SUPERVISOR_ID)
        .fetch_one(pool)
        .await?;
    sqlx::query("INSERT INTO supervisor_notes (id, supervisor_id, kind, body, version, created_at) VALUES (?,?,?,?,?,?)")
        .bind(crate::db::ulid())
        .bind(SUPERVISOR_ID)
        .bind("handoff")
        .bind(summary)
        .bind(v)
        .bind(crate::db::now())
        .execute(pool)
        .await?;
    Ok(v)
}

// ---------------------------------------------------------------- requests

/// Record the user's words behind an assignment. Returns the existing row when the same
/// `source_key` comes back (a replayed phone message must not become a second request).
pub async fn upsert_request(pool: &SqlitePool, source: &str, source_key: Option<&str>, text: &str) -> Result<String> {
    if let Some(k) = source_key.filter(|s| !s.trim().is_empty()) {
        if let Some(id) = sqlx::query_scalar::<_, String>(
            "SELECT id FROM supervisor_requests WHERE supervisor_id=? AND source_key=?",
        )
        .bind(SUPERVISOR_ID)
        .bind(k)
        .fetch_optional(pool)
        .await?
        {
            return Ok(id);
        }
    }
    let id = crate::db::ulid();
    sqlx::query(
        "INSERT INTO supervisor_requests (id, supervisor_id, source, source_key, text, status, created_at)
         VALUES (?,?,?,?,?,'open',?)",
    )
    .bind(&id)
    .bind(SUPERVISOR_ID)
    .bind(source)
    .bind(source_key.filter(|s| !s.trim().is_empty()))
    .bind(text)
    .bind(crate::db::now())
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn requests(pool: &SqlitePool, limit: i64) -> Result<Vec<Value>> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>, String, String, String)>(
        "SELECT id, source, source_key, text, status, created_at FROM supervisor_requests
          WHERE supervisor_id=? ORDER BY created_at DESC LIMIT ?",
    )
    .bind(SUPERVISOR_ID)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, source, key, text, status, created_at)| {
            json!({"id": id, "source": source, "source_key": key, "text": text, "status": status, "created_at": created_at})
        })
        .collect())
}

// ---------------------------------------------------------------- assignments

pub async fn assignment(pool: &SqlitePool, id: &str) -> Result<Option<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>("SELECT * FROM supervisor_assignments WHERE id=?")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn assignment_by_crid(pool: &SqlitePool, crid: &str) -> Result<Option<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        "SELECT * FROM supervisor_assignments WHERE supervisor_id=? AND client_request_id=?",
    )
    .bind(SUPERVISOR_ID)
    .bind(crid)
    .fetch_optional(pool)
    .await?)
}

pub async fn assignment_by_turn(pool: &SqlitePool, turn_id: &str) -> Result<Option<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>("SELECT * FROM supervisor_assignments WHERE turn_id=? LIMIT 1")
        .bind(turn_id)
        .fetch_optional(pool)
        .await?)
}

/// Write the assignment down *before* anything is sent. A crash between here and the prompt
/// leaves a `queued` row the controller picks up again with the same client_request_id.
pub async fn insert_assignment(
    pool: &SqlitePool,
    request_id: Option<&str>,
    target_bot_id: &str,
    client_request_id: &str,
    text: &str,
) -> Result<Assignment> {
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO supervisor_assignments
           (id, supervisor_id, request_id, target_bot_id, client_request_id, text, status, attempts, created_at, updated_at)
         VALUES (?,?,?,?,?,?, 'queued', 0, ?, ?)",
    )
    .bind(crate::db::ulid())
    .bind(SUPERVISOR_ID)
    .bind(request_id)
    .bind(target_bot_id)
    .bind(client_request_id)
    .bind(text)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(assignment_by_crid(pool, client_request_id).await?.expect("just inserted"))
}

pub async fn list_assignments(pool: &SqlitePool, limit: i64) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        "SELECT * FROM supervisor_assignments WHERE supervisor_id=? ORDER BY created_at DESC LIMIT ?",
    )
    .bind(SUPERVISOR_ID)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn open_assignments(pool: &SqlitePool) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        "SELECT * FROM supervisor_assignments WHERE supervisor_id=? AND status IN ('queued','delivered','unknown')
          ORDER BY created_at ASC",
    )
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?)
}

pub async fn mark_delivered(pool: &SqlitePool, id: &str, turn_id: &str, delivery: &str) -> Result<()> {
    // `unknown` delivery is its own state: the prompt may or may not have landed, so the
    // controller reconciles it against the turn instead of sending the work a second time.
    let status = if delivery == "unknown" { "unknown" } else { "delivered" };
    sqlx::query(
        "UPDATE supervisor_assignments SET turn_id=?, delivery=?, status=?, attempts=attempts+1,
           next_attempt_at=NULL, updated_at=? WHERE id=?",
    )
    .bind(turn_id)
    .bind(delivery)
    .bind(status)
    .bind(crate::db::now())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn defer(pool: &SqlitePool, id: &str, next_attempt_at: &str, why: &str) -> Result<()> {
    sqlx::query(
        "UPDATE supervisor_assignments SET attempts=attempts+1, next_attempt_at=?, error=?, updated_at=?
          WHERE id=? AND status='queued'",
    )
    .bind(next_attempt_at)
    .bind(why)
    .bind(crate::db::now())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn finish(pool: &SqlitePool, id: &str, status: &str, result: Option<&str>, error: Option<&str>) -> Result<()> {
    let now = crate::db::now();
    sqlx::query(
        "UPDATE supervisor_assignments SET status=?, result=COALESCE(?, result), error=COALESCE(?, error),
           completed_at=?, updated_at=? WHERE id=?",
    )
    .bind(status)
    .bind(result)
    .bind(error)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- inbox

/// Insert an event unless its `event_key` is already known. `Ok(None)` = a duplicate, which is
/// the normal outcome for a replayed turn event or a restart rescan.
pub async fn push_inbox(
    pool: &SqlitePool,
    event_key: &str,
    kind: &str,
    assignment_id: Option<&str>,
    bot_id: Option<&str>,
    turn_id: Option<&str>,
    payload: &Value,
) -> Result<Option<String>> {
    let id = crate::db::ulid();
    let now = crate::db::now();
    let res = sqlx::query(
        "INSERT OR IGNORE INTO supervisor_inbox
           (id, supervisor_id, event_key, assignment_id, bot_id, turn_id, kind, payload_json, state, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?, 'pending', ?, ?)",
    )
    .bind(&id)
    .bind(SUPERVISOR_ID)
    .bind(event_key)
    .bind(assignment_id)
    .bind(bot_id)
    .bind(turn_id)
    .bind(kind)
    .bind(payload.to_string())
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok((res.rows_affected() > 0).then_some(id))
}

pub async fn inbox(pool: &SqlitePool, limit: i64) -> Result<Vec<InboxEvent>> {
    Ok(sqlx::query_as::<_, InboxEvent>(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? ORDER BY created_at DESC LIMIT ?",
    )
    .bind(SUPERVISOR_ID)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Everything the manager has not acked yet — `pending` *and* `delivered` — oldest first, so a
/// backlog is worked through in the order it happened and an ack loop actually drains it.
pub async fn open_inbox(pool: &SqlitePool, limit: i64) -> Result<Vec<InboxEvent>> {
    Ok(sqlx::query_as::<_, InboxEvent>(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? AND state!='handled' ORDER BY created_at ASC, id ASC LIMIT ?",
    )
    .bind(SUPERVISOR_ID)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn pending_inbox(pool: &SqlitePool) -> Result<Vec<InboxEvent>> {
    Ok(sqlx::query_as::<_, InboxEvent>(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? AND state='pending' ORDER BY created_at ASC",
    )
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?)
}

/// Assignments not yet closed: queued, delivered or of unknown delivery.
pub async fn open_assignment_count(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM supervisor_assignments WHERE supervisor_id=? AND status IN ('queued','delivered','unknown')",
    )
    .bind(SUPERVISOR_ID)
    .fetch_one(pool)
    .await?)
}

/// Inbox events the manager has not acked.
pub async fn open_inbox_count(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE supervisor_id=? AND state!='handled'")
        .bind(SUPERVISOR_ID)
        .fetch_one(pool)
        .await?)
}

/// Count of everything still owed to the manager: unhandled notifications plus open work.
pub async fn pending_count(pool: &SqlitePool) -> Result<i64> {
    Ok(open_assignment_count(pool).await? + open_inbox_count(pool).await?)
}

/// Prompted, not yet confirmed. Deliberately *not* `handled`: only the manager acking it —
/// or the UI acking on its behalf — closes the event, so a failed notify keeps it queued.
pub async fn mark_delivered_inbox(pool: &SqlitePool, ids: &[String], notify_turn_id: &str) -> Result<()> {
    for id in ids {
        sqlx::query("UPDATE supervisor_inbox SET state='delivered', notify_turn_id=?, updated_at=? WHERE id=?")
            .bind(notify_turn_id)
            .bind(crate::db::now())
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn ack_inbox(pool: &SqlitePool, id: &str) -> Result<bool> {
    let res = sqlx::query("UPDATE supervisor_inbox SET state='handled', updated_at=? WHERE supervisor_id=? AND id=?")
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        migrate(&p).await.unwrap();
        p
    }

    #[tokio::test]
    async fn the_row_is_created_once_and_read_back_the_same() {
        let p = pool().await;
        let a = get_or_init(&p).await.unwrap();
        let b = get_or_init(&p).await.unwrap();
        assert_eq!(a.id, SUPERVISOR_ID);
        assert_eq!(a.created_at, b.created_at, "a second call must not replace the row");
        assert_eq!((a.identity.as_str(), a.effort.as_str(), a.active_model.as_str()), ("cc0", "low", "fable"));
    }

    /// The throttle's clock lives in the row, so a daemon restart does not buy the manager an
    /// extra wake-up.
    #[tokio::test]
    async fn the_last_wake_up_is_remembered_across_reads() {
        let p = pool().await;
        assert!(get_or_init(&p).await.unwrap().last_notify_at.is_none(), "never woken yet");
        set_last_notify(&p, "2026-09-12T10:00:00Z").await.unwrap();
        assert_eq!(get_or_init(&p).await.unwrap().last_notify_at.as_deref(), Some("2026-09-12T10:00:00Z"));
    }

    /// The whole point of the client_request_id: a retry after a crash is the same assignment.
    #[tokio::test]
    async fn one_client_request_id_is_one_assignment() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-1", "do the thing").await.unwrap();
        assert_eq!(a.status, "queued");
        assert!(insert_assignment(&p, None, "bot1", "req-1", "do the thing").await.is_err());
        assert_eq!(assignment_by_crid(&p, "req-1").await.unwrap().unwrap().id, a.id);
    }

    /// A replayed turn event — or the restart rescan seeing the same completion — must not
    /// wake the manager twice about one result.
    #[tokio::test]
    async fn the_same_event_key_only_lands_once() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let first = push_inbox(&p, "turn:t1:completed", "assignment_completed", None, None, None, &json!({})).await.unwrap();
        let again = push_inbox(&p, "turn:t1:completed", "assignment_completed", None, None, None, &json!({})).await.unwrap();
        assert!(first.is_some());
        assert!(again.is_none(), "a duplicate event is dropped, not queued again");
        assert_eq!(pending_inbox(&p).await.unwrap().len(), 1);
    }

    /// Delivered ≠ handled: only an ack closes an event, so a notify the manager never
    /// managed to answer stays owed to it.
    #[tokio::test]
    async fn delivery_does_not_close_an_event_but_an_ack_does() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let id = push_inbox(&p, "k1", "assignment_completed", None, None, None, &json!({})).await.unwrap().unwrap();
        mark_delivered_inbox(&p, &[id.clone()], "turn-9").await.unwrap();
        assert!(pending_inbox(&p).await.unwrap().is_empty(), "delivered events are not offered again");
        assert_eq!(pending_count(&p).await.unwrap(), 1, "but they are still outstanding");
        assert!(ack_inbox(&p, &id).await.unwrap());
        assert_eq!(pending_count(&p).await.unwrap(), 0);
        assert!(!ack_inbox(&p, "nope").await.unwrap());
    }

    #[tokio::test]
    async fn pending_counts_open_work_as_well_as_unhandled_news() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-1", "x").await.unwrap();
        assert_eq!(pending_count(&p).await.unwrap(), 1);
        mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
        assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "delivered");
        assert_eq!(pending_count(&p).await.unwrap(), 1, "delivered work is still open work");
        finish(&p, &a.id, "completed", Some("done"), None).await.unwrap();
        assert_eq!(pending_count(&p).await.unwrap(), 0);
        assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().result.as_deref(), Some("done"));
    }

    /// `unknown` delivery is its own state precisely so the controller reconciles it instead
    /// of sending the same job a second time.
    #[tokio::test]
    async fn unknown_delivery_is_not_folded_into_delivered() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-2", "x").await.unwrap();
        mark_delivered(&p, &a.id, "t2", "unknown").await.unwrap();
        let a = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(a.status, "unknown");
        assert!(a.is_open());
        assert_eq!(assignment_by_turn(&p, "t2").await.unwrap().unwrap().id, a.id);
    }

    /// A phone message that reaches the daemon twice is one request, not two.
    #[tokio::test]
    async fn a_replayed_source_turn_is_the_same_request() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = upsert_request(&p, "manager_turn", Some("turn-1"), "幫我處理登入").await.unwrap();
        let b = upsert_request(&p, "manager_turn", Some("turn-1"), "幫我處理登入").await.unwrap();
        assert_eq!(a, b);
        // Without a stable id there is nothing to dedupe on, and two asks stay two asks.
        let c = upsert_request(&p, "assignment_text_fallback", None, "同樣的字").await.unwrap();
        let d = upsert_request(&p, "assignment_text_fallback", None, "同樣的字").await.unwrap();
        assert_ne!(c, d);
    }

    /// A model switch retires the old controller, and only a *successful* switch moves
    /// `active_model`.
    #[tokio::test]
    async fn switching_bumps_the_generation_and_spends_the_budget() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let gen = set_active_model(&p, "opus", Some("2099-01-01T00:00:00Z")).await.unwrap();
        assert_eq!(gen, 1);
        let s = get_or_init(&p).await.unwrap();
        assert_eq!(s.active_model, "opus");
        assert_eq!(s.fallback_tries, 1);
        clear_fallback_budget(&p).await.unwrap();
        let s = get_or_init(&p).await.unwrap();
        assert_eq!(s.fallback_tries, 0);
        assert!(s.cooldown_until.is_none());
        assert_eq!(s.generation, 1, "clearing the budget is not a switch");
    }

    /// The 464-event night: the manager acks in the order shown, so the list has to be the
    /// open ones, oldest first — not the newest 200 of everything.
    #[tokio::test]
    async fn the_open_inbox_is_oldest_first_and_drains_with_acks() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        for i in 0..5 {
            let id = push_inbox(&p, &format!("k{i}"), "health_changed", None, None, None, &json!({"i": i}))
                .await
                .unwrap()
                .unwrap();
            // Distinct timestamps: `now()` has millisecond resolution and the loop is faster.
            sqlx::query("UPDATE supervisor_inbox SET created_at=? WHERE id=?")
                .bind(format!("2026-09-11T00:00:0{i}Z"))
                .bind(&id)
                .execute(&p)
                .await
                .unwrap();
        }
        let open = open_inbox(&p, 10).await.unwrap();
        assert_eq!(open.iter().map(|e| e.event_key.as_str()).collect::<Vec<_>>(), ["k0", "k1", "k2", "k3", "k4"]);
        assert_eq!(open_inbox(&p, 2).await.unwrap().len(), 2, "--limit applies");
        assert!(ack_inbox(&p, &open[0].id).await.unwrap());
        mark_delivered_inbox(&p, &[open[1].id.clone()], "t1").await.unwrap();
        let open = open_inbox(&p, 10).await.unwrap();
        assert_eq!(open.iter().map(|e| e.event_key.as_str()).collect::<Vec<_>>(), ["k1", "k2", "k3", "k4"], "delivered is still open; handled is gone");
        assert_eq!(open_inbox_count(&p).await.unwrap(), 4);
        assert_eq!(open_assignment_count(&p).await.unwrap(), 0);
        assert_eq!(pending_count(&p).await.unwrap(), 4);
    }

    /// A manager started before the watchdog existed is still wanted; one that was only set
    /// up (generation 0) is not — the watchdog must not be the first thing that starts it.
    #[tokio::test]
    async fn migration_backfills_desired_running_from_a_previous_start() {
        let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        // The pre-watchdog schema: the same DDL minus the three columns.
        for stmt in DDL.split(";\n") {
            let s = stmt.trim();
            if !s.is_empty() {
                sqlx::query(s).execute(&p).await.unwrap();
            }
        }
        for (id, gen, bot) in [("AGM", 2, Some("b1")), ("OLD", 0, Some("b2")), ("NONE", 3, None)] {
            sqlx::query("INSERT INTO supervisors (id, bot_id, generation, created_at, updated_at) VALUES (?,?,?,'t','t')")
                .bind(id)
                .bind(bot)
                .bind(gen)
                .execute(&p)
                .await
                .unwrap();
        }
        assert!(!has_column(&p, "supervisors", "desired_running").await.unwrap());
        migrate(&p).await.unwrap();
        migrate(&p).await.unwrap(); // idempotent
        let wanted: Vec<(String, i64)> = sqlx::query_as("SELECT id, desired_running FROM supervisors ORDER BY id")
            .fetch_all(&p)
            .await
            .unwrap();
        assert_eq!(wanted, [("AGM".to_string(), 1), ("NONE".to_string(), 0), ("OLD".to_string(), 0)]);
        let s = get_or_init(&p).await.unwrap();
        assert!(s.wants_running());
        set_desired_running(&p, false).await.unwrap();
        assert!(!get_or_init(&p).await.unwrap().wants_running());
        set_watchdog(&p, 3, Some("2026-09-11T00:00:00Z")).await.unwrap();
        set_desired_running(&p, true).await.unwrap();
        let s = get_or_init(&p).await.unwrap();
        assert_eq!((s.watchdog_attempts, s.watchdog_next_at.as_deref()), (0, None), "a fresh start clears the streak");
        set_status_detail(&p, Some("x")).await.unwrap();
        let s = get_or_init(&p).await.unwrap();
        assert_eq!((s.status.as_str(), s.status_detail.as_deref()), ("", Some("x")), "detail alone leaves status");
    }

    #[tokio::test]
    async fn every_summary_is_kept_as_a_versioned_note() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        assert_eq!(set_summary(&p, "第一版").await.unwrap(), 1);
        assert_eq!(set_summary(&p, "第二版").await.unwrap(), 2);
        assert_eq!(get_or_init(&p).await.unwrap().summary.as_deref(), Some("第二版"));
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_notes WHERE kind='handoff'")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(notes, 2, "the earlier summary is still recoverable");
    }
}
