use std::sync::Arc;
use axum::{extract::{Path, Query, State}, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::{api::RequestPrincipal, lifecycle::LcError, state::App};

#[derive(Debug, Deserialize, Default)]
pub(crate) struct PeersQuery {
    #[serde(rename = "for")]
    pub for_track: Option<String>,
}

fn model_cost_rank(model: Option<&str>) -> Option<u32> {
    let m = model?.to_ascii_lowercase();
    if m.contains("haiku") {
        Some(1)
    } else if m.contains("sonnet") {
        Some(2)
    } else if m.contains("terra") || m.contains("opus") {
        Some(3)
    } else {
        None
    }
}

pub(crate) async fn list(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Query(query): Query<PeersQuery>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    let caller = match principal {
        RequestPrincipal::User => None,
        RequestPrincipal::Bot(id) | RequestPrincipal::Child { child_id: id, .. } => {
            let bot = crate::db::bot(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::Forbidden(json!({"error":"forbidden"})))?;
            if bot.project_id != project_id { return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"cross_project"}))); }
            Some(bot)
        }
        _ => return Err(LcError::Forbidden(json!({"error":"forbidden"}))),
    };
    let project = crate::db::project(&app.db, &project_id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("project".into()))?;
    // 專家視角（專家模式頂層 specialist 呼叫）：授權給它的執行者才可直接派，其他要經管理者。管理者與其他視角不變。
    // 代班中的副管理者（advisor／specialist）有管理者的派工權限：給管理者視角，不套授權限制（同 `expert_cluster::sender_scope`）。
    let acting = match &caller { Some(c) if c.parent_bot_id.is_none() && matches!(c.role.as_deref(), Some("advisor" | "specialist")) => crate::runners::relay_deputy::acting(&app, &c.id).await, _ => false };
    let granted: Option<std::collections::BTreeSet<String>> = caller.as_ref()
        .filter(|c| !acting && project.mode == "expert" && c.role.as_deref() == Some("specialist") && c.parent_bot_id.is_none())
        .map(|c| crate::expert_cluster::granted_to(&project, c));
    let caller_id = caller.as_ref().map(|c| c.id.clone());
    let bots = crate::db::live_bots(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let mut peers = Vec::new();
    for bot in bots.iter().filter(|b| b.project_id == project_id) {
        let run = crate::db::active_run(&app.db, &bot.id).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        // A successful RPC establishes when we observed herdr, but not the freshness of its
        // cached underlying agent state. Keep that uncertainty visible to the caller.
        let mut status = "unknown".to_string();
        let mut observed_at = None;
        if let Some(r) = &run {
            if let (Some(session), Some(pane), Some(fence)) = (r.herdr_session.as_deref(), r.pane_id.as_deref(), app.hosts.fence(&project.host).await) {
                if let Some(client) = app.herdr_for_host_fence(&fence, session).await {
                    if let Ok(Ok(Some(agent))) = tokio::time::timeout(std::time::Duration::from_secs(2), client.agent_get(pane)).await {
                        if agent.pane_id == pane && fence.is_current_now() {
                            status = agent.agent_status.as_str().to_string();
                            observed_at = Some(crate::db::now());
                        }
                    }
                }
            }
        }
        let open: Option<i64> = sqlx::query_scalar("SELECT count(*) FROM relay_requests WHERE to_bot_id=? AND state IN ('pending','delivered')").bind(&bot.id).fetch_one(&app.db).await.ok();
        let mut peer = json!({"id":bot.id,"name":bot.name,"role":bot.role,"track":bot.track,"model":bot.model,"manager_use":bot.manager_use == 1,"host":project.host,"run_state":run.as_ref().map(|r| r.state.as_str()).unwrap_or("stopped"),"agent_status":status,"observed_at":observed_at,"status_stale":true,"open_requests":open});
        // 專家視角：執行者標 `granted`（true＝授權給你，可直接派；false＝要經管理者）。
        if let (Some(set), Some("executor")) = (&granted, bot.role.as_deref()) {
            peer["granted"] = json!(set.contains(&bot.id));
            if !set.contains(&bot.id) { peer["dispatch"] = json!("via_manager"); }
        }
        peers.push(peer);
    }
    let Some(for_track) = query.for_track.as_deref() else {
        // 沒給 --for：專家照一般排序但排在執行者之後（穩定排序，相對順序不動）。
        peers.sort_by_key(|p| p["role"].as_str() == Some("specialist"));
        // 執行者池條目與每條的已用數（`agm hire --pool <id>` 用的 id 就從這裡看）。
        let cfg = app.cfg.get().await;
        let cfg_bots = cfg.projects.iter().find(|c| c.id.as_deref() == Some(project_id.as_str())).map(|c| c.bots.as_slice()).unwrap_or(&[]);
        let pool = crate::executor_pool::list_json(&project.executor_pool(), cfg_bots);
        let clusters: Vec<Value> = crate::expert_cluster::project_clusters(&app, &project_id).await.map_err(|e| LcError::Upstream(e.to_string()))?.iter().map(|c| c.to_json()).collect();
        let mut out = json!({"project_id":project_id,"peers":peers,"executor_pool":pool,"clusters":clusters});
        out["executor_grants"] = json!(project.executor_grants());
        if acting { out["acting_deputy"] = json!(true); }
        return Ok(Json(out));
    };

    // Candidates evaluation for `for_track`
    // track 名稱只在專案內有意義：別的專案的同名 track 不算「進行中」（以收件 bot 的 project_id 劃範圍）。
    // 專家視角：派給專家自己的那則（管理者交辦的統籌請求）與專家自己派出的子任務都不算衝突——專家在自己進行中的 track 內
    // 可以把子任務分給多顆授權執行者；只有別人（管理者或其他專家）在同 track 交給其他成員的工作才擋。
    let coordinator = granted.as_ref().and(caller_id.as_deref());
    let active_track_holder: Option<String> = sqlx::query_scalar(
        "SELECT r.to_bot_id FROM relay_requests r JOIN bots b ON b.id = r.to_bot_id \
         WHERE r.track=? AND b.project_id=? AND r.state IN ('pending','delivered') \
         AND (? IS NULL OR (r.to_bot_id<>? AND r.from_bot_id<>?)) ORDER BY r.created_at DESC LIMIT 1"
    ).bind(for_track).bind(&project_id).bind(coordinator).bind(coordinator).bind(coordinator)
        .fetch_optional(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;

    let last_turns = crate::cache_clock::last_turns_by_bot(&app.db).await.unwrap_or_default();
    let now = chrono::Utc::now();

    struct Candidate {
        bot_id: String,
        name: String,
        role: Option<String>,
        model: Option<String>,
        eligible: bool,
        reasons: Vec<String>,
        score: i64,
        rank_tier: u32,
        /// 同 track 的專家：排在所有候選最前面（理由「專家（同 track）」）。
        specialist_match: bool,
        /// 專家視角：這顆執行者授權給呼叫的專家（`None`＝不是專家視角或不是執行者）。
        granted: Option<bool>,
        /// fork 提示用：codex、同 track（最近一筆 relay 的 track）、正在忙。
        fork_hint: bool,
        kind: String,
    }

    let mut candidates = Vec::new();
    for bot in bots.into_iter().filter(|b| b.project_id == project_id && matches!(b.role.as_deref(), Some("executor" | "specialist"))) {
        // 其他 track 的專家不是這個 track 的候選（不搶別的 track）。
        let is_specialist = bot.role.as_deref() == Some("specialist");
        if is_specialist && bot.track.as_deref() != Some(for_track) {
            continue;
        }
        // 專家（含代班中的專家）不把自己列成候選；其他角色照舊。
        if caller_id.as_deref() == Some(bot.id.as_str()) && caller.as_ref().is_some_and(|c| c.role.as_deref() == Some("specialist")) {
            continue;
        }
        let run = crate::db::active_run(&app.db, &bot.id).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        let mut status = run.as_ref().map(|r| r.agent_status.clone()).unwrap_or_else(|| "unknown".to_string());
        if let Some(r) = &run {
            if let (Some(session), Some(pane), Some(fence)) = (r.herdr_session.as_deref(), r.pane_id.as_deref(), app.hosts.fence(&project.host).await) {
                if let Some(client) = app.herdr_for_host_fence(&fence, session).await {
                    if let Ok(Ok(Some(agent))) = tokio::time::timeout(std::time::Duration::from_secs(2), client.agent_get(pane)).await {
                        if agent.pane_id == pane && fence.is_current_now() {
                            status = agent.agent_status.as_str().to_string();
                        }
                    }
                }
            }
        }

        let mut eligible = true;
        let mut reasons = Vec::new();

        // 0. manager_use 停用開關（SPEC quota protection）
        if bot.manager_use == 0 {
            eligible = false;
            reasons.push("使用者已停用".to_string());
        }

        // 1. 與專案同主機
        let bot_host = crate::db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| project.host.clone());
        if bot_host != project.host {
            eligible = false;
            reasons.push("跨主機".to_string());
        }

        // 2. run 不是 running 或 agent_status 是 blocked
        if run.as_ref().map(|r| r.state.as_str()) != Some("running") {
            eligible = false;
            reasons.push("未執行".to_string());
        }
        if status == "blocked" {
            eligible = false;
            reasons.push("已阻塞".to_string());
        }

        // 3. 身份額度 5h 已用 ≥ 85%
        let billing_id = crate::quota::identity_for_run(&bot, run.as_ref());
        let quota_base = crate::quota::quota_base_for_host(&app, &project.host, &bot.kind, billing_id.as_deref()).await;
        let q_key = crate::quota::quota_key(&project.host, &quota_base);
        let q_snap = app.quotas.lock().await.get(&q_key).cloned();
        match q_snap {
            Some(q) => {
                if let Some(w) = q.five_hour {
                    if w.used_pct >= 85.0 {
                        eligible = false;
                        reasons.push("額度用盡".to_string());
                    }
                } else {
                    reasons.push("額度未知".to_string());
                }
            }
            None => {
                reasons.push("額度未知".to_string());
            }
        }

        // 4. 衝突防護：某 track 已有 pending/delivered 的 relay_request 指派給某執行者時，其他執行者 eligible=false、理由「track 已由 X 進行中」
        if let Some(holder) = &active_track_holder {
            if holder != &bot.id {
                eligible = false;
                let holder_name: String = sqlx::query_scalar("SELECT name FROM bots WHERE id=?").bind(holder).fetch_optional(&app.db).await.ok().flatten().unwrap_or_else(|| holder.clone());
                reasons.push(format!("track 已由 {holder_name} 進行中"));
            }
        }

        // 5. 專家視角：只有授權給你的執行者能直接派；其他（含別的專家）要經管理者。
        let granted_here = match &granted {
            Some(set) if !is_specialist => Some(set.contains(&bot.id)),
            _ => None,
        };
        if granted.is_some() {
            if granted_here == Some(true) {
                reasons.push("已授權給你".to_string());
            } else {
                eligible = false;
                reasons.push("未授權（需經管理者）".to_string());
            }
        }

        // 排序分數計算
        let mut score: i64 = 0;
        if is_specialist {
            reasons.push("專家（同 track）".to_string());
        }

        // 最近一筆 relay_requests 的 track 相同者最優先（理由「同 track」）
        let last_req_track: Option<Option<String>> = sqlx::query_scalar(
            "SELECT track FROM relay_requests WHERE to_bot_id=? ORDER BY created_at DESC LIMIT 1"
        ).bind(&bot.id).fetch_optional(&app.db).await.ok().flatten();
        let same_track = matches!(&last_req_track, Some(Some(t)) if t == for_track);
        if let Some(Some(t)) = last_req_track {
            if t == for_track {
                score += 1000;
                reasons.push("同 track".to_string());
            }
        }

        // 其次 agent_status idle/done（「閒著」）
        if status == "idle" || status == "done" {
            score += 100;
            reasons.push("閒著".to_string());
        }

        // 再其次 codex 一律算 cache 熱、claude 看最後回合完成在 60 分內（「cache 熱」）
        let is_cache_warm = if bot.kind == "codex" {
            true
        } else if bot.kind == "claude" {
            if let Some(lt) = last_turns.get(&bot.id) {
                if let Some(completed_at_str) = &lt.completed_at {
                    if let Some(completed) = crate::quota::parse_utc(completed_at_str) {
                        (now - completed).num_minutes() <= 60
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if is_cache_warm {
            score += 10;
            reasons.push("cache 熱".to_string());
        }

        let rank = model_cost_rank(bot.model.as_deref()).unwrap_or(99);
        let busy = !matches!(status.as_str(), "idle" | "done" | "unknown");
        let fork_hint = bot.kind == "codex" && same_track && busy;

        candidates.push(Candidate {
            granted: granted_here,
            fork_hint,
            kind: bot.kind.clone(),
            bot_id: bot.id,
            name: bot.name,
            role: bot.role,
            model: bot.model,
            eligible,
            reasons,
            score,
            rank_tier: rank,
            specialist_match: is_specialist,
        });
    }

    candidates.sort_by(|a, b| {
        b.eligible.cmp(&a.eligible)
            .then_with(|| b.specialist_match.cmp(&a.specialist_match))
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.rank_tier.cmp(&b.rank_tier))
            .then_with(|| a.bot_id.cmp(&b.bot_id))
    });

    // 只是提示：同 track 有 codex 執行者在忙、而且它有 session，就指給 manager 看（不自動 fork）。
    let mut fork_suggestion = Value::Null;
    for c in candidates.iter().filter(|c| c.fork_hint) {
        if crate::db::latest_native_session_id(&app.db, &c.bot_id).await.ok().flatten().is_some() {
            fork_suggestion = json!({"source_bot_id": c.bot_id, "name": c.name, "kind": c.kind});
            break;
        }
    }
    let cand_json: Vec<Value> = candidates.into_iter().map(|c| {
        let mut v = json!({
            "id": c.bot_id,
            "name": c.name,
            "role": c.role,
            "model": c.model,
            "eligible": c.eligible,
            "score": c.score,
            "reasons": c.reasons,
        });
        if let Some(g) = c.granted { v["granted"] = json!(g); }
        v
    }).collect();

    let mut out = json!({"project_id":project_id,"candidates":cand_json,"fork_suggestion":fork_suggestion});
    if acting { out["acting_deputy"] = json!(true); }
    Ok(Json(out))
}

pub(crate) const EXPERT_NO_CHILD: &str = "專家模式沒有 child：用 agm peers 找同專案的頂層執行者／顧問，以 herdr agent prompt bot:<id> --expect-result 派工；管理者缺人手用 agm hire 增聘執行者。";

pub(crate) async fn spawn_authority(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    let RequestPrincipal::Bot(id) = principal else { return Err(LcError::Forbidden(json!({"error":"forbidden"}))); };
    let bot = crate::db::bot(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::Forbidden(json!({"error":"forbidden"})))?;
    let project = crate::db::project(&app.db, &bot.project_id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::Forbidden(json!({"error":"forbidden"})))?;
    if bot.parent_bot_id.is_some() || bot.managed_by == "child" { return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"child_cannot_spawn"}))); }
    // 專家模式沒有 child（使用者 2026-10-31）：執行者、顧問都是使用者建立的頂層 bot。
    if project.mode == "expert" { return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"expert_no_child","message":EXPERT_NO_CHILD}))); }
    Ok(Json(json!({"allowed":true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn peers_are_project_scoped_and_include_stopped_bots() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "peer").await;
        let value = list(State(app.clone()), Path(env.project_id.clone()), Query(PeersQuery::default()), Extension(RequestPrincipal::Bot(bot.id.clone()))).await.unwrap().0;
        assert!(value["peers"].as_array().unwrap().iter().any(|p| p["id"] == bot.id));
        let other = crate::db::ulid();
        sqlx::query("INSERT INTO projects(id,path,label,created_at) VALUES (?,?,?,?)").bind(&other).bind(format!("/tmp/{other}")).bind("other").bind(crate::db::now()).execute(&app.db).await.unwrap();
        assert!(matches!(list(State(app), Path(other), Query(PeersQuery::default()), Extension(RequestPrincipal::Bot(bot.id))).await, Err(LcError::Forbidden(_))));
    }

    #[tokio::test]
    async fn peers_manager_use_disabled_marks_ineligible() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();

        let b1 = crate::testing::claude_bot(&app, &env.project_id, "b1").await;
        sqlx::query("UPDATE bots SET role='executor', manager_use=0 WHERE id=?").bind(&b1.id).execute(&app.db).await.unwrap();
        let r1 = crate::testing::fake_run(&app, &b1.id).await;
        sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(&r1).execute(&app.db).await.unwrap();

        // 1. peers list without for
        let list_res = list(
            State(app.clone()),
            Path(env.project_id.clone()),
            Query(PeersQuery::default()),
            Extension(RequestPrincipal::Bot(b1.id.clone())),
        ).await.unwrap().0;
        let p1 = list_res["peers"].as_array().unwrap().iter().find(|p| p["id"] == b1.id).unwrap();
        assert_eq!(p1["manager_use"], false);

        // 2. candidates evaluation with for_track
        let cand_res = list(
            State(app.clone()),
            Path(env.project_id.clone()),
            Query(PeersQuery { for_track: Some("track-1".into()) }),
            Extension(RequestPrincipal::Bot(b1.id.clone())),
        ).await.unwrap().0;
        let c1 = cand_res["candidates"].as_array().unwrap().iter().find(|c| c["id"] == b1.id).unwrap();
        assert_eq!(c1["eligible"], false);
        assert!(c1["reasons"].as_array().unwrap().iter().any(|r| r == "使用者已停用"));
    }

    #[tokio::test]
    async fn candidates_evaluation_with_track_prioritizes_and_filters() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();

        // bot1: executor, running, idle, same track
        let b1 = crate::testing::claude_bot(&app, &env.project_id, "b1").await;
        sqlx::query("UPDATE bots SET role='executor', model='claude-haiku-5-5' WHERE id=?").bind(&b1.id).execute(&app.db).await.unwrap();
        let r1 = crate::testing::fake_run(&app, &b1.id).await;
        sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(&r1).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,track) VALUES('rr1',?,?,'c1','p','[]','answered',?,'feature-x')")
            .bind(&b1.id).bind(&b1.id).bind(crate::db::now()).execute(&app.db).await.unwrap();

        // bot2: executor, running, blocked
        let b2 = crate::testing::claude_bot(&app, &env.project_id, "b2").await;
        sqlx::query("UPDATE bots SET role='executor', model='claude-sonnet-4-6' WHERE id=?").bind(&b2.id).execute(&app.db).await.unwrap();
        let r2 = crate::testing::fake_run(&app, &b2.id).await;
        sqlx::query("UPDATE runs SET state='running', agent_status='blocked' WHERE id=?").bind(&r2).execute(&app.db).await.unwrap();

        // bot3: executor, stopped
        let b3 = crate::testing::claude_bot(&app, &env.project_id, "b3").await;
        sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&b3.id).execute(&app.db).await.unwrap();

        let res = list(
            State(app.clone()),
            Path(env.project_id.clone()),
            Query(PeersQuery { for_track: Some("feature-x".into()) }),
            Extension(RequestPrincipal::Bot(b1.id.clone())),
        ).await.unwrap().0;

        let cands = res["candidates"].as_array().unwrap();
        assert_eq!(cands.len(), 3);
        // b1 is top candidate, eligible, score includes same track (1000) + idle (100)
        assert_eq!(cands[0]["id"], b1.id);
        assert_eq!(cands[0]["eligible"], true);
        assert!(cands[0]["reasons"].as_array().unwrap().iter().any(|r| r == "同 track"));
        assert!(cands[0]["reasons"].as_array().unwrap().iter().any(|r| r == "閒著"));

        // b2 is blocked
        let b2_cand = cands.iter().find(|c| c["id"] == b2.id).unwrap();
        assert_eq!(b2_cand["eligible"], false);
        assert!(b2_cand["reasons"].as_array().unwrap().iter().any(|r| r == "已阻塞"));

        // b3 is stopped
        let b3_cand = cands.iter().find(|c| c["id"] == b3.id).unwrap();
        assert_eq!(b3_cand["eligible"], false);
        assert!(b3_cand["reasons"].as_array().unwrap().iter().any(|r| r == "未執行"));

        // Track conflict protection: if another bot is running feature-x, other executors get marked ineligible
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,track) VALUES('rr2',?,?,'c2','p','[]','pending',?,'feature-x')")
            .bind(&b1.id).bind(&b1.id).bind(crate::db::now()).execute(&app.db).await.unwrap();

        let res2 = list(
            State(app.clone()),
            Path(env.project_id.clone()),
            Query(PeersQuery { for_track: Some("feature-x".into()) }),
            Extension(RequestPrincipal::Bot(b1.id.clone())),
        ).await.unwrap().0;
        let cands2 = res2["candidates"].as_array().unwrap();
        let b2_in_conflict = cands2.iter().find(|c| c["id"] == b2.id).unwrap();
        assert_eq!(b2_in_conflict["eligible"], false);
        assert!(b2_in_conflict["reasons"].as_array().unwrap().iter().any(|r| r.as_str().unwrap().contains("進行中")));
    }

    #[tokio::test]
    async fn an_active_track_in_another_project_does_not_block_this_project() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
        let here = crate::testing::claude_bot(&app, &env.project_id, "here").await;
        sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&here.id).execute(&app.db).await.unwrap();
        let r = crate::testing::fake_run(&app, &here.id).await;
        sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(&r).execute(&app.db).await.unwrap();

        // 另一個專案的 bot 手上有同名 track 的 pending 委派。
        let other = crate::db::ulid();
        sqlx::query("INSERT INTO projects(id,path,label,created_at) VALUES (?,?,?,?)").bind(&other).bind(format!("/tmp/{other}")).bind("other").bind(crate::db::now()).execute(&app.db).await.unwrap();
        let there = crate::testing::claude_bot(&app, &other, "there").await;
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,track) VALUES('rr-x',?,?,'cx','p','[]','pending',?,'billing')")
            .bind(&there.id).bind(&there.id).bind(crate::db::now()).execute(&app.db).await.unwrap();

        let res = list(State(app.clone()), Path(env.project_id.clone()), Query(PeersQuery { for_track: Some("billing".into()) }), Extension(RequestPrincipal::Bot(here.id.clone()))).await.unwrap().0;
        let c = res["candidates"].as_array().unwrap().iter().find(|c| c["id"] == here.id).unwrap().clone();
        assert_eq!(c["eligible"], true, "{c}");
        assert!(!c["reasons"].as_array().unwrap().iter().any(|r| r.as_str().unwrap().contains("進行中")), "{c}");

        // 同專案的同名 track 仍會擋。
        let peer = crate::testing::claude_bot(&app, &env.project_id, "peer").await;
        sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&peer.id).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,track) VALUES('rr-y',?,?,'cy','p','[]','pending',?,'billing')")
            .bind(&peer.id).bind(&peer.id).bind(crate::db::now()).execute(&app.db).await.unwrap();
        let res = list(State(app), Path(env.project_id.clone()), Query(PeersQuery { for_track: Some("billing".into()) }), Extension(RequestPrincipal::Bot(here.id.clone()))).await.unwrap().0;
        let c = res["candidates"].as_array().unwrap().iter().find(|c| c["id"] == here.id).unwrap().clone();
        assert!(c["reasons"].as_array().unwrap().iter().any(|r| r.as_str().unwrap().contains("進行中")), "{c}");
    }
}

#[cfg(test)]
mod spawn_gate_tests {
    use super::*;
    #[tokio::test]
    async fn old_spawn_begin_refuses_expert_executor() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "executor").await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("X-AM-Bot-Token", bot.hook_token.parse().unwrap());
        let body = crate::runners::credential_spawn::SpawnBegin { bot_id: bot.id, timeout_ms: String::new(), purpose: String::new(), pane_id: String::new(), child_name: String::new() };
        let (status, _) = crate::runners::credential_spawn::begin(State(env.app.clone()), headers, axum::extract::Form(body)).await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }
}

#[cfg(test)]
mod specialist_peers_tests {
    use super::*;

    /// 專家的候選規則：同 track 的專家排第一（理由「專家（同 track）」）；其他 track 的專家不是候選；
    /// 沒給 --for 時專家排在執行者之後。
    #[tokio::test]
    async fn same_track_specialist_leads_other_tracks_drop_out_and_specialists_sort_last() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
        let exec = crate::testing::claude_bot(&app, &env.project_id, "exec").await;
        sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&exec.id).execute(&app.db).await.unwrap();
        let mine = crate::testing::claude_bot(&app, &env.project_id, "spec-infra").await;
        sqlx::query("UPDATE bots SET role='specialist', track='infra' WHERE id=?").bind(&mine.id).execute(&app.db).await.unwrap();
        let other = crate::testing::claude_bot(&app, &env.project_id, "spec-ui").await;
        sqlx::query("UPDATE bots SET role='specialist', track='ui' WHERE id=?").bind(&other.id).execute(&app.db).await.unwrap();

        let plain = list(State(app.clone()), Path(env.project_id.clone()), Query(PeersQuery::default()), Extension(RequestPrincipal::User)).await.unwrap().0;
        let ids: Vec<&str> = plain["peers"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
        let pos = |id: &str| ids.iter().position(|x| *x == id).unwrap();
        assert!(pos(&exec.id) < pos(&mine.id) && pos(&exec.id) < pos(&other.id), "專家排在執行者之後：{plain}");

        let res = list(State(app.clone()), Path(env.project_id.clone()), Query(PeersQuery { for_track: Some("infra".into()) }), Extension(RequestPrincipal::User)).await.unwrap().0;
        let cands = res["candidates"].as_array().unwrap();
        assert_eq!(cands[0]["id"], mine.id, "同 track 的專家排第一：{res}");
        assert!(cands[0]["reasons"].as_array().unwrap().iter().any(|r| r == "專家（同 track）"));
        assert!(cands.iter().all(|c| c["id"] != other.id), "其他 track 的專家不當候選：{res}");
    }
}

#[cfg(test)]
mod fork_suggestion_tests {
    use super::*;

    /// 同 track（最近一筆 relay 的 track）的 codex 執行者正忙、而且有 session：`fork_suggestion` 指給它；閒著就不指。
    #[tokio::test]
    async fn a_busy_same_track_codex_with_a_session_is_suggested_as_a_fork_source() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
        let codex = crate::testing::claude_bot(&app, &env.project_id, "codex-exec").await;
        sqlx::query("UPDATE bots SET kind='codex', role='executor' WHERE id=?").bind(&codex.id).execute(&app.db).await.unwrap();
        // 有一段已結束、記到 session 的 run（fork 的來源）。
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at) VALUES (?,?,'stopped','idle','sid-c','2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')")
            .bind(crate::db::ulid()).bind(&codex.id).execute(&app.db).await.unwrap();
        // 目前正在跑的 run：忙。
        let run = crate::testing::fake_run(&app, &codex.id).await;
        sqlx::query("UPDATE runs SET state='running', agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,track) VALUES('rr-fork',?,?,'c-fork','p','[]','delivered',?,'infra')")
            .bind(&codex.id).bind(&codex.id).bind(crate::db::now()).execute(&app.db).await.unwrap();

        let query = || PeersQuery { for_track: Some("infra".into()) };
        let busy = list(State(app.clone()), Path(env.project_id.clone()), Query(query()), Extension(RequestPrincipal::User)).await.unwrap().0;
        assert_eq!(busy["fork_suggestion"]["source_bot_id"], codex.id, "忙的同 track codex 有 session：提示它：{busy}");

        // 閒著：只是候選，不再提示 fork。
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let idle = list(State(app.clone()), Path(env.project_id.clone()), Query(query()), Extension(RequestPrincipal::User)).await.unwrap().0;
        assert!(idle["fork_suggestion"].is_null(), "閒著不提示：{idle}");
    }
}

#[cfg(test)]
mod granted_peers_tests {
    use super::*;

    /// 專家視角：授權給自己的執行者標 granted、可派；未授權的執行者 eligible=false「未授權（需經管理者）」；管理者視角不變。
    #[tokio::test]
    async fn a_specialist_sees_only_its_granted_executors_as_dispatchable() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
        let mut ids = Vec::new();
        for (name, role, track) in [("gp-mgr", "manager", None), ("gp-spec", "specialist", Some("db")), ("gp-e1", "executor", None), ("gp-e2", "executor", None)] {
            let b = crate::testing::claude_bot(&app, &env.project_id, name).await;
            sqlx::query("UPDATE bots SET role=?, track=? WHERE id=?").bind(role).bind(track).bind(&b.id).execute(&app.db).await.unwrap();
            let run = crate::testing::fake_run(&app, &b.id).await;
            sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:{role}", env.project_id)).bind(run).execute(&app.db).await.unwrap();
            ids.push(b.id);
        }
        let (mgr, spec, e1, e2) = (&ids[0], &ids[1], &ids[2], &ids[3]);
        let grants = json!([{"specialist": spec, "executor": e1, "track": "db", "granted_by": "user", "granted_at": "2026-11-06T00:00:00Z"}]).to_string();
        sqlx::query("UPDATE projects SET executor_grants_json=? WHERE id=?").bind(grants).bind(&env.project_id).execute(&app.db).await.unwrap();
        let call = |who: &str, track: Option<&str>| list(State(app.clone()), Path(env.project_id.clone()), Query(PeersQuery { for_track: track.map(str::to_string) }), Extension(RequestPrincipal::Bot(who.to_string())));

        let plain = call(spec, None).await.unwrap().0;
        let peer = |id: &str| plain["peers"].as_array().unwrap().iter().find(|p| p["id"] == id).unwrap().clone();
        assert_eq!(peer(e1)["granted"], true, "{plain}");
        assert_eq!(peer(e2)["granted"], false);
        assert_eq!(peer(e2)["dispatch"], "via_manager");
        assert_eq!(plain["executor_grants"][0]["executor"], e1.as_str());

        let res = call(spec, Some("db")).await.unwrap().0;
        let cand = |id: &str| res["candidates"].as_array().unwrap().iter().find(|c| c["id"] == id).cloned();
        assert!(cand(spec).is_none(), "專家不把自己列成候選：{res}");
        let c1 = cand(e1).unwrap();
        assert_eq!(c1["granted"], true);
        assert!(c1["reasons"].as_array().unwrap().iter().any(|r| r == "已授權給你"), "{c1}");
        let c2 = cand(e2).unwrap();
        assert_eq!((c2["eligible"].as_bool(), c2["granted"].as_bool()), (Some(false), Some(false)));
        assert!(c2["reasons"].as_array().unwrap().iter().any(|r| r == "未授權（需經管理者）"), "{c2}");

        // 管理者視角不變：沒有 granted 欄、不因授權擋候選。
        let m = call(mgr, Some("db")).await.unwrap().0;
        for c in m["candidates"].as_array().unwrap() {
            assert!(c.get("granted").is_none(), "{c}");
            assert!(!c["reasons"].as_array().unwrap().iter().any(|r| r == "未授權（需經管理者）"), "{c}");
        }
        let mp = call(mgr, None).await.unwrap().0;
        assert!(mp["peers"].as_array().unwrap().iter().all(|p| p.get("granted").is_none()), "{mp}");

        // 專家換 track：授權不再算數。
        sqlx::query("UPDATE bots SET track='ui' WHERE id=?").bind(spec).execute(&app.db).await.unwrap();
        let plain = call(spec, None).await.unwrap().0;
        assert!(plain["peers"].as_array().unwrap().iter().filter(|p| p["role"] == "executor").all(|p| p["granted"] == false), "{plain}");
    }
}

/// Astra 第二輪 #5：管理者把 track 交給專家 → 專家查 `peers --for <track>` 能挑授權給它的執行者，
/// 自己派出的子任務不互相衝突；別人在同 track 的工作照樣擋；代班中的專家拿管理者視角。
#[cfg(test)]
mod coordinator_peers_tests {
    use super::*;

    async fn member(env: &crate::testing::Env, name: &str, role: &str, track: Option<&str>) -> String {
        let b = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET role=?, track=? WHERE id=?").bind(role).bind(track).bind(&b.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &b.id).await;
        sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:{role}", env.project_id)).bind(run).execute(&env.app.db).await.unwrap();
        b.id
    }

    async fn relay(env: &crate::testing::Env, id: &str, from: &str, to: &str, track: &str) {
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,track) VALUES(?,?,?,?,'p','[]','delivered',?,?)")
            .bind(id).bind(from).bind(to).bind(id).bind(crate::db::now()).bind(track).execute(&env.app.db).await.unwrap();
    }

    fn cand(v: &Value, id: &str) -> Value {
        v["candidates"].as_array().unwrap().iter().find(|c| c["id"] == id).cloned().unwrap_or_else(|| panic!("{id} 不在候選：{v}"))
    }

    fn has(c: &Value, reason: &str) -> bool { c["reasons"].as_array().unwrap().iter().any(|r| r.as_str().is_some_and(|r| r.starts_with(reason))) }

    #[tokio::test]
    async fn a_specialist_coordinating_its_track_can_pick_its_granted_executors() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
        let mgr = member(&env, "cp-mgr", "manager", None).await;
        let spec = member(&env, "cp-spec", "specialist", Some("db")).await;
        let e1 = member(&env, "cp-e1", "executor", None).await;
        let e2 = member(&env, "cp-e2", "executor", None).await;
        let e3 = member(&env, "cp-e3", "executor", None).await;
        let grants = json!([
            {"specialist": spec, "executor": e1, "track": "db", "granted_by": "user", "granted_at": "2026-11-06T00:00:00Z"},
            {"specialist": spec, "executor": e2, "track": "db", "granted_by": "user", "granted_at": "2026-11-06T00:00:00Z"},
        ]).to_string();
        sqlx::query("UPDATE projects SET executor_grants_json=? WHERE id=?").bind(grants).bind(&env.project_id).execute(&app.db).await.unwrap();
        let peers = |who: &str| list(State(app.clone()), Path(env.project_id.clone()), Query(PeersQuery { for_track: Some("db".into()) }), Extension(RequestPrincipal::Bot(who.to_string())));

        // 管理者 --track db --expect-result 交給專家：active holder 是專家本人。
        relay(&env, "cp-1", &mgr, &spec, "db").await;
        let v = peers(&spec).await.unwrap().0;
        for e in [&e1, &e2] {
            let c = cand(&v, e);
            assert_eq!((c["eligible"].as_bool(), c["granted"].as_bool()), (Some(true), Some(true)), "專家統籌自己的 track 時授權執行者可派：{c}");
            assert!(!has(&c, "track 已由"), "{c}");
        }
        let c3 = cand(&v, &e3);
        assert_eq!(c3["eligible"], false);
        assert!(has(&c3, "未授權（需經管理者）"), "{c3}");
        // 管理者視角不變：track 由專家進行中，其他執行者照舊被擋。
        let m = peers(&mgr).await.unwrap().0;
        assert!(has(&cand(&m, &e1), "track 已由 cp-spec 進行中"), "{m}");

        // 專家把子任務派給 e1（同 track）：e2 仍可派，不因自己的子任務互相衝突。
        relay(&env, "cp-2", &spec, &e1, "db").await;
        let v = peers(&spec).await.unwrap().0;
        assert_eq!(cand(&v, &e2)["eligible"], true, "{v}");
        assert_eq!(cand(&v, &e1)["eligible"], true, "{v}");

        // 別人（管理者）在同 track 把工作交給 e3：專家視角也要擋。
        relay(&env, "cp-3", &mgr, &e3, "db").await;
        let v = peers(&spec).await.unwrap().0;
        for e in [&e1, &e2] { assert!(has(&cand(&v, e), "track 已由 cp-e3 進行中"), "{v}"); }
    }

    #[tokio::test]
    async fn an_acting_deputy_specialist_gets_the_manager_view() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
        let mgr = member(&env, "ad-mgr", "manager", None).await;
        let spec = member(&env, "ad-spec", "specialist", Some("db")).await;
        let e1 = member(&env, "ad-e1", "executor", None).await;
        let call = |track: Option<&str>| list(State(app.clone()), Path(env.project_id.clone()), Query(PeersQuery { for_track: track.map(str::to_string) }), Extension(RequestPrincipal::Bot(spec.clone())));
        // 不在代班：e1 沒授權 → 需經管理者。
        let v = call(Some("db")).await.unwrap().0;
        assert!(has(&cand(&v, &e1), "未授權（需經管理者）"), "{v}");
        assert!(v.get("acting_deputy").is_none());
        // 代班中（副管理者＋有改派來的未結請求）：管理者視角，不標授權、不擋未授權執行者，也不列自己。
        sqlx::query("UPDATE projects SET deputy_bot_id=?, deputy_after_sec=60 WHERE id=?").bind(&spec).bind(&env.project_id).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,redirected_from,redirected_at) VALUES('ad-r',?,?,'ad-r','q','[]','delivered',?,?,?)")
            .bind(&e1).bind(&spec).bind(crate::db::now()).bind(&mgr).bind(crate::db::now()).execute(&app.db).await.unwrap();
        let v = call(Some("db")).await.unwrap().0;
        assert_eq!(v["acting_deputy"], true, "{v}");
        let c = cand(&v, &e1);
        assert!(c.get("granted").is_none() && !has(&c, "未授權"), "{c}");
        assert!(v["candidates"].as_array().unwrap().iter().all(|c| c["id"] != spec.as_str()), "{v}");
        let plain = call(None).await.unwrap().0;
        assert_eq!(plain["acting_deputy"], true);
        assert!(plain["peers"].as_array().unwrap().iter().all(|p| p.get("granted").is_none()), "{plain}");
    }
}
