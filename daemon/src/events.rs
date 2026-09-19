//! herdr 事件訂閱與處理（SPEC §3.1、§6.6、§11.3.6）。
//!
//! pane id 只在一個 herdr session 內唯一，所以查表一律以 `(host, session, pane_id)` 為鍵。
//! herdr 的事件名稱點號／底線兩種寫法都有，比對前先正規化。

use crate::config::LOCAL_HOST;
use crate::state::App;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// How long `handle_status` waits for a remote drain before letting the fallback arm (§11.4.3).
const DRAIN_BUDGET: Duration = Duration::from_secs(4);

fn norm(name: &str) -> String {
    name.replace('.', "_")
}

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
    let mut backoff = Duration::from_millis(250);
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
        match client.subscribe(subs).await {
            Ok(mut rx) => {
                backoff = Duration::from_millis(250);
                if is_local_main {
                    app.connected.store(true, Ordering::SeqCst);
                    crate::state::emit_daemon_status(&app).await;
                }
                if is_local_default {
                    crate::state::set_default_connected(&app, true).await;
                }
                tracing::info!(host = %host, session = %session, "global herdr event subscription established");
                // Reconcile after every (re)connect.
                if is_local_default {
                    if let Err(e) = crate::default_session::sync(&app).await {
                        tracing::debug!(host = %host, session = %session, error = ?e, "default session sync failed");
                    }
                } else if let Err(e) = crate::reconcile::reconcile_host(&app, &host).await {
                    tracing::error!(host = %host, session = %session, error = ?e, "reconcile failed");
                }
                while let Some(ev) = rx.recv().await {
                    handle_global(&app, &host, &session, &ev).await;
                }
                if is_local_main {
                    app.connected.store(false, Ordering::SeqCst);
                    crate::state::emit_daemon_status(&app).await;
                }
                if is_local_default {
                    crate::state::set_default_connected(&app, false).await;
                }
                tracing::warn!(host = %host, session = %session, "global herdr event subscription dropped");
            }
            Err(e) => {
                if is_local_main {
                    app.connected.store(false, Ordering::SeqCst);
                    // 馬上讓 UI 知道，不然燈號會一直綠到下次連上為止。
                    crate::state::emit_daemon_status(&app).await;
                }
                if is_local_default {
                    crate::state::set_default_connected(&app, false).await;
                }
                tracing::debug!(host = %host, session = %session, error = %e, "herdr subscribe failed");
            }
        }
        // 遠端的重連歸 HostConn supervisor 管：這裡停手，等它在 master 回來後重開我們。
        if host != LOCAL_HOST && !app.host_connected(&host).await {
            tracing::info!(host = %host, "host disconnected; global subscription yields to the supervisor");
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

async fn handle_global(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event) {
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
            for b in crate::db::live_bots_on_host(&app.db, host).await.unwrap_or_default() {
                if let Ok(Some(r)) = crate::db::active_run(&app.db, &b.id).await {
                    if r.workspace_id.as_deref() == Some(ws) && app.session_for_run(&r).await.as_deref() == Some(session) {
                        crate::lifecycle::mark_run_exited(app, &r.id, "workspace closed").await;
                    }
                }
            }
            if app.session_for_host(host).await.as_deref() == Some(session) {
                let _ = sqlx::query("UPDATE projects SET workspace_id = NULL WHERE workspace_id = ? AND host = ?")
                    .bind(ws)
                    .bind(host)
                    .execute(&app.db)
                    .await;
                app.emit("project_changed", json!({})).await;
            }
        }
        "pane_agent_detected" => {
            tracing::debug!(host, session, data = %ev.data, "pane.agent_detected");
            if host == LOCAL_HOST && session == "default" {
                if let Err(e) = crate::default_session::sync(app).await {
                    tracing::debug!(host, session, error = ?e, "default session sync after agent detection failed");
                }
            } else if app.session_for_host(host).await.as_deref() == Some(session) {
                // 多半是 bot 剛開的子 pane（見 `reconcile`），不排一次對帳就要等到下次重啟才看得到。
                // 另開 task 並晚一拍，讓還在啟動的 agent 先報出名字與 kind。
                let app = app.clone();
                let host = host.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    if let Err(e) = crate::reconcile::reconcile_host(&app, &host).await {
                        tracing::debug!(host = %host, error = ?e, "reconcile after agent detection failed");
                    }
                });
            }
        }
        other => tracing::trace!(host, session, event = other, "unhandled global herdr event"),
    }
}

async fn end_runs_for_pane(app: &Arc<App>, host: &str, session: &str, pane_id: &str) {
    let fallback = app.session_for_host(host).await.unwrap_or_default();
    let mut ended_a_child = false;
    for r in crate::db::active_runs_for_pane(&app.db, host, pane_id, session, &fallback).await.unwrap_or_default() {
        crate::lifecycle::mark_run_exited(app, &r.id, "pane exited").await;
        if matches!(crate::db::bot(&app.db, &r.bot_id).await, Ok(Some(b)) if b.managed_by == "child") {
            ended_a_child = true;
        }
    }
    unwatch_pane_on_session(app, host, session, pane_id).await;
    // #60：子 agent 退役由對帳判定（pane 被搬走也會報舊 id 關閉），但關 pane 不保證會發
    // `pane.agent_detected`，所以這裡自己排一次，同樣晚一拍等 herdr 穩定。
    if ended_a_child {
        let app = app.clone();
        let host = host.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Err(e) = crate::reconcile::reconcile_host(&app, &host).await {
                tracing::debug!(host = %host, error = ?e, "reconcile after a child's pane closed failed");
            }
        });
    }
}

/// Open (or replace) the per-run status subscription in the host's configured session.
pub async fn watch_pane(app: &Arc<App>, host: &str, pane_id: &str) {
    let Some(session) = app.session_for_host(host).await else { return };
    watch_pane_on_session(app, host, &session, pane_id).await;
}

/// 這個 pane 目前登記的 watcher 世代（只在持有 `pane_watchers` 時讀寫）。自己結束的 watcher 要把
/// 自己的登記帶走——留著會讓 `watch_pane_on_session` 以為「已經在看」，那個 pane 從此訂閱不起來
/// （燈號凍在收編當下，§6.6）；但不能把同一個 pane 後來裝的**新** watcher 踢掉。
fn watcher_gens() -> &'static std::sync::Mutex<std::collections::HashMap<PaneKey, u64>> {
    static G: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PaneKey, u64>>> =
        std::sync::OnceLock::new();
    G.get_or_init(Default::default)
}

type PaneKey = (String, String, String);

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
        let mut backoff = Duration::from_millis(250);
        loop {
            let Some(client) = app2.herdr_for_session(&hst, &sess).await else { break };
            let subs = vec![json!({"type": "pane.agent_status_changed", "pane_id": pid})];
            match client.subscribe(subs).await {
                Ok(mut rx) => {
                    backoff = Duration::from_millis(250);
                    tracing::info!(host = %hst, session = %sess, pane_id = %pid, "watching pane agent status");
                    while let Some(ev) = rx.recv().await {
                        handle_status(&app2, &hst, &sess, &ev).await;
                    }
                    tracing::warn!(host = %hst, session = %sess, pane_id = %pid, "pane status subscription dropped");
                }
                Err(e) => tracing::debug!(host = %hst, session = %sess, pane_id = %pid, error = %e, "pane status subscribe failed"),
            }
            // Stop retrying once the run is no longer active.
            let fallback = app2.session_for_host(&hst).await.unwrap_or_default();
            let still = crate::db::active_runs_for_pane(&app2.db, &hst, &pid, &sess, &fallback).await.unwrap_or_default().len();
            if still == 0 {
                break;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
        forget_watcher(&app2, own, generation).await;
    });
    g.insert(key, handle);
}

/// Drop a watcher's own registration, on every path out of its loop.
async fn forget_watcher(app: &Arc<App>, key: PaneKey, generation: u64) {
    let mut watchers = app.pane_watchers.lock().await;
    let mut gens = watcher_gens().lock().unwrap();
    if gens.get(&key) == Some(&generation) {
        gens.remove(&key);
        watchers.remove(&key);
    }
}

pub async fn unwatch_pane(app: &Arc<App>, host: &str, pane_id: &str) {
    let Some(session) = app.session_for_host(host).await else { return };
    unwatch_pane_on_session(app, host, &session, pane_id).await;
}

pub async fn unwatch_pane_on_session(app: &Arc<App>, host: &str, session: &str, pane_id: &str) {
    let key = (host.to_string(), session.to_string(), pane_id.to_string());
    let mut watchers = app.pane_watchers.lock().await;
    watcher_gens().lock().unwrap().remove(&key);
    if let Some(h) = watchers.remove(&key) {
        h.abort();
    }
}

pub(crate) async fn handle_status(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event) {
    handle_status_try(app, host, session, ev, 0).await
}

/// 同一個 pane 最新的狀態事件是第幾則：讀不到 run 而延後重放的那一則，只在它之後沒有更新的事件時才算數（#192）。
fn status_seq() -> &'static std::sync::Mutex<std::collections::HashMap<PaneKey, u64>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PaneKey, u64>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// 讀不到 run 的狀態事件隔多久重放一次；用完就交給 `child_alerts::sweep` 的定時安全網。
const STATUS_REPLAY: [Duration; 4] = [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)];
const STATUS_REPLAY_IN_TESTS: [Duration; 20] = [Duration::from_millis(100); 20];

fn status_replay_delays() -> &'static [Duration] {
    if cfg!(test) {
        &STATUS_REPLAY_IN_TESTS
    } else {
        &STATUS_REPLAY
    }
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
        handle_status_try(&app, &host, &session, &ev, attempt + 1).await;
    });
}

async fn handle_status_try(app: &Arc<App>, host: &str, session: &str, ev: &crate::herdr::Event, attempt: usize) {
    let name = norm(&ev.event);
    if name != "pane_agent_status_changed" {
        tracing::trace!(event = %ev.event, "unhandled pane event");
        return;
    }
    let Some(pane_id) = ev.data.get("pane_id").and_then(|v| v.as_str()) else { return };
    let status = ev
        .data
        .get("agent_status")
        .and_then(|v| v.as_str())
        .map(|s| if s == "done" { "idle" } else { s })
        .unwrap_or("unknown")
        .to_string();
    tracing::info!(host, session, pane_id, status = %status, "pane.agent_status_changed");
    let key: PaneKey = (host.to_string(), session.to_string(), pane_id.to_string());
    let seq = {
        let mut m = status_seq().lock().unwrap();
        let n = m.entry(key.clone()).or_insert(0);
        *n += 1;
        *n
    };

    let fallback = app.session_for_host(host).await.unwrap_or_default();
    // 讀不到不等於這個 pane 沒有 run（#192）：以前讀取錯誤被當成空集合、整則事件丟掉。對 working／idle 之後的對帳還補得回來，
    // 但 `blocked` 那條邊的副作用（告訴父 agent 它的 child 在等）只有這一次——child 停在同一個問題上不會再有第二個狀態事件。
    // 稍後重放這一則（這個 pane 之後沒有更新的事件才算數）；重放也讀不到，還有 `child_alerts::sweep` 的定時安全網。
    let runs = match crate::db::active_runs_for_pane(&app.db, host, pane_id, session, &fallback).await {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(host, pane_id, status = %status, error = ?e, "could not look up the run for a pane status event; replaying it shortly");
            replay_status_later(app, host, session, ev, key, seq, attempt);
            return;
        }
    };
    let Some(run) = runs.into_iter().next() else {
        return;
    };
    let prev = run.agent_status.clone();
    // 卡住的 turn 要「持續」idle 才收：每個狀態事件都記，閃一下 working 就重算。
    crate::lifecycle::observe_agent_status(&run.id, &status);
    // 閒置回收收機前會對這一份（issue #144）：DB 寫不進去時，DB 的 idle 不能被當成閒著的證據。
    crate::supervisor::idle_sleep::observe_status(&run.id, &status);
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
    app.emit_bot_status(&run.bot_id).await;

    // The agent reacted to the prompt: the stall watchdog is no longer needed.
    if status == "working" || status == "blocked" {
        crate::lifecycle::cancel_stall(app, &run.id).await;
    }

    // 停下來等人回答時先看是不是 claude 的滿意度問卷——是就自己按 0（§3.1）。其他 blocked 不動。
    if prev != "blocked" && status == "blocked" {
        let (app2, run2) = (app.clone(), run.clone());
        tokio::spawn(async move {
            crate::tui_prompts::dismiss_if_survey(&app2, &run2).await;
        });
        // grok 撞週限的畫面（#222）：herdr 把它判成 `blocked`、沒有 working->idle 那條邊，畫面掃描不會自己跑。
        // 不是 grok 的 bot、或畫面上沒有那兩句，掃描什麼都不做；一個鍵都不按（選項 1、2 是付費）。
        if crate::db::bot(&app.db, &run.bot_id).await.ok().flatten().is_some_and(|b| b.kind == "grok") {
            crate::lifecycle::schedule_codex_notice_capture(app, &run.bot_id, &run.id);
        }
        // 子 agent 停在提問時，它的父 agent 不會自己知道（`child_alerts`）：UI 的徽章是給人看的，
        // 父 agent 是一顆 CLI 行程，沒有人打字進去就什麼都收不到。
        crate::child_alerts::on_child_blocked(app, &run);
    }
    // 不再 blocked：同一個問題下次再出現時才要再講一次。
    if prev == "blocked" && status != "blocked" {
        crate::child_alerts::forget(&run.bot_id);
    }

    // 沒有 in-flight turn 卻開始 working＝有人在 pane 裡打字：現在就開 external turn，UI 才串得到；
    // 等 Stop hook 的話整段回合對話都是空的。
    if prev != "working" && status == "working" && matches!(crate::db::in_flight_turn(&app.db, &run.id).await, Ok(None)) {
        crate::lifecycle::begin_external_turn(app, &run).await;
    }

    // §11.4.3：遠端 run 的內容留在那台的 spool，先 drain 再 arm 備援，終端快照才只在 hook 真的沒來時贏。
    // 有預算上限（下一個事件排在它後面），逾時就讓備援接手，CAS 保證不雙寫。
    if host != LOCAL_HOST && ((prev == "working" && status == "idle") || (prev != "blocked" && status == "blocked")) {
        let drain = crate::hookrecv::drain_remote_coalesced(app, host, &run.bot_id);
        match tokio::time::timeout(DRAIN_BUDGET, drain).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(host, bot_id = %run.bot_id, error = ?e, "remote drain failed"),
            Err(_) => tracing::warn!(host, bot_id = %run.bot_id, "remote drain timed out"),
        }
    }

    // §4.3: working -> idle arms the terminal fallback. `blocked` never does.
    if prev == "working" && status == "idle" {
        crate::lifecycle::arm_fallback(app, &run.id, &run.bot_id).await;
        // A prompt queued while the agent was still working waits for exactly this edge.
        crate::lifecycle::schedule_flush_queued(app, &run.bot_id);
    } else if prev != "idle" && status == "idle" {
        // 剛起來（`unknown`）或對話框剛關掉（`blocked`）就閒下來：排著的也該送了——bot 沒在跑時收下的那一則
        // （issue #122）正是等這一刻，不然要等退避的 timer（最少 15 秒）。flush 自己的閘門照舊把關。
        crate::lifecycle::schedule_flush_queued(app, &run.bot_id);
    }
}

// ---------------------------------------------------------------- agent titles

/// 標題輪詢間隔：herdr 沒有標題變更事件，只能問。一台一次 `agent.list` 覆蓋所有 run，變了才寫。
const TITLE_POLL: Duration = Duration::from_secs(4);

/// Titles that carry no information: the CLI's own name before it has been given a task.
const PLACEHOLDER_TITLES: &[&str] = &["claude code", "claude", "codex", "grok", "grok cli", "terminal", "zsh", "bash"];

/// 清理終端標題：herdr 已去掉 spinner，但有些 CLI 把狀態寫進標題本身（grok 的 `- Thinking - … - grok`），
/// 所以前後的破折號也要修掉。只重複燈號資訊的標題回 `None`。
fn clean_title(raw: &str) -> Option<String> {
    let t = raw.trim().trim_matches('-').trim();
    if t.is_empty() || PLACEHOLDER_TITLES.contains(&t.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(t.to_string())
}

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
            if app.herdr_session != "default" && app.default_connected.load(Ordering::SeqCst) {
                let fallback = app.herdr_session.clone();
                let client = app.default_herdr.clone();
                poll_titles(&app, LOCAL_HOST, "default", &fallback, &client).await;
            }
        }
    });
}

async fn poll_titles(app: &Arc<App>, host: &str, session: &str, fallback_session: &str, client: &crate::herdr::HerdrClient) {
    let Ok(agents) = client.agent_list().await else { return };
    for a in agents {
        let (Some(name), Some(raw)) = (a.name.as_deref(), a.terminal_title_stripped.as_deref()) else {
            continue;
        };
        let Some(title) = clean_title(raw) else { continue };
        let title = title.as_str();
        // 名字與 session 都要對：兩個 session 可能有同名 agent。
        let row = sqlx::query_as::<_, (String, String, Option<String>)>(
            &format!(
                "SELECT id, bot_id, agent_title FROM runs WHERE agent_name = ? AND state IN {} AND COALESCE(herdr_session, ?) = ? LIMIT 1",
                crate::db::ACTIVE_STATES
            ),
        )
        .bind(name)
        .bind(fallback_session)
        .bind(session)
        .fetch_optional(&app.db)
        .await;
        let Ok(Some((run_id, bot_id, current))) = row else { continue };
        if current.as_deref() == Some(title) {
            continue;
        }
        let _ = sqlx::query("UPDATE runs SET agent_title = ? WHERE id = ?")
            .bind(title)
            .bind(&run_id)
            .execute(&app.db)
            .await;
        app.emit_bot_status(&bot_id).await;
    }
    let _ = host; // kept in the helper signature for session-scoped tracing/debugging callers
}
