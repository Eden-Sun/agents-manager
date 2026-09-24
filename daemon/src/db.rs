//! SQLite storage (SPEC appendix C).

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::collections::BTreeSet;
use std::path::Path;
use std::str::FromStr;

mod schema_guard;

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
  -- claude only（issue #213）：讀哪份專案指示檔（`agents-md` plugin 的 `instructionFiles`）。NULL＝daemon 釘的預設 `claude-md`。
  instruction_files TEXT,
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
  -- 這個 run 用哪個身分起來的（issue #238）：pane 裡實際的帳號。'' ＝沒有身分（預設帳號）；NULL＝沒記（不是 daemon 起的、
  -- 或加這一欄之前的舊列），退回 bot 設定的身分。PATCH 改身分要重啟才生效，這段時間額度要記在這個身分上。
  runtime_identity TEXT,
  -- Native session requested by a `resume_native` start. Cleared by the first identity/turn hook.
  resume_session_id TEXT,
  -- 那一次 `resume_native` 的結論（issue #92）：`verified`（回報的就是要接的那段）、`mismatch`（CLI 開了
  -- 新對話）、`unverified`（等滿 `resume_gate::VERIFY_WINDOW` 都沒回報，刻意放行）。NULL＝沒要求接回、或還在等。
  resume_outcome TEXT,
  -- The agent's terminal title, its statusLine output / payload, a pending claude update and
  -- the error that cut the last turn short. All belong to this CLI process, so a restart
  -- starts from NULL.
  agent_title TEXT, status_line TEXT, status_json TEXT, update_notice TEXT, turn_error TEXT,
  -- `agent_status` 最後一次**真的改變**的時間（下面的 trigger 蓋；同值重寫不算改變）。前端拿它算
  -- 「跑了多久」，不再自己用本地時鐘瞎猜起點（issue #93）。NULL＝這個 run 還沒真的變過狀態，或是
  -- 升級前的舊列——那種前端退回自己觀察到的時間，並標成「不是 daemon 的紀錄」。
  agent_status_since TEXT,
  -- claude 原生 SubagentStart／SubagentStop 的最後一筆快照（issue #82）：純輔助可見性，不影響
  -- §6.5a 的血緣認領——child 本來就沒有 hook（§4.3），這一欄只會有頂層 bot 自己（in-process
  -- Task 工具）的紀錄。`{"event","agent_id","agent_type","transcript_path","at"}`，一律整筆覆蓋。
  subagent_json TEXT
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
  -- 'staging'：row 先落地、位元組還在寫；'ready'：可以 resolve/bind/read；'failed'：save() 自己標的，
  -- best-effort cleanup 可能沒清乾淨。只有 'ready' 能被 resolve/bind/read；'staging'／'failed' 由
  -- `attach::reconcile_orphans`（開機跑一次）收掉（issue #88）。
  state TEXT NOT NULL DEFAULT 'ready' CHECK (state IN ('staging','ready','failed')),
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS attachments_msg ON attachments(message_id);
-- issue #94：一顆 bot 自己的 Bash 工具跑 `herdr pane split`／`agent start`，那條指令的 stdout 就是
-- herdr 自己回的 JSON——直接告訴 daemon「這個 pane_id 是我剛剛開的」，比 §6.5a 的同 tab／名字前綴推斷
-- 更早、更精確。`reconcile::adopt_child` 認領前先查這裡；查不到才退回原本的血緣／前綴推斷（見
-- `daemon/src/spawn_hints.rs`）。`pane_id` 在同一個 herdr session 裡唯一，一顆 pane 只會被合法建立一次。
CREATE TABLE IF NOT EXISTS spawn_hints (
  pane_id TEXT PRIMARY KEY, host TEXT NOT NULL, bot_id TEXT NOT NULL REFERENCES bots(id),
  created_at TEXT NOT NULL
);
-- 預覽模式（issue #253）：頂層 bot 的專案起的 vite dev server。一顆 bot 一列；`status` 是 off／starting／running／failed，
-- `pane_id` 是放在該 bot 那個 tab 裡的 service pane，`off` 時是 NULL、`port` 也不再算被佔用。
CREATE TABLE IF NOT EXISTS bot_previews (
  bot_id TEXT PRIMARY KEY REFERENCES bots(id), host TEXT NOT NULL, pane_id TEXT, port INTEGER, dir TEXT,
  status TEXT NOT NULL, error TEXT, started_at TEXT, updated_at TEXT NOT NULL
);
-- 持久 intent（#355）：多步驟動作「已承諾、可能只做了一半」的紀錄，daemon 中途死掉後開機由 `intents` 模組接續。
-- 一列＝一件動作；同一目標同一種動作同時只能有一件開著（partial unique index）。見 `intents.rs`。
CREATE TABLE IF NOT EXISTS intents (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, subject_id TEXT NOT NULL, host TEXT NOT NULL DEFAULT 'local',
  payload_json TEXT NOT NULL DEFAULT '{}', step TEXT,
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','running','done','failed','abandoned')),
  owner_boot TEXT, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL, expires_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS intents_one_open ON intents(kind, subject_id) WHERE status IN ('pending','running');
-- 額度最後一次探測結果（issue #392）；daemon 重啟後先顯示這份，API 會標成 stale。
CREATE TABLE IF NOT EXISTS quota_cache (
  key TEXT PRIMARY KEY,
  quota_json TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
"#;

/// 這個 binary 認得的 schema 版本，存在 SQLite 內建的 `PRAGMA user_version`（跟資料庫檔案綁在一起，
/// 讀寫都在同一個交易裡，不像 `journal_mode` 那類 pragma 有「不能包進交易」的限制）。
///
/// **它是「最低相容 binary」的圍籬，不是 migration 帳本**（issue #72）：DB 記著 N，代表 `SCHEMA_VERSION`
/// 至少是 N 的 binary 才准開它，更舊的在 `migrate` 一開頭就拒絕，一個 DDL 都不碰（`daemon-update-kick.sh`
/// 那類滾動升級卡在舊 binary、或手動回滾時，不會拿過期的假設去讀、甚至用 `sync_trigger` 把守衛換回舊規則）。
/// 它不記錄「跑過哪幾號 migration」：`SCHEMA`／ALTER 名單／各子模組的 migrate 全是 `IF NOT EXISTS`／
/// `has_column` 檢查過的冪等操作，每次開 DB 全部重跑，實際長相由 [`schema_guard::check_drift`] 對著全新 DB 的
/// 標準答案核對。等真的出現非 additive 的資料轉換（改欄位型別、拆表、改約束得重建表），再引入照順序執行的
/// migration 框架。
///
/// **什麼時候升**：migrate 建出來的任何 schema 物件變了就升——不管是 `SCHEMA`、ALTER 名單還是哪個子模組，
/// 表、欄位、型別、預設值、約束、索引、trigger 內容（含由轉移表產生的守衛）都算，排版與註解不算。不靠人記：
/// `schema_guard` 的測試拿全新 DB 的指紋跟這裡最後一行比，對不上就紅，錯誤訊息會給出要加的那一行。
/// **只准在最後加一行，不准改既有的**——版本號沒動，舊 binary 就會照開它不懂的 schema。
///
/// 1～4 版在指紋之前。4 版之後又改過子模組 schema 卻沒升（`supervisor_approvals.request_reason`、
/// `release_triage` 表），同樣記著 4 的 DB 長相不一，所以從 5 開始釘。
const SCHEMA_HISTORY: &[(i64, &str)] = &[
    (5, "519fd4f808b8ac5b"),
    // issue #213：`bots.instruction_files`（claude bot 讀哪份專案指示檔）。
    (6, "f75ac921663299da"),
    // issue #238：`runs.runtime_identity`（run 用哪個身分起來的，額度記在它上面）。
    (7, "7372788a43638544"),
    // issue #240：`judge_shadow`（撞限第二意見的帳本，只記錄）。
    (8, "3b7f1d3c6f8712de"),
    // issue #253：`bot_previews`（頂層 bot 的 vite 預覽）。
    (9, "be0411b44c5111ab"),
    // issue #253 v2：`bot_previews.source`／`pid`（預覽可以接上既有的 vite）。
    (10, "d17c9db38b9c22e7"),
    // issue #253 v4：`bot_previews.command`／`kind`（預覽擴大成本機 dev server）。
    (11, "b2704fdf9aeb332c"),
    // issue #344：`bots.primary_position`（主力那列的固定順序）。
    (12, "dd16404b83c0823f"),
    // issue #355：`intents`（持久 intent）、`bots.launch_rev`／`runs.launch_rev`（啟動版本雜湊）；同版含 4cbacecc（#349）的 `remote_bot_dir_purges`（那顆沒升版）。
    (13, "44c2487afdf01452"),
    // issue #392：`quota_cache`（重啟後先顯示上一次的額度讀數）。
    (14, "0cb16547e439691a"),
    // issue #400：data-only model rewrite, so the schema fingerprint stays the same; includes deleted rows.
    (15, "0cb16547e439691a"),
    // issue #405：`messages.rewound_at`（對話倒回標掉的訊息，標記不刪）。
    (16, "cd536b2d4cda75e5"),
    // issue #339：`messages.relay_unverified`（沒帶 bot token 自稱的 relay_from）。
    (17, "b9b1eb7d09e3cb5b"),
    // issue #436：`supervisor_approvals.requester_unverified`（申請人是自稱的還是驗過的）。
    (18, "5df2ad9350704345"),
    // issue #474：pre-v2 的 `bot_previews` 重建補回 `REFERENCES bots(id)`。全新 DB 本來就有那個外鍵，
    // 所以指紋跟 v18 一樣（同 #400 那一版的情形）——升版是因為 migrate 真的動了既有資料庫。
    (19, "5df2ad9350704345"),
];
pub const SCHEMA_VERSION: i64 = SCHEMA_HISTORY[SCHEMA_HISTORY.len() - 1].0;

/// 裝一個 trigger，DB 裡那一份跟 `ddl` 不同就換掉（issue #186）。
///
/// 守衛的內容是由轉移表產生的（`turn_controller::guard_ddl`、`assignment_state::guard_ddl`）。以前用
/// `CREATE TRIGGER IF NOT EXISTS`：只有第一次建得進去，之後轉移表改了（新增或拿掉一條合法邊），舊 DB 裡那一份已經存在，
/// 守衛就永遠停在舊規則——新的合法轉移被擋下、拿掉的照樣放行。這裡每次開 DB 都拿 `sqlite_master.sql`（SQLite 存的是去掉
/// `IF NOT EXISTS` 的原文）跟現在的 DDL 比，不同才 DROP 再建，舊 DB 不必等人手動重建。呼叫端要在同一個交易裡：
/// 換到一半失敗整批回滾，不會留下一段沒有守衛的空窗。守衛內容變了一樣要升 [`SCHEMA_VERSION`]（issue #72）：
/// 不然舊 binary 開到這個 DB，會照它自己的轉移表把守衛換回舊規則。
pub(crate) async fn sync_trigger(conn: &mut sqlx::SqliteConnection, name: &str, ddl: &str) -> Result<()> {
    let want = ddl.trim().replacen("CREATE TRIGGER IF NOT EXISTS ", "CREATE TRIGGER ", 1);
    let have: Option<Option<String>> = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = ?")
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?;
    if have.flatten().as_deref() == Some(want.as_str()) {
        return Ok(());
    }
    sqlx::query(&format!("DROP TRIGGER IF EXISTS {name}")).execute(&mut *conn).await?;
    sqlx::query(&want).execute(&mut *conn).await.with_context(|| format!("create trigger {name}"))?;
    Ok(())
}

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

/// 套用全部 schema，最後對一次帳（[`schema_guard::check_drift`]）。
async fn migrate(pool: &SqlitePool) -> Result<()> {
    apply_migrations(pool).await?;
    schema_guard::check_drift(pool).await?;
    // 版本戳記最後才蓋（#289）：子模組 migrate 或漂移核對失敗時 daemon 起不來，這時 DB 不能已經宣稱是新版，
    // 否則回滾用的舊 binary 會被版本閘（#72）擋在門外。
    let stored: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(pool).await?;
    if stored < SCHEMA_VERSION {
        // `user_version` 不接受 bind 參數，但這裡的值是編譯期常數，不是外部輸入。
        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}")).execute(pool).await.context("stamp schema version")?;
    }
    Ok(())
}

/// Only the current schema is supported: older databases are not upgraded. Leftover tables of
/// removed features (`teams`, `team_*`) may exist; nothing reads them.
///
/// SCHEMA 與下面的 additive ALTER 包在同一個 transaction 裡：中途失敗（例如舊庫留下的資料
/// 違反新加的 UNIQUE INDEX）要整批回滾，不留半套 schema，重跑才會從乾淨的起點重新開始
/// （issue #58）。裡面全是 SQLite 認得在交易內執行的 DDL（`CREATE TABLE`／`CREATE INDEX`／
/// `ALTER TABLE ADD COLUMN`），沒有 `VACUUM`／`ATTACH`／`PRAGMA journal_mode` 這類定義上就
/// 不能包進交易的動作。下面各子模組自己的 `migrate`（`supervisor::store`、`read_marks`、
/// `panes`、`herdr_maintenance`、`mission::store`）不在這個交易裡：它們各自維護自己那張表，
/// 風險最高的「加欄＋回填」已經各自包了自己的 transaction（見 `read_marks::migrate`、
/// `supervisor::roles::migrate` 的 `claimed_by`），要把全部子模組併進同一個跨檔案交易得先把
/// 它們的 `&SqlitePool` 簽名都換成共用的連線／交易 handle，範圍超出這張 issue，先不動。
///
/// 進交易之前先比對 [`SCHEMA_VERSION`]：資料庫記的版本比這顆 binary 認得的還新，代表有更新版的
/// binary 已經動過這個檔案——直接拒絕，一個 SCHEMA／ALTER 都不碰，不要拿舊的欄位假設去讀一個看
/// 不懂的資料庫（issue #72）。版本戳記由 [`migrate`] 在全部子模組 migrate 與漂移核對通過之後才蓋，中途失敗時
/// 版本號維持原樣，不能宣稱「已經是這個版本」卻沒有真的套用成功。
///
/// 不含最後的漂移核對：`schema_guard` 拿它在全新的 in-memory DB 上跑一次，當 schema 的標準答案。
async fn apply_migrations(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin().await?;
    let stored_version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&mut *tx).await?;
    anyhow::ensure!(
        stored_version <= SCHEMA_VERSION,
        "資料庫的 schema 版本是 {stored_version}，這顆 daemon 只認得到 {SCHEMA_VERSION}（比較舊）。\
         代表已經有更新版的 daemon 動過這個檔案；請先把這顆升級到那個版本以上，不要用舊版繼續開它。"
    );
    for stmt in SCHEMA.split(";\n") {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        sqlx::query(s).execute(&mut *tx).await.with_context(|| format!("apply schema: {s}"))?;
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
        // 能不能自動重送，與「有沒有證據」分開（AGM 2026-09-16）。既有列給 1＝維持今天的行為：
        // 舊的 unverified 列早就把 resend_count 頂到上限，照樣不會被重送。
        ("turns", "auto_resend", "ALTER TABLE turns ADD COLUMN auto_resend INTEGER NOT NULL DEFAULT 1"),
        ("turns", "rollout_waits", "ALTER TABLE turns ADD COLUMN rollout_waits INTEGER NOT NULL DEFAULT 0"),
        ("turns", "rollout_wait_key", "ALTER TABLE turns ADD COLUMN rollout_wait_key TEXT"),
        // 第一次記下送達結果的時間（`prompt::mark_delivery`）。排隊的 turn 的 `created_at` 是**排進佇列**的時間，
        // 重啟補 stall watchdog 要看的是「剛送出」，不是「剛排隊」（review 2026-09-16 deliv L3）。舊列 NULL＝退回 created_at。
        ("turns", "delivered_at", "ALTER TABLE turns ADD COLUMN delivered_at TEXT"),
        // 排著的這一則被 flush 的額度閘擋下時看到的撞限（issue #108，`lifecycle::quota_hold`，JSON）。
        // `app.quotas` 只在記憶體：沒有這一欄，重啟後開機叫醒的 flush 會把它送進同一個還沒額度的身分。
        ("turns", "quota_hold", "ALTER TABLE turns ADD COLUMN quota_hold TEXT"),
        // 送出時 bot 沒在跑、daemon 先收下再替它啟動的那一則（issue #122，`lifecycle::start_send`）：
        // 沒有 run 也不當孤兒撤，啟動失敗只記原因（`start_error`），等使用者重新啟動或取消。
        ("turns", "awaits_start", "ALTER TABLE turns ADD COLUMN awaits_start INTEGER NOT NULL DEFAULT 0"),
        ("turns", "start_error", "ALTER TABLE turns ADD COLUMN start_error TEXT"),
        // 舊庫裡的每一列都是舊流程「先寫檔、DB insert 最後做」留下來的——insert 成功就代表檔案已經寫完，
        // 一律當 'ready'（issue #88）。
        (
            "attachments",
            "state",
            "ALTER TABLE attachments ADD COLUMN state TEXT NOT NULL DEFAULT 'ready' CHECK (state IN ('staging','ready','failed'))",
        ),
        // `agent_status` 最後一次真的改變的時間；見下面 `runs_agent_status_since` trigger 與 issue #93。
        ("runs", "agent_status_since", "ALTER TABLE runs ADD COLUMN agent_status_since TEXT"),
        // claude 原生 SubagentStart／SubagentStop 的最後一筆快照，純輔助可見性（issue #82）。
        ("runs", "subagent_json", "ALTER TABLE runs ADD COLUMN subagent_json TEXT"),
        // `resume_native` 的結論（issue #92）；見 SCHEMA 那一欄的說明與 `lifecycle::resume_gate`。
        ("runs", "resume_outcome", "ALTER TABLE runs ADD COLUMN resume_outcome TEXT"),
        // claude bot 讀哪份專案指示檔（issue #213）；舊列 NULL＝釘在 `claude-md`，跟加這一欄之前一樣。
        ("bots", "instruction_files", "ALTER TABLE bots ADD COLUMN instruction_files TEXT"),
        // run 用哪個身分起來的（issue #238）；舊列 NULL＝沒記，額度照 bot 設定的身分算，跟加這一欄之前一樣。
        ("runs", "runtime_identity", "ALTER TABLE runs ADD COLUMN runtime_identity TEXT"),
        // 預覽（issue #253 v2）：`spawned`（自己起的）／`attached`（接上既有的 vite）與被接上的 pid；舊列都是 spawned。
        ("bot_previews", "source", "ALTER TABLE bot_previews ADD COLUMN source TEXT NOT NULL DEFAULT 'spawned'"),
        ("bot_previews", "pid", "ALTER TABLE bot_previews ADD COLUMN pid INTEGER"),
        // 預覽擴大成本機 dev server（issue #253 v4）：實際跑的那一行與 dev server 種類。
        ("bot_previews", "command", "ALTER TABLE bot_previews ADD COLUMN command TEXT"),
        ("bot_previews", "kind", "ALTER TABLE bot_previews ADD COLUMN kind TEXT"),
        // 主力那列的固定順序（issue #344）；舊列 0＝沒排過，排在有排過的之後。
        ("bots", "primary_position", "ALTER TABLE bots ADD COLUMN primary_position INTEGER NOT NULL DEFAULT 0"),
        // 對話倒回（issue #405）：被倒掉的訊息標記不刪；舊列 NULL＝沒被倒掉。
        ("messages", "rewound_at", "ALTER TABLE messages ADD COLUMN rewound_at TEXT"),
        // 啟動相關設定的版本雜湊（#355 機制 B／#353）：`bots.launch_rev`＝目前設定的版本，`runs.launch_rev`＝這個 run 啟動時載入的版本；
        // NULL＝沒記（舊資料，視為相同、不誤報「需重啟」）。P1 只加欄位、沒有人讀寫。
        ("bots", "launch_rev", "ALTER TABLE bots ADD COLUMN launch_rev TEXT"),
        ("runs", "launch_rev", "ALTER TABLE runs ADD COLUMN launch_rev TEXT"),
        // issue #339：`relay_from` 是呼叫端自稱、沒帶自己的 bot token 證明（相容期）＝1。舊列 0＝不是這條路寫的。
        ("messages", "relay_unverified", "ALTER TABLE messages ADD COLUMN relay_unverified INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !has_column(&mut *tx, table, col).await? {
            sqlx::query(ddl).execute(&mut *tx).await.with_context(|| format!("add {table}.{col}"))?;
        }
    }
    // issue #474：pre-v2 建的 `bot_previews` 沒有 `REFERENCES bots(id)`——`CREATE TABLE IF NOT EXISTS`
    // 對既有 DB 是 no-op，而外鍵用 `ALTER TABLE` 加不回去，所以那種資料庫到今天都還缺這個約束
    // （`pragma_table_info` 看不到 FK，#470 之前漂移核對也不比表的約束，所以一直沒人發現）。
    // 唯一補得回來的做法是重建表。
    rebuild_bot_previews_fk(&mut tx).await?;
    // 建在這裡（column 一定已經存在之後），不是跟著上面的 SCHEMA 一起用 `;\n` 切開來送：這句 trigger
    // body 自己就帶了分號，切开來就會斷成兩句送不出去。寫 `agent_status` 的地方有好幾處（events／
    // reconcile／default session／bulk_restart／stuck_turns…），用 trigger 而不是在每一處補一行：
    // 漏掉一處就會讓「起點」在那條路徑上悄悄跟丟（issue #93）。
    sync_trigger(
        &mut tx,
        "runs_agent_status_since",
        "CREATE TRIGGER runs_agent_status_since AFTER UPDATE OF agent_status ON runs
           WHEN OLD.agent_status IS NOT NEW.agent_status
         BEGIN
           UPDATE runs SET agent_status_since = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = NEW.id;
         END",
    )
    .await
    .context("create runs_agent_status_since trigger")?;
    // Data-only migration; exact comparisons preserve explicitly versioned Claude ids.
    sqlx::query("UPDATE bots SET model='gpt-6-luna' WHERE kind='codex' AND model='gpt-5.6-luna'")
        .execute(&mut *tx).await.context("remap retired Codex model")?;
    sqlx::query("UPDATE bots SET model='claude-opus-5-5' WHERE kind='claude' AND model='opus'")
        .execute(&mut *tx).await.context("remap retired Claude alias")?;
    // Turn 狀態轉移的單一權威（issue #68）：合法邊只定義在 `lifecycle::turn_controller::LEGAL_EDGES`，
    // trigger 由它生成。二十來處 `UPDATE turns SET status` 各自帶的 CAS guard 照舊，這是它們的下限，
    // 而且未來新寫的路徑繞不過去——終局的回合不可能被改回進行中。
    crate::lifecycle::turn_controller::install_guard(&mut tx).await.context("create turns_status_transition trigger")?;
    tx.commit().await?;
    crate::supervisor::store::migrate(pool).await?;
    crate::read_marks::migrate(pool).await?;
    crate::fork_ops::migrate(pool).await?;
    crate::remote_purge::migrate(pool).await?;
    crate::panes::migrate(pool).await?;
    crate::herdr_maintenance::migrate(pool).await?;
    crate::mission::store::migrate(pool).await?;
    crate::hook_inbox::migrate(pool).await?;
    crate::build_scheduler::migrate(pool).await?;
    crate::release_triage::ledger::migrate(pool).await?;
    crate::judge::migrate(pool).await?;
    Ok(())
}

/// 把缺了 `REFERENCES bots(id)` 的 `bot_previews` 重建出來（issue #474）。
///
/// **只動缺 FK 的那種資料庫**：`pragma_foreign_key_list` 有東西就直接回來，所以全新 DB 與已經補過的
/// 都不會被碰，這支跑第二次是 no-op。
///
/// 欄位定義是從 `pragma_table_info` 現場讀出來重組的，不是寫死一份：這支在上面那串 additive ALTER
/// **之後**才跑，所以這時 `bot_previews` 已經有 `source`／`pid`／`command`／`kind`。寫死一份 DDL 的話，
/// 以後有人再加一欄就會在這裡悄悄把它的資料丟掉。（`bot_previews` 的欄位都沒有 `CHECK`，所以
/// pragma 讀得到的四樣——型別、NOT NULL、預設值、主鍵——就是全部；有 CHECK 的表不能照抄這個做法。）
///
/// 孤兒列（`bot_id` 指不到任何 bot）會被丟掉並記一行 warn：新表帶著 FK，而 `db::open` 開著
/// `foreign_keys`，不先濾掉的話 `INSERT … SELECT` 會整批失敗、整個 migrate 回滾、daemon 起不來。
/// 預覽是執行期狀態（一顆跑著的 vite），指不到 bot 的那幾列本來就沒有人會再用到。
async fn rebuild_bot_previews_fk(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    let fks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_list('bot_previews')")
        .fetch_one(&mut *conn)
        .await
        .context("read bot_previews foreign keys")?;
    if fks > 0 {
        return Ok(());
    }
    let cols: Vec<(String, String, i64, Option<String>, i64)> =
        sqlx::query_as("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info('bot_previews') ORDER BY cid")
            .fetch_all(&mut *conn)
            .await
            .context("read bot_previews columns")?;
    if cols.is_empty() {
        return Ok(());
    }
    let mut defs = Vec::with_capacity(cols.len());
    let mut names = Vec::with_capacity(cols.len());
    for (name, ty, notnull, dflt, pk) in &cols {
        let mut def = format!("{name} {ty}");
        if *pk > 0 {
            def.push_str(" PRIMARY KEY");
        }
        if *notnull != 0 && *pk == 0 {
            def.push_str(" NOT NULL");
        }
        if let Some(d) = dflt {
            def.push_str(&format!(" DEFAULT {d}"));
        }
        if name == "bot_id" {
            def.push_str(" REFERENCES bots(id)");
        }
        defs.push(def);
        names.push(name.as_str());
    }
    // `bot_id IS NULL` 也算孤兒，而且要明寫（i264 審 #474 提的）：`NULL NOT IN (…)` 是 NULL 不是 true，
    // 只寫 `NOT IN` 的話那種列不會被算進來，但下面 `INSERT … WHERE bot_id IN (…)` 一樣會把它濾掉
    // ——列被丟了、warn 卻少報一筆。（`bot_id TEXT PRIMARY KEY` 在 SQLite 是可以為 NULL 的。）
    let orphans: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM bot_previews WHERE bot_id IS NULL OR bot_id NOT IN (SELECT id FROM bots)")
        .fetch_one(&mut *conn)
        .await
        .context("count orphan bot_previews rows")?;
    if orphans > 0 {
        tracing::warn!(orphans, "bot_previews 有指不到 bot 的列；補外鍵時一併丟掉（issue #474）");
    }
    // 這張表自己的索引與 trigger：`DROP TABLE` 會一起帶走，而 `RENAME` 不會還回來（i406 在 #474 提的
    // 前瞻陷阱）。今天 `bot_previews` 一個都沒有，所以現況不會掉東西——但下一個替它加索引的人不會
    // 想到要回來改這裡，而掉了也不會有人立刻發現（少一個索引只是變慢，不會報錯）。所以先收起來、
    // 重建完照原樣建回去。
    //
    // `sql IS NULL` 的不收：那是 PRIMARY KEY／UNIQUE 自動建的 autoindex，新表的約束會自己重建，
    // 照抄反而會撞名。
    let extras: Vec<(String,)> = sqlx::query_as(
        "SELECT sql FROM sqlite_master
          WHERE tbl_name='bot_previews' AND type IN ('index','trigger') AND sql IS NOT NULL
          ORDER BY name",
    )
    .fetch_all(&mut *conn)
    .await
    .context("read bot_previews indexes and triggers")?;
    let list = names.join(", ");
    for stmt in [
        format!("CREATE TABLE bot_previews_new ({})", defs.join(", ")),
        format!("INSERT INTO bot_previews_new ({list}) SELECT {list} FROM bot_previews WHERE bot_id IN (SELECT id FROM bots)"),
        "DROP TABLE bot_previews".to_string(),
        "ALTER TABLE bot_previews_new RENAME TO bot_previews".to_string(),
    ] {
        sqlx::query(&stmt).execute(&mut *conn).await.with_context(|| format!("rebuild bot_previews: {stmt}"))?;
    }
    // 改完名才建：收起來的那幾句 DDL 指名的是 `bot_previews`，在 RENAME 之前建會落到舊名上。
    for (sql,) in &extras {
        sqlx::query(sql).execute(&mut *conn).await.with_context(|| format!("restore bot_previews object: {sql}"))?;
    }
    // 對帳：數量對不上就整個 migrate 失敗、交易回滾（DB 一個字都沒動），不要留一張少了索引的表
    // 讓人以後自己去發現。
    let after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE tbl_name='bot_previews' AND type IN ('index','trigger') AND sql IS NOT NULL",
    )
    .fetch_one(&mut *conn)
    .await
    .context("recount bot_previews indexes and triggers")?;
    anyhow::ensure!(
        after as usize == extras.len(),
        "bot_previews 重建後索引／trigger 數對不上（重建前 {}、重建後 {after}）：交易回滾，沒有動到資料庫",
        extras.len()
    );
    tracing::info!(
        rows_dropped = orphans,
        restored = extras.len(),
        "bot_previews 重建完成，補回 REFERENCES bots(id)（issue #474）"
    );
    Ok(())
}

/// 泛型 executor：`db::migrate` 要在自己的 transaction 裡查（`&mut *tx`），一般呼叫端仍然直接
/// 給 `&SqlitePool`（single-connection 的 migrate pool 同一時間只有一個實體連線，`tx` 開著時
/// 傳 `pool` 進來會卡住等一個永遠不會釋出的連線）。
pub async fn has_column<'e, E>(executor: E, table: &str, col: &str) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let cols: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as(&format!("PRAGMA table_info({table})")).fetch_all(executor).await?;
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
/// the UPDATE is the lock, so a queue flush and the stall watchdog cannot both resend. 寫不進去回錯，
/// 不是「額度用完了」（#193）。
pub async fn claim_resend(pool: &SqlitePool, turn_id: &str, max: i64) -> Result<bool> {
    Ok(sqlx::query("UPDATE turns SET resend_count = resend_count + 1 WHERE id = ? AND resend_count < ?")
        .bind(turn_id)
        .bind(max)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
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

/// 時間戳的**唯一**格式：RFC3339、UTC、固定到毫秒、以 `Z` 結尾（`2026-09-18T07:00:00.000Z`）。
///
/// 固定寬度而且以 `Z` 結尾這件事不是美觀問題——很多地方是拿這些字串**在 SQL 裡直接比大小**的
/// （`... WHERE next_attempt_at <= ?`）。同一種格式下字典序就等於時間序；一旦混進別種寬度，
/// 比較就會失準：`'…T07:00:00Z'` 與 `'…T07:00:00.000Z'` 在字串上不相等，`Z`(0x5A) 還大於 `.`(0x2E)。
/// 真正危險的是帶時區位移的格式（`+08:00`／`-05:00`）——那會讓字典序跟時間序差到**幾小時**。
/// 所以所有時間戳一律走 [`now`] 或 [`iso_in`]，不要各自 `to_rfc3339_opts`（issue #101）。
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 現在起 `secs` 秒後的時間戳，格式同 [`now`]。
///
/// 到期時間（`next_attempt_at`／`resume_at`／`notify_next_at`／`watchdog_next_at`／維護窗口）全部走這支。
/// 以前總管、看門狗、維護、API 各有一份一模一樣、卻寫到**秒**的 `iso_in`，於是
/// `notify_next_at`（秒）跟 `db::now()`（毫秒）在 SQL 裡比大小會差不到一秒（issue #101）。
pub fn iso_in(secs: i64) -> String {
    iso_at(chrono::Utc::now() + chrono::Duration::seconds(secs))
}

/// 把一個時刻寫成 [`now`] 的格式。外面來的時間（CLI 橫幅的重置時刻等）先 parse 再用這支正規化。
pub fn iso_at(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 把 DB 裡讀出來的時間戳解成**時刻**。任何合法 RFC3339 都認（秒／毫秒／微秒、`Z` 或 `+08:00`）。
///
/// **讀取端**的入口：既有資料庫裡有舊版寫的秒格式（`…:00Z`），也有外部來的字串（CLI 回報的重置時間
/// 原樣存下來，`quota::unix_to_rfc3339`）。寫入端統一成 [`now`] 的格式管不到這些，所以到期／先後判斷
/// 一律**解析後比時刻**（[`cmp_ts`]），不拿字串的字典序當時間序（issue #101 重開）。
pub fn parse_ts(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&chrono::Utc))
}

/// 兩個時間戳誰先誰後——比時刻，不比字串。`cmp_ts(deadline, now).is_le()` 就是「到期了」。
///
/// 兩邊都解得開才比時刻；有一邊解不開就退回字串比較（跟過去一樣：不憑空替壞資料編一個時間）。
pub fn cmp_ts(a: &str, b: &str) -> std::cmp::Ordering {
    match (parse_ts(a), parse_ts(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

/// 兩個（可能沒有的）時間戳是不是**同一個時刻**。都沒有算相同；只有一邊有就不同。
///
/// 拿存起來的 `quota_reset_at` 跟新讀到的重置時間比「有沒有變」用的：字串 `==` 會把舊資料的
/// `…:00Z` 與 `…:00.000Z` 當成兩個不同的時間，白白重寫一次狀態、多推一次事件。
pub fn same_instant(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => cmp_ts(a, b).is_eq(),
        _ => false,
    }
}

/// [`cmp_ts`] 的 SQL 版：把時間欄位（或運算式）正規化成 [`now`] 的格式，`WHERE ... <= ?`／`ORDER BY`
/// 才是照時刻比，不是照舊資料的字串寫法比。**綁的那一邊仍要來自 [`now`]／[`iso_at`]。**
///
/// SQLite 的 `strftime` 認秒／毫秒／微秒、`Z` 與 `+08:00`，`%f` 固定吐 `SS.SSS`。解不開的值
/// （`strftime` 回 NULL）退回原字串，行為跟沒包一樣。欄位是 NULL 就還是 NULL，`IS NULL` 的判斷不受影響。
///
/// 代價是這一欄用不上索引：只用在**到期時間**這種筆數很小的欄位（收件匣、租約、核准、交辦），
/// 不用在 `messages`／`turns` 的 `created_at` 這種靠索引分頁的欄位——那些欄位只由 [`now`] 寫，本來就同一種格式。
pub fn ts_sql(col: &str) -> String {
    format!("COALESCE(strftime('%Y-%m-%dT%H:%M:%fZ', {col}), {col})")
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
    /// claude only: `agents-md` plugin's `instructionFiles`. NULL = `config::INSTRUCTION_FILES_DEFAULT`.
    pub instruction_files: Option<String>,
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
    /// 主力那列的固定順序（issue #344）：1 起算，0＝從沒排過；取消釘選不清。純顯示，不進 config.toml。
    pub primary_position: i64,
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
    /// 啟動時的身分（issue #238），解讀見 [`Run::started_identity`]。
    pub runtime_identity: Option<String>,
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
    /// 那一次 `resume_native` 的結論：`verified`／`mismatch`／`unverified`（issue #92，`lifecycle::resume_gate`）。
    pub resume_outcome: Option<String>,
    /// `agent_status` 最後一次真的改變的時間（trigger 蓋，issue #93）；前端算「跑了多久」的起點。
    /// `None` = 還沒真的變過，或升級前的舊列。
    pub agent_status_since: Option<String>,
    /// claude 原生 SubagentStart／SubagentStop 的最後一筆快照（issue #82）。純可見性，`hookrecv` 是
    /// 唯一寫入者；不影響 §6.5a 的血緣認領。
    pub subagent_json: Option<String>,
    /// 這個 run 啟動時載入的啟動設定版本（`launch_rev::of`）；NULL＝沒記（adopt 來的、升版前的舊列），不誤報「需重啟」。
    #[sqlx(default)]
    pub launch_rev: Option<String>,
}

impl Run {
    /// 這個 run 是用哪個身分起來的（issue #238）：`Some(Some(名字))`、`Some(None)`＝沒有身分（預設帳號）、
    /// `None`＝沒記（不是 daemon 起的、或加欄位之前的舊列）——呼叫端退回 bot 設定的身分。
    pub fn started_identity(&self) -> Option<Option<String>> {
        let v = self.runtime_identity.as_deref()?;
        let v = v.trim();
        Some((!v.is_empty()).then(|| v.to_string()))
    }
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
    /// 0 = 沒有無損證據（UI 標「未驗證送達」）。只講證據，不決定重送。
    #[sqlx(default)]
    pub delivery_verified: i64,
    /// 0 = 這一則不做自動重送（打過字但證不明，重送會重複派工）。欄位預設 1；
    /// 真的讀不到這一欄時（理論上不會，migrate 先跑）退成 0＝不重送，寧可少送不要重複送。
    #[sqlx(default)]
    pub auto_resend: i64,
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
    /// 第一次記下送達結果的時間；排隊送出的 turn 靠它判「剛送出」（`created_at` 是排隊的時間）。
    #[sqlx(default)]
    #[serde(skip_serializing)]
    pub delivered_at: Option<String>,
    /// 1 = 送出時 bot 沒在跑，daemon 先收下、再替它啟動（issue #122）。只對 `queued` 有意義。
    #[sqlx(default)]
    pub awaits_start: i64,
    /// 上一次替這一則啟動 bot 失敗（或 run 起來後又結束）的原因；`None`＝沒失敗過或正在重試。
    #[sqlx(default)]
    pub start_error: Option<String>,
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
    /// 1 = `relay_from` 是呼叫端自稱、沒有 bot token 證明（issue #339 相容期）；前端在來源旁標「未驗證」。
    #[sqlx(default)]
    pub relay_unverified: i64,
    pub created_at: String,
    pub updated_at: Option<String>,
    /// 對話倒回（`rewind.rs`）標掉的時間：這一則已經不在 CLI 的對話脈絡裡了。標記不刪，NULL＝還在。
    #[sqlx(default)]
    pub rewound_at: Option<String>,
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
        // 第二鍵用 `rowid`（寫入順序），不是 `id`：`started_at` 只到毫秒，同一顆 bot 快速重啟時
        // 兩個 run 會擠進同一毫秒，ULID 的亂數段那時不保證遞增（issue #100／a4605b2）。
        // 這裡挑錯＝ `--resume` 接回另一段對話（issue #461）。
        "SELECT native_session_id, transcript_path FROM runs
          WHERE bot_id = ? AND ended_at IS NOT NULL AND native_session_id IS NOT NULL
          ORDER BY started_at DESC, rowid DESC LIMIT 1",
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
    // run 實際的身分（issue #238）：記了就用它，沒記才用 bot 設定的。
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT ident FROM (
           SELECT CASE WHEN r.runtime_identity IS NULL THEN b.identity ELSE r.runtime_identity END AS ident
             FROM runs r
             JOIN bots b ON b.id = r.bot_id
             JOIN projects p ON p.id = b.project_id
            WHERE r.state IN ('starting','running','stopping')
              AND p.host = ? AND b.deleted_at IS NULL)
          WHERE ident IS NOT NULL AND TRIM(ident) <> ''",
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
    /// issue #101：時間戳只有**一種**格式，而且那個格式必須讓「字典序＝時間序」。
    ///
    /// 很多判斷是拿這些字串在 SQL 裡直接比大小的，所以這不是風格問題：
    /// 寬度一變（秒 vs 毫秒）同一秒內就會比錯，格式一變成帶位移（`+08:00`）會差到**幾小時**。
    #[test]
    fn every_timestamp_has_the_one_canonical_shape() {
        let samples = [now(), iso_in(0), iso_in(60), iso_in(-60), iso_at(chrono::Utc::now())];
        for s in &samples {
            assert_eq!(s.len(), 24, "固定寬度才能比字串：{s}");
            assert!(s.ends_with('Z'), "一律 UTC 的 Z，不可以是 +08:00 這種：{s}");
            assert_eq!(&s[10..11], "T", "{s}");
            assert_eq!(&s[19..20], ".", "到毫秒：{s}");
            // 真的是這個時間，不是長得像而已。
            chrono::DateTime::parse_from_rfc3339(s).unwrap_or_else(|e| panic!("{s} 解不開：{e}"));
        }
    }

    /// 生產程式碼**只准**在 `db.rs` 決定時間戳格式（issue #101）。
    ///
    /// 上面兩條只證明 `db::` 這幾支對；要是別的模組自己 `to_rfc3339_opts(Secs)`，那兩條照樣綠。
    /// 這一條掃原始碼把那條路堵死——今晚的教訓：守衛沒被測到，跟沒有守衛是一樣的。
    /// `#[cfg(test)]` 之後的不算：測試本來就要造舊格式的資料（`roles.rs` 那條混存測試就是）。
    #[test]
    fn only_db_rs_decides_the_timestamp_format() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);
        let mut strays: Vec<String> = Vec::new();
        for f in files {
            let at = f.strip_prefix(&root).unwrap().display().to_string();
            if at == "db.rs" {
                continue; // 格式就是在這裡定義的
            }
            let src = std::fs::read_to_string(&f).unwrap();
            // 測試區塊以後不管：測試要造舊格式的列才測得到混存。
            let prod = src.split("#[cfg(test)]").next().unwrap_or("");
            for (i, line) in prod.lines().enumerate() {
                // 只擋真的會出事的兩種：
                //  - `Secs`：寬度跟毫秒不一樣，同一秒內字串比就會判錯（本 issue 的病灶）。
                //  - 裸的 `to_rfc3339()`：產出 `+00:00` 而不是 `Z`，字典序跟時間序會差到**幾小時**。
                // 直接寫 `Millis` 的雖然該改用 `db::` 的三支，但寬度是對的、不會判錯，先不擋。
                let bad = line.contains("SecondsFormat::Secs")
                    || (line.contains("to_rfc3339()") && !line.contains("to_rfc3339_opts"));
                if bad {
                    strays.push(format!("{at}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        assert!(
            strays.is_empty(),
            "時間戳一律用 db::now()／db::iso_in()／db::iso_at()（固定寬度、以 Z 結尾）。\n\
             `SecondsFormat::Secs` 同一秒內會判錯；裸的 `to_rfc3339()` 產出 +00:00，字典序會差到幾小時：\n{}",
            strays.join("\n")
        );
    }

    /// 字典序要等於時間序——這是所有 `WHERE ... <= ?` 成立的前提。
    #[test]
    fn lexicographic_order_is_chronological_order() {
        let base = chrono::DateTime::parse_from_rfc3339("2026-09-18T07:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let mut prev = iso_at(base - chrono::Duration::days(400));
        for ms in [1i64, 999, 1_000, 60_000, 3_600_000, 86_400_000] {
            let cur = iso_at(base + chrono::Duration::milliseconds(ms));
            assert!(prev < cur, "{prev} 應該排在 {cur} 前面");
            prev = cur;
        }
        // 跨年、跨月也要成立（補零）。
        assert!(iso_at(base) < iso_at(base + chrono::Duration::days(200)));
    }

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

        assert!(claim_resend(&pool, "t", 1).await.unwrap(), "第一次拿得到");
        assert!(!claim_resend(&pool, "t", 1).await.unwrap(), "額度只有一次");
        refund_resend(&pool, "t").await;
        assert!(claim_resend(&pool, "t", 1).await.unwrap(), "退還之後還有一次");
        refund_resend(&pool, "t").await;
        refund_resend(&pool, "t").await;
        let n: i64 = sqlx::query_scalar("SELECT resend_count FROM turns WHERE id='t'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "退還不會退成負數");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// issue #93：前端算「跑了多久」的起點要用這一欄，不能自己用瀏覽器時鐘瞎猜。重複寫同一個值
    /// （pane 又印了一行一樣的狀態）不能推遲起點；真的變了（包含繞了一圈回到原值）才推進。
    #[tokio::test]
    async fn agent_status_since_only_moves_when_the_status_actually_changes() {
        let dir = tmp_dir();
        let pool = open(&dir.join("t.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,started_at) VALUES ('r','b','running',?)").bind(now()).execute(&pool).await.unwrap();
        let since0: Option<String> = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert_eq!(since0, None, "剛建的 run 還沒真的變過狀態");

        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='r'").execute(&pool).await.unwrap();
        let since1: String = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert!(!since1.is_empty());

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='r'").execute(&pool).await.unwrap();
        let since2: String = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert_eq!(since1, since2, "同值重寫（重複的 pane 狀態行）不算改變，起點不動");

        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id='r'").execute(&pool).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='r'").execute(&pool).await.unwrap();
        let since3: String = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert_ne!(since1, since3, "又轉回 working：這是新的一段連續 working，起點要跟著換");

        let r = sqlx::query_as::<_, Run>("SELECT * FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert_eq!(r.agent_status_since.as_deref(), Some(since3.as_str()), "FromRow 讀得到新欄位");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// issue #88：`attachments.state` 是後補的欄位，舊 DB（沒有這一欄）打開時要補上，而且舊列（都是
    /// 舊流程「檔案寫完才 insert」留下來的，insert 成功就代表已經完整）一律回填成 `'ready'`，不能變成
    /// `NULL` 或別的預設值被 `resolve`/`read`/`bind` 擋掉。
    #[tokio::test]
    async fn an_old_database_gains_attachments_state_and_backfills_ready() {
        let dir = std::env::temp_dir().join(format!("am-attach-state-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)")
                .bind(now())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO attachments (id,bot_id,name,mime,size,local_path,agent_path,host,created_at)
                 VALUES ('a','b','n','image/png',1,'/l','/r','local',?)",
            )
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
            // 做成上一版的形狀：這一欄還不存在。
            sqlx::query("ALTER TABLE attachments DROP COLUMN state").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "attachments", "state").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB 照常開起來");
        assert!(has_column(&pool, "attachments", "state").await.unwrap(), "開的時候補上");
        let state: String = sqlx::query_scalar("SELECT state FROM attachments WHERE id='a'").fetch_one(&pool).await.unwrap();
        assert_eq!(state, "ready", "舊流程 insert 成功就代表檔案已經寫完，回填成 ready 而不是留白");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 升級路徑：trigger 是在 ALTER 名單補完欄位之後才建的，不能反過來（trigger body 引用一個舊
    /// DB 當下還沒有的欄位）。
    #[tokio::test]
    async fn an_old_database_without_the_column_still_gets_a_working_trigger() {
        let dir = std::env::temp_dir().join(format!("am-status-since-upgrade-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,started_at) VALUES ('r','b','running',?)").bind(now()).execute(&pool).await.unwrap();
            // 做成上一版的形狀：欄位跟 trigger 都還不存在。
            sqlx::query("DROP TRIGGER IF EXISTS runs_agent_status_since").execute(&pool).await.unwrap();
            sqlx::query("ALTER TABLE runs DROP COLUMN agent_status_since").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "runs", "agent_status_since").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB（缺欄位也缺 trigger）照常開起來");
        assert!(has_column(&pool, "runs", "agent_status_since").await.unwrap(), "開的時候補上欄位");
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='r'").execute(&pool).await.unwrap();
        let since: Option<String> = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert!(since.is_some(), "trigger 也補上了，不是只有欄位");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// schema 變更（additive）`turns.delivered_at`：沒有這一欄的舊 DB 開起來會補上，舊列是 NULL，`SELECT *` 照樣讀得進 `Turn`。
    #[tokio::test]
    async fn an_old_database_gains_turns_delivered_at_on_open() {
        let dir = std::env::temp_dir().join(format!("am-delivered-at-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
            let conv = conversation_id(&pool, "b").await.unwrap();
            sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES ('t',?,'web','completed','ok',?)")
                .bind(&conv).bind(now()).execute(&pool).await.unwrap();
            // 做成上一版的形狀：這一欄還不存在。
            sqlx::query("ALTER TABLE turns DROP COLUMN delivered_at").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "turns", "delivered_at").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB 照常開起來");
        assert!(has_column(&pool, "turns", "delivered_at").await.unwrap(), "開的時候補上");
        let t = sqlx::query_as::<_, Turn>("SELECT * FROM turns WHERE id='t'").fetch_one(&pool).await.unwrap();
        assert_eq!(t.delivered_at, None, "舊列沒有送出時間：重啟補 watchdog 退回看 created_at");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// schema 變更（additive）`bots.instruction_files`（issue #213）：沒有這一欄的舊 DB 開起來會補上，舊列是 NULL
    /// （＝daemon 釘的 `claude-md`），`SELECT *` 照樣讀得進 `Bot`。少了這條 ALTER，`check_schema_drift` 會讓 daemon 起不來。
    #[tokio::test]
    async fn an_old_database_gains_bots_instruction_files_on_open() {
        let dir = std::env::temp_dir().join(format!("am-instruction-files-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
            // 做成上一版的形狀：這一欄還不存在。
            sqlx::query("ALTER TABLE bots DROP COLUMN instruction_files").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "bots", "instruction_files").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB 照常開起來");
        assert!(has_column(&pool, "bots", "instruction_files").await.unwrap(), "開的時候補上");
        let b = bot(&pool, "b").await.unwrap().expect("舊列還在");
        assert_eq!(b.instruction_files, None, "舊列沒有設定：釘在預設 claude-md");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// schema 變更（additive）`runs.runtime_identity`（issue #238）：沒有這一欄的舊 DB 開起來會補上，舊列是 NULL
    /// （＝沒記，額度照 bot 設定的身分算，跟加這一欄之前一樣），`SELECT *` 照樣讀得進 `Run`。
    #[tokio::test]
    async fn an_old_database_gains_runs_runtime_identity_on_open() {
        let dir = std::env::temp_dir().join(format!("am-runtime-identity-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,identity,created_at) VALUES ('b','p','b','claude','t','cc1',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,started_at) VALUES ('r','b','running',?)").bind(now()).execute(&pool).await.unwrap();
            // 做成上一版的形狀：這一欄還不存在。
            sqlx::query("ALTER TABLE runs DROP COLUMN runtime_identity").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "runs", "runtime_identity").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB 照常開起來");
        assert!(has_column(&pool, "runs", "runtime_identity").await.unwrap(), "開的時候補上");
        let r = active_run(&pool, "b").await.unwrap().expect("舊列還在");
        assert_eq!((r.runtime_identity.clone(), r.started_identity()), (None, None), "舊列沒記：照 bot 設定的身分算");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 「這台有哪些身分在跑」（claude 探測拿它當登入證據）看的是 run 實際的身分（#238）：改了設定還沒重啟的是舊的那個；
    /// run 沒記的照 bot 設定的；起來時沒有身分的不算。
    #[tokio::test]
    async fn live_identities_are_the_ones_the_runs_started_with() {
        let dir = std::env::temp_dir().join(format!("am-live-identities-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = open(&dir.join("db.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
        for (bot, identity, run_identity) in [("a", Some("cc2"), Some("cc1")), ("b", Some("cc3"), None), ("c", Some("cc4"), Some(""))] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,identity,created_at) VALUES (?,'p',?,'claude','t',?,?)")
                .bind(bot)
                .bind(bot)
                .bind(identity)
                .bind(now())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,runtime_identity,started_at) VALUES (?,?,'running',?,?)")
                .bind(format!("r-{bot}"))
                .bind(bot)
                .bind(run_identity)
                .bind(now())
                .execute(&pool)
                .await
                .unwrap();
        }
        let live: Vec<String> = live_identities_on_host(&pool, crate::config::LOCAL_HOST).await.unwrap().into_iter().collect();
        assert_eq!(live, vec!["cc1".to_string(), "cc3".to_string()]);
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

    /// issue #474：pre-v2 的 `bot_previews` 沒有 `REFERENCES bots(id)`，外鍵又不能用 ALTER 加回去。
    /// migrate 要重建表補上，而且**不能把資料洗掉**——除了指不到 bot 的孤兒列（新表帶 FK、
    /// `foreign_keys` 開著，不濾掉的話 `INSERT … SELECT` 會整批失敗、整個 migrate 回滾）。
    ///
    /// 連同這張表自己的索引與 trigger 也要活下來：`DROP TABLE` 會一起帶走它們、`RENAME` 不會還回來
    /// （i406 在 #474 提的前瞻陷阱）。今天的 `bot_previews` 一個都沒有，所以測試自己造兩個。
    #[tokio::test]
    async fn v19_rebuilds_a_pre_v2_bot_previews_to_restore_its_foreign_key() {
        let dir = tmp_dir();
        let file = dir.join("pre-v2.sqlite3");
        // 先讓 migrate 建出完整的 schema，再把 `bot_previews` 換成 pre-v2 的形狀（沒有 FK）。
        // 手寫整份 `projects`／`bots` 會漏掉後來加的欄位（`projects.host` 之類），那些欄位上還有索引，
        // 下一次開 DB 就會炸在 `CREATE UNIQUE INDEX … ON projects(host, path)`——那是測試自己寫壞，
        // 不是 migrate 的問題。這樣做也更接近正式那顆：跑過歷代 migrate、只有那個外鍵一直缺。
        {
            let pool = open(&file).await.unwrap();
            for stmt in [
                "INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p','2026-01-01T00:00:00.000Z')",
                "INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b1','p','one','claude','t','2026-01-01T00:00:00.000Z')",
                "DROP TABLE bot_previews",
                "CREATE TABLE bot_previews (bot_id TEXT PRIMARY KEY, host TEXT NOT NULL, pane_id TEXT, port INTEGER, dir TEXT,
                   status TEXT NOT NULL, error TEXT, started_at TEXT, updated_at TEXT NOT NULL)",
                "INSERT INTO bot_previews (bot_id,host,pane_id,port,dir,status,error,started_at,updated_at)
                   VALUES ('b1','local','w1:p1',5173,'/tmp/x','running',NULL,'2026-01-01T00:00:00.000Z','2026-01-02T00:00:00.000Z')",
                // 孤兒：指不到任何 bot，補外鍵時要被丟掉。
                "INSERT INTO bot_previews (bot_id,host,pane_id,port,dir,status,error,started_at,updated_at)
                   VALUES ('gone','local','w1:p9',5174,'/tmp/y','off',NULL,NULL,'2026-01-02T00:00:00.000Z')",
                // 這張表自己的索引與 trigger：`DROP TABLE` 會一起帶走，重建完要照原樣回來（i406 在 #474
                // 提的前瞻陷阱）。今天的 `bot_previews` 一個都沒有，所以這裡自己造兩個來釘住行為。
                "CREATE INDEX bot_previews_by_host ON bot_previews(host)",
                "CREATE TRIGGER bot_previews_touch AFTER UPDATE ON bot_previews
                   BEGIN UPDATE bot_previews SET updated_at = updated_at WHERE bot_id = NEW.bot_id; END",
            ] {
                sqlx::query(stmt).execute(&pool).await.unwrap_or_else(|e| panic!("{stmt}: {e}"));
            }
            let fks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_list('bot_previews')").fetch_one(&pool).await.unwrap();
            assert_eq!(fks, 0, "前提：這時的 bot_previews 沒有外鍵");
            pool.close().await;
        }
        let pool = open(&file).await.expect("pre-v2 形狀的 bot_previews 要開得起來並且自己補好");
        let fks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_list('bot_previews')").fetch_one(&pool).await.unwrap();
        assert_eq!(fks, 1, "重建之後要有 bots(id) 那個外鍵");
        // 指得到 bot 的那一列要原封不動，連 ALTER 後來補的欄位都要有預設值。
        let row: (String, String, Option<i64>, String, String, String) = sqlx::query_as(
            "SELECT bot_id, host, port, status, updated_at, source FROM bot_previews",
        )
        .fetch_one(&pool)
        .await
        .expect("只該剩一列");
        assert_eq!(row.0, "b1");
        assert_eq!(row.1, "local");
        assert_eq!(row.2, Some(5173), "資料不能在重建時掉字");
        assert_eq!(row.3, "running");
        assert_eq!(row.4, "2026-01-02T00:00:00.000Z");
        assert_eq!(row.5, "spawned", "ALTER 補的欄位照樣拿到預設值");
        // 索引與 trigger 要原樣回來：`DROP TABLE` 帶走它們，`RENAME` 不會還回來。
        let extras: Vec<(String, String)> = sqlx::query_as(
            "SELECT type, name FROM sqlite_master
              WHERE tbl_name='bot_previews' AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            extras,
            vec![("index".to_string(), "bot_previews_by_host".to_string()), ("trigger".to_string(), "bot_previews_touch".to_string())],
            "重建前有的索引與 trigger 要一個不少"
        );
        // 重跑是 no-op：已經有 FK 就不再重建（否則每次開機都洗一次表）。
        pool.close().await;
        let pool = open(&file).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bot_previews").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1, "第二次開不該再動它");
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn v15_remaps_deprecated_models_including_soft_deleted_bots_idempotently() {
        let dir = tmp_dir();
        let pool = open(&dir.join("model-remap.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
        for (id, kind, model, deleted) in [
            ("c1", "codex", "gpt-5.6-luna", None),
            ("c2", "codex", "gpt-5.6-luna", Some(now())),
            ("a1", "claude", "opus", None),
            ("a2", "claude", "claude-opus-4-1", None),
        ] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,model,hook_token,deleted_at,created_at) VALUES (?,'p',?,?,?,'t',?,?)")
                .bind(id).bind(id).bind(kind).bind(model).bind(deleted).bind(now()).execute(&pool).await.unwrap();
        }
        sqlx::query("PRAGMA user_version = 14").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        let values: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id,model FROM bots ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(values, vec![
            ("a1".into(), Some("claude-opus-5-5".into())),
            ("a2".into(), Some("claude-opus-4-1".into())),
            ("c1".into(), Some("gpt-6-luna".into())),
            ("c2".into(), Some("gpt-6-luna".into())),
        ]);
        let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        // 蓋的是這顆 binary 的版本（之後再升版也一樣），不是寫死 15。
        assert_eq!(version, SCHEMA_VERSION);
        migrate(&pool).await.unwrap();
        let again: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id,model FROM bots ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(again, values);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 版本戳記要在子模組 migrate 與漂移核對之後才蓋：主交易 commit 了、子模組才失敗，DB 不能已經是新版
    /// （否則回滾用的舊 binary 被版本閘擋住）。
    #[tokio::test]
    async fn a_failed_submodule_migrate_leaves_the_version_unstamped() {
        let dir = tmp_dir();
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect(&format!("sqlite://{}?mode=rwc", dir.join("db.sqlite3").display())).await.unwrap();
        // hook_inbox::migrate 要建 index hook_events_dedupe；名字先被一張表占走，它會失敗，而主交易那時已經 commit。
        sqlx::query("CREATE TABLE hook_events_dedupe (x INTEGER)").execute(&pool).await.unwrap();
        assert!(migrate(&pool).await.is_err());
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, 0, "子模組沒 migrate 成功，版本戳記不能已經蓋上");
        sqlx::query("DROP TABLE hook_events_dedupe").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #58：SCHEMA／additive ALTER 中途失敗要整批回滾，不能留下「有些表建了、有些沒有」的半套
    /// schema——不然重跑會在同一個位置一直卡住，中途也不該讓任何讀者看到不一致的畫面。
    #[tokio::test]
    async fn a_failed_schema_migration_rolls_back_instead_of_leaving_half_a_schema() {
        let dir = tmp_dir();
        let path = dir.join("db.sqlite3");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        // 故障注入：`runs_pane` 這個名字先被一張普通表占走。索引與表共用同一個命名空間，
        // SCHEMA 跑到 `CREATE INDEX IF NOT EXISTS runs_pane ON runs(pane_id)` 會因為名字已經
        // 是一張表而報錯——這一步落在 `projects`／`bots`／`runs` 都已經在這次呼叫裡新建、
        // `runs_one_active` 也建完之後，剛好測得到「前面明明成功的東西」有沒有跟著回滾。
        sqlx::query("CREATE TABLE runs_pane (x INTEGER)").execute(&pool).await.unwrap();

        let err = migrate(&pool).await.expect_err("撞到命名衝突要失敗，不能靜靜吞掉");
        assert!(err.to_string().contains("runs_pane"), "錯誤要指名是哪句 DDL：{err}");

        for table in ["projects", "bots", "runs"] {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(n, 0, "{table} 在失敗的這次呼叫裡新建，沒有 transaction 的話會留下來；有了就該跟著回滾");
        }
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, 0, "schema 沒套用成功，版本戳記要跟著回滾，不能宣稱已經是這個版本");

        // 修好衝突，重跑：可重入，這次要乾淨地跑完，並且通過完整性檢查。
        sqlx::query("DROP TABLE runs_pane").execute(&pool).await.unwrap();
        migrate(&pool).await.expect("修好之後重跑要成功");
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check").fetch_one(&pool).await.unwrap();
        assert_eq!(integrity, "ok");
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, SCHEMA_VERSION, "這次真的套用成功了，版本戳記要跟著更新");

        // 再跑一次：可重入，結果要一樣（不會因為東西都已經在了就出錯，也不會重複建東西）。
        let cols_before = columns(&pool, "bots").await;
        migrate(&pool).await.expect("再跑一次也要成功（可重入）");
        assert_eq!(columns(&pool, "bots").await, cols_before);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// issue #72：舊 binary 開到被更新版動過的 DB 要拒絕啟動，不能拿舊的欄位假設去讀一個看不懂
    /// 的資料庫——這是目前 `CREATE TABLE IF NOT EXISTS` 完全偵測不到的一種壞情況。
    #[tokio::test]
    async fn a_db_stamped_by_a_newer_binary_refuses_an_older_one() {
        let dir = tmp_dir();
        let path = dir.join("db.sqlite3");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        migrate(&pool).await.unwrap();
        // 假裝這個檔案被一顆懂得更多欄位的未來版 binary 動過。
        let future = SCHEMA_VERSION + 1;
        sqlx::query(&format!("PRAGMA user_version = {future}")).execute(&pool).await.unwrap();

        let err = migrate(&pool).await.expect_err("DB 比這顆 binary 認得的新，要拒絕啟動");
        assert!(err.to_string().contains(&future.to_string()) && err.to_string().contains(&SCHEMA_VERSION.to_string()), "錯誤要講清楚兩個版本號：{err}");

        // 拒絕啟動不能順便把版本號改回來，也不能動任何 schema。
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, future);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 版本號功能上線前建立的舊資料庫（`user_version` 從沒被設過，SQLite 預設 0）一樣要能升上來，
    /// 而且升級之後可重入：同一版重跑版本號不變、不報錯（issue #72 驗收項）。
    #[tokio::test]
    async fn an_old_unversioned_db_upgrades_and_stays_reentrant() {
        let dir = tmp_dir();
        let path = dir.join("db.sqlite3");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        let before: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(before, 0, "全新檔案／版本號功能上線前的舊 DB，SQLite 預設就是 0");

        migrate(&pool).await.unwrap();
        let after: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(after, SCHEMA_VERSION);

        // 可重入：同一版再跑一次不報錯、版本號不變。
        migrate(&pool).await.expect("同一版重跑不該失敗");
        let again: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(again, SCHEMA_VERSION);

        pool.close().await;
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

    /// issue #461：同一毫秒的兩個 run，要接回的是**後寫進去的那一個**。
    ///
    /// `started_at` 只到毫秒（`now()`），而一顆 bot 快速重啟（stop 完馬上 start，
    /// `restart?resume=native` 就是這條路）會讓兩個 run 擠進同一毫秒。
    /// 那時 ULID 的亂數段不保證遞增，所以 `id` 的字典序跟寫入順序可能相反——
    /// 這裡故意把**後寫的那一筆給比較小的 id**，把那個情況釘死。
    ///
    /// 挑錯的後果不是少接回一次，是 `--resume` 進另一段對話，之後訊息都落在那段裡。
    #[tokio::test]
    async fn two_runs_in_the_same_millisecond_resume_the_one_written_last() {
        let dir = tmp_dir();
        let pool = open(&dir.join("samems.sqlite3")).await.unwrap();
        let at = now();
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(&at).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','pm','claude','tok',?)")
            .bind(&at).execute(&pool).await.unwrap();

        // 同一個 started_at，而且 id 的字典序跟寫入順序**相反**：
        // 先寫 `r-zzz`（舊的那次 run），後寫 `r-aaa`（真正最後那次）。
        let same = "2026-09-07T05:00:00.123Z";
        for (id, native) in [("r-zzz", "native-earlier"), ("r-aaa", "native-latest")] {
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?, 'b1', 'exited', 'unknown', ?, '/tmp/t.jsonl', ?, ?)",
            )
            .bind(id).bind(native).bind(same).bind(same)
            .execute(&pool).await.unwrap();
        }

        // 修好的寫法（`, rowid DESC`）：拿到後寫的那一個。
        let got = last_native_session(&pool, "b1").await.unwrap().map(|(sid, _)| sid);
        assert_eq!(got.as_deref(), Some("native-latest"), "同毫秒時要接回後寫進去的那一個 run");
        assert_eq!(last_native_session_id(&pool, "b1").await.unwrap().as_deref(), Some("native-latest"));

        // 舊寫法為什麼不行，在同一份資料上直接證明：`id DESC` 會挑到先寫的那一筆
        // （`r-zzz` > `r-aaa`），也就是**另一段對話**。沒有第二鍵的版本更糟——連決定性都沒有。
        let by_id: Option<String> = sqlx::query_scalar(
            "SELECT native_session_id FROM runs WHERE bot_id='b1' AND native_session_id IS NOT NULL
              ORDER BY started_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&pool).await.unwrap();
        assert_eq!(by_id.as_deref(), Some("native-earlier"), "舊寫法（id DESC）挑到的正是錯的那一段");
        assert_ne!(by_id, got, "兩種寫法在這份資料上必須不同，否則這條測試證明不了任何事");

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
