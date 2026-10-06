use am_core::{BotId, PaneReadSource, PortError, SessionId};
use std::future::Future;

/// Run-scoped pane read for callers that have a run identity but do not own a HostFence.
/// Implementations preserve the daemon's existing run/session resolution and handoff checks.
pub trait RunPaneReader: Send + Sync {
    fn read_run_pane<'a>(
        &'a self,
        bot: &'a BotId,
        run_session: Option<&'a SessionId>,
        pane_id: &'a str,
        source: PaneReadSource,
        lines: u32,
    ) -> impl Future<Output = Result<Option<String>, PortError>> + Send + 'a;
}

/// Run-scoped ANSI pane read for callers that need terminal styling as data. Implementations
/// preserve the daemon's existing run/session resolution and handoff checks, and request Herdr's
/// `format: ansi` representation without changing the selected source or line limit.
pub trait StyledRunPaneReader: Send + Sync {
    fn read_styled_run_pane<'a>(
        &'a self,
        bot: &'a BotId,
        run_session: Option<&'a SessionId>,
        pane_id: &'a str,
        source: PaneReadSource,
        lines: u32,
    ) -> impl Future<Output = Result<Option<String>, PortError>> + Send + 'a;
}
