//! herdr 事件訂閱與處理（SPEC §3.1、§6.6、§11.3.6）。
//!
//! pane id 只在一個 herdr session 內唯一，所以查表一律以 `(host, session, pane_id)` 為鍵。
//! herdr 的事件名稱點號／底線兩種寫法都有，比對前先正規化。

use std::sync::Arc;
use std::time::Duration;

/// P8（hook／事件／對帳）對其他 feature 的窄介面與 `App` 端實作（crate 拆分第 3 步）；檔案在 `daemon/src/`，不碰 `lib.rs`。
#[path = "ingress_ports.rs"]
pub mod ports;

/// How long `handle_status` waits for a remote drain before letting the fallback arm (§11.4.3).
pub const DRAIN_BUDGET: Duration = Duration::from_secs(4);
pub const HERDR_SUBSCRIBE_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
pub const HERDR_SUBSCRIBE_MAX_BACKOFF: Duration = Duration::from_secs(10);
pub const HERDR_SUBSCRIBE_STABLE_FOR: Duration = Duration::from_secs(30);

/// A short-lived subscription is still a failed connection attempt. Only a stable stream earns a
/// reset, otherwise a flapping local herdr can make every global and per-pane watcher reconnect at
/// the initial 250 ms interval forever.
pub fn herdr_subscribe_retry_delay(current: Duration, connected_for: Option<Duration>) -> Duration {
    if connected_for.is_some_and(|uptime| uptime >= HERDR_SUBSCRIBE_STABLE_FOR) {
        HERDR_SUBSCRIBE_INITIAL_BACKOFF
    } else {
        current
    }
}

pub fn norm(name: &str) -> String {
    name.replace('.', "_")
}

/// 偵測到的 agent 可能是 bot 剛開的子 pane，值得對帳；daemon 自己開的探測 workspace（額度探測，[`crate::probe_ws`]）不是——
/// 它們每 30～60 秒就來一個，每個都對整台主機對帳一輪是白做。
pub async fn detection_wants_reconcile(app: &impl crate::capabilities::HerdrRoutes, host: &str, session: &str, data: &serde_json::Value) -> bool {
    let Some(ws) = data.get("workspace_id").and_then(|v| v.as_str()) else { return true };
    let Some(client) = app.herdr_for_session(host, session).await else { return true };
    !crate::probe_ws::is_probe(client.socket_path(), ws)
}

/// 訂閱（重）建後的對帳；成功且 `autostart` 才補跑欠著的 autostart（#259）。`autostart_hosts` 保證每台主機只完成一次 autostart pass：
/// 之後的重連不會把使用者停掉的 bot 再開起來。
pub async fn reconcile_and_autostart(app: &impl crate::events::ports::ReconcileCommands, host: &str, autostart: bool) -> bool {
    match app.reconcile_host(host).await {
        Ok(()) => {
            if autostart {
                app.autostart_after_reconcile(host, true).await;
            }
            true
        }
        Err(e) => {
            tracing::error!(host, error = ?e, "reconcile failed");
            // 事件驅動的對帳失敗不會有下一個觸發點：斷線那段漏掉的 pane.closed 等不到補，排一輪晚一點的補跑（它失敗會自己再排）。
            app.schedule_deferred_pass(host);
            false
        }
    }
}

/// pane 關閉事件重試的間隔（#247）：DB 讀不到就不能當成「沒有 run」，事件只有這一次，要自己補。
const CLOSE_RETRY: [Duration; 6] = [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30), Duration::from_secs(60), Duration::from_secs(120)];
const CLOSE_RETRY_IN_TESTS: [Duration; 20] = [Duration::from_millis(100); 20];

pub fn close_retry_delays() -> &'static [Duration] {
    if cfg!(test) {
        &CLOSE_RETRY_IN_TESTS
    } else {
        &CLOSE_RETRY
    }
}

/// 這個 pane 目前登記的 watcher 世代（只在持有 `pane_watchers` 時讀寫）。自己結束的 watcher 要把
/// 自己的登記帶走——留著會讓 `watch_pane_on_session` 以為「已經在看」，那個 pane 從此訂閱不起來
/// （燈號凍在收編當下，§6.6）；但不能把同一個 pane 後來裝的**新** watcher 踢掉。
pub fn watcher_gens() -> &'static std::sync::Mutex<std::collections::HashMap<PaneKey, u64>> {
    static G: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PaneKey, u64>>> =
        std::sync::OnceLock::new();
    G.get_or_init(Default::default)
}

pub type PaneKey = (String, String, String);

/// Drop a watcher's own registration, on every path out of its loop. `run_ended`：離開是因為證明了這個 pane 已經沒有 active run
/// （不是拿不到 client）——只有這時才把 pane 的狀態序號與鎖一起帶走。
pub async fn forget_watcher(app: &impl crate::events::PaneWatchers, key: PaneKey, generation: u64, run_ended: bool) {
    let mut watchers = app.pane_watchers().lock().await;
    let mut gens = watcher_gens().lock().unwrap();
    if gens.get(&key) == Some(&generation) {
        gens.remove(&key);
        watchers.remove(&key);
        // watcher 自己退出（run 結束）也要把這個 pane 的狀態序號與鎖帶走，不只在 `unwatch_pane_on_session`：
        // 不然這兩張行程級表每個開過又收掉的 pane 留一格。拿不到 client 而退出的不帶（run 還活著，之後重連會再裝 watcher）。
        if run_ended {
            forget_pane_status_state(&key);
        }
    }
}

pub async fn unwatch_pane_on_session(app: &impl crate::events::PaneWatchers, host: &str, session: &str, pane_id: &str) {
    let key = (host.to_string(), session.to_string(), pane_id.to_string());
    let mut watchers = app.pane_watchers().lock().await;
    watcher_gens().lock().unwrap().remove(&key);
    if let Some(h) = watchers.remove(&key) {
        h.abort();
    }
    drop(watchers);
    forget_pane_status_state(&key);
}

/// 這個 pane 最新那一則狀態事件的編號：讀不到 run 而延後重放的那一則，只在它之後沒有更新的事件時才算數（#192）。
///
/// 編號取自一個**全域**遞增的計數器，不是每個 pane 各自從 1 數起——`forget_pane_status_state` 會在 pane 收掉時
/// 清項目，號碼若會重來，同一個 pane 的新事件就可能撞上某個還在等的舊重放手上那個號碼（issue #521）。
pub fn status_seq() -> &'static std::sync::Mutex<std::collections::HashMap<PaneKey, u64>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PaneKey, u64>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// 讀不到 run 的狀態事件隔多久重放一次；用完就交給 `child_alerts::sweep` 的定時安全網。
#[allow(dead_code)]
const STATUS_REPLAY: [Duration; 4] = [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)];
#[allow(dead_code)]
const STATUS_REPLAY_IN_TESTS: [Duration; 20] = [Duration::from_millis(100); 20];

pub fn status_replay_delays() -> &'static [Duration] {
    if cfg!(test) {
        &STATUS_REPLAY_IN_TESTS
    } else {
        &STATUS_REPLAY
    }
}

/// 同一個 pane 的狀態事件**一則一則寫**（#192 的補洞）：重放的那一則在「讀 run 到寫狀態」之間，較新的事件可能整則跑完，
/// 重放這時再把舊狀態寫回去就把新的蓋掉（`blocked` 蓋掉已經回答的 `idle`）。所以讀 run、比對「我還是不是最新」、寫狀態
/// 這一段在 pane 的鎖裡做；較新的事件一到就先登記序號（鎖之前），拿到鎖的重放看到序號變了就放棄。
pub fn pane_status_locks() -> &'static std::sync::Mutex<std::collections::HashMap<PaneKey, Arc<tokio::sync::Mutex<()>>>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PaneKey, Arc<tokio::sync::Mutex<()>>>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

pub fn pane_status_lock(key: &PaneKey) -> Arc<tokio::sync::Mutex<()>> {
    pane_status_locks().lock().unwrap_or_else(|e| e.into_inner()).entry(key.clone()).or_default().clone()
}

/// pane 不再有 run（關掉、或 run 收掉）時把這兩張表的項目帶走：它們是行程級的，不清的話每個開過又收掉的
/// pane 都留一格，長跑的 daemon 只增不減。拿走 `Arc` 的重放還握著自己那份，清掉只是不再有人查得到它。
///
/// 清得掉是因為 `status_seq` 的號碼取自**全域**計數器：同一個 pane 之後又有新 run 時，新的事件拿到的是更大的
/// 號碼，不會跟某個還沒跑完的重放撞號（重放看到號碼對不上就放棄，這正是它要的）。
pub fn forget_pane_status_state(key: &PaneKey) {
    status_seq().lock().unwrap().remove(key);
    pane_status_locks().lock().unwrap_or_else(|e| e.into_inner()).remove(key);
}

// ---------------------------------------------------------------- agent titles

/// Titles that carry no information: the CLI's own name before it has been given a task.
const PLACEHOLDER_TITLES: &[&str] = &["claude code", "claude", "codex", "grok", "grok cli", "terminal", "zsh", "bash"];

/// 清理終端標題：herdr 已去掉 spinner，但有些 CLI 把狀態寫進標題本身（grok 的 `- Thinking - … - grok`），
/// 所以前後的破折號也要修掉。只重複燈號資訊的標題回 `None`。
pub fn clean_title(raw: &str) -> Option<String> {
    let t = raw.trim().trim_matches('-').trim();
    if t.is_empty() || PLACEHOLDER_TITLES.contains(&t.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(t.to_string())
}

pub async fn poll_titles(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db), host: &str, session: &str, fallback_session: &str, client: &crate::herdr::HerdrClient) {
    let Ok(agents) = client.agent_list().await else { return };
    for a in agents {
        let Some(raw) = a.terminal_title_stripped.as_deref() else { continue };
        let Some(title) = clean_title(raw) else { continue };
        apply_agent_title(app, host, session, fallback_session, &a.pane_id, &title).await;
    }
}

/// 把標題寫進這個 pane 的 run。run 一律用 host＋herdr session＋pane_id 找（名稱是可改的顯示字串，改名後會對不上，
/// 同名的別顆 agent 又會被誤認；pane ID 只在各自 host 內唯一，所以 host 也要對）。
pub(crate) async fn apply_agent_title(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db), host: &str, session: &str, fallback_session: &str, pane_id: &str, title: &str) {
    let Ok(runs) = crate::db::active_runs_for_pane(app.db(), host, pane_id, session, fallback_session).await else { return };
    let Some(run) = runs.into_iter().next() else { return };
    if run.agent_title.as_deref() == Some(title) {
        return;
    }
    let _ = sqlx::query("UPDATE runs SET agent_title = ? WHERE id = ?")
        .bind(title)
        .bind(&run.id)
        .execute(app.db())
        .await;
    app.emit_bot_status(&run.bot_id).await;
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;

    #[test]
    fn herdr_subscription_backoff_resets_only_after_a_stable_stream() {
        let current = Duration::from_secs(4);
        assert_eq!(
            herdr_subscribe_retry_delay(current, Some(Duration::from_secs(29))),
            current,
            "短暫成功後又斷線仍按退避延遲重試"
        );
        assert_eq!(
            herdr_subscribe_retry_delay(current, Some(HERDR_SUBSCRIBE_STABLE_FOR)),
            HERDR_SUBSCRIBE_INITIAL_BACKOFF,
            "穩定連線才重設延遲"
        );
        assert_eq!(
            herdr_subscribe_retry_delay(current, None),
            current,
            "連線建立失敗也不重設延遲"
        );
    }

    async fn title_of(db: &sqlx::SqlitePool, run: &str) -> Option<String> {
        sqlx::query_scalar("SELECT agent_title FROM runs WHERE id = ?").bind(run).fetch_one(db).await.unwrap()
    }

    /// 名稱不當參照：標題寫回靠 host＋session＋pane_id 找 run。run 上的 agent_name 是會改的顯示字串——
    /// 改名後仍要寫得進去；別台 host 上同 pane id、同 agent 名的 run、以及別人用了舊名，都不能被寫到。
    #[tokio::test]
    async fn title_write_back_finds_the_run_by_pane_not_by_name() {
        use crate::testing as tt;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "title-bot").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        // 使用者把 herdr agent 改名：DB 裡的 agent_name 跟 herdr 現在報的名字對不上。
        sqlx::query("UPDATE runs SET agent_name = 'renamed-away' WHERE id = ?").bind(&run).execute(&app.db).await.unwrap();

        // 別台 host 的 run：同 pane id、同 session、agent_name 恰好是 herdr 現在報的名字。
        let remote_project = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'remote', 'mac2', ?)")
            .bind(&remote_project)
            .bind(format!("{}/remote", e.dir.display()))
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let remote_bot = tt::claude_bot(&app, &remote_project, "title-remote").await;
        let remote_run = tt::fake_run(&app, &remote_bot.id).await;
        sqlx::query("UPDATE runs SET pane_id = ?, agent_name = 'agent' WHERE id = ?").bind(&pane).bind(&remote_run).execute(&app.db).await.unwrap();

        apply_agent_title(&*app, "local", "test", "test", &pane, "修 bug").await;
        assert_eq!(title_of(&app.db, &run).await.as_deref(), Some("修 bug"), "改名後仍以 pane 認得 run");
        assert_eq!(title_of(&app.db, &remote_run).await, None, "別台 host 同 pane id 的 run 不會被寫到");

        apply_agent_title(&*app, "mac2", "test", "test", &pane, "遠端標題").await;
        assert_eq!(title_of(&app.db, &remote_run).await.as_deref(), Some("遠端標題"));
        assert_eq!(title_of(&app.db, &run).await.as_deref(), Some("修 bug"), "本機的 run 不受遠端那次影響");

        apply_agent_title(&*app, "local", "test", "test", "pane-unknown", "不存在").await;
        assert_eq!(title_of(&app.db, &run).await.as_deref(), Some("修 bug"), "pane 對不上就不寫，不改用名稱猜");
    }
}

/// 每個 pane 的事件 watcher 任務。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait PaneWatchers: Send + Sync {
    fn pane_watchers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<(String, String, String), tokio::task::JoinHandle<()>>>;
}
