//! 防遺忘承諾帳本（第 1 階段 T1）：`ledger_items`／`ledger_events` 的核心操作。
//!
//! 帳本屬於專案（`project_id`），不屬於單一 bot 或對話歷史。收件（[`ingest_user_message`]）以
//! `(project_id, source_message_id, ordinal)` 冪等；認領、交回、改派、狀態變更一律以 `version` 做樂觀鎖
//! （[`patch_item_cas`]），版本不符回 [`LedgerError::VersionConflict`]。每次成功的異動 `version + 1` 並寫一筆 `ledger_events`。
//! 這裡只管資料：不開 API、不注入 prompt、不碰 web（T2–T5 另外接）。

use crate::db;
use serde::Serialize;
use sqlx::{Sqlite, SqlitePool, Transaction};

/// 合法狀態（與表上的 CHECK 相同；先在 Rust 端擋，錯誤訊息比 SQLite 的好讀）。
pub const STATUSES: [&str; 7] = ["untriaged", "answered", "following", "clarifying", "done", "cancelled", "merged"];
/// 仍需人處理的狀態：會被注入摘要、會被孤兒回收。
pub const OPEN_STATUSES: [&str; 3] = ["untriaged", "following", "clarifying"];
/// 承諾事項的問責角色（單一承辦人＋管理者問責，見規格 §1.1）。
pub const ACCOUNTABLE_ROLE: &str = "manager";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct LedgerItem {
    pub id: String,
    pub project_id: String,
    pub code_no: i64,
    pub source_message_id: Option<String>,
    pub source_turn_id: Option<String>,
    pub ordinal: i64,
    pub title: String,
    pub status: String,
    pub accountable_role: String,
    pub owner_bot_id: Option<String>,
    pub promised_by_bot_id: Option<String>,
    pub relay_request_id: Option<String>,
    pub assignment_id: Option<String>,
    pub deliverable: String,
    pub next_step: String,
    pub wait_for: Option<String>,
    pub due_at: Option<String>,
    pub merged_into_id: Option<String>,
    pub close_reason: Option<String>,
    pub version: i64,
    pub last_reminded_version: Option<i64>,
    pub last_reminded_stage: Option<String>,
    pub last_reminded_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 注入摘要用的計數（只算「需要這個 bot 行動」的未結事項）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LedgerCounts {
    pub untriaged: i64,
    pub following: i64,
    pub clarifying: i64,
    /// 沒有承辦人（待接手）。
    pub orphaned: i64,
    /// 已到期（`due_at <= now`）。
    pub due: i64,
}

/// 新增承諾或拆項時可帶的欄位。`status` 未給時：新承諾預設 `following`，拆項預設 `untriaged`。
#[derive(Debug, Clone, Default)]
pub struct LedgerSplitInput {
    pub title: String,
    pub status: Option<String>,
    pub owner_bot_id: Option<String>,
    pub deliverable: String,
    pub next_step: String,
    pub wait_for: Option<String>,
    pub due_at: Option<String>,
}

/// 對既有事項的一次異動（每種對應一個 `ledger_events.action`）。
#[derive(Debug, Clone)]
pub enum LedgerPatchOp {
    /// 補漏修改（action=`status`）。只改有給的欄位；`status` 須在 [`STATUSES`] 內。
    Update {
        title: Option<String>,
        status: Option<String>,
        deliverable: Option<String>,
        next_step: Option<String>,
        wait_for: Option<String>,
        due_at: Option<String>,
    },
    /// 認領：承辦人設為 `owner_bot_id`（action=`claim`）。
    Claim { owner_bot_id: String },
    /// 交回：承辦人改為 `to`，`None` 代表回到待接手佇列（action=`handback`）。
    Handback { to: Option<String> },
    /// 併入另一項：狀態 `merged`，指向 `into_id`（同專案、不能是自己）（action=`merge`）。
    Merge { into_id: String },
    /// 取消：狀態 `cancelled`，記下原因（action=`cancel`）。
    Cancel { reason: String },
}

impl LedgerPatchOp {
    fn action(&self) -> &'static str {
        match self {
            Self::Update { .. } => "status",
            Self::Claim { .. } => "claim",
            Self::Handback { .. } => "handback",
            Self::Merge { .. } => "merge",
            Self::Cancel { .. } => "cancel",
        }
    }
}

#[derive(Debug)]
pub enum LedgerError {
    /// 版本不符：有人先改過了。`current_*` 是現在的值，呼叫端可以直接回給使用者。
    VersionConflict { current_version: i64, current_owner_bot_id: Option<String> },
    NotFound,
    /// 輸入不合法（狀態不在清單、拆項重複、合併到不存在的項等）。
    Invalid(String),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for LedgerError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionConflict { current_version, .. } => write!(f, "ledger_version_conflict（現在是 v{current_version}）"),
            Self::NotFound => write!(f, "找不到這筆承諾"),
            Self::Invalid(why) => write!(f, "{why}"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

/// `open` 條件的 SQL 片段（與 [`OPEN_STATUSES`] 同步）。
const OPEN_SQL: &str = "('untriaged','following','clarifying')";

/// 這則使用者訊息的標題：首行、去頭尾空白、最多 80 字（字元，不是位元組）。
fn title_from(raw_text: &str) -> String {
    let first = raw_text.trim().lines().next().unwrap_or("").trim();
    first.chars().take(80).collect()
}

async fn next_code(tx: &mut Transaction<'_, Sqlite>, project_id: &str) -> Result<i64, LedgerError> {
    Ok(sqlx::query_scalar("SELECT COALESCE(MAX(code_no), 0) + 1 FROM ledger_items WHERE project_id = ?").bind(project_id).fetch_one(&mut **tx).await?)
}

async fn fetch(tx: &mut Transaction<'_, Sqlite>, item_id: &str) -> Result<LedgerItem, LedgerError> {
    sqlx::query_as("SELECT * FROM ledger_items WHERE id = ?").bind(item_id).fetch_optional(&mut **tx).await?.ok_or(LedgerError::NotFound)
}

/// 一筆 `ledger_events`。
struct Event<'a> {
    item_id: &'a str,
    project_id: &'a str,
    actor: &'a str,
    action: &'a str,
    from_owner: Option<&'a str>,
    to_owner: Option<&'a str>,
    from_status: Option<&'a str>,
    to_status: Option<&'a str>,
    version: i64,
}

async fn record(tx: &mut Transaction<'_, Sqlite>, e: Event<'_>) -> Result<(), LedgerError> {
    sqlx::query(
        "INSERT INTO ledger_events (id, item_id, project_id, actor, action, from_owner, to_owner, from_status, to_status, version, detail_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '{}', ?)",
    )
    .bind(db::ulid())
    .bind(e.item_id)
    .bind(e.project_id)
    .bind(e.actor)
    .bind(e.action)
    .bind(e.from_owner)
    .bind(e.to_owner)
    .bind(e.from_status)
    .bind(e.to_status)
    .bind(e.version)
    .bind(db::now())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// 管理入口判斷（規格 §1.4A）：`expert` 專案是頂層 `manager`（`managed_by='user'`、無父層）或 `deputy_bot_id`；
/// `parent_child` 專案是頂層主力 bot（`managed_by='user'`、無父層）。
pub async fn is_project_management_actor(pool: &SqlitePool, project_id: &str, bot_id: &str) -> Result<bool, LedgerError> {
    let deputy: Option<i64> = sqlx::query_scalar("SELECT 1 FROM projects WHERE id = ? AND deputy_bot_id = ? AND deleted_at IS NULL")
        .bind(project_id)
        .bind(bot_id)
        .fetch_optional(pool)
        .await?;
    if deputy.is_some() {
        return Ok(true);
    }
    let top: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM bots b JOIN projects p ON p.id = b.project_id
         WHERE b.id = ? AND b.project_id = ? AND b.managed_by = 'user' AND b.parent_bot_id IS NULL AND b.deleted_at IS NULL AND p.deleted_at IS NULL
           AND ((p.mode = 'expert' AND b.role = 'manager') OR p.mode = 'parent_child')",
    )
    .bind(bot_id)
    .bind(project_id)
    .fetch_optional(pool)
    .await?;
    Ok(top.is_some())
}

/// 使用者訊息的冪等收件：同一則 `message_id`（ordinal 0）只建一筆 `untriaged`。已存在回 `Ok(None)`。
pub async fn ingest_user_message(
    pool: &SqlitePool,
    project_id: &str,
    owner_bot_id: &str,
    message_id: &str,
    turn_id: Option<&str>,
    raw_text: &str,
) -> Result<Option<LedgerItem>, LedgerError> {
    let mut tx = db::begin_write(pool).await?;
    let code = next_code(&mut tx, project_id).await?;
    let id = db::ulid();
    let now = db::now();
    let inserted = sqlx::query(
        "INSERT INTO ledger_items (id, project_id, code_no, source_message_id, source_turn_id, ordinal, title, status, accountable_role, owner_bot_id, version, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 0, ?, 'untriaged', ?, ?, 1, ?, ?)
         ON CONFLICT(project_id, source_message_id, ordinal) WHERE source_message_id IS NOT NULL DO NOTHING",
    )
    .bind(&id)
    .bind(project_id)
    .bind(code)
    .bind(message_id)
    .bind(turn_id)
    .bind(title_from(raw_text))
    .bind(ACCOUNTABLE_ROLE)
    .bind(owner_bot_id)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        tx.rollback().await?;
        return Ok(None);
    }
    record(&mut tx, Event { item_id: &id, project_id, actor: "daemon", action: "ingest", from_owner: None, to_owner: Some(owner_bot_id), from_status: None, to_status: Some("untriaged"), version: 1 }).await?;
    let item = fetch(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(Some(item))
}

/// 使用者訊息落地後的收件入口（四個 `INSERT INTO messages` 的呼叫端共用）。只收「這顆 bot 是專案管理入口」的人打的字；
/// 保溫（`KEEP_WARM_TEXT`）與 `/compact` 不是承諾，不入帳。轉發（relay）由呼叫端先篩掉。回傳 `None` 代表沒入帳（不是管理入口、已存在、或系統字）。
pub async fn ingest_if_management_message(pool: &SqlitePool, bot_id: &str, message_id: &str, turn_id: Option<&str>, text: &str) -> Result<Option<LedgerItem>, LedgerError> {
    let trimmed = text.trim();
    if trimmed == crate::primary_keep_warm::KEEP_WARM_TEXT || trimmed.starts_with("/compact") {
        return Ok(None);
    }
    let Some(project_id): Option<String> = sqlx::query_scalar("SELECT project_id FROM bots WHERE id = ? AND deleted_at IS NULL").bind(bot_id).fetch_optional(pool).await? else {
        return Ok(None);
    };
    if !is_project_management_actor(pool, &project_id, bot_id).await? {
        return Ok(None);
    }
    ingest_user_message(pool, &project_id, bot_id, message_id, turn_id, text).await
}

/// 把一則已收件的訊息拆成 `items.len()` 項：第 1 項更新原本的 `ordinal = 0`，其餘插入 `ordinal = 1..`。
/// 同一則訊息只能拆一次（已有 `ordinal > 0` 就拒絕）。回傳這則訊息的全部事項（依 `ordinal`）。
pub async fn split_message(
    pool: &SqlitePool,
    project_id: &str,
    actor: &str,
    source_message_id: &str,
    items: &[LedgerSplitInput],
) -> Result<Vec<LedgerItem>, LedgerError> {
    if items.is_empty() {
        return Err(LedgerError::Invalid("拆項至少要一項".into()));
    }
    let mut tx = db::begin_write(pool).await?;
    let base: LedgerItem = sqlx::query_as("SELECT * FROM ledger_items WHERE project_id = ? AND source_message_id = ? AND ordinal = 0")
        .bind(project_id)
        .bind(source_message_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(LedgerError::NotFound)?;
    let already: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_items WHERE project_id = ? AND source_message_id = ? AND ordinal > 0")
        .bind(project_id)
        .bind(source_message_id)
        .fetch_one(&mut *tx)
        .await?;
    if already > 0 {
        return Err(LedgerError::Invalid("這則訊息已經拆過項".into()));
    }
    let now = db::now();
    let first = &items[0];
    let first_status = first.status.clone().unwrap_or_else(|| base.status.clone());
    check_status(&first_status)?;
    let first_owner = first.owner_bot_id.clone().or_else(|| base.owner_bot_id.clone());
    let version = base.version + 1;
    sqlx::query("UPDATE ledger_items SET title = ?, status = ?, owner_bot_id = ?, deliverable = ?, next_step = ?, wait_for = ?, due_at = ?, version = ?, updated_at = ? WHERE id = ? AND version = ?")
        .bind(&first.title)
        .bind(&first_status)
        .bind(&first_owner)
        .bind(&first.deliverable)
        .bind(&first.next_step)
        .bind(&first.wait_for)
        .bind(&first.due_at)
        .bind(version)
        .bind(&now)
        .bind(&base.id)
        .bind(base.version)
        .execute(&mut *tx)
        .await?;
    record(&mut tx, Event { item_id: &base.id, project_id, actor, action: "split", from_owner: base.owner_bot_id.as_deref(), to_owner: first_owner.as_deref(), from_status: Some(&base.status), to_status: Some(&first_status), version }).await?;
    for (k, child) in items.iter().enumerate().skip(1) {
        let status = child.status.clone().unwrap_or_else(|| "untriaged".into());
        check_status(&status)?;
        let owner = child.owner_bot_id.clone().or_else(|| base.owner_bot_id.clone());
        let id = db::ulid();
        let code = next_code(&mut tx, project_id).await?;
        sqlx::query(
            "INSERT INTO ledger_items (id, project_id, code_no, source_message_id, source_turn_id, ordinal, title, status, accountable_role, owner_bot_id, deliverable, next_step, wait_for, due_at, version, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(code)
        .bind(source_message_id)
        .bind(&base.source_turn_id)
        .bind(k as i64)
        .bind(&child.title)
        .bind(&status)
        .bind(ACCOUNTABLE_ROLE)
        .bind(&owner)
        .bind(&child.deliverable)
        .bind(&child.next_step)
        .bind(&child.wait_for)
        .bind(&child.due_at)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        record(&mut tx, Event { item_id: &id, project_id, actor, action: "split", from_owner: None, to_owner: owner.as_deref(), from_status: None, to_status: Some(&status), version: 1 }).await?;
    }
    let all: Vec<LedgerItem> = sqlx::query_as("SELECT * FROM ledger_items WHERE project_id = ? AND source_message_id = ? ORDER BY ordinal")
        .bind(project_id)
        .bind(source_message_id)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(all)
}

/// 新增一筆後續承諾（不來自使用者訊息，或同一則訊息的追加項）。`source_message_id` 有值時排在該訊息最後（`ordinal` 接續）。
pub async fn commit_item(pool: &SqlitePool, project_id: &str, actor: &str, source_message_id: Option<&str>, input: &LedgerSplitInput) -> Result<LedgerItem, LedgerError> {
    let status = input.status.clone().unwrap_or_else(|| "following".into());
    check_status(&status)?;
    let mut tx = db::begin_write(pool).await?;
    let ordinal: i64 = match source_message_id {
        Some(m) => sqlx::query_scalar("SELECT COALESCE(MAX(ordinal), -1) + 1 FROM ledger_items WHERE project_id = ? AND source_message_id = ?")
            .bind(project_id)
            .bind(m)
            .fetch_one(&mut *tx)
            .await?,
        None => 0,
    };
    let code = next_code(&mut tx, project_id).await?;
    let id = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO ledger_items (id, project_id, code_no, source_message_id, ordinal, title, status, accountable_role, owner_bot_id, deliverable, next_step, wait_for, due_at, version, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(&id)
    .bind(project_id)
    .bind(code)
    .bind(source_message_id)
    .bind(ordinal)
    .bind(&input.title)
    .bind(&status)
    .bind(ACCOUNTABLE_ROLE)
    .bind(&input.owner_bot_id)
    .bind(&input.deliverable)
    .bind(&input.next_step)
    .bind(&input.wait_for)
    .bind(&input.due_at)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    record(&mut tx, Event { item_id: &id, project_id, actor, action: "commit", from_owner: None, to_owner: input.owner_bot_id.as_deref(), from_status: None, to_status: Some(&status), version: 1 }).await?;
    let item = fetch(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(item)
}

fn check_status(status: &str) -> Result<(), LedgerError> {
    if STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(LedgerError::Invalid(format!("未知的狀態 {status:?}")))
    }
}

/// 樂觀鎖異動：`expected_version` 與目前不符就回 [`LedgerError::VersionConflict`]，什麼都不寫。
pub async fn patch_item_cas(pool: &SqlitePool, project_id: &str, item_id: &str, expected_version: i64, actor: &str, op: LedgerPatchOp) -> Result<LedgerItem, LedgerError> {
    let mut tx = db::begin_write(pool).await?;
    let cur: LedgerItem = sqlx::query_as("SELECT * FROM ledger_items WHERE id = ? AND project_id = ?")
        .bind(item_id)
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(LedgerError::NotFound)?;
    if cur.version != expected_version {
        return Err(LedgerError::VersionConflict { current_version: cur.version, current_owner_bot_id: cur.owner_bot_id });
    }
    let mut next = cur.clone();
    match &op {
        LedgerPatchOp::Update { title, status, deliverable, next_step, wait_for, due_at } => {
            if let Some(s) = status {
                check_status(s)?;
                next.status = s.clone();
            }
            if let Some(t) = title {
                next.title = t.clone();
            }
            if let Some(d) = deliverable {
                next.deliverable = d.clone();
            }
            if let Some(n) = next_step {
                next.next_step = n.clone();
            }
            if wait_for.is_some() {
                next.wait_for = wait_for.clone();
            }
            if due_at.is_some() {
                next.due_at = due_at.clone();
            }
        }
        LedgerPatchOp::Claim { owner_bot_id } => next.owner_bot_id = Some(owner_bot_id.clone()),
        LedgerPatchOp::Handback { to } => next.owner_bot_id = to.clone(),
        LedgerPatchOp::Merge { into_id } => {
            if into_id == item_id {
                return Err(LedgerError::Invalid("不能併入自己".into()));
            }
            let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM ledger_items WHERE id = ? AND project_id = ?").bind(into_id).bind(project_id).fetch_optional(&mut *tx).await?;
            if exists.is_none() {
                return Err(LedgerError::Invalid("併入的目標不在這個專案".into()));
            }
            next.status = "merged".into();
            next.merged_into_id = Some(into_id.clone());
            next.close_reason = Some("merged".into());
        }
        LedgerPatchOp::Cancel { reason } => {
            next.status = "cancelled".into();
            next.close_reason = Some(reason.clone());
        }
    }
    let version = cur.version + 1;
    let now = db::now();
    let res = sqlx::query(
        "UPDATE ledger_items SET title = ?, status = ?, owner_bot_id = ?, deliverable = ?, next_step = ?, wait_for = ?, due_at = ?, merged_into_id = ?, close_reason = ?, version = ?, updated_at = ?
         WHERE id = ? AND version = ?",
    )
    .bind(&next.title)
    .bind(&next.status)
    .bind(&next.owner_bot_id)
    .bind(&next.deliverable)
    .bind(&next.next_step)
    .bind(&next.wait_for)
    .bind(&next.due_at)
    .bind(&next.merged_into_id)
    .bind(&next.close_reason)
    .bind(version)
    .bind(&now)
    .bind(item_id)
    .bind(expected_version)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() != 1 {
        return Err(LedgerError::VersionConflict { current_version: cur.version, current_owner_bot_id: cur.owner_bot_id });
    }
    record(
        &mut tx,
        Event {
            item_id,
            project_id,
            actor,
            action: op.action(),
            from_owner: cur.owner_bot_id.as_deref(),
            to_owner: next.owner_bot_id.as_deref(),
            from_status: Some(&cur.status),
            to_status: Some(&next.status),
            version,
        },
    )
    .await?;
    let item = fetch(&mut tx, item_id).await?;
    tx.commit().await?;
    Ok(item)
}

/// 孤兒承辦回收：承辦 bot 已軟刪（`deleted_at`）或硬刪（列不存在）的未結事項，改回待接手佇列（`owner_bot_id = NULL`，版本 +1）。
/// 事項本身不會消失。回傳被回收的事項。
pub async fn reclaim_orphaned_items(pool: &SqlitePool) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut tx = db::begin_write(pool).await?;
    let orphans: Vec<LedgerItem> = sqlx::query_as(&format!(
        "SELECT * FROM ledger_items li
         WHERE li.status IN {OPEN_SQL} AND li.owner_bot_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM bots b WHERE b.id = li.owner_bot_id AND b.deleted_at IS NULL)
         ORDER BY li.created_at, li.id"
    ))
    .fetch_all(&mut *tx)
    .await?;
    let now = db::now();
    let mut out = Vec::with_capacity(orphans.len());
    for item in orphans {
        let version = item.version + 1;
        sqlx::query("UPDATE ledger_items SET owner_bot_id = NULL, version = ?, updated_at = ? WHERE id = ? AND version = ?")
            .bind(version)
            .bind(&now)
            .bind(&item.id)
            .bind(item.version)
            .execute(&mut *tx)
            .await?;
        record(&mut tx, Event { item_id: &item.id, project_id: &item.project_id, actor: "daemon", action: "orphan_reclaim", from_owner: item.owner_bot_id.as_deref(), to_owner: None, from_status: Some(&item.status), to_status: Some(&item.status), version }).await?;
        out.push(fetch(&mut tx, &item.id).await?);
    }
    tx.commit().await?;
    Ok(out)
}

/// 注入摘要用：這個 bot 需要行動的未結事項（自己的，加上待接手的），以及計數。
/// 排序：① 本則（`current_message_id`）剛收件的 `untriaged` → ② 待接手 → ③ 已到期 → ④ 最久未更新。最多 `limit` 筆；計數算全部。
pub async fn query_actionable_for_actor(pool: &SqlitePool, project_id: &str, bot_id: &str, current_message_id: Option<&str>, limit: usize) -> Result<(Vec<LedgerItem>, LedgerCounts), LedgerError> {
    let now = db::now();
    let rows: Vec<LedgerItem> = sqlx::query_as(&format!(
        "SELECT * FROM ledger_items WHERE project_id = ? AND status IN {OPEN_SQL} AND (owner_bot_id = ? OR owner_bot_id IS NULL)"
    ))
    .bind(project_id)
    .bind(bot_id)
    .fetch_all(pool)
    .await?;
    let mut counts = LedgerCounts::default();
    for it in &rows {
        match it.status.as_str() {
            "untriaged" => counts.untriaged += 1,
            "following" => counts.following += 1,
            _ => counts.clarifying += 1,
        }
        if it.owner_bot_id.is_none() {
            counts.orphaned += 1;
        }
        if it.due_at.as_deref().is_some_and(|d| db::cmp_ts(d, &now) != std::cmp::Ordering::Greater) {
            counts.due += 1;
        }
    }
    let rank = |it: &LedgerItem| -> u8 {
        if it.status == "untriaged" && current_message_id.is_some() && it.source_message_id.as_deref() == current_message_id {
            0
        } else if it.owner_bot_id.is_none() {
            1
        } else if it.due_at.as_deref().is_some_and(|d| db::cmp_ts(d, &now) != std::cmp::Ordering::Greater) {
            2
        } else {
            3
        }
    };
    let mut rows = rows;
    rows.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| db::cmp_ts(&a.updated_at, &b.updated_at)).then_with(|| a.id.cmp(&b.id)));
    rows.truncate(limit);
    Ok((rows, counts))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::query("PRAGMA foreign_keys=ON").execute(&pool).await.unwrap();
        db::migrate_for_crate_tests(&pool).await.unwrap();
        let now = db::now();
        sqlx::query("INSERT INTO projects(id,path,label,created_at) VALUES('p1','/test/p1','p1',?)").bind(&now).execute(&pool).await.unwrap();
        for bot in ["b1", "b2"] {
            sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,'p1',?,'claude','t',?)").bind(bot).bind(bot).bind(&now).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('c1','b1',?)").bind(&now).execute(&pool).await.unwrap();
        for m in ["m1", "m2"] {
            sqlx::query("INSERT INTO messages(id,conversation_id,role,content,source,created_at) VALUES(?,'c1','user','hi','web',?)").bind(m).bind(&now).execute(&pool).await.unwrap();
        }
        pool
    }

    fn input(title: &str) -> LedgerSplitInput {
        LedgerSplitInput { title: title.into(), ..Default::default() }
    }

    #[tokio::test]
    async fn ingest_is_idempotent_by_message_id() {
        let pool = pool().await;
        let first = ingest_user_message(&pool, "p1", "b1", "m1", None, "  請查 CI 失敗\n第二行  ").await.unwrap().expect("first ingest creates L1");
        assert_eq!((first.code_no, first.status.as_str(), first.title.as_str()), (1, "untriaged", "請查 CI 失敗"));
        assert!(ingest_user_message(&pool, "p1", "b1", "m1", None, "請查 CI 失敗").await.unwrap().is_none(), "second ingest of the same message is a no-op");
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM ledger_items WHERE project_id = 'p1'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn split_one_message_into_three_items() {
        let pool = pool().await;
        ingest_user_message(&pool, "p1", "b1", "m1", None, "A、B、C 三件").await.unwrap();
        let all = split_message(&pool, "p1", "bot:b1", "m1", &[input("A"), input("B"), input("C")]).await.unwrap();
        assert_eq!(all.iter().map(|i| i.code_no).collect::<Vec<_>>(), vec![1, 2, 3]);
        let a = all[0].clone();
        let answered = patch_item_cas(&pool, "p1", &a.id, a.version, "bot:b1", LedgerPatchOp::Update { title: None, status: Some("answered".into()), deliverable: None, next_step: None, wait_for: None, due_at: None }).await.unwrap();
        assert_eq!(answered.status, "answered");
        let (open, counts) = query_actionable_for_actor(&pool, "p1", "b1", Some("m1"), 10).await.unwrap();
        assert_eq!(open.iter().map(|i| i.code_no).collect::<Vec<_>>(), vec![2, 3], "only the unanswered two stay open");
        assert_eq!(counts.untriaged, 2);
        assert!(split_message(&pool, "p1", "bot:b1", "m1", &[input("again")]).await.is_err(), "a message can be split only once");
    }

    #[tokio::test]
    async fn cas_version_conflict_prevents_double_claim() {
        let pool = pool().await;
        let item = ingest_user_message(&pool, "p1", "b1", "m1", None, "x").await.unwrap().unwrap();
        assert_eq!(item.version, 1);
        let won = patch_item_cas(&pool, "p1", &item.id, 1, "bot:b1", LedgerPatchOp::Claim { owner_bot_id: "b1".into() }).await.unwrap();
        assert_eq!(won.version, 2);
        let lost = patch_item_cas(&pool, "p1", &item.id, 1, "bot:b2", LedgerPatchOp::Claim { owner_bot_id: "b2".into() }).await;
        assert!(matches!(lost, Err(LedgerError::VersionConflict { current_version: 2, .. })), "{lost:?}");
        let (owner,): (Option<String>,) = sqlx::query_as("SELECT owner_bot_id FROM ledger_items WHERE id = ?").bind(&item.id).fetch_one(&pool).await.unwrap();
        assert_eq!(owner.as_deref(), Some("b1"), "the losing claim wrote nothing");
    }

    #[tokio::test]
    async fn deleted_bot_reclaims_item_to_unassigned_queue() {
        let pool = pool().await;
        let item = commit_item(&pool, "p1", "bot:b2", None, &LedgerSplitInput { title: "追蹤".into(), owner_bot_id: Some("b2".into()), ..Default::default() }).await.unwrap();
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = 'b2'").bind(db::now()).execute(&pool).await.unwrap();
        let reclaimed = reclaim_orphaned_items(&pool).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
        let after = &reclaimed[0];
        assert_eq!(after.id, item.id, "the item survives the bot's deletion");
        assert_eq!(after.owner_bot_id, None);
        assert!(after.version > item.version);
        let (open, counts) = query_actionable_for_actor(&pool, "p1", "b1", None, 10).await.unwrap();
        assert_eq!(open.len(), 1, "a manager sees the unassigned item");
        assert_eq!(counts.orphaned, 1);
    }
}
