//! 撞題提示（issue #557，#264 的 shadow）：派工或開票時，用測過的那題 Noul `same_work`
//! （門檻 0.5）比對同專案其他在跑的工作。機率過門檻就推一則 inbox，寫進 `judge_shadow`。
//! **不擋派工、不關票、不改認領、不按鍵。**
//!
//! 問答走 [`super::ask_noul`]，不另開 HTTP 客戶端。呼叫在背景 task 裡，不在 controller tick
//! 上等（#480）。沒開、專案不在名單、key 讀不到，什麼都不寫；逾時與非 2xx 記一筆 error，不重試。
//!
//! 比對的是 daemon 看得到的在跑工作，不另打 GitHub：
//! - 其他 child 的 `runs.agent_title`（父 bot 的孩子，頂層 bot 的標題不算）
//! - 未結案交辦裡點名的 `#N`，以及 worktree 分支名尾端的 `-gN`／`issue-N`（已認領的票）
//! - child 的 worktree 分支（`.git/HEAD`，沒有就用 `.claude/worktrees/<名>`）
//!
//! 被派工那顆自己的卡不比。開票那條的專案看 `[release_triage] repo` 對得上哪個專案。
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
use crate::state::App;

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

struct Candidate {
    key: String,
    project_id: Option<String>,
    title: String,
    summary: String,
    touches: Vec<String>,
    issue_ref: Option<String>,
    bot_id: String,
    agent_kind: String,
    parent_bot_id: Option<String>,
    assignment_id: Option<String>,
}

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
pub async fn schedule_assignment(app: &Arc<App>, assignment_id: &str) {
    if !app.cfg.get().await.judge.enabled {
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

/// 開票成功之後呼叫（release-triage publish）。`created == 0` 或關著就立刻回來。
pub async fn hint_after_publish(app: &Arc<App>, before: &[IssueRef], kind: &str, version: &str, created: usize) {
    if created == 0 || !app.cfg.get().await.judge.enabled {
        return;
    }
    let Ok(Some(after)) = ledger::get(&app.db, kind, version).await else {
        return;
    };
    let app = app.clone();
    let before = before.to_vec();
    tokio::spawn(async move {
        if let Err(e) = check_opened(&app, &before, &after).await {
            tracing::debug!(error = %e, "judge same_work issue hint skipped");
        }
    });
}

pub(crate) async fn check_assignment(app: &Arc<App>, assignment_id: &str) -> Result<()> {
    if !app.cfg.get().await.judge.enabled {
        return Ok(());
    }
    let Some(a) = crate::supervisor::store::assignment(&app.db, assignment_id).await? else {
        return Ok(());
    };
    // 通知不是新的工作；已經不會開始的交辦（派送當場失敗、被取消／收掉）也不是（#568）。
    if a.expects_review == 0 || !STARTABLE.contains(&a.status.as_str()) {
        return Ok(());
    }
    let Some(bot) = crate::db::bot(&app.db, &a.target_bot_id).await? else {
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
        },
    )
    .await
}

pub(crate) async fn check_opened(app: &Arc<App>, before: &[IssueRef], after: &Row) -> Result<()> {
    if !app.cfg.get().await.judge.enabled {
        return Ok(());
    }
    for cand in opened_candidates(before, after) {
        check_candidate(app, &cand).await?;
    }
    Ok(())
}

async fn check_candidate(app: &Arc<App>, cand: &Candidate) -> Result<()> {
    let cfg = app.cfg.get().await.judge;
    if !cfg.enabled {
        return Ok(());
    }
    // 沒設定：靜默略過，不占保險絲、不寫帳本。
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
        let label = crate::db::project(&app.db, &pid).await?.map(|p| p.label).unwrap_or_default();
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
            let snapshot = snapshot(&pair, &mine, &other);
            let slot = match super::reserve_slot(
                app,
                &cfg,
                &pid,
                &label,
                &cand.bot_id,
                &pair,
                &cand.agent_kind,
                &snapshot,
                false,
                "same_work",
            )
            .await
            {
                Ok(id) => id,
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
            settle_same_work(app, &slot, p, model, ms, tokens, error).await?;
            // 問的時候交辦被取消、收掉或裁示了：答案照記，但標成過時、不推提示，後面的對也不再問（#568）。
            // 第一對之前 `check_assignment` 看過；之後每一對之前都剛經過這裡，中間沒有 HTTP。
            if let Some(status) = gone(app, cand).await? {
                mark_stale(app, &slot, &status).await?;
                return Ok(());
            }
            // 失敗只留帳本。不把錯誤當成「撞了」去推 inbox。
            let Ok(a) = answer else { continue };
            if a.value >= SAME_WORK_THRESHOLD {
                push_hint(app, cand, &other, a.value, &pair).await?;
            }
        }
    }
    Ok(())
}

async fn settle_same_work(
    app: &Arc<App>,
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
        .execute(&app.db)
        .await?;
    Ok(())
}

/// 候選是交辦、而那筆已經不在 [`STARTABLE`] 裡：回它現在的狀態（讀不到＝被刪了，也算）。開票那條沒有交辦，永遠 `None`。
async fn gone(app: &Arc<App>, cand: &Candidate) -> Result<Option<String>> {
    let Some(id) = cand.assignment_id.as_deref() else { return Ok(None) };
    let status = crate::supervisor::store::assignment(&app.db, id).await?.map(|a| a.status).unwrap_or_else(|| "missing".into());
    Ok((!STARTABLE.contains(&status.as_str())).then_some(status))
}

/// 帳本上這一對的 JSON 加 `stale`＝答案回來時交辦的狀態。觀察留著（`jev_same_work` 照填），事後算誤報率時要排除它：
/// 候選根本沒開始，「後來有沒有被併掉」對它沒有意義。
async fn mark_stale(app: &Arc<App>, id: &str, status: &str) -> Result<()> {
    sqlx::query("UPDATE judge_shadow SET matched_line = json_set(matched_line, '$.stale', ?) WHERE id=?")
        .bind(status)
        .bind(id)
        .execute(&app.db)
        .await?;
    Ok(())
}

async fn push_hint(app: &Arc<App>, cand: &Candidate, other: &Card, p: f64, pair: &str) -> Result<()> {
    let parent = cand.parent_bot_id.clone().unwrap_or_else(|| cand.bot_id.clone());
    let existing = other.issue_ref.clone().unwrap_or_else(|| other.key.clone());
    let action = format!(
        "可能與進行中的工作重疊（same_work {p:.2}：{title}）。這是提示，不是判定：{HINT_NOISE}。不阻擋這次派工、不關票、不改認領、不按鍵。",
        title = truncate(&other.title, 80),
    );
    let payload = json!({
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
    });
    let event_key = format!("judge_same_work:{pair}");
    let id = crate::supervisor::store::push_inbox(
        &app.db,
        &event_key,
        "judge_same_work",
        cand.assignment_id.as_deref(),
        Some(&parent),
        None,
        &payload,
    )
    .await?;
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

async fn asked(app: &Arc<App>, pair: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE regex_verdict = 'same_work' AND run_id = ?")
        .bind(pair)
        .fetch_one(&app.db)
        .await?;
    Ok(n > 0)
}

/// release-triage 開的票屬於 `[release_triage] repo`（`owner/name`）。專案 label 或路徑最後一段等於 `name`
/// 的才算「同專案」；對不上就不問——拿別的專案在跑的工作來比只會是雜訊。
async fn repo_projects(app: &Arc<App>) -> Result<Vec<String>> {
    let repo = app.cfg.get().await.release_triage.repo.unwrap_or_default();
    let Some(name) = repo.trim().trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()).map(str::to_string) else {
        return Ok(Vec::new());
    };
    let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT id, label, path FROM projects WHERE deleted_at IS NULL").fetch_all(&app.db).await?;
    Ok(rows
        .into_iter()
        .filter(|(_, label, path)| *label == name || path.trim_end_matches('/').rsplit('/').next() == Some(name.as_str()))
        .map(|(id, _, _)| id)
        .collect())
}

async fn running_cards(app: &Arc<App>, project_id: &str, skip_key: &str, skip_bot: &str) -> Result<Vec<Card>> {
    let mut cards = Vec::new();
    for a in crate::supervisor::store::unsettled_assignments(&app.db).await? {
        if a.expects_review == 0 {
            continue;
        }
        let key = format!("asg:{}", a.id);
        if key == skip_key {
            continue;
        }
        let Some(bot) = crate::db::bot(&app.db, &a.target_bot_id).await? else { continue };
        if bot.deleted_at.is_some() || bot.project_id != project_id || bot.id == skip_bot {
            continue;
        }
        let mut card = assignment_card(&key, &a.text);
        card.bot_id = Some(bot.id);
        cards.push(card);
    }

    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT b.id, b.cwd, r.agent_title
           FROM bots b
           JOIN runs r ON r.bot_id = b.id AND r.state = 'running'
          WHERE b.project_id = ? AND b.deleted_at IS NULL AND b.parent_bot_id IS NOT NULL",
    )
    .bind(project_id)
    .fetch_all(&app.db)
    .await?;
    let mut by_bot: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    for (id, cwd, title) in rows {
        let slot = by_bot.entry(id).or_insert((None, None));
        if slot.0.is_none() {
            slot.0 = cwd;
        }
        if title.as_ref().is_some_and(|t| !t.trim().is_empty()) {
            slot.1 = title;
        }
    }
    for (id, (cwd, title)) in by_bot {
        if id == skip_bot {
            continue;
        }
        if let Some(title) = title {
            let titled = title_of(&title);
            let key = format!("title:{id}");
            if key != skip_key && !titled.is_empty() && !cards.iter().any(|c| c.title == titled) {
                let mut card = text_card(key, "child_title", "an assignment another agent is working on right now", &title);
                card.issue_ref = issue_numbers(&title).first().map(|n| format!("#{n}"));
                card.bot_id = Some(id.clone());
                cards.push(card);
            }
        }
        if let Some(cwd) = cwd {
            if let Some(branch) = branch_of(&cwd) {
                let key = format!("branch:{id}");
                if key != skip_key && !cards.iter().any(|c| c.title == branch) {
                    let mut card = text_card(key, "worktree_branch", "an assignment another agent is working on right now", &branch);
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
    let issues = issue_numbers(&format!("{} {}", cand.title, cand.summary));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::release_triage::ledger::{IssueRef, Row, Status};
    use crate::release_triage::{Bucket, Entry};
    use crate::testing as tt;
    use axum::http::StatusCode;

    #[derive(Clone, Copy)]
    enum Mode {
        Noul(f64),
        Hang,
        Status(u16),
    }

    async fn fake_jev(mode: Mode) -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            let mode = match mode {
                Mode::Noul(p) => Mode::Noul(p),
                Mode::Hang => Mode::Hang,
                Mode::Status(c) => Mode::Status(c),
            };
            async move {
                log.lock().unwrap().push(body);
                match mode {
                    Mode::Hang => {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        (StatusCode::OK, axum::Json(json!({})))
                    }
                    Mode::Status(code) => (StatusCode::from_u16(code).unwrap(), axum::Json(json!({"error": "no"}))),
                    Mode::Noul(p) => (
                        StatusCode::OK,
                        axum::Json(json!({"model": "jev-1.13.0", "answers": {"same_work": {"type": "noul", "noul": p}}, "usage": {"input_tokens": 400}})),
                    ),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen)
    }

    struct Rig {
        env: tt::Env,
        dir: std::path::PathBuf,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    async fn stand(mode: Mode, enabled: bool, listed: bool) -> Rig {
        let env = tt::env().await;
        let (url, seen) = fake_jev(mode).await;
        let dir = std::env::temp_dir().join(format!("am-judge-collision-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, "k-test\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let pid = if listed { env.project_id.clone() } else { "not-this-project".into() };
        let key_s = key.to_string_lossy().into_owned();
        env.app
            .cfg
            .update(move |c| {
                c.judge.enabled = enabled;
                c.judge.projects = vec![pid];
                c.judge.key_file = key_s;
                c.judge.endpoint = url;
                c.judge.timeout_ms = 300;
                Ok(())
            })
            .await
            .unwrap();
        Rig { env, dir, seen }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn write_branch(dir: &std::path::Path, branch: &str) {
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    }

    async fn child(app: &Arc<App>, project: &str, parent: &str, name: &str, cwd: &str, title: &str) -> db::Bot {
        let bot = tt::claude_bot(app, project, name).await;
        sqlx::query("UPDATE bots SET parent_bot_id=?, cwd=?, managed_by='child' WHERE id=?")
            .bind(parent)
            .bind(cwd)
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run = tt::fake_run(app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?").bind(title).bind(&run).execute(&app.db).await.unwrap();
        db::bot(&app.db, &bot.id).await.unwrap().unwrap()
    }

    async fn shadow_rows(app: &Arc<App>) -> Vec<(Option<f64>, Option<f64>, Option<String>, String, String)> {
        sqlx::query_as("SELECT jev_is_live_ui, jev_same_work, error, regex_verdict, matched_line FROM judge_shadow WHERE regex_verdict='same_work' ORDER BY at, id")
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    async fn inbox(app: &Arc<App>) -> Vec<crate::supervisor::store::InboxEvent> {
        crate::supervisor::store::pending_inbox(&app.db).await.unwrap().into_iter().filter(|e| e.kind == "judge_same_work").collect()
    }

    async fn status_of(app: &Arc<App>, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM supervisor_assignments WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    #[tokio::test]
    async fn overlap_at_or_above_half_hints_and_records_without_blocking() {
        let rig = stand(Mode::Noul(0.72), true, true).await;
        let app = rig.env.app.clone();
        let parent = tt::claude_bot(&app, &rig.env.project_id, "parent").await;
        let wt = rig.dir.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        write_branch(&wt, "fix/other-g557");
        let existing = child(&app, &rig.env.project_id, &parent.id, "sib", wt.to_str().unwrap(), "卡住的畫面判斷").await;
        crate::supervisor::store::insert_assignment(
            &app.db,
            None,
            &existing.id,
            "old-557",
            "實作 #557：派工時比對同專案在跑的工作 daemon/src/judge.rs",
            &[],
            None,
            true,
        )
        .await
        .unwrap();
        // 頂層 bot 的標題不是「其他父 bot 的 child」，不該被拿去問。
        let top = tt::claude_bot(&app, &rig.env.project_id, "top").await;
        let top_run = tt::fake_run(&app, &top.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?").bind("不要比對這個 #999").bind(&top_run).execute(&app.db).await.unwrap();

        let newbie = tt::claude_bot(&app, &rig.env.project_id, "newbie").await;
        sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?").bind(&parent.id).bind(&newbie.id).execute(&app.db).await.unwrap();
        // 被派工的那顆自己在跑的標題與先前的交辦：同一顆接著做不是撞題，不拿來比。
        let own_run = tt::fake_run(&app, &newbie.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?").bind("自己上一件 SELF-TITLE").bind(&own_run).execute(&app.db).await.unwrap();
        crate::supervisor::store::insert_assignment(&app.db, None, &newbie.id, "own-old", "SELF-ASSIGNMENT 還沒驗收", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(
            &app.db,
            None,
            &newbie.id,
            "new-557",
            "開票與派工的撞題提示，跟 #557 是同一件事，改 daemon/src/judge/collision.rs Bearer super-secret-token",
            &[],
            None,
            true,
        )
        .await
        .unwrap();

        check_assignment(&app, &a.id).await.unwrap();
        assert_eq!(status_of(&app, &a.id).await, "queued", "shadow 不改交辦狀態");
        assert!(rig.env.herdr.calls_to("pane.send_keys").is_empty(), "不按鍵");

        let sent = rig.seen.lock().unwrap().clone();
        assert!(!sent.is_empty());
        let blob = serde_json::to_string(&sent).unwrap();
        assert!(blob.contains(SAME_WORK_QUESTION));
        assert!(blob.contains(SAME_WORK_FOCUS));
        assert!(!blob.contains("\"collision\""), "不問四級 Score");
        assert!(!blob.contains(&newbie.id) && !blob.contains(&rig.env.project_id), "不送 bot／專案 id");
        assert!(!blob.contains("super-secret-token"), "交辦正文要遮罩");
        assert!(blob.contains("daemon/src/judge.rs") || blob.contains("collision.rs"));
        assert!(!blob.contains("#999") && !blob.contains("不要比對"), "頂層 bot 的標題不在比對裡");
        assert!(!blob.contains("SELF-TITLE") && !blob.contains("SELF-ASSIGNMENT"), "候選那顆自己的工作不比");
        let sources: HashSet<String> = shadow_rows(&app)
            .await
            .into_iter()
            .filter_map(|(_, _, _, _, line)| serde_json::from_str::<Value>(&line).ok())
            .filter_map(|v| v.get("source").and_then(|s| s.as_str()).map(str::to_string))
            .collect();
        assert!(sources.contains("claimed_issue"), "{sources:?}");
        assert!(sources.contains("child_title"), "{sources:?}");
        assert!(sources.contains("worktree_branch"), "{sources:?}");

        let rows = shadow_rows(&app).await;
        assert!(rows.iter().all(|(live, p, err, verdict, _)| {
            live.is_none() && *p == Some(0.72) && err.is_none() && verdict == "same_work"
        }));
        let cleared: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE regex_verdict='same_work' AND cleared_at IS NOT NULL")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(cleared, 0, "cleared_at 只服務撞限，撞題列維持 NULL 給事後對帳");

        let hints = inbox(&app).await;
        assert!(!hints.is_empty());
        assert!(hints.iter().all(|e| e.payload_json.contains(HINT_NOISE) && e.payload_json.contains("不阻擋")));
        assert!(hints.iter().any(|e| e.bot_id.as_deref() == Some(parent.id.as_str())), "提示掛在父 bot");

        let n = sent.len();
        check_assignment(&app, &a.id).await.unwrap();
        assert_eq!(rig.seen.lock().unwrap().len(), n, "同一對不再問");
        assert_eq!(inbox(&app).await.len(), hints.len());
    }

    #[tokio::test]
    async fn half_hints_and_just_under_only_records() {
        let rig = stand(Mode::Noul(0.5), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 sidebar 的捲動", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 sidebar 的捲動殘影", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert_eq!(status_of(&app, &a.id).await, "queued");
        assert_eq!(shadow_rows(&app).await.len(), 1);
        assert_eq!(shadow_rows(&app).await[0].1, Some(0.5));
        assert_eq!(inbox(&app).await.len(), 1, "0.5 要提示");

        let rig = stand(Mode::Noul(0.49), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 sidebar 的捲動", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 sidebar 的捲動殘影", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert_eq!(status_of(&app, &a.id).await, "queued");
        assert_eq!(shadow_rows(&app).await[0].1, Some(0.49));
        assert!(inbox(&app).await.is_empty(), "0.49 只記帳");
    }

    #[tokio::test]
    async fn timeout_and_http_failure_record_an_error_and_do_not_hint_or_block() {
        for mode in [Mode::Hang, Mode::Status(500)] {
            let rig = stand(mode, true, true).await;
            let app = rig.env.app.clone();
            let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
            let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
            crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 quota 橫幅", &[], None, true).await.unwrap();
            let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 quota 橫幅的誤判", &[], None, true).await.unwrap();
            check_assignment(&app, &a.id).await.unwrap();
            assert_eq!(status_of(&app, &a.id).await, "queued");
            let rows = shadow_rows(&app).await;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].0.is_none() && rows[0].1.is_none());
            let err = rows[0].2.as_deref().unwrap_or("");
            assert!(err.contains("request failed") || err.contains("http 500"), "{err}");
            assert!(inbox(&app).await.is_empty(), "失敗不推提示");
        }
    }

    #[tokio::test]
    async fn disabled_unlisted_missing_key_and_notices_skip_quietly() {
        let rig = stand(Mode::Noul(0.99), false, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "做 A 的另一面", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty());
        assert!(shadow_rows(&app).await.is_empty());

        let rig = stand(Mode::Noul(0.99), true, false).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "做 A 的另一面", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty() && shadow_rows(&app).await.is_empty(), "專案不在名單");

        let rig = stand(Mode::Noul(0.99), true, true).await;
        let app = rig.env.app.clone();
        app.cfg.update(|c| { c.judge.key_file = "/tmp/am-judge-no-such-key".into(); Ok(()) }).await.unwrap();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "做 A 的另一面", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty() && shadow_rows(&app).await.is_empty(), "沒有 key 不寫帳");

        let rig = stand(Mode::Noul(0.99), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let note = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "note", "收到，進 idle", &[], None, false).await.unwrap();
        check_assignment(&app, &note.id).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty(), "通知不問");
    }

    #[tokio::test]
    async fn schedule_returns_while_jev_is_still_hanging_and_the_assignment_stays_queued() {
        let rig = stand(Mode::Hang, true, true).await;
        let app = rig.env.app.clone();
        app.cfg.update(|c| { c.judge.timeout_ms = 5000; Ok(()) }).await.unwrap();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "做 A 的另一面", &[], None, true).await.unwrap();
        let started = Instant::now();
        schedule_assignment(&app, &a.id).await;
        assert!(started.elapsed() < std::time::Duration::from_millis(1000), "schedule 不能等 Jev 回來");
        assert_eq!(status_of(&app, &a.id).await, "queued");
        assert!(shadow_rows(&app).await.is_empty() || shadow_rows(&app).await[0].2.as_deref() == Some("pending") || shadow_rows(&app).await[0].1.is_none());
    }

    /// 真的走 `supervisor::assign`：派工照常回來、交辦照常建，撞題的問答在背景才落帳。
    #[tokio::test]
    async fn assign_returns_before_jev_answers_and_the_pair_lands_in_the_ledger() {
        let rig = stand(Mode::Hang, true, true).await;
        let app = rig.env.app.clone();
        app.cfg.update(|c| { c.judge.timeout_ms = 1500; Ok(()) }).await.unwrap();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 quota 橫幅", &[], None, true).await.unwrap();
        let manager = tt::claude_bot(&app, &rig.env.project_id, "agm").await;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisors SET bot_id=? WHERE id=?")
            .bind(&manager.id)
            .bind(crate::supervisor::store::SUPERVISOR_ID)
            .execute(&app.db)
            .await
            .unwrap();
        let started = Instant::now();
        let out = crate::supervisor::assign(&app, &bot.id, "修 quota 橫幅的誤判", "crid-557", None, &[], None, true, None, None, None, Default::default())
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(1000), "派工不能等 Jev");
        assert!(out["ownership_conflicts"].as_array().is_some_and(|v| v.is_empty()), "撞題不併進 ownership_conflicts");
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let row = loop {
            let rows = shadow_rows(&app).await;
            if rows.first().is_some_and(|r| r.2.as_deref() != Some("pending")) {
                break rows[0].clone();
            }
            assert!(Instant::now() < deadline, "背景 task 沒有落帳：{rows:?}");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert!(row.1.is_none() && row.2.as_deref().unwrap_or("").contains("request failed"), "{row:?}");
        let id = out["id"].as_str().unwrap();
        let status = status_of(&app, id).await;
        assert!(status != "failed" && status != "cancelled", "逾時不影響交辦：{status}");
        assert!(inbox(&app).await.is_empty());
    }

    /// #568：真的走 `supervisor::assign`，派送當場就 `dispatch_failed`（整段是終端控制序列，清完什麼都不剩）。
    /// 那筆已經停在 `awaiting_review`、不會開始了：不能再被當成「有人要開始做的工作」去問 Jev、推提示。
    #[tokio::test]
    async fn an_assignment_whose_dispatch_failed_on_the_spot_is_not_asked_or_hinted() {
        let rig = stand(Mode::Noul(0.99), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 quota 橫幅", &[], None, true).await.unwrap();
        let manager = tt::claude_bot(&app, &rig.env.project_id, "agm").await;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisors SET bot_id=? WHERE id=?")
            .bind(&manager.id)
            .bind(crate::supervisor::store::SUPERVISOR_ID)
            .execute(&app.db)
            .await
            .unwrap();
        let out = crate::supervisor::assign(&app, &bot.id, "\u{1b}[2J\u{1b}[31m", "crid-568", None, &[], None, true, None, None, None, Default::default())
            .await
            .unwrap();
        let id = out["id"].as_str().unwrap().to_string();
        let a = crate::supervisor::store::assignment(&app.db, &id).await.unwrap().unwrap();
        assert_eq!((a.status.as_str(), a.turn_status.as_deref()), ("awaiting_review", Some("dispatch_failed")), "前提：派送當場失敗");
        // 背景那個 task 跟這一次直接呼叫都要跳過；直接呼叫讓結果是決定性的，不靠等。
        check_assignment(&app, &id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(rig.seen.lock().unwrap().is_empty(), "不會開始的交辦不問 Jev");
        assert!(shadow_rows(&app).await.is_empty(), "也不占帳本");
        assert!(inbox(&app).await.is_empty(), "不推撞題提示");
    }

    /// 假 Jev：收到請求就卡住，直到 `release` 放行才回 `p`。
    async fn gated_jev(p: f64) -> (String, Arc<std::sync::Mutex<Vec<Value>>>, Arc<tokio::sync::Semaphore>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (log, g) = (seen.clone(), gate.clone());
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let (log, g) = (log.clone(), g.clone());
            async move {
                log.lock().unwrap().push(body);
                g.acquire().await.unwrap().forget();
                axum::Json(json!({"model": "jev-1.13.0", "answers": {"same_work": {"type": "noul", "noul": p}}, "usage": {"input_tokens": 400}}))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen, gate)
    }

    /// #568：Jev 還在回的時候交辦被取消／收掉，答案回來就不能再推提示；帳本留下這次觀察，但標成過時。
    /// 對照組：在途之間的正常推進（送達、等額度）不是過時，照樣提示——只看 `updated_at` 變沒變的話這兩條會被誤殺。
    #[tokio::test]
    async fn a_lifecycle_change_while_jev_is_answering_suppresses_the_hint_and_marks_the_row_stale() {
        use crate::supervisor::assignment_state::AssignmentState as S;
        for (to, stale) in [
            (S::Cancelled, true),
            (S::AwaitingReview, true),
            (S::Completed, true),
            (S::Superseded, true),
            (S::Blocked, true),
            (S::Delivered, false),
            (S::QuotaBlocked, false),
        ] {
            let rig = stand(Mode::Noul(0.0), true, true).await;
            let app = rig.env.app.clone();
            let (url, seen, gate) = gated_jev(0.93).await;
            app.cfg.update(move |c| { c.judge.endpoint = url; c.judge.timeout_ms = 10_000; Ok(()) }).await.unwrap();
            let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
            let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
            crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 sidebar 的捲動", &[], None, true).await.unwrap();
            let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 sidebar 的捲動殘影", &[], None, true).await.unwrap();

            let task = {
                let (app, id) = (app.clone(), a.id.clone());
                tokio::spawn(async move { check_assignment(&app, &id).await })
            };
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            while seen.lock().unwrap().is_empty() {
                assert!(Instant::now() < deadline, "Jev 沒被問到");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let moved = crate::supervisor::assignment_state::set_status(&app.db, &a.id, S::Queued, to, "test").await.unwrap();
            assert!(matches!(moved, crate::supervisor::assignment_state::Outcome::Applied), "{to:?}");
            gate.add_permits(16);
            task.await.unwrap().unwrap();

            let rows: Vec<(Option<f64>, String)> = sqlx::query_as("SELECT jev_same_work, matched_line FROM judge_shadow WHERE regex_verdict='same_work'")
                .fetch_all(&app.db)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1, "{to:?}");
            assert_eq!(rows[0].0, Some(0.93), "{to:?}：觀察照記");
            let line: Value = serde_json::from_str(&rows[0].1).unwrap();
            if stale {
                assert!(inbox(&app).await.is_empty(), "{to:?}：已經不會開始的交辦不推提示");
                assert_eq!(line["stale"].as_str(), Some(to.as_str()), "{to:?}：帳本分得出這筆是過時的觀察");
            } else {
                assert_eq!(inbox(&app).await.len(), 1, "{to:?}：在途的正常推進照樣提示");
                assert!(line.get("stale").is_none(), "{to:?}");
            }
        }
    }

    /// 答案回來之後只重讀狀態、不帶世代戳記，靠的是這個性質：離開可開工的狀態就回不來。
    /// 狀態機哪天多了一條回頭邊（例如 `blocked → queued`），這裡要先紅，逼人補上真的世代比對。
    #[test]
    fn once_out_of_the_startable_states_an_assignment_never_comes_back() {
        for from in crate::supervisor::assignment_state::ALL {
            if STARTABLE.contains(&from) {
                continue;
            }
            for to in STARTABLE {
                assert!(!crate::supervisor::assignment_state::allowed(from, to), "{from} → {to}");
            }
        }
    }

    #[tokio::test]
    async fn a_burst_asks_at_most_fifteen_and_keeps_the_shared_issue() {
        let rig = stand(Mode::Noul(0.2), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "cand").await;
        for i in 0..16 {
            let other = tt::claude_bot(&app, &rig.env.project_id, &format!("o{i}")).await;
            crate::supervisor::store::insert_assignment(
                &app.db,
                None,
                &other.id,
                &format!("old-{i}"),
                &format!("unrelated padding fix number {i} in web/src/pad{i}.tsx"),
                &[],
                None,
                true,
            )
            .await
            .unwrap();
        }
        let shared_bot = tt::claude_bot(&app, &rig.env.project_id, "shared").await;
        crate::supervisor::store::insert_assignment(
            &app.db,
            None,
            &shared_bot.id,
            "old-shared",
            "SHARED-557 跟 #557 是同一張票 daemon/src/judge.rs",
            &[],
            None,
            true,
        )
        .await
        .unwrap();
        let a = crate::supervisor::store::insert_assignment(
            &app.db,
            None,
            &bot.id,
            "new",
            "候選也是 #557，改 daemon/src/judge/collision.rs",
            &[],
            None,
            true,
        )
        .await
        .unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        let sent = rig.seen.lock().unwrap().clone();
        assert_eq!(sent.len(), MAX_PAIRS, "一次最多 15 對");
        let blob = serde_json::to_string(&sent).unwrap();
        assert!(blob.contains("SHARED-557"), "票號重疊的那張要排進名額");
        assert_eq!(status_of(&app, &a.id).await, "queued");
        assert!(inbox(&app).await.is_empty(), "0.2 不提示");
    }

    #[tokio::test]
    async fn an_opened_issue_is_compared_the_same_way() {
        let rig = stand(Mode::Noul(0.81), true, true).await;
        let app = rig.env.app.clone();
        // 票開在 `[release_triage] repo`；label 對得上的專案才是「同專案」。
        app.cfg.update(|c| { c.release_triage.repo = Some("owner/proj".into()); Ok(()) }).await.unwrap();
        let parent = tt::claude_bot(&app, &rig.env.project_id, "parent").await;
        let wt = rig.dir.join("wt2");
        std::fs::create_dir_all(&wt).unwrap();
        write_branch(&wt, "fix/quota-g404");
        child(&app, &rig.env.project_id, &parent.id, "sib", wt.to_str().unwrap(), "舊快照蓋掉額度").await;
        let before = Vec::new();
        let after = sample_row(404);
        let cands = opened_candidates(&before, &after);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].key, "issue:404");
        assert!(cands[0].summary.contains("screen.rs") || cands[0].touches.iter().any(|p| p.contains("screen.rs")));
        check_opened(&app, &before, &after).await.unwrap();
        let sent = rig.seen.lock().unwrap().clone();
        assert!(!sent.is_empty());
        let blob = serde_json::to_string(&sent).unwrap();
        assert!(blob.contains("quota banner") || blob.contains("claude 2.1.277"));
        assert!(blob.contains("fix/quota-g404") || blob.contains("issue #404") || blob.contains("#404"));
        assert!(!inbox(&app).await.is_empty());
        assert!(shadow_rows(&app).await.iter().any(|r| r.1 == Some(0.81)));
        // 已經在 before 裡的票不再問。
        let n = sent.len();
        check_opened(&app, &after.issues, &after).await.unwrap();
        assert_eq!(rig.seen.lock().unwrap().len(), n);
    }

    #[tokio::test]
    async fn an_issue_in_a_repo_no_project_matches_is_not_compared_with_other_projects() {
        let rig = stand(Mode::Noul(0.81), true, true).await;
        let app = rig.env.app.clone();
        app.cfg.update(|c| { c.release_triage.repo = Some("owner/some-other-repo".into()); Ok(()) }).await.unwrap();
        let parent = tt::claude_bot(&app, &rig.env.project_id, "parent").await;
        child(&app, &rig.env.project_id, &parent.id, "sib", rig.dir.to_str().unwrap(), "舊快照蓋掉額度").await;
        check_opened(&app, &[], &sample_row(404)).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty(), "別的 repo 開的票不拿來跟這個專案比");
        assert!(shadow_rows(&app).await.is_empty() && inbox(&app).await.is_empty());
    }

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
        let dir = std::env::temp_dir().join(format!("am-branch-{}", db::ulid()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/fix/demo-g7\n").unwrap();
        assert_eq!(branch_of(dir.to_str().unwrap()).as_deref(), Some("fix/demo-g7"));
        let nested = dir.join(".claude/worktrees/only-the-dirname");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(branch_of(nested.to_str().unwrap()).as_deref(), Some("only-the-dirname"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
