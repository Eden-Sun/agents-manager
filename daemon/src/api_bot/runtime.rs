//! App host for the inert Claude API bot runtime.
//!
//! Production remains deliberately fail-closed: constructing this value is memory-only, and no
//! production path can install provider, HTTP, or key dependencies. All `ApiBotPort` methods
//! refuse before database/config access when dependencies are absent. Tests inject fakes through
//! the test-only seam and exercise the same runtime core compiled into production.

use crate::lifecycle::api_bot_port::{
    ApiBeginStopError, ApiBeginStopMode, ApiBeginStopResult, ApiBotError, ApiRunCompletion, ApiRunKey, ApiRunStopEvidence, ApiRunStopProof, ApiTurnKey,
};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::registry::{
    ApiBotRegistry, RegistryError, RunStopJoin, RunTicket, RunTurnKey, TurnOutcome,
};

const API_BOT_SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(10);
const API_BOT_COMPLETION_MAX_ATTEMPTS: usize = 3;
const API_BOT_COMPLETION_RETRY_DELAY: Duration = Duration::from_millis(25);
const API_BOT_COMPLETION_STORAGE_ERROR: &str = "api_completion_storage";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FullKey {
    bot_id: String,
    run_id: String,
    turn_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AdmissionKey {
    bot_id: String,
    run_id: String,
}

impl From<&ApiTurnKey> for FullKey {
    fn from(key: &ApiTurnKey) -> Self {
        Self {
            bot_id: key.bot_id.clone(),
            run_id: key.run_id.clone(),
            turn_id: key.turn_id.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Completion {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinError {
    Unconfirmed,
    IdentityMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinState {
    Pending,
    Joined(Result<Completion, JoinError>),
    TimedOut,
}

struct ControlRecord {
    identity: Arc<()>,
    cancel: CancellationToken,
    joined: watch::Receiver<JoinState>,
    finalized: Option<Arc<super::same_run::Finalized>>,
    owner_exited: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct SessionCursor {
    identity: super::adapter::AdapterIdentity,
    revision: i64,
}

struct RuntimeInner {
    registry: Arc<ApiBotRegistry>,
    controls: Mutex<HashMap<FullKey, ControlRecord>>,
    sessions: Mutex<HashMap<String, SessionCursor>>,
    admission: AsyncMutex<HashMap<AdmissionKey, Weak<AsyncMutex<()>>>>,
    bot_admission: AsyncMutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    explicit_quarantine: Mutex<HashMap<String, Arc<()>>>,
    /// 失敗 turn 的固定原因代碼（runner 結束時寫入、completion owner 取走後寫進對話的系統訊息）。
    fail_reasons: Mutex<HashMap<FullKey, String>>,
    /// 達到 API 呼叫上限的回合（記錄該回合的上限值 N，由 completion owner 取走後寫進系統提示）。
    cap_reached: Mutex<HashMap<FullKey, usize>>,
    /// 對話壓縮的提示訊息（runner 結束時寫入、completion owner 取走後寫進對話的系統訊息）。
    compaction_notes: Mutex<HashMap<FullKey, String>>,
}

/// Runtime storage is initialized without tasks, sockets, or database work.
pub(crate) struct ApiBotRuntime {
    inner: Arc<RuntimeInner>,
    dependencies: Mutex<Option<ApiBotDependencies>>,
}

pub(super) struct RetentionGuard {
    _bot: OwnedMutexGuard<()>,
    _run: OwnedMutexGuard<()>,
}

pub(super) struct ExplicitRetireGuard {
    _bot: OwnedMutexGuard<()>, _run: OwnedMutexGuard<()>,
    key: FullKey, cursor: Option<SessionCursor>, control: Option<Arc<()>>,
}

impl ApiBotRuntime {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                registry: Arc::new(ApiBotRegistry::new()),
                controls: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                admission: AsyncMutex::new(HashMap::new()),
                bot_admission: AsyncMutex::new(HashMap::new()),
                explicit_quarantine: Mutex::new(HashMap::new()),
                fail_reasons: Mutex::new(HashMap::new()),
                cap_reached: Mutex::new(HashMap::new()),
                compaction_notes: Mutex::new(HashMap::new()),
            }),
            dependencies: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn set_test_dependencies(&self, dependencies: ApiBotDependencies) {
        *self
            .dependencies
            .lock()
            .expect("API bot dependencies mutex poisoned") = Some(dependencies);
    }

    #[cfg(test)]
    fn test_dependencies(&self) -> Option<ApiBotDependencies> {
        self.dependencies
            .lock()
            .expect("API bot dependencies mutex poisoned")
            .clone()
    }

    fn dependencies_snapshot(&self) -> Option<ApiBotDependencies> {
        self.dependencies
            .lock()
            .expect("API bot dependencies mutex poisoned")
            .clone()
    }

    async fn spawn(&self, app: &crate::state::App, key: &ApiTurnKey) -> Result<(), ApiBotError> {
        self.spawn_with_dependencies(app, key, self.dependencies_snapshot(), false).await
    }

    pub(super) async fn spawn_native(&self, app: &crate::state::App, key: &ApiTurnKey) -> Result<(), ApiBotError> {
        if !app.api_startup_gate.ready() { return Err(ApiBotError::Upstream); }
        self.spawn_with_dependencies(app, key, self.dependencies_snapshot(), true).await
    }

    async fn cancel_and_join(
        &self,
        app: &crate::state::App,
        key: &ApiTurnKey,
    ) -> Result<ApiRunCompletion, ApiBotError> {
        if !app.api_startup_gate.ready() { return Err(ApiBotError::Upstream); }
        self.cancel_and_join_core(app, key).await
    }

    async fn stop_proof(
        &self,
        app: &crate::state::App,
        key: &ApiRunKey,
    ) -> Result<ApiRunStopProof, ApiBotError> {
        if !app.api_startup_gate.ready() { return Err(ApiBotError::Upstream); }
        let dependencies = self.dependencies_snapshot();

        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("api_run_stop_before_admission", &key.run_id).await;
        // A pre-CAS spawn may have passed run validation but not installed its registry ticket yet.
        // Draining the same gate used by spawn makes that ticket visible before an empty-slot proof.
        let _bot_admission = self.bot_gate(&key.bot_id).await;
        let _admission = self.admission_gate(key).await;

        let run = crate::db::run(&app.db, &key.run_id)
            .await
            .map_err(|_| ApiBotError::Upstream)?
            .ok_or(ApiBotError::IdentityMismatch)?;
        if run.bot_id != key.bot_id || run.state != "stopping" {
            return Err(ApiBotError::IdentityMismatch);
        }
        let has_in_flight: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE run_id=? AND status='in_flight')",
        )
        .bind(&key.run_id)
        .fetch_one(&app.db)
        .await
        .map_err(|_| ApiBotError::Upstream)?;
        if has_in_flight {
            return Err(ApiBotError::Busy);
        }

        let timeout = dependencies.as_ref()
            .and_then(|d| d.shutdown_join_timeout)
            .unwrap_or(API_BOT_SHUTDOWN_JOIN_TIMEOUT);
        let joined = tokio::time::timeout(
            timeout,
            self.inner.registry.stop_and_join_run(&key.run_id),
        )
        .await
        .map_err(|_| ApiBotError::Unconfirmed)?
        .map_err(|error| match error {
            RegistryError::IdentityMismatch => ApiBotError::IdentityMismatch,
            _ => ApiBotError::Unconfirmed,
        })?;
        let evidence = match joined {
            RunStopJoin::NoLiveRunner => ApiRunStopEvidence::NoLiveRunner,
            RunStopJoin::Joined { turn_id, outcome } => {
                let completion = match outcome {
                    TurnOutcome::Completed => ApiRunCompletion::Completed,
                    TurnOutcome::Cancelled => ApiRunCompletion::Cancelled,
                    TurnOutcome::Failed(_) => ApiRunCompletion::Failed,
                };
                ApiRunStopEvidence::Joined {
                    turn_id,
                    completion,
                }
            }
        };
        let proof = ApiRunStopProof {
            key: key.clone(),
            evidence,
        };
        Ok(proof)
    }

    async fn live(&self, app: &crate::state::App, key: &ApiTurnKey) -> bool {
        if !app.api_startup_gate.ready() { return false; }
        cleanup_terminal_controls(&app.db, &self.inner).await;
        let full = FullKey::from(key);
        let controls = self.inner.controls.lock().expect("control mutex poisoned");
        controls.get(&full).is_some_and(|record| {
            matches!(
                *record.joined.borrow(),
                JoinState::Pending | JoinState::TimedOut
            )
        })
    }

    // Lock order: lifecycle bot lock (when held) → API bot gate → run admission → controls.
    // The only read-only gate user is the bounded fresh-target descriptor; it takes no later locks.
    async fn bot_gate(&self, bot_id: &str) -> OwnedMutexGuard<()> {
        let gate = {
            let mut gates = self.inner.bot_admission.lock().await;
            gates.retain(|_, gate| gate.strong_count() > 0);
            match gates.get(bot_id).and_then(Weak::upgrade) {
                Some(gate) => gate,
                None => { let gate = Arc::new(AsyncMutex::new(())); gates.insert(bot_id.to_owned(), Arc::downgrade(&gate)); gate }
            }
        };
        gate.lock_owned().await
    }

    async fn admission_gate(&self, key: &ApiRunKey) -> OwnedMutexGuard<()> {
        admission_gate_for(&self.inner, key).await
    }

    /// 建 run 前的前置檢查。確定沒有副作用就拒絕時回 `Refused(reason)`（穩定 reason，前端有中文說明）；
    /// 只有 DB／鎖／關機這類真的無法確認的情況才回 `Unconfirmed`。
    pub(super) async fn native_start_dependencies_ready(&self, app: &crate::state::App, bot_id: &str) -> Result<(), crate::lifecycle::api_bot_port::ApiNativeStartError> {
        use crate::lifecycle::api_bot_port::ApiNativeStartError as E;
        let _gate = tokio::select! {
            biased;
            _ = app.shutdown.cancelled() => return Err(E::Unconfirmed),
            result = tokio::time::timeout(Duration::from_secs(3), self.bot_gate(bot_id)) => result.map_err(|_| { tracing::warn!(bot = bot_id, "api native start not confirmed: bot gate busy for 3s"); E::Unconfirmed })?,
        };
        if !app.api_startup_gate.ready() || self.explicit_retire_quarantined(bot_id) { tracing::warn!(bot = bot_id, "api native start not confirmed: startup gate not ready or bot quarantined"); return Err(E::Unconfirmed); }
        let bot = crate::db::bot(&app.db, bot_id).await.map_err(|error| { tracing::warn!(bot = bot_id, ?error, "api native start not confirmed: bot lookup failed"); E::Unconfirmed })?
            .filter(|b| b.kind == "claude-api" && b.deleted_at.is_none()).ok_or_else(|| { tracing::warn!(bot = bot_id, "api native start not confirmed: bot missing, deleted or not claude-api"); E::Unconfirmed })?;
        let project = crate::db::project(&app.db, &bot.project_id).await.map_err(|error| { tracing::warn!(bot = bot_id, ?error, "api native start not confirmed: project lookup failed"); E::Unconfirmed })?
            .filter(|p| p.deleted_at.is_none() && p.handed_off_to.is_none()).ok_or_else(|| { tracing::warn!(bot = bot_id, "api native start not confirmed: project missing, deleted or handed off"); E::Unconfirmed })?;
        // 遠端專案不隔離、不需要 bwrap，但主機要連得上；沒連線就明確拒絕（不建 run）。
        let remote = project.host != crate::config::LOCAL_HOST;
        if remote && !app.hosts.get(&project.host).await.is_some_and(|c| c.is_connected()) { return Err(E::Refused("api_bot_host_offline")); }
        let cfg = app.cfg.get().await;
        if !cfg.claude_api.enabled || !cfg.claude_api.api_bot.enabled { return Err(E::Refused("api_bot_disabled")); }
        let cap = cfg.claude_api.api_bot.max_turn_cost_microusd;
        if matches!(cap, None | Some(0)) { return Err(E::Refused("api_bot_turn_cost_cap_missing")); }
        let limits = cfg.claude_api.api_bot.validated(cfg.claude_api.enabled).map_err(|_| E::Refused("api_bot_config_invalid"))?.ok_or(E::Refused("api_bot_disabled"))?;
        if self.dependencies_snapshot().is_none() {
            // 單次預留現在依實際 request 估；啟動時還沒有 request，用「3 萬 token 輸入（按快取寫入價）＋ max_output_tokens」當最低門檻。
            let factor = if bot.fast != 0 { 2_i64 } else { 1 };
            let reserve = (32_768_i64 * 5 + i64::from(limits.max_output_tokens) * 20) * factor;
            if limits.max_turn_cost_microusd < reserve { return Err(E::Refused("api_bot_turn_cost_cap_too_low")); }
            if !remote && (!native_bwrap_available() || !bwrap_userns_works().await) { return Err(E::Refused("api_bot_sandbox_unavailable")); }
            let dependencies = production_dependencies(app, &bot, &project, &limits).await.map_err(|_| E::Refused("api_bot_workspace_unavailable"))?;
            let key = dependencies.key_source.snapshot().await.map_err(|_| E::Refused("api_bot_key_unavailable"))?;
            if key.is_empty() { return Err(E::Refused("api_bot_key_unavailable")); }
            drop(key);
        }
        Ok(())
    }

    pub(super) async fn native_start_guard(&self, key: &ApiRunKey) -> Result<NativeStartGuard, ApiBotError> {
        let bot = self.bot_gate(&key.bot_id).await;
        let run = self.admission_gate(key).await;
        let controls = self.inner.controls.lock().expect("control mutex poisoned");
        if self.inner.registry.is_active(&key.run_id) || controls.iter().any(|(k, r)| k.bot_id == key.bot_id && (!r.owner_exited || !matches!(*r.joined.borrow(), JoinState::Joined(Ok(_))) || self.inner.registry.is_active(&k.run_id))) {
            tracing::info!(reason="api_native_start_owner_unconfirmed", "API start owner remains busy");
            return Err(ApiBotError::Busy);
        }
        drop(controls);
        Ok(NativeStartGuard { _bot: bot, _run: run })
    }

    pub(super) async fn retention_guard(&self, app: &crate::state::App, key: &ApiRunKey) -> Result<RetentionGuard, ApiBotError> {
        let bot = self.bot_gate(&key.bot_id).await;
        let run = self.admission_gate(key).await;
        cleanup_terminal_controls(&app.db, &self.inner).await;
        if self.inner.registry.is_active(&key.run_id) || self.inner.controls.lock().expect("control mutex poisoned").keys().any(|k| k.bot_id == key.bot_id) {
            return Err(ApiBotError::Busy);
        }
        Ok(RetentionGuard { _bot: bot, _run: run })
    }

    pub(super) async fn explicit_retire_guard(&self, app: &crate::state::App, c: &super::retention::RetentionCandidate) -> Result<ExplicitRetireGuard, crate::lifecycle::api_bot_port::ApiFreshError> {
        use crate::lifecycle::api_bot_port::ApiFreshError as E;
        let (Some(run), Some(turn)) = (c.2.as_ref(), c.3.as_ref()) else { return Err(E::Unresolved); };
        let key = FullKey { bot_id: c.0.clone(), run_id: run.clone(), turn_id: turn.clone() };
        cleanup_terminal_controls(&app.db, &self.inner).await;
        let wait = async {
            loop {
                let waiting = {
                    let controls = self.inner.controls.lock().expect("control mutex poisoned");
                    controls.get(&key).is_some_and(|record| !record.owner_exited)
                };
                if !waiting { break; }
                tokio::select! {
                    _ = app.shutdown.cancelled() => { tracing::warn!(bot=%c.0, reason="api_retire_busy", gate="shutdown_while_waiting_owner_exit", "explicit retire busy"); return Err(E::Busy) }
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
            Ok(())
        };
        tokio::time::timeout(API_BOT_SHUTDOWN_JOIN_TIMEOUT, wait).await.map_err(|_| { tracing::warn!(bot=%c.0, reason="api_retire_busy", gate="owner_exit_timeout", "explicit retire busy"); E::Busy })??;
        let bot = self.bot_gate(&c.0).await;
        let admission = self.admission_gate(&ApiRunKey { bot_id: c.0.clone(), run_id: run.clone() }).await;
        if app.shutdown.is_cancelled() || self.inner.registry.is_active(run) { tracing::warn!(bot=%c.0, run=%run, reason="api_retire_busy", gate="registry_active_or_shutdown", "explicit retire busy"); return Err(E::Busy); }
        let control = {
            let controls = self.inner.controls.lock().expect("control mutex poisoned");
            if let Some(other) = controls.keys().find(|other| other.bot_id == c.0 && *other != &key) { tracing::warn!(bot=%c.0, other_run=%other.run_id, reason="api_retire_busy", gate="other_control_record", "explicit retire busy"); return Err(E::Busy); }
            match controls.get(&key) {
                Some(record) if record.owner_exited && matches!(*record.joined.borrow(), JoinState::Joined(Ok(_))) => Some(record.identity.clone()),
                Some(r) => { tracing::warn!(bot=%c.0, run=%run, owner_exited=r.owner_exited, joined=?*r.joined.borrow(), reason="api_retire_busy", gate="own_control_not_joined", "explicit retire busy"); return Err(E::Busy) }
                None => None,
            }
        };
        let cursor = self.inner.sessions.lock().expect("session mutex poisoned").get(&c.0).cloned();
        if cursor.as_ref().is_some_and(|cursor| cursor.identity.session_id != c.1 || cursor.identity.bot_id != c.0 || cursor.identity.run_id != *run || cursor.identity.turn_id != *turn || cursor.identity.generation != c.4 || cursor.revision != c.5) { return Err(E::Unconfirmed); }
        Ok(ExplicitRetireGuard { _bot: bot, _run: admission, key, cursor, control })
    }

    pub(super) fn explicit_retire_quarantined(&self, bot: &str) -> bool {
        self.inner.explicit_quarantine.lock().expect("quarantine mutex poisoned").contains_key(bot)
    }

    pub(super) fn begin_explicit_retire(&self, guard: &ExplicitRetireGuard) -> Option<Arc<()>> {
        let mut quarantine = self.inner.explicit_quarantine.lock().expect("quarantine mutex poisoned");
        match quarantine.entry(guard.key.bot_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                let token = Arc::new(());
                entry.insert(token.clone());
                tracing::info!(reason="api_explicit_retire_in_progress", "API session retirement in progress");
                Some(token)
            }
            std::collections::hash_map::Entry::Occupied(_) => None,
        }
    }

    pub(super) fn clear_explicit_retire(&self, guard: &ExplicitRetireGuard, token: &Arc<()>) -> bool {
        let controls = self.inner.controls.lock().expect("control mutex poisoned");
        let sessions = self.inner.sessions.lock().expect("session mutex poisoned");
        let mut quarantine = self.inner.explicit_quarantine.lock().expect("quarantine mutex poisoned");
        let control_ok = match (&guard.control, controls.get(&guard.key)) {
            (None, None) => true,
            (Some(identity), Some(record)) => Arc::ptr_eq(identity, &record.identity) && record.owner_exited && matches!(*record.joined.borrow(), JoinState::Joined(Ok(_))),
            _ => false,
        } && !controls.keys().any(|key| key.bot_id == guard.key.bot_id && key != &guard.key);
        if !control_ok || sessions.get(&guard.key.bot_id) != guard.cursor.as_ref() || !quarantine.get(&guard.key.bot_id).is_some_and(|current| Arc::ptr_eq(current, token)) { return false; }
        quarantine.remove(&guard.key.bot_id);
        true
    }

    pub(super) fn finish_explicit_retire(&self, guard: &ExplicitRetireGuard, token: &Arc<()>) -> bool {
        let mut controls = self.inner.controls.lock().expect("control mutex poisoned");
        let mut sessions = self.inner.sessions.lock().expect("session mutex poisoned");
        let mut quarantine = self.inner.explicit_quarantine.lock().expect("quarantine mutex poisoned");
        let token_ok = quarantine.get(&guard.key.bot_id).is_some_and(|current| Arc::ptr_eq(current, token));
        let control_ok = match (&guard.control, controls.get(&guard.key)) {
            (None, None) => true,
            (Some(identity), Some(record)) => Arc::ptr_eq(identity, &record.identity) && record.owner_exited && matches!(*record.joined.borrow(), JoinState::Joined(Ok(_))),
            _ => false,
        } && !controls.keys().any(|key| key.bot_id == guard.key.bot_id && key != &guard.key);
        if !token_ok || !control_ok || sessions.get(&guard.key.bot_id) != guard.cursor.as_ref() { return false; }
        if guard.cursor.is_some() { sessions.remove(&guard.key.bot_id); }
        if guard.control.is_some() { controls.remove(&guard.key); }
        quarantine.remove(&guard.key.bot_id);
        true
    }

    async fn spawn_gate(&self, key: &ApiTurnKey) -> OwnedMutexGuard<()> {
        self.admission_gate(&ApiRunKey {
            bot_id: key.bot_id.clone(),
            run_id: key.run_id.clone(),
        })
        .await
    }

    #[cfg(all(test, feature = "daemon-test-harness"))]
    pub(super) fn arm_delivery_lost_ack_for_test(turn: &str) {
        assert!(delivery_lost_acks().lock().unwrap().insert(turn.to_owned()));
    }

    fn spawn_with_dependencies<'a>(
        &'a self,
        app: &'a crate::state::App,
        key: &'a ApiTurnKey,
        dependencies: Option<ApiBotDependencies>,
        native_pending: bool,
    ) -> impl Future<Output = Result<(), ApiBotError>> + Send + 'a {
        Box::pin(self.spawn_with_dependencies_body(app, key, dependencies, native_pending))
    }

    async fn spawn_with_dependencies_body(
        &self,
        app: &crate::state::App,
        key: &ApiTurnKey,
        dependencies: Option<ApiBotDependencies>,
        native_pending: bool,
    ) -> Result<(), ApiBotError> {
        let _bot_admission = self.bot_gate(&key.bot_id).await;
        if self.inner.explicit_quarantine.lock().expect("quarantine mutex poisoned").contains_key(&key.bot_id) { return Err(ApiBotError::Unconfirmed); }
        let _admission = self.spawn_gate(key).await;
        if !native_pending { cleanup_terminal_controls(&app.db, &self.inner).await; }
        let full = FullKey::from(key);
        let prior = {
            let controls = self.inner.controls.lock().expect("control mutex poisoned");
            let mut records = controls.iter().filter(|(k, _)| k.bot_id == full.bot_id && k.run_id == full.run_id);
            match (records.next(), records.next()) {
                (None, _) => None,
                (Some((old, record)), None) => {
                    if !matches!(*record.joined.borrow(), JoinState::Joined(Ok(Completion::Completed | Completion::Failed))) {
                        tracing::info!(reason = "api_same_run_prior_not_completed", "API bot same-run admission remains busy");
                        return Err(ApiBotError::Busy);
                    }
                    if old.turn_id == full.turn_id { return Err(ApiBotError::Busy); }
                    let Some(proof) = record.finalized.clone() else { return Err(ApiBotError::Busy); };
                    Some((old.clone(), record.identity.clone(), proof))
                }
                _ => return Err(ApiBotError::Busy),
            }
        };
        if let Some((old, _, proof)) = &prior {
            if proof.identity.bot_id != old.bot_id || proof.identity.run_id != old.run_id || proof.identity.turn_id != old.turn_id || self.inner.registry.is_active(&full.run_id) {
                return Err(ApiBotError::Busy);
            }
            if !super::same_run::verify(&app.shared(), proof).await { return Err(ApiBotError::Busy); }
            let sessions = self.inner.sessions.lock().expect("session mutex poisoned");
            if !sessions.get(&full.bot_id).is_some_and(|cursor| cursor.identity == proof.identity && cursor.revision == proof.revision) { return Err(ApiBotError::Busy); }
        }

        let (bot, project, run, prompt) = validated_turn(app, key).await?;
        if let Some((_, _, proof)) = &prior {
            let same_conversation: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turns old JOIN turns next ON next.conversation_id=old.conversation_id WHERE old.id=? AND old.run_id=? AND next.id=? AND next.run_id=? AND next.status='in_flight' AND (next.delivery='ok' OR (? AND next.delivery='pending')))")
                .bind(&proof.identity.turn_id).bind(&key.run_id).bind(&key.turn_id).bind(&key.run_id).bind(native_pending)
                .fetch_one(&app.db).await.map_err(|_| ApiBotError::Busy)?;
            if !same_conversation { return Err(ApiBotError::Busy); }
        }
        let cfg = app.cfg.get().await;
        let limits = cfg
            .claude_api
            .api_bot
            .validated(cfg.claude_api.enabled)
            .map_err(|_| ApiBotError::Upstream)?
            .ok_or(ApiBotError::Upstream)?;
        let dependencies = match dependencies {
            Some(dependencies) => dependencies,
            None => production_dependencies(app, &bot, &project, &limits).await?,
        };
        let effort = bot.effort.clone().ok_or(ApiBotError::Upstream)?;
        let options = super::protocol::Options {
            model: bot.model.clone().unwrap_or_else(|| crate::config::API_BOT_MODEL.into()),
            effort,
            fast: bot.fast != 0,
            max_tokens: limits.max_output_tokens,
            compact_at_input_tokens: limits.compact_at_input_tokens,
        };
        options.validate().map_err(|_| ApiBotError::Upstream)?;

        let agent_name = run.agent_name.as_deref().unwrap_or(&bot.name);
        let agent_md = crate::lifecycle::agent_md::load(app, &project).await;
        let fresh_system_text = super::prompt::compose_api_bot_system_prompt(
            &bot,
            agent_name,
            crate::lifecycle::start::bot_cwd(&bot, &project),
            (project.host != crate::config::LOCAL_HOST).then_some(project.host.as_str()),
            Some(&agent_md.text),
        );
        let system = serde_json::Value::String(fresh_system_text.clone());
        let adapter_limits = super::adapter::AdapterLimits {
            max_checkpoint_bytes: limits.max_checkpoint_bytes,
            max_turn_microusd: limits.max_turn_cost_microusd,
            max_requests_per_hour: i64::from(limits.max_requests_per_hour),
            total_cap_usd: cfg.claude_api.api_bot.total_cap_usd,
            total_cap_since: cfg.claude_api.api_bot.total_cap_since.clone(),
        };
        let (identity, expected_cursor, resume_old_admission) = self.identity_for_turn(app, &full).await?;
        let session_cursor = expected_cursor.clone();
        let (adapter, mut state) = if let Some(cursor) = session_cursor {
            let (adapter, state) = match super::adapter::ApiBotAdapter::rebind(
                app.db.clone(),
                &cursor.identity,
                cursor.revision,
                identity.clone(),
                options.clone(),
                system.clone(),
                adapter_limits.clone(),
                dependencies.quote.clone(),
                api_http_config(&dependencies, &limits),
                Some(dependencies.tools.clone()),
            ).await {
                Ok(value) => value,
                Err(_) => {
                    if resume_old_admission.is_some() {
                        let mut sessions = self.inner.sessions.lock().expect("session mutex poisoned");
                        if sessions.get(&key.bot_id) == Some(&cursor) { sessions.remove(&key.bot_id); }
                    }
                    return Err(if prior.is_some() { ApiBotError::Busy } else { ApiBotError::Unconfirmed });
                }
            };
            (Arc::new(adapter.with_progress_sink(progress_sink(app))), state)
        } else {
            let adapter = super::adapter::ApiBotAdapter::new(
                app.db.clone(),
                identity.clone(),
                options.clone(),
                system,
                adapter_limits,
                dependencies.quote.clone(),
                api_http_config(&dependencies, &limits),
                Some(dependencies.tools.clone()),
            )
            .await
            .map_err(|_| ApiBotError::Upstream)?;
            (
                Arc::new(adapter.with_progress_sink(progress_sink(app))),
                super::runner::State {
                    system: Some(fresh_system_text.clone()),
                    ..Default::default()
                },
            )
        };

        drop(resume_old_admission);
        if state.api_pending || state.tool_pending.is_some() {
            return Err(if prior.is_some() { ApiBotError::Busy } else { ApiBotError::Unconfirmed });
        }

        let needs_instruction_update = match &state.system {
            Some(stored) => {
                stored != &fresh_system_text
                    && !already_updated_with(&state.messages, &fresh_system_text)
            }
            None => false,
        };

        let user_content = if needs_instruction_update {
            let update_text = format!("[Instructions updated by AG Man]\n\n{fresh_system_text}");
            if prompt.is_empty() {
                serde_json::json!([
                    {"type": "text", "text": update_text}
                ])
            } else {
                serde_json::json!([
                    {"type": "text", "text": update_text},
                    {"type": "text", "text": prompt}
                ])
            }
        } else {
            serde_json::Value::String(prompt)
        };

        state.messages.push(super::protocol::Message {
            role: "user".into(),
            content: user_content,
        });
        if state.messages.is_empty() {
            return Err(ApiBotError::Upstream);
        }
        use super::runner::Journal;
        adapter
            .save(&state)
            .await
            .map_err(|_| if prior.is_some() { ApiBotError::Busy } else { ApiBotError::Upstream })?;
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("api_run_spawn_after_owner_writes", &key.run_id).await;

        let registry_key = RunTurnKey {
            run_id: key.run_id.clone(),
            turn_id: key.turn_id.clone(),
        };
        let (joined_tx, joined_rx) = watch::channel(JoinState::Pending);
        let identity_tag = Arc::new(());
        let adapter = Arc::clone(&adapter);
        let tools = SharedTools(Arc::clone(&dependencies.tools));
        let max_calls = limits.max_api_calls_per_turn as usize;
        let fail_inner = self.inner.clone();
        let fail_key = full.clone();
        let turn_timeout = Duration::from_secs(limits.turn_timeout_secs);
        let (final_text_tx, final_text_rx) = oneshot::channel();
        let (release_tx, release_rx) = if native_pending { let (tx, rx) = oneshot::channel(); (Some(tx), Some(rx)) } else { (None, None) };
        let (accept_tx, accept_rx) = if native_pending { let (tx, rx) = oneshot::channel(); (Some(tx), Some(rx)) } else { (None, None) };
        let mut join_wait = joined_rx.clone();
        let release_shutdown = app.shutdown.clone();
        #[cfg(all(test, feature = "daemon-test-harness"))]
        let runner_run_id = key.run_id.clone();
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("api_same_run_before_control_cas", &key.run_id).await;
        let (run_ticket, cancel) = {
        let mut controls = self.inner.controls.lock().expect("control mutex poisoned");
        if let Some((old, tag, proof)) = &prior {
            let matches = controls.get(old).is_some_and(|r| Arc::ptr_eq(&r.identity, tag) && r.finalized.as_ref().is_some_and(|p| Arc::ptr_eq(p, proof)));
            if !matches { return Err(ApiBotError::Busy); }
        }
        let run_ticket = self
            .inner
            .registry
            .spawn_tracked(registry_key, move |cancel| async move {
                if let Some(release) = release_rx {
                    let admitted = tokio::select! { biased; _ = release_shutdown.cancelled() => false, _ = cancel.cancelled() => false, result = release => result.is_ok() };
                    if !admitted { return Ok(TurnOutcome::Cancelled); }
                }
                #[cfg(all(test, feature = "daemon-test-harness"))]
                crate::lifecycle::race_point::hit("api_run_runner_before_entry", &runner_run_id)
                    .await;
                let responses_before = state.responses.len();
                let execution = super::runner::run(
                    adapter.as_ref(),
                    &tools,
                    adapter.as_ref(),
                    &mut state,
                    &cancel,
                    max_calls,
                );
                let (result, deadline_elapsed) =
                    await_runner_deadline(execution, &cancel, turn_timeout).await;
                let refusal = adapter.take_refusal();
                let compaction_note = state.responses[responses_before..].iter().rev().find_map(|r| {
                    let has_compaction_block = r.content.iter().any(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("compaction"));
                    let iterations = r.usage.get("iterations").and_then(serde_json::Value::as_array);
                    let comp_it = iterations.and_then(|arr| arr.iter().find(|it| it.get("type").and_then(serde_json::Value::as_str) == Some("compaction")));
                    if has_compaction_block || comp_it.is_some() {
                        let n = comp_it.and_then(|it| it.get("input_tokens")).and_then(serde_json::Value::as_u64);
                        let m = iterations
                            .and_then(|arr| arr.iter().find(|it| it.get("type").and_then(serde_json::Value::as_str) == Some("message")))
                            .and_then(|it| it.get("input_tokens"))
                            .and_then(serde_json::Value::as_u64)
                            .or_else(|| r.usage.get("input_tokens").and_then(serde_json::Value::as_u64));
                        if let (Some(n), Some(m)) = (n, m) {
                            Some(format!("對話已由 API 壓縮（約 {n} tokens → {m} tokens）"))
                        } else {
                            Some("對話已由 API 壓縮".to_string())
                        }
                    } else {
                        None
                    }
                });
                if let Some(note) = compaction_note {
                    fail_inner
                        .compaction_notes
                        .lock()
                        .expect("compaction note mutex poisoned")
                        .insert(fail_key.clone(), note);
                }
                let outcome = if deadline_elapsed {
                    TurnOutcome::Failed("turn_timeout".into())
                } else {
                    if matches!(&result, Ok(super::runner::Halt::MaxCallsReached)) {
                        fail_inner
                            .cap_reached
                            .lock()
                            .expect("cap reached mutex poisoned")
                            .insert(fail_key.clone(), max_calls);
                    }
                    map_runner_result(result, &cancel, refusal)
                };
                if let TurnOutcome::Failed(reason) = &outcome {
                    fail_inner.fail_reasons.lock().expect("fail reason mutex poisoned").insert(fail_key.clone(), reason.clone());
                }
                if matches!(&outcome, TurnOutcome::Completed) {
                    let last_response_with_text = state
                        .responses
                        .iter()
                        .rev()
                        .find(|r| !super::runner::final_text(r).trim().is_empty())
                        .or_else(|| state.responses.last());
                    if let Some(response) = last_response_with_text {
                        let _ = final_text_tx.send(super::completion::FinalText::from_response(response));
                    }
                }
                Ok(outcome)
            })
            .map_err(|_| ApiBotError::Busy)?;

        let cancel = run_ticket.cancellation_token();
        if let Some((old, _, _)) = &prior { controls.remove(old); }
        controls.insert(
                full.clone(),
                ControlRecord {
                    identity: identity_tag.clone(),
                    cancel: cancel.clone(),
                    joined: joined_rx,
                    finalized: None, owner_exited: false,
                },
            );
        (run_ticket, cancel)
        };

        let shutdown = app.shutdown.clone();
        let session_commit = SessionCommit {
            pool: app.db.clone(),
            inner: self.inner.clone(),
            bot_id: key.bot_id.clone(),
            identity,
            expected: expected_cursor,
            #[cfg(test)]
            before_revision: None,
            #[cfg(test)]
            after_commit_before_publish: None,
        };
        let completion_owner = CompletionOwner {
            app: app.shared(),
            key: ApiTurnKey {
                bot_id: key.bot_id.clone(),
                run_id: key.run_id.clone(),
                turn_id: key.turn_id.clone(),
            },
            identity: session_commit.identity.clone(),
            final_text: final_text_rx,
            #[cfg(test)]
            before_storage: None,
            #[cfg(test)]
            retry_observer: None,
            #[cfg(test)]
            store_fault: None,
        };
        let shutdown_timeout = dependencies
            .shutdown_join_timeout
            .unwrap_or(API_BOT_SHUTDOWN_JOIN_TIMEOUT);
        let owner_inner = self.inner.clone();
        let owner_key = full.clone();
        let supervisor_shutdown = shutdown.clone();
        let cancel_before_release = cancel.clone();
        app.background_tasks.spawn(Box::pin(async move {
            let accepted = match accept_rx { Some(rx) => rx.await.unwrap_or(false), None => true };
            let task = tokio::spawn(Box::pin(own_ticket_inner(
                run_ticket,
                cancel,
                shutdown,
                joined_tx,
                shutdown_timeout,
                accepted.then_some(completion_owner),
                accepted.then_some(session_commit),
            )));
            let outcome = task.await;
            #[cfg(all(test, feature = "daemon-test-harness"))]
            crate::lifecycle::race_point::hit("api_same_run_proof_before_cas", &owner_key.run_id).await;
            let mut controls = owner_inner.controls.lock().expect("control mutex poisoned");
            if let Some(record) = controls.get_mut(&owner_key) {
                if Arc::ptr_eq(&record.identity, &identity_tag) {
                    record.owner_exited = true;
                    if let Ok(Some(proof)) = outcome {
                        if !supervisor_shutdown.is_cancelled() && matches!(*record.joined.borrow(), JoinState::Joined(Ok(Completion::Completed | Completion::Failed))) {
                            record.finalized = Some(Arc::new(proof));
                        }
                    }
                }
            }
        }));
        if native_pending {
            #[cfg(all(test, feature = "daemon-test-harness"))]
            crate::lifecycle::race_point::hit("api_native_before_delivery_commit", &key.turn_id).await;
            let admitted = async {
                let mut tx = crate::db::begin_write(&app.db).await.map_err(|_| ())?;
                let changed = sqlx::query("UPDATE turns SET delivery='ok',delivery_verified=1 WHERE id=? AND run_id=? AND status='in_flight' AND delivery='pending' AND EXISTS(SELECT 1 FROM runs r JOIN bots b ON b.id=r.bot_id JOIN projects p ON p.id=b.project_id JOIN conversations c ON c.id=turns.conversation_id AND c.bot_id=b.id WHERE r.id=turns.run_id AND r.id=? AND r.bot_id=? AND r.state='running' AND b.kind='claude-api' AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.handed_off_to IS NULL AND b.cwd IS ? AND p.path=?)")
                    .bind(&key.turn_id).bind(&key.run_id).bind(&key.run_id).bind(&key.bot_id).bind(bot.cwd.as_deref()).bind(&project.path).execute(&mut *tx).await.map_err(|_| ())?;
                if changed.rows_affected() != 1 || app.shutdown.is_cancelled() { tx.rollback().await.map_err(|_| ())?; return Err(()); }
                tx.commit().await.map_err(|_| ())?;
                #[cfg(all(test, feature = "daemon-test-harness"))]
                if delivery_lost_acks().lock().unwrap().remove(&key.turn_id) { return Err(()); }
                Ok::<(), ()>(())
            }.await;
            if admitted.is_err() {
                if let Some(tx) = accept_tx { let _ = tx.send(false); }
                cancel_before_release.cancel();
                let _ = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM turns t JOIN runs r ON r.id=t.run_id JOIN conversations c ON c.id=t.conversation_id WHERE t.id=? AND r.id=? AND r.bot_id=? AND c.bot_id=r.bot_id AND t.delivery='ok')")
                    .bind(&key.turn_id).bind(&key.run_id).bind(&key.bot_id).fetch_one(&app.db).await;
                drop(release_tx);
                drop(_admission);
                drop(_bot_admission);
                let _ = tokio::time::timeout(API_BOT_SHUTDOWN_JOIN_TIMEOUT, wait_join(&mut join_wait)).await;
                return Err(ApiBotError::Unconfirmed);
            }
            #[cfg(all(test, feature = "daemon-test-harness"))]
            crate::lifecycle::race_point::hit("api_native_after_delivery_commit", &key.turn_id).await;
            if app.shutdown.is_cancelled() {
                if let Some(tx) = accept_tx { let _ = tx.send(false); }
                cancel_before_release.cancel();
                drop(release_tx);
                drop(_admission);
                drop(_bot_admission);
                let _ = tokio::time::timeout(API_BOT_SHUTDOWN_JOIN_TIMEOUT, wait_join(&mut join_wait)).await;
                return Err(ApiBotError::Unconfirmed);
            }
            let accepted = accept_tx.is_some_and(|tx| tx.send(true).is_ok());
            let released = if accepted { release_tx.is_some_and(|tx| tx.send(()).is_ok()) } else { drop(release_tx); false };
            if !accepted || !released {
                cancel_before_release.cancel();
                drop(_admission);
                drop(_bot_admission);
                let _ = tokio::time::timeout(API_BOT_SHUTDOWN_JOIN_TIMEOUT, wait_join(&mut join_wait)).await;
                return Err(ApiBotError::Unconfirmed);
            }
        }
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("api_run_spawn_after_registry", &key.run_id).await;
        Ok(())
    }

    async fn identity_for_turn(
        &self,
        app: &crate::state::App,
        key: &FullKey,
    ) -> Result<(super::adapter::AdapterIdentity, Option<SessionCursor>, Option<OwnedMutexGuard<()>>), ApiBotError> {
        let pool = &app.db;
        let session_cursor = {
            self.inner
                .sessions
                .lock()
                .expect("session mutex poisoned")
                .get(&key.bot_id)
                .cloned()
        };
        if let Some(cursor) = session_cursor.clone() {
            let generation = if cursor.identity.run_id == key.run_id {
                cursor.identity.generation
            } else {
                cursor
                    .identity
                    .generation
                    .checked_add(1)
                    .ok_or(ApiBotError::Unconfirmed)?
            };
            return Ok((
                super::adapter::AdapterIdentity {
                    session_id: cursor.identity.session_id.clone(),
                    bot_id: key.bot_id.clone(),
                    run_id: key.run_id.clone(),
                    turn_id: key.turn_id.clone(),
                    generation,
                },
                Some(cursor),
                None,
            ));
        }
        let existing: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL)",
        )
        .bind(&key.bot_id)
        .fetch_one(pool)
        .await
        .map_err(|_| ApiBotError::Upstream)?;
        if existing {
            if !app.api_startup_gate.ready() || app.shutdown.is_cancelled() { return Err(ApiBotError::Unconfirmed); }
            let old: Option<String> = sqlx::query_scalar("SELECT owner_run_id FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL")
                .bind(&key.bot_id).fetch_optional(pool).await.map_err(|_| ApiBotError::Unconfirmed)?.flatten();
            let Some(old) = old else { return Err(ApiBotError::Unconfirmed); };
            if old == key.run_id { return Err(ApiBotError::Unconfirmed); }
            // Caller holds bot gate then new-run admission. Old-run admission comes next;
            // completion retries take only their own run gate and never acquire the bot gate.
            let old_key = ApiRunKey { bot_id: key.bot_id.clone(), run_id: old.clone() };
            let old_admission = self.admission_gate(&old_key).await;
            let busy = {
                let controls = self.inner.controls.lock().expect("control mutex poisoned");
                self.inner.registry.is_active(&old) || self.inner.registry.is_active(&key.run_id)
                    || controls.iter().any(|(k,r)| k.bot_id == key.bot_id && (!r.owner_exited || !matches!(*r.joined.borrow(), JoinState::Joined(Ok(_))) || self.inner.registry.is_active(&k.run_id)))
            };
            if busy { return Err(ApiBotError::Busy); }
            let proof = match super::native_resume::prove(pool, &key.bot_id, &key.run_id, &old).await {
                Ok(proof) => proof,
                Err(error) => { tracing::info!(reason="api_native_resume_unconfirmed", "API session continuity unconfirmed"); return Err(error); }
            };
            if app.shutdown.is_cancelled() { return Err(ApiBotError::Unconfirmed); }
            let cursor = SessionCursor { identity: proof.identity, revision: proof.revision };
            let generation = cursor.identity.generation.checked_add(1).ok_or(ApiBotError::Unconfirmed)?;
            let mut sessions = self.inner.sessions.lock().expect("session mutex poisoned");
            if sessions.contains_key(&key.bot_id) { return Err(ApiBotError::Unconfirmed); }
            sessions.insert(key.bot_id.clone(), cursor.clone());
            return Ok((super::adapter::AdapterIdentity { session_id: cursor.identity.session_id.clone(), bot_id: key.bot_id.clone(), run_id: key.run_id.clone(), turn_id: key.turn_id.clone(), generation }, Some(cursor), Some(old_admission)));
        }
        Ok((
            super::adapter::AdapterIdentity {
                session_id: crate::db::ulid(),
                bot_id: key.bot_id.clone(),
                run_id: key.run_id.clone(),
                turn_id: key.turn_id.clone(),
                generation: 1,
            },
            None,
            None,
        ))
    }

    async fn cancel_and_join_core(
        &self,
        app: &crate::state::App,
        key: &ApiTurnKey,
    ) -> Result<ApiRunCompletion, ApiBotError> {
        let full = FullKey::from(key);
        let bot_admission = self.bot_gate(&key.bot_id).await;
        let admission = self.spawn_gate(key).await;
        let (cancel, mut joined) = {
            let controls = self.inner.controls.lock().expect("control mutex poisoned");
            let Some(record) = controls.get(&full) else {
                return Err(if controls.keys().any(|other| other.bot_id == full.bot_id && other.run_id == full.run_id) {
                    ApiBotError::Busy
                } else {
                    ApiBotError::Unconfirmed
                });
            };
            (record.cancel.clone(), record.joined.clone())
        };
        let stopping: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM runs r JOIN turns t ON t.run_id=r.id WHERE r.id=? AND r.bot_id=? AND t.id=? AND r.state='stopping' AND t.status='in_flight')",
        )
        .bind(&key.run_id)
        .bind(&key.bot_id)
        .bind(&key.turn_id)
        .fetch_one(&app.db)
        .await
        .map_err(|_| ApiBotError::Upstream)?;
        if !stopping {
            return Err(ApiBotError::IdentityMismatch);
        }
        drop(admission);
        drop(bot_admission);
        cancel.cancel();
        let receipt = wait_join(&mut joined).await?;
        match receipt {
            Ok(Completion::Completed) => Ok(ApiRunCompletion::Completed),
            Ok(Completion::Cancelled) => Ok(ApiRunCompletion::Cancelled),
            Ok(Completion::Failed) => Ok(ApiRunCompletion::Failed),
            Err(JoinError::Unconfirmed) => Err(ApiBotError::Unconfirmed),
            Err(JoinError::IdentityMismatch) => Err(ApiBotError::IdentityMismatch),
        }
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub(super) fn control_count_for_test(&self) -> usize {
        self.inner
            .controls
            .lock()
            .expect("control mutex poisoned")
            .len()
    }
}

impl Default for ApiBotRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::lifecycle::api_bot_port::ApiBotPort for crate::state::App {
    fn api_native_prompt(&self, bot_id: &str, text: &str, crid: &str) -> impl Future<Output = Result<crate::lifecycle::PromptOut, crate::lifecycle::api_bot_port::ApiNativePromptError>> + Send {
        async move { super::native_prompt::prompt(&self.shared(), bot_id, text, crid).await }
    }
    fn api_native_start(&self, bot_id: &str, mode: crate::lifecycle::api_bot_port::ApiNativeStartMode) -> impl Future<Output = Result<crate::lifecycle::api_bot_port::ApiNativeStartResult, crate::lifecycle::api_bot_port::ApiNativeStartError>> + Send {
        async move { super::native_start::start(&self.shared(), bot_id, mode).await }
    }
    async fn api_fresh_target(&self, bot_id: &str) -> crate::lifecycle::api_bot_port::ApiFreshAvailability {
        use crate::lifecycle::api_bot_port::{ApiFreshAvailability as A, ApiFreshTarget};
        match crate::db::bot(&self.db, bot_id).await { Ok(Some(bot)) if bot.kind == "claude-api" && bot.deleted_at.is_none() => {}, _ => return A::Unavailable };
        let _gate = match tokio::time::timeout(Duration::from_secs(3), self.api_bot_runtime.bot_gate(bot_id)).await { Ok(guard) => guard, Err(_) => return A::Unavailable };
        let bot = match crate::db::bot(&self.db, bot_id).await { Ok(Some(bot)) if bot.kind == "claude-api" && bot.deleted_at.is_none() => bot, _ => return A::Unavailable };
        if !self.api_startup_gate.ready() || self.shutdown.is_cancelled() || self.api_bot_runtime.explicit_retire_quarantined(&bot.id) { return A::Unavailable; }
        if !matches!(crate::db::project(&self.db, &bot.project_id).await, Ok(Some(p)) if p.deleted_at.is_none() && p.handed_off_to.is_none()) { return A::Unavailable; }
        let expiry = crate::db::ts_sql("expires_at");
        let sql = format!("SELECT id,generation,{expiry}>? FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL");
        match sqlx::query_as::<_, (String,i64,bool)>(&sql).bind(crate::db::now()).bind(&bot.id).fetch_optional(&self.db).await {
            Ok(Some((session_id,generation,true))) if generation > 0 => A::Available(ApiFreshTarget { session_id, generation }),
            Ok(None) => A::None,
            _ => A::Unavailable,
        }
    }
    fn api_recovery_ready(&self) -> bool { self.api_startup_gate.ready() }
    fn api_explicit_retire(&self, bot_id: &str, target: &crate::lifecycle::api_bot_port::ApiFreshTarget) -> impl Future<Output = Result<crate::lifecycle::api_bot_port::ApiFreshResult, crate::lifecycle::api_bot_port::ApiFreshError>> + Send {
        async move { super::explicit_retirement::retire(&self.shared(), bot_id, target).await }
    }
    fn api_run_begin_stop(&self, key: &ApiRunKey, mode: ApiBeginStopMode) -> impl Future<Output = Result<ApiBeginStopResult, ApiBeginStopError>> + Send {
        async move {
            let bot_guard = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => return Err(ApiBeginStopError::Busy),
                guard = self.api_bot_runtime.bot_gate(&key.bot_id) => guard,
            };
            let guard = tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => return Err(ApiBeginStopError::Busy),
                guard = self.api_bot_runtime.admission_gate(key) => guard,
            };
            if self.shutdown.is_cancelled() { return Err(ApiBeginStopError::Busy); }
            let result = crate::lifecycle::stop::api_begin_stop_transaction(&self.db, key, mode, &self.shutdown).await;
            drop(guard);
            drop(bot_guard);
            result
        }
    }
    fn api_turn_spawn_pending(&self, key: &ApiTurnKey) -> impl Future<Output = Result<(), ApiBotError>> + Send {
        async move {
            if !self.api_startup_gate.ready() { return Err(ApiBotError::Upstream); }
            self.api_bot_runtime.spawn_native(self, key).await
        }
    }

    fn api_turn_spawn(
        &self,
        key: &ApiTurnKey,
    ) -> impl Future<Output = Result<(), ApiBotError>> + Send {
        async move {
            if !self.api_startup_gate.ready() { return Err(ApiBotError::Upstream); }
            self.api_bot_runtime.spawn(self, key).await
        }
    }

    fn api_run_cancel_and_join(
        &self,
        key: &ApiTurnKey,
    ) -> impl Future<Output = Result<ApiRunCompletion, ApiBotError>> + Send {
        async move { self.api_bot_runtime.cancel_and_join(self, key).await }
    }

    fn api_run_stop_proof(
        &self,
        key: &ApiRunKey,
    ) -> impl Future<Output = Result<ApiRunStopProof, ApiBotError>> + Send {
        async move { self.api_bot_runtime.stop_proof(self, key).await }
    }

    fn api_run_live(&self, key: &ApiTurnKey) -> impl Future<Output = bool> + Send {
        async move { self.api_bot_runtime.live(self, key).await }
    }
}

#[derive(Clone)]
struct SessionCommit {
    pool: sqlx::SqlitePool,
    inner: Arc<RuntimeInner>,
    bot_id: String,
    identity: super::adapter::AdapterIdentity,
    expected: Option<SessionCursor>,
    #[cfg(test)]
    before_revision: Option<Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    after_commit_before_publish: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}

struct CompletionOwner {
    app: Arc<crate::state::App>,
    key: ApiTurnKey,
    identity: super::adapter::AdapterIdentity,
    final_text: oneshot::Receiver<super::completion::FinalText>,
    #[cfg(test)]
    before_storage: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    #[cfg(test)]
    retry_observer: Option<(Arc<std::sync::atomic::AtomicUsize>, Arc<tokio::sync::Notify>)>,
    #[cfg(test)]
    store_fault: Option<(CompletionStoreFault, Arc<std::sync::atomic::AtomicUsize>)>,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CompletionStoreFault {
    AmbiguousBeforeStore,
    AmbiguousAfterCommit,
}

async fn own_ticket_inner(
    ticket: RunTicket,
    cancel: CancellationToken,
    shutdown: CancellationToken,
    result_tx: watch::Sender<JoinState>,
    shutdown_timeout: Duration,
    completion_owner: Option<CompletionOwner>,
    session_commit: Option<SessionCommit>,
) -> Option<super::same_run::Finalized> {
    let join = ticket.join();
    tokio::pin!(join);
    let state = tokio::select! {
        result = &mut join => JoinState::Joined(map_join_result(result)),
        _ = shutdown.cancelled() => {
            cancel.cancel();
            match tokio::time::timeout(shutdown_timeout, &mut join).await {
                Ok(result) => JoinState::Joined(map_join_result(result)),
                Err(_) => JoinState::TimedOut,
            }
        }
    };
    if state == JoinState::TimedOut {
        tracing::warn!("API bot task join exceeded shutdown deadline; outcome remains unconfirmed");
    }
    // This receipt certifies only the real task join. A storage transaction must never delay or
    // rewrite it; stop waits on this watch before doing its own recovery transaction.
    result_tx.send_replace(state);

    let joined = match state {
        JoinState::Joined(Ok(completion)) => Some(completion),
        _ => None,
    };
    match (completion_owner, joined) {
        (Some(mut owner), Some(joined)) => {
            let final_text = if joined == Completion::Completed {
                (&mut owner.final_text).await.ok()
            } else { None };
            let fail_reason = owner.app.api_bot_runtime.inner.fail_reasons.lock().expect("fail reason mutex poisoned").remove(&FullKey::from(&owner.key));
            let cap_reached = owner.app.api_bot_runtime.inner.cap_reached.lock().expect("cap reached mutex poisoned").remove(&FullKey::from(&owner.key));
            let compaction_note = owner.app.api_bot_runtime.inner.compaction_notes.lock().expect("compaction note mutex poisoned").remove(&FullKey::from(&owner.key));
            if let Some((committed, payload, published)) = settle_joined_completion(
                &owner, joined, final_text, &shutdown, session_commit.as_ref(),
            ).await {
                let revision = committed.session_revision;
                super::completion::emit_after_commit(&owner.app, &owner.key, committed).await;
                if let Some(note) = compaction_note {
                    super::completion::post_compaction_notice(&owner.app, &owner.key, &note).await;
                }
                if joined == Completion::Failed {
                    super::completion::post_failure_notice(&owner.app, &owner.key, fail_reason.as_deref().unwrap_or("unknown")).await;
                } else if let Some(n) = cap_reached {
                    super::completion::post_cap_notice(&owner.app, &owner.key, n).await;
                }
                // 乾淨地結束（完成，或失敗但 checkpoint 已由 receipt 結清）才算 same-run 可接續；失敗後的下一則由 checkpoint 決定能不能繼續
                // （有 unknown 的送出會在 rebind 時以 pending_unknown 明確失敗），不再卡在 in_flight/pending。
                let clean = (joined == Completion::Completed && payload.outcome() == super::receipt::JoinOutcome::Completed)
                    || (joined == Completion::Failed && payload.outcome() == super::receipt::JoinOutcome::Failed);
                if clean && published {
                    #[cfg(all(test, feature = "daemon-test-harness"))]
                    crate::lifecycle::race_point::hit("api_same_run_owner_before_exit", &owner.key.run_id).await;
                    if shutdown.is_cancelled() { return None; }
                    return Some(super::same_run::Finalized { identity: owner.identity, revision, payload });
                }
            }
        }
        (None, Some(_)) => {
            // The only remaining caller is the isolated post-join cursor test path; production
            // always supplies a CompletionOwner and the transaction's exact revision.
            if let Some(commit) = session_commit {
                #[cfg(test)]
                if let Some(notify) = &commit.before_revision {
                    notify.notify_one();
                }
                let revision = if shutdown.is_cancelled() {
                    None
                } else {
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => None,
                        revision = stored_revision(&commit.pool, &commit.identity) => revision,
                    }
                };
                if let Some(revision) = revision {
                    publish_session_cursor(commit, revision);
                }
            }
        }
        _ => {}
    }
    None
}

#[cfg(test)]
async fn own_ticket(ticket: RunTicket, cancel: CancellationToken, shutdown: CancellationToken, result_tx: watch::Sender<JoinState>, shutdown_timeout: Duration, completion_owner: Option<CompletionOwner>, session_commit: Option<SessionCommit>) {
    let record = completion_owner.as_ref().and_then(|owner| {
        let inner = owner.app.api_bot_runtime.inner.clone();
        let key = FullKey::from(&owner.key);
        let identity = inner.controls.lock().ok()?.get(&key)?.identity.clone();
        Some((inner, key, identity))
    });
    let task = tokio::spawn(Box::pin(own_ticket_inner(ticket, cancel, shutdown, result_tx, shutdown_timeout, completion_owner, session_commit)));
    let _ = task.await;
    if let Some((inner, key, identity)) = record {
        let mut controls = inner.controls.lock().expect("control mutex poisoned");
        if let Some(record) = controls.get_mut(&key) {
            if Arc::ptr_eq(&record.identity, &identity) { record.owner_exited = true; }
        }
    }
}

async fn admission_gate_for(
    inner: &Arc<RuntimeInner>,
    key: &ApiRunKey,
) -> OwnedMutexGuard<()> {
    let gate_key = AdmissionKey {
        bot_id: key.bot_id.clone(),
        run_id: key.run_id.clone(),
    };
    let gate = {
        let mut gates = inner.admission.lock().await;
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&gate_key).and_then(Weak::upgrade) {
            gate
        } else {
            let gate = Arc::new(AsyncMutex::new(()));
            gates.insert(gate_key, Arc::downgrade(&gate));
            gate
        }
    };
    gate.lock_owned().await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompletionRecoveryReason {
    ShortRetry,
    Backoff,
    AwaitingProof,
    Recovered,
    Shutdown,
    Fenced,
    OwnerEnded,
}
impl CompletionRecoveryReason {
    fn code(self) -> &'static str {
        match self {
            Self::ShortRetry => "api_completion_short_retry",
            Self::Backoff => "api_completion_backoff",
            Self::AwaitingProof => "api_completion_awaiting_proof",
            Self::Recovered => "api_completion_recovered",
            Self::Shutdown => "api_completion_shutdown",
            Self::Fenced => "api_completion_fenced",
            Self::OwnerEnded => "api_completion_owner_ended",
        }
    }
}
fn log_completion_recovery(key: &ApiTurnKey, reason: CompletionRecoveryReason, failures: usize) {
    tracing::info!(bot = %key.bot_id, run = %key.run_id, turn = %key.turn_id,
        reason = reason.code(), failures, "API bot joined-completion recovery");
}
fn completion_retry_delay(failures: usize) -> Duration {
    if failures < API_BOT_COMPLETION_MAX_ATTEMPTS {
        return API_BOT_COMPLETION_RETRY_DELAY;
    }
    Duration::from_secs(
        1u64 << failures
            .saturating_sub(API_BOT_COMPLETION_MAX_ATTEMPTS)
            .min(5),
    )
    .min(Duration::from_secs(30))
}

async fn completion_retry_wait(shutdown: &CancellationToken, delay: Duration) -> bool {
    tokio::select! { biased; _ = shutdown.cancelled() => false, _ = tokio::time::sleep(delay) => true }
}

// Keep the multi-await recovery frame out of own_ticket's debug spawn/call stack.
fn settle_joined_completion<'a>(
    owner: &'a CompletionOwner,
    joined: Completion,
    final_text: Option<super::completion::FinalText>,
    shutdown: &'a CancellationToken,
    session_commit: Option<&'a SessionCommit>,
) -> impl Future<Output = Option<(super::completion::Committed, super::receipt::Payload, bool)>> + Send + 'a {
    Box::pin(settle_joined_completion_body(
        owner,
        joined,
        final_text,
        shutdown,
        session_commit,
    ))
}

async fn settle_joined_completion_body(
    owner: &CompletionOwner,
    joined: Completion,
    final_text: Option<super::completion::FinalText>,
    shutdown: &CancellationToken,
    session_commit: Option<&SessionCommit>,
) -> Option<(super::completion::Committed, super::receipt::Payload, bool)> {
    let payload = match (joined, final_text.as_ref()) {
        (Completion::Completed, Some(text)) => text.receipt_payload(),
        _ => super::receipt::Payload::ended(super::receipt::JoinOutcome::Failed),
    };
    let Ok(payload) = payload else {
        warn_completion_storage(&owner.key);
        return None;
    };
    drop(final_text); // The recovery loop retains only the one bounded visible payload.
    let mut failures = 0usize;
    let mut needs_proof = false;
    let mut last_reason = None;
    loop {
        if shutdown.is_cancelled() {
            log_completion_recovery(&owner.key, CompletionRecoveryReason::Shutdown, failures);
            return None;
        }
        let admission = if let Some(commit) = session_commit {
            let key = ApiRunKey {
                bot_id: commit.bot_id.clone(),
                run_id: commit.identity.run_id.clone(),
            };
            Some(tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    log_completion_recovery(&owner.key, CompletionRecoveryReason::Shutdown, failures);
                    return None;
                }
                guard = admission_gate_for(&commit.inner, &key) => guard,
            })
        } else {
            None
        };
        #[cfg(test)]
        if failures == 0 {
            if let Some(notify) = &owner.before_storage {
                notify.0.notify_one();
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return None,
                    _ = notify.1.notified() => {},
                }
            }
        }
        // An unconfirmed commit switches to read-only proof polling. Never retry a write blindly.
        let result = if needs_proof {
            Err(super::completion::StoreError::Ambiguous)
        } else {
            #[cfg(test)]
            {
                let injected = owner.store_fault.as_ref().map(|(fault, calls)| {
                    (
                        *fault,
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                    )
                });
                store_joined_completion_with_test_fault(owner, &payload, injected).await
            }
            #[cfg(not(test))]
            {
                store_joined_completion(owner, &payload).await
            }
        };
        let committed = match result {
            Ok(super::completion::Outcome::Committed(c)) => Some(c),
            Ok(super::completion::Outcome::Fenced) => {
                log_completion_recovery(&owner.key, CompletionRecoveryReason::Fenced, failures);
                return None;
            }
            Err(super::completion::StoreError::Retryable) => {
                if retire_stale_completion(owner).await {
                    log_completion_recovery(
                        &owner.key,
                        CompletionRecoveryReason::OwnerEnded,
                        failures,
                    );
                    return None;
                }
                None
            }
            Err(super::completion::StoreError::Ambiguous) => {
                match super::completion::probe_after_join(&owner.app, &owner.identity, &payload)
                    .await
                {
                    Ok(super::completion::Probe::Applied(c)) => Some(c),
                    Ok(super::completion::Probe::Pending) => {
                        needs_proof = false;
                        None
                    }
                    Ok(super::completion::Probe::Stale(item)) => {
                        if super::receipt::fence_stale(&owner.app.db, &item)
                            .await
                            .is_err()
                        {
                            warn_completion_storage(&owner.key);
                        }
                        log_completion_recovery(
                            &owner.key,
                            CompletionRecoveryReason::Fenced,
                            failures,
                        );
                        return None;
                    }
                    _ => {
                        needs_proof = true;
                        // This only retires an old completion owner; it never creates join proof.
                        if retire_stale_completion(owner).await {
                            log_completion_recovery(
                                &owner.key,
                                CompletionRecoveryReason::OwnerEnded,
                                failures,
                            );
                            return None;
                        }
                        None
                    }
                }
            }
        };
        if let Some(committed) = committed {
            #[cfg(test)]
            if let Some(commit) = session_commit {
                if let Some((reached, release)) = &commit.after_commit_before_publish {
                    reached.notify_one();
                    tokio::select! { biased; _ = shutdown.cancelled() => return None, _ = release.notified() => {} }
                }
            }
            if shutdown.is_cancelled() {
                log_completion_recovery(&owner.key, CompletionRecoveryReason::Shutdown, failures);
                return None;
            }
            let published = session_commit.is_some_and(|commit| publish_session_cursor(commit.clone(), committed.session_revision));
            drop(admission);
            if failures != 0 {
                log_completion_recovery(&owner.key, CompletionRecoveryReason::Recovered, failures);
            }
            return Some((committed, payload, published));
        }
        drop(admission); // All retry waits below are outside admission/bot/session locks.
        failures = failures.saturating_add(1);
        let reason = if needs_proof {
            CompletionRecoveryReason::AwaitingProof
        } else if failures >= API_BOT_COMPLETION_MAX_ATTEMPTS {
            CompletionRecoveryReason::Backoff
        } else {
            CompletionRecoveryReason::ShortRetry
        };
        if last_reason != Some(reason) {
            log_completion_recovery(&owner.key, reason, failures);
            last_reason = Some(reason);
        }
        #[cfg(test)]
        if let Some((count, notify)) = &owner.retry_observer {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            notify.notify_one();
        }
        #[cfg(all(test, feature = "daemon-test-harness"))]
        if failures >= API_BOT_COMPLETION_MAX_ATTEMPTS {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    log_completion_recovery(&owner.key, CompletionRecoveryReason::Shutdown, failures);
                    return None;
                }
                _ = crate::lifecycle::race_point::hit("api_completion_backoff", &owner.key.run_id) => {},
            }
        }
        if !completion_retry_wait(shutdown, completion_retry_delay(failures)).await {
            log_completion_recovery(&owner.key, CompletionRecoveryReason::Shutdown, failures);
            return None;
        }
    }
}

async fn retire_stale_completion(owner: &CompletionOwner) -> bool {
    if !matches!(
        super::completion::completion_owner_is_current_result(
            &owner.app,
            &owner.key,
            &owner.identity
        )
        .await,
        Ok(false)
    ) {
        return false; // Read errors never mean the owner ended.
    }
    if let Ok(Some(stored)) = super::receipt::load(&owner.app.db, &owner.identity).await {
        if matches!(stored.state, super::receipt::State::Pending)
            && super::receipt::fence_stale(&owner.app.db, &stored.item)
                .await
                .is_err()
        {
            warn_completion_storage(&owner.key);
        }
    }
    true
}

async fn store_joined_completion(
    owner: &CompletionOwner,
    payload: &super::receipt::Payload,
) -> Result<super::completion::Outcome, super::completion::StoreError> {
    super::completion::store_durable_after_join(&owner.app, &owner.key, &owner.identity, payload)
        .await
}

#[cfg(test)]
async fn store_joined_completion_with_test_fault(
    owner: &CompletionOwner,
    payload: &super::receipt::Payload,
    injected_fault: Option<(CompletionStoreFault, usize)>,
) -> Result<super::completion::Outcome, super::completion::StoreError> {
    if injected_fault == Some((CompletionStoreFault::AmbiguousBeforeStore, 0)) {
        return Err(super::completion::StoreError::Ambiguous);
    }
    let stored = store_joined_completion(owner, payload).await;
    if injected_fault == Some((CompletionStoreFault::AmbiguousAfterCommit, 0))
        && matches!(&stored, Ok(super::completion::Outcome::Committed(_)))
    {
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("api_completion_ack_lost", &owner.key.run_id).await;
        return Err(super::completion::StoreError::Ambiguous);
    }
    stored
}

fn warn_completion_storage(key: &ApiTurnKey) {
    tracing::warn!(
        bot = %key.bot_id,
        run = %key.run_id,
        turn = %key.turn_id,
        error_code = API_BOT_COMPLETION_STORAGE_ERROR,
        "joined API-bot completion storage requires recovery"
    );
}

fn publish_session_cursor(commit: SessionCommit, revision: i64) -> bool {
    let mut sessions = commit
        .inner
        .sessions
        .lock()
        .expect("session mutex poisoned");
    if sessions.get(&commit.bot_id) != commit.expected.as_ref() {
        return false;
    }
    sessions.insert(
        commit.bot_id,
        SessionCursor {
            identity: commit.identity,
            revision,
        },
    );
    true
}

fn map_join_result(result: Result<TurnOutcome, RegistryError>) -> Result<Completion, JoinError> {
    match result {
        Ok(TurnOutcome::Completed) => Ok(Completion::Completed),
        Ok(TurnOutcome::Cancelled) => Ok(Completion::Cancelled),
        Ok(TurnOutcome::Failed(_)) => Ok(Completion::Failed),
        Err(RegistryError::IdentityMismatch) => Err(JoinError::IdentityMismatch),
        Err(_) => Err(JoinError::Unconfirmed),
    }
}

async fn wait_join(
    joined: &mut watch::Receiver<JoinState>,
) -> Result<Result<Completion, JoinError>, ApiBotError> {
    loop {
        match *joined.borrow_and_update() {
            JoinState::Pending => {}
            JoinState::Joined(result) => return Ok(result),
            JoinState::TimedOut => return Err(ApiBotError::Unconfirmed),
        }
        joined
            .changed()
            .await
            .map_err(|_| ApiBotError::Unconfirmed)?;
    }
}

struct SharedTools(Arc<dyn super::runner::Tools>);

impl super::runner::Tools for SharedTools {
    fn execute<'a>(
        &'a self,
        call: &'a super::protocol::ToolCall,
        cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<String, String>> {
        self.0.execute(call, cancel)
    }
}

#[derive(Clone)]
struct ApiBotDependencies {
    endpoint: String,
    key_source: Arc<dyn super::adapter::KeySource>,
    quote: Arc<dyn super::quote::QuoteProvider>,
    tools: Arc<dyn super::runner::Tools>,
    shutdown_join_timeout: Option<Duration>,
}

/// 串流進度 → WS `turn_progress` 幀（即時廣播、不寫 DB；採 spawn 因為回呼是同步的）。
fn progress_sink(app: &crate::state::App) -> super::adapter::ProgressSink {
    let weak = Arc::downgrade(&app.shared());
    Arc::new(move |frame| {
        if let Some(app) = weak.upgrade() {
            tokio::spawn(async move { app.emit("turn_progress", frame).await; });
        }
    })
}

struct ConfigKeySource(std::sync::Weak<crate::state::App>);
impl super::adapter::KeySource for ConfigKeySource {
    fn snapshot(&self) -> futures::future::BoxFuture<'_, Result<super::adapter::Secret, ()>> {
        Box::pin(async move {
            let app = self.0.upgrade().ok_or(())?;
            let cfg = app.cfg.get().await;
            cfg.claude_api.api_bot.validated(cfg.claude_api.enabled).map_err(|_| ())?.ok_or(())?;
            crate::key_file::read_key(&cfg.claude_api.key_file).map(super::adapter::Secret::new).map_err(|_| ())
        })
    }
}

#[cfg(target_os = "linux")]
fn native_bwrap_available() -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata("/usr/bin/bwrap").is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(target_os = "linux"))]
fn native_bwrap_available() -> bool { false }

/// 探測 bwrap 能不能真的開 user namespace（Ubuntu 24 的 AppArmor 限制會讓它失敗）。不打 API、不碰 key。
async fn bwrap_userns_works() -> bool {
    let mut cmd = tokio::process::Command::new("/usr/bin/bwrap");
    cmd.args(["--unshare-user", "--ro-bind", "/", "/", "/usr/bin/true"]).env_clear()
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else { return false };
    matches!(tokio::time::timeout(Duration::from_secs(5), child.wait()).await, Ok(Ok(st)) if st.success())
}

/// 專案在本機＝LocalTools（bwrap 沙箱，要先確認 bwrap）；在遠端主機＝RemoteTools（ssh、不隔離，使用者 2026-11-02 裁示）。
async fn production_dependencies(app: &crate::state::App, bot: &crate::db::Bot, project: &crate::db::Project, limits: &am_config::api_bot::ValidatedLimits) -> Result<ApiBotDependencies, ApiBotError> {
    let root = crate::lifecycle::start::bot_cwd(bot, project);
    let tool_limits = super::tools::Limits { timeout: Duration::from_secs(limits.command_timeout_secs), output_bytes: limits.max_tool_output_bytes };
    let tools: Arc<dyn super::runner::Tools> = if project.host == crate::config::LOCAL_HOST {
        if !native_bwrap_available() { return Err(ApiBotError::Upstream); }
        Arc::new(super::tools::LocalTools::new(std::path::Path::new(root), tool_limits).map_err(|_| ApiBotError::Upstream)?)
    } else {
        let conn = app.hosts.get(&project.host).await.ok_or(ApiBotError::Upstream)?;
        Arc::new(super::tools::RemoteTools::new(conn, root, tool_limits).map_err(|_| ApiBotError::Upstream)?)
    };
    let key_source = Arc::new(ConfigKeySource(Arc::downgrade(&app.shared())));
    Ok(ApiBotDependencies { endpoint: "https://api.anthropic.com/v1/messages".into(), key_source, quote: Arc::new(super::pricing::DirectGlobal), tools, shutdown_join_timeout: None })
}

fn api_http_config(
    dependencies: &ApiBotDependencies,
    limits: &am_config::api_bot::ValidatedLimits,
) -> super::adapter::AdapterHttpConfig {
    super::adapter::AdapterHttpConfig {
        endpoint: dependencies.endpoint.clone(),
        key_source: dependencies.key_source.clone(),
        timeout: Duration::from_secs(limits.turn_timeout_secs),
        max_response_bytes: limits.max_checkpoint_bytes.min(8 * 1024 * 1024),
    }
}

async fn validated_turn(
    app: &crate::state::App,
    key: &ApiTurnKey,
) -> Result<(crate::db::Bot, crate::db::Project, crate::db::Run, String), ApiBotError> {
    let bot = crate::db::bot(&app.db, &key.bot_id)
        .await
        .map_err(|_| ApiBotError::Upstream)?
        .ok_or(ApiBotError::IdentityMismatch)?;
    if bot.deleted_at.is_some() || bot.kind != "claude-api" {
        return Err(ApiBotError::IdentityMismatch);
    }
    let project = crate::db::project(&app.db, &bot.project_id)
        .await
        .map_err(|_| ApiBotError::Upstream)?
        .ok_or(ApiBotError::IdentityMismatch)?;
    if project.deleted_at.is_some()
        || project.handed_off_to.is_some()
    {
        return Err(ApiBotError::IdentityMismatch);
    }
    let run = crate::db::run(&app.db, &key.run_id)
        .await
        .map_err(|_| ApiBotError::Upstream)?
        .ok_or(ApiBotError::IdentityMismatch)?;
    if run.bot_id != key.bot_id || run.state != "running" {
        return Err(ApiBotError::IdentityMismatch);
    }
    let prompt: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT prompt_text FROM turns WHERE id=? AND run_id=? AND status='in_flight'",
    )
    .bind(&key.turn_id)
    .bind(&key.run_id)
    .fetch_optional(&app.db)
    .await
    .map_err(|_| ApiBotError::Upstream)?;
    let prompt = prompt
        .filter(|text| !text.trim().is_empty())
        .ok_or(ApiBotError::IdentityMismatch)?;
    Ok((bot, project, run, prompt))
}

fn map_runner_result(
    result: Result<super::runner::Halt, super::runner::Error>,
    cancel: &CancellationToken,
    refusal: Option<String>,
) -> TurnOutcome {
    use super::http::Error as H;
    match result {
        Ok(
            super::runner::Halt::EndTurn
            | super::runner::Halt::Refusal
            | super::runner::Halt::MaxCallsReached,
        ) => TurnOutcome::Completed,
        Ok(super::runner::Halt::MaxTokens) => TurnOutcome::Failed("max_tokens".into()),
        Ok(super::runner::Halt::Other(_)) => TurnOutcome::Failed("unknown_stop".into()),
        Err(super::runner::Error::Cancelled) => TurnOutcome::Cancelled,
        Err(super::runner::Error::Model(H::Cancelled)) if cancel.is_cancelled() => TurnOutcome::Cancelled,
        Err(super::runner::Error::Budget) => TurnOutcome::Failed("api_bot_max_calls_reached".into()),
        // 預留被拒絕時儲存失敗會冒成 Journal；有明確原因（額度、key、報價）就用它。
        Err(super::runner::Error::Journal) => TurnOutcome::Failed(refusal.unwrap_or_else(|| "journal".into())),
        Err(super::runner::Error::PendingUnknown) => TurnOutcome::Failed("pending_unknown".into()),
        Err(super::runner::Error::InvalidResponse) => TurnOutcome::Failed("invalid_response".into()),
        Err(super::runner::Error::Model(e)) => TurnOutcome::Failed(match e {
            H::RateLimited { .. } => "api_rate_limited".into(),
            H::Http { status, kind, message, request_id } => {
                super::completion::format_http_failure_reason(
                    status,
                    kind.as_deref(),
                    message.as_deref(),
                    request_id.as_deref(),
                )
            }
            H::StreamAborted { kind, message } => super::completion::format_stream_abort_reason(kind.as_deref(), message.as_deref()),
            H::Timeout => "api_timeout".into(),
            H::TransportUnknown => "api_transport_unknown".into(),
            H::TooLarge | H::InvalidResponse => "api_invalid_response".into(),
            H::InvalidRequest | H::Cancelled => "api_invalid_request".into(),
        }),
    }
}

async fn await_runner_deadline<F: Future>(
    future: F,
    cancel: &CancellationToken,
    deadline: Duration,
) -> (F::Output, bool) {
    tokio::pin!(future);
    let timer = tokio::time::sleep(deadline);
    tokio::pin!(timer);
    tokio::select! {
        biased;
        _ = &mut timer => {
            cancel.cancel();
            (future.await, true)
        }
        result = &mut future => (result, false),
    }
}

async fn stored_revision(
    pool: &sqlx::SqlitePool,
    identity: &super::adapter::AdapterIdentity,
) -> Option<i64> {
    sqlx::query_scalar(
        "SELECT revision FROM api_bot_sessions WHERE id=? AND bot_id=? AND owner_run_id=? AND owner_turn_id=? AND generation=? AND retired_at IS NULL",
    )
    .bind(&identity.session_id)
    .bind(&identity.bot_id)
    .bind(&identity.run_id)
    .bind(&identity.turn_id)
    .bind(identity.generation)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}


// Startup-only capabilities: private metadata, never a new join or wire result.
#[cfg(all(test, feature = "daemon-test-harness"))]
fn delivery_lost_acks() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static FAULTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    FAULTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

pub(super) struct NativeStartGuard {
    _bot: OwnedMutexGuard<()>,
    _run: OwnedMutexGuard<()>,
}

pub(super) struct StartupGuard {
    identity: super::adapter::AdapterIdentity,
    inner: Arc<RuntimeInner>,
    _admission: OwnedMutexGuard<()>,
}
pub(super) enum StartupError {
    NotStartup,
    Busy,
    Storage,
}
impl StartupError {
    pub(super) fn code(&self) -> &'static str {
        match self {
            Self::NotStartup => "api_startup_not_exclusive",
            Self::Busy => "api_startup_live_owner",
            Self::Storage => "api_startup_storage",
        }
    }
}
pub(super) enum StartupCheckpoint {
    Safe,
    Fenced,
    Unresolved,
}
pub(super) enum StartupRestore {
    Restored,
    AlreadyCurrent,
    Fenced,
    Unresolved,
}

#[cfg(test)]
pub(super) struct StartupFixtureOwner {
    pub(super) shutdown: CancellationToken,
    pub(super) retries: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) wake: Arc<tokio::sync::Notify>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl ApiBotRuntime {
    fn startup_allowed(
        &self,
        app: &crate::state::App,
        identity: &super::adapter::AdapterIdentity,
    ) -> bool {
        std::ptr::eq(self, &app.api_bot_runtime)
            && !app.is_startup_ready()
            && self
                .dependencies
                .lock()
                .expect("dependency mutex poisoned")
                .is_none()
            && !self.inner.registry.is_active(&identity.run_id)
            && !self
                .inner
                .controls
                .lock()
                .expect("control mutex poisoned")
                .keys()
                .any(|key| key.bot_id == identity.bot_id && key.run_id == identity.run_id)
    }
    pub(super) async fn startup_guard(
        &self,
        app: &crate::state::App,
        identity: &super::adapter::AdapterIdentity,
    ) -> Result<StartupGuard, StartupError> {
        if !std::ptr::eq(self, &app.api_bot_runtime)
            || app.is_startup_ready()
            || self
                .dependencies
                .lock()
                .expect("dependency mutex poisoned")
                .is_some()
        {
            return Err(StartupError::NotStartup);
        }
        if !self.startup_allowed(app, identity) {
            return Err(StartupError::Busy);
        }
        let key = ApiRunKey {
            bot_id: identity.bot_id.clone(),
            run_id: identity.run_id.clone(),
        };
        let admission = tokio::select! { biased; _=app.shutdown.cancelled()=>return Err(StartupError::NotStartup), gate=self.admission_gate(&key)=>gate };
        if !self.startup_allowed(app, identity) {
            return Err(StartupError::Busy);
        }
        Ok(StartupGuard {
            identity: identity.clone(),
            inner: self.inner.clone(),
            _admission: admission,
        })
    }
    fn startup_guard_matches(
        &self,
        app: &crate::state::App,
        identity: &super::adapter::AdapterIdentity,
        guard: &StartupGuard,
    ) -> bool {
        Arc::ptr_eq(&self.inner, &guard.inner)
            && guard.identity == *identity
            && self.startup_allowed(app, identity)
    }
    pub(super) async fn startup_checkpoint_status(
        &self,
        app: &crate::state::App,
        identity: &super::adapter::AdapterIdentity,
        revision: i64,
        guard: &StartupGuard,
    ) -> Result<StartupCheckpoint, StartupError> {
        if !self.startup_guard_matches(app, identity, guard) {
            return Ok(StartupCheckpoint::Fenced);
        }
        let mut tx = app.db.begin().await.map_err(|_| StartupError::Storage)?;
        let result = startup_checkpoint_on(&mut tx, identity, revision).await;
        tx.rollback().await.map_err(|_| StartupError::Storage)?;
        result
    }
    pub(super) fn restore_startup_cursor<'a>(
        &'a self,
        app: &'a Arc<crate::state::App>,
        identity: &'a super::adapter::AdapterIdentity,
        revision: i64,
        guard: &'a StartupGuard,
    ) -> impl Future<Output = Result<StartupRestore, StartupError>> + Send + 'a {
        Box::pin(self.restore_startup_cursor_body(app, identity, revision, guard))
    }
    async fn restore_startup_cursor_body(
        &self,
        app: &Arc<crate::state::App>,
        identity: &super::adapter::AdapterIdentity,
        revision: i64,
        guard: &StartupGuard,
    ) -> Result<StartupRestore, StartupError> {
        use super::{completion, receipt};
        if !self.startup_guard_matches(app, identity, guard) {
            return Ok(StartupRestore::Fenced);
        }
        let expected = {
            self.inner
                .sessions
                .lock()
                .expect("session mutex poisoned")
                .get(&identity.bot_id)
                .cloned()
        };
        if expected
            .as_ref()
            .is_some_and(|cursor| cursor.identity != *identity || cursor.revision != revision)
        {
            return Ok(StartupRestore::Fenced);
        }
        let Some(stored) = receipt::load(&app.db, identity)
            .await
            .map_err(|_| StartupError::Storage)?
        else {
            return Ok(StartupRestore::Unresolved);
        };
        if stored.item.revision != revision {
            return Ok(StartupRestore::Fenced);
        }
        if !matches!(stored.state, receipt::State::Applied) {
            return Ok(StartupRestore::Unresolved);
        }
        let proven = match completion::probe_after_join(app, identity, &stored.item.payload)
            .await
            .map_err(|_| StartupError::Storage)?
        {
            completion::Probe::Applied(commit) => commit,
            completion::Probe::Stale(_) => return Ok(StartupRestore::Fenced),
            _ => return Ok(StartupRestore::Unresolved),
        };
        // Revalidate the read-only proof under a short write-intent lock; do not nest pool transactions.
        let mut tx = crate::db::begin_write(&app.db)
            .await
            .map_err(|_| StartupError::Storage)?;
        let safety = startup_checkpoint_on(&mut tx, identity, revision).await?;
        if !matches!(safety, StartupCheckpoint::Safe) {
            tx.rollback().await.map_err(|_| StartupError::Storage)?;
            return Ok(match safety {
                StartupCheckpoint::Fenced => StartupRestore::Fenced,
                _ => StartupRestore::Unresolved,
            });
        }
        let status = if stored.item.payload.outcome() == receipt::JoinOutcome::Completed {
            "completed"
        } else {
            "failed"
        };
        let valid = receipt::matches_on(&mut tx, &stored.item, receipt::State::Applied)
            .await
            .map_err(|_| StartupError::Storage)?
            && receipt::owner_on(&mut tx, identity, revision, status)
                .await
                .map_err(|_| StartupError::Storage)?
            && startup_message_matches_on(&mut tx, identity, &proven).await?;
        if !valid {
            tx.rollback().await.map_err(|_| StartupError::Storage)?;
            return Ok(StartupRestore::Fenced);
        }
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("api_startup_before_cursor_cas", &identity.bot_id).await;
        if !self.startup_guard_matches(app, identity, guard) {
            tx.rollback().await.map_err(|_| StartupError::Storage)?;
            return Ok(StartupRestore::Fenced);
        }
        let installed = {
            let mut sessions = self.inner.sessions.lock().expect("session mutex poisoned");
            if sessions.get(&identity.bot_id) != expected.as_ref() {
                None
            } else {
                let already = expected.is_some();
                sessions.insert(
                    identity.bot_id.clone(),
                    SessionCursor {
                        identity: identity.clone(),
                        revision,
                    },
                );
                Some(already)
            }
        };
        if tx.rollback().await.is_err() {
            if installed == Some(false) {
                let mut sessions = self.inner.sessions.lock().expect("session mutex poisoned");
                if sessions.get(&identity.bot_id).is_some_and(|cursor| {
                    cursor.identity == *identity && cursor.revision == revision
                }) {
                    sessions.remove(&identity.bot_id);
                }
            }
            return Err(StartupError::Storage);
        }
        Ok(match installed {
            Some(false) => StartupRestore::Restored,
            Some(true) => StartupRestore::AlreadyCurrent,
            None => StartupRestore::Fenced,
        })
    }
    #[cfg(test)]
    pub(super) fn startup_cursor_for_test(
        &self,
        bot: &str,
    ) -> Option<(super::adapter::AdapterIdentity, i64)> {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(bot)
            .map(|c| (c.identity.clone(), c.revision))
    }
    #[cfg(test)]
    pub(super) fn put_startup_cursor_for_test(
        &self,
        identity: super::adapter::AdapterIdentity,
        revision: i64,
    ) {
        self.inner.sessions.lock().unwrap().insert(
            identity.bot_id.clone(),
            SessionCursor { identity, revision },
        );
    }
    #[cfg(test)]
    pub(super) fn startup_set_fake_dependencies_for_test(
        &self,
        tools: Arc<dyn super::runner::Tools>,
    ) {
        self.startup_set_fake_dependencies_at_for_test(tools, "http://127.0.0.1:1/no-call");
    }
    #[cfg(test)]
    pub(super) fn startup_set_fake_dependencies_at_for_test(
        &self,
        tools: Arc<dyn super::runner::Tools>,
        endpoint: &str,
    ) {
        let authority = endpoint.strip_prefix("http://").and_then(|s| s.split(['/', '?', '#']).next());
        let port = authority.and_then(|a| a.strip_prefix("127.0.0.1:").or_else(|| a.strip_prefix("[::1]:")));
        assert!(port.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n != 0)), "fixture endpoint must be literal loopback with a port");
        self.set_test_dependencies(ApiBotDependencies {
            endpoint: endpoint.into(),
            key_source: Arc::new(super::adapter::FixedTestKey("startup-test-only".into())),
            quote: Arc::new(super::quote::FailClosed),
            tools,
            shutdown_join_timeout: None,
        });
    }
    #[cfg(test)]
    pub(super) fn startup_registry_for_test(&self) -> Arc<ApiBotRegistry> {
        self.inner.registry.clone()
    }
    #[cfg(test)]
    pub(super) fn spawn_startup_fixture_owner(
        &self,
        app: Arc<crate::state::App>,
        key: ApiTurnKey,
        identity: super::adapter::AdapterIdentity,
        response: super::protocol::Response,
    ) -> StartupFixtureOwner {
        let (text_tx, text_rx) = oneshot::channel();
        let ticket = self
            .inner
            .registry
            .spawn_tracked(
                RunTurnKey {
                    run_id: key.run_id.clone(),
                    turn_id: key.turn_id.clone(),
                },
                move |_cancel| async move {
                    let _ = text_tx.send(super::completion::FinalText::from_response(&response));
                    Ok(TurnOutcome::Completed)
                },
            )
            .unwrap();
        let cancel = ticket.cancellation_token();
        let (joined_tx, joined_rx) = watch::channel(JoinState::Pending);
        self.inner.controls.lock().unwrap().insert(
            FullKey::from(&key),
            ControlRecord {
                identity: Arc::new(()),
                cancel: cancel.clone(),
                joined: joined_rx,
                finalized: None, owner_exited: false,
            },
        );
        let shutdown = CancellationToken::new();
        let retries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake = Arc::new(tokio::sync::Notify::new());
        let owner = CompletionOwner {
            app,
            key,
            identity,
            final_text: text_rx,
            before_storage: None,
            retry_observer: Some((retries.clone(), wake.clone())),
            store_fault: None,
        };
        let task = tokio::spawn(Box::pin(own_ticket(
            ticket,
            cancel,
            shutdown.clone(),
            joined_tx,
            Duration::from_secs(10),
            Some(owner),
            None,
        )));
        StartupFixtureOwner {
            shutdown,
            retries,
            wake,
            task,
        }
    }
}

pub(super) async fn startup_checkpoint_on(
    conn: &mut sqlx::SqliteConnection,
    identity: &super::adapter::AdapterIdentity,
    revision: i64,
) -> Result<StartupCheckpoint, StartupError> {
    use am_base::db::api_bot::{Checkpoint, MAX_CHECKPOINT_BYTES};
    let in_flight = super::receipt::owner_on(conn, identity, revision, "in_flight")
        .await
        .map_err(|_| StartupError::Storage)?;
    let completed = super::receipt::owner_on(conn, identity, revision, "completed")
        .await
        .map_err(|_| StartupError::Storage)?;
    let owner_failed = super::receipt::owner_on(conn, identity, revision, "failed")
        .await
        .map_err(|_| StartupError::Storage)?;
    if !(in_flight || completed || owner_failed) {
        return Ok(StartupCheckpoint::Fenced);
    }
    let row:Option<(i64,Option<String>)>=sqlx::query_as("SELECT format_version,CASE WHEN length(CAST(checkpoint_json AS BLOB))<=? THEN checkpoint_json ELSE NULL END FROM api_bot_sessions WHERE id=? AND bot_id=? AND owner_run_id=? AND owner_turn_id=? AND generation=? AND revision=?")
        .bind(MAX_CHECKPOINT_BYTES as i64).bind(&identity.session_id).bind(&identity.bot_id).bind(&identity.run_id).bind(&identity.turn_id).bind(identity.generation).bind(revision)
        .fetch_optional(conn).await.map_err(|_|StartupError::Storage)?;
    let Some((1, Some(raw))) = row else {
        return Ok(StartupCheckpoint::Unresolved);
    };
    let safe = (|| {
        let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let checkpoint = Checkpoint::new(&value).ok()?;
        let state: super::runner::State = serde_json::from_value(checkpoint.value()).ok()?;
        Some(startup_context_settled(&state, owner_failed))
    })()
    .unwrap_or(false);
    Ok(if safe {
        StartupCheckpoint::Safe
    } else {
        StartupCheckpoint::Unresolved
    })
}
/// `owner_failed`：擁有這份 checkpoint 的 turn 已是終態 failed（且 receipt applied）。規則見
/// `am_base::db::api_bot::pending_settled_after_failed_owner`：只有它能放寬 `api_pending`，`tool_pending` 永遠不放寬。
pub(super) fn startup_context_settled(state: &super::runner::State, owner_failed: bool) -> bool {
    if !am_base::db::api_bot::pending_settled_after_failed_owner(state.api_pending, state.tool_pending.is_some(), owner_failed) {
        return false;
    }
    let mut declared = std::collections::HashSet::new();
    let mut returned = std::collections::HashSet::new();
    for message in &state.messages {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for block in blocks {
            match (
                message.role.as_str(),
                block.get("type").and_then(|v| v.as_str()),
            ) {
                ("assistant", Some("tool_use")) => {
                    let Some(id) = block.get("id").and_then(|v| v.as_str()) else {
                        return false;
                    };
                    declared.insert(id.to_owned());
                }
                ("user", Some("tool_result")) => {
                    let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) else {
                        return false;
                    };
                    if block.get("content").is_none() {
                        return false;
                    }
                    returned.insert(id.to_owned());
                }
                _ => {}
            }
        }
    }
    for response in &state.responses {
        let Ok(calls) = response.tools() else {
            return false;
        };
        for call in calls {
            declared.insert(call.id);
        }
    }
    declared
        .iter()
        .all(|id| state.completed_tool_ids.contains(id) && returned.contains(id))
        && returned.iter().all(|id| declared.contains(id))
}

async fn startup_message_matches_on(
    conn: &mut sqlx::SqliteConnection,
    identity: &super::adapter::AdapterIdentity,
    proven: &super::completion::Committed,
) -> Result<bool, StartupError> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&identity.turn_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|_| StartupError::Storage)?;
    let Some(message) = &proven.message else {
        return Ok(count == 0);
    };
    if count != 1 {
        return Ok(false);
    }
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE id=? AND conversation_id=? AND turn_id=? AND role='assistant' AND content=? AND source='transcript' AND incomplete=0 AND rewound_at IS NULL)")
        .bind(&message.id).bind(&message.conversation_id).bind(&identity.turn_id).bind(&message.content).fetch_one(conn).await.map_err(|_|StartupError::Storage)
}

async fn cleanup_terminal_controls(pool: &sqlx::SqlitePool, inner: &RuntimeInner) {
    let candidates: Vec<(FullKey, Arc<()>)> = {
        let controls = inner.controls.lock().expect("control mutex poisoned");
        controls
            .iter()
            .filter_map(|(key, record)| {
                (record.owner_exited && matches!(*record.joined.borrow(), JoinState::Joined(Ok(_))))
                    .then(|| (key.clone(), record.identity.clone()))
            })
            .collect()
    };

    let mut removable = Vec::new();
    for (key, identity) in candidates {
        let terminal: Option<(String, String)> = sqlx::query_as(
            "SELECT r.state,t.status FROM runs r JOIN turns t ON t.run_id=r.id WHERE r.id=? AND r.bot_id=? AND t.id=?",
        )
        .bind(&key.run_id)
        .bind(&key.bot_id)
        .bind(&key.turn_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
        if matches!(terminal, Some((ref run, ref turn)) if matches!(run.as_str(), "stopped" | "exited") && matches!(turn.as_str(), "completed" | "completed_fallback" | "failed"))
        {
            removable.push((key, identity));
        }
    }

    let mut controls = inner.controls.lock().expect("control mutex poisoned");
    for (key, identity) in removable {
        let same_record = controls.get(&key).is_some_and(|record| {
            Arc::ptr_eq(&record.identity, &identity)
                && record.owner_exited
                && matches!(*record.joined.borrow(), JoinState::Joined(Ok(_)))
        });
        if same_record {
            controls.remove(&key);
        }
    }
}

fn already_updated_with(messages: &[super::protocol::Message], fresh: &str) -> bool {
    let expected_prefix = format!("[Instructions updated by AG Man]\n\n{fresh}");
    for m in messages.iter().rev() {
        if m.role == "user" {
            if let Some(blocks) = m.content.as_array() {
                for b in blocks {
                    if b.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                        if let Some(text) = b.get("text").and_then(serde_json::Value::as_str) {
                            if text == expected_prefix {
                                return true;
                            }
                            if text.starts_with("[Instructions updated by AG Man]\n\n") {
                                return false;
                            }
                        }
                    }
                }
            } else if let Some(text) = m.content.as_str() {
                if text == expected_prefix {
                    return true;
                }
                if text.starts_with("[Instructions updated by AG Man]\n\n") {
                    return false;
                }
            }
        }
    }
    false
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
