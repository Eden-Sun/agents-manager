//! Bring the manager back when it dies.
//!
//! 2026-09-10 23:02Z: AGM was killed and nothing started it again for five and a half hours —
//! the one bot whose job is to notice a bot is down was the bot that was down. So the daemon
//! watches it. The rule is narrow on purpose: only a manager that is *wanted* (started once by
//! a human, not stopped since through `POST /supervisor/stop`) and not parked on
//! `waiting_quota` is restarted; a stop the user asked for stays stopped, and a fresh `setup`
//! stays down until someone starts it.
//!
//! Every decision is pure ([`plan`]) over the supervisor row plus the manager's liveness, so
//! the loop in `controller` just executes it. State lives in the row (`watchdog_attempts`,
//! `watchdog_next_at`), not in the task: two controller loops of the same generation see the
//! same count, and a daemon restart does not reset a failure streak to zero.

use crate::state::App;
use std::sync::Arc;

use super::store::{self, Supervisor};

/// Seconds between one failed start and the next. Flat after the fourth: a manager that will
/// not come up is retried every five minutes until [`MAX_ATTEMPTS`], not forever.
pub const BACKOFF_SECS: [u64; 4] = [30, 60, 120, 300];
/// Consecutive failed automatic starts before the watchdog reports and stops.
pub const MAX_ATTEMPTS: i64 = 5;

pub fn backoff_secs(failures: i64) -> u64 {
    let i = (failures.max(0) as usize).min(BACKOFF_SECS.len() - 1);
    BACKOFF_SECS[i]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to do: not wanted, not stopped, or parked on quota.
    Idle,
    /// Stopped and wanted, but the backoff has not run out yet. `schedule` = the next attempt
    /// still has to be written down (first observation of the outage).
    Wait { schedule: bool },
    /// The backoff ran out: start it now.
    Start,
    /// [`MAX_ATTEMPTS`] failures in a row: reported, no further retries until a human starts it.
    GaveUp,
}

/// `liveness` is [`super::manager_liveness`]: `stopped` | `starting` | `busy` | `idle`.
pub fn plan(sup: &Supervisor, liveness: &str, now_past: impl Fn(&str) -> bool) -> Plan {
    if sup.bot_id.is_none() || !sup.wants_running() || sup.status == "waiting_quota" {
        return Plan::Idle;
    }
    if liveness != "stopped" {
        return Plan::Idle;
    }
    if sup.watchdog_attempts >= MAX_ATTEMPTS {
        return Plan::GaveUp;
    }
    match sup.watchdog_next_at.as_deref() {
        None => Plan::Wait { schedule: true },
        Some(t) if !now_past(t) => Plan::Wait { schedule: false },
        Some(_) => Plan::Start,
    }
}

fn iso_in(secs: u64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn past(iso: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => t <= chrono::Utc::now(),
        Err(_) => true,
    }
}

/// One controller tick's worth of watching.
pub async fn tick(app: &Arc<App>) {
    let Ok(sup) = store::get_or_init(&app.db).await else { return };
    let Some(bot_id) = sup.bot_id.clone() else { return };
    let liveness = super::manager_liveness(app, &bot_id).await.unwrap_or("stopped");
    match plan(&sup, liveness, past) {
        Plan::Idle => {
            // Seen answering prompts again: the streak is over. `starting` is not enough — a
            // CLI that opens and dies would otherwise reset the count on every attempt.
            if matches!(liveness, "idle" | "busy") && (sup.watchdog_attempts > 0 || sup.watchdog_next_at.is_some()) {
                let _ = store::set_watchdog(&app.db, 0, None).await;
            }
        }
        Plan::Wait { schedule: true } => {
            let wait = backoff_secs(sup.watchdog_attempts);
            tracing::info!(attempts = sup.watchdog_attempts, wait, "supervisor is down and wanted; scheduling an automatic start");
            let _ = store::set_watchdog(&app.db, sup.watchdog_attempts, Some(&iso_in(wait))).await;
        }
        Plan::Wait { schedule: false } | Plan::GaveUp => {}
        Plan::Start => start(app, &bot_id, sup.watchdog_attempts).await,
    }
}

async fn start(app: &Arc<App>, bot_id: &str, failures: i64) {
    let _g = super::lock().await;
    // Re-read under the lock: `start`, `stop` or a bulk restart may have moved it meanwhile.
    let Ok(sup) = store::get_or_init(&app.db).await else { return };
    let liveness = super::manager_liveness(app, bot_id).await.unwrap_or("stopped");
    if plan(&sup, liveness, past) != Plan::Start {
        return;
    }
    let attempt = failures + 1;
    let detail = format!("watchdog 自動重新啟動（第 {attempt} 次）");
    match super::start_manager(app, Some(&detail)).await {
        Ok(()) => {
            tracing::info!(attempt, "supervisor watchdog started the manager");
            // Counted until it is seen idle/busy: a CLI that dies right after opening keeps
            // climbing the backoff instead of restarting every 30 seconds.
            let _ = store::set_watchdog(&app.db, attempt, Some(&iso_in(backoff_secs(attempt)))).await;
        }
        Err(e) => {
            let why = format!("{e:?}");
            tracing::warn!(attempt, error = %why, "supervisor watchdog failed to start the manager");
            if attempt >= MAX_ATTEMPTS {
                let _ = store::set_watchdog(&app.db, attempt, None).await;
                let _ = store::set_status_detail(
                    &app.db,
                    Some(&format!("watchdog 連續 {MAX_ATTEMPTS} 次自動啟動失敗，已停止重試；請手動 supervisor-start。最後錯誤：{why}")),
                )
                .await;
                app.emit("supervisor_changed", serde_json::json!({"watchdog": "gave_up"})).await;
            } else {
                let _ = store::set_watchdog(&app.db, attempt, Some(&iso_in(backoff_secs(attempt)))).await;
                let _ = store::set_status_detail(&app.db, Some(&format!("watchdog 自動啟動失敗（第 {attempt} 次）：{why}"))).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sup(wanted: bool, status: &str, attempts: i64, next: Option<&str>) -> Supervisor {
        Supervisor {
            id: "AGM".into(),
            bot_id: Some("b1".into()),
            project_id: None,
            cwd: None,
            identity: "cc0".into(),
            effort: "low".into(),
            active_model: "fable".into(),
            generation: 1,
            status: status.into(),
            status_detail: None,
            fallback_tries: 0,
            cooldown_until: None,
            quota_reset_at: None,
            remote_status: "unknown".into(),
            remote_url: None,
            summary: None,
            summary_version: 0,
            desired_running: i64::from(wanted),
            watchdog_attempts: attempts,
            watchdog_next_at: next.map(String::from),
            last_notify_at: None,
            persona_text: None,
            persona_version: 0,
            persona_hash: None,
            persona_source: None,
            persona_updated_at: None,
            persona_seed_hash: None,
            created_at: "now".into(),
            updated_at: "now".into(),
        }
    }

    #[test]
    fn the_backoff_is_30_60_120_300_and_then_flat() {
        assert_eq!([0, 1, 2, 3, 4, 9].map(backoff_secs), [30, 60, 120, 300, 300, 300]);
    }

    #[test]
    fn a_manager_the_user_stopped_stays_stopped() {
        assert_eq!(plan(&sup(false, "", 0, None), "stopped", |_| true), Plan::Idle);
        // Never started at all (fresh setup): not the watchdog's to bring up.
        let mut s = sup(false, "", 0, None);
        s.generation = 0;
        assert_eq!(plan(&s, "stopped", |_| true), Plan::Idle);
    }

    #[test]
    fn waiting_for_quota_is_not_an_outage() {
        assert_eq!(plan(&sup(true, "waiting_quota", 0, None), "stopped", |_| true), Plan::Idle);
    }

    #[test]
    fn a_running_manager_needs_nothing() {
        for l in ["idle", "busy", "starting"] {
            assert_eq!(plan(&sup(true, "", 2, Some("x")), l, |_| true), Plan::Idle, "{l}");
        }
    }

    #[test]
    fn the_first_observation_schedules_and_the_deadline_starts() {
        assert_eq!(plan(&sup(true, "", 0, None), "stopped", |_| true), Plan::Wait { schedule: true });
        assert_eq!(plan(&sup(true, "", 0, Some("later")), "stopped", |_| false), Plan::Wait { schedule: false });
        assert_eq!(plan(&sup(true, "", 0, Some("earlier")), "stopped", |_| true), Plan::Start);
    }

    #[test]
    fn five_failures_and_it_stops_trying() {
        assert_eq!(plan(&sup(true, "", 4, Some("earlier")), "stopped", |_| true), Plan::Start);
        assert_eq!(plan(&sup(true, "", 5, None), "stopped", |_| true), Plan::GaveUp);
        assert_eq!(plan(&sup(true, "", 5, Some("earlier")), "stopped", |_| true), Plan::GaveUp);
    }
}
