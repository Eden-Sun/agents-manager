//! REST + WebSocket API (SPEC §7).

use crate::config::{
    canonical_path, valid_bot_name, valid_host_name, valid_identity_name, HostCfg, IdentityCfg, LOCAL_HOST,
};
use std::collections::BTreeMap;
use crate::db;
use crate::lifecycle::{self, LcError};
use crate::state::App;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// The host-shell endpoints' implementation. Declared here rather than in `main.rs` because
/// this file is its only caller; the file itself sits alongside the other modules.
#[path = "shell.rs"]
pub mod shell;

impl IntoResponse for LcError {
    fn into_response(self) -> Response {
        match self {
            LcError::NotFound(what) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found", "what": what}))).into_response(),
            LcError::Conflict(v) => (StatusCode::CONFLICT, Json(v)).into_response(),
            LcError::Bad(m) => (StatusCode::BAD_REQUEST, Json(json!({"error": "bad_request", "message": m}))).into_response(),
            LcError::BadValue(v) => (StatusCode::BAD_REQUEST, Json(v)).into_response(),
            LcError::Upstream(m) => {
                (StatusCode::BAD_GATEWAY, Json(json!({"error": "upstream", "message": m}))).into_response()
            }
        }
    }
}

fn any_err<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/state", get(get_state))
        .route("/projects", post(create_project))
        .route("/order", post(set_order))
        .route("/projects/{id}", patch(patch_project).delete(delete_project))
        .route("/projects/{id}/bots", post(create_bot))
        .route("/projects/{id}/messages", get(get_project_messages))
        .route("/projects/{id}/chat", post(project_chat))
        .route("/projects/{id}/github/refresh", post(refresh_github))
        .route("/projects/{id}/submodules", get(get_submodules))
        .route("/projects/{id}/git", get(get_git))
        .route("/projects/{id}/git/commit", post(git_commit))
        .route("/projects/{id}/git/push", post(git_push))
        .route("/projects/{id}/git/pull", post(git_pull))
        .route("/projects/{id}/issues", get(get_issues))
        .route("/projects/{id}/issues/{number}", get(get_issue))
        // SPEC-team §10
        .route("/projects/{id}/teams", post(create_team))
        .route("/teams/{id}", get(get_team).patch(patch_team).delete(delete_team))
        .route("/teams/{id}/events", get(get_team_events))
        .route("/teams/{id}/pause", post(pause_team))
        .route("/teams/{id}/resume", post(resume_team))
        .route("/teams/{id}/approve", post(approve_team))
        .route("/teams/{id}/abort", post(abort_team))
        .route("/teams/{id}/cleanup", post(cleanup_team))
        // SPEC-team §2.3: the issue queue of a running team.
        .route("/teams/{id}/issues", post(add_team_issues))
        .route("/teams/{id}/issues/{issue_id}", delete(remove_team_issue))
        .route("/teams/{id}/close-issue", post(close_team_issue))
        .route("/teams/{id}/say", post(say_team))
        .route("/teams/{id}/answer", post(answer_team))
        .route("/teams/{tid}/tasks/{task_id}/decide", post(decide_team_task))
        .route("/bots/{id}", patch(patch_bot).delete(delete_bot))
        // SPEC §6.9: 一鍵把等著套用 claude 更新的閒置 bot 全部 exit + resume。放在 `{id}` 那組
        // 前面——axum 的 `/bots/{id}` 會把 `restart-idle` 當成 bot id 吃掉。
        .route("/bots/restart-idle", post(restart_idle_bots))
        .route("/bots/{id}/start", post(start_bot))
        .route("/bots/{id}/restart", post(restart_bot))
        .route("/bots/{id}/stop", post(stop_bot))
        .route("/bots/{id}/interrupt", post(interrupt_bot))
        .route("/bots/{id}/login", post(login_bot))
        .route("/bots/{id}/pane/move-to-tab", post(move_bot_pane_to_tab))
        .route("/bots/{id}/prompt", post(prompt_bot))
        .route(
            "/bots/{id}/attachments",
            post(upload_attachment).layer(DefaultBodyLimit::max(crate::attach::MAX_BYTES + 4096)),
        )
        .route("/attachments/{id}", get(get_attachment))
        .route("/bots/{id}/keys", post(keys_bot))
        .route("/bots/{id}/text", post(text_bot))
        .route("/bots/{id}/messages", get(get_messages))
        .route("/bots/{id}/terminal", get(get_terminal))
        .route("/turns/{id}/abandon", post(abandon_turn))
        .route("/bots/{id}/abort", post(abort_bot))
        .route("/hosts", post(create_host))
        .route("/hosts/{name}", delete(delete_host))
        .route("/hosts/{name}/reconnect", post(reconnect_host))
        .route("/hosts/{name}/tools/refresh", post(refresh_tools))
        .route("/hosts/{name}/tools/install", post(install_tool))
        .route("/hosts/{name}/gh", get(get_gh_status))
        .route("/hosts/{name}/gh/login", post(login_gh))
        .route("/hosts/{name}/gh/cancel", post(cancel_gh))
        .route("/hosts/{name}/shells", get(list_host_shells).post(open_host_shell))
        .route("/hosts/{name}/shells/{pane_id}", delete(close_host_shell))
        .route("/hosts/{name}/shells/{pane_id}/terminal", get(get_host_shell_terminal))
        .route("/hosts/{name}/shells/{pane_id}/text", post(host_shell_text))
        .route("/hosts/{name}/shells/{pane_id}/keys", post(host_shell_keys))
        .route("/models", get(get_models))
        .route("/quota", get(get_quota))
        .route("/mem", get(get_mem))
        .route("/mem/processes", get(get_mem_processes))
        .route("/mem/processes/kill", post(kill_mem_process))
        .route("/mem/processes/pane", get(get_mem_pane))
        .route("/search/messages", get(search_messages))
        .route("/bots/{id}/restore", post(restore_bot))
        .route("/identities", post(create_identity))
        .route("/identities/{name}", delete(delete_identity))
        .route("/fs/dirs", get(list_dirs))
        .layer(axum::middleware::from_fn_with_state(app.clone(), auth))
        .route("/session", get(get_session));

    Router::new()
        .nest("/api", api)
        .route("/ws", get(ws_handler))
        .route("/hook/{provider}", post(crate::hookrecv::receive))
        .fallback(get(crate::assets::serve))
        .with_state(app)
}

// ---------------------------------------------------------------- auth

/// The peer address of the TCP connection, **not** the `Host` header: the header is chosen
/// freely by the caller, so with `server.listen = "0.0.0.0:…"` anyone on the LAN could ask
/// for the UI token with `curl -H 'Host: localhost:…'`. Needs the router to be served with
/// `into_make_service_with_connect_info::<SocketAddr>()` (main.rs).
///
/// `allow_lan` (on for every dev run, off inside the packaged macOS app — see
/// `main.rs::dev_lan_default`, which also binds every interface) accepts any peer. Binding 0.0.0.0 while still rejecting
/// everything non-loopback would make the daemon reachable but useless; and "LAN" in practice
/// includes overlay networks like Tailscale (100.64.0.0/10), not just RFC1918, so an allowlist
/// of ranges chases an open-ended set. `allow_lan` is itself the explicit dev-only opt-in.
fn peer_is_local(peer: &std::net::SocketAddr, allow_lan: bool) -> bool {
    if allow_lan {
        return true;
    }
    match peer.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|m| m.is_loopback()),
    }
}

/// A5: parse the Origin and compare the **host** exactly. `starts_with` used to let
/// `http://localhost.attacker.com` through, and `null` (file://) is not a supported caller.
///
/// The port is deliberately *not* pinned to `listen`: the dev UI is served by Vite on another
/// local port and its proxy forwards the browser's Origin verbatim (`web/vite.config.ts` only
/// rewrites `Host`), so pinning it would reject every dev-server request. Cross-origin reads
/// still need the UI token.
///
/// `allow_lan` accepts any Origin — see `peer_is_local` above; same rationale.
fn origin_is_local(headers: &HeaderMap, _port: u16, allow_lan: bool) -> bool {
    if allow_lan {
        return true;
    }
    let Some(o) = headers.get("origin").and_then(|v| v.to_str().ok()) else { return true };
    let Some(rest) = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")) else { return false };
    // Reject anything with a path / userinfo; an Origin is scheme + host + optional port.
    if rest.contains('/') || rest.contains('@') {
        return false;
    }
    let host = match rest.rsplit_once(':') {
        // `[::1]` has colons of its own: only treat the tail as a port when it is numeric.
        Some((h, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => h,
        _ => rest,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

async fn auth(State(app): State<Arc<App>>, req: axum::extract::Request, next: Next) -> Response {
    let headers = req.headers().clone();
    if !origin_is_local(&headers, app.port, app.allow_lan) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "bad origin"}))).into_response();
    }
    let tok = headers.get("X-AM-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if tok != app.ui_token {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or bad X-AM-Token"}))).into_response();
    }
    next.run(req).await
}

async fn get_session(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !peer_is_local(&peer, app.allow_lan) || !origin_is_local(&headers, app.port, app.allow_lan) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "non-local request"}))).into_response();
    }
    Json(json!({"token": app.ui_token, "port": app.port})).into_response()
}

// ---------------------------------------------------------------- state

fn lamp(connected: bool, run: Option<&db::Run>) -> &'static str {
    if !connected {
        return "disconnected";
    }
    match run {
        None => "offline",
        Some(r) => match r.state.as_str() {
            "starting" => "starting",
            "stopping" => "stopping",
            "stopped" | "exited" => "offline",
            _ => match r.agent_status.as_str() {
                "idle" => "idle",
                "working" => "working",
                "blocked" => "blocked",
                _ => "unknown",
            },
        },
    }
}

/// `hosts[]` for `GET /api/state` (SPEC §11.6). `local` always comes first.
async fn hosts_list(app: &Arc<App>) -> Vec<Value> {
    let mut out = Vec::new();
    let tools = app.tools.lock().await.clone();
    for c in app.hosts.list().await {
        let connected = if c.is_local() { app.connected.load(Ordering::SeqCst) } else { c.is_connected() };
        let t = tools.get(&c.name);
        out.push(json!({
            "name": c.name,
            "ssh": c.cfg.as_ref().map(|x| x.ssh.clone()),
            "ssh_port": c.cfg.as_ref().map(|x| x.ssh_port),
            "ssh_opts": c.cfg.as_ref().map(|x| x.ssh_opts.clone()).unwrap_or_default(),
            "herdr_session": c.cfg.as_ref().map(|x| x.herdr_session.clone()).unwrap_or_else(|| app.herdr_session.clone()),
            "remote_path": c.cfg.as_ref().map(|x| x.remote_path.clone()),
            "connected": connected,
            "error": c.error_string().await,
            // v4.0
            "attach_command": crate::config::attach_command(c.cfg.as_ref(), &app.herdr_session),
            "tools": t.map(|x| json!(x.tools)),
            // v4.0: per-identity login state *on this host* (same shape as `tools`), covering
            // both `[[identities]]` and the `ccN` aliases read off this host (SPEC §16).
            "identities": t.map(|x| json!(x.identities)),
            // v4.1: the raw `ccN` aliases this host defines (env unexpanded, as written).
            "shell_identities": t.map(|x| json!(x.shell_identities)),
            "tools_checked_at": t.map(|x| x.checked_at.clone()),
        }));
    }
    out
}

pub async fn state_json(app: &Arc<App>) -> Result<Value, LcError> {
    let connected = app.connected.load(Ordering::SeqCst);
    let projects = db::live_projects(&app.db).await.map_err(any_err)?;
    let bots = db::live_bots(&app.db).await.map_err(any_err)?;
    let mut out = Vec::new();
    for p in projects {
        let mut bl = Vec::new();
        for b in bots.iter().filter(|b| b.project_id == p.id) {
            let run = db::active_run(&app.db, &b.id).await.map_err(any_err)?;
            let queued_turn = db::queued_turn_for_bot(&app.db, &b.id).await.map_err(any_err)?;
            let bot_connected = app.bot_connected(&b.id).await;
            bl.push(json!({
                "id": b.id,
                "project_id": b.project_id,
                "name": b.name,
                "kind": b.kind,
                "model": b.model,
                "effort": b.effort,
                "fast": b.fast == 1,
                "persona": b.persona,
                "args": b.args(),
                "autostart": b.autostart == 1,
                "inject_hooks": b.inject_hooks == 1,
                "auto_approve": b.auto_approve == 1,
                "identity": b.identity,
                "env": b.env(),
                "herdr_session": b.herdr_session.clone(),
                // SPEC-team §10.2: `user` for config.toml bots, `team` for team members.
                "managed_by": b.managed_by,
                "parent_bot_id": b.parent_bot_id,
                "cwd": b.cwd,
                "team": crate::team::bot_team_json(b),
                // herdr agent name: the live run's, else what the next start will use.
                "agent_name": run.as_ref().and_then(|r| r.agent_name.clone()).unwrap_or_else(|| crate::config::agent_name(&p.label, &b.id)),
                "run": run,
                // The queued web prompt is durable in SQLite; the UI only renders this state.
                "queued_turn": queued_turn,
                "lamp": lamp(bot_connected, run.as_ref()),
                "unread": 0,
            }));
        }
        out.push(json!({
            "id": p.id, "path": p.path, "label": p.label, "host": p.host,
            "workspace_id": p.workspace_id,
            "github": crate::github::cached(app, &p.id).await,
            "bots": bl,
            // SPEC-team §10.2
            "teams": crate::team::teams_json_for_project(app, &p.id).await,
        }));
    }
    Ok(json!({
        "daemon_seq": app.current_seq(),
        "connected": connected,
        "default_connected": app.default_connected.load(Ordering::SeqCst),
        "herdr_session": app.herdr_session,
        "hosts": hosts_list(app).await,
        "identities": app.cfg.get().await.identities,
        "projects": out,
    }))
}

async fn get_state(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(state_json(&app).await?))
}

// ---------------------------------------------------------------- config mutation

async fn reproject(app: &Arc<App>) -> Result<(), LcError> {
    crate::projection::project_config(&app.cfg, &app.db).await.map_err(any_err)
}

#[derive(Deserialize)]
struct NewProject {
    path: String,
    label: Option<String>,
    /// `"local"` (default) or a configured host name.
    host: Option<String>,
}

#[derive(Deserialize)]
struct DirsQuery {
    path: Option<String>,
    host: Option<String>,
    /// `1` / `true` also lists dot-directories (`.config`, `.claude`, …).
    hidden: Option<String>,
}

/// Directory browser for the "new project" picker. Lists only directories (no files),
/// hides dot-entries unless `hidden=1`, never follows into unreadable places, and reports the parent.
async fn list_dirs(State(app): State<Arc<App>>, Query(q): Query<DirsQuery>) -> Result<Json<Value>, LcError> {
    // SPEC §11.5: the same JSON, produced by a remote `sh` snippet.
    let host = q.host.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    let hidden = matches!(q.hidden.as_deref(), Some("1" | "true" | "yes"));
    if host != LOCAL_HOST {
        let conn = app.hosts.get(&host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
        let v = crate::hosts::remote_list_dirs(&conn, q.path.as_deref(), hidden)
            .await
            .map_err(|e| LcError::Upstream(format!("{e:#}")))?;
        return Ok(Json(v));
    }
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
    let raw = q.path.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| home.to_string_lossy().to_string());
    let raw = if let Some(rest) = raw.strip_prefix("~") { format!("{}{}", home.display(), rest) } else { raw };
    let path = std::fs::canonicalize(&raw).map_err(|e| LcError::Bad(format!("{raw}: {e}")))?;
    if !path.is_dir() {
        return Err(LcError::Bad(format!("{} is not a directory", path.display())));
    }
    let rd = tokio::task::spawn_blocking({
        let path = path.clone();
        move || -> std::io::Result<Vec<Value>> {
            let mut out = Vec::new();
            for ent in std::fs::read_dir(&path)? {
                let Ok(ent) = ent else { continue };
                let name = ent.file_name().to_string_lossy().to_string();
                if name.starts_with('.') && !hidden {
                    continue;
                }
                let Ok(ft) = ent.file_type() else { continue };
                let is_dir = if ft.is_symlink() { ent.path().is_dir() } else { ft.is_dir() };
                if !is_dir {
                    continue;
                }
                let full = path.join(&name);
                let has_git = full.join(".git").exists();
                out.push(json!({"name": name, "path": full.to_string_lossy(), "git": has_git}));
            }
            out.sort_by(|a, b| {
                a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
            });
            Ok(out)
        }
    })
    .await
    .map_err(any_err)?
    .map_err(|e| LcError::Bad(format!("{}: {e}", path.display())))?;
    let parent = path.parent().map(|p| p.to_string_lossy().to_string());
    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "parent": parent,
        "home": home.to_string_lossy(),
        "entries": rd,
    })))
}

async fn create_project(State(app): State<Arc<App>>, Json(b): Json<NewProject>) -> Result<Response, LcError> {
    let host = b.host.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    let path = if host == LOCAL_HOST {
        canonical_path(&b.path).map_err(|e| LcError::Bad(e.to_string()))?
    } else {
        // Remote paths cannot be canonicalized locally; ask the host (SPEC §11.6).
        let conn = app.hosts.get(&host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
        crate::hosts::remote_canonical_dir(&conn, &b.path)
            .await
            .map_err(|e| LcError::Bad(format!("{e:#}")))?
    };
    let label = b.label.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
        std::path::Path::new(&path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| path.clone())
    });
    let id = db::ulid();
    let res = app
        .cfg
        .update(|cfg| {
            if cfg.projects.iter().any(|p| p.path == path && p.host == host) {
                anyhow::bail!("duplicate");
            }
            cfg.projects.push(crate::config::ProjectCfg {
                id: Some(id.clone()),
                path: path.clone(),
                label: label.clone(),
                host: host.clone(),
                bots: vec![],
            });
            Ok(())
        })
        .await;
    match res {
        Ok(()) => {}
        Err(e) if e.to_string() == "duplicate" => {
            return Err(LcError::conflict("project path already registered", json!({"path": path})))
        }
        Err(e) => return Err(any_err(e)),
    }
    reproject(&app).await?;
    // v4.0: GitHub origin detection for the new project (blocking is fine: one git call).
    if let Ok(Some(p)) = db::project(&app.db, &id).await {
        crate::github::detect_project(&app, &p).await;
    }
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"project_id": id}))).into_response())
}

// ---------------------------------------------------------------- v4.0: GitHub

async fn refresh_github(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let p = db::project(&app.db, &id)
        .await
        .map_err(any_err)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let gh = crate::github::detect_project(&app, &p).await;
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"project_id": id, "github": gh}))).into_response())
}

#[derive(Deserialize)]
struct IssuesQuery {
    state: Option<String>,
    limit: Option<u32>,
    q: Option<String>,
    refresh: Option<String>,
    /// Submodule path (relative to the project); absent or empty = the project itself.
    repo: Option<String>,
}

/// `GET /api/projects/:id/submodules?refresh=1` — the project's `.gitmodules` entries with
/// their GitHub origins, so the UI can offer a submodule's issues next to the project's.
async fn get_submodules(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<IssuesQuery>,
) -> Result<Json<Value>, LcError> {
    let p = db::project(&app.db, &id)
        .await
        .map_err(any_err)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let subs = crate::github::list_submodules(&app, &p, flag(&q.refresh)).await?;
    Ok(Json(json!({"project_id": id, "submodules": subs})))
}

// ---------------------------------------------------------------- quick git (chat header chip)

async fn get_git(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    Ok(Json(serde_json::to_value(crate::git_quick::summary(&app, &id).await?).unwrap_or_default()))
}

async fn git_commit(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<crate::git_quick::CommitBody>,
) -> Result<Json<Value>, LcError> {
    Ok(Json(crate::git_quick::commit(&app, &id, &b.message).await?))
}

async fn git_push(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    Ok(Json(crate::git_quick::push(&app, &id).await?))
}

async fn git_pull(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    Ok(Json(crate::git_quick::pull(&app, &id).await?))
}

async fn get_issues(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<IssuesQuery>,
) -> Result<Json<Value>, LcError> {
    let state = q.state.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "open".into());
    let repo = q.repo.clone().unwrap_or_default();
    let v = crate::github::list_issues(&app, &id, &repo, &state, q.limit.unwrap_or(30), q.q.as_deref(), flag(&q.refresh)).await?;
    Ok(Json(v))
}

async fn get_issue(
    State(app): State<Arc<App>>,
    Path((id, number)): Path<(String, u64)>,
    Query(q): Query<IssuesQuery>,
) -> Result<Json<Value>, LcError> {
    Ok(Json(crate::github::get_issue(&app, &id, q.repo.as_deref().unwrap_or(""), number).await?))
}

// ---------------------------------------------------------------- SPEC-team §10: teams

/// `POST /api/projects/:id/teams` — 200 `{team_id}`; the members are created but not started
/// yet, so the caller follows along on the WS.
async fn create_team(
    State(app): State<Arc<App>>,
    Path(pid): Path<String>,
    Json(b): Json<crate::team::CreateTeam>,
) -> Result<Response, LcError> {
    let out = crate::team::create(&app, &pid, b).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// `GET /api/teams/:id` — the state object plus `tasks[]`, `roles`, base and worktree root.
async fn get_team(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    Ok(Json(crate::team::detail(&app, &id).await?))
}

/// `DELETE /api/teams/:id?branches=keep|delete` — SPEC-team §6.5a.
///
/// Any phase; a live team is stopped on the way out. `branches=delete` is the one flag that
/// destroys work, so it is opt-in and never inferred; anything else (including a typo) is
/// read as `keep`. Remote branches are never touched.
async fn delete_team(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, LcError> {
    let branches = q.get("branches").map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
    if !branches.is_empty() && !["keep", "delete"].contains(&branches.as_str()) {
        return Err(LcError::Bad("branches must be `keep` or `delete`".into()));
    }
    let out = crate::team::delete(&app, &id, branches == "delete").await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// `GET /api/teams/:id/events?before=&limit=` — the team log, oldest-first within a page.
async fn get_team_events(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let limit: i64 = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(100);
    let before = q.get("before").map(|s| s.as_str()).filter(|s| !s.is_empty());
    Ok(Json(crate::team::events(&app, &id, before, limit).await?))
}

async fn patch_team(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<crate::team::PatchTeam>,
) -> Result<Response, LcError> {
    Ok((StatusCode::OK, Json(crate::team::patch(&app, &id, b).await?)).into_response())
}

async fn pause_team(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    Ok((StatusCode::OK, Json(crate::team::pause(&app, &id).await?)).into_response())
}

async fn resume_team(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    Ok((StatusCode::OK, Json(crate::team::resume(&app, &id).await?)).into_response())
}

/// `POST /api/teams/:id/close-issue` — SPEC-team §10.7.
///
/// The user's consent *is* this request: the daemon never closes an issue on its own, and the
/// UI only offers the action on a team that reached `done`.
#[derive(serde::Deserialize)]
struct CloseIssueBody {
    /// Absent = the daemon writes its own summary comment; `""` = close with no comment.
    #[serde(default)]
    comment: Option<String>,
    /// Optional queued-issue id. Omitted keeps the legacy current-issue behaviour.
    #[serde(default)]
    issue_id: Option<String>,
}

/// The body is read as bytes rather than `Json<…>` because every field in it is optional:
/// `POST` with no body at all, with `{}`, or with a JSON content-type and an empty body all
/// mean the same thing ("close it, write the default comment"), and `Json` rejects the last
/// two with a parse error.
async fn close_team_issue(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, LcError> {
    let (issue_id, comment) = match std::str::from_utf8(&body).unwrap_or("").trim() {
        "" => (None, None),
        s => {
            let b = serde_json::from_str::<CloseIssueBody>(s).map_err(|e| LcError::Bad(format!("bad body: {e}")))?;
            (b.issue_id, b.comment)
        }
    };
    Ok((StatusCode::OK, Json(crate::team::close_issue_for(&app, &id, issue_id.as_deref(), comment).await?)).into_response())
}

#[derive(serde::Deserialize)]
struct AddIssues {
    #[serde(default)]
    issue_numbers: Vec<i64>,
    /// Single-issue convenience, matching `POST /projects/:id/teams`.
    #[serde(default)]
    issue_number: Option<i64>,
}

async fn add_team_issues(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<AddIssues>,
) -> Result<Response, LcError> {
    let mut ns = b.issue_numbers;
    if let Some(n) = b.issue_number {
        if !ns.contains(&n) {
            ns.push(n);
        }
    }
    Ok((StatusCode::OK, Json(crate::team::add_issues(&app, &id, &ns).await?)).into_response())
}

async fn remove_team_issue(
    State(app): State<Arc<App>>,
    Path((id, issue_id)): Path<(String, String)>,
) -> Result<Response, LcError> {
    Ok((StatusCode::OK, Json(crate::team::remove_queued_issue(&app, &id, &issue_id).await?)).into_response())
}

async fn approve_team(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    Ok((StatusCode::OK, Json(crate::team::approve(&app, &id).await?)).into_response())
}

#[derive(Deserialize)]
struct AbortTeam {
    #[serde(default)]
    reason: Option<String>,
}

async fn abort_team(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    body: Option<Json<AbortTeam>>,
) -> Result<Response, LcError> {
    let reason = body.and_then(|Json(b)| b.reason).filter(|s| !s.trim().is_empty());
    Ok((StatusCode::OK, Json(crate::team::abort(&app, &id, reason.as_deref()).await?)).into_response())
}

async fn cleanup_team(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    Ok((StatusCode::OK, Json(crate::team::cleanup(&app, &id).await?)).into_response())
}

#[derive(Deserialize)]
struct SayIn {
    text: String,
    /// `pm` (default) | `reviewer` | a member short name | a bot id.
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    client_request_id: Option<String>,
}

async fn say_team(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<SayIn>,
) -> Result<Response, LcError> {
    let to = b.to.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "pm".into());
    let crid = b.client_request_id.unwrap_or_else(db::ulid);
    Ok((StatusCode::OK, Json(crate::team::say(&app, &id, &b.text, &to, &crid).await?)).into_response())
}

#[derive(Deserialize)]
struct AnswerIn {
    text: String,
    #[serde(default)]
    client_request_id: Option<String>,
}

async fn answer_team(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<AnswerIn>,
) -> Result<Response, LcError> {
    let crid = b.client_request_id.unwrap_or_else(db::ulid);
    Ok((StatusCode::OK, Json(crate::team::answer(&app, &id, &b.text, &crid).await?)).into_response())
}

#[derive(Deserialize)]
struct DecideIn {
    action: String,
    #[serde(default)]
    note: Option<String>,
}

async fn decide_team_task(
    State(app): State<Arc<App>>,
    Path((tid, task_id)): Path<(String, String)>,
    Json(b): Json<DecideIn>,
) -> Result<Response, LcError> {
    let out = crate::team::decide(&app, &tid, &task_id, &b.action, b.note.as_deref()).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

#[derive(Deserialize)]
struct PatchProject {
    label: Option<String>,
}

/// `PATCH /api/projects/:id` `{"label"}` — rename a project. Never blocked by a live run:
/// a bot's herdr identity is derived from its bot id, so only the legacy names and the
/// `agent_name` slug of the *next* start follow the label, hence `needs_restart: false`.
async fn patch_project(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<PatchProject>,
) -> Result<Response, LcError> {
    let label = match &b.label {
        None => None,
        Some(l) => {
            let l = l.trim();
            if l.is_empty() {
                return Err(LcError::Bad("project label must not be empty".into()));
            }
            Some(l.to_string())
        }
    };
    app.cfg
        .update(|cfg| {
            let p = cfg
                .projects
                .iter_mut()
                .find(|p| p.id.as_deref() == Some(id.as_str()))
                .ok_or_else(|| anyhow::anyhow!("no-project"))?;
            if let Some(l) = &label {
                p.label = l.clone();
            }
            Ok(())
        })
        .await
        .map_err(|e| if e.to_string() == "no-project" { LcError::NotFound("project".into()) } else { any_err(e) })?;
    reproject(&app).await?;
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"project_id": id, "needs_restart": false}))).into_response())
}

async fn delete_project(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let bots = db::live_bots(&app.db).await.map_err(any_err)?;
    for b in bots.iter().filter(|b| b.project_id == id) {
        if db::active_run(&app.db, &b.id).await.map_err(any_err)?.is_some() {
            return Err(LcError::conflict("all bots must be stopped first", json!({"bot_id": b.id})));
        }
    }
    // SPEC-team §5.4: a live team owns bots and worktrees under this project.
    for t in db::teams_of_project(&app.db, &id).await.map_err(any_err)? {
        if !crate::team::is_terminal(&t.phase) {
            return Err(LcError::conflict("team is still running", json!({"team_id": t.id, "phase": t.phase})));
        }
    }
    app.cfg
        .update(|cfg| {
            cfg.projects.retain(|p| p.id.as_deref() != Some(id.as_str()));
            Ok(())
        })
        .await
        .map_err(any_err)?;
    reproject(&app).await?;
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

#[derive(Deserialize)]
struct NewBot {
    name: String,
    /// 2026-09-08: the sidebar's quick-add chips compute `<identity>-<n>` from the browser's
    /// bot list, which can be a beat behind right after a create — so the second click sent the
    /// same `cc1-1` and got a 409. With this set the daemon picks the next free suffix itself
    /// (`name` minus any trailing `-<n>` is the base) and returns the name it used.
    #[serde(default)]
    name_auto: bool,
    kind: String,
    /// claude `--model <m>` / codex `-m <m>` / grok `-m <m>`; omitted / null / "" = the CLI's own default.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    /// v4.0: codex Fast tier.
    #[serde(default)]
    fast: bool,
    /// v4.0: appended to the agent's system prompt.
    #[serde(default)]
    persona: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    autostart: bool,
    #[serde(default)]
    inject_hooks: Option<bool>,
    #[serde(default)]
    auto_approve: Option<bool>,
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
}

/// `None` = no identity requested; `Some(name)` = must exist **on that bot's host** and match
/// `kind` — the list is `[[identities]]` plus that host's `ccN` aliases (SPEC §16).
async fn check_identity(app: &Arc<App>, host: &str, identity: &Option<String>, kind: &str) -> Result<Option<String>, LcError> {
    let Some(name) = identity.clone().filter(|s| !s.trim().is_empty()) else { return Ok(None) };
    let Some(id) = crate::tools::identity_for_host(app, host, &name).await else {
        return Err(LcError::NotFound("identity".into()));
    };
    if id.kind != kind {
        return Err(LcError::Bad(format!("identity `{name}` is for {} but this bot is {kind}", id.kind)));
    }
    Ok(Some(name))
}

async fn create_bot(
    State(app): State<Arc<App>>,
    Path(pid): Path<String>,
    Json(b): Json<NewBot>,
) -> Result<Response, LcError> {
    if !valid_bot_name(&b.name) {
        return Err(LcError::Bad(format!("bot name: {}", crate::config::BOT_NAME_RE)));
    }
    if !crate::config::valid_kind(&b.kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::kinds_list())));
    }
    // The identity has to exist on the *project's* host — `cc1` is a different account there.
    let host = db::project(&app.db, &pid)
        .await
        .map_err(any_err)?
        .map(|p| p.host)
        .unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let identity = check_identity(&app, &host, &b.identity, &b.kind).await?;
    // v4.0: effort is kind-dependent (claude → always None).
    let effort = crate::config::normalize_effort(&b.kind, b.effort.as_deref()).map_err(LcError::Bad)?;
    let env: BTreeMap<String, String> = b.env.clone().unwrap_or_default();
    let id = db::ulid();
    let used_name = std::sync::Mutex::new(b.name.clone());
    let res = app
        .cfg
        .update(|cfg| {
            let p = cfg
                .projects
                .iter_mut()
                .find(|p| p.id.as_deref() == Some(pid.as_str()))
                .ok_or_else(|| anyhow::anyhow!("no-project"))?;
            // Bot names are unique per project (the herdr agent name is `<project>-<bot>`).
            let taken = |n: &str| p.bots.iter().any(|x| x.name == n);
            let name = if taken(&b.name) {
                if !b.name_auto {
                    anyhow::bail!("duplicate-name");
                }
                next_free_name(&b.name, &taken)
            } else {
                b.name.clone()
            };
            *used_name.lock().unwrap() = name.clone();
            p.bots.push(crate::config::BotCfg {
                id: Some(id.clone()),
                name,
                kind: b.kind.clone(),
                model: b.model.clone().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()),
                effort: effort.clone(),
                fast: b.fast,
                persona: b.persona.clone().filter(|s| !s.trim().is_empty()),
                args: b.args.clone(),
                autostart: b.autostart,
                inject_hooks: b.inject_hooks.unwrap_or(true),
                auto_approve: b.auto_approve.unwrap_or(true),
                identity: identity.clone(),
                env: env.clone(),
                herdr_session: None,
            });
            Ok(())
        })
        .await;
    match res {
        Ok(()) => {}
        Err(e) if e.to_string() == "duplicate-name" => {
            return Err(LcError::conflict("bot name already in use", json!({"name": b.name})))
        }
        Err(e) if e.to_string() == "no-project" => return Err(LcError::NotFound("project".into())),
        Err(e) => return Err(any_err(e)),
    }
    reproject(&app).await?;
    app.emit("bot_changed", json!({"bot_id": id})).await;
    let name = used_name.into_inner().unwrap_or_default();
    Ok((StatusCode::OK, Json(json!({"bot_id": id, "name": name}))).into_response())
}

/// `cc1-1` taken → `cc1-2`, `cc1-3`, …; a name with no numeric suffix (`review`) becomes
/// `review-2`. Always stays inside the 32-char bot-name limit by trimming the base.
fn next_free_name(wanted: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let base = match wanted.rfind('-') {
        Some(i) if wanted[i + 1..].chars().all(|c| c.is_ascii_digit()) && i + 1 < wanted.len() => &wanted[..i],
        _ => wanted,
    };
    for n in 1u32.. {
        let suffix = format!("-{n}");
        let room = 32usize.saturating_sub(suffix.len());
        let candidate = format!("{}{suffix}", &base[..base.len().min(room)]);
        if candidate != wanted && !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// `POST /api/order` — 側欄的專案／bot 排序。
///
/// 順序本來只存在瀏覽器的 localStorage，所以同一個 daemon 在手機上跟桌機上長得不一樣
/// （使用者 2026-09-09 回報）。config.toml 的陣列順序本身就是順序，把它寫回去就等於
/// 全裝置一致，也不用另開一份狀態。沒列到的（別的 client 剛新增的）維持相對順序接在後面。
#[derive(Deserialize)]
struct SetOrder {
    /// 專案 id，由上而下。
    #[serde(default)]
    projects: Option<Vec<String>>,
    /// `project_id` → 該專案的 bot id，由上而下。config.toml 沒有的（child bot）忽略。
    #[serde(default)]
    bots: Option<BTreeMap<String, Vec<String>>>,
}

/// `want` 給的順序排 `items`，沒被點名的維持原相對順序接在後面。
fn reorder_by<T>(items: &mut Vec<T>, want: &[String], id_of: impl Fn(&T) -> Option<String>) {
    let rank: BTreeMap<&str, usize> = want.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
    let n = want.len();
    let mut keyed: Vec<(usize, usize, T)> = std::mem::take(items)
        .into_iter()
        .enumerate()
        .map(|(i, it)| {
            let r = id_of(&it).and_then(|id| rank.get(id.as_str()).copied()).unwrap_or(n + i);
            (r, i, it)
        })
        .collect();
    keyed.sort_by_key(|(r, i, _)| (*r, *i));
    items.extend(keyed.into_iter().map(|(_, _, it)| it));
}

async fn set_order(State(app): State<Arc<App>>, Json(b): Json<SetOrder>) -> Result<Response, LcError> {
    if b.projects.is_none() && b.bots.is_none() {
        return Err(LcError::Bad("order: projects 或 bots 至少要有一個".into()));
    }
    app.cfg
        .update(|cfg| {
            if let Some(want) = &b.projects {
                reorder_by(&mut cfg.projects, want, |p| p.id.clone());
            }
            if let Some(map) = &b.bots {
                for p in cfg.projects.iter_mut() {
                    let Some(want) = p.id.as_deref().and_then(|id| map.get(id)) else { continue };
                    reorder_by(&mut p.bots, want, |x| x.id.clone());
                }
            }
            Ok(())
        })
        .await
        .map_err(any_err)?;
    reproject(&app).await?;
    app.emit("project_changed", json!({"reason": "order"})).await;
    Ok((StatusCode::OK, Json(json!({"ok": true}))).into_response())
}

#[derive(Deserialize)]
struct PatchBot {
    /// `Some(Some(m))` sets, `Some(None)` / `Some("")` clears, absent = unchanged.
    #[serde(default, deserialize_with = "double_option")]
    model: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    effort: Option<Option<String>>,
    /// v4.0
    fast: Option<bool>,
    /// v4.0: `Some(None)` / `Some("")` clears.
    #[serde(default, deserialize_with = "double_option")]
    persona: Option<Option<String>>,
    args: Option<Vec<String>>,
    autostart: Option<bool>,
    name: Option<String>,
    inject_hooks: Option<bool>,
    auto_approve: Option<bool>,
    /// `Some(Some(name))` binds, `Some(None)` / `Some("")` unbinds, absent = unchanged.
    #[serde(default, deserialize_with = "double_option")]
    identity: Option<Option<String>>,
    env: Option<BTreeMap<String, String>>,
}

fn double_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}

async fn patch_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<PatchBot>,
) -> Result<Response, LcError> {
    let active = db::active_run(&app.db, &id).await.map_err(any_err)?;
    if let Some(n) = &b.name {
        if !valid_bot_name(n) {
            return Err(LcError::Bad(format!("bot name: {}", crate::config::BOT_NAME_RE)));
        }
        // v3.8: the name is a nickname (herdr sees `<project>-<hash>`), so renaming is free.
        let me = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
        if db::live_bots(&app.db)
            .await
            .map_err(any_err)?
            .iter()
            .any(|x| x.id != id && x.project_id == me.project_id && &x.name == n)
        {
            return Err(LcError::conflict("bot name already in use in this project", json!({"name": n})));
        }
    }
    // Everything else may change while a run is live — it just needs a restart to take effect.
    let restart_relevant = b.model.is_some()
        || b.effort.is_some()
        || b.fast.is_some()
        || b.persona.is_some()
        || b.args.is_some()
        || b.identity.is_some()
        || b.env.is_some()
        || b.auto_approve.is_some()
        || b.inject_hooks.is_some();
    let needs_restart = active.is_some() && restart_relevant;
    let kind = db::bot(&app.db, &id).await.map_err(any_err)?.map(|x| x.kind).ok_or_else(|| LcError::NotFound("bot".into()))?;
    // v4.0: effort is kind-dependent; `Some(None)` clears.
    let effort: Option<Option<String>> = match &b.effort {
        None => None,
        Some(e) => Some(crate::config::normalize_effort(&kind, e.as_deref()).map_err(LcError::Bad)?),
    };
    if let Some(Some(name)) = &b.identity {
        if !name.trim().is_empty() {
            let host = db::bot_host(&app.db, &id).await.map_err(any_err)?;
            check_identity(&app, &host, &Some(name.clone()), &kind).await?;
        }
    }
    app.cfg
        .update(|cfg| {
            let bot = cfg
                .projects
                .iter_mut()
                .flat_map(|p| p.bots.iter_mut())
                .find(|x| x.id.as_deref() == Some(id.as_str()))
                .ok_or_else(|| anyhow::anyhow!("no-bot"))?;
            if let Some(idn) = &b.identity {
                bot.identity = idn.clone().filter(|s| !s.trim().is_empty());
            }
            if let Some(e) = &b.env {
                bot.env = e.clone();
            }
            if let Some(m) = &b.model {
                bot.model = m.clone().map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
            }
            if let Some(e) = &effort {
                bot.effort = e.clone();
            }
            if let Some(f) = b.fast {
                bot.fast = f;
            }
            if let Some(p) = &b.persona {
                bot.persona = p.clone().filter(|x| !x.trim().is_empty());
            }
            if let Some(a) = &b.args {
                bot.args = a.clone();
            }
            if let Some(a) = b.autostart {
                bot.autostart = a;
            }
            if let Some(n) = &b.name {
                bot.name = n.clone();
            }
            if let Some(h) = b.inject_hooks {
                bot.inject_hooks = h;
            }
            if let Some(a) = b.auto_approve {
                bot.auto_approve = a;
            }
            Ok(())
        })
        .await
        .map_err(|e| if e.to_string() == "no-bot" { LcError::NotFound("bot".into()) } else { any_err(e) })?;
    reproject(&app).await?;
    app.emit("bot_changed", json!({"bot_id": id})).await;
    // 有 slash 指令可以當場套用的欄位（grok effort / model、claude model）：
    // 只動這些欄位的話就不用重啟。grok 改模型時可以順便帶 effort（TUI `/model <id> <effort>`）。
    let extras = |skip: &[&str]| {
        let hit = |name: &str, present: bool| present && !skip.contains(&name);
        hit("model", b.model.is_some())
            || hit("effort", b.effort.is_some())
            || b.fast.is_some()
            || b.persona.is_some()
            || b.args.is_some()
            || b.identity.is_some()
            || b.env.is_some()
            || b.auto_approve.is_some()
            || b.inject_hooks.is_some()
    };
    // codex 的三個都能在執行中換（SPEC §4.4a）：`/model` 的兩層選單一次決定模型與強度，
    // `/fast` 開關 service tier。所以它可以一次收下 model / effort / fast 的任意組合，
    // claude / grok 則維持一次一個欄位（它們的 slash 指令就是一行一個值）。
    let live_fields: Vec<&str> = match kind.as_str() {
        "codex" if !extras(&["model", "effort", "fast"]) => ["model", "effort", "fast"]
            .into_iter()
            .filter(|f| match *f {
                "model" => b.model.is_some(),
                "effort" => b.effort.is_some(),
                _ => b.fast.is_some(),
            })
            .collect(),
        "grok" if b.model.is_some() && !extras(&["model", "effort"]) => vec!["model"],
        "grok" if b.effort.is_some() && !extras(&["effort"]) => vec!["effort"],
        "claude" if b.model.is_some() && !extras(&["model"]) => vec!["model"],
        "claude" if b.effort.is_some() && !extras(&["effort"]) => vec!["effort"],
        _ => Vec::new(),
    };
    let needs_restart = if needs_restart && !live_fields.is_empty() {
        !lifecycle::apply_live_setting(&app, &id, &live_fields).await
    } else {
        needs_restart
    };
    Ok((StatusCode::OK, Json(json!({"needs_restart": needs_restart}))).into_response())
}

async fn delete_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let bot = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    let host = db::bot_host(&app.db, &id).await.map_err(any_err)?;
    // 2026-09-08: the children it spawned go with it. They only exist as panes their parent
    // opened and rows the daemon adopted; left behind they would sit in the sidebar as
    // orphans with nothing to hang from. Deepest first, each stopped the same way.
    let mut removed_children = Vec::new();
    for child in descendant_children(&app, &id).await.map_err(any_err)?.into_iter().rev() {
        let _ = lifecycle::stop_bot(&app, &child.id).await;
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(db::now())
            .bind(&child.id)
            .execute(&app.db)
            .await
            .map_err(any_err)?;
        let child_host = db::bot_host(&app.db, &child.id).await.unwrap_or_else(|_| host.clone());
        lifecycle::purge_bot_dir(&app, &child.id, &child_host).await;
        app.emit("bot_changed", json!({"bot_id": child.id})).await;
        removed_children.push(child.id);
    }
    // SPEC §6.4: stop first (ctrl+c x2, pane closed on timeout), then drop the config entry.
    let _ = lifecycle::stop_bot(&app, &id).await;
    // A spawned child never entered config.toml, so the projection cannot retire it.
    if bot.managed_by == "child" {
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&id).execute(&app.db).await.map_err(any_err)?;
        lifecycle::purge_bot_dir(&app, &id, &host).await;
        app.emit("bot_changed", json!({"bot_id": id})).await;
        app.emit("project_changed", json!({"project_id": bot.project_id})).await;
        return Ok((StatusCode::OK, Json(json!({"removed_children": removed_children}))).into_response());
    }
    app.cfg
        .update(|cfg| {
            for p in cfg.projects.iter_mut() {
                p.bots.retain(|x| x.id.as_deref() != Some(id.as_str()));
            }
            Ok(())
        })
        .await
        .map_err(any_err)?;
    // Projection soft-deletes the row (`bots.deleted_at`); the conversation and its messages stay.
    reproject(&app).await?;
    lifecycle::purge_bot_dir(&app, &id, &host).await;
    app.emit("bot_changed", json!({"bot_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"removed_children": removed_children}))).into_response())
}

/// Every live `managed_by = 'child'` bot under `root`, parents before their children
/// (so `.rev()` deletes deepest first). Only spawned children follow the parent: a bot the
/// user created in config.toml is never someone's child.
async fn descendant_children(app: &Arc<App>, root: &str) -> anyhow::Result<Vec<db::Bot>> {
    let all = db::live_bots(&app.db).await?;
    let mut out = Vec::new();
    let mut frontier = vec![root.to_string()];
    while let Some(pid) = frontier.pop() {
        for b in all.iter().filter(|b| b.managed_by == "child" && b.parent_bot_id.as_deref() == Some(pid.as_str())) {
            if out.iter().any(|x: &db::Bot| x.id == b.id) {
                continue;
            }
            frontier.push(b.id.clone());
            out.push(b.clone());
        }
    }
    Ok(out)
}


/// `POST /api/bots/:id/restore` — 把誤刪的 bot 放回來。
///
/// 刪除本來就是軟的（`bots.deleted_at`，對話與訊息完整留著），所以「恢復」就是把 config.toml
/// 的條目寫回去、讓 projection 把 `deleted_at` 清掉——歷史會跟著整個回來。
///
/// 唯一救不回的是 bot 的工作目錄（刪除時 `purge_bot_dir` 真的砍了）：那裡面是 hook 設定與
/// 包裝腳本，下次啟動會重新產生，所以不影響復原。
///
/// `managed_by = "child"` 的 bot 從來沒進過 config.toml，projection 不管它，直接清欄位。
async fn restore_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    // `db::bot` 不過濾 deleted_at，所以軟刪除的也拿得到——這裡要的就是它。
    let bot = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_none() {
        return Err(LcError::conflict("bot is not deleted", json!({"bot_id": id})));
    }
    // 名字在專案內要唯一（docs/API.md）：同名的已經被建回來時，講清楚而不是默默失敗。
    let taken: Option<String> = sqlx::query_scalar(
        "SELECT id FROM bots WHERE project_id = ? AND name = ? AND deleted_at IS NULL AND id <> ?",
    )
    .bind(&bot.project_id)
    .bind(&bot.name)
    .bind(&id)
    .fetch_optional(&app.db)
    .await
    .map_err(any_err)?;
    if let Some(other) = taken {
        return Err(LcError::conflict(
            "bot name already in use in this project",
            json!({"bot_id": id, "name": bot.name, "taken_by": other}),
        ));
    }
    if bot.managed_by == "child" {
        sqlx::query("UPDATE bots SET deleted_at = NULL WHERE id = ?").bind(&id).execute(&app.db).await.map_err(any_err)?;
    } else {
        let entry = crate::config::BotCfg {
            id: Some(bot.id.clone()),
            name: bot.name.clone(),
            kind: bot.kind.clone(),
            model: bot.model.clone(),
            effort: bot.effort.clone(),
            fast: bot.fast != 0,
            persona: bot.persona.clone(),
            args: serde_json::from_str(&bot.args_json).unwrap_or_default(),
            autostart: bot.autostart != 0,
            inject_hooks: bot.inject_hooks != 0,
            auto_approve: bot.auto_approve != 0,
            identity: bot.identity.clone(),
            env: serde_json::from_str(&bot.env_json).unwrap_or_default(),
            herdr_session: None,
        };
        let pid = bot.project_id.clone();
        app.cfg
            .update(move |cfg| {
                let Some(p) = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())) else {
                    anyhow::bail!("the project this bot belonged to is gone")
                };
                if !p.bots.iter().any(|b| b.id.as_deref() == Some(entry.id.as_deref().unwrap_or_default())) {
                    p.bots.push(entry.clone());
                }
                Ok(())
            })
            .await
            .map_err(any_err)?;
        reproject(&app).await?;
    }
    app.emit("bot_changed", json!({"bot_id": id})).await;
    app.emit("project_changed", json!({"project_id": bot.project_id})).await;
    Ok((StatusCode::OK, Json(json!({"bot_id": id}))).into_response())
}

// ---------------------------------------------------------------- hosts (§11.6)

#[derive(Deserialize)]
struct NewHost {
    name: String,
    ssh: String,
    ssh_port: Option<u16>,
    ssh_opts: Option<Vec<String>>,
    herdr_session: Option<String>,
    remote_path: Option<String>,
    /// Accepted so an older client still posts cleanly; ignored since v4.3 (SPEC §11.4).
    hook_port: Option<u16>,
}

/// Write the `[[hosts]]` entry (upsert), then connect and report the outcome.
async fn create_host(State(app): State<Arc<App>>, Json(b): Json<NewHost>) -> Result<Response, LcError> {
    if b.name == LOCAL_HOST {
        return Err(LcError::Bad("`local` is reserved for this machine".into()));
    }
    if !valid_host_name(&b.name) {
        return Err(LcError::Bad(format!("host name must match {}", crate::config::SLUG_NAME_RE)));
    }
    if b.ssh.trim().is_empty() {
        return Err(LcError::Bad("ssh target must not be empty".into()));
    }
    let cfg = HostCfg {
        name: b.name.clone(),
        ssh: b.ssh.trim().to_string(),
        ssh_port: b.ssh_port.unwrap_or(22),
        ssh_opts: b.ssh_opts.unwrap_or_default(),
        herdr_session: b.herdr_session.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "agents-manager".into()),
        remote_path: b.remote_path.unwrap_or_default(),
        // v4.3: dropped on the way in, so saving a host from the UI also clears a stale value.
        hook_port: None,
    };
    if b.hook_port.is_some() {
        tracing::warn!(host = %b.name, "hook_port is ignored since v4.3 (remote hooks report through herdr; see SPEC §11.4)");
    }
    let c2 = cfg.clone();
    app.cfg
        .update(move |f| {
            match f.hosts.iter_mut().find(|h| h.name == c2.name) {
                Some(existing) => *existing = c2,
                None => f.hosts.push(c2),
            }
            Ok(())
        })
        .await
        .map_err(any_err)?;
    let hosts = app.cfg.get().await.hosts;
    app.hosts.apply_config(&app, &hosts).await;
    let (connected, error) = app.hosts.reconnect(&app, &b.name).await.unwrap_or((false, Some("host vanished".into())));
    app.emit("host_changed", json!({"name": b.name, "connected": connected, "error": error})).await;
    crate::state::emit_daemon_status(&app).await;
    Ok((StatusCode::OK, Json(json!({"name": b.name, "connected": connected, "error": error}))).into_response())
}

async fn delete_host(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    if name == LOCAL_HOST {
        return Err(LcError::Bad("`local` cannot be removed".into()));
    }
    for p in db::live_projects(&app.db).await.map_err(any_err)? {
        if p.host == name {
            return Err(LcError::conflict("host still used by projects", json!({"project_id": p.id})));
        }
    }
    let n2 = name.clone();
    app.cfg
        .update(move |f| {
            f.hosts.retain(|h| h.name != n2);
            Ok(())
        })
        .await
        .map_err(any_err)?;
    app.hosts.remove(&app, &name).await;
    app.emit("project_changed", json!({})).await;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

async fn reconnect_host(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    let (connected, error) = app.hosts.reconnect(&app, &name).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    Ok((StatusCode::OK, Json(json!({"name": name, "connected": connected, "error": error}))).into_response())
}

// ---------------------------------------------------------------- v4.0: tools / models / quota

/// `POST /api/hosts/:name/tools/refresh` — re-run CLI detection now.
async fn refresh_tools(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    if app.hosts.get(&name).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let ht = crate::tools::detect(&app, &name).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    if let Some(conn) = app.hosts.get(&name).await {
        crate::state::emit_host_changed(&app, &conn).await;
    }
    Ok((
        StatusCode::OK,
        Json(json!({
            "name": name,
            "tools": ht.tools,
            "identities": ht.identities,
            "shell_identities": ht.shell_identities,
            "tools_checked_at": ht.checked_at,
        })),
    )
        .into_response())
}

#[derive(Deserialize)]
struct InstallTool {
    kind: String,
    via_bot_id: String,
}

/// `POST /api/hosts/:name/tools/install` — ask a running agent on that host to install + log in.
async fn install_tool(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(b): Json<InstallTool>,
) -> Result<Response, LcError> {
    let out = crate::tools::install_via_bot(&app, &name, &b.kind, &b.via_bot_id).await?;
    Ok((StatusCode::OK, Json(json!({"turn_id": out.turn_id, "message_id": out.message_id, "delivery": out.delivery})))
        .into_response())
}

/// `GET /api/hosts/:name/gh` — whether `gh` on that host can talk to GitHub.
async fn get_gh_status(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    let v = crate::gh_auth::status(&app, &name).await?;
    Ok((StatusCode::OK, Json(v)).into_response())
}

#[derive(Default, Deserialize)]
struct GhLoginBody {
    mode: Option<String>,
    user: Option<String>,
}

/// `POST /api/hosts/:name/gh/login` — auto / copy / device / switch. See API.md.
async fn login_gh(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(b): Json<GhLoginBody>,
) -> Result<Response, LcError> {
    let v = crate::gh_auth::login(&app, &name, b.mode.as_deref(), b.user.as_deref()).await?;
    Ok((StatusCode::OK, Json(v)).into_response())
}

/// `POST /api/hosts/:name/gh/cancel` — drop an in-flight device-flow login.
async fn cancel_gh(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    let v = crate::gh_auth::cancel(&app, &name).await?;
    Ok((StatusCode::OK, Json(v)).into_response())
}

// ---------------------------------------------------------------- host shells

#[derive(Default, Deserialize)]
struct NewShell {
    cwd: Option<String>,
}

/// `POST /api/hosts/:name/shells` — open a plain shell pane on that host.
async fn open_host_shell(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    body: Option<Json<NewShell>>,
) -> Result<Response, LcError> {
    let cwd = body.and_then(|Json(b)| b.cwd);
    let s = shell::open(&app, &name, cwd.as_deref()).await?;
    Ok((StatusCode::OK, Json(json!(s))).into_response())
}

/// `GET /api/hosts/:name/shells` — the shells still alive on that host.
async fn list_host_shells(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    let shells = shell::list(&app, &name).await?;
    Ok((StatusCode::OK, Json(json!({"host": name, "shells": shells, "max": shell::MAX_PER_HOST}))).into_response())
}

/// `GET /api/hosts/:name/shells/:pane_id/terminal?source=&lines=` — shape as §7.
async fn get_host_shell_terminal(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let source = q.get("source").cloned().unwrap_or_else(|| "visible".into());
    let lines: u32 = q.get("lines").and_then(|s| s.parse().ok()).unwrap_or(200).clamp(1, 2000);
    Ok(Json(shell::read(&app, &name, &pane_id, &source, lines).await?))
}

#[derive(Deserialize)]
struct ShellTextIn {
    text: String,
    /// Defaults to true: typing a line and *not* running it is the unusual case.
    enter: Option<bool>,
}

/// `POST /api/hosts/:name/shells/:pane_id/text` — type a line into the shell.
async fn host_shell_text(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Json(b): Json<ShellTextIn>,
) -> Result<Response, LcError> {
    shell::send_text(&app, &name, &pane_id, &b.text, b.enter.unwrap_or(true)).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

/// Deliberately not `KeysIn`: a shell has no run, so there is no `expect_run_id` to honour
/// and accepting one would only look as though it did something.
#[derive(Deserialize)]
struct ShellKeysIn {
    keys: Vec<String>,
}

/// `POST /api/hosts/:name/shells/:pane_id/keys` — ctrl+c / esc / arrows.
async fn host_shell_keys(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Json(b): Json<ShellKeysIn>,
) -> Result<Response, LcError> {
    shell::send_keys(&app, &name, &pane_id, &b.keys).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

/// `DELETE /api/hosts/:name/shells/:pane_id` — close it (idempotent).
async fn close_host_shell(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
) -> Result<Response, LcError> {
    shell::close(&app, &name, &pane_id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

#[derive(Deserialize)]
struct ModelsQuery {
    kind: String,
    host: Option<String>,
    refresh: Option<String>,
    /// claude only: whose `settings.json` the "預設" effort hint is read from. Not required to
    /// exist — an unknown or wrong-kind name just falls back to the default account (SPEC §17.1).
    identity: Option<String>,
}

fn flag(v: &Option<String>) -> bool {
    matches!(v.as_deref().map(str::trim), Some("1") | Some("true") | Some("yes"))
}

/// `GET /api/models?kind=&host=&identity=&refresh=1`
async fn get_models(State(app): State<Arc<App>>, Query(q): Query<ModelsQuery>) -> Result<Json<Value>, LcError> {
    if !crate::config::valid_kind(&q.kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::kinds_list())));
    }
    let host = q.host.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    if app.hosts.get(&host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let identity = q.identity.as_deref().filter(|s| !s.trim().is_empty());
    let v = crate::models::list(&app, &host, &q.kind, identity, flag(&q.refresh)).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok(Json(v))
}

/// `GET /api/quota?refresh=1&host=`
///
/// `host` narrows a refresh to one host (default: `local` plus every connected remote one).
/// The body is always the full map — one entry per host + kind (SPEC §14).
#[cfg(test)]
mod project_tests {
    use super::*;

    /// `PATCH /api/projects/:id {label}` renames in place and never asks for a restart:
    /// the label only feeds the `agent_name` slug of the next start.
    #[tokio::test]
    async fn patch_renames_the_project_and_rejects_a_blank_label() {
        let e = crate::team::testing::env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        // `testing::env` only seeds the db row; the rename edits config.toml, so register it.
        app.cfg
            .update(|cfg| {
                cfg.projects.push(crate::config::ProjectCfg {
                    id: Some(pid.clone()),
                    path: e.repo.to_string_lossy().to_string(),
                    label: "proj".into(),
                    host: "local".into(),
                    bots: vec![],
                });
                Ok(())
            })
            .await
            .unwrap();

        let res = patch_project(
            State(app.clone()),
            Path(pid.clone()),
            Json(PatchProject { label: Some("  改過的名字  ".into()) }),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let p = db::project(&app.db, &pid).await.unwrap().unwrap();
        assert_eq!(p.label, "改過的名字", "the label is trimmed and projected into the db");
        assert!(app.cfg.get().await.projects.iter().any(|x| x.label == "改過的名字"), "and written to config.toml");

        // Blank is a 400, and the old label survives.
        let err = patch_project(State(app.clone()), Path(pid.clone()), Json(PatchProject { label: Some("   ".into()) }))
            .await
            .unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "blank label is a 400, got {err:?}");
        assert_eq!(db::project(&app.db, &pid).await.unwrap().unwrap().label, "改過的名字");

        // An unknown project is a 404.
        let err = patch_project(State(app), Path("nope".into()), Json(PatchProject { label: Some("x".into()) }))
            .await
            .unwrap_err();
        assert!(matches!(err, LcError::NotFound(_)), "unknown project is a 404, got {err:?}");
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn like_wildcards_typed_by_the_user_are_literal() {
        assert_eq!(like_escape("100%"), r"100\%");
        assert_eq!(like_escape("a_b"), r"a\_b");
        assert_eq!(like_escape(r"c:\path"), r"c:\\path");
        assert_eq!(like_escape("plain"), "plain");
    }

    #[test]
    fn the_snippet_is_taken_around_the_hit_not_from_the_start() {
        let long = format!("{}命中在很後面{}", "前".repeat(80), "後".repeat(80));
        let s = snippet_around(&long, "命中", 30);
        assert!(s.contains("命中"), "the hit itself must be in the window: {s}");
        assert!(s.starts_with('…'), "an elided head is marked: {s}");
        assert!(s.chars().count() <= 34, "and the window stays small: {s}");
    }

    #[test]
    fn a_hit_at_the_start_needs_no_leading_ellipsis() {
        let s = snippet_around("資料夾選擇介面的問題", "資料夾", 90);
        assert_eq!(s, "資料夾選擇介面的問題");
    }

    /// `İ.to_lowercase()` is two chars, so the lowercased copy is *longer* than the original:
    /// a hit position measured in it used to index past the end of the original and panic
    /// (`/search/messages` answered 500 for any message holding one).
    #[test]
    fn a_hit_after_a_char_that_grows_when_lowercased_stays_in_bounds() {
        let content = format!("{}命中", "İ".repeat(40));
        let s = snippet_around(&content, "命中", 90);
        assert!(s.contains("命中"), "the hit is in the window: {s}");
        let s = snippet_around(&content, "i\u{307}", 90);
        assert!(s.starts_with('İ'), "and a lowercase needle still matches the original: {s}");
    }
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<i64>,
}

/// `%` and `_` are LIKE wildcards and `\` is the escape we declare: a user typing any of
/// them means the character, not the pattern.
fn like_escape(q: &str) -> String {
    q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Where `needle_lower` (already lowercase) first appears in `chars`, compared
/// case-insensitively — as an index into `chars` itself.
///
/// Not `content.to_lowercase().find(..)`: lowercasing is not one char per char. `İ` becomes
/// two (`i` + a combining dot), so an index taken from the lowercased copy can point past the
/// end of the original — and slicing `chars` with it panicked the whole `/search/messages`
/// into a 500. Comparing char by char keeps every index in the original's coordinates.
fn find_ci(chars: &[char], needle_lower: &str) -> Option<usize> {
    (0..=chars.len()).find(|&at| starts_with_ci(&chars[at..], needle_lower))
}

/// Does `hay` start with `needle_lower`, ignoring case? Each haystack char is expanded by
/// `char::to_lowercase` (one char can yield several) and matched against the needle in order.
fn starts_with_ci(hay: &[char], needle_lower: &str) -> bool {
    let mut lows = hay.iter().flat_map(|c| c.to_lowercase());
    needle_lower.chars().all(|w| lows.next() == Some(w))
}

/// A window around the first hit, so the caller sees *why* the message matched rather than
/// its first 80 characters. Character-based, not byte-based: the content is mostly CJK.
fn snippet_around(content: &str, needle_lower: &str, width: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    let at = find_ci(&chars, needle_lower).unwrap_or(0);
    let start = at.saturating_sub(width / 3);
    let end = (start + width).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `GET /api/search/messages?q=` — which bots have said (or been told) this.
///
/// Plain `LIKE`, no FTS: the table is small (hundreds to low thousands of rows) and a scan
/// measures at ~20ms, so an index and its migration would cost more than they save. Revisit
/// if `messages` ever grows by an order of magnitude.
async fn search_messages(State(app): State<Arc<App>>, Query(q): Query<SearchQuery>) -> Result<Json<Value>, LcError> {
    let needle = q.q.unwrap_or_default();
    let needle = needle.trim();
    if needle.is_empty() {
        return Ok(Json(json!({"q": "", "bots": []})));
    }
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let pattern = format!("%{}%", like_escape(needle));
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        r#"SELECT c.bot_id, COUNT(*) AS hits,
                  (SELECT m2.content FROM messages m2
                     JOIN conversations c2 ON c2.id = m2.conversation_id
                    WHERE c2.bot_id = c.bot_id AND m2.content LIKE ?1 ESCAPE '\'
                    ORDER BY m2.created_at DESC LIMIT 1) AS newest
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
            WHERE m.content LIKE ?1 ESCAPE '\'
            GROUP BY c.bot_id
            ORDER BY hits DESC
            LIMIT ?2"#,
    )
    .bind(&pattern)
    .bind(limit)
    .fetch_all(&app.db)
    .await
    .map_err(any_err)?;

    let lower = needle.to_lowercase();
    let bots: Vec<Value> = rows
        .into_iter()
        .map(|(bot_id, hits, newest)| json!({"bot_id": bot_id, "hits": hits, "snippet": snippet_around(&newest, &lower, 90)}))
        .collect();
    Ok(Json(json!({"q": needle, "bots": bots})))
}

/// Live RSS of every herdr process tree we can reach (SPEC §15). The poller pushes
/// `mem_updated` when it moves; this is here for the first paint and for anyone polling.
async fn get_mem(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(serde_json::to_value(crate::memstat::sample(&app).await).map_err(any_err)?))
}

/// SPEC §15.4: what that RAM number is made of on one host, so the user can see which
/// processes are theirs to reclaim and which belong to a bot.
async fn get_mem_processes(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, LcError> {
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    if app.hosts.get(&host).await.is_none() {
        return Err(LcError::NotFound(format!("unknown host `{host}`")));
    }
    crate::memproc::processes(&app, &host).await.map(Json).map_err(|e| LcError::Upstream(format!("{e:#}")))
}

/// `GET /api/mem/processes/pane?host=&pane_id=&socket=&lines=` — the visible text of one pane in the
/// RAM list, so "自己開的 pane wM:pB" can be told apart from the other nine.
async fn get_mem_pane(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, LcError> {
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let Some(pane_id) = q.get("pane_id").filter(|p| !p.is_empty()) else {
        return Err(LcError::Bad("pane_id required".into()));
    };
    let lines: u32 = q.get("lines").and_then(|s| s.parse().ok()).unwrap_or(40).clamp(1, 500);
    if app.hosts.get(&host).await.is_none() {
        return Err(LcError::NotFound(format!("unknown host `{host}`")));
    }
    let socket = q.get("socket").map(String::as_str);
    Ok(Json(crate::memproc::pane_preview(&app, &host, pane_id, socket, lines).await?))
}

/// SPEC §15.4: signal one process inside a herdr tree. The guard rails (must be in the tree,
/// never herdr, never a bot) live in `memproc::kill`, which re-samples first.
async fn kill_mem_process(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Result<Json<Value>, LcError> {
    let host = body.get("host").and_then(|v| v.as_str()).unwrap_or(crate::config::LOCAL_HOST).to_string();
    let Some(pid) = body.get("pid").and_then(|v| v.as_i64()) else {
        return Err(LcError::Bad("pid required".into()));
    };
    let signal = body.get("signal").and_then(|v| v.as_str()).unwrap_or("TERM");
    if app.hosts.get(&host).await.is_none() {
        return Err(LcError::NotFound(format!("unknown host `{host}`")));
    }
    match crate::memproc::kill(&app, &host, pid as i32, signal).await {
        Err(e) => Err(LcError::Upstream(format!("{e:#}"))),
        Ok(Ok(v)) => Ok(Json(v)),
        Ok(Err(crate::memproc::KillDenied::NotInTree)) => Err(LcError::Bad(format!("pid {pid} 不在 {host} 的 herdr 樹裡"))),
        Ok(Err(crate::memproc::KillDenied::Herdr)) => Err(LcError::Bad("不能砍 herdr 本身".into())),
        Ok(Err(crate::memproc::KillDenied::Bot(id))) => {
            Err(LcError::conflict("bot_process", json!({"bot_id": id, "message": "這是 AG Man 的 bot，請用停止 bot"})))
        }
    }
}

async fn get_quota(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, LcError> {
    if flag(&q.get("refresh").cloned()) {
        let hosts = match q.get("host").map(String::as_str).filter(|h| !h.is_empty()) {
            Some(h) => {
                if app.hosts.get(h).await.is_none() {
                    return Err(LcError::NotFound(format!("unknown host `{h}`")));
                }
                vec![h.to_string()]
            }
            None => crate::quota::pollable_hosts(&app).await,
        };
        for host in hosts {
            if let Err(e) = crate::quota::refresh_codex(&app, &host).await {
                tracing::warn!(host = %host, error = %e, "codex quota refresh failed");
            }
            if let Err(e) = crate::quota_claude::refresh_claude(&app, &host).await {
                tracing::warn!(host = %host, error = %e, "claude quota refresh failed");
            }
            if let Err(e) = crate::quota_grok::refresh_grok(&app, &host).await {
                tracing::warn!(host = %host, error = %e, "grok quota refresh failed");
            }
        }
    }
    Ok(Json(crate::quota::snapshot(&app).await))
}


// ---------------------------------------------------------------- identities

#[derive(Deserialize)]
struct NewIdentity {
    name: String,
    kind: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    args: Vec<String>,
}

async fn create_identity(State(app): State<Arc<App>>, Json(b): Json<NewIdentity>) -> Result<Response, LcError> {
    if !valid_identity_name(&b.name) {
        return Err(LcError::Bad(format!("identity name must match {}", crate::config::SLUG_NAME_RE)));
    }
    if !crate::config::valid_kind(&b.kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::kinds_list())));
    }
    let cfg = IdentityCfg { name: b.name.clone(), kind: b.kind.clone(), env: b.env.clone(), args: b.args.clone() };
    let res = app
        .cfg
        .update(move |f| {
            if f.identities.iter().any(|i| i.name == cfg.name) {
                anyhow::bail!("duplicate");
            }
            f.identities.push(cfg);
            Ok(())
        })
        .await;
    match res {
        Ok(()) => {}
        Err(e) if e.to_string() == "duplicate" => {
            return Err(LcError::conflict("identity name already in use", json!({"name": b.name})))
        }
        Err(e) => return Err(any_err(e)),
    }
    reproject(&app).await?;
    app.emit("identities_changed", json!({})).await;
    // A new identity has no login answer on any host yet; ask each one in the background.
    for c in app.hosts.list().await {
        crate::tools::spawn_detect(app.clone(), c.name.clone());
    }
    Ok((StatusCode::OK, Json(json!({"name": b.name}))).into_response())
}

async fn delete_identity(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    for b in db::live_bots(&app.db).await.map_err(any_err)? {
        if b.identity.as_deref() == Some(name.as_str()) {
            return Err(LcError::conflict("identity still used by bots", json!({"bot_id": b.id})));
        }
    }
    let n2 = name.clone();
    app.cfg
        .update(move |f| {
            f.identities.retain(|i| i.name != n2);
            Ok(())
        })
        .await
        .map_err(any_err)?;
    // Drop the stale per-host login rows rather than leaving a deleted identity on the strip.
    // A `ccN` alias of the same name is a *different* entry (SPEC §16) and survives: it is put
    // back from that host's `shell_identities`, which the config never owned.
    for ht in app.tools.lock().await.values_mut() {
        ht.identities.remove(&name);
        if let Some(i) = ht.shell_identities.iter().find(|i| i.name == name) {
            let dir = i.env.get("CLAUDE_CONFIG_DIR").cloned();
            ht.identities.insert(name.clone(), crate::tools::IdentityInfo::shell(&i.name, &i.kind, dir));
        }
    }
    reproject(&app).await?;
    app.emit("identities_changed", json!({})).await;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

// ---------------------------------------------------------------- run control

async fn start_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let run_id = lifecycle::start_bot(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({"run_id": run_id}))).into_response())
}

/// SPEC §6.9 — 批次：挑出「帶著 claude 更新且閒置」的 bot，背景一顆一顆 exit + resume。
///
/// 立刻回計畫（誰要重啟、誰被跳過與原因），進度與摘要走 WS。一顆 `stop_bot` 最久等十秒，
/// 五顆就一分鐘——同步做完再回會把 HTTP 連線拖死。
async fn restart_idle_bots(State(app): State<Arc<App>>) -> Result<Response, LcError> {
    let plan = crate::bulk_restart::spawn(&app).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok((StatusCode::ACCEPTED, Json(plan)).into_response())
}

async fn restart_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let run_id = lifecycle::restart_bot(&app, &id).await?;
    app.emit("bot_changed", json!({"bot_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"run_id": run_id}))).into_response())
}

async fn stop_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let had = lifecycle::stop_bot(&app, &id).await?;
    if had {
        Ok((StatusCode::OK, Json(json!({}))).into_response())
    } else {
        Ok(StatusCode::NO_CONTENT.into_response())
    }
}

async fn interrupt_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    lifecycle::interrupt_bot(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

/// `POST /api/bots/:id/abort` — 強制結束目前回合（送不送得出 `esc` 都解鎖）。
async fn abort_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let out = lifecycle::abort_turns(&app, &id).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// 對正在跑的 bot 送登入指令：它的 TUI 會切進登入 / 切換帳號流程（claude 與 grok 的
/// `/login`），在使用者完成之前這個 bot 不能工作。這裡只負責把指令送進去——登入完成與否
/// 由 `POST /hosts/:name/tools/refresh` 重新偵測。
///
/// 404 = 沒有這個 bot；400 `login_unsupported` = 這個 kind 的 TUI 沒有登入指令（codex）；
/// 409 = 現在送不出去（`not_running` / `agent_busy` / `turn_in_flight` / `no_pane`）；
/// 502 = herdr 拒絕。
async fn login_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let out = lifecycle::login(&app, &id).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// Move a running bot's pane into a tab of its own, so it stops sharing the workspace's
/// width with its neighbours. The retrofit for bots started before one-bot-one-tab; the bot
/// keeps running throughout. 404 when there is no active run or no pane behind it, 502 when
/// herdr refuses; already-solo is a 200 that changes nothing.
async fn move_bot_pane_to_tab(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    lifecycle::move_pane_to_own_tab(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

#[derive(Deserialize)]
struct PromptIn {
    text: String,
    client_request_id: Option<String>,
    /// Attachment ids from `POST /bots/:id/attachments`, in display order.
    #[serde(default)]
    attachments: Vec<String>,
}

async fn prompt_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<PromptIn>,
) -> Result<Response, LcError> {
    let crid = b.client_request_id.unwrap_or_else(db::ulid);
    let out = lifecycle::prompt_with(&app, &id, &b.text, &crid, &b.attachments).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// `POST /api/bots/:id/attachments?name=<filename>` with the raw image as the body.
///
/// Raw bytes rather than multipart: the composer only ever sends one file per call, and it
/// keeps the daemon free of a form-parsing dependency.
async fn upload_attachment(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, LcError> {
    let name = q.get("name").map(String::as_str).unwrap_or("image").trim();
    let name = if name.is_empty() { "image" } else { name };
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|m| m.split(';').next().unwrap_or(m).trim().to_string())
        .unwrap_or_default();
    if !crate::attach::is_image(&mime) {
        return Err(LcError::Bad(format!("only images can be attached (Content-Type was `{mime}`)")));
    }
    // `{:#}` so the ssh / filesystem cause reaches the UI, not just "copy attachment to …".
    let a = crate::attach::save(&app, &id, name, &mime, &body)
        .await
        .map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok((StatusCode::OK, Json(crate::attach::to_json(&a))).into_response())
}

/// The stored bytes, for the UI's thumbnail (fetched with the token, then blob-URL'd).
async fn get_attachment(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let (mime, data) = crate::attach::read(&app, &id).await.map_err(|e| LcError::NotFound(e.to_string()))?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "private, max-age=31536000".into())],
        data,
    )
        .into_response())
}

#[derive(Deserialize)]
struct KeysIn {
    keys: Vec<String>,
    expect_run_id: Option<String>,
}

async fn keys_bot(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<KeysIn>) -> Result<Response, LcError> {
    lifecycle::send_keys(&app, &id, b.keys, b.expect_run_id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

#[derive(Deserialize)]
struct TextIn {
    text: String,
    /// 打完字要不要按 Enter。預設要——呼叫端要的幾乎都是「送出這句」。
    enter: Option<bool>,
    expect_run_id: Option<String>,
}

/// `POST /api/bots/:id/text` — 把整段文字打進 bot 的 pane（多行照原樣），預設接一個 Enter。
async fn text_bot(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<TextIn>) -> Result<Response, LcError> {
    lifecycle::send_text(&app, &id, &b.text, b.enter.unwrap_or(true), b.expect_run_id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

async fn abandon_turn(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    lifecycle::abandon_turn(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

// ---------------------------------------------------------------- reads

async fn get_messages(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let conv = db::conversation_id(&app.db, &id).await.map_err(any_err)?;
    let limit: i64 = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(100).clamp(1, 500);
    let before = q.get("before").cloned();
    let rows = match &before {
        Some(b) => sqlx::query_as::<_, db::Message>(
            "SELECT * FROM messages WHERE conversation_id=? AND id < ? ORDER BY id DESC LIMIT ?",
        )
        .bind(&conv)
        .bind(b)
        .bind(limit + 1),
        None => sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE conversation_id=? ORDER BY id DESC LIMIT ?")
            .bind(&conv)
            .bind(limit + 1),
    }
    .fetch_all(&app.db)
    .await
    .map_err(any_err)?;
    let has_more = rows.len() as i64 > limit;
    let mut msgs: Vec<db::Message> = rows.into_iter().take(limit as usize).collect();
    msgs.reverse();
    let turns = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? ORDER BY created_at DESC LIMIT ?")
        .bind(&conv)
        .bind(limit + 1)
        .fetch_all(&app.db)
        .await
        .map_err(any_err)?;
    Ok(Json(json!({"bot_id": id, "conversation_id": conv, "messages": msgs, "turns": turns, "has_more": has_more})))
}

/// SPEC §13.4: the project group timeline (every member bot's messages, merged).
async fn get_project_messages(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let limit: i64 = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(100);
    let before = q.get("before").map(|s| s.as_str()).filter(|s| !s.is_empty());
    Ok(Json(crate::group::messages(&app, &id, before, limit).await?))
}

/// SPEC §13.4: `@<bot>` / `@all` fan-out. No valid mention → 400 `{error:"no_mention", bots}`.
async fn project_chat(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<PromptIn>,
) -> Response {
    let crid = b.client_request_id.unwrap_or_else(db::ulid);
    match crate::group::chat(&app, &id, &b.text, &crid, &b.attachments).await {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(crate::group::Response400::Lc(e)) => e.into_response(),
        Err(crate::group::Response400::NoMention(bots)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "no_mention",
                "message": "text must mention @all or at least one bot of this project",
                "bots": bots,
            })),
        )
            .into_response(),
    }
}

async fn get_terminal(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let run = db::active_run(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let pane = run.pane_id.clone().ok_or_else(|| LcError::NotFound("pane".into()))?;
    let source = q.get("source").cloned().unwrap_or_else(|| "visible".into());
    if !["visible", "recent", "recent_unwrapped", "detection"].contains(&source.as_str()) {
        return Err(LcError::Bad("bad source".into()));
    }
    let lines: u32 = q.get("lines").and_then(|s| s.parse().ok()).unwrap_or(200).clamp(1, 2000);
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| LcError::Upstream(format!("no Herdr session is available for run `{}`", run.id)))?;
    let read = client.pane_read(&pane, &source, lines).await.map_err(any_err)?;
    // Pane geometry, so the UI can explain an unreadable snapshot instead of just showing it:
    // below roughly 60 columns a TUI agent lays its own text out one fragment per row and the
    // spaces fall off the ends, which no amount of parsing recovers (observed on `w8:pK` at 31).
    // Best effort — a snapshot is still worth returning without it.
    let (columns, rows) = match client.pane_size(&pane).await {
        Ok(Some((w, h))) => (Some(w), Some(h)),
        _ => (None, None),
    };
    Ok(Json(json!({
        "bot_id": id, "run_id": run.id, "pane_id": pane,
        "source": read.source, "text": read.text, "revision": read.revision, "truncated": read.truncated,
        "agent_status": run.agent_status,
        "columns": columns, "rows": rows,
    })))
}

// ---------------------------------------------------------------- websocket

async fn ws_handler(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_is_local(&headers, app.port, app.allow_lan) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    if q.get("token").map(|s| s.as_str()) != Some(app.ui_token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let since: Option<u64> = q.get("since").and_then(|s| s.parse().ok());
    ws.on_upgrade(move |socket| ws_loop(app, socket, since))
}

async fn ws_loop(app: Arc<App>, mut socket: WebSocket, since: Option<u64>) {
    let mut rx = app.subscribe();
    // Replay or ask for a resync before streaming live events.
    let backlog = match since {
        Some(s) => app.backlog(s).await,
        None => Some(vec![]),
    };
    match backlog {
        Some(evs) => {
            for e in evs {
                if socket.send(WsMessage::Text(serde_json::to_string(&e).unwrap().into())).await.is_err() {
                    return;
                }
            }
        }
        None => {
            let _ = socket
                .send(WsMessage::Text(json!({"type": "resync", "seq": app.current_seq()}).to_string().into()))
                .await;
        }
    }
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(e) => {
                    if socket.send(WsMessage::Text(serde_json::to_string(&e).unwrap().into())).await.is_err() { return; }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = socket.send(WsMessage::Text(json!({"type":"resync"}).to_string().into())).await;
                }
                Err(_) => return,
            },
            inbound = socket.recv() => match inbound {
                Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Text(_))) | Some(Ok(WsMessage::Binary(_))) => {}
                Some(Ok(WsMessage::Pong(_))) => {}
                _ => return,
            },
        }
    }
}

#[cfg(test)]
mod order_tests {
    use super::reorder_by;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn puts_the_named_ones_in_the_given_order() {
        let mut items = ids(&["a", "b", "c"]);
        reorder_by(&mut items, &ids(&["c", "a", "b"]), |x| Some(x.clone()));
        assert_eq!(items, ids(&["c", "a", "b"]));
    }

    /// 別的 client 剛新增、這個請求還不知道的項目：維持原相對順序接在後面，不被丟掉。
    #[test]
    fn unnamed_items_keep_their_relative_order_at_the_end() {
        let mut items = ids(&["a", "new1", "b", "new2"]);
        reorder_by(&mut items, &ids(&["b", "a"]), |x| Some(x.clone()));
        assert_eq!(items, ids(&["b", "a", "new1", "new2"]));
    }

    /// 沒有 id 的（手寫 config 還沒補 id）也一樣不能消失。
    #[test]
    fn items_without_an_id_survive() {
        let mut items = vec![Some("a".to_string()), None, Some("b".to_string())];
        reorder_by(&mut items, &ids(&["b"]), |x| x.clone());
        assert_eq!(items, vec![Some("b".to_string()), Some("a".to_string()), None]);
    }
}

#[cfg(test)]
mod name_tests {
    use super::next_free_name;

    #[test]
    fn next_free_name_bumps_the_suffix() {
        let taken = |n: &str| ["cc1-1", "cc1-2", "review"].contains(&n);
        assert_eq!(next_free_name("cc1-1", &taken), "cc1-3");
        assert_eq!(next_free_name("cc1-9", &taken), "cc1-3");
        assert_eq!(next_free_name("review", &taken), "review-1");
        let long = "a".repeat(32);
        assert!(next_free_name(&long, &taken).len() <= 32);
    }
}
