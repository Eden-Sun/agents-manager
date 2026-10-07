//! Mission HTTP adapters at the daemon composition boundary.

use crate::state::App;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

pub(crate) use crate::mission::{candidates, deliver, flow, pick, ports, store, workflow};

pub mod api;

/// 開機時呼叫一次。先訂閱再啟 task，避免漏掉訂閱完成前送出的事件。
pub fn spawn_relay(app: Arc<App>) {
    let mut rx = app.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    crate::mission::relay::relay(&app, &ev.kind, &ev.data).await;
                }
                Err(RecvError::Lagged(n)) => tracing::debug!(skipped = n, "mission relay lagged"),
                Err(RecvError::Closed) => break,
            }
        }
    });
}
