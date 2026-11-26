//! codex／agy bot 的「基準快照」與排程「每次從基準快照開始」。
//!
//! 基準快照＝把某一刻 codex bot 的 native session id 鎖下來（`bot_base_snapshots`）。session id 沿用
//! `db::latest_native_session_id`（hook 記在 `runs.native_session_id`，`agm hire --fork-from` 同一個來源）。
//! 排程勾了 `from_base`：runner 送出前等 bot 閒置，用 `StartOpts.fork_session`（＝codex `fork <session-id>`，
//! 同 `agm hire --fork-from`／`POST /bots/:id/fork` 的啟動路徑）把 bot 重啟成基準的一份新 session，再送排程指示。
//! codex fork 只會產生新 session，基準 rollout 不會被改；基準缺失就 failed，不退回在舊 context 執行。
//! agy 沒有原生 fork：鎖基準時把對話複成一份凍結的複本（`agy_session::clone_conversation`），基準＝複本的對話 id；
//! 執行時再從複本複出新對話、`--conversation=<新 id>` 接續（`start.rs` 的 fork 分支），複本與基準都不會被改。
//! 儲存排程勾 `from_base` 時目標 bot 沒有基準：[`spawn_ensure`] 在背景自動製作（有 session 就鎖，沒有就啟動 bot 取得 session 再鎖），
//! 儲存請求不等它。到點時沒有基準、也沒在建立：延後並開始建立（daemon 重啟、清除基準後都靠這條自動恢復）；建立中就延後（`base_building`）；
//! 建立失敗後 [`BUILD_COOLDOWN`] 內到點的排程直接 failed（`base_snapshot_create_failed`，可依重試額度重試），冷卻過了才再建，避免每個 tick 狂建。

use super::runner;
use super::store::{self, Occurrence, Schedule};
use crate::db;
use crate::events::ports::TurnCommands;
use crate::lifecycle::{self, LcError, QueueClass, StartOpts};
use crate::state::App;
use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// fork 重啟後等新 run 進入 running 的上限。
const SEED_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 3 } else { 240 });
const SEED_TEXT: &str = "建立排程基準對話，請只回 OK";
const READY_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 3 } else { 120 });

fn up(e: impl std::fmt::Display) -> LcError {
    LcError::Upstream(e.to_string())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Snapshot {
    pub session_id: String,
    pub transcript_path: Option<String>,
    pub locked_at: String,
    pub locked_by: String,
}

pub async fn get(pool: &sqlx::SqlitePool, bot_id: &str) -> Result<Option<Snapshot>> {
    Ok(sqlx::query_as("SELECT session_id, transcript_path, locked_at, locked_by FROM bot_base_snapshots WHERE bot_id = ?").bind(bot_id).fetch_optional(pool).await?)
}

fn view(s: &Snapshot) -> Value {
    json!({"locked": true, "session_id": s.session_id, "locked_at": s.locked_at, "locked_by": s.locked_by})
}

/// 使用者，或這顆 bot 所在 expert 專案的管理者；其他 bot（含 bot 自己）不行。
async fn authorize(app: &Arc<App>, principal: &crate::api::RequestPrincipal, id: &str) -> Result<db::Bot, LcError> {
    let bot = db::bot(&app.db, id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    let ok = match principal {
        crate::api::RequestPrincipal::User => true,
        crate::api::RequestPrincipal::Bot(caller) => crate::api::expert_manager_over(app, caller, &bot).await?,
        _ => false,
    };
    if ok { Ok(bot) } else { Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "manager_role_scope"}))) }
}

/// `GET /api/bots/{id}/base-snapshot`
pub async fn get_snapshot(State(app): State<Arc<App>>, Path(id): Path<String>, Extension(p): Extension<crate::api::RequestPrincipal>) -> Result<Response, LcError> {
    authorize(&app, &p, &id).await?;
    Ok((StatusCode::OK, Json(status_json(&app, &id).await?)).into_response())
}

/// 基準本身加上建立狀態（`build`：`{state: building|failed, message}`，沒有在做也沒失敗就是 null）。
async fn status_json(app: &Arc<App>, bot_id: &str) -> Result<Value, LcError> {
    let mut v = get(&app.db, bot_id).await.map_err(up)?.map_or(json!({"locked": false}), |s| view(&s));
    v["build"] = build_state(bot_id).map_or(Value::Null, |(state, message)| json!({"state": state, "message": message}));
    Ok(v)
}

/// `POST /api/bots/{id}/base-snapshot/build`：重新在背景製作基準（失敗後的重試）。已有基準就什麼都不做。立即回 202。
pub async fn build(State(app): State<Arc<App>>, Path(id): Path<String>, Extension(p): Extension<crate::api::RequestPrincipal>) -> Result<Response, LcError> {
    let bot = authorize(&app, &p, &id).await?;
    if !supported(&bot.kind) {
        return Err(unsupported(&bot.id));
    }
    spawn_ensure(&app, &bot.id, &bot.project_id);
    Ok((StatusCode::ACCEPTED, Json(status_json(&app, &bot.id).await?)).into_response())
}

fn supported(kind: &str) -> bool {
    matches!(kind, "codex" | "agy")
}

fn unsupported(bot_id: &str) -> LcError {
    LcError::BadValue(json!({"error": "bad_request", "reason": "base_snapshot_unsupported_kind", "bot_id": bot_id}))
}

/// `PUT /api/bots/{id}/base-snapshot`：把 bot 目前的 native session 鎖成基準（已有就換成現在這個）。codex／agy。
pub async fn lock(State(app): State<Arc<App>>, Path(id): Path<String>, Extension(p): Extension<crate::api::RequestPrincipal>) -> Result<Response, LcError> {
    let bot = authorize(&app, &p, &id).await?;
    let by = match &p {
        crate::api::RequestPrincipal::Bot(b) => b.clone(),
        _ => "user".into(),
    };
    let s = lock_latest(&app, &bot, &by).await?;
    Ok((StatusCode::OK, Json(view(&s))).into_response())
}

async fn latest_session(pool: &sqlx::SqlitePool, bot_id: &str) -> Result<Option<(String, Option<String>)>, LcError> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT native_session_id, transcript_path FROM runs WHERE bot_id = ? AND native_session_id IS NOT NULL ORDER BY started_at DESC, rowid DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(pool)
    .await
    .map_err(up)?;
    Ok(row.filter(|(s, _)| !s.trim().is_empty()))
}

/// 把 bot 最近的 native session 鎖成基準。agy 先複成凍結的複本，基準記複本的 id，bot 之後繼續用原對話也不會動到基準。
async fn lock_latest(app: &Arc<App>, bot: &db::Bot, by: &str) -> Result<Snapshot, LcError> {
    if !supported(&bot.kind) {
        return Err(unsupported(&bot.id));
    }
    let Some((mut session_id, mut transcript)) = latest_session(&app.db, &bot.id).await? else {
        return Err(LcError::conflict("base_snapshot_no_session", json!({"reason": "base_snapshot_no_session", "bot_id": bot.id})));
    };
    if bot.kind == "agy" {
        let host = db::bot_host(&app.db, &bot.id).await.map_err(up)?;
        session_id = lifecycle::agy_session::clone_conversation(app.as_ref(), &host, bot.identity.as_deref(), &session_id).await.map_err(|e| {
            let reason = if lifecycle::agy_session::is_sqlite3_missing(&e) { lifecycle::agy_session::SQLITE3_MISSING } else { "base_snapshot_copy_failed" };
            LcError::conflict(reason, json!({"bot_id": bot.id, "host": host, "cause": e.to_string()}))
        })?;
        lifecycle::agy_session::record_copy(&app.db, &session_id, &bot.id, &host, bot.identity.as_deref(), "base", None).await.map_err(up)?;
        transcript = None;
    }
    sqlx::query(
        "INSERT INTO bot_base_snapshots (bot_id, session_id, transcript_path, locked_at, locked_by) VALUES (?,?,?,?,?)
         ON CONFLICT(bot_id) DO UPDATE SET session_id = excluded.session_id, transcript_path = excluded.transcript_path, locked_at = excluded.locked_at, locked_by = excluded.locked_by",
    )
    .bind(&bot.id)
    .bind(&session_id)
    .bind(transcript.filter(|t| !t.trim().is_empty()))
    .bind(db::now())
    .bind(by)
    .execute(&app.db)
    .await
    .map_err(up)?;
    if bot.kind == "agy" {
        spawn_sweep(app, Some(&bot.id));
    }
    get(&app.db, &bot.id).await.map_err(up)?.ok_or_else(|| up("snapshot vanished"))
}

/// 背景清掉用不到的 agy 對話複本（`agy_session::sweep_copies`）；遠端要 ssh，不擋呼叫端。
pub fn spawn_sweep(app: &Arc<App>, bot_id: Option<&str>) {
    let (app, bot_id) = (app.clone(), bot_id.map(str::to_owned));
    tokio::spawn(async move {
        if let Err(e) = lifecycle::agy_session::sweep_copies(app.as_ref(), bot_id.as_deref()).await {
            tracing::warn!(bot = ?bot_id, error = ?e, "agy copies: sweep failed");
        }
    });
}

fn ensure_locks() -> &'static Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

/// 每顆 bot 還在製作中的份數（[`ensure`] 進出時加減）；有就是「建立中」。
fn building() -> &'static Mutex<std::collections::HashMap<String, usize>> {
    static BUILDING: OnceLock<Mutex<std::collections::HashMap<String, usize>>> = OnceLock::new();
    BUILDING.get_or_init(Default::default)
}

/// 每顆 bot 最近一次自動製作失敗的訊息；製作成功時清掉。
fn last_failure() -> &'static Mutex<std::collections::HashMap<String, (String, std::time::Instant)>> {
    static FAILED: OnceLock<Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>> = OnceLock::new();
    FAILED.get_or_init(Default::default)
}

/// 同一顆 bot 製作失敗後，這段時間內到點的排程直接 failed、不再重建（避免每個 tick 狂建）。
const BUILD_COOLDOWN: Duration = Duration::from_secs(600);

/// 冷卻期內最近一次製作失敗的訊息。
fn recent_failure(bot_id: &str) -> Option<String> {
    last_failure().lock().unwrap().get(bot_id).filter(|(_, at)| at.elapsed() < BUILD_COOLDOWN).map(|(m, _)| m.clone())
}

/// 基準的建立狀態：`building`（製作中），或 `failed`（最近一次失敗，附訊息）；都不是就是 None。
pub(super) fn build_state(bot_id: &str) -> Option<(&'static str, Option<String>)> {
    if building().lock().unwrap().get(bot_id).is_some_and(|n| *n > 0) {
        return Some(("building", None));
    }
    last_failure().lock().unwrap().get(bot_id).map(|(m, _)| ("failed", Some(m.clone())))
}

#[cfg(test)]
pub(super) fn set_building_for_test(bot_id: &str, on: bool) {
    let mut b = building().lock().unwrap();
    if on { *b.entry(bot_id.to_string()).or_default() += 1 } else { b.remove(bot_id); }
}

#[cfg(test)]
pub(super) fn set_failure_for_test(bot_id: &str, msg: &str, age: Duration) {
    let at = std::time::Instant::now().checked_sub(age).unwrap();
    last_failure().lock().unwrap().insert(bot_id.to_string(), (msg.to_string(), at));
}

async fn progress(app: &Arc<App>, bot_id: &str, state: &str, message: Option<&str>) {
    app.emit("base_snapshot_progress", json!({"bot_id": bot_id, "state": state, "message": message})).await;
}

/// 全新 agy 啟動後還沒有對話（第一則訊息送出才產生，hook 只回報狀態）：送一則 daemon 來源的種子訊息，等這一回合完成，
/// 再把 pane 開著的對話綁到 run（`agy_session::load`，遠端經 ssh 讀那台的 /proc／lsof），讓 `runs.native_session_id` 有值。
async fn seed_agy(app: &Arc<App>, bot: &db::Bot) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + SEED_TIMEOUT;
    let crid = format!("base-seed-{}", db::ulid());
    let sent = loop {
        match app.prompt_relayed_queueable(&bot.id, SEED_TEXT, &crid, None, QueueClass::DaemonNotice).await {
            Ok(out) => break out,
            Err(LcError::Conflict(v)) if v.get("retryable").and_then(Value::as_bool) == Some(true) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(if cfg!(test) { 50 } else { 2000 })).await;
            }
            Err(e) => return Err(format!("種子訊息送不出去（{e:?}）")),
        }
    };
    loop {
        let status: Option<String> = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(&sent.turn_id).fetch_optional(&app.db).await.map_err(|e| e.to_string())?;
        match status.as_deref() {
            Some("completed" | "completed_fallback") => break,
            Some("failed") | None => return Err("種子回合失敗或被撤回，agy 沒有建立對話".into()),
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("等了 {} 秒，agy 還沒答完種子訊息（可能卡在登入或提問）", SEED_TIMEOUT.as_secs()));
        }
        tokio::time::sleep(Duration::from_millis(if cfg!(test) { 20 } else { 500 })).await;
    }
    let bind_until = tokio::time::Instant::now() + Duration::from_secs(if cfg!(test) { 1 } else { 20 });
    loop {
        if latest_session(&app.db, &bot.id).await.map_err(|e| format!("{e:?}"))?.is_some() {
            return Ok(());
        }
        if let Some(run) = db::active_run(&app.db, &bot.id).await.map_err(|e| e.to_string())? {
            let host = db::bot_host(&app.db, &bot.id).await.map_err(|e| e.to_string())?;
            let _ = lifecycle::agy_session::load(app.as_ref(), &run, &host).await;
        }
        if tokio::time::Instant::now() >= bind_until {
            return Err("種子回合完成了，但讀不到 agy 的對話 id（pane 沒有開著對話檔）".into());
        }
        tokio::time::sleep(Duration::from_millis(if cfg!(test) { 50 } else { 1000 })).await;
    }
}

fn failure_message(e: &LcError) -> String {
    match e {
        LcError::Conflict(v) | LcError::BadValue(v) => v.get("message").and_then(Value::as_str).or_else(|| v.get("reason").and_then(Value::as_str)).map(str::to_owned).unwrap_or_else(|| format!("{e:?}")),
        other => format!("{other:?}"),
    }
}

/// 在背景製作基準，不擋呼叫端（儲存排程、改綁、重試都走這裡）。已有基準或已在製作的，後到的會等同一把鎖、不重做。
pub(super) fn spawn_ensure(app: &Arc<App>, bot_id: &str, project_id: &str) {
    // 計入「製作中」要在 spawn 當下做，不然下一個 tick 會看不到，又開一份。
    begin_build(bot_id);
    let (app, bot_id, project_id) = (app.clone(), bot_id.to_owned(), project_id.to_owned());
    tokio::spawn(async move {
        if let Err(e) = ensure(&app, &bot_id, &project_id).await {
            tracing::warn!(bot = %bot_id, error = ?e, "base snapshot: background build failed");
        }
        end_build(&bot_id);
    });
}

fn begin_build(bot_id: &str) {
    *building().lock().unwrap().entry(bot_id.to_string()).or_default() += 1;
}

fn end_build(bot_id: &str) {
    let mut b = building().lock().unwrap();
    if let Some(n) = b.get_mut(bot_id) {
        *n -= 1;
        if *n == 0 {
            b.remove(bot_id);
        }
    }
}

/// 勾 `from_base` 時目標 bot 沒有基準就自動製作（不回 400）。有 native session 就鎖；完全沒有就照正常方式啟動一次、
/// 等它回報 session 再鎖。bot 不合格（不是這個專案可接排程的 bot）時什麼都不做，由後續檢查回對的錯。
/// 同一顆 bot 同時只有一份在做：後到的請求等前一份的結果，有基準了就直接用，不再送第二則種子。
/// 開始與結束推 `base_snapshot_progress`（building／ready／failed），網頁據此顯示進度；失敗訊息留在 [`build_state`]。
async fn ensure(app: &Arc<App>, bot_id: &str, project_id: &str) -> Result<(), LcError> {
    if get(&app.db, bot_id).await.map_err(up)?.is_some() {
        return Ok(());
    }
    let out = ensure_locked(app, bot_id, project_id).await;
    match &out {
        Ok(true) => {
            last_failure().lock().unwrap().remove(bot_id);
        }
        Ok(false) => {}
        Err(e) => {
            last_failure().lock().unwrap().insert(bot_id.to_string(), (failure_message(e), std::time::Instant::now()));
        }
    }
    out.map(|_| ())
}

async fn ensure_locked(app: &Arc<App>, bot_id: &str, project_id: &str) -> Result<bool, LcError> {
    let lock = ensure_locks().lock().unwrap().entry(bot_id.to_string()).or_default().clone();
    let _one = lock.lock().await;
    if get(&app.db, bot_id).await.map_err(up)?.is_some() {
        return Ok(false);
    }
    let out = ensure_once(app, bot_id, project_id).await;
    match &out {
        Ok(true) => progress(app, bot_id, "ready", None).await,
        Ok(false) => {}
        Err(e) => progress(app, bot_id, "failed", Some(&failure_message(e))).await,
    }
    out
}

/// 回 true＝這次做了基準。
async fn ensure_once(app: &Arc<App>, bot_id: &str, project_id: &str) -> Result<bool, LcError> {
    {
        let mut tx = app.db.begin().await.map_err(up)?;
        if runner::eligible_bot_tx(&mut tx, bot_id, Some(project_id)).await.map_err(up)?.is_none() {
            return Ok(false);
        }
    }
    let Some(bot) = db::bot(&app.db, bot_id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()) else { return Ok(false) };
    if !supported(&bot.kind) {
        return Err(unsupported(bot_id));
    }
    progress(app, bot_id, "building", None).await;
    let failed = |why: String| LcError::conflict("base_snapshot_create_failed", json!({"reason": "base_snapshot_create_failed", "bot_id": bot_id, "message": format!("找不到 {} 的基準快照，自動製作也失敗：{why}", bot.name)}));
    if latest_session(&app.db, bot_id).await?.is_none() {
        if db::active_run(&app.db, bot_id).await.map_err(up)?.is_none() {
            Box::pin(lifecycle::start_bot_with(app, bot_id, StartOpts::default())).await.map_err(|e| failed(format!("啟動失敗（{e:?}）")))?;
        }
        if bot.kind == "agy" {
            seed_agy(app, &bot).await.map_err(failed)?;
        }
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        while latest_session(&app.db, bot_id).await?.is_none() {
            if tokio::time::Instant::now() >= deadline {
                return Err(failed("等不到 bot 回報對話 session".into()));
            }
            tokio::time::sleep(Duration::from_millis(if cfg!(test) { 20 } else { 500 })).await;
        }
    }
    lock_latest(app, &bot, "schedule").await.map(|_| true)
}

/// `DELETE /api/bots/{id}/base-snapshot`：清除基準（沒有也回成功）。仍勾著 `from_base` 的排程之後到點會重新製作基準並延後，不會失敗。
pub async fn clear(State(app): State<Arc<App>>, Path(id): Path<String>, Extension(p): Extension<crate::api::RequestPrincipal>) -> Result<Response, LcError> {
    authorize(&app, &p, &id).await?;
    sqlx::query("DELETE FROM bot_base_snapshots WHERE bot_id = ?").bind(&id).execute(&app.db).await.map_err(up)?;
    spawn_sweep(&app, Some(&id));
    Ok((StatusCode::OK, Json(json!({"locked": false}))).into_response())
}

/// 勾 `from_base` 的前提：bot 是 codex／agy（交易內查）。基準可以還沒有：儲存後由 [`spawn_ensure`] 在背景製作，
/// 到點時仍沒有的由 [`dispatch`] 延後或失敗。
pub(super) async fn require_kind_tx(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, bot_id: &str) -> Result<(), LcError> {
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM bots WHERE id = ?").bind(bot_id).fetch_optional(&mut **tx).await.map_err(up)?;
    if !kind.as_deref().is_some_and(supported) {
        return Err(unsupported(bot_id));
    }
    Ok(())
}

fn in_flight() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// 背景做 fork 重啟再綁 turn：重啟要等好幾秒，不能卡住 tick 或手動執行的 API。同一筆 occurrence 同時只跑一份。
pub(super) fn spawn_dispatch(app: &Arc<App>, occurrence_id: &str) {
    if !in_flight().lock().unwrap().insert(occurrence_id.to_string()) {
        return;
    }
    let (app, id) = (app.clone(), occurrence_id.to_string());
    tokio::spawn(Box::pin(async move {
        if let Err(e) = dispatch(&app, &id).await {
            tracing::warn!(occurrence = %id, error = ?e, "from-base dispatch failed; ledger keeps it pending");
        }
        in_flight().lock().unwrap().remove(&id);
    }));
}

/// 把一筆 pending occurrence 先 fork 到基準再綁成 turn。回 true＝已綁定。
pub(super) async fn dispatch(app: &Arc<App>, occurrence_id: &str) -> Result<bool> {
    let pool = &app.db;
    let Some(occ) = store::get_occurrence(pool, occurrence_id).await?.filter(|o| o.status == "pending") else { return Ok(false) };
    let Some(sched) = store::get_schedule(pool, &occ.schedule_id).await? else { return Ok(false) };
    let Some(bot) = db::bot(pool, &occ.bot_id).await? else { return Ok(false) };
    if !supported(&bot.kind) {
        fail(app, &sched, &occ, "base_snapshot_unsupported_kind", "基準快照只支援 codex／agy bot", false).await?;
        return Ok(false);
    }
    let Some(snap) = get(pool, &bot.id).await? else {
        if matches!(build_state(&bot.id), Some(("building", _))) {
            set_wait_reason(app, &sched, occurrence_id, "base_building").await;
        } else if let Some(msg) = recent_failure(&bot.id) {
            let why = format!("基準快照建立失敗（冷卻中，不重建），不在舊 context 執行：{msg}");
            fail(app, &sched, &occ, "base_snapshot_create_failed", &why, true).await?;
        } else {
            spawn_ensure(app, &bot.id, &bot.project_id);
            set_wait_reason(app, &sched, occurrence_id, "base_building").await;
        }
        return Ok(false);
    };
    let host = db::bot_host(pool, &bot.id).await?;
    if host == crate::config::LOCAL_HOST && bot.kind == "codex" {
        if let Some(path) = snap.transcript_path.as_deref() {
            if !std::path::Path::new(path).exists() {
                fail(app, &sched, &occ, "base_snapshot_rollout_missing", &format!("基準 session {} 的對話檔已不存在（{path}）", snap.session_id), false).await?;
                return Ok(false);
            }
        }
    }
    let conv = db::conversation_id(pool, &bot.id).await?;
    let busy: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ? AND status IN ('queued','in_flight')").bind(&conv).fetch_one(pool).await?;
    if busy > 0 {
        set_wait_reason(app, &sched, occurrence_id, "bot_busy").await;
        return Ok(false);
    }
    let opts = StartOpts { fork_session: Some(snap.session_id.clone()), fork_source_bot: Some(bot.id.clone()), ..Default::default() };
    let began = db::now();
    let started = if db::active_run(pool, &bot.id).await?.is_some() {
        Box::pin(lifecycle::restart_bot_with(app, &bot.id, StartOpts { require_idle: true, ..opts })).await
    } else {
        Box::pin(lifecycle::start_bot_with(app, &bot.id, opts)).await
    };
    let run_id = match started {
        Ok(r) => { store::mark_started(pool, occurrence_id, &began).await?; r }
        Err(LcError::Conflict(v)) if matches!(v.get("reason").and_then(Value::as_str), Some("not_idle" | "no_longer_idle")) => {
            set_wait_reason(app, &sched, occurrence_id, "bot_busy").await;
            return Ok(false);
        }
        Err(LcError::Conflict(v)) if v.get("reason").and_then(Value::as_str) == Some("base_fork_failed") => {
            let code = if v.get("code").and_then(Value::as_str) == Some(lifecycle::agy_session::SQLITE3_MISSING) { lifecycle::agy_session::SQLITE3_MISSING } else { "base_fork_failed" };
            fail(app, &sched, &occ, code, &format!("agy 對話分岔失敗：{v}"), code == "base_fork_failed").await?;
            return Ok(false);
        }
        Err(e) => {
            fail(app, &sched, &occ, "base_fork_start_failed", &format!("fork 重啟失敗：{e:?}"), true).await?;
            return Ok(false);
        }
    };
    if let Err((code, why)) = wait_running(pool, &run_id).await {
        fail(app, &sched, &occ, code, why, true).await?;
        return Ok(false);
    }
    if bot.kind == "agy" {
        spawn_sweep(app, Some(&bot.id));
    }
    runner::enqueue_now(app, occurrence_id).await
}

async fn set_wait_reason(app: &Arc<App>, sched: &Schedule, occurrence_id: &str, reason: &str) {
    if store::set_waiting_reason(&app.db, occurrence_id, Some(reason), &db::now()).await.unwrap_or(false) {
        app.emit("schedules_changed", json!({"project_id": sched.project_id, "id": sched.id})).await;
    }
}

/// 新 run 進到 running 才算 ready；中途結束（fork 失敗、基準讀不到）或逾時都回錯誤碼。
async fn wait_running(pool: &sqlx::SqlitePool, run_id: &str) -> Result<(), (&'static str, &'static str)> {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT state, ended_at FROM runs WHERE id = ?").bind(run_id).fetch_optional(pool).await.ok().flatten();
        match row {
            Some((state, None)) if state == "running" => return Ok(()),
            Some((_, Some(_))) | None => return Err(("base_fork_exited", "fork 出來的 bot 啟動後就結束了（基準 session 可能讀不到）")),
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(("base_fork_timeout", "fork 重啟後等不到 bot 就緒"));
        }
        tokio::time::sleep(Duration::from_millis(if cfg!(test) { 20 } else { 500 })).await;
    }
}

/// 這次送出前就失敗：記一次 attempt。`retryable` 且排程還有重試額度（P2）就排下一次，否則收成 failed 並走既有失敗通知。
pub(super) async fn fail(app: &Arc<App>, sched: &Schedule, occ: &Occurrence, code: &str, why: &str, retryable: bool) -> Result<()> {
    let now = db::now();
    let error = format!("{code}：{why}");
    let mut tx = db::begin_write(&app.db).await?;
    let Some(cur) = store::get_occurrence_tx(&mut tx, &occ.id).await?.filter(|o| o.status == "pending" && o.attempt_no == occ.attempt_no) else { return Ok(()) };
    let retry = if retryable { store::retry_at(sched, &cur, "failed", &now) } else { None };
    let summary = super::outcome::summary(&error);
    let runner = match (&cur.attempt_started_at, &cur.runner_json) {
        (Some(_), Some(r)) => Some(r.clone()),
        _ => store::runner_snapshot_tx(&mut tx, &cur.id).await?,
    };
    sqlx::query("INSERT INTO schedule_attempts (occurrence_id, attempt_no, turn_id, reply_message_id, outcome, outcome_summary, finished_at, started_at, runner_json) VALUES (?,?,NULL,NULL,'failed',?,?,?,?)")
        .bind(&cur.id).bind(cur.attempt_no).bind(&summary).bind(&now).bind(&cur.attempt_started_at).bind(&runner).execute(&mut *tx).await?;
    sqlx::query("UPDATE schedule_occurrences SET runner_json = ? WHERE id = ?").bind(&runner).bind(&cur.id).execute(&mut *tx).await?;
    if let Some(at) = &retry {
        sqlx::query("UPDATE schedule_occurrences SET error = ?, waiting_reason = NULL, outcome = 'failed', outcome_summary = ?, next_retry_at = ?, attempt_no = attempt_no + 1, attempt_started_at = NULL, updated_at = ? WHERE id = ?")
            .bind(&error).bind(&summary).bind(at).bind(&now).bind(&cur.id).execute(&mut *tx).await?;
    } else {
        store::finish_open_tx(&mut tx, &cur.id, "failed", &error, &now).await?;
    }
    tx.commit().await?;
    if retry.is_none() {
        runner::notify_terminal(app, sched, &cur.id, "failed", Some(&error), cur.attempt_no, cur.source == "dry_run").await;
    } else {
        app.emit("schedules_changed", json!({"project_id": sched.project_id, "id": sched.id})).await;
    }
    Ok(())
}
