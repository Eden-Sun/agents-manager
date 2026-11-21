//! 承諾帳本的 REST 與訊息清單附掛（第 1 階段 T2；資料層與 CAS 在 `am_base::ledger`）。
//!
//! - `GET   /api/projects/{id}/ledger[?status=open|all|a,b&owner=<bot_id>|unassigned&source_message_id=]`：列出＋計數。
//! - `POST  /api/projects/{id}/ledger`：新增後續承諾；帶 `source_message_id` 與 `items` 則把該訊息拆項（同一交易）。
//! - `PATCH /api/projects/{id}/ledger/{item_id}`：`op` = `update`｜`claim`｜`handback`｜`merge`｜`cancel`。`item_id` 可以是 id 或 `L<code_no>`。版本不符回 409。
//!
//! 權限：讀＝User 或同專案的 bot（含 child）；寫＝User 或同專案的專案管理入口（`ledger::is_project_management_actor`：
//! expert 的頂層 manager、代班副管理者、parent_child 的頂層 bot）。其他 bot 與 child 只能讀。跨專案在 scope 表就擋掉。
//! 訊息清單：[`attach_ledger_items_to_messages`] 把同一批訊息的承諾項塞進每則的 `ledger_items`（只在有項目時才加這個欄位）。

use std::{collections::HashMap, sync::Arc};

use am_base::ledger::{self, LedgerError, LedgerItem, LedgerPatchOp, LedgerSplitInput};
use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::{api::RequestPrincipal, db, lifecycle::LcError, state::App};

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ListQuery {
    pub status: Option<String>,
    pub owner: Option<String>,
    pub source_message_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChildIn {
    pub title: String,
    pub status: Option<String>,
    pub owner_bot_id: Option<String>,
    pub deliverable: Option<String>,
    pub next_step: Option<String>,
    pub wait_for: Option<String>,
    pub due_at: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateIn {
    pub source_message_id: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    pub owner_bot_id: Option<String>,
    pub deliverable: Option<String>,
    pub next_step: Option<String>,
    pub wait_for: Option<String>,
    pub due_at: Option<String>,
    pub items: Option<Vec<ChildIn>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct PatchIn {
    pub op: Option<String>,
    pub expected_version: i64,
    pub title: Option<String>,
    pub status: Option<String>,
    pub owner_bot_id: Option<String>,
    pub deliverable: Option<String>,
    pub next_step: Option<String>,
    pub wait_for: Option<String>,
    pub due_at: Option<String>,
    pub merged_into_id: Option<String>,
    pub close_reason: Option<String>,
}

const MAX_TITLE_CHARS: usize = 200;

fn up(e: impl std::fmt::Display) -> LcError {
    LcError::Upstream(e.to_string())
}

fn bad(field: &str, message: &str) -> LcError {
    LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_ledger_field", "field": field, "message": message}))
}

fn forbidden(reason: &str) -> LcError {
    LcError::Forbidden(json!({"error": "forbidden", "reason": reason}))
}

fn ledger_err(e: LedgerError) -> LcError {
    match e {
        LedgerError::VersionConflict { current_version, current_owner_bot_id } => LcError::Conflict(json!({
            "error": "ledger_version_conflict",
            "current_version": current_version,
            "current_owner_bot_id": current_owner_bot_id,
        })),
        LedgerError::NotFound => LcError::NotFoundValue(json!({"error": "not_found", "reason": "ledger_item_not_found"})),
        LedgerError::Invalid(why) => LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_ledger_field", "message": why})),
        LedgerError::Db(e) => up(e),
    }
}

fn opt(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn title(raw: Option<String>) -> Result<String, LcError> {
    let t = raw.unwrap_or_default().trim().to_string();
    if t.is_empty() {
        return Err(bad("title", "required"));
    }
    if t.chars().count() > MAX_TITLE_CHARS {
        return Err(bad("title", "at most 200 chars"));
    }
    Ok(t)
}

fn split_input(c: ChildIn) -> Result<LedgerSplitInput, LcError> {
    Ok(LedgerSplitInput {
        title: title(Some(c.title))?,
        status: opt(c.status),
        owner_bot_id: opt(c.owner_bot_id),
        deliverable: opt(c.deliverable).unwrap_or_default(),
        next_step: opt(c.next_step).unwrap_or_default(),
        wait_for: opt(c.wait_for),
        due_at: opt(c.due_at),
    })
}

/// 回給 API 的一筆：原欄位加上 `code`（`L<code_no>`，對齊第 0 階段的顯示）。
fn out(item: &LedgerItem) -> Value {
    let mut v = serde_json::to_value(item).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut v {
        map.insert("code".into(), json!(format!("L{}", item.code_no)));
    }
    v
}

async fn live_project(app: &Arc<App>, project_id: &str) -> Result<db::Project, LcError> {
    db::project(&app.db, project_id).await.map_err(up)?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("project".into()))
}

/// 呼叫端是 Bot 時確認它活著且屬於這個專案；User 回 `None`；其他 principal 一律拒絕。
async fn caller_in_project(app: &Arc<App>, principal: &RequestPrincipal, project_id: &str) -> Result<(), LcError> {
    let id = match principal {
        RequestPrincipal::User => return Ok(()),
        RequestPrincipal::Bot(id) | RequestPrincipal::Child { child_id: id, .. } => id,
        _ => return Err(forbidden("user_or_project_bot")),
    };
    let bot = db::bot(&app.db, id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| forbidden("unknown_bot"))?;
    if bot.project_id != project_id {
        return Err(forbidden("cross_project"));
    }
    Ok(())
}

/// 寫入者：回傳 `actor`（`user`／`bot:<id>`）。child 與非管理入口的 bot 只能讀。
async fn require_writer(app: &Arc<App>, principal: &RequestPrincipal, project_id: &str) -> Result<String, LcError> {
    caller_in_project(app, principal, project_id).await?;
    match principal {
        RequestPrincipal::User => Ok("user".into()),
        RequestPrincipal::Bot(id) => {
            if ledger::is_project_management_actor(&app.db, project_id, id).await.map_err(ledger_err)? {
                Ok(format!("bot:{id}"))
            } else {
                Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "role_required", "message": "承諾帳本只有專案管理入口（manager、代班副管理者、母子模式的頂層 bot）與使用者能寫；其他成員與 child 只能讀"})))
            }
        }
        _ => Err(forbidden("role_required")),
    }
}

/// 訊息附的 `source_message_id` 必須屬於這個專案（經 conversation → bot 對到 project）。
async fn message_in_project(pool: &SqlitePool, project_id: &str, message_id: &str) -> Result<(), LcError> {
    let ok: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM messages m JOIN conversations c ON c.id = m.conversation_id JOIN bots b ON b.id = c.bot_id WHERE m.id = ? AND b.project_id = ?",
    )
    .bind(message_id)
    .bind(project_id)
    .fetch_optional(pool)
    .await
    .map_err(up)?;
    if ok.is_none() {
        return Err(bad("source_message_id", "not a message of this project"));
    }
    Ok(())
}

async fn notify(app: &Arc<App>, project_id: &str, source_message_id: Option<&str>, item_ids: &[String]) {
    for item_id in item_ids {
        app.emit("ledger_updated", json!({"project_id": project_id, "source_message_id": source_message_id, "item_id": item_id})).await;
    }
}

pub(crate) async fn list(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Query(q): Query<ListQuery>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    caller_in_project(&app, &principal, &project_id).await?;
    live_project(&app, &project_id).await?;
    let all: Vec<LedgerItem> = sqlx::query_as("SELECT * FROM ledger_items WHERE project_id = ? ORDER BY code_no").bind(&project_id).fetch_all(&app.db).await.map_err(up)?;

    let filter_status: Option<Vec<String>> = match opt(q.status) {
        None => None,
        Some(s) if s == "all" => None,
        Some(s) if s == "open" => Some(ledger::OPEN_STATUSES.iter().map(|x| x.to_string()).collect()),
        Some(s) => {
            let list: Vec<String> = s.split(',').map(|x| x.trim().to_string()).collect();
            if list.iter().any(|x| !ledger::STATUSES.contains(&x.as_str())) {
                return Err(bad("status", "open, all, or comma-separated statuses"));
            }
            Some(list)
        }
    };
    let owner = opt(q.owner);
    let source = opt(q.source_message_id);
    let items: Vec<&LedgerItem> = all
        .iter()
        .filter(|i| filter_status.as_ref().is_none_or(|s| s.contains(&i.status)))
        .filter(|i| match owner.as_deref() {
            None => true,
            Some("unassigned") => i.owner_bot_id.is_none(),
            Some(b) => i.owner_bot_id.as_deref() == Some(b),
        })
        .filter(|i| source.as_deref().is_none_or(|m| i.source_message_id.as_deref() == Some(m)))
        .collect();

    let mut counts: HashMap<&str, i64> = ["untriaged", "following", "clarifying", "answered", "done", "orphaned"].into_iter().map(|k| (k, 0)).collect();
    for i in &all {
        let open = ledger::OPEN_STATUSES.contains(&i.status.as_str());
        if matches!(i.status.as_str(), "untriaged" | "following" | "clarifying" | "answered" | "done") {
            *counts.entry(static_key(&i.status)).or_default() += 1;
        }
        if open && i.owner_bot_id.is_none() {
            *counts.entry("orphaned").or_default() += 1;
        }
    }
    Ok(Json(json!({
        "items": items.iter().map(|i| out(i)).collect::<Vec<_>>(),
        "counts": counts,
    })))
}

fn static_key(s: &str) -> &'static str {
    ledger::STATUSES.iter().copied().find(|k| *k == s).unwrap_or("untriaged")
}

pub(crate) async fn create(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<CreateIn>,
) -> Result<Json<Value>, LcError> {
    live_project(&app, &project_id).await?;
    let actor = require_writer(&app, &principal, &project_id).await?;
    let source = opt(body.source_message_id.clone());
    if let Some(m) = &source {
        message_in_project(&app.db, &project_id, m).await?;
    }
    let items: Vec<LedgerItem> = match body.items {
        Some(children) => {
            let Some(m) = source.as_deref() else { return Err(bad("source_message_id", "required when items is given")) };
            if children.is_empty() {
                return Err(bad("items", "at least one item"));
            }
            let inputs = children.into_iter().map(split_input).collect::<Result<Vec<_>, _>>()?;
            ledger::split_message(&app.db, &project_id, &actor, m, &inputs).await.map_err(ledger_err)?
        }
        None => {
            let input = LedgerSplitInput {
                title: title(body.title)?,
                status: opt(body.status),
                owner_bot_id: opt(body.owner_bot_id),
                deliverable: opt(body.deliverable).unwrap_or_default(),
                next_step: opt(body.next_step).unwrap_or_default(),
                wait_for: opt(body.wait_for),
                due_at: opt(body.due_at),
            };
            vec![ledger::commit_item(&app.db, &project_id, &actor, source.as_deref(), &input).await.map_err(ledger_err)?]
        }
    };
    let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    notify(&app, &project_id, source.as_deref(), &ids).await;
    Ok(Json(json!({"items": items.iter().map(out).collect::<Vec<_>>()})))
}

/// `item_id` 可以是 id，也可以是 `L<code_no>`（同專案內的短碼）。
async fn resolve_item(pool: &SqlitePool, project_id: &str, raw: &str) -> Result<String, LcError> {
    let Some(n) = raw.strip_prefix('L').and_then(|n| n.parse::<i64>().ok()) else { return Ok(raw.to_string()) };
    let id: Option<String> = sqlx::query_scalar("SELECT id FROM ledger_items WHERE project_id = ? AND code_no = ?").bind(project_id).bind(n).fetch_optional(pool).await.map_err(up)?;
    id.ok_or_else(|| LcError::NotFoundValue(json!({"error": "not_found", "reason": "ledger_item_not_found"})))
}

pub(crate) async fn patch(
    State(app): State<Arc<App>>,
    Path((project_id, item_ref)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<PatchIn>,
) -> Result<Json<Value>, LcError> {
    live_project(&app, &project_id).await?;
    let actor = require_writer(&app, &principal, &project_id).await?;
    let item_id = resolve_item(&app.db, &project_id, &item_ref).await?;
    let caller_bot = match &principal {
        RequestPrincipal::Bot(id) => Some(id.clone()),
        _ => None,
    };
    let op = match opt(body.op.clone()).as_deref().unwrap_or("update") {
        "update" => LedgerPatchOp::Update {
            title: body.title.clone().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
            status: opt(body.status.clone()),
            deliverable: body.deliverable.clone(),
            next_step: body.next_step.clone(),
            wait_for: opt(body.wait_for.clone()),
            due_at: opt(body.due_at.clone()),
        },
        "claim" => {
            let owner = opt(body.owner_bot_id.clone()).or(caller_bot).ok_or_else(|| bad("owner_bot_id", "required for claim"))?;
            LedgerPatchOp::Claim { owner_bot_id: owner }
        }
        "handback" => LedgerPatchOp::Handback { to: opt(body.owner_bot_id.clone()) },
        "merge" => LedgerPatchOp::Merge { into_id: opt(body.merged_into_id.clone()).ok_or_else(|| bad("merged_into_id", "required for merge"))? },
        "cancel" => LedgerPatchOp::Cancel { reason: opt(body.close_reason.clone()).unwrap_or_else(|| "cancelled".into()) },
        _ => return Err(bad("op", "update, claim, handback, merge or cancel")),
    };
    let item = ledger::patch_item_cas(&app.db, &project_id, &item_id, body.expected_version, &actor, op).await.map_err(ledger_err)?;
    notify(&app, &project_id, item.source_message_id.as_deref(), std::slice::from_ref(&item.id)).await;
    Ok(Json(out(&item)))
}

/// 批次把承諾項附在訊息上：每則 `messages[i]` 的 `ledger_items`（依 `ordinal`）。沒有項目的訊息不加這個欄位。
pub(crate) async fn attach_ledger_items_to_messages(pool: &SqlitePool, messages: &mut [Value]) -> Result<(), LcError> {
    let ids: Vec<String> = messages.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let sql = format!("SELECT * FROM ledger_items WHERE source_message_id IN ({}) ORDER BY ordinal", vec!["?"; ids.len()].join(","));
    let mut q = sqlx::query_as::<_, LedgerItem>(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(pool).await.map_err(up)?;
    let mut by: HashMap<String, Vec<Value>> = HashMap::new();
    for r in &rows {
        if let Some(m) = &r.source_message_id {
            by.entry(m.clone()).or_default().push(out(r));
        }
    }
    for m in messages.iter_mut() {
        if let Some(id) = m["id"].as_str().map(str::to_owned) {
            if let Some(v) = by.remove(&id) {
                m["ledger_items"] = Value::Array(v);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod ledger_api_tests;
