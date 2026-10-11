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
/// 一台主機一輪探測的時間上限（#1302）。
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_secs(45);

/// 定期重探每台主機：只換了 CLI 的 mismatch 要即時亮警告。
pub fn spawn_poller(app: std::sync::Arc<crate::state::App>) {
    crate::background_loop::spawn_periodic(&app, "herdr version poller", REFRESH_EVERY, REFRESH_EVERY, |app| async move {
        refresh_all(&app).await;
    });
}

/// 一輪探所有主機（poller 與測試共用）。每台各自一個 task，並有各自的時間上限：卡住的一台只拖慢自己，
/// 別台照常更新（#1302）；超過上限的這一輪放掉，下一輪再探。
pub async fn refresh_all(app: &std::sync::Arc<crate::state::App>) {
    let mut set = tokio::task::JoinSet::new();
    for name in app.hosts.names().await {
        let app = app.clone();
        set.spawn(async move {
            if tokio::time::timeout(PROBE_BUDGET, refresh(&app, &name)).await.is_err() {
                tracing::warn!(host = %name, "herdr version probe ran past its budget; skipped this round");
            }
        });
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            tracing::warn!(error = %e, "herdr version probe task failed");
        }
    }
}
