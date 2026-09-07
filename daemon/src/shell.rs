//! Plain shells the daemon opens on a host, for the UI's host-shell panel.
//!
//! A "host shell" is one herdr pane running nothing but the user's login shell: no agent, no
//! run row, no turn. It exists so the odd jobs a remote host needs — installing a CLI, reading
//! a log, `gh auth status`, tidying a worktree — can be done from the UI instead of a second
//! terminal window and an `ssh`.
//!
//! **The registry is the whitelist.** Every endpoint except `open` looks the `(host, pane_id)`
//! pair up in [`Registry`] first and 404s when it is not there, so "you cannot send keys to an
//! arbitrary pane" holds by construction rather than by inspecting what a pane looks like.
//! It is memory only: after a daemon restart the table is empty and every pane from the
//! previous life is a stranger, which is exactly the behaviour we want from a whitelist.

use crate::db;
use crate::herdr::HerdrClient;
use crate::lifecycle::{LcError, LcResult};
use crate::state::App;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A plain shell has no natural per-host limit the way "one bot, one pane" does, and a
/// mis-click is cheap to repeat, so a stuck finger could leave a row of invisible panes
/// behind. Eight is more than the jobs this panel is for ever need at once.
pub const MAX_PER_HOST: usize = 8;

/// The workspace label used when a host has no project workspace to borrow.
const SHELL_LABEL: &str = "shell";

#[derive(Debug, Clone, Serialize)]
pub struct HostShell {
    pub host: String,
    pub herdr_session: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub cwd: String,
    pub created_at: String,
}

/// `App.host_shells`. See the module comment for why it is deliberately not persisted.
pub type Registry = Mutex<Vec<HostShell>>;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// The herdr client for a host's **manager** session, plus that session's name.
///
/// The local `default` session is intentionally unreachable from here: that one is the user's
/// own, and the daemon does not put things into it (`state::ensure_session` draws the same
/// line). A host that is configured but down fails here rather than after opening a pane, so
/// nothing unusable ever reaches the registry.
pub(crate) async fn client_for(app: &Arc<App>, host: &str) -> LcResult<(HerdrClient, String)> {
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let session = app
        .session_for_host(host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{host}` has no Herdr session configured")))?;
    if !app.session_connected(host, &session).await {
        return Err(LcError::Upstream(format!("host `{host}` is not connected")));
    }
    let client = app
        .herdr_for_session(host, &session)
        .await
        .ok_or_else(|| LcError::Upstream(format!("Herdr session `{session}` for host `{host}` is not configured")))?;
    Ok((client, session))
}

/// Where a shell with no explicit `cwd` should start: a project on that host, else its `$HOME`.
///
/// A project path is the better default because it is where the jobs this panel is for
/// actually happen (`git worktree list`, reading a log next to the checkout).
async fn default_cwd(app: &Arc<App>, host: &str) -> LcResult<String> {
    let projects = db::live_projects(&app.db).await.map_err(up)?;
    if let Some(p) = projects.iter().find(|p| p.host == host) {
        return Ok(p.path.clone());
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    conn.home().await.map_err(up)
}

/// The workspace a new shell goes into: borrow one of that host's projects, or make one.
///
/// Returns the pane straight away when a workspace had to be created, because its root pane
/// is *already* a tab of its own holding exactly one pane — the same reason
/// `lifecycle::acquire_run_pane` uses a fresh root as-is instead of doubling it.
///
/// A freshly created workspace is deliberately **not** written back to
/// `projects.workspace_id`: that column is where the project's bots live, and a shell is only
/// passing through. Writing it would make the next bot start put its pane in a workspace
/// labelled `shell`.
async fn acquire_pane(
    app: &Arc<App>,
    client: &HerdrClient,
    host: &str,
    cwd: &str,
) -> LcResult<crate::herdr::PaneInfo> {
    let projects = db::live_projects(&app.db).await.map_err(up)?;
    for p in projects.iter().filter(|p| p.host == host) {
        let Some(ws) = p.workspace_id.as_deref().filter(|w| !w.trim().is_empty()) else { continue };
        if client.workspace_get(ws).await.map_err(up)?.is_some() {
            return client.tab_create(ws, cwd, SHELL_LABEL, json!({})).await.map_err(up);
        }
    }
    let (_, root) = client.workspace_create(cwd, SHELL_LABEL, json!({})).await.map_err(up)?;
    Ok(root)
}

/// `POST /api/hosts/:name/shells` — open one.
pub async fn open(app: &Arc<App>, host: &str, cwd: Option<&str>) -> LcResult<HostShell> {
    let (client, session) = client_for(app, host).await?;
    // Counted before the pane is created, so a burst of clicks cannot race past the cap.
    let live = app.host_shells.lock().await.iter().filter(|s| s.host == host).count();
    if live >= MAX_PER_HOST {
        return Err(LcError::conflict("too_many_shells", json!({"host": host, "max": MAX_PER_HOST})));
    }
    let cwd = match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        None => default_cwd(app, host).await?,
    };
    let pane = acquire_pane(app, &client, host, &cwd).await?;
    let shell = HostShell {
        host: host.to_string(),
        herdr_session: session,
        workspace_id: pane.workspace_id.clone(),
        tab_id: pane.tab_id.clone(),
        pane_id: pane.pane_id.clone(),
        // What herdr actually opened, which is not always what was asked for (a path that does
        // not exist on that host lands somewhere else), so the UI shows the truth.
        cwd: pane.cwd.clone().unwrap_or(cwd),
        created_at: db::now(),
    };
    app.host_shells.lock().await.push(shell.clone());
    tracing::info!(host, pane_id = %shell.pane_id, cwd = %shell.cwd, "opened a host shell");
    Ok(shell)
}

/// `GET /api/hosts/:name/shells` — the ones still alive, sweeping the dead out on the way.
///
/// Closing a pane by hand in herdr is an ordinary thing to do, so a shell that is gone must
/// stop being listed rather than sit there forever as a row that answers nothing.
pub async fn list(app: &Arc<App>, host: &str) -> LcResult<Vec<HostShell>> {
    let (client, _) = client_for(app, host).await?;
    let mine: Vec<HostShell> = app.host_shells.lock().await.iter().filter(|s| s.host == host).cloned().collect();
    let mut alive = Vec::with_capacity(mine.len());
    let mut dead = Vec::new();
    for s in mine {
        // A herdr that cannot be asked is not evidence the pane died — keep it listed and let
        // the next poll decide, the same way `close_tab_if_empty` refuses to guess.
        match client.pane_get(&s.pane_id).await {
            Ok(None) => dead.push(s.pane_id.clone()),
            _ => alive.push(s),
        }
    }
    if !dead.is_empty() {
        app.host_shells.lock().await.retain(|s| s.host != host || !dead.contains(&s.pane_id));
    }
    Ok(alive)
}

/// The registry row for a pane, or 404. This is the whitelist check every endpoint below runs.
async fn registered(app: &Arc<App>, host: &str, pane_id: &str) -> LcResult<HostShell> {
    app.host_shells
        .lock()
        .await
        .iter()
        .find(|s| s.host == host && s.pane_id == pane_id)
        .cloned()
        .ok_or_else(|| LcError::NotFound("shell".into()))
}

/// `GET /api/hosts/:name/shells/:pane_id/terminal` — same shape as `GET /bots/:id/terminal`.
pub async fn read(app: &Arc<App>, host: &str, pane_id: &str, source: &str, lines: u32) -> LcResult<Value> {
    let shell = registered(app, host, pane_id).await?;
    if !["visible", "recent", "recent_unwrapped", "detection"].contains(&source) {
        return Err(LcError::Bad("bad source".into()));
    }
    let (client, _) = client_for(app, host).await?;
    let read = client.pane_read(pane_id, source, lines).await.map_err(up)?;
    // Best effort, exactly as in `get_terminal`: a snapshot is still worth returning without
    // the geometry, and the UI needs the width to explain a wrapped line rather than hide it.
    let (columns, rows) = match client.pane_size(pane_id).await {
        Ok(Some((w, h))) => (Some(w), Some(h)),
        _ => (None, None),
    };
    Ok(json!({
        "host": host, "pane_id": pane_id, "cwd": shell.cwd,
        "source": read.source, "text": read.text, "revision": read.revision, "truncated": read.truncated,
        "columns": columns, "rows": rows,
    }))
}

/// `POST /api/hosts/:name/shells/:pane_id/text` — type a line, optionally pressing Enter.
///
/// Enter is a **separate** `pane.send_keys`, not a `\n` inside the text: to herdr a newline in
/// `pane.send_text` is a pasted line break, not a key press (the same distinction
/// `herdr::fold_newlines` exists for). An empty `text` with `enter: true` is allowed on
/// purpose — "just press Enter" is a real thing to want at a prompt.
pub async fn send_text(app: &Arc<App>, host: &str, pane_id: &str, text: &str, enter: bool) -> LcResult<()> {
    registered(app, host, pane_id).await?;
    let (client, _) = client_for(app, host).await?;
    if !text.is_empty() {
        client.pane_send_text(pane_id, text).await.map_err(up)?;
    }
    if enter {
        client.pane_send_keys(pane_id, &["enter"]).await.map_err(up)?;
    }
    Ok(())
}

/// `POST /api/hosts/:name/shells/:pane_id/keys` — ctrl+c, esc, arrows. Names go to herdr
/// verbatim; the daemon does not translate them (same contract as `POST /bots/:id/keys`).
pub async fn send_keys(app: &Arc<App>, host: &str, pane_id: &str, keys: &[String]) -> LcResult<()> {
    registered(app, host, pane_id).await?;
    if keys.is_empty() {
        return Err(LcError::Bad("keys must not be empty".into()));
    }
    let (client, _) = client_for(app, host).await?;
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    client.pane_send_keys(pane_id, &refs).await.map_err(up)
}

/// `DELETE /api/hosts/:name/shells/:pane_id` — close the pane, and its tab when that empties it.
///
/// Idempotent: a pane_id that is not (or no longer) in the registry is success, because the
/// caller's goal — that shell is gone — already holds. Only a host we cannot even resolve is
/// an error, since then we do not know whether anything was left behind.
pub async fn close(app: &Arc<App>, host: &str, pane_id: &str) -> LcResult<()> {
    let (client, _) = client_for(app, host).await?;
    let Some(shell) = app.host_shells.lock().await.iter().find(|s| s.host == host && s.pane_id == pane_id).cloned()
    else {
        return Ok(());
    };
    crate::lifecycle::close_pane_and_tab(&client, Some(&shell.workspace_id), Some(&shell.tab_id), pane_id).await;
    app.host_shells.lock().await.retain(|s| s.host != host || s.pane_id != pane_id);
    tracing::info!(host, pane_id, "closed a host shell");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whitelist is what stops `POST …/shells/<any pane>/keys` reaching a bot's pane, so
    /// the lookup has to be keyed on **both** halves: a pane_id is only unique within a host,
    /// and herdr hands out the same `w1:p1` shape on every machine.
    #[test]
    fn a_shell_is_only_recognised_on_the_host_it_was_opened_on() {
        let rows = vec![
            HostShell {
                host: "local".into(),
                herdr_session: "agents-manager".into(),
                workspace_id: "w1".into(),
                tab_id: "w1:t2".into(),
                pane_id: "w1:p2".into(),
                cwd: "/tmp".into(),
                created_at: "2026-09-07T00:00:00Z".into(),
            },
            HostShell {
                host: "m4p".into(),
                herdr_session: "agents-manager".into(),
                workspace_id: "w3".into(),
                tab_id: "w3:t1".into(),
                pane_id: "w3:p1".into(),
                cwd: "/Users/m4p".into(),
                created_at: "2026-09-07T00:00:00Z".into(),
            },
        ];
        let found = |host: &str, pane: &str| rows.iter().any(|s| s.host == host && s.pane_id == pane);

        assert!(found("local", "w1:p2"));
        assert!(found("m4p", "w3:p1"));
        // The same pane_id on the wrong host must not match…
        assert!(!found("m4p", "w1:p2"));
        // …and neither must a pane the daemon never opened, which is every bot pane.
        assert!(!found("local", "w1:p1"));
    }
}
