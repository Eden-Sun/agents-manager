use am_core::{BotId, NoticeRequest, PromptRequest, RunId, TurnError, TurnId};
use std::future::Future;

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
