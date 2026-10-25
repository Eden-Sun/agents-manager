//! Inert API-bot host contract. No implementation, LcHost binding, or daemon dependency.
//! Cancellation success requires the runner's actual join receipt, never a missing registry
//! entry or a completed database turn. Runtime adapters are introduced separately.
use std::future::Future;

#[derive(Debug, PartialEq, Eq)]
pub struct ApiTurnKey {
    pub bot_id: String,
    pub run_id: String,
    pub turn_id: String,
}

/// Business outcome after confirmed task completion. A panic is an error, not Failed.
#[derive(Debug, PartialEq, Eq)]
pub enum ApiRunCompletion {
    Completed,
    Cancelled,
    Failed,
}

/// Static diagnostics: no provider response, prompt, key, or arbitrary upstream text.
#[derive(Debug, PartialEq, Eq)]
pub enum ApiBotError {
    Busy,
    IdentityMismatch,
    Unconfirmed,
    Upstream,
}

pub trait ApiBotPort: Send + Sync {
    fn api_turn_spawn(
        &self,
        key: &ApiTurnKey,
    ) -> impl Future<Output = Result<(), ApiBotError>> + Send;
    fn api_run_cancel_and_join(
        &self,
        key: &ApiTurnKey,
    ) -> impl Future<Output = Result<ApiRunCompletion, ApiBotError>> + Send;
    fn api_run_live(&self, key: &ApiTurnKey) -> impl Future<Output = bool> + Send;
}
