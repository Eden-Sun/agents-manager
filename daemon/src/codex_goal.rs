//! Spike #751: codex native thread Goals (`thread/goal/{set,get,clear}`, app-server v2) as an
//! optional execution contract under an AGM assignment. **Off by default and not wired into any
//! dispatch path**: this module only holds the pure pieces a canary needs (request shapes,
//! response/notification parsing, the "who may continue the thread" arbiter, the version gate).
//! Findings and the canary procedure: `docs/CODEX-GOALS-SPIKE.md`.
//!
//! Rules the pieces encode (the issue's "must" list):
//! * AGM's DB stays the authority; a goal is only ever set from an assignment, never from chat.
//! * Only the public app-server API is used — never codex's own SQLite.
//! * Anything we cannot prove (flag off, CLI too old, not a child, thread already carries another
//!   objective) falls back to today's resume-nudge / queued-prompt flow, with the reason.

// Spike: nothing calls this yet by design (see above).
#![allow(dead_code)]

use serde_json::{json, Value};

/// Env switch for the canary. Anything but `1` is off.
pub const FLAG_ENV: &str = "AGM_CODEX_NATIVE_GOAL";

/// First CLI where `features.goals` is stable and the goal RPCs exist (measured on 0.159.3).
pub const MIN_CLI: (u32, u32, u32) = (0, 159, 0);

pub fn flag_on() -> bool {
    std::env::var(FLAG_ENV).map(|v| v == "1").unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "usageLimited" => Self::UsageLimited,
            "budgetLimited" => Self::BudgetLimited,
            "complete" => Self::Complete,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadGoal {
    pub thread_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
}

/// A `ThreadGoal` object as it appears in `thread/goal/get|set` results and `thread/goal/updated`.
pub fn parse_goal(v: &Value) -> Option<ThreadGoal> {
    Some(ThreadGoal {
        thread_id: v.get("threadId")?.as_str()?.to_string(),
        objective: v.get("objective")?.as_str()?.to_string(),
        status: GoalStatus::parse(v.get("status")?.as_str()?)?,
        token_budget: v.get("tokenBudget").and_then(Value::as_i64),
        tokens_used: v.get("tokensUsed")?.as_i64()?,
        time_used_seconds: v.get("timeUsedSeconds")?.as_i64()?,
    })
}

/// `thread/goal/get` result: `{"goal": null}` is "no goal", not an error.
pub fn parse_get_result(result: &Value) -> Option<Option<ThreadGoal>> {
    match result.get("goal")? {
        Value::Null => Some(None),
        g => parse_goal(g).map(Some),
    }
}

pub fn set_params(thread_id: &str, objective: &str, token_budget: Option<i64>, status: Option<&str>) -> Value {
    let mut p = json!({"threadId": thread_id, "objective": objective});
    if let Some(b) = token_budget {
        p["tokenBudget"] = json!(b);
    }
    if let Some(s) = status {
        p["status"] = json!(s);
    }
    p
}

pub fn clear_params(thread_id: &str) -> Value {
    json!({"threadId": thread_id})
}

/// What a notification says about a goal, already bound to the thread it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalEvent {
    Updated { thread_id: String, turn_id: Option<String>, goal: ThreadGoal },
    Cleared { thread_id: String },
}

impl GoalEvent {
    pub fn thread_id(&self) -> &str {
        match self {
            Self::Updated { thread_id, .. } | Self::Cleared { thread_id } => thread_id,
        }
    }
}

/// `thread/goal/updated` / `thread/goal/cleared` JSON-RPC notification → event. Other methods and
/// malformed params are `None`, never a guess.
pub fn parse_notification(msg: &Value) -> Option<GoalEvent> {
    let params = msg.get("params")?;
    let thread_id = params.get("threadId")?.as_str()?.to_string();
    match msg.get("method")?.as_str()? {
        "thread/goal/updated" => {
            let goal = parse_goal(params.get("goal")?)?;
            // A notification whose goal names another thread must not be applied to this one.
            if goal.thread_id != thread_id {
                return None;
            }
            let turn_id = params.get("turnId").and_then(Value::as_str).map(String::from);
            Some(GoalEvent::Updated { thread_id, turn_id, goal })
        }
        "thread/goal/cleared" => Some(GoalEvent::Cleared { thread_id }),
        _ => None,
    }
}

/// `codex-cli 0.159.3` / `0.159.3` / `rust-v0.159.3` → `(0,159,3)`.
pub fn parse_cli_version(s: &str) -> Option<(u32, u32, u32)> {
    let tok = s.split_whitespace().last()?.trim_start_matches("rust-v").trim_start_matches('v');
    let mut it = tok.split(['.', '-', '+']);
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Set a native goal for this assignment.
    Native,
    /// Today's AGM flow, with the reason it was chosen (for the canary report / logs).
    Fallback(&'static str),
}

#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub flag_on: bool,
    pub cli_version: Option<&'a str>,
    pub managed_by_child: bool,
    /// The assignment is an explicit long-running one (not ordinary chat).
    pub long_assignment: bool,
    /// Objective the thread's goal already carries, if any.
    pub existing_goal_objective: Option<&'a str>,
    pub assignment_objective: &'a str,
}

/// Whether to use a native goal. Every "no" is an explicit fallback to the existing flow.
pub fn plan(c: &Candidate) -> Plan {
    if !c.flag_on {
        return Plan::Fallback("flag off");
    }
    let Some(v) = c.cli_version.and_then(parse_cli_version) else {
        return Plan::Fallback("cli version unknown");
    };
    if v < MIN_CLI {
        return Plan::Fallback("cli too old for goals");
    }
    if !c.managed_by_child {
        return Plan::Fallback("not a managed child");
    }
    if !c.long_assignment {
        return Plan::Fallback("not a long assignment");
    }
    match c.existing_goal_objective {
        // One AGM objective per thread: a different one already there is not ours to replace.
        Some(o) if o != c.assignment_objective => Plan::Fallback("thread already has another goal"),
        _ => Plan::Native,
    }
}

/// Who may push the next turn on a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuer {
    /// codex continues by itself; AGM must send no resume-nudge / queued continuation.
    Native,
    /// No live native goal: AGM's own flow decides.
    Agm,
    /// Nobody should push (budget / usage exhausted, or the goal says blocked).
    Hold,
}

/// The arbiter for "AGM continuation vs native goal continuation" (canary item 6).
/// `queued_user_input`: the user already has input waiting; it always goes first and AGM adds
/// nothing on top of it.
pub fn continuer(goal: Option<&ThreadGoal>, queued_user_input: bool) -> Continuer {
    let Some(g) = goal else { return Continuer::Agm };
    match g.status {
        GoalStatus::Active => Continuer::Native,
        // Queued user input resumes a paused goal's thread itself; nudging on top would double up.
        GoalStatus::Paused if queued_user_input => Continuer::Hold,
        GoalStatus::Paused | GoalStatus::Complete => Continuer::Agm,
        GoalStatus::Blocked | GoalStatus::UsageLimited | GoalStatus::BudgetLimited => Continuer::Hold,
    }
}

/// Cancelling an AGM assignment clears the native goal only when it is still *that* assignment's
/// goal (CAS on objective), so a retried / late cancel can never wipe a newer objective.
pub fn should_clear_on_cancel(goal: Option<&ThreadGoal>, assignment_objective: &str) -> bool {
    goal.is_some_and(|g| g.objective == assignment_objective)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(status: &str) -> Value {
        json!({"threadId":"t1","objective":"ship it","status":status,"tokenBudget":5000,
               "tokensUsed":12,"timeUsedSeconds":3,"createdAt":1,"updatedAt":2})
    }
    fn g(status: GoalStatus) -> ThreadGoal {
        ThreadGoal { thread_id: "t1".into(), objective: "ship it".into(), status, token_budget: Some(5000), tokens_used: 12, time_used_seconds: 3 }
    }
    fn cand<'a>() -> Candidate<'a> {
        Candidate { flag_on: true, cli_version: Some("codex-cli 0.159.3"), managed_by_child: true, long_assignment: true, existing_goal_objective: None, assignment_objective: "ship it" }
    }

    #[test]
    fn a_goal_from_codex_0_159_3_parses_with_its_budget_and_usage() {
        // Shape captured from a real `thread/goal/set` on codex-cli 0.159.3.
        let got = parse_goal(&goal("paused")).unwrap();
        assert_eq!(got.status, GoalStatus::Paused);
        assert_eq!((got.token_budget, got.tokens_used, got.time_used_seconds), (Some(5000), 12, 3));
    }

    #[test]
    fn no_goal_is_an_empty_answer_not_an_error() {
        assert_eq!(parse_get_result(&json!({"goal": null})), Some(None));
        assert_eq!(parse_get_result(&json!({"goal": goal("active")})).unwrap().unwrap().status, GoalStatus::Active);
        assert_eq!(parse_get_result(&json!({})), None);
    }

    #[test]
    fn an_unknown_status_is_not_guessed() {
        assert_eq!(parse_goal(&goal("sideways")), None);
    }

    #[test]
    fn set_params_only_carry_what_was_given() {
        assert_eq!(set_params("t1", "x", None, None), json!({"threadId":"t1","objective":"x"}));
        assert_eq!(
            set_params("t1", "x", Some(9), Some("paused")),
            json!({"threadId":"t1","objective":"x","tokenBudget":9,"status":"paused"})
        );
    }

    #[test]
    fn notifications_bind_to_the_thread_they_name() {
        let up = json!({"method":"thread/goal/updated","params":{"threadId":"t1","turnId":"u9","goal":goal("complete")}});
        let ev = parse_notification(&up).unwrap();
        assert_eq!(ev.thread_id(), "t1");
        assert!(matches!(ev, GoalEvent::Updated { turn_id: Some(ref u), .. } if u == "u9"));
        let cl = json!({"method":"thread/goal/cleared","params":{"threadId":"t2"}});
        assert_eq!(parse_notification(&cl), Some(GoalEvent::Cleared { thread_id: "t2".into() }));
    }

    #[test]
    fn a_notification_whose_goal_names_another_thread_is_dropped() {
        let bad = json!({"method":"thread/goal/updated","params":{"threadId":"t2","goal":goal("active")}});
        assert_eq!(parse_notification(&bad), None);
        assert_eq!(parse_notification(&json!({"method":"turn/started","params":{"threadId":"t1"}})), None);
    }

    #[test]
    fn cli_versions_parse_from_banner_and_tag_forms() {
        assert_eq!(parse_cli_version("codex-cli 0.159.3"), Some((0, 159, 3)));
        assert_eq!(parse_cli_version("rust-v0.160.0"), Some((0, 160, 0)));
        assert_eq!(parse_cli_version("0.159.0-alpha.2"), Some((0, 159, 0)));
        assert_eq!(parse_cli_version("garbage"), None);
    }

    #[test]
    fn everything_unproven_falls_back_to_the_existing_flow() {
        assert_eq!(plan(&cand()), Plan::Native);
        assert_eq!(plan(&Candidate { flag_on: false, ..cand() }), Plan::Fallback("flag off"));
        assert_eq!(plan(&Candidate { cli_version: None, ..cand() }), Plan::Fallback("cli version unknown"));
        assert_eq!(plan(&Candidate { cli_version: Some("codex-cli 0.158.9"), ..cand() }), Plan::Fallback("cli too old for goals"));
        assert_eq!(plan(&Candidate { managed_by_child: false, ..cand() }), Plan::Fallback("not a managed child"));
        assert_eq!(plan(&Candidate { long_assignment: false, ..cand() }), Plan::Fallback("not a long assignment"));
    }

    #[test]
    fn one_thread_carries_one_agm_objective() {
        assert_eq!(plan(&Candidate { existing_goal_objective: Some("other"), ..cand() }), Plan::Fallback("thread already has another goal"));
        // Re-binding the same objective (after an AGM restart) is fine and idempotent.
        assert_eq!(plan(&Candidate { existing_goal_objective: Some("ship it"), ..cand() }), Plan::Native);
    }

    #[test]
    fn agm_never_nudges_a_thread_whose_native_goal_is_driving_it() {
        assert_eq!(continuer(Some(&g(GoalStatus::Active)), false), Continuer::Native);
        assert_eq!(continuer(Some(&g(GoalStatus::Active)), true), Continuer::Native);
        assert_eq!(continuer(None, false), Continuer::Agm);
        assert_eq!(continuer(Some(&g(GoalStatus::Complete)), false), Continuer::Agm);
    }

    #[test]
    fn exhausted_or_blocked_goals_and_queued_input_hold_the_nudge() {
        for s in [GoalStatus::Blocked, GoalStatus::UsageLimited, GoalStatus::BudgetLimited] {
            assert_eq!(continuer(Some(&g(s)), false), Continuer::Hold);
        }
        assert_eq!(continuer(Some(&g(GoalStatus::Paused)), true), Continuer::Hold);
        assert_eq!(continuer(Some(&g(GoalStatus::Paused)), false), Continuer::Agm);
    }

    #[test]
    fn cancel_clears_only_the_goal_that_is_still_this_assignments() {
        assert!(should_clear_on_cancel(Some(&g(GoalStatus::Active)), "ship it"));
        assert!(!should_clear_on_cancel(Some(&g(GoalStatus::Active)), "older objective"));
        assert!(!should_clear_on_cancel(None, "ship it"));
    }
}
