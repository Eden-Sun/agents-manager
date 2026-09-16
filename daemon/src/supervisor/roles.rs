//! AGM 的兩個角色：巡檢（patrol）與協調（responder）。docs/SPEC.md §18.15。
//!
//! 使用者 2026-09-13：「主動找問題用 fable-low（額度不足 opus-low），回應 bots 用 opus-high」。
//! 巡檢就是原本那顆 AGM——`supervisors.bot_id`、使用者入口、Remote Control 全部不動；協調是
//! 第二顆 bot，只接 bot 的申請、交辦回報、核准請求與任務事件。
//!
//! 誰收哪一件事**由 daemon 依事件種類決定**（[`route`]），不問模型、不看名字：
//! 先喚醒巡檢再請它轉交，等於每個 bot 申請都先燒一輪 fable——這張表存在就是為了不讓那件事發生。
//!
//! 協調者還沒建立（舊部署）時一切照舊：協調的事件由巡檢收，節流也照巡檢的。一旦建立了，
//! 協調的事件就**只**給協調者；它沒額度、停了或登出，事件留在 inbox 等，不倒回巡檢。

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Patrol,
    Responder,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Patrol => "patrol",
            Role::Responder => "responder",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s.trim() {
            "patrol" => Some(Role::Patrol),
            "responder" => Some(Role::Responder),
            _ => None,
        }
    }
}

pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS supervisor_roles (
  -- 'patrol' | 'responder'。巡檢的 bot／模型／遠端入口仍在 `supervisors`；這一列只放它的喚醒統計。
  role TEXT PRIMARY KEY,
  bot_id TEXT,
  project_id TEXT,
  cwd TEXT,
  identity TEXT NOT NULL DEFAULT 'cc0',
  model TEXT NOT NULL DEFAULT 'opus',
  effort TEXT NOT NULL DEFAULT 'high',
  desired_running INTEGER NOT NULL DEFAULT 0,
  -- '' | 'waiting_quota'：黏著的覆寫，跟巡檢的 `supervisors.status` 同義。
  status TEXT NOT NULL DEFAULT '',
  status_detail TEXT,
  quota_reset_at TEXT,
  watchdog_attempts INTEGER NOT NULL DEFAULT 0,
  watchdog_next_at TEXT,
  watchdog_gave_up_at TEXT,
  watchdog_last_error TEXT,
  last_notify_at TEXT,
  -- 額度或送不出去時，下一次可以再試的時間（有界退避）。
  notify_next_at TEXT,
  last_wake_at TEXT,
  last_wake_reason TEXT,
  wakes INTEGER NOT NULL DEFAULT 0,
  events_delivered INTEGER NOT NULL DEFAULT 0,
  duplicates INTEGER NOT NULL DEFAULT 0,
  merged INTEGER NOT NULL DEFAULT 0,
  persona_text TEXT,
  persona_version INTEGER NOT NULL DEFAULT 0,
  persona_hash TEXT,
  persona_source TEXT,
  persona_updated_at TEXT,
  persona_seed_hash TEXT,
  -- 進入 `waiting_quota` 的時間。只有**這之後**答完的回合才能拿來證明額度回來了。
  waiting_since TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS supervisor_inbox_role_open
  ON supervisor_inbox(role, state) WHERE state != 'handled';
"#;

/// 從 `store::migrate` 最後呼叫。可重入：每一步都有 `IF NOT EXISTS` 或欄位檢查。
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    for (col, ddl) in [
        // NULL = 還沒分類（剛寫進來、或這個欄位出現以前的舊列）；controller 每個 tick 先補上。
        ("role", "ALTER TABLE supervisor_inbox ADD COLUMN role TEXT"),
        // 0 = 只記錄、不喚醒：回覆、ack、純通知、恢復。會跟下一次喚醒一起送，但自己不叫醒誰。
        ("wake", "ALTER TABLE supervisor_inbox ADD COLUMN wake INTEGER"),
        ("acked_by", "ALTER TABLE supervisor_inbox ADD COLUMN acked_by TEXT"),
        // 被合併掉的事件指向留下來的那一筆（例如同一段時間的多次 health_changed）。
        ("merged_into", "ALTER TABLE supervisor_inbox ADD COLUMN merged_into TEXT"),
    ] {
        if !super::store::has_column(pool, "supervisor_inbox", col).await? {
            sqlx::query(ddl).execute(pool).await?;
        }
    }
    // `claimed_by`：實際送給哪個角色（送出那一刻寫下）。ack 以它為準，兩個角色不會各收一次。
    // 加欄與回填**同一個 transaction**：分兩步的話，加欄成功、回填失敗之後重啟會看到欄位已經在，
    // 回填就永遠被跳過，舊巡檢收過的事件從此被當成沒人收過。
    if !super::store::has_column(pool, "supervisor_inbox", "claimed_by").await? {
        let mut tx = pool.begin().await?;
        sqlx::query("ALTER TABLE supervisor_inbox ADD COLUMN claimed_by TEXT").execute(&mut *tx).await?;
        let n = sqlx::query(&format!("UPDATE supervisor_inbox SET claimed_by='patrol' WHERE claimed_by IS NULL AND {SENT_EVIDENCE}"))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        if n > 0 {
            tracing::info!(events = n, "dual-role migration: events the single AGM already received stay with patrol");
        }
    }
    repair_unsent_patrol_claims(pool).await?;
    if !super::store::has_column(pool, "supervisor_assignments", "review_role").await? {
        // 回報給誰驗收。NULL = 協調者（巡檢自己的例行派工會明寫 patrol）。
        sqlx::query("ALTER TABLE supervisor_assignments ADD COLUMN review_role TEXT").execute(pool).await?;
    }
    for stmt in DDL.split(";\n") {
        let s = stmt.trim();
        if !s.is_empty() {
            sqlx::query(s).execute(pool).await?;
        }
    }
    if !super::store::has_column(pool, "supervisor_roles", "waiting_since").await? {
        sqlx::query("ALTER TABLE supervisor_roles ADD COLUMN waiting_since TEXT").execute(pool).await?;
    }
    Ok(())
}

/// 雙角色以前的事件「真的送出去過」的持久證據。
///
/// 升級前只有一顆 AGM（＝現在的巡檢），它收過的事件不會因為換了版本就變成協調者的：
/// `approval_requested`、交辦回報這些種類照新路由表會被分到協調者，舊庫裡「已經送給巡檢、還沒 ack」
/// 的那些（包含通知回合失敗被 recover 成 pending 的）若被協調者再送一次，claim 守衛又不讓它寫成
/// delivered——同一則事件每個 tick 送一次。
///
/// 只認**送達**留下的痕跡：`notify_turn_id`（`requeue_inbox` 特地保留它，recover 之後仍看得見）、
/// `delivered_at`、`state='delivered'`。`notify_attempts` **不算**——`defer_notify` 在完全送不出去、
/// 連回合都沒有時也會加一，把那些算成「巡檢收過」會讓它們繼續燒 fable。
const SENT_EVIDENCE: &str = "(notify_turn_id IS NOT NULL OR delivered_at IS NOT NULL OR state='delivered')";

/// 修正先前版本的回填把「只有嘗試次數、沒送出去」的 pending 事件誤記給巡檢。
///
/// 可重入、不需要記號：`roles::mark_delivered` 寫 `claimed_by` 時**一定**同時寫 `notify_turn_id` 與
/// `delivered_at`，所以「還在 pending、有 `claimed_by='patrol'`、卻完全沒有送達痕跡、也沒人 ack」的列
/// 只可能出自那次回填。放回 `NULL` 交給路由表重新決定；已被任一角色真正收走或結案的工作一律不碰。
async fn repair_unsent_patrol_claims(pool: &SqlitePool) -> Result<()> {
    let n = sqlx::query(&format!(
        "UPDATE supervisor_inbox SET claimed_by=NULL
          WHERE claimed_by='patrol' AND state='pending' AND acked_by IS NULL AND NOT {SENT_EVIDENCE}"
    ))
    .execute(pool)
    .await?
    .rows_affected();
    if n > 0 {
        tracing::warn!(events = n, "released pending events that were mis-claimed for patrol without any delivery evidence");
    }
    Ok(())
}

// ---------------------------------------------------------------- routing table

/// 一個事件歸誰、要不要叫醒人。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub role: Role,
    pub wake: bool,
}

/// 路由表（SPEC §18.15）。純函式：只看事件種類、payload 裡明寫的欄位，以及交辦記錄的驗收角色。
///
/// 不認得的種類給巡檢並喚醒：新事件寧可被看到一次，也不要無聲地躺在沒人收的角色底下。
pub fn route(kind: &str, payload: &Value, review_role: Option<&str>) -> Route {
    known_route(kind, payload, review_role).unwrap_or(Route { role: Role::Patrol, wake: true })
}

/// 表上**明寫**的分支；`None` ＝ 落到預設。拆出來是為了測得到「每一種寫進 inbox 的 kind 都有自己的分支」：
/// 表上寫 `quota_blocked`、寫入端寫 `assignment_quota_blocked`，兩年都不會有人發現——落到預設
/// 只是多叫醒巡檢一次，不會壞得很大聲（review 2026-09-16）。
fn known_route(kind: &str, payload: &Value, review_role: Option<&str>) -> Option<Route> {
    let needs_review = payload.get("needs_review").and_then(Value::as_bool);
    let reviewer = review_role.and_then(Role::parse).unwrap_or(Role::Responder);
    let r = |role, wake| Some(Route { role, wake });
    match kind {
        // bot 的申請：寫入時已決定收件角色（`bot_requests::intercept`）。
        "bot_request" => r(
            payload.get("to_role").and_then(Value::as_str).and_then(Role::parse).unwrap_or(Role::Responder),
            payload.get("wake").and_then(Value::as_bool).unwrap_or(true),
        ),
        "assignment_completed" | "assignment_failed" => r(reviewer, needs_review != Some(false)),
        // 送不進去、停在 blocked：要驗收角色決定改派、followup 或放掉（controller::undeliverable）。
        "assignment_undeliverable" => r(reviewer, true),
        // 通知型交辦送到了、額度擋住／恢復：controller 自己會處理，這些只是記錄。
        // 排隊中（`assignment_queued`）同理：它還在路上，等回合結束自己會送出。
        "assignment_noticed" | "assignment_queued" | "assignment_quota_blocked" | "assignment_quota_resumed" => r(reviewer, false),
        "approval_requested" | "mission_created" | "mission_question" | "mission_answered" | "mission_resumed"
        | "mission_identity_switch" | "mission_paused" | "mission_cancelled" => r(Role::Responder, true),
        // 倒下的就是巡檢自己：它的看門狗放棄、或它的通知一直送不出去（`notify_exhausted`）。送給巡檢等於
        // 送進已知壞掉的那條路——活著的協調者才收得到（review 2026-09-16 c1 M2、L4）。反方向對稱：
        // 協調者倒了是 `responder_watchdog_gave_up` 給巡檢。協調者沒建立時巡檢的 `due_for` 照樣撈得到。
        "watchdog_gave_up" => r(Role::Responder, true),
        "incident_opened" | "incident_resolved" if payload.pointer("/incident/kind").and_then(Value::as_str) == Some("notify_exhausted") => {
            r(Role::Responder, kind == "incident_opened")
        }
        // 系統層的故障：巡檢收、叫醒。
        "incident_opened" | "responder_watchdog_gave_up" | "responder_bot_missing" | "bot_restart_failed"
        | "supervisor_restart_retry" | "agm_cli_stale" | "pane_unowned" | "pane_orphaned" => r(Role::Patrol, true),
        // 恢復不叫醒人：開的那一筆已經叫過，關掉只要記下來。
        "incident_resolved" => r(Role::Patrol, false),
        // 協調者那一半也算：它倒了或在等額度，能發現的只有巡檢（review 2026-09-16 #7）。
        // 舊 payload 沒有 `responder_health` 就當 healthy。
        "health_changed" => {
            let status = payload.pointer("/manager_health/status").and_then(Value::as_str).unwrap_or("unknown");
            let responder = payload.pointer("/responder_health/status").and_then(Value::as_str).unwrap_or("healthy");
            r(Role::Patrol, status != "healthy" || responder != "healthy")
        }
        _ => None,
    }
}

/// 把還沒分類的 inbox 列補上角色與喚醒旗標。寫入端（store、mission、bulk_restart）都不用改，
/// 路由只存在這一個地方。
pub async fn classify(pool: &SqlitePool) -> Result<usize> {
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT i.id, i.kind, i.payload_json, a.review_role
           FROM supervisor_inbox i LEFT JOIN supervisor_assignments a ON a.id = i.assignment_id
          WHERE i.role IS NULL",
    )
    .fetch_all(pool)
    .await?;
    for (id, kind, payload, review_role) in &rows {
        let p: Value = serde_json::from_str(payload).unwrap_or_else(|_| json!({}));
        let rt = route(kind, &p, review_role.as_deref());
        sqlx::query("UPDATE supervisor_inbox SET role=?, wake=? WHERE id=? AND role IS NULL")
            .bind(rt.role.as_str())
            .bind(i64::from(rt.wake))
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(rows.len())
}

// ---------------------------------------------------------------- role rows

#[allow(dead_code)]
#[derive(Debug, Clone, FromRow)]
pub struct RoleRow {
    pub role: String,
    pub bot_id: Option<String>,
    pub project_id: Option<String>,
    pub cwd: Option<String>,
    pub identity: String,
    pub model: String,
    pub effort: String,
    pub desired_running: i64,
    pub status: String,
    pub status_detail: Option<String>,
    pub quota_reset_at: Option<String>,
    pub watchdog_attempts: i64,
    pub watchdog_next_at: Option<String>,
    pub watchdog_gave_up_at: Option<String>,
    pub watchdog_last_error: Option<String>,
    pub last_notify_at: Option<String>,
    pub notify_next_at: Option<String>,
    pub last_wake_at: Option<String>,
    pub last_wake_reason: Option<String>,
    pub wakes: i64,
    pub events_delivered: i64,
    pub duplicates: i64,
    pub merged: i64,
    pub persona_text: Option<String>,
    pub persona_version: i64,
    pub persona_hash: Option<String>,
    pub persona_source: Option<String>,
    pub persona_updated_at: Option<String>,
    pub persona_seed_hash: Option<String>,
    #[sqlx(default)]
    pub waiting_since: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl RoleRow {
    pub fn stats_json(&self) -> Value {
        json!({
            "wakes": self.wakes,
            "events_delivered": self.events_delivered,
            "duplicates": self.duplicates,
            "merged": self.merged,
            "last_wake_at": self.last_wake_at,
            "last_wake_reason": self.last_wake_reason,
            "last_notify_at": self.last_notify_at,
            "notify_next_at": self.notify_next_at,
        })
    }
}

pub async fn get(pool: &SqlitePool, role: Role) -> Result<RoleRow> {
    let now = crate::db::now();
    sqlx::query("INSERT OR IGNORE INTO supervisor_roles (role, created_at, updated_at) VALUES (?, ?, ?)")
        .bind(role.as_str())
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
    Ok(sqlx::query_as::<_, RoleRow>("SELECT * FROM supervisor_roles WHERE role=?")
        .bind(role.as_str())
        .fetch_one(pool)
        .await?)
}

pub async fn set_env(pool: &SqlitePool, role: Role, bot_id: &str, project_id: &str, cwd: &str) -> Result<()> {
    get(pool, role).await?;
    sqlx::query("UPDATE supervisor_roles SET bot_id=?, project_id=?, cwd=?, updated_at=? WHERE role=?")
        .bind(bot_id)
        .bind(project_id)
        .bind(cwd)
        .bind(crate::db::now())
        .bind(role.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_runtime(pool: &SqlitePool, role: Role, identity: &str, model: &str, effort: &str) -> Result<()> {
    get(pool, role).await?;
    sqlx::query("UPDATE supervisor_roles SET identity=?, model=?, effort=?, updated_at=? WHERE role=?")
        .bind(identity)
        .bind(model)
        .bind(effort)
        .bind(crate::db::now())
        .bind(role.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_desired_running(pool: &SqlitePool, role: Role, wanted: bool) -> Result<()> {
    get(pool, role).await?;
    // 人手啟動就是新的一輪：前一次 watchdog 放棄的紀錄與計數一起清掉。
    sqlx::query(
        "UPDATE supervisor_roles SET desired_running=?, watchdog_attempts=0, watchdog_next_at=NULL,
                watchdog_gave_up_at=NULL, watchdog_last_error=NULL, updated_at=? WHERE role=?",
    )
    .bind(i64::from(wanted))
    .bind(crate::db::now())
    .bind(role.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_status(pool: &SqlitePool, role: Role, status: &str, detail: Option<&str>, reset_at: Option<&str>) -> Result<()> {
    let now = crate::db::now();
    // `waiting_since` 只在**進入**等待時記一次，之後刷新 detail／reset 不改它；離開等待就清掉。
    sqlx::query(
        "UPDATE supervisor_roles
            SET status=?, status_detail=?, quota_reset_at=?, updated_at=?,
                waiting_since = CASE WHEN ?='waiting_quota' THEN COALESCE(waiting_since, ?) ELSE NULL END
          WHERE role=?",
    )
    .bind(status)
    .bind(detail)
    .bind(reset_at)
    .bind(&now)
    .bind(status)
    .bind(&now)
    .bind(role.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_status_detail(pool: &SqlitePool, role: Role, detail: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE supervisor_roles SET status_detail=?, updated_at=? WHERE role=?")
        .bind(detail)
        .bind(crate::db::now())
        .bind(role.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_watchdog(pool: &SqlitePool, role: Role, attempts: i64, next_at: Option<&str>, error: Option<&str>) -> Result<()> {
    let clear_gave_up = attempts == 0 && next_at.is_none();
    sqlx::query(
        "UPDATE supervisor_roles SET watchdog_attempts=?, watchdog_next_at=?,
                watchdog_last_error=COALESCE(?, CASE WHEN ? THEN NULL ELSE watchdog_last_error END),
                watchdog_gave_up_at=CASE WHEN ? THEN NULL ELSE watchdog_gave_up_at END, updated_at=?
          WHERE role=?",
    )
    .bind(attempts)
    .bind(next_at)
    .bind(error)
    .bind(clear_gave_up)
    .bind(clear_gave_up)
    .bind(crate::db::now())
    .bind(role.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

/// `true` 只在第一次：放棄只報一次。
pub async fn mark_watchdog_gave_up(pool: &SqlitePool, role: Role, why: &str) -> Result<Option<String>> {
    let now = crate::db::now();
    let res = sqlx::query(
        "UPDATE supervisor_roles SET watchdog_gave_up_at=?, watchdog_last_error=?, updated_at=?
          WHERE role=? AND watchdog_gave_up_at IS NULL",
    )
    .bind(&now)
    .bind(why)
    .bind(&now)
    .bind(role.as_str())
    .execute(pool)
    .await?;
    Ok((res.rows_affected() > 0).then_some(now))
}

pub async fn set_notify_next(pool: &SqlitePool, role: Role, next_at: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE supervisor_roles SET notify_next_at=?, updated_at=? WHERE role=?")
        .bind(next_at)
        .bind(crate::db::now())
        .bind(role.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

/// 一次真的送出去的喚醒。計數是驗收「fable 不會因為 bot 申請被叫醒」的證據。
pub async fn record_wake(pool: &SqlitePool, role: Role, events: usize, reason: &str) -> Result<()> {
    get(pool, role).await?;
    let now = crate::db::now();
    sqlx::query(
        "UPDATE supervisor_roles SET wakes=wakes+1, events_delivered=events_delivered+?, last_wake_at=?,
                last_wake_reason=?, last_notify_at=?, notify_next_at=NULL, updated_at=? WHERE role=?",
    )
    .bind(events as i64)
    .bind(&now)
    .bind(reason)
    .bind(&now)
    .bind(&now)
    .bind(role.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn count_duplicate(pool: &SqlitePool, role: Role) -> Result<()> {
    get(pool, role).await?;
    sqlx::query("UPDATE supervisor_roles SET duplicates=duplicates+1, updated_at=? WHERE role=?")
        .bind(crate::db::now())
        .bind(role.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

async fn count_merged(pool: &SqlitePool, role: Role, n: usize) -> Result<()> {
    if n == 0 {
        return Ok(());
    }
    get(pool, role).await?;
    sqlx::query("UPDATE supervisor_roles SET merged=merged+?, updated_at=? WHERE role=?")
        .bind(n as i64)
        .bind(crate::db::now())
        .bind(role.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

/// 協調者**建立過**沒有。這跟「它現在活著」是兩件事：一旦雙角色啟用，協調的事件就永遠是
/// 協調的——它停了、沒額度、bot 被刪掉，事件都留在 inbox 等，不會倒回巡檢（那等於又去燒 fable）。
pub async fn responder_configured(pool: &SqlitePool) -> Result<bool> {
    Ok(get(pool, Role::Responder).await?.bot_id.is_some())
}

/// 協調者的 bot，且那顆 bot 還在（使用者可能刪掉它）。`None` **不代表**回到單角色——
/// 那要看 [`responder_configured`]。
pub async fn responder_bot(pool: &SqlitePool) -> Result<Option<crate::db::Bot>> {
    let row = get(pool, Role::Responder).await?;
    let Some(id) = row.bot_id else { return Ok(None) };
    Ok(crate::db::bot(pool, &id).await?.filter(|b| b.deleted_at.is_none()))
}

/// 這顆 bot 是哪個角色；兩個都不是就 `None`。
pub async fn role_of_bot(pool: &SqlitePool, bot_id: &str) -> Result<Option<Role>> {
    let sup = super::store::get_or_init(pool).await?;
    if sup.bot_id.as_deref() == Some(bot_id) {
        return Ok(Some(Role::Patrol));
    }
    let r = get(pool, Role::Responder).await?;
    Ok((r.bot_id.as_deref() == Some(bot_id)).then_some(Role::Responder))
}

/// 角色**登記**的 bot id（不管那顆 bot 現在在不在）。路由與 claim 都認這個；要送信才需要
/// [`responder_bot`] 那種「還活著」的版本。
pub async fn bot_for(pool: &SqlitePool, role: Role) -> Result<Option<String>> {
    Ok(match role {
        Role::Patrol => super::store::get_or_init(pool).await?.bot_id,
        Role::Responder => get(pool, Role::Responder).await?.bot_id,
    })
}

// ---------------------------------------------------------------- per-role inbox

/// 一筆事件「歸誰」的唯一定義：送出去之後看 `claimed_by`，還沒送就看路由表寫的 `role`。
///
/// 兩個角色的查詢、ack 的守衛與 UI 的過濾全部用這一條，否則會出現「A 查得到、B 才寫得進去」的
/// 交錯：雙角色剛啟用時，先前由巡檢收走（`claimed_by='patrol'`）的協調事件會被 recover 放回
/// pending，若只看 `role` 就會被協調者撈去送，而 `mark_delivered` 的 claim 守衛又不讓它寫成
/// delivered——同一則事件每個 tick 送一次，兩邊都以為是自己的。
const OWNER: &str = "COALESCE(claimed_by, role)";

/// 這個角色這一輪可以送的事件（最舊在前）。
///
/// * 巡檢：自己擁有的；協調者**還沒建立**時（舊部署）連協調的一起收。
/// * 協調：只有自己擁有的。沒有重試上限——送不出去是有界退避（見 `responder::notify`），
///   事件一直留著。巡檢的事件照舊有 `max_attempts`。
pub async fn due_for(
    pool: &SqlitePool,
    role: Role,
    responder_configured: bool,
    now: &str,
    max_attempts: i64,
) -> Result<Vec<super::store::InboxEvent>> {
    let (owned, cap) = match (role, responder_configured) {
        (Role::Patrol, true) => (format!("{OWNER}='patrol'"), "notify_attempts < ?1".to_string()),
        (Role::Patrol, false) => (
            format!("{OWNER} IN ('patrol','responder')"),
            format!("({OWNER}='responder' OR notify_attempts < ?1)"),
        ),
        (Role::Responder, _) => (format!("{OWNER}='responder'"), "?1 = ?1".to_string()),
    };
    // 編號參數：三種組合用同一組 bind，不必各自對齊順序。
    let sql = format!(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=?2 AND state='pending' AND {owned}
           AND (notify_next_at IS NULL OR notify_next_at <= ?3) AND {cap}
         ORDER BY created_at ASC, id ASC"
    );
    Ok(sqlx::query_as::<_, super::store::InboxEvent>(&sql)
        .bind(max_attempts)
        .bind(super::store::SUPERVISOR_ID)
        .bind(now)
        .fetch_all(pool)
        .await?)
}

/// `GET /api/supervisor/inbox` 的清單。角色條件在 **SQL 的 LIMIT 之前**：先取 200 筆再過濾的話，
/// 最舊的 200 筆全是另一個角色時，自己的待辦永遠翻不到。
pub async fn list_for(pool: &SqlitePool, role: Option<Role>, all: bool, limit: i64) -> Result<Vec<super::store::InboxEvent>> {
    let owned = match role {
        Some(r) => format!("AND {OWNER}='{}'", r.as_str()),
        None => String::new(),
    };
    // 稽核視圖（`all`）最新在前；工作視圖只列沒結案的、最舊在前，照順序 ack 才清得掉。
    let (state, order) = if all { ("", "created_at DESC, id DESC") } else { ("AND state!='handled'", "created_at ASC, id ASC") };
    let sql = format!(
        "SELECT * FROM supervisor_inbox WHERE supervisor_id=? {state} {owned} ORDER BY {order} LIMIT ?"
    );
    Ok(sqlx::query_as::<_, super::store::InboxEvent>(&sql)
        .bind(super::store::SUPERVISOR_ID)
        .bind(limit)
        .fetch_all(pool)
        .await?)
}

/// 標記送達，並寫下是哪個角色收的。`claimed_by` 已經是別的角色的列不動——同一件事只能有一個
/// 角色收，重試或兩條迴圈交錯都不會變成兩個角色各處理一次。
pub async fn mark_delivered(pool: &SqlitePool, ids: &[String], role: Role, turn_id: &str, delivery: &str) -> Result<usize> {
    let now = crate::db::now();
    let mut n = 0;
    for id in ids {
        let res = sqlx::query(
            "UPDATE supervisor_inbox
                SET state='delivered', claimed_by=?, notify_turn_id=?, notify_delivery=?, delivered_at=?,
                    notify_attempts=notify_attempts+1, notify_next_at=NULL, notify_error=NULL, updated_at=?
              WHERE id=? AND state!='handled' AND (claimed_by IS NULL OR claimed_by=?)",
        )
        .bind(role.as_str())
        .bind(turn_id)
        .bind(delivery)
        .bind(&now)
        .bind(&now)
        .bind(id)
        .bind(role.as_str())
        .execute(pool)
        .await?;
        n += res.rows_affected() as usize;
    }
    Ok(n)
}

#[derive(Debug, PartialEq, Eq)]
pub enum AckOutcome {
    Acked,
    AlreadyHandled,
    NotFound,
    /// 這件事是另一個角色收的；ack 它等於替別人結案。
    ClaimedByOther(String),
}

/// 結案一則事件。`actor` = 驗證過的角色（bot token）；`None` = UI／使用者，什麼都能結。
///
/// 協調者還沒建立時，巡檢可以結協調的事件（因為那時就是它在收）。
pub async fn ack(pool: &SqlitePool, id: &str, actor: Option<Role>, responder_configured: bool) -> Result<AckOutcome> {
    // 守衛跟寫入在同一句 SQL：先 SELECT 再 UPDATE 的話，兩者之間送出的那一次 `mark_delivered`
    // 會把 claim 換成另一個角色，而這一句照樣寫下去——等於跨角色結了別人的案。
    let allowed = match actor {
        None => "1=1".to_string(),
        // 協調者還沒建立時，協調的事件就是巡檢在收，所以巡檢結得了。
        Some(Role::Patrol) if !responder_configured => format!("{OWNER} IN ('patrol','responder')"),
        Some(r) => format!("{OWNER}='{}'", r.as_str()),
    };
    let acked_by = actor.map(Role::as_str).unwrap_or("user");
    let sql = format!(
        "UPDATE supervisor_inbox SET state='handled', acked_by=?, updated_at=?
          WHERE supervisor_id=? AND id=? AND state!='handled' AND {allowed}"
    );
    let res = sqlx::query(&sql)
        .bind(acked_by)
        .bind(crate::db::now())
        .bind(super::store::SUPERVISOR_ID)
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() > 0 {
        return Ok(AckOutcome::Acked);
    }
    // 沒寫到：說得出是哪一種——不存在、已經結過，還是別的角色的。
    let row: Option<(String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT state, role, claimed_by FROM supervisor_inbox WHERE supervisor_id=? AND id=?")
            .bind(super::store::SUPERVISOR_ID)
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let Some((state, role, claimed)) = row else { return Ok(AckOutcome::NotFound) };
    if state == "handled" {
        return Ok(AckOutcome::AlreadyHandled);
    }
    Ok(AckOutcome::ClaimedByOther(claimed.or(role).unwrap_or_else(|| "patrol".into())))
}

/// 巡檢的合併去重（今天 fable 用量最大的來源）。在送之前、每個 tick 跑一次：
///
/// 1. 還沒送出的 `health_changed` 只留最新一筆——十分鐘內 degraded→healthy→degraded 是一件事，
///    讀的人只需要現在的狀態。
/// 2. 同一個 incident 開了又在送出前就恢復：兩筆一起結案，不叫醒任何人（抖動）。
///
/// 被合併的列 `state='handled'`、`acked_by='daemon'`、`merged_into` 指向留下來的那筆，稽核看得到。
pub async fn coalesce_patrol(pool: &SqlitePool) -> Result<usize> {
    let now = crate::db::now();
    let mut merged = 0usize;
    let health: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM supervisor_inbox WHERE state='pending' AND kind='health_changed'
          ORDER BY created_at DESC, id DESC",
    )
    .fetch_all(pool)
    .await?;
    if let Some(((keep,), older)) = health.split_first() {
        for (id,) in older {
            merged += sqlx::query(
                "UPDATE supervisor_inbox SET state='handled', acked_by='daemon', merged_into=?, updated_at=?
                  WHERE id=? AND state='pending'",
            )
            .bind(keep)
            .bind(&now)
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected() as usize;
        }
    }
    // `incident:<id>:opened` / `incident:<id>:resolved`（incidents.rs 的 event_key）。
    let flaps: Vec<(String, String)> = sqlx::query_as(
        "SELECT o.id, r.id FROM supervisor_inbox o
           JOIN supervisor_inbox r
             ON r.event_key = substr(o.event_key, 1, length(o.event_key) - length('opened')) || 'resolved'
          WHERE o.kind='incident_opened' AND o.state='pending' AND r.state='pending'",
    )
    .fetch_all(pool)
    .await?;
    for (opened, resolved) in &flaps {
        for id in [opened, resolved] {
            merged += sqlx::query(
                "UPDATE supervisor_inbox SET state='handled', acked_by='daemon', merged_into=?, updated_at=?
                  WHERE id=? AND state='pending'",
            )
            .bind(resolved)
            .bind(&now)
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected() as usize;
        }
    }
    count_merged(pool, Role::Patrol, merged).await?;
    Ok(merged)
}

/// 喚醒原因：這一批裡有哪些種類，給 UI 與稽核看「為什麼叫醒了它」。
pub fn wake_reason(events: &[super::store::InboxEvent]) -> String {
    let mut kinds: Vec<(String, usize)> = Vec::new();
    for e in events.iter().filter(|e| e.wake != Some(0)) {
        match kinds.iter_mut().find(|(k, _)| *k == e.kind) {
            Some((_, n)) => *n += 1,
            None => kinds.push((e.kind.clone(), 1)),
        }
    }
    kinds.iter().map(|(k, n)| if *n > 1 { format!("{k}×{n}") } else { k.clone() }).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        super::super::store::migrate(&p).await.unwrap();
        super::super::store::get_or_init(&p).await.unwrap();
        p
    }

    #[test]
    fn the_routing_table_sends_bot_business_to_the_responder_and_system_faults_to_patrol() {
        let p = json!({});
        for kind in ["approval_requested", "mission_question", "mission_created", "mission_answered", "mission_resumed", "mission_identity_switch", "mission_paused", "mission_cancelled"] {
            assert_eq!(route(kind, &p, None), Route { role: Role::Responder, wake: true }, "{kind}");
        }
        assert_eq!(route("assignment_completed", &json!({"needs_review": true}), None), Route { role: Role::Responder, wake: true });
        assert_eq!(route("assignment_failed", &json!({"needs_review": true}), Some("patrol")), Route { role: Role::Patrol, wake: true }, "巡檢自己的例行派工回到巡檢");
        // 寫入端真正用的名字（store::park_quota_blocked／resume_quota_blocked），不是表上以前寫的 `quota_blocked`。
        for kind in ["assignment_noticed", "assignment_queued", "assignment_quota_blocked", "assignment_quota_resumed"] {
            assert_eq!(route(kind, &p, None), Route { role: Role::Responder, wake: false }, "{kind} 只記錄、歸驗收角色");
            assert_eq!(route(kind, &p, Some("patrol")).role, Role::Patrol, "{kind} 跟著交辦的驗收角色");
        }
        assert_eq!(route("assignment_undeliverable", &p, None), Route { role: Role::Responder, wake: true }, "送不進去要驗收角色決定");
        assert_eq!(route("assignment_undeliverable", &p, Some("patrol")), Route { role: Role::Patrol, wake: true });
        for kind in ["incident_opened", "bot_restart_failed", "supervisor_restart_retry", "responder_watchdog_gave_up", "brand_new_kind"] {
            assert_eq!(route(kind, &p, None), Route { role: Role::Patrol, wake: true }, "{kind}");
        }
        // 巡檢自己倒了：送給巡檢等於沒送。活著的協調者收（review 2026-09-16 c1 M2、L4）。
        assert_eq!(route("watchdog_gave_up", &p, None), Route { role: Role::Responder, wake: true });
        let exhausted = json!({"incident": {"kind": "notify_exhausted"}});
        assert_eq!(route("incident_opened", &exhausted, None), Route { role: Role::Responder, wake: true });
        assert_eq!(route("incident_resolved", &exhausted, None), Route { role: Role::Responder, wake: false });
        assert_eq!(route("incident_opened", &json!({"incident": {"kind": "host_disconnected"}}), None).role, Role::Patrol);
        assert_eq!(route("incident_resolved", &p, None), Route { role: Role::Patrol, wake: false }, "恢復不叫醒人");
        assert!(!route("health_changed", &json!({"manager_health": {"status": "healthy"}}), None).wake);
        assert!(route("health_changed", &json!({"manager_health": {"status": "degraded"}}), None).wake);
        let responder = |s: &str| json!({"manager_health": {"status": "healthy"}, "responder_health": {"status": s}});
        assert_eq!(route("health_changed", &responder("degraded"), None), Route { role: Role::Patrol, wake: true }, "協調者倒了要叫醒巡檢");
        assert!(!route("health_changed", &responder("healthy"), None).wake);
        assert_eq!(
            route("bot_request", &json!({"to_role": "patrol", "wake": false}), None),
            Route { role: Role::Patrol, wake: false }
        );
        assert_eq!(route("bot_request", &json!({}), None), Route { role: Role::Responder, wake: true });
    }

    /// 從原始碼撈出「寫進 supervisor_inbox 的 kind」：`push_inbox(…)`／`push_inbox_tx(…)` 的第三個參數、
    /// `settle_and_notify(…)` 的第八個、`INSERT … supervisor_inbox … VALUES` 裡寫死的字串、
    /// `let kind = if … { "…" } else { "…" }` 的分支，以及 mission 的 `(key, "…", &payload)` 三元組。
    /// 手寫清單正是 09ec464 那個錯的來源，所以清單由寫入端自己長出來。
    fn inbox_kinds_written_in_source() -> std::collections::BTreeMap<String, String> {
        fn args_of(src: &str, open: usize) -> Vec<String> {
            let (mut depth, mut args, mut cur, mut chars) = (0i32, Vec::new(), String::new(), src[open..].chars().peekable());
            while let Some(c) = chars.next() {
                match c {
                    '"' => {
                        cur.push(c);
                        while let Some(s) = chars.next() {
                            cur.push(s);
                            if s == '\\' {
                                if let Some(e) = chars.next() {
                                    cur.push(e);
                                }
                            } else if s == '"' {
                                break;
                            }
                        }
                        continue;
                    }
                    '(' | '[' | '{' => {
                        depth += 1;
                        if depth == 1 {
                            continue;
                        }
                    }
                    ')' | ']' | '}' => {
                        depth -= 1;
                        if depth == 0 {
                            args.push(cur.trim().to_string());
                            return args;
                        }
                    }
                    ',' if depth == 1 => {
                        args.push(std::mem::take(&mut cur).trim().to_string());
                        continue;
                    }
                    _ => {}
                }
                cur.push(c);
            }
            args
        }
        fn is_kind(s: &str) -> bool {
            s.contains('_') && !s.starts_with('_') && s.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        }
        fn literal(arg: &str) -> Option<String> {
            let s = arg.strip_prefix('"')?.strip_suffix('"')?;
            is_kind(s).then(|| s.to_string())
        }
        /// `open` 的前一個字（略過空白）是 `before`、`close` 的後一個字是 `after` 的 `"…"`。
        fn wrapped(src: &str, before: char, after: &str) -> Vec<String> {
            let mut out = Vec::new();
            let parts: Vec<&str> = src.split('"').collect();
            for i in (1..parts.len()).step_by(2) {
                let prev = parts[i - 1].trim_end();
                let next = parts.get(i + 1).map_or("", |n| n.trim_start());
                if prev.ends_with(before) && next.starts_with(after) && is_kind(parts[i]) {
                    out.push(parts[i].to_string());
                }
            }
            out
        }
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
        let mut kinds = std::collections::BTreeMap::new();
        for f in files {
            let src = std::fs::read_to_string(&f).unwrap();
            let at = f.strip_prefix(&root).unwrap().display().to_string();
            let mut add = |k: String| {
                kinds.entry(k).or_insert_with(|| at.clone());
            };
            for (call, idx) in [("push_inbox(", 2), ("push_inbox_tx(", 2), ("settle_and_notify(", 7)] {
                for (pos, _) in src.match_indices(call) {
                    if let Some(k) = args_of(&src, pos + call.len() - 1).get(idx).and_then(|a| literal(a)) {
                        add(k);
                    }
                }
            }
            for (pos, _) in src.match_indices("INTO supervisor_inbox") {
                let tail = &src[pos..];
                let Some(v) = tail.find("VALUES") else { continue };
                let values = &tail[v..tail[v..].find(')').map_or(tail.len(), |e| v + e)];
                for s in values.split('\'').skip(1).step_by(2).filter(|s| is_kind(s)) {
                    add(s.to_string());
                }
            }
            for line in src.lines().filter(|l| l.contains("let kind = ")) {
                for k in wrapped(line, '{', "}") {
                    add(k);
                }
            }
            for k in wrapped(&src, ',', ", &payload)") {
                add(k);
            }
        }
        kinds
    }

    /// 每一種寫進 inbox 的 kind 都要在路由表有**明寫**的分支（review 2026-09-16 新發現 5）。
    #[test]
    fn every_kind_written_to_the_inbox_has_its_own_routing_branch() {
        let kinds = inbox_kinds_written_in_source();
        // 撈取本身要撈得到東西：否則 regex 一壞，這個測試會安靜地變成恆真。
        for must in ["assignment_quota_blocked", "assignment_quota_resumed", "assignment_undeliverable", "assignment_completed", "approval_requested", "bot_request", "incident_opened", "mission_question"] {
            assert!(kinds.contains_key(must), "source scan lost {must}: {kinds:?}");
        }
        // 撈不到的寫法（panes.rs 用 tuple 陣列決定 kind）逐一列在這裡，免得新 kind 又從預設路由溜走。
        for written_via_table in ["pane_orphaned", "pane_unowned"] {
            assert!(known_route(written_via_table, &json!({}), None).is_some(), "{written_via_table} 沒有明寫的分支");
        }
        let unrouted: Vec<_> = kinds.iter().filter(|(k, _)| known_route(k, &json!({}), None).is_none()).collect();
        assert!(unrouted.is_empty(), "these kinds are written to the inbox but fall through to the default route: {unrouted:?}");
    }

    /// 舊資料庫：欄位補上、舊列在下一個 tick 被分類，重跑 migrate 不會壞。
    #[tokio::test]
    async fn migration_is_reentrant_and_old_rows_get_classified() {
        let p = pool().await;
        super::super::store::push_inbox(&p, "k1", "approval_requested", None, None, None, &json!({})).await.unwrap();
        super::super::store::push_inbox(&p, "k2", "incident_resolved", None, None, None, &json!({})).await.unwrap();
        super::super::store::migrate(&p).await.unwrap();
        migrate(&p).await.unwrap();
        assert_eq!(classify(&p).await.unwrap(), 2);
        assert_eq!(classify(&p).await.unwrap(), 0, "分類過的不會再分");
        let rows: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT kind, role, wake FROM supervisor_inbox ORDER BY event_key").fetch_all(&p).await.unwrap();
        assert_eq!(rows, vec![("approval_requested".into(), "responder".into(), 1), ("incident_resolved".into(), "patrol".into(), 0)]);
    }

    /// **真的舊庫**（雙角色以前的 schema）升級。測試不先造 `claimed_by`——那正是要驗的東西：
    /// 升級當下只能靠持久證據（`notify_turn_id`／`delivered_at`／`notify_attempts`／`handled`）
    /// 判斷「這件事舊的單顆 AGM 已經收走了」。
    async fn pre_dual_pool() -> SqlitePool {
        let p = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        // 雙角色以前的 supervisor_inbox：沒有 role／wake／claimed_by／acked_by／merged_into。
        for stmt in [
            "CREATE TABLE supervisors (id TEXT PRIMARY KEY, bot_id TEXT, project_id TEXT, cwd TEXT,
               identity TEXT NOT NULL DEFAULT 'cc0', effort TEXT NOT NULL DEFAULT 'low',
               active_model TEXT NOT NULL DEFAULT 'fable', generation INTEGER NOT NULL DEFAULT 0,
               status TEXT NOT NULL DEFAULT '', status_detail TEXT, fallback_tries INTEGER NOT NULL DEFAULT 0,
               cooldown_until TEXT, quota_reset_at TEXT, remote_status TEXT NOT NULL DEFAULT 'unknown',
               remote_url TEXT, summary TEXT, summary_version INTEGER NOT NULL DEFAULT 0,
               created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE TABLE supervisor_inbox (id TEXT PRIMARY KEY, supervisor_id TEXT NOT NULL, event_key TEXT NOT NULL,
               assignment_id TEXT, bot_id TEXT, turn_id TEXT, kind TEXT NOT NULL,
               payload_json TEXT NOT NULL DEFAULT '{}', state TEXT NOT NULL DEFAULT 'pending', notify_turn_id TEXT,
               notify_delivery TEXT, notify_attempts INTEGER NOT NULL DEFAULT 0, notify_next_at TEXT,
               notify_error TEXT, delivered_at TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE UNIQUE INDEX supervisor_inbox_key ON supervisor_inbox(supervisor_id, event_key)",
        ] {
            sqlx::query(stmt).execute(&p).await.unwrap();
        }
        // 舊庫裡的四種列：送出過（等 ack）、送過但通知回合失敗被 recover 成 pending、已結案、從沒送過。
        let rows = [
            ("old-delivered", "approval_requested", "delivered", Some("t-old-1"), 1, Some("2026-09-01T00:00:00Z")),
            ("old-recovered", "assignment_completed", "pending", Some("t-old-2"), 2, Some("2026-09-01T00:05:00Z")),
            ("old-handled", "approval_requested", "handled", None, 0, None),
            ("old-untouched", "approval_requested", "pending", None, 0, None),
            // 送了三次都沒送出去（`defer_notify` 也會加 attempts），沒有回合、沒有 delivered_at。
            ("old-deferred-unsent", "approval_requested", "pending", None, 3, None),
        ];
        for (key, kind, state, turn, attempts, delivered) in rows {
            sqlx::query(
                "INSERT INTO supervisor_inbox (id, supervisor_id, event_key, kind, payload_json, state,
                   notify_turn_id, notify_attempts, delivered_at, created_at, updated_at)
                 VALUES (?,?,?,?,'{}',?,?,?,?,?,?)",
            )
            .bind(key)
            .bind(super::super::store::SUPERVISOR_ID)
            .bind(key)
            .bind(kind)
            .bind(state)
            .bind(turn)
            .bind(attempts)
            .bind(delivered)
            .bind("2026-09-01T00:00:00Z")
            .bind("2026-09-01T00:00:00Z")
            .execute(&p)
            .await
            .unwrap();
        }
        p
    }

    async fn claimed(pool: &SqlitePool, key: &str) -> (Option<String>, Option<String>, String) {
        sqlx::query_as("SELECT claimed_by, role, state FROM supervisor_inbox WHERE event_key=?")
            .bind(key)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn upgrading_a_real_pre_dual_database_keeps_the_old_managers_events_with_patrol() {
        let p = pre_dual_pool().await;
        super::super::store::migrate(&p).await.unwrap();
        super::super::store::migrate(&p).await.unwrap(); // 可重入：跑兩次結果一樣
        classify(&p).await.unwrap();

        // 送過的（含 recover 成 pending 的）仍是巡檢的，即使路由表把這些種類分給協調者。
        for key in ["old-delivered", "old-recovered"] {
            let (claimed_by, role, _) = claimed(&p, key).await;
            assert_eq!(claimed_by.as_deref(), Some("patrol"), "{key} 舊 AGM 已經收走了");
            assert_eq!(role.as_deref(), Some("responder"), "{key} 的種類照新路由表仍屬協調者");
        }
        // 沒有送達痕跡的交給新路由表：從沒送過的、嘗試過但一次都沒送出去的，以及沒有送達痕跡就結案的。
        for key in ["old-untouched", "old-deferred-unsent", "old-handled"] {
            let (claimed_by, role, _) = claimed(&p, key).await;
            assert_eq!(claimed_by, None, "{key} 沒有送達證據，不能記給巡檢（attempts 不算）");
            assert_eq!(role.as_deref(), Some("responder"));
        }

        // 雙角色啟用之後：協調者只拿沒送過的，送過的留給巡檢，不會兩邊各送一次。
        set_env(&p, Role::Responder, "resp", "proj", "/tmp").await.unwrap();
        let now = "2999-01-01T00:00:00Z";
        let mut resp: Vec<String> = due_for(&p, Role::Responder, true, now, 0).await.unwrap().into_iter().map(|e| e.event_key).collect();
        resp.sort();
        assert_eq!(resp, vec!["old-deferred-unsent".to_string(), "old-untouched".to_string()], "沒送出去過的不再燒巡檢的 fable");
        let patrol: Vec<String> = due_for(&p, Role::Patrol, true, now, 5).await.unwrap().into_iter().map(|e| e.event_key).collect();
        assert_eq!(patrol, vec!["old-recovered".to_string()], "recover 成 pending 的那件回到原收件者");

        // 原收件者才 ack 得動；協調者送不進去也結不掉。
        let id: String = sqlx::query_scalar("SELECT id FROM supervisor_inbox WHERE event_key='old-recovered'").fetch_one(&p).await.unwrap();
        assert_eq!(mark_delivered(&p, &[id.clone()], Role::Responder, "t-new", "ok").await.unwrap(), 0);
        assert_eq!(ack(&p, &id, Some(Role::Responder), true).await.unwrap(), AckOutcome::ClaimedByOther("patrol".into()));
        assert_eq!(ack(&p, &id, Some(Role::Patrol), true).await.unwrap(), AckOutcome::Acked);
    }

    /// 加欄與回填同一個 transaction：回填失敗時連欄位都不能留下，否則重啟看到欄位已經在，
    /// 回填就永遠被跳過。
    #[tokio::test]
    async fn a_failed_backfill_leaves_no_half_migrated_column_and_the_rerun_finishes_it() {
        let p = pre_dual_pool().await;
        // 故障注入：回填那句 UPDATE 會被擋下來。
        sqlx::query(
            "CREATE TRIGGER fail_backfill BEFORE UPDATE ON supervisor_inbox
             BEGIN SELECT RAISE(ABORT, 'injected backfill failure'); END",
        )
        .execute(&p)
        .await
        .unwrap();
        assert!(super::super::store::migrate(&p).await.is_err(), "回填失敗要讓 migrate 失敗");
        assert!(!super::super::store::has_column(&p, "supervisor_inbox", "claimed_by").await.unwrap(), "欄位也跟著回滾");

        sqlx::query("DROP TRIGGER fail_backfill").execute(&p).await.unwrap();
        super::super::store::migrate(&p).await.unwrap();
        let (claimed_by, _, _) = claimed(&p, "old-recovered").await;
        assert_eq!(claimed_by.as_deref(), Some("patrol"), "重跑一次就完整回填");
        let (claimed_by, _, _) = claimed(&p, "old-deferred-unsent").await;
        assert_eq!(claimed_by, None);
    }

    /// 已經照舊判準（attempts>0）回填過的線上庫：沒有送達證據、還在 pending、沒人 ack 的那些放回
    /// 路由表；真的被角色收走或結案的工作一律不碰。
    #[tokio::test]
    async fn events_mis_claimed_by_the_old_backfill_are_released_without_touching_real_claims() {
        let p = pre_dual_pool().await;
        super::super::store::migrate(&p).await.unwrap();
        // 模擬舊版回填的結果＋部署之後的真實工作。
        sqlx::query("UPDATE supervisor_inbox SET claimed_by='patrol' WHERE event_key='old-deferred-unsent'").execute(&p).await.unwrap();
        for (key, state, claimed, turn, delivered, acked) in [
            ("live-resp-delivered", "delivered", "responder", Some("t-r"), Some("2026-09-14T00:00:00Z"), None),
            ("live-patrol-acked", "handled", "patrol", None, None, Some("patrol")),
        ] {
            sqlx::query(
                "INSERT INTO supervisor_inbox (id, supervisor_id, event_key, kind, payload_json, state, claimed_by,
                   notify_turn_id, delivered_at, acked_by, notify_attempts, created_at, updated_at)
                 VALUES (?,?,?,'approval_requested','{}',?,?,?,?,?,0,'2026-09-14T00:00:00Z','2026-09-14T00:00:00Z')",
            )
            .bind(key).bind(super::super::store::SUPERVISOR_ID).bind(key).bind(state).bind(claimed).bind(turn).bind(delivered).bind(acked)
            .execute(&p).await.unwrap();
        }
        super::super::store::migrate(&p).await.unwrap();
        super::super::store::migrate(&p).await.unwrap(); // 可重入

        assert_eq!(claimed(&p, "old-deferred-unsent").await.0, None, "沒送達證據的誤歸屬被放回");
        assert_eq!(claimed(&p, "old-recovered").await.0.as_deref(), Some("patrol"), "有送達證據的不動");
        assert_eq!(claimed(&p, "live-resp-delivered").await.0.as_deref(), Some("responder"), "協調者真的收走的不動");
        let acked: (Option<String>, Option<String>, String) = claimed(&p, "live-patrol-acked").await;
        assert_eq!((acked.0.as_deref(), acked.2.as_str()), (Some("patrol"), "handled"), "已結案的不動");
    }

    #[tokio::test]
    async fn a_responder_event_is_only_due_for_patrol_until_a_responder_exists() {
        let p = pool().await;
        super::super::store::push_inbox(&p, "k1", "approval_requested", None, None, None, &json!({})).await.unwrap();
        super::super::store::push_inbox(&p, "k2", "incident_opened", None, None, None, &json!({})).await.unwrap();
        classify(&p).await.unwrap();
        let now = "2999-01-01T00:00:00Z";
        assert_eq!(due_for(&p, Role::Patrol, false, now, 5).await.unwrap().len(), 2, "舊部署：巡檢收全部");
        let patrol = due_for(&p, Role::Patrol, true, now, 5).await.unwrap();
        assert_eq!(patrol.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), vec!["incident_opened"]);
        let resp = due_for(&p, Role::Responder, true, now, 5).await.unwrap();
        assert_eq!(resp.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), vec!["approval_requested"]);
        // 協調者送不出去很多次，也不會掉回巡檢、也不會被當成用完重試。
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=99").execute(&p).await.unwrap();
        assert_eq!(due_for(&p, Role::Responder, true, now, 5).await.unwrap().len(), 1);
        assert!(due_for(&p, Role::Patrol, true, now, 5).await.unwrap().is_empty());
    }

    /// 兩個角色搶同一件事：先送到的那個角色 claim，另一個既標不了送達、也 ack 不了。
    #[tokio::test]
    async fn one_event_is_claimed_and_acked_by_exactly_one_role() {
        let p = pool().await;
        let id = super::super::store::push_inbox(&p, "k1", "approval_requested", None, None, None, &json!({})).await.unwrap().unwrap();
        classify(&p).await.unwrap();
        let ids = vec![id.clone()];
        assert_eq!(mark_delivered(&p, &ids, Role::Responder, "t1", "ok").await.unwrap(), 1);
        assert_eq!(mark_delivered(&p, &ids, Role::Patrol, "t2", "ok").await.unwrap(), 0, "已被協調者 claim");
        assert_eq!(ack(&p, &id, Some(Role::Patrol), true).await.unwrap(), AckOutcome::ClaimedByOther("responder".into()));
        assert_eq!(ack(&p, &id, Some(Role::Responder), true).await.unwrap(), AckOutcome::Acked);
        assert_eq!(ack(&p, &id, Some(Role::Responder), true).await.unwrap(), AckOutcome::AlreadyHandled);
        assert_eq!(ack(&p, "nope", None, true).await.unwrap(), AckOutcome::NotFound);
        let acked_by: String = sqlx::query_scalar("SELECT acked_by FROM supervisor_inbox WHERE id=?").bind(&id).fetch_one(&p).await.unwrap();
        assert_eq!(acked_by, "responder");
    }

    #[tokio::test]
    async fn concurrent_acks_from_both_roles_close_it_once() {
        let p = pool().await;
        let id = super::super::store::push_inbox(&p, "k1", "approval_requested", None, None, None, &json!({})).await.unwrap().unwrap();
        classify(&p).await.unwrap();
        // 使用者（UI）與協調者同時結：只有一個 Acked。
        let (a, b) = tokio::join!(ack(&p, &id, None, true), ack(&p, &id, Some(Role::Responder), true));
        let outcomes = [a.unwrap(), b.unwrap()];
        assert_eq!(outcomes.iter().filter(|o| **o == AckOutcome::Acked).count(), 1, "{outcomes:?}");
    }

    #[tokio::test]
    async fn patrol_folds_repeated_health_and_flapping_incidents_before_waking() {
        let p = pool().await;
        for (i, s) in ["degraded", "healthy", "degraded"].iter().enumerate() {
            super::super::store::push_inbox(&p, &format!("health:{i}"), "health_changed", None, None, None, &json!({"manager_health": {"status": s}})).await.unwrap();
            // created_at 精度是秒；排序要穩定就把時間往後推。
            sqlx::query("UPDATE supervisor_inbox SET created_at=? WHERE event_key=?")
                .bind(format!("2026-09-13T00:00:0{i}Z"))
                .bind(format!("health:{i}"))
                .execute(&p)
                .await
                .unwrap();
        }
        super::super::store::push_inbox(&p, "incident:I1:opened", "incident_opened", None, None, None, &json!({})).await.unwrap();
        super::super::store::push_inbox(&p, "incident:I1:resolved", "incident_resolved", None, None, None, &json!({})).await.unwrap();
        super::super::store::push_inbox(&p, "incident:I2:opened", "incident_opened", None, None, None, &json!({})).await.unwrap();
        classify(&p).await.unwrap();
        assert_eq!(coalesce_patrol(&p).await.unwrap(), 4, "兩筆舊 health + 一對抖動");
        let due = due_for(&p, Role::Patrol, true, "2999-01-01T00:00:00Z", 5).await.unwrap();
        let keys: Vec<&str> = due.iter().map(|e| e.event_key.as_str()).collect();
        assert_eq!(keys, vec!["health:2", "incident:I2:opened"]);
        assert_eq!(get(&p, Role::Patrol).await.unwrap().merged, 4);
        assert_eq!(coalesce_patrol(&p).await.unwrap(), 0, "再跑一次不會重複合併");
    }
}
