//! Supervisor (AGM) persistence — the authoritative record behind the manager LLM.
//!
//! The manager model only understands and decides; every fact it is allowed to rely on lives
//! in these tables, so a daemon restart, a model switch or a lost session never loses an
//! assignment or replays one twice.

use super::assignment_state;
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{FromRow, SqlitePool};

#[allow(unused_imports)]
pub use crate::supervisor_inbox::{
    assignment, assignment_by_crid, assignment_by_turn, push_inbox, push_inbox_tx, sql_list,
    unsettled_assignments, Assignment, EXECUTING_STATES, OPEN_STATES, STALLED_STATES,
    SUPERVISOR_ID,
};

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
  -- remote_*：Remote Control 的舊狀態欄，2026-10-12（#1477）起不讀不寫；欄位留著，舊 binary 的 SELECT 與
  -- ops 腳本的 INSERT 才不會壞（拿掉要升 SCHEMA_VERSION，沒有收益）。
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
  -- Lifecycle, *not* transport. In flight: queued | delivered | unknown. The turn ending puts
  -- it in `awaiting_review`; only an explicit AGM decision reaches completed | failed |
  -- cancelled | superseded, and `blocked` is a decision that keeps it open. The raw transport
  -- facts stay in `delivery`, `turn_status` and `evidence_complete` — a turn that ended is not
  -- the same claim as a job that was accepted.
  status TEXT NOT NULL DEFAULT 'queued',
  -- 0 = 通知（notice）：AGM 只是要把話說給 bot 聽，不等回覆也不驗收。回合一結束就自己結案，
  -- 不進 awaiting_review、不會變成 assignment_stalled。1 = 一般交辦，照 §18.3 等 AGM 裁示。
  expects_review INTEGER NOT NULL DEFAULT 1,
  -- `quota_blocked` 用：額度什麼時候回來（CLI 橫幅寫的 `try again at …`，沒寫就給預設）。
  resume_at TEXT,
  -- 因為額度被擋、之後自動重送的次數。重送用 `<crid>#r<n>` 當 client_request_id：
  -- 同一次重送重跑幾遍都只會有一個 turn，但跟前一次撞上限的那個 turn 分得開。
  quota_retries INTEGER NOT NULL DEFAULT 0,
  delivery TEXT,
  result TEXT,
  error TEXT,
  attempts INTEGER NOT NULL DEFAULT 0,
  -- 排不進去幾輪（409：對方回合中、沒有 run、框裡有字）。跟 `attempts` 分開記（issue #528）：
  -- `attempts` 是「上游錯誤重試到第幾次」的界，混進 409 的等待之後，一顆忙碌的 bot 排個八分鐘
  -- 就把那個界用完，之後第一個暫時性錯誤直接判 dispatch_failed。
  busy_rounds INTEGER NOT NULL DEFAULT 0,
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
-- 分頁與「還欠著哪幾件」共用的排序（issue #543）。`list_assignments_page` 是
-- `WHERE supervisor_id=? ORDER BY created_at DESC, id DESC`，而 `supervisor_id` 實際上只有一個值，
-- 所以少了這個索引時 planner 會走 crid 索引把整張表掃完、再 `USE TEMP B-TREE FOR ORDER BY`：
-- 離線兩萬筆量到翻 50 頁 202ms，加上之後 13ms（變成 covering index）。這張表只增不減，
-- 而 `assign()` 的 ownership 衝突檢查與 state／handoff 的未結案清單都踩在同一條路上。
CREATE INDEX IF NOT EXISTS supervisor_assignments_created
  ON supervisor_assignments(supervisor_id, created_at DESC, id DESC);
-- Every acceptance decision, kept whole: who decided, on what grounds, from where. The
-- assignment row carries only the latest one, and a state machine without an audit trail is
-- how "AGM said it was done" becomes unfalsifiable.
CREATE TABLE IF NOT EXISTS supervisor_reviews (
  id TEXT PRIMARY KEY,
  supervisor_id TEXT NOT NULL,
  assignment_id TEXT NOT NULL,
  -- accept | block | followup | fail | cancel
  decision TEXT NOT NULL,
  -- Status the assignment was in when the decision was taken, so a replayed decision is
  -- recognisable as one.
  from_status TEXT NOT NULL,
  to_status TEXT NOT NULL,
  actor TEXT NOT NULL,
  source TEXT NOT NULL DEFAULT 'api',
  reason TEXT,
  evidence TEXT,
  -- The follow-up assignment this decision created, when the decision was `followup`.
  followup_assignment_id TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS supervisor_reviews_assignment
  ON supervisor_reviews(assignment_id, created_at);
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
-- 依 state 撈 `delivered`／`pending`（issue #1653）：只有一個 supervisor_id，`supervisor_inbox_key` 的前綴等於整張表；
-- `state` 又排在 `payload_json` 後面，照它篩得把每列的 payload 讀過一遍。上面的 partial 索引幫不上——SQLite 不認
-- `state='delivered'` 蘊含 `state != 'handled'`（`INDEXED BY` 直接 no query solution）。這個複合索引讓兩個等號一起下去。
CREATE INDEX IF NOT EXISTS supervisor_inbox_sup_state ON supervisor_inbox(supervisor_id, state);
-- One row per *resource* that is currently wrong, not per tick that noticed it. The partial
-- unique index is the dedupe key: a restart cannot open a second incident for the same thing,
-- and a fault that comes back after a resolve gets a new row (a new occurrence), not a silent
-- re-use of the old one.
CREATE TABLE IF NOT EXISTS supervisor_incidents (
  id TEXT PRIMARY KEY,
  supervisor_id TEXT NOT NULL,
  -- host_disconnected | bot_stopped | assignment_stalled | notify_exhausted | approval_stalled（舊的 remote_entry 已隨 #1477 移除，開著的會被下一拍探針收掉）
  kind TEXT NOT NULL,
  -- What is wrong: a host name, a bot id, an assignment id. Never a message.
  resource TEXT NOT NULL,
  severity TEXT NOT NULL DEFAULT 'degraded',
  -- open | resolved
  status TEXT NOT NULL DEFAULT 'open',
  detail_json TEXT NOT NULL DEFAULT '{}',
  -- How many ticks have confirmed it since it opened; the threshold is applied before the row
  -- exists, so 1 already means "past the threshold".
  occurrences INTEGER NOT NULL DEFAULT 1,
  first_seen_at TEXT NOT NULL,
  last_seen_at TEXT NOT NULL,
  resolved_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS supervisor_incidents_open
  ON supervisor_incidents(supervisor_id, kind, resource) WHERE status='open';
CREATE INDEX IF NOT EXISTS supervisor_incidents_recent ON supervisor_incidents(first_seen_at);
-- A rebuild / restart permission, written down instead of living in a chat message: who asked,
-- for what, against which commit, until when, and who decided.
CREATE TABLE IF NOT EXISTS supervisor_approvals (
  id TEXT PRIMARY KEY,
  supervisor_id TEXT NOT NULL,
  requester TEXT NOT NULL,
  -- rebuild | restart | other: what the approval is *for*. A lease can only be taken on the
  -- resource its approval names.
  purpose TEXT NOT NULL,
  scope TEXT NOT NULL,
  -- The commit the approval was granted against. A different tree is a different decision.
  target_commit TEXT,
  -- pending | approved | denied | revoked | consumed
  status TEXT NOT NULL DEFAULT 'pending',
  decided_by TEXT,
  decided_at TEXT,
  -- AGM **裁示**時寫的理由。申請時寫的那段在 `request_reason`：以前共用一欄，裁示一寫就把申請
  -- 理由蓋掉，事後查「他當初為什麼要申請」只剩 deny 的那句（2026-09-19 實測）。
  reason TEXT,
  -- 申請者寫的理由（`approval request --reason`）。
  request_reason TEXT,
  -- `requester` 是呼叫端自稱的、還是驗過的（issue #436）。`1`＝沒帶 `X-AM-Bot-Id`／帶了但那顆
  -- 不是自稱的那位，只是一段字串；`0`＝建立時附上了那顆 bot 自己的 hook token，確實是它本人。
  -- 舊列一律 `1`（那時候整欄都是自稱的），所以預設是 1 而不是 0：沒驗過的不能事後被讀成驗過。
  requester_unverified INTEGER NOT NULL DEFAULT 1,
  expires_at TEXT,
  -- 呼叫端自己給的穩定 id：同一個 supervisor 下重送同一個 id 回原本那一筆（冪等）。NULL＝沒帶。
  client_request_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS supervisor_approvals_open ON supervisor_approvals(status, created_at);
-- 重送同一筆申請不該多開一列（2026-09-16：k8bw2f 對同一顆 commit 送了兩次 restart，
-- 因為它讀錯回應欄位以為沒送出去）。有帶穩定 request id 的才受這個唯一鍵約束；舊列是 NULL。
CREATE UNIQUE INDEX IF NOT EXISTS supervisor_approvals_request
  ON supervisor_approvals(supervisor_id, client_request_id) WHERE client_request_id IS NOT NULL;
-- One row per resource, ever. Holding it is `released_at IS NULL AND expires_at > now`, so a
-- holder that died releases by expiry and nothing stays locked forever. `fence` only ever goes
-- up: an old holder that wakes after its lease expired presents a stale fence and is refused,
-- even though its approval may still read "approved".
CREATE TABLE IF NOT EXISTS supervisor_leases (
  resource TEXT PRIMARY KEY,
  owner TEXT,
  approval_id TEXT,
  fence INTEGER NOT NULL DEFAULT 0,
  target_commit TEXT,
  acquired_at TEXT,
  expires_at TEXT,
  released_at TEXT,
  -- acquire 當下發的一次性憑證：renew／release 要出示它。owner 與 fence 是公開欄位
  -- （`lease status` 就看得到），只靠它們等於誰都能把別人正在換 binary 的窗口收掉。
  -- 只在 acquire 的回應裡出現一次，不進任何唯讀輸出。升級前既有的列是 NULL（見 SPEC §18.10）。
  lease_token TEXT,
  detail_json TEXT NOT NULL DEFAULT '{}'
);
-- §6.11：被 AGM 因為閒置收起來的 bot。有這一列 = 它不是壞掉也不是使用者關的，是睡著的，
-- 下次要用時得用 `--resume <native_session_id>` 接回原本那段對話。叫醒成功就把列刪掉。
CREATE TABLE IF NOT EXISTS bot_sleeps (
  bot_id TEXT PRIMARY KEY,
  -- 收起來的那一刻它跑在哪一段 native session（真正續接時仍以 `last_native_session` 為準，
  -- 這裡留著是為了讓「為什麼這顆是睡著的」看得出來）。
  native_session_id TEXT,
  -- 收起來時已經閒置幾分鐘。
  idle_minutes INTEGER NOT NULL DEFAULT 0,
  reason TEXT NOT NULL DEFAULT 'idle',
  slept_at TEXT NOT NULL
);
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
    // 先補欄位再跑 DDL：DDL 裡的唯一索引會用到 `client_request_id`，舊庫沒有那一欄就會整段失敗。
    // 表還不存在時 `has_column` 回 false，ALTER 也會失敗——所以只在表已經在的時候補。
    if table_exists(pool, "supervisor_approvals").await? && !has_column(pool, "supervisor_approvals", "client_request_id").await? {
        sqlx::query("ALTER TABLE supervisor_approvals ADD COLUMN client_request_id TEXT").execute(pool).await?;
    }
    if table_exists(pool, "supervisor_leases").await? && !has_column(pool, "supervisor_leases", "lease_token").await? {
        sqlx::query("ALTER TABLE supervisor_leases ADD COLUMN lease_token TEXT").execute(pool).await?;
    }
    for stmt in DDL.split(";\n") {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        sqlx::query(s).execute(pool).await?;
    }
    // Assignment 狀態轉移的單一權威（issue #71，跟 issue #68 的 `turns_status_transition` 同一個
    // 理由）：合法邊只定義在 `assignment_state::allowed`，trigger 由它生成。既有那幾支各自帶
    // `WHERE status IN (…)` guard 的函式照舊，這是它們的下限；未來新寫的路徑繞不過去。
    assignment_state::install_guard(pool).await?;
    // 換 commit 接續的等待起點（見 `Approval::wait_since`）。表由上面的 DDL 保證存在。
    if !has_column(pool, "supervisor_approvals", "wait_since").await? {
        sqlx::query("ALTER TABLE supervisor_approvals ADD COLUMN wait_since TEXT").execute(pool).await?;
    }
    // 申請理由（2026-09-19）：舊庫沒有這一欄。
    if !has_column(pool, "supervisor_approvals", "request_reason").await? {
        sqlx::query("ALTER TABLE supervisor_approvals ADD COLUMN request_reason TEXT").execute(pool).await?;
    }
    // issue #436：申請人是不是驗過的。既有的列都是自稱的年代寫的，所以補上去時一律 1。
    if !has_column(pool, "supervisor_approvals", "requester_unverified").await? {
        sqlx::query("ALTER TABLE supervisor_approvals ADD COLUMN requester_unverified INTEGER NOT NULL DEFAULT 1")
            .execute(pool)
            .await?;
    }
    // Additive columns for databases created before they existed (same pattern as `db::migrate`).
    // Keep `desired_running` in its existing position: the fresh-schema fingerprint includes column order.
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
        // The persona the manager actually runs on. Authoritative: `setup` seeds this when it
        // is empty and never writes over it afterwards, so an older binary running `setup`
        // cannot roll a newly updated persona back to whatever it happens to have compiled in.
        ("persona_text", "ALTER TABLE supervisors ADD COLUMN persona_text TEXT"),
        ("persona_version", "ALTER TABLE supervisors ADD COLUMN persona_version INTEGER NOT NULL DEFAULT 0"),
        ("persona_hash", "ALTER TABLE supervisors ADD COLUMN persona_hash TEXT"),
        // `embedded` (seeded from the binary) | `api` (somebody set it explicitly).
        ("persona_source", "ALTER TABLE supervisors ADD COLUMN persona_source TEXT"),
        ("persona_updated_at", "ALTER TABLE supervisors ADD COLUMN persona_updated_at TEXT"),
        // The embedded hash this was seeded from / last migrated to. Comparing it with the
        // running binary's is how "there is a newer embedded version" is distinguished from
        // "somebody deliberately customised this".
        ("persona_seed_hash", "ALTER TABLE supervisors ADD COLUMN persona_seed_hash TEXT"),
        // 舊的 remote-entry 觀測欄（#1477 起不讀不寫，欄位保留，見 CREATE TABLE 的註解）。
        ("remote_source", "ALTER TABLE supervisors ADD COLUMN remote_source TEXT"),
        ("remote_observed_at", "ALTER TABLE supervisors ADD COLUMN remote_observed_at TEXT"),
        // The run the observation was made against: a restart invalidates it.
        ("remote_session_id", "ALTER TABLE supervisors ADD COLUMN remote_session_id TEXT"),
        ("remote_actor", "ALTER TABLE supervisors ADD COLUMN remote_actor TEXT"),
        ("remote_evidence", "ALTER TABLE supervisors ADD COLUMN remote_evidence TEXT"),
        // When the watchdog stopped trying, and why. Persisted so the report happens exactly
        // once: before this, giving up was a state nobody was told about and every later tick
        // silently re-derived it (review #31).
        ("watchdog_gave_up_at", "ALTER TABLE supervisors ADD COLUMN watchdog_gave_up_at TEXT"),
        ("watchdog_last_error", "ALTER TABLE supervisors ADD COLUMN watchdog_last_error TEXT"),
    ] {
        if col == "desired_running" {
            if !has_column(pool, "supervisors", col).await? {
                add_column_with_backfill(
                    pool,
                    "supervisors",
                    col,
                    ddl,
                    "UPDATE supervisors SET desired_running=1 WHERE bot_id IS NOT NULL AND generation>0",
                )
                .await?;
            }
        } else if !has_column(pool, "supervisors", col).await? {
            sqlx::query(ddl).execute(pool).await?;
        }
    }
    // These backfills stay next to their original ALTERs. In particular, moving `legacy_closed`
    // changes SQLite's column order and makes fresh databases disagree with existing v38 ones.
    for (col, ddl) in [
        // The turn's own last word, kept apart from the lifecycle status: `completed`,
        // `completed_fallback`, `failed`, `dispatch_failed`. Losing it was how "the turn
        // ended" turned into "the job is done".
        ("turn_status", "ALTER TABLE supervisor_assignments ADD COLUMN turn_status TEXT"),
        // 0 when the result was scraped off the terminal instead of reported by a hook. An
        // assignment is never accepted automatically, and least of all on this evidence.
        ("evidence_complete", "ALTER TABLE supervisor_assignments ADD COLUMN evidence_complete INTEGER"),
        ("reviewed_at", "ALTER TABLE supervisor_assignments ADD COLUMN reviewed_at TEXT"),
        ("reviewed_by", "ALTER TABLE supervisor_assignments ADD COLUMN reviewed_by TEXT"),
        ("review_decision", "ALTER TABLE supervisor_assignments ADD COLUMN review_decision TEXT"),
        ("review_reason", "ALTER TABLE supervisor_assignments ADD COLUMN review_reason TEXT"),
        // The assignment that carries this one's unfinished part forward, and the pointer back.
        ("followup_assignment_id", "ALTER TABLE supervisor_assignments ADD COLUMN followup_assignment_id TEXT"),
        ("follow_up_of", "ALTER TABLE supervisor_assignments ADD COLUMN follow_up_of TEXT"),
        // Closed under the pre-review semantics: the turn ended and the row was called
        // `completed` without anyone accepting it. Kept closed (a backfill must not re-open a
        // month of work) but never presented as an acceptance. See docs/SPEC.md §18.
        ("legacy_closed", "ALTER TABLE supervisor_assignments ADD COLUMN legacy_closed INTEGER NOT NULL DEFAULT 0"),
        // Files / modules this assignment was handed, so an overlap with another open
        // assignment can at least be reported to AGM (§18.4).
        ("ownership_json", "ALTER TABLE supervisor_assignments ADD COLUMN ownership_json TEXT"),
        // 通知型交辦（見 SCHEMA 的欄位註解）。舊資料一律 1＝照原本的規則等 AGM 驗收。
        ("expects_review", "ALTER TABLE supervisor_assignments ADD COLUMN expects_review INTEGER NOT NULL DEFAULT 1"),
        ("resume_at", "ALTER TABLE supervisor_assignments ADD COLUMN resume_at TEXT"),
        ("quota_retries", "ALTER TABLE supervisor_assignments ADD COLUMN quota_retries INTEGER NOT NULL DEFAULT 0"),
        // 舊版群組任務的欄位保留以維持資料庫相容；程式不再讀寫。
        ("mission_id", "ALTER TABLE supervisor_assignments ADD COLUMN mission_id TEXT"),
        ("mission_role", "ALTER TABLE supervisor_assignments ADD COLUMN mission_role TEXT"),
        // 回合結束時 run 上記的 `turn_error`（撞限、API 錯誤、斷線…）。`turn_status` 只有
        // completed/failed 這種粗分類，換手要看的是「為什麼」。
        ("turn_error", "ALTER TABLE supervisor_assignments ADD COLUMN turn_error TEXT"),
        // 這一輪「一直 409 送不進去」從什麼時候開始（第一次撞 409 寫下）。409 保險絲從這裡計時，
        // 不是 `created_at`：在 quota_blocked 或 restart 窗口 hold 裡合法等待的時間不算「送不進去」。
        ("conflict_since", "ALTER TABLE supervisor_assignments ADD COLUMN conflict_since TEXT"),
        // 排不進去幾輪（見 SCHEMA 的欄位註解）。舊列從 0 開始：它們的 409 歷史本來就記在
        // `attempts` 裡分不出來，硬猜只會讓保險絲一升級就燒掉。
        ("busy_rounds", "ALTER TABLE supervisor_assignments ADD COLUMN busy_rounds INTEGER NOT NULL DEFAULT 0"),
    ] {
        if col == "turn_status" {
            if !has_column(pool, "supervisor_assignments", col).await? {
                add_column_with_backfill(
                    pool,
                    "supervisor_assignments",
                    col,
                    ddl,
                    "UPDATE supervisor_assignments SET turn_status='completed' WHERE status='completed'",
                )
                .await?;
            }
        } else if col == "legacy_closed" {
            if !has_column(pool, "supervisor_assignments", col).await? {
                add_column_with_backfill(
                    pool,
                    "supervisor_assignments",
                    col,
                    ddl,
                    "UPDATE supervisor_assignments SET legacy_closed=1 WHERE status IN ('completed','failed','cancelled')",
                )
                .await?;
            }
        } else if !has_column(pool, "supervisor_assignments", col).await? {
            sqlx::query(ddl).execute(pool).await?;
        }
    }
    for (col, ddl) in [
        // Durable notify bookkeeping: what the transport said, how many times we tried, when
        // the next attempt is due and why the last one did not stick.
        ("notify_delivery", "ALTER TABLE supervisor_inbox ADD COLUMN notify_delivery TEXT"),
        ("notify_attempts", "ALTER TABLE supervisor_inbox ADD COLUMN notify_attempts INTEGER NOT NULL DEFAULT 0"),
        ("notify_next_at", "ALTER TABLE supervisor_inbox ADD COLUMN notify_next_at TEXT"),
        ("notify_error", "ALTER TABLE supervisor_inbox ADD COLUMN notify_error TEXT"),
        ("delivered_at", "ALTER TABLE supervisor_inbox ADD COLUMN delivered_at TEXT"),
    ] {
        if !has_column(pool, "supervisor_inbox", col).await? {
            sqlx::query(ddl).execute(pool).await?;
        }
    }
    // issue #459：`responder_needs_login` 改名成 `role_unavailable`。舊 kind 的列再也不會被觀測到
    // （`observe` 只出新名），所以還開著的那些會變成永遠關不掉的孤兒——resource 是角色名的直接改寫
    // 過來（同一件事、同一把唯一索引 key），其餘（#420 時代 resource 是 bot_id 的）就地關掉。
    sqlx::query(
        "UPDATE supervisor_incidents SET kind=? WHERE kind=? AND resource IN ('patrol','responder')
           AND NOT EXISTS (SELECT 1 FROM supervisor_incidents b
                            WHERE b.supervisor_id=supervisor_incidents.supervisor_id AND b.kind=?
                              AND b.resource=supervisor_incidents.resource AND b.status='open')",
    )
    .bind(super::incidents::ROLE_UNAVAILABLE_KIND)
    .bind(super::incidents::ROLE_UNAVAILABLE_KIND_LEGACY)
    .bind(super::incidents::ROLE_UNAVAILABLE_KIND)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE supervisor_incidents SET status='resolved', resolved_at=? WHERE kind=? AND status='open'")
        .bind(crate::db::now())
        .bind(super::incidents::ROLE_UNAVAILABLE_KIND_LEGACY)
        .execute(pool)
        .await?;
    // 雙角色（巡檢／協調）的欄位與表，見 roles.rs。
    super::roles::migrate(pool).await?;
    Ok(())
}

/// 表還沒建出來時 `PRAGMA table_info` 回空集合，跟「有表但沒這一欄」長得一樣，
/// 所以補欄位之前要先分得出這兩種情況——對著不存在的表 ALTER 會整個 migrate 失敗。
async fn table_exists(pool: &SqlitePool, table: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
        .bind(table)
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

async fn add_column_with_backfill(pool: &SqlitePool, table: &str, col: &str, ddl: &str, backfill: &str) -> Result<()> {
    let mut tx = crate::db::begin_write(pool).await?;
    if !crate::db::has_column(&mut *tx, table, col).await? {
        sqlx::query(ddl).execute(&mut *tx).await?;
        sqlx::query(backfill).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(super) async fn has_column(pool: &SqlitePool, table: &str, col: &str) -> Result<bool> {
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
    pub summary: Option<String>,
    pub summary_version: i64,
    pub desired_running: i64,
    pub watchdog_attempts: i64,
    pub watchdog_next_at: Option<String>,
    pub last_notify_at: Option<String>,
    pub persona_text: Option<String>,
    pub persona_version: i64,
    pub persona_hash: Option<String>,
    pub persona_source: Option<String>,
    pub persona_updated_at: Option<String>,
    pub persona_seed_hash: Option<String>,
    pub watchdog_gave_up_at: Option<String>,
    pub watchdog_last_error: Option<String>,
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
    pub notify_delivery: Option<String>,
    pub notify_attempts: i64,
    pub notify_next_at: Option<String>,
    pub notify_error: Option<String>,
    pub delivered_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// `patrol` | `responder`；NULL = 還沒分類（見 `roles::classify`）。
    #[sqlx(default)]
    pub role: Option<String>,
    /// 0 = 只記錄、不喚醒。
    #[sqlx(default)]
    pub wake: Option<i64>,
    /// 實際送給哪個角色。
    #[sqlx(default)]
    pub claimed_by: Option<String>,
    #[sqlx(default)]
    pub acked_by: Option<String>,
    #[sqlx(default)]
    pub merged_into: Option<String>,
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
            // How the wake-up that carried this event went. `unknown` is its own answer: the
            // prompt may or may not have reached the manager, so the next attempt reconciles
            // against `notify_turn_id` instead of sending a second copy.
            "notify": {
                "turn_id": self.notify_turn_id,
                "delivery": self.notify_delivery,
                "attempts": self.notify_attempts,
                "next_at": self.notify_next_at,
                "error": self.notify_error,
                "delivered_at": self.delivered_at,
            },
            "role": self.role,
            "wake": self.wake.map(|w| w != 0),
            "claimed_by": self.claimed_by,
            "acked_by": self.acked_by,
            "merged_into": self.merged_into,
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
///
/// `Ok(())` means the watchdog will read this intent after a restart — nothing else does.
/// 沒有匹配到任何列也算失敗：呼叫端會照這個 `Ok` 去做啟停的副作用，而看門狗讀到的還是舊的意圖
/// （issue #84）。
pub async fn set_desired_running(pool: &SqlitePool, wanted: bool) -> Result<()> {
    let wrote = sqlx::query(
        "UPDATE supervisors SET desired_running=?, watchdog_attempts=0, watchdog_next_at=NULL,
           watchdog_gave_up_at=NULL, watchdog_last_error=NULL, updated_at=? WHERE id=?",
    )
    .bind(i64::from(wanted))
    .bind(crate::db::now())
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?;
    if wrote.rows_affected() == 0 {
        anyhow::bail!("desired_running={wanted} matched no supervisor row ({SUPERVISOR_ID})");
    }
    Ok(())
}

pub async fn set_watchdog(pool: &SqlitePool, attempts: i64, next_at: Option<&str>) -> Result<()> {
    // Resetting the streak (the manager answered again, or a human decided) also clears the
    // "gave up" record, so a later outage is reported as a new one instead of being swallowed
    // by a marker left over from the last one.
    let clear = attempts == 0;
    sqlx::query(
        "UPDATE supervisors SET watchdog_attempts=?, watchdog_next_at=?,
           watchdog_gave_up_at=CASE WHEN ? THEN NULL ELSE watchdog_gave_up_at END,
           watchdog_last_error=CASE WHEN ? THEN NULL ELSE watchdog_last_error END,
           updated_at=? WHERE id=?",
    )
    .bind(attempts)
    .bind(next_at)
    .bind(clear)
    .bind(clear)
    .bind(crate::db::now())
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record that the watchdog has stopped retrying. `Ok(true)` = this call was the first to say
/// so, which is what makes the report happen exactly once however many ticks observe it.
pub async fn mark_watchdog_gave_up(pool: &SqlitePool, why: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE supervisors SET watchdog_gave_up_at=?, watchdog_last_error=?, updated_at=?
          WHERE id=? AND watchdog_gave_up_at IS NULL",
    )
    .bind(crate::db::now())
    .bind(why)
    .bind(crate::db::now())
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
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

/// Write the persona, bumping its version. `source` is `embedded` (a seed or an explicit
/// migration) or `api` (somebody set the text).
///
/// `seed_hash` records which embedded text this came from, so "there is a newer version in the
/// binary" stays distinguishable from "this was customised on purpose". Returns the new version.
pub async fn set_persona(
    pool: &SqlitePool,
    text: &str,
    source: &str,
    seed_hash: Option<&str>,
) -> Result<i64> {
    let now = crate::db::now();
    sqlx::query(
        "UPDATE supervisors
            SET persona_text=?, persona_hash=?, persona_source=?, persona_updated_at=?,
                persona_seed_hash=COALESCE(?, persona_seed_hash), persona_version=persona_version+1, updated_at=?
          WHERE id=?",
    )
    .bind(text)
    .bind(super::persona::hash(text))
    .bind(source)
    .bind(&now)
    .bind(seed_hash)
    .bind(&now)
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?;
    Ok(sqlx::query_scalar::<_, i64>("SELECT persona_version FROM supervisors WHERE id=?")
        .bind(SUPERVISOR_ID)
        .fetch_one(pool)
        .await?)
}

/// Seed the persona only if there is none. `Ok(true)` = this call planted it.
///
/// The whole point: a first install gets the binary's text, and every install after that keeps
/// what it has. `setup` is idempotent, and idempotent must not mean "re-assert the default".
pub async fn seed_persona_if_empty(pool: &SqlitePool, text: &str) -> Result<bool> {
    seed_persona_from(pool, text, "embedded", text).await
}

/// First upgrade from the old bot/config authority must preserve the existing custom text.
/// The embedded hash is still recorded separately, so future binary upgrades stay observable.
pub async fn seed_persona_from(pool: &SqlitePool, text: &str, source: &str, embedded: &str) -> Result<bool> {
    let now = crate::db::now();
    let planted = sqlx::query(
        "UPDATE supervisors
            SET persona_text=?, persona_hash=?, persona_source=?, persona_updated_at=?,
                persona_seed_hash=?, persona_version=persona_version+1, updated_at=?
          WHERE id=? AND (persona_text IS NULL OR persona_text='')",
    )
    .bind(text)
    .bind(super::persona::hash(text))
    .bind(source)
    .bind(&now)
    .bind(super::persona::hash(embedded))
    .bind(&now)
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    Ok(planted)
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

/// `handoff` 備註（每次 `PUT /supervisor/handoff` 一筆，內容是整份管理摘要）留最近幾版：它們只寫不讀（`supervisors.summary` 才是現值，
/// 也寫在 AGM 目錄的 `handoff.md`），正式庫兩週 274 筆、10 MB，是 `supervisor_notes` 最大的一塊。
pub const HANDOFF_NOTES_KEPT: i64 = 20;

pub async fn set_summary(pool: &SqlitePool, summary: &str) -> Result<i64> {
    let mut tx = pool.begin().await?;
    let version: i64 = sqlx::query_scalar(
        "UPDATE supervisors SET summary=?, summary_version=summary_version+1, updated_at=? WHERE id=? RETURNING summary_version",
    )
    .bind(summary)
    .bind(crate::db::now())
    .bind(SUPERVISOR_ID)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO supervisor_notes (id, supervisor_id, kind, body, version, created_at) VALUES (?,?,?,?,?,?)")
        .bind(crate::db::ulid())
        .bind(SUPERVISOR_ID)
        .bind("handoff")
        .bind(summary)
        .bind(version)
        .bind(crate::db::now())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM supervisor_notes WHERE supervisor_id=? AND kind='handoff' AND rowid NOT IN
           (SELECT rowid FROM supervisor_notes WHERE supervisor_id=? AND kind='handoff' ORDER BY version DESC, rowid DESC LIMIT ?)",
    )
    .bind(SUPERVISOR_ID)
    .bind(SUPERVISOR_ID)
    .bind(HANDOFF_NOTES_KEPT)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(version)
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

/// Write the assignment down *before* anything is sent. A crash between here and the prompt
/// leaves a `queued` row the controller picks up again with the same client_request_id.
///
/// 測試夾具用；正式路徑一律走 [`insert_assignment_linked`]（連結跟列一起寫，issue #136）。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub async fn insert_assignment(
    pool: &SqlitePool,
    request_id: Option<&str>,
    target_bot_id: &str,
    client_request_id: &str,
    text: &str,
    ownership: &[String],
    follow_up_of: Option<&str>,
    expects_review: bool,
) -> Result<Assignment> {
    insert_assignment_linked(pool, request_id, target_bot_id, client_request_id, text, ownership, follow_up_of, expects_review, None).await
}

/// 建立交辦並同時記錄驗收角色。
#[allow(clippy::too_many_arguments)]
pub async fn insert_assignment_linked(
    pool: &SqlitePool,
    request_id: Option<&str>,
    target_bot_id: &str,
    client_request_id: &str,
    text: &str,
    ownership: &[String],
    follow_up_of: Option<&str>,
    expects_review: bool,
    review_role: Option<&str>,
) -> Result<Assignment> {
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO supervisor_assignments
           (id, supervisor_id, request_id, target_bot_id, client_request_id, text, status, attempts,
            ownership_json, follow_up_of, expects_review, review_role, created_at, updated_at)
         VALUES (?,?,?,?,?,?, 'queued', 0, ?, ?, ?, ?, ?, ?)",
    )
    .bind(crate::db::ulid())
    .bind(SUPERVISOR_ID)
    .bind(request_id)
    .bind(target_bot_id)
    .bind(client_request_id)
    .bind(text)
    .bind((!ownership.is_empty()).then(|| serde_json::to_string(ownership).unwrap_or_default()))
    .bind(follow_up_of)
    .bind(i64::from(expects_review))
    .bind(review_role)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(assignment_by_crid(pool, client_request_id).await?.expect("just inserted"))
}

/// 回合結束時 run 上記的錯誤原因（撞限、API 錯誤…）。
pub async fn set_turn_error(pool: &SqlitePool, id: &str, turn_error: &str) -> Result<()> {
    sqlx::query("UPDATE supervisor_assignments SET turn_error = ? WHERE id = ?")
        .bind(turn_error)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_assignments(pool: &SqlitePool, limit: i64) -> Result<Vec<Assignment>> {
    list_assignments_page(pool, None, limit, StatusFilter::All).await
}

/// 交辦清單的一頁，新的在前。`before` = 上一頁最後一筆的 `(created_at, id)`。
///
/// 有游標才翻得完：這支以前只回最新 `limit` 筆，而 `bin/agm` 的 `--open`／`--id` 是在
/// **客戶端**過濾——卡最久的那幾筆（`blocked` 天生活得比一頁久）於是從「未結案」裡整個消失
/// （issue #515，正式機上 6 筆未結案有 3 筆在頁外）。排序補上 `id` 才是全序：`created_at`
/// 同毫秒的兩筆之間沒有定序的話，翻頁會漏掉或重複（同 `supervisor_evidence` 的游標）。
pub async fn list_assignments_page(
    pool: &SqlitePool,
    before: Option<(&str, &str)>,
    limit: i64,
    status: StatusFilter<'_>,
) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(&format!(
        "SELECT * FROM supervisor_assignments
          WHERE supervisor_id=?1 {}
            AND (?2 IS NULL OR created_at < ?2 OR (created_at=?2 AND id < ?3))
          ORDER BY created_at DESC, id DESC LIMIT ?4",
        status.sql()
    ))
    .bind(SUPERVISOR_ID)
    .bind(before.map(|c| c.0))
    .bind(before.map(|c| c.1))
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// 清單要不要在 **SQL 裡**先篩掉。呼叫端過濾的話，「只想看 6 筆未結案」得先把整張表搬回去
/// 一頁一頁翻（issue #543）；而未結案的那幾筆天生是最舊的，翻頁成本正好落在最需要的答案上。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusFilter<'a> {
    /// 不篩。
    #[default]
    All,
    /// 還欠著的（`OPEN_STATES`）。
    Open,
    /// 剛好是這一個狀態。
    One(&'a str),
}

impl StatusFilter<'_> {
    /// 接在 `WHERE supervisor_id=?1` 後面的那一段。狀態字串**不從外面拼進 SQL**：
    /// `One` 只收認得的狀態（見 [`StatusFilter::parse`]），拼進去的永遠是這個模組自己的常數。
    fn sql(&self) -> String {
        match self {
            Self::All => String::new(),
            Self::Open => format!("AND status IN ({})", sql_list(&OPEN_STATES)),
            Self::One(s) => format!("AND status = '{s}'"),
        }
    }
}

impl<'a> StatusFilter<'a> {
    /// `open`＝未結案那一組，其餘要剛好是一個認得的狀態；不認得的回 `None`（呼叫端回 400，
    /// 不要靜靜地當成「沒有這個狀態的交辦」回一份空清單）。
    pub fn parse(raw: &'a str) -> Option<Self> {
        match raw.trim() {
            "" => Some(Self::All),
            "open" => Some(Self::Open),
            // 狀態清單只准有一份：`assignment_state::ALL` 是那一份（`sources_for` 也掃它）。
            // 在這裡抄第四份，改表的時候就會有人抄不走鐘。
            s => super::assignment_state::ALL.iter().find(|k| **k == s).map(|k| Self::One(k)),
        }
    }
}

/// 最近 `limit` 筆，**加上**掉在那一頁外面的未結案交辦。
///
/// 「最近幾筆」跟「還欠著幾件」是兩件事：`blocked`／`awaiting_review` 天生活得比一頁久，
/// 只看一頁的話 handoff 的 `open_assignments` 會少報，而少報的正是最該被看見的那幾筆
/// （issue #515：正式庫 1287 筆，5 筆未結案有 3 筆在最新 200 之外）。
pub async fn list_assignments_with_open(pool: &SqlitePool, limit: i64) -> Result<Vec<Assignment>> {
    let mut rows = list_assignments(pool, limit).await?;
    let seen: std::collections::HashSet<String> = rows.iter().map(|a| a.id.clone()).collect();
    rows.extend(unsettled_assignments(pool).await?.into_iter().filter(|a| !seen.contains(&a.id)));
    rows.sort_by(|a, b| (&b.created_at, &b.id).cmp(&(&a.created_at, &a.id)));
    Ok(rows)
}

/// The controller's work list: assignments the daemon itself can still move (dispatch, retry,
/// close against their turn). An `awaiting_review` row is *not* here — nothing the daemon does
/// advances it, only a decision does.
pub async fn open_assignments(pool: &SqlitePool) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        "SELECT * FROM supervisor_assignments WHERE supervisor_id=? AND status IN ('queued','delivered','unknown')
          ORDER BY created_at ASC, rowid ASC",
    )
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?)
}



/// Work that has never reached anybody: still `queued`, first created before `cutoff`.
///
/// This needs its own query because [`assignments_idle_since`] cannot see it. Every retry calls
/// `defer`, which moves `updated_at`, so an assignment bouncing off a stopped bot every five
/// minutes looks *busy* forever and never trips the idle probe — it would retry until the heat
/// death of the universe with nobody told. `created_at` is the honest clock here: it is how
/// long the work has been owed, regardless of how many times we have failed to hand it over.
pub async fn assignments_undelivered_since(pool: &SqlitePool, cutoff: &str) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        "SELECT * FROM supervisor_assignments WHERE supervisor_id=? AND status='queued'
           AND turn_id IS NULL AND created_at <= ? ORDER BY created_at ASC",
    )
    .bind(SUPERVISOR_ID)
    .bind(cutoff)
    .fetch_all(pool)
    .await?)
}

/// Assignments that have been sitting in one open state without moving since `cutoff` — the
/// stalled-work probe behind the `assignment_stalled` incident.
pub async fn assignments_idle_since(pool: &SqlitePool, cutoff: &str) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        // 兩種「不動」不是卡住：通知本來就沒人要驗收（它也不會停在 awaiting_review），
        // 被額度擋下的那種是**在等一個已知的時間點**，controller 自己會重送。把它們報成
        // incident 只會讓 AGM 每兩小時被叫醒一次去看一件沒有人需要做的事。清單見 STALLED_STATES。
        &format!(
            "SELECT * FROM supervisor_assignments WHERE supervisor_id=?
               AND status IN ({})
               AND NOT (expects_review=0 AND status='awaiting_review')
               AND updated_at <= ? ORDER BY updated_at ASC",
            sql_list(&STALLED_STATES)
        ),
    )
    .bind(SUPERVISOR_ID)
    .bind(cutoff)
    .fetch_all(pool)
    .await?)
}

/// 記下派出去的結果。回 `false` ＝ 這筆交辦已經不在途中（多半是派送途中被取消／裁示掉了），什麼都沒寫。
///
/// **這裡的 `WHERE` 就是 issue #71 的守衛。** 以前是 `WHERE id=?`：`dispatch` 從讀到這筆 `queued`
/// 交辦、到 `prompt_relayed_queueable` 送完回來中間有好幾個 await，那段時間裡 AGM 或使用者把它取消掉
/// （`review_with_followup` 走 CAS 寫進 `cancelled`）是完全做得到的；接著這一句無條件把它寫回
/// `delivered`，一筆已經結案的工作就這樣活過來，AGM 看到的是「還在跑」。
/// 合法的來源只有在途那三個（[`assignment_state::IN_FLIGHT`]）——`delivered`／`unknown` 也算，
/// 因為同一個 crid 重派本來就是冪等的，會再記一次。
pub async fn mark_delivered(pool: &SqlitePool, id: &str, turn_id: &str, delivery: &str) -> Result<bool> {
    // `unknown` delivery is its own state: the prompt may or may not have landed, so the
    // controller reconciles it against the turn instead of sending the work a second time.
    let status = if delivery == "unknown" { "unknown" } else { "delivered" };
    let from = sql_list(&assignment_state::sources_for(status));
    Ok(sqlx::query(&format!(
        "UPDATE supervisor_assignments SET turn_id=?, delivery=?, status=?, attempts=attempts+1,
           next_attempt_at=NULL, conflict_since=NULL, updated_at=? WHERE id=? AND status IN ({from})"
    ))
    .bind(turn_id)
    .bind(delivery)
    .bind(status)
    .bind(crate::db::now())
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// Hold a queued assignment until `until` **without** spending an attempt.
///
/// Waiting out a maintenance window is not a failed delivery: counting it would push the
/// assignment up the backoff ladder, and enough windows would eventually retire work that was
/// never actually tried. For the same reason the 409 streak ends here (`conflict_since`).
pub async fn hold(pool: &SqlitePool, id: &str, until: &str, why: &str) -> Result<()> {
    sqlx::query(
        "UPDATE supervisor_assignments SET next_attempt_at=?, error=?, conflict_since=NULL, updated_at=? WHERE id=? AND status='queued'",
    )
    .bind(until)
    .bind(why)
    .bind(crate::db::now())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Lift every hold a restart window put on queued assignments (their `error` is the
/// `pause_note`). A window ends when its lease is released or the daemon comes back, not at the
/// deadline written into the hold; returns how many were lifted.
pub async fn clear_restart_holds(pool: &SqlitePool) -> Result<u64> {
    Ok(sqlx::query(
        "UPDATE supervisor_assignments SET next_attempt_at=NULL, error=NULL, conflict_since=NULL, updated_at=?
          WHERE status='queued' AND error LIKE 'restart window held until %'",
    )
    .bind(crate::db::now())
    .execute(pool)
    .await?
    .rows_affected())
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

/// 409（對方回合中、沒有 run、框裡有字…）：排下一次，記下這一輪第一次撞 409 的時間，
/// 並把「排不進去幾輪」加一。`conflict_since` 已經有就不動——保險絲量的是「連續送不進去多久」。
///
/// **不加 `attempts`**（issue #528）：那個數字是 `dispatch` 給上游／暫時性錯誤的重試上限
/// （`a.attempts >= RETRY_BACKOFF.len()` 就判 `dispatch_failed`）。409 是「對方正忙」，
/// 不是一次失敗的派送；混在一起的話，一顆回合中的 bot 只要讓它排八分鐘（15+30+60+120+240 秒）
/// 就把上限用完，之後第一個 herdr 抖動就把使用者以為在跑的工作判成失敗。409 自己的界是
/// `conflict_since` 的時間與 [`crate::supervisor::controller`] 的輪數上限。
pub async fn defer_conflict(pool: &SqlitePool, id: &str, next_attempt_at: &str, why: &str) -> Result<()> {
    let now = crate::db::now();
    sqlx::query(
        "UPDATE supervisor_assignments SET busy_rounds=busy_rounds+1, next_attempt_at=?, error=?,
                conflict_since=COALESCE(conflict_since, ?), updated_at=?
          WHERE id=? AND status='queued'",
    )
    .bind(next_attempt_at)
    .bind(why)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 一直送不進去：標成 `blocked` 而**不是** `dispatch_failed`——工作沒有失敗，是進不去那顆 bot。
/// `blocked` 仍在 `OPEN_STATES` 裡（不會從未結案與 ownership 衝突裡消失），但不再每隔幾分鐘重試。
///
/// 只從 `queued` 動手——比表上「哪些狀態能走到 `blocked`」窄（那張表容許任何未結案狀態，因為
/// `blocked` 也是一種裁示），是刻意的：`mark_undeliverable` 講的是「這一次派送就是進不去」，
/// `delivered`／`unknown` 已經送出去了，進不去的保險絲是 [`block_stale_queue_tx`] 的事，兩者不能共用
/// 同一個更寬的 guard。guard 跟 CAS 走 [`assignment_state::set_status_on`]（issue #71 第二刀），
/// 這裡自己只管 `queued → blocked` 以外的欄位。
pub async fn mark_undeliverable(pool: &SqlitePool, id: &str, why: &str, event_key: &str, payload: &Value) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let moved = matches!(
        assignment_state::set_status_on(&mut tx, id, assignment_state::AssignmentState::Queued, assignment_state::AssignmentState::Blocked, why).await?,
        assignment_state::Outcome::Applied
    );
    if moved {
        sqlx::query("UPDATE supervisor_assignments SET error=?, next_attempt_at=NULL, updated_at=? WHERE id=?")
            .bind(why)
            .bind(crate::db::now())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // 通知同一個交易（#283）：寫不進去就整組不成立，下一輪重來，不留下沒人知道的 blocked。
        let bot: String = sqlx::query_scalar("SELECT target_bot_id FROM supervisor_assignments WHERE id=?").bind(id).fetch_one(&mut *tx).await?;
        push_inbox_tx(&mut tx, event_key, "assignment_undeliverable", Some(id), Some(&bot), None, payload).await?;
    }
    tx.commit().await?;
    Ok(moved)
}

/// What one execution outcome did to the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    /// The assignment moved from an executing state into `awaiting_review`.
    pub moved: bool,
    /// The notification was new (not a replay of one already queued).
    pub event_new: bool,
}

/// The execution half ended: park the assignment on `awaiting_review` and queue its
/// notification **in one transaction**.
///
/// The two used to be separate statements, and a daemon that died between them left an
/// assignment closed with nobody ever told about it — the restart rescan only looks at open
/// rows, so that result was gone for good. Either both land or neither does.
///
/// `turn_status` is the turn's own word (`completed`, `completed_fallback`, `failed`,
/// `dispatch_failed`); it is recorded, not interpreted. Nothing here accepts anything: only
/// [`review`] closes an assignment.
#[allow(clippy::too_many_arguments)]
pub async fn settle_and_notify(
    pool: &SqlitePool,
    id: &str,
    turn_status: &str,
    evidence_complete: bool,
    result: Option<&str>,
    error: Option<&str>,
    event_key: &str,
    kind: &str,
    payload: &Value,
) -> Result<Settled> {
    let mut tx = pool.begin().await?;
    let s = settle_and_notify_on(&mut tx, id, turn_status, evidence_complete, result, error, event_key, kind, payload).await?;
    tx.commit().await?;
    Ok(s)
}

/// [`settle_and_notify`] 的交易內版本：要跟別的寫入（例如任務停下來問人，issue #160）一起成立或一起不發生時用。
/// 不 commit，由呼叫端決定。
#[allow(clippy::too_many_arguments)]
pub async fn settle_and_notify_on(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
    turn_status: &str,
    evidence_complete: bool,
    result: Option<&str>,
    error: Option<&str>,
    event_key: &str,
    kind: &str,
    payload: &Value,
) -> Result<Settled> {
    let now = crate::db::now();
    // 通知（`expects_review=0`）而且回合是正常結束的：沒有東西要驗收，當場結案。
    // 這是 2026-09-12 兩件 `assignment_stalled` incident 的根因——AGM 說一句「收到」也要
    // 它自己回頭 review，沒 review 就被當成卡住的工作。
    let ok_turn = turn_status == "completed" || turn_status == "completed_fallback";
    let land_on = if ok_turn { "CASE WHEN expects_review=0 THEN 'completed' ELSE 'awaiting_review' END" } else { "'awaiting_review'" };
    // 守衛從轉移表算出來，不再自己抄一份 `('queued','delivered','unknown')`（issue #71）。
    // `completed` 那一支（通知當場結案）的來源跟 `awaiting_review` 相同，用同一份即可。
    let from = sql_list(&assignment_state::sources_for("awaiting_review"));
    let moved = sqlx::query(&format!(
        "UPDATE supervisor_assignments
            SET status={land_on}, turn_status=?, evidence_complete=?,
                result=COALESCE(?, result), error=COALESCE(?, error), completed_at=?, updated_at=?
          WHERE id=? AND status IN ({from})",
    ))
    .bind(turn_status)
    .bind(i64::from(evidence_complete))
    .bind(result)
    .bind(error)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        > 0;
    // `moved=false`：guard 沒讓這次真的轉移（例如這一列已經被 AGM cancel 掉，或別的呼叫已經
    // settle 過）。這次呼叫沒有造成任何新事實，不該無條件排一則「請驗收」的通知——已經關掉的
    // 交辦不能讓 AGM 收到「這件做完了、請驗收」的訊息（issue #99）。這一列現在合不合法一律問
    // `assignment_state`（上面 `from` 那句已經問過），這裡不另外設一套判斷。
    let event_new = if moved {
        let (bot_id, turn_id, expects_review, status): (String, Option<String>, i64, String) =
            sqlx::query_as("SELECT target_bot_id, turn_id, expects_review, status FROM supervisor_assignments WHERE id=?")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        // 通知結案了就不要再叫 AGM 來看：inbox 事件改成 `assignment_noticed`，digest 與
        // `needs_review` 都不會把它算成待辦。失敗的通知照舊走原本那條（它真的需要有人看）。
        let kind = if expects_review == 0 && status == "completed" { "assignment_noticed" } else { kind };
        let payload = if expects_review == 0 && status == "completed" {
            let mut p = payload.clone();
            p["needs_review"] = Value::Bool(false);
            p["kind"] = Value::String("notice".into());
            p
        } else {
            payload.clone()
        };
        sqlx::query(
            "INSERT OR IGNORE INTO supervisor_inbox
               (id, supervisor_id, event_key, assignment_id, bot_id, turn_id, kind, payload_json, state, created_at, updated_at)
             VALUES (?,?,?,?,?,?,?,?, 'pending', ?, ?)",
        )
        .bind(crate::db::ulid())
        .bind(SUPERVISOR_ID)
        .bind(event_key)
        .bind(id)
        .bind(&bot_id)
        .bind(&turn_id)
        .bind(kind)
        .bind(payload.to_string())
        .bind(&now)
        .bind(&now)
        .execute(&mut **tx)
        .await?
        .rows_affected()
            > 0
    } else {
        false
    };
    Ok(Settled { moved, event_new })
}

/// 額度撞牆：把 assignment 停在 `quota_blocked`，並在**同一個交易**裡通知 AGM。
///
/// 這不是失敗，也不是要人來看的東西：帳號的用量上限會在 `resume_at` 之後自己回來，
/// controller 屆時用下一個 `#r<n>` 重送。之所以要單獨一個狀態而不是塞回 `queued`，是因為
/// queued 每 10 秒就會被試一次——撞上限的帳號會被一路重試到 backoff 用完，然後變成
/// `dispatch_failed`，工作就這樣無聲斷掉（2026-09-12 codex-astra 三次都是這樣）。
/// 排進佇列後等太久還沒送出：停在 `blocked` 並留下理由，讓 AGM 看得到（AGM 2026-09-16）。
/// 只動 `delivered` 的列（就是排隊中的那些），回 true = 這次真的把它擋下了。
/// 保險絲要「turn 撤成功才標 blocked」，兩個寫入同一個交易。
///
/// 只從 `delivered` 動手——比表上的一般 guard 窄，理由跟 [`mark_undeliverable`] 同一種：這支管的
/// 是「排太久沒送出」的那條保險絲，不是任何一種裁示。guard 跟 CAS 走
/// [`assignment_state::set_status_on`]（issue #71 第二刀）。
pub async fn block_stale_queue_tx(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, id: &str, why: &str) -> Result<bool> {
    let moved = matches!(
        assignment_state::set_status_on(tx, id, assignment_state::AssignmentState::Delivered, assignment_state::AssignmentState::Blocked, why).await?,
        assignment_state::Outcome::Applied
    );
    if moved {
        sqlx::query("UPDATE supervisor_assignments SET error=?, updated_at=? WHERE id=?")
            .bind(why)
            .bind(crate::db::now())
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(moved)
}

pub async fn park_quota_blocked(
    pool: &SqlitePool,
    id: &str,
    resume_at: &str,
    why: &str,
    event_key: &str,
    payload: &Value,
) -> Result<Settled> {
    let now = crate::db::now();
    let mut tx = pool.begin().await?;
    let from = sql_list(&assignment_state::sources_for("quota_blocked"));
    let moved = sqlx::query(&format!(
        "UPDATE supervisor_assignments
            SET status='quota_blocked', resume_at=?, error=?, conflict_since=NULL, updated_at=?
          WHERE id=? AND status IN ({from})"
    ))
    .bind(resume_at)
    .bind(why)
    .bind(&now)
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    // guard 擋下（這一列已經被裁示掉、或別的路徑先 park 過）就沒有新事實：不推「在等額度」的通知
    // （issue #110，跟 `settle_and_notify`／`resume_quota_blocked` 同一條規則）。
    if !moved {
        tx.commit().await?;
        return Ok(Settled { moved, event_new: false });
    }
    let (bot_id, turn_id): (String, Option<String>) =
        sqlx::query_as("SELECT target_bot_id, turn_id FROM supervisor_assignments WHERE id=?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let event_new = sqlx::query(
        "INSERT OR IGNORE INTO supervisor_inbox
           (id, supervisor_id, event_key, assignment_id, bot_id, turn_id, kind, payload_json, state, created_at, updated_at)
         VALUES (?,?,?,?,?,?, 'assignment_quota_blocked', ?, 'pending', ?, ?)",
    )
    .bind(crate::db::ulid())
    .bind(SUPERVISOR_ID)
    .bind(event_key)
    .bind(id)
    .bind(&bot_id)
    .bind(&turn_id)
    .bind(payload.to_string())
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    tx.commit().await?;
    Ok(Settled { moved, event_new })
}

/// 額度回來了（或 `resume_at` 已經過去）：回到 `queued`，重送次數 +1，並通知 AGM 一聲。
///
/// `turn_id` 一併清掉：下一次是新的 turn（見 [`Assignment::dispatch_crid`]），留著舊的會讓
/// `assignment_by_turn` 把新舊兩回合混在一起。
///
/// 只有真的轉移了才推通知（issue #71 第二刀時發現：這裡以前不看 guard 有沒有擋下，被別的路徑
/// 先裁示掉的交辦還是會排一則「額度回來、重新排隊」的通知——跟 `settle_and_notify` 那條 issue #99
/// 修過的縫是同一種形狀，只是換了一個通知種類）。guard 跟 CAS 走
/// [`assignment_state::set_status_on`]。
pub async fn resume_quota_blocked(pool: &SqlitePool, id: &str, event_key: &str, payload: &Value) -> Result<Settled> {
    let now = crate::db::now();
    let mut tx = pool.begin().await?;
    let moved = matches!(
        assignment_state::set_status_on(&mut tx, id, assignment_state::AssignmentState::QuotaBlocked, assignment_state::AssignmentState::Queued, "額度回來，重新排隊").await?,
        assignment_state::Outcome::Applied
    );
    let event_new = if moved {
        sqlx::query(
            "UPDATE supervisor_assignments
                SET quota_retries=quota_retries+1, resume_at=NULL, turn_id=NULL, delivery=NULL,
                    next_attempt_at=NULL, attempts=0, updated_at=?
              WHERE id=?",
        )
        .bind(&now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        let (bot_id,): (String,) = sqlx::query_as("SELECT target_bot_id FROM supervisor_assignments WHERE id=?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO supervisor_inbox
               (id, supervisor_id, event_key, assignment_id, bot_id, kind, payload_json, state, created_at, updated_at)
             VALUES (?,?,?,?,?, 'assignment_quota_resumed', ?, 'pending', ?, ?)",
        )
        .bind(crate::db::ulid())
        .bind(SUPERVISOR_ID)
        .bind(event_key)
        .bind(id)
        .bind(&bot_id)
        .bind(payload.to_string())
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0
    } else {
        false
    };
    tx.commit().await?;
    Ok(Settled { moved, event_new })
}

/// 到了預計時間仍被擋：順延 `resume_at`，**並算一次** `quota_retries`（「重送 N 次仍被擋」的 N），
/// 不動狀態也不再發一次通知。以前只改時間不算次數，沒寫時間的撞限永遠到不了 `quota_exhausted`。
pub async fn extend_quota_blocked(pool: &SqlitePool, id: &str, resume_at: &str) -> Result<()> {
    sqlx::query("UPDATE supervisor_assignments SET resume_at=?, quota_retries=quota_retries+1, updated_at=? WHERE id=? AND status='quota_blocked'")
        .bind(resume_at)
        .bind(crate::db::now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 被額度擋著的全部（不管時間到了沒）——重送前要再問一次額度現況時用。
pub async fn quota_blocked_all(pool: &SqlitePool) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        "SELECT * FROM supervisor_assignments WHERE supervisor_id=? AND status='quota_blocked' ORDER BY updated_at ASC",
    )
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?)
}

/// A terminal assignment whose completion event never made it into the inbox.
///
/// Belt and braces for [`settle_and_notify`]: rows closed by an older daemon (or by a path that
/// predates the transaction) are swept up at startup instead of being silently dropped.
pub async fn settled_without_event(pool: &SqlitePool, limit: i64) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        "SELECT a.* FROM supervisor_assignments a
          WHERE a.supervisor_id=? AND a.status='awaiting_review' AND a.legacy_closed=0
            AND NOT EXISTS (SELECT 1 FROM supervisor_inbox i WHERE i.assignment_id=a.id
                              AND i.kind IN ('assignment_completed','assignment_failed'))
          ORDER BY a.updated_at ASC LIMIT ?",
    )
    .bind(SUPERVISOR_ID)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// The decision a `decision` string maps to. `None` = not a decision we know.
pub fn decision_status(decision: &str) -> Option<&'static str> {
    match decision {
        "accept" => Some("completed"),
        "fail" => Some("failed"),
        "cancel" => Some("cancelled"),
        "block" => Some("blocked"),
        "followup" => Some("superseded"),
        _ => None,
    }
}

/// Record an acceptance decision. 轉移合不合法問 [`assignment_state::allowed`]（issue #71）：以前
/// 註解寫「呼叫端已經檢查過了」，但那是一個沒有人保證得了的約定——結案的交辦再被裁示一次就會把
/// `reviewed_at` 覆蓋掉。不合法就回 `Ok(None)`，跟「找不到這筆」同一個出口。
///
/// **status 這一欄本身的寫入走 [`assignment_state::set_status_on`]**（issue #71 第二刀）：以前是
/// `WHERE id=?`，讀完 `before` 到這句 UPDATE 之間別的路徑先動手也不會被擋——只有測試在用這支，
/// 真的撞上的機率低，但既然要收成單一入口，這裡不該是唯一一處還在自己下裸的 SQL。CAS 綁的就是
/// 剛讀到的 `before.status`：別人先動了手，`set_status_on` 回 `Raced`，這裡跟著回 `Ok(None)`。
///
/// Idempotent by construction at the API layer: re-deciding the same way is answered from the
/// row without writing a second audit entry.
#[allow(clippy::too_many_arguments)]
pub async fn review(
    pool: &SqlitePool,
    id: &str,
    decision: &str,
    actor: &str,
    source: &str,
    reason: Option<&str>,
    evidence: Option<&str>,
    followup_assignment_id: Option<&str>,
) -> Result<Option<Assignment>> {
    let Some(to_status) = decision_status(decision) else { return Ok(None) };
    let Some(before) = assignment(pool, id).await? else { return Ok(None) };
    if !assignment_state::allowed(&before.status, to_status) {
        return Ok(None);
    }
    // 表已經說這一步合法，字串一定解得回去這兩個 enum——但用 `let...else` 不 panic：壞資料
    // 不該把整個請求打死，而是照「沒有這個轉移」處理。
    let (Some(from), Some(to)) = (assignment_state::AssignmentState::parse(&before.status), assignment_state::AssignmentState::parse(to_status)) else {
        return Ok(None);
    };
    let now = crate::db::now();
    let mut tx = pool.begin().await?;
    if !matches!(assignment_state::set_status_on(&mut tx, id, from, to, "review").await?, assignment_state::Outcome::Applied) {
        return Ok(None);
    }
    sqlx::query(
        "UPDATE supervisor_assignments
            SET review_decision=?, reviewed_by=?, reviewed_at=?, review_reason=?,
                followup_assignment_id=COALESCE(?, followup_assignment_id), updated_at=?
          WHERE id=?",
    )
    .bind(decision)
    .bind(actor)
    .bind(&now)
    .bind(reason)
    .bind(followup_assignment_id)
    .bind(&now)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO supervisor_reviews
           (id, supervisor_id, assignment_id, decision, from_status, to_status, actor, source,
            reason, evidence, followup_assignment_id, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(crate::db::ulid())
    .bind(SUPERVISOR_ID)
    .bind(id)
    .bind(decision)
    .bind(&before.status)
    .bind(to_status)
    .bind(actor)
    .bind(source)
    .bind(reason)
    .bind(evidence)
    .bind(followup_assignment_id)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    assignment(pool, id).await
}

/// The continuation to create alongside a `followup` decision.
pub struct FollowupSpec<'a> {
    pub target_bot_id: &'a str,
    pub client_request_id: &'a str,
    pub text: &'a str,
    pub ownership: &'a [String],
    /// The user request the parent traces back to, carried so the continuation answers the same
    /// ask rather than inventing a new one.
    pub request_id: Option<&'a str>,
}

/// What the decision did. `None` from [`review_with_followup`] means somebody else decided
/// first and nothing was written.
pub struct Decided {
    pub updated: Assignment,
    pub followup: Option<Assignment>,
    /// 這次決定寫下的稽核列；撤回 queued turn 之後要改寫它的 evidence（[`amend_review_evidence`]）。
    pub review_id: String,
}

/// `followup_request_id` 已經是**另一件**交辦的 client_request_id（不是這個 parent 的同一份續作）。
/// 具名錯誤，API 才能回 409 `followup_request_id_taken`：以前是 `anyhow::bail!` → 502 upstream，
/// 看起來像 daemon 壞了，照 5xx 重試同一個 id 永遠 502，看不出該換 id（review 2026-09-16 c1 L2）。
#[derive(Debug)]
pub struct FollowupIdTaken {
    pub client_request_id: String,
    pub assignment_id: String,
}

impl std::fmt::Display for FollowupIdTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "client_request_id {} already belongs to another assignment ({})", self.client_request_id, self.assignment_id)
    }
}

impl std::error::Error for FollowupIdTaken {}

/// Decide an assignment and, for `followup`, create its continuation — **in one transaction**,
/// guarded on the status we validated against.
///
/// Both halves of that matter and they fix different failures:
///
/// - *One* transaction, so a crash cannot leave a queued continuation whose parent was never
///   superseded (work nobody is tracking), or a superseded parent whose continuation does not
///   exist (work silently dropped).
/// - Guarded on `expect_status`, so two callers deciding the same assignment at once do not
///   both create a continuation. The loser's UPDATE matches no rows, the whole transaction
///   rolls back, and it is told it lost rather than quietly duplicating the work. This is why
///   the row must be committed before anything is *sent*: an assignment that has gone out
///   cannot be rolled back.
#[allow(clippy::too_many_arguments)]
pub async fn review_with_followup(
    pool: &SqlitePool,
    id: &str,
    expect_status: &str,
    decision: &str,
    actor: &str,
    source: &str,
    reason: Option<&str>,
    evidence: Option<&str>,
    followup: Option<FollowupSpec<'_>>,
) -> Result<Option<Decided>> {
    let Some(to_status) = decision_status(decision) else { return Ok(None) };
    let now = crate::db::now();
    let mut tx = pool.begin().await?;

    // The guard. Everything below only happens if the assignment is still where we left it.
    let moved = sqlx::query(
        "UPDATE supervisor_assignments
            SET status=?, review_decision=?, reviewed_by=?, reviewed_at=?, review_reason=?, updated_at=?
          WHERE id=? AND status=?",
    )
    .bind(to_status)
    .bind(decision)
    .bind(actor)
    .bind(&now)
    .bind(reason)
    .bind(&now)
    .bind(id)
    .bind(expect_status)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if !moved {
        // Nothing written; the caller re-reads to find out who decided and how.
        return Ok(None);
    }

    let mut created: Option<String> = None;
    if let Some(f) = followup {
        // A byte-identical retry of the same continuation is the same continuation. Any other
        // row already holding this id is a different piece of work and must not be adopted.
        let existing: Option<(String, Option<String>, String, String)> =
            sqlx::query_as("SELECT id, follow_up_of, target_bot_id, text FROM supervisor_assignments WHERE supervisor_id=? AND client_request_id=?")
                .bind(SUPERVISOR_ID)
                .bind(f.client_request_id)
                .fetch_optional(&mut *tx)
                .await?;
        let new_id = match existing {
            Some((existing_id, parent, target, text)) => {
                if parent.as_deref() != Some(id) || target != f.target_bot_id || text != f.text {
                    return Err(FollowupIdTaken { client_request_id: f.client_request_id.to_string(), assignment_id: existing_id }.into());
                }
                existing_id
            }
            None => {
                let new_id = crate::db::ulid();
                // `expects_review` 抄父交辦：通知（`--notice`）送不出去再 followup 重送，仍然是通知——
                // 以前這裡用欄位預設 1，續作變回要驗收的工單，兩小時後開 `assignment_stalled`（review 2026-09-16 c1 L6）。
                sqlx::query(
                    "INSERT INTO supervisor_assignments
                       (id, supervisor_id, request_id, target_bot_id, client_request_id, text, status, attempts,
                        ownership_json, follow_up_of, expects_review, created_at, updated_at)
                     VALUES (?,?,?,?,?,?, 'queued', 0, ?, ?,
                             COALESCE((SELECT expects_review FROM supervisor_assignments WHERE id = ?), 1), ?, ?)",
                )
                .bind(&new_id)
                .bind(SUPERVISOR_ID)
                .bind(f.request_id)
                .bind(f.target_bot_id)
                .bind(f.client_request_id)
                .bind(f.text)
                .bind((!f.ownership.is_empty()).then(|| serde_json::to_string(f.ownership).unwrap_or_default()))
                .bind(id)
                .bind(id)
                .bind(&now)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
                // Preserve the review role for the follow-up.
                sqlx::query(
                    "UPDATE supervisor_assignments
                        SET review_role = (SELECT review_role FROM supervisor_assignments WHERE id = ?)
                      WHERE id = ?",
                )
                .bind(id)
                .bind(&new_id)
                .execute(&mut *tx)
                .await?;
                new_id
            }
        };
        sqlx::query("UPDATE supervisor_assignments SET followup_assignment_id=?, updated_at=? WHERE id=?")
            .bind(&new_id)
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        created = Some(new_id);
    }

    let review_id = crate::db::ulid();
    sqlx::query(
        "INSERT INTO supervisor_reviews
           (id, supervisor_id, assignment_id, decision, from_status, to_status, actor, source,
            reason, evidence, followup_assignment_id, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&review_id)
    .bind(SUPERVISOR_ID)
    .bind(id)
    .bind(decision)
    .bind(expect_status)
    .bind(to_status)
    .bind(actor)
    .bind(source)
    .bind(reason)
    .bind(evidence)
    .bind(created.as_deref())
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let updated = assignment(pool, id).await?.expect("just updated");
    let followup = match created {
        Some(fid) => assignment(pool, &fid).await?,
        None => None,
    };
    Ok(Some(Decided { updated, followup, review_id }))
}

/// 改寫**同一個請求剛寫下**的稽核列的 evidence。只給 `post_review` 用：決定寫進去時還不知道排著的
/// turn 撤不撤得回來，撤回之後「turn 還在跑」那句就不成立，不能永久留在稽核裡讓後來的人不敢重派
/// （review 2026-09-16 c1 L1）。只在內容還是當初寫的那句時改，別的寫入不會被蓋掉。
pub async fn amend_review_evidence(pool: &SqlitePool, review_id: &str, expected: Option<&str>, evidence: Option<&str>) -> Result<bool> {
    Ok(sqlx::query("UPDATE supervisor_reviews SET evidence=? WHERE id=? AND evidence IS ?")
        .bind(evidence)
        .bind(review_id)
        .bind(expected)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

/// Every decision taken on one assignment, oldest first.
pub async fn reviews(pool: &SqlitePool, assignment_id: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query_as::<_, (String, String, String, String, String, String, Option<String>, Option<String>, Option<String>, String)>(
        "SELECT id, decision, from_status, to_status, actor, source, reason, evidence, followup_assignment_id, created_at
           FROM supervisor_reviews WHERE assignment_id=? ORDER BY created_at ASC",
    )
    .bind(assignment_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, decision, from, to, actor, source, reason, evidence, followup, at)| {
            json!({
                "id": id, "decision": decision, "from_status": from, "to_status": to,
                "actor": actor, "source": source, "reason": reason, "evidence": evidence,
                "followup_assignment_id": followup, "created_at": at,
            })
        })
        .collect())
}

/// Point a superseded assignment at the one that carries its work forward.
pub async fn link_followup(pool: &SqlitePool, id: &str, followup_id: &str) -> Result<()> {
    sqlx::query("UPDATE supervisor_assignments SET followup_assignment_id=?, updated_at=? WHERE id=?")
        .bind(followup_id)
        .bind(crate::db::now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- inbox

/// 處理完（`handled`）多久之後，過大的 payload 才被換成標記。
pub const COMPACT_HANDLED_AFTER_SECS: i64 = 24 * 3600;
/// 多大才算過大。一般事件的 payload 不到 2 KB；`health_changed` 把整份 `supervisor.assignments` 塞進去，約 280 KB。
pub const COMPACT_HANDLED_MIN_BYTES: i64 = 16 * 1024;

/// 把「處理完、放超過 `older_than_secs`、payload 超過 `min_bytes`」的 inbox 列的 payload 換成
/// `{"compacted":true,"original_bytes":N}`（加上原本就有的 `fingerprint`／`from_bot_id`，沒有的不補），回改了幾列。
/// 這兩個要留：`bot_request` 同一個 `client_request_id` 重送時，重播判斷看 `fingerprint`（壓掉就變成「同 id 換內容」的 409），
/// `reply_to` 的歸屬比對看 `from_bot_id`；json_patch 對 NULL 是「刪掉這個鍵」，所以缺的欄位不會變成 `null`。
///
/// 處理完的列沒有任何清理：正式庫 10 天 166 筆 `health_changed`、43 MB，是整顆 DB 最大的一塊。**不刪列**——
/// `event_key` 是去重鍵，`controller::sweep_missing_events` 看到「已結算卻沒有事件」的交辦會再補一筆，刪了會被重新生出來。
/// 只動 payload：列、鍵、狀態、時間都留著；沒處理完的（pending／delivered）不碰（還有人要讀）。冪等：壓過的已經很小。
pub async fn compact_handled_payloads(pool: &SqlitePool, older_than_secs: i64, min_bytes: i64) -> Result<u64> {
    let cutoff = crate::db::iso_in(-older_than_secs);
    let ts = crate::db::ts_sql("updated_at");
    let res = sqlx::query(&format!(
        "UPDATE supervisor_inbox
            SET payload_json = json_patch(
                    json_object('compacted', json('true'), 'original_bytes', length(payload_json)),
                    json_object('fingerprint', json_extract(payload_json, '$.fingerprint'),
                                'from_bot_id', json_extract(payload_json, '$.from_bot_id')))
          WHERE state = 'handled' AND length(payload_json) > ? AND {ts} <= ?"
    ))
    .bind(min_bytes)
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// 處理完（`handled`）超過這麼久的「流水帳」事件整列刪掉（60 天）。
pub const PRUNE_HANDLED_AFTER_SECS: i64 = 60 * 24 * 3600;
/// 刪得掉的種類：沒有任何東西靠它們的 `event_key` 去重。**不含**交辦結果（`assignment_*`）與核准等：
/// `sweep_missing_events` 看到「已結算卻沒有事件」的交辦會再補一筆，刪了會被重新生出來。
pub const PRUNABLE_KINDS: [&str; 3] = ["health_changed", "ops_alert", "bot_request"];

/// `compact_handled_payloads` 只壓大於 16 KB 的 payload、而且永遠不刪列：小的事件（每小時一則的 `ops_alert`、bot 的申請…）
/// 一年下來仍是一張只增不減的表。流水帳類的事件處理完放了 60 天就整列刪掉；host baseline 的永久去重 key 例外保留；其他種類、還沒處理完的（pending／delivered／gave_up）、
/// 比較新的一個字都不動。回刪了幾列。
pub async fn prune_handled_events(pool: &SqlitePool, older_than_secs: i64) -> Result<u64> {
    let cutoff = crate::db::iso_in(-older_than_secs);
    let ts = crate::db::ts_sql("updated_at");
    let kinds = PRUNABLE_KINDS.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(",");
    let res = sqlx::query(&format!(
        "DELETE FROM supervisor_inbox
          WHERE supervisor_id = ? AND state = 'handled' AND kind IN ({kinds}) AND {ts} <= ?
            AND NOT (kind = 'ops_alert' AND event_key GLOB 'ops_alert:daemon:host_baseline:*')"
    ))
    .bind(SUPERVISOR_ID)
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
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
///
/// 讀取端改走 `roles::list_for`（角色條件要在 SQL 的 LIMIT 之前）；這一支留給既有測試釘住
/// 「未 ack 的順序」這條規則。
#[cfg(test)]
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

/// Assignments not yet settled: in flight, waiting for acceptance, or blocked. A turn that
/// ended does not take its assignment off this count — only a decision does.
pub async fn open_assignment_count(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM supervisor_assignments WHERE supervisor_id=?
           AND status IN ({})",
        sql_list(&OPEN_STATES)
    ))
    .bind(SUPERVISOR_ID)
    .fetch_one(pool)
    .await?)
}

/// Of those, the ones nobody has accepted or rejected yet.
pub async fn awaiting_review_count(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM supervisor_assignments WHERE supervisor_id=? AND status='awaiting_review'",
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

// 單角色時代的讀寫，現在由 roles.rs 的角色版本取代；留給既有測試釘住同一組守衛。
#[cfg(test)]
/// Prompted, not yet confirmed. Deliberately *not* `handled`: only the manager acking it —
/// or the UI acking on its behalf — closes the event, so a failed notify keeps it queued.
pub async fn mark_delivered_inbox(
    pool: &SqlitePool,
    ids: &[String],
    notify_turn_id: &str,
    delivery: &str,
) -> Result<()> {
    let now = crate::db::now();
    for id in ids {
        // `AND state!='handled'`: the manager can ack the digest before this write lands (it
        // reads the events out of the prompt, not out of this row). Without the guard that ack
        // is undone and the same event is pushed at it again — an acknowledgement must never
        // go backwards.
        sqlx::query(
            "UPDATE supervisor_inbox
                SET state='delivered', notify_turn_id=?, notify_delivery=?, delivered_at=?,
                    notify_attempts=notify_attempts+1, notify_next_at=NULL, notify_error=NULL, updated_at=?
              WHERE id=? AND state!='handled'",
        )
        .bind(notify_turn_id)
        .bind(delivery)
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// A notify attempt that did not land. The events stay `pending`; only the attempt counter and
/// the backoff move, so a flaky transport cannot silently drop a window's worth of results.
pub async fn defer_notify(pool: &SqlitePool, ids: &[String], next_at: &str, why: &str) -> Result<()> {
    let now = crate::db::now();
    for id in ids {
        sqlx::query(
            "UPDATE supervisor_inbox
                SET notify_attempts=notify_attempts+1, notify_next_at=?, notify_error=?, updated_at=?
              WHERE id=? AND state!='handled'",
        )
        .bind(next_at)
        .bind(why)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Put a delivered-but-unanswered event back in the queue.
///
/// Used when the notify turn itself failed or was interrupted, and when a delivered event has
/// gone unacked past its deadline. `notify_turn_id` is kept: the next attempt has to be able to
/// check the original turn before sending anything a second time.
pub async fn requeue_inbox(pool: &SqlitePool, id: &str, why: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE supervisor_inbox SET state='pending', notify_error=?, notify_next_at=NULL, updated_at=?
          WHERE id=? AND state='delivered'",
    )
    .bind(why)
    .bind(crate::db::now())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// 放棄補送（`delivered → gave_up`）加上叫醒另一個角色的事件，同一個交易（#310）：喊人的事件寫不進去，事件就不轉 `gave_up`，
/// 留在 `delivered` 讓下一輪再判。回 `false` ＝ 事件已經不在 `delivered`，什麼都沒寫。
pub async fn give_up_inbox_and_notify(
    pool: &SqlitePool,
    id: &str,
    why: &str,
    notice_key: &str,
    assignment_id: Option<&str>,
    bot_id: Option<&str>,
    payload: &Value,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let res = sqlx::query(
        "UPDATE supervisor_inbox SET state='gave_up', notify_error=?, notify_next_at=NULL, updated_at=?
          WHERE id=? AND state='delivered'",
    )
    .bind(why)
    .bind(crate::db::now())
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Ok(false);
    }
    push_inbox_tx(&mut tx, notice_key, "inbox_gave_up", assignment_id, bot_id, None, payload).await?;
    tx.commit().await?;
    Ok(true)
}

/// Delivered events still unacked, oldest delivery first — the ACK-recovery work list.
pub async fn delivered_inbox(pool: &SqlitePool) -> Result<Vec<InboxEvent>> {
    Ok(sqlx::query_as::<_, InboxEvent>(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? AND state='delivered'
          ORDER BY COALESCE(delivered_at, updated_at) ASC",
    )
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?)
}

// 單角色時代的讀寫，現在由 roles.rs 的角色版本取代；留給既有測試釘住同一組守衛。
#[cfg(test)]
/// Pending events whose backoff has not run out yet are skipped; everything else is due.
pub async fn due_inbox(pool: &SqlitePool, now: &str, max_attempts: i64) -> Result<Vec<InboxEvent>> {
    let next_at = crate::db::ts_sql("notify_next_at");
    Ok(sqlx::query_as::<_, InboxEvent>(&format!(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? AND state='pending'
           AND (notify_next_at IS NULL OR {next_at} <= ?)
           AND notify_attempts < ?
          ORDER BY created_at ASC, id ASC"
    ))
    .bind(SUPERVISOR_ID)
    .bind(now)
    .bind(max_attempts)
    .fetch_all(pool)
    .await?)
}

/// Events that have spent their whole retry budget. They stay pending (nothing is swallowed),
/// but the daemon stops pushing them and raises an incident instead of burning quota forever.
///
/// 歸屬用 [`super::roles::OWNER`]，不是 `role`（issue #504）：`role='responder'` 而
/// `claimed_by='patrol'` 的列是巡檢的（`due_for` 就是這樣撈的，而且有 `notify_attempts < max` 的
/// 上限），照 `role` 判會讓它們在用完額度之後對這個探針隱形，而 [`responder_undelivered`] 照 `OWNER`
/// 判也不會收它們——沒有人被叫醒、也沒有任何 incident。外層再 `COALESCE(…, 'patrol')` 是保留原本
/// 「還沒分類的列（兩欄都是 NULL）算巡檢的」那個行為。
pub async fn exhausted_inbox(pool: &SqlitePool, max_attempts: i64) -> Result<Exhausted> {
    let owner = super::roles::OWNER;
    let owned = format!("state='pending' AND notify_attempts >= ? AND COALESCE({owner}, 'patrol')='patrol'");
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM supervisor_inbox WHERE supervisor_id=? AND {owned}"))
        .bind(SUPERVISOR_ID)
        .bind(max_attempts)
        .fetch_one(pool)
        .await?;
    let events = sqlx::query_as::<_, InboxEvent>(&format!(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? AND {owned}
          ORDER BY created_at ASC, rowid ASC LIMIT ?"
    ))
    .bind(SUPERVISOR_ID)
    .bind(max_attempts)
    .bind(EXHAUSTED_SCAN_LIMIT)
    .fetch_all(pool)
    .await?;
    Ok(Exhausted { events, total })
}

/// 一輪最多讀回這麼多則做為 incident 的樣本（issue #504 審核）。巡檢停在登入失效時整個佇列都會
/// 用盡額度，而 detail 只需要「幾則、最舊的是哪一則、試了幾次、最後的錯誤」；把幾百列全讀回來
/// 只是把記憶體與 JSON 撐大，答案一個字都不會變。`total` 不受它影響，所以數字照樣是真的。
pub const EXHAUSTED_SCAN_LIMIT: i64 = 20;

/// [`exhausted_inbox`] 的結果：樣本加上**真正的**總數。
///
/// 分成兩個欄位是因為 incident 是**一張**（`resource='patrol'`），不是一則事件一張：
/// 一則一張等於「一則送不出去的通知變成 N 張 critical incident」，而每一張又各推一則 inbox 事件給
/// 協調者——正是隔壁 `responder_undeliverable` 刻意聚合要避開的那場通知風暴（issue #420）。
pub struct Exhausted {
    /// 最舊的最多 [`EXHAUSTED_SCAN_LIMIT`] 則。
    pub events: Vec<InboxEvent>,
    /// 符合條件的總數，不受上限影響。
    pub total: i64,
}

/// 協調者的事件沒有重試上限（`roles::due_for`），送不出去就一直留在 pending、`notify_attempts` 一直加——
/// 2026-09-23 協調者沒登入時一則核准申請被試了 66 次，沒有任何 incident（issue #420）。這裡撈出試了
/// `min_attempts` 次以上還在 pending 的，給 incident 探針用。事件本身照舊留著、照舊退避重送。
pub async fn responder_undelivered(pool: &SqlitePool, min_attempts: i64) -> Result<Vec<InboxEvent>> {
    let owner = super::roles::OWNER;
    Ok(sqlx::query_as::<_, InboxEvent>(&format!(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? AND state='pending' AND notify_attempts >= ?
           AND {owner}='responder'
          ORDER BY created_at ASC, rowid ASC"
    ))
    .bind(SUPERVISOR_ID)
    .bind(min_attempts)
    .fetch_all(pool)
    .await?)
}

// ---------------------------------------------------------------- approvals & leases

/// Mirrors the row; several fields are read straight out of `to_json`.
#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct Approval {
    pub id: String,
    pub supervisor_id: String,
    pub requester: String,
    pub purpose: String,
    pub scope: String,
    pub target_commit: Option<String>,
    pub status: String,
    pub decided_by: Option<String>,
    pub decided_at: Option<String>,
    pub reason: Option<String>,
    /// 申請者寫的理由（裁示理由在 `reason`，兩個分開存，裁示不會蓋掉申請時寫的那段）。
    #[sqlx(default)]
    pub request_reason: Option<String>,
    pub expires_at: Option<String>,
    /// 呼叫端給的穩定 id（`--request-id`）。重送同一個回原本那一筆，不新增。
    #[sqlx(default)]
    pub client_request_id: Option<String>,
    /// `requester` 沒驗過（issue #436）：建立時沒帶 `X-AM-Bot-Id`＋那顆自己的 hook token，
    /// 所以那串字是自稱的。`1`＝自稱、`0`＝驗過本人。舊列（欄位加上去之前的）都是 `1`。
    #[sqlx(default)]
    pub requester_unverified: i64,
    pub created_at: String,
    pub updated_at: String,
    /// 同一個申請者換 commit 重新申請（`supersedes`）時接下來的等待起點：被取代那筆的 `wait_since`，
    /// 沒有就是它被核准的時間。升級計時（SPEC §18.10）看 `min(wait_since, decided_at)`——
    /// 核准綁 commit，main 一動就換一筆，計時不能跟著歸零（review2 sup 新發現 2）。
    #[sqlx(default)]
    pub wait_since: Option<String>,
}

impl Approval {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "requester": self.requester,
            "purpose": self.purpose,
            "scope": self.scope,
            "target_commit": self.target_commit,
            "status": self.status,
            "decided_by": self.decided_by,
            "decided_at": self.decided_at,
            "reason": self.reason,
            "request_reason": self.request_reason,
            "expires_at": self.expires_at,
            "client_request_id": self.client_request_id,
            "requester_unverified": self.requester_unverified != 0,
            "wait_since": self.wait_since,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }

    /// 實際的到期時間：寫了 `expires_at` 就用它；沒寫的，核准後放過 [`APPROVAL_MAX_AGE_SECS`] 一樣算過期。
    /// 還沒核准（沒有 `decided_at`）又沒寫有效期就是 `None`——那種狀態本來就不能拿來開窗口。
    pub fn effective_expiry(&self) -> Option<String> {
        if let Some(e) = self.expires_at.as_deref() {
            return Some(e.to_string());
        }
        let decided = chrono::DateTime::parse_from_rfc3339(self.decided_at.as_deref()?).ok()?;
        Some((decided + chrono::Duration::seconds(APPROVAL_MAX_AGE_SECS)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    }

    /// 升級計時的起點：接續來的 `wait_since` 與自己被核准的時間，取早的那個。還沒核准就沒有。
    pub fn waiting_since(&self) -> Option<&str> {
        let decided = self.decided_at.as_deref()?;
        Some(self.wait_since.as_deref().filter(|w| crate::db::cmp_ts(w, decided).is_lt()).unwrap_or(decided))
    }

    /// Why this approval cannot be used right now, if it cannot. `None` = usable.
    ///
    /// `now` and `commit` are passed in so this is a pure decision the tests can drive.
    pub fn refusal(&self, now: &str, resource: &str, commit: Option<&str>) -> Option<&'static str> {
        if self.status != "approved" {
            return Some(match self.status.as_str() {
                "pending" => "approval_not_decided",
                "denied" => "approval_denied",
                "revoked" => "approval_revoked",
                "consumed" => "approval_already_used",
                "superseded" => "approval_superseded",
                _ => "approval_not_usable",
            });
        }
        if self.effective_expiry().as_deref().is_some_and(|t| crate::db::cmp_ts(t, now).is_le()) {
            return Some("approval_expired");
        }
        if self.purpose != resource {
            return Some("approval_purpose_mismatch");
        }
        // An approval is for a tree, not for a permission in general. Rebuilding a different
        // commit under yesterday's yes is exactly the drift this table exists to stop.
        match (self.target_commit.as_deref(), commit) {
            (Some(a), Some(b)) if a != b => Some("approval_commit_mismatch"),
            (Some(_), None) => Some("approval_commit_required"),
            _ => None,
        }
    }
}

/// 同一個 request id 又送來，但這次的 purpose／scope／commit 跟原本那筆不一樣：
/// 不回舊的、也不覆寫它——那是兩件不同的事共用了一個 id，要呼叫端自己講清楚。
#[derive(Debug, thiserror::Error)]
#[error("request id `{request_id}` 已經是另一筆申請（{field}：`{old}` → `{new}`），換一個 id 或沿用原本那筆 {approval_id}")]
pub struct ApprovalRequestMismatch {
    pub request_id: String,
    pub approval_id: String,
    pub field: &'static str,
    pub old: String,
    pub new: String,
}

/// 建立（或找回）一筆申請。`created=false` ＝ 這個 request id 已經有一筆，回的是原本那筆。
#[derive(Debug, Clone)]
pub struct ApprovalOutcome {
    pub approval: Approval,
    pub created: bool,
    /// 這次一併取代掉的舊申請（`supersedes` 有效時）。
    pub superseded: Option<String>,
}

/// `supersedes` 指到的不是自己同一種用途的申請：什麼都不寫。
#[derive(Debug, thiserror::Error)]
#[error("approval `{approval_id}` 不能被這筆取代：{reason}")]
pub struct ApprovalSupersedeRefused {
    pub approval_id: String,
    pub reason: &'static str,
}

#[cfg(test)]
pub async fn create_approval(
    pool: &SqlitePool,
    requester: &str,
    purpose: &str,
    scope: &str,
    target_commit: Option<&str>,
    expires_at: Option<&str>,
    request_id: Option<&str>,
) -> Result<ApprovalOutcome> {
    create_approval_superseding(pool, requester, purpose, scope, target_commit, expires_at, request_id, None, None).await
}

/// 同上，外加 `supersedes`：**同一個申請者、同一種用途**換 commit 重新申請時，把舊的那筆
/// （還是 pending／approved、沒過期）在同一個 transaction 裡標成 `superseded`，並把它的等待起點接過來
/// （`wait_since`）。舊的已經不能用（過期、被駁、用掉）就不動它，新的從頭算。
#[allow(clippy::too_many_arguments)]
pub async fn create_approval_superseding(
    pool: &SqlitePool,
    requester: &str,
    purpose: &str,
    scope: &str,
    target_commit: Option<&str>,
    expires_at: Option<&str>,
    request_id: Option<&str>,
    supersedes: Option<&str>,
    request_reason: Option<&str>,
) -> Result<ApprovalOutcome> {
    // 沒有 HTTP 身份可驗（測試與舊呼叫端）：照 issue #436 的預設當成自稱。
    create_approval_inner(pool, requester, purpose, scope, target_commit, expires_at, request_id, supersedes, request_reason, true, false, None).await
}

/// 申請當下由認證中介層證明的 principal；舊 requester 顯示字串不能補猜成這份證據。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalPrincipal {
    Bot { bot_id: String },
    User,
}

pub async fn approval_principal(pool: &SqlitePool, approval_id: &str) -> Result<Option<ApprovalPrincipal>> {
    let body: Option<String> = sqlx::query_scalar("SELECT body FROM supervisor_notes WHERE id=? AND supervisor_id=? AND kind='approval_principal'")
        .bind(format!("approval-principal:{approval_id}")).bind(SUPERVISOR_ID).fetch_optional(pool).await?;
    body.map(|body| serde_json::from_str(&body).map_err(Into::into)).transpose()
}

#[allow(clippy::too_many_arguments)]
pub fn create_approval_principal_notifying<'a>(
    pool: &'a SqlitePool, requester: &'a str, purpose: &'a str, scope: &'a str, target_commit: Option<&'a str>,
    expires_at: Option<&'a str>, request_id: Option<&'a str>, supersedes: Option<&'a str>, request_reason: Option<&'a str>,
    principal: &'a ApprovalPrincipal,
) -> impl std::future::Future<Output = Result<ApprovalOutcome>> + Send + 'a {
    Box::pin(create_approval_inner(pool, requester, purpose, scope, target_commit, expires_at, request_id, supersedes, request_reason, false, true, Some(principal)))
}

/// [`create_approval_superseding`]，新建時同一個交易推 `approval_requested` 叫醒 AGM（#320）：通知寫不進去整筆不成立，
/// 申請者重送再來。重送（`created=false`）不再推。
#[allow(clippy::too_many_arguments)]
pub async fn create_approval_notifying(
    pool: &SqlitePool,
    requester: &str,
    purpose: &str,
    scope: &str,
    target_commit: Option<&str>,
    expires_at: Option<&str>,
    request_id: Option<&str>,
    supersedes: Option<&str>,
    request_reason: Option<&str>,
    // `requester` 驗過了沒（issue #436）：`true`＝只是自稱的。
    requester_unverified: bool,
) -> Result<ApprovalOutcome> {
    create_approval_inner(pool, requester, purpose, scope, target_commit, expires_at, request_id, supersedes, request_reason, requester_unverified, true, None).await
}

#[allow(clippy::too_many_arguments)]
async fn create_approval_inner(
    pool: &SqlitePool,
    requester: &str,
    purpose: &str,
    scope: &str,
    target_commit: Option<&str>,
    expires_at: Option<&str>,
    request_id: Option<&str>,
    supersedes: Option<&str>,
    // 申請者寫的理由（`approval request --reason`）。AGM 裁示時看的就是這段。
    request_reason: Option<&str>,
    // `requester` 只是自稱的（issue #436）：建立時沒有驗過那顆 bot 的身份。
    requester_unverified: bool,
    notify: bool,
    principal: Option<&ApprovalPrincipal>,
) -> Result<ApprovalOutcome> {
    let request_id = request_id.map(str::trim).filter(|s| !s.is_empty());
    let request_reason = request_reason.map(str::trim).filter(|s| !s.is_empty());
    if let Some(rid) = request_id {
        if let Some(existing) = approval_by_request(pool, rid).await? {
            // 已經被裁示的也回它本人：重送的人要看到的是「這件事已經有裁示了」，不是再開一筆。
            return same_request(existing, rid, requester, purpose, scope, target_commit)
                .map(|a| ApprovalOutcome { approval: a, created: false, superseded: None });
        }
    }
    let id = crate::db::ulid();
    let now = crate::db::now();
    // 先找要取代的那一筆再寫：deferred 的話讀完之後別的 writer 一 commit，INSERT 就 517，申請整個被拒（#831）。
    // 寫鎖從頭拿著，兩個同時的申請也排成先後，不會兩邊都讀到「沒有 pending」。
    let mut tx = crate::db::begin_write(pool).await?;
    let mut wait_since: Option<String> = None;
    let mut superseded = None;
    // issue #421：沒帶 `supersedes` 時 daemon 自己找「同一個申請者、同一種用途、還 **pending**」的那一筆。
    // kick 每個整點重試，而記住舊 id 的本機狀態檔掉了（換機器、檔案被清、daemon 換版）時，以前每一輪都開
    // 一筆新的 pending：AGM 收到四則 `approval_requested`、被叫醒四次，而它們講的是同一件事。
    // **只自動取代 pending**：`approved` 是人做過的裁示，不能因為排程又跑了一輪就無聲消失。
    let auto_supersedes = match supersedes.map(str::trim).filter(|s| !s.is_empty()) {
        Some(_) => None,
        None => sqlx::query_scalar::<_, String>(
            // 挑最新用**插入序**（`rowid`）：`created_at` 只到毫秒，平手時比 `id`，而 ULID 同一毫秒內
            // 只有隨機那段在變、不保證遞增——同毫秒兩筆 pending 時「最新那筆」會變成擲硬幣，
            // 取代到比較舊的那筆，最新的留著，AGM 照樣被同一件事叫醒兩次（issue #446，同 #443）。
            "SELECT id FROM supervisor_approvals
              WHERE supervisor_id=? AND requester=? AND purpose=? AND status='pending'
              ORDER BY rowid DESC LIMIT 1",
        )
        .bind(SUPERVISOR_ID)
        .bind(requester)
        .bind(purpose)
        .fetch_optional(&mut *tx)
        .await?,
    };
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("approval_after_supersede_read", requester).await;
    // 自動挑的那一筆不會讓整筆申請失敗：對不上（剛被裁示、剛過期）就只是不取代。
    let auto = auto_supersedes.is_some();
    if let Some(old_id) = supersedes.map(str::trim).filter(|s| !s.is_empty()).or(auto_supersedes.as_deref()) {
        let old = sqlx::query_as::<_, Approval>("SELECT * FROM supervisor_approvals WHERE id=?")
            .bind(old_id)
            .fetch_optional(&mut *tx)
            .await?;
        let refuse = |reason| anyhow::Error::new(ApprovalSupersedeRefused { approval_id: old_id.to_string(), reason });
        let bail = |reason| if auto { Ok(None) } else { Err(refuse(reason)) };
        let old = match old {
            Some(o) if o.requester != requester => bail("not_the_same_requester")?,
            Some(o) if o.purpose != purpose => bail("purpose_mismatch")?,
            Some(o) => Some(o),
            None => bail("approval_not_found")?,
        };
        let expired = |o: &Approval| o.expires_at.as_deref().is_some_and(|t| crate::db::cmp_ts(t, &now).is_le());
        let live = old.as_ref().is_some_and(|o| matches!(o.status.as_str(), "pending" | "approved") && !expired(o));
        // 自動找到的同申請者、同用途 pending 即使已過期也要收掉（#881）：過期的 pending 沒有人會再裁示，
        // 留著會讓 `approval_stalled` 關不掉、重申請又疊第二筆。明確帶 `supersedes` 的路徑不變。
        let stale_pending = auto && old.as_ref().is_some_and(|o| o.status == "pending" && expired(o));
        if let (true, Some(old)) = (live || stale_pending, old) {
            sqlx::query("UPDATE supervisor_approvals SET status='superseded', updated_at=? WHERE id=? AND status=?")
                .bind(&now)
                .bind(&old.id)
                .bind(&old.status)
                .execute(&mut *tx)
                .await?;
            // 還沒核准的舊申請沒有等待可接（等的是 AGM，不是安全窗口）；它自己接過來的照樣往下傳。
            // 過期的舊申請沒有在等：不接續它的等待起點。
            wait_since = if !live {
                None
            } else if old.status == "approved" {
                old.waiting_since().map(str::to_string)
            } else {
                old.wait_since.clone()
            };
            let body = json!({"approval_id": old.id, "from": old.status, "to": "superseded", "actor": requester,
                              "reason": format!("superseded by {id}")});
            sqlx::query("INSERT INTO supervisor_notes (id, supervisor_id, kind, body, version, created_at) VALUES (?,?,?,?,1,?)")
                .bind(crate::db::ulid())
                .bind(SUPERVISOR_ID)
                .bind("approval_decision")
                .bind(body.to_string())
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            // 那則 `approval_requested`（不論送出去了沒）一起收掉：不要叫它醒來、也不要補送一筆已經作廢的申請。
            sqlx::query(
                "UPDATE supervisor_inbox SET state='handled', acked_by='daemon', updated_at=?
                  WHERE supervisor_id=? AND event_key=? AND state!='handled'",
            )
            .bind(&now)
            .bind(SUPERVISOR_ID)
            .bind(format!("approval:{}:requested", old.id))
            .execute(&mut *tx)
            .await?;
            superseded = Some(old.id.clone());
        }
    }
    let insert = sqlx::query(
        "INSERT INTO supervisor_approvals
           (id, supervisor_id, requester, requester_unverified, purpose, scope, target_commit, status, expires_at, client_request_id, request_reason, wait_since, created_at, updated_at)
         VALUES (?,?,?,?,?,?,?, 'pending', ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(SUPERVISOR_ID)
    .bind(requester)
    .bind(i64::from(requester_unverified))
    .bind(purpose)
    .bind(scope)
    .bind(target_commit)
    .bind(expires_at)
    .bind(request_id)
    .bind(request_reason)
    .bind(&wait_since)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await;
    match insert {
        Ok(_) => {
            if let Some(principal) = principal {
                sqlx::query("INSERT INTO supervisor_notes (id,supervisor_id,kind,body,version,created_at) VALUES (?,?,'approval_principal',?,1,?)")
                    .bind(format!("approval-principal:{id}")).bind(SUPERVISOR_ID).bind(serde_json::to_string(principal)?).bind(&now)
                    .execute(&mut *tx).await?;
            }
            if notify {
                let row = sqlx::query_as::<_, Approval>("SELECT * FROM supervisor_approvals WHERE id=?").bind(&id).fetch_one(&mut *tx).await?;
                push_inbox_tx(&mut tx, &format!("approval:{id}:requested"), "approval_requested", None, None, None, &row.to_json()).await?;
            }
            tx.commit().await?;
            Ok(ApprovalOutcome { approval: approval(pool, &id).await?.expect("just inserted"), created: true, superseded })
        }
        // 兩個呼叫同時進來時，先到的那筆已經寫進去了（唯一鍵擋下第二筆）：回先到的那一筆，這次什麼都不取代。
        Err(e) => {
            tx.rollback().await?;
            let Some(rid) = request_id else { return Err(e.into()) };
            match approval_by_request(pool, rid).await? {
                Some(existing) => same_request(existing, rid, requester, purpose, scope, target_commit)
                    .map(|a| ApprovalOutcome { approval: a, created: false, superseded: None }),
                None => Err(e.into()),
            }
        }
    }
}

fn same_request(
    existing: Approval,
    request_id: &str,
    requester: &str,
    purpose: &str,
    scope: &str,
    target_commit: Option<&str>,
) -> Result<Approval> {
    let mismatch = |field: &'static str, old: String, new: String| {
        anyhow::Error::new(ApprovalRequestMismatch {
            request_id: request_id.to_string(),
            approval_id: existing.id.clone(),
            field,
            old,
            new,
        })
    };
    // 申請者也要對得上：兩顆 bot 撞同一個自然 id 時，第二顆不能拿回第一顆的核准（acquire 也驗 owner）。
    if existing.requester != requester {
        return Err(mismatch("requester", existing.requester.clone(), requester.to_string()));
    }
    if existing.purpose != purpose {
        return Err(mismatch("purpose", existing.purpose.clone(), purpose.to_string()));
    }
    if existing.scope != scope {
        return Err(mismatch("scope", existing.scope.clone(), scope.to_string()));
    }
    if existing.target_commit.as_deref().unwrap_or_default() != target_commit.unwrap_or_default() {
        return Err(mismatch(
            "target_commit",
            existing.target_commit.clone().unwrap_or_default(),
            target_commit.unwrap_or_default().to_string(),
        ));
    }
    Ok(existing)
}

pub async fn approval_by_request(pool: &SqlitePool, request_id: &str) -> Result<Option<Approval>> {
    Ok(sqlx::query_as::<_, Approval>(
        "SELECT * FROM supervisor_approvals WHERE supervisor_id=? AND client_request_id=?",
    )
    .bind(SUPERVISOR_ID)
    .bind(request_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn approval(pool: &SqlitePool, id: &str) -> Result<Option<Approval>> {
    Ok(sqlx::query_as::<_, Approval>("SELECT * FROM supervisor_approvals WHERE id=?")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// 已核准、還沒被消耗、也還沒過期的 `rebuild`／`restart` 申請裡，**最早被決定**的那一筆。
///
/// SPEC §18.10「等太久就縮小封鎖面」的計時從它的 `decided_at` 起算：一筆核准放著沒用掉，
/// 代表這段時間一直等不到安全窗口。
/// 申請、裁示都沒給有效期的核准，核准後多久沒用就失效。AGM 通常給明確的 `expires_in_secs`（kick 是 6 小時）；
/// 沒給的不能是「永遠」：放了好幾天的 `approved` 還能開窗口，也讓升級計時一開始就算滿。
pub const APPROVAL_MAX_AGE_SECS: i64 = 24 * 60 * 60;

pub async fn oldest_live_window_approval(pool: &SqlitePool, now: &str) -> Result<Option<Approval>> {
    // `expires_at` 可能是舊版寫的秒格式：正規化成毫秒再比（issue #101）。
    let expires_at = crate::db::ts_sql("expires_at");
    let decided = crate::db::ts_sql("decided_at");
    Ok(sqlx::query_as::<_, Approval>(&format!(
        "SELECT * FROM supervisor_approvals
          WHERE supervisor_id=?3 AND status='approved' AND purpose IN ('rebuild','restart')
            AND decided_at IS NOT NULL
            AND (({expires_at} > ?1 AND expires_at IS NOT NULL) OR (expires_at IS NULL AND {decided} > ?2))
          ORDER BY MIN(COALESCE(wait_since, decided_at), decided_at) LIMIT 1"
    ))
    .bind(now)
    .bind(crate::db::iso_in(-APPROVAL_MAX_AGE_SECS))
    .bind(SUPERVISOR_ID)
    .fetch_optional(pool)
    .await?)
}

pub async fn approvals(pool: &SqlitePool, limit: i64) -> Result<Vec<Approval>> {
    Ok(sqlx::query_as::<_, Approval>(
        "SELECT * FROM supervisor_approvals WHERE supervisor_id=? ORDER BY created_at DESC LIMIT ?",
    )
    .bind(SUPERVISOR_ID)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Approve, deny, revoke or consume. Only a `pending` approval can be approved or denied; a
/// revoke applies to one that was already approved, and takes effect for every later acquire.
#[cfg(test)]
pub async fn decide_approval(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    actor: &str,
    reason: Option<&str>,
    expires_at: Option<&str>,
) -> Result<Option<Approval>> {
    let now = crate::db::now();
    sqlx::query(
        "UPDATE supervisor_approvals
            SET status=?, decided_by=?, decided_at=?, reason=COALESCE(?, reason),
                expires_at=COALESCE(?, expires_at), updated_at=?
          WHERE id=?",
    )
    .bind(status)
    .bind(actor)
    .bind(&now)
    .bind(reason)
    .bind(expires_at)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    approval(pool, id).await
}

/// 決定一筆核准並把它寫進歷程——**同一個 transaction**。
///
/// `Ok(None)` = 狀態已經不是 `from_status`（別人先決定了），什麼都沒寫。
///
/// `consumed`／`superseded` 不是裁示，**不覆寫** `decided_at`／`decided_by`：誰在什麼時候核准的要留著，
/// 升級計時也從那個時間算；誰、什麼時候用掉的記在 note 裡（review2 sup #6）。
///
/// 兩件事綁在一起的理由：歷程是 append-only 的承諾。先 UPDATE 再另外 INSERT 的話，note 寫失敗
/// 就留下「有決定、沒紀錄」的半套狀態，而呼叫端重試會撞上 `idempotent`（同一個決定）直接回成功
/// ——那筆 audit 永遠補不回來。
#[allow(clippy::too_many_arguments)]
pub async fn decide_approval_from(
    pool: &SqlitePool,
    id: &str,
    from_status: &str,
    status: &str,
    actor: &str,
    reason: Option<&str>,
    expires_at: Option<&str>,
) -> Result<Option<(Approval, String)>> {
    let now = crate::db::now();
    let note_id = crate::db::ulid();
    let mut tx = pool.begin().await?;
    let keeps_decision = matches!(status, "consumed" | "superseded");
    let res = sqlx::query(
        "UPDATE supervisor_approvals
            SET status=?, decided_by=CASE WHEN ? THEN decided_by ELSE ? END, decided_at=CASE WHEN ? THEN decided_at ELSE ? END,
                reason=COALESCE(?, reason), expires_at=COALESCE(?, expires_at), updated_at=?
          WHERE id=? AND status=?",
    )
    .bind(status)
    .bind(keeps_decision)
    .bind(actor)
    .bind(keeps_decision)
    .bind(&now)
    .bind(reason)
    .bind(expires_at)
    .bind(&now)
    .bind(id)
    .bind(from_status)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(None);
    }
    let body = json!({"approval_id": id, "from": from_status, "to": status, "actor": actor, "reason": reason});
    sqlx::query("INSERT INTO supervisor_notes (id, supervisor_id, kind, body, version, created_at) VALUES (?,?,?,?,1,?)")
        .bind(&note_id)
        .bind(SUPERVISOR_ID)
        .bind("approval_decision")
        .bind(body.to_string())
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    // 還 pending 就被裁示：叫 AGM 裁示的那則 `approval_requested` 跟著收掉（跟被取代那條路同一個道理）。
    // 不收的話，它會繼續被補送、叫醒 AGM 去裁示一筆已經有結果的申請，協調者不可用時 failover 還會把它改派給巡檢。
    if from_status == "pending" {
        sqlx::query(
            "UPDATE supervisor_inbox SET state='handled', acked_by='daemon', updated_at=?
              WHERE supervisor_id=? AND event_key=? AND state!='handled'",
        )
        .bind(&now)
        .bind(SUPERVISOR_ID)
        .bind(format!("approval:{id}:requested"))
        .execute(&mut *tx)
        .await?;
    }
    let row = sqlx::query_as::<_, Approval>("SELECT * FROM supervisor_approvals WHERE id=?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some((row, note_id)))
}


pub use crate::supervisor_inbox::add_note;

/// 每筆核准的決定歷程，最舊在前。一次查完再分組：核准筆數不多，但一筆一次查詢會變 N+1。
pub async fn approval_decisions(pool: &SqlitePool) -> Result<std::collections::HashMap<String, Vec<Value>>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        // 同一毫秒寫進去的兩筆（核准後馬上撤銷）靠 `rowid` 排：`id` 是 ULID，同一毫秒內的亂數段不保證遞增，
        // 以前用 `id ASC` 當第二鍵，歷程偶爾會倒過來（review 2026-09-16）。
        "SELECT body, created_at FROM supervisor_notes WHERE supervisor_id=? AND kind='approval_decision' ORDER BY created_at ASC, rowid ASC",
    )
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?;
    Ok(group_decisions(rows))
}

/// 只取這幾筆核准的決定歷程（`GET /approvals` 用）。全表讀出再丟掉的成本隨換版歷史線性成長，所以這裡只讀要的（#972）。
/// `json_valid` 先擋掉不是 JSON 的列，不讓 `json_extract` 把整句查詢報錯；那種列照舊略過，跟 [`approval_decisions`] 一樣。
pub async fn approval_decisions_for(pool: &SqlitePool, ids: &[String]) -> Result<std::collections::HashMap<String, Vec<Value>>> {
    if ids.is_empty() {
        return Ok(Default::default());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT body, created_at FROM supervisor_notes WHERE supervisor_id=? AND kind='approval_decision'
           AND CASE WHEN json_valid(body) THEN json_extract(body, '$.approval_id') END IN (SELECT value FROM json_each(?))
         ORDER BY created_at ASC, rowid ASC",
    )
    .bind(SUPERVISOR_ID)
    .bind(serde_json::to_string(ids)?)
    .fetch_all(pool)
    .await?;
    Ok(group_decisions(rows))
}

fn group_decisions(rows: Vec<(String, String)>) -> std::collections::HashMap<String, Vec<Value>> {
    let mut out: std::collections::HashMap<String, Vec<Value>> = Default::default();
    for (body, at) in rows {
        let Ok(mut v) = serde_json::from_str::<Value>(&body) else { continue };
        let Some(id) = v.get("approval_id").and_then(Value::as_str).map(str::to_string) else { continue };
        v["at"] = json!(at);
        out.entry(id).or_default().push(v);
    }
    out
}

/// Mirrors the row.
#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct Lease {
    pub resource: String,
    pub owner: Option<String>,
    pub approval_id: Option<String>,
    pub fence: i64,
    pub target_commit: Option<String>,
    pub acquired_at: Option<String>,
    pub expires_at: Option<String>,
    pub released_at: Option<String>,
    /// **不要**放進 `to_json`：那個形狀會出現在 `lease status`、`/api/supervisor` 與事件裡。
    #[sqlx(default)]
    pub lease_token: Option<String>,
    pub detail_json: String,
}

impl Lease {
    pub fn to_json(&self) -> Value {
        json!({
            "resource": self.resource,
            "owner": self.owner,
            "approval_id": self.approval_id,
            "fence": self.fence,
            "target_commit": self.target_commit,
            "acquired_at": self.acquired_at,
            "expires_at": self.expires_at,
            "released_at": self.released_at,
            "held": self.held_at(&crate::db::now()),
        })
    }

    pub fn held_at(&self, now: &str) -> bool {
        self.released_at.is_none() && self.expires_at.as_deref().is_some_and(|t| crate::db::cmp_ts(t, now).is_gt())
    }
}

pub async fn lease(pool: &SqlitePool, resource: &str) -> Result<Option<Lease>> {
    Ok(sqlx::query_as::<_, Lease>("SELECT * FROM supervisor_leases WHERE resource=?")
        .bind(resource)
        .fetch_optional(pool)
        .await?)
}

pub async fn leases(pool: &SqlitePool) -> Result<Vec<Lease>> {
    Ok(sqlx::query_as::<_, Lease>("SELECT * FROM supervisor_leases ORDER BY resource").fetch_all(pool).await?)
}

/// **送達臨界區的唯一定義**（SPEC §18.10）：這一筆 turn 此刻是不是「daemon 正要把字送進 pane，
/// 中途被打斷會掉東西」。兩種：
/// - `queued` 而且這顆 bot 還有活著、不在 `blocked`、也不在 `working` 的 run（flush 隨時會領它去打字）。
///   `working` 不算（issue #1677）：flush 在 working 時一個字都不會打（`queue::flush_queued_locked`），
///   排隊跨 daemon 重啟保得住、回合結束的邊在新 daemon 照樣收得到；算進來的話長回合加一則排隊訊息就讓部署永遠拿不到窗口；
/// - `in_flight` 而 `delivery` 還是 `pending`（正在打字／送出）。
///
/// flush 讀完 run 之後 bot 才轉 working、窗口恰好此時拿走的那一瞬，由 flush **認領之後**再看一次窗口收尾，
/// 不靠這個條件。
///
/// 安全檢查（`maintenance::safety`）、`acquire_lease` 的條件式寫入與入場閘門都讀這一份，
/// 才不會養出兩套會漂移的規則（issue #86）。用的別名固定是 `t`（turns）與 `c`（conversations）。
// claude-api（#1944）沒有 pane、回合在 daemon 內跑：只有 run 真的在 running、而且回合還在合理時間內（15 分，> turn_timeout）
// 才算送達臨界區。2026-11-01 一則卡在 stopping run 裡的 in_flight 回合讓換版窗口等了兩個多小時（死結：修正要換版才上線）。
pub const DELIVERY_CRITICAL_PREDICATE: &str = "(t.status='queued' AND EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = c.bot_id
                       AND r.state IN ('starting','running','stopping') AND r.agent_status NOT IN ('blocked','working')))
      OR (t.status='in_flight' AND t.delivery='pending' AND NOT EXISTS (SELECT 1 FROM bots b WHERE b.id=c.bot_id AND b.kind='claude-api'))
      OR (t.status='in_flight' AND EXISTS (SELECT 1 FROM bots b WHERE b.id=c.bot_id AND b.kind='claude-api')
          AND EXISTS (SELECT 1 FROM runs r WHERE r.id=t.run_id AND r.state='running')
          AND t.created_at > strftime('%Y-%m-%dT%H:%M:%fZ','now','-15 minutes'))";

/// 上面那份條件套在「任何一顆 bot」上，給 `NOT EXISTS (…)` 用。沒有 bind 參數。
pub fn delivery_critical_anywhere_sql() -> String {
    format!(
        "SELECT 1 FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE {DELIVERY_CRITICAL_PREDICATE} LIMIT 1"
    )
}

/// 同一份條件，但**放過一顆 bot**（bind 一個 bot id）。
///
/// 給 `acquire_lease` 的 `exclude_bot` 用：申請窗口的那顆 bot，自己的回合不該把自己擋在門外
/// （2026-09-18：AM-m3 在自己的回合裡 30 次都拿不到 restart，因為 quiet_delivery 不吃排除）。
/// 只放過**一顆**，而且呼叫端已經驗過它就是核准的 requester；其他 bot 的臨界區照擋。
pub fn delivery_critical_except_sql() -> String {
    format!(
        "SELECT 1 FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE (c.bot_id <> ? OR EXISTS (SELECT 1 FROM bots b WHERE b.id=c.bot_id AND b.kind='claude-api')) AND ({DELIVERY_CRITICAL_PREDICATE}) LIMIT 1"
    )
}

/// 上面那份條件限定一顆 bot，回傳擋住的那一筆 turn id。
pub fn delivery_critical_for_bot_sql() -> String {
    format!(
        "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE c.bot_id = ? AND ({DELIVERY_CRITICAL_PREDICATE}) LIMIT 1"
    )
}

/// 這一刻有沒有**任何**一顆 bot 在送達臨界區。只給「拿不到租約時要說清楚是哪一種輸法」用；
/// 真正把關的是 [`acquire_lease`] 那一句 UPDATE 自己帶的條件。
pub async fn delivery_critical_anywhere(pool: &SqlitePool) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(&delivery_critical_anywhere_sql()).fetch_optional(pool).await?.is_some())
}

/// Take the lease on `resource`, if nobody holds it.
///
/// One conditional UPDATE does the whole thing, so two callers racing for the same window
/// cannot both win: the loser's `rows_affected` is 0. `fence` is bumped on every successful
/// acquire and every operation afterwards has to present the current one — that is what stops a
/// holder that stalled past its expiry from carrying on as if it still had the window.
///
/// `quiet_delivery` 再加一個條件：**寫下去的那一刻**沒有任何 bot 在送達臨界區
/// （[`DELIVERY_CRITICAL_PREDICATE`]）。安全檢查是在這之前讀的，中間那一瞬有 prompt 把 turn
/// commit 進來的話，光靠那個讀不出來——窗口就會在「有人正在打字」的情況下被拿走（issue #86 的
/// TOCTOU）。兩邊都是單句寫入，SQLite 自己會排序，所以先 commit 的那個贏：
/// prompt 先 → 這裡 0 rows；這裡先 → prompt 在第一個字之前看到租約，撤回自己那一筆。
pub async fn acquire_lease(
    pool: &SqlitePool,
    resource: &str,
    owner: &str,
    approval_id: Option<&str>,
    target_commit: Option<&str>,
    expires_at: &str,
    quiet_delivery: bool,
    exclude_bot: Option<&str>,
    detail: &Value,
) -> Result<Option<Lease>> {
    let now = crate::db::now();
    sqlx::query("INSERT OR IGNORE INTO supervisor_leases (resource, fence, released_at) VALUES (?, 0, ?)")
        .bind(resource)
        .bind(&now)
        .execute(pool)
        .await?;
    // 前一個持有者**過期**而不是 release（腳本掛了、child 被殺）時，沒有人走過 release，
    // 它背後的核准因此還是 `approved`——同一張「可以」可以在有效期內開好幾個窗口，
    // 而決定歷程上一筆紀錄都沒有（review 2026-09-16）。接手前先把它消耗掉。
    // 租約的 `expires_at` 可能是舊版寫的秒格式：正規化成毫秒再比（issue #101）。
    let expires_sql = crate::db::ts_sql("expires_at");
    let stale_approval = sqlx::query_scalar::<_, Option<String>>(&format!(
        "SELECT approval_id FROM supervisor_leases
          WHERE resource=? AND released_at IS NULL AND expires_at IS NOT NULL AND {expires_sql} <= ?"
    ))
    .bind(resource)
    .bind(&now)
    .fetch_optional(pool)
    .await?
    .flatten();
    let token = crate::supervisor::new_token();
    // 排除那顆 bot 時走帶 bind 的變體；`exclude_bot` 是呼叫端驗過的 requester 自己那顆
    // （`maintenance::acquire`），不是隨便一個 id。
    let quiet = match (quiet_delivery, exclude_bot) {
        (false, _) => String::new(),
        (true, Some(_)) => format!(" AND NOT EXISTS ({})", delivery_critical_except_sql()),
        (true, None) => format!(" AND NOT EXISTS ({})", delivery_critical_anywhere_sql()),
    };
    let sql = format!(
        "UPDATE supervisor_leases
            SET owner=?, approval_id=?, target_commit=?, fence=fence+1, acquired_at=?, expires_at=?,
                released_at=NULL, lease_token=?, detail_json=?
          WHERE resource=? AND (released_at IS NOT NULL OR expires_at IS NULL OR {expires_sql} <= ?){quiet}"
    );
    let mut q = sqlx::query(&sql)
    .bind(owner)
    .bind(approval_id)
    .bind(target_commit)
    .bind(&now)
    .bind(expires_at)
    .bind(&token)
    .bind(detail.to_string())
    .bind(resource)
    .bind(&now);
    if quiet_delivery {
        if let Some(bot) = exclude_bot {
            q = q.bind(bot);
        }
    }
    let taken = q.execute(pool).await?.rows_affected() > 0;
    if !taken {
        return Ok(None);
    }
    if let Some(stale) = stale_approval.filter(|a| Some(a.as_str()) != approval_id) {
        // `decide_approval_from` 只在它還是 `approved` 時才動，而且**會寫進 supervisor_notes**
        // （`decide_approval` 是無條件 UPDATE，還會把 `decided_at` 覆寫成消耗時間，
        // 升級判定的計時就是從那個欄位算的）。
        let _ = decide_approval_from(pool, &stale, "approved", "consumed", "daemon", Some("lease expired"), None).await;
    }
    lease(pool, resource).await
}

/// Extend a lease you still hold. A stale fence, a different owner or an expired lease all fail
/// — the holder has to find out it lost the window rather than assume it still has it.
/// 出示的憑證對不對。**唯二**的放行：舊資料（升級前建的租約沒有 token，否則升級當下會卡住一個
/// 窗口沒人能還）與呼叫端明說的強制釋放。兩者都由呼叫端負責留稽核。
#[derive(Debug, Clone, Copy)]
pub enum LeaseProof<'a> {
    Token(&'a str),
    /// 呼叫端什麼都沒帶。升級前建立的租約（`lease_token IS NULL`）才放行——否則升級當下
    /// 握在手上的那個窗口永遠沒人還得了。
    Absent,
    /// daemon 啟動時收上一輪殘留的 restart 租約，或 AGM 的 `--force`。
    Forced,
}

impl LeaseProof<'_> {
    /// `stored` 是 DB 裡那一列的 token。
    pub fn allows(&self, stored: Option<&str>) -> bool {
        match (self, stored) {
            (LeaseProof::Forced, _) => true,
            // 升級前的舊租約：沒有 token 可以對，放行（一次性的過渡，見 SPEC §18.10）。
            (_, None) => true,
            (LeaseProof::Absent, Some(_)) => false,
            (LeaseProof::Token(given), Some(want)) => {
                // 長度先比，再逐位元組比：不要因為比對提早結束而洩漏前綴。
                given.len() == want.len() && given.bytes().zip(want.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
            }
        }
    }

    /// 強制釋放不比對持有者：持有者可能已經不在了，那正是要強制的理由。
    pub fn is_forced(&self) -> bool {
        matches!(self, LeaseProof::Forced)
    }
}

/// 這個 resource 現在那一列的 token（沒有列或沒有 token 就是 `None`）。
pub async fn lease_token(pool: &SqlitePool, resource: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>("SELECT lease_token FROM supervisor_leases WHERE resource=?")
        .bind(resource)
        .fetch_optional(pool)
        .await?
        .flatten())
}

pub async fn renew_lease(pool: &SqlitePool, resource: &str, owner: &str, fence: i64, expires_at: &str) -> Result<bool> {
    let now = crate::db::now();
    let held_until = crate::db::ts_sql("expires_at");
    Ok(sqlx::query(&format!(
        "UPDATE supervisor_leases SET expires_at=?
          WHERE resource=? AND owner=? AND fence=? AND released_at IS NULL AND {held_until} > ?"
    ))
    .bind(expires_at)
    .bind(resource)
    .bind(owner)
    .bind(fence)
    .bind(&now)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// Give the window back. Deliberately *not* gated on `expires_at`: a holder whose lease has
/// just expired should still be able to say it is finished.
pub async fn release_lease(pool: &SqlitePool, resource: &str, owner: &str, fence: i64) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE supervisor_leases SET released_at=? WHERE resource=? AND owner=? AND fence=? AND released_at IS NULL",
    )
    .bind(crate::db::now())
    .bind(resource)
    .bind(owner)
    .bind(fence)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// 強制釋放：不比對 owner／fence（持有者可能已經不在了，那正是要強制的理由）。
/// 呼叫端負責留稽核紀錄。
pub async fn force_release_lease(pool: &SqlitePool, resource: &str, fence: i64) -> Result<bool> {
    Ok(sqlx::query("UPDATE supervisor_leases SET released_at=? WHERE resource=? AND fence=? AND released_at IS NULL")
        .bind(crate::db::now())
        .bind(resource)
        .bind(fence)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}

// ---------------------------------------------------------------- incidents

/// Mirrors the row: `FromRow` needs every column, and the JSON readers take several of them
/// straight out of `to_json`.
#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct Incident {
    pub id: String,
    pub supervisor_id: String,
    pub kind: String,
    pub resource: String,
    pub severity: String,
    pub status: String,
    pub detail_json: String,
    pub occurrences: i64,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub resolved_at: Option<String>,
}

impl Incident {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "resource": self.resource,
            "severity": self.severity,
            "status": self.status,
            "detail": serde_json::from_str::<Value>(&self.detail_json).unwrap_or_else(|_| json!({})),
            "occurrences": self.occurrences,
            "first_seen_at": self.first_seen_at,
            "last_seen_at": self.last_seen_at,
            "resolved_at": self.resolved_at,
        })
    }
}

/// Open an incident for `(kind, resource)`, or refresh the one already open.
///
/// Returns `(incident, opened)`. `opened == false` on every tick after the first, which is what
/// keeps a five-hour outage at one notification instead of six hundred.
#[cfg(test)]
pub async fn open_incident(
    pool: &SqlitePool,
    kind: &str,
    resource: &str,
    severity: &str,
    detail: &Value,
) -> Result<(Incident, bool)> {
    open_incident_notifying(pool, kind, resource, severity, detail, false).await
}

/// [`open_incident`]；`notify` 時第一次開起來的同一個交易裡推 `incident_opened`（#319）：通知寫不進去，
/// incident 就不開，下一輪 detector 重來——`opened` 只回 `true` 一次，先開後推的話寫不進去就永遠沒人被告知。
pub async fn open_incident_notifying(
    pool: &SqlitePool,
    kind: &str,
    resource: &str,
    severity: &str,
    detail: &Value,
    notify: bool,
) -> Result<(Incident, bool)> {
    let now = crate::db::now();
    let mut tx = pool.begin().await?;
    let opened = sqlx::query(
        "INSERT OR IGNORE INTO supervisor_incidents
           (id, supervisor_id, kind, resource, severity, status, detail_json, occurrences, first_seen_at, last_seen_at)
         VALUES (?,?,?,?,?, 'open', ?, 1, ?, ?)",
    )
    .bind(crate::db::ulid())
    .bind(SUPERVISOR_ID)
    .bind(kind)
    .bind(resource)
    .bind(severity)
    .bind(detail.to_string())
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if !opened {
        sqlx::query(
            "UPDATE supervisor_incidents SET last_seen_at=?, occurrences=occurrences+1, severity=?, detail_json=?
              WHERE supervisor_id=? AND kind=? AND resource=? AND status='open'",
        )
        .bind(&now)
        .bind(severity)
        .bind(detail.to_string())
        .bind(SUPERVISOR_ID)
        .bind(kind)
        .bind(resource)
        .execute(&mut *tx)
        .await?;
    }
    let row = sqlx::query_as::<_, Incident>(
        "SELECT * FROM supervisor_incidents WHERE supervisor_id=? AND kind=? AND resource=? AND status='open'",
    )
    .bind(SUPERVISOR_ID)
    .bind(kind)
    .bind(resource)
    .fetch_one(&mut *tx)
    .await?;
    if opened && notify {
        push_inbox_tx(&mut tx, &format!("incident:{}:opened", row.id), "incident_opened", None, None, None, &serde_json::json!({"incident": row.to_json()})).await?;
    }
    tx.commit().await?;
    Ok((row, opened))
}

/// The condition cleared. `Ok(None)` = there was nothing open, which is the ordinary case.
#[cfg(test)]
pub async fn resolve_incident(pool: &SqlitePool, kind: &str, resource: &str) -> Result<Option<Incident>> {
    resolve_incident_notifying(pool, kind, resource, false).await
}

/// [`resolve_incident`]；`notify` 時同一個交易推 `incident_resolved`（#319），理由同 [`open_incident_notifying`]。
pub async fn resolve_incident_notifying(pool: &SqlitePool, kind: &str, resource: &str, notify: bool) -> Result<Option<Incident>> {
    // 先讀開著的那一筆再改：deferred 的話讀完之後別的 writer 一 commit 就 517，收不掉、這一輪的通知也沒了（#831）。
    let mut tx = crate::db::begin_write(pool).await?;
    let row = sqlx::query_as::<_, Incident>(
        "SELECT * FROM supervisor_incidents WHERE supervisor_id=? AND kind=? AND resource=? AND status='open'",
    )
    .bind(SUPERVISOR_ID)
    .bind(kind)
    .bind(resource)
    .fetch_optional(&mut *tx)
    .await?;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("incident_resolve_after_read", resource).await;
    let Some(row) = row else { return Ok(None) };
    let now = crate::db::now();
    sqlx::query("UPDATE supervisor_incidents SET status='resolved', resolved_at=?, last_seen_at=? WHERE id=?")
        .bind(&now)
        .bind(&now)
        .bind(&row.id)
        .execute(&mut *tx)
        .await?;
    let out = sqlx::query_as::<_, Incident>("SELECT * FROM supervisor_incidents WHERE id=?").bind(&row.id).fetch_optional(&mut *tx).await?;
    if notify {
        if let Some(inc) = &out {
            push_inbox_tx(&mut tx, &format!("incident:{}:resolved", inc.id), "incident_resolved", None, None, None, &serde_json::json!({"incident": inc.to_json()})).await?;
        }
    }
    tx.commit().await?;
    Ok(out)
}

pub async fn open_incidents(pool: &SqlitePool) -> Result<Vec<Incident>> {
    Ok(sqlx::query_as::<_, Incident>(
        "SELECT * FROM supervisor_incidents WHERE supervisor_id=? AND status='open' ORDER BY first_seen_at ASC",
    )
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?)
}

pub async fn incidents(pool: &SqlitePool, limit: i64) -> Result<Vec<Incident>> {
    Ok(sqlx::query_as::<_, Incident>(
        "SELECT * FROM supervisor_incidents WHERE supervisor_id=? ORDER BY first_seen_at DESC LIMIT ?",
    )
    .bind(SUPERVISOR_ID)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

// 單角色時代的讀寫，現在由 roles.rs 的角色版本取代；留給既有測試釘住同一組守衛。
#[cfg(test)]
pub async fn ack_inbox(pool: &SqlitePool, id: &str) -> Result<bool> {
    let res = sqlx::query("UPDATE supervisor_inbox SET state='handled', updated_at=? WHERE supervisor_id=? AND id=?")
        .bind(crate::db::now())
        .bind(SUPERVISOR_ID)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
#[path = "store_tests/tests_crate_only.rs"]
mod tests;

#[cfg(all(test, not(feature = "daemon-test-harness")))]
#[path = "store_tests/persona_migration_tests.rs"]
mod persona_migration_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "store_tests/tests.rs"]
mod tests;
