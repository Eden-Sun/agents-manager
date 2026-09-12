//! The team scheduler (SPEC-team §3, §4, §8, §9) — the only thing that ever speaks *for* a
//! bot, and the only thing that runs git on a team's behalf.
//!
//! # Shape
//!
//! Everything a team is lives in SQLite; memory holds nothing but a `tokio` task per live
//! team and the set of team ids that already have one. That is what makes §7.5 (daemon
//! restart) a non-event: `respawn_schedulers` re-enters the same three functions.
//!
//! * [`step`] — one idempotent pass over one team: budget / quota gates, then the task
//!   engine, then the outbox. Safe to call at any time, from any number of places.
//! * [`apply_reply`] — a member finished a turn: parse the trailing ` ```am-team ` block and
//!   move the state machine. Writes rows; never sends. The sending is `step`'s job, so a
//!   reply that arrives while the team is paused is still recorded.
//! * [`startup`] — phase `starting`: start the members, then hand the issue to the PM.
//!
//! # The loop guards of §4.5, and where each one lives
//!
//! | § | guard | here |
//! |---|---|---|
//! | topology  | star: pm↔worker, pm↔reviewer, reviewer→worker | [`Action::parse`] refuses an action the role may not use, and [`enqueue`] is only ever called from the transitions below — there is no worker→worker edge to take |
//! | state-driven | a relay follows a state change, not a sentence | [`advance_tasks`] sends per task state, and each send is paired with the state write, so one state cannot emit the same relay twice |
//! | relay budget | `max_relays`, repair prompts included | [`flush`] refuses to deliver past the cap → `paused(budget_relays)` |
//! | review rounds | `max_review_rounds` | [`Verdict::request_changes`] handling → `exhausted` + `paused(review_exhausted)` |
//! | dispatch cap | one open task per worker; no identical repeat | [`dispatch`] (plus the DB's partial unique index) |
//! | PM stall | no task left to wait for, then repeated `wait` | [`wait`] nudges once, then `paused(pm_stalled)` |
//! | wall clock | `max_wall_clock_min` | [`gates`] |
//! | quota | `quota_stop_pct` per relay | [`gates`] and again inside [`flush`] |
//!
//! And the final rule that binds them: **every one of those pauses, never aborts** (§4.5
//! last row). `aborting` is reachable from `POST /teams/:id/abort` alone.

use crate::db;
use crate::lifecycle::{self, LcError, LcResult};
use crate::state::App;
use crate::team::{self, Budget};
use crate::team_git as tg;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

/// How often a scheduler wakes up on its own (wall-clock budget, stragglers, a member that
/// left `blocked`). Turn completions do not wait for this — they arrive on the turn bus.
const TICK: std::time::Duration = std::time::Duration::from_secs(20);

/// SPEC-team appendix A: relay bodies are capped; the full text is always in the timeline.
const RELAY_MAX: usize = 8 * 1024;

/// §4.4: two repair prompts for the same reply, then a human looks at it.
const MAX_REPAIRS: i64 = 2;

/// §8.2 / §6.3: a conflicting task goes back to its author at most twice.
const MAX_REBASES: i64 = 2;

// ---------------------------------------------------------------- the registry

/// The live schedulers, `team_id -> (generation, task)`. The generation is what lets a
/// scheduler withdraw *its own* registration and never a successor's.
fn running() -> &'static StdMutex<BTreeMap<String, Live>> {
    static R: OnceLock<StdMutex<BTreeMap<String, Live>>> = OnceLock::new();
    R.get_or_init(|| StdMutex::new(BTreeMap::new()))
}

struct Live {
    generation: u64,
    /// `None` only for the instant between reserving the id and the task existing.
    task: Option<tokio::task::JoinHandle<()>>,
}

fn next_gen() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Deregistration on *every* way out of the loop, panic and `abort` included. A leftover id
/// used to make the team permanently unschedulable — `spawn` reads a present id as "already
/// running" and returns, so `resume` / `decide` / `respawn_schedulers` all answered 200 OK
/// while nothing was left to act on them.
struct Registration {
    team_id: String,
    generation: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // Never `unwrap` a lock in a `Drop` that may itself be running on a panic unwind:
        // a poisoned mutex there would abort the process.
        let mut g = running().lock().unwrap_or_else(|e| e.into_inner());
        if g.get(&self.team_id).map(|l| l.generation) == Some(self.generation) {
            g.remove(&self.team_id);
        }
    }
}

/// Stop a team's scheduler and wait for the task to be gone (SPEC-team §6.5a). `delete`
/// needs exactly that ordering: the loop only ends by itself when `tick` reads a terminal
/// phase, so a row deleted underneath a live loop leaves it warning every `TICK` for ever.
pub async fn stop(team_id: &str) {
    let live = running().lock().unwrap().remove(team_id);
    if let Some(Live { task: Some(h), .. }) = live {
        h.abort();
        let _ = h.await;
    }
}

/// Background schedulers are off inside `cargo test`: the tests drive [`step`],
/// [`advance_tasks`] and [`apply_reply`] directly so a scenario is deterministic, and a task
/// racing them would write events the assertions cannot predict.
fn enabled() -> bool {
    !cfg!(test)
}

/// SPEC-team §3: one single-threaded event loop per live team. Calling this twice for the
/// same team is a no-op — `create`, `resume`, `decide` and `respawn_schedulers` all call it
/// without coordinating.
pub fn spawn(app: &Arc<App>, team_id: &str) {
    if !enabled() {
        return;
    }
    let generation = {
        let mut g = running().lock().unwrap();
        if g.contains_key(team_id) {
            return;
        }
        let generation = next_gen();
        g.insert(team_id.to_string(), Live { generation, task: None });
        generation
    };
    let app = app.clone();
    let team_id = team_id.to_string();
    // Moved into the task, so the id is withdrawn on every exit path — panic included.
    let reg = Registration { team_id: team_id.clone(), generation };
    let key = team_id.clone();
    let handle = tokio::spawn(async move {
        let _reg = reg;
        let mut turns = app.subscribe_turns();
        loop {
            let done = match tick(&app, &team_id).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(team = %team_id, error = ?e, "team scheduler step failed");
                    false
                }
            };
            if done {
                break;
            }
            tokio::select! {
                ev = turns.recv() => match ev {
                    // A relay turn carries `team_id`; a user's group-chat turn to a member does
                    // not (`turns.team_id` is NULL), so it is matched through the bot instead —
                    // its reply may still carry a block (`on_turn_done`).
                    Ok(t) if t.is_done() && (t.team_id.as_deref() == Some(team_id.as_str())
                        || (t.team_id.is_none() && member_of(&app, &team_id, &t.bot_id).await)) => {
                        if let Err(e) = on_turn_done(&app, &team_id, &t.bot_id, &t.turn_id, &t.status).await {
                            tracing::warn!(team = %team_id, error = ?e, "team scheduler: turn handling failed");
                        }
                    }
                    Ok(_) => {}
                    // No ring buffer on the turn bus: a lagging subscriber re-reads the DB,
                    // which is exactly what the next `tick` does anyway.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(team = %team_id, skipped = n, "team scheduler lagged the turn bus");
                    }
                    Err(_) => break,
                },
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    });
    let mut g = running().lock().unwrap();
    match g.get_mut(&key) {
        Some(l) if l.generation == generation => l.task = Some(handle),
        // `stop` ran while we were spawning: the team is on its way out, so this loop must
        // not outlive it.
        _ => handle.abort(),
    }
}

/// One loop iteration; `true` means the team reached a terminal phase and the task may stop.
async fn tick(app: &Arc<App>, team_id: &str) -> LcResult<bool> {
    let t = team::load(app, team_id).await?;
    if team::is_terminal(&t.phase) {
        return Ok(true);
    }
    if t.phase == "starting" {
        startup(app, team_id).await?;
    }
    step(app, team_id).await?;
    Ok(team::is_terminal(&team::load(app, team_id).await?.phase))
}

// ---------------------------------------------------------------- context

struct Ctx {
    /// The queue entries being worked right now (§2.3), in queue order.
    ///
    /// With a finite parallelism (1–4) this is at most one row and every existing decision
    /// reads it through [`Ctx::issue`]. Unlimited mode (§4.5) has up to
    /// `MAX_CONCURRENT_ISSUES` of them at once, and then a task's `issue_id` — never the
    /// `teams` mirror — is what says which integration branch it belongs to.
    issues: Vec<db::TeamIssue>,
    team: db::Team,
    project: db::Project,
    members: Vec<db::Bot>,
    budget: Budget,
}

impl Ctx {
    async fn load(app: &Arc<App>, team_id: &str) -> LcResult<Ctx> {
        let team = team::load(app, team_id).await?;
        let project = db::project(&app.db, &team.project_id)
            .await
            .map_err(up)?
            .ok_or_else(|| LcError::NotFound("project".into()))?;
        let members: Vec<db::Bot> = db::team_members(&app.db, team_id)
            .await
            .map_err(up)?
            .into_iter()
            .filter(|b| b.deleted_at.is_none())
            .collect();
        let budget = Budget::from_json(&team.budget_json);
        // §2.3: which entries of the issue queue the team is on. Empty only between issues.
        let issues = db::working_team_issues(&app.db, team_id).await.map_err(up)?;
        Ok(Ctx { team, project, members, budget, issues })
    }

    /// The first working issue — the `teams` mirror, and with a finite parallelism simply
    /// *the* issue the team is on.
    fn issue(&self) -> Option<&db::TeamIssue> {
        self.issues.first()
    }
    /// The current issue's id, for scoping tasks / relays / the budget to it.
    fn issue_id(&self) -> Option<&str> {
        self.issues.first().map(|i| i.id.as_str())
    }
    /// §4.5: `workers.count = 0`. Every new code path below branches on this and nothing
    /// else, so a team with a parallelism of 1–4 behaves exactly as it always did.
    fn unlimited(&self) -> bool {
        team::is_unlimited(&self.team)
    }
    fn issue_of(&self, issue_id: &str) -> Option<&db::TeamIssue> {
        self.issues.iter().find(|i| i.id == issue_id)
    }
    /// The integration branch a task belongs to: its own issue's, falling back to the `teams`
    /// mirror for a task written before the queue existed.
    fn integration_of(&self, t: &db::TeamTask) -> String {
        t.issue_id
            .as_deref()
            .and_then(|i| self.issue_of(i))
            .and_then(|i| i.branch.clone())
            .unwrap_or_else(|| self.team.branch.clone())
    }
    /// The executors that may take work on `issue`. Unlimited mode gives every issue its own
    /// batch (§6.2's per-issue directories); a finite parallelism has one pool for the team,
    /// which a `done.workers = "keep"` may carry across issues under its old name.
    fn workers_for(&self, issue: &db::TeamIssue) -> Vec<&db::Bot> {
        // §2.6: a rescue is one member finishing what the team could not. While it runs that
        // member is the issue's only executor — the retired dev workers are gone, and the
        // rescuer (usually the reviewer) is the one holding the task.
        if let Some(id) = self.team.rescue_bot_id.as_deref() {
            return self.members.iter().filter(|b| b.id == id).collect();
        }
        if self.unlimited() {
            team::workers_of_issue(&self.members, &self.team.id, issue.seq)
        } else {
            self.workers()
        }
    }
    fn host(&self) -> &str {
        &self.project.host
    }
    fn role(&self, role: &str) -> Option<&db::Bot> {
        self.members.iter().find(|b| b.team_role.as_deref() == Some(role))
    }
    fn pm(&self) -> Option<&db::Bot> {
        self.role("pm")
    }
    fn reviewer(&self) -> Option<&db::Bot> {
        self.role("reviewer")
    }
    fn workers(&self) -> Vec<&db::Bot> {
        self.members.iter().filter(|b| b.team_role.as_deref() == Some("worker")).collect()
    }
    fn by_id(&self, id: &str) -> Option<&db::Bot> {
        self.members.iter().find(|b| b.id == id)
    }
    fn short(&self, b: &db::Bot) -> String {
        team::short_name(&b.name, &self.team.id)
    }
    /// The member's own worktree — and the `-C` target of every worktree-level git command
    /// this module runs.
    ///
    /// Fallible on purpose: `team::checked_member_cwd` is the §6.1 invariant, and making it
    /// a `Result` is how the compiler guarantees no call site can skip it. `bot_cwd`'s
    /// "empty means the project path" fallback is exactly the wrong answer here — for a team
    /// member the project path is the one directory that must never be written to.
    fn wt(&self, b: &db::Bot) -> LcResult<String> {
        team::checked_member_cwd(&self.team, &self.project, b).map_err(unsafe_layout)
    }
    /// The integration worktree (`<root>/main`), where and only where the daemon merges.
    fn main_wt(&self) -> LcResult<String> {
        team::checked_main_wt(&self.team, &self.project).map_err(unsafe_layout)
    }
    /// Whole-team check, run before anything git happens.
    fn check_layout(&self) -> LcResult<()> {
        team::check_layout(&self.team, &self.project, &self.members).map_err(unsafe_layout)
    }
}

fn unsafe_layout(e: String) -> LcError {
    LcError::Upstream(format!("unsafe team layout: {e}"))
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

// ---------------------------------------------------------------- the `am-team` protocol (§4.4)

/// Pull the **last** ` ```am-team ` fenced block out of a reply. Only the last one counts, so
/// an agent quoting the protocol earlier in its answer cannot hijack the parse.
pub fn extract_block(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut best: Option<String> = None;
    let mut i = 0usize;
    while i < lines.len() {
        let l = lines[i].trim_start();
        let fence: Option<usize> = l.strip_prefix("```").map(|_| 3).or_else(|| l.strip_prefix("~~~").map(|_| 3));
        if fence.is_some() {
            let marker = &l[..3];
            let info = l[3..].trim().trim_start_matches('`').trim();
            if info.eq_ignore_ascii_case("am-team") {
                let mut body = String::new();
                let mut j = i + 1;
                let mut closed = false;
                while j < lines.len() {
                    if lines[j].trim_start().starts_with(marker) && lines[j].trim().trim_end_matches(marker).is_empty()
                    {
                        closed = true;
                        break;
                    }
                    body.push_str(lines[j]);
                    body.push('\n');
                    j += 1;
                }
                // An unterminated fence is still taken: a `completed_fallback` capture is
                // routinely truncated, and the JSON either parses or it does not.
                let _ = closed;
                best = Some(body);
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    best
}

/// `extract_block` + JSON. The error string is what goes back to the agent verbatim in the
/// repair prompt, so it says what is wrong and nothing else.
pub fn parse_block(text: &str) -> Result<Value, String> {
    let Some(body) = extract_block(text) else {
        return Err("找不到 am-team 區塊".into());
    };
    let v: Value = serde_json::from_str(body.trim()).map_err(|e| format!("am-team 區塊不是合法 JSON：{e}"))?;
    if !v.is_object() {
        return Err("am-team 區塊必須是一個 JSON 物件".into());
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq)]
pub struct DispatchItem {
    /// Whom the PM named, if it named anyone. `None` — the normal case since §4.5 became a
    /// worker pool — means "whichever executor is free next".
    pub to: Option<String>,
    pub title: String,
    pub brief: String,
    pub files: Vec<String>,
    /// §4.4 (2026-09-09): which issue this task belongs to. Optional while only one issue is
    /// working; **required** in unlimited mode, where the daemon cannot guess.
    pub issue: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Dispatch(Vec<DispatchItem>),
    Wait,
    /// `keep_workers`: the PM's call on whether the next issue reuses this batch of
    /// executors (their context is still useful) or gets a fresh one.
    Done { summary: String, keep_workers: bool, issue: Option<i64> },
    AskUser { question: String },
    Abort { reason: String },
    Report { blocked: bool, summary: String, notes: String },
    Verdict { approve: bool, summary: String, must_fix: Vec<String> },
}

/// `"issue": 48` or `"issue": "#48"` — the PM writes both. Absent or unreadable is `None`,
/// which the caller turns into "the only working issue" or a refusal (§4.5).
fn issue_field(v: &Value) -> Option<i64> {
    let f = v.get("issue").or_else(|| v.get("issue_number"))?;
    if let Some(n) = f.as_i64() {
        return Some(n);
    }
    let t = f.as_str()?.trim().trim_start_matches('#');
    t.parse::<i64>().ok()
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn list(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

impl Action {
    /// §4.4's table, enforced. This is loop guard #1: a worker cannot emit `dispatch`, a PM
    /// cannot emit `verdict`, so the star topology is not merely a convention.
    pub fn parse(role: &str, v: &Value) -> Result<Action, String> {
        let action = s(v, "action");
        if action.is_empty() {
            return Err("am-team 區塊缺少 action".into());
        }
        match (role, action.as_str()) {
            ("pm", "dispatch") => {
                let raw = v.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
                if raw.is_empty() {
                    return Err("dispatch 需要至少一筆 tasks".into());
                }
                let mut out = Vec::new();
                for t in &raw {
                    let to = s(t, "to");
                    let title = s(t, "title");
                    let brief = s(t, "brief");
                    // §4.5: `to` is optional — the daemon assigns from the pool. Naming an
                    // executor still works, and still means that one and no other.
                    if brief.is_empty() {
                        return Err("每筆 task 都需要 brief".into());
                    }
                    out.push(DispatchItem {
                        to: Some(to).filter(|t| !t.is_empty()),
                        title: if title.is_empty() { "task".into() } else { title },
                        brief,
                        files: list(t, "files"),
                        issue: issue_field(t),
                    });
                }
                Ok(Action::Dispatch(out))
            }
            ("pm", "wait") => Ok(Action::Wait),
            ("pm", "done") => {
                // `"workers": "keep" | "replace"` (or `keep_workers: bool`); unspecified = replace,
                // the behaviour before the PM had a say.
                let keep = match v.get("workers").and_then(Value::as_str).map(|w| w.trim().to_ascii_lowercase()) {
                    Some(w) if w == "keep" => true,
                    Some(w) if w == "replace" => false,
                    Some(w) => return Err(format!("done.workers 必須是 keep 或 replace，不是 `{w}`")),
                    None => v.get("keep_workers").and_then(Value::as_bool).unwrap_or(false),
                };
                Ok(Action::Done { summary: s(v, "summary"), keep_workers: keep, issue: issue_field(v) })
            }
            ("pm", "ask_user") => {
                let q = s(v, "question");
                if q.is_empty() {
                    return Err("ask_user 需要 question".into());
                }
                Ok(Action::AskUser { question: q })
            }
            ("pm", "abort") => Ok(Action::Abort { reason: s(v, "reason") }),
            ("worker", "report") => {
                // Executors (codex especially) like to nest the payload under `report` and to
                // pick their own vocabulary for `status`. Read through the nesting and map the
                // obvious synonyms: a repair round costs a whole turn to say "done" again.
                let body = v.get("report").filter(|r| r.is_object()).unwrap_or(v);
                let st = s(body, "status").to_ascii_lowercase();
                let blocked = match st.as_str() {
                    "done" | "completed" | "complete" | "finished" | "success" | "ok" => false,
                    "blocked" | "stuck" | "failed" | "need_help" | "needs_help" | "help" => true,
                    _ => return Err("report.status 必須是 done 或 blocked".into()),
                };
                let mut summary = s(body, "summary");
                if summary.is_empty() {
                    // No `summary`: the rest of the object is the summary — the PM needs
                    // *something* to reason about (commits, what was verified, what is left).
                    let mut rest = body.clone();
                    if let Some(o) = rest.as_object_mut() {
                        o.remove("action");
                        o.remove("status");
                        o.remove("notes");
                    }
                    summary = if rest.as_object().map_or(true, |o| o.is_empty()) {
                        if blocked { "（執行者沒有說明）".into() } else { "（執行者沒有摘要）".into() }
                    } else {
                        serde_json::to_string(&rest).unwrap_or_default()
                    };
                }
                Ok(Action::Report { blocked, summary, notes: s(body, "notes") })
            }
            ("reviewer", "verdict") => {
                let r = s(v, "result");
                if !["approve", "request_changes"].contains(&r.as_str()) {
                    return Err("verdict.result 必須是 approve 或 request_changes".into());
                }
                Ok(Action::Verdict { approve: r == "approve", summary: s(v, "summary"), must_fix: list(v, "must_fix") })
            }
            (role, a) => Err(format!("`{a}` 不是 {role} 允許的 action（允許：{}）", allowed(role))),
        }
    }
}

fn allowed(role: &str) -> &'static str {
    match role {
        "pm" => "dispatch, wait, done, ask_user, abort",
        "reviewer" => "verdict",
        _ => "report",
    }
}

// ---------------------------------------------------------------- relay text (appendix A)

fn clip(s: &str) -> String {
    if s.chars().count() <= RELAY_MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(RELAY_MAX).collect();
    format!("{head}\n\n（已截斷，完整內容見時間軸）")
}

pub(crate) fn footer(role: &str) -> String {
    format!(
        "\n\n---\n回覆結尾必須包含一個 ```am-team fenced 區塊（JSON），允許的 action：{}。\
         區塊之外的文字給人看，區塊給系統看。",
        allowed(role)
    )
}

// ---------------------------------------------------------------- events & relays

/// Append a `relay` to the outbox. Nothing is delivered here: `flush` owns the send, so a
/// relay produced while the team is paused simply waits (§9.1 — queue, never silently skip).
#[allow(clippy::too_many_arguments)]
async fn enqueue(
    app: &Arc<App>,
    team_id: &str,
    from: Option<&str>,
    to: &str,
    task_id: Option<&str>,
    action: &str,
    text: String,
) -> LcResult<db::TeamEvent> {
    team::record_event(
        app,
        team_id,
        "relay",
        from,
        Some(to),
        task_id,
        Some("pending"),
        json!({"action": action, "text": clip(&text)}),
    )
    .await
}

async fn note(app: &Arc<App>, team_id: &str, payload: Value) -> LcResult<()> {
    team::record_event(app, team_id, "note", None, None, None, None, payload).await.map(|_| ())
}

/// Every guard in §4.5 ends here, and nowhere else does the scheduler change the phase to
/// something a human did not ask for. Pausing an already-paused team keeps the first reason.
/// §8.1 pause — with one exception introduced by the issue queue (§2.3).
///
/// The reasons in `DECISION_PAUSES` (`merge_conflict`, `review_exhausted`, `pm_abort`) are
/// about **this issue**, not about the team. When there is another issue waiting, the user's
/// ruling is to mark this one failed — keeping its branch — and carry on, rather than parking
/// the whole team on a question nobody may be around to answer. With nothing left in the queue
/// the old behaviour stands, so a single-issue team still stops and waits for `decide`.
///
/// Every other reason (`budget_time`, `quota_low`, `member_lost`, `worktree_missing`,
/// `protocol_error`…) is a team-level problem that the next issue would just hit again, so
/// those always pause.
async fn pause(app: &Arc<App>, team: &db::Team, reason: &str) -> LcResult<()> {
    pause_with_detail(app, team, reason, None).await
}

/// [`pause`] carrying §4.5's structured detail (which member, which quota window, how much is
/// left). Everything else about a pause is unchanged — the detail is what the banner reads.
async fn pause_with_detail(app: &Arc<App>, team: &db::Team, reason: &str, detail: Option<Value>) -> LcResult<()> {
    // `team` is the caller's snapshot; the user may have paused or aborted since (#31).
    let team = &team::load(app, &team.id).await?;
    if team.phase == "paused" || team::is_terminal(&team.phase) || team.phase == "aborting" {
        return Ok(());
    }
    if team::DECISION_PAUSES.contains(&reason) {
        let has_next = db::next_queued_issue(&app.db, &team.id).await.map_err(up)?.is_some();
        if has_next {
            // Boxed: the advance can itself end up back in `pause` (a failed hand-over), and
            // an `async fn` that calls itself needs an indirection to have a finite size.
            return Box::pin(close_issue_and_advance(app, &team.id, "failed", Some(reason), None)).await;
        }
    }
    team::sched_pause(app, &team.id, reason, detail).await?;
    Ok(())
}

async fn pending_for(app: &Arc<App>, team_id: &str, bot_id: &str) -> LcResult<Vec<db::TeamEvent>> {
    sqlx::query_as::<_, db::TeamEvent>(
        "SELECT * FROM team_events WHERE team_id = ? AND to_bot_id = ? AND kind = 'relay' AND status = 'pending'
         ORDER BY seq",
    )
    .bind(team_id)
    .bind(bot_id)
    .fetch_all(&app.db)
    .await
    .map_err(up)
}

/// §9.2's relay budget, counted **per issue** (§2.3). A team that works a queue would blow a
/// team-lifetime cap on its second issue for no reason the user would recognise; scoped this
/// way the same defaults mean the same thing they always did for a single-issue team.
async fn relay_count(app: &Arc<App>, team_id: &str, issue_id: Option<&str>, status: &str) -> i64 {
    match issue_id {
        Some(i) => sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM team_events WHERE team_id=? AND issue_id=? AND kind='relay' AND status=?",
        )
        .bind(team_id)
        .bind(i)
        .bind(status),
        None => sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM team_events WHERE team_id=? AND kind='relay' AND status=?",
        )
        .bind(team_id)
        .bind(status),
    }
    .fetch_one(&app.db)
    .await
    .unwrap_or(0)
}

/// §9.2's `usage_json`, recomputed from the log so it survives a restart.
async fn refresh_usage(app: &Arc<App>, ctx: &Ctx) -> LcResult<()> {
    let relays = relay_count(app, &ctx.team.id, ctx.issue_id(), "delivered").await;
    let tasks = sched_tasks(app, ctx).await?;
    let rounds: i64 = tasks.iter().map(|t| t.round).sum();
    let elapsed = elapsed_min_of(app, &ctx.team, ctx.issue()).await;
    let mut per_bot = serde_json::Map::new();
    for b in &ctx.members {
        let n = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id = t.conversation_id
             WHERE c.bot_id = ? AND t.team_id = ?",
        )
        .bind(&b.id)
        .bind(&ctx.team.id)
        .fetch_one(&app.db)
        .await
        .unwrap_or(0);
        per_bot.insert(b.name.clone(), json!({"turns": n}));
    }
    let usage = json!({
        "relays": relays, "review_rounds_total": rounds,
        "elapsed_min": elapsed, "per_bot": Value::Object(per_bot),
    });
    sqlx::query("UPDATE teams SET usage_json = ? WHERE id = ?")
        .bind(serde_json::to_string(&usage).unwrap_or_else(|_| "{}".into()))
        .bind(&ctx.team.id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    Ok(())
}

/// Minutes on the **current issue** (§2.3): the wall-clock budget is what stops one issue
/// running away, not what caps the length of a queue.
///
/// **Paused time does not count** (2026-09-09): a team that sat in `paused(quota_low)`
/// overnight came back to `budget_time` on the very next pass and had to be raised 240 →
/// 480 → 960 before it would move. The clock is for a *running* issue. The paused stretches
/// are read off the `phase` events, so nothing new is stored.
async fn elapsed_min_of(app: &Arc<App>, t: &db::Team, issue: Option<&db::TeamIssue>) -> i64 {
    let start = issue
        .and_then(|i| i.started_at.as_deref())
        .or(t.started_at.as_deref())
        .unwrap_or(t.created_at.as_str());
    let Ok(s) = chrono::DateTime::parse_from_rfc3339(start) else { return 0 };
    let s = s.with_timezone(&chrono::Utc);
    let now = chrono::Utc::now();
    let paused = paused_secs_since(app, &t.id, start, t.phase == "paused").await;
    ((now - s).num_seconds() - paused).max(0) / 60
}

/// Seconds spent in `paused` since `since` (RFC 3339), summed from the phase log. An open
/// pause (the team is paused right now) runs to the present.
async fn paused_secs_since(app: &Arc<App>, team_id: &str, since: &str, paused_now: bool) -> i64 {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT payload_json, created_at FROM team_events WHERE team_id = ? AND kind = 'phase' AND created_at >= ? ORDER BY seq",
    )
    .bind(team_id)
    .bind(since)
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();
    let ts = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&chrono::Utc));
    let mut total = 0i64;
    let mut open: Option<chrono::DateTime<chrono::Utc>> = None;
    for (payload, at) in rows {
        let v: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let to = v.get("to").and_then(Value::as_str).unwrap_or("");
        let from = v.get("from").and_then(Value::as_str).unwrap_or("");
        let Some(at) = ts(&at) else { continue };
        if to == "paused" && open.is_none() {
            open = Some(at);
        } else if from == "paused" && to != "paused" {
            if let Some(o) = open.take() {
                total += (at - o).num_seconds().max(0);
            }
        }
    }
    if let Some(o) = open {
        if paused_now {
            total += (chrono::Utc::now() - o).num_seconds().max(0);
        }
    }
    total
}

// ---------------------------------------------------------------- gates (§4.5, §9.2)

/// One member's worst quota window: which member, which identity, which window, how much is
/// left and when it comes back. `quota_low` used to be a bare code, which told the user a
/// team had stopped but not *whose* account to go and look at (§4.5).
struct QuotaHit {
    bot_id: String,
    name: String,
    role: Option<String>,
    kind: String,
    identity: Option<String>,
    host: String,
    /// `five_hour` / `seven_day` — the window key, exactly as `GET /api/quota` names it.
    window: &'static str,
    used_pct: f64,
    resets_at: Option<String>,
}

impl QuotaHit {
    fn to_json(&self, team_id: &str) -> Value {
        json!({
            "bot_id": self.bot_id,
            "name": self.name,
            "short": team::short_name(&self.name, team_id),
            "role": self.role,
            "kind": self.kind,
            "identity": self.identity,
            "host": self.host,
            "window": self.window,
            "used_pct": self.used_pct,
            "remaining_pct": (100.0 - self.used_pct).max(0.0),
            "resets_at": self.resets_at,
        })
    }
}

/// The quota reading for one member's kind (`kind` or `kind:<identity>`) **on the bot's
/// host** (SPEC §14), if there is one. The window returned is the one closest to its cap —
/// that is the one that stops the team, so that is the one the banner has to name.
async fn quota_hit(app: &Arc<App>, bot: &db::Bot) -> Option<QuotaHit> {
    let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
    let q = app.quotas.lock().await;
    let identity = bot.identity.clone().filter(|s| !s.is_empty());
    let keys: Vec<String> = match identity.as_deref() {
        Some(i) => vec![format!("{}:{i}", bot.kind), bot.kind.clone()],
        None => vec![bot.kind.clone()],
    }
    .iter()
    .map(|base| crate::quota::quota_key(&host, base))
    .collect();
    for k in keys {
        let Some(quota) = q.get(&k) else { continue };
        let worst = [("five_hour", &quota.five_hour), ("seven_day", &quota.seven_day)]
            .into_iter()
            .filter_map(|(name, w)| w.as_ref().map(|w| (name, w)))
            .fold(None, |acc: Option<(&'static str, &crate::quota::Window)>, (name, w)| {
                Some(match acc {
                    Some(prev) if prev.1.used_pct >= w.used_pct => prev,
                    _ => (name, w),
                })
            });
        let (window, w) = worst?;
        return Some(QuotaHit {
            bot_id: bot.id.clone(),
            name: bot.name.clone(),
            role: bot.team_role.clone(),
            kind: bot.kind.clone(),
            identity,
            host,
            window,
            used_pct: w.used_pct,
            resets_at: w.resets_at.clone(),
        });
    }
    None
}

/// Every member at or past `stop_pct`, worst first — the whole list, because two roles on the
/// same account run out together and naming only one sends the user to fix half the problem.
async fn quota_low_members(app: &Arc<App>, ctx: &Ctx) -> Vec<QuotaHit> {
    let mut hits = Vec::new();
    if team::quota_check_disabled(ctx.budget.quota_stop_pct) {
        return hits;
    }
    for b in &ctx.members {
        if let Some(hit) = quota_hit(app, b).await {
            if hit.used_pct >= ctx.budget.quota_stop_pct {
                hits.push(hit);
            }
        }
    }
    hits.sort_by(|a, b| b.used_pct.total_cmp(&a.used_pct));
    hits
}

/// §4.5's `quota_low` pause, with the roster of who ran out attached (§10.6 `pause_detail`).
async fn pause_quota_low(app: &Arc<App>, ctx: &Ctx, hits: &[QuotaHit]) -> LcResult<()> {
    let detail = json!({
        "stop_pct": ctx.budget.quota_stop_pct,
        "members": hits.iter().map(|h| h.to_json(&ctx.team.id)).collect::<Vec<_>>(),
    });
    pause_with_detail(app, &ctx.team, "quota_low", Some(detail)).await
}

/// Wall clock and quota, checked on every pass. Returns `true` when the team may proceed.
async fn gates(app: &Arc<App>, ctx: &Ctx) -> LcResult<bool> {
    if ctx.unlimited() {
        // §4.5: the wall clock is per issue, so one runaway issue is closed out and the rest
        // keep going — the same ruling `DECISION_PAUSES` already makes for the queue.
        for q in ctx.issues.clone() {
            if elapsed_min_of(app, &ctx.team, Some(&q)).await >= ctx.budget.max_wall_clock_min {
                close_one_issue(app, &ctx.team.id, &q, "failed", Some("budget_time"), None).await?;
                return Ok(false);
            }
        }
    } else if elapsed_min_of(app, &ctx.team, ctx.issue()).await >= ctx.budget.max_wall_clock_min {
        pause(app, &ctx.team, "budget_time").await?;
        return Ok(false);
    }
    let low = quota_low_members(app, ctx).await;
    if !low.is_empty() {
        pause_quota_low(app, ctx, &low).await?;
        return Ok(false);
    }
    Ok(true)
}

/// §4.6: `true` when a supervised gate has been released more recently than it was raised.
/// Purely event-derived, so it survives a daemon restart the way everything else does.
async fn gate_open(app: &Arc<App>, team: &db::Team, gate: &str) -> bool {
    if team.supervised == 0 {
        return true;
    }
    let reason = format!("gate:{gate}");
    let rows = sqlx::query_as::<_, db::TeamEvent>(
        "SELECT * FROM team_events WHERE team_id = ? AND kind IN ('phase','note') ORDER BY seq DESC LIMIT 200",
    )
    .bind(&team.id)
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();
    for e in rows {
        let p: Value = serde_json::from_str(&e.payload_json).unwrap_or_else(|_| json!({}));
        if e.kind == "note" && p.get("action").and_then(Value::as_str) == Some("gate_release") {
            if p.get("gate").and_then(Value::as_str) == Some(gate) {
                return true;
            }
        }
        if e.kind == "phase" && p.get("reason").and_then(Value::as_str) == Some(reason.as_str()) {
            return false;
        }
    }
    false
}

async fn hold_gate(app: &Arc<App>, ctx: &Ctx, gate: &str, what: Value) -> LcResult<()> {
    note(app, &ctx.team.id, json!({"action": "gate", "gate": gate, "pending": what})).await?;
    pause(app, &ctx.team, &format!("gate:{gate}")).await
}

// ---------------------------------------------------------------- outbox (§8.4, §9.1)

/// Deliver whatever is pending, one prompt per recipient.
///
/// This is where §8.4 lives: the PM can only hold one in-flight turn, but two workers report
/// at nearly the same time, so **every** pending relay for a bot is merged into a single
/// prompt. It is also the relay-budget and quota gate: nothing leaves without passing both.
async fn flush(app: &Arc<App>, ctx: &Ctx) -> LcResult<()> {
    if ctx.team.phase == "paused" || ctx.team.phase == "aborting" || team::is_terminal(&ctx.team.phase) {
        return Ok(());
    }
    for bot in &ctx.members {
        let pending = pending_for(app, &ctx.team.id, &bot.id).await?;
        if pending.is_empty() {
            continue;
        }
        // §4.5 relay budget. Repair prompts are rows here too, so they count.
        let (sent, cap) = if ctx.unlimited() {
            // Every relay is stamped with the first working issue, so per-issue counting means
            // nothing here; the cap is instead `max_relays` per issue the team has started.
            (
                relay_count(app, &ctx.team.id, None, "delivered").await,
                ctx.budget.max_relays * started_issue_count(app, &ctx.team.id).await,
            )
        } else {
            (relay_count(app, &ctx.team.id, ctx.issue_id(), "delivered").await, ctx.budget.max_relays)
        };
        if sent + pending.len() as i64 > cap {
            pause(app, &ctx.team, "budget_relays").await?;
            return Ok(());
        }
        // §4.5 quota, re-checked immediately before the send.
        if !team::quota_check_disabled(ctx.budget.quota_stop_pct)
            && quota_hit(app, bot).await.is_some_and(|h| h.used_pct >= ctx.budget.quota_stop_pct)
        {
            // The recipient is what stops this send, but the banner lists everyone who is out:
            // resuming only to stop on the next member helps nobody.
            let low = quota_low_members(app, ctx).await;
            pause_quota_low(app, ctx, &low).await?;
            return Ok(());
        }
        // §9.1: an undeliverable member is a pause, never a skip.
        let Some(run) = db::active_run(&app.db, &bot.id).await.map_err(up)? else {
            pause(app, &ctx.team, &format!("member_lost:{}", bot.name)).await?;
            return Ok(());
        };
        if run.state != "running" {
            pause(app, &ctx.team, &format!("member_lost:{}", bot.name)).await?;
            return Ok(());
        }
        if run.agent_status == "blocked" {
            pause(app, &ctx.team, &format!("member_blocked:{}", bot.name)).await?;
            return Ok(());
        }
        if db::in_flight_turn(&app.db, &run.id).await.map_err(up)?.is_some() {
            continue; // §8.4: queue behind the turn in flight (a user's `say` included)
        }

        let text = merged_text(&pending, bot.team_role.as_deref().unwrap_or("worker"));
        let from = pending.iter().find_map(|e| e.from_bot_id.clone());
        // §3: `team:<team_id>:<event_id>` — the event id makes the send idempotent across a
        // restart, because the retry re-uses the very same pending row.
        let crid = format!("team:{}:{}", ctx.team.id, pending[0].id);
        let out = match lifecycle::prompt_grouped(app, &bot.id, &text, &crid, None, None, &[]).await {
            Ok(o) => o,
            Err(LcError::Conflict(v)) => {
                let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
                if reason.contains("in flight") {
                    continue;
                }
                if reason.contains("blocked") {
                    pause(app, &ctx.team, &format!("member_blocked:{}", bot.name)).await?;
                } else if reason.contains("unknown delivery") {
                    pause(app, &ctx.team, "delivery_unknown").await?;
                } else {
                    pause(app, &ctx.team, &format!("member_lost:{}", bot.name)).await?;
                }
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(bot = %bot.name, error = ?e, "team relay delivery failed");
                pause(app, &ctx.team, "upstream").await?;
                return Ok(());
            }
        };
        // §2.1: `relay_from` is the source bot id, or the reserved `'daemon'` for the
        // daemon's own relays; NULL stays reserved for what the user typed personally.
        let relay_from = from.clone().unwrap_or_else(|| "daemon".into());
        let _ = sqlx::query("UPDATE messages SET team_id = ?, relay_from = ? WHERE id = ?")
            .bind(&ctx.team.id)
            .bind(&relay_from)
            .bind(&out.message_id)
            .execute(&app.db)
            .await;
        let _ = sqlx::query("UPDATE turns SET team_id = ?, team_event_id = ? WHERE id = ?")
            .bind(&ctx.team.id)
            .bind(&pending[0].id)
            .bind(&out.turn_id)
            .execute(&app.db)
            .await;
        for e in &pending {
            let _ = sqlx::query("UPDATE team_events SET status='delivered', turn_id=? WHERE id=?")
                .bind(&out.turn_id)
                .bind(&e.id)
                .execute(&app.db)
                .await;
            // §8.2: `queued ──relay 送達──► working`.
            if let Some(tid) = &e.task_id {
                let action = serde_json::from_str::<Value>(&e.payload_json)
                    .ok()
                    .and_then(|p| p.get("action").and_then(Value::as_str).map(String::from))
                    .unwrap_or_default();
                if ["dispatch", "rework", "rebase"].contains(&action.as_str()) {
                    set_task_state(app, tid, "working").await?;
                }
            }
        }
        // §7.5 / §9.1: a turn we could not confirm blocks everything until a human abandons
        // it — but only *after* the rows above are written, so the retry has something to
        // resume from. Nothing else is sent this pass.
        if out.delivery == "unknown" {
            pause(app, &ctx.team, "delivery_unknown").await?;
            return refresh_usage(app, ctx).await;
        }
    }
    refresh_usage(app, ctx).await
}

/// One prompt out of N pending relays (§8.4 / appendix A.4).
fn merged_text(pending: &[db::TeamEvent], role: &str) -> String {
    let body = |e: &db::TeamEvent| -> String {
        serde_json::from_str::<Value>(&e.payload_json)
            .ok()
            .and_then(|p| p.get("text").and_then(Value::as_str).map(String::from))
            .unwrap_or_default()
    };
    if pending.len() == 1 {
        return format!("{}{}", body(&pending[0]), footer(role));
    }
    let mut out = format!("以下是 {} 則訊息：\n", pending.len());
    for (i, e) in pending.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n", i + 1, body(e)));
    }
    out.push_str(&footer(role));
    clip(&out)
}

// ---------------------------------------------------------------- task rows

async fn set_task_state(app: &Arc<App>, task_id: &str, state: &str) -> LcResult<db::TeamTask> {
    sqlx::query("UPDATE team_tasks SET state = ?, updated_at = ? WHERE id = ?")
        .bind(state)
        .bind(db::now())
        .bind(task_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    let t = db::team_task(&app.db, task_id)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("task".into()))?;
    app.emit("team_task_updated", json!({"team_id": t.team_id, "task": team::task_json(&t)})).await;
    Ok(t)
}

async fn emit_task(app: &Arc<App>, task_id: &str) {
    if let Ok(Some(t)) = db::team_task(&app.db, task_id).await {
        app.emit("team_task_updated", json!({"team_id": t.team_id, "task": team::task_json(&t)})).await;
    }
}

/// The executor a task belongs to. Only a task still waiting in the queue for a free slot
/// (§4.5) has none, and every caller of this is past that point — a relay with no recipient
/// would be dropped silently, so this is an error rather than an empty string.
fn task_owner(t: &db::TeamTask) -> LcResult<&str> {
    t.worker_bot_id
        .as_deref()
        .ok_or_else(|| LcError::Upstream(format!("task t{} has no executor yet", t.seq)))
}

/// The task a worker is currently on. The DB's partial unique index guarantees there is at
/// most one — that is what makes `workers.count` a parallelism limit (§4.5).
async fn open_task_of(app: &Arc<App>, team_id: &str, worker: &str) -> LcResult<Option<db::TeamTask>> {
    sqlx::query_as::<_, db::TeamTask>(
        "SELECT * FROM team_tasks WHERE team_id = ? AND worker_bot_id = ?
           AND state NOT IN ('merged','skipped','failed') ORDER BY seq LIMIT 1",
    )
    .bind(team_id)
    .bind(worker)
    .fetch_optional(&app.db)
    .await
    .map_err(up)
}

// ---------------------------------------------------------------- startup (§7.4)

/// Record a member that would not start, in both places the user looks: the team timeline
/// (`member_start_failed`) and the member's own conversation. Shared by `startup` and
/// `reopen_startup` — the two differ only in what they do *after* the report.
async fn report_member_start_failure(app: &Arc<App>, team_id: &str, bot: &db::Bot, error: &str) -> LcResult<()> {
    note(app, team_id, json!({"action": "member_start_failed", "bot": bot.name, "error": error})).await?;
    // Also say it where the user is actually looking. Without this the only symptom is a grey
    // lamp and `paused(member_lost)`: the reason lived in the team log alone, and the panel
    // does not render a note that carries no `text`.
    if let Ok(conv) = db::conversation_id(&app.db, &bot.id).await {
        let _ = lifecycle::insert_message(
            app,
            &conv,
            None,
            "system",
            &format!("這個成員沒能啟動：{error}。修好原因後在這裡按「啟動」，再回 Team 面板按「繼續」。"),
            "system",
            false,
            None,
        )
        .await;
    }
    Ok(())
}

/// Stop members left running when startup takes a terminal failure path. Cleanup is deliberately
/// best effort: the startup error and its timeline note are the useful diagnosis, so a failed
/// stop must only be logged and never replace them.
async fn stop_failed_startup_members(app: &Arc<App>, members: &[db::Bot]) {
    for b in members {
        if let Err(e) = lifecycle::stop_bot(app, &b.id).await {
            tracing::warn!(bot = %b.name, error = ?e, "startup failure cleanup: stop_bot failed");
        }
    }
}

/// Start a done Team's long-lived members for a queued continuation. The old issue's workers
/// are retired first; PM/reviewer keep their bot ids, worktrees and conversations, and ask the
/// lifecycle layer to resume their last native session when the provider supports it.
async fn reopen_startup(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let issues = db::team_issues(&app.db, team_id).await.map_err(up)?;
    let previous = issues
        .into_iter()
        .filter(|i| i.state == "done")
        .max_by_key(|i| i.seq)
        .ok_or_else(|| LcError::Upstream("cannot reopen a team with no completed issue".into()))?;
    team::retire_issue_workers(app, team_id, &previous.id).await;

    let ctx = Ctx::load(app, team_id).await?;
    for e in crate::trust::pretrust_members(app, &ctx.members).await {
        tracing::warn!(team = %team_id, error = %e, "could not pre-trust a team worktree on reopen");
        note(app, team_id, json!({"action": "pretrust_failed", "error": e})).await?;
    }
    for b in &ctx.members {
        if !matches!(b.team_role.as_deref(), Some("pm") | Some("reviewer")) {
            continue;
        }
        if db::active_run(&app.db, &b.id).await.map_err(up)?.is_some() {
            continue;
        }
        if let Err(e) = lifecycle::start_bot_with(app, &b.id, lifecycle::StartOpts { resume_native: true }).await {
            report_member_start_failure(app, team_id, b, &format!("{e:?}")).await?;
            // §2.5.2: a reopen that cannot bring PM or reviewer back pauses in place. It must
            // **not** take §7.4's `failed + cleanup` road — that would throw away a finished
            // issue's worktrees over a start error the user can just fix and resume.
            let reason = format!("member_failed:{}", team::short_name(&b.name, team_id));
            pause(app, &ctx.team, &reason).await?;
            return Ok(());
        }
    }

    sqlx::query("UPDATE teams SET started_at = COALESCE(started_at, ?) WHERE id = ?")
        .bind(db::now())
        .bind(team_id)
        .execute(&app.db)
        .await
        .map_err(up)?;

    let next = db::next_queued_issue(&app.db, team_id).await.map_err(up)?;
    let Some(next) = next else {
        return Err(LcError::Upstream("reopened team has no queued issue".into()));
    };
    if let Err(e) = team::start_issue(app, team_id, &next, false).await {
        note(
            app,
            team_id,
            json!({"action": "issue_start_failed", "issue_number": next.issue_number, "error": format!("{e:?}")}),
        )
        .await?;
        let ctx = Ctx::load(app, team_id).await?;
        pause(app, &ctx.team, "upstream").await?;
        return Ok(());
    }
    start_issue_workers(app, team_id).await?;
    let pm = Ctx::load(app, team_id).await?.pm().cloned();
    let context_lost = match pm {
        Some(pm) => pm_context_lost_after_latest_reopen(app, team_id, &pm.id).await,
        None => false,
    };
    hand_issue_to_pm(app, team_id, false, Some(context_lost)).await
}

/// §2.6: the rescue is over when its issue is. Clearing the column puts `workers_for` back to
/// the ordinary pool, so a later reopen builds real executors again.
async fn clear_rescue(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    sqlx::query("UPDATE teams SET rescue_bot_id = NULL WHERE id = ? AND rescue_bot_id IS NOT NULL")
        .bind(team_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    Ok(())
}

/// Phase `starting` for a rescue (§2.6): PM, reviewer and the rescuer come back with their
/// native sessions, then the team goes straight to `working` — the task the user asked for is
/// already in the table, so there is nothing for the PM to plan.
async fn rescue_startup(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    let Some(rescuer_id) = ctx.team.rescue_bot_id.clone() else { return Ok(()) };
    for e in crate::trust::pretrust_members(app, &ctx.members).await {
        tracing::warn!(team = %team_id, error = %e, "could not pre-trust a team worktree on rescue");
        note(app, team_id, json!({"action": "pretrust_failed", "error": e})).await?;
    }
    for b in &ctx.members {
        let wanted = b.id == rescuer_id || matches!(b.team_role.as_deref(), Some("pm") | Some("reviewer"));
        if !wanted {
            continue;
        }
        if db::active_run(&app.db, &b.id).await.map_err(up)?.is_some() {
            continue;
        }
        if let Err(e) = lifecycle::start_bot_with(app, &b.id, lifecycle::StartOpts { resume_native: true }).await {
            report_member_start_failure(app, team_id, b, &format!("{e:?}")).await?;
            // Same rule as a reopen: a rescue that cannot bring a member back pauses in place.
            // Throwing away a delivered issue over a start error would be the worse trade.
            let reason = format!("member_failed:{}", team::short_name(&b.name, team_id));
            pause(app, &ctx.team, &reason).await?;
            return Ok(());
        }
    }
    sqlx::query("UPDATE teams SET started_at = COALESCE(started_at, ?) WHERE id = ?")
        .bind(db::now())
        .bind(team_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    // Starting the members takes a while; a pause or abort in the meantime wins (#31).
    if !team::sched_phase(app, team_id, "working", false).await? {
        return Ok(());
    }
    // `fill_workers` hands the task over on the next pass; tell the PM what is happening so the
    // timeline (and the PM's own context) does not skip from "done" to a merge out of nowhere.
    let ctx = Ctx::load(app, team_id).await?;
    if let Some(pm) = ctx.pm() {
        let who = ctx.by_id(&rescuer_id).map(|b| ctx.short(b)).unwrap_or_else(|| "收尾者".into());
        let text = format!(
            "使用者把這個 issue 沒解決的 task 全部交給 {who} 收尾了。\
             它做完會照常回報、審查、合併；在那之前你不用派工，回 `wait` 就好。",
        );
        enqueue(app, team_id, None, &pm.id, None, "rescue", text).await?;
    }
    Ok(())
}

/// Whether the PM received a `member_context_lost` note after the latest reopen action. The
/// scheduler uses this durable event rather than an in-memory flag, so a daemon restart between
/// starting the member and sending the relay preserves the wording choice.
async fn pm_context_lost_after_latest_reopen(app: &Arc<App>, team_id: &str, pm_id: &str) -> bool {
    let events = sqlx::query_as::<_, db::TeamEvent>("SELECT * FROM team_events WHERE team_id = ? ORDER BY seq")
        .bind(team_id)
        .fetch_all(&app.db)
        .await
        .unwrap_or_default();
    let latest_reopen = events.iter().filter_map(|e| {
        let p: Value = serde_json::from_str(&e.payload_json).ok()?;
        (p.get("action").and_then(|v| v.as_str()) == Some("team_reopened")).then_some(e.seq)
    }).max();
    let Some(reopen_seq) = latest_reopen else { return false };
    events.iter().any(|e| {
        e.seq > reopen_seq
            && e.to_bot_id.as_deref() == Some(pm_id)
            && serde_json::from_str::<Value>(&e.payload_json)
                .ok()
                .and_then(|p| p.get("action").and_then(|v| v.as_str()).map(|a| a == "member_context_lost"))
                .unwrap_or(false)
    })
}

/// Phase `starting`: bring the members up one at a time, then hand the issue to the PM.
/// Re-entrant — a member that is already running is left alone, which is what makes this
/// safe to call again after a restart.
pub async fn startup(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    if ctx.team.phase != "starting" {
        return Ok(());
    }
    // A normal creation has a working first issue. A done-team reopen has only a queued issue;
    // that distinction is durable and survives a daemon restart.
    if ctx.issues.is_empty() && db::next_queued_issue(&app.db, team_id).await.map_err(up)?.is_some() {
        return reopen_startup(app, team_id).await;
    }
    // §2.6: a rescue re-opened a finished issue with the task already written. Nothing to plan
    // and no new executors to build — just bring the three bots back and let the engine run.
    if ctx.team.rescue_bot_id.is_some() {
        return rescue_startup(app, team_id).await;
    }
    // The §6.2 worktrees are directories claude and codex have never seen, and both stop the
    // first interactive run in one on a "do you trust this folder?" dialog — with the cursor
    // on *No* for claude, so the CLI quits by itself after a minute and `agent.wait` reports
    // `agent_not_running`. Nobody is at the pane to answer it, and every team gets fresh
    // directories, so this failed *every* team. Write the record the dialog would write
    // first; `crate::trust` amends the CLI's own config file in place and is a no-op once
    // the path is already trusted, which is what makes this safe to repeat on a restart.
    for e in crate::trust::pretrust_members(app, &ctx.members).await {
        tracing::warn!(team = %team_id, error = %e, "could not pre-trust a team worktree");
        note(app, team_id, json!({"action": "pretrust_failed", "error": e})).await?;
    }

    let mut failed: Vec<String> = Vec::new();
    for b in &ctx.members {
        if db::active_run(&app.db, &b.id).await.map_err(up)?.is_some() {
            continue;
        }
        if let Err(e) = lifecycle::start_bot(app, &b.id).await {
            report_member_start_failure(app, team_id, b, &format!("{e:?}")).await?;
            // §7.4: no PM, no team. A worker short is survivable; a missing reviewer is a
            // decision for the human ("merge unreviewed" or "try again").
            match b.team_role.as_deref() {
                // §7.4: no PM, no team — and §6.5 sends a failed creation through the same
                // cleanup, so nothing is left half-built.
                Some("pm") => {
                    if !team::sched_phase(app, team_id, "failed", false).await? {
                        return Ok(());
                    }
                    stop_failed_startup_members(app, &ctx.members).await;
                    if let Err(e) = team::cleanup(app, team_id).await {
                        tracing::warn!(team = team_id, error = ?e, "cleanup after a failed PM start");
                    }
                    return Ok(());
                }
                Some("reviewer") => {
                    pause(app, &ctx.team, &format!("member_failed:{}", b.name)).await?;
                    return Ok(());
                }
                _ => failed.push(b.name.clone()),
            }
        }
    }
    if !failed.is_empty() && failed.len() == ctx.workers().len() {
        team::sched_phase(app, team_id, "failed", false).await?;
        stop_failed_startup_members(app, &ctx.members).await;
        return Ok(());
    }

    sqlx::query("UPDATE teams SET started_at = COALESCE(started_at, ?) WHERE id = ?")
        .bind(db::now())
        .bind(team_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    // `startup` is re-entrant: paused here, "continue" returns to `starting` and runs it again.
    if !team::sched_phase(app, team_id, "planning", false).await? {
        return Ok(());
    }

    // §4.5 unlimited: the first issue is already working (it was created with the team);
    // everything else on the queue starts now, and A.1's first relay covers the lot.
    let ctx = Ctx::load(app, team_id).await?;
    if ctx.unlimited() {
        if start_issues_up_to_capacity(app, team_id).await? == 0 {
            team::refresh_unlimited_docs(app, team_id).await?;
            hand_issues_to_pm(app, team_id, &[]).await?;
        }
        return Ok(());
    }
    // Appendix A.1, the first relay. `from` is NULL, so the timeline reads `daemon → pm`.
    let Some(pm) = ctx.pm() else { return Ok(()) };
    let names: Vec<String> = ctx.workers().iter().map(|w| ctx.short(w)).collect();
    let text = format!(
        "Issue #{n}「{title}」。全文在 `.agents-manager/team/ISSUE.md`（在你的 cwd 內）。\
         併行數 {k}（執行者：{who}）——`dispatch` 不用指定 `to`，派幾筆都可以，超過併行數的會排隊。\
         請先讀取 `.agents-manager/team/ISSUE.md` 與 `TEAM.md` 再派工。",
        n = ctx.team.issue_number,
        title = ctx.team.issue_title,
        k = names.len(),
        who = names.join("、"),
    );
    enqueue(app, team_id, None, &pm.id, None, "first", text).await?;
    Ok(())
}

/// The tasks that belong to the issue the team is on. Everything the scheduler decides —
/// "are we done", "is a review free", "did the PM repeat itself" — is about the current issue
/// only, never the whole team's history (§2.3). Falls back to every task when a team somehow
/// has no current issue, which is what a pre-queue database looks like mid-migration.
async fn issue_tasks(app: &Arc<App>, team_id: &str, issue_id: Option<&str>) -> LcResult<Vec<db::TeamTask>> {
    match issue_id {
        Some(i) => db::team_tasks_of_issue(&app.db, team_id, i).await.map_err(up),
        None => db::team_tasks(&app.db, team_id).await.map_err(up),
    }
}

/// Everything the scheduler is currently responsible for: the tasks of **every** working
/// issue, in `seq` order. Identical to `issue_tasks(ctx.issue_id())` whenever there is one
/// working issue, which is every team with a finite parallelism.
async fn sched_tasks(app: &Arc<App>, ctx: &Ctx) -> LcResult<Vec<db::TeamTask>> {
    if !ctx.unlimited() {
        return issue_tasks(app, &ctx.team.id, ctx.issue_id()).await;
    }
    let mut out = Vec::new();
    for q in &ctx.issues {
        out.extend(db::team_tasks_of_issue(&app.db, &ctx.team.id, &q.id).await.map_err(up)?);
    }
    out.sort_by_key(|t| t.seq);
    Ok(out)
}

/// How many issues this team has actually started (working or finished). The relay budget is
/// "per issue" (§9.2), and in unlimited mode a relay cannot be attributed to one issue — the
/// PM answers several in a single turn — so the cap is scaled by this instead.
async fn started_issue_count(app: &Arc<App>, team_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM team_issues WHERE team_id = ? AND state IN ('working','done','failed')",
    )
    .bind(team_id)
    .fetch_one(&app.db)
    .await
    .unwrap_or(1)
    .max(1)
}

/// §4.4: which working issue an `issue` field points at. `None` is allowed only while exactly
/// one issue is working — the case every finite-parallelism team is always in.
fn resolve_issue<'a>(ctx: &'a Ctx, want: Option<i64>) -> Result<&'a db::TeamIssue, String> {
    match want {
        Some(n) => ctx
            .issues
            .iter()
            .find(|i| i.issue_number == n)
            .ok_or_else(|| format!("#{n} 不是進行中的 issue（進行中：{}）", issue_list(ctx))),
        None => match ctx.issues.len() {
            1 => Ok(&ctx.issues[0]),
            0 => Err("目前沒有進行中的 issue".into()),
            _ => Err(format!("同時有多個 issue 進行中（{}），每一筆都要寫 `issue`", issue_list(ctx))),
        },
    }
}

/// `[#48] ` in front of a relay to the PM, so a message about one of several issues in
/// flight says which one at a glance (A.4). Empty with a finite parallelism.
fn issue_tag(ctx: &Ctx, issue_id: Option<&str>) -> String {
    if !ctx.unlimited() {
        return String::new();
    }
    match issue_id.and_then(|i| ctx.issue_of(i)) {
        Some(q) => format!("[#{}] ", q.issue_number),
        None => String::new(),
    }
}

fn issue_list(ctx: &Ctx) -> String {
    ctx.issues.iter().map(|i| format!("#{}", i.issue_number)).collect::<Vec<_>>().join("、")
}

// ---------------------------------------------------------------- one pass (§8)

/// One idempotent pass: gates, then the task engine, then the outbox.
pub async fn step(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    if team::is_terminal(&ctx.team.phase) || ctx.team.phase == "aborting" || ctx.team.phase == "paused" {
        return Ok(());
    }
    // §6.1 #1, checked before any git runs: a team whose members are not inside its own
    // worktree root is stopped, not "fixed". This is the guard the 55-uncommitted-files
    // incident needed — the broken team's members had an explicit cwd of the main checkout.
    if let Err(e) = ctx.check_layout() {
        note(app, team_id, json!({"action": "unsafe_layout", "detail": format!("{e:?}")})).await?;
        pause(app, &ctx.team, "worktree_missing").await?;
        return Ok(());
    }
    if !gates(app, &ctx).await? {
        return Ok(());
    }
    advance_tasks(app, team_id).await?;
    let ctx = Ctx::load(app, team_id).await?;
    if ctx.team.phase == "finishing" {
        finish(app, &ctx).await?;
        return Ok(());
    }
    flush(app, &ctx).await
}

/// The git-carrying half of a pass: merge what is approved, send what is reported to review.
/// Split out from [`step`] so a test can drive the state machine without a live agent.
pub async fn advance_tasks(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    // A task can cross two edges in one pass — `reported → merging → merged` when there is
    // no reviewer, or `merged` freeing the reviewer for the next `reported` task — so this
    // runs to a fixed point rather than once. The bound is a safety net, not a schedule:
    // every iteration must move at least one task, and there are finitely many.
    for _ in 0..16 {
        if !advance_once(app, team_id).await? {
            return Ok(());
        }
    }
    Ok(())
}

/// One sweep. `true` = something changed and it is worth sweeping again.
async fn advance_once(app: &Arc<App>, team_id: &str) -> LcResult<bool> {
    let ctx = Ctx::load(app, team_id).await?;
    if team::is_terminal(&ctx.team.phase) || ctx.team.phase == "paused" || ctx.team.phase == "aborting" {
        return Ok(false);
    }
    ctx.check_layout()?;
    // 0. §4.5: give any free executor the next queued task. This is the one place the pool
    //    refills, so it has to run before the states below move a task into a terminal one —
    //    and again on the next sweep, which is what `changed` below buys.
    fill_workers(app, &ctx).await?;
    let tasks = sched_tasks(app, &ctx).await?;

    // 1. a reported task goes to review, or — with no reviewer — straight to the merge queue.
    //    Review is serialised: one task at a time, in report order (§8.4).
    let mut changed = false;
    match ctx.reviewer() {
        None => {
            // §8.2: no reviewer means `reported → merging`.
            for t in tasks.iter().filter(|t| t.state == "reported") {
                set_task_state(app, &t.id, "merging").await?;
                changed = true;
            }
        }
        Some(rev) => {
            // §2.6: nobody reviews their own work. A rescue is carried by the reviewer itself,
            // so its report goes straight to the merge queue — the alternative is asking a bot
            // to approve the diff it just wrote.
            for t in tasks.iter().filter(|t| t.state == "reported" && t.worker_bot_id.as_deref() == Some(rev.id.as_str())) {
                set_task_state(app, &t.id, "merging").await?;
                changed = true;
            }
            if !changed && !tasks.iter().any(|t| t.state == "reviewing") {
            if let Some(t) = tasks.iter().find(|t| t.state == "reported" && t.worker_bot_id.as_deref() != Some(rev.id.as_str())) {
                let wt = ctx.wt(rev)?;
                if let Err(e) = tg::checkout_detach(app, ctx.host(), &wt, &t.branch).await {
                    note(app, team_id, json!({"action": "review_checkout_failed", "error": e.to_string()})).await?;
                    pause(app, &ctx.team, "upstream").await?;
                    return Ok(false);
                }
                set_task_state(app, &t.id, "reviewing").await?;
                let text = format!(
                    "請審查 task t{seq}「{title}」（分支 `{br}`，第 {round} 回）。\
                     你的 cwd 已 checkout 到該分支（detached）。用 `git diff {integ}...HEAD` 看變更。\
                     執行者的回報：{report}",
                    seq = t.seq,
                    title = t.title,
                    br = t.branch,
                    round = t.round + 1,
                    // §4.5: the diff base is **this task's** issue, not the `teams` mirror —
                    // with several issues in flight the mirror is somebody else's branch.
                    integ = ctx.integration_of(t),
                    report = t.last_report.clone().unwrap_or_default(),
                );
                enqueue(app, team_id, t.worker_bot_id.as_deref(), &rev.id, Some(&t.id), "review", text).await?;
                changed = true;
            }
            }
        }
    }

    // 2. merge whatever is approved — SPEC-team §6.1 #3, by the daemon, with git.
    for t in sched_tasks(app, &ctx).await? {
        if t.state != "merging" {
            continue;
        }
        let ctx = Ctx::load(app, team_id).await?;
        if !gate_open(app, &ctx.team, "merge").await {
            hold_gate(app, &ctx, "merge", json!({"task_id": t.id, "branch": t.branch})).await?;
            return Ok(false);
        }
        if !merge_one(app, &ctx, &t).await? {
            return Ok(false); // paused
        }
        // A merge frees a slot, so the next sweep's `fill_workers` has something to do.
        changed = true;
    }
    Ok(changed)
}

/// One `git merge --no-ff` and everything that follows from its two outcomes.
/// `false` = the team was paused and the caller must stop.
async fn merge_one(app: &Arc<App>, ctx: &Ctx, t: &db::TeamTask) -> LcResult<bool> {
    let main_wt = ctx.main_wt()?;
    let integration = ctx.integration_of(t);
    // §6.3: never fold a hand edit of the integration branch into a merge commit.
    match tg::status_porcelain(app, ctx.host(), &main_wt).await {
        Ok(s) if !s.trim().is_empty() => {
            note(app, &ctx.team.id, json!({"action": "integration_dirty", "status": s})).await?;
            pause(app, &ctx.team, "integration_dirty").await?;
            return Ok(false);
        }
        Ok(_) => {}
        Err(e) => {
            note(app, &ctx.team.id, json!({"action": "integration_missing", "error": e.to_string()})).await?;
            pause(app, &ctx.team, "worktree_missing").await?;
            return Ok(false);
        }
    }
    // §4.5: one integration worktree, several integration branches. Park it on the one this
    // task belongs to before merging — with a finite parallelism it is already there.
    if integration != ctx.team.branch {
        if let Err(e) = tg::checkout_branch(app, ctx.host(), &main_wt, &integration).await {
            note(app, &ctx.team.id, json!({"action": "integration_checkout_failed",
                 "branch": integration, "error": e.to_string()}))
            .await?;
            pause(app, &ctx.team, "upstream").await?;
            return Ok(false);
        }
    }
    let outcome = tg::merge_task(app, ctx.host(), &main_wt, &t.branch).await.map_err(up)?;
    match outcome {
        tg::MergeOutcome::Merged { sha } => {
            sqlx::query("UPDATE team_tasks SET state='merged', merge_sha=?, updated_at=? WHERE id=?")
                .bind(&sha)
                .bind(db::now())
                .bind(&t.id)
                .execute(&app.db)
                .await
                .map_err(up)?;
            emit_task(app, &t.id).await;
            team::record_event(
                app,
                &ctx.team.id,
                "merge",
                None,
                None,
                Some(&t.id),
                None,
                json!({"branch": t.branch, "result": "merged", "sha": sha}),
            )
            .await?;
            if let Some(pm) = ctx.pm() {
                let text = format!(
                    "{}t{} 「{}」已合併進 `{}`（{}）。",
                    issue_tag(ctx, t.issue_id.as_deref()),
                    t.seq,
                    t.title,
                    integration,
                    &sha[..sha.len().min(8)]
                );
                enqueue(app, &ctx.team.id, None, &pm.id, Some(&t.id), "merge_note", text).await?;
            }
            Ok(true)
        }
        tg::MergeOutcome::Conflict { files, message } => {
            team::record_event(
                app,
                &ctx.team.id,
                "merge",
                None,
                None,
                Some(&t.id),
                None,
                json!({"branch": t.branch, "result": "conflict", "conflict_files": files, "message": message}),
            )
            .await?;
            let attempts = t.rebase_attempts + 1;
            sqlx::query("UPDATE team_tasks SET state='rebasing', rebase_attempts=?, updated_at=? WHERE id=?")
                .bind(attempts)
                .bind(db::now())
                .bind(&t.id)
                .execute(&app.db)
                .await
                .map_err(up)?;
            emit_task(app, &t.id).await;
            if attempts > MAX_REBASES {
                pause_issue(app, ctx, t.issue_id.as_deref(), "merge_conflict").await?;
                return Ok(false);
            }
            // §6.1 #4: whoever wrote the code resolves the conflict, in their own worktree.
            let text = format!(
                "整合分支 `{integ}` 已前進，你的 t{seq} 合併時衝突（{files}）。\
                 請在你的 worktree 執行 `git rebase {integ}`，解掉衝突並確認可建置後再 `report`。",
                integ = integration,
                seq = t.seq,
                files = if files.is_empty() { "見 git 輸出".into() } else { files.join("、") },
            );
            enqueue(app, &ctx.team.id, None, task_owner(&t)?, Some(&t.id), "rebase", text).await?;
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------- replies (§4.4 → §8.2)

/// A member's turn ended: find its reply and run it through the protocol.
/// Is `bot_id` a live member of `team_id`? Used to route a member's non-relay turns.
async fn member_of(app: &Arc<App>, team_id: &str, bot_id: &str) -> bool {
    sqlx::query_scalar::<_, String>("SELECT team_id FROM bots WHERE id = ? AND deleted_at IS NULL")
        .bind(bot_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .as_deref()
        == Some(team_id)
}

pub async fn on_turn_done(
    app: &Arc<App>,
    team_id: &str,
    bot_id: &str,
    turn_id: &str,
    status: &str,
) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    if team::is_terminal(&ctx.team.phase) {
        return Ok(());
    }
    if ctx.by_id(bot_id).is_none() {
        return Ok(());
    }
    // Only a turn the scheduler itself produced carries the protocol. A user's `say` (event
    // kind `user`) is an ordinary conversation and must never draw a repair prompt.
    let ev_kind = sqlx::query_scalar::<_, String>(
        "SELECT e.kind FROM team_events e JOIN turns t ON t.team_event_id = e.id WHERE t.id = ?",
    )
    .bind(turn_id)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?;
    let text = sqlx::query_scalar::<_, String>(
        "SELECT content FROM messages WHERE turn_id = ? AND role = 'assistant' ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(turn_id)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?
    .unwrap_or_default();
    if ev_kind.as_deref() != Some("relay") {
        // Not the scheduler's turn — but if the member *chose* to end it with a valid block
        // (a user nudged it with「請再 report 一次」after a lost reply, 2026-09-08 #50), that
        // block is its answer and is applied like any other. An invalid or absent block is
        // still just conversation: no repair, no attempt charged.
        let role = ctx.by_id(bot_id).and_then(|b| b.team_role.clone()).unwrap_or_else(|| "worker".into());
        if status == "completed" && parse_block(&text).and_then(|v| Action::parse(&role, &v)).is_ok() {
            note(app, team_id, json!({"action": "user_turn_block", "bot": bot_id, "role": role})).await?;
            let _ = apply_reply(app, team_id, bot_id, &text, status).await;
        }
        return step(app, team_id).await;
    }
    // A reply that could not be applied — an unsafe layout, a git command that failed — is
    // still followed by a `step`, because `step` is what turns that condition into a visible
    // `paused` rather than a warning in the log.
    let applied = apply_reply(app, team_id, bot_id, &text, status).await;
    if let Err(e) = &applied {
        // A reply the scheduler failed to apply (a DB or git error, not a protocol error)
        // would otherwise vanish: the member has said its piece, nothing is pending for it,
        // and the team sits in its phase forever. Ask once for a resend; the second failure
        // in a row is left to the log, so a persistent fault cannot ping-pong.
        let last_was_retry = sqlx::query_scalar::<_, String>(
            "SELECT payload_json FROM team_events WHERE team_id=? AND to_bot_id=? AND kind='relay' ORDER BY seq DESC LIMIT 1",
        )
        .bind(team_id)
        .bind(bot_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .and_then(|p| serde_json::from_str::<Value>(&p).ok())
        .map(|v| v["action"] == "retry")
        .unwrap_or(false);
        if !last_was_retry {
            let text = format!(
                "系統套用你上一則回覆時失敗（{e:?}），沒有任何 task 被建立或狀態被改動。請依原本的判斷把同一份 ```am-team 區塊重新送一次。"
            );
            enqueue(app, team_id, None, bot_id, None, "retry", text).await?;
        }
    }
    let stepped = step(app, team_id).await;
    applied.and(stepped)
}

/// Parse one reply and move the state machine. Writes rows; sends nothing.
///
/// `status` is the turn's: `completed_fallback` (a terminal scrape, possibly truncated) and
/// `failed` are both treated as "no block", per §4.4's last paragraph.
pub async fn apply_reply(
    app: &Arc<App>,
    team_id: &str,
    bot_id: &str,
    text: &str,
    status: &str,
) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    let Some(bot) = ctx.by_id(bot_id).cloned() else { return Ok(()) };
    let role = bot.team_role.clone().unwrap_or_else(|| "worker".into());
    // `report` commits in the member's worktree and `dispatch` cuts a branch in it, so the
    // §6.1 invariant is checked here too — a reply must never be the thing that reaches a
    // directory the team does not own.
    ctx.check_layout()?;

    if status == "failed" {
        return repair(app, &ctx, &bot, "上一回合沒有完成").await;
    }
    if status == "completed_fallback" {
        // A terminal scrape is never trusted as a reply (§4.4, even with a block in it). But
        // if it caught a *busy* screen (a spinner, `esc to interrupt`) the member is still
        // working: the real reply comes with the hook, so a repair prompt would land mid-turn
        // and the attempt would be charged for nothing. Note it and wait.
        if fallback_caught_busy_screen(text) {
            note(app, team_id, json!({"action": "fallback_busy", "bot": bot.name, "role": role})).await?;
            return Ok(());
        }
        return repair(app, &ctx, &bot, "你的回覆是從終端畫面擷取的，可能不完整").await;
    }
    let action = match parse_block(text).and_then(|v| Action::parse(&role, &v)) {
        Ok(a) => a,
        Err(e) => return repair(app, &ctx, &bot, &e).await,
    };
    // A reply that parsed ends the repair chain. Without this marker the count would be
    // "repair prompts at the tail of this bot's relay log", which keeps two old repairs
    // alive across any number of good turns in between — a later single bad reply would
    // then pause the team on its first offence instead of its third.
    team::record_event(
        app,
        team_id,
        "note",
        None,
        Some(&bot.id),
        None,
        None,
        json!({"action": "reply_ok", "role": role}),
    )
    .await?;
    match action {
        Action::Dispatch(items) => dispatch(app, &ctx, &bot, items).await,
        Action::Wait => wait(app, &ctx).await,
        Action::Done { summary, keep_workers, issue } => {
            pm_done(app, &ctx, &bot, &summary, keep_workers, issue).await
        }
        Action::AskUser { question } => {
            note(app, team_id, json!({"action": "ask_user", "question": question})).await?;
            pause(app, &ctx.team, "ask_user").await
        }
        Action::Abort { reason } => {
            note(app, team_id, json!({"action": "pm_abort", "reason": reason})).await?;
            pause(app, &ctx.team, "pm_abort").await
        }
        Action::Report { blocked, summary, notes } => report(app, &ctx, &bot, blocked, &summary, &notes).await,
        Action::Verdict { approve, summary, must_fix } => verdict(app, &ctx, &bot, approve, &summary, &must_fix).await,
    }
}

/// The terminal fallback scraped a pane that was still mid-turn: codex's
/// `• Working (4s • esc to interrupt)`, claude's `✢ Baking…`, or a tool line still running.
/// Such a "reply" is not the member's answer and must not count against it.
fn fallback_caught_busy_screen(text: &str) -> bool {
    text.lines().map(str::trim).any(|l| {
        l.contains("esc to interrupt")
            || (l.starts_with('•') && l.contains("Working ("))
            || (l.starts_with("Running ") && l.ends_with('…'))
    })
}

/// §4.4's repair prompt. Two of these back to back for the same member and a human takes over.
async fn repair(app: &Arc<App>, ctx: &Ctx, bot: &db::Bot, why: &str) -> LcResult<()> {
    let role = bot.team_role.as_deref().unwrap_or("worker");
    let n = consecutive_repairs(app, &ctx.team.id, &bot.id).await;
    note(app, &ctx.team.id, json!({"action": "protocol_error", "bot": bot.name, "error": why, "attempt": n + 1}))
        .await?;
    if n >= MAX_REPAIRS {
        pause(app, &ctx.team, "protocol_error").await?;
        return Ok(());
    }
    let text = format!(
        "你上一則回覆沒有有效的 am-team 區塊：{why}。請只回傳該區塊（```am-team 開頭的 JSON），\
         允許的 action：{}。",
        allowed(role)
    );
    enqueue(app, &ctx.team.id, None, &bot.id, None, "repair", text).await?;
    Ok(())
}

/// How many repair prompts this member has drawn in a row: walk its own slice of the log
/// backwards and stop at the first thing that is not a repair prompt — an ordinary relay, or
/// the `reply_ok` marker a successfully parsed reply leaves behind.
async fn consecutive_repairs(app: &Arc<App>, team_id: &str, bot_id: &str) -> i64 {
    let rows = sqlx::query_as::<_, db::TeamEvent>(
        "SELECT * FROM team_events WHERE team_id = ? AND to_bot_id = ? AND kind IN ('relay','note')
         ORDER BY seq DESC LIMIT 20",
    )
    .bind(team_id)
    .bind(bot_id)
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();
    let mut n = 0;
    for e in rows {
        let a = serde_json::from_str::<Value>(&e.payload_json)
            .ok()
            .and_then(|p| p.get("action").and_then(Value::as_str).map(String::from))
            .unwrap_or_default();
        if e.kind == "relay" && a == "repair" {
            n += 1;
        } else {
            break;
        }
    }
    n
}

// ---------------------------------------------------------------- PM

/// §4.5 — the PM hands over a batch of tasks; the daemon decides who runs them and when.
///
/// Two halves, deliberately separate. **Intake** writes every item as a `queued` row (with a
/// worker only when the PM named one) and nothing else: no branch, no relay. **Fill**
/// ([`fill_workers`]) is what actually starts work, and it runs whenever a slot might have
/// opened. That split is why the PM may dispatch ten tasks against a parallelism of two —
/// eight simply wait — and why `gate:dispatch` can stop between "the plan is recorded" and
/// "the executors are told".
async fn dispatch(app: &Arc<App>, ctx: &Ctx, pm: &db::Bot, items: Vec<DispatchItem>) -> LcResult<()> {
    let mut rejected: Vec<String> = Vec::new();
    let mut created: Vec<String> = Vec::new(); // task shorthand (`t3`), for the gate note
    let existing = sched_tasks(app, ctx).await?;
    // `team_tasks_seq` is unique per **team**, not per issue: the second issue's first task
    // must continue the numbering (t2, t3…) or the INSERT below collides with t1 of the
    // first issue and the PM's whole dispatch is lost.
    let mut next_seq = db::team_tasks(&app.db, &ctx.team.id).await.map_err(up)?.iter().map(|t| t.seq).max().unwrap_or(0);
    // §6.3: overlapping `files` are a warning to the PM, never a refusal.
    let mut overlaps: Vec<String> = Vec::new();
    // The same brief twice inside one issue is a loop, whoever it was aimed at (§4.5). Since
    // the daemon now picks the executor, "same brief to the same worker" would no longer
    // catch the PM re-sending its whole plan — the brief alone is the identity.
    // Paired with the issue: the same brief on two different issues is two jobs, not a loop.
    let mut briefs: Vec<(Option<String>, String)> =
        existing.iter().map(|t| (t.issue_id.clone(), t.brief.trim().to_string())).collect();

    for it in items {
        // §4.4: which issue this task is for. With one issue working the field may be left
        // out; with several the daemon refuses rather than guessing (a task on the wrong
        // integration branch is a merge conflict nobody asked for).
        let issue = match resolve_issue(ctx, it.issue) {
            Ok(q) => q.clone(),
            Err(e) => {
                rejected.push(format!("「{}」：{e}", it.title));
                continue;
            }
        };
        // A named executor is still honoured — but a busy one is now a queue to join, not a
        // refusal: the task simply waits for *that* worker instead of any worker.
        let worker = match &it.to {
            None => None,
            Some(to) => {
                let want = to.trim().trim_start_matches('@').to_ascii_lowercase();
                let Some(w) = ctx
                    .workers()
                    .into_iter()
                    .find(|w| {
                        w.id == *to || w.name.to_ascii_lowercase() == want || ctx.short(w).to_ascii_lowercase() == want
                    })
                    .cloned()
                else {
                    rejected.push(format!("`{to}` 不是這個 team 的執行者"));
                    continue;
                };
                Some(w)
            }
        };
        if briefs.iter().any(|(i, b)| i.as_deref() == Some(issue.id.as_str()) && b == it.brief.trim()) {
            note(app, &ctx.team.id, json!({"action": "pm_repeat", "brief": it.brief})).await?;
            pause(app, &ctx.team, "pm_repeat").await?;
            return Ok(());
        }
        briefs.push((Some(issue.id.clone()), it.brief.trim().to_string()));
        for other in existing.iter().filter(|t| {
            t.issue_id.as_deref() == Some(issue.id.as_str())
                && !["merged", "skipped", "failed"].contains(&t.state.as_str())
        }) {
            let of: Vec<String> = serde_json::from_str(&other.files_json).unwrap_or_default();
            for f in &it.files {
                if of.iter().any(|x| x == f) {
                    overlaps.push(format!("t{} 與新的 task 都列了 {f}", other.seq));
                }
            }
        }

        next_seq += 1;
        // The branch **name** is settled now so the row is complete and the PM can be told
        // about it; the branch itself is cut in `fill_workers`, at the moment the task
        // actually starts, so §6.2 still holds (it contains everything merged before it).
        let branch =
            team::task_branch(issue.branch.as_deref().unwrap_or(&ctx.team.branch), next_seq);
        let task_id = db::ulid();
        let now = db::now();
        sqlx::query(
            "INSERT INTO team_tasks (id, team_id, issue_id, seq, title, brief, files_json, want_worker_bot_id,
               branch, state, round, rebase_attempts, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,'queued',0,0,?,?)",
        )
        .bind(&task_id)
        .bind(&ctx.team.id)
        .bind(&issue.id)
        .bind(next_seq)
        .bind(&it.title)
        .bind(&it.brief)
        .bind(serde_json::to_string(&it.files).unwrap_or_else(|_| "[]".into()))
        .bind(worker.as_ref().map(|w| w.id.clone()))
        .bind(&branch)
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .map_err(up)?;
        emit_task(app, &task_id).await;
        created.push(format!("t{next_seq}"));
    }

    // The tasks are on record either way; when the team was paused under this pass (#31) they
    // wait for "continue", whose `fill_now` hands them out.
    let moved = created.is_empty()
        || ctx.team.phase == "working"
        || team::sched_phase(app, &ctx.team.id, "working", true).await?;
    if !rejected.is_empty() || !overlaps.is_empty() {
        let mut msg = String::new();
        if !rejected.is_empty() {
            msg.push_str(&format!("以下派工沒有生效：{}。", rejected.join("；")));
        }
        if !overlaps.is_empty() {
            msg.push_str(&format!("注意：{}（請以檔案 / 模組切分）。", overlaps.join("；")));
        }
        enqueue(app, &ctx.team.id, None, &pm.id, None, "note", msg).await?;
    }
    if !moved {
        return Ok(());
    }
    // §4.6 gate:dispatch — the plan is on record, but nothing has been cut or sent yet, so
    // this is exactly where a human gets to look before the executors start moving.
    if !created.is_empty() && !gate_open(app, &ctx.team, "dispatch").await {
        let ctx2 = Ctx::load(app, &ctx.team.id).await?;
        hold_gate(app, &ctx2, "dispatch", json!({"tasks": created})).await?;
        return Ok(());
    }
    // §4.5 unlimited: each issue starts with one executor and grows to fit what the PM
    // actually dispatched — capped per issue and for the whole team.
    if ctx.unlimited() {
        let mut grew = false;
        for q in ctx.issues.clone() {
            let open = issue_tasks(app, &ctx.team.id, Some(&q.id))
                .await?
                .into_iter()
                .filter(|t| !["merged", "skipped", "failed"].contains(&t.state.as_str()))
                .count() as u32;
            let have = ctx.workers_for(&q).len() as u32;
            if open > have {
                let now = team::ensure_workers_for(app, &ctx.team.id, &q, open).await?;
                grew |= now > have;
            }
        }
        if grew {
            team::refresh_unlimited_docs(app, &ctx.team.id).await?;
        }
    }
    let ctx = Ctx::load(app, &ctx.team.id).await?;
    fill_workers(app, &ctx).await?;
    // Tell the PM what its batch actually became — but only when the answer is not simply
    // "all of it is running". Every note here becomes a prompt the PM has to answer, so a
    // confirmation of what it just asked for would cost a whole turn to say nothing. The
    // A.4 table under each batch of reports carries the same counts for free.
    if !created.is_empty() {
        let tasks = sched_tasks(app, &ctx).await?;
        let n = ctx.workers().len();
        let running: Vec<String> = tasks
            .iter()
            .filter(|t| t.worker_bot_id.is_some() && !["merged", "skipped", "failed"].contains(&t.state.as_str()))
            .map(|t| {
                let who = t.worker_bot_id.as_deref().and_then(|w| ctx.by_id(w)).map(|b| ctx.short(b)).unwrap_or_default();
                format!("t{}→{who}", t.seq)
            })
            .collect();
        let waiting = tasks.iter().filter(|t| t.worker_bot_id.is_none()).count();
        if waiting > 0 {
            let msg = format!(
                "收到 {got} 筆。併行數 {n}：現在跑 {m} 筆（{who}），排隊 {waiting} 筆——排隊的會在有執行者空下來時自動派出，你不用再派一次。",
                got = created.len(),
                m = running.len(),
                who = if running.is_empty() { "無".into() } else { running.join("、") },
            );
            enqueue(app, &ctx.team.id, None, &pm.id, None, "note", msg).await?;
        }
    }
    Ok(())
}

/// §4.5 — fill the free executor slots right now, called from outside the scheduler loop:
/// releasing `gate:dispatch`, resuming a paused team, or raising the parallelism. All three
/// are user actions whose whole point is that work starts, so they cannot wait for the next
/// background pass. A no-op when nothing is queued or nobody is free.
pub async fn fill_now(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    fill_workers(app, &ctx).await
}

/// §4.5 — hand queued tasks to whichever executors are free, in `seq` order.
///
/// This is the whole of the pool: `workers.count` executors exist, the DB's
/// `team_tasks_one_open_per_worker` index means each can hold exactly one unfinished task, so
/// "free" is simply "has no open task". Called after intake, after every task reaches a
/// terminal state, on resume, and on each scheduler pass — always the same function, never a
/// background loop of its own.
async fn fill_workers(app: &Arc<App>, ctx: &Ctx) -> LcResult<()> {
    if team::is_terminal(&ctx.team.phase) || ctx.team.phase == "paused" || ctx.team.phase == "aborting" {
        return Ok(());
    }
    // §4.5: one pass per working issue. Each issue has its own executors and its own
    // integration branch, and a task must be cut from **its** branch — using the `teams`
    // mirror here would branch issue #48's task off issue #49's work.
    for issue in ctx.issues.clone() {
        fill_issue(app, ctx, &issue).await?;
    }
    Ok(())
}

async fn fill_issue(app: &Arc<App>, ctx: &Ctx, issue: &db::TeamIssue) -> LcResult<()> {
    let integration = issue.branch.clone().unwrap_or_else(|| ctx.team.branch.clone());
    for worker in ctx.workers_for(issue) {
        // A worker with no live run is deliberately *not* skipped here. §9.1 says an
        // undeliverable member is a pause, never a skip, and that is what `flush` does with
        // the relay this loop queues. Skipping instead would leave the queue quietly starved
        // with the team still reading as `working`.
        if open_task_of(app, &ctx.team.id, &worker.id).await?.is_some() {
            continue;
        }
        // The earliest queued task that is either unassigned or was addressed to this one.
        let Some(t) = issue_tasks(app, &ctx.team.id, Some(&issue.id))
            .await?
            .into_iter()
            .find(|t| {
                t.state == "queued"
                    && t.worker_bot_id.is_none()
                    && t.want_worker_bot_id.as_deref().map(|w| w == worker.id).unwrap_or(true)
            })
        else {
            continue;
        };
        // §6.2: cut from the integration branch **now** — at the moment the task starts, not
        // when it was planned — so it already contains everything merged before it.
        let worker_wt = ctx.wt(worker)?;
        if let Err(e) = tg::checkout_task_branch(app, ctx.host(), &worker_wt, &t.branch, &integration).await {
            note(app, &ctx.team.id, json!({"action": "branch_failed", "branch": t.branch, "error": e.to_string()}))
                .await?;
            pause(app, &ctx.team, "upstream").await?;
            return Ok(());
        }
        sqlx::query("UPDATE team_tasks SET worker_bot_id = ?, updated_at = ? WHERE id = ?")
            .bind(&worker.id)
            .bind(db::now())
            .bind(&t.id)
            .execute(&app.db)
            .await
            .map_err(up)?;
        emit_task(app, &t.id).await;
        let files: Vec<String> = serde_json::from_str(&t.files_json).unwrap_or_default();
        let head = if ctx.unlimited() {
            format!("這是 issue #{} 的工作，背景在 `.agents-manager/team/ISSUE-{}.md`。\n\n", issue.issue_number, issue.issue_number)
        } else {
            String::new()
        };
        let text = format!(
            "{head}Task t{seq}「{title}」（分支 `{branch}` 已建好並 checkout 在你的 cwd）。\n\n{brief}\n\n\
             相關檔案：{files}\n\
             只在你的 cwd 工作：不要 cd 出去、不要動 ../、不要 git push、不要切換分支。\
             每個邏輯段落 `git commit`，完成後回覆結尾附一個 am-team 區塊回報，格式固定：\n\
             ```am-team\n{{\"action\":\"report\",\"status\":\"done\",\"summary\":\"改了什麼、怎麼驗的\"}}\n```\n\
             做不下去就 `\"status\":\"blocked\"` 並在 summary 說明原因。欄位就這三個，不要包在別的物件裡。",
            seq = t.seq,
            title = t.title,
            branch = t.branch,
            brief = t.brief,
            files = if files.is_empty() { "（未指定）".into() } else { files.join("、") },
        );
        let pm_id = ctx.pm().map(|p| p.id.clone());
        enqueue(app, &ctx.team.id, pm_id.as_deref(), &worker.id, Some(&t.id), "dispatch", text).await?;
    }
    Ok(())
}

/// §8.3: a PM that says `wait` when there is nothing left to wait for gets one nudge; the next
/// such reply pauses the team until a user decides what to do.
async fn wait(app: &Arc<App>, ctx: &Ctx) -> LcResult<()> {
    let tasks = sched_tasks(app, ctx).await?;
    if !tasks.is_empty() && !tasks.iter().all(|t| ["merged", "skipped", "failed"].contains(&t.state.as_str())) {
        return Ok(());
    }
    let Some(pm) = ctx.pm() else { return Ok(()) };
    // Only one nudge: if the previous relay to the PM already was one, stop asking.
    let last = sqlx::query_as::<_, db::TeamEvent>(
        "SELECT * FROM team_events WHERE team_id=? AND to_bot_id=? AND kind='relay' ORDER BY seq DESC LIMIT 1",
    )
    .bind(&ctx.team.id)
    .bind(&pm.id)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?;
    if let Some(e) = last {
        let a = serde_json::from_str::<Value>(&e.payload_json)
            .ok()
            .and_then(|p| p.get("action").and_then(Value::as_str).map(String::from))
            .unwrap_or_default();
        if a == "nudge" {
            if ctx.team.phase != "paused" {
                pause(app, &ctx.team, "pm_stalled").await?;
                enqueue(
                    app,
                    &ctx.team.id,
                    None,
                    &pm.id,
                    None,
                    "nudge",
                    "PM 連續兩次回覆 `wait`，team 已暫停，等使用者決定。".into(),
                )
                .await?;
            }
            return Ok(());
        }
    }
    enqueue(
        app,
        &ctx.team.id,
        None,
        &pm.id,
        None,
        "nudge",
        format!("所有 {} 個 task 都已進入終態。請 `done`（附 summary）或再 `dispatch`。", tasks.len()),
    )
    .await
    .map(|_| ())
}

/// §8.3: resume a `pm_stalled` team with a fresh prompt instead of leaving the PM idle.
pub async fn nudge_pm_after_resume(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    let Some(pm) = ctx.pm() else { return Ok(()) };
    enqueue(
        app,
        team_id,
        None,
        &pm.id,
        None,
        "nudge",
        "team 已恢復執行。請 `done`（附 summary）或再 `dispatch`。".into(),
    )
    .await
    .map(|_| ())
}

/// §8.3: the PM declares completion, the daemon verifies it.
async fn pm_done(
    app: &Arc<App>,
    ctx: &Ctx,
    pm: &db::Bot,
    summary: &str,
    keep_workers: bool,
    want_issue: Option<i64>,
) -> LcResult<()> {
    // §4.5: `done` settles **one** issue. With a finite parallelism there is only ever one to
    // settle and the field may be left out; unlimited mode requires it and delivers only that
    // issue — the others keep running.
    let issue = match resolve_issue(ctx, want_issue) {
        Ok(q) => q.clone(),
        Err(e) => {
            enqueue(app, &ctx.team.id, None, &pm.id, None, "reject", format!("還不能 `done`：{e}。")).await?;
            return Ok(());
        }
    };
    let tasks = issue_tasks(app, &ctx.team.id, Some(&issue.id)).await?;
    let open: Vec<String> = tasks
        .iter()
        .filter(|t| !["merged", "skipped", "failed"].contains(&t.state.as_str()))
        .map(|t| match t.worker_bot_id {
            // §4.5: a task nobody has picked up yet is still work the PM cannot call done.
            None => format!("t{}（排隊中）", t.seq),
            Some(_) => format!("t{}（{}）", t.seq, t.state),
        })
        .collect();
    if !open.is_empty() {
        enqueue(
            app,
            &ctx.team.id,
            None,
            &pm.id,
            None,
            "reject",
            format!(
                "還不能 `done`（#{n}）：{} 尚未進入終態。請等回報或用 `wait`。",
                open.join("、"),
                n = issue.issue_number
            ),
        )
        .await?;
        return Ok(());
    }
    sqlx::query("UPDATE team_issues SET summary = ? WHERE id = ?")
        .bind(summary)
        .bind(&issue.id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    if ctx.unlimited() {
        // No `finishing` phase: that is a whole-team state, and the rest of the queue is still
        // working. Deliver this one issue here and now, then top the queue back up.
        return deliver_one_issue(app, ctx, &issue, summary).await;
    }
    sqlx::query("UPDATE teams SET summary = ? WHERE id = ?")
        .bind(summary)
        .bind(&ctx.team.id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    // The decision is read back by `close_issue_and_advance` once delivery is done; it lives
    // in the log (no new column) and only the most recent one for this issue counts.
    note(app, &ctx.team.id, json!({"action": "worker_plan", "keep": keep_workers})).await?;
    team::sched_phase(app, &ctx.team.id, "finishing", true).await?;
    Ok(())
}

/// What the PM asked for in its `done` for `issue_id`: `true` to keep this batch of workers
/// for the next issue. Absent (the PM never said, or the issue failed) means replace.
async fn keep_workers_for(app: &Arc<App>, team_id: &str, issue_id: &str) -> bool {
    sqlx::query_scalar::<_, String>(
        "SELECT payload_json FROM team_events WHERE team_id=? AND issue_id=? AND kind='note'
           AND json_extract(payload_json, '$.action') = 'worker_plan' ORDER BY seq DESC LIMIT 1",
    )
    .bind(team_id)
    .bind(issue_id)
    .fetch_optional(&app.db)
    .await
    .ok()
    .flatten()
    .and_then(|p| serde_json::from_str::<Value>(&p).ok())
    .and_then(|v| v["keep"].as_bool())
    .unwrap_or(false)
}

// ---------------------------------------------------------------- worker

async fn report(
    app: &Arc<App>,
    ctx: &Ctx,
    worker: &db::Bot,
    blocked: bool,
    summary: &str,
    notes: &str,
) -> LcResult<()> {
    let Some(task) = open_task_of(app, &ctx.team.id, &worker.id).await? else {
        return repair(app, ctx, worker, "你目前沒有進行中的 task").await;
    };
    // A report for a task that has already left the worker's hands (`reported` / `reviewing`
    // / `merging`) is a re-send — a late hook, a user nudge, a repair answered after the
    // first answer got through. Taking it again would drop the task back to `reported` and
    // send the reviewer the same round twice (2026-09-09 #48 t21). Keep the text, do nothing.
    if !blocked && !["working", "changes_requested", "rebasing", "blocked_by_worker", "queued"].contains(&task.state.as_str()) {
        note(app, &ctx.team.id, json!({"action": "duplicate_report", "bot": worker.name, "task": task.seq, "state": task.state})).await?;
        return Ok(());
    }
    let body = if summary.trim().is_empty() { notes.to_string() } else { summary.to_string() };
    sqlx::query("UPDATE team_tasks SET last_report = ?, updated_at = ? WHERE id = ?")
        .bind(&body)
        .bind(db::now())
        .bind(&task.id)
        .execute(&app.db)
        .await
        .map_err(up)?;

    if blocked {
        set_task_state(app, &task.id, "blocked_by_worker").await?;
        if let Some(pm) = ctx.pm() {
            let text = format!(
                "{tag}`{who}` t{seq}「{title}」→ `blocked`：{why}\n{table}\n請決定下一步（`dispatch` / `wait` / `done` / `ask_user`）。",
                tag = issue_tag(ctx, task.issue_id.as_deref()),
                who = ctx.short(worker),
                seq = task.seq,
                title = task.title,
                why = if notes.trim().is_empty() { body.clone() } else { notes.to_string() },
                table = task_table(app, &ctx.team.id).await,
            );
            enqueue(app, &ctx.team.id, Some(&worker.id), &pm.id, Some(&task.id), "report", text).await?;
        }
        return Ok(());
    }

    // Appendix C, report: nothing the worker wrote is allowed to be lost, and an empty task
    // branch is a mistake to correct rather than an empty merge to make.
    let wt = ctx.wt(worker)?;
    let msg = format!("wip({}): uncommitted at report", ctx.short(worker));
    match tg::commit_all(app, ctx.host(), &wt, &msg).await {
        Ok(true) => note(app, &ctx.team.id, json!({"action": "auto_commit", "bot": worker.name})).await?,
        Ok(false) => {}
        Err(e) => {
            note(app, &ctx.team.id, json!({"action": "auto_commit_failed", "error": e.to_string()})).await?;
        }
    }
    let integration = ctx.integration_of(&task);
    match tg::commits_ahead(app, ctx.host(), &wt, &integration).await {
        Ok(0) => {
            return repair(
                app,
                ctx,
                worker,
                &format!("你的分支 `{}` 相對 `{}` 沒有任何 commit", task.branch, integration),
            )
            .await
        }
        Ok(_) => {}
        Err(e) => {
            note(app, &ctx.team.id, json!({"action": "rev_list_failed", "error": e.to_string()})).await?;
        }
    }
    // §8.2: a rebase report goes straight back into the merge queue.
    let next = if task.state == "rebasing" { "merging" } else { "reported" };
    set_task_state(app, &task.id, next).await?;
    Ok(())
}

/// The little status table appendix A.4 puts under a batch of reports.
async fn task_table(app: &Arc<App>, team_id: &str) -> String {
    let Ok(ctx) = Ctx::load(app, team_id).await else { return String::new() };
    if ctx.unlimited() {
        return task_table_by_issue(app, &ctx).await;
    }
    // The PM only ever sees the issue it is working on (§2.3).
    let issue = db::current_team_issue(&app.db, team_id).await.ok().flatten();
    let tasks = issue_tasks(app, team_id, issue.as_ref().map(|i| i.id.as_str())).await.unwrap_or_default();
    if tasks.is_empty() {
        return String::new();
    }
    let rows: Vec<String> = tasks
        .iter()
        .map(|t| match t.worker_bot_id {
            None => format!("t{}「{}」= 排隊中", t.seq, t.title),
            Some(_) => format!("t{}「{}」= {}", t.seq, t.title, t.state),
        })
        .collect();
    // §4.5: the PM plans against the parallelism, so say what it currently is.
    let n = Ctx::load(app, team_id).await.map(|c| c.workers().len()).unwrap_or(0);
    let running = tasks
        .iter()
        .filter(|t| t.worker_bot_id.is_some() && !["merged", "skipped", "failed"].contains(&t.state.as_str()))
        .count();
    format!("目前 task 狀態（併行 {running}/{n}）：{}", rows.join("；"))
}

/// Appendix A.4 in unlimited mode: the same table, grouped by issue, because the PM is
/// managing several at once and a flat list of `t1…t9` tells it nothing about which is which.
async fn task_table_by_issue(app: &Arc<App>, ctx: &Ctx) -> String {
    let mut groups: Vec<String> = Vec::new();
    for q in &ctx.issues {
        let tasks = issue_tasks(app, &ctx.team.id, Some(&q.id)).await.unwrap_or_default();
        let rows: Vec<String> = tasks
            .iter()
            .map(|t| match t.worker_bot_id {
                None => format!("t{}「{}」= 排隊中", t.seq, t.title),
                Some(_) => format!("t{}「{}」= {}", t.seq, t.title, t.state),
            })
            .collect();
        let running = tasks
            .iter()
            .filter(|t| t.worker_bot_id.is_some() && !["merged", "skipped", "failed"].contains(&t.state.as_str()))
            .count();
        groups.push(format!(
            "#{n} · `{b}`（∞ · 執行者 {k} · 進行中 {running}）：{rows}",
            n = q.issue_number,
            b = q.branch.clone().unwrap_or_default(),
            k = ctx.workers_for(q).len(),
            rows = if rows.is_empty() { "（尚無 task）".into() } else { rows.join("；") },
        ));
    }
    if groups.is_empty() {
        return String::new();
    }
    format!("目前 task 狀態（按 issue 分組）：\n{}", groups.join("\n"))
}

// ---------------------------------------------------------------- reviewer

async fn verdict(
    app: &Arc<App>,
    ctx: &Ctx,
    rev: &db::Bot,
    approve: bool,
    summary: &str,
    must_fix: &[String],
) -> LcResult<()> {
    let tasks = sched_tasks(app, ctx).await?;
    let Some(task) = tasks.into_iter().find(|t| t.state == "reviewing") else {
        return repair(app, ctx, rev, "目前沒有待審查的 task").await;
    };
    sqlx::query("UPDATE team_tasks SET last_verdict = ?, updated_at = ? WHERE id = ?")
        .bind(summary)
        .bind(db::now())
        .bind(&task.id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    if approve {
        set_task_state(app, &task.id, "merging").await?;
        return Ok(());
    }
    // §4.5 review rounds: `round == max` and another `request_changes` ends the loop.
    if task.round >= ctx.budget.max_review_rounds {
        set_task_state(app, &task.id, "exhausted").await?;
        pause_issue(app, ctx, task.issue_id.as_deref(), "review_exhausted").await?;
        return Ok(());
    }
    let round = task.round + 1;
    sqlx::query("UPDATE team_tasks SET round = ?, state = 'changes_requested', updated_at = ? WHERE id = ?")
        .bind(round)
        .bind(db::now())
        .bind(&task.id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    emit_task(app, &task.id).await;
    let text = format!(
        "Reviewer 打回（第 {round} 回）：{summary}\n必修：{fix}\n在同一分支 `{br}` 繼續 commit，完成後再 `report`。",
        fix = if must_fix.is_empty() { "（未列出）".into() } else { must_fix.join("；") },
        br = task.branch,
    );
    enqueue(app, &ctx.team.id, Some(&rev.id), task_owner(&task)?, Some(&task.id), "rework", text).await?;
    Ok(())
}

// ---------------------------------------------------------------- the human's decisions (§10.5)

/// `POST /teams/:id/tasks/:tid/decide` with `rework`: the user gave a stuck task one more
/// round, so the worker has to be told. `force_merge` needs nothing here (the merge queue
/// picks the task up on the next pass) and `skip` is already terminal.
pub async fn relay_rework_decision(
    app: &Arc<App>,
    team: &db::Team,
    task: &db::TeamTask,
    note_text: Option<&str>,
) -> LcResult<()> {
    let text = format!(
        "使用者決定再給 t{seq}「{title}」一個回合{extra}。在同一分支 `{br}` 繼續，完成後再 `report`。",
        seq = task.seq,
        title = task.title,
        br = task.branch,
        extra = match note_text.map(str::trim).filter(|s| !s.is_empty()) {
            Some(n) => format!("：{n}"),
            None => String::new(),
        },
    );
    enqueue(app, &team.id, None, task_owner(task)?, Some(&task.id), "rework", text).await.map(|_| ())
}

// ---------------------------------------------------------------- deliver (§6.4, §8.1)

/// Phase `finishing`. `branch` writes a summary and stops; `pr` is the one path in the whole
/// feature that pushes, and it says so in the note it leaves behind.
async fn finish(app: &Arc<App>, ctx: &Ctx) -> LcResult<()> {
    if !gate_open(app, &ctx.team, "deliver").await {
        hold_gate(app, ctx, "deliver", json!({"deliver": ctx.team.deliver, "branch": ctx.team.branch})).await?;
        return Ok(());
    }
    let summary = ctx.team.summary.clone().unwrap_or_default();
    if !deliver_branch_or_pr(
        app,
        ctx,
        ctx.team.issue_number,
        &ctx.team.issue_title.clone(),
        &ctx.team.branch.clone(),
        ctx.issue_id().map(String::from).as_deref(),
        &summary,
    )
    .await?
    {
        return Ok(());
    }
    close_issue_and_advance(app, &ctx.team.id, "done", None, Some(summary.as_str())).await
}

/// §6.4: hand one issue over — a local branch, or a PR when the team was asked for one.
/// `false` = the team was paused (a failed `gh`) and the caller must stop.
///
/// Shared by `finish` (the whole team is finishing its only working issue) and by
/// `deliver_one_issue` (§4.5 unlimited: one issue of several is done and the rest run on),
/// which is why every per-issue value is a parameter rather than read off the `teams` mirror.
async fn deliver_branch_or_pr(
    app: &Arc<App>,
    ctx: &Ctx,
    issue_number: i64,
    issue_title: &str,
    branch: &str,
    issue_id: Option<&str>,
    summary: &str,
) -> LcResult<bool> {
    if ctx.team.deliver == "pr" {
        // A submodule team delivers to the submodule's repo, not the project's.
        let gh = if ctx.team.repo.is_empty() {
            crate::github::cached(app, &ctx.team.project_id).await
        } else {
            crate::github::list_submodules(app, &ctx.project, false)
                .await
                .ok()
                .and_then(|subs| subs.into_iter().find(|s| s.path == ctx.team.repo).and_then(|s| s.github))
        };
        match gh {
            None => {
                // §6.4: not a GitHub project (or `gh` unusable) → quietly become `branch`.
                note(app, &ctx.team.id, json!({"action": "deliver_downgraded", "to": "branch",
                     "why": "project has no GitHub origin"}))
                .await?;
            }
            Some(info) => {
                let base = tg::remote_base_branch(app, ctx.host(), &team::repo_path(&ctx.project, &ctx.team.repo), &ctx.team.base_ref).await;
                let title = format!("{issue_title} (#{issue_number})");
                let body = format!("{summary}\n\nCloses #{issue_number}");
                match tg::deliver_pr(
                    app,
                    ctx.host(),
                    &ctx.main_wt()?,
                    ctx.team.worktree_root.trim_end_matches('/'),
                    branch,
                    &info.slug(),
                    &base,
                    &title,
                    &body,
                )
                .await
                {
                    Ok(url) => {
                        sqlx::query("UPDATE teams SET pr_url = ? WHERE id = ?")
                            .bind(&url)
                            .bind(&ctx.team.id)
                            .execute(&app.db)
                            .await
                            .map_err(up)?;
                        if let Some(iid) = issue_id {
                            let _ = sqlx::query("UPDATE team_issues SET pr_url = ? WHERE id = ?")
                                .bind(&url)
                                .bind(iid)
                                .execute(&app.db)
                                .await;
                        }
                        note(app, &ctx.team.id, json!({"action": "pr_created", "url": url, "pushed": branch}))
                            .await?;
                    }
                    Err(e) => {
                        note(app, &ctx.team.id, json!({"action": "deliver_failed", "error": e.to_string()})).await?;
                        pause(app, &ctx.team, "deliver_failed").await?;
                        return Ok(false);
                    }
                }
            }
        }
    } else {
        // §6.4 `branch`: the work stays local. Nothing is pushed, by the user's ruling.
        note(app, &ctx.team.id, json!({"action": "delivered", "deliver": "branch",
             "branch": branch, "pushed": false, "summary": summary}))
        .await?;
    }
    Ok(true)
}

/// SPEC-team §2.3: settle the issue the team just finished with, then either hand it the next
/// one or end the team.
///
/// `state` is `done` when it was delivered and `failed` when the queue moved past it. Either
/// way this issue's workers are retired — the next issue gets a fresh set — while the PM and
/// the reviewer keep running so the team accumulates context across the whole queue.
async fn close_issue_and_advance(
    app: &Arc<App>,
    team_id: &str,
    state: &str,
    fail_reason: Option<&str>,
    summary: Option<&str>,
) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    if ctx.unlimited() {
        // §4.5: an unlimited team settles issues one at a time through `close_one_issue`;
        // this is the single-issue machine and must not advance a queue it does not own.
        let Some(current) = ctx.issue().cloned() else { return end_team(app, &ctx).await };
        return close_one_issue(app, team_id, &current, state, fail_reason, summary).await;
    }
    let Some(current) = ctx.issue().cloned() else {
        // No current issue at all: nothing to settle, so this is simply the end.
        return end_team(app, &ctx).await;
    };
    sqlx::query(
        "UPDATE team_issues SET state = ?, fail_reason = ?, summary = COALESCE(?, summary), ended_at = ?
         WHERE id = ?",
    )
    .bind(state)
    .bind(fail_reason)
    .bind(summary)
    .bind(db::now())
    .bind(&current.id)
    .execute(&app.db)
    .await
    .map_err(up)?;
    note(
        app,
        team_id,
        json!({"action": if state == "done" { "issue_finished" } else { "issue_failed" },
               "issue_number": current.issue_number, "seq": current.seq,
               "branch": current.branch, "reason": fail_reason}),
    )
    .await?;
    clear_rescue(app, team_id).await?;
    let next = db::next_queued_issue(&app.db, team_id).await.map_err(up)?;
    let Some(next) = next else {
        // Last issue: leave the worktrees exactly as `finish` always did. `done` stops the
        // members but keeps their trees for the user to look at; removing them is `cleanup`,
        // which stays a separate human decision (§12 #6).
        let ctx = Ctx::load(app, team_id).await?;
        return end_team(app, &ctx).await;
    };
    // Only now — with somewhere to go — are this issue's executors replaced. Unless the PM
    // asked to keep them (`done.workers = "keep"`): then they stay up, with their context, and
    // only their leftover tasks are closed out.
    let keep = state == "done" && keep_workers_for(app, team_id, &current.id).await;
    if keep {
        team::fail_open_tasks(app, team_id, &current.id).await;
    } else {
        team::retire_issue_workers(app, team_id, &current.id).await;
    }
    if let Err(e) = team::start_issue(app, team_id, &next, keep).await {
        // The team is healthy but the queue cannot move; that is a human problem, not a
        // reason to throw away the rest of the queue.
        note(app, team_id, json!({"action": "issue_start_failed", "issue_number": next.issue_number,
             "error": format!("{e:?}")}))
        .await?;
        let ctx = Ctx::load(app, team_id).await?;
        pause(app, &ctx.team, "upstream").await?;
        return Ok(());
    }
    // `start_issue` only *inserts* the new executors; `startup` — the one place members get
    // started — runs solely in the `starting` phase. Without this the PM's first dispatch on
    // the second issue hits a worker with no run and the team parks on `member_lost`.
    if !keep {
        start_issue_workers(app, team_id).await?;
    }
    hand_issue_to_pm(app, team_id, keep, None).await
}

// ---------------------------------------------------------------- unlimited parallelism (§4.5)

/// A `DECISION_PAUSES` reason that belongs to **one** issue.
///
/// With a finite parallelism this is [`pause`] and nothing more — the whole team stops (or,
/// with something left on the queue, moves past this issue), exactly as before. In unlimited
/// mode the other issues in flight are unaffected: this one is closed `failed`, keeping its
/// branch, and the queue is topped back up.
async fn pause_issue(app: &Arc<App>, ctx: &Ctx, issue_id: Option<&str>, reason: &str) -> LcResult<()> {
    if !ctx.unlimited() {
        return pause(app, &ctx.team, reason).await;
    }
    let Some(q) = issue_id.and_then(|i| ctx.issue_of(i)).cloned() else {
        return pause(app, &ctx.team, reason).await;
    };
    close_one_issue(app, &ctx.team.id, &q, "failed", Some(reason), None).await
}

/// Settle one row of the queue in unlimited mode: mark it, retire **its** executors, and let
/// whatever is still queued take the seat. The team ends only when nothing is working and
/// nothing is queued.
///
/// Deliberately not `close_issue_and_advance`: that one is the single-issue machine (settle,
/// then start exactly one successor, then re-hand the whole team to the PM) and it stays
/// untouched for teams with a finite parallelism.
async fn close_one_issue(
    app: &Arc<App>,
    team_id: &str,
    issue: &db::TeamIssue,
    state: &str,
    fail_reason: Option<&str>,
    summary: Option<&str>,
) -> LcResult<()> {
    sqlx::query(
        "UPDATE team_issues SET state = ?, fail_reason = ?, summary = COALESCE(?, summary), ended_at = ?
         WHERE id = ?",
    )
    .bind(state)
    .bind(fail_reason)
    .bind(summary)
    .bind(db::now())
    .bind(&issue.id)
    .execute(&app.db)
    .await
    .map_err(up)?;
    note(
        app,
        team_id,
        json!({"action": if state == "done" { "issue_finished" } else { "issue_failed" },
               "issue_number": issue.issue_number, "seq": issue.seq,
               "branch": issue.branch, "reason": fail_reason}),
    )
    .await?;
    clear_rescue(app, team_id).await?;
    team::retire_workers_of_issue(app, team_id, issue).await;
    team::sync_issue_mirror(app, team_id).await?;
    start_issues_up_to_capacity(app, team_id).await?;
    let ctx = Ctx::load(app, team_id).await?;
    if ctx.issues.is_empty() && db::next_queued_issue(&app.db, team_id).await.map_err(up)?.is_none() {
        return end_team(app, &ctx).await;
    }
    team::refresh_unlimited_docs(app, team_id).await
}

/// §4.5 unlimited: deliver one finished issue while the rest of the team carries on.
async fn deliver_one_issue(app: &Arc<App>, ctx: &Ctx, issue: &db::TeamIssue, summary: &str) -> LcResult<()> {
    if !gate_open(app, &ctx.team, "deliver").await {
        hold_gate(app, ctx, "deliver", json!({"deliver": ctx.team.deliver, "issue_number": issue.issue_number,
             "branch": issue.branch}))
        .await?;
        return Ok(());
    }
    let branch = issue.branch.clone().unwrap_or_else(|| ctx.team.branch.clone());
    if !deliver_branch_or_pr(
        app,
        ctx,
        issue.issue_number,
        &issue.issue_title,
        &branch,
        Some(issue.id.as_str()),
        summary,
    )
    .await?
    {
        return Ok(());
    }
    close_one_issue(app, &ctx.team.id, issue, "done", None, Some(summary)).await
}

/// §4.5 unlimited: start every queued issue there is room for, and tell the PM about them.
///
/// A no-op for a team with a finite parallelism — that queue is worked one entry at a time by
/// `close_issue_and_advance`, and this function must never touch it.
/// Returns how many issues it started.
pub async fn start_issues_up_to_capacity(app: &Arc<App>, team_id: &str) -> LcResult<usize> {
    let ctx = Ctx::load(app, team_id).await?;
    if !ctx.unlimited() || team::is_terminal(&ctx.team.phase) || ctx.team.phase == "aborting" {
        return Ok(0);
    }
    let mut open = ctx.issues.len();
    let mut started: Vec<db::TeamIssue> = Vec::new();
    while open < team::MAX_CONCURRENT_ISSUES {
        let Some(next) = db::next_queued_issue(&app.db, team_id).await.map_err(up)? else { break };
        if let Err(e) = team::start_issue(app, team_id, &next, false).await {
            note(app, team_id, json!({"action": "issue_start_failed", "issue_number": next.issue_number,
                 "error": format!("{e:?}")}))
            .await?;
            let ctx = Ctx::load(app, team_id).await?;
            pause(app, &ctx.team, "upstream").await?;
            return Ok(0);
        }
        open += 1;
        started.push(next);
    }
    if started.is_empty() {
        return Ok(0);
    }
    // `start_issue` only *inserts* each issue's first executor; `startup` runs solely in the
    // `starting` phase, so somebody has to actually launch them.
    start_issue_workers(app, team_id).await?;
    team::sync_issue_mirror(app, team_id).await?;
    team::refresh_unlimited_docs(app, team_id).await?;
    hand_issues_to_pm(app, team_id, &started).await?;
    Ok(started.len())
}

/// Appendix A.1 for unlimited mode: one relay telling the PM everything it now manages and
/// that `issue` is not optional any more.
async fn hand_issues_to_pm(app: &Arc<App>, team_id: &str, started: &[db::TeamIssue]) -> LcResult<()> {
    if team::load(app, team_id).await?.phase == "starting" {
        team::sched_phase(app, team_id, "planning", false).await?;
    }
    let ctx = Ctx::load(app, team_id).await?;
    let Some(pm) = ctx.pm() else { return Ok(()) };
    let rows: Vec<String> = ctx
        .issues
        .iter()
        .map(|q| {
            let who: Vec<String> = ctx.workers_for(q).iter().map(|b| ctx.short(b)).collect();
            format!(
                "- #{n}「{t}」：整合分支 `{b}`，全文 `.agents-manager/team/ISSUE-{n}.md`，執行者 {who}",
                n = q.issue_number,
                t = q.issue_title,
                b = q.branch.clone().unwrap_or_default(),
                who = if who.is_empty() { "（尚未建立）".into() } else { who.join("、") },
            )
        })
        .collect();
    let queued = db::team_issues(&app.db, team_id)
        .await
        .map_err(up)?
        .into_iter()
        .filter(|i| i.state == "queued")
        .count();
    let tail = if queued > 0 {
        format!("佇列裡還有 {queued} 個 issue，等這裡有 issue 結束就會自動開始。")
    } else {
        String::new()
    };
    let new_ones: Vec<String> = started.iter().map(|q| format!("#{}", q.issue_number)).collect();
    let new = if new_ones.is_empty() { "這是第一批".to_string() } else { format!("這一批新開的是 {}", new_ones.join("、")) };
    let text = format!(
        "併行數是**無限**：你同時在管 {n} 個 issue（{new}）。\n{rows}\n\n\
         因此**每一筆 `dispatch` 的 task 都要寫 `issue`（issue 號），`done` 也要寫 `issue`**——\
         `done` 只交付那一個 issue，其他的照常進行。`to` 一樣可以省略，daemon 會派給那個 issue 有空的執行者；\
         每個 issue 先給 1 個執行者，你派幾筆就開幾個（每個 issue 最多 4 個、全隊最多 12 個），\
         多出來的會排隊。請先讀 `.agents-manager/team/TEAM.md` 與各自的 `ISSUE-<n>.md` 再派工。{tail}",
        n = ctx.issues.len(),
        rows = rows.join("\n"),
    );
    enqueue(app, team_id, None, &pm.id, None, "first", text).await?;
    Ok(())
}

/// Start the executors `start_issue` just created (the PM and the reviewer are already up).
/// A failure is noted, not fatal here: the PM's dispatch to a worker without a run is what
/// turns it into `paused(member_lost)`, the same way it always has.
async fn start_issue_workers(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    let workers: Vec<db::Bot> = ctx.workers().into_iter().cloned().collect();
    for e in crate::trust::pretrust_members(app, &workers).await {
        tracing::warn!(team = %team_id, error = %e, "could not pre-trust a team worktree");
        note(app, team_id, json!({"action": "pretrust_failed", "error": e})).await?;
    }
    for b in &workers {
        if db::active_run(&app.db, &b.id).await.map_err(up)?.is_some() {
            continue;
        }
        match lifecycle::start_bot(app, &b.id).await {
            Ok(_) => {}
            Err(e) => {
                let msg = format!("{e:?}");
                note(app, team_id, json!({"action": "member_start_failed", "bot": b.name, "error": msg})).await?;
                if let Ok(conv) = db::conversation_id(&app.db, &b.id).await {
                    let _ = lifecycle::insert_message(
                        app,
                        &conv,
                        None,
                        "system",
                        &format!("這個成員沒能啟動：{msg}。修好原因後在這裡按「啟動」，再回 Team 面板按「繼續」。"),
                        "system",
                        false,
                        None,
                    )
                    .await;
                }
            }
        }
    }
    Ok(())
}

/// The queue is empty: stop every member and write the one terminal phase this module has.
async fn end_team(app: &Arc<App>, ctx: &Ctx) -> LcResult<()> {
    // §8.1: `done` stops the members; cleanup stays a separate, human decision (§12 #6).
    for b in &ctx.members {
        if let Err(e) = lifecycle::stop_bot(app, &b.id).await {
            tracing::warn!(bot = %b.name, error = ?e, "team finish: stop_bot failed");
        }
    }
    // Stopping every member is the slowest await in the scheduler — the likeliest place for an
    // abort to land in between (#31). A pause keeps `done` as its resume target instead.
    team::sched_phase(app, &ctx.team.id, "done", true).await?;
    Ok(())
}

/// Appendix A.1's first relay, re-sent for each issue in the queue. The PM is the same agent
/// across the whole queue, so this is the message that tells it the subject changed — and that
/// its executors are different people now.
async fn hand_issue_to_pm(
    app: &Arc<App>,
    team_id: &str,
    kept_workers: bool,
    context_lost: Option<bool>,
) -> LcResult<()> {
    // Paused mid-advance, the relay still goes out on "continue" (the outbox waits), and
    // `resume_phase` points at `planning` rather than the `finishing` that delivered the last
    // issue. Aborted: the team is over and the PM is told nothing.
    team::sched_phase(app, team_id, "planning", true).await?;
    let ctx = Ctx::load(app, team_id).await?;
    if team::is_terminal(&ctx.team.phase) || ctx.team.phase == "aborting" {
        return Ok(());
    }
    let Some(pm) = ctx.pm() else { return Ok(()) };
    let names: Vec<String> = ctx.workers().iter().map(|w| ctx.short(w)).collect();
    let queued = db::team_issues(&app.db, team_id)
        .await
        .map_err(up)?
        .into_iter()
        .filter(|i| i.state == "queued")
        .count();
    let tail = if queued > 0 { format!("這個 issue 完成後，佇列裡還有 {queued} 個。") } else { String::new() };
    let who_line = if kept_workers {
        format!("執行者照你的決定沿用上一個 issue 那批：{who}（併行數 {k}），他們記得先前的工作。", who = names.join("、"), k = names.len())
    } else {
        format!("執行者換了一批，現在是：{who}（併行數 {k}）。", who = names.join("、"), k = names.len())
    };
    let context_line = match context_lost {
        Some(true) => "你是重新啟動的 PM，先前的對話不在了；先讀 `.agents-manager/team/TEAM.md` 與 `ISSUE.md`。",
        Some(false) => "這是你原本那段對話的延續，先前 issue 的內容你都記得。",
        None => "",
    };
    let text = format!(
        "換下一個 issue：#{n}「{title}」。全文在 `.agents-manager/team/ISSUE.md`（已更新）。{who_line}\
         {context_line}請重新讀取 `.agents-manager/team/ISSUE.md` 與 `TEAM.md` 再派工，先前 issue 的 task 一律不要再提。{tail}",
        n = ctx.team.issue_number,
        title = ctx.team.issue_title,
    );
    enqueue(app, team_id, None, &pm.id, None, "next_issue", text).await?;
    Ok(())
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_last_am_team_block_counts() {
        let text = "先講一段\n\n```am-team\n{\"action\":\"wait\"}\n```\n\n改主意了\n\n```am-team\n{\"action\":\"done\",\"summary\":\"ok\"}\n```\n";
        let v = parse_block(text).unwrap();
        assert_eq!(v["action"], "done");
        assert_eq!(Action::parse("pm", &v).unwrap(), Action::Done { summary: "ok".into(), keep_workers: false, issue: None });
    }

    #[test]
    fn a_quoted_protocol_example_cannot_hijack_the_parse() {
        // A worker explaining the protocol in prose, then actually answering.
        let text = "我等一下會用 ```am-team``` 區塊回報。\n\n```am-team\n{\"action\":\"report\",\"status\":\"done\",\"summary\":\"加了測試\"}\n```";
        let a = Action::parse("worker", &parse_block(text).unwrap()).unwrap();
        assert_eq!(a, Action::Report { blocked: false, summary: "加了測試".into(), notes: String::new() });
    }

    #[test]
    fn parse_failures_say_what_is_wrong() {
        assert_eq!(parse_block("沒有區塊").unwrap_err(), "找不到 am-team 區塊");
        assert!(parse_block("```am-team\n{oops\n```").unwrap_err().contains("合法 JSON"));
        assert!(parse_block("```am-team\n[1,2]\n```").unwrap_err().contains("JSON 物件"));
        // An unterminated fence still parses — a terminal scrape is often truncated.
        assert_eq!(parse_block("```am-team\n{\"action\":\"wait\"}").unwrap()["action"], "wait");
    }

    /// Loop guard #1 (topology): the role table of §4.4 is enforced, not advisory.
    #[test]
    fn a_role_cannot_use_another_roles_action() {
        let disp = json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"t","brief":"b"}]});
        assert!(Action::parse("pm", &disp).is_ok());
        assert!(Action::parse("worker", &disp).unwrap_err().contains("report"));
        assert!(Action::parse("reviewer", &disp).unwrap_err().contains("verdict"));

        let rep = json!({"action":"report","status":"done","summary":"s"});
        assert!(Action::parse("worker", &rep).is_ok());
        assert!(Action::parse("pm", &rep).is_err());

        let ver = json!({"action":"verdict","result":"approve","summary":"s"});
        assert!(Action::parse("reviewer", &ver).is_ok());
        assert!(Action::parse("worker", &ver).is_err());

        // …and the field checks inside an otherwise-allowed action.
        assert!(Action::parse("worker", &json!({"action":"report","status":"maybe"})).is_err());
        // codex 2026-09-08 (#50): nested payload, its own vocabulary, no `summary`.
        let nested = json!({"action":"report","report":{"status":"completed","commits":["dafc2c4"],"not_done":["截圖"]}});
        match Action::parse("worker", &nested).unwrap() {
            Action::Report { blocked, summary, .. } => {
                assert!(!blocked);
                assert!(summary.contains("dafc2c4") && summary.contains("截圖"), "{summary}");
            }
            _ => panic!("not a report"),
        }
        match Action::parse("worker", &json!({"action":"report","status":"stuck"})).unwrap() {
            Action::Report { blocked, .. } => assert!(blocked),
            _ => panic!(),
        }
        assert!(super::fallback_caught_busy_screen("• Working (4s • esc to interrupt)\n› Ask Codex to do anything"));
        assert!(!super::fallback_caught_busy_screen("```am-team\n{\"action\":\"wait\"}\n```"));
        assert!(Action::parse("reviewer", &json!({"action":"verdict","result":"lgtm"})).is_err());
        assert!(Action::parse("pm", &json!({"action":"dispatch","tasks":[]})).is_err());
        assert!(Action::parse("pm", &json!({"action":"dispatch","tasks":[{"to":"dev-1"}]})).is_err());
        assert!(Action::parse("pm", &json!({"action":"ask_user"})).is_err());
        assert!(Action::parse("pm", &json!({})).unwrap_err().contains("缺少 action"));
    }

    #[test]
    fn dispatch_reads_every_field() {
        let v = json!({"action":"dispatch","tasks":[
            {"to":"@dev-1","title":"A","brief":"do A","files":["src/a.rs","src/b.rs"]},
            {"to":"dev-2","brief":"do B"}]});
        match Action::parse("pm", &v).unwrap() {
            Action::Dispatch(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].files, vec!["src/a.rs", "src/b.rs"]);
                assert_eq!(items[1].title, "task", "a missing title gets a placeholder");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn relay_bodies_are_capped() {
        let long = "字".repeat(RELAY_MAX + 500);
        let out = clip(&long);
        assert!(out.chars().count() < RELAY_MAX + 40);
        assert!(out.ends_with("完整內容見時間軸）"));
        assert_eq!(clip("短"), "短");
    }
}

/// The §13 acceptance scenarios, driven through the real state machine over a real git
/// repository. What is *not* here is anything that needs a live CLI: the tests below replace
/// "the agent wrote this" with `apply_reply`, which is exactly the text the hook path hands
/// the scheduler, and they replace "the pane exists" with a `runs` row.
#[cfg(test)]
mod scenarios {
    use super::*;
    use crate::team::testing::*;
    use crate::team_git::testing::run as git;

    struct S {
        e: Env,
        tid: String,
    }

    impl S {
        async fn new(workers: u32, reviewer: bool) -> S {
            S::with_issues(workers, reviewer, vec![issue()]).await
        }
        /// A team whose queue holds more than one issue (§2.3).
        async fn with_issues(workers: u32, reviewer: bool, issues: Vec<crate::team::IssueRef>) -> S {
            let e = env().await;
            let tid =
                make_team_with(&e.app, &e.project_id, req(Some(workers), reviewer), issues).await;
            // The scheduler only runs from `planning` onwards; `startup` needs live agents.
            crate::team::set_phase(&e.app, &tid, "planning", None, None).await.unwrap();
            sqlx::query("UPDATE teams SET started_at = ? WHERE id = ?")
                .bind(db::now())
                .bind(&tid)
                .execute(&e.app.db)
                .await
                .unwrap();
            S { e, tid }
        }
        fn app(&self) -> &Arc<App> {
            &self.e.app
        }
        async fn ctx(&self) -> Ctx {
            Ctx::load(self.app(), &self.tid).await.unwrap()
        }
        async fn bot(&self, role: &str, n: usize) -> db::Bot {
            let c = self.ctx().await;
            match role {
                "pm" => c.pm().unwrap().clone(),
                "reviewer" => c.reviewer().unwrap().clone(),
                _ => c.workers()[n].clone(),
            }
        }
        /// What the hook path would have delivered: prose plus the protocol block.
        async fn reply(&self, bot: &db::Bot, block: Value) {
            let text = format!("我做完了，細節如下。\n\n```am-team\n{block}\n```\n");
            apply_reply(self.app(), &self.tid, &bot.id, &text, "completed").await.unwrap();
        }
        async fn tasks(&self) -> Vec<db::TeamTask> {
            db::team_tasks(&self.app().db, &self.tid).await.unwrap()
        }
        async fn team(&self) -> db::Team {
            crate::team::load(self.app(), &self.tid).await.unwrap()
        }
        async fn pending(&self, bot: &db::Bot) -> Vec<db::TeamEvent> {
            pending_for(self.app(), &self.tid, &bot.id).await.unwrap()
        }
        /// Give every member a `runs` row so the outbox will actually try to deliver.
        async fn arm(&self) {
            for b in &self.ctx().await.members {
                fake_run(self.app(), &b.id).await;
            }
        }
        async fn unpause(&self) {
            let t = self.team().await;
            if t.phase == "paused" {
                crate::team::set_phase(self.app(), &self.tid, "working", None, None).await.unwrap();
            }
        }
        /// A member's worktree, addressed by its protocol short name (`main`, `reviewer`,
        /// `dev-1`). Worker directories carry the issue's queue position (§2.3) and every
        /// scenario here works the first issue, so `dev-N` maps to `i1-dev-N`.
        fn wt(&self, name: &str) -> std::path::PathBuf {
            let dir =
                if name.starts_with("dev-") { format!("i1-{name}") } else { name.to_string() };
            std::path::PathBuf::from(&self.e.dir).join("data/teams").join(&self.tid).join(dir)
        }
    }

/// Complete the PM turn created by `POST /teams/:id/answer` as the hook path would.
async fn complete_answer_turn(s: &S, response: &str) -> String {
    let pm = s.bot("pm", 0).await;
    let out = crate::team::answer(s.app(), &s.tid, "使用者的回答", "answer-test")
        .await
        .unwrap();
    let turn_id = out["turn_id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE turns SET status='completed', delivery='ok', completed_at=? WHERE id=?")
        .bind(db::now())
        .bind(&turn_id)
        .execute(&s.app().db)
        .await
        .unwrap();
    let conv = db::conversation_id(&s.app().db, &pm.id).await.unwrap();
    crate::lifecycle::insert_message(s.app(), &conv, Some(&turn_id), "assistant", response, "hook", false, None)
        .await
        .unwrap();
    on_turn_done(s.app(), &s.tid, &pm.id, &turn_id, "completed").await.unwrap();
    turn_id
}

    /// An answer keeps the user's timeline text unchanged, but the delivered PM prompt must
    /// restate the protocol so the PM's next reply can be applied instead of leaving planning.
    #[tokio::test]
    async fn answer_applies_pm_action_and_advances_phase() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"ask_user","question":"要不要繼續？"})).await;
        assert_eq!(s.team().await.pause_reason.as_deref(), Some("ask_user"));
        s.arm().await;

        let out = crate::team::answer(s.app(), &s.tid, "繼續", "answer-footer-test").await.unwrap();
        let delivered = s
            .e
            .herdr
            .first_call("agent.prompt")
            .and_then(|p| p["text"].as_str().map(String::from))
            .unwrap();
        assert!(delivered.contains("```am-team"));
        assert!(delivered.contains("dispatch, wait, done, ask_user, abort"));

        let turn_id = out["turn_id"].as_str().unwrap();
        sqlx::query("UPDATE turns SET status='completed', delivery='ok', completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(turn_id)
            .execute(&s.app().db)
            .await
            .unwrap();
        let conv = db::conversation_id(&s.app().db, &pm.id).await.unwrap();
        crate::lifecycle::insert_message(
            s.app(),
            &conv,
            Some(turn_id),
            "assistant",
            "收到，我來派工。\n\n```am-team\n{\"action\":\"dispatch\",\"tasks\":[{\"to\":\"dev-1\",\"brief\":\"完成工作\"}]}\n```",
            "hook",
            false,
            None,
        )
        .await
        .unwrap();
        let worker = s.bot("worker", 0).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE bot_id=? AND state='running'")
            .bind(&worker.id)
            .execute(&s.app().db)
            .await
            .unwrap();
        // Keep the worker's delivery slot occupied so this test observes the PM transition
        // without making the mock herdr invent a completed worker turn.
        let worker_run = db::active_run(&s.app().db, &worker.id).await.unwrap().unwrap();
        let worker_conv = db::conversation_id(&s.app().db, &worker.id).await.unwrap();
        sqlx::query(
            "INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind("answer-worker-busy")
        .bind(&worker_conv)
        .bind(&worker_run.id)
        .bind(db::now())
        .execute(&s.app().db)
        .await
        .unwrap();

        on_turn_done(s.app(), &s.tid, &pm.id, turn_id, "completed").await.unwrap();

        assert_eq!(s.tasks().await.len(), 1, "the PM action was applied");
        let team = s.team().await;
        assert_eq!(team.phase, "working", "planning advanced after answer: {team:?}");
    }

    /// A PM response without a block remains ordinary user conversation: no repair is queued
    /// and the resumed team stays in planning until the PM sends a protocol response.
    #[tokio::test]
    async fn answer_without_pm_block_keeps_existing_behavior() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"ask_user","question":"需要更多資訊"})).await;
        s.arm().await;
        complete_answer_turn(&s, "我先想一下，稍後回覆。").await;

        assert_eq!(s.team().await.phase, "planning");
        assert!(s.tasks().await.is_empty());
        let repairs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM team_events WHERE team_id=? AND kind='note' AND json_extract(payload_json,'$.action')='protocol_error'",
        )
        .bind(&s.tid)
        .fetch_one(&s.app().db)
        .await
        .unwrap();
        assert_eq!(repairs, 0);
    }

    fn dispatch2() -> Value {
        json!({"action":"dispatch","tasks":[
            {"to":"dev-1","title":"A","brief":"做 A","files":["a.txt"]},
            {"to":"dev-2","title":"B","brief":"做 B","files":["b.txt"]}]})
    }

    fn issue3() -> crate::team::IssueRef {
        crate::team::IssueRef {
            number: 44,
            title: "and one more".into(),
            url: "https://example.invalid/44".into(),
            body: "再來一個。".into(),
        }
    }

    /// Make the current queue row look like the completed issue that a real `finish` pass
    /// leaves behind. Keeping this small DB helper in the scenario module lets the reopen
    /// tests focus on the boundary without pretending an agent produced a protocol reply.
    async fn settle_current_issue(s: &S) -> db::TeamIssue {
        let current = db::current_team_issue(&s.app().db, &s.tid).await.unwrap().unwrap();
        let at = db::now();
        sqlx::query("UPDATE team_issues SET state='done', summary='完成這個 issue', ended_at=? WHERE id=?")
            .bind(&at)
            .bind(&current.id)
            .execute(&s.app().db)
            .await
            .unwrap();
        crate::team::set_phase(s.app(), &s.tid, "done", None, None).await.unwrap();
        db::team_issue(&s.app().db, &current.id).await.unwrap().unwrap()
    }

    async fn enter_reopen(s: &S) {
        settle_current_issue(s).await;
        crate::team::set_phase(s.app(), &s.tid, "starting", Some("reopen"), None).await.unwrap();
    }

    /// §2.5.3 / §2.5.4: the old workers are retired, while the long-lived PM/reviewer and
    /// integration worktree stay put; the new issue gets a fresh per-issue worker batch and
    /// its own integration branch cut from the team's base.
    #[tokio::test]
    async fn reopen_retires_last_workers_and_builds_a_new_batch() {
        let s = S::with_issues(1, true, vec![issue(), issue2()]).await;
        let before = s.ctx().await;
        let pm = before.pm().unwrap().clone();
        let reviewer = before.reviewer().unwrap().clone();
        let old_worker = before.workers()[0].clone();
        let old_worker_cwd = old_worker.cwd.clone().unwrap();
        // #61: the retired worker's hook material must go with it.
        let old_worker_dir = s.app().bot_dir(&old_worker.id);
        std::fs::create_dir_all(old_worker_dir.join("bin")).unwrap();
        std::fs::write(old_worker_dir.join("bin").join("herdr"), "shim").unwrap();
        let pm_cwd = pm.cwd.clone().unwrap();
        let reviewer_cwd = reviewer.cwd.clone().unwrap();
        let base_sha = before.team.base_sha.clone();

        // §2.5.6 #8: a spec changed while the team was still running is what the new batch is
        // built from — `start_issue` reads `roles_json.workers.spec`, not the retired bots.
        // (`patch` itself refuses a terminal team, so this has to happen before `done`.)
        crate::team::patch(
            s.app(),
            &s.tid,
            crate::team::PatchTeam {
                workers: Some(crate::team::RolePatch { model: Some(Some("sonnet".into())), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        enter_reopen(&s).await;
        crate::team::retire_issue_workers(s.app(), &s.tid, &before.issue().unwrap().id).await;
        let next = db::next_queued_issue(&s.app().db, &s.tid).await.unwrap().unwrap();
        crate::team::start_issue(s.app(), &s.tid, &next, false).await.unwrap();

        let after = s.ctx().await;
        assert_eq!(after.pm().unwrap().id, pm.id);
        assert_eq!(after.pm().unwrap().cwd.as_deref(), Some(pm_cwd.as_str()));
        assert_eq!(after.reviewer().unwrap().id, reviewer.id);
        assert_eq!(after.reviewer().unwrap().cwd.as_deref(), Some(reviewer_cwd.as_str()));
        assert!(db::bot(&s.app().db, &old_worker.id).await.unwrap().unwrap().deleted_at.is_some());
        assert!(!std::path::Path::new(&old_worker_cwd).exists(), "the previous worker worktree is retired");
        assert!(!old_worker_dir.exists(), "#61: retire_workers left the retired worker's bots/<id>/ directory behind");

        let workers = after.workers();
        assert_eq!(workers.len(), 1);
        assert_ne!(workers[0].id, old_worker.id);
        assert_eq!(workers[0].kind, "claude");
        assert_eq!(workers[0].model.as_deref(), Some("sonnet"), "the new batch uses the patched spec");
        assert!(workers[0].cwd.as_deref().unwrap().ends_with("/i2-dev-1"));
        let t = after.team;
        assert_eq!(t.branch, crate::team::integration_branch(43, &crate::team::tid6(&s.tid)));
        assert_eq!(git(&s.wt("main"), &["rev-parse", "--abbrev-ref", "HEAD"]), t.branch);
        assert_eq!(git(&s.wt("main"), &["rev-parse", "HEAD"]), base_sha);
    }

    /// §2.5.3: a PM/reviewer start failure pauses the reopened Team in place. It must not
    /// turn the Team into `failed` or invoke cleanup, because the user can repair and resume
    /// the same member.
    #[tokio::test]
    async fn reopen_pauses_on_member_failed_instead_of_cleanup() {
        let s = S::with_issues(1, false, vec![issue(), issue2()]).await;
        let pm = s.bot("pm", 0).await;
        let root = s.team().await.worktree_root;
        enter_reopen(&s).await;
        s.app().connected.store(false, std::sync::atomic::Ordering::SeqCst);

        reopen_startup(s.app(), &s.tid).await.unwrap();

        let t = s.team().await;
        assert_eq!(t.phase, "paused");
        assert_eq!(t.pause_reason.as_deref(), Some("member_failed:pm"));
        assert!(t.ended_at.is_none());
        assert!(std::path::Path::new(&root).exists(), "the team root was not cleaned up");
        assert!(db::bot(&s.app().db, &pm.id).await.unwrap().unwrap().deleted_at.is_none());
        assert!(db::next_queued_issue(&s.app().db, &s.tid).await.unwrap().is_some());
    }

    /// A startup that loses every executor is terminal. The members that did start must be
    /// stopped, while the member_start_failed notes remain the source of the diagnosis.
    #[tokio::test]
    async fn startup_all_workers_failed_stops_started_members_and_keeps_reason() {
        let e = env().await;
        let tid = make_team(&e.app, &e.project_id, req(Some(2), true)).await;
        let s = S { e, tid };
        let members = s.ctx().await.members;
        let workers: Vec<db::Bot> = members
            .iter()
            .filter(|b| b.team_role.as_deref() == Some("worker"))
            .cloned()
            .collect();
        let bad_cwd = s.e.repo.to_string_lossy().to_string();
        for worker in &workers {
            sqlx::query("UPDATE bots SET cwd=? WHERE id=?")
                .bind(&bad_cwd)
                .bind(&worker.id)
                .execute(&s.app().db)
                .await
                .unwrap();
        }

        startup(s.app(), &s.tid).await.unwrap();

        assert_eq!(s.team().await.phase, "failed");
        let pm = members.iter().find(|b| b.team_role.as_deref() == Some("pm")).unwrap();
        let reviewer = members.iter().find(|b| b.team_role.as_deref() == Some("reviewer")).unwrap();
        for member in [pm, reviewer] {
            let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE bot_id=? ORDER BY started_at DESC LIMIT 1")
                .bind(&member.id)
                .fetch_one(&s.app().db)
                .await
                .unwrap();
            assert_eq!(state, "stopped", "{} was not stopped", member.name);
        }
        let notes: Vec<String> = sqlx::query_scalar(
            "SELECT payload_json FROM team_events WHERE team_id=? AND kind='note' ORDER BY seq",
        )
        .bind(&s.tid)
        .fetch_all(&s.app().db)
        .await
        .unwrap();
        let failures: Vec<Value> = notes
            .iter()
            .filter_map(|n| serde_json::from_str(n).ok())
            .filter(|p: &Value| p["action"] == "member_start_failed")
            .collect();
        assert_eq!(failures.len(), workers.len());
        assert!(failures.iter().all(|p| p["error"].as_str().unwrap_or_default().contains("inside the project checkout")));
        assert!(s.e.herdr.methods().iter().any(|m| m == "agent.send_keys"));
    }

    /// One failed executor is survivable: startup continues with the healthy worker and does
    /// not stop the PM or reviewer that were successfully started.
    #[tokio::test]
    async fn startup_partial_worker_failure_keeps_started_members_running() {
        let e = env().await;
        let tid = make_team(&e.app, &e.project_id, req(Some(2), true)).await;
        let s = S { e, tid };
        let members = s.ctx().await.members;
        let worker = members
            .iter()
            .find(|b| b.team_role.as_deref() == Some("worker"))
            .unwrap();
        sqlx::query("UPDATE bots SET cwd=? WHERE id=?")
            .bind(s.e.repo.to_string_lossy().to_string())
            .bind(&worker.id)
            .execute(&s.app().db)
            .await
            .unwrap();

        startup(s.app(), &s.tid).await.unwrap();

        assert_eq!(s.team().await.phase, "planning");
        for member in members.iter().filter(|b| matches!(b.team_role.as_deref(), Some("pm") | Some("reviewer"))) {
            let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE bot_id=? ORDER BY started_at DESC LIMIT 1")
                .bind(&member.id)
                .fetch_one(&s.app().db)
                .await
                .unwrap();
            assert_eq!(state, "running", "{} was stopped after a partial worker failure", member.name);
        }
        assert!(!s.e.herdr.methods().iter().any(|m| m == "agent.send_keys"));
    }

    /// §2.5.5: the hand-over relay explicitly distinguishes a successful native continuation
    /// from a fresh PM conversation, while both variants still target the next issue.
    #[tokio::test]
    async fn reopen_hands_next_issue_to_pm() {
        let s = S::with_issues(1, false, vec![issue(), issue2()]).await;
        let first = settle_current_issue(&s).await;
        crate::team::retire_issue_workers(s.app(), &s.tid, &first.id).await;
        crate::team::set_phase(s.app(), &s.tid, "starting", Some("reopen"), None).await.unwrap();
        let next = db::next_queued_issue(&s.app().db, &s.tid).await.unwrap().unwrap();
        crate::team::start_issue(s.app(), &s.tid, &next, false).await.unwrap();

        hand_issue_to_pm(s.app(), &s.tid, false, Some(false)).await.unwrap();
        let relays: Vec<(i64, String, Option<String>)> = sqlx::query_as(
            "SELECT seq, payload_json, to_bot_id FROM team_events WHERE team_id=? AND kind='relay' ORDER BY seq",
        )
        .bind(&s.tid)
        .fetch_all(&s.app().db)
        .await
        .unwrap();
        let first_payload: Value = serde_json::from_str(&relays.last().unwrap().1).unwrap();
        assert_eq!(relays.last().unwrap().2.as_deref(), Some(s.bot("pm", 0).await.id.as_str()));
        assert_eq!(first_payload["action"], "next_issue");
        assert!(first_payload["text"].as_str().unwrap().contains("這是你原本那段對話的延續，先前 issue 的內容你都記得"));

        hand_issue_to_pm(s.app(), &s.tid, false, Some(true)).await.unwrap();
        let relays: Vec<String> = sqlx::query_scalar(
            "SELECT payload_json FROM team_events WHERE team_id=? AND kind='relay' ORDER BY seq",
        )
        .bind(&s.tid)
        .fetch_all(&s.app().db)
        .await
        .unwrap();
        let second: Value = serde_json::from_str(relays.last().unwrap()).unwrap();
        assert!(second["text"].as_str().unwrap().contains("你是重新啟動的 PM，先前的對話不在了；先讀"));
        assert_eq!(s.team().await.phase, "planning");
    }

    /// §2.5.6: after one continuation reaches `done`, the same Team can be reopened again and
    /// its next queued issue becomes the new current row.
    #[tokio::test]
    async fn reopen_twice() {
        let s = S::with_issues(1, false, vec![issue(), issue2(), issue3()]).await;
        let pm_id = s.bot("pm", 0).await.id;

        // Entered through `startup`, which is the only caller in production: `starting` with no
        // working issue but a queued one is what tells it this is a reopen (§2.5.2).
        enter_reopen(&s).await;
        startup(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.team().await.phase, "planning");

        // §2.5.6 #9: a daemon restart inside the reopen must not build a second worker batch.
        // `start_issue` already made the queue row `working`, so `startup` takes the ordinary
        // road and only re-checks members.
        let workers_before: Vec<String> = s.ctx().await.workers().iter().map(|b| b.id.clone()).collect();
        crate::team::set_phase(s.app(), &s.tid, "starting", None, None).await.unwrap();
        startup(s.app(), &s.tid).await.unwrap();
        let workers_after: Vec<String> = s.ctx().await.workers().iter().map(|b| b.id.clone()).collect();
        assert_eq!(workers_after, workers_before);

        crate::lifecycle::stop_bot(s.app(), &pm_id).await.unwrap();

        settle_current_issue(&s).await;
        crate::team::set_phase(s.app(), &s.tid, "starting", Some("reopen"), None).await.unwrap();
        reopen_startup(s.app(), &s.tid).await.unwrap();

        let current = db::current_team_issue(&s.app().db, &s.tid).await.unwrap().unwrap();
        assert_eq!(current.issue_number, 44);
        assert_eq!(s.team().await.phase, "planning");
        assert_eq!(db::team_issues(&s.app().db, &s.tid).await.unwrap().iter().filter(|i| i.state == "done").count(), 2);
    }

    // ------------------------------------------------------------ §4.5 unlimited parallelism

    /// A team in unlimited mode with its whole queue open, the way `startup` leaves it.
    async fn unlimited(issues: Vec<crate::team::IssueRef>) -> S {
        let s = S::with_issues(0, true, issues).await;
        start_issues_up_to_capacity(s.app(), &s.tid).await.unwrap();
        s
    }

    /// The working issue with that number, and its executors' short names.
    async fn issue_of(s: &S, number: i64) -> db::TeamIssue {
        s.ctx().await.issues.into_iter().find(|i| i.issue_number == number).expect("issue is working")
    }

    /// Three issues on the queue, `workers.count = 0`: all three are worked at once, each on
    /// its own integration branch with its own single executor.
    #[tokio::test]
    async fn unlimited_works_every_queued_issue_at_once() {
        let s = unlimited(vec![issue(), issue2(), issue3()]).await;
        let ctx = s.ctx().await;
        assert_eq!(ctx.issues.len(), 3, "every queued issue is working");
        assert!(ctx.unlimited());

        let mut branches: Vec<String> = Vec::new();
        for q in &ctx.issues {
            assert_eq!(ctx.workers_for(q).len(), 1, "#{} starts with one executor", q.issue_number);
            branches.push(q.branch.clone().expect("a working issue has its integration branch"));
        }
        branches.sort();
        branches.dedup();
        assert_eq!(branches.len(), 3, "one integration branch per issue");
        // Six executors would be four issues too many: the whole team is three workers + PM + reviewer.
        assert_eq!(ctx.workers().len(), 3);
        // The PM is told, once, that it manages all three and that `issue` is mandatory.
        let p = s.pending(&s.bot("pm", 0).await).await;
        let text = p.iter().map(|e| serde_json::from_str::<Value>(&e.payload_json).unwrap()["text"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join("\n");
        for n in ["#42", "#43", "#44"] {
            assert!(text.contains(n), "the first relay lists {n}: {text}");
        }
        assert!(text.contains("`issue`"));
    }

    /// Three tasks on one issue: that issue grows from one executor to three, and nothing is
    /// added to the issues nobody dispatched to.
    #[tokio::test]
    async fn unlimited_builds_executors_on_demand_per_issue() {
        let s = unlimited(vec![issue(), issue2()]).await;
        let pm = s.bot("pm", 0).await;
        s.reply(
            &pm,
            json!({"action":"dispatch","tasks":[
                {"issue":42,"title":"A","brief":"做 A"},
                {"issue":42,"title":"B","brief":"做 B"},
                {"issue":42,"title":"C","brief":"做 C"}]}),
        )
        .await;

        let ctx = s.ctx().await;
        let a = issue_of(&s, 42).await;
        let b = issue_of(&s, 43).await;
        assert_eq!(ctx.workers_for(&a).len(), 3, "one executor per dispatched task, up to the cap");
        assert_eq!(ctx.workers_for(&b).len(), 1, "the issue nobody dispatched to is untouched");
        // …and all three tasks are running, none of them queued behind a parallelism.
        let tasks = s.tasks().await;
        assert_eq!(tasks.len(), 3);
        assert!(tasks.iter().all(|t| t.worker_bot_id.is_some()), "{tasks:?}");
        assert!(tasks.iter().all(|t| t.issue_id.as_deref() == Some(a.id.as_str())));
    }

    /// The whole-team ceiling: six issues each asking for four executors cannot make 24.
    #[tokio::test]
    async fn unlimited_stops_at_twelve_executors_for_the_whole_team() {
        let s = unlimited(vec![issue(), issue2(), issue3()]).await;
        let pm = s.bot("pm", 0).await;
        for n in [42i64, 43, 44] {
            s.reply(
                &pm,
                json!({"action":"dispatch","tasks":[
                    {"issue":n,"title":"a","brief":format!("{n} 做 a")},
                    {"issue":n,"title":"b","brief":format!("{n} 做 b")},
                    {"issue":n,"title":"c","brief":format!("{n} 做 c")},
                    {"issue":n,"title":"d","brief":format!("{n} 做 d")}]}),
            )
            .await;
        }
        let ctx = s.ctx().await;
        assert_eq!(ctx.workers().len(), 12, "MAX_TEAM_WORKERS");
        for q in &ctx.issues {
            assert_eq!(ctx.workers_for(q).len(), 4, "MAX_WORKERS per issue");
        }
    }

    /// The trap this feature is most likely to fall into: a task must be cut from **its own**
    /// issue's integration branch, not from the `teams` mirror.
    #[tokio::test]
    async fn a_task_branches_off_its_own_issue_not_the_mirror() {
        let s = unlimited(vec![issue(), issue2()]).await;
        let b = issue_of(&s, 43).await;
        // Put a commit on #43's integration branch only. The mirror still points at #42.
        let main = s.wt("main");
        git(&main, &["checkout", "-q", b.branch.as_deref().unwrap()]);
        std::fs::write(main.join("only-in-43.txt"), "43\n").unwrap();
        git(&main, &["add", "-A"]);
        git(&main, &["commit", "-q", "-m", "only in #43"]);
        git(&main, &["checkout", "-q", &s.team().await.branch]);

        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"issue":42,"title":"A","brief":"做 A"}]})).await;

        let a = issue_of(&s, 42).await;
        let task = s.tasks().await.into_iter().next().unwrap();
        assert_eq!(task.issue_id.as_deref(), Some(a.id.as_str()));
        assert!(task.branch.starts_with(a.branch.as_deref().unwrap()), "{}", task.branch);
        let wt = std::path::PathBuf::from(&s.e.dir).join("data/teams").join(&s.tid).join("i1-dev-1");
        assert_eq!(git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]), task.branch);
        assert!(!wt.join("only-in-43.txt").exists(), "#42's task must not see #43's work");

        // And #43's own task does branch off #43.
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"issue":43,"title":"B","brief":"做 B"}]})).await;
        let wt43 = std::path::PathBuf::from(&s.e.dir).join("data/teams").join(&s.tid).join("i2-dev-1");
        assert!(wt43.join("only-in-43.txt").exists(), "#43's task starts from #43");
    }

    /// A dispatch that does not say which issue is refused while several are working, and the
    /// PM is told why rather than having a task land on the wrong branch.
    #[tokio::test]
    async fn a_dispatch_without_an_issue_is_refused_while_several_run() {
        let s = unlimited(vec![issue(), issue2()]).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"title":"A","brief":"做 A"}]})).await;
        assert!(s.tasks().await.is_empty(), "nothing is created");
        let text = s
            .pending(&pm)
            .await
            .iter()
            .map(|e| serde_json::from_str::<Value>(&e.payload_json).unwrap()["text"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("`issue`"), "{text}");

        // An issue that is not working is refused the same way.
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"issue":999,"title":"A","brief":"做 A"}]})).await;
        assert!(s.tasks().await.is_empty());
    }

    /// `done` settles one issue and one only: #42 is delivered and closed while #43 keeps its
    /// executor, its branch and its `working` row.
    #[tokio::test]
    async fn done_delivers_one_issue_and_leaves_the_others_running() {
        let s = unlimited(vec![issue(), issue2()]).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[
            {"issue":42,"title":"A","brief":"做 A"},
            {"issue":43,"title":"B","brief":"做 B"}]}))
        .await;

        // #42's executor does the work and it is reviewed and merged.
        let a = issue_of(&s, 42).await;
        let dev = s.ctx().await.workers_for(&a)[0].clone();
        let wt = std::path::PathBuf::from(&dev.cwd.clone().unwrap());
        std::fs::write(wt.join("a.txt"), "a\n").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-q", "-m", "a"]);
        s.reply(&dev, json!({"action":"report","status":"done","summary":"做完了"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        let rev = s.bot("reviewer", 0).await;
        s.reply(&rev, json!({"action":"verdict","result":"approve","summary":"ok"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.tasks().await.iter().find(|t| t.title == "A").unwrap().state, "merged");

        // `done` for #42 only.
        s.reply(&pm, json!({"action":"done","issue":42,"summary":"#42 完成"})).await;

        let issues = db::team_issues(&s.app().db, &s.tid).await.unwrap();
        let a = issues.iter().find(|i| i.issue_number == 42).unwrap();
        let b = issues.iter().find(|i| i.issue_number == 43).unwrap();
        assert_eq!(a.state, "done");
        assert_eq!(a.summary.as_deref(), Some("#42 完成"));
        assert_eq!(b.state, "working", "the other issue is untouched");
        assert_ne!(s.team().await.phase, "done", "the team is not finished");
        // #43 still has its executor and its open task.
        let ctx = s.ctx().await;
        assert_eq!(ctx.issues.len(), 1);
        assert_eq!(ctx.workers_for(&issue_of(&s, 43).await).len(), 1);
        let b_task = s.tasks().await.into_iter().find(|t| t.title == "B").unwrap();
        assert!(b_task.worker_bot_id.is_some(), "#43's task still has its executor");
        assert_ne!(b_task.state, "failed", "closing #42 must not fail #43's task");
    }

    /// `done` for an issue whose tasks are still open is refused, and the refusal names the
    /// issue — the PM has several and needs to know which one it got wrong.
    #[tokio::test]
    async fn done_is_refused_per_issue() {
        let s = unlimited(vec![issue(), issue2()]).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"issue":42,"title":"A","brief":"做 A"}]})).await;
        s.reply(&pm, json!({"action":"done","issue":42,"summary":"x"})).await;
        let text = s
            .pending(&pm)
            .await
            .iter()
            .map(|e| serde_json::from_str::<Value>(&e.payload_json).unwrap()["text"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("#42"), "{text}");
        assert_eq!(issue_of(&s, 42).await.state, "working");
    }

    /// A finite parallelism never reaches any of the above: the mode is off, one issue at a
    /// time, and the `issue` field is simply ignored.
    #[tokio::test]
    async fn a_finite_parallelism_is_still_one_issue_at_a_time() {
        let s = S::with_issues(2, true, vec![issue(), issue2()]).await;
        assert!(!s.ctx().await.unlimited());
        assert_eq!(s.ctx().await.issues.len(), 1);
        assert_eq!(start_issues_up_to_capacity(s.app(), &s.tid).await.unwrap(), 0);
        assert_eq!(s.ctx().await.issues.len(), 1, "the queue is still worked one at a time");
    }

    /// **T2** — the PM dispatches two tasks and each lands on the right worker, with its own
    /// branch cut from the integration branch and a relay addressed to it.
    #[tokio::test]
    async fn t2_dispatch_reaches_the_right_workers() {
        let s = S::new(2, true).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, dispatch2()).await;

        let tasks = s.tasks().await;
        assert_eq!(tasks.len(), 2);
        let (d1, d2) = (s.bot("worker", 0).await, s.bot("worker", 1).await);
        assert_eq!(tasks[0].worker_bot_id.as_deref(), Some(d1.id.as_str()));
        assert_eq!(tasks[1].worker_bot_id.as_deref(), Some(d2.id.as_str()));
        assert_eq!(tasks[0].branch, "team/i42-".to_string() + &crate::team::tid6(&s.tid) + "-t1");
        assert_eq!(tasks[0].state, "queued");
        assert_eq!(s.team().await.phase, "working", "the first dispatch moves the team to `working`");

        // The branches exist and are checked out in the right worktrees.
        assert_eq!(git(&s.wt("dev-1"), &["rev-parse", "--abbrev-ref", "HEAD"]), tasks[0].branch);
        assert_eq!(git(&s.wt("dev-2"), &["rev-parse", "--abbrev-ref", "HEAD"]), tasks[1].branch);

        // One pending relay each, carrying the task and the brief.
        for (b, t) in [(&d1, &tasks[0]), (&d2, &tasks[1])] {
            let p = s.pending(b).await;
            assert_eq!(p.len(), 1);
            assert_eq!(p[0].task_id.as_deref(), Some(t.id.as_str()));
            assert_eq!(p[0].from_bot_id.as_deref(), Some(pm.id.as_str()), "the relay says it came from the PM");
            let payload: Value = serde_json::from_str(&p[0].payload_json).unwrap();
            assert_eq!(payload["action"], "dispatch");
            assert!(payload["text"].as_str().unwrap().contains(&t.brief));
        }
        assert!(s.pending(&s.bot("reviewer", 0).await).await.is_empty());
    }

    /// **T2 continued** — the DB side of delivery: one relay becomes one Turn, and
    /// `turns.team_event_id` points back at it. (The pane behind the run is fake, so the RPC
    /// fails and delivery ends up `unknown` — itself the §9.1 path worth pinning down.)
    #[tokio::test]
    async fn a_delivered_relay_links_turn_message_and_event() {
        let s = S::new(1, false).await;
        s.arm().await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        let d1 = s.bot("worker", 0).await;

        let ctx = s.ctx().await;
        flush(s.app(), &ctx).await.unwrap();

        let ev = sqlx::query_as::<_, db::TeamEvent>(
            "SELECT * FROM team_events WHERE team_id=? AND to_bot_id=? AND kind='relay'",
        )
        .bind(&s.tid)
        .bind(&d1.id)
        .fetch_one(&s.app().db)
        .await
        .unwrap();
        assert_eq!(ev.status.as_deref(), Some("delivered"));
        let turn_id = ev.turn_id.clone().expect("the relay recorded its turn");
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&s.app().db)
            .await
            .unwrap();
        assert_eq!(turn.team_id.as_deref(), Some(s.tid.as_str()));
        assert_eq!(turn.team_event_id.as_deref(), Some(ev.id.as_str()), "T2: turns.team_event_id lines up");
        // §2.1's three-way distinction: a bot id here, `'daemon'` for the daemon's own, NULL
        // for what the user typed.
        let m = sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE turn_id=? AND role='user'")
            .bind(&turn_id)
            .fetch_one(&s.app().db)
            .await
            .unwrap();
        assert_eq!(m.team_id.as_deref(), Some(s.tid.as_str()));
        assert_eq!(m.relay_from.as_deref(), Some(pm.id.as_str()));
        assert_eq!(s.tasks().await[0].state, "working", "delivery is what moves `queued → working`");
        assert_eq!(s.team().await.pause_reason.as_deref(), Some("delivery_unknown"));
    }

    /// **T3** — two workers report almost at once; the PM can only hold one turn, so §8.4
    /// says merge them into a single prompt rather than dropping or serialising them.
    #[tokio::test]
    async fn t3_two_reports_reach_the_pm_as_one_prompt() {
        let s = S::new(2, false).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, dispatch2()).await;
        // Both workers hit a wall, which is the report that goes straight to the PM.
        for n in 0..2 {
            let w = s.bot("worker", n).await;
            s.reply(&w, json!({"action":"report","status":"blocked","summary":"卡住","notes":format!("原因 {n}")}))
                .await;
        }
        let pending = s.pending(&pm).await;
        assert_eq!(pending.len(), 2, "both reports are queued for the PM");

        // One prompt out of two events, numbered, with the status table.
        let merged = merged_text(&pending, "pm");
        assert!(merged.starts_with("以下是 2 則訊息："), "{merged}");
        assert!(merged.contains("1. ") && merged.contains("2. "));
        assert!(merged.contains("原因 0") && merged.contains("原因 1"));
        assert!(merged.contains("dispatch, wait, done, ask_user, abort"), "the PM's allowed actions are restated");

        // And delivery really does collapse them onto one Turn.
        s.arm().await;
        let ctx = s.ctx().await;
        flush(s.app(), &ctx).await.unwrap();
        let after = sqlx::query_as::<_, db::TeamEvent>(
            "SELECT * FROM team_events WHERE team_id=? AND to_bot_id=? AND kind='relay' ORDER BY seq",
        )
        .bind(&s.tid)
        .bind(&pm.id)
        .fetch_all(&s.app().db)
        .await
        .unwrap();
        assert_eq!(after.len(), 2);
        assert!(after.iter().all(|e| e.status.as_deref() == Some("delivered")));
        assert_eq!(after[0].turn_id, after[1].turn_id, "T3: two reports, one Turn");
        assert!(s.tasks().await.iter().all(|t| t.state == "blocked_by_worker"));
    }

    /// **T4** — `request_changes` sends the rework back to the same worker and bumps `round`.
    #[tokio::test]
    async fn t4_request_changes_goes_back_to_the_author() {
        let s = S::new(1, true).await;
        let (pm, d1, rev) = (s.bot("pm", 0).await, s.bot("worker", 0).await, s.bot("reviewer", 0).await);
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"寫好了"})).await;
        assert_eq!(s.tasks().await[0].state, "reported");

        // The task goes to review and the reviewer's worktree is parked on its branch.
        advance_tasks(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.tasks().await[0].state, "reviewing");
        assert!(s.wt("reviewer").join("a.txt").exists(), "the reviewer can see the change");
        assert_eq!(s.pending(&rev).await.len(), 1);

        s.reply(&rev, json!({"action":"verdict","result":"request_changes","summary":"少了測試",
                             "must_fix":["a.txt 要有測試"]}))
            .await;
        let t = &s.tasks().await[0];
        assert_eq!(t.state, "changes_requested");
        assert_eq!(t.round, 1, "T4: round 1");
        let p = s.pending(&d1).await;
        let payload: Value = serde_json::from_str(&p.last().unwrap().payload_json).unwrap();
        assert_eq!(payload["action"], "rework");
        assert!(payload["text"].as_str().unwrap().contains("a.txt 要有測試"));
        assert_eq!(p.last().unwrap().from_bot_id.as_deref(), Some(rev.id.as_str()));

        // A second round is allowed (max 2), a third is not: `exhausted` + a pause for a human.
        set_task_state(s.app(), &t.id, "reviewing").await.unwrap();
        s.reply(&rev, json!({"action":"verdict","result":"request_changes","summary":"再一次"})).await;
        assert_eq!(s.tasks().await[0].round, 2);
        set_task_state(s.app(), &t.id, "reviewing").await.unwrap();
        s.reply(&rev, json!({"action":"verdict","result":"request_changes","summary":"還是不行"})).await;
        assert_eq!(s.tasks().await[0].state, "exhausted");
        let team = s.team().await;
        assert_eq!(team.phase, "paused");
        assert_eq!(team.pause_reason.as_deref(), Some("review_exhausted"), "a cap pauses, never aborts");
    }

    /// **T5** — two workers touch the same line. The first merges, the second conflicts; the
    /// daemon aborts the merge and hands it back to its author, who rebases and reports
    /// again, and the second merge succeeds.
    #[tokio::test]
    async fn t5_a_conflict_goes_back_to_its_author_and_then_merges() {
        let s = S::new(2, false).await; // no reviewer: `reported → merging` directly
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, dispatch2()).await;
        let (d1, d2) = (s.bot("worker", 0).await, s.bot("worker", 1).await);

        // Both rewrite README.md — a guaranteed conflict.
        std::fs::write(s.wt("dev-1").join("README.md"), "dev-1 了\n").unwrap();
        std::fs::write(s.wt("dev-2").join("README.md"), "dev-2 了\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"改了 README"})).await;
        s.reply(&d2, json!({"action":"report","status":"done","summary":"也改了 README"})).await;
        // The uncommitted work was committed for them (appendix C).
        assert_eq!(git(&s.wt("dev-1"), &["status", "--porcelain"]), "");

        advance_tasks(s.app(), &s.tid).await.unwrap();
        let tasks = s.tasks().await;
        assert_eq!(tasks[0].state, "merged", "the first one goes in cleanly");
        assert!(tasks[0].merge_sha.is_some());
        assert_eq!(tasks[1].state, "rebasing", "the second conflicts and is handed back");
        assert_eq!(tasks[1].rebase_attempts, 1);
        assert_eq!(s.team().await.phase, "working", "a conflict is not a pause");

        // The conflict was recorded with its files, and the integration tree is clean again.
        let merges = sqlx::query_as::<_, db::TeamEvent>("SELECT * FROM team_events WHERE team_id=? AND kind='merge' ORDER BY seq")
            .bind(&s.tid)
            .fetch_all(&s.app().db)
            .await
            .unwrap();
        assert_eq!(merges.len(), 2);
        let conflict: Value = serde_json::from_str(&merges[1].payload_json).unwrap();
        assert_eq!(conflict["result"], "conflict");
        assert_eq!(conflict["conflict_files"], json!(["README.md"]));
        assert_eq!(git(&s.wt("main"), &["status", "--porcelain"]), "");

        // dev-2 is told to rebase, in its own worktree — nobody else's.
        let p = s.pending(&d2).await;
        let payload: Value = serde_json::from_str(&p.last().unwrap().payload_json).unwrap();
        assert_eq!(payload["action"], "rebase");
        assert!(payload["text"].as_str().unwrap().contains("git rebase"));
        assert!(payload["text"].as_str().unwrap().contains("README.md"));

        // It rebases and reports again; the second merge now succeeds.
        set_task_state(s.app(), &s.tasks().await[1].id, "rebasing").await.unwrap();
        git(&s.wt("dev-2"), &["rebase", "-X", "theirs", &s.team().await.branch]);
        s.reply(&d2, json!({"action":"report","status":"done","summary":"rebase 完成"})).await;
        assert_eq!(s.tasks().await[1].state, "merging", "a rebase report re-enters the merge queue");
        advance_tasks(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.tasks().await[1].state, "merged");

        // T8: through all of that the user's checkout never moved.
        assert_eq!(git(&s.e.repo, &["status", "--porcelain"]), "");
    }

    /// **T6** — the relay budget pauses the team, topping it up and resuming carries on.
    /// A cap must never end a team; only a person can.
    #[tokio::test]
    async fn t6_the_relay_budget_pauses_and_a_top_up_resumes() {
        let s = S::new(2, false).await;
        crate::team::patch(
            s.app(),
            &s.tid,
            crate::team::PatchTeam {
                budget: Some(crate::team::BudgetPatch { max_relays: Some(1), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        s.arm().await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, dispatch2()).await;
        assert_eq!(s.tasks().await.len(), 2, "two relays are queued");

        // One relay fits, the second recipient tips it over the cap.
        let ctx = s.ctx().await;
        flush(s.app(), &ctx).await.unwrap();
        s.unpause().await; // the fake pane makes the first delivery `unknown`; step past it
        let ctx = s.ctx().await;
        flush(s.app(), &ctx).await.unwrap();
        let t = s.team().await;
        assert_eq!(t.phase, "paused");
        assert_eq!(t.pause_reason.as_deref(), Some("budget_relays"));
        assert_eq!(t.resume_phase.as_deref(), Some("working"), "resume goes back where it was");
        let d2 = s.bot("worker", 1).await;
        assert_eq!(s.pending(&d2).await.len(), 1, "the undelivered relay is still waiting, not dropped");

        // `step` refuses to do anything while paused…
        step(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.pending(&d2).await.len(), 1);

        // …and a top-up plus resume gets it moving again (§4.5's last row).
        crate::team::patch(
            s.app(),
            &s.tid,
            crate::team::PatchTeam {
                budget: Some(crate::team::BudgetPatch { max_relays: Some(40), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        crate::team::resume(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.team().await.phase, "working");
        let ctx = s.ctx().await;
        flush(s.app(), &ctx).await.unwrap();
        assert!(s.pending(&d2).await.is_empty(), "T6: the queued relay went out after the top-up");
        let usage: Value = serde_json::from_str(&s.team().await.usage_json).unwrap();
        assert_eq!(usage["relays"], 2);
    }

    /// **T7** — everything a team is lives in SQLite, so a fresh `App` over the same files
    /// picks the run up exactly where it was. This is the restart, minus the process exit.
    #[tokio::test]
    async fn t7_a_restarted_daemon_carries_on_from_the_database() {
        let s = S::new(1, false).await;
        let (pm, d1) = (s.bot("pm", 0).await, s.bot("worker", 0).await);
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"好了"})).await;
        assert_eq!(s.tasks().await[0].state, "reported");
        let pending_before = s.pending(&d1).await.len();

        // A second daemon over the same data directory and the same sqlite file.
        let data = s.e.dir.join("data");
        let pool = db::open(&data.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(data.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(data.join("herdr.sock"));
        let app2 = App::new(
            pool,
            client.clone(),
            client,
            cfg,
            data.clone(),
            data.join("agents-managerd"),
            7799,
            "t".into(),
            "test".into(),
            false,
        );
        app2.connected.store(true, std::sync::atomic::Ordering::SeqCst);

        // Nothing was handed over in memory; it re-reads the tables.
        assert_eq!(db::live_teams(&app2.db).await.unwrap().len(), 1);
        crate::team::respawn_schedulers(&app2).await;
        let after = crate::team::load(&app2, &s.tid).await.unwrap();
        assert_eq!(after.phase, "working", "the worktrees and the workspace are still there");

        advance_tasks(&app2, &s.tid).await.unwrap();
        let tasks = db::team_tasks(&app2.db, &s.tid).await.unwrap();
        assert_eq!(tasks[0].state, "merged", "T7: the merge the first daemon never got to");
        assert_eq!(
            pending_for(&app2, &s.tid, &d1.id).await.unwrap().len(),
            pending_before,
            "the pending relay survived the restart untouched"
        );
    }

    /// **T8 / T9** — a whole run to `done` with `deliver = branch`, then cleanup.
    ///
    /// The repository has no remote at all, so if anything on this path tried to push, it
    /// would fail loudly rather than silently succeeding.
        /// `done.workers = "keep"`: the PM may carry its executors over to the next issue — same
    /// bot rows, same worktrees, no retirement — and the hand-over relay says so.
    #[tokio::test]
    async fn the_pm_can_keep_its_workers_across_issues() {
        let s = S::with_issues(1, true, vec![issue(), issue2()]).await;
        let (pm, rev, d1) = (s.bot("pm", 0).await, s.bot("reviewer", 0).await, s.bot("worker", 0).await);

        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"好了"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        s.reply(&rev, json!({"action":"verdict","result":"approve"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        s.reply(&pm, json!({"action":"done","summary":"完成了 A","workers":"keep"})).await;
        assert_eq!(s.team().await.phase, "finishing");
        let ctx = s.ctx().await;
        finish(s.app(), &ctx).await.unwrap();

        let t = s.team().await;
        assert_eq!((t.phase.as_str(), t.issue_number), ("planning", 43));
        let c = s.ctx().await;
        let workers = c.workers();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].id, d1.id, "the executor is the same bot");
        assert!(std::path::Path::new(&s.wt("dev-1")).exists(), "its worktree stays");
        assert!(db::bot(&s.app().db, &d1.id).await.unwrap().unwrap().deleted_at.is_none());

        let last: String = sqlx::query_scalar(
            "SELECT payload_json FROM team_events WHERE team_id=? AND kind='relay' ORDER BY seq DESC LIMIT 1",
        )
        .bind(&s.tid)
        .fetch_one(&s.app().db)
        .await
        .unwrap();
        let v: Value = serde_json::from_str(&last).unwrap();
        assert_eq!(v["action"], "next_issue");
        assert!(v["text"].as_str().unwrap().contains("沿用"), "{v}");
    }

    /// §2.3: one team, two issues. The PM and the reviewer are the *same bots* throughout;
    /// the executors are not, and neither is the integration branch.
    #[tokio::test]
    async fn the_queue_carries_the_pm_across_issues_and_swaps_the_workers() {
        let s = S::with_issues(1, true, vec![issue(), issue2()]).await;
        let (pm, rev, d1) = (s.bot("pm", 0).await, s.bot("reviewer", 0).await, s.bot("worker", 0).await);
        let first_branch = s.team().await.branch;

        // Work issue #42 to delivery.
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"好了"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        s.reply(&rev, json!({"action":"verdict","result":"approve"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.tasks().await[0].state, "merged");
        s.reply(&pm, json!({"action":"done","summary":"完成了 A"})).await;
        assert_eq!(s.team().await.phase, "finishing");
        let ctx = s.ctx().await;
        finish(s.app(), &ctx).await.unwrap();

        // The team did **not** end: it moved to the next issue.
        let t = s.team().await;
        assert_eq!(t.phase, "planning", "the queue is not empty, so the team keeps going");
        assert_eq!(t.issue_number, 43, "`teams` mirrors the current queue entry");
        assert_ne!(t.branch, first_branch, "issue #43 gets its own integration branch");
        assert_eq!(git(&s.wt("main"), &["rev-parse", "--abbrev-ref", "HEAD"]), t.branch);

        // The queue itself.
        let q = db::team_issues(&s.app().db, &s.tid).await.unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!((q[0].state.as_str(), q[0].issue_number), ("done", 42));
        assert_eq!((q[1].state.as_str(), q[1].issue_number), ("working", 43));
        assert_eq!(q[0].summary.as_deref(), Some("完成了 A"));

        // Same PM, same reviewer — same rows, so the same conversations and contexts.
        let c = s.ctx().await;
        assert_eq!(c.pm().unwrap().id, pm.id, "the PM is never replaced");
        assert_eq!(c.reviewer().unwrap().id, rev.id, "the reviewer is never replaced");

        // New executors, in their own directory; the old one is gone from disk and from the team.
        let workers = c.workers();
        assert_eq!(workers.len(), 1);
        assert_ne!(workers[0].id, d1.id, "issue #43 gets a fresh executor");
        assert!(workers[0].cwd.as_deref().unwrap().ends_with("/i2-dev-1"), "{:?}", workers[0].cwd);
        assert!(std::path::Path::new(&s.wt("dev-1")).exists() == false, "issue #42's worktree was removed");
        assert!(s.wt("i2-dev-1").exists(), "issue #43's worktree exists");
        let listed = git(&s.e.repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(listed.matches("worktree ").count(), 4, "main checkout + pm + reviewer + i2-dev-1");

        // The PM was told, in its own conversation, that the subject changed.
        let last: String = sqlx::query_scalar(
            "SELECT payload_json FROM team_events WHERE team_id=? AND kind='relay' ORDER BY seq DESC LIMIT 1",
        )
        .bind(&s.tid)
        .fetch_one(&s.app().db)
        .await
        .unwrap();
        let v: Value = serde_json::from_str(&last).unwrap();
        assert_eq!(v["action"], "next_issue");
        assert!(v["text"].as_str().unwrap().contains("#43"), "{v}");

        // The relay budget starts again for the new issue (§2.3).
        let c = s.ctx().await;
        assert_eq!(relay_count(s.app(), &s.tid, c.issue_id(), "delivered").await, 0);
    }

    #[tokio::test]
    async fn t8_and_t9_branch_delivery_leaves_the_checkout_alone_and_cleans_up() {
        let s = S::new(1, false).await;
        let (pm, d1) = (s.bot("pm", 0).await, s.bot("worker", 0).await);
        let head_before = git(&s.e.repo, &["rev-parse", "HEAD"]);
        let before = tree_of(&s.e.repo);

        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"好了"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.tasks().await[0].state, "merged");

        // §8.3: the PM cannot declare `done` while anything is open — but everything is.
        s.reply(&pm, json!({"action":"done","summary":"完成了 A"})).await;
        assert_eq!(s.team().await.phase, "finishing");
        let ctx = s.ctx().await;
        finish(s.app(), &ctx).await.unwrap();
        let t = s.team().await;
        assert_eq!(t.phase, "done");
        assert_eq!(t.summary.as_deref(), Some("完成了 A"));
        assert!(t.pr_url.is_none(), "`branch` never opens a PR");
        let delivered = sqlx::query_scalar::<_, String>(
            // The delivery note is no longer the last one: finishing an issue writes an
            // `issue_finished` note after it (§2.3), so ask for this note by its action.
            "SELECT payload_json FROM team_events WHERE team_id=? AND kind='note'
               AND payload_json LIKE '%\"action\":\"delivered\"%' ORDER BY seq DESC LIMIT 1",
        )
        .bind(&s.tid)
        .fetch_one(&s.app().db)
        .await
        .unwrap();
        let d: Value = serde_json::from_str(&delivered).unwrap();
        assert_eq!(d["deliver"], "branch");
        assert_eq!(d["pushed"], false);

        // T8: the user's checkout is byte-for-byte where it started.
        assert_eq!(git(&s.e.repo, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(git(&s.e.repo, &["status", "--porcelain"]), "");
        assert_eq!(tree_of(&s.e.repo), before, "nothing new under <project.path>");
        let listed = git(&s.e.repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(listed.matches("worktree ").count(), 3, "main checkout + pm + dev-1");
        // The work is on the integration branch, and only there.
        assert_eq!(git(&s.e.repo, &["show", &format!("{}:a.txt", t.branch)]), "v1");
        assert!(!git(&s.e.repo, &["ls-tree", "--name-only", "HEAD"]).contains("a.txt"), "…and not on the user's HEAD");

        // T9: cleanup removes the worktrees and the root, closes the workspace, keeps branches.
        assert_eq!(s.e.herdr.count(), 1);
        crate::team::cleanup(s.app(), &s.tid).await.unwrap();
        let listed = git(&s.e.repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(listed.matches("worktree ").count(), 1, "only the main checkout is left: {listed}");
        assert!(!git(&s.e.repo, &["worktree", "list"]).contains("prunable"), "remove → prune → rmdir, in that order");
        assert!(!std::path::Path::new(&t.worktree_root).exists());
        assert_eq!(s.e.herdr.count(), 0, "§6.4a: the team's workspace was closed");
        let branches = git(&s.e.repo, &["branch", "--list"]);
        assert!(branches.contains(&t.branch), "branches are kept: {branches}");
        assert!(branches.contains("-t1"));
        // The record survives a cleanup — that is what distinguishes it from a delete.
        assert!(db::team(&s.app().db, &s.tid).await.unwrap().is_some());
    }

    /// §6.5a — `DELETE` removes the record too, keeps the conversation, and only touches
    /// branches when asked. Also: it works in a *live* phase, unlike `cleanup`.
    #[tokio::test]
    async fn delete_removes_the_record_but_not_the_conversation() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        let t = s.team().await;
        assert_eq!(t.phase, "working", "not terminal — `cleanup` would refuse this");
        assert!(crate::team::cleanup(s.app(), &s.tid).await.is_err());

        // Something the PM "said", which must outlive the team.
        let conv = db::conversation_id(&s.app().db, &pm.id).await.unwrap();
        crate::lifecycle::insert_message(s.app(), &conv, None, "assistant", "我拆好了", "hook", false, None)
            .await
            .unwrap();

        let out = crate::team::delete(s.app(), &s.tid, false).await.unwrap();
        assert_eq!(out["deleted"], true);
        assert_eq!(out["branches_deleted"], json!([]), "branches are kept by default");
        assert!(db::team(&s.app().db, &s.tid).await.unwrap().is_none());
        assert!(db::team_tasks(&s.app().db, &s.tid).await.unwrap().is_empty());
        let evs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM team_events WHERE team_id=?")
            .bind(&s.tid)
            .fetch_one(&s.app().db)
            .await
            .unwrap();
        assert_eq!(evs, 0);
        // The members are soft-deleted; what they said is untouched (SPEC §6.4).
        let members = db::team_members(&s.app().db, &s.tid).await.unwrap();
        assert!(members.iter().all(|m| m.deleted_at.is_some()));
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?")
            .bind(&conv)
            .fetch_one(&s.app().db)
            .await
            .unwrap();
        assert_eq!(kept, 1, "the conversation survives the team");
        // Site cleaned up all the same.
        assert!(!std::path::Path::new(&t.worktree_root).exists());
        assert_eq!(git(&s.e.repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
        assert_eq!(s.e.herdr.count(), 0);
        assert!(git(&s.e.repo, &["branch", "--list"]).contains(&t.branch));

        // Idempotent: a second delete is a plain 404.
        assert!(matches!(crate::team::delete(s.app(), &s.tid, false).await, Err(LcError::NotFound(w)) if w == "team"));
    }

    /// §6.5a — `?branches=delete` is the one path that destroys work, so it is checked on
    /// its own: both the integration branch and every task branch go.
    #[tokio::test]
    async fn delete_with_branches_removes_them() {
        let s = S::new(1, false).await;
        let (pm, d1) = (s.bot("pm", 0).await, s.bot("worker", 0).await);
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"好了"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        let t = s.team().await;
        let task_branch = s.tasks().await[0].branch.clone();

        let out = crate::team::delete(s.app(), &s.tid, true).await.unwrap();
        let gone: Vec<String> = serde_json::from_value(out["branches_deleted"].clone()).unwrap();
        assert!(gone.contains(&task_branch) && gone.contains(&t.branch), "{gone:?}");
        let branches = git(&s.e.repo, &["branch", "--list"]);
        assert!(!branches.contains("team/i42-"), "every team branch is gone: {branches}");
        // The user's own branch is untouched.
        assert!(branches.contains("main"));
    }

    /// §4.4 — a reply without a usable block draws a repair prompt, twice, and then stops
    /// for a human instead of asking for ever.
    #[tokio::test]
    async fn a_broken_reply_gets_two_repair_prompts_then_pauses() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        let broken = |t: &str| {
            let (app, tid, id) = (s.app().clone(), s.tid.clone(), pm.id.clone());
            let t = t.to_string();
            async move { apply_reply(&app, &tid, &id, &t, "completed").await.unwrap() }
        };
        broken("我覺得應該先看看程式碼").await;
        let p = s.pending(&pm).await;
        assert_eq!(p.len(), 1);
        let payload: Value = serde_json::from_str(&p[0].payload_json).unwrap();
        assert_eq!(payload["action"], "repair");
        assert!(payload["text"].as_str().unwrap().contains("找不到 am-team 區塊"));
        assert!(payload["text"].as_str().unwrap().contains("dispatch, wait, done"));
        assert!(p[0].from_bot_id.is_none(), "a repair prompt comes from the daemon, not a bot");

        broken("```am-team\n{壞掉的\n```").await;
        assert_eq!(s.pending(&pm).await.len(), 2);
        assert_eq!(s.team().await.phase, "planning", "still going after two");

        broken("還是不對").await;
        let t = s.team().await;
        assert_eq!(t.phase, "paused");
        assert_eq!(t.pause_reason.as_deref(), Some("protocol_error"));
        assert_eq!(s.pending(&pm).await.len(), 2, "no third prompt was sent");

        // A good reply afterwards resets the counter (the run of repairs is broken by it).
        crate::team::set_phase(s.app(), &s.tid, "planning", None, None).await.unwrap();
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        assert_eq!(consecutive_repairs(s.app(), &s.tid, &pm.id).await, 0);
    }

    /// A terminal-scrape reply is treated as "no block" even when one is visible, because
    /// the scrape itself may be truncated (§4.4's last paragraph).
    #[tokio::test]
    async fn a_terminal_fallback_reply_is_never_trusted() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        let good = "```am-team\n{\"action\":\"wait\"}\n```";
        apply_reply(s.app(), &s.tid, &pm.id, good, "completed_fallback").await.unwrap();
        let p = s.pending(&pm).await;
        assert_eq!(serde_json::from_str::<Value>(&p[0].payload_json).unwrap()["action"], "repair");
        // A failed turn is the same story.
        apply_reply(s.app(), &s.tid, &pm.id, "", "failed").await.unwrap();
        assert_eq!(s.pending(&pm).await.len(), 2);
    }

    /// 2026-09-09 (#48 t21): a second `report{done}` for a task that is already under review is
    /// a re-send, not a new report — the task stays `reviewing` and the reviewer is not asked
    /// the same round twice.
    #[tokio::test]
    async fn a_second_report_while_reviewing_is_ignored() {
        let s = S::new(1, true).await;
        let (pm, d1, rev) = (s.bot("pm", 0).await, s.bot("worker", 0).await, s.bot("reviewer", 0).await);
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"寫好了"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.tasks().await[0].state, "reviewing");
        assert_eq!(s.pending(&rev).await.len(), 1);

        s.reply(&d1, json!({"action":"report","status":"done","summary":"寫好了（再送一次）"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.tasks().await[0].state, "reviewing", "still with the reviewer");
        assert_eq!(s.pending(&rev).await.len(), 1, "no second review relay");
        assert_ne!(s.team().await.phase, "paused");
    }

    /// 2026-09-09: the wall clock skips the minutes a team spent paused.
    #[tokio::test]
    async fn paused_time_is_not_on_the_wall_clock() {
        let s = S::new(1, false).await;
        let since = "2026-01-01T00:00:00Z";
        let ev = |from: &str, to: &str, at: &str| {
            let (app, tid) = (s.app().clone(), s.tid.clone());
            let (from, to, at) = (from.to_string(), to.to_string(), at.to_string());
            async move {
                sqlx::query("INSERT INTO team_events (id, team_id, seq, kind, payload_json, created_at) VALUES (?, ?, (SELECT COALESCE(MAX(seq),0)+1 FROM team_events WHERE team_id=?), 'phase', ?, ?)")
                    .bind(db::ulid()).bind(&tid).bind(&tid)
                    .bind(json!({"from": from, "to": to}).to_string()).bind(at)
                    .execute(&app.db).await.unwrap();
            }
        };
        ev("working", "paused", "2026-01-01T01:00:00Z").await;
        ev("paused", "working", "2026-01-01T03:00:00Z").await;
        ev("working", "paused", "2026-01-01T05:00:00Z").await;
        ev("paused", "working", "2026-01-01T05:30:00Z").await;
        let secs = paused_secs_since(s.app(), &s.tid, since, false).await;
        assert_eq!(secs, 2 * 3600 + 30 * 60);
    }

    /// §4.5 (2026-09-08) — the worker pool. Three tasks against a parallelism of one: the
    /// first runs, the other two wait with nobody on them, and each is handed over — and only
    /// then cut from the integration branch — as the one before it merges.
    #[tokio::test]
    async fn a_parallelism_of_one_runs_the_queue_one_at_a_time() {
        let s = S::new(1, false).await;
        s.arm().await;
        let (pm, d1) = (s.bot("pm", 0).await, s.bot("worker", 0).await);
        s.reply(
            &pm,
            json!({"action":"dispatch","tasks":[
                {"title":"A","brief":"做 A"},{"title":"B","brief":"做 B"},{"title":"C","brief":"做 C"}]}),
        )
        .await;

        let tasks = s.tasks().await;
        assert_eq!(tasks.len(), 3);
        assert_eq!(
            tasks[0].worker_bot_id.as_deref(),
            Some(d1.id.as_str()),
            "t1 is on the one executor"
        );
        assert!(
            tasks[1].worker_bot_id.is_none() && tasks[2].worker_bot_id.is_none(),
            "t2/t3 are queued"
        );
        assert!(
            tasks.iter().all(|t| t.want_worker_bot_id.is_none()),
            "the PM named nobody"
        );
        let branches = git(&s.e.repo, &["branch", "--list"]);
        assert!(branches.contains(&tasks[0].branch), "t1's branch is cut");
        assert!(
            !branches.contains(&tasks[1].branch),
            "t2's branch waits until t2 starts"
        );

        // t1 through to merged; the freed slot picks up t2 in the same sweep.
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(
            &d1,
            json!({"action":"report","status":"done","summary":"好了"}),
        )
        .await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        let tasks = s.tasks().await;
        assert_eq!(tasks[0].state, "merged");
        assert_eq!(
            tasks[1].worker_bot_id.as_deref(),
            Some(d1.id.as_str()),
            "t2 was handed over automatically"
        );
        assert!(tasks[2].worker_bot_id.is_none(), "t3 is still queued");
        assert!(
            git(&s.e.repo, &["branch", "--list"]).contains(&tasks[1].branch),
            "cut now, not at dispatch"
        );
        // §6.2: cut from the integration branch *now*, so it already carries t1's merge.
        assert_eq!(git(&s.wt("dev-1"), &["show", "HEAD:a.txt"]), "v1");
    }

    /// §4.5 — a task the PM addressed to a busy executor waits for **that** executor, even
    /// with another one free.
    #[tokio::test]
    async fn a_named_task_waits_for_its_own_executor() {
        let s = S::new(2, false).await;
        s.arm().await;
        let pm = s.bot("pm", 0).await;
        let (d1, d2) = (s.bot("worker", 0).await, s.bot("worker", 1).await);
        s.reply(
            &pm,
            json!({"action":"dispatch","tasks":[
                {"to":"dev-1","title":"A","brief":"做 A"},{"to":"dev-1","title":"B","brief":"做 B"}]}),
        )
        .await;
        let tasks = s.tasks().await;
        assert_eq!(tasks[0].worker_bot_id.as_deref(), Some(d1.id.as_str()));
        assert!(
            tasks[1].worker_bot_id.is_none(),
            "dev-2 must not pick up dev-1's task"
        );
        assert_eq!(tasks[1].want_worker_bot_id.as_deref(), Some(d1.id.as_str()));
        assert!(open_task_of(s.app(), &s.tid, &d2.id)
            .await
            .unwrap()
            .is_none());
    }

    /// §8.3 — `done` counts the queue too: a task nobody has started yet is still open work.
    #[tokio::test]
    async fn done_is_refused_while_tasks_are_still_queued() {
        let s = S::new(1, false).await;
        s.arm().await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"title":"A","brief":"做 A"},{"title":"B","brief":"做 B"}]}))
            .await;
        // Take the running one out of the way; the queued one alone must still block `done`.
        set_task_state(s.app(), &s.tasks().await[0].id, "merged")
            .await
            .unwrap();
        s.reply(&pm, json!({"action":"done","summary":"完成"}))
            .await;
        assert_ne!(s.team().await.phase, "finishing", "the queue is not empty");
        let p = s.pending(&pm).await;
        let last: Value = serde_json::from_str(&p.last().unwrap().payload_json).unwrap();
        let text = last["text"].as_str().unwrap();
        assert!(
            text.contains("還不能 `done`") && text.contains("排隊中"),
            "{text}"
        );
    }

    /// §4.4 — `to` is optional now; `brief` never was.
    #[test]
    fn dispatch_parses_without_a_recipient_but_not_without_a_brief() {
        let a = Action::parse(
            "pm",
            &json!({"action":"dispatch","tasks":[{"title":"A","brief":"做 A"}]}),
        )
        .unwrap();
        assert_eq!(
            a,
            Action::Dispatch(vec![DispatchItem {
                to: None,
                title: "A".into(),
                brief: "做 A".into(),
                files: vec![],
                issue: None
            }])
        );
        let named = Action::parse(
            "pm",
            &json!({"action":"dispatch","tasks":[{"to":"dev-1","brief":"b"}]}),
        )
        .unwrap();
        assert_eq!(
            named,
            Action::Dispatch(vec![DispatchItem {
                to: Some("dev-1".into()),
                title: "task".into(),
                brief: "b".into(),
                files: vec![],
                issue: None
            }])
        );
        assert!(Action::parse(
            "pm",
            &json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A"}]})
        )
        .unwrap_err()
        .contains("brief"));
    }

    /// §4.5 — a named executor that is busy is a queue to join, not a refusal; an unknown
    /// one is still refused; the same brief twice is still a loop.
    #[tokio::test]
    async fn a_named_busy_worker_queues_and_a_repeat_still_pauses() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        let one = |brief: &str| {
            json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":brief}]})
        };
        s.reply(&pm, one("做 A")).await;
        assert_eq!(s.tasks().await.len(), 1);

        // A second task for the same busy worker is queued behind the first, not rejected.
        s.reply(&pm, one("做 B")).await;
        let tasks = s.tasks().await;
        assert_eq!(tasks.len(), 2, "the second one is on the queue");
        assert!(tasks[1].worker_bot_id.is_none(), "nobody is on it yet");
        assert_eq!(tasks[1].want_worker_bot_id.as_deref(), Some(s.bot("worker", 0).await.id.as_str()));
        // An unknown recipient is refused, not silently dropped.
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-9","title":"C","brief":"做 C"}]})).await;
        let p = s.pending(&pm).await;
        let last: Value = serde_json::from_str(&p.last().unwrap().payload_json).unwrap();
        assert!(last["text"].as_str().unwrap().contains("dev-9"));

        // The identical brief again is a loop → pause, whoever it was aimed at.
        s.reply(&pm, one("做 A")).await;
        let t = s.team().await;
        assert_eq!(t.pause_reason.as_deref(), Some("pm_repeat"));
    }

    /// §8.3 — `done` is verified, and repeated `wait` after all work is done pauses a stalled PM.
    #[tokio::test]
    async fn done_is_checked_and_wait_pauses_a_stalled_pm() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        s.reply(&pm, json!({"action":"done","summary":"我說完成就完成"})).await;
        assert_eq!(s.team().await.phase, "working", "the daemon checked the tasks");
        let last: Value = serde_json::from_str(&s.pending(&pm).await.last().unwrap().payload_json).unwrap();
        assert_eq!(last["action"], "reject");

        set_task_state(s.app(), &s.tasks().await[0].id, "merged").await.unwrap();
        s.reply(&pm, json!({"action":"wait"})).await;
        let n = s.pending(&pm).await.len();
        let last: Value = serde_json::from_str(&s.pending(&pm).await.last().unwrap().payload_json).unwrap();
        assert_eq!(last["action"], "nudge");
        s.reply(&pm, json!({"action":"wait"})).await;
        let team = s.team().await;
        assert_eq!(team.phase, "paused");
        assert_eq!(team.pause_reason.as_deref(), Some("pm_stalled"));
        assert_eq!(s.pending(&pm).await.len(), n + 1, "the pause explanation is queued once");
        let last: Value = serde_json::from_str(&s.pending(&pm).await.last().unwrap().payload_json).unwrap();
        assert_eq!(last["action"], "nudge");
        assert!(last["text"].as_str().unwrap().contains("已暫停，等使用者決定"));
    }

    #[tokio::test]
    async fn resuming_pm_stalled_nudges_pm_again() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        set_task_state(s.app(), &s.tasks().await[0].id, "merged").await.unwrap();
        s.reply(&pm, json!({"action":"wait"})).await;
        s.reply(&pm, json!({"action":"wait"})).await;
        assert_eq!(s.team().await.pause_reason.as_deref(), Some("pm_stalled"));

        s.arm().await;
        crate::team::resume(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.team().await.phase, "working");
        let last: Value = serde_json::from_str(&s.pending(&pm).await.last().unwrap().payload_json).unwrap();
        assert_eq!(last["action"], "nudge");
        assert!(last["text"].as_str().unwrap().contains("`done`"));
        assert!(last["text"].as_str().unwrap().contains("`dispatch`"));
    }

    #[tokio::test]
    async fn waiting_twice_without_tasks_pauses_the_team() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"wait"})).await;
        let last: Value = serde_json::from_str(&s.pending(&pm).await.last().unwrap().payload_json).unwrap();
        assert_eq!(last["action"], "nudge");
        s.reply(&pm, json!({"action":"wait"})).await;
        let team = s.team().await;
        assert_eq!(team.phase, "paused");
        assert_eq!(team.pause_reason.as_deref(), Some("pm_stalled"));

    }

    /// §4.5 wall clock and quota — both pause, neither aborts.
    #[tokio::test]
    async fn the_time_and_quota_gates_pause() {
        let s = S::new(1, false).await;
        // Backdate the start past the budget. The wall clock is per issue now (§2.3), so the
        // queue row is what has to move; `teams.started_at` goes with it to keep the two
        // consistent for anything that reads the team's own start.
        let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(200)).to_rfc3339();
        sqlx::query("UPDATE teams SET started_at = ? WHERE id = ?")
            .bind(&long_ago)
            .bind(&s.tid)
            .execute(&s.app().db)
            .await
            .unwrap();
        sqlx::query("UPDATE team_issues SET started_at = ? WHERE team_id = ?")
            .bind(&long_ago)
            .bind(&s.tid)
            .execute(&s.app().db)
            .await
            .unwrap();
        step(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.team().await.pause_reason.as_deref(), Some("budget_time"));

        let s = S::new(1, false).await;
        s.app().quotas.lock().await.insert(
            "claude".into(),
            crate::quota::Quota {
                five_hour: Some(crate::quota::Window { used_pct: 97.0, resets_at: None }),
                seven_day: None,
                fable: None,
                reset_credits: None,
                limit_hit: None,
                plan: None,
                updated_at: db::now(),
                source: "test".into(),
                account: None,
                host: crate::config::LOCAL_HOST.into(),
            },
        );
        step(s.app(), &s.tid).await.unwrap();
        let t = s.team().await;
        assert_eq!(t.pause_reason.as_deref(), Some("quota_low"));
        assert!(!crate::team::is_terminal(&t.phase), "a quota cap never ends a team");
        // §4.5: the pause has to name *who* ran out, or the user cannot tell which account to
        // go and look at. Every member is on the same kind here, so all of them are listed.
        let d = crate::team::pause_detail_json(&t);
        let members = d["members"].as_array().cloned().unwrap_or_default();
        assert!(!members.is_empty(), "quota_low with no member named: {d}");
        assert_eq!(members[0]["window"], "five_hour");
        assert_eq!(members[0]["kind"], "claude");
        assert_eq!(members[0]["used_pct"], 97.0);
        assert_eq!(members[0]["remaining_pct"], 3.0);
        assert_eq!(members[0]["short"], "dev-1", "the short role name is what the banner prints");
        assert_eq!(members.len(), 1, "the PM is a codex bot with no quota row: {d}");
        // A `resume` clears the detail with the pause it described.
        crate::team::resume(s.app(), &s.tid).await.unwrap();
        assert!(crate::team::pause_detail_json(&s.team().await).is_null());

        // Two kinds out at once: both are named, worst first, so resuming after fixing one
        // account does not stop the team again on the other.
        s.app().quotas.lock().await.insert(
            "codex".into(),
            crate::quota::Quota {
                five_hour: Some(crate::quota::Window { used_pct: 91.0, resets_at: None }),
                seven_day: Some(crate::quota::Window {
                    used_pct: 99.0,
                    resets_at: Some("2026-09-16T03:20:00Z".into()),
                }),
                fable: None,
                reset_credits: None,
                limit_hit: None,
                plan: None,
                updated_at: db::now(),
                source: "test".into(),
                account: None,
                host: crate::config::LOCAL_HOST.into(),
            },
        );
        step(s.app(), &s.tid).await.unwrap();
        let d = crate::team::pause_detail_json(&s.team().await);
        let members = d["members"].as_array().cloned().unwrap_or_default();
        assert_eq!(members.len(), 2, "both members are out: {d}");
        assert_eq!(members[0]["short"], "pm", "worst first");
        assert_eq!(members[0]["window"], "seven_day", "the window closest to its cap");
        assert_eq!(members[0]["resets_at"], "2026-09-16T03:20:00Z");
    }

    /// §4.6 — the supervised gates hold, and `approve` releases exactly one.
    #[tokio::test]
    async fn supervised_gates_hold_dispatch_and_merge() {
        let s = S::new(1, false).await;
        crate::team::patch(
            s.app(),
            &s.tid,
            crate::team::PatchTeam { supervised: Some(true), ..Default::default() },
        )
        .await
        .unwrap();
        let (pm, d1) = (s.bot("pm", 0).await, s.bot("worker", 0).await);
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        let t = s.team().await;
        assert_eq!(t.pause_reason.as_deref(), Some("gate:dispatch"));
        assert_eq!(s.tasks().await.len(), 1, "the task exists; only its relay is held");

        crate::team::approve(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.team().await.phase, "working");

        // The merge gate is a separate hold.
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"好了"})).await;
        advance_tasks(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.team().await.pause_reason.as_deref(), Some("gate:merge"));
        assert_eq!(s.tasks().await[0].state, "merging", "still waiting, not merged");
        crate::team::approve(s.app(), &s.tid).await.unwrap();
        advance_tasks(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.tasks().await[0].state, "merged");
    }

    /// §6.3 — a hand edit of the integration branch stops the merge instead of being folded
    /// into a merge commit nobody reviewed.
    #[tokio::test]
    async fn a_dirty_integration_worktree_stops_the_merge() {
        let s = S::new(1, false).await;
        let (pm, d1) = (s.bot("pm", 0).await, s.bot("worker", 0).await);
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        std::fs::write(s.wt("dev-1").join("a.txt"), "v1\n").unwrap();
        s.reply(&d1, json!({"action":"report","status":"done","summary":"好了"})).await;
        // The PM "helpfully" edits the integration worktree.
        std::fs::write(s.wt("main").join("README.md"), "PM 手癢\n").unwrap();
        advance_tasks(s.app(), &s.tid).await.unwrap();
        let t = s.team().await;
        assert_eq!(t.pause_reason.as_deref(), Some("integration_dirty"));
        assert_eq!(s.tasks().await[0].state, "merging", "the task is untouched, waiting");
    }

    /// Appendix C — a `report` with no commits on the branch is a mistake to correct, not an
    /// empty merge to make.
    #[tokio::test]
    async fn reporting_an_empty_branch_asks_again() {
        let s = S::new(1, false).await;
        let (pm, d1) = (s.bot("pm", 0).await, s.bot("worker", 0).await);
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        s.reply(&d1, json!({"action":"report","status":"done","summary":"我什麼都沒做"})).await;
        assert_eq!(s.tasks().await[0].state, "queued", "not advanced");
        let payload: Value = serde_json::from_str(&s.pending(&d1).await.last().unwrap().payload_json).unwrap();
        assert_eq!(payload["action"], "repair");
        assert!(payload["text"].as_str().unwrap().contains("沒有任何 commit"));
    }

    /// §6.4a's additive migration, exercised the way it will actually be met: a database
    /// whose `teams` table was written before the column existed, with a row already in it.
    ///
    /// This is the failure mode that took the daemon down earlier in this branch's life — a
    /// schema change that only worked on a fresh file — so it is checked directly rather
    /// than inferred from "the tests pass on a new database".
    #[tokio::test]
    async fn an_older_database_gains_workspace_id_without_losing_its_rows() {
        let dir = std::env::temp_dir().join(format!("am-migrate-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("db.sqlite3");
        {
            // The `teams` table exactly as an earlier build wrote it: no `workspace_id`.
            let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=rwc", file.display())).await.unwrap();
            sqlx::query(
                "CREATE TABLE teams (
                   id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
                   issue_number INTEGER NOT NULL, issue_title TEXT NOT NULL, issue_url TEXT NOT NULL,
                   phase TEXT NOT NULL, pause_reason TEXT, resume_phase TEXT,
                   base_ref TEXT NOT NULL, base_sha TEXT NOT NULL, branch TEXT NOT NULL, worktree_root TEXT NOT NULL,
                   deliver TEXT NOT NULL, supervised INTEGER NOT NULL DEFAULT 0,
                   roles_json TEXT NOT NULL, budget_json TEXT NOT NULL, usage_json TEXT NOT NULL DEFAULT '{}',
                   pr_url TEXT, summary TEXT,
                   created_at TEXT NOT NULL, started_at TEXT, ended_at TEXT)",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO teams (id, project_id, issue_number, issue_title, issue_url, phase, base_ref,
                   base_sha, branch, worktree_root, deliver, roles_json, budget_json, created_at)
                 VALUES ('old','p',7,'舊的','u','working','HEAD','abc','team/i7-old','/tmp/old','branch','{}','{}','now')",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        // Opening it is the migration.
        let pool = db::open(&file).await.unwrap();
        let cols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('teams')")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(cols.iter().any(|c| c == "workspace_id"), "the column was added: {cols:?}");
        let t = db::team(&pool, "old").await.unwrap().expect("the existing row survived");
        assert_eq!(t.issue_title, "舊的");
        assert_eq!(t.workspace_id, None, "an old team has no workspace — which is what the reconcile pauses on");
        // Re-opening the same file is a no-op, not a second ALTER.
        pool.close().await;
        let pool = db::open(&file).await.unwrap();
        assert!(db::team(&pool, "old").await.unwrap().is_some());
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §6.4a — the reconcile pauses a team whose workspace is gone rather than starting its
    /// members somewhere else.
    #[tokio::test]
    async fn a_vanished_workspace_pauses_the_team() {
        let s = S::new(1, false).await;
        let ws = s.team().await.workspace_id.unwrap();
        s.e.herdr.workspaces.lock().unwrap().remove(&ws);
        crate::team::respawn_schedulers(s.app()).await;
        let t = s.team().await;
        assert_eq!(t.phase, "paused");
        assert_eq!(t.pause_reason.as_deref(), Some("workspace_missing"));
        assert_eq!(t.resume_phase.as_deref(), Some("planning"));
    }

    /// §6.4a — a member's pane goes in the team's workspace, and a member whose workspace
    /// has gone refuses to start rather than landing in the user's own space.
    #[tokio::test]
    async fn a_member_starts_in_the_teams_workspace() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        // The mock has no `agent.start`, so the start fails — but *where* it fails is the
        // point: the error names the missing workspace, not a project one.
        s.e.herdr.workspaces.lock().unwrap().clear();
        let err = format!("{:?}", crate::lifecycle::start_bot(s.app(), &pm.id).await.unwrap_err());
        assert!(err.contains("workspace"), "{err}");
        // …and the project's workspace was still never created behind the user's back.
        assert_eq!(db::project(&s.app().db, &s.e.project_id).await.unwrap().unwrap().workspace_id, None);
    }

    /// §6.4 — `deliver = pr` on a project with no GitHub origin quietly becomes `branch`.
    /// Nothing is pushed on any path but a real PR, which is why this one is worth pinning.
    #[tokio::test]
    async fn pr_delivery_without_a_github_origin_falls_back_to_branch() {
        let s = S::new(1, false).await;
        crate::team::patch(
            s.app(),
            &s.tid,
            crate::team::PatchTeam { deliver: Some("pr".into()), ..Default::default() },
        )
        .await
        .unwrap();
        crate::team::set_phase(s.app(), &s.tid, "finishing", None, None).await.unwrap();
        let ctx = s.ctx().await;
        finish(s.app(), &ctx).await.unwrap();
        let t = s.team().await;
        assert_eq!(t.phase, "done");
        assert!(t.pr_url.is_none());
        let notes: Vec<String> =
            sqlx::query_scalar("SELECT payload_json FROM team_events WHERE team_id=? AND kind='note'")
                .bind(&s.tid)
                .fetch_all(&s.app().db)
                .await
                .unwrap();
        assert!(notes.iter().any(|n| n.contains("deliver_downgraded")), "{notes:?}");
        // The repo has no remote, so a push would have errored; the branch is still local.
        assert!(git(&s.e.repo, &["branch", "--list"]).contains(&t.branch));
    }

    /// **The incident, reproduced exactly.** A team created by a build that predated the
    /// worktree layout ended up with `worktree_root = ''` and all four members' `cwd` set to
    /// the user's main checkout — which had 55 uncommitted files in it. Nothing stopped a
    /// `report` from running `git add -A && git commit` there.
    ///
    /// Every path that could have reached the checkout is checked here, and the test asserts
    /// the negative that matters: the user's uncommitted work is still uncommitted.
    #[tokio::test]
    async fn a_member_pointed_at_the_main_checkout_can_do_nothing_at_all() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":"做 A"}]})).await;
        let d1 = s.bot("worker", 0).await;

        // The user is mid-edit in their own checkout.
        std::fs::write(s.e.repo.join("wip.txt"), "使用者還沒 commit 的東西\n").unwrap();
        std::fs::write(s.e.repo.join("README.md"), "使用者改到一半\n").unwrap();
        let dirty_before = git(&s.e.repo, &["status", "--porcelain"]);
        assert!(dirty_before.contains("wip.txt"));

        // Corrupt the team into the shape the incident had.
        let repo = s.e.repo.to_string_lossy().to_string();
        sqlx::query("UPDATE teams SET worktree_root = '' WHERE id = ?")
            .bind(&s.tid)
            .execute(&s.app().db)
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET cwd = ? WHERE team_id = ?")
            .bind(&repo)
            .bind(&s.tid)
            .execute(&s.app().db)
            .await
            .unwrap();

        // 1. The guard rejects it on its own terms, with both halves of the reason.
        let t = s.team().await;
        let project = db::project(&s.app().db, &s.e.project_id).await.unwrap().unwrap();
        let bots = db::team_members(&s.app().db, &s.tid).await.unwrap();
        let err = crate::team::check_layout(&t, &project, &bots).unwrap_err();
        assert!(err.contains("worktree_root"), "{err}");
        sqlx::query("UPDATE teams SET worktree_root = ? WHERE id = ?")
            .bind(format!("{}/data/teams/{}", s.e.dir.to_string_lossy(), s.tid))
            .bind(&s.tid)
            .execute(&s.app().db)
            .await
            .unwrap();
        // …and with a perfectly good root, the *explicit* main-checkout cwd is still refused.
        // This is the half a null check would have missed: the cwd was not missing, it was
        // wrong.
        let t = s.team().await;
        let err = crate::team::check_layout(&t, &project, &bots).unwrap_err();
        assert!(err.contains("inside the project checkout"), "{err}");

        // 2. A `report` — the `add -A && commit` path — refuses instead of committing.
        let res = apply_reply(
            s.app(),
            &s.tid,
            &d1.id,
            "```am-team\n{\"action\":\"report\",\"status\":\"done\",\"summary\":\"好了\"}\n```",
            "completed",
        )
        .await;
        assert!(res.is_err(), "a report must not run git in the user's checkout");

        // 3. A dispatch — the `checkout -b` path — likewise.
        assert!(apply_reply(
            s.app(),
            &s.tid,
            &pm.id,
            "```am-team\n{\"action\":\"dispatch\",\"tasks\":[{\"to\":\"dev-1\",\"title\":\"B\",\"brief\":\"做 B\"}]}\n```",
            "completed",
        )
        .await
        .is_err());

        // 4. The merge path.
        assert!(advance_tasks(s.app(), &s.tid).await.is_err());

        // 5. `step` does not error — it pauses the team where a human can see it.
        step(s.app(), &s.tid).await.unwrap();
        let t = s.team().await;
        assert_eq!(t.phase, "paused");
        assert_eq!(t.pause_reason.as_deref(), Some("worktree_missing"));

        // 6. Pressing "continue" does not revive it. This was the last hole: `resume` used
        //    to go straight to `spawn_scheduler` without re-checking anything.
        match crate::team::resume(s.app(), &s.tid).await {
            Err(LcError::Conflict(v)) => assert_eq!(v["reason"], "the team's worktrees are not usable"),
            other => panic!("resume should refuse an unsafe layout, got {other:?}"),
        }

        // 7. Starting a member does not open a pane in the user's checkout either.
        let err = format!("{:?}", crate::lifecycle::start_bot(s.app(), &d1.id).await.unwrap_err());
        assert!(err.contains("inside the project checkout"), "{err}");

        // And the point of all of it: the user's work is exactly as they left it.
        assert_eq!(git(&s.e.repo, &["status", "--porcelain"]), dirty_before);
        assert_eq!(std::fs::read_to_string(s.e.repo.join("wip.txt")).unwrap(), "使用者還沒 commit 的東西\n");
    }

    /// The containment test itself — the cases a naive `starts_with` gets wrong.
    #[test]
    fn path_containment_is_by_component_not_by_prefix() {
        use crate::team::is_within;
        assert!(is_within("/a/b", "/a/b"));
        assert!(is_within("/a/b", "/a/b/c"));
        assert!(is_within("/a/b/", "/a/b/c/"));
        assert!(!is_within("/a/b", "/a/bc"), "`/a/bc` is not inside `/a/b`");
        assert!(!is_within("/a/b", "/a"));
        assert!(!is_within("/a/b", "/x/y"));
        assert!(!is_within("", "/a/b"));
        assert!(!is_within("/a/b", ""));
    }

    /// §6.5 — `check_worktrees` used to accept a member whose cwd was the main checkout,
    /// because the main checkout is the first line of `git worktree list`.
    /// 2026-09-08 issue #56：`create` 先 INSERT、稍後才 `workspace.create`，中間被別的 herdr
    /// 事件觸發的 reconcile 掃到，team 一出生就 `paused(workspace_missing)`。建立中的 team
    /// （starting 且還沒有 workspace）reconcile 要放過。
    #[tokio::test]
    async fn reconcile_leaves_a_team_that_is_still_being_created_alone() {
        let s = S::new(1, false).await;
        sqlx::query("UPDATE teams SET phase = 'starting', workspace_id = NULL WHERE id = ?")
            .bind(&s.tid)
            .execute(&s.app().db)
            .await
            .unwrap();
        crate::team::reconcile_teams_on_host(s.app(), crate::config::LOCAL_HOST).await;
        let t = s.team().await;
        assert_eq!(t.phase, "starting");
        assert_eq!(t.pause_reason, None);
    }

    #[tokio::test]
    async fn the_main_checkout_does_not_count_as_a_team_worktree() {
        let s = S::new(1, false).await;
        sqlx::query("UPDATE bots SET cwd = ? WHERE team_id = ? AND team_role = 'worker'")
            .bind(s.e.repo.to_string_lossy().to_string())
            .bind(&s.tid)
            .execute(&s.app().db)
            .await
            .unwrap();
        // The per-host reconcile — the one `reconcile_host` actually runs, not just the boot
        // path — catches it.
        crate::team::reconcile_teams_on_host(s.app(), crate::config::LOCAL_HOST).await;
        let t = s.team().await;
        assert_eq!(t.phase, "paused");
        assert_eq!(t.pause_reason.as_deref(), Some("worktree_missing"));
    }

    /// #31：scheduler 一回合裡拿的是開頭載入的 `Ctx`，中間 await（停成員、跑 git、送 relay）
    /// 的時候使用者按了 abort。舊快照說 `planning`，於是 guard 的 `pause` 和收尾的 `end_team`
    /// 照寫——把 `aborted` 蓋回 `paused` / `done`，終態的 team 就這樣活回來。
    #[tokio::test]
    async fn a_stale_snapshot_cannot_overwrite_an_abort() {
        let s = S::new(1, true).await;
        let stale = s.ctx().await;
        assert_eq!(stale.team.phase, "planning");
        crate::team::abort(s.app(), &s.tid, None).await.unwrap();
        assert_eq!(s.team().await.phase, "aborted");

        pause(s.app(), &stale.team, "budget_time").await.unwrap();
        let t = s.team().await;
        assert_eq!(t.phase, "aborted", "a guard on a stale snapshot must not pause an aborted team");
        assert_eq!(t.pause_reason, None);

        end_team(s.app(), &stale).await.unwrap();
        hand_issue_to_pm(s.app(), &s.tid, false, None).await.unwrap();
        let pm = stale.pm().unwrap().clone();
        pm_done(s.app(), &stale, &pm, "做完了", false, None).await.unwrap();
        assert_eq!(s.team().await.phase, "aborted", "nothing the scheduler writes may leave `aborted`");
        // No phase event after the abort's own `aborting → aborted`.
        let last: Value = serde_json::from_str(
            &sqlx::query_scalar::<_, String>(
                "SELECT payload_json FROM team_events WHERE team_id = ? AND kind = 'phase' ORDER BY seq DESC LIMIT 1",
            )
            .bind(&s.tid)
            .fetch_one(&s.app().db)
            .await
            .unwrap(),
        )
        .unwrap();
        assert_eq!((last["from"].as_str(), last["to"].as_str()), (Some("aborting"), Some("aborted")));
    }

    /// #31 的另一半：使用者按了暫停，舊快照的 `dispatch` / `pm_done` 把 phase 寫成
    /// `working` / `finishing`，暫停就這樣被吃掉，而且 `pause_reason = user` 也跟著消失。
    /// 暫停要留著；scheduler 走到的進度記進 `resume_phase`，按「繼續」才回得到對的地方。
    #[tokio::test]
    async fn a_stale_snapshot_cannot_lift_a_user_pause() {
        let s = S::new(1, true).await;
        let pm = s.bot("pm", 0).await;
        let stale = s.ctx().await;
        crate::team::pause(s.app(), &s.tid).await.unwrap();

        let items = match Action::parse("pm", &json!({"action":"dispatch","tasks":[{"title":"A","brief":"做 A"}]})) {
            Ok(Action::Dispatch(v)) => v,
            other => panic!("{other:?}"),
        };
        dispatch(s.app(), &stale, &pm, items).await.unwrap();
        let t = s.team().await;
        assert_eq!(t.phase, "paused", "dispatch on a stale snapshot must not lift a user pause");
        assert_eq!(t.pause_reason.as_deref(), Some("user"));
        assert_eq!(t.resume_phase.as_deref(), Some("working"), "resume goes where the scheduler got to");
        assert_eq!(s.tasks().await.len(), 1, "the task itself is on record");

        // A guard firing on the same stale snapshot keeps the first reason.
        pause(s.app(), &stale.team, "budget_time").await.unwrap();
        assert_eq!(s.team().await.pause_reason.as_deref(), Some("user"));

        crate::team::resume(s.app(), &s.tid).await.unwrap();
        assert_eq!(s.team().await.phase, "working");
    }

    /// Every file under a directory, with its contents — the `find -newer` of §13's T8, but
    /// exact rather than timestamp-based.
    fn tree_of(dir: &std::path::Path) -> Vec<(String, u64)> {
        fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, u64)>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                // `.git` holds the worktree registrations, which §6.2 explicitly accepts.
                if p.file_name().map(|n| n == ".git").unwrap_or(false) {
                    continue;
                }
                if p.is_dir() {
                    walk(&p, base, out);
                } else {
                    let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
                    out.push((rel, e.metadata().map(|m| m.len()).unwrap_or(0)));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }
}
