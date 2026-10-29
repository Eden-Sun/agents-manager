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

pub enum ApiBeginStopMode { User { withdraw: bool }, Idle }
pub enum ApiBeginStopResult { Applied { withdrawn: Vec<(String, super::queue::Revoked)> }, Lost }
#[derive(Debug, PartialEq, Eq)]
pub enum ApiBeginStopError { Storage, IdentityMismatch, Busy, Ambiguous }

/// Exact private session selected by the caller, never the current successor.
pub struct ApiFreshTarget { pub session_id: String, pub generation: i64 }
pub enum ApiFreshResult { Cleared }
#[derive(Debug, PartialEq, Eq)]
pub enum ApiFreshError { Busy, IdentityMismatch, Unresolved, Storage, Unconfirmed }

pub enum ApiNativePromptError { Unconfirmed }
pub enum ApiFreshAvailability { Available(ApiFreshTarget), None, Unavailable }
pub enum ApiNativeStartMode { Resume { required: bool }, Fresh(ApiFreshTarget) }
pub struct ApiNativeStartResult { pub run_id: String, pub resumed: bool }
pub enum ApiNativeStartError { Unconfirmed, ClearedButStartUnconfirmed }

pub trait ApiBotPort: Send + Sync {
    fn api_native_start(&self, _bot_id: &str, _mode: ApiNativeStartMode) -> impl Future<Output = Result<ApiNativeStartResult, ApiNativeStartError>> + Send { async { Err(ApiNativeStartError::Unconfirmed) } }
    fn api_native_prompt(&self, _bot_id: &str, _text: &str, _crid: &str) -> impl Future<Output = Result<crate::lc_error::PromptOut, ApiNativePromptError>> + Send { async { Err(ApiNativePromptError::Unconfirmed) } }
    fn api_fresh_target(&self, _bot_id: &str) -> impl Future<Output = ApiFreshAvailability> + Send { async { ApiFreshAvailability::Unavailable } }
    fn api_recovery_ready(&self) -> bool { false }
    fn api_explicit_retire(&self, _bot_id: &str, _target: &ApiFreshTarget) -> impl Future<Output = Result<ApiFreshResult, ApiFreshError>> + Send { async { Err(ApiFreshError::Unconfirmed) } }
    fn api_run_begin_stop(&self, key: &ApiRunKey, mode: ApiBeginStopMode)
        -> impl Future<Output = Result<ApiBeginStopResult, ApiBeginStopError>> + Send;
    fn api_turn_spawn_pending(&self, _key: &ApiTurnKey) -> impl Future<Output = Result<(), ApiBotError>> + Send { async { Err(ApiBotError::Upstream) } }
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
