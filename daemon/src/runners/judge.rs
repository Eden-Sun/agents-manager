//! `judge` runner：shadow limit hit、stuck sweep、shadow settled 與 HTTP 管理端點。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::capabilities::{BgTasks, Shutdown};
use crate::judge::{key_status, stage_key, Sample};
use crate::lc_error::LcError;
use crate::state::App;

// ───────────── Shadow Limit Hit ─────────────

/// 鎖外去問：問答本身丟背景，呼叫端（畫面處理）不等它。
pub async fn shadow_limit_hit(app: &Arc<App>, sample: Sample) {
    if !app.cfg.get().await.judge.enabled {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::judge::observe(&app, sample).await {
            tracing::debug!(error = %e, "judge shadow skipped");
        }
    });
}

// ───────────── Shadow Settled ─────────────

/// 結案寫進 `result` 之後呼叫。**先看開關再丟背景**（#480）：HTTP 不在 tick 裡等。
pub async fn shadow_settled(
    app: &Arc<App>,
    assignment_id: &str,
    bot_id: &str,
    turn_id: Option<&str>,
    turn_status: &str,
    result: Option<&str>,
) {
    if turn_status != "completed" {
        return;
    }
    let Some(report) = result.map(str::trim).filter(|s| !s.is_empty()) else { return };
    if !app.cfg.get().await.judge.enabled {
        return;
    }
    let sample = crate::judge::report::Sample {
        assignment_id: assignment_id.to_string(),
        bot_id: bot_id.to_string(),
        turn_id: turn_id.map(str::to_string),
        report: report.to_string(),
    };
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::judge::report::observe(&app, &sample).await {
            tracing::debug!(error = %e, assignment = %sample.assignment_id, "judge report shadow skipped");
        }
    });
}

// ───────────── Stuck Sweep ─────────────

/// 一次只准一個 sweep 在跑。
pub(crate) static SWEEPING: AtomicBool = AtomicBool::new(false);

pub(crate) const MAX_PER_ROUND: usize = 10;
pub(crate) const SEEN_COOLDOWN: Duration = Duration::from_secs(120);

/// 控制迴圈每拍呼叫。**丟背景跑**（issue #480）。
pub fn sweep(app: &Arc<App>) {
    // 測試裡 controller 每拍都會經過這裡；`SWEEPING` 是行程全域旗標，會跟直接拿 `SweepGuard` 的測試互撞（issue #947）。
    // 同 `idle_sleep::sweep`／`primary_keep_warm`：測試不跑背景 sweep，要驗的測試自己呼叫 `sweep_once`。
    if cfg!(test) || app.shutdown.is_cancelled() {
        return;
    }
    let Some(guard) = SweepGuard::take() else {
        tracing::debug!("judge stuck sweep: 上一輪還在跑，這一拍跳過");
        return;
    };
    let round_app = app.clone();
    spawn_sweep_task(app, guard, async move { sweep_once(&round_app).await });
}

pub(crate) fn spawn_sweep_task<F>(app: &(impl BgTasks + Shutdown), guard: SweepGuard, round: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if app.shutdown().is_cancelled() {
        drop(guard);
        return;
    }
    let shutdown = app.shutdown().clone();
    let tasks = app.background_tasks().clone();
    tasks.spawn(async move {
        let _guard = guard;
        tokio::select! {
            _ = shutdown.cancelled() => {}
            _ = round => {}
        }
    });
}

pub(crate) struct SweepGuard;

impl SweepGuard {
    pub(crate) fn take() -> Option<Self> {
        (!SWEEPING.swap(true, Ordering::SeqCst)).then_some(SweepGuard)
    }
}

impl Drop for SweepGuard {
    fn drop(&mut self) {
        SWEEPING.store(false, Ordering::SeqCst);
    }
}

/// 一輪的本體。
pub async fn sweep_once(app: &Arc<App>) {
    let cfg = app.cfg.get().await.judge;
    if !cfg.enabled {
        return;
    }
    let cands = match crate::judge::stuck::candidates(app, crate::judge::stuck::STUCK_AFTER_SECS).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "judge stuck sweep: cannot list queued turns");
            return;
        }
    };
    let now = Instant::now();
    let due: Vec<crate::judge::stuck::Stuck> = {
        let mut seen = app.judge_stuck_seen.lock().await;
        seen.retain(|_, t| now.duration_since(*t) < SEEN_COOLDOWN);
        let picked: Vec<crate::judge::stuck::Stuck> = cands.into_iter().filter(|c| !seen.contains_key(&c.turn_id)).take(MAX_PER_ROUND).collect();
        for c in &picked {
            seen.insert(c.turn_id.clone(), now);
        }
        picked
    };
    for c in due {
        if let Err(e) = crate::judge::stuck::inspect(app, &c).await {
            tracing::debug!(bot = %c.bot_id, error = %e, "judge stuck sweep skipped");
        }
    }
}

// ───────────── HTTP Endpoints ─────────────

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/judge/shadow", get(get_shadow))
        .route("/judge/settings", get(get_settings).put(put_settings))
}

#[derive(Deserialize)]
struct ShadowQuery {
    limit: Option<i64>,
}

async fn get_shadow(State(app): State<Arc<App>>, Query(q): Query<ShadowQuery>) -> Result<Json<Value>, LcError> {
    let cfg = app.cfg.get().await.judge;
    let rows = sqlx::query("SELECT * FROM judge_shadow ORDER BY at DESC LIMIT ?")
        .bind(q.limit.unwrap_or(200).clamp(1, 1000))
        .fetch_all(&app.db)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    let rows: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String, _>("id"),
                "at": r.get::<String, _>("at"),
                "bot_id": r.get::<String, _>("bot_id"),
                "run_id": r.get::<String, _>("run_id"),
                "kind": r.get::<String, _>("kind"),
                "matched_line": r.get::<String, _>("matched_line"),
                "composer_idle": r.get::<bool, _>("composer_idle"),
                "regex_verdict": r.get::<String, _>("regex_verdict"),
                "jev_is_live_ui": r.get::<Option<f64>, _>("jev_is_live_ui"),
                "jev_same_work": r.get::<Option<f64>, _>("jev_same_work"),
                "model": r.get::<Option<String>, _>("model"),
                "ms": r.get::<Option<i64>, _>("ms"),
                "input_tokens": r.get::<Option<i64>, _>("input_tokens"),
                "error": r.get::<Option<String>, _>("error"),
                "cleared_at": r.get::<Option<String>, _>("cleared_at"),
                "assignment_id": r.get::<Option<String>, _>("assignment_id"),
                "claims_verified": r.get::<Option<f64>, _>("claims_verified"),
                "asks_parent_action": r.get::<Option<f64>, _>("asks_parent_action"),
            })
        })
        .collect();
    Ok(Json(json!({"enabled": cfg.enabled, "projects": cfg.projects, "model": cfg.model, "rows": rows})))
}

async fn settings_json(app: &impl crate::capabilities::Cfg) -> Value {
    let cfg = app.cfg().get().await.judge;
    let key = key_status(&cfg.key_file);
    json!({"enabled": cfg.enabled, "projects": cfg.projects, "model": cfg.model, "key_present": key.is_ok(), "key_error": key.err()})
}

pub(crate) async fn get_settings(State(app): State<Arc<App>>) -> Json<Value> {
    Json(settings_json(&app).await)
}

#[derive(Deserialize)]
pub(crate) struct SettingsBody {
    pub enabled: Option<bool>,
    pub projects: Option<Vec<String>>,
    pub token: Option<String>,
}

pub(crate) async fn put_settings(State(app): State<Arc<App>>, Json(body): Json<SettingsBody>) -> Result<Json<Value>, LcError> {
    let key_file = app.cfg.get().await.judge.key_file;
    let staged = match body.token.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(token) => Some(stage_key(&key_file, token).map_err(|e| LcError::Bad(e.to_string()))?),
        None => None,
    };
    if body.enabled == Some(true) {
        let checked = match &staged {
            Some(s) => key_status(&s.staged_path().to_string_lossy()),
            None => key_status(&key_file),
        };
        if let Err(reason) = checked {
            return Err(LcError::Conflict(json!({"error": "needs_key", "reason": reason})));
        }
    }
    let projects = body.projects.map(|ps| {
        let mut ps: Vec<String> = ps.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
        ps.sort();
        ps.dedup();
        ps
    });
    let prev = app.cfg.get().await.judge;
    app.cfg
        .update(move |c| {
            if let Some(enabled) = body.enabled {
                c.judge.enabled = enabled;
            }
            if let Some(projects) = projects {
                c.judge.projects = projects;
            }
            Ok(())
        })
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    if let Some(staged) = staged {
        if let Err(e) = staged.publish() {
            let _ = app
                .cfg
                .update(move |c| {
                    c.judge.enabled = prev.enabled;
                    c.judge.projects = prev.projects;
                    Ok(())
                })
                .await;
            return Err(LcError::Upstream(format!("key publish failed: {e}")));
        }
    }
    Ok(Json(settings_json(&app).await))
}
