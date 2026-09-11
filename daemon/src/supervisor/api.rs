//! `/api/supervisor/*` — the contract the web UI and the `agm` CLI both code against.
//!
//! Every route sits inside the existing `X-AM-Token` auth layer; the supervisor has no
//! privilege of its own, it simply reuses the daemon's local management authority.

use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::{controller, health, setup, store};

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

pub async fn get_supervisor(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::status_json(&app).await?))
}

pub async fn get_health(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(health::snapshot(&app).await?))
}

/// Build the environment. Idempotent, and deliberately does **not** start anything: a manager
/// that comes up by itself before a human has looked at it is how you get a Remote Control
/// session nobody verified.
pub async fn post_setup(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    let (_project_id, bot_id, deployed) = setup::ensure_env(&app).await?;
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    // Bringing the environment back up is a new generation: an old controller (from a config
    // that pointed at a bot which no longer exists) must not keep sending notifications.
    if sup.generation == 0 {
        controller::respawn(&app).await;
    }
    controller::reconcile(&app).await;
    app.emit("supervisor_changed", json!({"bot_id": bot_id})).await;
    let mut out = super::status_json(&app).await?;
    out["deployed"] = serde_json::to_value(&deployed).unwrap_or(Value::Null);
    Ok(Json(out))
}

pub async fn post_start(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    // Choose the configured fallback before launching the CLI, so a low Fable bucket starts
    // directly on Opus instead of briefly opening the wrong session.
    if let Err(e) = controller::auto_switch_if_fable_low(&app).await {
        tracing::warn!(error = ?e, "automatic Fable quota switch during start failed");
    }
    super::start_manager(&app, None).await?;
    // From here on the manager is *supposed* to be up: if it dies, the watchdog brings it back.
    let _ = store::set_desired_running(&app.db, true).await;
    Ok(Json(super::status_json(&app).await?))
}

pub async fn post_stop(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    let bot = super::manager_bot(&app)
        .await?
        .ok_or_else(|| LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})))?;
    // Written *before* the stop: a watchdog tick between the two must not read "wanted, but
    // stopped" and start it straight back up.
    let _ = store::set_desired_running(&app.db, false).await;
    crate::lifecycle::stop_bot(&app, &bot.id).await?;
    // A stopped CLI takes its Remote Control session with it; claiming otherwise would send
    // the user to a dead URL on their phone.
    let _ = store::set_remote(&app.db, "unknown", None).await;
    app.emit("supervisor_changed", json!({"stopped": true})).await;
    Ok(Json(super::status_json(&app).await?))
}

/// Switch to the other fixed candidate. Bounded: see `controller::switch_candidate`.
pub async fn post_fallback(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let _g = super::lock().await;
    let switched = controller::switch_candidate(&app, "requested").await?;
    let mut out = super::status_json(&app).await?;
    out["switched"] = json!(switched);
    Ok(Json(out))
}

pub async fn get_assignments(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows = store::list_assignments(&app.db, 200).await.map_err(up)?;
    Ok(Json(json!({"assignments": rows.iter().map(store::Assignment::to_json).collect::<Vec<_>>()})))
}

#[derive(Deserialize)]
pub struct AssignIn {
    pub target_bot_id: String,
    pub text: String,
    pub client_request_id: String,
    #[serde(default)]
    pub source_turn_id: Option<String>,
}

pub async fn post_assignment(
    State(app): State<Arc<App>>,
    Json(b): Json<AssignIn>,
) -> Result<Json<Value>, LcError> {
    let a = super::assign(&app, &b.target_bot_id, &b.text, &b.client_request_id, b.source_turn_id.as_deref()).await?;
    Ok(Json(a))
}

pub async fn get_handoff(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    let assignments = store::list_assignments(&app.db, 200).await.map_err(up)?;
    let open = assignments.iter().filter(|a| a.is_open()).count();
    Ok(Json(json!({
        "summary": sup.summary,
        "summary_version": sup.summary_version,
        "updated_at": sup.updated_at,
        "requests": store::requests(&app.db, 100).await.map_err(up)?,
        "assignments": assignments.iter().map(store::Assignment::to_json).collect::<Vec<_>>(),
        "inbox": store::inbox(&app.db, 100).await.map_err(up)?.iter().map(store::InboxEvent::to_json).collect::<Vec<_>>(),
        "open_assignments": open,
        "pending_count": store::pending_count(&app.db).await.map_err(up)?,
    })))
}

#[derive(Deserialize)]
pub struct HandoffIn {
    pub summary: String,
}

pub async fn put_handoff(State(app): State<Arc<App>>, Json(b): Json<HandoffIn>) -> Result<Json<Value>, LcError> {
    let version = store::set_summary(&app.db, &b.summary).await.map_err(up)?;
    // The readable copy in the manager's own directory. The database stays authoritative;
    // this is what the manager reads on a cold start before anything else is available.
    let sup = store::get_or_init(&app.db).await.map_err(up)?;
    if let Some(cwd) = sup.cwd.as_deref() {
        let _ = std::fs::write(
            std::path::Path::new(cwd).join("handoff.md"),
            format!("# AGM 管理摘要（v{version}）\n\n{}\n", b.summary),
        );
    }
    app.emit("supervisor_changed", json!({"summary_version": version})).await;
    Ok(Json(json!({"summary": b.summary, "summary_version": version})))
}

#[derive(Deserialize, Default)]
pub struct InboxQuery {
    /// `all=1`: include handled events too (newest first, the audit view). Default is the
    /// work view: only what is still open, oldest first.
    #[serde(default)]
    pub all: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn get_inbox(
    State(app): State<Arc<App>>,
    Query(q): Query<InboxQuery>,
) -> Result<Json<Value>, LcError> {
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let all = q.all.as_deref().is_some_and(|v| matches!(v, "1" | "true" | "yes"));
    let events = if all {
        store::inbox(&app.db, limit).await.map_err(up)?
    } else {
        store::open_inbox(&app.db, limit).await.map_err(up)?
    };
    Ok(Json(json!({
        "events": events.iter().map(store::InboxEvent::to_json).collect::<Vec<_>>(),
        "open": store::open_inbox_count(&app.db).await.map_err(up)?,
        "all": all,
        "limit": limit,
    })))
}

pub async fn post_inbox_ack(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    if !store::ack_inbox(&app.db, &id).await.map_err(up)? {
        return Err(LcError::NotFound("inbox event".into()));
    }
    app.emit("supervisor_changed", json!({"acked": id})).await;
    Ok(Json(json!({})))
}

pub async fn get_sanitized_state(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(super::sanitized_state(&app).await?))
}
