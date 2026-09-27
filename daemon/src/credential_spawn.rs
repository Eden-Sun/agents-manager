//! Credential inheritance fences for herdr pane / child-agent creation.
//!
//! A bot's already-running CLI can create panes outside the daemon's SQLite transaction. The shim
//! therefore reserves a spawn permit before invoking herdr and reports the resulting pane before
//! releasing it. Rotation sets a short-lived fence in the same registry before checking descendants;
//! permits already in flight make rotation fail closed, and new permits are refused until the token
//! update commits or the rotation aborts.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::App;

#[derive(Default)]
pub struct Gate {
    rotating: HashSet<String>,
    permits: HashMap<String, HashSet<String>>,
}

impl Gate {
    fn begin_rotation(&mut self, bot_id: &str) -> Result<(), FenceError> {
        if !self.rotating.insert(bot_id.to_string()) {
            return Err(FenceError::AlreadyRotating);
        }
        let in_flight = self.permits.get(bot_id).map_or(0, HashSet::len);
        if in_flight > 0 {
            self.rotating.remove(bot_id);
            return Err(FenceError::SpawnsInFlight(in_flight));
        }
        Ok(())
    }

    fn reserve(&mut self, bot_id: &str, permit_id: &str) -> Result<(), ()> {
        if self.rotating.contains(bot_id) {
            return Err(());
        }
        self.permits.entry(bot_id.to_string()).or_default().insert(permit_id.to_string());
        Ok(())
    }

    fn permit_active(&self, bot_id: &str, permit_id: &str) -> bool {
        self.permits.get(bot_id).is_some_and(|permits| permits.contains(permit_id))
    }

    fn release(&mut self, bot_id: &str, permit_id: &str) -> bool {
        let Some(permits) = self.permits.get_mut(bot_id) else { return false };
        let removed = permits.remove(permit_id);
        if permits.is_empty() {
            self.permits.remove(bot_id);
        }
        removed
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FenceError {
    AlreadyRotating,
    SpawnsInFlight(usize),
}

/// An RAII rotation fence: every pre-commit return clears the fence, while the token update keeps
/// new pane creation closed until its database transaction has committed.
pub struct RotationFence {
    app: Arc<App>,
    bot_id: String,
}

impl RotationFence {
    pub fn begin(app: &Arc<App>, bot_id: &str) -> Result<Self, FenceError> {
        let mut gate = app.credential_spawn_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        gate.begin_rotation(bot_id)?;
        Ok(Self { app: app.clone(), bot_id: bot_id.to_string() })
    }

    pub fn committed(self) {
        self.clear();
    }

    fn clear(&self) {
        self.app.credential_spawn_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).rotating.remove(&self.bot_id);
    }
}

impl Drop for RotationFence {
    fn drop(&mut self) {
        self.clear();
    }
}

fn reserve(app: &Arc<App>, bot_id: &str, permit_id: &str) -> Result<(), ()> {
    let mut gate = app.credential_spawn_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    gate.reserve(bot_id, permit_id)
}

fn permit_active(app: &Arc<App>, bot_id: &str, permit_id: &str) -> bool {
    app.credential_spawn_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .permit_active(bot_id, permit_id)
}

fn release(app: &Arc<App>, bot_id: &str, permit_id: &str) -> bool {
    app.credential_spawn_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).release(bot_id, permit_id)
}

#[derive(Deserialize)]
pub struct SpawnBegin {
    bot_id: String,
}

#[derive(Deserialize)]
pub struct SpawnFinish {
    bot_id: String,
    permit_id: String,
    pane_id: String,
    #[serde(default)]
    purpose: String,
}

fn fail(status: StatusCode, reason: &str, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message, "reason": reason})))
}

async fn authenticated_bot(app: &Arc<App>, bot_id: &str, token: &str) -> Result<crate::db::Bot, (StatusCode, Json<Value>)> {
    match crate::db::bot(&app.db, bot_id).await {
        Ok(Some(bot)) if bot.deleted_at.is_none() && !token.is_empty() && crate::api::ct_eq(token, &bot.hook_token) => Ok(bot),
        Ok(_) => Err(fail(StatusCode::UNAUTHORIZED, "bad_bot_proof", "unknown bot or bad token")),
        Err(e) => {
            tracing::warn!(bot = bot_id, error = %e, "credential spawn could not read bot proof");
            Err(fail(StatusCode::SERVICE_UNAVAILABLE, "bot_unreadable", "bot credential could not be checked; spawn refused"))
        }
    }
}

/// `POST /relay/spawn/begin`: reserve the parent's credential while a shim invokes herdr.
pub async fn begin(State(app): State<Arc<App>>, headers: axum::http::HeaderMap, axum::extract::Form(body): axum::extract::Form<SpawnBegin>) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if let Err(response) = authenticated_bot(&app, &body.bot_id, token).await {
        return response;
    }
    let permit_id = crate::db::ulid();
    if reserve(&app, &body.bot_id, &permit_id).is_err() {
        return fail(StatusCode::CONFLICT, "credential_rotation_pending", "credential rotation is checking child panes; spawn refused");
    }
    // Close the read-before-reserve window: a rotation may have committed between the first token
    // read and reserve(), after which its fence is already down and only this second read notices.
    if let Err(response) = authenticated_bot(&app, &body.bot_id, token).await {
        release(&app, &body.bot_id, &permit_id);
        return response;
    }
    (StatusCode::OK, Json(json!({"permit_id": permit_id})))
}

/// `POST /relay/spawn/finish`: record the inherited-credential pane before dropping its permit.
pub async fn finish(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    axum::extract::Form(body): axum::extract::Form<SpawnFinish>,
) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let bot = match authenticated_bot(&app, &body.bot_id, token).await {
        Ok(bot) => bot,
        Err(response) => return response,
    };
    if !permit_active(&app, &body.bot_id, &body.permit_id) {
        return fail(StatusCode::CONFLICT, "spawn_permit_missing", "spawn permit is missing; rotation safety is unknown");
    }
    if body.pane_id.trim().is_empty() {
        return fail(StatusCode::BAD_REQUEST, "pane_id_missing", "herdr did not identify the created pane; spawn remains fenced");
    }
    let host = match crate::db::bot_host(&app.db, &bot.id).await {
        Ok(host) => host,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "credential spawn could not read bot host");
            return fail(StatusCode::SERVICE_UNAVAILABLE, "host_unreadable", "bot host could not be checked; spawn remains fenced");
        }
    };
    let parent_pane = match crate::db::active_run(&app.db, &bot.id).await {
        Ok(run) => run.and_then(|r| r.pane_id),
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "credential spawn could not read active pane");
            return fail(StatusCode::SERVICE_UNAVAILABLE, "run_unreadable", "parent pane could not be checked; spawn remains fenced");
        }
    };
    if parent_pane.as_deref() != Some(body.pane_id.as_str()) {
        if let Err(e) = crate::panes::note_purpose(&app, &host, &body.pane_id, &bot, body.purpose.trim()).await {
            tracing::warn!(bot = %bot.id, pane = %body.pane_id, error = %e, "credential-bearing pane could not be registered");
            return fail(StatusCode::SERVICE_UNAVAILABLE, "pane_registration_failed", "created pane could not be registered; spawn remains fenced");
        }
    }
    if !release(&app, &body.bot_id, &body.permit_id) {
        return fail(StatusCode::CONFLICT, "spawn_permit_missing", "spawn permit changed while registering the pane");
    }
    (StatusCode::OK, Json(json!({"pane_id": body.pane_id, "registered": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_rotation_refuses_new_spawns_and_an_existing_spawn_refuses_rotation() {
        let mut gate = Gate::default();
        let bot = "gate-test";
        gate.reserve(bot, "before-rotation").unwrap();
        assert_eq!(gate.begin_rotation(bot).unwrap_err(), FenceError::SpawnsInFlight(1));
        assert!(gate.reserve(bot, "during-failed-rotation").is_ok(), "failed fence setup must be cleared");
        gate.release(bot, "before-rotation");
        gate.release(bot, "during-failed-rotation");

        gate.begin_rotation(bot).unwrap();
        assert!(gate.reserve(bot, "during-rotation").is_err());
        gate.rotating.remove(bot);
        assert!(gate.reserve(bot, "after-aborted-rotation").is_ok());
    }
}
