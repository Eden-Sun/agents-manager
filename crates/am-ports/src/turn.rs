use am_core::{BotId, NoticeRequest, PromptRequest, RunId, SessionId, TurnError, TurnId};
use std::future::Future;
use std::path::PathBuf;

pub trait TurnControl: Send + Sync {
    fn start_bot(&self, bot: BotId) -> impl Future<Output = Result<RunId, TurnError>> + Send + '_;

    fn stop_bot(
        &self,
        bot: BotId,
        reason: Option<String>,
    ) -> impl Future<Output = Result<(), TurnError>> + Send + '_;

    fn send_prompt(
        &self,
        request: PromptRequest,
    ) -> impl Future<Output = Result<TurnId, TurnError>> + Send + '_;

    fn compact_bot(&self, bot: BotId) -> impl Future<Output = Result<(), TurnError>> + Send + '_;

    fn interrupt(
        &self,
        bot: BotId,
        reason: String,
    ) -> impl Future<Output = Result<(), TurnError>> + Send + '_;

    fn queue_notice(
        &self,
        notice: NoticeRequest,
    ) -> impl Future<Output = Result<(), TurnError>> + Send + '_;
}

/// Resolve the local Codex rollout log for one bot session. The composition adapter owns bot and
/// host lookup plus Codex home resolution; features only receive a path they are allowed to read.
pub trait CodexRolloutAccess: Send + Sync {
    fn local_rollout_path<'a>(
        &'a self,
        bot: &'a BotId,
        session: &'a SessionId,
    ) -> impl Future<Output = Option<PathBuf>> + Send + 'a;
}
