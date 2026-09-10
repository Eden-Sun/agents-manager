//! Cheap, deterministic health summary for AGM and the UI.

use crate::lifecycle::LcError;
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub async fn snapshot(app: &Arc<App>) -> Result<Value, LcError> {
    let bots = crate::db::live_bots(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let mut running = 0usize;
    let mut busy = 0usize;
    let mut stopped = 0usize;
    for bot in &bots {
        match crate::db::active_run(&app.db, &bot.id).await.map_err(|e| LcError::Upstream(e.to_string()))? {
            Some(run) => {
                running += 1;
                if run.agent_status == "working" || run.agent_status == "blocked" { busy += 1; }
            }
            None => stopped += 1,
        }
    }
    let supervisor = crate::supervisor::status_json(app).await?;
    let supervisor_status = supervisor.get("status").and_then(Value::as_str).unwrap_or("unknown");
    let severity = if supervisor_status == "failed" || supervisor_status == "waiting_quota" {
        "critical"
    } else if supervisor_status == "not_configured" || supervisor_status == "stopped" || !app.connected.load(std::sync::atomic::Ordering::SeqCst) {
        "degraded"
    } else { "healthy" };
    let hosts = app.hosts.list().await;
    let disconnected_hosts = hosts.iter().filter(|h| !h.is_connected()).count();
    Ok(json!({
        "status": severity,
        "checked_at": crate::db::now(),
        "daemon": {"connected": app.connected.load(std::sync::atomic::Ordering::SeqCst)},
        "supervisor": supervisor,
        "bots": {"total": bots.len(), "running": running, "busy": busy, "stopped": stopped},
        "quota": crate::quota::snapshot(app).await,
        "pending_assignments": crate::supervisor::store::pending_count(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "hosts": {"total": hosts.len(), "disconnected": disconnected_hosts},
    }))
}

/// Poll health outside the assignment controller. The daemon emits only changes, and sends a
/// durable inbox event when the summary changes so AGM can reason about it without `/loop`.
pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut previous = String::new();
        loop {
            tick.tick().await;
            let Ok(snapshot) = snapshot(&app).await else { continue };
            let fingerprint = serde_json::json!({
                "status": snapshot.get("status"),
                "supervisor": snapshot.get("supervisor").and_then(|v| v.get("status")),
                "running": snapshot.pointer("/bots/running"),
                "busy": snapshot.pointer("/bots/busy"),
                "pending": snapshot.get("pending_assignments"),
                "disconnected": snapshot.pointer("/hosts/disconnected"),
            }).to_string();
            if fingerprint == previous { continue; }
            previous = fingerprint.clone();
            let _ = app.emit("supervisor_health", snapshot.clone()).await;
            let status = snapshot.get("status").and_then(Value::as_str).unwrap_or("unknown");
            let bucket = chrono::Utc::now().timestamp() / 1800;
            let key = format!("health:{fingerprint}:{bucket}");
            let bot_id = snapshot.pointer("/supervisor/bot_id").and_then(Value::as_str);
            let _ = crate::supervisor::store::push_inbox(
                &app.db, &key, "health_changed", None, bot_id, None, &snapshot,
            ).await;
            tracing::info!(status, "supervisor health changed");
        }
    });
}
