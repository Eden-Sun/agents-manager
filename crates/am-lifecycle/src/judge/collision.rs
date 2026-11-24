//! 撞題提示（issue #557，#264 的 shadow）：派工或開票時，用測過的那題 Noul `same_work`
//! （門檻 0.5）比對同專案其他在跑的工作。機率過門檻就推一則 inbox，寫進 `judge_shadow`。
//! **不擋派工、不關票、不改認領、不按鍵。**
//!
//! 問答走 [`super::ask_noul`]，不另開 HTTP 客戶端。呼叫在背景 task 裡，不在 controller tick
//! 上等（#480）。沒開、專案不在名單、key 讀不到，什麼都不寫；逾時與非 2xx 記一筆 error，不重試。
//!
//! 比對的是 daemon 看得到的在跑工作，不另打 GitHub：
//! - 其他 child 的 `runs.agent_title`，以及同專案、目前有 running run 的頂層 bot 標題
//! - 未結案交辦裡點名的 `#N`，以及 worktree 分支名尾端的 `-gN`／`issue-N`（已認領的票）
//! - child 與 running 頂層 bot 的 worktree 分支（`.git/HEAD`，沒有就用 `.claude/worktrees/<名>`）
//!
//! 被派工那顆自己的卡不比。開票那條的專案看 `[release_triage] project_ids`。
//!
//! 路徑前綴的 `ownership_conflicts` 照舊、不跟這個機率加成一個分數。同一對只問一次。
//! 超過 [`MAX_PAIRS`] 時先問票號或路徑 basename 有重疊的——那只是名額的順序，不是分數。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use serde_json::{json, Value};

use crate::release_triage::ledger::{self, IssueRef, Row};

#[cfg(any(test, feature = "test-hooks"))]
macro_rules! race_gate_type {
    ($vis:vis) => {
        $vis struct AskedRaceGate {
            pub pair: String,
            pub barrier: Arc<tokio::sync::Barrier>,
        }
    };
}
#[cfg(feature = "test-hooks")]
race_gate_type!(pub);
#[cfg(not(feature = "test-hooks"))]
#[cfg(any(test, feature = "test-hooks"))]
race_gate_type!(pub(self));

#[cfg(any(test, feature = "test-hooks"))]
static ASKED_RACE_GATE: std::sync::OnceLock<std::sync::Mutex<Option<AskedRaceGate>>> = std::sync::OnceLock::new();

/// #264 量過的門檻。低於的只記帳本。
pub const SAME_WORK_THRESHOLD: f64 = 0.5;
/// 一次觸發最多問幾對。評估時一張新票對上約 15 張在跑的工作。
const MAX_PAIRS: usize = 15;
const SUMMARY_CHARS: usize = 450;
/// 提示文案要帶上的現實精度（#264：基準率約 7% 時 precision 約 0.68）。
const HINT_NOISE: &str = "離線 precision 約 0.68，大約每三則有一則是雜訊";

/// 還算「有人要開始做的工作」的交辦狀態（#568）：在途的三個，加上等額度（controller 時間到會自己重送）。
/// 其餘——`awaiting_review`（含當場 `dispatch_failed`）、`blocked`、四個終局——都不會再開始，不拿去問、不推提示。
/// 狀態機裡離開這四個之後沒有回頭邊（唯一的回頭邊是 `quota_blocked → queued`，兩端都在裡面），
/// 所以答案回來之後「重讀還在這四個裡」就等於「中間沒有讓它不再開始的轉移」，不另帶世代戳記
/// （`once_out_of_the_startable_states_an_assignment_never_comes_back` 釘著這個性質）。
/// 也刻意不比 `updated_at`：`queued → delivered`、撞額度這種在途推進會動它，但工作照樣要開始。
const STARTABLE: [&str; 4] = ["queued", "delivered", "unknown", "quota_blocked"];

const SAME_WORK_QUESTION: &str = "Before a second agent starts on `candidate`, should a maintainer link or merge it with `existing` rather than let the two run independently?";
const SAME_WORK_FOCUS: &str = "Answer yes only when the two would fix the same underlying cause or would step on each other's change. Sharing a subsystem, a file or vocabulary is not enough.";

struct Card {
    key: String,
    source: &'static str,
    jev_kind: &'static str,
    title: String,
    summary: String,
    touches: Vec<String>,
    issue_ref: Option<String>,
    /// 這張卡是哪顆 bot 的工作。候選自己那顆的卡不拿來比：同一顆 bot 接著做不是撞題。
    bot_id: Option<String>,
}

macro_rules! candidate_type {
    ($vis:vis) => {
        $vis struct Candidate {
            pub key: String,
            pub project_id: Option<String>,
            pub title: String,
            pub summary: String,
            pub touches: Vec<String>,
            pub issue_ref: Option<String>,
            pub bot_id: String,
            pub agent_kind: String,
            pub parent_bot_id: Option<String>,
            pub assignment_id: Option<String>,
            pub live_bots: Vec<String>,
            pub ownership: String,
            pub host_fence: Option<crate::hosts::HostFence>,
        }
    };
}
#[cfg(feature = "test-hooks")]
candidate_type!(pub);
#[cfg(not(feature = "test-hooks"))]
candidate_type!(pub(self));

impl Candidate {
    fn as_card(&self) -> Card {
        Card {
            key: self.key.clone(),
            source: "candidate",
            jev_kind: "work somebody is about to start",
            title: self.title.clone(),
            summary: self.summary.clone(),
            touches: self.touches.clone(),
            issue_ref: self.issue_ref.clone(),
            bot_id: Some(self.bot_id.clone()),
        }
    }
}

/// 派工之後呼叫。關著連 task 都不起。開著也只等 spawn，不等 Jev。
pub async fn schedule_assignment(app: &Arc<impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse + 'static>, assignment_id: &str) {
    if !app.cfg().get().await.judge.enabled {
        return;
    }
    let app = app.clone();
    let assignment_id = assignment_id.to_string();
    tokio::spawn(async move {
        if let Err(e) = check_assignment(&app, &assignment_id).await {
            tracing::debug!(error = %e, %assignment_id, "judge same_work skipped");
        }
    });
}

/// 開票成功之後呼叫（release-triage publish）。關著、或帳本這次沒多新開的票就立刻回來。
/// 看帳本前後的差，不看呼叫端的 `created`：publish 開到一半停（deferred／failed）時，先開出去的那幾張票也要比對（#1386）。
pub async fn hint_after_publish(app: &Arc<impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse + 'static>, before: &[IssueRef], kind: &str, version: &str, _created: usize) {
    if !app.cfg().get().await.judge.enabled {
        return;
    }
    let Ok(Some(after)) = ledger::get(app.db(), kind, version).await else {
        return;
    };
    if !has_new_issue(before, &after) {
        return;
    }
    let app = app.clone();
    let before = before.to_vec();
    tokio::spawn(async move {
        if let Err(e) = check_opened(&app, &before, &after).await {
            tracing::debug!(error = %e, "judge same_work issue hint skipped");
        }
    });
}

/// 這次 publish 之後帳本多了新開的票（留言不算）。
fn has_new_issue(before: &[IssueRef], after: &Row) -> bool {
    let prev: HashSet<i64> = before.iter().map(|i| i.number).collect();
    after.issues.iter().any(|i| !i.comment && !prev.contains(&i.number))
}

/// `POST /api/judge/collision-check`（#1330）：bot 認領票（`agm issue claim`）時問一次撞題。
/// 候選收集、同一題、同一個開關與名單都跟派工那條一樣；只記帳本、推提示，不擋認領、不改認領、不按鍵。
pub struct ClaimAsk {
    /// 呼叫端自己的專案。路由只接受這個專案的問題。
    pub project_id: String,
    /// 呼叫端（認領的那顆 bot）。`child` 對不上這個專案的一顆 bot 時，候選就記在它名下。
    pub caller: String,
    pub issue: Option<u64>,
    pub title: String,
    /// 被派去做的那顆的名字，只做顯示：bot／agent 名稱可以改，不拿來找 bot、不進候選 key。
    pub child: String,
    /// 被派去做的那顆的 bot ID（呼叫端明講才有）。它自己在跑的卡不拿來比：同一顆接著做不是撞題。
    /// 沒有、或不是這個專案的存活 bot：候選記在呼叫端名下（缺證據＝未知，不用名字猜）。
    pub child_bot_id: Option<String>,
    /// 這次認領的穩定 ID（`agm issue claim` 的 `claim_id`）：候選 key 靠它，重送／spool 重放是同一個候選。
    pub claim_id: Option<String>,
    pub branch: Option<String>,
}

/// 路由收到就回 202；這裡只等 spawn，不等 Jev。關著連 task 都不起。
pub async fn schedule_claim(app: &Arc<impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse + 'static>, ask: ClaimAsk) {
    if !app.cfg().get().await.judge.enabled {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = check_claim(&app, &ask).await {
            tracing::debug!(error = %e, "judge same_work claim check skipped");
        }
    });
}

pub async fn check_claim(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse), ask: &ClaimAsk) -> Result<()> {
    if !app.cfg().get().await.judge.enabled {
        return Ok(());
    }
    let child: Option<(String, String, Option<String>)> = match ask.child_bot_id.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => {
            sqlx::query_as("SELECT id, kind, parent_bot_id FROM bots WHERE project_id = ? AND id = ? AND deleted_at IS NULL")
                .bind(&ask.project_id)
                .bind(id)
                .fetch_optional(app.db())
                .await?
        }
        None => None,
    };
    let (bot_id, agent_kind, parent_bot_id) = child.unwrap_or_else(|| (ask.caller.clone(), String::new(), None));
    let mut summary = ask.title.clone();
    if let Some(branch) = &ask.branch {
        summary = format!("{summary}\nbranch {branch}");
    }
    let key = match ask.claim_id.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => format!("claim:{id}"),
        None => format!("claim:{}:{}", ask.caller, ask.issue.map_or_else(|| "-".to_string(), |n| n.to_string())),
    };
    let mut live_bots = vec![ask.caller.clone(), bot_id.clone()];
    live_bots.extend(parent_bot_id.clone());
    let ownership = crate::supervisor_inbox::claim_ownership(app.db(), &live_bots).await?;
    let host_fence = app.claim_host_fence(&ask.caller).await;
    if let (Some(fence), Some(run)) = (&host_fence, crate::db::active_run(app.db(), &ask.caller).await?) {
        if crate::db::run_host_generation(app.db(), &run.id).await?.is_some_and(|was| fence.target_epoch().is_some_and(|now| now != was)) {
            return Ok(());
        }
    }
    check_candidate(
        app,
        &Candidate {
            key,
            project_id: Some(ask.project_id.clone()),
            title: ask.title.clone(),
            summary: summarize(&summary),
            touches: paths_of(&ask.title),
            issue_ref: ask.issue.map(|n| format!("#{n}")),
            bot_id,
            agent_kind,
            parent_bot_id,
            assignment_id: None,
            live_bots,
            ownership,
            host_fence,
        },
    )
    .await
}

pub async fn check_assignment(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse), assignment_id: &str) -> Result<()> {
    if !app.cfg().get().await.judge.enabled {
        return Ok(());
    }
    let Some(a) = crate::supervisor_inbox::assignment(app.db(), assignment_id).await? else {
        return Ok(());
    };
    if a.expects_review == 0 || !STARTABLE.contains(&a.status.as_str()) {
        return Ok(());
    }
    let Some(bot) = crate::db::bot(app.db(), &a.target_bot_id).await? else {
        return Ok(());
    };
    if bot.deleted_at.is_some() {
        return Ok(());
    }
    let numbers = issue_numbers(&a.text);
    check_candidate(
        app,
        &Candidate {
            key: format!("asg:{}", a.id),
            project_id: Some(bot.project_id),
            title: title_of(&a.text),
            summary: summarize(&a.text),
            touches: paths_of(&a.text),
            issue_ref: numbers.first().map(|n| format!("#{n}")),
            bot_id: bot.id,
            agent_kind: bot.kind,
            parent_bot_id: bot.parent_bot_id,
            assignment_id: Some(a.id),
            live_bots: Vec::new(),
            ownership: String::new(),
            host_fence: None,
        },
    )
    .await
}

pub async fn check_opened(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse), before: &[IssueRef], after: &Row) -> Result<()> {
    if !app.cfg().get().await.judge.enabled {
        return Ok(());
    }
    for cand in opened_candidates(before, after) {
        check_candidate(app, &cand).await?;
    }
    Ok(())
}

async fn check_candidate(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse), cand: &Candidate) -> Result<()> {
    let cfg = app.cfg().get().await.judge;
    if !cfg.enabled {
        return Ok(());
    }
    let key = match super::read_key(&cfg.key_file) {
        Ok(k) => k,
        Err(_) => return Ok(()),
    };
    let projects = match &cand.project_id {
        Some(id) => vec![id.clone()],
        None => repo_projects(app).await?,
    };
    let mine = cand.as_card();
    for pid in projects {
        if crate::handoff::project_handed_off_to(app.db(), &pid).await?.is_some() {
            continue;
        }
        let label = crate::db::project(app.db(), &pid).await?.map(|p| p.label).unwrap_or_default();
        if matches!(super::gate(&cfg, &pid, &label, 0), Some(super::Skip::Disabled | super::Skip::ProjectNotListed)) {
            continue;
        }
        let others = running_cards(app, &pid, &cand.key, &cand.bot_id).await?;
        let ranked = rank(&mine, others);
        for other in ranked {
            let pair = format!("{}|{}", cand.key, other.key);
            if asked(app, &pair).await? {
                continue;
            }
            if !cand.live_bots.is_empty() && (!crate::supervisor_inbox::bots_live(app.db(), &cand.live_bots).await?
                || crate::supervisor_inbox::claim_ownership(app.db(), &cand.live_bots).await? != cand.ownership
                || cand.host_fence.as_ref().is_some_and(|f| !f.is_current_now())) {
                return Ok(());
            }
            #[cfg(any(test, feature = "test-hooks"))]
            gate_after_asked_for_test(&pair).await;
            let snapshot = snapshot(&pair, &mine, &other);
            let slot = match super::reserve_same_work_slot(
                app,
                &cfg,
                &pid,
                &label,
                &cand.bot_id,
                &pair,
                &cand.agent_kind,
                &snapshot,
            )
            .await
            {
                Ok(Some(id)) => id,
                Ok(None) => continue,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("Fuse") || msg.contains("Disabled") || msg.contains("ProjectNotListed") {
                        return Ok(());
                    }
                    return Err(e);
                }
            };
            let shown = if label.is_empty() { "project".to_string() } else { super::mask(&label) };
            let body = request_body(&cfg.model, &shown, &mine, &other);
            let started = Instant::now();
            let answer = super::ask_noul(&cfg, &key, &body, "same_work").await;
            let ms = started.elapsed().as_millis() as i64;
            let (p, model, tokens, error) = match &answer {
                Ok(a) => (Some(a.value), a.model.clone(), a.input_tokens, None),
                Err(e) => (None, None, None, Some(e.to_string())),
            };
            if !cand.live_bots.is_empty() && (!crate::supervisor_inbox::bots_live(app.db(), &cand.live_bots).await?
                || crate::supervisor_inbox::claim_ownership(app.db(), &cand.live_bots).await? != cand.ownership
                || cand.host_fence.as_ref().is_some_and(|f| !f.is_current_now())) {
                settle_same_work(app, &slot, None, None, ms, None, Some("bot_gone".into())).await?;
                return Ok(());
            }
            settle_same_work(app, &slot, p, model, ms, tokens, error).await?;
            if let Some(status) = gone(app, cand).await? {
                mark_stale(app, &slot, &status).await?;
                return Ok(());
            }
            let Ok(a) = answer else { continue };
            if a.value >= SAME_WORK_THRESHOLD {
                push_hint(app, cand, &other, a.value, &pair).await?;
            }
        }
    }
    Ok(())
}

async fn settle_same_work(
    app: &impl crate::capabilities::Db,
    id: &str,
    p: Option<f64>,
    model: Option<String>,
    ms: i64,
    tokens: Option<i64>,
    error: Option<String>,
) -> Result<()> {
    sqlx::query("UPDATE judge_shadow SET jev_same_work=?, model=?, ms=?, input_tokens=?, error=? WHERE id=?")
        .bind(p)
        .bind(model)
        .bind(ms)
        .bind(tokens)
        .bind(error)
        .bind(id)
        .execute(app.db())
        .await?;
    Ok(())
}

/// 候選是交辦、而那筆已經不在 [`STARTABLE`] 裡：回它現在的狀態（讀不到＝被刪了，也算）。開票那條沒有交辦，永遠 `None`。
async fn gone(app: &impl crate::capabilities::Db, cand: &Candidate) -> Result<Option<String>> {
    let Some(id) = cand.assignment_id.as_deref() else { return Ok(None) };
    let status = crate::supervisor_inbox::assignment(app.db(), id).await?.map(|a| a.status).unwrap_or_else(|| "missing".into());
    Ok((!STARTABLE.contains(&status.as_str())).then_some(status))
}

/// 帳本上這一對的 JSON 加 `stale`＝答案回來時交辦的狀態。觀察留著（`jev_same_work` 照填），事後算誤報率時要排除它：
/// 候選根本沒開始，「後來有沒有被併掉」對它沒有意義。
async fn mark_stale(app: &impl crate::capabilities::Db, id: &str, status: &str) -> Result<()> {
    sqlx::query("UPDATE judge_shadow SET matched_line = json_set(matched_line, '$.stale', ?) WHERE id=?")
        .bind(status)
        .bind(id)
        .execute(app.db())
        .await?;
    Ok(())
}

async fn push_hint(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), cand: &Candidate, other: &Card, p: f64, pair: &str) -> Result<()> {
    let parent = cand.parent_bot_id.clone().unwrap_or_else(|| cand.bot_id.clone());
    let existing = other.issue_ref.clone().unwrap_or_else(|| other.key.clone());
    let action = format!(
        "{badge}：可能與進行中的工作重疊（same_work {p:.2}：{title}）。這是提示，不是判定：{HINT_NOISE}。不阻擋這次派工、不關票、不改認領、不按鍵。",
        badge = super::badge(Some(p)),
        title = truncate(&other.title, 80),
    );
    let payload = super::mark(json!({
        "assignment_id": cand.assignment_id,
        "parent_bot_id": cand.parent_bot_id,
        "probability": p,
        "threshold": SAME_WORK_THRESHOLD,
        "source": other.source,
        "candidate_ref": cand.key,
        "candidate_title": cand.title,
        "existing_ref": existing,
        "existing_title": other.title,
        "shadow": true,
        "action": action,
    }), Some(p));
    let event_key = format!("judge_same_work:{pair}");
    let id = if cand.live_bots.is_empty() {
        crate::supervisor_inbox::push_inbox(app.db(), &event_key, "judge_same_work", cand.assignment_id.as_deref(), Some(&parent), None, &payload).await?
    } else {
        let write = crate::supervisor_inbox::push_inbox_if_live(
            app.db(),
            &event_key,
            "judge_same_work",
            cand.assignment_id.as_deref(),
            Some(&parent),
            None,
            &payload,
            &cand.live_bots,
            &cand.ownership,
        );
        if let Some(fence) = &cand.host_fence {
            fence.run_current(write).await.transpose()?.flatten()
        } else {
            write.await?
        }
    };
    if id.is_some() {
        app.emit("supervisor_changed", json!({"judge_same_work": event_key})).await;
    }
    Ok(())
}

fn snapshot(pair: &str, cand: &Card, other: &Card) -> String {
    json!({
        "pair": pair,
        "candidate_ref": cand.key,
        "existing_ref": other.key,
        "candidate_title": truncate(&cand.title, 180),
        "existing_title": truncate(&other.title, 180),
        "source": other.source,
    })
    .to_string()
}

async fn asked(app: &impl crate::capabilities::Db, pair: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE regex_verdict = 'same_work' AND run_id = ?")
        .bind(pair)
        .fetch_one(app.db())
        .await?;
    Ok(n > 0)
}

#[cfg(any(test, feature = "test-hooks"))]
async fn gate_after_asked_for_test(pair: &str) {
    let barrier = ASKED_RACE_GATE
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap()
        .as_ref()
        .filter(|gate| gate.pair == pair)
        .map(|gate| gate.barrier.clone());
    if let Some(barrier) = barrier {
        barrier.wait().await;
    }
}

/// release-triage 開的票屬於 `[release_triage] repo`；同專案由 `[release_triage] project_ids` 明講（專案 ID，可多個）。
/// 專案 label 與路徑都會改、也會有同名的別個專案，不拿來猜；沒設、或設的專案已刪，就不問——拿別的專案在跑的工作來比只會是雜訊。
async fn repo_projects(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db)) -> Result<Vec<String>> {
    let wanted = app.cfg().get().await.release_triage.project_ids;
    let mut live = Vec::new();
    for id in wanted {
        if !live.contains(&id) && crate::db::project(app.db(), &id).await?.is_some_and(|p| p.deleted_at.is_none()) {
            live.push(id);
        }
    }
    Ok(live)
}

async fn running_cards(app: &impl crate::capabilities::Db, project_id: &str, skip_key: &str, skip_bot: &str) -> Result<Vec<Card>> {
    let mut cards = Vec::new();
    for a in crate::supervisor_inbox::unsettled_assignments(app.db()).await? {
        if a.expects_review == 0 {
            continue;
        }
        let key = format!("asg:{}", a.id);
        if key == skip_key {
            continue;
        }
        let Some(bot) = crate::db::bot(app.db(), &a.target_bot_id).await? else { continue };
        if bot.deleted_at.is_some() || bot.project_id != project_id || bot.id == skip_bot {
            continue;
        }
        let mut card = assignment_card(&key, &a.text);
        card.bot_id = Some(bot.id);
        cards.push(card);
    }

    let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT b.id, b.cwd, r.agent_title, b.parent_bot_id
           FROM bots b
           JOIN runs r ON r.bot_id = b.id AND r.state = 'running'
          WHERE b.project_id = ? AND b.deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_all(app.db())
    .await?;
    let mut by_bot: HashMap<String, (Option<String>, Option<String>, bool)> = HashMap::new();
    for (id, cwd, title, parent_bot_id) in rows {
        let slot = by_bot.entry(id).or_insert((None, None, parent_bot_id.is_none()));
        if slot.0.is_none() {
            slot.0 = cwd;
        }
        if title.as_ref().is_some_and(|t| !t.trim().is_empty()) {
            slot.1 = title;
        }
    }
    for (id, (cwd, title, top_level)) in by_bot {
        if id == skip_bot {
            continue;
        }
        let (title_source, branch_source, jev_kind) = if top_level {
            ("top_level_title", "top_level_branch", "a same-project top-level bot that is currently running")
        } else {
            ("child_title", "worktree_branch", "an assignment another agent is working on right now")
        };
        if let Some(title) = title {
            let titled = title_of(&title);
            let key = format!("title:{id}");
            if key != skip_key && !titled.is_empty() && !cards.iter().any(|c| c.title == titled) {
                let mut card = text_card(key, title_source, jev_kind, &title);
                card.issue_ref = issue_numbers(&title).first().map(|n| format!("#{n}"));
                card.bot_id = Some(id.clone());
                cards.push(card);
            }
        }
        if let Some(cwd) = cwd {
            if let Some(branch) = branch_of(&cwd) {
                let key = format!("branch:{id}");
                if key != skip_key && !cards.iter().any(|c| c.title == branch) {
                    let mut card = text_card(key, branch_source, jev_kind, &branch);
                    card.summary = summarize(&format!("worktree branch {branch}"));
                    card.issue_ref = issue_numbers(&branch).first().map(|n| format!("#{n}"));
                    card.bot_id = Some(id.clone());
                    cards.push(card);
                }
            }
        }
    }

    cards.retain(|c| c.key != skip_key && !c.title.is_empty());
    Ok(cards)
}

fn assignment_card(key: &str, text: &str) -> Card {
    let mut card = text_card(key.to_string(), "assignment", "an assignment another agent is working on right now", text);
    if let Some(n) = issue_numbers(text).first() {
        card.source = "claimed_issue";
        card.jev_kind = "an issue that is already open";
        card.issue_ref = Some(format!("#{n}"));
    }
    card
}

fn text_card(key: String, source: &'static str, jev_kind: &'static str, raw: &str) -> Card {
    Card {
        key,
        source,
        jev_kind,
        title: title_of(raw),
        summary: summarize(raw),
        touches: paths_of(raw),
        issue_ref: issue_numbers(raw).first().map(|n| format!("#{n}")),
        bot_id: None,
    }
}

fn rank(cand: &Card, mut others: Vec<Card>) -> Vec<Card> {
    let mut issues = issue_numbers(&format!("{} {}", cand.title, cand.summary));
    if let Some(r) = &cand.issue_ref {
        push_num(&mut issues, r.trim_start_matches('#'));
    }
    let bases: HashSet<String> = cand.touches.iter().map(|p| basename(p).to_string()).collect();
    others.sort_by(|a, b| {
        let sa = u8::from(overlaps(a, &issues, &bases));
        let sb = u8::from(overlaps(b, &issues, &bases));
        sb.cmp(&sa).then_with(|| a.key.cmp(&b.key))
    });
    others.truncate(MAX_PAIRS);
    others
}

fn overlaps(card: &Card, issues: &[String], bases: &HashSet<String>) -> bool {
    if let Some(r) = &card.issue_ref {
        let n = r.trim_start_matches('#');
        if issues.iter().any(|i| i == n) {
            return true;
        }
    }
    card.touches.iter().any(|p| bases.contains(basename(p)))
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// 這次 publish 新開出來的票（留言不算）。
fn opened_candidates(before: &[IssueRef], after: &Row) -> Vec<Candidate> {
    let prev: HashSet<i64> = before.iter().map(|i| i.number).collect();
    let proposals: Vec<crate::release_triage::verdict::StoredProposal> = after
        .verdicts
        .as_ref()
        .and_then(|v| v.get("issues"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let mut out = Vec::new();
    for iss in &after.issues {
        if iss.comment || prev.contains(&iss.number) {
            continue;
        }
        let Some(p) = proposals.iter().find(|p| crate::release_triage::issue::marker(&after.kind, &after.version, &p.entry_ids) == iss.marker) else {
            continue;
        };
        let title = crate::release_triage::issue::title(&after.kind, &after.version, p);
        let body = crate::release_triage::issue::render_body(&after.kind, &after.version, &after.entries, p);
        let numbers = issue_numbers(&format!("{title}\n{body}"));
        out.push(Candidate {
            key: format!("issue:{}", iss.number),
            project_id: None,
            title: title_of(&title),
            summary: summarize(&body),
            touches: paths_of(&body),
            issue_ref: numbers.first().map(|n| format!("#{n}")).or_else(|| Some(format!("#{}", iss.number))),
            bot_id: format!("issue:{}", iss.number),
            agent_kind: "issue".into(),
            parent_bot_id: None,
            assignment_id: None,
            live_bots: Vec::new(),
            ownership: String::new(),
            host_fence: None,
        });
    }
    out
}

fn request_body(model: &str, project: &str, cand: &Card, existing: &Card) -> Value {
    json!({
        "model": model,
        "state": {
            "project": project,
            "candidate": {
                "kind": "work somebody is about to start",
                "title": cand.title,
                "summary": cand.summary,
                "touches": cand.touches,
            },
            "existing": {
                "kind": existing.jev_kind,
                "ref": existing.issue_ref.clone().unwrap_or_else(|| "in-flight".into()),
                "title": existing.title,
                "summary": existing.summary,
                "touches": existing.touches,
            }
        },
        "questions": {"same_work": {
            "type": "noul",
            "instructions": {"question": SAME_WORK_QUESTION, "focus": SAME_WORK_FOCUS},
            "criteria": {
                "true": "A maintainer would stop and connect the two items first",
                "false": "The two items can proceed as separate work"
            }
        }}
    })
}

fn title_of(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or(text);
    truncate(&collapse(&super::mask(line)), 180)
}

fn summarize(text: &str) -> String {
    let masked = super::mask(text);
    let mut out = String::new();
    let mut fence = false;
    for line in masked.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        let clean: String = line.chars().filter(|c| !matches!(c, '#' | '*' | '>' | '`' | '|')).collect();
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(clean.trim());
    }
    truncate(&collapse(&out), SUMMARY_CHARS)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn paths_of(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| c.is_whitespace() || matches!(c, '`' | '(' | ')' | '[' | ']' | ',' | ';' | '"' | '\'' | '|' | '<' | '>' | ':' )) {
        let tok = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-');
        if tok.len() < 4 || tok.len() > 200 {
            continue;
        }
        let ext_ok = [".rs", ".ts", ".tsx", ".sh", ".py", ".toml", ".md"].iter().any(|e| tok.ends_with(e));
        if !ext_ok || out.iter().any(|p| p == tok) {
            continue;
        }
        out.push(tok.to_string());
        if out.len() == 8 {
            break;
        }
    }
    out
}

/// `#N`、`issue-N`、分支名尾端的 `-gN`。
fn issue_numbers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '#' {
            if let Some(n) = digits_at(&chars, i + 1) {
                push_num(&mut out, &n);
                i += 1 + n.len();
                continue;
            }
        }
        i += 1;
    }
    for tok in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_') {
        if let Some(n) = tok.strip_prefix("issue-") {
            if is_issue_num(n) {
                push_num(&mut out, n);
            }
        }
        if let Some((_, n)) = tok.rsplit_once("-g") {
            if is_issue_num(n) && tok.ends_with(&format!("-g{n}")) {
                push_num(&mut out, n);
            }
        }
    }
    out
}

fn digits_at(chars: &[char], start: usize) -> Option<String> {
    if start >= chars.len() || !chars[start].is_ascii_digit() {
        return None;
    }
    let mut n = String::new();
    for c in &chars[start..] {
        if c.is_ascii_digit() {
            n.push(*c);
            if n.len() > 6 {
                return None;
            }
        } else if c.is_ascii_alphanumeric() || *c == '_' {
            return None;
        } else {
            break;
        }
    }
    is_issue_num(&n).then_some(n)
}

fn is_issue_num(s: &str) -> bool {
    !s.is_empty() && s.len() <= 6 && s.chars().all(|c| c.is_ascii_digit()) && !s.starts_with('0')
}

fn push_num(out: &mut Vec<String>, n: &str) {
    if !out.iter().any(|e| e == n) {
        out.push(n.to_string());
    }
}

/// `.git/HEAD` 的分支；worktree 的 `.git` 是檔案時跟著 gitdir 走。沒有就用目錄名。
fn branch_of(cwd: &str) -> Option<String> {
    git_head_branch(Path::new(cwd)).or_else(|| worktree_dirname(cwd))
}

fn git_head_branch(cwd: &Path) -> Option<String> {
    let git = cwd.join(".git");
    let head = if git.is_file() {
        let txt = std::fs::read_to_string(&git).ok()?;
        let dir = txt.trim().strip_prefix("gitdir:")?.trim();
        let dir = Path::new(dir);
        let dir = if dir.is_absolute() { dir.to_path_buf() } else { cwd.join(dir) };
        dir.join("HEAD")
    } else if git.is_dir() {
        git.join("HEAD")
    } else {
        None?
    };
    let text = std::fs::read_to_string(head).ok()?;
    let text = text.trim().strip_prefix("ref:").map(str::trim).unwrap_or(text.trim());
    text.strip_prefix("refs/heads/").map(str::to_string).filter(|s| !s.is_empty())
}

fn worktree_dirname(cwd: &str) -> Option<String> {
    let marker = ".claude/worktrees/";
    let rest = cwd.split(marker).nth(1)?;
    let name = rest.split('/').next().filter(|s| !s.is_empty())?;
    Some(name.to_string())
}

#[cfg(any(test, feature = "test-hooks"))]
pub mod test_seams {
    #[cfg(feature = "test-hooks")]
    pub use super::{AskedRaceGate, Candidate};
    pub const HINT_NOISE: &str = super::HINT_NOISE;
    pub const MAX_PAIRS: usize = super::MAX_PAIRS;
    pub const SAME_WORK_FOCUS: &str = super::SAME_WORK_FOCUS;
    pub const SAME_WORK_QUESTION: &str = super::SAME_WORK_QUESTION;
    pub const STARTABLE: [&str; 4] = super::STARTABLE;

    pub fn asked_race_gate() -> &'static std::sync::OnceLock<std::sync::Mutex<Option<super::AskedRaceGate>>> {
        &super::ASKED_RACE_GATE
    }

    pub async fn asked(app: &impl crate::capabilities::Db, pair: &str) -> super::Result<bool> {
        super::asked(app, pair).await
    }

    pub async fn check_candidate(
        app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse),
        cand: &super::Candidate,
    ) -> super::Result<()> {
        super::check_candidate(app, cand).await
    }

    pub fn opened_candidates(before: &[super::IssueRef], after: &super::Row) -> Vec<super::Candidate> {
        super::opened_candidates(before, after)
    }
}

#[cfg(all(test, not(feature = "test-hooks")))]
mod tests {
    use super::*;
    use crate::release_triage::{ledger::Status, Bucket, Entry};

    fn sample_row(number: i64) -> Row {
        let entry = Entry {
            id: "e1".into(),
            text: "The limit line is matched inside a diff".into(),
            bucket: Bucket::Kept,
            categories: vec![],
            rules: vec![],
        };
        let marker = crate::release_triage::issue::marker("claude", "2.1.277", &["e1".into()]);
        Row {
            kind: "claude".into(),
            version: "2.1.277".into(),
            status: Status::Published,
            assigned_bot_id: None,
            dispatch_gen: 0,
            entries: vec![entry],
            verdicts: Some(json!({"issues": [{
                "entry_ids": ["e1"],
                "triage": "adopt",
                "title": "quota banner is parsed inside source",
                "goal": "stop matching the limit line inside printed source in daemon/src/lifecycle/screen.rs",
                "suggestion": "tighten the matcher",
                "acceptance": "a diff containing the line is not a hit"
            }]})),
            issues: vec![IssueRef {
                marker,
                entry_ids: vec!["e1".into()],
                number,
                url: format!("https://example.test/{number}"),
                created_at: "2026-09-25T00:00:00.000Z".into(),
                comment: false,
            }],
            dispatched_at: None,
            attempts: 1,
            publish_error: None,
            created_at: "2026-09-25T00:00:00.000Z".into(),
            updated_at: "2026-09-25T00:00:00.000Z".into(),
        }
    }


    #[test]
    fn issue_numbers_and_branches_follow_the_three_sources() {
        assert_eq!(issue_numbers("實作 #557 與 issue-12"), vec!["557".to_string(), "12".to_string()]);
        assert_eq!(issue_numbers("fix/kd61te-g558"), vec!["558".to_string()]);
        assert!(issue_numbers("#557abc").is_empty(), "號碼後面緊接字母不是票號");
        assert!(issue_numbers("#0123").is_empty(), "前導零不是票號");
        let dir = crate::test_dirs::scratch_dir("am-branch");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/fix/demo-g7\n").unwrap();
        assert_eq!(branch_of(dir.to_str().unwrap()).as_deref(), Some("fix/demo-g7"));
        let nested = dir.join(".claude/worktrees/only-the-dirname");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(branch_of(nested.to_str().unwrap()).as_deref(), Some("only-the-dirname"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #1385：候選的票號只在 `issue_ref`（claim 的票號、交辦的第二行），不在 title／summary 裡：名額排序也要看它，
    /// 不然同一張票的那張卡會被 15 張的上限截掉。
    #[test]
    fn rank_puts_the_card_that_shares_the_candidates_issue_ref_first() {
        let card = |key: &str, issue_ref: Option<&str>| Card {
            key: key.into(),
            source: "candidate",
            jev_kind: "work somebody is about to start",
            title: "修卡住的畫面判斷".into(),
            summary: "修卡住的畫面判斷".into(),
            touches: vec![],
            issue_ref: issue_ref.map(String::from),
            bot_id: None,
        };
        let others = || {
            let mut v: Vec<Card> = (0..16).map(|i| card(&format!("asg:{i:02}"), None)).collect();
            v.push(card("zzz-shared", Some("#77")));
            v
        };
        let cand = card("claim:x:77", Some("#77"));
        let ranked = rank(&cand, others());
        assert_eq!(ranked.len(), MAX_PAIRS);
        assert_eq!(ranked[0].key, "zzz-shared", "候選的票號在 issue_ref，同一張票的卡要排在最前");
        let bare = card("claim:x:77", None);
        assert!(!rank(&bare, others()).iter().any(|c| c.key == "zzz-shared"), "沒有票號就只照 key 排，這張被截掉");
    }

    /// #1386：publish 開到一半停（deferred／failed）時，先開出去的那幾張票也要比對：看帳本前後的差，不看呼叫端數的 `created`。
    #[test]
    fn a_publish_that_stopped_midway_still_counts_its_new_issues() {
        let row = sample_row(404);
        assert!(has_new_issue(&[], &row), "帳本裡多了一張新開的票，就算這次 created 是 0 也要比對");
        assert!(!has_new_issue(&row.issues, &row), "沒有新開的票就不比對");
        let mut only_comment = sample_row(404);
        only_comment.issues[0].comment = true;
        assert!(!has_new_issue(&[], &only_comment), "只留言不算開票");
    }

}
