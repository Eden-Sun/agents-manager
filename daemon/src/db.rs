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
"#;

/// 這個 binary 認得的 schema 版本，存在 SQLite 內建的 `PRAGMA user_version`（跟資料庫檔案綁在一起，
/// 讀寫都在同一個交易裡，不像 `journal_mode` 那類 pragma 有「不能包進交易」的限制）。
///
/// 這不是「照順序跑第 N 號 migration」的版本號——`SCHEMA`／下面的 additive ALTER 名單本來就是
/// `CREATE TABLE IF NOT EXISTS`／`has_column` 檢查過的冪等操作，天生可重入，這次刻意不推翻
/// （issue #72 的調查結論，見 `migrate` 上面的說明）。這個數字只解決一件事：**擋住舊 binary 開到
/// 新 schema**——目前完全偵測不到這種情況，`daemon-update-kick.sh` 那類滾動升級如果有一顆卡在舊
/// binary 卻碰到剛被新版升級過的 DB，會拿著過期的欄位假設去讀一個它不認識的資料庫。每次在 `SCHEMA`
/// 或 ALTER 名單裡加東西，這個數字要跟著 +1；忘記加只會讓 `check_schema_drift` 照樣抓到欄位對不上
/// （那個檢查看的是實際欄位，不看這個數字），不會讓資料庫壞掉，但舊 binary 就少了這一層提早攔截。
pub const SCHEMA_VERSION: i64 = 4;

/// 裝一個 trigger，DB 裡那一份跟 `ddl` 不同就換掉（issue #186）。
///
/// 守衛的內容是由轉移表產生的（`turn_controller::guard_ddl`、`assignment_state::guard_ddl`）。以前用
/// `CREATE TRIGGER IF NOT EXISTS`：只有第一次建得進去，之後轉移表改了（新增或拿掉一條合法邊），舊 DB 裡那一份已經存在，
/// 守衛就永遠停在舊規則——新的合法轉移被擋下、拿掉的照樣放行。這裡每次開 DB 都拿 `sqlite_master.sql`（SQLite 存的是去掉
/// `IF NOT EXISTS` 的原文）跟現在的 DDL 比，不同才 DROP 再建，所以不必靠 `SCHEMA_VERSION`。呼叫端要在同一個交易裡：
/// 換到一半失敗整批回滾，不會留下一段沒有守衛的空窗。
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
/// 不懂的資料庫（issue #72）。版本比較與最後的版本戳記都在同一個交易裡：SCHEMA／ALTER 失敗時
/// 版本號要跟著回滾，不能宣稱「已經是這個版本」卻沒有真的套用成功。
async fn migrate(pool: &SqlitePool) -> Result<()> {
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
    ] {
        if !has_column(&mut *tx, table, col).await? {
            sqlx::query(ddl).execute(&mut *tx).await.with_context(|| format!("add {table}.{col}"))?;
        }
    }
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
    // Turn 狀態轉移的單一權威（issue #68）：合法邊只定義在 `lifecycle::turn_controller::LEGAL_EDGES`，
    // trigger 由它生成。二十來處 `UPDATE turns SET status` 各自帶的 CAS guard 照舊，這是它們的下限，
    // 而且未來新寫的路徑繞不過去——終局的回合不可能被改回進行中。
    crate::lifecycle::turn_controller::install_guard(&mut tx).await.context("create turns_status_transition trigger")?;
    // 版本戳記放最後：所有 DDL 都成功了才蓋，中途失敗整批回滾、下次從乾淨的起點重來。
    if stored_version < SCHEMA_VERSION {
        // `user_version` 不接受 bind 參數（跟 `table_info` 那個 PRAGMA 一樣），但這裡的值是編譯期常數，
        // 不是外部輸入，直接內嵌沒有注入風險。
        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}")).execute(&mut *tx).await.context("stamp schema version")?;
    }
    tx.commit().await?;
    crate::supervisor::store::migrate(pool).await?;
    crate::read_marks::migrate(pool).await?;
    crate::panes::migrate(pool).await?;
    crate::herdr_maintenance::migrate(pool).await?;
    crate::mission::store::migrate(pool).await?;
    crate::hook_inbox::migrate(pool).await?;
    crate::build_scheduler::migrate(pool).await?;
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
    /// 那一次 `resume_native` 的結論：`verified`／`mismatch`／`unverified`（issue #92，`lifecycle::resume_gate`）。
    pub resume_outcome: Option<String>,
    /// `agent_status` 最後一次真的改變的時間（trigger 蓋，issue #93）；前端算「跑了多久」的起點。
    /// `None` = 還沒真的變過，或升級前的舊列。
    pub agent_status_since: Option<String>,
    /// claude 原生 SubagentStart／SubagentStop 的最後一筆快照（issue #82）。純可見性，`hookrecv` 是
    /// 唯一寫入者；不影響 §6.5a 的血緣認領。
    pub subagent_json: Option<String>,
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

