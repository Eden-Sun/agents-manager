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
    let t = tokio::spawn(async move { global_loop(a, h, s).await });
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
                // herdr live-handoff 後訂閱重建：server 版本／protocol 可能換了，重問一次（#254）。
                if is_local_main || host != LOCAL_HOST {
                    let (app2, host2) = (app.clone(), host.clone());
                    tokio::spawn(async move { app2.refresh_herdr_version(&host2).await });
                }
                // Reconcile after every (re)connect.
                if is_local_default {
                    if let Err(e) = app.sync_default_session().await {
                        tracing::debug!(host = %host, session = %session, error = ?e, "default session sync failed");
                    }
                } else {
                    // 遠端 supervisor 連上那一輪對帳失敗、這裡才成功（連線沒斷）：欠著的一生一次 autostart 在這補（#259）。本機由開機那一輪負責。
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
                }
                if is_local_default {
                    app.set_default_connected(false).await;
                }
                tracing::warn!(host = %host, session = %session, "global herdr event subscription dropped");
            }
            Err(e) => {
                if is_local_main {
                    app.connected.store(false, Ordering::SeqCst);
                    // 馬上讓 UI 知道，不然燈號會一直綠到下次連上為止。
                    app.emit_daemon_status().await;
                }
                if is_local_default {
                    app.set_default_connected(false).await;
                }
                tracing::debug!(host = %host, session = %session, error = %e, "herdr subscribe failed");
            }
        }
        // 遠端的重連歸 HostConn supervisor 管：這裡停手，等它在 master 回來後重開我們。
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
                // 多半是 bot 剛開的子 pane（見 `reconcile`），不排一次對帳就要等到下次重啟才看得到。
                // 另開 task 並晚一拍，讓還在啟動的 agent 先報出名字與 kind。
                let app = app.clone();
                let host = host.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    if let Err(e) = app.reconcile_host(&host).await {
                        tracing::debug!(host = %host, error = ?e, "reconcile after agent detection failed");
                    }
                });
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
        // 已收成功的 child 仍要排對帳（重試時它不再是 active，不會再算到）。
    }
    if converged {
        unwatch_pane_on_session(app, host, session, pane_id).await;
    }
    // #60：子 agent 退役由對帳判定（pane 被搬走也會報舊 id 關閉），但關 pane 不保證會發
    // `pane.agent_detected`，所以這裡自己排一次，同樣晚一拍等 herdr 穩定。
    if ended_a_child {
        let app = app.clone();
        let host = host.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Err(e) = app.reconcile_host(&host).await {
                tracing::debug!(host = %host, error = ?e, "reconcile after a child's pane closed failed");
            }
        });
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
        // #708：移交出去的專案的映射不動。
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
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        close_workspace_try(&app, &host, &session, &ws, attempt + 1).await;
    });
}

fn retry_pane_close(app: &Arc<App>, host: &str, session: &str, pane_id: &str, attempt: usize) {
    let Some(wait) = close_retry_delays().get(attempt).copied() else {
        tracing::warn!(host, pane_id, "gave up retrying a pane-close; periodic reconcile is the safety net");
        return;
    };
    let (app, host, session, pane_id) = (app.clone(), host.to_string(), session.to_string(), pane_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        end_runs_for_pane_try(&app, &host, &session, &pane_id, attempt + 1).await;
    });
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
    let handle = tokio::spawn(async move {
        let mut backoff = HERDR_SUBSCRIBE_INITIAL_BACKOFF;
        let mut resubscribing = false;
        // 只有「證明 run 已經結束」那一種離開才算收掉這個 pane；拿不到 client（遠端斷線、session 對不上）離開時 run 還活著，
        // 等著重放的狀態事件與 pane 的鎖都還要用（`forget_watcher`）。
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
                    // 每次 daemon／herdr 重啟或遠端斷線，每個 pane 各掉一次（一天 237 行 WARN）；底下會自己重訂並補讀狀態，真有問題
                    // 由「host connect failed」「herdr event stream closed」那些行說，這行只是逐 pane 的旁證。
                    tracing::info!(host = %hst, session = %sess, pane_id = %pid, "pane status subscription dropped");
                }
                Err(e) => {
                    // 這段時間的狀態邊也漏了：之後訂閱成功要補讀，不只是「斷線後重接」才補。
                    resubscribing = true;
                    tracing::debug!(host = %hst, session = %sess, pane_id = %pid, error = %e, "pane status subscribe failed");
                }
            }
            // Stop retrying only once the run is *provably* no longer active (#244): a read error or an
            // unknown session is not proof, so keep the watcher and retry after the backoff.
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
    });
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

fn replay_status_later(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event, key: PaneKey, seq: u64, attempt: usize) {
    let Some(wait) = status_replay_delays().get(attempt).copied() else {
        tracing::warn!(host, pane = %key.2, "gave up replaying a pane status event; the periodic child-alert sweep is the safety net");
        return;
    };
    let (app, host, session, ev) = (app.clone(), host.to_string(), session.to_string(), ev.clone());
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        // 這個 pane 之後又來過事件：那一則比這一則新，這一則不再算數。
        if status_seq().lock().unwrap().get(&key) != Some(&seq) {
            return;
        }
        handle_status_try(&app, &host, &session, &ev, attempt + 1, Some(seq)).await;
    });
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
        // `runs.agent_status` 的 CHECK 只認這四種：herdr 之後新增的值原樣寫進去整句 UPDATE 會失敗。
        .map(|s| match s {
            "done" => "idle",
            "idle" | "working" | "blocked" | "unknown" => s,
            _ => "unknown",
        })
        .unwrap_or("unknown")
        .to_string();
    tracing::info!(host, session, pane_id, status = %reported_status, "pane.agent_status_changed");
    let key: PaneKey = (host.to_string(), session.to_string(), pane_id.to_string());
    // 重放的那一則不是新事件：沿用原來的序號，不能把自己登記成「最新」。
    let seq = match replay_seq {
        Some(seq) => seq,
        None => {
            // 全域遞增，不是每個 pane 從 1 開始數：`forget_pane_status_state` 清掉項目之後，同一個 pane 的
            // 新事件才不會拿到某個還在等的重放手上那個號碼（拿到就會把舊狀態當成「還是最新的」寫回去）。
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
    // 讀不到不等於這個 pane 沒有 run（#192）：以前讀取錯誤被當成空集合、整則事件丟掉。對 working／idle 之後的對帳還補得回來，
    // 但 `blocked` 那條邊的副作用（告訴父 agent 它的 child 在等）只有這一次——child 停在同一個問題上不會再有第二個狀態事件。
    // 稍後重放這一則（這個 pane 之後沒有更新的事件才算數）；重放也讀不到，還有 `child_alerts::sweep` 的定時安全網。
    let runs = match crate::db::active_runs_for_pane(&app.db, host, pane_id, session, &fallback).await {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(host, pane_id, status = %reported_status, error = ?e, "could not look up the run for a pane status event; replaying it shortly");
            replay_status_later(app, host, session, ev, key, seq, attempt);
            return;
        }
    };
    let Some(run) = runs.into_iter().next() else {
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
    // #708：移交出去的專案，狀態、外部回合、備援、通知 parent 都歸接手的 daemon。讀不到就照「讀不到 run」重放。
    match app.db.bot_handed_off_to(&run.bot_id).await {
        Ok(None) => {}
        Ok(Some(_)) => return,
        Err(e) => {
            tracing::warn!(host, pane_id, status = %status, error = ?e, "could not tell whether a pane's project was handed off; replaying it shortly");
            replay_status_later(app, host, session, ev, key, seq, attempt);
            return;
        }
    }
    // Herdr owns the run status from its first report after a synthetic Dangerous rm marker.
    crate::dangerous_rm::on_herdr_status(&run.id);
    let prev = run.agent_status.clone();
    // 卡住的 turn 要「持續」idle 才收：每個狀態事件都記，閃一下 working 就重算。
    app.observe_agent_status(&run.id, &status);
    // 閒置回收收機前會對這一份（issue #144）：DB 寫不進去時，DB 的 idle 不能被當成閒著的證據。
    app.observe_idle_status(&run.id, &status);
    if prev != status {
        if let Err(e) = sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?")
            .bind(&status)
            .bind(&run.id)
            .execute(&app.db)
            .await
        {
            tracing::warn!(run = %run.id, status = %status, error = ?e, "could not persist the agent status");
        }
    }
    drop(write_guard);
    app.emit_bot_status(&run.bot_id).await;

    // The agent reacted to the prompt: the stall watchdog is no longer needed.
    if status == "working" || status == "blocked" {
        app.cancel_stall(&run.id).await;
    }
    // 續行提示的 10 秒從「驗證過且 idle」起算；變成 working／blocked 就取消（#424）。
    if matches!(status.as_str(), "idle" | "working" | "blocked") {
        app.poke_resume_nudge(&run.bot_id);
    }

    // 停下來等人回答時先看是不是 claude 的滿意度問卷——是就自己按 0（§3.1）。其他 blocked 不動。
    if prev != "blocked" && status == "blocked" {
        let (app2, run2) = (app.clone(), run.clone());
        tokio::spawn(async move {
            app2.dismiss_survey_if_shown(&run2).await;
        });
        // grok 撞週限的畫面（#222）：herdr 把它判成 `blocked`、沒有 working->idle 那條邊，畫面掃描不會自己跑。
        // 不是 grok 的 bot、或畫面上沒有那兩句，掃描什麼都不做；一個鍵都不按（選項 1、2 是付費）。
        if crate::db::bot(&app.db, &run.bot_id).await.ok().flatten().is_some_and(|b| b.kind == "grok") {
            app.schedule_codex_notice_capture(&run.bot_id, &run.id);
        }
        // 子 agent 停在提問時，它的父 agent 不會自己知道（`child_alerts`）：UI 的徽章是給人看的，
        // 父 agent 是一顆 CLI 行程，沒有人打字進去就什麼都收不到。
        crate::runners::child_alerts::on_child_blocked(app, &run);
        // claude 2.1.281 的防誤刪框：通知使用者（帶目標），一個鍵都不按。
        crate::runners::dangerous_rm::on_blocked(app, &run);
        // Codex 的模型遷移提示也等使用者本人選擇，不把後續訊息送進選單。
        app.codex_migration_on_blocked(&run);
        // claude 一般權限確認選單：等畫面畫完讀一次，結構化原因（`blocked_reason`）寫「等待權限確認：<工具>」。只看、不按鍵。
        {
            let (app2, run2) = (app.clone(), run.clone());
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                if let Ok(Some(fresh)) = crate::db::run(&app2.db, &run2.id).await {
                    if fresh.agent_status == "blocked" {
                        crate::blocked_reason::observe(&app2, &fresh).await;
                    }
                }
            });
        }
    }
    // 不再 blocked：權限框的原因跟著清（選單關了、回合繼續）。
    if prev == "blocked" && status != "blocked" {
        crate::blocked_reason::forget(&run.id);
    }
    // claude 2.1.281 的 Session paused 選單 herdr 判成 idle：看一眼畫面，是就補標 blocked（不按鍵）。
    if prev != "idle" && status == "idle" {
        app.session_paused_on_idle(&run);
    }
    // 不再 blocked：同一個問題下次再出現時才要再講一次。
    if prev == "blocked" && status != "blocked" {
        crate::child_alerts::forget(&run.bot_id);
    }

    // 沒有 in-flight turn 卻開始 working＝有人在 pane 裡打字：現在就開 external turn，UI 才串得到；
    // 等 Stop hook 的話整段回合對話都是空的。
    if prev != "working" && status == "working" && matches!(crate::db::in_flight_turn(&app.db, &run.id).await, Ok(None)) {
        app.begin_external_turn(&run).await;
    }

    // §11.4.3：遠端 run 的內容留在那台的 spool，先 drain 再 arm 備援，終端快照才只在 hook 真的沒來時贏。
    // 有預算上限（下一個事件排在它後面），逾時就讓備援接手，CAS 保證不雙寫。
    if host != LOCAL_HOST && ((prev == "working" && status == "idle") || (prev != "blocked" && status == "blocked")) {
        let drain = app.drain_remote_coalesced(host, &run.bot_id);
        match tokio::time::timeout(DRAIN_BUDGET, drain).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(host, bot_id = %run.bot_id, error = ?e, "remote drain failed"),
            Err(_) => tracing::warn!(host, bot_id = %run.bot_id, "remote drain timed out"),
        }
    }

    // 建議下一句只在 idle 有意義：回合開始（或停在對話框）就忘掉；回到 idle 的前幾秒補讀（claude 另外算出來的，Stop 當下還沒畫）。
    if status != "idle" {
        crate::prompt_suggestion::forget(&run.id);
    } else if prev != "idle" {
        app.prompt_suggestion_on_idle(&run);
    }

    // §4.3: working -> idle arms the terminal fallback. `blocked` never does.
    if prev == "working" && status == "idle" {
        app.arm_fallback(&run.id, &run.bot_id).await;
        // A prompt queued while the agent was still working waits for exactly this edge.
        app.schedule_flush_queued(&run.bot_id);
        // 忙的時候改的 codex fast 等的就是這條邊（#393）。
        app.schedule_deferred_live(&run.bot_id);
    } else if prev != "idle" && status == "idle" {
        // 剛起來（`unknown`）或對話框剛關掉（`blocked`）就閒下來：排著的也該送了——bot 沒在跑時收下的那一則
        // （issue #122）正是等這一刻，不然要等退避的 timer（最少 15 秒）。flush 自己的閘門照舊把關。
        app.schedule_flush_queued(&run.bot_id);
        app.schedule_deferred_live(&run.bot_id);
    }
}

// ---------------------------------------------------------------- agent titles

const TITLE_POLL: std::time::Duration = std::time::Duration::from_secs(4);

/// 讓 `runs.agent_title` 跟上 agent 自己現在的標題（claude 會寫成它正在做的事）。
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
            // default session 不在 HostManager 裡，但可能有被採用的 agent，標題也要跟。
            if app.herdr_session.as_str() != "default" && app.default_connected.load(Ordering::SeqCst) {
                let fallback = app.herdr_session.clone();
                let client = app.default_herdr.clone();
                poll_titles(&app, LOCAL_HOST, "default", &fallback, &client).await;
            }
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

        // 再一次訂閱重建的對帳不能再起一次（一生一次；使用者停掉的就是靠這個不被重開）。
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

        // 同一個 pane 之後裝了新的 watcher（世代不同）：舊的 watcher 退出不能動它的狀態。
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
        let session = "unreachable-session"; // 本機沒有這個 session 的 client：`herdr_for_session` 回 None
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
        let run = tt::fake_run(&app, &bot.id).await; // workspace ws-1
        let other = tt::claude_bot(&app, &e.project_id, "other").await;
        let other_run = tt::fake_run(&app, &other.id).await;
        sqlx::query("UPDATE runs SET workspace_id='ws-2' WHERE id=?").bind(&other_run).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE projects SET workspace_id='ws-1' WHERE id=?").bind(&e.project_id).execute(&app.db).await.unwrap();

        tt::make_table_unreadable(&app, "bots").await;
        handle_global(&app, LOCAL_HOST, "test", &ws_event("ws-1")).await;
        // 前提（這一刻沒收到）要在 bots **還讀不到的時候**看（runs 本身讀得到）：一旦讀得到，背景重試隨時會收斂，
        // 高負載下測試這邊晚一步再看就會看到已經收好的結果。
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
        e.herdr.set_agent("agent", &pane, false); // herdr 現在說 idle
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
        e.herdr.set_agent("agent", &pane, false); // herdr 現在說 idle：working → idle 發生在訂閱建起來之前
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
        tt::fake_run(&app, &bot.id).await; // agent_status = idle
        let pane = format!("pane-{}", bot.id);
        let ev = |status: &str| crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"pane_id": pane, "agent_status": status}),
        };
        let stored = || async { sqlx::query_scalar::<_, String>("SELECT agent_status FROM runs WHERE bot_id=?").bind(&bot.id).fetch_one(&app.db).await.unwrap() };
        let key: PaneKey = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        // 舊的 blocked 事件登記的號碼（讀不到 run、排了重放）。編號是全域遞增的（#521），所以這裡只能
        // 斷言「較新的事件把它換掉了」。舊號碼用 0：計數器從 1 起算，永遠不會發出 0（單獨跑這個測試時，
        // 第一則事件就是 1，拿 1 當舊號碼會撞號、重放被當成最新的）。
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

    /// m4p 每 39 秒被完整對帳一輪（十幾顆 bot × 好幾個 ssh RPC）：原因是 grok 額度探測（每 30 秒）開的 workspace 一偵測到
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
        // 重複的關閉事件是安全的。
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
}
