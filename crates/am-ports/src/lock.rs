use am_core::{BotId, PortError};
use std::future::Future;

/// A held lease; releasing it is tied to dropping the guard.
pub trait BotLockGuard: Send + Sync {}

pub trait BotLock: Send + Sync {
    fn lock_bot<'a>(
        &'a self,
        bot: &'a BotId,
    ) -> impl Future<Output = Result<Box<dyn BotLockGuard + 'a>, PortError>> + Send + 'a;
}
