//! Mission HTTP adapters at the daemon composition boundary.

use crate::state::App;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

pub(crate) use crate::mission::{candidates, deliver, flow, pick, ports, store, workflow};

pub mod api;

/// 開機時呼叫一次。先訂閱再啟 task，避免漏掉訂閱完成前送出的事件。
pub fn spawn_relay(app: Arc<App>) {
    // 訂閱放在 supervisor 的 factory 外面：迴圈 panic 重啟後接著同一個 receiver，不漏掉也不重訂。
    let rx = Arc::new(tokio::sync::Mutex::new(app.subscribe()));
    crate::background_loop::spawn_restartable(&app, "mission relay", {
        let app = app.clone();
        move || {
            let (app, rx) = (app.clone(), rx.clone());
            async move {
                let shutdown = app.shutdown.clone();
                let mut rx = rx.lock().await;
                loop {
                    let received = tokio::select! {
                        _ = shutdown.cancelled() => return,
                        received = rx.recv() => received,
                    };
                    match received {
                        Ok(ev) => {
                            crate::mission::relay::relay(&app, &ev.kind, &ev.data).await;
                        }
                        Err(RecvError::Lagged(n)) => tracing::debug!(skipped = n, "mission relay lagged"),
                        Err(RecvError::Closed) => break,
                    }
                }
            }
        }
    });
}
