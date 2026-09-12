//! System incidents: what is wrong *outside* the manager itself.
//!
//! `health::snapshot` answers "is AGM up". That was never the same question as "is the system
//! healthy", and the 2026-09-12 review found the gap: a remote host could drop, a bot could sit
//! dead for hours and an assignment could stall, and the summary still read `healthy` because
//! none of it touched the manager's own row.
//!
//! So faults are tracked per *resource*, not per tick. A condition has to persist past a
//! configured threshold before it becomes an incident ([`Detector`] holds the first sighting);
//! once open, the incident is a durable row that a restart deduplicates against; when the
//! condition clears, exactly one resolution event goes out. That shape is what keeps this
//! honest in both directions — a fault cannot be lost in a debounce, and a flapping counter
//! cannot turn into a notification storm.
//!
//! What is deliberately *not* an incident: a bot the user stopped, a pane blocked waiting for
//! an answer, a queue that is merely busy, and the manager's own idle/busy churn. An unknown
//! reading is reported as `unknown`, never folded into `healthy`.

use crate::state::App;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use super::store;

/// Severity ordering, worst last. `unknown` sits above healthy on purpose: not knowing is not
/// the same as being fine.
pub const SEVERITIES: [&str; 4] = ["healthy", "unknown", "degraded", "critical"];

pub fn worst(a: &str, b: &str) -> String {
    let rank = |s: &str| SEVERITIES.iter().position(|x| *x == s).unwrap_or(0);
    if rank(a) >= rank(b) { a.to_string() } else { b.to_string() }
}

/// One thing that is currently wrong, as the probes see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub kind: String,
    pub resource: String,
    pub severity: String,
    pub detail: String,
}

impl Observation {
    fn key(&self) -> (String, String) {
        (self.kind.clone(), self.resource.clone())
    }
}

/// Thresholds, in seconds, plus the notify budget. Read from `[supervisor]` in config.toml.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub host_disconnected_secs: i64,
    pub bot_stopped_secs: i64,
    pub assignment_stalled_secs: i64,
    pub notify_max_attempts: i64,
}

impl Thresholds {
    pub fn from_cfg(cfg: &crate::config::SupervisorCfg) -> Self {
        Self {
            host_disconnected_secs: cfg.host_disconnected_secs as i64,
            bot_stopped_secs: cfg.bot_stopped_secs as i64,
            assignment_stalled_secs: cfg.assignment_stalled_secs as i64,
            notify_max_attempts: cfg.notify_max_attempts,
        }
    }
}

/// Holds how long each condition has been true, so a threshold can be applied before anything
/// durable is written.
///
/// In memory on purpose: after a daemon restart a condition has to be observed for its
/// threshold again before it opens an incident. That errs towards quiet, and the incidents that
/// were already open are still in the database — a restart cannot lose one, only delay a new
/// one by at most the threshold.
#[derive(Debug, Default)]
pub struct Detector {
    since: HashMap<(String, String), i64>,
}

/// What one pass decided to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Conditions that have now been true for long enough to be written down.
    pub open: Vec<Observation>,
    /// Incidents whose condition is no longer observed.
    pub resolve: Vec<(String, String)>,
}

impl Detector {
    /// `now` is a unix timestamp; `open` is what is currently in the database.
    pub fn plan(
        &mut self,
        observations: &[Observation],
        open: &[(String, String)],
        thresholds: &Thresholds,
        now: i64,
    ) -> Plan {
        let mut plan = Plan::default();
        let seen: Vec<(String, String)> = observations.iter().map(Observation::key).collect();
        for obs in observations {
            let key = obs.key();
            let first = *self.since.entry(key.clone()).or_insert(now);
            let held = now - first;
            let needed = match obs.kind.as_str() {
                "host_disconnected" => thresholds.host_disconnected_secs,
                "bot_stopped" => thresholds.bot_stopped_secs,
                // The stalled and exhausted probes carry their own age test; a second wait here
                // would just double the threshold.
                _ => 0,
            };
            if held >= needed || open.contains(&key) {
                plan.open.push(obs.clone());
            }
        }
        // Anything open that nobody observed this pass has cleared.
        for key in open {
            if !seen.contains(key) {
                plan.resolve.push(key.clone());
            }
        }
        self.since.retain(|k, _| seen.contains(k));
        plan
    }
}

/// Read every cheap probe. No LLM, no process is killed to find out how it is doing: this runs
/// on the 30-second health tick and may not cost more than a few queries.
pub async fn observe(app: &Arc<App>, thresholds: &Thresholds) -> Vec<Observation> {
    let mut out = Vec::new();

    // A host the daemon cannot reach takes every bot on it with it, and nothing else notices.
    for host in app.hosts.list().await {
        if !host.is_connected() {
            out.push(Observation {
                kind: "host_disconnected".into(),
                resource: host.name.clone(),
                severity: "degraded".into(),
                detail: json!({"host": host.name}).to_string(),
            });
        }
    }

    // "Expected running" is the user's own `autostart`, not a guess: a bot somebody stopped on
    // purpose is not a fault, and treating it as one is how a health page becomes noise.
    if let Ok(bots) = crate::db::live_bots(&app.db).await {
        for bot in bots {
            if bot.autostart == 0 {
                continue;
            }
            if matches!(crate::db::active_run(&app.db, &bot.id).await, Ok(None)) {
                out.push(Observation {
                    kind: "bot_stopped".into(),
                    resource: bot.id.clone(),
                    severity: "degraded".into(),
                    detail: json!({"bot_id": bot.id, "name": bot.name, "expected": "autostart"}).to_string(),
                });
            }
        }
    }

    // Work that has not moved in hours. `updated_at` moves on every retry and every delivery,
    // so this only fires on something genuinely stuck — including an `awaiting_review` row
    // nobody has accepted, which is the case the review found in production.
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(thresholds.assignment_stalled_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if let Ok(stalled) = store::assignments_idle_since(&app.db, &cutoff).await {
        for a in stalled {
            out.push(Observation {
                kind: "assignment_stalled".into(),
                resource: a.id.clone(),
                severity: "degraded".into(),
                detail: json!({
                    "assignment_id": a.id,
                    "bot_id": a.target_bot_id,
                    "status": a.status,
                    "updated_at": a.updated_at,
                })
                .to_string(),
            });
        }
    }

    // A notification nobody could deliver after every retry. Critical: this is the path the
    // manager learns anything through, and a silent one is worse than a loud failure.
    if let Ok(stuck) = store::exhausted_inbox(&app.db, thresholds.notify_max_attempts).await {
        for e in stuck {
            out.push(Observation {
                kind: "notify_exhausted".into(),
                resource: e.id.clone(),
                severity: "critical".into(),
                detail: json!({
                    "event_id": e.id,
                    "event_key": e.event_key,
                    "kind": e.kind,
                    "attempts": e.notify_attempts,
                    "error": e.notify_error,
                })
                .to_string(),
            });
        }
    }

    // The phone entry point, but only when there is evidence it is *broken*. `unknown` with an
    // unsupported capability is a documented limit, not a fault: opening an incident for it
    // would mean a permanent red light nobody can clear.
    let remote = super::remote::status(app).await;
    let remote_status = remote.get("status").and_then(Value::as_str).unwrap_or("unknown");
    if super::remote::severity(remote_status) == "degraded" {
        out.push(Observation {
            kind: "remote_entry".into(),
            resource: super::setup::REMOTE_NAME.to_string(),
            severity: "degraded".into(),
            detail: remote.to_string(),
        });
    }

    out
}

/// Apply one pass: write what changed, and queue one inbox event per transition.
pub async fn sweep(app: &Arc<App>, detector: &mut Detector) {
    let cfg = app.cfg.get().await;
    let thresholds = Thresholds::from_cfg(&cfg.supervisor);
    let observations = observe(app, &thresholds).await;
    let Ok(open) = store::open_incidents(&app.db).await else { return };
    let open_keys: Vec<(String, String)> = open.iter().map(|i| (i.kind.clone(), i.resource.clone())).collect();
    let plan = detector.plan(&observations, &open_keys, &thresholds, chrono::Utc::now().timestamp());

    for obs in plan.open {
        let detail: Value = serde_json::from_str(&obs.detail).unwrap_or_else(|_| json!({}));
        let Ok((incident, opened)) =
            store::open_incident(&app.db, &obs.kind, &obs.resource, &obs.severity, &detail).await
        else {
            continue;
        };
        if !opened {
            continue;
        }
        tracing::warn!(kind = %obs.kind, resource = %obs.resource, severity = %obs.severity, "system incident opened");
        let _ = store::push_inbox(
            &app.db,
            &format!("incident:{}:opened", incident.id),
            "incident_opened",
            None,
            None,
            None,
            &json!({"incident": incident.to_json()}),
        )
        .await;
        app.emit("supervisor_changed", json!({"incident": incident.to_json()})).await;
    }

    for (kind, resource) in plan.resolve {
        let Ok(Some(incident)) = store::resolve_incident(&app.db, &kind, &resource).await else { continue };
        tracing::info!(kind = %kind, resource = %resource, "system incident resolved");
        let _ = store::push_inbox(
            &app.db,
            &format!("incident:{}:resolved", incident.id),
            "incident_resolved",
            None,
            None,
            None,
            &json!({"incident": incident.to_json()}),
        )
        .await;
        app.emit("supervisor_changed", json!({"incident": incident.to_json()})).await;
    }
}

/// The system half of the health summary: severity, and the incidents behind it.
pub async fn system_health(app: &Arc<App>) -> Value {
    let open = store::open_incidents(&app.db).await.unwrap_or_default();
    let severity = open.iter().fold("healthy".to_string(), |acc, i| worst(&acc, &i.severity));
    json!({
        "status": severity,
        "open_incidents": open.len(),
        "incidents": open.iter().map(store::Incident::to_json).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(kind: &str, resource: &str) -> Observation {
        Observation {
            kind: kind.into(),
            resource: resource.into(),
            severity: "degraded".into(),
            detail: "{}".into(),
        }
    }

    fn thresholds() -> Thresholds {
        Thresholds {
            host_disconnected_secs: 120,
            bot_stopped_secs: 300,
            assignment_stalled_secs: 7200,
            notify_max_attempts: 5,
        }
    }

    /// A host that drops for ten seconds during a reconnect is not an outage. One that stays
    /// down past the threshold is, and it is written down exactly once.
    #[test]
    fn a_blip_is_not_an_incident_but_a_real_outage_is() {
        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs("host_disconnected", "mac2")];
        assert!(d.plan(&seen, &[], &t, 1000).open.is_empty(), "first sighting is not yet an incident");
        assert!(d.plan(&seen, &[], &t, 1119).open.is_empty(), "one second short of the threshold");
        assert_eq!(d.plan(&seen, &[], &t, 1120).open.len(), 1, "past the threshold it opens");
        // Already open: every later pass just refreshes it, and `sweep` only notifies on the
        // transition, so a five-hour outage stays one notification.
        assert_eq!(d.plan(&seen, &[("host_disconnected".into(), "mac2".into())], &t, 9999).open.len(), 1);
    }

    #[test]
    fn a_condition_that_clears_resolves_exactly_the_open_one() {
        let mut d = Detector::default();
        let t = thresholds();
        let open = vec![("host_disconnected".to_string(), "mac2".to_string())];
        let plan = d.plan(&[], &open, &t, 5000);
        assert_eq!(plan.resolve, open);
        assert!(plan.open.is_empty());
        // And the clock starts over, so a fault that comes back has to hold the threshold again
        // rather than re-opening on its first tick.
        let seen = [obs("host_disconnected", "mac2")];
        assert!(d.plan(&seen, &[], &t, 6000).open.is_empty());
    }

    /// The counter-churn case from 2026-09-10: bots starting and finishing all night is not a
    /// system fault and must not produce a single incident.
    #[test]
    fn bot_churn_produces_nothing() {
        let mut d = Detector::default();
        let t = thresholds();
        for i in 0..660 {
            assert!(d.plan(&[], &[], &t, 1000 + i * 30).open.is_empty());
        }
    }

    /// Stalled work and exhausted notifications carry their own age test, so they open on the
    /// first pass that sees them rather than waiting a second threshold.
    #[test]
    fn probes_that_already_tested_age_open_immediately() {
        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs("assignment_stalled", "a1"), obs("notify_exhausted", "e1")];
        assert_eq!(d.plan(&seen, &[], &t, 1000).open.len(), 2);
    }

    #[test]
    fn not_knowing_outranks_healthy_but_not_a_real_fault() {
        assert_eq!(worst("healthy", "unknown"), "unknown");
        assert_eq!(worst("unknown", "degraded"), "degraded");
        assert_eq!(worst("critical", "degraded"), "critical");
        assert_eq!(worst("healthy", "healthy"), "healthy");
    }
}
