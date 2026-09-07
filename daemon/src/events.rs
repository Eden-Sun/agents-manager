//! herdr event subscription topology (SPEC §3.1, §11.3.6) and event handling (§6.6).
//!
//! - one global connection **per host/session**: pane.exited / pane.closed / workspace.closed /
//!   pane.agent_detected
//! - one connection per active Run: pane.agent_status_changed {pane_id}
//!
//! pane ids are only unique within a Herdr session, so every lookup is keyed by
//! `(host, session, pane_id)`.
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

/// Start (or restart) the global subscription for one host's configured session. Replacing the old task and
/// registering the new one happens under one lock, so two quick calls cannot leave two
/// loops running (each of which would reconcile independently).
pub async fn spawn_global_for_host(app: Arc<App>, host: String) {
    let Some(session) = app.session_for_host(&host).await else {
        tracing::warn!(host, "cannot start global subscription without a configured session");
        return;
    };
    spawn_global_for_session(app, host, session).await;
}

/// Start (or restart) a global subscription for an explicit host/session pair.
pub async fn spawn_global_for_session(app: Arc<App>, host: String, session: String) {
    let mut g = app.global_watchers.lock().await;
    let key = (host.clone(), session.clone());
    if let Some(old) = g.remove(&key) {
        old.abort();
    }
    let (a, h, s) = (app.clone(), host.clone(), session.clone());
    let t = tokio::spawn(async move { global_loop(a, h, s).await });
    g.insert(key, t);
}

/// Local host convenience (kept for `main.rs`).
pub async fn spawn_global(app: Arc<App>) {
    spawn_global_for_host(app, LOCAL_HOST.to_string()).await;
}

async fn global_loop(app: Arc<App>, host: String, session: String) {
    let is_local_main = host == LOCAL_HOST && session == app.herdr_session.as_str();
    let is_local_default = host == LOCAL_HOST && session == "default";
    let mut backoff = Duration::from_millis(250);
    loop {
        let Some(client) = app.herdr_for_session(&host, &session).await else {
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
                if is_local_main {
                    app.connected.store(true, Ordering::SeqCst);
                    crate::state::emit_daemon_status(&app).await;
                }
                if is_local_default {
                    crate::state::set_default_connected(&app, true).await;
                }
                tracing::info!(host = %host, session = %session, "global herdr event subscription established");
                // Reconcile after every (re)connect.
                if is_local_default {
                    if let Err(e) = crate::default_session::sync(&app).await {
                        tracing::debug!(host = %host, session = %session, error = ?e, "default session sync failed");
                    }
                } else if let Err(e) = crate::reconcile::reconcile_host(&app, &host).await {
                    tracing::error!(host = %host, session = %session, error = ?e, "reconcile failed");
                }
                while let Some(ev) = rx.recv().await {
                    handle_global(&app, &host, &session, &ev).await;
                }
                if is_local_main {
                    app.connected.store(false, Ordering::SeqCst);
                    crate::state::emit_daemon_status(&app).await;
                }
                if is_local_default {
                    crate::state::set_default_connected(&app, false).await;
                }
                tracing::warn!(host = %host, session = %session, "global herdr event subscription dropped");
            }
            Err(e) => {
                if is_local_main {
                    app.connected.store(false, Ordering::SeqCst);
                    // A6: tell the UI right away, otherwise the lamp stays green until a
                    // later attempt succeeds.
                    crate::state::emit_daemon_status(&app).await;
                }
                if is_local_default {
                    crate::state::set_default_connected(&app, false).await;
                }
                tracing::debug!(host = %host, session = %session, error = %e, "herdr subscribe failed");
            }
        }
        // A remote host's reconnect is owned by its HostConn supervisor: stop retrying here
        // once the host is down, and let the supervisor respawn us after the master is back.
        if host != LOCAL_HOST && !app.host_connected(&host).await {
            tracing::info!(host = %host, "host disconnected; global subscription yields to the supervisor");
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

async fn handle_global(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event) {
    let name = norm(&ev.event);
    match name.as_str() {
        "pane_exited" | "pane_closed" => {
            let Some(pane_id) = ev.data.get("pane_id").and_then(|v| v.as_str()) else { return };
            tracing::info!(host, pane_id, event = %ev.event, "pane gone");
            end_runs_for_pane(app, host, session, pane_id).await;
        }
        "workspace_closed" => {
            let Some(ws) = ev.data.get("workspace_id").and_then(|v| v.as_str()) else { return };
            tracing::info!(host, workspace_id = ws, "workspace closed");
            for b in crate::db::live_bots_on_host(&app.db, host).await.unwrap_or_default() {
                if let Ok(Some(r)) = crate::db::active_run(&app.db, &b.id).await {
                    if r.workspace_id.as_deref() == Some(ws) && app.session_for_run(&r).await.as_deref() == Some(session) {
                        crate::lifecycle::mark_run_exited(app, &r.id, "workspace closed").await;
                    }
                }
            }
            if app.session_for_host(host).await.as_deref() == Some(session) {
                let _ = sqlx::query("UPDATE projects SET workspace_id = NULL WHERE workspace_id = ? AND host = ?")
                    .bind(ws)
                    .bind(host)
                    .execute(&app.db)
                    .await;
                app.emit("project_changed", json!({})).await;
            }
        }
        "pane_agent_detected" => {
            tracing::debug!(host, session, data = %ev.data, "pane.agent_detected");
            if host == LOCAL_HOST && session == "default" {
                if let Err(e) = crate::default_session::sync(app).await {
                    tracing::debug!(host, session, error = ?e, "default session sync after agent detection failed");
                }
            } else if app.session_for_host(host).await.as_deref() == Some(session) {
                // A new agent in the manager's own session: most often a child pane a bot just
                // opened (`<parent>-<suffix>`, see `reconcile`), which nobody would otherwise
                // notice until the next restart. Off this task, and a beat late, so an agent
                // that is still settling reports its name and kind.
                let app = app.clone();
                let host = host.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    if let Err(e) = crate::reconcile::reconcile_host(&app, &host).await {
                        tracing::debug!(host = %host, error = ?e, "reconcile after agent detection failed");
                    }
                });
            }
        }
        other => tracing::trace!(host, session, event = other, "unhandled global herdr event"),
    }
}

async fn end_runs_for_pane(app: &Arc<App>, host: &str, session: &str, pane_id: &str) {
    let fallback = app.session_for_host(host).await.unwrap_or_default();
    for r in crate::db::active_runs_for_pane(&app.db, host, pane_id, session, &fallback).await.unwrap_or_default() {
        crate::lifecycle::mark_run_exited(app, &r.id, "pane exited").await;
    }
    unwatch_pane_on_session(app, host, session, pane_id).await;
}

/// Open (or replace) the per-run status subscription in the host's configured session.
pub async fn watch_pane(app: &Arc<App>, host: &str, pane_id: &str) {
    let Some(session) = app.session_for_host(host).await else { return };
    watch_pane_on_session(app, host, &session, pane_id).await;
}

/// Generation of the watcher currently registered for a pane, only ever read or written
/// while holding `pane_watchers`. A watcher that ends by itself has to take its own map entry
/// with it — `watch_pane_on_session` treats a present key as "already watching", so a
/// leftover entry meant that pane could never be subscribed again: the bot's lamp froze at
/// its adoption-time status and neither `cancel_stall` nor `begin_external_turn` ever fired
/// (§6.6) — but it must not evict a *newer* watcher installed for the same pane meanwhile.
fn watcher_gens() -> &'static std::sync::Mutex<std::collections::HashMap<PaneKey, u64>> {
    static G: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PaneKey, u64>>> =
        std::sync::OnceLock::new();
    G.get_or_init(Default::default)
}

type PaneKey = (String, String, String);

/// Open (or replace) the per-run `pane.agent_status_changed` subscription for an explicit session.
pub async fn watch_pane_on_session(app: &Arc<App>, host: &str, session: &str, pane_id: &str) {
    let key = (host.to_string(), session.to_string(), pane_id.to_string());
    let mut g = app.pane_watchers.lock().await;
    if g.contains_key(&key) {
        return;
    }
    let generation = {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        watcher_gens().lock().unwrap().insert(key.clone(), n);
        n
    };
    let app2 = app.clone();
    let pid = pane_id.to_string();
    let hst = host.to_string();
    let sess = session.to_string();
    let own = key.clone();
    let handle = tokio::spawn(async move {
        let mut backoff = Duration::from_millis(250);
        loop {
            let Some(client) = app2.herdr_for_session(&hst, &sess).await else { break };
            let subs = vec![json!({"type": "pane.agent_status_changed", "pane_id": pid})];
            match client.subscribe(subs).await {
                Ok(mut rx) => {
                    backoff = Duration::from_millis(250);
                    tracing::info!(host = %hst, session = %sess, pane_id = %pid, "watching pane agent status");
                    while let Some(ev) = rx.recv().await {
                        handle_status(&app2, &hst, &sess, &ev).await;
                    }
                    tracing::warn!(host = %hst, session = %sess, pane_id = %pid, "pane status subscription dropped");
                }
                Err(e) => tracing::debug!(host = %hst, session = %sess, pane_id = %pid, error = %e, "pane status subscribe failed"),
            }
            // Stop retrying once the run is no longer active.
            let fallback = app2.session_for_host(&hst).await.unwrap_or_default();
            let still = crate::db::active_runs_for_pane(&app2.db, &hst, &pid, &sess, &fallback).await.unwrap_or_default().len();
            if still == 0 {
                break;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
        forget_watcher(&app2, own, generation).await;
    });
    g.insert(key, handle);
}

/// Drop a watcher's own registration, on every path out of its loop.
async fn forget_watcher(app: &Arc<App>, key: PaneKey, generation: u64) {
    let mut watchers = app.pane_watchers.lock().await;
    let mut gens = watcher_gens().lock().unwrap();
    if gens.get(&key) == Some(&generation) {
        gens.remove(&key);
        watchers.remove(&key);
    }
}

pub async fn unwatch_pane(app: &Arc<App>, host: &str, pane_id: &str) {
    let Some(session) = app.session_for_host(host).await else { return };
    unwatch_pane_on_session(app, host, &session, pane_id).await;
}

pub async fn unwatch_pane_on_session(app: &Arc<App>, host: &str, session: &str, pane_id: &str) {
    let key = (host.to_string(), session.to_string(), pane_id.to_string());
    let mut watchers = app.pane_watchers.lock().await;
    watcher_gens().lock().unwrap().remove(&key);
    if let Some(h) = watchers.remove(&key) {
        h.abort();
    }
}

async fn handle_status(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event) {
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
    tracing::info!(host, session, pane_id, status = %status, "pane.agent_status_changed");

    let fallback = app.session_for_host(host).await.unwrap_or_default();
    let Some(run) = crate::db::active_runs_for_pane(&app.db, host, pane_id, session, &fallback).await.unwrap_or_default().into_iter().next()
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

    // The agent reacted to the prompt: the stall watchdog is no longer needed.
    if status == "working" || status == "blocked" {
        crate::lifecycle::cancel_stall(app, &run.id).await;
    }

    // 剛停下來等人回答的話，先看一眼是不是 claude 那份滿意度問卷——是的話 daemon 自己按 0，
    // 使用者不必為了一個跟工作無關的問題被彈窗打斷（§3.2）。其他 blocked 原封不動。
    if prev != "blocked" && status == "blocked" {
        let (app2, run2) = (app.clone(), run.clone());
        tokio::spawn(async move {
            crate::tui_prompts::dismiss_if_survey(&app2, &run2).await;
        });
    }

    // An agent that starts working with no turn in flight was prompted from the tmux pane, not
    // from the web. Open the external turn now so the UI streams it live; waiting for the Stop
    // hook would leave the conversation blank for the whole run. A turn already in flight means
    // this is the web's own prompt taking effect — leave it alone.
    if prev != "working" && status == "working" && matches!(crate::db::in_flight_turn(&app.db, &run.id).await, Ok(None)) {
        crate::lifecycle::begin_external_turn(app, &run).await;
    }

    // §4.3: working -> idle arms the terminal fallback. `blocked` never does.
    if prev == "working" && status == "idle" {
        crate::lifecycle::arm_fallback(app, &run.id, &run.bot_id).await;
        // A prompt queued while the agent was still working waits for exactly this edge.
        crate::lifecycle::schedule_flush_queued(app, &run.bot_id);
    }
}

// ---------------------------------------------------------------- agent titles

/// How often the agents' self-chosen titles are refreshed.
///
/// There is no herdr event for a title change, so this polls. One `agent.list` per host
/// covers every run on it, and a write only happens when the text actually changed.
const TITLE_POLL: Duration = Duration::from_secs(4);

/// Titles that carry no information: the CLI's own name before it has been given a task.
const PLACEHOLDER_TITLES: &[&str] = &["claude code", "claude", "codex", "grok", "grok cli", "terminal", "zsh", "bash"];

/// Tidy one raw terminal title.
///
/// herdr already strips the spinner glyph, but some CLIs bake their status into the title
/// itself (grok: `- Thinking - <task> - grok`), so trim the leading/trailing dashes too.
/// Returns `None` for a title that says nothing the status lamp does not already say.
fn clean_title(raw: &str) -> Option<String> {
    let t = raw.trim().trim_matches('-').trim();
    if t.is_empty() || PLACEHOLDER_TITLES.contains(&t.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(t.to_string())
}

/// Keep `runs.agent_title` in step with what each agent currently calls itself
/// (`terminal_title_stripped` — for Claude Code, a running summary of its task).
pub fn spawn_title_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(TITLE_POLL).await;
            for conn in app.hosts.list().await {
                if !conn.is_local() && !conn.is_connected() {
                    continue;
                }
                let Some(session) = app.session_for_host(&conn.name).await else { continue };
                let Some(client) = app.herdr_for_session(&conn.name, &session).await else { continue };
                poll_titles(&app, &conn.name, &session, &session, &client).await;
            }
            // The default session is not represented by HostManager. It may contain agents
            // imported into the UI, so keep their live titles current as well.
            if app.herdr_session != "default" && app.default_connected.load(Ordering::SeqCst) {
                let fallback = app.herdr_session.clone();
                let client = app.default_herdr.clone();
                poll_titles(&app, LOCAL_HOST, "default", &fallback, &client).await;
            }
        }
    });
}

async fn poll_titles(app: &Arc<App>, host: &str, session: &str, fallback_session: &str, client: &crate::herdr::HerdrClient) {
    let Ok(agents) = client.agent_list().await else { return };
    for a in agents {
        let (Some(name), Some(raw)) = (a.name.as_deref(), a.terminal_title_stripped.as_deref()) else {
            continue;
        };
        let Some(title) = clean_title(raw) else { continue };
        let title = title.as_str();
        // Match on both the agent name and session; named and default sessions may reuse names.
        let row = sqlx::query_as::<_, (String, String, Option<String>)>(
            &format!(
                "SELECT id, bot_id, agent_title FROM runs WHERE agent_name = ? AND state IN {} AND COALESCE(herdr_session, ?) = ? LIMIT 1",
                crate::db::ACTIVE_STATES
            ),
        )
        .bind(name)
        .bind(fallback_session)
        .bind(session)
        .fetch_optional(&app.db)
        .await;
        let Ok(Some((run_id, bot_id, current))) = row else { continue };
        if current.as_deref() == Some(title) {
            continue;
        }
        let _ = sqlx::query("UPDATE runs SET agent_title = ? WHERE id = ?")
            .bind(title)
            .bind(&run_id)
            .execute(&app.db)
            .await;
        app.emit_bot_status(&bot_id).await;
    }
    let _ = host; // kept in the helper signature for session-scoped tracing/debugging callers
}
