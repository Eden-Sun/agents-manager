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

fn any_err<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// 定案之後的工作脫離請求生命週期（#1065）：工作在自己的 task 裡跑完，handler 只等它的結果。
/// client 斷線（handler future 被 drop）時工作照樣跑完、照常 complete／record_failure，intent 不會停在 running 沒人補。
pub(crate) async fn detach<T: Send + 'static>(
    key: &str,
    work: impl std::future::Future<Output = Result<T, LcError>> + Send + 'static,
) -> Result<T, LcError> {
    // 呼叫端是 task-local（config 寫入的稽核 `http=` 欄位讀它）：脫離之後要帶過去，不然工作裡寫的稽核都會變成 `-`。
    let caller = crate::config_audit::http_caller();
    #[cfg(test)]
    let done_key = key.to_string();
    // 先把呼叫端給的 future 裝箱（#1465）：下面兩層包裝（完成訊號、task-local scope）與 spawn 就只帶一個指標，
    // 不再各留一份幾十 KB 的副本——debug 建置下這個函式的 poll 框原本有 150 KiB 上下。
    let work = Box::pin(work);
    // 完成訊號在脫離的 task 裡發（handler 已經被 drop 時也要發）；只在測試編進去。
    let work = async move {
        let out = work.await;
        #[cfg(test)]
        work_done_for_test(&done_key).notify_one();
        out
    };
    // Box::pin（#1465）：debug 建置下 spawn 會把 future 一層一層以值傳下去，脫離的工作（restart／promote／fork／刪除）
    // 的 future 動輒數十 KB，不裝箱就是 1 MiB 上下的堆疊。
    let task = tokio::spawn(Box::pin(crate::config_audit::HTTP_CALLER.scope(caller, work)));
    #[cfg(test)]
    work_registry().lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), task.abort_handle());
    let out = task.await.map_err(any_err);
    #[cfg(test)]
    work_registry().lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    out?
}

/// 測試用：脫離出去的刪除工作以 subject id 登記，好讓測試模擬「行程死亡」時連它一起殺掉。
#[cfg(test)]
fn work_registry() -> &'static std::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>> {
    static V: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>> = std::sync::OnceLock::new();
    V.get_or_init(Default::default)
}

/// 測試用：脫離工作結束時對 `key` 發的完成訊號。permit 會留著，先發後等也收得到，測試不靠 sleep。
#[cfg(test)]
pub(crate) fn work_done_for_test(key: &str) -> Arc<tokio::sync::Notify> {
    static V: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Notify>>>> = std::sync::OnceLock::new();
    let map = V.get_or_init(Default::default);
    map.lock().unwrap_or_else(|e| e.into_inner()).entry(key.to_string()).or_default().clone()
}

/// 測試用：等脫離工作的完成訊號。逾時只在工作根本沒做完（修正前 handler 沒脫離、訊號永遠不會發）時才會到，
/// 讓測試 FAIL 而不是掛住；成功路徑不靠這個時間，完成訊號先發後等也照樣收得到。
#[cfg(test)]
pub(crate) async fn wait_work_done_for_test(key: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(60), work_done_for_test(key).notified())
        .await
        .expect("脫離工作沒做完：完成訊號沒有發（handler 沒有把工作脫離出去？）");
}

/// 殺掉 `key` 的脫離工作（它若還沒登記就回 false）。
#[cfg(test)]
pub(crate) fn kill_delete_work_for_test(key: &str) -> bool {
    match work_registry().lock().unwrap_or_else(|e| e.into_inner()).remove(key) {
        Some(h) => {
            h.abort();
            true
        }
        None => false,
    }
}

/// 寫設定的路徑專用：`ConfigStore::update` 在落盤前驗不過時回 **400 `config_invalid`**，而不是 502。
///
/// 502 的定義是「herdr／DB 出錯」（SPEC §3.1）；設定不合法是**請求的問題**，混成同一個碼，呼叫端分不出
/// 「你的設定有問題、改一下再送」跟「ssh 斷了、等一下重試」。`config_written: false` 是關鍵的一半：
/// 跟 `projection_refused` 的 `config_written: true` 相反，這次什麼都沒寫，直接重送修正後的請求就好。
pub(crate) fn cfg_err(e: anyhow::Error) -> LcError {
    // 寫之前磁碟被手改：這次沒寫，原樣重送即可（#1647）。
    if e.downcast_ref::<crate::config::ConfigChangedOnDisk>().is_some() {
        return LcError::conflict(
            "config.toml changed on disk; nothing was written",
            json!({"reason": "config_changed_on_disk", "config_written": false, "retryable": true,
                   "message": "config.toml 在這次寫入之前被手改了，這次沒有寫入；請重送同一個請求"}),
        );
    }
    match e.downcast_ref::<crate::projection::ConfigInvalid>() {
        // `to_string()` 而不是 `c.0`：Display 才帶著「（config.toml 未變更）」，那句是給人看的 recovery path。
        Some(c) => LcError::BadValue(
            json!({"error": "config_invalid", "message": c.to_string(), "config_written": false}),
        ),
        None => any_err(e),
    }
}

#[cfg(test)]
mod changed_on_disk_tests;

#[cfg(test)]
mod startup_readiness_tests;

/// 同時進行的附件上傳上限：body 先整個讀進記憶體（`Bytes`，單檔至多 `attach::MAX_BYTES`），不限並發就是 N × 50 MiB。
pub(crate) const UPLOAD_CONCURRENCY: usize = 4;

/// 占一個上傳名額直到回應結束（含 body 讀取）；滿了回 429，不排隊（排隊會讓慢速連線占住位置）。名額跟著 router 走，不是全域的。
async fn upload_slot(State(slots): State<Arc<tokio::sync::Semaphore>>, req: axum::extract::Request, next: Next) -> Response {
    match slots.try_acquire_owned() {
        Ok(permit) => {
            let response = next.run(req).await;
            drop(permit);
            response
        }
        Err(_) => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "2")],
            Json(json!({"error": "too_many_uploads", "message": "同時上傳的附件太多，請稍後重試", "max": UPLOAD_CONCURRENCY})),
        )
            .into_response(),
    }
}

/// axum extractor（Json／Query／Path）驗不過時回的是 text/plain；API.md §1 的錯誤一律 JSON。
/// 只包這三種狀態碼的純文字回應，狀態碼不動；handler 自己回的 JSON 錯誤原樣通過。
async fn json_rejections(req: axum::extract::Request, next: Next) -> Response {
    let res = next.run(req).await;
    let status = res.status();
    let plain = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("text/plain"));
    if !plain || !matches!(status, StatusCode::BAD_REQUEST | StatusCode::UNSUPPORTED_MEDIA_TYPE | StatusCode::UNPROCESSABLE_ENTITY) {
        return res;
    }
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap_or_default();
    (status, Json(json!({"error": "bad_request", "message": String::from_utf8_lossy(&bytes)}))).into_response()
}

/// AGM 的管理面 route layer：被證明身份的一般 bot 403（`supervisor::bot_requests::forbid_plain_bot`）。
macro_rules! agm_gate {
    ($app:expr) => {
        axum::middleware::from_fn_with_state($app.clone(), crate::runners::supervisor::gate_plain_bots)
    };
}

pub fn router(app: Arc<App>) -> Router {
    let upload_slots = Arc::new(tokio::sync::Semaphore::new(UPLOAD_CONCURRENCY));
    let api = Router::new()
        .route("/projects/{id}/peers", get(crate::expert_peers::list))
        .route("/spawn-authority", get(crate::expert_peers::spawn_authority))
        .route("/model-mode", get(crate::model_mode::get).put(crate::model_mode::put))
        .route("/push/vapid-key", get(crate::webpush::get_vapid_key))
        .route("/push/subscriptions", get(crate::webpush::list).post(crate::webpush::subscribe).delete(crate::webpush::unsubscribe))
        .route("/push/test", post(crate::webpush::test_push))
        .route("/push/settings", get(crate::webpush::get_settings).put(crate::webpush::put_settings))
        .route("/push/presence", post(crate::webpush::post_presence))
        .route("/state", get(get_state))
        .route("/projects", post(create_project))
        .route("/order", post(set_order))
        .route("/intents", get(list_intents))
        // 對話輸入框的草稿（各瀏覽器共用，見 `drafts.rs`）。
        .route("/drafts", get(crate::drafts::get_http))
        .route("/drafts/{key}", axum::routing::put(crate::drafts::put_http))
        // 專案排程（User-only，見 `schedules/api.rs`）。
        .route("/projects/{id}/schedules", get(crate::schedules::api::list_for_project).post(crate::schedules::api::create))
        .route("/projects/{id}/schedules/rebind", post(crate::schedules::api::rebind))
        .route("/schedules/{id}", get(crate::schedules::api::get_one).patch(crate::schedules::api::patch).delete(crate::schedules::api::delete))
        .route("/schedules/{id}/run", post(crate::schedules::api::run_now))
        .route("/schedules/{id}/history", get(crate::schedules::api::history))
        .route("/projects/{id}", patch(patch_project).delete(delete_project_route))
        .route("/projects/{id}/bots", post(create_bot))
        // §6.5e：非 agent 的 shell／服務 pane。
        .route("/projects/{id}/panes", get(crate::runners::panes::list_for_project))
        .route("/panes/{id}/adopt", post(crate::runners::panes::adopt))
        .route("/panes/{id}/close", post(crate::runners::panes::close))
        .route("/panes/{id}/alias", axum::routing::put(crate::runners::panes::set_alias))
        .route("/panes/{id}/focus", post(crate::runners::panes::focus))
        .route("/panes", get(crate::runners::panes::list_all))
        .route("/identity-prefs", get(crate::identity_prefs::get_identity_prefs))
        .route("/identities/{name}/disabled", axum::routing::put(crate::identity_prefs::put_identity_disabled))
        .route("/projects/{id}/github/refresh", post(refresh_github))
        .route("/projects/{id}/submodules", get(get_submodules))
        .route("/projects/{id}/git", get(get_git))
        .route("/projects/{id}/git/commit", post(git_commit))
        .route("/projects/{id}/git/push", post(git_push))
        .route("/projects/{id}/git/pull", post(git_pull))
        .route("/projects/{id}/issues", get(get_issues))
        .route("/projects/{id}/issues/{number}", get(get_issue))
        .route("/bots/{id}", patch(patch_bot).delete(delete_bot_route))
        .route("/bots/{id}/role", axum::routing::put(put_bot_role))
        .route("/bots/{id}/base-snapshot", get(crate::schedules::base::get_snapshot).put(crate::schedules::base::lock).delete(crate::schedules::base::clear))
        .route("/bots/{id}/member", post(crate::expert_member::adopt))
        .route("/projects/{id}/members", post(hire_member))
        // SPEC §6.9；放在 `{id}` 那組前面，否則 `restart-idle` 會被當成 bot id。
        .route("/bots/restart-idle", post(restart_idle_bots))
        .route("/bots/{id}/api_fresh_target", get(api_fresh_target))
        .route("/bots/{id}/api-session/retire", post(crate::api_bot_retire_route::retire_api_session))
        .route("/bots/{id}/api-usage", get(api_bot_usage))
        .route("/bots/{id}/start", post(start_bot_http))
        .route("/bots/{id}/restart", post(restart_bot))
        .route("/bots/{id}/credential/rotate", post(rotate_bot_credential))
        .route("/bots/{id}/fork", post(crate::fork::fork_bot))
        .route("/bots/{id}/promote", post(crate::promote::promote_bot))
        .route("/bots/{id}/stop", post(stop_bot))
        .route("/bots/{id}/rewind", post(crate::runners::rewind::post_rewind))
        .route("/bots/{id}/preview", get(preview_get).post(preview_start).delete(preview_stop))
        .route("/bots/{id}/interrupt", post(interrupt_bot))
        .route("/bots/{id}/login", post(login_bot))
        .route("/bots/{id}/compact", post(compact_bot))
        .route("/bots/{id}/keep-warm/skip", post(keep_warm_skip_bot))
        .route("/bots/{id}/pane/move-to-tab", post(move_bot_pane_to_tab))
        .route("/bots/{id}/prompt", post(prompt_bot))
        .route("/relay-requests", get(list_relay_requests))
        .route("/relay/resolve", get(resolve_relay_target))
        .route("/pane-authorize", post(crate::pane_authority::authorize))
        // 分享 bot（SPEC「分享 bot」）：只收 UI token，handler 自己再擋一次 principal。
        .route("/bots/{id}/share", get(crate::runners::share_admin::get_share).post(crate::runners::share_admin::post_share))
        .route("/bots/{id}/share/rotate", post(crate::runners::share_admin::post_rotate))
        .route(
            "/bots/{id}/attachments",
            post(upload_attachment)
                .layer(DefaultBodyLimit::max(crate::attach::MAX_BYTES + 4096))
                .layer(axum::middleware::from_fn_with_state(upload_slots, upload_slot)),
        )
        .route("/attachments/{id}", get(get_attachment))
        .route("/bots/{id}/keys", post(keys_bot))
        .route("/bots/{id}/send-queued-now", post(send_queued_now_bot))
        .route("/bots/{id}/text", post(text_bot))
        .route("/bots/{id}/suggestion/accept", post(accept_suggestion))
        .route("/bots/{id}/messages", get(get_messages_with_relay_names))
        .route("/bots/{id}/terminal", get(get_terminal))
        .route("/bots/{id}/composer-draft", get(get_composer_draft))
        .route("/bots/{id}/pending-question", get(crate::runners::pending_question::get_pending_question))
        .route("/bots/{id}/local-image", get(crate::app_ports_p10::get))
        // bot 交給使用者的檔案（§6.5f）：只讀 outbox。scratchpad 不再給使用者，舊路徑明確 404。
        .route("/bots/{id}/outbox", get(crate::app_ports_p10::list))
        .route("/bots/{id}/outbox/file", get(crate::app_ports_p10::file))
        .route("/bots/{id}/scratchpad", get(crate::outbox::scratchpad_gone))
        .route("/bots/{id}/scratchpad/file", get(crate::outbox::scratchpad_gone))
        .route("/bots/{id}/read", post(crate::read_marks::post))
        .route("/turns/{id}/abandon", post(abandon_turn))
        .route("/turns/{id}/withdraw", post(withdraw_turn_route))
        .route("/bots/{id}/abort", post(abort_bot))
        .route("/hosts", post(create_host))
        .route("/hosts/{name}", delete(delete_host))
        .route("/hosts/{name}/reconnect", post(reconnect_host))
        .route("/hosts/{name}/tools/refresh", post(refresh_tools))
        .route("/hosts/{name}/tools/install", post(install_tool))
        .route("/hosts/{name}/baseline/plan", get(baseline_plan))
        .route("/hosts/{name}/baseline/apply", post(baseline_apply))
        .route("/hosts/{name}/baseline/restore", post(baseline_restore))
        // header 一鍵升級 codex：裝好、驗版本、接著重啟那台的 codex（SPEC §6.9）。只給 UI。
        .route("/hosts/{name}/cli-update", post(crate::cli_update::post_cli_update))
        // header 一鍵升級 herdr：下載驗版本 → 等閒置 → 維護窗口裡重啟 server → bot 接回（SPEC §6.9b）。只給 UI。
        .route("/hosts/{name}/herdr-update", post(crate::herdr_upgrade::post_herdr_update))
        .route("/hosts/{name}/identities/{identity}/login", post(login_identity))
        .route("/hosts/{name}/identities/{identity}/logout", post(logout_identity))
        .route("/hosts/{name}/agy/logout", post(logout_agy))
        .route("/hosts/{name}/agy/install", post(install_agy))
        .route("/hosts/{name}/gh", get(get_gh_status))
        .route("/hosts/{name}/gh/login", post(login_gh))
        .route("/hosts/{name}/gh/cancel", post(cancel_gh))
        .route("/hosts/{name}/shells", get(list_host_shells).post(open_host_shell))
        .route("/hosts/{name}/shells/{pane_id}", delete(close_host_shell))
        .route("/hosts/{name}/shells/{pane_id}/terminal", get(get_host_shell_terminal))
        .route("/hosts/{name}/shells/{pane_id}/text", post(host_shell_text))
        .route("/hosts/{name}/shells/{pane_id}/login", get(host_shell_login_status))
        .route("/hosts/{name}/shells/{pane_id}/login/code", post(host_shell_login_code))
        .route("/hosts/{name}/shells/{pane_id}/keys", post(host_shell_keys))
        .route("/models", get(get_models))
        .route("/changelog", get(get_changelog))
        .route(
            "/changelog/translation",
            get(crate::changelog_translate::get_translation).post(crate::changelog_translate::post_translation),
        )
        .route("/messages/{id}/translate", post(crate::message_translate::post_translate))
        .route("/pending-questions/translate", post(crate::message_translate::post_pending_question_translate))
        .route("/quota", get(get_quota))
        .route("/quota/probe", post(probe_quota))
        .route("/quota/reset-credit/use", post(use_reset_credit))
        // issue #90：build scheduler 的唯讀現況（UI 用一般 X-AM-Token）。acquire／renew／release 見下方
        // 的 `/build-slots/*`（不在 `/api` 底下：bot 的 pane 只有自己的 hook token，拿不到這個）。
        .route("/build-slots", get(crate::runners::build_scheduler::get_status))
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
        .route("/search/messages", get(search_messages).layer(agm_gate!(app)))
        // AGM 總管（docs/goals/agm-supervisor-environment-plan-2026-09-09.md）。
        .route("/supervisor", get(crate::runners::supervisor::api::get_supervisor).layer(agm_gate!(app)))
        .route("/supervisor/health", get(crate::runners::supervisor::api::get_health).layer(agm_gate!(app)))
        // AGM 的管理面：被證明身份的一般 bot 一律 403 `role_required`（`bot_requests::forbid_plain_bot`）。
        .route("/supervisor/setup", post(crate::runners::supervisor::api::post_setup).layer(agm_gate!(app)))
        .route("/supervisor/start", post(crate::runners::supervisor::api::post_start).layer(agm_gate!(app)))
        .route("/supervisor/stop", post(crate::runners::supervisor::api::post_stop).layer(agm_gate!(app)))
        .route("/supervisor/fallback", post(crate::runners::supervisor::api::post_fallback).layer(agm_gate!(app)))
        .route(
            "/supervisor/assignments",
            // POST 不掛整條 gate：一般 bot 可以對 AGM 角色 bot 送 `notice`（release／herdr 更新任務的 `agm assign --notice --bot <巡檢>`），
            // 其餘由 handler 自己判斷（`supervisor::api::post_assignment`）。
            get(crate::runners::supervisor::api::get_assignments).layer(agm_gate!(app)).post(crate::runners::supervisor::api::post_assignment),
        )
        .route(
            "/supervisor/handoff",
            get(crate::runners::supervisor::api::get_handoff).layer(agm_gate!(app)).merge(axum::routing::put(crate::runners::supervisor::api::put_handoff).layer(agm_gate!(app))),
        )
        .route("/supervisor/assignments/{id}", get(crate::runners::supervisor::api::get_assignment).layer(agm_gate!(app)))
        // 回合結束只到 awaiting_review；驗收／阻塞／續作／取消都走這支（SPEC §18.3）。
        .route("/supervisor/assignments/{id}/review", post(crate::runners::supervisor::api::post_review).layer(agm_gate!(app)))
        // 使用者在更新提示上按「請 AGM 解析」：把這一版的 changelog 派給協調者判讀（唯讀）。
        .route(
            "/claude-update/review",
            get(crate::claude_review::get_review).post(crate::claude_review::post_review),
        )
        .route("/supervisor/incidents", get(crate::runners::supervisor::api::get_incidents).layer(agm_gate!(app)))
        // 人設：持久版本是權威，內嵌版只在首次安裝當種子（SPEC §18.11）。
        .route(
            "/supervisor/persona",
            get(crate::runners::supervisor::api::get_persona).layer(agm_gate!(app)).put(crate::runners::supervisor::api::put_persona),
        )
        .route("/supervisor/persona/adopt-embedded", post(crate::runners::supervisor::api::post_persona_adopt))
        .route("/supervisor/build-inputs", get(crate::runners::supervisor::api::get_build_inputs).layer(agm_gate!(app)))
        // 左上角「立即部署」：落後多少、有沒有在跑；按下去交給既有的 daemon-update-kick（SPEC §18.2）。
        .route("/deploy/status", get(crate::deploy_now::get_status))
        .route("/deploy/now", post(crate::deploy_now::post_now))
        .route("/deploy/wait/escalate", post(crate::deploy_wait::post_escalate))
        .route("/deploy/wait/dismiss", post(crate::deploy_wait::post_dismiss))
        // 重建／重啟的核准與執行租約（SPEC §18.10）。
        .route(
            "/supervisor/approvals",
            get(crate::runners::supervisor::api::get_approvals).layer(agm_gate!(app)).post(crate::runners::supervisor::api::post_approval),
        )
        .route(
            "/supervisor/approvals/{id}/decide",
            post(crate::runners::supervisor::api::post_approval_decision).layer(agm_gate!(app)),
        )
        .route("/supervisor/maintenance/safety", get(crate::runners::supervisor::api::get_maintenance_safety).layer(agm_gate!(app)))
        .route("/supervisor/leases", get(crate::runners::supervisor::api::get_leases).layer(agm_gate!(app)))
        .route("/supervisor/leases/{resource}/acquire", post(crate::runners::supervisor::api::post_lease_acquire))
        .route("/supervisor/leases/{resource}/renew", post(crate::runners::supervisor::api::post_lease_renew))
        .route("/supervisor/leases/{resource}/release", post(crate::runners::supervisor::api::post_lease_release))
        .route("/supervisor/inbox", get(crate::runners::supervisor::api::get_inbox).layer(agm_gate!(app)))
        .route("/supervisor/inbox/{id}/ack", post(crate::runners::supervisor::api::post_inbox_ack))
        .route("/supervisor/state", get(crate::runners::supervisor::api::get_sanitized_state).layer(agm_gate!(app)))
        // 已安裝的 bin/agm vs 這顆 binary 內嵌的那份（SPEC §18.2a）：GET 比對、POST 就地換版，
        // 不必等下一次開機（issue #532）。
        .route(
            "/supervisor/cli",
            get(crate::runners::supervisor::cli::get_cli).layer(agm_gate!(app)).merge(post(crate::runners::supervisor::cli::post_cli_refresh).layer(agm_gate!(app))),
        )
        // 排程腳本卡住時喊人（SPEC §18.9）：只寫一則 durable inbox 事件。
        .route("/supervisor/ops-alerts", post(crate::runners::supervisor::api::post_ops_alert).layer(agm_gate!(app)))
        .route("/supervisor/evidence", get(crate::supervisor_evidence::search).layer(agm_gate!(app)))
        .merge(crate::runners::supervisor::responder_api::routes(app.clone()))
        .merge(crate::runners::release_triage::routes())
        .merge(crate::runners::upstream_update::routes())
        .merge(crate::herdr_integration::routes())
        .merge(crate::runners::judge::routes())
        .merge(crate::claude_api::routes())
        .route("/bots/{id}/restore", post(restore_bot))
        .route("/identities", post(create_identity))
        .route("/identities/{name}", delete(delete_identity))
        .route("/fs/dirs", get(list_dirs).post(make_dir))
        .route("/capabilities", get(get_capabilities))
        .route("/services/daemon-swap/probe/{id}", post(service_daemon_swap_probe))
        .route("/services/daemon-swap/restart-window", post(service_daemon_swap_restart_window))
        .route("/services/herdr-upgrade/resume/{id}", post(service_herdr_upgrade_resume))
        .route("/services/herdr-upgrade/notify", post(service_herdr_upgrade_notify))
        .route("/supervisor/herdr-maintenance", get(crate::runners::herdr_maintenance::get).layer(agm_gate!(app)))
        .route("/supervisor/herdr-maintenance/open", post(crate::runners::herdr_maintenance::open))
        .route("/supervisor/herdr-maintenance/end", post(crate::runners::herdr_maintenance::end))
        .layer(axum::middleware::from_fn(json_rejections))
        .layer(axum::middleware::from_fn_with_state(app.clone(), auth))
        .route("/session", get(get_session))
        // 沒這條路由的 /api/* 要回 JSON 404，不能掉到外層的 SPA fallback（200 的 index.html）。
        .fallback(api_route_not_found);

    Router::new()
        .nest("/api", api)
        .route("/ws", get(ws_handler))
        // 主機 shell 的畫面推送：WebSocket 瀏覽器帶不了 X-AM-Token，所以跟 `/ws` 一樣掛在 `/api` 認證層外、用 query token（shell_stream.rs）。
        .route("/api/hosts/{name}/shells/{pane_id}/stream", get(crate::shell_stream::stream))
        .route("/hook/{provider}", post(crate::runners::hookrecv::receive))
        .route("/relay/announce", post(relay_announce))
        // Credential-bearing pane creation is fenced against concurrent credential rotation.
        .route("/relay/spawn/begin", post(crate::runners::credential_spawn::begin))
        .route("/relay/spawn/finish", post(crate::runners::credential_spawn::finish))
        .route("/relay/spawn/abort", post(crate::runners::credential_spawn::abort))
        // issue #1790：開完子 agent 後登記「這顆 pane 是我開的」，歸屬以它為準。
        .route("/relay/spawn/claim", post(crate::runners::credential_spawn::claim))
        // §6.5e：bot 開完 pane 後回報用途（歸屬另外從行程環境推斷）。
        .route("/relay/pane", post(relay_pane))
        // 母 bot 的 `herdr agent list` 附上子 agent 的 cache 還熱多久（kids_cache）。
        .route("/relay/kids", post(relay_kids))
        // issue #90：cargo shim 用（bot 的 hook token，或人工 host shell 的一般 X-AM-Token）。
        .route("/build-slots/acquire", post(crate::runners::build_scheduler::post_acquire))
        .route("/build-slots/renew", post(crate::runners::build_scheduler::post_renew))
        .route("/build-slots/release", post(crate::runners::build_scheduler::post_release))
        .fallback(get(crate::assets::serve))
        .with_state(app.clone())
        .layer(axum::middleware::from_fn_with_state(app.clone(), startup_readiness))
        // 最外層：/api 與 /ws 以外（靜態檔、hook、relay、build-slots）以前不看 Host。
        .layer(axum::middleware::from_fn_with_state(app, reject_rebound_host))
}

async fn reject_rebound_host(State(app): State<Arc<App>>, req: axum::extract::Request, next: Next) -> Response {
    if !origin_is_local(req.headers(), app.port, app.allow_lan()) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "bad origin"}))).into_response();
    }
    next.run(req).await
}

async fn api_route_not_found() -> LcError {
    LcError::NotFound("route".into())
}

/// SPEC §6.5d：shim 先報，hook 回音時補 `relay_from`，總管裁示才不像使用者打的。
/// 驗證用該 bot 的 `hook_token`（`X-AM-Bot-Token`）：它只證明「我是那顆 bot」，也只用來標來源。
#[derive(serde::Deserialize)]
struct RelayAnnounce {
    bot_id: String,
    #[serde(default)]
    to_agent: String,
    #[serde(default)]
    to_bot_id: Option<String>,
    #[serde(default)]
    to_run_id: Option<String>,
    #[serde(default)]
    delivery_id: Option<String>,
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
    if crate::share::refuses_bot_principal(&app.db, &bot.id).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden", "reason": "restricted_bot"})));
    }
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

#[derive(Deserialize)]
struct RelayKids {
    bot_id: String,
}

/// 純文字：shim 原樣印到 stderr（見 `kids_cache`）。沒有子 agent＝空字串。
async fn relay_kids(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Form(body): axum::extract::Form<RelayKids>,
) -> (StatusCode, String) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    match db::bot(&app.db, &body.bot_id).await {
        Ok(Some(b)) if b.deleted_at.is_none() && !token.is_empty() && ct_eq(token, &b.hook_token) => {}
        _ => return (StatusCode::UNAUTHORIZED, String::new()),
    }
    if crate::share::refuses_bot_principal(&app.db, &body.bot_id).await {
        return (StatusCode::FORBIDDEN, String::new());
    }
    match crate::kids_cache::text_for(&app, &body.bot_id).await {
        Ok(text) => (StatusCode::OK, text),
        Err(e) => {
            tracing::warn!(bot = %body.bot_id, error = ?e, "could not list child cache state");
            (StatusCode::SERVICE_UNAVAILABLE, String::new())
        }
    }
}

/// A shim freezes a selected name to a pane before announcing. Never reinterpret that pane as a name.
async fn relay_target_run(app: &Arc<App>, sender: &str, target: &str) -> anyhow::Result<Option<db::Run>> {
    let host = db::bot_host(&app.db, sender).await?;
    let Some(fallback) = app.session_for_host(&host).await else { return Ok(None) };
    let sender_run = db::active_run(&app.db, sender).await?;
    let session = sender_run.as_ref().and_then(|r| r.herdr_session.as_deref()).unwrap_or(fallback.as_str());
    let (bot_id, pane_id) = match target.strip_prefix("bot:") {
        Some(id) => (Some(id), None),
        None => (None, Some(target)),
    };
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT r.id FROM runs r JOIN bots b ON b.id=r.bot_id JOIN projects p ON p.id=b.project_id
         WHERE r.state='running' AND b.deleted_at IS NULL AND p.handed_off_to IS NULL AND p.host=?
           AND COALESCE(r.herdr_session,?)=? AND (b.id=? OR r.pane_id=?) LIMIT 2")
        .bind(&host).bind(&fallback).bind(session).bind(bot_id).bind(pane_id).fetch_all(&app.db).await?;
    if ids.len() != 1 { return Ok(None); }
    db::run(&app.db, &ids[0]).await
}

async fn relay_announce(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    // 表單而非 JSON：POSIX sh shim 用 `--data-urlencode` 對任意內容都安全。
    axum::extract::Form(mut body): axum::extract::Form<RelayAnnounce>,
) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let ok = match db::bot(&app.db, &body.bot_id).await {
        Ok(Some(b)) if b.deleted_at.is_none() => !token.is_empty() && ct_eq(token, &b.hook_token),
        _ => false,
    };
    if !ok {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unknown bot or bad token"})));
    }
    if crate::share::refuses_bot_principal(&app.db, &body.bot_id).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden", "reason": "restricted_bot"})));
    }
    // A frozen pane or explicit bot selector supplies stable recipient evidence; never resolve display names.
    if body.to_bot_id.as_deref().is_none_or(str::is_empty) && (body.to_agent.starts_with("bot:") || body.to_agent.contains(":p")) {
        match relay_target_run(&app, &body.bot_id, &body.to_agent).await {
            Ok(run) => {
                body.to_bot_id = body.to_agent.strip_prefix("bot:").map(str::to_owned)
                    .or_else(|| run.as_ref().map(|r| r.bot_id.clone()));
                body.to_run_id = run.map(|r| r.id);
            }
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"relay_watch_unavailable", "retryable":true, "detail":format!("{e:#}")}))),
        }
    }
    // Resolve the actual live run and enforce current authorization for every managed relay.
    let resolved = match relay_target_run(&app, &body.bot_id, &body.to_agent).await {
        Ok(v) => v,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"relay_watch_unavailable","watch_unavailable":true,"retryable":true}))),
    };
    let Some(target) = resolved.as_ref() else {
        return (StatusCode::FORBIDDEN, Json(json!({"error":"relay_target_unverified","message":"無法確認目標 bot，請使用 bot ID"})));
    };
    if body.to_bot_id.as_deref().filter(|id| !id.is_empty()).is_some_and(|id| id != target.bot_id) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"relay_recipient_mismatch"})));
    }
    if let Err(e) = crate::relay_policy::relay_prompt_allowed(&app, &body.bot_id, &target.bot_id, crate::relay_policy::Admission::NewRequest).await {
        return match e {
            crate::relay_policy::RelayDenied::MemberDisabled(message) => (StatusCode::FORBIDDEN, Json(crate::relay_policy::member_disabled_body(&message))),
            crate::relay_policy::RelayDenied::Denied(reason) => (StatusCode::FORBIDDEN, Json(json!({"error":"forbidden","reason":reason}))),
            crate::relay_policy::RelayDenied::Unavailable(reason) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"authorization_unavailable","reason":reason}))),
        };
    }
    body.to_bot_id = Some(target.bot_id.clone());
    body.to_run_id = Some(target.id.clone());
    // 舊 shim 只有顯示名稱：缺 ID 的報備保持未知，不攔進角色佇列、不標寄件來源。
    let Some(to_bot_id) = body.to_bot_id.as_deref().filter(|id| !id.is_empty()) else {
        return (StatusCode::OK, Json(json!({})));
    };
    let Some(delivery_id) = body.delivery_id.as_deref().filter(|id| !id.is_empty()) else {
        return (StatusCode::OK, Json(json!({})));
    };
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
    match crate::supervisor::bot_requests::role_bot_by_id(&app, to_bot_id).await {
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
        match crate::lifecycle::relay_watch::resolve(&app, &body.bot_id, body.to_run_id.as_deref().unwrap_or_default()).await {
            Ok(run) => {
                // #1460：沒有目標（含移交出去的）就只看一眼是不是移交，留 log；正常路徑不多一次查詢。
                if run.is_none() {
                    crate::lifecycle::relay_watch::note_handoff_direct(&app, &body.bot_id, body.to_run_id.as_deref().unwrap_or_default()).await;
                }
                run
            }
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
    if let Some(run) = relay_run.as_ref() {
        if run.bot_id != to_bot_id {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "relay_recipient_mismatch"})));
        }
    }
    // 報備記的是寄件者所在的主機（收件 run 只能在同一台主機認領）；讀不到就跟收件目標讀不到一樣，要 shim 重試。
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
    // 寄件 bot 送這句當下正在跑的回合（#927，child_done 用它認「這一回合已經自己回報過」）。找不到不擋送出，只是少一個綁定。
    let from_turn = match crate::lifecycle::relay_watch::sender_turn_at(&app, &body.bot_id, &db::now()).await {
        Ok(turn) => turn,
        Err(e) => {
            tracing::warn!(bot = %body.bot_id, error = ?e, "relay announce could not find the sender's running turn");
            None
        }
    };
    let Some(run) = relay_run.as_ref() else { return (StatusCode::OK, Json(json!({}))) };
    let Some(pane_id) = run.pane_id.clone().filter(|p| !p.is_empty()) else {
        return (StatusCode::OK, Json(json!({})));
    };
    if !crate::agent_relay::announce_delivery(&host, &body.bot_id, &run.id, delivery_id, &body.text, from_turn.as_deref()) {
        return (StatusCode::OK, Json(json!({"to_pane_id": pane_id})));
    }
    // #380：收件方 UI 看得出在跑，字卡在輸入列時補 Enter；先登記受關機屏障管理的 watcher，再回 2xx。
    crate::lifecycle::relay_watch::on_resolved_announce(&app, &body.bot_id, from_turn.as_deref(), &body.text, relay_run).await;
    (StatusCode::OK, Json(json!({"to_pane_id": pane_id})))
}

/// issue #143：寫給 AGM 的申請，路由狀態**不知道**不等於「不是 AGM」。
#[cfg(test)]
mod relay_announce_tests;

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
pub(crate) fn origin_is_local(headers: &HeaderMap, _port: u16, allow_lan: bool) -> bool {
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
    Child { child_id: String, parent_bot_id: String },
    Service(String),
}

/// Central policy for every Bot-principal route fence. `UserOnly` also denies registered AGM
/// roles; `UserOrAgm` preserves delegated management access for registered roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BotRoutePolicy {
    UserOnly,
    UserOrAgm,
}

const BOT_ROUTE_POLICIES: &[(&str, &str, BotRoutePolicy)] = &[
    // 基準快照：User，加上同專案 expert 管理者（見 `manager_base_snapshot`）；其他 Bot（含自己）不行。
    ("GET", "/api/bots/{id}/base-snapshot", BotRoutePolicy::UserOnly),
    ("PUT", "/api/bots/{id}/base-snapshot", BotRoutePolicy::UserOnly),
    ("DELETE", "/api/bots/{id}/base-snapshot", BotRoutePolicy::UserOnly),
    ("GET", "/api/model-mode", BotRoutePolicy::UserOnly),
    ("PUT", "/api/model-mode", BotRoutePolicy::UserOnly),
    ("GET", "/api/push/vapid-key", BotRoutePolicy::UserOnly),
    ("GET", "/api/push/subscriptions", BotRoutePolicy::UserOnly),
    ("POST", "/api/push/subscriptions", BotRoutePolicy::UserOnly),
    ("DELETE", "/api/push/subscriptions", BotRoutePolicy::UserOnly),
    ("POST", "/api/push/test", BotRoutePolicy::UserOnly),
    ("GET", "/api/push/settings", BotRoutePolicy::UserOnly),
    ("PUT", "/api/push/settings", BotRoutePolicy::UserOnly),
    ("POST", "/api/push/presence", BotRoutePolicy::UserOnly),
    ("DELETE", "/api/hosts/{name}/shells/{pane_id}", BotRoutePolicy::UserOnly),
    ("GET", "/api/attachments/{id}", BotRoutePolicy::UserOnly),
    ("GET", "/api/bots/{id}/local-image", BotRoutePolicy::UserOnly),
    ("GET", "/api/bots/{id}/api_fresh_target", BotRoutePolicy::UserOnly),
    ("POST", "/api/bots/{id}/api-session/retire", BotRoutePolicy::UserOnly),
    ("GET", "/api/bots/{id}/api-usage", BotRoutePolicy::UserOnly),
    ("GET", "/api/bots/{id}/outbox", BotRoutePolicy::UserOnly),
    ("GET", "/api/bots/{id}/outbox/file", BotRoutePolicy::UserOnly),
    ("GET", "/api/build-slots", BotRoutePolicy::UserOnly),
    ("GET", "/api/changelog", BotRoutePolicy::UserOnly),
    ("GET", "/api/changelog/translation", BotRoutePolicy::UserOnly),
    ("POST", "/api/changelog/translation", BotRoutePolicy::UserOnly),
    ("GET", "/api/deploy/status", BotRoutePolicy::UserOnly),
    ("POST", "/api/deploy/wait/escalate", BotRoutePolicy::UserOnly),
    ("POST", "/api/deploy/wait/dismiss", BotRoutePolicy::UserOnly),
    ("GET", "/api/drafts", BotRoutePolicy::UserOnly),
    ("GET", "/api/hosts/{name}/shells", BotRoutePolicy::UserOnly),
    ("GET", "/api/hosts/{name}/shells/{pane_id}/terminal", BotRoutePolicy::UserOnly),
    ("GET", "/api/intents", BotRoutePolicy::UserOnly),
    ("GET", "/api/mem", BotRoutePolicy::UserOnly),
    ("GET", "/api/mem/processes", BotRoutePolicy::UserOnly),
    ("GET", "/api/mem/processes/pane", BotRoutePolicy::UserOnly),
    ("GET", "/api/panes", BotRoutePolicy::UserOnly),
    ("GET", "/api/projects/{id}/panes", BotRoutePolicy::UserOnly),
    ("POST", "/api/bots/{id}/attachments", BotRoutePolicy::UserOnly),
    ("POST", "/api/bots/{id}/keys", BotRoutePolicy::UserOnly),
    ("POST", "/api/bots/{id}/send-queued-now", BotRoutePolicy::UserOnly),
    ("POST", "/api/messages/{id}/translate", BotRoutePolicy::UserOnly),
    ("POST", "/api/bots/{id}/read", BotRoutePolicy::UserOnly),
    ("POST", "/api/bots/{id}/suggestion/accept", BotRoutePolicy::UserOnly),
    ("POST", "/api/bots/{id}/text", BotRoutePolicy::UserOnly),
    ("POST", "/api/hosts/{name}/shells", BotRoutePolicy::UserOnly),
    ("POST", "/api/hosts/{name}/shells/{pane_id}/keys", BotRoutePolicy::UserOnly),
    ("POST", "/api/hosts/{name}/shells/{pane_id}/text", BotRoutePolicy::UserOnly),
    ("GET", "/api/hosts/{name}/shells/{pane_id}/login", BotRoutePolicy::UserOnly),
    ("POST", "/api/hosts/{name}/shells/{pane_id}/login/code", BotRoutePolicy::UserOnly),
    ("POST", "/api/panes/{id}/adopt", BotRoutePolicy::UserOnly),
    ("POST", "/api/panes/{id}/close", BotRoutePolicy::UserOnly),
    ("POST", "/api/panes/{id}/focus", BotRoutePolicy::UserOnly),
    ("PUT", "/api/panes/{id}/alias", BotRoutePolicy::UserOnly),
    ("PUT", "/api/drafts/{key}", BotRoutePolicy::UserOnly),
    ("GET", "/api/projects/{id}/schedules", BotRoutePolicy::UserOnly),
    ("POST", "/api/projects/{id}/schedules", BotRoutePolicy::UserOnly),
    ("POST", "/api/projects/{id}/schedules/rebind", BotRoutePolicy::UserOnly),
    ("GET", "/api/schedules/{id}", BotRoutePolicy::UserOnly),
    ("PATCH", "/api/schedules/{id}", BotRoutePolicy::UserOnly),
    ("DELETE", "/api/schedules/{id}", BotRoutePolicy::UserOnly),
    ("POST", "/api/schedules/{id}/run", BotRoutePolicy::UserOnly),
    ("GET", "/api/schedules/{id}/history", BotRoutePolicy::UserOnly),
    ("GET", "/api/fs/dirs", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/fs/dirs", BotRoutePolicy::UserOnly),
    ("GET", "/api/build/remote", BotRoutePolicy::UserOrAgm),
    ("PUT", "/api/build/remote", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/build/remote/test", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/build/remote/install-toolchain", BotRoutePolicy::UserOrAgm),
    ("GET", "/api/judge/settings", BotRoutePolicy::UserOrAgm),
    ("PUT", "/api/judge/settings", BotRoutePolicy::UserOrAgm),
    ("GET", "/api/judge/shadow", BotRoutePolicy::UserOrAgm),
    ("GET", "/api/claude-api/settings", BotRoutePolicy::UserOrAgm),
    ("PUT", "/api/claude-api/settings", BotRoutePolicy::UserOrAgm),
    ("GET", "/api/claude-api/ledger", BotRoutePolicy::UserOrAgm),
    ("GET", "/api/claude-api/balance", BotRoutePolicy::UserOnly),
    ("PUT", "/api/claude-api/balance-anchor", BotRoutePolicy::UserOnly),
    ("GET", "/api/claude-api/cap", BotRoutePolicy::UserOnly),
    ("PUT", "/api/claude-api/cap", BotRoutePolicy::UserOnly),
    ("GET", "/api/claude-update/review", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/claude-update/review", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/credential/rotate", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/restart-idle", BotRoutePolicy::UserOrAgm),
    ("PATCH", "/api/bots/{id}", BotRoutePolicy::UserOrAgm),
    ("DELETE", "/api/bots/{id}", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/start", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/restart", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/fork", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/promote", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/stop", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/rewind", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/interrupt", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/login", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/compact", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/keep-warm/skip", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/abort", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/pane/move-to-tab", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/restore", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/bots/{id}/preview", BotRoutePolicy::UserOrAgm),
    ("DELETE", "/api/bots/{id}/preview", BotRoutePolicy::UserOrAgm),
    // #989：網頁專用的寫入——撤回／放棄回合。一般 Bot 在自己的專案裡也不能打。
    ("POST", "/api/turns/{id}/withdraw", BotRoutePolicy::UserOnly),
    ("POST", "/api/turns/{id}/abandon", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/projects", BotRoutePolicy::UserOrAgm),
    ("PATCH", "/api/projects/{id}", BotRoutePolicy::UserOrAgm),
    ("DELETE", "/api/projects/{id}", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/projects/{id}/bots", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/projects/{id}/github/refresh", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/projects/{id}/git/commit", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/projects/{id}/git/push", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/projects/{id}/git/pull", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/order", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/hosts", BotRoutePolicy::UserOrAgm),
    ("DELETE", "/api/hosts/{name}", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/hosts/{name}/reconnect", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/hosts/{name}/tools/refresh", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/hosts/{name}/tools/install", BotRoutePolicy::UserOrAgm),
    ("GET", "/api/hosts/{name}/baseline/plan", BotRoutePolicy::UserOnly),
    ("POST", "/api/hosts/{name}/baseline/apply", BotRoutePolicy::UserOnly),
    ("POST", "/api/hosts/{name}/baseline/restore", BotRoutePolicy::UserOnly),
    ("POST", "/api/hosts/{name}/identities/{identity}/login", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/hosts/{name}/identities/{identity}/logout", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/hosts/{name}/agy/logout", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/hosts/{name}/agy/install", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/hosts/{name}/gh/login", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/hosts/{name}/gh/cancel", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/identities", BotRoutePolicy::UserOrAgm),
    ("DELETE", "/api/identities/{name}", BotRoutePolicy::UserOrAgm),
    ("PUT", "/api/identities/{name}/disabled", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/mem/processes/kill", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/quota/reset-credit/use", BotRoutePolicy::UserOnly),
    ("POST", "/api/quota/probe", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/release-triage/dispatched", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/release-triage/auto", BotRoutePolicy::UserOrAgm),
    ("POST", "/api/release-triage/publish", BotRoutePolicy::UserOrAgm),
];

fn bot_route_policy(method: &str, uri: &axum::http::Uri) -> Option<BotRoutePolicy> {
    if bot_query_route_requires_user(method, uri) {
        return Some(BotRoutePolicy::UserOrAgm);
    }
    let method = if method == "HEAD" { "GET" } else { method };
    BOT_ROUTE_POLICIES
        .iter()
        .find(|(expected_method, pattern, _)| *expected_method == method && route_path_matches(pattern, uri.path()))
        .map(|(_, _, policy)| *policy)
}

fn route_path_matches(pattern: &str, path: &str) -> bool {
    let expected: Vec<_> = pattern.split('/').collect();
    let Some(actual): Option<Vec<_>> = path.split('/').map(decode_api_path_segment).collect() else {
        return false;
    };
    expected.len() == actual.len()
        && expected.iter().zip(&actual).all(|(expected, actual)| {
            (expected.starts_with('{') && expected.ends_with('}') && !actual.is_empty())
                || *expected == actual
        })
}

fn bot_query_route_requires_user(method: &str, uri: &axum::http::Uri) -> bool {
    let method = if method == "HEAD" { "GET" } else { method };
    if method != "GET" || !matches!(uri.path(), "/api/quota" | "/api/models") {
        return false;
    }
    Query::<HashMap<String, String>>::try_from_uri(uri)
        .ok()
        .is_some_and(|query| flag(&query.0.get("refresh").cloned()))
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or bad API credential"}))).into_response()
}

async fn startup_readiness(State(app): State<Arc<App>>, req: axum::extract::Request, next: Next) -> Response {
    if !app.is_startup_ready() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            Json(json!({"error": "starting", "message": "daemon startup is still in progress"})),
        )
            .into_response();
    }
    next.run(req).await
}

async fn auth(State(app): State<Arc<App>>, mut req: axum::extract::Request, next: Next) -> Response {
    let headers = req.headers().clone();
    if !origin_is_local(&headers, app.port, app.allow_lan()) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "bad origin"}))).into_response();
    }
    let has_bot = headers.contains_key("X-AM-Bot-Id") || headers.contains_key("X-AM-Bot-Token");
    let has_service = headers.contains_key("X-AM-Service-Id") || headers.contains_key("X-AM-Service-Token");
    let has_ui = headers.contains_key("X-AM-Token");
    let has_child = headers.contains_key("X-AM-Child-Spawn-Id") || headers.contains_key("X-AM-Child-Token");
    let principal = if has_child {
        if has_bot || has_service || has_ui { return unauthorized(); }
        let id = headers.get("X-AM-Child-Spawn-Id").and_then(|v| v.to_str().ok()).unwrap_or("");
        let token = headers.get("X-AM-Child-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
        let child = match crate::child_credentials::authenticate(&app, id, token).await {
            Ok(child) => child,
            Err(StatusCode::CONFLICT) => return (StatusCode::CONFLICT, Json(json!({"error":"child_not_registered"}))).into_response(),
            Err(status) => return status.into_response(),
        };
        let path = req.extensions().get::<OriginalUri>().map(|uri| uri.0.path()).unwrap_or_else(|| req.uri().path());
        let is_read = matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD);
        let allowed_prompt = *req.method() == axum::http::Method::POST && decoded_bot_path_segments(path).is_some_and(|p| p == vec!["api", "bots", child.parent_id.as_str(), "prompt"]);
        if !is_read && !allowed_prompt && !(*req.method() == axum::http::Method::POST && path == "/api/pane-authorize") { return (StatusCode::FORBIDDEN, Json(json!({"error":"child_scope_denied"}))).into_response(); }
        RequestPrincipal::Child { child_id: child.child_id, parent_bot_id: child.parent_id }
    } else if has_bot || has_service {
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
            // 分享用的受限 bot：它的 token 只能打自己的 hook，`/api` 一條都不給（SPEC「分享 bot」）。
            if crate::share::refuses_bot_principal(&app.db, &bot.id).await {
                return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden", "reason": "restricted_bot"}))).into_response();
            }
            RequestPrincipal::Bot(bot.id)
        }
    } else {
        let tok = headers.get("X-AM-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
        if !has_ui || !ct_eq(tok, &app.ui_token) {
            return unauthorized();
        }
        RequestPrincipal::User
    };
    if let RequestPrincipal::Bot(bot_id) | RequestPrincipal::Child { child_id: bot_id, .. } = &principal {
        let uri = req.extensions().get::<OriginalUri>().map(|uri| &uri.0).unwrap_or_else(|| req.uri());
        let method = req.method().as_str();
        let path = uri.path();
        let policy = bot_route_policy(method, uri);
        let is_agm_role = if policy == Some(BotRoutePolicy::UserOrAgm) && matches!(&principal, RequestPrincipal::Bot(_)) {
            match crate::supervisor::roles::role_of_bot(&app.db, bot_id).await {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(error) => {
                    tracing::error!(bot_id = %bot_id, error = ?error, "could not resolve AGM role for Bot route policy");
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
            }
        } else { false };
        // 專家模式的 manager 可以啟動／重啟它管的 bot（角色改了要重啟才套用；使用者 2026-10-31）。
        // 管理者刪成員不能帶 `?confirm=`（那是給 AGM／使用者確認刪 AGM 的 bot 用的），免得繞過 supervisor_owned 守衛。
        // 判斷要跟 handler 用同一個解碼器（`Query<DeleteQuery>`）：原始字串比對會被 `%63onfirm` 這類編碼繞過（review L2 #1）。
        // 解碼失敗就當成有帶（fail closed）。
        let confirm_query = method == "DELETE" && axum::extract::Query::<DeleteQuery>::try_from_uri(uri).map_or(true, |q| q.0.confirm.is_some());
        let manager_lifecycle = policy == Some(BotRoutePolicy::UserOrAgm) && !is_agm_role && !confirm_query && manager_may_start(&app, bot_id, method, path).await;
        // 派工者撤回自己派出、還沒送的那一則：路由仍是 UserOnly，只對這個情境放行（見 withdraw_turn_route）。
        let sender_withdraw = method == "POST" && decoded_bot_path_segments(path).is_some_and(|p| p.len() == 4 && p[0] == "api" && p[1] == "turns" && p[3] == "withdraw")
            && lifecycle::sender_may_withdraw(&app, &decoded_bot_path_segments(path).unwrap()[2], bot_id).await.unwrap_or(false);
        let manager_base = policy == Some(BotRoutePolicy::UserOnly) && matches!(&principal, RequestPrincipal::Bot(_)) && manager_base_snapshot(&app, bot_id, path).await;
        if (policy == Some(BotRoutePolicy::UserOnly) && !sender_withdraw && !manager_base) || (policy == Some(BotRoutePolicy::UserOrAgm) && !is_agm_role && !manager_lifecycle) {
            return bot_user_only().into_response();
        }
        if let Err(e) = authorize_bot_path(&app, bot_id, method, path).await {
            return e.into_response();
        }
        // 分享用 bot（SPEC §20）不給 AGM 刪、停（含刪它所在的專案）：只有使用者自己能。
        // 守衛要看解碼後的路徑（#992）：範圍檢查與 handler 拿到的都是解碼後的 id，`%30…` 這種編碼不能讓守衛查不到。
        let Some(decoded) = decoded_bot_path_segments(path) else { return bot_scope_denied(bot_id).into_response() };
        let decoded_path = format!("/{}", decoded.join("/"));
        if let Some(target) = crate::share::guards_from_bot_principal(&app.db, method, &decoded_path).await {
            return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden", "reason": "share_bot_protected", "bot_id": target}))).into_response();
        }
    }
    req.extensions_mut().insert(principal.clone());
    // 會改東西的請求記下是誰發的：config.toml 的寫入 log 與刪除 intent 要引用（issue #406）。
    if matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS) {
        return next.run(req).await;
    }
    // 只有 hook token 對得上才算「驗過的 bot」；`X-AM-Caller` 一律只當自稱記（review d77434c0 #3）。
    let verified = match &principal {
        RequestPrincipal::Child { child_id, .. } => Some(child_id.clone()),
        RequestPrincipal::Bot(_) => verified_caller_bot(&app, &headers).await,
        _ => None,
    };
    let service = match &principal { RequestPrincipal::Service(id) => Some(id.as_str()), _ => None };
    let caller = crate::config_audit::describe_request(&req, verified.as_deref(), service);
    crate::config_audit::HTTP_CALLER.scope(caller, next.run(req)).await
}

fn bot_user_only() -> LcError {
    LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"}))
}

fn bot_scope_denied(bot_id: &str) -> LcError {
    LcError::Forbidden(json!({"error": "forbidden", "reason": "bot_resource_scope", "bot_id": bot_id}))
}

async fn bot_descends_from_or_is(app: &Arc<App>, caller: &str, target: &str) -> Result<bool, LcError> {
    let found: i64 = sqlx::query_scalar(
        "WITH RECURSIVE owned(id) AS (
             SELECT id FROM bots WHERE id = ?
             UNION
             SELECT child.id FROM bots child JOIN owned parent ON child.parent_bot_id = parent.id
         )
         SELECT EXISTS(SELECT 1 FROM owned WHERE id = ?)",
    )
    .bind(caller)
    .bind(target)
    .fetch_one(&app.db)
    .await
    .map_err(any_err)?;
    Ok(found != 0)
}

async fn project_in_bot_tree(app: &Arc<App>, caller: &str, project_id: &str) -> Result<bool, LcError> {
    let found: i64 = sqlx::query_scalar(
        "WITH RECURSIVE owned(id) AS (
             SELECT id FROM bots WHERE id = ?
             UNION
             SELECT child.id FROM bots child JOIN owned parent ON child.parent_bot_id = parent.id
         )
         SELECT EXISTS(
             SELECT 1 FROM bots b JOIN owned ON owned.id = b.id WHERE b.project_id = ?
         )",
    )
    .bind(caller)
    .bind(project_id)
    .fetch_one(&app.db)
    .await
    .map_err(any_err)?;
    Ok(found != 0)
}

async fn bot_role_may_cross(app: &Arc<App>, bot_id: &str) -> Result<bool, LcError> {
    crate::supervisor::roles::role_of_bot(&app.db, bot_id)
        .await
        .map(|role| role.is_some())
        .map_err(any_err)
}

async fn bot_may_access_bot(app: &Arc<App>, caller: &str, target: &str) -> Result<bool, LcError> {
    bot_descends_from_or_is(app, caller, target).await
}

/// 路徑每一段都做 `%XX` 解碼（空段略過）；解不開回 `None`。範圍檢查與分享守衛共用，兩邊看到的 id 必須一樣（#992）。
fn decoded_bot_path_segments(path: &str) -> Option<Vec<String>> {
    path.split('/').filter(|part| !part.is_empty()).map(decode_api_path_segment).collect()
}

fn decode_api_path_segment(segment: &str) -> Option<String> {
    fn hex(byte: u8) -> Option<u8> {
        (byte as char).to_digit(16).map(|digit| digit as u8)
    }
    let input = segment.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let (Some(high), Some(low)) = (input.get(i + 1).and_then(|b| hex(*b)), input.get(i + 2).and_then(|b| hex(*b))) else {
                return None;
            };
            decoded.push(high * 16 + low);
            i += 3;
        } else {
            decoded.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Enforce ownership on routes whose path identifier resolves to a bot, project, turn,
/// attachment, or assignment. User and service principals retain their existing route
/// behavior; AGM role bots keep their explicitly delegated cross-resource scope.
async fn authorize_bot_path(app: &Arc<App>, caller: &str, method: &str, path: &str) -> Result<(), LcError> {
    let decoded = decoded_bot_path_segments(path).ok_or_else(|| bot_scope_denied(caller))?;
    let parts: Vec<&str> = decoded.iter().map(String::as_str).collect();
    if parts.first() != Some(&"api") {
        return Ok(());
    }

    let target = match (parts.get(1).copied(), parts.get(2).copied()) {
        (Some("bots"), Some(id)) if !matches!(id, "deleted" | "restart-idle") => {
            // Sending a prompt is the intentional cross-bot route. `prompt_bot` verifies the
            // sender's Bot proof and relay metadata before it can write to the recipient.
            if method == "POST" && parts.get(3) == Some(&"prompt") {
                return Ok(());
            }
            // 專家管理者不能刪自己（樹狀存取規則會讓 bot 碰到自己，這裡先擋）；AGM 角色照舊能自刪。
            if method == "DELETE" && parts.len() == 3 && id == caller
                && crate::supervisor::roles::role_of_bot(&app.db, caller).await.map_err(any_err)?.is_none()
            {
                return Err(bot_scope_denied(caller));
            }
            if let Some(target_bot) = db::bot(&app.db, id).await.map_err(any_err)? {
                if bot_may_access_bot(app, caller, id).await? {
                    return Ok(());
                }
                if matches!(method, "GET" | "HEAD") && expert_manager_over(app, caller, &target_bot).await? {
                    return Ok(());
                }
                // 專家模式的管理者可以替同專案的頂層 bot 設執行者／顧問角色（使用者 2026-10-31）；handler 再驗一次並只收 role。
                if method == "PUT" && parts.len() == 4 && parts[3] == "role" && expert_manager_over(app, caller, &target_bot).await? {
                    return Ok(());
                }
                if parts.len() == 4 && parts[3] == "base-snapshot" && expert_manager_over(app, caller, &target_bot).await? {
                    return Ok(());
                }
                if manager_lifecycle_op(method, &parts, &target_bot) && expert_manager_over(app, caller, &target_bot).await? {
                    return Ok(());
                }
                if crate::runners::relay_deputy::may_restart(app, caller, method, &parts, &target_bot).await {
                    return Ok(());
                }
                // 巡檢（patrol）可以重啟／啟動任何頂層 bot（使用者 2026-10-31）；child 與其他操作仍限自己的樹。
                if method == "POST"
                    && parts.len() == 4
                    && matches!(parts[3], "restart" | "start")
                    && target_bot.parent_bot_id.is_none()
                    && target_bot.deleted_at.is_none()
                    && crate::supervisor::roles::role_of_bot(&app.db, caller).await.map_err(any_err)?
                        == Some(crate::supervisor::roles::Role::Patrol)
                {
                    return Ok(());
                }
                Some("bot")
            } else {
                None
            }
        }
        (Some("projects"), Some(id)) => {
            if db::project(&app.db, id).await.map_err(any_err)?.is_some() {
                if project_in_bot_tree(app, caller, id).await? { return Ok(()); }
                Some("project")
            } else {
                None
            }
        }
        (Some("turns"), Some(id)) => {
            let owner: Option<String> = sqlx::query_scalar(
                "SELECT c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id = ?",
            )
            .bind(id)
            .fetch_optional(&app.db)
            .await
            .map_err(any_err)?;
            if let Some(owner) = owner {
                if bot_may_access_bot(app, caller, &owner).await? {
                    return Ok(());
                }
                if matches!(method, "GET" | "HEAD") {
                    if let Some(owner_bot) = db::bot(&app.db, &owner).await.map_err(any_err)? {
                        if expert_manager_over(app, caller, &owner_bot).await? {
                            return Ok(());
                        }
                    }
                }
                // 派工者撤回自己派出、還在佇列的那一則（見 withdraw_turn_route）。
                if method == "POST" && parts.len() == 4 && parts[3] == "withdraw" && lifecycle::sender_may_withdraw(app, id, caller).await? {
                    return Ok(());
                }
                Some("turn")
            } else {
                None
            }
        }
        (Some("attachments"), Some(id)) => {
            let owner: Option<String> = sqlx::query_scalar("SELECT bot_id FROM attachments WHERE id = ?")
                .bind(id)
                .fetch_optional(&app.db)
                .await
                .map_err(any_err)?;
            if let Some(owner) = owner {
                if bot_may_access_bot(app, caller, &owner).await? {
                    return Ok(());
                }
                if matches!(method, "GET" | "HEAD") {
                    if let Some(owner_bot) = db::bot(&app.db, &owner).await.map_err(any_err)? {
                        if expert_manager_over(app, caller, &owner_bot).await? {
                            return Ok(());
                        }
                    }
                }
                Some("attachment")
            } else {
                None
            }
        }
        (Some("supervisor"), Some("assignments")) if parts.len() >= 4 => {
            let id = parts[3];
            let owner: Option<String> = sqlx::query_scalar("SELECT target_bot_id FROM supervisor_assignments WHERE id = ?")
                .bind(id)
                .fetch_optional(&app.db)
                .await
                .map_err(any_err)?;
            if let Some(owner) = owner {
                let explicit_supervisor_route = (parts.len() == 4 && method == "GET")
                    || (parts.len() == 5 && parts.get(4) == Some(&"review") && method == "POST");
                if bot_may_access_bot(app, caller, &owner).await?
                    || (explicit_supervisor_route && bot_role_may_cross(app, caller).await?)
                {
                    return Ok(());
                }
                Some("assignment")
            } else {
                None
            }
        }
        _ => None,
    };
    match target {
        Some(resource) => Err(LcError::Forbidden(json!({
            "error": "forbidden",
            "reason": "bot_resource_scope",
            "resource": resource,
            "bot_id": caller,
        }))),
        None => Ok(()),
    }
}

async fn get_session(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    // This route bootstraps the shared UI token, so it intentionally has no UI-token auth layer.
    // Bot and Service credentials must not turn that exception into a way to obtain the UI token.
    if ["X-AM-Bot-Id", "X-AM-Bot-Token", "X-AM-Child-Spawn-Id", "X-AM-Child-Token", "X-AM-Service-Id", "X-AM-Service-Token"]
        .iter()
        .any(|name| headers.contains_key(*name))
    {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden", "reason": "user_only"}))).into_response();
    }
    if !peer_is_local(&peer, app.allow_lan()) || !origin_is_local(&headers, app.port, app.allow_lan()) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "non-local request"}))).into_response();
    }
    Json(json!({"token": app.ui_token, "port": app.port})).into_response()
}

#[cfg(test)]
mod session_principal_tests;

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
async fn hosts_list(app: &Arc<App>, cfg: &crate::config::ConfigFile) -> Vec<Value> {
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
            "shared_session": crate::shared_host::flagged_in(cfg, &c.name),
            "connected": connected,
            "error": c.error_string().await,
            // 遠端斷線起點；連著／本機為 null（UI 的離線警示條算「離線多久」）。
            "disconnected_since": c.disconnected_since(),
            "attach_command": crate::config::attach_command(c.cfg.as_ref(), &app.herdr_session),
            "tools": t.map(|x| crate::quota_agy::tools_json(&c.name, &x.tools)),
            // Login state *on this host*, `[[identities]]` + this host's `ccN` aliases (SPEC §16).
            "identities": t.map(|x| crate::login_prompt::identities_json(app, &c.name, &x.identities)),
            // Env unexpanded, as written.
            "shell_identities": t.map(|x| json!(x.shell_identities)),
            "tools_checked_at": t.map(|x| x.checked_at.clone()),
            // 工作環境一致性（只讀檢查，#719）：`{issues: null|[{id,severity,message}], checked_at}`；沒量過＝null。
            "baseline": baselines.get(&c.name).map(|b| json!(b.view(chrono::Utc::now()))),
            // herdr 版本（server／protocol 來自 ping，只在連著時報；CLI 來自探測），SPEC §11.6。
            "herdr": crate::herdr_version::for_host(&c, connected, t),
        }));
    }
    out
}

pub async fn state_json(app: &Arc<App>) -> Result<Value, LcError> {
    // 整份設定一次請求只取一份（#1663）：hosts 逐台讀會是 N 次整份 clone。
    let cfg = app.cfg.get().await;
    let connected = app.connected.load(Ordering::SeqCst);
    let projects = db::live_projects(&app.db).await.map_err(any_err)?;
    let bots = db::live_bots(&app.db).await.map_err(any_err)?;
    let unread = crate::read_marks::unread_counts(&app.db).await.map_err(any_err)?;
    let read_marks = crate::read_marks::marks(&app.db).await.map_err(any_err)?;
    // §6.11：AGM 因為閒置收起來的那些。一次讀完，免得每顆 bot 再問一次資料庫。
    let asleep = crate::supervisor::idle_sleep::all_asleep(app).await;
    let previews = crate::preview::state_map(&app.db).await.map_err(any_err)?;
    let share_profiles = crate::share::store::profiles(&app.db).await.map_err(any_err)?;
    let sharing = crate::share::store::shared_ids(&app.db).await.map_err(any_err)?;
    // 每顆 bot 的 run 與排隊中的回合各一次讀完：逐顆查是 N+1（34 顆 bot 約 30 ms，隨 bot 數線性長）。
    let mut runs = db::active_runs_by_bot(&app.db).await.map_err(any_err)?;
    let mut queued_turns = db::queued_turns_by_bot(&app.db).await.map_err(any_err)?;
    // 快取倒數（`cache_clock`）：每顆 bot 最近一筆送出去的回合，一次讀完。
    let last_turns = crate::cache_clock::last_turns_by_bot(&app.db).await.map_err(any_err)?;
    // 被排程綁著的 bot（沒有未刪除的排程＝不在內）：側欄把沒設角色的 bot 歸到「排程 bot」區用。
    let scheduled_bots: std::collections::HashSet<String> = sqlx::query_scalar("SELECT DISTINCT bot_id FROM schedules WHERE deleted_at IS NULL").fetch_all(&app.db).await.map_err(any_err)?.into_iter().collect();
    // 建立時帶的冪等鍵（只在 config.toml，不在 DB）：前端用它把「建立中」佔位列對到真 bot，不靠名字（#1796）。
    let create_request_ids: std::collections::HashMap<&str, &str> = cfg
        .projects
        .iter()
        .flat_map(|p| p.bots.iter())
        .filter_map(|c| Some((c.id.as_deref()?, c.create_request_id.as_deref()?)))
        .collect();
    // 撞限（#1899）：整份快照一次批次算完（查詢次數不隨 bot 數增加），逐顆只取值。
    let limit_hits: HashMap<String, crate::quota::LimitHit> = bots
        .iter()
        .zip(crate::runners::quota::limit_hits_for_bots(app, &bots).await)
        .filter_map(|(b, hit)| hit.map(|hit| (b.id.clone(), hit)))
        .collect();
    let stale_hosts = app.remote_shim_stale.lock().await.clone();
    // B 期：每顆 bot 在 run 啟動時記下的生效政策指紋（NULL＝沒記，不誤報待套用）。
    let policy_revs: std::collections::HashMap<String, String> = sqlx::query_as::<_, (String, String)>(
        "SELECT bot_id, policy_rev FROM runs WHERE state IN ('starting','running','stopping') AND policy_rev IS NOT NULL",
    )
    .fetch_all(&app.db)
    .await
    .map_err(any_err)?
    .into_iter()
    .collect();
    let mut out = Vec::new();
    for p in projects {
        let mut bl = Vec::new();
        let role_policy = p.role_policy();
        // 專家模式群集（同設定的執行者）：用這份快照算，不另外查 DB。
        let cluster_names = if p.mode == "expert" { crate::expert_cluster::names_by_bot(&crate::expert_cluster::clusters_of(&bots, &runs, &p.id)) } else { Default::default() };
        for b in bots.iter().filter(|b| b.project_id == p.id) {
            let run = runs.remove(&b.id);
            let policy_drift = policy_revs.get(&b.id).is_some_and(|recorded| {
                crate::lifecycle::runtime_overlay::policy_rev_now(b, &role_policy) != *recorded
            });
            let queued_turn = queued_turns.remove(&b.id);
            let shim_stale = if p.host == LOCAL_HOST {
                let path = app.data_dir.join("bots").join(&b.id).join("bin/herdr");
                (run.is_some() || path.exists()) && std::fs::read_to_string(path).ok().as_deref() != Some(crate::herdr_shim::SHIM_SH)
            } else { stale_hosts.contains_key(&p.host) };
            // host 就是這個專案的 host（`db::bot_host` 的 JOIN 同一個欄位），不必再讀 bot／bot_host 兩次。
            let bot_connected = app.bot_connected_on(b, &p.host).await;
            let mut run_v = crate::background_jobs::run_json(app, &run, run.as_ref().map(|r| r.id.as_str()));
            crate::cache_clock::annotate(&mut run_v, &b.kind, last_turns.get(&b.id));
            crate::send_now_capability::annotate(&mut run_v, &b.kind, run.as_ref(), cfg.codex.instant_interrupt);
            let limit_hit = crate::state::limit_hit_value(b, limit_hits.get(&b.id));
            let mut bot_v = json!({
                "id": b.id,
                "project_id": b.project_id,
                // Legacy child nicknames may contain a redundant generated project-agent prefix.
                // Keep the stored value intact, but project the concise name through shared state.
                "name": if b.managed_by == "child" { crate::config::short_child_name(&p.label, &b.name) } else { b.name.clone() },
                "kind": b.kind,
                "role": b.role,
                "runtime_role_policy": run.as_ref().and_then(|r| r.runtime_role_policy.as_deref()),
                "daemon_url_pending": p.host != LOCAL_HOST && run.as_ref().is_some_and(|r| r.daemon_url_injected == 0),
                "role_pending": shim_stale || policy_drift || (p.host != LOCAL_HOST && run.as_ref().is_some_and(|r| r.daemon_url_injected == 0)) || run.as_ref().is_some_and(|r| role_policy_drifted(r.runtime_role_policy.as_deref(), &p.id, &p.host, &p.mode, b.role.as_deref())),
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
                "create_request_id": create_request_ids.get(b.id.as_str()),
                // 使用者釘的主要 bot（純顯示）。
                "primary": b.is_primary == 1,
                // 頂層主力撞限中才有值（#1899，唯讀；判斷在 daemon）。child 與沒撞限一律 null。
                "limit_hit": limit_hit,
                // 主力那列的固定順序（#344）：1 起算，0＝沒排過；取消釘選不清。
                "primary_position": b.primary_position,
                // 專家模式保護額度開關 A：可被管理者使用（預設 true）
                "manager_use": b.manager_use == 1,
                // 專家模式專長軌道（role=specialist 時必填）；執行期覆蓋（B 期用，NULL＝跟角色）。
                "track": b.track,
                "runtime_override": b.runtime_override_json.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()),
                // 執行中的 CLI 載入的啟動設定跟現在存的不同＝要重啟（#353）：從資料算，PATCH 回應掉了也看得到。
                "needs_restart": run.as_ref().is_some_and(|r| crate::launch_rev::is_stale(b, r)) && !crate::lifecycle::is_deferred(&b.id),
                "live_apply_deferred": crate::lifecycle::is_deferred(&b.id),
                "cwd": b.cwd,
                // 分享用 bot（SPEC §20）：`restricted`／`trusted`；一般 bot＝null。
                "share_profile": share_profiles.get(&b.id),
                // 分享連結開著（`bot_shares` 有一列）；變了發 `bot_share_changed`。
                "share_enabled": sharing.contains(&b.id),
                "agent_name": run.as_ref().and_then(|r| r.agent_name.clone()).unwrap_or_else(|| crate::config::agent_name(&p.label, &b.id)),
                // #714：`run.background_jobs`＝回合結束後畫面上還標著的背景工作數（記憶體裡的，不在 DB）。
                // `run.last_api_at`／`run.cache_ttl_secs`：快取倒數（`cache_clock`）。
                "run": run_v,
                // §6.11：停著是因為 AGM 收起來省 RAM，不是壞掉也不是使用者關的；下次要用會自動
                // 用 `--resume` 叫醒。`null` = 不是這種停。
                // 預覽模式（§6.12）：`{status, port}`；沒開（或 off）是 `null`。
                "preview": previews.get(&b.id),
                "asleep": asleep.get(&b.id).map(|(at, mins)| json!({"since": at, "idle_minutes": mins})),
                "queued_turn": queued_turn,
                "lamp": lamp(bot_connected, run.as_ref()),
                // 跨裝置共用的未讀回合數與已讀標記（read_marks.rs）。
                "unread": unread.get(&b.id).copied().unwrap_or(0),
                "read_mark": read_marks.get(&b.id).map(crate::read_marks::json_value),
            });
            // 專家模式群集名（同專案同設定執行者 ≥2 顆）；不在群集內＝null（json! 巨集已到遞迴上限，所以在外面補）。
            bot_v["cluster"] = json!(cluster_names.get(&b.id));
            bot_v["scheduled"] = json!(scheduled_bots.contains(&b.id));
            bl.push(bot_v);
        }
        out.push(json!({
            "id": p.id, "path": p.path, "label": p.label, "host": p.host, "mode": p.mode,
            "workspace_id": p.workspace_id,
            "handed_off_to": p.handed_off_to,
            "role_policy": p.role_policy(),
            "deputy": p.deputy_bot_id.as_ref().map(|bot_id| json!({"bot_id": bot_id, "after_sec": p.deputy_after_sec})),
            "specialist_dispatch": p.specialist_dispatch,
            "github": crate::github::cached(app, &p.id).await,
            "bots": bl,
        }));
    }
    Ok(json!({
        "daemon_seq": app.current_seq(),
        // 現在有沒有一批一鍵重啟在跑（issue #492）：進度只走 WS，`bots_restart_done` 收不到時前端要有地方對帳。
        "restart_batch": crate::runners::bulk_restart::running_batch(&app.data_dir),
        // 還沒收尾的 codex 升級（同一個理由：`cli_update_done` 收不到時的對帳來源）；存在 DB，daemon 重啟後也還在（#564）。
        "cli_updates": crate::cli_update::running_list(app).await,
        "codex_reset": app.codex_resets.lock().await.snapshot.clone(),
        "herdr_updates": crate::herdr_upgrade::running_list(app),
        // 等換版窗口超過 3 分鐘、已通知使用者的那一次部署（SPEC §18.10）；WS `deploy_wait` 收不到時的對帳來源。
        "deploy_wait": crate::deploy_wait::view(app),
        "connected": connected,
        "default_connected": app.default_connected.load(Ordering::SeqCst),
        "herdr_session": app.herdr_session,
        "hosts": hosts_list(app, &cfg).await,
        "identities": cfg.identities,
        "projects": out,
    }))
}

async fn get_state(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    match principal {
        RequestPrincipal::User => {
            let mut state = state_json(&app).await?;
            // #719 階段 3a：`[host_baseline.sync]` 對不上主機的鍵，只給使用者看（bot 的視圖從 `state_json` 出，不帶這欄）。
            let cfg = app.cfg.get().await;
            state["host_baseline_orphan_sync_keys"] = json!(cfg.host_baseline_orphan_sync_keys());
            // #719 階段 3b：每台主機實際生效的模式（只會是 "off"／"ask"，`auto` 當 "ask"），網頁據此決定畫不畫「補齊」鈕。同樣只給使用者。
            let sync: serde_json::Map<String, Value> = std::iter::once(crate::config::LOCAL_HOST)
                .chain(cfg.hosts.iter().map(|h| h.name.as_str()))
                .map(|name| {
                    let mode = match cfg.host_baseline_sync(name) {
                        crate::config::HostBaselineSyncMode::Ask => "ask",
                        _ => "off",
                    };
                    (name.to_string(), json!(mode))
                })
                .collect();
            state["host_baseline_sync"] = Value::Object(sync);
            Ok(Json(state))
        }
        RequestPrincipal::Bot(bot_id) | RequestPrincipal::Child { child_id: bot_id, .. } => Ok(Json(crate::bot_state::view_for_bot(&app, &bot_id).await?)),
        RequestPrincipal::Service(_) => Err(bot_user_only()),
    }
}

/// issue #73 reopen：daemon 裡所有寫 config.toml 的 mutation（Project／Bot／identity）都走
/// `projection::update_and_project`——套用、純驗證、DB-backed 大量軟刪閘門都在**寫入 TOML 之前**
/// 做完，取代「先 `ConfigStore::update` 落盤、再另外呼叫 `project_config` 投影」的兩段式。
/// 閘門擋下來時 config.toml **沒有**被動過，`config_written: false`：呼叫端改一下再送同一個請求就好，
/// 不必先去 config.toml 補列（跟啟動投影／背景重投那類沒有伴隨 mutation 的 `project_config` 呼叫端不同，
/// 那些仍是 `config_written: true`，見 `projection::guard_removals`）。
pub(crate) fn projection_err(e: anyhow::Error) -> LcError {
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
mod cfg_err_tests;

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

#[derive(Deserialize)]
struct MkdirIn {
    host: Option<String>,
    /// 目前瀏覽的上層目錄（可 `~`／`~/x`）；沒帶＝家目錄。
    parent: Option<String>,
    /// 新資料夾的名字：單一一段（`bot_input::check_new_dir_name`）。
    name: String,
}

/// 目錄選擇器的本機路徑：空＝家目錄；只認 `~`、`~/…` 與絕對路徑（同 `bot_input::check_project_path`）。
/// `~foo` 不是家目錄底下的東西、相對路徑會以 daemon 的工作目錄為基準，兩種都 400。
fn expand_dir_path(raw: Option<&str>, home: &std::path::Path) -> Result<String, LcError> {
    let p = raw.map(str::trim).filter(|s| !s.is_empty());
    match p {
        None | Some("~") => Ok(home.to_string_lossy().into_owned()),
        Some(p) => match p.strip_prefix("~/") {
            Some(rest) => Ok(home.join(rest).to_string_lossy().into_owned()),
            None if std::path::Path::new(p).is_absolute() => Ok(p.to_string()),
            None => Err(LcError::Bad("path must be absolute (or `~` / start with `~/`)".into())),
        },
    }
}

/// 目錄瀏覽不該列的地方（憑證、金鑰、daemon 自己的資料與各 CLI 的設定／身份目錄）。純判斷，可測。
/// 用 component 比（`.sshx` 不是 `.ssh`，`.claudeish` 不是 `.claude`）；身份目錄 `~/.claude-<名>` 整族都擋。
fn fs_dir_denied(path: &std::path::Path, home: &std::path::Path, data_dir: &std::path::Path) -> bool {
    if path.starts_with(data_dir) {
        return true;
    }
    const UNDER_HOME: [&str; 9] = [".ssh", ".gnupg", ".aws", ".kube", ".config/agents-manager", ".claude", ".codex", ".grok", ".gemini"];
    if UNDER_HOME.iter().any(|d| {
        let root = home.join(d);
        path.starts_with(&root) || std::fs::canonicalize(&root).is_ok_and(|root| path.starts_with(root))
    }) {
        return true;
    }
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(".claude-") {
                let root = entry.path();
                if path.starts_with(&root) || std::fs::canonicalize(root).is_ok_and(|root| path.starts_with(root)) {
                    return true;
                }
            }
        }
    }
    path.strip_prefix(home)
        .ok()
        .and_then(|rest| rest.components().next())
        .is_some_and(|first| first.as_os_str().to_string_lossy().starts_with(".claude-"))
}

fn local_dir_entries(path: &std::path::Path, hidden: bool) -> std::io::Result<(Vec<Value>, bool)> {
    use std::os::unix::io::AsRawFd as _;
    let components = if path == std::path::Path::new("/") {
        Vec::new()
    } else {
        crate::trusted_open::safe_relative_components(path.strip_prefix("/").map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "directory path is not absolute"))?)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "unsafe directory path"))?
    };
    let dir = crate::trusted_open::open_bound_dir(std::path::Path::new("/"), &components, None)?;
    let fd_path = std::path::PathBuf::from(format!("/dev/fd/{}", dir.as_raw_fd()));
    let mut names: Vec<(String, std::ffi::OsString, bool)> = Vec::new();
    let completed = crate::trusted_open::read_dir_bound_while(&dir, |entry| {
        let name = entry.name.to_string_lossy().to_string();
        if name.chars().any(char::is_control) || (name.starts_with('.') && !hidden) {
            return true;
        }
        let follows_to_dir = entry.is_symlink && std::fs::metadata(fd_path.join(&entry.name)).is_ok_and(|m| m.is_dir());
        if !entry.is_dir && !follows_to_dir {
            return true;
        }
        if names.len() >= crate::hosts::DIR_LIST_LIMIT * 10 {
            return false;
        }
        names.push((name, entry.name, entry.is_symlink));
        true
    })?;
    let mut truncated = !completed;
    names.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    if names.len() > crate::hosts::DIR_LIST_LIMIT {
        names.truncate(crate::hosts::DIR_LIST_LIMIT);
        truncated = true;
    }
    let out = names
        .into_iter()
        .map(|(name, entry_name, is_symlink)| {
            let has_git = !is_symlink
                && crate::trusted_open::open_dir_entry_in(&dir, &entry_name).ok().is_some_and(|child| {
                    crate::trusted_open::read_dir_bound(&child).is_ok_and(|entries| entries.iter().any(|e| e.name == std::ffi::OsStr::new(".git")))
                });
            let full = if path == std::path::Path::new("/") { format!("/{name}") } else { format!("{}/{name}", path.display()) };
            json!({"name": name, "path": full, "git": has_git})
        })
        .collect();
    Ok((out, truncated))
}

async fn list_dirs(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>, Query(q): Query<DirsQuery>) -> Result<Json<Value>, LcError> {
    // SPEC §11.5。DirPicker 專用：pane 裡的 bot（拿得到自己的 token）不能借 daemon 的身份去列本機、更不能經 daemon 的 ssh 列別台主機。
    if principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
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
    let raw = expand_dir_path(q.path.as_deref(), &home)?;
    let path = std::fs::canonicalize(&raw).map_err(|e| LcError::Bad(format!("{raw}: {e}")))?;
    // 錯誤訊息講使用者自己打的那串，不替人把符號連結解開、講出真正指到哪。
    if !path.is_dir() {
        return Err(LcError::Bad(format!("{raw} is not a directory")));
    }
    let canon_home = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
    let canon_data = std::fs::canonicalize(&app.data_dir).unwrap_or_else(|_| app.data_dir.clone());
    if fs_dir_denied(&path, &canon_home, &canon_data) {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "directory_not_browsable"})));
    }
    let (rd, truncated) = tokio::task::spawn_blocking({
        let path = path.clone();
        move || local_dir_entries(&path, hidden)
    })
    .await
    .map_err(any_err)?
    .map_err(|e| LcError::Bad(format!("{raw}: {e}")))?;
    let parent = path.parent().map(|p| p.to_string_lossy().to_string());
    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "parent": parent,
        "home": home.to_string_lossy(),
        "entries": rd,
        "truncated": truncated,
    })))
}

/// 在已經 canonicalize 的 `parent`（沿路 `O_NOFOLLOW` 逐層開，不給「檢查完才被換成 symlink」的空檔）底下新建 `name`。
/// 已存在（含同名檔案、symlink）回 `AlreadyExists`。
fn local_make_dir(parent: &std::path::Path, name: &str) -> std::io::Result<()> {
    let components = if parent == std::path::Path::new("/") {
        Vec::new()
    } else {
        crate::trusted_open::safe_relative_components(parent.strip_prefix("/").map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "directory path is not absolute"))?)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "unsafe directory path"))?
    };
    let dir = crate::trusted_open::open_bound_dir(std::path::Path::new("/"), &components, None)?;
    crate::trusted_open::make_new_dir_in(&dir, std::ffi::OsStr::new(name), 0o755).map(|_| ())
}

/// `POST /api/fs/dirs` `{host?, parent?, name}`：DirPicker 的「新資料夾」。在 `parent` 底下建 `name`，回 `{path, name, parent}`——
/// UI 直接把 `path` 當成 Project 路徑。只有 User principal；`parent`、新路徑都套 `GET /api/fs/dirs` 同一份「不給進」規則（SPEC §11.5）。
/// 名字見 `bot_input::check_new_dir_name`（400）；已經有同名的東西 → 409 `already_exists`，什麼都沒動。
async fn make_dir(State(app): State<Arc<App>>, Extension(principal): Extension<RequestPrincipal>, Json(b): Json<MkdirIn>) -> Result<Json<Value>, LcError> {
    if principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    let name = crate::bot_input::check_new_dir_name(&b.name)?;
    let host = b.host.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    let exists = |parent: &str| {
        LcError::conflict(
            "already_exists",
            json!({"name": name, "parent": parent, "message": format!("`{name}` already exists in {parent}; pick another name or choose that folder")}),
        )
    };
    if host != LOCAL_HOST {
        let conn = app.hosts.get(&host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
        let parent = b.parent.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("~");
        return match crate::hosts::remote_make_dir(&conn, parent, &name).await.map_err(|e| LcError::Upstream(format!("{e:#}")))? {
            crate::hosts::MakeDirOutcome::Created(path) => {
                let parent_path = std::path::Path::new(&path).parent().map(|p| p.to_string_lossy().to_string());
                Ok(Json(json!({"path": path, "name": name, "parent": parent_path})))
            }
            crate::hosts::MakeDirOutcome::Exists => Err(exists(parent)),
        };
    }
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
    let raw = expand_dir_path(b.parent.as_deref(), &home)?;
    let parent = std::fs::canonicalize(&raw).map_err(|e| LcError::Bad(format!("{raw}: {e}")))?;
    if !parent.is_dir() {
        return Err(LcError::Bad(format!("{raw} is not a directory")));
    }
    let canon_home = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
    let canon_data = std::fs::canonicalize(&app.data_dir).unwrap_or_else(|_| app.data_dir.clone());
    let target = parent.join(&name);
    if fs_dir_denied(&parent, &canon_home, &canon_data) || fs_dir_denied(&target, &canon_home, &canon_data) {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "directory_not_browsable"})));
    }
    let made = tokio::task::spawn_blocking({
        let (parent, name) = (parent.clone(), name.clone());
        move || local_make_dir(&parent, &name)
    })
    .await
    .map_err(any_err)?;
    match made {
        Ok(()) => Ok(Json(json!({"path": target.to_string_lossy(), "name": name, "parent": parent.to_string_lossy()}))),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(exists(&parent.to_string_lossy())),
        Err(e) => Err(LcError::Bad(format!("cannot create {}: {e}", target.display()))),
    }
}

async fn create_project(State(app): State<Arc<App>>, Json(b): Json<NewProject>) -> Result<Response, LcError> {
    let host = b.host.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    if let Some(l) = b.label.as_deref() {
        crate::bot_input::check_project_label(l.trim())?;
    }
    crate::bot_input::check_project_path(&b.path)?;
    let path = if host == LOCAL_HOST {
        let canonical = canonical_path(b.path.trim()).map_err(|e| LcError::Bad(e.to_string()))?;
        crate::bot_input::check_is_dir(&canonical)?;
        canonical
    } else {
        // SPEC §11.6.
        let conn = app.hosts.get(&host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
        crate::hosts::remote_canonical_dir(&conn, b.path.trim())
            .await
            .map_err(|e| LcError::Bad(format!("{e:#}")))?
    };
    // 驗的與存的都是 trim 過的（同 patch_project）：頭尾空白不進 config.toml，長度上限也對實際存的字串生效。
    let label = b.label.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| {
        std::path::Path::new(&path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| path.clone())
    });
    let id = db::ulid();
    let res = crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
        if cfg.projects.iter().any(|p| p.path == path && p.host == host) {
            anyhow::bail!("duplicate");
        }
        cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
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

#[derive(Deserialize, Default)]
pub(crate) struct PatchProject {
    mode: Option<String>,
    manager_bot_id: Option<String>,
    label: Option<String>,
    /// #708：字串＝移交給那台主機的 daemon，`null`＝收回，不帶＝不動。
    #[serde(default)]
    handed_off_to: Option<Value>,
    /// 專家模式角色政策（role -> policy 物件；B 期才用，現在只存、只讀）。`null`＝清掉，不帶＝不動。
    #[serde(default)]
    role_policy: Option<Value>,
    /// 專家模式副管理者：`{bot_id, after_sec?}`（`after_sec` 下限 60，預設 180）；`null`＝清掉。僅 User。
    #[serde(default, deserialize_with = "double_option_value")]
    deputy: Option<Option<Value>>,
    /// 專家派工總開關（`true`＝專家可直接派工給本專案執行者）；不帶＝不動。僅 User。關掉時當場收回排隊中失去依據的專家派工。
    #[serde(default)]
    specialist_dispatch: Option<bool>,
}

/// Never blocked by a live run: herdr identity derives from the bot id, so `needs_restart: false`.
pub(crate) async fn patch_project(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<PatchProject>,
) -> Result<Response, LcError> {
    if (b.mode.is_some() || b.manager_bot_id.is_some() || b.role_policy.is_some() || b.deputy.is_some() || b.specialist_dispatch.is_some()) && principal != RequestPrincipal::User { return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"user_only"}))); }
    let project_for_gate = if b.mode.is_some() {
        Some(db::project(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("project".into()))?)
    } else { None };
    let expert_gate = project_for_gate.as_ref().map(|p| db::expert_host_gate(&p.host));
    let _expert_guard = match &expert_gate { Some(gate) => Some(gate.lock().await), None => None };
    let label = match &b.label {
        None => None,
        Some(l) => {
            let l = l.trim();
            if l.is_empty() {
                return Err(LcError::Bad("project label must not be empty".into()));
            }
            crate::bot_input::check_project_label(l)?;
            Some(l.to_string())
        }
    };
    let handed_off_to = match &b.handed_off_to {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(h)) if !h.trim().is_empty() => Some(Some(h.trim().to_string())),
        Some(_) => return Err(LcError::Bad("handed_off_to must be a non-empty host name or null".into())),
    };
    // role_policy：key 只收 manager／executor／advisor／specialist，值一律是 JSON object；其他一律 400 `invalid_role_policy`。
    let parsed_role_policy: Option<BTreeMap<String, Value>> = match &b.role_policy {
        None => None,
        Some(Value::Object(map)) => {
            let mut out = BTreeMap::new();
            for (role, policy) in map {
                if !matches!(role.as_str(), "manager" | "executor" | "advisor" | "specialist") || !policy.is_object() {
                    return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_role_policy", "role": role})));
                }
                let normalized = crate::role_policy::validate_role(role, policy)?;
                out.insert(role.clone(), normalized);
            }
            Some(out)
        }
        Some(Value::Null) => Some(BTreeMap::new()),
        Some(_) => return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_role_policy"}))),
    };
    let parsed_deputy: Option<Option<crate::config::DeputyCfg>> = match &b.deputy {
        None => None,
        Some(raw) => {
            let d = crate::runners::relay_deputy::parse_patch(raw.as_ref().unwrap_or(&Value::Null))?;
            if let Some(d) = &d {
                let expert = match b.mode.as_deref() { Some(m) => m == "expert", None => db::project(&app.db, &id).await.ok().flatten().is_some_and(|p| p.mode == "expert") };
                crate::runners::relay_deputy::validate_target(&app, &id, expert, d).await?;
            }
            Some(d)
        }
    };
    let projection =     crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
        let p = cfg
            .projects
            .iter_mut()
            .find(|p| p.id.as_deref() == Some(id.as_str()))
            .ok_or_else(|| anyhow::anyhow!("no-project"))?;
        if let Some(manager) = &b.manager_bot_id {
            if !p.bots.iter().any(|bot| bot.id.as_deref() == Some(manager.as_str())) { anyhow::bail!("manager must be a project bot"); }
            for bot in &mut p.bots {
                if bot.id.as_deref() == Some(manager.as_str()) { bot.role = Some("manager".into()); }
                else if bot.role.as_deref() == Some("manager") { bot.role = None; }
            }
        }
        if let Some(mode) = &b.mode { p.mode = mode.clone(); }
        if let Some(l) = &label {
            p.label = l.clone();
        }
        if let Some(h) = &handed_off_to {
            p.handed_off_to = h.clone();
        }
        if let Some(rp) = &parsed_role_policy {
            p.role_policy = rp.clone();
        }
        if let Some(d) = &parsed_deputy {
            p.deputy = d.clone();
        }
        if let Some(on) = b.specialist_dispatch {
            p.specialist_dispatch = on;
        }
        Ok(())
    })
    .await;
    projection.map_err(|e| if e.to_string() == "no-project" { LcError::NotFound("project".into()) } else { projection_err(e) })?;
    // 專家派工的依據可能失效（關掉開關、離開 expert）：當場收回排隊中、還沒認領的專家派工（重驗後才收，依據仍成立的不動）。
    if b.specialist_dispatch == Some(false) || b.mode.is_some() {
        crate::specialist_dispatch::withdraw_disabled(&app, &id).await;
    }
    if let Some(h) = &handed_off_to {
        tracing::warn!(project_id = %id, handed_off_to = ?h, http = %crate::config_audit::http_caller(), "project handoff changed");
        // 收回：下一輪對帳照常接手，不等下一個 herdr 事件。
        if h.is_none() {
            if let Ok(Some(p)) = db::project(&app.db, &id).await {
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::runners::reconcile::reconcile_host(&app, &p.host).await {
                        tracing::warn!(host = %p.host, error = ?e, "reconcile after taking a project back failed");
                    }
                });
            }
        }
    }
    app.emit("project_changed", json!({"project_id": id})).await;
    let mut needs_restart = false;
    if b.mode.is_some() || b.manager_bot_id.is_some() {
        for bot in db::live_bots(&app.db).await.map_err(any_err)?.into_iter().filter(|bot| bot.project_id == id) {
            if db::active_run(&app.db, &bot.id).await.map_err(any_err)?.is_some() { needs_restart = true; break; }
        }
    }
    Ok((StatusCode::OK, Json(json!({"project_id": id, "needs_restart": needs_restart}))).into_response())
}

pub(crate) async fn soft_delete_child(app: &Arc<App>, bot_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=? AND deleted_at IS NULL").bind(db::now()).bind(bot_id).execute(&app.db).await?;
    crate::api_bot::schedule_retention_after_delete(app);
    crate::share::revoke_bot_share(app, bot_id).await
}

/// `?confirm=supervisor`：刪 AGM 的 bot／專案要明講（issue #406）。
#[derive(Deserialize, Default)]
pub(crate) struct DeleteQuery {
    pub(crate) confirm: Option<String>,
}

/// HTTP 進來的刪除先過 AGM 閘門。
pub(crate) async fn delete_project_http(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<Response, LcError> {
    crate::supervisor_owned::guard_project_delete(&app.db, &id, q.confirm.as_deref()).await?;
    delete_project(State(app), Path(id)).await
}

/// 路由用的刪除入口：中介層已依解碼後的 query 擋過一次；這裡再依 principal 驗：帶 `confirm` 只允許使用者、
/// 服務、或 AGM 角色的 Bot（review L2 #1）。非 AGM 的 Bot（含 child）帶 `confirm` 一律 403，什麼都不動。
async fn delete_bot_route(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
    principal: Option<Extension<RequestPrincipal>>,
) -> Result<Response, LcError> {
    require_confirm_principal(&app, principal.map(|Extension(p)| p), q.confirm.as_deref()).await?;
    delete_bot_http(State(app), Path(id), Query(q)).await
}

async fn delete_project_route(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
    principal: Option<Extension<RequestPrincipal>>,
) -> Result<Response, LcError> {
    require_confirm_principal(&app, principal.map(|Extension(p)| p), q.confirm.as_deref()).await?;
    delete_project_http(State(app), Path(id), Query(q)).await
}

#[cfg(test)]
pub(crate) async fn delete_bot_route_for_test(
    state: State<Arc<App>>,
    path: Path<String>,
    query: Query<DeleteQuery>,
    principal: Option<Extension<RequestPrincipal>>,
) -> Result<Response, LcError> {
    delete_bot_route(state, path, query, principal).await
}

async fn require_confirm_principal(app: &Arc<App>, principal: Option<RequestPrincipal>, confirm: Option<&str>) -> Result<(), LcError> {
    if confirm.is_none() {
        return Ok(());
    }
    let denied = || LcError::Forbidden(json!({"error": "forbidden", "reason": "confirm_not_allowed"}));
    match principal {
        None | Some(RequestPrincipal::User) | Some(RequestPrincipal::Service(_)) => Ok(()),
        Some(RequestPrincipal::Bot(bot_id)) => match crate::supervisor::roles::role_of_bot(&app.db, &bot_id).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(denied()),
            Err(error) => Err(LcError::Upstream(format!("{error:#}"))),
        },
        Some(RequestPrincipal::Child { .. }) => Err(denied()),
    }
}

pub(crate) async fn delete_bot_http(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<DeleteQuery>) -> Result<Response, LcError> {
    crate::supervisor_owned::guard_bot_delete(&app.db, &id, q.confirm.as_deref()).await?;
    delete_bot(State(app), Path(id)).await
}

async fn refuse_handed_off_project(pool: &sqlx::SqlitePool, id: &str) -> Result<(), LcError> {
    let project = db::project(pool, id)
        .await
        .map_err(any_err)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    if let Some(host) = project.handed_off_to {
        return Err(LcError::conflict("handed_off", json!({"project_id": id, "handed_off_to": host})));
    }
    Ok(())
}

async fn delete_project(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    // Handoff ownership takes precedence over deletion: the receiving daemon owns these bots and panes.
    refuse_handed_off_project(&app.db, &id).await?;
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
    // 已刪的專案再刪一次＝404，不能走到 `begin`：那會沿用並 abandon 背景還在補的 intent（#1058）。
    let project = db::project(&app.db, &id)
        .await
        .map_err(any_err)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    if let Some(host) = project.handed_off_to {
        return Err(LcError::conflict("handed_off", json!({"project_id": id, "handed_off_to": host})));
    }
    let host = project.host;
    // 授權範圍在臨界區裡從**當下的** TOML 算；TOML 裡多出沒鎖住的 bot 就拒絕。
    let held: std::collections::HashSet<String> = ids.iter().cloned().collect();
    // 持久 intent（#355 P3）：定案**之前**先 commit（payload＝當時的 bot 快照）；定案之後 daemon 死掉，開機由 `delete_intents::recover_host` 補完。
    let snapshot: Vec<Value> = in_project.iter().map(|b| json!({"id": b.id, "managed_by": b.managed_by})).collect();
    let requested_by = crate::config_audit::http_caller();
    tracing::warn!(project_id = %id, http = %requested_by, "project delete requested");
    detach(&id, finish_project_delete(app, id.clone(), host, held, ids, in_project, snapshot, requested_by, guards)).await
}

/// 定案之後的收尾（#1065）：在 `detach` 裡跑，client 斷線也會跑完；`begin` 之後的步驟都在這裡。
#[allow(clippy::too_many_arguments)]
async fn finish_project_delete(app: Arc<App>, id: String, host: String, held: std::collections::HashSet<String>, ids: Vec<String>, in_project: Vec<db::Bot>, snapshot: Vec<Value>, requested_by: String, guards: Vec<tokio::sync::OwnedMutexGuard<()>>) -> Result<Response, LcError> {
    let expert_gate = db::expert_host_gate(&host);
    let _expert_guard = expert_gate.lock().await;
    let intent = crate::delete_intents::begin(&app, "delete_project", &id, &host, &json!({"bots": snapshot, "requested_by": requested_by}))
        .await?;
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_after_intent", &id).await;
    if let Err(e) = delete_in_config(&app, crate::projection::DeleteTarget::Project { id: &id, held: &held }).await {
        crate::delete_intents::abandon(&app, &intent, "delete_in_config refused").await;
        return Err(e);
    }
    crate::api_bot::schedule_retention_after_delete(&app);
    let mut share_retry = false;
    if let Err(e) = crate::share::revoke_project_shares(&app, &id).await {
        tracing::error!(project = %id, error = %e, "project deleted but its share links could not all be revoked; retrying in the background");
        share_retry = true;
    }
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_project_after_commit", &id).await;
    // 輸入框草稿跟著專案走：群組草稿與每顆 bot 的草稿都清掉，其他瀏覽器也會收到清除事件。
    let draft_keys: Vec<String> = std::iter::once(format!("group:{id}")).chain(ids.iter().map(|b| format!("bot:{b}"))).collect();
    crate::drafts::clear_keys(&app, &draft_keys).await;
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
    let retry_failed = !failed.is_empty() || share_retry;
    if retry_failed {
        // 各路徑自己的記憶體重試 task 換成 intent 的重試（#355）：intent 留著，背景用 recovery 補完（含清目錄），daemon 死掉開機也接得回。
        crate::delete_intents::spawn_retry(app.clone(), intent.clone(), "some project cleanup could not be completed".into());
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
        return Err(any_err("project deleted, but some cleanup could not be completed yet; retrying in the background"));
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
    /// `"restricted"`＝分享用的受限 bot、`"trusted"`＝信任分享（一般 bot 的權限，SPEC §20.1）。只在建立時決定，之後不能切換。
    #[serde(default)]
    share_profile: Option<String>,
    /// 只收 `"scheduler"`（排程專屬 bot，不算執行者）；專家模式專案另收 `executor`／`advisor`，缺省＝執行者；其他值 400。非專家專案缺省＝沒有角色。
    #[serde(default)]
    role: Option<String>,
    /// 分享用 bot 的資料夾（`{"kind":"new","name"}`／`{"kind":"existing","path"}`）。受限：沒帶＝新資料夾、名字用 bot 名；
    /// 信任分享：只收既有資料夾，沒帶＝專案目錄。
    #[serde(default)]
    share_folder: Option<crate::share::folder::ShareFolderIn>,
    /// 信任分享一定要帶 `true`：拿到連結的人可以透過它操作這台機器上的任何東西（使用者 2026-10-04）。
    #[serde(default)]
    confirm_trusted: bool,
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

/// claude-api bot 沒有 CLI、沒有 pane、沒有帳號身份：本機與遠端專案都收（遠端不隔離，見 SPEC），模型只能是價目表核准的清單，
/// 其餘只對 CLI 有意義的欄位一律 400（不默默忽略，免得使用者以為生效）。
fn check_api_bot_create(b: &NewBot) -> Result<(), LcError> {
    let bad = |reason: &str| LcError::BadValue(json!({"error": "bad_request", "reason": reason}));
    if let Some(m) = b.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        if !crate::config::api_bot_model_supported(m) {
            return Err(bad("api_bot_model_unsupported"));
        }
    }
    if b.identity.as_deref().is_some_and(|s| !s.trim().is_empty())
        || !b.args.is_empty()
        || b.env.as_ref().is_some_and(|e| !e.is_empty())
        || b.share_profile.as_deref().is_some_and(|s| !s.trim().is_empty())
        || b.share_folder.is_some()
        || b.confirm_trusted
        || b.autostart
    {
        return Err(bad("api_bot_cli_fields_unsupported"));
    }
    Ok(())
}

fn remap_model(kind: &str, model: Option<&str>) -> (Option<String>, Option<Value>) {
    let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else { return (None, None) };
    match crate::models::remap_deprecated_model(kind, model) {
        Some(to) => (Some(to.to_string()), Some(json!({"model": {"from": model, "to": to}}))),
        None => (Some(model.to_string()), None),
    }
}

/// 寫 config 時 `projection::validate` 在 config 鎖裡擋下的「bot 指到不存在的身份」。檢查與寫入是同一個原子步驟，
/// 所以 `create_bot` 驗過身份之後、寫入之前被刪掉，或 `delete_identity` 查完之後、刪除之前有 bot 開始用，都不會留下孤兒身份；
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
    if !crate::config::valid_bot_kind(&b.kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::bot_kinds_list())));
    }
    // 專家模式專案的 bot 一定要有角色：沒帶就是執行者；可選執行者／顧問／排程專用（管理者、專家要另外指派）。
    // 非專家專案維持舊規則：只收 `scheduler`。分享用 bot 不是專家成員，不補預設。
    let expert_project = db::project(&app.db, &pid).await.map_err(any_err)?.is_some_and(|p| p.mode == "expert");
    let scheduler_role = match b.role.as_deref().map(str::trim) {
        None | Some("") => (expert_project && b.share_profile.as_deref().map(str::trim).is_none_or(str::is_empty)).then(|| "executor".to_string()),
        Some("scheduler") => Some("scheduler".to_string()),
        Some(r @ ("executor" | "advisor")) if expert_project => Some(r.to_string()),
        Some(_) => return Err(LcError::Bad("role on create only accepts `scheduler` (expert projects also `executor`／`advisor`)".into())),
    };
    let api_bot = b.kind == crate::config::API_BOT_KIND;
    if api_bot {
        check_api_bot_create(&b)?;
    }
    // model／env／args 最後是 CLI 的 argv 與 pane 的環境變數，形狀不對在這裡就擋（`bot_input`）。
    if let Some(m) = &b.model {
        crate::bot_input::check_model(m)?;
    }
    if let Some(env) = &b.env {
        crate::bot_input::check_env(env)?;
    }
    crate::bot_input::check_args(&b.args)?;
    // `cc1` is a different account on each host.
    let host = db::project(&app.db, &pid)
        .await
        .map_err(any_err)?
        .map(|p| p.host)
        .unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    let identity = check_identity(&app, &host, &b.identity, &b.kind).await?;
    // 分享用 bot：專案在本機，或設定裡有這台主機（不認得＝409 `unsupported_host`）。
    let host_known = host == crate::config::LOCAL_HOST || app.hosts.get(&host).await.is_some();
    let share_profile = match b.share_profile.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(crate::share::store::PROFILE_RESTRICTED) => {
            crate::share::cage::check_create(&b.kind, &host, host_known, &b.args, b.env.as_ref())?;
            Some(crate::share::store::PROFILE_RESTRICTED)
        }
        Some(crate::share::store::PROFILE_TRUSTED) => {
            if !b.confirm_trusted {
                return Err(LcError::BadValue(json!({
                    "error": "bad_request",
                    "reason": "confirm_trusted_required",
                    "message": "信任分享：拿到連結的人可以透過它操作這台機器上的任何東西，建立時要帶 confirm_trusted:true",
                })));
            }
            crate::share::cage::check_profile(&b.kind, &host, host_known)?;
            if matches!(b.share_folder, Some(crate::share::folder::ShareFolderIn::New { .. })) {
                return Err(LcError::Bad("trusted share bots use an existing folder (share_folder.kind must be `existing`)".into()));
            }
            Some(crate::share::store::PROFILE_TRUSTED)
        }
        Some(other) => return Err(LcError::Bad(format!("share_profile must be `restricted` or `trusted` (got `{other}`)"))),
    };
    if b.confirm_trusted && share_profile != Some(crate::share::store::PROFILE_TRUSTED) {
        return Err(LcError::Bad("confirm_trusted is only for share_profile `trusted`".into()));
    }
    // 受限＝關進籠子（`share::cage`）；信任分享是一般 bot 的權限，只有分享入口與「分享 bot」的保護跟受限一樣。
    let restricted = share_profile == Some(crate::share::store::PROFILE_RESTRICTED);
    let share_bot = share_profile.is_some();
    #[cfg(test)]
    crate::lifecycle::race_point::hit("create_bot_after_identity_check", &pid).await;
    let (model, remapped) = if api_bot { (Some(b.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or(crate::config::API_BOT_MODEL).to_string()), None) } else { remap_model(&b.kind, b.model.as_deref()) };
    // 受限 bot 沒指定模型：用 `/api/models` 當下列出的最新 Opus（使用者 2026-10-03：不寫死，CLI 的帳號預設會跑到舊版）。
    let model = match (restricted, model) {
        (true, None) => Some(crate::share::cage::latest_opus(&app, identity.as_deref()).await),
        (_, m) => m,
    };
    if b.share_folder.is_some() && !share_bot {
        return Err(LcError::Bad("share_folder is only for a share_profile".into()));
    }
    let share_folder = match share_profile {
        Some(crate::share::store::PROFILE_TRUSTED) => match b.share_folder.clone() {
            Some(f) => Some(f),
            None => {
                let path = db::project(&app.db, &pid).await.map_err(any_err)?.map(|p| p.path).ok_or_else(|| LcError::NotFound("project".into()))?;
                Some(crate::share::folder::ShareFolderIn::Existing { path })
            }
        },
        Some(_) => Some(b.share_folder.clone().unwrap_or(crate::share::folder::ShareFolderIn::New { name: b.name.clone() })),
        None => None,
    };
    let mut effort = crate::config::normalize_effort(&b.kind, b.effort.as_deref()).map_err(LcError::Bad)?;
    if api_bot && effort.is_none() {
        effort = Some(crate::config::API_BOT_DEFAULT_EFFORT.to_string());
    }
    let env: BTreeMap<String, String> = b.env.clone().unwrap_or_default();
    check_env_names(&env)?;
    let id = db::ulid();
    let used_name = std::sync::Mutex::new(b.name.clone());
    let create_request_id = b.client_request_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(c) = &create_request_id {
        if c.len() > 128 || !c.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_.:".contains(ch)) {
            return Err(LcError::Bad("client_request_id must be 1..=128 chars of [A-Za-z0-9-_.:]".into()));
        }
        // 這個前綴是 daemon 在 setup 角色 bot 時自己留的記號：使用者給了就等於偽造「這顆是 setup 建的」。
        if c.starts_with(crate::supervisor::bot_requests::ROLE_SETUP_MARK_PREFIX) {
            return Err(LcError::Bad(format!("client_request_id must not start with `{}` (reserved)", crate::supervisor::bot_requests::ROLE_SETUP_MARK_PREFIX)));
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
            restricted,
            share_folder,
            share_profile,
            scheduler_role,
        ])
        .to_string()
    });
    let replayed: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);
    let reused: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    // child（reconcile 認領的）不在 config.toml，但 DB 的 `(project_id, name)` 唯一約束算它們。
    // 只看 config 會先寫進檔、投影再爆，之後這個專案每次新增／修改都 502（#654）。
    let db_names = live_bot_names(&app.db, &pid).await.map_err(any_err)?;
    // 受限 bot：config 寫進去之前先記下來，投影出來的那一列從第一次啟動起就在籠子裡（沒有當一般 bot 起來的空窗）。
    let workspace = match &share_folder {
        Some(folder) => {
            // 同一個請求鍵的重送：新資料夾已經是上一次建的，照用不回 409（真正的判斷在下面 config 鎖裡）。
            let replay = match &create_request_id {
                Some(c) => app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).any(|x| x.create_request_id.as_deref() == Some(c.as_str())),
                None => false,
            };
            Some(
                crate::share::admin::reserve_share_bot_on(&app, &host, &id, share_profile.unwrap_or(crate::share::store::PROFILE_RESTRICTED), folder, replay)
                    .await?,
            )
        }
        None => None,
    };
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
        p.bots.push(crate::config::BotCfg { role: scheduler_role.clone(), manager_use: true, track: None, runtime_override: None, fork_from: None,
            id: Some(id.clone()),
            name,
            kind: b.kind.clone(),
            model: model.clone(),
            effort: effort.clone(),
            fast: b.fast,
            persona: b.persona.clone().filter(|s| !s.trim().is_empty()),
            args: b.args.clone(),
            autostart: b.autostart,
            // 分享入口靠 hook 收 bot 的回覆：分享用 bot（兩種）一律注入。
            inject_hooks: b.inject_hooks.unwrap_or(true) || share_bot,
            auto_approve: b.auto_approve.unwrap_or(true) && !restricted,
            identity: identity.clone(),
            env: env.clone(),
            herdr_session: None,
            create_request_id: create_request_id.clone(),
            create_fingerprint: create_fingerprint.clone(),
        });
        Ok(())
    })
    .await;
    if let Some((ws, created_folder)) = &workspace {
        let created = res.is_ok() && replayed.lock().unwrap().is_none();
        crate::share::admin::finish_share_bot_on(&app, &host, &id, ws, *created_folder, created).await;
        // 遠端的保留標記（本機的 `finish_restricted` 已經放了）。
        if created && host != crate::config::LOCAL_HOST {
            crate::share::keep_new_remote_share(&app, &id).await;
        }
    }
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
    #[serde(default, deserialize_with = "double_option")]
    role: Option<Option<String>>,
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
    /// 專家模式保護額度：可被管理者使用（只限 User 角色修改，預設 true）
    manager_use: Option<bool>,
    /// 專家模式專長軌道（role=specialist 時必填；只限 User 角色修改）。`null`／空字串＝清掉。
    #[serde(default, deserialize_with = "double_option")]
    track: Option<Option<String>>,
    /// 執行期覆寫（B 期，只限 User）：物件＝覆寫；`null`＝清掉（跟角色）。欄位見 `role_policy.rs`。
    #[serde(default, deserialize_with = "double_option_value")]
    runtime_override: Option<Option<Value>>,
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

/// `Option<Option<Value>>`：缺＝不動，`null`＝清掉，其他＝設成那個值。
fn double_option_value<'de, D>(d: D) -> Result<Option<Option<Value>>, D::Error>
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
    if let Some(role) = &b.role { push("role", role.clone()); }
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
    if let Some(pin) = b.is_primary {
        if pin {
            sets.push("is_primary = 1".into());
            sets.push("primary_position = CASE WHEN primary_position = 0 THEN (SELECT COALESCE(MAX(primary_position), 0) + 1 FROM bots) ELSE primary_position END".into());
        } else {
            sets.push("is_primary = 0".into());
        }
    }
    if let Some(mu) = b.manager_use {
        sets.push("manager_use = ?".into());
        vals.push(Some((mu as i64).to_string()));
    }
    if let Some(t) = &b.track {
        sets.push("track = ?".into());
        vals.push(t.clone().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()));
    }
    if let Some(ro) = &b.runtime_override {
        sets.push("runtime_override_json = ?".into());
        vals.push(ro.as_ref().map(|v| v.to_string()));
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

/// 改名後 herdr 的分頁標籤跟著換（2026-10-17 使用者：分身 `衣服-fork` 改名 `衣服-PM` 後，herdr 選單只看得到舊名，以為 pane 不見了）。
/// 分頁標籤是開 run 時寫的 [`tab_label`](crate::lifecycle::start::tab_label)。子 agent 跟 parent 同一個分頁，不動。失敗只記 log。
async fn relabel_tab(app: &App, bot_id: &str) {
    let Ok(Some(bot)) = db::bot(&app.db, bot_id).await else { return };
    if bot.parent_bot_id.is_some() {
        return;
    }
    let Ok(Some(run)) = db::active_run(&app.db, bot_id).await else { return };
    let Some(tab_id) = run.tab_id.clone() else { return };
    let Some(client) = app.herdr_for_run(&run).await else { return };
    let label = crate::lifecycle::start::tab_label(&bot);
    match client.tab_rename(&tab_id, &label).await {
        Ok(()) => tracing::info!(bot = %bot.name, tab_id, "herdr tab relabelled after rename"),
        Err(e) => tracing::warn!(bot = %bot.name, tab_id, error = %e, "could not relabel the herdr tab after rename"),
    }
}

/// `caller` 是 expert 專案的 manager，`target` 是同專案、不是自己的存活頂層 bot，或自己開的 child。
pub(crate) async fn expert_manager_over(app: &Arc<App>, caller: &str, target: &db::Bot) -> Result<bool, LcError> {
    let own_child = target.managed_by == "child" && target.parent_bot_id.as_deref() == Some(caller);
    if target.id == caller || (target.parent_bot_id.is_some() && !own_child) || target.deleted_at.is_some() {
        return Ok(false);
    }
    let Some(me) = db::bot(&app.db, caller).await.map_err(any_err)? else { return Ok(false) };
    if me.deleted_at.is_some() || me.parent_bot_id.is_some() || me.role.as_deref() != Some("manager") || me.project_id != target.project_id {
        return Ok(false);
    }
    Ok(db::project(&app.db, &me.project_id).await.map_err(any_err)?.is_some_and(|p| p.deleted_at.is_none() && p.mode == "expert"))
}

/// `GET|PUT|DELETE /api/bots/{id}/base-snapshot` by the expert manager of that bot's project.
async fn manager_base_snapshot(app: &Arc<App>, caller: &str, path: &str) -> bool {
    let Some(parts) = decoded_bot_path_segments(path) else { return false };
    if parts.len() != 4 || parts[0] != "api" || parts[1] != "bots" || parts[3] != "base-snapshot" { return false }
    let Ok(Some(target)) = db::bot(&app.db, &parts[2]).await else { return false };
    expert_manager_over(app, caller, &target).await.unwrap_or(false)
}

/// `POST /api/bots/{id}/start|restart` by an expert manager over that bot.
async fn manager_may_start(app: &Arc<App>, caller: &str, method: &str, path: &str) -> bool {
    let Some(parts) = decoded_bot_path_segments(path) else { return false };
    if parts.len() < 3 || parts[0] != "api" || parts[1] != "bots" { return false }
    let Ok(Some(target)) = db::bot(&app.db, &parts[2]).await else { return false };
    if target.manager_use == 0 { return false }
    if crate::runners::relay_deputy::may_restart(app, caller, method, &parts, &target).await { return true }
    if !manager_lifecycle_op(method, &parts, &target) { return false }
    expert_manager_over(app, caller, &target).await.unwrap_or(false)
}

/// start／restart：manager 管的任何成員；stop 與刪除：只限執行者（顧問常駐，只有使用者收）。
fn manager_lifecycle_op(method: &str, parts: &[impl AsRef<str>], target: &db::Bot) -> bool {
    let sub = parts.get(3).map(|p| p.as_ref());
    match (method, parts.len(), sub) {
        ("POST", 4, Some("start" | "restart")) => true,
        ("POST", 4, Some("stop")) => matches!(target.role.as_deref(), Some("executor" | "specialist")),
        // 刪除：只限自己專案的頂層執行者／專家（child 跟著母 bot 走，不另外刪）。
        ("DELETE", 3, None) => target.parent_bot_id.is_none() && matches!(target.role.as_deref(), Some("executor" | "specialist")),
        // 改模型／強度／快速／名稱：頂層、非管理者的成員；欄位範圍在 patch_bot 再驗（只收這四個）。
        ("PATCH", 3, None) => target.parent_bot_id.is_none() && target.role.as_deref() != Some("manager"),
        _ => false,
    }
}

#[derive(Deserialize)]
struct HireMember {
    name: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    identity: Option<String>,
    /// `executor`（預設）或 `specialist`（後者必帶 `track`）。
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    track: Option<String>,
    /// C 期：從同專案、同主機、codex、有 session 的 codex bot 分岔（新 member 第一次啟動用 `codex fork <session>`）。
    #[serde(default)]
    fork_from: Option<String>,
}

/// `fork_from` 的來源檢查（違反回 400／409，錯誤碼見 API.md）。回傳來源最近一段的 codex session id（只為了驗證存在）。
async fn validate_fork_source(app: &Arc<App>, project: &db::Project, source_id: &str, new_kind: &str) -> Result<(), LcError> {
    let bad = |reason: &str| LcError::BadValue(json!({"error": "bad_request", "reason": reason, "fork_from": source_id}));
    if new_kind != "codex" {
        return Err(bad("fork_target_not_codex"));
    }
    let source = db::bot(&app.db, source_id).await.map_err(any_err)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| bad("fork_source_not_found"))?;
    if source.project_id != project.id {
        return Err(bad("fork_source_other_project"));
    }
    if db::bot_host(&app.db, &source.id).await.map_err(any_err)? != project.host {
        return Err(bad("fork_source_other_host"));
    }
    if source.kind != "codex" {
        return Err(bad("fork_source_not_codex"));
    }
    if db::latest_native_session_id(&app.db, &source.id).await.map_err(any_err)?.is_none() {
        return Err(LcError::conflict("fork_source_no_session", json!({"reason": "fork_source_no_session", "fork_from": source_id})));
    }
    Ok(())
}

/// 專家模式的 manager 增聘執行者（使用者 2026-10-31：專家模式沒有 child，manager 要人手就增聘成員）。
/// 建好的是頂層成員、角色固定 executor；顧問只有使用者能建。不自動啟動，manager 用 `agm bot start` 啟動。
async fn hire_member(
    State(app): State<Arc<App>>,
    Path(pid): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<HireMember>,
) -> Result<Response, LcError> {
    let project = db::project(&app.db, &pid).await.map_err(any_err)?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("project".into()))?;
    let allowed = match &principal {
        RequestPrincipal::User => true,
        RequestPrincipal::Bot(caller) => db::bot(&app.db, caller).await.map_err(any_err)?
            .is_some_and(|m| m.deleted_at.is_none() && m.parent_bot_id.is_none() && m.role.as_deref() == Some("manager") && m.project_id == pid),
        _ => false,
    };
    if !allowed || project.mode != "expert" {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "manager_role_scope"})));
    }
    let role = match b.role.as_deref().map(str::trim) {
        None | Some("") | Some("executor") => "executor",
        Some("specialist") => "specialist",
        Some("scheduler") => "scheduler",
        Some(_) => return Err(LcError::Bad("role must be executor, specialist or scheduler".into())),
    };
    let track = b.track.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    if role == "specialist" && track.is_none() {
        return Err(LcError::BadValue(json!({"error": "track_required"})));
    }
    // fork：先驗來源（同專案、同主機、codex、有 session），新 member 也得是 codex。
    let planned_kind = b.kind.as_deref().unwrap_or_default();
    if let Some(source) = b.fork_from.as_deref() {
        validate_fork_source(&app, &project, source, planned_kind).await?;
    }
    let kind = b.kind.clone().ok_or_else(|| LcError::Bad("kind is required".into()))?;
    // fork：沒給的 model／effort／identity 從來源 bot 繼承（2026-11-08：tr3 沒帶 --effort，落在獨立群集）。
    // 逐欄判斷：呼叫端有給的一律照給，只補空的。
    let source = match b.fork_from.as_deref() {
        Some(src) => db::bot(&app.db, src).await.map_err(any_err)?,
        None => None,
    };
    let fill = |own: Option<&str>, from: Option<String>| -> Option<String> {
        own.map(str::trim).filter(|v| !v.is_empty()).map(str::to_string).or_else(|| from.filter(|v| !v.trim().is_empty()))
    };
    let model = fill(b.model.as_deref(), source.as_ref().and_then(|s| s.model.clone()));
    let effort = fill(b.effort.as_deref(), source.as_ref().and_then(|s| s.effort.clone()));
    let identity = fill(b.identity.as_deref(), source.as_ref().and_then(|s| s.identity.clone()));
    let new: NewBot = serde_json::from_value(json!({"name": b.name, "kind": kind, "model": model, "effort": effort, "identity": identity}))
        .map_err(|e| LcError::Bad(e.to_string()))?;
    let res = create_bot(State(app.clone()), Path(pid.clone()), Json(new)).await?;
    let (parts, body) = res.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.map_err(any_err)?;
    let created: Value = serde_json::from_slice(&bytes).map_err(any_err)?;
    let Some(id) = created["bot_id"].as_str().map(str::to_string) else { return Ok(Response::from_parts(parts, axum::body::Body::from(bytes))) };
    let patch: PatchBot = serde_json::from_value(json!({"role": role, "track": track})).map_err(any_err)?;
    patch_bot(State(app.clone()), Path(id.clone()), Extension(RequestPrincipal::User), Json(patch)).await?;
    // fork 來源只寫進 config，不進 DB。
    if b.fork_from.is_some() {
        let bot_id = id.clone();
        let fork_from = b.fork_from.clone();
        crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
            let p = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).ok_or_else(|| anyhow::anyhow!("no-project"))?;
            if let Some(bot) = p.bots.iter_mut().find(|bot| bot.id.as_deref() == Some(bot_id.as_str())) {
                bot.fork_from = fork_from.clone();
            }
            Ok(())
        })
        .await
        .map_err(projection_err)?;
    }
    Ok((StatusCode::OK, Json(json!({"bot_id": id, "name": created["name"], "role": role}))).into_response())
}

#[derive(Deserialize)]
struct PutBotRole {
    role: Option<String>,
    /// 專家（specialist）的專長軌道。管理者設 specialist 時必帶；使用者可隨時改。
    #[serde(default)]
    track: Option<String>,
}

/// 只改角色：使用者可設任何角色；expert 專案的 manager 只能把同專案頂層 bot 設成 executor／advisor／specialist 或清掉。
/// 設成 specialist 一律要帶 `track`（缺 → 400 `track_required`）。
async fn put_bot_role(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<PutBotRole>,
) -> Result<Response, LcError> {
    let role = b.role.filter(|r| !r.trim().is_empty());
    let track = b.track.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    match &principal {
        RequestPrincipal::User => {}
        RequestPrincipal::Bot(caller) => {
            let target = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
            if !expert_manager_over(&app, caller, &target).await? || !matches!(role.as_deref(), None | Some("executor" | "advisor" | "specialist")) {
                return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "manager_role_scope"})));
            }
            // 管理者只能在設成專家時一併帶 track；其他角色不能順手改 track。
            if role.as_deref() != Some("specialist") && b.track.is_some() {
                return Err(bot_user_only());
            }
            if role.as_deref() == Some("specialist") && track.is_none() {
                return Err(LcError::BadValue(json!({"error": "track_required"})));
            }
        }
        _ => return Err(bot_user_only()),
    }
    let mut body = json!({ "role": role });
    if track.is_some() {
        body["track"] = json!(track);
    }
    let patch: PatchBot = serde_json::from_value(body).map_err(any_err)?;
    let response = patch_bot(State(app.clone()), Path(id.clone()), Extension(RequestPrincipal::User), Json(patch)).await?;
    // A child cannot be restarted in place (its parent respawns it and that makes a new record), so its
    // role applies to the running child immediately; the parent tells the child what the role means.
    if let Some(bot) = db::bot(&app.db, &id).await.map_err(any_err)?.filter(|b| b.managed_by == "child") {
        if let (Some(run), Some(project)) = (db::active_run(&app.db, &id).await.map_err(any_err)?, db::project(&app.db, &bot.project_id).await.map_err(any_err)?) {
            let policy = format!("{}:{}:{}:{}", project.id, project.host, project.mode, bot.role.as_deref().unwrap_or(""));
            sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(&policy).bind(&run.id).execute(&app.db).await.map_err(any_err)?;
            app.emit("bot_changed", json!({"bot_id": id})).await;
            return Ok((StatusCode::OK, Json(json!({"needs_restart": false, "applied_live": true}))).into_response());
        }
    }
    Ok(response)
}

async fn patch_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(mut b): Json<PatchBot>,
) -> Result<Response, LcError> {
    if (b.identity.is_some() || b.env.is_some() || b.role.is_some() || b.manager_use.is_some() || b.track.is_some() || b.runtime_override.is_some()) && principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    // 專家管理者（非 AGM 角色的 Bot，含代班的副管理者）只能改 model／effort／fast／name（使用者 2026-11-08）。
    if let RequestPrincipal::Bot(caller) = &principal {
        if crate::supervisor::roles::role_of_bot(&app.db, caller).await.map_err(any_err)?.is_none()
            && (b.persona.is_some() || b.args.is_some() || b.autostart.is_some() || b.inject_hooks.is_some() || b.auto_approve.is_some() || b.is_primary.is_some())
        {
            return Err(bot_user_only());
        }
    }
    if let Some(Some(role)) = &b.role { if !matches!(role.as_str(), "manager" | "executor" | "advisor" | "specialist" | "scheduler") { return Err(LcError::Bad("invalid role".into())); } }
    // 專家（specialist）一定要有 track：合併改動後的結果（角色＋track）必須有 track，否則 400。
    if b.role.is_some() || b.track.is_some() {
        let current = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
        let next_role = match &b.role {
            Some(r) => r.clone(),
            None => current.role.clone(),
        };
        let next_track = match &b.track {
            Some(t) => t.clone(),
            None => current.track.clone(),
        };
        let has_track = next_track.as_deref().map(str::trim).is_some_and(|t| !t.is_empty());
        if next_role.as_deref() == Some("specialist") && !has_track {
            return Err(LcError::BadValue(json!({"error": "track_required"})));
        }
    }
    if let Some(env) = &b.env {
        check_env_names(env)?;
    }
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
    if let Some(Some(m)) = &b.model {
        crate::bot_input::check_model(m)?;
    }
    if b.model.is_some() {
        let current = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
        if current.kind == crate::config::API_BOT_KIND {
            let model = b.model.as_ref().and_then(|m| m.as_deref()).unwrap_or(crate::config::API_BOT_MODEL).trim();
            if !crate::config::api_bot_model_supported(model) { return Err(LcError::BadValue(json!({"error":"bad_request","reason":"api_bot_model_unsupported"}))); }
            b.model = Some(Some(model.to_owned()));
        }
    }
    if let Some(env) = &b.env {
        crate::bot_input::check_env(env)?;
    }
    if let Some(args) = &b.args {
        crate::bot_input::check_args(args)?;
    }
    let restart_relevant = b.role.is_some() || b.model.is_some()
        || b.effort.is_some()
        || b.fast.is_some()
        || b.persona.is_some()
        || b.args.is_some()
        || b.identity.is_some()
        || b.env.is_some()
        || b.auto_approve.is_some()
        || b.inject_hooks.is_some()
        || b.track.is_some()
        || b.runtime_override.is_some();
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
    // 執行期覆寫：看得到 kind／model（PATCH 帶的 model 優先，否則用現在存的），用同一套 context 視窗與 TTL 規則驗證，存正規化後的物件。
    if let Some(Some(ro)) = &b.runtime_override {
        let current = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
        let model = match &b.model {
            Some(Some(m)) => m.clone(),
            Some(None) => String::new(),
            None => current.model.clone().unwrap_or_default(),
        };
        let normalized = crate::role_policy::validate_bot(&kind, &model, ro)?;
        b.runtime_override = Some(Some(normalized));
    }
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
    // child 的 primary 與其他欄位都在 bots 表，交給同一條 UPDATE，避免失敗時留下半套狀態。
    let managed_by = db::bot(&app.db, &id).await.map_err(any_err)?.map(|x| x.managed_by).unwrap_or_default();
    if managed_by != "user" && b.role.as_ref().and_then(|r| r.as_deref()) == Some("manager") { return Err(LcError::Bad("child cannot be manager".into())); }
    if managed_by == "user" {
        if let Some(pin) = b.is_primary {
            let n = crate::primary_order::set_pinned(&app.db, &id, pin).await.map_err(any_err)?;
            if n == 0 {
                return Err(LcError::NotFound("bot".into()));
            }
        }
    }
    let touches_config =
        restart_relevant || b.name.is_some() || b.autostart.is_some() || b.manager_use.is_some();
    if !touches_config {
        if managed_by != "user" && b.is_primary.is_some() {
            patch_unprojected_bot(&app, &id, &b, &effort).await?;
        }
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
        if let Some(role) = &b.role { bot.role = role.clone(); }
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
        if let Some(mu) = b.manager_use {
            bot.manager_use = mu;
        }
        if let Some(t) = &b.track {
            bot.track = t.clone().map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
        }
        if let Some(ro) = &b.runtime_override {
            bot.runtime_override = ro.clone();
        }
        Ok(())
    })
    .await
    .map_err(|e| if e.to_string() == "no-bot" { LcError::NotFound("bot".into()) } else { projection_err(e) })?;
    }
    app.emit("bot_changed", json!({"bot_id": id})).await;
    if b.name.is_some() {
        // 遠端主機要經 ssh，不讓改名的回應等它。
        let (app, id) = (app.clone(), id.clone());
        tokio::spawn(async move { relabel_tab(&app, &id).await });
    }
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
    // 回合中改 codex 的 model／effort 時，live 套用會按 Esc 停下回合、套用後送 continue 接著跑（#1681）：這次有沒有按。
    let mut live_interrupted = false;
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
                    "interrupted": false,
                    "pending_bookkeeping": false,
                    "reason": format!("target_revision_unreadable: {e}"),
                });
                return Ok((StatusCode::OK, Json(out)).into_response());
            }
        };
        let baseline_rev = pre_patch_rev.clone().unwrap_or_else(|| target_rev.clone());
        live_revisions = Some((baseline_rev.clone(), target_rev.clone()));
        let (outcome, interrupted) = lifecycle::apply_live_setting_interrupting(
            &app,
            &id,
            &live_fields,
            &baseline_rev,
            &target_rev,
        )
        .await;
        live_interrupted = interrupted;
        Some(outcome)
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
    let role_needs_restart = if b.role.is_some() {
        match (db::bot(&app.db, &id).await, db::active_run(&app.db, &id).await) {
            (Ok(Some(bot)), Ok(Some(run))) => match db::project(&app.db, &bot.project_id).await {
                Ok(Some(project)) => run.runtime_role_policy.as_deref() != Some(format!("{}:{}:{}:{}", project.id, project.host, project.mode, bot.role.as_deref().unwrap_or("")).as_str()),
                _ => true,
            },
            (Ok(_), Ok(None)) => false,
            _ => true,
        }
    } else { false };
    let mut out = json!({"needs_restart": needs_restart || role_needs_restart});
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
            "interrupted": live_interrupted,
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
    crate::handoff::refuse(&app.db, &id).await?;
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
    detach(&id, finish_bot_delete(app, id.clone(), bot, host, children, snapshot, requested_by, _guards)).await
}

/// 定案之後的收尾（#1065）：在 `detach` 裡跑，client 斷線也會跑完；`begin` 之後的步驟都在這裡。
#[allow(clippy::too_many_arguments)]
async fn finish_bot_delete(app: Arc<App>, id: String, bot: db::Bot, host: String, children: Vec<db::Bot>, snapshot: Vec<Value>, requested_by: String, _guards: Vec<tokio::sync::OwnedMutexGuard<()>>) -> Result<Response, LcError> {
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
    crate::api_bot::schedule_retention_after_delete(&app);
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_bot_after_decided", &id).await;
    // 輸入框草稿跟著 bot 走（含一併刪掉的 child）。
    let draft_keys: Vec<String> = std::iter::once(&id).chain(children.iter().map(|c| &c.id)).map(|b| format!("bot:{b}")).collect();
    crate::drafts::clear_keys(&app, &draft_keys).await;
    let mut child_retry = false;
    if let Err(e) = crate::share::revoke_bot_share(&app, &id).await {
        tracing::error!(bot = %bot.name, error = %e, "bot deleted but its share capability could not be revoked; retrying in the background");
        child_retry = true;
    }
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
    // intent：有 child 軟刪或 share capability 清理沒寫成就留著、背景補完；否則收成 done。
    if child_retry {
        crate::delete_intents::spawn_retry(app.clone(), intent.clone(), "some bot cleanup could not be completed".into());
    } else {
        crate::delete_intents::complete(&app, &intent).await;
    }
    // 排程不跟著刪：告訴呼叫端哪些排程需要改綁，並通知已開著的列表重讀資格。
    let affected_schedules: Result<Vec<(String, String)>, _> = sqlx::query_as(
        "SELECT id, name FROM schedules WHERE bot_id = ? AND deleted_at IS NULL ORDER BY id",
    ).bind(&id).fetch_all(&app.db).await;
    let schedules = match affected_schedules {
        Ok(rows) => {
            for (schedule_id, _) in &rows {
                app.emit("schedules_changed", json!({"project_id": bot.project_id, "id": schedule_id})).await;
            }
            Some(rows.into_iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>())
        }
        Err(e) => {
            tracing::warn!(error = %e, "deleted bot schedules could not be listed");
            None
        }
    };
    let mut out = json!({"removed_children": removed_children, "affected_schedules": schedules});
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
/// 刪除前排著的 restart 在還原成功時放棄：恢復（recovery）worker 會在拿到 bot 鎖後重新檢查 intent 狀態。
async fn abandon_pending_restart<'e, E>(exec: E, bot_id: &str) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "UPDATE intents SET status='abandoned', last_error='bot restore superseded the pending restart', updated_at=?
         WHERE kind='restart' AND subject_id=? AND status IN ('pending','running')",
    )
    .bind(db::now())
    .bind(bot_id)
    .execute(exec)
    .await?;
    Ok(())
}

pub(crate) async fn restore_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    // 跟 delete_bot／start 同一把 per-bot 鎖（#301）：child 同時拿 parent 鎖，避免 parent delete 的 child 快照漏掉
    // 正在還原的 child。依 id 排序一次拿齊，跟 delete_bot／delete_project 的多 bot 鎖順序一致。
    let mut attempt = 0;
    let (bot, _guards) = loop {
        let before = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
        let ids = std::iter::once(id.clone())
            .chain(before.parent_bot_id.iter().filter(|p| !p.is_empty()).cloned())
            .collect();
        let (_, guards) = lock_bots_in_order(&app, ids).await;
        let current = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
        if current.managed_by == before.managed_by && current.parent_bot_id == before.parent_bot_id {
            break (current, guards);
        }
        drop(guards);
        attempt += 1;
        if attempt >= 3 {
            return Err(LcError::conflict("parent_changed", json!({"bot_id": id})));
        }
    };
    if bot.deleted_at.is_none() {
        return Err(LcError::conflict("bot is not deleted", json!({"bot_id": id})));
    }
    crate::handoff::refuse(&app.db, &id).await?;
    // A restored restricted bot keeps its profile, but a public capability from before deletion never returns.
    crate::share::revoke_bot_share(&app, &id).await.map_err(any_err)?;
    // 放棄的 intent 只在還原真的成功時才寫（下面各分支、驗證都過之後）：被 409 擋下的還原不能先動它（交易範圍）。
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
        #[cfg(test)]
        crate::lifecycle::race_point::hit("restore_child_after_liveness_check", &id).await;
        // A child restored by AGM is reopened by its parent in the existing pane; allow ten minutes
        // for that asynchronous hand-off before reconcile applies the normal retirement rule.
        let mut tx = app.db.begin().await.map_err(any_err)?;
        crate::child_reconcile_safety::record_retirement_grace_on(&mut tx, &id).await.map_err(any_err)?;
        let changed = sqlx::query(
            "UPDATE bots SET deleted_at = NULL WHERE id = ? AND deleted_at IS NOT NULL
               AND EXISTS (SELECT 1 FROM projects p WHERE p.id = bots.project_id AND p.deleted_at IS NULL)
               AND (bots.parent_bot_id IS NULL OR EXISTS (SELECT 1 FROM bots parent WHERE parent.id = bots.parent_bot_id AND parent.deleted_at IS NULL))",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(any_err)?
        .rows_affected();
        if changed != 1 {
            tx.rollback().await.map_err(any_err)?;
            return Err(LcError::conflict("the project or parent bot of this child is deleted", json!({"bot_id": id})));
        }
        // 還原成功才放棄刪除前排著的 restart，跟復活在同一個交易裡。
        abandon_pending_restart(&mut *tx, &id).await.map_err(any_err)?;
        tx.commit().await.map_err(any_err)?;
    } else {
        // 讀不懂的 args／env 不能當成空的寫回 config（#295）：env 可能帶帳號設定，還原後會以錯的身份起。
        let args = serde_json::from_str(&bot.args_json).map_err(|e| LcError::conflict("bot args_json is unreadable; not restoring", json!({"bot_id": id, "error": e.to_string()})))?;
        let env = serde_json::from_str(&bot.env_json).map_err(|e| LcError::conflict("bot env_json is unreadable; not restoring", json!({"bot_id": id, "error": e.to_string()})))?;
        let entry = crate::config::BotCfg { role: None, manager_use: true, track: None, runtime_override: None, fork_from: None,
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
                anyhow::bail!("project-gone")
            };
            if !p.bots.iter().any(|b| b.id.as_deref() == Some(entry.id.as_deref().unwrap_or_default())) {
                p.bots.push(entry.clone());
            }
            Ok(())
        })
        .await
        .map_err(|e| {
            // 專案已刪＝呼叫端的前提不成立（還原不回去、重試也一樣）：409，不是 502 upstream。
            if e.to_string() == "project-gone" {
                LcError::conflict("the project of this bot is deleted", json!({"bot_id": id, "project_id": bot.project_id}))
            } else {
                projection_err(e)
            }
        })?;
        abandon_pending_restart(&app.db, &id).await.map_err(any_err)?;
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
    // 受限分享用 bot 的工作目錄（issue #828）：刪除時也是收進回收區的。
    // 還原時搬回；若已過期或被淘汰則重建乾淨的工作目錄與 inbox，舊檔案不再重現。
    if let Ok(Some(ws)) = crate::share::store::restricted_workspace(&app.db, &id).await {
        if let Some(ws_path) = crate::share::folder::validate_workspace_path(&app.data_dir, &ws) {
            match crate::bot_trash::restore_kind(&app.data_dir, &id, Some(crate::bot_trash::SHARE_WORKSPACE), &ws_path) {
                Ok(Some(from)) => tracing::info!(bot = %id, from = %from.display(), to = %ws, "restored restricted share workspace from bots-trash"),
                Ok(None) => {
                    if !ws_path.exists() {
                        let mut b = std::fs::DirBuilder::new();
                        b.recursive(true);
                        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
                        if let Err(e) = b.create(&ws_path) {
                            tracing::warn!(bot = %id, dir = %ws, error = %e, "could not recreate clean share workspace folder");
                        }
                    }
                    if ws_path.is_dir() {
                        let _ = crate::share::folder::ensure_inbox(&ws_path);
                        tracing::warn!(bot = %id, dir = %ws, "restricted share workspace expired or evicted from trash; recreated clean workspace");
                    }
                }
                Err(e) => tracing::warn!(bot = %id, error = %e, "could not restore restricted share workspace from bots-trash"),
            }
        }
    }
    crate::runners::remote_trash::restore_for(&app, &id).await;
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

// A host config write and its HostManager update are one mutation. Serialize create/update and
// delete so a concurrent same-name request cannot remove or install a stale connection afterward.
static HOST_CONFIG_MUTATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 這次更新會不會改變「這個名字指到哪台機器或 SSH 身份」。`ssh_opts` 可以改 `User`、`IdentityFile` 等登入身份，
/// 因此有 live project 時任何 `ssh_opts` 變更都要明確確認；`remote_path` 只影響同一台上的工具路徑。
fn repoints_host(old: &HostCfg, new: &HostCfg) -> bool {
    old.ssh != new.ssh || old.ssh_port != new.ssh_port || old.herdr_session != new.herdr_session || old.ssh_opts != new.ssh_opts
}

/// 主機上還活著的專案。`delete_host` 用同一條判斷（`api.rs` 的 `host still used by projects`）。
async fn live_projects_on_host(app: &Arc<App>, host: &str) -> Result<Vec<db::Project>, LcError> {
    Ok(db::live_projects(&app.db).await.map_err(any_err)?.into_iter().filter(|p| p.host == host).collect())
}

/// Bot work that keeps a host generation relevant after its project has been soft-deleted.
/// Shared-session hosts intentionally keep the other daemon's remote bot directories, so only
/// their live bot rows and active runs block host removal/repointing.
async fn host_bots_requiring_attention(app: &Arc<App>, host: &str) -> Result<Vec<String>, LcError> {
    let require_remote_purge = !crate::shared_host::is_shared(app, host).await;
    // 欠著目錄的 bot 歸屬看記號（#1320）：專案搬走之後，舊主機的債仍擋舊主機的刪除／換指，不能只看 `projects.host`。
    Ok(sqlx::query_scalar(
        "SELECT b.id FROM bots b
           JOIN projects p ON p.id = b.project_id
           LEFT JOIN remote_bot_dir_purges rp ON rp.bot_id = b.id
         WHERE (p.host = ?1 OR (?2 = 1 AND rp.host = ?1 AND rp.purged_at IS NULL))
           AND (
             b.deleted_at IS NULL
             OR EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = b.id AND r.state IN ('starting','running','stopping'))
             OR (?2 = 1 AND (rp.bot_id IS NULL OR rp.purged_at IS NULL) AND (rp.bot_id IS NULL OR rp.host = ?1))
           )
         ORDER BY b.id",
    )
    .bind(host)
    .bind(if require_remote_purge { 1_i64 } else { 0_i64 })
    .fetch_all(&app.db)
    .await
    .map_err(any_err)?)
}

async fn create_host(
    State(app): State<Arc<App>>,
    Query(q): Query<DeleteQuery>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<NewHost>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
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
    let host_mutation = HOST_CONFIG_MUTATION.lock().await;
    // 更新既有主機時，沒帶的選填欄位一律沿用舊值（共用 session 是安全開關 #709；remote_path 被清掉會讓遠端 PATH 找不到 herdr，#1752）。
    // 新主機才用預設值。
    let prev = app.cfg.get().await.hosts.into_iter().find(|h| h.name == b.name);
    let cfg = HostCfg { daemon_url: prev.as_ref().and_then(|p| p.daemon_url.clone()),
        name: b.name.clone(),
        ssh: b.ssh.trim().to_string(),
        ssh_port: b.ssh_port.or(prev.as_ref().map(|p| p.ssh_port)).unwrap_or(22),
        ssh_opts: b.ssh_opts.or_else(|| prev.as_ref().map(|p| p.ssh_opts.clone())).unwrap_or_default(),
        // trim 跟 `ssh` 同一條規矩：存進 config 的字就是之後直接交給 herdr 的字。
        herdr_session: b
            .herdr_session
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| prev.as_ref().map(|p| p.herdr_session.clone()))
            .unwrap_or_else(|| crate::config::DEFAULT_HERDR_SESSION.to_string()),
        remote_path: b.remote_path.or_else(|| prev.as_ref().map(|p| p.remote_path.clone())).unwrap_or_default(),
        shared_session: b.shared_session.or(prev.as_ref().map(|p| p.shared_session)).unwrap_or(false),
    };
    // 這支同時是新增與**更新**（docs/API.md）。更新到「指去另一台機器」時要跟 `delete_host` 一樣先確認
    // 主機上沒有活著的專案（issue #544）：`apply_config` 會把連線整個換掉，但 `runs` 一列都不動——
    // 那些 run 還帶著**舊那台**開出來的 pane id，接下來 daemon 會拿它們去問新那台。輕則 pane 不存在、
    // run 被判成不見了收掉；重則新機器上剛好有同樣的 pane id（herdr 的 id 是每個實例自己編的），
    // prompt 與按鍵就送進一顆完全不相干的 pane。刪掉只是「連線沒了」，改掉是「連線還在、但接到別台」。
    let existing = app.cfg.get().await.hosts.into_iter().find(|h| h.name == cfg.name);
    let is_repoint = existing.as_ref().is_some_and(|old| repoints_host(old, &cfg));
    let repoint_bots = if is_repoint {
        let projects = live_projects_on_host(&app, &b.name).await?;
        let bots = host_bots_requiring_attention(&app, &b.name).await?;
        if (!projects.is_empty() || !bots.is_empty()) && q.confirm.as_deref() != Some("repoint") {
            let old = existing.as_ref().expect("is_repoint implies an existing host");
            return Err(LcError::conflict(
                "host still used by projects",
                json!({
                    "reason": "host_repoint_in_use",
                    "host": b.name,
                    "from": {"ssh": old.ssh, "ssh_port": old.ssh_port, "herdr_session": old.herdr_session},
                    "to": {"ssh": cfg.ssh, "ssh_port": cfg.ssh_port, "herdr_session": cfg.herdr_session},
                    "projects": projects.iter().map(|p| json!({"id": p.id, "label": p.label})).collect::<Vec<_>>(),
                    "bot_ids": bots,
                    "hint": "先移走專案、停止孤兒 bot，並等遠端 bot 目錄清理完成；真的要改就帶 ?confirm=repoint（既有的 run 會留著舊機器的 pane id）",
                }),
            ));
        }
        bots
    } else {
        Vec::new()
    };
    let c2 = cfg.clone();
    let confirm_repoint = q.confirm.as_deref() == Some("repoint");
    let update = app
        .cfg
        .update(move |f| {
            // Project registration/deletion also commits through this config lock. Recheck there
            // so a project that lands after the async DB snapshot cannot slip past confirmation.
            if !confirm_repoint
                && f.hosts.iter().find(|h| h.name == c2.name).is_some_and(|old| repoints_host(old, &c2))
                && f.projects.iter().any(|p| p.host == c2.name)
            {
                anyhow::bail!("host_repoint_in_use");
            }
            match f.hosts.iter_mut().find(|h| h.name == c2.name) {
                Some(existing) => *existing = c2,
                None => f.hosts.push(c2),
            }
            Ok(())
        })
        .await;
    if let Err(e) = update {
        if e.to_string() == "host_repoint_in_use" {
            let old = existing.as_ref().expect("a raced repoint retains its old host config");
            let projects: Vec<_> = app
                .cfg
                .get()
                .await
                .projects
                .into_iter()
                .filter(|p| p.host == cfg.name)
                .map(|p| json!({"id": p.id, "label": p.label}))
                .collect();
            return Err(LcError::conflict(
                "host still used by projects",
                json!({
                    "reason": "host_repoint_in_use",
                    "host": b.name,
                    "from": {"ssh": old.ssh, "ssh_port": old.ssh_port, "herdr_session": old.herdr_session},
                    "to": {"ssh": cfg.ssh, "ssh_port": cfg.ssh_port, "herdr_session": cfg.herdr_session},
                    "projects": projects,
                    "bot_ids": repoint_bots,
                    "hint": "先移走專案、停止孤兒 bot，並等遠端 bot 目錄清理完成；真的要改就帶 ?confirm=repoint（既有的 run 會留著舊機器的 pane id）",
                }),
            ));
        }
        // 設定不合法是請求的問題（400 config_invalid），不是上游壞掉（#1568）。
        return Err(cfg_err(e));
    }
    let hosts = app.cfg.get().await.hosts;
    let changed_hosts = app.hosts.apply_config(&app, &hosts).await;
    #[cfg(test)]
    crate::lifecycle::race_point::hit("create_host_after_apply_config", &b.name).await;
    drop(host_mutation);
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

async fn delete_host(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    if name == LOCAL_HOST {
        return Err(LcError::Bad("`local` cannot be removed".into()));
    }
    let _host_mutation = HOST_CONFIG_MUTATION.lock().await;
    for p in db::live_projects(&app.db).await.map_err(any_err)? {
        if p.host == name {
            return Err(LcError::conflict("host still used by projects", json!({"project_id": p.id})));
        }
    }
    // Project deletion may leave a soft-deleted bot's remote directory (including its herdr/cargo shims)
    // waiting for `remote_purge`, or a late child may still have an active run. Removing the host row
    // would discard the only route needed to stop that run and finish cleaning its directory.
    let pending_bots = host_bots_requiring_attention(&app, &name).await?;
    if !pending_bots.is_empty() {
        return Err(LcError::conflict(
            "host has bots requiring cleanup",
            json!({
                "reason": "host_bot_cleanup_pending",
                "host": name,
                "bot_ids": pending_bots,
                "hint": "先停止孤兒 bot，並等遠端 bot 目錄與 shim 清理完成，再移除主機",
            }),
        ));
    }
    let n2 = name.clone();
    let remove = app
        .cfg
        .update(move |f| {
            // Project registration shares this config lock. Recheck while holding it so a project
            // committed after the DB snapshot cannot be left pointing at a host we just removed.
            if f.projects.iter().any(|p| p.host == n2) {
                anyhow::bail!("host_still_used_by_projects");
            }
            f.hosts.retain(|h| h.name != n2);
            Ok(())
        })
        .await;
    if let Err(e) = remove {
        if e.to_string() == "host_still_used_by_projects" {
            let project_id = app.cfg.get().await.projects.into_iter().find(|p| p.host == name).and_then(|p| p.id);
            return Err(LcError::conflict("host still used by projects", json!({"project_id": project_id})));
        }
        return Err(cfg_err(e));
    }
    #[cfg(test)]
    crate::lifecycle::race_point::hit("delete_host_after_config_remove", &name).await;
    app.hosts.remove(&app, &name).await;
    app.emit("project_changed", json!({})).await;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

async fn reconnect_host(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let (connected, error) = app.hosts.reconnect(&app, &name).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    Ok((StatusCode::OK, Json(json!({"name": name, "connected": connected, "error": error}))).into_response())
}

async fn refresh_tools(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let fence = app.hosts.fence(&name).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    let ht = crate::tools::detect_with_fence(&app, &name, &fence).await.map_err(|e| detect_error(&name, e))?;
    crate::state::emit_host_changed(&app, &fence).await;
    Ok((
        StatusCode::OK,
        Json(json!({
            "name": name,
            "tools": crate::quota_agy::tools_json(&name, &ht.tools),
            "identities": crate::login_prompt::identities_json(&app, &name, &ht.identities),
            "shell_identities": ht.shell_identities,
            "tools_checked_at": ht.checked_at,
            "baseline": app.host_baseline.lock().await.get(&name).map(|b| b.view(chrono::Utc::now())),
        })),
    )
        .into_response())
}

/// 一致性差異的 dry-run 計畫（#719 階段 2，API §12.6a）：只由快取的差異與宣告檔推算，不連主機、不寫任何東西。
async fn baseline_plan(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    if app.hosts.get(&name).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let (report, plan) = cached_baseline_plan(&app, &name).await?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "host": name,
            "checked_at": report.checked_at,
            "stale": report.stale,
            "plan_rev": plan.plan_rev,
            "actions": plan.actions,
        })),
    )
        .into_response())
}

/// 快取的差異＋宣告檔推出的計畫（plan 與 apply 共用同一份推算，`plan_rev` 才對得上）。
async fn cached_baseline_plan(app: &Arc<App>, name: &str) -> Result<(crate::host_baseline::BaselineReport, crate::host_baseline::plan::Plan), LcError> {
    let Some(report) = app.host_baseline.lock().await.get(name).map(|b| b.snapshot(chrono::Utc::now())) else {
        return Err(LcError::conflict("baseline_unknown", json!({"host": name, "message": "這台主機還沒量過一致性"})));
    };
    let Some(issues) = report.issues.as_deref() else {
        return Err(LcError::conflict("baseline_unknown", json!({"host": name, "message": "上一次探測沒跑完，差異未知"})));
    };
    let (Ok(m), Ok(v)) = (crate::host_baseline::manifest::shipped(), crate::host_baseline::manifest::shipped_values()) else {
        return Err(LcError::Unavailable(json!({"error": "unavailable", "reason": "baseline_manifest_invalid", "host": name})));
    };
    let plan = crate::host_baseline::plan::build(m, v, issues);
    Ok((report, plan))
}

/// #719 階段 3a：apply／restore 在主機上跑腳本的上限。
const BASELINE_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 在 fence 綁的那條連線上跑（本機 `/bin/sh`、遠端 ssh）。呼叫端包在 `run_if_current` 裡。
async fn baseline_run_script(fence: &crate::hosts::HostFence, script: &str) -> anyhow::Result<String> {
    if fence.conn().is_local() {
        let out = crate::hosts::sh_local(script, BASELINE_WRITE_TIMEOUT).await?.ok_or_else(|| anyhow::anyhow!("local baseline script timed out"))?;
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    fence.conn().ssh_exec_path_timeout(script, BASELINE_WRITE_TIMEOUT).await
}

fn baseline_superseded(name: &str) -> LcError {
    LcError::conflict("host_superseded", json!({"host": name, "retryable": true}))
}

#[derive(Deserialize)]
struct BaselineApply {
    plan_rev: String,
    /// 省略＝計畫裡全部 `merge_key`。
    #[serde(default)]
    ids: Option<Vec<String>>,
}

/// 一致性差異補齊（#719 階段 3a，API §12.6b）：只補計畫裡的 `merge_key`、只補不蓋，寫前在主機上備份。
async fn baseline_apply(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<BaselineApply>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let fence = app.hosts.fence(&name).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    baseline_apply_with_fence(&app, &name, &fence, b).await
}

async fn baseline_apply_with_fence(app: &Arc<App>, name: &str, fence: &crate::hosts::HostFence, b: BaselineApply) -> Result<Response, LcError> {
    use crate::host_baseline::apply;
    if app.cfg.get().await.host_baseline_sync(name) == crate::config::HostBaselineSyncMode::Off {
        return Err(LcError::conflict(
            "sync_disabled",
            json!({"host": name, "message": "這台主機沒有開一致性補齊（config.toml 的 [host_baseline.sync]）"}),
        ));
    }
    let (_, plan) = cached_baseline_plan(app, name).await?;
    if plan.plan_rev != b.plan_rev {
        return Err(LcError::conflict("plan_outdated", json!({"host": name, "plan_rev": plan.plan_rev})));
    }
    let prepared = apply::prepare(&plan, b.ids.as_deref())
        .map_err(|id| LcError::BadValue(json!({"error": "bad_request", "reason": "not_fixable", "id": id})))?;
    let mut results = Vec::new();
    let mut backup_id = None;
    let mut error = None;
    if !prepared.writes.is_empty() {
        let id = apply::new_backup_id();
        let script = apply::render_apply_sh(&id, &plan.plan_rev, &prepared.writes).map_err(|e| LcError::Upstream(e.into()))?;
        let out = app
            .hosts
            .run_if_current(fence, baseline_run_script(fence, &script))
            .await
            .ok_or_else(|| baseline_superseded(name))?
            .map_err(|e| LcError::Upstream(format!("{e:#}")))?;
        let run = apply::parse_apply(&out, &prepared.writes)
            .ok_or_else(|| LcError::Upstream("baseline apply output was cut short; the host may be partly written".into()))?;
        backup_id = run.backup_written.then_some(id);
        error = run.error;
        results = run.outcomes;
        crate::tools::spawn_detect(app.clone(), name.to_string());
    }
    results.extend(prepared.manual);
    Ok((
        StatusCode::OK,
        Json(json!({"host": name, "plan_rev": plan.plan_rev, "backup_id": backup_id, "error": error, "results": results})),
    )
        .into_response())
}

#[derive(Deserialize)]
struct BaselineRestore {
    backup_id: String,
}

/// 照主機上的備份還原（#719 階段 3a，API §12.6c）。sync 是 off 也能用；回應只有目標名與結果。
async fn baseline_restore(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<BaselineRestore>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    if !crate::host_baseline::apply::valid_backup_id(&b.backup_id) {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_backup_id"})));
    }
    let fence = app.hosts.fence(&name).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    baseline_restore_with_fence(&app, &name, &fence, &b.backup_id).await
}

async fn baseline_restore_with_fence(app: &Arc<App>, name: &str, fence: &crate::hosts::HostFence, backup_id: &str) -> Result<Response, LcError> {
    use crate::host_baseline::apply;
    let script = apply::render_restore_sh(backup_id)
        .map_err(|_| LcError::BadValue(json!({"error": "bad_request", "reason": "invalid_backup_id"})))?;
    let out = app
        .hosts
        .run_if_current(fence, baseline_run_script(fence, &script))
        .await
        .ok_or_else(|| baseline_superseded(name))?
        .map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    let run = apply::parse_restore(&out).ok_or_else(|| LcError::Upstream("baseline restore output was cut short".into()))?;
    match run.error {
        Some("not_found") => return Err(LcError::NotFoundValue(json!({"error": "not_found", "reason": "backup_not_found"}))),
        Some(refused) => return Err(LcError::conflict("restore_refused", json!({"host": name, "refused": refused}))),
        None => {}
    }
    crate::tools::spawn_detect(app.clone(), name.to_string());
    Ok((StatusCode::OK, Json(json!({"host": name, "backup_id": backup_id, "files": run.files}))).into_response())
}

#[cfg(test)]
mod baseline_plan_route_tests;

#[cfg(test)]
mod baseline_apply_route_tests;

/// 偵測結果被作廢不是上游錯誤：409 `host_superseded`、可重試；其餘錯誤照舊 502。
fn detect_error(host: &str, e: anyhow::Error) -> LcError {
    if e.is::<crate::tools::Superseded>() {
        return LcError::conflict("host_superseded", json!({"host": host, "retryable": true}));
    }
    LcError::Upstream(format!("{e:#}"))
}

#[derive(Deserialize)]
struct InstallTool {
    kind: String,
    /// 省略（或空字串）＝不借 bot，daemon 開臨時 host-shell pane 直接裝（issue #1931）。
    #[serde(default)]
    via_bot_id: Option<String>,
    #[serde(default)]
    install_id: Option<String>,
}

async fn install_tool(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<InstallTool>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let Some(via) = b.via_bot_id.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
        let out = crate::tool_install_shell::start(&app, &name, &b.kind, b.install_id.as_deref()).await?;
        return Ok((StatusCode::OK, Json(out)).into_response());
    };
    let out = crate::app_ports_p3::install_via_bot(&app, &name, &b.kind, via).await?;
    Ok((StatusCode::OK, Json(json!({"turn_id": out.turn_id, "message_id": out.message_id, "delivery": out.delivery})))
        .into_response())
}

/// Login output is only visible through the returned shell pane — never in logs, events, or the response.
async fn login_identity(
    State(app): State<Arc<App>>,
    Path((name, identity)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    identity_auth(app, name, identity, false).await
}

/// 登出走同一條路：開臨時 pane、帶同一組環境變數下指令、等 CLI 結束再重驗登入狀態。
/// 分開的只有指令本身——共用一條才不會有一邊忘了帶 `CLAUDE_CONFIG_DIR` 而動到別的帳號。
async fn logout_identity(
    State(app): State<Arc<App>>,
    Path((name, identity)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    identity_auth(app, name, identity, true).await
}

/// agy 沒有 `logout` 子命令也沒有身份：直接刪那台主機的憑證檔、清額度快照（`quota_agy::logout`）。
async fn logout_agy(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    require_user(&principal)?;
    match crate::runners::quota_agy::logout(&app, &name).await {
        Ok(removed) => Ok(Json(json!({"removed": removed}))),
        Err(crate::quota_agy::LogoutError::UnknownHost) => Err(LcError::NotFound("host".into())),
        Err(crate::quota_agy::LogoutError::Superseded) => Err(LcError::conflict("host_superseded", json!({"host": name, "retryable": true}))),
        Err(crate::quota_agy::LogoutError::Failed(m)) => Err(LcError::Upstream(m)),
    }
}

/// 裝／更新那台主機的 agy（官方 manifest ＋ sha512，不跑官方 `install.sh`；`agy_install::install`）。
async fn install_agy(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    require_user(&principal)?;
    match crate::agy_install::install(&app, &name).await {
        Ok(done) => {
            // 升好了：上游更新快照立刻跟上，header／設定頁不用等下一輪 10 分鐘巡邏才收起「可更新」。
            crate::upstream_update::note_installed(app.as_ref(), "agy", &name, &done.to).await;
            Ok(Json(serde_json::to_value(done).unwrap_or(Value::Null)))
        }
        Err(crate::agy_install::InstallError::UnknownHost) => Err(LcError::NotFound("host".into())),
        Err(crate::agy_install::InstallError::Busy(m)) => {
            Err(LcError::conflict("agy_install_in_progress", json!({"host": name, "message": m})))
        }
        Err(crate::agy_install::InstallError::Failed { reason, message }) => {
            tracing::warn!(host = %name, reason, %message, "agy install failed");
            Err(LcError::Upstream(format!("{reason}: {message}")))
        }
    }
}

async fn identity_auth(app: Arc<App>, name: String, identity: String, logout: bool) -> Result<Response, LcError> {
    // 沒寫 host 的身份在遠端也生效：未知主機若先查 identity／PATH，會變成 409「CLI 不在 PATH」。
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
    // claude（貼 code）與 codex（裝置碼）的登入 pane 都登記，網頁才拿得到網址與碼。
    let login_reservation = if !logout && matches!(idn.kind.as_str(), "claude" | "codex") {
        crate::runners::login_assist::prune_dead(&app, &name, &identity).await;
        Some(crate::runners::login_assist::reserve(&app, &name, &identity)?)
    } else {
        None
    };
    let login_kind = idn.kind.clone();
    let app_for_open = app.clone();
    let name_for_open = name.clone();
    let command_for_open = command.clone();
    let opened = app.hosts.run_if_current(&fence, async move {
        let shell = shell::open(&app_for_open, &name_for_open, None).await?;
        if let Err(e) = shell::send_text(&app_for_open, &name_for_open, &shell.pane_id, &command_for_open, true).await {
            let _ = shell::close(&app_for_open, &name_for_open, &shell.pane_id).await;
            return Err(e);
        }
        if let Some(reservation) = login_reservation {
            reservation.register(&shell.pane_id, &shell.created_at, &login_kind);
        }
        Ok::<_, LcError>(shell)
    }).await;
    let shell = opened
        .ok_or_else(|| LcError::Upstream(format!("host `{name}` changed before identity authentication started")))??;
    crate::tools::spawn_identity_login_watch(app, name, shell.pane_id.clone(), identity, idn.kind, logout, fence);
    Ok((StatusCode::OK, Json(json!(shell))).into_response())
}

#[cfg(test)]
mod agy_logout_route_tests;

#[cfg(test)]
mod install_tool_route_tests;

#[cfg(test)]
mod refresh_tool_error_tests;

#[cfg(test)]
mod identity_auth_error_tests;

async fn get_gh_status(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
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
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<GhLoginBody>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let v = crate::gh_auth::login(&app, &name, b.mode.as_deref(), b.user.as_deref()).await?;
    Ok((StatusCode::OK, Json(v)).into_response())
}

async fn cancel_gh(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
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
    Extension(principal): Extension<RequestPrincipal>,
    body: Option<Json<NewShell>>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let cwd = body.and_then(|Json(b)| b.cwd);
    let s = shell::open(&app, &name, cwd.as_deref()).await?;
    Ok((StatusCode::OK, Json(json!(s))).into_response())
}

async fn list_host_shells(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let shells = shell::list(&app, &name).await?;
    Ok((StatusCode::OK, Json(json!({"host": name, "shells": shells, "max": shell::MAX_PER_HOST}))).into_response())
}

async fn get_host_shell_terminal(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    require_user(&principal)?;
    let source = q.get("source").cloned().unwrap_or_else(|| "visible".into());
    let lines: u32 = q.get("lines").and_then(|s| s.parse().ok()).unwrap_or(200).clamp(1, 2000);
    let ansi = match q.get("format").map(String::as_str) {
        None | Some("text") => false,
        Some("ansi") => true,
        Some(_) => return Err(LcError::Bad("bad format".into())),
    };
    Ok(Json(shell::read_as(&app, &name, &pane_id, &source, lines, ansi).await?))
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
    Extension(principal): Extension<RequestPrincipal>,
    Query(q): Query<HashMap<String, String>>,
    Json(b): Json<ShellTextIn>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    shell::send_text(&app, &name, &pane_id, &b.text, b.enter.unwrap_or(true)).await?;
    let body = shell::echo_after_send(&app, &name, &pane_id, q.get("echo").map(String::as_str) == Some("ansi")).await;
    Ok((StatusCode::OK, Json(body)).into_response())
}

/// `GET /api/hosts/:name/shells/:pane_id/login`：daemon 為 claude 身份登入開的 pane 上，從畫面取出的 OAuth 網址與進度（`login_assist.rs`）。
/// 網址不進 log／事件，所以不快取。不是登入 pane 一律 404。
async fn host_shell_login_status(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let v = crate::runners::login_assist::status(&app, &name, &pane_id).await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(v)).into_response())
}

#[derive(Deserialize)]
struct LoginCodeIn {
    code: String,
}

/// `POST /api/hosts/:name/shells/:pane_id/login/code`：畫面還在等 code 才把它打進登入 pane 並按 Enter。
async fn host_shell_login_code(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<LoginCodeIn>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let v = crate::runners::login_assist::submit_code(&app, &name, &pane_id, &b.code).await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(v)).into_response())
}

/// Not `KeysIn`: a shell has no run, so accepting `expect_run_id` would be a lie.
#[derive(Deserialize)]
struct ShellKeysIn {
    keys: Vec<String>,
}

async fn host_shell_keys(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
    Query(q): Query<HashMap<String, String>>,
    Json(b): Json<ShellKeysIn>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    shell::send_keys(&app, &name, &pane_id, &b.keys).await?;
    let body = shell::echo_after_send(&app, &name, &pane_id, q.get("echo").map(String::as_str) == Some("ansi")).await;
    Ok((StatusCode::OK, Json(body)).into_response())
}

/// 記憶體清單或 `panes` 表認得的才關；兩邊都沒有 404。走 `panes` 表的那條照 `?confirm=` 決定要不要先 409（`shell::close_confirmed`）。
async fn close_host_shell(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Extension(principal): Extension<RequestPrincipal>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
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
mod changelog_route_tests;

fn api_bot_models() -> Value {
    let models: Vec<_> = [crate::config::API_BOT_MODEL, crate::config::API_BOT_FABLE_MODEL].into_iter().map(|model| json!({
        "id": model, "display_name": if model == crate::config::API_BOT_MODEL { "Claude Opus 5.5" } else { "Claude Fable 5.2" }, "description": "", "is_default": model == crate::config::API_BOT_MODEL,
        "default_effort": crate::config::API_BOT_DEFAULT_EFFORT,
        "efforts": crate::config::efforts_for_kind(crate::config::API_BOT_KIND),
        "service_tiers": [{"id":"fast", "name":"fast", "description":"speed=fast（單價 2 倍）"}],
    })).collect();
    json!({"kind":crate::config::API_BOT_KIND,"host":LOCAL_HOST,"source":"static","fetched_at":db::now(),"models":models})
}

async fn get_models(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Query(q): Query<ModelsQuery>,
) -> Result<Json<Value>, LcError> {
    if q.kind == crate::config::API_BOT_KIND {
        // 沒有 CLI 可問：清單是寫死的一筆（harness 只會送這個模型），不看 host／身份、也不走快取。
        return Ok(Json(api_bot_models()));
    }
    if !crate::config::valid_kind(&q.kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::kinds_list())));
    }
    let host = q.host.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    if app.hosts.get(&host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let identity = q.identity.as_deref().filter(|s| !s.trim().is_empty());
    if let RequestPrincipal::Bot(bot_id) | RequestPrincipal::Child { child_id: bot_id, .. } = &principal {
        if matches!(&principal, RequestPrincipal::Child { .. }) || crate::supervisor::roles::role_of_bot(&app.db, bot_id).await.map_err(any_err)?.is_none() {
            if let Some(cached) = crate::models::cached(&app, &host, &q.kind, identity).await {
                return Ok(Json(cached));
            }
            // #1890：未命中就由 daemon 在背景排一趟刷新（single-flight、每格每分鐘一次、失敗退避）；這次請求仍只回 503。
            let refresh = crate::runners::models_refresh::on_cache_miss(&app, &host, &q.kind, identity).await;
            return Err(LcError::Unavailable(json!({
                "error": "model_cache_miss",
                "message": "一般 Bot 只能讀取 10 分鐘內的模型快取；daemon 已排程背景刷新，請稍後重試（見 refresh）",
                "host": host,
                "kind": q.kind,
                "identity": identity,
                "retry_after_secs": 30,
                "refresh": refresh,
            })));
        }
    }
    let v = crate::runners::models::list(&app, &host, &q.kind, identity, flag(&q.refresh)).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok(Json(v))
}

#[cfg(test)]
mod project_tests;

#[cfg(test)]
mod search_tests;

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
        return Err(unknown_host(&host));
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
        return Err(unknown_host(&host));
    }
    let socket = q.get("socket").map(String::as_str);
    Ok(Json(crate::app_ports_p3::pane_preview(&app, &host, pane_id, socket, lines).await?))
}

/// 不認得的主機（#1384）：404 的 `what` 一律是資源名 `host`（API.md §1，同其他端點）；那句話放 message，網頁照讀 `reason ?? message`。
fn unknown_host(host: &str) -> LcError {
    LcError::NotFoundValue(json!({"error": "not_found", "what": "host", "host": host, "message": format!("unknown host `{host}`")}))
}

/// `pid` 要是正的 32 位元整數：`as i32` 會把 2³²+N 截成 N，守衛驗的與訊號打的就都是另一顆行程。
fn kill_pid(body: &Value) -> Result<i32, LcError> {
    body.get("pid")
        .and_then(Value::as_i64)
        .and_then(|p| i32::try_from(p).ok())
        .filter(|p| *p > 0)
        .ok_or_else(|| LcError::Bad("pid must be a positive integer".into()))
}

/// API.md（§mem）：`signal` 只認 `TERM`／`KILL`（不分大小寫）；沒帶或 `null` 就是 `TERM`。其他值（含非字串）400，
/// 而且在查主機、取樣、送訊號**之前**就擋下，什麼都不送。以前靜默降成 TERM（`HUP` 也會送 TERM）。
fn kill_signal(body: &Value) -> Result<&'static str, LcError> {
    match body.get("signal") {
        None | Some(Value::Null) => Ok("TERM"),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("TERM") => Ok("TERM"),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("KILL") => Ok("KILL"),
        Some(other) => Err(LcError::BadValue(json!({"error": "bad_signal", "signal": other, "allowed": ["TERM", "KILL"]}))),
    }
}

/// SPEC §15.4. Guard rails (in the tree, never herdr／daemon, never a bot) live in `memproc::kill`,
/// which re-samples first and confirms the pid is still the same process in the same command (#526).
async fn kill_mem_process(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Result<Json<Value>, LcError> {
    let host = body.get("host").and_then(|v| v.as_str()).unwrap_or(crate::config::LOCAL_HOST).to_string();
    let pid = kill_pid(&body)?;
    let signal = kill_signal(&body)?;
    if app.hosts.get(&host).await.is_none() {
        return Err(unknown_host(&host));
    }
    match crate::memproc::kill(&app, &host, pid, signal).await {
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

#[cfg(test)]
mod mem_kill_pid_tests;

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
                    return Err(unknown_host(h));
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
                    let (codex, claude, grok, agy) = tokio::join!(
                        crate::runners::quota::refresh_codex(&app, &host),
                        crate::runners::quota_claude::refresh_claude(&app, &host),
                        crate::runners::quota_grok::refresh_grok(&app, &host),
                        crate::runners::quota_agy::refresh_agy(&app, &host),
                    );
                    for (kind, res) in [("codex", codex), ("claude", claude), ("grok", grok), ("agy", agy)] {
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
    let mut snap = crate::quota::snapshot(&app).await;
    crate::runners::reset_credit::annotate_held_bots(&app, &mut snap).await;
    let mut resp = Json(snap).into_response();
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
    match crate::runners::quota_claude::force_probe(&app, host, account).await {
        Ok((key, quota)) => Ok(Json(json!({"key": key, "quota": crate::quota::quota_value(&quota, false)}))),
        Err(crate::runners::quota_claude::ForceProbeError::UnknownAccount) => Err(LcError::NotFound("identity".into())),
        Err(crate::runners::quota_claude::ForceProbeError::NotInstalled) => {
            Err(LcError::conflict("claude_not_installed", json!({"host": host})))
        }
        Err(crate::runners::quota_claude::ForceProbeError::Failed(m)) => Err(LcError::Upstream(m)),
    }
}

#[derive(Deserialize)]
struct UseResetCreditBody {
    host: Option<String>,
    identity: Option<String>,
    credit_id: Option<String>,
}

/// 花一張重置券（User-only：花錢的決定只給使用者；bot／AGM 角色一律 403 `user_only`）。
/// 指定 `credit_id` 就用那張，沒指定挑最早到期的。`200 {outcome, key, resumed}`；沒券 `409 {reason: no_reset_credit}`。
async fn use_reset_credit(State(app): State<Arc<App>>, Json(body): Json<UseResetCreditBody>) -> Result<Json<Value>, LcError> {
    let host = body.host.filter(|h| !h.is_empty()).unwrap_or_else(|| LOCAL_HOST.to_string());
    if app.hosts.get(&host).await.is_none() {
        return Err(unknown_host(&host));
    }
    let identity = body.identity.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let credit_id = body.credit_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match crate::runners::reset_credit::redeem(&app, &host, identity, credit_id).await {
        Ok(r) => Ok(Json(json!({"outcome": r.outcome.as_str(), "key": r.key, "resumed": r.resumed}))),
        Err(crate::runners::reset_credit::RedeemError::NoCredit) => Err(LcError::conflict("no_reset_credit", json!({"host": host}))),
        Err(crate::runners::reset_credit::RedeemError::UnknownCredit) => Err(LcError::NotFound("credit".into())),
        Err(crate::runners::reset_credit::RedeemError::Rpc(e)) => Err(LcError::Upstream(format!("{e:#}"))),
    }
}

#[cfg(test)]
mod delete_identity_tests;

#[cfg(test)]
mod quota_refresh_tests;

#[derive(Deserialize)]
struct NewIdentity {
    name: String,
    kind: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    args: Vec<String>,
    /// 哪一台主機的身份（SPEC §16.2）。省略＝本機。
    #[serde(default)]
    host: Option<String>,
}

/// env 的 key 一律要是合法的環境變數名稱。以前不合法的照樣存，到用的時候才被 `tools::valid_env_name` 靜靜濾掉——
/// 少打一個 `=`、名字帶空白，身份／bot 的 env 就悄悄變成空的（身份＝用了預設帳號），沒有任何錯誤。存之前就擋，說出是哪個 key。
fn check_env_names(env: &BTreeMap<String, String>) -> Result<(), LcError> {
    match env.keys().find(|k| !crate::tools::valid_env_name(k)) {
        Some(k) => Err(LcError::Bad(format!(
            "env key `{k}` is not a valid variable name (letters, digits and _ only, not starting with a digit)"
        ))),
        None => Ok(()),
    }
}

/// 身份的家目錄變了（新增／刪除）：那個名字的模型清單快取作廢（#1349）。key＝`<host>/<kind>/<identity>`，host 與 kind 都不含 `/`。
/// 不分主機一律清：沒寫 host 的身份在遠端也生效；清掉只是下一次重抓。
async fn forget_identity_models(app: &Arc<App>, name: &str) {
    app.models_cache.lock().await.retain(|k, _| k.splitn(3, '/').nth(2) != Some(name));
}

/// 身份 env 的值不能留空（#1750）：`CLAUDE_CONFIG_DIR=` 這種空值存下來，啟動時等於沒設，身份悄悄用了預設帳號。
/// 只擋身份：bot 自己的 env 空值照舊（刻意清掉某個變數是合理的用法）。
fn check_identity_env_values(env: &BTreeMap<String, String>) -> Result<(), LcError> {
    match env.iter().find(|(_, v)| v.trim().is_empty()) {
        Some((k, _)) => Err(LcError::Bad(format!(
            "identity env `{k}` value is empty (an empty value is the same as not setting it, so the identity would silently use the default account); give it a value or remove the entry"
        ))),
        None => Ok(()),
    }
}

async fn create_identity(State(app): State<Arc<App>>, Json(b): Json<NewIdentity>) -> Result<Response, LcError> {
    crate::bot_input::check_env(&b.env)?;
    check_identity_env_values(&b.env)?;
    crate::bot_input::check_args(&b.args)?;
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
    forget_identity_models(&app, &b.name).await;
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

/// `host` 上名叫 `name` 的身份，實際生效的是不是那筆**沒寫 host** 的 config（`tools::merge_identities` 的規則）。
/// 那台還沒偵測過 shell 的 `ccN`（`shell` 是 `None`）時照「偵測完、那台沒有」算：寧可擋下一次刪除，
/// 也不要刪掉之後那台 bot 下次啟動才發現身份沒了（quota 修正 2026-09-16 轉來）。
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
    // 被刪那一筆的 kind（刪除前的設定裡找；找不到＝本來就沒有這筆，額度 key 不必動）（#1350）。
    let removed_kind = cfg.identities.iter().find(|i| i.name == name && i.host_or_local() == host).map(|i| i.kind.clone());
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
    // 要忘掉的額度 key（#1350）：`<kind>:<name>`，只在這個身份真的不再生效的那幾台。鎖放掉之後才清（清它會推事件）。
    let mut forget: Vec<(String, String)> = Vec::new();
    let forget_key = |h: &str| removed_kind.as_ref().map(|kind| crate::quota::quota_key(h, &crate::quota::quota_base(kind, Some(&name))));
    {
        let mut tools = app.tools.lock().await;
        if let Some(key) = forget_key(&host).filter(|_| !tools.contains_key(&host)) {
            forget.push((host.clone(), key));
        }
        for (h, ht) in tools.iter_mut() {
            let has_own = cfg_after.identities.iter().any(|i| i.name == name && i.host_or_local() == h.as_str());
            if !(*h == host || (removing_hostless && !has_own)) {
                continue;
            }
            // 同名的 shell alias（`ccN`）接手：那把 key 仍然有效，不清。
            if !ht.shell_identities.iter().any(|i| i.name == name) {
                if let Some(key) = forget_key(h) {
                    forget.push((h.clone(), key));
                }
            }
            ht.identities.remove(&name);
            if let Some(i) = ht.shell_identities.iter().find(|i| i.name == name) {
                let dir = i.env.get("CLAUDE_CONFIG_DIR").cloned();
                ht.identities.insert(name.clone(), crate::tools::IdentityInfo::shell(&i.name, &i.kind, dir));
            }
        }
    }
    forget.sort();
    forget.dedup();
    for (h, key) in forget {
        let existed = app.quotas.lock().await.contains_key(&key);
        crate::quota::forget(app.as_ref(), &key).await;
        if existed {
            app.emit("quota_updated", json!({"kind": key, "host": h, "quota": null})).await;
        }
    }
    forget_identity_models(&app, &name).await;
    app.emit("identities_changed", json!({})).await;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

/// `GET /api/bots/{id}/api-usage`：claude-api bot 的累計花費與 token 用量（唯讀，只讀 `api_bot_attempts`，不含 prompt／回覆）。
/// 金額都是整數微美元：`recorded`＝已結算的估價；`reserved`＝已預留、還沒回的；`unknown`＝結果不明、保留預留額（不捏造成零）。
/// 命中率＝cache_read ÷ (input＋cache_read＋cache_creation)（Anthropic 的 `input_tokens` 不含快取的部分）。
async fn api_bot_usage(State(app): State<Arc<App>>, Path(id): Path<String>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    if principal != RequestPrincipal::User { return Err(bot_user_only()); }
    let bot = db::bot(&app.db, &id).await.map_err(any_err)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.kind != crate::config::API_BOT_KIND {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "not_api_bot"})));
    }
    type Totals = (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64);
    let t: Totals = sqlx::query_as(
        "SELECT COUNT(*),
                COALESCE(SUM(state='recorded'),0), COALESCE(SUM(state='reserved'),0), COALESCE(SUM(state='unknown'),0), COALESCE(SUM(state='failed'),0),
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(estimated_microusd,0) END),0),
                COALESCE(SUM(CASE WHEN state='reserved' THEN reserved_microusd END),0),
                COALESCE(SUM(CASE WHEN state='unknown' THEN COALESCE(estimated_microusd,reserved_microusd) END),0),
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(json_extract(usage_json,'$.input_tokens'),0) END),0),
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(json_extract(usage_json,'$.cache_read_input_tokens'),0) END),0),
                COALESCE(SUM(CASE WHEN state='recorded' THEN COALESCE(json_extract(usage_json,'$.cache_creation_input_tokens'),0) END),0)
         FROM api_bot_attempts WHERE bot_id=?",
    )
    .bind(&id).fetch_one(&app.db).await.map_err(any_err)?;
    let output: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(COALESCE(json_extract(usage_json,'$.output_tokens'),0)),0) FROM api_bot_attempts WHERE bot_id=? AND state='recorded'",
    )
    .bind(&id).fetch_one(&app.db).await.map_err(any_err)?;
    let last: Option<(Option<String>, String, Option<String>, String)> = sqlx::query_as(
        "SELECT actual_speed, requested_speed, usage_json, created_at FROM api_bot_attempts WHERE bot_id=? AND state='recorded' ORDER BY created_at DESC, ordinal DESC LIMIT 1",
    )
    .bind(&id).fetch_optional(&app.db).await.map_err(any_err)?;
    let (input, cache_read, cache_creation) = (t.8, t.9, t.10);
    let denom = input + cache_read + cache_creation;
    let hit_rate = if denom > 0 { json!(cache_read as f64 / denom as f64) } else { Value::Null };
    let last_json = last.map(|(actual, requested, usage, at)| {
        let u: Value = usage.and_then(|u| serde_json::from_str(&u).ok()).unwrap_or(Value::Null);
        let g = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
        json!({"speed": actual.unwrap_or(requested), "at": at, "input_tokens": g("input_tokens"), "output_tokens": g("output_tokens"),
               "cache_read_input_tokens": g("cache_read_input_tokens"), "cache_creation_input_tokens": g("cache_creation_input_tokens")})
    });
    Ok(Json(json!({
        "bot_id": id,
        "attempts": {"total": t.0, "recorded": t.1, "reserved": t.2, "unknown": t.3, "failed": t.4},
        "cost_microusd": {"recorded": t.5, "reserved": t.6, "unknown": t.7},
        "tokens": {"input": input, "output": output, "cache_read": cache_read, "cache_creation": cache_creation},
        "cache_hit_rate": hit_rate,
        "last": last_json,
        "turns": crate::api_bot_turn_cost::recent_turns(&app.db, &id).await.map_err(any_err)?,
    })))
}

async fn api_fresh_target(State(app): State<Arc<App>>, Path(id): Path<String>, Extension(principal): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    if principal != RequestPrincipal::User { return Err(bot_user_only()); }
    use crate::lifecycle::api_bot_port::{ApiBotPort, ApiFreshAvailability};
    let value = match app.api_fresh_target(&id).await {
        ApiFreshAvailability::Available(target) => json!({"availability":"available","session_id":target.session_id,"generation":target.generation}),
        ApiFreshAvailability::None => json!({"availability":"none"}),
        ApiFreshAvailability::Unavailable => json!({"availability":"unavailable"}),
    };
    Ok(Json(value))
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

#[derive(Deserialize, Default)]
struct NativeStartQuery {
    resume: Option<String>,
    session: Option<String>,
    api_expected_session_id: Option<String>,
    api_expected_generation: Option<String>,
}

async fn start_bot_http(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<NativeStartQuery>, Extension(principal): Extension<RequestPrincipal>) -> Result<Response, LcError> {
    let common = StartQuery { resume: q.resume, session: q.session };
    let opts = resume_opts(&common)?;
    let kind = db::bot(&app.db, &id).await.map_err(|_| LcError::Upstream("bot_kind_unconfirmed".into()))?.map(|b| b.kind);
    if kind.as_deref() == Some("claude-api") {
        use crate::lifecycle::api_bot_port::{ApiBotPort, ApiFreshTarget, ApiNativeStartError, ApiNativeStartMode};
        if opts.resume_session.is_some() { return Err(LcError::Bad("api_native_session_override_unsupported".into())); }
        let fresh = common.resume.as_deref().is_some_and(|v| v.trim() == "fresh");
        let mode = if fresh {
            if principal != RequestPrincipal::User { return Err(bot_user_only()); }
            let session_id = q.api_expected_session_id.as_ref().filter(|s| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))).ok_or_else(|| LcError::Bad("api_fresh_target_required".into()))?;
            let generation = q.api_expected_generation.as_deref().and_then(|g| g.parse::<i64>().ok()).filter(|g| *g > 0).ok_or_else(|| LcError::Bad("api_fresh_target_required".into()))?;
            ApiNativeStartMode::Fresh(ApiFreshTarget { session_id: session_id.clone(), generation })
        } else {
            if q.api_expected_session_id.is_some() || q.api_expected_generation.is_some() { return Err(LcError::Bad("api_fresh_target_requires_fresh".into())); }
            ApiNativeStartMode::Resume { required: opts.resume_required }
        };
        refuse_child_restart(&app, &id).await?;
        let result = app.api_native_start(&id, mode).await.map_err(|error| match error {
            ApiNativeStartError::Unconfirmed => LcError::Upstream("api_native_start_unconfirmed".into()),
            ApiNativeStartError::ClearedButStartUnconfirmed => LcError::Upstream("api_fresh_cleared_start_unconfirmed".into()),
            ApiNativeStartError::Refused(reason) => LcError::conflict(reason, json!({"reason": reason})),
        })?;
        return Ok((StatusCode::OK, Json(json!({"run_id":result.run_id,"resumed":result.resumed}))).into_response());
    }
    start_bot(State(app), Path(id), Query(common)).await
}

async fn start_bot(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<StartQuery>) -> Result<Response, LcError> {
    // 參數不合法要在任何副作用（包括叫醒睡著的 bot）之前就 400，與 restart_bot 同順序。
    let opts = resume_opts(&q)?;
    refuse_child_restart(&app, &id).await?;
    // 脫離請求（#1356）：agent 起來要等 60 秒以上，client 斷線時啟動照樣做完，不會停在 starting。
    let body = detach(&id, {
        let (app, id) = (app.clone(), id.clone());
        async move {
            // 被 AGM 因為閒置收起來的（§6.11）一律走續接：使用者按「啟動」要的是把剛剛那顆帶著對話的
            // bot 叫回來，不是開一段新的空白對話。
            if crate::supervisor::idle_sleep::wake(&app, &id, "使用者按了啟動")
                .await
                .map_err(|e| LcError::Upstream(e.to_string()))?
            {
                // 叫醒接不回時會退回開新對話（`idle_sleep::wake`）：照實際結果回，不寫死 `true`（issue #107）。
                return match db::active_run(&app.db, &id).await.map_err(any_err)? {
                    Some(run) => started_json(&app, &run.id, &lifecycle::StartOpts { resume_native: true, ..Default::default() }).await,
                    None => Ok(json!({"run_id": null, "resumed": false})),
                };
            }
            let run_id = lifecycle::start_bot_with(&app, &id, opts.clone()).await?;
            started_json(&app, &run_id, &opts).await
        }
    })
    .await?;
    Ok((StatusCode::OK, Json(body)).into_response())
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
    let plan = crate::runners::bulk_restart::spawn(&app).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok((StatusCode::ACCEPTED, Json(plan)).into_response())
}

async fn restart_bot(State(app): State<Arc<App>>, Path(id): Path<String>, Query(q): Query<StartQuery>) -> Result<Response, LcError> {
    let opts = resume_opts(&q)?;
    refuse_child_restart(&app, &id).await?;
    // 連點兩下／兩個鈕／兩個分頁：同一顆 bot 同樣的重啟**仍在進行中**，後來的不再重啟一次，等它做完、回同一個結果（`restart_coalesce`）。
    // 做完之後再來的是新的一次（改了設定馬上重啟要套用新設定）。
    let lead = match crate::restart_coalesce::admit(&id, &format!("{opts:?}")).await {
        crate::restart_coalesce::Admission::Joined(run_id) => {
            let mut body = started_json(&app, &run_id, &opts).await?;
            body["coalesced"] = json!(true);
            return Ok((StatusCode::OK, Json(body)).into_response());
        }
        crate::restart_coalesce::Admission::Lead(lead) => lead,
    };
    // 脫離請求（#1355）：stop 與 start 之間 client 斷線，重啟照樣做完，不會停在 stopped、intent 停在 pending。
    // `lead` 也一起搬進去：leader 斷線時 `Joined` 的請求還是拿得到結果。
    // 出錯就直接丟掉 `lead`（等著的請求會自己重試），不是假裝成功。
    let body = detach(&id, {
        let (app, id) = (app.clone(), id.clone());
        async move {
            let run_id = lifecycle::restart_bot_with(&app, &id, opts.clone()).await?;
            lead.finish(&run_id);
            let body = started_json(&app, &run_id, &opts).await?;
            app.emit("bot_changed", json!({"bot_id": id})).await;
            Ok(body)
        }
    })
    .await?;
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
    // 一路讀（bot、子孫、pane、重啟 intent）最後才寫 token：deferred 的話讀完之後別的 writer 一 commit 就 517，輪替被拒（#831）。
    // 交易裡只有 DB 與記憶體裡的 fence，不等 herdr。
    let mut tx = db::begin_write(&app.db).await.map_err(any_err)?;
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
    let rotation_fence = match crate::runners::credential_spawn::RotationFence::begin(&app, &id) {
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
                crate::runners::restart_intents::ROTATION_INTENT_TTL_SECS,
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
    if let crate::runners::restart_intents::Outcome::Retry(why) = crate::runners::restart_intents::drive_once(&app, &intent_id).await {
        crate::runners::restart_intents::retry_later(&app, &intent_id);
        return Err(pending_error(why));
    }
    let intent = match crate::intents::get(&app.db, &intent_id).await {
        Ok(Some(intent)) => intent,
        Ok(None) => {
            crate::runners::restart_intents::retry_later(&app, &intent_id);
            return Err(pending_error("restart intent could not be read after credential commit".into()));
        }
        Err(e) => {
            crate::runners::restart_intents::retry_later(&app, &intent_id);
            return Err(pending_error(format!("restart intent could not be read after credential commit: {e:#}")));
        }
    };
    if intent.status != "done" {
        let open = matches!(intent.status.as_str(), "pending" | "running");
        if intent.status == "pending" {
            crate::runners::restart_intents::retry_later(&app, &intent_id);
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

/// UI-only actions require the actual user principal: a bot's hook token proves which bot called,
/// but does not authorize host administration, reading a user's shell, or raw pane control.
fn require_user(principal: &RequestPrincipal) -> Result<(), LcError> {
    if *principal == RequestPrincipal::User {
        return Ok(());
    }
    Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})))
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
        lifecycle::RelaySrc::daemon_notice(Some(crate::agent_relay::DAEMON_SENDER)),
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
/// daemon-swap 服務身份自己開一筆立即核准的 restart 單再走**同一個** `maintenance::acquire`：
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
    if !crate::supervisor::maintenance::valid_commit(commit) {
        return Err(LcError::Bad("commit 要是 7～64 碼十六進位的 git sha".into()));
    }
    let ttl = b.ttl_secs.unwrap_or(900);
    let actor = format!("service({})", crate::service_auth::DAEMON_SWAP);
    // 同一張核准一輪輪沿用，升級計時才接得下去（`swap_window`）。
    let id = crate::swap_window::approval_for(&app, owner, commit, &actor).await.map_err(any_err)?;
    match crate::supervisor::maintenance::acquire(&app, "restart", owner, &id, Some(commit), ttl, true, &[]).await {
        Ok(v) => {
            let auto_escalated = v.pointer("/safety/escalated").and_then(Value::as_bool).unwrap_or(false);
            crate::deploy_wait::observe(&app, owner, &id, commit, Ok(auto_escalated)).await;
            Ok(Json(v))
        }
        Err(e) => {
            // 等太久要告訴使用者、讓使用者調度（SPEC §18.10，使用者 2026-10-04）。
            crate::deploy_wait::observe(&app, owner, &id, commit, Err(&e)).await;
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
        lifecycle::RelaySrc::daemon_notice(Some(crate::agent_relay::DAEMON_SENDER)),
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
    // 脫離請求（#1357）：送完 ctrl+c 之後 client 斷線，停止照樣記成 stopped，不會卡在 stopping 或被收成 exited。
    let had = detach(&id, {
        let (app, id) = (app.clone(), id.clone());
        async move { lifecycle::stop_bot(&app, &id).await }
    })
    .await?;
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

#[derive(Deserialize)]
struct KeepWarmSkipIn {
    skip: bool,
}

/// `POST /api/bots/{id}/keep-warm/skip`：「不用保溫」鈕（主力的 claude；SPEC §6.5k）。body `{"skip": bool}`，回 `{"keep_warm_skip": bool}`。
async fn keep_warm_skip_bot(State(app): State<Arc<App>>, Path(id): Path<String>, Json(body): Json<KeepWarmSkipIn>) -> Result<Response, LcError> {
    let skip = crate::primary_keep_warm::skip_route(&app, &id, body.skip).await?;
    Ok(Json(json!({"keep_warm_skip": skip})).into_response())
}

/// `POST /api/bots/{id}/compact`：對閒著的 bot 送 `/compact`（網頁 context 旁的「壓縮」鈕，2026-10-04 使用者）。
async fn compact_bot(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    let out = lifecycle::compact(&app, &id, None).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// Retrofit for bots started before one-bot-one-tab; the bot keeps running.
async fn move_bot_pane_to_tab(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Response, LcError> {
    lifecycle::move_pane_to_own_tab(&app, &id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

#[derive(Deserialize)]
pub(crate) struct PromptIn {
    /// `submit_draft` 時不用帶（送的是框裡那段）；其他時候空字串照舊 400。
    #[serde(default)]
    text: String,
    client_request_id: Option<String>,
    #[serde(default)]
    expect_result: bool,
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
    /// 只對分享用 bot 有意義（SPEC §20）：擁有者送的訊息分享頁本來就看不到，這一回合 bot 的回覆預設也不給看；
    /// 帶 `true`＝這一則觸發的回覆照樣出現在分享頁（例如替 bot 補一句要對 end user 說的話）。
    #[serde(default)]
    share_reply_visible: bool,
    #[serde(default)]
    track: Option<String>,
    /// 自動／群集派工（shim 解析 `bot:cluster:<名稱>` 或 `bot:auto:<tier>[:<track>]` 後帶來）：只對 `expect_result` 有意義。
    #[serde(default)]
    cluster: Option<String>,
}

#[derive(Deserialize)]
struct RelayRequestsQuery {
    from: Option<String>,
    to: Option<String>,
    open: Option<String>,
}

#[derive(Deserialize)]
struct RelayResolveQuery { target: String }

async fn resolve_relay_target(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Query(q): Query<RelayResolveQuery>,
) -> Result<Json<Value>, LcError> {
    let (RequestPrincipal::Bot(sender) | RequestPrincipal::Child { child_id: sender, .. }) = principal else {
        return Err(LcError::Bad("relay resolution requires a bot principal".into()));
    };
    if let Some(name) = q.target.strip_prefix("bot:cluster:") {
        return resolve_cluster_target(&app, &sender, name).await;
    }
    if let Some(selector) = q.target.strip_prefix("bot:auto:") {
        return resolve_auto_target(&app, &sender, selector).await;
    }
    if let Some(id) = q.target.strip_prefix("bot:") {
        if id == sender { return Err(LcError::Bad("relay_self".into())); }
        let bot = db::bot(&app.db, id).await.map_err(any_err)?
            .filter(|bot| bot.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("relay target".into()))?;
        check_relay_policy(&app, &sender, &bot.id).await?;
        let run = db::active_run(&app.db, &bot.id).await.map_err(any_err)?;
        return Ok(Json(json!({"to_bot_id": bot.id, "to_run_id": run.as_ref().map(|r| &r.id), "to_agent": run.as_ref().and_then(|r| r.agent_name.as_ref())})));
    }
    let host = db::bot_host(&app.db, &sender).await.map_err(any_err)?;
    let fallback = app.session_for_host(&host).await.ok_or_else(|| LcError::NotFound("host session".into()))?;
    let sender_run = db::active_run(&app.db, &sender).await.map_err(any_err)?;
    let session = sender_run.as_ref().and_then(|r| r.herdr_session.as_deref()).unwrap_or(&fallback);
    let mut candidates: Vec<(String, Option<String>, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT b.id,r.id,r.agent_name,r.pane_id,b.name FROM runs r JOIN bots b ON b.id=r.bot_id JOIN projects p ON p.id=b.project_id WHERE r.state='running' AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.handed_off_to IS NULL AND p.host=? AND COALESCE(r.herdr_session,?)=? AND (r.pane_id=? OR r.agent_name=? OR b.name=?)"
    ).bind(&host).bind(&fallback).bind(session).bind(&q.target).bind(&q.target).bind(&q.target)
        .fetch_all(&app.db).await.map_err(any_err)?;
    // Display names are project-scoped and remain usable when the member is stopped.
    let members: Vec<(String, Option<String>, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT b.id,r.id,r.agent_name,r.pane_id,b.name FROM bots b JOIN projects p ON p.id=b.project_id LEFT JOIN runs r ON r.bot_id=b.id AND r.state='running' WHERE b.project_id=(SELECT project_id FROM bots WHERE id=?) AND b.name=? AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.handed_off_to IS NULL"
    ).bind(&sender).bind(&q.target).fetch_all(&app.db).await.map_err(any_err)?;
    for member in members {
        if !candidates.iter().any(|candidate| candidate.0 == member.0) { candidates.push(member); }
    }
    if candidates.is_empty() {
        // 跨主機：只用 agent 名稱（帶 $AM_AGENT_NAME 前綴，全域應唯一）找；pane id 與 bot 名稱各主機自己的，不做跨主機比對。
        candidates = sqlx::query_as(
            "SELECT b.id,r.id,r.agent_name,r.pane_id,b.name FROM runs r JOIN bots b ON b.id=r.bot_id JOIN projects p ON p.id=b.project_id WHERE r.state='running' AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.handed_off_to IS NULL AND r.agent_name=?"
        ).bind(&q.target).fetch_all(&app.db).await.map_err(any_err)?;
    }
    let mut rows = Vec::new();
    let mut denied_reason: Option<serde_json::Value> = None;
    for candidate in candidates {
        match crate::relay_policy::relay_prompt_allowed(&app, &sender, &candidate.0, crate::relay_policy::Admission::NewRequest).await {
            Ok(()) => rows.push(candidate),
            Err(crate::relay_policy::RelayDenied::Denied(reason)) => { denied_reason.get_or_insert(json!({"error":"forbidden","reason":reason})); },
            Err(crate::relay_policy::RelayDenied::MemberDisabled(message)) => { denied_reason.get_or_insert(crate::relay_policy::member_disabled_body(&message)); },
            Err(crate::relay_policy::RelayDenied::Unavailable(e)) => return Err(LcError::Upstream(e)),
        }
    }
    if rows.is_empty() {
        // 候選存在但全被政策擋掉：回 403 帶原因，不要吞成 404。
        return Err(match denied_reason {
            Some(body) => LcError::Forbidden(body),
            None => LcError::NotFound("relay target".into()),
        });
    }
    if rows.len() != 1 {
        return Err(LcError::Conflict(json!({"error":"ambiguous_relay_target", "candidates":rows.iter().map(|r| json!({"name":r.4,"bot_id":r.0})).collect::<Vec<_>>() })));
    }
    let (bot, run, agent, pane, _) = &rows[0];
    Ok(Json(json!({"to_bot_id":bot,"to_run_id":run,"to_agent":agent.as_ref().or(pane.as_ref())})))
}

/// `bot:cluster:<名稱>`：manager（或代班中的副管理者）可對群集派工；專家在「專家可直接派工給執行者」開關開著時可挑群集裡本專案的頂層執行者（見 `expert_cluster::Scope`）。
/// 挑閒置、manager_use=1 的成員，都忙就先挑一顆（讓請求照一般流程建立），並回 `cluster_idle=false`——送出時 daemon 會把請求留在 pending，
/// 等第一個閒下來的成員接手。停著的成員（範圍內）管理者與專家都會自動啟動。
pub(crate) async fn resolve_cluster_target(app: &Arc<App>, sender: &str, name: &str) -> Result<Json<Value>, LcError> {
    let from = db::bot(&app.db, sender).await.map_err(any_err)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("relay sender".into()))?;
    let Some(scope) = crate::expert_cluster::sender_scope(app, &from).await.map_err(any_err)? else {
        return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"cluster_manager_only"})));
    };
    let clusters = crate::expert_cluster::project_clusters(app, &from.project_id).await.map_err(any_err)?;
    let full = clusters.iter().find(|c| c.name == name).ok_or_else(|| LcError::NotFound("cluster".into()))?;
    let cluster = full.restricted(&scope);
    if cluster.members.is_empty() {
        return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"cluster_not_dispatchable","message":"這個群集沒有你可以直接派工的執行者：請改送管理者"})));
    }
    let chosen = match cluster.pick_best(None) {
        Some(m) => Some((m.id.clone(), true)),
        None => {
            // 沒有閒置成員：先啟動一顆停著的（群集已有啟動中的就不再啟動），請求留 pending 等它閒下來。
            cluster.pick_to_start().map(|m| m.id.clone())
                .or_else(|| cluster.members.iter().filter(|m| m.manager_use && !m.quota_blocked && (m.running || m.starting)).map(|m| m.id.clone()).next())
                .or_else(|| cluster.members.iter().filter(|m| m.manager_use && !m.quota_blocked).map(|m| m.id.clone()).next())
                .map(|id| (id, false))
        }
    };
    let Some((bot_id, idle)) = chosen else { return Err(LcError::Forbidden(json!({"error":"member_not_dispatchable","message":"群集沒有可派工的成員：成員可能已停用或額度不足","sent":false}))) };
    check_relay_policy(app, sender, &bot_id).await?;
    let run = db::active_run(&app.db, &bot_id).await.map_err(any_err)?;
    Ok(Json(json!({"to_bot_id": bot_id, "to_run_id": run.as_ref().map(|r| &r.id), "to_agent": run.as_ref().and_then(|r| r.agent_name.as_ref()), "cluster": name, "cluster_idle": idle})))
}

/// `bot:auto:<tier>[:<track>]`：依 tier 和共用成員評分挑選本專案 executor，包含不成群的單顆。
pub(crate) async fn resolve_auto_target(app: &Arc<App>, sender: &str, selector: &str) -> Result<Json<Value>, LcError> {
    let Some(target) = crate::expert_cluster::parse_auto_target(&format!("auto:{selector}")) else {
        return Err(LcError::Bad("auto tier 必須是 cheap、standard 或 strong".into()));
    };
    let from = db::bot(&app.db, sender).await.map_err(any_err)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("relay sender".into()))?;
    let Some(scope) = crate::expert_cluster::sender_scope(app, &from).await.map_err(any_err)? else {
        return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"cluster_manager_only"})));
    };
    let mut members: Vec<_> = crate::expert_cluster::project_executors(app, &from.project_id).await.map_err(any_err)?
        .into_iter().filter(|m| crate::expert_cluster::tier_of(&m.kind, m.model.as_deref()) == target.tier)
        .collect();
    members.retain(|m| scope.allows(&m.id));
    if members.is_empty() {
        return Err(LcError::NotFound("auto executor".into()));
    }
    let chosen = crate::expert_cluster::pick_best_member(&members, target.track.as_deref()).map(|m| (m.id.clone(), true));
    let (bot_id, idle) = if let Some(chosen) = chosen {
        chosen
    } else {
        let candidate = crate::expert_cluster::pick_to_start_member(&members).map(|m| m.id.clone());
        let fallback = candidate
            .or_else(|| members.iter().filter(|m| m.manager_use && !m.quota_blocked && (m.running || m.starting)).min_by(|a, b| a.id.cmp(&b.id)).map(|m| m.id.clone()))
            .or_else(|| crate::expert_cluster::pick_to_start_member(&members).map(|m| m.id.clone()))
            .or_else(|| members.iter().filter(|m| m.manager_use && !m.quota_blocked).min_by(|a, b| a.id.cmp(&b.id)).map(|m| m.id.clone()));
        let Some(id) = fallback else { return Err(LcError::Forbidden(json!({"error":"member_not_dispatchable","message":"沒有可派工的成員：成員可能已停用或額度不足","sent":false}))) };
        (id, false)
    };
    check_relay_policy(app, sender, &bot_id).await?;
    let run = db::active_run(&app.db, &bot_id).await.map_err(any_err)?;
    let cluster = format!("auto:{}{}", target.tier, target.track.as_deref().map(|t| format!(":{t}")).unwrap_or_default());
    Ok(Json(json!({"to_bot_id": bot_id, "to_run_id": run.as_ref().map(|r| &r.id), "to_agent": run.as_ref().and_then(|r| r.agent_name.as_ref()), "cluster": cluster, "cluster_idle": idle})))
}

async fn check_relay_policy(app: &Arc<App>, from: &str, to: &str) -> Result<(), LcError> {
    check_relay_basis(app, from, to).await.map(|_| ())
}

/// 同 [`check_relay_policy`]，另回這次放行的依據（建 turn 時記進 `messages.relay_auth`，見 `relay_policy::relay_prompt_basis`）。
async fn check_relay_basis(app: &Arc<App>, from: &str, to: &str) -> Result<Option<String>, LcError> {
    match crate::relay_policy::relay_prompt_basis(app, from, to, crate::relay_policy::Admission::NewRequest).await {
        Ok(basis) => Ok(basis),
        Err(crate::relay_policy::RelayDenied::MemberDisabled(message)) => Err(LcError::Forbidden(crate::relay_policy::member_disabled_body(&message))),
        Err(crate::relay_policy::RelayDenied::Denied(reason)) => Err(LcError::Forbidden(json!({"error":"forbidden","reason":reason}))),
        Err(crate::relay_policy::RelayDenied::Unavailable(reason)) => Err(LcError::Upstream(reason)),
    }
}

async fn list_relay_requests(
    State(app): State<Arc<App>>,
    Extension(principal): Extension<RequestPrincipal>,
    Query(q): Query<RelayRequestsQuery>,
) -> Result<Json<Value>, LcError> {
    let caller = match &principal { RequestPrincipal::Bot(id) | RequestPrincipal::Child { child_id: id, .. } => Some(id.as_str()), _ => None };
    let rows: Vec<(String, String, String, Option<String>, Option<String>, String, String, Option<String>, Option<String>, Option<String>, Option<String>, i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id,from_bot_id,to_bot_id,to_run_id,to_turn_id,state,created_at,answered_at,notified_at,notice_turn_id,processed_at,attempts,error,redirected_from FROM relay_requests WHERE (? IS NULL OR from_bot_id=? OR to_bot_id=?) AND (? IS NULL OR from_bot_id=?) AND (? IS NULL OR to_bot_id=?) AND (?=0 OR state IN ('pending','delivered')) ORDER BY created_at DESC"
    ).bind(caller).bind(caller).bind(caller).bind(&q.from).bind(&q.from).bind(&q.to).bind(&q.to)
        .bind(i64::from(q.open.as_deref() == Some("1"))).fetch_all(&app.db).await.map_err(any_err)?;
    let cfg = crate::runners::relay_requests::FollowCfg::from_env();
    let mut out = Vec::with_capacity(rows.len());
    for (id,from,to,run,turn,state,created,answered,notified,notice_turn,processed,attempts,error,redirected_from) in rows {
        let followup = if state == "notified" { crate::runners::relay_requests::followup_view(&app, &id, &from, &to, turn.as_deref(), notified.as_deref(), &cfg).await } else { None };
        out.push(json!({
            "id":id,"from_bot_id":from,"to_bot_id":to,"to_run_id":run,"to_turn_id":turn,"state":state,
            "created_at":created,"answered_at":answered,"notified_at":notified,"notice_turn_id":notice_turn,"processed_at":processed,"attempts":attempts,"error":error,"redirected_from":redirected_from,
            "followup":followup
        }));
    }
    Ok(Json(json!({"requests": out })))
}

/// Start a stopped expert member only for an authorized project manager or acting deputy.
/// The start task outlives the HTTP wait: cancellation must not strand a half-created run.
async fn ensure_relay_member_ready(app: &Arc<App>, sender: &str, target: &str) -> Result<(), LcError> {
    let run = db::active_run(&app.db, target).await.map_err(any_err)?;
    if run.as_ref().is_some_and(|r| r.state == "running") { return Ok(()); }
    let to = db::bot(&app.db, target).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("relay target".into()))?;
    let from = db::bot(&app.db, sender).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("relay sender".into()))?;
    let stopped = |reason: &str| LcError::Conflict(json!({"error":"member_stopped", "message":format!("{} 未啟動，且無法自動啟動：{}", to.name, reason), "sent":false}));
    if to.manager_use == 0 {
        return Err(LcError::Forbidden(json!({"error":"member_not_dispatchable", "message":format!("{} 未啟動，且無法自動啟動：成員已停用（manager_use=false）", to.name), "sent":false})));
    }
    let project = db::project(&app.db, &from.project_id).await.map_err(any_err)?.ok_or_else(|| stopped("專案不存在"))?;
    if from.project_id != to.project_id || project.mode != "expert" { return Err(stopped("不屬於同一個專家專案")); }
    if to.parent_bot_id.is_some() || to.managed_by != "user" || !matches!(to.role.as_deref(), Some("executor" | "advisor" | "specialist")) {
        return Err(stopped("目標不是可派工的頂層成員"));
    }
    // 自動啟動的權限與派工範圍相同：管理者（或代班副管理者）不限；專家只限它可直接派工的頂層執行者（開關關著就沒有範圍）。
    let scope = crate::expert_cluster::sender_scope(app, &from).await.map_err(any_err)?;
    if !scope.as_ref().is_some_and(|s| s.allows(target)) {
        return Err(stopped("不在你可自動啟動的範圍內（管理者／代班副管理者，或專家派工開關開著時的本專案頂層執行者）"));
    }
    if run.as_ref().is_some_and(|r| r.state != "starting" && r.state != "running") { return Err(stopped("成員正在停止")); }
    let app2 = app.clone();
    let id = target.to_string();
    let name = to.name.clone();
    let mut task = tokio::spawn(Box::pin(async move {
        if db::active_run(&app2.db, &id).await.map_err(|e| e.to_string())?.is_none() {
            if let Err(e) = Box::pin(lifecycle::start_bot(&app2, &id)).await {
                // Another request can win the start lock while we wait for it.
                if !db::active_run(&app2.db, &id).await.map_err(|e| e.to_string())?.is_some_and(|r| r.state == "starting" || r.state == "running") {
                    return Err(format!("{:?}", e));
                }
            }
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if tokio::time::Instant::now() >= deadline { return Err("等待就緒逾時".to_string()); }
            match db::active_run(&app2.db, &id).await.map_err(|e| e.to_string())? {
                Some(r) if r.state == "running" && matches!(r.agent_status.as_str(), "idle" | "done") => return Ok(()),
                Some(r) if r.state == "starting" || r.state == "running" => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
                _ => return Err("啟動中的 run 已結束".to_string()),
            }
        }
    }));
    let result = tokio::time::timeout(std::time::Duration::from_secs(25), &mut task).await;
    match result {
        Ok(Ok(Ok(()))) => { check_relay_policy(app, sender, target).await?; Ok(()) },
        Ok(result) => Err(LcError::Unavailable(json!({"error":"member_start_failed", "message":format!("{} 無法自動啟動：{:?}", name, result), "sent":false}))),
        Err(_) => Err(LcError::Unavailable(json!({"error":"member_start_timeout", "message":format!("{} 自動啟動等待就緒逾時；啟動可能仍在進行", name), "sent":false}))),
    }
}

pub(crate) async fn prompt_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<PromptIn>,
) -> Result<Response, LcError> {
    let bot_record = db::bot(&app.db, &id).await.map_err(|_| LcError::Upstream("bot_kind_unconfirmed".into()))?;
    let kind = bot_record.as_ref().map(|b| b.kind.as_str());
    if kind == Some("claude-api") {
        let is_advisor = bot_record.as_ref().and_then(|b| b.role.as_deref()) == Some("advisor");
        let is_bot_dispatch = matches!(&principal, RequestPrincipal::Bot(_) | RequestPrincipal::Child { .. }) || b.relay_from.is_some();
        if is_bot_dispatch && !is_advisor {
            let name = bot_record.as_ref().map(|b| b.name.as_str()).unwrap_or(&id);
            return Err(LcError::Forbidden(json!({
                "error": "member_not_dispatchable",
                "message": format!("{name} 是 claude-api（純 API 模型），不能接受 bot 派工"),
                "sent": false
            })));
        }
        if is_bot_dispatch && is_advisor && !b.expect_result {
            return Err(LcError::Bad("api_native_prompt_unsupported: advisor requires expect_result".into()));
        }
        if b.send_now || b.clear_draft || b.submit_draft || b.share_reply_visible || !b.attachments.is_empty() || b.ack || b.reply_to.is_some() {
            return Err(LcError::Bad("api_native_prompt_unsupported".into()));
        }
        if !is_advisor && (b.start_if_stopped || b.queue_if_busy) {
            return Err(LcError::Bad("api_native_prompt_unsupported".into()));
        }
    }
    // 把回覆公開到分享頁只有使用者（UI token）能決定：Bot／Service 帶旗標一律 403，且不送出（issue #855）。
    if b.share_reply_visible && principal != RequestPrincipal::User {
        return Err(bot_user_only());
    }
    // #990：`start_if_stopped` 會替對方開機、`send_now` 會打斷對方的回合，等同 start／interrupt（#804 的政策）：
    // 只給 User 與登記的 AGM 角色。一般 Bot 帶這兩個旗標在任何副作用前就 403（跨 bot 的 prompt 不能繞過 start／interrupt）。
    if (b.start_if_stopped || b.send_now) && principal != RequestPrincipal::User {
        let is_role = match &principal {
            RequestPrincipal::Bot(caller) => crate::supervisor::roles::role_of_bot(&app.db, caller).await.map_err(any_err)?.is_some(),
            _ => false,
        };
        if !is_role {
            return Err(bot_user_only());
        }
    }
    // #923：冪等鍵進 turns 的唯一索引、再隨回應與日誌回出去，先驗長度與字元（同 fork 的做法）。
    let given_crid = match b.client_request_id.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => Some(crate::request_id::validate("client_request_id", c)?),
        None => None,
    };
    let crid = given_crid.clone().unwrap_or_else(db::ulid);
    if crid.starts_with("relay-request:") || crid.starts_with("relay-result:") || crid.starts_with(crate::runners::relay_requests::WITHDRAWN_PREFIX) {
        return Err(LcError::Bad("client_request_id uses a reserved daemon prefix".into()));
    }
    // `relay_from` is metadata, never an alternate principal. A proven bot that omits it is
    // attributed to the id already authenticated by middleware. A User request claiming a bot
    // without that bot's token is refused (#410 ended #339's unsigned compatibility branch).
    let relay_claim = b.relay_from.as_deref().map(str::trim).filter(|value| !value.is_empty());
    let relay = match &principal {
        RequestPrincipal::Child { child_id, parent_bot_id } => {
            let child = db::bot(&app.db, child_id).await.map_err(any_err)?;
            let still_owned = child.is_some_and(|b| b.deleted_at.is_none() && b.managed_by == "child" && b.parent_bot_id.as_deref() == Some(parent_bot_id.as_str()));
            if !still_owned || child_id.as_str() == crate::agent_relay::DAEMON_SENDER || &id != parent_bot_id || child_id == &id || relay_claim.is_some_and(|claim| claim != child_id.as_str()) {
                return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"child_scope_denied"})));
            }
            Some(crate::relay_auth::Relay { from: child_id.clone() })
        }
        _ => {
            let effective_relay = relay_claim.or_else(|| headers.get("X-AM-Bot-Id").and_then(|v| v.to_str().ok()));
            crate::relay_auth::authenticate(&app, &headers, effective_relay, &id).await?
        }
    };
    // 這次派工只靠什麼放行（專家派工開關／代班）；建 turn 的那一句依它正向確認仍成立（`lifecycle::relay_grant`）。
    let mut relay_auth: Option<String> = None;
    if !b.expect_result {
        if let Some(r) = &relay { relay_auth = check_relay_basis(&app, &r.from, &id).await?; }

    }
    if b.expect_result {
        let (RequestPrincipal::Bot(sender) | RequestPrincipal::Child { child_id: sender, .. }) = &principal else { return Err(LcError::Bad("expect_result requires a bot principal".into())); };
        if relay.as_ref().map(|r| &r.from) != Some(sender) { return Err(LcError::Bad("expect_result requires authenticated relay".into())); }
        if b.text.trim().is_empty() { return Err(LcError::Bad("text must not be empty".into())); }
        if b.send_now || b.start_if_stopped || b.clear_draft || b.submit_draft || b.ack || b.reply_to.is_some() {
            return Err(LcError::Bad("expect_result cannot be combined with interruption, startup, drafts or acknowledgement".into()));
        }
        let files = serde_json::to_string(&b.attachments).map_err(any_err)?;
        let cluster = b.cluster.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(str::to_string);
        // 冪等身分（#2069）：直接派工是（來源, 收件者, 鍵）；群集派工的 `to_bot_id` 會被 `claim_target` 改派給別的成員，
        // 所以改用（來源, 群集, 鍵）認原請求，否則原呼叫端重送給原目標會再建一筆、重複派工。
        let existing: Option<(String, String, String, String, Option<String>)> = sqlx::query_as("SELECT id,prompt_text,attachments_json,state,to_turn_id FROM relay_requests WHERE from_bot_id=? AND client_request_id=? AND (to_bot_id=? OR redirected_from=? OR (cluster IS NOT NULL AND cluster=?)) ORDER BY to_bot_id<>? LIMIT 1")
            .bind(sender).bind(&crid).bind(&id).bind(&id).bind(&cluster).bind(&id).fetch_optional(&app.db).await.map_err(any_err)?;
        if let Some((request_id, text, stored_files, state, turn_id)) = existing {
            if text != b.text || stored_files != files { return Err(LcError::conflict("client_request_id reused with different request", json!({}))); }
            return Ok((StatusCode::OK, Json(json!({"request_id":request_id,"turn_id":turn_id,"message_id":null,"delivery":state,"state":state}))).into_response());
        }
        relay_auth = check_relay_basis(&app, sender, &id).await?;
        let mut cluster_key: Option<String> = None;
        if let Some(name) = &cluster {
            let from = db::bot(&app.db, sender).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("relay sender".into()))?;
            let Some(scope) = crate::expert_cluster::sender_scope(&app, &from).await.map_err(any_err)? else {
                return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"cluster_manager_only"})));
            };
            let target_ok = if let Some(auto) = crate::expert_cluster::parse_auto_target(name) {
                crate::expert_cluster::project_executors(&app, &from.project_id).await.map_err(any_err)?.iter()
                    .any(|m| m.id == id && crate::expert_cluster::tier_of(&m.kind, m.model.as_deref()) == auto.tier)
            } else {
                let clusters = crate::expert_cluster::project_clusters(&app, &from.project_id).await.map_err(any_err)?;
                let found = clusters.iter().find(|c| &c.name == name && c.members.iter().any(|m| m.id == id));
                if let Some(c) = found {
                    cluster_key = Some(serde_json::to_string(&crate::expert_cluster::ClusterKey::from_cluster(&from.project_id, c)).map_err(any_err)?);
                }
                found.is_some()
            };
            if !target_ok { return Err(LcError::Bad("target is not a member of that cluster".into())); }
            // 專家：目標必須是它可直接派工的成員（check_relay_policy 已核過，這裡明確擋群集越界）。
            if !scope.allows(&id) { return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"cluster_not_dispatchable"}))); }
        }
        crate::attach::resolve(&app, &id, &b.attachments).await.map_err(|e| LcError::Bad(e.to_string()))?;
        if kind != Some("claude-api") {
            ensure_relay_member_ready(&app, sender, &id).await?;
        }
        let request_id = db::ulid();
        let lock = crate::runners::child_done::parent_lock(&format!("relay-request:{request_id}"));
        let _guard = lock.lock().await;
        // 群集請求：同（來源, 群集, 鍵）已有一筆（可能剛被改派走、唯一鍵擋不到）就不插，跟上面的查重同一個身分。
        let inserted = sqlx::query("INSERT OR IGNORE INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,track,cluster,cluster_key) SELECT ?,?,?,?,?,?,'pending',?,?,?,? WHERE ? IS NULL OR NOT EXISTS(SELECT 1 FROM relay_requests WHERE from_bot_id=? AND client_request_id=? AND cluster=?)")
            .bind(&request_id).bind(sender).bind(&id).bind(&crid).bind(&b.text).bind(&files).bind(db::now()).bind(&b.track).bind(&cluster).bind(&cluster_key)
            .bind(&cluster).bind(sender).bind(&crid).bind(&cluster).execute(&app.db).await.map_err(any_err)?.rows_affected() == 1;
        #[cfg(test)]
        crate::lifecycle::race_point::hit("relay_request_after_insert", &crid).await;
        let (request_id, text, stored_files): (String, String, String) = sqlx::query_as("SELECT id,prompt_text,attachments_json FROM relay_requests WHERE from_bot_id=? AND client_request_id=? AND (to_bot_id=? OR (cluster IS NOT NULL AND cluster=?)) ORDER BY id<>?, to_bot_id<>? LIMIT 1")
            .bind(sender).bind(&crid).bind(&id).bind(&cluster).bind(&request_id).bind(&id).fetch_one(&app.db).await.map_err(any_err)?;
        if text != b.text || stored_files != files { return Err(LcError::conflict("client_request_id reused with different request", json!({}))); }
        let (request_state, stored_turn): (String,Option<String>) = sqlx::query_as("SELECT state,to_turn_id FROM relay_requests WHERE id=?")
            .bind(&request_id).fetch_one(&app.db).await.map_err(any_err)?;
        if !inserted || request_state != "pending" || stored_turn.is_some() {
            return Ok((StatusCode::OK, Json(json!({"request_id":request_id,"turn_id":stored_turn,"message_id":null,"delivery":request_state,"state":request_state}))).into_response());
        }
        // 群集派工且被挑中的成員已不閒（resolve 當時都忙，或之後被別的工作佔走）：留在 pending，
        // 由 relay_requests 的 reconcile 在第一個閒下來的成員身上送出，不在忙的成員後面排隊。
        if cluster.is_some() && !crate::expert_cluster::member_idle(&app, &id).await {
            return Ok((StatusCode::ACCEPTED, Json(json!({"request_id":request_id,"turn_id":null,"message_id":null,"delivery":"pending","cluster":cluster}))).into_response());
        }
        // Recheck the immutable named-cluster key before the immediate delivery path too.
        // If selection moved, the normal pending reconciler delivers to the new member.
        if cluster_key.is_some() {
            let name = cluster.as_deref().expect("named key requires cluster");
            if crate::expert_cluster::claim_target(&app, &request_id, &id, name).await.map_err(any_err)?.as_deref() != Some(id.as_str()) {
                return Ok((StatusCode::ACCEPTED, Json(json!({"request_id":request_id,"turn_id":null,"message_id":null,"delivery":"pending","cluster":cluster}))).into_response());
            }
        }
        let delivery_key = format!("relay-request:{request_id}");
        let is_target_claude_api = kind == Some("claude-api");
        let out = if is_target_claude_api {
            use crate::lifecycle::api_bot_port::{ApiBotPort, ApiNativeStartError, ApiNativeStartMode};
            if let Err((cap, spent)) = crate::claude_api_cap::check_total_cap_allowed(&app, 0).await {
                let cap_str = crate::claude_api_cap::format_usd_pretty(cap);
                let spent_str = crate::claude_api_cap::format_usd_pretty(spent);
                let err_msg = format!("api_total_cap_reached:cap={cap_str}:spent={spent_str}");
                sqlx::query("UPDATE relay_requests SET state='failed',error=? WHERE id=? AND state='pending' AND to_turn_id IS NULL")
                    .bind(&err_msg).bind(&request_id).execute(&app.db).await.map_err(any_err)?;
                return Ok((StatusCode::ACCEPTED, Json(json!({"request_id":request_id,"turn_id":null,"message_id":null,"delivery":"failed","error":err_msg}))).into_response());
            }
            let active = db::active_run(&app.db, &id).await.map_err(any_err)?;
            let run_state = active.as_ref().map(|r| r.state.as_str());
            if run_state != Some("running") {
                if run_state == Some("starting") {
                    return Ok((StatusCode::ACCEPTED, Json(json!({"request_id":request_id,"turn_id":null,"message_id":null,"delivery":"pending"}))).into_response());
                }
                match ApiBotPort::api_native_start(&app, &id, ApiNativeStartMode::Resume { required: false }).await {
                    Ok(_) => {
                        return Ok((StatusCode::ACCEPTED, Json(json!({"request_id":request_id,"turn_id":null,"message_id":null,"delivery":"pending"}))).into_response());
                    }
                    Err(e) => {
                        let err_msg = match e {
                            ApiNativeStartError::Refused(reason) => format!("start_failed:{reason}"),
                            ApiNativeStartError::ClearedButStartUnconfirmed => "start_failed:api_fresh_cleared_start_unconfirmed".to_string(),
                            ApiNativeStartError::Unconfirmed => "start_failed:api_native_start_unconfirmed".to_string(),
                        };
                        sqlx::query("UPDATE relay_requests SET state='failed',error=? WHERE id=? AND state='pending' AND to_turn_id IS NULL")
                            .bind(&err_msg).bind(&request_id).execute(&app.db).await.map_err(any_err)?;
                        return Ok((StatusCode::ACCEPTED, Json(json!({"request_id":request_id,"turn_id":null,"message_id":null,"delivery":"failed","error":err_msg}))).into_response());
                    }
                }
            }
            let has_inflight: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.status='in_flight')")
                .bind(&id).fetch_one(&app.db).await.map_err(any_err)?;
            if has_inflight {
                return Ok((StatusCode::ACCEPTED, Json(json!({"request_id":request_id,"turn_id":null,"message_id":null,"delivery":"pending"}))).into_response());
            }
            match ApiBotPort::api_native_prompt(&app, &id, &b.text, &delivery_key).await {
                Ok(out) => out,
                Err(e) => {
                    tracing::warn!(request = %request_id, error = ?e, "claude-api relay prompt unconfirmed, keeping pending for reconciliation");
                    return Ok((StatusCode::ACCEPTED, Json(json!({"request_id":request_id,"turn_id":null,"message_id":null,"delivery":"pending"}))).into_response());
                }
            }
        } else {
            match lifecycle::prompt_from_api_queue_if_busy(&app, &id, &b.text, &delivery_key, &b.attachments, lifecycle::RelaySrc::trusted(Some(sender)).with_auth(relay_auth.as_deref()), false, None).await {
                Ok(out) => out,
                Err(e) => {
                    tracing::warn!(request = %request_id, "relay request accepted for later reconciliation");
                    let mut permanent = matches!(&e, LcError::Bad(_) | LcError::BadValue(_) | LcError::NotFound(_) | LcError::Forbidden(_));
                    if permanent {
                        permanent = sqlx::query("UPDATE relay_requests SET state='failed',error=? WHERE id=? AND state='pending' AND to_turn_id IS NULL AND NOT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=relay_requests.to_bot_id AND t.client_request_id=?)")
                            .bind("request could not be delivered").bind(&request_id).bind(&delivery_key).execute(&app.db).await.map_err(any_err)?.rows_affected() == 1;
                    }
                    return Ok((StatusCode::ACCEPTED, Json(json!({"request_id":request_id,"turn_id":null,"message_id":null,"delivery":if permanent { "failed" } else { "pending" }}))).into_response());
                }
            }
        };
        let updated = sqlx::query("UPDATE relay_requests SET to_turn_id=?,to_run_id=(SELECT run_id FROM turns WHERE id=?),state=CASE WHEN state='pending' AND EXISTS(SELECT 1 FROM turns WHERE id=? AND status<>'queued' AND delivery='ok' AND delivery_verified=1) THEN 'delivered' ELSE state END WHERE id=? AND state IN ('pending','delivered')")
            .bind(&out.turn_id).bind(&out.turn_id).bind(&out.turn_id).bind(&request_id).execute(&app.db).await.map_err(any_err)?.rows_affected();
        if updated == 0 {
            let (state, turn_id): (String, Option<String>) = sqlx::query_as("SELECT state,to_turn_id FROM relay_requests WHERE id=?").bind(&request_id).fetch_one(&app.db).await.map_err(any_err)?;
            return Ok((StatusCode::OK, Json(json!({"request_id":request_id,"turn_id":turn_id,"message_id":null,"delivery":state,"state":state}))).into_response());
        }
        let mut response = serde_json::to_value(out).map_err(any_err)?;
        response["request_id"] = json!(request_id);
        return Ok((StatusCode::OK, Json(response)).into_response());
    }
    // bot 寫給 AGM 的申請不直接開回合：排進協調者的佇列，回 202（SPEC §18.15）。
    if let Some(r) = &relay {
        let mark = crate::supervisor::bot_requests::ReplyMark { ack: b.ack, reply_to: b.reply_to.as_deref() };
        let queued = crate::supervisor::bot_requests::intercept(&app, &id, &r.from, &b.text, given_crid.as_deref(), &b.attachments, true, "api", mark);
        if let Some(v) = queued.await? {
            return Ok((StatusCode::ACCEPTED, Json(v)).into_response());
        }
    }
    let src = lifecycle::RelaySrc::trusted(relay.as_ref().map(|r| r.from.as_str())).with_auth(relay_auth.as_deref());
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
    if let Some(r) = &relay {
        crate::attach::resolve(&app, &id, &b.attachments).await.map_err(|e| LcError::Bad(e.to_string()))?;
        ensure_relay_member_ready(&app, &r.from, &id).await?;
    }
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
    if b.share_reply_visible && matches!(crate::share::store::is_share_bot(&app.db, &id).await, Ok(true)) {
        // 記不下來只是回覆不出現在分享頁（預設行為），訊息本身已經送出，不回錯。
        if let Err(e) = crate::share::store::mark_reply_visible(&app.db, &out.message_id).await {
            tracing::warn!(bot = %id, error = %e, "could not mark the share reply as visible");
        }
    }
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
    let a = crate::attach::save_bytes(&app, &id, name, &mime, body)
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

async fn keys_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<KeysIn>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    shell::check_keys(&b.keys)?;
    lifecycle::send_keys(&app, &id, b.keys, b.expect_run_id).await?;
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

#[derive(Deserialize, Default)]
struct SendQueuedNowIn {
    expect_run_id: Option<String>,
}

/// `POST /api/bots/:id/send-queued-now`：補充排在 claude 終端佇列時，送 send-now 鍵立刻送出（`lifecycle::queued_send_now`）。
async fn send_queued_now_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    body: Option<Json<SendQueuedNowIn>>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    lifecycle::send_queued_now(&app, &id, b.expect_run_id).await?;
    Ok((StatusCode::OK, Json(json!({"sent": true}))).into_response())
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

async fn text_bot(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<TextIn>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    shell::check_text(&b.text)?;
    let record = b.record.unwrap_or(false);
    let m = lifecycle::send_text_recorded(&app, &id, &b.text, b.enter.unwrap_or(true), b.expect_run_id, record).await?;
    let body = if record { json!({ "message_id": m.map(|m| m.id) }) } else { json!({}) };
    Ok((StatusCode::OK, Json(body)).into_response())
}

#[derive(Deserialize)]
struct SuggestionAcceptIn {
    /// 網頁看到的那句建議（`run.prompt_suggestion`）；畫面上的建議跟它不完全相同就 409，不送任何鍵。
    suggestion: String,
    expect_run_id: Option<String>,
    client_request_id: Option<String>,
}

/// `POST /api/bots/:id/suggestion/accept`：對 pane 送 Tab 把 claude 的灰字建議收進輸入框，確認後送 Enter（跟終端一模一樣）。
/// 只有使用者自己：bot 的 token 不能替人收下建議（`lifecycle::suggestion`）。
async fn accept_suggestion(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<SuggestionAcceptIn>,
) -> Result<Response, LcError> {
    require_user(&principal)?;
    let crid = match b.client_request_id.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => crate::request_id::validate("client_request_id", c)?,
        None => db::ulid(),
    };
    let out = lifecycle::accept_prompt_suggestion(&app, &id, &b.suggestion, b.expect_run_id.as_deref(), &crid).await?;
    Ok((StatusCode::OK, Json(out)).into_response())
}

/// 派工者可以撤回自己派出、還在佇列裡的那一則（[`lifecycle::sender_may_withdraw`]）；其他人照舊只有使用者能撤。
async fn withdraw_turn_route(State(app): State<Arc<App>>, Path(id): Path<String>, principal: Option<Extension<RequestPrincipal>>) -> Result<Response, LcError> {
    if let Some(Extension(RequestPrincipal::Bot(caller) | RequestPrincipal::Child { child_id: caller, .. })) = principal {
        lifecycle::withdraw_turn_by_sender(&app, &id, &caller).await?;
        return Ok((StatusCode::OK, Json(json!({"withdrawn": true}))).into_response());
    }
    withdraw_turn(State(app), Path(id)).await
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
mod withdraw_turn_tests;

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
    // `before=` 空字串等同沒帶（回第一頁），同 `turn_id`／`role` 與群組版；不是「游標指到一個不存在的 id」。
    let before_rowid = match q.get("before").map(String::as_str).filter(|b| !b.is_empty()) {
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
    // 只在同一個 conversation 底下過濾，查不到的 turn 是空清單不是 404。
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
    // `turns`：這一頁訊息所屬的回合（送達警示與撤回鈕要查得到，翻到更早的頁也一樣）∪ 這段對話還沒結束的回合
    // （判斷還在不在跑）。以前是「最新 limit+1 筆」，翻頁或帶 `turn_id=` 時跟頁上的訊息對不上（#954）。
    // 一頁最多 500 則訊息，`?` 佔位符低於 SQLite 的 999 個參數上限。第二排序鍵 `rowid`：同毫秒的回合順序要穩定。
    let mut page_turns: Vec<&str> = msgs.iter().filter_map(|m| m.turn_id.as_deref()).collect();
    page_turns.sort_unstable();
    page_turns.dedup();
    let mut turns_sql = String::from("SELECT * FROM turns WHERE conversation_id=? AND (status IN ('queued','in_flight')");
    if !page_turns.is_empty() {
        turns_sql.push_str(" OR id IN (");
        turns_sql.push_str(&vec!["?"; page_turns.len()].join(","));
        turns_sql.push(')');
    }
    turns_sql.push_str(") ORDER BY created_at DESC, rowid DESC");
    let mut turns_query = sqlx::query_as::<_, db::Turn>(&turns_sql).bind(&conv);
    for t in &page_turns {
        turns_query = turns_query.bind(*t);
    }
    let turns = turns_query.fetch_all(&app.db).await.map_err(any_err)?;
    Ok(Json(json!({"bot_id": id, "conversation_id": conv, "messages": msgs, "turns": turns, "has_more": has_more})))
}

/// `id → (名字, 已收掉)`。不濾 `deleted_at`：已收掉的 child 也要認得出名字。`?` 數量受一頁 500 則限制，低於 SQLite 的 999。
async fn relay_names(app: &App, ids: &[&str]) -> Result<HashMap<String, (String, bool)>, LcError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!("SELECT id, name, deleted_at IS NOT NULL FROM bots WHERE id IN ({})", vec!["?"; ids.len()].join(","));
    let mut query = sqlx::query_as::<_, (String, String, bool)>(&sql);
    for id in ids {
        query = query.bind(*id);
    }
    Ok(query
        .fetch_all(&app.db)
        .await
        .map_err(any_err)?
        .into_iter()
        .map(|(id, name, gone)| (id, (name, gone)))
        .collect())
}

/// 把 `relay_from_name`／`relay_from_deleted` 補到一則訊息的 JSON 上（查得到才補）。
fn stamp_relay_name(m: &mut Value, names: &HashMap<String, (String, bool)>) {
    if let Some((name, gone)) = m["relay_from"].as_str().and_then(|id| names.get(id)).cloned() {
        m["relay_from_name"] = json!(name);
        m["relay_from_deleted"] = json!(gone);
    }
}

/// 群集派工的紀錄（`relay_requests.cluster`）：`turn_id → (群集名, 實際接手的成員 id)`。
/// 成員那邊的 relay 訊息所在回合的 `client_request_id` 是 `relay-request:<id>`，管理者那邊的結果通知是 `relay-result:<id>:r<n>`，
/// 兩者都對回同一筆請求；不改 schema，現查現補。一般派工（`cluster IS NULL`）不會出現在結果裡。
async fn cluster_dispatch_by_turn(app: &App, turn_ids: &[&str]) -> Result<HashMap<String, (String, String)>, LcError> {
    if turn_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT t.id, r.cluster, r.to_bot_id FROM turns t JOIN relay_requests r ON r.id = CASE \
           WHEN t.client_request_id LIKE 'relay-request:%' THEN substr(t.client_request_id, 15) \
           WHEN t.client_request_id LIKE 'relay-result:%' THEN substr(substr(t.client_request_id, 14), 1, instr(substr(t.client_request_id, 14) || ':', ':') - 1) END \
         WHERE r.cluster IS NOT NULL AND t.id IN ({})",
        vec!["?"; turn_ids.len()].join(",")
    );
    let mut query = sqlx::query_as::<_, (String, String, String)>(&sql);
    for id in turn_ids {
        query = query.bind(*id);
    }
    Ok(query.fetch_all(&app.db).await.map_err(any_err)?.into_iter().map(|(t, c, b)| (t, (c, b))).collect())
}

/// 把 `via_cluster`／`assignee_bot_id`／`assignee_name` 補到一則群集派工相關的訊息上。
fn stamp_cluster_dispatch(m: &mut Value, by_turn: &HashMap<String, (String, String)>, names: &HashMap<String, (String, bool)>) {
    let Some((cluster, assignee)) = m["turn_id"].as_str().and_then(|t| by_turn.get(t)).cloned() else { return };
    if m["relay_from"].is_null() {
        return;
    }
    m["via_cluster"] = json!(cluster);
    if let Some((name, _)) = names.get(&assignee) {
        m["assignee_name"] = json!(name);
    }
    m["assignee_bot_id"] = json!(assignee);
}

/// 整頁 `messages` 補代發來源名字（REST 的單 bot 與群組兩條路共用）。
async fn add_relay_names(app: &App, body: &mut Value) -> Result<(), LcError> {
    let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let mut ids: Vec<String> = msgs.iter().filter_map(|m| m["relay_from"].as_str()).filter(|id| *id != "daemon").map(str::to_owned).collect();
    ids.sort_unstable();
    ids.dedup();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut names = relay_names(app, &refs).await?;
    let turns: Vec<&str> = msgs.iter().filter(|m| m["relay_from"].is_string()).filter_map(|m| m["turn_id"].as_str()).collect();
    let clusters = cluster_dispatch_by_turn(app, &turns).await?;
    let missing: Vec<&str> = clusters.values().map(|(_, b)| b.as_str()).filter(|b| !names.contains_key(*b)).collect();
    names.extend(relay_names(app, &missing).await?);
    for m in msgs.iter_mut() {
        stamp_relay_name(m, &names);
        stamp_cluster_dispatch(m, &clusters, &names);
    }
    Ok(())
}

/// WS 幀：`message_added` 補代發來源名字（同 REST）；其餘原樣。WS 只接 User（見 `ws_handler`），不再判 principal。
/// 查不到名字（DB 讀失敗）就照舊送原幀，前端靠記住的名字兜底。
async fn ws_frame_text(app: &App, e: &crate::state::WsEvent) -> String {
    if e.kind != "message_added" {
        return serde_json::to_string(e).unwrap();
    }
    let mut v = serde_json::to_value(e).unwrap();
    let relay = v["data"]["message"]["relay_from"].as_str().filter(|id| *id != "daemon").map(str::to_owned);
    if let Some(id) = relay {
        if let Ok(names) = relay_names(app, &[id.as_str()]).await {
            stamp_relay_name(&mut v["data"]["message"], &names);
        }
    }
    if let Some(turn) = v["data"]["message"]["turn_id"].as_str().filter(|_| v["data"]["message"]["relay_from"].is_string()).map(str::to_owned) {
        if let Ok(clusters) = cluster_dispatch_by_turn(app, &[turn.as_str()]).await {
            let assignee: Vec<String> = clusters.values().map(|(_, b)| b.clone()).collect();
            let refs: Vec<&str> = assignee.iter().map(String::as_str).collect();
            let names = relay_names(app, &refs).await.unwrap_or_default();
            stamp_cluster_dispatch(&mut v["data"]["message"], &clusters, &names);
        }
    }
    v.to_string()
}

async fn get_messages_with_relay_names(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    Extension(principal): Extension<RequestPrincipal>,
) -> Result<Json<Value>, LcError> {
    let Json(mut body) = get_messages(State(app.clone()), Path(id), Query(q)).await?;
    if principal == RequestPrincipal::User {
        add_relay_names(&app, &mut body).await?;
    }
    Ok(Json(body))
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
    // `format=ansi` 讀的是帶 SGR 色碼的原樣；預設（沒帶或 `text`）維持純文字、回應形狀不變。
    let ansi = match q.get("format").map(String::as_str) {
        None | Some("text") => false,
        Some("ansi") => true,
        Some(_) => return Err(LcError::Bad("bad format".into())),
    };
    // herdr 的 `detection` 不管 format 都回純文字，標成 ansi 會騙人，所以直接拒絕。
    if ansi && source == "detection" {
        return Err(LcError::Bad("format=ansi 不支援 source=detection，請改用 visible／recent／recent_unwrapped".into()));
    }
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| LcError::Upstream(format!("no Herdr session is available for run `{}`", run.id)))?;
    let read = if ansi {
        client.pane_read_ansi(&pane, &source, lines).await
    } else {
        client.pane_read(&pane, &source, lines).await
    }
    .map_err(any_err)?;
    // Below ~60 columns TUI text is unrecoverably fragmented (seen on `w8:pK` at 31); lets the UI explain it.
    let (columns, rows) = match client.pane_size(&pane).await {
        Ok(Some((w, h))) => (Some(w), Some(h)),
        _ => (None, None),
    };
    let mut body = json!({
        "bot_id": id, "run_id": run.id, "pane_id": pane,
        "source": read.source, "text": read.text, "revision": read.revision, "truncated": read.truncated,
        "agent_status": run.agent_status,
        "columns": columns, "rows": rows,
    });
    if ansi {
        body["format"] = json!("ansi");
    }
    Ok(Json(body))
}

/// `GET /api/bots/:id/composer-draft` — 唯讀看一眼 pane 輸入框有沒有一段沒送出的字（完整、不截），不按任何鍵。
/// 權限比照 `GET /bots/:id/terminal`；沒有 active run／pane 回 404。
async fn get_composer_draft(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let run = db::active_run(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let bot = db::bot(&app.db, &id).await.map_err(any_err)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| LcError::Upstream(format!("no Herdr session is available for run `{}`", run.id)))?;
    Ok(Json(crate::lifecycle::composer_draft::snapshot(&client, &run, &bot).await?))
}

async fn ws_handler(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_is_local(&headers, app.port, app.allow_lan()) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    // `/ws` is outside the `/api` auth middleware, but it is still a User-only endpoint.
    // Do not silently ignore an explicit Bot/Service identity when the query also carries the
    // shared UI token; that would let one request cross principal boundaries.
    if ["X-AM-Bot-Id", "X-AM-Bot-Token", "X-AM-Child-Spawn-Id", "X-AM-Child-Token", "X-AM-Service-Id", "X-AM-Service-Token"]
        .iter()
        .any(|name| headers.contains_key(*name))
    {
        return (StatusCode::UNAUTHORIZED, "mixed credentials").into_response();
    }
    if !ct_eq(q.get("token").map(|s| s.as_str()).unwrap_or(""), &app.ui_token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let since: Option<u64> = q.get("since").and_then(|s| s.parse().ok());
    // 連線數上限：每條連線一個 task、一個訂閱與（慢 client 時）一份緩衝，不能無限開。
    let Some(slot) = WsSlot::take(&app) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many websocket connections").into_response();
    };
    ws.max_message_size(crate::state::WS_MAX_INBOUND_BYTES)
        .max_frame_size(crate::state::WS_MAX_INBOUND_BYTES)
        .on_upgrade(move |socket| async move {
            ws_loop(app, socket, since).await;
            drop(slot);
        })
}

/// 一條開著的 WebSocket 連線佔的名額（[`crate::state::App::ws_connections`]）；連線結束（含 client 斷線、被我們踢掉）時放回。
pub(crate) struct WsSlot(Arc<App>);

impl WsSlot {
    pub(crate) fn take(app: &Arc<App>) -> Option<WsSlot> {
        let max = app.ws_max_connections.load(Ordering::SeqCst);
        app.ws_connections
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < max).then_some(n + 1))
            .ok()
            .map(|_| WsSlot(app.clone()))
    }
}

impl Drop for WsSlot {
    fn drop(&mut self) {
        self.0.ws_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 送一幀；client 太久不收（TCP 緩衝滿了）就放棄，呼叫端收掉這條連線。
async fn ws_send(socket: &mut WebSocket, msg: WsMessage, timeout: std::time::Duration) -> bool {
    matches!(tokio::time::timeout(timeout, socket.send(msg)).await, Ok(Ok(())))
}

async fn ws_loop(app: Arc<App>, mut socket: WebSocket, since: Option<u64>) {
    let mut rx = app.subscribe();
    let send_timeout = app.ws_send_timeout();
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
                if !ws_send(&mut socket, WsMessage::Text(ws_frame_text(&app, &e).await.into()), send_timeout).await {
                    return;
                }
            }
        }
        None => {
            if !ws_send(&mut socket, WsMessage::Text(json!({"type": "resync", "seq": app.current_seq()}).to_string().into()), send_timeout).await {
                return;
            }
        }
    }
    // 沒事件也定期送一幀（issue #760）：不佔 seq、不進重播環，客戶端只拿它當「線還活著」的證據。
    let every = app.ws_ping_every();
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if !ws_send(&mut socket, WsMessage::Text(json!({"type": "ping"}).to_string().into()), send_timeout).await { return; }
            }
            ev = rx.recv() => match ev {
                // backlog 已經送過這一則（訂閱與讀環之間送出的，兩邊都有）：跳過，不是漏送（issue #521）。
                Ok(e) if e.seq <= sent_through => {}
                Ok(e) => {
                    sent_through = e.seq;
                    if !ws_send(&mut socket, WsMessage::Text(ws_frame_text(&app, &e).await.into()), send_timeout).await { return; }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // 帶 seq，跟重連那條一樣（API.md §8：`{"type":"resync","seq":12}`）——少了它，
                    // 客戶端在 lag 之後的 `lastSeq` 會停在漏掉的事件之前，下一次重連白跑一趟 backlog（issue #482）。
                    if !ws_send(&mut socket, WsMessage::Text(json!({"type": "resync", "seq": app.current_seq()}).to_string().into()), send_timeout).await { return; }
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
mod ws_shutdown_tests;

/// WebSocket 伺服端的資源上限（對抗式審查）：client→server 的訊息大小、同時連線數、一直不讀的 client、事件裡不能出現憑證欄位。
#[cfg(test)]
mod ws_limits_tests;

/// issue #521：`ws_loop` 先訂閱再讀環，兩者之間送出的耐久事件兩邊都有——同一條連線上同一個 `seq` 只能送一次。
#[cfg(test)]
mod ws_backlog_dedupe_tests;

/// issue #760：瀏覽器沒辦法看到 WebSocket 的 ping 控制幀，所以 daemon 定期送一個 text 幀 `{"type":"ping"}`，
/// 讓客戶端有「連線還活著」的證據；半開連線（睡眠、NAT／tailscale 逾時）下它收不到，就能判定斷線。
#[cfg(test)]
mod ws_heartbeat_tests;

#[cfg(test)]
mod order_tests;

#[cfg(test)]
mod name_tests;

#[cfg(test)]
mod message_tests;

#[cfg(test)]
mod delete_bot_tests;

#[cfg(test)]
mod attachment_tests;

#[cfg(test)]
mod prompt_route_tests;

#[cfg(test)]
mod interrupt_route_tests;

#[cfg(test)]
mod resume_query_tests;

#[cfg(test)]
mod started_json_tests;

/// bot 設定的 API：建／改／還原，與啟動時寫進 `--settings` 的檔案。
#[cfg(test)]
mod bot_config_tests;

/// #243：pane-purpose 回報遇到 host 讀不到，要 503 讓 shim 重試——不能把遠端 pane id 寫進 `host='local'`。
#[cfg(test)]
mod relay_pane_host_tests;

#[cfg(test)]
mod extractor_rejection_tests;

#[cfg(test)]
mod unknown_api_route_tests;

#[cfg(test)]
mod per_principal_auth_tests;

#[cfg(test)]
mod file_api_auth_tests;

#[cfg(test)]
mod caller_audit_tests;

#[cfg(test)]
mod ct_eq_tests;

/// #350：一個請求同時要寫 config.toml 與只存 DB 的欄位（`primary`／`primary_position`）時，兩個 store 不可能同一個交易
/// （`update_and_project` 自己就會寫 DB，外層交易會死鎖；事後回滾又是第二次寫入、一樣會失敗）。所以混送一律 400、什麼都不寫，
/// 呼叫端分成兩次；不會有「回失敗、其中一半卻生效了」。
#[cfg(test)]
mod mixed_store_tests;

/// #352：`POST /api/projects/:id/bots` 的冪等。回應遺失後原樣重送（快速新增每次帶 `name_auto:true`）不能再建第二顆 bot；
/// 記在 config.toml 的那顆 bot 上（daemon 持久，瀏覽器重整、daemon 重啟都在）。
#[cfg(test)]
mod create_bot_idempotency_tests;

#[cfg(test)]
mod child_restore_tests;

/// #339：`relay_from` 要跟呼叫者自己的身份綁在一起（`relay_auth`）。走真的 `prompt_bot` handler。
#[cfg(test)]
mod relay_from_auth_tests;

/// DNS rebinding：攻擊者的網域先指到他的伺服器載入頁面、再改指 127.0.0.1，之後頁面對自己 origin 的 GET
/// **不帶 `Origin`**（同源 GET 瀏覽器不送），TCP 對端又是 loopback——只看 Origin 與對端擋不住，`GET /api/session` 就把
/// UI token 交出去。瀏覽器送的 `Host` 是攻擊者的網域，所以 `allow_lan` 關著時 Host 也必須是 loopback 名稱。
#[cfg(test)]
mod host_header_tests;

/// `allow_lan` 開著（dev／Tailscale 存取）時的暴露面。以前這時 Host 與 Origin 一律放行：使用者瀏覽器開著任何網頁，
/// 攻擊者把自己的網域 DNS rebind 到區網上的 daemon，同源之後 `GET /api/session` 就把 UI token 交出去。
/// 區網存取用的是 IP、`localhost`、單一標籤主機名、`.local`、`.ts.net` 這類攻擊者拿不到的名字；其他名字不放行
/// （要用自訂網域，`AM_ALLOWED_HOSTS` 明列）。
#[cfg(test)]
mod lan_exposure_tests;

/// 上傳整個 body 先讀進記憶體（`Bytes`），單檔上限 50 MiB：不限並發的話 N 個大檔同時上傳就是 N × 50 MiB。
/// 同時進行的上傳有上限，滿了回 429（`Retry-After`），不排隊（排隊會讓慢速連線占住位置）。
#[cfg(test)]
mod upload_concurrency_tests;

/// #1322：巡檢健康摘要的 running／busy／stopped 不能逐顆 bot 再查一次 `active_run`（N+1）。
#[cfg(test)]
mod health_query_count_tests;

/// `GET /api/state` 的 SQL 語句數不能隨 bot 數成長（以前每顆 bot 再問 5 次：active_run、queued_turn、bot＋bot_host×2），
/// 而且每顆 bot 的 run／lamp／排隊中的回合要跟逐顆查的結果一樣。
#[cfg(test)]
mod state_query_count_tests;

/// `GET /api/state` 與 WebSocket 推送的成本（量測）。`AM_STATE_BENCH_DIR` 指到一個放著 `db.sqlite3`（正式 DB 的**複本**）的暫存目錄時才跑，
/// 沒設就直接過；複本用完要立刻刪（見 CLAUDE.md：DB 只能放 `mktemp -d /tmp/am-state-XXXX`）。
#[cfg(test)]
mod state_cost_tests;

#[cfg(test)]
mod host_target_tests;

#[cfg(test)]
#[path = "bot_scope_matrix.rs"]
mod bot_scope_matrix;

#[cfg(test)]
mod handoff_delete_tests;

#[cfg(test)]
mod handoff_restore_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
mod native_api_http_tests;

#[cfg(test)]
mod relay_result_api_tests;

/// 跑著的 run 是否還在用舊的角色政策。專家模式上線前起的 run 沒有記錄（`None`）：
/// 一般 parent_child 專案、沒分配角色的 bot 跟新政策沒有差別，不算待套用（不然每顆舊 bot 都被要求重啟）。
fn role_policy_drifted(runtime: Option<&str>, project_id: &str, host: &str, mode: &str, role: Option<&str>) -> bool {
    let role = role.unwrap_or("");
    match runtime {
        Some(policy) => policy != format!("{project_id}:{host}:{mode}:{role}"),
        None => !(mode == "parent_child" && role.is_empty()),
    }
}

#[cfg(test)]
mod daemon_url_pending_tests;

#[cfg(test)]
mod patrol_restart_scope_tests;

#[cfg(test)]
mod manager_role_scope_tests;

#[cfg(test)]
mod expert_member_tests;

/// 專家（specialist）的 API 約定：track 必填、只有使用者能改 track、role_policy 只有使用者能改；hire 由使用者或 manager 決定 kind／model／effort。
#[cfg(test)]
mod specialist_role_tests;

/// B 期角色 policy／bot 覆寫：User-only、值的驗證（看 kind 的視窗與 TTL）、寫進 config 並投影到 DB。
#[cfg(test)]
mod runtime_policy_api_tests;

/// C 期（`agm hire --fork-from`）：來源檢查的四種拒絕、條目容量計入 fork 出來的成員、config 記下待 fork 的來源。
#[cfg(test)]
mod hire_fork_api_tests;

#[cfg(test)]
mod reserved_request_prefix_tests;

#[cfg(test)]
mod relay_autostart_tests;

#[cfg(test)]
mod claude_api_advisor_relay_tests;
