//! SQLite storage (SPEC appendix C).

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::path::Path;
use std::str::FromStr;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY, path TEXT NOT NULL, label TEXT NOT NULL,
  host TEXT NOT NULL DEFAULT 'local',
  workspace_id TEXT,
  deleted_at TEXT, created_at TEXT NOT NULL
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
  team_id TEXT,
  team_role TEXT,
  cwd TEXT,
  herdr_session TEXT,
  -- A herdr agent the bot itself spawned (named `<parent agent name>-<suffix>`), adopted by
  -- the reconcile and shown under its parent. NULL = a top-level bot.
  parent_bot_id TEXT,
  hook_token TEXT NOT NULL, deleted_at TEXT, created_at TEXT NOT NULL
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
  -- Native session requested by a reopen. Cleared by the first identity/turn hook.
  resume_session_id TEXT
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
  team_id TEXT,
  team_event_id TEXT,
  created_at TEXT NOT NULL, completed_at TEXT,
  -- The exact text to hand to the CLI when a queued web prompt is activated. NULL for old
  -- turns and external turns; the user-facing message keeps the original text.
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
  team_id TEXT,
  relay_from TEXT,
  created_at TEXT NOT NULL, updated_at TEXT
);
CREATE INDEX IF NOT EXISTS messages_conv_time ON messages(conversation_id, created_at);
CREATE INDEX IF NOT EXISTS messages_turn ON messages(turn_id);
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
CREATE TABLE IF NOT EXISTS teams (
  id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
  issue_number INTEGER NOT NULL, issue_title TEXT NOT NULL, issue_url TEXT NOT NULL,
  phase TEXT NOT NULL CHECK (phase IN ('starting','planning','working','finishing','done','paused','aborting','aborted','failed')),
  pause_reason TEXT, resume_phase TEXT,
  base_ref TEXT NOT NULL, base_sha TEXT NOT NULL, branch TEXT NOT NULL, worktree_root TEXT NOT NULL,
  -- SPEC-team §6.4a: the team's own herdr workspace. Nullable: a row written by an earlier
  -- build has none, and `NULL` is exactly what the reconcile treats as `workspace_missing`.
  workspace_id TEXT,
  deliver TEXT NOT NULL CHECK (deliver IN ('branch','pr')),
  supervised INTEGER NOT NULL DEFAULT 0,
  roles_json TEXT NOT NULL,
  budget_json TEXT NOT NULL, usage_json TEXT NOT NULL DEFAULT '{}',
  pr_url TEXT, summary TEXT,
  -- When the user closed the issue from the finished team (never automatic; see team::close_issue).
  issue_closed_at TEXT,
  -- Submodule path (relative to the project) the team works in; '' = the project itself.
  repo TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL, started_at TEXT, ended_at TEXT
);
CREATE INDEX IF NOT EXISTS teams_project ON teams(project_id);
-- SPEC-team §2.3: the team's issue queue. One team works a list of issues in `seq` order,
-- keeping the same PM and reviewer throughout and taking a fresh set of workers per issue.
--
-- `teams` keeps its scalar `issue_*` / `branch` / `base_sha` / `pr_url` / `summary` columns as
-- a mirror of whichever row here is `working` (or the last one finished). That is what lets
-- every existing query, `team_json` and the UI keep working unchanged, and it avoids rebuilding
-- `teams` just to drop three NOT NULLs (SPEC §12.5: SQLite cannot edit a CHECK in place).
CREATE TABLE IF NOT EXISTS team_issues (
  id TEXT PRIMARY KEY, team_id TEXT NOT NULL REFERENCES teams(id),
  seq INTEGER NOT NULL,
  issue_number INTEGER NOT NULL, issue_title TEXT NOT NULL, issue_url TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('queued','working','done','failed','skipped')),
  -- Filled when the issue starts: each issue cuts its own integration branch from `base_ref`.
  branch TEXT, base_sha TEXT,
  summary TEXT, pr_url TEXT, issue_closed_at TEXT,
  -- Why the queue moved past this one without delivering it (a `DECISION_PAUSES` reason).
  fail_reason TEXT,
  created_at TEXT NOT NULL, started_at TEXT, ended_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS team_issues_seq ON team_issues(team_id, seq);
-- SPEC-team §2.3: an issue number is unique among the rows still **on** the queue, not among
-- every row ever queued. A `done` / `failed` / `skipped` entry is a finished record, so the
-- same issue can be queued again as a new `seq` (a re-run after a failure, or a second pass
-- someone asks for) while the old row stays in the log.
CREATE UNIQUE INDEX IF NOT EXISTS team_issues_number_open ON team_issues(team_id, issue_number) WHERE state IN ('queued','working');
CREATE INDEX IF NOT EXISTS team_issues_open ON team_issues(team_id, state) WHERE state IN ('queued','working');
CREATE TABLE IF NOT EXISTS team_tasks (
  id TEXT PRIMARY KEY, team_id TEXT NOT NULL REFERENCES teams(id),
  seq INTEGER NOT NULL, title TEXT NOT NULL, brief TEXT NOT NULL, files_json TEXT NOT NULL DEFAULT '[]',
  worker_bot_id TEXT NOT NULL REFERENCES bots(id), branch TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('queued','working','reported','reviewing','changes_requested','exhausted',
                                       'blocked_by_worker','rebasing','merging','merged','skipped','failed')),
  round INTEGER NOT NULL DEFAULT 0, rebase_attempts INTEGER NOT NULL DEFAULT 0,
  last_report TEXT, last_verdict TEXT, merge_sha TEXT,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS team_tasks_seq ON team_tasks(team_id, seq);
CREATE UNIQUE INDEX IF NOT EXISTS team_tasks_one_open_per_worker ON team_tasks(worker_bot_id) WHERE state NOT IN ('merged','skipped','failed');
CREATE TABLE IF NOT EXISTS team_events (
  id TEXT PRIMARY KEY, team_id TEXT NOT NULL REFERENCES teams(id),
  -- Per-team insertion order, 1, 2, 3… This, not the ULID, is what §10.4 orders and pages by:
  -- a ULID's timestamp is only millisecond-resolution, and a scheduler that dispatches two
  -- workers writes several events inside one millisecond, where the ordering between ULIDs is
  -- decided by their random bits. `seq` makes the log's order exactly its write order.
  seq INTEGER NOT NULL DEFAULT 0,
  kind TEXT NOT NULL CHECK (kind IN ('relay','phase','merge','note','user')),
  from_bot_id TEXT, to_bot_id TEXT, task_id TEXT, turn_id TEXT,
  status TEXT CHECK (status IN ('pending','delivered','dropped')),
  payload_json TEXT NOT NULL, created_at TEXT NOT NULL
);
-- `team_events_seq` (UNIQUE on team_id, seq) is created after the ALTER section, not here:
-- on a file that already has a `seq`-less team_events, SCHEMA runs before the column exists.
CREATE INDEX IF NOT EXISTS team_events_pending ON team_events(team_id, to_bot_id) WHERE status = 'pending';
"#;

pub async fn open(path: &Path) -> Result<SqlitePool> {
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(10));
    // Migrations run on a throw-away single-connection pool that is closed again before the
    // real one is built.
    //
    // Why: the `projects` / `bots` migrations *rebuild* their table (SQLite cannot edit a
    // CHECK or a UNIQUE), which changes the column order. A connection that is idle in the
    // pool while that happens keeps the old layout, so the first `SELECT *` on it decodes the
    // wrong columns — on a pre-grok database that surfaced as
    // `decoding column "identity": … not compatible with SQL type INTEGER`, because the old
    // `identity` sits where the new `auto_approve` is. Handing out only connections opened
    // *after* the schema is final removes the whole class of problem.
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

/// Apply the schema and every additive / rebuild migration. Runs on one connection.
async fn migrate(mpool: &SqlitePool) -> Result<()> {
    let pool = mpool.clone();
    for stmt in SCHEMA.split(";\n") {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        sqlx::query(s).execute(&pool).await.with_context(|| format!("apply schema: {s}"))?;
    }
    // Additive migrations for databases created before a column existed.
    for (table, col, ddl) in [
        ("bots", "auto_approve", "ALTER TABLE bots ADD COLUMN auto_approve INTEGER NOT NULL DEFAULT 1"),
        ("projects", "host", "ALTER TABLE projects ADD COLUMN host TEXT NOT NULL DEFAULT 'local'"),
        ("bots", "identity", "ALTER TABLE bots ADD COLUMN identity TEXT"),
        ("bots", "env_json", "ALTER TABLE bots ADD COLUMN env_json TEXT NOT NULL DEFAULT '{}'"),
        ("bots", "herdr_session", "ALTER TABLE bots ADD COLUMN herdr_session TEXT"),
        ("runs", "agent_name", "ALTER TABLE runs ADD COLUMN agent_name TEXT"),
        // The herdr tab the run's pane lives in. A bot started from v4 on gets a tab of its
        // own (`tab.create`), and stopping it takes the tab with it. Additive and nullable:
        // every run started before this column existed was `pane.split` into a shared tab,
        // and NULL is exactly that — "this pane does not own its tab, leave the tab alone".
        // Reconcile backfills it from herdr, so old runs are adopted, never failed, for it.
        ("runs", "tab_id", "ALTER TABLE runs ADD COLUMN tab_id TEXT"),
        ("runs", "herdr_session", "ALTER TABLE runs ADD COLUMN herdr_session TEXT"),
        ("teams", "repo", "ALTER TABLE teams ADD COLUMN repo TEXT NOT NULL DEFAULT ''"),
        ("bots", "parent_bot_id", "ALTER TABLE bots ADD COLUMN parent_bot_id TEXT"),
        // What the agent calls itself right now (its terminal title, e.g. Claude Code's
        // one-line summary of the task it is on).
        ("runs", "agent_title", "ALTER TABLE runs ADD COLUMN agent_title TEXT"),
        // The bot's own status bar, verbatim (claude's statusLine output, ANSI stripped).
        ("runs", "status_line", "ALTER TABLE runs ADD COLUMN status_line TEXT"),
        // The whole statusLine payload (model, context_window, rate_limits, cost…) as JSON.
        ("runs", "status_json", "ALTER TABLE runs ADD COLUMN status_json TEXT"),
        ("bots", "model", "ALTER TABLE bots ADD COLUMN model TEXT"),
        ("bots", "effort", "ALTER TABLE bots ADD COLUMN effort TEXT"),
        ("bots", "fast", "ALTER TABLE bots ADD COLUMN fast INTEGER NOT NULL DEFAULT 0"),
        ("bots", "persona", "ALTER TABLE bots ADD COLUMN persona TEXT"),
        // SPEC §13: project group chat stamps every message of one send with a group id.
        ("messages", "group_id", "ALTER TABLE messages ADD COLUMN group_id TEXT"),
        // Images dropped into the composer: a JSON array of {id, name, mime, size, path}.
        ("messages", "attachments_json", "ALTER TABLE messages ADD COLUMN attachments_json TEXT"),
        // SPEC-team §2.1: team members are ordinary bots carrying these four columns.
        // `managed_by='team'` also takes the bot out of the TOML projection (projection.rs).
        ("bots", "managed_by", "ALTER TABLE bots ADD COLUMN managed_by TEXT NOT NULL DEFAULT 'user'"),
        ("bots", "team_id", "ALTER TABLE bots ADD COLUMN team_id TEXT"),
        ("bots", "team_role", "ALTER TABLE bots ADD COLUMN team_role TEXT"),
        // SPEC-team §2.2: where `pane.split` puts the agent. NULL = `project.path`.
        ("bots", "cwd", "ALTER TABLE bots ADD COLUMN cwd TEXT"),
        // Which team relay produced this turn (NULL = user / external).
        ("turns", "team_id", "ALTER TABLE turns ADD COLUMN team_id TEXT"),
        ("turns", "team_event_id", "ALTER TABLE turns ADD COLUMN team_event_id TEXT"),
        // Server-side prompt queue: old databases get the payload column before the turns
        // table is rebuilt to widen its status CHECK below.
        ("turns", "prompt_text", "ALTER TABLE turns ADD COLUMN prompt_text TEXT"),
        // Which team this message belongs to, and which bot it was relayed from.
        ("messages", "team_id", "ALTER TABLE messages ADD COLUMN team_id TEXT"),
        ("messages", "relay_from", "ALTER TABLE messages ADD COLUMN relay_from TEXT"),
        // SPEC-team §10.4: the team log's own order (see the column comment in SCHEMA).
        ("team_events", "seq", "ALTER TABLE team_events ADD COLUMN seq INTEGER NOT NULL DEFAULT 0"),
        // SPEC-team §6.4a: the team's own herdr workspace. Additive and nullable, so a
        // database written before this column existed opens unchanged — the teams in it
        // simply have no workspace, which the reconcile reports as `workspace_missing`
        // rather than crashing the daemon on startup.
        ("teams", "workspace_id", "ALTER TABLE teams ADD COLUMN workspace_id TEXT"),
        // When the user closed the team's GitHub issue. NULL = still open, or never asked —
        // closing is always an explicit human action, so an old row simply has nothing here.
        ("teams", "issue_closed_at", "ALTER TABLE teams ADD COLUMN issue_closed_at TEXT"),
        // Native session requested by a done-team reopen. NULL for ordinary starts and old runs.
        ("runs", "resume_session_id", "ALTER TABLE runs ADD COLUMN resume_session_id TEXT"),
        // SPEC-team §2.3: which queued issue this task / event belongs to. Nullable because
        // every row written before the queue existed belongs to the team's one and only issue,
        // which the backfill below fills in.
        ("team_tasks", "issue_id", "ALTER TABLE team_tasks ADD COLUMN issue_id TEXT"),
        ("team_events", "issue_id", "ALTER TABLE team_events ADD COLUMN issue_id TEXT"),
    ] {
        let has: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?"))
            .bind(col)
            .fetch_one(&pool)
            .await?;
        if has == 0 {
            sqlx::query(ddl).execute(&pool).await.with_context(|| format!("migrate: {ddl}"))?;
        }
    }
    // Partial index on the §13 column; created here (not in SCHEMA) because on a pre-§13 file
    // SCHEMA runs before the ALTER above.
    sqlx::query("CREATE INDEX IF NOT EXISTS messages_group ON messages(group_id) WHERE group_id IS NOT NULL")
        .execute(&pool)
        .await?;
    // SPEC-team §2.3: give every pre-queue team the one-row queue it always implicitly had.
    // The id is derived from the team id rather than a fresh ULID so the backfill is
    // idempotent, and the `NOT EXISTS` guard means it only ever runs once per team.
    sqlx::query(
        "INSERT INTO team_issues
           (id, team_id, seq, issue_number, issue_title, issue_url, state,
            branch, base_sha, summary, pr_url, issue_closed_at, created_at, started_at, ended_at)
         SELECT t.id || '-i1', t.id, 1, t.issue_number, t.issue_title, t.issue_url,
                CASE t.phase WHEN 'done' THEN 'done'
                             WHEN 'failed' THEN 'failed'
                             WHEN 'aborted' THEN 'failed'
                             ELSE 'working' END,
                t.branch, t.base_sha, t.summary, t.pr_url, t.issue_closed_at,
                t.created_at, t.started_at, t.ended_at
           FROM teams t
          WHERE NOT EXISTS (SELECT 1 FROM team_issues i WHERE i.team_id = t.id)",
    )
    .execute(&pool)
    .await?;
    // A database upgraded after the issue queue migration can already have a one-row queue,
    // but rows created by an intermediate build may not have copied the old team mirror's
    // close stamp. Keep the queue row authoritative while preserving the old value.
    sqlx::query(
        "UPDATE team_issues SET issue_closed_at = (
             SELECT t.issue_closed_at FROM teams t
              WHERE t.id = team_issues.team_id AND t.issue_number = team_issues.issue_number
         )
          WHERE issue_closed_at IS NULL",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE team_tasks SET issue_id = (SELECT i.id FROM team_issues i
           WHERE i.team_id = team_tasks.team_id ORDER BY i.seq LIMIT 1)
         WHERE issue_id IS NULL",
    )
    .execute(&pool)
    .await?;
    // SPEC-team §10.4: `team_events.seq` replaced the ULID as the log's order. A file written
    // by an earlier build of this branch has the column added by the ALTER above with every
    // row at 0, so number the existing rows (by id, the best order that file has) before the
    // UNIQUE index goes on. Real sequence numbers start at 1, so `seq = 0` means "not yet
    // numbered" and the backfill is safe to re-run.
    sqlx::query(
        "UPDATE team_events SET seq = (SELECT COUNT(*) FROM team_events e2
           WHERE e2.team_id = team_events.team_id AND e2.id <= team_events.id)
         WHERE seq = 0",
    )
    .execute(&pool)
    .await?;
    sqlx::query("DROP INDEX IF EXISTS team_events_team_time").execute(&pool).await?;
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS team_events_seq ON team_events(team_id, seq)")
        .execute(&pool)
        .await?;
    // SPEC-team §2.3: `(team_id, issue_number)` used to be unique over *every* row, which made
    // an issue that had already been done (or had failed) unqueueable for ever. The partial
    // index in SCHEMA replaces it: uniqueness now only covers the rows still on the queue.
    // Old rows can never violate it — the index it replaces was stricter.
    sqlx::query("DROP INDEX IF EXISTS team_issues_number").execute(&pool).await?;
    // Bot names used to be unique across the whole daemon; since the herdr agent name is now
    // `<project>-<bot>`, uniqueness is per project.
    sqlx::query("DROP INDEX IF EXISTS bots_name_live").execute(&pool).await?;
    // The first cut of the §11 migration made (host, path) unique over *all* rows, which
    // stopped a soft-deleted project's directory from being registered again.
    sqlx::query("DROP INDEX IF EXISTS projects_host_path").execute(&pool).await?;
    // SPEC §11: `path` used to be UNIQUE across every machine. The same directory can now
    // exist on several hosts, so rebuild the table with a UNIQUE(host, path) index instead.
    {
        let mut conn = pool.acquire().await?;
        recover_stale_rebuild(&mut conn, "projects").await?;
        let projects_ddl: Option<String> =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type='table' AND name='projects'")
                .fetch_optional(&mut *conn)
                .await?;
        let needs_rebuild = projects_ddl.as_deref().map(|d| d.contains("path TEXT NOT NULL UNIQUE")).unwrap_or(false);
        if needs_rebuild {
            tracing::info!("migrating projects.path UNIQUE -> UNIQUE(host, path)");
            rebuild_table(
                &mut conn,
                "projects",
                &[
                    "DROP TABLE IF EXISTS projects_new",
                    "CREATE TABLE projects_new (
                       id TEXT PRIMARY KEY, path TEXT NOT NULL, label TEXT NOT NULL,
                       host TEXT NOT NULL DEFAULT 'local', workspace_id TEXT,
                       deleted_at TEXT, created_at TEXT NOT NULL)",
                    "INSERT INTO projects_new (id, path, label, host, workspace_id, deleted_at, created_at)
                       SELECT id, path, label, host, workspace_id, deleted_at, created_at FROM projects",
                    "DROP TABLE projects",
                    "ALTER TABLE projects_new RENAME TO projects",
                ],
            )
            .await?;
        }
        sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS projects_host_path_live ON projects(host, path) WHERE deleted_at IS NULL")
            .execute(&mut *conn)
            .await?;
    }
    migrate_bots_kind_check(&pool).await?;
    migrate_turn_status_check(&pool).await?;
    Ok(())
}

/// Run one "rebuild the table" migration (`CREATE xxx_new` / `INSERT … SELECT` / `DROP xxx` /
/// `ALTER xxx_new RENAME TO xxx`) as a single transaction on ONE connection.
///
/// Why a transaction: SQLite DDL is transactional, so the daemon can die at any point in the
/// dance and the next start sees either the untouched old table or the finished new one —
/// never an empty `xxx` next to a full `xxx_new` (#34). `PRAGMA foreign_keys` is a no-op
/// inside a transaction, so both pragmas are set outside it; they are per-connection, which
/// is why the caller must hand over the connection it inspected the schema on.
/// `legacy_alter_table=ON` keeps the rename from rewriting the FKs of tables that reference
/// `xxx(id)` by name, so the ids stay valid.
async fn rebuild_table(conn: &mut sqlx::SqliteConnection, table: &str, steps: &[&str]) -> Result<()> {
    sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await?;
    sqlx::query("PRAGMA legacy_alter_table=ON").execute(&mut *conn).await?;
    let result: Result<()> = async {
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        for stmt in steps {
            sqlx::query(stmt).execute(&mut *tx).await.with_context(|| format!("migrate {table}: {stmt}"))?;
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    // Restore the pragmas even when the transaction rolled back, so a later statement on this
    // connection does not silently run without FK enforcement.
    sqlx::query("PRAGMA legacy_alter_table=OFF").execute(&mut *conn).await?;
    sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await?;
    result
}

async fn table_exists(conn: &mut sqlx::SqliteConnection, table: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
        .bind(table)
        .fetch_one(&mut *conn)
        .await?;
    Ok(n > 0)
}

async fn row_count(conn: &mut sqlx::SqliteConnection, table: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}")).fetch_one(&mut *conn).await?)
}

/// Deal with a `<table>_new` scratch table left behind by a build that ran the rebuild
/// without a transaction and died half-way (the bug in #34). By the time this runs, `SCHEMA`
/// has already put an (empty) `<table>` back, so the rule is simply: never drop the copy
/// that holds the data.
///
/// * `<table>_new` empty → it is a leftover scratch copy, drop it.
/// * `<table>_new` has rows and `<table>` is empty → the old code died between `DROP` and
///   `RENAME`; finish the job by swapping `<table>_new` in (in a transaction).
/// * both have rows → something we do not understand; refuse to start rather than guess.
async fn recover_stale_rebuild(conn: &mut sqlx::SqliteConnection, table: &str) -> Result<()> {
    let scratch = format!("{table}_new");
    if !table_exists(conn, &scratch).await? {
        return Ok(());
    }
    let in_scratch = row_count(conn, &scratch).await?;
    if in_scratch == 0 {
        tracing::warn!("dropping empty scratch table {scratch} left by an interrupted migration");
        sqlx::query(&format!("DROP TABLE {scratch}")).execute(&mut *conn).await?;
        return Ok(());
    }
    let in_table = if table_exists(conn, table).await? { row_count(conn, table).await? } else { 0 };
    if in_table > 0 {
        anyhow::bail!(
            "both {table} ({in_table} rows) and {scratch} ({in_scratch} rows) hold data after an \
             interrupted migration; refusing to guess which one to keep — inspect the database by hand"
        );
    }
    tracing::warn!("finishing an interrupted rebuild of {table}: swapping in {scratch} ({in_scratch} rows)");
    let drop_old = format!("DROP TABLE IF EXISTS {table}");
    let rename = format!("ALTER TABLE {scratch} RENAME TO {table}");
    rebuild_table(conn, table, &[&drop_old, &rename]).await
}

/// SPEC §12: `bots.kind` gained `grok`. SQLite cannot edit a CHECK constraint, so a database
/// created with the two-value CHECK is rebuilt once (same rename dance as `projects` above).
/// `runs` / `conversations` reference `bots(id)` by name, so with `legacy_alter_table=ON` the
/// rename does not rewrite their FKs and the ids stay valid.
async fn migrate_bots_kind_check(pool: &SqlitePool) -> Result<()> {
    let mut conn = pool.acquire().await?;
    recover_stale_rebuild(&mut conn, "bots").await?;
    let ddl: Option<String> = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type='table' AND name='bots'")
        .fetch_optional(&mut *conn)
        .await?;
    let needs_rebuild = ddl.as_deref().map(|d| d.contains("kind IN ('claude','codex'))")).unwrap_or(false);
    if needs_rebuild {
        tracing::info!("migrating bots.kind CHECK -> ('claude','codex','grok')");
        rebuild_table(
            &mut conn,
            "bots",
            &[
                "DROP TABLE IF EXISTS bots_new",
                "CREATE TABLE bots_new (
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
                   team_id TEXT,
                   team_role TEXT,
                   cwd TEXT,
                   herdr_session TEXT,
                   parent_bot_id TEXT,
                   hook_token TEXT NOT NULL, deleted_at TEXT, created_at TEXT NOT NULL)",
                "INSERT INTO bots_new (id, project_id, name, kind, model, effort, fast, persona, args_json, autostart, inject_hooks, auto_approve, identity, env_json, managed_by, team_id, team_role, cwd, herdr_session, parent_bot_id, hook_token, deleted_at, created_at)
                   SELECT id, project_id, name, kind, model, effort, fast, persona, args_json, autostart, inject_hooks, auto_approve, identity, env_json, managed_by, team_id, team_role, cwd, herdr_session, parent_bot_id, hook_token, deleted_at, created_at FROM bots",
                "DROP TABLE bots",
                "ALTER TABLE bots_new RENAME TO bots",
            ],
        )
        .await?;
    }
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS bots_name_project_live ON bots(project_id, name) WHERE deleted_at IS NULL")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `turns.status` gained `queued` for the daemon-owned prompt queue. SQLite cannot edit a
/// CHECK constraint in place, so rebuild the table once while preserving every existing row.
async fn migrate_turn_status_check(pool: &SqlitePool) -> Result<()> {
    let mut conn = pool.acquire().await?;
    recover_stale_rebuild(&mut conn, "turns").await?;
    let ddl: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type='table' AND name='turns'")
            .fetch_optional(&mut *conn)
            .await?;
    let needs_rebuild = ddl.as_deref().map(|d| !d.contains("status IN ('queued'")).unwrap_or(false);
    if needs_rebuild {
        tracing::info!("migrating turns.status CHECK to include queued prompts");
        rebuild_table(
            &mut conn,
            "turns",
            &[
                "DROP TABLE IF EXISTS turns_new",
                "CREATE TABLE turns_new (
                   id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id),
                   run_id TEXT REFERENCES runs(id),
                   origin TEXT NOT NULL CHECK (origin IN ('web','external')),
                   status TEXT NOT NULL CHECK (status IN ('queued','in_flight','completed','completed_fallback','failed')),
                   delivery TEXT NOT NULL DEFAULT 'pending' CHECK (delivery IN ('pending','ok','unknown','failed')),
                   client_request_id TEXT,
                   native_session_id TEXT, native_turn_id TEXT,
                   team_id TEXT, team_event_id TEXT,
                   created_at TEXT NOT NULL, completed_at TEXT, prompt_text TEXT
                )",
                "INSERT INTO turns_new (id, conversation_id, run_id, origin, status, delivery, client_request_id,
                   native_session_id, native_turn_id, team_id, team_event_id, created_at, completed_at, prompt_text)
                 SELECT id, conversation_id, run_id, origin, status, delivery, client_request_id,
                   native_session_id, native_turn_id, team_id, team_event_id, created_at, completed_at, prompt_text
                 FROM turns",
                "DROP TABLE turns",
                "ALTER TABLE turns_new RENAME TO turns",
            ],
        )
        .await?;
    }
    for stmt in [
        "CREATE UNIQUE INDEX IF NOT EXISTS turns_one_in_flight ON turns(run_id) WHERE status = 'in_flight'",
        "CREATE UNIQUE INDEX IF NOT EXISTS turns_one_queued ON turns(conversation_id) WHERE status = 'queued'",
        "CREATE UNIQUE INDEX IF NOT EXISTS turns_client_req ON turns(conversation_id, client_request_id) WHERE client_request_id IS NOT NULL",
        "CREATE UNIQUE INDEX IF NOT EXISTS turns_native ON turns(native_session_id, native_turn_id) WHERE native_turn_id IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS turns_conv_time ON turns(conversation_id, created_at)",
    ] {
        sqlx::query(stmt).execute(&mut *conn).await.with_context(|| format!("migrate turns: {stmt}"))?;
    }
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
    /// SPEC-team §2.1: `user` (from config.toml) or `team` (a daemon-owned team member,
    /// which the TOML projection must leave alone).
    pub managed_by: String,
    pub team_id: Option<String>,
    /// `pm` | `worker` | `reviewer` for team members; NULL otherwise.
    pub team_role: Option<String>,
    /// SPEC-team §2.2: working directory for `pane.split`. NULL = the project's path.
    pub cwd: Option<String>,
    /// Herdr session override. `Some("default")` identifies an imported user-session bot.
    pub herdr_session: Option<String>,
    /// The bot whose agent spawned this one (`<parent agent name>-<suffix>`); None = top-level.
    pub parent_bot_id: Option<String>,
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
    /// SPEC-team §2.1: the team this turn belongs to, and the relay that produced it.
    pub team_id: Option<String>,
    pub team_event_id: Option<String>,
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
    /// SPEC-team §2.1: the team this message belongs to, and (for a relay) the bot it came from.
    pub team_id: Option<String>,
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

/// SPEC-team §2.1 — one issue's team. SQLite is authoritative; teams never enter config.toml.
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct Team {
    pub id: String,
    pub project_id: String,
    pub issue_number: i64,
    pub issue_title: String,
    pub issue_url: String,
    pub phase: String,
    pub pause_reason: Option<String>,
    pub resume_phase: Option<String>,
    pub base_ref: String,
    pub base_sha: String,
    pub branch: String,
    pub worktree_root: String,
    /// SPEC-team §6.4a: the herdr workspace this team's panes live in. `None` on a row from
    /// before the column existed, or after the workspace was closed.
    pub workspace_id: Option<String>,
    pub deliver: String,
    pub supervised: i64,
    pub roles_json: String,
    pub budget_json: String,
    pub usage_json: String,
    pub pr_url: Option<String>,
    pub summary: Option<String>,
    /// When the user closed the issue from this team (SPEC-team §10.7). `None` = the issue was
    /// never closed from here; nothing in the daemon ever sets it without a human asking.
    pub issue_closed_at: Option<String>,
    /// Submodule path (relative to the project) this team works in; empty = the project itself.
    pub repo: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

/// SPEC-team §8.2 — one dispatched piece of work and its review state machine.
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct TeamTask {
    pub id: String,
    pub team_id: String,
    /// Which queued issue this task belongs to (SPEC-team §2.3). `None` only on a row that
    /// predates the queue and whose team has since been deleted.
    pub issue_id: Option<String>,
    pub seq: i64,
    pub title: String,
    pub brief: String,
    pub files_json: String,
    pub worker_bot_id: String,
    pub branch: String,
    pub state: String,
    pub round: i64,
    pub rebase_attempts: i64,
    pub last_report: Option<String>,
    pub last_verdict: Option<String>,
    pub merge_sha: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// SPEC-team §2.1 — the team audit log and the pending-relay outbox in one table.
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct TeamEvent {
    pub id: String,
    pub team_id: String,
    /// Per-team insertion order (1, 2, 3…). The log is ordered and paged by this, never by
    /// `id`: several events written inside one millisecond get ULIDs whose relative order is
    /// random, which made §10.4's "oldest first" contract non-deterministic.
    pub seq: i64,
    /// The queued issue this event happened under; `None` for team-level events.
    pub issue_id: Option<String>,
    pub kind: String,
    pub from_bot_id: Option<String>,
    pub to_bot_id: Option<String>,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub status: Option<String>,
    pub payload_json: String,
    pub created_at: String,
}

/// SPEC-team §2.3 — one entry in a team's issue queue.
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct TeamIssue {
    pub id: String,
    pub team_id: String,
    pub seq: i64,
    pub issue_number: i64,
    pub issue_title: String,
    pub issue_url: String,
    pub state: String,
    pub branch: Option<String>,
    pub base_sha: Option<String>,
    pub summary: Option<String>,
    pub pr_url: Option<String>,
    pub issue_closed_at: Option<String>,
    pub fail_reason: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

/// The queue states that still owe work; the queue is done when none are left.
pub const OPEN_ISSUE_STATES: &str = "('queued','working')";

pub const ACTIVE_STATES: &str = "('starting','running','stopping')";

pub async fn team(pool: &SqlitePool, id: &str) -> Result<Option<Team>> {
    Ok(sqlx::query_as::<_, Team>("SELECT * FROM teams WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

/// Every team of a project, oldest first.
pub async fn teams_of_project(pool: &SqlitePool, project_id: &str) -> Result<Vec<Team>> {
    Ok(sqlx::query_as::<_, Team>("SELECT * FROM teams WHERE project_id = ? ORDER BY created_at")
        .bind(project_id)
        .fetch_all(pool)
        .await?)
}

/// Teams that still need a scheduler (SPEC-team §7.5): anything not in a terminal phase.
pub async fn live_teams(pool: &SqlitePool) -> Result<Vec<Team>> {
    Ok(sqlx::query_as::<_, Team>(
        "SELECT * FROM teams WHERE phase NOT IN ('done','aborted','failed') ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?)
}

/// A team's member bots (including soft-deleted ones after cleanup), in creation order.
pub async fn team_members(pool: &SqlitePool, team_id: &str) -> Result<Vec<Bot>> {
    Ok(sqlx::query_as::<_, Bot>("SELECT * FROM bots WHERE team_id = ? ORDER BY created_at")
        .bind(team_id)
        .fetch_all(pool)
        .await?)
}

pub async fn team_tasks(pool: &SqlitePool, team_id: &str) -> Result<Vec<TeamTask>> {
    Ok(sqlx::query_as::<_, TeamTask>("SELECT * FROM team_tasks WHERE team_id = ? ORDER BY seq")
        .bind(team_id)
        .fetch_all(pool)
        .await?)
}

pub async fn team_task(pool: &SqlitePool, id: &str) -> Result<Option<TeamTask>> {
    Ok(sqlx::query_as::<_, TeamTask>("SELECT * FROM team_tasks WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

/// A team's whole issue queue, in working order.
pub async fn team_issues(pool: &SqlitePool, team_id: &str) -> Result<Vec<TeamIssue>> {
    Ok(sqlx::query_as::<_, TeamIssue>("SELECT * FROM team_issues WHERE team_id = ? ORDER BY seq")
        .bind(team_id)
        .fetch_all(pool)
        .await?)
}

pub async fn team_issue(pool: &SqlitePool, id: &str) -> Result<Option<TeamIssue>> {
    Ok(sqlx::query_as::<_, TeamIssue>("SELECT * FROM team_issues WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

/// The issue the team is on right now, or `None` between issues / once the queue is empty.
pub async fn current_team_issue(pool: &SqlitePool, team_id: &str) -> Result<Option<TeamIssue>> {
    Ok(sqlx::query_as::<_, TeamIssue>(
        "SELECT * FROM team_issues WHERE team_id = ? AND state = 'working' ORDER BY seq LIMIT 1",
    )
    .bind(team_id)
    .fetch_optional(pool)
    .await?)
}

/// The next issue waiting to be picked up, if any.
pub async fn next_queued_issue(pool: &SqlitePool, team_id: &str) -> Result<Option<TeamIssue>> {
    Ok(sqlx::query_as::<_, TeamIssue>(
        "SELECT * FROM team_issues WHERE team_id = ? AND state = 'queued' ORDER BY seq LIMIT 1",
    )
    .bind(team_id)
    .fetch_optional(pool)
    .await?)
}

/// The tasks of one issue, in dispatch order. Task `seq` stays team-global (its unique index
/// is `(team_id, seq)`), so this filters on `issue_id`, not on a per-issue counter.
pub async fn team_tasks_of_issue(pool: &SqlitePool, team_id: &str, issue_id: &str) -> Result<Vec<TeamTask>> {
    Ok(sqlx::query_as::<_, TeamTask>(
        "SELECT * FROM team_tasks WHERE team_id = ? AND issue_id = ? ORDER BY seq",
    )
    .bind(team_id)
    .bind(issue_id)
    .fetch_all(pool)
    .await?)
}

pub async fn bot(pool: &SqlitePool, id: &str) -> Result<Option<Bot>> {
    Ok(sqlx::query_as::<_, Bot>("SELECT * FROM bots WHERE id = ?").bind(id).fetch_optional(pool).await?)
}

pub async fn live_bots(pool: &SqlitePool) -> Result<Vec<Bot>> {
    Ok(sqlx::query_as::<_, Bot>("SELECT * FROM bots WHERE deleted_at IS NULL ORDER BY created_at")
        .fetch_all(pool)
        .await?)
}

pub async fn live_projects(pool: &SqlitePool) -> Result<Vec<Project>> {
    Ok(sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE deleted_at IS NULL ORDER BY created_at")
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

/// The most recent native session from an ended run. Reopen deliberately does not resume an
/// active run: the scheduler only calls this after the team has reached `done`.
pub async fn last_native_session_id(pool: &SqlitePool, bot_id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT native_session_id FROM runs
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
    sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?)")
        .bind(&id)
        .bind(bot_id)
        .bind(now())
        .execute(pool)
        .await?;
    Ok(id)
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
/// started under, so a rename of the project label (or a legacy bare-name run) keeps working.
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
mod team_migration_tests {
    use super::*;

    /// A pre-SPEC-team database: no `bots.{managed_by,team_id,team_role,cwd}`, no
    /// `turns.{team_id,team_event_id}`, no `messages.{team_id,relay_from}` — and the old
    /// two-value `bots.kind` CHECK, so `open` also has to run the table rebuild.
    const OLD_SCHEMA: &str = "
CREATE TABLE projects (id TEXT PRIMARY KEY, path TEXT NOT NULL, label TEXT NOT NULL,
  host TEXT NOT NULL DEFAULT 'local', workspace_id TEXT, deleted_at TEXT, created_at TEXT NOT NULL);
CREATE TABLE bots (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
  name TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('claude','codex')),
  args_json TEXT NOT NULL DEFAULT '[]', autostart INTEGER NOT NULL DEFAULT 0,
  inject_hooks INTEGER NOT NULL DEFAULT 1,
  hook_token TEXT NOT NULL, deleted_at TEXT, created_at TEXT NOT NULL);
CREATE TABLE runs (id TEXT PRIMARY KEY, bot_id TEXT NOT NULL REFERENCES bots(id),
  state TEXT NOT NULL, agent_status TEXT NOT NULL DEFAULT 'unknown',
  workspace_id TEXT, pane_id TEXT, adopted INTEGER NOT NULL DEFAULT 0,
  native_session_id TEXT, transcript_path TEXT,
  last_read_revision INTEGER, last_read_tail_hash TEXT,
  started_at TEXT NOT NULL, ended_at TEXT);
CREATE TABLE conversations (id TEXT PRIMARY KEY, bot_id TEXT NOT NULL UNIQUE REFERENCES bots(id), created_at TEXT NOT NULL);
CREATE TABLE turns (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id),
  run_id TEXT REFERENCES runs(id), origin TEXT NOT NULL, status TEXT NOT NULL,
  delivery TEXT NOT NULL DEFAULT 'pending', client_request_id TEXT,
  native_session_id TEXT, native_turn_id TEXT, created_at TEXT NOT NULL, completed_at TEXT);
CREATE TABLE messages (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id),
  turn_id TEXT REFERENCES turns(id), role TEXT NOT NULL, content TEXT NOT NULL, source TEXT NOT NULL,
  incomplete INTEGER NOT NULL DEFAULT 0, terminal_snapshot TEXT,
  created_at TEXT NOT NULL, updated_at TEXT);
";

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

    /// A database written before the team columns existed picks them up on the next start,
    /// keeps its rows, and gains the three new tables.
    #[tokio::test]
    async fn old_database_is_migrated_in_place() {
        let dir = tmp_dir();
        let file = dir.join("old.sqlite3");
        {
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display()))
                .unwrap()
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
            for stmt in OLD_SCHEMA.split(";\n") {
                let s = stmt.trim();
                if !s.is_empty() {
                    sqlx::query(s).execute(&pool).await.unwrap();
                }
            }
            sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
                .bind(now())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','old','claude','tok',?)",
            )
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let pool = open(&file).await.expect("migrate an old database");
        // The very first query after `open`, exactly like the daemon's own start-up order:
        // this is what caught the stale-column-layout bug that `open` now avoids by closing
        // the migration pool before building the real one.
        let all = live_bots(&pool).await.expect("live_bots right after the rebuild");
        assert_eq!(all.len(), 1);
        {
            let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
            crate::projection::project_config(&cfg, &pool).await.expect("projection after the rebuild");
        }

        for c in ["managed_by", "team_id", "team_role", "cwd"] {
            assert!(columns(&pool, "bots").await.contains(&c.to_string()), "bots.{c} missing");
        }
        for c in ["team_id", "team_event_id"] {
            assert!(columns(&pool, "turns").await.contains(&c.to_string()), "turns.{c} missing");
        }
        for c in ["team_id", "relay_from"] {
            assert!(columns(&pool, "messages").await.contains(&c.to_string()), "messages.{c} missing");
        }

        // The existing bot survived the CHECK rebuild and defaults to user-managed.
        let b = bot(&pool, "b1").await.unwrap().expect("old bot row still there");
        assert_eq!(b.name, "old");
        assert_eq!(b.managed_by, "user");
        assert!(b.team_id.is_none() && b.team_role.is_none() && b.cwd.is_none());

        // `grok` is now accepted (the CHECK really was rebuilt).
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b2','p1','g','grok','t',?)")
            .bind(now())
            .execute(&pool)
            .await
            .expect("grok allowed after the rebuild");

        // The three new tables exist and are usable.
        for t in ["teams", "team_tasks", "team_events"] {
            let n: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {t}")).fetch_one(&pool).await.unwrap();
            assert_eq!(n, 0);
        }
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build an `OLD_SCHEMA` file with one project / bot / conversation and `n` turns.
    async fn old_file_with_turns(file: &std::path::Path, n: usize) {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
        for stmt in OLD_SCHEMA.split(";\n") {
            let s = stmt.trim();
            if !s.is_empty() {
                sqlx::query(s).execute(&pool).await.unwrap();
            }
        }
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','old','claude','tok',?)")
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES ('c1','b1',?)")
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
        for i in 0..n {
            sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, created_at) VALUES (?,'c1','web','completed',?)")
                .bind(format!("t{i}"))
                .bind(now())
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;
    }

    /// #34: a build without the transaction could die between `DROP TABLE turns` and
    /// `ALTER TABLE turns_new RENAME TO turns`, leaving every turn in `turns_new` and no
    /// `turns` at all. On the next start `SCHEMA` recreates an empty `turns`; the old code
    /// then dropped `turns_new` — all conversation history gone. The recovery must instead
    /// swap the full scratch table back in.
    #[tokio::test]
    async fn a_rebuild_interrupted_between_drop_and_rename_keeps_every_turn() {
        let dir = tmp_dir();
        let file = dir.join("half-migrated.sqlite3");
        old_file_with_turns(&file, 3).await;
        {
            // Reproduce the crash window by hand: copy `turns` into `turns_new`, drop `turns`.
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display())).unwrap();
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
            // `turns_new` has the shape the real migration gives it (the old build got that
            // far); the only thing missing is the RENAME.
            sqlx::query(
                "CREATE TABLE turns_new (
                   id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL REFERENCES conversations(id),
                   run_id TEXT REFERENCES runs(id),
                   origin TEXT NOT NULL CHECK (origin IN ('web','external')),
                   status TEXT NOT NULL CHECK (status IN ('queued','in_flight','completed','completed_fallback','failed')),
                   delivery TEXT NOT NULL DEFAULT 'pending' CHECK (delivery IN ('pending','ok','unknown','failed')),
                   client_request_id TEXT, native_session_id TEXT, native_turn_id TEXT,
                   team_id TEXT, team_event_id TEXT,
                   created_at TEXT NOT NULL, completed_at TEXT, prompt_text TEXT)",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO turns_new (id, conversation_id, origin, status, created_at)
                 SELECT id, conversation_id, origin, status, created_at FROM turns",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("DROP TABLE turns").execute(&pool).await.unwrap();
            pool.close().await;
        }

        let pool = open(&file).await.expect("a half-migrated database opens");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 3, "the turns that sat in turns_new were swapped back in");
        let stale: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name='turns_new'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stale, 0, "no scratch table is left behind");
        // The recovered table is queryable through the daemon's own accessor, and the file
        // opens a second time without touching anything.
        assert!(queued_turn_for_bot(&pool, "b1").await.unwrap().is_none());
        pool.close().await;
        let pool = open(&file).await.expect("re-open is a no-op");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 3);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same window for `projects`: the old code's "finish the job" branch renamed
    /// `projects_new` over a `projects` that `SCHEMA` had already recreated, and the daemon
    /// failed to start forever. Now the empty copy is the one that goes.
    #[tokio::test]
    async fn a_projects_rebuild_interrupted_between_drop_and_rename_still_opens() {
        let dir = tmp_dir();
        let file = dir.join("half-migrated-projects.sqlite3");
        old_file_with_turns(&file, 0).await;
        {
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display())).unwrap();
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
            sqlx::query("PRAGMA foreign_keys=OFF").execute(&pool).await.unwrap();
            sqlx::query("CREATE TABLE projects_new AS SELECT * FROM projects").execute(&pool).await.unwrap();
            sqlx::query("DROP TABLE projects").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let pool = open(&file).await.expect("a half-migrated projects table does not brick the daemon");
        let p = project(&pool, "p1").await.unwrap().expect("the project row survived");
        assert_eq!(p.label, "p");
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An *empty* leftover scratch table (the rebuild died before the INSERT) is just dropped.
    #[tokio::test]
    async fn an_empty_scratch_table_is_dropped() {
        let dir = tmp_dir();
        let file = dir.join("empty-scratch.sqlite3");
        old_file_with_turns(&file, 2).await;
        {
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display())).unwrap();
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
            sqlx::query("CREATE TABLE turns_new AS SELECT * FROM turns WHERE 0").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let pool = open(&file).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 2);
        let stale: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name='turns_new'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stale, 0);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When both the real table and the scratch copy hold rows, refuse rather than guess.
    #[tokio::test]
    async fn two_populated_copies_refuse_to_open() {
        let dir = tmp_dir();
        let file = dir.join("ambiguous.sqlite3");
        old_file_with_turns(&file, 2).await;
        {
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display())).unwrap();
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
            sqlx::query("CREATE TABLE turns_new AS SELECT * FROM turns").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let err = match open(&file).await {
            Ok(_) => panic!("must not pick one copy silently"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("turns_new"), "error names the scratch table: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rebuild whose middle step fails leaves the old table exactly as it was (the whole
    /// dance is one transaction) and the connection with its pragmas restored.
    #[tokio::test]
    async fn a_failing_rebuild_rolls_back_and_restores_pragmas() {
        let dir = tmp_dir();
        let file = dir.join("rollback.sqlite3");
        old_file_with_turns(&file, 2).await;
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display())).unwrap().foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let err = rebuild_table(
            &mut conn,
            "turns",
            &[
                "CREATE TABLE turns_new AS SELECT * FROM turns",
                "DROP TABLE turns",
                "THIS IS NOT SQL",
                "ALTER TABLE turns_new RENAME TO turns",
            ],
        )
        .await
        .expect_err("the bad statement fails the rebuild");
        assert!(format!("{err:#}").contains("migrate turns"), "{err:#}");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&mut *conn).await.unwrap();
        assert_eq!(n, 2, "DROP TABLE turns was rolled back");
        let stale: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name='turns_new'")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(stale, 0, "the scratch table was rolled back too");
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys").fetch_one(&mut *conn).await.unwrap();
        assert_eq!(fk, 1, "foreign_keys is back on after the failure");
        drop(conn);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file written by the first cut of this branch has `team_events` without `seq`.
    /// The upgrade must add the column, number the existing rows by their old (`id`) order
    /// and only then put the UNIQUE index on — the index cannot live in SCHEMA, which runs
    /// before the ALTER on an existing table.
    #[tokio::test]
    async fn team_events_gains_its_sequence_and_backfills() {
        let dir = tmp_dir();
        let file = dir.join("pre-seq.sqlite3");
        {
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display()))
                .unwrap()
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
            for stmt in [
                "CREATE TABLE projects (id TEXT PRIMARY KEY, path TEXT NOT NULL, label TEXT NOT NULL,
                   host TEXT NOT NULL DEFAULT 'local', workspace_id TEXT, deleted_at TEXT, created_at TEXT NOT NULL)",
                "CREATE TABLE teams (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
                   issue_number INTEGER NOT NULL, issue_title TEXT NOT NULL, issue_url TEXT NOT NULL, phase TEXT NOT NULL,
                   pause_reason TEXT, resume_phase TEXT, base_ref TEXT NOT NULL, base_sha TEXT NOT NULL,
                   branch TEXT NOT NULL, worktree_root TEXT NOT NULL, deliver TEXT NOT NULL,
                   supervised INTEGER NOT NULL DEFAULT 0, roles_json TEXT NOT NULL, budget_json TEXT NOT NULL,
                   usage_json TEXT NOT NULL DEFAULT '{}', pr_url TEXT, summary TEXT,
                   created_at TEXT NOT NULL, started_at TEXT, ended_at TEXT)",
                // No `seq`, and the index the first cut created.
                "CREATE TABLE team_events (id TEXT PRIMARY KEY, team_id TEXT NOT NULL REFERENCES teams(id),
                   kind TEXT NOT NULL, from_bot_id TEXT, to_bot_id TEXT, task_id TEXT, turn_id TEXT, status TEXT,
                   payload_json TEXT NOT NULL, created_at TEXT NOT NULL)",
                "CREATE INDEX team_events_team_time ON team_events(team_id, id)",
                "INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p','x')",
                "INSERT INTO teams VALUES ('tmA','p1',7,'t','u','working',NULL,NULL,'HEAD','','b','','branch',0,'{}','{}','{}',NULL,NULL,'x',NULL,NULL)",
                "INSERT INTO teams VALUES ('tmB','p1',8,'t','u','working',NULL,NULL,'HEAD','','b','','branch',0,'{}','{}','{}',NULL,NULL,'x',NULL,NULL)",
                "INSERT INTO team_events VALUES ('01A','tmA','phase',NULL,NULL,NULL,NULL,NULL,'{}','x')",
                "INSERT INTO team_events VALUES ('01B','tmA','note',NULL,NULL,NULL,NULL,NULL,'{}','x')",
                "INSERT INTO team_events VALUES ('01C','tmB','note',NULL,NULL,NULL,NULL,NULL,'{}','x')",
                "INSERT INTO team_events VALUES ('01D','tmA','note',NULL,NULL,NULL,NULL,NULL,'{}','x')",
            ] {
                sqlx::query(stmt).execute(&pool).await.unwrap();
            }
            pool.close().await;
        }

        let pool = open(&file).await.expect("upgrade a pre-seq database");
        assert!(columns(&pool, "team_events").await.contains(&"seq".to_string()));
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT id, seq FROM team_events ORDER BY team_id, seq").fetch_all(&pool).await.unwrap();
        // Numbering is per team, dense from 1, in the old id order.
        assert_eq!(
            rows,
            vec![("01A".into(), 1), ("01B".into(), 2), ("01D".into(), 3), ("01C".into(), 1)]
        );
        // The stale index is gone and the unique one is on.
        let idx: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'team_events%' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(idx, vec!["team_events_pending".to_string(), "team_events_seq".to_string()]);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A queue row can already exist when an older build is upgraded. Preserve the legacy
    /// scalar close stamp on that row so the per-issue close guard is not reset by migration.
    #[tokio::test]
    async fn issue_closed_at_is_backfilled_for_an_existing_queue_row() {
        let dir = tmp_dir();
        let file = dir.join("closed-issue.sqlite3");
        let pool = open(&file).await.unwrap();
        let at = now();
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(&at)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO teams (id, project_id, issue_number, issue_title, issue_url, phase, base_ref, base_sha,
               branch, worktree_root, deliver, roles_json, budget_json, issue_closed_at, created_at)
             VALUES ('tm1','p1',42,'done','https://example.invalid/42','done','HEAD','base','team/i42-x','/tmp/team',
                     'branch','{}','{}',?,?)",
        )
        .bind(&at)
        .bind(&at)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO team_issues (id, team_id, seq, issue_number, issue_title, issue_url, state,
               branch, base_sha, created_at)
             VALUES ('tm1-i1','tm1',1,42,'done','https://example.invalid/42','done','team/i42-x','base',?)",
        )
        .bind(&at)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let pool = open(&file).await.unwrap();
        let stamp: Option<String> = sqlx::query_scalar("SELECT issue_closed_at FROM team_issues WHERE id='tm1-i1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stamp.as_deref(), Some(at.as_str()));
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn seed(pool: &SqlitePool) {
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(now())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, managed_by, team_id, team_role, cwd, hook_token, created_at)
             VALUES ('w1','p1','i9-dev-1','claude','team','tm1','worker','/tmp/p','tok',?)",
        )
        .bind(now())
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO teams (id, project_id, issue_number, issue_title, issue_url, phase, base_ref, base_sha,
               branch, worktree_root, deliver, supervised, roles_json, budget_json, usage_json, created_at)
             VALUES ('tm1','p1',9,'t','u','working','HEAD','','team/i9-x','','branch',0,'{}','{}','{}',?)",
        )
        .bind(now())
        .execute(pool)
        .await
        .unwrap();
    }

    /// The team tables behave: FK to the project, per-team task ordering, the
    /// "one open task per worker" unique index, and the `live_teams` phase filter.
    #[tokio::test]
    async fn team_rows_and_indexes() {
        let dir = tmp_dir();
        let pool = open(&dir.join("t.sqlite3")).await.unwrap();
        seed(&pool).await;

        assert_eq!(teams_of_project(&pool, "p1").await.unwrap().len(), 1);
        assert_eq!(live_teams(&pool).await.unwrap().len(), 1);
        assert_eq!(team_members(&pool, "tm1").await.unwrap()[0].name, "i9-dev-1");

        let insert_task = |id: &'static str, seq: i64, state: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO team_tasks (id, team_id, seq, title, brief, worker_bot_id, branch, state, created_at, updated_at)
                     VALUES (?,'tm1',?,'t','b','w1','br',?,?,?)",
                )
                .bind(id)
                .bind(seq)
                .bind(state)
                .bind(now())
                .bind(now())
                .execute(&pool)
                .await
            }
        };
        insert_task("t1", 1, "working").await.unwrap();
        // §4.5: a worker may only have one task that is not in a terminal state.
        assert!(insert_task("t2", 2, "queued").await.is_err(), "second open task for the same worker");
        // …but once the first one is merged, the next may be dispatched.
        sqlx::query("UPDATE team_tasks SET state='merged' WHERE id='t1'").execute(&pool).await.unwrap();
        insert_task("t2", 2, "queued").await.unwrap();
        assert_eq!(team_tasks(&pool, "tm1").await.unwrap().iter().map(|t| t.seq).collect::<Vec<_>>(), vec![1, 2]);
        // §2.1: `(team_id, seq)` is unique.
        assert!(insert_task("t3", 2, "skipped").await.is_err(), "duplicate seq");

        for (id, seq) in [("e1", 1), ("e2", 2)] {
            sqlx::query(
                "INSERT INTO team_events (id, team_id, seq, kind, to_bot_id, status, payload_json, created_at)
                 VALUES (?,'tm1',?,'relay','w1','pending','{\"a\":1}',?)",
            )
            .bind(id)
            .bind(seq)
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
        }
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM team_events WHERE team_id='tm1' AND status='pending'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 2);
        // §10.4: `seq` is unique per team, so the log can never have two "same position" rows.
        assert!(sqlx::query(
            "INSERT INTO team_events (id, team_id, seq, kind, payload_json, created_at)
             VALUES ('e3','tm1',2,'note','{}','x')"
        )
        .execute(&pool)
        .await
        .is_err());

        // A finished team drops out of `live_teams` but stays on the project.
        sqlx::query("UPDATE teams SET phase='done' WHERE id='tm1'").execute(&pool).await.unwrap();
        assert!(live_teams(&pool).await.unwrap().is_empty());
        assert_eq!(teams_of_project(&pool, "p1").await.unwrap().len(), 1);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A database written before `runs.tab_id` existed — which is every live install right
    /// now — must open, keep its runs, and gain the column. This project has already been
    /// bitten once by a column added to `SCHEMA` without the matching additive `ALTER`: the
    /// `CREATE TABLE IF NOT EXISTS` is a no-op on an existing file, so the daemon starts and
    /// then fails on the first query that names the column.
    #[tokio::test]
    async fn a_database_without_runs_tab_id_gains_it_and_keeps_its_runs() {
        let dir = tmp_dir();
        let file = dir.join("pre-tab.sqlite3");
        {
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display()))
                .unwrap()
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
            for stmt in OLD_SCHEMA.split(";\n") {
                let s = stmt.trim();
                if !s.is_empty() {
                    sqlx::query(s).execute(&pool).await.unwrap();
                }
            }
            assert!(!columns(&pool, "runs").await.contains(&"tab_id".to_string()), "precondition");
            sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
                .bind(now())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','old','claude','tok',?)",
            )
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
            // A bot that was running when the daemon was upgraded: split into a shared tab,
            // so there is nothing to put in the new column.
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, started_at)
                 VALUES ('r1','b1','running','working','w8','w8:p3',?)",
            )
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let pool = open(&file).await.expect("an old database opens");

        assert!(columns(&pool, "runs").await.contains(&"tab_id".to_string()), "runs.tab_id was added");
        // The first query the daemon runs after `open` reads `runs` through the `Run` struct,
        // which now names `tab_id` — the exact shape of the failure this test exists for.
        let r = active_run(&pool, "b1").await.expect("active_run right after the migration").expect("still active");
        assert_eq!(r.id, "r1");
        assert_eq!(r.state, "running");
        assert_eq!(r.agent_status, "working");
        assert_eq!(r.pane_id.as_deref(), Some("w8:p3"), "the pane mapping survived");
        assert_eq!(r.tab_id, None, "an old split run simply has no tab of its own");

        // And the column is writable, so the reconcile can backfill it.
        sqlx::query("UPDATE runs SET tab_id='w8:t1' WHERE id='r1'").execute(&pool).await.unwrap();
        assert_eq!(active_run(&pool, "b1").await.unwrap().unwrap().tab_id.as_deref(), Some("w8:t1"));

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

