//! Issue Team (`docs/SPEC-team.md`) — data layer, REST surface and WS events.
//!
//! What is here (stage 1 items 1, 2, 4, 5 of SPEC-team §13):
//!
//! * the `teams` / `team_tasks` / `team_events` rows and the JSON shapes of §10;
//! * creating a team: validation, the DB record, and the member Bots (ordinary `bots` rows
//!   carrying `managed_by='team'`, so every existing start / stop / prompt / hook / reconcile
//!   path works on them unchanged — SPEC-team §5.2);
//! * the control endpoints (pause / resume / approve / abort / cleanup / PATCH / say /
//!   answer / decide) and the three WS events.
//!
//! The scheduler event loop, the `am-team` protocol and the task state machine live next
//! door in `team_sched.rs`; every git and `gh` command lives in `team_git.rs`. This module
//! owns creation (including the §6.2 worktree layout), the reads, and the control endpoints.

use crate::config::LOCAL_HOST;
use crate::db;
use crate::lifecycle::{self, LcError, LcResult};
use crate::state::App;
use crate::team_git as tg;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

// ---------------------------------------------------------------- constants

/// SPEC-team §12 #7: one reviewer serialises review, so more than four workers only queue.
pub const MAX_WORKERS: u32 = 4;
/// SPEC-team §2.3: how many issues one team may have queued at once. The cap exists because
/// the PM keeps its context for the whole queue — a very long queue would run it out of
/// window long before the daemon ran out of anything.
pub const MAX_QUEUED_ISSUES: usize = 20;
/// SPEC-team §7.1 (2026-09-08): this is the **併行數** — how many tasks may run at the same
/// time — not a headcount the PM has to plan around. One by default: extra parallelism costs
/// extra quota, so it stays an explicit choice.
pub const DEFAULT_WORKER_COUNT: u32 = 1;

/// SPEC-team §4.5 (2026-09-09): `workers.count = 0` means **unlimited** — every issue on the
/// queue is worked at the same time and the executor count grows with what the PM dispatches.
/// `0` is a new value of a field that has always been 1–4; those keep their old meaning
/// exactly (one issue at a time, a fixed pool).
pub const UNLIMITED_WORKERS: u32 = 0;
/// Unlimited is unlimited in intent, not in panes. Six issues in flight is already twelve
/// worktrees and a herdr workspace nobody can read; the seventh waits for one to finish.
pub const MAX_CONCURRENT_ISSUES: usize = 6;
/// …and the whole team stops at twelve executors, whatever the per-issue arithmetic says.
/// This is a pane and quota ceiling, not a budget: the relay budget is counted separately.
pub const MAX_TEAM_WORKERS: u32 = 12;

pub const DEFAULT_BASE_REF: &str = "HEAD";

/// SPEC-team §12 #1 — **user decision, overriding the proposal's `pr`**: `git push` to
/// origin is an outward-facing, hard-to-take-back action, so a team leaves a local branch
/// unless a PR was explicitly asked for. Both values remain supported.
pub const DEFAULT_DELIVER: &str = "branch";
pub const DELIVERS: [&str; 2] = ["branch", "pr"];

/// SPEC-team §12 #3: full-auto by default; the supervised gates are opt-in.
pub const DEFAULT_SUPERVISED: bool = false;

/// SPEC-team §8.1. `paused` keeps the phase it will return to in `resume_phase`.
pub const TERMINAL_PHASES: [&str; 3] = ["done", "aborted", "failed"];
/// Task states a user `decide` may act on (SPEC-team §10.5).
pub const DECIDABLE_STATES: [&str; 3] = ["exhausted", "blocked_by_worker", "rebasing"];
/// The pauses a `decide` clears: they are the ones a decidable task puts the team into, so
/// the decision has to lift them too (SPEC-team §8.2). Any other pause — a user pause, a
/// budget or quota stop — stays exactly where it is.
pub const DECISION_PAUSES: [&str; 3] = ["merge_conflict", "review_exhausted", "pm_abort"];

pub fn is_terminal(phase: &str) -> bool {
    TERMINAL_PHASES.contains(&phase)
}

/// SPEC-team §2.3: the `team_issues` states that still hold a place in the queue — waiting to
/// start, or being worked right now. The other three (`done` / `failed` / `skipped`) are
/// finished records: they stay in the log for good, but they no longer reserve the issue
/// number, so the same issue can be queued again as a later `seq`.
pub fn is_open_issue_state(state: &str) -> bool {
    state == "queued" || state == "working"
}

// ---------------------------------------------------------------- budget

/// The team's stored parallelism, as `roles_json` holds it. `0` = unlimited (§4.5).
pub fn worker_count_of(t: &db::Team) -> u32 {
    serde_json::from_str::<Value>(&t.roles_json)
        .ok()
        .and_then(|r| r.get("workers").and_then(|w| w.get("count")).and_then(|c| c.as_u64()))
        .unwrap_or(DEFAULT_WORKER_COUNT as u64)
        .min(MAX_WORKERS as u64) as u32
}

/// §4.5: is this team in unlimited mode? Every new code path added for unlimited parallelism
/// branches on this and on nothing else, so a team with a parallelism of 1–4 never reaches
/// any of it.
pub fn is_unlimited(t: &db::Team) -> bool {
    worker_count_of(t) == UNLIMITED_WORKERS
}

/// The executors that belong to one issue: their §6.2 directory (and therefore their §7.3
/// nickname) carries the issue's queue position, which is what keeps two issues' `dev-1`
/// apart while both are in flight.
pub fn workers_of_issue<'a>(members: &'a [db::Bot], team_id: &str, issue_seq: i64) -> Vec<&'a db::Bot> {
    let prefix = format!("t{}-i{issue_seq}-dev-", tid6(team_id));
    members
        .iter()
        .filter(|b| {
            b.deleted_at.is_none() && b.team_role.as_deref() == Some("worker") && b.name.starts_with(&prefix)
        })
        .collect()
}

/// SPEC-team §9.2 / §12 #2 — the defaults are the user's ruling, not a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub max_relays: i64,
    pub max_review_rounds: i64,
    pub max_wall_clock_min: i64,
    pub quota_stop_pct: f64,
}

impl Default for Budget {
    fn default() -> Self {
        Self { max_relays: 40, max_review_rounds: 2, max_wall_clock_min: 120, quota_stop_pct: 90.0 }
    }
}

/// Every field optional: used both by `POST /teams` (`budget`) and `PATCH /teams/:id`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BudgetPatch {
    #[serde(default)]
    pub max_relays: Option<i64>,
    #[serde(default)]
    pub max_review_rounds: Option<i64>,
    #[serde(default)]
    pub max_wall_clock_min: Option<i64>,
    #[serde(default)]
    pub quota_stop_pct: Option<f64>,
}

impl Budget {
    /// Parse a stored `budget_json`; a corrupt value falls back to the defaults rather than
    /// failing a read of the whole team.
    pub fn from_json(s: &str) -> Budget {
        serde_json::from_str(s).unwrap_or_default()
    }

    /// Apply a partial patch, rejecting values that would make the loop guards useless.
    pub fn apply(&mut self, p: &BudgetPatch) -> Result<(), String> {
        if let Some(v) = p.max_relays {
            if !(1..=1000).contains(&v) {
                return Err("max_relays must be between 1 and 1000".into());
            }
            self.max_relays = v;
        }
        if let Some(v) = p.max_review_rounds {
            if !(0..=20).contains(&v) {
                return Err("max_review_rounds must be between 0 and 20".into());
            }
            self.max_review_rounds = v;
        }
        if let Some(v) = p.max_wall_clock_min {
            if !(1..=10_080).contains(&v) {
                return Err("max_wall_clock_min must be between 1 and 10080".into());
            }
            self.max_wall_clock_min = v;
        }
        if let Some(v) = p.quota_stop_pct {
            if !v.is_finite() || !(1.0..=100.0).contains(&v) {
                return Err("quota_stop_pct must be between 1 and 100".into());
            }
            self.quota_stop_pct = v;
        }
        Ok(())
    }
}

/// `quota_stop_pct = 100` means "do not stop on quota at all" (2026-09-09): the user's way of
/// forcing a `paused(quota_low)` team on when the reading is stale or they simply want to
/// spend the last few percent. Every quota gate — create, reopen, scheduler — branches here.
pub fn quota_check_disabled(stop_pct: f64) -> bool {
    stop_pct >= 100.0
}

/// SPEC-team §9.2 — `usage_json`. Written by the scheduler; read by the UI's budget bar.
pub fn empty_usage() -> Value {
    json!({"relays": 0, "review_rounds_total": 0, "elapsed_min": 0, "per_bot": {}})
}

// ---------------------------------------------------------------- request shapes (§10.1)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleSpec {
    pub kind: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub fast: bool,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub persona_extra: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkersSpec {
    /// `0` = unlimited (§4.5), 1–4 = a fixed parallelism. Default 1.
    #[serde(default)]
    pub count: Option<u32>,
    pub kind: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub fast: bool,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default)]
    pub persona_extra: Option<String>,
}

impl WorkersSpec {
    pub fn role(&self) -> RoleSpec {
        RoleSpec {
            kind: self.kind.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            fast: self.fast,
            identity: self.identity.clone(),
            persona_extra: self.persona_extra.clone(),
        }
    }
    pub fn resolved_count(&self) -> u32 {
        self.count.unwrap_or(DEFAULT_WORKER_COUNT)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateTeam {
    /// SPEC-team §2.3: the issue queue, worked in this order. `issue_number` is the original
    /// single-issue form and is still accepted — a one-element queue.
    #[serde(default)]
    pub issue_numbers: Option<Vec<i64>>,
    #[serde(default)]
    pub issue_number: Option<i64>,
    pub pm: RoleSpec,
    pub workers: WorkersSpec,
    /// Explicit `null` = no reviewer; `reported → merging` directly (SPEC-team §8.2).
    #[serde(default)]
    pub reviewer: Option<RoleSpec>,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub deliver: Option<String>,
    #[serde(default)]
    pub supervised: Option<bool>,
    #[serde(default)]
    pub budget: Option<BudgetPatch>,
    /// Submodule path (relative to the project) whose issues this team works; empty/absent =
    /// the project itself. Must be a listed submodule with a GitHub origin.
    #[serde(default)]
    pub repo: Option<String>,
}

impl CreateTeam {
    pub fn repo_rel(&self) -> String {
        self.repo.as_deref().unwrap_or("").trim().trim_matches('/').to_string()
    }
    /// The requested queue, de-duplicated and in the order given. Rejects an empty request so
    /// a team can never exist with nothing to do.
    pub fn issue_list(&self) -> Result<Vec<i64>, String> {
        let mut out: Vec<i64> = Vec::new();
        for n in self.issue_numbers.clone().unwrap_or_default().into_iter().chain(self.issue_number) {
            if !out.contains(&n) {
                out.push(n);
            }
        }
        if out.is_empty() {
            return Err("issue_numbers must not be empty".into());
        }
        if out.len() > MAX_QUEUED_ISSUES {
            return Err(format!("at most {MAX_QUEUED_ISSUES} issues per team"));
        }
        Ok(out)
    }
}

/// The issue a team is built around, already resolved (via `gh`, or supplied by a test).
#[derive(Debug, Clone, PartialEq)]
pub struct IssueRef {
    pub number: i64,
    pub title: String,
    pub url: String,
    /// The issue body, verbatim. It becomes `ISSUE.md` and is never put in a prompt.
    pub body: String,
}

// ---------------------------------------------------------------- naming (§7.3, §6.2)

/// The team's short hash: the last 6 alphanumerics of its ULID, lowercased — the same trick
/// `config::agent_name` uses for bots.
pub fn tid6(team_id: &str) -> String {
    let tail: String = team_id
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        "team".into()
    } else {
        tail
    }
}

/// SPEC-team §6.2: the integration branch, `team/i<issue>-<tid6>`.
pub fn integration_branch(issue_number: i64, tid6: &str) -> String {
    integration_branch_pass(issue_number, tid6, 1)
}

/// The integration branch for the `pass`-th time this team takes on this issue number
/// (1-based). §2.3 lets a `done` / `failed` / `skipped` issue be queued again, and the branch
/// from the earlier pass is still there — branches are never deleted except by `delete`
/// (§6.5) — so `git checkout -b` on the same name would simply fail. The second and later
/// passes therefore get a `-r<pass>` suffix; the first keeps the plain §6.2 name, so nothing
/// that already exists is renamed.
pub fn integration_branch_pass(issue_number: i64, tid6: &str, pass: usize) -> String {
    if pass <= 1 {
        format!("team/i{issue_number}-{tid6}")
    } else {
        format!("team/i{issue_number}-{tid6}-r{pass}")
    }
}

/// The integration branch for one queue row: how many times this team has taken on that issue
/// number, counting this row, decides which pass it is.
async fn issue_branch(app: &Arc<App>, team_id: &str, q: &db::TeamIssue) -> LcResult<String> {
    let pass = db::team_issues(&app.db, team_id)
        .await
        .map_err(any_err)?
        .iter()
        .filter(|i| i.issue_number == q.issue_number && i.seq <= q.seq)
        .count();
    Ok(integration_branch_pass(q.issue_number, &tid6(team_id), pass))
}

/// A task branch, `<integration>-t<seq>-<worker short name>`.
///
/// **Deviation from SPEC-team §6.2, forced by git.** The spec asks for
/// `team/i42-k3f9x2/t1-dev-1` alongside an integration branch called `team/i42-k3f9x2`, and
/// git cannot hold both: refs are files, so `refs/heads/team/i42-k3f9x2` being a file makes
/// `refs/heads/team/i42-k3f9x2/t1-dev-1` impossible —
/// `fatal: cannot lock ref …: 'refs/heads/team/i42-k3f9x2' exists`.
///
/// The `/` is therefore a `-`. Everything else about the name is unchanged, the branches
/// still sort together under `team/`, and the integration branch keeps the name §6.2, §10.2
/// and the UI all use. (The alternative — moving the integration branch to
/// `team/i42-k3f9x2/main` — would have renamed the branch the user is handed at the end.)
///
/// The executor's short name used to be part of it. Since §4.5 became a worker pool the
/// branch is named when the task is *planned* and cut when it is *started*, and by then a
/// different executor may be the one free — so the task's own number is the whole identity.
pub fn task_branch(integration: &str, seq: i64) -> String {
    format!("{integration}-t{seq}")
}

/// SPEC-team §6.2: `<data_dir>/teams/<team_id>` — **outside** the repository, so the user's
/// checkout gains no files at all and its cleanliness does not depend on any ignore rule.
pub fn worktree_root(app: &Arc<App>, team_id: &str) -> String {
    app.data_dir.join("teams").join(team_id).to_string_lossy().to_string()
}

/// The directory name a member's worktree gets under the team root (§6.2).
///
/// PM and reviewer live at fixed paths for the whole life of the team — they are never
/// restarted, and `bots.cwd` is only read when a pane opens, so their directory cannot move.
/// Workers are replaced for every issue (SPEC-team §2.3), so their directory carries the
/// issue's queue position; otherwise issue #2's `dev-1` would collide with issue #1's.
pub fn member_dir(role: &str, index: u32, issue_seq: i64) -> String {
    match role {
        "pm" => "main".into(),
        "reviewer" => "reviewer".into(),
        _ => format!("i{issue_seq}-dev-{index}"),
    }
}

/// SPEC-team §7.3: a member's nickname inside the project — `tk3f9x2-pm`, `tk3f9x2-rev`,
/// `tk3f9x2-i1-dev-1`.
///
/// The prefix is the *team*, not the issue: a team outlives any one issue now, and a PM named
/// after issue #42 would still be called that while the team works issue #43. Workers add the
/// issue's queue position because each issue gets a fresh set of them and the names have to
/// stay unique within the project.
pub fn member_nick(tid6: &str, role: &str, index: u32, issue_seq: i64) -> String {
    match role {
        "pm" => format!("t{tid6}-pm"),
        "reviewer" => format!("t{tid6}-rev"),
        _ => format!("t{tid6}-i{issue_seq}-dev-{index}"),
    }
}

/// The name the `am-team` protocol uses (`pm`, `rev`, `dev-1`): the nickname with its team and
/// issue prefixes removed.
///
/// Both prefixes are optional so this also handles teams created before the issue queue, whose
/// members are named `i42-pm` / `i42-dev-1` — stripping a leading `i<digits>-` covers those
/// without needing to know the issue number.
pub fn short_name(nick: &str, team_id: &str) -> String {
    let rest = nick.strip_prefix(&format!("t{}-", tid6(team_id))).unwrap_or(nick);
    match rest.split_once('-') {
        Some((head, tail))
            if head.starts_with('i') && head.len() > 1 && head[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            tail.to_string()
        }
        _ => rest.to_string(),
    }
}

// ---------------------------------------------------------------- role personas (§7.1)

/// A short, path-free role persona — what a member is created with, before its worktree
/// exists. `full_persona` replaces it with the appendix A text once the layout is on disk.
fn role_persona(role: &str, issue_number: i64, extra: Option<&str>) -> Option<String> {
    let base = match role {
        // SPEC-team §2.3: the PM and the reviewer outlive any one issue, and a persona is only
        // applied when the agent starts — so neither may name an issue or a branch. Whatever
        // changes per issue is delivered as a file (`ISSUE.md`) and a relay instead.
        "pm" => (
            "你是這個 team 的 PM。你不寫程式、不 commit：把當前 issue 拆成互不重疊的 task 派給執行者，\
             收回報後決定下一步，全部完成後寫摘要。不確定就問使用者。\
             這個 team 會依序處理多個 issue，當前是哪一個一律以 `ISSUE.md` 為準。"
        )
        .to_string(),
        "reviewer" => (
            "你是這個 team 的 reviewer。唯讀：可以跑 build / test，不要 commit、不要改檔。\
             判斷變更是否正確、是否符合當前 issue、是否會破壞其他部分，打回票時要具體到檔案與行為。\
             當前 issue 一律以 `ISSUE.md` 為準。"
        )
        .to_string(),
        _ => format!(
            "你是 issue #{issue_number} 的執行者。只在指派給你的工作目錄裡工作：不要 cd 出去、不要動別人的目錄、\
             不要 git push、不要切換分支。只在自己的 worktree 工作；禁止 git stash / --autostash；只 git add 自己的檔案；\
             收尾前跑 scripts/check.sh。每個邏輯段落 git commit，完成後回報改了什麼、如何驗證。"
        ),
    };
    let extra = extra.map(str::trim).filter(|s| !s.is_empty());
    Some(match extra {
        Some(e) => format!("{base}\n\n{e}"),
        None => base,
    })
}

/// SPEC-team appendix A.1–A.3: the persona a member actually runs with — its own path, the
/// integration branch, and who else is on the team. Written after the worktrees exist,
/// because until then there are no paths to name.
#[allow(clippy::too_many_arguments)]
fn full_persona(
    role: &str,
    issue_number: i64,
    short: &str,
    cwd: &str,
    integration: &str,
    roster: &str,
    extra: Option<&str>,
    unlimited: bool,
) -> String {
    let base = match role {
        // Neither of these names the issue or the integration branch: both change when the
        // team moves to the next issue, and a persona is only read at agent start. `ISSUE.md`
        // and `TEAM.md` are rewritten in place at every issue boundary and carry the current
        // values; `{roster}` here is only the long-lived members.
        "pm" => format!(
            "你是這個 team 的 PM，暱稱 `{short}`。你不寫程式、不 commit。\
             你的 cwd `{cwd}` 是當前整合分支的 worktree，只當唯讀參考（你改了東西會讓整合停下來）。\n\
             長期成員：{roster}\n\
             工作：讀 `.agents-manager/team/ISSUE.md` 與 `.agents-manager/team/TEAM.md`（都在你的 cwd 內），\
             把當前 issue 拆成互不重疊（以檔案 / 模組切分）的 task，用 `dispatch` 派工——\
             **不用指定 `to`**，daemon 會把每筆 task 派給有空的執行者（併行數就是同時能跑幾筆）。\
             你可以**隨時再 `dispatch`**，不必等前一批做完：多出來的 task 會排隊，跑完一筆就補一筆。\
             沒事做就 `wait`；收到回報後決定下一步；所有 task 合併後 `done` 並寫摘要。不確定就 `ask_user`。\n\
             這個 team 會依序處理多個 issue：`done` 之後 daemon 會交派下一個 issue。\
             `done` 時由你決定執行者要不要換一批：`\"workers\": \"keep\"` 沿用這批（他們對程式碼的理解還有用、\
             而且對話還不算太長時），`\"workers\": \"replace\"` 換新的（他們的上下文已經很長或已經偏題時）；\
             屆時 `ISSUE.md` 與 `TEAM.md` 都會更新，換批時執行者的名字也會變，一律以檔案為準。\n\
             合併與分支由 daemon 用 git 處理，你不需要（也不可以）自己 merge。"
        ),
        "reviewer" => format!(
            "你是這個 team 的 reviewer，暱稱 `{short}`。你的 cwd `{cwd}` 會被 daemon checkout 到待審分支（detached HEAD）。\n\
             長期成員：{roster}\n\
             唯讀：可以跑 build / test，不要 commit、不要改檔、不要切換分支。\
             待審分支與當前整合分支都寫在 `.agents-manager/team/TEAM.md`，用 `git diff <整合分支>...HEAD` 看變更。\
             判斷是否正確、是否符合當前 issue、是否會破壞其他部分；\
             用 `verdict` 回覆，`request_changes` 時 `must_fix` 要具體到檔案與行為。"
        ),
        _ => format!(
            "你是 issue #{issue_number} 的執行者 `{short}`。你的 cwd `{cwd}` 是你專屬的 git worktree，\
             你只能在這裡工作：不要 `cd` 出去、不要動 `../`、不要進入使用者的主 checkout、\
             不要 `git push`、不要自己切換分支（分支由 daemon 建好並 checkout）。只在自己的 worktree 工作，\
             禁止 `git stash` / `--autostash`；只 `git add` 自己的檔案；收尾前跑 `scripts/check.sh`。\n\
             成員：{roster}\n\
             整合分支是 `{integration}`。每個邏輯段落 `git commit`。完成後用 `report` 回報：\
             `summary` 說明改了什麼、如何驗證。做不下去用 `status: blocked` 說明原因。\
             背景資料在 `.agents-manager/team/ISSUE.md`（在你的 cwd 內）。"
        ),
    };
    // §4.5: in unlimited mode the PM manages several issues at once, so the one thing its
    // persona must carry is that `issue` is a required field — the rest is in `TEAM.md`,
    // which is rewritten whenever the set of issues in flight changes.
    let base = if unlimited && role == "pm" {
        format!(
            "{base}\n\n這個 team 的併行數是**無限**：佇列裡的 issue 會同時開工，你會同時管好幾個。\
             因此 `dispatch` 的**每一筆 task 都必須寫 `issue`（issue 號）**，`done` 也必須寫 `issue`——\
             `done` 只交付那一個 issue，其他的照跑。哪些 issue 在進行中、各自的整合分支與執行者，\
             一律以 `.agents-manager/team/TEAM.md` 為準；每個 issue 的全文在 `ISSUE-<n>.md`。"
        )
    } else {
        base
    };
    match extra.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => format!("{base}\n\n{e}"),
        None => base,
    }
}

/// `ISSUE.md` — the issue's full text, written once so no relay ever has to carry it
/// (appendix A: "issue 全文永遠走各 worktree 內的 ISSUE.md，不塞進 prompt").
fn issue_md(issue: &IssueRef) -> String {
    format!(
        "# Issue #{n} — {title}\n\n{url}\n\n---\n\n{body}\n",
        n = issue.number,
        title = issue.title,
        url = issue.url,
        body = if issue.body.trim().is_empty() { "（這個 issue 沒有內文）" } else { issue.body.trim() },
    )
}

/// `TEAM.md` — who is who, where they live, and the protocol in one page.
fn team_md(issue: &IssueRef, branch: &str, root: &str, repo: &str, members: &[(String, String, String)]) -> String {
    let repo_line = if repo.is_empty() {
        String::new()
    } else {
        format!("- 這個 issue 屬於專案的 submodule `{repo}`：每個成員的 cwd 都是**該 submodule** 的 worktree，分支與 PR 也都在 submodule 的 repo\n")
    };
    let mut s = format!(
        "# Team · issue #{n}\n\n- 整合分支：`{branch}`\n- team 根目錄：`{root}`\n{repo_line}- 合併由 daemon 用 `git merge --no-ff` 執行，agent 不要自己合併\n\n## 成員\n\n| 短名 | 角色 | cwd |\n|---|---|---|\n",
        n = issue.number,
    );
    for (short, role, cwd) in members {
        s.push_str(&format!("| `{short}` | {role} | `{cwd}` |\n"));
    }
    s.push_str(
        "\n## 協定\n\n每則回覆的**最後**要有一個 ```am-team fenced 區塊（一個 JSON 物件）。daemon 只讀最後一個。\n\n\
         - pm：`dispatch`（每筆要 `brief`，`to` 可省略——daemon 會派給有空的執行者；派幾筆都可以，超過併行數的排隊）/ `wait` / `done`（可帶 `workers: keep | replace`，決定下一個 issue 是否沿用執行者）/ `ask_user` / `abort`\n\
         - worker：`report`（`status: done | blocked`）\n\
         - reviewer：`verdict`（`result: approve | request_changes`）\n\n\
         區塊之外的文字給人看，區塊給系統看。\n",
    );
    s
}

/// `TEAM.md` for a team in unlimited mode (§4.5): several issues are in flight, so the page
/// leads with the table of what is running — issue, its integration branch, its `ISSUE-<n>.md`
/// and its executors — instead of naming one issue in the heading.
fn team_md_unlimited(
    working: &[(i64, String, String, Vec<String>)],
    root: &str,
    repo: &str,
    members: &[(String, String, String)],
) -> String {
    let repo_line = if repo.is_empty() {
        String::new()
    } else {
        format!("- 這個 team 屬於專案的 submodule `{repo}`：每個成員的 cwd 都是**該 submodule** 的 worktree\n")
    };
    let mut s = format!(
        "# Team · 無限併行（同時進行 {n} 個 issue）\n\n- team 根目錄：`{root}`\n{repo_line}         - 合併由 daemon 用 `git merge --no-ff` 執行，agent 不要自己合併\n\n         ## 進行中的 issue\n\n| issue | 整合分支 | 全文 | 執行者 |\n|---|---|---|---|\n",
        n = working.len(),
    );
    for (number, title, branch, who) in working {
        s.push_str(&format!(
            "| #{number}「{title}」 | `{branch}` | `.agents-manager/team/ISSUE-{number}.md` | {who} |\n",
            who = if who.is_empty() { "（尚未建立）".to_string() } else { who.join("、") },
        ));
    }
    s.push_str("\n## 成員\n\n| 短名 | 角色 | cwd |\n|---|---|---|\n");
    for (short, role, cwd) in members {
        s.push_str(&format!("| `{short}` | {role} | `{cwd}` |\n"));
    }
    s.push_str(
        "\n## 協定\n\n每則回覆的**最後**要有一個 ```am-team fenced 區塊（一個 JSON 物件）。daemon 只讀最後一個。\n\n         - pm：`dispatch`（每筆要 `brief` 與 **`issue`（issue 號）**，`to` 可省略）/ `wait` /          `done`（要帶 **`issue`**，只交付那一個 issue）/ `ask_user` / `abort`\n         - worker：`report`（`status: done | blocked`）\n         - reviewer：`verdict`（`result: approve | request_changes`）\n\n         區塊之外的文字給人看，區塊給系統看。\n",
    );
    s
}

// ---------------------------------------------------------------- JSON shapes (§10.2–§10.4)

pub fn task_json(t: &db::TeamTask) -> Value {
    json!({
        "id": t.id,
        "issue_id": t.issue_id,
        "team_id": t.team_id,
        "seq": t.seq,
        "title": t.title,
        "brief": t.brief,
        "files": serde_json::from_str::<Value>(&t.files_json).unwrap_or_else(|_| json!([])),
        "worker_bot_id": t.worker_bot_id,
        "branch": t.branch,
        "state": t.state,
        "round": t.round,
        "rebase_attempts": t.rebase_attempts,
        "last_report": t.last_report,
        "last_verdict": t.last_verdict,
        "merge_sha": t.merge_sha,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
    })
}

pub fn event_json(e: &db::TeamEvent) -> Value {
    json!({
        "id": e.id,
        "team_id": e.team_id,
        // Per-team insertion order. Clients that merge live `team_event` frames into a loaded
        // page should sort and de-duplicate on this, not on `id` or `created_at`.
        "seq": e.seq,
        "kind": e.kind,
        "from_bot_id": e.from_bot_id,
        "to_bot_id": e.to_bot_id,
        "task_id": e.task_id,
        "turn_id": e.turn_id,
        "status": e.status,
        "payload": serde_json::from_str::<Value>(&e.payload_json).unwrap_or_else(|_| json!({})),
        "created_at": e.created_at,
    })
}

/// Counts per task state plus `total` — the §10.2 `tasks_summary`.
pub fn tasks_summary(tasks: &[db::TeamTask]) -> Value {
    let mut m = serde_json::Map::new();
    for t in tasks {
        let n = m.get(&t.state).and_then(|v| v.as_i64()).unwrap_or(0);
        m.insert(t.state.clone(), json!(n + 1));
    }
    m.insert("total".into(), json!(tasks.len()));
    Value::Object(m)
}

/// The team object of `GET /api/state` (SPEC-team §10.2).
/// One entry of the issue queue, as the UI sees it (§2.3).
pub fn issue_json(i: &db::TeamIssue) -> Value {
    json!({
        "id": i.id,
        "seq": i.seq,
        "issue_number": i.issue_number,
        "issue_title": i.issue_title,
        "issue_url": i.issue_url,
        "state": i.state,
        "branch": i.branch,
        "summary": i.summary,
        "pr_url": i.pr_url,
        "issue_closed_at": i.issue_closed_at,
        "fail_reason": i.fail_reason,
        "started_at": i.started_at,
        "ended_at": i.ended_at,
    })
}

pub async fn team_json(app: &Arc<App>, t: &db::Team) -> Value {
    let members = db::team_members(&app.db, &t.id).await.unwrap_or_default();
    let tasks = db::team_tasks(&app.db, &t.id).await.unwrap_or_default();
    // §2.3: the queue, plus which entry the scalar `issue_*` fields above are mirroring.
    let issues = db::team_issues(&app.db, &t.id).await.unwrap_or_default();
    let current_issue_id = issues.iter().find(|i| i.state == "working").map(|i| i.id.clone());
    let issues_summary = json!({
        "total": issues.len(),
        "done": issues.iter().filter(|i| i.state == "done").count(),
        "failed": issues.iter().filter(|i| i.state == "failed" || i.state == "skipped").count(),
        "queued": issues.iter().filter(|i| i.state == "queued").count(),
    });
    json!({
        "id": t.id,
        "issues": issues.iter().map(issue_json).collect::<Vec<_>>(),
        "current_issue_id": current_issue_id,
        "issues_summary": issues_summary,
        "project_id": t.project_id,
        "issue_number": t.issue_number,
        "issue_title": t.issue_title,
        "issue_url": t.issue_url,
        "label": t.label,
        "phase": t.phase,
        "pause_reason": t.pause_reason,
        "pause_detail": pause_detail_json(t),
        "resume_phase": t.resume_phase,
        "branch": t.branch,
        "deliver": t.deliver,
        "supervised": t.supervised == 1,
        "members": members.iter().map(|b| json!({
            "bot_id": b.id,
            "role": b.team_role,
            "name": b.name,
            "short": short_name(&b.name, &t.id),
            "deleted": b.deleted_at.is_some(),
        })).collect::<Vec<_>>(),
        "tasks_summary": tasks_summary(&tasks),
        "budget": Budget::from_json(&t.budget_json),
        "usage": serde_json::from_str::<Value>(&t.usage_json).unwrap_or_else(|_| empty_usage()),
        "pr_url": t.pr_url,
        "repo": t.repo,
        "summary": t.summary,
        "issue_closed_at": t.issue_closed_at,
        "created_at": t.created_at,
        "started_at": t.started_at,
        "ended_at": t.ended_at,
    })
}

/// `projects[].teams[]` for `GET /api/state`.
pub async fn teams_json_for_project(app: &Arc<App>, project_id: &str) -> Vec<Value> {
    let teams = db::teams_of_project(&app.db, project_id).await.unwrap_or_default();
    let mut out = Vec::with_capacity(teams.len());
    for t in &teams {
        out.push(team_json(app, t).await);
    }
    out
}

/// The `team` field on a bot in `GET /api/state`: `{team_id, role} | null`.
pub fn bot_team_json(b: &db::Bot) -> Value {
    match (&b.team_id, &b.team_role) {
        (Some(id), role) => json!({"team_id": id, "role": role}),
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------- WS events (§10.6)

/// SPEC-team §10.6: `pause_detail` as a JSON value, `null` when the pause has nothing to add
/// (and when an old row's column cannot be parsed — a broken detail must never hide a pause).
pub fn pause_detail_json(t: &db::Team) -> Value {
    t.pause_detail_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or(Value::Null)
}

async fn emit_team_changed(app: &Arc<App>, t: &db::Team) {
    app.emit(
        "team_changed",
        json!({
            "team_id": t.id,
            "project_id": t.project_id,
            "phase": t.phase,
            "pause_reason": t.pause_reason,
            "pause_detail": pause_detail_json(t),
            "usage": serde_json::from_str::<Value>(&t.usage_json).unwrap_or_else(|_| empty_usage()),
        }),
    )
    .await;
}

async fn emit_task_updated(app: &Arc<App>, task: &db::TeamTask) {
    app.emit("team_task_updated", json!({"team_id": task.team_id, "task": task_json(task)})).await;
}

async fn emit_team_event(app: &Arc<App>, e: &db::TeamEvent) {
    app.emit("team_event", json!({"team_id": e.team_id, "event": event_json(e)})).await;
}

// ---------------------------------------------------------------- row helpers

fn any_err<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

pub async fn load(app: &Arc<App>, team_id: &str) -> LcResult<db::Team> {
    db::team(&app.db, team_id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("team".into()))
}

/// Append one `team_events` row and push it on the WS.
#[allow(clippy::too_many_arguments)]
pub async fn record_event(
    app: &Arc<App>,
    team_id: &str,
    kind: &str,
    from_bot_id: Option<&str>,
    to_bot_id: Option<&str>,
    task_id: Option<&str>,
    status: Option<&str>,
    payload: Value,
) -> LcResult<db::TeamEvent> {
    let id = db::ulid();
    let now = db::now();
    let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    // §2.3: every event belongs to whichever issue the team is on. Looked up here rather than
    // threaded through ~30 call sites — the answer is always "the current one", and the write
    // that makes an issue current happens before any event about it.
    let issue_id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM team_issues WHERE team_id = ? AND state = 'working' ORDER BY seq LIMIT 1",
    )
    .bind(team_id)
    .fetch_optional(&app.db)
    .await
    .ok()
    .flatten();
    // `seq` is picked inside the INSERT: SQLite serialises writers, so the sub-select and the
    // row it numbers are one statement under one write lock and two concurrent events cannot
    // land on the same number. The UNIQUE(team_id, seq) index is the belt to that braces —
    // if it ever did fire, retrying re-reads MAX(seq) and takes the next one.
    const SQL: &str = "INSERT INTO team_events
         (id, team_id, seq, issue_id, kind, from_bot_id, to_bot_id, task_id, turn_id, status, payload_json, created_at)
         VALUES (?,?,(SELECT COALESCE(MAX(seq),0)+1 FROM team_events WHERE team_id = ?),?,?,?,?,?,NULL,?,?,?)";
    let mut attempt = 0;
    loop {
        let r = sqlx::query(SQL)
            .bind(&id)
            .bind(team_id)
            .bind(team_id)
            .bind(&issue_id)
            .bind(kind)
            .bind(from_bot_id)
            .bind(to_bot_id)
            .bind(task_id)
            .bind(status)
            .bind(&payload_json)
            .bind(&now)
            .execute(&app.db)
            .await;
        match r {
            Ok(_) => break,
            Err(e) => {
                let dup = e.as_database_error().map(|d| d.is_unique_violation()).unwrap_or(false);
                attempt += 1;
                if !dup || attempt >= 3 {
                    return Err(any_err(e));
                }
            }
        }
    }
    let e = sqlx::query_as::<_, db::TeamEvent>("SELECT * FROM team_events WHERE id = ?")
        .bind(&id)
        .fetch_one(&app.db)
        .await
        .map_err(any_err)?;
    emit_team_event(app, &e).await;
    Ok(e)
}

/// Move a team to `phase`, record the transition and push `team_changed`.
pub async fn set_phase(
    app: &Arc<App>,
    team_id: &str,
    phase: &str,
    pause_reason: Option<&str>,
    resume_phase: Option<&str>,
) -> LcResult<db::Team> {
    set_phase_detail(app, team_id, phase, pause_reason, resume_phase, None).await
}

/// [`set_phase`] plus §4.5's structured `pause_detail` — for the reasons where the code alone
/// leaves the user guessing (`quota_low`: *whose* account, which window, how much is left).
/// The column is rewritten on **every** phase change, so a stale detail can never outlive the
/// pause it described.
pub async fn set_phase_detail(
    app: &Arc<App>,
    team_id: &str,
    phase: &str,
    pause_reason: Option<&str>,
    resume_phase: Option<&str>,
    pause_detail: Option<Value>,
) -> LcResult<db::Team> {
    write_phase(app, team_id, phase, pause_reason, resume_phase, pause_detail, None)
        .await?
        .ok_or_else(|| LcError::NotFound("team".into()))
}

/// The phases the scheduler moves a team through by itself (SPEC-team §8.1). `paused`,
/// `aborting` and the terminal phases were written by somebody else — a human, or a guard —
/// and the scheduler never writes over them (#31).
pub const SCHED_PHASES: [&str; 4] = ["starting", "planning", "working", "finishing"];

/// A phase write from the scheduler (#31). Every scheduler pass acts on a `Ctx` loaded at its
/// start and then awaits — members stopping, git, relays — so by the time it writes, a user
/// may have paused or aborted. The write is therefore a compare-and-set against the phase
/// actually in the row, and only ever out of [`SCHED_PHASES`]:
///
/// * aborting / terminal: dropped — nothing revives a team the user ended;
/// * paused: the pause stays, and with `defer` the target becomes its `resume_phase`, so
///   "continue" lands where the scheduler got to (not back in `finishing` to deliver the same
///   issue twice). Re-entrant `starting` work passes `defer = false` and simply runs again.
///
/// `true` only when the phase actually moved now; the caller stops its pass otherwise.
pub async fn sched_phase(app: &Arc<App>, team_id: &str, phase: &str, defer: bool) -> LcResult<bool> {
    for _ in 0..3 {
        let t = load(app, team_id).await?;
        if SCHED_PHASES.contains(&t.phase.as_str()) {
            if write_phase(app, team_id, phase, None, None, None, Some(&t.phase)).await?.is_some() {
                return Ok(true);
            }
            continue;
        }
        if t.phase == "paused" && defer {
            let r = sqlx::query("UPDATE teams SET resume_phase = ? WHERE id = ? AND phase = 'paused'")
                .bind(phase)
                .bind(team_id)
                .execute(&app.db)
                .await
                .map_err(any_err)?;
            if r.rows_affected() == 0 {
                continue;
            }
            emit_team_changed(app, &load(app, team_id).await?).await;
        }
        tracing::info!(team = %team_id, phase = %t.phase, to = phase, "scheduler phase write not applied: the team was paused or ended");
        return Ok(false);
    }
    Ok(false)
}

/// A guard's pause (§4.5), under the same rule as [`sched_phase`]: only out of a phase the
/// scheduler owns, with `resume_phase` taken from the row rather than from a stale snapshot.
/// An existing pause keeps its first reason. `true` when this call paused the team.
pub async fn sched_pause(app: &Arc<App>, team_id: &str, reason: &str, detail: Option<Value>) -> LcResult<bool> {
    for _ in 0..3 {
        let t = load(app, team_id).await?;
        if !SCHED_PHASES.contains(&t.phase.as_str()) {
            return Ok(false);
        }
        let from = Some(t.phase.as_str());
        if write_phase(app, team_id, "paused", Some(reason), from, detail.clone(), from).await?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `expect = Some(p)`: write only while the row is still in phase `p`; `None` when it was not.
async fn write_phase(
    app: &Arc<App>,
    team_id: &str,
    phase: &str,
    pause_reason: Option<&str>,
    resume_phase: Option<&str>,
    pause_detail: Option<Value>,
    expect: Option<&str>,
) -> LcResult<Option<db::Team>> {
    let before = load(app, team_id).await?;
    let from = expect.unwrap_or(&before.phase).to_string();
    let detail_json = pause_detail.as_ref().map(|d| d.to_string());
    let ended_at = if is_terminal(phase) { Some(db::now()) } else { None };
    // Reopen is a transition out of a terminal phase, not a pause. Keep its audit reason in
    // the phase event, while the row itself must remain resumable with no pause marker and no
    // stale terminal timestamp (§2.5.2).
    // `phase = COALESCE(?, phase)` is always true when `expect` is NULL: the unconditional write.
    let written = if phase == "starting" && pause_reason == Some("reopen") {
        sqlx::query(
            "UPDATE teams SET phase = ?, pause_reason = NULL, resume_phase = NULL,
               pause_detail_json = NULL, ended_at = NULL WHERE id = ? AND phase = COALESCE(?, phase)",
        )
        .bind(phase)
        .bind(team_id)
        .bind(expect)
        .execute(&app.db)
        .await
        .map_err(any_err)?
    } else {
        sqlx::query(
            "UPDATE teams SET phase = ?, pause_reason = ?, resume_phase = ?,
               pause_detail_json = ?, ended_at = COALESCE(?, ended_at) WHERE id = ? AND phase = COALESCE(?, phase)",
        )
        .bind(phase)
        .bind(pause_reason)
        .bind(resume_phase)
        .bind(detail_json)
        .bind(ended_at)
        .bind(team_id)
        .bind(expect)
        .execute(&app.db)
        .await
        .map_err(any_err)?
    };
    if written.rows_affected() == 0 {
        return Ok(None);
    }
    record_event(
        app,
        team_id,
        "phase",
        None,
        None,
        None,
        None,
        json!({"from": from, "to": phase, "reason": pause_reason, "detail": pause_detail}),
    )
    .await?;
    let after = load(app, team_id).await?;
    emit_team_changed(app, &after).await;
    Ok(Some(after))
}

// ---------------------------------------------------------------- validation

/// The same identity rule as `POST /projects/:id/bots` — resolved on the team's host, so a
/// discovered `ccN` counts there too (SPEC §16).
async fn check_identity(app: &Arc<App>, host: &str, identity: &Option<String>, kind: &str) -> LcResult<Option<String>> {
    let Some(name) = identity.clone().filter(|s| !s.trim().is_empty()) else { return Ok(None) };
    let Some(i) = crate::tools::identity_for_host(app, host, &name).await else {
        return Err(LcError::NotFound("identity".into()));
    };
    if i.kind != kind {
        return Err(LcError::Bad(format!("identity `{name}` is for {} but this role is {kind}", i.kind)));
    }
    Ok(Some(name))
}

/// A role spec with `kind` / `effort` / `identity` checked and normalised.
struct CheckedRole {
    kind: String,
    model: Option<String>,
    effort: Option<String>,
    fast: bool,
    identity: Option<String>,
    persona_extra: Option<String>,
}

async fn check_role(app: &Arc<App>, host: &str, r: &RoleSpec, label: &str) -> LcResult<CheckedRole> {
    if !crate::config::valid_kind(&r.kind) {
        return Err(LcError::Bad(format!("{label}.kind must be {}", crate::config::kinds_list())));
    }
    let effort = crate::config::normalize_effort(&r.kind, r.effort.as_deref())
        .map_err(|e| LcError::Bad(format!("{label}: {e}")))?;
    let identity = check_identity(app, host, &r.identity, &r.kind).await?;
    Ok(CheckedRole {
        kind: r.kind.clone(),
        model: r.model.clone().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()),
        effort,
        fast: r.fast,
        identity,
        persona_extra: r.persona_extra.clone().filter(|s| !s.trim().is_empty()),
    })
}

/// SPEC-team §4.5 / §9.2: refuse to start a team on a kind that is already near its cap.
/// Kinds with no quota data are never blocked. Quota is per host (SPEC §14), so the rows
/// consulted are the ones for the project's host.
async fn check_quota(app: &Arc<App>, host: &str, roles: &[&CheckedRole], stop_pct: f64) -> LcResult<()> {
    if quota_check_disabled(stop_pct) {
        return Ok(());
    }
    let q = app.quotas.lock().await.clone();
    for r in roles {
        let keys: Vec<String> = match &r.identity {
            Some(i) => vec![format!("{}:{i}", r.kind), r.kind.clone()],
            None => vec![r.kind.clone()],
        }
        .iter()
        .map(|base| crate::quota::quota_key(host, base))
        .collect();
        for k in keys {
            let Some(quota) = q.get(&k) else { continue };
            for w in [&quota.five_hour, &quota.seven_day].into_iter().flatten() {
                if w.used_pct >= stop_pct {
                    return Err(LcError::BadValue(
                        json!({"error": "quota_low", "kind": r.kind, "used_pct": w.used_pct}),
                    ));
                }
            }
            break; // the identity row wins when it exists
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- create (§10.1)

/// Resolve the issue through the existing `gh issue view` path.
/// The directory git runs in for this team: the project checkout, or the submodule inside it.
pub fn repo_path(project: &db::Project, repo: &str) -> String {
    crate::github::repo_path(&project.path, repo)
}

async fn resolve_issue(app: &Arc<App>, project_id: &str, repo: &str, number: i64) -> LcResult<IssueRef> {
    if number <= 0 {
        return Err(LcError::Bad("issue_number must be positive".into()));
    }
    let v = crate::github::get_issue(app, project_id, repo, number as u64).await?;
    let i = v.get("issue").cloned().unwrap_or(Value::Null);
    let title = i.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let url = i.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let body = i.get("body").and_then(|x| x.as_str()).unwrap_or("").to_string();
    Ok(IssueRef { number, title, url, body })
}

/// `POST /api/projects/:id/teams`.
pub async fn create(app: &Arc<App>, project_id: &str, req: CreateTeam) -> LcResult<Value> {
    let numbers = req.issue_list().map_err(LcError::Bad)?;
    let repo = req.repo_rel();
    let mut issues = Vec::with_capacity(numbers.len());
    for n in numbers {
        issues.push(resolve_issue(app, project_id, &repo, n).await?);
    }
    create_with_issues(app, project_id, req, issues).await
}

/// `create` with the queue already resolved. Split out so the whole validation + DB path can
/// be exercised without a GitHub round trip.
pub async fn create_with_issues(
    app: &Arc<App>,
    project_id: &str,
    req: CreateTeam,
    issues: Vec<IssueRef>,
) -> LcResult<Value> {
    let issue = issues.first().cloned().ok_or_else(|| LcError::Bad("issue_numbers must not be empty".into()))?;
    let project = db::project(&app.db, project_id)
        .await
        .map_err(any_err)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;

    let worker_count = req.workers.resolved_count();
    if worker_count > MAX_WORKERS {
        return Err(LcError::Bad(format!("workers.count must be 0 (unlimited) or between 1 and {MAX_WORKERS}")));
    }
    // §4.5 unlimited: the *stored* count stays 0, but the first issue still starts with one
    // executor — `ensure_workers_for` adds the rest on demand as the PM dispatches.
    let build_count = if worker_count == UNLIMITED_WORKERS { 1 } else { worker_count };
    let deliver = req.deliver.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| DEFAULT_DELIVER.into());
    if !DELIVERS.contains(&deliver.as_str()) {
        return Err(LcError::Bad("deliver must be `branch` or `pr`".into()));
    }
    let base_ref = req.base.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| DEFAULT_BASE_REF.into());
    let supervised = req.supervised.unwrap_or(DEFAULT_SUPERVISED);
    let mut budget = Budget::default();
    if let Some(p) = &req.budget {
        budget.apply(p).map_err(LcError::Bad)?;
    }

    let pm = check_role(app, &project.host, &req.pm, "pm").await?;
    let workers = check_role(app, &project.host, &req.workers.role(), "workers").await?;
    let reviewer = match &req.reviewer {
        Some(r) => Some(check_role(app, &project.host, r, "reviewer").await?),
        None => None,
    };
    let mut quota_roles: Vec<&CheckedRole> = vec![&pm, &workers];
    if let Some(r) = &reviewer {
        quota_roles.push(r);
    }
    check_quota(app, &project.host, &quota_roles, budget.quota_stop_pct).await?;

    // SPEC-team §13: the first stage is local-host only. Everything below would work over
    // `run_on_host` / `ssh_put`, but nothing has been tested against a remote checkout yet,
    // and half-tested git on someone else's machine is not a thing to ship.
    if project.host != LOCAL_HOST {
        return Err(LcError::Bad(format!(
            "teams are local-host only in this stage; project `{}` lives on `{}`",
            project.label, project.host
        )));
    }

    // A submodule team: `repo` must be one of the project's listed submodules (the list is
    // what turns a user-supplied path into a directory git runs in), and it must be checked
    // out — an empty submodule directory has no commits to branch from.
    let repo = req.repo_rel();
    if !repo.is_empty() {
        if !crate::github::valid_repo_rel(&repo) {
            return Err(LcError::Bad(format!("repo `{repo}` is not a relative path")));
        }
        let subs = crate::github::list_submodules(app, &project, false).await?;
        if !subs.iter().any(|s| s.path == repo) {
            return Err(LcError::Bad(format!("`{repo}` is not a submodule of this project")));
        }
    }
    let git_dir = repo_path(&project, &repo);

    let team_id = db::ulid();
    let t6 = tid6(&team_id);
    let branch = integration_branch(issue.number, &t6);
    let root = worktree_root(app, &team_id);

    // SPEC-team §7.4 / appendix C: `base_sha` and `worktree_root` are `NOT NULL`, so both
    // are resolved **before** the row is written. The directories themselves can come later —
    // the paths are computed, not discovered.
    if !tg::is_inside_work_tree(app, &project.host, &git_dir).await {
        return Err(LcError::BadValue(json!({"error": "not_a_git_repo", "path": git_dir})));
    }
    let base_sha = tg::resolve_commit(app, &project.host, &git_dir, &base_ref)
        .await
        .map_err(|e| LcError::Bad(e.to_string()))?;
    let worktree_root = root.clone();

    let roles_json = json!({
        "pm": role_spec_json(&pm),
        "workers": {"count": worker_count, "spec": role_spec_json(&workers)},
        "reviewer": reviewer.as_ref().map(role_spec_json),
    });
    let now = db::now();
    sqlx::query(
        "INSERT INTO teams (id, project_id, issue_number, issue_title, issue_url, phase, pause_reason, resume_phase,
           base_ref, base_sha, branch, worktree_root, deliver, supervised, roles_json, budget_json, usage_json,
           pr_url, summary, repo, created_at, started_at, ended_at)
         VALUES (?,?,?,?,?,'starting',NULL,NULL,?,?,?,?,?,?,?,?,?,NULL,NULL,?,?,NULL,NULL)",
    )
    .bind(&team_id)
    .bind(&project.id)
    .bind(issue.number)
    .bind(&issue.title)
    .bind(&issue.url)
    .bind(&base_ref)
    .bind(&base_sha)
    .bind(&branch)
    .bind(&worktree_root)
    .bind(&deliver)
    .bind(supervised as i64)
    .bind(serde_json::to_string(&roles_json).unwrap_or_else(|_| "{}".into()))
    .bind(serde_json::to_string(&budget).unwrap_or_else(|_| "{}".into()))
    .bind(serde_json::to_string(&empty_usage()).unwrap_or_else(|_| "{}".into()))
    .bind(&repo)
    .bind(&now)
    .execute(&app.db)
    .await
    .map_err(any_err)?;

    // SPEC-team §2.3: the queue. The first entry is already `working` and carries the branch
    // and base commit resolved above; the rest wait. The scalar columns on `teams` mirror
    // whichever entry is current, which is what keeps every existing query working.
    for (i, iss) in issues.iter().enumerate() {
        let seq = i as i64 + 1;
        let first = i == 0;
        sqlx::query(
            "INSERT INTO team_issues (id, team_id, seq, issue_number, issue_title, issue_url, state,
               branch, base_sha, created_at, started_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(db::ulid())
        .bind(&team_id)
        .bind(seq)
        .bind(iss.number)
        .bind(&iss.title)
        .bind(&iss.url)
        .bind(if first { "working" } else { "queued" })
        .bind(if first { Some(branch.as_str()) } else { None })
        .bind(if first { Some(base_sha.as_str()) } else { None })
        .bind(&now)
        .bind(if first { Some(now.as_str()) } else { None })
        .execute(&app.db)
        .await
        .map_err(any_err)?;
    }

    // SPEC-team §6.2 / appendix C: the integration branch, then one worktree per member,
    // all under `<data_dir>/teams/<id>/` — outside the repository. The only commands aimed
    // at the user's checkout are `branch` and `worktree add`, neither of which writes a byte
    // into its working tree.
    let mut plan: Vec<(&'static str, u32, &CheckedRole)> = vec![("pm", 0, &pm)];
    for n in 1..=build_count {
        plan.push(("worker", n, &workers));
    }
    if let Some(r) = &reviewer {
        plan.push(("reviewer", 0, r));
    }
    let dirs: Vec<String> =
        plan.iter().map(|(role, i, _)| format!("{root}/{}", member_dir(role, *i, 1))).collect();

    let mut created: Vec<String> = Vec::new();
    let mut integration_branch_created = false;
    // Filled in by the §6.4a step below; `None` until then so an early failure knows there
    // is no workspace to close.
    let workspace_id: Option<String>;
    let build = async {
        tg::create_branch(app, &project.host, &git_dir, &branch, &base_sha)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?;
        integration_branch_created = true;
        for (i, (role, _, _)) in plan.iter().enumerate() {
            // Only `main/` may hold the integration branch — git refuses the same branch in
            // two worktrees, so everyone else starts detached at the base commit (§6.2).
            let (rev, detach) =
                if *role == "pm" { (branch.as_str(), false) } else { (base_sha.as_str(), true) };
            tg::worktree_add(app, &project.host, &git_dir, &dirs[i], rev, detach)
                .await
                .map_err(|e| LcError::Upstream(e.to_string()))?;
        }
        Ok::<(), LcError>(())
    }
    .await;
    if let Err(e) = build {
        rollback_create(
            app, &project, &repo, &team_id, &created, &root, &dirs, None,
            &branch, &base_sha, integration_branch_created,
        ).await;
        return Err(e);
    }

    // SPEC-team §6.4a: the team gets its **own** herdr workspace, rooted at the team
    // directory, created after the worktrees and before the members. Four-plus panes would
    // otherwise be squeezed into the workspace the user is actually working in, and a team
    // is temporary where a project's workspace is permanent. If it cannot be created the
    // whole team fails — falling back to the project's workspace would leave throw-away
    // panes in the user's own space, which is the thing this exists to prevent.
    match make_workspace(app, &project, &team_id, &root, &issue).await {
        Ok(ws) => workspace_id = Some(ws),
        Err(e) => {
            rollback_create(
                app, &project, &repo, &team_id, &created, &root, &dirs, None,
                &branch, &base_sha, integration_branch_created,
            ).await;
            return Err(e);
        }
    }
    if let Err(e) = sqlx::query("UPDATE teams SET workspace_id = ? WHERE id = ?")
        .bind(&workspace_id)
        .bind(&team_id)
        .execute(&app.db)
        .await
        .map_err(any_err)
    {
        rollback_create(
            app, &project, &repo, &team_id, &created, &root, &dirs, workspace_id.as_deref(),
            &branch, &base_sha, integration_branch_created,
        ).await;
        return Err(e);
    }

    for (i, (role, index, spec)) in plan.iter().enumerate() {
        let nick = member_nick(&t6, role, *index, 1);
        match insert_member(app, &project, &team_id, &nick, &t6, role, issue.number, spec, &dirs[i]).await {
            Ok(bot_id) => created.push(bot_id),
            Err(e) => {
                // SPEC-team §6.5: a half-built team is torn down rather than left behind.
                rollback_create(
                    app, &project, &repo, &team_id, &created, &root, &dirs, workspace_id.as_deref(),
                    &branch, &base_sha, integration_branch_created,
                ).await;
                return Err(e);
            }
        }
    }

    // The docs and the real personas need the roster, so they come after the members exist.
    let members = db::team_members(&app.db, &team_id).await.map_err(any_err)?;
    let roster: Vec<(String, String, String)> = members
        .iter()
        .map(|b| {
            (
                short_name(&b.name, &team_id),
                b.team_role.clone().unwrap_or_default(),
                b.cwd.clone().unwrap_or_default(),
            )
        })
        .collect();
    let issue_doc = issue_md(&issue);
    let team_doc = team_md(&issue, &branch, &root, &repo, &roster);
    let docs = async {
        // The originals live at the team root; each worktree gets an ignored copy inside the
        // member's own cwd so the agent never has to read across directories (§6.2).
        tg::put_file(app, &project.host, &format!("{root}/ISSUE.md"), &issue_doc)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?;
        tg::put_file(app, &project.host, &format!("{root}/TEAM.md"), &team_doc)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?;
        for d in &dirs {
            tg::write_team_docs(app, &project.host, d, &issue_doc, &team_doc)
                .await
                .map_err(|e| LcError::Upstream(e.to_string()))?;
        }
        Ok::<(), LcError>(())
    }
    .await;
    if let Err(e) = docs {
        rollback_create(
            app, &project, &repo, &team_id, &created, &root, &dirs, workspace_id.as_deref(),
            &branch, &base_sha, integration_branch_created,
        ).await;
        return Err(e);
    }

    // Short names only. `agent.start` types the whole command into a shell and the line is
    // truncated past roughly a kilobyte — four absolute worktree paths in here pushed the PM's
    // persona over that (2026-09-06: the command stopped mid-word, Enter never arrived, and the
    // team failed on `agent_not_running` sixty seconds later). Every member's path is in
    // `TEAM.md`, which the persona already tells them to read.
    let roster_line: String =
        roster.iter().map(|(s, r, _)| format!("`{s}`（{r}）")).collect::<Vec<_>>().join("、");
    for b in &members {
        let role = b.team_role.clone().unwrap_or_default();
        let spec = match role.as_str() {
            "pm" => &pm,
            "reviewer" => reviewer.as_ref().unwrap_or(&workers),
            _ => &workers,
        };
        let p = full_persona(
            &role,
            issue.number,
            &short_name(&b.name, &team_id),
            b.cwd.as_deref().unwrap_or_default(),
            &branch,
            &roster_line,
            spec.persona_extra.as_deref(),
            worker_count == UNLIMITED_WORKERS,
        );
        let _ = sqlx::query("UPDATE bots SET persona = ? WHERE id = ?").bind(&p).bind(&b.id).execute(&app.db).await;
    }

    record_event(
        app,
        &team_id,
        "phase",
        None,
        None,
        None,
        None,
        json!({"from": Value::Null, "to": "starting", "reason": "team created"}),
    )
    .await?;

    for b in &created {
        app.emit("bot_changed", json!({"bot_id": b})).await;
    }
    let t = load(app, &team_id).await?;
    emit_team_changed(app, &t).await;
    app.emit("project_changed", json!({"project_id": project.id})).await;
    spawn_scheduler(app, &team_id);
    Ok(json!({"team_id": team_id}))
}

// ---------------------------------------------------------------- the issue queue (§2.3)

/// Reopen uses the role choices persisted at Team creation. Validate them again before any
/// issue is resolved or inserted: the identities/quota snapshot may have changed while the
/// finished Team was waiting for the user.
async fn validate_reopen_roles(app: &Arc<App>, t: &db::Team, host: &str) -> LcResult<()> {
    let roles: Value = serde_json::from_str(&t.roles_json).unwrap_or(Value::Null);
    let pm_spec: RoleSpec = roles
        .get("pm")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .ok_or_else(|| LcError::Upstream("team roles_json has no pm spec".into()))?;
    let pm = check_role(app, host, &pm_spec, "pm").await?;
    let (_, workers) = queued_worker_role(app, t, host).await?;
    let reviewer = match roles.get("reviewer") {
        Some(v) if !v.is_null() => {
            let spec: RoleSpec = serde_json::from_value(v.clone())
                .map_err(|_| LcError::Upstream("team roles_json has an invalid reviewer spec".into()))?;
            Some(check_role(app, host, &spec, "reviewer").await?)
        }
        _ => None,
    };
    let mut quota_roles: Vec<&CheckedRole> = vec![&pm, &workers];
    if let Some(r) = &reviewer {
        quota_roles.push(r);
    }
    check_quota(app, host, &quota_roles, Budget::from_json(&t.budget_json).quota_stop_pct).await
}

/// `POST /api/teams/:id/issues` — append to the queue of a running team.
///
/// SPEC-team §2.5.1: `done` is the one terminal phase that takes new issues. The site is still
/// there (worktrees, PM/reviewer conversations, integration branch), so appending pulls the
/// team back into the §2.3 queue instead of asking the user to build a new one. `aborted` and
/// `failed` have no site worth continuing from and keep the old refusal.
pub async fn add_issues(app: &Arc<App>, team_id: &str, numbers: &[i64]) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    // Everything that can be refused without talking to GitHub is refused here, so a rejected
    // request costs no `gh issue view` round trip and writes nothing.
    let reopening = check_add_issues(app, &t, numbers).await?;
    let mut resolved = Vec::with_capacity(numbers.len());
    for n in numbers {
        resolved.push(resolve_issue(app, &t.project_id, &t.repo, *n).await?);
    }
    add_resolved_issues(app, team_id, resolved, reopening).await
}

/// The GitHub-free half of [`add_issues`]: every guard, plus whether this is a §2.5 reopen.
async fn check_add_issues(app: &Arc<App>, t: &db::Team, numbers: &[i64]) -> LcResult<bool> {
    if is_terminal(&t.phase) && t.phase != "done" {
        return Err(LcError::conflict("team is finished", json!({"phase": t.phase})));
    }
    if numbers.is_empty() {
        return Err(LcError::Bad("issue_numbers must not be empty".into()));
    }
    let existing = db::team_issues(&app.db, &t.id).await.map_err(any_err)?;
    let reopening = t.phase == "done";
    if reopening {
        // §2.5.1: "not cleaned up" is the PM member bot still being there. `cleanup` and
        // `delete` both stamp `deleted_at`; `workspace_id` is unreliable on teams built
        // before §6.4a, which is why the member row is the judge.
        let members = db::team_members(&app.db, &t.id).await.map_err(any_err)?;
        let pm_live = members.iter().any(|b| b.team_role.as_deref() == Some("pm") && b.deleted_at.is_none());
        if !pm_live {
            return Err(LcError::conflict("team is cleaned up", json!({"phase": t.phase})));
        }
        let project = db::project(&app.db, &t.project_id)
            .await
            .map_err(any_err)?
            .ok_or_else(|| LcError::NotFound("project".into()))?;
        validate_reopen_roles(app, t, &project.host).await?;
    }
    // The cap is about what the PM still has to hold in its head (§2.3), so only the rows
    // still **on** the queue count. A team that has delivered 19 issues in batches of five
    // used to be refused the twentieth batch for good, even though the queue was empty.
    let open = existing.iter().filter(|i| is_open_issue_state(&i.state)).count();
    if open + numbers.len() > MAX_QUEUED_ISSUES {
        return Err(LcError::Bad(format!("at most {MAX_QUEUED_ISSUES} open issues per team ({open} on the queue)")));
    }
    // "Already queued" means still **on** the queue: `queued` (waiting) or `working` (running).
    // A `done` / `failed` / `skipped` row is a finished record, not a claim on the issue
    // number, so the same issue can be queued again — a failed one to retry it, a delivered
    // one because the user wants another pass. The old row stays in the log and the new one
    // takes the next `seq` (§2.3). Duplicates inside the request itself count too: `[57, 57]`
    // would otherwise queue #57 twice and the second pass would never be startable.
    if let Some(issue_number) = numbers.iter().enumerate().find_map(|(index, issue)| {
        (numbers[..index].contains(issue)
            || existing.iter().any(|i| i.issue_number == *issue && is_open_issue_state(&i.state)))
            .then_some(*issue)
    }) {
        return Err(LcError::conflict("issue already queued", json!({"issue_number": issue_number})));
    }
    Ok(reopening)
}

/// `add_issues` with the queue already resolved, mirroring `create` / `create_with_issues`:
/// the whole insert + reopen path can be exercised without a GitHub round trip.
async fn add_resolved_issues(
    app: &Arc<App>,
    team_id: &str,
    resolved: Vec<IssueRef>,
    reopening: bool,
) -> LcResult<Value> {
    let existing = db::team_issues(&app.db, team_id).await.map_err(any_err)?;
    let mut next_seq = existing.iter().map(|i| i.seq).max().unwrap_or(0) + 1;
    let now = db::now();
    let mut added = Vec::with_capacity(resolved.len());
    for iss in resolved {
        sqlx::query(
            "INSERT INTO team_issues (id, team_id, seq, issue_number, issue_title, issue_url, state, created_at)
             VALUES (?,?,?,?,?,?, 'queued', ?)",
        )
        .bind(db::ulid())
        .bind(team_id)
        .bind(next_seq)
        .bind(iss.number)
        .bind(&iss.title)
        .bind(&iss.url)
        .bind(&now)
        .execute(&app.db)
        .await
        .map_err(any_err)?;
        added.push(iss.number);
        next_seq += 1;
    }
    record_event(
        app,
        team_id,
        "note",
        None,
        None,
        None,
        None,
        json!({"action": "issues_queued", "issue_numbers": added}),
    )
    .await?;
    if reopening {
        record_event(
            app,
            team_id,
            "note",
            None,
            None,
            None,
            None,
            json!({
                "action": "team_reopened",
                "by": "user",
                "issue_numbers": added,
                "from_phase": "done",
            }),
        )
        .await?;
        // `set_phase` clears the stale terminal timestamp and keeps `pause_reason` NULL while
        // preserving "reopen" in the phase event for the audit log.
        set_phase(app, team_id, "starting", Some("reopen"), None).await?;
        spawn_scheduler(app, team_id);
    }
    let t = load(app, team_id).await?;
    emit_team_changed(app, &t).await;
    Ok(json!({"issues": db::team_issues(&app.db, team_id).await.map_err(any_err)?.iter().map(issue_json).collect::<Vec<_>>()}))
}

/// `DELETE /api/teams/:id/issues/:issue_id` — drop an entry that has not started. An issue
/// already worked (or being worked) stays in the log; only the waiting ones can be taken back.
pub async fn remove_queued_issue(app: &Arc<App>, team_id: &str, issue_id: &str) -> LcResult<Value> {
    let q = db::team_issue(&app.db, issue_id)
        .await
        .map_err(any_err)?
        .filter(|i| i.team_id == team_id)
        .ok_or_else(|| LcError::NotFound("issue".into()))?;
    if q.state != "queued" {
        return Err(LcError::conflict("only a queued issue can be removed", json!({"state": q.state})));
    }
    sqlx::query("DELETE FROM team_issues WHERE id = ?").bind(issue_id).execute(&app.db).await.map_err(any_err)?;
    record_event(
        app,
        team_id,
        "note",
        None,
        None,
        None,
        None,
        json!({"action": "issue_unqueued", "issue_number": q.issue_number}),
    )
    .await?;
    let t = load(app, team_id).await?;
    emit_team_changed(app, &t).await;
    Ok(json!({}))
}

/// The worker role a team was created with, read back out of `roles_json`.
async fn queued_worker_role(app: &Arc<App>, t: &db::Team, host: &str) -> LcResult<(u32, CheckedRole)> {
    let roles: Value = serde_json::from_str(&t.roles_json).unwrap_or(Value::Null);
    let count = roles
        .get("workers")
        .and_then(|w| w.get("count"))
        .and_then(|c| c.as_u64())
        .unwrap_or(1)
        .clamp(1, MAX_WORKERS as u64) as u32;
    let spec: RoleSpec = roles
        .get("workers")
        .and_then(|w| w.get("spec"))
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .ok_or_else(|| LcError::Upstream("team roles_json has no worker spec".into()))?;
    Ok((count, check_role(app, host, &spec, "workers").await?))
}

/// Re-read the issue from GitHub so `ISSUE.md` carries its current body. Falls back to what
/// the queue already knows: a queued issue is worth starting even when `gh` is unavailable.
async fn refresh_issue(app: &Arc<App>, project_id: &str, repo: &str, q: &db::TeamIssue) -> IssueRef {
    match resolve_issue(app, project_id, repo, q.issue_number).await {
        Ok(i) if !i.title.trim().is_empty() => i,
        _ => IssueRef {
            number: q.issue_number,
            title: q.issue_title.clone(),
            url: q.issue_url.clone(),
            body: String::new(),
        },
    }
}

/// SPEC-team §2.3: put the team onto `q` — its own integration branch, a fresh set of workers,
/// and rewritten docs. The PM and the reviewer are **not** touched: they keep running, keep
/// their conversation, and learn about the new issue from `ISSUE.md` plus the relay that
/// `team_sched` sends after this returns.
///
/// Every issue is cut from the team's original `base_sha`, not from wherever `base_ref` points
/// now, so the queue's entries stay independent of each other and of anything the user merges
/// while the team is running.
pub async fn start_issue(app: &Arc<App>, team_id: &str, q: &db::TeamIssue, keep_workers: bool) -> LcResult<()> {
    let t = load(app, team_id).await?;
    let project = db::project(&app.db, &t.project_id)
        .await
        .map_err(any_err)?
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let t6 = tid6(team_id);
    let branch = issue_branch(app, team_id, q).await?;
    let root = t.worktree_root.clone();
    let main_wt = checked_main_wt(&t, &project).map_err(LcError::Bad)?;

    // The PM's worktree is about to move to a different branch, so it has to be clean —
    // the same rule `merge_one` applies before every merge.
    let dirty = tg::status_porcelain(app, &project.host, &main_wt).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if !dirty.trim().is_empty() {
        return Err(LcError::Upstream(format!("integration worktree is dirty, cannot start issue #{}", q.issue_number)));
    }
    tg::checkout_task_branch(app, &project.host, &main_wt, &branch, &t.base_sha)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;

    // A fresh set of workers, in their own per-issue directories (§6.2) — unless the PM kept
    // the previous batch, which then simply stays where it is.
    let mut dirs: Vec<String> = Vec::new();
    let mut created: Vec<String> = Vec::new();
    let count = if keep_workers { 0 } else { queued_worker_role(app, &t, &project.host).await?.0 };
    let spec = if keep_workers { None } else { Some(queued_worker_role(app, &t, &project.host).await?.1) };
    for n in 1..=count {
        let spec = spec.as_ref().expect("spec is loaded when workers are created");
        let dir = format!("{root}/{}", member_dir("worker", n, q.seq));
        tg::worktree_add(app, &project.host, &repo_path(&project, &t.repo), &dir, &t.base_sha, true)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?;
        let nick = member_nick(&t6, "worker", n, q.seq);
        let bot_id = insert_member(app, &project, team_id, &nick, &t6, "worker", q.issue_number, &spec, &dir).await?;
        created.push(bot_id);
        dirs.push(dir);
    }

    // Docs last: they name every member, so the roster has to exist first.
    let issue = refresh_issue(app, &t.project_id, &t.repo, q).await;
    let members = db::team_members(&app.db, team_id).await.map_err(any_err)?;
    let live: Vec<&db::Bot> = members.iter().filter(|b| b.deleted_at.is_none()).collect();
    let roster: Vec<(String, String, String)> = live
        .iter()
        .map(|b| {
            (short_name(&b.name, team_id), b.team_role.clone().unwrap_or_default(), b.cwd.clone().unwrap_or_default())
        })
        .collect();
    let issue_doc = issue_md(&issue);
    if is_unlimited(&t) {
        // §4.5 unlimited: one `ISSUE-<n>.md` per issue, because several are open at once and
        // a single `ISSUE.md` could not say which one a task belongs to. `TEAM.md` lists them
        // all and is rewritten by `refresh_unlimited_docs` once the whole batch has started.
        tg::put_file(app, &project.host, &format!("{root}/ISSUE-{}.md", issue.number), &issue_doc)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?;
        for b in &live {
            if let Some(cwd) = b.cwd.as_deref().filter(|c| !c.is_empty()) {
                tg::write_issue_doc(app, &project.host, cwd, issue.number, &issue_doc)
                    .await
                    .map_err(|e| LcError::Upstream(e.to_string()))?;
            }
        }
    } else {
        let team_doc = team_md(&issue, &branch, &root, &t.repo, &roster);
        tg::put_file(app, &project.host, &format!("{root}/ISSUE.md"), &issue_doc)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?;
        tg::put_file(app, &project.host, &format!("{root}/TEAM.md"), &team_doc)
            .await
            .map_err(|e| LcError::Upstream(e.to_string()))?;
        // Every live worktree, not just the new ones: the PM and the reviewer read their own copy.
        for b in &live {
            if let Some(cwd) = b.cwd.as_deref().filter(|c| !c.is_empty()) {
                tg::write_team_docs(app, &project.host, cwd, &issue_doc, &team_doc)
                    .await
                    .map_err(|e| LcError::Upstream(e.to_string()))?;
            }
        }
    }

    // The queue row becomes current, and `teams` mirrors it (§2.3).
    let now = db::now();
    sqlx::query("UPDATE team_issues SET state = 'working', branch = ?, base_sha = ?, started_at = COALESCE(started_at, ?) WHERE id = ?")
        .bind(&branch)
        .bind(&t.base_sha)
        .bind(&now)
        .bind(&q.id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;
    // §2.3: the `teams` scalars mirror the current issue. In unlimited mode there is no such
    // thing as *the* current issue, so the mirror is only ever the **first** working row (old
    // UI titles); `sync_issue_mirror` re-establishes that once the batch has started.
    sqlx::query(
        "UPDATE teams SET issue_number = ?, issue_title = ?, issue_url = ?, branch = ?,
           pr_url = NULL, summary = NULL, issue_closed_at = NULL WHERE id = ?",
    )
    .bind(issue.number)
    .bind(&issue.title)
    .bind(&issue.url)
    .bind(&branch)
    .bind(team_id)
    .execute(&app.db)
    .await
    .map_err(any_err)?;

    record_event(
        app,
        team_id,
        "note",
        None,
        None,
        None,
        None,
        json!({"action": "issue_started", "issue_number": q.issue_number, "seq": q.seq, "branch": branch}),
    )
    .await?;
    for b in &created {
        app.emit("bot_changed", json!({"bot_id": b})).await;
    }
    let t = load(app, team_id).await?;
    emit_team_changed(app, &t).await;
    app.emit("project_changed", json!({"project_id": t.project_id})).await;
    Ok(())
}

/// Stop, soft-delete and un-worktree the workers of a finished issue. The PM and the reviewer
/// are deliberately left alone — they are what makes this one team rather than a new one.
///
/// Any task still open is written `failed` first: `team_tasks_one_open_per_worker` is unique on
/// the bot alone, so a retired worker holding a non-terminal task would block that row forever.
pub async fn retire_issue_workers(app: &Arc<App>, team_id: &str, issue_id: &str) {
    retire_workers(app, team_id, issue_id, None).await
}

/// §4.5 unlimited: retire **only** the executors of `issue`, matched on the queue position in
/// their §6.2 directory.
///
/// [`retire_issue_workers`] retires the team's whole current batch, which is exactly right
/// when there is one batch (a `done.workers = "keep"` carries `i1-dev-*` into the second
/// issue, and that batch still has to go when *it* finishes) and catastrophic when six issues
/// each have their own — closing #42 would stop #43's executors mid-task.
pub async fn retire_workers_of_issue(app: &Arc<App>, team_id: &str, issue: &db::TeamIssue) {
    retire_workers(app, team_id, &issue.id, Some(issue.seq)).await
}

async fn retire_workers(app: &Arc<App>, team_id: &str, issue_id: &str, only_seq: Option<i64>) {
    let Ok(Some(t)) = db::team(&app.db, team_id).await else { return };
    let Ok(Some(project)) = db::project(&app.db, &t.project_id).await else { return };
    fail_open_tasks(app, team_id, issue_id).await;

    let members = db::team_members(&app.db, team_id).await.unwrap_or_default();
    let keep_only: Option<Vec<String>> =
        only_seq.map(|seq| workers_of_issue(&members, team_id, seq).iter().map(|b| b.id.clone()).collect());
    for b in members.iter().filter(|b| b.deleted_at.is_none() && b.team_role.as_deref() == Some("worker")) {
        if keep_only.as_ref().is_some_and(|ids| !ids.contains(&b.id)) {
            continue;
        }
        // Only this issue's workers: their cwd is the per-issue directory (§6.2).
        let Some(cwd) = b.cwd.clone().filter(|c| !c.is_empty()) else { continue };
        if !cwd.starts_with(&format!("{}/i", t.worktree_root.trim_end_matches('/'))) {
            continue;
        }
        let _ = crate::lifecycle::stop_bot(app, &b.id).await;
        let _ = sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(&b.id)
            .execute(&app.db)
            .await;
        // #61: its hook material goes with it — before the worktree check below, which may
        // `continue` and must not leave the directory behind when it does.
        crate::lifecycle::purge_bot_dir(app, &b.id, &project.host).await;
        if let Err(why) = removable_member_dir(&project, &t.worktree_root, &cwd) {
            tracing::warn!(team = team_id, dir = %cwd, why, "refusing to remove a worker worktree");
            continue;
        }
        tg::worktree_remove(app, &project.host, &repo_path(&project, &t.repo), &cwd).await;
        app.emit("bot_changed", json!({"bot_id": b.id})).await;
    }
    tg::worktree_prune(app, &project.host, &repo_path(&project, &t.repo)).await;
}

/// §4.5 unlimited: make sure issue `q` has at least `needed` executors, building them on
/// demand as the PM dispatches.
///
/// Two hard ceilings, both about panes and quota rather than budget: `MAX_WORKERS` per issue
/// (one reviewer serialises review, so a fifth executor on one issue only queues) and
/// `MAX_TEAM_WORKERS` across the whole team. Hitting either is not an error — the extra tasks
/// simply wait, exactly as they do with a finite parallelism.
///
/// Returns how many executors the issue has afterwards.
pub async fn ensure_workers_for(app: &Arc<App>, team_id: &str, q: &db::TeamIssue, needed: u32) -> LcResult<u32> {
    let t = load(app, team_id).await?;
    if !is_unlimited(&t) {
        return Ok(0);
    }
    let project = db::project(&app.db, &t.project_id)
        .await
        .map_err(any_err)?
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let members: Vec<db::Bot> =
        db::team_members(&app.db, team_id).await.map_err(any_err)?.into_iter().filter(|b| b.deleted_at.is_none()).collect();
    let have = workers_of_issue(&members, team_id, q.seq).len() as u32;
    let want = needed.min(MAX_WORKERS);
    if want <= have {
        return Ok(have);
    }
    let total = members.iter().filter(|b| b.team_role.as_deref() == Some("worker")).count() as u32;
    let room = MAX_TEAM_WORKERS.saturating_sub(total);
    let to = have + (want - have).min(room);
    if to <= have {
        record_event(
            app,
            team_id,
            "note",
            None,
            None,
            None,
            None,
            json!({"action": "worker_cap", "issue_number": q.issue_number, "team_workers": total,
                   "cap": MAX_TEAM_WORKERS}),
        )
        .await?;
        return Ok(have);
    }
    let (_, spec) = queued_worker_role(app, &t, &project.host).await?;
    let integration = q.branch.clone().unwrap_or_else(|| t.branch.clone());
    grow_workers(app, &project, &t, q.seq, &integration, q.issue_number, have, to, &spec).await?;
    Ok(to)
}

/// §2.3 / §4.5: point the `teams` scalar mirror at the **first** working issue. With a finite
/// parallelism that is simply the one issue in flight, which is what `start_issue` already
/// wrote; in unlimited mode it is the one the old single-issue UI shows in the title.
pub async fn sync_issue_mirror(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let Some(first) = db::working_team_issues(&app.db, team_id).await.map_err(any_err)?.into_iter().next() else {
        return Ok(());
    };
    let t = load(app, team_id).await?;
    if t.issue_number == first.issue_number {
        return Ok(());
    }
    sqlx::query("UPDATE teams SET issue_number = ?, issue_title = ?, issue_url = ?, branch = ? WHERE id = ?")
        .bind(first.issue_number)
        .bind(&first.issue_title)
        .bind(&first.issue_url)
        .bind(first.branch.clone().unwrap_or_else(|| t.branch.clone()))
        .bind(team_id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;
    Ok(())
}

/// §4.5 unlimited: rewrite `TEAM.md` — the one page that says which issues are in flight,
/// on which branch, with which executors — into every live member's worktree. Called after a
/// batch of issues starts and after executors are added, so the PM never has to be told in a
/// relay what a file can carry.
pub async fn refresh_unlimited_docs(app: &Arc<App>, team_id: &str) -> LcResult<()> {
    let t = load(app, team_id).await?;
    if !is_unlimited(&t) {
        return Ok(());
    }
    let project = db::project(&app.db, &t.project_id)
        .await
        .map_err(any_err)?
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let members: Vec<db::Bot> =
        db::team_members(&app.db, team_id).await.map_err(any_err)?.into_iter().filter(|b| b.deleted_at.is_none()).collect();
    let working = db::working_team_issues(&app.db, team_id).await.map_err(any_err)?;
    let rows: Vec<(i64, String, String, Vec<String>)> = working
        .iter()
        .map(|q| {
            let who = workers_of_issue(&members, team_id, q.seq)
                .iter()
                .map(|b| short_name(&b.name, team_id))
                .collect::<Vec<_>>();
            (q.issue_number, q.issue_title.clone(), q.branch.clone().unwrap_or_default(), who)
        })
        .collect();
    let roster: Vec<(String, String, String)> = members
        .iter()
        .map(|b| {
            (short_name(&b.name, team_id), b.team_role.clone().unwrap_or_default(), b.cwd.clone().unwrap_or_default())
        })
        .collect();
    let doc = team_md_unlimited(&rows, &t.worktree_root, &t.repo, &roster);
    tg::put_file(app, &project.host, &format!("{}/TEAM.md", t.worktree_root), &doc)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    for b in &members {
        if let Some(cwd) = b.cwd.as_deref().filter(|c| !c.is_empty()) {
            tg::put_file(app, &project.host, &format!("{}/.agents-manager/team/TEAM.md", cwd.trim_end_matches('/')), &doc)
                .await
                .map_err(|e| LcError::Upstream(e.to_string()))?;
        }
    }
    Ok(())
}

/// Close out whatever an issue left open: `team_tasks_one_open_per_worker` is unique on the
/// worker, so a stale open task would block the next dispatch to the same executor.
pub async fn fail_open_tasks(app: &Arc<App>, team_id: &str, issue_id: &str) {
    let _ = sqlx::query(
        "UPDATE team_tasks SET state = 'failed', updated_at = ?
          WHERE team_id = ? AND issue_id = ? AND state NOT IN ('merged','skipped','failed')",
    )
    .bind(db::now())
    .bind(team_id)
    .bind(issue_id)
    .execute(&app.db)
    .await;
}

fn role_spec_json(r: &CheckedRole) -> Value {
    json!({
        "kind": r.kind, "model": r.model, "effort": r.effort, "fast": r.fast,
        "identity": r.identity, "persona_extra": r.persona_extra,
    })
}

/// Insert one team member straight into `bots` — **not** through config.toml (SPEC-team §5.3).
#[allow(clippy::too_many_arguments)]
async fn insert_member(
    app: &Arc<App>,
    project: &db::Project,
    team_id: &str,
    nick: &str,
    tid6: &str,
    role: &str,
    issue_number: i64,
    spec: &CheckedRole,
    cwd: &str,
) -> LcResult<String> {
    // §7.3: a second team on the same issue would collide on the nickname; disambiguate with
    // the team hash before giving up.
    let mut name = nick.to_string();
    if name_taken(app, &project.id, &name).await? {
        name = format!("{nick}-{tid6}");
        if name_taken(app, &project.id, &name).await? {
            return Err(LcError::conflict("team member name already in use", json!({"name": name})));
        }
    }
    if !crate::config::valid_bot_name(&name) {
        return Err(LcError::Bad(format!("team member name `{name}`: {}", crate::config::BOT_NAME_RE)));
    }
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, model, effort, fast, persona, args_json, autostart,
           inject_hooks, auto_approve, identity, env_json, managed_by, team_id, team_role, cwd, hook_token, created_at)
         VALUES (?,?,?,?,?,?,?,?,'[]',0,1,1,?,'{}','team',?,?,?,?,?)",
    )
    .bind(&id)
    .bind(&project.id)
    .bind(&name)
    .bind(&spec.kind)
    .bind(&spec.model)
    .bind(&spec.effort)
    .bind(spec.fast as i64)
    .bind(role_persona(role, issue_number, spec.persona_extra.as_deref()))
    .bind(&spec.identity)
    .bind(team_id)
    .bind(role)
    // §2.2: the member's own worktree. `start_inner` already puts `pane.split` here.
    .bind(cwd)
    .bind(crate::projection::new_token())
    .bind(db::now())
    .execute(&app.db)
    .await
    .map_err(any_err)?;
    db::conversation_id(&app.db, &id).await.map_err(any_err)?;
    Ok(id)
}

async fn name_taken(app: &Arc<App>, project_id: &str, name: &str) -> LcResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM bots WHERE project_id = ? AND name = ? AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(name)
    .fetch_one(&app.db)
    .await
    .map_err(any_err)?;
    Ok(n > 0)
}

/// Creation failed partway (SPEC-team §6.5: "建 team 失敗（任何一步）走同一個 cleanup").
/// Soft-delete the members, unwind whatever worktrees exist, and park the team in `failed`
/// so the failure is visible and `cleanup` still works on it.
#[allow(clippy::too_many_arguments)]
async fn rollback_create(
    app: &Arc<App>,
    project: &db::Project,
    repo: &str,
    team_id: &str,
    created: &[String],
    root: &str,
    dirs: &[String],
    workspace_id: Option<&str>,
    branch: &str,
    base_sha: &str,
    branch_created: bool,
) {
    let now = db::now();
    for b in created {
        let _ = sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(&now).bind(b).execute(&app.db).await;
        // #61: a member that never got going still had its hook material written.
        crate::lifecycle::purge_bot_dir(app, b, &project.host).await;
    }
    close_workspace(app, project, workspace_id).await;
    remove_worktrees(app, project, repo, root, dirs).await;
    if branch_created {
        remove_empty_created_branch(app, project, repo, branch, base_sha).await;
    }
    let _ = sqlx::query("UPDATE teams SET phase='failed', pause_reason=NULL, ended_at=? WHERE id=?")
        .bind(&now)
        .bind(team_id)
        .execute(&app.db)
        .await;
}

/// Roll back only the integration branch created by this attempt, and only while it still
/// points at the resolved base. A branch with any commit is user work, even if the team later
/// failed to finish creating, so it stays available for recovery.
async fn remove_empty_created_branch(app: &Arc<App>, project: &db::Project, repo: &str, branch: &str, base_sha: &str) {
    let git_dir = repo_path(project, repo);
    let range = format!("{base_sha}..{branch}");
    let count = match tg::git(app, &project.host, &git_dir, &["rev-list", "--count", &range], tg::GIT_TIMEOUT).await {
        Ok(out) if out.ok() => match out.trimmed().parse::<i64>() {
            Ok(count) => count,
            Err(e) => {
                tracing::warn!(branch = %branch, error = %e, "team create rollback: could not inspect integration branch; keeping it");
                return;
            }
        },
        Ok(out) => {
            tracing::warn!(branch = %branch, error = %out.message(), "team create rollback: could not inspect integration branch; keeping it");
            return;
        }
        Err(e) => {
            tracing::warn!(branch = %branch, error = %e, "team create rollback: could not inspect integration branch; keeping it");
            return;
        }
    };
    if count != 0 {
        tracing::warn!(branch = %branch, commits = count, "team create rollback: keeping integration branch with work");
        return;
    }
    match tg::git(app, &project.host, &git_dir, &["branch", "-D", branch], tg::GIT_TIMEOUT).await {
        Ok(out) if out.ok() => {}
        Ok(out) => tracing::warn!(branch = %branch, error = %out.message(), "team create rollback: branch -D failed"),
        Err(e) => tracing::warn!(branch = %branch, error = %e, "team create rollback: branch -D failed"),
    }
}

// ---------------------------------------------------------------- the team's workspace (§6.4a)

/// The herdr client and session a team's panes live on. A team is a project-local thing, so
/// it always uses the host's configured named session — never the user's `default` one.
async fn team_herdr(app: &Arc<App>, project: &db::Project) -> LcResult<(crate::herdr::HerdrClient, String)> {
    let session = app
        .session_for_host(&project.host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{}` is not configured", project.host)))?;
    if !app.session_connected(&project.host, &session).await {
        return Err(LcError::conflict("host is not connected", json!({"host": project.host})));
    }
    let client = app
        .herdr_for_session(&project.host, &session)
        .await
        .ok_or_else(|| LcError::Upstream(format!("Herdr session `{session}` is not configured")))?;
    Ok((client, session))
}

/// SPEC-team §6.4a: `workspace.create` rooted at the team directory, labelled so the user can
/// tell it apart from their project workspace at a glance.
async fn make_workspace(
    app: &Arc<App>,
    project: &db::Project,
    team_id: &str,
    root: &str,
    issue: &IssueRef,
) -> LcResult<String> {
    let (client, _) = team_herdr(app, project).await?;
    let label = format!("{} · team #{} {}", project.label, issue.number, issue.title);
    let label: String = label.chars().take(80).collect();
    let (ws, _root_pane) = client
        .workspace_create(root, &label, json!({}))
        .await
        .map_err(|e| LcError::Upstream(format!("could not create the team workspace: {e}")))?;
    tracing::info!(team = team_id, workspace = %ws.workspace_id, "created the team workspace");
    Ok(ws.workspace_id)
}

/// `workspace.close` — one call takes every member pane with it, which is the point of
/// giving a team its own workspace in the first place (§6.4a).
async fn close_workspace(app: &Arc<App>, project: &db::Project, workspace_id: Option<&str>) {
    let Some(ws) = workspace_id.filter(|w| !w.trim().is_empty()) else { return };
    let Ok((client, _)) = team_herdr(app, project).await else { return };
    if let Err(e) = client.workspace_close(ws).await {
        tracing::warn!(workspace = ws, error = %e, "closing the team workspace failed");
    }
}

/// SPEC-team §6.5, and **the order is the whole point**: `worktree remove` (which deletes
/// the `.git/worktrees/<name>/` registration) → `worktree prune` → delete the directory.
/// Removing the directory first leaves an orphan registration that `git worktree list`
/// reports as `prunable` for ever, and blocks re-using the same path without `add -f`.
///
/// **Both destructive calls are guarded by the §6.1 containment invariant**, because both of
/// them take a raw database value and hand it to something that deletes without asking:
/// `worktree remove --force --force` throws away uncommitted work in whatever it is pointed
/// at, and `remove_dir` is a literal `rm -rf` of `teams.worktree_root`. `bots.cwd` and
/// `teams.worktree_root` are not trustworthy on their own — a team written by a build that
/// predated the worktree layout had `worktree_root = ''` and every member pointing at the
/// user's main checkout. Today git happens to refuse `worktree remove` on a *main* working
/// tree, but it will happily remove a linked worktree the user made by hand, and `rm -rf`
/// refuses nothing at all. So: a directory is only removed if it is inside the team root and
/// outside the checkout (`removable_member_dir`), and the root is only deleted if it is not
/// the checkout, not inside it, and not a parent of it (`removable_root`).
async fn remove_worktrees(app: &Arc<App>, project: &db::Project, repo: &str, root: &str, dirs: &[String]) {
    let git_dir = repo_path(project, repo);
    let root = root.trim();
    if root.is_empty() {
        return;
    }
    for d in dirs {
        if let Err(why) = removable_member_dir(project, root, d) {
            tracing::warn!(worktree = %d, root = %root, project = %project.path, %why,
                           "refusing to `worktree remove` a directory outside the team root");
            continue;
        }
        let out = tg::worktree_remove(app, &project.host, &git_dir, d).await;
        if !out.ok() {
            tracing::debug!(worktree = %d, error = %out.message(), "worktree remove reported an error");
        }
    }
    tg::worktree_prune(app, &project.host, &git_dir).await;
    if let Err(why) = removable_root(project, root) {
        tracing::warn!(root = %root, project = %project.path, %why,
                       "refusing to delete the team root: it is not safely outside the project checkout");
        return;
    }
    tg::remove_dir(app, &project.host, root).await;
}

/// May `worktree remove --force --force` be pointed at `dir`? Only inside the team root, and
/// never inside the user's checkout — the same two tests `checked_member_cwd` applies before
/// any git command runs in a member's cwd, so a directory that was never legal to *work* in
/// is not legal to *delete* either.
pub fn removable_member_dir(project: &db::Project, root: &str, dir: &str) -> Result<(), String> {
    let dir = dir.trim();
    if dir.is_empty() {
        return Err("empty path".into());
    }
    if is_within(&project.path, dir) {
        return Err(format!("`{dir}` is inside the project checkout `{}`", project.path));
    }
    if !is_within(root, dir) {
        return Err(format!("`{dir}` is outside the team root `{root}`"));
    }
    Ok(())
}

/// May the team root be `rm -rf`'d? Not if it is the checkout, inside it, or above it.
/// The last case is the one string equality misses: a `worktree_root` of `/Users/me` is
/// neither equal to nor inside the checkout, and deleting it takes the checkout with it.
pub fn removable_root(project: &db::Project, root: &str) -> Result<(), String> {
    let root = root.trim();
    if root.is_empty() {
        return Err("empty worktree_root".into());
    }
    if is_within(&project.path, root) {
        return Err(format!("team root `{root}` is the project checkout `{}` or inside it", project.path));
    }
    if is_within(root, &project.path) {
        return Err(format!("team root `{root}` contains the project checkout `{}`", project.path));
    }
    Ok(())
}

// ---------------------------------------------------------------- reads (§10.3, §10.4)

/// `GET /api/teams/:id`.
pub async fn detail(app: &Arc<App>, team_id: &str) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    let tasks = db::team_tasks(&app.db, &t.id).await.map_err(any_err)?;
    let mut v = team_json(app, &t).await;
    if let Some(o) = v.as_object_mut() {
        o.insert("tasks".into(), json!(tasks.iter().map(task_json).collect::<Vec<_>>()));
        o.insert("base_ref".into(), json!(t.base_ref));
        o.insert("base_sha".into(), json!(t.base_sha));
        o.insert("worktree_root".into(), json!(t.worktree_root));
        o.insert(
            "roles".into(),
            serde_json::from_str::<Value>(&t.roles_json).unwrap_or_else(|_| json!({})),
        );
    }
    Ok(v)
}

/// `GET /api/teams/:id/events?before=&limit=` — paginated backwards, returned ascending
/// (the same contract as `GET /projects/:id/messages`: `before` is the **id** of the oldest
/// event the caller already holds).
///
/// Ordering is by `team_events.seq`, not by the ULID. A ULID's timestamp only has millisecond
/// resolution, and one scheduler step writes several events inside a single millisecond — the
/// relative order of those ULIDs is then random, so both "oldest first" and the `before=`
/// cursor were non-deterministic. `seq` is the per-team write order, so a page is exactly the
/// slice the caller asked for, with no gaps and no repeats.
pub async fn events(app: &Arc<App>, team_id: &str, before: Option<&str>, limit: i64) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    let limit = limit.clamp(1, 500);
    // The cursor stays an event id (the front end just echoes `events[0].id` back); it is
    // resolved to that event's `seq` here.
    let before_seq: Option<i64> = match before {
        None => None,
        Some(b) => Some(
            sqlx::query_scalar::<_, i64>("SELECT seq FROM team_events WHERE team_id = ? AND id = ?")
                .bind(&t.id)
                .bind(b)
                .fetch_optional(&app.db)
                .await
                .map_err(any_err)?
                .ok_or_else(|| LcError::Bad(format!("`before` is not an event of this team: {b}")))?,
        ),
    };
    let rows = match before_seq {
        Some(s) => sqlx::query_as::<_, db::TeamEvent>(
            "SELECT * FROM team_events WHERE team_id = ? AND seq < ? ORDER BY seq DESC LIMIT ?",
        )
        .bind(&t.id)
        .bind(s)
        .bind(limit + 1)
        .fetch_all(&app.db)
        .await,
        None => sqlx::query_as::<_, db::TeamEvent>(
            "SELECT * FROM team_events WHERE team_id = ? ORDER BY seq DESC LIMIT ?",
        )
        .bind(&t.id)
        .bind(limit + 1)
        .fetch_all(&app.db)
        .await,
    }
    .map_err(any_err)?;
    let has_more = rows.len() as i64 > limit;
    let mut evs: Vec<db::TeamEvent> = rows.into_iter().take(limit as usize).collect();
    evs.reverse();
    Ok(json!({
        "team_id": t.id,
        "events": evs.iter().map(event_json).collect::<Vec<_>>(),
        "has_more": has_more,
    }))
}

// ---------------------------------------------------------------- control (§10.5)

/// `POST /api/teams/:id/pause`. Idempotent: pausing a paused team is a no-op.
pub async fn pause(app: &Arc<App>, team_id: &str) -> LcResult<Value> {
    // Compare-and-set (#31): the scheduler may move the phase between the read and the write,
    // and `resume_phase` has to be the phase actually left, not the one read a moment ago.
    loop {
        let t = load(app, team_id).await?;
        if is_terminal(&t.phase) {
            return Err(LcError::conflict("team already finished", json!({"phase": t.phase})));
        }
        if t.phase == "paused" {
            return Ok(json!({}));
        }
        let from = Some(t.phase.as_str());
        if write_phase(app, team_id, "paused", Some("user"), from, None, from).await?.is_some() {
            return Ok(json!({}));
        }
    }
}

/// `POST /api/teams/:id/resume`.
pub async fn resume(app: &Arc<App>, team_id: &str) -> LcResult<Value> {
    resume_inner(app, team_id, false).await
}

/// SPEC-team §11：成員離開 `blocked` 後自動 resume——這是唯一會自動 resume 的暫停原因
/// （它不是預算問題，人回答完提示就該繼續）。
///
/// 2026-09-10 使用者：codex 的升級選單卡住成員 → team `paused(member_blocked:<name>)`；
/// 在那個成員自己的畫面回答完（不是走暫停橫幅那顆按鈕）之後，team 仍然停在那裡不動，
/// 因為補送 `resume` 的邏輯只長在那顆按鈕上。改在 daemon 做，回答的地方就不重要了。
///
/// 只在 `pause_reason` 正好指著這個成員時動作；其他原因（預算、額度、gate）不碰。
///
/// 回傳的是 **boxed future**，不是 `async fn`：`resume` 會走到 `lifecycle` → `events`，
/// 而呼叫這裡的也是 `events`，`async fn` 的 opaque type 會被 rustc 判成循環（E0391）。
/// 裝箱把型別擦掉就切斷了那個環。
pub fn resume_if_member_unblocked(
    app: &Arc<App>,
    bot_id: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    let (app, bot_id) = (app.clone(), bot_id.to_string());
    Box::pin(async move {
        let Ok(Some(bot)) = db::bot(&app.db, &bot_id).await else { return };
        let Some(team_id) = bot.team_id.clone() else { return };
        let Ok(Some(t)) = db::team(&app.db, &team_id).await else { return };
        if t.phase != "paused" || t.pause_reason.as_deref() != Some(format!("member_blocked:{}", bot.name).as_str()) {
            return;
        }
        match resume(&app, &team_id).await {
            Ok(_) => tracing::info!(team = %team_id, member = %bot.name, "member left blocked: team resumed"),
            Err(e) => tracing::warn!(team = %team_id, member = %bot.name, error = ?e, "auto-resume after unblock failed"),
        }
    })
}

/// `POST /api/teams/:id/approve` — release one supervised gate (SPEC-team §4.6).
pub async fn approve(app: &Arc<App>, team_id: &str) -> LcResult<Value> {
    resume_inner(app, team_id, true).await
}

async fn resume_inner(app: &Arc<App>, team_id: &str, gate_only: bool) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    if t.phase != "paused" {
        return Err(LcError::conflict("team is not paused", json!({"phase": t.phase})));
    }
    let reason = t.pause_reason.clone().unwrap_or_default();
    if gate_only && !reason.starts_with("gate:") {
        return Err(LcError::conflict("team is not waiting at a supervised gate", json!({"pause_reason": reason})));
    }
    // Before anything else: a team is only allowed to move again if its layout still holds.
    // Pressing "continue" on an orphaned team used to revive it — including one whose members
    // pointed at the user's own checkout — because `resume` went straight to
    // `spawn_scheduler` without re-running the checks the boot path does.
    if let Some(project) = db::project(&app.db, &t.project_id).await.map_err(any_err)? {
        if let Err(e) = check_workspace(app, &project, &t).await {
            return Err(LcError::conflict("the team's workspace is gone", json!({"detail": e})));
        }
        if let Err(e) = check_worktrees(app, &project, &t).await {
            return Err(LcError::conflict("the team's worktrees are not usable", json!({"detail": e})));
        }
    }
    // §10.5: resuming out of `member_lost` needs the member back first.
    if reason.starts_with("member_lost") {
        for b in db::team_members(&app.db, &t.id).await.map_err(any_err)? {
            if b.deleted_at.is_some() {
                continue;
            }
            if db::active_run(&app.db, &b.id).await.map_err(any_err)?.is_none() {
                return Err(LcError::conflict("member not running", json!({"bot_id": b.id, "name": b.name})));
            }
        }
    }
    // §4.6: releasing a gate is recorded, so the scheduler can tell "this gate was approved"
    // from "this gate has never been raised" after a restart, with no in-memory flag.
    if gate_only {
        let gate = reason.trim_start_matches("gate:").to_string();
        record_event(app, team_id, "note", None, None, None, None, json!({"action": "gate_release", "gate": gate}))
            .await?;
    }
    let back = t.resume_phase.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "planning".into());
    set_phase(app, team_id, &back, None, None).await?;
    if reason == "pm_stalled" {
        crate::team_sched::nudge_pm_after_resume(app, team_id).await?;
    }
    // §4.5: releasing `gate:dispatch` is what actually starts the batch the PM planned, so
    // the branches are cut and the relays queued here rather than on some later pass.
    if let Err(e) = crate::team_sched::fill_now(app, team_id).await {
        tracing::warn!(team = %team_id, error = ?e, "could not hand out queued tasks on resume");
    }
    spawn_scheduler(app, team_id);
    Ok(json!({}))
}

/// `POST /api/teams/:id/abort` — the only irreversible user action (SPEC-team §9.2).
pub async fn abort(app: &Arc<App>, team_id: &str, reason: Option<&str>) -> LcResult<Value> {
    // Compare-and-set (#31): a team the scheduler finished between the read and the write is
    // `done`, and an abort must not rewrite that into `aborted`.
    let t = loop {
        let t = load(app, team_id).await?;
        if is_terminal(&t.phase) {
            return Err(LcError::conflict("team already finished", json!({"phase": t.phase})));
        }
        if write_phase(app, team_id, "aborting", None, None, None, Some(&t.phase)).await?.is_some() {
            break t;
        }
    };
    record_event(app, team_id, "note", None, None, None, None, json!({"action": "abort", "reason": reason})).await?;
    for b in db::team_members(&app.db, &t.id).await.map_err(any_err)? {
        if b.deleted_at.is_some() {
            continue;
        }
        if let Err(e) = lifecycle::stop_bot(app, &b.id).await {
            tracing::warn!(bot = %b.name, error = ?e, "team abort: stop_bot failed");
        }
    }
    set_phase(app, team_id, "aborted", None, None).await?;
    Ok(json!({}))
}

/// The comment `close_issue` leaves on the issue when the caller does not supply one.
///
/// It is written for someone reading the issue a month later with no access to this machine:
/// what was done, where the code actually is, and — the part that matters most — whether it
/// has reached the default branch yet. A `deliver=branch` team leaves the work on a local
/// branch only, so the comment says so instead of implying the fix has shipped.
pub fn close_comment(t: &db::Team, tasks: &[db::TeamTask]) -> String {
    let mut s = String::from("已由 agents-manager 的 issue team 完成。\n\n");
    if let Some(sum) = t.summary.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        s.push_str(sum);
        s.push_str("\n\n");
    }
    // `base_ref` is `HEAD` by default, which means nothing to someone reading the issue on
    // github.com — name it the way a reader would.
    let head_base = t.base_ref.trim().eq_ignore_ascii_case("HEAD");
    let base = if head_base { "預設分支".to_string() } else { format!("`{}`", t.base_ref) };
    // CJK runs together without a space, but a Latin/backtick run needs one before it.
    let base_in_prose = if head_base { base.clone() } else { format!(" {base}") };
    s.push_str(&format!("- 整合分支：`{}`（base {base} @ {}）\n", t.branch, short_sha(&t.base_sha)));
    for task in tasks.iter().filter(|x| x.state == "merged") {
        let sha = task.merge_sha.as_deref().map(short_sha).unwrap_or_default();
        s.push_str(&format!("- 已合併 t{}「{}」{}\n", task.seq, task.title, if sha.is_empty() {
            String::new()
        } else {
            format!("：{sha}")
        }));
    }
    match t.pr_url.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        Some(url) => s.push_str(&format!("- PR：{url}\n")),
        // Said plainly, because closing an issue whose fix is not on the default branch is
        // exactly the case where a future reader needs to know where to look.
        None => s.push_str(&format!(
            "\n這條分支**還沒有合併進{base_in_prose}、也沒有開 PR**；若最後沒有採用，請重開這個 issue。\n"
        )),
    }
    s
}

/// The same default comment, but with the branch/summary/PR belonging to a particular queued
/// issue rather than the scalar mirror on `teams`.
pub fn close_comment_for_issue(t: &db::Team, issue: &db::TeamIssue, tasks: &[db::TeamTask]) -> String {
    let mut mirror = t.clone();
    mirror.issue_number = issue.issue_number;
    mirror.issue_title = issue.issue_title.clone();
    // A reopened Team's scalar columns now describe the next issue. The queue row is the
    // source of truth for a historical close comment; falling back to the mirror would leak
    // the next issue's summary or PR when the finished row intentionally has neither.
    mirror.branch = issue.branch.clone().unwrap_or_default();
    mirror.base_sha = issue.base_sha.clone().unwrap_or_default();
    mirror.summary = issue.summary.clone();
    mirror.pr_url = issue.pr_url.clone();
    close_comment(&mirror, tasks)
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// `POST /api/teams/:id/close-issue` — close the team's GitHub issue, on the user's say-so.
///
/// SPEC-team §10.7. Two rules make this safe to have at all:
///
/// 1. **Only a finished team.** `done` means the PM declared the work complete *and* the
///    daemon verified every task reached a terminal state; an aborted or failed team has no
///    business closing anything.
/// 2. **Only a human.** Nothing in the scheduler calls this. The endpoint exists so the UI can
///    offer the action once, and the issue closes because someone pressed the button.
///
/// Already closed on GitHub (someone did it by hand, or a `deliver=pr` PR closed it) is
/// success, not an error: the row is stamped so the offer stops being shown.
pub async fn close_issue(app: &Arc<App>, team_id: &str, comment: Option<String>) -> LcResult<Value> {
    close_issue_for(app, team_id, None, comment).await
}

/// Close one queued issue. `issue_id = None` preserves the original endpoint's meaning — the
/// row mirrored by `teams.issue_number`; the IssueQueue passes an id so a finished Team can
/// close an earlier issue without accidentally acting on the latest one.
pub async fn close_issue_for(
    app: &Arc<App>,
    team_id: &str,
    issue_id: Option<&str>,
    comment: Option<String>,
) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    let target = match issue_id {
        Some(id) => db::team_issue(&app.db, id).await.map_err(any_err)?.filter(|i| i.team_id == team_id),
        // No `issue_id` means "the one `teams` mirrors", i.e. the latest pass on that number —
        // §2.3 lets the same issue be queued more than once, so search from the back.
        None => db::team_issues(&app.db, team_id)
            .await
            .map_err(any_err)?
            .into_iter()
            .rev()
            .find(|i| i.issue_number == t.issue_number),
    }
    .ok_or_else(|| LcError::NotFound("issue".into()))?;
    if target.state != "done" {
        return Err(LcError::conflict(
            "the issue can only be closed from a finished team",
            json!({"state": target.state, "phase": t.phase}),
        ));
    }
    if let Some(at) = &target.issue_closed_at {
        return Err(LcError::conflict("the issue was already closed from this team", json!({"closed_at": at})));
    }
    let tasks = db::team_tasks_of_issue(&app.db, &t.id, &target.id).await.map_err(any_err)?;
    // An explicit empty string is "close it without a comment"; absent is "write the default".
    let body = match comment {
        Some(c) if c.trim().is_empty() => None,
        Some(c) => Some(c),
        None => Some(close_comment_for_issue(&t, &target, &tasks)),
    };
    let out = crate::github::close_issue(app, &t.project_id, &t.repo, target.issue_number as u64, body.as_deref()).await?;

    let now = db::now();
    sqlx::query("UPDATE team_issues SET issue_closed_at = ? WHERE id = ?")
        .bind(&now)
        .bind(&target.id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;
    // Keep the legacy mirror in sync only when the target is the mirrored/current issue.
    if target.issue_number == t.issue_number {
        sqlx::query("UPDATE teams SET issue_closed_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&t.id)
            .execute(&app.db)
            .await
            .map_err(any_err)?;
    }
    record_event(
        app,
        &t.id,
        "note",
        None,
        None,
        None,
        None,
        json!({
            "action": "issue_closed",
            "by": "user",
            "number": target.issue_number,
            "url": out.get("url").cloned().unwrap_or(Value::Null),
            "already_closed": out.get("already_closed").cloned().unwrap_or(Value::Bool(false)),
            "comment": body,
        }),
    )
    .await?;
    let t = load(app, team_id).await?;
    emit_team_changed(app, &t).await;
    Ok(out)
}

/// `POST /api/teams/:id/cleanup` — terminal teams only (SPEC-team §6.5, §12 #6: never automatic).
pub async fn cleanup(app: &Arc<App>, team_id: &str) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    if !is_terminal(&t.phase) {
        return Err(LcError::conflict("team is still running", json!({"phase": t.phase})));
    }
    let now = db::now();
    let mut removed = Vec::new();
    // Collected before the members are soft-deleted: `bots.cwd` *is* the worktree list.
    let members = db::team_members(&app.db, &t.id).await.map_err(any_err)?;
    let dirs: Vec<String> =
        members.iter().filter_map(|b| b.cwd.clone()).filter(|c| !c.trim().is_empty()).collect();
    for b in members {
        if b.deleted_at.is_none() {
            let _ = lifecycle::stop_bot(app, &b.id).await;
            sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?")
                .bind(&now)
                .bind(&b.id)
                .execute(&app.db)
                .await
                .map_err(any_err)?;
            let host = db::bot_host(&app.db, &b.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
            lifecycle::purge_bot_dir(app, &b.id, &host).await;
        }
        removed.push(b.id.clone());
        app.emit("bot_changed", json!({"bot_id": b.id})).await;
    }
    // §6.5: remove → prune → rmdir. Branches (integration and task) are kept for ever by
    // design — they are cheap, they are the audit trail, and deleting the user's history is
    // not the daemon's call.
    if let Ok(Some(project)) = db::project(&app.db, &t.project_id).await {
        // §6.4a: one `workspace.close` takes every member pane with it.
        close_workspace(app, &project, t.workspace_id.as_deref()).await;
        remove_worktrees(app, &project, &t.repo, &t.worktree_root, &dirs).await;
    }
    let _ = sqlx::query("UPDATE teams SET workspace_id = NULL WHERE id = ?").bind(team_id).execute(&app.db).await;
    record_event(
        app,
        team_id,
        "note",
        None,
        None,
        None,
        None,
        json!({"action": "cleanup", "members": removed, "worktrees_removed": dirs, "branches_kept": t.branch}),
    )
    .await?;
    let t = load(app, team_id).await?;
    emit_team_changed(app, &t).await;
    app.emit("project_changed", json!({"project_id": t.project_id})).await;
    Ok(json!({}))
}

/// `DELETE /api/teams/:id?branches=keep|delete` — SPEC-team §6.5a.
///
/// `cleanup` tidies the site and **keeps the record**; `delete` removes the record too. Both
/// exist because they answer different questions: "I am done looking at this" versus "get it
/// off my screen".
///
/// Three things it deliberately does **not** destroy:
///
/// * the members' conversations — the same rule as deleting a Bot (SPEC §6.4). What was
///   removed is the container and its scheduling log, not what anyone said;
/// * the branches, unless `branches=delete` was asked for explicitly. That flag is the only
///   path in the whole feature that throws work away;
/// * anything on a remote. A pushed branch is outward-facing and only a human retracts it.
///
/// Any phase may be deleted (unlike `cleanup`): a live team is stopped on the way out.
/// Idempotent — a second delete is a 404 with `{"error":"not_found","what":"team"}`.
pub async fn delete(app: &Arc<App>, team_id: &str, delete_branches: bool) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    let project = db::project(&app.db, &t.project_id).await.map_err(any_err)?;
    let members = db::team_members(&app.db, &t.id).await.map_err(any_err)?;
    let dirs: Vec<String> =
        members.iter().filter_map(|b| b.cwd.clone()).filter(|c| !c.trim().is_empty()).collect();

    // §6.5a: the UI gets a "this is happening" frame before any of the slow parts. `deleting`
    // is not a `teams.phase` value (the CHECK constraint has no such state and the row is
    // about to be gone anyway) — it exists only on the wire.
    app.emit(
        "team_changed",
        json!({"team_id": t.id, "project_id": t.project_id, "phase": "deleting", "pause_reason": null,
               "usage": serde_json::from_str::<Value>(&t.usage_json).unwrap_or_else(|_| empty_usage())}),
    )
    .await;

    // 1. stop the scheduler, and stop it *first*. Its loop only ends when `tick` reads a
    // terminal phase, so a live team deleted underneath it would leave the loop failing
    // `team::load` and warning every 20s for ever — and, worse, acting on rows in the middle
    // of being removed. The phase goes terminal before the task is stopped so that a
    // `resume` / `decide` racing this delete cannot bring a new loop back either.
    if !is_terminal(&t.phase) {
        let _ = sqlx::query("UPDATE teams SET phase='aborted', ended_at=COALESCE(ended_at, ?) WHERE id=?")
            .bind(db::now())
            .bind(team_id)
            .execute(&app.db)
            .await;
    }
    crate::team_sched::stop(team_id).await;

    // 2. stop the members. A non-terminal team is aborted on the way out.
    //
    // Every step here is best-effort, on purpose: this whole function's contract is "the team
    // record goes away", and a transient failure on any one member (a busy connection, a slow
    // host) must not leave the team stuck forever with no way to retry cleanly. A `?` here once
    // did exactly that — one failed `UPDATE` aborted the function before the `teams` row was
    // ever touched, and the only way out was manual SQL surgery on the live database.
    let now = db::now();
    let mut member_ids = Vec::new();
    for b in &members {
        if b.deleted_at.is_none() {
            let _ = lifecycle::stop_bot(app, &b.id).await;
            if let Err(e) = sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
                .bind(&now)
                .bind(&b.id)
                .execute(&app.db)
                .await
            {
                tracing::warn!(bot = %b.id, error = %e, "team delete: soft-deleting a member failed");
            }
            let host = db::bot_host(&app.db, &b.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
            lifecycle::purge_bot_dir(app, &b.id, &host).await;
        }
        // The bot row and its conversation stay: `deleted_at` is the same soft delete
        // `DELETE /bots/:id` uses, so the timeline survives (SPEC §6.4).
        member_ids.push(b.id.clone());
    }

    // 3. worktrees (remove → prune → rmdir, §6.5), then the workspace (§6.4a).
    let mut removed_branches: Vec<String> = Vec::new();
    if let Some(p) = &project {
        remove_worktrees(app, p, &t.repo, &t.worktree_root, &dirs).await;
        close_workspace(app, p, t.workspace_id.as_deref()).await;
        if delete_branches {
            // The only destructive path there is. Task branches first, then the integration
            // branch, so a `-D` failure on one leaves the rest recoverable. Remote branches
            // are never touched — pushing was a human decision and so is unpushing.
            let tasks = db::team_tasks(&app.db, &t.id).await.unwrap_or_default();
            for b in tasks.iter().map(|x| x.branch.clone()).chain(std::iter::once(t.branch.clone())) {
                // Belt to the braces of `branches=delete` being opt-in: only ever a branch
                // this feature could have created. A hand-edited `team_tasks.branch` must
                // not be able to turn a delete into "remove the user's work".
                if !b.starts_with("team/") {
                    tracing::warn!(branch = %b, "team delete: refusing to remove a branch outside `team/`");
                    continue;
                }
                let out = tg::git(app, &p.host, &repo_path(p, &t.repo), &["branch", "-D", &b], tg::GIT_TIMEOUT).await;
                match out {
                    Ok(o) if o.ok() => removed_branches.push(b),
                    Ok(o) => tracing::warn!(branch = %b, error = %o.message(), "team delete: branch -D failed"),
                    Err(e) => tracing::warn!(branch = %b, error = %e, "team delete: branch -D failed"),
                }
            }
        }
    }

    // 4. the rows. Children first — `team_events` / `team_tasks` / `team_issues` all
    // reference `teams(id)`, and foreign keys are on (`db::open`).
    sqlx::query("DELETE FROM team_events WHERE team_id = ?").bind(team_id).execute(&app.db).await.map_err(any_err)?;
    sqlx::query("DELETE FROM team_tasks WHERE team_id = ?").bind(team_id).execute(&app.db).await.map_err(any_err)?;
    sqlx::query("DELETE FROM team_issues WHERE team_id = ?").bind(team_id).execute(&app.db).await.map_err(any_err)?;
    sqlx::query("DELETE FROM teams WHERE id = ?").bind(team_id).execute(&app.db).await.map_err(any_err)?;
    // The bots keep `team_id` so their surviving messages can still say which team they were
    // in; they are soft-deleted, so nothing lists them any more.

    for b in &member_ids {
        app.emit("bot_changed", json!({"bot_id": b})).await;
    }
    app.emit("team_changed", json!({"team_id": t.id, "project_id": t.project_id, "deleted": true})).await;
    app.emit("project_changed", json!({"project_id": t.project_id})).await;
    Ok(json!({"deleted": true, "branches_deleted": removed_branches}))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PatchTeam {
    /// 使用者取的短名。`Some("")` / `Some(null)` = 清掉，改回 `#編號 issue 標題`。
    /// 跟其他欄位不同，已經結束的 team 也能改——名字是給人找東西用的。
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub budget: Option<BudgetPatch>,
    #[serde(default)]
    pub supervised: Option<bool>,
    #[serde(default)]
    pub deliver: Option<String>,
    #[serde(default)]
    pub workers: Option<RolePatch>,
    #[serde(default)]
    pub pm: Option<RolePatch>,
    #[serde(default)]
    pub reviewer: Option<RolePatch>,
}

/// Change one role mid-run (§10.5). All three roles take the same shape.
///
/// `apply = "next"` (default) only rewrites the spec in `roles_json`, which is what the next
/// batch of executors is created from; `apply = "now"` also restarts the live members, which
/// drops whatever they were in the middle of. `kind` is the exception: a member's CLI is
/// decided when its pane starts, so changing it always swaps the bot (see [`swap_member`])
/// no matter what `apply` says.
///
/// `Option<Option<String>>` on purpose: an absent key leaves the field alone, an explicit
/// `null` clears it back to the kind's default.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RolePatch {
    /// `workers` only: the parallelism (§7.1). Raising it mid-run creates and starts the
    /// extra executors at once; lowering it only takes effect for the next batch.
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub model: Option<Option<String>>,
    #[serde(default)]
    pub effort: Option<Option<String>>,
    #[serde(default)]
    pub fast: Option<bool>,
    #[serde(default)]
    pub identity: Option<Option<String>>,
    #[serde(default)]
    pub apply: Option<String>,
}

/// Where one role lives: the key in `roles_json`, and the `bots.team_role` its members carry.
struct RoleSlot {
    /// `roles_json` key **and** the label used in error messages / the `patch` note.
    key: &'static str,
    team_role: &'static str,
}

const ROLE_SLOTS: [RoleSlot; 3] = [
    RoleSlot { key: "pm", team_role: "pm" },
    RoleSlot { key: "workers", team_role: "worker" },
    RoleSlot { key: "reviewer", team_role: "reviewer" },
];

/// `roles_json` nests the executors one level deeper (`workers.count` sits beside the spec);
/// the other two roles *are* the spec.
fn read_role_spec(roles: &Value, key: &str) -> Option<RoleSpec> {
    let v = if key == "workers" { roles.get("workers")?.get("spec")? } else { roles.get(key)? };
    serde_json::from_value(v.clone()).ok()
}

fn write_role_spec(roles: &mut Value, key: &str, spec: Value) {
    if key == "workers" {
        if let Some(w) = roles.get_mut("workers").and_then(Value::as_object_mut) {
            w.insert("spec".into(), spec);
        }
    } else if let Some(o) = roles.as_object_mut() {
        o.insert(key.into(), spec);
    }
}

/// `Some(None)` (an explicit `null`) clears the field; whitespace counts as empty.
fn patched_opt(v: &Option<String>) -> Option<String> {
    v.clone().map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

/// `PATCH /api/teams/:id` — top up the budget, flip supervised, change the delivery mode.
pub async fn patch(app: &Arc<App>, team_id: &str, p: PatchTeam) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    // 改名先做，而且不受 phase 限制：已經結束的 team 一樣要能取個找得到的名字。
    if let Some(raw) = &p.label {
        let label = raw.trim();
        if label.chars().count() > 60 {
            return Err(LcError::Bad("team 名稱最長 60 個字".into()));
        }
        sqlx::query("UPDATE teams SET label = ? WHERE id = ?")
            .bind(if label.is_empty() { None } else { Some(label) })
            .bind(team_id)
            .execute(&app.db)
            .await
            .map_err(any_err)?;
        app.emit("team_changed", json!({"team_id": team_id})).await;
        // 只送 label 的請求到此為止：不必碰 budget / roles，也不必被 terminal 擋下來。
        if p.budget.is_none()
            && p.supervised.is_none()
            && p.deliver.is_none()
            && p.workers.is_none()
            && p.pm.is_none()
            && p.reviewer.is_none()
        {
            return Ok(json!({"applied": "label"}));
        }
    }
    if is_terminal(&t.phase) {
        return Err(LcError::conflict("team already finished", json!({"phase": t.phase})));
    }
    let mut budget = Budget::from_json(&t.budget_json);
    if let Some(bp) = &p.budget {
        budget.apply(bp).map_err(LcError::Bad)?;
    }
    let deliver = match &p.deliver {
        None => t.deliver.clone(),
        Some(d) if DELIVERS.contains(&d.as_str()) => d.clone(),
        Some(_) => return Err(LcError::Bad("deliver must be `branch` or `pr`".into())),
    };
    let supervised = p.supervised.unwrap_or(t.supervised == 1);
    let mut roles: Value = serde_json::from_str(&t.roles_json).unwrap_or_else(|_| json!({}));
    // One note entry per role that was actually touched, so the timeline says *what* changed.
    let mut role_notes = serde_json::Map::new();
    for slot in ROLE_SLOTS.iter() {
        let rp = match slot.key {
            "pm" => p.pm.as_ref(),
            "workers" => p.workers.as_ref(),
            _ => p.reviewer.as_ref(),
        };
        let Some(rp) = rp else { continue };
        let note = patch_role(app, &t, slot, rp, &mut roles).await?;
        role_notes.insert(slot.key.into(), note);
    }
    // §10.5 — `workers.count` is the parallelism, and it is the one field whose two
    // directions differ: more slots are worth having *now* (the queue is already full of
    // tasks nobody is running), fewer slots cannot retract an executor that is mid-task, so
    // they wait for the next batch. The extra members are made the same way `start_issue`
    // makes a fresh batch, then `fill_workers` hands them whatever was queued.
    let mut applied = "next_batch";
    // §4.5 (2026-09-09): `0` is unlimited and may be switched on or off while the team runs.
    // Switching **to** 0 starts every queued issue right away; switching **away** from it
    // starts no further issue — whatever is in flight finishes — and the executor count is
    // `n` from the next batch on.
    let mut unlimit_now = false;
    if let Some(c) = p.workers.as_ref().and_then(|w| w.count) {
        if c > MAX_WORKERS {
            return Err(LcError::Bad(format!(
                "workers.count must be 0 (unlimited) or between 1 and {MAX_WORKERS}"
            )));
        }
        if let Some(w) = roles.get_mut("workers").and_then(Value::as_object_mut) {
            w.insert("count".into(), json!(c));
        }
        if c == UNLIMITED_WORKERS {
            unlimit_now = true;
            applied = "now";
        }
        let live = db::team_members(&app.db, team_id)
            .await
            .map_err(any_err)?
            .into_iter()
            .filter(|b| b.deleted_at.is_none() && b.team_role.as_deref() == Some("worker"))
            .count() as u32;
        if c != UNLIMITED_WORKERS && c > live && !is_terminal(&t.phase) {
            let spec = read_role_spec(&roles, "workers")
                .ok_or_else(|| LcError::Upstream("team roles_json has no worker spec".into()))?;
            let project = db::project(&app.db, &t.project_id)
                .await
                .map_err(any_err)?
                .ok_or_else(|| LcError::NotFound("project".into()))?;
            let checked = check_role(app, &project.host, &spec, "workers").await?;
            let cur = db::current_team_issue(&app.db, team_id).await.map_err(any_err)?;
            let issue_seq = cur.as_ref().map(|i| i.seq).unwrap_or(1);
            grow_workers(app, &project, &t, issue_seq, &t.branch, t.issue_number, live, c, &checked).await?;
            applied = "now";
        }
    }
    let workers_note = role_notes.get("workers").cloned().unwrap_or(Value::Null);
    sqlx::query("UPDATE teams SET budget_json = ?, deliver = ?, supervised = ?, roles_json = ? WHERE id = ?")
        .bind(serde_json::to_string(&budget).unwrap_or_else(|_| "{}".into()))
        .bind(&deliver)
        .bind(supervised as i64)
        .bind(serde_json::to_string(&roles).unwrap_or_else(|_| "{}".into()))
        .bind(team_id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;
    record_event(
        app,
        team_id,
        "note",
        None,
        None,
        None,
        None,
        json!({
            "action": "patch", "budget": budget, "deliver": deliver, "supervised": supervised,
            // `workers` stays at the top level: it is what every reader of this note has
            // parsed since §10.5 existed. `roles` is the full picture (pm / workers / reviewer).
            "workers": workers_note, "roles": Value::Object(role_notes),
        }),
    )
    .await?;
    let t = load(app, team_id).await?;
    emit_team_changed(app, &t).await;
    // The new slots are useless until something is put in them, and the queue is where the
    // work already is (§4.5).
    if unlimit_now && !is_terminal(&t.phase) {
        if let Err(e) = crate::team_sched::start_issues_up_to_capacity(app, team_id).await {
            tracing::warn!(team = %team_id, error = ?e, "could not open the queue for unlimited parallelism");
        }
    }
    if applied == "now" {
        if let Err(e) = crate::team_sched::fill_now(app, team_id).await {
            tracing::warn!(team = %team_id, error = ?e, "could not fill the new executor slots right away");
        }
    }
    Ok(json!({"applied": applied}))
}

/// §10.5 — add executor slots to a running team: `dev-(old+1)` … `dev-(new)`, each with its
/// own worktree, the same path `start_issue` takes for a fresh batch.
#[allow(clippy::too_many_arguments)]
async fn grow_workers(
    app: &Arc<App>,
    project: &db::Project,
    t: &db::Team,
    issue_seq: i64,
    integration: &str,
    issue_number: i64,
    from: u32,
    to: u32,
    spec: &CheckedRole,
) -> LcResult<()> {
    let t6 = tid6(&t.id);
    let root = t.worktree_root.clone();
    for n in (from + 1)..=to {
        let dir = format!("{root}/{}", member_dir("worker", n, issue_seq));
        tg::worktree_add(
            app,
            &project.host,
            &repo_path(project, &t.repo),
            &dir,
            &t.base_sha,
            true,
        )
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
        let nick = member_nick(&t6, "worker", n, issue_seq);
        let bot_id = insert_member(
            app,
            project,
            &t.id,
            &nick,
            &t6,
            "worker",
            issue_number,
            spec,
            &dir,
        )
        .await?;
        // The short persona `insert_member` leaves behind has none of the report protocol.
        let roster: Vec<String> = db::team_members(&app.db, &t.id)
            .await
            .map_err(any_err)?
            .iter()
            .filter(|b| b.deleted_at.is_none())
            .map(|b| {
                format!(
                    "`{}`（{}）",
                    short_name(&b.name, &t.id),
                    b.team_role.clone().unwrap_or_default()
                )
            })
            .collect();
        let persona = full_persona(
            "worker",
            issue_number,
            &short_name(&nick, &t.id),
            &dir,
            integration,
            &roster.join("、"),
            spec.persona_extra.as_deref(),
            is_unlimited(t),
        );
        sqlx::query("UPDATE bots SET persona = ? WHERE id = ?")
            .bind(&persona)
            .bind(&bot_id)
            .execute(&app.db)
            .await
            .map_err(any_err)?;
        if let Ok(Some(bot)) = db::bot(&app.db, &bot_id).await {
            for e in crate::trust::pretrust_members(app, std::slice::from_ref(&bot)).await {
                tracing::warn!(team = %t.id, error = %e, "could not pre-trust a new executor's worktree");
            }
        }
        if let Err(e) = crate::lifecycle::start_bot(app, &bot_id).await {
            tracing::warn!(team = %t.id, error = ?e, "a new executor failed to start");
            record_event(
                app,
                &t.id,
                "note",
                None,
                None,
                None,
                None,
                json!({"action": "member_start_failed", "bot": nick, "error": format!("{e:?}")}),
            )
            .await?;
        }
        app.emit("bot_changed", json!({"bot_id": bot_id})).await;
    }
    Ok(())
}

/// One role's half of [`patch`] (§10.5). Rewrites `roles_json` — which is what the next batch
/// of executors is built from — and then brings the live members into line.
///
/// `model` / `effort` / `fast` / `identity` are plain column updates; `apply = "now"` restarts
/// the members so the change takes effect immediately. `kind` cannot be a column update at
/// all: which CLI a member runs is decided when its pane starts, so a new kind means a new
/// bot ([`swap_member`]).
async fn patch_role(
    app: &Arc<App>,
    t: &db::Team,
    slot: &RoleSlot,
    rp: &RolePatch,
    roles: &mut Value,
) -> LcResult<Value> {
    let apply_now = match rp.apply.as_deref().map(str::trim) {
        None | Some("") | Some("next") => false,
        Some("now") => true,
        Some(other) => {
            return Err(LcError::Bad(format!("{}.apply must be `next` or `now`, not `{other}`", slot.key)))
        }
    };
    let project = db::project(&app.db, &t.project_id)
        .await
        .map_err(any_err)?
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let mut spec = read_role_spec(roles, slot.key).ok_or_else(|| match slot.key {
        // A team built without a reviewer has no spec to edit — and adding one mid-run would
        // need a worktree and a whole startup, which is not what a PATCH is for.
        "reviewer" => LcError::Bad("this team has no reviewer".into()),
        k => LcError::Upstream(format!("team roles_json has no {k} spec")),
    })?;
    let from_kind = spec.kind.clone();
    if let Some(k) = &rp.kind {
        spec.kind = k.trim().to_string();
    }
    if let Some(m) = &rp.model {
        spec.model = patched_opt(m);
    }
    if let Some(e) = &rp.effort {
        spec.effort = patched_opt(e);
    }
    if let Some(f) = rp.fast {
        spec.fast = f;
    }
    if let Some(i) = &rp.identity {
        spec.identity = patched_opt(i);
    }
    // Unchanged from before: kind installed, effort valid for that kind, identity present and
    // belonging to that same kind — all resolved on the team's host.
    let checked = check_role(app, &project.host, &spec, slot.key).await?;
    write_role_spec(roles, slot.key, role_spec_json(&checked));

    let live: Vec<db::Bot> = db::team_members(&app.db, &t.id)
        .await
        .map_err(any_err)?
        .into_iter()
        .filter(|b| b.deleted_at.is_none() && b.team_role.as_deref() == Some(slot.team_role))
        .collect();
    let mut restarted: Vec<String> = Vec::new();
    let mut swapped: Vec<Value> = Vec::new();

    if checked.kind != from_kind {
        // Refuse before touching anything: swapping a member out from under a turn that is
        // still running loses the reply the team is waiting for. The user pauses first.
        for b in &live {
            if let Some(run) = db::active_run(&app.db, &b.id).await.map_err(any_err)? {
                if db::in_flight_turn(&app.db, &run.id).await.map_err(any_err)?.is_some() {
                    return Err(LcError::conflict(
                        "member busy",
                        json!({"role": slot.key, "bot_id": b.id, "name": short_name(&b.name, &t.id)}),
                    ));
                }
            }
        }
        for b in &live {
            let new_id = swap_member(app, &project, t, b, &checked).await?;
            swapped.push(json!({"name": short_name(&b.name, &t.id), "from_bot_id": b.id, "to_bot_id": new_id}));
        }
    } else {
        // The spec is what these members were started from, so the same fields move with it.
        // Without `apply = "now"` they keep running as they are until they are replaced.
        for b in &live {
            sqlx::query("UPDATE bots SET model = ?, effort = ?, fast = ?, identity = ? WHERE id = ?")
                .bind(&checked.model)
                .bind(&checked.effort)
                .bind(checked.fast as i64)
                .bind(&checked.identity)
                .bind(&b.id)
                .execute(&app.db)
                .await
                .map_err(any_err)?;
            app.emit("bot_changed", json!({"bot_id": b.id})).await;
            if apply_now && db::active_run(&app.db, &b.id).await.map_err(any_err)?.is_some() {
                match crate::lifecycle::restart_bot(app, &b.id).await {
                    Ok(_) => restarted.push(short_name(&b.name, &t.id)),
                    Err(e) => {
                        tracing::warn!(bot = %b.name, error = ?e, "team member restart after a patch failed")
                    }
                }
            }
        }
    }
    Ok(json!({
        "role": slot.key, "kind": checked.kind, "from": from_kind, "to": checked.kind,
        "model": checked.model, "effort": checked.effort, "fast": checked.fast,
        "identity": checked.identity,
        "apply": if apply_now { "now" } else { "next" },
        "restarted": restarted, "swapped": swapped,
    }))
}

/// §7.6 — changing a member's `kind` means replacing the bot.
///
/// The CLI a member runs is chosen when its pane starts, so flipping `bots.kind` under a
/// running agent only makes the row lie about the process. Instead: stop and soft-delete the
/// old bot (its messages stay — the conversation is the team's log), insert a new one with the
/// **same name, cwd and role**, hand it the unfinished tasks, and start it.
///
/// The phase is deliberately left alone. A `paused` team stays paused so the user presses
/// 「繼續」 when they are ready; a running one carries on with the new member in place.
async fn swap_member(
    app: &Arc<App>,
    project: &db::Project,
    t: &db::Team,
    old: &db::Bot,
    spec: &CheckedRole,
) -> LcResult<String> {
    let role = old.team_role.clone().unwrap_or_default();
    let cwd = old.cwd.clone().unwrap_or_default();
    let _ = crate::lifecycle::stop_bot(app, &old.id).await;
    // Soft-delete first: `insert_member` refuses a name that is still taken, and the point of
    // the swap is that the new member keeps the name the protocol already routes to.
    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
        .bind(db::now())
        .bind(&old.id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;
    // #61: the seat keeps its name, not its id — `bots/<id>/` of the retired bot is garbage.
    crate::lifecycle::purge_bot_dir(app, &old.id, &project.host).await;
    let new_id =
        insert_member(app, project, &t.id, &old.name, &tid6(&t.id), &role, t.issue_number, spec, &cwd).await?;

    // The full persona, the same one `create` writes once the layout exists — the short one
    // `insert_member` leaves behind has none of the dispatch / verdict protocol.
    let roster: Vec<String> = db::team_members(&app.db, &t.id)
        .await
        .map_err(any_err)?
        .iter()
        .filter(|b| b.deleted_at.is_none())
        .map(|b| format!("`{}`（{}）", short_name(&b.name, &t.id), b.team_role.clone().unwrap_or_default()))
        .collect();
    let persona = full_persona(
        &role,
        t.issue_number,
        &short_name(&old.name, &t.id),
        &cwd,
        &t.branch,
        &roster.join("、"),
        spec.persona_extra.as_deref(),
        is_unlimited(t),
    );
    sqlx::query("UPDATE bots SET persona = ? WHERE id = ?")
        .bind(&persona)
        .bind(&new_id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;

    // `team_tasks_one_open_per_worker` is unique on the bot, so an unfinished task left on the
    // retired bot would block every future dispatch to this seat.
    sqlx::query(
        "UPDATE team_tasks SET worker_bot_id = ?, updated_at = ?
          WHERE team_id = ? AND worker_bot_id = ? AND state NOT IN ('merged','skipped','failed')",
    )
    .bind(&new_id)
    .bind(db::now())
    .bind(&t.id)
    .bind(&old.id)
    .execute(&app.db)
    .await
    .map_err(any_err)?;
    // A queued task the PM addressed to this seat by name (`dispatch{to}`) waits on
    // `want_worker_bot_id`; left on the retired bot it would never match the replacement and
    // sit in `queued` until `pm_stalled`.
    sqlx::query(
        "UPDATE team_tasks SET want_worker_bot_id = ?, updated_at = ?
          WHERE team_id = ? AND want_worker_bot_id = ? AND state = 'queued'",
    )
    .bind(&new_id)
    .bind(db::now())
    .bind(&t.id)
    .bind(&old.id)
    .execute(&app.db)
    .await
    .map_err(any_err)?;
    // Same for a rescue in progress: `workers_for` answers only the rescue bot while
    // `rescue_bot_id` is set, and a swapped rescuer would leave that set empty.
    sqlx::query("UPDATE teams SET rescue_bot_id = ? WHERE id = ? AND rescue_bot_id = ?")
        .bind(&new_id)
        .bind(&t.id)
        .bind(&old.id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;

    // A member with no run is `member_lost` the moment the PM dispatches to it, and `resume`
    // refuses to move until every member is running — so start it here, the same way
    // `start_issue_workers` does for a fresh batch. A failure is noted, not fatal: the user
    // can fix the cause and press 啟動 on the bot.
    if let Ok(Some(bot)) = db::bot(&app.db, &new_id).await {
        for e in crate::trust::pretrust_members(app, std::slice::from_ref(&bot)).await {
            tracing::warn!(team = %t.id, error = %e, "could not pre-trust a swapped member's worktree");
        }
    }
    if let Err(e) = crate::lifecycle::start_bot(app, &new_id).await {
        let msg = format!("{e:?}");
        tracing::warn!(bot = %old.name, error = %msg, "swapped team member failed to start");
        record_event(
            app,
            &t.id,
            "note",
            None,
            None,
            None,
            None,
            json!({"action": "member_start_failed", "bot": old.name, "error": msg}),
        )
        .await?;
    }
    app.emit("bot_changed", json!({"bot_id": old.id})).await;
    app.emit("bot_changed", json!({"bot_id": new_id})).await;
    Ok(new_id)
}

/// Resolve the `to` field of `say`: a role (`pm` / `reviewer` / `rev` / `dev-1`), a bot id,
/// or a member's nickname.
pub async fn resolve_member(app: &Arc<App>, t: &db::Team, to: &str) -> LcResult<db::Bot> {
    let want = to.trim().trim_start_matches('@');
    if want.is_empty() {
        return Err(LcError::Bad("to must not be empty".into()));
    }
    let members: Vec<db::Bot> =
        db::team_members(&app.db, &t.id).await.map_err(any_err)?.into_iter().filter(|b| b.deleted_at.is_none()).collect();
    let lower = want.to_ascii_lowercase();
    let by_role = match lower.as_str() {
        "pm" => Some("pm"),
        "reviewer" | "rev" => Some("reviewer"),
        _ => None,
    };
    if let Some(role) = by_role {
        if let Some(b) = members.iter().find(|b| b.team_role.as_deref() == Some(role)) {
            return Ok(b.clone());
        }
    }
    if let Some(b) = members.iter().find(|b| b.id == want) {
        return Ok(b.clone());
    }
    if let Some(b) = members.iter().find(|b| {
        b.name.to_ascii_lowercase() == lower || short_name(&b.name, &t.id).to_ascii_lowercase() == lower
    }) {
        return Ok(b.clone());
    }
    Err(LcError::NotFound("team member".into()))
}

/// `POST /api/teams/:id/say` — the user talking into the team.
///
/// Recorded as a `kind:"user"` event and **not** counted against `max_relays` (SPEC-team
/// §4.5): only the daemon's own relays burn budget.
pub async fn say(
    app: &Arc<App>,
    team_id: &str,
    text: &str,
    to: &str,
    client_request_id: &str,
) -> LcResult<Value> {
    say_with_delivery(app, team_id, text, to, client_request_id, text).await
}

async fn say_with_delivery(
    app: &Arc<App>,
    team_id: &str,
    text: &str,
    to: &str,
    client_request_id: &str,
    delivery_text: &str,
) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    if text.trim().is_empty() {
        return Err(LcError::Bad("text must not be empty".into()));
    }
    if is_terminal(&t.phase) {
        return Err(LcError::conflict("team already finished", json!({"phase": t.phase})));
    }
    let bot = resolve_member(app, &t, to).await?;
    let crid = if client_request_id.trim().is_empty() { db::ulid() } else { client_request_id.to_string() };
    let ev = record_event(
        app,
        team_id,
        "user",
        None,
        Some(&bot.id),
        None,
        None,
        json!({"text": text, "to": bot.name}),
    )
    .await?;
    let out = lifecycle::prompt_grouped(app, &bot.id, text, &crid, None, Some(delivery_text), &[]).await?;
    // Stamp the team on the rows the ordinary prompt path just created so the team timeline
    // and `turns.team_id` line up without touching `lifecycle`.
    let _ = sqlx::query("UPDATE messages SET team_id = ? WHERE id = ?")
        .bind(team_id)
        .bind(&out.message_id)
        .execute(&app.db)
        .await;
    let _ = sqlx::query("UPDATE turns SET team_id = ?, team_event_id = ? WHERE id = ?")
        .bind(team_id)
        .bind(&ev.id)
        .bind(&out.turn_id)
        .execute(&app.db)
        .await;
    let _ = sqlx::query("UPDATE team_events SET turn_id = ? WHERE id = ?")
        .bind(&out.turn_id)
        .bind(&ev.id)
        .execute(&app.db)
        .await;
    Ok(json!({
        "team_id": t.id, "event_id": ev.id,
        "bot_id": bot.id, "bot_name": bot.name,
        "turn_id": out.turn_id, "message_id": out.message_id, "delivery": out.delivery,
    }))
}

/// `POST /api/teams/:id/answer` — reply to the PM's `ask_user`: a `say` to the PM plus a
/// resume when that is what the team is waiting on.
pub async fn answer(app: &Arc<App>, team_id: &str, text: &str, client_request_id: &str) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    // Keep the user's answer as-is in the timeline, but remind the PM in the delivered
    // prompt that this turn still needs the protocol block. Without this, the PM's
    // post-`ask_user` reply is an ordinary `kind:"user"` turn and commonly has no block for
    // `on_turn_done` to apply, leaving the team in `planning`.
    let delivery = format!("{}{}", text, crate::team_sched::footer("pm"));
    let mut out = say_with_delivery(app, team_id, text, "pm", client_request_id, &delivery).await?;
    let waiting = t.phase == "paused" && t.pause_reason.as_deref().map(|r| r.starts_with("ask_user")).unwrap_or(false);
    if waiting {
        resume(app, team_id).await?;
    }
    if let Some(o) = out.as_object_mut() {
        o.insert("resumed".into(), json!(waiting));
    }
    Ok(out)
}

/// `POST /api/teams/:id/issues/retry-failed` — SPEC-team §2.6b (2026-09-11 使用者：
/// 「例如這類都要能接力完成」).
///
/// The issue queue keeps `failed` rows with the reason on them — a merge conflict nobody could
/// resolve, a wall-clock budget that ran out. Those are not decisions, they are unfinished work:
/// the user's own answer is to run them again rather than to retype four issue numbers into the
/// reopen box. §2.3 already allows the same number back on the queue (a finished row stops
/// reserving it), so this is exactly [`add_issues`] with the numbers filled in for you, plus a
/// note that says which failure each retry answers.
/// The queue entries §2.6b puts back: the **latest attempt** of each issue number, kept only
/// when that attempt failed or was skipped. An issue that failed once and was delivered on the
/// retry is finished; one queued three times and failed twice comes back once, not twice.
pub(crate) fn stuck_issues(issues: &[db::TeamIssue]) -> Vec<&db::TeamIssue> {
    let mut latest: std::collections::BTreeMap<i64, &db::TeamIssue> = std::collections::BTreeMap::new();
    for i in issues {
        if latest.get(&i.issue_number).map(|p| i.seq > p.seq).unwrap_or(true) {
            latest.insert(i.issue_number, i);
        }
    }
    latest.values().copied().filter(|i| i.state == "failed" || i.state == "skipped").collect()
}

pub async fn retry_failed_issues(app: &Arc<App>, team_id: &str) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    let issues = db::team_issues(&app.db, team_id).await.map_err(any_err)?;
    let stuck = stuck_issues(&issues);
    if stuck.is_empty() {
        return Err(LcError::conflict("no failed issue to retry", json!({"phase": t.phase})));
    }
    let numbers: Vec<i64> = stuck.iter().map(|i| i.issue_number).collect();
    let out = add_issues(app, team_id, &numbers).await?;
    record_event(
        app,
        team_id,
        "note",
        None,
        None,
        None,
        None,
        json!({
            "action": "issues_retried",
            "by": "user",
            "issues": stuck.iter().map(|i| json!({
                "issue_number": i.issue_number,
                "seq": i.seq,
                "reason": i.fail_reason,
                "branch": i.branch,
            })).collect::<Vec<_>>(),
        }),
    )
    .await?;
    let mut out = out;
    if let Some(o) = out.as_object_mut() {
        o.insert("retried".into(), json!(numbers));
    }
    Ok(out)
}

/// `POST /api/teams/:id/rescue` — SPEC-team §2.6 (2026-09-11 使用者).
///
/// A finished team can still be carrying tasks nobody solved (`failed` / `skipped`). Re-queuing
/// the whole issue (§2.5) restarts planning and throws away what *did* land; what the user
/// actually wants is 「擇一個 model 來處理所有失敗的」——usually the reviewer, which by then has
/// read every diff in this issue and has an idle worktree.
///
/// So: one new task carrying **all** of them, handed to one member. One task and not one per
/// failure because `team_tasks_one_open_per_worker` allows a member exactly one open task at a
/// time — and because "fix these five things" is the single job the user is asking for.
///
/// The PM may not be the rescuer: its cwd is the integration worktree (`main/`), and cutting a
/// task branch there would dirty the tree the daemon merges into.
pub async fn rescue(app: &Arc<App>, team_id: &str, bot_id: Option<&str>) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    if t.phase != "done" {
        return Err(LcError::conflict("team is not finished", json!({"phase": t.phase})));
    }
    let members = db::team_members(&app.db, team_id).await.map_err(any_err)?;
    let live: Vec<&db::Bot> = members.iter().filter(|b| b.deleted_at.is_none()).collect();
    if !live.iter().any(|b| b.team_role.as_deref() == Some("pm")) {
        // §2.5.1's judge, for the same reason: a cleaned-up team has no worktrees left.
        return Err(LcError::conflict("team is cleaned up", json!({"phase": t.phase})));
    }
    let tasks = db::team_tasks(&app.db, team_id).await.map_err(any_err)?;
    let unresolved: Vec<&db::TeamTask> =
        tasks.iter().filter(|x| x.state == "failed" || x.state == "skipped").collect();
    if unresolved.is_empty() {
        return Err(LcError::conflict("nothing to rescue", json!({"phase": t.phase})));
    }
    let bot = match bot_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => *live
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| LcError::conflict("not a live member of this team", json!({"bot_id": id})))?,
        None => *live
            .iter()
            .find(|b| b.team_role.as_deref() == Some("reviewer"))
            .ok_or_else(|| LcError::conflict("this team has no reviewer; pick a member", json!({"team_id": team_id})))?,
    };
    if bot.team_role.as_deref() == Some("pm") {
        return Err(LcError::conflict("the PM cannot be the rescuer", json!({"bot_id": bot.id})));
    }

    // Which issue does the rescue belong to? The one the newest unresolved task was cut from,
    // so its branch is the integration branch that already carries everything that merged.
    let issues = db::team_issues(&app.db, team_id).await.map_err(any_err)?;
    let newest = unresolved.iter().max_by_key(|x| x.seq).copied().expect("non-empty");
    let issue = newest
        .issue_id
        .as_deref()
        .and_then(|id| issues.iter().find(|i| i.id == id))
        .or_else(|| issues.iter().max_by_key(|i| i.seq))
        .ok_or_else(|| LcError::conflict("this team has no issue to rescue into", json!({"team_id": team_id})))?;
    let integration = issue.branch.clone().unwrap_or_else(|| t.branch.clone());

    let next_seq = tasks.iter().map(|x| x.seq).max().unwrap_or(0) + 1;
    let branch = task_branch(&integration, next_seq);
    let mut brief = String::from(
        "這些 task 上一輪沒有解決（failed / skipped）。請在你的 cwd 逐一處理掉，\
         做不到的在回報裡說明原因，不要留給下一個人猜。\n\n",
    );
    let mut files: Vec<String> = Vec::new();
    for x in &unresolved {
        let fs: Vec<String> = serde_json::from_str(&x.files_json).unwrap_or_default();
        for f in &fs {
            if !files.iter().any(|y| y == f) {
                files.push(f.clone());
            }
        }
        brief.push_str(&format!(
            "## t{seq}「{title}」（{state}）\n{b}\n{last}相關檔案：{f}\n\n",
            seq = x.seq,
            title = x.title,
            state = x.state,
            b = x.brief.trim(),
            last = match x.last_report.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
                Some(r) => format!("最後回報：{r}\n"),
                None => String::new(),
            },
            f = if fs.is_empty() { "（未指定）".to_string() } else { fs.join("、") },
        ));
    }

    let task_id = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO team_tasks (id, team_id, issue_id, seq, title, brief, files_json, want_worker_bot_id,
           branch, state, round, rebase_attempts, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,'queued',0,0,?,?)",
    )
    .bind(&task_id)
    .bind(team_id)
    .bind(&issue.id)
    .bind(next_seq)
    .bind(format!("收尾未解決的 {} 個 task", unresolved.len()))
    .bind(&brief)
    .bind(serde_json::to_string(&files).unwrap_or_else(|_| "[]".into()))
    .bind(&bot.id)
    .bind(&branch)
    .bind(&now)
    .bind(&now)
    .execute(&app.db)
    .await
    .map_err(any_err)?;

    // Back onto the queue: the issue is being worked again, the team is no longer ended, and
    // `rescue_bot_id` is what makes the chosen member the issue's only executor (§2.6).
    // `ended_at` goes back to NULL with it: the issue is being worked again, and §2.3 reads
    // that column to tell a finished entry from a live one.
    sqlx::query("UPDATE team_issues SET state = 'working', ended_at = NULL WHERE id = ?")
        .bind(&issue.id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;
    sqlx::query("UPDATE teams SET ended_at = NULL, rescue_bot_id = ? WHERE id = ?")
        .bind(&bot.id)
        .bind(team_id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;
    record_event(
        app,
        team_id,
        "note",
        None,
        Some(&bot.id),
        Some(&task_id),
        None,
        json!({
            "action": "team_rescue",
            "by": "user",
            "bot": short_name(&bot.name, team_id),
            "issue_number": issue.issue_number,
            "task_seqs": unresolved.iter().map(|x| x.seq).collect::<Vec<_>>(),
        }),
    )
    .await?;
    // Members were stopped when the team finished, so this goes through `starting` like a
    // reopen does — `rescue_startup` brings PM / reviewer / rescuer back and hands over.
    set_phase(app, team_id, "starting", None, None).await?;
    spawn_scheduler(app, team_id);
    let created = db::team_task(&app.db, &task_id).await.map_err(any_err)?;
    Ok(json!({
        "task": created.as_ref().map(task_json),
        "bot_id": bot.id,
        "bot": short_name(&bot.name, team_id),
        "issue_number": issue.issue_number,
        "rescued": unresolved.len(),
    }))
}

/// `POST /api/teams/:id/tasks/:tid/decide` — the human unblocking one task (SPEC-team §8.2).
pub async fn decide(
    app: &Arc<App>,
    team_id: &str,
    task_id: &str,
    action: &str,
    note: Option<&str>,
) -> LcResult<Value> {
    let t = load(app, team_id).await?;
    if is_terminal(&t.phase) {
        return Err(LcError::conflict("team already finished", json!({"phase": t.phase})));
    }
    let task = db::team_task(&app.db, task_id)
        .await
        .map_err(any_err)?
        .filter(|x| x.team_id == t.id)
        .ok_or_else(|| LcError::NotFound("task".into()))?;
    if !DECIDABLE_STATES.contains(&task.state.as_str()) {
        return Err(LcError::conflict(
            "task is not waiting for a decision",
            json!({"task_id": task.id, "state": task.state}),
        ));
    }
    let next = match action {
        "rework" => "working",
        "force_merge" => "merging",
        "skip" => "skipped",
        _ => return Err(LcError::Bad("action must be rework, force_merge or skip".into())),
    };
    sqlx::query("UPDATE team_tasks SET state = ?, updated_at = ? WHERE id = ?")
        .bind(next)
        .bind(db::now())
        .bind(&task.id)
        .execute(&app.db)
        .await
        .map_err(any_err)?;
    record_event(
        app,
        team_id,
        "note",
        None,
        task.worker_bot_id.as_deref(),
        Some(&task.id),
        None,
        json!({"action": action, "note": note, "from": task.state, "to": next}),
    )
    .await?;
    let updated = db::team_task(&app.db, &task.id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("task".into()))?;
    emit_task_updated(app, &updated).await;
    // `rework` needs the worker told what to do; `force_merge` is picked up by the merge
    // queue on the scheduler's next pass; `skip` needs nothing further.
    if action == "rework" {
        crate::team_sched::relay_rework_decision(app, &t, &updated, note).await?;
    }
    // The phases that produce a decidable task are exactly the paused ones
    // (`merge_conflict`, `review_exhausted`, `pm_abort`), and `step` returns at once while
    // the phase is `paused` — so without this the decision was recorded, answered 200 OK and
    // then sat there until the user separately pressed resume. Same treatment as `answer`.
    let now = load(app, team_id).await?;
    let waiting = now.phase == "paused"
        && now
            .pause_reason
            .as_deref()
            .map(|r| DECISION_PAUSES.iter().any(|p| r.starts_with(p)))
            .unwrap_or(false);
    if waiting {
        resume(app, team_id).await?;
    }
    spawn_scheduler(app, team_id);
    Ok(json!({"task": task_json(&updated), "resumed": waiting}))
}

// ---------------------------------------------------------------- scheduler seam (§3)

/// SPEC-team §3 — start (or re-use) this team's event loop. The loop itself is
/// `team_sched::spawn`; calling this twice for one team is a no-op, which is why `create`,
/// `resume`, `decide` and `respawn_schedulers` can all call it without coordinating.
pub fn spawn_scheduler(app: &Arc<App>, team_id: &str) {
    crate::team_sched::spawn(app, team_id);
}

/// SPEC-team §7.5 / §6.5: after a daemon restart every non-terminal team gets its scheduler
/// back, and every project that has one gets a `git worktree prune` — which collects the
/// registrations of worktrees a user deleted by hand while the daemon was down.
///
/// Nothing is restored from memory: the scheduler re-reads `teams`, `team_tasks` and the
/// pending rows of `team_events`, and carries on from there. A pending relay is re-sent with
/// the same `client_request_id`, so a turn that actually made it out is not duplicated.
pub async fn respawn_schedulers(app: &Arc<App>) {
    let teams = db::live_teams(&app.db).await.unwrap_or_default();
    if teams.is_empty() {
        return;
    }
    tracing::info!(count = teams.len(), "resuming teams after restart");
    let mut pruned: std::collections::BTreeSet<String> = Default::default();
    for t in teams {
        if let Ok(Some(p)) = db::project(&app.db, &t.project_id).await {
            if pruned.insert(format!("{}/{}", p.id, t.repo)) {
                tg::worktree_prune(app, &p.host, &repo_path(&p, &t.repo)).await;
            }
            // §6.4a: the workspace its panes live in must still exist, or the members have
            // nowhere to start. Same treatment as a missing worktree: pause, do not guess.
            if let Err(reason) = check_workspace(app, &p, &t).await {
                let _ = record_event(
                    app,
                    &t.id,
                    "note",
                    None,
                    None,
                    None,
                    None,
                    json!({"action": "workspace_missing", "detail": reason}),
                )
                .await;
                if t.phase != "paused" {
                    let _ = set_phase(app, &t.id, "paused", Some("workspace_missing"), Some(&t.phase)).await;
                }
                continue;
            }
            // §6.5: a team whose worktrees vanished cannot be continued blindly.
            if let Err(reason) = check_worktrees(app, &p, &t).await {
                let _ = record_event(
                    app,
                    &t.id,
                    "note",
                    None,
                    None,
                    None,
                    None,
                    json!({"action": "worktree_missing", "detail": reason}),
                )
                .await;
                if t.phase != "paused" {
                    let _ = set_phase(app, &t.id, "paused", Some("worktree_missing"), Some(&t.phase)).await;
                }
                continue;
            }
        }
        // §11：停在 `member_blocked:<name>` 而那個成員已經不是 blocked 了（人在別的畫面回完
        // 提示、或 daemon 沒開機的時候被處理掉），開機時就把它接回去；那個 blocked→idle 的
        // 邊緣已經過了，等不到第二次。
        if t.phase == "paused" {
            if let Some(name) = t.pause_reason.as_deref().and_then(|r| r.strip_prefix("member_blocked:")) {
                let members = db::team_members(&app.db, &t.id).await.unwrap_or_default();
                let still = match members.iter().find(|b| b.name == name) {
                    Some(b) => matches!(db::active_run(&app.db, &b.id).await, Ok(Some(r)) if r.agent_status == "blocked"),
                    // 成員不見了：那是 `member_lost` 的事，這裡不代為決定。
                    None => true,
                };
                if !still {
                    match resume(app, &t.id).await {
                        Ok(_) => tracing::info!(team = %t.id, member = %name, "member no longer blocked: team resumed on boot"),
                        Err(e) => tracing::warn!(team = %t.id, member = %name, error = ?e, "boot auto-resume failed"),
                    }
                    continue;
                }
            }
        }
        spawn_scheduler(app, &t.id);
    }
}

/// SPEC-team §6.4a: the team's workspace must still be there. A host that is not connected
/// is *not* a missing workspace — that is a transient condition the ordinary lamp already
/// shows, and pausing on it would fire every time herdr restarts a second later than us.
async fn check_workspace(app: &Arc<App>, project: &db::Project, t: &db::Team) -> Result<(), String> {
    let Some(ws) = t.workspace_id.as_deref().filter(|w| !w.trim().is_empty()) else {
        return Err("this team has no workspace".into());
    };
    let Ok((client, _)) = team_herdr(app, project).await else { return Ok(()) };
    match client.workspace_get(ws).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!("workspace {ws} is gone")),
        Err(_) => Ok(()),
    }
}

// ---------------------------------------------------------------- the cwd invariant (§6.1 #1)

/// Is `child` `dir` itself, or inside it? Compared on path components, so `/a/b` does not
/// contain `/a/bc`, and resolved through symlinks when both ends exist.
///
/// `""` contains nothing — that is what makes the `worktree_root = ''` shape fail every
/// containment test rather than pass them all. `/` is the opposite and has to be spelled out,
/// because trimming its trailing slash would otherwise turn it into `""`: filesystem root
/// contains every absolute path there is.
pub fn is_within(dir: &str, child: &str) -> bool {
    let norm = |s: &str| {
        let t = s.trim();
        let trimmed = t.trim_end_matches('/');
        if trimmed.is_empty() {
            return if t.starts_with('/') { "/".to_string() } else { String::new() };
        }
        std::fs::canonicalize(trimmed).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| trimmed.to_string())
    };
    let (d, c) = (norm(dir), norm(child));
    if d.is_empty() || c.is_empty() {
        return false;
    }
    if d == "/" {
        return c.starts_with('/');
    }
    c == d || c.starts_with(&format!("{d}/"))
}

/// **The invariant that keeps a team away from the user's checkout.**
///
/// Every worktree-level git command a team runs — `checkout -b` at dispatch, `add -A &&
/// commit` at report, `checkout --detach` at review — takes its `-C` from `bots.cwd`. If that
/// value is ever the project's own path, one `add -A` sweeps whatever the user had in
/// progress into a task branch. That is not hypothetical: a team created by a build that
/// predated the worktree layout had `worktree_root = ''` and all four members pointing at
/// the main checkout, which had 55 uncommitted files in it at the time.
///
/// So the check is a **path containment test, not a null check** — the broken shape had an
/// explicit cwd, not a missing one — and it is the single function every caller uses:
/// `Ctx::wt` (the scheduler's git target), `start_inner` (before a pane is ever opened) and
/// `check_worktrees` / `resume` (before a paused team is allowed to move again).
pub fn checked_member_cwd(team: &db::Team, project: &db::Project, bot: &db::Bot) -> Result<String, String> {
    let root = team.worktree_root.trim();
    if root.is_empty() {
        return Err(format!("team {} has no worktree_root", team.id));
    }
    let cwd = bot.cwd.as_deref().map(str::trim).unwrap_or("");
    if cwd.is_empty() {
        return Err(format!("member `{}` has no cwd", bot.name));
    }
    // The user's checkout, and anything inside it, is off limits — §6.1 #1 is the whole
    // reason the worktrees live in the data directory in the first place.
    if is_within(&project.path, cwd) {
        return Err(format!("member `{}` cwd `{cwd}` is inside the project checkout `{}`", bot.name, project.path));
    }
    if !is_within(root, cwd) {
        return Err(format!("member `{}` cwd `{cwd}` is outside the team root `{root}`", bot.name));
    }
    Ok(cwd.to_string())
}

/// The integration worktree, `<root>/main`, checked the same way (§6.3: the daemon merges
/// there and nowhere else).
pub fn checked_main_wt(team: &db::Team, project: &db::Project) -> Result<String, String> {
    let root = team.worktree_root.trim();
    if root.is_empty() {
        return Err(format!("team {} has no worktree_root", team.id));
    }
    if is_within(&project.path, root) {
        return Err(format!("team root `{root}` is inside the project checkout `{}`", project.path));
    }
    Ok(format!("{}/main", root.trim_end_matches('/')))
}

/// Every member of a team, checked. `Ok(())` means no git command this team runs can reach
/// the user's checkout.
pub fn check_layout(team: &db::Team, project: &db::Project, members: &[db::Bot]) -> Result<(), String> {
    checked_main_wt(team, project)?;
    for b in members.iter().filter(|b| b.deleted_at.is_none()) {
        checked_member_cwd(team, project, b)?;
    }
    Ok(())
}

/// Two paths naming the same directory. String equality is not enough on macOS, where
/// `/var` is a symlink to `/private/var`: the daemon stores the path it was given and git
/// reports the resolved one, so the two never matched and every team looked as if its
/// worktrees had vanished.
fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim_end_matches('/').to_string();
    if norm(a) == norm(b) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Every member's cwd must be inside the team root **and** still be a registered worktree
/// (SPEC-team §6.5).
///
/// The containment test comes first and is the important half. Comparing against
/// `git worktree list` alone was blind to the shape that actually happened: the user's main
/// checkout is the *first line* of that list, so a member whose cwd had become
/// `<project.path>` passed the "your worktree still exists" check with flying colours. The
/// main checkout is therefore excluded from `have` as well.
async fn check_worktrees(app: &Arc<App>, project: &db::Project, t: &db::Team) -> Result<(), String> {
    let members = db::team_members(&app.db, &t.id).await.unwrap_or_default();
    check_layout(t, project, &members)?;
    let want: Vec<String> = members
        .iter()
        .filter(|b| b.deleted_at.is_none())
        .filter_map(|b| b.cwd.clone())
        .filter(|c| !c.trim().is_empty())
        .collect();
    if want.is_empty() {
        return Ok(());
    }
    let git_dir = repo_path(project, &t.repo);
    let have: Vec<String> = tg::worktree_paths(app, &project.host, &git_dir)
        .await
        .into_iter()
        .filter(|h| !same_path(h, &git_dir))
        .collect();
    let missing: Vec<&String> = want.iter().filter(|w| !have.iter().any(|h| same_path(h, w))).collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
    }
}

/// SPEC-team §6.5 / §6.4a on every reconcile pass, not only at boot: a team whose worktrees
/// or workspace went away — or whose layout no longer satisfies the §6.1 invariant — is
/// paused where a human can see it.
/// The per-host entry point reconcile actually uses. Reconciliation is always scoped to one
/// host — pane and workspace ids are only unique within that host's herdr session — so a pass
/// over `local` must not go looking for a remote team's worktrees (and vice versa).
pub async fn reconcile_teams_on_host(app: &Arc<App>, host: &str) {
    reconcile_teams_inner(app, Some(host)).await;
}

async fn reconcile_teams_inner(app: &Arc<App>, host: Option<&str>) {
    for t in db::live_teams(&app.db).await.unwrap_or_default() {
        if t.phase == "paused" || t.phase == "aborting" {
            continue;
        }
        // 正在建立中的 team：`create` 先 INSERT（workspace_id 還是 NULL）、幾百毫秒後才
        // `workspace.create`。這段空窗被別的 herdr 事件觸發的 reconcile 掃到，就會被判成
        // workspace_missing 而暫停（2026-09-08 issue #56 實測）。建不成 create 自己會 rollback，
        // 這裡不用替它判。
        if t.phase == "starting" && t.workspace_id.as_deref().map_or(true, |w| w.trim().is_empty()) {
            continue;
        }
        let Ok(Some(p)) = db::project(&app.db, &t.project_id).await else { continue };
        if host.map(|h| p.host != h).unwrap_or(false) {
            continue;
        }
        let bad = match check_workspace(app, &p, &t).await {
            Err(e) => Some(("workspace_missing", e)),
            Ok(()) => check_worktrees(app, &p, &t).await.err().map(|e| ("worktree_missing", e)),
        };
        if let Some((reason, detail)) = bad {
            let _ = record_event(app, &t.id, "note", None, None, None, None, json!({"action": reason, "detail": detail}))
                .await;
            let _ = set_phase(app, &t.id, "paused", Some(reason), Some(&t.phase)).await;
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tid6_takes_the_last_six_alphanumerics() {
        assert_eq!(tid6("01JQZK3F9X2ABCDEF"), "abcdef");
        assert_eq!(tid6("01JQZK3F9X2"), "k3f9x2");
        assert_eq!(tid6(""), "team");
    }

    /// §6.1 #1. Reported 2026-09-06: a team created by a build that predated `team_git.rs`
    /// got `worktree_root = ''` and all four members' `cwd` written as the user's checkout,
    /// with 55 uncommitted files in it. Nothing was damaged — the run paused on a PM question
    /// before any worker reported — but a single `git add -A && commit` would have swallowed
    /// the lot. `is_within` is the whole of the containment decision, so pin its edges.
    #[test]
    fn containment_rejects_the_users_own_checkout() {
        let proj = "/Users/u/project/agents-manager";
        // The incident itself: cwd *is* the checkout. Equality has to count as "inside".
        assert!(is_within(proj, proj));
        assert!(is_within(proj, "/Users/u/project/agents-manager/daemon/src"));
        // A trailing slash on either side is the same directory, not a different one.
        assert!(is_within(&format!("{proj}/"), proj));

        // A shared textual prefix is not containment — the classic way this check goes wrong.
        assert!(!is_within(proj, "/Users/u/project/agents-manager-2"));
        assert!(!is_within("/a/b", "/a/bc"));
        // The team root is elsewhere entirely, which is the point of putting it in the data dir.
        assert!(!is_within(proj, "/Users/u/.config/agents-manager/teams/01ABC/dev-1"));

        let root = "/Users/u/.config/agents-manager/teams/01ABC";
        assert!(is_within(root, &format!("{root}/dev-1")));
        assert!(!is_within(root, "/Users/u/.config/agents-manager/teams/01ABCD/dev-1"));
        // An empty side can never contain anything: `worktree_root = ''` must not pass.
        assert!(!is_within("", proj));
        assert!(!is_within(root, ""));
        // …but `/` is not the empty string, however much trimming its slash looks like it.
        // It contains every absolute path, which is what makes `rm -rf /` fail the guard.
        assert!(is_within("/", proj));
        assert!(is_within("/", "/"));
        assert!(!is_within(proj, "/"));
        assert!(!is_within("/", "relative/path"));
    }

    #[test]
    fn branch_names_follow_6_2() {
        let b = integration_branch(42, "k3f9x2");
        assert_eq!(b, "team/i42-k3f9x2");
        // A `/` here would be impossible: `refs/heads/team/i42-k3f9x2` is a file, so git
        // cannot also create `refs/heads/team/i42-k3f9x2/t1-dev-1` under it.
        assert_eq!(task_branch(&b, 1), "team/i42-k3f9x2-t1");
        assert!(task_branch(&b, 1).starts_with(&b), "task branches still sort with theirs");
    }

    #[test]
    fn member_nicknames_and_short_names() {
        assert_eq!(member_nick("k3f9x2", "pm", 0, 1), "tk3f9x2-pm");
        assert_eq!(member_nick("k3f9x2", "worker", 2, 3), "tk3f9x2-i3-dev-2");
        assert_eq!(member_nick("k3f9x2", "reviewer", 0, 1), "tk3f9x2-rev");
        // `tid6` of this id is `k3f9x2`, which is what `short_name` strips.
        let tid = "01ARZ3NDEKTSV4RRFFQ69K3F9X2";
        assert_eq!(short_name("tk3f9x2-i3-dev-2", tid), "dev-2");
        assert_eq!(short_name("tk3f9x2-pm", tid), "pm");
        assert_eq!(short_name("tk3f9x2-rev", tid), "rev");
        // Members of a team created before the issue queue are still named after the issue.
        assert_eq!(short_name("i42-dev-2", tid), "dev-2");
        assert_eq!(short_name("i42-pm", tid), "pm");
        // The `i<n>-` strip is deliberately issue-agnostic: within one team there is only ever
        // one member per short name, so it does not need to know which issue it is looking at.
        assert_eq!(short_name("i7-pm", tid), "pm");
        // A prefix that is not `i<digits>-` is left alone.
        assert_eq!(short_name("ix-pm", tid), "ix-pm");
        // Every generated nickname must survive the bot-name rule.
        for n in [
            member_nick("k3f9x2", "pm", 0, 1),
            member_nick("k3f9x2", "worker", 4, 12),
            member_nick("k3f9x2", "reviewer", 0, 1),
        ] {
            assert!(crate::config::valid_bot_name(&n), "{n}");
        }
    }

    #[test]
    fn budget_defaults_are_the_user_ruling() {
        let b = Budget::default();
        assert_eq!(b.max_relays, 40);
        assert_eq!(b.max_review_rounds, 2);
        assert_eq!(b.max_wall_clock_min, 120);
        assert_eq!(b.quota_stop_pct, 90.0);
        // And they round-trip through the stored JSON.
        let s = serde_json::to_string(&b).unwrap();
        assert_eq!(Budget::from_json(&s), b);
        assert_eq!(Budget::from_json("not json"), b);
    }

    #[test]
    fn budget_patch_merges_and_validates() {
        let mut b = Budget::default();
        b.apply(&BudgetPatch { max_relays: Some(80), ..Default::default() }).unwrap();
        assert_eq!(b.max_relays, 80);
        assert_eq!(b.max_review_rounds, 2, "untouched fields keep their value");
        assert!(b.apply(&BudgetPatch { max_relays: Some(0), ..Default::default() }).is_err());
        assert!(b.apply(&BudgetPatch { quota_stop_pct: Some(101.0), ..Default::default() }).is_err());
        assert!(b.apply(&BudgetPatch { max_wall_clock_min: Some(0), ..Default::default() }).is_err());
        assert_eq!(b.max_relays, 80, "a rejected patch changes nothing else");
    }

    #[test]
    fn deliver_default_is_branch_not_pr() {
        // SPEC-team §12 #1 proposes `pr`; the user chose `branch`.
        assert_eq!(DEFAULT_DELIVER, "branch");
        assert!(DELIVERS.contains(&"pr"));
        assert!(!DEFAULT_SUPERVISED);
        assert_eq!(MAX_WORKERS, 4);
    }

    #[test]
    fn tasks_summary_counts_by_state() {
        let t = |state: &str| db::TeamTask {
            id: db::ulid(),
            team_id: "t".into(),
            issue_id: None,
            seq: 1,
            title: String::new(),
            brief: String::new(),
            files_json: "[]".into(),
            worker_bot_id: Some("b".into()),
            want_worker_bot_id: None,
            branch: String::new(),
            state: state.into(),
            round: 0,
            rebase_attempts: 0,
            last_report: None,
            last_verdict: None,
            merge_sha: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let s = tasks_summary(&[t("working"), t("working"), t("merged")]);
        assert_eq!(s["working"], 2);
        assert_eq!(s["merged"], 1);
        assert_eq!(s["total"], 3);
        assert_eq!(tasks_summary(&[])["total"], 0);
    }

    #[test]
    fn phase_terminality() {
        for p in ["done", "aborted", "failed"] {
            assert!(is_terminal(p));
        }
        for p in ["starting", "planning", "working", "finishing", "paused", "aborting"] {
            assert!(!is_terminal(p));
        }
    }

    fn proj(path: &str) -> db::Project {
        db::Project {
            id: "p".into(),
            path: path.into(),
            label: "p".into(),
            host: LOCAL_HOST.into(),
            workspace_id: None,
            deleted_at: None,
            created_at: String::new(),
        }
    }

    /// The two guards in front of `remove_worktrees`' destructive calls, at the level of the
    /// paths themselves. Same incident shape as `containment_rejects_the_users_own_checkout`,
    /// but on the *delete* side: `worktree remove --force --force` and `rm -rf` were the only
    /// two places `bots.cwd` / `teams.worktree_root` reached a destructive command without
    /// ever passing `check_layout`.
    #[test]
    fn only_directories_inside_the_team_root_may_be_removed() {
        let p = proj("/Users/u/project/agents-manager");
        let root = "/Users/u/.config/agents-manager/teams/01ABC";

        assert!(removable_member_dir(&p, root, &format!("{root}/dev-1")).is_ok());
        assert!(removable_member_dir(&p, root, root).is_ok(), "the root itself is inside itself");

        // The incident's own shape: cwd is the checkout.
        assert!(removable_member_dir(&p, root, &p.path).is_err());
        // A linked worktree the *user* made by hand. git will not save us here — it refuses
        // only the main working tree — and `--force --force` would take their uncommitted
        // work with it.
        assert!(removable_member_dir(&p, root, &format!("{}/.claude/worktrees/foo", p.path)).is_err());
        // Anywhere else at all.
        assert!(removable_member_dir(&p, root, "/Users/u/somewhere-else").is_err());
        assert!(removable_member_dir(&p, root, "/Users/u/.config/agents-manager/teams/01ABCD/dev-1").is_err());
        assert!(removable_member_dir(&p, root, "  ").is_err());
        assert!(removable_member_dir(&p, "", &format!("{root}/dev-1")).is_err(), "`worktree_root = ''` contains nothing");
    }

    /// `remove_dir` is a literal `rm -rf` of a database column, so it gets the strictest of
    /// the two: not the checkout, not inside it, and — the case string equality misses — not
    /// a directory that *contains* the checkout.
    #[test]
    fn the_team_root_is_never_rm_rfd_near_the_checkout() {
        let p = proj("/Users/u/project/agents-manager");
        assert!(removable_root(&p, "/Users/u/.config/agents-manager/teams/01ABC").is_ok());

        assert!(removable_root(&p, &p.path).is_err(), "the checkout itself");
        assert!(removable_root(&p, "/Users/u/project/agents-manager/").is_err(), "…with a trailing slash");
        assert!(removable_root(&p, "/Users/u/project/agents-manager/.claude/worktrees").is_err(), "inside it");
        assert!(removable_root(&p, "/Users/u/project").is_err(), "a parent takes the checkout with it");
        assert!(removable_root(&p, "/Users/u").is_err());
        assert!(removable_root(&p, "/").is_err());
        assert!(removable_root(&p, "").is_err(), "the `worktree_root = ''` shape");
        assert!(removable_root(&p, "   ").is_err());
        // A shared prefix is still not containment.
        assert!(removable_root(&p, "/Users/u/project/agents-manager-2").is_ok());
    }
}

#[cfg(test)]
pub mod testing {
    //! Shared fixtures for the team tests (here and in `team_sched`).
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    /// A stand-in herdr server speaking the real newline-JSON protocol on a real unix socket:
    /// one request per connection, `{"id","method","params"}` in, `{"id","result"}` out.
    ///
    /// It answers what creating a team needs (`ping`, the three `workspace.*` calls of §6.4a)
    /// plus the tab / pane bookkeeping the one-bot-one-tab work turns on: `tab.create`,
    /// `tab.list`, `tab.close`, `pane.split`, `pane.close`, `pane.get`, `pane.move`, and the
    /// `session.snapshot` / `agent.list` pair a reconcile runs on, plus the two calls that make
    /// a pane readable — `pane.read` (`set_screen`) and `pane.process_info` (`set_argv`), which
    /// is all an adopted, hook-less agent can be observed through. It stops there, deliberately:
    /// `agent.start` / `agent.wait` are also covered with lightweight bookkeeping so the reopen
    /// scheduler can exercise the real lifecycle path; hook injection and
    /// `ensure_kind_installed` probing for a real CLI still belong to a live agent.
    ///
    /// One place it knowingly differs from herdr 0.8.2: closing a tab's last pane does **not**
    /// reap the tab here, though the real server does. That is on purpose — the daemon must
    /// tidy the tab up itself rather than rely on that, and only a mock that leaves the empty
    /// tab standing can show whether it did.
    #[derive(Debug, Clone)]
    pub struct MockTab {
        pub tab_id: String,
        pub workspace_id: String,
        pub label: String,
        pub panes: Vec<String>,
    }

    pub struct MockHerdr {
        pub workspaces: Arc<StdMutex<BTreeMap<String, String>>>,
        /// Tabs in creation order, each holding its panes.
        pub tabs: Arc<StdMutex<Vec<MockTab>>>,
        /// `pane.read` answers, per pane id: the terminal snapshot a test wants scraped.
        pub screens: Arc<StdMutex<BTreeMap<String, String>>>,
        /// `pane.process_info` answers, per pane id: the argv the pane's CLI is running with.
        pub argvs: Arc<StdMutex<BTreeMap<String, Vec<String>>>>,
        /// `pane.process_info` answers, per pane id: the pid of that CLI (default 1). Only a
        /// test that reads something *off* the process — its account (SPEC §16.6) — needs the
        /// panes to be told apart by pid.
        pub pids: Arc<StdMutex<BTreeMap<String, i64>>>,
        /// Every `(method, params)` the daemon sent, so a test can assert *how* it asked —
        /// "started through `tab.create`, never `pane.split`" is only checkable here.
        pub calls: Arc<StdMutex<Vec<(String, Value)>>>,
        /// What `agent.list` and `agent.get` answer with. A test fills this in to describe
        /// the agents herdr is supposed to be running.
        pub agents: Arc<StdMutex<Vec<Value>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockHerdr {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    /// The mock's whole mutable world, so one lock covers a request.
    #[derive(Clone)]
    struct MockState {
        workspaces: Arc<StdMutex<BTreeMap<String, String>>>,
        tabs: Arc<StdMutex<Vec<MockTab>>>,
        calls: Arc<StdMutex<Vec<(String, Value)>>>,
        agents: Arc<StdMutex<Vec<Value>>>,
        screens: Arc<StdMutex<BTreeMap<String, String>>>,
        argvs: Arc<StdMutex<BTreeMap<String, Vec<String>>>>,
        pids: Arc<StdMutex<BTreeMap<String, i64>>>,
        seq: Arc<std::sync::atomic::AtomicU64>,
    }

    impl MockState {
        fn next(&self) -> u64 {
            self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        }
        fn pane_json(&self, pane_id: &str, tab: &MockTab, cwd: Option<&Value>) -> Value {
            // Whatever `agents` says is sitting in this pane — named or, like herdr after it
            // cleared a name, not — is what `pane.get` reports as the pane's agent.
            let occupant = self
                .agents
                .lock()
                .unwrap()
                .iter()
                .find(|a| a.get("pane_id").and_then(Value::as_str) == Some(pane_id))
                .cloned();
            let (agent, status) = match occupant {
                Some(a) => (a.get("agent").cloned().unwrap_or(Value::Null), a.get("agent_status").cloned().unwrap_or(Value::Null)),
                None => (Value::Null, Value::Null),
            };
            json!({"pane_id": pane_id, "workspace_id": tab.workspace_id, "tab_id": tab.tab_id,
                   "cwd": cwd.cloned().unwrap_or(Value::Null), "agent": agent, "agent_status": status})
        }
        fn tab_json(t: &MockTab) -> Value {
            json!({"tab_id": t.tab_id, "workspace_id": t.workspace_id, "label": t.label,
                   "pane_count": t.panes.len()})
        }
        /// Add a tab holding one fresh pane. Returns (tab, pane_id).
        fn new_tab(&self, workspace_id: &str, label: &str) -> (MockTab, String) {
            let n = self.next();
            let tab = MockTab {
                tab_id: format!("{workspace_id}:t{n}"),
                workspace_id: workspace_id.to_string(),
                label: label.to_string(),
                panes: vec![format!("{workspace_id}:p{n}")],
            };
            let pane = tab.panes[0].clone();
            self.tabs.lock().unwrap().push(tab.clone());
            (tab, pane)
        }
        fn find_pane(&self, pane_id: &str) -> Option<MockTab> {
            self.tabs.lock().unwrap().iter().find(|t| t.panes.iter().any(|p| p == pane_id)).cloned()
        }
    }

    impl MockHerdr {
        pub fn start(socket: std::path::PathBuf) -> MockHerdr {
            let _ = std::fs::remove_file(&socket);
            let listener = tokio::net::UnixListener::bind(&socket).expect("bind mock herdr socket");
            let state = MockState {
                workspaces: Default::default(),
                tabs: Default::default(),
                calls: Default::default(),
                agents: Default::default(),
                screens: Default::default(),
                argvs: Default::default(),
                pids: Default::default(),
                seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            };
            let (workspaces, tabs, calls, agents) =
                (state.workspaces.clone(), state.tabs.clone(), state.calls.clone(), state.agents.clone());
            let (screens, argvs, pids) = (state.screens.clone(), state.argvs.clone(), state.pids.clone());
            let handle = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let st = state.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                        let (r, mut w) = stream.into_split();
                        let mut line = String::new();
                        if BufReader::new(r).read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
                        let id = req.get("id").cloned().unwrap_or(Value::Null);
                        let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                        let params = req.get("params").cloned().unwrap_or(json!({}));
                        st.calls.lock().unwrap().push((method.clone(), params.clone()));
                        let wid_of = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                        let out = match method.as_str() {
                            "ping" => json!({"id": id, "result": {"version": "mock", "protocol": 20}}),
                            "workspace.create" => {
                                let wid = format!("ws-{}", st.next());
                                let label = params.get("label").and_then(Value::as_str).unwrap_or("").to_string();
                                st.workspaces.lock().unwrap().insert(wid.clone(), label.clone());
                                let (tab, pane) = st.new_tab(&wid, &label);
                                json!({"id": id, "result": {
                                    "workspace": {"workspace_id": wid, "label": label, "pane_count": 1},
                                    "tab": MockState::tab_json(&tab),
                                    "root_pane": st.pane_json(&pane, &tab, params.get("cwd"))}})
                            }
                            "workspace.get" => {
                                let wid = wid_of("workspace_id");
                                match st.workspaces.lock().unwrap().get(&wid).cloned() {
                                    Some(label) => json!({"id": id, "result": {"workspace":
                                        {"workspace_id": wid, "label": label, "pane_count": 1}}}),
                                    None => json!({"id": id, "error": {"code": "not_found", "message": "no such workspace"}}),
                                }
                            }
                            "workspace.close" => {
                                let wid = wid_of("workspace_id");
                                st.workspaces.lock().unwrap().remove(&wid);
                                st.tabs.lock().unwrap().retain(|t| t.workspace_id != wid);
                                json!({"id": id, "result": {}})
                            }
                            "tab.create" => {
                                let wid = wid_of("workspace_id");
                                let label = params.get("label").and_then(Value::as_str).unwrap_or("").to_string();
                                if !st.workspaces.lock().unwrap().contains_key(&wid) {
                                    json!({"id": id, "error": {"code": "workspace_not_found", "message": wid}})
                                } else {
                                    let (tab, pane) = st.new_tab(&wid, &label);
                                    json!({"id": id, "result": {"type": "tab_created",
                                        "tab": MockState::tab_json(&tab),
                                        "root_pane": st.pane_json(&pane, &tab, params.get("cwd"))}})
                                }
                            }
                            "tab.list" => {
                                let wid = wid_of("workspace_id");
                                let tabs: Vec<Value> = st
                                    .tabs
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .filter(|t| wid.is_empty() || t.workspace_id == wid)
                                    .map(MockState::tab_json)
                                    .collect();
                                json!({"id": id, "result": {"type": "tab_list", "tabs": tabs}})
                            }
                            "tab.close" => {
                                let tid = wid_of("tab_id");
                                let mut tabs = st.tabs.lock().unwrap();
                                let before = tabs.len();
                                tabs.retain(|t| t.tab_id != tid);
                                if tabs.len() == before {
                                    json!({"id": id, "error": {"code": "tab_not_found", "message": tid}})
                                } else {
                                    json!({"id": id, "result": {"type": "ok"}})
                                }
                            }
                            "pane.split" => {
                                let target = wid_of("target_pane_id");
                                let mut tabs = st.tabs.lock().unwrap();
                                match tabs.iter_mut().find(|t| t.panes.iter().any(|p| *p == target)) {
                                    None => json!({"id": id, "error": {"code": "pane_not_found", "message": target}}),
                                    Some(t) => {
                                        let pane = format!("{}:p{}", t.workspace_id, st.next());
                                        t.panes.push(pane.clone());
                                        json!({"id": id, "result": {"type": "pane_info", "pane":
                                            json!({"pane_id": pane, "workspace_id": t.workspace_id,
                                                   "tab_id": t.tab_id, "cwd": params.get("cwd"),
                                                   "agent": null, "agent_status": null})}})
                                    }
                                }
                            }
                            "pane.close" => {
                                // NB: no tab reaping — see the type comment.
                                let pid = wid_of("pane_id");
                                let mut tabs = st.tabs.lock().unwrap();
                                for t in tabs.iter_mut() {
                                    t.panes.retain(|p| *p != pid);
                                }
                                json!({"id": id, "result": {"type": "ok"}})
                            }
                            "pane.get" => {
                                let pid = wid_of("pane_id");
                                match st.find_pane(&pid) {
                                    Some(t) => json!({"id": id, "result": {"type": "pane_info",
                                        "pane": st.pane_json(&pid, &t, None)}}),
                                    None => json!({"id": id, "error": {"code": "pane_not_found", "message": pid}}),
                                }
                            }
                            "pane.move" => {
                                let pid = wid_of("pane_id");
                                let label = params
                                    .get("destination")
                                    .and_then(|d| d.get("label"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                match st.find_pane(&pid) {
                                    None => json!({"id": id, "error": {"code": "pane_not_found", "message": pid}}),
                                    Some(prev) => {
                                        // Take it out of its old tab (left standing, empty or not)
                                        // and give it a brand new one, keeping the pane id.
                                        st.tabs.lock().unwrap().iter_mut().for_each(|t| t.panes.retain(|p| *p != pid));
                                        let n = st.next();
                                        let tab = MockTab {
                                            tab_id: format!("{}:t{n}", prev.workspace_id),
                                            workspace_id: prev.workspace_id.clone(),
                                            label,
                                            panes: vec![pid.clone()],
                                        };
                                        st.tabs.lock().unwrap().push(tab.clone());
                                        json!({"id": id, "result": {"type": "pane_move", "move_result": {
                                            "changed": true,
                                            "previous_pane_id": pid,
                                            "previous_workspace_id": prev.workspace_id,
                                            "previous_tab_id": prev.tab_id,
                                            "created_tab": MockState::tab_json(&tab),
                                            "pane": st.pane_json(&pid, &tab, None)}}})
                                    }
                                }
                            }
                            "session.snapshot" => {
                                let tabs = st.tabs.lock().unwrap().clone();
                                let names: Vec<String> = st
                                    .agents
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .filter_map(|a| a.get("pane_id").and_then(Value::as_str).map(String::from))
                                    .collect();
                                let panes: Vec<Value> = tabs
                                    .iter()
                                    .flat_map(|t| t.panes.iter())
                                    .map(|p| json!({"pane_id": p, "agent": if names.contains(p) { json!("x") } else { Value::Null }}))
                                    .collect();
                                let wss: Vec<Value> = st
                                    .workspaces
                                    .lock()
                                    .unwrap()
                                    .keys()
                                    .map(|w| json!({"workspace_id": w}))
                                    .collect();
                                json!({"id": id, "result": {"type": "session_snapshot", "snapshot":
                                    {"workspaces": wss, "panes": panes,
                                     "tabs": tabs.iter().map(MockState::tab_json).collect::<Vec<_>>()}}})
                            }
                            "pane.read" => {
                                let pid = wid_of("pane_id");
                                let text = st.screens.lock().unwrap().get(&pid).cloned().unwrap_or_default();
                                json!({"id": id, "result": {"type": "pane_read", "read": {
                                    "pane_id": pid, "source": params.get("source").cloned().unwrap_or(json!("recent_unwrapped")),
                                    "format": "text", "text": text, "revision": 1, "truncated": false}}})
                            }
                            "pane.process_info" => {
                                let pid = wid_of("pane_id");
                                let os_pid = st.pids.lock().unwrap().get(&pid).copied().unwrap_or(1);
                                match st.argvs.lock().unwrap().get(&pid).cloned() {
                                    None => json!({"id": id, "result": {"process_info":
                                        {"pane_id": pid, "foreground_processes": []}}}),
                                    Some(argv) => json!({"id": id, "result": {"process_info": {"pane_id": pid,
                                        "foreground_processes": [{"argv": argv, "argv0": argv.first().cloned(),
                                        "cwd": "/tmp/p", "pid": os_pid}]}}}),
                                }
                            }
                            "agent.send_keys" => {
                                let target = wid_of("target");
                                let ctrl_c = params
                                    .get("keys")
                                    .and_then(Value::as_array)
                                    .map(|keys| keys.iter().any(|k| k.as_str() == Some("ctrl+c")))
                                    .unwrap_or(false);
                                if ctrl_c {
                                    st.agents
                                        .lock()
                                        .unwrap()
                                        .retain(|a| a.get("name").and_then(Value::as_str) != Some(target.as_str()));
                                }
                                json!({"id": id, "result": {}})
                            }
                            "agent.start" => {
                                let name = wid_of("name");
                                let kind = wid_of("kind");
                                let pane_id = wid_of("pane_id");
                                let args = params
                                    .get("args")
                                    .and_then(Value::as_array)
                                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                                    .unwrap_or_default();
                                let tab = st.find_pane(&pane_id);
                                let (workspace_id, tab_id, cwd) = tab
                                    .map(|t| (t.workspace_id, t.tab_id, params.get("cwd").and_then(Value::as_str).unwrap_or("/tmp/p").to_string()))
                                    .unwrap_or_else(|| ("ws-1".into(), "tab-1".into(), "/tmp/p".into()));
                                let agent = json!({
                                    "name": name,
                                    "agent": kind,
                                    "agent_status": "idle",
                                    "workspace_id": workspace_id,
                                    "tab_id": tab_id,
                                    "pane_id": pane_id,
                                    "cwd": cwd,
                                    "interactive_ready": true,
                                    "launch_pending": false,
                                    "state_change_seq": 1,
                                    "revision": 1,
                                });
                                st.argvs.lock().unwrap().insert(pane_id, args);
                                let mut agents = st.agents.lock().unwrap();
                                agents.retain(|a| a.get("name") != Some(&Value::String(name.clone())));
                                agents.push(agent.clone());
                                json!({"id": id, "result": {"agent": agent}})
                            }
                            "agent.rename" => {
                                let target = wid_of("target");
                                let name = params.get("name").and_then(Value::as_str).map(String::from);
                                let mut agents = st.agents.lock().unwrap();
                                let hit = agents.iter_mut().find(|a| {
                                    a.get("pane_id").and_then(Value::as_str) == Some(target.as_str())
                                        || a.get("name").and_then(Value::as_str) == Some(target.as_str())
                                });
                                match hit {
                                    Some(a) => {
                                        a["name"] = name.map(Value::from).unwrap_or(Value::Null);
                                        json!({"id": id, "result": {"agent": a.clone()}})
                                    }
                                    None => json!({"id": id, "error": {"code": "agent_not_found", "message": target}}),
                                }
                            }
                            "agent.wait" => {
                                let target = wid_of("target");
                                match st
                                    .agents
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .find(|a| a.get("name").and_then(Value::as_str) == Some(target.as_str()))
                                    .cloned()
                                {
                                    Some(agent) => json!({"id": id, "result": {"agent": agent}}),
                                    None => json!({"id": id, "error": {"code": "not_found", "message": target}}),
                                }
                            }
                            "agent.list" => {
                                json!({"id": id, "result": {"agents": st.agents.lock().unwrap().clone()}})
                            }
                            "agent.get" => {
                                // Like herdr, a target is a live agent name or the id of the pane
                                // hosting it.
                                let target = wid_of("target");
                                let found = st
                                    .agents
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .find(|a| {
                                        a.get("name").and_then(Value::as_str) == Some(target.as_str())
                                            || a.get("pane_id").and_then(Value::as_str) == Some(target.as_str())
                                    })
                                    .cloned();
                                match found {
                                    Some(a) => json!({"id": id, "result": {"agent": a}}),
                                    None => json!({"id": id, "error": {"code": "not_found", "message": target}}),
                                }
                            }
                            other => json!({"id": id, "error": {"code": "unsupported",
                                            "message": format!("mock herdr does not implement {other}")}}),
                        };
                        let mut bytes = serde_json::to_vec(&out).unwrap();
                        bytes.push(b'\n');
                        let _ = w.write_all(&bytes).await;
                        let _ = w.flush().await;
                    });
                }
            });
            MockHerdr { workspaces, tabs, calls, agents, screens, argvs, pids, handle }
        }

        pub fn count(&self) -> usize {
            self.workspaces.lock().unwrap().len()
        }

        /// The methods the daemon called, in order.
        pub fn methods(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
        }

        /// The params of the first call to `method`, if it was made at all.
        pub fn first_call(&self, method: &str) -> Option<Value> {
            self.calls.lock().unwrap().iter().find(|(m, _)| m == method).map(|(_, p)| p.clone())
        }

        /// What `pane.read` will answer for `pane_id`.
        pub fn set_screen(&self, pane_id: &str, text: &str) {
            self.screens.lock().unwrap().insert(pane_id.to_string(), text.to_string());
        }

        /// What `pane.process_info` will report as the pane's foreground argv.
        pub fn set_argv(&self, pane_id: &str, argv: &[&str]) {
            self.argvs.lock().unwrap().insert(pane_id.to_string(), argv.iter().map(|s| s.to_string()).collect());
        }

        /// What `pane.process_info` will report as that CLI's pid.
        pub fn set_pid(&self, pane_id: &str, pid: i64) {
            self.pids.lock().unwrap().insert(pane_id.to_string(), pid);
        }

        pub fn tab(&self, tab_id: &str) -> Option<MockTab> {
            self.tabs.lock().unwrap().iter().find(|t| t.tab_id == tab_id).cloned()
        }

        pub fn tabs_in(&self, workspace_id: &str) -> Vec<MockTab> {
            self.tabs.lock().unwrap().iter().filter(|t| t.workspace_id == workspace_id).cloned().collect()
        }
    }

    pub struct Env {
        pub app: Arc<App>,
        pub project_id: String,
        /// The user's checkout — the thing §6.1 promises never to write to.
        pub repo: std::path::PathBuf,
        pub dir: std::path::PathBuf,
        pub herdr: MockHerdr,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A daemon with a real sqlite file, a real (throw-away) git repository as its project,
    /// and a mock herdr behind the socket.
    ///
    /// The data directory is deliberately a **sibling** of the repository, not a child: that
    /// is the §6.2 guarantee under test, and putting `teams/` inside the checkout would make
    /// every "the user's tree stayed clean" assertion vacuous.
    pub async fn env() -> Env {
        let dir = std::env::temp_dir().join(format!("am-team-{}", db::ulid()));
        let repo = dir.join("repo");
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        crate::team_git::testing::init_repo(&repo);
        let pool = db::open(&data.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(data.join("config.toml")).await.unwrap();
        let sock = data.join("herdr.sock");
        let herdr = MockHerdr::start(sock.clone());
        let client = crate::herdr::HerdrClient::new(sock);
        let app = App::new(
            pool,
            client.clone(),
            client,
            cfg,
            data.clone(),
            data.join("agents-managerd"),
            7799,
            "test-token".into(),
            "test".into(),
            false,
        );
        app.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let pid = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?, 'local', ?)")
            .bind(&pid)
            .bind(repo.to_string_lossy().to_string())
            .bind("proj")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        Env { app, project_id: pid, repo, dir, herdr }
    }

    pub fn role(kind: &str) -> RoleSpec {
        RoleSpec { kind: kind.into(), model: None, effort: None, fast: false, identity: None, persona_extra: None }
    }

    pub fn workers_spec(kind: &str, count: Option<u32>) -> WorkersSpec {
        WorkersSpec {
            count,
            kind: kind.into(),
            model: None,
            effort: None,
            fast: false,
            identity: None,
            persona_extra: None,
        }
    }

    pub fn req(count: Option<u32>, reviewer: bool) -> CreateTeam {
        CreateTeam {
            repo: None,
            issue_numbers: None,
            issue_number: Some(42),
            pm: role("codex"),
            workers: workers_spec("claude", count),
            reviewer: reviewer.then(|| role("grok")),
            base: None,
            deliver: None,
            supervised: None,
            budget: None,
        }
    }

    pub fn issue() -> IssueRef {
        IssueRef {
            number: 42,
            title: "make it work".into(),
            url: "https://example.invalid/42".into(),
            body: "讓它動起來。".into(),
        }
    }

    pub async fn make_team(app: &Arc<App>, pid: &str, r: CreateTeam) -> String {
        make_team_with(app, pid, r, vec![issue()]).await
    }

    /// A team with a whole issue queue (§2.3).
    pub async fn make_team_with(app: &Arc<App>, pid: &str, r: CreateTeam, issues: Vec<IssueRef>) -> String {
        create_with_issues(app, pid, r, issues).await.unwrap()["team_id"].as_str().unwrap().to_string()
    }

    /// A second issue for queue tests.
    pub fn issue2() -> IssueRef {
        IssueRef {
            number: 43,
            title: "and then this".into(),
            url: "https://example.invalid/43".into(),
            body: "再做這個。".into(),
        }
    }

    /// A `runs` row that looks running to every precondition in `prompt_grouped`, without a
    /// live agent behind it. The RPC that follows fails, which is `delivery = "unknown"` —
    /// a real §9.1 outcome, and the one that lets a relay's DB side be tested end to end.
    pub async fn fake_run(app: &Arc<App>, bot_id: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1',?, 'agent', 'test', ?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(format!("pane-{bot_id}"))
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        id
    }
}

#[cfg(test)]
mod api_tests {
    use super::testing::*;
    use super::*;

    /// §10.1 defaults: 2 workers + a reviewer, `deliver=branch`, `supervised=false`, the
    /// §12 budget — and the members are ordinary `bots` rows tagged `managed_by='team'`.
    #[tokio::test]
    async fn create_writes_the_team_and_its_members() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(None, true)).await;

        let t = load(&app, &tid).await.unwrap();
        assert_eq!(t.phase, "starting");
        assert_eq!(t.deliver, "branch", "§12 #1: the user's default is `branch`, not `pr`");
        assert_eq!(t.supervised, 0);
        assert_eq!(t.base_ref, "HEAD");
        assert_eq!(t.issue_number, 42);
        assert_eq!(t.branch, integration_branch(42, &tid6(&tid)));
        assert_eq!(Budget::from_json(&t.budget_json), Budget::default());
        // §7.4: both NOT NULL columns were resolved before the row was written.
        assert_eq!(t.base_sha.len(), 40, "base_sha is a real commit");
        assert_eq!(t.worktree_root, worktree_root(&app, &tid));

        let members = db::team_members(&app.db, &tid).await.unwrap();
        // §7.1 (2026-09-08): `workers.count` is the parallelism and defaults to 1.
        assert_eq!(members.len(), 3, "pm + 1 worker + reviewer");
        let roles: Vec<&str> = members.iter().map(|m| m.team_role.as_deref().unwrap()).collect();
        assert_eq!(roles, vec!["pm", "worker", "reviewer"]);
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        let t6 = tid6(&tid);
        assert_eq!(
            names,
            vec![
                format!("t{t6}-pm"),
                format!("t{t6}-i1-dev-1"),
                format!("t{t6}-rev")
            ]
        );
        for m in &members {
            assert_eq!(m.managed_by, "team");
            assert_eq!(m.team_id.as_deref(), Some(tid.as_str()));
            assert!(m.cwd.is_some(), "a member always has an explicit cwd");
            assert!(m.persona.is_some());
            // Every member has a conversation, so the timeline works from minute one.
            assert!(db::conversation_id(&app.db, &m.id).await.is_ok());
        }
        assert_eq!(members[0].kind, "codex");
        assert_eq!(members[1].kind, "claude");
        assert_eq!(members[2].kind, "grok");

        // The creation is logged.
        let evs = events(&app, &tid, None, 100).await.unwrap();
        assert_eq!(evs["events"][0]["kind"], "phase");
        assert_eq!(evs["events"][0]["payload"]["to"], "starting");

        // …and none of this reached config.toml (SPEC-team §5.3).
        assert!(app.cfg.get().await.projects.is_empty());

        // T1 (§13): the §6.2 layout is real on disk, and it is outside the repository.
        let root = std::path::PathBuf::from(&t.worktree_root);
        assert!(root.starts_with(&app.data_dir), "the team root lives under the data dir, not the repo");
        assert!(!root.starts_with(&e.repo));
        assert!(root.join("ISSUE.md").exists() && root.join("TEAM.md").exists());
        assert!(std::fs::read_to_string(root.join("ISSUE.md")).unwrap().contains("讓它動起來"));
        for (i, m) in members.iter().enumerate() {
            let cwd = std::path::PathBuf::from(m.cwd.clone().unwrap());
            let want = root.join(member_dir(m.team_role.as_deref().unwrap(), i as u32, 1));
            assert_eq!(cwd, want, "{} lives in its own worktree", m.name);
            assert!(cwd.join(".git").exists(), "{} is a real worktree", m.name);
            // The doc copies are inside the member's cwd and ignored (§6.2).
            let docs = cwd.join(".agents-manager/team");
            assert!(docs.join("ISSUE.md").exists() && docs.join("TEAM.md").exists());
            assert_eq!(std::fs::read_to_string(docs.join(".gitignore")).unwrap(), "*\n");
            assert!(m.persona.as_deref().unwrap().contains(m.cwd.as_deref().unwrap()), "the persona names its cwd");
        }
        // The PM holds the integration branch; everyone else is detached (§6.2).
        let g = |d: &std::path::Path, a: &[&str]| crate::team_git::testing::run(d, a);
        assert_eq!(g(&root.join("main"), &["rev-parse", "--abbrev-ref", "HEAD"]), t.branch);
        assert_eq!(g(&root.join("i1-dev-1"), &["rev-parse", "--abbrev-ref", "HEAD"]), "HEAD");

        // §6.4a: the team got its own workspace, and it is not the project's.
        assert_eq!(e.herdr.count(), 1);
        assert!(t.workspace_id.is_some());
        assert_eq!(
            db::project(&app.db, &pid).await.unwrap().unwrap().workspace_id,
            None,
            "the project's own workspace was never touched"
        );

        // T8's core promise, checked at creation time: nothing was written into the checkout.
        assert_eq!(g(&e.repo, &["status", "--porcelain"]), "");
        assert_eq!(g(&e.repo, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 4);
    }

    /// §6.4a: if the workspace cannot be made, the whole team fails and unwinds — it must
    /// never fall back to the project's workspace, which is the user's own working space.
    #[tokio::test]
    async fn a_team_without_a_workspace_is_not_created() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        // Pull the socket out from under it: `workspace.create` now fails.
        app.connected.store(false, std::sync::atomic::Ordering::SeqCst);
        let err = create_with_issues(&app, &pid, req(Some(1), false), vec![issue()]).await.unwrap_err();
        match err {
            LcError::Conflict(detail) => assert_eq!(detail["reason"], "host is not connected"),
            other => panic!("expected the workspace failure to survive rollback, got {other:?}"),
        }
        // The row is parked in `failed` and every worktree it had built is gone again.
        let teams = db::teams_of_project(&app.db, &pid).await.unwrap();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].phase, "failed");
        assert!(!std::path::Path::new(&teams[0].worktree_root).exists(), "the half-built root was removed");
        let listed = crate::team_git::testing::run(&e.repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(listed.matches("worktree ").count(), 1, "no orphan registrations: {listed}");
        let branches = crate::team_git::testing::run(&e.repo, &["branch", "--list", &teams[0].branch]);
        assert!(branches.trim().is_empty(), "the empty integration branch was removed: {branches}");
    }

    /// A failed creation must not destroy work that landed on the integration branch while
    /// rollback was running. This uses the same temporary-repo branch path as create and calls
    /// the real rollback after putting one commit on that branch.
    #[tokio::test]
    async fn rollback_keeps_an_integration_branch_with_commits() {
        let e = env().await;
        let app = e.app.clone();
        let project = db::project(&app.db, &e.project_id).await.unwrap().unwrap();
        let repo = e.repo.to_string_lossy().to_string();
        let base = crate::team_git::resolve_commit(&app, LOCAL_HOST, &repo, "HEAD")
            .await
            .unwrap();
        let branch = "team/i42-preserve";
        crate::team_git::create_branch(&app, LOCAL_HOST, &repo, branch, &base)
            .await
            .unwrap();
        let root = e.dir.join("rollback-root");
        std::fs::create_dir_all(&root).unwrap();
        let wt = root.join("branch-worktree");
        let wt_path = wt.to_string_lossy().to_string();
        crate::team_git::worktree_add(
            &app,
            LOCAL_HOST,
            &repo,
            &wt_path,
            branch,
            false,
        )
        .await
        .unwrap();
        std::fs::write(wt.join("work.txt"), "user work\n").unwrap();
        assert!(crate::team_git::commit_all(&app, LOCAL_HOST, &wt_path, "user work").await.unwrap());

        let root_path = root.to_string_lossy().to_string();
        rollback_create(
            &app,
            &project,
            "",
            "test-team",
            &[],
            &root_path,
            &[wt_path],
            None,
            branch,
            &base,
            true,
        )
        .await;

        assert!(!wt.exists(), "rollback removed the worktree and released the branch");
        let branches = crate::team_git::testing::run(&e.repo, &["branch", "--list", branch]);
        assert!(branches.contains(branch), "a branch with work must survive rollback: {branches}");
    }

    /// The TOML projection must not soft-delete team members just because they are not in
    /// config.toml (SPEC-team §5.3 — the one exception to the projection rule).
    #[tokio::test]
    async fn projection_leaves_team_members_alone() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        crate::projection::project_config(&app.cfg, &app.db).await.unwrap();
        let members = db::team_members(&app.db, &tid).await.unwrap();
        assert_eq!(members.len(), 2);
        assert!(members.iter().all(|m| m.deleted_at.is_none()), "team members survive a reprojection");
    }

    #[tokio::test]
    async fn create_rejects_bad_input() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let bad = |r: CreateTeam| async { create_with_issues(&app, &pid, r, vec![issue()]).await };

        // §12 #7: at most four executors. `0` is no longer out of range — §4.5 (2026-09-09)
        // gave it a meaning, `unlimited`, and it is covered by the unlimited scenarios.
        let mut r = req(Some(5), false);
        assert!(matches!(bad(r).await, Err(LcError::Bad(_))));
        // unknown kind
        r = req(Some(1), false);
        r.pm.kind = "gemini".into();
        assert!(matches!(bad(r).await, Err(LcError::Bad(_))));
        // deliver must be branch | pr
        r = req(Some(1), false);
        r.deliver = Some("merge".into());
        assert!(matches!(bad(r).await, Err(LcError::Bad(_))));
        // budget bounds
        r = req(Some(1), false);
        r.budget = Some(BudgetPatch { max_relays: Some(-1), ..Default::default() });
        assert!(matches!(bad(r).await, Err(LcError::Bad(_))));
        // an effort the kind does not accept
        r = req(Some(1), false);
        r.workers.effort = Some("turbo".into());
        r.workers.kind = "codex".into();
        assert!(matches!(bad(r).await, Err(LcError::Bad(_))));
        // an unknown project
        assert!(matches!(create_with_issues(&app, "nope", req(None, false), vec![issue()]).await, Err(LcError::NotFound(_))));
        // nothing was written by any of those
        assert!(db::teams_of_project(&app.db, &pid).await.unwrap().is_empty());
    }

    /// §9.2: a kind already at or above `quota_stop_pct` refuses the team with a
    /// machine-readable 400.
    #[tokio::test]
    async fn create_refuses_when_quota_is_low() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        app.quotas.lock().await.insert(
            "claude".into(),
            crate::quota::Quota {
                five_hour: Some(crate::quota::Window { used_pct: 93.0, resets_at: None }),
                seven_day: None,
                fable: None,
                reset_credits: None,
                limit_hit: None,
                plan: None,
                updated_at: db::now(),
                source: "test".into(),
                account: None,
                host: LOCAL_HOST.into(),
            },
        );
        match create_with_issues(&app, &pid, req(Some(1), false), vec![issue()]).await {
            Err(LcError::BadValue(v)) => {
                assert_eq!(v["error"], "quota_low");
                assert_eq!(v["kind"], "claude");
                assert_eq!(v["used_pct"], 93.0);
            }
            other => panic!("expected quota_low, got {other:?}"),
        }
        // Raising the team's own threshold above the reading lets it through (§12 #4).
        let mut r = req(Some(1), false);
        r.budget = Some(BudgetPatch { quota_stop_pct: Some(95.0), ..Default::default() });
        assert!(create_with_issues(&app, &pid, r, vec![issue()]).await.is_ok());
        // `quota_stop_pct = 100` switches the check off: even a 100% reading goes through.
        app.quotas.lock().await.get_mut("claude").unwrap().five_hour =
            Some(crate::quota::Window { used_pct: 100.0, resets_at: None });
        let mut r = req(Some(1), false);
        r.budget = Some(BudgetPatch { quota_stop_pct: Some(100.0), ..Default::default() });
        assert!(create_with_issues(&app, &pid, r, vec![issue()]).await.is_ok());
    }

    /// §7.3: a second team on the same issue disambiguates the member nicknames.
    #[tokio::test]
    async fn a_second_team_on_the_same_issue_gets_distinct_names() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let a = make_team(&app, &pid, req(Some(1), false)).await;
        let b = make_team(&app, &pid, req(Some(1), false)).await;
        assert_ne!(a, b);
        // Nicknames are keyed on the team, not the issue (§7.3), so two teams on the same
        // issue no longer collide at all — `insert_member`'s `-<tid6>` suffix stays only as a
        // guard against a user-created bot that happens to own the name.
        let names: Vec<String> = db::team_members(&app.db, &b).await.unwrap().into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec![format!("t{}-pm", tid6(&b)), format!("t{}-i1-dev-1", tid6(&b))]);
        let a_names: Vec<String> = db::team_members(&app.db, &a).await.unwrap().into_iter().map(|m| m.name).collect();
        assert!(a_names.iter().all(|n| !names.contains(n)), "{a_names:?} vs {names:?}");
    }

    /// §2.5.1: only an uncleaned `done` Team may be reopened. Other terminal states keep the
    /// old finished guard, and a cleaned Team has no live PM to continue the conversation.
    #[tokio::test]
    async fn reopen_refuses_aborted_failed_and_cleaned_up() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        for phase in ["aborted", "failed"] {
            let tid = make_team(&app, &pid, req(Some(1), false)).await;
            set_phase(&app, &tid, phase, None, None).await.unwrap();
            let before = db::team_issues(&app.db, &tid).await.unwrap().len();
            match add_issues(&app, &tid, &[57]).await {
                Err(LcError::Conflict(v)) => assert_eq!(v["reason"], "team is finished"),
                other => panic!("expected finished conflict for {phase}, got {other:?}"),
            }
            assert_eq!(db::team_issues(&app.db, &tid).await.unwrap().len(), before);
        }

        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        set_phase(&app, &tid, "done", None, None).await.unwrap();
        let pm = db::team_members(&app.db, &tid)
            .await
            .unwrap()
            .into_iter()
            .find(|b| b.team_role.as_deref() == Some("pm"))
            .unwrap();
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?")
            .bind(db::now())
            .bind(&pm.id)
            .execute(&app.db)
            .await
            .unwrap();
        let before = db::team_issues(&app.db, &tid).await.unwrap().len();
        match add_issues(&app, &tid, &[57]).await {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v["reason"], "team is cleaned up");
                assert_eq!(v["phase"], "done");
            }
            other => panic!("expected cleaned-up conflict, got {other:?}"),
        }
        assert_eq!(db::team_issues(&app.db, &tid).await.unwrap().len(), before);
    }

    /// §2.5.6 #1: appending to an uncleaned `done` team queues the issue **and** puts the team
    /// back on the road to `planning`. `add_issues` itself needs `gh`, so the test drives the
    /// two halves it is made of — the same pair the endpoint calls.
    #[tokio::test]
    async fn reopen_queues_the_issue_and_restarts_the_team() {
        let e = env().await;
        let tid = make_team(&e.app, &e.project_id, req(Some(1), true)).await;
        sqlx::query("UPDATE team_issues SET state='done' WHERE team_id=?")
            .bind(&tid)
            .execute(&e.app.db)
            .await
            .unwrap();
        set_phase(&e.app, &tid, "done", None, None).await.unwrap();
        let before = events(&e.app, &tid, None, 500).await.unwrap()["events"].as_array().unwrap().len();

        let t = load(&e.app, &tid).await.unwrap();
        assert!(check_add_issues(&e.app, &t, &[43]).await.unwrap(), "a done team appends as a reopen");
        add_resolved_issues(&e.app, &tid, vec![issue2()], true).await.unwrap();

        let queue = db::team_issues(&e.app.db, &tid).await.unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!((queue[1].seq, queue[1].issue_number, queue[1].state.as_str()), (2, 43, "queued"));

        // The reopen leaves the team resumable, not paused, and drops the terminal stamp.
        let t = load(&e.app, &tid).await.unwrap();
        assert_eq!(t.phase, "starting");
        assert!(t.pause_reason.is_none());
        assert!(t.ended_at.is_none());

        let evs = events(&e.app, &tid, None, 500).await.unwrap();
        let evs = evs["events"].as_array().unwrap();
        let tail: Vec<(String, Value)> =
            evs[before..].iter().map(|v| (v["kind"].as_str().unwrap().to_string(), v["payload"].clone())).collect();
        assert_eq!(tail.len(), 3, "issues_queued, team_reopened, then the phase move: {tail:?}");
        assert_eq!(tail[0].0, "note");
        assert_eq!(tail[0].1["action"], "issues_queued");
        assert_eq!(tail[0].1["issue_numbers"], json!([43]));
        assert_eq!(tail[1].0, "note");
        assert_eq!(tail[1].1["action"], "team_reopened");
        assert_eq!(tail[1].1["by"], "user");
        assert_eq!(tail[1].1["from_phase"], "done");
        assert_eq!(tail[1].1["issue_numbers"], json!([43]));
        assert_eq!(tail[2].0, "phase");
        assert_eq!((tail[2].1["from"].as_str(), tail[2].1["to"].as_str()), (Some("done"), Some("starting")));
        assert_eq!(tail[2].1["reason"], "reopen");
    }

    /// §2.3: "already queued" is `queued` / `working` only. A `done` / `failed` / `skipped`
    /// entry is the record of a finished pass, so the same issue can go back on the queue —
    /// the retry after a failure, or a second pass someone asks for. Before this, one pass
    /// over #42 made #42 unqueueable in that team for ever, which is what the UI hit as
    /// `409 issue already queued`.
    #[tokio::test]
    async fn a_finished_issue_can_be_queued_again() {
        let e = env().await;
        let tid = make_team(&e.app, &e.project_id, req(Some(1), false)).await;
        let set_state = |state: &'static str| {
            let (db, tid) = (e.app.db.clone(), tid.clone());
            async move {
                sqlx::query("UPDATE team_issues SET state=? WHERE team_id=?")
                    .bind(state)
                    .bind(&tid)
                    .execute(&db)
                    .await
                    .unwrap();
            }
        };

        for state in ["done", "failed", "skipped"] {
            set_state(state).await;
            let t = load(&e.app, &tid).await.unwrap();
            assert!(
                check_add_issues(&e.app, &t, &[42]).await.is_ok(),
                "a {state} #42 no longer holds the number"
            );
        }
        // The two states that do hold it: the queue would otherwise have two live entries for
        // one issue and the second could never be started.
        for state in ["queued", "working"] {
            set_state(state).await;
            let t = load(&e.app, &tid).await.unwrap();
            match check_add_issues(&e.app, &t, &[42]).await {
                Err(LcError::Conflict(v)) => {
                    assert_eq!(v["reason"], "issue already queued");
                    assert_eq!(v["issue_number"], 42);
                }
                other => panic!("expected a conflict for a {state} #42, got {other:?}"),
            }
        }
        // A duplicate inside one request is still a conflict, whatever the queue holds.
        set_state("done").await;
        let t = load(&e.app, &tid).await.unwrap();
        match check_add_issues(&e.app, &t, &[42, 42]).await {
            Err(LcError::Conflict(v)) => assert_eq!(v["issue_number"], 42),
            other => panic!("expected a conflict for [42, 42], got {other:?}"),
        }
    }

    /// The second pass appends a row instead of touching the first, and takes its own branch:
    /// the first pass's `team/i42-<tid6>` is still in the repo (§6.5 never deletes branches),
    /// so `checkout -b` on the same name would fail.
    #[tokio::test]
    async fn a_second_pass_appends_a_row_with_its_own_branch() {
        let e = env().await;
        let tid = make_team(&e.app, &e.project_id, req(Some(1), false)).await;
        sqlx::query("UPDATE team_issues SET state='failed', fail_reason='merge_conflict' WHERE team_id=?")
            .bind(&tid)
            .execute(&e.app.db)
            .await
            .unwrap();

        add_resolved_issues(&e.app, &tid, vec![issue()], false).await.unwrap();
        let queue = db::team_issues(&e.app.db, &tid).await.unwrap();
        assert_eq!(queue.len(), 2, "the failed pass is kept as the record");
        assert_eq!((queue[0].seq, queue[0].state.as_str()), (1, "failed"));
        assert_eq!((queue[1].seq, queue[1].issue_number, queue[1].state.as_str()), (2, 42, "queued"));

        let t6 = tid6(&tid);
        assert_eq!(issue_branch(&e.app, &tid, &queue[0]).await.unwrap(), format!("team/i42-{t6}"));
        assert_eq!(issue_branch(&e.app, &tid, &queue[1]).await.unwrap(), format!("team/i42-{t6}-r2"));

        // The partial unique index is the backstop: two *open* rows on one number stay
        // impossible even if a caller bypasses `check_add_issues`.
        let dup = sqlx::query(
            "INSERT INTO team_issues (id, team_id, seq, issue_number, issue_title, issue_url, state, created_at)
             VALUES (?,?,3,42,'t','u','queued',?)",
        )
        .bind(db::ulid())
        .bind(&tid)
        .bind(db::now())
        .execute(&e.app.db)
        .await;
        assert!(dup.is_err(), "team_issues_number_open still forbids a second live #42");
    }

    /// §2.5.2: `reopen` is an auditable phase transition, not a paused state, and it clears
    /// the stale terminal timestamp so the team can be resumed again later.
    #[tokio::test]
    async fn reopen_phase_clears_terminal_markers() {
        let e = env().await;
        let tid = make_team(&e.app, &e.project_id, req(Some(1), false)).await;
        set_phase(&e.app, &tid, "done", None, None).await.unwrap();
        assert!(load(&e.app, &tid).await.unwrap().ended_at.is_some());
        let reopened = set_phase(&e.app, &tid, "starting", Some("reopen"), None).await.unwrap();
        assert_eq!(reopened.phase, "starting");
        assert!(reopened.pause_reason.is_none());
        assert!(reopened.resume_phase.is_none());
        assert!(reopened.ended_at.is_none());
        let evs = events(&e.app, &tid, None, 100).await.unwrap();
        let payload = evs["events"].as_array().unwrap().last().unwrap()["payload"].clone();
        assert_eq!(payload["from"], "done");
        assert_eq!(payload["to"], "starting");
        assert_eq!(payload["reason"], "reopen");
    }

    /// §10.5 pause / resume / approve, including the `gate:` restriction.
    #[tokio::test]
    async fn pause_resume_and_approve() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        set_phase(&app, &tid, "working", None, None).await.unwrap();

        assert!(resume(&app, &tid).await.is_err(), "resuming a running team is a conflict");
        pause(&app, &tid).await.unwrap();
        let t = load(&app, &tid).await.unwrap();
        assert_eq!(t.phase, "paused");
        assert_eq!(t.resume_phase.as_deref(), Some("working"));
        pause(&app, &tid).await.unwrap(); // idempotent
        assert_eq!(load(&app, &tid).await.unwrap().resume_phase.as_deref(), Some("working"));

        // `approve` only releases a supervised gate.
        assert!(approve(&app, &tid).await.is_err());
        resume(&app, &tid).await.unwrap();
        let t = load(&app, &tid).await.unwrap();
        assert_eq!(t.phase, "working");
        assert!(t.pause_reason.is_none() && t.resume_phase.is_none());

        set_phase(&app, &tid, "paused", Some("gate:merge"), Some("working")).await.unwrap();
        approve(&app, &tid).await.unwrap();
        assert_eq!(load(&app, &tid).await.unwrap().phase, "working");
    }

    /// §10.7: closing the issue is a human action on a finished team, and nothing else.
    ///
    /// Both guards run *before* the `gh` call, which is what makes them testable here — a
    /// test box has no GitHub, so a guard that leaked through would fail on the network
    /// rather than on the rule it is supposed to enforce.
    #[tokio::test]
    async fn the_issue_can_only_be_closed_once_and_only_when_the_team_is_done() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;

        // Still `starting`: the work is not done, so there is nothing to close.
        match close_issue(&app, &tid, None).await {
            Err(LcError::Conflict(v)) => assert_eq!(v["phase"], "starting"),
            other => panic!("expected a conflict, got {other:?}"),
        }
        // Aborted is terminal but not successful — still refused.
        set_phase(&app, &tid, "aborted", None, None).await.unwrap();
        assert!(matches!(close_issue(&app, &tid, None).await, Err(LcError::Conflict(_))));

        // Done, but already closed from here once: refused rather than commented on twice.
        let closed_at = db::now();
        sqlx::query("UPDATE team_issues SET state='done', issue_closed_at=? WHERE team_id=?")
            .bind(&closed_at)
            .bind(&tid)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE teams SET phase='done', issue_closed_at=? WHERE id=?")
            .bind(&closed_at)
            .bind(&tid)
            .execute(&app.db)
            .await
            .unwrap();
        match close_issue(&app, &tid, None).await {
            Err(LcError::Conflict(v)) => assert!(v["closed_at"].is_string()),
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    /// §10.7 / §2.5: after a reopen the queue id, rather than the team's scalar mirror, selects
    /// the issue to close. A queued continuation must still be refused before any GitHub call;
    /// the finished row's own branch and summary are what the default comment would use.
    #[tokio::test]
    async fn close_issue_after_reopen_targets_the_finished_issue() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team_with(&app, &pid, req(Some(1), false), vec![issue(), issue2()]).await;
        let queue = db::team_issues(&app.db, &tid).await.unwrap();
        let finished_id = queue[0].id.clone();
        let queued_id = queue[1].id.clone();
        let finished_at = db::now();
        sqlx::query(
            "UPDATE team_issues SET state='done', branch='team/i42-finished', base_sha='1234567890abcdef',
             summary='第一個 issue 的完成摘要', pr_url='https://github.com/o/r/pull/42', ended_at=? WHERE id=?",
        )
        .bind(&finished_at)
        .bind(&finished_id)
        .execute(&app.db)
        .await
        .unwrap();
        set_phase(&app, &tid, "done", None, None).await.unwrap();
        let reopened = set_phase(&app, &tid, "starting", Some("reopen"), None).await.unwrap();

        match close_issue_for(&app, &tid, Some(&queued_id), None).await {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v["error"], "conflict");
                assert_eq!(v["state"], "queued");
                assert_eq!(v["phase"], "starting");
            }
            other => panic!("expected the queued continuation to be refused, got {other:?}"),
        }

        // §2.5.4 / §2.5.6 #7: the "already closed from here" guard reads the queue row, not
        // `teams.issue_closed_at` — which `start_issue` clears on every new issue, and which
        // therefore cannot remember that #42 was closed.
        sqlx::query("UPDATE team_issues SET issue_closed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&finished_id)
            .execute(&app.db)
            .await
            .unwrap();
        assert!(load(&app, &tid).await.unwrap().issue_closed_at.is_none(), "the mirror was never stamped");
        match close_issue_for(&app, &tid, Some(&finished_id), None).await {
            Err(LcError::Conflict(v)) => assert!(v["closed_at"].is_string(), "{v}"),
            other => panic!("expected the second close to be refused, got {other:?}"),
        }

        let finished = db::team_issue(&app.db, &finished_id).await.unwrap().unwrap();
        let comment = close_comment_for_issue(&reopened, &finished, &[]);
        assert!(comment.contains("第一個 issue 的完成摘要"), "the finished row supplies the summary: {comment}");
        assert!(comment.contains("team/i42-finished"), "the finished row supplies the branch: {comment}");
        assert!(comment.contains("https://github.com/o/r/pull/42"), "the finished row supplies the PR: {comment}");

        // A historical row with no summary or PR must not inherit either field from the
        // reopened team's current mirror.
        let mut current_mirror = reopened.clone();
        current_mirror.summary = Some("第二個 issue 的摘要".into());
        current_mirror.pr_url = Some("https://github.com/o/r/pull/57".into());
        let mut sparse_finished = finished.clone();
        sparse_finished.summary = None;
        sparse_finished.pr_url = None;
        let comment = close_comment_for_issue(&current_mirror, &sparse_finished, &[]);
        assert!(!comment.contains("第二個 issue 的摘要"), "the current summary leaked: {comment}");
        assert!(!comment.contains("https://github.com/o/r/pull/57"), "the current PR leaked: {comment}");
    }

    /// §10.7: what the comment says. The `deliver=branch` caveat is the load-bearing part —
    /// closing an issue whose fix never reached the base branch is exactly when a future
    /// reader needs to be told where the code actually is.
    #[tokio::test]
    async fn the_close_comment_names_the_branch_the_shas_and_the_caveat() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        let worker = db::team_members(&app.db, &tid).await.unwrap().remove(1);
        sqlx::query(
            "INSERT INTO team_tasks (id, team_id, seq, title, brief, worker_bot_id, branch, state, merge_sha, created_at, updated_at)
             VALUES (?,?,1,'rowid 分頁','b',?,'br','merged','8b1d50809af62183819b35f6ec646b63f2131b6f',?,?)",
        )
        .bind(db::ulid())
        .bind(&tid)
        .bind(&worker.id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query("UPDATE teams SET summary='把排序換成 rowid' WHERE id=?")
            .bind(&tid)
            .execute(&app.db)
            .await
            .unwrap();

        let t = load(&app, &tid).await.unwrap();
        let tasks = db::team_tasks(&app.db, &tid).await.unwrap();
        let c = close_comment(&t, &tasks);
        assert!(c.contains("把排序換成 rowid"), "the PM's summary leads: {c}");
        assert!(c.contains(&t.branch), "the integration branch is named: {c}");
        assert!(c.contains("8b1d5080") && c.contains("rowid 分頁"), "merged tasks are listed: {c}");
        assert!(c.contains("還沒有合併進預設分支"), "`HEAD` is not a name a reader knows: {c}");
        assert!(!c.contains("`HEAD`"), "and it is not printed raw either: {c}");
        sqlx::query("UPDATE teams SET base_ref='main' WHERE id=?").bind(&tid).execute(&app.db).await.unwrap();
        let named = close_comment(&load(&app, &tid).await.unwrap(), &tasks);
        assert!(named.contains("還沒有合併進 `main`"), "a real base branch is named: {named}");

        // A PR replaces the caveat with the link — the work *is* somewhere a reader can go.
        sqlx::query("UPDATE teams SET pr_url='https://github.com/o/r/pull/9' WHERE id=?")
            .bind(&tid)
            .execute(&app.db)
            .await
            .unwrap();
        let c = close_comment(&load(&app, &tid).await.unwrap(), &tasks);
        assert!(c.contains("https://github.com/o/r/pull/9"), "the PR is linked: {c}");
        assert!(!c.contains("還沒有合併進"), "and the caveat is gone: {c}");
    }

    /// §10.5: `member_lost` will not resume while a member is still down.
    #[tokio::test]
    async fn resume_refuses_while_a_member_is_lost() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        set_phase(&app, &tid, "paused", Some("member_lost:dev-1"), Some("working")).await.unwrap();
        match resume(&app, &tid).await {
            Err(LcError::Conflict(v)) => assert_eq!(v["reason"], "member not running"),
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    /// §10.5 abort → aborted (terminal), then cleanup soft-deletes the members.
    /// §12 #6: cleanup never happens on its own.
    #[tokio::test]
    async fn abort_then_cleanup() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), true)).await;

        assert!(cleanup(&app, &tid).await.is_err(), "cleanup needs a terminal team");
        abort(&app, &tid, Some("changed my mind")).await.unwrap();
        let t = load(&app, &tid).await.unwrap();
        assert_eq!(t.phase, "aborted");
        assert!(t.ended_at.is_some());
        assert!(is_terminal(&t.phase));
        assert!(abort(&app, &tid, None).await.is_err(), "aborting twice is a conflict");
        assert!(patch(&app, &tid, PatchTeam::default()).await.is_err(), "a finished team cannot be patched");
        // Members are still there until the user asks for a cleanup.
        assert!(db::team_members(&app.db, &tid).await.unwrap().iter().all(|m| m.deleted_at.is_none()));

        cleanup(&app, &tid).await.unwrap();
        let members = db::team_members(&app.db, &tid).await.unwrap();
        assert_eq!(members.len(), 3);
        assert!(members.iter().all(|m| m.deleted_at.is_some()), "cleanup soft-deletes every member");
        // The team row and its log survive (§6.5: the history stays).
        assert!(db::team(&app.db, &tid).await.unwrap().is_some());
    }

    /// §10.5 PATCH: top up the budget, flip supervised, switch the delivery mode.
    #[tokio::test]
    async fn patch_merges_budget_and_validates_deliver() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        patch(
            &app,
            &tid,
            PatchTeam {
                budget: Some(BudgetPatch { max_relays: Some(80), ..Default::default() }),
                supervised: Some(true),
                deliver: Some("pr".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let t = load(&app, &tid).await.unwrap();
        let b = Budget::from_json(&t.budget_json);
        assert_eq!(b.max_relays, 80);
        assert_eq!(b.max_review_rounds, 2, "the other fields are untouched");
        assert_eq!(t.supervised, 1);
        assert_eq!(t.deliver, "pr");
        assert!(patch(&app, &tid, PatchTeam { deliver: Some("push".into()), ..Default::default() }).await.is_err());
    }

    /// §10.5: the three roles are editable, and only the fields that were sent move.
    #[tokio::test]
    async fn patch_edits_each_role_without_touching_the_others() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), true)).await;
        patch(
            &app,
            &tid,
            PatchTeam {
                pm: Some(RolePatch { model: Some(Some("gpt-5.6-luna".into())), ..Default::default() }),
                reviewer: Some(RolePatch { effort: Some(Some("low".into())), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let t = load(&app, &tid).await.unwrap();
        let roles: Value = serde_json::from_str(&t.roles_json).unwrap();
        assert_eq!(roles["pm"]["model"], "gpt-5.6-luna");
        assert_eq!(roles["pm"]["kind"], "codex", "kind is untouched when it is not sent");
        assert_eq!(roles["reviewer"]["effort"], "low");
        assert_eq!(roles["workers"]["spec"]["kind"], "claude", "the other roles do not move");

        let members = db::team_members(&app.db, &tid).await.unwrap();
        let pm = members.iter().find(|b| b.team_role.as_deref() == Some("pm")).unwrap();
        assert_eq!(pm.model.as_deref(), Some("gpt-5.6-luna"), "the live member follows the spec");
        assert!(members.iter().all(|b| b.deleted_at.is_none()), "nothing is swapped without a kind change");
    }

    /// §7.6: a member's CLI is fixed when its pane starts, so a new `kind` is a new bot —
    /// same name, same cwd, same seat, and the unfinished work moves with it.
    #[tokio::test]
    async fn patch_changes_worker_kind_swaps_bot() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        let old = db::team_members(&app.db, &tid)
            .await
            .unwrap()
            .into_iter()
            .find(|b| b.team_role.as_deref() == Some("worker"))
            .unwrap();
        assert_eq!(old.kind, "claude");
        // #61: the retired bot's hook material must not outlive it.
        let old_dir = app.bot_dir(&old.id).unwrap();
        std::fs::create_dir_all(old_dir.join("bin")).unwrap();
        std::fs::write(old_dir.join("bin").join("herdr"), "shim").unwrap();
        let task_id = db::ulid();
        sqlx::query(
            "INSERT INTO team_tasks (id, team_id, seq, title, brief, worker_bot_id, branch, state, created_at, updated_at)
             VALUES (?,?,1,'t','b',?,'br','working',?,?)",
        )
        .bind(&task_id)
        .bind(&tid)
        .bind(&old.id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        // A second `dispatch{to:"dev-1"}` queued behind it, and a rescue pinned to the seat.
        let queued_id = db::ulid();
        sqlx::query(
            "INSERT INTO team_tasks (id, team_id, seq, title, brief, want_worker_bot_id, branch, state, created_at, updated_at)
             VALUES (?,?,2,'t2','b',?,'br2','queued',?,?)",
        )
        .bind(&queued_id)
        .bind(&tid)
        .bind(&old.id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query("UPDATE teams SET rescue_bot_id = ? WHERE id = ?").bind(&old.id).bind(&tid).execute(&app.db).await.unwrap();

        patch(
            &app,
            &tid,
            PatchTeam {
                workers: Some(RolePatch { kind: Some("grok".into()), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let members = db::team_members(&app.db, &tid).await.unwrap();
        let gone = members.iter().find(|b| b.id == old.id).unwrap();
        assert!(gone.deleted_at.is_some(), "the old bot is retired, and its messages stay with it");
        assert!(!old_dir.exists(), "#61: swap_member left the retired bot's bots/<id>/ directory behind");
        let new = members
            .iter()
            .find(|b| b.deleted_at.is_none() && b.team_role.as_deref() == Some("worker"))
            .expect("a replacement worker");
        assert_eq!(new.kind, "grok");
        assert_eq!(new.name, old.name, "the seat keeps its name — the protocol routes by it");
        assert_eq!(new.cwd, old.cwd, "and its worktree");
        let owner: String = sqlx::query_scalar("SELECT worker_bot_id FROM team_tasks WHERE id = ?")
            .bind(&task_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(owner, new.id, "the unfinished task follows the seat");
        let want: String = sqlx::query_scalar("SELECT want_worker_bot_id FROM team_tasks WHERE id = ?")
            .bind(&queued_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(want, new.id, "a queued task addressed to the seat by name follows it too");
        let t = load(&app, &tid).await.unwrap();
        assert_eq!(t.rescue_bot_id.as_deref(), Some(new.id.as_str()), "so does a rescue pinned to it");
        assert_eq!(serde_json::from_str::<Value>(&t.roles_json).unwrap()["workers"]["spec"]["kind"], "grok");
    }

    /// #61: a team whose creation fails is rolled back — and the members it had already
    /// written must not leave their `bots/<id>/` directories behind.
    #[tokio::test]
    async fn rollback_create_removes_the_bot_dirs_it_soft_deletes() {
        let e = env().await;
        let app = e.app.clone();
        let project = db::project(&app.db, &e.project_id).await.unwrap().unwrap();
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,'t-dev-1','claude','[]',0,1,'tok','team',?)",
        )
        .bind(&bot)
        .bind(&e.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let dir = app.bot_dir(&bot).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("herdr"), "shim").unwrap();

        // i49 之後多了整合分支的參數；這個測試不測分支清理，branch_created=false 直接跳過那段。
        rollback_create(&app, &project, &project.path, &db::ulid(), &[bot.clone()], "", &[], None, "", "", false).await;

        assert!(db::bot(&app.db, &bot).await.unwrap().unwrap().deleted_at.is_some());
        assert!(!dir.exists(), "#61: rollback_create left the soft-deleted bot's directory behind");
    }

    /// #61: the startup sweep removes only the directories of soft-deleted bots. A live bot's
    /// directory and one no bot row claims are left exactly as they are.
    #[tokio::test]
    async fn the_startup_sweep_removes_only_deleted_bots_dirs() {
        let e = env().await;
        let app = e.app.clone();
        let mk = |name: &str, deleted: bool| {
            let app = app.clone();
            let pid = e.project_id.clone();
            let name = name.to_string();
            async move {
                let id = db::ulid();
                sqlx::query(
                    "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at, deleted_at)
                     VALUES (?,?,?,'claude','[]',0,1,'tok','team',?,?)",
                )
                .bind(&id)
                .bind(&pid)
                .bind(&name)
                .bind(db::now())
                .bind(deleted.then(db::now))
                .execute(&app.db)
                .await
                .unwrap();
                std::fs::create_dir_all(app.bot_dir(&id).unwrap().join("bin")).unwrap();
                id
            }
        };
        let gone = mk("t-dev-old", true).await;
        let live = mk("t-dev-new", false).await;
        let stranger = db::ulid();
        std::fs::create_dir_all(app.bot_dir(&stranger).unwrap().join("bin")).unwrap();

        let removed = crate::lifecycle::purge_deleted_bot_dirs(&app).await;

        assert_eq!(removed, 1);
        assert!(!app.bot_dir(&gone).unwrap().exists(), "a deleted bot's directory is removed");
        assert!(app.bot_dir(&live).unwrap().exists(), "a live bot's directory is kept");
        assert!(app.bot_dir(&stranger).unwrap().exists(), "a directory no bot row claims is not ours to judge");
    }

    /// A swap in the middle of a turn would throw away the reply the team is waiting for.
    #[tokio::test]
    async fn patch_refuses_a_kind_swap_while_the_member_is_mid_turn() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        let worker = db::team_members(&app.db, &tid)
            .await
            .unwrap()
            .into_iter()
            .find(|b| b.team_role.as_deref() == Some("worker"))
            .unwrap();
        let run_id = fake_run(&app, &worker.id).await;
        let conv = db::conversation_id(&app.db, &worker.id).await.unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, created_at) VALUES (?,?,?,'web','in_flight',?)",
        )
        .bind(db::ulid())
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let err = patch(
            &app,
            &tid,
            PatchTeam {
                workers: Some(RolePatch { kind: Some("grok".into()), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LcError::Conflict(ref v) if v["reason"] == "member busy"), "{err:?}");
        let members = db::team_members(&app.db, &tid).await.unwrap();
        assert!(members.iter().all(|b| b.deleted_at.is_none()), "and nothing is retired on the way out");
    }

    /// A team built without a reviewer has no spec to edit — adding one mid-run would need a
    /// worktree and a startup, which is not what a PATCH does.
    #[tokio::test]
    async fn patch_reviewer_on_a_team_that_has_none_is_a_400() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        let err = patch(
            &app,
            &tid,
            PatchTeam {
                reviewer: Some(RolePatch { model: Some(Some("sonnet".into())), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "{err:?}");
    }

    /// §10.2 / §10.3 shapes, and §10.4's backwards pagination.
    #[tokio::test]
    async fn state_detail_and_event_pagination() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(2), true)).await;

        let teams = teams_json_for_project(&app, &pid).await;
        assert_eq!(teams.len(), 1);
        let t = &teams[0];
        assert_eq!(t["id"], tid.as_str());
        assert_eq!(t["phase"], "starting");
        assert_eq!(t["deliver"], "branch");
        assert_eq!(t["supervised"], false);
        assert_eq!(t["members"].as_array().unwrap().len(), 4);
        assert_eq!(t["members"][1]["short"], "dev-1");
        assert_eq!(t["tasks_summary"]["total"], 0);
        assert_eq!(t["budget"]["max_relays"], 40);
        assert_eq!(t["usage"]["relays"], 0);

        let d = detail(&app, &tid).await.unwrap();
        assert_eq!(d["tasks"], json!([]));
        assert_eq!(d["base_ref"], "HEAD");
        assert_eq!(d["roles"]["workers"]["count"], 2);
        assert_eq!(d["roles"]["reviewer"]["kind"], "grok");
        assert!(matches!(detail(&app, "nope").await, Err(LcError::NotFound(_))));

        // The bot's `team` field on `GET /state`.
        let member = db::team_members(&app.db, &tid).await.unwrap().remove(0);
        assert_eq!(bot_team_json(&member), json!({"team_id": tid, "role": "pm"}));

        for i in 0..5 {
            record_event(&app, &tid, "note", None, None, None, None, json!({"n": i})).await.unwrap();
        }
        let page = events(&app, &tid, None, 2).await.unwrap();
        let evs = page["events"].as_array().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(page["has_more"], true);
        assert_eq!(evs[0]["payload"]["n"], 3, "a page is oldest-first inside itself");
        assert_eq!(evs[1]["payload"]["n"], 4);
        let older = events(&app, &tid, evs[0]["id"].as_str(), 100).await.unwrap();
        assert_eq!(older["has_more"], false);
        assert_eq!(older["events"].as_array().unwrap().len(), 4, "3 notes + the creation event");
    }

    /// §2.6 rescue: a finished team's unresolved tasks become **one** task for **one** member.
    #[tokio::test]
    async fn rescue_hands_every_unresolved_task_to_one_member() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), true)).await;
        let members = db::team_members(&app.db, &tid).await.unwrap();
        let reviewer = members.iter().find(|b| b.team_role.as_deref() == Some("reviewer")).unwrap().clone();
        let pm = members.iter().find(|b| b.team_role.as_deref() == Some("pm")).unwrap().clone();
        let issue = db::team_issues(&app.db, &tid).await.unwrap().remove(0);
        let mut ids = Vec::new();
        for (seq, state) in [(1, "merged"), (2, "failed"), (3, "skipped")] {
            let id = db::ulid();
            sqlx::query(
                "INSERT INTO team_tasks (id, team_id, issue_id, seq, title, brief, files_json, branch, state, round, created_at, updated_at)
                 VALUES (?,?,?,?,?,?,'[\"a.rs\"]','br',?,0,?,?)",
            )
            .bind(&id).bind(&tid).bind(&issue.id).bind(seq)
            .bind(format!("t{seq}")).bind(format!("做 t{seq}")).bind(state)
            .bind(db::now()).bind(db::now())
            .execute(&app.db).await.unwrap();
            ids.push(id);
        }

        // A team still running has nothing to rescue — that is what `decide` is for.
        assert!(matches!(rescue(&app, &tid, None).await, Err(LcError::Conflict(_))));
        set_phase(&app, &tid, "done", None, None).await.unwrap();
        // The PM may not carry it: its cwd is the integration worktree.
        assert!(matches!(rescue(&app, &tid, Some(&pm.id)).await, Err(LcError::Conflict(_))));
        assert!(matches!(rescue(&app, &tid, Some("nobody")).await, Err(LcError::Conflict(_))));

        let out = rescue(&app, &tid, None).await.unwrap();
        assert_eq!(out["bot_id"], reviewer.id, "the reviewer is the default rescuer");
        assert_eq!(out["rescued"], 2, "only failed / skipped");
        let tasks = db::team_tasks(&app.db, &tid).await.unwrap();
        let new: Vec<&db::TeamTask> = tasks.iter().filter(|t| !ids.contains(&t.id)).collect();
        assert_eq!(new.len(), 1, "one task, not one per failure");
        let r = new[0];
        assert_eq!((r.state.as_str(), r.seq), ("queued", 4));
        assert_eq!(r.want_worker_bot_id.as_deref(), Some(reviewer.id.as_str()));
        assert!(r.worker_bot_id.is_none(), "the executor is picked by the scheduler, one at a time");
        assert!(r.brief.contains("t2") && r.brief.contains("t3") && !r.brief.contains("t1「"));
        // The team is back on the queue with the rescuer recorded.
        let t = load(&app, &tid).await.unwrap();
        assert_eq!((t.phase.as_str(), t.rescue_bot_id.as_deref()), ("starting", Some(reviewer.id.as_str())));
        assert!(t.ended_at.is_none());
        assert_eq!(db::team_issues(&app.db, &tid).await.unwrap()[0].state, "working");

        // Nothing left unresolved → the second call has nothing to do.
        sqlx::query("UPDATE team_tasks SET state='merged' WHERE state IN ('failed','skipped')")
            .execute(&app.db).await.unwrap();
        set_phase(&app, &tid, "done", None, None).await.unwrap();
        assert!(matches!(rescue(&app, &tid, None).await, Err(LcError::Conflict(_))));
    }

    /// §2.6b: which queue entries come back — the latest attempt of each number, and only when
    /// it failed or was skipped. (The re-queue itself is `add_issues`, which needs GitHub.)
    #[tokio::test]
    async fn retry_failed_issues_picks_the_latest_stuck_attempt() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team_with(&app, &pid, req(Some(1), false), vec![issue(), issue2()]).await;
        let rows = db::team_issues(&app.db, &tid).await.unwrap();
        // #42 delivered, #43 failed on a merge conflict.
        for (i, state) in rows.iter().zip(["done", "failed"]) {
            sqlx::query("UPDATE team_issues SET state = ?, fail_reason = ?, ended_at = ? WHERE id = ?")
                .bind(state)
                .bind(if state == "done" { None } else { Some("merge_conflict") })
                .bind(db::now())
                .bind(&i.id)
                .execute(&app.db)
                .await
                .unwrap();
        }
        let all = db::team_issues(&app.db, &tid).await.unwrap();
        assert_eq!(stuck_issues(&all).iter().map(|i| i.issue_number).collect::<Vec<_>>(), vec![43]);

        // #43 queued again and delivered: the number is finished, so nothing is stuck any more.
        sqlx::query(
            "INSERT INTO team_issues (id, team_id, seq, issue_number, issue_title, issue_url, state, created_at)
             VALUES (?,?,?,43,'and then this','u','done',?)",
        )
        .bind(db::ulid()).bind(&tid).bind(3i64).bind(db::now())
        .execute(&app.db).await.unwrap();
        let all = db::team_issues(&app.db, &tid).await.unwrap();
        assert!(stuck_issues(&all).is_empty(), "the retry delivered it");
        set_phase(&app, &tid, "done", None, None).await.unwrap();
        // …and the endpoint refuses before it ever calls GitHub.
        assert!(matches!(retry_failed_issues(&app, &tid).await, Err(LcError::Conflict(_))));
    }

    /// §10.5 decide: only tasks that are actually stuck, and only the three actions.
    #[tokio::test]
    async fn decide_moves_a_stuck_task() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        let worker = db::team_members(&app.db, &tid).await.unwrap().remove(1);
        let task_id = db::ulid();
        sqlx::query(
            "INSERT INTO team_tasks (id, team_id, seq, title, brief, worker_bot_id, branch, state, round, created_at, updated_at)
             VALUES (?,?,1,'t','b',?,'br','working',2,?,?)",
        )
        .bind(&task_id)
        .bind(&tid)
        .bind(&worker.id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        // `working` is not a decidable state.
        assert!(matches!(decide(&app, &tid, &task_id, "rework", None).await, Err(LcError::Conflict(_))));
        sqlx::query("UPDATE team_tasks SET state='exhausted' WHERE id=?").bind(&task_id).execute(&app.db).await.unwrap();
        assert!(matches!(decide(&app, &tid, &task_id, "burn", None).await, Err(LcError::Bad(_))));
        let out = decide(&app, &tid, &task_id, "rework", Some("one more round")).await.unwrap();
        assert_eq!(out["task"]["state"], "working");
        assert_eq!(db::team_task(&app.db, &task_id).await.unwrap().unwrap().state, "working");
        assert!(matches!(decide(&app, &tid, "no-such-task", "skip", None).await, Err(LcError::NotFound(_))));

        // force_merge / skip
        sqlx::query("UPDATE team_tasks SET state='blocked_by_worker' WHERE id=?").bind(&task_id).execute(&app.db).await.unwrap();
        assert_eq!(decide(&app, &tid, &task_id, "skip", None).await.unwrap()["task"]["state"], "skipped");
    }

    /// §10.4 must be deterministic even when a whole scheduler step is written inside one
    /// millisecond — a ULID only has millisecond resolution, so ordering by `id` was a coin
    /// flip. 60 events back to back, then read them page by page and check the log is exactly
    /// the write order, with no gap and no repeat.
    #[tokio::test]
    async fn the_event_log_keeps_its_write_order_within_one_millisecond() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(1), false)).await;
        // The creation event is already there; number the rest from 0.
        const N: i64 = 60;
        let mut ids = Vec::new();
        for i in 0..N {
            ids.push(record_event(&app, &tid, "note", None, None, None, None, json!({"n": i})).await.unwrap());
        }
        // The burst really does put several events in the same millisecond — the case where
        // ULID ordering is a coin flip, and the reason this test exists.
        let mut per_ms: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for e in &ids {
            *per_ms.entry(e.created_at.as_str()).or_default() += 1;
        }
        assert!(
            per_ms.values().any(|n| *n >= 2),
            "expected at least one millisecond with two events; got {per_ms:?}"
        );
        // `seq` is dense and in write order regardless.
        assert_eq!(ids.iter().map(|e| e.seq).collect::<Vec<_>>(), (2..=N + 1).collect::<Vec<_>>());

        // One big page: oldest first, matching the write order exactly.
        let all = events(&app, &tid, None, 500).await.unwrap();
        let got: Vec<i64> = all["events"].as_array().unwrap().iter().skip(1).map(|e| e["payload"]["n"].as_i64().unwrap()).collect();
        assert_eq!(got, (0..N).collect::<Vec<_>>());

        // Page backwards with the `before` cursor and rebuild the same list.
        let mut walked: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = events(&app, &tid, cursor.as_deref(), 7).await.unwrap();
            let evs = page["events"].as_array().unwrap().clone();
            // Each page is oldest-first inside itself.
            let seqs: Vec<i64> = evs.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
            let mut sorted = seqs.clone();
            sorted.sort_unstable();
            assert_eq!(seqs, sorted, "a page is oldest-first inside itself");
            cursor = evs.first().and_then(|e| e["id"].as_str()).map(String::from);
            let mut front = evs;
            front.append(&mut walked);
            walked = front;
            if page["has_more"] != true {
                break;
            }
        }
        let seqs: Vec<i64> = walked.iter().map(|e| e["seq"].as_i64().unwrap()).collect();
        assert_eq!(seqs, (1..=N + 1).collect::<Vec<_>>(), "paging covers every event once, in order");

        // A cursor that is not an event of this team is a 400, not a silently wrong page.
        assert!(matches!(events(&app, &tid, Some("not-an-event"), 10).await, Err(LcError::Bad(_))));
    }

    /// §10.5 say: the recipient can be a role, a short name, a nickname or a bot id; an
    /// unknown one is a 404 and nothing is delivered without a running member.
    #[tokio::test]
    async fn say_resolves_the_recipient() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(Some(2), true)).await;
        let t = load(&app, &tid).await.unwrap();
        let members = db::team_members(&app.db, &tid).await.unwrap();

        let t6 = tid6(&tid);
        for (to, expect) in [
            ("pm", format!("t{t6}-pm")),
            ("dev-2", format!("t{t6}-i1-dev-2")),
            (Box::leak(format!("t{t6}-rev").into_boxed_str()) as &str, format!("t{t6}-rev")),
            ("rev", format!("t{t6}-rev")),
        ] {
            assert_eq!(resolve_member(&app, &t, to).await.unwrap().name, expect, "to={to}");
        }
        assert_eq!(resolve_member(&app, &t, &members[0].id).await.unwrap().id, members[0].id);
        assert_eq!(resolve_member(&app, &t, "@pm").await.unwrap().name, format!("t{t6}-pm"));
        assert!(matches!(resolve_member(&app, &t, "nobody").await, Err(LcError::NotFound(_))));

        assert!(matches!(say(&app, &tid, "  ", "pm", "c1").await, Err(LcError::Bad(_))));
        assert!(matches!(say(&app, &tid, "hi", "nobody", "c1").await, Err(LcError::NotFound(_))));
        // The member is not running, so the ordinary prompt path refuses it.
        assert!(say(&app, &tid, "hi", "pm", "c1").await.is_err());
    }

    /// §6.5a end to end: `branches=delete` on a team whose members are still (apparently)
    /// running must still stop them, remove every row, and never bail out halfway — the
    /// `teams` row disappearing is the one thing this call promises no matter what else
    /// on the way out succeeds or fails.
    #[tokio::test]
    async fn delete_removes_everything_even_with_live_members() {
        let e = env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let tid = make_team(&app, &pid, req(None, true)).await;
        let members = db::team_members(&app.db, &tid).await.unwrap();
        for m in &members {
            let _ = fake_run(&app, &m.id).await;
        }
        delete(&app, &tid, true).await.unwrap();
        assert!(load(&app, &tid).await.is_err(), "the team row is gone");
        assert!(db::team_tasks(&app.db, &tid).await.unwrap().is_empty());
        assert!(events(&app, &tid, None, 10).await.is_err(), "its events are gone with it");
        for m in &members {
            assert!(db::bot(&app.db, &m.id).await.unwrap().unwrap().deleted_at.is_some(), "{} was soft-deleted", m.name);
        }
    }

    /// A hand-made linked worktree of the user's own, so `remove_worktrees` is pointed at
    /// exactly the shape git does **not** protect: `worktree remove --force --force` refuses
    /// a *main* working tree ("fatal: '…' is a main working tree") and nothing else, so a
    /// `bots.cwd` naming one of these would have thrown the user's uncommitted work away.
    fn user_worktree(repo: &std::path::Path) -> String {
        let wt = repo.join(".claude/worktrees/foo");
        crate::team_git::testing::run(repo, &["worktree", "add", "--detach", &wt.to_string_lossy(), "HEAD"]);
        std::fs::write(wt.join("mine.txt"), "work the user has not committed\n").unwrap();
        wt.to_string_lossy().to_string()
    }

    /// **The `--force --force` guard.** `dirs` comes straight from `bots.cwd` and never went
    /// through `check_layout`, so cleanup / delete would hand git whatever the column said.
    #[tokio::test]
    async fn cleanup_refuses_to_remove_a_worktree_outside_the_team_root() {
        let e = env().await;
        let app = e.app.clone();
        let project = db::project(&app.db, &e.project_id).await.unwrap().unwrap();
        let mine = user_worktree(&e.repo);
        // A perfectly ordinary team root, so only the *member* paths are in question.
        let root = app.data_dir.join("teams/01ABC");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.to_string_lossy().to_string();

        remove_worktrees(
            &app,
            &project,
            "",
            &root,
            &[mine.clone(), e.repo.to_string_lossy().to_string(), "/tmp/somewhere-else".into()],
        )
        .await;

        assert!(std::path::Path::new(&mine).exists(), "the user's own worktree survived");
        assert_eq!(
            std::fs::read_to_string(std::path::Path::new(&mine).join("mine.txt")).unwrap(),
            "work the user has not committed\n",
        );
        let listed = crate::team_git::testing::run(&e.repo, &["worktree", "list", "--porcelain"]);
        assert!(listed.contains(&mine), "still a registered worktree: {listed}");
        assert!(e.repo.join("README.md").exists(), "the checkout is intact");
        // The legitimate half still happened: the team root itself was removed.
        assert!(!std::path::Path::new(&root).exists(), "the team root was still cleaned up");
    }

    /// **The `rm -rf` guard.** `remove_dir` is `remove_dir_all` locally and `rm -rf` over ssh,
    /// applied to whatever `teams.worktree_root` holds — the single most destructive call in
    /// the feature and, before this, the only one with no check in front of it. The `''`
    /// value the incident actually had is covered by the existing empty-root early return;
    /// these are the values that would have got through it.
    #[tokio::test]
    async fn the_team_root_is_not_deleted_when_it_endangers_the_checkout() {
        let e = env().await;
        let app = e.app.clone();
        let project = db::project(&app.db, &e.project_id).await.unwrap().unwrap();
        let inside = e.repo.join("sub/dir");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("keep.txt"), "keep\n").unwrap();

        for root in [
            e.repo.to_string_lossy().to_string(),          // the checkout itself
            format!("{}/", e.repo.to_string_lossy()),      // …with a trailing slash
            inside.to_string_lossy().to_string(),          // inside it
            e.dir.to_string_lossy().to_string(),           // a parent of it
        ] {
            remove_worktrees(&app, &project, "", &root, &[]).await;
            assert!(e.repo.join("README.md").exists(), "the checkout survived root = `{root}`");
            assert!(inside.join("keep.txt").exists(), "nothing inside it was touched either (root = `{root}`)");
        }

        // …and a root that really is the team's own is still deleted, so the guard did not
        // just turn cleanup off.
        let root = app.data_dir.join("teams/01ABC");
        std::fs::create_dir_all(&root).unwrap();
        remove_worktrees(&app, &project, "", &root.to_string_lossy(), &[]).await;
        assert!(!root.exists(), "a real team root is still removed");
    }
}
