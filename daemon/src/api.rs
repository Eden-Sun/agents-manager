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
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, OriginalUri, Path, Query, State};
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
            LcError::NotFoundValue(v) => (StatusCode::NOT_FOUND, Json(v)).into_response(),
            LcError::Conflict(v) => (StatusCode::CONFLICT, Json(v)).into_response(),
            LcError::Bad(m) => (StatusCode::BAD_REQUEST, Json(json!({"error": "bad_request", "message": m}))).into_response(),
            LcError::BadValue(v) => (StatusCode::BAD_REQUEST, Json(v)).into_response(),
            LcError::Unprocessable(v) => (StatusCode::UNPROCESSABLE_ENTITY, Json(v)).into_response(),
            LcError::Forbidden(v) => (StatusCode::FORBIDDEN, Json(v)).into_response(),
            LcError::Unavailable(v) => {
                let retry = v.get("retry_after_secs").and_then(Value::as_i64).unwrap_or(10).max(1);
                (StatusCode::SERVICE_UNAVAILABLE, [(axum::http::header::RETRY_AFTER, retry.to_string())], Json(v)).into_response()
            }
            LcError::Uncommitted(v) => (StatusCode::SERVICE_UNAVAILABLE, Json(v)).into_response(),
            LcError::Upstream(m) => {
                (StatusCode::BAD_GATEWAY, Json(json!({"error": "upstream", "message": m}))).into_response()
            }
        }
    }
}

fn any_err<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// 寫設定的路徑專用：`ConfigStore::update` 在落盤前驗不過時回 **400 `config_invalid`**，而不是 502。
///
/// 502 的定義是「herdr／DB 出錯」（SPEC §3.1）；設定不合法是**請求的問題**，混成同一個碼，呼叫端分不出
/// 「你的設定有問題、改一下再送」跟「ssh 斷了、等一下重試」。`config_written: false` 是關鍵的一半：
/// 跟 `projection_refused` 的 `config_written: true` 相反，這次什麼都沒寫，直接重送修正後的請求就好。
fn cfg_err(e: anyhow::Error) -> LcError {
    match e.downcast_ref::<crate::projection::ConfigInvalid>() {
        // `to_string()` 而不是 `c.0`：Display 才帶著「（config.toml 未變更）」，那句是給人看的 recovery path。
        Some(c) => LcError::BadValue(
            json!({"error": "config_invalid", "message": c.to_string(), "config_written": false}),
        ),
        None => any_err(e),
    }
}

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/state", get(get_state))
        .route("/projects", post(create_project))
        .route("/order", post(set_order))
        .route("/intents", get(list_intents))
        // 對話輸入框的草稿（各瀏覽器共用，見 `drafts.rs`）。
        .route("/drafts", get(crate::drafts::get_http))
        .route("/drafts/{key}", axum::routing::put(crate::drafts::put_http))
        .route("/projects/{id}", patch(patch_project).delete(delete_project_http))
        .route("/projects/{id}/bots", post(create_bot))
        .route("/projects/{id}/messages", get(get_project_messages))
        // §6.5e：非 agent 的 shell／服務 pane。
        .route("/projects/{id}/panes", get(crate::panes::list_for_project))
        .route("/panes/{id}/adopt", post(crate::panes::adopt))
        .route("/panes/{id}/close", post(crate::panes::close))
        .route("/panes/{id}/focus", post(crate::panes::focus))
        .route("/panes", get(crate::panes::list_all))
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
        .route("/bots/{id}", patch(patch_bot).delete(delete_bot_http))
        // SPEC §6.9；放在 `{id}` 那組前面，否則 `restart-idle` 會被當成 bot id。
        .route("/bots/restart-idle", post(restart_idle_bots))
        .route("/bots/{id}/start", post(start_bot))
        .route("/bots/{id}/restart", post(restart_bot))
        .route("/bots/{id}/credential/rotate", post(rotate_bot_credential))
        .route("/bots/{id}/fork", post(crate::fork::fork_bot))
        .route("/bots/{id}/promote", post(crate::promote::promote_bot))
        .route("/bots/{id}/stop", post(stop_bot))
        .route("/bots/{id}/rewind", post(crate::rewind::post_rewind))
        .route("/bots/{id}/preview", get(preview_get).post(preview_start).delete(preview_stop))
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
        .route("/bots/{id}/pending-question", get(crate::pending_question::get_pending_question))
        .route("/bots/{id}/local-image", get(crate::local_image::get))
        // bot 交給使用者的檔案（§6.5f）：只讀 outbox。scratchpad 不再給使用者，舊路徑明確 404。
        .route("/bots/{id}/outbox", get(crate::outbox::list))
        .route("/bots/{id}/outbox/file", get(crate::outbox::file))
        .route("/bots/{id}/scratchpad", get(crate::outbox::scratchpad_gone))
        .route("/bots/{id}/scratchpad/file", get(crate::outbox::scratchpad_gone))
        .route("/bots/{id}/read", post(crate::read_marks::post))
        .route("/projects/{id}/group/read", post(crate::read_marks::post_group))
        .route("/turns/{id}/abandon", post(abandon_turn))
        .route("/turns/{id}/withdraw", post(withdraw_turn))
        .route("/bots/{id}/abort", post(abort_bot))
        .route("/hosts", post(create_host))
        .route("/hosts/{name}", delete(delete_host))
        .route("/hosts/{name}/reconnect", post(reconnect_host))
        .route("/hosts/{name}/tools/refresh", post(refresh_tools))
        .route("/hosts/{name}/tools/install", post(install_tool))
        // header 一鍵升級 codex：裝好、驗版本、接著重啟那台的 codex（SPEC §6.9）。只給 UI。
        .route("/hosts/{name}/cli-update", post(crate::cli_update::post_cli_update))
        // header 一鍵升級 herdr：下載驗版本 → 等閒置 → 維護窗口裡重啟 server → bot 接回（SPEC §6.9b）。只給 UI。
        .route("/hosts/{name}/herdr-update", post(crate::herdr_upgrade::post_herdr_update))
        .route("/hosts/{name}/identities/{identity}/login", post(login_identity))
        .route("/hosts/{name}/identities/{identity}/logout", post(logout_identity))
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
        .route("/quota/probe", post(probe_quota))
        // issue #90：build scheduler 的唯讀現況（UI 用一般 X-AM-Token）。acquire／renew／release 見下方
        // 的 `/build-slots/*`（不在 `/api` 底下：bot 的 pane 只有自己的 hook token，拿不到這個）。
        .route("/build-slots", get(crate::build_scheduler::get_status))
        // issue #104：開發者專用外部 Cargo worker 設定。密碼只進 data-dir 的 0600 secret file。
        .route(
            "/build/remote",
            get(crate::remote_cargo::get_settings).put(crate::remote_cargo::put_settings),
        )
        .route("/build/remote/test", post(crate::remote_cargo::test_settings))
        .route("/build/remote/install-toolchain", post(crate::remote_cargo::install_settings))
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
        // 使用者在更新提示上按「請 AGM 解析」：把這一版的 changelog 派給協調者判讀（唯讀）。
        .route(
            "/claude-update/review",
            get(crate::claude_review::get_review).post(crate::claude_review::post_review),
        )
        .route("/supervisor/incidents", get(crate::supervisor::api::get_incidents))
        // 人設：持久版本是權威，內嵌版只在首次安裝當種子（SPEC §18.11）。
        .route(
            "/supervisor/persona",
            get(crate::supervisor::api::get_persona).put(crate::supervisor::api::put_persona),
        )
        .route("/supervisor/persona/adopt-embedded", post(crate::supervisor::api::post_persona_adopt))
        .route("/supervisor/build-inputs", get(crate::supervisor::api::get_build_inputs))
        // 左上角「立即部署」：落後多少、有沒有在跑；按下去交給既有的 daemon-update-kick（SPEC §18.2）。
        .route("/deploy/status", get(crate::deploy_now::get_status))
        .route("/deploy/now", post(crate::deploy_now::post_now))
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
        // 已安裝的 bin/agm vs 這顆 binary 內嵌的那份（SPEC §18.2a）：GET 比對、POST 就地換版，
        // 不必等下一次開機（issue #532）。
        .route(
            "/supervisor/cli",
            get(crate::supervisor::cli_refresh::get_cli).post(crate::supervisor::cli_refresh::post_cli_refresh),
        )
        // 排程腳本卡住時喊人（SPEC §18.9）：只寫一則 durable inbox 事件。
        .route("/supervisor/ops-alerts", post(crate::supervisor::api::post_ops_alert))
        .route("/supervisor/evidence", get(crate::supervisor_evidence::search))
        .merge(crate::supervisor::responder_api::routes())
        .merge(crate::release_triage::http::routes())
        .merge(crate::upstream_update::routes())
        .merge(crate::judge::http::routes())
        .merge(crate::deleted_bots::routes())
        .route("/bots/{id}/restore", post(restore_bot))
        .route("/identities", post(create_identity))
        .route("/identities/{name}", delete(delete_identity))
        .route("/fs/dirs", get(list_dirs))
        .route("/capabilities", get(get_capabilities))
        .route("/services/daemon-swap/probe/{id}", post(service_daemon_swap_probe))
        .route("/services/daemon-swap/restart-window", post(service_daemon_swap_restart_window))
        .route("/services/herdr-upgrade/resume/{id}", post(service_herdr_upgrade_resume))
        .route("/services/herdr-upgrade/notify", post(service_herdr_upgrade_notify))
        .route("/supervisor/herdr-maintenance", get(crate::herdr_maintenance::get))
        .route("/supervisor/herdr-maintenance/open", post(crate::herdr_maintenance::open))
        .route("/supervisor/herdr-maintenance/end", post(crate::herdr_maintenance::end))
        .layer(axum::middleware::from_fn_with_state(app.clone(), auth))
        .route("/session", get(get_session))
        // 沒這條路由的 /api/* 要回 JSON 404，不能掉到外層的 SPA fallback（200 的 index.html）。
        .fallback(api_route_not_found);

    Router::new()
        .nest("/api", api)
        .route("/ws", get(ws_handler))
        .route("/hook/{provider}", post(crate::hookrecv::receive))
        .route("/relay/announce", post(relay_announce))
        // Credential-bearing pane creation is fenced against concurrent credential rotation.
        .route("/relay/spawn/begin", post(crate::credential_spawn::begin))
        .route("/relay/spawn/finish", post(crate::credential_spawn::finish))
        .route("/relay/spawn/abort", post(crate::credential_spawn::abort))
        // §6.5e：bot 開完 pane 後回報用途（歸屬另外從行程環境推斷）。
        .route("/relay/pane", post(relay_pane))
        // issue #90：cargo shim 用（bot 的 hook token，或人工 host shell 的一般 X-AM-Token）。
        .route("/build-slots/acquire", post(crate::build_scheduler::post_acquire))
        .route("/build-slots/renew", post(crate::build_scheduler::post_renew))
        .route("/build-slots/release", post(crate::build_scheduler::post_release))
        .fallback(get(crate::assets::serve))
        .with_state(app)
}

async fn api_route_not_found() -> LcError {
    LcError::NotFound("route".into())
}

/// SPEC §6.5d：shim 先報，hook 回音時補 `relay_from`，總管裁示才不像使用者打的。
/// 驗證用該 bot 的 `hook_token`（`X-AM-Bot-Token`）：它只證明「我是那顆 bot」，也只用來標來源。
#[derive(serde::Deserialize)]
struct RelayAnnounce {
    bot_id: String,
    to_agent: String,
    text: String,
    /// shim 的 `--ack`（`1`）／`--reply-to <id>`：寄件端明講是回覆才不叫醒 AGM（SPEC §18.15）。
    #[serde(default)]
    ack: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
}

#[derive(Deserialize)]
struct RelayPane {
    bot_id: String,
    pane_id: String,
    #[serde(default)]
    purpose: String,
}

/// `POST /relay/pane`（表單，同 `/relay/announce` 那條路）：記下這顆 pane 是為了什麼開的。
/// 歸屬不靠這裡——那是掃描時從 pane 行程樹的 `AM_BOT_ID` 推斷的（§6.5e）；報不成功只是少一個用途字串。
async fn relay_pane(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Form(body): axum::extract::Form<RelayPane>,
) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let bot = match db::bot(&app.db, &body.bot_id).await {
        Ok(Some(b)) if b.deleted_at.is_none() && !token.is_empty() && ct_eq(token, &b.hook_token) => b,
        _ => return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unknown bot or bad token"}))),
    };
    // 讀不到 host 就 503 讓 shim 重試：退回 local 會把遠端 pane id 寫進本機 namespace（#243）。
    let Ok(host) = db::bot_host(&app.db, &bot.id).await else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "bot host unreadable; retry"})));
    };
    match crate::panes::note_purpose(&app, &host, &body.pane_id, &bot, body.purpose.trim()).await {
        Ok(()) => (StatusCode::OK, Json(json!({"pane_id": body.pane_id, "purpose": body.purpose}))),
        Err(e) => {
            tracing::warn!(pane = %body.pane_id, error = ?e, "could not record a pane purpose");
            (StatusCode::OK, Json(json!({"pane_id": body.pane_id, "recorded": false})))
        }
    }
}

async fn relay_announce(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    // 表單而非 JSON：POSIX sh shim 用 `--data-urlencode` 對任意內容都安全。
    axum::extract::Form(body): axum::extract::Form<RelayAnnounce>,
) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let ok = match db::bot(&app.db, &body.bot_id).await {
        Ok(Some(b)) if b.deleted_at.is_none() => !token.is_empty() && ct_eq(token, &b.hook_token),
        _ => false,
    };
    if !ok {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unknown bot or bad token"})));
    }
    // 寫給 AGM 的：協調者存在時排進它的佇列，shim 看到 `routed` 就不再打進 pane（SPEC §18.15）。
    // 路由狀態**不知道**不等於「不是 AGM」（issue #143）：查不出目標是不是 AGM、或確定是 AGM 卻寫不進佇列，
    // 都回 503 `routing_unavailable`，shim 看到就明確失敗、不直送——直送會繞過 durable inbox、去重與 wake／ack 語意。
    let unavailable = |why: String| {
        tracing::warn!(to = %body.to_agent, error = %why, "could not route a bot request for AGM; refusing instead of falling back to the pane");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "routing_unavailable", "routing_unavailable": true, "retryable": true, "detail": why,
                        "message": "這則可能是寫給 AGM 的，但現在排不進協調佇列：沒有送出，也不會直接打進它的 pane，請稍後重試"})),
        )
    };
    match crate::supervisor::bot_requests::role_bot_by_agent(&app, &body.to_agent).await {
        Ok(Some(target)) => {
            let mark = crate::supervisor::bot_requests::ReplyMark {
                ack: matches!(body.ack.as_deref().map(str::trim), Some("1" | "true")),
                reply_to: body.reply_to.as_deref(),
            };
            match crate::supervisor::bot_requests::intercept(&app, &target, &body.bot_id, &body.text, None, &[], true, "herdr_shim", mark).await {
                Ok(Some(v)) => return (StatusCode::OK, Json(v)),
                // 協調者還沒建立（舊部署）或角色對自己：本來就不攔，照舊直送。
                Ok(None) => {}
                Err(e) => return unavailable(format!("{e:?}")),
            }
        }
        Ok(None) => {}
        Err(e) => return unavailable(format!("{e:?}")),
    }
    // #610：shim 收到 2xx 後會直接把 prompt 送進 pane。先確認 watcher admission 的 DB lookup
    // 成功；讀取錯誤不是「非 managed target」，必須回 retryable error，不能讓唯一的 watchdog 消失。
    let relay_run = if body.text.trim().is_empty() {
        None
    } else {
        match crate::lifecycle::relay_watch::resolve(&app, &body.bot_id, &body.to_agent).await {
            Ok(run) => run,
            Err(e) => {
                tracing::warn!(to = %body.to_agent, error = ?e, "relay watch admission failed; refusing direct prompt so the shim can retry");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "error": "relay_watch_unavailable",
                        "watch_unavailable": true,
                        "retryable": true,
                        "detail": format!("{e:#}"),
                        "message": "收件目標狀態暫時讀不到；沒有直送，請稍後重試",
                    })),
                );
            }
        }
    };
    // 報備記的是寄件者所在的主機（同名 agent 在別台主機不能認領）；讀不到就跟收件目標讀不到一樣，要 shim 重試。
    let host = match db::bot_host(&app.db, &body.bot_id).await {
        Ok(host) => host,
        Err(e) => {
            tracing::warn!(to = %body.to_agent, error = ?e, "relay announce could not read the sender's host; refusing direct prompt so the shim can retry");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "relay_watch_unavailable",
                    "watch_unavailable": true,
                    "retryable": true,
                    "detail": format!("{e:#}"),
                    "message": "寄件者所在主機暫時讀不到；沒有直送，請稍後重試",
                })),
            );
        }
    };
    crate::agent_relay::announce(&host, &body.bot_id, &body.to_agent, &body.text);
    // #380：收件方 UI 看得出在跑，字卡在輸入列時補 Enter；resolve 已在回 2xx 前確認，後續盯梢仍背景做。
    let (app2, from, text) = (app.clone(), body.bot_id.clone(), body.text.clone());
    tokio::spawn(async move { crate::lifecycle::relay_watch::on_resolved_announce(&app2, &from, &text, relay_run).await });
    (StatusCode::OK, Json(json!({})))
}

/// issue #143：寫給 AGM 的申請，路由狀態**不知道**不等於「不是 AGM」。
#[cfg(test)]
mod relay_announce_tests {
    use super::*;
    use crate::supervisor::bot_requests::flow_tests;

    async fn announce(app: &Arc<App>, to: &str, text: &str) -> (StatusCode, Value) {
        let mut headers = HeaderMap::new();
        headers.insert("X-AM-Bot-Token", "tok-w1".parse().unwrap());
        let body = RelayAnnounce { bot_id: "w1".into(), to_agent: to.into(), text: text.into(), ack: None, reply_to: None };
        let (code, Json(v)) = relay_announce(State(app.clone()), headers, axum::extract::Form(body)).await;
        (code, v)
    }

    async fn bot_requests(app: &Arc<App>) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='bot_request'").fetch_one(&app.db).await.unwrap()
    }

    /// 目標確定是 AGM、協調佇列卻寫不進去：以前 log 一句就 fallback，回 200 `{}`，shim 照原本流程把申請直接打進
    /// AGM 的 pane——繞過 durable inbox、去重、wake／ack 語意，控制面也不知道走了旁路。要回 5xx，而且不 announce。
    /// 查不出目標是不是 AGM（DB 讀失敗）也一樣。真的不是 AGM 的目標照舊直送。
    #[tokio::test]
    async fn a_request_for_agm_that_cannot_be_queued_is_not_sent_to_the_pane() {
        let app = flow_tests::app().await;
        flow_tests::configure_responder(&app).await;

        // 一般 bot：不是 AGM，照舊 announce、回 200 `{}`（shim 接著直送）。
        let (code, v) = announce(&app, "builder", "幫我看一下 relay-143-plain").await;
        assert_eq!((code, v), (StatusCode::OK, json!({})));
        assert!(crate::agent_relay::claim(crate::config::LOCAL_HOST, "builder", "幫我看一下 relay-143-plain").is_some(), "一般目標照舊記下直送");

        // 目標是 AGM，但協調佇列寫不進去。
        sqlx::query("CREATE TRIGGER test_inbox_down BEFORE INSERT ON supervisor_inbox BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();
        let (code, v) = announce(&app, "AGM", "請核准重建 relay-143-queue").await;
        assert!(code.is_server_error(), "寫不進佇列不能假裝成「不是 AGM」：{code} {v}");
        assert_eq!(v["routing_unavailable"], true, "{v}");
        assert!(crate::agent_relay::claim(crate::config::LOCAL_HOST, "AGM", "請核准重建 relay-143-queue").is_none(), "不能退回直接打進 pane");

        // DB 恢復後重試：只有一筆 durable 申請。
        sqlx::query("DROP TRIGGER test_inbox_down").execute(&app.db).await.unwrap();
        let (code, v) = announce(&app, "AGM", "請核准重建 relay-143-queue").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["routed"], "responder", "{v}");
        assert_eq!(bot_requests(&app).await, 1);

        // 連「目標是不是 AGM」都查不出來（角色表讀不到）：一樣不能當成「不是」。
        sqlx::query("DROP TABLE supervisor_roles").execute(&app.db).await.unwrap();
        let (code, v) = announce(&app, "AGM-responder", "請核准重啟 relay-143-lookup").await;
        assert!(code.is_server_error(), "{code} {v}");
        assert!(crate::agent_relay::claim(crate::config::LOCAL_HOST, "AGM-responder", "請核准重啟 relay-143-lookup").is_none());
    }

    /// #610：announce 的 managed-run 查詢讀不到時，不能先回 200 讓 shim 直送並永久略過 watchdog。
    #[tokio::test]
    async fn a_transient_relay_watch_admission_error_is_retryable_before_direct_announce() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let to = "relay-610-unknown";
        let text = "請重試 relay-610-admission";
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,identity,hook_token,created_at) VALUES ('w1',?,'fixer','claude','cc0','tok-w1',?)")
            .bind(&env.project_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

        crate::testing::make_table_unreadable(&app, "runs").await;
        let (code, body) = announce(&app, to, text).await;
        let announced = crate::agent_relay::claim(crate::config::LOCAL_HOST, to, text);
        crate::testing::make_table_readable(&app, "runs").await;

        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "讀不到收件 run 時 shim 必須重試，不能直送：{body}");
        assert_eq!(body["watch_unavailable"], true, "回應要明確標記 watcher admission 失敗：{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert!(announced.is_none(), "watcher admission 失敗時不能先宣告、放 shim 直送");

        let (retry_code, retry_body) = announce(&app, to, text).await;
        assert_eq!((retry_code, retry_body), (StatusCode::OK, json!({})), "資料庫恢復後可正常重試");
        assert!(crate::agent_relay::claim(crate::config::LOCAL_HOST, to, text).is_some(), "成功重試才記下直送 announce");
    }
}

/// TCP peer address, **not** `Host`: the header is caller-chosen, so on 0.0.0.0 anyone on the LAN
/// could fetch the UI token with `curl -H 'Host: localhost:…'`. Needs connect_info (main.rs).
/// `allow_lan` is the explicit dev-only opt-in (off in the packaged app, `main.rs::dev_lan_default`);
/// no range allowlist because "LAN" includes overlays like Tailscale (100.64.0.0/10). The peer is
/// not filtered, but Host／Origin still are (`origin_is_local`): that is what stops DNS rebinding.
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
/// `Host` 或 Origin 的 authority 是不是 loopback 名稱（可帶 port）；`[::1]` 自己有冒號，只有尾端全是數字才當 port。
fn is_loopback_host(authority: &str) -> bool {
    let a = authority.trim();
    let host = match a.rsplit_once(':') {
        Some((h, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => h,
        _ => a,
    };
    matches!(host.to_ascii_lowercase().as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

/// `allow_lan` 開著時 Host／Origin 能用的名字：區網存取用的是 IP 字面值、`localhost`、單一標籤主機名、
/// `.local`／`.ts.net`／`.home.arpa`／`.lan`／`.internal`／`.localdomain`，這些名字攻擊者的 DNS 都給不出來。
/// 其他名字（含 `127.0.0.1.evil.example`、`192.168.1.5.nip.io`）擋掉：那正是 DNS rebinding 用的。
/// 要用自訂網域，在環境變數 `AM_ALLOWED_HOSTS` 明列（逗號分隔、整個主機名完全相同）。
fn lan_host_ok(authority: &str, extra: &[String]) -> bool {
    let a = authority.trim();
    let host = match a.rsplit_once(':') {
        Some((h, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => h,
        _ => a,
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return inner.parse::<std::net::Ipv6Addr>().is_ok();
    }
    if host.parse::<std::net::Ipv4Addr>().is_ok() || host == "localhost" || extra.iter().any(|e| *e == host) {
        return true;
    }
    if !host.contains('.') {
        return host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    }
    [".local", ".ts.net", ".home.arpa", ".lan", ".internal", ".localdomain"]
        .iter()
        .any(|suffix| host.ends_with(suffix) && host.len() > suffix.len())
}

fn extra_allowed_hosts() -> &'static [String] {
    static EXTRA: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    EXTRA.get_or_init(|| {
        std::env::var("AM_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect()
    })
}

/// Host 與 Origin 的名字都要過關：`allow_lan` 關著＝loopback 名稱，開著＝[`lan_host_ok`]。
/// 任何 Origin 都放行的舊行為讓別的網頁（或 rebind 過來的網域）在 LAN 模式下拿得到 UI token。
fn origin_is_local(headers: &HeaderMap, _port: u16, allow_lan: bool) -> bool {
    let name_ok = |authority: &str| if allow_lan { lan_host_ok(authority, extra_allowed_hosts()) } else { is_loopback_host(authority) };
    // DNS rebinding：同源 GET 沒有 Origin，所以 Host 也要過關（沒有 Host 的不是瀏覽器）。
    if let Some(host) = headers.get("host") {
        let Ok(host) = host.to_str() else { return false };
        if !name_ok(host) {
            return false;
        }
    }
    let Some(o) = headers.get("origin").and_then(|v| v.to_str().ok()) else { return true };
    let Some(rest) = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")) else { return false };
    if rest.contains('/') || rest.contains('@') {
        return false;
    }
    name_ok(rest)
}

/// 呼叫端自稱是某顆 bot（`X-AM-Bot-Id`）而且拿得出那顆 bot 的 hook token（`X-AM-Bot-Token`）時，回
/// `<id>(<name>)`；其餘一律 `None`（只有 UI token 的網頁、腳本，或 token 對不上的冒名）。
/// 讀不到 bot 也是 `None`：DB 一時忙不能把自稱升級成驗過的。
pub(crate) async fn verified_caller_bot(app: &Arc<App>, headers: &HeaderMap) -> Option<String> {
    // 空的／非 UTF-8 的 token 算「沒驗過」，跟 `relay_auth` 同一條線（#339／44348e62）；前後空白一樣先去掉。
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
    let (id, token) = (header("X-AM-Bot-Id")?, header("X-AM-Bot-Token")?);
    let bot = db::bot(&app.db, id).await.ok().flatten()?;
    (bot.deleted_at.is_none() && ct_eq(token, &bot.hook_token)).then(|| format!("{}({})", bot.id, bot.name))
}

/// 常數時間比對 token：逐位元 OR 差異，不因第一個不同的位元組提早結束（長度不同直接不等，長度本來就不是祕密）。
pub(crate) fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RequestPrincipal {
    User,
    Bot(String),
    Service(String),
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or bad API credential"}))).into_response()
}

async fn auth(State(app): State<Arc<App>>, mut req: axum::extract::Request, next: Next) -> Response {
    let headers = req.headers().clone();
    if !origin_is_local(&headers, app.port, app.allow_lan) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "bad origin"}))).into_response();
    }
    let has_bot = headers.contains_key("X-AM-Bot-Id") || headers.contains_key("X-AM-Bot-Token");
    let has_service = headers.contains_key("X-AM-Service-Id") || headers.contains_key("X-AM-Service-Token");
    let has_ui = headers.contains_key("X-AM-Token");
    let principal = if has_bot || has_service {
        // A presented identity is decisive: partial, invalid, or mixed credentials never fall
        // through to the shared user token.
        if has_bot == has_service || has_ui {
            return unauthorized();
        }
        if has_service {
            let id = headers.get("X-AM-Service-Id").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
            let token = headers.get("X-AM-Service-Token").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
            let (Some(id), Some(token)) = (id, token) else { return unauthorized() };
            let valid = app.service_tokens.read().ok().and_then(|tokens| tokens.get(id).cloned()).is_some_and(|expected| ct_eq(token, &expected));
            if !valid {
                return unauthorized();
            }
            let path = req.extensions().get::<OriginalUri>().map(|uri| uri.0.path()).unwrap_or_else(|| req.uri().path());
            if !crate::service_auth::allows(id, req.method().as_str(), path) {
                return (StatusCode::FORBIDDEN, Json(json!({"error": "service scope denied", "service": id}))).into_response();
            }
            RequestPrincipal::Service(id.to_string())
        } else {
            let id = headers.get("X-AM-Bot-Id").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
            let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
            let (Some(id), Some(token)) = (id, token) else { return unauthorized() };
            let bot = match db::bot(&app.db, id).await {
                Ok(Some(bot)) if bot.deleted_at.is_none() && ct_eq(token, &bot.hook_token) => bot,
                Ok(_) => return unauthorized(),
                Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
            RequestPrincipal::Bot(bot.id)
        }
    } else {
        let tok = headers.get("X-AM-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
        if !has_ui || !ct_eq(tok, &app.ui_token) {
            return unauthorized();
        }
        RequestPrincipal::User
    };
    req.extensions_mut().insert(principal.clone());
    // 會改東西的請求記下是誰發的：config.toml 的寫入 log 與刪除 intent 要引用（issue #406）。
    if matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS) {
        return next.run(req).await;
    }
    // 只有 hook token 對得上才算「驗過的 bot」；`X-AM-Caller` 一律只當自稱記（review d77434c0 #3）。
    let verified = if matches!(principal, RequestPrincipal::Bot(_)) { verified_caller_bot(&app, &headers).await } else { None };
    let service = match &principal { RequestPrincipal::Service(id) => Some(id.as_str()), _ => None };
    let caller = crate::config_audit::describe_request(&req, verified.as_deref(), service);
    crate::config_audit::HTTP_CALLER.scope(caller, next.run(req)).await
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

pub(crate) fn lamp(connected: bool, run: Option<&db::Run>) -> &'static str {
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
    let baselines = app.host_baseline.lock().await.clone();
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
            // #709：讀當下的設定（改了不重連，連線上的快照會是舊的）。
            "shared_session": crate::shared_host::is_shared(app, &c.name).await,
            "connected": connected,
            "error": c.error_string().await,
            // 遠端斷線起點；連著／本機為 null（UI 的離線警示條算「離線多久」）。
            "disconnected_since": c.disconnected_since(),
            "attach_command": crate::config::attach_command(c.cfg.as_ref(), &app.herdr_session),
            "tools": t.map(|x| json!(x.tools)),
            // Login state *on this host*, `[[identities]]` + this host's `ccN` aliases (SPEC §16).
            "identities": t.map(|x| json!(x.identities)),
            // Env unexpanded, as written.
            "shell_identities": t.map(|x| json!(x.shell_identities)),
            "tools_checked_at": t.map(|x| x.checked_at.clone()),
            // 工作環境一致性（只讀檢查，#719）：`{issues: null|[{id,severity,message}], checked_at}`；沒量過＝null。
            "baseline": baselines.get(&c.name).map(|b| json!(b)),
            // herdr 版本（server／protocol 來自 ping，只在連著時報；CLI 來自探測），SPEC §11.6。
            "herdr": crate::herdr_version::for_host(&c, connected, t),
        }));
    }
    out
}

pub async fn state_json(app: &Arc<App>) -> Result<Value, LcError> {
    let connected = app.connected.load(Ordering::SeqCst);
    let projects = db::live_projects(&app.db).await.map_err(any_err)?;
    let bots = db::live_bots(&app.db).await.map_err(any_err)?;
    let unread = crate::read_marks::unread_counts(&app.db).await.map_err(any_err)?;
    let read_marks = crate::read_marks::marks(&app.db).await.map_err(any_err)?;
    let group_unread = crate::read_marks::group_unread_counts(&app.db).await.map_err(any_err)?;
    let group_marks = crate::read_marks::group_marks(&app.db).await.map_err(any_err)?;
    // §6.11：AGM 因為閒置收起來的那些。一次讀完，免得每顆 bot 再問一次資料庫。
    let asleep = crate::supervisor::idle_sleep::all_asleep(app).await;
    let previews = crate::preview::state_map(&app.db).await.map_err(any_err)?;
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
                // 主力那列的固定順序（#344）：1 起算，0＝沒排過；取消釘選不清。
                "primary_position": b.primary_position,
                // 執行中的 CLI 載入的啟動設定跟現在存的不同＝要重啟（#353）：從資料算，PATCH 回應掉了也看得到。
                "needs_restart": run.as_ref().is_some_and(|r| crate::launch_rev::is_stale(b, r)) && !crate::lifecycle::is_deferred(&b.id),
                "live_apply_deferred": crate::lifecycle::is_deferred(&b.id),
                "cwd": b.cwd,
                "agent_name": run.as_ref().and_then(|r| r.agent_name.clone()).unwrap_or_else(|| crate::config::agent_name(&p.label, &b.id)),
                // #714：`run.background_jobs`＝回合結束後畫面上還標著的背景工作數（記憶體裡的，不在 DB）。
                "run": crate::background_jobs::run_json(app, &run, run.as_ref().map(|r| r.id.as_str())),
                // §6.11：停著是因為 AGM 收起來省 RAM，不是壞掉也不是使用者關的；下次要用會自動
                // 用 `--resume` 叫醒。`null` = 不是這種停。
                // 預覽模式（§6.12）：`{status, port}`；沒開（或 off）是 `null`。
                "preview": previews.get(&b.id),
                "asleep": asleep.get(&b.id).map(|(at, mins)| json!({"since": at, "idle_minutes": mins})),
                "queued_turn": queued_turn,
                "lamp": lamp(bot_connected, run.as_ref()),
                // 跨裝置共用的未讀回合數與已讀標記（read_marks.rs）。
                "unread": unread.get(&b.id).copied().unwrap_or(0),
                "read_mark": read_marks.get(&b.id).map(|m| json!({"at": m.at, "id": m.message_id})),
            }));
        }
        out.push(json!({
            "id": p.id, "path": p.path, "label": p.label, "host": p.host,
            "workspace_id": p.workspace_id,
            "handed_off_to": p.handed_off_to,
            // 群組未讀與已讀標記跨裝置共用（read_marks.rs，#756）。
            "group_unread": group_unread.get(&p.id).copied().unwrap_or(0),
            "group_read_mark": group_marks.get(&p.id).map(|m| json!({"at": m.at, "id": m.message_id})),
            "github": crate::github::cached(app, &p.id).await,
            "bots": bl,
        }));
    }
    Ok(json!({
        "daemon_seq": app.current_seq(),
        // 現在有沒有一批一鍵重啟在跑（issue #492）：進度只走 WS，`bots_restart_done` 收不到時前端要有地方對帳。
        "restart_batch": crate::bulk_restart::running_batch(&app.data_dir),
        // 還沒收尾的 codex 升級（同一個理由：`cli_update_done` 收不到時的對帳來源）；存在 DB，daemon 重啟後也還在（#564）。
        "cli_updates": crate::cli_update::running_list(app).await,
        "herdr_updates": crate::herdr_upgrade::running_list(app),
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

/// issue #73 reopen：daemon 裡所有寫 config.toml 的 mutation（Project／Bot／identity）都走
/// `projection::update_and_project`——套用、純驗證、DB-backed 大量軟刪閘門都在**寫入 TOML 之前**
/// 做完，取代「先 `ConfigStore::update` 落盤、再另外呼叫 `project_config` 投影」的兩段式。
/// 閘門擋下來時 config.toml **沒有**被動過，`config_written: false`：呼叫端改一下再送同一個請求就好，
/// 不必先去 config.toml 補列（跟啟動投影／背景重投那類沒有伴隨 mutation 的 `project_config` 呼叫端不同，
/// 那些仍是 `config_written: true`，見 `projection::guard_removals`）。
fn projection_err(e: anyhow::Error) -> LcError {
    match e.downcast_ref::<crate::projection::ProjectionRefused>() {
        Some(r) => LcError::conflict(
            "projection_refused",
            json!({"reason": "projection_refused",
                   "message": format!("{}（這次的變更沒有寫進 config.toml：處理完 DB 那邊的落差，或改小這次的範圍，再重送同一個請求）", r.detail),
                   "config_written": false,
                   "bots": r.bots, "projects": r.projects}),
        ),
        None => cfg_err(e),
    }
}

#[cfg(test)]
mod cfg_err_tests {
    /// issue #73：設定不合法是**請求的問題**，回 400 `config_invalid` 並講明什麼都沒寫。
    ///
    /// 混進 502（「herdr／DB 出錯」）的話，呼叫端分不出「改一下再送」跟「ssh 斷了、等一下重試」。
    /// 觸發路徑是真的會發生的那條：外面把 config.toml 換成不合法的檔案，下一次寫設定時
    /// `ConfigStore::update` 重讀最新版、套用、驗不過。
    #[tokio::test]
    async fn an_invalid_config_is_a_400_that_says_nothing_was_written() {
        let env = crate::testing::env().await;
        let app = &env.app;
        // `kind = 'nope'` 不是合法的 kind：投影一定擋。
        let broken = "[server]\nlisten = '127.0.0.1:7788'\n\n                      [[projects]]\nid = 'p1'\npath = '/tmp'\nlabel = 'demo'\nhost = 'remote'\n\n                      [[projects.bots]]\nid = 'b1'\nname = 'worker'\nkind = 'nope'\n";
        std::fs::write(&app.cfg.path, broken).unwrap();

        let err = app.cfg.update(|cfg| { cfg.projects[0].label = "renamed".into(); Ok(()) }).await.unwrap_err();
        match super::cfg_err(err) {
            crate::lifecycle::LcError::BadValue(body) => {
                assert_eq!(body["error"], "config_invalid");
                assert_eq!(body["config_written"], false, "跟 projection_refused 相反：這次什麼都沒寫");
                let msg = body["message"].as_str().unwrap();
                assert!(msg.contains("invalid bot kind"), "原因要留著：{msg}");
                assert!(msg.contains("config.toml 未變更"), "{msg}");
            }
            other => panic!("要是 400 config_invalid，不是 {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&app.cfg.path).unwrap(), broken, "檔案一個字都不該動");
    }

    /// 真正的上游錯誤仍然是 502：`cfg_err` 只認 `ConfigInvalid`，不是把所有錯誤都降成 400。
    #[test]
    fn other_errors_are_still_upstream() {
        match super::cfg_err(anyhow::anyhow!("herdr socket gone")) {
            crate::lifecycle::LcError::Upstream(m) => assert!(m.contains("herdr socket gone")),
            other => panic!("{other:?}"),
        }
    }

    /// issue #73 reopen：`projection_err` 把 `ProjectionRefused` 對應到 409、`config_written: false`——
    /// 新的 commit boundary 下，閘門一律在落盤前就擋，不會再出現舊 `reproject`（已刪）那種
    /// `config_written: true`（已經寫進 config.toml）的狀況。
    #[test]
    fn projection_err_says_the_change_was_not_written() {
        let refused = crate::projection::ProjectionRefused {
            detail: "test detail".into(),
            bots: vec!["b1".into()],
            projects: vec![],
            supervisor_children: vec![],
            bot_limit_exceeded: false,
        };
        match super::projection_err(anyhow::Error::new(refused)) {
            crate::lifecycle::LcError::Conflict(body) => {
                assert_eq!(body["reason"], "projection_refused");
                assert_eq!(body["config_written"], false);
                assert_eq!(body["bots"], serde_json::json!(["b1"]));
            }
            other => panic!("{other:?}"),
        }
    }
}

/// 刪除走 `projection::delete_from_config` 的單一臨界區。目標不在 config、或授權以外還有列會不見，
/// 都是 409：狀態不對，不是上游壞掉。
async fn delete_in_config(
    app: &Arc<App>,
    target: crate::projection::DeleteTarget<'_>,
) -> Result<crate::projection::Deleting, LcError> {
    crate::projection::delete_from_config(&app.cfg, &app.db, target).await.map_err(|e| {
        if let Some(n) = e.downcast_ref::<crate::projection::NotInConfig>() {
            LcError::conflict("not_in_config", json!({"message": n.to_string()}))
        } else if let Some(r) = e.downcast_ref::<crate::projection::DeleteRefused>() {
            LcError::conflict("delete_refused", json!({"message": r.to_string()}))
        } else {
            any_err(e)
        }
    })
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
    let res = crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
        if cfg.projects.iter().any(|p| p.path == path && p.host == host) {
            anyhow::bail!("duplicate");
        }
        cfg.projects.push(crate::config::ProjectCfg {
            handed_off_to: None,
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
        Err(e) => return Err(projection_err(e)),
    }
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
    /// #708：字串＝移交給那台主機的 daemon，`null`＝收回，不帶＝不動。
    #[serde(default)]
    handed_off_to: Option<Value>,
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
    let handed_off_to = match &b.handed_off_to {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(h)) if !h.trim().is_empty() => Some(Some(h.trim().to_string())),
        Some(_) => return Err(LcError::Bad("handed_off_to must be a non-empty host name or null".into())),
    };
    crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
        let p = cfg
            .projects
            .iter_mut()
            .find(|p| p.id.as_deref() == Some(id.as_str()))
            .ok_or_else(|| anyhow::anyhow!("no-project"))?;
        if let Some(l) = &label {
            p.label = l.clone();
        }
        if let Some(h) = &handed_off_to {
            p.handed_off_to = h.clone();
        }
        Ok(())
    })
    .await
    .map_err(|e| if e.to_string() == "no-project" { LcError::NotFound("project".into()) } else { projection_err(e) })?;
    if let Some(h) = &handed_off_to {
        tracing::warn!(project_id = %id, handed_off_to = ?h, http = %crate::config_audit::http_caller(), "project handoff changed");
        // 收回：下一輪對帳照常接手，不等下一個 herdr 事件。
        if h.is_none() {
            if let Ok(Some(p)) = db::project(&app.db, &id).await {
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::reconcile::reconcile_host(&app, &p.host).await {
                        tracing::warn!(host = %p.host, error = ?e, "reconcile after taking a project back failed");
                    }
                });
            }
        }
    }
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok((StatusCode::OK, Json(json!({"project_id": id, "needs_restart": false}))).into_response())
}

pub(crate) async fn soft_delete_child(app: &Arc<App>, bot_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=? AND deleted_at IS NULL").bind(db::now()).bind(bot_id).execute(&app.db).await.map(|_| ())
}

/// `?confirm=supervisor`：刪 AGM 的 bot／專案要明講（issue #406）。
#[derive(Deserialize, Default)]
pub(crate) struct DeleteQuery {
    pub(crate) confirm: Option<String>,
}

/// HTTP 進來的刪除先過 AGM 閘門；daemon 內部（mission 收臨時 bot）直接叫 `delete_bot`／`delete_project`。
pub(crate) async fn delete_project_http(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<Response, LcError> {
    crate::supervisor_owned::guard_project_delete(&app.db, &id, q.confirm.as_deref()).await?;
    delete_project(State(app), Path(id)).await
}

pub(crate) async fn delete_bot_http(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<DeleteQuery>) -> Result<Response, LcError> {
    crate::supervisor_owned::guard_bot_delete(&app.db, &id, q.confirm.as_deref()).await?;
    delete_bot(State(app), Path(id)).await
}

async fn delete_project(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    // 拿著專案裡每顆 bot 的 per-bot 鎖（start 用同一把）再確認都停了、再定案：鎖外檢查會跟 start 競爭，
    // 刪掉剛被重新啟動的 bot（sol 四輪）。依 id 排序拿鎖；其他路徑一次只拿一把，不會形成環。
    // 跟 `delete_bot` 同一個做法：拿鎖的途中又有 child 被認領進來的話，它沒被鎖住也不在快照裡——放掉重來（不能在持鎖時
    // 再補拿，那會亂了順序）。定案之後還有最後一道（下面的 `late`）。
    let mut attempt = 0;
    let (in_project, ids, guards) = loop {
        let in_project: Vec<db::Bot> =
            db::live_bots(&app.db).await.map_err(any_err)?.into_iter().filter(|b| b.project_id == id).collect();
        let ids: Vec<String> = in_project.iter().map(|b| b.id.clone()).collect();
        #[cfg(test)]
        crate::lifecycle::race_point::hit("delete_project_before_lock", &id).await;
        let (ids, guards) = lock_bots_in_order(&app, ids).await;
        let now: std::collections::BTreeSet<String> =
            db::live_bots(&app.db).await.map_err(any_err)?.into_iter().filter(|b| b.project_id == id).map(|b| b.id).collect();
        if now.iter().eq(ids.iter().collect::<std::collections::BTreeSet<_>>().into_iter()) {
            break (in_project, ids, guards);
        }
        drop(guards);
        attempt += 1;
        if attempt >= 3 {
            return Err(LcError::conflict("children_changed", json!({"project_id": id})));
        }
    };
    for bot_id in &ids {
        if db::active_run(&app.db, bot_id).await.map_err(any_err)?.is_some() {
            return Err(LcError::conflict("all bots must be stopped first", json!({"bot_id": bot_id})));
        }
    }
    // 專案 host 在**定案之前**讀好（#246）：定案之後才重讀、讀不到就退回 local，會去砍本機同 id 的目錄、
    // 遠端那份留著沒人回收。讀不到就 502、什麼都還沒動，可以原樣再按一次。
    let host = db::project(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("project".into()))?.host;
    // 授權範圍在臨界區裡從**當下的** TOML 算；TOML 裡多出沒鎖住的 bot 就拒絕。
    let held: std::collections::HashSet<String> = ids.iter().cloned().collect();
    // 持久 intent（#355 P3）：定案**之前**先 commit（payload＝當時的 bot 快照）；定案之後 daemon 死掉，開機由 `delete_intents::recover_host` 補完。
    let snapshot: Vec<Value> = in_project.iter().map(|b| json!({"id": b.id, "managed_by": b.managed_by})).collect();
    let requested_by = crate::config_audit::http_caller();
    tracing::warn!(project_id = %id, http = %requested_by, "project delete requested");
    let intent = crate::delete_intents::begin(&app, "delete_project", &id, &host, &json!({"bots": snapshot, "requested_by": requested_by}))
        .await?;
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_after_intent", &id).await;
    if let Err(e) = delete_in_config(&app, crate::projection::DeleteTarget::Project { id: &id, held: &held }).await {
        crate::delete_intents::abandon(&app, &intent, "delete_in_config refused").await;
        return Err(e);
    }
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_project_after_commit", &id).await;
    // 輸入框草稿跟著專案走：群組草稿與每顆 bot 的草稿都清掉，其他瀏覽器也會收到清除事件。
    let draft_keys: Vec<String> = std::iter::once(format!("group:{id}")).chain(ids.iter().map(|b| format!("bot:{b}"))).collect();
    crate::drafts::clear_keys(&app, &draft_keys).await;
    // 專案沒了，它底下還開著的任務也跟著收（issue #498）：任務沒有軟刪，而 `mission::store::open_unpaused`
    // （`workflow::wake_stalled_at` 掃的那份）沒有存活性條件——不收的話它們永遠停在 open，十分鐘後還會推一則
    // `mission_next` 要 AGM 去推一個專案與 bot 都不存在的任務。順帶把它們底下還開著的交辦也收掉。
    //
    // 臨時 bot（`agm-mission-*`）**不是**走 `closed_with_live_temp_bots` 那條（issue #498 的複看）：
    // 那條要求 `bots.deleted_at IS NULL`，而下面的 `delete_in_config` 與逐顆軟刪已經先把它們收掉了，
    // 所以那條掃不到、也不需要掃——刪專案本來就把專案裡每一顆 bot 都軟刪了。
    //
    // 盡力而為：收不掉只記 log，不讓刪除回頭——專案在 config 裡已經定案刪除了。
    match crate::mission::store::cancel_open_for_project(&app.db, &id, "project_deleted").await {
        Ok(0) => {}
        Ok(n) => tracing::info!(project = %id, missions = n, "project deleted; its open missions were cancelled"),
        Err(e) => tracing::error!(project = %id, error = ?e, "project deleted but its open missions could not be cancelled"),
    }
    // config 投影只軟刪「不在 TOML 裡的 user bot」，child 本來就不進 TOML，所以會留下一批
    // `deleted_at IS NULL`、project 卻已經軟刪的列：UI 看不到、reconcile 也掃不到（`live_bots_on_host`
    // 要求專案還活著），它們的 pane 與 hook 目錄從此沒人回收（review 2026-09-16）。
    // 鎖還在手上，順手比照 delete_bot 收掉。
    // 專案已經在 config 裡定案刪除，之後不能再「一顆失敗就整段中止」：後面的 child 會永遠留在 `deleted_at IS NULL`，
    // 使用者再按一次只會拿到 not_in_config（#284）。每顆各自試，寫不進去的背景重試到成功才 purge，最後仍回錯讓人知道。
    let mut failed: Vec<db::Bot> = Vec::new();
    let mut kept_dirs: Vec<Value> = Vec::new();
    // 定案之後再列一次：快照與定案之間才被認領進專案的 child（不在 `in_project`、沒被鎖住）一起軟刪。專案已經是已刪，
    // reconcile 之後不會再掃它們，留成 `deleted_at IS NULL` 就永遠沒人收。它們沒經過「都已停止」的檢查，所以有 active run 的
    // 只軟刪、不動目錄（列進 `kept_dirs`）。user bot 在 TOML 裡，`held` 已擋下沒鎖住的。
    let late: Vec<db::Bot> = match db::live_bots(&app.db).await {
        Ok(all) => all.into_iter().filter(|b| b.project_id == id && b.managed_by != "user" && !held.contains(&b.id)).collect(),
        Err(e) => {
            tracing::error!(project = %id, error = ?e, "project deleted but its children could not be listed again; stragglers may stay");
            Vec::new()
        }
    };
    for bot in &late {
        if matches!(db::active_run(&app.db, &bot.id).await, Ok(None)) {
            continue;
        }
        kept_dirs.push(json!({"bot_id": bot.id, "reason": "run_still_active"}));
    }
    let late_with_run: std::collections::HashSet<String> =
        kept_dirs.iter().filter_map(|k| k["bot_id"].as_str().map(str::to_string)).collect();
    for bot in in_project.iter().chain(late.iter()).filter(|b| b.managed_by != "user") {
        // 這個 UPDATE 以前用 `let _ =` 忽略結果：DB 寫不進去也照樣往下 purge，變成
        // 「DB 說它還活著、runtime 目錄卻已經被砍光」，事後救不回來（issue #87）。purge 只能發生在 DB 已經確定寫成 deleted 之後。
        if let Err(e) = soft_delete_child(&app, &bot.id).await {
            tracing::error!(bot = %bot.name, error = ?e, "project deleted but its child could not be soft-deleted; retrying in the background");
            failed.push(bot.clone());
            continue;
        }
        if !late_with_run.contains(&bot.id) {
            lifecycle::purge_bot_dir(&app, &bot.id, &host).await;
        }
        tracing::info!(bot = %bot.name, project = %id, "project deleted; its child bot went with it");
    }
    let retry_failed = !failed.is_empty();
    if retry_failed {
        // 各路徑自己的記憶體重試 task 換成 intent 的重試（#355）：intent 留著，背景用 recovery 補完（含清目錄），daemon 死掉開機也接得回。
        crate::delete_intents::spawn_retry(app.clone(), intent.clone(), "some child bots could not be soft-deleted".into());
    }
    // user bot 由 config 投影軟刪，但 bots/<id>/ 沒人清：本機要等下次開機、遠端永遠不掃（#313）。已確認皆無 active run、鎖在手。
    // 只清「確定軟刪」的；讀不到就留著。清不掉（ssh 失敗等）不算成功，列進 kept_dirs。
    for bot in in_project.iter().filter(|b| b.managed_by == "user") {
        match db::bot(&app.db, &bot.id).await {
            Ok(Some(b)) if b.deleted_at.is_some() => {
                if !lifecycle::purge_bot_dir(&app, &bot.id, &host).await {
                    kept_dirs.push(json!({"bot_id": bot.id, "reason": "purge_failed"}));
                }
            }
            _ => kept_dirs.push(json!({"bot_id": bot.id, "reason": "delete_state_unreadable"})),
        }
    }
    drop(guards);
    app.emit("project_changed", json!({"project_id": id})).await;
    if retry_failed {
        return Err(any_err("project deleted, but some child bots could not be soft-deleted yet; retrying in the background"));
    }
    crate::delete_intents::complete(&app, &intent).await;
    let mut out = json!({});
    if !kept_dirs.is_empty() {
        out["kept_dirs"] = json!(kept_dirs);
    }
    Ok((StatusCode::OK, Json(out)).into_response())
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
    /// 冪等鍵（#352）：同一個鍵＋同樣的請求內容重送，拿回第一次建好的那顆（`{bot_id,name,replayed:true}`），不再建第二顆；
    /// 同一個鍵換了請求內容回 409 `request_id_reused`；沒帶＝照舊每次都建。`name_auto` 時 `name` 只是提示，不算請求內容。
    #[serde(default)]
    client_request_id: Option<String>,
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

fn remap_model(kind: &str, model: Option<&str>) -> (Option<String>, Option<Value>) {
    let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else { return (None, None) };
    match crate::models::remap_deprecated_model(kind, model) {
        Some(to) => (Some(to.to_string()), Some(json!({"model": {"from": model, "to": to}}))),
        None => (Some(model.to_string()), None),
    }
}

/// 寫 config 時 `projection::validate` 在 config 鎖裡擋下的「bot 指到不存在的身分」。檢查與寫入是同一個原子步驟，
/// 所以 `create_bot` 驗過身分之後、寫入之前被刪掉，或 `delete_identity` 查完之後、刪除之前有 bot 開始用，都不會留下孤兒身分；
/// 只是兩邊本來都回籠統的 400 `config_invalid`，這裡翻成呼叫端分得出的碼（404 identity／409 identity still used by bots）。
fn is_unknown_identity(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::projection::ConfigInvalid>().is_some_and(|c| c.0.contains("references unknown identity"))
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
    #[cfg(test)]
    crate::lifecycle::race_point::hit("create_bot_after_identity_check", &pid).await;
    let (model, remapped) = remap_model(&b.kind, b.model.as_deref());
    let effort = crate::config::normalize_effort(&b.kind, b.effort.as_deref()).map_err(LcError::Bad)?;
    let env: BTreeMap<String, String> = b.env.clone().unwrap_or_default();
    let id = db::ulid();
    let used_name = std::sync::Mutex::new(b.name.clone());
    let create_request_id = b.client_request_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(c) = &create_request_id {
        if c.len() > 128 || !c.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_.:".contains(ch)) {
            return Err(LcError::Bad("client_request_id must be 1..=128 chars of [A-Za-z0-9-_.:]".into()));
        }
    }
    // 請求指紋＝會影響這顆 bot 的欄位；`name_auto` 時名字只是提示（重送時瀏覽器的清單已同步，算出來的名字本來就會變）。
    let create_fingerprint = create_request_id.as_ref().map(|_| {
        json!([
            pid,
            if b.name_auto { Value::Null } else { json!(b.name) },
            b.name_auto,
            b.kind,
            model,
            effort,
            b.fast,
            b.persona.as_deref().filter(|s| !s.trim().is_empty()),
            b.args,
            b.autostart,
            b.inject_hooks.unwrap_or(true),
            b.auto_approve.unwrap_or(true),
            identity,
            env,
        ])
        .to_string()
    });
    let replayed: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);
    let reused: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    // child（reconcile 認領的）不在 config.toml，但 DB 的 `(project_id, name)` 唯一約束算它們。
    // 只看 config 會先寫進檔、投影再爆，之後這個專案每次新增／修改都 502（#654）。
    let db_names = live_bot_names(&app.db, &pid).await.map_err(any_err)?;
    let res = crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
        let p = cfg
            .projects
            .iter_mut()
            .find(|p| p.id.as_deref() == Some(pid.as_str()))
            .ok_or_else(|| anyhow::anyhow!("no-project"))?;
        // 同一個請求鍵：在 config 鎖裡查（跟寫入同一個原子步驟），重送不會並發建出兩顆。
        if let Some(c) = &create_request_id {
            if let Some(prev) = p.bots.iter().find(|x| x.create_request_id.as_deref() == Some(c.as_str())) {
                if prev.create_fingerprint == create_fingerprint {
                    *replayed.lock().unwrap() = Some((prev.id.clone().unwrap_or_default(), prev.name.clone()));
                    return Ok(());
                }
                *reused.lock().unwrap() = Some(prev.id.clone().unwrap_or_default());
                anyhow::bail!("request-id-reused");
            }
        }
        let taken = |n: &str| db_names.iter().any(|x| x == n) || p.bots.iter().any(|x| x.name == n);
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
            model: model.clone(),
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
            create_request_id: create_request_id.clone(),
            create_fingerprint: create_fingerprint.clone(),
        });
        Ok(())
    })
    .await;
    match res {
        Ok(()) => {}
        Err(e) if e.to_string() == "request-id-reused" => {
            return Err(LcError::conflict(
                "request_id_reused",
                json!({"bot_id": reused.into_inner().unwrap(), "detail": "same client_request_id, different request"}),
            ))
        }
        Err(e) if e.to_string() == "duplicate-name" => {
            return Err(LcError::conflict("bot name already in use", json!({"name": b.name})))
        }
        Err(e) if e.to_string() == "no-project" => return Err(LcError::NotFound("project".into())),
        Err(e) if is_unknown_identity(&e) => return Err(LcError::NotFound("identity".into())),
        Err(e) => return Err(projection_err(e)),
    }
    if let Some((bot_id, name)) = replayed.into_inner().unwrap() {
        // 重送：拿回原本那顆，什麼都沒新建、不推事件。
        let mut body = json!({"bot_id": bot_id, "name": name, "replayed": true});
        if let Some(value) = &remapped { body["remapped"] = value.clone(); }
        return Ok((StatusCode::OK, Json(body)).into_response());
    }
    app.emit("bot_changed", json!({"bot_id": id})).await;
    let name = used_name.into_inner().unwrap_or_default();
    let mut body = json!({"bot_id": id, "name": name});
    if let Some(value) = remapped { body["remapped"] = value; }
    Ok((StatusCode::OK, Json(body)).into_response())
}

/// 這個專案還活著的 bot 名，含 child（不在 config.toml 裡的那些）。
pub(crate) async fn live_bot_names(pool: &sqlx::SqlitePool, project_id: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT name FROM bots WHERE project_id = ? AND deleted_at IS NULL")
        .bind(project_id)
        .fetch_all(pool)
        .await
}

/// `cc1-1` → `cc1-2`; `review` → `review-2`. Trims the base to stay within 32 chars.
///
/// `valid_bot_name` allows any Unicode (only `chars().count() <= 32` is checked, not ASCII —
/// `小幫手` is a legal name), so the truncation has to be char-based like
/// `fork.rs::default_name` / `default_session.rs::imported_name`. A byte-based slice used to
/// panic ("byte index N is not a char boundary") whenever the cut landed inside a multi-byte
/// character (issue #112).
pub(crate) fn next_free_name(wanted: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let base = match wanted.rfind('-') {
        Some(i) if wanted[i + 1..].chars().all(|c| c.is_ascii_digit()) && i + 1 < wanted.len() => &wanted[..i],
        _ => wanted,
    };
    for n in 1u32.. {
        let suffix = format!("-{n}");
        let room = 32usize.saturating_sub(suffix.chars().count());
        let truncated: String = base.chars().take(room).collect();
        let candidate = format!("{truncated}{suffix}");
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
    /// 主力那列的順序（#344）：bot id 的陣列，位置寫進 `bots.primary_position`（不進 config.toml）；沒點名的維持原值。
    #[serde(default)]
    primary: Option<Vec<String>>,
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

/// 持久 intent 的最近紀錄（#355 P1：只讀，目前沒有路徑會寫它）。
async fn list_intents(State(app): State<Arc<App>>) -> Result<Response, LcError> {
    let rows = crate::intents::recent(&app.db, 100).await.map_err(any_err)?;
    Ok((StatusCode::OK, Json(json!({"intents": rows}))).into_response())
}

async fn set_order(State(app): State<Arc<App>>, Json(b): Json<SetOrder>) -> Result<Response, LcError> {
    if b.projects.is_none() && b.bots.is_none() && b.primary.is_none() {
        return Err(LcError::Bad("order: projects、bots 或 primary 至少要有一個".into()));
    }
    // 兩個 store 不可能同一個交易：`primary` 只存 DB、projects／bots 寫 config.toml；混送要嘛半套生效、要嘛回滾也可能失敗。
    // 一律 400、什麼都不寫，分成兩次送（#350）。
    if b.primary.is_some() && (b.projects.is_some() || b.bots.is_some()) {
        return Err(LcError::Bad("order: primary（只存 DB）不能跟 projects／bots（寫 config.toml）同一個請求送，請分成兩次".into()));
    }
    // 先驗再動：點名了不存在的 bot，config 那邊的順序也不寫。
    if let Some(ids) = &b.primary {
        crate::primary_order::validate(&app.db, ids).await?;
    }
    // 只有主力順序（`primary`）的請求不碰 config.toml：它只存 DB。
    if b.projects.is_some() || b.bots.is_some() {
        crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
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
        .map_err(projection_err)?;
    }
    if let Some(ids) = &b.primary {
        crate::primary_order::write(&app.db, ids).await?;
    }
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
    Json(mut b): Json<PatchBot>,
) -> Result<Response, LcError> {
    let active = db::active_run(&app.db, &id).await.map_err(any_err)?;
    #[cfg(test)]
    crate::lifecycle::race_point::hit("patch_after_active_snapshot", &id).await;
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
    let mut pre_patch_rev = None;
    // 改設定之前：active run 還沒記啟動版本就用「改之前」的補記，改完才看得出過期（#353）。
    // 這是設定提交的 admission prerequisite；任何讀取或補記不確定都不可以繼續寫 TOML／DB。
    if restart_relevant {
        if let Some(r) = &active {
            let before = match db::bot(&app.db, &id).await {
                Ok(Some(bot)) => bot,
                Ok(None) => return Err(LcError::NotFound("bot".into())),
                Err(e) => {
                    return Err(LcError::Unavailable(json!({
                        "reason": "launch_revision_baseline_unreadable",
                        "message": format!("configuration was not changed; retry PATCH: {e}"),
                        "retryable": true,
                        "config_unchanged": true,
                        "retry_after_secs": 5,
                    })));
                }
            };
            pre_patch_rev = Some(crate::launch_rev::of(&before));
            if let Err(e) = crate::launch_rev::stamp_if_missing(&app.db, r, &before).await {
                return Err(LcError::Unavailable(json!({
                    "reason": "launch_revision_baseline_failed",
                    "message": format!("configuration was not changed; retry PATCH: {e}"),
                    "retryable": true,
                    "config_unchanged": true,
                    "retry_after_secs": 5,
                })));
            }
        }
    }
    let needs_restart = active.is_some() && restart_relevant;
    let kind = db::bot(&app.db, &id).await.map_err(any_err)?.map(|x| x.kind).ok_or_else(|| LcError::NotFound("bot".into()))?;
    let (model, remapped) = match &b.model {
        Some(value) => {
            let (model, remapped) = remap_model(&kind, value.as_deref());
            b.model = Some(model.clone());
            (Some(model), remapped)
        }
        None => (None, None),
    };
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
    // `primary` 只存 DB、其他欄位寫 config.toml：兩個 store 不可能同一個交易，混送半套生效的風險比多送一次請求大得多（#350）。
    // config 裡的 bot（managed_by=user）混送一律 400、什麼都不寫；child bot 全在 DB，不受這條限制。
    if b.is_primary.is_some() && (restart_relevant || b.name.is_some() || b.autostart.is_some()) {
        let managed_by = db::bot(&app.db, &id).await.map_err(any_err)?.map(|x| x.managed_by).unwrap_or_default();
        if managed_by == "user" {
            return Err(LcError::Bad("primary（只存 DB）不能跟會寫 config.toml 的欄位（name／autostart／model／effort／args／identity／env…）同一個請求送，請分成兩次 PATCH".into()));
        }
    }
    // child bot 不在 config.toml，cfg.update 只會 404（2026-09-09 使用者：child 身分改不了）→ 直接改 DB。
    if let Some(pin) = b.is_primary {
        let n = crate::primary_order::set_pinned(&app.db, &id, pin).await.map_err(any_err)?;
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
    crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
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
        if b.model.is_some() {
            bot.model = model.clone().flatten();
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
    .map_err(|e| if e.to_string() == "no-bot" { LcError::NotFound("bot".into()) } else { projection_err(e) })?;
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
    let mut live_revisions = None;
    let live = if needs_restart && !live_fields.is_empty() {
        let target_rev = match db::bot(&app.db, &id).await {
            Ok(Some(bot)) => crate::launch_rev::of(&bot),
            Ok(None) => String::new(),
            Err(e) => {
                let mut out = json!({"needs_restart": true});
                if let Some(value) = remapped {
                    out["remapped"] = value;
                }
                out["live_apply"] = json!({
                    "fields": live_fields,
                    "applied": false,
                    "deferred": false,
                    "pending_bookkeeping": false,
                    "reason": format!("target_revision_unreadable: {e}"),
                });
                return Ok((StatusCode::OK, Json(out)).into_response());
            }
        };
        let baseline_rev = pre_patch_rev.clone().unwrap_or_else(|| target_rev.clone());
        live_revisions = Some((baseline_rev.clone(), target_rev.clone()));
        Some(
            lifecycle::apply_live_setting_with_revision(
                &app,
                &id,
                &live_fields,
                &baseline_rev,
                &target_rev,
            )
            .await,
        )
    } else {
        None
    };
    // codex 的 fast（也含 model／effort）忙的時候不重啟：記下來，下一次 idle 再套（#393，lifecycle/deferred_live.rs）。
    // claude／grok 也一樣（#712），但它們一次只能排一個欄位（`defer_live` 的 single_field）。
    let busy = matches!(&live, Some(lifecycle::LiveApplyOutcome::Failed(why)) if lifecycle::is_busy_reason(why));
    let deferred = busy
        && live_revisions.as_ref().is_some_and(|(baseline_rev, target_rev)| {
            lifecycle::defer_live(&id, &live_fields, baseline_rev, target_rev, kind != "codex")
        });
    if deferred {
        // 上面那則 bot_changed 送出時還沒排上：前端要再拉一次 state 才看得到 `live_apply_deferred`。
        app.emit("bot_changed", json!({"bot_id": id})).await;
    }
    let needs_restart = match &live {
        Some(lifecycle::LiveApplyOutcome::Applied { .. }) => match (
            db::bot(&app.db, &id).await,
            db::active_run(&app.db, &id).await,
        ) {
            (Ok(Some(bot)), Ok(Some(run))) => crate::launch_rev::is_stale(&bot, &run),
            _ => true,
        },
        Some(
            lifecycle::LiveApplyOutcome::BookkeepingPending { .. }
            | lifecycle::LiveApplyOutcome::Failed(_),
        ) => !deferred,
        // 沒走當場套用（persona／args 等）：跟 `/state` 同一個判斷（launch_rev），值沒變或改回載入的值就不必重啟。
        // 讀不到就保守說要重啟。
        None if needs_restart => match (db::bot(&app.db, &id).await, db::active_run(&app.db, &id).await) {
            (Ok(Some(bot)), Ok(Some(run))) => crate::launch_rev::is_stale(&bot, &run),
            _ => true,
        },
        None => false,
    };
    let mut out = json!({"needs_restart": needs_restart});
    if let Some(value) = remapped { out["remapped"] = value; }
    if let Some(outcome) = live {
        let (applied, pending_bookkeeping, reason) = match outcome {
            lifecycle::LiveApplyOutcome::Applied { .. } => (true, false, None),
            lifecycle::LiveApplyOutcome::BookkeepingPending { reason, .. } => {
                (false, true, Some(reason))
            }
            lifecycle::LiveApplyOutcome::Failed(why) => (false, false, Some(why)),
        };
        out["live_apply"] = json!({
            "fields": live_fields,
            "applied": applied,
            "deferred": deferred,
            "pending_bookkeeping": pending_bookkeeping,
            "reason": reason,
        });
    }
    Ok((StatusCode::OK, Json(out)).into_response())
}

async fn preview_get(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    Ok(Json(crate::preview::get(&app, &id).await?).into_response())
}
async fn preview_start(State(app): State<Arc<App>>, Path(id): Path<String>, body: Bytes) -> Result<Response, LcError> {
    // body 可有可無（沒帶＝auto）；帶了就要是合法的 JSON。
    let req: crate::preview::StartReq = if body.iter().all(u8::is_ascii_whitespace) {
        Default::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| LcError::Bad(format!("preview body: {e}")))?
    };
    Ok(Json(crate::preview::start(&app, &id, req).await?).into_response())
}
async fn preview_stop(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    Ok(Json(crate::preview::stop(&app, &id).await?).into_response())
}

pub(crate) async fn delete_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    // 整個刪除都拿著這顆 bot 與它所有 child 的 per-bot 鎖：start 要同一把，等它拿到時 bot 已經刪了
    // （NotFound），不會在「定案」和「停機」之間插進來重開一個 run。
    // 鎖要跟 delete_project 一樣**依 id 排序一次拿齊**：先拿 parent、之後再逐顆拿 child 的話，child id
    // 排在 parent 前面時會跟 delete_project 互等成死鎖（ULID 不保證 parent 比較小；sol 五輪）。
    // 2026-09-08: spawned children go with it, else they're sidebar orphans. Deepest first.
    let mut attempt = 0;
    let (children, _guards) = loop {
        let before: Vec<db::Bot> = descendant_children(&app, &id).await.map_err(any_err)?;
        let ids = std::iter::once(id.clone()).chain(before.iter().map(|c| c.id.clone())).collect();
        let (_, guards) = lock_bots_in_order(&app, ids).await;
        // 拿鎖的途中又認領了新的 child：它沒被鎖住，放掉重來（不能在持鎖時再補拿，那就又亂了順序）。
        let now: Vec<db::Bot> = descendant_children(&app, &id).await.map_err(any_err)?;
        if now.iter().map(|c| &c.id).eq(before.iter().map(|c| &c.id)) {
            break (now, guards);
        }
        drop(guards);
        attempt += 1;
        if attempt >= 3 {
            return Err(LcError::conflict("children_changed", json!({"bot_id": id})));
        }
    };
    let children: Vec<db::Bot> = children.into_iter().rev().collect();
    let bot = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    let host = db::bot_host(&app.db, &id).await.map_err(any_err)?;
    // 定案之前先確定讀得到每一顆的 run 狀態：讀不到就 502、什麼都不動，可以原樣再按一次。定案之後才發現讀不到，
    // 已經是「軟刪了、run 不知道還在不在」，只能保住目錄（#210）。
    for b in std::iter::once(&bot).chain(children.iter()) {
        db::active_run(&app.db, &b.id).await.map_err(any_err)?;
    }
    // 先定案、再停機（sol 四輪）：會 409 的只有這一步，這時什麼都還沒停；定案之後沒有會失敗回頭的步驟，
    // 所以不會留下「已停、未刪」。（child 由母 agent 開，daemon 本來就重開不了它，事後回滾做不到。）
    // 持久 intent（#355 P3）：定案**之前**先 commit（payload＝當時的 child 快照，深的先）；定案之後 daemon 死掉，開機由 `delete_intents::recover_host` 補完。
    let snapshot: Vec<Value> = children.iter().map(|c| json!({"id": c.id, "managed_by": c.managed_by})).collect();
    // 誰按的刪除：留在 log 與 intent（DB）裡，事後查得到（issue #406：13:28Z 那兩筆就是查不到）。
    let requested_by = crate::config_audit::http_caller();
    tracing::warn!(bot = %bot.name, bot_id = %id, http = %requested_by, "bot delete requested");
    let intent =
        crate::delete_intents::begin(&app, "delete_bot", &id, &host, &json!({"bots": snapshot, "requested_by": requested_by})).await?;
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_after_intent", &id).await;
    let decided: Result<(), LcError> = if bot.managed_by == "child" {
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&id).execute(&app.db).await.map(|_| ()).map_err(any_err)
    } else {
        delete_in_config(&app, crate::projection::DeleteTarget::Bot(&id)).await.map(|_| ())
    };
    if let Err(e) = decided {
        crate::delete_intents::abandon(&app, &intent, "the delete was refused").await;
        return Err(e);
    }
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_bot_after_decided", &id).await;
    // 輸入框草稿跟著 bot 走（含一併刪掉的 child）。
    let draft_keys: Vec<String> = std::iter::once(&id).chain(children.iter().map(|c| &c.id)).map(|b| format!("bot:{b}")).collect();
    crate::drafts::clear_keys(&app, &draft_keys).await;
    let mut child_retry = false;
    // 目錄只在「確定沒有 active run」時才 purge（#210）；不確定的留著，列在回應的 `kept_dirs`，下次開機的
    // `purge_deleted_bot_dirs` 在 run 確定結束之後再收。
    let mut removed_children = Vec::new();
    let mut kept_dirs: Vec<Value> = Vec::new();
    for child in children {
        let settled = stop_for_delete_locked(&app, &child.id).await;
        // 定案之後不能因為某顆 child 的軟刪寫不進去就 early return（#296）：後面的 child 與母 bot 都還沒停，母 bot 已軟刪、
        // 沒人會再處理它的 run。失敗就記成保留目錄、背景重試，其餘照做。
        let settled = match soft_delete_child(&app, &child.id).await {
            Ok(()) => settled,
            Err(e) => {
                tracing::error!(bot = %child.name, error = ?e, "child could not be soft-deleted; the delete intent stays open and retries in the background");
                child_retry = true;
                Err("soft delete failed; retrying in the background")
            }
        };
        match settled {
            Ok(()) => {
                let child_host = db::bot_host(&app.db, &child.id).await.unwrap_or_else(|_| host.clone());
                lifecycle::purge_bot_dir(&app, &child.id, &child_host).await;
            }
            Err(why) => kept_dirs.push(json!({"bot_id": child.id, "reason": why})),
        }
        app.emit("bot_changed", json!({"bot_id": child.id})).await;
        removed_children.push(child.id);
    }
    // 已經拿著全部的鎖：一律用 locked 版（stop_bot 會再拿同一把而卡死）。
    match stop_for_delete_locked(&app, &id).await {
        // Soft delete; the conversation and its messages stay.
        Ok(()) => {
            lifecycle::purge_bot_dir(&app, &id, &host).await;
        }
        Err(why) => kept_dirs.push(json!({"bot_id": id, "reason": why})),
    }
    app.emit("bot_changed", json!({"bot_id": id})).await;
    if bot.managed_by == "child" {
        app.emit("project_changed", json!({"project_id": bot.project_id})).await;
    }
    // intent：有 child 軟刪沒寫成就留著、背景補完（含清目錄）；否則收成 done。
    if child_retry {
        crate::delete_intents::spawn_retry(app.clone(), intent.clone(), "some child bots could not be soft-deleted".into());
    } else {
        crate::delete_intents::complete(&app, &intent).await;
    }
    let mut out = json!({"removed_children": removed_children});
    if !kept_dirs.is_empty() {
        out["kept_dirs"] = json!(kept_dirs);
    }
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// If the host is down the stop fails; end the run anyway, else no reconcile ever ends it and
/// `purge_deleted_bot_dirs` waits forever (review 2026-09-12 d). The orphan-pane sweep reclaims the pane later.
/// 多顆 bot 的 per-bot 鎖一律**依 id 排序、一次拿齊**（刪除是唯一會同時持多把的路徑；其他路徑一次只拿一把）。
/// 回傳排序去重後的 id 與鎖。
pub(crate) async fn lock_bots_in_order(
    app: &Arc<App>,
    mut ids: Vec<String>,
) -> (Vec<String>, Vec<tokio::sync::OwnedMutexGuard<()>>) {
    ids.sort();
    ids.dedup();
    let mut guards = Vec::with_capacity(ids.len());
    for bot_id in &ids {
        guards.push(app.bot_lock(bot_id).await.lock_owned().await);
    }
    (ids, guards)
}

/// 刪除前的停機。`Ok(())`＝停好了、而且**讀得到**現在沒有 active run，這顆的目錄可以 purge；`Err(reason)`＝不確定，
/// 目錄要留著（#210，跟 `purge_deleted_bot_dirs` 同一條原則：只有確定才有權刪）：
/// - `stop_not_confirmed`：停機失敗（主機連不上、agent 沒退出…）。run 照舊強制收成 `exited`（已軟刪的 bot 不進對帳，
///   不收就沒有人收），但 agent 可能還活著——目錄留給下次開機的清掃；
/// - `run_state_unreadable`：最後那次讀不到 active run（停機失敗時連強制收掉都做不了；run 可能還在跑）；
/// - `run_still_active`：停完、收完再讀，run 還是 active（終態寫不進去）。
///
/// 唯一的證明是最後那一次讀到 `Ok(None)`。
pub(crate) async fn stop_for_delete_locked(app: &Arc<App>, bot_id: &str) -> Result<(), &'static str> {
    let mut forced = false;
    if let Err(e) = lifecycle::stop_bot_locked(app, bot_id).await {
        tracing::warn!(bot = %bot_id, error = ?e, "could not stop the bot while deleting it; ending its run");
        // 讀不到就收不掉：往下由最後那一次讀取判定（讀不到＝不確定，目錄留著）。
        if let Ok(Some(run)) = db::active_run(&app.db, bot_id).await {
            lifecycle::mark_run_exited(app, &run.id, "the bot was deleted while its host was unreachable").await;
            forced = true;
        }
    }
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_stop_before_proof", bot_id).await;
    match db::active_run(&app.db, bot_id).await {
        Ok(None) if forced => {
            tracing::warn!(bot = %bot_id, "the stop was not confirmed (the run was ended anyway); keeping the bot's directory");
            Err("stop_not_confirmed")
        }
        Ok(None) => Ok(()),
        Ok(Some(run)) => {
            tracing::warn!(bot = %bot_id, run = %run.id, "the bot's run is still active after the stop; keeping its directory");
            Err("run_still_active")
        }
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = %e, "could not confirm the bot has no live run; keeping its directory");
            Err("run_state_unreadable")
        }
    }
}

/// Parents before children, so `.rev()` deletes deepest first.
pub(crate) async fn descendant_children(app: &Arc<App>, root: &str) -> anyhow::Result<Vec<db::Bot>> {
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
pub(crate) async fn restore_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    // 跟 delete_bot／start 同一把 per-bot 鎖（#301）：delete_bot 定案後才停機、purge，沒鎖的話還原會插進來，
    // 事後被 purge 砍掉剛還原的 bot 的目錄與 run。
    let _guard = app.bot_lock(&id).await.lock_owned().await;
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
        // child 的專案或母 bot 已經刪了：還原只會得到一顆誰都看不到的活 bot（#298）。
        let project_live = db::project(&app.db, &bot.project_id).await.map_err(any_err)?.is_some_and(|p| p.deleted_at.is_none());
        let parent_live = match &bot.parent_bot_id {
            Some(pid) => db::bot(&app.db, pid).await.map_err(any_err)?.is_some_and(|p| p.deleted_at.is_none()),
            None => true,
        };
        if !project_live || !parent_live {
            return Err(LcError::conflict("the project or parent bot of this child is deleted", json!({"bot_id": id})));
        }
        // A child restored by AGM is reopened by its parent in the existing pane; allow ten minutes
        // for that asynchronous hand-off before reconcile applies the normal retirement rule.
        crate::child_reconcile_safety::record_retirement_grace(&app.db, &id).await.map_err(any_err)?;
        sqlx::query("UPDATE bots SET deleted_at = NULL WHERE id = ?").bind(&id).execute(&app.db).await.map_err(any_err)?;
    } else {
        // 讀不懂的 args／env 不能當成空的寫回 config（#295）：env 可能帶帳號設定，還原後會以錯的身分起。
        let args = serde_json::from_str(&bot.args_json).map_err(|e| LcError::conflict("bot args_json is unreadable; not restoring", json!({"bot_id": id, "error": e.to_string()})))?;
        let env = serde_json::from_str(&bot.env_json).map_err(|e| LcError::conflict("bot env_json is unreadable; not restoring", json!({"bot_id": id, "error": e.to_string()})))?;
        let entry = crate::config::BotCfg {
            id: Some(bot.id.clone()),
            name: bot.name.clone(),
            kind: bot.kind.clone(),
            model: bot.model.clone(),
            effort: bot.effort.clone(),
            fast: bot.fast != 0,
            persona: bot.persona.clone(),
            args,
            autostart: bot.autostart != 0,
            inject_hooks: bot.inject_hooks != 0,
            auto_approve: bot.auto_approve != 0,
            identity: bot.identity.clone(),
            env,
            herdr_session: bot.herdr_session.clone(),
            create_request_id: None,
            create_fingerprint: None,
        };
        let pid = bot.project_id.clone();
        crate::projection::update_and_project(&app.cfg, &app.db, move |cfg| {
            let Some(p) = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())) else {
                anyhow::bail!("the project this bot belonged to is gone")
            };
            if !p.bots.iter().any(|b| b.id.as_deref() == Some(entry.id.as_deref().unwrap_or_default())) {
                p.bots.push(entry.clone());
            }
            Ok(())
        })
        .await
        .map_err(projection_err)?;
    }
    // 刪除時搬進回收區的目錄搬回來（issue #406）。本機才有；搬不回來不擋還原（下次啟動會重建需要的檔）。
    if let Ok(dir) = app.bot_dir(&id) {
        match crate::bot_trash::restore(&app.data_dir, &id, &dir) {
            Ok(Some(from)) => tracing::info!(bot = %id, from = %from.display(), "restored bot config dir from bots-trash"),
            Ok(None) => {}
            Err(e) => tracing::warn!(bot = %id, error = %e, "could not restore bot config dir from bots-trash"),
        }
    }
    // 附件副本也是刪除時一起收進回收區的（#465）：不搬回來的話還原後對話還在、縮圖全破。
    {
        let att = crate::bot_trash::attachments_dir(&app.data_dir, &id);
        match crate::bot_trash::restore_kind(&app.data_dir, &id, Some(crate::bot_trash::ATTACHMENTS), &att) {
            Ok(Some(from)) => tracing::info!(bot = %id, from = %from.display(), "restored bot attachments from bots-trash"),
            Ok(None) => {}
            Err(e) => tracing::warn!(bot = %id, error = %e, "could not restore bot attachments from bots-trash"),
        }
    }
    crate::remote_trash::restore_for(&app, &id).await;
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
    /// #709：不帶＝沿用既有那一筆的值（新主機是 false）。
    shared_session: Option<bool>,
}

/// 這次更新會不會改變「這個名字指到哪台機器」。`remote_path`／`ssh_opts` 只影響在同一台上怎麼跑，
/// 改它們不會把既有的 run 接到別台去，所以不擋（issue #544）。
fn repoints_host(old: &HostCfg, new: &HostCfg) -> bool {
    old.ssh != new.ssh || old.ssh_port != new.ssh_port || old.herdr_session != new.herdr_session
}

/// 主機上還活著的專案。`delete_host` 用同一條判斷（`api.rs` 的 `host still used by projects`）。
async fn live_projects_on_host(app: &Arc<App>, host: &str) -> Result<Vec<db::Project>, LcError> {
    Ok(db::live_projects(&app.db).await.map_err(any_err)?.into_iter().filter(|p| p.host == host).collect())
}

async fn create_host(
    State(app): State<Arc<App>>,
    Query(q): Query<DeleteQuery>,
    Json(b): Json<NewHost>,
) -> Result<Response, LcError> {
    if b.name == LOCAL_HOST {
        return Err(LcError::Bad("`local` is reserved for this machine".into()));
    }
    if !valid_host_name(&b.name) {
        return Err(LcError::Bad(format!("host name must match {}", crate::config::SLUG_NAME_RE)));
    }
    if b.ssh.trim().is_empty() {
        return Err(LcError::Bad("ssh target must not be empty".into()));
    }
    let session_wanted = b.herdr_session.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(crate::config::DEFAULT_HERDR_SESSION);
    if let Some(why) = crate::config::host_target_problem(b.ssh.trim(), session_wanted) {
        return Err(LcError::Bad(why));
    }
    if let Some(why) = crate::config::ssh_opts_problem(b.ssh_opts.as_deref().unwrap_or_default()) {
        return Err(LcError::Bad(why));
    }
    // 共用 session 是安全開關（#709）：沒帶就沿用，不因為一次只改 ssh 的更新而悄悄關掉。
    let prev_shared = app.cfg.get().await.hosts.iter().any(|h| h.name == b.name && h.shared_session);
    let cfg = HostCfg {
        name: b.name.clone(),
        ssh: b.ssh.trim().to_string(),
        ssh_port: b.ssh_port.unwrap_or(22),
        ssh_opts: b.ssh_opts.unwrap_or_default(),
        // trim 跟 `ssh` 同一條規矩：存進 config 的字就是之後直接交給 herdr 的字。
        herdr_session: b
            .herdr_session
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| crate::config::DEFAULT_HERDR_SESSION.to_string()),
        remote_path: b.remote_path.unwrap_or_default(),
        shared_session: b.shared_session.unwrap_or(prev_shared),
    };
    // 這支同時是新增與**更新**（docs/API.md）。更新到「指去另一台機器」時要跟 `delete_host` 一樣先確認
    // 主機上沒有活著的專案（issue #544）：`apply_config` 會把連線整個換掉，但 `runs` 一列都不動——
    // 那些 run 還帶著**舊那台**開出來的 pane id，接下來 daemon 會拿它們去問新那台。輕則 pane 不存在、
    // run 被判成不見了收掉；重則新機器上剛好有同樣的 pane id（herdr 的 id 是每個實例自己編的），
    // prompt 與按鍵就送進一顆完全不相干的 pane。刪掉只是「連線沒了」，改掉是「連線還在、但接到別台」。
    let existing = app.cfg.get().await.hosts.into_iter().find(|h| h.name == cfg.name);
    if let Some(old) = existing.filter(|old| repoints_host(old, &cfg)) {
        let live = live_projects_on_host(&app, &b.name).await?;
        if !live.is_empty() && q.confirm.as_deref() != Some("repoint") {
            return Err(LcError::conflict(
                "host still used by projects",
                json!({
                    "reason": "host_repoint_in_use",
                    "host": b.name,
                    "from": {"ssh": old.ssh, "ssh_port": old.ssh_port, "herdr_session": old.herdr_session},
                    "to": {"ssh": cfg.ssh, "ssh_port": cfg.ssh_port, "herdr_session": cfg.herdr_session},
                    "projects": live.iter().map(|p| json!({"id": p.id, "label": p.label})).collect::<Vec<_>>(),
                    "hint": "先把這些專案移走或刪掉；真的要改就帶 ?confirm=repoint（既有的 run 會留著舊機器的 pane id）",
                }),
            ));
        }
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
    let fence = app.hosts.fence(&name).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    let ht = crate::tools::detect_with_fence(&app, &name, &fence).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    crate::state::emit_host_changed(&app, &fence).await;
    Ok((
        StatusCode::OK,
        Json(json!({
            "name": name,
            "tools": ht.tools,
            "identities": ht.identities,
            "shell_identities": ht.shell_identities,
            "tools_checked_at": ht.checked_at,
            "baseline": app.host_baseline.lock().await.get(&name),
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
    identity_auth(app, name, identity, false).await
}

/// 登出走同一條路：開臨時 pane、帶同一組環境變數下指令、等 CLI 結束再重驗登入狀態。
/// 分開的只有指令本身——共用一條才不會有一邊忘了帶 `CLAUDE_CONFIG_DIR` 而動到別的帳號。
async fn logout_identity(
    State(app): State<Arc<App>>,
    Path((name, identity)): Path<(String, String)>,
) -> Result<Response, LcError> {
    identity_auth(app, name, identity, true).await
}

async fn identity_auth(app: Arc<App>, name: String, identity: String, logout: bool) -> Result<Response, LcError> {
    // 沒寫 host 的身分在遠端也生效：未知主機若先查 identity／PATH，會變成 409「CLI 不在 PATH」。
    // 開 pane 之前記下主機權威，watcher 只對這一條連線收尾（#347）。
    let Some(fence) = app.hosts.fence(&name).await else {
        return Err(LcError::NotFound("host".into()));
    };
    let idn = crate::tools::identity_for_host(&app, &name, &identity)
        .await
        .ok_or_else(|| LcError::NotFound("identity".into()))?;
    let Some(_) = crate::tools::cached_path(&app, &name, &idn.kind).await else {
        return Err(LcError::conflict(
            "identity_login_unavailable",
            json!({"host": name, "identity": identity, "kind": idn.kind, "message": "CLI 不在 PATH，無法登入"}),
        ));
    };
    let home = crate::hosts::home_for_fence(&fence).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let env = idn
        .env
        .iter()
        .filter(|(k, _)| crate::tools::valid_env_name(k))
        .map(|(k, v)| (k.clone(), crate::config::expand_home(v, &home)))
        .collect::<BTreeMap<_, _>>();
    let command = if logout {
        crate::tools::identity_logout_command(&idn.kind, &env)
            .ok_or_else(|| LcError::Bad(format!("kind {} 沒有登出指令", idn.kind)))?
    } else {
        crate::tools::identity_login_command(&idn.kind, &env)
            .ok_or_else(|| LcError::Bad(format!("kind {} 沒有登入指令", idn.kind)))?
    };
    let opened = app.hosts.run_if_current(&fence, async {
        let shell = shell::open(&app, &name, None).await?;
        if let Err(e) = shell::send_text(&app, &name, &shell.pane_id, &command, true).await {
            let _ = shell::close(&app, &name, &shell.pane_id).await;
            return Err(e);
        }
        Ok::<_, LcError>(shell)
    }).await;
    let shell = opened
        .ok_or_else(|| LcError::Upstream(format!("host `{name}` changed before identity authentication started")))??;
    crate::tools::spawn_identity_login_watch(app, name, shell.pane_id.clone(), identity, idn.kind, logout, fence);
    Ok((StatusCode::OK, Json(json!(shell))).into_response())
}

#[cfg(test)]
mod identity_auth_error_tests {
    use super::*;
    use axum::response::IntoResponse;

    async fn body_of(err: LcError) -> (StatusCode, Value) {
        let resp = err.into_response();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn with_identity(app: &Arc<App>, name: &str, kind: &str) {
        let ident = crate::config::IdentityCfg {
            name: name.into(),
            kind: kind.into(),
            host: None,
            env: Default::default(),
            args: vec![],
        };
        app.cfg
            .update(move |cfg| {
                cfg.identities.push(ident);
                Ok(())
            })
            .await
            .unwrap();
    }

    /// API.md：CLI 不在 PATH → `409 {"reason":"identity_login_unavailable"}`。
    /// extra 裡不能再寫 `reason`，否則會蓋掉機器 key，前端對不到這條。
    #[tokio::test]
    async fn missing_cli_is_409_identity_login_unavailable() {
        let e = crate::testing::env().await;
        with_identity(&e.app, "cc1", "claude").await;
        let err = login_identity(State(e.app.clone()), Path(("local".into(), "cc1".into())))
            .await
            .expect_err("no CLI on PATH must not open a pane");
        let (status, body) = body_of(err).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "conflict");
        assert_eq!(body["reason"], "identity_login_unavailable", "{body}");
        assert_eq!(body["identity"], "cc1");
        assert_eq!(body["kind"], "claude");
    }

    #[tokio::test]
    async fn identity_auth_stops_before_opening_a_shell_when_remote_home_is_unreadable() {
        use std::sync::atomic::Ordering;

        let e = crate::testing::env().await;
        let host = format!("identity-api-home-616-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = e.app.hosts.insert_remote_for_test(HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        conn.connected.store(true, Ordering::SeqCst);
        e.app.cfg.update(|cfg| {
            cfg.identities.push(crate::config::IdentityCfg {
                name: "cx1".into(),
                kind: "codex".into(),
                host: Some(host.clone()),
                env: [("CODEX_HOME".into(), "~/.codex-cx1".into())].into(),
                args: vec![],
            });
            Ok(())
        }).await.unwrap();
        e.app.tools.lock().await.insert(host.clone(), crate::tools::HostTools {
            tools: [("codex".into(), crate::tools::ToolInfo { installed: true, path: Some("/usr/bin/codex".into()), version: None, logged_in: Some(true) })].into(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        });
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Err(anyhow::anyhow!("injected remote HOME read failure"))
        });

        let err = identity_auth(e.app.clone(), host.clone(), "cx1".into(), false).await.expect_err("unknown HOME must not start identity authentication");
        assert!(matches!(err, LcError::Upstream(ref message) if message.contains("HOME")), "return a retryable upstream failure: {err:?}");
        assert_eq!(calls.lock().unwrap().len(), 1, "only HOME resolution may run; no auth pane command is sent");
        assert!(e.app.host_shells.lock().await.iter().all(|shell| shell.host != host), "no shell is opened on an unrelated path");
    }

    /// issue #544：`POST /api/hosts` 同時是新增與更新。把 `ssh` 改成指到**另一台機器**時，
    /// 要跟 `delete_host` 一樣先確認主機上沒有活著的專案——`apply_config` 換掉連線但不動 `runs`，
    /// 那些 run 還帶著舊機器的 pane id。只改 `remote_path`／`ssh_opts` 不會換機器，照樣放行。
    #[tokio::test]
    async fn repointing_a_host_with_live_projects_is_refused_but_cosmetic_edits_are_not() {
        let e = crate::testing::env().await;
        let host = NewHost {
            name: "zz92".into(),
            ssh: "old-box".into(),
            ssh_port: None,
            ssh_opts: None,
            herdr_session: None,
            remote_path: None,
            shared_session: None,
        };
        let mk = |h: &NewHost| NewHost {
            name: h.name.clone(),
            ssh: h.ssh.clone(),
            ssh_port: h.ssh_port,
            ssh_opts: h.ssh_opts.clone(),
            herdr_session: h.herdr_session.clone(),
            remote_path: h.remote_path.clone(),
            shared_session: h.shared_session,
        };
        // 先把主機寫進 config（不必等它真的連上：這條測的是閘門，不是連線）。
        e.app.cfg
            .update(move |f| {
                f.hosts.push(HostCfg {
                    shared_session: false,
                    name: "zz92".into(),
                    ssh: "old-box".into(),
                    ssh_port: 22,
                    ssh_opts: vec![],
                    herdr_session: "agents-manager".into(),
                    remote_path: String::new(),
                });
                Ok(())
            })
            .await
            .unwrap();
        // 這台上放一個活著的專案。
        crate::projection::update_and_project(&e.app.cfg, &e.app.db, |cfg| {
            cfg.projects.push(crate::config::ProjectCfg {
                handed_off_to: None,
                id: Some("01PROJZZ92".into()),
                path: "/srv/work".into(),
                label: "work".into(),
                host: "zz92".into(),
                bots: vec![],
            });
            Ok(())
        })
        .await
        .unwrap();

        // 1. 改 ssh（換機器）→ 409，而且什麼都沒寫進去。
        let mut repoint = mk(&host);
        repoint.ssh = "new-box".into();
        let err = create_host(State(e.app.clone()), Query(DeleteQuery::default()), Json(repoint))
            .await
            .expect_err("repointing a host in use must be refused");
        let (status, body) = body_of(err).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["reason"], "host_repoint_in_use", "{body}");
        assert_eq!(body["projects"][0]["id"], "01PROJZZ92", "{body}");
        let after = e.app.cfg.get().await.hosts.into_iter().find(|h| h.name == "zz92").unwrap();
        assert_eq!(after.ssh, "old-box", "被擋下來就不能改到 config：{body}");

        // 2. 只改不會換機器的欄位（remote_path）→ 放行。
        let mut cosmetic = mk(&host);
        cosmetic.remote_path = Some("/opt/bin".into());
        create_host(State(e.app.clone()), Query(DeleteQuery::default()), Json(cosmetic))
            .await
            .expect("cosmetic edits are not a repoint");
        let after = e.app.cfg.get().await.hosts.into_iter().find(|h| h.name == "zz92").unwrap();
        assert_eq!((after.ssh.as_str(), after.remote_path.as_str()), ("old-box", "/opt/bin"));

        // 3. 明確確認就放行（沿用 ?confirm= 的先例）。
        let mut forced = mk(&host);
        forced.ssh = "new-box".into();
        create_host(State(e.app.clone()), Query(DeleteQuery { confirm: Some("repoint".into()) }), Json(forced))
            .await
            .expect("?confirm=repoint is the documented escape hatch");
        let after = e.app.cfg.get().await.hosts.into_iter().find(|h| h.name == "zz92").unwrap();
        assert_eq!(after.ssh, "new-box");
    }

    /// API.md：host 不存在是 404。沒寫 host 的身分在遠端也生效，所以找不到主機時不能先走到
    /// 「CLI 不在 PATH」那條 409——呼叫端會去裝 CLI，其實主機根本沒這台。
    #[tokio::test]
    async fn unknown_host_is_404_host_not_cli_missing() {
        let e = crate::testing::env().await;
        with_identity(&e.app, "work", "codex").await;
        let err = login_identity(State(e.app.clone()), Path(("no-such-host".into(), "work".into())))
            .await
            .expect_err("unknown host");
        let (status, body) = body_of(err).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"], "not_found");
        assert_eq!(body["what"], "host", "{body}");
    }
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

/// 記憶體清單或 `panes` 表認得的才關；兩邊都沒有 404。走 `panes` 表的那條照 `?confirm=` 決定要不要先 409（`shell::close_confirmed`）。
async fn close_host_shell(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, LcError> {
    shell::close_confirmed(&app, &name, &pane_id, flag(&q.get("confirm").cloned())).await?;
    crate::drafts::clear_shell(&app, &name, &pane_id).await;
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
/// 未知 kind／host 也走這條（不是 400／404）：確認框必須能顯示原因，不能讓 HTTP 層失敗。
async fn get_changelog(State(app): State<Arc<App>>, Query(q): Query<ChangelogQuery>) -> Result<Json<Value>, LcError> {
    let kind = q.kind.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "claude".to_string());
    let host = q.host.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    let from = q.from.as_deref().filter(|s| !s.trim().is_empty());
    let to = q.to.as_deref().filter(|s| !s.trim().is_empty());
    let r = crate::changelog::lookup(&app, &host, &kind, from, to).await;
    Ok(Json(serde_json::to_value(r).map_err(any_err)?))
}

#[cfg(test)]
mod changelog_route_tests {
    use super::*;
    use axum::response::IntoResponse;

    async fn call(kind: Option<&str>, host: Option<&str>) -> (StatusCode, Value) {
        let e = crate::testing::env().await;
        let q = ChangelogQuery {
            kind: kind.map(str::to_string),
            host: host.map(str::to_string),
            from: None,
            to: None,
        };
        match get_changelog(State(e.app.clone()), Query(q)).await {
            Ok(Json(v)) => (StatusCode::OK, v),
            Err(err) => {
                let resp = err.into_response();
                let status = resp.status();
                let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
                (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
            }
        }
    }

    /// API.md：changelog **永遠 200**；沒有來源的 kind 是 `found:false`，不是 400。
    #[tokio::test]
    async fn an_unknown_kind_is_200_found_false_not_400() {
        let (status, body) = call(Some("nope"), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["found"], false);
        assert!(body["error"].as_str().is_some_and(|s| !s.is_empty()), "{body}");
    }

    /// API.md：host 不存在也是 200 `found:false`，不是 404——UI 必須能寫「找不到 changelog」。
    /// 而且講的是**這台 host 不認得**，不能退回去問本機：只看 `found:false`＋有 error 的話，改成退回本機也照綠
    /// （測試環境的本機 claude 一樣讀不到版本，#220 的測試原本就是這樣沒釘住）。
    #[tokio::test]
    async fn an_unknown_host_is_200_found_false_not_404() {
        let (status, body) = call(Some("claude"), Some("no-such-host")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["found"], false);
        assert_eq!(body["host"], "no-such-host", "回的是問的那台，不是 local：{body}");
        assert!(body["installed_version"].is_null(), "沒去問任何一台的版本：{body}");
        let err = body["error"].as_str().unwrap_or_default();
        assert!(err.contains("unknown host") && err.contains("no-such-host"), "錯誤要講是這台 host 不認得：{body}");
    }
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
                    handed_off_to: None,
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
            Json(PatchProject { label: Some("  改過的名字  ".into()), handed_off_to: None }),
        )
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let p = db::project(&app.db, &pid).await.unwrap().unwrap();
        assert_eq!(p.label, "改過的名字", "the label is trimmed and projected into the db");
        assert!(app.cfg.get().await.projects.iter().any(|x| x.label == "改過的名字"), "and written to config.toml");

        let err = patch_project(State(app.clone()), Path(pid.clone()), Json(PatchProject { label: Some("   ".into()), handed_off_to: None }))
            .await
            .unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "blank label is a 400, got {err:?}");
        assert_eq!(db::project(&app.db, &pid).await.unwrap().unwrap().label, "改過的名字");

        let err = patch_project(State(app), Path("nope".into()), Json(PatchProject { label: Some("x".into()), handed_off_to: None }))
            .await
            .unwrap_err();
        assert!(matches!(err, LcError::NotFound(_)), "unknown project is a 404, got {err:?}");
    }

    /// #709：`GET /api/hosts` 的 `shared_session` 讀當下的設定；`POST /api/hosts` 沒帶這個欄位就沿用，不悄悄關掉。
    #[tokio::test]
    async fn the_shared_session_flag_is_listed_live_and_kept_when_a_host_update_omits_it() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        crate::shared_host::tests::shared_host(&e, true).await;
        let flag = |app: Arc<App>| async move {
            hosts_list(&app).await.into_iter().find(|h| h["name"] == "sh1").unwrap()["shared_session"].clone()
        };
        assert_eq!(flag(app.clone()).await, json!(true));

        let update = |shared: Option<bool>| NewHost {
            name: "sh1".into(),
            ssh: "sh1.invalid".into(),
            ssh_port: None,
            ssh_opts: None,
            herdr_session: Some("test".into()),
            remote_path: None,
            shared_session: shared,
        };
        create_host(State(app.clone()), Query(DeleteQuery::default()), Json(update(None))).await.unwrap();
        assert!(app.cfg.get().await.hosts.iter().any(|h| h.name == "sh1" && h.shared_session), "沒帶就沿用");
        create_host(State(app.clone()), Query(DeleteQuery::default()), Json(update(Some(false)))).await.unwrap();
        assert_eq!(flag(app.clone()).await, json!(false));
    }

    /// #708：`handed_off_to` 設得上、清得掉，寫進 config.toml、投影進 DB、`GET /api/state` 看得到；空字串與非字串是 400。
    #[tokio::test]
    async fn patch_hands_a_project_off_and_takes_it_back() {
        let e = crate::testing::env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let path = e.repo.to_string_lossy().to_string();
        app.cfg
            .update(move |cfg| {
                cfg.projects.push(crate::config::ProjectCfg {
                    id: Some(pid.clone()),
                    path,
                    label: "proj".into(),
                    host: "local".into(),
                    bots: vec![],
                    handed_off_to: None,
                });
                Ok(())
            })
            .await
            .unwrap();
        let pid = e.project_id.clone();
        let patch = |v: Value| PatchProject { label: None, handed_off_to: Some(v) };
        let state_flag = |app: Arc<App>| async move {
            let s = state_json(&app).await.unwrap();
            s["projects"].as_array().unwrap()[0]["handed_off_to"].clone()
        };

        patch_project(State(app.clone()), Path(pid.clone()), Json(patch(json!(" agm-host ")))).await.unwrap();
        assert_eq!(db::project(&app.db, &pid).await.unwrap().unwrap().handed_off_to.as_deref(), Some("agm-host"));
        assert_eq!(app.cfg.get().await.projects[0].handed_off_to.as_deref(), Some("agm-host"), "written to config.toml");
        assert_eq!(state_flag(app.clone()).await, json!("agm-host"));

        for bad in [json!("  "), json!(3)] {
            let err = patch_project(State(app.clone()), Path(pid.clone()), Json(patch(bad))).await.unwrap_err();
            assert!(matches!(err, LcError::Bad(_)), "got {err:?}");
        }
        patch_project(State(app.clone()), Path(pid.clone()), Json(PatchProject { label: Some("renamed".into()), handed_off_to: None }))
            .await
            .unwrap();
        assert_eq!(state_flag(app.clone()).await, json!("agm-host"), "a label-only patch leaves the handoff alone");

        patch_project(State(app.clone()), Path(pid.clone()), Json(patch(Value::Null))).await.unwrap();
        assert!(db::project(&app.db, &pid).await.unwrap().unwrap().handed_off_to.is_none());
        assert!(app.cfg.get().await.projects[0].handed_off_to.is_none());
        assert_eq!(state_flag(app.clone()).await, Value::Null);
    }

    /// issue #73 reopen：需要 DB 才判得出來的大量軟刪閘門，現在也在**落盤之前**擋下非刪除的 mutation。
    ///
    /// 情境是 config.toml 被外部改掉（少了 3 顆 bot，DB 還沒被投影追上——事故路徑，不是使用者剛刪完），
    /// 這時 `patch_project` 只是改個 label、完全不碰 bot 列表。以前的順序是：`app.cfg.update` 先把
    /// （已經跟 DB 對不上的）整份 config 落盤，`reproject` 投影當下才被閘門擋下，回 `config_written: true`——
    /// TOML 已經被「合法化」寫入了。統一 commit boundary（`projection::update_and_project`）之後，
    /// 閘門在同一個臨界區內、寫檔前就查過 DB 快照：擋下來時 TOML 位元組與 DB 都不變。
    #[tokio::test]
    async fn a_non_delete_mutation_the_bulk_guard_would_reject_leaves_the_file_and_the_db_untouched() {
        let e = crate::testing::env().await;
        let (app, pid) = (e.app.clone(), e.project_id.clone());
        let repo = e.repo.to_string_lossy().to_string();

        // 種 4 顆 bot 並投影進 DB：DB 的活列＝上一次投影的結果。
        app.cfg
            .update(|cfg| {
                cfg.projects.push(crate::config::ProjectCfg {
                    handed_off_to: None,
                    id: Some(pid.clone()),
                    path: repo.clone(),
                    label: "proj".into(),
                    host: LOCAL_HOST.into(),
                    bots: ["b1", "b2", "b3", "b4"]
                        .into_iter()
                        .map(|id| crate::config::BotCfg {
                            id: Some(id.to_string()),
                            name: id.to_string(),
                            kind: "claude".into(),
                            model: None,
                            effort: None,
                            fast: false,
                            persona: None,
                            args: vec![],
                            autostart: false,
                            inject_hooks: true,
                            auto_approve: true,
                            identity: None,
                            env: Default::default(),
                            herdr_session: None,
                            create_request_id: None,
                            create_fingerprint: None,
                        })
                        .collect(),
                });
                Ok(())
            })
            .await
            .unwrap();
        crate::projection::project_config(&app.cfg, &app.db).await.unwrap();
        assert_eq!(db::live_bots(&app.db).await.unwrap().len(), 4);

        // 外面把 config.toml 換掉：只剩 1 顆 bot。`ConfigStore::update` 會先重讀最新版，這裡先強迫
        // 重讀一次（不改任何東西，跟 projection.rs 的 `a_config_swapped_under_a_running_daemon_is_refused` 同一招）。
        let reduced = format!(
            "[server]\nlisten = '127.0.0.1:7788'\n\n[[projects]]\nid = '{pid}'\npath = '{repo}'\nlabel = 'proj'\nhost = 'local'\n\n[[projects.bots]]\nid = 'b1'\nname = 'b1'\nkind = 'claude'\n"
        );
        std::fs::write(&app.cfg.path, &reduced).unwrap();
        app.cfg.update(|_| Ok(())).await.unwrap();

        let err = patch_project(State(app.clone()), Path(pid.clone()), Json(PatchProject { label: Some("renamed".into()), handed_off_to: None }))
            .await
            .unwrap_err();
        match err {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "projection_refused");
                assert_eq!(body["config_written"], false, "落盤前就被擋，跟身分 API 的 config_written:true 相反：{body}");
            }
            other => panic!("expected 409 projection_refused, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&app.cfg.path).unwrap(), reduced, "TOML 一個字都不能動");
        assert_eq!(app.cfg.get().await.projects[0].label, "proj", "記憶體裡那份也不能變（rename 沒套用）");
        assert_eq!(db::live_bots(&app.db).await.unwrap().len(), 4, "DB 不該被動到");
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
    // 已刪 bot 仍在結果裡（總管查證據要），但活的排前面：命中較多的已刪 bot 不能把活 bot 擠出 `limit`（#766）。
    let rows: Vec<(String, bool, i64, String)> = sqlx::query_as(
        r#"SELECT c.bot_id, b.deleted_at IS NOT NULL AS bot_deleted, COUNT(*) AS hits,
                  (SELECT m2.content FROM messages m2
                     JOIN conversations c2 ON c2.id = m2.conversation_id
                    WHERE c2.bot_id = c.bot_id AND m2.content LIKE ?1 ESCAPE '\'
                    ORDER BY m2.created_at DESC, m2.rowid DESC LIMIT 1) AS newest
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             JOIN bots b ON b.id = c.bot_id
            WHERE m.content LIKE ?1 ESCAPE '\'
            GROUP BY c.bot_id
            ORDER BY bot_deleted ASC, hits DESC, c.bot_id
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
        .map(|(bot_id, bot_deleted, hits, newest)| {
            json!({"bot_id": bot_id, "bot_deleted": bot_deleted, "hits": hits, "snippet": snippet_around(&newest, &lower, 90)})
        })
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

/// SPEC §15.4. Guard rails (in the tree, never herdr／daemon, never a bot) live in `memproc::kill`,
/// which re-samples first and confirms the pid is still the same process in the same command (#526).
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
        Ok(Err(crate::memproc::KillDenied::Daemon)) => Err(LcError::Bad("不能砍 AG Man 自己（或它開的 ssh／helper）".into())),
        // 什麼都沒送：那個 pid 在重新取樣與送訊號之間換了行程。重新整理清單再決定。
        Ok(Err(crate::memproc::KillDenied::PidChanged)) => Err(LcError::conflict(
            "pid_changed",
            json!({"pid": pid, "message": "這個 pid 已經不是剛才那一顆行程了，沒有送出任何訊號；重新整理清單再試"}),
        )),
        Ok(Err(crate::memproc::KillDenied::Bot(id))) => {
            Err(LcError::conflict("bot_process", json!({"bot_id": id, "message": "這是 AG Man 的 bot，請用停止 bot"})))
        }
    }
}

/// `?refresh=1` 最多等這麼久就回當下的快照。claude 探測一次最久 40 秒、grok 25 秒，而且每台主機各探一次：
/// 以前依序 await 全部跑完才回，一台慢的 ssh 主機就把 HTTP 請求拖到好幾分鐘（quota 修正 2026-09-16 轉來）。
const QUOTA_REFRESH_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// 等背景工作最多 `wait`。逾時回 `false`，**工作不會被取消**（丟掉 `JoinHandle` 不會 abort）：探測照樣跑完、寫進 quota、推 WS。
async fn finished_within(wait: std::time::Duration, job: tokio::task::JoinHandle<()>) -> bool {
    tokio::time::timeout(wait, job).await.is_ok()
}

async fn get_quota(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Response, LcError> {
    let mut pending = false;
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
        // 各主機併發（`quota::for_each_host`，跟背景輪詢同一份），同一台的三個 kind 也併發；各自的 probe_lock 照舊。
        let app2 = app.clone();
        let job = tokio::spawn(async move {
            crate::quota::for_each_host(hosts, move |host| {
                let app = app2.clone();
                async move {
                    let (codex, claude, grok) = tokio::join!(
                        crate::quota::refresh_codex(&app, &host),
                        crate::quota_claude::refresh_claude(&app, &host),
                        crate::quota_grok::refresh_grok(&app, &host),
                    );
                    for (kind, res) in [("codex", codex), ("claude", claude), ("grok", grok)] {
                        if let Err(e) = res {
                            tracing::warn!(host = %host, kind, error = %e, "quota refresh failed");
                        }
                    }
                }
            })
            .await
        });
        pending = !finished_within(QUOTA_REFRESH_WAIT, job).await;
    }
    let mut resp = Json(crate::quota::snapshot(&app).await).into_response();
    if pending {
        // body 是 quota key 的 map，不能塞旗標進去（前端會把它當成一格額度）：用 header 說「還有探測在背景跑」。
        resp.headers_mut().insert("x-am-quota-refresh", axum::http::HeaderValue::from_static("pending"));
    }
    Ok(resp)
}

/// `POST /api/quota/probe?kind=claude&account=cc0[&host=]`（#404）：強制重跑那個帳號的 `/usage`，
/// 結果（`source=claude-usage`）直接覆寫 cache——statusLine 的守衛擋不下來的錯值，人手動校正的出口。
async fn probe_quota(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, LcError> {
    let kind = q.get("kind").map(String::as_str).unwrap_or("claude");
    if kind != "claude" {
        return Err(LcError::Bad(format!("kind `{kind}` has no forced usage probe; only `claude`")));
    }
    let host = q.get("host").map(String::as_str).filter(|h| !h.is_empty()).unwrap_or(LOCAL_HOST);
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let account = q.get("account").map(|a| a.trim()).filter(|a| !a.is_empty());
    match crate::quota_claude::force_probe(&app, host, account).await {
        Ok((key, quota)) => Ok(Json(json!({"key": key, "quota": crate::quota::quota_value(&quota, false)}))),
        Err(crate::quota_claude::ForceProbeError::UnknownAccount) => Err(LcError::NotFound("identity".into())),
        Err(crate::quota_claude::ForceProbeError::NotInstalled) => {
            Err(LcError::conflict("claude_not_installed", json!({"host": host})))
        }
        Err(crate::quota_claude::ForceProbeError::Failed(m)) => Err(LcError::Upstream(m)),
    }
}

#[cfg(test)]
mod delete_identity_tests {
    use super::*;

    fn ident(name: &str, kind: &str, host: Option<&str>) -> crate::config::IdentityCfg {
        crate::config::IdentityCfg { name: name.into(), kind: kind.into(), host: host.map(String::from), env: Default::default(), args: vec![] }
    }

    /// 沒寫 host 的身分在遠端也生效（quota 那顆 `dca3c4c`），除非那台有自己同名的（config 明寫、或 shell 的 `ccN`）。
    #[test]
    fn a_hostless_identity_is_in_effect_on_a_remote_host_unless_that_host_has_its_own() {
        let hostless = vec![ident("work", "codex", None)];
        assert!(hostless_identity_in_effect(&hostless, "m4p", None, "work"));
        let shadowed = vec![ident("work", "codex", None), ident("work", "codex", Some("m4p"))];
        assert!(!hostless_identity_in_effect(&shadowed, "m4p", None, "work"), "那台有明寫的");
        let cc = vec![ident("cc1", "claude", None)];
        let m4p_shell = vec![ident("cc1", "claude", None)];
        assert!(!hostless_identity_in_effect(&cc, "m4p", Some(&m4p_shell), "cc1"), "那台 shell 自己有 cc1");
        assert!(hostless_identity_in_effect(&cc, "m4p", Some(&[]), "cc1"), "那台偵測過、沒有 cc1");
        assert!(hostless_identity_in_effect(&cc, "m4p", None, "cc1"), "還沒偵測：照最保守的算");
    }

    /// 刪掉沒寫 host 的 `work`：m4p 上的 bot 正在用它（那台沒有自己的 `work`）→ 409，不能只看本機的 bot。
    #[tokio::test]
    async fn deleting_a_hostless_identity_is_refused_while_a_remote_bot_relies_on_it() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let text = |own: &str| {
            format!(
                "[server]\nlisten = '127.0.0.1:7788'\n\n[[identities]]\nname = 'work'\nkind = 'codex'\n{own}\n\
                 [[projects]]\nid = 'p9'\npath = '/Users/me/wt'\nlabel = 'wt'\nhost = 'm4p'\n\n\
                 [[projects.bots]]\nid = 'b9'\nname = 'worker'\nkind = 'codex'\nidentity = 'work'\n"
            )
        };
        std::fs::write(&app.cfg.path, text("")).unwrap();
        app.cfg.update(|_| Ok(())).await.unwrap();
        crate::projection::project_config(&app.cfg, &app.db).await.unwrap();

        let del = || delete_identity(State(app.clone()), Path("work".into()), Query(IdentityHostQuery { host: None }));
        match del().await {
            Err(LcError::Conflict(body)) => assert_eq!((body["bot_id"].as_str(), body["host"].as_str()), (Some("b9"), Some("m4p"))),
            other => panic!("遠端 bot 還靠它：{:?}", other.map(|r| r.status())),
        }
        assert_eq!(app.cfg.get().await.identities.len(), 1, "什麼都沒刪");

        // m4p 有自己的 work：刪沒寫 host 的那筆不影響那台的 bot。
        std::fs::write(&app.cfg.path, text("\n[[identities]]\nname = 'work'\nkind = 'codex'\nhost = 'm4p'\n")).unwrap();
        app.cfg.update(|_| Ok(())).await.unwrap();
        crate::projection::project_config(&app.cfg, &app.db).await.unwrap();
        del().await.expect("那台用的是自己的 work");
        let left = app.cfg.get().await.identities;
        assert_eq!((left.len(), left[0].host.as_deref()), (1, Some("m4p")));
    }

    /// 並行：`create_bot` 驗過身分存在之後、寫進 config 之前，那個身分被刪掉。`projection::validate` 在 config 鎖裡重驗，
    /// 所以不會建出帶孤兒身分的 bot（config 與 DB 都沒有它）；錯誤碼是 404 identity（以前是籠統的 400 `config_invalid`）。
    #[tokio::test]
    async fn a_bot_created_while_its_identity_is_deleted_is_refused() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (spid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
        app.cfg
            .update(move |c| {
                c.identities.push(ident("racer", "codex", None));
                c.projects.push(crate::config::ProjectCfg { id: Some(spid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![], handed_off_to: None });
                Ok(())
            })
            .await
            .unwrap();
        let (a2, pid) = (app.clone(), env.project_id.clone());
        crate::lifecycle::race_point::arm("create_bot_after_identity_check", &env.project_id, move || async move {
            delete_identity(State(a2.clone()), Path("racer".into()), Query(IdentityHostQuery { host: None }))
                .await
                .expect("沒有 bot 在用，刪得掉");
            let _ = pid;
        });
        let body = json!({"name": "late", "kind": "codex", "identity": "racer"});
        let res = create_bot(State(app.clone()), Path(env.project_id.clone()), Json(serde_json::from_value(body).unwrap())).await;
        assert!(matches!(res, Err(LcError::NotFound(_))), "身分已被刪：不能建出帶孤兒身分的 bot：{:?}", res.map(|r| r.status()));
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE name = 'late'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 0);
        assert!(app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).all(|b| b.name != "late"), "config 也不能留下它");
    }

    /// 並行：`delete_identity` 查完「沒有 bot 在用」之後、真的刪除之前，有 bot 開始用它。刪除在 config 鎖裡被 `projection::validate`
    /// 擋下（身分還在、bot 也在）；錯誤碼是 409 identity still used by bots（以前是籠統的 400 `config_invalid`）。
    #[tokio::test]
    async fn an_identity_taken_by_a_bot_while_it_is_being_deleted_survives() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let (spid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
        app.cfg
            .update(move |c| {
                c.identities.push(ident("racer2", "codex", None));
                c.projects.push(crate::config::ProjectCfg { id: Some(spid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![], handed_off_to: None });
                Ok(())
            })
            .await
            .unwrap();
        let (a2, pid) = (app.clone(), env.project_id.clone());
        crate::lifecycle::race_point::arm("delete_identity_after_check", "racer2", move || async move {
            let body = json!({"name": "early", "kind": "codex", "identity": "racer2"});
            create_bot(State(a2.clone()), Path(pid), Json(serde_json::from_value(body).unwrap())).await.expect("身分還在：建得起來");
        });
        let res = delete_identity(State(app.clone()), Path("racer2".into()), Query(IdentityHostQuery { host: None })).await;
        assert!(matches!(res, Err(LcError::Conflict(_))), "有 bot 剛開始用它：不能刪：{:?}", res.map(|r| r.status()));
        assert!(app.cfg.get().await.identities.iter().any(|i| i.name == "racer2"), "身分還在");
    }

    /// 刪 m4p 那一台的 `work`：本機另有一筆自己的 `work`，它的快取列不能跟著消失（快取只在下次偵測才會補回來）。
    #[tokio::test]
    async fn deleting_one_hosts_identity_leaves_the_other_hosts_cached_row() {
        let env = crate::testing::env().await;
        let app = &env.app;
        std::fs::write(
            &app.cfg.path,
            "[server]\nlisten = '127.0.0.1:7788'\n\n[[identities]]\nname = 'work'\nkind = 'codex'\nhost = 'm4p'\n\n\
             [[identities]]\nname = 'work'\nkind = 'codex'\nhost = 'local'\n\n\
             [[projects]]\nid = 'p9'\npath = '/Users/me/wt'\nlabel = 'wt'\nhost = 'm4p'\n",
        )
        .unwrap();
        app.cfg.update(|_| Ok(())).await.unwrap();
        crate::projection::project_config(&app.cfg, &app.db).await.unwrap();
        for host in ["local", "m4p"] {
            let mut ht = crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![],
                utc_offset_secs: None,
                herdr_cli: None,
                checked_at: crate::db::now(),
            };
            ht.identities.insert("work".into(), crate::tools::IdentityInfo::shell("work", "codex", None));
            app.tools.lock().await.insert(host.into(), ht);
        }
        delete_identity(State(app.clone()), Path("work".into()), Query(IdentityHostQuery { host: Some("m4p".into()) }))
            .await
            .expect("沒有 bot 在用");
        let tools = app.tools.lock().await;
        assert!(!tools["m4p"].identities.contains_key("work"), "被刪的那一台的快取列要拿掉");
        assert!(tools["local"].identities.contains_key("work"), "本機自己的 work 沒被刪，快取列不能一起消失");
    }
}

#[cfg(test)]
mod quota_refresh_tests {
    /// 逾時就先回，背景的探測不能被取消（它跑完照樣寫進 quota、推 WS）。
    ///
    /// 不靠牆鐘（以前 `elapsed < 250ms`＋`sleep(500ms)` 在慢 runner 上會掛）：慢的那個工作卡在閘門上，
    /// 閘門在「已經逾時先回」之後才放開——所以「不等慢的那一個」是結構上成立的，不是量出來的；放開後用輪詢等它跑完。
    #[tokio::test]
    async fn a_slow_refresh_answers_early_and_keeps_running() {
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (g, d) = (gate.clone(), done.clone());
        let slow = tokio::spawn(async move {
            g.notified().await;
            d.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(!super::finished_within(std::time::Duration::from_millis(20), slow).await, "卡在閘門上：逾時先回");
        assert!(!done.load(std::sync::atomic::Ordering::SeqCst), "先回的時候慢的還沒做完");
        gate.notify_one();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !done.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "逾時後背景沒有跑完（被取消了？）");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let quick = tokio::spawn(async {});
        assert!(super::finished_within(std::time::Duration::from_secs(30), quick).await);
    }
}

#[derive(Deserialize)]
struct NewIdentity {
    name: String,
    kind: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    args: Vec<String>,
    /// 哪一台主機的身分（SPEC §16.2）。省略＝本機。
    #[serde(default)]
    host: Option<String>,
}

async fn create_identity(State(app): State<Arc<App>>, Json(b): Json<NewIdentity>) -> Result<Response, LcError> {
    if !valid_identity_name(&b.name) {
        return Err(LcError::Bad(format!("identity name must match {}", crate::config::SLUG_NAME_RE)));
    }
    if !crate::config::valid_kind(&b.kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::kinds_list())));
    }
    let host = b.host.clone().map(|h| h.trim().to_string()).filter(|h| !h.is_empty());
    if let Some(h) = host.as_deref().filter(|h| *h != crate::config::LOCAL_HOST) {
        let known = app.hosts.list().await.iter().any(|c| c.name == h);
        if !known {
            return Err(LcError::conflict("unknown host", json!({"host": h})));
        }
    }
    let cfg = IdentityCfg { name: b.name.clone(), kind: b.kind.clone(), host, env: b.env.clone(), args: b.args.clone() };
    let res = crate::projection::update_and_project(&app.cfg, &app.db, move |f| {
        // 鍵是 `(host, name)`：同一個名字可以在不同主機各有一份（同名不同帳號正是 §16.2 的前提）。
        if f.identities.iter().any(|i| i.name == cfg.name && i.host_or_local() == cfg.host_or_local()) {
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
        Err(e) => return Err(projection_err(e)),
    }
    app.emit("identities_changed", json!({})).await;
    for c in app.hosts.list().await {
        crate::tools::spawn_detect(app.clone(), c.name.clone());
    }
    Ok((StatusCode::OK, Json(json!({"name": b.name}))).into_response())
}

#[derive(Deserialize)]
struct IdentityHostQuery {
    /// 要刪哪一台的那一筆（SPEC §16.2）。省略＝本機。
    #[serde(default)]
    host: Option<String>,
}

/// `host` 上名叫 `name` 的身分，實際生效的是不是那筆**沒寫 host** 的 config（`tools::merge_identities` 的規則）。
/// 那台還沒偵測過 shell 的 `ccN`（`shell` 是 `None`）時照「偵測完、那台沒有」算：寧可擋下一次刪除，
/// 也不要刪掉之後那台 bot 下次啟動才發現身分沒了（quota 修正 2026-09-16 轉來）。
fn hostless_identity_in_effect(
    config: &[crate::config::IdentityCfg],
    host: &str,
    shell: Option<&[crate::config::IdentityCfg]>,
    name: &str,
) -> bool {
    crate::tools::merge_identities(config, host, Some(shell.unwrap_or_default()))
        .into_iter()
        .find(|(i, _)| i.name == name)
        // shell 讀到的 `ccN` 也沒有 host：要看來源，不能只看 `is_hostless`。
        .is_some_and(|(i, source)| source == crate::tools::SOURCE_CONFIG && i.is_hostless())
}

async fn delete_identity(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Query(q): Query<IdentityHostQuery>,
) -> Result<Response, LcError> {
    let host = q.host.as_deref().map(str::trim).filter(|h| !h.is_empty()).unwrap_or(crate::config::LOCAL_HOST).to_string();
    let cfg = app.cfg.get().await;
    // 刪本機那一筆時，沒寫 host 的那份也一起刪（`host_or_local`）；它在遠端也適用（`tools::merge_identities` 第 4 條）。
    let removing_hostless = host == crate::config::LOCAL_HOST && cfg.identities.iter().any(|i| i.name == name && i.is_hostless());
    for b in db::live_bots(&app.db).await.map_err(any_err)? {
        if b.identity.as_deref() != Some(name.as_str()) {
            continue;
        }
        // 同一台的 bot 還在用它。同名的 `cc1` 在別台通常是別的帳號——除非那台用的正是這筆沒寫 host 的。
        // 讀不到某顆 bot 的 host 就不刪（#243）：當成 local 可能放行「遠端 bot 還在用」的 identity。
        let bot_host = db::bot_host(&app.db, &b.id).await.map_err(any_err)?;
        let shell = app.tools.lock().await.get(&bot_host).map(|t| t.shell_identities.clone());
        if bot_host == host
            || (removing_hostless && hostless_identity_in_effect(&cfg.identities, &bot_host, shell.as_deref(), &name))
        {
            return Err(LcError::conflict("identity still used by bots", json!({"bot_id": b.id, "host": bot_host})));
        }
    }
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_identity_after_check", &name).await;
    let (n2, h2) = (name.clone(), host.clone());
    crate::projection::update_and_project(&app.cfg, &app.db, move |f| {
        f.identities.retain(|i| !(i.name == n2 && i.host_or_local() == h2));
        Ok(())
    })
    .await
    .map_err(|e| {
        if is_unknown_identity(&e) {
            // 查完之後、刪除之前有 bot 開始用它：config 鎖裡的驗證擋下來了（什麼都沒刪）。
            LcError::conflict("identity still used by bots", json!({"host": host, "detail": "a bot started using it while it was being deleted"}))
        } else {
            projection_err(e)
        }
    })?;
    // A same-named `ccN` alias is a different entry (SPEC §16) and is put back from `shell_identities`.
    // 只動受影響的那幾台的快取：被刪的那一台；刪的是沒寫 host 的那筆時，還靠它的每一台（自己沒有明寫同名的）。
    let cfg_after = app.cfg.get().await;
    for (h, ht) in app.tools.lock().await.iter_mut() {
        let has_own = cfg_after.identities.iter().any(|i| i.name == name && i.host_or_local() == h.as_str());
        if !(*h == host || (removing_hostless && !has_own)) {
            continue;
        }
        ht.identities.remove(&name);
        if let Some(i) = ht.shell_identities.iter().find(|i| i.name == name) {
            let dir = i.env.get("CLAUDE_CONFIG_DIR").cloned();
            ht.identities.insert(name.clone(), crate::tools::IdentityInfo::shell(&i.name, &i.kind, dir));
        }
    }
    app.emit("identities_changed", json!({})).await;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

#[derive(Deserialize, Default)]
struct StartQuery {
    /// 沒帶（預設）：有記錄的 session 就接回，沒有才開新對話（2026-10-02 使用者：「預設必 resume」——console-rpa 換身分後
    /// 重啟沒帶參數，起了新 session、整個失憶）。`native`：一定要接回，接不回回 409 `resumed:false`、不啟動。
    /// `fresh`：明確要開新對話。
    resume: Option<String>,
    /// 跟 `resume=native` 一起：不看 DB，接這一段 session（救援用；只有 `bin/agm` 露出這個旗標）。
    session: Option<String>,
}

fn resume_opts(q: &StartQuery) -> Result<lifecycle::StartOpts, LcError> {
    let session = q.session.as_deref().map(str::trim).filter(|v| !v.is_empty());
    match q.resume.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
        None if session.is_some() => Err(LcError::Bad("`session` needs `resume=native`".into())),
        None => Ok(lifecycle::StartOpts { resume_native: true, ..Default::default() }),
        Some("fresh") if session.is_none() => Ok(lifecycle::StartOpts::default()),
        Some("fresh") => Err(LcError::Bad("`session` needs `resume=native`".into())),
        Some("native") => {
            if let Some(s) = session {
                if s.len() > 128 || !s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
                    return Err(LcError::Bad("`session` must be a plain session id".into()));
                }
            }
            Ok(lifecycle::StartOpts { resume_native: true, resume_required: true, resume_session: session.map(str::to_string), ..Default::default() })
        }
        Some(other) => Err(LcError::Bad(format!("unknown resume mode `{other}` (only `native` or `fresh`)"))),
    }
}

/// 回應裡的 `resumed`／`session_id`／`resume_outcome`（issue #107）。
///
/// `runs.resume_session_id` 只是「帶了 `--resume`、還在等 CLI 回報」的暫存：SessionStart 一到就被清掉，
/// 而這支是在 bot 鎖放掉之後才讀——只看它，接回成功的會被說成 `resumed:false`。結論以
/// `runs.resume_outcome` 為準（`lifecycle::resume_gate`）：
/// - `verified`：接回了，`session_id` 是回報的那一個；
/// - `mismatch`：CLI 開了新對話，`resumed:false`；
/// - 還沒結論或 `unverified`（等滿沒回報、刻意放行）：`resume_session_id` 還在，照「帶了 `--resume`」算 `true`；
/// - 都沒有：這次沒帶 `--resume`（接不回、退回開新對話）。
async fn started_json(app: &Arc<App>, run_id: &str, opts: &lifecycle::StartOpts) -> Result<Value, LcError> {
    if !opts.resume_native {
        return Ok(json!({"run_id": run_id}));
    }
    let (pending, outcome, native): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT resume_session_id, resume_outcome, native_session_id FROM runs WHERE id = ?")
            .bind(run_id)
            .fetch_optional(&app.db)
            .await
            .map_err(any_err)?
            .unwrap_or_default();
    let (resumed, sid) = match outcome.as_deref() {
        Some("verified") => (true, native),
        Some("mismatch") => (false, None),
        _ => (pending.is_some(), pending),
    };
    Ok(json!({"run_id": run_id, "resumed": resumed, "session_id": sid, "resume_outcome": outcome}))
}

async fn start_bot(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<StartQuery>) -> Result<Response, LcError> {
    refuse_child_restart(&app, &id).await?;
    // 被 AGM 因為閒置收起來的（§6.11）一律走續接：使用者按「啟動」要的是把剛剛那顆帶著對話的
    // bot 叫回來，不是開一段新的空白對話。
    if crate::supervisor::idle_sleep::wake(&app, &id, "使用者按了啟動")
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
    {
        // 叫醒接不回時會退回開新對話（`idle_sleep::wake`）：照實際結果回，不寫死 `true`（issue #107）。
        let body = match db::active_run(&app.db, &id).await.map_err(any_err)? {
            Some(run) => started_json(&app, &run.id, &lifecycle::StartOpts { resume_native: true, ..Default::default() }).await?,
            None => json!({"run_id": null, "resumed": false}),
        };
        return Ok((StatusCode::OK, Json(body)).into_response());
    }
    let opts = resume_opts(&q)?;
    let run_id = lifecycle::start_bot_with(&app, &id, opts.clone()).await?;
    Ok((StatusCode::OK, Json(started_json(&app, &run_id, &opts).await?)).into_response())
}

/// 子 agent（`parent_bot_id` 非空）一律由父 bot 用 herdr 重開，daemon 的 start／restart 不收（SPEC §6.5a，
/// 2026-09-22：herdr 全重啟後對子 agent 下 restart，pane 已不在，原地重啟反而讓 reconcile 把它退役軟刪）。
/// 一鍵重啟（§6.9）走的是 `bulk_restart` 內部的原地重啟，不經這裡。
async fn refuse_child_restart(app: &Arc<App>, id: &str) -> Result<(), LcError> {
    let Some(bot) = db::bot(&app.db, id).await.map_err(any_err)? else { return Ok(()) };
    match bot.parent_bot_id.as_deref().filter(|p| !p.is_empty()) {
        Some(parent) => Err(LcError::conflict(
            "child_restart_forbidden",
            json!({"bot_id": id, "parent_bot_id": parent, "message": "子 agent 一律由父 bot 用 herdr 重開；被軟刪的先 POST /api/bots/{id}/restore（§10.4a）"}),
        )),
        None => Ok(()),
    }
}

/// 這顆 daemon 支援哪些要先確認才能用的能力（例如升級腳本在停 herdr 前要確定 `resume_native_start`）。
async fn get_capabilities() -> Json<Value> {
    Json(json!({"capabilities": ["resume_native_start", "herdr_maintenance", "service_principals", "swap_restart_window"]}))
}

/// SPEC §6.9。立刻回計畫、進度走 WS：一顆 `stop_bot` 最久十秒，同步做完會拖死 HTTP 連線。
async fn restart_idle_bots(State(app): State<Arc<App>>) -> Result<Response, LcError> {
    let plan = crate::bulk_restart::spawn(&app).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok((StatusCode::ACCEPTED, Json(plan)).into_response())
}

async fn restart_bot(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<StartQuery>) -> Result<Response, LcError> {
    let opts = resume_opts(&q)?;
    refuse_child_restart(&app, &id).await?;
    let run_id = lifecycle::restart_bot_with(&app, &id, opts.clone()).await?;
    let body = started_json(&app, &run_id, &opts).await?;
    app.emit("bot_changed", json!({"bot_id": id})).await;
    Ok((StatusCode::OK, Json(body)).into_response())
}

/// Replace the per-bot proof immediately. A live bot is restarted so the new `AM_BOT_TOKEN` is
/// present before it can make another authenticated request; a stopped bot uses it on next start.
///
/// Child bots are refused: their pane was opened by the parent and inherits the **parent's**
/// `AM_BOT_ID`／token (herdr shim, SPEC §6.5b); `restart_child_in_pane` does not rebuild that env,
/// so a rotated child token would never reach the pane. Rotate the parent instead.
async fn rotate_bot_credential(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    if principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    let lock = app.bot_lock(&id).await;
    let lock_guard = lock.lock().await;
    let mut tx = app.db.begin().await.map_err(any_err)?;
    let bot = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE id=?")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(any_err)?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.managed_by == "child" {
        return Err(LcError::conflict(
            "a child agent runs on its parent's credential; rotate the parent bot",
            json!({"reason": "child_uses_parent_credential", "bot_id": id, "parent_bot_id": bot.parent_bot_id}),
        ));
    }
    lifecycle::refuse_default_session(&bot)?;
    // herdr creates panes outside SQLite and bot_lock. The shim's spawn permit shares this gate:
    // once the fence is visible, new pane / child-agent creation fails closed; an operation already
    // in herdr makes this rotation refuse before touching the proof.
    let rotation_fence = match crate::credential_spawn::RotationFence::begin(&app, &id) {
        Ok(fence) => fence,
        Err(crate::credential_spawn::FenceError::AlreadyRotating) => {
            return Err(LcError::conflict("credential rotation is already checking child panes", json!({"reason": "credential_rotation_pending", "bot_id": id})));
        }
        Err(crate::credential_spawn::FenceError::SpawnsInFlight(count)) => {
            return Err(LcError::conflict(
                "a child pane is being created with the current credential; retry rotation after it finishes",
                json!({"reason": "child_spawn_in_progress", "bot_id": id, "active_spawns": count}),
            ));
        }
    };
    let host: String = sqlx::query_scalar("SELECT p.host FROM bots b JOIN projects p ON p.id=b.project_id WHERE b.id=?")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(any_err)?
        .unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    // Child panes carry the environment of the bot that opened them. Refuse before changing the
    // parent's proof while any live descendant still depends on it; follow the full parent chain
    // because a grandchild can retain the same inherited AM_BOT_ID/token pair.
    let live_descendants: Vec<String> = sqlx::query_scalar(
        "WITH RECURSIVE descendants(id) AS (
             SELECT id FROM bots WHERE parent_bot_id = ?
             UNION
             SELECT b.id FROM bots b JOIN descendants d ON b.parent_bot_id = d.id
         )
         SELECT DISTINCT d.id FROM descendants d
         JOIN runs r ON r.bot_id = d.id AND r.state IN ('starting','running','stopping')
         ORDER BY d.id",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await
    .map_err(any_err)?;
    #[cfg(test)]
    crate::lifecycle::race_point::hit("credential_rotation_after_descendant_query", &id).await;
    if !live_descendants.is_empty() {
        return Err(LcError::conflict(
            "live descendants still use the parent's credential; stop them before rotating",
            json!({"reason": "live_children_use_credential", "bot_id": id, "children": live_descendants}),
        ));
    }
    // Bare panes opened with `herdr pane split` are not bots yet, so they do not appear in runs.
    // The shim registers each successful pane creation in the existing pane inventory before
    // releasing its permit; keep the old credential until those live siblings are gone too.
    let inherited_panes: Vec<String> = sqlx::query_scalar("SELECT pane_id FROM panes WHERE host=? AND owner_bot_id=? ORDER BY pane_id")
        .bind(&host)
        .bind(&id)
        .fetch_all(&mut *tx)
        .await
        .map_err(any_err)?;
    if !inherited_panes.is_empty() {
        return Err(LcError::conflict(
            "live panes created by this bot still carry its credential; close them before rotating",
            json!({"reason": "live_children_use_credential", "bot_id": id, "child_panes": inherited_panes}),
        ));
    }
    let active = sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE bot_id=? AND state IN ('starting','running','stopping') LIMIT 1")
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(any_err)?;
    let intent_id = if let Some(run) = &active {
        let open_restart: Option<(String, String)> = sqlx::query_as(
            "SELECT id, payload_json FROM intents WHERE kind='restart' AND subject_id=? AND status IN ('pending','running') LIMIT 1",
        )
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(any_err)?;
        if let Some((intent_id, payload_json)) = open_restart {
            let already_rotated = serde_json::from_str::<Value>(&payload_json)
                .ok()
                .and_then(|payload| payload.get("credential_rotation").and_then(Value::as_bool))
                .unwrap_or(false);
            if already_rotated {
                return Err(LcError::conflict(
                    "credential rotation is already committed and its restart is pending",
                    json!({"reason": "restart_pending", "credential_rotated": true, "restart_pending": true, "bot_id": id, "intent_id": intent_id}),
                ));
            }
            return Err(LcError::conflict(
                "a restart is already pending for this bot",
                json!({"reason": "restart_in_progress", "bot_id": id, "intent_id": intent_id}),
            ));
        }
        let payload = json!({
            "credential_rotation": true,
            "opts": lifecycle::StartOpts::default(),
            "from_run_id": run.id,
            "bot_name": bot.name,
        });
        Some(
            crate::intents::insert_pending_on(
                &mut tx,
                "restart",
                &id,
                &host,
                &payload,
                crate::restart_intents::ROTATION_INTENT_TTL_SECS,
            )
            .await
            .map_err(any_err)?,
        )
    } else {
        None
    };
    let token = crate::projection::new_token();
    let changed = sqlx::query("UPDATE bots SET hook_token=? WHERE id=? AND hook_token=? AND deleted_at IS NULL")
        .bind(&token)
        .bind(&id)
        .bind(&bot.hook_token)
        .execute(&mut *tx)
        .await
        .map_err(any_err)?
        .rows_affected();
    if changed == 0 {
        return Err(LcError::NotFound("bot".into()));
    }
    tx.commit().await.map_err(any_err)?;
    rotation_fence.committed();
    drop(lock_guard);
    app.emit("bot_changed", json!({"bot_id": id})).await;
    let Some(intent_id) = intent_id else {
        return Ok((StatusCode::OK, Json(json!({"credential_rotated": true, "restarted": false, "run_id": null}))).into_response());
    };
    #[cfg(test)]
    crate::lifecycle::race_point::hit("credential_rotation_committed", &id).await;

    let pending_error = |detail: String| {
        LcError::conflict(
            "credential rotated, but restarting the bot is still pending",
            json!({"reason": "restart_pending", "credential_rotated": true, "restart_pending": true, "bot_id": id, "intent_id": intent_id, "detail": detail}),
        )
    };
    if let crate::restart_intents::Outcome::Retry(why) = crate::restart_intents::drive_once(&app, &intent_id).await {
        crate::restart_intents::retry_later(&app, &intent_id);
        return Err(pending_error(why));
    }
    let intent = match crate::intents::get(&app.db, &intent_id).await {
        Ok(Some(intent)) => intent,
        Ok(None) => {
            crate::restart_intents::retry_later(&app, &intent_id);
            return Err(pending_error("restart intent could not be read after credential commit".into()));
        }
        Err(e) => {
            crate::restart_intents::retry_later(&app, &intent_id);
            return Err(pending_error(format!("restart intent could not be read after credential commit: {e:#}")));
        }
    };
    if intent.status != "done" {
        let open = matches!(intent.status.as_str(), "pending" | "running");
        if intent.status == "pending" {
            crate::restart_intents::retry_later(&app, &intent_id);
        }
        return Err(LcError::conflict(
            "credential rotated, but restarting the bot did not finish",
            json!({
                "reason": if open { "restart_pending" } else { "restart_failed" },
                "credential_rotated": true,
                "restart_pending": open,
                "bot_id": id,
                "intent_id": intent_id,
                "detail": intent.last_error,
            }),
        ));
    }
    let run_id = match db::active_run(&app.db, &id).await {
        Ok(Some(run)) => run.id,
        Ok(None) => {
            return Err(LcError::conflict(
                "credential rotated, but no active run was found after restart",
                json!({"reason": "restart_result_unreadable", "credential_rotated": true, "restart_pending": false, "bot_id": id, "intent_id": intent_id}),
            ));
        }
        Err(e) => {
            return Err(LcError::conflict(
                "credential rotated, but the active run could not be read",
                json!({"reason": "restart_result_unreadable", "credential_rotated": true, "restart_pending": false, "bot_id": id, "intent_id": intent_id, "detail": format!("{e:#}")}),
            ));
        }
    };
    Ok((StatusCode::OK, Json(json!({"credential_rotated": true, "restarted": true, "run_id": run_id}))).into_response())
}

fn require_service(principal: &RequestPrincipal, expected: &str) -> Result<(), LcError> {
    if matches!(principal, RequestPrincipal::Service(id) if id == expected) {
        Ok(())
    } else {
        Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "service_only", "service": expected})))
    }
}

/// Launchd's fixed harmless probe (`daemon-swap.sh` 3b). The service picks which bot answers the
/// self-test, but not the text, the source label, or anything else a real prompt could carry.
async fn service_daemon_swap_probe(
    State(app): State<Arc<App>>,
    Path(bot_id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_service(&principal, crate::service_auth::DAEMON_SWAP)?;
    if db::bot(&app.db, &bot_id).await.map_err(any_err)?.filter(|b| b.deleted_at.is_none()).is_none() {
        return Err(LcError::NotFound("bot".into()));
    }
    let out = lifecycle::prompt_from_api(
        &app,
        &bot_id,
        DAEMON_SWAP_PROBE_TEXT,
        &format!("daemon-swap-probe-{}", db::ulid()),
        &[],
        lifecycle::RelaySrc { from: Some(crate::agent_relay::DAEMON_SENDER), unverified: false },
        false,
        None,
    )
    .await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwapWindowIn {
    owner: String,
    commit: String,
    #[serde(default)]
    ttl_secs: Option<i64>,
}

/// 例行自動部署換版的窗口（使用者 2026-09-29：建置已在推 main 前測過、ubuntu-ci 也跑過，不再要核准單）。
/// daemon-swap 服務身分自己開一筆立即核准的 restart 單再走**同一個** `maintenance::acquire`：
/// 沒有人 working／送達中／別人握租約才拿得到、拿到時暫停 assignment 派送、fence 與 lease_token 都照舊。
/// 拿不到就把剛開的單撤掉，不留 pending／approved 的殘單。不接受 `exclude_bot_ids`：沒有「自己那顆 bot」可排除。
async fn service_daemon_swap_restart_window(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<SwapWindowIn>,
) -> Result<Json<Value>, LcError> {
    require_service(&principal, crate::service_auth::DAEMON_SWAP)?;
    let (owner, commit) = (b.owner.trim(), b.commit.trim());
    if owner.is_empty() || commit.is_empty() {
        return Err(LcError::Bad("owner 與 commit 都必填".into()));
    }
    let ttl = b.ttl_secs.unwrap_or(900);
    let actor = format!("service({})", crate::service_auth::DAEMON_SWAP);
    // 同一張核准一輪輪沿用，升級計時才接得下去（`swap_window`）。
    let id = crate::swap_window::approval_for(&app, owner, commit, &actor).await.map_err(any_err)?;
    match crate::supervisor::maintenance::acquire(&app, "restart", owner, &id, Some(commit), ttl, true, &[]).await {
        Ok(v) => Ok(Json(v)),
        Err(e) => {
            if !crate::swap_window::keep_after(&e) {
                let _ = crate::supervisor::store::decide_approval_from(&app.db, &id, "approved", "revoked", &actor, Some("沒拿到窗口"), None).await;
            }
            Err(e)
        }
    }
}

const DAEMON_SWAP_PROBE_TEXT: &str = "[build 自測，回 ok 即可，不要做任何事]";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceNotifyIn {
    text: String,
}

/// Launchd herdr upgrade may notify the configured responder, but cannot choose a bot or claim
/// another bot as the source. The daemon marks the message as its own maintenance action.
async fn service_herdr_upgrade_notify(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<ServiceNotifyIn>,
) -> Result<Response, LcError> {
    require_service(&principal, crate::service_auth::HERDR_UPGRADE)?;
    let text = body.text.trim();
    if text.is_empty() || text.len() > 4096 {
        return Err(LcError::Bad("text must be 1..=4096 bytes".into()));
    }
    let bot = crate::supervisor::roles::responder_bot(&app.db)
        .await
        .map_err(any_err)?
        .ok_or_else(|| LcError::Unavailable(json!({"error": "responder_unavailable", "retry_after_secs": 30})))?;
    let out = lifecycle::prompt_from_api(
        &app,
        &bot.id,
        text,
        &format!("herdr-upgrade-notify-{}", db::ulid()),
        &[],
        lifecycle::RelaySrc { from: Some(crate::agent_relay::DAEMON_SENDER), unverified: false },
        false,
        None,
    )
    .await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// Resume a bot after herdr server upgrade. The service can perform only this fixed operation;
/// it cannot choose a session, stop/restart a bot, or pass query options through to start_bot.
async fn service_herdr_upgrade_resume(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_service(&principal, crate::service_auth::HERDR_UPGRADE)?;
    start_bot(
        State(app),
        Path(id),
        Query(StartQuery { resume: Some("native".into()), session: None }),
    )
    .await
}

async fn stop_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let had = lifecycle::stop_bot(&app, &id).await?;
    if had {
        Ok((StatusCode::OK, Json(json!({}))).into_response())
    } else {
        Ok(StatusCode::NO_CONTENT.into_response())
    }
}

/// `turn_id`（選填）：只打斷這一筆。重試上一次中斷時帶上它，那一筆已經不在飛就不按 Esc（#147）。
#[derive(Default, Deserialize)]
struct InterruptBody {
    turn_id: Option<String>,
}

async fn interrupt_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    body: Option<Json<InterruptBody>>,
) -> Result<Response, LcError> {
    let turn_id = body.and_then(|Json(b)| b.turn_id);
    lifecycle::interrupt_turn(&app, &id, turn_id.as_deref()).await?;
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
    /// `submit_draft` 時不用帶（送的是框裡那段）；其他時候空字串照舊 400。
    #[serde(default)]
    text: String,
    client_request_id: Option<String>,
    /// In display order.
    #[serde(default)]
    attachments: Vec<String>,
    /// relay metadata；有效 Bot principal 省略時由 daemon 補成該 bot id。
    /// Bot principal 帶其他 bot id 時，middleware 的 principal proof 與 relay_auth 會拒絕。
    #[serde(default)]
    relay_from: Option<String>,
    /// bot 寫給 AGM 時明講「這是回覆」：`ack`（純告知）或 `reply_to`（回哪一則事件／交辦）。
    /// 都沒帶 = 新的事，叫醒協調者（SPEC §18.15）。
    #[serde(default)]
    ack: bool,
    #[serde(default)]
    reply_to: Option<String>,
    /// 插隊送出（issue #103）：對方回合中時打斷它，而不是回 409。只有 claude ≥ 2.1.275 的 run 認得
    /// 那顆鍵；其他情況照舊排隊／409，body 會帶 `send_now_refused` 說明為什麼沒插隊。
    #[serde(default)]
    send_now: bool,
    /// bot 沒在跑時：daemon 先把這一則收下（`delivery: "queued"`）、再替它啟動，起來後由佇列送出（issue #122）。
    /// bot 在跑就跟沒帶一樣。
    #[serde(default)]
    start_if_stopped: bool,
    /// Busy run: persist this user prompt and wait for the next idle edge (issue #733).
    #[serde(default)]
    queue_if_busy: bool,
    /// 409 `composer_busy` 之後（2026-09-26）：`clear_draft` 先清掉框裡那段再送 `text`，`submit_draft` 改成送出框裡那段
    /// （按 Enter、不帶 `text`）。兩個都要帶 `expect_draft_token`＝409 回的完整草稿 token；框裡換了字就 409 `draft_changed`、不動它。
    #[serde(default)]
    clear_draft: bool,
    #[serde(default)]
    submit_draft: bool,
    #[serde(default)]
    expect_draft_token: Option<String>,
}

async fn prompt_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(b): Json<PromptIn>,
) -> Result<Response, LcError> {
    let given_crid = b.client_request_id.clone();
    let crid = b.client_request_id.unwrap_or_else(db::ulid);
    // `relay_from` is metadata, never an alternate principal. A proven bot that omits it is
    // attributed to the id already authenticated by middleware. A User request claiming a bot
    // without that bot's token is refused (#410 ended #339's unsigned compatibility branch).
    let relay_claim = b.relay_from.as_deref().map(str::trim).filter(|value| !value.is_empty());
    let effective_relay = relay_claim.or_else(|| headers.get("X-AM-Bot-Id").and_then(|v| v.to_str().ok()));
    let relay = crate::relay_auth::authenticate(&app, &headers, effective_relay, &id).await?;
    // bot 寫給 AGM 的申請不直接開回合：排進協調者的佇列，回 202（SPEC §18.15）。
    if let Some(r) = &relay {
        let mark = crate::supervisor::bot_requests::ReplyMark { ack: b.ack, reply_to: b.reply_to.as_deref() };
        let queued = crate::supervisor::bot_requests::intercept(&app, &id, &r.from, &b.text, given_crid.as_deref(), &b.attachments, true, "api", mark);
        if let Some(v) = queued.await? {
            return Ok((StatusCode::ACCEPTED, Json(v)).into_response());
        }
    }
    let src = lifecycle::RelaySrc { from: relay.as_ref().map(|r| r.from.as_str()), unverified: false };
    // 框裡的草稿只有使用者自己在畫面上處理：bot 轉送的 prompt 不能替人送出或清掉別人的字。
    if (b.clear_draft || b.submit_draft) && relay.is_some() {
        return Err(LcError::Bad("clear_draft / submit_draft are for the user's own prompts, not relayed ones".into()));
    }
    let expect = if b.clear_draft || b.submit_draft {
        match b.expect_draft_token.as_deref().filter(|d| !d.trim().is_empty()) {
            Some(d) => Some(d),
            None => return Err(LcError::Bad("clear_draft / submit_draft need expect_draft_token (the token the 409 showed)".into())),
        }
    } else {
        None
    };
    let out = if b.submit_draft {
        if b.clear_draft || b.send_now || b.start_if_stopped || b.queue_if_busy || !b.attachments.is_empty() {
            return Err(LcError::Bad("submit_draft sends the composer's draft as it is; it takes no other options".into()));
        }
        lifecycle::submit_composer_draft(&app, &id, expect.unwrap_or_default(), &crid).await?
    } else if b.start_if_stopped && !b.send_now && !b.clear_draft {
        lifecycle::prompt_starting_or_queue(&app, &id, &b.text, &crid, &b.attachments, src, b.queue_if_busy).await?
    } else if b.queue_if_busy && !b.clear_draft {
        lifecycle::prompt_from_api_queue_if_busy(&app, &id, &b.text, &crid, &b.attachments, src, b.send_now, expect).await?
    } else {
        lifecycle::prompt_from_api(&app, &id, &b.text, &crid, &b.attachments, src, b.send_now, expect).await?
    };
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
    let name = q.get("name").map(String::as_str).unwrap_or("file").trim();
    let name = if name.is_empty() { "file" } else { name };
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|m| m.split(';').next().unwrap_or(m).trim().to_string())
        .unwrap_or_default();
    // 2026-09-14：任何檔案都收。mime 只決定 UI 畫縮圖還是檔案晶片，agent 讀到的一律是路徑。
    let mime = if mime.is_empty() { "application/octet-stream".to_string() } else { mime };
    // `{:#}` so the ssh / filesystem cause reaches the UI.
    let a = crate::attach::save(&app, &id, name, &mime, &body)
        .await
        .map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok((StatusCode::OK, Json(crate::attach::to_json(&a))).into_response())
}

/// 上傳時收的 `Content-Type` 是呼叫端自己給的（`upload_attachment` 刻意什麼都收），所以送回去的時候
/// 比照 `outbox::file` 三件事：白名單外一律 `application/octet-stream`、非圖片帶
/// `Content-Disposition: attachment`、一律 `nosniff`。使用者的 HTML 不該在 daemon 這個 origin 跑起來
/// （UI token 就在這個 origin 的 localStorage）——原本擋住它的只是「認證只看 header，導航不帶 token」
/// 與「前端只對 image/* 抓位元組」這兩個跟這支端點無關的巧合（#471）。
async fn get_attachment(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let (mime, data) = crate::attach::read(&app, &id).await.map_err(|e| {
        tracing::warn!(attachment = %id, error = %e, "attachment read failed");
        LcError::NotFound("attachment".into())
    })?;
    let served = crate::attach::served_mime(&mime);
    let disposition = if crate::attach::is_inline(served) { "inline" } else { "attachment" };
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, served.to_string()),
            (header::CONTENT_DISPOSITION, disposition.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            // svg 是 inline 的（拿掉會讓現有縮圖變破圖），所以這一層要擋住「真的被導航到」的情況：
            // sandbox 讓它拿不到這個 origin，腳本也不會跑。`<img>` 載入不是 document，這個標頭對它沒作用，
            // 但那條路本來就不會執行腳本。**注意**：`URL.createObjectURL` 只保留 MIME、不帶標頭，
            // 所以未來若要加「在新分頁開啟附件」，不能靠這個標頭，要用別的方式（#471）。
            (header::CONTENT_SECURITY_POLICY, "sandbox; default-src 'none'".to_string()),
            (header::CACHE_CONTROL, "private, max-age=31536000".to_string()),
        ],
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
    /// true＝網頁的「補充」：打字成功後記成進行中回合的一則使用者訊息（API.md）。
    record: Option<bool>,
}

async fn text_bot(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<TextIn>) -> Result<Response, LcError> {
    let record = b.record.unwrap_or(false);
    let m = lifecycle::send_text_recorded(&app, &id, &b.text, b.enter.unwrap_or(true), b.expect_run_id, record).await?;
    let body = if record { json!({ "message_id": m.map(|m| m.id) }) } else { json!({}) };
    Ok((StatusCode::OK, Json(body)).into_response())
}

/// issue #122／#733：撤回還在等 bot 起來或閒下來的訊息，回傳原文與附件供輸入框還原。已經送出去的撤不回來（409）。
async fn withdraw_turn(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let restored = lifecycle::withdraw_turn(&app, &id).await?;
    Ok((
        StatusCode::OK,
        Json(json!({ "text": restored.text, "attachments": restored.attachments })),
    )
        .into_response())
}

#[cfg(test)]
mod withdraw_turn_tests {
    use super::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn withdrawing_an_awaits_idle_turn_returns_its_text_and_attachment_ids() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "withdraw-awaits-idle").await;
        let conversation_id = db::conversation_id(&app.db, &bot.id).await.unwrap();

        let turn_id = db::ulid();
        let message_id = db::ulid();
        let attachment_id = db::ulid();
        let text = "撤回時要放回這段文字";
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, awaits_idle)
             VALUES (?,?,'web','queued','pending',?,?,1)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(text)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, attachments_json, created_at)
             VALUES (?,?,?,'user',?,'web',?,?)",
        )
        .bind(&message_id)
        .bind(&conversation_id)
        .bind(&turn_id)
        .bind(text)
        .bind(
            json!([{ "id": attachment_id, "name": "proof.png", "mime": "image/png", "size": 3, "path": "/tmp/proof.png" }])
                .to_string(),
        )
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, message_id, created_at)
             VALUES (?,?,'proof.png','image/png',3,'/tmp/proof.png','/tmp/proof.png','local',?,?)",
        )
        .bind(&attachment_id)
        .bind(&bot.id)
        .bind(&message_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let response = withdraw_turn(State(app.clone()), Path(turn_id.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body, json!({ "text": text, "attachments": [attachment_id] }));
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(status, "failed");

        let claimed_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, awaits_idle)
             VALUES (?,?,'web','in_flight','pending',?,?,1)",
        )
        .bind(&claimed_id)
        .bind(&conversation_id)
        .bind(text)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let response = withdraw_turn(State(app.clone()), Path(claimed_id.clone()))
            .await
            .unwrap_err()
            .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&claimed_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(status, "in_flight");
    }
}

async fn abandon_turn(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    lifecycle::abandon_turn(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

/// `before=` 游標指不到東西的 404（#766）。`reason` 是機器可讀的：`before_message_gone`（被刪掉或根本沒有）、
/// `before_message_not_in_conversation`（是別顆 bot／別個專案的訊息）。呼叫端的正解都是重載第一頁，不是重試。
pub(crate) fn cursor_not_found(reason: &str, message_id: &str) -> LcError {
    LcError::NotFoundValue(json!({"error": "not_found", "what": "before message", "reason": reason, "message_id": message_id}))
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
    // 游標必須是這段對話裡還在的訊息：被刪掉的或別顆 bot 的一律 404＋reason，不拿全域 rowid 默默切出錯的一頁（#766）。
    let before_rowid = match q.get("before") {
        Some(b) => {
            let at: Option<(i64, String)> = sqlx::query_as("SELECT rowid, conversation_id FROM messages WHERE id = ?")
                .bind(b)
                .fetch_optional(&app.db)
                .await
                .map_err(any_err)?;
            match at {
                None => return Err(cursor_not_found("before_message_gone", b)),
                Some((_, c)) if c != conv => return Err(cursor_not_found("before_message_not_in_conversation", b)),
                Some((rowid, _)) => Some(rowid),
            }
        }
        None => None,
    };
    // `turn_id` / `role`：前端要證明某個回合是不是群組回覆（帶 `group_id` 的 user 訊息），沒有它就得
    // 翻整段歷史猜。只在同一個 conversation 底下過濾，查不到的 turn 是空清單不是 404。
    let turn_filter = q.get("turn_id").map(|s| s.as_str()).filter(|s| !s.is_empty());
    let role_filter = match q.get("role").map(|s| s.as_str()).filter(|s| !s.is_empty()) {
        // 靜默忽略會讓呼叫端以為過濾過了，拿整段當成某個 role 的全部。
        Some(r) if !["user", "assistant", "system"].contains(&r) => return Err(LcError::Bad(format!("bad role `{r}`"))),
        other => other,
    };
    let mut sql = String::from("SELECT *, rowid AS seq FROM messages WHERE conversation_id=?");
    if before_rowid.is_some() {
        sql.push_str(" AND rowid < ?");
    }
    if turn_filter.is_some() {
        sql.push_str(" AND turn_id = ?");
    }
    if role_filter.is_some() {
        sql.push_str(" AND role = ?");
    }
    sql.push_str(" ORDER BY rowid DESC LIMIT ?");
    let mut query = sqlx::query_as::<_, db::Message>(&sql).bind(&conv);
    if let Some(b) = before_rowid {
        query = query.bind(b);
    }
    if let Some(t) = turn_filter {
        query = query.bind(t);
    }
    if let Some(r) = role_filter {
        query = query.bind(r);
    }
    let rows = query.bind(limit + 1).fetch_all(&app.db).await.map_err(any_err)?;
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
    if !ct_eq(q.get("token").map(|s| s.as_str()).unwrap_or(""), &app.ui_token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let since: Option<u64> = q.get("since").and_then(|s| s.parse().ok());
    ws.on_upgrade(move |socket| ws_loop(app, socket, since))
}

async fn ws_loop(app: Arc<App>, mut socket: WebSocket, since: Option<u64>) {
    let mut rx = app.subscribe();
    #[cfg(test)]
    crate::lifecycle::race_point::hit("ws_subscribed", &app.data_dir.display().to_string()).await;
    // **先訂閱再讀環**是對的：反過來的話兩者之間的事件誰都收不到。代價是重複——那一段時間送出的耐久事件
    // 兩邊都有（`emit` 把推進環與 `bus.send` 包在同一把 ring 鎖裡，而我們的 `backlog` 正在等那把鎖），
    // 同一個 `seq` 會送兩次。客戶端沒有 seq 去重，而 `bots_restart_progress` 這類 handler 是純累加
    // （`restartBatch.ts` 的 `done + 1`），重複等於「又發生了一次」：進度會超前甚至衝過總數（issue #521）。
    //
    // 所以這條連線自己記「backlog 送到哪」，之後低於它的即時幀跳過＝**同一個 seq 在一條連線上只送一次**。
    // 從 0 起算、只被真的送出去的 backlog 幀推高，刻意**不拿 `since` 當起點**：daemon 重啟後 seq 從頭來，
    // 客戶端帶著舊的大 `since` 重連會落到下面的 resync 分支（那時一幀 backlog 都沒送），拿 `since` 當起點
    // 會把新 daemon 那些號碼很小的即時幀全部擋掉（issue #368 要補的正是那段）。
    let mut sent_through = 0u64;
    let backlog = match since {
        Some(s) => app.backlog(s).await,
        None => Some(vec![]),
    };
    match backlog {
        Some(evs) => {
            for e in evs {
                sent_through = sent_through.max(e.seq);
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
    // 沒事件也定期送一幀（issue #760）：不佔 seq、不進重播環，客戶端只拿它當「線還活著」的證據。
    let every = app.ws_ping_every();
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if socket.send(WsMessage::Text(json!({"type": "ping"}).to_string().into())).await.is_err() { return; }
            }
            ev = rx.recv() => match ev {
                // backlog 已經送過這一則（訂閱與讀環之間送出的，兩邊都有）：跳過，不是漏送（issue #521）。
                Ok(e) if e.seq <= sent_through => {}
                Ok(e) => {
                    sent_through = e.seq;
                    if socket.send(WsMessage::Text(serde_json::to_string(&e).unwrap().into())).await.is_err() { return; }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // 帶 seq，跟重連那條一樣（API.md §8：`{"type":"resync","seq":12}`）——少了它，
                    // 客戶端在 lag 之後的 `lastSeq` 會停在漏掉的事件之前，下一次重連白跑一趟 backlog（issue #482）。
                    let _ = socket
                        .send(WsMessage::Text(json!({"type": "resync", "seq": app.current_seq()}).to_string().into()))
                        .await;
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
mod ws_shutdown_tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// review 2026-09-16（deliv「沒把握」）：`ws_loop` 沒有 shutdown 通道，開著分頁時 SIGTERM 會不會卡在 graceful shutdown？
    /// 實測不會：axum 0.8 的 `serve` 不追蹤已經 upgrade 的連線，`with_graceful_shutdown` 照樣立刻回來，之後 runtime 收掉
    /// `ws_loop`。這條釘住它——哪天換成會等 upgrade 連線的 server，就得真的給 `ws_loop` 一條 shutdown 通道。
    #[tokio::test]
    async fn an_open_websocket_does_not_hold_up_a_graceful_shutdown() {
        let env = crate::testing::env().await;
        let router = super::router(env.app.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /ws?token=test-token HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 1024];
        let n = client.read(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf[..n]);
        assert!(head.starts_with("HTTP/1.1 101"), "要真的是一條開著的 websocket：{head}");

        tx.send(()).unwrap();
        let done = tokio::time::timeout(std::time::Duration::from_secs(3), server).await;
        assert!(done.is_ok(), "開著的 websocket 讓 graceful shutdown 卡住了");
        drop(client);
    }
}

/// issue #521：`ws_loop` 先訂閱再讀環，兩者之間送出的耐久事件兩邊都有——同一條連線上同一個 `seq` 只能送一次。
#[cfg(test)]
mod ws_backlog_dedupe_tests {
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 讀到 `\r\n\r\n` 為止，一次一個 byte：整塊讀會把後面那些 WS 幀的位元組一起吃掉。
    async fn read_handshake(s: &mut tokio::net::TcpStream) -> String {
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).await.unwrap();
            head.push(b[0]);
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    /// server→client 的 text 幀沒有 mask；長度 126 以上才有 16-bit 的延伸長度。
    async fn read_text_frame(s: &mut tokio::net::TcpStream) -> serde_json::Value {
        let mut hdr = [0u8; 2];
        s.read_exact(&mut hdr).await.unwrap();
        assert_eq!(hdr[0] & 0x0f, 1, "只預期 text 幀，拿到 opcode {:x}", hdr[0] & 0x0f);
        let mut len = (hdr[1] & 0x7f) as usize;
        if len == 126 {
            let mut ext = [0u8; 2];
            s.read_exact(&mut ext).await.unwrap();
            len = u16::from_be_bytes(ext) as usize;
        }
        let mut buf = vec![0u8; len];
        s.read_exact(&mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    /// 修正前：那一則落在 backlog 與即時串流兩邊，客戶端連收兩次同一個 `seq`——而
    /// `bots_restart_progress` 這類 handler 是純累加，重複就是多算一次。
    #[tokio::test]
    async fn an_event_emitted_between_subscribe_and_backlog_is_sent_once() {
        let env = crate::testing::env().await;
        // 環裡先有一則，`since` 才有東西可以往後接。
        env.app.emit("bot_changed", json!({"mark": "before"})).await;
        let since = env.app.current_seq();

        // 訂閱之後、讀環之前送出的那一則。key 用 data-dir，平行測試不會互相觸發。
        let app2 = env.app.clone();
        crate::lifecycle::race_point::arm("ws_subscribed", &env.app.data_dir.display().to_string(), move || async move {
            app2.emit("bot_changed", json!({"mark": "in_window"})).await;
        });

        let router = super::router(env.app.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET /ws?token=test-token&since={since} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        client.write_all(req.as_bytes()).await.unwrap();
        let head = read_handshake(&mut client).await;
        assert!(head.starts_with("HTTP/1.1 101"), "要真的升級成 websocket：{head}");

        // 第一幀＝backlog 裡的那一則（沒有它就是漏送，不是重複）。
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), read_text_frame(&mut client)).await.unwrap();
        assert_eq!(first["data"]["mark"], "in_window", "窗口裡那一則要從 backlog 送出來：{first}");
        let first_seq = first["seq"].as_u64().unwrap();

        // 哨兵在第一幀之後才送，所以它一定排在「重複的那一則」後面：下一幀是誰就見分曉。
        env.app.emit("bot_changed", json!({"mark": "sentinel"})).await;
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), read_text_frame(&mut client)).await.unwrap();
        assert_eq!(second["data"]["mark"], "sentinel", "同一個 seq 被送第二次（backlog 一次、即時串流一次）：{second}");
        assert!(second["seq"].as_u64().unwrap() > first_seq, "哨兵的 seq 要比前一則大：{second}");
        drop(client);
    }
}

/// issue #760：瀏覽器沒辦法看到 WebSocket 的 ping 控制幀，所以 daemon 定期送一個 text 幀 `{"type":"ping"}`，
/// 讓客戶端有「連線還活著」的證據；半開連線（睡眠、NAT／tailscale 逾時）下它收不到，就能判定斷線。
#[cfg(test)]
mod ws_heartbeat_tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn read_handshake(s: &mut tokio::net::TcpStream) {
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).await.unwrap();
            head.push(b[0]);
        }
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));
    }

    async fn read_text_frame(s: &mut tokio::net::TcpStream) -> serde_json::Value {
        let mut hdr = [0u8; 2];
        s.read_exact(&mut hdr).await.unwrap();
        assert_eq!(hdr[0] & 0x0f, 1, "只預期 text 幀，拿到 opcode {:x}", hdr[0] & 0x0f);
        let mut len = (hdr[1] & 0x7f) as usize;
        if len == 126 {
            let mut ext = [0u8; 2];
            s.read_exact(&mut ext).await.unwrap();
            len = u16::from_be_bytes(ext) as usize;
        }
        let mut buf = vec![0u8; len];
        s.read_exact(&mut buf).await.unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    async fn connect(app: &std::sync::Arc<crate::state::App>) -> tokio::net::TcpStream {
        let router = super::router(app.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /ws?token=test-token HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
            .await
            .unwrap();
        read_handshake(&mut client).await;
        client
    }

    /// 沒有任何事件也要定期有幀：靜止的連線若不送東西，客戶端分不出「沒事」與「線已經斷了」。
    #[tokio::test]
    async fn an_idle_socket_gets_a_ping_frame_without_a_seq() {
        let env = crate::testing::env().await;
        env.app.set_ws_ping_every(std::time::Duration::from_millis(60));
        let mut client = connect(&env.app).await;
        for _ in 0..2 {
            let ping = tokio::time::timeout(std::time::Duration::from_secs(5), read_text_frame(&mut client)).await.unwrap();
            assert_eq!(ping["type"], "ping", "{ping}");
            assert!(ping.get("seq").is_none(), "ping 不佔 seq（不能推動客戶端的 lastSeq／重播環）：{ping}");
        }
        assert_eq!(env.app.current_seq(), 0, "ping 不是事件，不能讓 seq 前進");
        drop(client);
    }

    /// 有事件在跑時 ping 照送，事件不被 ping 吞掉或換序。
    #[tokio::test]
    async fn events_still_arrive_between_pings() {
        let env = crate::testing::env().await;
        env.app.set_ws_ping_every(std::time::Duration::from_millis(60));
        let mut client = connect(&env.app).await;
        env.app.emit("bot_changed", serde_json::json!({"mark": "x"})).await;
        let mut got_event = false;
        for _ in 0..5 {
            let f = tokio::time::timeout(std::time::Duration::from_secs(5), read_text_frame(&mut client)).await.unwrap();
            if f["type"] == "bot_changed" {
                got_event = true;
                break;
            }
            assert_eq!(f["type"], "ping");
        }
        assert!(got_event);
        drop(client);
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

    /// issue #112：`valid_bot_name` 允許任意 Unicode（`chars().count() <= 32`，非 ASCII 也合法），
    /// 但撞名時原本按**位元組**長度截斷——29 個 ASCII 字元後面接一個多位元組字元、剛好落在
    /// `room`（byte）那一刀中間就會直接 panic（"byte index N is not a char boundary"）。
    #[test]
    fn next_free_name_does_not_panic_on_a_multibyte_boundary() {
        let name = format!("{}中", "a".repeat(29));
        assert_eq!(name.chars().count(), 30, "valid_bot_name 用字元數判，這個名字合法");
        let taken = |n: &str| n == name;
        let out = next_free_name(&name, &taken);
        assert!(out.chars().count() <= 32, "{out}");
        assert_ne!(out, name);
    }

    /// 中文名字整段截斷也不能把字切一半（亂碼），比照 `fork.rs::default_name`／
    /// `default_session.rs::imported_name` 以字元為單位截斷——是「32 個字元」的字元預算，
    /// 不是位元組，太保守（例如 3 位元組的中文只留 10 個字）也算沒修對。
    #[test]
    fn next_free_name_truncates_on_char_boundaries_for_cjk_names() {
        let long = "審".repeat(40);
        let taken = |n: &str| n == long;
        let out = next_free_name(&long, &taken);
        assert_eq!(out, format!("{}-1", "審".repeat(30)), "{out}");
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
            Err(LcError::NotFoundValue(_))
        ));
    }

    /// 同一毫秒的訊息、id 不照插入序：每一則帶 `seq`（SQLite rowid，單調遞增），前端在 created_at 相同時靠它排，
    /// 不必退回 id（ULID 的隨機段在同一毫秒內不單調）。REST 分頁、群組時間軸與 WS `message_added` 三條路都要帶。
    #[tokio::test]
    async fn every_message_carries_the_monotonic_insert_seq_on_rest_group_and_ws() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let bot = crate::testing::claude_bot(&app, &e.project_id, "seq-bot").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let at = db::now();
        for id in ["m3", "m1", "m2"] {
            sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?, 'user', ?, 'web', ?)")
                .bind(id)
                .bind(&conv)
                .bind(id)
                .bind(&at)
                .execute(&app.db)
                .await
                .unwrap();
        }
        let seqs = |v: &Value| -> Vec<(String, i64)> {
            v["messages"].as_array().unwrap().iter().map(|m| (m["id"].as_str().unwrap().to_string(), m["seq"].as_i64().unwrap_or(-1))).collect()
        };
        let Json(body) = get_messages(State(app.clone()), Path(bot.id.clone()), Query(HashMap::new())).await.unwrap();
        let rest = seqs(&body);
        assert_eq!(rest.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(), ["m3", "m1", "m2"], "REST 照插入序");
        assert!(rest.windows(2).all(|w| w[0].1 >= 0 && w[0].1 < w[1].1), "REST 的 seq 要單調遞增：{rest:?}");
        let rowid: i64 = sqlx::query_scalar("SELECT rowid FROM messages WHERE id='m1'").fetch_one(&app.db).await.unwrap();
        assert_eq!(rest[1].1, rowid, "seq 就是 rowid，跟 before= 分頁用的同一把尺");

        let Json(group) = get_project_messages(State(app.clone()), Path(e.project_id.clone()), Query(HashMap::new())).await.unwrap();
        assert_eq!(seqs(&group), rest, "群組時間軸帶同一個 seq");

        let mut rx = app.subscribe();
        let m = crate::lifecycle::insert_message(&app, &conv, None, "assistant", "new", "hook", false, None).await.unwrap();
        let mut pushed = None;
        while let Ok(ev) = rx.try_recv() {
            if ev.kind == "message_added" && ev.data["message"]["id"] == json!(m.id) {
                pushed = ev.data["message"]["seq"].as_i64();
            }
        }
        let last = rest.last().unwrap().1;
        assert!(pushed.is_some_and(|s| s > last), "message_added 的 seq 要比既有的大：{pushed:?} vs {last}");
    }

    async fn a_bot_with_conv(e: &crate::testing::Env, name: &str) -> (String, String) {
        let id = db::ulid();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?,'claude','tok',?)")
            .bind(&id)
            .bind(&e.project_id)
            .bind(name)
            .bind(db::now())
            .execute(&e.app.db)
            .await
            .unwrap();
        let conv = db::conversation_id(&e.app.db, &id).await.unwrap();
        (id, conv)
    }

    async fn a_turn(db: &sqlx::SqlitePool, conv: &str, id: &str) {
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','completed','ok',?)")
            .bind(id)
            .bind(conv)
            .bind(db::now())
            .execute(db)
            .await
            .unwrap();
    }

    async fn a_message(db: &sqlx::SqlitePool, conv: &str, id: &str, turn: &str, role: &str, group: Option<&str>) {
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, created_at) VALUES (?,?,?,?,?, 'web', ?, ?)",
        )
        .bind(id)
        .bind(conv)
        .bind(turn)
        .bind(role)
        .bind(id)
        .bind(group)
        .bind(db::now())
        .execute(db)
        .await
        .unwrap();
    }

    async fn not_found_reason(r: Result<Json<Value>, LcError>) -> Option<String> {
        let resp = r.err().expect("應該是錯誤").into_response();
        if resp.status() != StatusCode::NOT_FOUND {
            return None;
        }
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()["reason"].as_str().map(str::to_string)
    }

    /// #766：游標指到已經被刪掉的訊息、或別顆 bot 的訊息，不能 400 也不能默默用全域 rowid 切出錯的一頁，
    /// 一律 404 並帶 reason，呼叫端才知道要重載而不是重試。
    #[tokio::test]
    async fn a_before_cursor_that_is_gone_or_foreign_is_a_404_with_a_reason() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let (mine, my_conv) = a_bot_with_conv(&e, "cursor-mine").await;
        let (_other, other_conv) = a_bot_with_conv(&e, "cursor-other").await;
        a_turn(&app.db, &my_conv, "t-mine").await;
        a_turn(&app.db, &other_conv, "t-other").await;
        a_message(&app.db, &my_conv, "m-mine-1", "t-mine", "user", None).await;
        a_message(&app.db, &other_conv, "m-other-1", "t-other", "user", None).await;
        a_message(&app.db, &my_conv, "m-mine-2", "t-mine", "assistant", None).await;
        a_message(&app.db, &my_conv, "m-gone", "t-mine", "assistant", None).await;
        sqlx::query("DELETE FROM messages WHERE id='m-gone'").execute(&app.db).await.unwrap();

        let page = |before: &str| Query(HashMap::from([("before".to_string(), before.to_string())]));
        // 自己的訊息照舊。
        let Json(ok) = get_messages(State(app.clone()), Path(mine.clone()), page("m-mine-2")).await.unwrap();
        assert_eq!(ok["messages"].as_array().unwrap().len(), 1);
        assert_eq!(not_found_reason(get_messages(State(app.clone()), Path(mine.clone()), page("m-gone")).await).await.as_deref(), Some("before_message_gone"));
        assert_eq!(not_found_reason(get_messages(State(app.clone()), Path(mine.clone()), page("never-existed")).await).await.as_deref(), Some("before_message_gone"));
        assert_eq!(
            not_found_reason(get_messages(State(app.clone()), Path(mine.clone()), page("m-other-1")).await).await.as_deref(),
            Some("before_message_not_in_conversation")
        );

        // 群組時間軸同一條規則：訊息要屬於這個專案的某顆 bot（含已刪的：游標在清單載入後才刪 bot 照樣能翻）。
        let project = e.project_id.clone();
        let Json(ok) = crate::group::messages(&app, &project, Some("m-mine-2"), 10).await.map(Json).unwrap();
        assert!(ok["messages"].as_array().unwrap().iter().any(|m| m["id"] == "m-other-1"));
        let group = |r: crate::lifecycle::LcResult<Value>| r.map(Json);
        assert_eq!(not_found_reason(group(crate::group::messages(&app, &project, Some("m-gone"), 10).await)).await.as_deref(), Some("before_message_gone"));
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p-elsewhere','/tmp/elsewhere','e',?)").bind(db::now()).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b-elsewhere','p-elsewhere','x','claude','tok',?)").bind(db::now()).execute(&app.db).await.unwrap();
        let conv_elsewhere = db::conversation_id(&app.db, "b-elsewhere").await.unwrap();
        a_turn(&app.db, &conv_elsewhere, "t-else").await;
        a_message(&app.db, &conv_elsewhere, "m-elsewhere", "t-else", "user", None).await;
        assert_eq!(
            not_found_reason(group(crate::group::messages(&app, &project, Some("m-elsewhere"), 10).await)).await.as_deref(),
            Some("before_message_not_in_conversation")
        );
    }

    async fn deleted_bot_with_hits(e: &crate::testing::Env, name: &str, hits: usize) -> String {
        let (id, conv) = a_bot_with_conv(e, name).await;
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&id).execute(&e.app.db).await.unwrap();
        let turn = format!("t-{name}");
        a_turn(&e.app.db, &conv, &turn).await;
        for i in 0..hits {
            a_message(&e.app.db, &conv, &format!("{name}-{i}"), &turn, "user", None).await;
        }
        id
    }

    /// #766：已刪 bot 仍在結果裡（AGM 查證據要），但不能用較多的命中數把活 bot 擠出 `limit`；每列帶 `bot_deleted`。
    #[tokio::test]
    async fn search_keeps_live_bots_ahead_of_deleted_ones() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let (live, live_conv) = a_bot_with_conv(&e, "needle-live").await;
        a_turn(&app.db, &live_conv, "t-live").await;
        a_message(&app.db, &live_conv, "needle-live-0", "t-live", "user", None).await;
        let big = deleted_bot_with_hits(&e, "needle-big", 5).await;
        let mid = deleted_bot_with_hits(&e, "needle-mid", 3).await;

        let search = |limit| search_messages(State(app.clone()), Query(SearchQuery { q: Some("needle".into()), limit: Some(limit) }));
        let Json(two) = search(2).await.unwrap();
        let rows = two["bots"].as_array().unwrap();
        assert_eq!(rows.iter().map(|r| r["bot_id"].as_str().unwrap()).collect::<Vec<_>>(), [live.as_str(), big.as_str()]);
        assert_eq!(rows.iter().map(|r| r["bot_deleted"].as_bool().unwrap()).collect::<Vec<_>>(), [false, true]);
        let Json(all) = search(10).await.unwrap();
        assert_eq!(all["bots"].as_array().unwrap().iter().map(|r| r["bot_id"].as_str().unwrap()).collect::<Vec<_>>(), [live.as_str(), big.as_str(), mid.as_str()]);
    }

    /// 同一顆 bot 兩則命中同一毫秒：snippet 取後寫入的那則（rowid 較大），不是任一則。
    #[tokio::test]
    async fn search_snippet_is_the_last_written_hit_when_timestamps_tie() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let (_bot, conv) = a_bot_with_conv(&e, "tie-bot").await;
        let at = db::now();
        for (id, text) in [("zz-first", "tiehit 先寫的"), ("aa-second", "tiehit 後寫的")] {
            sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'user',?,'web',?)")
                .bind(id).bind(&conv).bind(text).bind(&at).execute(&app.db).await.unwrap();
        }
        let Json(r) = search_messages(State(app), Query(SearchQuery { q: Some("tiehit".into()), limit: None })).await.unwrap();
        assert_eq!(r["bots"][0]["hits"], 2);
        assert!(r["bots"][0]["snippet"].as_str().unwrap().contains("後寫的"), "{r}");
    }

    /// 前端要判斷「這個回合是不是群組回覆」：只問那個 turn 的 user 訊息，不必翻整段歷史。
    #[tokio::test]
    async fn turn_id_and_role_filter_the_page() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let (bot_id, conv) = a_bot_with_conv(&e, "filter-bot").await;
        a_turn(&app.db, &conv, "t-old").await;
        a_turn(&app.db, &conv, "t-group").await;
        a_message(&app.db, &conv, "m-old", "t-old", "user", None).await;
        a_message(&app.db, &conv, "m-group", "t-group", "user", Some("grp-1")).await;
        // 群組 prompt 後面跟著一長串 assistant：以前靠翻頁找就會被擠出去。
        for i in 0..250 {
            a_message(&app.db, &conv, &format!("m-a{i:03}"), "t-group", "assistant", None).await;
        }

        let q = HashMap::from([("turn_id".to_string(), "t-group".to_string()), ("role".to_string(), "user".to_string())]);
        let Json(body) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["id"], "m-group");
        assert_eq!(messages[0]["group_id"], "grp-1");
        assert_eq!(body["has_more"], false);

        // turn 只過濾 turn，不順便挑 role。
        let q = HashMap::from([("turn_id".to_string(), "t-group".to_string()), ("limit".to_string(), "500".to_string())]);
        let Json(body) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(251));

        // 別的回合、別顆 bot 的同名 turn 都查不到（查詢限定在這個 conversation）。
        let q = HashMap::from([("turn_id".to_string(), "t-nope".to_string())]);
        let Json(body) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(0));
        let (other_id, other_conv) = a_bot_with_conv(&e, "other-bot").await;
        a_turn(&app.db, &other_conv, "t-other").await;
        a_message(&app.db, &other_conv, "m-other", "t-other", "user", Some("grp-9")).await;
        let q = HashMap::from([("turn_id".to_string(), "t-group".to_string()), ("role".to_string(), "user".to_string())]);
        let Json(body) = get_messages(State(app.clone()), Path(other_id.clone()), Query(q)).await.unwrap();
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(0));
        let q = HashMap::from([("turn_id".to_string(), "t-other".to_string()), ("role".to_string(), "user".to_string())]);
        let Json(body) = get_messages(State(app.clone()), Path(other_id), Query(q)).await.unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["id"], "m-other");

        // 沒帶過濾＝現狀（整段、依插入順序）。
        let q = HashMap::from([("limit".to_string(), "500".to_string())]);
        let Json(body) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(252));

        // role 打錯要說，不能默默當作沒過濾。
        let q = HashMap::from([("role".to_string(), "assistant ".to_string())]);
        assert!(matches!(
            get_messages(State(app), Path(bot_id), Query(q)).await,
            Err(LcError::Bad(msg)) if msg.contains("role")
        ));
    }

    /// 同一回合的 user 訊息超過一頁：`before` 要能在過濾後繼續往前翻。
    #[tokio::test]
    async fn filtered_pages_keep_paging_with_before() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let (bot_id, conv) = a_bot_with_conv(&e, "filter-page-bot").await;
        a_turn(&app.db, &conv, "t-g").await;
        a_message(&app.db, &conv, "u-group", "t-g", "user", Some("grp-1")).await;
        for i in 0..3 {
            a_message(&app.db, &conv, &format!("u-more{i}"), "t-g", "user", None).await;
        }

        let q = HashMap::from([
            ("turn_id".to_string(), "t-g".to_string()),
            ("role".to_string(), "user".to_string()),
            ("limit".to_string(), "2".to_string()),
        ]);
        let Json(first) = get_messages(State(app.clone()), Path(bot_id.clone()), Query(q)).await.unwrap();
        assert_eq!(first["has_more"], true);
        let oldest = first["messages"][0]["id"].as_str().unwrap().to_string();
        assert!(first["messages"].as_array().unwrap().iter().all(|m| m["group_id"].is_null()));

        let q = HashMap::from([
            ("turn_id".to_string(), "t-g".to_string()),
            ("role".to_string(), "user".to_string()),
            ("limit".to_string(), "2".to_string()),
            ("before".to_string(), oldest),
        ]);
        let Json(second) = get_messages(State(app), Path(bot_id), Query(q)).await.unwrap();
        assert_eq!(second["has_more"], false);
        assert_eq!(second["messages"][0]["id"], "u-group");
        assert_eq!(second["messages"][0]["group_id"], "grp-1");
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

    /// user bot 由 config.toml 管：測試要把它寫進 config，刪除才會在 TOML 裡找到目標。
    async fn in_config(e: &crate::testing::Env, bots: &[(&str, &str)]) {
        let (pid, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
        let bots: Vec<crate::config::BotCfg> = bots
            .iter()
            .map(|(id, name)| {
                toml::from_str(&format!("id = '{id}'\nname = '{name}'\nkind = 'claude'\n")).unwrap()
            })
            .collect();
        e.app
            .cfg
            .update(move |cfg| {
                cfg.projects = vec![crate::config::ProjectCfg {
                    handed_off_to: None,
                    id: Some(pid),
                    path: repo,
                    label: "proj".into(),
                    host: crate::config::LOCAL_HOST.into(),
                    bots,
                }];
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn a_running_run(app: &Arc<App>, bot_id: &str) -> String {
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','pane-x','agent','no-such-session',?)",
        )
        .bind(&run)
        .bind(bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        run
    }

    fn reason(e: LcError) -> String {
        match e {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
            other => format!("{other:?}"),
        }
    }

    /// 閘門沒過就什麼都不動：不停 bot、不停也不刪 child、不 purge、config 不寫（sol 三輪）。
    /// 1 個專案、2 顆 bot，TOML 被外部拿掉 b2 之後刪 b1：小資料集也不能靠「沒到門檻」放行。
    #[tokio::test]
    async fn a_refused_delete_stops_and_removes_nothing() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        let b2 = a_bot(&e, "bravo", "user").await;
        let child = a_bot(&e, "alfa-kid", "child").await;
        sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&b1).bind(&child).execute(&app.db).await.unwrap();
        let (r1, rc) = (a_running_run(&app, &b1).await, a_running_run(&app, &child).await);
        in_config(&e, &[(&b1, "alfa")]).await; // bravo 被外部拿掉了

        let err = delete_bot(State(app.clone()), Path(b1.clone())).await.unwrap_err();
        assert_eq!(reason(err), "delete_refused");
        for (bot, run) in [(&b1, &r1), (&child, &rc)] {
            assert!(db::bot(&app.db, bot).await.unwrap().unwrap().deleted_at.is_none(), "{bot} 不該被刪");
            assert_eq!(db::run(&app.db, run).await.unwrap().unwrap().state, "running", "{bot} 不該被停");
        }
        assert!(db::bot(&app.db, &b2).await.unwrap().unwrap().deleted_at.is_none());
        assert!(app.cfg.get().await.projects[0].bots.iter().any(|b| b.id.as_deref() == Some(b1.as_str())), "config 沒寫");
    }

    /// 停機期間外部改 TOML：定案已經在停機之前做完，所以不會 409、不會留下「已停、未刪」（sol 四輪）。
    /// 舊的「預檢 → 停 → 再定案」在這裡會因為 bravo 不見了而 409，留下停掉卻沒刪的 alfa。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_toml_changing_while_the_bot_stops_does_not_strand_it() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let (b1, b2) = (a_bot(&e, "alfa", "user").await, a_bot(&e, "bravo", "user").await);
        in_config(&e, &[(&b1, "alfa"), (&b2, "bravo")]).await;
        let run = crate::testing::fake_run(&app, &b1).await; // session `test`＝mock herdr，stop 會真的送鍵

        let task = tokio::spawn({
            let (app, b1) = (app.clone(), b1.clone());
            async move { delete_bot(State(app), Path(b1)).await.map(|_| ()).map_err(reason) }
        });
        let _ = crate::testing::eventually!(e.herdr.methods().iter().any(|m| m == "agent.send_keys"));
        assert!(e.herdr.methods().iter().any(|m| m == "agent.send_keys"), "stop 應該已經開始");
        // 停機進行中，外面把 bravo 從 TOML 拿掉。
        let text = std::fs::read_to_string(&app.cfg.path).unwrap();
        let pruned: crate::config::ConfigFile = {
            let mut c: crate::config::ConfigFile = toml::from_str(&text).unwrap();
            c.projects[0].bots.retain(|b| b.id.as_deref() != Some(b2.as_str()));
            c
        };
        std::fs::write(&app.cfg.path, toml::to_string(&pruned).unwrap()).unwrap();

        assert_eq!(task.await.unwrap(), Ok(()), "定案在停機前已經做完，不該 409");
        assert!(db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_some(), "alfa 刪掉了");
        assert_ne!(db::run(&app.db, &run).await.unwrap().unwrap().state, "running", "而且停了");
        assert!(db::bot(&app.db, &b2).await.unwrap().unwrap().deleted_at.is_none(), "bravo 沒被順手刪掉");
    }

    /// 刪專案與 start 並發：start 的關鍵段是「拿 per-bot 鎖 → 確認沒被刪 → 建 run」（`start_bot_locked_with`，
    /// 中間還有開 workspace 那段時間）。刪專案若在鎖外檢查 stopped，會刪掉剛啟動的 bot（sol 四輪）。
    /// 不變式：不會出現「bot 已刪、run 還在跑」。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_project_delete_never_removes_a_bot_that_just_started() {
        for round in 0..10 {
            let e = crate::testing::env().await;
            let app = e.app.clone();
            let b1 = a_bot(&e, "alfa", "user").await;
            in_config(&e, &[(&b1, "alfa")]).await;

            let gate = Arc::new(tokio::sync::Barrier::new(2));
            let starter = tokio::spawn({
                let (app, b1, gate) = (app.clone(), b1.clone(), gate.clone());
                async move {
                    gate.wait().await;
                    let lock = app.bot_lock(&b1).await;
                    let _g = lock.lock().await;
                    if db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_some() {
                        return false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await; // 開 workspace 那段
                    a_running_run(&app, &b1).await;
                    true
                }
            });
            let deleter = tokio::spawn({
                let (app, pid, gate) = (app.clone(), e.project_id.clone(), gate.clone());
                async move {
                    gate.wait().await;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    delete_project(State(app), Path(pid)).await.map(|_| ()).map_err(reason)
                }
            });
            let started = starter.await.unwrap();
            let deleted = deleter.await.unwrap();
            let bot_gone = db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_some();
            let running = db::active_run(&app.db, &b1).await.unwrap().is_some();
            assert!(!(bot_gone && running), "round {round}: 刪掉了正在跑的 bot（started={started} deleted={deleted:?}）");
            assert_eq!(started, running, "round {round}");
            assert_eq!(deleted.is_ok(), bot_gone, "round {round}: {deleted:?}");
        }
    }

    /// child 的軟刪 DB 寫入失敗時，不能繼續砍它的 runtime 目錄：以前那個 `UPDATE` 的結果被
    /// `let _ =` 吃掉，寫不進去也照樣 purge，變成「DB 說它還活著、檔案已經被砍光」，事後救不回來
    /// （issue #87）。整支 API 要跟著失敗，DB 那一列也要維持 `deleted_at IS NULL`。
    #[tokio::test]
    async fn a_child_whose_soft_delete_write_fails_keeps_its_runtime_dir() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;
        let child = a_bot(&e, "alfa-kid", "child").await;

        let dir = app.bot_dir(&child).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"keep me").unwrap();

        // 故障注入：只讓這顆 child 的軟刪 UPDATE 失敗，其他查詢不受影響。
        sqlx::query(&format!(
            "CREATE TRIGGER am_test_fail_child_delete BEFORE UPDATE OF deleted_at ON bots
             WHEN NEW.id = '{child}' BEGIN SELECT RAISE(ABORT, 'boom'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();

        assert!(delete_project(State(app.clone()), Path(e.project_id.clone())).await.is_err(), "DB 寫不進去，API 不能回成功");
        assert!(dir.join("marker").exists(), "DB 寫不進去卻把 runtime 目錄砍了");
        assert!(db::bot(&app.db, &child).await.unwrap().unwrap().deleted_at.is_none(), "DB 沒寫成功，不該說它已經刪除");
    }

    /// #284：第一顆 child 的軟刪寫不進去時，後面的 child 仍要收掉，失敗的那顆背景重試到成功——專案已經定案刪除，
    /// 使用者再按一次只會 not_in_config，不能留下永遠沒人回收的活 bot。
    #[tokio::test]
    async fn a_child_that_fails_to_soft_delete_does_not_strand_its_siblings() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;
        let mut kids = vec![];
        for n in ["alfa-k1", "alfa-k2", "alfa-k3"] {
            kids.push(a_bot(&e, n, "child").await);
        }
        sqlx::query(&format!(
            "CREATE TRIGGER am_test_fail_first BEFORE UPDATE OF deleted_at ON bots WHEN NEW.id = '{}' BEGIN SELECT RAISE(ABORT, 'boom'); END",
            kids[0]
        ))
        .execute(&app.db)
        .await
        .unwrap();
        assert!(delete_project(State(app.clone()), Path(e.project_id.clone())).await.is_err());
        for k in &kids[1..] {
            assert!(db::bot(&app.db, k).await.unwrap().unwrap().deleted_at.is_some(), "失敗的那顆後面的 child 也要收掉");
        }
        sqlx::query("DROP TRIGGER am_test_fail_first").execute(&app.db).await.unwrap();
        assert!(crate::testing::eventually!(db::bot(&app.db, &kids[0]).await.unwrap().unwrap().deleted_at.is_some()), "寫得進去之後背景補上");
    }

    /// #296：定案之後某顆 child 的軟刪寫不進去，不能 early return 把母 bot 的 run 留著不停。
    #[tokio::test]
    async fn a_child_soft_delete_failure_after_the_decision_does_not_strand_the_parent_run() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let parent = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&parent, "alfa")]).await;
        let child = a_bot(&e, "alfa-kid", "child").await;
        sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&child).execute(&app.db).await.unwrap();
        crate::testing::fake_run(&app, &parent).await;
        sqlx::query(&format!(
            "CREATE TRIGGER am_test_fail_kid BEFORE UPDATE OF deleted_at ON bots WHEN NEW.id = '{child}' BEGIN SELECT RAISE(ABORT, 'boom'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let res = delete_bot(State(app.clone()), Path(parent.clone())).await;
        assert!(res.is_ok(), "定案之後的失敗記進 kept_dirs，不能回錯：{:?}", res.err().map(|e| format!("{e:?}")));
        assert!(db::active_run(&app.db, &parent).await.unwrap().is_none(), "母 bot 已刪，它的 run 一定要停");
        sqlx::query("DROP TRIGGER am_test_fail_kid").execute(&app.db).await.unwrap();
        assert!(crate::testing::eventually!(db::bot(&app.db, &child).await.unwrap().unwrap().deleted_at.is_some()), "寫得進去之後背景補上");
    }

    /// issue #498：刪專案要把它底下**還開著**的任務一起收掉，並留下為什麼。
    ///
    /// 不收的話任務永遠停在 open（任務沒有軟刪），`workflow::wake_stalled_at` 十分鐘後還會推一則
    /// `mission_next` 要 AGM 去推一個專案與 bot 都不存在的任務——「叫醒之後不再叫」那一半由
    /// `mission::workflow` 的 `a_mission_whose_project_was_deleted_is_not_woken_any_more` 釘。
    #[tokio::test]
    async fn deleting_a_project_cancels_its_open_missions() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        // 專案要在 config.toml 裡，`delete_project` 才肯動（否則 409 not_in_config）。
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;
        let open = crate::mission::store::create(
            &app.db,
            &crate::mission::store::NewMission {
                project_id: &e.project_id,
                client_request_id: "crid-open",
                text: "還開著的",
                delivery_mode: "push_main",
                executor_kind: "claude",
                on_5h_limit: "wait",
                max_rounds: 2,
                parent_mission_id: None,
            },
        )
        .await
        .unwrap()
        .0;
        // 已經結案的那一筆不能被再寫一次（`cancelled_at` 只給還開著的）。
        let done = crate::mission::store::create(
            &app.db,
            &crate::mission::store::NewMission {
                project_id: &e.project_id,
                client_request_id: "crid-done",
                text: "早就完成的",
                delivery_mode: "push_main",
                executor_kind: "claude",
                on_5h_limit: "wait",
                max_rounds: 2,
                parent_mission_id: None,
            },
        )
        .await
        .unwrap()
        .0;
        sqlx::query("UPDATE missions SET completed_at = ?, result_summary = '交付了' WHERE id = ?")
            .bind(db::now())
            .bind(&done.id)
            .execute(&app.db)
            .await
            .unwrap();

        // 底下一件還開著的交辦：任務收了，它也要跟著收（issue #498 的複看：不收的話額度回來時
        // 還會被 dispatch 到已經軟刪的 bot，收成 awaiting_review 再推一則 assignment_failed）。
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &b1, "crid-a1", "做事", &[], None, true).await.unwrap();
        crate::supervisor::store::set_mission_link(&app.db, &a.id, &open.id, "executor").await.unwrap();

        delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap();

        let a_after = crate::supervisor::store::assignment(&app.db, &a.id).await.unwrap().unwrap();
        assert_eq!(a_after.status, "cancelled", "任務收了，底下開著的交辦也要收：{}", a_after.status);

        let after = crate::mission::store::get(&app.db, &open.id).await.unwrap().unwrap();
        assert!(after.cancelled_at.is_some(), "開著的任務要跟著專案收掉");
        let ev = crate::mission::store::events(&app.db, &open.id).await.unwrap();
        let last = ev.last().expect("有事件");
        assert_eq!(last.kind, "cancelled");
        let payload: Value = serde_json::from_str(&last.payload_json).unwrap();
        assert_eq!(payload["reason"], "project_deleted", "為什麼被收要查得到：{payload}");

        let done_after = crate::mission::store::get(&app.db, &done.id).await.unwrap().unwrap();
        assert!(done_after.cancelled_at.is_none() && done_after.completed_at.is_some(), "已經結案的不動它");
        assert_eq!(
            crate::mission::store::events(&app.db, &done.id).await.unwrap().iter().filter(|e| e.kind == "cancelled").count(),
            0,
            "結案的任務不該多一則 cancelled"
        );
    }

    /// #298：專案已刪的 child 不能還原成看不到的活 bot。
    #[tokio::test]
    async fn a_child_of_a_deleted_project_cannot_be_restored() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;
        let child = a_bot(&e, "alfa-kid", "child").await;
        delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap();
        assert!(restore_bot(State(app.clone()), Path(child.clone())).await.is_err());
        assert!(db::bot(&app.db, &child).await.unwrap().unwrap().deleted_at.is_some());
    }

    /// #757：刪母 bot 會連它的 child 一起軟刪；「最近刪除」只列 user bot，復原母 bot 之後 child 仍是已刪、可以再個別復原
    /// （母 bot 活了就過得了 #298 的檢查）。母 bot 復原後沒有 run（不自動重開），清單裡也不再有它。
    #[tokio::test]
    async fn restoring_a_deleted_parent_leaves_its_children_deleted_but_restorable() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let parent = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&parent, "alfa")]).await;
        let kid = a_bot(&e, "alfa-kid", "child").await;
        sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&kid).execute(&app.db).await.unwrap();

        delete_bot(State(app.clone()), Path(parent.clone())).await.unwrap();
        assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_some(), "child 跟著母 bot 一起軟刪");
        let Json(listed) = crate::deleted_bots::list_deleted(State(app.clone())).await.unwrap();
        let ids: Vec<&str> = listed["bots"].as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap()).collect();
        assert_eq!(ids, [parent.as_str()], "只列 user bot：{listed}");

        restore_bot(State(app.clone()), Path(parent.clone())).await.unwrap();
        assert!(db::bot(&app.db, &parent).await.unwrap().unwrap().deleted_at.is_none());
        assert!(db::active_run(&app.db, &parent).await.unwrap().is_none(), "復原不自動開 run");
        assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_some(), "child 不跟著復原（由父 bot／AGM 自己處理）");
        let Json(after) = crate::deleted_bots::list_deleted(State(app.clone())).await.unwrap();
        assert!(after["bots"].as_array().unwrap().is_empty(), "復原後清單空了：{after}");

        restore_bot(State(app.clone()), Path(kid.clone())).await.expect("母 bot 活了，child 可以個別復原");
        assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_none());
    }

    /// #301：delete_bot 定案後、停機／purge 之前，restore_bot 必須等鎖，不能插進來。
    #[tokio::test]
    async fn a_restore_waits_for_the_delete_that_holds_the_bot_lock() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let id = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&id, "alfa")]).await;
        let outcome = Arc::new(std::sync::Mutex::new(None));
        let (a, oid, o2) = (app.clone(), id.clone(), outcome.clone());
        crate::lifecycle::race_point::arm("delete_bot_after_decided", &id, move || async move {
            let r = tokio::time::timeout(std::time::Duration::from_millis(300), restore_bot(State(a), Path(oid))).await;
            *o2.lock().unwrap() = Some(r.is_ok());
        });
        delete_bot(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(*outcome.lock().unwrap(), Some(false), "delete 還持著鎖的時候，restore 不能完成");
        restore_bot(State(app.clone()), Path(id.clone())).await.unwrap();
    }

    /// 輸入框草稿跟著 bot／專案走：刪掉之後 `GET /api/drafts` 不再列它，其他瀏覽器收到清除事件。
    #[tokio::test]
    async fn deleting_a_bot_or_project_clears_its_composer_drafts() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        let b2 = a_bot(&e, "bravo", "user").await;
        in_config(&e, &[(&b1, "alfa"), (&b2, "bravo")]).await;
        let (k1, k2, kg) = (format!("bot:{b1}"), format!("bot:{b2}"), format!("group:{}", e.project_id));
        for k in [&k1, &k2, &kg] {
            crate::drafts::put(&app.db, k, "草稿").await.unwrap();
        }
        let mut rx = app.subscribe();
        delete_bot(State(app.clone()), Path(b1.clone())).await.unwrap();
        let keys: Vec<String> = crate::drafts::list(&app.db).await.unwrap().into_iter().map(|d| d.key).collect();
        assert_eq!(keys, {
            let mut v = vec![k2.clone(), kg.clone()];
            v.sort();
            v
        });
        let mut cleared = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if ev.kind == "draft_updated" {
                assert_eq!(ev.data["text"], "");
                cleared.push(ev.data["key"].as_str().unwrap().to_string());
            }
        }
        assert_eq!(cleared, [k1.clone()]);
        delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap();
        assert!(crate::drafts::list(&app.db).await.unwrap().is_empty(), "專案刪除帶走群組草稿與剩下那顆 bot 的草稿");
    }

    /// #313：刪專案要一併清掉 user bot 的 runtime 目錄；清不掉（遠端主機不明）不能當成清掉了。
    #[tokio::test]
    async fn deleting_a_project_purges_its_user_bot_dirs_and_reports_failures() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;
        let dir = runtime_dir(&app, &b1);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"x").unwrap();
        let out = body_of(delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap()).await;
        assert!(!dir.exists(), "user bot 的目錄要跟著清掉");
        assert!(out.get("kept_dirs").is_none(), "{out}");

        // 遠端專案、主機不明：ssh 清不了，不能回成「清掉了」。
        let e2 = crate::testing::env().await;
        let app2 = e2.app.clone();
        let b2 = a_bot(&e2, "beta", "user").await;
        in_config(&e2, &[(&b2, "beta")]).await;
        sqlx::query("UPDATE projects SET host='ghost' WHERE id=?").bind(&e2.project_id).execute(&app2.db).await.unwrap();
        let out = body_of(delete_project(State(app2.clone()), Path(e2.project_id.clone())).await.unwrap()).await;
        assert_eq!(out["kept_dirs"], json!([{"bot_id": b2, "reason": "purge_failed"}]), "{out}");
    }

    /// 快照（鎖之前列的 bot）之後才被認領進專案的 child：以前不在快照裡，專案定案刪除後它的列仍是 `deleted_at IS NULL`——
    /// UI 看不到、reconcile 也不掃（專案已刪），pane 與目錄永遠沒人收。定案之後要再列一次，連它一起軟刪。
    #[tokio::test]
    async fn a_child_adopted_after_the_snapshot_is_soft_deleted_with_the_project() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;
        let late = db::ulid();
        {
            let (app, pid, late) = (app.clone(), e.project_id.clone(), late.clone());
            // 快照與定案之間（持久 intent 寫完、config 還沒改）：另一條路認領了一顆新的 child。
            crate::lifecycle::race_point::arm("delete_after_intent", &e.project_id, move || async move {
                sqlx::query(
                    "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
                     VALUES (?,?,'late-kid','claude','[]',0,1,'tok','child',?,?)",
                )
                .bind(&late)
                .bind(&pid)
                .bind(&b1)
                .bind(db::now())
                .execute(&app.db)
                .await
                .unwrap();
            });
        }
        delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap();
        assert!(deleted(&app, &late).await, "專案刪了，快照之後才出現的 child 不能留成活列");
    }

    /// 同一個窗口提早一步：列完 bot、還沒拿鎖時被認領的 child。拿到鎖之後重列發現變了，放掉重來，這一顆就進了快照（有 intent、
    /// 有鎖、有「都已停止」的檢查），而不是靠定案之後的補收。
    #[tokio::test]
    async fn a_child_adopted_before_the_locks_are_taken_joins_the_snapshot() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;
        let early = db::ulid();
        {
            let (app, pid, early) = (app.clone(), e.project_id.clone(), early.clone());
            crate::lifecycle::race_point::arm("delete_project_before_lock", &e.project_id, move || async move {
                sqlx::query(
                    "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
                     VALUES (?,?,'early-kid','claude','[]',0,1,'tok','child',?,?)",
                )
                .bind(&early)
                .bind(&pid)
                .bind(&b1)
                .bind(db::now())
                .execute(&app.db)
                .await
                .unwrap();
            });
        }
        delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap();
        assert!(deleted(&app, &early).await);
        let snap: String = sqlx::query_scalar("SELECT payload_json FROM intents WHERE subject_id = ? AND kind = 'delete_project'")
            .bind(&e.project_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(snap.contains(&early), "重列之後它在持久 intent 的快照裡：{snap}");
    }

    // ---- #355 P3：刪除在定案之後（或之前）行程死掉，開機補完 ----

    /// 模擬行程死亡：刪除走到 `point` 就卡住，再把整個 future abort 掉。
    async fn die_in_delete(e: &crate::testing::Env, key: &str, point: &'static str, project: bool) {
        let reached = Arc::new(tokio::sync::Notify::new());
        let r2 = reached.clone();
        crate::lifecycle::race_point::arm(point, key, move || async move {
            r2.notify_one();
            std::future::pending::<()>().await
        });
        let (app, k) = (e.app.clone(), key.to_string());
        let h = tokio::spawn(async move {
            if project {
                delete_project(State(app), Path(k)).await.map(|_| ())
            } else {
                delete_bot(State(app), Path(k)).await.map(|_| ())
            }
        });
        reached.notified().await;
        h.abort();
        let _ = h.await;
    }

    async fn deleted(app: &Arc<App>, id: &str) -> bool {
        db::bot(&app.db, id).await.unwrap().unwrap().deleted_at.is_some()
    }

    async fn intent_states(app: &Arc<App>, subject: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT status FROM intents WHERE subject_id = ? ORDER BY created_at").bind(subject).fetch_all(&app.db).await.unwrap()
    }

    async fn parent_and_child(e: &crate::testing::Env) -> (String, String) {
        let parent = a_bot(e, "alfa", "user").await;
        in_config(e, &[(&parent, "alfa")]).await;
        let kid = a_bot(e, "alfa-kid", "child").await;
        sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&kid).execute(&e.app.db).await.unwrap();
        (parent, kid)
    }

    /// #296：母 bot 已定案刪除、child 還沒處理就死。以前 child 永遠活著（reconcile 不掃已刪母 bot 的 child）；現在開機補完，而且補兩次／併發補都一樣。
    #[tokio::test]
    async fn a_delete_bot_killed_after_the_decision_is_completed_on_boot() {
        let e = crate::testing::env().await;
        let (parent, kid) = parent_and_child(&e).await;
        die_in_delete(&e, &parent, "delete_bot_after_decided", false).await;
        assert!(deleted(&e.app, &parent).await, "定案了");
        assert!(!deleted(&e.app, &kid).await, "child 還活著——這就是 #296 的半套");
        assert_eq!(intent_states(&e.app, &parent).await, vec!["running"]);

        let app2 = crate::testing::restart_app(&e).await;
        crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
        assert!(deleted(&app2, &kid).await, "開機補完：child 收掉了");
        assert_eq!(intent_states(&app2, &parent).await, vec!["done"]);
        crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
        tokio::join!(crate::delete_intents::recover_host(&app2, LOCAL_HOST), crate::delete_intents::recover_host(&app2, LOCAL_HOST));
        assert_eq!(intent_states(&app2, &parent).await, vec!["done"], "冪等");
    }

    /// **#508**：定案之後 daemon 停超過 delete intent 的 TTL（一小時）才開回來。開機對帳的真實順序是
    /// `restart_intents::recover_host`（第一步就是收過期的 intent）→ `delete_intents::recover_host`（補完）。
    /// 以前第一步會把它收成 `failed`＋推 AGM inbox，補完那一步的 `intents::open()` 再也撈不到它，留下
    /// 「母 bot 已軟刪、child 還活著」，而且再按一次刪除只得 404——期限的時鐘在 daemon 死著時照走，
    /// 而 daemon 死著正是 intent 存在的理由。現在要照樣補完。
    #[tokio::test]
    async fn a_delete_interrupted_by_a_daemon_outage_longer_than_the_ttl_is_still_completed_on_boot() {
        let e = crate::testing::env().await;
        let (parent, kid) = parent_and_child(&e).await;
        die_in_delete(&e, &parent, "delete_bot_after_decided", false).await;
        assert!(deleted(&e.app, &parent).await && !deleted(&e.app, &kid).await, "前提：定案了、child 還活著");
        // daemon 躺了一整夜：expires_at 早就過了。
        sqlx::query("UPDATE intents SET expires_at = '2020-01-01T00:00:00.000Z' WHERE subject_id = ?")
            .bind(&parent)
            .execute(&e.app.db)
            .await
            .unwrap();

        let app2 = crate::testing::restart_app(&e).await;
        crate::restart_intents::recover_host(&app2, LOCAL_HOST).await;
        assert_eq!(intent_states(&app2, &parent).await, vec!["running"], "還沒補過就不准收成 failed");
        crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
        assert!(deleted(&app2, &kid).await, "開機補完：child 收掉了");
        assert_eq!(intent_states(&app2, &parent).await, vec!["done"]);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'intent_failed' AND bot_id = ?")
            .bind(&parent)
            .fetch_one(&app2.db)
            .await
            .unwrap();
        assert_eq!(n, 0, "沒有人被通知『補不完』——它補完了");
    }

    /// 定案之前就死：世界沒變（bot 還活著、還在 config），abandoned。
    #[tokio::test]
    async fn a_delete_bot_killed_before_the_decision_is_abandoned() {
        let e = crate::testing::env().await;
        let (parent, kid) = parent_and_child(&e).await;
        die_in_delete(&e, &parent, "delete_after_intent", false).await;
        let app2 = crate::testing::restart_app(&e).await;
        crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
        assert_eq!(intent_states(&app2, &parent).await, vec!["abandoned"]);
        assert!(!deleted(&app2, &parent).await && !deleted(&app2, &kid).await, "什麼都沒刪");
    }

    /// #284：專案已定案刪除、child 還沒軟刪就死。
    #[tokio::test]
    async fn a_delete_project_killed_after_the_decision_is_completed_on_boot() {
        let e = crate::testing::env().await;
        let (_parent, kid) = parent_and_child(&e).await;
        die_in_delete(&e, &e.project_id, "delete_project_after_commit", true).await;
        assert!(!deleted(&e.app, &kid).await, "child 還活著、專案已刪——這就是 #284 的半套");
        let app2 = crate::testing::restart_app(&e).await;
        crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
        assert!(deleted(&app2, &kid).await, "開機補完");
        assert_eq!(intent_states(&app2, &e.project_id).await, vec!["done"]);
    }

    #[tokio::test]
    async fn a_delete_project_killed_before_the_decision_is_abandoned() {
        let e = crate::testing::env().await;
        let (_parent, kid) = parent_and_child(&e).await;
        die_in_delete(&e, &e.project_id, "delete_after_intent", true).await;
        let app2 = crate::testing::restart_app(&e).await;
        crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
        assert_eq!(intent_states(&app2, &e.project_id).await, vec!["abandoned"]);
        assert!(!deleted(&app2, &kid).await);
    }

    /// 補不成（child 的軟刪一直寫不進去）：最多試 MAX_ATTEMPTS 次，failed＋AGM inbox。
    #[tokio::test]
    async fn a_delete_that_can_never_be_completed_gives_up_and_tells_agm() {
        let e = crate::testing::env().await;
        let (parent, kid) = parent_and_child(&e).await;
        die_in_delete(&e, &parent, "delete_bot_after_decided", false).await;
        sqlx::query(&format!(
            "CREATE TRIGGER am_test_kid_stuck BEFORE UPDATE OF deleted_at ON bots WHEN NEW.id = '{kid}' BEGIN SELECT RAISE(ABORT, 'boom'); END"
        ))
        .execute(&e.app.db)
        .await
        .unwrap();
        let app2 = crate::testing::restart_app(&e).await;
        crate::delete_intents::recover_host(&app2, LOCAL_HOST).await;
        let mut st = vec![];
        for _ in 0..200 {
            st = intent_states(&app2, &parent).await;
            if st == vec!["failed"] {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(st, vec!["failed"]);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'intent_failed' AND bot_id = ?").bind(&parent).fetch_one(&app2.db).await.unwrap();
        assert_eq!(n, 1, "AGM inbox 有一則 intent_failed");
    }

    /// intent 寫不進去＝什麼都還沒動：不能定案刪除。
    #[tokio::test]
    async fn a_delete_that_cannot_record_its_intent_decides_nothing() {
        let e = crate::testing::env().await;
        let (parent, _kid) = parent_and_child(&e).await;
        sqlx::query("CREATE TRIGGER am_test_no_intents BEFORE INSERT ON intents BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END").execute(&e.app.db).await.unwrap();
        assert!(delete_bot(State(e.app.clone()), Path(parent.clone())).await.is_err());
        assert!(!deleted(&e.app, &parent).await, "沒定案");
    }

    /// 死鎖回歸（sol 五輪）：child id 字典序**小於** parent。舊寫法 delete_bot 先持 parent、定案後才拿 child，
    /// delete_project 依序先持 child 再等 parent → 互等。現在兩邊都依 id 排序一次拿齊，必須在時限內都結束。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn project_delete_and_parent_delete_never_deadlock() {
        for round in 0..10 {
            let e = crate::testing::env().await;
            let app = e.app.clone();
            let parent = format!("zz-parent-{round}");
            let child = format!("aa-child-{round}");
            for (bot_id, name, managed_by) in [(&parent, "alfa", "user"), (&child, "alfa-kid", "child")] {
                sqlx::query(
                    "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
                     VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
                )
                .bind(bot_id)
                .bind(&e.project_id)
                .bind(name)
                .bind(managed_by)
                .bind(db::now())
                .execute(&app.db)
                .await
                .unwrap();
            }
            sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&child).execute(&app.db).await.unwrap();
            assert!(child < parent, "這條測試要 child 排在前面");
            in_config(&e, &[(&parent, "alfa")]).await;

            let gate = Arc::new(tokio::sync::Barrier::new(2));
            let by_bot = tokio::spawn({
                let (app, parent, gate) = (app.clone(), parent.clone(), gate.clone());
                async move {
                    gate.wait().await;
                    delete_bot(State(app), Path(parent)).await.map(|_| ()).map_err(reason)
                }
            });
            let by_project = tokio::spawn({
                let (app, pid, gate) = (app.clone(), e.project_id.clone(), gate.clone());
                async move {
                    gate.wait().await;
                    delete_project(State(app), Path(pid)).await.map(|_| ()).map_err(reason)
                }
            });
            let both = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                (by_bot.await.unwrap(), by_project.await.unwrap())
            })
            .await
            .unwrap_or_else(|_| panic!("round {round}: delete_bot 與 delete_project 互等卡死"));
            // 至少有一邊成功，parent 一定刪掉了；另一邊看到的是「已經不在」而不是卡住。
            assert!(both.0.is_ok() || both.1.is_ok(), "round {round}: {both:?}");
            assert!(db::bot(&app.db, &parent).await.unwrap().unwrap().deleted_at.is_some(), "round {round}");
        }
    }

    /// TOML 被外部清空後刪原本的專案：目標此刻不在 TOML → 拒絕，DB 裡的專案與 bot 一列都不動。
    #[tokio::test]
    async fn deleting_a_project_the_toml_no_longer_has_is_refused() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;
        app.cfg.update(|cfg| { cfg.projects.clear(); Ok(()) }).await.unwrap();

        let err = delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap_err();
        assert_eq!(reason(err), "not_in_config");
        assert!(db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_none());
        assert_eq!(db::live_projects(&app.db).await.unwrap().len(), 1);

        let err = delete_bot(State(app.clone()), Path(b1.clone())).await.unwrap_err();
        assert_eq!(reason(err), "not_in_config");
        assert!(db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_none());
    }

    /// 兩支不同的 DELETE 真的同時跑：各自只刪自己那顆，不會把對方剛寫進 config 的刪除當成未授權而拒絕，
    /// 也不會替對方放行。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_deletes_at_once_each_remove_only_their_own_bot() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let ids: Vec<String> = futures::future::join_all(["a", "b", "c", "d"].map(|n| a_bot(&e, n, "user"))).await;
        in_config(&e, &[(&ids[0], "a"), (&ids[1], "b"), (&ids[2], "c"), (&ids[3], "d")]).await;

        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let spawn = |id: String| {
            let (app, gate) = (app.clone(), gate.clone());
            tokio::spawn(async move {
                gate.wait().await;
                delete_bot(State(app), Path(id)).await.map(|_| ()).map_err(reason)
            })
        };
        let (x, y) = (spawn(ids[0].clone()), spawn(ids[1].clone()));
        assert_eq!(x.await.unwrap(), Ok(()));
        assert_eq!(y.await.unwrap(), Ok(()));

        let live: Vec<String> = db::live_bots(&app.db).await.unwrap().into_iter().map(|b| b.id).collect();
        assert_eq!(live.len(), 2, "{live:?}");
        assert!(live.contains(&ids[2]) && live.contains(&ids[3]));
        let in_toml: Vec<String> =
            app.cfg.get().await.projects[0].bots.iter().filter_map(|b| b.id.clone()).collect();
        assert_eq!(in_toml, vec![ids[2].clone(), ids[3].clone()]);
    }

    /// review 2026-09-12 d: no reconcile walks deleted bots, so nothing else would end that run.
    #[tokio::test]
    async fn a_bot_deleted_while_its_host_is_down_does_not_keep_a_running_run() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let id = a_bot(&e, "remote-ish", "user").await;
        in_config(&e, &[(&id, "remote-ish")]).await;
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

    /// 這顆 bot 的 runtime 目錄（裡面放著 hook 設定，砍掉就補不回來）。
    fn runtime_dir(app: &Arc<App>, bot_id: &str) -> std::path::PathBuf {
        let dir = app.bot_dir(bot_id).unwrap();
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(dir.join("hooks/settings.json"), "{}").unwrap();
        dir
    }

    /// #246：遠端專案刪除定案之後 `projects` 讀不到，child 的目錄清理不能退回 local——本機同 id 的目錄一根毛都不能動。
    /// host 在定案之前就讀好；定案前就讀不到則 502、什麼都沒動。
    #[tokio::test]
    async fn a_remote_project_delete_never_purges_local_dirs_when_the_host_cannot_be_reread() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let kid = a_bot(&e, "kid", "child").await;
        in_config(&e, &[]).await;
        sqlx::query("UPDATE projects SET host='remote1' WHERE id=?").bind(&e.project_id).execute(&app.db).await.unwrap();
        let dir = runtime_dir(&app, &kid);

        // 定案前讀不到：整支失敗、bot 沒軟刪。
        crate::testing::make_table_unreadable(&app, "projects").await;
        let err = delete_project(State(app.clone()), Path(e.project_id.clone())).await.unwrap_err();
        crate::testing::make_table_readable(&app, "projects").await;
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        assert!(db::bot(&app.db, &kid).await.unwrap().unwrap().deleted_at.is_none());
        assert!(dir.exists());

        // 定案之後才讀不到（重讀的舊寫法在這裡退回 local）：本機目錄必須還在。
        let a = app.clone();
        crate::lifecycle::race_point::arm("delete_project_after_commit", &e.project_id, move || async move {
            crate::testing::make_table_unreadable(&a, "projects").await
        });
        let _ = delete_project(State(app.clone()), Path(e.project_id.clone())).await;
        crate::testing::make_table_readable(&app, "projects").await;
        assert!(dir.join("hooks/settings.json").exists(), "遠端專案的清理不能砍到本機目錄");
    }

    async fn body_of(resp: Response) -> Value {
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap()
    }

    /// #210 驗收 1（讀不到 run 的狀態）：定案之前就讀不到 → 502、什麼都不動（bot 沒刪、config 沒寫、沒停、目錄在），可以原樣再按一次。
    #[tokio::test]
    async fn an_unreadable_run_state_refuses_the_delete_before_anything_is_decided() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let b1 = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&b1, "alfa")]).await;
        let run = crate::testing::fake_run(&app, &b1).await;
        let dir = runtime_dir(&app, &b1);

        crate::testing::make_table_unreadable(&app, "runs").await;
        let err = delete_bot(State(app.clone()), Path(b1.clone())).await.unwrap_err();
        crate::testing::make_table_readable(&app, "runs").await;

        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        assert!(db::bot(&app.db, &b1).await.unwrap().unwrap().deleted_at.is_none(), "定案前不能刪");
        assert!(app.cfg.get().await.projects[0].bots.iter().any(|b| b.id.as_deref() == Some(b1.as_str())), "config 沒寫");
        assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "running", "沒停");
        assert!(dir.join("hooks/settings.json").exists());
        assert!(!e.herdr.methods().iter().any(|m| m == "agent.send_keys"), "一個鍵都沒送");
    }

    /// #210 驗收 1（停機失敗而且讀不到 active run）：定案之後才讀不到——停機失敗、run 收不掉——目錄必須留著，
    /// 回應列在 `kept_dirs`；DB 好了 run 還活著，開機清掃也不會去刪它。
    #[tokio::test]
    async fn a_failed_stop_that_cannot_read_the_run_keeps_the_dir() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let id = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&id, "alfa")]).await;
        let run = a_running_run(&app, &id).await; // session 不存在：stop 一定失敗
        let dir = runtime_dir(&app, &id);
        let a = app.clone();
        crate::lifecycle::race_point::arm("delete_bot_after_decided", &id, move || async move {
            crate::testing::make_table_unreadable(&a, "runs").await
        });

        let out = body_of(delete_bot(State(app.clone()), Path(id.clone())).await.unwrap()).await;
        crate::testing::make_table_readable(&app, "runs").await;

        assert_eq!(out["kept_dirs"], json!([{"bot_id": id, "reason": "run_state_unreadable"}]), "{out}");
        assert!(dir.join("hooks/settings.json").exists(), "讀不到 run 的狀態，不能刪它的目錄");
        assert!(db::bot(&app.db, &id).await.unwrap().unwrap().deleted_at.is_some(), "刪除本身已經定案");
        assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "running", "讀不到就沒有動 run");
        assert_eq!(crate::lifecycle::purge_deleted_bot_dirs(&app).await, 0, "run 還活著：開機清掃也不刪");
        assert!(dir.exists());
    }

    /// #210：停機失敗（主機連不上）而 run 讀得到：run 照舊強制收成 exited，但停機沒有確認、agent 可能還活著——目錄留著、
    /// 列在 `kept_dirs`；沒有 run 的 parent 照常 purge。下次開機的清掃在 run 確定結束之後把它收掉。
    #[tokio::test]
    async fn a_delete_whose_stop_failed_keeps_that_dir_until_the_startup_sweep() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let parent = a_bot(&e, "alfa", "user").await;
        let kid = a_bot(&e, "alfa-kid", "child").await;
        sqlx::query("UPDATE bots SET parent_bot_id = ? WHERE id = ?").bind(&parent).bind(&kid).execute(&app.db).await.unwrap();
        in_config(&e, &[(&parent, "alfa")]).await;
        let run = a_running_run(&app, &kid).await;
        let (parent_dir, kid_dir) = (runtime_dir(&app, &parent), runtime_dir(&app, &kid));

        let out = body_of(delete_bot(State(app.clone()), Path(parent.clone())).await.unwrap()).await;

        assert_eq!(out["removed_children"], json!([kid]), "{out}");
        assert_eq!(out["kept_dirs"], json!([{"bot_id": kid, "reason": "stop_not_confirmed"}]), "{out}");
        assert!(!parent_dir.exists(), "沒有 run、確定沒有：照常 purge");
        assert!(kid_dir.join("hooks/settings.json").exists(), "停機沒有確認：目錄留著");
        assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "exited", "run 照舊被收掉，不留孤兒");

        assert_eq!(crate::lifecycle::purge_deleted_bot_dirs(&app).await, 1, "下次開機：run 確定結束了才收");
        assert!(!kid_dir.exists());
    }

    /// 收完再讀，run 還是 active（`exited` 寫不進去）：不是「確定沒有 active run」，目錄留著。
    #[tokio::test]
    async fn a_run_that_could_not_be_ended_keeps_the_dir() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let id = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&id, "alfa")]).await;
        let run = a_running_run(&app, &id).await;
        let dir = runtime_dir(&app, &id);
        sqlx::query("CREATE TRIGGER refuse_exit BEFORE UPDATE OF state ON runs WHEN NEW.state = 'exited' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let out = body_of(delete_bot(State(app.clone()), Path(id.clone())).await.unwrap()).await;

        assert_eq!(out["kept_dirs"], json!([{"bot_id": id, "reason": "run_still_active"}]), "{out}");
        assert!(dir.join("hooks/settings.json").exists());
        assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "running");
    }

    /// #210 驗收 2：停好了、而且讀得到現在沒有 active run，才 purge；回應不帶 `kept_dirs`。
    #[tokio::test]
    async fn a_confirmed_stop_purges_the_dir() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let id = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&id, "alfa")]).await;
        let run = crate::testing::fake_run(&app, &id).await; // session `test`＝mock herdr，stop 會真的送鍵
        let dir = runtime_dir(&app, &id);

        let out = body_of(delete_bot(State(app.clone()), Path(id.clone())).await.unwrap()).await;

        assert!(out.get("kept_dirs").is_none(), "{out}");
        assert!(!dir.exists(), "確定停了、確定沒有 run：purge");
        assert_eq!(db::run(&app.db, &run).await.unwrap().unwrap().state, "stopped");
    }

    /// 停好了，但「現在沒有 active run」讀不到（停機與收尾之間 DB 壞了）：不是確定，目錄留著。
    #[tokio::test]
    async fn a_stop_whose_result_cannot_be_read_back_keeps_the_dir() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let id = a_bot(&e, "alfa", "user").await;
        in_config(&e, &[(&id, "alfa")]).await;
        crate::testing::fake_run(&app, &id).await;
        let dir = runtime_dir(&app, &id);
        let a = app.clone();
        crate::lifecycle::race_point::arm("delete_stop_before_proof", &id, move || async move {
            crate::testing::make_table_unreadable(&a, "runs").await
        });

        let out = body_of(delete_bot(State(app.clone()), Path(id.clone())).await.unwrap()).await;
        crate::testing::make_table_readable(&app, "runs").await;

        assert_eq!(out["kept_dirs"], json!([{"bot_id": id, "reason": "run_state_unreadable"}]), "{out}");
        assert!(dir.join("hooks/settings.json").exists());
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

    /// 2026-09-14 使用者：暫存區要收任意檔。上傳端不再看 mime，沒帶 Content-Type 也要收。
    #[tokio::test]
    async fn upload_takes_any_file_not_just_images() {
        let e = crate::testing::env().await;
        let bot_id = crate::testing::claude_bot(&e.app, &e.project_id, "attach-bot").await.id;
        let query = Query(HashMap::from([("name".to_string(), "run.log".to_string())]));
        let headers = HeaderMap::from_iter([(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))]);
        assert_eq!(
            status(upload_attachment(State(e.app.clone()), Path(bot_id.clone()), query, headers, Bytes::from_static(b"boom\n")).await).await,
            StatusCode::OK
        );
        // 完全沒有 Content-Type 的上傳（有些瀏覽器貼上就是這樣）也不能被擋。
        let query = Query(HashMap::from([("name".to_string(), "notes.md".to_string())]));
        assert_eq!(
            status(upload_attachment(State(e.app.clone()), Path(bot_id), query, HeaderMap::new(), Bytes::from_static(b"# hi")).await).await,
            StatusCode::OK
        );
    }
}

#[cfg(test)]
mod prompt_route_tests {
    //! Through the real `prompt_bot` handler and `IntoResponse`: the status codes the API documents.
    use super::*;

    async fn typed_bot(e: &crate::testing::Env, kind: &str) -> String {
        typed_bot_named(e, kind, "route-bot", "pane-route").await
    }

    /// 同一個專案要好幾顆（名字與 pane 都不能撞）的測試用。
    async fn typed_bot_named(e: &crate::testing::Env, kind: &str, name: &str, pane: &str) -> String {
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
        sqlx::query("UPDATE bots SET kind = ? WHERE id = ?").bind(kind).bind(&bot.id).execute(&e.app.db).await.unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1',?,?,'test',1,?)",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .bind(pane)
        .bind(name)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        bot.id
    }

    async fn call(e: &crate::testing::Env, bot: &str, text: String, crid: &str) -> (StatusCode, Value) {
        let body = PromptIn { text, client_request_id: Some(crid.into()), attachments: vec![], relay_from: None, ack: false, reply_to: None, send_now: false, start_if_stopped: false, queue_if_busy: false, clear_draft: false, submit_draft: false, expect_draft_token: None };
        let resp = match prompt_bot(State(e.app.clone()), Path(bot.to_string()), HeaderMap::new(), Json(body)).await {
            Ok(r) => r,
            Err(err) => err.into_response(),
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn call_queue_if_busy(e: &crate::testing::Env, bot: &str, text: &str, crid: &str, attachments: &[String]) -> (StatusCode, Value) {
        let body: PromptIn = serde_json::from_value(json!({
            "text": text,
            "client_request_id": crid,
            "queue_if_busy": true,
            "attachments": attachments,
        })).unwrap();
        let resp = match prompt_bot(State(e.app.clone()), Path(bot.to_string()), HeaderMap::new(), Json(body)).await {
            Ok(r) => r,
            Err(err) => err.into_response(),
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// 不可能照原樣送出的 prompt 是真的 HTTP 422，body 說清楚原因（sol 第八輪 #1）。
    #[tokio::test]
    async fn an_unsendable_prompt_is_http_422_with_a_machine_readable_body() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "grok").await;
        e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        let (status, body) = call(&e, &bot, "x".repeat(crate::lifecycle::MAX_PROVABLE_CHARS + 1), "route-422").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "delivery_unprovable");
        assert_eq!(body["reason"], "prompt_too_long_to_prove");
        assert_eq!(body["sent"], false);
    }

    #[tokio::test]
    async fn a_user_prompt_can_claim_the_single_busy_queue_slot_idempotently() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "grok").await;
        let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'")
            .bind(&bot).fetch_one(&e.app.db).await.unwrap();
        let conversation_id = db::conversation_id(&e.app.db, &bot).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
            .bind(db::ulid()).bind(&conversation_id).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();

        let attachment_id = db::ulid();
        sqlx::query("INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at) VALUES (?,?,'image.png','image/png',1,'/tmp/image.png','/tmp/image.png','local',?)")
            .bind(&attachment_id).bind(&bot).bind(db::now()).execute(&e.app.db).await.unwrap();
        let attachments = vec![attachment_id.clone()];
        let (status, first) = call_queue_if_busy(&e, &bot, "稍後送出", "busy-queue-1", &attachments).await;
        assert_eq!(status, StatusCode::OK, "queue_if_busy 收下忙碌中的 prompt：{first}");
        assert_eq!(first["delivery"], "queued", "{first}");
        let turn_id = first["turn_id"].as_str().unwrap();
        let message_id = first["message_id"].as_str().unwrap();
        let stored: (String, String, String, i64, Option<String>) = sqlx::query_as("SELECT status, origin, delivery, awaits_idle, prompt_text FROM turns WHERE id=?")
            .bind(turn_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!((stored.0.as_str(), stored.1.as_str(), stored.2.as_str(), stored.3), ("queued", "web", "pending", 1));
        assert!(stored.4.as_deref().unwrap().starts_with("稍後送出\n\n"), "附件路徑要存在 flush 可送的 prompt_text：{stored:?}");
        let replay = call_queue_if_busy(&e, &bot, "稍後送出", "busy-queue-1", &attachments).await;
        assert_eq!(replay.0, StatusCode::OK, "{}", replay.1);
        assert_eq!(replay.1["turn_id"], turn_id, "same client_request_id returns the queued turn");
        assert_eq!(replay.1["message_id"], message_id);

        let (status, second) = call_queue_if_busy(&e, &bot, "再一則", "busy-queue-2", &[]).await;
        assert_eq!(status, StatusCode::CONFLICT, "one busy queue slot per bot: {second}");
        assert_eq!(second["reason"], "queue_slot_taken", "{second}");
        assert_eq!(second["turn_id"], turn_id, "the conflict identifies the existing slot");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE conversation_id=? AND status='queued'")
            .bind(&conversation_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(count, 1, "the refused request writes no turn");
        let message_turn: String = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id=?")
            .bind(message_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(message_turn, turn_id);
        let bound: Option<String> = sqlx::query_scalar("SELECT message_id FROM attachments WHERE id=?")
            .bind(&attachment_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(bound.as_deref(), Some(message_id), "message and attachment binding commit together");
        let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(turn_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(serde_json::to_value(turn).unwrap()["awaits_idle"], 1, "turn JSON exposes the wait flag");
    }

    /// 唯一的 queued 槽被別人佔著時，409 要說是誰佔著（`holder`），web 才講得出人話：
    /// 以前 AGM 派工／別的 bot／啟動等待佔著時，使用者的 Enter 撞上 DB 唯一索引，只拿到英文的
    /// 「a turn is already queued for this bot」，沒有 `queue_slot_taken`、也沒有 `turn_id`。
    #[tokio::test]
    async fn a_queue_slot_held_by_someone_else_is_a_structured_409_that_says_who() {
        let e = crate::testing::env().await;
        let agm = typed_bot_named(&e, "claude", "agm-bot", "pane-agm").await;
        sqlx::query("INSERT INTO supervisors (id, bot_id, created_at, updated_at) VALUES ('sup-1', ?, ?, ?)").bind(&agm).bind(db::now()).bind(db::now()).execute(&e.app.db).await.unwrap();
        let other = typed_bot_named(&e, "claude", "other-bot", "pane-other").await;

        // (佔著的人, awaits_idle, awaits_start, crid, relay_from, 期望的 holder.kind)
        let cases: Vec<(&str, i64, i64, Option<String>, Option<String>, &str)> = vec![
            ("agm", 0, 0, None, Some(agm.clone()), "agm"),
            ("bot", 0, 0, None, Some(other.clone()), "bot"),
            ("daemon", 0, 0, None, Some("daemon".into()), "daemon"),
            ("start", 0, 1, None, None, "start"),
            ("user", 1, 0, None, None, "user"),
        ];
        for (name, awaits_idle, awaits_start, crid, relay_from, kind) in cases {
            let bot = typed_bot_named(&e, "grok", &format!("holder-{name}"), &format!("pane-{name}")).await;
            let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'").bind(&bot).fetch_one(&e.app.db).await.unwrap();
            let conv = db::conversation_id(&e.app.db, &bot).await.unwrap();
            sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
                .bind(db::ulid()).bind(&conv).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();
            let held = db::ulid();
            sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, awaits_idle, awaits_start, client_request_id) VALUES (?, ?, 'web', 'queued', 'pending', 'held', ?, ?, ?, ?)")
                .bind(&held).bind(&conv).bind(db::now()).bind(awaits_idle).bind(awaits_start).bind(crid).execute(&e.app.db).await.unwrap();
            sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, relay_unverified, created_at) VALUES (?, ?, ?, 'user', 'held', 'web', ?, 0, ?)")
                .bind(db::ulid()).bind(&conv).bind(&held).bind(relay_from).bind(db::now()).execute(&e.app.db).await.unwrap();

            let (status, body) = call_queue_if_busy(&e, &bot, "我也要排隊", &format!("slot-{name}"), &[]).await;
            assert_eq!(status, StatusCode::CONFLICT, "{name}: {body}");
            assert_eq!(body["reason"], "queue_slot_taken", "{name}: {body}");
            assert_eq!(body["turn_id"], held, "{name}: 指出佔著的那一筆：{body}");
            assert_eq!(body["holder"]["kind"], kind, "{name}: {body}");
            if kind == "agm" || kind == "bot" {
                assert!(body["holder"]["bot_name"].is_string(), "{name}: 要帶佔著的 bot 名字：{body}");
            }
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE conversation_id=? AND status='queued'").bind(&conv).fetch_one(&e.app.db).await.unwrap();
            assert_eq!(count, 1, "{name}: 被拒的請求什麼都不寫");
        }
    }

    /// `relay_from` 沒帶 bot token 時只是**自稱**（`relay_unverified`）：不能拿它當事實講「AGM／某顆 bot 的訊息正在排隊」，
    /// 也不該替呼叫端把任意 bot id 解析成名字。自稱的一律只說「別的 bot」，不帶名字與 id。
    #[tokio::test]
    async fn a_self_declared_relay_source_is_never_named_or_called_agm() {
        let e = crate::testing::env().await;
        let agm = typed_bot_named(&e, "claude", "agm-bot", "pane-agm2").await;
        sqlx::query("INSERT INTO supervisors (id, bot_id, created_at, updated_at) VALUES ('sup-2', ?, ?, ?)").bind(&agm).bind(db::now()).bind(db::now()).execute(&e.app.db).await.unwrap();
        let victim = typed_bot_named(&e, "grok", "holder-spoof", "pane-spoof").await;
        let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'").bind(&victim).fetch_one(&e.app.db).await.unwrap();
        let conv = db::conversation_id(&e.app.db, &victim).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
            .bind(db::ulid()).bind(&conv).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();
        let held = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, awaits_idle) VALUES (?, ?, 'web', 'queued', 'pending', 'held', ?, 0)")
            .bind(&held).bind(&conv).bind(db::now()).execute(&e.app.db).await.unwrap();
        // 自稱是 AGM 的 bot（沒帶 token）。
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, relay_unverified, created_at) VALUES (?, ?, ?, 'user', 'held', 'web', ?, 1, ?)")
            .bind(db::ulid()).bind(&conv).bind(&held).bind(&agm).bind(db::now()).execute(&e.app.db).await.unwrap();

        let (status, body) = call_queue_if_busy(&e, &victim, "我也要排隊", "slot-spoof", &[]).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["reason"], "queue_slot_taken", "{body}");
        assert_eq!(body["holder"]["kind"], "bot", "自稱的來源不能被講成 AGM：{body}");
        assert!(body["holder"].get("bot_name").is_none() && body["holder"].get("bot_id").is_none(), "自稱的來源不帶名字與 id：{body}");
    }

    /// web 的 Enter 永遠帶 `queue_if_busy`，bot 沒在跑時再加 `start_if_stopped`。畫面上的 run 比 daemon 慢一拍
    /// （bot 剛起來、frame 還沒到）時兩個旗標會一起打到「已經在跑而且忙」的 bot：`start_if_stopped`
    /// 「bot 在跑就跟沒帶一樣」，所以結果該跟只帶 `queue_if_busy` 相同——落地排隊，而不是丟掉旗標回 409（Refs #733）。
    #[tokio::test]
    async fn start_if_stopped_does_not_drop_queue_if_busy_for_a_running_busy_bot() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "grok").await;
        let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'")
            .bind(&bot).fetch_one(&e.app.db).await.unwrap();
        let conversation_id = db::conversation_id(&e.app.db, &bot).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
            .bind(db::ulid()).bind(&conversation_id).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();
        let body: PromptIn = serde_json::from_value(json!({
            "text": "兩個旗標一起來",
            "client_request_id": "start-and-queue",
            "start_if_stopped": true,
            "queue_if_busy": true,
        })).unwrap();
        let resp = match prompt_bot(State(e.app.clone()), Path(bot.clone()), HeaderMap::new(), Json(body)).await {
            Ok(r) => r,
            Err(err) => err.into_response(),
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert_eq!(status, StatusCode::OK, "在跑的忙碌 bot 照 queue_if_busy 排隊：{out}");
        assert_eq!(out["delivery"], "queued", "{out}");
        let awaits_idle: i64 = sqlx::query_scalar("SELECT awaits_idle FROM turns WHERE id=?")
            .bind(out["turn_id"].as_str().unwrap()).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(awaits_idle, 1);
    }

    #[tokio::test]
    async fn queue_if_busy_uses_the_agent_status_when_no_turn_is_in_flight() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "grok").await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE bot_id=? AND state='running'")
            .bind(&bot).execute(&e.app.db).await.unwrap();
        let (status, body) = call_queue_if_busy(&e, &bot, "等 agent 閒下來", "busy-agent-status", &[]).await;
        assert_eq!(status, StatusCode::OK, "非 idle 狀態也要持久排隊：{body}");
        assert_eq!(body["delivery"], "queued", "{body}");
        assert!(e.herdr.calls_to("agent.prompt").is_empty());
        assert!(e.herdr.calls_to("pane.send_text").is_empty());
    }

    #[tokio::test]
    async fn busy_prompt_admission_dispatch_and_withdrawal_share_the_durable_queue() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "claude").await;
        let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'")
            .bind(&bot).fetch_one(&e.app.db).await.unwrap();
        let conversation_id = db::conversation_id(&e.app.db, &bot).await.unwrap();
        let previous_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
            .bind(&previous_id).bind(&conversation_id).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();

        // B can restore a still-queued prompt submitted by A before C dispatches it.
        let (status, withdrawn) = call_queue_if_busy(&e, &bot, "先放回輸入框", "queue-withdraw-first", &[]).await;
        assert_eq!(status, StatusCode::OK, "{withdrawn}");
        let withdrawn_id = withdrawn["turn_id"].as_str().unwrap().to_string();
        let response = withdraw_turn(State(e.app.clone()), Path(withdrawn_id.clone())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), json!({"text": "先放回輸入框", "attachments": []}));
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&withdrawn_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(status, "failed");

        // A second prompt remains queued while its predecessor is active, then C dispatches it at idle.
        let (status, admitted) = call_queue_if_busy(&e, &bot, "等閒下來再送", "queue-dispatch-next", &[]).await;
        assert_eq!(status, StatusCode::OK, "{admitted}");
        let queued_id = admitted["turn_id"].as_str().unwrap();
        crate::lifecycle::flush_queued_locked(&e.app, &bot).await.unwrap();
        let still_queued: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(queued_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(still_queued, "queued", "busy run keeps the accepted prompt durable");
        assert!(e.herdr.calls_to("pane.send_text").is_empty());

        e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?").bind(db::now()).bind(&previous_id).execute(&e.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();
        crate::lifecycle::flush_queued_locked(&e.app, &bot).await.unwrap();
        let sent: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?").bind(queued_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!((sent.0.as_str(), sent.1.as_str()), ("in_flight", "ok"));
        assert_eq!(e.herdr.calls_to("pane.send_text").len(), 1, "C sends the admitted prompt once");

        let response = withdraw_turn(State(e.app.clone()), Path(queued_id.to_string())).await.unwrap_err().into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT, "B cannot restore a turn C has already claimed");
    }

    #[tokio::test]
    async fn queue_if_busy_on_an_idle_run_keeps_direct_delivery() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "grok").await;
        e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        let (status, body) = call_queue_if_busy(&e, &bot, "現在就送", "idle-queue-flag", &[]).await;
        assert_eq!(status, StatusCode::OK, "idle runs keep the existing direct prompt route: {body}");
        assert_ne!(body["delivery"], "queued", "an idle run sends directly");
        let awaits_idle: i64 = sqlx::query_scalar("SELECT awaits_idle FROM turns WHERE id=?")
            .bind(body["turn_id"].as_str().unwrap()).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(awaits_idle, 0);
    }

    #[tokio::test]
    async fn a_failed_attachment_binding_rolls_back_the_busy_queue_admission() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "grok").await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE bot_id=? AND state='running'")
            .bind(&bot).execute(&e.app.db).await.unwrap();
        let attachment_id = db::ulid();
        sqlx::query("INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at) VALUES (?,?,'image.png','image/png',1,'/tmp/image.png','/tmp/image.png','local',?)")
            .bind(&attachment_id).bind(&bot).bind(db::now()).execute(&e.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER refuse_busy_queue_attachment BEFORE UPDATE OF message_id ON attachments BEGIN SELECT RAISE(ABORT, 'injected bind failure'); END")
            .execute(&e.app.db).await.unwrap();

        let (status, body) = call_queue_if_busy(&e, &bot, "有附件", "busy-queue-rollback", &[attachment_id.clone()]).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "failed attachment binding must fail the admission: {body}");
        let (turns, messages): (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM turns WHERE client_request_id='busy-queue-rollback'), (SELECT count(*) FROM messages WHERE content='有附件')")
            .fetch_one(&e.app.db).await.unwrap();
        let bound: Option<String> = sqlx::query_scalar("SELECT message_id FROM attachments WHERE id=?")
            .bind(&attachment_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!((turns, messages, bound), (0, 0, None), "turn, message and attachment binding all roll back");
    }

    /// #337：同一個 client_request_id 換了內容不能回第一則的結果；完全一樣的重送照舊回同一個 turn。
    #[tokio::test]
    async fn a_reused_request_id_with_different_text_is_a_409_not_the_first_result() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "grok").await;
        e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        let (s1, b1) = call(&e, &bot, "first".into(), "crid-x").await;
        assert_eq!(s1, StatusCode::OK, "{b1}");
        let (s2, b2) = call(&e, &bot, "first".into(), "crid-x").await;
        assert_eq!((s2, &b2["turn_id"]), (StatusCode::OK, &b1["turn_id"]), "一樣的重送回同一個 turn");
        let (s3, b3) = call(&e, &bot, "second".into(), "crid-x").await;
        assert_eq!(s3, StatusCode::CONFLICT, "{b3}");
        assert_eq!(b3["reason"], "text_mismatch");
    }

    /// 已刪除的 bot 不能收 prompt（送進使用者已經刪掉的 pane 還回 200）。
    #[tokio::test]
    async fn a_soft_deleted_bot_does_not_take_a_prompt() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "grok").await;
        e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&bot).execute(&e.app.db).await.unwrap();
        let (status, body) = call(&e, &bot, "hello".into(), "del-1").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    /// 輸入框有字是 HTTP 409、可重試，沒有建立 turn。
    #[tokio::test]
    async fn a_busy_composer_is_http_409_and_retryable() {
        let e = crate::testing::env().await;
        let bot = typed_bot(&e, "grok").await;
        e.herdr.live_pane(
            "pane-route",
            crate::testing::LivePane { width: Some(120), boxed: true, composer: vec!["草稿".into()], ..Default::default() },
        );
        let (status, body) = call(&e, &bot, "Reply with PONG".into(), "route-409").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["reason"], "composer_busy");
        assert_eq!(body["retryable"], true);
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&e.app.db).await.unwrap();
        assert_eq!(turns, 0);
    }
}

#[cfg(test)]
mod resume_query_tests {
    use super::*;

    #[test]
    fn only_native_is_a_resume_mode_and_it_is_strict() {
        let q = |v: Option<&str>| StartQuery { resume: v.map(str::to_string), session: None };
        // 2026-10-02 起預設接回（接不回才開新的），`fresh` 才是開新對話。
        let default = resume_opts(&q(None)).unwrap();
        assert!(default.resume_native && !default.resume_required, "預設：能接就接，接不回照樣啟動");
        assert_eq!(resume_opts(&q(Some(""))).unwrap(), default);
        assert_eq!(resume_opts(&q(Some("fresh"))).unwrap(), lifecycle::StartOpts::default(), "fresh：開新對話");
        let native = resume_opts(&q(Some("native"))).unwrap();
        assert!(native.resume_native && native.resume_required, "native 一定是「接不回就不啟動」");
        assert!(matches!(resume_opts(&q(Some("bogus"))), Err(LcError::Bad(_))));
    }

    /// `session=<id>` 只跟 `resume=native` 一起收，而且只收乾淨的 id（救援路徑，2026-09-22）。
    #[test]
    fn an_explicit_session_rides_only_on_native_and_must_be_plain() {
        let q = |r: Option<&str>, s: Option<&str>| StartQuery { resume: r.map(str::to_string), session: s.map(str::to_string) };
        let o = resume_opts(&q(Some("native"), Some(" 246fcf93-af39-48d3-8041-b27df5a91958 "))).unwrap();
        assert_eq!(o.resume_session.as_deref(), Some("246fcf93-af39-48d3-8041-b27df5a91958"));
        assert!(o.resume_native && o.resume_required);
        assert!(resume_opts(&q(Some("native"), Some(""))).unwrap().resume_session.is_none());
        assert!(matches!(resume_opts(&q(None, Some("abc"))), Err(LcError::Bad(_))), "沒有 native 不收 session");
        assert!(matches!(resume_opts(&q(Some("native"), Some("a b"))), Err(LcError::Bad(_))));
        assert!(matches!(resume_opts(&q(Some("native"), Some("../x"))), Err(LcError::Bad(_))));
    }

    #[tokio::test]
    async fn capabilities_advertise_resume_maintenance_and_service_auth() {
        let v = get_capabilities().await.0;
        let caps: Vec<&str> = v["capabilities"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(caps.contains(&"resume_native_start") && caps.contains(&"herdr_maintenance") && caps.contains(&"service_principals"), "{v}");
    }
}

#[cfg(test)]
mod started_json_tests {
    //! issue #107：`?resume=native` 的回應要照接回的**結論**說，不是看一個 SessionStart 一到就被清掉的暫存欄位。
    use super::*;
    use crate::testing as tt;

    async fn resumed_run(app: &Arc<App>, project_id: &str, name: &str, resume: Option<&str>) -> (String, String) {
        let bot = tt::claude_bot(app, project_id, name).await;
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, resume_session_id)
             VALUES (?,?,'running','idle','ws-1',?,'agent','test',?,?)",
        )
        .bind(&run)
        .bind(&bot.id)
        .bind(format!("pane-{}", bot.id))
        .bind(db::now())
        .bind(resume)
        .execute(&app.db)
        .await
        .unwrap();
        (bot.id, run)
    }

    async fn session_start(app: &Arc<App>, bot_id: &str, run_id: &str, session: &str) {
        crate::hookrecv::process(
            app,
            &crate::hookrecv::HookBody {
                bot_id: bot_id.to_string(),
                provider: "claude".into(),
                payload: json!({"hook_event_name": "SessionStart", "session_id": session, "source": "resume"}),
                received_at: None,
                truncated: false,
                run_id: Some(run_id.to_string()),
            },
        )
        .await
        .unwrap();
    }

    fn summary(v: &Value) -> (Value, Value, Value) {
        (v["resumed"].clone(), v["session_id"].clone(), v["resume_outcome"].clone())
    }

    #[tokio::test]
    async fn resumed_reports_the_recorded_outcome_not_the_marker_session_start_clears() {
        let e = tt::env().await;
        let app = e.app.clone();
        let opts = lifecycle::StartOpts { resume_native: true, resume_required: true, ..Default::default() };

        // 帶了 --resume、還在等回報。
        let (bot, run) = resumed_run(&app, &e.project_id, "pending", Some("s-1")).await;
        assert_eq!(summary(&started_json(&app, &run, &opts).await.unwrap()), (json!(true), json!("s-1"), Value::Null));
        // SessionStart 在回應組好之前就到了：暫存欄位清掉，但接回了就是接回了。
        session_start(&app, &bot, &run, "s-1").await;
        let marker: Option<String> = sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(marker, None, "前提：暫存欄位真的被清掉了");
        assert_eq!(summary(&started_json(&app, &run, &opts).await.unwrap()), (json!(true), json!("s-1"), json!("verified")));

        // CLI 開了新對話：不是接回。
        let (bot, run) = resumed_run(&app, &e.project_id, "mismatch", Some("s-2")).await;
        session_start(&app, &bot, &run, "s-brand-new").await;
        assert_eq!(summary(&started_json(&app, &run, &opts).await.unwrap()), (json!(false), Value::Null, json!("mismatch")));

        // 這次根本沒帶 --resume（接不回、退回開新對話）。
        let (_bot, run) = resumed_run(&app, &e.project_id, "fresh", None).await;
        assert_eq!(summary(&started_json(&app, &run, &opts).await.unwrap()), (json!(false), Value::Null, Value::Null));

        // 沒要求 resume：只回 run_id，跟以前一樣。
        assert_eq!(started_json(&app, &run, &lifecycle::StartOpts::default()).await.unwrap(), json!({"run_id": run}));
    }

    /// 按「啟動」叫醒睡著的 bot：接不回原對話時 `wake` 會退回開新對話，回應不能照樣說 `resumed:true`。
    #[tokio::test]
    async fn waking_a_sleeping_bot_that_could_not_resume_says_so() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "sleeper").await;
        // 睡著了，但沒有任何一段對話可以接（沒有 native session）。
        sqlx::query("INSERT INTO bot_sleeps (bot_id, native_session_id, idle_minutes, reason, slept_at) VALUES (?,NULL,90,'idle',?)")
            .bind(&bot.id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let resp = start_bot(State(app.clone()), Path(bot.id.clone()), Query(StartQuery { resume: None, session: None })).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
        let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("叫醒了");
        assert_eq!(body["run_id"], json!(run.id));
        assert_eq!(body["resumed"], json!(false), "開的是新對話：{body}");
        lifecycle::stop_bot(&app, &bot.id).await.unwrap();
    }
}

/// bot 設定的 API：建／改／還原，與啟動時寫進 `--settings` 的檔案。
#[cfg(test)]
mod bot_config_tests {
    use super::*;
    use crate::testing as tt;
    use crate::testing::{env, Env};

    /// `testing::env` 只種 DB 那一列；bot 要從 config.toml 進來，所以先把專案寫進 config。
    async fn seed_project(e: &Env) {
        let (pid, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
        e.app
            .cfg
            .update(move |cfg| {
                cfg.projects.push(crate::config::ProjectCfg {
                    handed_off_to: None,
                    id: Some(pid),
                    path: repo,
                    label: "proj".into(),
                    host: LOCAL_HOST.into(),
                    bots: vec![],
                });
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn add(e: &Env, body: Value) -> Result<String, LcError> {
        let res = create_bot(State(e.app.clone()), Path(e.project_id.clone()), Json(serde_json::from_value(body).unwrap())).await?;
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        Ok(serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string())
    }

    async fn patch(e: &Env, id: &str, body: Value) -> Result<Value, LcError> {
        let res = patch_bot(State(e.app.clone()), Path(id.to_string()), Json(serde_json::from_value(body).unwrap())).await?;
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    /// 這顆 bot 的 `persona` 在 config.toml 與 DB 兩邊各是什麼。
    async fn stored(e: &Env, id: &str) -> (Option<String>, Option<String>) {
        let cfg = e.app.cfg.get().await;
        let in_cfg = cfg.projects[0].bots.iter().find(|b| b.id.as_deref() == Some(id)).unwrap().persona.clone();
        (in_cfg, db::bot(&e.app.db, id).await.unwrap().unwrap().persona)
    }

    #[tokio::test]
    async fn deprecated_models_are_remapped_at_create_and_patch_and_response_says_so() {
        let e = env().await;
        seed_project(&e).await;
        let create = create_bot(
            State(e.app.clone()),
            Path(e.project_id.clone()),
            Json(serde_json::from_value(json!({"name":"old-codex", "kind":"codex", "model":"gpt-5.6-luna"})).unwrap()),
        ).await.unwrap();
        let created: Value = serde_json::from_slice(&axum::body::to_bytes(create.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(created["remapped"]["model"]["from"], "gpt-5.6-luna");
        assert_eq!(created["remapped"]["model"]["to"], "gpt-6-luna");
        let codex_id = created["bot_id"].as_str().unwrap();
        assert_eq!(db::bot(&e.app.db, codex_id).await.unwrap().unwrap().model.as_deref(), Some("gpt-6-luna"));

        for (name, old, replacement) in [
            ("old-codex-sol", "gpt-5.6-sol", "gpt-6-sol"),
            ("old-codex-terra", "gpt-5.6-terra", "gpt-6-sol"),
        ] {
            let response = create_bot(
                State(e.app.clone()),
                Path(e.project_id.clone()),
                Json(serde_json::from_value(json!({"name":name, "kind":"codex", "model":old})).unwrap()),
            )
            .await
            .unwrap();
            let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
            assert_eq!(body["remapped"]["model"]["from"], old);
            assert_eq!(body["remapped"]["model"]["to"], replacement);
            assert_eq!(db::bot(&e.app.db, body["bot_id"].as_str().unwrap()).await.unwrap().unwrap().model.as_deref(), Some(replacement));
        }

        let create_claude = create_bot(
            State(e.app.clone()),
            Path(e.project_id.clone()),
            Json(serde_json::from_value(json!({"name":"old-claude-create", "kind":"claude", "model":"opus"})).unwrap()),
        ).await.unwrap();
        let created_claude: Value = serde_json::from_slice(&axum::body::to_bytes(create_claude.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(created_claude["remapped"]["model"]["to"], "claude-opus-5-5");
        assert_eq!(db::bot(&e.app.db, created_claude["bot_id"].as_str().unwrap()).await.unwrap().unwrap().model.as_deref(), Some("claude-opus-5-5"));

        let claude_id = add(&e, json!({"name":"old-claude", "kind":"claude"})).await.unwrap();
        let patched = patch(&e, &claude_id, json!({"model":"opus"})).await.unwrap();
        assert_eq!(patched["remapped"]["model"]["from"], "opus");
        assert_eq!(patched["remapped"]["model"]["to"], "claude-opus-5-5");
        assert_eq!(db::bot(&e.app.db, &claude_id).await.unwrap().unwrap().model.as_deref(), Some("claude-opus-5-5"));
        let exact = patch(&e, &claude_id, json!({"model":"claude-opus-4-1"})).await.unwrap();
        assert!(exact.get("remapped").is_none(), "versioned model was explicitly chosen: {exact}");
        assert_eq!(db::bot(&e.app.db, &claude_id).await.unwrap().unwrap().model.as_deref(), Some("claude-opus-4-1"));
    }

    /// 最近一次 `agent.start` 的 `--settings` 檔案內容：daemon 真的交給 claude 的那包設定。
    fn settings_of_last_start(e: &Env) -> Value {
        let call = e.herdr.calls_to("agent.start").pop().expect("有起過 agent");
        let args: Vec<String> = call["args"].as_array().unwrap().iter().filter_map(|a| a.as_str().map(String::from)).collect();
        let at = args.iter().position(|a| a == "--settings").expect("claude 帶 --settings") + 1;
        serde_json::from_slice(&std::fs::read(&args[at]).unwrap()).unwrap()
    }

    /// 2026-10-02 使用者決定拿掉「專案指示檔」：新版 claude 自己吃 AGENTS.md，daemon 不再釘 `agents-md` plugin 的 `instructionFiles`，
    /// `--settings` 裡沒有 `pluginConfigs`，CLI 用它自己的預設。舊網頁還送 `instruction_files` 時照收、當沒這個欄位，其他欄位照存。
    #[tokio::test]
    async fn claude_starts_without_pinning_instruction_files_and_a_legacy_field_is_ignored() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(&e, json!({"name": "legacy", "kind": "claude", "persona": "P", "instruction_files": "managed-only"})).await.unwrap();
        assert_eq!(stored(&e, &id).await, (Some("P".into()), Some("P".into())), "舊欄位不影響其他欄位");

        lifecycle::start_bot(&e.app, &id).await.unwrap();
        assert!(settings_of_last_start(&e).get("pluginConfigs").is_none(), "不再釘 instructionFiles");

        let out = patch(&e, &id, json!({"instruction_files": "claude-md-and-agents-md"})).await.unwrap();
        assert_eq!(out["needs_restart"], json!(false), "舊欄位單獨送：什麼都沒改，不要求重啟：{out}");
        let out = patch(&e, &id, json!({"persona": "Q", "instruction_files": "claude-md"})).await.unwrap();
        assert_eq!(out["needs_restart"], json!(true), "{out}");
        assert_eq!(stored(&e, &id).await, (Some("Q".into()), Some("Q".into())));

        let st = state_json(&e.app).await.unwrap();
        let bot = st["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap().clone();
        assert!(bot.get("instruction_files").is_none(), "state 不再輸出這個欄位：{bot}");
    }

    #[tokio::test]
    async fn patch_accepts_full_claude_model_names_and_codex_catalog_models() {
        let e = env().await;
        seed_project(&e).await;
        let claude = add(&e, json!({"name": "claude", "kind": "claude"})).await.unwrap();
        let codex = add(&e, json!({"name": "codex", "kind": "codex"})).await.unwrap();

        for (id, model) in [(&claude, "claude-opus-5-5"), (&codex, "gpt-6-luna")] {
            let out = patch(&e, id, json!({"model": model})).await.unwrap();
            assert_eq!(out["needs_restart"], json!(false), "idle bot accepts {model}: {out}");
            assert_eq!(db::bot(&e.app.db, id).await.unwrap().unwrap().model.as_deref(), Some(model));
        }
    }

    /// #353：改了要重啟才生效的設定，`needs_restart` 不能只存在 PATCH 的 HTTP 回應裡——回應掉了（或 daemon 之後重啟）
    /// 就永遠沒人知道執行中的 CLI 還拿著舊設定。現在從資料算：run 啟動時記下載入的版本，bot 目前的版本對不上＝要重啟。
    #[tokio::test]
    async fn a_config_change_under_a_running_bot_stays_visible_after_the_patch_response_is_lost() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(&e, json!({"name": "a", "kind": "claude"})).await.unwrap();
        let run = lifecycle::start_bot(&e.app, &id).await.unwrap();
        let stamped: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run).fetch_one(&e.app.db).await.unwrap();
        assert!(stamped.is_some(), "run 啟動時記下載入的版本");
        let needs = |e: &Env, id: String| {
            let app = e.app.clone();
            async move {
                let st = state_json(&app).await.unwrap();
                st["projects"].as_array().unwrap().iter().flat_map(|p| p["bots"].as_array().unwrap().clone()).find(|b| b["id"] == json!(id)).unwrap()["needs_restart"].clone()
            }
        };
        assert_eq!(needs(&e, id.clone()).await, json!(false));

        // 「回應掉了」：呼叫端沒拿到 needs_restart，我們也不看它——只看資料。
        let _lost = patch(&e, &id, json!({"persona": "換一份人設 A"})).await.unwrap();
        assert_eq!(needs(&e, id.clone()).await, json!(true), "改了設定、run 還是舊版：從資料看得出要重啟");

        // 重啟＝新 run 載入新版本，不再過期。
        lifecycle::restart_bot(&e.app, &id).await.unwrap();
        assert_eq!(needs(&e, id.clone()).await, json!(false));

        // 只改可以當場套用的欄位（claude 的 model → /model）：套用成功就是已生效，不能被誤判成過期。
        let out = patch(&e, &id, json!({"model": "opus"})).await.unwrap();
        if out.get("live_apply").is_some_and(|l| l["applied"] == json!(true)) {
            assert_eq!(needs(&e, id.clone()).await, json!(false), "當場套用成功：run 的版本跟著更新");
        }

        // 升版前的舊 run（沒記版本）：改設定那一刻補記「改之前」的版本，之後照樣看得出。
        sqlx::query("UPDATE runs SET launch_rev = NULL WHERE bot_id = ? AND state IN ('starting','running')").bind(&id).execute(&e.app.db).await.unwrap();
        assert_eq!(needs(&e, id.clone()).await, json!(false), "沒記版本＝不誤報");
        let _ = patch(&e, &id, json!({"persona": "換一份人設"})).await.unwrap();
        assert_eq!(needs(&e, id.clone()).await, json!(true), "舊 run 也在改設定的那刻開始被追蹤");
    }

    /// 稽核：沒有當場套用可走的欄位（persona 等），PATCH 回應的 `needs_restart` 只看「有 active run 且改了這類欄位」，
    /// 不看值真的變沒變：改成跟現在一樣、或改回 run 啟動時的值，回應說要重啟，`/state` 卻（照 launch_rev）說不用。
    #[tokio::test]
    async fn a_patch_that_leaves_the_running_bots_settings_as_loaded_does_not_ask_for_a_restart() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(&e, json!({"name": "a", "kind": "claude", "persona": "P"})).await.unwrap();
        lifecycle::start_bot(&e.app, &id).await.unwrap();

        let out = patch(&e, &id, json!({"persona": "P"})).await.unwrap();
        assert_eq!(out["needs_restart"], json!(false), "跟載入的一樣：不必重啟 {out}");
        let out = patch(&e, &id, json!({"persona": "Q"})).await.unwrap();
        assert_eq!(out["needs_restart"], json!(true), "{out}");
        let out = patch(&e, &id, json!({"persona": "P"})).await.unwrap();
        assert_eq!(out["needs_restart"], json!(false), "改回 run 載入的值：不必重啟 {out}");
    }

    #[tokio::test]
    async fn patch_does_not_commit_when_the_old_launch_revision_cannot_be_recorded() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(&e, json!({"name": "baseline", "kind": "claude", "persona": "A"})).await.unwrap();
        let run_id = lifecycle::start_bot(&e.app, &id).await.unwrap();
        let old_rev = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
        sqlx::query("UPDATE runs SET launch_rev = NULL WHERE id = ?").bind(&run_id).execute(&e.app.db).await.unwrap();
        sqlx::query(&format!(
            "CREATE TRIGGER refuse_launch_baseline BEFORE UPDATE OF launch_rev ON runs
             WHEN OLD.id = '{}' AND OLD.launch_rev IS NULL AND NEW.launch_rev IS NOT NULL
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
            run_id
        ))
        .execute(&e.app.db)
        .await
        .unwrap();

        let before = stored(&e, &id).await;
        let err = patch(&e, &id, json!({"persona": "B"})).await.unwrap_err();
        assert!(matches!(err, LcError::Unavailable(ref v) if v["reason"] == "launch_revision_baseline_failed"), "expected retryable refusal: {err:?}");
        assert_eq!(stored(&e, &id).await, before, "failed baseline write must leave TOML and DB projection unchanged");
        let launch_rev: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run_id).fetch_one(&e.app.db).await.unwrap();
        assert!(launch_rev.is_none(), "the failed stamp must not manufacture a baseline");

        sqlx::query("DROP TRIGGER refuse_launch_baseline").execute(&e.app.db).await.unwrap();
        let retried = patch(&e, &id, json!({"persona": "B"})).await.unwrap();
        assert_eq!(retried["needs_restart"], json!(true));
        assert_eq!(stored(&e, &id).await, (Some("B".into()), Some("B".into())), "retry stamps A before committing B");
        let launch_rev: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(launch_rev.as_deref(), Some(old_rev.as_str()), "retry must stamp the revision from config A before committing config B");
    }

    #[tokio::test]
    async fn live_apply_stamps_the_run_selected_after_the_patch_snapshot() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(&e, json!({"name": "run-bound", "kind": "claude", "model": "claude-sonnet-4-5"})).await.unwrap();
        let run_a = tt::fake_run(&e.app, &id).await;
        let old_rev = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
        sqlx::query("UPDATE runs SET launch_rev = ?, runtime_model = 'claude-sonnet-4-5' WHERE id = ?")
            .bind(&old_rev)
            .bind(&run_a)
            .execute(&e.app.db)
            .await
            .unwrap();

        let run_b = db::ulid();
        let pane_b = format!("pane-{run_b}");
        e.herdr.set_screen(&pane_b, "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch to Claude Opus 5.5\n  2. No, go back\n");
        let screens = e.herdr.screens.clone();
        let pane_after_answer = pane_b.clone();
        crate::lifecycle::race_point::arm("slash_after_answer_before_read", &pane_b, move || async move {
            screens.lock().unwrap().insert(pane_after_answer, "Claude Code\n❯\n".into());
        });

        let app = e.app.clone();
        let bot_for_rollover = id.clone();
        let run_a_for_rollover = run_a.clone();
        let run_b_for_rollover = run_b.clone();
        let pane_for_rollover = pane_b.clone();
        let baseline = old_rev.clone();
        crate::lifecycle::race_point::arm("patch_after_active_snapshot", &id, move || async move {
            sqlx::query("UPDATE runs SET state = 'exited', ended_at = ? WHERE id = ?")
                .bind(db::now())
                .bind(&run_a_for_rollover)
                .execute(&app.db)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, started_at, launch_rev, runtime_model)
                 VALUES (?, ?, 'running', 'idle', ?, ?, ?, 'claude-sonnet-4-5')",
            )
            .bind(&run_b_for_rollover)
            .bind(&bot_for_rollover)
            .bind(&pane_for_rollover)
            .bind(db::now())
            .bind(&baseline)
            .execute(&app.db)
            .await
            .unwrap();
        });

        let out = patch(&e, &id, json!({"model": "claude-opus-5-5"})).await.unwrap();
        assert_eq!(out["live_apply"]["applied"], json!(true), "the replacement run accepted the live model: {out}");
        assert_eq!(out["needs_restart"], json!(false), "the run receiving the live apply must not keep a false restart badge");
        let a_rev: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run_a).fetch_one(&e.app.db).await.unwrap();
        let b_rev: Option<String> = sqlx::query_scalar("SELECT launch_rev FROM runs WHERE id = ?").bind(&run_b).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(a_rev.as_deref(), Some(old_rev.as_str()), "bookkeeping must not stamp the stale PATCH snapshot");
        assert_eq!(b_rev.as_deref(), Some(crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap()).as_str()), "bookkeeping must stamp the actual live target run");
        let state = state_json(&e.app).await.unwrap();
        let needs_restart = state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap()["needs_restart"].clone();
        assert_eq!(needs_restart, json!(false));
    }

    #[tokio::test]
    async fn a_deferred_live_apply_keeps_its_stamp_retry_after_launch_write_fails() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(
            &e,
            json!({"name": "deferred-stamp-retry", "kind": "codex", "model": "gpt-6-luna", "effort": "max", "fast": false}),
        )
        .await
        .unwrap();
        let run_id = tt::fake_run(&e.app, &id).await;
        let pane = format!("pane-{id}");
        let baseline = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
        sqlx::query(
            "UPDATE runs SET pane_id = ?, launch_rev = ?, runtime_model = 'gpt-6-luna', runtime_effort = 'max', runtime_fast = 0, agent_status = 'working' WHERE id = ?",
        )
        .bind(&pane)
        .bind(&baseline)
        .bind(&run_id)
        .execute(&e.app.db)
        .await
        .unwrap();

        // 回合中只切 fast 會直接送（#712）；輸入框裡有使用者的草稿就不碰，排到回合結束——這條測的是排隊那條路。
        e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
        let out = patch(&e, &id, json!({"fast": true}))
            .await
            .unwrap();
        assert_eq!(out["live_apply"]["deferred"], json!(true), "busy run should queue this live apply: {out}");
        assert!(e.herdr.calls_to("pane.send_text").is_empty(), "草稿還在框裡：一個字都不打");
        assert_eq!(out["needs_restart"], json!(false), "waiting for idle must not ask for restart: {out}");
        let pending_state = state_json(&e.app).await.unwrap();
        let pending_bot = pending_state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap();
        assert_eq!(pending_bot["needs_restart"], json!(false));
        assert_eq!(pending_bot["live_apply_deferred"], json!(true));
        assert_eq!(db::bot(&e.app.db, &id).await.unwrap().unwrap().fast, 1);
        let target = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
        sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?")
            .bind(&run_id)
            .execute(&e.app.db)
            .await
            .unwrap();
        e.herdr.set_screen(&pane, "gpt-6-luna max · /tmp · Context 43% used · 5h 12% left\n");
        let replace_screen = e.herdr.set_screen_later();
        let toggled_screen = "gpt-6-luna max fast · /tmp · Context 43% used · 5h 12% left\n";
        let pane_after_toggle = pane.clone();
        crate::lifecycle::race_point::arm(
            "codex_after_fast_toggle",
            &pane,
            move || async move {
                replace_screen(&pane_after_toggle, toggled_screen);
            },
        );
        sqlx::query(&format!(
            "CREATE TRIGGER refuse_deferred_launch_stamp BEFORE UPDATE OF launch_rev ON runs
             WHEN OLD.id = '{}' AND NEW.launch_rev = '{}'
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
            run_id, target
        ))
        .execute(&e.app.db)
        .await
        .unwrap();
        assert_eq!(
            crate::codex_live::parse_status_line(toggled_screen).map(|seen| seen.fast),
            Some(true),
            "the pane fixture must show Codex's confirmed fast tier"
        );

        let deferred_outcome = lifecycle::apply_deferred_once(&e.app, &id).await;
        assert!(
            matches!(deferred_outcome, Some(lifecycle::LiveApplyOutcome::Applied { .. })),
            "the deferred TUI readback must succeed before stamp debt exists: {deferred_outcome:?}"
        );
        let applied_state = state_json(&e.app).await.unwrap();
        let applied_bot = applied_state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap();
        assert_eq!(applied_bot["live_apply_deferred"], json!(false));

        let sent_before_retry = e.herdr.calls_to("pane.send_keys").len();
        assert_eq!(sent_before_retry, 1, "the deferred fast toggle ran exactly once");
        assert_eq!(
            e.herdr.screens.lock().unwrap().get(&pane).map(String::as_str),
            Some(toggled_screen),
            "the exact post-toggle pane revision must be visible at readback"
        );

        let (runtime, runtime_fast, launch_rev, live_rev): (
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT runtime_model, runtime_fast, launch_rev, live_rev FROM runs WHERE id = ?",
        )
        .bind(&run_id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert_eq!(runtime.as_deref(), Some("gpt-6-luna"));
        assert_eq!(
            live_rev.as_deref(),
            Some(target.as_str()),
            "a successful live readback must retain durable stamp retry debt"
        );
        assert_eq!(launch_rev.as_deref(), Some(baseline.as_str()));
        assert_eq!(runtime_fast, Some(1), "the readback snapshot includes the changed fast tier");
        let state = state_json(&e.app).await.unwrap();
        let shown = state["projects"][0]["bots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == json!(id))
            .unwrap();
        assert_eq!(
            shown["needs_restart"],
            json!(false),
            "the matching persisted live revision must prevent a false restart badge"
        );

        sqlx::query("DROP TRIGGER refuse_deferred_launch_stamp")
            .execute(&e.app.db)
            .await
            .unwrap();
        let _ = lifecycle::retry_live_apply_bookkeeping_once(&e.app, &run_id)
            .await
            .unwrap();
        let (runtime_fast, launch_rev, live_rev): (Option<i64>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT runtime_fast, launch_rev, live_rev FROM runs WHERE id = ?")
                .bind(&run_id)
                .fetch_one(&e.app.db)
                .await
                .unwrap();
        assert_eq!(runtime_fast, Some(1));
        assert_eq!(launch_rev.as_deref(), Some(target.as_str()));
        assert_eq!(live_rev, None);
        assert_eq!(
            e.herdr.calls_to("pane.send_keys").len(),
            sent_before_retry,
            "retry changes bookkeeping only and never resends to the TUI"
        );
    }

    const CODEX_WORKING_ANSI: &str = include_str!("lifecycle/fixtures/codex-0.155-working.ansi");
    const CODEX_DRAFT_ANSI: &str = include_str!("lifecycle/fixtures/codex-0.155-draft.ansi");

    /// 真的回合中畫面（輸入框空的），最後一列換成 0.157 的狀態列：印的是顯示名 `GPT-6-Luna`，不是 id。
    fn codex_working_screen(status: &str) -> String {
        let mut rows: Vec<&str> = CODEX_WORKING_ANSI.lines().collect();
        rows.pop();
        rows.push(status);
        rows.join("\n") + "\n"
    }

    /// 在跑一個回合的 codex bot（#712）：model／effort 已對上，fast 關。回 (bot id, run id, pane)。
    async fn busy_codex(e: &tt::Env, name: &str) -> (String, String, String) {
        seed_project(e).await;
        let id = add(e, json!({"name": name, "kind": "codex", "model": "gpt-6-luna", "effort": "max", "fast": false})).await.unwrap();
        let run_id = tt::fake_run(&e.app, &id).await;
        let pane = format!("pane-{id}");
        let baseline = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
        sqlx::query(
            "UPDATE runs SET pane_id = ?, launch_rev = ?, runtime_model = 'gpt-6-luna', runtime_effort = 'max', runtime_fast = 0, agent_status = 'working' WHERE id = ?",
        )
        .bind(&pane)
        .bind(&baseline)
        .bind(&run_id)
        .execute(&e.app.db)
        .await
        .unwrap();
        (id, run_id, pane)
    }

    /// #712：回合中只切 fast——codex 0.157.1 的 `/fast` 回合中照樣生效，不排隊、不要求重啟、不按 Esc（Esc＝中斷回合）。
    /// 狀態列印顯示名 `GPT-6-Luna` 也要讀回得過（以前大小寫不同就判 `readback_model_mismatch`、退回重啟）。
    #[tokio::test]
    async fn a_busy_fast_only_change_goes_in_during_the_turn() {
        let e = env().await;
        let (id, run_id, pane) = busy_codex(&e, "fast-mid-turn").await;
        e.herdr.set_screen(&pane, &codex_working_screen("  GPT-6-Luna max · /tmp · Context 43% used · 5h 12% left"));
        let replace_screen = e.herdr.set_screen_later();
        let toggled = codex_working_screen("  GPT-6-Luna max fast · /tmp · Context 43% used · 5h 12% left");
        let pane_after = pane.clone();
        crate::lifecycle::race_point::arm("codex_after_fast_toggle", &pane, move || async move {
            replace_screen(&pane_after, &toggled);
        });

        let out = patch(&e, &id, json!({"fast": true})).await.unwrap();
        assert_eq!(out["live_apply"]["applied"], json!(true), "回合中直接套用: {out}");
        assert_eq!(out["live_apply"]["deferred"], json!(false), "{out}");
        assert_eq!(out["needs_restart"], json!(false), "{out}");
        let typed: Vec<Value> = e.herdr.calls_to("pane.send_text");
        assert_eq!(typed.len(), 1, "一次 /fast: {typed:?}");
        assert_eq!(typed[0]["text"], json!("/fast"));
        let keys = e.herdr.calls_to("pane.send_keys");
        assert!(!format!("{keys:?}").contains("Escape"), "回合中絕不按 Esc（會中斷回合）: {keys:?}");
        let (runtime_model, runtime_fast): (Option<String>, Option<i64>) =
            sqlx::query_as("SELECT runtime_model, runtime_fast FROM runs WHERE id = ?").bind(&run_id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(runtime_model.as_deref(), Some("gpt-6-luna"), "顯示名轉成 id 的寫法");
        assert_eq!(runtime_fast, Some(1));
        let state = state_json(&e.app).await.unwrap();
        let shown = state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).unwrap();
        assert_eq!(shown["needs_restart"], json!(false));
        assert_eq!(shown["live_apply_deferred"], json!(false));
    }

    /// #712：回合中輸入框有使用者的草稿——打 `/fast` 會接在草稿後面、Enter 把整段送出去。不碰，排到回合結束，也不要求重啟。
    #[tokio::test]
    async fn a_busy_fast_change_waits_when_the_composer_holds_a_draft() {
        let e = env().await;
        let (id, _run_id, pane) = busy_codex(&e, "fast-draft").await;
        e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
        let out = patch(&e, &id, json!({"fast": true})).await.unwrap();
        assert_eq!(out["live_apply"]["deferred"], json!(true), "{out}");
        assert_eq!(out["live_apply"]["reason"], json!("codex: busy_not_ready"), "{out}");
        assert_eq!(out["needs_restart"], json!(false), "{out}");
        assert!(e.herdr.calls_to("pane.send_text").is_empty(), "一個字都不打");
        assert!(e.herdr.calls_to("pane.send_keys").is_empty(), "一個鍵都不按");
        assert!(lifecycle::is_deferred(&id));
        lifecycle::apply_deferred_once(&e.app, &id).await; // 清掉全域排程，不留給別的測試
    }

    /// 稽核：閒著的 codex 輸入框裡有使用者的草稿時，`apply`（model／effort／fast 當場套用）不能把 `/model`、`/fast` 打在草稿後面——
    /// 接在草稿後面的 Enter 會把整段「草稿/fast」當 prompt 送出去。回合中那條（#712）有這道檢查，閒著這條沒有。
    #[tokio::test]
    async fn an_idle_codex_live_apply_never_types_after_a_users_draft() {
        for patch_body in [json!({"fast": true}), json!({"effort": "high"})] {
            let e = env().await;
            let (id, run_id, pane) = busy_codex(&e, "idle-draft").await;
            sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?").bind(&run_id).execute(&e.app.db).await.unwrap();
            e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
            let out = patch(&e, &id, patch_body.clone()).await.unwrap();
            assert_eq!(out["live_apply"]["applied"], json!(false), "{patch_body}: {out}");
            assert!(e.herdr.calls_to("pane.send_text").is_empty(), "{patch_body}: 草稿後面不能再打字: {:?}", e.herdr.calls_to("pane.send_text"));
            assert!(e.herdr.calls_to("pane.send_keys").is_empty(), "{patch_body}: 一個鍵都不按（Enter 會送出草稿）: {:?}", e.herdr.calls_to("pane.send_keys"));
        }
    }

    /// 排著的 live 套用在 `limit` 內被取走（`schedule_deferred_live` 延遲 1.5 秒後 `take`）。
    async fn deferred_taken_within(id: &str, limit: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + limit;
        while std::time::Instant::now() < deadline {
            if !lifecycle::is_deferred(id) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        false
    }

    /// #712：agent 早就 idle、只剩回合紀錄還在飛時排下的，等不到 idle 邊——回合收掉（`emit_turn`）就要套。
    #[tokio::test]
    async fn a_fast_change_deferred_behind_an_in_flight_turn_applies_when_the_turn_closes() {
        let e = env().await;
        let (id, run_id, pane) = busy_codex(&e, "fast-turn-close").await;
        let conv = db::conversation_id(&e.app.db, &id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&turn_id)
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&e.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?").bind(&run_id).execute(&e.app.db).await.unwrap();
        e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
        let out = patch(&e, &id, json!({"fast": true})).await.unwrap();
        assert_eq!(out["live_apply"]["deferred"], json!(true), "{out}");
        assert!(!deferred_taken_within(&id, std::time::Duration::from_millis(2500)).await, "回合還在飛：不動");

        e.herdr.set_screen(&pane, "  gpt-6-luna max · /tmp · Context 43% used · 5h 12% left\n");
        sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?").bind(&turn_id).execute(&e.app.db).await.unwrap();
        lifecycle::emit_turn(&e.app, &turn_id).await;
        assert!(deferred_taken_within(&id, std::time::Duration::from_secs(6)).await, "回合收掉那一刻就該套");
    }

    /// #712：blocked（權限框）→ idle 也是一條 idle 邊：排著的 fast 要套，不是只等 working → idle。
    #[tokio::test]
    async fn a_deferred_fast_change_applies_after_a_blocked_prompt_closes() {
        let e = env().await;
        let (id, run_id, pane) = busy_codex(&e, "fast-after-blocked").await;
        e.herdr.set_screen(&pane, CODEX_DRAFT_ANSI);
        let out = patch(&e, &id, json!({"fast": true})).await.unwrap();
        assert_eq!(out["live_apply"]["deferred"], json!(true), "{out}");
        sqlx::query("UPDATE runs SET agent_status = 'blocked' WHERE id = ?").bind(&run_id).execute(&e.app.db).await.unwrap();
        e.herdr.set_screen(&pane, "  gpt-6-luna max · /tmp · Context 43% used · 5h 12% left\n");
        let ev = crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"pane_id": pane, "agent_status": "idle"}),
        };
        crate::events::handle_status(&e.app, crate::config::LOCAL_HOST, "test", &ev).await;
        assert!(deferred_taken_within(&id, std::time::Duration::from_secs(6)).await, "blocked → idle 就該套");
    }

    /// #712：claude 忙的時候改 model 排到回合結束、不要求重啟；再改 effort（slash 指令一次一個值）不合併，照舊要重啟。
    #[tokio::test]
    async fn a_busy_claude_defers_one_field_and_asks_for_restart_on_a_second() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(&e, json!({"name": "claude-busy", "kind": "claude", "model": "claude-sonnet-4-5", "effort": "high"})).await.unwrap();
        let run_id = tt::fake_run(&e.app, &id).await;
        let baseline = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
        sqlx::query("UPDATE runs SET pane_id = ?, launch_rev = ?, agent_status = 'working' WHERE id = ?")
            .bind(format!("pane-{id}"))
            .bind(&baseline)
            .bind(&run_id)
            .execute(&e.app.db)
            .await
            .unwrap();
        let first = patch(&e, &id, json!({"model": "claude-opus-5-5"})).await.unwrap();
        assert_eq!(first["live_apply"]["deferred"], json!(true), "{first}");
        assert_eq!(first["needs_restart"], json!(false), "{first}");
        let second = patch(&e, &id, json!({"effort": "low"})).await.unwrap();
        assert_eq!(second["live_apply"]["deferred"], json!(false), "{second}");
        assert_eq!(second["needs_restart"], json!(true), "{second}");
        lifecycle::apply_deferred_once(&e.app, &id).await;
    }

    #[tokio::test]
    async fn a_live_apply_with_a_failed_runtime_write_is_not_reported_as_applied() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(
            &e,
            json!({"name": "runtime-write-fails", "kind": "claude", "model": "claude-sonnet-4-5"}),
        )
        .await
        .unwrap();
        let run_id = tt::fake_run(&e.app, &id).await;
        let pane = format!("pane-{id}");
        let baseline = crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap());
        sqlx::query("UPDATE runs SET pane_id = ?, launch_rev = ?, runtime_model = 'claude-sonnet-4-5' WHERE id = ?")
            .bind(&pane)
            .bind(&baseline)
            .bind(&run_id)
            .execute(&e.app.db)
            .await
            .unwrap();
        e.herdr.set_screen(&pane, "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch to Claude Opus 5.5\n  2. No, go back\n");
        let screens = e.herdr.screens.clone();
        let pane_after_answer = pane.clone();
        crate::lifecycle::race_point::arm(
            "slash_after_answer_before_read",
            &pane,
            move || async move {
                screens
                    .lock()
                    .unwrap()
                    .insert(pane_after_answer, "Claude Code\n❯\n".into());
            },
        );
        sqlx::query(&format!(
            "CREATE TRIGGER refuse_live_runtime BEFORE UPDATE OF runtime_model ON runs
             WHEN OLD.id = '{}' AND NEW.runtime_model = 'claude-opus-5-5'
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
            run_id
        ))
        .execute(&e.app.db)
        .await
        .unwrap();

        let mut events = e.app.subscribe();
        let out = patch(&e, &id, json!({"model": "claude-opus-5-5"}))
            .await
            .unwrap();

        assert_eq!(
            out["live_apply"]["applied"],
            json!(false),
            "runtime bookkeeping failed, so PATCH cannot claim a completed live apply: {out}"
        );
        assert_eq!(
            out["live_apply"]["pending_bookkeeping"],
            json!(true),
            "the observed TUI result must become DB-only retry debt: {out}"
        );
        assert_eq!(
            out["needs_restart"],
            json!(true),
            "failed runtime persistence cannot clear launch drift: {out}"
        );
        let runtime: Option<String> =
            sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id = ?")
                .bind(&run_id)
                .fetch_one(&e.app.db)
                .await
                .unwrap();
        assert_eq!(
            runtime.as_deref(),
            Some("claude-sonnet-4-5"),
            "the rejected UPDATE leaves the prior runtime snapshot intact"
        );
        while let Ok(event) = events.try_recv() {
            assert_ne!(
                event.kind, "bot_status",
                "runtime status cannot be published before the runtime row commits"
            );
        }
        let debt: (String, String, String) = sqlx::query_as(
            "SELECT run_id, target_rev, runtime_model FROM live_apply_debts WHERE run_id = ?",
        )
        .bind(&run_id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert_eq!(debt.0, run_id);
        assert_eq!(
            debt.1,
            crate::launch_rev::of(&db::bot(&e.app.db, &id).await.unwrap().unwrap())
        );
        assert_eq!(
            debt.2, "claude-opus-5-5",
            "the TUI readback snapshot, not a new TUI command, is the retry input"
        );
        let sent_before = e.herdr.calls_to("pane.send_text").len();
        assert_eq!(sent_before, 1, "the live command ran once");

        sqlx::query("DROP TRIGGER refuse_live_runtime")
            .execute(&e.app.db)
            .await
            .unwrap();
        let _ = lifecycle::retry_live_apply_bookkeeping_once(&e.app, &run_id)
            .await
            .unwrap();

        let (runtime, launch_rev, live_rev): (Option<String>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT runtime_model, launch_rev, live_rev FROM runs WHERE id = ?")
                .bind(&run_id)
                .fetch_one(&e.app.db)
                .await
                .unwrap();
        assert_eq!(runtime.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(
            launch_rev.as_deref(),
            Some(debt.1.as_str()),
            "retry stamps the exact run after its runtime snapshot commits"
        );
        assert_eq!(live_rev, None, "successful stamp clears its proof marker");
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM live_apply_debts WHERE run_id = ?")
                .bind(&run_id)
                .fetch_one(&e.app.db)
                .await
                .unwrap();
        assert_eq!(
            remaining, 0,
            "the debt clears only with the successful runtime transaction"
        );
        assert_eq!(
            e.herdr.calls_to("pane.send_text").len(),
            sent_before,
            "recovery is bookkeeping only; no slash resend"
        );
        let mut committed_status = None;
        while let Ok(event) = events.try_recv() {
            if event.kind == "bot_status" {
                committed_status = Some(event);
            }
        }
        let committed_status = committed_status.expect("committed runtime row should publish bot_status");
        assert_eq!(
            committed_status.data["run"]["runtime_model"],
            json!("claude-opus-5-5"),
            "bot_status must only carry the committed runtime snapshot"
        );
        let state = state_json(&e.app).await.unwrap();
        let shown = state["projects"][0]["bots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == json!(id))
            .unwrap();
        assert_eq!(shown["needs_restart"], json!(false));
    }

    /// 刪掉再還原：設定跟著回來（還原是從 DB 那一列重建 config 條目，漏抄欄位會在這裡悄悄變回預設）。
    #[tokio::test]
    async fn restoring_a_deleted_bot_keeps_its_persona() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(&e, json!({"name": "keep", "kind": "claude", "persona": "P"})).await.unwrap();

        delete_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();
        assert!(e.app.cfg.get().await.projects[0].bots.iter().all(|b| b.id.as_deref() != Some(id.as_str())), "刪掉之後 config 裡沒有了");
        restore_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();

        assert_eq!(stored(&e, &id).await, (Some("P".into()), Some("P".into())));
    }

    /// #295：還原不能把讀不懂的 args／env 當成空的寫進 config，也不能把 herdr_session 洗成 NULL。
    #[tokio::test]
    async fn restoring_keeps_env_args_and_session_and_refuses_unreadable_json() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(&e, json!({"name": "keep2", "kind": "claude"})).await.unwrap();
        delete_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();
        sqlx::query(r#"UPDATE bots SET herdr_session='sess', env_json='{"A":"1"}', args_json='["--x"]' WHERE id=?"#).bind(&id).execute(&e.app.db).await.unwrap();
        restore_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();
        let cfg = e.app.cfg.get().await;
        let b = cfg.projects[0].bots.iter().find(|b| b.id.as_deref() == Some(id.as_str())).unwrap();
        assert_eq!(b.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(b.args, vec!["--x".to_string()]);
        assert_eq!(b.herdr_session.as_deref(), Some("sess"));
        assert_eq!(db::bot(&e.app.db, &id).await.unwrap().unwrap().herdr_session.as_deref(), Some("sess"));

        delete_bot(State(e.app.clone()), Path(id.clone())).await.unwrap();
        sqlx::query("UPDATE bots SET env_json='not json' WHERE id=?").bind(&id).execute(&e.app.db).await.unwrap();
        assert!(restore_bot(State(e.app.clone()), Path(id.clone())).await.is_err(), "讀不懂的 env 不能還原成空的");
        assert!(db::bot(&e.app.db, &id).await.unwrap().unwrap().deleted_at.is_some());
        assert!(e.app.cfg.get().await.projects[0].bots.iter().all(|b| b.id.as_deref() != Some(id.as_str())));
    }

    /// 舊的 config.toml 還留著 `instruction_files = …`：照樣讀得進來、投影得動，bot 不受影響。
    #[tokio::test]
    async fn a_leftover_instruction_files_key_in_config_toml_is_ignored() {
        let e = env().await;
        seed_project(&e).await;
        let id = db::ulid();
        let bid = id.clone();
        e.app
            .cfg
            .update(move |cfg| {
                let b: crate::config::BotCfg = toml::from_str(&format!("id = '{bid}'\nname = 'old'\nkind = 'claude'\ninstruction_files = 'managed-only'\n")).unwrap();
                cfg.projects[0].bots.push(b);
                Ok(())
            })
            .await
            .unwrap();
        crate::projection::project_config(&e.app.cfg, &e.app.db).await.unwrap();
        lifecycle::start_bot(&e.app, &id).await.unwrap();
        assert!(settings_of_last_start(&e).get("pluginConfigs").is_none());
    }
}

/// #243：pane-purpose 回報遇到 host 讀不到，要 503 讓 shim 重試——不能把遠端 pane id 寫進 `host='local'`。
#[cfg(test)]
mod relay_pane_host_tests {
    use super::*;

    #[tokio::test]
    async fn an_unreadable_bot_host_is_a_retryable_error_and_writes_no_pane_row() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let bot = crate::testing::claude_bot(&app, &e.project_id, "alfa").await;
        let report = || {
            let mut h = HeaderMap::new();
            h.insert("X-AM-Bot-Token", "tok".parse().unwrap());
            let body = RelayPane { bot_id: bot.id.clone(), pane_id: "w1-9".into(), purpose: "build".into() };
            relay_pane(State(app.clone()), h, axum::extract::Form(body))
        };
        let panes = || async { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM panes").fetch_one(&app.db).await.unwrap() };

        crate::testing::make_table_unreadable(&app, "projects").await;
        let (code, _) = report().await;
        crate::testing::make_table_readable(&app, "projects").await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(panes().await, 0, "不能寫出任何 pane 列");

        let (code, _) = report().await;
        assert_eq!(code, StatusCode::OK, "DB 好了重送就成功");
        let host: String = sqlx::query_scalar("SELECT host FROM panes WHERE pane_id='w1-9'").fetch_one(&app.db).await.unwrap();
        assert_eq!(host, "local");
    }
}

#[cfg(test)]
mod unknown_api_route_tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn raw(app: std::sync::Arc<crate::state::App>, req: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = super::router(app);
        let server = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
        });
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        c.write_all(format!("{req} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        server.abort();
        out
    }

    /// 打錯路徑（或新前端打舊 daemon 還沒有的端點）以前拿到 200 的 index.html：呼叫端看不出「沒這個端點」，
    /// 而且不帶 token 也回 200。API.md §1：找不到是 404 JSON。
    #[tokio::test]
    async fn an_unknown_api_path_is_a_json_404_not_the_spa_shell() {
        let env = crate::testing::env().await;
        for req in ["GET /api/no-such-endpoint", "POST /api/no-such-endpoint", "GET /api/bots/x/no-such-sub"] {
            let out = raw(env.app.clone(), req).await;
            assert!(out.starts_with("HTTP/1.1 404"), "{req}: {out}");
            assert!(out.contains("\"error\":\"not_found\""), "{req}: {out}");
        }
    }
}

#[cfg(test)]
mod per_principal_auth_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// `testing::claude_bot` gives every bot the same `hook_token` ('tok'); identity tests need
    /// each bot's token to be its own, or "someone else's token" is indistinguishable from "mine".
    async fn distinct_bot(e: &crate::testing::Env, name: &str) -> db::Bot {
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
        sqlx::query("UPDATE bots SET hook_token=? WHERE id=?").bind(format!("tok-{}", bot.id)).bind(&bot.id).execute(&e.app.db).await.unwrap();
        db::bot(&e.app.db, &bot.id).await.unwrap().unwrap()
    }

    async fn raw(app: Arc<App>, request: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = super::router(app);
        let server = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
        });
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        c.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        server.abort();
        out
    }

    async fn spawn_begin(app: Arc<App>, bot_id: &str, token: &str) -> String {
        let body = format!("bot_id={bot_id}");
        raw(
            app,
            format!(
                "POST /relay/spawn/begin HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Token: {token}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await
    }

    async fn spawn_finish(app: Arc<App>, bot_id: &str, token: &str, permit_id: &str, pane_id: &str) -> String {
        let body = format!("bot_id={bot_id}&permit_id={permit_id}&pane_id={pane_id}&purpose=child");
        raw(
            app,
            format!(
                "POST /relay/spawn/finish HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Token: {token}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await
    }

    fn response_json(response: &str) -> Value {
        serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap_or("{}")).unwrap()
    }

    async fn state(app: Arc<App>, headers: &[(&str, &str)]) -> String {
        let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        raw(
            app,
            format!("GET /api/state HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\nContent-Length: 0\r\n\r\n"),
        )
        .await
    }

    #[tokio::test]
    async fn a_bot_id_and_its_token_authenticate_without_the_ui_token() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "bot-auth").await;
        let response = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &bot.hook_token)]).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    }

    #[tokio::test]
    async fn a_bot_id_without_its_token_cannot_fall_back_to_a_valid_ui_token() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "missing-bot-token").await;
        let response = state(e.app.clone(), &[("X-AM-Token", &e.app.ui_token), ("X-AM-Bot-Id", &bot.id)]).await;
        assert!(response.starts_with("HTTP/1.1 401") || response.starts_with("HTTP/1.1 403"), "{response}");
    }

    #[tokio::test]
    async fn a_wrong_bot_token_cannot_fall_back_to_a_valid_ui_token() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "wrong-bot-token").await;
        let response = state(
            e.app.clone(),
            &[("X-AM-Token", &e.app.ui_token), ("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", "wrong")],
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 401"), "invalid Bot proof must not downgrade to User: {response}");
    }

    #[tokio::test]
    async fn an_authenticated_bot_with_no_relay_from_is_stored_as_that_bot() {
        let e = crate::testing::env().await;
        let sender = distinct_bot(&e, "sender").await;
        let target = distinct_bot(&e, "target").await;
        sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&target.id).execute(&e.app.db).await.unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-principal','target','test',1,?)",
        )
        .bind(db::ulid())
        .bind(&target.id)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        e.herdr.live_pane("pane-principal", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });

        let body = json!({ "text": "bot-authored", "client_request_id": "bot-authored-no-relay" });
        let bytes = serde_json::to_vec(&body).unwrap();
        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                target.id,
                sender.id,
                sender.hook_token,
                bytes.len(),
                String::from_utf8(bytes).unwrap(),
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let payload = response.split("\r\n\r\n").nth(1).unwrap_or_default();
        let payload: Value = serde_json::from_str(payload).unwrap();
        let message_id = payload["message_id"].as_str().expect("prompt response message id");
        let relay: Option<String> = sqlx::query_scalar("SELECT relay_from FROM messages WHERE id=?")
            .bind(message_id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(relay.as_deref(), Some(sender.id.as_str()));
    }

    #[tokio::test]
    async fn an_empty_relay_from_cannot_override_the_authenticated_bot_identity() {
        let e = crate::testing::env().await;
        let sender = distinct_bot(&e, "empty-relay-sender").await;
        let target = distinct_bot(&e, "empty-relay-target").await;
        sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&target.id).execute(&e.app.db).await.unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-empty-relay','target','test',1,?)",
        )
        .bind(db::ulid())
        .bind(&target.id)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        e.herdr.live_pane("pane-empty-relay", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });

        let body = serde_json::to_vec(&json!({ "text": "empty-relay-authored", "client_request_id": "empty-relay-authored", "relay_from": "" })).unwrap();
        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                target.id,
                sender.id,
                sender.hook_token,
                body.len(),
                String::from_utf8(body).unwrap(),
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let payload = response.split("\r\n\r\n").nth(1).unwrap_or_default();
        let payload: Value = serde_json::from_str(payload).unwrap();
        let message_id = payload["message_id"].as_str().expect("prompt response message id");
        let relay: Option<String> = sqlx::query_scalar("SELECT relay_from FROM messages WHERE id=?")
            .bind(message_id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(relay.as_deref(), Some(sender.id.as_str()), "empty body claim must not erase the authenticated bot identity");
    }

    #[tokio::test]
    async fn a_service_credential_authenticates_only_inside_its_scope() {
        let e = crate::testing::env().await;
        e.app.service_tokens.write().unwrap().insert(crate::service_auth::HERDR_UPGRADE.into(), "service-secret".into());
        let allowed = raw(
            e.app.clone(),
            "GET /api/capabilities HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Service-Id: herdr-upgrade\r\nX-AM-Service-Token: service-secret\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".into(),
        )
        .await;
        assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");

        let denied = state(e.app.clone(), &[("X-AM-Service-Id", "herdr-upgrade"), ("X-AM-Service-Token", "service-secret")]).await;
        assert!(denied.starts_with("HTTP/1.1 403"), "service must not read arbitrary state: {denied}");

        let fallback = state(
            e.app.clone(),
            &[("X-AM-Token", &e.app.ui_token), ("X-AM-Service-Id", "herdr-upgrade"), ("X-AM-Service-Token", "wrong")],
        )
        .await;
        assert!(fallback.starts_with("HTTP/1.1 401"), "invalid service credentials must not downgrade to User: {fallback}");
    }

    async fn service_post(app: Arc<App>, id: &str, token: &str, path: &str) -> String {
        raw(
            app,
            format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Service-Id: {id}\r\nX-AM-Service-Token: {token}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
        )
        .await
    }

    #[tokio::test]
    async fn daemon_swap_opens_its_own_restart_window_without_an_approval_from_anyone_else() {
        let e = crate::testing::env().await;
        let body = || SwapWindowIn { owner: "daemon-update-kick".into(), commit: "abc123".into(), ttl_secs: Some(600) };
        let svc = || Extension(RequestPrincipal::Service(crate::service_auth::DAEMON_SWAP.into()));
        // 只有 daemon-swap 服務身分；使用者與 bot 都不行。
        for other in [RequestPrincipal::User, RequestPrincipal::Bot("b1".into()), RequestPrincipal::Service(crate::service_auth::HERDR_UPGRADE.into())] {
            let r = service_daemon_swap_restart_window(State(e.app.clone()), Extension(other), Json(body())).await;
            assert!(matches!(r, Err(LcError::Forbidden(_))));
        }
        let Json(v) = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(body())).await.unwrap();
        assert_eq!(v["lease"]["held"], true, "{v}");
        assert!(v["lease_token"].as_str().is_some_and(|t| !t.is_empty()));
        assert_eq!(v["approval"]["decided_by"], "service(daemon-swap)");
        assert_eq!(v["approval"]["purpose"], "restart");
        // 窗口被握著時別人拿不到，而且不留下 approved 的殘單。
        let mut other = body();
        other.owner = "someone-else".into();
        let r = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(other)).await;
        assert!(matches!(r, Err(LcError::Conflict(_))), "{r:?}");
        let live = crate::supervisor::store::approvals(&e.app.db, 100).await.unwrap().into_iter().filter(|a| a.status == "approved").count();
        assert_eq!(live, 1, "只剩握著窗口的那一筆");
    }

    #[tokio::test]
    async fn daemon_swap_window_is_refused_while_a_bot_is_working() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "swap-busy").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
        let body = SwapWindowIn { owner: "daemon-update-kick".into(), commit: "abc123".into(), ttl_secs: None };
        let r = service_daemon_swap_restart_window(
            State(e.app.clone()),
            Extension(RequestPrincipal::Service(crate::service_auth::DAEMON_SWAP.into())),
            Json(body),
        )
        .await;
        assert!(matches!(&r, Err(LcError::Conflict(v)) if v["detail"]["reason"] == "not_idle" || v["reason"] == "not_idle"), "{r:?}");
    }

    /// 2026-10-01：一直有 bot 在忙時，每一輪都開新核准、拿不到就撤，升級計時每輪歸零，自動部署卡了一整晚。
    /// 現在同 commit 沿用同一張（`not_idle` 不撤），換 commit 開新的並接續舊的等待；等滿門檻 working 就不再擋。
    #[tokio::test]
    async fn daemon_swap_keeps_one_approval_across_rounds_so_a_busy_fleet_still_gets_a_window() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "swap-forever-busy").await;
        let run = crate::testing::fake_run(&e.app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
        let svc = || Extension(RequestPrincipal::Service(crate::service_auth::DAEMON_SWAP.into()));
        let ask = |commit: &str| SwapWindowIn { owner: "daemon-update-kick".into(), commit: commit.into(), ttl_secs: None };
        let approval_of = |r: &Result<Json<Value>, LcError>| match r {
            Err(LcError::Conflict(v)) => v["safety"]["escalation_approval_id"].as_str().or(v["detail"]["safety"]["escalation_approval_id"].as_str()).map(String::from),
            _ => None,
        };
        let r1 = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask("c1"))).await;
        let r2 = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask("c1"))).await;
        let (a1, a2) = (approval_of(&r1), approval_of(&r2));
        assert!(a1.is_some(), "{r1:?}");
        assert_eq!(a1, a2, "同 commit 下一輪沿用同一張核准");
        let a1 = a1.unwrap();
        assert_eq!(crate::supervisor::store::approval(&e.app.db, &a1).await.unwrap().unwrap().status, "approved", "not_idle 不撤");

        // 那張核准已經等了 31 分鐘（把核准時間往前推）；main 又動了，換 commit 也接得下去。
        let old = crate::db::iso_in(-31 * 60);
        sqlx::query("UPDATE supervisor_approvals SET decided_at=?, created_at=? WHERE id=?").bind(&old).bind(&old).bind(&a1).execute(&e.app.db).await.unwrap();
        let Json(v) = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask("c2"))).await.expect("等滿門檻之後 working 不再擋");
        assert_eq!(v["lease"]["held"], true, "{v}");
        assert_eq!(crate::supervisor::store::approval(&e.app.db, &a1).await.unwrap().unwrap().status, "superseded");
    }

    #[tokio::test]
    async fn daemon_swap_service_scope_accepts_only_its_fixed_probe_route() {
        let e = crate::testing::env().await;
        e.app.service_tokens.write().unwrap().insert(crate::service_auth::DAEMON_SWAP.into(), "swap-secret".into());
        let target = distinct_bot(&e, "swap-probe-target").await;
        // An unknown bot is 404 from the handler: auth and scope let the request through.
        let response = service_post(e.app.clone(), "daemon-swap", "swap-secret", "/api/services/daemon-swap/probe/01NOSUCHBOT").await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        // The same credential cannot use the generic prompt, bot control, or the other service's route.
        for path in [
            format!("/api/bots/{}/prompt", target.id),
            format!("/api/bots/{}/stop", target.id),
            format!("/api/services/herdr-upgrade/resume/{}", target.id),
            "/api/services/herdr-upgrade/notify".to_string(),
        ] {
            let denied = service_post(e.app.clone(), "daemon-swap", "swap-secret", &path).await;
            assert!(denied.starts_with("HTTP/1.1 403"), "{path}: {denied}");
        }
        // Another service's token under this id, or an unknown service id, is not a credential.
        e.app.service_tokens.write().unwrap().insert(crate::service_auth::HERDR_UPGRADE.into(), "herdr-secret".into());
        let swapped = service_post(e.app.clone(), "daemon-swap", "herdr-secret", "/api/services/daemon-swap/probe/01NOSUCHBOT").await;
        assert!(swapped.starts_with("HTTP/1.1 401"), "{swapped}");
        let unknown = service_post(e.app.clone(), "root", "swap-secret", "/api/services/daemon-swap/probe/01NOSUCHBOT").await;
        assert!(unknown.starts_with("HTTP/1.1 401"), "{unknown}");
    }

    #[tokio::test]
    async fn a_bot_principal_cannot_call_a_service_route() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "not-a-service").await;
        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/services/herdr-upgrade/resume/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                bot.id, bot.id, bot.hook_token
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403") && response.contains("service_only"), "{response}");
    }

    #[tokio::test]
    async fn a_child_credential_is_not_rotated_because_its_pane_uses_the_parents() {
        let e = crate::testing::env().await;
        let child = distinct_bot(&e, "rotate-child").await;
        sqlx::query("UPDATE bots SET managed_by='child' WHERE id=?").bind(&child.id).execute(&e.app.db).await.unwrap();
        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                child.id, e.app.ui_token
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 409") && response.contains("child_uses_parent_credential"), "{response}");
        let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&child.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(current, child.hook_token);
    }

    #[tokio::test]
    async fn a_bot_principal_cannot_rotate_a_credential() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "bot-cannot-rotate").await;
        let old = bot.hook_token.clone();
        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                bot.id, bot.id, bot.hook_token
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "only User may rotate bot credentials: {response}");
        let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(current, old);
    }

    #[tokio::test]
    async fn relay_from_cannot_override_the_authenticated_bot_identity() {
        let e = crate::testing::env().await;
        let sender = distinct_bot(&e, "authenticated-sender").await;
        let claimed = distinct_bot(&e, "forged-relay").await;
        let target = distinct_bot(&e, "relay-target").await;
        let body = serde_json::to_vec(&json!({"text": "must-not-send", "relay_from": claimed.id})).unwrap();
        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                target.id,
                sender.id,
                sender.hook_token,
                body.len(),
                String::from_utf8(body).unwrap(),
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE content='must-not-send'")
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(count, 0, "mismatched relay identity must be rejected before message creation");
    }

    /// Defense in depth: even if two bots ever shared a token, a Bot principal still may only
    /// claim its own id as `relay_from`. (`testing::claude_bot` gives both bots the same 'tok'.)
    #[tokio::test]
    async fn relay_from_is_compared_by_id_even_when_tokens_collide() {
        let e = crate::testing::env().await;
        let sender = crate::testing::claude_bot(&e.app, &e.project_id, "collide-sender").await;
        let claimed = crate::testing::claude_bot(&e.app, &e.project_id, "collide-claimed").await;
        let target = crate::testing::claude_bot(&e.app, &e.project_id, "collide-target").await;
        assert_eq!(sender.hook_token, claimed.hook_token, "precondition: the helper hands out one shared token");
        let body = serde_json::to_vec(&json!({"text": "collide-must-not-send", "relay_from": claimed.id})).unwrap();
        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                target.id,
                sender.id,
                sender.hook_token,
                body.len(),
                String::from_utf8(body).unwrap(),
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403") && response.contains("relay_from_mismatch"), "{response}");
    }

    /// #410 對抗式審查：HTTP 層（含認證中介層）上所有「沒有證明卻想讓 `relay_from` 記上別顆 bot」的送法，
    /// 一律被擋、什麼都不寫；擋掉的錯誤碼／內容不能洩漏某顆 bot 存不存在。
    #[tokio::test]
    async fn no_header_trick_gets_an_unproven_relay_from_stored() {
        let e = crate::testing::env().await;
        let sender = distinct_bot(&e, "adv-sender").await;
        let victim = distinct_bot(&e, "adv-victim").await;
        let target = distinct_bot(&e, "adv-target").await;
        let gone = distinct_bot(&e, "adv-gone").await;
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&gone.id).execute(&e.app.db).await.unwrap();
        let ui = e.app.ui_token.clone();
        let send = |marker: &str, relay: &str, headers: Vec<(String, String)>| {
            let app = e.app.clone();
            let (target, marker, relay) = (target.id.clone(), marker.to_string(), relay.to_string());
            async move {
                let body = serde_json::to_vec(&json!({"text": marker, "client_request_id": marker, "relay_from": relay})).unwrap();
                let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                let resp = raw(
                    app.clone(),
                    format!(
                        "POST /api/bots/{target}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        String::from_utf8(body).unwrap()
                    ),
                )
                .await;
                let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE content=?").bind(&marker).fetch_one(&app.db).await.unwrap();
                assert_eq!(stored, 0, "{marker}: nothing may be written\n{resp}");
                resp
            }
        };
        let h = |k: &str, v: &str| (k.to_string(), v.to_string());
        let status = |r: &str| r.lines().next().unwrap_or_default().to_string();

        // UI token（User）自稱 bot、沒有任何 bot 憑證。
        let r = send("adv-ui-only", &victim.id, vec![h("X-AM-Token", &ui)]).await;
        assert!(status(&r).contains("403") && r.contains("relay_from_token_required"), "{r}");
        // UI token 加上受害者的 token（混合憑證不得降級／升級）：401。
        for (name, headers) in [
            ("adv-ui-plus-token", vec![h("X-AM-Token", &ui), h("X-AM-Bot-Token", &victim.hook_token)]),
            ("adv-ui-plus-empty-token", vec![h("X-AM-Token", &ui), h("X-AM-Bot-Token", "")]),
            ("adv-token-only", vec![h("X-AM-Bot-Token", &victim.hook_token)]),
            ("adv-empty-token", vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", "")]),
            ("adv-victim-id-sender-token", vec![h("X-AM-Bot-Id", &victim.id), h("X-AM-Bot-Token", &sender.hook_token)]),
            ("adv-lowercased-id", vec![h("X-AM-Bot-Id", &sender.id.to_lowercase()), h("X-AM-Bot-Token", &sender.hook_token)]),
            ("adv-deleted-bot", vec![h("X-AM-Bot-Id", &gone.id), h("X-AM-Bot-Token", &gone.hook_token)]),
        ] {
            let r = send(name, &victim.id, headers).await;
            assert!(status(&r).contains("401"), "{name}: {r}");
        }
        // 自己的憑證、claim 別顆：不管大小寫的 header 名、前後空白、重複 header，都是 403 mismatch。
        let mut bodies = Vec::new();
        for (name, relay, headers) in [
            ("adv-plain", victim.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &sender.hook_token)]),
            ("adv-lower-names", victim.id.clone(), vec![h("x-am-bot-id", &sender.id), h("x-am-bot-token", &sender.hook_token)]),
            ("adv-padded", format!("  {}  ", victim.id), vec![h("X-AM-Bot-Id", &format!(" {} ", sender.id)), h("X-AM-Bot-Token", &format!(" {} ", sender.hook_token))]),
            ("adv-dup-id", victim.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Id", &victim.id), h("X-AM-Bot-Token", &sender.hook_token)]),
            ("adv-dup-token", victim.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &sender.hook_token), h("X-AM-Bot-Token", &victim.hook_token)]),
            ("adv-dup-token-rev", victim.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &victim.hook_token), h("X-AM-Bot-Token", &sender.hook_token)]),
            ("adv-deleted-claim", gone.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &sender.hook_token)]),
            ("adv-nosuch-claim", "01MNOSUCHBOTIDATALL0000000".to_string(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &sender.hook_token)]),
        ] {
            let r = send(name, &relay, headers).await;
            let ok_status = if name == "adv-dup-token-rev" { "401" } else { "403" };
            assert!(status(&r).contains(ok_status), "{name}: {r}");
            if ok_status == "403" {
                assert!(r.contains("relay_from_mismatch"), "{name}: {r}");
                bodies.push(r.split("\r\n\r\n").nth(1).unwrap_or_default().to_string());
            }
        }
        // 活的、已刪的、根本不存在的 claim，回的內容完全一樣（不能當 bot id 神諭）。
        assert!(bodies.windows(2).all(|w| w[0] == w[1]), "{bodies:?}");
        // 自己轉給自己：400，同樣不寫。
        let r = send(
            "adv-self",
            &target.id,
            vec![h("X-AM-Bot-Id", &target.id), h("X-AM-Bot-Token", &target.hook_token)],
        )
        .await;
        assert!(status(&r).contains("400") && r.contains("relay_self"), "{r}");
    }

    #[tokio::test]
    async fn rotating_a_bot_credential_invalidates_the_old_token_without_returning_the_new_one() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "rotate-auth").await;
        let old = bot.hook_token.clone();
        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                bot.id, e.app.ui_token
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains(&old), "response must never disclose either credential");
        let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
        assert_ne!(current, old, "old bot credential is invalidated immediately");
        let stale = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &old)]).await;
        assert!(stale.starts_with("HTTP/1.1 401"), "the old proof must already be unusable: {stale}");
    }

    #[tokio::test]
    async fn rotating_a_default_session_bot_is_refused_before_changing_or_stopping_it() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "rotate-default-session").await;
        sqlx::query("UPDATE bots SET herdr_session='default' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        let run_id = crate::testing::fake_run(&e.app, &bot.id).await;
        sqlx::query("UPDATE runs SET herdr_session='default' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();

        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                bot.id, e.app.ui_token
            ),
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 409") && response.contains("\"reason\":\"default_session\""), "{response}");
        let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(current, bot.hook_token, "default-session credentials are not rotated");
        assert_eq!(db::run(&e.app.db, &run_id).await.unwrap().unwrap().state, "running", "the imported pane is untouched");
        let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM intents WHERE kind='restart' AND subject_id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(intents, 0, "a default-session rotation cannot leave a restart intent");
        assert!(e.herdr.calls_to("agent.send_keys").is_empty(), "the user's pane must not receive ctrl+c");
    }

    #[tokio::test]
    async fn resuming_credential_rotation_refuses_default_session_before_sending_keys() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "resume-rotate-default-session").await;
        sqlx::query("UPDATE bots SET herdr_session='default' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        let run_id = crate::testing::fake_run(&e.app, &bot.id).await;
        sqlx::query("UPDATE runs SET herdr_session='default' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();

        let lock = e.app.bot_lock(&bot.id).await;
        let _guard = lock.lock().await;
        let result = lifecycle::resume_credential_rotation_locked(&e.app, &bot.id, lifecycle::StartOpts::default(), &run_id).await;

        assert!(matches!(result, Err(LcError::Conflict(ref body)) if body["reason"] == "default_session"));
        assert_eq!(db::run(&e.app.db, &run_id).await.unwrap().unwrap().state, "running", "recovery must leave the imported run active");
        assert!(e.herdr.calls_to("agent.send_keys").is_empty(), "the user's pane must not receive ctrl+c");
    }

    #[tokio::test]
    async fn an_unreadable_intents_table_does_not_commit_credential_rotation() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "rotate-read-failure").await;
        crate::testing::fake_run(&e.app, &bot.id).await;
        let old = bot.hook_token.clone();
        crate::testing::make_table_unreadable(&e.app, "intents").await;

        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                bot.id, e.app.ui_token
            ),
        )
        .await;

        assert!(!response.starts_with("HTTP/1.1 200"), "an unreadable intent table must fail before rotation: {response}");
        crate::testing::make_table_readable(&e.app, "intents").await;
        let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(current, old, "failed preflight must leave the old credential valid");
        assert!(!response.contains("\"credential_rotated\":true"), "a pre-commit failure must not claim the credential changed: {response}");
        let old_proof = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &old)]).await;
        assert!(old_proof.starts_with("HTTP/1.1 200"), "old credential must still authenticate: {old_proof}");
    }

    #[tokio::test]
    async fn a_postcommit_restart_failure_reports_rotation_and_keeps_recovery_intent() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "rotate-restart-failure").await;
        crate::lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
        sqlx::query("UPDATE bots SET identity='no-such-identity' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
        let old = bot.hook_token.clone();

        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                bot.id, e.app.ui_token
            ),
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 409"), "restart failure must be reported after rotation: {response}");
        assert!(response.contains("\"credential_rotated\":true"), "post-commit errors must say the old credential is invalid: {response}");
        assert!(response.contains("restart_pending"), "the caller needs to know recovery remains scheduled: {response}");
        assert!(!response.contains(&old), "the response must not disclose the old proof");
        let payload: String = sqlx::query_scalar("SELECT payload_json FROM intents WHERE kind='restart' AND subject_id=? ORDER BY created_at DESC LIMIT 1")
            .bind(&bot.id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert!(payload.contains("\"credential_rotation\":true"), "the restart intent must preserve its rotation-specific recovery rule: {payload}");
        let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
        assert_ne!(current, old, "the token update is durable even though the restart must be retried");
    }

    #[tokio::test]
    async fn rotating_a_live_bot_restarts_it_and_completes_its_durable_intent() {
        let e = crate::testing::env().await;
        let bot = distinct_bot(&e, "rotate-live").await;
        let old_run = crate::lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
        let old_token = bot.hook_token.clone();

        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                bot.id, e.app.ui_token
            ),
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 200"), "live credential rotation should restart successfully: {response}");
        assert!(response.contains("\"credential_rotated\":true") && response.contains("\"restarted\":true"), "success response must report rotation and restart: {response}");
        assert!(!response.contains(&old_token), "the response must not disclose the old credential");
        let new_token: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
        assert_ne!(new_token, old_token, "the bot token is rotated");
        let stale = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &old_token)]).await;
        assert!(stale.starts_with("HTTP/1.1 401"), "old proof is invalid after commit: {stale}");
        let fresh = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &new_token)]).await;
        assert!(fresh.starts_with("HTTP/1.1 200"), "new proof is available to the restarted run: {fresh}");
        assert_ne!(db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap().id, old_run, "the pane was restarted");
        let payload: String = sqlx::query_scalar("SELECT payload_json FROM intents WHERE kind='restart' AND subject_id=? ORDER BY created_at DESC LIMIT 1")
            .bind(&bot.id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert!(payload.contains("\"credential_rotation\":true"), "completed restart keeps its rotation semantics in the intent");
    }

    #[tokio::test]
    async fn rotation_refuses_a_live_grandchild_that_inherited_the_parent_credential() {
        let e = crate::testing::env().await;
        let parent = distinct_bot(&e, "rotate-parent-with-live-descendants").await;
        let child = distinct_bot(&e, "rotate-child-with-live-descendant").await;
        let grandchild = distinct_bot(&e, "rotate-grandchild").await;
        for (id, parent_id) in [(&child.id, &parent.id), (&grandchild.id, &child.id)] {
            sqlx::query("UPDATE bots SET managed_by='child', parent_bot_id=?, hook_token=? WHERE id=?")
                .bind(parent_id)
                .bind(&parent.hook_token)
                .bind(id)
                .execute(&e.app.db)
                .await
                .unwrap();
        }
        let parent_run = crate::testing::fake_run(&e.app, &parent.id).await;
        let child_run = crate::testing::fake_run(&e.app, &child.id).await;
        let grandchild_run = crate::testing::fake_run(&e.app, &grandchild.id).await;

        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                parent.id, e.app.ui_token
            ),
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 409"), "rotation must be refused while an inherited credential is live: {response}");
        assert!(response.contains("live_children_use_credential"), "the conflict must identify the credential dependency: {response}");
        assert!(response.contains(&child.id) && response.contains(&grandchild.id), "the live descendant set must include both levels: {response}");
        let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&parent.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(current, parent.hook_token, "a refused rotation must leave the old token valid");
        let old_proof = state(e.app.clone(), &[("X-AM-Bot-Id", &parent.id), ("X-AM-Bot-Token", &parent.hook_token)]).await;
        assert!(old_proof.starts_with("HTTP/1.1 200"), "the refusal must keep the inherited proof usable: {old_proof}");
        for (bot_id, run_id) in [(&parent.id, &parent_run), (&child.id, &child_run), (&grandchild.id, &grandchild_run)] {
            assert_eq!(crate::db::active_run(&e.app.db, bot_id).await.unwrap().unwrap().id, *run_id, "refusal must not restart or touch any dependent pane");
        }
    }

    #[tokio::test]
    async fn stopped_or_deleted_children_without_live_runs_do_not_block_parent_rotation() {
        let e = crate::testing::env().await;
        let parent = distinct_bot(&e, "rotate-parent-with-stopped-child").await;
        let stopped = distinct_bot(&e, "rotate-stopped-child").await;
        let deleted = distinct_bot(&e, "rotate-deleted-child").await;
        for child in [&stopped, &deleted] {
            sqlx::query("UPDATE bots SET managed_by='child', parent_bot_id=?, hook_token=? WHERE id=?")
                .bind(&parent.id)
                .bind(&parent.hook_token)
                .bind(&child.id)
                .execute(&e.app.db)
                .await
                .unwrap();
        }
        let stopped_run = crate::testing::fake_run(&e.app, &stopped.id).await;
        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
            .bind(crate::db::now())
            .bind(&stopped_run)
            .execute(&e.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(crate::db::now()).bind(&deleted.id).execute(&e.app.db).await.unwrap();

        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                parent.id, e.app.ui_token
            ),
        )
        .await;

        assert!(response.starts_with("HTTP/1.1 200"), "retired descendants do not hold the credential: {response}");
        let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&parent.id).fetch_one(&e.app.db).await.unwrap();
        assert_ne!(current, parent.hook_token, "the parent can rotate once all dependent panes are gone");
    }

    #[tokio::test]
    async fn a_child_spawn_permit_blocks_rotation_and_the_registered_pane_keeps_the_old_proof() {
        let e = crate::testing::env().await;
        let parent = distinct_bot(&e, "rotate-parent-with-spawn-permit").await;
        let old = parent.hook_token.clone();
        let begin = spawn_begin(e.app.clone(), &parent.id, &old).await;
        assert!(begin.starts_with("HTTP/1.1 200"), "the parent may reserve before opening a child pane: {begin}");
        let permit = response_json(&begin)["permit_id"].as_str().unwrap().to_string();

        let rotate = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                parent.id, e.app.ui_token
            ),
        )
        .await;
        assert!(rotate.starts_with("HTTP/1.1 409") && rotate.contains("child_spawn_in_progress"), "an in-flight external spawn must prevent rotation: {rotate}");
        assert_eq!(db::bot(&e.app.db, &parent.id).await.unwrap().unwrap().hook_token, old, "refusal keeps the old proof valid");

        let finish = spawn_finish(e.app.clone(), &parent.id, &old, &permit, "w1:p-unadopted-child").await;
        assert!(finish.starts_with("HTTP/1.1 200"), "successful pane creation must be registered before releasing its permit: {finish}");
        let rotate = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                parent.id, e.app.ui_token
            ),
        )
        .await;
        assert!(rotate.starts_with("HTTP/1.1 409") && rotate.contains("w1:p-unadopted-child"), "an unadopted sibling pane still carries the old credential: {rotate}");
        assert_eq!(db::bot(&e.app.db, &parent.id).await.unwrap().unwrap().hook_token, old, "the sibling pane keeps a valid proof until closed");
    }

    /// #664：shim 回報 herdr 沒開出 pane 時，abort 放開 permit，輪替不再 409。
    #[tokio::test]
    async fn aborting_a_spawn_permit_lets_rotation_proceed() {
        let e = crate::testing::env().await;
        let parent = distinct_bot(&e, "rotate-after-abort").await;
        let old = parent.hook_token.clone();
        let begin = spawn_begin(e.app.clone(), &parent.id, &old).await;
        assert!(begin.starts_with("HTTP/1.1 200"), "{begin}");
        let permit = response_json(&begin)["permit_id"].as_str().unwrap().to_string();
        let body = format!("bot_id={}&permit_id={permit}", parent.id);
        let abort = raw(
            e.app.clone(),
            format!(
                "POST /relay/spawn/abort HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Token: {old}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(abort.starts_with("HTTP/1.1 200") && abort.contains("\"released\":true"), "{abort}");
        let rotate = raw(
            e.app.clone(),
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                parent.id, e.app.ui_token
            ),
        )
        .await;
        assert!(rotate.starts_with("HTTP/1.1 200"), "abort 之後輪替不再被 child_spawn_in_progress 擋住：{rotate}");
    }

    #[tokio::test]
    async fn the_spawn_gate_refuses_a_child_created_after_the_descendant_snapshot() {
        let e = crate::testing::env().await;
        let parent = distinct_bot(&e, "rotate-parent-spawn-race").await;
        let old = parent.hook_token.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        let app = e.app.clone();
        let bot_id = parent.id.clone();
        let token = old.clone();
        crate::lifecycle::race_point::arm("credential_rotation_after_descendant_query", &parent.id, move || async move {
            // This is the shim's pre-herdr request. A 409 means it will not invoke herdr and no
            // inherited-credential pane can appear between the descendant snapshot and UPDATE.
            let response = spawn_begin(app, &bot_id, &token).await;
            let _ = send.send(response);
        });
        let app = e.app.clone();
        let bot_id = parent.id.clone();
        let ui_token = app.ui_token.clone();
        let rotation = tokio::spawn(async move {
            raw(
                app,
                format!(
                    "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                    bot_id, ui_token
                ),
            )
            .await
        });
        let spawn_response = receive.await.unwrap();
        let rotation_response = rotation.await.unwrap();

        assert!(spawn_response.starts_with("HTTP/1.1 409") && spawn_response.contains("credential_rotation_pending"), "shim must refuse the spawn while the fence is visible: {spawn_response}");
        assert!(rotation_response.starts_with("HTTP/1.1 200"), "with no dependent pane, rotation can commit after the fence check: {rotation_response}");
        let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&parent.id).fetch_one(&e.app.db).await.unwrap();
        assert_ne!(current, old, "only the fenced rotation may invalidate the old proof");
        let stale_spawn = spawn_begin(e.app.clone(), &parent.id, &old).await;
        assert!(stale_spawn.starts_with("HTTP/1.1 401"), "a delayed shim call cannot spawn with the revoked proof: {stale_spawn}");
    }
}

#[cfg(test)]
mod caller_audit_tests {
    //! review d77434c0 #3：`X-AM-Caller` 是自由文字，任何拿得到 UI token 的人都能寫 `caller=agm`。
    //! 身分只認 `X-AM-Bot-Id` ＋ 對得上的 `X-AM-Bot-Token`；自稱記在另一格，而且要看得出是自稱。
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 一顆進了 config.toml 的 user bot（刪除才找得到目標）。
    async fn a_user_bot(e: &crate::testing::Env, name: &str) -> db::Bot {
        let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
        let (pid, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
        let (bid, bname) = (bot.id.clone(), name.to_string());
        e.app
            .cfg
            .update(move |cfg| {
                let entry: crate::config::BotCfg =
                    toml::from_str(&format!("id = '{bid}'\nname = '{bname}'\nkind = 'claude'\n")).unwrap();
                match cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())) {
                    Some(p) => p.bots.push(entry),
                    None => cfg.projects.push(crate::config::ProjectCfg {
                        handed_off_to: None,
                        id: Some(pid),
                        path: repo,
                        label: "proj".into(),
                        host: LOCAL_HOST.into(),
                        bots: vec![entry],
                    }),
                }
                Ok(())
            })
            .await
            .unwrap();
        bot
    }

    /// 真的走一次 HTTP（含 `auth` 中介層），回 `(狀態行, intent 記下的呼叫端)`。
    /// `DELETE /api/bots/{id}` with exactly these headers (no UI token added); the raw response.
    async fn delete_raw(app: &Arc<App>, bot_id: &str, headers: &[(&str, &str)]) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = super::router(app.clone());
        let server =
            tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
        let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        let req = format!("DELETE /api/bots/{bot_id} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\nContent-Length: 0\r\n\r\n");
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        c.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        server.abort();
        out
    }

    /// Delete with these headers, then read who the delete intent says asked for it.
    async fn delete_as(app: &Arc<App>, bot_id: &str, headers: &[(&str, &str)]) -> (String, String) {
        let out = delete_raw(app, bot_id, headers).await;
        let payload: String = sqlx::query_scalar("SELECT payload_json FROM intents WHERE kind = 'delete_bot' AND subject_id = ?")
            .bind(bot_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&payload).unwrap();
        (out.lines().next().unwrap_or_default().to_string(), v["requested_by"].as_str().unwrap_or_default().to_string())
    }

    /// As the User (shared UI token) plus these extra headers.
    async fn delete_over_http(app: &Arc<App>, bot_id: &str, headers: &[(&str, &str)]) -> (String, String) {
        let mut all = vec![("X-AM-Token", app.ui_token.as_str())];
        all.extend_from_slice(headers);
        delete_as(app, bot_id, &all).await
    }

    /// 只有 hook token 對得上才填 `bot=`；`X-AM-Caller` 不管寫什麼都只進 `caller_self_reported=`。
    #[tokio::test]
    async fn a_self_reported_caller_is_never_recorded_as_a_verified_bot() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let liar = a_user_bot(&e, "alfa").await;
        let impostor = a_user_bot(&e, "bravo").await;
        let real = a_user_bot(&e, "charlie").await;

        // (1) 只會自稱：記成自稱，`bot=` 空著。
        let (status, by) = delete_over_http(&app, &liar.id, &[("X-AM-Caller", "agm"), ("User-Agent", "curl/8")]).await;
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        assert!(by.contains("caller_self_reported=agm"), "{by}");
        assert!(by.contains(" bot=-"), "沒有 hook token 就不能有身分：{by}");
        assert!(by.contains("peer=127.0.0.1:") && by.contains("ua=curl/8"), "{by}");

        // (2) 自稱是某顆 bot 但 token 對不上：#556 起中介層直接 401，不降級成 User，也就刪不掉、不留 intent。
        let refused = delete_raw(&app, &impostor.id, &[("X-AM-Bot-Id", &real.id), ("X-AM-Bot-Token", "not-the-token"), ("X-AM-Caller", "agm")]).await;
        assert!(refused.starts_with("HTTP/1.1 401"), "token 對不上不能算驗過：{refused}");
        assert!(db::bot(&app.db, &impostor.id).await.unwrap().unwrap().deleted_at.is_none(), "被拒的請求不能刪掉 bot");

        // (3) 帶對 token（Bot principal，不混帶 UI token）：用驗過的身分，自稱照樣只是自稱。
        let token: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id = ?").bind(&real.id).fetch_one(&app.db).await.unwrap();
        let (_, by) = delete_as(&app, &real.id, &[("X-AM-Bot-Id", &real.id), ("X-AM-Bot-Token", &token), ("X-AM-Caller", "agm")]).await;
        assert!(by.contains(&format!("bot={}(charlie)", real.id)), "{by}");
        assert!(by.contains("caller_self_reported=agm"), "{by}");
    }

    /// 軟刪的 bot 拿自己的 token 也不算驗過（token 外流之後還能冒名記帳）。
    #[tokio::test]
    async fn a_deleted_bots_token_no_longer_proves_identity() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let bot = crate::testing::claude_bot(&app, &e.project_id, "gone").await;
        let token: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id = ?").bind(&bot.id).fetch_one(&app.db).await.unwrap();
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Id", bot.id.parse().unwrap());
        h.insert("X-AM-Bot-Token", token.parse().unwrap());
        assert!(verified_caller_bot(&app, &h).await.is_some(), "前提：活著時驗得過");

        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&bot.id).execute(&app.db).await.unwrap();
        assert_eq!(verified_caller_bot(&app, &h).await, None);
    }
}

#[cfg(test)]
mod ct_eq_tests {
    use super::ct_eq;

    #[test]
    fn compares_whole_tokens_exactly() {
        assert!(ct_eq("0123abcd", "0123abcd"));
        assert!(!ct_eq("0123abcd", "0123abce"), "只差最後一位");
        assert!(!ct_eq("0123abcd", "1123abcd"), "只差第一位");
        assert!(!ct_eq("0123abcd", "0123abc"), "長度不同");
        assert!(!ct_eq("", "x"));
        assert!(ct_eq("", ""), "空對空只是函式本身的性質；呼叫端仍要先擋空 token");
    }

    #[tokio::test]
    async fn post_order_with_primary_writes_positions_and_state_reports_them() {
        use super::*;
        let e = crate::testing::env().await;
        let a = crate::testing::claude_bot(&e.app, &e.project_id, "a").await.id;
        let b = crate::testing::claude_bot(&e.app, &e.project_id, "b").await.id;
        let order = |ids: &[&str]| SetOrder { projects: None, bots: None, primary: Some(ids.iter().map(|s| s.to_string()).collect()) };
        let resp = set_order(State(e.app.clone()), Json(order(&[&b, &a]))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let state = state_json(&e.app).await.unwrap();
        let bots = state["projects"][0]["bots"].as_array().unwrap().clone();
        let pos = |id: &str| bots.iter().find(|x| x["id"] == id).unwrap()["primary_position"].as_i64().unwrap();
        assert_eq!((pos(&b), pos(&a)), (1, 2));
        // 未知 bot → 400，而且什麼都沒寫。
        let err = set_order(State(e.app.clone()), Json(order(&[&a, "ghost"]))).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)));
        let state = state_json(&e.app).await.unwrap();
        let bots = state["projects"][0]["bots"].as_array().unwrap().clone();
        assert_eq!(bots.iter().find(|x| x["id"] == a.as_str()).unwrap()["primary_position"], 2);
        // 三個欄位都沒有仍是 400。
        let none = SetOrder { projects: None, bots: None, primary: None };
        assert!(matches!(set_order(State(e.app.clone()), Json(none)).await.unwrap_err(), LcError::Bad(_)));
    }

    #[tokio::test]
    async fn post_order_rolls_back_if_a_named_bot_disappears_during_the_write() {
        use super::*;
        let e = crate::testing::env().await;
        let a = crate::testing::claude_bot(&e.app, &e.project_id, "a").await.id;
        let b = crate::testing::claude_bot(&e.app, &e.project_id, "b").await.id;
        // 模擬 validate 之後、第二筆 UPDATE 之前另一個請求刪掉 b。
        let trigger = format!(
            "CREATE TRIGGER delete_order_bot_after_first_update AFTER UPDATE OF primary_position ON bots WHEN NEW.id = '{a}' BEGIN UPDATE bots SET deleted_at = 'triggered' WHERE id = '{b}'; END"
        );
        sqlx::query(&trigger).execute(&e.app.db).await.unwrap();
        let body = SetOrder { projects: None, bots: None, primary: Some(vec![a.clone(), b.clone()]) };

        let err = set_order(State(e.app.clone()), Json(body)).await;
        assert!(err.is_err(), "bot 在寫入途中消失時不能回成功");
        let a_row: (i64, Option<String>) = sqlx::query_as("SELECT primary_position, deleted_at FROM bots WHERE id = ?")
            .bind(&a)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        let b_row: (i64, Option<String>) = sqlx::query_as("SELECT primary_position, deleted_at FROM bots WHERE id = ?")
            .bind(&b)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(a_row, (0, None), "前一筆位置也要跟著回滾");
        assert_eq!(b_row, (0, None), "被刪掉的 bot 變更也要跟著回滾");
    }
}

/// #350：一個請求同時要寫 config.toml 與只存 DB 的欄位（`primary`／`primary_position`）時，兩個 store 不可能同一個交易
/// （`update_and_project` 自己就會寫 DB，外層交易會死鎖；事後回滾又是第二次寫入、一樣會失敗）。所以混送一律 400、什麼都不寫，
/// 呼叫端分成兩次；不會有「回失敗、其中一半卻生效了」。
#[cfg(test)]
mod mixed_store_tests {
    use super::*;
    use crate::testing::{env, Env};

    async fn seed_project(e: &Env) {
        let (pid, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
        e.app
            .cfg
            .update(move |cfg| {
                cfg.projects.push(crate::config::ProjectCfg { id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![], handed_off_to: None });
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn add(e: &Env, name: &str) -> String {
        let res = create_bot(State(e.app.clone()), Path(e.project_id.clone()), Json(serde_json::from_value(json!({"name": name, "kind": "claude"})).unwrap())).await.unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string()
    }

    async fn patch(e: &Env, id: &str, body: Value) -> Result<Response, LcError> {
        patch_bot(State(e.app.clone()), Path(id.to_string()), Json(serde_json::from_value(body).unwrap())).await
    }

    async fn pin_state(e: &Env, id: &str) -> (i64, i64) {
        sqlx::query_as("SELECT is_primary, primary_position FROM bots WHERE id = ?").bind(id).fetch_one(&e.app.db).await.unwrap()
    }

    async fn model_of(e: &Env, id: &str) -> Option<String> {
        e.app.cfg.get().await.projects[0].bots.iter().find(|b| b.id.as_deref() == Some(id)).unwrap().model.clone()
    }

    #[tokio::test]
    async fn a_patch_mixing_primary_with_a_config_field_is_rejected_and_changes_nothing() {
        let e = env().await;
        seed_project(&e).await;
        let id = add(&e, "mix").await;
        let before = model_of(&e, &id).await;
        let err = patch(&e, &id, json!({"primary": true, "model": "opus"})).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "混送要 400：{err:?}");
        assert_eq!(pin_state(&e, &id).await, (0, 0), "primary 不能單獨生效");
        assert_eq!(model_of(&e, &id).await, before, "config 欄位也沒動");
        // 分開送各自照常成功。
        patch(&e, &id, json!({"primary": true})).await.unwrap();
        assert_eq!(pin_state(&e, &id).await.0, 1);
        patch(&e, &id, json!({"model": "opus"})).await.unwrap();
        assert_eq!(model_of(&e, &id).await.as_deref(), Some("claude-opus-5-5"));
    }

    #[tokio::test]
    async fn an_order_request_mixing_primary_with_config_orders_is_rejected_and_changes_nothing() {
        let e = env().await;
        seed_project(&e).await;
        let a = add(&e, "a").await;
        let b = add(&e, "b").await;
        let order_of = |e: &Env| {
            let app = e.app.clone();
            async move { app.cfg.get().await.projects[0].bots.iter().map(|x| x.id.clone().unwrap()).collect::<Vec<_>>() }
        };
        let before = order_of(&e).await;
        let mut bots = BTreeMap::new();
        bots.insert(e.project_id.clone(), vec![b.clone(), a.clone()]);
        let mixed = SetOrder { projects: None, bots: Some(bots), primary: Some(vec![b.clone(), a.clone()]) };
        let err = set_order(State(e.app.clone()), Json(mixed)).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "混送要 400：{err:?}");
        assert_eq!(order_of(&e).await, before, "config 順序沒動");
        assert_eq!(pin_state(&e, &a).await.1, 0, "primary_position 也沒寫");
    }
}

/// #352：`POST /api/projects/:id/bots` 的冪等。回應遺失後原樣重送（快速新增每次帶 `name_auto:true`）不能再建第二顆 bot；
/// 記在 config.toml 的那顆 bot 上（daemon 持久，瀏覽器重整、daemon 重啟都在）。
#[cfg(test)]
mod create_bot_idempotency_tests {
    use super::*;
    use crate::testing::{env, Env};

    async fn seed_project(e: &Env) {
        let (pid, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
        e.app
            .cfg
            .update(move |cfg| {
                cfg.projects.push(crate::config::ProjectCfg { id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![], handed_off_to: None });
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn create(e: &Env, body: Value) -> Result<Value, LcError> {
        let res = create_bot(State(e.app.clone()), Path(e.project_id.clone()), Json(serde_json::from_value(body).unwrap())).await?;
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    async fn bots_in_config(e: &Env) -> usize {
        e.app.cfg.get().await.projects[0].bots.len()
    }

    /// #654：child 占著 `review` 時，再建同名要 409，config 不能先寫進去。
    #[tokio::test]
    async fn creating_a_bot_with_a_childs_name_conflicts_and_the_next_create_still_works() {
        let e = env().await;
        seed_project(&e).await;
        create(&e, json!({"name": "alfa", "kind": "claude"})).await.unwrap();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES ('child-review',?,'review','claude','[]',0,1,'tok','child',?)",
        )
        .bind(&e.project_id)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        let err = create(&e, json!({"name": "review", "kind": "claude"})).await.unwrap_err();
        match err {
            LcError::Conflict(v) => assert_eq!(v["reason"], "bot name already in use"),
            other => panic!("要 409，不是投影 502：{other:?}"),
        }
        let names: Vec<String> = e.app.cfg.get().await.projects[0].bots.iter().map(|b| b.name.clone()).collect();
        assert_eq!(names, vec!["alfa".to_string()]);
        let bravo = create(&e, json!({"name": "bravo", "kind": "claude"})).await.unwrap();
        assert!(db::bot(&e.app.db, bravo["bot_id"].as_str().unwrap()).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_resent_create_with_the_same_request_id_returns_the_original_bot() {
        let e = env().await;
        seed_project(&e).await;
        let body = |name: &str| json!({"name": name, "name_auto": true, "kind": "claude", "client_request_id": "req-1"});
        let first = create(&e, body("cc1-1")).await.unwrap();
        // 回應遺失後重送：瀏覽器清單已同步，`nextName` 算出來的名字變成 cc1-2——名字只是 name_auto 的提示，不算請求內容。
        let again = create(&e, body("cc1-2")).await.unwrap();
        assert_eq!((again["bot_id"].clone(), again["name"].clone()), (first["bot_id"].clone(), first["name"].clone()), "重送拿回原本那顆：{again}");
        assert_eq!(bots_in_config(&e).await, 1, "只有一顆 bot");
        // 持久在 config.toml（瀏覽器重整、daemon 重啟都在）。
        let text = std::fs::read_to_string(&e.app.cfg.path).unwrap();
        assert!(text.contains("req-1"), "request id 要寫進 config.toml：{text}");
    }

    #[tokio::test]
    async fn the_same_request_id_with_a_different_request_is_a_409() {
        let e = env().await;
        seed_project(&e).await;
        create(&e, json!({"name": "a", "kind": "claude", "client_request_id": "req-2"})).await.unwrap();
        let err = create(&e, json!({"name": "b", "kind": "codex", "client_request_id": "req-2"})).await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)), "同 id 換 kind：{err:?}");
        assert_eq!(bots_in_config(&e).await, 1);
    }

    #[tokio::test]
    async fn a_new_request_id_is_a_deliberate_second_create() {
        let e = env().await;
        seed_project(&e).await;
        let a = create(&e, json!({"name": "cc1-1", "name_auto": true, "kind": "claude", "client_request_id": "req-3"})).await.unwrap();
        let b = create(&e, json!({"name": "cc1-1", "name_auto": true, "kind": "claude", "client_request_id": "req-4"})).await.unwrap();
        assert_ne!(a["bot_id"], b["bot_id"]);
        assert_eq!(b["name"], "cc1-2", "新的請求照常用 name_auto 往後找");
        assert_eq!(bots_in_config(&e).await, 2);
    }

    #[tokio::test]
    async fn a_create_without_a_request_id_behaves_as_before() {
        let e = env().await;
        seed_project(&e).await;
        create(&e, json!({"name": "x", "name_auto": true, "kind": "claude"})).await.unwrap();
        create(&e, json!({"name": "x", "name_auto": true, "kind": "claude"})).await.unwrap();
        assert_eq!(bots_in_config(&e).await, 2);
    }
}

#[cfg(test)]
mod child_restore_tests {
    //! SPEC §6.5a：被軟刪的子 agent 走既有的 `POST /bots/{id}/restore`（§10.4a）還原，不直接改 DB；
    //! 子 agent 的 start／restart 一律 409 `child_restart_forbidden`。
    use super::*;
    use crate::lifecycle::restart_kind_tests::live_child;
    use crate::testing as tt;

    async fn soft_delete(app: &Arc<App>, id: &str) {
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE bot_id=?").bind(db::now()).bind(id).execute(&app.db).await.unwrap();
    }

    fn conflict_reason(err: LcError) -> String {
        match err {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or("").to_string(),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_soft_deleted_child_is_restored_without_a_run() {
        let e = tt::env().await;
        let kid = live_child(&e, "pvw").await;
        soft_delete(&e.app, &kid.id).await;
        let resp = restore_bot(State(e.app.clone()), Path(kid.id.clone())).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let b = db::bot(&e.app.db, &kid.id).await.unwrap().unwrap();
        assert!(b.deleted_at.is_none() && b.managed_by == "child" && b.parent_bot_id.is_some());
        assert!(db::active_run(&e.app.db, &kid.id).await.unwrap().is_none(), "restore 不開 run");
        // Herdr has not reopened the child yet; a reconcile in this window must preserve its row/history.
        *e.herdr.agents.lock().unwrap() = Vec::new();
        crate::reconcile::reconcile_host(&e.app, crate::config::LOCAL_HOST).await.unwrap();
        assert!(db::bot(&e.app.db, &kid.id).await.unwrap().unwrap().deleted_at.is_none(), "restore grace keeps the original child row");
    }

    #[tokio::test]
    async fn an_unreopened_restored_child_is_retired_after_its_grace_expires() {
        let e = tt::env().await;
        let kid = live_child(&e, "pvw-expired").await;
        soft_delete(&e.app, &kid.id).await;
        restore_bot(State(e.app.clone()), Path(kid.id.clone())).await.unwrap();
        *e.herdr.agents.lock().unwrap() = Vec::new();
        sqlx::query("UPDATE supervisor_notes SET body='2000-01-01T00:00:00.000Z' WHERE supervisor_id=? AND kind='child_retirement_grace'")
            .bind(&kid.id)
            .execute(&e.app.db)
            .await
            .unwrap();

        crate::reconcile::reconcile_host(&e.app, crate::config::LOCAL_HOST).await.unwrap();

        assert!(db::bot(&e.app.db, &kid.id).await.unwrap().unwrap().deleted_at.is_some(), "expired grace restores the existing retirement rule");
    }

    #[tokio::test]
    async fn restore_refuses_a_live_child_and_a_taken_name() {
        let e = tt::env().await;
        let kid = live_child(&e, "dup").await;
        assert!(conflict_reason(restore_bot(State(e.app.clone()), Path(kid.id.clone())).await.unwrap_err()).contains("not deleted"));
        soft_delete(&e.app, &kid.id).await;
        // 同名的另一顆活 child（例如父 bot 已經用 herdr 重開了一顆）。
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
                     SELECT ?, project_id, name, kind, '[]', 0, 0, 'tok2', 'child', parent_bot_id, ? FROM bots WHERE id=?")
            .bind(db::ulid()).bind(db::now()).bind(&kid.id).execute(&e.app.db).await.unwrap();
        assert!(conflict_reason(restore_bot(State(e.app.clone()), Path(kid.id.clone())).await.unwrap_err()).contains("already in use"));
        assert!(matches!(restore_bot(State(e.app.clone()), Path("nope".into())).await.unwrap_err(), LcError::NotFound(_)));
    }

    #[tokio::test]
    async fn start_and_restart_refuse_children() {
        let e = tt::env().await;
        let kid = live_child(&e, "kid").await;
        let q = || Query(StartQuery { resume: None, session: None });
        assert_eq!(conflict_reason(restart_bot(State(e.app.clone()), Path(kid.id.clone()), q()).await.unwrap_err()), "child_restart_forbidden");
        assert_eq!(conflict_reason(start_bot(State(e.app.clone()), Path(kid.id.clone()), q()).await.unwrap_err()), "child_restart_forbidden");
        assert!(db::active_run(&e.app.db, &kid.id).await.unwrap().is_some(), "子 agent 的 run 一根毛都不能動");
    }
}

/// #339：`relay_from` 要跟呼叫者自己的身分綁在一起（`relay_auth`）。走真的 `prompt_bot` handler。
#[cfg(test)]
mod relay_from_auth_tests {
    use super::*;

    struct Fx {
        e: crate::testing::Env,
        alfa: String,
        bravo: String,
        target: String,
    }

    /// 寄件的 `alfa`（token `tok-alfa`）、收件的 `target`（grok、閒著、pane 活著）、旁觀的 `bravo`（`tok-bravo`）。
    async fn fx() -> Fx {
        let e = crate::testing::env().await;
        let mut ids = vec![];
        for name in ["alfa", "bravo", "target"] {
            let b = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
            sqlx::query("UPDATE bots SET hook_token=? WHERE id=?").bind(format!("tok-{name}")).bind(&b.id).execute(&e.app.db).await.unwrap();
            ids.push(b.id);
        }
        let target = ids[2].clone();
        sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&target).execute(&e.app.db).await.unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-relay','target','test',1,?)",
        )
        .bind(db::ulid())
        .bind(&target)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
        e.herdr.live_pane("pane-relay", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        Fx { e, alfa: ids[0].clone(), bravo: ids[1].clone(), target }
    }

    /// `send` 只做得出「有帶一個合法字串」與「完全不帶」。空字串與非 UTF-8 的 header 值要另外拼。
    async fn send_raw_token(app: &Arc<App>, to: &str, relay_from: &str, raw: &[u8], crid: &str) -> (StatusCode, Value) {
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Token", axum::http::HeaderValue::from_bytes(raw).unwrap());
        let body = PromptIn {
            text: "幫我看一下".into(),
            client_request_id: Some(crid.into()),
            attachments: vec![],
            relay_from: Some(relay_from.into()),
            ack: false,
            reply_to: None,
            send_now: false,
            start_if_stopped: false,
            queue_if_busy: false,
            clear_draft: false,
            submit_draft: false,
            expect_draft_token: None,
        };
        let resp = match prompt_bot(State(app.clone()), Path(to.to_string()), h, Json(body)).await {
            Ok(r) => r,
            Err(err) => err.into_response(),
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn send(app: &Arc<App>, to: &str, relay_from: &str, token: Option<&str>, crid: &str) -> (StatusCode, Value) {
        let mut h = HeaderMap::new();
        if let Some(t) = token {
            h.insert("X-AM-Bot-Token", t.parse().unwrap());
        }
        let body = PromptIn {
            text: "幫我看一下".into(),
            client_request_id: Some(crid.into()),
            attachments: vec![],
            relay_from: Some(relay_from.into()),
            ack: false,
            reply_to: None,
            send_now: false,
            start_if_stopped: false,
            queue_if_busy: false,
            clear_draft: false,
            submit_draft: false,
            expect_draft_token: None,
        };
        let resp = match prompt_bot(State(app.clone()), Path(to.to_string()), h, Json(body)).await {
            Ok(r) => r,
            Err(err) => err.into_response(),
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn counts(app: &Arc<App>) -> (i64, i64) {
        let t = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&app.db).await.unwrap();
        let m = sqlx::query_scalar("SELECT COUNT(*) FROM messages").fetch_one(&app.db).await.unwrap();
        (t, m)
    }

    async fn stored(app: &Arc<App>, message_id: &str) -> (Option<String>, i64) {
        sqlx::query_as("SELECT relay_from, relay_unverified FROM messages WHERE id=?").bind(message_id).fetch_one(&app.db).await.unwrap()
    }

    /// issue 的紅測試：拿自己（bravo）的 token 冒 alfa 的名 → 403，一個字都沒寫。
    #[tokio::test]
    async fn another_bots_token_cannot_speak_as_alfa() {
        let f = fx().await;
        let (status, body) = send(&f.e.app, &f.target, &f.alfa, Some("tok-bravo"), "mismatch").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["reason"], "relay_from_mismatch");
        assert_eq!(counts(&f.e.app).await, (0, 0), "被拒的冒名不留 turn、不留訊息");
    }

    /// `daemon` 是 daemon 自己的哨符：HTTP 帶進來一律 403（帶什麼 token 都一樣）。
    #[tokio::test]
    async fn nobody_can_claim_to_be_the_daemon_over_http() {
        let f = fx().await;
        for (token, crid) in [(None, "d-none"), (Some("tok-alfa"), "d-alfa")] {
            let (status, body) = send(&f.e.app, &f.target, crate::agent_relay::DAEMON_SENDER, token, crid).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{token:?}: {body}");
            assert_eq!(body["reason"], "relay_from_reserved");
        }
        assert_eq!(counts(&f.e.app).await, (0, 0));
    }

    /// 帶自己的 token：照送，記成已驗證。
    #[tokio::test]
    async fn a_bot_with_its_own_token_is_a_verified_relay() {
        let f = fx().await;
        let (status, body) = send(&f.e.app, &f.target, &f.alfa, Some("tok-alfa"), "verified").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(stored(&f.e.app, body["message_id"].as_str().unwrap()).await, (Some(f.alfa.clone()), 0));
    }

    /// #410 相容期結束：沒帶 token 的 `relay_from` 一律 403 `relay_from_token_required`，
    /// 什麼都不寫（沒有 turn、沒有訊息、沒有 `message_added`）。
    #[tokio::test]
    async fn an_unsigned_relay_is_refused_and_writes_nothing() {
        let f = fx().await;
        let mut rx = f.e.app.subscribe();
        let (status, body) = send(&f.e.app, &f.target, &f.alfa, None, "unsigned").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["reason"], "relay_from_token_required", "{body}");
        assert_eq!(counts(&f.e.app).await, (0, 0));
        while let Ok(ev) = rx.try_recv() {
            assert_ne!(ev.kind, "message_added", "被拒絕的 relay 不能推訊息");
        }
    }

    /// `start_if_stopped`（另一個寫訊息的地方）一樣拒絕，不留下排隊中的訊息。
    #[tokio::test]
    async fn the_start_if_stopped_path_refuses_an_unsigned_relay_too() {
        let f = fx().await;
        let stopped = crate::testing::claude_bot(&f.e.app, &f.e.project_id, "sleeper").await;
        let body = PromptIn {
            text: "起來後看一下".into(),
            client_request_id: Some("sis".into()),
            attachments: vec![],
            relay_from: Some(f.alfa.clone()),
            ack: false,
            reply_to: None,
            send_now: false,
            start_if_stopped: true,
            queue_if_busy: false,
            clear_draft: false,
            submit_draft: false,
            expect_draft_token: None,
        };
        let err = prompt_bot(State(f.e.app.clone()), Path(stopped.id.clone()), HeaderMap::new(), Json(body)).await.expect_err("unsigned relay");
        assert!(matches!(err, LcError::Forbidden(_)), "{err:?}");
        assert_eq!(counts(&f.e.app).await, (0, 0));
    }

    /// 框裡的草稿（409 `composer_busy`）只有使用者自己處理：`clear_draft`／`submit_draft` 沒帶 `expect_draft_token` 是 400，
    /// bot 轉送的也是 400——都在碰 pane 之前擋下，一個鍵都不按。
    #[tokio::test]
    async fn draft_actions_need_the_confirmed_draft_and_the_user_themself() {
        let f = fx().await;
        f.e.herdr.live_pane("pane-relay", crate::testing::LivePane { width: Some(120), boxed: true, composer: vec!["假草稿".into()], ..Default::default() });
        let call = |clear: bool, submit: bool, expect: Option<&str>, relay: Option<String>| {
            let body = PromptIn {
                text: if submit { String::new() } else { "我的".into() },
                client_request_id: Some(db::ulid()),
                attachments: vec![],
                relay_from: relay,
                ack: false,
                reply_to: None,
                send_now: false,
                start_if_stopped: false,
                queue_if_busy: false,
                clear_draft: clear,
                submit_draft: submit,
                expect_draft_token: expect.map(str::to_string),
            };
            // 轉送要帶那顆 bot 自己的 token（#410 後沒帶就是 403），這樣測的才是「草稿動作不給轉送」那一條。
            let mut headers = HeaderMap::new();
            if body.relay_from.is_some() {
                headers.insert("X-AM-Bot-Token", "tok-alfa".parse().unwrap());
            }
            prompt_bot(State(f.e.app.clone()), Path(f.target.clone()), headers, Json(body))
        };
        let busy = call(false, false, None, None).await.expect_err("composer_busy");
        let token = match busy {
            LcError::Conflict(body) => body["draft_token"].as_str().expect("full draft token").to_string(),
            other => panic!("expected composer_busy, got {other:?}"),
        };
        for (clear, submit) in [(true, false), (false, true)] {
            let err = call(clear, submit, None, None).await.expect_err("no expect_draft_token");
            assert!(matches!(err, LcError::Bad(_)), "{err:?}");
            let err = call(clear, submit, Some(&token), Some(f.alfa.clone())).await.expect_err("relayed");
            assert!(matches!(err, LcError::Bad(_)), "{err:?}");
        }
        assert!(f.e.herdr.calls_to("pane.send_keys").is_empty());
        // 帶齊了就真的送出框裡那段（grok：沒有無損證據，unverified）。
        let resp = call(false, true, Some(&token), None).await.unwrap();
        let out: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(out["delivery"], "unverified", "{out}");
    }

    /// 寫給 AGM（協調者存在）的申請：對不上的 token 一樣 403、不進收件匣；沒帶也 403（#410 相容期已結束）。
    #[tokio::test]
    async fn requests_to_agm_follow_the_same_rule() {
        use crate::supervisor::bot_requests::flow_tests::{app, configure_responder};
        let app = app().await;
        configure_responder(&app).await;
        let inbox = || async { sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM supervisor_inbox").fetch_one(&app.db).await.unwrap() };
        let (status, body) = send(&app, "patrol", "w1", Some("tok-w2"), "agm-mismatch").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(inbox().await, 0);
        let (status, body) = send(&app, "patrol", "w1", Some("tok-w1"), "agm-signed").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let (status, body) = send(&app, "patrol", "w1", None, "agm-unsigned").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let verified: Vec<bool> = sqlx::query_scalar("SELECT json_extract(payload_json,'$.sender_verified') FROM supervisor_inbox ORDER BY created_at, id")
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(verified, vec![true]);
    }

    // --------------------------------------------- review 7ed32d94：同一張 #339 的收尾

    /// 「有帶 `X-AM-Bot-Token` 但值是空的／非 UTF-8」以前掉進「沒帶」那一格，於是送一個壞掉的
    /// header 就能走相容期冒名放行。有帶就得對得上：一律 403，一個字都不寫。
    #[tokio::test]
    async fn a_present_but_unusable_token_is_a_mismatch_not_a_missing_one() {
        let f = fx().await;
        // 空字串、只有空白、非 UTF-8（0xff 不是合法 UTF-8，但是合法的 header 位元組）。
        for (raw, label) in [(&b""[..], "empty"), (&b"   "[..], "blank"), (&b"\xff\xfe"[..], "not-utf8")] {
            let (status, body) = send_raw_token(&f.e.app, &f.target, &f.alfa, raw, label).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {body}");
            assert_eq!(body["reason"], "relay_from_mismatch", "{label}: {body}");
        }
        assert_eq!(counts(&f.e.app).await, (0, 0), "三種壞 token 都不能留下 turn 或訊息");
    }

    /// 帶錯 token 的呼叫端不該從狀態碼讀出「這個 bot id 存不存在」：不存在／已刪的 relay_from
    /// 以前先查 bot 回 400、存在的才 403，一個一個試就掃得出 id。有帶 token 就一律先 403。
    #[tokio::test]
    async fn a_wrong_token_never_reveals_whether_the_bot_exists() {
        let f = fx().await;
        let gone = crate::testing::claude_bot(&f.e.app, &f.e.project_id, "gone").await;
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&gone.id).execute(&f.e.app.db).await.unwrap();

        let mut seen = vec![];
        for (from, crid) in [(f.alfa.as_str(), "probe-live"), ("01MNOSUCHBOTIDATALL0000000", "probe-absent"), (gone.id.as_str(), "probe-deleted")] {
            let (status, body) = send(&f.e.app, &f.target, from, Some("tok-bravo"), crid).await;
            seen.push((status, body["reason"].as_str().unwrap_or_default().to_string()));
        }
        assert_eq!(
            seen,
            vec![(StatusCode::FORBIDDEN, "relay_from_mismatch".to_string()); 3],
            "活著的、不存在的、已刪的：帶錯 token 一律同一個回應"
        );
        assert_eq!(counts(&f.e.app).await, (0, 0));

        // 沒帶 token、bot 不存在照舊是 400（那條路沒有 token 可以對，也沒有神諭可讀）。
        let (status, _) = send(&f.e.app, &f.target, "01MNOSUCHBOTIDATALL0000000", None, "absent-nosig").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// 票上要的 sender ≠ target：`relay_from` 指向收件的那顆 bot 自己，UI 會畫出「A → A」，
    /// 而「這句話不是你自己想的」對自己沒有意義。400 `relay_self`，什麼都不寫。
    #[tokio::test]
    async fn a_bot_cannot_relay_a_message_to_itself() {
        let f = fx().await;
        // 連帶對自己那顆的正確 token 也不行：驗得出身分不代表這個來源標示講得通。
        sqlx::query("UPDATE bots SET hook_token='tok-target' WHERE id=?").bind(&f.target).execute(&f.e.app.db).await.unwrap();
        for (token, crid) in [(None, "self-none"), (Some("tok-target"), "self-signed")] {
            let (status, body) = send(&f.e.app, &f.target, &f.target, token, crid).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{token:?}: {body}");
            assert_eq!(body["reason"], "relay_self", "{token:?}: {body}");
        }
        assert_eq!(counts(&f.e.app).await, (0, 0), "自己送給自己不留 turn、不留訊息");

        // 送給**別顆** bot 照舊通：擋的是 from == to，不是「有 relay_from」。
        let (status, body) = send(&f.e.app, &f.target, &f.bravo, Some("tok-bravo"), "self-ok").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

/// DNS rebinding：攻擊者的網域先指到他的伺服器載入頁面、再改指 127.0.0.1，之後頁面對自己 origin 的 GET
/// **不帶 `Origin`**（同源 GET 瀏覽器不送），TCP 對端又是 loopback——只看 Origin 與對端擋不住，`GET /api/session` 就把
/// UI token 交出去。瀏覽器送的 `Host` 是攻擊者的網域，所以 `allow_lan` 關著時 Host 也必須是 loopback 名稱。
#[cfg(test)]
mod host_header_tests {
    use super::origin_is_local;
    use axum::http::{HeaderMap, HeaderValue};

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn a_rebound_hostname_is_refused_even_without_an_origin_header() {
        assert!(!origin_is_local(&h(&[("host", "evil.example:7788")]), 7788, false), "同源 GET 沒有 Origin，只看 Host 才擋得住");
        assert!(!origin_is_local(&h(&[("host", "127.0.0.1.evil.example:7788")]), 7788, false));
        assert!(!origin_is_local(&h(&[("host", "localhost.evil.example")]), 7788, false));
        assert!(!origin_is_local(&h(&[("host", "evil.example"), ("origin", "http://127.0.0.1:7788")]), 7788, false));
    }

    #[test]
    fn loopback_hosts_and_hostless_clients_still_pass() {
        for host in ["127.0.0.1:7788", "127.0.0.1", "localhost:5173", "localhost", "[::1]:7788", "[::1]", "LOCALHOST:7788"] {
            assert!(origin_is_local(&h(&[("host", host)]), 7788, false), "{host}");
        }
        // 沒有 Host 的不是瀏覽器（HTTP/1.0、內部呼叫）：照舊放行。
        assert!(origin_is_local(&h(&[]), 7788, false));
        assert!(origin_is_local(&h(&[("origin", "http://localhost:5173"), ("host", "127.0.0.1:7788")]), 7788, false));
    }

    /// 審查（2026-10-01）：Host 的各種寫法。能放行的只有 `127.0.0.1`、`localhost`、`[::1]`（大小寫不拘、可帶純數字 port）；
    /// 其他一律拒絕＝fail closed——包括「其實指向本機」的寫法（`localhost.`、`[::ffff:127.0.0.1]`、`127.1`），瀏覽器不會這樣送，
    /// 寧可擋掉也不要讓比對變成可被繞的字串遊戲。
    #[test]
    fn host_shapes_are_matched_exactly_not_by_prefix_or_suffix() {
        for ok in ["127.0.0.1:1", "127.0.0.1:65535", "LocalHost:7788", "[::1]:0", "  localhost:7788  "] {
            assert!(origin_is_local(&h(&[("host", ok)]), 7788, false), "{ok:?}");
        }
        for bad in [
            "localhost.", "localhost.:7788", "127.0.0.1.", "127.1", "2130706433", "0.0.0.0:7788", "[::ffff:127.0.0.1]", "[0:0:0:0:0:0:0:1]",
            "localhost:", "localhost:7788:", "localhost:7788:80", "localhost:+80", "localhost:８０", "localhost:7788@evil.example",
            "evil.example#localhost", "evil.example/localhost", "localhost@evil.example", "user@localhost", "localhost,evil.example",
            "localhost evil.example", "foo.localhost", "localhost.evil.example:7788", "127.0.0.1:7788.evil.example", "[::1]x", "[::1", "::1",
            "ⅼocalhost", "",
        ] {
            assert!(!origin_is_local(&h(&[("host", bad)]), 7788, false), "{bad:?}");
        }
        // Origin 也是整段比對：帶路徑、帳密、別的 scheme、`null` 都不收。
        for bad in ["http://localhost@evil.example", "http://localhost.evil.example", "http://localhost/x", "ftp://localhost", "null", "file://", "http://127.0.0.1:7788.evil.example"] {
            assert!(!origin_is_local(&h(&[("host", "127.0.0.1:7788"), ("origin", bad)]), 7788, false), "origin {bad:?}");
        }
    }

    #[test]
    fn allow_lan_keeps_accepting_lan_names() {
        assert!(origin_is_local(&h(&[("host", "agm-host.tailnet.ts.net:7788")]), 7788, true));
    }
}

/// `allow_lan` 開著（dev／Tailscale 存取）時的暴露面。以前這時 Host 與 Origin 一律放行：使用者瀏覽器開著任何網頁，
/// 攻擊者把自己的網域 DNS rebind 到區網上的 daemon，同源之後 `GET /api/session` 就把 UI token 交出去。
/// 區網存取用的是 IP、`localhost`、單一標籤主機名、`.local`、`.ts.net` 這類攻擊者拿不到的名字；其他名字不放行
/// （要用自訂網域，`AM_ALLOWED_HOSTS` 明列）。
#[cfg(test)]
mod lan_exposure_tests {
    use super::origin_is_local;
    use axum::http::{HeaderMap, HeaderValue};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn a_rebound_hostname_is_refused_in_lan_mode_too() {
        for host in ["evil.example:7788", "evil.example", "127.0.0.1.evil.example:7788", "192.168.1.5.nip.io", "x.ts.net.evil.example", "local"] {
            let ok = origin_is_local(&h(&[("host", host)]), 7788, true);
            // `local` 是單一標籤（允許）；其他都不行。
            assert_eq!(ok, host == "local", "{host}");
        }
        assert!(!origin_is_local(&h(&[("host", "192.168.1.5:7788"), ("origin", "http://evil.example")]), 7788, true), "別的網頁從瀏覽器打過來");
        assert!(!origin_is_local(&h(&[("host", "192.168.1.5:7788"), ("origin", "null")]), 7788, true));
        assert!(!origin_is_local(&h(&[("host", "192.168.1.5:7788"), ("origin", "http://evil.example@192.168.1.5")]), 7788, true));
    }

    #[test]
    fn lan_names_a_user_actually_types_still_pass() {
        for host in [
            "192.168.1.5:7788", "10.0.0.2", "100.64.0.9:7788", "[fd7a:115c:a1e0::1]:7788", "[::1]", "localhost:5173",
            "agm-host.tailnet.ts.net:7788", "box.local", "agm-host", "nas.home.arpa",
        ] {
            assert!(origin_is_local(&h(&[("host", host)]), 7788, true), "{host}");
        }
        assert!(origin_is_local(&h(&[("host", "192.168.1.5:7788"), ("origin", "http://192.168.1.5:7788")]), 7788, true));
        assert!(origin_is_local(&h(&[("host", "agm-host.tailnet.ts.net:7788"), ("origin", "https://agm-host.tailnet.ts.net")]), 7788, true));
        assert!(origin_is_local(&h(&[("host", "192.168.1.5:7788")]), 7788, true), "沒有 Origin 的（curl、同源 GET）照舊");
        assert!(origin_is_local(&h(&[]), 7788, true));
    }

    /// vite dev server（5173）的 proxy 一律把 Host／Origin 改成 daemon 自己的位址（`changeOrigin` ＋ `rewriteOrigin`），
    /// 所以不論使用者用 IP、.ts.net 或 localhost 連 5173，daemon 看到的都是這一組：兩種模式都要放行，不能被 403。
    #[test]
    fn what_the_vite_proxy_forwards_passes_in_both_modes() {
        for lan in [false, true] {
            assert!(origin_is_local(&h(&[("host", "127.0.0.1:7788"), ("origin", "http://127.0.0.1:7788")]), 7788, lan), "allow_lan={lan}");
        }
    }

    #[test]
    fn an_explicitly_allowed_hostname_passes() {
        assert!(super::lan_host_ok("agm.example.com:7788", &["agm.example.com".to_string()]));
        assert!(!super::lan_host_ok("evil.example.com", &["agm.example.com".to_string()]));
    }

    async fn raw(app: std::sync::Arc<crate::state::App>, request: &str) -> String {
        let router = super::router(app);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        c.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    /// 端到端：LAN 模式下，rebind 過來的 Host 拿不到 token、也不能用 token 打 /api。
    #[tokio::test]
    async fn lan_mode_does_not_hand_the_token_to_a_rebound_host() {
        let env = crate::testing::env().await;
        let app = crate::testing::restart_app_lan(&env, true).await;
        let rebound = raw(app.clone(), "GET /api/session HTTP/1.1\r\nHost: evil.example:7788\r\nConnection: close\r\n\r\n").await;
        assert!(rebound.starts_with("HTTP/1.1 403"), "{rebound}");
        assert!(!rebound.contains("test-token"), "{rebound}");
        let lan = raw(app.clone(), "GET /api/session HTTP/1.1\r\nHost: 192.168.1.5:7788\r\nConnection: close\r\n\r\n").await;
        assert!(lan.starts_with("HTTP/1.1 200"), "區網 IP 存取照舊（#556：持有 UI token 就是使用者，allow_lan 的風險使用者已接受）：{lan}");
        let api = raw(app.clone(), "GET /api/state HTTP/1.1\r\nHost: evil.example\r\nX-AM-Token: test-token\r\nConnection: close\r\n\r\n").await;
        assert!(api.starts_with("HTTP/1.1 403"), "有 token 也不行：{api}");
        let ws = raw(app, "GET /ws?token=test-token HTTP/1.1\r\nHost: 192.168.1.5:7788\r\nOrigin: http://evil.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await;
        assert!(ws.starts_with("HTTP/1.1 403"), "別的網頁開 WebSocket：{ws}");
    }

    /// 沒有憑證的呼叫端看到的錯誤不能帶出路徑或內部細節。
    #[tokio::test]
    async fn unauthenticated_errors_do_not_leak_paths() {
        let env = crate::testing::env().await;
        let data = env.app.data_dir.display().to_string();
        let app = crate::testing::restart_app_lan(&env, true).await;
        for req in [
            "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            "GET /api/bots/x/outbox/file?path=../../etc/passwd HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            "GET /api/nope HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            "POST /hook/claude HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            "POST /relay/announce HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "GET /../../etc/passwd HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        ] {
            let resp = raw(app.clone(), req).await;
            assert!(!resp.contains(&data) && !resp.contains("/home/") && !resp.contains("/Users/") && !resp.contains("root:"), "{req}\n{resp}");
        }
        let state = raw(app, "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await;
        assert!(state.starts_with("HTTP/1.1 401"), "{state}");
    }

    /// service principal 只能打明列路徑：UI／bot 的路徑（含 /api/state、/api/session 之外的寫入）一律 403。
    #[tokio::test]
    async fn a_service_principal_is_confined_to_its_listed_paths() {
        let env = crate::testing::env().await;
        let app = crate::testing::restart_app_lan(&env, true).await;
        app.service_tokens.write().unwrap().insert("herdr-upgrade".into(), "svc-token".into());
        for (method, path) in [
            ("GET", "/api/state"), ("GET", "/api/supervisor/state/"), ("POST", "/api/bots/x/prompt"), ("POST", "/api/bots/x/keys"),
            ("GET", "/api/panes/"), ("POST", "/api/services/daemon-swap/restart-window"), ("POST", "/api/services/herdr-upgrade/resume/a/b"),
            ("GET", "/api/capabilities?x=1/../state"), ("DELETE", "/api/hosts/m4p"), ("PUT", "/api/drafts/k"),
        ] {
            let resp = raw(app.clone(), &format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Service-Id: herdr-upgrade\r\nX-AM-Service-Token: svc-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await;
            let allowed_listed = path.starts_with("/api/capabilities");
            assert!(allowed_listed || resp.starts_with("HTTP/1.1 403") || resp.starts_with("HTTP/1.1 404") || resp.starts_with("HTTP/1.1 405"), "{method} {path}: {}", resp.lines().next().unwrap_or(""));
        }
        // 同時帶 UI token 與 service 身分：拒絕，不降級。
        let mixed = raw(app, "GET /api/capabilities HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Service-Id: herdr-upgrade\r\nX-AM-Service-Token: svc-token\r\nX-AM-Token: test-token\r\nConnection: close\r\n\r\n").await;
        assert!(mixed.starts_with("HTTP/1.1 401"), "{mixed}");
    }
}

#[cfg(test)]
mod host_target_tests {
    use super::*;
    use crate::config::host_target_problem;

    /// `POST /api/hosts` 的 `ssh` 是 ssh 的一個 argv、`herdr_session` 會進遠端路徑與 plist：形狀不對一律 400，不寫進 config。
    #[test]
    fn a_host_target_cannot_smuggle_an_ssh_option_or_a_path() {
        for ok in ["m4p@100.112.229.82", "build-box", "ssh-alias", "user@host.example.com", "[::1]", "10.0.0.5"] {
            assert_eq!(host_target_problem(ok, "agents-manager"), None, "{ok}");
        }
        for bad in ["-oProxyCommand=touch /tmp/pwned", "-F/tmp/evil", "host name", "host\nname", "ho\tst", "a\u{0}b", ""] {
            assert!(host_target_problem(bad, "agents-manager").is_some(), "{bad:?}");
        }
        for ok in ["agents-manager", "am_v40", "Session.1"] {
            assert_eq!(host_target_problem("box", ok), None, "{ok}");
        }
        for bad in ["../../etc", "a/b", "-x", ".hidden", "a b", "a;b", "a\"b", "<x>", "a&b", "x\ny"] {
            assert!(host_target_problem("box", bad).is_some(), "{bad:?}");
        }
    }

    fn opts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// `ssh_opts` 是「原樣附加到每個 ssh 指令」：`-oProxyCommand=…`／`LocalCommand` 就是在**跑 daemon 的這台機器**上執行任意命令。
    /// `POST /api/hosts` 任何 principal（含 bot 的 hook token）都能呼叫，以前只驗 `ssh` 與 `herdr_session`，這個欄位完全沒驗。
    /// 只放行真的用得到的：`-i 檔`、`-o 白名單鍵=值`（dev sshd 與 ConnectTimeout 那類）。
    #[test]
    fn ssh_opts_cannot_carry_a_command_to_run_locally() {
        for ok in [
            opts(&[]),
            opts(&["-o", "ConnectTimeout=2"]),
            opts(&["-i", "/home/u/.ssh/id_ed25519", "-o", "UserKnownHostsFile=/tmp/k", "-o", "StrictHostKeyChecking=yes"]),
            opts(&["-oConnectTimeout=5", "-oStrictHostKeyChecking=no"]),
            opts(&["-o", "serveraliveinterval=30"]),
            opts(&["-4"]),
        ] {
            assert_eq!(crate::config::ssh_opts_problem(&ok), None, "{ok:?}");
        }
        for bad in [
            opts(&["-oProxyCommand=touch /tmp/pwned"]),
            opts(&["-o", "ProxyCommand=touch /tmp/pwned"]),
            opts(&["-o", "proxycommand=touch /tmp/pwned"]),
            opts(&["-o", "ProxyCommand touch /tmp/pwned"]),
            opts(&["-o", "PermitLocalCommand=yes", "-o", "LocalCommand=touch /tmp/pwned"]),
            opts(&["-o", "KnownHostsCommand=touch /tmp/pwned"]),
            opts(&["-o", "Include=/tmp/evil.conf"]),
            opts(&["-F", "/tmp/evil.conf"]),
            opts(&["-J", "-oProxyCommand=true"]),
            opts(&["-i", "-oProxyCommand=true"]),
            opts(&["-o"]),
            opts(&["-o", "ConnectTimeout"]),
            opts(&["host"]),
            opts(&["-o", "ConnectTimeout=2\nProxyCommand=true"]),
            opts(&["-L", "8080:localhost:80"]),
            opts(&["-p", "2222"]),
        ] {
            assert!(crate::config::ssh_opts_problem(&bad).is_some(), "{bad:?}");
        }
        assert!(host_target_problem("me@-oProxyCommand=true", "agents-manager").is_some(), "@ 後面以 - 開頭的主機名也會被當成選項");
    }

    #[tokio::test]
    async fn create_host_refuses_ssh_opts_that_run_a_command_before_touching_config() {
        let e = crate::testing::env().await;
        let before = e.app.cfg.get().await.hosts.len();
        let err = create_host(
            State(e.app.clone()),
            Query(DeleteQuery::default()),
            Json(NewHost { name: "evil2".into(), ssh: "me@10.0.0.9".into(), ssh_port: None, ssh_opts: Some(opts(&["-o", "ProxyCommand=touch /tmp/pwned"])), herdr_session: None, remote_path: None, shared_session: None }),
        )
        .await
        .expect_err("ProxyCommand must be refused");
        assert!(matches!(err, LcError::Bad(_)), "{err:?}");
        assert_eq!(e.app.cfg.get().await.hosts.len(), before, "什麼都沒寫");
    }

    #[tokio::test]
    async fn create_host_refuses_a_target_that_looks_like_an_option_before_touching_config() {
        let e = crate::testing::env().await;
        let before = e.app.cfg.get().await.hosts.len();
        let err = create_host(
            State(e.app.clone()),
            Query(DeleteQuery::default()),
            Json(NewHost { name: "evil1".into(), ssh: "-oProxyCommand=true".into(), ssh_port: None, ssh_opts: None, herdr_session: None, remote_path: None, shared_session: None }),
        )
        .await
        .expect_err("an option-looking target must be refused");
        assert!(matches!(err, LcError::Bad(_)), "{err:?}");
        assert_eq!(e.app.cfg.get().await.hosts.len(), before, "什麼都沒寫");
    }
}
