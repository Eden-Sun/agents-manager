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
    let manager_severity = if supervisor_status == "failed" || supervisor_status == "waiting_quota" {
        "critical"
    } else if supervisor_status == "not_configured" || supervisor_status == "stopped" || !app.connected.load(std::sync::atomic::Ordering::SeqCst) {
        "degraded"
    } else { "healthy" };
    let hosts = app.hosts.list().await;
    let disconnected_hosts = hosts.iter().filter(|h| !h.is_connected()).count();
    let system = crate::supervisor::incidents::system_health(app).await;
    let system_severity = system.get("status").and_then(Value::as_str).unwrap_or("unknown");
    // The compat projection. `status` used to mean "is AGM all right", and a caller that only
    // reads this field must not be told everything is fine while a host is down — so it is now
    // the worse of the two halves, and the halves are published next to it. See docs/SPEC.md §18.
    let severity = crate::supervisor::incidents::worst(manager_severity, system_severity);
    Ok(json!({
        "status": severity,
        "checked_at": crate::db::now(),
        // Two questions, two answers: whether the manager can work, and whether the system
        // around it is intact. Folding them into one number is what let `healthy` mean neither.
        "manager_health": {
            "status": manager_severity,
            "supervisor_status": supervisor_status,
            "daemon_connected": app.connected.load(std::sync::atomic::Ordering::SeqCst),
        },
        "system_health": system,
        "daemon": {"connected": app.connected.load(std::sync::atomic::Ordering::SeqCst)},
        "supervisor": supervisor,
        "bots": {"total": bots.len(), "running": running, "busy": busy, "stopped": stopped},
        "quota": crate::quota::snapshot(app).await,
        // Two numbers, not one sum: an assignment still running and a notification nobody
        // acked are different kinds of "owed", and adding them hid a 464-event backlog.
        "pending_assignments": crate::supervisor::store::open_assignment_count(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "awaiting_review": crate::supervisor::store::awaiting_review_count(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "inbox_open": crate::supervisor::store::open_inbox_count(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "hosts": {"total": hosts.len(), "disconnected": disconnected_hosts},
    }))
}

/// What the inbox debounce keys on. `idle` and `busy` are one state here: the manager going
/// busy on its own turn is not news the manager needs an inbox event about.
pub fn inbox_state(supervisor_status: &str) -> &str {
    match supervisor_status {
        "idle" | "busy" => "running",
        other => other,
    }
}

/// No one is there to read an event: it is not queued, and whatever changed meanwhile is
/// folded into the one snapshot sent when the manager is back.
fn manager_down(inbox_state: &str) -> bool {
    matches!(inbox_state, "stopped" | "starting" | "not_configured")
}

/// Decides which health ticks become `health_changed` inbox events.
///
/// 2026-09-10: the fingerprint included the busy / running / pending counters, so a night of
/// bots starting and finishing put 464 events in front of a manager that was not even up. Only
/// the severity and the manager's own state count now, and nothing is queued while it is down.
#[derive(Debug, Default)]
pub struct Debounce {
    last_pushed: Option<(String, String)>,
    /// Something changed while the manager was down; it gets one snapshot when it is back.
    suppressed: bool,
}

impl Debounce {
    /// `true` = queue this tick's snapshot.
    pub fn observe(&mut self, severity: &str, supervisor_status: &str) -> bool {
        let state = inbox_state(supervisor_status);
        let key = (severity.to_string(), state.to_string());
        let changed = self.last_pushed.as_ref() != Some(&key);
        if manager_down(state) {
            if changed {
                self.suppressed = true;
            }
            return false;
        }
        if changed || self.suppressed {
            self.last_pushed = Some(key);
            self.suppressed = false;
            return true;
        }
        false
    }
}

/// Poll health outside the assignment controller. The daemon emits every fingerprint change to
/// the UI, and queues a durable inbox event only when the *state* changes (see [`Debounce`]),
/// so AGM can reason about it without `/loop` and without wading through counters.
pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut previous = String::new();
        let mut debounce = Debounce::default();
        let mut detector = crate::supervisor::incidents::Detector::default();
        loop {
            tick.tick().await;
            // Incidents first: the snapshot below reports what this pass decided, so a fault
            // and the health reading that mentions it never disagree by one tick.
            crate::supervisor::incidents::sweep(&app, &mut detector).await;
            let Ok(snapshot) = snapshot(&app).await else { continue };
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
            // Keyed on the *manager's* half only. System faults have their own durable
            // incidents with their own one-event-per-transition rule; letting them move this
            // key too would tell the manager the same thing twice.
            let manager_status =
                snapshot.pointer("/manager_health/status").and_then(Value::as_str).unwrap_or("unknown").to_string();
            if !debounce.observe(&manager_status, &sup_status) {
                continue;
            }
            let key = format!("health:{manager_status}:{}:{}", inbox_state(&sup_status), chrono::Utc::now().timestamp());
            let bot_id = snapshot.pointer("/supervisor/bot_id").and_then(Value::as_str);
            let _ = crate::supervisor::store::push_inbox(
                &app.db, &key, "health_changed", None, bot_id, None, &snapshot,
            ).await;
            tracing::info!(status, supervisor = %sup_status, "supervisor health changed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_do_not_make_events_only_state_does() {
        let mut d = Debounce::default();
        assert!(d.observe("healthy", "idle"), "the first reading after boot is news");
        // Ticks with different bot counts land here as the same (severity, state): nothing.
        assert!(!d.observe("healthy", "idle"));
        assert!(!d.observe("healthy", "busy"), "the manager's own busy/idle is not a state change");
        assert!(d.observe("degraded", "idle"));
        assert!(d.observe("healthy", "idle"));
        assert!(d.observe("critical", "waiting_quota"));
    }

    #[test]
    fn nothing_is_queued_while_the_manager_is_down_and_one_snapshot_when_it_is_back() {
        let mut d = Debounce::default();
        assert!(d.observe("healthy", "idle"));
        // 5.5 hours of 30-second ticks against a dead manager: zero events, not 464.
        for _ in 0..660 {
            assert!(!d.observe("degraded", "stopped"));
        }
        assert!(!d.observe("degraded", "starting"));
        assert!(d.observe("healthy", "idle"), "one snapshot once it is back, even to the same state");
        assert!(!d.observe("healthy", "idle"));
    }

    #[test]
    fn a_manager_that_was_never_up_gets_its_first_snapshot_when_it_is() {
        let mut d = Debounce::default();
        assert!(!d.observe("degraded", "not_configured"));
        assert!(!d.observe("degraded", "stopped"));
        assert!(d.observe("healthy", "busy"));
    }
}
