use super::apply::ApplyRun;
use super::plan::Plan;
use crate::config::HostBaselineSyncMode;
use std::time::{Duration, Instant};

pub const MAX_FAILURES: u32 = 3;
const BACKOFF: [Duration; 2] = [Duration::from_secs(300), Duration::from_secs(1800)];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoState {
    pub plan_rev: String,
    pub done: bool,
    pub failures: u32,
    pub retry_after: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    Apply,
    Skip(&'static str),
}

pub fn gate(mode: HostBaselineSyncMode, plan: &Plan, state: &AutoState, now: Instant) -> Gate {
    if mode != HostBaselineSyncMode::Auto {
        return Gate::Skip("not_auto");
    }
    if plan.fixable().is_empty() {
        return Gate::Skip("nothing_to_fix");
    }
    if state.plan_rev != plan.plan_rev {
        return Gate::Apply;
    }
    if state.done {
        return Gate::Skip("already_applied");
    }
    if state.failures >= MAX_FAILURES {
        return Gate::Skip("gave_up");
    }
    if state.retry_after.is_some_and(|t| now < t) {
        return Gate::Skip("backoff");
    }
    Gate::Apply
}

pub fn all_applied(run: &ApplyRun) -> bool {
    run.error.is_none() && !run.outcomes.is_empty() && run.outcomes.iter().all(|o| matches!(o.result, "applied" | "present"))
}

pub fn record(state: &mut AutoState, plan_rev: &str, ok: bool, now: Instant) -> Option<Duration> {
    if state.plan_rev != plan_rev {
        *state = AutoState { plan_rev: plan_rev.to_string(), ..Default::default() };
    }
    if ok {
        state.done = true;
        state.failures = 0;
        state.retry_after = None;
        return None;
    }
    state.failures += 1;
    let delay = BACKOFF.get(state.failures as usize - 1).copied();
    state.retry_after = delay.map(|d| now + d);
    delay
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_baseline::manifest;
    use crate::host_baseline::plan::build;
    use crate::host_baseline::{BaselineIssue, CRITICAL, WARN};

    fn plan_with_fix() -> Plan {
        let issues = vec![BaselineIssue { id: "codex.config.toml:features.hooks".into(), severity: WARN, message: String::new(), note: None }];
        build(manifest::shipped().unwrap(), manifest::shipped_values().unwrap(), &issues)
    }

    fn plan_nothing() -> Plan {
        let issues = vec![BaselineIssue { id: "tool.herdr".into(), severity: CRITICAL, message: String::new(), note: None }];
        build(manifest::shipped().unwrap(), manifest::shipped_values().unwrap(), &issues)
    }

    #[test]
    fn off_and_ask_never_apply() {
        let p = plan_with_fix();
        let s = AutoState::default();
        assert_eq!(gate(HostBaselineSyncMode::Off, &p, &s, Instant::now()), Gate::Skip("not_auto"));
        assert_eq!(gate(HostBaselineSyncMode::Ask, &p, &s, Instant::now()), Gate::Skip("not_auto"));
    }

    #[test]
    fn auto_applies_a_fixable_plan_once_and_not_again_for_the_same_plan() {
        let p = plan_with_fix();
        let now = Instant::now();
        let mut s = AutoState::default();
        assert_eq!(gate(HostBaselineSyncMode::Auto, &p, &s, now), Gate::Apply);
        record(&mut s, &p.plan_rev, true, now);
        assert_eq!(gate(HostBaselineSyncMode::Auto, &p, &s, now), Gate::Skip("already_applied"));
    }

    #[test]
    fn the_agy_rtk_gap_follows_the_auto_ask_off_gate() {
        let issues = vec![BaselineIssue { id: "agy.rtk_rewrite".into(), severity: WARN, message: String::new(), note: None }];
        let p = build(manifest::shipped().unwrap(), manifest::shipped_values().unwrap(), &issues);
        let s = AutoState::default();
        assert_eq!(gate(HostBaselineSyncMode::Auto, &p, &s, Instant::now()), Gate::Apply);
        assert_eq!(gate(HostBaselineSyncMode::Ask, &p, &s, Instant::now()), Gate::Skip("not_auto"));
        assert_eq!(gate(HostBaselineSyncMode::Off, &p, &s, Instant::now()), Gate::Skip("not_auto"));
    }

    #[test]
    fn nothing_fixable_is_skipped() {
        let p = plan_nothing();
        assert_eq!(gate(HostBaselineSyncMode::Auto, &p, &AutoState::default(), Instant::now()), Gate::Skip("nothing_to_fix"));
    }

    #[test]
    fn failures_back_off_then_give_up_and_a_changed_plan_resets() {
        let p = plan_with_fix();
        let t0 = Instant::now();
        let mut s = AutoState::default();
        assert_eq!(record(&mut s, &p.plan_rev, false, t0), Some(Duration::from_secs(300)), "a retry is scheduled, not only waited for");
        assert_eq!(s.failures, 1);
        assert_eq!(gate(HostBaselineSyncMode::Auto, &p, &s, t0), Gate::Skip("backoff"));
        assert_eq!(gate(HostBaselineSyncMode::Auto, &p, &s, t0 + Duration::from_secs(301)), Gate::Apply);
        assert_eq!(record(&mut s, &p.plan_rev, false, t0 + Duration::from_secs(301)), Some(Duration::from_secs(1800)));
        assert_eq!(record(&mut s, &p.plan_rev, false, t0 + Duration::from_secs(2101)), None, "the third failure stops for good");
        assert_eq!(s.failures, 3);
        assert_eq!(gate(HostBaselineSyncMode::Auto, &p, &s, t0 + Duration::from_secs(99999)), Gate::Skip("gave_up"));
        let mut other = p.clone();
        other.plan_rev = "other-rev".into();
        assert_eq!(gate(HostBaselineSyncMode::Auto, &other, &s, t0 + Duration::from_secs(99999)), Gate::Apply);
    }

    #[test]
    fn partial_or_skipped_outcomes_are_not_a_success() {
        use crate::host_baseline::apply::Outcome;
        let o = |result: &'static str| Outcome { id: "x".into(), result, reason: None };
        let run = |outcomes: Vec<Outcome>, error: Option<String>| ApplyRun { outcomes, backup_written: true, error };
        assert!(all_applied(&run(vec![o("applied"), o("present")], None)));
        assert!(!all_applied(&run(vec![o("applied"), o("skipped")], None)), "a skipped key must be retried, not marked done");
        assert!(!all_applied(&run(vec![o("applied")], Some("backup_failed".into()))));
        assert!(!all_applied(&run(vec![], None)));
    }

    struct Home(std::path::PathBuf);

    impl Home {
        fn new() -> Self {
            let dir = crate::test_dirs::scratch_dir("am-auto");
            std::fs::create_dir_all(dir.join(".codex")).unwrap();
            Self(dir)
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_existing_value_is_never_overwritten_by_an_auto_apply() {
        use crate::host_baseline::apply::{new_backup_id, prepare, render_apply_sh};
        let home = Home::new();
        let cfg = home.0.join(".codex/config.toml");
        std::fs::write(&cfg, "[features]\nhooks = false\n").unwrap();
        let p = plan_with_fix();
        let prepared = prepare(&p, None).unwrap();
        let script = render_apply_sh(&new_backup_id(), &p.plan_rev, &prepared.writes).unwrap();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .current_dir(&home.0)
            .env_clear()
            .env("HOME", &home.0)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(out.status.success());
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(after.contains("hooks = false"), "{after}");
        assert!(!after.contains("hooks = true"), "{after}");
    }
}
