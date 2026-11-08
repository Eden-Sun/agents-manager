//! 專家模式「群集」：同專案裡 (kind, model, effort, identity) 相同、role=executor、存活的頂層成員 ≥2 顆就是一個群集（不論有沒有在跑）。
//! 管理者可以 `herdr agent prompt bot:cluster:<名稱>` 派工，由 daemon 挑閒置成員；沒有閒置的就排進 relay_requests
//! 的 pending 佇列（`relay_requests.cluster`），第一個閒下來的成員接手（`runners::relay_requests` 的 reconcile）。
use std::collections::BTreeMap;
use std::sync::Arc;
use serde_json::{json, Value};
use crate::{db, state::App};

/// 一顆候選成員的快照（純資料，方便單測）。
#[derive(Clone, Debug)]
pub struct Member {
    pub id: String,
    pub kind: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub identity: Option<String>,
    pub track: Option<String>,
    pub manager_use: bool,
    pub running: bool,
    /// 有任何進行中的 run（starting／running／stopping）；false＝停著。
    pub active: bool,
    /// run 正在 starting。
    pub starting: bool,
    pub idle: bool,
    pub working: bool,
    /// agent_status 最後一次改變的時間（越晚＝越近閒下來＝cache 越熱）。
    pub since: Option<String>,
    pub quota_blocked: bool,
    pub context_pct: Option<f64>,
    /// 側欄 menu 順序：該 bot 在 config.toml 所屬 project 的 `bots` 陣列索引（0 起算，越小越上面）。
    /// `None`＝config 裡找不到（例如 child、剛建立還沒投影）；排在所有有位置的後面。
    pub menu_pos: Option<usize>,
}

/// menu 位置的排序鍵：有位置的依索引由小到大，沒位置的排最後。
fn menu_key(m: &Member) -> (bool, usize) {
    (m.menu_pos.is_none(), m.menu_pos.unwrap_or(0))
}

#[derive(Clone, Debug)]
pub struct Cluster {
    pub name: String,
    pub kind: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub identity: Option<String>,
    pub members: Vec<Member>,
}

/// Stable identity of a named cluster; display names and member IDs can change.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusterKey {
    pub project_id: String,
    pub kind: String,
    pub model: String,
    pub effort: String,
    pub identity: String,
}
impl ClusterKey {
    pub fn from_cluster(project_id: &str, c: &Cluster) -> Self {
        Self { project_id: project_id.into(), kind: c.kind.clone(), model: c.model.clone().unwrap_or_default(),
            effort: c.effort.clone().unwrap_or_default(), identity: c.identity.clone().unwrap_or_default() }
    }
    fn matches(&self, m: &Member) -> bool {
        self.kind == m.kind && self.model == m.model.as_deref().unwrap_or_default()
            && self.effort == m.effort.as_deref().unwrap_or_default()
            && self.identity == m.identity.as_deref().unwrap_or_default()
    }
}

impl Cluster {
    pub fn idle(&self) -> usize { self.members.iter().filter(|m| m.idle).count() }
    pub fn working(&self) -> usize { self.members.iter().filter(|m| m.working).count() }
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "kind": self.kind, "model": self.model, "effort": self.effort, "identity": self.identity,
            "members": self.members.iter().map(|m| &m.id).collect::<Vec<_>>(), "idle": self.idle(), "working": self.working()})
    }
    /// 可接工作的閒置成員：manager_use=1、running、idle、額度未達 critical。
    pub fn pick_best(&self, want_track: Option<&str>) -> Option<&Member> {
        pick_best_member(&self.members, want_track)
    }
}

pub(crate) fn pick_best_member<'a>(members: &'a [Member], want_track: Option<&str>) -> Option<&'a Member> {
    members.iter().filter(|m| m.manager_use && m.running && m.idle && !m.quota_blocked).min_by(|a, b| {
        let track_rank = |m: &Member| if want_track.is_some_and(|t| m.track.as_deref() == Some(t)) { 0 } else { 1 };
        track_rank(a).cmp(&track_rank(b))
            .then_with(|| menu_key(a).cmp(&menu_key(b)))
            .then_with(|| a.context_pct.unwrap_or(50.0).total_cmp(&b.context_pct.unwrap_or(50.0)))
            .then_with(|| b.since.cmp(&a.since))
            .then_with(|| a.id.cmp(&b.id))
    })
}

/// 將模型家族映射到 worker 的預算 tier。
pub fn tier_of(kind: &str, model: Option<&str>) -> &'static str {
    let kind = kind.to_ascii_lowercase();
    let model = model.unwrap_or_default().to_ascii_lowercase();
    if model.contains("haiku") || model.contains("luna")
        || (model.contains("gemini") && (model.contains("flash-low") || model.contains("flash-medium")))
    {
        "cheap"
    } else if model.contains("opus") || model.contains("fable") || model.contains("sol") || model.contains("astra") {
        "strong"
    } else if model.contains("sonnet") || model.contains("terra") || kind == "grok"
        || (model.contains("gemini") && model.contains("flash-high"))
        || (kind == "agy" && model.trim().is_empty())
    {
        "standard"
    } else {
        "standard"
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoTarget {
    pub tier: String,
    pub track: Option<String>,
}

/// Parses the stored selector form `auto:<tier>[:<track>]`.
pub fn parse_auto_target(selector: &str) -> Option<AutoTarget> {
    let rest = selector.strip_prefix("auto:")?;
    let mut parts = rest.splitn(3, ':');
    let tier = parts.next()?;
    if !matches!(tier, "cheap" | "standard" | "strong") { return None }
    let track = parts.next().map(str::to_string);
    if parts.next().is_some() || track.as_deref() == Some("") { return None }
    Some(AutoTarget { tier: tier.to_string(), track })
}

fn context_pct(run: Option<&db::Run>) -> Option<f64> {
    let status = serde_json::from_str::<Value>(run?.status_json.as_deref()?).ok()?;
    let context = status.get("context_window")?;
    let pct = context.get("used_percentage").and_then(Value::as_f64).or_else(|| {
        let used = context.get("total_input_tokens").and_then(Value::as_f64)?;
        let size = context.get("context_window_size").and_then(Value::as_f64)?;
        (size > 0.0).then_some(used * 100.0 / size)
    })?;
    (pct.is_finite() && (0.0..=100.0).contains(&pct)).then_some(pct)
}

fn member_of(bot: &db::Bot, run: Option<&db::Run>) -> Member {
    let running = run.is_some_and(|r| r.state == "running");
    let starting = run.is_some_and(|r| r.state == "starting");
    Member {
        id: bot.id.clone(), kind: bot.kind.clone(), model: bot.model.clone(), effort: bot.effort.clone(), identity: bot.identity.clone(),
        track: bot.track.clone(), manager_use: bot.manager_use == 1, running, active: run.is_some(), starting,
        idle: running && run.is_some_and(|r| r.agent_status == "idle"),
        working: running && run.is_some_and(|r| r.agent_status == "working"),
        since: run.and_then(|r| r.agent_status_since.clone()), quota_blocked: false, context_pct: context_pct(run),
        menu_pos: None,
    }
}

fn sanitize(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_ascii_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' };
        if c == '-' && out.ends_with('-') { continue }
        out.push(c);
    }
    out.trim_matches('-').to_string()
}

/// model 的短名：認得的家族名（haiku／sonnet／opus／fable）取家族名，其餘用 model 本身（去掉 `claude-` 前綴）；沒有 model 用 kind。
pub fn short_model(kind: &str, model: Option<&str>) -> String {
    let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else { return sanitize(kind) };
    let low = model.to_ascii_lowercase();
    for fam in ["haiku", "sonnet", "opus", "fable"] {
        if low.contains(fam) { return fam.to_string(); }
    }
    let s = sanitize(low.strip_prefix("claude-").unwrap_or(&low));
    if s.is_empty() { sanitize(kind) } else { s }
}

/// 把一個專案的成員分群、命名。只回成員 ≥2 的群集，依名稱排序。
pub fn group(members: &[Member]) -> Vec<Cluster> {
    type Key = (String, String, String, String);
    let mut by: BTreeMap<Key, Vec<Member>> = BTreeMap::new();
    for m in members {
        let key = (m.kind.clone(), m.model.clone().unwrap_or_default(), m.effort.clone().unwrap_or_default(), m.identity.clone().unwrap_or_default());
        by.entry(key).or_default().push(m.clone());
    }
    let mut groups: Vec<(String, Cluster)> = by.into_iter().filter(|(_, v)| v.len() >= 2).map(|(k, mut v)| {
        v.sort_by(|a, b| a.id.cmp(&b.id));
        let base = short_model(&k.0, v[0].model.as_deref());
        (base, Cluster { name: String::new(), kind: k.0, model: v[0].model.clone(), effort: v[0].effort.clone(), identity: v[0].identity.clone(), members: v })
    }).collect();
    // 同短名的群集依序加 -<effort>、-<identity>，仍撞名最後補序號。
    let mut names: Vec<String> = groups.iter().map(|(b, _)| b.clone()).collect();
    let collides = |names: &[String], i: usize| names.iter().enumerate().any(|(j, n)| j != i && *n == names[i]);
    for step in 0..2 {
        let snapshot = names.clone();
        for i in 0..names.len() {
            if collides(&snapshot, i) {
                let c = &groups[i].1;
                let part = if step == 0 { c.effort.as_deref() } else { c.identity.as_deref() };
                if let Some(p) = part.map(sanitize).filter(|p| !p.is_empty()) { names[i] = format!("{}-{}", names[i], p); }
            }
        }
    }
    // 序號：同名的第一群保留原名，其餘補 -2、-3…；產生的名字也可能撞到別群的自然名稱（#2071），
    // 所以先把所有現有名稱占住，每個候選都要確認沒被用過才採用。
    let snapshot = names.clone();
    let mut used: std::collections::BTreeSet<String> = snapshot.iter().cloned().collect();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for i in 0..names.len() {
        if collides(&snapshot, i) && !seen.insert(snapshot[i].clone()) {
            let mut n = 2;
            while used.contains(&format!("{}-{}", snapshot[i], n)) { n += 1; }
            names[i] = format!("{}-{}", snapshot[i], n);
            used.insert(names[i].clone());
        }
    }
    for ((_, c), n) in groups.iter_mut().zip(names) { c.name = n; }
    let mut out: Vec<Cluster> = groups.into_iter().map(|(_, c)| c).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 專案的群集，從已讀好的 bots／runs 算（/api/state 一次讀完的快照直接用）。只算頂層、role=executor、未刪除的 bot。
pub fn clusters_of(bots: &[db::Bot], runs: &std::collections::HashMap<String, db::Run>, project_id: &str) -> Vec<Cluster> {
    let members: Vec<Member> = bots.iter()
        .filter(|b| b.project_id == project_id && b.role.as_deref() == Some("executor") && b.parent_bot_id.is_none() && b.deleted_at.is_none() && b.kind != "claude-api")
        .map(|b| member_of(b, runs.get(&b.id))).collect();
    group(&members)
}

/// 誰能對群集派工、能挑哪些成員：管理者（或代班中的副管理者）不限；專家在專案的專家派工開關開著時可挑本專案存活頂層執行者。
/// 停著的成員在範圍內都會自動啟動（管理者與專家一致；範圍之外的成員一律不碰）。其他人（含開關關著時的專家）`None`＝不能對群集派工。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope { All, Specialist(std::collections::BTreeSet<String>) }

impl Scope {
    pub fn allows(&self, bot_id: &str) -> bool {
        match self { Scope::All => true, Scope::Specialist(ids) => ids.contains(bot_id) }
    }
}

pub async fn sender_scope(app: &Arc<App>, sender: &db::Bot) -> anyhow::Result<Option<Scope>> {
    if sender.deleted_at.is_some() { return Ok(None) }
    if sender.role.as_deref() == Some("manager") && sender.parent_bot_id.is_none() { return Ok(Some(Scope::All)) }
    // 代班權限只屬於頂層 advisor／specialist（副管理者的合格角色）；降級後即使設定還沒清掉也不算。
    let deputy_role = matches!(sender.role.as_deref(), Some("advisor" | "specialist")) && sender.parent_bot_id.is_none();
    if deputy_role && crate::runners::relay_deputy::acting(app, &sender.id).await { return Ok(Some(Scope::All)) }
    if sender.role.as_deref() != Some("specialist") || sender.parent_bot_id.is_some() { return Ok(None) }
    let Some(project) = db::project(&app.db, &sender.project_id).await? else { return Ok(None) };
    if project.mode != "expert" || !project.specialist_dispatch { return Ok(None) }
    Ok(Some(Scope::Specialist(dispatchable_executors(app, &project, sender).await?)))
}

/// 這位專家此刻可直接派工的執行者 id：專案是 expert 且開關開著、專家是存活頂層 specialist 時，本專案所有存活頂層 executor；否則空。
pub async fn dispatchable_executors(app: &Arc<App>, project: &db::Project, specialist: &db::Bot) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let ok = project.mode == "expert" && project.specialist_dispatch && project.deleted_at.is_none() && project.handed_off_to.is_none()
        && specialist.project_id == project.id && specialist.deleted_at.is_none() && specialist.parent_bot_id.is_none()
        && specialist.role.as_deref() == Some("specialist");
    if !ok { return Ok(Default::default()) }
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM bots WHERE project_id=? AND role='executor' AND kind<>'claude-api' AND parent_bot_id IS NULL AND deleted_at IS NULL")
        .bind(&project.id).fetch_all(&app.db).await?;
    Ok(ids.into_iter().collect())
}

impl Cluster {
    /// 只留下 `scope` 允許的成員（名稱不變；`Scope::All` 原樣）。
    pub fn restricted(&self, scope: &Scope) -> Cluster {
        let mut c = self.clone();
        c.members.retain(|m| scope.allows(&m.id));
        c
    }
}

/// 專案的群集（讀 DB）。
pub async fn project_clusters(app: &Arc<App>, project_id: &str) -> anyhow::Result<Vec<Cluster>> {
    Ok(group(&project_executors(app, project_id).await?))
}

/// 存活的頂層執行者快照（包含不成群的單顆），並補上此專案主機的額度狀態。
pub async fn project_executors(app: &Arc<App>, project_id: &str) -> anyhow::Result<Vec<Member>> {
    let Some(project) = db::project(&app.db, project_id).await? else { return Ok(Vec::new()) };
    let bots = db::live_bots(&app.db).await?;
    let runs = db::active_runs_by_bot(&app.db).await?;
    let mut members: Vec<Member> = bots.iter()
        .filter(|b| b.project_id == project_id && b.role.as_deref() == Some("executor") && b.parent_bot_id.is_none() && b.deleted_at.is_none() && b.kind != "claude-api")
        .map(|b| member_of(b, runs.get(&b.id))).collect();
    // 側欄 menu 順序：config.toml 裡這個 project 的 bots 陣列索引（拖曳排序寫的就是它）。
    let cfg = app.cfg.get().await;
    let menu: std::collections::HashMap<String, usize> = cfg.projects.iter()
        .find(|p| p.id.as_deref() == Some(project_id))
        .map(|p| p.bots.iter().enumerate().filter_map(|(i, b)| b.id.clone().map(|id| (id, i))).collect())
        .unwrap_or_default();
    let now = chrono::Utc::now();
    for member in &mut members {
        member.menu_pos = menu.get(&member.id).copied();
        let Some(bot) = bots.iter().find(|b| b.id == member.id) else { continue };
        let identity = crate::quota::identity_for_run(bot, runs.get(&bot.id));
        let base = crate::quota::quota_base_for_host(app, &project.host, &bot.kind, identity.as_deref()).await;
        let key = crate::quota::quota_key(&project.host, &base);
        let quota = app.quotas.lock().await.get(&key).cloned();
        member.quota_blocked = quota.is_some_and(|q| {
            q.exhausted(crate::quota::Bucket::FiveHour, now) || q.exhausted(crate::quota::Bucket::SevenDay, now)
        });
    }
    Ok(members)
}

/// bot id → 群集名。
pub fn names_by_bot(clusters: &[Cluster]) -> BTreeMap<String, String> {
    clusters.iter().flat_map(|c| c.members.iter().map(move |m| (m.id.clone(), c.name.clone()))).collect()
}

/// 這顆成員現在能不能直接接工作（manager_use=1、running、idle）。
pub async fn member_idle(app: &Arc<App>, bot_id: &str) -> bool {
    let Ok(Some(bot)) = db::bot(&app.db, bot_id).await else { return false };
    if bot.deleted_at.is_some() { return false }
    project_executors(app, &bot.project_id).await.ok().and_then(|members| {
        members.into_iter().find(|m| m.id == bot_id)
    }).is_some_and(|m| m.manager_use && m.running && m.idle && !m.quota_blocked)
}

impl Cluster {
    /// 沒有閒置成員時該啟動哪一顆：manager_use=1、完全沒有進行中 run 的成員（照 menu 位置最上面的）。
    /// 群集裡已有「啟動中」的成員就回 `None`（一次只啟動一顆）。
    pub fn pick_to_start(&self) -> Option<&Member> {
        pick_to_start_member(&self.members)
    }
}

pub(crate) fn pick_to_start_member(members: &[Member]) -> Option<&Member> {
    if members.iter().any(|m| m.starting) { return None }
    members.iter().filter(|m| m.manager_use && !m.active && !m.quota_blocked)
        .min_by(|a, b| menu_key(a).cmp(&menu_key(b)).then_with(|| a.id.cmp(&b.id)))
}

/// 群集沒有閒置成員時，啟動一顆停著的成員（走既有的 `lifecycle::start_bot`，脫離呼叫端執行）；請求留在 pending，
/// 等它起來閒置後由 reconcile 送出。回傳被啟動的 bot id；沒啟動（已有啟動中、或沒有可啟動的停著成員）回 `None`。
pub async fn start_one_if_needed(app: &Arc<App>, project_id: &str, cluster: &str) -> Option<String> {
    let clusters = project_clusters(app, project_id).await.ok()?;
    let c = clusters.iter().find(|c| c.name == cluster)?;
    if c.pick_best(None).is_some() { return None }
    start_member_if_needed(app, &c.members).await
}

pub async fn start_member_if_needed(app: &Arc<App>, members: &[Member]) -> Option<String> {
    let id = pick_to_start_member(members)?.id.clone();
    let (app, bot) = (app.clone(), id.clone());
    tokio::spawn(Box::pin(async move {
        if let Err(e) = crate::lifecycle::start_bot(&app, &bot).await { tracing::warn!(bot = %bot, error = ?e, "cluster auto-start failed"); }
    }));
    Some(id)
}

/// 群集請求（還沒送出）現在該由誰接：第一個閒置成員（cache 最熱的優先）。沒有閒置成員回 `None`＝繼續等。
/// 目標換人時以 CAS 改 `to_bot_id`（紀錄裡寫實際接手的 bot）；回傳最後的目標。
///
/// 群集名是顯示用，成員設定與名稱都可能變；具名請求使用建立時保存的固定鍵。
/// 只要仍有符合鍵與目前權限的成員就繼續等待或投遞；舊請求缺少固定鍵則安全失敗。
pub async fn claim_target(app: &Arc<App>, request_id: &str, project_bot: &str, cluster: &str) -> anyhow::Result<Option<String>> {
    let gone = |why: String| async move {
        sqlx::query("UPDATE relay_requests SET state='failed',error=? WHERE id=? AND state='pending' AND to_turn_id IS NULL")
            .bind(why).bind(request_id).execute(&app.db).await.map(|_| None)
    };
    let auto = parse_auto_target(cluster);
    let request: Option<(String, Option<String>, Option<String>)> = sqlx::query_as("SELECT from_bot_id,track,cluster_key FROM relay_requests WHERE id=?")
        .bind(request_id).fetch_optional(&app.db).await?;
    let sender = match request.as_ref() { Some((f, _, _)) => db::bot(&app.db, f).await?, None => None };
    let Some(from) = sender.as_ref() else { return Ok(gone(format!("cluster {cluster}: sender not found")).await?) };
    let mut members = if let Some(target) = &auto {
        project_executors(app, &from.project_id).await?.into_iter()
            .filter(|m| tier_of(&m.kind, m.model.as_deref()) == target.tier).collect::<Vec<_>>()
    } else {
        // Older pending requests lack an immutable identity. Never infer it from a bot
        // that may already have changed model or identity; fail closed for retry.
        let key = request.as_ref().and_then(|(_, _, k)| k.as_deref())
            .and_then(|raw| serde_json::from_str::<ClusterKey>(raw).ok());
        let Some(key) = key.filter(|k| k.project_id == from.project_id) else {
            return Ok(gone(format!("cluster {cluster}: original cluster identity unavailable; please resubmit")).await?)
        };
        project_executors(app, &key.project_id).await?.into_iter().filter(|m| key.matches(m)).collect::<Vec<_>>()
    };
    // 每次重選都用發起者「當下」的權限（不論角色）：管理者／代班中的副管理者不限；專家只在開關開著時、限本專案頂層執行者。
    // 發起者不存在、已刪、降級或代班結束（`sender_scope` 回 None）就終止請求——不預設 All，也不會先自動啟動成員。
    let scope = match &sender { Some(s) => sender_scope(app, s).await?, None => None };
    let Some(scope) = scope else { return Ok(gone(format!("cluster {cluster}: sender may no longer dispatch to clusters")).await?) };
    members.retain(|m| scope.allows(&m.id));
    if members.is_empty() { return Ok(gone(format!("cluster {cluster}: no member the sender may dispatch to")).await?) }
    let want_track = auto.as_ref().and_then(|t| t.track.as_deref()).or_else(|| request.as_ref().and_then(|(_, t, _)| t.as_deref()));
    let Some(pick) = pick_best_member(&members, want_track) else {
        start_member_if_needed(app, &members).await;
        return Ok(None)
    };
    if pick.id == project_bot { return Ok(Some(pick.id.clone())) }
    let moved = sqlx::query("UPDATE OR IGNORE relay_requests SET to_bot_id=?,attempts=0,error=NULL WHERE id=? AND state='pending' AND to_turn_id IS NULL AND to_bot_id=? AND NOT EXISTS(SELECT 1 FROM turns t JOIN conversations cv ON cv.id=t.conversation_id WHERE cv.bot_id=? AND t.client_request_id='relay-request:'||relay_requests.id)")
        .bind(&pick.id).bind(request_id).bind(project_bot).bind(project_bot).execute(&app.db).await?.rows_affected();
    Ok(if moved == 1 { Some(pick.id.clone()) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, model: &str, effort: &str, identity: &str) -> Member {
        Member { id: id.into(), kind: "claude".into(), model: Some(model.into()), effort: if effort.is_empty() { None } else { Some(effort.into()) },
            identity: if identity.is_empty() { None } else { Some(identity.into()) }, track: None, manager_use: true, running: true, active: true, starting: false,
            idle: false, working: false, since: None, quota_blocked: false, context_pct: None, menu_pos: None }
    }

    #[test]
    fn tier_classifies_model_families_and_auto_selectors() {
        for (kind, model, tier) in [
            ("claude", "claude-haiku-5-5", "cheap"), ("codex", "gpt-6-luna", "cheap"),
            ("agy", "gemini-3.8-flash-low", "cheap"), ("agy", "gemini-3.8-flash-medium", "cheap"),
            ("claude", "sonnet-5-5", "standard"), ("codex", "gpt-6-terra", "standard"),
            ("grok", "", "standard"), ("agy", "", "standard"), ("agy", "gemini-3.8-flash-high", "standard"),
            ("claude", "opus-5-5", "strong"), ("claude", "fable-5-1", "strong"),
            ("codex", "gpt-6-sol", "strong"), ("codex", "gpt-6-astra", "strong"), ("other", "unknown", "standard"),
        ] {
            assert_eq!(tier_of(kind, (!model.is_empty()).then_some(model)), tier, "{kind}/{model}");
        }
        assert_eq!(parse_auto_target("auto:cheap:db"), Some(AutoTarget { tier: "cheap".into(), track: Some("db".into()) }));
        assert_eq!(parse_auto_target("auto:strong"), Some(AutoTarget { tier: "strong".into(), track: None }));
        assert!(parse_auto_target("auto:unknown").is_none());
        assert!(parse_auto_target("auto:cheap:").is_none());
        assert!(parse_auto_target("auto:cheap:a:b").is_none());
    }

    #[test]
    fn groups_need_two_and_use_short_model_names() {
        let cs = group(&[m("a", "claude-haiku-4-5", "", ""), m("b", "claude-haiku-4-5", "", ""), m("c", "claude-sonnet-5-5", "", ""), m("d", "gpt-6.1-terra", "medium", ""), m("e", "gpt-6.1-terra", "medium", "")]);
        assert_eq!(cs.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["gpt-6.1-terra", "haiku"]);
        assert_eq!(cs[1].members.len(), 2);
    }

    #[test]
    fn same_model_different_effort_or_identity_gets_suffixes() {
        let cs = group(&[m("a", "claude-haiku-4-5", "low", ""), m("b", "claude-haiku-4-5", "low", ""), m("c", "claude-haiku-4-5", "high", ""), m("d", "claude-haiku-4-5", "high", "")]);
        assert_eq!(cs.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["haiku-high", "haiku-low"]);
        let cs = group(&[m("a", "claude-haiku-4-5", "low", "x"), m("b", "claude-haiku-4-5", "low", "x"), m("c", "claude-haiku-4-5", "low", "y"), m("d", "claude-haiku-4-5", "low", "y")]);
        assert_eq!(cs.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["haiku-low-x", "haiku-low-y"]);
    }

    #[test]
    fn generated_number_suffix_never_collides_with_natural_name() {
        // #2071：前兩群都叫 haiku-low-cc1，補序號的 -2 會撞到第三群原本就叫 haiku-low-cc1-2。
        let cs = group(&[m("a", "claude-haiku-4-5", "low", "cc1"), m("b", "claude-haiku-4-5", "low", "cc1"),
            m("c", "claude-haiku-5-5", "low", "cc1"), m("d", "claude-haiku-5-5", "low", "cc1"),
            m("e", "claude-haiku-5-5", "low", "cc1-2"), m("f", "claude-haiku-5-5", "low", "cc1-2")]);
        let names: Vec<&str> = cs.iter().map(|c| c.name.as_str()).collect();
        let uniq: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(uniq.len(), cs.len(), "群集名必須唯一：{names:?}");
        assert_eq!(names, ["haiku-low-cc1", "haiku-low-cc1-2", "haiku-low-cc1-3"]);
        // 自然名稱 haiku-low-cc1-2 仍屬於 identity=cc1-2 那群；每群都能用名稱找到自己。
        assert_eq!(cs.iter().find(|c| c.name == "haiku-low-cc1-2").unwrap().identity.as_deref(), Some("cc1-2"));
        for c in &cs { assert!(std::ptr::eq(cs.iter().find(|x| x.name == c.name).unwrap(), c)); }
    }

    #[test]
    fn stopped_members_still_count_and_start_one_at_a_time() {
        let mut b = m("b", "claude-haiku-4-5", "", "");
        b.running = false; b.active = false;
        let mut a = m("a", "claude-haiku-4-5", "", "");
        a.running = false; a.active = false;
        let cs = group(&[a.clone(), b.clone()]);
        assert_eq!((cs.len(), cs[0].name.as_str(), cs[0].members.len()), (1, "haiku", 2));
        assert_eq!(cs[0].pick_to_start().map(|x| x.id.as_str()), Some("a"));
        a.quota_blocked = true;
        assert_eq!(pick_to_start_member(&[a.clone(), b.clone()]).map(|x| x.id.as_str()), Some("b"), "blocked stopped member is skipped");
        b.quota_blocked = true;
        assert!(pick_to_start_member(&[a.clone(), b.clone()]).is_none(), "all blocked members are skipped");
        a.active = true; a.starting = true;
        assert!(group(&[a, b]).remove(0).pick_to_start().is_none(), "已有啟動中的就不再啟動");
    }

    #[test]
    fn pick_best_prioritizes_track_then_context_then_cache_and_skips_blocked() {
        let mut a = m("a", "claude-haiku-4-5", "", ""); a.idle = true; a.since = Some("2026-11-02T01:00:00Z".into());
        let mut b = m("b", "claude-haiku-4-5", "", ""); b.idle = true; b.since = Some("2026-11-02T02:00:00Z".into());
        let mut c = m("c", "claude-haiku-4-5", "", ""); c.idle = true; c.since = Some("2026-11-02T03:00:00Z".into()); c.manager_use = false;
        a.context_pct = Some(90.0); a.track = Some("db".into());
        b.context_pct = Some(20.0);
        c.context_pct = Some(1.0);
        let members = vec![a.clone(), b.clone(), c];
        assert_eq!(pick_best_member(&members, Some("db")).map(|x| x.id.as_str()), Some("a"), "track priority beats context and cache");
        a.quota_blocked = true;
        let members = vec![a, b.clone(), { let mut x = b; x.id = "z".into(); x.since = Some("2026-11-02T03:00:00Z".into()); x.context_pct = Some(20.0); x }];
        assert_eq!(pick_best_member(&members, None).map(|x| x.id.as_str()), Some("z"), "equal context uses latest idle edge");
        let members = vec![members[1].clone(), { let mut x = members[2].clone(); x.context_pct = Some(40.0); x.since = Some("2026-11-02T04:00:00Z".into()); x }];
        assert_eq!(pick_best_member(&members, None).map(|x| x.id.as_str()), Some("b"), "lower context beats a warmer cache");
        let members = members.into_iter().map(|mut x| { x.quota_blocked = true; x }).collect::<Vec<_>>();
        assert!(pick_best_member(&members, None).is_none(), "all critical members behave as no idle candidates");
        let mut w = m("w", "claude-haiku-4-5", "", ""); w.working = true;
        let cs = group(&[w.clone(), { let mut x = w; x.id = "x".into(); x }]);
        assert!(cs[0].pick_best(None).is_none());
    }

    #[test]
    fn pick_best_follows_menu_order_before_context_and_cache() {
        let mk = |id: &str, pos: Option<usize>, ctx: f64, since: &str| {
            let mut x = m(id, "claude-haiku-4-5", "", ""); x.idle = true; x.menu_pos = pos;
            x.context_pct = Some(ctx); x.since = Some(since.into()); x
        };
        let top = mk("top", Some(0), 90.0, "2026-11-02T01:00:00Z");
        let second = mk("second", Some(1), 10.0, "2026-11-02T05:00:00Z");
        assert_eq!(pick_best_member(&[second.clone(), top.clone()], None).map(|x| x.id.as_str()), Some("top"), "menu 上面的贏，即使 context 高、cache 冷");
        let mut second_tracked = second.clone(); second_tracked.track = Some("db".into());
        assert_eq!(pick_best_member(&[top.clone(), second_tracked], Some("db")).map(|x| x.id.as_str()), Some("second"), "track 相符仍勝過 menu 位置");
        let unplaced = mk("unplaced", None, 1.0, "2026-11-02T06:00:00Z");
        assert_eq!(pick_best_member(&[unplaced.clone(), second.clone()], None).map(|x| x.id.as_str()), Some("second"), "menu_pos=None 排在有位置的後面");
        let a = mk("a", None, 30.0, "2026-11-02T01:00:00Z");
        let b = mk("b", None, 20.0, "2026-11-02T01:00:00Z");
        assert_eq!(pick_best_member(&[a, b], None).map(|x| x.id.as_str()), Some("b"), "menu 相同（都 None）退回 context");
    }

    #[test]
    fn pick_to_start_follows_menu_order_not_id() {
        let stopped = |id: &str, pos: Option<usize>| {
            let mut x = m(id, "claude-haiku-4-5", "", ""); x.running = false; x.active = false; x.menu_pos = pos; x
        };
        let a = stopped("a", Some(1));
        let mut b = stopped("b", Some(0));
        assert_eq!(pick_to_start_member(&[a.clone(), b.clone()]).map(|x| x.id.as_str()), Some("b"), "menu 上面的先啟動，不是 id 最小");
        b.quota_blocked = true;
        assert_eq!(pick_to_start_member(&[a.clone(), b.clone()]).map(|x| x.id.as_str()), Some("a"), "quota_blocked 照常跳過");
        b.quota_blocked = false; b.menu_pos = None;
        assert_eq!(pick_to_start_member(&[b.clone(), a.clone()]).map(|x| x.id.as_str()), Some("a"), "沒位置的排最後");
    }

    #[tokio::test]
    async fn project_executors_reads_menu_order_from_config_and_auto_picks_top() {
        let env = crate::testing::env().await;
        crate::testing::seed_project_config(&env).await;
        let manager = expert_member(&env, "mgr-menu", "manager", "idle").await;
        let a = expert_member(&env, "menu-a", "executor", "idle").await;
        let b = expert_member(&env, "menu-b", "executor", "idle").await;
        // b 在 menu 上面（bots 陣列 [b, a]），但 b 的 context 高、cache 冷；a 的 context 低、cache 熱。
        // 不看 menu 時 a 會贏，所以 b 勝出就是 menu 位置生效。
        sqlx::query("UPDATE runs SET status_json=?, agent_status_since=? WHERE bot_id=?")
            .bind(r#"{"context_window":{"used_percentage":70}}"#).bind("2026-11-02T01:00:00Z").bind(&b.id).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET status_json=?, agent_status_since=? WHERE bot_id=?")
            .bind(r#"{"context_window":{"used_percentage":5}}"#).bind("2026-11-02T05:00:00Z").bind(&a.id).execute(&env.app.db).await.unwrap();
        let (pid, b_id, a_id) = (env.project_id.clone(), b.id.clone(), a.id.clone());
        env.app.cfg.update(move |cfg| {
            let p = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).expect("seeded project");
            p.bots = vec![
                crate::config::BotCfg { id: Some(b_id.clone()), name: "menu-b".into(), kind: "claude".into(), ..Default::default() },
                crate::config::BotCfg { id: Some(a_id.clone()), name: "menu-a".into(), kind: "claude".into(), ..Default::default() },
            ];
            Ok(())
        }).await.unwrap();
        let members = project_executors(&env.app, &env.project_id).await.unwrap();
        assert_eq!(members.iter().find(|m| m.id == b.id).unwrap().menu_pos, Some(0));
        assert_eq!(members.iter().find(|m| m.id == a.id).unwrap().menu_pos, Some(1));
        assert!(members.iter().find(|m| m.id == manager.id).is_none(), "管理者不是執行者");
        let out = crate::api::resolve_auto_target(&env.app, &manager.id, "cheap").await.unwrap().0;
        assert_eq!(out["to_bot_id"], b.id, "menu 上面的 b 勝過 context 較低、cache 較熱的 a");
    }

    async fn expert_member(env: &crate::testing::Env, name: &str, role: &str, status: &str) -> db::Bot {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET role=?, model='claude-haiku-5-5' WHERE id=?").bind(role).bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET state='running', agent_status=?, runtime_role_policy=? WHERE id=?")
            .bind(status).bind(format!("{}:local:expert:{role}", env.project_id)).bind(run).execute(&env.app.db).await.unwrap();
        bot
    }

    #[tokio::test]
    async fn resolve_picks_idle_member_skips_disabled_and_refuses_non_manager() {
        let env = crate::testing::env().await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let manager = expert_member(&env, "mgr", "manager", "idle").await;
        let busy = expert_member(&env, "w1", "executor", "working").await;
        let off = expert_member(&env, "w2", "executor", "idle").await;
        let idle = expert_member(&env, "w3", "executor", "idle").await;
        sqlx::query("UPDATE bots SET manager_use=0 WHERE id=?").bind(&off.id).execute(&env.app.db).await.unwrap();
        let cs = project_clusters(&env.app, &env.project_id).await.unwrap();
        assert_eq!((cs.len(), cs[0].name.as_str(), cs[0].members.len()), (1, "haiku", 3));
        let out = crate::api::resolve_cluster_target(&env.app, &manager.id, "haiku").await.unwrap().0;
        assert_eq!(out["to_bot_id"], idle.id);
        assert_ne!(out["to_bot_id"], off.id);
        assert!(matches!(crate::api::resolve_cluster_target(&env.app, &busy.id, "haiku").await, Err(crate::lifecycle::LcError::Forbidden(_))));
        assert!(matches!(crate::api::resolve_cluster_target(&env.app, &manager.id, "nope").await, Err(crate::lifecycle::LcError::NotFound(_))));
    }

    #[tokio::test]
    async fn a_named_cluster_with_only_quota_blocked_members_does_not_route_to_one() {
        let env = crate::testing::env().await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let manager = expert_member(&env, "mgr-quota-cluster", "manager", "idle").await;
        let a = expert_member(&env, "quota-cluster-a", "executor", "idle").await;
        let b = expert_member(&env, "quota-cluster-b", "executor", "working").await;
        let critical = crate::quota::Quota {
            five_hour: Some(crate::quota::Window { used_pct: 97.0, resets_at: Some("2999-01-01T00:00:00Z".into()), observed_at: Some(db::now()) }),
            seven_day: None, fable: None, reset_credits: None, limit_hit: None, plan: None,
            updated_at: db::now(), source: "test".into(), account: None, host: "local".into(),
        };
        env.app.quotas.lock().await.insert(crate::quota::quota_key("local", "claude"), critical);
        assert!(matches!(crate::api::resolve_cluster_target(&env.app, &manager.id, "haiku").await, Err(crate::lifecycle::LcError::Forbidden(v)) if v["error"] == "member_not_dispatchable"));
        assert!(project_executors(&env.app, &env.project_id).await.unwrap().iter().filter(|m| m.id == a.id || m.id == b.id).all(|m| m.quota_blocked));
    }

    #[tokio::test]
    async fn auto_resolve_scores_singleton_executors_by_track_and_context() {
        let env = crate::testing::env().await;
        let manager = expert_member(&env, "mgr-auto", "manager", "idle").await;
        let db_worker = expert_member(&env, "db-worker", "executor", "idle").await;
        let infra_worker = expert_member(&env, "infra-worker", "executor", "idle").await;
        sqlx::query("UPDATE bots SET identity='db-quota-test' WHERE id=?").bind(&db_worker.id).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET track='db' WHERE id=?").bind(&db_worker.id).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET model='gpt-6-luna', track='infra' WHERE id=?").bind(&infra_worker.id).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET status_json=? WHERE bot_id=?").bind(r#"{"context_window":{"used_percentage":82}}"#).bind(&db_worker.id).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET status_json=? WHERE bot_id=?").bind(r#"{"context_window":{"used_percentage":8}}"#).bind(&infra_worker.id).execute(&env.app.db).await.unwrap();

        let by_track = crate::api::resolve_auto_target(&env.app, &manager.id, "cheap:db").await.unwrap().0;
        assert_eq!(by_track["to_bot_id"], db_worker.id, "matching track wins before context");
        assert_eq!(by_track["cluster"], "auto:cheap:db");
        let by_context = crate::api::resolve_auto_target(&env.app, &manager.id, "cheap").await.unwrap().0;
        assert_eq!(by_context["to_bot_id"], infra_worker.id, "singleton workers participate; lower context wins");

        let critical = crate::quota::Quota {
            five_hour: Some(crate::quota::Window { used_pct: 97.0, resets_at: Some("2999-01-01T00:00:00Z".into()), observed_at: Some(db::now()) }),
            seven_day: None, fable: None, reset_credits: None, limit_hit: None, plan: None,
            updated_at: db::now(), source: "test".into(), account: None, host: "local".into(),
        };
        assert!(critical.exhausted(crate::quota::Bucket::FiveHour, chrono::Utc::now()), "fixture must be critical");
        let db_bot = db::bot(&env.app.db, &db_worker.id).await.unwrap().unwrap();
        let run = db::active_run(&env.app.db, &db_worker.id).await.unwrap();
        let identity = crate::quota::identity_for_run(&db_bot, run.as_ref());
        let base = crate::quota::quota_base_for_host(&env.app, "local", &db_bot.kind, identity.as_deref()).await;
        let key = crate::quota::quota_key("local", &base);
        env.app.quotas.lock().await.insert(key.clone(), critical);
        let members = project_executors(&env.app, &env.project_id).await.unwrap();
        let db_snapshot = members.iter().find(|m| m.id == db_worker.id).unwrap();
        assert!(db_snapshot.quota_blocked, "quota key {key}, identity {identity:?}");
        assert!(!members.iter().find(|m| m.id == infra_worker.id).unwrap().quota_blocked);
        let after_quota = crate::api::resolve_auto_target(&env.app, &manager.id, "cheap:db").await.unwrap().0;
        assert_eq!(after_quota["to_bot_id"], infra_worker.id, "critical identity quota blocks the otherwise matching-track worker");

        sqlx::query("UPDATE bots SET identity='db-quota-test' WHERE id=?").bind(&infra_worker.id).execute(&env.app.db).await.unwrap();
        assert!(matches!(crate::api::resolve_auto_target(&env.app, &manager.id, "cheap:db").await, Err(crate::lifecycle::LcError::Forbidden(v)) if v["error"] == "member_not_dispatchable"), "when every candidate is blocked, do not route to one as a pending placeholder");
    }

    #[tokio::test]
    async fn pending_named_cluster_stays_on_original_key_when_target_changes_or_is_deleted() {
        for change in ["model", "effort", "identity", "deleted"] {
            let env = crate::testing::env().await;
            sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
            let manager = expert_member(&env, "mgr", "manager", "idle").await;
            let a = expert_member(&env, "original", "executor", "working").await;
            let b = expert_member(&env, "original-peer", "executor", "idle").await;
            let c = expert_member(&env, "other", "executor", "idle").await;
            sqlx::query("UPDATE bots SET model='claude-sonnet-5-5' WHERE id=?").bind(&c.id).execute(&env.app.db).await.unwrap();
            sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('r',?,?,'key','fixture','[]','pending',?,'haiku')")
                .bind(&manager.id).bind(&a.id).bind(db::now()).execute(&env.app.db).await.unwrap();
            sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('r2',?,?,'key2','fixture','[]','pending',?,'haiku')")
                .bind(&manager.id).bind(&a.id).bind(db::now()).execute(&env.app.db).await.unwrap();
            let statement = match change {
                "model" => "UPDATE bots SET model='claude-sonnet-5-5' WHERE id=?",
                "effort" => "UPDATE bots SET effort='high' WHERE id=?",
                "identity" => "UPDATE bots SET identity='other' WHERE id=?",
                _ => "UPDATE bots SET deleted_at='2026-01-01T00:00:00Z' WHERE id=?",
            };
            sqlx::query(statement).bind(&a.id).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET agent_status='idle' WHERE bot_id=?").bind(&a.id).execute(&env.app.db).await.unwrap();
            assert_eq!(claim_target(&env.app, "r", &a.id, "haiku").await.unwrap(), Some(b.id.clone()), "{change}");
            let (to, key): (String, String) = sqlx::query_as("SELECT to_bot_id,cluster_key FROM relay_requests WHERE id='r'").fetch_one(&env.app.db).await.unwrap();
            assert_eq!(to, b.id, "{change}");
            assert_eq!(serde_json::from_str::<ClusterKey>(&key).unwrap().model, "claude-haiku-5-5");
            sqlx::query("UPDATE bots SET deleted_at='2026-01-01T00:00:00Z' WHERE id=?").bind(&b.id).execute(&env.app.db).await.unwrap();
            assert_eq!(claim_target(&env.app, "r2", &a.id, "haiku").await.unwrap(), None, "{change}");
            let (state, to): (String, String) = sqlx::query_as("SELECT state,to_bot_id FROM relay_requests WHERE id='r2'").fetch_one(&env.app.db).await.unwrap();
            assert_eq!(state, "failed", "{change}");
            assert_ne!(to, c.id, "must not dispatch to the different cluster");
        }
    }

    #[tokio::test]
    async fn legacy_named_request_without_a_key_fails_closed() {
        let env = crate::testing::env().await;
        let manager = expert_member(&env, "mgr", "manager", "idle").await;
        let a = expert_member(&env, "w1", "executor", "idle").await;
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('r',?,?,'key','fixture','[]','pending',?,'haiku')")
            .bind(&manager.id).bind(&a.id).bind(db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE relay_requests SET cluster_key=NULL WHERE id='r'").execute(&env.app.db).await.unwrap();
        assert_eq!(claim_target(&env.app, "r", &a.id, "haiku").await.unwrap(), None);
        let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE id='r'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "failed");
    }

    #[tokio::test]
    async fn queued_request_goes_to_the_first_member_that_turns_idle() {
        let env = crate::testing::env().await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let manager = expert_member(&env, "mgr", "manager", "idle").await;
        let a = expert_member(&env, "w1", "executor", "working").await;
        let b = expert_member(&env, "w2", "executor", "working").await;
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('rq',?,?,'k','p','[]','pending',?,'haiku')")
            .bind(&manager.id).bind(&a.id).bind(db::now()).execute(&env.app.db).await.unwrap();
        // 全忙：沒人接手，請求原地等。
        assert_eq!(claim_target(&env.app, "rq", &a.id, "haiku").await.unwrap(), None);
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE bot_id=?").bind(&b.id).execute(&env.app.db).await.unwrap();
        assert_eq!(claim_target(&env.app, "rq", &a.id, "haiku").await.unwrap(), Some(b.id.clone()));
        let to: String = sqlx::query_scalar("SELECT to_bot_id FROM relay_requests WHERE id='rq'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(to, b.id);
    }

    #[tokio::test]
    async fn auto_pending_request_reselects_across_executors_and_fails_without_a_tier_candidate() {
        let env = crate::testing::env().await;
        let manager = expert_member(&env, "mgr-auto-claim", "manager", "idle").await;
        let a = expert_member(&env, "w-auto-a", "executor", "working").await;
        let b = expert_member(&env, "w-auto-b", "executor", "idle").await;
        sqlx::query("UPDATE bots SET model='gpt-6-luna' WHERE id=?").bind(&b.id).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('rq-auto',?,?,'k-auto','p','[]','pending',?,'auto:cheap')")
            .bind(&manager.id).bind(&a.id).bind(db::now()).execute(&env.app.db).await.unwrap();
        assert_eq!(claim_target(&env.app, "rq-auto", &a.id, "auto:cheap").await.unwrap(), Some(b.id.clone()));
        let to: String = sqlx::query_scalar("SELECT to_bot_id FROM relay_requests WHERE id='rq-auto'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(to, b.id);

        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('rq-auto-gone',?,?,'k-auto-gone','p','[]','pending',?,'auto:strong')")
            .bind(&manager.id).bind(&a.id).bind(db::now()).execute(&env.app.db).await.unwrap();
        assert_eq!(claim_target(&env.app, "rq-auto-gone", &a.id, "auto:strong").await.unwrap(), None);
        let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE id='rq-auto-gone'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "failed", "tier without any live executor fails instead of waiting forever");
    }

    /// #2070：建立 pending 後新增同模型不同 effort 的群集，原群集名從 `haiku` 變 `haiku-low`；
    /// 原成員閒下來時請求仍要送到它。群集真的消失時請求要進 failed（sweep 會通知派工者），不能永久 pending。
    #[tokio::test]
    async fn pending_cluster_request_survives_rename_and_fails_when_cluster_vanishes() {
        let env = crate::testing::env().await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let manager = expert_member(&env, "mgr", "manager", "idle").await;
        let a = expert_member(&env, "w1", "executor", "working").await;
        let b = expert_member(&env, "w2", "executor", "working").await;
        sqlx::query("UPDATE bots SET effort='low' WHERE id IN (?,?)").bind(&a.id).bind(&b.id).execute(&env.app.db).await.unwrap();
        assert_eq!(project_clusters(&env.app, &env.project_id).await.unwrap().iter().map(|c| c.name.clone()).collect::<Vec<_>>(), ["haiku"]);
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('rq',?,?,'k','p','[]','pending',?,'haiku')")
            .bind(&manager.id).bind(&a.id).bind(db::now()).execute(&env.app.db).await.unwrap();
        for n in ["h1", "h2"] {
            let h = expert_member(&env, n, "executor", "idle").await;
            sqlx::query("UPDATE bots SET effort='high' WHERE id=?").bind(&h.id).execute(&env.app.db).await.unwrap();
        }
        assert_eq!(project_clusters(&env.app, &env.project_id).await.unwrap().iter().map(|c| c.name.clone()).collect::<Vec<_>>(), ["haiku-high", "haiku-low"]);
        assert_eq!(claim_target(&env.app, "rq", &a.id, "haiku").await.unwrap(), None, "原群集全忙：繼續等，不能被 high 群集接走");
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE bot_id=?").bind(&b.id).execute(&env.app.db).await.unwrap();
        assert_eq!(claim_target(&env.app, "rq", &a.id, "haiku").await.unwrap(), Some(b.id.clone()));
        // 原群集縮到一顆時仍保留身份；剩餘成員閒置後可以接手。
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('rq2',?,?,'k2','p','[]','pending',?,'haiku')")
            .bind(&manager.id).bind(&a.id).bind(db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&b.id).execute(&env.app.db).await.unwrap();
        assert_eq!(claim_target(&env.app, "rq2", &a.id, "haiku").await.unwrap(), None);
        let (state, error): (String, Option<String>) = sqlx::query_as("SELECT state,error FROM relay_requests WHERE id='rq2'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "pending");
        assert!(error.is_none());
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE bot_id=?").bind(&a.id).execute(&env.app.db).await.unwrap();
        assert_eq!(claim_target(&env.app, "rq2", &a.id, "haiku").await.unwrap(), Some(a.id.clone()));
    }

    #[tokio::test]
    async fn all_stopped_cluster_dispatch_starts_exactly_one_member() {
        let env = crate::testing::env().await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let manager = expert_member(&env, "mgr", "manager", "idle").await;
        let mut ids = vec![];
        for n in ["w1", "w2"] {
            let bot = crate::testing::claude_bot(&env.app, &env.project_id, n).await;
            sqlx::query("UPDATE bots SET role='executor', model='claude-haiku-5-5' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
            ids.push(bot.id);
        }
        let cs = project_clusters(&env.app, &env.project_id).await.unwrap();
        assert_eq!((cs.len(), cs[0].members.len()), (1, 2));
        let started = start_one_if_needed(&env.app, &env.project_id, "haiku").await;
        assert!(started.is_some());
        // 已有啟動中的成員：不重複啟動。
        let first = started.unwrap();
        let run = crate::testing::fake_run(&env.app, &first).await;
        sqlx::query("UPDATE runs SET state='starting' WHERE id=?").bind(run).execute(&env.app.db).await.unwrap();
        assert_eq!(start_one_if_needed(&env.app, &env.project_id, "haiku").await, None);
        // 起來並閒置後，請求送達它。
        sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE bot_id=?").bind(&first).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('rq',?,?,'k','p','[]','pending',?,'haiku')")
            .bind(&manager.id).bind(ids.iter().find(|i| **i != first).unwrap()).bind(db::now()).execute(&env.app.db).await.unwrap();
        assert_eq!(claim_target(&env.app, "rq", ids.iter().find(|i| **i != first).unwrap(), "haiku").await.unwrap(), Some(first));
    }
}

#[cfg(test)]
mod specialist_cluster_tests {
    use super::*;

    struct Fx { env: crate::testing::Env, mgr: String, spec: String, e1: String, e2: String }

    /// 專家專案（專家派工開關預設開）：管理者、專家（track db）、同設定的兩顆 haiku 執行者（成一個群集）。
    async fn fx() -> Fx {
        let env = crate::testing::env().await;
        let app = &env.app;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
        let mut ids = Vec::new();
        for (name, role, track) in [("gc-mgr", "manager", None), ("gc-spec", "specialist", Some("db")), ("gc-e1", "executor", None), ("gc-e2", "executor", None)] {
            let b = crate::testing::claude_bot(app, &env.project_id, name).await;
            sqlx::query("UPDATE bots SET role=?, track=?, model='claude-haiku-5-5' WHERE id=?").bind(role).bind(track).bind(&b.id).execute(&app.db).await.unwrap();
            let run = crate::testing::fake_run(app, &b.id).await;
            sqlx::query("UPDATE runs SET state='running', agent_status='working', runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:{role}", env.project_id)).bind(run).execute(&app.db).await.unwrap();
            ids.push(b.id);
        }
        Fx { mgr: ids[0].clone(), spec: ids[1].clone(), e1: ids[2].clone(), e2: ids[3].clone(), env }
    }

    async fn status(f: &Fx, bot: &str, s: &str) {
        sqlx::query("UPDATE runs SET agent_status=? WHERE bot_id=?").bind(s).bind(bot).execute(&f.env.app.db).await.unwrap();
    }

    async fn switch(f: &Fx, on: bool) {
        sqlx::query("UPDATE projects SET specialist_dispatch=? WHERE id=?").bind(on).bind(&f.env.project_id).execute(&f.env.app.db).await.unwrap();
    }

    #[tokio::test]
    async fn a_specialist_resolves_a_cluster_while_the_switch_is_on() {
        let f = fx().await;
        let app = &f.env.app;
        // 開著：專家跟管理者一樣拿到閒著的成員。
        status(&f, &f.e2, "idle").await;
        let spec = crate::api::resolve_cluster_target(app, &f.spec, "haiku").await.unwrap().0;
        assert_eq!((spec["to_bot_id"].as_str(), spec["cluster_idle"].as_bool()), (Some(f.e2.as_str()), Some(true)), "{spec}");
        let mgr = crate::api::resolve_cluster_target(app, &f.mgr, "haiku").await.unwrap().0;
        assert_eq!(mgr["to_bot_id"], f.e2.as_str(), "{mgr}");
        // child 化的成員不算（專家只派頂層執行者）。
        assert_eq!(sender_scope(app, &db::bot(&app.db, &f.spec).await.unwrap().unwrap()).await.unwrap(), Some(Scope::Specialist([f.e1.clone(), f.e2.clone()].into_iter().collect())));
        sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?").bind(&f.mgr).bind(&f.e2).execute(&app.db).await.unwrap();
        assert!(!sender_scope(app, &db::bot(&app.db, &f.spec).await.unwrap().unwrap()).await.unwrap().unwrap().allows(&f.e2));
        sqlx::query("UPDATE bots SET parent_bot_id=NULL WHERE id=?").bind(&f.e2).execute(&app.db).await.unwrap();
        // 關掉：專家不能對群集派工；管理者照舊。
        switch(&f, false).await;
        match crate::api::resolve_cluster_target(app, &f.spec, "haiku").await {
            Err(crate::lifecycle::LcError::Forbidden(v)) => assert_eq!(v["reason"], "cluster_manager_only"),
            other => panic!("{other:?}"),
        }
        assert!(crate::api::resolve_cluster_target(app, &f.mgr, "haiku").await.is_ok());
        // 執行者本身不能對群集派工。
        assert!(matches!(crate::api::resolve_cluster_target(app, &f.e1, "haiku").await, Err(crate::lifecycle::LcError::Forbidden(_))));
    }

    #[tokio::test]
    async fn a_pending_specialist_cluster_request_moves_while_on_and_fails_once_off() {
        let f = fx().await;
        let app = &f.env.app;
        let rid = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,'gc-key','p','[]','pending',?,'haiku')")
            .bind(&rid).bind(&f.spec).bind(&f.e1).bind(db::now()).execute(&app.db).await.unwrap();
        // 全忙：繼續等（專家不替自己啟動成員）。
        assert_eq!(claim_target(app, &rid, &f.e1, "haiku").await.unwrap(), None);
        // e2 閒下來：開著時改派給它。
        status(&f, &f.e2, "idle").await;
        assert_eq!(claim_target(app, &rid, &f.e1, "haiku").await.unwrap(), Some(f.e2.clone()));
        // 另一筆，關掉後：請求標 failed，不改派。
        let rid2 = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,'gc-key2','p','[]','pending',?,'haiku')")
            .bind(&rid2).bind(&f.spec).bind(&f.e1).bind(db::now()).execute(&app.db).await.unwrap();
        switch(&f, false).await;
        assert_eq!(claim_target(app, &rid2, &f.e1, "haiku").await.unwrap(), None);
        let (state, to): (String, String) = sqlx::query_as("SELECT state,to_bot_id FROM relay_requests WHERE id=?").bind(&rid2).fetch_one(&app.db).await.unwrap();
        assert_eq!((state.as_str(), to.as_str()), ("failed", f.e1.as_str()));
    }
}

/// Astra 第二輪 #2：pending 群集請求每次重選都用發起者當下的權限；發起者降級、被刪、代班結束時終止請求，不改派、不自動啟動。
#[cfg(test)]
mod sender_rescope_tests {
    use super::*;
    use axum::{body::Body, http::{Request, StatusCode}};
    use tower::ServiceExt;

    #[derive(Clone, Copy, Debug)]
    enum Sender { Specialist, Deputy, Manager }
    #[derive(Clone, Copy, Debug)]
    enum Change { Demote, Delete, EndActing, SwitchOff, None }

    struct Fx { env: crate::testing::Env, mgr: String, spec: String, deputy: String, e1: String, e2: String, e3: String }

    async fn member(env: &crate::testing::Env, name: &str, role: &str, track: Option<&str>, status: Option<&str>) -> String {
        let b = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET role=?, track=?, model='claude-haiku-5-5' WHERE id=?").bind(role).bind(track).bind(&b.id).execute(&env.app.db).await.unwrap();
        if let Some(status) = status {
            let run = crate::testing::fake_run(&env.app, &b.id).await;
            sqlx::query("UPDATE runs SET state='running', agent_status=?, runtime_role_policy=? WHERE id=?").bind(status).bind(format!("{}:local:expert:{role}", env.project_id)).bind(run).execute(&env.app.db).await.unwrap();
        }
        b.id
    }

    /// 管理者、專家 S（track db；專家派工開關開著）、副管理者 D（advisor，代班中）、haiku 群集 e1／e2（running）＋ e3（停著，可被自動啟動）。
    async fn fx() -> Fx {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
        let mgr = member(&env, "rs-mgr", "manager", None, Some("idle")).await;
        let spec = member(&env, "rs-spec", "specialist", Some("db"), Some("idle")).await;
        let deputy = member(&env, "rs-deputy", "advisor", None, Some("idle")).await;
        let e1 = member(&env, "rs-e1", "executor", None, Some("working")).await;
        let e2 = member(&env, "rs-e2", "executor", None, Some("working")).await;
        let e3 = member(&env, "rs-e3", "executor", None, None).await;
        sqlx::query("UPDATE projects SET deputy_bot_id=?, deputy_after_sec=60 WHERE id=?").bind(&deputy).bind(&env.project_id).execute(&app.db).await.unwrap();
        // 代班中：有一則從管理者改派給 D、還沒做完的請求。
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,redirected_from,redirected_at) VALUES('rs-redir',?,?,'rs-redir','q','[]','delivered',?,?,?)")
            .bind(&spec).bind(&deputy).bind(db::now()).bind(&mgr).bind(db::now()).execute(&app.db).await.unwrap();
        assert!(crate::runners::relay_deputy::acting(&app, &deputy).await);
        let cs = project_clusters(&app, &env.project_id).await.unwrap();
        assert_eq!((cs.len(), cs[0].name.as_str(), cs[0].members.len()), (1, "haiku", 3), "fixture 要成一個三顆成員的 haiku 群集");
        Fx { env, mgr, spec, deputy, e1, e2, e3 }
    }

    async fn http(f: &Fx, from: &str, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder().method(method).uri(path).header("host", "127.0.0.1:7799")
            .header("X-AM-Bot-Id", from).header("X-AM-Bot-Token", "tok").header("content-type", "application/json")
            .body(Body::from(body.to_string())).unwrap();
        let res = crate::api::router(f.env.app.clone()).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn status(f: &Fx, bot: &str, s: &str) {
        sqlx::query("UPDATE runs SET agent_status=? WHERE bot_id=?").bind(s).bind(bot).execute(&f.env.app.db).await.unwrap();
    }

    /// 經真實的 resolve＋POST 建立一則 pending 群集請求：resolve 時讓目標閒著（避免建立當下就自動啟動），POST 前改成忙（留在 pending）。
    async fn dispatch(f: &Fx, from: &str) -> (String, String) {
        let target = if from == f.spec { &f.e1 } else { &f.e2 };
        status(f, target, "idle").await;
        let (code, v) = http(f, from, "GET", "/api/relay/resolve?target=bot:cluster:haiku", Value::Null).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let to = v["to_bot_id"].as_str().unwrap().to_string();
        assert_eq!(&to, target, "{v}");
        status(f, &to, "working").await;
        let body = json!({"text": "work", "client_request_id": format!("rs-{from}"), "expect_result": true, "cluster": "haiku"});
        let (code, v) = http(f, from, "POST", &format!("/api/bots/{to}/prompt"), body).await;
        assert!(code.is_success(), "{code}: {v}");
        assert_eq!(v["delivery"], "pending", "{v}");
        let rid = v["request_id"].as_str().unwrap().to_string();
        let cluster: Option<String> = sqlx::query_scalar("SELECT cluster FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(cluster.as_deref(), Some("haiku"));
        let raw: String = sqlx::query_scalar("SELECT cluster_key FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&f.env.app.db).await.unwrap();
        let key: crate::expert_cluster::ClusterKey = serde_json::from_str(&raw).unwrap();
        assert_eq!(key.project_id, f.env.project_id);
        assert!(key.model.contains("haiku"));
        (rid, to)
    }

    #[tokio::test]
    async fn auto_selector_uses_relay_resolve_and_persists_its_cluster_for_pending_claims() {
        let f = fx().await;
        status(&f, &f.e2, "idle").await;
        let (code, resolved) = http(&f, &f.mgr, "GET", "/api/relay/resolve?target=bot:auto:cheap", Value::Null).await;
        assert_eq!(code, StatusCode::OK, "{resolved}");
        assert_eq!(resolved["cluster"], "auto:cheap");
        assert_eq!(resolved["to_bot_id"], f.e2);

        status(&f, &f.e2, "working").await;
        let body = json!({"text":"auto work","client_request_id":"auto-route-key","expect_result":true,"cluster":"auto:cheap"});
        let (code, response) = http(&f, &f.mgr, "POST", &format!("/api/bots/{}/prompt", f.e2), body).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{response}");
        assert_eq!(response["delivery"], "pending");
        let request_id = response["request_id"].as_str().unwrap();
        let stored: Option<String> = sqlx::query_scalar("SELECT cluster FROM relay_requests WHERE id=?").bind(request_id).fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(stored.as_deref(), Some("auto:cheap"));

        status(&f, &f.e1, "idle").await;
        assert_eq!(claim_target(&f.env.app, request_id, &f.e2, "auto:cheap").await.unwrap(), Some(f.e1.clone()));
    }

    async fn change(f: &Fx, who: &str, c: Change) {
        let db = &f.env.app.db;
        match c {
            Change::Demote => { sqlx::query("UPDATE bots SET role='executor', track=NULL WHERE id=?").bind(who).execute(db).await.unwrap(); }
            Change::Delete => { sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(who).execute(db).await.unwrap(); }
            Change::SwitchOff => { sqlx::query("UPDATE projects SET specialist_dispatch=0 WHERE id=?").bind(&f.env.project_id).execute(db).await.unwrap(); }
            Change::EndActing => { sqlx::query("UPDATE relay_requests SET state='answered', answered_at=? WHERE id='rs-redir'").bind(db::now()).execute(db).await.unwrap(); }
            Change::None => {}
        }
    }

    async fn e3_runs(f: &Fx) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&f.e3).fetch_one(&f.env.app.db).await.unwrap()
    }

    async fn row(f: &Fx, rid: &str) -> (String, String, Option<String>) {
        sqlx::query_as("SELECT state,to_bot_id,error FROM relay_requests WHERE id=?").bind(rid).fetch_one(&f.env.app.db).await.unwrap()
    }

    #[tokio::test]
    async fn a_sender_that_lost_its_authority_neither_moves_nor_starts_members() {
        for (sender, c) in [(Sender::Specialist, Change::Demote), (Sender::Specialist, Change::Delete), (Sender::Specialist, Change::SwitchOff), (Sender::Deputy, Change::EndActing), (Sender::Deputy, Change::Demote)] {
            // 不改派：另一顆成員閒下來（舊行為會以 Scope::All 改派給它）。
            let f = fx().await;
            let from = match sender { Sender::Specialist => f.spec.clone(), Sender::Deputy => f.deputy.clone(), Sender::Manager => f.mgr.clone() };
            let (rid, to) = dispatch(&f, &from).await;
            change(&f, &from, c).await;
            let other = if to == f.e1 { &f.e2 } else { &f.e1 };
            status(&f, other, "idle").await;
            assert_eq!(claim_target(&f.env.app, &rid, &to, "haiku").await.unwrap(), None, "{sender:?}/{c:?}");
            let (state, now_to, error) = row(&f, &rid).await;
            assert_eq!((state.as_str(), now_to.as_str()), ("failed", to.as_str()), "{sender:?}/{c:?}: {error:?}");
            assert!(error.unwrap_or_default().contains("may no longer dispatch"), "{sender:?}/{c:?}");

            // 不啟動：全忙、e3 停著（舊行為會先自動啟動 e3）。
            let f = fx().await;
            let from = match sender { Sender::Specialist => f.spec.clone(), Sender::Deputy => f.deputy.clone(), Sender::Manager => f.mgr.clone() };
            let (rid, to) = dispatch(&f, &from).await;
            change(&f, &from, c).await;
            assert_eq!(claim_target(&f.env.app, &rid, &to, "haiku").await.unwrap(), None, "{sender:?}/{c:?}");
            assert_eq!(row(&f, &rid).await.0, "failed", "{sender:?}/{c:?}");
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert_eq!(e3_runs(&f).await, 0, "{sender:?}/{c:?}: 不得替失去權限的發起者啟動成員");
        }
    }

    /// 對照組：權限仍在的管理者／代班副管理者照舊——全忙時自動啟動停著的成員、請求繼續等；專家不啟動。
    #[tokio::test]
    async fn senders_that_keep_their_authority_behave_as_before() {
        for sender in [Sender::Manager, Sender::Deputy, Sender::Specialist] {
            let f = fx().await;
            let from = match sender { Sender::Specialist => f.spec.clone(), Sender::Deputy => f.deputy.clone(), Sender::Manager => f.mgr.clone() };
            let (rid, to) = dispatch(&f, &from).await;
            change(&f, &from, Change::None).await;
            assert_eq!(claim_target(&f.env.app, &rid, &to, "haiku").await.unwrap(), None);
            assert_eq!(row(&f, &rid).await.0, "pending", "{sender:?}");
            // 專家的派工範圍內（本專案頂層執行者）的停著成員，與管理者一樣會被自動啟動。
            assert!(crate::testing::eventually!(e3_runs(&f).await > 0), "{sender:?}: 有權限的發起者照舊自動啟動停著的成員");
        }
    }
}
