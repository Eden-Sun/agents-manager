//! Reconciliation (SPEC §6.5): runs at daemon start and after every event-stream reconnect.

use crate::db;
use crate::state::App;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

pub async fn reconcile(app: &Arc<App>) -> Result<()> {
    let snapshot = app.herdr.snapshot().await?;
    let agents = app.herdr.agent_list().await.unwrap_or_default();
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
    for p in db::live_projects(&app.db).await? {
        if let Some(ws) = p.workspace_id.as_deref() {
            if !live_ws.contains(&ws.to_string()) {
                sqlx::query("UPDATE projects SET workspace_id=NULL WHERE id=?").bind(&p.id).execute(&app.db).await?;
                tracing::info!(project = %p.label, "workspace disappeared; mapping cleared");
            }
        }
    }

    let bots = db::live_bots(&app.db).await?;
    for bot in bots {
        let lock = app.bot_lock(&bot.id).await;
        let _g = lock.lock().await;
        let active = db::active_run(&app.db, &bot.id).await?;
        match (active, by_name.get(&bot.name)) {
            (Some(run), Some(agent)) => {
                // Still alive: refresh pane_id (pane move changes it) and status.
                let status = agent.agent_status.normalized().as_str().to_string();
                sqlx::query("UPDATE runs SET pane_id=?, workspace_id=?, agent_status=?, state=CASE WHEN state='starting' THEN 'running' ELSE state END WHERE id=?")
                    .bind(&agent.pane_id)
                    .bind(&agent.workspace_id)
                    .bind(&status)
                    .bind(&run.id)
                    .execute(&app.db)
                    .await?;
                if run.pane_id.as_deref() != Some(agent.pane_id.as_str()) {
                    if let Some(old) = run.pane_id.as_deref() {
                        crate::events::unwatch_pane(app, old).await;
                    }
                }
                crate::events::watch_pane(app, &agent.pane_id).await;
                tracing::info!(bot = %bot.name, run = %run.id, pane = %agent.pane_id, "reconcile: kept active run");
            }
            (Some(run), None) => {
                tracing::info!(bot = %bot.name, run = %run.id, "reconcile: agent gone, marking run exited");
                crate::lifecycle::mark_run_exited(app, &run.id, "agent not found during reconcile").await;
            }
            (None, Some(agent)) => {
                let run_id = db::ulid();
                let status = agent.agent_status.normalized().as_str().to_string();
                sqlx::query(
                    "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, adopted, started_at)
                     VALUES (?,?,'running',?,?,?,1,?)",
                )
                .bind(&run_id)
                .bind(&bot.id)
                .bind(&status)
                .bind(&agent.workspace_id)
                .bind(&agent.pane_id)
                .bind(db::now())
                .execute(&app.db)
                .await?;
                // Make sure the project keeps pointing at the workspace the agent lives in.
                sqlx::query("UPDATE projects SET workspace_id=? WHERE id=? AND workspace_id IS NULL")
                    .bind(&agent.workspace_id)
                    .bind(&bot.project_id)
                    .execute(&app.db)
                    .await?;
                crate::events::watch_pane(app, &agent.pane_id).await;
                tracing::info!(bot = %bot.name, run = %run_id, pane = %agent.pane_id, "reconcile: adopted existing agent");
            }
            (None, None) => {}
        }
        app.emit_bot_status(&bot.id).await;
    }

    // Orphan pane recovery: panes we created for finished runs that no longer host an agent.
    let dead: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT pane_id FROM runs WHERE pane_id IS NOT NULL AND state IN ('exited','stopped')
         AND pane_id NOT IN (SELECT pane_id FROM runs WHERE pane_id IS NOT NULL AND state IN ('starting','running','stopping'))",
    )
    .fetch_all(&app.db)
    .await?;
    for pane in dead {
        if live_panes.contains(&pane) && !panes_with_agent.contains(&pane) {
            tracing::info!(pane_id = %pane, "reconcile: closing orphan pane");
            let _ = app.herdr.pane_close(&pane).await;
        }
    }
    Ok(())
}
