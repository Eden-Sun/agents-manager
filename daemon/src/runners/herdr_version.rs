//! Herdr version refresh runner.

use crate::herdr_version::for_host;

/// 重 ping＋重探 CLI 版本，有變就推 `host_changed`（#254）：herdr live-handoff 後 server 版本／protocol 換了，
/// 或只換了 CLI，快取都會過期。`cli` 由呼叫端探（測試直接給）；讀不到＝None，不保留舊值。
#[cfg(test)]
pub async fn refresh_with(app: &std::sync::Arc<crate::state::App>, host: &str, cli: Option<String>) {
    let Some(fence) = app.hosts.fence(host).await else { return };
    refresh_with_fence(app, host, &fence, cli).await;
}

/// `cli` 是在 `fence` 之下量的：探測途中主機被重連／改指到另一台，就不能把它寫進現在那台的快取（#347，
/// 否則 A 機的 CLI 版本會被掛到 B 機上，誤報或藏起 server/CLI 不一致）。
pub async fn refresh_with_fence(app: &std::sync::Arc<crate::state::App>, host: &str, fence: &crate::hosts::HostFence, cli: Option<String>) {
    let conn = fence.conn().clone();
    let connected = if conn.is_local() { app.connected.load(std::sync::atomic::Ordering::SeqCst) } else { conn.is_connected() };
    let before = for_host(&conn, connected, app.tools.lock().await.get(host));
    let _ = conn.client.ping().await;
    let after = {
        let mut tools = app.tools.lock().await;
        if !app.hosts.is_current(fence).await {
            tracing::info!(host, "herdr version probe superseded by a reconnect/reconfigure; discarded");
            return;
        }
        if let Some(t) = tools.get_mut(host) {
            t.herdr_cli = cli;
        }
        for_host(&conn, connected, tools.get(host))
    };
    if before != after {
        tracing::info!(host, ?before, ?after, "herdr version changed");
        crate::app_ports_p12::emit_host_changed(app, fence).await;
    }
}

pub async fn refresh(app: &std::sync::Arc<crate::state::App>, host: &str) {
    let Some(fence) = app.hosts.fence(host).await else { return };
    let cli = crate::tools::probe_herdr_cli(app, host).await;
    refresh_with_fence(app, host, &fence, cli).await;
}

const REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// 定期重探每台主機：只換了 CLI 的 mismatch 要即時亮警告。
pub fn spawn_poller(app: std::sync::Arc<crate::state::App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(REFRESH_EVERY).await;
            for name in app.hosts.names().await {
                refresh(&app, &name).await;
            }
        }
    });
}
