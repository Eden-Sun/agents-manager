//! `judge` runner：shadow limit hit、stuck sweep、shadow settled 與 HTTP 管理端點。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
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
    tokio::spawn(Box::pin(async move {
        if let Err(e) = crate::judge::observe(&app, sample).await {
            tracing::debug!(error = %e, "judge shadow skipped");
        }
    }));
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
    tokio::spawn(Box::pin(async move {
        if let Err(e) = crate::judge::report::observe(&app, &sample).await {
            tracing::debug!(error = %e, assignment = %sample.assignment_id, "judge report shadow skipped");
        }
    }));
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
    crate::background_loop::spawn_named(&tasks, "judge task", async move {
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
        .route("/judge/collision-check", post(post_collision_check))
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

// ───────────── Claim Collision Check（#1330）─────────────

/// 題目與分支名的上限（字數）。
const MAX_CLAIM_TEXT_CHARS: usize = 500;

#[derive(Deserialize)]
struct CollisionIn {
    #[serde(default)]
    issue: Option<u64>,
    title: String,
    child: String,
    #[serde(default)]
    branch: Option<String>,
}

/// 呼叫端的 bot 列讀不到（#1384）：404 `what` 是資源名 `bot`，那句話放 message。
fn bot_not_found(caller: &str) -> LcError {
    LcError::NotFoundValue(json!({"error": "not_found", "what": "bot", "message": format!("bot {caller}")}))
}

/// `POST /api/judge/collision-check`：bot 認領票時問一次撞題（只記帳本、推提示，不擋認領）。回 202，不等 Jev。
/// 只看呼叫端自己的專案：專案從驗過的 bot id 查，不從請求本文拿。
async fn post_collision_check(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<CollisionIn>) -> Result<(StatusCode, Json<Value>), LcError> {
    let Some(caller) = crate::supervisor::bot_requests::verified_bot_id(&app, &headers).await? else {
        return Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "bot_only",
            "message": "撞題檢查只給 bot 自己用（要帶 X-AM-Bot-Id 與它的 token）",
        })));
    };
    let title = b.title.trim().to_string();
    let child = b.child.trim().to_string();
    let branch = b.branch.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if title.is_empty() || child.is_empty() {
        return Err(LcError::Bad("title 與 child 不能是空的".into()));
    }
    for (label, value) in [("title", title.as_str()), ("child", child.as_str()), ("branch", branch.as_deref().unwrap_or(""))] {
        if value.chars().count() > MAX_CLAIM_TEXT_CHARS {
            return Err(LcError::Bad(format!("{label} 最多 {MAX_CLAIM_TEXT_CHARS} 字")));
        }
    }
    let bot = crate::db::bot(&app.db, &caller)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .ok_or_else(|| bot_not_found(&caller))?;
    crate::judge::collision::schedule_claim(
        &app,
        crate::judge::collision::ClaimAsk { project_id: bot.project_id, caller, issue: b.issue, title, child, branch },
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(json!({"queued": true}))))
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
        .map_err(crate::api::cfg_err)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use axum::http::HeaderMap;

    fn body(title: &str) -> CollisionIn {
        CollisionIn { issue: Some(1330), title: title.into(), child: "newbie".into(), branch: Some("fix/newbie-g1330".into()) }
    }

    /// #1384：bot 列讀不到＝404 `what:"bot"`（資源名，同 API.md §1），那句話放 message。
    #[tokio::test]
    async fn a_vanished_bot_is_404_bot_with_the_sentence_in_message() {
        use axum::response::IntoResponse;
        let resp = bot_not_found("01MGHOSTBOT").into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(v["what"], "bot", "{v}");
        assert!(v["message"].as_str().unwrap_or_default().contains("01MGHOSTBOT"), "{v}");
    }

    fn bot_headers(id: &str, token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Id", id.parse().unwrap());
        h.insert("X-AM-Bot-Token", token.parse().unwrap());
        h
    }

    /// `*` is a persisted all-projects selector, not a project id that must exist.
    #[tokio::test]
    async fn wildcard_project_setting_round_trips_through_put_and_get() {
        let env = tt::env().await;
        let Json(saved) = put_settings(
            State(env.app.clone()),
            Json(SettingsBody { enabled: Some(false), projects: Some(vec![" * ".into(), "*".into(), " ".into()]), token: None }),
        )
        .await
        .unwrap();

        assert_eq!(saved["projects"], json!(["*"]));
        assert_eq!(get_settings(State(env.app.clone())).await.0["projects"], json!(["*"]));
        assert_eq!(env.app.cfg.get().await.judge.projects, vec!["*"]);
    }

    /// 只有驗過的 bot 能問：沒帶身分、或帶了對不上的 token，都在看 body 之前就擋下（#1330）。
    #[tokio::test]
    async fn the_collision_check_is_only_for_a_proven_bot() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "newbie").await;
        let none = post_collision_check(State(env.app.clone()), HeaderMap::new(), Json(body("修卡住的畫面判斷"))).await;
        assert!(matches!(none, Err(LcError::Forbidden(_))), "沒帶身分要 403");
        let forged = post_collision_check(State(env.app.clone()), bot_headers(&bot.id, "not-the-token"), Json(body("修卡住的畫面判斷"))).await;
        assert!(forged.is_err(), "token 對不上不能靠 bot id 冒用");
    }

    /// 驗過的 bot 得到 202、不等 Jev；Jev 沒開（預設）就什麼都不寫進帳本。
    #[tokio::test]
    async fn a_proven_bot_is_accepted_and_nothing_is_recorded_while_jev_is_off() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "newbie").await;
        let (status, Json(v)) = post_collision_check(State(env.app.clone()), bot_headers(&bot.id, &bot.hook_token), Json(body("修卡住的畫面判斷")))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(v["queued"], true);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(rows, 0);
    }

    /// 空的題目、或超過上限的字數，在排進任何工作之前就回 400。
    #[tokio::test]
    async fn bad_bodies_are_refused_before_anything_is_queued() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "newbie").await;
        let h = || bot_headers(&bot.id, &bot.hook_token);
        let blank = post_collision_check(State(env.app.clone()), h(), Json(body("   "))).await;
        assert!(matches!(blank, Err(LcError::Bad(_))), "空題目要 400");
        let long = "字".repeat(MAX_CLAIM_TEXT_CHARS + 1);
        let too_long = post_collision_check(State(env.app.clone()), h(), Json(body(&long))).await;
        assert!(matches!(too_long, Err(LcError::Bad(_))), "超過上限要 400");
    }
}
