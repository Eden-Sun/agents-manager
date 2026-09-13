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
        // 群組任務（docs/goals/agm-missions.md）。
        .route(
            "/projects/{id}/missions",
            get(crate::mission::api::get_missions).post(crate::mission::api::post_mission),
        )
        .route("/missions/{id}", get(crate::mission::api::get_mission))
        .route("/missions/{id}/events", post(crate::mission::api::post_event))
        .route("/missions/{id}/pause", post(crate::mission::api::post_pause))
        .route("/missions/{id}/resume", post(crate::mission::api::post_resume))
        // 完成後的追問／回覆／追加修改（AGM 裁示 01M2D18PQZSJ4Z5BJC21TF9Q77）。
        .route("/missions/{id}/question", post(crate::mission::api::post_question))
        .route("/missions/{id}/answer", post(crate::mission::api::post_answer))
        .route("/missions/{id}/revise", post(crate::mission::api::post_revise))
        .route("/missions/{id}/cancel", post(crate::mission::api::post_cancel))
        .route("/missions/{id}/complete", post(crate::mission::api::post_complete))
        .route("/missions/{id}/round", post(crate::mission::api::post_round))
        .route("/missions/{id}/pick", get(crate::mission::api::get_pick))
        .route("/missions/{id}/deliver", post(crate::mission::api::post_deliver))
        .route("/identity-prefs", get(crate::mission::api::get_identity_prefs))
        .route("/identities/{name}/disabled", axum::routing::put(crate::mission::api::put_identity_disabled))
        .route("/projects/{id}/github/refresh", post(refresh_github))
        .route("/projects/{id}/submodules", get(get_submodules))
        .route("/projects/{id}/git", get(get_git))
        .route("/projects/{id}/git/commit", post(git_commit))
        .route("/projects/{id}/git/push", post(git_push))
        .route("/projects/{id}/git/pull", post(git_pull))
        .route("/projects/{id}/issues", get(get_issues))
        .route("/projects/{id}/issues/{number}", get(get_issue))
        .route("/bots/{id}", patch(patch_bot).delete(delete_bot))
        // SPEC §6.9；放在 `{id}` 那組前面，否則 `restart-idle` 會被當成 bot id。
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
        .route("/hosts/{name}/identities/{identity}/login", post(login_identity))
        .route("/hosts/{name}/gh", get(get_gh_status))
        .route("/hosts/{name}/gh/login", post(login_gh))
        .route("/hosts/{name}/gh/cancel", post(cancel_gh))
        .route("/hosts/{name}/shells", get(list_host_shells).post(open_host_shell))
        .route("/hosts/{name}/shells/{pane_id}", delete(close_host_shell))
        .route("/hosts/{name}/shells/{pane_id}/terminal", get(get_host_shell_terminal))
        .route("/hosts/{name}/shells/{pane_id}/text", post(host_shell_text))
        .route("/hosts/{name}/shells/{pane_id}/keys", post(host_shell_keys))
        .route("/models", get(get_models))
        .route("/changelog", get(get_changelog))
        .route("/quota", get(get_quota))
        .route("/mem", get(get_mem))
        .route("/mem/processes", get(get_mem_processes))
        .route("/mem/processes/kill", post(kill_mem_process))
        .route("/mem/processes/pane", get(get_mem_pane))
        .route("/search/messages", get(search_messages))
        // AGM 總管（docs/goals/agm-supervisor-environment-plan-2026-09-09.md）。
        .route("/supervisor", get(crate::supervisor::api::get_supervisor))
        .route("/supervisor/health", get(crate::supervisor::api::get_health))
        .route("/supervisor/setup", post(crate::supervisor::api::post_setup))
        .route("/supervisor/start", post(crate::supervisor::api::post_start))
        .route("/supervisor/stop", post(crate::supervisor::api::post_stop))
        .route("/supervisor/fallback", post(crate::supervisor::api::post_fallback))
        .route(
            "/supervisor/assignments",
            get(crate::supervisor::api::get_assignments).post(crate::supervisor::api::post_assignment),
        )
        .route(
            "/supervisor/handoff",
            get(crate::supervisor::api::get_handoff).put(crate::supervisor::api::put_handoff),
        )
        .route("/supervisor/assignments/{id}", get(crate::supervisor::api::get_assignment))
        // 回合結束只到 awaiting_review；驗收／阻塞／續作／取消都走這支（SPEC §18.3）。
        .route("/supervisor/assignments/{id}/review", post(crate::supervisor::api::post_review))
        .route("/supervisor/incidents", get(crate::supervisor::api::get_incidents))
        // 人設：持久版本是權威，內嵌版只在首次安裝當種子（SPEC §18.11）。
        .route(
            "/supervisor/persona",
            get(crate::supervisor::api::get_persona).put(crate::supervisor::api::put_persona),
        )
        .route("/supervisor/persona/adopt-embedded", post(crate::supervisor::api::post_persona_adopt))
        .route("/supervisor/build-inputs", get(crate::supervisor::api::get_build_inputs))
        // 遠端入口：argv 只算 requested，宣稱通了要有帶 actor 的觀測（SPEC §18.12）。
        .route(
            "/supervisor/remote",
            get(crate::supervisor::api::get_remote).post(crate::supervisor::api::post_remote_observation),
        )
        // 重建／重啟的核准與執行租約（SPEC §18.10）。
        .route(
            "/supervisor/approvals",
            get(crate::supervisor::api::get_approvals).post(crate::supervisor::api::post_approval),
        )
        .route("/supervisor/approvals/{id}/decide", post(crate::supervisor::api::post_approval_decision))
        .route("/supervisor/maintenance/safety", get(crate::supervisor::api::get_maintenance_safety))
        .route("/supervisor/leases", get(crate::supervisor::api::get_leases))
        .route("/supervisor/leases/{resource}/acquire", post(crate::supervisor::api::post_lease_acquire))
        .route("/supervisor/leases/{resource}/renew", post(crate::supervisor::api::post_lease_renew))
        .route("/supervisor/leases/{resource}/release", post(crate::supervisor::api::post_lease_release))
        .route("/supervisor/inbox", get(crate::supervisor::api::get_inbox))
        .route("/supervisor/inbox/{id}/ack", post(crate::supervisor::api::post_inbox_ack))
        .route("/supervisor/state", get(crate::supervisor::api::get_sanitized_state))
        .route("/supervisor/evidence", get(crate::supervisor_evidence::search))
        .merge(crate::supervisor::responder_api::routes())
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
        .route("/relay/announce", post(relay_announce))
        .fallback(get(crate::assets::serve))
        .with_state(app)
}

/// SPEC §6.5d：shim 先報，hook 回音時補 `relay_from`，總管裁示才不像使用者打的。
/// 驗證用該 bot 的 `hook_token`（`X-AM-Bot-Token`）：它只證明「我是那顆 bot」，也只用來標來源。
#[derive(serde::Deserialize)]
struct RelayAnnounce {
    bot_id: String,
    to_agent: String,
    text: String,
}

async fn relay_announce(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    // 表單而非 JSON：POSIX sh shim 用 `--data-urlencode` 對任意內容都安全。
    axum::extract::Form(body): axum::extract::Form<RelayAnnounce>,
) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let ok = match db::bot(&app.db, &body.bot_id).await {
        Ok(Some(b)) if b.deleted_at.is_none() => !token.is_empty() && token == b.hook_token,
        _ => false,
    };
    if !ok {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unknown bot or bad token"})));
    }
    // 寫給 AGM 的：協調者存在時排進它的佇列，shim 看到 `routed` 就不再打進 pane（SPEC §18.15）。
    if let Ok(Some(target)) = crate::supervisor::bot_requests::role_bot_by_agent(&app, &body.to_agent).await {
        match crate::supervisor::bot_requests::intercept(&app, &target, &body.bot_id, &body.text, None, &[], true, "herdr_shim").await {
            Ok(Some(v)) => return (StatusCode::OK, Json(v)),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = ?e, "could not queue a bot request for AGM; falling back to the pane"),
        }
    }
    crate::agent_relay::announce(&body.bot_id, &body.to_agent, &body.text);
    (StatusCode::OK, Json(json!({})))
}

/// TCP peer address, **not** `Host`: the header is caller-chosen, so on 0.0.0.0 anyone on the LAN
/// could fetch the UI token with `curl -H 'Host: localhost:…'`. Needs connect_info (main.rs).
/// `allow_lan` is the explicit dev-only opt-in (off in the packaged app, `main.rs::dev_lan_default`);
/// no range allowlist because "LAN" includes overlays like Tailscale (100.64.0.0/10).
fn peer_is_local(peer: &std::net::SocketAddr, allow_lan: bool) -> bool {
    if allow_lan {
        return true;
    }
    match peer.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|m| m.is_loopback()),
    }
}

/// A5: compare the **host** exactly — `starts_with` let `http://localhost.attacker.com` through.
/// Port not pinned: the Vite dev proxy forwards Origin verbatim; cross-origin reads still need the token.
/// `allow_lan` accepts any Origin, same rationale as `peer_is_local`.
fn origin_is_local(headers: &HeaderMap, _port: u16, allow_lan: bool) -> bool {
    if allow_lan {
        return true;
    }
    let Some(o) = headers.get("origin").and_then(|v| v.to_str().ok()) else { return true };
    let Some(rest) = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")) else { return false };
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

/// SPEC §11.6. `local` always comes first.
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
            "attach_command": crate::config::attach_command(c.cfg.as_ref(), &app.herdr_session),
            "tools": t.map(|x| json!(x.tools)),
            // Login state *on this host*, `[[identities]]` + this host's `ccN` aliases (SPEC §16).
            "identities": t.map(|x| json!(x.identities)),
            // Env unexpanded, as written.
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
                "managed_by": b.managed_by,
                "parent_bot_id": b.parent_bot_id,
                // 使用者釘的主要 bot（純顯示）。
                "primary": b.is_primary == 1,
                "cwd": b.cwd,
                "agent_name": run.as_ref().and_then(|r| r.agent_name.clone()).unwrap_or_else(|| crate::config::agent_name(&p.label, &b.id)),
                "run": run,
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

async fn reproject(app: &Arc<App>) -> Result<(), LcError> {
    crate::projection::project_config(&app.cfg, &app.db).await.map_err(any_err)
}

#[derive(Deserialize)]
struct NewProject {
    path: String,
    label: Option<String>,
    host: Option<String>,
}

#[derive(Deserialize)]
struct DirsQuery {
    path: Option<String>,
    host: Option<String>,
    hidden: Option<String>,
}

async fn list_dirs(State(app): State<Arc<App>>, Query(q): Query<DirsQuery>) -> Result<Json<Value>, LcError> {
    // SPEC §11.5.
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
        // SPEC §11.6.
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
    if let Ok(Some(p)) = db::project(&app.db, &id).await {
        crate::github::detect_project(&app, &p).await;
    }
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"project_id": id}))).into_response())
}

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
    /// Submodule path; absent or empty = the project itself.
    repo: Option<String>,
}

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

#[derive(Deserialize)]
struct PatchProject {
    label: Option<String>,
}

/// Never blocked by a live run: herdr identity derives from the bot id, so `needs_restart: false`.
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
    /// 2026-09-08: quick-add chips compute `<identity>-<n>` from a stale list → second click 409'd.
    /// The daemon picks the next free suffix and returns the name it used.
    #[serde(default)]
    name_auto: bool,
    kind: String,
    /// omitted / null / "" = the CLI's own default.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    fast: bool,
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

/// Must exist **on that bot's host** (`[[identities]]` + its `ccN` aliases, SPEC §16) and match `kind`.
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
    // `cc1` is a different account on each host.
    let host = db::project(&app.db, &pid)
        .await
        .map_err(any_err)?
        .map(|p| p.host)
        .unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let identity = check_identity(&app, &host, &b.identity, &b.kind).await?;
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

/// `cc1-1` → `cc1-2`; `review` → `review-2`. Trims the base to stay within 32 chars.
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

/// 側欄排序寫回 config.toml 陣列順序，全裝置一致（使用者 2026-09-09 回報手機桌機不同）。
#[derive(Deserialize)]
struct SetOrder {
    #[serde(default)]
    projects: Option<Vec<String>>,
    /// config.toml 沒有的（child bot）忽略。
    #[serde(default)]
    bots: Option<BTreeMap<String, Vec<String>>>,
}

/// 沒被點名的（別的 client 剛新增的）維持原相對順序接在後面。
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
    fast: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    persona: Option<Option<String>>,
    args: Option<Vec<String>>,
    autostart: Option<bool>,
    name: Option<String>,
    inject_hooks: Option<bool>,
    auto_approve: Option<bool>,
    /// 使用者釘的主要 bot：純顯示，不進 config.toml（child bot 也能釘）、不需重啟。
    #[serde(rename = "primary")]
    is_primary: Option<bool>,
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

/// For bots with no config.toml entry (`managed_by` = `child`): written straight to `bots`.
async fn patch_unprojected_bot(app: &Arc<App>, id: &str, b: &PatchBot, effort: &Option<Option<String>>) -> Result<(), LcError> {
    let mut sets: Vec<String> = Vec::new();
    let mut vals: Vec<Option<String>> = Vec::new();
    let mut push = |col: &str, v: Option<String>| {
        sets.push(format!("{col} = ?"));
        vals.push(v);
    };
    if let Some(idn) = &b.identity {
        push("identity", idn.clone().filter(|s| !s.trim().is_empty()));
    }
    if let Some(e) = &b.env {
        push("env_json", Some(serde_json::to_string(e).unwrap_or_else(|_| "{}".into())));
    }
    if let Some(m) = &b.model {
        push("model", m.clone().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()));
    }
    if let Some(e) = effort {
        push("effort", e.clone());
    }
    if let Some(f) = b.fast {
        push("fast", Some((f as i64).to_string()));
    }
    if let Some(p) = &b.persona {
        push("persona", p.clone().filter(|x| !x.trim().is_empty()));
    }
    if let Some(a) = &b.args {
        push("args_json", Some(serde_json::to_string(a).unwrap_or_else(|_| "[]".into())));
    }
    if let Some(a) = b.autostart {
        push("autostart", Some((a as i64).to_string()));
    }
    if let Some(n) = &b.name {
        push("name", Some(n.clone()));
    }
    if let Some(h) = b.inject_hooks {
        push("inject_hooks", Some((h as i64).to_string()));
    }
    if let Some(a) = b.auto_approve {
        push("auto_approve", Some((a as i64).to_string()));
    }
    if sets.is_empty() {
        return Ok(());
    }
    let sql = format!("UPDATE bots SET {} WHERE id = ? AND deleted_at IS NULL", sets.join(", "));
    let mut q = sqlx::query(&sql);
    for v in vals {
        q = q.bind(v);
    }
    let n = q.bind(id).execute(&app.db).await.map_err(any_err)?.rows_affected();
    if n == 0 {
        return Err(LcError::NotFound("bot".into()));
    }
    Ok(())
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
    // child bot 不在 config.toml，cfg.update 只會 404（2026-09-09 使用者：child 身分改不了）→ 直接改 DB。
    if let Some(pin) = b.is_primary {
        let n = sqlx::query("UPDATE bots SET is_primary = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(pin as i64)
            .bind(&id)
            .execute(&app.db)
            .await
            .map_err(any_err)?
            .rows_affected();
        if n == 0 {
            return Err(LcError::NotFound("bot".into()));
        }
    }
    let touches_config =
        restart_relevant || b.name.is_some() || b.autostart.is_some();
    let managed_by = db::bot(&app.db, &id).await.map_err(any_err)?.map(|x| x.managed_by).unwrap_or_default();
    if !touches_config {
        app.emit("bot_changed", json!({"bot_id": id})).await;
        return Ok((StatusCode::OK, Json(json!({"needs_restart": false}))).into_response());
    }
    if managed_by != "user" {
        patch_unprojected_bot(&app, &id, &b, &effort).await?;
    } else {
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
    }
    app.emit("bot_changed", json!({"bot_id": id})).await;
    // 只動可用 slash 指令當場套用的欄位就不用重啟；grok `/model <id> <effort>` 可順帶 effort。
    let extras = |skip: &[&str]| {
        let hit = |name: &str, present: bool| present && !skip.contains(&name);
        hit("model", b.model.is_some())
            || hit("effort", b.effort.is_some())
            // `fast` 也要吃 `skip`（SPEC §4.4a），否則只改 fast 永遠回 needs_restart（2026-09-09 實測）。
            || hit("fast", b.fast.is_some())
            || b.persona.is_some()
            || b.args.is_some()
            || b.identity.is_some()
            || b.env.is_some()
            || b.auto_approve.is_some()
            || b.inject_hooks.is_some()
    };
    // codex 可一次收 model / effort / fast 任意組合（SPEC §4.4a）；claude / grok 的 slash 指令一次一個值。
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
    // 失敗要說得出哪一步（2026-09-13 使用者：codex 改 effort 靜默落回重啟）。只在真的試過時才出現。
    let live = if needs_restart && !live_fields.is_empty() {
        Some(lifecycle::apply_live_setting(&app, &id, &live_fields).await)
    } else {
        None
    };
    let needs_restart = match &live {
        Some(reason) => reason.is_some(),
        None => needs_restart,
    };
    let mut out = json!({"needs_restart": needs_restart});
    if let Some(reason) = live {
        out["live_apply"] = json!({
            "fields": live_fields,
            "applied": reason.is_none(),
            "reason": reason,
        });
    }
    Ok((StatusCode::OK, Json(out)).into_response())
}

pub(crate) async fn delete_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let bot = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    let host = db::bot_host(&app.db, &id).await.map_err(any_err)?;
    // 2026-09-08: spawned children go with it, else they're sidebar orphans. Deepest first.
    let mut removed_children = Vec::new();
    for child in descendant_children(&app, &id).await.map_err(any_err)?.into_iter().rev() {
        stop_for_delete(&app, &child.id).await;
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
    // SPEC §6.4: stop first, then drop the config entry.
    stop_for_delete(&app, &id).await;
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
    // Soft delete; the conversation and its messages stay.
    reproject(&app).await?;
    lifecycle::purge_bot_dir(&app, &id, &host).await;
    app.emit("bot_changed", json!({"bot_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"removed_children": removed_children}))).into_response())
}

/// If the host is down the stop fails; end the run anyway, else no reconcile ever ends it and
/// `purge_deleted_bot_dirs` waits forever (review 2026-09-12 d). The orphan-pane sweep reclaims the pane later.
async fn stop_for_delete(app: &Arc<App>, bot_id: &str) {
    if let Err(e) = lifecycle::stop_bot(app, bot_id).await {
        tracing::warn!(bot = %bot_id, error = ?e, "could not stop the bot before deleting it; ending its run");
        if let Ok(Some(run)) = db::active_run(&app.db, bot_id).await {
            lifecycle::mark_run_exited(app, &run.id, "the bot was deleted while its host was unreachable").await;
        }
    }
}

/// Parents before children, so `.rev()` deletes deepest first.
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

/// 刪除是軟的：寫回 config.toml 條目讓 projection 清 `deleted_at`；child bot 直接清欄位。
/// 被 `purge_bot_dir` 砍掉的工作目錄下次啟動會重新產生。
async fn restore_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let bot = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_none() {
        return Err(LcError::conflict("bot is not deleted", json!({"bot_id": id})));
    }
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
    if bot.managed_by != "user" {
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

#[derive(Deserialize)]
struct NewHost {
    name: String,
    ssh: String,
    ssh_port: Option<u16>,
    ssh_opts: Option<Vec<String>>,
    herdr_session: Option<String>,
    remote_path: Option<String>,
}

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
    let changed_hosts = app.hosts.apply_config(&app, &hosts).await;
    let (connected, error) = if changed_hosts.contains(&b.name) {
        match app.hosts.get(&b.name).await {
            Some(conn) => app.hosts.wait_for_connection(conn).await.unwrap_or((false, Some("host vanished".into()))),
            None => (false, Some("host vanished".into())),
        }
    } else {
        app.hosts.reconnect(&app, &b.name).await.unwrap_or((false, Some("host vanished".into())))
    };
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

async fn install_tool(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(b): Json<InstallTool>,
) -> Result<Response, LcError> {
    let out = crate::tools::install_via_bot(&app, &name, &b.kind, &b.via_bot_id).await?;
    Ok((StatusCode::OK, Json(json!({"turn_id": out.turn_id, "message_id": out.message_id, "delivery": out.delivery})))
        .into_response())
}

/// Login output is only visible through the returned shell pane — never in logs, events, or the response.
async fn login_identity(
    State(app): State<Arc<App>>,
    Path((name, identity)): Path<(String, String)>,
) -> Result<Response, LcError> {
    let idn = crate::tools::identity_for_host(&app, &name, &identity)
        .await
        .ok_or_else(|| LcError::NotFound("identity".into()))?;
    let Some(_) = crate::tools::cached_path(&app, &name, &idn.kind).await else {
        return Err(LcError::conflict(
            "identity_login_unavailable",
            json!({"host": name, "identity": identity, "kind": idn.kind, "reason": "CLI 不在 PATH，無法登入"}),
        ));
    };
    let home = crate::tools::host_home(&app, &name).await;
    let env = idn
        .env
        .iter()
        .filter(|(k, _)| crate::tools::valid_env_name(k))
        .map(|(k, v)| (k.clone(), crate::config::expand_home(v, &home)))
        .collect::<BTreeMap<_, _>>();
    let command = crate::tools::identity_login_command(&idn.kind, &env)
        .ok_or_else(|| LcError::Bad(format!("kind {} 沒有登入指令", idn.kind)))?;
    let shell = shell::open(&app, &name, None).await?;
    if let Err(e) = shell::send_text(&app, &name, &shell.pane_id, &command, true).await {
        let _ = shell::close(&app, &name, &shell.pane_id).await;
        return Err(e);
    }
    crate::tools::spawn_identity_login_watch(app, name, shell.pane_id.clone(), identity, idn.kind);
    Ok((StatusCode::OK, Json(json!(shell))).into_response())
}

async fn get_gh_status(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    let v = crate::gh_auth::status(&app, &name).await?;
    Ok((StatusCode::OK, Json(v)).into_response())
}

#[derive(Default, Deserialize)]
struct GhLoginBody {
    mode: Option<String>,
    user: Option<String>,
}

async fn login_gh(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(b): Json<GhLoginBody>,
) -> Result<Response, LcError> {
    let v = crate::gh_auth::login(&app, &name, b.mode.as_deref(), b.user.as_deref()).await?;
    Ok((StatusCode::OK, Json(v)).into_response())
}

async fn cancel_gh(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    let v = crate::gh_auth::cancel(&app, &name).await?;
    Ok((StatusCode::OK, Json(v)).into_response())
}

#[derive(Default, Deserialize)]
struct NewShell {
    cwd: Option<String>,
}

async fn open_host_shell(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    body: Option<Json<NewShell>>,
) -> Result<Response, LcError> {
    let cwd = body.and_then(|Json(b)| b.cwd);
    let s = shell::open(&app, &name, cwd.as_deref()).await?;
    Ok((StatusCode::OK, Json(json!(s))).into_response())
}

async fn list_host_shells(State(app): State<Arc<App>>, Path(name): Path<String>) -> Result<Response, LcError> {
    let shells = shell::list(&app, &name).await?;
    Ok((StatusCode::OK, Json(json!({"host": name, "shells": shells, "max": shell::MAX_PER_HOST}))).into_response())
}

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
    /// Defaults to true.
    enter: Option<bool>,
}

async fn host_shell_text(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Json(b): Json<ShellTextIn>,
) -> Result<Response, LcError> {
    shell::send_text(&app, &name, &pane_id, &b.text, b.enter.unwrap_or(true)).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

/// Not `KeysIn`: a shell has no run, so accepting `expect_run_id` would be a lie.
#[derive(Deserialize)]
struct ShellKeysIn {
    keys: Vec<String>,
}

async fn host_shell_keys(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Json(b): Json<ShellKeysIn>,
) -> Result<Response, LcError> {
    shell::send_keys(&app, &name, &pane_id, &b.keys).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

/// Idempotent.
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
    /// claude only; unknown / wrong-kind names fall back to the default account (SPEC §17.1).
    identity: Option<String>,
}

fn flag(v: &Option<String>) -> bool {
    matches!(v.as_deref().map(str::trim), Some("1") | Some("true") | Some("yes"))
}

#[derive(Deserialize)]
struct ChangelogQuery {
    kind: Option<String>,
    host: Option<String>,
    /// 沒有就只給新版那一段。
    from: Option<String>,
    /// codex 新版還沒進磁碟，要由畫面 `Update available! a -> b` 帶進來；不給就探磁碟。
    to: Option<String>,
}

/// 永遠 200：抓不到時 `found:false` + `error`，UI 照實寫「找不到 changelog」。
async fn get_changelog(State(app): State<Arc<App>>, Query(q): Query<ChangelogQuery>) -> Result<Json<Value>, LcError> {
    let kind = q.kind.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "claude".to_string());
    if !crate::config::valid_kind(&kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::kinds_list())));
    }
    let host = q.host.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    if app.hosts.get(&host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let from = q.from.as_deref().filter(|s| !s.trim().is_empty());
    let to = q.to.as_deref().filter(|s| !s.trim().is_empty());
    let r = crate::changelog::lookup(&app, &host, &kind, from, to).await;
    Ok(Json(serde_json::to_value(r).map_err(any_err)?))
}

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

#[cfg(test)]
mod project_tests {
    use super::*;

    #[tokio::test]
    async fn patch_renames_the_project_and_rejects_a_blank_label() {
        let e = crate::testing::env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        // `testing::env` only seeds the db row; the rename edits config.toml.
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

        let err = patch_project(State(app.clone()), Path(pid.clone()), Json(PatchProject { label: Some("   ".into()) }))
            .await
            .unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "blank label is a 400, got {err:?}");
        assert_eq!(db::project(&app.db, &pid).await.unwrap().unwrap().label, "改過的名字");

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

    /// `İ` lowercases to two chars; an index into the lowercased copy used to panic (500).
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

/// A user typing `%`, `_` or `\` means the character, not the pattern.
fn like_escape(q: &str) -> String {
    q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Not `to_lowercase().find(..)`: `İ` lowercases to two chars, so that index can overrun the
/// original and panicked `/search/messages` into a 500. Char-by-char keeps original coordinates.
fn find_ci(chars: &[char], needle_lower: &str) -> Option<usize> {
    (0..=chars.len()).find(|&at| starts_with_ci(&chars[at..], needle_lower))
}

fn starts_with_ci(hay: &[char], needle_lower: &str) -> bool {
    let mut lows = hay.iter().flat_map(|c| c.to_lowercase());
    needle_lower.chars().all(|w| lows.next() == Some(w))
}

/// Window around the first hit; char-based since content is mostly CJK.
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

/// Plain `LIKE`, no FTS: scan measures ~20ms on a small table; revisit if `messages` grows 10×.
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

/// SPEC §15. The poller pushes `mem_updated`; this is for the first paint.
async fn get_mem(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    Ok(Json(serde_json::to_value(crate::memstat::sample(&app).await).map_err(any_err)?))
}

/// SPEC §15.4.
async fn get_mem_processes(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, LcError> {
    let host = q.get("host").cloned().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    if app.hosts.get(&host).await.is_none() {
        return Err(LcError::NotFound(format!("unknown host `{host}`")));
    }
    crate::memproc::processes(&app, &host).await.map(Json).map_err(|e| LcError::Upstream(format!("{e:#}")))
}

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

/// SPEC §15.4. Guard rails (in the tree, never herdr, never a bot) live in `memproc::kill`, which re-samples first.
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
    // A same-named `ccN` alias is a different entry (SPEC §16) and is put back from `shell_identities`.
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

async fn start_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let run_id = lifecycle::start_bot(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({"run_id": run_id}))).into_response())
}

/// SPEC §6.9。立刻回計畫、進度走 WS：一顆 `stop_bot` 最久十秒，同步做完會拖死 HTTP 連線。
async fn restart_idle_bots(State(app): State<Arc<App>>) -> Result<Response, LcError> {
    let plan = crate::bulk_restart::spawn(&app).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok((StatusCode::ACCEPTED, Json(plan)).into_response())
}

async fn restart_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    // 子 agent 的 pane 是父開的，stop + start 會被拒（SPEC §6.5a），改在原 pane 裡 exit + resume。
    let child = db::bot(&app.db, &id).await.map_err(any_err)?.is_some_and(|b| b.managed_by == "child");
    let run_id =
        if child { lifecycle::restart_child_in_pane(&app, &id).await? } else { lifecycle::restart_bot(&app, &id).await? };
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

/// 送不送得出 `esc` 都解鎖。
async fn abort_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let out = lifecycle::abort_turns(&app, &id).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// 只把 `/login` 送進去；完成與否由 `tools/refresh` 重新偵測。
async fn login_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let out = lifecycle::login(&app, &id).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// Retrofit for bots started before one-bot-one-tab; the bot keeps running.
async fn move_bot_pane_to_tab(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    lifecycle::move_pane_to_own_tab(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

#[derive(Deserialize)]
struct PromptIn {
    text: String,
    client_request_id: Option<String>,
    /// In display order.
    #[serde(default)]
    attachments: Vec<String>,
    /// 另一顆 bot 的 id 或哨符 `daemon`；省略 = 使用者自己打的。
    /// 2026-09-12 使用者：「就連 AGM 自己的 message 也要區分是由 daemon 觸發而非 user」。
    #[serde(default)]
    relay_from: Option<String>,
}

async fn prompt_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<PromptIn>,
) -> Result<Response, LcError> {
    let given_crid = b.client_request_id.clone();
    let crid = b.client_request_id.unwrap_or_else(db::ulid);
    // 只收存在的 bot 或哨符 daemon：隨便填等於讓呼叫端冒名。
    let relay_from = match b.relay_from.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(crate::agent_relay::DAEMON_SENDER) => Some(crate::agent_relay::DAEMON_SENDER.to_string()),
        Some(from) => match db::bot(&app.db, from).await.map_err(any_err)? {
            Some(b) if b.deleted_at.is_none() => Some(b.id),
            _ => return Err(LcError::Bad(format!("relay_from must be a live bot id or `{}`", crate::agent_relay::DAEMON_SENDER))),
        },
    };
    // bot 寫給 AGM 的申請不直接開回合：排進協調者的佇列，回 202（SPEC §18.15）。
    if let Some(from) = relay_from.as_deref().filter(|f| *f != crate::agent_relay::DAEMON_SENDER) {
        let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok());
        let verified = crate::supervisor::bot_requests::sender_verified(&app, token, from).await;
        let queued = crate::supervisor::bot_requests::intercept(&app, &id, from, &b.text, given_crid.as_deref(), &b.attachments, verified, "api");
        if let Some(v) = queued.await? {
            return Ok((StatusCode::ACCEPTED, Json(v)).into_response());
        }
    }
    let out = lifecycle::prompt_relayed(&app, &id, &b.text, &crid, &b.attachments, relay_from.as_deref()).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// Raw bytes, not multipart: one file per call, and no form-parsing dependency.
async fn upload_attachment(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, LcError> {
    if body.is_empty() {
        return Err(LcError::Bad("attachment is empty".into()));
    }
    if body.len() > crate::attach::MAX_BYTES {
        return Err(LcError::Bad(format!(
            "attachment is {} bytes; the limit is {}",
            body.len(),
            crate::attach::MAX_BYTES
        )));
    }
    let bot = db::bot(&app.db, &id)
        .await
        .map_err(any_err)?
        .filter(|bot| bot.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    let project_is_live = db::project(&app.db, &bot.project_id)
        .await
        .map_err(any_err)?
        .is_some_and(|project| project.deleted_at.is_none());
    if !project_is_live {
        return Err(LcError::NotFound("bot".into()));
    }
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
    // `{:#}` so the ssh / filesystem cause reaches the UI.
    let a = crate::attach::save(&app, &id, name, &mime, &body)
        .await
        .map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok((StatusCode::OK, Json(crate::attach::to_json(&a))).into_response())
}

async fn get_attachment(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let (mime, data) = crate::attach::read(&app, &id).await.map_err(|e| {
        tracing::warn!(attachment = %id, error = %e, "attachment read failed");
        LcError::NotFound("attachment".into())
    })?;
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
    /// 預設 true。
    enter: Option<bool>,
    expect_run_id: Option<String>,
}

async fn text_bot(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<TextIn>) -> Result<Response, LcError> {
    lifecycle::send_text(&app, &id, &b.text, b.enter.unwrap_or(true), b.expect_run_id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

async fn abandon_turn(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    lifecycle::abandon_turn(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

async fn get_messages(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    // Unknown id would trip the FK in `conversation_id` → 502 (review 2026-09-12 #9); deleted bots stay readable (API.md §10.4).
    if db::bot(&app.db, &id).await.map_err(any_err)?.is_none() {
        return Err(LcError::NotFound("bot".into()));
    }
    let conv = db::conversation_id(&app.db, &id).await.map_err(any_err)?;
    let limit: i64 = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(100).clamp(1, 500);
    let before_rowid = match q.get("before") {
        Some(b) => Some(
            sqlx::query_scalar::<_, i64>("SELECT rowid FROM messages WHERE id = ?")
                .bind(b)
                .fetch_optional(&app.db)
                .await
                .map_err(any_err)?
                .ok_or_else(|| LcError::Bad(format!("before message `{b}` not found")))?,
        ),
        None => None,
    };
    let rows = match before_rowid {
        Some(b) => sqlx::query_as::<_, db::Message>(
            "SELECT * FROM messages WHERE conversation_id=? AND rowid < ? ORDER BY rowid DESC LIMIT ?",
        )
        .bind(&conv)
        .bind(b)
        .bind(limit + 1),
        None => sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE conversation_id=? ORDER BY rowid DESC LIMIT ?")
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

/// SPEC §13.4.
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
    // Below ~60 columns TUI text is unrecoverably fragmented (seen on `w8:pK` at 31); lets the UI explain it.
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

    #[test]
    fn unnamed_items_keep_their_relative_order_at_the_end() {
        let mut items = ids(&["a", "new1", "b", "new2"]);
        reorder_by(&mut items, &ids(&["b", "a"]), |x| Some(x.clone()));
        assert_eq!(items, ids(&["b", "a", "new1", "new2"]));
    }

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

#[cfg(test)]
mod message_tests {
    use super::*;

    #[tokio::test]
    async fn messages_page_by_insert_order_when_ids_are_not_monotonic() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let bot_id = "messages-test-bot";
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?,'claude','tok',?)")
            .bind(bot_id)
            .bind(&e.project_id)
            .bind(bot_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
        for id in ["m3", "m1", "m2"] {
            sqlx::query(
                "INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?, 'user', ?, 'web', ?)",
            )
            .bind(id)
            .bind(&conv)
            .bind(id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        }

        let mut before: Option<String> = None;
        let expected = ["m2", "m1", "m3"];
        for (page, want) in expected.iter().enumerate() {
            let mut q = HashMap::from([(String::from("limit"), String::from("1"))]);
            if let Some(cursor) = &before {
                q.insert("before".into(), cursor.clone());
            }
            let Json(body) = get_messages(State(app.clone()), Path(bot_id.into()), Query(q)).await.unwrap();
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["id"], *want);
            assert_eq!(body["has_more"], page + 1 < expected.len());
            before = Some(messages[0]["id"].as_str().unwrap().to_string());
        }

        let q = HashMap::from([(String::from("before"), String::from("missing"))]);
        assert!(matches!(
            get_messages(State(app), Path(bot_id.into()), Query(q)).await,
            Err(LcError::Bad(_))
        ));
    }

    /// review 2026-09-12 #9; deleted bots still answer (API.md §10.4).
    #[tokio::test]
    async fn messages_for_an_unknown_bot_are_not_found() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        assert!(matches!(
            get_messages(State(app.clone()), Path("no-such-bot".into()), Query(HashMap::new())).await,
            Err(LcError::NotFound(what)) if what == "bot"
        ));
        let gone = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, hook_token, deleted_at, created_at) VALUES (?,?,'gone','claude','tok',?,?)",
        )
        .bind(&gone)
        .bind(&e.project_id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let Json(body) = get_messages(State(app), Path(gone.clone()), Query(HashMap::new())).await.unwrap();
        assert_eq!(body["bot_id"], gone);
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(0));
    }
}

#[cfg(test)]
mod delete_bot_tests {
    use super::*;

    async fn a_bot(e: &crate::testing::Env, name: &str, managed_by: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
        )
        .bind(&id)
        .bind(&e.project_id)
        .bind(name)
        .bind(managed_by)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        id
    }

    /// review 2026-09-12 d: no reconcile walks deleted bots, so nothing else would end that run.
    #[tokio::test]
    async fn a_bot_deleted_while_its_host_is_down_does_not_keep_a_running_run() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let id = a_bot(&e, "remote-ish", "user").await;
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','pane-x','agent','no-such-session',?)",
        )
        .bind(&run)
        .bind(&id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        delete_bot(State(app.clone()), Path(id.clone())).await.unwrap();

        let bot = db::bot(&app.db, &id).await.unwrap().unwrap();
        assert!(bot.deleted_at.is_some(), "the bot is gone");
        assert!(db::active_run(&app.db, &id).await.unwrap().is_none(), "no orphan run stays active");
        let r = db::run(&app.db, &run).await.unwrap().unwrap();
        assert_eq!(r.state, "exited");
        assert!(r.ended_at.is_some());
    }
}

#[cfg(test)]
mod attachment_tests {
    use super::*;
    use axum::http::HeaderValue;

    fn image_headers() -> HeaderMap {
        HeaderMap::from_iter([(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))])
    }

    async fn status(result: Result<Response, LcError>) -> StatusCode {
        match result {
            Ok(response) => response.status(),
            Err(error) => error.into_response().status(),
        }
    }

    #[tokio::test]
    async fn upload_rejects_empty_oversized_unknown_and_deleted_bots() {
        let e = crate::testing::env().await;
        let query = Query(HashMap::new());

        assert_eq!(
            status(upload_attachment(
                State(e.app.clone()),
                Path("missing".into()),
                query.clone(),
                image_headers(),
                Bytes::new(),
            )
            .await)
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(upload_attachment(
                State(e.app.clone()),
                Path("missing".into()),
                query.clone(),
                image_headers(),
                Bytes::from(vec![0; crate::attach::MAX_BYTES + 1]),
            )
            .await)
            .await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(upload_attachment(
                State(e.app.clone()),
                Path("missing".into()),
                query.clone(),
                image_headers(),
                Bytes::from_static(b"png"),
            )
            .await)
            .await,
            StatusCode::NOT_FOUND
        );

        let deleted_id = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, hook_token, deleted_at, created_at)
             VALUES (?, ?, ?, 'claude', ?, ?, ?)",
        )
        .bind(&deleted_id)
        .bind(&e.project_id)
        .bind("deleted-bot")
        .bind("test-token")
        .bind(crate::db::now())
        .bind(crate::db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        assert_eq!(
            status(upload_attachment(
                State(e.app.clone()),
                Path(deleted_id),
                query,
                image_headers(),
                Bytes::from_static(b"png"),
            )
            .await)
            .await,
            StatusCode::NOT_FOUND
        );
    }
}
