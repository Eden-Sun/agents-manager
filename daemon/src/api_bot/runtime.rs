//! App host for the inert Claude API bot runtime.
//!
//! Production remains deliberately fail-closed: constructing this value is memory-only, and no
//! production path can install provider, HTTP, or key dependencies. All `ApiBotPort` methods
//! refuse before database/config access when dependencies are absent. Tests inject fakes through
//! the test-only seam and exercise the same runtime core compiled into production.

use crate::lifecycle::api_bot_port::{
    ApiBotError, ApiRunCompletion, ApiRunKey, ApiRunStopEvidence, ApiRunStopProof, ApiTurnKey,
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
}

/// Runtime storage is initialized without tasks, sockets, or database work.
pub(crate) struct ApiBotRuntime {
    inner: Arc<RuntimeInner>,
    dependencies: Mutex<Option<ApiBotDependencies>>,
}

impl ApiBotRuntime {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                registry: Arc::new(ApiBotRegistry::new()),
                controls: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                admission: AsyncMutex::new(HashMap::new()),
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
        let Some(dependencies) = self.dependencies_snapshot() else {
            return Err(ApiBotError::Upstream);
        };
        self.spawn_with_dependencies(app, key, dependencies).await
    }

    async fn cancel_and_join(
        &self,
        app: &crate::state::App,
        key: &ApiTurnKey,
    ) -> Result<ApiRunCompletion, ApiBotError> {
        if self.dependencies_snapshot().is_none() {
            return Err(ApiBotError::Upstream);
        }
        self.cancel_and_join_core(app, key).await
    }

    async fn stop_proof(
        &self,
        app: &crate::state::App,
        key: &ApiRunKey,
    ) -> Result<ApiRunStopProof, ApiBotError> {
        let Some(dependencies) = self.dependencies_snapshot() else {
            return Err(ApiBotError::Upstream);
        };

        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("api_run_stop_before_admission", &key.run_id).await;
        // A pre-CAS spawn may have passed run validation but not installed its registry ticket yet.
        // Draining the same gate used by spawn makes that ticket visible before an empty-slot proof.
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

        let timeout = dependencies
            .shutdown_join_timeout
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
        if self.dependencies_snapshot().is_none() {
            return false;
        }
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

    async fn admission_gate(&self, key: &ApiRunKey) -> OwnedMutexGuard<()> {
        admission_gate_for(&self.inner, key).await
    }

    async fn spawn_gate(&self, key: &ApiTurnKey) -> OwnedMutexGuard<()> {
        self.admission_gate(&ApiRunKey {
            bot_id: key.bot_id.clone(),
            run_id: key.run_id.clone(),
        })
        .await
    }

    async fn spawn_with_dependencies(
        &self,
        app: &crate::state::App,
        key: &ApiTurnKey,
        dependencies: ApiBotDependencies,
    ) -> Result<(), ApiBotError> {
        let _admission = self.spawn_gate(key).await;
        cleanup_terminal_controls(&app.db, &self.inner).await;
        let full = FullKey::from(key);
        let bot_run_already_admitted = {
            self.inner
                .controls
                .lock()
                .expect("control mutex poisoned")
                .keys()
                .any(|active| active.bot_id == full.bot_id && active.run_id == full.run_id)
        };
        if bot_run_already_admitted {
            return Err(ApiBotError::Busy);
        }

        let (bot, project, run, prompt) = validated_turn(app, key).await?;
        let cfg = app.cfg.get().await;
        let limits = cfg
            .claude_api
            .api_bot
            .validated(cfg.claude_api.enabled)
            .map_err(|_| ApiBotError::Upstream)?
            .ok_or(ApiBotError::Upstream)?;
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
        let (identity, expected_cursor) = self.identity_for_turn(&app.db, &full).await?;
        let session_cursor = expected_cursor.clone();
        let (adapter, mut state) = if let Some(cursor) = session_cursor {
            let (adapter, state) = super::adapter::ApiBotAdapter::rebind(
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
            )
            .await
            .map_err(|_| ApiBotError::Unconfirmed)?;
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

        if state.api_pending || state.tool_pending.is_some() {
            return Err(ApiBotError::Unconfirmed);
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
            .map_err(|_| ApiBotError::Upstream)?;
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
        #[cfg(all(test, feature = "daemon-test-harness"))]
        let runner_run_id = key.run_id.clone();
        let run_ticket = self
            .inner
            .registry
            .spawn_tracked(registry_key, move |cancel| async move {
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
        self.inner
            .controls
            .lock()
            .expect("control mutex poisoned")
            .insert(
                full,
                ControlRecord {
                    identity: identity_tag,
                    cancel: cancel.clone(),
                    joined: joined_rx,
                },
            );

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
        app.background_tasks.spawn(Box::pin(async move {
            own_ticket(
                run_ticket,
                cancel,
                shutdown,
                joined_tx,
                shutdown_timeout,
                Some(completion_owner),
                Some(session_commit),
            )
            .await;
        }));
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("api_run_spawn_after_registry", &key.run_id).await;
        Ok(())
    }

    async fn identity_for_turn(
        &self,
        pool: &sqlx::SqlitePool,
        key: &FullKey,
    ) -> Result<(super::adapter::AdapterIdentity, Option<SessionCursor>), ApiBotError> {
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
            return Err(ApiBotError::Unconfirmed);
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
        ))
    }

    async fn cancel_and_join_core(
        &self,
        app: &crate::state::App,
        key: &ApiTurnKey,
    ) -> Result<ApiRunCompletion, ApiBotError> {
        let full = FullKey::from(key);
        let (cancel, mut joined) = {
            let controls = self.inner.controls.lock().expect("control mutex poisoned");
            let record = controls.get(&full).ok_or(ApiBotError::Unconfirmed)?;
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
    fn control_count_for_test(&self) -> usize {
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
    fn api_turn_spawn(
        &self,
        key: &ApiTurnKey,
    ) -> impl Future<Output = Result<(), ApiBotError>> + Send {
        async move { self.api_bot_runtime.spawn(self, key).await }
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

async fn own_ticket(
    ticket: RunTicket,
    cancel: CancellationToken,
    shutdown: CancellationToken,
    result_tx: watch::Sender<JoinState>,
    shutdown_timeout: Duration,
    completion_owner: Option<CompletionOwner>,
    session_commit: Option<SessionCommit>,
) {
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
            let admission = if let Some(commit) = &session_commit {
                Some(
                    admission_gate_for(
                        &commit.inner,
                        &ApiRunKey {
                            bot_id: commit.bot_id.clone(),
                            run_id: commit.identity.run_id.clone(),
                        },
                    )
                    .await,
                )
            } else {
                None
            };
            #[cfg(test)]
            if let Some(notify) = &owner.before_storage {
                notify.0.notify_one();
                notify.1.notified().await;
            }
            let final_text = if joined == Completion::Completed {
                (&mut owner.final_text).await.ok()
            } else {
                None
            };
            if let Some(committed) = settle_joined_completion(&owner, joined, final_text).await {
                if let Some(commit) = session_commit {
                    #[cfg(test)]
                    if let Some((reached, release)) = &commit.after_commit_before_publish {
                        reached.notify_one();
                        release.notified().await;
                    }
                    publish_session_cursor(commit, committed.session_revision);
                }
                drop(admission);
                // The exact cursor from the committing transaction is visible before notifications
                // can trigger another turn. No sessions mutex is held during DB I/O or event awaits.
                super::completion::emit_after_commit(&owner.app, &owner.key, committed).await;
            } else {
                drop(admission);
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

async fn settle_joined_completion(
    owner: &CompletionOwner,
    joined: Completion,
    final_text: Option<super::completion::FinalText>,
) -> Option<super::completion::Committed> {
    let payload = match (joined, final_text.as_ref()) {
        (Completion::Completed, Some(text)) => text.receipt_payload(),
        _ => super::receipt::Payload::ended(super::receipt::JoinOutcome::Failed),
    };
    let Ok(payload) = payload else {
        warn_completion_storage(&owner.key);
        return None; // Oversize is never truncated or replayed as another request.
    };
    for attempt in 0..API_BOT_COMPLETION_MAX_ATTEMPTS {
        #[cfg(test)]
        let result = {
            let injected_fault = owner.store_fault.as_ref().map(|(fault, calls)| {
                (
                    *fault,
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                )
            });
            store_joined_completion_with_test_fault(owner, &payload, injected_fault).await
        };
        #[cfg(not(test))]
        let result = store_joined_completion(owner, &payload).await;
        let retry = match result {
            Ok(super::completion::Outcome::Committed(committed)) => return Some(committed),
            Ok(super::completion::Outcome::Fenced) => return None,
            Err(super::completion::StoreError::Retryable) => true,
            Err(super::completion::StoreError::Ambiguous) => {
                match super::completion::probe_after_join(&owner.app, &owner.identity, &payload)
                    .await
                {
                    Ok(super::completion::Probe::Applied(committed)) => return Some(committed),
                    Ok(super::completion::Probe::Pending) => true,
                    Ok(super::completion::Probe::Stale(item)) => {
                        let _ = super::receipt::fence_stale(&owner.app.db, &item).await;
                        return None;
                    }
                    _ => {
                        warn_completion_storage(&owner.key);
                        return None;
                    }
                }
            }
        };
        if retry {
            #[cfg(test)]
            if let Some((count, notify)) = &owner.retry_observer {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                notify.notify_one();
            }
        }
        if attempt + 1 < API_BOT_COMPLETION_MAX_ATTEMPTS {
            tokio::time::sleep(API_BOT_COMPLETION_RETRY_DELAY).await;
        }
    }
    warn_completion_storage(&owner.key);
    None
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
    key: String,
    quote: Arc<dyn super::quote::QuoteProvider>,
    tools: Arc<dyn super::runner::Tools>,
    shutdown_join_timeout: Option<Duration>,
}

fn api_http_config(
    dependencies: &ApiBotDependencies,
    limits: &am_config::api_bot::ValidatedLimits,
) -> super::adapter::AdapterHttpConfig {
    super::adapter::AdapterHttpConfig {
        endpoint: dependencies.endpoint.clone(),
        key: dependencies.key.clone(),
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

async fn cleanup_terminal_controls(pool: &sqlx::SqlitePool, inner: &RuntimeInner) {
    let candidates: Vec<(FullKey, Arc<()>)> = {
        let controls = inner.controls.lock().expect("control mutex poisoned");
        controls
            .iter()
            .filter_map(|(key, record)| {
                matches!(*record.joined.borrow(), JoinState::Joined(_))
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
                && matches!(*record.joined.borrow(), JoinState::Joined(_))
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
            key: "sk-ant-test-only".into(),
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
            "CREATE TRIGGER completion_permanent_failure BEFORE INSERT ON messages WHEN NEW.turn_id='{}' BEGIN SELECT RAISE(ABORT, 'injected permanent completion error'); END",
            turn_id
        ))
        .execute(&env.app.db)
        .await
        .unwrap();

        let runtime = &env.app.api_bot_runtime;
        runtime.set_test_dependencies(ApiBotDependencies {
            endpoint: "http://127.0.0.1:1/no-call".into(),
            key: "test-only".into(),
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
            ControlRecord { identity: Arc::new(()), cancel: cancel.clone(), joined: joined_rx.clone() },
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
        tokio::time::timeout(DEADLOCK_TIMEOUT, owner).await.unwrap().unwrap();
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
        sqlx::query("DROP TRIGGER completion_permanent_failure")
            .execute(&env.app.db)
            .await
            .unwrap();
        assert!(crate::lifecycle::stop_bot(&env.app, &bot_id).await.unwrap());
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
            settle_joined_completion(&owner, Completion::Completed, Some(text))
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
            assert!(settle_joined_completion(&owner, joined, None)
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

}
