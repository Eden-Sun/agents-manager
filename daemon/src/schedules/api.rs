//! 排程的 HTTP 層（User-only；路由政策登記在 `api.rs` 的 `BOT_ROUTE_POLICIES` 與 `bot_scope_matrix.rs`）。
//!
//! - `GET／POST /api/projects/{id}/schedules`（POST 必帶 `client_request_id`，同專案同 key 同內容回原筆）
//! - `GET／PATCH／DELETE /api/schedules/{id}`、`POST /api/schedules/{id}/run`、`GET /api/schedules/{id}/history`
//!
//! 所有寫入都用 `db::begin_write`（BEGIN IMMEDIATE）開交易，交易內重讀，不信任交易外的讀。
//! 寫完推 WS `schedules_changed`（只帶 `{project_id, id}`），網頁依 id 重抓，prompt 不進全域 state。

use super::runner;
use super::store::{self, Schedule};
use super::time::{self, Spec};
use crate::db;
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_NAME_CHARS: usize = 80;
const MAX_HISTORY_LIMIT: i64 = 100;
const MAX_CRID_LEN: usize = 128;

/// 時間規格的輸入欄位；`mode` 決定用哪幾個欄位。
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct SpecIn {
    pub mode: String,
    pub at: Option<String>,
    pub every_secs: Option<i64>,
    pub time: Option<String>,
    pub weekdays: Option<Vec<u8>>,
}

fn build_spec(input: &SpecIn, now: DateTime<Utc>) -> Result<Spec, LcError> {
    Ok(match input.mode.as_str() {
        "once" => Spec::Once { at: input.at.clone().ok_or_else(|| LcError::Bad("單次需要 at（本地時間）".into()))? },
        "interval" => Spec::Interval {
            every_secs: input.every_secs.ok_or_else(|| LcError::Bad("固定間隔需要 every_secs".into()))?,
            anchor: now,
        },
        "daily" => Spec::Daily { time: input.time.clone().ok_or_else(|| LcError::Bad("每日需要 time".into()))? },
        "weekly" => Spec::Weekly {
            time: input.time.clone().ok_or_else(|| LcError::Bad("每週需要 time".into()))?,
            weekdays: input.weekdays.clone().unwrap_or_default(),
        },
        other => return Err(LcError::Bad(format!("mode 只能是 once／interval／daily／weekly，收到 {other}"))),
    })
}

fn check_text(name: &str, prompt: &str) -> Result<(), LcError> {
    if name.trim().is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(LcError::Bad(format!("名稱不能空白，且不能超過 {MAX_NAME_CHARS} 字")));
    }
    if prompt.trim().is_empty() {
        return Err(LcError::Bad("任務內容不能是空的".into()));
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(LcError::Bad(format!("任務內容不能超過 {MAX_PROMPT_BYTES} 位元組")));
    }
    Ok(())
}

fn check_notify(notify: &str) -> Result<(), LcError> {
    match notify {
        "failure" | "all" | "none" => Ok(()),
        other => Err(LcError::Bad(format!("notify 只能是 failure／all／none，收到 {other}"))),
    }
}

/// 冪等 key：1..=128 個 ASCII 字元（字母、數字、`-_.:`）。
fn check_crid(crid: &str) -> Result<(), LcError> {
    let ok = !crid.is_empty() && crid.len() <= MAX_CRID_LEN && crid.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    if ok {
        Ok(())
    } else {
        Err(LcError::Bad("client_request_id 要是 1 到 128 個字元（英數字與 - _ . :）".into()))
    }
}

fn db_err(e: impl std::fmt::Display) -> LcError {
    LcError::Upstream(e.to_string())
}

fn schedule_json(s: &Schedule) -> Value {
    json!({
        "id": s.id, "project_id": s.project_id, "bot_id": s.bot_id, "name": s.name, "prompt": s.prompt,
        "mode": s.mode, "tz": s.tz, "spec": serde_json::from_str::<Value>(&s.spec_json).unwrap_or(Value::Null),
        "next_due": s.next_due, "enabled": s.enabled == 1, "generation": s.generation, "notify": s.notify,
        "created_at": s.created_at, "updated_at": s.updated_at,
    })
}

async fn with_open(pool: &sqlx::SqlitePool, s: &Schedule) -> Result<Value, LcError> {
    let mut v = schedule_json(s);
    v["open"] = match store::open_occurrence(pool, &s.id).await.map_err(db_err)? {
        Some(o) => store::occurrence_view(pool, &o).await.map_err(db_err)?,
        None => Value::Null,
    };
    Ok(v)
}

async fn announce(app: &App, project_id: &str, id: &str) {
    app.emit("schedules_changed", json!({"project_id": project_id, "id": id})).await;
}

#[derive(Debug, Deserialize)]
pub struct CreateIn {
    pub client_request_id: String,
    pub name: String,
    pub prompt: String,
    pub bot_id: String,
    pub tz: String,
    pub spec: SpecIn,
    #[serde(default = "default_notify")]
    pub notify: String,
}

fn default_notify() -> String {
    "failure".into()
}

/// 內容 fingerprint（不含時間錨點）：同 key 但內容不同 → 409，不會默默回舊的。
fn create_fingerprint(b: &CreateIn) -> String {
    json!({"name": b.name.trim(), "prompt": b.prompt, "bot_id": b.bot_id, "tz": b.tz, "spec": b.spec, "notify": b.notify}).to_string()
}

/// `GET /api/projects/{id}/schedules`
pub async fn list_for_project(State(app): State<Arc<App>>, Path(project_id): Path<String>) -> Result<Json<Value>, LcError> {
    let rows = store::list_schedules(&app.db, &project_id).await.map_err(db_err)?;
    let mut out = Vec::with_capacity(rows.len());
    for s in &rows {
        out.push(with_open(&app.db, s).await?);
    }
    Ok(Json(json!({"schedules": out})))
}

/// `POST /api/projects/{id}/schedules`：只建立排程，不立即執行。同 `client_request_id` 重送回原筆。
pub async fn create(State(app): State<Arc<App>>, Path(project_id): Path<String>, Json(b): Json<CreateIn>) -> Result<Json<Value>, LcError> {
    check_crid(&b.client_request_id)?;
    check_text(&b.name, &b.prompt)?;
    check_notify(&b.notify)?;
    let fp = create_fingerprint(&b);
    let now = Utc::now();
    let mut tx = db::begin_write(&app.db).await.map_err(db_err)?;
    if let Some(prev) = store::find_by_create_key_tx(&mut tx, &project_id, &b.client_request_id).await.map_err(db_err)? {
        if prev.create_fp.as_deref() != Some(fp.as_str()) {
            return Err(LcError::Conflict(json!({"error": "client_request_id_reused", "message": "同一個 client_request_id 已用在不同內容的排程上"})));
        }
        return Ok(Json(schedule_json(&prev)));
    }
    let spec = build_spec(&b.spec, now)?;
    let first = time::validate_and_first(&spec, &b.tz, now).map_err(|e| LcError::Bad(e.to_string()))?;
    if runner::eligible_bot_tx(&mut tx, &b.bot_id, Some(&project_id)).await.map_err(db_err)?.is_none() {
        return Err(LcError::Bad("這顆 Bot 不能接收排程（需要是同專案的一般 Bot，不能是 child、分享或受限）".into()));
    }
    let stamp = db::now();
    let s = Schedule {
        id: db::ulid(),
        project_id: project_id.clone(),
        bot_id: b.bot_id.clone(),
        name: b.name.trim().to_string(),
        prompt: b.prompt.clone(),
        mode: b.spec.mode.clone(),
        tz: b.tz.clone(),
        spec_json: serde_json::to_string(&spec).map_err(db_err)?,
        next_due: first.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        enabled: 1,
        generation: 1,
        notify: b.notify.clone(),
        create_crid: Some(b.client_request_id.clone()),
        create_fp: Some(fp),
        created_at: stamp.clone(),
        updated_at: stamp,
        deleted_at: None,
    };
    store::insert_schedule_tx(&mut tx, &s).await.map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    announce(&app, &project_id, &s.id).await;
    Ok(Json(schedule_json(&s)))
}

/// `GET /api/schedules/{id}`：排程本身、未結束的 occurrence（含綁定 turn 的真實狀態）、最近 10 筆歷史。
pub async fn get_one(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let s = store::get_schedule(&app.db, &id).await.map_err(db_err)?.filter(|s| s.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("schedule".into()))?;
    let mut v = with_open(&app.db, &s).await?;
    let history = store::history(&app.db, &id, 10, 0).await.map_err(db_err)?;
    let mut recent = Vec::with_capacity(history.len());
    for o in &history {
        recent.push(store::occurrence_view(&app.db, o).await.map_err(db_err)?);
    }
    v["recent"] = json!(recent);
    Ok(Json(v))
}

#[derive(Debug, Deserialize)]
pub struct PatchIn {
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub bot_id: Option<String>,
    pub tz: Option<String>,
    pub spec: Option<SpecIn>,
    pub notify: Option<String>,
    pub enabled: Option<bool>,
}

/// `PATCH /api/schedules/{id}`。內容、Bot、時區或時間變動，或停用時：
/// 同一個 bot 鎖與交易內撤回未送出的排隊（已送出的回合照跑），再升 generation。
pub async fn patch(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<PatchIn>) -> Result<Json<Value>, LcError> {
    let pool = app.db.clone();
    let head = store::get_schedule(&pool, &id).await.map_err(db_err)?.filter(|s| s.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("schedule".into()))?;
    let lock = app.bot_lock(&head.bot_id).await;
    let _guard = lock.lock().await;
    let now = db::now();
    let mut tx = db::begin_write(&pool).await.map_err(db_err)?;
    let cur = store::get_schedule_tx(&mut tx, &id).await.map_err(db_err)?.filter(|s| s.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("schedule".into()))?;
    if cur.bot_id != head.bot_id {
        return Err(LcError::Conflict(json!({"error": "schedule_changed", "message": "排程剛剛換了 Bot，請重新整理再試"})));
    }
    let open = store::open_occurrence_tx(&mut tx, &id).await.map_err(db_err)?;
    let name = b.name.as_ref().map(|n| n.trim().to_string()).unwrap_or_else(|| cur.name.clone());
    let prompt = b.prompt.clone().unwrap_or_else(|| cur.prompt.clone());
    let tz = b.tz.clone().unwrap_or_else(|| cur.tz.clone());
    let notify = b.notify.clone().unwrap_or_else(|| cur.notify.clone());
    check_text(&name, &prompt)?;
    check_notify(&notify)?;
    let bot_id = b.bot_id.clone().unwrap_or_else(|| cur.bot_id.clone());
    let bot_changed = bot_id != cur.bot_id;
    if bot_changed {
        if open.is_some() {
            return Err(LcError::Conflict(json!({"error": "schedule_open", "message": "排程還有未結束的執行，換 Bot 前請先等它結束或停用"})));
        }
        if runner::eligible_bot_tx(&mut tx, &bot_id, Some(&cur.project_id)).await.map_err(db_err)?.is_none() {
            return Err(LcError::Bad("這顆 Bot 不能接收排程（需要是同專案的一般 Bot，不能是 child、分享或受限）".into()));
        }
    }
    let now_t = Utc::now();
    let (mode, spec_json, spec_changed) = match &b.spec {
        Some(sp) => {
            let spec = build_spec(sp, now_t)?;
            (sp.mode.clone(), serde_json::to_string(&spec).map_err(db_err)?, true)
        }
        None => (cur.mode.clone(), cur.spec_json.clone(), false),
    };
    let enabled = b.enabled.unwrap_or(cur.enabled == 1);
    let content_changed = name != cur.name || prompt != cur.prompt || tz != cur.tz || notify != cur.notify || spec_json != cur.spec_json || bot_changed;
    let disabling = cur.enabled == 1 && !enabled;
    let mut revoked = None;
    if (content_changed || disabling) && open.is_some() {
        revoked = runner::withdraw_open_tx(&mut tx, &id, "排程已停用或已修改", &now).await.map_err(db_err)?;
    }
    let spec: Spec = serde_json::from_str(&spec_json).map_err(db_err)?;
    let next_due = if enabled {
        // 內容、時間變了，或從停用重新啟用：從現在重算；否則保留原本的下一次。
        if content_changed || spec_changed || cur.enabled == 0 {
            time::validate_and_first(&spec, &tz, now_t).map_err(|e| LcError::Bad(e.to_string()))?.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        } else {
            cur.next_due.clone()
        }
    } else {
        cur.next_due.clone()
    };
    let generation = if content_changed { cur.generation + 1 } else { cur.generation };
    sqlx::query(
        "UPDATE schedules SET name = ?, prompt = ?, bot_id = ?, mode = ?, tz = ?, spec_json = ?, next_due = ?, enabled = ?, generation = ?, notify = ?, updated_at = ?
          WHERE id = ?",
    )
    .bind(&name)
    .bind(&prompt)
    .bind(&bot_id)
    .bind(&mode)
    .bind(&tz)
    .bind(&spec_json)
    .bind(&next_due)
    .bind(if enabled { 1_i64 } else { 0 })
    .bind(generation)
    .bind(&notify)
    .bind(&now)
    .bind(&id)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    drop(_guard);
    if let Some((turn_id, r)) = revoked {
        crate::lifecycle::announce_revoked(&app, &turn_id, r).await;
    }
    let out = store::get_schedule(&pool, &id).await.map_err(db_err)?.ok_or_else(|| LcError::NotFound("schedule".into()))?;
    announce(&app, &out.project_id, &id).await;
    Ok(Json(with_open(&pool, &out).await?))
}

/// `DELETE /api/schedules/{id}`：軟刪（保留關聯，不讓 NOT EXISTS 反向放行）。鎖內重讀，Bot 變了就 409。
pub async fn delete(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let pool = app.db.clone();
    let head = store::get_schedule(&pool, &id).await.map_err(db_err)?.filter(|s| s.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("schedule".into()))?;
    let lock = app.bot_lock(&head.bot_id).await;
    let _guard = lock.lock().await;
    let now = db::now();
    let mut tx = db::begin_write(&pool).await.map_err(db_err)?;
    let cur = store::get_schedule_tx(&mut tx, &id).await.map_err(db_err)?.filter(|s| s.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("schedule".into()))?;
    if cur.bot_id != head.bot_id {
        return Err(LcError::Conflict(json!({"error": "schedule_changed", "message": "排程剛剛換了 Bot，請重新整理再試"})));
    }
    let revoked = runner::withdraw_open_tx(&mut tx, &id, "排程已刪除", &now).await.map_err(db_err)?;
    sqlx::query("UPDATE schedules SET deleted_at = ?, enabled = 0, next_due = NULL, updated_at = ? WHERE id = ? AND deleted_at IS NULL")
        .bind(&now)
        .bind(&now)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    drop(_guard);
    if let Some((turn_id, r)) = revoked {
        crate::lifecycle::announce_revoked(&app, &turn_id, r).await;
    }
    announce(&app, &cur.project_id, &id).await;
    Ok(Json(json!({"ok": true, "id": id})))
}

#[derive(Debug, Deserialize, Default)]
pub struct RunIn {
    #[serde(default)]
    pub client_request_id: Option<String>,
}

/// `POST /api/schedules/{id}/run`：手動執行。交易內重讀排程（停用、刪除、換版都要擋）。
/// 同一個 client_request_id 先查它自己的那筆（不管現在有沒有別的未結束），再看未結束的；不會拿舊 key 回新的。
pub async fn run_now(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<RunIn>) -> Result<Json<Value>, LcError> {
    let pool = app.db.clone();
    let crid = match b.client_request_id {
        Some(c) => {
            check_crid(&c)?;
            Some(c)
        }
        None => None,
    };
    let now = db::now();
    let mut tx = db::begin_write(&pool).await.map_err(db_err)?;
    let s = store::get_schedule_tx(&mut tx, &id).await.map_err(db_err)?.filter(|s| s.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("schedule".into()))?;
    // 同一個 key 的重送：mapping 是權威，回原來那一筆（不論它現在是哪個狀態，也不論排程後來是否停用）。
    if let Some(c) = crid.as_deref() {
        if let Some(prev) = store::find_request_tx(&mut tx, &id, c).await.map_err(db_err)? {
            tx.commit().await.map_err(db_err)?;
            return replay_response(&app, &s, &prev).await;
        }
    }
    if s.enabled != 1 {
        return Err(LcError::Conflict(json!({"error": "schedule_disabled", "message": "排程已停用，先啟用再手動執行"})));
    }
    // 新的 key：併進未結束的 occurrence（不論它是排程來的、還是手動來的），或新開一筆；不論哪一種都記下 key → 那一筆。
    let occ_id = match store::open_occurrence_tx(&mut tx, &id).await.map_err(db_err)? {
        Some(open) => open.id,
        None => insert_manual(&mut tx, &s, crid.clone(), &now).await?,
    };
    if let Some(c) = crid.as_deref() {
        store::bind_request_tx(&mut tx, &id, c, &occ_id, &now).await.map_err(db_err)?;
    }
    tx.commit().await.map_err(db_err)?;
    // 只對還是 pending 的叫 enqueue；它會自己重查有效性與鎖。
    let _ = runner::enqueue(&app, &occ_id).await.map_err(db_err)?;
    let occ = store::get_occurrence(&pool, &occ_id).await.map_err(db_err)?;
    announce(&app, &s.project_id, &id).await;
    let view = match occ {
        Some(o) => Some(store::occurrence_view(&pool, &o).await.map_err(db_err)?),
        None => None,
    };
    Ok(Json(json!({"occurrence": view})))
}

/// 重送的回應：回原來那一筆。若它還是 pending（先前的 enqueue 被擋），這次再試一次；不另開新的。
async fn replay_response(app: &Arc<App>, s: &Schedule, occ_id: &str) -> Result<Json<Value>, LcError> {
    let _ = runner::enqueue(app, occ_id).await.map_err(db_err)?;
    let view = match store::get_occurrence(&app.db, occ_id).await.map_err(db_err)? {
        Some(o) => Some(store::occurrence_view(&app.db, &o).await.map_err(db_err)?),
        None => None,
    };
    announce(app, &s.project_id, &s.id).await;
    Ok(Json(json!({"occurrence": view})))
}

async fn insert_manual(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, s: &Schedule, crid: Option<String>, now: &str) -> Result<String, LcError> {
    let occ = store::Occurrence {
        id: db::ulid(),
        schedule_id: s.id.clone(),
        source: "manual".into(),
        scheduled_for: now.into(),
        generation: s.generation,
        bot_id: s.bot_id.clone(),
        prompt: s.prompt.clone(),
        client_request_id: crid,
        status: "pending".into(),
        waiting_reason: None,
        notified: 0,
        merged_count: 0,
        conversation_id: None,
        turn_id: None,
        error: None,
        created_at: now.into(),
        updated_at: now.into(),
    };
    store::insert_occurrence_tx(tx, &occ).await.map_err(db_err)?;
    Ok(occ.id)
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// `GET /api/schedules/{id}/history?limit=&offset=`：分頁，limit 上限 100。
pub async fn history(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<HistoryQuery>) -> Result<Json<Value>, LcError> {
    store::get_schedule(&app.db, &id).await.map_err(db_err)?.ok_or_else(|| LcError::NotFound("schedule".into()))?;
    let limit = q.limit.unwrap_or(20);
    let offset = q.offset.unwrap_or(0);
    if !(1..=MAX_HISTORY_LIMIT).contains(&limit) || offset < 0 {
        return Err(LcError::Bad(format!("limit 要在 1 到 {MAX_HISTORY_LIMIT} 之間，offset 不能是負的")));
    }
    let rows = store::history(&app.db, &id, limit, offset).await.map_err(db_err)?;
    let mut out = Vec::with_capacity(rows.len());
    for o in &rows {
        out.push(store::occurrence_view(&app.db, o).await.map_err(db_err)?);
    }
    Ok(Json(json!({"occurrences": out, "limit": limit, "offset": offset})))
}
