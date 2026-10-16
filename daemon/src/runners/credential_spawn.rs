//! `credential_spawn` runner 與 API handlers。

use std::sync::Arc;
use std::time::Duration;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::credential_spawn::{
    permit_ttl, FenceError, FinishClaimError, SpawnAuthority,
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
    authority: Option<SpawnAuthority>,
    completed: bool,
}

impl FinishClaim {
    fn begin(app: &Arc<App>, bot_id: &str, permit_id: &str, pane_id: &str) -> Result<Self, FinishClaimError> {
        let authority = app.credential_spawn_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .claim_finish_with_authority(bot_id, permit_id, pane_id)?;
        Ok(Self { app: app.clone(), bot_id: bot_id.to_string(), permit_id: permit_id.to_string(), pane_id: pane_id.to_string(), authority, completed: false })
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

    fn reject(mut self) {
        self.app.credential_spawn_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .reject_finish(&self.bot_id, &self.permit_id, &self.pane_id);
        self.completed = true;
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

fn reserve(app: &Arc<App>, bot_id: &str, permit_id: &str, ttl: Duration, authority: SpawnAuthority) -> Result<(), ()> {
    let mut gate = app.credential_spawn_gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    gate.reserve_authoritative(bot_id, permit_id, ttl, authority)
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
pub struct SpawnClaim {
    pub bot_id: String,
    pub pane_id: String,
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
    let bot = match authenticated_bot(&app, &body.bot_id, token).await {
        Ok(bot) => bot,
        Err(response) => return response,
    };
    let host = match crate::db::bot_host(&app.db, &bot.id).await {
        Ok(host) => host,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "credential spawn could not read bot host");
            return fail(StatusCode::SERVICE_UNAVAILABLE, "host_unreadable", "bot host could not be checked; spawn refused");
        }
    };
    let fence = match app.hosts.fence(&host).await {
        Some(fence) => fence,
        None => return fail(StatusCode::CONFLICT, "spawn_host_unavailable", "bot host is unavailable; spawn refused"),
    };
    let run = match crate::db::active_run(&app.db, &bot.id).await {
        Ok(run) => run,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "credential spawn could not read active run");
            return fail(StatusCode::SERVICE_UNAVAILABLE, "run_unreadable", "active run could not be checked; spawn refused");
        }
    };
    let session = match run.as_ref().and_then(|run| run.herdr_session.clone().filter(|s| !s.is_empty())) {
        Some(session) => Some(session),
        None => app.session_for_bot_with_host_fence(&bot, &host, &fence).await,
    };
    let Some(session) = session else {
        return fail(StatusCode::CONFLICT, "spawn_session_unavailable", "bot session is unavailable; spawn refused");
    };
    let authority = SpawnAuthority { host: host.clone(), host_authority: fence.authority_key(), herdr_session: session, run_id: run.map(|run| run.id) };
    let permit_id = crate::db::ulid();
    if reserve(&app, &body.bot_id, &permit_id, permit_ttl(&body.timeout_ms), authority.clone()).is_err() {
        return fail(StatusCode::CONFLICT, "credential_rotation_pending", "credential rotation is checking child panes; spawn refused");
    }
    // Close the read-before-reserve window: a rotation may have committed between the first token
    // read and reserve(), after which its fence is already down and only this second read notices.
    if let Err(response) = authenticated_bot(&app, &body.bot_id, token).await {
        release(&app, &body.bot_id, &permit_id);
        return response;
    }
    let run_is_current = match crate::db::active_run(&app.db, &bot.id).await {
        Ok(current) => current.as_ref().map(|run| run.id.as_str()) == authority.run_id.as_deref(),
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "credential spawn could not recheck active run");
            false
        }
    };
    let still_current = fence.is_current_now()
        && crate::db::bot_host(&app.db, &bot.id).await.ok().as_deref() == Some(authority.host.as_str())
        && run_is_current;
    if !still_current {
        release(&app, &body.bot_id, &permit_id);
        return fail(StatusCode::CONFLICT, "spawn_authority_changed", "bot host or run changed while reserving spawn; retry");
    }
    (StatusCode::OK, Json(json!({"permit_id": permit_id})))
}

/// `POST /relay/spawn/claim` (issue #1790): the shim registers that `bot_id` itself just started the agent in `pane_id`.
/// Child attribution follows this durable fact instead of the herdr name prefix. A bot can only claim as itself:
/// the token must be that `bot_id`'s own.
pub async fn claim(State(app): State<Arc<App>>, headers: axum::http::HeaderMap, axum::extract::Form(body): axum::extract::Form<SpawnClaim>) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let bot = match authenticated_bot(&app, &body.bot_id, token).await {
        Ok(bot) => bot,
        Err(response) => return response,
    };
    let pane_id = body.pane_id.trim();
    if pane_id.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "pane_id_missing", "herdr did not identify the started pane");
    }
    match crate::spawn_hints::claim(&app, &bot.id, pane_id).await {
        Ok(()) => (StatusCode::OK, Json(json!({"pane_id": pane_id, "claimed": true}))),
        Err(e) => {
            tracing::warn!(bot = %bot.id, pane = %pane_id, error = %e, "spawn claim could not be recorded");
            fail(StatusCode::SERVICE_UNAVAILABLE, "claim_unrecorded", "spawn claim could not be recorded")
        }
    }
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
    let Some(authority) = claim.authority.clone() else {
        claim.reject();
        return fail(StatusCode::CONFLICT, "spawn_authority_missing", "spawn permit has no captured authority; retry spawn");
    };
    let current_host = match crate::db::bot_host(&app.db, &bot.id).await {
        Ok(host) => host,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "credential spawn could not read bot host");
            return fail(StatusCode::SERVICE_UNAVAILABLE, "host_unreadable", "bot host could not be checked; spawn remains fenced");
        }
    };
    let fence = match app.hosts.fence(&authority.host).await {
        Some(fence) if authority.host_authority.matches(&fence) && current_host == authority.host => fence,
        _ => {
            claim.reject();
            return fail(StatusCode::CONFLICT, "spawn_host_superseded", "spawn host changed after the permit was issued");
        }
    };
    let run = match crate::db::active_run(&app.db, &bot.id).await {
        Ok(run) if run.as_ref().map(|current| current.id.as_str()) == authority.run_id.as_deref() => run,
        Ok(_) => {
            claim.reject();
            return fail(StatusCode::CONFLICT, "spawn_run_superseded", "bot run changed after the permit was issued");
        }
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "credential spawn could not read active pane");
            return fail(StatusCode::SERVICE_UNAVAILABLE, "run_unreadable", "parent pane could not be checked; spawn remains fenced");
        }
    };
    let current_session = app.session_for_bot_with_host_fence(&bot, &authority.host, &fence).await;
    let current_session = run.as_ref().and_then(|run| run.herdr_session.clone().filter(|s| !s.is_empty())).or(current_session);
    if current_session.as_deref() != Some(authority.herdr_session.as_str()) {
        claim.reject();
        return fail(StatusCode::CONFLICT, "spawn_session_superseded", "bot session changed after the permit was issued");
    }
    let Some(client) = app.herdr_for_host_fence(&fence, &authority.herdr_session).await else {
        claim.reject();
        return fail(StatusCode::CONFLICT, "spawn_host_superseded", "spawn host session is no longer available");
    };
    let parent_pane = run.as_ref().and_then(|run| run.pane_id.clone());
    let operation = fence.run_current(async {
        let current_host = match crate::db::bot_host(&app.db, &bot.id).await {
            Ok(host) => host,
            Err(e) => {
                tracing::warn!(bot = %bot.id, error = %e, "credential spawn could not recheck bot host");
                return Err((StatusCode::SERVICE_UNAVAILABLE, "host_unreadable", "bot host could not be rechecked; spawn remains fenced"));
            }
        };
        let current_run = match crate::db::active_run(&app.db, &bot.id).await {
            Ok(run) => run,
            Err(e) => {
                tracing::warn!(bot = %bot.id, error = %e, "credential spawn could not recheck active run");
                return Err((StatusCode::SERVICE_UNAVAILABLE, "run_unreadable", "active run could not be rechecked; spawn remains fenced"));
            }
        };
        if current_host != authority.host || current_run.as_ref().map(|run| run.id.as_str()) != authority.run_id.as_deref() {
            return Err((StatusCode::CONFLICT, "spawn_authority_changed", "bot host or run changed before pane registration"));
        }
        match client.pane_get(pane_id).await {
            Ok(Some(_)) => {},
            Ok(None) => return Err((StatusCode::SERVICE_UNAVAILABLE, "spawn_pane_unverified", "created pane was not found in the captured herdr session; retry finish")),
            Err(e) => {
                tracing::warn!(bot = %bot.id, pane = %pane_id, error = %e, "credential spawn could not verify pane");
                return Err((StatusCode::SERVICE_UNAVAILABLE, "spawn_pane_unreadable", "created pane could not be verified; spawn remains fenced"));
            }
        }
        if parent_pane.as_deref() != Some(pane_id) {
            crate::panes::note_purpose(&app, &authority.host, pane_id, &bot, body.purpose.trim()).await
                .map_err(|e| {
                    tracing::warn!(bot = %bot.id, pane = %pane_id, error = %e, "credential-bearing pane could not be registered");
                    (StatusCode::SERVICE_UNAVAILABLE, "pane_registration_failed", "created pane could not be registered; spawn remains fenced")
                })?;
        }
        Ok(())
    }).await;
    let result = match operation {
        None => Err((StatusCode::CONFLICT, "spawn_host_superseded", "spawn host changed before pane registration")),
        Some(result) => result,
    };
    if let Err((status, reason, message)) = result {
        if status == StatusCode::CONFLICT { claim.reject(); }
        return fail(status, reason, message);
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use axum::Form;

    fn bot_headers(token: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("X-AM-Bot-Token", HeaderValue::from_str(token).unwrap());
        headers
    }

    async fn reserve(app: &Arc<App>, parent: &crate::db::Bot) -> String {
        let (status, Json(body)) = begin(
            State(app.clone()),
            bot_headers(&parent.hook_token),
            Form(SpawnBegin { bot_id: parent.id.clone(), timeout_ms: String::new() }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "spawn begin should reserve the parent credential: {body}");
        body["permit_id"].as_str().expect("permit id").to_string()
    }

    async fn finish_for(app: &Arc<App>, parent: &crate::db::Bot, permit_id: &str, pane_id: &str) -> (StatusCode, Json<Value>) {
        finish(
            State(app.clone()),
            bot_headers(&parent.hook_token),
            Form(SpawnFinish {
                bot_id: parent.id.clone(),
                permit_id: permit_id.to_string(),
                pane_id: pane_id.to_string(),
                purpose: "login shell".to_string(),
            }),
        )
        .await
    }

    #[tokio::test]
    async fn finishing_a_spawn_for_a_plain_shell_pane_registers_it() {
        let e = crate::testing::env().await;
        let parent = crate::testing::claude_bot(&e.app, &e.project_id, "plain-shell-parent").await;
        crate::testing::fake_run(&e.app, &parent.id).await;
        let permit_id = reserve(&e.app, &parent).await;
        let pane_id = "w1:p-shell";
        e.herdr.live_pane(pane_id, crate::testing::LivePane { rows: Some(40), ..Default::default() });

        let (status, Json(body)) = finish_for(&e.app, &parent, &permit_id, pane_id).await;
        assert_eq!(status, StatusCode::OK, "a shell pane without an agent should finish successfully: {body}");
        assert_eq!(body["registered"], true);
        let owner: String = sqlx::query_scalar("SELECT owner_bot_id FROM panes WHERE pane_id=?")
            .bind(pane_id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(owner, parent.id);

        let (status, Json(body)) = finish_for(&e.app, &parent, &permit_id, pane_id).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["reason"], "spawn_permit_missing");
    }

    #[tokio::test]
    async fn finishing_a_spawn_for_a_pane_herdr_does_not_know_stays_fenced() {
        let e = crate::testing::env().await;
        let parent = crate::testing::claude_bot(&e.app, &e.project_id, "retry-shell-parent").await;
        crate::testing::fake_run(&e.app, &parent.id).await;
        let permit_id = reserve(&e.app, &parent).await;
        let pane_id = "w1:p-retry-shell";

        let (status, Json(body)) = finish_for(&e.app, &parent, &permit_id, pane_id).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["reason"], "spawn_pane_unverified");

        e.herdr.live_pane(pane_id, crate::testing::LivePane { rows: Some(40), ..Default::default() });
        let (status, Json(body)) = finish_for(&e.app, &parent, &permit_id, pane_id).await;
        assert_eq!(status, StatusCode::OK, "a transiently unknown pane can be retried: {body}");
        assert_eq!(body["registered"], true);
    }
}
