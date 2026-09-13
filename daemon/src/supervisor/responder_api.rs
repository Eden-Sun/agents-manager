//! `/api/supervisor/responder/*`：協調者的建立、啟停與人設。docs/API.md。
//!
//! 放在自己的檔案、以 [`routes`] 併進主路由，巡檢的 `/api/supervisor/*` 完全不動。

use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::responder;
use super::roles::{self, Role};

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// 掛在 `/api` 底下、auth layer 之內。
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/supervisor/responder", get(get_responder))
        .route("/supervisor/responder/setup", post(post_setup))
        .route("/supervisor/responder/start", post(post_start))
        .route("/supervisor/responder/stop", post(post_stop))
        .route("/supervisor/responder/persona", get(get_persona).put(put_persona))
}

async fn get_responder(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(responder::status_json(&app).await?))
}

#[derive(Deserialize, Default)]
struct SetupIn {
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

/// 建立環境。冪等、不啟動：跟巡檢的 setup 一樣，要人明確 start 一次。
async fn post_setup(State(app): State<Arc<App>>, body: Option<Json<SetupIn>>) -> Result<Json<Value>, LcError> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let _g = super::lock().await;
    let (_project, bot_id, deployed) =
        responder::ensure_env(&app, b.identity.as_deref(), b.model.as_deref(), b.effort.as_deref()).await?;
    app.emit("supervisor_changed", json!({"responder_bot_id": bot_id})).await;
    let mut out = responder::status_json(&app).await?;
    out["deployed"] = serde_json::to_value(&deployed).unwrap_or(Value::Null);
    Ok(Json(out))
}

async fn post_start(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    responder::start(&app, None).await?;
    roles::set_desired_running(&app.db, Role::Responder, true).await.map_err(up)?;
    Ok(Json(responder::status_json(&app).await?))
}

async fn post_stop(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    responder::stop(&app).await?;
    Ok(Json(responder::status_json(&app).await?))
}

async fn get_persona(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let text = responder::effective_persona(&app).await?;
    let row = roles::get(&app.db, Role::Responder).await.map_err(up)?;
    let embedded = responder::persona_body();
    let embedded_hash = super::persona::hash(&embedded);
    let run_started = match row.bot_id.as_deref() {
        Some(id) => crate::db::active_run(&app.db, id).await.map_err(up)?.map(|r| r.started_at),
        None => None,
    };
    let loaded = super::persona::loaded_state(run_started.as_deref(), row.persona_updated_at.as_deref());
    Ok(Json(json!({
        "role": Role::Responder.as_str(),
        "stored": {
            "version": row.persona_version,
            "hash": row.persona_hash,
            "source": row.persona_source,
            "updated_at": row.persona_updated_at,
            "seeded_from": row.persona_seed_hash,
            "length": text.chars().count(),
            "text": text,
        },
        "embedded": {"hash": embedded_hash, "length": embedded.chars().count()},
        "loaded": {"status": loaded.as_str(), "run_started_at": run_started},
        "upgrade_available": row.persona_seed_hash.as_deref().is_some_and(|h| h != embedded_hash),
        "needs_restart": loaded.needs_restart(),
    })))
}

#[derive(Deserialize)]
struct PersonaIn {
    text: String,
    #[serde(default)]
    expected_version: Option<i64>,
}

async fn put_persona(State(app): State<Arc<App>>, Json(b): Json<PersonaIn>) -> Result<Json<Value>, LcError> {
    let text = b.text.trim();
    if text.is_empty() {
        return Err(LcError::Bad("persona text must not be empty".into()));
    }
    let _g = super::lock().await;
    let version = responder::set_persona(&app, text, b.expected_version).await?;
    responder::apply_persona(&app, text).await.map_err(|e| {
        LcError::conflict(
            "persona stored but projection sync is incomplete; retry the same text to repair",
            json!({"reason": "persona_sync_incomplete", "stored": true, "version": version, "sync_error": format!("{e:?}")}),
        )
    })?;
    drop(_g);
    app.emit("supervisor_changed", json!({"responder_persona_version": version})).await;
    Ok(Json(get_persona(State(app.clone())).await?.0))
}
