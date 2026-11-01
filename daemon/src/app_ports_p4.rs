//! P4 composition adapters from the frozen am-ports turn capability to lifecycle operations.
//!
//! This is the only place the P4 port adapter knows the concrete App. Feature callers can depend
//! on `am_ports::TurnControl` without importing lifecycle internals or the App state type.

use am_core::{
    BotId, EventEnvelope, EventSeq, HostFence as PortHostFence, NoticeRequest, PaneReadSource,
    PortError, PromptRequest, RunId, SessionId, TurnError, TurnEvent, TurnId,
};
use am_ports::{
    CodexRolloutAccess, EventSink, HerdrPort, RunPaneReader, StyledRunPaneReader,
    SystemMessageWriter, TurnControl, TurnEvents,
};
use std::future::Future;
use std::sync::Arc;

use crate::lifecycle::{self, LcError};
use crate::state::App;

/// Composition-root wrapper that exposes only the frozen turn operations to feature callers.
pub struct AppTurnControl<'a> {
    app: &'a Arc<App>,
}

impl<'a> AppTurnControl<'a> {
    pub fn new(app: &'a Arc<App>) -> Self {
        Self { app }
    }
}

impl TurnControl for AppTurnControl<'_> {
    fn start_bot(&self, bot: BotId) -> impl Future<Output = Result<RunId, TurnError>> + Send + '_ {
        async move {
            lifecycle::start_bot(self.app, &bot)
                .await
                .map_err(|err| map_error(&bot, err))
        }
    }

    fn stop_bot(
        &self,
        bot: BotId,
        _reason: Option<String>,
    ) -> impl Future<Output = Result<(), TurnError>> + Send + '_ {
        async move {
            lifecycle::stop_bot(self.app, &bot)
                .await
                .map(|_| ())
                .map_err(|err| map_error(&bot, err))
        }
    }

    fn send_prompt(
        &self,
        request: PromptRequest,
    ) -> impl Future<Output = Result<TurnId, TurnError>> + Send + '_ {
        async move {
            let request_id = request.client_request_id.unwrap_or_else(crate::db::ulid);
            let sent = match request.expected_run_id.as_deref() {
                Some(run_id) => {
                    lifecycle::prompt_expect_run(self.app, &request.bot_id, &request.text, &request_id, run_id).await
                }
                None => lifecycle::prompt(self.app, &request.bot_id, &request.text, &request_id).await,
            };
            sent.map(|out| out.turn_id).map_err(|err| map_error(&request.bot_id, err))
        }
    }

    fn compact_bot(
        &self,
        bot: BotId,
        expected_run_id: Option<RunId>,
    ) -> impl Future<Output = Result<(), TurnError>> + Send + '_ {
        async move {
            lifecycle::compact(self.app, &bot, expected_run_id.as_deref())
                .await
                .map(|_| ())
                .map_err(|err| map_error(&bot, err))
        }
    }

    fn interrupt(
        &self,
        bot: BotId,
        _reason: String,
    ) -> impl Future<Output = Result<(), TurnError>> + Send + '_ {
        async move {
            lifecycle::interrupt_turn(self.app, &bot, None)
                .await
                .map_err(|err| map_error(&bot, err))
        }
    }

    fn queue_notice(
        &self,
        notice: NoticeRequest,
    ) -> impl Future<Output = Result<(), TurnError>> + Send + '_ {
        async move {
            let request_id = notice.dedupe_key.unwrap_or_else(crate::db::ulid);
            lifecycle::prompt_from_api_queue_if_busy(
                self.app,
                &notice.bot_id,
                &notice.text,
                &request_id,
                &[],
                lifecycle::RelaySrc::daemon_notice(Some("daemon")),
                false,
                None,
            )
            .await
            .map(|_| ())
            .map_err(|err| map_error(&notice.bot_id, err))
        }
    }
}

fn map_error(bot: &str, err: LcError) -> TurnError {
    match err {
        LcError::NotFound(what) if what == "bot" => TurnError::BotNotFound(bot.to_owned()),
        LcError::NotFound(what) => TurnError::Failed(format!("not found: {what}")),
        LcError::NotFoundValue(value) => TurnError::Failed(value.to_string()),
        LcError::Conflict(value) => TurnError::Busy(value.to_string()),
        LcError::Upstream(message) => TurnError::Unavailable(message),
        LcError::Bad(message) => TurnError::InvalidRequest(message),
        LcError::BadValue(value) | LcError::Unprocessable(value) | LcError::Forbidden(value) => {
            TurnError::InvalidRequest(value.to_string())
        }
        LcError::Unavailable(value) | LcError::Uncommitted(value) => {
            TurnError::Unavailable(value.to_string())
        }
    }
}

/// App-side implementation of the host-fenced Herdr operations. The v1.2 methods carry a pane id
/// because the Herdr RPCs are pane-scoped; the older v1.1 methods stay available but cannot safely
/// choose a target pane, so this adapter rejects them instead of guessing.
pub struct AppHerdrPort<'a> {
    app: &'a Arc<App>,
}

impl<'a> AppHerdrPort<'a> {
    pub fn new(app: &'a Arc<App>) -> Self {
        Self { app }
    }

    /// Keep run-scoped reads on the same handoff/session selection path for plain and styled
    /// readers. Unavailable DB/session state has historically meant "nothing to read" here.
    async fn run_pane_client(
        &self,
        bot: &str,
        run_session: Option<&str>,
    ) -> Option<crate::herdr::HerdrClient> {
        if !matches!(crate::handoff::bot_handed_off_to(&self.app.db, bot).await, Ok(None)) {
            return None;
        }
        let Ok(host) = crate::db::bot_host(&self.app.db, bot).await else {
            return None;
        };
        let session = if let Some(session) = run_session.filter(|session| !session.is_empty()) {
            session.to_string()
        } else {
            let Ok(Some(bot_row)) = crate::db::bot(&self.app.db, bot).await else {
                return None;
            };
            self.app.session_for_bot(&bot_row, &host).await?
        };
        self.app.herdr_for_session(&host, &session).await
    }

    /// Compatibility entry for existing run-based callers until HostRuntime owns minting the
    /// core fence. It keeps App's existing run/session selection and uses the same pane.read RPC.
    pub async fn read_run_pane(
        &self,
        run: &crate::db::Run,
        pane_id: &str,
        source: PaneReadSource,
        lines: u32,
    ) -> anyhow::Result<Option<String>> {
        let Some(client) = self.app.herdr_for_run(run).await else {
            return Ok(None);
        };
        Ok(Some(
            client
                .pane_read(pane_id, source.as_str(), lines)
                .await?
                .text,
        ))
    }

    async fn actual_fence(
        &self,
        fence: &PortHostFence,
    ) -> Result<crate::hosts::HostFence, PortError> {
        let conn = self.app.hosts.get(&fence.host_id).await.ok_or_else(|| {
            PortError::NotFound(format!("host `{}` is not configured", fence.host_id))
        })?;
        self.app
            .hosts
            .fence_for_generation(&conn, fence.generation)
            .await
            .ok_or_else(|| PortError::Conflict(format!("host `{}` fence is stale", fence.host_id)))
    }

    async fn fenced_client(
        &self,
        fence: &PortHostFence,
        session: &str,
    ) -> Result<(crate::hosts::HostFence, crate::herdr::HerdrClient), PortError> {
        let actual = self.actual_fence(fence).await?;
        let client = self
            .app
            .herdr_for_host_fence(&actual, session)
            .await
            .ok_or_else(|| {
                PortError::Conflict(format!(
                    "session `{session}` is not current for host `{}`",
                    fence.host_id
                ))
            })?;
        Ok((actual, client))
    }
}

/// App-side compatibility path for run-scoped pane reads that have not yet been given a host fence.
impl RunPaneReader for AppHerdrPort<'_> {
    fn read_run_pane<'a>(
        &'a self,
        bot: &'a BotId,
        run_session: Option<&'a SessionId>,
        pane_id: &'a str,
        source: PaneReadSource,
        lines: u32,
    ) -> impl Future<Output = Result<Option<String>, PortError>> + Send + 'a {
        async move {
            let Some(client) = self
                .run_pane_client(bot, run_session.map(String::as_str))
                .await
            else {
                return Ok(None);
            };
            client
                .pane_read(pane_id, source.as_str(), lines)
                .await
                .map(|read| Some(read.text))
                .map_err(|error| PortError::Unavailable(error.to_string()))
        }
    }
}

impl StyledRunPaneReader for AppHerdrPort<'_> {
    fn read_styled_run_pane<'a>(
        &'a self,
        bot: &'a BotId,
        run_session: Option<&'a SessionId>,
        pane_id: &'a str,
        source: PaneReadSource,
        lines: u32,
    ) -> impl Future<Output = Result<Option<String>, PortError>> + Send + 'a {
        async move {
            let Some(client) = self
                .run_pane_client(bot, run_session.map(String::as_str))
                .await
            else {
                return Ok(None);
            };
            crate::lifecycle::read_styled(&client, pane_id, source.as_str(), lines)
                .await
                .map(Some)
                .map_err(|error| PortError::Unavailable(error.to_string()))
        }
    }
}

/// Compatibility entry point retained for non-P4 callers while the screen probe moves behind its port.
pub async fn shows_login_problem(app: &Arc<App>, run: &crate::db::Run) -> Option<bool> {
    let reader = AppHerdrPort::new(app);
    crate::tui_prompts::shows_login_problem_with_reader(
        &reader,
        &run.bot_id,
        run.herdr_session.as_ref(),
        run.pane_id.as_deref(),
    )
    .await
}

impl HerdrPort for AppHerdrPort<'_> {
    fn ping<'a>(
        &'a self,
        fence: &'a PortHostFence,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            let actual = self.actual_fence(fence).await?;
            let client = actual.conn().client.clone();
            let result = self
                .app
                .hosts
                .run_if_current(&actual, client.ping())
                .await
                .ok_or_else(|| {
                    PortError::Conflict(format!("host `{}` fence expired", fence.host_id))
                })?;
            result
                .map(|_| ())
                .map_err(|error| PortError::Unavailable(error.to_string()))
        }
    }

    fn screen_text<'a>(
        &'a self,
        _fence: &'a PortHostFence,
        _session: &'a String,
    ) -> impl Future<Output = Result<String, PortError>> + Send + 'a {
        async {
            Err(PortError::InvalidInput(
                "screen_text needs a pane id; use read_pane".into(),
            ))
        }
    }

    fn pane_read<'a>(
        &'a self,
        _fence: &'a PortHostFence,
        _session: &'a String,
        _source: PaneReadSource,
        _lines: u32,
    ) -> impl Future<Output = Result<String, PortError>> + Send + 'a {
        async {
            Err(PortError::InvalidInput(
                "pane_read needs a pane id; use read_pane".into(),
            ))
        }
    }

    fn read_pane<'a>(
        &'a self,
        fence: &'a PortHostFence,
        session: &'a String,
        pane_id: &'a str,
        source: PaneReadSource,
        lines: u32,
    ) -> impl Future<Output = Result<String, PortError>> + Send + 'a {
        async move {
            let (actual, client) = self.fenced_client(fence, session).await?;
            let result = self
                .app
                .hosts
                .run_if_current(&actual, client.pane_read(pane_id, source.as_str(), lines))
                .await
                .ok_or_else(|| {
                    PortError::Conflict(format!("host `{}` fence expired", fence.host_id))
                })?;
            result
                .map(|read| read.text)
                .map_err(|error| PortError::Unavailable(error.to_string()))
        }
    }

    fn send_text<'a>(
        &'a self,
        _fence: &'a PortHostFence,
        _session: &'a String,
        _text: String,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async {
            Err(PortError::InvalidInput(
                "send_text needs a pane id; use send_text_to_pane".into(),
            ))
        }
    }

    fn send_text_to_pane<'a>(
        &'a self,
        fence: &'a PortHostFence,
        session: &'a String,
        pane_id: &'a str,
        text: String,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            let (actual, client) = self.fenced_client(fence, session).await?;
            let result = self
                .app
                .hosts
                .run_if_current(&actual, client.pane_send_text(pane_id, &text))
                .await
                .ok_or_else(|| {
                    PortError::Conflict(format!("host `{}` fence expired", fence.host_id))
                })?;
            result.map_err(|error| PortError::Unavailable(error.to_string()))
        }
    }
}

/// App-side source for local Codex rollout paths. It preserves the existing bot lookup, identity
/// environment resolution, and session log search in the lifecycle helpers.
pub struct AppCodexRolloutAccess<'a> {
    app: &'a Arc<App>,
}

impl<'a> AppCodexRolloutAccess<'a> {
    pub fn new(app: &'a Arc<App>) -> Self {
        Self { app }
    }
}

impl CodexRolloutAccess for AppCodexRolloutAccess<'_> {
    fn local_rollout_path<'a>(
        &'a self,
        bot: &'a BotId,
        session: &'a SessionId,
    ) -> impl Future<Output = Option<std::path::PathBuf>> + Send + 'a {
        async move {
            let bot = crate::db::bot(&self.app.db, bot).await.ok().flatten()?;
            let home = crate::lifecycle::codex_home(self.app, &bot).await?;
            crate::lifecycle::codex_session_log_async(home, session.clone()).await
        }
    }

    fn remote_rollout_read<'a>(
        &'a self,
        run: &'a RunId,
        bot: &'a BotId,
        session: &'a SessionId,
        offset: Option<u64>,
        cap: u64,
    ) -> impl Future<Output = Option<am_ports::RemoteRollout>> + Send + 'a {
        async move {
            let run = crate::db::run(&self.app.db, run).await.ok().flatten()?;
            let bot = crate::db::bot(&self.app.db, bot).await.ok().flatten()?;
            let host = crate::db::bot_host(&self.app.db, &bot.id).await.ok()?;
            crate::runners::codex_async_question_remote::rollout_read(self.app, &bot, &run, &host, session, offset, cap).await
        }
    }
}

/// App-side adapters preserve the existing projection and event ordering behavior.
pub struct AppEventSink<'a, A> {
    app: &'a A,
}

impl<'a, A: crate::capabilities::Emit + crate::capabilities::BotStatusEmit> AppEventSink<'a, A> {
    pub fn new(app: &'a A) -> Self {
        Self { app }
    }
}

impl<A: crate::capabilities::Emit + crate::capabilities::BotStatusEmit> EventSink for AppEventSink<'_, A> {
    fn emit<'a>(
        &'a self,
        event: EventEnvelope,
    ) -> impl Future<Output = Result<EventSeq, PortError>> + Send + 'a {
        async move {
            let payload: serde_json::Value =
                serde_json::from_str(&event.payload_json).map_err(|error| {
                    PortError::InvalidInput(format!("event payload is not JSON: {error}"))
                })?;
            if !payload.is_object() {
                return Err(PortError::InvalidInput(
                    "event payload must be a JSON object".into(),
                ));
            }
            self.app.emit(&event.kind, payload).await;
            // App::emit owns the sequence assignment; callers of this P4 adapter do not use the
            // return value, but expose the current sequence for the port's typed result.
            Ok(self.app.current_seq())
        }
    }

    fn bot_status_changed<'a>(
        &'a self,
        bot: &'a str,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            self.app.emit_bot_status(bot).await;
            Ok(())
        }
    }
}

pub struct AppTurnEvents<'a> {
    app: &'a Arc<App>,
}

impl<'a> AppTurnEvents<'a> {
    pub fn new(app: &'a Arc<App>) -> Self {
        Self { app }
    }

    async fn publish_turn_row(
        &self,
        turn_id: &str,
        delivery: Option<&str>,
    ) -> Result<(), PortError> {
        let row = sqlx::query_as::<_, (String, String, String)>(
            "SELECT c.bot_id, t.status, t.delivery FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE t.id=?",
        )
        .bind(turn_id)
        .fetch_optional(&self.app.db)
        .await
        .map_err(|error| PortError::Unavailable(error.to_string()))?;
        if let Some((bot_id, status, current_delivery)) = row {
            self.app.publish_turn(crate::state::TurnEvent {
                bot_id,
                turn_id: turn_id.to_string(),
                status,
                delivery: delivery.unwrap_or(&current_delivery).to_string(),
            });
        }
        Ok(())
    }
}

/// App-side adapter for the keep-warm system note. It uses the regular lifecycle message path so
/// insertion, ownership validation, and `message_added` publication retain their existing rules.
pub struct AppSystemMessageWriter<'a> {
    app: &'a Arc<App>,
}

impl<'a> AppSystemMessageWriter<'a> {
    pub fn new(app: &'a Arc<App>) -> Self {
        Self { app }
    }
}

impl SystemMessageWriter for AppSystemMessageWriter<'_> {
    fn append_system_message(
        &self,
        bot: BotId,
        content: String,
    ) -> impl Future<Output = Result<(), PortError>> + Send + '_ {
        self.append_system_message_idempotent(bot, content, None)
    }

    fn append_system_message_idempotent(
        &self,
        bot: BotId,
        content: String,
        idempotency_key: Option<String>,
    ) -> impl Future<Output = Result<(), PortError>> + Send + '_ {
        async move {
            let conversation = crate::db::conversation_id(&self.app.db, &bot)
                .await
                .map_err(|error| PortError::Unavailable(error.to_string()))?;
            if let Some(key) = idempotency_key {
                crate::lifecycle::insert_message_idempotent(
                    self.app,
                    &conversation,
                    "system",
                    &content,
                    "system",
                    &key,
                )
                .await
                .map(|_| ())
                .map_err(|error| PortError::Unavailable(error.to_string()))
            } else {
                crate::lifecycle::insert_message(
                    self.app,
                    &conversation,
                    None,
                    "system",
                    &content,
                    "system",
                    false,
                    None,
                )
                .await
                .map(|_| ())
                .map_err(|error| PortError::Unavailable(error.to_string()))
            }
        }
    }
}

/// Wall-clock adapter. Monotonic throttling stays inside the feature; this clock supplies UTC time.
pub struct AppClock;

impl am_ports::Clock for AppClock {
    fn now_unix_ms(&self) -> am_core::UnixMillis {
        chrono::Utc::now().timestamp_millis()
    }
}

impl TurnEvents for AppTurnEvents<'_> {
    fn publish<'a>(
        &'a self,
        event: TurnEvent,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            match event {
                TurnEvent::Completed {
                    run_id, turn_id, ..
                } => {
                    let turn_id = match turn_id {
                        Some(turn_id) => Some(turn_id),
                        None => sqlx::query_scalar::<_, String>(
                            "SELECT id FROM turns WHERE run_id=? ORDER BY created_at DESC, rowid DESC LIMIT 1",
                        )
                        .bind(run_id)
                        .fetch_optional(&self.app.db)
                        .await
                        .map_err(|error| PortError::Unavailable(error.to_string()))?,
                    };
                    if let Some(turn_id) = turn_id {
                        self.publish_turn_row(&turn_id, None).await?;
                    }
                }
                TurnEvent::DeliveryChanged { turn_id, state } => {
                    self.publish_turn_row(&turn_id, Some(&state)).await?;
                }
            }
            Ok(())
        }
    }

    fn turn_changed<'a>(
        &'a self,
        turn: &'a TurnId,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            crate::lifecycle::emit_turn(self.app, turn).await;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{map_error, LcError};
    use am_core::TurnError;
    use serde_json::json;

    #[test]
    fn lifecycle_errors_keep_the_frozen_turn_error_categories() {
        assert_eq!(
            map_error("b1", LcError::NotFound("bot".into())),
            TurnError::BotNotFound("b1".into())
        );
        assert_eq!(
            map_error("b1", LcError::Conflict(json!({"error": "conflict"}))),
            TurnError::Busy("{\"error\":\"conflict\"}".into())
        );
        assert_eq!(
            map_error("b1", LcError::Bad("invalid prompt".into())),
            TurnError::InvalidRequest("invalid prompt".into())
        );
        assert_eq!(
            map_error("b1", LcError::Upstream("host unavailable".into())),
            TurnError::Unavailable("host unavailable".into())
        );
    }
}
