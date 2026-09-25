//! `GET /api/judge/shadow`（issue #240）：shadow 帳本，唯讀。docs/API.md。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::lifecycle::LcError;
use crate::state::App;

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/judge/shadow", get(get_shadow)).route("/judge/settings", get(get_settings).put(put_settings))
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
            })
        })
        .collect();
    // key 與 key_file 的內容都不回；只回開關，讓人看得出為什麼是空的。
    Ok(Json(json!({"enabled": cfg.enabled, "projects": cfg.projects, "model": cfg.model, "rows": rows})))
}

async fn settings_json(app: &Arc<App>) -> Value {
    let cfg = app.cfg.get().await.judge;
    let key = super::key_status(&cfg.key_file);
    // key 永遠不回：只說有沒有、不能用的話為什麼。
    json!({"enabled": cfg.enabled, "projects": cfg.projects, "model": cfg.model, "key_present": key.is_ok(), "key_error": key.err()})
}

pub(super) async fn get_settings(State(app): State<Arc<App>>) -> Json<Value> {
    Json(settings_json(&app).await)
}

#[derive(Deserialize)]
pub(super) struct SettingsBody {
    pub enabled: Option<bool>,
    pub projects: Option<Vec<String>>,
    /// 貼進來的 API key；省略或空字串＝不動現有的。
    pub token: Option<String>,
}

/// 網頁「環境設定」存檔。改完當下生效（每次要問之前才讀設定與 key 檔），不用重啟。
pub(super) async fn put_settings(State(app): State<Arc<App>>, Json(body): Json<SettingsBody>) -> Result<Json<Value>, LcError> {
    let key_file = app.cfg.get().await.judge.key_file;
    // token 只先暫存（0600 暫存檔），config 寫成功之後才換掉現行的 key；config 失敗就整個丟掉，
    // 不會有「回 5xx、新 key 卻已生效」的半套（issue #342）。
    let staged = match body.token.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(token) => Some(super::stage_key(&key_file, token).map_err(|e| LcError::Bad(e.to_string()))?),
        None => None,
    };
    // 沒有可用的 key 就不給開：開了也只會每次記一筆 error。有新 key 時檢查的是新 key（暫存檔）。
    if body.enabled == Some(true) {
        let checked = match &staged {
            Some(s) => super::key_status(&s.staged_path().to_string_lossy()),
            None => super::key_status(&key_file),
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
            // config 已經寫了但 key 換不上去：把開關與專案範圍退回原值，不留「開著卻沒新 key」。
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
