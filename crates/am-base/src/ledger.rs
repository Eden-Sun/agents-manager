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
/// 管理者的待辦（未結）：注入摘要、孤兒回收、提醒、計數都用這一組。`answered` 不在內——那是「交件、待使用者驗收」，不是管理者要做的事。
pub const OPEN_STATUSES: [&str; 3] = ["untriaged", "following", "clarifying"];
/// 使用者看得到的未結（網頁的「追蹤中」列與面板、`GET ?status=open`）：管理者待辦加上「待你驗收」（`answered`）。
pub const VISIBLE_OPEN_STATUSES: [&str; 4] = ["untriaged", "following", "clarifying", "answered"];
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

/// 計數。`query_actionable_for_actor` 只填未結狀態（`answered`、`done` 為 0）；全專案的計數用 [`count_project`]。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LedgerCounts {
    pub untriaged: i64,
    pub following: i64,
    pub clarifying: i64,
    pub answered: i64,
    pub done: i64,
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
    /// 這筆事項綁的 relay 請求（副管理者代班改派對帳用）；`None` 不綁。
    pub relay_request_id: Option<String>,
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
        /// 指定承辦人（`None` 不動）。要交回待接手請用 `Handback`。
        owner_bot_id: Option<String>,
        /// 結案／取消的原因（`done` 時帶上，`None` 不動）。
        close_reason: Option<String>,
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
    /// `ledger_events.action`：從 `untriaged` 離開的 `Update` 記為 `triage`（§1.2），其餘狀態變更記 `status`。
    fn action(&self, from_status: &str, to_status: &str) -> &'static str {
        match self {
            Self::Update { .. } if from_status == "untriaged" && to_status != "untriaged" => "triage",
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

/// 管理者待辦的 SQL 片段（與 [`OPEN_STATUSES`] 同步）。
const OPEN_SQL: &str = "('untriaged','following','clarifying')";
/// 使用者可見未結的 SQL 片段（與 [`VISIBLE_OPEN_STATUSES`] 同步）。
pub const VISIBLE_OPEN_SQL: &str = "('untriaged','following','clarifying','answered')";

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
    for item in items {
        check_owner(pool, project_id, item.owner_bot_id.as_deref()).await?;
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
    check_owner(pool, project_id, input.owner_bot_id.as_deref()).await?;
    let mut tx = db::begin_write(pool).await?;
    // 追加項從 1 起跳：`ordinal = 0` 留給 `ingest_user_message` 的原始收件項。
    let ordinal: i64 = match source_message_id {
        Some(m) => {
            let owned: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM messages msg JOIN conversations c ON c.id = msg.conversation_id JOIN bots b ON b.id = c.bot_id WHERE msg.id = ? AND b.project_id = ?",
            )
            .bind(m)
            .bind(project_id)
            .fetch_optional(&mut *tx)
            .await?;
            if owned.is_none() {
                return Err(LedgerError::Invalid("source_message_id 不屬於這個專案".into()));
            }
            sqlx::query_scalar("SELECT COALESCE(MAX(ordinal), 0) + 1 FROM ledger_items WHERE project_id = ? AND source_message_id = ?")
                .bind(project_id)
                .bind(m)
                .fetch_one(&mut *tx)
                .await?
        }
        None => 0,
    };
    let code = next_code(&mut tx, project_id).await?;
    let id = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO ledger_items (id, project_id, code_no, source_message_id, ordinal, title, status, accountable_role, owner_bot_id, deliverable, next_step, wait_for, due_at, relay_request_id, version, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
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
    .bind(&input.relay_request_id)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    record(&mut tx, Event { item_id: &id, project_id, actor, action: "commit", from_owner: None, to_owner: input.owner_bot_id.as_deref(), from_status: None, to_status: Some(&status), version: 1 }).await?;
    let item = fetch(&mut tx, &id).await?;
    tx.commit().await?;
    Ok(item)
}

/// 承辦人若指定，必須是專案管理入口（管理者或副管理者）：執行者不承辦帳本項目（2026-11-21 裁示）。
async fn check_owner(pool: &SqlitePool, project_id: &str, owner: Option<&str>) -> Result<(), LedgerError> {
    if let Some(o) = owner {
        if !is_project_management_actor(pool, project_id, o).await? {
            return Err(LedgerError::Invalid("承辦人必須是這個專案的管理入口（管理者或副管理者），執行者不承辦帳本項目".into()));
        }
    }
    Ok(())
}

fn check_status(status: &str) -> Result<(), LedgerError> {
    if STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(LedgerError::Invalid(format!("未知的狀態 {status:?}")))
    }
}

/// 解析事項定位：`L<正整數>`（大小寫皆可）走同專案的 `code_no`；其餘當作 ULID `id`。都找不到回 `NotFound`。
/// 只給 tx 內用，讓 CAS 與 `Merge` 的目標看到同一份資料。
async fn resolve_id(tx: &mut Transaction<'_, Sqlite>, project_id: &str, raw: &str) -> Result<String, LedgerError> {
    let code = if raw.len() <= 12 {
        raw.strip_prefix('L').or_else(|| raw.strip_prefix('l')).and_then(|n| n.parse::<i64>().ok()).filter(|n| *n > 0)
    } else {
        None
    };
    let id: Option<String> = match code {
        Some(n) => sqlx::query_scalar("SELECT id FROM ledger_items WHERE project_id = ? AND code_no = ?").bind(project_id).bind(n).fetch_optional(&mut **tx).await?,
        None => sqlx::query_scalar("SELECT id FROM ledger_items WHERE project_id = ? AND id = ?").bind(project_id).bind(raw).fetch_optional(&mut **tx).await?,
    };
    id.ok_or(LedgerError::NotFound)
}

/// 樂觀鎖異動：`expected_version` 與目前不符就回 [`LedgerError::VersionConflict`]，什麼都不寫。
/// `item_ref` 可以是 ULID，也可以是 `L<code_no>`；`Merge` 的目標同樣接受兩種寫法，寫進 `merged_into_id` 的一律是解析後的 ULID。
pub async fn patch_item_cas(pool: &SqlitePool, project_id: &str, item_ref: &str, expected_version: i64, actor: &str, op: LedgerPatchOp) -> Result<LedgerItem, LedgerError> {
    // 承辦人只能是專案管理入口（管理者或副管理者）：帳本是管理者對使用者的承諾，派給執行者是另一回事（2026-11-21 裁示）。
    let new_owner = match &op {
        LedgerPatchOp::Claim { owner_bot_id } => Some(owner_bot_id.as_str()),
        LedgerPatchOp::Handback { to } => to.as_deref(),
        LedgerPatchOp::Update { owner_bot_id, .. } => owner_bot_id.as_deref(),
        _ => None,
    };
    if let Some(owner) = new_owner {
        if !is_project_management_actor(pool, project_id, owner).await? {
            return Err(LedgerError::Invalid("承辦人必須是這個專案的管理入口（管理者或副管理者），執行者不承辦帳本項目".into()));
        }
    }
    let mut tx = db::begin_write(pool).await?;
    let item_id = resolve_id(&mut tx, project_id, item_ref).await?;
    let cur: LedgerItem = sqlx::query_as("SELECT * FROM ledger_items WHERE id = ? AND project_id = ?")
        .bind(&item_id)
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(LedgerError::NotFound)?;
    if cur.version != expected_version {
        return Err(LedgerError::VersionConflict { current_version: cur.version, current_owner_bot_id: cur.owner_bot_id });
    }
    let mut next = cur.clone();
    match &op {
        LedgerPatchOp::Update { title, status, deliverable, next_step, wait_for, due_at, owner_bot_id, close_reason } => {
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
            if owner_bot_id.is_some() {
                next.owner_bot_id = owner_bot_id.clone();
            }
            if close_reason.is_some() {
                next.close_reason = close_reason.clone();
            }
        }
        LedgerPatchOp::Claim { owner_bot_id } => next.owner_bot_id = Some(owner_bot_id.clone()),
        LedgerPatchOp::Handback { to } => next.owner_bot_id = to.clone(),
        LedgerPatchOp::Merge { into_id } => {
            let target = resolve_id(&mut tx, project_id, into_id).await.map_err(|e| match e {
                LedgerError::NotFound => LedgerError::Invalid("併入的目標不在這個專案".into()),
                other => other,
            })?;
            if target == item_id {
                return Err(LedgerError::Invalid("不能併入自己".into()));
            }
            next.status = "merged".into();
            next.merged_into_id = Some(target);
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
    .bind(&item_id)
    .bind(expected_version)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() != 1 {
        return Err(LedgerError::VersionConflict { current_version: cur.version, current_owner_bot_id: cur.owner_bot_id });
    }
    record(
        &mut tx,
        Event {
            item_id: &item_id,
            project_id,
            actor,
            action: op.action(&cur.status, &next.status),
            from_owner: cur.owner_bot_id.as_deref(),
            to_owner: next.owner_bot_id.as_deref(),
            from_status: Some(&cur.status),
            to_status: Some(&next.status),
            version,
        },
    )
    .await?;
    let item = fetch(&mut tx, &item_id).await?;
    tx.commit().await?;
    Ok(item)
}

/// 孤兒承辦回收：承辦 bot 已軟刪、已硬刪，或已不再是專案管理入口（`is_project_management_actor` 的判準：卸任的副手也算）的未結事項，改回待接手佇列（`owner_bot_id = NULL`，版本 +1）。
/// 事項本身不會消失。回傳被回收的事項。
pub async fn reclaim_orphaned_items(pool: &SqlitePool) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut tx = db::begin_write(pool).await?;
    let orphans: Vec<LedgerItem> = sqlx::query_as(&format!(
        "SELECT * FROM ledger_items li
         WHERE li.status IN {OPEN_SQL} AND li.owner_bot_id IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM bots b
               JOIN projects p ON p.id = b.project_id
               WHERE b.id = li.owner_bot_id
                 AND b.project_id = li.project_id
                 AND b.deleted_at IS NULL
                 AND p.deleted_at IS NULL
                 AND (
                     p.deputy_bot_id = b.id
                     OR (b.managed_by = 'user' AND b.parent_bot_id IS NULL
                         AND ((p.mode = 'expert' AND b.role = 'manager') OR p.mode = 'parent_child'))
                 )
           )
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

/// 專案全部事項的計數（`GET` 用）：每個狀態各計一次；`orphaned` ＝未結且沒有承辦人；`due` ＝未結且已到期。
pub async fn count_project(pool: &SqlitePool, project_id: &str) -> Result<LedgerCounts, LedgerError> {
    let now = db::now();
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as("SELECT status, owner_bot_id, due_at FROM ledger_items WHERE project_id = ?").bind(project_id).fetch_all(pool).await?;
    let mut c = LedgerCounts::default();
    for (status, owner, due) in rows {
        match status.as_str() {
            "untriaged" => c.untriaged += 1,
            "following" => c.following += 1,
            "clarifying" => c.clarifying += 1,
            "answered" => c.answered += 1,
            "done" => c.done += 1,
            _ => {}
        }
        if OPEN_STATUSES.contains(&status.as_str()) {
            if owner.is_none() {
                c.orphaned += 1;
            }
            if due.as_deref().is_some_and(|d| db::cmp_ts(d, &now) != std::cmp::Ordering::Greater) {
                c.due += 1;
            }
        }
    }
    Ok(c)
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


/// 副管理者代班改派（`relay_deputy::redirect`）：綁在這則 relay 請求上的未結事項改給副管理者（`version + 1`）。
/// 還沒有任何綁定事項時，以 `relay_request_id` 冪等建一筆（標題取 `prompt_text`，直接由副手承辦）。
/// 只動主管理者名下或沒有承辦人的未結事項；副手已經接的、已結案的不動。
pub async fn redirect_to_deputy(pool: &SqlitePool, project_id: &str, manager_id: &str, deputy_id: &str, relay_request_id: &str, prompt_text: &str) -> Result<Vec<LedgerItem>, LedgerError> {
    let mut tx = db::begin_write(pool).await?;
    let linked: Option<i64> = sqlx::query_scalar("SELECT 1 FROM ledger_items WHERE project_id = ? AND relay_request_id = ? LIMIT 1")
        .bind(project_id)
        .bind(relay_request_id)
        .fetch_optional(&mut *tx)
        .await?;
    let now = db::now();
    if linked.is_none() {
        let code = next_code(&mut tx, project_id).await?;
        let id = db::ulid();
        let t = title_from(prompt_text);
        let title = if t.is_empty() { "代班請求".to_string() } else { t };
        sqlx::query(
            "INSERT INTO ledger_items (id, project_id, code_no, source_message_id, ordinal, title, status, accountable_role, owner_bot_id, deliverable, next_step, wait_for, due_at, relay_request_id, version, created_at, updated_at)
             VALUES (?, ?, ?, NULL, 0, ?, 'untriaged', ?, ?, '', '', NULL, NULL, ?, 1, ?, ?)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(code)
        .bind(&title)
        .bind(ACCOUNTABLE_ROLE)
        .bind(deputy_id)
        .bind(relay_request_id)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        record(&mut tx, Event { item_id: &id, project_id, actor: "daemon", action: "redirect", from_owner: Some(manager_id), to_owner: Some(deputy_id), from_status: None, to_status: Some("untriaged"), version: 1 }).await?;
        let item = fetch(&mut tx, &id).await?;
        tx.commit().await?;
        return Ok(vec![item]);
    }
    let rows: Vec<LedgerItem> = sqlx::query_as(&format!(
        "SELECT * FROM ledger_items WHERE project_id = ? AND relay_request_id = ? AND (owner_bot_id = ? OR owner_bot_id IS NULL) AND status IN {OPEN_SQL} ORDER BY ordinal, id"
    ))
    .bind(project_id)
    .bind(relay_request_id)
    .bind(manager_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for cur in rows {
        let version = cur.version + 1;
        let res = sqlx::query("UPDATE ledger_items SET owner_bot_id = ?, version = ?, updated_at = ? WHERE id = ? AND version = ?")
            .bind(deputy_id)
            .bind(version)
            .bind(&now)
            .bind(&cur.id)
            .bind(cur.version)
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() != 1 {
            continue;
        }
        record(&mut tx, Event { item_id: &cur.id, project_id, actor: "daemon", action: "redirect", from_owner: cur.owner_bot_id.as_deref(), to_owner: Some(deputy_id), from_status: Some(&cur.status), to_status: Some(&cur.status), version }).await?;
        out.push(fetch(&mut tx, &cur.id).await?);
    }
    tx.commit().await?;
    Ok(out)
}

/// 代班摘要要附的三組對帳（只列、不轉移承辦：副手名下的事項要顯式 `handback`／`claim` 才會換人）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DigestGroups {
    /// 副已完成且已回覆：`answered`／`done`，承辦仍是副手。
    pub done_replied: Vec<LedgerItem>,
    /// 副仍在做（承辦仍為副手，主勿重做）：`following`／`clarifying`。
    pub still_doing: Vec<LedgerItem>,
    /// 待主接手：未結、沒有承辦人（副手交回或孤兒回收）。
    pub awaiting_claim: Vec<LedgerItem>,
}

pub async fn digest_groups(pool: &SqlitePool, project_id: &str, deputy_id: &str) -> Result<DigestGroups, LedgerError> {
    let rows: Vec<LedgerItem> = sqlx::query_as(&format!(
        "SELECT * FROM ledger_items WHERE project_id = ? AND ((owner_bot_id = ? AND status IN ('answered','done','following','clarifying')) OR (owner_bot_id IS NULL AND status IN {OPEN_SQL})) ORDER BY code_no"
    ))
    .bind(project_id)
    .bind(deputy_id)
    .fetch_all(pool)
    .await?;
    let mut g = DigestGroups::default();
    for it in rows {
        match (it.owner_bot_id.as_deref(), it.status.as_str()) {
            (None, _) => g.awaiting_claim.push(it),
            (Some(_), "answered" | "done") => g.done_replied.push(it),
            (Some(_), _) => g.still_doing.push(it),
        }
    }
    Ok(g)
}

/// 摘要尾端的三組文字（沒有項目的組不出現；每組最多 10 行）。
pub fn digest_lines(g: &DigestGroups) -> Vec<String> {
    const MAX: usize = 10;
    let mut out = Vec::new();
    let mut section = |head: &str, items: &[LedgerItem], line: &dyn Fn(&LedgerItem) -> String| {
        if items.is_empty() {
            return;
        }
        out.push(head.to_string());
        for it in items.iter().take(MAX) {
            out.push(line(it));
        }
        if items.len() > MAX {
            out.push(format!("…另 {} 件", items.len() - MAX));
        }
    };
    section("【副已完成且已回覆】", &g.done_replied, &|it| format!("- L{} {}", it.code_no, it.title));
    section("【副仍在做（承辦仍為副手，主勿重做）】", &g.still_doing, &|it| format!("- L{} {}", it.code_no, it.title));
    section("【待主接手（請用 agm ledger claim L<no> --expect-version <v> 認領）】", &g.awaiting_claim, &|it| {
        let next = if it.next_step.is_empty() { String::new() } else { format!("，下一步：{}", it.next_step) };
        format!("- L{} --expect-version {}：{}{}", it.code_no, it.version, it.title, next)
    });
    out
}

/// 專案的頂層管理者（`expert`：`managed_by='user'`、無父層、`role='manager'`；`parent_child`：頂層主力 bot）。
pub async fn top_manager(pool: &SqlitePool, project_id: &str) -> Result<Option<String>, LedgerError> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT b.id FROM bots b JOIN projects p ON p.id = b.project_id
         WHERE b.project_id = ? AND b.managed_by = 'user' AND b.parent_bot_id IS NULL AND b.deleted_at IS NULL AND p.deleted_at IS NULL
           AND ((p.mode = 'expert' AND b.role = 'manager') OR p.mode = 'parent_child')
         ORDER BY b.created_at, b.id LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// 去重提醒的一筆候選（§1.6）：`stage` 是 `untriaged`／`due`／`orphaned`；`prev_*` 是標記前的值（送不出去時還原）。
#[derive(Debug, Clone, PartialEq)]
pub struct Reminder {
    pub item_id: String,
    pub project_id: String,
    pub code_no: i64,
    pub title: String,
    pub status: String,
    pub owner_bot_id: Option<String>,
    pub next_step: String,
    pub version: i64,
    pub stage: &'static str,
    pub due_at: Option<String>,
    pub prev_version: Option<i64>,
    pub prev_stage: Option<String>,
    pub prev_at: Option<String>,
}

/// 未分流需等這麼久（收件回合已結束）才提醒；到期後逾時超過這個時間才升級給頂層管理者。
pub const UNTRIAGED_AFTER_SECS: i64 = 600;
pub const ESCALATE_AFTER_SECS: i64 = 1800;

/// 找出現在該提醒的事項，依 `(item, stage, version)` 去重：同一組已提醒過（`last_reminded_*` 相同）就跳過。
pub async fn due_reminders(pool: &SqlitePool, now: chrono::DateTime<chrono::Utc>) -> Result<Vec<Reminder>, LedgerError> {
    let rows: Vec<LedgerItem> = sqlx::query_as(&format!("SELECT * FROM ledger_items WHERE status IN {OPEN_SQL} ORDER BY created_at, id"))
        .fetch_all(pool)
        .await?;
    let mut out = Vec::new();
    for it in rows {
        let stage: Option<&'static str> = if it.status == "untriaged" {
            let created = chrono::DateTime::parse_from_rfc3339(&it.created_at).map(|d| d.with_timezone(&chrono::Utc)).ok();
            let old = created.is_some_and(|c| (now - c).num_seconds() >= UNTRIAGED_AFTER_SECS);
            let turn_done = match &it.source_turn_id {
                None => true,
                Some(t) => {
                    let st: Option<String> = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(t).fetch_optional(pool).await?;
                    st.is_none_or(|s| matches!(s.as_str(), "completed" | "completed_fallback" | "failed"))
                }
            };
            (old && turn_done).then_some("untriaged")
        } else if it.due_at.as_deref().and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok()).is_some_and(|d| d.with_timezone(&chrono::Utc) <= now) {
            Some("due")
        } else if it.owner_bot_id.is_none() {
            Some("orphaned")
        } else {
            None
        };
        let Some(stage) = stage else { continue };
        if it.last_reminded_version == Some(it.version) && it.last_reminded_stage.as_deref() == Some(stage) {
            continue;
        }
        out.push(Reminder {
            item_id: it.id.clone(),
            project_id: it.project_id.clone(),
            code_no: it.code_no,
            title: it.title.clone(),
            status: it.status.clone(),
            owner_bot_id: it.owner_bot_id.clone(),
            next_step: it.next_step.clone(),
            version: it.version,
            stage,
            due_at: it.due_at.clone(),
            prev_version: it.last_reminded_version,
            prev_stage: it.last_reminded_stage.clone(),
            prev_at: it.last_reminded_at.clone(),
        });
    }
    Ok(out)
}

/// 標記已提醒（CAS：版本還是 `r.version` 才寫）。回傳是否真的寫入；沒寫入代表它已經在別處變了。
pub async fn mark_reminded(pool: &SqlitePool, r: &Reminder) -> Result<bool, LedgerError> {
    let res = sqlx::query("UPDATE ledger_items SET last_reminded_version = ?, last_reminded_stage = ?, last_reminded_at = ? WHERE id = ? AND version = ?")
        .bind(r.version)
        .bind(r.stage)
        .bind(db::now())
        .bind(&r.item_id)
        .bind(r.version)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// 送不出去（管理者忙、佇列滿）：還原標記，下一輪再提醒。
pub async fn unmark_reminded(pool: &SqlitePool, r: &Reminder) -> Result<(), LedgerError> {
    sqlx::query("UPDATE ledger_items SET last_reminded_version = ?, last_reminded_stage = ?, last_reminded_at = ? WHERE id = ? AND version = ? AND last_reminded_stage = ?")
        .bind(r.prev_version)
        .bind(&r.prev_stage)
        .bind(&r.prev_at)
        .bind(&r.item_id)
        .bind(r.version)
        .bind(r.stage)
        .execute(pool)
        .await?;
    Ok(())
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
        let answered = patch_item_cas(&pool, "p1", &a.id, a.version, "bot:b1", LedgerPatchOp::Update { title: None, status: Some("answered".into()), deliverable: None, next_step: None, wait_for: None, due_at: None, owner_bot_id: None, close_reason: None }).await.unwrap();
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
        sqlx::query("UPDATE projects SET deputy_bot_id = 'b2' WHERE id = 'p1'").execute(&pool).await.unwrap();
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
        sqlx::query("UPDATE projects SET deputy_bot_id = 'b2' WHERE id = 'p1'").execute(&pool).await.unwrap();
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

    #[tokio::test]
    async fn a_retired_deputy_loses_its_items_and_the_manager_keeps_them() {
        let pool = pool().await;
        sqlx::query("UPDATE projects SET mode = 'expert' WHERE id = 'p1'").execute(&pool).await.unwrap();
        sqlx::query("UPDATE bots SET role = 'manager' WHERE id = 'b1'").execute(&pool).await.unwrap();
        sqlx::query("UPDATE bots SET role = 'executor' WHERE id = 'b2'").execute(&pool).await.unwrap();
        assert!(is_project_management_actor(&pool, "p1", "b1").await.unwrap(), "expert manager");
        assert!(!is_project_management_actor(&pool, "p1", "b2").await.unwrap(), "executor is not");
        sqlx::query("UPDATE projects SET deputy_bot_id = 'b2' WHERE id = 'p1'").execute(&pool).await.unwrap();
        assert!(is_project_management_actor(&pool, "p1", "b2").await.unwrap(), "acting deputy");
        let deputy_item = commit_item(&pool, "p1", "bot:b2", None, &LedgerSplitInput { title: "代班".into(), owner_bot_id: Some("b2".into()), ..Default::default() }).await.unwrap();
        let mgr_item = commit_item(&pool, "p1", "bot:b1", None, &LedgerSplitInput { title: "自己的".into(), owner_bot_id: Some("b1".into()), ..Default::default() }).await.unwrap();

        sqlx::query("UPDATE projects SET deputy_bot_id = NULL WHERE id = 'p1'").execute(&pool).await.unwrap();
        let reclaimed = reclaim_orphaned_items(&pool).await.unwrap();
        assert_eq!(reclaimed.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec![deputy_item.id.as_str()], "only the retired deputy's item goes back");
        let (owner,): (Option<String>,) = sqlx::query_as("SELECT owner_bot_id FROM ledger_items WHERE id = ?").bind(&mgr_item.id).fetch_one(&pool).await.unwrap();
        assert_eq!(owner.as_deref(), Some("b1"));
    }

    #[tokio::test]
    async fn short_codes_resolve_and_merge_and_done_carry_their_reason() {
        let pool = pool().await;
        let first = ingest_user_message(&pool, "p1", "b1", "m1", None, "第一件").await.unwrap().unwrap();
        let second = commit_item(&pool, "p1", "bot:b1", None, &LedgerSplitInput { title: "第二件".into(), owner_bot_id: Some("b1".into()), ..Default::default() }).await.unwrap();
        assert_eq!(second.code_no, 2);
        let merged = patch_item_cas(&pool, "p1", "L2", second.version, "bot:b1", LedgerPatchOp::Merge { into_id: "L1".into() }).await.unwrap();
        assert_eq!((merged.status.as_str(), merged.merged_into_id.as_deref()), ("merged", Some(first.id.as_str())), "the target is stored as the ULID");
        let done = patch_item_cas(
            &pool,
            "p1",
            "l1",
            first.version,
            "user",
            LedgerPatchOp::Update { title: None, status: Some("done".into()), deliverable: None, next_step: None, wait_for: None, due_at: None, owner_bot_id: None, close_reason: Some("已確認".into()) },
        )
        .await
        .unwrap();
        assert_eq!((done.status.as_str(), done.close_reason.as_deref()), ("done", Some("已確認")));
        let actions: Vec<(String,)> = sqlx::query_as("SELECT action FROM ledger_events WHERE item_id = ? ORDER BY created_at, rowid").bind(&first.id).fetch_all(&pool).await.unwrap();
        // 從 untriaged 直接結案：算一次 triage（離開待分流），不另記 status。
        assert_eq!(actions.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(), vec!["ingest", "triage"]);
        // 之後從已分流的狀態再改，才記 status。
        let reopened = patch_item_cas(&pool, "p1", &first.id, done.version, "user", LedgerPatchOp::Update { title: None, status: Some("following".into()), deliverable: None, next_step: None, wait_for: None, due_at: None, owner_bot_id: None, close_reason: None }).await.unwrap();
        let last: (String,) = sqlx::query_as("SELECT action FROM ledger_events WHERE item_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1").bind(&first.id).fetch_one(&pool).await.unwrap();
        assert_eq!((last.0.as_str(), reopened.status.as_str()), ("status", "following"));
        assert!(patch_item_cas(&pool, "p1", "L1", done.version, "bot:b1", LedgerPatchOp::Merge { into_id: "L1".into() }).await.is_err(), "cannot merge into itself");
        assert!(matches!(patch_item_cas(&pool, "p1", "L9", 1, "user", LedgerPatchOp::Cancel { reason: "x".into() }).await, Err(LedgerError::NotFound)));
    }

    #[tokio::test]
    async fn a_commitment_source_must_belong_to_the_project_and_extra_items_start_at_one() {
        let pool = pool().await;
        ingest_user_message(&pool, "p1", "b1", "m1", None, "原始").await.unwrap();
        let extra = commit_item(&pool, "p1", "bot:b1", Some("m1"), &LedgerSplitInput { title: "追加".into(), ..Default::default() }).await.unwrap();
        assert_eq!(extra.ordinal, 1, "ordinal 0 stays with the ingested item");
        sqlx::query("INSERT INTO projects(id,path,label,created_at) VALUES('p2','/test/p2','p2',?)").bind(db::now()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('b9','p2','b9','claude','t',?)").bind(db::now()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('c9','b9',?)").bind(db::now()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,role,content,source,created_at) VALUES('m9','c9','user','x','web',?)").bind(db::now()).execute(&pool).await.unwrap();
        let foreign = commit_item(&pool, "p1", "bot:b1", Some("m9"), &LedgerSplitInput { title: "外人的".into(), ..Default::default() }).await;
        assert!(matches!(foreign, Err(LedgerError::Invalid(_))), "{foreign:?}");
    }

    /// 管理者 b1（expert 頂層 manager）、副手 b3（projects.deputy_bot_id）、執行者 b2。
    async fn with_management(pool: &SqlitePool) {
        sqlx::query("UPDATE projects SET mode='expert' WHERE id='p1'").execute(pool).await.unwrap();
        sqlx::query("UPDATE bots SET role='manager', managed_by='user' WHERE id='b1'").execute(pool).await.unwrap();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at,role,managed_by) VALUES('b3','p1','b3','claude','t',?,'advisor','user')")
            .bind(db::now()).execute(pool).await.unwrap();
        sqlx::query("UPDATE projects SET deputy_bot_id='b3' WHERE id='p1'").execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn only_management_entrants_can_own_ledger_items() {
        let pool = pool().await;
        with_management(&pool).await;
        let item = commit_item(&pool, "p1", "user", None, &input("追蹤")).await.unwrap();
        let to_executor = patch_item_cas(&pool, "p1", &item.id, 1, "user", LedgerPatchOp::Claim { owner_bot_id: "b2".into() }).await;
        assert!(matches!(to_executor, Err(LedgerError::Invalid(_))), "{to_executor:?}");
        let handed = patch_item_cas(&pool, "p1", &item.id, 1, "user", LedgerPatchOp::Claim { owner_bot_id: "b3".into() }).await.unwrap();
        assert_eq!(handed.owner_bot_id.as_deref(), Some("b3"), "副手可以承辦");
        let to_exec = patch_item_cas(&pool, "p1", &item.id, handed.version, "user", LedgerPatchOp::Handback { to: Some("b2".into()) }).await;
        assert!(matches!(to_exec, Err(LedgerError::Invalid(_))), "交回給執行者也不行：{to_exec:?}");
        let upd = patch_item_cas(&pool, "p1", &item.id, handed.version, "user", LedgerPatchOp::Update { title: None, status: None, deliverable: None, next_step: None, wait_for: None, due_at: None, owner_bot_id: Some("b2".into()), close_reason: None }).await;
        assert!(matches!(upd, Err(LedgerError::Invalid(_))), "Update 指定執行者也不行：{upd:?}");
        let back = patch_item_cas(&pool, "p1", &item.id, handed.version, "user", LedgerPatchOp::Handback { to: Some("b1".into()) }).await.unwrap();
        assert_eq!(back.owner_bot_id.as_deref(), Some("b1"));
        let unassigned = patch_item_cas(&pool, "p1", &item.id, back.version, "user", LedgerPatchOp::Handback { to: None }).await.unwrap();
        assert_eq!(unassigned.owner_bot_id, None, "交回待接手（NULL）正常");
    }

    #[tokio::test]
    async fn removed_deputy_items_are_reclaimed_to_the_unassigned_queue() {
        let pool = pool().await;
        with_management(&pool).await;
        let item = commit_item(&pool, "p1", "user", None, &input("副手的事")).await.unwrap();
        patch_item_cas(&pool, "p1", &item.id, 1, "user", LedgerPatchOp::Claim { owner_bot_id: "b3".into() }).await.unwrap();
        sqlx::query("UPDATE projects SET deputy_bot_id=NULL WHERE id='p1'").execute(&pool).await.unwrap();
        let moved = reclaim_orphaned_items(&pool).await.unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].owner_bot_id, None, "卸任的副手名下回到待接手");
    }

    #[tokio::test]
    async fn redirect_moves_the_linked_open_items_to_the_deputy_only() {
        let pool = pool().await;
        with_management(&pool).await;
        let linked = commit_item(&pool, "p1", "user", None, &LedgerSplitInput { title: "連到請求".into(), owner_bot_id: Some("b1".into()), ..Default::default() }).await.unwrap();
        sqlx::query("UPDATE ledger_items SET relay_request_id='rq1' WHERE id=?").bind(&linked.id).execute(&pool).await.unwrap();
        let other = commit_item(&pool, "p1", "user", None, &LedgerSplitInput { title: "別的".into(), owner_bot_id: Some("b1".into()), ..Default::default() }).await.unwrap();
        let moved = redirect_to_deputy(&pool, "p1", "b1", "b3", "rq1", "x").await.unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!((moved[0].owner_bot_id.as_deref(), moved[0].version), (Some("b3"), linked.version + 1));
        let (owner,): (Option<String>,) = sqlx::query_as("SELECT owner_bot_id FROM ledger_items WHERE id=?").bind(&other.id).fetch_one(&pool).await.unwrap();
        assert_eq!(owner.as_deref(), Some("b1"), "沒綁這則請求的事項不動");
        let (action,): (String,) = sqlx::query_as("SELECT action FROM ledger_events WHERE item_id=? AND action='redirect'").bind(&linked.id).fetch_one(&pool).await.unwrap();
        assert_eq!(action, "redirect");
    }

    #[tokio::test]
    async fn digest_groups_list_without_moving_ownership() {
        let pool = pool().await;
        with_management(&pool).await;
        let mk = |t: &str, o: Option<&str>| LedgerSplitInput { title: t.into(), owner_bot_id: o.map(str::to_string), ..Default::default() };
        let done = commit_item(&pool, "p1", "user", None, &mk("已回", Some("b3"))).await.unwrap();
        patch_item_cas(&pool, "p1", &done.id, 1, "user", LedgerPatchOp::Update { title: None, status: Some("answered".into()), deliverable: None, next_step: None, wait_for: None, due_at: None, owner_bot_id: None, close_reason: None }).await.unwrap();
        commit_item(&pool, "p1", "user", None, &mk("還在做", Some("b3"))).await.unwrap();
        commit_item(&pool, "p1", "user", None, &mk("交回的", None)).await.unwrap();
        let g = digest_groups(&pool, "p1", "b3").await.unwrap();
        assert_eq!((g.done_replied.len(), g.still_doing.len(), g.awaiting_claim.len()), (1, 1, 1));
        let lines = digest_lines(&g).join("\n");
        assert!(lines.contains("【副已完成且已回覆】") && lines.contains("【待主接手"), "{lines}");
        let (owner,): (Option<String>,) = sqlx::query_as("SELECT owner_bot_id FROM ledger_items WHERE id=?").bind(&done.id).fetch_one(&pool).await.unwrap();
        assert_eq!(owner.as_deref(), Some("b3"), "摘要不轉移承辦");
    }

    #[tokio::test]
    async fn reminders_dedupe_by_item_stage_and_version() {
        let pool = pool().await;
        with_management(&pool).await;
        let item = commit_item(&pool, "p1", "user", None, &input("未分流")).await.unwrap();
        sqlx::query("UPDATE ledger_items SET status='untriaged', created_at=? WHERE id=?")
            .bind("2020-01-01T00:00:00.000Z").bind(&item.id).execute(&pool).await.unwrap();
        let now = chrono::Utc::now();
        let first = due_reminders(&pool, now).await.unwrap();
        assert_eq!(first.iter().map(|r| r.stage).collect::<Vec<_>>(), vec!["untriaged"]);
        assert!(mark_reminded(&pool, &first[0]).await.unwrap());
        assert!(due_reminders(&pool, now).await.unwrap().is_empty(), "同 (item, stage, version) 不再提醒");
        // 版本變了（有人動過）就可以再提醒一次。
        sqlx::query("UPDATE ledger_items SET version = version + 1 WHERE id=?").bind(&item.id).execute(&pool).await.unwrap();
        assert_eq!(due_reminders(&pool, now).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unmark_restores_the_previous_reminder_state() {
        let pool = pool().await;
        let item = commit_item(&pool, "p1", "user", None, &input("孤兒")).await.unwrap();
        let r = due_reminders(&pool, chrono::Utc::now()).await.unwrap().remove(0);
        assert_eq!(r.stage, "orphaned");
        assert!(mark_reminded(&pool, &r).await.unwrap());
        unmark_reminded(&pool, &r).await.unwrap();
        assert_eq!(due_reminders(&pool, chrono::Utc::now()).await.unwrap().len(), 1, "送不出去還原後下一輪還會提醒");
        let _ = item;
    }

    #[tokio::test]
    async fn creating_or_splitting_with_an_executor_owner_is_rejected() {
        let pool = pool().await;
        with_management(&pool).await;
        let made = commit_item(&pool, "p1", "user", None, &LedgerSplitInput { title: "給執行者".into(), owner_bot_id: Some("b2".into()), ..Default::default() }).await;
        assert!(matches!(made, Err(LedgerError::Invalid(_))), "{made:?}");
        ingest_user_message(&pool, "p1", "b1", "m1", None, "A、B").await.unwrap();
        let split = split_message(&pool, "p1", "user", "m1", &[input("A"), LedgerSplitInput { title: "B".into(), owner_bot_id: Some("b2".into()), ..Default::default() }]).await;
        assert!(matches!(split, Err(LedgerError::Invalid(_))), "{split:?}");
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM ledger_items WHERE project_id = 'p1'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1, "only the ingested message item; the rejected writes left nothing");
        let ok = commit_item(&pool, "p1", "user", None, &LedgerSplitInput { title: "給副手".into(), owner_bot_id: Some("b3".into()), ..Default::default() }).await.unwrap();
        assert_eq!(ok.owner_bot_id.as_deref(), Some("b3"), "副手可以");
    }

    #[tokio::test]
    async fn redirect_creates_the_item_once_when_nothing_is_linked_yet() {
        let pool = pool().await;
        with_management(&pool).await;
        let first = redirect_to_deputy(&pool, "p1", "b1", "b3", "rq9", "請看 CI 失敗的原因").await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].owner_bot_id.as_deref(), first[0].status.as_str(), first[0].title.as_str()), (Some("b3"), "untriaged", "請看 CI 失敗的原因"));
        assert_eq!(first[0].relay_request_id.as_deref(), Some("rq9"));
        let again = redirect_to_deputy(&pool, "p1", "b1", "b3", "rq9", "請看 CI 失敗的原因").await.unwrap();
        assert!(again.is_empty(), "已經由副手承辦，再改派什麼都不動");
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM ledger_items WHERE relay_request_id = 'rq9'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1, "冪等：不重複建項");
    }

    #[tokio::test]
    async fn answered_waits_for_the_user_and_is_not_the_managers_to_do() {
        let pool = pool().await;
        let item = commit_item(&pool, "p1", "bot:b1", None, &LedgerSplitInput { title: "交件".into(), owner_bot_id: Some("b1".into()), ..Default::default() }).await.unwrap();
        let answered = patch_item_cas(&pool, "p1", &item.id, item.version, "bot:b1", LedgerPatchOp::Update { title: None, status: Some("answered".into()), deliverable: Some("已更新 CI 腳本，見 PR #12".into()), next_step: None, wait_for: None, due_at: Some("2000-01-01T00:00:00Z".into()), owner_bot_id: None, close_reason: None }).await.unwrap();
        assert_eq!(answered.deliverable, "已更新 CI 腳本，見 PR #12");
        let later = chrono::Utc::now() + chrono::Duration::hours(3);
        assert!(due_reminders(&pool, later).await.unwrap().iter().all(|r| r.item_id != item.id), "the manager is not nagged about a delivered item");
        let (mgr, _) = query_actionable_for_actor(&pool, "p1", "b1", None, 10).await.unwrap();
        assert!(mgr.iter().all(|i| i.id != item.id), "not in the manager's actionable list");
        assert!(VISIBLE_OPEN_STATUSES.contains(&"answered") && !OPEN_STATUSES.contains(&"answered"));
        let visible: Vec<(String,)> = sqlx::query_as(&format!("SELECT id FROM ledger_items WHERE project_id = 'p1' AND status IN {VISIBLE_OPEN_SQL}")).fetch_all(&pool).await.unwrap();
        assert_eq!(visible.len(), 1, "the user still sees it in the open list");
    }
}
