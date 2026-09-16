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
    // Throw-away single-connection pool: no pooled connection keeps a pre-migration table layout.
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

/// Only the current schema is supported: older databases are not upgraded. Leftover tables of
/// removed features (`teams`, `team_*`) may exist; nothing reads them.
async fn migrate(pool: &SqlitePool) -> Result<()> {
    for stmt in SCHEMA.split(";\n") {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        sqlx::query(s).execute(pool).await.with_context(|| format!("apply schema: {s}"))?;
    }
    // Additive columns for databases created before they existed.
    for (table, col, ddl) in [
        // 2026-09-14: daemon 對這個 pane 直接打過字（當場套用 slash、codex 選單、/login）。之後這個
        // run 的 prompt 一律改走「打字進 pane 再看畫面」，因為 herdr `agent.prompt` 在這種 pane 上
        // 回 ok 卻沒送進去（wits-c1-op-xh 14:24、15:33）。存在 DB：daemon 重啟後不能忘記，否則第一則
        // 又走回已知會失效的那條路。
        ("runs", "pane_typed", "ALTER TABLE runs ADD COLUMN pane_typed INTEGER NOT NULL DEFAULT 0"),
        // 這個 turn 已經被 watchdog 重送過幾次。存在 DB 才擋得住「重啟後又重送同一則」。
        ("turns", "resend_count", "ALTER TABLE turns ADD COLUMN resend_count INTEGER NOT NULL DEFAULT 0"),
        // 0 = the prompt was typed and submitted on a run with no lossless evidence (grok, remote
        // hosts, codex before its session is known). `delivery` stays 'ok' — its CHECK constraint
        // predates this state and cannot be widened without rebuilding the table — and this column
        // carries the "check it by hand" mark to the API, the UI and the supervisor.
        ("turns", "delivery_verified", "ALTER TABLE turns ADD COLUMN delivery_verified INTEGER NOT NULL DEFAULT 1"),
        // A queued prompt that could not be typed yet (busy box, transcript not reported): how many
        // times it has been put back, and not before when it is tried again. Persisted so the
        // backoff and its limit survive a restart and cannot be reset by extra wake-ups.
        ("turns", "flush_retries", "ALTER TABLE turns ADD COLUMN flush_retries INTEGER NOT NULL DEFAULT 0"),
        ("turns", "next_flush_at", "ALTER TABLE turns ADD COLUMN next_flush_at TEXT"),
        // Put-backs spent waiting for a codex rollout, counted only for that reason and only for
        // the run and session in `rollout_wait_key` (`<run id>:<session id>`); a different key
        // starts the count again (sol review round ten #2).
        ("turns", "rollout_waits", "ALTER TABLE turns ADD COLUMN rollout_waits INTEGER NOT NULL DEFAULT 0"),
        ("turns", "rollout_wait_key", "ALTER TABLE turns ADD COLUMN rollout_wait_key TEXT"),
    ] {
        if !has_column(pool, table, col).await? {
            sqlx::query(ddl).execute(pool).await.with_context(|| format!("add {table}.{col}"))?;
        }
    }
    crate::supervisor::store::migrate(pool).await?;
    crate::read_marks::migrate(pool).await?;
    crate::mission::store::migrate(pool).await?;
    check_schema_drift(pool).await?;
    Ok(())
}

/// `SCHEMA` 的 `CREATE TABLE IF NOT EXISTS` 對**既有**資料庫是完全的 no-op，所以「欄位有哪些」其實
/// 記在兩個地方：宣告式的 SCHEMA，與上面那份手維護的 ALTER 名單。往 SCHEMA 加一欄卻忘了補 ALTER，
/// 在開發者自己的機器上一律是綠的（每個測試都開新 DB），到使用者那裡才會炸——而且是 `SELECT *` 的
/// `FromRow` 整個失敗，daemon 起不來，錯誤訊息是 sqlx 的 column-not-found（review 2026-09-16）。
///
/// 所以 migrate 的最後一步自己對一次帳：宣告了什麼欄位，DB 就要有什麼欄位。
async fn check_schema_drift(pool: &SqlitePool) -> Result<()> {
    for (table, declared) in declared_columns(SCHEMA) {
        let have: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as(&format!("PRAGMA table_info({table})")).fetch_all(pool).await?;
        if have.is_empty() {
            continue; // 這一版沒建出來（舊功能留下的宣告）：不是這個檢查要管的事
        }
        let missing: Vec<&str> =
            declared.iter().filter(|c| !have.iter().any(|h| h.1.eq_ignore_ascii_case(c))).map(|c| c.as_str()).collect();
        anyhow::ensure!(
            missing.is_empty(),
            "schema drift：`SCHEMA` 宣告了 {table}.{} 但這個資料庫沒有。CREATE TABLE IF NOT EXISTS 對既有 DB 不做事，\
             請在 db.rs 的 ALTER 名單補一條 `ALTER TABLE {table} ADD COLUMN …`（既有列要能留白）。",
            missing.join("、")
        );
    }
    Ok(())
}

/// 從 `CREATE TABLE IF NOT EXISTS <名字> ( … )` 抽出欄位名。只認每一行的第一個 token，
/// 約束子句（PRIMARY／FOREIGN／UNIQUE／CHECK／CONSTRAINT）與 `--` 註解跳過。
fn declared_columns(schema: &str) -> Vec<(String, Vec<String>)> {
    const HEAD: &str = "CREATE TABLE IF NOT EXISTS ";
    let mut out = Vec::new();
    for chunk in schema.split(HEAD).skip(1) {
        let Some(open) = chunk.find('(') else { continue };
        let table = chunk[..open].trim().trim_matches('"').to_string();
        let Some(close) = chunk.find("\n)") else { continue };
        let mut cols = Vec::new();
        for line in chunk[open + 1..close].lines() {
            let line = line.split("--").next().unwrap_or("").trim().trim_end_matches(',').trim();
            let Some(first) = line.split_whitespace().next() else { continue };
            let upper = first.to_ascii_uppercase();
            if ["PRIMARY", "FOREIGN", "UNIQUE", "CHECK", "CONSTRAINT"].contains(&upper.as_str()) {
                continue;
            }
            if first.is_empty() || first.starts_with('(') {
                continue;
            }
            cols.push(first.trim_matches('"').to_string());
        }
        if !cols.is_empty() {
            out.push((table, cols));
        }
    }
    out
}

pub async fn has_column(pool: &SqlitePool, table: &str, col: &str) -> Result<bool> {
    let cols: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as(&format!("PRAGMA table_info({table})")).fetch_all(pool).await?;
    Ok(cols.iter().any(|c| c.1 == col))
}

/// `runs.pane_typed`: has the daemon typed straight into this run's pane?
pub async fn set_pane_typed(pool: &SqlitePool, run_id: &str) -> Result<()> {
    sqlx::query("UPDATE runs SET pane_typed = 1 WHERE id = ?").bind(run_id).execute(pool).await?;
    Ok(())
}

/// `Err` is not `false`: the caller decides what an unreadable marker means, and for delivery it
/// means "assume this pane needs typing" (sol review 2026-09-14 #3).
pub async fn pane_typed(pool: &SqlitePool, run_id: &str) -> Result<bool> {
    let v: Option<i64> = sqlx::query_scalar("SELECT pane_typed FROM runs WHERE id = ?")
        .bind(run_id)
        .fetch_optional(pool)
        .await?;
    Ok(v.unwrap_or(0) != 0)
}

/// Claim one re-delivery for `turn_id`, at most `max` per turn. `true` = claimed (and counted);
/// the UPDATE is the lock, so a queue flush and the stall watchdog cannot both resend.
pub async fn claim_resend(pool: &SqlitePool, turn_id: &str, max: i64) -> bool {
    matches!(
        sqlx::query("UPDATE turns SET resend_count = resend_count + 1 WHERE id = ? AND resend_count < ?")
            .bind(turn_id)
            .bind(max)
            .execute(pool)
            .await,
        Ok(r) if r.rows_affected() > 0
    )
}

/// 退還一次重送額度。只有在**確定一個位元組都沒寫進 pane** 時才准叫（`Delivered::NotAttempted`
/// 的契約）：把「試過但被當下就消失的原因擋掉」算成「送過一次」，等於讓唯一一次補救機會白白蒸發
/// （review 2026-09-16）。
pub async fn refund_resend(pool: &SqlitePool, turn_id: &str) {
    let _ = sqlx::query("UPDATE turns SET resend_count = resend_count - 1 WHERE id = ? AND resend_count > 0")
        .bind(turn_id)
        .execute(pool)
        .await;
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
    /// NULL = the CLI's own default.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// codex Fast service tier (`-c service_tier="priority"`).
    pub fast: i64,
    /// Appended to the agent's system prompt.
    pub persona: Option<String>,
    pub args_json: String,
    pub autostart: i64,
    pub inject_hooks: i64,
    pub auto_approve: i64,
    pub identity: Option<String>,
    pub env_json: String,
    /// `user` (from config.toml) or `child` (an agent another bot spawned; never in the TOML).
    pub managed_by: String,
    /// NULL = the project's path.
    pub cwd: Option<String>,
    /// `Some("default")` identifies an imported user-session bot.
    pub herdr_session: Option<String>,
    /// None = top-level.
    pub parent_bot_id: Option<String>,
    /// 使用者釘選的「主要執行的 bot」（見 SCHEMA 欄位註解）。
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
    /// Tearing the run down never closes a tab on this field alone: only if it is left with no panes.
    pub tab_id: Option<String>,
    pub adopted: i64,
    /// Name the run was started under, so a project-label rename keeps working; `None` → bot name.
    pub agent_name: Option<String>,
    /// NULL on older rows = the project's session.
    pub herdr_session: Option<String>,
    /// herdr's `terminal_title_stripped` (Claude Code's running task summary).
    pub agent_title: Option<String>,
    /// The user's own claude statusLine output, relayed by `statusline_cmd`.
    pub status_line: Option<String>,
    /// statusLine payload verbatim (minus transcript path).
    pub status_json: Option<String>,
    /// Kept in step by [`crate::update_watch`].
    pub update_notice: Option<String>,
    /// SPEC §4.4a: parsed back from the start argv, updated by live slash commands. `None` on
    /// adopted runs (argv unknown).
    pub runtime_model: Option<String>,
    /// See [`Run::runtime_model`].
    pub runtime_effort: Option<String>,
    /// See [`Run::runtime_model`].
    pub runtime_fast: Option<i64>,
    /// `API Error: …` that cut the last turn short; cleared when the next turn opens ([`crate::turn_error`]).
    pub turn_error: Option<String>,
    pub native_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub last_read_revision: Option<i64>,
    pub last_read_tail_hash: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// Consumed by the first identity hook (Claude) or first completed turn hook (Codex/Grok).
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
    /// 0 = delivered without lossless evidence ("unverified"); see the migration note.
    #[sqlx(default)]
    pub delivery_verified: i64,
    /// Times a queued prompt was put back because it could not be typed yet.
    #[sqlx(default)]
    #[serde(skip_serializing)]
    pub flush_retries: i64,
    /// Not before this (RFC 3339) is that queued prompt tried again.
    #[sqlx(default)]
    #[serde(skip_serializing)]
    pub next_flush_at: Option<String>,
    #[sqlx(default)]
    #[serde(skip_serializing)]
    pub rollout_waits: i64,
    #[sqlx(default)]
    #[serde(skip_serializing)]
    pub rollout_wait_key: Option<String>,
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
    /// SPEC §13: shared by every message of one `POST /projects/:id/chat` send.
    pub group_id: Option<String>,
    /// See `attach.rs`.
    pub attachments_json: Option<String>,
    /// NULL = the user typed it.
    pub relay_from: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
}

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

pub async fn last_native_session_id(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>> {
    Ok(last_native_session(pool, bot_id).await?.map(|(id, _)| id))
}

/// Transcript path included so a restart can tell a resumable session from one never written.
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

pub async fn live_bots_on_host(pool: &SqlitePool, host: &str) -> Result<Vec<Bot>> {
    Ok(sqlx::query_as::<_, Bot>(
        "SELECT b.* FROM bots b JOIN projects p ON p.id = b.project_id
         WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.host = ? ORDER BY b.created_at",
    )
    .bind(host)
    .fetch_all(pool)
    .await?)
}

pub async fn bot_host(pool: &SqlitePool, bot_id: &str) -> Result<String> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT p.host FROM bots b JOIN projects p ON p.id = b.project_id WHERE b.id = ?",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or_else(|| crate::config::LOCAL_HOST.to_string()))
}

/// A live run under an identity proves that account is logged in on `host`, whatever
/// `claude auth status` said ([`crate::quota_claude`]); survives a daemon restart.
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

/// `fallback_session` covers old rows without a stored session.
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

/// For a *new* run; existing runs use [`run_target`].
pub async fn agent_name_for_bot(pool: &SqlitePool, bot: &Bot) -> Result<String> {
    let label: Option<String> = sqlx::query_scalar("SELECT label FROM projects WHERE id = ?")
        .bind(&bot.project_id)
        .fetch_optional(pool)
        .await?;
    Ok(crate::config::agent_name(label.as_deref().unwrap_or(""), &bot.id))
}

pub fn run_target(run: &Run, bot: &Bot) -> String {
    run.agent_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| bot.name.clone())
}

/// Keeps the Stop hook from re-adding a prompt already scraped off the pane's echo.
pub async fn turn_user_messages(pool: &SqlitePool, turn_id: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'user' ORDER BY created_at")
        .bind(turn_id)
        .fetch_all(pool)
        .await?)
}

/// Only the text with attachment paths (`attach::deliver_text`) matches the pane echo,
/// not what the user typed.
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

pub async fn queued_turn(pool: &SqlitePool, conversation_id: &str) -> Result<Option<Turn>> {
    Ok(sqlx::query_as::<_, Turn>(
        "SELECT * FROM turns WHERE conversation_id = ? AND status = 'queued' ORDER BY created_at, id LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?)
}

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

    /// 往 `SCHEMA` 加欄位卻忘了補 ALTER 名單：以前在開發者機器上一律是綠的（每個測試都開新 DB），
    /// 到使用者那裡才炸成 `SELECT *` 的 FromRow 失敗、daemon 起不來。現在 migrate 自己對帳。
    #[tokio::test]
    async fn a_column_the_alter_list_forgot_is_caught_before_the_user_sees_it() {
        let dir = std::env::temp_dir().join(format!("am-drift-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        // 舊資料庫：`bots` 少了一堆後來才加的欄位，而且 CREATE TABLE IF NOT EXISTS 不會補。
        {
            let old = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{}?mode=rwc", path.display()))
                .await
                .unwrap();
            // 少的是 `env_json`：它在 SCHEMA 裡、不在 ALTER 名單裡，也沒有索引用到它——
            // 正好是「加欄位忘了補 ALTER」會留下的形狀。索引要用的欄位照給，才測得到這個檢查本身。
            sqlx::query(
                "CREATE TABLE bots (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, name TEXT NOT NULL,
                   kind TEXT NOT NULL, model TEXT, effort TEXT, fast INTEGER NOT NULL DEFAULT 0, persona TEXT,
                   args_json TEXT NOT NULL DEFAULT '[]', autostart INTEGER NOT NULL DEFAULT 0,
                   inject_hooks INTEGER NOT NULL DEFAULT 1, auto_approve INTEGER NOT NULL DEFAULT 1,
                   identity TEXT, managed_by TEXT NOT NULL DEFAULT 'user', cwd TEXT, herdr_session TEXT,
                   parent_bot_id TEXT, hook_token TEXT NOT NULL, deleted_at TEXT, created_at TEXT NOT NULL,
                   is_primary INTEGER NOT NULL DEFAULT 0, position INTEGER NOT NULL DEFAULT 0)",
            )
            .execute(&old)
            .await
            .unwrap();
            old.close().await;
        }
        let err = open(&path).await.expect_err("少欄位的舊 DB 不該靜靜開起來").to_string();
        assert!(err.contains("schema drift"), "{err}");
        assert!(err.contains("bots."), "錯誤訊息要指名是哪張表：{err}");
        assert!(err.contains("ALTER TABLE"), "要告訴人怎麼修：{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 一個字都沒寫進去的那次重送要退還額度：`MAX_PROMPT_RESENDS` 是 1，
    /// 被「框裡剛好有字」這種兩秒後就消失的原因吃掉，等於永遠補救不了。
    #[tokio::test]
    async fn a_resend_that_wrote_nothing_gives_the_budget_back() {
        let dir = std::env::temp_dir().join(format!("am-refund-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = open(&dir.join("t.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
        let conv = conversation_id(&pool, "b").await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES ('t',?,'web','in_flight','ok',?)")
            .bind(&conv).bind(now()).execute(&pool).await.unwrap();

        assert!(claim_resend(&pool, "t", 1).await, "第一次拿得到");
        assert!(!claim_resend(&pool, "t", 1).await, "額度只有一次");
        refund_resend(&pool, "t").await;
        assert!(claim_resend(&pool, "t", 1).await, "退還之後還有一次");
        refund_resend(&pool, "t").await;
        refund_resend(&pool, "t").await;
        let n: i64 = sqlx::query_scalar("SELECT resend_count FROM turns WHERE id='t'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "退還不會退成負數");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

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

    /// Active runs and ended runs without an id must not steal the continuation slot.
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

    /// See [`crate::quota_claude::should_probe_identity`].
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
            ("b2", "pm", "cc2", "stopped"),
            ("b3", "pl", "cc3", "running"),
            ("b4", "pm", "", "running"),
            ("b5", "pm", "cc4", "starting"),
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

        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = 'b1'").bind(now()).execute(&pool).await.unwrap();
        assert_eq!(live_identities_on_host(&pool, "m4p").await.unwrap(), ["cc4".to_string()].into_iter().collect());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

