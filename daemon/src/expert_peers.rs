use std::sync::Arc;
use axum::{extract::{Path, State}, Extension, Json};
use serde_json::{json, Value};
use crate::{api::RequestPrincipal, lifecycle::LcError, state::App};

pub(crate) async fn list(State(app): State<Arc<App>>, Path(project_id): Path<String>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    match principal {
        RequestPrincipal::User => {},
        RequestPrincipal::Bot(id) => {
            let bot = crate::db::bot(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::Forbidden(json!({"error":"forbidden"})))?;
            if bot.project_id != project_id { return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"cross_project"}))); }
        }
        _ => return Err(LcError::Forbidden(json!({"error":"forbidden"}))),
    }
    let project = crate::db::project(&app.db, &project_id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("project".into()))?;
    let bots = crate::db::live_bots(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let mut peers = Vec::new();
    for bot in bots.into_iter().filter(|b| b.project_id == project_id) {
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
    Ok(Json(json!({"project_id":project_id,"peers":peers})))
}

pub(crate) async fn spawn_authority(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    let RequestPrincipal::Bot(id) = principal else { return Err(LcError::Forbidden(json!({"error":"forbidden"}))); };
    let bot = crate::db::bot(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::Forbidden(json!({"error":"forbidden"})))?;
    let project = crate::db::project(&app.db, &bot.project_id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::Forbidden(json!({"error":"forbidden"})))?;
    if bot.parent_bot_id.is_some() || bot.managed_by == "child" { return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"child_cannot_spawn"}))); }
    if project.mode == "expert" {
        let run = crate::db::active_run(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?;
        let policy = format!("{}:{}:{}:{}", project.id, project.host, project.mode, bot.role.as_deref().unwrap_or(""));
        if bot.role.as_deref() != Some("manager") || run.as_ref().and_then(|r| r.runtime_role_policy.as_deref()) != Some(policy.as_str()) { return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"role_not_applied"}))); }
    }
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
        let value = list(State(app.clone()), Path(env.project_id.clone()), Extension(RequestPrincipal::Bot(bot.id.clone()))).await.unwrap().0;
        assert!(value["peers"].as_array().unwrap().iter().any(|p| p["id"] == bot.id));
        let other = crate::db::ulid();
        sqlx::query("INSERT INTO projects(id,path,label,created_at) VALUES (?,?,?,?)").bind(&other).bind(format!("/tmp/{other}")).bind("other").bind(crate::db::now()).execute(&app.db).await.unwrap();
        assert!(matches!(list(State(app), Path(other), Extension(RequestPrincipal::Bot(bot.id))).await, Err(LcError::Forbidden(_))));
    }

    #[tokio::test]
    async fn expert_spawn_requires_current_manager_policy_and_never_allows_child() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "peer").await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        assert!(matches!(spawn_authority(State(app.clone()), Extension(RequestPrincipal::Bot(bot.id.clone()))).await, Err(LcError::Forbidden(_))));
        sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run = crate::testing::fake_run(&app, &bot.id).await;
        let policy = format!("{}:local:expert:manager", env.project_id);
        sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(&policy).bind(&run).execute(&app.db).await.unwrap();
        assert!(spawn_authority(State(app.clone()), Extension(RequestPrincipal::Bot(bot.id.clone()))).await.is_ok());
        sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        assert!(spawn_authority(State(app.clone()), Extension(RequestPrincipal::Bot(bot.id.clone()))).await.is_err());
        sqlx::query("UPDATE bots SET role='manager', managed_by='child' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        assert!(matches!(spawn_authority(State(app), Extension(RequestPrincipal::Bot(bot.id))).await, Err(LcError::Forbidden(_))));
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
        let body = crate::runners::credential_spawn::SpawnBegin { bot_id: bot.id, timeout_ms: String::new(), purpose: String::new() };
        let (status, _) = crate::runners::credential_spawn::begin(State(env.app.clone()), headers, axum::extract::Form(body)).await;
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }
}
