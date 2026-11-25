//! codex／agy bot 的「基準快照」與排程「每次從基準快照開始」。
//!
//! 基準快照＝把某一刻 codex bot 的 native session id 鎖下來（`bot_base_snapshots`）。session id 沿用
//! `db::latest_native_session_id`（hook 記在 `runs.native_session_id`，`agm hire --fork-from` 同一個來源）。
//! 排程勾了 `from_base`：runner 送出前等 bot 閒置，用 `StartOpts.fork_session`（＝codex `fork <session-id>`，
//! 同 `agm hire --fork-from`／`POST /bots/:id/fork` 的啟動路徑）把 bot 重啟成基準的一份新 session，再送排程指示。
//! codex fork 只會產生新 session，基準 rollout 不會被改；基準缺失就 failed，不退回在舊 context 執行。
//! agy 沒有原生 fork：鎖基準時把對話複成一份凍結的複本（`agy_session::clone_conversation`），基準＝複本的對話 id；
//! 執行時再從複本複出新對話、`--conversation=<新 id>` 接續（`start.rs` 的 fork 分支），複本與基準都不會被改。
//! 勾 `from_base` 時目標 bot 沒有基準：[`ensure`] 先自動製作（有 session 就鎖，沒有就啟動 bot 取得 session 再鎖）。

use super::runner;
use super::store::{self, Occurrence, Schedule};
use crate::db;
use crate::lifecycle::{self, LcError, StartOpts};
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
    let v = get(&app.db, &id).await.map_err(up)?.map_or(json!({"locked": false}), |s| view(&s));
    Ok((StatusCode::OK, Json(v)).into_response())
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
        session_id = lifecycle::agy_session::clone_conversation(app.as_ref(), &host, bot.identity.as_deref(), &session_id)
            .await
            .map_err(|e| LcError::conflict("base_snapshot_copy_failed", json!({"reason": "base_snapshot_copy_failed", "bot_id": bot.id, "cause": e.to_string()})))?;
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
    get(&app.db, &bot.id).await.map_err(up)?.ok_or_else(|| up("snapshot vanished"))
}

/// 勾 `from_base` 前：目標 bot 沒有基準就自動製作（不回 400）。有 native session 就鎖；完全沒有就照正常方式啟動一次、
/// 等它回報 session 再鎖。bot 不合格（不是這個專案可接排程的 bot）時什麼都不做，由後續檢查回對的錯。
pub(super) async fn ensure(app: &Arc<App>, bot_id: &str, project_id: &str) -> Result<(), LcError> {
    if get(&app.db, bot_id).await.map_err(up)?.is_some() {
        return Ok(());
    }
    {
        let mut tx = app.db.begin().await.map_err(up)?;
        if runner::eligible_bot_tx(&mut tx, bot_id, Some(project_id)).await.map_err(up)?.is_none() {
            return Ok(());
        }
    }
    let Some(bot) = db::bot(&app.db, bot_id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()) else { return Ok(()) };
    if !supported(&bot.kind) {
        return Err(unsupported(bot_id));
    }
    let failed = |why: String| LcError::conflict("base_snapshot_create_failed", json!({"reason": "base_snapshot_create_failed", "bot_id": bot_id, "message": format!("找不到 {} 的基準快照，自動製作也失敗：{why}", bot.name)}));
    if latest_session(&app.db, bot_id).await?.is_none() {
        if db::active_run(&app.db, bot_id).await.map_err(up)?.is_none() {
            Box::pin(lifecycle::start_bot_with(app, bot_id, StartOpts::default())).await.map_err(|e| failed(format!("啟動失敗（{e:?}）")))?;
        }
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        while latest_session(&app.db, bot_id).await?.is_none() {
            if tokio::time::Instant::now() >= deadline {
                return Err(failed("等不到 bot 回報對話 session".into()));
            }
            tokio::time::sleep(Duration::from_millis(if cfg!(test) { 20 } else { 500 })).await;
        }
    }
    lock_latest(app, &bot, "schedule").await.map(|_| ())
}

/// `DELETE /api/bots/{id}/base-snapshot`：清除基準（沒有也回成功）。仍勾著 `from_base` 的排程之後會 failed（`base_snapshot_missing`）。
pub async fn clear(State(app): State<Arc<App>>, Path(id): Path<String>, Extension(p): Extension<crate::api::RequestPrincipal>) -> Result<Response, LcError> {
    authorize(&app, &p, &id).await?;
    sqlx::query("DELETE FROM bot_base_snapshots WHERE bot_id = ?").bind(&id).execute(&app.db).await.map_err(up)?;
    Ok((StatusCode::OK, Json(json!({"locked": false}))).into_response())
}

/// 勾 `from_base` 的前提：bot 是 codex／agy 且已鎖基準（交易內查，不向池子再要連線；沒有基準由呼叫端先 [`ensure`]）。
pub(super) async fn require_tx(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, bot_id: &str) -> Result<(), LcError> {
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM bots WHERE id = ?").bind(bot_id).fetch_optional(&mut **tx).await.map_err(up)?;
    if !kind.as_deref().is_some_and(supported) {
        return Err(unsupported(bot_id));
    }
    let has: Option<i64> = sqlx::query_scalar("SELECT 1 FROM bot_base_snapshots WHERE bot_id = ?").bind(bot_id).fetch_optional(&mut **tx).await.map_err(up)?;
    if has.is_none() {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "base_snapshot_required", "bot_id": bot_id})));
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
        fail(app, &sched, &occ, "base_snapshot_missing", "這顆 bot 沒有基準快照（可能已被清除），不在舊 context 執行", false).await?;
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
        wait_busy(app, &sched, occurrence_id).await;
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
            wait_busy(app, &sched, occurrence_id).await;
            return Ok(false);
        }
        Err(LcError::Conflict(v)) if v.get("reason").and_then(Value::as_str) == Some("base_fork_failed") => {
            fail(app, &sched, &occ, "base_fork_failed", &format!("agy 對話分岔失敗：{v}"), true).await?;
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
    runner::enqueue_now(app, occurrence_id).await
}

async fn wait_busy(app: &Arc<App>, sched: &Schedule, occurrence_id: &str) {
    if store::set_waiting_reason(&app.db, occurrence_id, Some("bot_busy"), &db::now()).await.unwrap_or(false) {
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
    sqlx::query("INSERT INTO schedule_attempts (occurrence_id, attempt_no, turn_id, reply_message_id, outcome, outcome_summary, finished_at, started_at) VALUES (?,?,NULL,NULL,'failed',?,?,?)")
        .bind(&cur.id).bind(cur.attempt_no).bind(&summary).bind(&now).bind(&cur.attempt_started_at).execute(&mut *tx).await?;
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
