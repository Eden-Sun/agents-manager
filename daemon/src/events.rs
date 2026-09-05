//! herdr event subscription topology (SPEC §3.1, §11.3.6) and event handling (§6.6).
//!
//! - one global connection **per host**: pane.exited / pane.closed / workspace.closed / pane.agent_detected
//! - one connection per active Run: pane.agent_status_changed {pane_id}
//!
//! pane ids are only unique within a host, so every lookup is keyed by `(host, pane_id)`.
//!
//! herdr is inconsistent about event naming (`pane.agent_status_changed` uses dots,
//! `pane_updated` uses underscores), so every name is normalized before matching.

use crate::config::LOCAL_HOST;
use crate::state::App;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn norm(name: &str) -> String {
    name.replace('.', "_")
}

/// Start (or restart) the global subscription for one host.
pub fn spawn_global_for_host(app: Arc<App>, host: String) {
    tokio::spawn(async move {
        if let Some(old) = app.global_watchers.lock().await.remove(&host) {
            old.abort();
        }
        let (a, h) = (app.clone(), host.clone());
        let t = tokio::spawn(async move { global_loop(a, h).await });
        app.global_watchers.lock().await.insert(host, t);
    });
}

/// Local host convenience (kept for `main.rs`).
pub fn spawn_global(app: Arc<App>) {
    spawn_global_for_host(app, LOCAL_HOST.to_string());
}

async fn global_loop(app: Arc<App>, host: String) {
    let is_local = host == LOCAL_HOST;
    let mut backoff = Duration::from_millis(250);
    loop {
        let Some(client) = app.herdr_for(&host).await else {
            tracing::info!(host = %host, "host gone; stopping global subscription");
            return;
        };
        let subs = vec![
            json!({"type": "pane.exited"}),
            json!({"type": "pane.closed"}),
            json!({"type": "workspace.closed"}),
            json!({"type": "pane.agent_detected"}),
        ];
        match client.subscribe(subs).await {
            Ok(mut rx) => {
                backoff = Duration::from_millis(250);
                if is_local {
                    app.connected.store(true, Ordering::SeqCst);
                    crate::state::emit_daemon_status(&app).await;
                }
                tracing::info!(host = %host, "global herdr event subscription established");
                // Reconcile after every (re)connect.
                if let Err(e) = crate::reconcile::reconcile_host(&app, &host).await {
                    tracing::error!(host = %host, error = ?e, "reconcile failed");
                }
                while let Some(ev) = rx.recv().await {
                    handle_global(&app, &host, &ev).await;
                }
                if is_local {
                    app.connected.store(false, Ordering::SeqCst);
                    crate::state::emit_daemon_status(&app).await;
                }
                tracing::warn!(host = %host, "global herdr event subscription dropped");
            }
            Err(e) => {
                if is_local {
                    app.connected.store(false, Ordering::SeqCst);
                }
                tracing::warn!(host = %host, error = %e, "herdr subscribe failed");
            }
        }
        // A remote host's reconnect is owned by its HostConn supervisor: stop retrying here
        // once the host is down, and let the supervisor respawn us after the master is back.
        if !is_local && !app.host_connected(&host).await {
            tracing::info!(host = %host, "host disconnected; global subscription yields to the supervisor");
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

async fn handle_global(app: &Arc<App>, host: &str, ev: &crate::herdr::Event) {
    let name = norm(&ev.event);
    match name.as_str() {
        "pane_exited" | "pane_closed" => {
            let Some(pane_id) = ev.data.get("pane_id").and_then(|v| v.as_str()) else { return };
            tracing::info!(host, pane_id, event = %ev.event, "pane gone");
            end_runs_for_pane(app, host, pane_id).await;
        }
        "workspace_closed" => {
            let Some(ws) = ev.data.get("workspace_id").and_then(|v| v.as_str()) else { return };
            tracing::info!(host, workspace_id = ws, "workspace closed");
            let _ = sqlx::query("UPDATE projects SET workspace_id = NULL WHERE workspace_id = ? AND host = ?")
                .bind(ws)
                .bind(host)
                .execute(&app.db)
                .await;
            for b in crate::db::live_bots_on_host(&app.db, host).await.unwrap_or_default() {
                if let Ok(Some(r)) = crate::db::active_run(&app.db, &b.id).await {
                    if r.workspace_id.as_deref() == Some(ws) {
                        crate::lifecycle::mark_run_exited(app, &r.id, "workspace closed").await;
                    }
                }
            }
            app.emit("project_changed", json!({})).await;
        }
        "pane_agent_detected" => {
            tracing::debug!(host, data = %ev.data, "pane.agent_detected");
        }
        other => tracing::trace!(host, event = other, "unhandled global herdr event"),
    }
}

async fn end_runs_for_pane(app: &Arc<App>, host: &str, pane_id: &str) {
    for r in crate::db::active_runs_for_pane(&app.db, host, pane_id).await.unwrap_or_default() {
        crate::lifecycle::mark_run_exited(app, &r.id, "pane exited").await;
    }
    unwatch_pane(app, host, pane_id).await;
}

/// Open (or replace) the per-run `pane.agent_status_changed` subscription.
pub async fn watch_pane(app: &Arc<App>, host: &str, pane_id: &str) {
    let key = (host.to_string(), pane_id.to_string());
    let mut g = app.pane_watchers.lock().await;
    if g.contains_key(&key) {
        return;
    }
    let app2 = app.clone();
    let pid = pane_id.to_string();
    let hst = host.to_string();
    let handle = tokio::spawn(async move {
        let mut backoff = Duration::from_millis(250);
        loop {
            let Some(client) = app2.herdr_for(&hst).await else { break };
            let subs = vec![json!({"type": "pane.agent_status_changed", "pane_id": pid})];
            match client.subscribe(subs).await {
                Ok(mut rx) => {
                    backoff = Duration::from_millis(250);
                    tracing::info!(host = %hst, pane_id = %pid, "watching pane agent status");
                    while let Some(ev) = rx.recv().await {
                        handle_status(&app2, &hst, &ev).await;
                    }
                    tracing::warn!(host = %hst, pane_id = %pid, "pane status subscription dropped");
                }
                Err(e) => tracing::warn!(host = %hst, pane_id = %pid, error = %e, "pane status subscribe failed"),
            }
            // Stop retrying once the run is no longer active.
            let still = crate::db::active_runs_for_pane(&app2.db, &hst, &pid).await.unwrap_or_default().len();
            if still == 0 {
                break;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    });
    g.insert(key, handle);
}

pub async fn unwatch_pane(app: &Arc<App>, host: &str, pane_id: &str) {
    if let Some(h) = app.pane_watchers.lock().await.remove(&(host.to_string(), pane_id.to_string())) {
        h.abort();
    }
}

async fn handle_status(app: &Arc<App>, host: &str, ev: &crate::herdr::Event) {
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
    tracing::info!(host, pane_id, status = %status, "pane.agent_status_changed");

    let Some(run) = crate::db::active_runs_for_pane(&app.db, host, pane_id).await.unwrap_or_default().into_iter().next()
    else {
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
