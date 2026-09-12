//! The candidate / quota state machine, as a pure function.
//!
//! `cc0` is one account with two candidates, `fable` and `opus`. Fable has its own weekly
//! bucket; the 5-hour and 7-day windows are shared by both. So there are exactly three
//! things to know, and [`decide`] says which one applies this tick:
//!
//! * a shared window is at its limit → nothing to switch *to*: park on `waiting_quota` and
//!   record when it comes back (`quota_reset_at`);
//! * on `fable` with its bucket nearly empty → move to `opus`;
//! * on `opus` with the Fable bucket back → move home, after the same 30-minute cooldown.
//!
//! Two rules keep it from lying. A window whose `resets_at` has passed has been refilled no
//! matter what the (stale) reading says — the `/usage` probe only runs once a minute and only
//! while an account can answer. And a switch is only *proposed* when the manager is idle with
//! no turn in flight (or not running at all): `/model` mid-turn is how a user's message gets
//! eaten (b95142a), so a busy manager gets [`Decision::Defer`] and the next tick asks again.

use crate::quota::{Quota, Window};
use chrono::{DateTime, Utc};

use super::store::Supervisor;

/// Leave Fable when its weekly bucket has less than this left.
pub const FABLE_MIN_REMAINING_PCT: f64 = 5.0;
/// Come back to Fable only once the bucket shows at least this much — a wider gap than the
/// exit threshold, so one reading on the line does not flip the manager back and forth.
pub const FABLE_RETURN_REMAINING_PCT: f64 = 20.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Keep,
    /// A shared window is critical: both candidates are out. `reset_at` = the nearest reset
    /// among the critical windows, `None` when the reading has no timestamp.
    WaitQuota { reset_at: Option<String> },
    /// The shared windows are usable again: leave `waiting_quota`.
    Resume,
    /// Switch now. `reset_at` is what `quota_reset_at` should say afterwards: leaving Fable
    /// records when its bucket refills; coming home clears it.
    SwitchTo { model: &'static str, reason: String, reset_at: Option<String> },
    /// A switch is due, but the manager is mid-turn. Ask again next tick.
    Defer { model: &'static str },
}

pub fn past(iso: &str, now: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(iso) {
        Ok(t) => t <= now,
        // An unreadable timestamp must not park the manager forever.
        Err(_) => true,
    }
}

fn reset_passed(w: &Window, now: DateTime<Utc>) -> bool {
    w.resets_at.as_deref().is_some_and(|t| past(t, now))
}

/// What is left as far as we can tell. Past its reset the window is full again, whatever
/// the last reading said — the reading is simply older than the reset.
fn effective_remaining(w: &Window, now: DateTime<Utc>) -> f64 {
    if reset_passed(w, now) {
        100.0
    } else {
        (100.0 - w.used_pct).max(0.0)
    }
}

fn shared_critical(w: &Window, now: DateTime<Utc>) -> bool {
    !reset_passed(w, now) && w.critical()
}

fn cooldown_over(sup: &Supervisor, now: DateTime<Utc>) -> bool {
    sup.cooldown_until.as_deref().map(|t| past(t, now)).unwrap_or(true)
}

/// `liveness` is [`super::manager_liveness`]: `stopped` | `starting` | `busy` | `idle`.
pub fn decide(sup: &Supervisor, quota: Option<&Quota>, liveness: &str, now: DateTime<Utc>) -> Decision {
    if sup.bot_id.is_none() {
        return Decision::Keep;
    }
    let Some(q) = quota else {
        // No reading is not a full account. The one thing a missing reading may end is a
        // wait whose recorded reset has come and gone.
        if sup.status == "waiting_quota" && sup.quota_reset_at.as_deref().is_some_and(|t| past(t, now)) {
            return Decision::Resume;
        }
        return Decision::Keep;
    };

    // Shared windows first: with the 5-hour or 7-day bucket gone there is nothing to switch to.
    let critical: Vec<&Window> =
        [&q.five_hour, &q.seven_day].into_iter().flatten().filter(|w| shared_critical(w, now)).collect();
    if !critical.is_empty() {
        let reset_at = critical.iter().filter_map(|w| w.resets_at.clone()).min();
        if sup.status == "waiting_quota" && sup.quota_reset_at == reset_at {
            return Decision::Keep;
        }
        return Decision::WaitQuota { reset_at };
    }
    if sup.status == "waiting_quota" {
        // `waiting_quota` is also where `switch_candidate` parks after both candidates were
        // tried inside one cooldown. The shared windows being fine says nothing about that
        // wait (Fable is the empty one), and ending it early would just re-enter it on the
        // next tick; it ends with the cooldown or the reset it wrote down, whichever first.
        let budget_spent = sup.fallback_tries >= 1 && !cooldown_over(sup, now);
        let reset_passed = sup.quota_reset_at.as_deref().map(|t| past(t, now)).unwrap_or(true);
        return if budget_spent && !reset_passed { Decision::Keep } else { Decision::Resume };
    }

    let idle = matches!(liveness, "idle" | "stopped");
    let fable = q.fable.as_ref();
    if sup.active_model == "fable" {
        let Some(w) = fable else { return Decision::Keep };
        if effective_remaining(w, now) >= FABLE_MIN_REMAINING_PCT {
            return Decision::Keep;
        }
        if !idle {
            return Decision::Defer { model: "opus" };
        }
        return Decision::SwitchTo {
            model: "opus",
            reason: format!("fable 剩餘額度低於 {FABLE_MIN_REMAINING_PCT:.0}%"),
            reset_at: w.resets_at.clone(),
        };
    }
    // On opus: go home once Fable is back and the cooldown from the last switch is over.
    let Some(w) = fable else { return Decision::Keep };
    if effective_remaining(w, now) < FABLE_RETURN_REMAINING_PCT || !cooldown_over(sup, now) {
        return Decision::Keep;
    }
    if !idle {
        return Decision::Defer { model: "fable" };
    }
    Decision::SwitchTo {
        model: "fable",
        reason: if reset_passed(w, now) {
            "fable 週額度已重置".to_string()
        } else {
            format!("fable 剩餘額度回到 {:.0}%", effective_remaining(w, now))
        },
        reset_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-09-11T06:30:00Z";
    const EARLIER: &str = "2026-09-11T06:00:00Z";
    const LATER: &str = "2026-09-11T09:00:00Z";
    const MUCH_LATER: &str = "2026-09-14T04:00:00Z";

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&Utc)
    }

    fn w(used: f64, resets: &str) -> Option<Window> {
        Some(Window { used_pct: used, resets_at: Some(resets.into()) })
    }

    fn quota(five: Option<Window>, seven: Option<Window>, fable: Option<Window>) -> Quota {
        Quota {
            five_hour: five,
            seven_day: seven,
            fable,
            reset_credits: None,
            plan: None,
            updated_at: NOW.into(),
            source: "test".into(),
            account: Some("cc0".into()),
            host: "local".into(),
        }
    }

    fn sup(model: &str, status: &str, cooldown: Option<&str>, reset: Option<&str>) -> Supervisor {
        Supervisor {
            id: "AGM".into(),
            bot_id: Some("b1".into()),
            project_id: None,
            cwd: None,
            identity: "cc0".into(),
            effort: "low".into(),
            active_model: model.into(),
            generation: 2,
            status: status.into(),
            status_detail: None,
            fallback_tries: 0,
            cooldown_until: cooldown.map(String::from),
            quota_reset_at: reset.map(String::from),
            remote_status: "unknown".into(),
            remote_url: None,
            summary: None,
            summary_version: 0,
            desired_running: 1,
            watchdog_attempts: 0,
            watchdog_next_at: None,
            last_notify_at: None,
            created_at: "t".into(),
            updated_at: "t".into(),
        }
    }

    fn healthy() -> Quota {
        quota(w(24.0, LATER), w(40.0, MUCH_LATER), w(10.0, MUCH_LATER))
    }

    #[test]
    fn no_reading_is_not_a_full_account() {
        assert_eq!(decide(&sup("fable", "", None, None), None, "idle", now()), Decision::Keep);
        assert_eq!(decide(&sup("opus", "", None, None), None, "idle", now()), Decision::Keep);
        // …but a wait whose recorded reset has passed does end.
        assert_eq!(decide(&sup("opus", "waiting_quota", None, Some(EARLIER)), None, "idle", now()), Decision::Resume);
        assert_eq!(decide(&sup("opus", "waiting_quota", None, Some(LATER)), None, "idle", now()), Decision::Keep);
    }

    #[test]
    fn a_critical_shared_window_parks_the_manager_with_the_nearest_reset() {
        // 5h at 97% used, 7d at 96% used: both critical, the 5h window resets first.
        let q = quota(w(97.0, LATER), w(96.0, MUCH_LATER), w(10.0, MUCH_LATER));
        assert_eq!(
            decide(&sup("fable", "", None, None), Some(&q), "idle", now()),
            Decision::WaitQuota { reset_at: Some(LATER.into()) }
        );
        // Already waiting on that reset: nothing to re-announce.
        assert_eq!(decide(&sup("fable", "waiting_quota", None, Some(LATER)), Some(&q), "idle", now()), Decision::Keep);
        // A fresh reading with a different reset updates the wait.
        let q2 = quota(w(97.0, MUCH_LATER), None, None);
        assert_eq!(
            decide(&sup("fable", "waiting_quota", None, Some(LATER)), Some(&q2), "idle", now()),
            Decision::WaitQuota { reset_at: Some(MUCH_LATER.into()) }
        );
    }

    #[test]
    fn a_shared_limit_outranks_any_candidate_switch() {
        // Fable at 4% *and* the 5h window gone: switching to opus would find no quota either.
        let q = quota(w(99.0, LATER), w(40.0, MUCH_LATER), w(96.0, MUCH_LATER));
        assert!(matches!(decide(&sup("fable", "", None, None), Some(&q), "idle", now()), Decision::WaitQuota { .. }));
    }

    #[test]
    fn the_wait_ends_when_the_shared_windows_are_back() {
        // Fresh reading, usable again.
        assert_eq!(decide(&sup("fable", "waiting_quota", None, Some(LATER)), Some(&healthy()), "idle", now()), Decision::Resume);
        // Stale reading still says 99% used, but its reset has passed: refilled.
        let stale = quota(w(99.0, EARLIER), w(40.0, MUCH_LATER), w(10.0, MUCH_LATER));
        assert_eq!(decide(&sup("fable", "waiting_quota", None, Some(EARLIER)), Some(&stale), "idle", now()), Decision::Resume);
        // Parked by "both candidates tried" (budget spent, cooldown running) with the reset
        // still ahead: keep waiting — resuming would only re-enter the wait next tick.
        let mut both = sup("opus", "waiting_quota", Some(LATER), Some(LATER));
        both.fallback_tries = 1;
        assert_eq!(decide(&both, Some(&healthy()), "idle", now()), Decision::Keep);
        // …until the cooldown is over, which gives the next switch its own budget.
        let mut over = sup("opus", "waiting_quota", Some(EARLIER), Some(LATER));
        over.fallback_tries = 1;
        assert_eq!(decide(&over, Some(&healthy()), "idle", now()), Decision::Resume);
    }

    #[test]
    fn fable_nearly_empty_moves_to_opus_only_when_idle() {
        let q = quota(w(24.0, LATER), w(40.0, MUCH_LATER), w(96.0, MUCH_LATER));
        assert_eq!(
            decide(&sup("fable", "", None, None), Some(&q), "idle", now()),
            Decision::SwitchTo { model: "opus", reason: "fable 剩餘額度低於 5%".into(), reset_at: Some(MUCH_LATER.into()) }
        );
        // Not running: the config switch is safe, the next start comes up on opus.
        assert!(matches!(decide(&sup("fable", "", None, None), Some(&q), "stopped", now()), Decision::SwitchTo { model: "opus", .. }));
        for l in ["busy", "starting"] {
            assert_eq!(decide(&sup("fable", "", None, None), Some(&q), l, now()), Decision::Defer { model: "opus" }, "{l}");
        }
        // 5% exactly is not "below 5%".
        let edge = quota(w(24.0, LATER), w(40.0, MUCH_LATER), w(95.0, MUCH_LATER));
        assert_eq!(decide(&sup("fable", "", None, None), Some(&edge), "idle", now()), Decision::Keep);
    }

    #[test]
    fn a_stale_fable_reading_past_its_reset_does_not_chase_the_manager_off_fable() {
        let stale = quota(w(24.0, LATER), w(40.0, MUCH_LATER), w(98.0, EARLIER));
        assert_eq!(decide(&sup("fable", "", None, None), Some(&stale), "idle", now()), Decision::Keep);
    }

    #[test]
    fn opus_comes_home_once_fable_is_back_and_the_cooldown_is_over() {
        let back = quota(w(24.0, LATER), w(40.0, MUCH_LATER), w(70.0, MUCH_LATER));
        assert_eq!(
            decide(&sup("opus", "", Some(EARLIER), Some(EARLIER)), Some(&back), "idle", now()),
            Decision::SwitchTo { model: "fable", reason: "fable 剩餘額度回到 30%".into(), reset_at: None }
        );
        assert!(matches!(decide(&sup("opus", "", None, None), Some(&back), "stopped", now()), Decision::SwitchTo { model: "fable", .. }));
        // Cooldown still running: the plan's bound, one switch per window.
        assert_eq!(decide(&sup("opus", "", Some(LATER), None), Some(&back), "idle", now()), Decision::Keep);
        // Mid-turn: wait.
        assert_eq!(decide(&sup("opus", "", Some(EARLIER), None), Some(&back), "busy", now()), Decision::Defer { model: "fable" });
        // 19% is not enough; 20% is.
        let low = quota(w(24.0, LATER), w(40.0, MUCH_LATER), w(81.0, MUCH_LATER));
        assert_eq!(decide(&sup("opus", "", None, None), Some(&low), "idle", now()), Decision::Keep);
        let edge = quota(w(24.0, LATER), w(40.0, MUCH_LATER), w(80.0, MUCH_LATER));
        assert!(matches!(decide(&sup("opus", "", None, None), Some(&edge), "idle", now()), Decision::SwitchTo { model: "fable", .. }));
    }

    #[test]
    fn a_passed_fable_reset_counts_as_refilled_even_if_the_reading_is_stale() {
        // The 2026-09-11 case: switched away at 98% used, the bucket reset at 06:00Z, the
        // probe has not answered since. The reset passing is the signal.
        let stale = quota(w(24.0, LATER), w(40.0, MUCH_LATER), w(98.0, EARLIER));
        assert_eq!(
            decide(&sup("opus", "", Some(EARLIER), Some(EARLIER)), Some(&stale), "idle", now()),
            Decision::SwitchTo { model: "fable", reason: "fable 週額度已重置".into(), reset_at: None }
        );
    }

    #[test]
    fn without_a_fable_reading_nobody_moves() {
        let q = quota(w(24.0, LATER), w(40.0, MUCH_LATER), None);
        assert_eq!(decide(&sup("fable", "", None, None), Some(&q), "idle", now()), Decision::Keep);
        assert_eq!(decide(&sup("opus", "", None, None), Some(&q), "idle", now()), Decision::Keep);
    }
}
