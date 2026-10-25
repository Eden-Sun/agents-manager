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

/// Identifies a run whose stop has already won the lifecycle CAS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiRunKey {
    pub bot_id: String,
    pub run_id: String,
}

/// Business outcome after confirmed task completion. A panic is an error, not Failed.
#[derive(Debug, PartialEq, Eq)]
pub enum ApiRunCompletion {
    Completed,
    Cancelled,
    Failed,
}

/// Evidence that no runner remains after the run's spawn admission has been drained.
#[derive(Debug, PartialEq, Eq)]
pub enum ApiRunStopEvidence {
    NoLiveRunner,
    Joined {
        turn_id: String,
        completion: ApiRunCompletion,
    },
}

/// Run-scoped stop evidence. The key binds the evidence to the lifecycle CAS target.
#[derive(Debug, PartialEq, Eq)]
pub struct ApiRunStopProof {
    pub key: ApiRunKey,
    pub evidence: ApiRunStopEvidence,
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
    fn api_run_stop_proof(
        &self,
        key: &ApiRunKey,
    ) -> impl Future<Output = Result<ApiRunStopProof, ApiBotError>> + Send;
    fn api_run_live(&self, key: &ApiTurnKey) -> impl Future<Output = bool> + Send;
}
