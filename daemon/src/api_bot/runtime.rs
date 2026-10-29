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

    pub(super) async fn native_start_dependencies_ready(&self, app: &crate::state::App, bot_id: &str) -> Result<(), ApiBotError> {
        let _gate = tokio::select! {
            biased;
            _ = app.shutdown.cancelled() => return Err(ApiBotError::Unconfirmed),
            result = tokio::time::timeout(Duration::from_secs(3), self.bot_gate(bot_id)) => result.map_err(|_| ApiBotError::Busy)?,
        };
        if !app.api_startup_gate.ready() || self.explicit_retire_quarantined(bot_id) { return Err(ApiBotError::Unconfirmed); }
        let bot = crate::db::bot(&app.db, bot_id).await.map_err(|_| ApiBotError::Upstream)?.filter(|b| b.kind == "claude-api" && b.deleted_at.is_none()).ok_or(ApiBotError::Unconfirmed)?;
        let project = crate::db::project(&app.db, &bot.project_id).await.map_err(|_| ApiBotError::Upstream)?.filter(|p| p.deleted_at.is_none() && p.host == crate::config::LOCAL_HOST && p.handed_off_to.is_none()).ok_or(ApiBotError::Unconfirmed)?;
        let cfg = app.cfg.get().await;
        let limits = cfg.claude_api.api_bot.validated(cfg.claude_api.enabled).map_err(|_| ApiBotError::Upstream)?.ok_or(ApiBotError::Upstream)?;
        if self.dependencies_snapshot().is_none() {
            let dependencies = production_dependencies(app, &bot, &project, &limits)?;
            let key = dependencies.key_source.snapshot().await.map_err(|_| ApiBotError::Upstream)?;
            if key.is_empty() { return Err(ApiBotError::Upstream); }
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
        let wait = async {
            loop {
                let waiting = {
                    let controls = self.inner.controls.lock().expect("control mutex poisoned");
                    controls.get(&key).is_some_and(|record| !record.owner_exited)
                };
                if !waiting { break; }
                tokio::select! {
                    _ = app.shutdown.cancelled() => return Err(E::Busy),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
            Ok(())
        };
        tokio::time::timeout(API_BOT_SHUTDOWN_JOIN_TIMEOUT, wait).await.map_err(|_| E::Busy)??;
        let bot = self.bot_gate(&c.0).await;
        let admission = self.admission_gate(&ApiRunKey { bot_id: c.0.clone(), run_id: run.clone() }).await;
        if app.shutdown.is_cancelled() || self.inner.registry.is_active(run) { return Err(E::Busy); }
        let control = {
            let controls = self.inner.controls.lock().expect("control mutex poisoned");
            if controls.keys().any(|other| other.bot_id == c.0 && other != &key) { return Err(E::Busy); }
            match controls.get(&key) {
                Some(record) if record.owner_exited && matches!(*record.joined.borrow(), JoinState::Joined(Ok(_))) => Some(record.identity.clone()),
                Some(_) => return Err(E::Busy),
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
                    if !matches!(*record.joined.borrow(), JoinState::Joined(Ok(Completion::Completed))) {
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
            None => production_dependencies(app, &bot, &project, &limits)?,
        };
        let effort = bot.effort.clone().ok_or(ApiBotError::Upstream)?;
        let options = super::protocol::Options {
            effort,
            fast: bot.fast != 0,
            max_tokens: limits.max_output_tokens,
        };
        options.validate().map_err(|_| ApiBotError::Upstream)?;

        let agent_name = run.agent_name.as_deref().unwrap_or(&bot.name);
        let agent_md = crate::lifecycle::agent_md::load(app, &project).await;
        let system = serde_json::Value::String(crate::lifecycle::setup::persona_text(
            &bot,
            agent_name,
            Some(&agent_md.text),
        ));
        let adapter_limits = super::adapter::AdapterLimits {
            max_checkpoint_bytes: limits.max_checkpoint_bytes,
            max_turn_microusd: limits.max_turn_cost_microusd,
            max_requests_per_hour: i64::from(limits.max_requests_per_hour),
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
            (Arc::new(adapter), state)
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
            (Arc::new(adapter), super::runner::State::default())
        };

        drop(resume_old_admission);
        if state.api_pending || state.tool_pending.is_some() {
            return Err(if prior.is_some() { ApiBotError::Busy } else { ApiBotError::Unconfirmed });
        }
        state.messages.push(super::protocol::Message {
            role: "user".into(),
            content: serde_json::Value::String(prompt),
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
                let outcome = if deadline_elapsed {
                    TurnOutcome::Failed("turn_timeout".into())
                } else {
                    map_runner_result(result, &cancel)
                };
                if matches!(&outcome, TurnOutcome::Completed) {
                    if let Some(response) = state.responses.last() {
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
                        if !supervisor_shutdown.is_cancelled() && matches!(*record.joined.borrow(), JoinState::Joined(Ok(Completion::Completed))) {
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
                let changed = sqlx::query("UPDATE turns SET delivery='ok',delivery_verified=1 WHERE id=? AND run_id=? AND status='in_flight' AND delivery='pending' AND EXISTS(SELECT 1 FROM runs r JOIN bots b ON b.id=r.bot_id JOIN projects p ON p.id=b.project_id JOIN conversations c ON c.id=turns.conversation_id AND c.bot_id=b.id WHERE r.id=turns.run_id AND r.id=? AND r.bot_id=? AND r.state='running' AND b.kind='claude-api' AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.host='local' AND p.handed_off_to IS NULL AND b.cwd IS ? AND p.path=?)")
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
        if !matches!(crate::db::project(&self.db, &bot.project_id).await, Ok(Some(p)) if p.deleted_at.is_none() && p.host == crate::config::LOCAL_HOST && p.handed_off_to.is_none()) { return A::Unavailable; }
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
            if let Some((committed, payload, published)) = settle_joined_completion(
                &owner, joined, final_text, &shutdown, session_commit.as_ref(),
            ).await {
                let revision = committed.session_revision;
                super::completion::emit_after_commit(&owner.app, &owner.key, committed).await;
                if joined == Completion::Completed && payload.outcome() == super::receipt::JoinOutcome::Completed && published {
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

fn production_dependencies(app: &crate::state::App, bot: &crate::db::Bot, project: &crate::db::Project, limits: &am_config::api_bot::ValidatedLimits) -> Result<ApiBotDependencies, ApiBotError> {
    if !native_bwrap_available() { return Err(ApiBotError::Upstream); }
    let root = crate::lifecycle::start::bot_cwd(bot, project);
    let tools = super::tools::LocalTools::new(std::path::Path::new(root), super::tools::Limits { timeout: Duration::from_secs(limits.command_timeout_secs), output_bytes: limits.max_tool_output_bytes }).map_err(|_| ApiBotError::Upstream)?;
    let key_source = Arc::new(ConfigKeySource(Arc::downgrade(&app.shared())));
    Ok(ApiBotDependencies { endpoint: "https://api.anthropic.com/v1/messages".into(), key_source, quote: Arc::new(super::pricing::Opus55Global), tools: Arc::new(tools), shutdown_join_timeout: None })
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
        || project.host != crate::config::LOCAL_HOST
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
) -> TurnOutcome {
    match result {
        Ok(super::runner::Halt::EndTurn | super::runner::Halt::Refusal) => TurnOutcome::Completed,
        Ok(super::runner::Halt::MaxTokens) => TurnOutcome::Failed("max_tokens".into()),
        Ok(super::runner::Halt::Other(_)) => TurnOutcome::Failed("unknown_stop".into()),
        Err(super::runner::Error::Cancelled) => TurnOutcome::Cancelled,
        Err(super::runner::Error::Model(super::http::Error::Cancelled))
            if cancel.is_cancelled() =>
        {
            TurnOutcome::Cancelled
        }
        Err(super::runner::Error::Budget) => TurnOutcome::Failed("budget".into()),
        Err(super::runner::Error::Journal) => TurnOutcome::Failed("journal".into()),
        Err(super::runner::Error::PendingUnknown) => TurnOutcome::Failed("pending_unknown".into()),
        Err(super::runner::Error::InvalidResponse) => {
            TurnOutcome::Failed("invalid_response".into())
        }
        Err(super::runner::Error::Model(_)) => TurnOutcome::Failed("model".into()),
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

async fn startup_checkpoint_on(
    conn: &mut sqlx::SqliteConnection,
    identity: &super::adapter::AdapterIdentity,
    revision: i64,
) -> Result<StartupCheckpoint, StartupError> {
    use am_base::db::api_bot::{Checkpoint, MAX_CHECKPOINT_BYTES};
    let live = super::receipt::owner_on(conn, identity, revision, "in_flight")
        .await
        .map_err(|_| StartupError::Storage)?
        || super::receipt::owner_on(conn, identity, revision, "completed")
            .await
            .map_err(|_| StartupError::Storage)?
        || super::receipt::owner_on(conn, identity, revision, "failed")
            .await
            .map_err(|_| StartupError::Storage)?;
    if !live {
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
        Some(startup_context_settled(&state))
    })()
    .unwrap_or(false);
    Ok(if safe {
        StartupCheckpoint::Safe
    } else {
        StartupCheckpoint::Unresolved
    })
}
pub(super) fn startup_context_settled(state: &super::runner::State) -> bool {
    if state.api_pending || state.tool_pending.is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_bot::protocol::Response;
    use crate::api_bot::quote::{QuoteError, RequestEnvelope, ReservationQuote, UsageQuote};
    use crate::lifecycle::api_bot_port::{ApiBotPort, ApiRunKey, ApiRunStopEvidence};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    const DEADLOCK_TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn claude_api_kind_stays_out_of_the_bot_creation_allowlist() {
        // create_bot rejects any kind not accepted by this guard before it reaches storage.
        assert!(!crate::config::valid_kind("claude-api"));
    }

    #[test]
    fn native_start_prompt_dispatch_fits_one_mib_stack() {
        std::thread::Builder::new().stack_size(1024 * 1024).spawn(native_start_and_prompt_dispatch_to_fake_provider).unwrap().join().unwrap();
    }

    #[tokio::test]
    async fn native_start_and_prompt_dispatch_to_fake_provider() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, shutdown, server) = fake_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid();
        let crid = crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,fast,hook_token,created_at) VALUES(?,?,'native-api','claude-api','high',0,'fixture',?)")
            .bind(&bot).bind(&env.project_id).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let result = tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            let run = crate::lifecycle::start_bot(&env.app, &bot).await.map_err(|_| "native start failed")?;
            let pane: Option<String> = sqlx::query_scalar("SELECT pane_id FROM runs WHERE id=?")
                .bind(&run).fetch_one(&env.app.db).await.map_err(|_| "run missing")?;
            if pane.is_some() { return Err("API run has a pane"); }
            crate::lifecycle::prompt(&env.app, &bot, "fixture prompt", &crid)
                .await.map_err(|_| "native prompt failed")?;
            tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.map_err(|_| "fake provider not called")?;
            Ok::<(), &str>(())
        }).await;
        shutdown.cancel();
        server.await.unwrap();
        assert!(matches!(result, Ok(Ok(()))), "native fake dispatch: {result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_start_guard_preserves_unconfirmed_and_unexited_owners() {
        let env = crate::testing::env().await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        sqlx::query("UPDATE turns SET status='completed' WHERE id=?").bind(&turn).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        let old = FullKey { bot_id: bot.clone(), run_id: run, turn_id: turn };
        let key = ApiRunKey { bot_id: bot, run_id: crate::db::ulid() };
        let (sender, joined) = watch::channel(JoinState::Joined(Err(JoinError::Unconfirmed)));
        env.app.api_bot_runtime.inner.controls.lock().unwrap().insert(old.clone(), ControlRecord { identity: Arc::new(()), cancel: CancellationToken::new(), joined, finalized: None, owner_exited: true });
        cleanup_terminal_controls(&env.app.db, &env.app.api_bot_runtime.inner).await;
        assert!(env.app.api_bot_runtime.inner.controls.lock().unwrap().contains_key(&old));
        assert!(matches!(env.app.api_bot_runtime.native_start_guard(&key).await, Err(ApiBotError::Busy)));
        sender.send_replace(JoinState::Joined(Ok(Completion::Completed)));
        env.app.api_bot_runtime.inner.controls.lock().unwrap().get_mut(&old).unwrap().owner_exited = false;
        cleanup_terminal_controls(&env.app.db, &env.app.api_bot_runtime.inner).await;
        assert!(env.app.api_bot_runtime.inner.controls.lock().unwrap().contains_key(&old));
        assert!(matches!(env.app.api_bot_runtime.native_start_guard(&key).await, Err(ApiBotError::Busy)));
        env.app.api_bot_runtime.inner.controls.lock().unwrap().get_mut(&old).unwrap().owner_exited = true;
        assert!(env.app.api_bot_runtime.native_start_guard(&key).await.is_ok());
        cleanup_terminal_controls(&env.app.db, &env.app.api_bot_runtime.inner).await;
        assert!(!env.app.api_bot_runtime.inner.controls.lock().unwrap().contains_key(&old));
    }

    #[tokio::test]
    async fn native_pending_real_commit_synthetic_lost_ack_does_not_dispatch_on_retry() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let crid = crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)")
            .bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        crate::lifecycle::start_bot(&env.app, &bot).await.unwrap();
        super::super::native_prompt::arm_pending_lost_ack_for_test(&crid);
        let first = crate::lifecycle::prompt(&env.app, &bot, "fixture", &crid).await;
        let retry = crate::lifecycle::prompt(&env.app, &bot, "fixture", &crid).await;
        let rows: Vec<(String,String)> = sqlx::query_as("SELECT id,delivery FROM turns WHERE client_request_id=?").bind(&crid).fetch_all(&env.app.db).await.unwrap();
        let n = calls.load(Ordering::SeqCst);
        shutdown.cancel(); server.await.unwrap();
        assert!(first.is_err() && retry.is_err());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "pending");
        assert_eq!(n, 0);
        assert_eq!(env.app.api_bot_runtime.control_count_for_test(), 0);
    }

    #[tokio::test]
    async fn native_runner_waits_for_durable_delivery_acceptance() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0)); let provider_hit = Arc::new(Notify::new());
        let (endpoint, shutdown, server) = fake_server(calls.clone(), provider_hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let crid = crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)")
            .bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        crate::lifecycle::start_bot(&env.app, &bot).await.unwrap();
        let pending_reached = Arc::new(Notify::new()); let pending_release = Arc::new(Notify::new());
        let a = pending_reached.clone(); let b = pending_release.clone();
        crate::lifecycle::race_point::arm("api_native_prompt_after_pending_commit", &crid, move || async move { a.notify_one(); b.notified().await; });
        let app = env.app.clone(); let task_bot = bot.clone(); let task_crid = crid.clone();
        let task = tokio::spawn(async move { crate::lifecycle::prompt(&app, &task_bot, "fixture", &task_crid).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, pending_reached.notified()).await.unwrap();
        let turn: String = sqlx::query_scalar("SELECT id FROM turns WHERE client_request_id=?").bind(&crid).fetch_one(&env.app.db).await.unwrap();
        let before_reached = Arc::new(Notify::new()); let before_release = Arc::new(Notify::new());
        let a = before_reached.clone(); let b = before_release.clone();
        crate::lifecycle::race_point::arm("api_native_before_delivery_commit", &turn, move || async move { a.notify_one(); b.notified().await; });
        pending_release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, before_reached.notified()).await.unwrap();
        let before: String = sqlx::query_scalar("SELECT delivery FROM turns WHERE id=?").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        let before_calls = calls.load(Ordering::SeqCst);
        let controls = env.app.api_bot_runtime.control_count_for_test();
        let after_reached = Arc::new(Notify::new()); let after_release = Arc::new(Notify::new());
        let a = after_reached.clone(); let b = after_release.clone();
        crate::lifecycle::race_point::arm("api_native_after_delivery_commit", &turn, move || async move { a.notify_one(); b.notified().await; });
        before_release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, after_reached.notified()).await.unwrap();
        let committed: String = sqlx::query_scalar("SELECT delivery FROM turns WHERE id=?").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        let committed_calls = calls.load(Ordering::SeqCst);
        after_release.notify_one();
        let result = tokio::time::timeout(DEADLOCK_TIMEOUT, task).await.unwrap().unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, provider_hit.notified()).await.unwrap();
        let after: String = sqlx::query_scalar("SELECT delivery FROM turns WHERE id=?").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        env.app.background_tasks.close();
        tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(before, "pending"); assert_eq!(before_calls, 0); assert_eq!(controls, 1);
        assert_eq!(committed, "ok"); assert_eq!(committed_calls, 0);
        assert_eq!(after, "ok"); assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_fresh_start_holds_lifecycle_lock_until_new_run_is_created() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiNativeStartMode};
        async fn completed(app: &crate::state::App, key: &ApiTurnKey, stage: &str) {
            let result = tokio::time::timeout(DEADLOCK_TIMEOUT, async {
                loop {
                    let status: Option<String> = sqlx::query_scalar("SELECT status FROM turns WHERE id=? AND run_id=?")
                        .bind(&key.turn_id).bind(&key.run_id).fetch_optional(&app.db).await.unwrap();
                    if status.as_deref() == Some("completed") { break; }
                    tokio::task::yield_now().await;
                }
            }).await;
            assert!(result.is_ok(), "{stage} completion timed out");
        }
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0)); let hit = Arc::new(Notify::new());
        let (endpoint, shutdown, server) = fake_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)")
            .bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let run = tokio::time::timeout(DEADLOCK_TIMEOUT, crate::lifecycle::start_bot(&env.app, &bot)).await.expect("first start timed out").unwrap();
        let first = tokio::time::timeout(DEADLOCK_TIMEOUT, crate::lifecycle::prompt(&env.app, &bot, "first", &crate::db::ulid())).await.expect("first prompt timed out").unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.expect("provider response barrier timed out");
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run, turn_id: first.turn_id };
        completed(&env.app, &key, "first turn").await;
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { loop { if env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.owner_exited) { break; } tokio::task::yield_now().await; } }).await.expect("first owner exit timed out");
        let (session, generation): (String,i64) = sqlx::query_as("SELECT id,generation FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        let target = ApiFreshTarget { session_id: session.clone(), generation };
        let committed = Arc::new(Notify::new()); let finish_first = Arc::new(Notify::new());
        let a = committed.clone(); let b = finish_first.clone();
        crate::lifecycle::race_point::arm("api_fresh_after_commit", &session, move || async move { a.notify_one(); b.notified().await; });
        let app = env.app.clone(); let id = bot.clone(); let first_target = ApiFreshTarget { session_id:session.clone(), generation };
        let first_start = tokio::spawn(async move { app.api_native_start(&id, ApiNativeStartMode::Fresh(first_target)).await });
        let reached = tokio::time::timeout(DEADLOCK_TIMEOUT, committed.notified()).await;
        if reached.is_err() { finish_first.notify_one(); panic!("fresh postcommit barrier timed out"); }
        let queued = super::super::native_start::observe_next_lock_pending(&bot);
        let app = env.app.clone(); let id = bot.clone();
        let second_start = tokio::spawn(async move { app.api_native_start(&id, ApiNativeStartMode::Fresh(target)).await });
        let observed = tokio::time::timeout(DEADLOCK_TIMEOUT, queued).await;
        finish_first.notify_one();
        assert!(matches!(observed, Ok(Ok(()))), "tracked owner did not poll the bot lock pending");
        let next = match tokio::time::timeout(DEADLOCK_TIMEOUT, first_start).await.expect("first fresh owner timed out").expect("first fresh task failed") { Ok(value) => value, Err(_) => panic!("fresh start did not complete") };
        assert!(tokio::time::timeout(DEADLOCK_TIMEOUT, second_start).await.expect("second fresh owner timed out").expect("second fresh task failed").is_err());
        let second = tokio::time::timeout(DEADLOCK_TIMEOUT, crate::lifecycle::prompt(&env.app, &bot, "successor", &crate::db::ulid())).await.expect("successor prompt timed out").unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.expect("provider response barrier timed out");
        let next_key = ApiTurnKey { bot_id: bot.clone(), run_id: next.run_id, turn_id: second.turn_id };
        completed(&env.app, &next_key, "successor turn").await;
        let successor: String = sqlx::query_scalar("SELECT id FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        assert_ne!(successor, session);
        let still: String = sqlx::query_scalar("SELECT id FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(still, successor); assert_eq!(calls.load(Ordering::SeqCst), 2);
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.expect("background task drain timed out");
        shutdown.cancel(); tokio::time::timeout(DEADLOCK_TIMEOUT, server).await.expect("fake server shutdown timed out").unwrap();
    }

    #[tokio::test]
    async fn native_user_router_fresh_target_start_and_prompt_dispatch() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn request(addr: std::net::SocketAddr, method: &str, path: &str, token: &str, body: &str) -> (String, Value) {
            tokio::time::timeout(DEADLOCK_TIMEOUT, async {
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                let request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(request.as_bytes()).await.unwrap();
                let mut response = String::new(); stream.read_to_string(&mut response).await.unwrap();
                let (headers, body) = response.split_once("\r\n\r\n").expect("HTTP response framing");
                (headers.lines().next().unwrap().to_owned(), serde_json::from_str(body).expect("JSON response"))
            }).await.expect("router HTTP request timed out")
        }
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0)); let hit = Arc::new(Notify::new());
        let (endpoint, provider_stop, provider) = fake_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)")
            .bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); let addr = listener.local_addr().unwrap();
        let router_stop = CancellationToken::new(); let stop = router_stop.clone();
        let router = crate::api::router(env.app.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(async move { stop.cancelled().await }).await.unwrap() });
        let token = &env.app.ui_token;
        let (status, started) = request(addr, "POST", &format!("/api/bots/{bot}/start"), token, "").await;
        assert!(status.contains(" 200 "), "start status: {status}");
        let run = started["run_id"].as_str().unwrap().to_owned();
        let crid = crate::db::ulid();
        let (status, first) = request(addr, "POST", &format!("/api/bots/{bot}/prompt"), token, &json!({"text":"first","client_request_id":crid}).to_string()).await;
        assert!(status.contains(" 200 "), "first prompt status: {status}");
        let turn = first["turn_id"].as_str().unwrap().to_owned();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.expect("first provider request timed out");
        let key = ApiTurnKey { bot_id:bot.clone(), run_id:run, turn_id:turn };
        wait_until_completed(&env.app, &key).await;
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { loop { if env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.owner_exited) { break; } tokio::task::yield_now().await; } }).await.expect("first owner exit timed out");
        let (status, descriptor) = request(addr, "GET", &format!("/api/bots/{bot}/api_fresh_target"), token, "").await;
        assert!(status.contains(" 200 ")); assert_eq!(descriptor["availability"], "available");
        let session = descriptor["session_id"].as_str().unwrap().to_owned(); let generation = descriptor["generation"].as_i64().unwrap();
        let messages_before: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE turn_id=?").bind(&key.turn_id).fetch_one(&env.app.db).await.unwrap();
        let ledger_before: (String, String, i64) = sqlx::query_as("SELECT id,state,reserved_microusd FROM api_bot_attempts WHERE turn_id=?").bind(&key.turn_id).fetch_one(&env.app.db).await.unwrap();
        let path = format!("/api/bots/{bot}/start?resume=fresh&api_expected_session_id={session}&api_expected_generation={generation}");
        let (status, fresh) = request(addr, "POST", &path, token, "").await;
        assert!(status.contains(" 200 "), "fresh start status: {status}");
        let next_run = fresh["run_id"].as_str().unwrap().to_owned(); assert_ne!(next_run, key.run_id);
        let target_exists: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap();
        let messages_after: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE turn_id=?").bind(&key.turn_id).fetch_one(&env.app.db).await.unwrap();
        let ledger_after: (String, String, i64) = sqlx::query_as("SELECT id,state,reserved_microusd FROM api_bot_attempts WHERE id=?").bind(&ledger_before.0).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(target_exists, 0); assert_eq!(messages_before, 2); assert_eq!(messages_after, messages_before); assert_eq!(ledger_after, ledger_before);
        let (status, second) = request(addr, "POST", &format!("/api/bots/{bot}/prompt"), token, &json!({"text":"second","client_request_id":crate::db::ulid()}).to_string()).await;
        assert!(status.contains(" 200 "), "second prompt status: {status}");
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.expect("second provider request timed out");
        let next = ApiTurnKey { bot_id:bot.clone(), run_id:next_run, turn_id:second["turn_id"].as_str().unwrap().to_owned() };
        wait_until_completed(&env.app, &next).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.expect("task drain timed out");
        router_stop.cancel(); provider_stop.cancel();
        tokio::time::timeout(DEADLOCK_TIMEOUT, server).await.expect("router shutdown timed out").unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, provider).await.expect("provider shutdown timed out").unwrap();
    }

    #[tokio::test]
    async fn native_shutdown_after_delivery_commit_never_releases_runner() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let mut deps = env.app.api_bot_runtime.test_dependencies().unwrap();
        deps.tools = Arc::new(CountingTools(tool_calls.clone()));
        env.app.api_bot_runtime.set_test_dependencies(deps);
        let bot = crate::db::ulid(); let crid = crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)")
            .bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        crate::lifecycle::start_bot(&env.app, &bot).await.unwrap();
        let pending = Arc::new(Notify::new()); let pending_release = Arc::new(Notify::new());
        let a = pending.clone(); let b = pending_release.clone();
        crate::lifecycle::race_point::arm("api_native_prompt_after_pending_commit", &crid, move || async move { a.notify_one(); b.notified().await; });
        let app = env.app.clone(); let id = bot.clone(); let request = crid.clone();
        let task = tokio::spawn(async move { crate::lifecycle::prompt(&app, &id, "fixture", &request).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, pending.notified()).await.unwrap();
        let turn: String = sqlx::query_scalar("SELECT id FROM turns WHERE client_request_id=?").bind(&crid).fetch_one(&env.app.db).await.unwrap();
        let reached = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let a = reached.clone(); let b = release.clone();
        crate::lifecycle::race_point::arm("api_native_after_delivery_commit", &turn, move || async move { a.notify_one(); b.notified().await; });
        pending_release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, reached.notified()).await.unwrap();
        env.app.shutdown.cancel(); release.notify_one();
        assert!(tokio::time::timeout(DEADLOCK_TIMEOUT, task).await.unwrap().unwrap().is_err());
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0); assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        assert_ne!(status, "completed");
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn native_delivery_real_commit_synthetic_lost_ack_never_releases_runner() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let crid = crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)")
            .bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let run = crate::lifecycle::start_bot(&env.app, &bot).await.unwrap();
        let reached = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let a = reached.clone(); let b = release.clone();
        crate::lifecycle::race_point::arm("api_native_prompt_after_pending_commit", &crid, move || async move { a.notify_one(); b.notified().await; });
        let app = env.app.clone(); let task_bot = bot.clone(); let task_crid = crid.clone();
        let task = tokio::spawn(async move { crate::lifecycle::prompt(&app, &task_bot, "fixture", &task_crid).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, reached.notified()).await.unwrap();
        let turn: String = sqlx::query_scalar("SELECT id FROM turns WHERE client_request_id=?").bind(&crid).fetch_one(&env.app.db).await.unwrap();
        ApiBotRuntime::arm_delivery_lost_ack_for_test(&turn);
        release.notify_one();
        let first = tokio::time::timeout(DEADLOCK_TIMEOUT, task).await.unwrap().unwrap();
        let retry = crate::lifecycle::prompt(&env.app, &bot, "fixture", &crid).await;
        let delivery: String = sqlx::query_scalar("SELECT delivery FROM turns WHERE id=?").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        env.app.background_tasks.close();
        tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        let registry_empty = !env.app.api_bot_runtime.inner.registry.is_active(&run);
        let key_for_fault = turn.clone();
        let owner_exited = env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey { bot_id: bot, run_id: run, turn_id: turn }).is_some_and(|r| r.owner_exited);
        shutdown.cancel(); server.await.unwrap();
        assert!(first.is_err() && retry.is_err());
        assert_eq!(delivery, "ok", "the real CAS committed before its synthetic lost acknowledgement");
        assert!(!delivery_lost_acks().lock().unwrap().contains(&key_for_fault));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(registry_empty && owner_exited, "the unreleased runner and owner actually exited");
    }

    #[tokio::test]
    async fn native_completed_crid_readback_after_receipt_removal_does_not_dispatch() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0)); let hit = Arc::new(Notify::new());
        let (endpoint, shutdown, server) = fake_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let crid = crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let run = crate::lifecycle::start_bot(&env.app, &bot).await.unwrap();
        let first = crate::lifecycle::prompt(&env.app, &bot, "fixture", &crid).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.unwrap();
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run, turn_id: first.turn_id.clone() };
        wait_until_completed(&env.app, &key).await;
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            loop {
                let exited = env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.owner_exited);
                if exited { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE turn_id=?").bind(&first.turn_id).execute(&env.app.db).await.unwrap();
        let retry = crate::lifecycle::prompt(&env.app, &bot, "fixture", &crid).await.unwrap();
        let mismatch = crate::lifecycle::prompt(&env.app, &bot, "different", &crid).await;
        let duplicate = crate::db::ulid();
        let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?").bind(&first.turn_id).fetch_one(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'user','fixture','web',?)").bind(&duplicate).bind(&conv).bind(&first.turn_id).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let duplicate_result = crate::lifecycle::prompt(&env.app, &bot, "fixture", &crid).await;
        sqlx::query("DELETE FROM messages WHERE id IN (?,?)").bind(&duplicate).bind(&first.message_id).execute(&env.app.db).await.unwrap();
        let missing_result = crate::lifecycle::prompt(&env.app, &bot, "fixture", &crid).await;
        let failed_turn = crate::db::ulid(); let failed_crid = crate::db::ulid(); let failed_message = crate::db::ulid();
        let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?").bind(&first.turn_id).fetch_one(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,client_request_id,prompt_text,created_at) VALUES(?,?,?,'web','failed','ok',?,'fixture',?)").bind(&failed_turn).bind(&conv).bind(&key.run_id).bind(&failed_crid).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'user','fixture','web',?)").bind(&failed_message).bind(&conv).bind(&failed_turn).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let failed = crate::lifecycle::prompt(&env.app, &bot, "fixture", &failed_crid).await;
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
        assert_eq!(retry.turn_id, first.turn_id); assert_eq!(retry.message_id, first.message_id);
        assert!(mismatch.is_err() && duplicate_result.is_err() && missing_result.is_err() && failed.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_maintenance_and_delivery_acceptance_are_serialized() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0)); let hit = Arc::new(Notify::new());
        let (endpoint, shutdown, server) = fake_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let first_crid = crate::db::ulid(); let crid = crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let run = crate::lifecycle::start_bot(&env.app, &bot).await.unwrap();
        let until = crate::db::iso_in(300);
        let lease = crate::supervisor::store::acquire_lease(&env.app.db, "restart", "fixture", None, None, &until, true, None, &json!({})).await.unwrap().unwrap();
        let blocked = crate::lifecycle::prompt(&env.app, &bot, "fixture", &first_crid).await;
        assert!(crate::supervisor::store::release_lease(&env.app.db, "restart", "fixture", lease.fence).await.unwrap());
        let reached = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let a = reached.clone(); let b = release.clone();
        crate::lifecycle::race_point::arm("api_native_prompt_after_pending_commit", &crid, move || async move { a.notify_one(); b.notified().await; });
        let app = env.app.clone(); let task_bot = bot.clone(); let task_crid = crid.clone();
        let task = tokio::spawn(async move { crate::lifecycle::prompt(&app, &task_bot, "fixture", &task_crid).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, reached.notified()).await.unwrap();
        let turn: String = sqlx::query_scalar("SELECT id FROM turns WHERE client_request_id=?").bind(&crid).fetch_one(&env.app.db).await.unwrap();
        let after_reached = Arc::new(Notify::new()); let after_release = Arc::new(Notify::new());
        let a = after_reached.clone(); let b = after_release.clone();
        crate::lifecycle::race_point::arm("api_native_after_delivery_commit", &turn, move || async move { a.notify_one(); b.notified().await; });
        let pending_lease = crate::supervisor::store::acquire_lease(&env.app.db, "restart", "fixture", None, None, &until, true, None, &json!({})).await.unwrap();
        release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, after_reached.notified()).await.unwrap();
        let accepted_lease = crate::supervisor::store::acquire_lease(&env.app.db, "restart", "fixture", None, None, &until, true, Some(&bot), &json!({})).await.unwrap();
        let before_release = calls.load(Ordering::SeqCst);
        after_release.notify_one();
        let out = tokio::time::timeout(DEADLOCK_TIMEOUT, task).await.unwrap().unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.unwrap();
        let key = ApiTurnKey { bot_id: bot, run_id: run, turn_id: turn };
        wait_until_completed(&env.app, &key).await;
        let completed_lease = crate::supervisor::store::acquire_lease(&env.app.db, "restart", "fixture", None, None, &until, true, None, &json!({})).await.unwrap();
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
        assert!(blocked.is_err() && out.is_ok());
        assert!(pending_lease.is_none() && accepted_lease.is_none() && completed_lease.is_some());
        assert_eq!(before_release, 0); assert_eq!(calls.load(Ordering::SeqCst), 1);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE client_request_id=?").bind(&first_crid).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn native_join_error_is_busy_for_start_and_stop_does_not_forge_join() {
        let env = crate::testing::env().await;
        enable_fake_runtime(&env.app, "http://127.0.0.1:1".into(), DEADLOCK_TIMEOUT).await;
        let bot=crate::db::ulid(); let run=crate::db::ulid(); let turn=crate::db::ulid(); let conv=crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let key=FullKey { bot_id:bot.clone(), run_id:run.clone(), turn_id:turn.clone() };
        let (_tx, joined)=watch::channel(JoinState::Joined(Err(JoinError::Unconfirmed)));
        env.app.api_bot_runtime.inner.controls.lock().unwrap().insert(key.clone(), ControlRecord { identity:Arc::new(()), cancel:CancellationToken::new(), joined, finalized:None, owner_exited:true });
        let start_key=ApiRunKey { bot_id:bot.clone(), run_id:crate::db::ulid() };
        cleanup_terminal_controls(&env.app.db, &env.app.api_bot_runtime.inner).await;
        assert!(matches!(env.app.api_bot_runtime.native_start_guard(&start_key).await, Err(ApiBotError::Busy)));
        let stopped=tokio::time::timeout(DEADLOCK_TIMEOUT, crate::lifecycle::stop_bot(&env.app, &bot)).await.unwrap();
        assert!(stopped.is_err(), "unconfirmed join must not be reported as a successful stop");
        assert!(env.app.api_bot_runtime.inner.controls.lock().unwrap().contains_key(&key));
        let state: String=sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "stopping");
    }

    #[test]
    fn native_restart_resume_fits_one_mib_stack() {
        std::thread::Builder::new().stack_size(1024 * 1024).spawn(native_restart_resume_rechecks_durable_proof_and_rebinds).unwrap().join().unwrap();
    }

    #[tokio::test]
    async fn native_restart_resume_rechecks_durable_proof_and_rebinds() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0)); let hit = Arc::new(Notify::new());
        let (endpoint, shutdown, server) = fake_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint.clone(), DEADLOCK_TIMEOUT).await;
        let bot=crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let run = crate::lifecycle::start_bot(&env.app, &bot).await.unwrap();
        let first = crate::lifecycle::prompt(&env.app, &bot, "first", &crate::db::ulid()).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.unwrap();
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run, turn_id: first.turn_id };
        wait_until_completed(&env.app, &key).await;
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { loop { if env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.owner_exited) { break; } tokio::task::yield_now().await; } }).await.unwrap();
        let session: String = sqlx::query_scalar("SELECT id FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        crate::lifecycle::stop_bot(&env.app, &bot).await.unwrap();
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        let app = crate::testing::restart_app(&env).await;
        app.set_startup_ready(false);
        super::super::startup_gate::barrier(&app).await;
        app.set_startup_ready(true);
        assert!(app.api_startup_gate.ready());
        assert!(app.api_bot_runtime.inner.sessions.lock().unwrap().is_empty(), "history skip does not restore a cursor");
        enable_fake_runtime(&app, endpoint, DEADLOCK_TIMEOUT).await;
        let run2 = crate::lifecycle::start_bot_with(&app, &bot, crate::lifecycle::StartOpts { resume_native:true, resume_required:true, ..Default::default() }).await.unwrap();
        let second = crate::lifecycle::prompt(&app, &bot, "second", &crate::db::ulid()).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.unwrap();
        let key2 = ApiTurnKey { bot_id:bot.clone(), run_id:run2, turn_id:second.turn_id };
        wait_until_completed(&app, &key2).await;
        app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, app.background_tasks.wait()).await.unwrap();
        let (actual, generation): (String,i64) = sqlx::query_as("SELECT id,generation FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL").bind(&bot).fetch_one(&app.db).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
        assert_eq!(actual, session); assert_eq!(generation, 2); assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_key_snapshot_rotates_between_provider_calls_without_exposure() {
        use std::os::unix::fs::PermissionsExt;
        struct ServerState { calls: AtomicUsize, old_ok: std::sync::atomic::AtomicBool, new_ok: std::sync::atomic::AtomicBool, entered: Notify, release: Notify, tool: String }
        async fn complete(axum::extract::State(s): axum::extract::State<Arc<ServerState>>, headers: axum::http::HeaderMap, Json(_): Json<Value>) -> Json<Value> {
            let n = s.calls.fetch_add(1, Ordering::SeqCst);
            let key = headers.get("x-api-key").map(|h| h.as_bytes()).unwrap_or_default();
            if n == 0 {
                s.old_ok.store(key == b"synthetic-first", Ordering::SeqCst); s.entered.notify_one(); s.release.notified().await;
                Json(json!({"id":"fake-1","model":"claude-opus-5-5","content":[{"type":"tool_use","id":s.tool.clone(),"name":"bash","input":{"command":"true"}}],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}))
            } else {
                s.new_ok.store(key == b"synthetic-second", Ordering::SeqCst);
                Json(json!({"id":"fake-2","model":"claude-opus-5-5","content":[{"type":"text","text":"fixture"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}))
            }
        }
        let env = crate::testing::env().await;
        let fake_key = env.dir.join(format!("synthetic-{}", crate::db::ulid()));
        std::fs::write(&fake_key, "synthetic-first").unwrap(); std::fs::set_permissions(&fake_key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let state = Arc::new(ServerState { calls: AtomicUsize::new(0), old_ok: std::sync::atomic::AtomicBool::new(false), new_ok: std::sync::atomic::AtomicBool::new(false), entered: Notify::new(), release: Notify::new(), tool: crate::db::ulid() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); let addr = listener.local_addr().unwrap();
        let router = Router::new().route("/v1/messages", post(complete)).with_state(state.clone());
        let shutdown = CancellationToken::new(); let stop = shutdown.clone();
        let server = tokio::spawn(async move { axum::serve(listener, router).with_graceful_shutdown(async move { stop.cancelled().await }).await.unwrap() });
        enable_fake_runtime(&env.app, format!("http://{addr}/v1/messages"), DEADLOCK_TIMEOUT).await;
        let path = fake_key.to_string_lossy().into_owned();
        env.app.cfg.update(|cfg| { cfg.claude_api.key_file = path; Ok(()) }).await.unwrap();
        env.app.api_bot_runtime.set_test_dependencies(ApiBotDependencies { endpoint:format!("http://{addr}/v1/messages"), key_source:Arc::new(ConfigKeySource(Arc::downgrade(&env.app))), quote:Arc::new(Quote), tools:Arc::new(NoTools), shutdown_join_timeout:Some(DEADLOCK_TIMEOUT) });
        let bot=crate::db::ulid(); let crid=crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let run=crate::lifecycle::start_bot(&env.app, &bot).await.unwrap();
        let out=crate::lifecycle::prompt(&env.app, &bot, "fixture", &crid).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, state.entered.notified()).await.unwrap();
        std::fs::write(&fake_key, "synthetic-second").unwrap();
        state.release.notify_one();
        let key=ApiTurnKey { bot_id:bot, run_id:run, turn_id:out.turn_id };
        wait_until_completed(&env.app, &key).await;
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap(); std::fs::remove_file(fake_key).unwrap();
        assert_eq!(state.calls.load(Ordering::SeqCst), 2);
        assert!(state.old_ok.load(Ordering::SeqCst) && state.new_ok.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_key_read_fault_causes_no_reservation_or_http() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0)); let tool_calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint.clone(), DEADLOCK_TIMEOUT).await;
        let missing = env.dir.join(format!("synthetic-missing-{}", crate::db::ulid())).to_string_lossy().into_owned();
        env.app.cfg.update(|cfg| { cfg.claude_api.key_file = missing; Ok(()) }).await.unwrap();
        env.app.api_bot_runtime.set_test_dependencies(ApiBotDependencies { endpoint, key_source:Arc::new(ConfigKeySource(Arc::downgrade(&env.app))), quote:Arc::new(Quote), tools:Arc::new(CountingTools(tool_calls.clone())), shutdown_join_timeout:Some(DEADLOCK_TIMEOUT) });
        let bot=crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        crate::lifecycle::start_bot(&env.app, &bot).await.unwrap();
        let _ = crate::lifecycle::prompt(&env.app, &bot, "fixture", &crate::db::ulid()).await;
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        let reservations: i64=sqlx::query_scalar("SELECT count(*) FROM api_bot_attempts WHERE bot_id=?").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
        assert_eq!(reservations, 0); assert_eq!(calls.load(Ordering::SeqCst), 0); assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn native_second_turn_reuses_same_run_only_after_finalized_owner() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0)); let hit = Arc::new(Notify::new());
        let (endpoint, shutdown, server) = fake_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot=crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let run=crate::lifecycle::start_bot(&env.app,&bot).await.unwrap();
        let first=crate::lifecycle::prompt(&env.app,&bot,"first",&crate::db::ulid()).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.unwrap();
        let key=ApiTurnKey { bot_id:bot.clone(),run_id:run.clone(),turn_id:first.turn_id };
        wait_until_completed(&env.app,&key).await;
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { loop { if env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.owner_exited && r.finalized.is_some()) { break; } tokio::task::yield_now().await; } }).await.unwrap();
        let session: String=sqlx::query_scalar("SELECT id FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        let second=crate::lifecycle::prompt(&env.app,&bot,"second",&crate::db::ulid()).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await.unwrap();
        let key2=ApiTurnKey { bot_id:bot.clone(),run_id:run.clone(),turn_id:second.turn_id };
        wait_until_completed(&env.app,&key2).await;
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { loop { if env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key2)).is_some_and(|r| r.owner_exited) { break; } tokio::task::yield_now().await; } }).await.unwrap();
        crate::lifecycle::stop_bot(&env.app,&bot).await.unwrap();
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT,env.app.background_tasks.wait()).await.unwrap();
        let restarted=crate::testing::restart_app(&env).await;
        restarted.set_startup_ready(false);
        super::super::startup_gate::barrier(&restarted).await;
        let ready=restarted.api_startup_gate.ready();
        let (s,g,r): (String,i64,String)=sqlx::query_as("SELECT id,generation,owner_run_id FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
        assert!(ready, "historical same-run Applied receipts must not quarantine restart");
        assert_eq!((s,g,r),(session,1,run)); assert_eq!(calls.load(Ordering::SeqCst),2);
    }

    #[tokio::test]
    async fn native_preflight_disabled_refuses_fresh_before_stop_or_delete() {
        let env = crate::testing::env().await;
        let bot=crate::db::ulid(); let run=crate::db::ulid(); let turn=crate::db::ulid(); let conv=crate::db::ulid(); let session=crate::db::ulid();
        insert_bot_and_turn(&env.app,&bot,&run,&turn,&conv,&env.project_id,"fixture").await;
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,1,0,'{}',?,?,?)").bind(&session).bind(&bot).bind(&run).bind(&turn).bind(crate::db::now()).bind(crate::db::now()).bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
        let target=crate::lifecycle::api_bot_port::ApiFreshTarget { session_id:session.clone(),generation:1 };
        let result=env.app.api_native_start(&bot,crate::lifecycle::api_bot_port::ApiNativeStartMode::Fresh(target)).await;
        let state:String=sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap();
        let attempts:i64=sqlx::query_scalar("SELECT count(*) FROM api_bot_attempts WHERE bot_id=?").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        assert!(result.is_err()); assert_eq!(state,"running"); assert_eq!(count,1); assert_eq!(attempts,0);
    }

    #[tokio::test]
    async fn native_fresh_descriptor_gate_timeout_is_bot_scoped_and_quarantine_closed() {
        use crate::lifecycle::api_bot_port::ApiFreshAvailability as A;
        let env=crate::testing::env().await;
        let a=crate::db::ulid(); let b=crate::db::ulid();
        for bot in [&a,&b] {
            let run=crate::db::ulid(); let turn=crate::db::ulid(); let conv=crate::db::ulid(); let session=crate::db::ulid();
            insert_bot_and_turn(&env.app,bot,&run,&turn,&conv,&env.project_id,"fixture").await;
            sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,1,0,'{}',?,?,?)").bind(session).bind(bot).bind(run).bind(turn).bind(crate::db::now()).bind(crate::db::now()).bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
        }
        let gate=env.app.api_bot_runtime.bot_gate(&a).await;
        let app=env.app.clone(); let bot_a=a.clone();
        let waiting=tokio::spawn(async move { app.api_fresh_target(&bot_a).await });
        let other=tokio::time::timeout(DEADLOCK_TIMEOUT,env.app.api_fresh_target(&b)).await.unwrap();
        let blocked=tokio::time::timeout(DEADLOCK_TIMEOUT,waiting).await.unwrap().unwrap();
        drop(gate);
        env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().insert(a.clone(),Arc::new(()));
        let quarantined=env.app.api_fresh_target(&a).await;
        let remaining:i64=sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE bot_id IN (?,?)").bind(&a).bind(&b).fetch_one(&env.app.db).await.unwrap();
        assert!(matches!(other,A::Available(_)) && matches!(blocked,A::Unavailable) && matches!(quarantined,A::Unavailable));
        assert_eq!(remaining,2);
    }

    #[tokio::test]
    async fn native_delivery_cas_fault_cancels_and_joins_without_release() {
        let env=crate::testing::env().await;
        let calls=Arc::new(AtomicUsize::new(0)); let tool_calls=Arc::new(AtomicUsize::new(0));
        let (endpoint,shutdown,server)=fake_server(calls.clone(),Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app,endpoint.clone(),DEADLOCK_TIMEOUT).await;
        env.app.api_bot_runtime.set_test_dependencies(ApiBotDependencies { endpoint, key_source:Arc::new(super::super::adapter::FixedTestKey("synthetic".into())),quote:Arc::new(Quote),tools:Arc::new(CountingTools(tool_calls.clone())),shutdown_join_timeout:Some(DEADLOCK_TIMEOUT) });
        let bot=crate::db::ulid(); let crid=crate::db::ulid();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let run=crate::lifecycle::start_bot(&env.app,&bot).await.unwrap();
        let reached=Arc::new(Notify::new()); let release=Arc::new(Notify::new()); let a=reached.clone(); let b=release.clone();
        crate::lifecycle::race_point::arm("api_native_prompt_after_pending_commit",&crid,move || async move { a.notify_one(); b.notified().await; });
        let app=env.app.clone(); let task_bot=bot.clone(); let task_crid=crid.clone();
        let task=tokio::spawn(async move { crate::lifecycle::prompt(&app,&task_bot,"fixture",&task_crid).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT,reached.notified()).await.unwrap();
        let turn:String=sqlx::query_scalar("SELECT id FROM turns WHERE client_request_id=?").bind(&crid).fetch_one(&env.app.db).await.unwrap();
        let trigger=format!("CREATE TRIGGER native_reject_delivery BEFORE UPDATE OF delivery ON turns WHEN NEW.id='{turn}' AND NEW.delivery='ok' BEGIN SELECT RAISE(ABORT,'fixture'); END");
        sqlx::query(&trigger).execute(&env.app.db).await.unwrap(); release.notify_one();
        let result=tokio::time::timeout(DEADLOCK_TIMEOUT,task).await.unwrap().unwrap();
        let retry=crate::lifecycle::prompt(&env.app,&bot,"fixture",&crid).await;
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT,env.app.background_tasks.wait()).await.unwrap();
        let delivery:String=sqlx::query_scalar("SELECT delivery FROM turns WHERE id=?").bind(&turn).fetch_one(&env.app.db).await.unwrap();
        let exited=env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey { bot_id:bot,run_id:run.clone(),turn_id:turn }).is_some_and(|r|r.owner_exited);
        let empty=!env.app.api_bot_runtime.inner.registry.is_active(&run);
        shutdown.cancel(); server.await.unwrap();
        assert!(result.is_err() && retry.is_err()); assert_eq!(delivery,"pending"); assert!(exited && empty);
        assert_eq!(calls.load(Ordering::SeqCst),0); assert_eq!(tool_calls.load(Ordering::SeqCst),0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_key_read_failure_does_not_poison_another_bot_after_repair() {
        use std::os::unix::fs::PermissionsExt;
        let env=crate::testing::env().await; let calls=Arc::new(AtomicUsize::new(0)); let hit=Arc::new(Notify::new());
        let (endpoint,shutdown,server)=fake_server(calls.clone(),hit.clone()).await;
        enable_fake_runtime(&env.app,endpoint.clone(),DEADLOCK_TIMEOUT).await;
        let fake_key=env.dir.join(format!("synthetic-{}",crate::db::ulid())); let path=fake_key.to_string_lossy().into_owned();
        env.app.cfg.update(|cfg| {cfg.claude_api.key_file=path; Ok(())}).await.unwrap();
        env.app.api_bot_runtime.set_test_dependencies(ApiBotDependencies {endpoint,key_source:Arc::new(ConfigKeySource(Arc::downgrade(&env.app))),quote:Arc::new(Quote),tools:Arc::new(NoTools),shutdown_join_timeout:Some(DEADLOCK_TIMEOUT)});
        let a=crate::db::ulid(); let b=crate::db::ulid();
        for bot in [&a,&b] { sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,hook_token,created_at) VALUES(?,?,?,'claude-api','high','fixture',?)").bind(bot).bind(&env.project_id).bind(bot).bind(crate::db::now()).execute(&env.app.db).await.unwrap(); crate::lifecycle::start_bot(&env.app,bot).await.unwrap(); }
        let first=crate::lifecycle::prompt(&env.app,&a,"fixture",&crate::db::ulid()).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT,async { loop { let status:String=sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&first.turn_id).fetch_one(&env.app.db).await.unwrap(); if status=="failed" {break} tokio::task::yield_now().await; } }).await.unwrap();
        std::fs::write(&fake_key,"synthetic-repaired").unwrap(); std::fs::set_permissions(&fake_key,std::fs::Permissions::from_mode(0o600)).unwrap();
        let second=crate::lifecycle::prompt(&env.app,&b,"fixture",&crate::db::ulid()).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT,hit.notified()).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT,async { loop { let status:String=sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&second.turn_id).fetch_one(&env.app.db).await.unwrap(); if status=="completed" {break} tokio::task::yield_now().await; } }).await.unwrap();
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT,env.app.background_tasks.wait()).await.unwrap();
        let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM api_bot_attempts WHERE bot_id=?),(SELECT count(*) FROM api_bot_attempts WHERE bot_id=?)").bind(&a).bind(&b).fetch_one(&env.app.db).await.unwrap();
        shutdown.cancel(); server.await.unwrap(); std::fs::remove_file(fake_key).unwrap();
        assert_eq!(counts,(0,1)); assert_eq!(calls.load(Ordering::SeqCst),1);
    }

    struct Quote;

    impl super::super::quote::QuoteProvider for Quote {
        fn reserve(&self, _: &RequestEnvelope) -> Result<ReservationQuote, QuoteError> {
            Ok(ReservationQuote {
                reserved_microusd: 100,
                pricing_version: "fixture-v1".into(),
            })
        }

        fn settle(&self, _: &RequestEnvelope, _: &Response) -> Result<UsageQuote, QuoteError> {
            Ok(UsageQuote {
                estimated_microusd: 50,
                actual_speed: None,
            })
        }
    }

    struct NoTools;

    impl super::super::runner::Tools for NoTools {
        fn execute<'a>(
            &'a self,
            _: &'a crate::api_bot::protocol::ToolCall,
            _: &'a CancellationToken,
        ) -> futures::future::BoxFuture<'a, Result<String, String>> {
            Box::pin(async { Err("unexpected tool call".into()) })
        }
    }

    struct CountingTools(Arc<AtomicUsize>);
    impl super::super::runner::Tools for CountingTools {
        fn execute<'a>(&'a self, _: &'a crate::api_bot::protocol::ToolCall, _: &'a CancellationToken) -> futures::future::BoxFuture<'a, Result<String, String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("unexpected tool call".into()) })
        }
    }

    struct BlockingTools {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl super::super::runner::Tools for BlockingTools {
        fn execute<'a>(
            &'a self,
            _: &'a crate::api_bot::protocol::ToolCall,
            _: &'a CancellationToken,
        ) -> futures::future::BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok("fixture tool result".into())
            })
        }
    }

    #[derive(Clone)]
    struct LockWaitProbe {
        app: Arc<crate::state::App>,
        requested: Arc<Notify>,
    }

    impl crate::capabilities::Db for LockWaitProbe {
        fn db(&self) -> &sqlx::SqlitePool {
            &self.app.db
        }
    }

    impl crate::capabilities::BotLocks for LockWaitProbe {
        fn bot_lock(
            &self,
            bot_id: &str,
        ) -> impl Future<Output = Arc<tokio::sync::Mutex<()>>> + Send {
            let app = self.app.clone();
            let requested = self.requested.clone();
            let bot_id = bot_id.to_string();
            async move {
                let lock = app.bot_lock(&bot_id).await;
                // The current-thread test task cannot observe this notification until
                // settle_once polls `lock().await` and yields while stop still owns the lock.
                requested.notify_one();
                lock
            }
        }
    }

    impl crate::capabilities::BotStatusEmit for LockWaitProbe {
        fn emit_bot_status(&self, bot_id: &str) -> impl Future<Output = ()> + Send {
            self.app.emit_bot_status(bot_id)
        }
    }

    impl crate::lifecycle::s6_ports::RunStateServices for LockWaitProbe {
        fn reconcile_host(&self, host: &str) -> impl Future<Output = anyhow::Result<()>> + Send {
            let app = self.app.clone();
            let host = host.to_string();
            async move { crate::runners::reconcile::reconcile_host(&app, &host).await }
        }

        fn finish_stop(&self, run_id: &str) -> impl Future<Output = ()> + Send {
            let app = self.app.clone();
            let run_id = run_id.to_string();
            async move { crate::lifecycle::finish_stop(&app, &run_id).await }
        }
    }

    struct HeldResponse {
        calls: Arc<AtomicUsize>,
        hit: Arc<Notify>,
        response: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<Value>>>,
    }

    async fn fake_server(
        calls: Arc<AtomicUsize>,
        hit: Arc<Notify>,
    ) -> (String, CancellationToken, tokio::task::JoinHandle<()>) {
        async fn complete(
            axum::extract::State((calls, hit)): axum::extract::State<(
                Arc<AtomicUsize>,
                Arc<Notify>,
            )>,
            Json(_body): Json<Value>,
        ) -> Json<Value> {
            calls.fetch_add(1, Ordering::SeqCst);
            hit.notify_one();
            Json(json!({
                "id":"fixture-message",
                "type":"message",
                "role":"assistant",
                "model":"claude-opus-5-5",
                "content":[{"type":"text","text":"fixture answer"}],
                "stop_reason":"end_turn",
                "usage":{"input_tokens":1,"output_tokens":1}
            }))
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/messages", post(complete))
            .with_state((calls, hit));
        let shutdown = CancellationToken::new();
        let shutdown_task = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown_task.cancelled().await })
                .await
                .unwrap();
        });
        (format!("http://{address}/v1/messages"), shutdown, task)
    }

    async fn held_response_server(
        calls: Arc<AtomicUsize>,
        hit: Arc<Notify>,
    ) -> (
        String,
        CancellationToken,
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Sender<Value>,
    ) {
        async fn complete(
            axum::extract::State(state): axum::extract::State<Arc<HeldResponse>>,
            Json(_body): Json<Value>,
        ) -> Json<Value> {
            state.calls.fetch_add(1, Ordering::SeqCst);
            state.hit.notify_one();
            let response = state.response.lock().await.take().unwrap().await.unwrap();
            Json(response)
        }
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let state = Arc::new(HeldResponse {
            calls,
            hit,
            response: tokio::sync::Mutex::new(Some(response_rx)),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/messages", post(complete))
            .with_state(state);
        let shutdown = CancellationToken::new();
        let shutdown_task = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown_task.cancelled().await })
                .await
                .unwrap();
        });
        (
            format!("http://{address}/v1/messages"),
            shutdown,
            task,
            response_tx,
        )
    }

    async fn tool_response_server(
        calls: Arc<AtomicUsize>,
        hit: Arc<Notify>,
    ) -> (String, CancellationToken, tokio::task::JoinHandle<()>) {
        async fn complete(
            axum::extract::State((calls, hit)): axum::extract::State<(
                Arc<AtomicUsize>,
                Arc<Notify>,
            )>,
            Json(_body): Json<Value>,
        ) -> Json<Value> {
            calls.fetch_add(1, Ordering::SeqCst);
            hit.notify_one();
            Json(json!({
                "id":"fixture-tool-message",
                "type":"message",
                "role":"assistant",
                "model":"claude-opus-5-5",
                "content":[{"type":"tool_use","id":"toolu_fixture","name":"bash","input":{"command":"true"}}],
                "stop_reason":"tool_use",
                "usage":{"input_tokens":1,"output_tokens":1}
            }))
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/messages", post(complete))
            .with_state((calls, hit));
        let shutdown = CancellationToken::new();
        let shutdown_task = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown_task.cancelled().await })
                .await
                .unwrap();
        });
        (format!("http://{address}/v1/messages"), shutdown, task)
    }

    async fn enable_fake_runtime(
        app: &crate::state::App,
        endpoint: String,
        shutdown_timeout: Duration,
    ) {
        app.cfg
            .update(|cfg| {
                cfg.claude_api.enabled = true;
                cfg.claude_api.api_bot.enabled = true;
                cfg.claude_api.api_bot.max_turn_cost_microusd = Some(1000);
                cfg.claude_api.api_bot.turn_timeout_secs = 30;
                cfg.claude_api.api_bot.command_timeout_secs = 10;
                Ok(())
            })
            .await
            .unwrap();
        app.api_bot_runtime.set_test_dependencies(ApiBotDependencies {
            endpoint,
            key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-only".into())),
            quote: Arc::new(Quote),
            tools: Arc::new(NoTools),
            shutdown_join_timeout: Some(shutdown_timeout),
        });
    }

    async fn insert_bot_and_turn(
        app: &crate::state::App,
        bot_id: &str,
        run_id: &str,
        turn_id: &str,
        conversation_id: &str,
        project_id: &str,
        prompt: &str,
    ) {
        let now = crate::db::now();
        sqlx::query("INSERT OR IGNORE INTO bots(id,project_id,name,kind,effort,fast,hook_token,created_at) VALUES(?,?,?,'claude-api','high',0,'fixture',?)")
            .bind(bot_id)
            .bind(project_id)
            .bind(format!("api-test-{bot_id}"))
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT OR IGNORE INTO conversations(id,bot_id,created_at) VALUES(?,?,?)")
            .bind(conversation_id)
            .bind(bot_id)
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
            .bind(run_id)
            .bind(bot_id)
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES(?,?,?,'web','in_flight','ok',?,?)")
            .bind(turn_id)
            .bind(conversation_id)
            .bind(run_id)
            .bind(prompt)
            .bind(now)
            .execute(&app.db)
            .await
            .unwrap();
    }

    fn clone_turn_key(key: &ApiTurnKey) -> ApiTurnKey {
        ApiTurnKey {
            bot_id: key.bot_id.clone(),
            run_id: key.run_id.clone(),
            turn_id: key.turn_id.clone(),
        }
    }

    async fn wait_until_joined(app: &crate::state::App, key: &ApiTurnKey) {
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while app.api_run_live(key).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_until_completed(app: &crate::state::App, key: &ApiTurnKey) {
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            loop {
                let status: Option<String> = sqlx::query_scalar(
                    "SELECT status FROM turns WHERE id=? AND run_id=?",
                )
                .bind(&key.turn_id)
                .bind(&key.run_id)
                .fetch_optional(&app.db)
                .await
                .unwrap();
                if status.as_deref() == Some("completed") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn create_completion_session(
        app: &crate::state::App,
        key: &ApiTurnKey,
    ) -> super::super::adapter::AdapterIdentity {
        let session_id = crate::db::ulid();
        let identity = super::super::adapter::AdapterIdentity {
            session_id: session_id.clone(),
            bot_id: key.bot_id.clone(),
            run_id: key.run_id.clone(),
            turn_id: key.turn_id.clone(),
            generation: 1,
        };
        let fence = am_base::db::api_bot::Fence {
            session: &session_id,
            bot: &key.bot_id,
            run: &key.run_id,
            turn: &key.turn_id,
            generation: 1,
            revision: 0,
        };
        let checkpoint = am_base::db::api_bot::Checkpoint::new(&json!({
            "messages": [{"role":"user","content":"fixture prompt"}],
            "responses": [],
            "api_pending": false,
            "tool_pending": null,
            "completed_tool_ids": []
        }))
        .unwrap();
        am_base::db::api_bot::create(&app.db, &fence, &checkpoint)
            .await
            .unwrap();
        identity
    }

    async fn reserve_completion_attempt(
        app: &crate::state::App,
        identity: &super::super::adapter::AdapterIdentity,
    ) {
        let fence = am_base::db::api_bot::Fence {
            session: &identity.session_id,
            bot: &identity.bot_id,
            run: &identity.run_id,
            turn: &identity.turn_id,
            generation: identity.generation,
            revision: 0,
        };
        let checkpoint = am_base::db::api_bot::Checkpoint::new(&json!({
            "messages": [{"role":"user","content":"fixture prompt"}],
            "responses": [],
            "api_pending": true,
            "tool_pending": null,
            "completed_tool_ids": []
        }))
        .unwrap();
        am_base::db::api_bot::reserve(
            &app.db,
            &fence,
            &am_base::db::api_bot::Attempt {
                id: "runtime-completion-reservation",
                ordinal: 0,
                request_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                speed: "standard",
                reserved_microusd: 250,
                pricing_version: "fixture-v1",
            },
            &checkpoint,
            1000,
            10,
        )
        .await
        .unwrap();
    }

    fn completion_response() -> Response {
        Response {
            id: "runtime-join-fixture".into(),
            model: "fixture-model".into(),
            content: vec![json!({"type":"text","text":"joined final"})],
            stop_reason: "end_turn".into(),
            usage: json!({"input_tokens":1,"output_tokens":1}),
        }
    }

    #[tokio::test]
    async fn production_app_port_calls_are_direct_fail_closed_and_new_is_idle() {
        let dir = crate::testing::track(
            std::env::temp_dir().join(format!("api-bot-inert-{}", crate::db::ulid())),
        );
        std::fs::create_dir_all(&dir).unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let cfg = crate::runners::app_ports_p2::load_config(dir.join("config.toml"))
            .await
            .unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        let app = crate::state::App::new(
            pool.clone(),
            client.clone(),
            client,
            cfg,
            dir.clone(),
            dir.join("daemon"),
            7799,
            "test".into(),
            "test".into(),
            false,
        );
        let key = ApiTurnKey {
            bot_id: "missing".into(),
            run_id: "missing".into(),
            turn_id: "missing".into(),
        };
        assert_eq!(app.api_turn_spawn(&key).await, Err(ApiBotError::Upstream));
        assert_eq!(
            app.api_run_cancel_and_join(&key).await,
            Err(ApiBotError::Upstream)
        );
        assert_eq!(
            app.api_run_stop_proof(&ApiRunKey {
                bot_id: key.bot_id.clone(),
                run_id: key.run_id.clone(),
            })
            .await,
            Err(ApiBotError::Upstream)
        );
        assert!(!app.api_run_live(&key).await);
        assert_eq!(
            pool.size(),
            0,
            "App::new and production port calls do not open SQLite connections"
        );
        assert_eq!(
            app.background_tasks.len(),
            0,
            "App::new starts no API bot background tasks"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn app_new_has_no_api_bot_dependencies_or_preflight_db_and_config_reads() {
        let dir = crate::testing::track(
            std::env::temp_dir().join(format!("api-bot-no-deps-{}", crate::db::ulid())),
        );
        std::fs::create_dir_all(&dir).unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let cfg = crate::runners::app_ports_p2::load_config(dir.join("config.toml"))
            .await
            .unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        am_config::GET_CALLS.with(|calls| calls.set(0));
        let app = crate::state::App::new(
            pool.clone(),
            client.clone(),
            client,
            cfg,
            dir.clone(),
            dir.join("daemon"),
            7799,
            "test".into(),
            "test".into(),
            false,
        );
        assert!(app
            .api_bot_runtime
            .dependencies
            .lock()
            .expect("API bot dependencies mutex poisoned")
            .is_none());
        assert_eq!(
            am_config::GET_CALLS.with(|calls| calls.get()),
            0,
            "App::new does not read ConfigStore"
        );

        let key = ApiTurnKey {
            bot_id: "missing".into(),
            run_id: "missing".into(),
            turn_id: "missing".into(),
        };
        assert_eq!(app.api_turn_spawn(&key).await, Err(ApiBotError::Upstream));
        assert_eq!(
            app.api_run_cancel_and_join(&key).await,
            Err(ApiBotError::Upstream)
        );
        assert_eq!(
            app.api_run_stop_proof(&ApiRunKey {
                bot_id: key.bot_id.clone(),
                run_id: key.run_id.clone(),
            })
            .await,
            Err(ApiBotError::Upstream)
        );
        assert!(!app.api_run_live(&key).await);
        assert_eq!(pool.size(), 0, "all four port calls reject before SQLite");
        assert_eq!(
            am_config::GET_CALLS.with(|calls| calls.get()),
            0,
            "all four port calls reject before ConfigStore::get"
        );
        assert_eq!(app.background_tasks.len(), 0);
    }

    #[tokio::test]
    async fn production_port_has_zero_http_and_zero_attempts_even_with_a_fixture_server() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (_endpoint, server_shutdown, server_task) = fake_server(calls.clone(), hit).await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "fixture prompt",
        )
        .await;
        let key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id,
            turn_id,
        };

        assert_eq!(
            env.app.api_turn_spawn(&key).await,
            Err(ApiBotError::Upstream)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let attempts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM api_bot_attempts WHERE bot_id=?")
                .bind(&bot_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        let sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM api_bot_sessions WHERE bot_id=?")
                .bind(&bot_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        assert_eq!(
            (attempts, sessions),
            (0, 0),
            "FailClosed rejects before reserve and checkpoint creation"
        );
        server_shutdown.cancel();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn same_run_stop_missing_control_distinguishes_handover() {
        let env = crate::testing::env().await;
        enable_fake_runtime(&env.app, "http://127.0.0.1:1".into(), DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let first = crate::db::ulid(); let second = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &first, &conv, &env.project_id, "fixture").await;
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: second };
        let old = FullKey { bot_id: bot, run_id: run, turn_id: first };
        let (_tx, joined) = watch::channel(JoinState::Joined(Ok(Completion::Completed)));
        env.app.api_bot_runtime.inner.controls.lock().unwrap().insert(old.clone(), ControlRecord { identity: Arc::new(()), cancel: CancellationToken::new(), joined, finalized: None, owner_exited: false });
        assert_eq!(env.app.api_run_cancel_and_join(&key).await, Err(ApiBotError::Busy));
        env.app.api_bot_runtime.inner.controls.lock().unwrap().remove(&old);
        assert_eq!(env.app.api_run_cancel_and_join(&key).await, Err(ApiBotError::Unconfirmed));
    }

    #[test]
    fn fresh_true_owner_and_transaction_fit_one_mib_stack() {
        std::thread::Builder::new().stack_size(1024 * 1024).spawn(|| {
            fresh_applied_receipt_waits_for_actual_owner_exit();
            fresh_waits_for_stop_true_join_and_owner_before_deletion();
        }).unwrap().join().unwrap();
    }

    #[tokio::test]
    async fn fresh_real_commit_synthetic_lost_ack_clears_exact_memory_and_marker() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshResult};
        let env = super::super::retention_tests::fixture().await;
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
        let session = crate::db::ulid();
        sqlx::query("UPDATE api_bot_sessions SET id=? WHERE id='r2-session'").bind(&session).execute(&env.app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,'r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&session).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        let cursor = SessionCursor { identity: super::super::adapter::AdapterIdentity { session_id: session.clone(), bot_id: "r2-bot".into(), run_id: "r2-run".into(), turn_id: "r2-turn".into(), generation: 1 }, revision: 5000 };
        env.app.api_bot_runtime.inner.sessions.lock().unwrap().insert("r2-bot".into(), cursor);
        super::super::explicit_retirement::arm_lost_ack_for_test(&session);
        let target = ApiFreshTarget { session_id: session.clone(), generation: 1 };
        assert!(matches!(env.app.api_explicit_retire("r2-bot", &target).await, Ok(ApiFreshResult::Cleared)));
        assert!(!env.app.api_bot_runtime.inner.sessions.lock().unwrap().contains_key("r2-bot"));
        assert!(!env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().contains_key("r2-bot"));
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn fresh_applied_receipt_waits_for_actual_owner_exit() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshResult};
        let env = crate::testing::env().await;
        let (endpoint, shutdown, server) = fake_server(Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_same_run_owner_before_exit", &run, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: turn };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        let (session, generation): (String, i64) = sqlx::query_as("SELECT id,generation FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_join_receipts WHERE session_id=? AND state='applied'").bind(&session).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(applied, 1);
        let app = env.app.clone(); let target_bot = bot.clone(); let target_session = session.clone();
        let task = tokio::spawn(async move { app.api_explicit_retire(&target_bot, &ApiFreshTarget { session_id: target_session, generation }).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            loop { let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap(); if state == "stopped" { break; } tokio::task::yield_now().await; }
        }).await.unwrap();
        assert!(!task.is_finished(), "owner has not exited yet");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 1);
        release.notify_one();
        assert!(matches!(tokio::time::timeout(DEADLOCK_TIMEOUT, task).await.unwrap().unwrap(), Ok(ApiFreshResult::Cleared)));
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 0);
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_waits_for_stop_true_join_and_owner_before_deletion() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshError};
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_run_runner_before_entry", &run, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: turn };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        let (session, generation): (String, i64) = sqlx::query_as("SELECT id,generation FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        let cancellation = env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).unwrap().cancel.clone();
        let app = env.app.clone(); let id = bot.clone(); let target_session = session.clone();
        let task = tokio::spawn(async move { app.api_explicit_retire(&id, &ApiFreshTarget { session_id: target_session, generation }).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, cancellation.cancelled()).await.unwrap();
        assert!(!task.is_finished());
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 1);
        release.notify_one();
        let result = tokio::time::timeout(DEADLOCK_TIMEOUT, task).await.unwrap().unwrap();
        assert!(matches!(result, Err(ApiFreshError::Unresolved)), "a true join without a durable receipt is not deletion proof");
        let evidence: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM api_bot_sessions WHERE id=?),(SELECT count(*) FROM api_bot_join_receipts WHERE session_id=?)").bind(&session).bind(&session).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(evidence, (1, 0)); assert_eq!(calls.load(Ordering::SeqCst), 0);
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_dropped_caller_keeps_spawn_blocked_until_commit_owner_finishes() {
        use crate::lifecycle::api_bot_port::ApiFreshTarget;
        let env = super::super::retention_tests::fixture().await;
        let session = crate::db::ulid();
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET id=? WHERE id='r2-session'").bind(&session).execute(&env.app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,'r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&session).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0)); let tool_calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let mut deps = env.app.api_bot_runtime.test_dependencies().unwrap(); deps.tools = Arc::new(CountingTools(tool_calls.clone())); env.app.api_bot_runtime.set_test_dependencies(deps);
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_fresh_after_commit", &session, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let app = env.app.clone(); let task_session = session.clone();
        let caller = tokio::spawn(async move { app.api_explicit_retire("r2-bot", &ApiFreshTarget { session_id: task_session, generation: 1 }).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        caller.abort(); let _ = caller.await;
        let key = ApiTurnKey { bot_id: "r2-bot".into(), run_id: "r2-run".into(), turn_id: "r2-turn".into() };
        let mut spawn = Box::pin(env.app.api_turn_spawn(&key));
        assert!(futures::poll!(spawn.as_mut()).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 0); assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        release.notify_one();
        assert!(tokio::time::timeout(DEADLOCK_TIMEOUT, spawn).await.unwrap().is_err());
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        assert!(!env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().contains_key("r2-bot"));
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap(); assert_eq!(n, 0);
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_quarantine_recovers_only_after_restart_startup_proof() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshError};
        let env = super::super::retention_tests::fixture().await;
        let session = crate::db::ulid();
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET id=? WHERE id='r2-session'").bind(&session).execute(&env.app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,'r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&session).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        crate::lifecycle::race_point::arm("api_fresh_after_commit", &session, || async move { panic!("synthetic owner panic"); });
        let target = ApiFreshTarget { session_id: session, generation: 1 };
        assert!(matches!(env.app.api_explicit_retire("r2-bot", &target).await, Err(ApiFreshError::Unconfirmed)));
        assert!(env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().contains_key("r2-bot"));
        let restarted = crate::testing::restart_app(&env).await;
        assert!(!restarted.api_startup_gate.ready());
        restarted.set_startup_ready(false);
        super::super::startup_gate::barrier(&restarted).await;
        assert!(restarted.api_startup_gate.ready());
        assert!(!restarted.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().contains_key("r2-bot"));
    }

    #[tokio::test]
    async fn fresh_does_not_clear_a_replaced_operation_token() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshError};
        let env = super::super::retention_tests::fixture().await;
        let session = crate::db::ulid();
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET id=? WHERE id='r2-session'").bind(&session).execute(&env.app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,'r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&session).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_fresh_before_commit", &session, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let app = env.app.clone(); let task_session = session.clone();
        let task = tokio::spawn(async move { app.api_explicit_retire("r2-bot", &ApiFreshTarget { session_id: task_session, generation: 1 }).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        let other_token = Arc::new(());
        env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().insert("r2-bot".into(), other_token.clone());
        release.notify_one();
        assert!(matches!(tokio::time::timeout(DEADLOCK_TIMEOUT, task).await.unwrap().unwrap(), Err(ApiFreshError::Unconfirmed)));
        assert!(env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().get("r2-bot").is_some_and(|token| Arc::ptr_eq(token, &other_token)));
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap(); assert_eq!(n, 0);
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
    }

    #[tokio::test]
    async fn fresh_quarantine_preflight_rejects_before_stop_or_mutation() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshError};
        let env = super::super::retention_tests::fixture().await;
        sqlx::query("UPDATE runs SET state='running' WHERE id='r2-run'").execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('fresh-inflight','r2-conv','r2-run','web','in_flight','ok',?)").bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('fresh-preflight-hold','r2-session','r2-bot',1,0,'fixture','standard','reserved',123,'fixture',?)").bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0)); let tool_calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let mut deps = env.app.api_bot_runtime.test_dependencies().unwrap(); deps.tools = Arc::new(CountingTools(tool_calls.clone())); env.app.api_bot_runtime.set_test_dependencies(deps);
        env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().insert("r2-bot".into(), Arc::new(()));
        let target = ApiFreshTarget { session_id: "r2-session".into(), generation: 1 };
        assert!(matches!(env.app.api_explicit_retire("r2-bot", &target).await, Err(ApiFreshError::Unconfirmed)));
        let state: (String, String, String, i64, i64, i64) = sqlx::query_as("SELECT r.state,t.status,a.state,a.reserved_microusd,(SELECT count(*) FROM api_bot_sessions WHERE id='r2-session'),(SELECT count(*) FROM api_bot_join_receipts WHERE session_id='r2-session') FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_attempts a ON a.id='fresh-preflight-hold' WHERE r.id='r2-run' AND t.id='fresh-inflight'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, ("running".into(), "in_flight".into(), "reserved".into(), 123, 1, 1));
        assert_eq!(calls.load(Ordering::SeqCst), 0); assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_shutdown_before_commit_rolls_back_and_keeps_marker() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshError};
        let env = super::super::retention_tests::fixture().await;
        let session = crate::db::ulid();
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET id=? WHERE id='r2-session'").bind(&session).execute(&env.app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,'r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&session).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_fresh_before_commit", &session, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let app = env.app.clone(); let task_session = session.clone();
        let task = tokio::spawn(async move { app.api_explicit_retire("r2-bot", &ApiFreshTarget { session_id: task_session, generation: 1 }).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        env.app.shutdown.cancel(); release.notify_one();
        assert!(matches!(tokio::time::timeout(DEADLOCK_TIMEOUT, task).await.unwrap().unwrap(), Err(ApiFreshError::Busy)));
        let count: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM api_bot_sessions WHERE id=?),(SELECT count(*) FROM api_bot_join_receipts WHERE session_id=?)").bind(&session).bind(&session).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, (1,1));
        assert!(env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().contains_key("r2-bot"));
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
    }

    #[tokio::test]
    async fn fresh_commit_error_target_present_keeps_quarantine() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshError};
        let env = super::super::retention_tests::fixture().await;
        sqlx::query("CREATE TABLE fresh_present_parent(id INTEGER PRIMARY KEY)").execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TABLE fresh_present_child(parent INTEGER REFERENCES fresh_present_parent(id) DEFERRABLE INITIALLY DEFERRED)").execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER fresh_present_fault AFTER DELETE ON api_bot_sessions BEGIN INSERT INTO fresh_present_child(parent) VALUES(7); END").execute(&env.app.db).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0)); let tool_calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let mut deps = env.app.api_bot_runtime.test_dependencies().unwrap(); deps.tools = Arc::new(CountingTools(tool_calls.clone())); env.app.api_bot_runtime.set_test_dependencies(deps);
        let target = ApiFreshTarget { session_id: "r2-session".into(), generation: 1 };
        assert!(matches!(tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.api_explicit_retire("r2-bot", &target)).await.unwrap(), Err(ApiFreshError::Unconfirmed)));
        assert!(env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().contains_key("r2-bot"));
        let count: (i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM api_bot_sessions WHERE id='r2-session'),(SELECT count(*) FROM api_bot_join_receipts WHERE session_id='r2-session')").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, (1,1));
        let key = ApiTurnKey { bot_id: "r2-bot".into(), run_id: "r2-run".into(), turn_id: "r2-turn".into() };
        assert_eq!(env.app.api_turn_spawn(&key).await, Err(ApiBotError::Unconfirmed));
        assert_eq!(calls.load(Ordering::SeqCst), 0); assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_commit_readback_fault_quarantines_only_target_bot() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshError};
        let env = super::super::retention_tests::fixture().await;
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET id='fresh-readback' WHERE id='r2-session'").execute(&env.app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES('fresh-readback','r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TABLE fresh_fault_parent(id INTEGER PRIMARY KEY)").execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TABLE fresh_fault_child(parent INTEGER REFERENCES fresh_fault_parent(id) DEFERRABLE INITIALLY DEFERRED)").execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER fresh_commit_fault AFTER DELETE ON api_bot_sessions BEGIN INSERT INTO fresh_fault_child(parent) VALUES(7); END").execute(&env.app.db).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let mut deps = env.app.api_bot_runtime.test_dependencies().unwrap();
        deps.tools = Arc::new(CountingTools(tool_calls.clone()));
        env.app.api_bot_runtime.set_test_dependencies(deps);
        let successor = SessionCursor { identity: super::super::adapter::AdapterIdentity { session_id: "readback-successor".into(), bot_id: "r2-bot".into(), run_id: "successor-run".into(), turn_id: "successor-turn".into(), generation: 2 }, revision: 1 };
        let successor_for_hook = successor.clone(); let hook_app = env.app.clone();
        let pool = env.app.db.clone();
        crate::lifecycle::race_point::arm("api_fresh_before_readback", "fresh-readback", move || async move {
            sqlx::query("ALTER TABLE api_bot_sessions RENAME TO fresh_unavailable_sessions").execute(&pool).await.unwrap();
            hook_app.api_bot_runtime.inner.sessions.lock().unwrap().insert("r2-bot".into(), successor_for_hook);
        });
        let target = ApiFreshTarget { session_id: "fresh-readback".into(), generation: 1 };
        assert!(matches!(env.app.api_explicit_retire("r2-bot", &target).await, Err(ApiFreshError::Unconfirmed)));
        assert!(env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().contains_key("r2-bot"));
        let key = ApiTurnKey { bot_id: "r2-bot".into(), run_id: "r2-run".into(), turn_id: "r2-turn".into() };
        assert_eq!(env.app.api_turn_spawn(&key).await, Err(ApiBotError::Unconfirmed));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        assert!(env.app.api_bot_runtime.inner.sessions.lock().unwrap().get("r2-bot") == Some(&successor));
        sqlx::query("ALTER TABLE fresh_unavailable_sessions RENAME TO api_bot_sessions").execute(&env.app.db).await.unwrap();
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let other = ApiTurnKey { bot_id: bot, run_id: run, turn_id: turn };
        env.app.api_turn_spawn(&other).await.unwrap(); wait_until_completed(&env.app, &other).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_panicked_owner_keeps_quarantine() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshError};
        let env = super::super::retention_tests::fixture().await;
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET id='fresh-panic' WHERE id='r2-session'").execute(&env.app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES('fresh-panic','r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        let (endpoint, shutdown, server) = fake_server(Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        crate::lifecycle::race_point::arm("api_fresh_after_commit", "fresh-panic", || async move { panic!("synthetic owner panic"); });
        let target = ApiFreshTarget { session_id: "fresh-panic".into(), generation: 1 };
        assert!(matches!(env.app.api_explicit_retire("r2-bot", &target).await, Err(ApiFreshError::Unconfirmed)));
        assert!(env.app.api_bot_runtime.inner.explicit_quarantine.lock().unwrap().contains_key("r2-bot"));
        let key = ApiTurnKey { bot_id: "r2-bot".into(), run_id: "r2-run".into(), turn_id: "r2-turn".into() };
        assert_eq!(env.app.api_turn_spawn(&key).await, Err(ApiBotError::Unconfirmed));
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn fresh_memory_cas_mismatch_quarantines_only_affected_bot() {
        use crate::lifecycle::api_bot_port::{ApiFreshTarget, ApiFreshError};
        let env = super::super::retention_tests::fixture().await;
        let session_id = crate::db::ulid();
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET id=? WHERE id='r2-session'").bind(&session_id).execute(&env.app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,'r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&session_id).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_fresh_after_commit", &session_id, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let app = env.app.clone(); let task_session = session_id.clone();
        let job = tokio::spawn(async move {
            let target = ApiFreshTarget { session_id: task_session, generation: 1 };
            app.api_explicit_retire("r2-bot", &target).await
        });
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        let successor = SessionCursor { identity: super::super::adapter::AdapterIdentity { session_id: "successor".into(), bot_id: "r2-bot".into(), run_id: "successor-run".into(), turn_id: "successor-turn".into(), generation: 2 }, revision: 1 };
        env.app.api_bot_runtime.inner.sessions.lock().unwrap().insert("r2-bot".into(), successor.clone());
        release.notify_one();
        assert!(matches!(tokio::time::timeout(DEADLOCK_TIMEOUT, job).await.unwrap().unwrap(), Err(ApiFreshError::Unconfirmed)));
        assert!(env.app.api_bot_runtime.inner.sessions.lock().unwrap().get("r2-bot") == Some(&successor));
        let key = ApiTurnKey { bot_id: "r2-bot".into(), run_id: "successor-run".into(), turn_id: "successor-turn".into() };
        assert_eq!(env.app.api_turn_spawn(&key).await, Err(ApiBotError::Unconfirmed));
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let other = ApiTurnKey { bot_id: bot, run_id: run, turn_id: turn };
        assert!(env.app.api_turn_spawn(&other).await.is_ok());
        wait_until_completed(&env.app, &other).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        env.app.background_tasks.close(); tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn r2_cleanup_waits_for_actual_owner_exit() {
        let env = crate::testing::env().await;
        let (endpoint, shutdown, server) = fake_server(Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let entered_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_same_run_owner_before_exit", &run, move || async move { entered_hook.notify_one(); release_hook.notified().await; });
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: turn };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        cleanup_terminal_controls(&env.app.db, &env.app.api_bot_runtime.inner).await;
        assert!(env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|record| !record.owner_exited));
        let run_key = ApiRunKey { bot_id: bot, run_id: run };
        assert!(matches!(env.app.api_bot_runtime.retention_guard(&env.app, &run_key).await, Err(ApiBotError::Busy)));
        env.app.background_tasks.close(); release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        assert!(env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|record| record.owner_exited));
        cleanup_terminal_controls(&env.app.db, &env.app.api_bot_runtime.inner).await;
        assert!(!env.app.api_bot_runtime.inner.controls.lock().unwrap().contains_key(&FullKey::from(&key)));
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn r2_owner_exit_panic_shutdown_and_identity_fence() {
        for mode in ["panic", "shutdown", "aba"] {
            let env = crate::testing::env().await;
            let (endpoint, shutdown, server) = fake_server(Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new())).await;
            enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
            let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
            insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
            let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
            let entered_hook = entered.clone(); let release_hook = release.clone();
            let point = if mode == "aba" { "api_same_run_proof_before_cas" } else { "api_same_run_owner_before_exit" };
            crate::lifecycle::race_point::arm(point, &run, move || async move {
                entered_hook.notify_one(); release_hook.notified().await;
                if mode == "panic" { panic!("synthetic owner panic"); }
            });
            let key = ApiTurnKey { bot_id: bot, run_id: run, turn_id: turn };
            env.app.api_turn_spawn(&key).await.unwrap();
            tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
            {
                let mut controls = env.app.api_bot_runtime.inner.controls.lock().unwrap();
                let record = controls.get_mut(&FullKey::from(&key)).unwrap();
                assert!(!record.owner_exited);
                if mode == "aba" { record.identity = Arc::new(()); }
            }
            if mode == "shutdown" { env.app.shutdown.cancel(); }
            env.app.background_tasks.close(); release.notify_one();
            tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
            let controls = env.app.api_bot_runtime.inner.controls.lock().unwrap();
            let record = controls.get(&FullKey::from(&key)).unwrap();
            assert_eq!(record.owner_exited, mode != "aba");
            assert!(record.finalized.is_none());
            drop(controls);
            shutdown.cancel(); server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn r2_bot_gate_blocks_spawn_and_stop_until_release() {
        let env = crate::testing::env().await;
        let (endpoint, shutdown, server) = fake_server(Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let gate = env.app.api_bot_runtime.bot_gate(&bot).await;
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: turn };
        let run_key = ApiRunKey { bot_id: bot, run_id: run.clone() };
        let mut spawn = Box::pin(env.app.api_turn_spawn(&key));
        let mut stop = Box::pin(env.app.api_run_begin_stop(&run_key, ApiBeginStopMode::User { withdraw: true }));
        assert!(futures::poll!(spawn.as_mut()).is_pending());
        assert!(futures::poll!(stop.as_mut()).is_pending());
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "running");
        drop(spawn);
        drop(gate);
        assert!(matches!(tokio::time::timeout(DEADLOCK_TIMEOUT, stop).await.unwrap(), Ok(ApiBeginStopResult::Applied { .. })));
        assert!(env.app.api_turn_spawn(&key).await.is_err());
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn r2_scanner_serializes_cross_run_spawn_and_stop() {
        for operation in ["spawn", "stop", "idle", "cancel", "proof"] {
            let env = crate::testing::env().await;
            let calls = Arc::new(AtomicUsize::new(0));
            let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
            enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
            let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
            insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
            if operation == "idle" || operation == "proof" {
                sqlx::query("UPDATE turns SET status='failed' WHERE id=?").bind(&turn).execute(&env.app.db).await.unwrap();
                sqlx::query("UPDATE runs SET agent_status='idle',state=? WHERE id=?").bind(if operation == "proof" { "stopping" } else { "running" }).bind(&run).execute(&env.app.db).await.unwrap();
            }
            let old_run = crate::db::ulid(); let old_turn = crate::db::ulid(); let session = crate::db::ulid();
            let now = crate::db::now();
            sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'exited',?)").bind(&old_run).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
            sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','failed','ok',?)").bind(&old_turn).bind(&conv).bind(&old_run).bind(&now).execute(&env.app.db).await.unwrap();
            sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,1,1,'{}',?,?,?)").bind(&session).bind(&bot).bind(&old_run).bind(&old_turn).bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
            sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,state,created_at,processed_at) VALUES(?,?,?,?,1,1,'failed','applied',?,?)").bind(&session).bind(&bot).bind(&old_run).bind(&old_turn).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
            let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
            let entered_hook = entered.clone(); let release_hook = release.clone();
            crate::lifecycle::race_point::arm("api_retention_before_tx", &old_run, move || async move { entered_hook.notify_one(); release_hook.notified().await; });
            let scan_app = env.app.clone();
            let scan = tokio::spawn(async move { super::super::retention::scan_once(&scan_app).await });
            tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
            let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: turn };
            let run_key = ApiRunKey { bot_id: bot, run_id: run.clone() };
            if operation == "spawn" {
                let mut spawn = Box::pin(env.app.api_turn_spawn(&key));
                assert!(futures::poll!(spawn.as_mut()).is_pending());
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                release.notify_one();
                tokio::time::timeout(DEADLOCK_TIMEOUT, scan).await.unwrap().unwrap().unwrap();
                let _ = tokio::time::timeout(DEADLOCK_TIMEOUT, spawn).await.unwrap();
            } else if operation == "cancel" {
                let mut cancel = Box::pin(env.app.api_run_cancel_and_join(&key));
                assert!(futures::poll!(cancel.as_mut()).is_pending());
                release.notify_one();
                tokio::time::timeout(DEADLOCK_TIMEOUT, scan).await.unwrap().unwrap().unwrap();
                assert_eq!(tokio::time::timeout(DEADLOCK_TIMEOUT, cancel).await.unwrap(), Err(ApiBotError::Unconfirmed));
            } else if operation == "proof" {
                let mut proof = Box::pin(env.app.api_run_stop_proof(&run_key));
                assert!(futures::poll!(proof.as_mut()).is_pending());
                release.notify_one();
                tokio::time::timeout(DEADLOCK_TIMEOUT, scan).await.unwrap().unwrap().unwrap();
                assert!(tokio::time::timeout(DEADLOCK_TIMEOUT, proof).await.unwrap().is_ok());
            } else {
                let mode = if operation == "idle" { ApiBeginStopMode::Idle } else { ApiBeginStopMode::User { withdraw: true } };
                let mut stop = Box::pin(env.app.api_run_begin_stop(&run_key, mode));
                assert!(futures::poll!(stop.as_mut()).is_pending());
                release.notify_one();
                tokio::time::timeout(DEADLOCK_TIMEOUT, scan).await.unwrap().unwrap().unwrap();
                assert!(matches!(tokio::time::timeout(DEADLOCK_TIMEOUT, stop).await.unwrap(), Ok(ApiBeginStopResult::Applied { .. })));
            }
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap();
            assert_eq!(count, 1, "active successor must prevent retention");
            shutdown.cancel(); server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn r2_stop_transaction_blocks_actual_scanner_until_commit() {
        let env = crate::testing::env().await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let old_run = crate::db::ulid(); let old_turn = crate::db::ulid(); let session = crate::db::ulid(); let now = crate::db::now();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'exited',?)").bind(&old_run).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','failed','ok',?)").bind(&old_turn).bind(&conv).bind(&old_run).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,?,?,1,1,'{}',?,?,?)").bind(&session).bind(&bot).bind(&old_run).bind(&old_turn).bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,state,created_at,processed_at) VALUES(?,?,?,?,1,1,'failed','applied',?,?)").bind(&session).bind(&bot).bind(&old_run).bind(&old_turn).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let entered_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_begin_stop_before_commit", &run, move || async move { entered_hook.notify_one(); release_hook.notified().await; });
        let key_for_test_bot = bot.clone();
        let key = ApiRunKey { bot_id: bot, run_id: run };
        let app = env.app.clone();
        let stop = tokio::spawn(async move { app.api_run_begin_stop(&key, ApiBeginStopMode::User { withdraw: true }).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        let lifecycle_lock = crate::capabilities::BotLocks::bot_lock(env.app.as_ref(), &key_for_test_bot).await;
        let scan_app = env.app.clone();
        let scan = tokio::spawn(async move { super::super::retention::scan_once(&scan_app).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while lifecycle_lock.try_lock().is_ok() { tokio::task::yield_now().await; }
        }).await.unwrap();
        assert!(!scan.is_finished(), "scanner owns lifecycle lock but must wait for stop's bot gate");
        release.notify_one();
        assert!(matches!(tokio::time::timeout(DEADLOCK_TIMEOUT, stop).await.unwrap().unwrap(), Ok(ApiBeginStopResult::Applied { .. })));
        tokio::time::timeout(DEADLOCK_TIMEOUT, scan).await.unwrap().unwrap().unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id=?").bind(&session).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn r2_spawn_holds_bot_gate_until_runner_is_installed() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_run_spawn_after_owner_writes", &run, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: turn };
        let spawn_app = env.app.clone(); let spawn_key = clone_turn_key(&key);
        let spawn = tokio::spawn(async move { spawn_app.api_turn_spawn(&spawn_key).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET expires_at=? WHERE bot_id=?").bind(crate::db::iso_in(-60)).bind(&bot).execute(&env.app.db).await.unwrap();
        let lock = crate::capabilities::BotLocks::bot_lock(env.app.as_ref(), &bot).await;
        let scan_app = env.app.clone();
        let scan = tokio::spawn(async move { super::super::retention::scan_once(&scan_app).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { while lock.try_lock().is_ok() { tokio::task::yield_now().await; } }).await.unwrap();
        assert!(!scan.is_finished());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        release.notify_one();
        assert!(tokio::time::timeout(DEADLOCK_TIMEOUT, spawn).await.unwrap().unwrap().is_ok());
        tokio::time::timeout(DEADLOCK_TIMEOUT, scan).await.unwrap().unwrap().unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE bot_id=?").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 1);
        env.app.background_tasks.close();
        tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn r2_cancel_true_join_allows_scanner_to_observe_live_owner() {
        let env = crate::testing::env().await;
        let (endpoint, shutdown, server) = fake_server(Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_run_runner_before_entry", &run, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: turn };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        sqlx::query("UPDATE api_bot_sessions SET expires_at=? WHERE bot_id=?").bind(crate::db::iso_in(-60)).bind(&bot).execute(&env.app.db).await.unwrap();
        let run_key = ApiRunKey { bot_id: bot.clone(), run_id: run };
        assert!(matches!(env.app.api_run_begin_stop(&run_key, ApiBeginStopMode::User { withdraw: true }).await, Ok(ApiBeginStopResult::Applied { .. })));
        let cancellation = env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).unwrap().cancel.clone();
        let cancel_app = env.app.clone(); let cancel_key = clone_turn_key(&key);
        let cancel = tokio::spawn(async move { cancel_app.api_run_cancel_and_join(&cancel_key).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, cancellation.cancelled()).await.unwrap();
        assert!(!cancel.is_finished(), "cancel waits for the actual runner join");
        tokio::time::timeout(DEADLOCK_TIMEOUT, super::super::retention::scan_once(&env.app)).await.unwrap().unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE bot_id=?").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 1);
        release.notify_one();
        assert_eq!(tokio::time::timeout(DEADLOCK_TIMEOUT, cancel).await.unwrap().unwrap(), Ok(ApiRunCompletion::Cancelled));
        env.app.background_tasks.close();
        tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn r2_shutdown_timeout_retains_live_registry_and_control() {
        let env = crate::testing::env().await;
        let (endpoint, shutdown, server) = fake_server(Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, Duration::from_millis(5)).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let enter_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_run_runner_before_entry", &run, move || async move { enter_hook.notify_one(); release_hook.notified().await; });
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: turn };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        env.app.shutdown.cancel(); env.app.background_tasks.close();
        tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        assert!(env.app.api_bot_runtime.inner.registry.is_active(&run));
        assert!(env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|record| record.owner_exited && matches!(*record.joined.borrow(), JoinState::TimedOut) && record.finalized.is_none()));
        let run_key = ApiRunKey { bot_id: bot, run_id: run.clone() };
        assert!(matches!(env.app.api_bot_runtime.retention_guard(&env.app, &run_key).await, Err(ApiBotError::Busy)));
        release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { while env.app.api_bot_runtime.inner.registry.is_active(&run) { tokio::task::yield_now().await; } }).await.unwrap();
        shutdown.cancel(); server.await.unwrap();
    }

    #[test]
    fn r2_owner_and_scanner_interactions_fit_one_mib_stack() {
        std::thread::Builder::new().stack_size(1024 * 1024).spawn(|| {
            r2_cleanup_waits_for_actual_owner_exit();
            r2_cancel_true_join_allows_scanner_to_observe_live_owner();
            r2_scanner_serializes_cross_run_spawn_and_stop();
            r2_stop_transaction_blocks_actual_scanner_until_commit();
        }).unwrap().join().unwrap();
    }

    #[tokio::test]
    async fn same_run_stop_shutdown_while_waiting_for_gate_is_busy() {
        let env = crate::testing::env().await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let key = ApiRunKey { bot_id: bot, run_id: run.clone() };
        let guard = env.app.api_bot_runtime.admission_gate(&key).await;
        let mut pending = Box::pin(env.app.api_run_begin_stop(&key, ApiBeginStopMode::User { withdraw: true }));
        assert!(futures::poll!(pending.as_mut()).is_pending(), "stop must actually wait on admission");
        env.app.shutdown.cancel();
        let result = tokio::time::timeout(DEADLOCK_TIMEOUT, pending).await.unwrap();
        assert!(matches!(result, Err(ApiBeginStopError::Busy)));
        drop(guard);
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "running");
    }

    #[tokio::test]
    async fn same_run_shutdown_prevents_finalized_proof_on_both_owner_boundaries() {
        for point in ["api_same_run_owner_before_exit", "api_same_run_proof_before_cas"] {
            let env = crate::testing::env().await;
            let (endpoint, server_shutdown, server_task) = fake_server(Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new())).await;
            enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
            let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
            insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
            let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
            let entered_hook = entered.clone(); let release_hook = release.clone();
            crate::lifecycle::race_point::arm(point, &run, move || async move { entered_hook.notify_one(); release_hook.notified().await; });
            let key = ApiTurnKey { bot_id: bot, run_id: run, turn_id: turn };
            env.app.api_turn_spawn(&key).await.unwrap();
            tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
            env.app.shutdown.cancel();
            env.app.background_tasks.close();
            release.notify_one();
            tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
            assert!(env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.finalized.is_none()));
            server_shutdown.cancel(); server_task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn same_run_owner_panic_never_publishes_finalized_proof() {
        let env = crate::testing::env().await;
        let (endpoint, shutdown, server) = fake_server(Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let reached = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let reached_hook = reached.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_same_run_owner_before_exit", &run, move || async move { reached_hook.notify_one(); release_hook.notified().await; panic!("synthetic owner failure"); });
        let key = ApiTurnKey { bot_id: bot, run_id: run, turn_id: turn };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, reached.notified()).await.unwrap();
        env.app.background_tasks.close(); release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.background_tasks.wait()).await.unwrap();
        assert!(env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.finalized.is_none()));
        shutdown.cancel(); server.await.unwrap();
    }

    #[test]
    fn same_run_success_and_blocked_owner_fit_one_mib_stack() {
        std::thread::Builder::new().stack_size(1024 * 1024).spawn(|| {
            same_run_second_turn_continues_after_joined_completion();
            same_run_joined_owner_must_exit_before_next_admission();
            completion_backoff_releases_all_locks_and_stop_wins_without_resurrection();
        }).unwrap().join().unwrap();
    }

    #[tokio::test]
    async fn same_run_noncompleted_prior_is_busy_without_dispatch() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let next = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: turn };
        let old = FullKey { bot_id: bot, run_id: run, turn_id: crate::db::ulid() };
        for state in [JoinState::Joined(Ok(Completion::Failed)), JoinState::Joined(Ok(Completion::Cancelled)), JoinState::Joined(Err(JoinError::Unconfirmed)), JoinState::Pending] {
            let (_tx, joined) = watch::channel(state);
            env.app.api_bot_runtime.inner.controls.lock().unwrap().insert(old.clone(), ControlRecord { identity: Arc::new(()), cancel: CancellationToken::new(), joined, finalized: None, owner_exited: false });
            assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions").fetch_one(&env.app.db).await.unwrap();
            assert_eq!(sessions, 0);
        }
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn same_run_control_aba_after_rebind_stays_busy_without_replay() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let first = crate::db::ulid(); let second = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &first, &conv, &env.project_id, "fixture first").await;
        let old = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: first };
        env.app.api_turn_spawn(&old).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { while !env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&old)).is_some_and(|r| r.finalized.is_some()) { tokio::task::yield_now().await; } }).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES(?,?,?,'web','in_flight','ok','fixture second',?)").bind(&second).bind(&conv).bind(&run).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let entered_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_same_run_before_control_cas", &run, move || async move { entered_hook.notify_one(); release_hook.notified().await; });
        let next = ApiTurnKey { bot_id: bot, run_id: run, turn_id: second };
        let task_app = env.app.clone(); let task_key = clone_turn_key(&next);
        let task = tokio::spawn(async move { task_app.api_turn_spawn(&task_key).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        env.app.api_bot_runtime.inner.controls.lock().unwrap().get_mut(&FullKey::from(&old)).unwrap().identity = Arc::new(());
        release.notify_one();
        assert_eq!(tokio::time::timeout(DEADLOCK_TIMEOUT, task).await.unwrap().unwrap(), Err(ApiBotError::Busy));
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(env.app.api_bot_runtime.inner.controls.lock().unwrap().contains_key(&FullKey::from(&old)));
        let owner: String = sqlx::query_scalar("SELECT owner_turn_id FROM api_bot_sessions WHERE bot_id=?").bind(&next.bot_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(owner, next.turn_id);
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn same_run_stop_cas_wins_after_first_finalized_before_second_spawn() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0)); let tool_calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let mut deps = env.app.api_bot_runtime.test_dependencies().unwrap();
        deps.tools = Arc::new(CountingTools(tool_calls.clone())); env.app.api_bot_runtime.set_test_dependencies(deps);
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let first = crate::db::ulid(); let second = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &first, &conv, &env.project_id, "fixture first").await;
        let old = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: first.clone() };
        env.app.api_turn_spawn(&old).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { while !env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&old)).is_some_and(|r| r.finalized.is_some()) { tokio::task::yield_now().await; } }).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES(?,?,?,'web','in_flight','ok','fixture second',?)").bind(&second).bind(&conv).bind(&run).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let tag = env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&old)).unwrap().identity.clone();
        let cursor = env.app.api_bot_runtime.inner.sessions.lock().unwrap().get(&bot).cloned().unwrap();
        let run_key = ApiRunKey { bot_id: bot.clone(), run_id: run.clone() };
        assert!(matches!(env.app.api_run_begin_stop(&run_key, ApiBeginStopMode::User { withdraw: false }).await, Ok(ApiBeginStopResult::Applied { .. })));
        let next = ApiTurnKey { bot_id: bot.clone(), run_id: run, turn_id: second };
        assert!(env.app.api_turn_spawn(&next).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1); assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        let controls = env.app.api_bot_runtime.inner.controls.lock().unwrap();
        assert!(controls.get(&FullKey::from(&old)).is_some_and(|r| Arc::ptr_eq(&r.identity, &tag) && r.finalized.is_some()));
        assert!(!controls.contains_key(&FullKey::from(&next))); drop(controls);
        assert!(env.app.api_bot_runtime.inner.sessions.lock().unwrap().get(&bot) == Some(&cursor));
        let owner: String = sqlx::query_scalar("SELECT owner_turn_id FROM api_bot_sessions WHERE bot_id=?").bind(&bot).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(owner, first);
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn same_run_stop_cas_first_blocks_spawn_and_http() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let turn = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &turn, &conv, &env.project_id, "fixture").await;
        let run_key = ApiRunKey { bot_id: bot.clone(), run_id: run.clone() };
        assert!(matches!(env.app.api_run_begin_stop(&run_key, ApiBeginStopMode::User { withdraw: false }).await, Ok(ApiBeginStopResult::Applied { .. })));
        let key = ApiTurnKey { bot_id: bot, run_id: run, turn_id: turn };
        assert!(env.app.api_turn_spawn(&key).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE bot_id=?").bind(&key.bot_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(sessions, 0);
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn same_run_stop_waits_for_rebind_handover_before_cas() {
        for point in ["api_run_spawn_after_owner_writes", "api_same_run_before_control_cas", "api_run_spawn_after_registry"] {
            same_run_stop_handover_case(point).await;
        }
    }

    async fn same_run_stop_handover_case(point: &'static str) {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, shutdown, server) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let first = crate::db::ulid(); let second = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &first, &conv, &env.project_id, "fixture first").await;
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: first };
        env.app.api_turn_spawn(&key).await.unwrap();
        wait_until_completed(&env.app, &key).await;
        tokio::time::timeout(DEADLOCK_TIMEOUT, async { while !env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.finalized.is_some()) { tokio::task::yield_now().await; } }).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES(?,?,?,'web','in_flight','ok','fixture second',?)").bind(&second).bind(&conv).bind(&run).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let reached = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let reached_hook = reached.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm(point, &run, move || async move { reached_hook.notify_one(); release_hook.notified().await; });
        let next = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: second };
        let runner_reached = Arc::new(Notify::new()); let runner_release = Arc::new(Notify::new());
        let runner_reached_hook = runner_reached.clone(); let runner_release_hook = runner_release.clone();
        crate::lifecycle::race_point::arm("api_run_runner_before_entry", &run, move || async move { runner_reached_hook.notify_one(); runner_release_hook.notified().await; });
        let spawn_app = env.app.clone();
        let next_spawn = clone_turn_key(&next);
        let spawn = tokio::spawn(async move { spawn_app.api_turn_spawn(&next_spawn).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, reached.notified()).await.unwrap();
        let old_retained = env.app.api_bot_runtime.inner.controls.lock().unwrap().contains_key(&FullKey::from(&key));
        assert_eq!(old_retained, point != "api_run_spawn_after_registry");
        let stop_key = ApiRunKey { bot_id: bot, run_id: run.clone() };
        let mut stop = Box::pin(env.app.api_run_begin_stop(&stop_key, ApiBeginStopMode::User { withdraw: false }));
        assert!(futures::poll!(stop.as_mut()).is_pending());
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "running");
        release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, spawn).await.unwrap().unwrap().unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, runner_reached.notified()).await.unwrap();
        assert!(matches!(tokio::time::timeout(DEADLOCK_TIMEOUT, stop).await.unwrap(), Ok(ApiBeginStopResult::Applied { .. })));
        let cancellation = env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&next)).unwrap().cancel.clone();
        let cancel_app = env.app.clone(); let cancel_key = clone_turn_key(&next);
        let cancel = tokio::spawn(async move { cancel_app.api_run_cancel_and_join(&cancel_key).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, cancellation.cancelled()).await.unwrap();
        runner_release.notify_one();
        assert_eq!(tokio::time::timeout(DEADLOCK_TIMEOUT, cancel).await.unwrap().unwrap(), Ok(ApiRunCompletion::Cancelled));
        shutdown.cancel(); server.await.unwrap();
    }

    #[tokio::test]
    async fn same_run_joined_owner_must_exit_before_next_admission() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, server_shutdown, server_task) = fake_server(calls.clone(), Arc::new(Notify::new())).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot = crate::db::ulid(); let run = crate::db::ulid(); let first = crate::db::ulid(); let second = crate::db::ulid(); let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot, &run, &first, &conv, &env.project_id, "fixture first").await;
        let entered = Arc::new(Notify::new()); let release = Arc::new(Notify::new());
        let entered_hook = entered.clone(); let release_hook = release.clone();
        crate::lifecycle::race_point::arm("api_same_run_owner_before_exit", &run, move || async move { entered_hook.notify_one(); release_hook.notified().await; });
        let key = ApiTurnKey { bot_id: bot.clone(), run_id: run.clone(), turn_id: first.clone() };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES(?,?,?,'web','in_flight','ok','fixture second',?)").bind(&second).bind(&conv).bind(&run).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let next = ApiTurnKey { bot_id: bot, run_id: run, turn_id: second };
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while !env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.finalized.is_some()) { tokio::task::yield_now().await; }
        }).await.unwrap();
        env.app.api_turn_spawn(&next).await.unwrap();
        wait_until_completed(&env.app, &next).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server_shutdown.cancel(); server_task.await.unwrap();
    }

    #[tokio::test]
    async fn same_run_second_turn_continues_after_joined_completion() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, server_shutdown, server_task) = fake_server(calls.clone(), hit).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let mut deps = env.app.api_bot_runtime.test_dependencies().unwrap();
        deps.tools = Arc::new(CountingTools(tool_calls.clone()));
        env.app.api_bot_runtime.set_test_dependencies(deps);
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let first = crate::db::ulid();
        let second = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot_id, &run_id, &first, &conv, &env.project_id, "first fixture turn").await;
        let key = ApiTurnKey { bot_id: bot_id.clone(), run_id: run_id.clone(), turn_id: first.clone() };
        env.app.api_turn_spawn(&key).await.unwrap();
        wait_until_completed(&env.app, &key).await;
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            loop {
                let cursor = env.app.api_bot_runtime.inner.sessions.lock().unwrap().get(&bot_id).cloned();
                let finalized = env.app.api_bot_runtime.inner.controls.lock().unwrap().get(&FullKey::from(&key)).is_some_and(|r| r.finalized.is_some());
                if cursor.is_some_and(|c| c.identity.turn_id == first) && finalized { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES(?,?,?,'web','in_flight','ok','second fixture turn',?)")
            .bind(&second).bind(&conv).bind(&run_id).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let next = ApiTurnKey { bot_id, run_id, turn_id: second };
        let original: String = sqlx::query_scalar("SELECT final_text FROM api_bot_join_receipts WHERE turn_id=?").bind(&first).fetch_one(&env.app.db).await.unwrap();
        sqlx::query("UPDATE api_bot_join_receipts SET final_text='altered' WHERE turn_id=?").bind(&first).execute(&env.app.db).await.unwrap();
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        sqlx::query("UPDATE api_bot_join_receipts SET final_text=? WHERE turn_id=?").bind(original).bind(&first).execute(&env.app.db).await.unwrap();
        for state in ["pending", "fenced"] {
            sqlx::query("UPDATE api_bot_join_receipts SET state=?,processed_at=CASE WHEN ?='pending' THEN NULL ELSE ? END WHERE turn_id=?").bind(state).bind(state).bind(crate::db::now()).bind(&first).execute(&env.app.db).await.unwrap();
            assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        sqlx::query("UPDATE api_bot_join_receipts SET state='applied',processed_at=? WHERE turn_id=?").bind(crate::db::now()).bind(&first).execute(&env.app.db).await.unwrap();
        sqlx::query("ALTER TABLE api_bot_join_receipts RENAME TO fixture_hidden_receipts").execute(&env.app.db).await.unwrap();
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        sqlx::query("ALTER TABLE fixture_hidden_receipts RENAME TO api_bot_join_receipts").execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TABLE fixture_receipt AS SELECT * FROM api_bot_join_receipts WHERE turn_id=?").bind(&first).execute(&env.app.db).await.unwrap();
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE turn_id=?").bind(&first).execute(&env.app.db).await.unwrap();
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        sqlx::query("INSERT INTO api_bot_join_receipts SELECT * FROM fixture_receipt").execute(&env.app.db).await.unwrap();
        let duplicate = crate::db::ulid();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,incomplete,created_at) VALUES(?,?,?,'assistant','fixture duplicate','transcript',0,?)")
            .bind(&duplicate).bind(&conv).bind(&first).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        sqlx::query("DELETE FROM messages WHERE id=?").bind(&duplicate).execute(&env.app.db).await.unwrap();
        let original_checkpoint: String = sqlx::query_scalar("SELECT checkpoint_json FROM api_bot_sessions WHERE bot_id=?").bind(&next.bot_id).fetch_one(&env.app.db).await.unwrap();
        let mut pending: serde_json::Value = serde_json::from_str(&original_checkpoint).unwrap();
        pending["api_pending"] = true.into();
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=? WHERE bot_id=?").bind(pending.to_string()).bind(&next.bot_id).execute(&env.app.db).await.unwrap();
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=? WHERE bot_id=?").bind(&original_checkpoint).bind(&next.bot_id).execute(&env.app.db).await.unwrap();
        let mut tool_pending: serde_json::Value = serde_json::from_str(&original_checkpoint).unwrap();
        tool_pending["tool_pending"] = "fixture-pending".into();
        let tool_checkpoint = tool_pending.to_string();
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=? WHERE bot_id=?").bind(&tool_checkpoint).bind(&next.bot_id).execute(&env.app.db).await.unwrap();
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        let still_pending: String = sqlx::query_scalar("SELECT checkpoint_json FROM api_bot_sessions WHERE bot_id=?").bind(&next.bot_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(still_pending, tool_checkpoint);
        assert_eq!(calls.load(Ordering::SeqCst), 1); assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=? WHERE bot_id=?").bind(original_checkpoint).bind(&next.bot_id).execute(&env.app.db).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        let old_cursor = env.app.api_bot_runtime.inner.sessions.lock().unwrap().get(&next.bot_id).cloned().unwrap();
        env.app.api_bot_runtime.inner.sessions.lock().unwrap().get_mut(&next.bot_id).unwrap().revision += 1;
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        env.app.api_bot_runtime.inner.sessions.lock().unwrap().insert(next.bot_id.clone(), old_cursor.clone());
        sqlx::query("UPDATE turns SET delivery='pending' WHERE id=?").bind(&next.turn_id).execute(&env.app.db).await.unwrap();
        assert_eq!(env.app.api_turn_spawn(&next).await, Err(ApiBotError::Busy));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id=?").bind(&next.turn_id).execute(&env.app.db).await.unwrap();
        let gate = Arc::new(tokio::sync::Barrier::new(3));
        let app_a = env.app.clone(); let key_a = clone_turn_key(&next); let gate_a = gate.clone();
        let app_b = env.app.clone(); let key_b = clone_turn_key(&next); let gate_b = gate.clone();
        let a = tokio::spawn(async move { gate_a.wait().await; app_a.api_turn_spawn(&key_a).await });
        let b = tokio::spawn(async move { gate_b.wait().await; app_b.api_turn_spawn(&key_b).await });
        gate.wait().await;
        let (a, b) = tokio::time::timeout(DEADLOCK_TIMEOUT, async { (a.await.unwrap(), b.await.unwrap()) }).await.unwrap();
        assert!(matches!((a, b), (Ok(()), Err(ApiBotError::Busy)) | (Err(ApiBotError::Busy), Ok(()))));
        wait_until_completed(&env.app, &next).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
        let records: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM api_bot_sessions WHERE bot_id=?),(SELECT count(*) FROM api_bot_join_receipts WHERE bot_id=? AND state='applied'),(SELECT count(*) FROM messages WHERE turn_id IN (?,?) AND role='assistant')")
            .bind(&next.bot_id).bind(&next.bot_id).bind(&first).bind(&next.turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(records, (1, 2, 2));
        let (session, generation, revision, checkpoint): (String, i64, i64, String) = sqlx::query_as("SELECT id,generation,revision,checkpoint_json FROM api_bot_sessions WHERE bot_id=?").bind(&next.bot_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(session, old_cursor.identity.session_id);
        assert_eq!(generation, old_cursor.identity.generation);
        assert!(revision > old_cursor.revision);
        let checkpoint: serde_json::Value = serde_json::from_str(&checkpoint).unwrap();
        let messages = checkpoint["messages"].as_array().unwrap();
        let first_pos = messages.iter().position(|m| m["content"] == "first fixture turn").unwrap();
        let second_pos = messages.iter().position(|m| m["content"] == "second fixture turn").unwrap();
        assert!(first_pos < second_pos);
        server_shutdown.cancel();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn fake_app_port_persists_first_turn_rebinds_and_prunes_terminal_control_records() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, server_shutdown, server_task) =
            fake_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot_id = crate::db::ulid();
        let conv = crate::db::ulid();

        for n in 0..8 {
            let run_id = crate::db::ulid();
            let turn_id = crate::db::ulid();
            insert_bot_and_turn(
                &env.app,
                &bot_id,
                &run_id,
                &turn_id,
                &conv,
                &env.project_id,
                &format!("turn {n}"),
            )
            .await;
            let key = ApiTurnKey {
                bot_id: bot_id.clone(),
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
            };
            if n == 0 {
                let wrong_bot = ApiTurnKey {
                    bot_id: crate::db::ulid(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                };
                let wrong_turn = ApiTurnKey {
                    bot_id: bot_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: crate::db::ulid(),
                };
                let wrong_run = ApiTurnKey {
                    bot_id: bot_id.clone(),
                    run_id: crate::db::ulid(),
                    turn_id: turn_id.clone(),
                };
                assert_eq!(
                    env.app.api_turn_spawn(&wrong_bot).await,
                    Err(ApiBotError::IdentityMismatch)
                );
                assert_eq!(
                    env.app.api_turn_spawn(&wrong_turn).await,
                    Err(ApiBotError::IdentityMismatch)
                );
                assert_eq!(
                    env.app.api_turn_spawn(&wrong_run).await,
                    Err(ApiBotError::IdentityMismatch)
                );
                sqlx::query("UPDATE projects SET host='remote-fixture' WHERE id=?")
                    .bind(&env.project_id)
                    .execute(&env.app.db)
                    .await
                    .unwrap();
                assert_eq!(
                    env.app.api_turn_spawn(&key).await,
                    Err(ApiBotError::IdentityMismatch)
                );
                sqlx::query("UPDATE projects SET host='local' WHERE id=?")
                    .bind(&env.project_id)
                    .execute(&env.app.db)
                    .await
                    .unwrap();
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    0,
                    "bad ownership and remote projects are rejected before HTTP"
                );
            }
            if n == 0 {
                let gate = Arc::new(tokio::sync::Barrier::new(3));
                let app_a = env.app.clone();
                let app_b = env.app.clone();
                let key_a = clone_turn_key(&key);
                let key_b = clone_turn_key(&key);
                let gate_a = gate.clone();
                let gate_b = gate.clone();
                let spawn_a = tokio::spawn(async move {
                    gate_a.wait().await;
                    app_a.api_turn_spawn(&key_a).await
                });
                let spawn_b = tokio::spawn(async move {
                    gate_b.wait().await;
                    app_b.api_turn_spawn(&key_b).await
                });
                gate.wait().await;
                let result_a = tokio::time::timeout(DEADLOCK_TIMEOUT, spawn_a)
                    .await
                    .unwrap()
                    .unwrap();
                let result_b = tokio::time::timeout(DEADLOCK_TIMEOUT, spawn_b)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    matches!(
                        (result_a, result_b),
                        (Ok(()), Err(ApiBotError::Busy)) | (Err(ApiBotError::Busy), Ok(()))
                    ),
                    "simultaneous same-run admission has one owner and one Busy result"
                );
            } else {
                env.app.api_turn_spawn(&key).await.unwrap();
            }
            tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified())
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), n as usize + 1);
            let sessions: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL",
            )
            .bind(&bot_id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
            assert_eq!(sessions, 1, "parallel spawn cannot create a second session");
            wait_until_joined(&env.app, &key).await;
            wait_until_completed(&env.app, &key).await;
            let checkpoint: String = sqlx::query_scalar("SELECT checkpoint_json FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL")
                .bind(&bot_id).fetch_one(&env.app.db).await.unwrap();
            let checkpoint: Value = serde_json::from_str(&checkpoint).unwrap();
            assert!(
                checkpoint["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["content"] == json!(format!("turn {n}"))),
                "every turn's prompt is durable before it is run"
            );
            assert!(crate::lifecycle::stop_bot(&env.app, &bot_id).await.unwrap());
            let terminal: (String, String) = sqlx::query_as(
                "SELECT r.state,t.status FROM runs r JOIN turns t ON t.run_id=r.id WHERE r.id=? AND t.id=?",
            )
            .bind(&run_id)
            .bind(&turn_id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
            assert_eq!(terminal, ("stopped".into(), "completed".into()));
            assert!(env.app.api_bot_runtime.control_count_for_test() <= 1);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            8,
            "initial state and each rebind submit exactly one HTTP request"
        );
        assert!(
            env.app.api_bot_runtime.control_count_for_test() <= 1,
            "terminal control records are reclaimed on the next spawn"
        );
        server_shutdown.cancel();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn stopping_during_held_http_waits_for_join_and_late_response_keeps_reservation_unknown()
    {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, server_shutdown, server_task, late_response) =
            held_response_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "cancel while HTTP is held",
        )
        .await;
        let key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id,
        };

        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified())
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let cas = sqlx::query(
            "UPDATE runs SET state='stopping' WHERE id=? AND bot_id=? AND state='running'",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .execute(&env.app.db)
        .await
        .unwrap();
        assert_eq!(
            cas.rows_affected(),
            1,
            "stop claims the running row with a CAS"
        );

        let joined = tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.api_run_cancel_and_join(&key))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(joined, ApiRunCompletion::Cancelled);
        assert!(
            !env.app.api_run_live(&key).await,
            "return follows the real task join"
        );

        let _ = late_response.send(json!({
            "id":"late-fixture-message",
            "type":"message",
            "role":"assistant",
            "model":"claude-opus-5-5",
            "content":[{"type":"text","text":"late response"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        }));
        server_shutdown.cancel();
        server_task.await.unwrap();

        let attempt: (String, Option<i64>, i64) = sqlx::query_as(
            "SELECT state,estimated_microusd,reserved_microusd FROM api_bot_attempts WHERE bot_id=? AND run_id=?",
        )
        .bind(&bot_id)
        .bind(&run_id)
        .fetch_one(&env.app.db)
        .await
        .unwrap();
        assert_eq!(attempt, ("reserved".into(), None, 100));
    }

    #[tokio::test]
    async fn idle_stop_proof_drains_pre_cas_admission_before_joining_live_runner() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, server_shutdown, server_task, late_response) =
            held_response_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "admission race proof",
        )
        .await;
        let turn_key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        };
        let run_key = ApiRunKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
        };

        let owner_writes_complete = Arc::new(Notify::new());
        let release_owner_writes = Arc::new(Notify::new());
        let owner_writes_complete_hook = owner_writes_complete.clone();
        let release_owner_writes_hook = release_owner_writes.clone();
        crate::lifecycle::race_point::arm(
            "api_run_spawn_after_owner_writes",
            &run_id,
            move || async move {
                owner_writes_complete_hook.notify_one();
                release_owner_writes_hook.notified().await;
            },
        );
        let registry_installed = Arc::new(Notify::new());
        let release_registry = Arc::new(Notify::new());
        let registry_installed_hook = registry_installed.clone();
        let release_registry_hook = release_registry.clone();
        crate::lifecycle::race_point::arm(
            "api_run_spawn_after_registry",
            &run_id,
            move || async move {
                registry_installed_hook.notify_one();
                release_registry_hook.notified().await;
            },
        );
        let spawn_app = env.app.clone();
        let spawn_key = clone_turn_key(&turn_key);
        let spawn = tokio::spawn(async move { spawn_app.api_turn_spawn(&spawn_key).await });
        let spawn_completed_owner_writes = tokio::time::timeout(DEADLOCK_TIMEOUT, owner_writes_complete.notified())
            .await
            .is_ok();

        release_owner_writes.notify_one();
        let registry_was_installed = tokio::time::timeout(DEADLOCK_TIMEOUT, registry_installed.notified())
            .await
            .is_ok();
        let http_request_arrived = tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified())
            .await
            .is_ok();

        let cas = sqlx::query(
            "UPDATE runs SET state='stopping' WHERE id=? AND bot_id=? AND state='running'",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .execute(&env.app.db)
        .await;
        let cas_applied = cas.as_ref().is_ok_and(|result| result.rows_affected() == 1);

        let mut proof = Box::pin(env.app.api_run_stop_proof(&run_key));
        let proof_polled_pending_on_admission = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(proof.as_mut().poll(cx).is_pending())
        })
        .await;

        let observed_http_calls_while_held = calls.load(Ordering::SeqCst);
        let terminal_update = sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(crate::db::now())
            .bind(&turn_id)
            .execute(&env.app.db)
            .await;
        release_registry.notify_one();
        let mut spawn = spawn;
        let spawn_result = tokio::time::timeout(DEADLOCK_TIMEOUT, &mut spawn).await;
        let proof_result = tokio::time::timeout(DEADLOCK_TIMEOUT, proof).await;
        let later_spawn = env.app.api_turn_spawn(&turn_key).await;

        let _ = late_response.send(json!({
            "id":"late-idle-proof-fixture",
            "type":"message",
            "role":"assistant",
            "model":"claude-opus-5-5",
            "content":[{"type":"text","text":"late fixture response"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        }));
        let spawn_finished_after_release = if spawn_result.is_err() {
            tokio::time::timeout(DEADLOCK_TIMEOUT, &mut spawn)
                .await
                .is_ok()
        } else {
            true
        };
        let runner_joined_after_release = tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while env.app.api_run_live(&turn_key).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        server_shutdown.cancel();
        let server_result = server_task.await;
        let attempt: Result<(String, Option<i64>, i64), sqlx::Error> = sqlx::query_as(
            "SELECT state,estimated_microusd,reserved_microusd FROM api_bot_attempts WHERE bot_id=? AND run_id=?",
        )
        .bind(&bot_id)
        .bind(&run_id)
        .fetch_one(&env.app.db)
        .await;

        assert!(spawn_completed_owner_writes, "spawn passed validation and completed all DB owner writes before the stop CAS");
        assert!(cas_applied, "the test stop CAS moved the run to stopping");
        assert!(proof_polled_pending_on_admission, "the proof was polled to Pending on the spawn-held admission gate");
        assert!(registry_was_installed, "spawn installed its ticket and control before releasing admission");
        assert!(http_request_arrived);
        assert_eq!(observed_http_calls_while_held, 1);
        assert_eq!(terminal_update.unwrap().rows_affected(), 1);
        assert_eq!(spawn_result.unwrap().unwrap(), Ok(()));
        assert!(spawn_finished_after_release);
        let proof = proof_result.unwrap().unwrap();
        assert_eq!(proof.key, run_key);
        assert_eq!(
            proof.evidence,
            ApiRunStopEvidence::Joined {
                turn_id,
                completion: ApiRunCompletion::Cancelled,
            },
            "the admitted runner is cancelled and truly joined before the proof is returned"
        );
        assert!(runner_joined_after_release, "cleanup reaches the actual runner join before asserting");
        assert_eq!(later_spawn, Err(ApiBotError::Busy));
        assert!(server_result.is_ok());
        assert_eq!(attempt.unwrap(), ("reserved".into(), None, 100));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_stop_proof_drains_spawn_paused_after_owner_writes_before_registry() {
        let env = crate::testing::env().await;
        enable_fake_runtime(&env.app, "http://127.0.0.1:1".into(), DEADLOCK_TIMEOUT).await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "owner writes completed before stop CAS, registry ticket not installed yet",
        )
        .await;
        let turn_key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        };
        let run_key = ApiRunKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
        };

        let owner_writes_complete = Arc::new(Notify::new());
        let release_owner_writes = Arc::new(Notify::new());
        let owner_writes_complete_hook = owner_writes_complete.clone();
        let release_owner_writes_hook = release_owner_writes.clone();
        crate::lifecycle::race_point::arm(
            "api_run_spawn_after_owner_writes",
            &run_id,
            move || async move {
                owner_writes_complete_hook.notify_one();
                release_owner_writes_hook.notified().await;
            },
        );
        let registry_installed = Arc::new(Notify::new());
        let release_registry = Arc::new(Notify::new());
        let registry_installed_hook = registry_installed.clone();
        let release_registry_hook = release_registry.clone();
        crate::lifecycle::race_point::arm(
            "api_run_spawn_after_registry",
            &run_id,
            move || async move {
                registry_installed_hook.notify_one();
                release_registry_hook.notified().await;
            },
        );
        let runner_entered = Arc::new(Notify::new());
        let release_runner = Arc::new(Notify::new());
        let runner_entered_hook = runner_entered.clone();
        let release_runner_hook = release_runner.clone();
        crate::lifecycle::race_point::arm(
            "api_run_runner_before_entry",
            &run_id,
            move || async move {
                runner_entered_hook.notify_one();
                release_runner_hook.notified().await;
            },
        );

        let spawn_app = env.app.clone();
        let spawn_key = clone_turn_key(&turn_key);
        let spawn = tokio::spawn(async move { spawn_app.api_turn_spawn(&spawn_key).await });
        let spawn_completed_owner_writes =
            tokio::time::timeout(DEADLOCK_TIMEOUT, owner_writes_complete.notified())
                .await
                .is_ok();
        let registry_empty_before_cas = !env.app.api_bot_runtime.inner.registry.is_active(&run_id);
        let control_absent_before_cas = !env
            .app
            .api_bot_runtime
            .inner
            .controls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&FullKey::from(&turn_key));
        let cas = sqlx::query(
            "UPDATE runs SET state='stopping' WHERE id=? AND bot_id=? AND state='running'",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .execute(&env.app.db)
        .await;
        let cas_applied = cas.as_ref().is_ok_and(|result| result.rows_affected() == 1);

        let stop_at_admission = Arc::new(Notify::new());
        let stop_at_admission_hook = stop_at_admission.clone();
        crate::lifecycle::race_point::arm(
            "api_run_stop_before_admission",
            &run_id,
            move || async move {
                stop_at_admission_hook.notify_one();
            },
        );
        // This hook returns inline; after it, the proof's next await is the shared gate.
        // Spawn is still parked after owner writes, so Pending here is admission drain.
        let mut proof = Box::pin(env.app.api_run_stop_proof(&run_key));
        let proof_first_poll = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(match proof.as_mut().poll(cx) {
                std::task::Poll::Pending => None,
                std::task::Poll::Ready(result) => Some(result),
            })
        })
        .await;
        let stop_reached_admission =
            tokio::time::timeout(DEADLOCK_TIMEOUT, stop_at_admission.notified())
                .await
                .is_ok();
        let proof_polled_pending_on_shared_gate = proof_first_poll.is_none();

        release_owner_writes.notify_one();
        let registry_was_installed =
            tokio::time::timeout(DEADLOCK_TIMEOUT, registry_installed.notified())
                .await
                .is_ok();
        let runner_is_held_before_entry =
            tokio::time::timeout(DEADLOCK_TIMEOUT, runner_entered.notified())
                .await
                .is_ok();
        let registered_slot = env.app.api_bot_runtime.inner.registry.is_active(&run_id);
        let run_cancel = env
            .app
            .api_bot_runtime
            .inner
            .controls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&FullKey::from(&turn_key))
            .map(|record| record.cancel.clone());
        let terminal_update = sqlx::query(
            "UPDATE turns SET status='completed',completed_at=? WHERE id=?",
        )
        .bind(crate::db::now())
        .bind(&turn_id)
        .execute(&env.app.db)
        .await;

        release_registry.notify_one();
        let mut spawn = spawn;
        let first_spawn_result = tokio::time::timeout(DEADLOCK_TIMEOUT, &mut spawn).await;
        let mut early_proof_result = proof_first_poll;
        let cancellation_requested = match (run_cancel.as_ref(), early_proof_result.is_none()) {
            (Some(cancel), true) => tokio::time::timeout(DEADLOCK_TIMEOUT, async {
                tokio::select! {
                    _ = cancel.cancelled() => true,
                    result = &mut proof => {
                        early_proof_result = Some(result);
                        false
                    }
                }
            })
            .await
            .unwrap_or(false),
            _ => false,
        };
        let runner_live_while_held = env.app.api_run_live(&turn_key).await;
        release_runner.notify_one();

        let spawn_result = if first_spawn_result.is_err() {
            tokio::time::timeout(DEADLOCK_TIMEOUT, &mut spawn).await
        } else {
            first_spawn_result
        };
        let proof_result = match early_proof_result {
            Some(result) => Ok(result),
            None => tokio::time::timeout(DEADLOCK_TIMEOUT, proof).await,
        };
        let runner_joined = tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while env.app.api_run_live(&turn_key).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();

        assert!(spawn_completed_owner_writes, "spawn completed its owner DB writes before the stop CAS");
        assert!(registry_empty_before_cas, "the stop CAS happened before spawn_tracked installed a slot");
        assert!(control_absent_before_cas, "spawn had not published its control before the stop CAS");
        assert!(cas_applied, "the test stop CAS moved the run to stopping");
        assert!(stop_reached_admission, "proof reached the gate attempt before its first poll returned");
        assert!(proof_polled_pending_on_shared_gate, "proof returned Pending while spawn still held the shared gate");
        assert!(registry_was_installed, "spawn installed its ticket and control before releasing admission");
        assert!(runner_is_held_before_entry, "the real registered runner reached the test-only pre-entry gate");
        assert!(registered_slot, "the run remained live in the registry while the runner was held");
        assert_eq!(terminal_update.unwrap().rows_affected(), 1);
        assert!(matches!(&spawn_result, Ok(Ok(Ok(())))), "spawn did not finish successfully: {spawn_result:?}");
        assert!(cancellation_requested, "the run-scoped proof requested cancellation before runner release");
        assert!(runner_live_while_held, "the tracked runner remained live until its gate was released");
        let proof_result = proof_result.unwrap().unwrap();
        assert_eq!(proof_result.key, run_key);
        assert_eq!(
            proof_result.evidence,
            ApiRunStopEvidence::Joined {
                turn_id,
                completion: ApiRunCompletion::Cancelled,
            },
            "the admission proof returns only after joining the registered runner"
        );
        assert!(runner_joined, "the registry slot is gone after the actual runner join");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_lifecycle_stop_cas_first_rejects_spawn_and_holds_lock_through_commit() {
        let env = crate::testing::env().await;
        enable_fake_runtime(&env.app, "http://127.0.0.1:1".into(), DEADLOCK_TIMEOUT).await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "CAS first admission proof",
        )
        .await;
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(crate::db::now())
            .bind(&turn_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let turn_key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id,
        };

        let before_admission = Arc::new(Notify::new());
        let release_admission = Arc::new(Notify::new());
        let before_admission_hook = before_admission.clone();
        let release_admission_hook = release_admission.clone();
        crate::lifecycle::race_point::arm(
            "api_run_stop_before_admission",
            &run_id,
            move || async move {
                before_admission_hook.notify_one();
                release_admission_hook.notified().await;
            },
        );
        let proof_succeeded = Arc::new(Notify::new());
        let release_commit = Arc::new(Notify::new());
        let proof_succeeded_hook = proof_succeeded.clone();
        let release_commit_hook = release_commit.clone();
        crate::lifecycle::race_point::arm(
            "api_stop_proof_before_recover",
            &run_id,
            move || async move {
                proof_succeeded_hook.notify_one();
                release_commit_hook.notified().await;
            },
        );
        let stop_app = env.app.clone();
        let stop_bot_id = bot_id.clone();
        let stop = tokio::spawn(async move {
            crate::lifecycle::stop_bot(&stop_app, &stop_bot_id).await
        });
        let before_admission_seen = tokio::time::timeout(DEADLOCK_TIMEOUT, before_admission.notified())
            .await
            .is_ok();
        let state_after_cas: Option<String> = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
            .bind(&run_id)
            .fetch_optional(&env.app.db)
            .await
            .unwrap();
        let spawn_after_cas = env.app.api_turn_spawn(&turn_key).await;
        let slot_after_rejected_spawn = env.app.api_bot_runtime.inner.registry.is_active(&run_id);
        release_admission.notify_one();
        let proof_succeeded_before_recover = tokio::time::timeout(DEADLOCK_TIMEOUT, proof_succeeded.notified())
            .await
            .is_ok();
        let state_while_proof_paused: Option<String> = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
            .bind(&run_id)
            .fetch_optional(&env.app.db)
            .await
            .unwrap();

        let lock_requested = Arc::new(Notify::new());
        let probe = LockWaitProbe {
            app: env.app.clone(),
            requested: lock_requested.clone(),
        };
        let settle_run = run_id.clone();
        let settle = tokio::spawn(async move {
            crate::lifecycle::run_state::settle_once(
                &probe,
                &settle_run,
                &crate::lifecycle::run_state::Settle::BackToRunning,
            )
            .await
        });
        let back_to_running_polled_to_shared_lock = tokio::time::timeout(DEADLOCK_TIMEOUT, lock_requested.notified())
            .await
            .is_ok();

        release_commit.notify_one();
        let stop_result = tokio::time::timeout(DEADLOCK_TIMEOUT, stop).await;
        let settle_result = tokio::time::timeout(DEADLOCK_TIMEOUT, settle).await;
        let final_state: Option<String> = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
            .bind(&run_id)
            .fetch_optional(&env.app.db)
            .await
            .unwrap();

        assert!(before_admission_seen);
        assert_eq!(state_after_cas.as_deref(), Some("stopping"));
        assert_eq!(spawn_after_cas, Err(ApiBotError::IdentityMismatch));
        assert!(!slot_after_rejected_spawn, "CAS-first spawn does not install a registry slot");
        assert!(proof_succeeded_before_recover, "empty registry proof returned through real lifecycle stop");
        assert_eq!(state_while_proof_paused.as_deref(), Some("stopping"));
        assert!(back_to_running_polled_to_shared_lock, "BackToRunning requested the shared bot lock while stop held it");
        assert!(stop_result.unwrap().unwrap().unwrap());
        assert!(settle_result.unwrap().unwrap());
        assert_eq!(final_state.as_deref(), Some("stopped"));
    }

    #[tokio::test]
    async fn idle_lifecycle_stop_commits_after_empty_registry_proof_and_blocks_later_spawn() {
        let env = crate::testing::env().await;
        enable_fake_runtime(&env.app, "http://127.0.0.1:1".into(), DEADLOCK_TIMEOUT).await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "empty registry proof",
        )
        .await;
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(crate::db::now())
            .bind(&turn_id)
            .execute(&env.app.db)
            .await
            .unwrap();

        assert!(crate::lifecycle::stop_bot(&env.app, &bot_id).await.unwrap());
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
            .bind(&run_id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        let turn_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(state, "stopped");
        assert_eq!(turn_status, "completed", "idle proof does not rewrite a terminal turn");
        assert_eq!(
            env.app.api_turn_spawn(&ApiTurnKey {
                bot_id,
                run_id,
                turn_id,
            })
            .await,
            Err(ApiBotError::IdentityMismatch),
            "after proof, a new admission rechecks stopping before registering a runner"
        );
    }

    #[tokio::test]
    async fn shutdown_timeout_is_unconfirmed_and_does_not_forge_a_join() {
        let registry = Arc::new(ApiBotRegistry::new());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entered_task = entered.clone();
        let release_task = release.clone();
        let ticket = registry
            .spawn_tracked(
                RunTurnKey {
                    run_id: "r".into(),
                    turn_id: "t".into(),
                },
                move |_cancel| async move {
                    entered_task.notify_one();
                    release_task.notified().await;
                    Ok(TurnOutcome::Completed)
                },
            )
            .unwrap();
        let cancel = ticket.cancellation_token();
        let shutdown = CancellationToken::new();
        let (tx, mut rx) = watch::channel(JoinState::Pending);
        let owner = tokio::spawn(own_ticket(
            ticket,
            cancel.clone(),
            shutdown.clone(),
            tx,
            Duration::from_millis(5),
            None,
            None,
        ));
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified())
            .await
            .unwrap();
        shutdown.cancel();
        tokio::time::timeout(DEADLOCK_TIMEOUT, owner)
            .await
            .unwrap()
            .unwrap();
        assert!(
            cancel.is_cancelled(),
            "shutdown explicitly signals the run token"
        );
        assert_eq!(*rx.borrow_and_update(), JoinState::TimedOut);
        assert_eq!(wait_join(&mut rx).await, Err(ApiBotError::Unconfirmed));
        release.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while registry.is_active("r") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *rx.borrow_and_update(),
            JoinState::TimedOut,
            "a later registry finish cannot rewrite timeout as success"
        );
    }

    #[tokio::test]
    async fn turn_deadline_cancels_but_keeps_polling_the_same_runner_future() {
        let cancel = CancellationToken::new();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entered_future = entered.clone();
        let release_future = release.clone();
        let cancel_future = cancel.clone();
        let task = tokio::spawn(async move {
            let future = async move {
                entered_future.notify_one();
                release_future.notified().await;
                "joined"
            };
            await_runner_deadline(future, &cancel_future, Duration::from_millis(5)).await
        });
        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified())
            .await
            .unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, cancel.cancelled())
            .await
            .unwrap();
        assert!(
            !task.is_finished(),
            "deadline cannot turn a still-running future into Failed/Completed"
        );
        release.notify_one();
        assert_eq!(
            tokio::time::timeout(DEADLOCK_TIMEOUT, task)
                .await
                .unwrap()
                .unwrap(),
            ("joined", true),
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_post_join_revision_lookup_without_losing_join_receipt() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let held_connection = pool.acquire().await.unwrap();
        let runtime = ApiBotRuntime::new();
        let ticket = runtime
            .inner
            .registry
            .spawn_tracked(
                RunTurnKey {
                    run_id: "revision-wait".into(),
                    turn_id: "t".into(),
                },
                |_| async { Ok(TurnOutcome::Completed) },
            )
            .unwrap();
        let cancel = ticket.cancellation_token();
        let shutdown = CancellationToken::new();
        let before_revision = Arc::new(Notify::new());
        let (tx, mut rx) = watch::channel(JoinState::Pending);
        let owner = tokio::spawn(own_ticket(
            ticket,
            cancel,
            shutdown.clone(),
            tx,
            DEADLOCK_TIMEOUT,
            None,
            Some(SessionCommit {
                pool: pool.clone(),
                inner: runtime.inner.clone(),
                bot_id: "b".into(),
                identity: super::super::adapter::AdapterIdentity {
                    session_id: "s".into(),
                    bot_id: "b".into(),
                    run_id: "revision-wait".into(),
                    turn_id: "t".into(),
                    generation: 1,
                },
                expected: None,
                before_revision: Some(before_revision.clone()),
                after_commit_before_publish: None,
            }),
        ));

        tokio::time::timeout(DEADLOCK_TIMEOUT, before_revision.notified())
            .await
            .unwrap();
        shutdown.cancel();
        tokio::time::timeout(DEADLOCK_TIMEOUT, owner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            *rx.borrow_and_update(),
            JoinState::Joined(Ok(Completion::Completed)),
            "DB cursor maintenance cannot replace a real join receipt with timeout or extend shutdown",
        );
        assert!(runtime.inner.sessions.lock().unwrap().is_empty());
        drop(held_connection);
    }

    #[tokio::test]
    async fn dropping_pending_port_waiter_preserves_the_same_join_receipt() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, server_shutdown, server_task) =
            tool_response_server(calls.clone(), hit.clone()).await;
        let tools_entered = Arc::new(Notify::new());
        let release_tools = Arc::new(Notify::new());
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let mut dependencies = env.app.api_bot_runtime.test_dependencies().unwrap();
        dependencies.tools = Arc::new(BlockingTools {
            entered: tools_entered.clone(),
            release: release_tools.clone(),
        });
        env.app
            .api_bot_runtime
            .set_test_dependencies(dependencies);

        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "drop a pending stop waiter",
        )
        .await;
        let key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified())
            .await
            .unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, tools_entered.notified())
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=? AND bot_id=?")
            .bind(&run_id)
            .bind(&bot_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let cancel = {
            let controls = env
                .app
                .api_bot_runtime
                .inner
                .controls
                .lock()
                .expect("control mutex poisoned");
            controls.get(&FullKey::from(&key)).unwrap().cancel.clone()
        };
        let waiter_app = env.app.clone();
        let waiter_key = clone_turn_key(&key);
        let mut waiter =
            Box::pin(async move { waiter_app.api_run_cancel_and_join(&waiter_key).await });
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                result = &mut waiter => panic!("stop waiter completed while fake tool still blocks: {result:?}"),
            }
        })
        .await
        .unwrap();
        let mut join_wait_poll = std::future::poll_fn(|cx| match waiter.as_mut().poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Ready(()),
            std::task::Poll::Ready(result) => {
                panic!("stop waiter completed while fake tool still blocks: {result:?}")
            }
        });
        tokio::time::timeout(DEADLOCK_TIMEOUT, &mut join_wait_poll)
            .await
            .unwrap();
        drop(join_wait_poll);
        assert!(
            env.app.api_run_live(&key).await,
            "join owner retains the blocked runner after waiter polling"
        );
        drop(waiter);

        release_tools.notify_one();
        let second = tokio::time::timeout(DEADLOCK_TIMEOUT, env.app.api_run_cancel_and_join(&key))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, ApiRunCompletion::Cancelled);
        let receipt = {
            let controls = env
                .app
                .api_bot_runtime
                .inner
                .controls
                .lock()
                .expect("control mutex poisoned");
            let result = *controls.get(&FullKey::from(&key)).unwrap().joined.borrow();
            result
        };
        assert_eq!(receipt, JoinState::Joined(Ok(Completion::Cancelled)));
        assert!(!env.app.api_run_live(&key).await);
        server_shutdown.cancel();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn panic_and_early_channel_close_remain_unconfirmed() {
        assert_eq!(
            map_join_result(Err(RegistryError::TaskPanicked)),
            Err(JoinError::Unconfirmed),
        );
        let (tx, mut rx) = watch::channel(JoinState::Pending);
        drop(tx);
        assert_eq!(wait_join(&mut rx).await, Err(ApiBotError::Unconfirmed));
    }

    #[tokio::test]
    async fn lifecycle_stop_waits_for_the_real_tool_join_before_settling() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, server_shutdown, server_task) =
            tool_response_server(calls, hit.clone()).await;
        let tools_entered = Arc::new(Notify::new());
        let release_tools = Arc::new(Notify::new());
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let mut dependencies = env.app.api_bot_runtime.test_dependencies().unwrap();
        dependencies.tools = Arc::new(BlockingTools {
            entered: tools_entered.clone(),
            release: release_tools.clone(),
        });
        env.app.api_bot_runtime.set_test_dependencies(dependencies);

        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "stop while a tool is held",
        )
        .await;
        let key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        };
        let spawn_result = env.app.api_turn_spawn(&key).await;
        let hit_result = tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified()).await;
        let tools_entered_result =
            tokio::time::timeout(DEADLOCK_TIMEOUT, tools_entered.notified()).await;
        let owner = env
            .app
            .api_bot_runtime
            .inner
            .controls
            .lock()
            .ok()
            .and_then(|controls| {
                controls
                    .get(&FullKey::from(&key))
                    .map(|record| (record.cancel.clone(), record.joined.clone()))
            });
        let cancel = owner
            .as_ref()
            .map(|(cancel, _)| cancel.clone())
            .unwrap_or_else(CancellationToken::new);
        let stop_app = env.app.clone();
        let stop_bot_id = bot_id.clone();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let stop_task = tokio::spawn(async move {
            let result = crate::lifecycle::stop_bot(&stop_app, &stop_bot_id).await;
            let _ = stop_tx.send(result);
        });
        let stop_progress = tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                result = &mut stop_rx => Some(result),
            }
        })
        .await;
        let observed_cancel_before_stop_return = matches!(&stop_progress, Ok(None));

        let state_before_cleanup =
            sqlx::query_scalar::<_, String>("SELECT state FROM runs WHERE id=?")
                .bind(&run_id)
                .fetch_one(&env.app.db)
                .await;
        let turn_status_before_cleanup =
            sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
                .bind(&turn_id)
                .fetch_one(&env.app.db)
                .await;
        let live_before_cleanup = env.app.api_run_live(&key).await;
        let stop_finished_before_cleanup = stop_task.is_finished();

        // Restore the fixture owner, then cancel and release the actual runner before asserting.
        let restore_run = sqlx::query("UPDATE runs SET state='stopping' WHERE id=? AND bot_id=?")
            .bind(&run_id)
            .bind(&bot_id)
            .execute(&env.app.db)
            .await;
        let restore_turn = sqlx::query(
            "UPDATE turns SET status='in_flight',completed_at=NULL WHERE id=? AND run_id=?",
        )
        .bind(&turn_id)
        .bind(&run_id)
        .execute(&env.app.db)
        .await;
        cancel.cancel();
        release_tools.notify_one();
        let registry = env.app.api_bot_runtime.inner.registry.clone();
        let _registry_join =
            tokio::time::timeout(DEADLOCK_TIMEOUT, registry.cancel_and_join(&run_id)).await;
        let owner_join = if let Some((_, mut joined)) = owner {
            Some(tokio::time::timeout(DEADLOCK_TIMEOUT, wait_join(&mut joined)).await)
        } else {
            None
        };
        let stop_result = match stop_progress {
            Ok(Some(result)) => Some(result),
            Ok(None) | Err(_) => tokio::time::timeout(DEADLOCK_TIMEOUT, &mut stop_rx)
                .await
                .ok(),
        };
        let stop_task_result = stop_task.await;
        let cleanup_stop_result =
            tokio::time::timeout(DEADLOCK_TIMEOUT, crate::lifecycle::stop_bot(&env.app, &bot_id))
                .await;
        let state_after_cleanup =
            sqlx::query_scalar::<_, String>("SELECT state FROM runs WHERE id=?")
                .bind(&run_id)
                .fetch_one(&env.app.db)
                .await;
        let turn_status_after_cleanup =
            sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
                .bind(&turn_id)
                .fetch_one(&env.app.db)
                .await;
        let live_after_cleanup = env.app.api_run_live(&key).await;
        server_shutdown.cancel();
        let server_result = server_task.await;

        assert!(spawn_result.is_ok(), "API turn spawn failed: {spawn_result:?}");
        assert!(hit_result.is_ok(), "fake provider was not reached: {hit_result:?}");
        assert!(
            tools_entered_result.is_ok(),
            "fake tool did not enter its gate: {tools_entered_result:?}"
        );
        assert!(
            observed_cancel_before_stop_return,
            "stop returned or timed out before requesting cancellation"
        );
        assert!(
            matches!(&state_before_cleanup, Ok(state) if state == "stopping"),
            "run was not stopping while the tool was held: {state_before_cleanup:?}"
        );
        assert!(
            matches!(&turn_status_before_cleanup, Ok(status) if status == "in_flight"),
            "turn settled before the tool joined: {turn_status_before_cleanup:?}"
        );
        assert!(live_before_cleanup, "the blocked task had not joined");
        assert!(!stop_finished_before_cleanup, "lifecycle stop returned before the tool runner joined");
        assert!(matches!(&restore_run, Ok(result) if result.rows_affected() == 1));
        assert!(matches!(&restore_turn, Ok(result) if result.rows_affected() == 1));
        assert!(
            matches!(&owner_join, Some(Ok(Ok(_)))),
            "cleanup did not receive the exact owner join receipt: {owner_join:?}"
        );
        assert!(
            matches!(&stop_result, Some(Ok(Ok(true)))),
            "lifecycle stop did not finish after joining: {stop_result:?}"
        );
        assert!(
            matches!(&stop_task_result, Ok(())),
            "stop task did not finish: {stop_task_result:?}"
        );
        assert!(
            matches!(&cleanup_stop_result, Ok(Ok(_))),
            "cleanup stop retry failed: {cleanup_stop_result:?}"
        );
        assert!(
            matches!(&state_after_cleanup, Ok(state) if state == "stopped"),
            "run was not stopped after true join cleanup: {state_after_cleanup:?}"
        );
        assert!(
            matches!(&turn_status_after_cleanup, Ok(status) if status == "failed"),
            "turn was not settled after true join cleanup: {turn_status_after_cleanup:?}"
        );
        assert!(!live_after_cleanup, "the test runner remained live after true join cleanup");
        assert!(
            matches!(&server_result, Ok(())),
            "fake provider task did not stop: {server_result:?}"
        );
    }

    #[tokio::test]
    async fn lifecycle_stop_recovers_held_http_attempt_as_unknown_without_releasing_its_reservation() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, server_shutdown, server_task, late_response) =
            held_response_server(calls.clone(), hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "stop while HTTP is held",
        )
        .await;
        let key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified())
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert!(tokio::time::timeout(
            DEADLOCK_TIMEOUT,
            crate::lifecycle::stop_bot(&env.app, &bot_id),
        )
        .await
        .unwrap()
        .unwrap());
        let attempt: (String, Option<i64>, i64) = sqlx::query_as(
            "SELECT state,estimated_microusd,reserved_microusd FROM api_bot_attempts WHERE bot_id=? AND run_id=?",
        )
        .bind(&bot_id)
        .bind(&run_id)
        .fetch_one(&env.app.db)
        .await
        .unwrap();
        assert_eq!(attempt, ("unknown".into(), None, 100));

        let _ = late_response.send(json!({
            "id":"late-fixture-message",
            "type":"message",
            "role":"assistant",
            "model":"claude-opus-5-5",
            "content":[{"type":"text","text":"late response"}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        }));
        server_shutdown.cancel();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn missing_owner_and_completed_turn_do_not_forge_a_join_receipt() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, server_shutdown, server_task) = fake_server(calls, hit.clone()).await;
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;

        let missing_bot = crate::db::ulid();
        let missing_run = crate::db::ulid();
        let missing_turn = crate::db::ulid();
        let missing_conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &missing_bot,
            &missing_run,
            &missing_turn,
            &missing_conv,
            &env.project_id,
            "no runner owner",
        )
        .await;
        let missing_key = ApiTurnKey {
            bot_id: missing_bot.clone(),
            run_id: missing_run.clone(),
            turn_id: missing_turn.clone(),
        };
        let stop_error = crate::lifecycle::stop_bot(&env.app, &missing_bot)
            .await
            .expect_err("missing runner owner cannot make lifecycle stop succeed");
        assert!(matches!(stop_error, crate::lifecycle::LcError::Upstream(message) if message.starts_with("api_bot_stop_unconfirmed:unconfirmed")));
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
            .bind(&missing_run)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        let turn_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&missing_turn)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(state, "stopping");
        assert_eq!(turn_status, "in_flight");
        assert_eq!(
            env.app.api_run_cancel_and_join(&missing_key).await,
            Err(ApiBotError::Unconfirmed),
        );
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(crate::db::now())
            .bind(&missing_turn)
            .execute(&env.app.db)
            .await
            .unwrap();
        assert_eq!(
            env.app.api_run_cancel_and_join(&missing_key).await,
            Err(ApiBotError::Unconfirmed),
            "a completed DB turn cannot replace the missing join owner",
        );

        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "terminal DB row is not an in-flight owner",
        )
        .await;
        let key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        };
        env.app.api_turn_spawn(&key).await.unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified())
            .await
            .unwrap();
        wait_until_joined(&env.app, &key).await;
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?")
            .bind(&run_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(crate::db::now())
            .bind(&turn_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        assert_eq!(
            env.app.api_run_cancel_and_join(&key).await,
            Err(ApiBotError::IdentityMismatch),
            "a DB completed row cannot impersonate the lifecycle join receipt",
        );
        server_shutdown.cancel();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_stop_joins_a_live_runner_after_its_turn_row_is_terminal() {
        let env = crate::testing::env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let hit = Arc::new(Notify::new());
        let (endpoint, server_shutdown, server_task) =
            tool_response_server(calls, hit.clone()).await;
        let tools_entered = Arc::new(Notify::new());
        let release_tools = Arc::new(Notify::new());
        enable_fake_runtime(&env.app, endpoint, DEADLOCK_TIMEOUT).await;
        let mut dependencies = env.app.api_bot_runtime.test_dependencies().unwrap();
        dependencies.tools = Arc::new(BlockingTools {
            entered: tools_entered.clone(),
            release: release_tools.clone(),
        });
        env.app.api_bot_runtime.set_test_dependencies(dependencies);

        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "terminal turn row still has a live runner",
        )
        .await;
        let key = ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        };
        env.app.api_turn_spawn(&key).await.unwrap();
        let http_request_arrived = tokio::time::timeout(DEADLOCK_TIMEOUT, hit.notified())
            .await
            .is_ok();
        let tool_was_entered = tokio::time::timeout(DEADLOCK_TIMEOUT, tools_entered.notified())
            .await
            .is_ok();
        let terminal_update = sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(crate::db::now())
            .bind(&turn_id)
            .execute(&env.app.db)
            .await;

        let stop_app = env.app.clone();
        let stop_bot_id = bot_id.clone();
        let stop = tokio::spawn(async move {
            crate::lifecycle::stop_bot(&stop_app, &stop_bot_id).await
        });
        let run_cancel = env
            .app
            .api_bot_runtime
            .inner
            .controls
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&FullKey::from(&key))
            .map(|record| record.cancel.clone());
        let state_while_tool_is_held = tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            loop {
                let state: Result<String, sqlx::Error> = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
                    .bind(&run_id)
                    .fetch_one(&env.app.db)
                    .await;
                match state {
                    Ok(state) if state == "stopping" => break Some(state),
                    Ok(_) => tokio::task::yield_now().await,
                    Err(_) => break None,
                }
            }
        })
        .await;
        let cancellation_requested_while_tool_is_held = match run_cancel.as_ref() {
            Some(cancel) => tokio::time::timeout(DEADLOCK_TIMEOUT, cancel.cancelled())
                .await
                .is_ok(),
            None => false,
        };
        let cancelled_while_tool_is_held =
            run_cancel.is_some_and(|cancel| cancel.is_cancelled());
        let runner_live_while_tool_is_held = env.app.api_run_live(&key).await;

        release_tools.notify_one();
        let mut stop = stop;
        let stop_result = tokio::time::timeout(DEADLOCK_TIMEOUT, &mut stop).await;
        let runner_joined = tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while env.app.api_run_live(&key).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        let state: Result<String, sqlx::Error> = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
            .bind(&run_id)
            .fetch_one(&env.app.db)
            .await;
        let turn_status: Result<String, sqlx::Error> = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&env.app.db)
            .await;
        let runner_live_after_join = env.app.api_run_live(&key).await;
        server_shutdown.cancel();
        let server_result = server_task.await;

        assert!(http_request_arrived);
        assert!(tool_was_entered);
        assert_eq!(terminal_update.unwrap().rows_affected(), 1);
        assert_eq!(state_while_tool_is_held.unwrap(), Some("stopping".into()));
        assert!(cancellation_requested_while_tool_is_held, "stop reached the cancellation notification while the tool was held");
        assert!(cancelled_while_tool_is_held, "the run-scoped proof requests cancellation");
        assert!(runner_live_while_tool_is_held, "the runner is live while the fake tool is held");
        assert!(stop_result.unwrap().unwrap().unwrap());
        assert!(runner_joined, "stop returns only after the tracked runner is joined");
        assert_eq!(state.unwrap(), "stopped");
        assert_eq!(turn_status.unwrap(), "completed", "idle proof does not rewrite a terminal turn");
        assert!(!runner_live_after_join);
        assert!(server_result.is_ok());
    }

    #[tokio::test]
    async fn joined_completion_waits_for_real_ticket_join_and_publishes_cursor_before_events() {
        let env = crate::testing::env().await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot_id, &run_id, &turn_id, &conv, &env.project_id, "join gate").await;
        let key = ApiTurnKey { bot_id: bot_id.clone(), run_id: run_id.clone(), turn_id: turn_id.clone() };
        let identity = create_completion_session(&env.app, &key).await;
        let runtime = ApiBotRuntime::new();
        let entered = Arc::new(Notify::new());
        let release_runner = Arc::new(Notify::new());
        let entered_task = entered.clone();
        let release_task = release_runner.clone();
        let (text_tx, text_rx) = oneshot::channel();
        let ticket = runtime.inner.registry.spawn_tracked(
            RunTurnKey { run_id: run_id.clone(), turn_id: turn_id.clone() },
            move |_cancel| async move {
                let response = completion_response();
                let _ = text_tx.send(super::super::completion::FinalText::from_response(&response));
                entered_task.notify_one();
                release_task.notified().await;
                Ok(TurnOutcome::Completed)
            },
        ).unwrap();
        let cancel = ticket.cancellation_token();
        let (joined_tx, mut joined_rx) = watch::channel(JoinState::Pending);
        let before_storage = Arc::new(Notify::new());
        let release_storage = Arc::new(Notify::new());
        let mut events = env.app.subscribe();
        let owner = tokio::spawn(own_ticket(
            ticket,
            cancel,
            CancellationToken::new(),
            joined_tx,
            DEADLOCK_TIMEOUT,
            Some(CompletionOwner {
                app: env.app.clone(),
                key: clone_turn_key(&key),
                identity: identity.clone(),
                final_text: text_rx,
                before_storage: Some((before_storage.clone(), release_storage.clone())),
                retry_observer: None,
                store_fault: None,
            }),
            Some(SessionCommit {
                pool: env.app.db.clone(),
                inner: runtime.inner.clone(),
                bot_id: bot_id.clone(),
                identity: identity.clone(),
                expected: None,
                before_revision: None,
                after_commit_before_publish: None,
            }),
        ));

        tokio::time::timeout(DEADLOCK_TIMEOUT, entered.notified()).await.unwrap();
        assert_eq!(*joined_rx.borrow_and_update(), JoinState::Pending);
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(status, "in_flight");
        assert_eq!(count, 0, "runner payload before task exit is not completion");
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM api_bot_join_receipts WHERE turn_id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap(), 0,
            "visible payload before true join is not a durable receipt");

        release_runner.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, before_storage.notified()).await.unwrap();
        assert_eq!(*joined_rx.borrow_and_update(), JoinState::Joined(Ok(Completion::Completed)));
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(status, "in_flight", "join receipt is independent of storage");
        assert!(events.try_recv().is_err(), "no event before commit and cursor publication");

        let waiter_inner = runtime.inner.clone();
        let waiter_key = ApiRunKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
        };
        let mut admission_waiter =
            Box::pin(admission_gate_for(&waiter_inner, &waiter_key));
        let first_poll = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(admission_waiter.as_mut().poll(cx))
        })
        .await;
        assert!(first_poll.is_pending(), "new admission waits for cursor publication");

        release_storage.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, owner).await.unwrap().unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(status, "completed");
        let receipt: Option<(String, String)> = sqlx::query_as(
            "SELECT state,final_text FROM api_bot_join_receipts WHERE session_id=? AND turn_id=?"
        ).bind(&identity.session_id).bind(&turn_id).fetch_optional(&env.app.db).await.unwrap();
        assert_eq!(receipt, Some(("applied".into(), "joined final".into())),
            "a true joined completion must atomically apply its durable visible-text receipt");
        let cursor = runtime.inner.sessions.lock().unwrap().get(&bot_id).cloned().unwrap();
        assert_eq!(cursor.identity, identity);
        let db_revision: i64 = sqlx::query_scalar("SELECT revision FROM api_bot_sessions WHERE id=?")
            .bind(&cursor.identity.session_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(cursor.revision, db_revision);
        let _admission = tokio::time::timeout(DEADLOCK_TIMEOUT, admission_waiter)
            .await
            .unwrap();
        let admitted_cursor = waiter_inner
            .sessions
            .lock()
            .unwrap()
            .get(&bot_id)
            .cloned()
            .expect("admission sees the committed cursor");
        assert_eq!(admitted_cursor.identity, identity);
        assert_eq!(admitted_cursor.revision, db_revision);
        assert_eq!(events.try_recv().unwrap().kind, "message_added");
        assert_eq!(events.try_recv().unwrap().kind, "turn_updated");
    }

    #[tokio::test]
    async fn committed_cursor_is_published_even_if_stop_wins_before_cache_update() {
        let env = crate::testing::env().await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot_id, &run_id, &turn_id, &conv, &env.project_id, "post-commit stop").await;
        let key = ApiTurnKey { bot_id: bot_id.clone(), run_id: run_id.clone(), turn_id: turn_id.clone() };
        let identity = create_completion_session(&env.app, &key).await;
        let runtime = ApiBotRuntime::new();
        let (text_tx, text_rx) = oneshot::channel();
        let ticket = runtime.inner.registry.spawn_tracked(
            RunTurnKey { run_id: run_id.clone(), turn_id: turn_id.clone() },
            move |_cancel| async move {
                let response = completion_response();
                let _ = text_tx.send(super::super::completion::FinalText::from_response(&response));
                Ok(TurnOutcome::Completed)
            },
        ).unwrap();
        let cancel = ticket.cancellation_token();
        let (joined_tx, _) = watch::channel(JoinState::Pending);
        let after_commit = Arc::new(Notify::new());
        let release_publish = Arc::new(Notify::new());
        let owner = tokio::spawn(own_ticket(
            ticket,
            cancel,
            CancellationToken::new(),
            joined_tx,
            DEADLOCK_TIMEOUT,
            Some(CompletionOwner {
                app: env.app.clone(),
                key: clone_turn_key(&key),
                identity: identity.clone(),
                final_text: text_rx,
                before_storage: None,
                retry_observer: None,
                store_fault: None,
            }),
            Some(SessionCommit {
                pool: env.app.db.clone(),
                inner: runtime.inner.clone(),
                bot_id: bot_id.clone(),
                identity: identity.clone(),
                expected: None,
                before_revision: None,
                after_commit_before_publish: Some((after_commit.clone(), release_publish.clone())),
            }),
        ));

        tokio::time::timeout(DEADLOCK_TIMEOUT, after_commit.notified()).await.unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(status, "completed", "completion transaction already committed");
        let moved = sqlx::query("UPDATE runs SET state='stopping' WHERE id=? AND state='running'")
            .bind(&run_id).execute(&env.app.db).await.unwrap();
        assert_eq!(moved.rows_affected(), 1);
        release_publish.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, owner).await.unwrap().unwrap();
        let cursor = runtime.inner.sessions.lock().unwrap().get(&bot_id).cloned().unwrap();
        assert_eq!(cursor.identity, identity);
        let db_revision: i64 = sqlx::query_scalar("SELECT revision FROM api_bot_sessions WHERE id=?")
            .bind(&cursor.identity.session_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(cursor.revision, db_revision);
    }

    #[tokio::test]
    async fn late_join_owner_cannot_overwrite_a_newer_session_cursor() {
        let runtime = ApiBotRuntime::new();
        let bot_id = "cursor-bot".to_string();
        let old = super::super::adapter::AdapterIdentity {
            session_id: "s".into(),
            bot_id: bot_id.clone(),
            run_id: "r1".into(),
            turn_id: "t1".into(),
            generation: 1,
        };
        let newer = super::super::adapter::AdapterIdentity {
            session_id: "s".into(),
            bot_id: bot_id.clone(),
            run_id: "r1".into(),
            turn_id: "t2".into(),
            generation: 1,
        };
        runtime.inner.sessions.lock().unwrap().insert(
            bot_id.clone(),
            SessionCursor { identity: newer.clone(), revision: 6 },
        );
        let commit = SessionCommit {
            pool: sqlx::sqlite::SqlitePoolOptions::new()
                .connect_lazy("sqlite::memory:")
                .unwrap(),
            inner: runtime.inner.clone(),
            bot_id: bot_id.clone(),
            identity: old.clone(),
            expected: Some(SessionCursor { identity: old, revision: 4 }),
            before_revision: None,
            after_commit_before_publish: None,
        };
        assert!(!publish_session_cursor(commit, 5));
        let current = runtime.inner.sessions.lock().unwrap().get(&bot_id).cloned().unwrap();
        assert_eq!(current.identity, newer);
        assert_eq!(current.revision, 6);
    }

    #[tokio::test]
    async fn transient_completion_db_error_retries_after_rollback() {
        let env = crate::testing::env().await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot_id, &run_id, &turn_id, &conv, &env.project_id, "retry once").await;
        let key = ApiTurnKey { bot_id: bot_id.clone(), run_id: run_id.clone(), turn_id: turn_id.clone() };
        let identity = create_completion_session(&env.app, &key).await;
        sqlx::query(&format!(
            "CREATE TRIGGER completion_retry_once BEFORE INSERT ON messages WHEN NEW.turn_id='{}' BEGIN SELECT RAISE(ABORT, 'injected transient completion error'); END",
            turn_id
        ))
        .execute(&env.app.db)
        .await
        .unwrap();

        let runtime = ApiBotRuntime::new();
        let (text_tx, text_rx) = oneshot::channel();
        let ticket = runtime.inner.registry.spawn_tracked(
            RunTurnKey { run_id: run_id.clone(), turn_id: turn_id.clone() },
            move |_cancel| async move {
                let response = completion_response();
                let _ = text_tx.send(super::super::completion::FinalText::from_response(&response));
                Ok(TurnOutcome::Completed)
            },
        ).unwrap();
        let cancel = ticket.cancellation_token();
        let (joined_tx, _) = watch::channel(JoinState::Pending);
        let retry_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retry_wake = Arc::new(Notify::new());
        let owner = tokio::spawn(own_ticket(
            ticket,
            cancel,
            CancellationToken::new(),
            joined_tx,
            DEADLOCK_TIMEOUT,
            Some(CompletionOwner {
                app: env.app.clone(),
                key: clone_turn_key(&key),
                identity: identity.clone(),
                final_text: text_rx,
                before_storage: None,
                retry_observer: Some((retry_count.clone(), retry_wake.clone())),
                store_fault: None,
            }),
            Some(SessionCommit {
                pool: env.app.db.clone(),
                inner: runtime.inner.clone(),
                bot_id,
                identity,
                expected: None,
                before_revision: None,
                after_commit_before_publish: None,
            }),
        ));

        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while retry_count.load(std::sync::atomic::Ordering::SeqCst) < 1 {
                retry_wake.notified().await;
            }
        })
        .await
        .unwrap();
        sqlx::query("DROP TRIGGER completion_retry_once")
            .execute(&env.app.db)
            .await
            .unwrap();
        tokio::time::timeout(DEADLOCK_TIMEOUT, owner).await.unwrap().unwrap();
        assert_eq!(retry_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(status, "completed");
        let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(messages, 1);
    }

    #[tokio::test]
    async fn permanent_completion_error_keeps_real_join_receipt_for_stop_recovery() {
        let env = crate::testing::env().await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot_id, &run_id, &turn_id, &conv, &env.project_id, "stop recovers joined error").await;
        let key = ApiTurnKey { bot_id: bot_id.clone(), run_id: run_id.clone(), turn_id: turn_id.clone() };
        let identity = create_completion_session(&env.app, &key).await;
        reserve_completion_attempt(&env.app, &identity).await;
        sqlx::query(&format!(
            "CREATE TRIGGER completion_permanent_failure BEFORE INSERT ON messages WHEN NEW.role='assistant' AND NEW.turn_id='{}' BEGIN SELECT RAISE(ABORT, 'injected permanent completion error'); END",
            turn_id
        ))
        .execute(&env.app.db)
        .await
        .unwrap();

        let runtime = &env.app.api_bot_runtime;
        runtime.set_test_dependencies(ApiBotDependencies {
            endpoint: "http://127.0.0.1:1/no-call".into(),
            key_source: Arc::new(super::super::adapter::FixedTestKey("test-only".into())),
            quote: Arc::new(Quote),
            tools: Arc::new(NoTools),
            shutdown_join_timeout: Some(DEADLOCK_TIMEOUT),
        });
        let (text_tx, text_rx) = oneshot::channel();
        let ticket = runtime.inner.registry.spawn_tracked(
            RunTurnKey { run_id: run_id.clone(), turn_id: turn_id.clone() },
            move |_cancel| async move {
                let response = completion_response();
                let _ = text_tx.send(super::super::completion::FinalText::from_response(&response));
                Ok(TurnOutcome::Completed)
            },
        ).unwrap();
        let cancel = ticket.cancellation_token();
        let (joined_tx, joined_rx) = watch::channel(JoinState::Pending);
        runtime.inner.controls.lock().unwrap().insert(
            FullKey::from(&key),
            ControlRecord { identity: Arc::new(()), cancel: cancel.clone(), joined: joined_rx.clone(), finalized: None, owner_exited: false },
        );
        let retry_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retry_wake = Arc::new(Notify::new());
        let owner = tokio::spawn(own_ticket(
            ticket,
            cancel,
            CancellationToken::new(),
            joined_tx,
            DEADLOCK_TIMEOUT,
            Some(CompletionOwner {
                app: env.app.clone(),
                key: clone_turn_key(&key),
                identity,
                final_text: text_rx,
                before_storage: None,
                retry_observer: Some((retry_count.clone(), retry_wake.clone())),
                store_fault: None,
            }),
            None,
        ));

        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while retry_count.load(std::sync::atomic::Ordering::SeqCst) < API_BOT_COMPLETION_MAX_ATTEMPTS {
                retry_wake.notified().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *joined_rx.borrow(),
            JoinState::Joined(Ok(Completion::Completed)),
            "storage retry exhaustion cannot rewrite genuine task completion"
        );
        let before_stop: (String, String, String) = sqlx::query_as(
            "SELECT r.state,t.status,a.state FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_attempts a ON a.run_id=r.id WHERE r.id=? AND t.id=?",
        )
        .bind(&run_id)
        .bind(&turn_id)
        .fetch_one(&env.app.db)
        .await
        .unwrap();
        assert_eq!(before_stop, ("running".into(), "in_flight".into(), "reserved".into()));
        assert_eq!(sqlx::query_scalar::<_, String>("SELECT state FROM api_bot_join_receipts WHERE turn_id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap(), "pending",
            "short retry exhaustion retains durable recovery data for D2b/D3");
        assert!(crate::lifecycle::stop_bot(&env.app, &bot_id).await.unwrap());
        tokio::time::timeout(DEADLOCK_TIMEOUT, owner).await.unwrap().unwrap();
        sqlx::query("DROP TRIGGER completion_permanent_failure")
            .execute(&env.app.db)
            .await
            .unwrap();

        let after_stop: (String, String, String, i64, i64) = sqlx::query_as(
            "SELECT r.state,t.status,a.state,a.reserved_microusd,(SELECT COUNT(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant') FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_attempts a ON a.run_id=r.id WHERE r.id=? AND t.id=?",
        )
        .bind(&run_id)
        .bind(&turn_id)
        .fetch_one(&env.app.db)
        .await
        .unwrap();
        assert_eq!(after_stop, ("stopped".into(), "failed".into(), "unknown".into(), 250, 0));
    }

    #[tokio::test]
    async fn ambiguous_store_with_exact_uncommitted_owner_retries_once() {
        let env = crate::testing::env().await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot_id, &run_id, &turn_id, &conv, &env.project_id, "retry ambiguous owner").await;
        let key = ApiTurnKey { bot_id: bot_id.clone(), run_id: run_id.clone(), turn_id: turn_id.clone() };
        let identity = create_completion_session(&env.app, &key).await;
        let runtime = ApiBotRuntime::new();
        let fault_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (text_tx, text_rx) = oneshot::channel();
        let ticket = runtime.inner.registry.spawn_tracked(
            RunTurnKey { run_id: run_id.clone(), turn_id: turn_id.clone() },
            move |_cancel| async move {
                let response = completion_response();
                let _ = text_tx.send(super::super::completion::FinalText::from_response(&response));
                Ok(TurnOutcome::Completed)
            },
        ).unwrap();
        let cancel = ticket.cancellation_token();
        let (joined_tx, joined_rx) = watch::channel(JoinState::Pending);
        let mut events = env.app.subscribe();
        let owner = tokio::spawn(own_ticket(
            ticket,
            cancel,
            CancellationToken::new(),
            joined_tx,
            DEADLOCK_TIMEOUT,
            Some(CompletionOwner {
                app: env.app.clone(),
                key: clone_turn_key(&key),
                identity: identity.clone(),
                final_text: text_rx,
                before_storage: None,
                retry_observer: None,
                store_fault: Some((CompletionStoreFault::AmbiguousBeforeStore, fault_calls.clone())),
            }),
            Some(SessionCommit {
                pool: env.app.db.clone(),
                inner: runtime.inner.clone(),
                bot_id: bot_id.clone(),
                identity: identity.clone(),
                expected: None,
                before_revision: None,
                after_commit_before_publish: None,
            }),
        ));

        tokio::time::timeout(DEADLOCK_TIMEOUT, owner).await.unwrap().unwrap();
        assert_eq!(
            *joined_rx.borrow(),
            JoinState::Joined(Ok(Completion::Completed)),
            "storage retry cannot replace the real RunTicket receipt"
        );
        assert_eq!(fault_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(status, "completed");
        let assistant: Vec<(String, String)> = sqlx::query_as(
            "SELECT id,content FROM messages WHERE turn_id=? AND role='assistant'",
        )
        .bind(&turn_id).fetch_all(&env.app.db).await.unwrap();
        assert_eq!(assistant.len(), 1, "retry commits one assistant message");
        assert_eq!(assistant[0].1, "joined final");
        // Completed turns publish bot_status after turn_updated; assert all completion events by
        // their exact owner key so this legitimate projection does not mask a duplicate pair.
        let mut message_events = Vec::new();
        let mut turn_events = Vec::new();
        let mut status_events = Vec::new();
        while let Ok(event) = events.try_recv() {
            if event.kind == "message_added"
                && event.data["message"]["turn_id"].as_str() == Some(turn_id.as_str())
            {
                message_events.push(event);
            } else if event.kind == "turn_updated"
                && event.data["turn"]["id"].as_str() == Some(turn_id.as_str())
            {
                turn_events.push(event);
            } else if event.kind == "bot_status"
                && event.data["bot_id"].as_str() == Some(bot_id.as_str())
                && event.data["run"]["id"].as_str() == Some(run_id.as_str())
            {
                status_events.push(event);
            }
        }
        assert_eq!(message_events.len(), 1, "one message_added for this exact turn");
        assert_eq!(
            message_events[0].data["bot_id"].as_str(),
            Some(bot_id.as_str())
        );
        assert_eq!(
            message_events[0].data["message"]["id"].as_str(),
            Some(assistant[0].0.as_str())
        );
        assert_eq!(message_events[0].data["message"]["role"], "assistant");
        assert_eq!(turn_events.len(), 1, "one turn_updated for this exact turn");
        assert_eq!(turn_events[0].data["bot_id"].as_str(), Some(bot_id.as_str()));
        assert_eq!(
            turn_events[0].data["turn"]["run_id"].as_str(),
            Some(run_id.as_str())
        );
        assert_eq!(turn_events[0].data["turn"]["status"], "completed");
        assert!(message_events[0].seq < turn_events[0].seq);
        assert_eq!(status_events.len(), 1, "one bot_status for this exact bot/run");
        assert!(turn_events[0].seq < status_events[0].seq);
    }

    #[tokio::test]
    async fn ambiguous_store_proves_applied_receipt_and_emits_once_without_retry() {
        let env = crate::testing::env().await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(&env.app, &bot_id, &run_id, &turn_id, &conv, &env.project_id, "stop ambiguous resend").await;
        let key = ApiTurnKey { bot_id: bot_id.clone(), run_id: run_id.clone(), turn_id: turn_id.clone() };
        let identity = create_completion_session(&env.app, &key).await;
        let runtime = ApiBotRuntime::new();
        let fault_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (text_tx, text_rx) = oneshot::channel();
        let ticket = runtime.inner.registry.spawn_tracked(
            RunTurnKey { run_id: run_id.clone(), turn_id: turn_id.clone() },
            move |_cancel| async move {
                let response = completion_response();
                let _ = text_tx.send(super::super::completion::FinalText::from_response(&response));
                Ok(TurnOutcome::Completed)
            },
        ).unwrap();
        let cancel = ticket.cancellation_token();
        let (joined_tx, joined_rx) = watch::channel(JoinState::Pending);
        let mut events = env.app.subscribe();
        let owner = tokio::spawn(own_ticket(
            ticket,
            cancel,
            CancellationToken::new(),
            joined_tx,
            DEADLOCK_TIMEOUT,
            Some(CompletionOwner {
                app: env.app.clone(),
                key: clone_turn_key(&key),
                identity: identity.clone(),
                final_text: text_rx,
                before_storage: None,
                retry_observer: None,
                store_fault: Some((CompletionStoreFault::AmbiguousAfterCommit, fault_calls.clone())),
            }),
            Some(SessionCommit {
                pool: env.app.db.clone(),
                inner: runtime.inner.clone(),
                bot_id: bot_id.clone(),
                identity,
                expected: None,
                before_revision: None,
                after_commit_before_publish: None,
            }),
        ));

        tokio::time::timeout(DEADLOCK_TIMEOUT, owner).await.unwrap().unwrap();
        assert_eq!(
            *joined_rx.borrow(),
            JoinState::Joined(Ok(Completion::Completed)),
            "ambiguous storage cannot erase the true join receipt"
        );
        assert_eq!(fault_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(status, "completed", "the simulated lost acknowledgment followed a commit");
        let assistant: Vec<String> = sqlx::query_scalar(
            "SELECT content FROM messages WHERE turn_id=? AND role='assistant'",
        )
        .bind(&turn_id).fetch_all(&env.app.db).await.unwrap();
        assert_eq!(assistant, vec!["joined final".to_string()]);
        let applied: String = sqlx::query_scalar("SELECT state FROM api_bot_join_receipts WHERE turn_id=?")
            .bind(&turn_id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(applied, "applied", "success requires durable applied proof, not an assistant alone");
        let cursor = runtime.inner.sessions.lock().unwrap().get(&bot_id).cloned().unwrap();
        assert_eq!(cursor.identity.turn_id, turn_id);
        let mut kinds = Vec::new();
        while let Ok(event) = events.try_recv() { kinds.push(event.kind); }
        assert_eq!(kinds.iter().filter(|kind| kind.as_str()=="message_added").count(), 1);
        assert_eq!(kinds.iter().filter(|kind| kind.as_str()=="turn_updated").count(), 1);
    }

    #[tokio::test]
    async fn api_bot_restart_is_rejected_with_a_stable_reason() {
        let env = crate::testing::env().await;
        let bot_id = crate::db::ulid();
        let run_id = crate::db::ulid();
        let turn_id = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot_id,
            &run_id,
            &turn_id,
            &conv,
            &env.project_id,
            "API bot restart must not enter the CLI start path",
        )
        .await;

        let error = crate::lifecycle::restart_bot(&env.app, &bot_id)
            .await
            .expect_err("API bots cannot restart through the CLI lifecycle");
        match error {
            crate::lifecycle::LcError::Conflict(body) => {
                assert_eq!(body["reason"], "api_bot_restart_unsupported");
            }
            other => panic!("expected named restart conflict, received {other:?}"),
        }
        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
            .bind(&run_id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(state, "running", "restart refusal leaves the run untouched");
    }
    #[tokio::test]
    async fn oversized_joined_final_text_fails_closed_without_receipt_or_accounting() {
        let env = crate::testing::env().await;
        let bot = crate::db::ulid();
        let run = crate::db::ulid();
        let turn = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot,
            &run,
            &turn,
            &conv,
            &env.project_id,
            "oversized fixture",
        )
        .await;
        let key = ApiTurnKey {
            bot_id: bot,
            run_id: run,
            turn_id: turn.clone(),
        };
        let identity = create_completion_session(&env.app, &key).await;
        reserve_completion_attempt(&env.app, &identity).await;
        let (_tx, rx) = oneshot::channel();
        let owner = CompletionOwner {
            app: env.app.clone(),
            key,
            identity,
            final_text: rx,
            before_storage: None,
            retry_observer: None,
            store_fault: None,
        };
        let mut response = completion_response();
        response.content =
            vec![json!({"type":"text","text":"x".repeat(super::super::receipt::MAX_TEXT_BYTES+1)})];
        let text = super::super::completion::FinalText::from_response(&response);
        assert!(
            settle_joined_completion(&owner, Completion::Completed, Some(text), &CancellationToken::new(), None)
                .await
                .is_none()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM api_bot_join_receipts")
                .fetch_one(&env.app.db)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
                .bind(&turn)
                .fetch_one(&env.app.db)
                .await
                .unwrap(),
            "in_flight"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM api_bot_attempts WHERE turn_id=?")
                .bind(&turn)
                .fetch_one(&env.app.db)
                .await
                .unwrap(),
            "reserved"
        );
    }

    #[tokio::test]
    async fn failed_cancelled_or_missing_final_text_use_failure_receipts() {
        for joined in [
            Completion::Completed,
            Completion::Failed,
            Completion::Cancelled,
        ] {
            let env = crate::testing::env().await;
            let bot = crate::db::ulid();
            let run = crate::db::ulid();
            let turn = crate::db::ulid();
            let conv = crate::db::ulid();
            insert_bot_and_turn(
                &env.app,
                &bot,
                &run,
                &turn,
                &conv,
                &env.project_id,
                "failure fixture",
            )
            .await;
            let key = ApiTurnKey {
                bot_id: bot,
                run_id: run,
                turn_id: turn.clone(),
            };
            let identity = create_completion_session(&env.app, &key).await;
            let (_tx, rx) = oneshot::channel();
            let owner = CompletionOwner {
                app: env.app.clone(),
                key,
                identity,
                final_text: rx,
                before_storage: None,
                retry_observer: None,
                store_fault: None,
            };
            assert!(settle_joined_completion(&owner, joined, None, &CancellationToken::new(), None)
                .await
                .is_some());
            let row: (String, String, Option<String>) = sqlx::query_as(
                "SELECT state,outcome,final_text FROM api_bot_join_receipts WHERE turn_id=?",
            )
            .bind(&turn)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
            assert_eq!(row, ("applied".into(), "failed".into(), None));
            assert_eq!(
                sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
                    .bind(&turn)
                    .fetch_one(&env.app.db)
                    .await
                    .unwrap(),
                "failed"
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'"
                )
                .bind(&turn)
                .fetch_one(&env.app.db)
                .await
                .unwrap(),
                0
            );
        }
    }


    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BackoffProofFault {
        None,
        BeforeStore,
        AfterCommit,
    }
    struct BackoffFixture {
        env: crate::testing::Env,
        key: ApiTurnKey,
        identity: super::super::adapter::AdapterIdentity,
        shutdown: CancellationToken,
        owner: tokio::task::JoinHandle<()>,
        retries: Arc<std::sync::atomic::AtomicUsize>,
        wake: Arc<Notify>,
        runner_calls: Arc<std::sync::atomic::AtomicUsize>,
        store_calls: Arc<std::sync::atomic::AtomicUsize>,
        joined: watch::Receiver<JoinState>,
        at_backoff: Arc<Notify>,
        release_backoff: Arc<Notify>,
    }
    impl Drop for BackoffFixture {
        fn drop(&mut self) {
            self.shutdown.cancel();
            self.release_backoff.notify_one();
        }
    }
    async fn backoff_fixture() -> BackoffFixture {
        backoff_fixture_with_proof_fault(BackoffProofFault::None).await
    }
    async fn backoff_fixture_with_proof_fault(mode: BackoffProofFault) -> BackoffFixture {
        let env = crate::testing::env().await;
        let bot = crate::db::ulid();
        let run = crate::db::ulid();
        let turn = crate::db::ulid();
        let conv = crate::db::ulid();
        insert_bot_and_turn(
            &env.app,
            &bot,
            &run,
            &turn,
            &conv,
            &env.project_id,
            "storage backoff",
        )
        .await;
        let key = ApiTurnKey {
            bot_id: bot.clone(),
            run_id: run.clone(),
            turn_id: turn.clone(),
        };
        let identity = create_completion_session(&env.app, &key).await;
        reserve_completion_attempt(&env.app, &identity).await;
        sqlx::query("CREATE TRIGGER completion_backoff_failure BEFORE INSERT ON messages WHEN NEW.role='assistant' BEGIN SELECT RAISE(ABORT,'synthetic backoff fault'); END")
            .execute(&env.app.db).await.unwrap();
        if mode == BackoffProofFault::BeforeStore {
            sqlx::query("ALTER TABLE api_bot_join_receipts RENAME TO hidden_join_receipts")
                .execute(&env.app.db)
                .await
                .unwrap();
        }
        if mode == BackoffProofFault::BeforeStore {
            sqlx::query("ALTER TABLE bots RENAME TO hidden_bots")
                .execute(&env.app.db)
                .await
                .unwrap();
        }
        if mode == BackoffProofFault::AfterCommit {
            sqlx::query("DROP TRIGGER completion_backoff_failure")
                .execute(&env.app.db)
                .await
                .unwrap();
            let pool = env.app.db.clone();
            crate::lifecycle::race_point::arm(
                "api_completion_ack_lost",
                &run,
                move || async move {
                    sqlx::query("ALTER TABLE api_bot_join_receipts RENAME TO hidden_join_receipts")
                        .execute(&pool)
                        .await
                        .unwrap();
                },
            );
        }
        let runtime = &env.app.api_bot_runtime;
        runtime.set_test_dependencies(ApiBotDependencies {
            endpoint: "http://127.0.0.1:1/no-call".into(),
            key_source: Arc::new(super::super::adapter::FixedTestKey("fake".into())),
            quote: Arc::new(Quote),
            tools: Arc::new(NoTools),
            shutdown_join_timeout: Some(DEADLOCK_TIMEOUT),
        });
        let at_backoff = Arc::new(Notify::new());
        let release_backoff = Arc::new(Notify::new());
        let reached = at_backoff.clone();
        let release = release_backoff.clone();
        crate::lifecycle::race_point::arm("api_completion_backoff", &run, move || async move {
            reached.notify_one();
            release.notified().await;
        });
        let runner_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = runner_calls.clone();
        let (text_tx, text_rx) = oneshot::channel();
        let ticket = runtime
            .inner
            .registry
            .spawn_tracked(
                RunTurnKey {
                    run_id: run,
                    turn_id: turn,
                },
                move |_cancel| async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = text_tx.send(super::super::completion::FinalText::from_response(
                        &completion_response(),
                    ));
                    Ok(TurnOutcome::Completed)
                },
            )
            .unwrap();
        let cancel = ticket.cancellation_token();
        let (joined_tx, joined) = watch::channel(JoinState::Pending);
        runtime.inner.controls.lock().unwrap().insert(
            FullKey::from(&key),
            ControlRecord {
                identity: Arc::new(()),
                cancel: cancel.clone(),
                joined: joined.clone(),
                finalized: None, owner_exited: false,
            },
        );
        let retries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake = Arc::new(Notify::new());
        let shutdown = CancellationToken::new();
        let store_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner = tokio::spawn(own_ticket(
            ticket,
            cancel,
            shutdown.clone(),
            joined_tx,
            DEADLOCK_TIMEOUT,
            Some(CompletionOwner {
                app: env.app.clone(),
                key: clone_turn_key(&key),
                identity: identity.clone(),
                final_text: text_rx,
                before_storage: None,
                retry_observer: Some((retries.clone(), wake.clone())),
                store_fault: match mode {
                    BackoffProofFault::None => None,
                    BackoffProofFault::BeforeStore => Some((
                        CompletionStoreFault::AmbiguousBeforeStore,
                        store_calls.clone(),
                    )),
                    BackoffProofFault::AfterCommit => Some((
                        CompletionStoreFault::AmbiguousAfterCommit,
                        store_calls.clone(),
                    )),
                },
            }),
            Some(SessionCommit {
                pool: env.app.db.clone(),
                inner: runtime.inner.clone(),
                bot_id: bot,
                identity: identity.clone(),
                expected: None,
                before_revision: None,
                after_commit_before_publish: None,
            }),
        ));
        BackoffFixture {
            env,
            key,
            identity,
            shutdown,
            owner,
            retries,
            wake,
            runner_calls,
            store_calls,
            joined,
            at_backoff,
            release_backoff,
        }
    }
    async fn wait_for_long_backoff(f: &BackoffFixture) {
        tokio::time::timeout(DEADLOCK_TIMEOUT, async {
            while f.retries.load(std::sync::atomic::Ordering::SeqCst)
                < API_BOT_COMPLETION_MAX_ATTEMPTS
            {
                f.wake.notified().await;
            }
            f.at_backoff.notified().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn joined_storage_recovers_after_short_retries_without_stop_or_runner_replay() {
        let mut f = backoff_fixture().await;
        wait_for_long_backoff(&f).await;
        assert_eq!(
            *f.joined.borrow(),
            JoinState::Joined(Ok(Completion::Completed))
        );
        sqlx::query("DROP TRIGGER completion_backoff_failure")
            .execute(&f.env.app.db)
            .await
            .unwrap();
        f.release_backoff.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, &mut f.owner)
            .await
            .unwrap()
            .unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
        assert_eq!(
            status, "completed",
            "joined storage must recover after the three short retries without Stop"
        );
        assert_eq!(f.runner_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'"
            )
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
            1
        );
        let cursor = f
            .env
            .app
            .api_bot_runtime
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&f.key.bot_id)
            .cloned()
            .unwrap();
        assert_eq!(cursor.identity, f.identity);
    }
    #[tokio::test]
    async fn completion_backoff_releases_all_locks_and_stop_wins_without_resurrection() {
        let mut f = backoff_fixture().await;
        wait_for_long_backoff(&f).await;
        let key = ApiRunKey {
            bot_id: f.key.bot_id.clone(),
            run_id: f.key.run_id.clone(),
        };
        let guard = tokio::time::timeout(
            DEADLOCK_TIMEOUT,
            admission_gate_for(&f.env.app.api_bot_runtime.inner, &key),
        )
        .await
        .expect("backoff must release admission");
        drop(guard);
        let bot_lock = f.env.app.bot_lock(&f.key.bot_id).await;
        let guard = tokio::time::timeout(DEADLOCK_TIMEOUT, bot_lock.lock())
            .await
            .expect("backoff must not hold bot lock");
        drop(guard);
        assert!(f.env.app.api_bot_runtime.inner.sessions.try_lock().is_ok());
        assert!(
            matches!(
                f.env.app.api_turn_spawn(&f.key).await,
                Err(ApiBotError::Busy)
            ),
            "free admission does not authorize another runner before cursor publication"
        );
        assert!(tokio::time::timeout(
            DEADLOCK_TIMEOUT,
            crate::lifecycle::stop_bot(&f.env.app, &f.key.bot_id)
        )
        .await
        .unwrap()
        .unwrap());
        f.release_backoff.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, &mut f.owner)
            .await
            .unwrap()
            .unwrap();
        let row:(String,String,String,i64,i64)=sqlx::query_as("SELECT r.state,t.status,a.state,a.reserved_microusd,(SELECT COUNT(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant') FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_attempts a ON a.turn_id=t.id WHERE t.id=?")
            .bind(&f.key.turn_id).fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(
            row,
            ("stopped".into(), "failed".into(), "unknown".into(), 250, 0)
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT state FROM api_bot_join_receipts WHERE turn_id=?"
            )
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
            "fenced"
        );
        assert_eq!(f.runner_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            *f.joined.borrow(),
            JoinState::Joined(Ok(Completion::Completed))
        );
    }
    #[tokio::test]
    async fn shutdown_interrupts_backoff_and_keeps_durable_pending_without_fake_completion() {
        let mut f = backoff_fixture().await;
        let mut events = f.env.app.subscribe();
        wait_for_long_backoff(&f).await;
        f.shutdown.cancel();
        tokio::time::timeout(DEADLOCK_TIMEOUT, &mut f.owner)
            .await
            .unwrap()
            .unwrap();
        let row:(String,String,String,String,i64)=sqlx::query_as("SELECT r.state,t.status,a.state,j.state,a.reserved_microusd FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_attempts a ON a.turn_id=t.id JOIN api_bot_join_receipts j ON j.turn_id=t.id WHERE t.id=?")
            .bind(&f.key.turn_id).fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(
            row,
            (
                "running".into(),
                "in_flight".into(),
                "reserved".into(),
                "pending".into(),
                250
            )
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'"
            )
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
            0
        );
        assert!(events.try_recv().is_err());
        assert!(f
            .env
            .app
            .api_bot_runtime
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&f.key.bot_id)
            .is_none());
        assert_eq!(
            *f.joined.borrow(),
            JoinState::Joined(Ok(Completion::Completed))
        );
        assert_eq!(f.runner_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn unconfirmed_storage_polls_read_only_until_proof_returns() {
        let mut f = backoff_fixture_with_proof_fault(BackoffProofFault::BeforeStore).await;
        wait_for_long_backoff(&f).await;
        assert_eq!(
            f.store_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "unconfirmed proof must not call the writer again"
        );
        sqlx::query("ALTER TABLE hidden_bots RENAME TO bots")
            .execute(&f.env.app.db)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE hidden_join_receipts RENAME TO api_bot_join_receipts")
            .execute(&f.env.app.db)
            .await
            .unwrap();
        sqlx::query("DROP TRIGGER completion_backoff_failure")
            .execute(&f.env.app.db)
            .await
            .unwrap();
        f.release_backoff.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, &mut f.owner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(f.store_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(f.runner_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT state FROM api_bot_join_receipts WHERE turn_id=?"
            )
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
            "applied"
        );
    }
    #[test]
    fn completion_backoff_is_bounded_and_does_not_overflow() {
        assert_eq!(completion_retry_delay(1), Duration::from_millis(25));
        assert_eq!(completion_retry_delay(2), Duration::from_millis(25));
        for (failures, seconds) in [
            (3, 1),
            (4, 2),
            (5, 4),
            (6, 8),
            (7, 16),
            (8, 30),
            (usize::MAX, 30),
        ] {
            assert_eq!(
                completion_retry_delay(failures),
                Duration::from_secs(seconds)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn completion_retry_sleep_is_immediately_interruptible_without_advancing_time() {
        let shutdown = CancellationToken::new();
        let mut waiting = Box::pin(completion_retry_wait(&shutdown, Duration::from_secs(30)));
        let first =
            std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx))).await;
        assert!(first.is_pending());
        shutdown.cancel();
        let interrupted =
            std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx))).await;
        assert_eq!(interrupted, std::task::Poll::Ready(false));
        let active = CancellationToken::new();
        let mut timer = Box::pin(completion_retry_wait(&active, Duration::from_secs(1)));
        let first =
            std::future::poll_fn(|cx| std::task::Poll::Ready(timer.as_mut().poll(cx))).await;
        assert!(first.is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(timer.await);
    }

    #[tokio::test]
    async fn lost_commit_ack_keeps_read_only_recovery_when_turn_is_already_completed() {
        let mut f = backoff_fixture_with_proof_fault(BackoffProofFault::AfterCommit).await;
        let reached=tokio::time::timeout(DEADLOCK_TIMEOUT,async {
            tokio::select! { result = &mut f.owner => { result.unwrap(); false }, _ = f.at_backoff.notified() => true }
        }).await.unwrap();
        assert!(
            reached,
            "a completed turn alone cannot retire recovery while Applied proof is unreadable"
        );
        assert_eq!(f.store_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        sqlx::query("ALTER TABLE hidden_join_receipts RENAME TO api_bot_join_receipts")
            .execute(&f.env.app.db)
            .await
            .unwrap();
        f.release_backoff.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, &mut f.owner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            f.store_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Applied proof must not replay the writer"
        );
        assert_eq!(f.runner_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'"
            )
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
            1
        );
        let cursor = f
            .env
            .app
            .api_bot_runtime
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&f.key.bot_id)
            .cloned()
            .expect("recovered applied proof publishes cursor");
        assert_eq!(cursor.identity, f.identity);
    }

    #[tokio::test]
    async fn stopped_owner_with_pruned_session_exits_recovery_without_recreating_state() {
        let mut f = backoff_fixture().await;
        wait_for_long_backoff(&f).await;
        assert!(crate::lifecycle::stop_bot(&f.env.app, &f.key.bot_id)
            .await
            .unwrap());
        sqlx::query("DELETE FROM api_bot_sessions WHERE id=?")
            .bind(&f.identity.session_id)
            .execute(&f.env.app.db)
            .await
            .unwrap();
        f.release_backoff.notify_one();
        tokio::time::timeout(DEADLOCK_TIMEOUT, &mut f.owner)
            .await
            .unwrap()
            .unwrap();
        let row:(String,String,String,i64)=sqlx::query_as("SELECT r.state,t.status,a.state,a.reserved_microusd FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_attempts a ON a.turn_id=t.id WHERE t.id=?")
            .bind(&f.key.turn_id).fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(
            row,
            ("stopped".into(), "failed".into(), "unknown".into(), 250)
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM api_bot_sessions")
                .fetch_one(&f.env.app.db)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM api_bot_join_receipts")
                .fetch_one(&f.env.app.db)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'"
            )
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
            0
        );
        assert_eq!(f.runner_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

}
