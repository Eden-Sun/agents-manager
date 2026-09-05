//! REST + WebSocket API (SPEC §7).

use crate::config::{
    canonical_path, valid_bot_name, valid_host_name, valid_identity_name, HostCfg, IdentityCfg, LOCAL_HOST,
};
use std::collections::BTreeMap;
use crate::db;
use crate::lifecycle::{self, LcError};
use crate::state::App;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

impl IntoResponse for LcError {
    fn into_response(self) -> Response {
        match self {
            LcError::NotFound(what) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found", "what": what}))).into_response(),
            LcError::Conflict(v) => (StatusCode::CONFLICT, Json(v)).into_response(),
            LcError::Bad(m) => (StatusCode::BAD_REQUEST, Json(json!({"error": "bad_request", "message": m}))).into_response(),
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
        .route("/projects/{id}", delete(delete_project))
        .route("/projects/{id}/bots", post(create_bot))
        .route("/projects/{id}/messages", get(get_project_messages))
        .route("/projects/{id}/chat", post(project_chat))
        .route("/bots/{id}", patch(patch_bot).delete(delete_bot))
        .route("/bots/{id}/start", post(start_bot))
        .route("/bots/{id}/restart", post(restart_bot))
        .route("/bots/{id}/stop", post(stop_bot))
        .route("/bots/{id}/interrupt", post(interrupt_bot))
        .route("/bots/{id}/prompt", post(prompt_bot))
        .route("/bots/{id}/keys", post(keys_bot))
        .route("/bots/{id}/messages", get(get_messages))
        .route("/bots/{id}/terminal", get(get_terminal))
        .route("/turns/{id}/abandon", post(abandon_turn))
        .route("/hosts", post(create_host))
        .route("/hosts/{name}", delete(delete_host))
        .route("/hosts/{name}/reconnect", post(reconnect_host))
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

fn host_is_local(headers: &HeaderMap, port: u16) -> bool {
    let Some(h) = headers.get("host").and_then(|v| v.to_str().ok()) else { return false };
    let expected = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
    expected.iter().any(|e| e == h)
}

/// A5: parse the Origin and compare the **host** exactly. `starts_with` used to let
/// `http://localhost.attacker.com` through, and `null` (file://) is not a supported caller.
///
/// The port is deliberately *not* pinned to `listen`: the dev UI is served by Vite on another
/// local port and its proxy forwards the browser's Origin verbatim (`web/vite.config.ts` only
/// rewrites `Host`), so pinning it would reject every dev-server request. Cross-origin reads
/// still need the UI token.
fn origin_is_local(headers: &HeaderMap, _port: u16) -> bool {
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
    if !origin_is_local(&headers, app.port) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "bad origin"}))).into_response();
    }
    let tok = headers.get("X-AM-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if tok != app.ui_token {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or bad X-AM-Token"}))).into_response();
    }
    next.run(req).await
}

async fn get_session(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !host_is_local(&headers, app.port) || !origin_is_local(&headers, app.port) {
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
    for c in app.hosts.list().await {
        let connected = if c.is_local() { app.connected.load(Ordering::SeqCst) } else { c.is_connected() };
        out.push(json!({
            "name": c.name,
            "ssh": c.cfg.as_ref().map(|x| x.ssh.clone()),
            "ssh_port": c.cfg.as_ref().map(|x| x.ssh_port),
            "ssh_opts": c.cfg.as_ref().map(|x| x.ssh_opts.clone()).unwrap_or_default(),
            "herdr_session": c.cfg.as_ref().map(|x| x.herdr_session.clone()).unwrap_or_else(|| app.herdr_session.clone()),
            "remote_path": c.cfg.as_ref().map(|x| x.remote_path.clone()),
            "hook_port": c.cfg.as_ref().and_then(|x| x.hook_port),
            "connected": connected,
            "error": c.error_string().await,
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
        // SPEC §11.6: a bot on a disconnected host lamps `disconnected`.
        let host_up = app.host_connected(&p.host).await;
        let mut bl = Vec::new();
        for b in bots.iter().filter(|b| b.project_id == p.id) {
            let run = db::active_run(&app.db, &b.id).await.map_err(any_err)?;
            bl.push(json!({
                "id": b.id,
                "project_id": b.project_id,
                "name": b.name,
                "kind": b.kind,
                "model": b.model,
                "args": b.args(),
                "autostart": b.autostart == 1,
                "inject_hooks": b.inject_hooks == 1,
                "auto_approve": b.auto_approve == 1,
                "identity": b.identity,
                "env": b.env(),
                // herdr agent name: the live run's, else what the next start will use.
                "agent_name": run.as_ref().and_then(|r| r.agent_name.clone()).unwrap_or_else(|| crate::config::agent_name(&p.label, &b.name)),
                "run": run,
                "lamp": lamp(host_up, run.as_ref()),
                "unread": 0,
            }));
        }
        out.push(json!({
            "id": p.id, "path": p.path, "label": p.label, "host": p.host,
            "workspace_id": p.workspace_id, "bots": bl
        }));
    }
    Ok(json!({
        "daemon_seq": app.current_seq(),
        "connected": connected,
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
}

/// Directory browser for the "new project" picker. Lists only directories (no files),
/// hides dot-entries, never follows into unreadable places, and reports the parent.
async fn list_dirs(State(app): State<Arc<App>>, Query(q): Query<DirsQuery>) -> Result<Json<Value>, LcError> {
    // SPEC §11.5: the same JSON, produced by a remote `sh` snippet.
    let host = q.host.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    if host != LOCAL_HOST {
        let conn = app.hosts.get(&host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
        let v = crate::hosts::remote_list_dirs(&conn, q.path.as_deref())
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
                if name.starts_with('.') {
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
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"project_id": id}))).into_response())
}

async fn delete_project(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let bots = db::live_bots(&app.db).await.map_err(any_err)?;
    for b in bots.iter().filter(|b| b.project_id == id) {
        if db::active_run(&app.db, &b.id).await.map_err(any_err)?.is_some() {
            return Err(LcError::conflict("all bots must be stopped first", json!({"bot_id": b.id})));
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
    kind: String,
    /// claude `--model <m>` / codex `-m <m>`; omitted / null / "" = the CLI's own default.
    #[serde(default)]
    model: Option<String>,
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

/// `None` = no identity requested; `Some(name)` = must exist and match `kind`.
async fn check_identity(app: &Arc<App>, identity: &Option<String>, kind: &str) -> Result<Option<String>, LcError> {
    let Some(name) = identity.clone().filter(|s| !s.trim().is_empty()) else { return Ok(None) };
    let cfg = app.cfg.get().await;
    let Some(id) = cfg.identities.iter().find(|i| i.name == name) else {
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
        return Err(LcError::Bad(format!("bot name must match {}", crate::config::BOT_NAME_RE)));
    }
    if b.kind != "claude" && b.kind != "codex" {
        return Err(LcError::Bad("kind must be claude or codex".into()));
    }
    let identity = check_identity(&app, &b.identity, &b.kind).await?;
    let env: BTreeMap<String, String> = b.env.clone().unwrap_or_default();
    let id = db::ulid();
    let res = app
        .cfg
        .update(|cfg| {
            let p = cfg
                .projects
                .iter_mut()
                .find(|p| p.id.as_deref() == Some(pid.as_str()))
                .ok_or_else(|| anyhow::anyhow!("no-project"))?;
            // Bot names are unique per project (the herdr agent name is `<project>-<bot>`).
            if p.bots.iter().any(|x| x.name == b.name) {
                anyhow::bail!("duplicate-name");
            }
            p.bots.push(crate::config::BotCfg {
                id: Some(id.clone()),
                name: b.name.clone(),
                kind: b.kind.clone(),
                model: b.model.clone().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()),
                args: b.args.clone(),
                autostart: b.autostart,
                inject_hooks: b.inject_hooks.unwrap_or(true),
                auto_approve: b.auto_approve.unwrap_or(true),
                identity: identity.clone(),
                env: env.clone(),
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
    Ok((StatusCode::OK, Json(json!({"bot_id": id}))).into_response())
}

#[derive(Deserialize)]
struct PatchBot {
    /// `Some(Some(m))` sets, `Some(None)` / `Some("")` clears, absent = unchanged.
    #[serde(default, deserialize_with = "double_option")]
    model: Option<Option<String>>,
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
            return Err(LcError::Bad(format!("bot name must match {}", crate::config::BOT_NAME_RE)));
        }
        // herdr binds the agent name at start time, so renaming needs the bot stopped.
        if let Some(run) = &active {
            return Err(LcError::conflict(
                "cannot rename a bot with an active run",
                json!({"bot_id": id, "run_id": run.id}),
            ));
        }
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
        || b.args.is_some()
        || b.identity.is_some()
        || b.env.is_some()
        || b.auto_approve.is_some()
        || b.inject_hooks.is_some();
    let needs_restart = active.is_some() && restart_relevant;
    if let Some(Some(name)) = &b.identity {
        if !name.trim().is_empty() {
            let kind = db::bot(&app.db, &id)
                .await
                .map_err(any_err)?
                .map(|x| x.kind)
                .ok_or_else(|| LcError::NotFound("bot".into()))?;
            check_identity(&app, &Some(name.clone()), &kind).await?;
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
    Ok((StatusCode::OK, Json(json!({"needs_restart": needs_restart}))).into_response())
}

async fn delete_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let bot = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    let host = db::bot_host(&app.db, &id).await.map_err(any_err)?;
    // SPEC §6.4: stop first (ctrl+c x2, pane closed on timeout), then drop the config entry.
    let _ = lifecycle::stop_bot(&app, &id).await;
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
    Ok((StatusCode::OK, Json(json!({}))).into_response())
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
    hook_port: Option<u16>,
}

/// Write the `[[hosts]]` entry (upsert), then connect and report the outcome.
async fn create_host(State(app): State<Arc<App>>, Json(b): Json<NewHost>) -> Result<Response, LcError> {
    if b.name == LOCAL_HOST {
        return Err(LcError::Bad("`local` is reserved for this machine".into()));
    }
    if !valid_host_name(&b.name) {
        return Err(LcError::Bad(format!("host name must match {}", crate::config::BOT_NAME_RE)));
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
        hook_port: b.hook_port,
    };
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
        return Err(LcError::Bad(format!("identity name must match {}", crate::config::BOT_NAME_RE)));
    }
    if b.kind != "claude" && b.kind != "codex" {
        return Err(LcError::Bad("kind must be claude or codex".into()));
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
    reproject(&app).await?;
    app.emit("identities_changed", json!({})).await;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

// ---------------------------------------------------------------- run control

async fn start_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let run_id = lifecycle::start_bot(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({"run_id": run_id}))).into_response())
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

#[derive(Deserialize)]
struct PromptIn {
    text: String,
    client_request_id: Option<String>,
}

async fn prompt_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(b): Json<PromptIn>,
) -> Result<Response, LcError> {
    let crid = b.client_request_id.unwrap_or_else(db::ulid);
    let out = lifecycle::prompt(&app, &id, &b.text, &crid).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
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
    match crate::group::chat(&app, &id, &b.text, &crid).await {
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
    let host = db::bot_host(&app.db, &id).await.map_err(any_err)?;
    let client = app
        .herdr_for(&host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{host}` is not configured")))?;
    let read = client.pane_read(&pane, &source, lines).await.map_err(any_err)?;
    Ok(Json(json!({
        "bot_id": id, "run_id": run.id, "pane_id": pane,
        "source": read.source, "text": read.text, "revision": read.revision, "truncated": read.truncated,
        "agent_status": run.agent_status,
    })))
}

// ---------------------------------------------------------------- websocket

async fn ws_handler(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_is_local(&headers, app.port) {
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
