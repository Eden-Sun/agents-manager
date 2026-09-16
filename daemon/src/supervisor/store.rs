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
-- One row per *resource* that is currently wrong, not per tick that noticed it. The partial
-- unique index is the dedupe key: a restart cannot open a second incident for the same thing,
-- and a fault that comes back after a resolve gets a new row (a new occurrence), not a silent
-- re-use of the old one.
CREATE TABLE IF NOT EXISTS supervisor_incidents (
  id TEXT PRIMARY KEY,
  supervisor_id TEXT NOT NULL,
  -- host_disconnected | bot_stopped | assignment_stalled | notify_exhausted | remote_entry
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
  reason TEXT,
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
        // What the remote-entry status is based on. Without these, `remote_status` was a word
        // with no provenance and no expiry — and `requested` was being read as "connected".
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
        // 群組任務（mission/，docs/goals/agm-missions.md）：這件交辦屬於哪個任務、擔任哪個角色
        // （executor | reviewer | verifier）。follow-up 會沿用父交辦的這兩欄。
        ("mission_id", "ALTER TABLE supervisor_assignments ADD COLUMN mission_id TEXT"),
        ("mission_role", "ALTER TABLE supervisor_assignments ADD COLUMN mission_role TEXT"),
        // 回合結束時 run 上記的 `turn_error`（撞限、API 錯誤、斷線…）。`turn_status` 只有
        // completed/failed 這種粗分類，換手要看的是「為什麼」。
        ("turn_error", "ALTER TABLE supervisor_assignments ADD COLUMN turn_error TEXT"),
        // 這一輪「一直 409 送不進去」從什麼時候開始（第一次撞 409 寫下）。409 保險絲從這裡計時，
        // 不是 `created_at`：在 quota_blocked 或 restart 窗口 hold 裡合法等待的時間不算「送不進去」。
        ("conflict_since", "ALTER TABLE supervisor_assignments ADD COLUMN conflict_since TEXT"),
    ] {
        if !has_column(pool, "supervisor_assignments", col).await? {
            sqlx::query(ddl).execute(pool).await?;
            if col == "legacy_closed" {
                sqlx::query(
                    "UPDATE supervisor_assignments SET legacy_closed=1
                       WHERE status IN ('completed','failed','cancelled')",
                )
                .execute(pool)
                .await?;
            }
            if col == "turn_status" {
                // Rows closed by the old `on_turn_done` really did see a completed turn; that
                // fact is recoverable, the acceptance is not.
                sqlx::query("UPDATE supervisor_assignments SET turn_status='completed' WHERE status='completed'")
                    .execute(pool)
                    .await?;
            }
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
    pub remote_status: String,
    pub remote_url: Option<String>,
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
    pub remote_source: Option<String>,
    pub remote_observed_at: Option<String>,
    pub remote_session_id: Option<String>,
    pub remote_actor: Option<String>,
    pub remote_evidence: Option<String>,
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
    pub turn_status: Option<String>,
    pub evidence_complete: Option<i64>,
    pub reviewed_at: Option<String>,
    pub reviewed_by: Option<String>,
    pub review_decision: Option<String>,
    pub review_reason: Option<String>,
    pub followup_assignment_id: Option<String>,
    pub follow_up_of: Option<String>,
    pub legacy_closed: i64,
    pub ownership_json: Option<String>,
    /// 0 = 通知，不等回覆也不驗收（見 SCHEMA）。
    pub expects_review: i64,
    /// `quota_blocked` 時：額度預計什麼時候回來。
    pub resume_at: Option<String>,
    /// 因額度自動重送過幾次。
    pub quota_retries: i64,
    /// 群組任務：所屬任務與角色（見 migrate 的欄位註解）。
    pub mission_id: Option<String>,
    pub mission_role: Option<String>,
    /// 回合結束時 run 上的 `turn_error`。
    pub turn_error: Option<String>,
    /// 回報給哪個 AGM 角色驗收（`patrol` | `responder`；NULL = 協調者）。見 roles.rs。
    #[sqlx(default)]
    pub review_role: Option<String>,
    /// 這一輪第一次撞 409 的時間（見 migrate 的欄位註解）。
    #[sqlx(default)]
    pub conflict_since: Option<String>,
}

/// Lifecycle states an assignment can still move out of on its own.
pub const EXECUTING_STATES: [&str; 3] = ["queued", "delivered", "unknown"];
/// Everything AGM still owes attention to: in flight, waiting to be accepted, or blocked.
/// `quota_blocked` 也在裡面——工作還沒做完，只是在等額度回來；controller 會自己重送。
pub const OPEN_STATES: [&str; 6] =
    ["queued", "delivered", "unknown", "awaiting_review", "blocked", "quota_blocked"];

/// `'a','b',…` 給 SQL 的 `IN (…)` 用。以前四個查詢各自把清單硬寫進字串，三種答案：
/// `quota_blocked` 因此從 ownership 衝突與未結案計數裡消失——AGM 查過衝突、回報「沒有人握著這塊」，
/// 然後把同一個模組派給第二顆 bot（review 2026-09-16）。清單只准有一份。
fn sql_list(states: &[&str]) -> String {
    states.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",")
}

/// 「卡住了沒人管」只看這些：`quota_blocked` 在等一個已知的時間點（SPEC §18.9 明講不算卡住），
/// `blocked` 在等人回答——兩個都不是沒人管，所以刻意不進這張表。其餘的未結案都算。
pub const STALLED_STATES: [&str; 4] = ["queued", "delivered", "unknown", "awaiting_review"];

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
            // When the next retry is due. Without it "attempts: 37" is a number with no story:
            // you cannot tell a job that is retrying on schedule from one that is wedged.
            "next_attempt_at": self.next_attempt_at,
            "request_id": self.request_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "completed_at": self.completed_at,
            // The raw transport facts, kept next to the lifecycle state rather than folded
            // into it: what the turn itself ended as, and whether we saw all of the reply.
            "turn_status": self.turn_status,
            "evidence_complete": self.evidence_complete.map(|v| v != 0),
            "open": self.is_open(),
            "awaiting_review": self.status == "awaiting_review",
            "review": {
                "decision": self.review_decision,
                "by": self.reviewed_by,
                "at": self.reviewed_at,
                "reason": self.review_reason,
                "followup_assignment_id": self.followup_assignment_id,
            },
            "follow_up_of": self.follow_up_of,
            // True for rows closed before acceptance existed: closed, but never accepted by
            // anyone. Not a claim that the work was verified.
            "legacy_closed": self.legacy_closed != 0,
            "ownership": self.ownership(),
            "review_role": self.review_role.as_deref().unwrap_or("responder"),
            // `notice` = AGM 只是把話說給 bot 聽：送到、回合結束就結案，沒有驗收這一段。
            "kind": if self.is_notice() { "notice" } else { "task" },
            "expects_review": !self.is_notice(),
            // 只有 `quota_blocked` 用得到：額度什麼時候回來、已經自動重送幾次。
            "resume_at": self.resume_at,
            "quota_retries": self.quota_retries,
            // 群組任務（沒有就是 null）。
            "mission_id": self.mission_id,
            "role": self.mission_role,
            "turn_error": self.turn_error,
            // 一直 409 送不進去的這一輪從什麼時候開始；保險絲從這裡計時（SPEC §18.8）。
            "conflict_since": self.conflict_since,
        })
    }

    /// 通知型（不等回覆、不驗收）。
    pub fn is_notice(&self) -> bool {
        self.expects_review == 0
    }

    /// 這一次要送出去用的 `client_request_id`。
    ///
    /// 第一次就是 assignment 自己的那個；被額度擋下重送時加上 `#r<n>`——`lifecycle::prompt`
    /// 的冪等是「同一個 crid 回同一個 turn」，不換 id 的話重送會直接拿回上一個**撞到上限的**
    /// turn，等於什麼都沒送。加了序號之後：同一次重送重跑幾遍仍然只有一個 turn（冪等還在），
    /// 但跟上一次分得開。
    pub fn dispatch_crid(&self) -> String {
        if self.quota_retries <= 0 {
            self.client_request_id.clone()
        } else {
            format!("{}#r{}", self.client_request_id, self.quota_retries)
        }
    }

    /// Files / modules this assignment was handed.
    pub fn ownership(&self) -> Vec<String> {
        self.ownership_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .unwrap_or_default()
    }

    /// Still owed to AGM: in flight, waiting for acceptance, or explicitly blocked.
    pub fn is_open(&self) -> bool {
        OPEN_STATES.contains(&self.status.as_str())
    }

    /// The daemon can still move this one on its own (retry, reconcile, close the turn).
    pub fn is_executing(&self) -> bool {
        EXECUTING_STATES.contains(&self.status.as_str())
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
pub async fn set_desired_running(pool: &SqlitePool, wanted: bool) -> Result<()> {
    sqlx::query(
        "UPDATE supervisors SET desired_running=?, watchdog_attempts=0, watchdog_next_at=NULL,
           watchdog_gave_up_at=NULL, watchdog_last_error=NULL, updated_at=? WHERE id=?",
    )
    .bind(i64::from(wanted))
    .bind(crate::db::now())
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?;
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

/// Record what is known about the remote entry point, and **where it came from**.
///
/// `source` is `argv` (we asked for a session), `manual` (a person says they reached it) or
/// `provider` (an observation, when one becomes available). `session_id` binds the claim to the
/// run it was made against, so a restart cannot inherit it.
pub async fn set_remote_observed(
    pool: &SqlitePool,
    status: &str,
    url: Option<&str>,
    source: &str,
    session_id: Option<&str>,
    actor: Option<&str>,
    evidence: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE supervisors
            SET remote_status=?, remote_url=?, remote_source=?, remote_session_id=?, remote_actor=?,
                remote_evidence=?, remote_observed_at=?, updated_at=? WHERE id=?",
    )
    .bind(status)
    .bind(url)
    .bind(source)
    .bind(session_id)
    .bind(actor)
    .bind(evidence)
    .bind(crate::db::now())
    .bind(crate::db::now())
    .bind(SUPERVISOR_ID)
    .execute(pool)
    .await?;
    Ok(())
}

/// The daemon's own bookkeeping: we started (or stopped) the session, so this is what we asked
/// for. `argv` can never mean more than `requested`.
pub async fn set_remote(pool: &SqlitePool, status: &str, url: Option<&str>) -> Result<()> {
    set_remote_observed(pool, status, url, "argv", None, None, None).await
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
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO supervisor_assignments
           (id, supervisor_id, request_id, target_bot_id, client_request_id, text, status, attempts,
            ownership_json, follow_up_of, expects_review, created_at, updated_at)
         VALUES (?,?,?,?,?,?, 'queued', 0, ?, ?, ?, ?, ?)",
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
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(assignment_by_crid(pool, client_request_id).await?.expect("just inserted"))
}

/// 把一件交辦掛到群組任務上。必須在 dispatch 之前寫：派送當下就可能撞額度，那時要看得到任務設定。
pub async fn set_mission_link(pool: &SqlitePool, id: &str, mission_id: &str, role: &str) -> Result<()> {
    sqlx::query("UPDATE supervisor_assignments SET mission_id = ?, mission_role = ?, updated_at = ? WHERE id = ?")
        .bind(mission_id)
        .bind(role)
        .bind(crate::db::now())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
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

/// 一個群組任務底下的所有交辦，舊的在前。
pub async fn mission_assignments(pool: &SqlitePool, mission_id: &str) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        "SELECT * FROM supervisor_assignments WHERE mission_id = ? ORDER BY created_at, id",
    )
    .bind(mission_id)
    .fetch_all(pool)
    .await?)
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

/// The controller's work list: assignments the daemon itself can still move (dispatch, retry,
/// close against their turn). An `awaiting_review` row is *not* here — nothing the daemon does
/// advances it, only a decision does.
pub async fn open_assignments(pool: &SqlitePool) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(
        "SELECT * FROM supervisor_assignments WHERE supervisor_id=? AND status IN ('queued','delivered','unknown')
          ORDER BY created_at ASC",
    )
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?)
}

/// Everything still owed to AGM: in flight, waiting for acceptance, or blocked. This is the
/// list the handoff, the open count and the dispatch UI read — a job whose turn happens to have
/// ended is still on it until somebody accepts it.
pub async fn unsettled_assignments(pool: &SqlitePool) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(&format!(
        "SELECT * FROM supervisor_assignments WHERE supervisor_id=?
           AND status IN ({})
          ORDER BY created_at ASC",
        sql_list(&OPEN_STATES)
    ))
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

pub async fn mark_delivered(pool: &SqlitePool, id: &str, turn_id: &str, delivery: &str) -> Result<()> {
    // `unknown` delivery is its own state: the prompt may or may not have landed, so the
    // controller reconciles it against the turn instead of sending the work a second time.
    let status = if delivery == "unknown" { "unknown" } else { "delivered" };
    sqlx::query(
        "UPDATE supervisor_assignments SET turn_id=?, delivery=?, status=?, attempts=attempts+1,
           next_attempt_at=NULL, conflict_since=NULL, updated_at=? WHERE id=?",
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

/// 409（對方回合中、沒有 run、框裡有字…）：跟 [`defer`] 一樣排下一次，並記下這一輪第一次撞 409 的時間。
/// 已經有就不動——保險絲量的是「連續送不進去多久」。
pub async fn defer_conflict(pool: &SqlitePool, id: &str, next_attempt_at: &str, why: &str) -> Result<()> {
    let now = crate::db::now();
    sqlx::query(
        "UPDATE supervisor_assignments SET attempts=attempts+1, next_attempt_at=?, error=?,
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
pub async fn mark_undeliverable(pool: &SqlitePool, id: &str, why: &str) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE supervisor_assignments SET status='blocked', error=?, next_attempt_at=NULL, updated_at=?
          WHERE id=? AND status='queued'",
    )
    .bind(why)
    .bind(crate::db::now())
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
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
    let now = crate::db::now();
    let mut tx = pool.begin().await?;
    // 通知（`expects_review=0`）而且回合是正常結束的：沒有東西要驗收，當場結案。
    // 這是 2026-09-12 兩件 `assignment_stalled` incident 的根因——AGM 說一句「收到」也要
    // 它自己回頭 review，沒 review 就被當成卡住的工作。
    let ok_turn = turn_status == "completed" || turn_status == "completed_fallback";
    let land_on = if ok_turn { "CASE WHEN expects_review=0 THEN 'completed' ELSE 'awaiting_review' END" } else { "'awaiting_review'" };
    let moved = sqlx::query(&format!(
        "UPDATE supervisor_assignments
            SET status={land_on}, turn_status=?, evidence_complete=?,
                result=COALESCE(?, result), error=COALESCE(?, error), completed_at=?, updated_at=?
          WHERE id=? AND status IN ('queued','delivered','unknown')",
    ))
    .bind(turn_status)
    .bind(i64::from(evidence_complete))
    .bind(result)
    .bind(error)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    let (bot_id, turn_id, expects_review, status): (String, Option<String>, i64, String) =
        sqlx::query_as("SELECT target_bot_id, turn_id, expects_review, status FROM supervisor_assignments WHERE id=?")
            .bind(id)
            .fetch_one(&mut *tx)
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
    let event_new = sqlx::query(
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
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    tx.commit().await?;
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
pub async fn block_stale_queue(pool: &SqlitePool, id: &str, why: &str) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let moved = block_stale_queue_tx(&mut tx, id, why).await?;
    tx.commit().await?;
    Ok(moved)
}

/// [`block_stale_queue`] 的交易內版本：保險絲要「turn 撤成功才標 blocked」，兩個寫入同一個交易。
pub async fn block_stale_queue_tx(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, id: &str, why: &str) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE supervisor_assignments SET status='blocked', error=?, updated_at=?
          WHERE id=? AND status='delivered'",
    )
    .bind(why)
    .bind(crate::db::now())
    .bind(id)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        > 0)
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
    let moved = sqlx::query(
        "UPDATE supervisor_assignments
            SET status='quota_blocked', resume_at=?, error=?, conflict_since=NULL, updated_at=?
          WHERE id=? AND status IN ('queued','delivered','unknown')",
    )
    .bind(resume_at)
    .bind(why)
    .bind(&now)
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
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
pub async fn resume_quota_blocked(pool: &SqlitePool, id: &str, event_key: &str, payload: &Value) -> Result<Settled> {
    let now = crate::db::now();
    let mut tx = pool.begin().await?;
    let moved = sqlx::query(
        "UPDATE supervisor_assignments
            SET status='queued', quota_retries=quota_retries+1, resume_at=NULL, turn_id=NULL,
                delivery=NULL, next_attempt_at=NULL, attempts=0, updated_at=?
          WHERE id=? AND status='quota_blocked'",
    )
    .bind(&now)
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    let (bot_id,): (String,) = sqlx::query_as("SELECT target_bot_id FROM supervisor_assignments WHERE id=?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let event_new = sqlx::query(
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
        > 0;
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

/// Record an acceptance decision. The caller has already checked that the transition is legal.
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
    let now = crate::db::now();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE supervisor_assignments
            SET status=?, review_decision=?, reviewed_by=?, reviewed_at=?, review_reason=?,
                followup_assignment_id=COALESCE(?, followup_assignment_id), updated_at=?
          WHERE id=?",
    )
    .bind(to_status)
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
}

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
                    anyhow::bail!("client_request_id {} already belongs to another assignment", f.client_request_id);
                }
                existing_id
            }
            None => {
                let new_id = crate::db::ulid();
                sqlx::query(
                    "INSERT INTO supervisor_assignments
                       (id, supervisor_id, request_id, target_bot_id, client_request_id, text, status, attempts,
                        ownership_json, follow_up_of, created_at, updated_at)
                     VALUES (?,?,?,?,?,?, 'queued', 0, ?, ?, ?, ?)",
                )
                .bind(&new_id)
                .bind(SUPERVISOR_ID)
                .bind(f.request_id)
                .bind(f.target_bot_id)
                .bind(f.client_request_id)
                .bind(f.text)
                .bind((!f.ownership.is_empty()).then(|| serde_json::to_string(f.ownership).unwrap_or_default()))
                .bind(id)
                .bind(&now)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
                // 接續的工作仍然是同一個任務、同一個角色（撞限換手的 followup 就是靠這個接回任務）。
                sqlx::query(
                    "UPDATE supervisor_assignments
                        SET mission_id = (SELECT mission_id FROM supervisor_assignments WHERE id = ?),
                            mission_role = (SELECT mission_role FROM supervisor_assignments WHERE id = ?),
                            review_role = (SELECT review_role FROM supervisor_assignments WHERE id = ?)
                      WHERE id = ?",
                )
                .bind(id)
                .bind(id)
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
    Ok(Some(Decided { updated, followup }))
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
    Ok(sqlx::query_as::<_, InboxEvent>(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? AND state='pending'
           AND (notify_next_at IS NULL OR notify_next_at <= ?)
           AND notify_attempts < ?
          ORDER BY created_at ASC, id ASC",
    )
    .bind(SUPERVISOR_ID)
    .bind(now)
    .bind(max_attempts)
    .fetch_all(pool)
    .await?)
}

/// Events that have spent their whole retry budget. They stay pending (nothing is swallowed),
/// but the daemon stops pushing them and raises an incident instead of burning quota forever.
pub async fn exhausted_inbox(pool: &SqlitePool, max_attempts: i64) -> Result<Vec<InboxEvent>> {
    Ok(sqlx::query_as::<_, InboxEvent>(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? AND state='pending' AND notify_attempts >= ?
           AND COALESCE(role, 'patrol')='patrol'
          ORDER BY created_at ASC",
    )
    .bind(SUPERVISOR_ID)
    .bind(max_attempts)
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
    pub expires_at: Option<String>,
    /// 呼叫端給的穩定 id（`--request-id`）。重送同一個回原本那一筆，不新增。
    #[sqlx(default)]
    pub client_request_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
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
            "expires_at": self.expires_at,
            "client_request_id": self.client_request_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
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
                _ => "approval_not_usable",
            });
        }
        if self.expires_at.as_deref().is_some_and(|t| t <= now) {
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
}

pub async fn create_approval(
    pool: &SqlitePool,
    requester: &str,
    purpose: &str,
    scope: &str,
    target_commit: Option<&str>,
    expires_at: Option<&str>,
    request_id: Option<&str>,
) -> Result<ApprovalOutcome> {
    let request_id = request_id.map(str::trim).filter(|s| !s.is_empty());
    if let Some(rid) = request_id {
        if let Some(existing) = approval_by_request(pool, rid).await? {
            // 已經被裁示的也回它本人：重送的人要看到的是「這件事已經有裁示了」，不是再開一筆。
            return same_request(existing, rid, purpose, scope, target_commit).map(|a| ApprovalOutcome { approval: a, created: false });
        }
    }
    let id = crate::db::ulid();
    let now = crate::db::now();
    let insert = sqlx::query(
        "INSERT INTO supervisor_approvals
           (id, supervisor_id, requester, purpose, scope, target_commit, status, expires_at, client_request_id, created_at, updated_at)
         VALUES (?,?,?,?,?,?, 'pending', ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(SUPERVISOR_ID)
    .bind(requester)
    .bind(purpose)
    .bind(scope)
    .bind(target_commit)
    .bind(expires_at)
    .bind(request_id)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await;
    match insert {
        Ok(_) => Ok(ApprovalOutcome { approval: approval(pool, &id).await?.expect("just inserted"), created: true }),
        // 兩個呼叫同時進來時，先到的那筆已經寫進去了（唯一鍵擋下第二筆）：回先到的那一筆。
        Err(e) => {
            let Some(rid) = request_id else { return Err(e.into()) };
            match approval_by_request(pool, rid).await? {
                Some(existing) => {
                    same_request(existing, rid, purpose, scope, target_commit).map(|a| ApprovalOutcome { approval: a, created: false })
                }
                None => Err(e.into()),
            }
        }
    }
}

fn same_request(
    existing: Approval,
    request_id: &str,
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
pub async fn oldest_live_window_approval(pool: &SqlitePool, now: &str) -> Result<Option<Approval>> {
    Ok(sqlx::query_as::<_, Approval>(
        "SELECT * FROM supervisor_approvals
          WHERE supervisor_id=? AND status='approved' AND purpose IN ('rebuild','restart')
            AND decided_at IS NOT NULL AND (expires_at IS NULL OR expires_at > ?)
          ORDER BY decided_at LIMIT 1",
    )
    .bind(SUPERVISOR_ID)
    .bind(now)
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
    let res = sqlx::query(
        "UPDATE supervisor_approvals
            SET status=?, decided_by=?, decided_at=?, reason=COALESCE(?, reason),
                expires_at=COALESCE(?, expires_at), updated_at=?
          WHERE id=? AND status=?",
    )
    .bind(status)
    .bind(actor)
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
    let row = sqlx::query_as::<_, Approval>("SELECT * FROM supervisor_approvals WHERE id=?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some((row, note_id)))
}


/// 一筆 append-only 的稽核紀錄。強制釋放這種「可以做、但要留下是誰為什麼」的動作走這裡。
pub async fn add_note(pool: &SqlitePool, kind: &str, body: &Value) -> Result<String> {
    let id = crate::db::ulid();
    sqlx::query("INSERT INTO supervisor_notes (id, supervisor_id, kind, body, version, created_at) VALUES (?,?,?,?,1,?)")
        .bind(&id)
        .bind(SUPERVISOR_ID)
        .bind(kind)
        .bind(body.to_string())
        .bind(crate::db::now())
        .execute(pool)
        .await?;
    Ok(id)
}

/// 每筆核准的決定歷程，最舊在前。一次查完再分組：核准筆數不多，但一筆一次查詢會變 N+1。
pub async fn approval_decisions(pool: &SqlitePool) -> Result<std::collections::HashMap<String, Vec<Value>>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT body, created_at FROM supervisor_notes WHERE supervisor_id=? AND kind='approval_decision' ORDER BY created_at ASC, id ASC",
    )
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?;
    let mut out: std::collections::HashMap<String, Vec<Value>> = Default::default();
    for (body, at) in rows {
        let Ok(mut v) = serde_json::from_str::<Value>(&body) else { continue };
        let Some(id) = v.get("approval_id").and_then(Value::as_str).map(str::to_string) else { continue };
        v["at"] = json!(at);
        out.entry(id).or_default().push(v);
    }
    Ok(out)
}

pub async fn set_review_role(pool: &SqlitePool, id: &str, role: &str) -> Result<()> {
    sqlx::query("UPDATE supervisor_assignments SET review_role=? WHERE id=?")
        .bind(role)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
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
        self.released_at.is_none() && self.expires_at.as_deref().is_some_and(|t| t > now)
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

/// Take the lease on `resource`, if nobody holds it.
///
/// One conditional UPDATE does the whole thing, so two callers racing for the same window
/// cannot both win: the loser's `rows_affected` is 0. `fence` is bumped on every successful
/// acquire and every operation afterwards has to present the current one — that is what stops a
/// holder that stalled past its expiry from carrying on as if it still had the window.
pub async fn acquire_lease(
    pool: &SqlitePool,
    resource: &str,
    owner: &str,
    approval_id: Option<&str>,
    target_commit: Option<&str>,
    expires_at: &str,
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
    let stale_approval = sqlx::query_scalar::<_, Option<String>>(
        "SELECT approval_id FROM supervisor_leases
          WHERE resource=? AND released_at IS NULL AND expires_at IS NOT NULL AND expires_at <= ?",
    )
    .bind(resource)
    .bind(&now)
    .fetch_optional(pool)
    .await?
    .flatten();
    let token = crate::projection::new_token();
    let taken = sqlx::query(
        "UPDATE supervisor_leases
            SET owner=?, approval_id=?, target_commit=?, fence=fence+1, acquired_at=?, expires_at=?,
                released_at=NULL, lease_token=?, detail_json=?
          WHERE resource=? AND (released_at IS NOT NULL OR expires_at IS NULL OR expires_at <= ?)",
    )
    .bind(owner)
    .bind(approval_id)
    .bind(target_commit)
    .bind(&now)
    .bind(expires_at)
    .bind(&token)
    .bind(detail.to_string())
    .bind(resource)
    .bind(&now)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
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
    Ok(sqlx::query(
        "UPDATE supervisor_leases SET expires_at=?
          WHERE resource=? AND owner=? AND fence=? AND released_at IS NULL AND expires_at > ?",
    )
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
pub async fn force_release_lease(pool: &SqlitePool, resource: &str) -> Result<bool> {
    Ok(sqlx::query("UPDATE supervisor_leases SET released_at=? WHERE resource=? AND released_at IS NULL")
        .bind(crate::db::now())
        .bind(resource)
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
pub async fn open_incident(
    pool: &SqlitePool,
    kind: &str,
    resource: &str,
    severity: &str,
    detail: &Value,
) -> Result<(Incident, bool)> {
    let now = crate::db::now();
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
    .execute(pool)
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
        .execute(pool)
        .await?;
    }
    let row = sqlx::query_as::<_, Incident>(
        "SELECT * FROM supervisor_incidents WHERE supervisor_id=? AND kind=? AND resource=? AND status='open'",
    )
    .bind(SUPERVISOR_ID)
    .bind(kind)
    .bind(resource)
    .fetch_one(pool)
    .await?;
    Ok((row, opened))
}

/// The condition cleared. `Ok(None)` = there was nothing open, which is the ordinary case.
pub async fn resolve_incident(pool: &SqlitePool, kind: &str, resource: &str) -> Result<Option<Incident>> {
    let row = sqlx::query_as::<_, Incident>(
        "SELECT * FROM supervisor_incidents WHERE supervisor_id=? AND kind=? AND resource=? AND status='open'",
    )
    .bind(SUPERVISOR_ID)
    .bind(kind)
    .bind(resource)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let now = crate::db::now();
    sqlx::query("UPDATE supervisor_incidents SET status='resolved', resolved_at=?, last_seen_at=? WHERE id=?")
        .bind(&now)
        .bind(&now)
        .bind(&row.id)
        .execute(pool)
        .await?;
    Ok(sqlx::query_as::<_, Incident>("SELECT * FROM supervisor_incidents WHERE id=?")
        .bind(&row.id)
        .fetch_optional(pool)
        .await?)
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
        let a = insert_assignment(&p, None, "bot1", "req-1", "do the thing", &[], None, true).await.unwrap();
        assert_eq!(a.status, "queued");
        assert!(insert_assignment(&p, None, "bot1", "req-1", "do the thing", &[], None, true).await.is_err());
        assert_eq!(assignment_by_crid(&p, "req-1").await.unwrap().unwrap().id, a.id);
    }

    /// 通知（`expects_review=false`）：送到、回合結束就自己結案。
    ///
    /// 2026-09-12 兩件 `assignment_stalled` incident 的根因——AGM 對 bot 說一句「收到」也被
    /// 當成一份要驗收的工作，沒人 review 就被 controller 報成卡住。
    #[tokio::test]
    async fn a_notice_closes_itself_when_the_turn_ends() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "notice-1", "收到，進 idle", &[], None, false).await.unwrap();
        assert!(a.is_notice());
        mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
        let s = settle_and_notify(&p, &a.id, "completed", true, Some("好"), None, "k1", "assignment_completed", &json!({}))
            .await
            .unwrap();
        assert!(s.moved);
        let after = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(after.status, "completed", "通知不需要驗收");
        assert!(after.completed_at.is_some());
        assert_eq!(awaiting_review_count(&p).await.unwrap(), 0);
        // inbox 事件還是有（AGM 想看得到），但它不是待辦。
        let ev = pending_inbox(&p).await.unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, "assignment_noticed");
        let payload: Value = serde_json::from_str(&ev[0].payload_json).unwrap();
        assert_eq!(payload["needs_review"], json!(false));
    }

    /// …但**失敗**的通知還是要有人看：送不出去這件事本身是壞消息。
    #[tokio::test]
    async fn a_notice_that_failed_still_waits_for_agm() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "notice-2", "看完即可", &[], None, false).await.unwrap();
        settle_and_notify(&p, &a.id, "dispatch_failed", true, None, Some("bot has no active run"), "k2", "assignment_failed", &json!({}))
            .await
            .unwrap();
        let after = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(after.status, "awaiting_review");
        assert_eq!(awaiting_review_count(&p).await.unwrap(), 1);
    }

    /// 一般交辦不受影響：回合結束照樣停在 awaiting_review。
    #[tokio::test]
    async fn a_task_still_waits_for_acceptance() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "task-1", "做這個", &[], None, true).await.unwrap();
        settle_and_notify(&p, &a.id, "completed", true, Some("做完了"), None, "k3", "assignment_completed", &json!({}))
            .await
            .unwrap();
        assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "awaiting_review");
    }

    /// 撞到用量上限不是失敗也不是完成：停在 `quota_blocked`，等額度回來自己重送。
    #[tokio::test]
    async fn a_quota_block_parks_and_then_resumes_with_a_fresh_crid() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "job-1", "跑那個批次", &[], None, true).await.unwrap();
        mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
        let parked = park_quota_blocked(&p, &a.id, "2026-09-13T01:00:00Z", "撞到上限", "qb:1", &json!({}))
            .await
            .unwrap();
        assert!(parked.moved && parked.event_new);
        let blocked = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(blocked.status, "quota_blocked");
        assert_eq!(blocked.resume_at.as_deref(), Some("2026-09-13T01:00:00Z"));
        assert!(blocked.is_open(), "工作還沒做完，只是在等額度");
        // 這一段時間內它不算「卡住」——incident 探針看不到它。
        assert!(assignments_idle_since(&p, "2999-01-01T00:00:00Z").await.unwrap().iter().all(|x| x.id != a.id));

        let resumed = resume_quota_blocked(&p, &a.id, "qr:1", &json!({})).await.unwrap();
        assert!(resumed.moved);
        let back = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(back.status, "queued");
        assert_eq!(back.quota_retries, 1);
        assert!(back.turn_id.is_none(), "下一次是新的 turn");
        // crid 換過了，否則 `lifecycle::prompt` 會直接把上一個撞牆的 turn 還回來。
        assert_eq!(back.dispatch_crid(), "job-1#r1");
        assert_eq!(a.dispatch_crid(), "job-1");
        // 兩則通知都在（擋下 / 重送），而且都不是待辦。
        let kinds: Vec<String> = pending_inbox(&p).await.unwrap().into_iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&"assignment_quota_blocked".to_string()));
        assert!(kinds.contains(&"assignment_quota_resumed".to_string()));
    }

    /// 同一次擋下重放幾遍只會有一則通知（event_key 帶重送次數）。
    #[tokio::test]
    async fn parking_twice_does_not_wake_agm_twice() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "job-2", "x", &[], None, true).await.unwrap();
        let first = park_quota_blocked(&p, &a.id, "2026-09-13T01:00:00Z", "撞到上限", "qb:2", &json!({})).await.unwrap();
        let again = park_quota_blocked(&p, &a.id, "2026-09-13T01:00:00Z", "撞到上限", "qb:2", &json!({})).await.unwrap();
        assert!(first.event_new);
        assert!(!again.event_new, "重放不再通知一次");
        assert!(!again.moved, "已經在 quota_blocked 了");
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
        mark_delivered_inbox(&p, &[id.clone()], "turn-9", "ok").await.unwrap();
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
        let a = insert_assignment(&p, None, "bot1", "req-1", "x", &[], None, true).await.unwrap();
        assert_eq!(pending_count(&p).await.unwrap(), 1);
        mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
        assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "delivered");
        assert_eq!(pending_count(&p).await.unwrap(), 1, "delivered work is still open work");
        settle_and_notify(&p, &a.id, "completed", true, Some("done"), None, "k-done", "assignment_completed", &json!({}))
            .await
            .unwrap();
        assert_eq!(
            pending_count(&p).await.unwrap(),
            2,
            "a finished turn is still open work, plus the notification nobody has read"
        );
        review(&p, &a.id, "accept", "AGM", "cli", Some("編譯通過"), Some("turn t1"), None).await.unwrap();
        let done = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(done.status, "completed");
        assert_eq!(done.result.as_deref(), Some("done"));
        assert_eq!(open_assignment_count(&p).await.unwrap(), 0, "accepted work leaves the open list");
    }

    /// Giving up is reported once, not once per tick (review #31), and a recovery re-arms it so
    /// the *next* outage is not silenced by a marker left over from the last one.
    #[tokio::test]
    async fn the_watchdog_reports_giving_up_once_and_a_recovery_re_arms_it() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        set_desired_running(&p, true).await.unwrap();
        set_watchdog(&p, 5, None).await.unwrap();

        assert!(mark_watchdog_gave_up(&p, "CLI 起來就死").await.unwrap(), "the first tick reports");
        for _ in 0..50 {
            assert!(!mark_watchdog_gave_up(&p, "CLI 起來就死").await.unwrap(), "later ticks stay silent");
        }
        let s = get_or_init(&p).await.unwrap();
        assert!(s.watchdog_gave_up_at.is_some());
        assert_eq!(s.watchdog_last_error.as_deref(), Some("CLI 起來就死"));

        // Seen answering again: the streak resets and so does the give-up record.
        set_watchdog(&p, 0, None).await.unwrap();
        let s = get_or_init(&p).await.unwrap();
        assert!(s.watchdog_gave_up_at.is_none(), "a recovery clears it");
        assert!(s.watchdog_last_error.is_none());
        assert!(mark_watchdog_gave_up(&p, "第二次").await.unwrap(), "so a later outage is reported again");

        // A human deciding (start / stop) also clears it.
        set_desired_running(&p, true).await.unwrap();
        let s = get_or_init(&p).await.unwrap();
        assert!(s.watchdog_gave_up_at.is_none());
        assert_eq!((s.watchdog_attempts, s.watchdog_next_at), (0, None));

        // A non-zero update must *not* clear it: that is the retry bookkeeping, not a recovery.
        mark_watchdog_gave_up(&p, "第三次").await.unwrap();
        set_watchdog(&p, 5, Some("2099-01-01T00:00:00Z")).await.unwrap();
        assert!(get_or_init(&p).await.unwrap().watchdog_gave_up_at.is_some());
    }

    /// The stopped-bot case (review #30). Every retry calls `defer`, which moves `updated_at`,
    /// so work bouncing off a dead bot every five minutes looks *busy* forever and the idle
    /// probe never sees it. The undelivered probe clocks it from `created_at` instead.
    #[tokio::test]
    async fn work_that_keeps_retrying_is_visible_even_though_it_never_looks_idle() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "stopped-bot", "req-r", "做事", &[], None, true).await.unwrap();
        // Created long ago; retried 20 times, the last one just now. `defer` stamps
        // `updated_at` off the real clock, so the cutoff has to sit between the two: older than
        // "now" (or the idle probe would match anything) and newer than `created_at`.
        sqlx::query("UPDATE supervisor_assignments SET created_at='2020-01-01T00:00:00.000Z' WHERE id=?")
            .bind(&a.id)
            .execute(&p)
            .await
            .unwrap();
        for _ in 0..20 {
            defer(&p, &a.id, "2099-01-01T00:00:00Z", "bot has no active run").await.unwrap();
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(60))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let cutoff = cutoff.as_str();
        assert!(
            assignments_idle_since(&p, cutoff).await.unwrap().is_empty(),
            "the idle probe is blind to it: every retry refreshes updated_at"
        );
        let seen = assignments_undelivered_since(&p, cutoff).await.unwrap();
        assert_eq!(seen.len(), 1, "but never-delivered work is clocked from created_at");
        assert_eq!(seen[0].attempts, 20);
        assert_eq!(seen[0].error.as_deref(), Some("bot has no active run"), "with the real reason, not `conflict`");
        assert_eq!(seen[0].to_json()["next_attempt_at"], "2099-01-01T00:00:00Z", "and when it will try again");

        // Once it actually goes out it is no longer undelivered, whatever happens next.
        mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
        assert!(assignments_undelivered_since(&p, cutoff).await.unwrap().is_empty());
    }

    /// Cancelling work that is or may be running is allowed — and must not pretend it un-sent
    /// anything. The transport facts survive the decision precisely so nobody has to guess.
    #[tokio::test]
    async fn cancelling_unknown_delivery_keeps_the_evidence_that_it_may_be_running() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-u", "x", &[], None, true).await.unwrap();
        mark_delivered(&p, &a.id, "t7", "unknown").await.unwrap();
        assert_eq!(assignment(&p, &a.id).await.unwrap().unwrap().status, "unknown");

        review(&p, &a.id, "cancel", "AGM", "cli", Some("使用者改主意"), Some("送達未知，沒有中止回合"), None)
            .await
            .unwrap();
        let a = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(a.status, "cancelled");
        assert!(!a.is_open());
        // The two facts that stop `cancelled` from being read as "it never went out".
        assert_eq!(a.delivery.as_deref(), Some("unknown"), "delivery is not rewritten by the decision");
        assert_eq!(a.turn_id.as_deref(), Some("t7"), "and the turn is still named, so it can be checked");
        let log = reviews(&p, &a.id).await.unwrap();
        assert!(log[0]["evidence"].as_str().unwrap().contains("沒有中止回合"));
    }

    /// The heart of the 2026-09-12 P1: a turn ending is not a job being done.
    ///
    /// `01M246903Z54XW872GWD7XXJAE` answered "still waiting on the build, will report back" and
    /// the row said `completed`. Now that reply parks the assignment on `awaiting_review`, where
    /// it stays on the open list until somebody decides.
    #[tokio::test]
    async fn a_finished_turn_waits_for_acceptance_instead_of_closing_itself() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-a", "改好那個 bug", &[], None, true).await.unwrap();
        mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
        let s = settle_and_notify(
            &p,
            &a.id,
            "completed",
            true,
            Some("還在等編譯，等一下回報"),
            None,
            "assignment_completed:a:t1",
            "assignment_completed",
            &json!({"result": "還在等編譯"}),
        )
        .await
        .unwrap();
        assert_eq!(s, Settled { moved: true, event_new: true });
        let a = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(a.status, "awaiting_review");
        assert!(a.is_open(), "still owed to the manager");
        assert!(!a.is_executing(), "but the daemon has nothing left to do for it");
        assert_eq!(a.turn_status.as_deref(), Some("completed"), "the raw fact is kept");
        assert_eq!(awaiting_review_count(&p).await.unwrap(), 1);
        assert!(open_assignments(&p).await.unwrap().is_empty(), "not on the controller's work list");
        assert_eq!(unsettled_assignments(&p).await.unwrap().len(), 1, "but on the manager's");

        // "Still waiting on the build" is not an acceptance: blocking keeps it open.
        review(&p, &a.id, "block", "AGM", "cli", Some("等編譯"), None, None).await.unwrap();
        let a = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(a.status, "blocked");
        assert!(a.is_open());
        assert_eq!(open_assignment_count(&p).await.unwrap(), 1);
    }

    /// Every decision names who made it, on what grounds. Without that the state machine is
    /// just a different way of asserting the same unfalsifiable "it is done".
    #[tokio::test]
    async fn every_decision_is_recorded_with_its_actor_and_evidence() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-b", "x", &[], None, true).await.unwrap();
        settle_and_notify(&p, &a.id, "completed", true, Some("ok"), None, "k", "assignment_completed", &json!({}))
            .await
            .unwrap();
        review(&p, &a.id, "accept", "AGM", "cli", Some("測試通過"), Some("commit abc123"), None).await.unwrap();
        let log = reviews(&p, &a.id).await.unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0]["decision"], "accept");
        assert_eq!(log[0]["from_status"], "awaiting_review");
        assert_eq!(log[0]["to_status"], "completed");
        assert_eq!(log[0]["actor"], "AGM");
        assert_eq!(log[0]["evidence"], "commit abc123");
    }

    /// A continuation is a new assignment pointing back at the old one — never a rewrite of
    /// text that has already been sent to a bot.
    #[tokio::test]
    async fn a_follow_up_is_a_new_assignment_linked_to_the_old_one() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let first = insert_assignment(&p, None, "bot1", "req-1", "做 A", &[], None, true).await.unwrap();
        settle_and_notify(&p, &first.id, "completed", true, Some("A 做了一半"), None, "k1", "assignment_completed", &json!({}))
            .await
            .unwrap();
        let next = insert_assignment(&p, None, "bot1", "req-1-follow", "把 A 做完", &[], Some(&first.id), true).await.unwrap();
        link_followup(&p, &first.id, &next.id).await.unwrap();
        review(&p, &first.id, "followup", "AGM", "cli", Some("還差一半"), None, Some(&next.id)).await.unwrap();
        let first = assignment(&p, &first.id).await.unwrap().unwrap();
        assert_eq!(first.status, "superseded");
        assert_eq!(first.followup_assignment_id.as_deref(), Some(next.id.as_str()));
        assert!(!first.is_open(), "the original is closed…");
        let next = assignment(&p, &next.id).await.unwrap().unwrap();
        assert_eq!(next.follow_up_of.as_deref(), Some(first.id.as_str()));
        assert!(next.is_open(), "…because the follow-up carries the work");
        assert_eq!(first.text, "做 A", "the delivered words are never rewritten");
    }

    fn spec<'a>(target: &'a str, crid: &'a str, text: &'a str, owns: &'a [String]) -> FollowupSpec<'a> {
        FollowupSpec { target_bot_id: target, client_request_id: crid, text, ownership: owns, request_id: None }
    }

    /// Two callers deciding the same assignment at once must not each派 a continuation. The
    /// guard is the status the caller validated against: the loser matches no rows, rolls back
    /// whole, and is told it lost — *before* anything has been sent to a bot.
    #[tokio::test]
    async fn two_concurrent_followups_on_one_parent_create_exactly_one_continuation() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let parent = insert_assignment(&p, None, "bot1", "req-p", "做 A", &[], None, true).await.unwrap();
        settle_and_notify(&p, &parent.id, "completed", true, Some("做一半"), None, "k", "assignment_completed", &json!({}))
            .await
            .unwrap();

        let first = review_with_followup(
            &p, &parent.id, "awaiting_review", "followup", "AGM", "cli", Some("還差一半"), None,
            Some(spec("bot1", "follow-A", "把 A 做完", &[])),
        )
        .await
        .unwrap()
        .expect("the first decision wins");
        assert_eq!(first.updated.status, "superseded");
        let first_child = first.followup.expect("a continuation was created");

        // The second caller read the same `awaiting_review` and is deciding with a *different*
        // request id — the case that used to produce two continuations.
        let second = review_with_followup(
            &p, &parent.id, "awaiting_review", "followup", "AGM", "web", Some("我也覺得要續作"), None,
            Some(spec("bot1", "follow-B", "另一份續作", &[])),
        )
        .await
        .unwrap();
        assert!(second.is_none(), "the loser writes nothing at all");

        let all = list_assignments(&p, 50).await.unwrap();
        assert_eq!(all.len(), 2, "one parent, one continuation — not two");
        assert!(all.iter().all(|a| a.client_request_id != "follow-B"), "the loser's row was never inserted");
        assert_eq!(reviews(&p, &parent.id).await.unwrap().len(), 1, "and no second audit entry");
        assert_eq!(
            assignment(&p, &parent.id).await.unwrap().unwrap().followup_assignment_id.as_deref(),
            Some(first_child.id.as_str())
        );

        // A byte-identical retry of the winner is idempotent at the assignment level: the same
        // request id finds the row that already exists rather than making a second one.
        let retry = review_with_followup(
            &p, &parent.id, "superseded", "followup", "AGM", "cli", Some("重送"), None,
            Some(spec("bot1", "follow-A", "把 A 做完", &[])),
        )
        .await
        .unwrap()
        .expect("the parent is still where this caller thinks it is");
        assert_eq!(retry.followup.unwrap().id, first_child.id);
        assert_eq!(list_assignments(&p, 50).await.unwrap().len(), 2, "still two rows");
    }

    /// If any part of the decision fails, none of it lands: no superseded parent, no audit row,
    /// and above all no queued continuation that nobody is tracking.
    #[tokio::test]
    async fn a_failed_followup_rolls_back_and_leaves_no_orphan_queued_row() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let parent = insert_assignment(&p, None, "bot1", "req-p", "做 A", &[], None, true).await.unwrap();
        settle_and_notify(&p, &parent.id, "completed", true, Some("做一半"), None, "k", "assignment_completed", &json!({}))
            .await
            .unwrap();
        // Somebody else's assignment already owns this client_request_id.
        let other = insert_assignment(&p, None, "bot2", "taken-crid", "別人的工作", &[], None, true).await.unwrap();

        let err = review_with_followup(
            &p, &parent.id, "awaiting_review", "followup", "AGM", "cli", Some("續作"), None,
            Some(spec("bot1", "taken-crid", "把 A 做完", &[])),
        )
        .await;
        assert!(err.is_err(), "adopting someone else's request id is refused");

        let parent_now = assignment(&p, &parent.id).await.unwrap().unwrap();
        assert_eq!(parent_now.status, "awaiting_review", "the parent was not superseded");
        assert!(parent_now.review_decision.is_none(), "and no decision was recorded");
        assert!(parent_now.followup_assignment_id.is_none());
        assert!(reviews(&p, &parent.id).await.unwrap().is_empty(), "no audit row either");
        let all = list_assignments(&p, 50).await.unwrap();
        assert_eq!(all.len(), 2, "no orphan continuation was left behind");
        assert!(all.iter().all(|a| a.follow_up_of.is_none()));
        assert_eq!(assignment(&p, &other.id).await.unwrap().unwrap().text, "別人的工作", "and the other row is untouched");
    }

    /// The transactional outbox: either the assignment moves and the event is queued, or
    /// neither happens. A replay of the same outcome changes nothing.
    #[tokio::test]
    async fn settling_twice_moves_nothing_and_queues_one_event() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-c", "x", &[], None, true).await.unwrap();
        mark_delivered(&p, &a.id, "t9", "ok").await.unwrap();
        let key = "assignment_completed:a:t9";
        let first = settle_and_notify(&p, &a.id, "completed", true, Some("r"), None, key, "assignment_completed", &json!({}))
            .await
            .unwrap();
        let again = settle_and_notify(&p, &a.id, "completed", true, Some("r"), None, key, "assignment_completed", &json!({}))
            .await
            .unwrap();
        assert_eq!(first, Settled { moved: true, event_new: true });
        assert_eq!(again, Settled { moved: false, event_new: false }, "a duplicate completion is not a second event");
        assert_eq!(pending_inbox(&p).await.unwrap().len(), 1);
    }

    /// If the notification cannot be written, the assignment does not move either.
    ///
    /// The failure is forced by dropping the inbox table — crude, but it exercises the real
    /// rollback path, and the property is the one that matters: there is no state in which the
    /// work is closed and nobody was told.
    #[tokio::test]
    async fn a_failed_notification_rolls_the_whole_settle_back() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-tx", "x", &[], None, true).await.unwrap();
        mark_delivered(&p, &a.id, "t1", "ok").await.unwrap();
        sqlx::query("DROP TABLE supervisor_inbox").execute(&p).await.unwrap();
        let err = settle_and_notify(&p, &a.id, "completed", true, Some("done"), None, "k", "assignment_completed", &json!({}))
            .await;
        assert!(err.is_err(), "the write failed");
        let a = assignment(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(a.status, "delivered", "so the assignment is still open and will be reconciled again");
        assert!(a.turn_status.is_none(), "and nothing of the half-written outcome survived");
    }

    /// A result closed by an older daemon, with nobody ever told: the sweep finds it. Once the
    /// write is transactional this can only be a pre-upgrade row — which is exactly the backlog
    /// the review was worried about.
    #[tokio::test]
    async fn a_settled_assignment_with_no_event_is_found_by_the_sweep() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-d", "x", &[], None, true).await.unwrap();
        sqlx::query("UPDATE supervisor_assignments SET status='awaiting_review', turn_status='completed' WHERE id=?")
            .bind(&a.id)
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(settled_without_event(&p, 10).await.unwrap().len(), 1);
        push_inbox(&p, "k", "assignment_completed", Some(&a.id), None, None, &json!({})).await.unwrap();
        assert!(settled_without_event(&p, 10).await.unwrap().is_empty(), "once queued it is not swept again");
    }

    /// `unknown` delivery is its own state precisely so the controller reconciles it instead
    /// of sending the same job a second time.
    #[tokio::test]
    async fn unknown_delivery_is_not_folded_into_delivered() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = insert_assignment(&p, None, "bot1", "req-2", "x", &[], None, true).await.unwrap();
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
        mark_delivered_inbox(&p, &[open[1].id.clone()], "t1", "ok").await.unwrap();
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

    /// The ACK / mark_delivered race. The manager reads the digest out of the prompt and acks
    /// before the delivery write lands; without the guard that write reopens the event and the
    /// same news is pushed at it again.
    #[tokio::test]
    async fn an_acknowledgement_never_goes_backwards() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let id = push_inbox(&p, "k1", "assignment_completed", None, None, None, &json!({})).await.unwrap().unwrap();
        assert!(ack_inbox(&p, &id).await.unwrap());
        mark_delivered_inbox(&p, &[id.clone()], "turn-1", "ok").await.unwrap();
        let e = inbox(&p, 10).await.unwrap().into_iter().find(|e| e.id == id).unwrap();
        assert_eq!(e.state, "handled", "the late delivery write must not un-ack it");
        assert_eq!(open_inbox_count(&p).await.unwrap(), 0);
    }

    /// Delivery is not an answer. A failed send leaves everything pending with a backoff; a
    /// delivered-but-unacked event can be handed back; the retry budget is bounded, and an
    /// exhausted event is still kept rather than dropped.
    #[tokio::test]
    async fn notifications_are_retried_a_bounded_number_of_times_and_never_dropped() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let id = push_inbox(&p, "k1", "assignment_completed", None, None, None, &json!({})).await.unwrap().unwrap();
        let ids = [id.clone()];

        // A send that failed: still pending, but not due until the backoff passes.
        defer_notify(&p, &ids, "2099-01-01T00:00:00Z", "delivery failed").await.unwrap();
        assert!(due_inbox(&p, "2026-09-12T00:00:00Z", 5).await.unwrap().is_empty(), "backoff holds it");
        assert_eq!(due_inbox(&p, "2099-06-01T00:00:00Z", 5).await.unwrap().len(), 1, "and then releases it");

        // Delivered, unanswered, handed back: the notify turn is kept so the next attempt can
        // check it instead of blindly sending a second copy.
        mark_delivered_inbox(&p, &ids, "turn-7", "unknown").await.unwrap();
        assert!(pending_inbox(&p).await.unwrap().is_empty());
        assert_eq!(delivered_inbox(&p).await.unwrap().len(), 1);
        assert!(requeue_inbox(&p, &id, "delivered but never acknowledged").await.unwrap());
        let e = pending_inbox(&p).await.unwrap().remove(0);
        assert_eq!(e.notify_turn_id.as_deref(), Some("turn-7"));
        assert_eq!(e.notify_delivery.as_deref(), Some("unknown"), "unknown delivery stays unknown");
        assert!(!requeue_inbox(&p, &id, "again").await.unwrap(), "only a delivered event can be requeued");

        // Spend the budget. The event never leaves the inbox; what stops is the pushing.
        for _ in 0..5 {
            defer_notify(&p, &ids, "2000-01-01T00:00:00Z", "nope").await.unwrap();
        }
        assert!(due_inbox(&p, "2099-01-01T00:00:00Z", 5).await.unwrap().is_empty(), "budget spent, no more sends");
        assert_eq!(exhausted_inbox(&p, 5).await.unwrap().len(), 1, "and it is visible as an incident source");
        assert_eq!(open_inbox_count(&p).await.unwrap(), 1, "still owed, never swallowed");
    }

    /// Incidents are one row per resource, deduplicated across restarts, with a fresh row for a
    /// recurrence so "it happened again" is not lost in an old timestamp.
    #[tokio::test]
    async fn an_incident_is_one_row_per_resource_and_a_recurrence_is_a_new_one() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let (first, opened) = open_incident(&p, "host_disconnected", "mac2", "degraded", &json!({"host": "mac2"}))
            .await
            .unwrap();
        assert!(opened);
        let (again, opened) = open_incident(&p, "host_disconnected", "mac2", "degraded", &json!({})).await.unwrap();
        assert!(!opened, "a second tick refreshes the incident, it does not open another");
        assert_eq!(again.id, first.id);
        assert_eq!(again.occurrences, 2);
        assert_eq!(open_incidents(&p).await.unwrap().len(), 1);

        let resolved = resolve_incident(&p, "host_disconnected", "mac2").await.unwrap().unwrap();
        assert_eq!(resolved.status, "resolved");
        assert!(resolved.resolved_at.is_some());
        assert!(open_incidents(&p).await.unwrap().is_empty());
        assert!(resolve_incident(&p, "host_disconnected", "mac2").await.unwrap().is_none(), "resolving twice is a no-op");

        let (third, opened) = open_incident(&p, "host_disconnected", "mac2", "degraded", &json!({})).await.unwrap();
        assert!(opened, "the same fault coming back is a new occurrence");
        assert_ne!(third.id, first.id);
        assert_eq!(incidents(&p, 10).await.unwrap().len(), 2, "the resolved one is still on the record");
    }

    /// The upgrade path. Rows closed under the old "turn ended = done" rule stay closed — a
    /// backfill must not re-dispatch a month of work — but they are marked `legacy_closed`, so
    /// nothing presents them as having been accepted by anyone.
    #[tokio::test]
    async fn the_migration_marks_old_rows_legacy_instead_of_reopening_them() {
        let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        for stmt in DDL.split(";\n") {
            let s = stmt.trim();
            if !s.is_empty() {
                sqlx::query(s).execute(&p).await.unwrap();
            }
        }
        for (id, status) in [("a1", "completed"), ("a2", "failed"), ("a3", "delivered"), ("a4", "cancelled")] {
            sqlx::query(
                "INSERT INTO supervisor_assignments
                   (id, supervisor_id, target_bot_id, client_request_id, text, status, attempts, created_at, updated_at)
                 VALUES (?,?, 'bot1', ?, 'x', ?, 0, 't', 't')",
            )
            .bind(id)
            .bind(SUPERVISOR_ID)
            .bind(format!("crid-{id}"))
            .bind(status)
            .execute(&p)
            .await
            .unwrap();
        }
        assert!(!has_column(&p, "supervisor_assignments", "legacy_closed").await.unwrap());
        migrate(&p).await.unwrap();
        migrate(&p).await.unwrap(); // idempotent
        let rows: Vec<(String, String, i64, Option<String>)> =
            sqlx::query_as("SELECT id, status, legacy_closed, turn_status FROM supervisor_assignments ORDER BY id")
                .fetch_all(&p)
                .await
                .unwrap();
        assert_eq!(rows[0], ("a1".into(), "completed".into(), 1, Some("completed".into())));
        assert_eq!(rows[1], ("a2".into(), "failed".into(), 1, None));
        assert_eq!(rows[2], ("a3".into(), "delivered".into(), 0, None), "work in flight is untouched");
        assert_eq!(rows[3], ("a4".into(), "cancelled".into(), 1, None));
        assert_eq!(open_assignment_count(&p).await.unwrap(), 1, "the backfill does not re-open closed work");
        // And the sweep leaves them alone: nobody wants a month of old results pushed at AGM.
        assert!(settled_without_event(&p, 10).await.unwrap().is_empty());
    }

    /// Ownership is recorded on the assignment so an overlap can be reported. It is data for
    /// AGM to arbitrate with, not a lock.
    #[tokio::test]
    async fn an_assignment_remembers_the_files_it_was_handed() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let owns = vec!["daemon/src/supervisor".to_string(), "scripts/agm.py".to_string()];
        let a = insert_assignment(&p, None, "bot1", "req-o", "x", &owns, None, true).await.unwrap();
        assert_eq!(a.ownership(), owns);
        assert_eq!(a.to_json()["ownership"][1], "scripts/agm.py");
        let b = insert_assignment(&p, None, "bot2", "req-p", "x", &[], None, true).await.unwrap();
        assert!(b.ownership().is_empty());
    }

    /// Two executors racing for the same rebuild window: exactly one wins. This is the whole
    /// reason the lease exists — the old flow let both read "nothing is working" and proceed.
    #[tokio::test]
    async fn two_executors_race_for_one_window_and_only_one_gets_it() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let soon = "2099-01-01T00:00:00Z";
        let first = acquire_lease(&p, "rebuild", "bot-a", Some("ap1"), Some("abc"), soon, &json!({})).await.unwrap();
        let second = acquire_lease(&p, "rebuild", "bot-b", Some("ap2"), Some("abc"), soon, &json!({})).await.unwrap();
        let first = first.expect("first acquire wins");
        assert!(second.is_none(), "the second executor is refused while the window is held");
        assert_eq!(first.owner.as_deref(), Some("bot-a"));
        assert!(first.held_at(&crate::db::now()));

        // The holder can renew; nobody else can, and neither can a stale fence.
        assert!(renew_lease(&p, "rebuild", "bot-a", first.fence, soon).await.unwrap());
        assert!(!renew_lease(&p, "rebuild", "bot-b", first.fence, soon).await.unwrap(), "not your lease");
        assert!(!renew_lease(&p, "rebuild", "bot-a", first.fence - 1, soon).await.unwrap(), "stale fence");

        // Released: the next executor gets it, with a higher fence.
        assert!(release_lease(&p, "rebuild", "bot-a", first.fence).await.unwrap());
        assert!(!release_lease(&p, "rebuild", "bot-a", first.fence).await.unwrap(), "releasing twice is a no-op");
        let second = acquire_lease(&p, "rebuild", "bot-b", Some("ap2"), Some("abc"), soon, &json!({}))
            .await
            .unwrap()
            .expect("free again");
        assert!(second.fence > first.fence, "the fence only goes up");
    }

    /// A holder that died does not lock the window forever, and cannot carry on afterwards: its
    /// fence is stale the moment somebody else takes over.
    #[tokio::test]
    async fn an_expired_lease_is_taken_over_and_the_old_token_stops_working() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let dead = acquire_lease(&p, "restart", "bot-a", None, None, "2000-01-01T00:00:00Z", &json!({}))
            .await
            .unwrap()
            .unwrap();
        assert!(!dead.held_at(&crate::db::now()), "already expired");
        let next = acquire_lease(&p, "restart", "bot-b", None, None, "2099-01-01T00:00:00Z", &json!({}))
            .await
            .unwrap()
            .expect("an expired lease does not block the next window");
        assert_eq!(next.owner.as_deref(), Some("bot-b"));
        assert!(
            !renew_lease(&p, "restart", "bot-a", dead.fence, "2099-01-01T00:00:00Z").await.unwrap(),
            "the old holder's token is worthless even though its approval may still say approved"
        );
        assert!(!release_lease(&p, "restart", "bot-a", dead.fence).await.unwrap());
    }

    /// The approval is a record, and revoking it takes effect for the next acquire even though
    /// the words "AGM said yes" were true yesterday.
    /// 起因：2026-09-16 k8bw2f 對同一顆 commit 送了兩筆一樣的 restart 申請（它讀錯回應欄位，
    /// 以為沒送出去），AGM 只能核一筆、駁一筆。同一個 request id 重送要回原本那一筆。
    #[tokio::test]
    async fn one_request_id_is_one_approval() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        async fn ask(p: &SqlitePool, rid: Option<&str>) -> ApprovalOutcome {
            create_approval(p, "k8bw2f", "restart", "daemon", Some("ca7b22d"), None, rid).await.unwrap()
        }
        let first = ask(&p, Some("restart-ca7b22d")).await;
        assert!(first.created);
        assert_eq!(first.approval.client_request_id.as_deref(), Some("restart-ca7b22d"));

        let again = ask(&p, Some("restart-ca7b22d")).await;
        assert!(!again.created, "重送不是新的一筆");
        assert_eq!(again.approval.id, first.approval.id);
        assert_eq!(approvals(&p, 10).await.unwrap().len(), 1, "DB 裡還是只有一列");

        // 不同 id 就是不同的申請。
        let other = ask(&p, Some("restart-ca7b22d-retry")).await;
        assert!(other.created);
        assert_ne!(other.approval.id, first.approval.id);
        assert_eq!(approvals(&p, 10).await.unwrap().len(), 2);

        // 沒帶 id 的照舊：每次都開一筆新的（既有腳本不受影響）。
        let a = ask(&p, None).await;
        let b = ask(&p, None).await;
        assert!(a.created && b.created);
        assert_ne!(a.approval.id, b.approval.id);
        assert_eq!(approvals(&p, 10).await.unwrap().len(), 4);
    }

    /// 同一個 id 換了內容不是重送：回 409（`ApprovalRequestMismatch`），原本那筆一個字都不動。
    #[tokio::test]
    async fn the_same_request_id_with_different_content_is_refused() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let first = create_approval(&p, "bot", "rebuild", "daemon", Some("abc123"), None, Some("nightly"))
            .await
            .unwrap()
            .approval;

        for (purpose, scope, commit, field) in [
            ("rebuild", "daemon", Some("def456"), "target_commit"),
            ("restart", "daemon", Some("abc123"), "purpose"),
            ("rebuild", "web", Some("abc123"), "scope"),
        ] {
            let err = create_approval(&p, "bot", purpose, scope, commit, None, Some("nightly")).await.unwrap_err();
            let m = err.downcast::<ApprovalRequestMismatch>().expect("回的是內容不符，不是別的錯");
            assert_eq!(m.field, field);
            assert_eq!(m.approval_id, first.id, "錯誤訊息要指回原本那筆");
        }
        let kept = approval(&p, &first.id).await.unwrap().unwrap();
        assert_eq!((kept.purpose.as_str(), kept.scope.as_str(), kept.target_commit.as_deref()), ("rebuild", "daemon", Some("abc123")));
        assert_eq!(approvals(&p, 10).await.unwrap().len(), 1, "被拒絕的那幾次什麼都沒寫進去");
    }

    /// 已經裁示過的同 id 再送，回的還是它本人——重送的人要看到的是「這件事已經有裁示了」。
    #[tokio::test]
    async fn a_decided_request_id_comes_back_as_itself() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        for status in ["approved", "denied", "revoked", "consumed"] {
            let rid = format!("win-{status}");
            let first = create_approval(&p, "bot", "rebuild", "daemon", Some("abc123"), None, Some(&rid))
                .await
                .unwrap()
                .approval;
            decide_approval(&p, &first.id, status, "AGM", None, None).await.unwrap();
            let again = create_approval(&p, "bot", "rebuild", "daemon", Some("abc123"), None, Some(&rid)).await.unwrap();
            assert!(!again.created, "{status}：不該因為已經決定就再開一筆");
            assert_eq!(again.approval.id, first.id);
            assert_eq!(again.approval.status, status, "{status}：回的是它現在的裁示");
        }
        assert_eq!(approvals(&p, 10).await.unwrap().len(), 4);
    }

    /// 舊資料庫（沒有 `client_request_id` 那一欄）升級後照常可讀，既有列留白。
    #[tokio::test]
    async fn an_old_database_gains_the_column_without_losing_rows() {
        let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE supervisor_approvals (
               id TEXT PRIMARY KEY, supervisor_id TEXT NOT NULL, requester TEXT NOT NULL,
               purpose TEXT NOT NULL, scope TEXT NOT NULL, target_commit TEXT,
               status TEXT NOT NULL DEFAULT 'pending', decided_by TEXT, decided_at TEXT, reason TEXT,
               expires_at TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
        )
        .execute(&p)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO supervisor_approvals (id, supervisor_id, requester, purpose, scope, status, created_at, updated_at)
             VALUES ('old-1', 'AGM', 'bot', 'rebuild', 'daemon', 'approved', '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')",
        )
        .execute(&p)
        .await
        .unwrap();

        migrate(&p).await.unwrap();
        migrate(&p).await.unwrap(); // 可重入

        let old = approval(&p, "old-1").await.unwrap().expect("舊列還在");
        assert_eq!(old.client_request_id, None, "既有列留白");
        assert_eq!(old.status, "approved");
        // 新的還是能帶 id 進來，而且不會跟留白的舊列撞唯一鍵。
        let a = create_approval(&p, "bot", "rebuild", "daemon", None, None, Some("r-1")).await.unwrap();
        assert!(a.created);
        assert!(!create_approval(&p, "bot", "rebuild", "daemon", None, None, Some("r-1")).await.unwrap().created);
        assert_eq!(approvals(&p, 10).await.unwrap().len(), 2);
    }

    /// 「未結案」以前被抄成四份、三種答案：`quota_blocked` 從 ownership 衝突與未結案計數裡消失，
    /// AGM 於是回報「沒有人握著這塊」，把同一個模組派給第二顆 bot（review 2026-09-16）。
    #[tokio::test]
    async fn every_open_state_shows_up_where_ownership_is_checked() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        for (i, st) in OPEN_STATES.iter().enumerate() {
            let a = insert_assignment(&p, None, "b", &format!("crid-{i}"), "做事", &["daemon/src".to_string()], None, true).await.unwrap();
            sqlx::query("UPDATE supervisor_assignments SET status=? WHERE id=?").bind(st).bind(&a.id).execute(&p).await.unwrap();
        }
        assert_eq!(open_assignment_count(&p).await.unwrap(), OPEN_STATES.len() as i64, "未結案計數要涵蓋每一個未結案狀態");
        let unsettled: Vec<String> = unsettled_assignments(&p).await.unwrap().into_iter().map(|a| a.status).collect();
        for st in OPEN_STATES {
            assert!(unsettled.iter().any(|s| s == st), "{st} 不在 ownership 衝突看的清單裡：{unsettled:?}");
        }
        // 「卡住沒人管」是另一張表，而且刻意不含這兩個（都在等一個確定的東西）。
        assert!(!STALLED_STATES.contains(&"quota_blocked"));
        assert!(!STALLED_STATES.contains(&"blocked"));
    }

    /// 前一個持有者過期而不是 release 時，那張「可以」以前還是 approved——同一筆核准可以開好幾個
    /// 重啟窗口，而且決定歷程上看不出來被用過（review 2026-09-16）。
    #[tokio::test]
    async fn taking_over_an_expired_lease_consumes_the_approval_it_rested_on() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let first = create_approval(&p, "bot-a", "restart", "daemon", None, None, None).await.unwrap().approval;
        decide_approval(&p, &first.id, "approved", "AGM", None, None).await.unwrap();
        let past = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        acquire_lease(&p, "restart", "runner-1", Some(&first.id), None, &past, &json!({})).await.unwrap().unwrap();
        assert_eq!(approval(&p, &first.id).await.unwrap().unwrap().status, "approved");

        // 沒有人 release，租約自己過期，下一個人接手。
        let second = create_approval(&p, "bot-a", "restart", "daemon", None, None, None).await.unwrap().approval;
        decide_approval(&p, &second.id, "approved", "AGM", None, None).await.unwrap();
        let later = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        acquire_lease(&p, "restart", "runner-2", Some(&second.id), None, &later, &json!({})).await.unwrap().unwrap();

        assert_eq!(approval(&p, &first.id).await.unwrap().unwrap().status, "consumed", "過期沒 release 的那張要被消耗掉");
        assert_eq!(approval(&p, &second.id).await.unwrap().unwrap().status, "approved", "接手的這張還在用");
        let notes = approval_decisions(&p).await.unwrap();
        assert!(notes.get(&first.id).is_some_and(|v| !v.is_empty()), "消耗要留在決定歷程裡");
    }

    #[tokio::test]
    async fn an_approval_is_decided_once_and_revocable() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let a = create_approval(&p, "bot-a", "rebuild", "daemon/", Some("abc123"), Some("2099-01-01T00:00:00Z"), None)
            .await
            .unwrap()
            .approval;
        assert_eq!(a.status, "pending");
        assert_eq!(a.refusal("2026-09-12T12:00:00Z", "rebuild", Some("abc123")), Some("approval_not_decided"));
        let a = decide_approval(&p, &a.id, "approved", "AGM", Some("沒有人在跑"), None).await.unwrap().unwrap();
        assert_eq!(a.refusal("2026-09-12T12:00:00Z", "rebuild", Some("abc123")), None);
        assert_eq!(a.decided_by.as_deref(), Some("AGM"));
        let a = decide_approval(&p, &a.id, "revoked", "AGM", Some("使用者開始新回合"), None).await.unwrap().unwrap();
        assert_eq!(a.refusal("2026-09-12T12:00:00Z", "rebuild", Some("abc123")), Some("approval_revoked"));
        assert_eq!(approvals(&p, 10).await.unwrap().len(), 1);
    }

    /// The downgrade the review found: an old binary running `setup` writing its compiled-in
    /// persona back over one somebody had just updated. Seeding only fills an empty slot.
    #[tokio::test]
    async fn a_stored_persona_is_never_overwritten_by_the_embedded_default() {
        let p = pool().await;
        get_or_init(&p).await.unwrap();
        let s = get_or_init(&p).await.unwrap();
        assert_eq!((s.persona_version, s.persona_text.clone()), (0, None), "a fresh install has none");

        // First install: the embedded text seeds it.
        assert!(seed_persona_if_empty(&p, "內嵌版 v1").await.unwrap());
        let s = get_or_init(&p).await.unwrap();
        assert_eq!(s.persona_text.as_deref(), Some("內嵌版 v1"));
        assert_eq!(s.persona_version, 1);
        assert_eq!(s.persona_source.as_deref(), Some("embedded"));
        assert_eq!(s.persona_seed_hash, s.persona_hash, "it remembers which embedded text it came from");

        // Somebody updates it through the API.
        assert_eq!(set_persona(&p, "使用者改過的 v2", "api", None).await.unwrap(), 2);

        // An older binary runs `setup` again: it must not win.
        assert!(!seed_persona_if_empty(&p, "內嵌版 v1").await.unwrap(), "seeding is refused once there is a persona");
        let s = get_or_init(&p).await.unwrap();
        assert_eq!(s.persona_text.as_deref(), Some("使用者改過的 v2"));
        assert_eq!(s.persona_version, 2, "and the version does not move");
        assert_eq!(s.persona_source.as_deref(), Some("api"));
        assert_eq!(s.persona_hash.as_deref(), Some(super::super::persona::hash("使用者改過的 v2").as_str()));
        // The seed hash still points at the embedded text it started from, which is what makes
        // "there is a newer embedded version" a different question from "this was customised".
        assert_eq!(s.persona_seed_hash.as_deref(), Some(super::super::persona::hash("內嵌版 v1").as_str()));

        // An explicit migration is the one way the embedded text takes over.
        let v = set_persona(&p, "內嵌版 v3", "embedded", Some(&super::super::persona::hash("內嵌版 v3"))).await.unwrap();
        let s = get_or_init(&p).await.unwrap();
        assert_eq!((v, s.persona_text.as_deref()), (3, Some("內嵌版 v3")));
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

#[cfg(test)]
mod persona_migration_tests {
    use super::*;

    #[tokio::test]
    async fn first_upgrade_preserves_custom_bot_persona_and_embedded_baseline() {
        let p = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        migrate(&p).await.unwrap();
        get_or_init(&p).await.unwrap();
        assert!(seed_persona_from(&p, "custom newer text", "legacy_bot", "old binary text").await.unwrap());
        let row = get_or_init(&p).await.unwrap();
        assert_eq!(row.persona_text.as_deref(), Some("custom newer text"));
        assert_eq!(row.persona_source.as_deref(), Some("legacy_bot"));
        assert_eq!(row.persona_seed_hash, Some(super::super::persona::hash("old binary text")));
        assert!(!seed_persona_if_empty(&p, "another binary").await.unwrap());
        assert!(!seed_persona_from(&p, "outdated bot projection", "legacy_bot", "old binary").await.unwrap());
        assert_eq!(get_or_init(&p).await.unwrap().persona_text.as_deref(), Some("custom newer text"));
    }
}
