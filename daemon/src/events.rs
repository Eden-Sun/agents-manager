//! herdr event subscription topology (SPEC §3.1) and event handling (§6.6).
//!
//! - one global connection: pane.exited / pane.closed / workspace.closed / pane.agent_detected
//! - one connection per active Run: pane.agent_status_changed {pane_id}
//!
//! herdr is inconsistent about event naming (`pane.agent_status_changed` uses dots,
//! `pane_updated` uses underscores), so every name is normalized before matching.

use crate::state::App;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn norm(name: &str) -> String {
    name.replace('.', "_")
}

pub fn spawn_global(app: Arc<App>) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(250);
        loop {
            let subs = vec![
                json!({"type": "pane.exited"}),
                json!({"type": "pane.closed"}),
                json!({"type": "workspace.closed"}),
                json!({"type": "pane.agent_detected"}),
            ];
            match app.herdr.subscribe(subs).await {
                Ok(mut rx) => {
                    backoff = Duration::from_millis(250);
                    app.connected.store(true, Ordering::SeqCst);
                    tracing::info!("global herdr event subscription established");
                    app.emit("daemon_status", json!({"connected": true})).await;
                    // Reconcile after every (re)connect.
                    if let Err(e) = crate::reconcile::reconcile(&app).await {
                        tracing::error!(error = ?e, "reconcile failed");
                    }
                    while let Some(ev) = rx.recv().await {
                        handle_global(&app, &ev).await;
                    }
                    app.connected.store(false, Ordering::SeqCst);
                    app.emit("daemon_status", json!({"connected": false})).await;
                    tracing::warn!("global herdr event subscription dropped");
                }
                Err(e) => {
                    app.connected.store(false, Ordering::SeqCst);
                    tracing::warn!(error = %e, "herdr subscribe failed");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    });
}

async fn handle_global(app: &Arc<App>, ev: &crate::herdr::Event) {
    let name = norm(&ev.event);
    match name.as_str() {
        "pane_exited" | "pane_closed" => {
            let Some(pane_id) = ev.data.get("pane_id").and_then(|v| v.as_str()) else { return };
            tracing::info!(pane_id, event = %ev.event, "pane gone");
            end_runs_for_pane(app, pane_id).await;
        }
        "workspace_closed" => {
            let Some(ws) = ev.data.get("workspace_id").and_then(|v| v.as_str()) else { return };
            tracing::info!(workspace_id = ws, "workspace closed");
            let _ = sqlx::query("UPDATE projects SET workspace_id = NULL WHERE workspace_id = ?")
                .bind(ws)
                .execute(&app.db)
                .await;
            if let Ok(runs) = crate::db::all_active_runs(&app.db).await {
                for r in runs.into_iter().filter(|r| r.workspace_id.as_deref() == Some(ws)) {
                    crate::lifecycle::mark_run_exited(app, &r.id, "workspace closed").await;
                }
            }
            app.emit("project_changed", json!({})).await;
        }
        "pane_agent_detected" => {
            tracing::debug!(data = %ev.data, "pane.agent_detected");
        }
        other => tracing::trace!(event = other, "unhandled global herdr event"),
    }
}

async fn end_runs_for_pane(app: &Arc<App>, pane_id: &str) {
    let runs = sqlx::query_as::<_, crate::db::Run>(
        "SELECT * FROM runs WHERE pane_id = ? AND state IN ('starting','running','stopping')",
    )
    .bind(pane_id)
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();
    for r in runs {
        crate::lifecycle::mark_run_exited(app, &r.id, "pane exited").await;
    }
    unwatch_pane(app, pane_id).await;
}

/// Open (or replace) the per-run `pane.agent_status_changed` subscription.
pub async fn watch_pane(app: &Arc<App>, pane_id: &str) {
    let mut g = app.pane_watchers.lock().await;
    if g.contains_key(pane_id) {
        return;
    }
    let app2 = app.clone();
    let pid = pane_id.to_string();
    let handle = tokio::spawn(async move {
        let mut backoff = Duration::from_millis(250);
        loop {
            let subs = vec![json!({"type": "pane.agent_status_changed", "pane_id": pid})];
            match app2.herdr.subscribe(subs).await {
                Ok(mut rx) => {
                    backoff = Duration::from_millis(250);
                    tracing::info!(pane_id = %pid, "watching pane agent status");
                    while let Some(ev) = rx.recv().await {
                        handle_status(&app2, &ev).await;
                    }
                    tracing::warn!(pane_id = %pid, "pane status subscription dropped");
                }
                Err(e) => tracing::warn!(pane_id = %pid, error = %e, "pane status subscribe failed"),
            }
            // Stop retrying once the run is no longer active.
            let still = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM runs WHERE pane_id = ? AND state IN ('starting','running','stopping')",
            )
            .bind(&pid)
            .fetch_one(&app2.db)
            .await
            .unwrap_or(0);
            if still == 0 {
                break;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    });
    g.insert(pane_id.to_string(), handle);
}

pub async fn unwatch_pane(app: &Arc<App>, pane_id: &str) {
    if let Some(h) = app.pane_watchers.lock().await.remove(pane_id) {
        h.abort();
    }
}

async fn handle_status(app: &Arc<App>, ev: &crate::herdr::Event) {
    let name = norm(&ev.event);
    if name != "pane_agent_status_changed" {
        tracing::trace!(event = %ev.event, "unhandled pane event");
        return;
    }
    let Some(pane_id) = ev.data.get("pane_id").and_then(|v| v.as_str()) else { return };
    let status = ev
        .data
        .get("agent_status")
        .and_then(|v| v.as_str())
        .map(|s| if s == "done" { "idle" } else { s })
        .unwrap_or("unknown")
        .to_string();
    tracing::info!(pane_id, status = %status, "pane.agent_status_changed");

    let Some(run) = sqlx::query_as::<_, crate::db::Run>(
        "SELECT * FROM runs WHERE pane_id = ? AND state IN ('starting','running','stopping') LIMIT 1",
    )
    .bind(pane_id)
    .fetch_optional(&app.db)
    .await
    .ok()
    .flatten() else {
        return;
    };
    let prev = run.agent_status.clone();
    if prev != status {
        let _ = sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?")
            .bind(&status)
            .bind(&run.id)
            .execute(&app.db)
            .await;
    }
    app.emit_bot_status(&run.bot_id).await;

    // §4.3: working -> idle arms the terminal fallback. `blocked` never does.
    if prev == "working" && status == "idle" {
        crate::lifecycle::arm_fallback(app, &run.id, &run.bot_id).await;
    }
}
