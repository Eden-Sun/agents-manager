#![forbid(unsafe_code)]

mod capture;
mod clock;
mod database;
mod events;
mod host;
mod lock;
mod quota;
mod run_pane;
mod turn;

pub use am_core::{
    BotId, EventEnvelope, EventSeq, HostFence, HostId, LimitHit, NoticeRequest, PaneReadSource,
    PortError, PromptRequest, Quota, QuotaKey, QuotaSnapshot, RunId, SessionId, TurnError,
    TurnEvent, TurnId, UnixMillis,
};
pub use capture::CaptureParser;
pub use clock::{Clock, IdSource};
pub use database::DbContext;
pub use events::{EventSink, TurnEvents};
pub use host::{HerdrPort, HostRuntime};
pub use lock::{BotLock, BotLockGuard};
pub use quota::QuotaAccess;
pub use run_pane::RunPaneReader;
pub use turn::TurnControl;

pub type PortResult<T> = Result<T, PortError>;
