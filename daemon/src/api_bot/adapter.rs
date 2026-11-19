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
use super::quote::QuotedRequest;
use super::quote::QuoteProvider;
use super::runner::{Journal, JournalFailure, Model, State, Tools};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdapterIdentity {
    pub(crate) session_id: String,
    pub(crate) bot_id: String,
    pub(crate) run_id: String,
    pub(crate) turn_id: String,
    pub(crate) generation: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AdapterLimits {
    pub(crate) max_checkpoint_bytes: usize,
    pub(crate) max_turn_microusd: i64,
    pub(crate) max_requests_per_hour: i64,
    pub(crate) total_cap_usd: Option<f64>,
    pub(crate) total_cap_since: Option<String>,
    pub(crate) auto_lower_effort_turn_cost_microusd: Option<i64>,
}

impl AdapterLimits {
    pub(crate) fn validate(&self) -> Result<(), http::Error> {
        if self.max_checkpoint_bytes == 0 || self.max_checkpoint_bytes > 8 * 1024 * 1024 {
            return Err(http::Error::InvalidRequest);
        }
        if self.max_turn_microusd <= 0 || self.max_requests_per_hour < 0 {
            return Err(http::Error::InvalidRequest);
        }
        if self.auto_lower_effort_turn_cost_microusd.is_some_and(|t| t < 0) {
            return Err(http::Error::InvalidRequest);
        }
        Ok(())
    }
}

/// 一個 session 要用哪個供應商：識別字（`provider::for_id` 認得的）、它的 session 選項、報價。
/// session 建立時固定；接續時必須與 checkpoint 的標籤一致。
pub(crate) struct AdapterProvider {
    pub(crate) id: &'static str,
    pub(crate) options: super::provider::SessionOptions,
    pub(crate) quote: Arc<dyn QuoteProvider>,
}

impl AdapterProvider {
    /// Claude：`protocol::Options` 無損轉成中立的 session 選項。
    pub(crate) fn anthropic(options: Options, quote: Arc<dyn QuoteProvider>) -> Self {
        Self { id: super::anthropic::ID, options: super::anthropic::session_options(options), quote }
    }
}

pub(super) struct Secret(String);
impl Secret {
    pub(super) fn new(value: String) -> Self { Self(value) }
    pub(super) fn is_empty(&self) -> bool { self.0.trim().is_empty() }
    fn as_str(&self) -> &str { &self.0 }
    /// 測試才讀得到內容（驗證「哪一家的 key 檔」）；正式碼只有 `as_str` 的私有用法，沒有 Debug／Display。
    #[cfg(test)]
    pub(super) fn expose_for_test(&self) -> &str { &self.0 }
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
    system: Value,
    limits: AdapterLimits,
    quote_provider: Arc<dyn QuoteProvider>,
    /// 這個 session 的模型供應商（建立時固定）：編碼請求、送出、錯誤分類與 transcript 編解碼都經過它。
    provider: Arc<dyn super::provider::Provider>,
    /// 這個 session 的 checkpoint 版本標籤：`Some(provider id)`＝v2（新建的 session）；`None`＝沒有標籤的舊 session，
    /// 維持舊格式存回去（位元組不變）。
    session_tag: Option<String>,
    http_cfg: AdapterHttpConfig,
    tools: Option<Arc<dyn Tools>>,
    cursor: Mutex<CursorState>,
    /// 預留被拒絕（還沒送請求）的原因，給 runtime 寫進失敗通知；只存固定代碼。
    refusal: std::sync::Mutex<Option<String>>,
    /// 串流進度（`turn_progress` 幀）的出口；沒設就不回報。
    progress_sink: Option<ProgressSink>,
    progress_seq: std::sync::atomic::AtomicU64,
}

/// 收 `turn_progress` 幀的 JSON（`bot_id`／`run_id`／`turn_id`／`text`／`activity`／`alert`／`revision`／`est_cost_microusd`）。
pub(crate) type ProgressSink = Arc<dyn Fn(Value) + Send + Sync>;

struct CursorState {
    revision: i64,
    last_api_pending: bool,
    last_durable_state: State,
    active_attempt: Option<ActiveAttempt>,
    settled_response: Option<SettledResponse>,
    effective_effort: String,
    effort_auto_lowered: bool,
}

struct SettledResponse {
    response: Response,
    estimated_microusd: i64,
    actual_speed: Option<&'static str>,
    duration_ms: u64,
}

struct ActiveAttempt {
    id: String,
    /// reserve 時報價、算 hash 的那一份已編碼請求；送出的也是它。
    envelope: Arc<dyn QuotedRequest>,
    permit_available: bool,
    key: Option<Secret>,
}

pub(super) use super::anthropic::checkpoint_matches_model;
#[cfg(test)]
pub(super) use super::anthropic::definitive_rejection;

impl ApiBotAdapter {
    /// 接上串流進度出口（runtime 在建好 adapter 後呼叫）。
    pub(crate) fn with_progress_sink(mut self, sink: ProgressSink) -> Self {
        self.progress_sink = Some(sink);
        self
    }

    fn progress_frame(&self, p: &super::sse::StreamProgress) -> Value {
        let est = self.provider.running_cost_microusd(p);
        serde_json::json!({
            "bot_id": self.identity.bot_id,
            "run_id": self.identity.run_id,
            "turn_id": self.identity.turn_id,
            "text": p.text_tail,
            "activity": "",
            "alert": "",
            "revision": self.progress_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1,
            "est_cost_microusd": est,
        })
    }

    fn refuse(&self, reason: impl Into<String>) {
        *self.refusal.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.into());
    }
    /// 取走（並清掉）最近一次預留被拒絕的原因。
    pub(crate) fn take_refusal(&self) -> Option<String> {
        self.refusal.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// 要存成 checkpoint 的 JSON：版本標籤一律照 session 固定的那一個寫（新 session＝`{"v":2,"provider":…}`，舊 session＝不帶）。
    /// runner 手上的 state 帶著別的 provider 標籤＝不是這個 session 的 transcript，拒絕。
    fn checkpoint_value(&self, state: &State) -> Result<Value, ()> {
        if state.provider.is_some() && state.provider != self.session_tag {
            return Err(());
        }
        let mut val = serde_json::to_value(state).map_err(|_| ())?;
        let object = val.as_object_mut().ok_or(())?;
        match &self.session_tag {
            Some(provider) => {
                object.insert("v".into(), Value::from(am_base::db::api_bot::CHECKPOINT_VERSION));
                object.insert("provider".into(), Value::String(provider.clone()));
            }
            None => {
                object.remove("v");
                object.remove("provider");
            }
        }
        Ok(val)
    }

    /// 這個 session 的 transcript 編解碼（runner 用）。
    pub(crate) fn codec(&self) -> &dyn super::provider::Codec {
        self.provider.codec()
    }

    #[cfg(test)]
    pub(crate) fn system(&self) -> &Value {
        &self.system
    }

    /// Create adapter for the first turn (generation 1, fresh session at revision 0). Claude 的便利入口：
    /// 等同 [`Self::new_with`] 帶 `anthropic` 的 [`AdapterProvider`]。
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
        Self::new_with(pool, identity, AdapterProvider::anthropic(options, quote_provider), system, limits, http_cfg, tools).await
    }

    /// 開一個新 session，供應商由 `spec` 指定並從此固定（寫進 checkpoint 的標籤與每筆 attempt 的 `provider`）。
    pub(crate) async fn new_with(
        pool: SqlitePool,
        identity: AdapterIdentity,
        spec: AdapterProvider,
        system: Value,
        limits: AdapterLimits,
        http_cfg: AdapterHttpConfig,
        tools: Option<Arc<dyn Tools>>,
    ) -> Result<Self, http::Error> {
        let AdapterProvider { id: provider_id, options, quote: quote_provider } = spec;
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

        let system_str = match &system {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        // 新 session：固定 provider，checkpoint 從第一份就帶版本標籤（v2）。
        let provider = super::provider::for_id(provider_id, options, quote_provider.clone()).map_err(|_| http::Error::InvalidRequest)?;
        let initial_state = State {
            system: Some(system_str),
            v: Some(am_base::db::api_bot::CHECKPOINT_VERSION),
            provider: Some(provider.id().to_string()),
            ..Default::default()
        };
        let state_val =
            serde_json::to_value(&initial_state).map_err(|_| http::Error::InvalidRequest)?;
        let checkpoint = Checkpoint::new(&state_val).map_err(|_| http::Error::InvalidRequest)?;

        db_api::create(&pool, &fence, &checkpoint)
            .await
            .map_err(|_| http::Error::InvalidRequest)?;

        let effective_effort = provider.effort().to_string();
        Ok(Self {
            pool,
            identity,
            system,
            limits,
            // session 的標籤就是它的供應商（之後每一份 checkpoint 都照這個寫）。
            session_tag: Some(provider.id().to_string()),
            provider,
            quote_provider,
            http_cfg,
            tools,
            refusal: std::sync::Mutex::new(None),
            progress_sink: None,
            progress_seq: std::sync::atomic::AtomicU64::new(0),
            cursor: Mutex::new(CursorState {
                revision: 0,
                last_api_pending: false,
                last_durable_state: initial_state,
                active_attempt: None,
                settled_response: None,
                effective_effort,
                effort_auto_lowered: false,
            }),
        })
    }

    /// Rebind an existing settled session to a new run / turn, loading durable state. Claude 的便利入口：
    /// 等同 [`Self::rebind_with`] 帶 `anthropic` 的 [`AdapterProvider`]。
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
        Self::rebind_with(pool, old_fence, old_revision, new_identity, AdapterProvider::anthropic(options, quote_provider), system, limits, http_cfg, tools).await
    }

    /// 接續既有 session。session 的供應商在建立時就固定：checkpoint 標的（沒有標籤＝Claude）必須就是 `spec` 的那一家，
    /// 不一致就拒絕，而且不改任何 durable 狀態（owner／revision 不動）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn rebind_with(
        pool: SqlitePool,
        old_fence: &AdapterIdentity,
        old_revision: i64,
        new_identity: AdapterIdentity,
        spec: AdapterProvider,
        system: Value,
        limits: AdapterLimits,
        http_cfg: AdapterHttpConfig,
        tools: Option<Arc<dyn Tools>>,
    ) -> Result<(Self, State), http::Error> {
        let AdapterProvider { id: provider_id, options, quote: quote_provider } = spec;
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

        // Thinking signatures belong to their originating model. Reject before changing durable owner/revision.
        // The prior turn is already terminal; load() requires a live turn and cannot be used here.
        // Rebind below revalidates this exact revision before changing ownership.
        let expiry = am_base::db::ts_sql("expires_at");
        let previous: Option<String> = sqlx::query_scalar(&format!("SELECT checkpoint_json FROM api_bot_sessions WHERE id=? AND bot_id=? AND owner_run_id=? AND owner_turn_id=? AND generation=? AND revision=? AND retired_at IS NULL AND {expiry}>? AND length(CAST(checkpoint_json AS BLOB))<=?"))
            .bind(old_fence.session_id.as_str()).bind(old_fence.bot_id.as_str())
            .bind(old_fence.run_id.as_str()).bind(old_fence.turn_id.as_str())
            .bind(old_fence.generation).bind(old_revision).bind(am_base::db::now())
            .bind(am_base::db::api_bot::MAX_CHECKPOINT_BYTES as i64)
            .fetch_optional(&pool).await.map_err(|_| http::Error::InvalidRequest)?;
        let previous: State = serde_json::from_str(&previous.ok_or(http::Error::InvalidRequest)?)
            .map_err(|_| http::Error::InvalidRequest)?;
        // session 的 provider 在建立時就固定：依 checkpoint 的標籤取回同一個（沒有標籤＝Claude）；不認得就拒絕，不改任何 durable 狀態。
        if previous.provider_id() != provider_id {
            return Err(http::Error::InvalidRequest);
        }
        let provider = super::provider::for_id(provider_id, options, quote_provider.clone()).map_err(|_| http::Error::InvalidRequest)?;
        let session_tag = previous.provider.clone();
        if !provider.checkpoint_matches_model(&previous) { return Err(http::Error::InvalidRequest); }

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
        let mut loaded_state: State =
            serde_json::from_value(loaded_chk.value()).map_err(|_| http::Error::InvalidRequest)?;
        if loaded_state.provider != session_tag {
            return Err(http::Error::InvalidRequest);
        }
        provider.codec().prune_after_compaction(&mut loaded_state);

        let effective_system = if let Some(ref stored) = loaded_state.system {
            Value::String(stored.clone())
        } else {
            let system_str = match &system {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            loaded_state.system = Some(system_str);
            system
        };

        let effective_effort = provider.effort().to_string();
        let adapter = Self {
            pool,
            identity: new_identity,
            system: effective_system,
            limits,
            provider,
            session_tag,
            quote_provider,
            http_cfg,
            tools,
            refusal: std::sync::Mutex::new(None),
            progress_sink: None,
            progress_seq: std::sync::atomic::AtomicU64::new(0),
            cursor: Mutex::new(CursorState {
                revision: new_rev,
                last_api_pending: false,
                last_durable_state: loaded_state.clone(),
                active_attempt: None,
                settled_response: None,
                effective_effort,
                effort_auto_lowered: false,
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
            let (envelope, attempt_id, key, effective_effort, effort_auto_lowered) = {
                let mut cursor = self.cursor.lock().await;
                let effective_effort = cursor.effective_effort.clone();
                let effort_auto_lowered = cursor.effort_auto_lowered;
                let active = cursor
                    .active_attempt
                    .as_mut()
                    .ok_or(http::Error::InvalidRequest)?;
                if !active.permit_available {
                    return Err(http::Error::InvalidRequest);
                }

                let reconstructed = self.provider.encode(&effective_effort, &self.system, messages, None)
                    .map_err(|_| http::Error::InvalidRequest)?;

                if reconstructed.hash() != active.envelope.hash() {
                    return Err(http::Error::InvalidRequest);
                }
                // Invalid input does not spend the matching reservation's permit.
                // Consume atomically only after identity validation, still before any HTTP.
                let key = active.key.take().ok_or(http::Error::InvalidRequest)?;
                active.permit_available = false;

                (
                    active.envelope.clone(),
                    active.id.clone(),
                    key,
                    effective_effort,
                    effort_auto_lowered,
                )
            };

            // 送出的是 reserve 時報價、算 hash 的那一份已編碼請求（`active.envelope`），不重新組 body。
            let transport = || super::provider::Transport {
                endpoint: &self.http_cfg.endpoint,
                key: key.as_str(),
                timeout: self.http_cfg.timeout,
                max_response_bytes: self.http_cfg.max_response_bytes,
            };

            let req_start = std::time::Instant::now();
            let res = match &self.progress_sink {
                Some(sink) => {
                    let report = |p: &super::sse::StreamProgress| sink(self.progress_frame(p));
                    self.provider.send(transport(), envelope.as_ref(), cancel, Some(&report)).await
                }
                None => self.provider.send(transport(), envelope.as_ref(), cancel, None).await,
            };
            let duration_ms = req_start.elapsed().as_millis() as u64;
            drop(key);

            let mut cursor = self.cursor.lock().await;
            match res {
                Ok(resp) => {
                    // HTTP 已成功收到完整回應：帳是確定的。內容不合法（工具解析失敗、一批超過 32 個、
                    // 工具 id 重複、模型不符）或用量不合規則時，也要把這次 attempt 結成 `recorded`、收掉 api_pending，
                    // 之後的 prompt 才送得出去；transcript 維持送出前的 durable 狀態（不留半截 assistant tool batch）。
                    let content_problem = self.provider.invalid_content_reason(&resp, &cursor.last_durable_state);
                    let quote_res = self.quote_provider.settle(envelope.as_ref(), &resp);
                    let valid_quote = match &quote_res {
                        Ok(uq)
                            if uq.estimated_microusd >= 0
                                && uq
                                    .actual_speed
                                    .is_none_or(|s| matches!(s, "fast" | "standard")) =>
                        {
                            Some(uq.clone())
                        }
                        _ => None,
                    };

                    if let (Some(uq), None) = (&valid_quote, &content_problem) {
                        let mut resp = resp;
                        if let Some(obj) = resp.usage.as_object_mut() {
                            obj.insert("effort".to_string(), Value::String(effective_effort));
                            if effort_auto_lowered {
                                obj.insert("auto_lowered_effort".to_string(), Value::Bool(true));
                            }
                        }
                        cursor.settled_response = Some(SettledResponse {
                            response: resp.clone(),
                            estimated_microusd: uq.estimated_microusd,
                            actual_speed: uq.actual_speed,
                            duration_ms,
                        });
                        Ok(resp)
                    } else {
                        let reason = match &content_problem {
                            Some(cp) => format!("content: {cp}"),
                            None => {
                                let problem = self.provider.usage_problem(envelope.as_ref(), &resp);
                                match problem {
                                    Some(p) => format!("untrusted_usage: {p}"),
                                    None => format!("untrusted_usage: {:?}", quote_res.as_ref().err()),
                                }
                            }
                        };
                        let record = am_base::db::api_bot::RejectedResponseRecord {
                            bot_id: &self.identity.bot_id,
                            turn_id: &self.identity.turn_id,
                            attempt_id: &attempt_id,
                            created_at: &crate::db::now(),
                            reason: &reason,
                            response_model: &resp.model,
                            stop_reason: &resp.stop_reason,
                            usage: &resp.usage,
                            content_blocks: &resp.content,
                        };
                        let _ = am_base::db::api_bot::record_rejected_response(&self.pool, &record).await;
                        // 有可信用量就照實算；用量本身不可信時費用不明，按預留額（上限）計，usage 不存。
                        let (receipt_speed, receipt_usage, estimated) = match &valid_quote {
                            Some(uq) => (uq.actual_speed, Some(&resp.usage), uq.estimated_microusd),
                            None => (
                                None,
                                None,
                                self.quote_provider.reserve(envelope.as_ref()).map(|q| q.reserved_microusd).unwrap_or(0).max(0),
                            ),
                        };
                        tracing::warn!(
                            reason = %reason,
                            stop_reason = %resp.stop_reason,
                            response_model = %resp.model,
                            estimated_microusd = estimated,
                            usage = %resp.usage,
                            "api bot response rejected after HTTP success; attempt recorded"
                        );
                        let fence = Fence {
                            session: &self.identity.session_id,
                            bot: &self.identity.bot_id,
                            run: &self.identity.run_id,
                            turn: &self.identity.turn_id,
                            generation: self.identity.generation,
                            revision: cursor.revision,
                        };
                        let mut next_state = cursor.last_durable_state.clone();
                        next_state.api_pending = false;
                        let val = self.checkpoint_value(&next_state)
                            .map_err(|_| http::Error::InvalidResponseDetail(reason.clone()))?;
                        let chk =
                            Checkpoint::new(&val).map_err(|_| http::Error::InvalidResponseDetail(reason.clone()))?;
                        let receipt = db_api::Receipt {
                            state: "recorded",
                            actual_speed: receipt_speed,
                            usage: receipt_usage,
                            estimated_microusd: Some(estimated),
                        };
                        let next_rev =
                            db_api::record(&self.pool, &fence, &attempt_id, &receipt, &chk)
                                .await
                                .map_err(|_| http::Error::InvalidResponseDetail(reason.clone()))?;

                        cursor.revision = next_rev;
                        cursor.last_api_pending = false;
                        cursor.last_durable_state = next_state;
                        cursor.active_attempt = None;

                        Err(http::Error::InvalidResponseDetail(reason))
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
                    // 明確被 API 拒絕（4xx、429、529）＝這次請求沒有被處理、不計費：結果是已知的，
                    // 照 `record` 的規則要把 pending 標記收掉（receipt `failed`、費用 0）。否則 checkpoint 永遠停在
                    // api_pending，這顆 bot 之後的每一則都被當成「上一次還沒結算」拒收，重啟時還會讓整個 API 啟動閘隔離。
                    // 逾時、連線中斷、回應讀壞才是真的不知道有沒有處理，維持 `unknown`。
                    let rejected = self.provider.definitive_rejection(&err);
                    let mut next_state = cursor.last_durable_state.clone();
                    next_state.api_pending = !rejected;

                    if let Ok(val) = self.checkpoint_value(&next_state) {
                        if let Ok(chk) = Checkpoint::new(&val) {
                            let receipt = db_api::Receipt {
                                state: if rejected { "failed" } else { "unknown" },
                                actual_speed: None,
                                usage: None,
                                estimated_microusd: rejected.then_some(0),
                            };
                            if let Ok(next_rev) =
                                db_api::record(&self.pool, &fence, &attempt_id, &receipt, &chk)
                                    .await
                            {
                                cursor.revision = next_rev;
                                cursor.last_api_pending = !rejected;
                                cursor.last_durable_state = next_state;
                            }
                        }
                    }
                    Err(err)
                }
            }
        })
    }
}

/// 一次 `save` 的診斷資料：只放 id、數字與 reason 代碼，**不放** prompt、工具輸出、金鑰或 checkpoint 內容。
#[derive(Default)]
pub(super) struct SaveDiag {
    pub(super) bot_id: String,
    pub(super) turn_id: String,
    pub(super) generation: i64,
    pub(super) revision: Option<i64>,
    pub(super) pending_before: Option<bool>,
    pub(super) pending_after: Option<bool>,
    pub(super) bytes: Option<u64>,
    pub(super) limit: Option<u64>,
    pub(super) sqlite_code: Option<String>,
}

impl SaveDiag {
    fn new(bot_id: &str, turn_id: &str, generation: i64) -> Self {
        Self { bot_id: bot_id.to_string(), turn_id: turn_id.to_string(), generation, ..Self::default() }
    }

    /// 這一行就是統一出口記的內容（`key=value`，放在 `log` 欄位）：只有 id、數字與 reason 代碼。
    pub(super) fn log_line(&self, failure: JournalFailure) -> String {
        format!(
            "reason={} bot_id={} turn_id={} generation={} revision={:?} pending_before={:?} pending_after={:?} bytes={:?} limit={:?} sqlite_code={:?}",
            failure.code(),
            self.bot_id,
            self.turn_id,
            self.generation,
            self.revision,
            self.pending_before,
            self.pending_after,
            self.bytes,
            self.limit,
            self.sqlite_code
        )
    }

    /// 統一出口：每次失敗只記一行。
    fn emit(&self, failure: JournalFailure) {
        tracing::warn!(log = %self.log_line(failure), "API bot checkpoint not saved");
    }
}

/// 把 DB 錯誤分成圍籬、revision 與真正的寫入失敗；SQLite 錯誤只附上代碼。
pub(super) fn db_failure(diag: &mut SaveDiag, e: &anyhow::Error) -> JournalFailure {
    if let Some(sqlx::Error::Database(db)) = e.downcast_ref::<sqlx::Error>() {
        diag.sqlite_code = db.code().map(|c| c.into_owned());
    }
    if e.downcast_ref::<db_api::StaleFence>().is_some() {
        JournalFailure::FenceMismatch
    } else if e.downcast_ref::<db_api::StaleRevision>().is_some() {
        JournalFailure::StaleRevision
    } else {
        JournalFailure::DbWriteFailed
    }
}

impl Journal for ApiBotAdapter {
    fn save<'a>(&'a self, state: &'a State) -> BoxFuture<'a, Result<(), JournalFailure>> {
        Box::pin(async move {
            let (result, diag) = self.save_observed(state).await;
            if let Err(failure) = result {
                diag.emit(failure);
            }
            result
        })
    }
}

impl ApiBotAdapter {
    /// `save` 的可觀察版本：回結果與這一次的診斷資料（測試用來確認統一出口記的內容）。
    pub(super) async fn save_observed(&self, state: &State) -> (Result<(), JournalFailure>, SaveDiag) {
        let mut diag = SaveDiag::new(&self.identity.bot_id, &self.identity.turn_id, self.identity.generation);
        let result = self.save_checkpoint(state, &mut diag).await;
        (result, diag)
    }

    /// `Journal::save` 的本體：每個失敗都回具名原因；log 由 `save` 統一記一次（見 [`SaveDiag::emit`]）。
    async fn save_checkpoint(&self, state: &State, diag: &mut SaveDiag) -> Result<(), JournalFailure> {
            if state.provider.is_some() && state.provider != self.session_tag {
                return Err(JournalFailure::CheckpointProviderMismatch);
            }
            let val = self.checkpoint_value(state).map_err(|_| JournalFailure::CheckpointSerialize)?;
            let chk = Checkpoint::new(&val).map_err(|_| JournalFailure::CheckpointSerialize)?;
            let raw_len = serde_json::to_vec(&val).map_err(|_| JournalFailure::CheckpointSerialize)?.len();
            diag.bytes = Some(raw_len as u64);
            diag.limit = Some(self.limits.max_checkpoint_bytes as u64);
            if raw_len > self.limits.max_checkpoint_bytes {
                return Err(JournalFailure::CheckpointTooLarge);
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
            diag.revision = Some(cursor.revision);
            diag.pending_before = Some(before_pending);
            diag.pending_after = Some(after_pending);

            match (before_pending, after_pending) {
                (true, true) => {
                    // While HTTP is active/pending, disallow arbitrary mutation of messages/responses.
                    // Only allow exactly matching the last durable state (idempotent checkpoint), without granting permits.
                    let durable_val =
                        self.checkpoint_value(&cursor.last_durable_state).map_err(|_| JournalFailure::CheckpointSerialize)?;
                    if val != durable_val {
                        return Err(JournalFailure::PendingStateChanged);
                    }
                    Ok(())
                }
                (false, true) => {
                    if let Some(threshold) = self.limits.auto_lower_effort_turn_cost_microusd {
                        if threshold > 0 {
                            let spent: i64 = sqlx::query_scalar(
                                "SELECT COALESCE(SUM(COALESCE(estimated_microusd, reserved_microusd)), 0)
                                 FROM api_bot_attempts WHERE turn_id=? AND state IN ('recorded', 'reserved')",
                            )
                            .bind(&self.identity.turn_id)
                            .fetch_one(&self.pool)
                            .await
                            .unwrap_or(0);

                            if spent >= threshold {
                                let configured = self.provider.effort();
                                let lowered = self.provider.lower_effort(configured);
                                if lowered != configured && cursor.effective_effort != lowered {
                                    tracing::info!(
                                        bot_id = %self.identity.bot_id,
                                        turn_id = %self.identity.turn_id,
                                        from = %configured,
                                        to = %lowered,
                                        spent_microusd = spent,
                                        threshold_microusd = threshold,
                                        "auto lowered effort due to turn cost threshold"
                                    );
                                }
                                cursor.effort_auto_lowered = lowered != configured;
                                cursor.effective_effort = lowered;
                            } else {
                                cursor.effective_effort = self.provider.effort().to_string();
                                cursor.effort_auto_lowered = false;
                            }
                        }
                    }

                    let envelope = self.provider
                        .encode(&cursor.effective_effort, &self.system, &state.messages, state.responses.last())
                        .map_err(|_| JournalFailure::EncodeFailed)?;
                    let key = self.http_cfg.key_source.snapshot().await.map_err(|_| {
                        tracing::info!(reason="api_key_unavailable", "API key snapshot unavailable");
                        self.refuse("api_bot_key_unavailable");
                        JournalFailure::Refused
                    })?;
                    if key.is_empty() { tracing::info!(reason="api_key_unavailable", "API key snapshot unavailable"); self.refuse("api_bot_key_unavailable"); return Err(JournalFailure::Refused); }
                    let quote = self.quote_provider.reserve(envelope.as_ref()).map_err(|_| {
                        self.refuse("api_bot_quote_refused");
                        JournalFailure::InvalidQuote
                    })?;
                    if quote.reserved_microusd <= 0 || quote.pricing_version.is_empty() {
                        return Err(JournalFailure::InvalidQuote);
                    }

                    // 帳本範圍跟著 session 的供應商：每筆 attempt 記它的 `provider`，每小時請求數與總上限都只算同一家的。
                    let ledger_provider = self.provider.id();
                    let claude = ledger_provider == db_api::PROVIDER_ANTHROPIC;
                    let total_cap = match (self.limits.total_cap_usd, &self.limits.total_cap_since) {
                        (Some(cap_usd), Some(since)) if cap_usd >= 0.0 && !since.is_empty() => Some((cap_usd, since.as_str())),
                        _ => None,
                    };
                    // Claude：照舊在預留前先查（只算 anthropic 的 attempt；別家的花費不佔 Claude 的額）。
                    if let (true, Some((cap_usd, since))) = (claude, total_cap) {
                        {
                            let spent_microusd: i64 = sqlx::query_scalar(
                                "SELECT COALESCE(SUM(
                                    CASE WHEN state='recorded' THEN COALESCE(estimated_microusd, 0)
                                         WHEN state='reserved' THEN reserved_microusd
                                         WHEN state='unknown' THEN COALESCE(estimated_microusd, reserved_microusd)
                                         ELSE 0 END
                                 ), 0)
                                 FROM api_bot_attempts
                                 WHERE provider = ? AND created_at >= ? AND state IN ('recorded', 'reserved', 'unknown')",
                            )
                            .bind(ledger_provider)
                            .bind(since)
                            .fetch_one(&self.pool)
                            .await
                            // 讀不到已花金額就當成超過上限（fail-closed），不冒險送出請求。
                            .unwrap_or(i64::MAX);

                            let cap_microusd = (cap_usd * 1_000_000.0).round() as i64;
                            let total_with_reservation = spent_microusd.saturating_add(quote.reserved_microusd.max(0));
                            if total_with_reservation > cap_microusd {
                                let spent_usd = spent_microusd as f64 / 1_000_000.0;
                                let cap_str = crate::claude_api_cap::format_usd_pretty(cap_usd);
                                let spent_str = crate::claude_api_cap::format_usd_pretty(spent_usd);
                                self.refuse(format!("api_total_cap_reached:cap={cap_str}:spent={spent_str}"));
                                return Err(JournalFailure::Refused);
                            }
                        }
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

                    // 其他供應商：總上限在預留的同一個寫交易裡檢查（兩筆並發只有一筆拿得到最後的額度）。
                    let scope = if claude {
                        db_api::ProviderScope::ANTHROPIC
                    } else {
                        db_api::ProviderScope {
                            provider: ledger_provider,
                            service_tier: Some(envelope.speed()),
                            total_cap: total_cap.map(|(cap_usd, since)| db_api::TotalCap { since, cap_microusd: (cap_usd * 1_000_000.0).round() as i64 }),
                        }
                    };
                    let res = db_api::reserve_scoped(
                        &self.pool,
                        &fence,
                        &attempt,
                        &chk,
                        self.limits.max_turn_microusd,
                        self.limits.max_requests_per_hour,
                        &scope,
                    )
                    .await
                    .map_err(|e| {
                        if let Some(cap) = e.downcast_ref::<db_api::TotalCapRefused>() {
                            let usd = |microusd: i64| crate::claude_api_cap::format_usd_pretty(microusd as f64 / 1_000_000.0);
                            self.refuse(format!("api_total_cap_reached:cap={}:spent={}", usd(cap.cap_microusd), usd(cap.spent_microusd)));
                            return JournalFailure::Refused;
                        }
                        match e.downcast_ref::<db_api::ReserveRefused>() {
                            Some(db_api::ReserveRefused::TurnBudget) => {
                                self.refuse("api_bot_turn_cost_cap_exceeded");
                                JournalFailure::Refused
                            }
                            Some(db_api::ReserveRefused::HourlyLimit) => {
                                self.refuse("api_bot_hourly_limit_reached");
                                JournalFailure::Refused
                            }
                            None => db_failure(diag, &e),
                        }
                    })?;

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
                        db_api::Reservation::Existing { .. } => Err(JournalFailure::AttemptExists),
                    }
                }
                (true, false) => {
                    let (active_id, resp, estimated_microusd, actual_speed, duration_ms) = {
                        let active = cursor.active_attempt.as_ref().ok_or(JournalFailure::ResponseTransitionMismatch)?;
                        let settled = cursor.settled_response.as_ref().ok_or(JournalFailure::ResponseTransitionMismatch)?;

                        // Verify legal increment from last durable state to state with assistant response
                        let durable = &cursor.last_durable_state;
                        if state.responses.len() != durable.responses.len() + 1 {
                            return Err(JournalFailure::ResponseTransitionMismatch);
                        }

                        // Response has no PartialEq; compare prefix via JSON values
                        let durable_resp_val =
                            serde_json::to_value(&durable.responses).map_err(|_| JournalFailure::ResponseTransitionMismatch)?;
                        let state_prefix_resp_val =
                            serde_json::to_value(&state.responses[..durable.responses.len()])
                                .map_err(|_| JournalFailure::ResponseTransitionMismatch)?;
                        if durable_resp_val != state_prefix_resp_val {
                            return Err(JournalFailure::ResponseTransitionMismatch);
                        }

                        let last_resp_val = serde_json::to_value(state.responses.last().unwrap())
                            .map_err(|_| JournalFailure::ResponseTransitionMismatch)?;
                        let settled_resp_val =
                            serde_json::to_value(&settled.response).map_err(|_| JournalFailure::ResponseTransitionMismatch)?;
                        if last_resp_val != settled_resp_val {
                            return Err(JournalFailure::ResponseTransitionMismatch);
                        }

                        // transcript 只能多出「這份回應照 session 的 codec 收進去」的那一段（Claude：一則 assistant 訊息；
                        // OpenAI：回應的每個 output item 各一筆）。由 codec 從上一份 durable 狀態算出預期值再整份比對。
                        let mut expected = durable.clone();
                        let accepted = self.provider.codec().accept(&mut expected, settled.response.clone()).map_err(|_| JournalFailure::ResponseTransitionMismatch)?;
                        if state.messages != expected.messages {
                            return Err(JournalFailure::ResponseTransitionMismatch);
                        }
                        let expected_tool_pending = accepted.calls.first().map(|c| c.id.clone());
                        if state.tool_pending != expected_tool_pending {
                            return Err(JournalFailure::ResponseTransitionMismatch);
                        }
                        if state.completed_tool_ids != durable.completed_tool_ids {
                            return Err(JournalFailure::ResponseTransitionMismatch);
                        }

                        (
                            active.id.clone(),
                            settled.response.clone(),
                            settled.estimated_microusd,
                            settled.actual_speed,
                            settled.duration_ms,
                        )
                    };

                    let mut usage_with_meta = resp.usage.clone();
                    if let Some(obj) = usage_with_meta.as_object_mut() {
                        obj.insert("duration_ms".into(), serde_json::json!(duration_ms));
                        obj.insert("stop_reason".into(), serde_json::json!(resp.stop_reason));
                    }

                    let receipt = db_api::Receipt {
                        state: "recorded",
                        actual_speed,
                        usage: Some(&usage_with_meta),
                        estimated_microusd: Some(estimated_microusd),
                    };

                    let next_rev = db_api::record(&self.pool, &fence, &active_id, &receipt, &chk)
                        .await
                        .map_err(|e| db_failure(diag, &e))?;

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
                        .map_err(|e| db_failure(diag, &e))?;
                    cursor.revision = next_rev;
                    cursor.last_api_pending = after_pending;
                    cursor.last_durable_state = state.clone();
                    Ok(())
                }
            }
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
