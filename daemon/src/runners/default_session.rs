//! `default_session` 背景巡邏 runner。

use crate::default_session::{sync, SESSION};
use crate::state::App;
use std::sync::Arc;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(8);

/// Poll as a backstop for agents started after the daemon and for Herdr versions that do not
/// emit `pane.agent_detected` for every launch.
pub fn spawn_poller(app: Arc<App>) {
    tokio::spawn(async move {
        let mut last_error = None;
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if let Err(e) = sync(&app).await {
                let message = format!("{e:#}");
                if last_error.as_deref() != Some(message.as_str()) {
                    tracing::warn!(session = SESSION, error = %message, "default session poll failed");
                    last_error = Some(message);
                }
            } else {
                last_error = None;
            }
        }
    });
}
