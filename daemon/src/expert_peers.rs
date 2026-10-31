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
    match principal {
        RequestPrincipal::User => {},
        RequestPrincipal::Bot(id) | RequestPrincipal::Child { child_id: id, .. } => {
            let bot = crate::db::bot(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::Forbidden(json!({"error":"forbidden"})))?;
            if bot.project_id != project_id { return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"cross_project"}))); }
        }
        _ => return Err(LcError::Forbidden(json!({"error":"forbidden"}))),
    }
    let project = crate::db::project(&app.db, &project_id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("project".into()))?;
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
        peers.push(json!({"id":bot.id,"name":bot.name,"role":bot.role,"model":bot.model,"host":project.host,"run_state":run.as_ref().map(|r| r.state.as_str()).unwrap_or("stopped"),"agent_status":status,"observed_at":observed_at,"status_stale":true,"open_requests":open}));
    }
    let Some(for_track) = query.for_track.as_deref() else {
        return Ok(Json(json!({"project_id":project_id,"peers":peers})));
    };

    // Candidates evaluation for `for_track`
    let active_track_holder: Option<String> = sqlx::query_scalar(
        "SELECT to_bot_id FROM relay_requests WHERE track=? AND state IN ('pending','delivered') ORDER BY created_at DESC LIMIT 1"
    ).bind(for_track).fetch_optional(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;

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
    }

    let mut candidates = Vec::new();
    for bot in bots.into_iter().filter(|b| b.project_id == project_id && b.role.as_deref() == Some("executor")) {
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

        // 排序分數計算
        let mut score: i64 = 0;

        // 最近一筆 relay_requests 的 track 相同者最優先（理由「同 track」）
        let last_req_track: Option<Option<String>> = sqlx::query_scalar(
            "SELECT track FROM relay_requests WHERE to_bot_id=? ORDER BY created_at DESC LIMIT 1"
        ).bind(&bot.id).fetch_optional(&app.db).await.ok().flatten();
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

        candidates.push(Candidate {
            bot_id: bot.id,
            name: bot.name,
            role: bot.role,
            model: bot.model,
            eligible,
            reasons,
            score,
            rank_tier: rank,
        });
    }

    candidates.sort_by(|a, b| {
        b.eligible.cmp(&a.eligible)
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.rank_tier.cmp(&b.rank_tier))
            .then_with(|| a.bot_id.cmp(&b.bot_id))
    });

    let cand_json: Vec<Value> = candidates.into_iter().map(|c| json!({
        "id": c.bot_id,
        "name": c.name,
        "role": c.role,
        "model": c.model,
        "eligible": c.eligible,
        "score": c.score,
        "reasons": c.reasons,
    })).collect();

    Ok(Json(json!({"project_id":project_id,"candidates":cand_json})))
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
