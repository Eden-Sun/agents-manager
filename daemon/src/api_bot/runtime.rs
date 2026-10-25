//! App host for the inert Claude API bot runtime.
//!
//! Production remains deliberately fail-closed: constructing this value is memory-only, and no
//! production path can install provider, HTTP, or key dependencies. All `ApiBotPort` methods
//! refuse before database/config access when dependencies are absent. Tests inject fakes through
//! the test-only seam and exercise the same runtime core compiled into production.

use crate::lifecycle::api_bot_port::{ApiBotError, ApiRunCompletion, ApiTurnKey};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::registry::{ApiBotRegistry, RegistryError, RunTicket, RunTurnKey, TurnOutcome};

const API_BOT_SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

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

#[derive(Clone)]
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

    async fn spawn_gate(&self, key: &ApiTurnKey) -> OwnedMutexGuard<()> {
        let gate_key = AdmissionKey {
            bot_id: key.bot_id.clone(),
            run_id: key.run_id.clone(),
        };
        let gate = {
            let mut gates = self.inner.admission.lock().await;
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
        let identity = self.identity_for_turn(&app.db, &full).await?;
        let session_cursor = {
            self.inner
                .sessions
                .lock()
                .expect("session mutex poisoned")
                .get(&key.bot_id)
                .cloned()
        };
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
        let run_ticket = self
            .inner
            .registry
            .spawn_tracked(registry_key, move |cancel| async move {
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
                if deadline_elapsed {
                    Ok(TurnOutcome::Failed("turn_timeout".into()))
                } else {
                    Ok(map_runner_result(result, &cancel))
                }
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
            #[cfg(test)]
            before_revision: None,
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
                Some(session_commit),
            )
            .await;
        }));
        Ok(())
    }

    async fn identity_for_turn(
        &self,
        pool: &sqlx::SqlitePool,
        key: &FullKey,
    ) -> Result<super::adapter::AdapterIdentity, ApiBotError> {
        let session_cursor = {
            self.inner
                .sessions
                .lock()
                .expect("session mutex poisoned")
                .get(&key.bot_id)
                .cloned()
        };
        if let Some(cursor) = session_cursor {
            let generation = if cursor.identity.run_id == key.run_id {
                cursor.identity.generation
            } else {
                cursor
                    .identity
                    .generation
                    .checked_add(1)
                    .ok_or(ApiBotError::Unconfirmed)?
            };
            return Ok(super::adapter::AdapterIdentity {
                session_id: cursor.identity.session_id,
                bot_id: key.bot_id.clone(),
                run_id: key.run_id.clone(),
                turn_id: key.turn_id.clone(),
                generation,
            });
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
        Ok(super::adapter::AdapterIdentity {
            session_id: crate::db::ulid(),
            bot_id: key.bot_id.clone(),
            run_id: key.run_id.clone(),
            turn_id: key.turn_id.clone(),
            generation: 1,
        })
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

    fn api_run_live(&self, key: &ApiTurnKey) -> impl Future<Output = bool> + Send {
        async move { self.api_bot_runtime.live(self, key).await }
    }
}

struct SessionCommit {
    pool: sqlx::SqlitePool,
    inner: Arc<RuntimeInner>,
    bot_id: String,
    identity: super::adapter::AdapterIdentity,
    #[cfg(test)]
    before_revision: Option<Arc<tokio::sync::Notify>>,
}

async fn own_ticket(
    ticket: RunTicket,
    cancel: CancellationToken,
    shutdown: CancellationToken,
    result_tx: watch::Sender<JoinState>,
    shutdown_timeout: Duration,
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
    if matches!(state, JoinState::Joined(_)) {
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
                commit
                    .inner
                    .sessions
                    .lock()
                    .expect("session mutex poisoned")
                    .insert(
                        commit.bot_id,
                        SessionCursor {
                            identity: commit.identity,
                            revision,
                        },
                    );
            }
        }
    }
    result_tx.send_replace(state);
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
    use crate::lifecycle::api_bot_port::ApiBotPort;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};
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
        sqlx::query("INSERT OR IGNORE INTO bots(id,project_id,name,kind,effort,fast,hook_token,created_at) VALUES(?,?,'api-test','claude-api','high',0,'fixture',?)")
            .bind(bot_id)
            .bind(project_id)
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

    async fn stop_and_collect(app: &crate::state::App, key: &ApiTurnKey) -> ApiRunCompletion {
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=? AND bot_id=?")
            .bind(&key.run_id)
            .bind(&key.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        app.api_run_cancel_and_join(key).await.unwrap()
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
        assert!(!app.api_run_live(&key).await);
        assert_eq!(pool.size(), 0, "all three port calls reject before SQLite");
        assert_eq!(
            am_config::GET_CALLS.with(|calls| calls.get()),
            0,
            "all three port calls reject before ConfigStore::get"
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
            assert_eq!(
                stop_and_collect(&env.app, &key).await,
                ApiRunCompletion::Completed
            );
            sqlx::query("UPDATE runs SET state='stopped',ended_at=? WHERE id=?")
                .bind(crate::db::now())
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
                before_revision: Some(before_revision.clone()),
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
}
