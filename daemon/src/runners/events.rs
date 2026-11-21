//! Runner tasks and event-loop adapters for herdr events.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use serde_json::json;

use crate::config::LOCAL_HOST;
use crate::events::{
    close_retry_delays, detection_wants_reconcile, forget_watcher,
    herdr_subscribe_retry_delay, norm, pane_status_lock, poll_titles,
    reconcile_and_autostart, status_replay_delays, status_seq,
    unwatch_pane_on_session, watcher_gens, PaneKey, DRAIN_BUDGET,
    HERDR_SUBSCRIBE_INITIAL_BACKOFF, HERDR_SUBSCRIBE_MAX_BACKOFF,
};
use crate::capabilities::{BotStatusEmit, Emit, HerdrRoutes};
use crate::events::ports::{HandoffRepo, HostSidePort, ProviderPort, ReconcileCommands, SupervisorSignals, TurnCommands};
use crate::state::App;

/// 起（或重起）一個 host 的全域訂閱。換掉舊 task 與登記新的在同一把鎖內，否則兩次快速呼叫會留下
/// 兩個迴圈各自對帳。
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
    let t = tokio::spawn(Box::pin(async move { global_loop(a, h, s).await }));
    g.insert(key, t);
}

/// `main.rs` 用的本機捷徑。
pub async fn spawn_global(app: Arc<App>) {
    spawn_global_for_host(app, LOCAL_HOST.to_string()).await;
}

async fn global_loop(app: Arc<App>, host: String, session: String) {
    let is_local_main = host == LOCAL_HOST && session == app.herdr_session.as_str();
    let is_local_default = host == LOCAL_HOST && session == "default";
    let mut backoff = HERDR_SUBSCRIBE_INITIAL_BACKOFF;
    let mut reconnect_pending = false;
    let mut recovery_guard = crate::herdr_recovery::RecoveryGuard::default();
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
        let mut connected_for = None;
        match client.subscribe(subs).await {
            Ok(mut rx) => {
                if is_local_main {
                    app.connected.store(true, Ordering::SeqCst);
                    app.emit_daemon_status().await;
                }
                if is_local_default {
                    app.set_default_connected(true).await;
                }
                tracing::info!(host = %host, session = %session, "global herdr event subscription established");
                if is_local_main || host != LOCAL_HOST {
                    let (app2, host2) = (app.clone(), host.clone());
                    tokio::spawn(async move { app2.refresh_herdr_version(&host2).await });
                }
                if is_local_main && reconnect_pending {
                    if let Some(report) = crate::herdr_recovery::recover_after_reconnect(&app, &client, &mut recovery_guard).await {
                        tracing::info!(
                            resumed = report.resumed.len(),
                            failed = report.failed.len(),
                            children = report.children_lost.len(),
                            limited = report.limited,
                            skipped = ?report.skipped,
                            "herdr unexpected-restart recovery finished"
                        );
                        crate::herdr_recovery::publish_recovery_report(&app, &report).await;
                    }
                    reconnect_pending = false;
                }
                if is_local_default {
                    if let Err(e) = app.sync_default_session().await {
                        tracing::debug!(host = %host, session = %session, error = ?e, "default session sync failed");
                    }
                } else {
                    reconcile_and_autostart(&app, &host, host != LOCAL_HOST).await;
                }
                let connected_at = Instant::now();
                while let Some(ev) = rx.recv().await {
                    handle_global(&app, &host, &session, &ev).await;
                }
                connected_for = Some(connected_at.elapsed());
                if is_local_main {
                    app.connected.store(false, Ordering::SeqCst);
                    app.emit_daemon_status().await;
                    reconnect_pending = true;
                }
                if is_local_default {
                    app.set_default_connected(false).await;
                }
                tracing::warn!(host = %host, session = %session, "global herdr event subscription dropped");
            }
            Err(e) => {
                if is_local_main {
                    app.connected.store(false, Ordering::SeqCst);
                    app.emit_daemon_status().await;
                }
                if is_local_default {
                    app.set_default_connected(false).await;
                }
                tracing::debug!(host = %host, session = %session, error = %e, "herdr subscribe failed");
            }
        }
        if host != LOCAL_HOST && !app.host_connected(&host).await {
            tracing::info!(host = %host, "host disconnected; global subscription yields to the supervisor");
            return;
        }
        let delay = herdr_subscribe_retry_delay(backoff, connected_for);
        tokio::time::sleep(delay).await;
        backoff = (delay * 2).min(HERDR_SUBSCRIBE_MAX_BACKOFF);
    }
}

pub async fn handle_global(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event) {
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
            close_workspace_try(app, host, session, ws, 0).await;
        }
        "pane_agent_detected" => {
            tracing::debug!(host, session, data = %ev.data, "pane.agent_detected");
            if host == LOCAL_HOST && session == "default" {
                if let Err(e) = app.sync_default_session().await {
                    tracing::debug!(host, session, error = ?e, "default session sync after agent detection failed");
                }
            } else if app.session_for_host(host).await.as_deref() == Some(session) && detection_wants_reconcile(app, host, session, &ev.data).await {
                let app = app.clone();
                let host = host.to_string();
                tokio::spawn(Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    crate::runners::reconcile::request_immediate_pass(&app, &host);
                }));
            }
        }
        other => tracing::trace!(host, session, event = other, "unhandled global herdr event"),
    }
}

async fn end_runs_for_pane(app: &Arc<App>, host: &str, session: &str, pane_id: &str) {
    end_runs_for_pane_try(app, host, session, pane_id, 0).await
}

/// pane 已經沒了（外部事實）。順序：找出受影響的 run → 收成 exited → **才**拆 watcher。
/// 任何一步讀寫不到就不拆 watcher（它還能提供證據）、也不當成沒有 run，稍後整段重來；
/// `mark_run_exited` 靠 CAS，重複收是安全的。
async fn end_runs_for_pane_try(app: &Arc<App>, host: &str, session: &str, pane_id: &str, attempt: usize) {
    let Some(fallback) = app.session_for_host(host).await else {
        tracing::warn!(host, pane_id, "pane closed but the host's session is unknown; will retry");
        retry_pane_close(app, host, session, pane_id, attempt);
        return;
    };
    let runs = match crate::db::active_runs_for_pane(&app.db, host, pane_id, session, &fallback).await {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(host, pane_id, error = ?e, "could not look up the runs of a closed pane; watcher kept, retrying");
            retry_pane_close(app, host, session, pane_id, attempt);
            return;
        }
    };
    let mut ended_a_child = false;
    let mut converged = true;
    for r in runs {
        if matches!(app.mark_run_exited(&r.id, "pane exited").await, crate::lc_error::RunExit::NotRecorded) {
            converged = false;
            continue;
        }
        if matches!(crate::db::bot(&app.db, &r.bot_id).await, Ok(Some(b)) if b.managed_by == "child") {
            ended_a_child = true;
        }
    }
    if !converged {
        tracing::warn!(host, pane_id, "a closed pane's run could not be recorded as exited; watcher kept, retrying");
        retry_pane_close(app, host, session, pane_id, attempt);
    }
    if converged {
        unwatch_pane_on_session(app, host, session, pane_id).await;
    }
    if ended_a_child {
        let app = app.clone();
        let host = host.to_string();
        tokio::spawn(Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Err(e) = app.reconcile_host(&host).await {
                tracing::debug!(host = %host, error = ?e, "reconcile after a child's pane closed failed");
            }
        }));
    }
}

/// workspace 關閉（#245）：外部事實，事件只來一次。DB 讀不到就不能當成沒有 run／沒有綁定，
/// 也不能發出「project_changed」假裝清好了；任何一步沒成就整段稍後重來（每一步都是冪等的）。
async fn close_workspace_try(app: &Arc<App>, host: &str, session: &str, ws: &str, attempt: usize) {
    let mut converged = true;
    match crate::db::live_bots_on_host(&app.db, host).await {
        Ok(bots) => {
            for b in bots {
                match crate::db::active_run(&app.db, &b.id).await {
                    Ok(Some(r)) => {
                        if r.workspace_id.as_deref() == Some(ws)
                            && app.session_for_run(&r).await.as_deref() == Some(session)
                            && matches!(app.mark_run_exited(&r.id, "workspace closed").await, crate::lc_error::RunExit::NotRecorded)
                        {
                            converged = false;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(host, workspace_id = ws, bot = %b.id, error = ?e, "could not read a bot's run after its workspace closed");
                        converged = false;
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(host, workspace_id = ws, error = ?e, "could not list the host's bots after a workspace closed");
            converged = false;
        }
    }
    if app.session_for_host(host).await.as_deref() == Some(session) {
        match sqlx::query("UPDATE projects SET workspace_id = NULL WHERE workspace_id = ? AND host = ? AND handed_off_to IS NULL").bind(ws).bind(host).execute(&app.db).await {
            Ok(_) => app.emit("project_changed", json!({})).await,
            Err(e) => {
                tracing::warn!(host, workspace_id = ws, error = ?e, "could not clear the closed workspace's project binding");
                converged = false;
            }
        }
    }
    if !converged {
        retry_workspace_close(app, host, session, ws, attempt);
    }
}

fn retry_workspace_close(app: &Arc<App>, host: &str, session: &str, ws: &str, attempt: usize) {
    let Some(wait) = close_retry_delays().get(attempt).copied() else {
        tracing::warn!(host, workspace_id = ws, "gave up retrying a workspace-close; periodic reconcile is the safety net");
        return;
    };
    let (app, host, session, ws) = (app.clone(), host.to_string(), session.to_string(), ws.to_string());
    tokio::spawn(Box::pin(async move {
        tokio::time::sleep(wait).await;
        close_workspace_try(&app, &host, &session, &ws, attempt + 1).await;
    }));
}

fn retry_pane_close(app: &Arc<App>, host: &str, session: &str, pane_id: &str, attempt: usize) {
    let Some(wait) = close_retry_delays().get(attempt).copied() else {
        tracing::warn!(host, pane_id, "gave up retrying a pane-close; periodic reconcile is the safety net");
        return;
    };
    let (app, host, session, pane_id) = (app.clone(), host.to_string(), session.to_string(), pane_id.to_string());
    tokio::spawn(Box::pin(async move {
        tokio::time::sleep(wait).await;
        end_runs_for_pane_try(&app, &host, &session, &pane_id, attempt + 1).await;
    }));
}

/// Open (or replace) the per-run status subscription in the host's configured session.
pub async fn watch_pane(app: &Arc<App>, host: &str, pane_id: &str) {
    let Some(session) = app.session_for_host(host).await else { return };
    watch_pane_on_session(app, host, &session, pane_id).await;
}

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
    let handle = tokio::spawn(Box::pin(async move {
        let mut backoff = HERDR_SUBSCRIBE_INITIAL_BACKOFF;
        let mut resubscribing = false;
        let mut run_ended = false;
        loop {
            let Some(client) = app2.herdr_for_session(&hst, &sess).await else { break };
            let subs = vec![json!({"type": "pane.agent_status_changed", "pane_id": pid})];
            let mut connected_for = None;
            match client.subscribe(subs).await {
                Ok(mut rx) => {
                    tracing::info!(host = %hst, session = %sess, pane_id = %pid, "watching pane agent status");
                    if resubscribing {
                        resync_pane_status(&app2, &client, &hst, &sess, &pid).await;
                    }
                    resubscribing = true;
                    let connected_at = Instant::now();
                    while let Some(ev) = rx.recv().await {
                        handle_status(&app2, &hst, &sess, &ev).await;
                    }
                    connected_for = Some(connected_at.elapsed());
                    tracing::info!(host = %hst, session = %sess, pane_id = %pid, "pane status subscription dropped");
                }
                Err(e) => {
                    resubscribing = true;
                    tracing::debug!(host = %hst, session = %sess, pane_id = %pid, error = %e, "pane status subscribe failed");
                }
            }
            match app2.session_for_host(&hst).await {
                None => tracing::warn!(host = %hst, pane_id = %pid, "watcher cannot tell the host's session; keeping it"),
                Some(fallback) => match crate::db::active_runs_for_pane(&app2.db, &hst, &pid, &sess, &fallback).await {
                    Ok(runs) if runs.is_empty() => {
                        run_ended = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(host = %hst, pane_id = %pid, error = ?e, "watcher cannot read the pane's runs; keeping it"),
                },
            }
            let delay = herdr_subscribe_retry_delay(backoff, connected_for);
            tokio::time::sleep(delay).await;
            backoff = (delay * 2).min(HERDR_SUBSCRIBE_MAX_BACKOFF);
        }
        forget_watcher(&app2, own, generation, run_ended).await;
    }));
    g.insert(key, handle);
}

/// 狀態訂閱斷過再接上（herdr 重啟、或 0.9.2+ 讀太慢被回 `events_lost` 收掉）：斷掉那段的狀態邊已經漏了——
/// 漏掉 `working → idle` 的話燈號就凍在 working 直到下一則事件。訂閱**先**接上再讀 pane 當下的狀態，照一則狀態事件處理
/// （讀完之後的變化由新訂閱送，不留縫）。讀不到或 pane 已經不在就不補：pane 收掉由全域訂閱重連時的 reconcile 處理。
async fn resync_pane_status(app: &Arc<App>, client: &crate::herdr::HerdrClient, host: &str, session: &str, pane_id: &str) {
    let pane = match client.pane_get(pane_id).await {
        Ok(Some(pane)) => pane,
        Ok(None) => return,
        Err(e) => {
            tracing::debug!(host, session, pane_id, error = ?e, "could not re-read a pane's status after resubscribing");
            return;
        }
    };
    let Some(status) = pane.agent_status else { return };
    let ev = crate::herdr::Event {
        event: "pane.agent_status_changed".into(),
        data: json!({"pane_id": pane_id, "agent": pane.agent, "agent_status": status.as_str()}),
    };
    handle_status(app, host, session, &ev).await;
}

pub async fn handle_status(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event) {
    handle_status_try(app, host, session, ev, 0, None).await
}

/// 狀態寫不進 DB 的 pane，等修復（#1184）。鍵是 pane；值帶著寫失敗那時的 run：修復時 pane 上的 run 已經換了就丟掉。
/// 放在 [`App`] 上、只在記憶體裡：daemon 重啟或主機重連時 `reconcile` 會把 DB 狀態重新對回 herdr，那條路徑本來就是修復。
#[derive(Clone)]
pub(crate) struct StatusRepair {
    /// `None`＝寫失敗時連 run 都查不出來（#1293）：修復時拿 pane 當下的 run，不拿舊的 run 比對。
    run_id: Option<String>,
    since: Instant,
    /// 每次登記（含覆寫）都換一個號碼：修復清掉的是**它自己拿到的那一筆**，不是 pane 底下當下的任何一筆（#1290）。
    gen: u64,
}

fn next_repair_gen() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed) + 1
}

/// 修復迴圈的週期。重放用完之後靠它補，不是靠 `child_alerts::sweep`（那個只看子 agent 的提問）。
const STATUS_REPAIR_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// 同一個 pane 又寫失敗：用這一次寫失敗時的 run 取代舊登記（#1290）。舊的 run 已經換掉，留著它只會讓修復把新 run 丟掉。
/// 登記（`run_id` 為 `None` 表示 run 還查不出來，#1293）。
fn mark_status_repair(app: &App, key: &PaneKey, run_id: Option<&str>) {
    app.status_repairs.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), StatusRepair { run_id: run_id.map(str::to_string), since: Instant::now(), gen: next_repair_gen() });
}

/// 寫進去了（或本來就一致）：這個 pane 不再欠 DB 一個狀態。
fn clear_status_repair(app: &App, key: &PaneKey) {
    app.status_repairs.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
}

/// 只清掉 `gen` 這一筆；登記之後已經被新的寫失敗覆寫的，留著（#1290：非同步的修復不能抹掉較新的登記）。
fn clear_status_repair_if(app: &App, key: &PaneKey, gen: u64) {
    let mut m = app.status_repairs.lock().unwrap_or_else(|e| e.into_inner());
    if m.get(key).is_some_and(|r| r.gen == gen) {
        m.remove(key);
    }
}

pub fn spawn_status_repair(app: Arc<App>) {
    crate::background_loop::spawn_periodic(&app, "status repair", STATUS_REPAIR_EVERY, STATUS_REPAIR_EVERY, |app| async move {
        repair_pending_statuses(&app).await;
    });
}

/// 待修復的 pane 各重新對一次 herdr：重讀**當下**的狀態再寫，不重放舊事件，所以不會把舊的 idle 蓋回新的 working。
/// pane 上的 run 已經不是登記時那一個就丟掉（那一筆跟著舊 run 結束）。回傳這一輪結束時還剩幾個。
pub async fn repair_pending_statuses(app: &Arc<App>) -> usize {
    let pending: Vec<(PaneKey, StatusRepair)> = app.status_repairs.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (key, repair) in &pending {
        let (host, session, pane_id) = key;
        let fallback = app.session_for_host(host).await.unwrap_or_default();
        let runs = match crate::db::active_runs_for_pane(&app.db, host, pane_id, session, &fallback).await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::debug!(host, pane_id, error = ?e, "status repair: could not look up the pane's run; trying again");
                continue;
            }
        };
        #[cfg(test)]
        crate::race_point::hit("status_repair_after_lookup", pane_id).await;
        let stale = match &repair.run_id {
            Some(id) => runs.first().map(|r| r.id.as_str()) != Some(id.as_str()),
            None => runs.is_empty(),
        };
        if stale {
            clear_status_repair_if(app, key, repair.gen);
            continue;
        }
        let Some(run) = runs.first() else { continue };
        match app.db.bot_handed_off_to(&run.bot_id).await {
            Ok(None) => {}
            Ok(Some(_)) => {
                clear_status_repair_if(app, key, repair.gen);
                continue;
            }
            Err(_) => continue,
        }
        let Some(client) = app.herdr_for_session(host, session).await else { continue };
        resync_pane_status(app, &client, host, session, pane_id).await;
    }
    let left = app.status_repairs.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(oldest) = left.values().map(|r| r.since.elapsed()).max() {
        tracing::info!(pending = left.len(), oldest_secs = oldest.as_secs(), "pane status repair backlog");
    }
    left.len()
}

fn replay_status_later(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event, key: PaneKey, seq: u64, attempt: usize) {
    let Some(wait) = status_replay_delays().get(attempt).copied() else {
        tracing::warn!(host, pane = %key.2, "gave up replaying a pane status event; the status repair loop re-reads the pane");
        return;
    };
    let (app, host, session, ev) = (app.clone(), host.to_string(), session.to_string(), ev.clone());
    tokio::spawn(Box::pin(async move {
        tokio::time::sleep(wait).await;
        if status_seq().lock().unwrap().get(&key) != Some(&seq) {
            return;
        }
        handle_status_try(&app, &host, &session, &ev, attempt + 1, Some(seq)).await;
    }));
}

async fn handle_status_try(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event, attempt: usize, replay_seq: Option<u64>) {
    let name = norm(&ev.event);
    if name != "pane_agent_status_changed" {
        tracing::trace!(event = %ev.event, "unhandled pane event");
        return;
    }
    let Some(pane_id) = ev.data.get("pane_id").and_then(|v| v.as_str()) else { return };
    let reported_status = ev
        .data
        .get("agent_status")
        .and_then(|v| v.as_str())
        .map(|s| match s {
            "done" => "idle",
            "idle" | "working" | "blocked" | "unknown" => s,
            _ => "unknown",
        })
        .unwrap_or("unknown")
        .to_string();
    tracing::info!(host, session, pane_id, status = %reported_status, "pane.agent_status_changed");
    let key: PaneKey = (host.to_string(), session.to_string(), pane_id.to_string());
    let seq = match replay_seq {
        Some(seq) => seq,
        None => {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed) + 1;
            status_seq().lock().unwrap().insert(key.clone(), n);
            n
        }
    };
    let pane_lock = pane_status_lock(&key);
    let write_guard = pane_lock.lock().await;
    if replay_seq.is_some() && status_seq().lock().unwrap().get(&key) != Some(&seq) {
        return;
    }

    let fallback = app.session_for_host(host).await.unwrap_or_default();
    let runs = match crate::db::active_runs_for_pane(&app.db, host, pane_id, session, &fallback).await {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(host, pane_id, status = %reported_status, error = ?e, "could not look up the run for a pane status event; replaying it shortly");
            mark_status_repair(app, &key, None);
            replay_status_later(app, host, session, ev, key, seq, attempt);
            return;
        }
    };
    let Some(run) = runs.into_iter().next() else {
        clear_status_repair(app, &key);
        return;
    };
    let status = if reported_status == "unknown"
        && ev.data.get("agent").and_then(serde_json::Value::as_str) == Some("codex")
        && run.state != "starting"
    {
        "idle".to_string()
    } else {
        reported_status
    };
    match app.db.bot_handed_off_to(&run.bot_id).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            clear_status_repair(app, &key);
            return;
        }
        Err(e) => {
            tracing::warn!(host, pane_id, status = %status, error = ?e, "could not tell whether a pane's project was handed off; replaying it shortly");
            mark_status_repair(app, &key, Some(&run.id));
            replay_status_later(app, host, session, ev, key, seq, attempt);
            return;
        }
    }
    crate::dangerous_rm::on_herdr_status(&run.id);
    let prev = run.agent_status.clone();
    app.observe_agent_status(&run.id, &status);
    app.observe_idle_status(&run.id, &status);
    if prev != status {
        if let Err(e) = sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?")
            .bind(&status)
            .bind(&run.id)
            .execute(&app.db)
            .await
        {
            tracing::warn!(run = %run.id, status = %status, error = ?e, "could not persist the agent status; replaying it shortly");
            mark_status_repair(app, &key, Some(&run.id));
            drop(write_guard);
            replay_status_later(app, host, session, ev, key, seq, attempt);
            return;
        }
    }
    clear_status_repair(app, &key);
    drop(write_guard);
    app.emit_bot_status(&run.bot_id).await;

    if status == "working" || status == "blocked" {
        app.cancel_stall(&run.id).await;
    }
    if matches!(status.as_str(), "idle" | "working" | "blocked") {
        app.poke_resume_nudge(&run.bot_id);
    }

    if prev != "blocked" && status == "blocked" {
        let (app2, run2) = (app.clone(), run.clone());
        tokio::spawn(async move {
            app2.dismiss_survey_if_shown(&run2).await;
        });
        if crate::db::bot(&app.db, &run.bot_id).await.ok().flatten().is_some_and(|b| b.kind == "grok") {
            app.schedule_codex_notice_capture(&run.bot_id, &run.id);
        }
        crate::runners::child_alerts::on_child_blocked(app, &run);
        crate::runners::dangerous_rm::on_blocked(app, &run);
        app.codex_migration_on_blocked(&run);
        {
            let (app2, run2) = (app.clone(), run.clone());
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                if let Ok(Some(fresh)) = crate::db::run(&app2.db, &run2.id).await {
                    if fresh.agent_status == "blocked" {
                        crate::blocked_reason::observe(&app2, &fresh).await;
                        crate::runners::codex_async_question::observe(&app2, &fresh).await;
                        if crate::lifecycle::poller::settle_false_blocked(&app2, &fresh).await.unwrap_or(false) {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                        if let Ok(Some(again)) = crate::db::run(&app2.db, &run2.id).await {
                            if again.agent_status == "blocked" {
                                let _ = crate::lifecycle::poller::settle_false_blocked(&app2, &again).await;
                            }
                        }
                    }
                }
            });
        }
    }
    if prev == "blocked" && status != "blocked" {
        crate::blocked_reason::forget(&run.id);
    }
    if prev != "idle" && status == "idle" {
        app.session_paused_on_idle(&run);
    }
    if prev == "blocked" && status != "blocked" {
        crate::child_alerts::forget(&run.bot_id);
    }

    if prev != "working" && status == "working" && matches!(crate::db::in_flight_turn(&app.db, &run.id).await, Ok(None)) {
        app.begin_external_turn(&run).await;
    }

    if host != LOCAL_HOST && ((prev == "working" && status == "idle") || (prev != "blocked" && status == "blocked")) {
        let drain = app.drain_remote_coalesced(host, &run.bot_id);
        match tokio::time::timeout(DRAIN_BUDGET, drain).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(host, bot_id = %run.bot_id, error = ?e, "remote drain failed"),
            Err(_) => tracing::warn!(host, bot_id = %run.bot_id, "remote drain timed out"),
        }
    }

    if status != "idle" {
        crate::prompt_suggestion::forget(&run.id);
        crate::runners::composer_draft_watch::on_not_idle(&app, &run);
    } else if prev != "idle" {
        app.prompt_suggestion_on_idle(&run);
        crate::runners::composer_draft_watch::on_idle(&app, &run);
    }

    let inflight = if prev == "working" && status == "idle" {
        true
    } else if prev == "blocked" && status == "idle" {
        crate::db::in_flight_turn(&app.db, &run.id).await.ok().flatten().is_some()
    } else {
        false
    };
    if inflight {
        app.arm_fallback(&run.id, &run.bot_id).await;
        app.schedule_flush_queued(&run.bot_id);
        app.schedule_deferred_live(&run.bot_id);
        crate::update_restart::schedule(app, &run.bot_id);
    } else if prev != "idle" && status == "idle" {
        app.schedule_flush_queued(&run.bot_id);
        app.schedule_deferred_live(&run.bot_id);
        crate::update_restart::schedule(app, &run.bot_id);
    }

    if prev != "idle" && status == "idle" && run.adopted != 0 {
        crate::lifecycle::poller::spawn_idle_transcript_sync(app, &run.id, &run.bot_id);
    }
}

const TITLE_POLL: std::time::Duration = std::time::Duration::from_secs(4);

/// 讓 `runs.agent_title` 跟上 agent 自己現在的標題（claude 會寫成它正在做的事）。
pub fn spawn_title_poller(app: Arc<App>) {
    crate::background_loop::spawn_periodic(&app, "title poller", TITLE_POLL, TITLE_POLL, |app| async move {
        for conn in app.hosts.list().await {
            if !conn.is_local() && !conn.is_connected() {
                continue;
            }
            let Some(session) = app.session_for_host(&conn.name).await else { continue };
            let Some(client) = app.herdr_for_session(&conn.name, &session).await else { continue };
            poll_titles(&app, &conn.name, &session, &session, &client).await;
        }
        if app.herdr_session.as_str() != "default" && app.default_connected.load(Ordering::SeqCst) {
            let fallback = app.herdr_session.clone();
            let client = app.default_herdr.clone();
            poll_titles(&app, LOCAL_HOST, "default", &fallback, &client).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::events::{pane_status_locks, status_seq, watcher_gens, PaneKey};
    use crate::testing as tt;

    async fn run_state(app: &Arc<App>, run: &str) -> String {
        sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(run).fetch_one(&app.db).await.unwrap()
    }

    /// #259：supervisor 連上那一輪對帳失敗（autostart 跳過、主機沒記成跑過），連線沒斷、訂閱建好後對帳成功——
    /// 欠著的 autostart 要在這補；補過之後再對帳不會再起，使用者停掉的也不會被重開。
    #[tokio::test]
    async fn a_later_successful_subscribe_reconcile_pays_the_owed_autostart() {
        let e = tt::env().await;
        let app = &e.app;
        let bot = tt::claude_bot(app, &e.project_id, "auto").await;
        sqlx::query("UPDATE bots SET autostart=1 WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let active = || async { crate::db::active_run(&app.db, &bot.id).await.unwrap().is_some() };

        e.herdr.fail_next("session.snapshot", tt::Fault::Refuse);
        assert!(!reconcile_and_autostart(app, LOCAL_HOST, true).await, "對帳失敗");
        assert!(!active().await && app.autostart_hosts.lock().unwrap().is_empty(), "失敗不算數：沒起、主機沒記成跑過");

        assert!(reconcile_and_autostart(app, LOCAL_HOST, true).await, "之後對帳成功");
        assert!(crate::testing::eventually!(active().await), "欠著的 autostart 在這補上");

        assert!(reconcile_and_autostart(app, LOCAL_HOST, true).await);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&bot.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(runs, 1, "只跑一次：不再替它開新的 run");
    }

    /// 訂閱（重）建後的那一輪對帳失敗（herdr 剛重啟、snapshot 讀不到）：斷線那段漏掉的 `pane.closed` 沒人補，
    /// run 會一直停在 running，直到下一個 herdr 事件或重啟。事件驅動的對帳失敗必須自己排補跑。
    #[tokio::test]
    async fn a_failed_reconcile_after_resubscribing_is_retried_on_its_own() {
        let e = tt::env().await;
        let app = &e.app;
        e.herdr.fail_next("session.snapshot", tt::Fault::Refuse);
        assert!(!reconcile_and_autostart(app, LOCAL_HOST, false).await, "對帳失敗");
        let after_failure = e.herdr.calls_to("session.snapshot").len();
        assert!(
            crate::testing::eventually!(e.herdr.calls_to("session.snapshot").len() > after_failure),
            "沒有任何補跑：失敗的對帳要等到下一個事件才會再來"
        );
    }

    async fn plant_watcher(app: &Arc<App>, pane: &str) {
        let h = tokio::spawn(std::future::pending::<()>());
        app.pane_watchers.lock().await.insert((LOCAL_HOST.into(), "test".into(), pane.into()), h);
    }

    async fn watching(app: &Arc<App>, pane: &str) -> bool {
        app.pane_watchers.lock().await.contains_key(&(LOCAL_HOST.to_string(), "test".to_string(), pane.to_string()))
    }

    /// #244：訂閱掉線後的「還有沒有 active run」讀不到，以前當成 0 個、watcher 永久退出。現在不退；DB 好了它照樣在，run 真的結束才退。
    #[tokio::test]
    async fn a_watcher_whose_liveness_read_fails_keeps_watching() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "watched").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);

        tt::make_table_unreadable(&app, "runs").await;
        watch_pane_on_session(&app, LOCAL_HOST, "test", &pane).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(watching(&app, &pane).await, "讀不到 run 時 watcher 不能自己退出");
        tt::make_table_readable(&app, "runs").await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(watching(&app, &pane).await, "run 還活著，watcher 仍在");

        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let _ = crate::testing::eventually!(!watching(&app, &pane).await);
        assert!(!watching(&app, &pane).await, "確認 run 已結束才退出");
    }

    /// watcher 因為 run 結束自己退出（不是被 `unwatch_pane` 拆掉）：pane 的狀態序號與鎖也要跟著走。
    #[tokio::test]
    async fn a_watcher_that_exits_on_its_own_takes_the_panes_status_state_with_it() {
        let e = tt::env().await;
        let app = e.app.clone();
        let key: PaneKey = (LOCAL_HOST.to_string(), "test".to_string(), "pane-self-exit".to_string());
        status_seq().lock().unwrap().insert(key.clone(), 5);
        let _ = pane_status_lock(&key);
        watcher_gens().lock().unwrap().insert(key.clone(), 7);
        app.pane_watchers.lock().await.insert(key.clone(), tokio::spawn(std::future::pending::<()>()));

        forget_watcher(&app, key.clone(), 7, true).await;
        assert!(!status_seq().lock().unwrap().contains_key(&key), "狀態序號要清掉");
        assert!(!pane_status_locks().lock().unwrap().contains_key(&key), "pane 的鎖要清掉");
        assert!(!app.pane_watchers.lock().await.contains_key(&key));

        status_seq().lock().unwrap().insert(key.clone(), 9);
        watcher_gens().lock().unwrap().insert(key.clone(), 8);
        forget_watcher(&app, key.clone(), 7, true).await;
        assert!(status_seq().lock().unwrap().contains_key(&key), "新的 watcher 的狀態不被舊的帶走");
        status_seq().lock().unwrap().remove(&key);
        watcher_gens().lock().unwrap().remove(&key);
    }

    /// watcher 也會因為「這台主機／session 現在拿不到 herdr client」（遠端斷線、session 對不上）離開迴圈——那不是 run 結束，
    /// run 還活著、之後重連會再裝 watcher。這條路徑不能把 pane 的狀態序號與鎖帶走：讀不到 run 而延後重放的那一則 `blocked`
    /// 是 child 的父 agent 唯一會被告知的一次（#192），序號被清掉它就被當成「已經有更新的事件」而放棄；鎖被換掉則讓
    /// 兩個處理同一個 pane 的狀態事件可以同時跑。
    #[tokio::test]
    async fn a_watcher_that_leaves_because_the_host_is_unreachable_keeps_the_panes_status_state() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "unreach").await;
        let _run = tt::fake_run(&app, &bot.id).await;
        let session = "unreachable-session";
        let pane = format!("pane-{}", bot.id);
        let key: PaneKey = (LOCAL_HOST.to_string(), session.to_string(), pane.clone());
        status_seq().lock().unwrap().insert(key.clone(), 41);
        let lock_before = pane_status_lock(&key);

        watch_pane_on_session(&app, LOCAL_HOST, session, &pane).await;
        let gone = crate::testing::eventually!(!app.pane_watchers.lock().await.contains_key(&key));
        assert!(gone, "拿不到 client 時 watcher 退出（之後重連再裝）");
        assert_eq!(status_seq().lock().unwrap().get(&key), Some(&41), "run 還活著：等著重放的那一則不能被丟掉");
        assert!(Arc::ptr_eq(&lock_before, &pane_status_lock(&key)), "pane 的鎖不能被換掉");
        crate::events::forget_pane_status_state(&key);
    }

    fn ws_event(ws: &str) -> crate::herdr::Event {
        crate::herdr::Event { event: "workspace.closed".into(), data: json!({"workspace_id": ws}) }
    }

    async fn project_ws(app: &Arc<App>, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT workspace_id FROM projects WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    /// #245：workspace.closed 時讀不到 bot／run 以前當成「沒有」，run 留活、綁定殘留；現在重試到收斂，只動符合的 workspace。
    #[tokio::test]
    async fn a_workspace_close_whose_reads_fail_is_retried_until_it_converges() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "wsbot").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let other = tt::claude_bot(&app, &e.project_id, "other").await;
        let other_run = tt::fake_run(&app, &other.id).await;
        sqlx::query("UPDATE runs SET workspace_id='ws-2' WHERE id=?").bind(&other_run).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE projects SET workspace_id='ws-1' WHERE id=?").bind(&e.project_id).execute(&app.db).await.unwrap();

        tt::make_table_unreadable(&app, "bots").await;
        handle_global(&app, LOCAL_HOST, "test", &ws_event("ws-1")).await;
        assert_eq!(run_state(&app, &run).await, "running", "前提：這一刻沒收到");
        tt::make_table_readable(&app, "bots").await;

        let _ = crate::testing::eventually!(run_state(&app, &run).await == "exited" && project_ws(&app, &e.project_id).await.is_none());
        assert_eq!(run_state(&app, &run).await, "exited");
        assert_eq!(project_ws(&app, &e.project_id).await, None, "綁定清掉");
        assert_eq!(run_state(&app, &other_run).await, "running", "別的 workspace 不受影響");
    }

    /// 狀態訂閱重接（含 herdr 0.9.2+ 的 `events_lost`）之後補讀 pane 當下的狀態：斷掉那段漏掉的 `working → idle`
    /// 要補回來，燈號不能凍在 working。pane 已經不在就什麼都不做。
    #[tokio::test]
    async fn a_resubscribed_pane_watcher_catches_up_on_the_status_it_missed() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "resync").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let stored = || async { sqlx::query_scalar::<_, String>("SELECT agent_status FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap() };

        resync_pane_status(&app, &app.herdr, LOCAL_HOST, "test", "pane-gone").await;
        assert_eq!(stored().await, "working", "別的、已經不在的 pane 不動這個 run");

        e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
        e.herdr.set_agent("agent", &pane, false);
        resync_pane_status(&app, &app.herdr, LOCAL_HOST, "test", &pane).await;
        assert_eq!(stored().await, "idle", "漏掉的 working → idle 要補回來");
    }

    /// 第一次訂閱就失敗（herdr 剛重啟、握手被拒），之後才訂閱成功：失敗到成功之間的狀態邊一樣漏了，
    /// 不能因為「這是第一次成功」就不補讀 pane 當下的狀態（燈號會凍在收編當下的 working，直到下一則事件）。
    #[tokio::test]
    async fn a_watcher_whose_first_subscribe_failed_catches_up_once_it_connects() {
        let e = tt::env().await;
        let app = e.app.clone();
        e.herdr.allow_subscribe.store(true, Ordering::SeqCst);
        let bot = tt::claude_bot(&app, &e.project_id, "late-sub").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
        e.herdr.set_agent("agent", &pane, false);
        e.herdr.fail_next("events.subscribe", tt::Fault::Refuse);

        watch_pane_on_session(&app, LOCAL_HOST, "test", &pane).await;
        let stored = || async { sqlx::query_scalar::<_, String>("SELECT agent_status FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap() };
        let _ = crate::testing::eventually!(e.herdr.calls_to("events.subscribe").len() >= 2 && stored().await == "idle");
        assert!(e.herdr.calls_to("events.subscribe").len() >= 2, "第一次被拒、重試成功");
        assert_eq!(stored().await, "idle", "訂閱建好後要補讀當下狀態，漏掉的 working → idle 才補得回來");
    }

    /// #245：清綁定的 UPDATE 寫失敗一次，不能永遠殘留。
    #[tokio::test]
    async fn a_failed_workspace_binding_clear_is_retried() {
        let e = tt::env().await;
        let app = e.app.clone();
        sqlx::query("UPDATE projects SET workspace_id='ws-9' WHERE id=?").bind(&e.project_id).execute(&app.db).await.unwrap();
        tt::make_table_unreadable(&app, "projects").await;
        handle_global(&app, LOCAL_HOST, "test", &ws_event("ws-9")).await;
        tt::make_table_readable(&app, "projects").await;
        let _ = crate::testing::eventually!(project_ws(&app, &e.project_id).await.is_none());
        assert_eq!(project_ws(&app, &e.project_id).await, None);
    }

    /// #192 重放的補洞：舊的 `blocked` 重放已經過了「我還是最新嗎」那一關、正在讀 run 的時候，較新的 `idle` 整則跑完，
    /// 重放接著把 `blocked` 寫回去，蓋掉已經回答的 `idle`（整樹高負載下 `a_replayed_status_event_never_overwrites_a_newer_one` 偶發紅）。
    /// 這裡把「較新的事件先進來、舊重放後拿到 pane 的鎖」的順序做成確定的：舊重放拿到鎖之後要重新比對序號，不能寫。
    #[tokio::test]
    async fn a_stale_replay_that_gets_the_pane_after_a_newer_event_writes_nothing() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "stale-replay").await;
        tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let ev = |status: &str| crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"pane_id": pane, "agent_status": status}),
        };
        let stored = || async { sqlx::query_scalar::<_, String>("SELECT agent_status FROM runs WHERE bot_id=?").bind(&bot.id).fetch_one(&app.db).await.unwrap() };
        let key: PaneKey = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        status_seq().lock().unwrap().insert(key.clone(), 0);

        let held = pane_status_lock(&key).lock_owned().await;
        let newer = { let (app, ev) = (app.clone(), ev("idle")); tokio::spawn(async move { handle_status(&app, LOCAL_HOST, "test", &ev).await }) };
        let _ = crate::testing::eventually!(status_seq().lock().unwrap().get(&key).is_some_and(|n| *n != 0));
        let replay = { let (app, ev) = (app.clone(), ev("blocked")); tokio::spawn(async move { handle_status_try(&app, LOCAL_HOST, "test", &ev, 1, Some(0)).await }) };
        tokio::task::yield_now().await;
        drop(held);
        newer.await.unwrap();
        replay.await.unwrap();
        assert_eq!(stored().await, "idle", "較新的 idle 不能被舊的 blocked 重放蓋掉");
    }

    /// Codex's unknown status only means idle once startup has completed. `PaneInfo` and raw events
    /// have no launch_pending bit, so the run state is the evidence for whether to fold it.
    #[tokio::test]
    async fn a_codex_unknown_event_is_preserved_while_starting_and_folded_when_running() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "codex-status").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let unknown = crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"agent": "codex", "pane_id": pane, "agent_status": "unknown"}),
        };

        sqlx::query("UPDATE runs SET state='starting', agent_status='unknown' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        handle_status(&app, LOCAL_HOST, "test", &unknown).await;
        let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "unknown", "starting Codex must not trigger idle handling");

        sqlx::query("UPDATE runs SET state='running', agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        handle_status(&app, LOCAL_HOST, "test", &unknown).await;
        let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "idle", "running Codex unknown keeps the #732 idle behavior");
    }

    /// blocked -> idle 時若有 in-flight turn，也排定 terminal fallback。
    #[tokio::test]
    async fn a_blocked_to_idle_transition_arms_fallback_when_turn_is_in_flight() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "blocked-idle").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let conv = crate::db::conversation_id(&app.db, &bot.id).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES ('turn-bi', ?, ?, 'web', 'in_flight', 'ok', ?)")
            .bind(&conv)
            .bind(&run)
            .bind(&now)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();

        let idle_ev = crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"agent": "codex", "pane_id": pane, "agent_status": "idle"}),
        };
        handle_status(&app, LOCAL_HOST, "test", &idle_ev).await;
        let timers = app.fallback_timers.lock().await;
        assert!(timers.contains_key(&run), "blocked->idle 在有 in-flight turn 時要 arm fallback");
    }

    /// herdr 之後若多出一個我們不認得的 agent_status 值：`runs.agent_status` 有 CHECK 只認四種，原樣寫進去整句 UPDATE 失敗
    /// （只留一行 warn），燈號就停在舊值而且之後每一則同值事件都一樣失敗。不認得的一律當 `unknown`。
    #[tokio::test]
    async fn a_status_value_herdr_adds_later_is_stored_as_unknown_not_dropped() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "new-status").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let ev = crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"agent": "claude", "pane_id": pane, "agent_status": "waiting"}),
        };
        handle_status(&app, LOCAL_HOST, "test", &ev).await;
        let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "unknown", "不認得的狀態不能讓寫入整句失敗、燈號停在舊的 working");
    }

    /// B8（#1146）：寫不進 `runs.agent_status` 的狀態事件要重放到寫進去為止，不能只記一行 warn 就丟掉。
    #[tokio::test]
    async fn a_status_write_that_fails_is_replayed_until_it_lands() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "status-write-fail").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER fail_status BEFORE UPDATE OF agent_status ON runs BEGIN SELECT RAISE(ABORT, 'status boom'); END").execute(&app.db).await.unwrap();
        let ev = crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"agent": "claude", "pane_id": pane, "agent_status": "idle"}),
        };
        handle_status(&app, LOCAL_HOST, "test", &ev).await;
        let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "working", "寫入失敗：DB 還是舊值（這一輪）");
        sqlx::query("DROP TRIGGER fail_status").execute(&app.db).await.unwrap();
        let landed = crate::testing::eventually!({
            let s: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
            s == "idle"
        });
        assert!(landed, "寫入失敗的狀態事件要重放到寫進去為止");
    }

    /// mac_builder 每 39 秒被完整對帳一輪（十幾顆 bot × 好幾個 ssh RPC）：原因是 grok 額度探測（每 30 秒）開的 workspace 一偵測到
    /// agent 就觸發 `pane.agent_detected` → 兩秒後對整台主機對帳。探測登記過的 workspace 不排；別的照舊排。
    #[tokio::test]
    async fn an_agent_detected_in_a_quota_probe_workspace_does_not_schedule_a_reconcile() {
        let e = tt::env().await;
        let app = e.app.clone();
        let probe = crate::probe_ws::ProbeWorkspace::register(app.herdr.socket_path(), "w-probe");
        let detected = |ws: &str| json!({"agent": "grok", "pane_id": format!("{ws}:p1"), "workspace_id": ws});
        assert!(!detection_wants_reconcile(&app, LOCAL_HOST, "test", &detected("w-probe")).await, "探測 workspace 不對帳");
        assert!(detection_wants_reconcile(&app, LOCAL_HOST, "test", &detected("w-child")).await, "別的 workspace（可能是子 agent）照舊對帳");
        assert!(detection_wants_reconcile(&app, LOCAL_HOST, "test", &json!({"agent": "grok"})).await, "沒有 workspace_id 的事件看不出來，照舊");
        drop(probe);
        assert!(!detection_wants_reconcile(&app, LOCAL_HOST, "test", &detected("w-probe")).await, "剛結束：退出的偵測事件還會晚幾秒到，也不對帳");
    }

    /// issue #1472：herdr 沒有「agent 改名」事件，唯一的觸發點是 `pane.agent_detected`。無名 agent 被偵測到時（事件資料裡本來就沒有名字）
    /// 不能被 `detection_wants_reconcile` 擋掉：兩秒後對帳就要走到「補名字＋認領」，不用等下次偵測或重啟。
    #[tokio::test]
    async fn an_unnamed_agent_detected_is_renamed_and_adopted_by_the_reconcile_it_triggers() {
        let e = tt::env().await;
        let app = e.app.clone();
        let client = crate::herdr::HerdrClient::new(e.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", serde_json::json!({})).await.unwrap();
        let parent = tt::claude_bot(&app, &e.project_id, "alfa").await;
        let parent_agent = crate::config::agent_name("proj", &parent.id);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(crate::db::ulid())
        .bind(&parent.id)
        .bind(&ws.workspace_id)
        .bind(&root.pane_id)
        .bind(&root.tab_id)
        .bind(&parent_agent)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let split = client.pane_split(&root.pane_id, "down", "/tmp/p", serde_json::json!({})).await.unwrap();
        crate::spawn_hints::record(&app, &parent.id, &split.pane_id).await.unwrap();
        e.herdr.set_argv(&split.pane_id, &["claude", "--name", &format!("{parent_agent}-om-fix")]);
        let agent = |name: Option<&str>, pane: &crate::herdr::PaneInfo| {
            serde_json::json!({"name": name, "agent": "claude", "agent_status": "idle", "workspace_id": pane.workspace_id,
                               "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
        };
        *e.herdr.agents.lock().unwrap() = vec![agent(Some(&parent_agent), &root), agent(None, &split)];

        let data = serde_json::json!({"agent": "claude", "pane_id": split.pane_id, "workspace_id": split.workspace_id});
        assert!(detection_wants_reconcile(&app, LOCAL_HOST, "test", &data).await, "無名 agent 的偵測事件照樣對帳");
        handle_global(&app, LOCAL_HOST, "test", &crate::herdr::Event { event: "pane_agent_detected".into(), data }).await;

        let adopted = crate::testing::eventually!({
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE agent_name = ? AND state = 'running'")
                .bind(format!("{parent_agent}-om-fix"))
                .fetch_one(&app.db)
                .await
                .unwrap();
            n == 1
        });
        assert!(adopted, "偵測事件之後的對帳要補上名字並認領");
        let parent_of: Option<String> = sqlx::query_scalar("SELECT parent_bot_id FROM bots WHERE name = 'om-fix'").fetch_one(&app.db).await.unwrap();
        assert_eq!(parent_of.as_deref(), Some(parent.id.as_str()));
    }

    fn close_event(pane: &str) -> crate::herdr::Event {
        crate::herdr::Event { event: "pane.closed".into(), data: json!({"pane_id": pane}) }
    }

    /// #247：pane.closed 到的那一刻讀不到 run，以前被當成「沒有 run」、照樣拆 watcher，事件就永久漏掉。
    /// 現在 watcher 留著、稍後重試；DB 好了之後不需要第二個事件，run 收成 exited、watcher 才拆。
    #[tokio::test]
    async fn a_pane_close_whose_run_lookup_fails_is_retried_and_keeps_the_watcher() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "closer").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        plant_watcher(&app, &pane).await;

        tt::make_table_unreadable(&app, "runs").await;
        handle_global(&app, LOCAL_HOST, "test", &close_event(&pane)).await;
        assert!(watching(&app, &pane).await, "讀不到 run 時不能拆 watcher（也就是這一刻沒收到）");
        tt::make_table_readable(&app, "runs").await;

        let _ = crate::testing::eventually!(run_state(&app, &run).await == "exited" && !watching(&app, &pane).await);
        assert_eq!(run_state(&app, &run).await, "exited", "重試補上，不靠第二個事件");
        assert!(!watching(&app, &pane).await, "收斂之後才拆 watcher");
        handle_global(&app, LOCAL_HOST, "test", &close_event(&pane)).await;
        assert_eq!(run_state(&app, &run).await, "exited");
    }

    /// issue #521：`status_seq` 與 `pane_status_lock` 是行程級的表，pane 收掉時要把項目帶走，
    /// 否則每個開過又關掉的 pane 都留一格，長跑的 daemon 只增不減。
    #[tokio::test]
    async fn a_closed_pane_leaves_nothing_behind_in_the_process_wide_tables() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "leaver").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let key: PaneKey = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        plant_watcher(&app, &pane).await;

        let ev = crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"pane_id": pane, "agent_status": "working"}),
        };
        handle_status(&app, LOCAL_HOST, "test", &ev).await;
        assert!(status_seq().lock().unwrap().contains_key(&key), "狀態事件要先在表裡留下項目，這條測試才有意義");
        assert!(pane_status_locks().lock().unwrap().contains_key(&key));

        handle_global(&app, LOCAL_HOST, "test", &close_event(&pane)).await;
        let _ = crate::testing::eventually!(run_state(&app, &run).await == "exited" && !watching(&app, &pane).await);
        assert!(!status_seq().lock().unwrap().contains_key(&key), "pane 收掉之後 status_seq 不該還留著它");
        assert!(!pane_status_locks().lock().unwrap().contains_key(&key), "pane 收掉之後 pane_status_lock 不該還留著它");
    }

    fn status_event(pane: &str, status: &str) -> crate::herdr::Event {
        crate::herdr::Event { event: "pane.agent_status_changed".into(), data: json!({"pane_id": pane, "agent": "claude", "agent_status": status}) }
    }

    /// 這顆 pane 是否還在待修復清單裡（登錄表在這顆 App 上，只看這顆 App 的）。
    fn pending_repair(app: &Arc<App>, pane: &str) -> bool {
        app.status_repairs.lock().unwrap().contains_key(&(LOCAL_HOST.to_string(), "test".to_string(), pane.to_string()))
    }

    async fn status_in_db(app: &Arc<App>, run: &str) -> String {
        sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(run).fetch_one(&app.db).await.unwrap()
    }

    /// DB 寫不進去（trigger 擋住 `agent_status` 的 UPDATE）。
    async fn break_status_writes(app: &Arc<App>) {
        sqlx::query("CREATE TRIGGER status_write_fails BEFORE UPDATE OF agent_status ON runs BEGIN SELECT RAISE(ABORT, 'database is locked'); END")
            .execute(&app.db)
            .await
            .unwrap();
    }

    /// #1184：DB 寫不進去的時間超過重放的預算（測試裡 20 次、每次 100ms），之後 herdr 沒有任何新事件：
    /// 修復迴圈重讀 pane 當下的狀態寫回去。頂層 bot 與子 agent 形狀的 bot 都要收得回來。
    #[tokio::test]
    async fn a_status_write_that_outlasts_the_replay_is_repaired_without_a_new_event() {
        let e = tt::env().await;
        let app = e.app.clone();
        let mut cases = Vec::new();
        for name in ["repair-top", "repair-child"] {
            let bot = tt::claude_bot(&app, &e.project_id, name).await;
            let run = tt::fake_run(&app, &bot.id).await;
            let pane = format!("pane-{}", bot.id);
            e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
            e.herdr.set_agent(name, &pane, false);
            sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
            cases.push((run, pane));
        }
        break_status_writes(&app).await;
        for (_, pane) in &cases {
            handle_status(&app, LOCAL_HOST, "test", &status_event(pane, "idle")).await;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        for (run, _) in &cases {
            assert_eq!(status_in_db(&app, run).await, "working", "寫不進去時 DB 停在舊狀態");
        }
        repair_pending_statuses(&app).await;
        for (_, pane) in &cases {
            assert!(pending_repair(&app, pane), "寫不進去的 pane 都還欠著 DB 一個狀態");
        }

        sqlx::query("DROP TRIGGER status_write_fails").execute(&app.db).await.unwrap();
        repair_pending_statuses(&app).await;
        for (_, pane) in &cases {
            assert!(!pending_repair(&app, pane), "DB 恢復後修復迴圈一輪就補完");
        }
        for (run, _) in &cases {
            assert_eq!(status_in_db(&app, run).await, "idle", "沒有任何新事件，靠的是重讀 herdr");
        }
    }

    /// #1184：修復寫回的是 herdr **當下**的狀態（working），不是登記時那一則舊事件（idle）。
    #[tokio::test]
    async fn a_repair_writes_what_herdr_says_now_not_the_stale_event() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "repair-now").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
        e.herdr.set_unnamed_agent(&pane, None);
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        break_status_writes(&app).await;
        handle_status(&app, LOCAL_HOST, "test", &status_event(&pane, "idle")).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(pending_repair(&app, &pane), "寫不進去的 pane 還欠著 DB 一個狀態");

        repair_pending_statuses(&app).await;
        assert!(!pending_repair(&app, &pane), "修復讀到的是 herdr 當下的值，跟 DB 一致就不用寫");
        sqlx::query("DROP TRIGGER status_write_fails").execute(&app.db).await.unwrap();
        repair_pending_statuses(&app).await;
        assert_eq!(status_in_db(&app, &run).await, "working", "舊的 idle 不能被寫回去");
    }

    /// #1184：登記之後那個 pane 的 run 結束了：修復丟掉這筆，不寫到已經結束的 run。
    #[tokio::test]
    async fn a_pending_repair_is_dropped_once_its_run_is_gone() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "repair-gone").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
        e.herdr.set_agent("repair-gone", &pane, false);
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        break_status_writes(&app).await;
        handle_status(&app, LOCAL_HOST, "test", &status_event(&pane, "idle")).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        repair_pending_statuses(&app).await;
        assert!(pending_repair(&app, &pane), "寫不進去的 pane 還欠著 DB 一個狀態");

        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        sqlx::query("DROP TRIGGER status_write_fails").execute(&app.db).await.unwrap();
        repair_pending_statuses(&app).await;
        assert!(!pending_repair(&app, &pane), "run 結束了，這筆不再算數");
        assert_eq!(status_in_db(&app, &run).await, "working", "已經結束的 run 不被寫");
    }

    /// 登記的 run（沒有登記就是 None）。
    fn repaired_run(app: &Arc<App>, key: &PaneKey) -> Option<String> {
        app.status_repairs.lock().unwrap().get(key).and_then(|r| r.run_id.clone())
    }

    /// #1290：同一顆 pane 換了新 run、新 run 的寫入也失敗：登記要跟著換到新 run，修復才找得到它。
    /// 修之前登記還是舊 run（R1），DB 恢復後修復看到的是 R2 ≠ R1，把登記丟掉，R2 永遠停在舊狀態。
    #[tokio::test]
    async fn a_status_repair_follows_the_pane_to_its_replacement_run() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "repl-top").await;
        let pane = format!("pane-{}", bot.id);
        let key: PaneKey = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
        let r1 = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&r1).execute(&app.db).await.unwrap();
        break_status_writes(&app).await;
        handle_status_try(&app, LOCAL_HOST, "test", &status_event(&pane, "idle"), status_replay_delays().len(), None).await;
        assert_eq!(repaired_run(&app, &key), Some(r1.clone()));

        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&r1).execute(&app.db).await.unwrap();
        let r2 = tt::fake_run(&app, &bot.id).await;
        handle_status_try(&app, LOCAL_HOST, "test", &status_event(&pane, "working"), status_replay_delays().len(), None).await;
        assert_eq!(repaired_run(&app, &key), Some(r2.clone()), "登記要跟著 pane 換到 R2");

        sqlx::query("DROP TRIGGER status_write_fails").execute(&app.db).await.unwrap();
        e.herdr.set_unnamed_agent(&pane, None);
        repair_pending_statuses(&app).await;
        assert!(!pending_repair(&app, &pane), "DB 恢復後修復一輪補完");
        assert_eq!(status_in_db(&app, &r2).await, "working", "R2 的狀態要被修回來，不是被丟掉");
    }

    /// #1290：修復讀完 DB、清登記之前，同一顆 pane 又登記了新的 run：修復只清它自己拿到的那一筆，新登記留著。
    #[tokio::test]
    async fn a_repair_sweep_never_erases_a_newer_registration_for_its_pane() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "repl-race").await;
        let pane = format!("pane-{}", bot.id);
        let key: PaneKey = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
        e.herdr.set_agent("repl-race", &pane, false);
        let r1 = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&r1).execute(&app.db).await.unwrap();
        break_status_writes(&app).await;
        handle_status_try(&app, LOCAL_HOST, "test", &status_event(&pane, "idle"), status_replay_delays().len(), None).await;
        assert_eq!(repaired_run(&app, &key), Some(r1.clone()));

        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&r1).execute(&app.db).await.unwrap();
        let r2 = tt::fake_run(&app, &bot.id).await;
        let (hook_app, hook_key, r2c) = (app.clone(), key.clone(), r2.clone());
        crate::race_point::arm("status_repair_after_lookup", &pane, move || async move {
            mark_status_repair(&hook_app, &hook_key, Some(&r2c));
        });
        repair_pending_statuses(&app).await;
        assert_eq!(repaired_run(&app, &key), Some(r2), "修復清的只能是它自己拿到的 R1，R2 的新登記要留著");
    }

    /// #1293：查不出 pane 的 run（`runs` 讀不到）、重放又用完，之後 herdr 沒有任何新事件：
    /// 登記在 run 還不知道的時候就建立，DB 恢復後修復一輪補上 pane 當下的狀態。頂層 bot 與子 agent 形狀的 bot 都要收得回來。
    #[tokio::test]
    async fn a_status_event_whose_run_lookup_outlasts_the_replay_is_repaired_without_a_new_event() {
        let e = tt::env().await;
        let app = e.app.clone();
        let mut cases = Vec::new();
        for name in ["lookup-top", "lookup-child"] {
            let bot = tt::claude_bot(&app, &e.project_id, name).await;
            let run = tt::fake_run(&app, &bot.id).await;
            let pane = format!("pane-{}", bot.id);
            e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
            e.herdr.set_agent(name, &pane, false);
            sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
            cases.push((run, pane));
        }
        tt::make_table_unreadable(&app, "runs").await;
        for (_, pane) in &cases {
            handle_status_try(&app, LOCAL_HOST, "test", &status_event(pane, "idle"), status_replay_delays().len(), None).await;
            assert!(pending_repair(&app, pane), "查不出 run 也要登記，這顆 pane 欠著 DB 一個狀態");
        }
        tt::make_table_readable(&app, "runs").await;

        repair_pending_statuses(&app).await;
        for ((run, pane), _) in cases.iter().zip(0..) {
            assert!(!pending_repair(&app, pane), "DB 恢復後修復一輪補完");
            assert_eq!(status_in_db(&app, run).await, "idle", "沒有任何新事件，靠的是重讀 herdr");
        }
    }

    /// #1409：已移交專案的 pane，狀態事件進來時把它欠著的修復登記丟掉（移交期間這顆 daemon 不寫它的狀態）。
    #[tokio::test]
    async fn a_repair_registration_is_dropped_once_the_project_is_handed_off() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "handoff-event").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
        e.herdr.set_agent("handoff-event", &pane, false);
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let key: PaneKey = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        mark_status_repair(&app, &key, None);
        sqlx::query("UPDATE projects SET handed_off_to = 'mac' WHERE id = ?").bind(&e.project_id).execute(&app.db).await.unwrap();

        handle_status(&app, LOCAL_HOST, "test", &status_event(&pane, "idle")).await;
        assert!(!pending_repair(&app, &pane), "移交出去的專案不欠這顆 daemon 狀態：登記要丟掉");
        assert_eq!(status_in_db(&app, &run).await, "working", "移交後這顆 daemon 不寫它的 DB");
    }

    /// #1409：修復迴圈不對已移交專案的 pane 打 herdr RPC（SPEC §6.5h：一個 pane RPC 都不打），登記也跟著丟掉。
    #[tokio::test]
    async fn the_repair_loop_never_reads_a_handed_off_pane() {
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "handoff-loop").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        e.herdr.live_pane(&pane, tt::LivePane { rows: Some(40), ..Default::default() });
        e.herdr.set_agent("handoff-loop", &pane, false);
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let key: PaneKey = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        mark_status_repair(&app, &key, None);
        sqlx::query("UPDATE projects SET handed_off_to = 'mac' WHERE id = ?").bind(&e.project_id).execute(&app.db).await.unwrap();

        let reads_before = e.herdr.calls_to("pane.get").len();
        assert_eq!(repair_pending_statuses(&app).await, 0, "移交的 pane 不留在待修復清單裡");
        assert_eq!(e.herdr.calls_to("pane.get").len(), reads_before, "修復迴圈不對移交的 pane 打 RPC");
        assert_eq!(status_in_db(&app, &run).await, "working");
    }

    /// #1409：pane 上已經沒有 run：沒有東西可修，登記順手丟掉（修復迴圈下一輪也會當過期清，這裡不必再多活一輪）。
    #[tokio::test]
    async fn a_status_event_for_a_pane_with_no_run_drops_its_registration() {
        let e = tt::env().await;
        let app = e.app.clone();
        let pane = "pane-no-run".to_string();
        let key: PaneKey = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        mark_status_repair(&app, &key, None);

        handle_status(&app, LOCAL_HOST, "test", &status_event(&pane, "idle")).await;
        assert!(!pending_repair(&app, &pane), "沒有 run 的 pane 沒有東西可修，登記要丟掉");
    }
}
