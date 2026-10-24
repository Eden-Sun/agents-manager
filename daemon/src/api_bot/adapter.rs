//! Single-turn runner adapter connecting inert execution (A) to SQLite storage (B).
//!
//! Lock hierarchy:
//! 1. Cursor lock (`tokio::sync::Mutex`) protects short DB mutations (reserve, record, save).
//!    It MUST NEVER be held across long async I/O (`Model::complete` network requests or
//!    `Tools::execute` child processes).
//! 2. External stop operations CAS `runs.state` to `stopping` and trigger cancellation.
//!    They NEVER acquire the cursor lock.

use std::sync::Arc;
use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use am_base::db::api_bot::{self as db_api, Checkpoint, Fence};

use super::http;
use super::protocol::{Message, Options, Response, ToolCall};
use super::quote::{QuoteError, QuoteProvider, RequestEnvelope};
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

#[derive(Clone)]
pub(crate) struct AdapterHttpConfig {
    pub(crate) endpoint: String,
    pub(crate) key: String,
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
    cursor: Mutex<CursorState>,
}

struct CursorState {
    revision: i64,
    last_api_pending: bool,
    active_attempt: Option<ActiveAttempt>,
    cached_response: Option<Response>,
}

struct ActiveAttempt {
    id: String,
    envelope: RequestEnvelope,
    permit_available: bool,
}

impl ApiBotAdapter {
    pub(crate) async fn new(
        pool: SqlitePool,
        identity: AdapterIdentity,
        options: Options,
        system: Value,
        limits: AdapterLimits,
        quote_provider: Arc<dyn QuoteProvider>,
        http_cfg: AdapterHttpConfig,
    ) -> Result<Self, http::Error> {
        let fence = Fence {
            session: &identity.session_id,
            bot: &identity.bot_id,
            run: &identity.run_id,
            turn: &identity.turn_id,
            generation: identity.generation,
            revision: 0,
        };

        let initial_state = State::default();
        let state_val = serde_json::to_value(&initial_state).map_err(|_| http::Error::InvalidRequest)?;
        let checkpoint = Checkpoint::new(&state_val).map_err(|_| http::Error::InvalidRequest)?;

        if identity.generation == 1 {
            db_api::create(&pool, &fence, &checkpoint)
                .await
                .map_err(|_| http::Error::InvalidRequest)?;
        }

        Ok(Self {
            pool,
            identity,
            options,
            system,
            limits,
            quote_provider,
            http_cfg,
            cursor: Mutex::new(CursorState {
                revision: 0,
                last_api_pending: false,
                active_attempt: None,
                cached_response: None,
            }),
        })
    }

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
            let (req, attempt_id) = {
                let mut cursor = self.cursor.lock().await;
                let active = cursor.active_attempt.as_mut().ok_or(http::Error::InvalidRequest)?;
                if !active.permit_available {
                    return Err(http::Error::InvalidRequest);
                }
                active.permit_available = false;

                let envelope = RequestEnvelope::new(self.options.clone(), self.system.clone(), messages.to_vec())
                    .map_err(|_| http::Error::InvalidRequest)?;

                if envelope.hash() != active.envelope.hash() {
                    return Err(http::Error::InvalidRequest);
                }

                let req = http::FullMessagesRequest {
                    endpoint: &self.http_cfg.endpoint,
                    key: &self.http_cfg.key,
                    options: active.envelope.options(),
                    system: active.envelope.system(),
                    messages: active.envelope.messages(),
                    timeout: self.http_cfg.timeout,
                    max_response_bytes: self.http_cfg.max_response_bytes,
                };
                (req, active.id.clone())
            };

            let res = http::complete(&req, cancel).await;

            let mut cursor = self.cursor.lock().await;
            match res {
                Ok(resp) => {
                    cursor.cached_response = Some(resp.clone());
                    Ok(resp)
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
                    let unknown_state = State {
                        messages: messages.to_vec(),
                        responses: Vec::new(),
                        api_pending: true,
                        tool_pending: None,
                        completed_tool_ids: std::collections::HashSet::new(),
                    };
                    if let Ok(val) = serde_json::to_value(&unknown_state) {
                        if let Ok(chk) = Checkpoint::new(&val) {
                            let receipt = db_api::Receipt {
                                state: "unknown",
                                actual_speed: None,
                                usage: None,
                                estimated_microusd: None,
                            };
                            if let Ok(next_rev) = db_api::record(&self.pool, &fence, &attempt_id, &receipt, &chk).await {
                                cursor.revision = next_rev;
                                cursor.last_api_pending = true;
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
                (false, true) => {
                    let envelope = RequestEnvelope::new(self.options.clone(), self.system.clone(), state.messages.clone())
                        .map_err(|_| ())?;
                    let quote = self.quote_provider.reserve(&envelope).map_err(|_| ())?;
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
                            cursor.active_attempt = Some(ActiveAttempt {
                                id: attempt_id,
                                envelope,
                                permit_available: true,
                            });
                            Ok(())
                        }
                        db_api::Reservation::Existing { .. } => Err(()),
                    }
                }
                (true, false) => {
                    let active = cursor.active_attempt.take().ok_or(())?;
                    let resp = cursor.cached_response.take().ok_or(())?;

                    let receipt = match self.quote_provider.settle(&active.envelope, &resp) {
                        Ok(usage_quote) => db_api::Receipt {
                            state: "recorded",
                            actual_speed: usage_quote.actual_speed,
                            usage: Some(&resp.usage),
                            estimated_microusd: Some(usage_quote.estimated_microusd),
                        },
                        Err(QuoteError::UntrustedUsage) | Err(QuoteError::Unapproved) | Err(QuoteError::InvalidRequest) => {
                            db_api::Receipt {
                                state: "unknown",
                                actual_speed: None,
                                usage: None,
                                estimated_microusd: None,
                            }
                        }
                    };

                    let next_rev = db_api::record(&self.pool, &fence, &active.id, &receipt, &chk)
                        .await
                        .map_err(|_| ())?;

                    cursor.revision = next_rev;
                    cursor.last_api_pending = false;
                    Ok(())
                }
                _ => {
                    let next_rev = db_api::save(&self.pool, &fence, &chk).await.map_err(|_| ())?;
                    cursor.revision = next_rev;
                    cursor.last_api_pending = after_pending;
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
            super::tools::default_tools().execute(call, cancel).await
        })
    }
}
