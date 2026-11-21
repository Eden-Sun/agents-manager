//! GitHub host scan scheduling runner.

use crate::state::App;
use std::sync::Arc;

/// Scan a host in the background, coalescing repeated reconcile requests.
pub fn spawn_detect_host(app: Arc<App>, host: String) {
    let Some(mut claim) = crate::github::host_detection_registry().claim(&app.data_dir, &host) else {
        return;
    };
    if app.shutdown.is_cancelled() {
        return;
    }
    let tracker = app.background_tasks.clone();
    crate::background_loop::spawn_named(&tracker, "github host detect", async move {
        loop {
            crate::github::detect_host_once(&app, &host).await;
            if !claim.finish() {
                break;
            }
        }
    });
}

pub fn spawn_detect_all(app: Arc<App>) {
    spawn_detect_host(app, crate::config::LOCAL_HOST.to_string());
}
