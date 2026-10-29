//! Single-turn runner adapter connecting inert execution (A) to SQLite storage (B).
//!
//! Lock hierarchy:
//! 1. Cursor lock (`tokio::sync::Mutex`) protects short DB mutations (reserve, record, save).
//!    It MUST NEVER be held across long async I/O (`Model::complete` network requests or
//!    `Tools::execute` child processes).
//! 2. External stop operations CAS `runs.state` to `stopping` and trigger cancellation.
//!    They NEVER acquire the cursor lock.

use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use am_base::db::api_bot::{self as db_api, Checkpoint, Fence};

use super::http;
use super::protocol::{Message, Options, Response, ToolCall};
use super::quote::{QuoteProvider, RequestEnvelope};
use super::runner::{self, Journal, Model, State, Tools};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdapterIdentity {
    pub(crate) session_id: String,
    pub(crate) bot_id: String,
    pub(crate) run_id: String,
    pub(crate) turn_id: String,
    pub(crate) generation: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdapterLimits {
    pub(crate) max_checkpoint_bytes: usize,
    pub(crate) max_turn_microusd: i64,
    pub(crate) max_requests_per_hour: i64,
}

impl AdapterLimits {
    pub(crate) fn validate(&self) -> Result<(), http::Error> {
        if self.max_checkpoint_bytes == 0 || self.max_checkpoint_bytes > 8 * 1024 * 1024 {
            return Err(http::Error::InvalidRequest);
        }
        if self.max_turn_microusd <= 0 || self.max_requests_per_hour <= 0 {
            return Err(http::Error::InvalidRequest);
        }
        Ok(())
    }
}

pub(super) struct Secret(String);
impl Secret {
    pub(super) fn new(value: String) -> Self { Self(value) }
    pub(super) fn is_empty(&self) -> bool { self.0.trim().is_empty() }
    fn as_str(&self) -> &str { &self.0 }
}
impl Drop for Secret {
    fn drop(&mut self) {
        // Volatile overwrites keep this best-effort clearing from being optimized away.
        for byte in unsafe { self.0.as_bytes_mut() } { unsafe { std::ptr::write_volatile(byte, 0) }; }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}
pub(super) trait KeySource: Send + Sync {
    fn snapshot(&self) -> BoxFuture<'_, Result<Secret, ()>>;
}
#[cfg(test)]
pub(super) struct FixedTestKey(pub(super) String);
#[cfg(test)]
impl KeySource for FixedTestKey {
    fn snapshot(&self) -> BoxFuture<'_, Result<Secret, ()>> { Box::pin(async { Ok(Secret::new(self.0.clone())) }) }
}

#[derive(Clone)]
pub(crate) struct AdapterHttpConfig {
    pub(crate) endpoint: String,
    pub(super) key_source: Arc<dyn KeySource>,
    pub(crate) timeout: std::time::Duration,
    pub(crate) max_response_bytes: usize,
}

pub(crate) struct ApiBotAdapter {
    pool: SqlitePool,
    identity: AdapterIdentity,
    options: Options,
    system: Value,
    limits: AdapterLimits,
    quote_provider: Arc<dyn QuoteProvider>,
    http_cfg: AdapterHttpConfig,
    tools: Option<Arc<dyn Tools>>,
    cursor: Mutex<CursorState>,
}

struct CursorState {
    revision: i64,
    last_api_pending: bool,
    last_durable_state: State,
    active_attempt: Option<ActiveAttempt>,
    settled_response: Option<SettledResponse>,
}

struct SettledResponse {
    response: Response,
    estimated_microusd: i64,
    actual_speed: Option<&'static str>,
}

struct ActiveAttempt {
    id: String,
    envelope: RequestEnvelope,
    permit_available: bool,
    key: Option<Secret>,
}

impl ApiBotAdapter {
    /// Create adapter for the first turn (generation 1, fresh session at revision 0).
    pub(crate) async fn new(
        pool: SqlitePool,
        identity: AdapterIdentity,
        options: Options,
        system: Value,
        limits: AdapterLimits,
        quote_provider: Arc<dyn QuoteProvider>,
        http_cfg: AdapterHttpConfig,
        tools: Option<Arc<dyn Tools>>,
    ) -> Result<Self, http::Error> {
        options
            .validate()
            .map_err(|_| http::Error::InvalidRequest)?;
        limits.validate()?;
        if identity.generation != 1
            || identity.session_id.is_empty()
            || identity.bot_id.is_empty()
            || identity.run_id.is_empty()
            || identity.turn_id.is_empty()
        {
            return Err(http::Error::InvalidRequest);
        }

        let fence = Fence {
            session: &identity.session_id,
            bot: &identity.bot_id,
            run: &identity.run_id,
            turn: &identity.turn_id,
            generation: identity.generation,
            revision: 0,
        };

        let initial_state = State::default();
        let state_val =
            serde_json::to_value(&initial_state).map_err(|_| http::Error::InvalidRequest)?;
        let checkpoint = Checkpoint::new(&state_val).map_err(|_| http::Error::InvalidRequest)?;

        db_api::create(&pool, &fence, &checkpoint)
            .await
            .map_err(|_| http::Error::InvalidRequest)?;

        Ok(Self {
            pool,
            identity,
            options,
            system,
            limits,
            quote_provider,
            http_cfg,
            tools,
            cursor: Mutex::new(CursorState {
                revision: 0,
                last_api_pending: false,
                last_durable_state: initial_state,
                active_attempt: None,
                settled_response: None,
            }),
        })
    }

    /// Rebind an existing settled session to a new run / turn, loading durable state.
    pub(crate) async fn rebind(
        pool: SqlitePool,
        old_fence: &AdapterIdentity,
        old_revision: i64,
        new_identity: AdapterIdentity,
        options: Options,
        system: Value,
        limits: AdapterLimits,
        quote_provider: Arc<dyn QuoteProvider>,
        http_cfg: AdapterHttpConfig,
        tools: Option<Arc<dyn Tools>>,
    ) -> Result<(Self, State), http::Error> {
        options
            .validate()
            .map_err(|_| http::Error::InvalidRequest)?;
        limits.validate()?;

        if new_identity.session_id != old_fence.session_id
            || new_identity.bot_id != old_fence.bot_id
            || new_identity.generation < old_fence.generation
            || (new_identity.run_id != old_fence.run_id
                && new_identity.generation <= old_fence.generation)
            || old_revision < 0
            || new_identity.run_id.is_empty()
            || new_identity.turn_id.is_empty()
        {
            return Err(http::Error::InvalidRequest);
        }

        let fence_old = Fence {
            session: &old_fence.session_id,
            bot: &old_fence.bot_id,
            run: &old_fence.run_id,
            turn: &old_fence.turn_id,
            generation: old_fence.generation,
            revision: old_revision,
        };

        let new_rev = db_api::rebind(
            &pool,
            &fence_old,
            &new_identity.run_id,
            &new_identity.turn_id,
            new_identity.generation,
        )
        .await
        .map_err(|_| http::Error::InvalidRequest)?;

        let current_fence = Fence {
            session: &new_identity.session_id,
            bot: &new_identity.bot_id,
            run: &new_identity.run_id,
            turn: &new_identity.turn_id,
            generation: new_identity.generation,
            revision: new_rev,
        };

        let loaded_chk = db_api::load(&pool, &current_fence)
            .await
            .map_err(|_| http::Error::InvalidRequest)?;
        let loaded_state: State =
            serde_json::from_value(loaded_chk.value()).map_err(|_| http::Error::InvalidRequest)?;

        let adapter = Self {
            pool,
            identity: new_identity,
            options,
            system,
            limits,
            quote_provider,
            http_cfg,
            tools,
            cursor: Mutex::new(CursorState {
                revision: new_rev,
                last_api_pending: false,
                last_durable_state: loaded_state.clone(),
                active_attempt: None,
                settled_response: None,
            }),
        };

        Ok((adapter, loaded_state))
    }

    /// Exposes read-only inspection of current durable revision for testing assertions.
    #[cfg(test)]
    pub(crate) async fn current_revision(&self) -> i64 {
        let guard = self.cursor.lock().await;
        guard.revision
    }
}

impl Model for ApiBotAdapter {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Response, http::Error>> {
        Box::pin(async move {
            let (envelope, attempt_id, key) = {
                let mut cursor = self.cursor.lock().await;
                let active = cursor
                    .active_attempt
                    .as_mut()
                    .ok_or(http::Error::InvalidRequest)?;
                if !active.permit_available {
                    return Err(http::Error::InvalidRequest);
                }

                let reconstructed = RequestEnvelope::new(
                    self.options.clone(),
                    self.system.clone(),
                    messages.to_vec(),
                )
                .map_err(|_| http::Error::InvalidRequest)?;

                if reconstructed.hash() != active.envelope.hash() {
                    return Err(http::Error::InvalidRequest);
                }
                // Invalid input does not spend the matching reservation's permit.
                // Consume atomically only after identity validation, still before any HTTP.
                let key = active.key.take().ok_or(http::Error::InvalidRequest)?;
                active.permit_available = false;

                (reconstructed, active.id.clone(), key)
            };

            let req = http::FullMessagesRequest {
                endpoint: &self.http_cfg.endpoint,
                key: key.as_str(),
                options: envelope.options(),
                system: envelope.system(),
                messages: envelope.messages(),
                timeout: self.http_cfg.timeout,
                max_response_bytes: self.http_cfg.max_response_bytes,
            };

            let res = http::complete(&req, cancel).await;
            drop(key);

            let mut cursor = self.cursor.lock().await;
            match res {
                Ok(resp) => {
                    let quote_res = self.quote_provider.settle(&envelope, &resp);
                    let valid_quote = match quote_res {
                        Ok(uq)
                            if uq.estimated_microusd >= 0
                                && uq
                                    .actual_speed
                                    .is_none_or(|s| matches!(s, "fast" | "standard")) =>
                        {
                            Some(uq)
                        }
                        _ => None,
                    };

                    if let Some(uq) = valid_quote {
                        cursor.settled_response = Some(SettledResponse {
                            response: resp.clone(),
                            estimated_microusd: uq.estimated_microusd,
                            actual_speed: uq.actual_speed,
                        });
                        Ok(resp)
                    } else {
                        // Untrusted usage, unapproved quote, or invalid negative/speed: record unknown failclosed
                        let fence = Fence {
                            session: &self.identity.session_id,
                            bot: &self.identity.bot_id,
                            run: &self.identity.run_id,
                            turn: &self.identity.turn_id,
                            generation: self.identity.generation,
                            revision: cursor.revision,
                        };
                        let mut unknown_state = cursor.last_durable_state.clone();
                        unknown_state.api_pending = true;

                        let val = serde_json::to_value(&unknown_state)
                            .map_err(|_| http::Error::InvalidResponse)?;
                        let chk =
                            Checkpoint::new(&val).map_err(|_| http::Error::InvalidResponse)?;

                        let receipt = db_api::Receipt {
                            state: "unknown",
                            actual_speed: None,
                            usage: None,
                            estimated_microusd: None,
                        };

                        let next_rev =
                            db_api::record(&self.pool, &fence, &attempt_id, &receipt, &chk)
                                .await
                                .map_err(|_| http::Error::InvalidResponse)?;

                        cursor.revision = next_rev;
                        cursor.last_api_pending = true;
                        cursor.last_durable_state = unknown_state;

                        Err(http::Error::InvalidResponse)
                    }
                }
                Err(err) => {
                    let fence = Fence {
                        session: &self.identity.session_id,
                        bot: &self.identity.bot_id,
                        run: &self.identity.run_id,
                        turn: &self.identity.turn_id,
                        generation: self.identity.generation,
                        revision: cursor.revision,
                    };
                    let mut unknown_state = cursor.last_durable_state.clone();
                    unknown_state.api_pending = true;

                    if let Ok(val) = serde_json::to_value(&unknown_state) {
                        if let Ok(chk) = Checkpoint::new(&val) {
                            let receipt = db_api::Receipt {
                                state: "unknown",
                                actual_speed: None,
                                usage: None,
                                estimated_microusd: None,
                            };
                            if let Ok(next_rev) =
                                db_api::record(&self.pool, &fence, &attempt_id, &receipt, &chk)
                                    .await
                            {
                                cursor.revision = next_rev;
                                cursor.last_api_pending = true;
                                cursor.last_durable_state = unknown_state;
                            }
                        }
                    }
                    Err(err)
                }
            }
        })
    }
}

impl Journal for ApiBotAdapter {
    fn save<'a>(&'a self, state: &'a State) -> BoxFuture<'a, Result<(), ()>> {
        Box::pin(async move {
            let val = serde_json::to_value(state).map_err(|_| ())?;
            let chk = Checkpoint::new(&val).map_err(|_| ())?;
            let raw_len = serde_json::to_vec(&val).map_err(|_| ())?.len();
            if raw_len > self.limits.max_checkpoint_bytes {
                return Err(());
            }

            let mut cursor = self.cursor.lock().await;
            let fence = Fence {
                session: &self.identity.session_id,
                bot: &self.identity.bot_id,
                run: &self.identity.run_id,
                turn: &self.identity.turn_id,
                generation: self.identity.generation,
                revision: cursor.revision,
            };

            let before_pending = cursor.last_api_pending;
            let after_pending = state.api_pending;

            match (before_pending, after_pending) {
                (true, true) => {
                    // While HTTP is active/pending, disallow arbitrary mutation of messages/responses.
                    // Only allow exactly matching the last durable state (idempotent checkpoint), without granting permits.
                    let durable_val =
                        serde_json::to_value(&cursor.last_durable_state).map_err(|_| ())?;
                    if val != durable_val {
                        return Err(());
                    }
                    Ok(())
                }
                (false, true) => {
                    let envelope = RequestEnvelope::new(
                        self.options.clone(),
                        self.system.clone(),
                        state.messages.clone(),
                    )
                    .map_err(|_| ())?;
                    let key = self.http_cfg.key_source.snapshot().await.map_err(|_| {
                        tracing::info!(reason="api_key_unavailable", "API key snapshot unavailable");
                    })?;
                    if key.is_empty() { tracing::info!(reason="api_key_unavailable", "API key snapshot unavailable"); return Err(()); }
                    let quote = self.quote_provider.reserve(&envelope).map_err(|_| ())?;
                    if quote.reserved_microusd <= 0 || quote.pricing_version.is_empty() {
                        return Err(());
                    }

                    let attempt_id = am_base::db::ulid();
                    let attempt = db_api::Attempt {
                        id: &attempt_id,
                        ordinal: state.responses.len() as i64,
                        request_hash: envelope.hash(),
                        speed: envelope.speed(),
                        reserved_microusd: quote.reserved_microusd,
                        pricing_version: &quote.pricing_version,
                    };

                    let res = db_api::reserve(
                        &self.pool,
                        &fence,
                        &attempt,
                        &chk,
                        self.limits.max_turn_microusd,
                        self.limits.max_requests_per_hour,
                    )
                    .await
                    .map_err(|_| ())?;

                    match res {
                        db_api::Reservation::Fresh(next_rev) => {
                            cursor.revision = next_rev;
                            cursor.last_api_pending = true;
                            cursor.last_durable_state = state.clone();
                            cursor.active_attempt = Some(ActiveAttempt {
                                id: attempt_id,
                                envelope,
                                permit_available: true,
                                key: Some(key),
                            });
                            Ok(())
                        }
                        db_api::Reservation::Existing { .. } => Err(()),
                    }
                }
                (true, false) => {
                    let (active_id, resp, estimated_microusd, actual_speed) = {
                        let active = cursor.active_attempt.as_ref().ok_or(())?;
                        let settled = cursor.settled_response.as_ref().ok_or(())?;

                        // Verify legal increment from last durable state to state with assistant response
                        let durable = &cursor.last_durable_state;
                        if state.responses.len() != durable.responses.len() + 1 {
                            return Err(());
                        }

                        // Response has no PartialEq; compare prefix via JSON values
                        let durable_resp_val =
                            serde_json::to_value(&durable.responses).map_err(|_| ())?;
                        let state_prefix_resp_val =
                            serde_json::to_value(&state.responses[..durable.responses.len()])
                                .map_err(|_| ())?;
                        if durable_resp_val != state_prefix_resp_val {
                            return Err(());
                        }

                        let last_resp_val = serde_json::to_value(state.responses.last().unwrap())
                            .map_err(|_| ())?;
                        let settled_resp_val =
                            serde_json::to_value(&settled.response).map_err(|_| ())?;
                        if last_resp_val != settled_resp_val {
                            return Err(());
                        }

                        if state.messages.len() != durable.messages.len() + 1 {
                            return Err(());
                        }
                        if durable.messages != state.messages[..durable.messages.len()] {
                            return Err(());
                        }
                        let last_msg = state.messages.last().unwrap();
                        if last_msg.role != "assistant"
                            || last_msg.content != Value::from(settled.response.content.clone())
                        {
                            return Err(());
                        }

                        let expected_tools = settled.response.tools().map_err(|_| ())?;
                        let expected_tool_pending = expected_tools.first().map(|c| c.id.clone());
                        if state.tool_pending != expected_tool_pending {
                            return Err(());
                        }
                        if state.completed_tool_ids != durable.completed_tool_ids {
                            return Err(());
                        }

                        (
                            active.id.clone(),
                            settled.response.clone(),
                            settled.estimated_microusd,
                            settled.actual_speed,
                        )
                    };

                    let receipt = db_api::Receipt {
                        state: "recorded",
                        actual_speed,
                        usage: Some(&resp.usage),
                        estimated_microusd: Some(estimated_microusd),
                    };

                    let next_rev = db_api::record(&self.pool, &fence, &active_id, &receipt, &chk)
                        .await
                        .map_err(|_| ())?;

                    cursor.active_attempt = None;
                    cursor.settled_response = None;
                    cursor.revision = next_rev;
                    cursor.last_api_pending = false;
                    cursor.last_durable_state = state.clone();
                    Ok(())
                }
                _ => {
                    let next_rev = db_api::save(&self.pool, &fence, &chk)
                        .await
                        .map_err(|_| ())?;
                    cursor.revision = next_rev;
                    cursor.last_api_pending = after_pending;
                    cursor.last_durable_state = state.clone();
                    Ok(())
                }
            }
        })
    }
}

impl Tools for ApiBotAdapter {
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            match &self.tools {
                Some(tools) => tools.execute(call, cancel).await,
                None => Err("tools unsupported in C1 inert adapter".into()),
            }
        })
    }
}
