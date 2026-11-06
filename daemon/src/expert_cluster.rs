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
pub async fn claim_target(app: &Arc<App>, request_id: &str, project_bot: &str, cluster: &str) -> anyhow::Result<Option<String>> {
    let Some(bot) = db::bot(&app.db, project_bot).await? else { return Ok(None) };
    let clusters = project_clusters(app, &bot.project_id).await?;
    let Some(c) = clusters.iter().find(|c| c.name == cluster) else { return Ok(None) };
    let Some(pick) = c.pick_idle() else {
        start_one_if_needed(app, &bot.project_id, cluster).await;
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
