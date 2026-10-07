//! hook 收件的組裝層：`POST /hook/{provider}` handler、StatusLine 單槽、遠端 spool 的背景掃描與重放（吃 `App`、起背景 task 的部分）。
//! `hookrecv` 只留對 `HookHost` 泛型的核心（`process*`、`drain_remote`、`replay_spool`）與純規則；這裡把它接到 `Arc<App>`（step 6a）。

use crate::events::ports::ApiPort;
use crate::db;
use crate::hookrecv::{self, HookBody, HookHost};
use crate::state::App;
use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

impl HookHost for Arc<App> {
    fn wake_hook_inbox(&self) {
        self.hook_inbox_wake.notify_one();
    }

    fn hook_bot_dir(&self, bot_id: &str) -> Result<std::path::PathBuf> {
        App::bot_dir(self, bot_id)
    }

    async fn after_turn_end(&self, body: &HookBody) {
        crate::runners::ask_answers::after_turn_end(self, body).await
    }

    async fn background_stop(&self, run: &db::Run, payload: &Value) {
        crate::runners::background_hook::on_stop(self, run, payload).await
    }

    async fn transcript_allowed(&self, bot: &db::Bot, path: &str) -> bool {
        crate::app_ports_p5::transcript_allowed(self, bot, path).await
    }

    async fn local_transcript_allowed(&self, bot: &db::Bot, path: &str) -> bool {
        crate::app_ports_p5::local_transcript_allowed(self, bot, path).await
    }
}

pub async fn receive(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Json(body): Json<HookBody>,
) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    // 分不出「沒這顆 bot」與「token 不對」（#304）：先驗 token，410 只給 token 正確者。
    let unauthorized = || (StatusCode::UNAUTHORIZED, Json(json!({"error": "unknown bot or bad token"})));
    let bot = match db::bot(&app.db, &body.bot_id).await {
        Ok(Some(b)) => b,
        _ => return unauthorized(),
    };
    if token.is_empty() || !app.ct_eq(token, &bot.hook_token) {
        return unauthorized();
    }
    // A3: a deleted bot's surviving agent must not be able to create turns / messages.
    if bot.deleted_at.is_some() {
        return (StatusCode::GONE, Json(json!({"error": "bot deleted"})));
    }
    let provider = if body.provider.is_empty() { provider } else { body.provider.clone() }.to_ascii_lowercase();
    // provider 要跟這顆 bot 的 kind 一致：claude bot 的 pane 裡跑 `codex exec -c notify=[… --bot $AM_BOT_ID …]`，
    // token／run id 都是從 pane 環境繼承來的、全對，codex 的 thread-id 就被當成這顆 claude bot 的 session
    // 記成 native_session_id 還標 verified（2026-09-22 AM-issuers-XH：DB 記了一個不存在的 UUIDv7，
    // restart --resume native 兩次都起不來）。
    if !hookrecv::provider_matches_kind(&provider, &bot.kind) {
        tracing::warn!(bot = %bot.name, kind = %bot.kind, %provider, "hook from another provider; ignored");
        return (StatusCode::CONFLICT, Json(json!({"error": "provider_mismatch", "bot_kind": bot.kind, "provider": provider})));
    }
    let mut b = body;
    b.provider = provider;

    // 單槽、最新的贏的訊號：不進佇列，掉一格只是晚一次重繪（`hook_inbox` 模組說明）。
    if matches!(hookrecv::classify(&b.provider, &b.payload), hookrecv::HookKind::StatusLine) {
        spawn_statusline(&app, b);
        return (StatusCode::OK, Json(json!({})));
    }

    match crate::hook_inbox::accept(&app.db, &b, crate::hook_inbox::Source::Http).await {
        Ok(accepted) => {
            // commit 之後才叫醒 worker：醒來一定看得到那一列。
            app.hook_inbox_wake.notify_one();
            (StatusCode::OK, Json(json!({"stored": accepted.is_new()})))
        }
        Err(e) => {
            tracing::error!(error = ?e, bot = %b.bot_id, "hook not persisted; telling the sender to spool it");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "not persisted"})))
        }
    }
}

/// 每顆 bot 在等處理的最新一則 StatusLine。
fn status_slots() -> &'static std::sync::Mutex<std::collections::HashMap<String, HookBody>> {
    static SLOTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, HookBody>>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(Default::default)
}

#[cfg(test)]
static STATUSLINE_TASKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) fn statusline_tasks_spawned() -> usize {
    STATUSLINE_TASKS.load(std::sync::atomic::Ordering::SeqCst)
}

/// StatusLine 是「最新的贏」的單槽訊號：處理它要拿 bot 鎖，鎖被長操作握著時，每一則各丟一個 task 排隊就會一路疊上去
/// （每個 task 抱一份 body，高頻 × 慢鎖＝記憶體與排隊都無上限）。同一顆 bot 只留一格：槽裡已經有一則在等，
/// 後到的直接取代它（等的那個 task 醒來處理的就是最新的）；槽是空的才起一個 task。所以每顆 bot 最多一個在處理、一個在等。
fn spawn_statusline(app: &Arc<App>, b: HookBody) {
    let bot_id = b.bot_id.clone();
    let replaced = status_slots().lock().unwrap_or_else(|e| e.into_inner()).insert(bot_id.clone(), b).is_some();
    if replaced {
        return;
    }
    #[cfg(test)]
    STATUSLINE_TASKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let app = app.clone();
    tokio::spawn(async move {
        // 先排隊等鎖、拿到鎖才把槽裡「當下最新」的那則取走：等鎖期間到的都已經取代過了。
        let lock = app.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        let Some(b) = status_slots().lock().unwrap_or_else(|e| e.into_inner()).remove(&bot_id) else { return };
        if let Err(e) = hookrecv::process_locked(&app, &b).await {
            tracing::error!(error = ?e, "statusline processing failed");
        }
    });
}


/// SPEC §11.4.4.
pub async fn drain_remote_coalesced(app: &Arc<App>, host: &str, bot_id: &str) -> Result<usize> {
    {
        let mut g = hookrecv::drain_gates().lock().unwrap();
        let now = std::time::Instant::now();
        hookrecv::prune_gates(&mut g, now);
        let e = g.entry(bot_id.to_string()).or_default();
        if !hookrecv::gate_admit(e, now) {
            tracing::debug!(bot_id, host, "drain merged into the one in the window");
            return Ok(0);
        }
    }
    let n = hookrecv::drain_remote(app, host, bot_id).await?;
    let again = {
        let mut g = hookrecv::drain_gates().lock().unwrap();
        hookrecv::gate_take_again(g.entry(bot_id.to_string()).or_default())
    };
    // An empty drain may mean the hook is still writing its line (§11.4.4).
    let delay = if again {
        Some(hookrecv::DRAIN_WINDOW)
    } else if n == 0 {
        Some(hookrecv::DRAIN_RETRY)
    } else {
        None
    };
    if let Some(d) = delay {
        let (app2, host2, bot2) = (app.clone(), host.to_string(), bot_id.to_string());
        let tasks = app.background_tasks.clone();
        tasks.spawn(async move {
            tokio::select! {
                _ = app2.shutdown.cancelled() => return,
                _ = tokio::time::sleep(d) => {}
            }
            let drained = tokio::select! {
                _ = app2.shutdown.cancelled() => return,
                result = hookrecv::drain_remote(&app2, &host2, &bot2) => result,
            };
            if let Err(e) = drained {
                tracing::debug!(bot_id = %bot2, host = %host2, error = ?e, "follow-up drain failed");
            }
        });
    }
    Ok(n)
}

/// SPEC §11.4.4.
pub fn spawn_spool_scanner(app: Arc<App>) {
    let loop_app = app.clone();
    crate::background_loop::spawn_restartable(&app, "remote spool scanner", move || {
        let app = loop_app.clone();
        async move { spool_scanner_loop(app).await }
    });
}

async fn spool_scanner_loop(app: Arc<App>) {
        loop {
            tokio::select! {
                _ = app.shutdown.cancelled() => return,
                _ = tokio::time::sleep(hookrecv::SCAN_EVERY) => {}
            }
            for conn in app.hosts.list().await {
                if app.shutdown.is_cancelled() {
                    return;
                }
                if conn.is_local() || !conn.is_connected() {
                    continue;
                }
                let root = crate::startup::remote_root_for(app.instance().as_deref());
                let script = hookrecv::scan_script(&root);
                let pending = match tokio::select! {
                    _ = app.shutdown.cancelled() => return,
                    result = conn.ssh_exec(&script) => result,
                } {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(host = %conn.name, error = ?e, "spool scan failed");
                        continue;
                    }
                };
                let ids: std::collections::HashSet<&str> =
                    pending.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
                if ids.is_empty() {
                    continue;
                }
                for b in db::live_bots_on_host(&app.db, &conn.name).await.unwrap_or_default() {
                    if app.shutdown.is_cancelled() {
                        return;
                    }
                    if !ids.contains(b.id.as_str()) {
                        continue;
                    }
                    let drained = tokio::select! {
                        _ = app.shutdown.cancelled() => return,
                        result = drain_remote_coalesced(&app, &conn.name, &b.id) => result,
                    };
                    if let Err(e) = drained {
                        tracing::debug!(bot = %b.name, host = %conn.name, error = ?e, "scanned drain failed");
                    }
                }
            }
        }
}


#[allow(dead_code)]
pub async fn replay_all(app: &Arc<App>) {
    for host in app.hosts.names().await {
        replay_host(app, &host).await;
    }
}

/// 重放一台 host 的 spool。列舉 bot 讀不到、或某顆 bot 的重放失敗，都不能當成「沒有 spool」：
/// 記下來、背景重試到補齊為止（#243），沒有新的 reconnect／status 事件也會補。
pub async fn replay_host(app: &Arc<App>, host: &str) {
    if app.shutdown.is_cancelled() {
        return;
    }
    let passed = tokio::select! {
        _ = app.shutdown.cancelled() => return,
        passed = replay_host_pass(app, host) => passed,
    };
    if passed {
        return;
    }
    let (app, host) = (app.clone(), host.to_string());
    let loop_app = app.clone();
    crate::background_loop::spawn_restartable(&app, "remote spool replay retry", move || {
        let app = loop_app.clone();
        let host = host.clone();
        async move {
            for _ in 0..REPLAY_RETRIES {
                tokio::select! {
                    _ = app.shutdown.cancelled() => return,
                    _ = tokio::time::sleep(REPLAY_RETRY_EVERY) => {}
                }
                if replay_host_pass(&app, &host).await {
                    return;
                }
            }
            tracing::error!(host, "spool replay still failing after retries; spools stay on the host until the next reconnect");
        }
    });
}

#[cfg(not(test))]
const REPLAY_RETRIES: u32 = 40;
// 測試的間隔是 50ms，40 次只撐 2 秒：runner 負載高時，測試執行緒從 `replay_host` 到把表改回
// 可讀之間就可能超過，背景重試先放棄，spool 永遠收不進來（#255 的另一個偶發來源）。
#[cfg(test)]
const REPLAY_RETRIES: u32 = 400;
#[cfg(not(test))]
const REPLAY_RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(test)]
const REPLAY_RETRY_EVERY: std::time::Duration = std::time::Duration::from_millis(50);

/// 一輪；全部成功才回 true。
async fn replay_host_pass(app: &Arc<App>, host: &str) -> bool {
    if app.shutdown.is_cancelled() {
        return false;
    }
    let bots = match db::live_bots_on_host(&app.db, host).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(host, error = ?e, "spool replay: could not list the host's bots; will retry");
            return false;
        }
    };
    let mut ok = true;
    for b in bots {
        if app.shutdown.is_cancelled() {
            return false;
        }
        let replayed = tokio::select! {
            _ = app.shutdown.cancelled() => return false,
            result = hookrecv::replay_spool(app, &b.id) => result,
        };
        if let Err(e) = replayed {
            tracing::warn!(bot = %b.name, host, error = ?e, "spool replay failed; will retry");
            ok = false;
        }
    }
    ok
}
