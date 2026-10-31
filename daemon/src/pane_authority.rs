//! Authorize managed pane input using daemon-owned identity and live herdr state.
use crate::{api::RequestPrincipal, db, lifecycle::LcError, state::App};
use axum::{extract::State, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Deserialize)]
pub(crate) struct PaneAuthority {
    pub kind: String,
    pub target: Option<String>,
    pub session: Option<String>,
    pub machine: Option<String>,
}

/// Authenticated caller; child credentials are connected by the child principal slice.
pub(crate) struct PaneCaller {
    pub bot_id: String,
    pub is_child_principal: bool,
}

pub(crate) async fn authorize(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(input): Json<PaneAuthority>,
) -> Result<Json<Value>, LcError> {
    let RequestPrincipal::Bot(bot_id) = principal else {
        return Err(LcError::Forbidden(json!({"error":"forbidden"})));
    };
    authorize_caller(
        app,
        PaneCaller {
            bot_id,
            is_child_principal: false,
        },
        input,
    )
    .await
}

pub(crate) async fn authorize_caller(
    app: Arc<App>,
    caller: PaneCaller,
    input: PaneAuthority,
) -> Result<Json<Value>, LcError> {
    let id = caller.bot_id;
    let bot = db::bot(&app.db, &id)
        .await
        .map_err(up)?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::Forbidden(json!({"error":"bot_unavailable"})))?;
    let project = db::project(&app.db, &bot.project_id)
        .await
        .map_err(up)?
        .filter(|p| p.deleted_at.is_none() && p.handed_off_to.is_none())
        .ok_or_else(|| LcError::Forbidden(json!({"error":"project_unavailable"})))?;
    let run = db::active_run(&app.db, &id).await.map_err(up)?;
    if let Some(run) = &run {
        let run_host: Option<String> = sqlx::query_scalar("SELECT host FROM runs WHERE id=?")
            .bind(&run.id)
            .fetch_one(&app.db)
            .await
            .map_err(up)?;
        if run_host.as_deref().is_some_and(|host| host != project.host) {
            return Err(LcError::Forbidden(json!({"error":"run_host_mismatch"})));
        }
    }
    let fallback = app
        .session_for_bot(&bot, &project.host)
        .await
        .ok_or_else(|| LcError::Upstream("herdr session unavailable".into()))?;
    let session = run
        .as_ref()
        .and_then(|r| r.herdr_session.as_deref())
        .unwrap_or(&fallback);
    if input
        .session
        .as_deref()
        .is_some_and(|value| value != session)
        || input
            .machine
            .as_deref()
            .is_some_and(|value| value != project.host)
    {
        return Err(LcError::Forbidden(
            json!({"error":"herdr_context_mismatch"}),
        ));
    }
    if input.kind == "context" {
        return Ok(Json(json!({"allowed":true})));
    }
    if !matches!(input.kind.as_str(), "pane" | "agent") {
        return Err(LcError::Bad("unknown pane selector kind".into()));
    }
    let selector = input
        .target
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| LcError::Bad("target required".into()))?;
    let client = app
        .herdr_for_session(&project.host, session)
        .await
        .ok_or_else(|| LcError::Upstream("herdr session unavailable".into()))?;
    // A name is resolved by daemon, and the returned pane id is the only transport target.
    let pane_id = if input.kind == "agent" {
        client
            .agent_get(selector)
            .await
            .map_err(up)?
            .ok_or_else(|| LcError::NotFound("agent".into()))?
            .pane_id
    } else {
        selector.to_string()
    };
    let pane = client
        .pane_get(&pane_id)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("pane".into()))?;
    let agents = client.agent_list().await.map_err(up)?;
    let own = run.as_ref().and_then(|r| r.pane_id.as_deref()) == Some(pane_id.as_str());
    let child = if caller.is_child_principal || bot.managed_by == "child" {
        false
    } else {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM bots b JOIN runs r ON r.bot_id=b.id JOIN projects p ON p.id=b.project_id WHERE b.deleted_at IS NULL AND b.managed_by='child' AND b.parent_bot_id=? AND r.state IN ('starting','running','stopping') AND r.pane_id=? AND COALESCE(r.host,p.host)=? AND COALESCE(r.herdr_session,?)=?)")
            .bind(&id).bind(&pane_id).bind(&project.host).bind(&fallback).bind(session).fetch_one(&app.db).await.map_err(up)?
    };
    let registered: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs r JOIN bots b ON b.id=r.bot_id JOIN projects p ON p.id=b.project_id WHERE r.state IN ('starting','running','stopping') AND COALESCE(r.host,p.host)=? AND COALESCE(r.herdr_session,?)=? AND r.pane_id=?)")
        .bind(&project.host).bind(&fallback).bind(session).bind(&pane_id).fetch_one(&app.db).await.map_err(up)?;
    let is_agent = pane.agent.as_ref().is_some_and(|v| !v.is_empty())
        || agents.iter().any(|a| a.pane_id == pane_id)
        || registered;
    if is_agent && !own && !child {
        return Err(LcError::Forbidden(
            json!({"error":"agent_pane_forbidden","message":"請改用 herdr agent prompt"}),
        ));
    }
    Ok(Json(json!({"allowed":true,"pane_id":pane_id})))
}
fn up(e: impl std::fmt::Display) -> LcError {
    LcError::Upstream(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn request(
        app: &Arc<App>,
        id: &str,
        kind: &str,
        target: Option<&str>,
        session: Option<&str>,
        machine: Option<&str>,
    ) -> Result<Json<Value>, LcError> {
        authorize(
            State(app.clone()),
            Extension(RequestPrincipal::Bot(id.into())),
            Json(PaneAuthority {
                kind: kind.into(),
                target: target.map(str::to_string),
                session: session.map(str::to_string),
                machine: machine.map(str::to_string),
            }),
        )
        .await
    }

    #[tokio::test]
    async fn own_child_and_shell_are_allowed_other_agents_are_denied() {
        let e = crate::testing::env().await;
        let parent = crate::testing::claude_bot(&e.app, &e.project_id, "pane-parent").await;
        let child = crate::testing::claude_bot(&e.app, &e.project_id, "pane-child").await;
        let peer = crate::testing::claude_bot(&e.app, &e.project_id, "pane-peer").await;
        sqlx::query("UPDATE bots SET managed_by='child',parent_bot_id=? WHERE id=?")
            .bind(&parent.id)
            .bind(&child.id)
            .execute(&e.app.db)
            .await
            .unwrap();
        for bot in [&parent, &child, &peer] {
            crate::testing::fake_run(&e.app, &bot.id).await;
        }
        let own = format!("pane-{}", parent.id);
        let kid = format!("pane-{}", child.id);
        let other = format!("pane-{}", peer.id);
        e.herdr.set_agent("parent", &own, false);
        e.herdr.set_agent("kid", &kid, false);
        e.herdr.set_agent("peer", &other, false);
        e.herdr.set_agent("unadopted", "pane-unadopted", false);
        e.herdr.live_pane(
            "shell-pane",
            crate::testing::LivePane {
                rows: Some(24),
                ..Default::default()
            },
        );
        for target in [&own, &kid, &"shell-pane".to_string()] {
            assert!(
                request(&e.app, &parent.id, "pane", Some(target), None, None)
                    .await
                    .is_ok(),
                "{target}"
            );
        }
        for target in [&other, &"pane-unadopted".to_string()] {
            assert!(
                matches!(
                    request(&e.app, &parent.id, "pane", Some(target), None, None).await,
                    Err(LcError::Forbidden(_))
                ),
                "{target}"
            );
        }
        let Json(result) = request(&e.app, &parent.id, "agent", Some("kid"), None, None)
            .await
            .unwrap();
        assert_eq!(result["pane_id"], kid);
        assert!(matches!(
            request(&e.app, &parent.id, "agent", Some("peer"), None, None).await,
            Err(LcError::Forbidden(_))
        ));
        assert!(matches!(
            request(&e.app, &parent.id, "context", None, Some("other"), None).await,
            Err(LcError::Forbidden(_))
        ));
        assert!(matches!(
            request(&e.app, &parent.id, "context", None, None, Some("other")).await,
            Err(LcError::Forbidden(_))
        ));
        assert!(
            request(&e.app, &parent.id, "pane", Some("missing"), None, None)
                .await
                .is_err()
        );
        sqlx::query("UPDATE runs SET host='other-host' WHERE bot_id=?")
            .bind(&parent.id)
            .execute(&e.app.db)
            .await
            .unwrap();
        assert!(matches!(
            request(&e.app, &parent.id, "pane", Some(&own), None, None).await,
            Err(LcError::Forbidden(_))
        ));
        sqlx::query("UPDATE runs SET host=NULL WHERE bot_id=?")
            .bind(&parent.id)
            .execute(&e.app.db)
            .await
            .unwrap();
        // Even if mapped to a parent bot, a child principal never inherits its child exception.
        for (target, allowed) in [
            (&own, true),
            (&"shell-pane".to_string(), true),
            (&kid, false),
        ] {
            let result = authorize_caller(
                e.app.clone(),
                PaneCaller {
                    bot_id: parent.id.clone(),
                    is_child_principal: true,
                },
                PaneAuthority {
                    kind: "pane".into(),
                    target: Some(target.clone()),
                    session: None,
                    machine: None,
                },
            )
            .await;
            assert_eq!(result.is_ok(), allowed, "{target}");
        }
        // Child principals cannot obtain the parent exception for descendants.
        sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?")
            .bind(&child.id)
            .bind(&peer.id)
            .execute(&e.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET managed_by='child' WHERE id=?")
            .bind(&peer.id)
            .execute(&e.app.db)
            .await
            .unwrap();
        assert!(matches!(
            request(&e.app, &child.id, "pane", Some(&other), None, None).await,
            Err(LcError::Forbidden(_))
        ));
    }
}
