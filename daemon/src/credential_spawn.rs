//! Credential inheritance fences for herdr pane / child-agent creation.
//!
//! A bot's already-running CLI can create panes outside the daemon's SQLite transaction. The shim
//! therefore reserves a spawn permit before invoking herdr and reports the resulting pane before
//! releasing it. Rotation sets a short-lived fence in the same registry before checking descendants;
//! permits already in flight make rotation fail closed, and new permits are refused until the token
//! update commits or the rotation aborts.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::App;

/// A permit the shim never finishes (herdr failed, curl timed out after reserve, Ctrl-C) must not
/// block credential rotation until the daemon restarts (#664).
const PERMIT_TTL: Duration = Duration::from_secs(60);

/// `herdr agent start --timeout` 最長 300 秒，加上收尾的餘裕。
const MAX_PERMIT_TTL: Duration = Duration::from_secs(330);

/// 這次 spawn 的 permit 有效多久：至少 [`PERMIT_TTL`]；shim 帶了 `agent start` 的 `--timeout`（毫秒）就涵蓋它再加 30 秒餘裕，
/// 上限 [`MAX_PERMIT_TTL`]。看不懂的值當沒帶——寧可短（過期只是不再擋輪替）也不要讓亂填的值把 permit 撐成永久。
fn permit_ttl(timeout_ms: &str) -> Duration {
    let Ok(ms) = timeout_ms.trim().parse::<u64>() else { return PERMIT_TTL };
    let wanted = Duration::from_millis(ms.min(MAX_PERMIT_TTL.as_millis() as u64)) + Duration::from_secs(30);
    wanted.clamp(PERMIT_TTL, MAX_PERMIT_TTL)
}

/// 一張 permit：何時開的、有效多久。
struct Permit {
    at: Instant,
    ttl: Duration,
    pane_id: Option<String>,
    finishing: bool,
}

impl Permit {
    fn alive(&self, now: Instant) -> bool {
        self.finishing || now.saturating_duration_since(self.at) < self.ttl
    }
}

#[derive(Debug, PartialEq, Eq)]
enum FinishClaimError {
    Missing,
    PaneMismatch,
    InProgress,
}

#[derive(Default)]
pub struct Gate {
    rotating: HashSet<String>,
    permits: HashMap<String, HashMap<String, Permit>>,
}

impl Gate {
    fn sweep(&mut self, bot_id: &str, now: Instant) {
        let Some(permits) = self.permits.get_mut(bot_id) else { return };
        permits.retain(|_, p| p.alive(now));
        if permits.is_empty() {
            self.permits.remove(bot_id);
        }
    }

    fn begin_rotation(&mut self, bot_id: &str) -> Result<(), FenceError> {
        if !self.rotating.insert(bot_id.to_string()) {
            return Err(FenceError::AlreadyRotating);
        }
        self.sweep(bot_id, Instant::now());
        let in_flight = self.permits.get(bot_id).map_or(0, HashMap::len);
        if in_flight > 0 {
            self.rotating.remove(bot_id);
            return Err(FenceError::SpawnsInFlight(in_flight));
        }
        Ok(())
    }

    fn reserve(&mut self, bot_id: &str, permit_id: &str, ttl: Duration) -> Result<(), ()> {
        if self.rotating.contains(bot_id) {
            return Err(());
        }
        self.sweep(bot_id, Instant::now());
        self.permits.entry(bot_id.to_string()).or_default().insert(
            permit_id.to_string(),
            Permit { at: Instant::now(), ttl, pane_id: None, finishing: false },
        );
        Ok(())
    }

    #[cfg(test)]
    fn permit_active(&self, bot_id: &str, permit_id: &str) -> bool {
        self.permits.get(bot_id).is_some_and(|permits| permits.get(permit_id).is_some_and(|p| p.alive(Instant::now())))
    }

    /// Bind a permit before any asynchronous registration. A replay with a different pane must
    /// never get as far as the DB write, even when two finish requests arrive together.
    fn claim_finish(&mut self, bot_id: &str, permit_id: &str, pane_id: &str) -> Result<(), FinishClaimError> {
        self.sweep(bot_id, Instant::now());
        let permit = self
            .permits
            .get_mut(bot_id)
            .and_then(|permits| permits.get_mut(permit_id))
            .filter(|p| p.alive(Instant::now()))
            .ok_or(FinishClaimError::Missing)?;
        if permit.finishing {
            return Err(FinishClaimError::InProgress);
        }
        if permit.pane_id.as_deref().is_some_and(|bound| bound != pane_id) {
            return Err(FinishClaimError::PaneMismatch);
        }
        permit.pane_id.get_or_insert_with(|| pane_id.to_string());
        permit.finishing = true;
        Ok(())
    }

    /// An abort can only discard an untouched permit; once finish started, its registration must
    /// either complete or return an error and release the claim for a retry.
    fn release(&mut self, bot_id: &str, permit_id: &str) -> bool {
        if self.permits.get(bot_id).and_then(|permits| permits.get(permit_id)).is_some_and(|p| p.finishing) {
            return false;
        }
        self.remove(bot_id, permit_id)
    }

    fn complete_finish(&mut self, bot_id: &str, permit_id: &str, pane_id: &str) -> bool {
        let valid = self
            .permits
            .get(bot_id)
            .and_then(|permits| permits.get(permit_id))
            .is_some_and(|p| p.finishing && p.pane_id.as_deref() == Some(pane_id));
        valid && self.remove(bot_id, permit_id)
    }

    fn unclaim_finish(&mut self, bot_id: &str, permit_id: &str, pane_id: &str) {
        if let Some(p) = self
            .permits
            .get_mut(bot_id)
            .and_then(|permits| permits.get_mut(permit_id))
            .filter(|p| p.pane_id.as_deref() == Some(pane_id))
        {
            p.finishing = false;
        }
    }

    fn remove(&mut self, bot_id: &str, permit_id: &str) -> bool {
        let Some(permits) = self.permits.get_mut(bot_id) else { return false };
        let removed = permits.remove(permit_id).is_some();
        if permits.is_empty() {
            self.permits.remove(bot_id);
        }
        removed
    }

    /// Test-only: pretend `permit_id` was reserved `age` ago so TTL can be checked without sleeping.
    #[cfg(test)]
    fn age_permit(&mut self, bot_id: &str, permit_id: &str, age: Duration) {
        if let Some(p) = self.permits.get_mut(bot_id).and_then(|p| p.get_mut(permit_id)) {
            p.at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
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

fn reserve(app: &impl crate::credential_spawn::CredentialSpawnGate, bot_id: &str, permit_id: &str, ttl: Duration) -> Result<(), ()> {
    let mut gate = app.credential_spawn_gate().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    gate.reserve(bot_id, permit_id, ttl)
}

fn release(app: &impl crate::credential_spawn::CredentialSpawnGate, bot_id: &str, permit_id: &str) -> bool {
    app.credential_spawn_gate().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).release(bot_id, permit_id)
}

#[derive(Deserialize)]
pub struct SpawnBegin {
    bot_id: String,
    /// `agent start --timeout` 的毫秒數（shim 帶；split／tab create 沒有）。
    #[serde(default)]
    timeout_ms: String,
}

#[derive(Deserialize)]
pub struct SpawnFinish {
    bot_id: String,
    permit_id: String,
    pane_id: String,
    #[serde(default)]
    purpose: String,
}

#[derive(Deserialize)]
pub struct SpawnAbort {
    bot_id: String,
    permit_id: String,
}

fn fail(status: StatusCode, reason: &str, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message, "reason": reason})))
}

async fn authenticated_bot(app: &impl crate::capabilities::Db, bot_id: &str, token: &str) -> Result<crate::db::Bot, (StatusCode, Json<Value>)> {
    match crate::db::bot(app.db(), bot_id).await {
        Ok(Some(bot)) if bot.deleted_at.is_none() && !token.is_empty() && crate::models::app_ports_p13::ct_eq(token, &bot.hook_token) => {
            // 分享用的受限 bot 不能開子 agent（SPEC「分享 bot」）。
            if crate::models::app_ports_p13::refuses_bot_principal(app.db(), &bot.id).await {
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
        if let Err(e) = crate::models::app_ports_p13::note_purpose(&app, &host, pane_id, &bot, body.purpose.trim()).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_rotation_refuses_new_spawns_and_an_existing_spawn_refuses_rotation() {
        let mut gate = Gate::default();
        let bot = "gate-test";
        gate.reserve(bot, "before-rotation", PERMIT_TTL).unwrap();
        assert_eq!(gate.begin_rotation(bot).unwrap_err(), FenceError::SpawnsInFlight(1));
        assert!(gate.reserve(bot, "during-failed-rotation", PERMIT_TTL).is_ok(), "failed fence setup must be cleared");
        gate.release(bot, "before-rotation");
        gate.release(bot, "during-failed-rotation");

        gate.begin_rotation(bot).unwrap();
        assert!(gate.reserve(bot, "during-rotation", PERMIT_TTL).is_err());
        gate.rotating.remove(bot);
        assert!(gate.reserve(bot, "after-aborted-rotation", PERMIT_TTL).is_ok());
    }

    /// #664：失敗的 agent start 留下的 permit 過了 TTL 就不再擋輪替。
    #[test]
    fn an_expired_spawn_permit_does_not_block_rotation() {
        let mut gate = Gate::default();
        let bot = "ttl";
        gate.reserve(bot, "stuck", PERMIT_TTL).unwrap();
        gate.age_permit(bot, "stuck", PERMIT_TTL + Duration::from_secs(1));
        gate.begin_rotation(bot).unwrap();
        assert!(!gate.permit_active(bot, "stuck"));
    }

    #[test]
    fn a_spawn_permit_is_bound_to_one_pane_and_abort_cannot_race_finish() {
        let mut gate = Gate::default();
        let bot = "single-pane";
        let permit = "one-use";
        gate.reserve(bot, permit, PERMIT_TTL).unwrap();

        gate.claim_finish(bot, permit, "w1:p1").unwrap();
        assert!(!gate.release(bot, permit), "abort must not remove a pane registration while finish is in progress");
        assert_eq!(gate.claim_finish(bot, permit, "w1:p2"), Err(FinishClaimError::InProgress));

        gate.unclaim_finish(bot, permit, "w1:p1");
        assert_eq!(gate.claim_finish(bot, permit, "w1:p2"), Err(FinishClaimError::PaneMismatch));
        gate.claim_finish(bot, permit, "w1:p1").unwrap();
        assert!(gate.complete_finish(bot, permit, "w1:p1"));
        assert_eq!(gate.claim_finish(bot, permit, "w1:p1"), Err(FinishClaimError::Missing));
    }

    fn bot_headers() -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("X-AM-Bot-Token", "tok".parse().unwrap());
        h
    }

    /// `herdr agent start --timeout` 最長 300 秒：子 agent 慢慢啟動時，permit 不能在 `finish` 之前就過期
    /// （以前固定 60 秒：pane 其實開好了，finish 回 409「permit missing」、shim 報尚未登記）。
    /// permit 的有效期涵蓋那次的 timeout，期間照樣擋輪替。
    #[tokio::test]
    async fn a_long_agent_start_timeout_keeps_its_permit_until_finish() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "slow-spawner").await;
        let begin_with = |timeout_ms: &str| {
            let (app, id, t) = (app.clone(), bot.id.clone(), timeout_ms.to_string());
            async move {
                let (st, Json(v)) = begin(State(app), bot_headers(), axum::extract::Form(SpawnBegin { bot_id: id, timeout_ms: t })).await;
                assert_eq!(st, StatusCode::OK, "{v}");
                v["permit_id"].as_str().unwrap().to_string()
            }
        };
        let permit = begin_with("120000").await;
        app.credential_spawn_gate.lock().unwrap().age_permit(&bot.id, &permit, Duration::from_secs(100));
        assert_eq!(RotationFence::begin(&app, &bot.id).err(), Some(FenceError::SpawnsInFlight(1)), "100 秒時 120 秒 timeout 的 spawn 還在飛，要擋輪替");
        let (st, Json(v)) = finish(
            State(app.clone()),
            bot_headers(),
            axum::extract::Form(SpawnFinish { bot_id: bot.id.clone(), permit_id: permit.clone(), pane_id: "w1:p9".into(), purpose: String::new() }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "pane 開好了、finish 要登記成功：{v}");

        // 沒帶 timeout（split／tab create、舊 shim）維持 60 秒。
        let quick = begin_with("").await;
        app.credential_spawn_gate.lock().unwrap().age_permit(&bot.id, &quick, PERMIT_TTL + Duration::from_secs(1));
        assert!(RotationFence::begin(&app, &bot.id).is_ok(), "沒帶 timeout 的 permit 過 60 秒就不擋輪替");
    }

    /// 有效期有上限（herdr 自己最多等 300 秒）：亂填的 timeout 不能讓 permit 永遠擋輪替。
    #[test]
    fn the_permit_lifetime_is_bounded() {
        assert_eq!(permit_ttl(""), PERMIT_TTL);
        assert_eq!(permit_ttl("abc"), PERMIT_TTL);
        assert_eq!(permit_ttl("30000"), PERMIT_TTL, "預設 30 秒的啟動：60 秒綽綽有餘");
        assert_eq!(permit_ttl("120000"), Duration::from_secs(150));
        assert_eq!(permit_ttl("999999999999"), MAX_PERMIT_TTL);
    }
}

/// 開登入殼的閘門。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait CredentialSpawnGate: Send + Sync {
    fn credential_spawn_gate(&self) -> &std::sync::Mutex<crate::credential_spawn::Gate>;
}
