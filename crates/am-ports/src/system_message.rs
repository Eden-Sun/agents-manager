use am_core::{BotId, PortError};
use std::future::Future;

/// Writes a durable system message to the conversation owned by a bot.
pub trait SystemMessageWriter: Send + Sync {
    fn append_system_message(
        &self,
        bot: BotId,
        content: String,
    ) -> impl Future<Output = Result<(), PortError>> + Send + '_;
}
