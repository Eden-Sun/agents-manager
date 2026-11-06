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

impl Cluster {
    pub fn idle(&self) -> usize { self.members.iter().filter(|m| m.idle).count() }
    pub fn working(&self) -> usize { self.members.iter().filter(|m| m.working).count() }
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "kind": self.kind, "model": self.model, "effort": self.effort, "identity": self.identity,
            "members": self.members.iter().map(|m| &m.id).collect::<Vec<_>>(), "idle": self.idle(), "working": self.working()})
    }
    /// 可接工作的閒置成員：manager_use=1、running、idle；多個時挑最近閒下來的（cache 最熱）。
    pub fn pick_idle(&self) -> Option<&Member> {
        self.members.iter().filter(|m| m.manager_use && m.running && m.idle).max_by(|a, b| a.since.cmp(&b.since).then_with(|| b.id.cmp(&a.id)))
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
        .filter(|b| b.project_id == project_id && b.role.as_deref() == Some("executor") && b.parent_bot_id.is_none() && b.deleted_at.is_none())
        .map(|b| {
            let r = runs.get(&b.id);
            let running = r.is_some_and(|r| r.state == "running");
            let starting = r.is_some_and(|r| r.state == "starting");
            Member { id: b.id.clone(), kind: b.kind.clone(), model: b.model.clone(), effort: b.effort.clone(), identity: b.identity.clone(),
                manager_use: b.manager_use == 1, running, active: r.is_some(), starting, idle: running && r.is_some_and(|r| r.agent_status == "idle"),
                working: running && r.is_some_and(|r| r.agent_status == "working"), since: r.and_then(|r| r.agent_status_since.clone()) }
        }).collect();
    group(&members)
}

/// 誰能對群集派工、能挑哪些成員：管理者（或代班中的副管理者）不限；專家只能挑授權給自己的執行者（`executor_grants`），
/// 而且不會為它自動啟動停著的成員（授權只涵蓋派工，不含 start）。其他人 `None`＝不能對群集派工。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope { All, Granted(std::collections::BTreeSet<String>) }

impl Scope {
    pub fn allows(&self, bot_id: &str) -> bool {
        match self { Scope::All => true, Scope::Granted(ids) => ids.contains(bot_id) }
    }
    /// 可不可以替這個發送者自動啟動成員。
    pub fn may_start(&self) -> bool { matches!(self, Scope::All) }
}

pub async fn sender_scope(app: &Arc<App>, sender: &db::Bot) -> anyhow::Result<Option<Scope>> {
    if sender.deleted_at.is_some() { return Ok(None) }
    if sender.role.as_deref() == Some("manager") && sender.parent_bot_id.is_none() { return Ok(Some(Scope::All)) }
    // 代班權限只屬於頂層 advisor／specialist（副管理者的合格角色）；降級後即使設定還沒清掉也不算。
    let deputy_role = matches!(sender.role.as_deref(), Some("advisor" | "specialist")) && sender.parent_bot_id.is_none();
    if deputy_role && crate::runners::relay_deputy::acting(app, &sender.id).await { return Ok(Some(Scope::All)) }
    if sender.role.as_deref() != Some("specialist") || sender.parent_bot_id.is_some() { return Ok(None) }
    let Some(project) = db::project(&app.db, &sender.project_id).await? else { return Ok(None) };
    if project.mode != "expert" { return Ok(None) }
    Ok(Some(Scope::Granted(granted_to(&project, sender))))
}

/// 授權給這位專家的執行者 id（track 要等於專家現在的 track）。
pub fn granted_to(project: &db::Project, specialist: &db::Bot) -> std::collections::BTreeSet<String> {
    project.executor_grants().into_iter()
        .filter(|g| g.specialist == specialist.id && specialist.track.as_deref() == Some(g.track.as_str()))
        .map(|g| g.executor).collect()
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
    let bots = db::live_bots(&app.db).await?;
    let runs = db::active_runs_by_bot(&app.db).await?;
    Ok(clusters_of(&bots, &runs, project_id))
}

/// bot id → 群集名。
pub fn names_by_bot(clusters: &[Cluster]) -> BTreeMap<String, String> {
    clusters.iter().flat_map(|c| c.members.iter().map(move |m| (m.id.clone(), c.name.clone()))).collect()
}

/// 這顆成員現在能不能直接接工作（manager_use=1、running、idle）。
pub async fn member_idle(app: &Arc<App>, bot_id: &str) -> bool {
    let Ok(Some(bot)) = db::bot(&app.db, bot_id).await else { return false };
    if bot.manager_use != 1 || bot.deleted_at.is_some() { return false }
    db::active_run(&app.db, bot_id).await.ok().flatten().is_some_and(|r| r.state == "running" && r.agent_status == "idle")
}

impl Cluster {
    /// 沒有閒置成員時該啟動哪一顆：manager_use=1、完全沒有進行中 run 的成員（id 最小的）。
    /// 群集裡已有「啟動中」的成員就回 `None`（一次只啟動一顆）。
    pub fn pick_to_start(&self) -> Option<&Member> {
        if self.members.iter().any(|m| m.starting) { return None }
        self.members.iter().filter(|m| m.manager_use && !m.active).min_by(|a, b| a.id.cmp(&b.id))
    }
}

/// 群集沒有閒置成員時，啟動一顆停著的成員（走既有的 `lifecycle::start_bot`，脫離呼叫端執行）；請求留在 pending，
/// 等它起來閒置後由 reconcile 送出。回傳被啟動的 bot id；沒啟動（已有啟動中、或沒有可啟動的停著成員）回 `None`。
pub async fn start_one_if_needed(app: &Arc<App>, project_id: &str, cluster: &str) -> Option<String> {
    let clusters = project_clusters(app, project_id).await.ok()?;
    let c = clusters.iter().find(|c| c.name == cluster)?;
    if c.pick_idle().is_some() { return None }
    let id = c.pick_to_start()?.id.clone();
    let (app, bot) = (app.clone(), id.clone());
    tokio::spawn(Box::pin(async move {
        if let Err(e) = crate::lifecycle::start_bot(&app, &bot).await { tracing::warn!(bot = %bot, error = ?e, "cluster auto-start failed"); }
    }));
    Some(id)
}

/// 群集請求（還沒送出）現在該由誰接：第一個閒置成員（cache 最熱的優先）。沒有閒置成員回 `None`＝繼續等。
/// 目標換人時以 CAS 改 `to_bot_id`（紀錄裡寫實際接手的 bot）；回傳最後的目標。
///
/// #2070：群集名是顯示用、會隨其他群集增減而變（`haiku` → `haiku-low`），所以不拿名稱找群集，
/// 改找「目前含 `to_bot_id` 的群集」——`to_bot_id` 建立時就驗過是該群集成員、換人也只換成同群集成員，
/// 而群集是以 kind/model/effort/identity 分的，名稱怎麼變它都在原群集裡。
/// 原群集真的不在了（成員刪除、縮到剩一顆）就把請求標成 failed，交給 sweep 通知派工者，不無限等。
pub async fn claim_target(app: &Arc<App>, request_id: &str, project_bot: &str, cluster: &str) -> anyhow::Result<Option<String>> {
    let gone = |why: String| async move {
        sqlx::query("UPDATE relay_requests SET state='failed',error=? WHERE id=? AND state='pending' AND to_turn_id IS NULL")
            .bind(why).bind(request_id).execute(&app.db).await.map(|_| None)
    };
    let Some(bot) = db::bot(&app.db, project_bot).await? else { return Ok(gone(format!("cluster {cluster} no longer exists")).await?) };
    let clusters = project_clusters(app, &bot.project_id).await?;
    let Some(c) = clusters.iter().find(|c| c.members.iter().any(|m| m.id == project_bot)) else {
        return Ok(gone(format!("cluster {cluster} no longer exists")).await?)
    };
    // 每次重選都用發起者「當下」的權限（不論角色）：管理者／代班中的副管理者不限；專家只限授權給它的成員。
    // 發起者不存在、已刪、降級或代班結束（`sender_scope` 回 None）就終止請求——不預設 All，也不會先自動啟動成員。
    let from: Option<String> = sqlx::query_scalar("SELECT from_bot_id FROM relay_requests WHERE id=?").bind(request_id).fetch_optional(&app.db).await?;
    let sender = match from { Some(f) => db::bot(&app.db, &f).await?, None => None };
    let scope = match &sender { Some(s) => sender_scope(app, s).await?, None => None };
    let Some(scope) = scope else { return Ok(gone(format!("cluster {cluster}: sender may no longer dispatch to clusters")).await?) };
    let c = c.restricted(&scope);
    if c.members.is_empty() { return Ok(gone(format!("cluster {cluster}: no member granted to the sender")).await?) }
    let Some(pick) = c.pick_idle() else {
        if scope.may_start() { start_one_if_needed(app, &bot.project_id, &c.name).await; }
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
            identity: if identity.is_empty() { None } else { Some(identity.into()) }, manager_use: true, running: true, active: true, starting: false, idle: false, working: false, since: None }
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
        a.active = true; a.starting = true;
        assert!(group(&[a, b]).remove(0).pick_to_start().is_none(), "已有啟動中的就不再啟動");
    }

    #[test]
    fn pick_idle_prefers_hottest_and_skips_manager_use_off() {
        let mut a = m("a", "claude-haiku-4-5", "", ""); a.idle = true; a.since = Some("2026-11-02T01:00:00Z".into());
        let mut b = m("b", "claude-haiku-4-5", "", ""); b.idle = true; b.since = Some("2026-11-02T02:00:00Z".into());
        let mut c = m("c", "claude-haiku-4-5", "", ""); c.idle = true; c.since = Some("2026-11-02T03:00:00Z".into()); c.manager_use = false;
        let cs = group(&[a, b, c]);
        assert_eq!(cs[0].pick_idle().map(|x| x.id.as_str()), Some("b"));
        let mut w = m("w", "claude-haiku-4-5", "", ""); w.working = true;
        let cs = group(&[w.clone(), { let mut x = w; x.id = "x".into(); x }]);
        assert!(cs[0].pick_idle().is_none());
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
        // 群集縮到剩一顆＝消失：請求進 failed，不永久 pending。
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES('rq2',?,?,'k2','p','[]','pending',?,'haiku')")
            .bind(&manager.id).bind(&a.id).bind(db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&b.id).execute(&env.app.db).await.unwrap();
        assert_eq!(claim_target(&env.app, "rq2", &a.id, "haiku").await.unwrap(), None);
        let (state, error): (String, Option<String>) = sqlx::query_as("SELECT state,error FROM relay_requests WHERE id='rq2'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(state, "failed");
        assert!(error.unwrap_or_default().contains("no longer exists"));
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
mod granted_cluster_tests {
    use super::*;

    struct Fx { env: crate::testing::Env, mgr: String, spec: String, e1: String, e2: String }

    /// 專家專案：管理者、專家（track db）、同設定的兩顆 haiku 執行者（成一個群集）；只把 e1 授權給專家。
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
        let grants = json!([{"specialist": ids[1], "executor": ids[2], "track": "db", "granted_by": "user", "granted_at": "2026-11-06T00:00:00Z"}]).to_string();
        sqlx::query("UPDATE projects SET executor_grants_json=? WHERE id=?").bind(grants).bind(&env.project_id).execute(&app.db).await.unwrap();
        Fx { mgr: ids[0].clone(), spec: ids[1].clone(), e1: ids[2].clone(), e2: ids[3].clone(), env }
    }

    async fn status(f: &Fx, bot: &str, s: &str) {
        sqlx::query("UPDATE runs SET agent_status=? WHERE bot_id=?").bind(s).bind(bot).execute(&f.env.app.db).await.unwrap();
    }

    #[tokio::test]
    async fn a_specialist_resolves_a_cluster_only_to_its_granted_member() {
        let f = fx().await;
        let app = &f.env.app;
        // 未授權的 e2 閒著、e1 在忙：專家仍只會拿到 e1（不越界），管理者拿到閒著的 e2。
        status(&f, &f.e2, "idle").await;
        let spec = crate::api::resolve_cluster_target(app, &f.spec, "haiku").await.unwrap().0;
        assert_eq!(spec["to_bot_id"], f.e1.as_str(), "{spec}");
        assert_eq!(spec["cluster_idle"], false);
        let mgr = crate::api::resolve_cluster_target(app, &f.mgr, "haiku").await.unwrap().0;
        assert_eq!(mgr["to_bot_id"], f.e2.as_str(), "{mgr}");
        // 撤銷後：群集裡沒有授權給它的成員 → 403 cluster_not_granted。
        sqlx::query("UPDATE projects SET executor_grants_json='[]' WHERE id=?").bind(&f.env.project_id).execute(&app.db).await.unwrap();
        match crate::api::resolve_cluster_target(app, &f.spec, "haiku").await {
            Err(crate::lifecycle::LcError::Forbidden(v)) => assert_eq!(v["reason"], "cluster_not_granted"),
            other => panic!("{other:?}"),
        }
        // 執行者本身不能對群集派工。
        assert!(matches!(crate::api::resolve_cluster_target(app, &f.e1, "haiku").await, Err(crate::lifecycle::LcError::Forbidden(_))));
    }

    #[tokio::test]
    async fn a_pending_specialist_cluster_request_is_never_moved_to_an_ungranted_member() {
        let f = fx().await;
        let app = &f.env.app;
        let rid = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,'gc-key','p','[]','pending',?,'haiku')")
            .bind(&rid).bind(&f.spec).bind(&f.e1).bind(db::now()).execute(&app.db).await.unwrap();
        // e2（未授權）閒下來：不改派，繼續等 e1。
        status(&f, &f.e2, "idle").await;
        assert_eq!(claim_target(app, &rid, &f.e1, "haiku").await.unwrap(), None);
        let to: String = sqlx::query_scalar("SELECT to_bot_id FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&app.db).await.unwrap();
        assert_eq!(to, f.e1);
        // e1 閒下來：交給它。
        status(&f, &f.e1, "idle").await;
        assert_eq!(claim_target(app, &rid, &f.e1, "haiku").await.unwrap(), Some(f.e1.clone()));
        // 授權撤掉：請求標 failed，不改派給 e2。
        status(&f, &f.e1, "working").await;
        sqlx::query("UPDATE projects SET executor_grants_json='[]' WHERE id=?").bind(&f.env.project_id).execute(&app.db).await.unwrap();
        assert_eq!(claim_target(app, &rid, &f.e1, "haiku").await.unwrap(), None);
        let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "failed");
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
    enum Change { Demote, Delete, EndActing, None }

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

    /// 管理者、專家 S（track db，授權 e1、e2）、副管理者 D（advisor，代班中）、haiku 群集 e1／e2（running）＋ e3（停著，可被自動啟動）。
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
        let grants = json!([
            {"specialist": spec, "executor": e1, "track": "db", "granted_by": "user", "granted_at": "2026-11-06T00:00:00Z"},
        ]).to_string();
        sqlx::query("UPDATE projects SET executor_grants_json=?, deputy_bot_id=?, deputy_after_sec=60 WHERE id=?").bind(grants).bind(&deputy).bind(&env.project_id).execute(&app.db).await.unwrap();
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
        (rid, to)
    }

    async fn change(f: &Fx, who: &str, c: Change) {
        let db = &f.env.app.db;
        match c {
            Change::Demote => { sqlx::query("UPDATE bots SET role='executor', track=NULL WHERE id=?").bind(who).execute(db).await.unwrap(); }
            Change::Delete => { sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(who).execute(db).await.unwrap(); }
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
        for (sender, c) in [(Sender::Specialist, Change::Demote), (Sender::Specialist, Change::Delete), (Sender::Deputy, Change::EndActing), (Sender::Deputy, Change::Demote)] {
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
            if matches!(sender, Sender::Specialist) {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                assert_eq!(e3_runs(&f).await, 0, "專家不替自己啟動成員");
            } else {
                assert!(crate::testing::eventually!(e3_runs(&f).await > 0), "{sender:?}: 有權限的發起者照舊自動啟動停著的成員");
            }
        }
    }
}
