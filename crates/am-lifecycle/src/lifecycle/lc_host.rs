//! Lifecycle's host boundary: only the narrow capabilities lifecycle operations use.

/// Host capabilities shared by lifecycle entry points. Composition-specific behavior is exposed
/// through the small service ports in `s6_ports` and `send_now::ports`.
pub trait LcHost:
    crate::lifecycle::start::StartContext
    + crate::lifecycle::s6_ports::StuckTurnContext
    + crate::lifecycle::s6_ports::DeferredLiveContext
    + crate::lifecycle::s6_ports::SendNowContext
    + crate::lifecycle::s6_ports::TurnEventHostServices
    + crate::lifecycle::s6_ports::InterruptGraceHostServices
    + crate::lifecycle::s6_ports::RestartHoldHostServices
    + crate::lifecycle::s6_ports::TranscriptOriginServices
    + crate::lifecycle::s6_ports::TurnErrorContext
    + crate::lifecycle::s6_ports::BusySendServices
    + crate::lifecycle::s6_ports::CodexSteerContext
    + crate::lifecycle::s6_ports::LiveApplyDebtContext
    + crate::lifecycle::s6_ports::GrokTranscriptContext
    + crate::lifecycle::poller::ProgressPollers
    + crate::lifecycle::poller::StallTimers
    + crate::lifecycle::poller::ProgressEmitted
    + crate::lifecycle::poller::FallbackTimers
    + crate::lifecycle::send_now::ports::AttachSendPort
    + crate::lifecycle::send_now::ports::CodexSendPort
    + crate::lifecycle::send_now::ports::IdleSleepPort
    + crate::lifecycle::send_now::ports::MaintenancePort
    + crate::lifecycle::send_now::ports::SendEnvPort
    + crate::lifecycle::send_now::ports::ShareSendRepo
    + crate::login_prompt::LoginNeeded
{}
