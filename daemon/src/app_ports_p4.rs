//! P4 composition adapters from the frozen am-ports turn capability to lifecycle operations.
//!
//! This is the only place the P4 port adapter knows the concrete App. Feature callers can depend
//! on `am_ports::TurnControl` without importing lifecycle internals or the App state type.

use am_core::{BotId, NoticeRequest, PromptRequest, RunId, TurnError, TurnId};
use am_ports::TurnControl;
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
            lifecycle::prompt(self.app, &request.bot_id, &request.text, &request_id)
                .await
                .map(|out| out.turn_id)
                .map_err(|err| map_error(&request.bot_id, err))
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
                lifecycle::RelaySrc::trusted(Some("daemon")),
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
