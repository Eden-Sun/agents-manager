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
    /// `blind` names probe kinds that could not run this pass. Their incidents are left alone:
    /// an empty result from a query that errored is not evidence that the fault cleared.
    pub fn plan(
        &mut self,
        observations: &[Observation],
        open: &[(String, String)],
        blind: &[&str],
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
                // The stalled, undelivered and exhausted probes carry their own age test; a
                // second wait here would just double the threshold.
                _ => 0,
            };
            if held >= needed || open.contains(&key) {
                plan.open.push(obs.clone());
            }
        }
        // Anything open that nobody observed this pass has cleared — unless the probe that
        // would have seen it never ran, in which case we know nothing and say nothing.
        for key in open {
            if !seen.contains(key) && !blind.contains(&key.0.as_str()) {
                plan.resolve.push(key.clone());
            }
        }
        // A blind probe's timer is left alone too, so a fault that was already accumulating
        // does not have to start its threshold over because of one failed query.
        self.since.retain(|k, _| seen.contains(k) || blind.contains(&k.0.as_str()));
        plan
    }
}

/// What one sweep could see.
///
/// `failed` names the probes whose query errored. It matters because "the query said nothing is
/// wrong" and "the query did not answer" produce the same empty list, and treating the second
/// as the first makes the sweep *resolve* open incidents — announcing a recovery that nobody
/// observed. A probe that could not run keeps its incidents exactly where they are.
#[derive(Debug, Default)]
pub struct Probed {
    pub seen: Vec<Observation>,
    pub failed: Vec<&'static str>,
}

impl Probed {
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Read every cheap probe. No LLM, no process is killed to find out how it is doing: this runs
/// on the 30-second health tick and may not cost more than a few queries.
pub async fn observe(app: &Arc<App>, thresholds: &Thresholds) -> Probed {
    let mut probed = Probed::default();
    let out = &mut probed.seen;

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
    match crate::db::live_bots(&app.db).await {
        Ok(bots) => {
            for bot in bots {
                if bot.autostart == 0 {
                    continue;
                }
                match crate::db::active_run(&app.db, &bot.id).await {
                    Ok(None) => out.push(Observation {
                        kind: "bot_stopped".into(),
                        resource: bot.id.clone(),
                        severity: "degraded".into(),
                        detail: json!({"bot_id": bot.id, "name": bot.name, "expected": "autostart"}).to_string(),
                    }),
                    Ok(Some(_)) => {}
                    // Could not tell whether this bot is running. Not knowing is not "it is fine".
                    Err(e) => {
                        tracing::warn!(bot = %bot.id, error = ?e, "bot_stopped probe failed");
                        probed.failed.push("bot_stopped");
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = ?e, "bot_stopped probe failed");
            probed.failed.push("bot_stopped");
        }
    }

    // Work that has not moved in hours. `updated_at` moves on every retry and every delivery,
    // so this only fires on something genuinely stuck — including an `awaiting_review` row
    // nobody has accepted, which is the case the review found in production.
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(thresholds.assignment_stalled_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    match store::assignments_idle_since(&app.db, &cutoff).await {
        Err(e) => {
            tracing::warn!(error = ?e, "assignment_stalled probe failed");
            probed.failed.push("assignment_stalled");
        }
        Ok(stalled) => {
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
    }

    // Work that has never been handed over at all — the stopped-bot case. The idle probe above
    // is blind to it: every retry moves `updated_at`, so an assignment bouncing off a stopped
    // bot every five minutes looks busy forever. The detail carries the retry count and the
    // last refusal, which is what makes it actionable instead of just red.
    match store::assignments_undelivered_since(&app.db, &cutoff).await {
        Err(e) => {
            tracing::warn!(error = ?e, "assignment_undelivered probe failed");
            probed.failed.push("assignment_undelivered");
        }
        Ok(undelivered) => {
        for a in undelivered {
            out.push(Observation {
                kind: "assignment_undelivered".into(),
                resource: a.id.clone(),
                severity: "degraded".into(),
                detail: json!({
                    "assignment_id": a.id,
                    "bot_id": a.target_bot_id,
                    "attempts": a.attempts,
                    "last_error": a.error,
                    "next_attempt_at": a.next_attempt_at,
                    "created_at": a.created_at,
                })
                .to_string(),
            });
        }
        }
    }

    // A notification nobody could deliver after every retry. Critical: this is the path the
    // manager learns anything through, and a silent one is worse than a loud failure.
    match store::exhausted_inbox(&app.db, thresholds.notify_max_attempts).await {
        Err(e) => {
            tracing::warn!(error = ?e, "notify_exhausted probe failed");
            probed.failed.push("notify_exhausted");
        }
        Ok(stuck) => {
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

    probed
}

/// Incidents whose whole point is that the inbox is not working. Queueing an inbox event for
/// them is how a stuck notification becomes two stuck notifications: the event cannot be
/// delivered either, so it exhausts its own retries, which opens another incident, and so on.
/// They are still written down and still show up in `system_health` and on the UI — what they
/// do not get is a wake-up down the very channel they are reporting as broken.
fn notifiable(kind: &str) -> bool {
    kind != "notify_exhausted"
}

/// Apply one pass: write what changed, and queue one inbox event per transition.
pub async fn sweep(app: &Arc<App>, detector: &mut Detector) {
    let cfg = app.cfg.get().await;
    let thresholds = Thresholds::from_cfg(&cfg.supervisor);
    let probed = observe(app, &thresholds).await;
    // Cannot read what is already open: do nothing at all rather than guess in either direction.
    let Ok(open) = store::open_incidents(&app.db).await else {
        tracing::warn!("incident sweep skipped: could not read the open incidents");
        return;
    };
    let open_keys: Vec<(String, String)> = open.iter().map(|i| (i.kind.clone(), i.resource.clone())).collect();
    if !probed.ok() {
        tracing::warn!(blind = ?probed.failed, "some incident probes could not run; their incidents are left as they are");
    }
    let plan = detector.plan(
        &probed.seen,
        &open_keys,
        &probed.failed,
        &thresholds,
        chrono::Utc::now().timestamp(),
    );

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
        if notifiable(&obs.kind) {
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
        }
        app.emit("supervisor_changed", json!({"incident": incident.to_json()})).await;
    }

    for (kind, resource) in plan.resolve {
        let Ok(Some(incident)) = store::resolve_incident(&app.db, &kind, &resource).await else { continue };
        tracing::info!(kind = %kind, resource = %resource, "system incident resolved");
        if notifiable(&kind) {
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
        }
        app.emit("supervisor_changed", json!({"incident": incident.to_json()})).await;
    }
}

/// The system half of the health summary: severity, and the incidents behind it.
pub async fn system_health(app: &Arc<App>) -> Value {
    // `unwrap_or_default()` here used to turn a failed query into an empty list, and an empty
    // list into `healthy` — the summary claimed the system was fine on the strength of a read
    // that never happened.
    let open = match store::open_incidents(&app.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = ?e, "could not read open incidents");
            return json!({
                "status": "unknown",
                "error": "could not read the incident table",
                "open_incidents": null,
                "incidents": [],
            });
        }
    };
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
        assert!(d.plan(&seen, &[], &[], &t, 1000).open.is_empty(), "first sighting is not yet an incident");
        assert!(d.plan(&seen, &[], &[], &t, 1119).open.is_empty(), "one second short of the threshold");
        assert_eq!(d.plan(&seen, &[], &[], &t, 1120).open.len(), 1, "past the threshold it opens");
        // Already open: every later pass just refreshes it, and `sweep` only notifies on the
        // transition, so a five-hour outage stays one notification.
        assert_eq!(d.plan(&seen, &[("host_disconnected".into(), "mac2".into())], &[], &t, 9999).open.len(), 1);
    }

    #[test]
    fn a_condition_that_clears_resolves_exactly_the_open_one() {
        let mut d = Detector::default();
        let t = thresholds();
        let open = vec![("host_disconnected".to_string(), "mac2".to_string())];
        let plan = d.plan(&[], &open, &[], &t, 5000);
        assert_eq!(plan.resolve, open);
        assert!(plan.open.is_empty());
        // And the clock starts over, so a fault that comes back has to hold the threshold again
        // rather than re-opening on its first tick.
        let seen = [obs("host_disconnected", "mac2")];
        assert!(d.plan(&seen, &[], &[], &t, 6000).open.is_empty());
    }

    /// The counter-churn case from 2026-09-10: bots starting and finishing all night is not a
    /// system fault and must not produce a single incident.
    #[test]
    fn bot_churn_produces_nothing() {
        let mut d = Detector::default();
        let t = thresholds();
        for i in 0..660 {
            assert!(d.plan(&[], &[], &[], &t, 1000 + i * 30).open.is_empty());
        }
    }

    /// Stalled work and exhausted notifications carry their own age test, so they open on the
    /// first pass that sees them rather than waiting a second threshold.
    #[test]
    fn probes_that_already_tested_age_open_immediately() {
        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs("assignment_stalled", "a1"), obs("notify_exhausted", "e1")];
        assert_eq!(d.plan(&seen, &[], &[], &t, 1000).open.len(), 2);
    }

    /// A probe whose query errored returns no observations — exactly like a probe that looked
    /// and found nothing. Treating them the same makes the sweep announce a recovery nobody
    /// saw, which is worse than silence: it closes an incident that is still happening.
    #[test]
    fn a_probe_that_could_not_run_never_resolves_its_incidents() {
        let mut d = Detector::default();
        let t = thresholds();
        let open = vec![
            ("host_disconnected".to_string(), "mac2".to_string()),
            ("assignment_stalled".to_string(), "a1".to_string()),
        ];
        // The stalled probe failed this pass; the host probe ran and saw nothing.
        let plan = d.plan(&[], &open, &["assignment_stalled"], &t, 5000);
        assert_eq!(
            plan.resolve,
            vec![("host_disconnected".to_string(), "mac2".to_string())],
            "only the probe that actually looked may close its incident"
        );
        // And once it can run again and still sees nothing, it resolves normally.
        let plan = d.plan(&[], &open, &[], &t, 5030);
        assert_eq!(plan.resolve.len(), 2);
    }

    /// A blind pass must not restart a threshold that was already accumulating, or a fault
    /// could dodge every incident by coinciding with an intermittent query failure.
    #[test]
    fn a_blind_pass_does_not_reset_a_threshold_in_progress() {
        let mut d = Detector::default();
        let t = thresholds();
        let seen = [obs("host_disconnected", "mac2")];
        assert!(d.plan(&seen, &[], &[], &t, 1000).open.is_empty(), "clock starts");
        // The probe fails for a while: no observation, but the fault is not known to be gone.
        for at in [1030, 1060, 1090] {
            assert!(d.plan(&[], &[], &["host_disconnected"], &t, at).open.is_empty());
        }
        // Back up, still down: the original sighting still counts, so it opens on time.
        assert_eq!(d.plan(&seen, &[], &[], &t, 1120).open.len(), 1, "threshold measured from the first sighting");
    }

    /// The incidents that say "the inbox is broken" must not be announced through the inbox.
    #[test]
    fn the_broken_notification_channel_is_not_used_to_report_itself() {
        assert!(!notifiable("notify_exhausted"), "this one would retry, exhaust, and open another incident");
        for kind in ["host_disconnected", "bot_stopped", "assignment_stalled", "assignment_undelivered", "remote_entry"] {
            assert!(notifiable(kind), "{kind} is safe to wake the manager about");
        }
    }

    #[test]
    fn not_knowing_outranks_healthy_but_not_a_real_fault() {
        assert_eq!(worst("healthy", "unknown"), "unknown");
        assert_eq!(worst("unknown", "degraded"), "degraded");
        assert_eq!(worst("critical", "degraded"), "critical");
        assert_eq!(worst("healthy", "healthy"), "healthy");
    }
}
