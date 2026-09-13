//! SQLite storage (SPEC appendix C).

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::collections::BTreeSet;
use std::path::Path;
use std::str::FromStr;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY, path TEXT NOT NULL, label TEXT NOT NULL,
  host TEXT NOT NULL DEFAULT 'local',
  workspace_id TEXT,
  deleted_at TEXT, created_at TEXT NOT NULL,
  -- 同 `bots.position`：側欄順序，來自 config.toml 的陣列位置。
  position INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS projects_host_path_live ON projects(host, path) WHERE deleted_at IS NULL;
CREATE TABLE IF NOT EXISTS bots (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
  name TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('claude','codex','grok')),
  model TEXT,
  effort TEXT,
  fast INTEGER NOT NULL DEFAULT 0,
  persona TEXT,
  args_json TEXT NOT NULL DEFAULT '[]', autostart INTEGER NOT NULL DEFAULT 0,
  inject_hooks INTEGER NOT NULL DEFAULT 1,
  auto_approve INTEGER NOT NULL DEFAULT 1,
  identity TEXT,
  env_json TEXT NOT NULL DEFAULT '{}',
  managed_by TEXT NOT NULL DEFAULT 'user',
  cwd TEXT,
  herdr_session TEXT,
  -- A herdr agent the bot itself spawned (named `<parent agent name>-<suffix>`), adopted by
  -- the reconcile and shown under its parent. NULL = a top-level bot.
  parent_bot_id TEXT,
  hook_token TEXT NOT NULL, deleted_at TEXT, created_at TEXT NOT NULL,
  -- 使用者把這顆標成「主要執行的 bot」：純顯示用的釘選（UI 標題列下面那一列會把它們排在
  -- 最前面），跟 run 無關，所以不進 config.toml 的投影，改了也不用重啟。存在 daemon 而不是
  -- 瀏覽器：使用者在手機與電腦上追的是同一組 bot。
  is_primary INTEGER NOT NULL DEFAULT 0,
  -- 側欄順序 = config.toml 陣列裡的位置（`POST /api/order` 寫回去，投影時填這裡）。
  -- 沒有它的話清單只能照 created_at 排，排序就只能存在瀏覽器，每台裝置各自一份。
  position INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS bots_name_project_live ON bots(project_id, name) WHERE deleted_at IS NULL;
CREATE TABLE IF NOT EXISTS runs (
  id TEXT PRIMARY KEY, bot_id TEXT NOT NULL REFERENCES bots(id),
  state TEXT NOT NULL CHECK (state IN ('starting','running','stopping','stopped','exited')),
  agent_status TEXT NOT NULL DEFAULT 'unknown' CHECK (agent_status IN ('idle','working','blocked','unknown')),
  workspace_id TEXT, pane_id TEXT, tab_id TEXT, adopted INTEGER NOT NULL DEFAULT 0,
  agent_name TEXT, herdr_session TEXT,
  native_session_id TEXT, transcript_path TEXT,
  last_read_revision INTEGER, last_read_tail_hash TEXT,
  started_at TEXT NOT NULL, ended_at TEXT,
  -- SPEC §4.4a: what this run is *actually* on — stamped from the argv it was started with,
  -- and updated when a slash command changes it live. NULL = the daemon did not start it.
  runtime_model TEXT, runtime_effort TEXT, runtime_fast INTEGER,
  -- Native session requested by a `resume_native` start. Cleared by the first identity/turn hook.
  resume_session_id TEXT,
  -- The agent's terminal title, its statusLine output / payload, a pending claude update and
  -- the error that cut the last turn short. All belong to this CLI process, so a restart
  -- starts from NULL.
  agent_title TEXT, status_line TEXT, status_json TEXT, update_notice TEXT, turn_error TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS runs_one_active ON runs(bot_id) WHERE state IN ('starting','running','stopping');
CREATE INDEX IF NOT EXISTS runs_pane ON runs(pane_id);
CREATE TABLE IF NOT EXISTS conversations (
  id TEXT PRIMARY KEY, bot_id TEXT NOT NULL UNIQUE REFERENCES bots(id), created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS turns (
  id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id),
  run_id TEXT REFERENCES runs(id),
  origin TEXT NOT NULL CHECK (origin IN ('web','external')),
  status TEXT NOT NULL CHECK (status IN ('queued','in_flight','completed','completed_fallback','failed')),
  delivery TEXT NOT NULL DEFAULT 'pending' CHECK (delivery IN ('pending','ok','unknown','failed')),
  client_request_id TEXT,
  native_session_id TEXT, native_turn_id TEXT,
  created_at TEXT NOT NULL, completed_at TEXT,
  -- The exact text to hand to the CLI when a queued web prompt is activated. NULL for
  -- external turns; the user-facing message keeps the original text.
  prompt_text TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS turns_one_in_flight ON turns(run_id) WHERE status = 'in_flight';
CREATE UNIQUE INDEX IF NOT EXISTS turns_one_queued ON turns(conversation_id) WHERE status = 'queued';
CREATE UNIQUE INDEX IF NOT EXISTS turns_client_req ON turns(conversation_id, client_request_id) WHERE client_request_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS turns_native ON turns(native_session_id, native_turn_id) WHERE native_turn_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS turns_conv_time ON turns(conversation_id, created_at);
CREATE TABLE IF NOT EXISTS messages (
  id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id),
  turn_id TEXT REFERENCES turns(id),
  role TEXT NOT NULL CHECK (role IN ('user','assistant','system')),
  content TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('web','hook','transcript','terminal_fallback','system')),
  incomplete INTEGER NOT NULL DEFAULT 0, terminal_snapshot TEXT,
  group_id TEXT,
  attachments_json TEXT,
  relay_from TEXT,
  created_at TEXT NOT NULL, updated_at TEXT
);
CREATE INDEX IF NOT EXISTS messages_conv_time ON messages(conversation_id, created_at);
CREATE INDEX IF NOT EXISTS messages_turn ON messages(turn_id);
CREATE INDEX IF NOT EXISTS messages_group ON messages(group_id) WHERE group_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS attachments (
  id TEXT PRIMARY KEY, bot_id TEXT NOT NULL REFERENCES bots(id),
  name TEXT NOT NULL, mime TEXT NOT NULL, size INTEGER NOT NULL,
  -- Where the daemon can read the bytes back (UI thumbnails).
  local_path TEXT NOT NULL,
  -- Absolute path on the bot's host; this is what the agent is told to read.
  agent_path TEXT NOT NULL, host TEXT NOT NULL,
  message_id TEXT REFERENCES messages(id),
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS attachments_msg ON attachments(message_id);
"#;

pub async fn open(path: &Path) -> Result<SqlitePool> {
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(10));
    // Migrations run on a throw-away single-connection pool, so no pooled connection keeps a
    // table layout from before a migration changed it.
    {
        let mpool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts.clone())
            .await
            .with_context(|| format!("open sqlite {}", path.display()))?;
        migrate(&mpool).await?;
        mpool.close().await;
    }
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("open sqlite {}", path.display()))?;
    Ok(pool)
}

/// Apply the schema (`CREATE … IF NOT EXISTS`, so an existing file is left as it is), then the
/// supervisor and mission stores' own migrations. Runs on one connection.
///
/// Only the current schema is supported: a database from before these columns existed is not
/// upgraded. Tables of removed features (`teams`, `team_*`) and their columns may still exist
/// in an old file; nothing reads them.
async fn migrate(pool: &SqlitePool) -> Result<()> {
    for stmt in SCHEMA.split(";\n") {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        sqlx::query(s).execute(pool).await.with_context(|| format!("apply schema: {s}"))?;
    }
    // AGM 總管的持久資料（supervisor/store.rs）。
    crate::supervisor::store::migrate(pool).await?;
    // 群組任務（mission/store.rs）。
    crate::mission::store::migrate(pool).await?;
    Ok(())
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn ulid() -> String {
    ulid::Ulid::new().to_string()
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct Project {
    pub id: String,
    pub path: String,
    pub label: String,
    /// `"local"` or a configured host name (SPEC §11.2).
    pub host: String,
    pub workspace_id: Option<String>,
    pub deleted_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct Bot {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub kind: String,
    /// claude `--model <m>` / codex `-m <m>` / grok `-m <m>`; NULL = the CLI's own default.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// v4.0: codex Fast service tier (`-c service_tier="priority"`).
    pub fast: i64,
    /// v4.0: text appended to the agent's system prompt.
    pub persona: Option<String>,
    pub args_json: String,
    pub autostart: i64,
    pub inject_hooks: i64,
    pub auto_approve: i64,
    pub identity: Option<String>,
    pub env_json: String,
    /// `user` (from config.toml) or `child` (an agent another bot spawned; never in the TOML).
    pub managed_by: String,
    /// Working directory for the pane. NULL = the project's path.
    pub cwd: Option<String>,
    /// Herdr session override. `Some("default")` identifies an imported user-session bot.
    pub herdr_session: Option<String>,
    /// The bot whose agent spawned this one (`<parent agent name>-<suffix>`); None = top-level.
    pub parent_bot_id: Option<String>,
    /// 使用者釘選的「主要執行的 bot」（見 SCHEMA 的欄位註解）。0 = 一般。
    pub is_primary: i64,
    #[serde(skip_serializing)]
    pub hook_token: String,
    pub deleted_at: Option<String>,
    pub created_at: String,
}

impl Bot {
    pub fn args(&self) -> Vec<String> {
        serde_json::from_str(&self.args_json).unwrap_or_default()
    }
    pub fn env(&self) -> std::collections::BTreeMap<String, String> {
        serde_json::from_str(&self.env_json).unwrap_or_default()
    }
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct Run {
    pub id: String,
    pub bot_id: String,
    pub state: String,
    pub agent_status: String,
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
    /// The herdr tab the run's pane sits in — from `tab.create` at start, from `pane.move`
    /// when the user gives a running bot its own tab, or refreshed from herdr on reconcile.
    /// `None` on runs from before the column existed, which were split into a shared tab and
    /// have not been reconciled since. Tearing the run down never closes a tab on the
    /// strength of this field alone: the tab is only closed if it is left with no panes.
    pub tab_id: Option<String>,
    pub adopted: i64,
    /// The herdr agent name this run was started (or adopted) under. `None` on rows from
    /// before the column existed; `run_target` falls back to the bot's bare name then.
    pub agent_name: Option<String>,
    /// Effective Herdr session for this run. NULL on older rows means the project's session.
    pub herdr_session: Option<String>,
    /// The agent's self-chosen label — herdr's `terminal_title_stripped`, which for Claude
    /// Code is its running summary of the current task. NULL until one is seen.
    pub agent_title: Option<String>,
    /// What the pane's status bar reads right now — the user's own claude statusLine
    /// command's output, relayed by `statusline_cmd`. NULL for kinds/bots without one.
    pub status_line: Option<String>,
    /// The statusLine payload verbatim (minus the transcript path), for the richer web
    /// status bar: context window, full model name, cost, rate limits.
    pub status_json: Option<String>,
    /// The pending-update notice claude prints on its bottom line once it has downloaded a
    /// new version ("Update installed · Restart to update"). NULL when there is none on
    /// screen; [`crate::update_watch`] keeps it in step.
    pub update_notice: Option<String>,
    /// SPEC §4.4a: the model this run's CLI is actually on — read back from the argv it was
    /// started with (the inverse of `lifecycle::model_args`), and rewritten when a slash
    /// command applies a change live. `None` on an adopted run, whose argv we never built,
    /// and on rows written before the column existed: unknown, which the UI shows as nothing.
    pub runtime_model: Option<String>,
    /// The reasoning effort this run is actually on. See [`Run::runtime_model`].
    pub runtime_effort: Option<String>,
    /// Whether this run is actually on the fast/priority tier. See [`Run::runtime_model`].
    pub runtime_fast: Option<i64>,
    /// The `API Error: …` line the pane showed when this run's last turn ended — the turn was
    /// cut short by the API, not finished. NULL when the last turn ended cleanly; cleared the
    /// moment the next turn opens ([`crate::turn_error`]).
    pub turn_error: Option<String>,
    pub native_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub last_read_revision: Option<i64>,
    pub last_read_tail_hash: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// The native session id a reopen asked the CLI to continue. It is consumed by the first
    /// identity hook (Claude) or first completed turn hook (Codex/Grok).
    pub resume_session_id: Option<String>,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct Turn {
    pub id: String,
    pub conversation_id: String,
    pub run_id: Option<String>,
    pub origin: String,
    pub status: String,
    pub delivery: String,
    pub client_request_id: Option<String>,
    pub native_session_id: Option<String>,
    pub native_turn_id: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
    /// Exact prompt payload for a queued web turn; never exposed in REST/WS turn JSON.
    #[serde(skip_serializing)]
    pub prompt_text: Option<String>,
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub turn_id: Option<String>,
    pub role: String,
    pub content: String,
    pub source: String,
    pub incomplete: i64,
    pub terminal_snapshot: Option<String>,
    /// SPEC §13: set on every message produced by one `POST /projects/:id/chat` send
    /// (the per-bot user copies and the "skipped" system notes); NULL otherwise.
    pub group_id: Option<String>,
    /// JSON array of the images sent with this message (`attach.rs`); NULL when there are none.
    pub attachments_json: Option<String>,
    /// The bot (or `daemon`) that relayed this message; NULL = the user typed it.
    pub relay_from: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
}

/// A message row joined with the bot it belongs to — the unit of the project group timeline.
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct GroupMessage {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub message: Message,
    pub bot_id: String,
    pub bot_name: String,
}

pub const ACTIVE_STATES: &str = "('starting','running','stopping')";

pub async fn bot(pool: &SqlitePool, id: &str) -> Result<Option<Bot>> {
    Ok(sqlx::query_as::<_, Bot>("SELECT * FROM bots WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

pub async fn live_bots(pool: &SqlitePool) -> Result<Vec<Bot>> {
    Ok(sqlx::query_as::<_, Bot>("SELECT * FROM bots WHERE deleted_at IS NULL ORDER BY position, created_at")
        .fetch_all(pool)
        .await?)
}

pub async fn live_projects(pool: &SqlitePool) -> Result<Vec<Project>> {
    Ok(sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE deleted_at IS NULL ORDER BY position, created_at")
        .fetch_all(pool)
        .await?)
}

pub async fn project(pool: &SqlitePool, id: &str) -> Result<Option<Project>> {
    Ok(sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

pub async fn active_run(pool: &SqlitePool, bot_id: &str) -> Result<Option<Run>> {
    Ok(sqlx::query_as::<_, Run>(
        "SELECT * FROM runs WHERE bot_id = ? AND state IN ('starting','running','stopping') LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn run(pool: &SqlitePool, id: &str) -> Result<Option<Run>> {
    Ok(sqlx::query_as::<_, Run>("SELECT * FROM runs WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

/// The most recent native session from an ended run.
pub async fn last_native_session_id(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>> {
    Ok(last_native_session(pool, bot_id).await?.map(|(id, _)| id))
}

/// The last ended run's native session id and, when the provider's hook reported one, the
/// transcript file it lives in — so a restart can tell a session it can resume from one that
/// was never written.
pub async fn last_native_session(pool: &SqlitePool, bot_id: &str) -> Result<Option<(String, Option<String>)>> {
    Ok(sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT native_session_id, transcript_path FROM runs
          WHERE bot_id = ? AND ended_at IS NOT NULL AND native_session_id IS NOT NULL
          ORDER BY started_at DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn all_active_runs(pool: &SqlitePool) -> Result<Vec<Run>> {
    Ok(sqlx::query_as::<_, Run>("SELECT * FROM runs WHERE state IN ('starting','running','stopping')")
        .fetch_all(pool)
        .await?)
}

pub async fn conversation_id(pool: &SqlitePool, bot_id: &str) -> Result<String> {
    if let Some(row) =
        sqlx::query_scalar::<_, String>("SELECT id FROM conversations WHERE bot_id = ?").bind(bot_id).fetch_optional(pool).await?
    {
        return Ok(row);
    }
    let id = ulid();
    sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?) ON CONFLICT(bot_id) DO NOTHING")
        .bind(&id)
        .bind(bot_id)
        .bind(now())
        .execute(pool)
        .await?;
    Ok(sqlx::query_scalar::<_, String>("SELECT id FROM conversations WHERE bot_id = ?")
        .bind(bot_id)
        .fetch_one(pool)
        .await?)
}

/// Bots on one host (join through their project), in creation order.
pub async fn live_bots_on_host(pool: &SqlitePool, host: &str) -> Result<Vec<Bot>> {
    Ok(sqlx::query_as::<_, Bot>(
        "SELECT b.* FROM bots b JOIN projects p ON p.id = b.project_id
         WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.host = ? ORDER BY b.created_at",
    )
    .bind(host)
    .fetch_all(pool)
    .await?)
}

/// The host a bot lives on. Missing rows fall back to `local`.
pub async fn bot_host(pool: &SqlitePool, bot_id: &str) -> Result<String> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT p.host FROM bots b JOIN projects p ON p.id = b.project_id WHERE b.id = ?",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or_else(|| crate::config::LOCAL_HOST.to_string()))
}

/// Every identity that has a live bot run on `host` right now.
///
/// A bot chatting under `cc1` on that host is proof the account *is* logged in there, whatever
/// the last `claude auth status` answered — see [`crate::quota_claude`]. Unlike the in-memory
/// statusLine trace this survives a daemon restart, because it is just the run table.
pub async fn live_identities_on_host(pool: &SqlitePool, host: &str) -> Result<BTreeSet<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT b.identity FROM runs r
           JOIN bots b ON b.id = r.bot_id
           JOIN projects p ON p.id = b.project_id
          WHERE r.state IN ('starting','running','stopping')
            AND p.host = ? AND b.deleted_at IS NULL
            AND b.identity IS NOT NULL AND b.identity <> ''",
    )
    .bind(host)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Active runs sitting on `pane_id` in a host/session. `fallback_session` is used for old rows
/// whose effective session was not stored yet.
pub async fn active_runs_for_pane(
    pool: &SqlitePool,
    host: &str,
    pane_id: &str,
    session: &str,
    fallback_session: &str,
) -> Result<Vec<Run>> {
    Ok(sqlx::query_as::<_, Run>(
        "SELECT r.* FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
         WHERE r.pane_id = ? AND p.host = ? AND r.state IN ('starting','running','stopping')
           AND COALESCE(r.herdr_session, ?) = ?",
    )
    .bind(pane_id)
    .bind(host)
    .bind(fallback_session)
    .bind(session)
    .fetch_all(pool)
    .await?)
}

/// The herdr agent name a *new* run of this bot should use: `<project label slug>-<bot>`.
pub async fn agent_name_for_bot(pool: &SqlitePool, bot: &Bot) -> Result<String> {
    let label: Option<String> = sqlx::query_scalar("SELECT label FROM projects WHERE id = ?")
        .bind(&bot.project_id)
        .fetch_optional(pool)
        .await?;
    Ok(crate::config::agent_name(label.as_deref().unwrap_or(""), &bot.id))
}

/// The herdr target to address an *existing* run with. Runs record the name they were
/// started under, so a rename of the project label keeps working.
pub fn run_target(run: &Run, bot: &Bot) -> String {
    run.agent_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| bot.name.clone())
}

/// The user messages already stored on a turn — used to keep the Stop hook from re-adding a
/// prompt that was already scraped off the pane's prompt echo.
pub async fn turn_user_messages(pool: &SqlitePool, turn_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'user' ORDER BY created_at")
        .bind(turn_id)
        .fetch_all(pool)
        .await?)
}

/// The same rows, each with its `attachments_json`. The timeline stores what the user
/// *typed*; what the agent was actually handed also carries the attachment paths
/// (`attach::deliver_text`), and only that fuller text matches the echo on the pane.
pub async fn turn_user_messages_with_attachments(
    pool: &SqlitePool,
    turn_id: &str,
) -> Result<Vec<(String, Option<String>)>> {
    Ok(sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT content, attachments_json FROM messages WHERE turn_id = ? AND role = 'user' ORDER BY created_at",
    )
    .bind(turn_id)
    .fetch_all(pool)
    .await?)
}

pub async fn in_flight_turn(pool: &SqlitePool, run_id: &str) -> Result<Option<Turn>> {
    Ok(sqlx::query_as::<_, Turn>("SELECT * FROM turns WHERE run_id = ? AND status = 'in_flight'")
        .bind(run_id)
        .fetch_optional(pool)
        .await?)
}

/// The one daemon-owned prompt waiting behind the current turn, if any.
pub async fn queued_turn(pool: &SqlitePool, conversation_id: &str) -> Result<Option<Turn>> {
    Ok(sqlx::query_as::<_, Turn>(
        "SELECT * FROM turns WHERE conversation_id = ? AND status = 'queued' ORDER BY created_at, id LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?)
}

/// Same lookup by bot id, used by the state snapshot without making callers know conversation ids.
pub async fn queued_turn_for_bot(pool: &SqlitePool, bot_id: &str) -> Result<Option<Turn>> {
    Ok(sqlx::query_as::<_, Turn>(
        "SELECT t.* FROM turns t JOIN conversations c ON c.id = t.conversation_id
         WHERE c.bot_id = ? AND t.status = 'queued' ORDER BY t.created_at, t.id LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("am-db-test-{}", ulid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn columns(pool: &SqlitePool, table: &str) -> Vec<String> {
        sqlx::query_scalar::<_, String>(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// Running `open` twice must be a no-op the second time (every migration is guarded).
    #[tokio::test]
    async fn open_is_idempotent() {
        let dir = tmp_dir();
        let file = dir.join("new.sqlite3");
        let p1 = open(&file).await.unwrap();
        let before = columns(&p1, "bots").await;
        p1.close().await;
        let p2 = open(&file).await.unwrap();
        assert_eq!(columns(&p2, "bots").await, before);
        p2.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conversation_id_is_race_safe() {
        let dir = tmp_dir();
        let pool = open(&dir.join("conversation-race.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','bot','claude','tok',?)")
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();

        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let pool = pool.clone();
            calls.spawn(async move { conversation_id(&pool, "b1").await });
        }

        let mut ids = Vec::new();
        while let Some(result) = calls.join_next().await {
            ids.push(result.unwrap().unwrap());
        }
        assert_eq!(ids.len(), 20);
        assert!(ids.iter().all(|id| id == &ids[0]));

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE bot_id = 'b1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reopen only considers a native session from an ended run. An active run, and an ended
    /// run whose provider never reported an id, must not steal the continuation slot.
    #[tokio::test]
    async fn last_native_session_id_uses_the_latest_ended_run() {
        let dir = tmp_dir();
        let pool = open(&dir.join("sessions.sqlite3")).await.unwrap();
        let at = now();
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(&at)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','pm','claude','tok',?)")
            .bind(&at)
            .execute(&pool)
            .await
            .unwrap();
        for (id, state, native, started, ended) in [
            ("r-old", "stopped", Some("native-old"), "2026-09-07T00:00:00Z", Some("2026-09-07T00:01:00Z")),
            ("r-active", "running", Some("native-active"), "2026-09-07T02:00:00Z", None),
            ("r-new", "exited", Some("native-new"), "2026-09-07T03:00:00Z", Some("2026-09-07T03:01:00Z")),
            ("r-no-id", "stopped", None, "2026-09-07T04:00:00Z", Some("2026-09-07T04:01:00Z")),
        ] {
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
                 VALUES (?,?, 'stopped', 'unknown', ?, ?, ?)",
            )
            .bind(id)
            .bind("b1")
            .bind(native)
            .bind(started)
            .bind(ended)
            .execute(&pool)
            .await
            .unwrap();
            if state != "stopped" {
                sqlx::query("UPDATE runs SET state=? WHERE id=?").bind(state).bind(id).execute(&pool).await.unwrap();
            }
        }
        assert_eq!(last_native_session_id(&pool, "b1").await.unwrap().as_deref(), Some("native-new"));
        assert_eq!(last_native_session_id(&pool, "missing").await.unwrap(), None);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The daemon-restart-proof half of the claude quota gate: an identity with a live run on a
    /// host counts as logged in there, whatever the last `auth status` said (see
    /// [`crate::quota_claude::should_probe_identity`]).
    #[tokio::test]
    async fn live_identities_are_per_host_and_only_count_active_runs() {
        let dir = tmp_dir();
        let pool = open(&dir.join("live.sqlite3")).await.unwrap();
        for (id, host) in [("pl", "local"), ("pm", "m4p")] {
            sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)")
                .bind(id).bind(format!("/tmp/{id}")).bind(id).bind(host).bind(now())
                .execute(&pool).await.unwrap();
        }
        // (bot, project, identity, run state)
        let bots = [
            ("b1", "pm", "cc1", "running"),
            ("b2", "pm", "cc2", "stopped"),   // ended — proves nothing
            ("b3", "pl", "cc3", "running"),   // another host
            ("b4", "pm", "", "running"),      // no identity (default account)
            ("b5", "pm", "cc4", "starting"),  // still counts
        ];
        for (b, p, ident, state) in bots {
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, hook_token, identity, created_at) VALUES (?,?,?,'claude','tok',?,?)",
            )
            .bind(b).bind(p).bind(b).bind(ident).bind(now())
            .execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO runs (id, bot_id, state, started_at) VALUES (?,?,?,?)")
                .bind(format!("r{b}")).bind(b).bind(state).bind(now())
                .execute(&pool).await.unwrap();
        }
        let live = live_identities_on_host(&pool, "m4p").await.unwrap();
        assert_eq!(live, ["cc1".to_string(), "cc4".to_string()].into_iter().collect());
        assert_eq!(live_identities_on_host(&pool, "local").await.unwrap(), ["cc3".to_string()].into_iter().collect());

        // A deleted bot stops vouching for its account.
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = 'b1'").bind(now()).execute(&pool).await.unwrap();
        assert_eq!(live_identities_on_host(&pool, "m4p").await.unwrap(), ["cc4".to_string()].into_iter().collect());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

