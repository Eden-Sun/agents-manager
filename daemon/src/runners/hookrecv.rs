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

impl HookHost for App {
    fn wake_hook_inbox(&self) {
        self.hook_inbox_wake.notify_one();
    }

    fn hook_bot_dir(&self, bot_id: &str) -> Result<std::path::PathBuf> {
        App::bot_dir(&self.shared(), bot_id)
    }

    async fn after_turn_end(&self, body: &HookBody) {
        crate::runners::ask_answers::after_turn_end(&self.shared(), body).await
    }

    async fn background_stop(&self, run: &db::Run, payload: &Value) {
        crate::runners::background_hook::on_stop(&self.shared(), run, payload).await
    }

    async fn transcript_allowed(&self, bot: &db::Bot, path: &str) -> bool {
        crate::app_ports_p5::transcript_allowed(&self.shared(), bot, path).await
    }

    async fn local_transcript_allowed(&self, bot: &db::Bot, path: &str) -> bool {
        crate::app_ports_p5::local_transcript_allowed(&self.shared(), bot, path).await
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
        scan_once(&app).await;
    }
}

/// 一輪掃描：本機 spool 先收，再把每台遠端主機各自掃。主機之間併行（#1067）：一台 ssh 慢、或它的某顆 bot 卡住，
/// 不拖住別台；下一輪要等這一輪的所有主機都回來。
async fn scan_once(app: &Arc<App>) {
    replay_local_spools(app).await;
    let mut scans = tokio::task::JoinSet::new();
    for conn in app.hosts.list().await {
        if conn.is_local() || !conn.is_connected() {
            continue;
        }
        scans.spawn(scan_remote_host(app.clone(), conn));
    }
    while let Some(res) = scans.join_next().await {
        if let Err(e) = res {
            tracing::warn!(error = ?e, "remote spool scan task failed");
        }
    }
}

/// 掃一台遠端主機：ssh 列出有 spool 的 bot，再逐顆 claim 進來。
async fn scan_remote_host(app: Arc<App>, conn: Arc<crate::hosts::HostConn>) {
    if app.shutdown.is_cancelled() {
        return;
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
            return;
        }
    };
    let ids: std::collections::HashSet<&str> = pending.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
    if ids.is_empty() {
        return;
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

/// 本機 spool 的定時重放：daemon 活著時 POST 失敗（503、逾時）寫進 spool 的那幾則，不必等下一次 daemon 重啟。
/// 先看檔案在不在才叫 `replay_spool`（它會拿 bot 鎖），沒有 spool 的 bot 不碰鎖。
async fn replay_local_spools(app: &Arc<App>) -> usize {
    let bots = match db::live_bots_on_host(&app.db, crate::config::LOCAL_HOST).await {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!(error = ?e, "local spool scan: could not list bots");
            return 0;
        }
    };
    let mut n = 0;
    for b in bots {
        if app.shutdown.is_cancelled() {
            break;
        }
        let Ok(dir) = app.hook_bot_dir(&b.id) else { continue };
        let pending = ["hook-spool.jsonl", "hook-spool.jsonl.claim", "hook-spool.jsonl.replaying"]
            .iter()
            .any(|f| dir.join(f).exists());
        if !pending {
            continue;
        }
        match hookrecv::replay_spool_if_idle(app, &b.id).await {
            Ok(Some(k)) => n += k,
            Ok(None) => tracing::debug!(bot = %b.name, "local spool: bot busy; next scan retries"),
            Err(e) => tracing::warn!(bot = %b.name, error = ?e, "local spool replay failed; next scan retries"),
        }
    }
    n
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

#[cfg(test)]
mod local_spool_tests {
    use super::*;
    use crate::testing as tt;

    /// daemon 不重啟，本機 spool 也要被定時重放（#1003）：以前只有開機與 host 連上時才收。
    #[tokio::test]
    async fn a_spooled_local_hook_is_replayed_without_a_restart() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "local-spool").await;
        let dir = app.hook_bot_dir(&bot.id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let line = json!({
            "bot_id": bot.id,
            "provider": "claude",
            "payload": {"hook_event_name": "Stop", "session_id": "s1", "prompt_id": "p1", "last_assistant_message": "done"},
            "received_at": "2026-10-10T00:00:00.000Z",
            "truncated": false,
        });
        std::fs::write(dir.join("hook-spool.jsonl"), format!("{line}\n")).unwrap();

        assert_eq!(replay_local_spools(&app).await, 1);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hook_events WHERE bot_id = ?")
            .bind(&bot.id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(!dir.join("hook-spool.jsonl").exists());
        assert!(!dir.join("hook-spool.jsonl.replaying").exists());
        // 沒有 spool 的 bot 不重放、也不碰鎖。
        assert_eq!(replay_local_spools(&app).await, 0);
    }

    fn spool_line(bot_id: &str, prompt_id: &str) -> String {
        json!({
            "bot_id": bot_id,
            "provider": "claude",
            "payload": {"hook_event_name": "Stop", "session_id": "s1", "prompt_id": prompt_id, "last_assistant_message": "done"},
            "received_at": format!("2026-10-10T00:00:00.000Z-{prompt_id}"),
            "truncated": false,
        })
        .to_string()
    }

    /// 已經收進收件匣（`hook_events`）的列數：spool 收進來就有一列，不必等 worker 處理。
    async fn ingested(app: &Arc<App>, bot_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM hook_events WHERE bot_id = ?").bind(bot_id).fetch_one(&app.db).await.unwrap()
    }

    /// #1067：A 握著自己的鎖（像重啟等就緒）時，同一輪掃描裡 B 的本機 spool 與遠端主機上 C 的 spool 仍要收進來；
    /// A 的留著，放鎖後下一輪再收。以前本機重放是排隊等 A 的鎖，後面的 B 與整個遠端掃描都停在那裡。
    #[tokio::test]
    async fn a_bot_holding_its_lock_does_not_stall_the_local_or_remote_scan() {
        let env = tt::env().await;
        let app = env.app.clone();
        let a = tt::claude_bot(&app, &env.project_id, "spool-stall-a").await;
        let b = tt::claude_bot(&app, &env.project_id, "spool-stall-b").await;
        for (bot, p) in [(&a, "pa"), (&b, "pb")] {
            let dir = app.hook_bot_dir(&bot.id).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("hook-spool.jsonl"), format!("{}\n", spool_line(&bot.id, p))).unwrap();
        }

        // 遠端：一台 host 上一顆 bot C（掃描腳本回 C 的 id，claim 腳本回 C 的一行）。
        let host = format!("spool-stall-{}", db::ulid());
        let conn = app
            .hosts
            .insert_remote_for_test(crate::config::HostCfg {
                shared_session: false,
                name: host.clone(),
                ssh: "unused".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "am-test".into(),
                remote_path: String::new(),
            })
            .await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let project = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'remote', ?, ?)")
            .bind(&project)
            .bind(format!("/remote/{project}"))
            .bind(&host)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let c = tt::claude_bot(&app, &project, "spool-stall-c").await;
        let (c_id, c_line) = (c.id.clone(), spool_line(&c.id, "pc"));
        crate::hosts::set_ssh_fake(&host, move |script| {
            if script.contains("am_pending") {
                return Ok(format!("{c_id}\n"));
            }
            if script.contains("umask 077") {
                return Ok(format!("{c_line}\n"));
            }
            Ok(String::new())
        });

        let lock = app.bot_lock(&a.id).await;
        let held = lock.lock().await;
        let scan_app = app.clone();
        let scan = tokio::spawn(async move { scan_once(&scan_app).await });
        assert!(tt::eventually!(ingested(&app, &b.id).await == 1), "A 握著鎖時，B 的本機 spool 仍要收進來");
        assert!(tt::eventually!(ingested(&app, &c.id).await == 1), "A 握著鎖時，遠端主機上 C 的 spool 仍要收進來");
        assert_eq!(ingested(&app, &a.id).await, 0, "A 還卡在它的鎖上");

        // 前提：A 的鎖整段都還握著。放鎖後，下一輪把 A 的 spool 收進來。
        assert!(lock.try_lock().is_err(), "前提：A 的鎖整段都還握著");
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(30), scan).await.expect("這一輪不能卡在 A 的鎖上").unwrap();
        scan_once(&app).await;
        assert_eq!(ingested(&app, &a.id).await, 1, "放鎖後下一輪 A 的 spool 收進來");
        assert!(!app.hook_bot_dir(&a.id).unwrap().join("hook-spool.jsonl").exists());
    }
}
