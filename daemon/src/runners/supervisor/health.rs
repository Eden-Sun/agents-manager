//! App-backed scheduling for supervisor health probes.

use crate::state::App;
use crate::supervisor::ports::HostProbes;
use crate::supervisor::health as core;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Poll health outside the assignment controller. The daemon emits every fingerprint change to
/// the UI, and queues a durable inbox event only when the *state* changes (see [`core::Debounce`]),
/// so AGM can reason about it without `/loop` and without wading through counters.
pub fn spawn(app: Arc<App>) {
    let loop_app = app.clone();
    app.spawn_restartable("supervisor health", move || {
        let app = loop_app.clone();
        async move { health_loop(app).await }
    });
}

async fn health_loop(app: Arc<App>) {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut previous = String::new();
        let mut debounce = core::Debounce::default();
        let mut detector = crate::supervisor::incidents::Detector::default();
        let mut last_compact: Option<std::time::Instant> = None;
        loop {
            tokio::select! {
                _ = app.shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            if last_compact.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600)) {
                last_compact = Some(std::time::Instant::now());
                match crate::supervisor::store::compact_handled_payloads(
                    &app.db,
                    crate::supervisor::store::COMPACT_HANDLED_AFTER_SECS,
                    crate::supervisor::store::COMPACT_HANDLED_MIN_BYTES,
                )
                .await
                {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(compacted = n, "compacted old handled inbox payloads"),
                    Err(e) => tracing::warn!(error = ?e, "could not compact old handled inbox payloads"),
                }
                match crate::supervisor::store::prune_handled_events(&app.db, crate::supervisor::store::PRUNE_HANDLED_AFTER_SECS).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(pruned = n, "pruned old handled log events from the inbox"),
                    Err(e) => tracing::warn!(error = ?e, "could not prune old handled inbox events"),
                }
            }
            crate::supervisor::role_faults::refresh(&app).await;
            crate::supervisor::incidents::sweep(&app, &mut detector).await;
            let Ok(snapshot) = core::snapshot(&app).await else { continue };
            let status = snapshot.get("status").and_then(Value::as_str).unwrap_or("unknown").to_string();
            let sup_status = snapshot.pointer("/supervisor/status").and_then(Value::as_str).unwrap_or("unknown").to_string();
            let fingerprint = serde_json::json!({
                "status": status,
                "supervisor": sup_status,
                "running": snapshot.pointer("/bots/running"),
                "busy": snapshot.pointer("/bots/busy"),
                "pending": snapshot.get("pending_assignments"),
                "awaiting_review": snapshot.get("awaiting_review"),
                "inbox_open": snapshot.get("inbox_open"),
                "disconnected": snapshot.pointer("/hosts/disconnected"),
                "incidents": snapshot.pointer("/system_health/open_incidents"),
            }).to_string();
            if fingerprint != previous {
                previous = fingerprint;
                let _ = app.emit("supervisor_health", snapshot.clone()).await;
            }
            let severity = core::inbox_severity(&snapshot);
            core::queue_health_change(&app.db, &mut debounce, &snapshot, &severity, &sup_status).await;
        }
}
