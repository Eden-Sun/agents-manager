//! `credential_spawn` runner 與 API handlers。

use std::sync::Arc;
use std::time::Duration;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::credential_spawn::{
    permit_ttl, FenceError, FinishClaimError,
};
use crate::state::App;

fn fail(status: StatusCode, reason: &str, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message, "reason": reason})))
}

async fn authenticated_bot(app: &impl crate::capabilities::Db, bot_id: &str, token: &str) -> Result<crate::db::Bot, (StatusCode, Json<Value>)> {
    match crate::db::bot(app.db(), bot_id).await {
        Ok(Some(bot)) if bot.deleted_at.is_none() && !token.is_empty() && crate::agent_relay::ct_eq(token, &bot.hook_token) => {
            // 分享用的受限 bot 不能開子 agent（SPEC「分享 bot」）。
            if crate::db::refuses_bot_principal(app.db(), &bot.id).await {
                return Err(fail(StatusCode::FORBIDDEN, "restricted_bot", "a restricted share bot cannot spawn panes"));
            }
            Ok(bot)
        }
        Ok(_) => Err(fail(StatusCode::UNAUTHORIZED, "bad_bot_proof", "unknown bot or bad token")),
        Err(e) => {
            tracing::warn!(bot = bot_id, error = %e, "credential spawn could not read bot proof");
            Err(fail(StatusCode::SERVICE_UNAVAILABLE, "bot_unreadable", "bot credential could not be checked; spawn refused"))
        }
    }
}

/// Drop resets a failed or cancelled `finish` so the same pane can be retried, while keeping the
/// pane binding. A successful finish consumes the permit atomically after recording the pane.
struct FinishClaim {
    app: Arc<App>,
    bot_id: String,
    permit_id: String,
    pane_id: String,
    completed: bool,
}

impl FinishClaim {
    fn begin(app: &Arc<App>, bot_id: &str, permit_id: &str, pane_id: &str) -> Result<Self, FinishClaimError> {
        app.credential_spawn_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .claim_finish(bot_id, permit_id, pane_id)?;
        Ok(Self { app: app.clone(), bot_id: bot_id.to_string(), permit_id: permit_id.to_string(), pane_id: pane_id.to_string(), completed: false })
    }

    fn complete(mut self) -> bool {
        let released = self
            .app
            .credential_spawn_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .complete_finish(&self.bot_id, &self.permit_id, &self.pane_id);
        self.completed = released;
        released
    }
}

impl Drop for FinishClaim {
    fn drop(&mut self) {
        if !self.completed {
            self.app
                .credential_spawn_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .unclaim_finish(&self.bot_id, &self.permit_id, &self.pane_id);
        }
    }
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

fn reserve(app: &Arc<App>, bot_id: &str, permit_id: &str, ttl: Duration) -> Result<(), ()> {
    let mut gate = app.credential_spawn_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    gate.reserve(bot_id, permit_id, ttl)
}

fn release(app: &Arc<App>, bot_id: &str, permit_id: &str) -> bool {
    let mut gate = app.credential_spawn_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    gate.release(bot_id, permit_id)
}

#[derive(Deserialize)]
pub struct SpawnBegin {
    pub bot_id: String,
    #[serde(default)]
    pub timeout_ms: String,
}

#[derive(Deserialize)]
pub struct SpawnFinish {
    pub bot_id: String,
    pub permit_id: String,
    pub pane_id: String,
    #[serde(default)]
    pub purpose: String,
}

#[derive(Deserialize)]
pub struct SpawnAbort {
    pub bot_id: String,
    pub permit_id: String,
}

/// `POST /relay/spawn/begin`: reserve the parent's credential while a shim invokes herdr.
pub async fn begin(State(app): State<Arc<App>>, headers: axum::http::HeaderMap, axum::extract::Form(body): axum::extract::Form<SpawnBegin>) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if let Err(response) = authenticated_bot(&app, &body.bot_id, token).await {
        return response;
    }
    let permit_id = crate::db::ulid();
    if reserve(&app, &body.bot_id, &permit_id, permit_ttl(&body.timeout_ms)).is_err() {
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
    let pane_id = body.pane_id.trim();
    if pane_id.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "pane_id_missing", "herdr did not identify the created pane; spawn remains fenced");
    }
    let claim = match FinishClaim::begin(&app, &body.bot_id, &body.permit_id, pane_id) {
        Ok(claim) => claim,
        Err(FinishClaimError::Missing) => {
            return fail(StatusCode::CONFLICT, "spawn_permit_missing", "spawn permit is missing; rotation safety is unknown");
        }
        Err(FinishClaimError::PaneMismatch) => {
            return fail(StatusCode::CONFLICT, "spawn_permit_mismatch", "spawn permit is already bound to another pane");
        }
        Err(FinishClaimError::InProgress) => {
            return fail(StatusCode::CONFLICT, "spawn_finish_in_progress", "spawn permit is already being finished");
        }
    };
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
    if parent_pane.as_deref() != Some(pane_id) {
        if let Err(e) = crate::panes::note_purpose(&app, &host, pane_id, &bot, body.purpose.trim()).await {
            tracing::warn!(bot = %bot.id, pane = %pane_id, error = %e, "credential-bearing pane could not be registered");
            return fail(StatusCode::SERVICE_UNAVAILABLE, "pane_registration_failed", "created pane could not be registered; spawn remains fenced");
        }
    }
    if !claim.complete() {
        return fail(StatusCode::CONFLICT, "spawn_permit_missing", "spawn permit changed while registering the pane");
    }
    (StatusCode::OK, Json(json!({"pane_id": pane_id, "registered": true})))
}

/// `POST /relay/spawn/abort`: drop a permit when herdr did not create a pane (#664).
/// Missing permits are still 200 — the shim retries and a TTL sweep may have won the race.
pub async fn abort(
    State(app): State<Arc<App>>,
    headers: axum::http::HeaderMap,
    axum::extract::Form(body): axum::extract::Form<SpawnAbort>,
) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if let Err(response) = authenticated_bot(&app, &body.bot_id, token).await {
        return response;
    }
    let released = release(&app, &body.bot_id, &body.permit_id);
    (StatusCode::OK, Json(json!({"released": released})))
}
