#![forbid(unsafe_code)]

use std::fmt;

pub type BotId = String;
pub type HostId = String;
pub type RunId = String;
pub type SessionId = String;
pub type TurnId = String;
pub type QuotaKey = String;
pub type UnixMillis = i64;
pub type EventSeq = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFence {
    pub host_id: HostId,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub used_pct: f64,
    pub resets_at: Option<String>,
    pub observed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResetCredits {
    pub available: i64,
    pub title: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LimitHit {
    pub message: String,
    pub until: Option<String>,
    pub at: String,
    pub bucket: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Quota {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub fable: Option<Window>,
    pub reset_credits: Option<ResetCredits>,
    pub limit_hit: Option<LimitHit>,
    pub plan: Option<String>,
    pub updated_at: String,
    pub source: String,
    pub account: Option<String>,
    pub host: String,
}

pub type QuotaSnapshot = Quota;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    pub bot_id: BotId,
    pub text: String,
    pub client_request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeRequest {
    pub bot_id: BotId,
    pub text: String,
    pub dedupe_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    Completed {
        bot_id: BotId,
        run_id: RunId,
        turn_id: Option<TurnId>,
    },
    DeliveryChanged {
        turn_id: TurnId,
        state: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventEnvelope {
    pub kind: String,
    pub bot_id: Option<BotId>,
    /// A JSON object encoded as text. The event sink owns sequencing and sanitization.
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    NotFound(String),
    Conflict(String),
    Unavailable(String),
    InvalidInput(String),
    Failed(String),
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(message)
            | Self::Conflict(message)
            | Self::Unavailable(message)
            | Self::InvalidInput(message)
            | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PortError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnError {
    BotNotFound(BotId),
    Busy(BotId),
    QuotaBlocked(String),
    InvalidRequest(String),
    Unavailable(String),
    Failed(String),
}

impl fmt::Display for TurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BotNotFound(bot_id) => write!(f, "bot {bot_id} was not found"),
            Self::Busy(bot_id) => write!(f, "bot {bot_id} is busy"),
            Self::QuotaBlocked(message)
            | Self::InvalidRequest(message)
            | Self::Unavailable(message)
            | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for TurnError {}
