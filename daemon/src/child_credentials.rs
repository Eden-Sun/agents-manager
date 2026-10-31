//! Authenticate scoped credentials issued to spawned children.
use std::sync::Arc;
use sha2::{Digest, Sha256};
use axum::http::StatusCode;
use crate::state::App;

pub(crate) fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub(crate) struct ChildPrincipal {
    pub child_id: String,
    pub parent_id: String,
}

pub(crate) async fn authenticate(app: &Arc<App>, id: &str, token: &str) -> Result<ChildPrincipal, StatusCode> {
    let row: Option<(String, String, String, String, String, String, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT token_hash,parent_bot_id,state,expires_at,host,herdr_session,child_bot_id,run_id,pane_id FROM child_credentials WHERE spawn_id=?"
    ).bind(id).fetch_optional(&app.db).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let Some((hash, parent_id, state, expires_at, host, session, child_id, run_id, pane_id)) = row else { return Err(StatusCode::UNAUTHORIZED) };
    if token.is_empty() || !crate::agent_relay::ct_eq(&hash, &hash_token(token)) { return Err(StatusCode::UNAUTHORIZED); }
    if state == "pending" {
        if crate::db::cmp_ts(&expires_at, &crate::db::now()).is_gt() { return Err(StatusCode::CONFLICT); }
        sqlx::query("UPDATE child_credentials SET state='revoked' WHERE spawn_id=? AND state='pending'").bind(id).execute(&app.db).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        return Err(StatusCode::UNAUTHORIZED);
    }
    if state != "active" { return Err(StatusCode::UNAUTHORIZED); }
    let (Some(child_id), Some(run_id)) = (child_id, run_id) else { return Err(StatusCode::UNAUTHORIZED) };
    let parent = crate::db::bot(&app.db, &parent_id).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if parent.is_none_or(|p| p.deleted_at.is_some()) { return Err(StatusCode::UNAUTHORIZED); }
    let child = crate::db::bot(&app.db, &child_id).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?.filter(|b| b.deleted_at.is_none() && b.managed_by == "child" && b.parent_bot_id.as_deref() == Some(parent_id.as_str())).ok_or(StatusCode::UNAUTHORIZED)?;
    let current_host = crate::db::bot_host(&app.db, &child_id).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if current_host != host {
        sqlx::query("UPDATE child_credentials SET state='revoked' WHERE spawn_id=?").bind(id).execute(&app.db).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        return Err(StatusCode::UNAUTHORIZED);
    }
    let run = crate::db::active_run(&app.db, &child_id).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?.filter(|r| r.id == run_id && r.pane_id.as_deref() == Some(pane_id.as_str()) && r.herdr_session.as_deref() == Some(session.as_str())).ok_or(StatusCode::UNAUTHORIZED)?;
    // A recycled pane/name is not proof that this is still the same child instance.
    if run.terminal_id.as_deref().filter(|v| !v.is_empty()).is_none() && run.native_session_id.as_deref().filter(|v| !v.is_empty()).is_none() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let fence = app.hosts.fence(&host).await.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let client = app.herdr_for_host_fence(&fence, &session).await.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let agents = tokio::time::timeout(std::time::Duration::from_secs(3), client.agent_list()).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if !fence.is_current_now() { return Err(StatusCode::SERVICE_UNAVAILABLE); }
    let mut matches = agents.iter().filter(|a| a.pane_id == pane_id);
    let observed = matches.next();
    if matches.next().is_some() { return Err(StatusCode::SERVICE_UNAVAILABLE); }
    let mut mismatch = observed.is_none();
    let mut proven = false;
    if let Some(agent) = observed {
        let native = crate::lifecycle::dead_panes::herdr_session_id(agent);
        for (expected, actual) in [
            (run.terminal_id.as_deref(), agent.terminal_id.as_deref()),
            (run.native_session_id.as_deref(), native.as_deref()),
        ] {
            if let (Some(expected), Some(actual)) = (expected.filter(|s| !s.is_empty()), actual.filter(|s| !s.is_empty())) {
                if expected == actual { proven = true; } else { mismatch = true; }
            }
        }
    }
    if mismatch {
        sqlx::query("UPDATE child_credentials SET state='revoked' WHERE spawn_id=?").bind(id).execute(&app.db).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !proven { return Err(StatusCode::SERVICE_UNAVAILABLE); }
    Ok(ChildPrincipal { child_id: child.id, parent_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::{Request, StatusCode}};
    use tower::ServiceExt;

    #[tokio::test]
    async fn child_scope_expiry_and_revocation_are_enforced_by_router() {
        let e = crate::testing::env().await;
        let parent = crate::testing::claude_bot(&e.app, &e.project_id, "credential-parent").await;
        let child = crate::testing::claude_bot(&e.app, &e.project_id, "credential-child").await;
        sqlx::query("UPDATE bots SET managed_by='child',parent_bot_id=? WHERE id=?").bind(&parent.id).bind(&child.id).execute(&e.app.db).await.unwrap();
        let run_id = crate::testing::fake_run(&e.app, &child.id).await;
        let pane = format!("pane-{}", child.id);
        let name = "credential-parent-child";
        e.herdr.set_agent(name, &pane, true);
        sqlx::query("UPDATE runs SET native_session_id='sess-1' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();
        let spawn = crate::db::ulid();
        let token = crate::projection::new_token();
        sqlx::query("INSERT INTO child_credentials(spawn_id,token_hash,parent_bot_id,child_name,host,herdr_session,pane_id,state,created_at,expires_at) VALUES(?,?,?,?,'local','test',?,'pending',?,?)")
            .bind(&spawn).bind(hash_token(&token)).bind(&parent.id).bind(name).bind(&pane).bind(crate::db::now()).bind(crate::db::iso_in(900)).execute(&e.app.db).await.unwrap();
        let request = |method: &str, path: &str| Request::builder().method(method).uri(path).header("host", "127.0.0.1:7799").header("X-AM-Child-Spawn-Id", &spawn).header("X-AM-Child-Token", &token).header("content-type", "application/json").body(Body::from("{}" )).unwrap();
        let router = crate::api::router(e.app.clone());
        let pending = router.clone().oneshot(request("GET", "/api/state")).await.unwrap();
        assert_eq!(pending.status(), StatusCode::CONFLICT);
        let mut mixed = request("GET", "/api/state");
        mixed.headers_mut().insert("X-AM-Bot-Token", "tok".parse().unwrap());
        assert_eq!(router.clone().oneshot(mixed).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        sqlx::query("UPDATE child_credentials SET state='active',child_bot_id=?,run_id=? WHERE spawn_id=?").bind(&child.id).bind(&run_id).bind(&spawn).execute(&e.app.db).await.unwrap();
        let active = router.clone().oneshot(request("GET", "/api/state")).await.unwrap();
        assert_eq!(active.status(), StatusCode::OK);
        sqlx::query("UPDATE runs SET native_session_id=NULL WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();
        let unknown = router.clone().oneshot(request("GET", "/api/state")).await.unwrap();
        assert_eq!(unknown.status(), StatusCode::SERVICE_UNAVAILABLE);
        sqlx::query("UPDATE runs SET native_session_id='sess-1' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();
        let mut spoof = request("POST", &format!("/api/bots/{}/prompt", parent.id));
        *spoof.body_mut() = Body::from(format!(r#"{{"text":"hello","relay_from":"{}"}}"#, parent.id));
        assert_eq!(router.clone().oneshot(spoof).await.unwrap().status(), StatusCode::FORBIDDEN);
        let other = router.clone().oneshot(request("POST", &format!("/api/bots/{}/prompt", child.id))).await.unwrap();
        assert_eq!(other.status(), StatusCode::FORBIDDEN);
        let spawn_attempt = router.clone().oneshot(request("POST", &format!("/api/bots/{}/start", child.id))).await.unwrap();
        assert_eq!(spawn_attempt.status(), StatusCode::FORBIDDEN);
        e.herdr.agents.lock().unwrap().clear();
        let gone = router.clone().oneshot(request("GET", "/api/state")).await.unwrap();
        assert_eq!(gone.status(), StatusCode::UNAUTHORIZED);
        let state: String = sqlx::query_scalar("SELECT state FROM child_credentials WHERE spawn_id=?").bind(&spawn).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(state, "revoked");
        // Separately exercise the run-close trigger.
        sqlx::query("UPDATE child_credentials SET state='active' WHERE spawn_id=?").bind(&spawn).execute(&e.app.db).await.unwrap();
        e.herdr.set_agent(name, &pane, true);
        sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();
        let revoked = router.clone().oneshot(request("GET", "/api/state")).await.unwrap();
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
        sqlx::query("UPDATE child_credentials SET state='active' WHERE spawn_id=?").bind(&spawn).execute(&e.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET managed_by='user',parent_bot_id=NULL WHERE id=?").bind(&child.id).execute(&e.app.db).await.unwrap();
        let unlinked: String = sqlx::query_scalar("SELECT state FROM child_credentials WHERE spawn_id=?").bind(&spawn).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(unlinked, "revoked");
        sqlx::query("UPDATE child_credentials SET state='pending',expires_at=? WHERE spawn_id=?").bind(crate::db::iso_in(-1)).bind(&spawn).execute(&e.app.db).await.unwrap();
        let expired = router.oneshot(request("GET", "/api/state")).await.unwrap();
        assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);
    }
}
