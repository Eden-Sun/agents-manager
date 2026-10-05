use am_core::{BotId, HostFence, HostId, PaneReadSource, PortError, RunId, SessionId};
use std::future::Future;

pub trait HostRuntime: Send + Sync {
    fn current_fence<'a>(
        &'a self,
        host: &'a HostId,
    ) -> impl Future<Output = Result<Option<HostFence>, PortError>> + Send + 'a;

    fn session_for_bot<'a>(
        &'a self,
        bot: &'a BotId,
        host: &'a HostId,
    ) -> impl Future<Output = Result<Option<SessionId>, PortError>> + Send + 'a;

    fn session_for_run<'a>(
        &'a self,
        run: &'a RunId,
    ) -> impl Future<Output = Result<Option<SessionId>, PortError>> + Send + 'a;

    fn is_connected<'a>(
        &'a self,
        fence: &'a HostFence,
        session: &'a SessionId,
    ) -> impl Future<Output = Result<bool, PortError>> + Send + 'a;
}

pub trait HerdrPort: Send + Sync {
    fn ping<'a>(
        &'a self,
        fence: &'a HostFence,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a;

    fn screen_text<'a>(
        &'a self,
        fence: &'a HostFence,
        session: &'a SessionId,
    ) -> impl Future<Output = Result<String, PortError>> + Send + 'a;

    fn pane_read<'a>(
        &'a self,
        fence: &'a HostFence,
        session: &'a SessionId,
        source: PaneReadSource,
        lines: u32,
    ) -> impl Future<Output = Result<String, PortError>> + Send + 'a;

    fn send_text<'a>(
        &'a self,
        fence: &'a HostFence,
        session: &'a SessionId,
        text: String,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a;
}
