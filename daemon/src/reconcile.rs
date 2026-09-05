//! Reconciliation (SPEC §6.5): runs at daemon start, after every event-stream reconnect,
//! and after a remote host's ssh master comes back (SPEC §11.3.4).
//!
//! Reconciliation is always scoped to **one host** — pane / workspace / agent ids are only
//! unique within a host's herdr session.

use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

/// Reconcile every known host.
#[allow(dead_code)]
pub async fn reconcile(app: &Arc<App>) -> Result<()> {
    let mut first_err = None;
    for host in app.hosts.names().await {
        if let Err(e) = reconcile_host(app, &host).await {
            tracing::error!(host = %host, error = ?e, "reconcile failed");
            if host == LOCAL_HOST && first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub async fn reconcile_host(app: &Arc<App>, host: &str) -> Result<()> {
    let Some(client) = app.herdr_for(host).await else {
        anyhow::bail!("unknown host `{host}`");
    };
    // v4.0: refresh the GitHub-origin cache for this host's projects (off-path).
    crate::github::spawn_detect_host(app.clone(), host.to_string());
    let snapshot = client.snapshot().await?;
    // A1: never reconcile against an empty list. A transient RPC failure would otherwise look
    // like "no agents on this host" and mark every active Run `exited` (failing their turns).
    let agents = client.agent_list().await.map_err(|e| {
        anyhow::anyhow!("agent.list failed on host `{host}`: {e}; skipping reconcile so runs are not falsely exited")
    })?;
    let by_name: HashMap<String, &crate::herdr::AgentInfo> =
        agents.iter().filter_map(|a| a.name.clone().map(|n| (n, a))).collect();

    let live_ws: Vec<String> = snapshot
        .get("workspaces")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|w| w.get("workspace_id").and_then(|s| s.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    let live_panes: Vec<String> = snapshot
        .get("panes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|p| p.get("pane_id").and_then(|s| s.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    let panes_with_agent: Vec<String> = snapshot
        .get("panes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|p| p.get("agent").map(|g| !g.is_null()).unwrap_or(false))
                .filter_map(|p| p.get("pane_id").and_then(|s| s.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Drop workspace mappings that no longer exist.
    for p in db::live_projects(&app.db).await?.into_iter().filter(|p| p.host == host) {
        if let Some(ws) = p.workspace_id.as_deref() {
            if !live_ws.contains(&ws.to_string()) {
                sqlx::query("UPDATE projects SET workspace_id=NULL WHERE id=?").bind(&p.id).execute(&app.db).await?;
                tracing::info!(host, project = %p.label, "workspace disappeared; mapping cleared");
            }
        }
    }

    let bots = db::live_bots_on_host(&app.db, host).await?;
    for bot in bots {
        let lock = app.bot_lock(&bot.id).await;
        let _g = lock.lock().await;
        let active = db::active_run(&app.db, &bot.id).await?;
        // Names this bot may be running under: the run's recorded name, the current
        // `<project>-<bot>` scheme, and the legacy bare bot name (runs from before v3.5).
        let computed = db::agent_name_for_bot(&app.db, &bot).await?;
        let mut candidates: Vec<String> = Vec::new();
        if let Some(r) = &active {
            if let Some(n) = r.agent_name.clone().filter(|s| !s.is_empty()) {
                candidates.push(n);
            }
        }
        let label: String = sqlx::query_scalar("SELECT label FROM projects WHERE id = ?")
            .bind(&bot.project_id)
            .fetch_optional(&app.db)
            .await?
            .unwrap_or_default();
        let legacy = crate::config::agent_name_legacy(&label, &bot.name);
        for n in [computed.clone(), legacy, bot.name.clone()] {
            if !candidates.contains(&n) {
                candidates.push(n);
            }
        }
        let mut found: Option<crate::herdr::AgentInfo> = None;
        let mut found_name: Option<String> = None;
        for n in &candidates {
            if let Some(a) = by_name.get(n) {
                found = Some((*a).clone());
                found_name = Some(n.clone());
                break;
            }
        }
        // The snapshot above was taken *before* this bot's lock was acquired, so a Run that
        // started in the meantime would look dead. Re-check that one agent under the lock.
        if found.is_none() && active.is_some() {
            for n in &candidates {
                if let Some(a) = client.agent_get(n).await.ok().flatten() {
                    tracing::debug!(host, bot = %bot.name, agent = %n, "reconcile: agent appeared after the snapshot");
                    found = Some(a);
                    found_name = Some(n.clone());
                    break;
                }
            }
        }
        match (active, found.as_ref()) {
            (Some(run), Some(agent)) => {
                let agent: &crate::herdr::AgentInfo = agent;
                // Still alive: refresh pane_id (pane move changes it) and status.
                let status = agent.agent_status.normalized().as_str().to_string();
                sqlx::query("UPDATE runs SET pane_id=?, workspace_id=?, agent_status=?, agent_name=COALESCE(?, agent_name), state=CASE WHEN state='starting' THEN 'running' ELSE state END WHERE id=?")
                    .bind(&agent.pane_id)
                    .bind(&agent.workspace_id)
                    .bind(&status)
                    .bind(&found_name)
                    .bind(&run.id)
                    .execute(&app.db)
                    .await?;
                if run.pane_id.as_deref() != Some(agent.pane_id.as_str()) {
                    if let Some(old) = run.pane_id.as_deref() {
                        crate::events::unwatch_pane(app, host, old).await;
                    }
                }
                crate::events::watch_pane(app, host, &agent.pane_id).await;
                tracing::info!(host, bot = %bot.name, run = %run.id, pane = %agent.pane_id, "reconcile: kept active run");
            }
            (Some(run), None) => {
                tracing::info!(host, bot = %bot.name, run = %run.id, "reconcile: agent gone, marking run exited");
                crate::lifecycle::mark_run_exited(app, &run.id, "agent not found during reconcile").await;
            }
            (None, Some(agent)) => {
                let run_id = db::ulid();
                let status = agent.agent_status.normalized().as_str().to_string();
                sqlx::query(
                    "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, adopted, agent_name, started_at)
                     VALUES (?,?,'running',?,?,?,1,?,?)",
                )
                .bind(&run_id)
                .bind(&bot.id)
                .bind(&status)
                .bind(&agent.workspace_id)
                .bind(&agent.pane_id)
                .bind(&found_name)
                .bind(db::now())
                .execute(&app.db)
                .await?;
                // Make sure the project keeps pointing at the workspace the agent lives in.
                sqlx::query("UPDATE projects SET workspace_id=? WHERE id=? AND workspace_id IS NULL")
                    .bind(&agent.workspace_id)
                    .bind(&bot.project_id)
                    .execute(&app.db)
                    .await?;
                crate::events::watch_pane(app, host, &agent.pane_id).await;
                tracing::info!(host, bot = %bot.name, run = %run_id, pane = %agent.pane_id, "reconcile: adopted existing agent");
            }
            (None, None) => {}
        }
        app.emit_bot_status(&bot.id).await;
    }

    // Orphan pane recovery: panes we created for finished runs that no longer host an agent.
    let dead: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT r.pane_id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
         WHERE r.pane_id IS NOT NULL AND p.host = ? AND r.state IN ('exited','stopped')
         AND r.pane_id NOT IN (
           SELECT r2.pane_id FROM runs r2 JOIN bots b2 ON b2.id = r2.bot_id JOIN projects p2 ON p2.id = b2.project_id
           WHERE r2.pane_id IS NOT NULL AND p2.host = ? AND r2.state IN ('starting','running','stopping'))",
    )
    .bind(host)
    .bind(host)
    .fetch_all(&app.db)
    .await?;
    for pane in dead {
        if live_panes.contains(&pane) && !panes_with_agent.contains(&pane) {
            tracing::info!(host, pane_id = %pane, "reconcile: closing orphan pane");
            let _ = client.pane_close(&pane).await;
        }
    }
    Ok(())
}
