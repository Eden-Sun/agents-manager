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
//! # The seven loop guards of §4.5, and where each one lives
//!
//! | § | guard | here |
//! |---|---|---|
//! | topology  | star: pm↔worker, pm↔reviewer, reviewer→worker | [`Action::parse`] refuses an action the role may not use, and [`enqueue`] is only ever called from the transitions below — there is no worker→worker edge to take |
//! | state-driven | a relay follows a state change, not a sentence | [`advance_tasks`] sends per task state, and each send is paired with the state write, so one state cannot emit the same relay twice |
//! | relay budget | `max_relays`, repair prompts included | [`flush`] refuses to deliver past the cap → `paused(budget_relays)` |
//! | review rounds | `max_review_rounds` | [`Verdict::request_changes`] handling → `exhausted` + `paused(review_exhausted)` |
//! | dispatch cap | one open task per worker; no identical repeat | [`dispatch`] (plus the DB's partial unique index) |
//! | wall clock | `max_wall_clock_min` | [`gates`] |
//! | quota | `quota_stop_pct` per relay | [`gates`] and again inside [`flush`] |
//!
//! And the eighth rule that binds them: **every one of those pauses, never aborts** (§4.5
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
                    Ok(t) if t.team_id.as_deref() == Some(team_id.as_str()) && t.is_done() => {
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
    /// The queue entry being worked right now (§2.3).
    issue: Option<db::TeamIssue>,
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
        // §2.3: which entry of the issue queue the team is on. `None` only between issues.
        let issue = db::current_team_issue(&app.db, team_id).await.map_err(up)?;
        Ok(Ctx { team, project, members, budget, issue })
    }

    /// The current issue's id, for scoping tasks / relays / the budget to it.
    fn issue_id(&self) -> Option<&str> {
        self.issue.as_ref().map(|i| i.id.as_str())
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
    pub to: String,
    pub title: String,
    pub brief: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Dispatch(Vec<DispatchItem>),
    Wait,
    /// `keep_workers`: the PM's call on whether the next issue reuses this batch of
    /// executors (their context is still useful) or gets a fresh one.
    Done { summary: String, keep_workers: bool },
    AskUser { question: String },
    Abort { reason: String },
    Report { blocked: bool, summary: String, notes: String },
    Verdict { approve: bool, summary: String, must_fix: Vec<String> },
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
                    if to.is_empty() || brief.is_empty() {
                        return Err("每筆 task 都需要 to 與 brief".into());
                    }
                    out.push(DispatchItem {
                        to,
                        title: if title.is_empty() { "task".into() } else { title },
                        brief,
                        files: list(t, "files"),
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
                Ok(Action::Done { summary: s(v, "summary"), keep_workers: keep })
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
                let st = s(v, "status");
                if !["done", "blocked"].contains(&st.as_str()) {
                    return Err("report.status 必須是 done 或 blocked".into());
                }
                Ok(Action::Report { blocked: st == "blocked", summary: s(v, "summary"), notes: s(v, "notes") })
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

fn footer(role: &str) -> String {
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
    team::set_phase(app, &team.id, "paused", Some(reason), Some(&team.phase)).await?;
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
    let tasks = issue_tasks(app, &ctx.team.id, ctx.issue_id()).await?;
    let rounds: i64 = tasks.iter().map(|t| t.round).sum();
    let elapsed = elapsed_min_of(&ctx.team, ctx.issue.as_ref());
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
fn elapsed_min_of(t: &db::Team, issue: Option<&db::TeamIssue>) -> i64 {
    let start = issue
        .and_then(|i| i.started_at.as_deref())
        .or(t.started_at.as_deref())
        .unwrap_or(t.created_at.as_str());
    let Ok(s) = chrono::DateTime::parse_from_rfc3339(start) else { return 0 };
    (chrono::Utc::now() - s.with_timezone(&chrono::Utc)).num_minutes().max(0)
}

// ---------------------------------------------------------------- gates (§4.5, §9.2)

/// The quota reading for one member's kind (`kind` or `kind:<identity>`) **on the bot's
/// host** (SPEC §14), if there is one.
async fn quota_pct(app: &Arc<App>, bot: &db::Bot) -> Option<f64> {
    let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
    let q = app.quotas.lock().await;
    let keys: Vec<String> = match bot.identity.as_deref().filter(|s| !s.is_empty()) {
        Some(i) => vec![format!("{}:{i}", bot.kind), bot.kind.clone()],
        None => vec![bot.kind.clone()],
    }
    .iter()
    .map(|base| crate::quota::quota_key(&host, base))
    .collect();
    for k in keys {
        if let Some(quota) = q.get(&k) {
            return [&quota.five_hour, &quota.seven_day]
                .into_iter()
                .flatten()
                .map(|w| w.used_pct)
                .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))));
        }
    }
    None
}

/// Wall clock and quota, checked on every pass. Returns `true` when the team may proceed.
async fn gates(app: &Arc<App>, ctx: &Ctx) -> LcResult<bool> {
    if elapsed_min_of(&ctx.team, ctx.issue.as_ref()) >= ctx.budget.max_wall_clock_min {
        pause(app, &ctx.team, "budget_time").await?;
        return Ok(false);
    }
    for b in &ctx.members {
        if let Some(pct) = quota_pct(app, b).await {
            if pct >= ctx.budget.quota_stop_pct {
                pause(app, &ctx.team, "quota_low").await?;
                return Ok(false);
            }
        }
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
        let sent = relay_count(app, &ctx.team.id, ctx.issue_id(), "delivered").await;
        if sent + pending.len() as i64 > ctx.budget.max_relays {
            pause(app, &ctx.team, "budget_relays").await?;
            return Ok(());
        }
        // §4.5 quota, re-checked immediately before the send.
        if let Some(pct) = quota_pct(app, bot).await {
            if pct >= ctx.budget.quota_stop_pct {
                pause(app, &ctx.team, "quota_low").await?;
                return Ok(());
            }
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

/// The task a worker is currently on. The DB's partial unique index guarantees there is at
/// most one (§4.5 dispatch cap).
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

/// Phase `starting`: bring the members up one at a time, then hand the issue to the PM.
/// Re-entrant — a member that is already running is left alone, which is what makes this
/// safe to call again after a restart.
pub async fn startup(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let ctx = Ctx::load(app, team_id).await?;
    if ctx.team.phase != "starting" {
        return Ok(());
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
            let msg = format!("{e:?}");
            note(app, team_id, json!({"action": "member_start_failed", "bot": b.name, "error": msg})).await?;
            // Also say it where the user is actually looking. Without this the only symptom is
            // a grey lamp and `paused(member_lost)`: the reason lived in the team log alone,
            // and the panel does not render a note that carries no `text`.
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
            // §7.4: no PM, no team. A worker short is survivable; a missing reviewer is a
            // decision for the human ("merge unreviewed" or "try again").
            match b.team_role.as_deref() {
                // §7.4: no PM, no team — and §6.5 sends a failed creation through the same
                // cleanup, so nothing is left half-built.
                Some("pm") => {
                    team::set_phase(app, team_id, "failed", None, None).await?;
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
        team::set_phase(app, team_id, "failed", None, None).await?;
        return Ok(());
    }

    sqlx::query("UPDATE teams SET started_at = COALESCE(started_at, ?) WHERE id = ?")
        .bind(db::now())
        .bind(team_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    team::set_phase(app, team_id, "planning", None, None).await?;

    // Appendix A.1, the first relay. `from` is NULL, so the timeline reads `daemon → pm`.
    let ctx = Ctx::load(app, team_id).await?;
    let Some(pm) = ctx.pm() else { return Ok(()) };
    let names: Vec<String> = ctx.workers().iter().map(|w| ctx.short(w)).collect();
    let text = format!(
        "Issue #{n}「{title}」。全文在 `.agents-manager/team/ISSUE.md`（在你的 cwd 內）。\
         目前有 {k} 位執行者可派：{who}。請先讀取 `.agents-manager/team/ISSUE.md` 與 `TEAM.md` 再派工。",
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
    let tasks = issue_tasks(app, team_id, ctx.issue_id()).await?;

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
        Some(rev) if !tasks.iter().any(|t| t.state == "reviewing") => {
            if let Some(t) = tasks.iter().find(|t| t.state == "reported") {
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
                    integ = ctx.team.branch,
                    report = t.last_report.clone().unwrap_or_default(),
                );
                enqueue(app, team_id, Some(&t.worker_bot_id), &rev.id, Some(&t.id), "review", text).await?;
                changed = true;
            }
        }
        Some(_) => {}
    }

    // 2. merge whatever is approved — SPEC-team §6.1 #3, by the daemon, with git.
    for t in issue_tasks(app, team_id, ctx.issue_id()).await? {
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
        changed = true;
    }
    Ok(changed)
}

/// One `git merge --no-ff` and everything that follows from its two outcomes.
/// `false` = the team was paused and the caller must stop.
async fn merge_one(app: &Arc<App>, ctx: &Ctx, t: &db::TeamTask) -> LcResult<bool> {
    let main_wt = ctx.main_wt()?;
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
                let text = format!("t{} 「{}」已合併進 `{}`（{}）。", t.seq, t.title, ctx.team.branch, &sha[..sha.len().min(8)]);
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
                pause(app, &ctx.team, "merge_conflict").await?;
                return Ok(false);
            }
            // §6.1 #4: whoever wrote the code resolves the conflict, in their own worktree.
            let text = format!(
                "整合分支 `{integ}` 已前進，你的 t{seq} 合併時衝突（{files}）。\
                 請在你的 worktree 執行 `git rebase {integ}`，解掉衝突並確認可建置後再 `report`。",
                integ = ctx.team.branch,
                seq = t.seq,
                files = if files.is_empty() { "見 git 輸出".into() } else { files.join("、") },
            );
            enqueue(app, &ctx.team.id, None, &t.worker_bot_id, Some(&t.id), "rebase", text).await?;
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------- replies (§4.4 → §8.2)

/// A member's turn ended: find its reply and run it through the protocol.
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
    if ev_kind.as_deref() != Some("relay") {
        return step(app, team_id).await;
    }
    let text = sqlx::query_scalar::<_, String>(
        "SELECT content FROM messages WHERE turn_id = ? AND role = 'assistant' ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(turn_id)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?
    .unwrap_or_default();
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
        Action::Done { summary, keep_workers } => pm_done(app, &ctx, &bot, &summary, keep_workers).await,
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

async fn dispatch(app: &Arc<App>, ctx: &Ctx, pm: &db::Bot, items: Vec<DispatchItem>) -> LcResult<()> {
    let mut rejected: Vec<String> = Vec::new();
    let mut created: Vec<(String, String)> = Vec::new(); // (task_id, worker short)
    let existing = issue_tasks(app, &ctx.team.id, ctx.issue_id()).await?;
    // `team_tasks_seq` is unique per **team**, not per issue: the second issue's first task
    // must continue the numbering (t2, t3…) or the INSERT below collides with t1 of the
    // first issue and the PM's whole dispatch is lost.
    let mut next_seq = db::team_tasks(&app.db, &ctx.team.id).await.map_err(up)?.iter().map(|t| t.seq).max().unwrap_or(0);
    // §6.3: overlapping `files` are a warning to the PM, never a refusal.
    let mut overlaps: Vec<String> = Vec::new();

    for it in items {
        let want = it.to.trim().trim_start_matches('@').to_ascii_lowercase();
        let Some(worker) = ctx
            .workers()
            .into_iter()
            .find(|w| {
                w.id == it.to || w.name.to_ascii_lowercase() == want || ctx.short(w).to_ascii_lowercase() == want
            })
            .cloned()
        else {
            rejected.push(format!("`{}` 不是這個 team 的執行者", it.to));
            continue;
        };
        // §4.5 dispatch cap: one open task per worker.
        if open_task_of(app, &ctx.team.id, &worker.id).await?.is_some() {
            rejected.push(format!("`{}` 還有未完成的 task", ctx.short(&worker)));
            continue;
        }
        // §4.5: the same brief to the same worker twice is a loop, not a plan.
        if existing.iter().any(|t| t.worker_bot_id == worker.id && t.brief.trim() == it.brief.trim()) {
            note(app, &ctx.team.id, json!({"action": "pm_repeat", "to": ctx.short(&worker), "brief": it.brief}))
                .await?;
            pause(app, &ctx.team, "pm_repeat").await?;
            return Ok(());
        }
        for other in existing.iter().filter(|t| !["merged", "skipped", "failed"].contains(&t.state.as_str())) {
            let of: Vec<String> = serde_json::from_str(&other.files_json).unwrap_or_default();
            for f in &it.files {
                if of.iter().any(|x| x == f) {
                    overlaps.push(format!("t{} 與新的 task 都列了 {f}", other.seq));
                }
            }
        }

        next_seq += 1;
        let short = ctx.short(&worker);
        let branch = team::task_branch(&ctx.team.branch, next_seq, &short);
        // §6.2: cut from the integration branch **now**, so this task already contains
        // everything merged before it.
        let worker_wt = ctx.wt(&worker)?;
        if let Err(e) = tg::checkout_task_branch(app, ctx.host(), &worker_wt, &branch, &ctx.team.branch).await {
            note(app, &ctx.team.id, json!({"action": "branch_failed", "branch": branch, "error": e.to_string()}))
                .await?;
            pause(app, &ctx.team, "upstream").await?;
            return Ok(());
        }
        let task_id = db::ulid();
        let now = db::now();
        sqlx::query(
            "INSERT INTO team_tasks (id, team_id, issue_id, seq, title, brief, files_json, worker_bot_id, branch,
               state, round, rebase_attempts, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,'queued',0,0,?,?)",
        )
        .bind(&task_id)
        .bind(&ctx.team.id)
        .bind(ctx.issue_id())
        .bind(next_seq)
        .bind(&it.title)
        .bind(&it.brief)
        .bind(serde_json::to_string(&it.files).unwrap_or_else(|_| "[]".into()))
        .bind(&worker.id)
        .bind(&branch)
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .map_err(up)?;
        emit_task(app, &task_id).await;

        let text = format!(
            "Task t{seq}「{title}」（分支 `{branch}` 已建好並 checkout 在你的 cwd）。\n\n{brief}\n\n\
             相關檔案：{files}\n\
             只在你的 cwd 工作：不要 cd 出去、不要動 ../、不要 git push、不要切換分支。\
             每個邏輯段落 `git commit`，完成後用 `report` 回報。",
            seq = next_seq,
            title = it.title,
            branch = branch,
            brief = it.brief,
            files = if it.files.is_empty() { "（未指定）".into() } else { it.files.join("、") },
        );
        enqueue(app, &ctx.team.id, Some(&pm.id), &worker.id, Some(&task_id), "dispatch", text).await?;
        created.push((task_id, short));
    }

    if !created.is_empty() && ctx.team.phase != "working" {
        team::set_phase(app, &ctx.team.id, "working", None, None).await?;
    }
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
    // §4.6 gate:dispatch — hold the freshly queued relays until a human says go.
    if !created.is_empty() && !gate_open(app, &ctx.team, "dispatch").await {
        let ctx2 = Ctx::load(app, &ctx.team.id).await?;
        hold_gate(app, &ctx2, "dispatch", json!({"tasks": created.iter().map(|c| &c.1).collect::<Vec<_>>()})).await?;
    }
    Ok(())
}

/// §8.3: a PM that says `wait` when there is nothing left to wait for gets one nudge, once.
async fn wait(app: &Arc<App>, ctx: &Ctx) -> LcResult<()> {
    let tasks = issue_tasks(app, &ctx.team.id, ctx.issue_id()).await?;
    if tasks.is_empty() || !tasks.iter().all(|t| ["merged", "skipped", "failed"].contains(&t.state.as_str())) {
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

/// §8.3: the PM declares completion, the daemon verifies it.
async fn pm_done(app: &Arc<App>, ctx: &Ctx, pm: &db::Bot, summary: &str, keep_workers: bool) -> LcResult<()> {
    let tasks = issue_tasks(app, &ctx.team.id, ctx.issue_id()).await?;
    let open: Vec<String> = tasks
        .iter()
        .filter(|t| !["merged", "skipped", "failed"].contains(&t.state.as_str()))
        .map(|t| format!("t{}（{}）", t.seq, t.state))
        .collect();
    if !open.is_empty() {
        enqueue(
            app,
            &ctx.team.id,
            None,
            &pm.id,
            None,
            "reject",
            format!("還不能 `done`：{} 尚未進入終態。請等回報或用 `wait`。", open.join("、")),
        )
        .await?;
        return Ok(());
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
    team::set_phase(app, &ctx.team.id, "finishing", None, None).await?;
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
                "`{who}` t{seq}「{title}」→ `blocked`：{why}\n{table}\n請決定下一步（`dispatch` / `wait` / `done` / `ask_user`）。",
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
    match tg::commits_ahead(app, ctx.host(), &wt, &ctx.team.branch).await {
        Ok(0) => {
            return repair(
                app,
                ctx,
                worker,
                &format!("你的分支 `{}` 相對 `{}` 沒有任何 commit", task.branch, ctx.team.branch),
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
    // The PM only ever sees the issue it is working on (§2.3).
    let issue = db::current_team_issue(&app.db, team_id).await.ok().flatten();
    let tasks = issue_tasks(app, team_id, issue.as_ref().map(|i| i.id.as_str())).await.unwrap_or_default();
    if tasks.is_empty() {
        return String::new();
    }
    let rows: Vec<String> = tasks.iter().map(|t| format!("t{}「{}」= {}", t.seq, t.title, t.state)).collect();
    format!("目前 task 狀態：{}", rows.join("；"))
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
    let tasks = issue_tasks(app, &ctx.team.id, ctx.issue_id()).await?;
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
        pause(app, &ctx.team, "review_exhausted").await?;
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
    enqueue(app, &ctx.team.id, Some(&rev.id), &task.worker_bot_id, Some(&task.id), "rework", text).await?;
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
    enqueue(app, &team.id, None, &task.worker_bot_id, Some(&task.id), "rework", text).await.map(|_| ())
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
                let title = format!("{} (#{})", ctx.team.issue_title, ctx.team.issue_number);
                let body = format!("{summary}\n\nCloses #{}", ctx.team.issue_number);
                match tg::deliver_pr(
                    app,
                    ctx.host(),
                    &ctx.main_wt()?,
                    ctx.team.worktree_root.trim_end_matches('/'),
                    &ctx.team.branch,
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
                        if let Some(iid) = ctx.issue_id() {
                            let _ = sqlx::query("UPDATE team_issues SET pr_url = ? WHERE id = ?")
                                .bind(&url)
                                .bind(iid)
                                .execute(&app.db)
                                .await;
                        }
                        note(app, &ctx.team.id, json!({"action": "pr_created", "url": url, "pushed": ctx.team.branch}))
                            .await?;
                    }
                    Err(e) => {
                        note(app, &ctx.team.id, json!({"action": "deliver_failed", "error": e.to_string()})).await?;
                        pause(app, &ctx.team, "deliver_failed").await?;
                        return Ok(());
                    }
                }
            }
        }
    } else {
        // §6.4 `branch`: the work stays local. Nothing is pushed, by the user's ruling.
        note(app, &ctx.team.id, json!({"action": "delivered", "deliver": "branch",
             "branch": ctx.team.branch, "pushed": false, "summary": summary}))
        .await?;
    }
    close_issue_and_advance(app, &ctx.team.id, "done", None, Some(summary.as_str())).await
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
    let Some(current) = ctx.issue.clone() else {
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
    hand_issue_to_pm(app, team_id, keep).await
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
    team::set_phase(app, &ctx.team.id, "done", None, None).await?;
    Ok(())
}

/// Appendix A.1's first relay, re-sent for each issue in the queue. The PM is the same agent
/// across the whole queue, so this is the message that tells it the subject changed — and that
/// its executors are different people now.
async fn hand_issue_to_pm(app: &Arc<App>, team_id: &str, kept_workers: bool) -> LcResult<()> {
    team::set_phase(app, team_id, "planning", None, None).await?;
    let ctx = Ctx::load(app, team_id).await?;
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
        format!("執行者照你的決定沿用上一個 issue 那批：{who}（共 {k} 位），他們記得先前的工作。", who = names.join("、"), k = names.len())
    } else {
        format!("執行者換了一批，現在可派的是：{who}（共 {k} 位）。", who = names.join("、"), k = names.len())
    };
    let text = format!(
        "換下一個 issue：#{n}「{title}」。全文在 `.agents-manager/team/ISSUE.md`（已更新）。{who_line}\
         請重新讀取 `.agents-manager/team/ISSUE.md` 與 `TEAM.md` 再派工，先前 issue 的 task 一律不要再提。{tail}",
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
        assert_eq!(Action::parse("pm", &v).unwrap(), Action::Done { summary: "ok".into(), keep_workers: false });
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

    fn dispatch2() -> Value {
        json!({"action":"dispatch","tasks":[
            {"to":"dev-1","title":"A","brief":"做 A","files":["a.txt"]},
            {"to":"dev-2","title":"B","brief":"做 B","files":["b.txt"]}]})
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
        assert_eq!(tasks[0].worker_bot_id, d1.id);
        assert_eq!(tasks[1].worker_bot_id, d2.id);
        assert_eq!(tasks[0].branch, "team/i42-".to_string() + &crate::team::tid6(&s.tid) + "-t1-dev-1");
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
        assert!(branches.contains("-t1-dev-1"));
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

    /// §4.5 dispatch cap — one open task per worker, and never the same brief twice.
    #[tokio::test]
    async fn the_dispatch_cap_refuses_seconds_and_repeats() {
        let s = S::new(1, false).await;
        let pm = s.bot("pm", 0).await;
        let one = |brief: &str| {
            json!({"action":"dispatch","tasks":[{"to":"dev-1","title":"A","brief":brief}]})
        };
        s.reply(&pm, one("做 A")).await;
        assert_eq!(s.tasks().await.len(), 1);

        // A second task for a busy worker is refused, and the PM is told why.
        s.reply(&pm, one("做 B")).await;
        assert_eq!(s.tasks().await.len(), 1, "still one");
        let p = s.pending(&pm).await;
        let last: Value = serde_json::from_str(&p.last().unwrap().payload_json).unwrap();
        assert!(last["text"].as_str().unwrap().contains("還有未完成的 task"));
        // An unknown recipient is refused the same way, not silently dropped.
        s.reply(&pm, json!({"action":"dispatch","tasks":[{"to":"dev-9","title":"C","brief":"做 C"}]})).await;
        let p = s.pending(&pm).await;
        let last: Value = serde_json::from_str(&p.last().unwrap().payload_json).unwrap();
        assert!(last["text"].as_str().unwrap().contains("dev-9"));

        // The identical brief again, once the worker is free, is a loop → pause.
        set_task_state(s.app(), &s.tasks().await[0].id, "merged").await.unwrap();
        s.reply(&pm, one("做 A")).await;
        let t = s.team().await;
        assert_eq!(t.pause_reason.as_deref(), Some("pm_repeat"));
    }

    /// §8.3 — `done` is verified, not taken on trust, and `wait` with nothing left nudges once.
    #[tokio::test]
    async fn done_is_checked_and_wait_nudges_once() {
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
        assert_eq!(s.pending(&pm).await.len(), n, "only one nudge, not a nudge loop");

        s.reply(&pm, json!({"action":"done","summary":"真的完成了"})).await;
        assert_eq!(s.team().await.phase, "finishing");
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
