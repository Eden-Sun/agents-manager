//! 純資料庫函式與公用型別：supervisor inbox 事件寫入、交辦讀取（SPEC §18.2、§18.3）。
//!
//! 從 `supervisor::store` 拆出至下層模組，切斷下層對上層 `supervisor` 的反向引用。

use anyhow::Result;
use serde_json::Value;
use sqlx::{FromRow, SqlitePool};

/// There is exactly one supervisor, and its id is stable across restarts and model switches.
pub const SUPERVISOR_ID: &str = "AGM";

/// Lifecycle states an assignment can still move out of on its own.
pub const EXECUTING_STATES: [&str; 3] = ["queued", "delivered", "unknown"];

/// Everything AGM still owes attention to: in flight, waiting to be accepted, or blocked.
/// `quota_blocked` 也在裡面——工作還沒做完，只是在等額度回來；controller 會自己重送。
pub const OPEN_STATES: [&str; 6] =
    ["queued", "delivered", "unknown", "awaiting_review", "blocked", "quota_blocked"];

/// 「卡住了沒人管」只看這些：`quota_blocked` 在等一個已知的時間點（SPEC §18.9 明講不算卡住），
/// `blocked` 在等人回答——兩個都不是沒人管，所以刻意不進這張表。其餘的未結案都算。
pub const STALLED_STATES: [&str; 4] = ["queued", "delivered", "unknown", "awaiting_review"];

/// `'a','b',…` 給 SQL 的 `IN (…)` 用。清單只准有一份。
pub fn sql_list(states: &[&str]) -> String {
    states.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",")
}

/// 內容指紋：FNV-1a 64，取 12 碼十六進位。不能用 `DefaultHasher`——它不保證跨版本穩定，
/// 備份檔名會變，「同一個雜湊只留一份」就失效了。
pub fn short_hash(content: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in content {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
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
    /// 排不進去幾輪（409）。跟 `attempts` 分開，見 SCHEMA 的欄位註解與 issue #528。
    #[sqlx(default)]
    pub busy_rounds: i64,
}

impl Assignment {
    /// The wire shape the front end and the `agm` CLI agreed on.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
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
            "busy_rounds": self.busy_rounds,
            "next_attempt_at": self.next_attempt_at,
            "request_id": self.request_id,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "completed_at": self.completed_at,
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
            "legacy_closed": self.legacy_closed != 0,
            "ownership": self.ownership(),
            "review_role": self.review_role.as_deref().unwrap_or("responder"),
            "kind": if self.is_notice() { "notice" } else { "task" },
            "expects_review": !self.is_notice(),
            "resume_at": self.resume_at,
            "quota_retries": self.quota_retries,
            "mission_id": self.mission_id,
            "role": self.mission_role,
            "turn_error": self.turn_error,
            "conflict_since": self.conflict_since,
        })
    }

    /// 通知型（不等回覆、不驗收）。
    pub fn is_notice(&self) -> bool {
        self.expects_review == 0
    }

    /// 這一次要送出去用的 `client_request_id`。
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

/// Everything still owed to AGM: in flight, waiting for acceptance, or blocked. This is the
/// list the handoff, the open count and the dispatch UI read — a job whose turn happens to have
/// ended is still on it until somebody accepts it.
pub async fn unsettled_assignments(pool: &SqlitePool) -> Result<Vec<Assignment>> {
    Ok(sqlx::query_as::<_, Assignment>(&format!(
        "SELECT * FROM supervisor_assignments WHERE supervisor_id=?
           AND status IN ({})
          ORDER BY created_at ASC, rowid ASC",
        sql_list(&OPEN_STATES)
    ))
    .bind(SUPERVISOR_ID)
    .fetch_all(pool)
    .await?)
}

/// Insert an event unless its `event_key` is already known. `Ok(None)` = a duplicate, which is
/// the normal outcome for a replayed turn event or a restart rescan.
pub async fn push_inbox_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
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
    .execute(&mut **tx)
    .await?;
    Ok((res.rows_affected() > 0).then_some(id))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_hash_stability() {
        let h1 = short_hash(b"hello world");
        let h2 = short_hash(b"hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 12);
        assert_ne!(h1, short_hash(b"hello world!"));
    }

    #[test]
    fn test_sql_list() {
        assert_eq!(sql_list(&["a", "b", "c"]), "'a','b','c'");
        assert_eq!(sql_list(&[]), "");
    }
}
