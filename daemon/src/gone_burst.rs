//! 同一台 host 短時間內有多顆頂層 bot 的 agent 同時「不見」（#1884）。
//!
//! 對帳／pane 事件／定時掃描發現 run 的 agent 不在，只會把 run 收成 exited。單顆是常態，但同一台 host 在
//! 幾十秒內一口氣掉三顆以上不同的 bot，多半是外力：某顆 bot 的 `pkill`／`killall` 範圍過大、OOM、整台重啟。
//! 這裡推一則 `ops_alert` 給巡檢（只告警，不重啟、不動 autostart 補開）。
//!
//! **以「這一次轉移自己成功」為證據，不看結束前的快照**：run 可能被對帳、`pane.exited` 事件、`dead_panes::sweep`、
//! `relay_watch` 任何一條路先收掉；`mark_run_exited` 只有 CAS 贏的那一條才會寫 `state='exited'`、`ended_at`（轉移當下的時間）
//! 與 `exit_reason`。所以入口（[`mark_run_exited`] 與 `TurnCommands for App` 的 wrapper、[`after_sweep`]）只做兩件事：
//! 結束**前**讀一次 run 的狀態排掉 `stopping`、結束**後**（wrapper 回 `Recorded`／`TurnOwed`；掃描則是它回傳的 id）spawn 一個任務確認，
//! 不 await、不拖慢收尾。任務裡才讀 DB：
//! - **維護**：任務一開始就讀 `herdr_maintenance` 那一列（純 SELECT，**不呼叫** `herdr_maintenance::active`——它在窗口過期時會同步退休
//!   子 agent、寫交易、發事件），之後拿 run 的 `ended_at` 比窗口（`opened_at <= ended_at < until`）。過期還沒被關的窗口不算維護；讀不到當在維護。
//! - **stop／restart 競速**：結束前讀到 `running`、CAS 前 stop 才寫下 `stopping`，這一筆 CAS 會從 `stopping` 轉成 `exited`，
//!   光看轉移的結果分不出來。stop 一定會把它改標回 `stopped`（`run_state::relabel`），所以等一小段 [`SETTLE`] 再讀：
//!   終態必須還是 `exited` 而且 `exit_reason` 就是這一次的原因（別的路徑先收掉的，`exit_reason` 是它的原因，不歸這裡）。
//! - **時間**：窗內的時間用 run 的 `ended_at`，不是任務跑到的時間。
//!
//! 記帳本身（[`record_at`]）只動記憶體：不 await、不碰 DB、std Mutex 臨界區只有一次 retain／push，每 host 最多 [`MAX_RECORDS`] 筆，
//! 等著確認的任務最多 [`MAX_PENDING`] 個，超過的不確認（那表示同時已經掉了一大波，補不補這幾筆都不影響告警）。
//! 達門檻時另 spawn 一個任務推 inbox（DB 寫）。

use crate::lc_error::RunExit;
use crate::state::App;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 往回看多久內「同時」掉的才算同一波。
pub(crate) const WINDOW: Duration = Duration::from_secs(30);
/// 窗內至少幾顆**不同的**頂層 bot 才告警。
pub(crate) const THRESHOLD: usize = 3;
/// 推過一則之後，同一台 host 這段時間內不再推。
const COOLDOWN: Duration = Duration::from_secs(300);
/// 每台 host 最多記幾筆（窗口內的上限，防記憶體長大）。
const MAX_RECORDS: usize = 256;
/// 結束之後等多久再確認（給 stop 把 pane-exit 收的 run 改標回 `stopped`）。
const SETTLE: Duration = if cfg!(test) { Duration::from_millis(150) } else { Duration::from_secs(3) };
/// 同時最多幾個等著確認的任務。
const MAX_PENDING: usize = 256;
/// `dead_panes::sweep` 收 run 用的原因。
const SWEEP_REASON: &str = "pane gone";

/// 算「agent 被外力弄不見」的收尾原因。daemon 自己的動作（stop／restart／刪除／更新重啟）用別的原因，或是結束前先進 `stopping`，不在這裡。
const COUNTED_REASONS: [&str; 5] = [
    "agent not found during reconcile",
    SWEEP_REASON,
    "pane exited",
    "workspace closed",
    "agent not found in default session",
];

#[derive(Clone, Debug)]
pub(crate) struct Gone {
    pub bot_id: String,
    pub name: String,
    pub run_id: String,
    pub at: String,
    seen: Instant,
}

#[derive(Default)]
struct HostState {
    exits: Vec<Gone>,
    quiet_until: Option<Instant>,
}

fn table() -> &'static Mutex<HashMap<String, HostState>> {
    static T: OnceLock<Mutex<HashMap<String, HostState>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// 記一筆，回「這一筆讓同一台 host 達門檻而且不在冷卻」時的整波名單。只動記憶體。
/// 同一顆 bot 窗內只算一次（重開再掉不加數）；已經在窗外的這一筆（`seen` 太舊）不記。
pub(crate) fn record_at(key: &str, gone: Gone, now: Instant) -> Option<Vec<Gone>> {
    if now.saturating_duration_since(gone.seen) >= WINDOW {
        return None;
    }
    let mut table = table().lock().unwrap_or_else(|e| e.into_inner());
    let st = table.entry(key.to_string()).or_default();
    st.exits.retain(|g| now.saturating_duration_since(g.seen) < WINDOW && g.bot_id != gone.bot_id);
    st.exits.push(gone);
    if st.exits.len() > MAX_RECORDS {
        let excess = st.exits.len() - MAX_RECORDS;
        st.exits.drain(..excess);
    }
    if st.quiet_until.is_some_and(|q| now < q) {
        return None;
    }
    st.quiet_until = None;
    if st.exits.len() < THRESHOLD {
        return None;
    }
    st.quiet_until = Some(now + COOLDOWN);
    Some(st.exits.clone())
}

fn clear_quiet(key: &str) {
    if let Some(st) = table().lock().unwrap_or_else(|e| e.into_inner()).get_mut(key) {
        st.quiet_until = None;
    }
}

fn key_of(app: &App, host: &str) -> String {
    format!("{}\u{0}{host}", app.data_dir.display())
}

struct Who {
    bot_id: String,
    name: String,
    host: String,
}

/// run 屬於哪顆頂層 bot、在哪台 host。子 agent（含被收編、被取代的）、已刪的 bot 都不算。
async fn who(app: &Arc<App>, bot_id: &str) -> Option<Who> {
    let bot = crate::db::bot(&app.db, bot_id).await.ok()??;
    if bot.managed_by == "child" || bot.deleted_at.is_some() {
        return None;
    }
    let host = crate::db::bot_host(&app.db, &bot.id).await.ok()?;
    Some(Who { bot_id: bot.id, name: bot.name, host })
}

/// 結束**前**的判斷（一次主鍵查詢）：原因在清單內，而且 run 還是 `running`／`starting`——`stopping`（使用者或 daemon 的
/// stop／restart）一開始就不算。結束前讀到 `running` 不保證結束當下還是，那一段由任務裡的 [`SETTLE`] 之後再確認。
pub(crate) async fn eligible(app: &Arc<App>, run_id: &str, reason: &str) -> bool {
    if !COUNTED_REASONS.contains(&reason) {
        return false;
    }
    matches!(crate::db::run(&app.db, run_id).await, Ok(Some(run)) if matches!(run.state.as_str(), "running" | "starting"))
}

/// wrapper 的用法：`eligible` → 原本的 `lifecycle::mark_run_exited` → `after_exit`。這一條路真的寫下 exited 才排確認。
pub(crate) fn after_exit(app: &Arc<App>, eligible: bool, run_id: &str, reason: &str, exit: RunExit) {
    if eligible && matches!(exit, RunExit::Recorded | RunExit::TurnOwed) {
        confirm_later(app, vec![run_id.to_string()], reason.to_string());
    }
}

/// 三個步驟包成一個呼叫（兩個 `mark_run_exited` 的 wrapper 都走這裡）。回傳值就是原本的回傳值。
pub(crate) async fn mark_run_exited(app: &Arc<App>, run_id: &str, reason: &str) -> RunExit {
    let eligible = eligible(app, run_id, reason).await;
    let exit = crate::lifecycle::mark_run_exited(app, run_id, reason).await;
    after_exit(app, eligible, run_id, reason, exit);
    exit
}

/// `dead_panes::sweep` 回傳的 id（它把 `AlreadyEnded` 也算進去，所以不能直接當「這次收掉的」）：交給任務確認。
pub(crate) fn after_sweep(app: &Arc<App>, run_ids: &[String]) {
    if !run_ids.is_empty() {
        confirm_later(app, run_ids.to_vec(), SWEEP_REASON.to_string());
    }
}

/// 在 `ended_at` 那一刻有沒有在維護：窗口是 `opened_at <= ended_at < until`。窗口過了 `until` 還沒被關也不算維護中。
fn in_window(w: &crate::herdr_maintenance::Window, ended_at: &str) -> bool {
    crate::db::cmp_ts(&w.opened_at, ended_at).is_le() && crate::db::cmp_ts(ended_at, &w.until).is_lt()
}

static PENDING: AtomicUsize = AtomicUsize::new(0);

/// 等著確認的任務的名額；任務結束（含 panic 展開）就還。
struct Slot;

impl Slot {
    fn take() -> Option<Slot> {
        if PENDING.fetch_add(1, Ordering::AcqRel) < MAX_PENDING {
            return Some(Slot);
        }
        PENDING.fetch_sub(1, Ordering::AcqRel);
        None
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        PENDING.fetch_sub(1, Ordering::AcqRel);
    }
}

fn confirm_later(app: &Arc<App>, run_ids: Vec<String>, reason: String) {
    let Some(slot) = Slot::take() else {
        tracing::debug!("gone burst: too many exits waiting to be confirmed; skipped");
        return;
    };
    let app = app.clone();
    crate::background_loop::spawn_named(
        &app.background_tasks.clone(),
        "gone burst confirm",
        Box::pin(async move {
            let _slot = slot;
            // 維護窗口趁還在時先讀（純 SELECT）；讀不到＝不知道，當在維護，這一批都不算。
            let window = crate::herdr_maintenance::row(&app.db).await;
            tokio::time::sleep(SETTLE).await;
            let Ok(window) = window else { return };
            for run_id in &run_ids {
                if let Some((who, at)) = confirmed(&app, run_id, &reason, window.as_ref()).await {
                    note(&app, who, run_id.clone(), at);
                }
            }
        }),
    );
}

/// 這一次轉移自己成功的證據：終態還是 `exited`（stop 改標成 `stopped`、別的路徑改過的都不是）、`exit_reason` 是這次的原因、
/// 轉移當下不在維護、屬於頂層 bot。回 bot 與轉移當下的時間。
async fn confirmed(app: &Arc<App>, run_id: &str, reason: &str, window: Option<&crate::herdr_maintenance::Window>) -> Option<(Who, String)> {
    let (bot_id, state, exit_reason, ended_at): (String, String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT bot_id, state, exit_reason, ended_at FROM runs WHERE id = ?").bind(run_id).fetch_optional(&app.db).await.ok()??;
    let ended_at = ended_at?;
    if state != "exited" || exit_reason.as_deref() != Some(reason) || window.is_some_and(|w| in_window(w, &ended_at)) {
        return None;
    }
    Some((who(app, &bot_id).await?, ended_at))
}

fn note(app: &Arc<App>, who: Who, run_id: String, at: String) {
    // 轉移當下到現在過了多久；時間戳壞了就當剛剛。
    let age = crate::db::parse_ts(&at).and_then(|t| (chrono::Utc::now() - t).to_std().ok()).unwrap_or_default();
    let now = Instant::now();
    let seen = now.checked_sub(age).unwrap_or(now);
    let key = key_of(app, &who.host);
    let gone = Gone { bot_id: who.bot_id, name: who.name, run_id, at, seen };
    let Some(wave) = record_at(&key, gone, now) else { return };
    let app = app.clone();
    let host = who.host;
    crate::background_loop::spawn_named(
        &app.background_tasks.clone(),
        "gone burst alert",
        Box::pin(async move {
            if let Err(e) = push(&app, &host, &wave).await {
                clear_quiet(&key);
                tracing::error!(host = %host, error = ?e, "could not queue the agents_gone_burst ops_alert");
            }
        }),
    );
}

async fn push(app: &Arc<App>, host: &str, wave: &[Gone]) -> anyhow::Result<()> {
    let names: Vec<&str> = wave.iter().map(|g| g.name.as_str()).collect();
    let bots: Vec<_> = wave.iter().map(|g| json!({"bot_id": g.bot_id, "name": g.name, "run_id": g.run_id, "gone_at": g.at})).collect();
    let first = wave.first().map(|g| g.at.as_str()).unwrap_or_default();
    let last = wave.last().map(|g| g.at.as_str()).unwrap_or_default();
    let detail = format!(
        "host `{host}` 在 {} 秒內有 {} 顆不同的 bot 的 agent 同時不見（{}）。daemon 只把它們的 run 收成 exited，沒有重啟任何一顆。",
        WINDOW.as_secs(),
        wave.len(),
        names.join("、"),
    );
    let payload = json!({
        "source": "daemon",
        "reason": "agents_gone_burst",
        "subject": host,
        "host": host,
        "count": wave.len(),
        "bots": bots,
        "bot_names": names,
        "window_secs": WINDOW.as_secs(),
        "first_gone_at": first,
        "last_gone_at": last,
        "detail": detail,
        "action": "不是 daemon 的 stop／restart、也不是計畫中的 herdr 維護：查那台 host 上有沒有範圍過大的 pkill／killall／kill -9 -1、OOM 或整台重啟，也可能是使用者一次手動關了多個 pane（daemon.log 搜 `agent gone`／`pane exited`）；autostart 的 bot 照原本規則補開，其餘要人手 `start?resume=native`。",
    });
    // key 帶窗內最早那顆 run：daemon 重啟後同一波重推也會被 INSERT OR IGNORE 吞掉。
    let first_run = wave.first().map(|g| g.run_id.as_str()).unwrap_or_default();
    let key = format!("ops_alert:daemon:agents_gone_burst:{host}:{first_run}");
    if crate::supervisor_inbox::push_inbox(&app.db, &key, "ops_alert", None, None, None, &payload).await?.is_some() {
        tracing::warn!(host, count = wave.len(), "several agents vanished at once; ops_alert queued");
        app.emit("supervisor_changed", json!({"ops_alert": key})).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ports::TurnCommands;
    use crate::lc_error::RunExit;
    use crate::testing::{self as tt, eventually};
    use crate::{config, db};

    fn gone(bot: &str, at: Instant) -> Gone {
        Gone { bot_id: bot.into(), name: bot.into(), run_id: format!("run-{bot}"), at: String::new(), seen: at }
    }

    // ---- 記帳（純記憶體，時間由測試給）----

    #[test]
    fn individual_exits_do_not_alert_and_the_third_distinct_bot_does_once() {
        let k = "t-three";
        let t = Instant::now();
        assert!(record_at(k, gone("a", t), t).is_none());
        assert!(record_at(k, gone("b", t), t).is_none());
        let wave = record_at(k, gone("c", t), t).expect("third distinct bot");
        assert_eq!(wave.len(), 3);
        assert!(record_at(k, gone("d", t), t).is_none(), "同一窗口不重複");
        assert!(record_at(k, gone("e", t + Duration::from_secs(5)), t + Duration::from_secs(5)).is_none());
    }

    #[test]
    fn the_same_bot_gone_again_counts_once() {
        let k = "t-same";
        let t = Instant::now();
        for _ in 0..5 {
            assert!(record_at(k, gone("a", t), t).is_none());
        }
        assert!(record_at(k, gone("b", t), t).is_none(), "a 重開掉五次＋b ＝ 2 顆");
        assert_eq!(record_at(k, gone("c", t), t).unwrap().len(), 3);
    }

    #[test]
    fn exits_older_than_the_window_are_forgotten() {
        let k = "t-window";
        let t = Instant::now();
        record_at(k, gone("a", t), t);
        record_at(k, gone("b", t), t);
        let later = t + WINDOW + Duration::from_secs(1);
        assert!(record_at(k, gone("c", later), later).is_none(), "a、b 已在窗外");
    }

    #[test]
    fn hosts_are_counted_apart_and_cooldown_ends() {
        let t = Instant::now();
        record_at("t-h1", gone("a", t), t);
        record_at("t-h1", gone("b", t), t);
        assert!(record_at("t-h2", gone("c", t), t).is_none(), "跨 host 不合併");
        assert!(record_at("t-h1", gone("c", t), t).is_some());
        let much_later = t + COOLDOWN + Duration::from_secs(1);
        for b in ["x", "y"] {
            assert!(record_at("t-h1", gone(b, much_later), much_later).is_none());
        }
        assert!(record_at("t-h1", gone("z", much_later), much_later).is_some(), "冷卻過後下一波可以再推");
    }

    #[test]
    fn the_record_list_is_capped() {
        let k = "t-cap";
        let t = Instant::now();
        record_at(k, gone("first", t), t);
        for n in 0..MAX_RECORDS + 10 {
            record_at(k, gone(&format!("b{n}"), t), t);
        }
        assert!(table().lock().unwrap().get(k).unwrap().exits.len() <= MAX_RECORDS);
    }

    // ---- 端到端（wrapper → 記帳 → spawn 推 inbox）----

    async fn alerts(app: &Arc<crate::state::App>) -> Vec<serde_json::Value> {
        crate::supervisor::store::inbox(&app.db, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "ops_alert")
            .map(|e| serde_json::from_str::<serde_json::Value>(&e.payload_json).unwrap())
            .filter(|p| p["reason"] == "agents_gone_burst")
            .collect()
    }

    async fn running(env: &tt::Env, names: &[&str]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for n in names {
            let bot = tt::claude_bot(&env.app, &env.project_id, n).await;
            out.push((bot.id.clone(), tt::fake_run(&env.app, &bot.id).await));
        }
        out
    }

    async fn quiet_period() {
        tokio::time::sleep(SETTLE * 3).await;
    }

    #[tokio::test]
    async fn a_reconcile_that_loses_many_agents_pushes_one_alert() {
        let env = tt::env().await;
        for n in ["a", "b", "c"] {
            let bot = tt::claude_bot(&env.app, &env.project_id, n).await;
            crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        }
        env.herdr.agents.lock().unwrap().clear();
        env.herdr.workspaces.lock().unwrap().clear();
        env.herdr.tabs.lock().unwrap().clear();
        crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();
        assert!(eventually!(alerts(&env.app).await.len() == 1));
        let a = alerts(&env.app).await;
        assert_eq!((a[0]["count"].as_u64(), a[0]["host"].as_str(), a[0]["window_secs"].as_u64()), (Some(3), Some(config::LOCAL_HOST), Some(30)));
        crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();
        quiet_period().await;
        assert_eq!(alerts(&env.app).await.len(), 1, "同一波不重複");
    }

    #[tokio::test]
    async fn pane_exited_events_that_end_the_runs_first_are_counted_too() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        for (_, run) in &runs[..2] {
            env.app.mark_run_exited(run, "pane exited").await;
        }
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty(), "兩顆不告警");
        env.app.mark_run_exited(&runs[2].1, "workspace closed").await;
        assert!(eventually!(alerts(&env.app).await.len() == 1));
        assert_eq!(alerts(&env.app).await[0]["count"], 3);
    }

    #[tokio::test]
    async fn the_dead_pane_sweep_wrapper_counts_the_runs_it_ended() {
        use crate::lifecycle::s6_ports::RelayWatchServices;
        let env = tt::env().await;
        // fake_run 的 pane 在 herdr 裡不存在：sweep 會把它們收成 `pane gone`。
        let runs = running(&env, &["a", "b", "c"]).await;
        let swept = env.app.sweep_dead_panes().await;
        assert_eq!(swept.len(), 3, "{swept:?} for {runs:?}");
        assert!(eventually!(alerts(&env.app).await.len() == 1));
    }

    #[tokio::test]
    async fn sweep_ids_that_another_path_ended_are_not_counted() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        let ids: Vec<String> = runs.iter().map(|(_, r)| r.clone()).collect();
        // `dead_panes::sweep` 把 `AlreadyEnded` 也放進回傳：使用者 stop 先完成（stopped、沒有 exit_reason）、
        // 別的路徑先收掉（exit_reason 是它自己的原因）都不歸掃描。
        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?").bind(db::now()).bind(&ids[0]).execute(&env.app.db).await.unwrap();
        crate::lifecycle::mark_run_exited(&env.app, &ids[1], "restart for the claude update failed").await;
        crate::lifecycle::mark_run_exited(&env.app, &ids[2], "pane exited").await;
        after_sweep(&env.app, &ids);
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty());
    }

    #[tokio::test]
    async fn sweep_exits_are_stamped_when_they_happened_not_when_the_sweep_returned() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        let ids: Vec<String> = runs.iter().map(|(_, r)| r.clone()).collect();
        for (n, id) in ids.iter().enumerate() {
            crate::lifecycle::mark_run_exited(&env.app, id, SWEEP_REASON).await;
            // 一顆在一分鐘前、其餘剛剛：窗是 30 秒，不能因為同一輪掃描一起回傳就算同一波。
            let at = db::iso_in(if n == 0 { -60 } else { 0 });
            sqlx::query("UPDATE runs SET ended_at=? WHERE id=?").bind(at).bind(id).execute(&env.app.db).await.unwrap();
        }
        after_sweep(&env.app, &ids);
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty(), "一分鐘前那顆在窗外，只剩 2 顆");
    }

    /// 結束前讀到 `running`、CAS 之前 stop 才寫下 `stopping`：CAS 會從 `stopping` 轉成 `exited`，之後 stop 改標回 `stopped`。
    #[tokio::test]
    async fn a_stop_that_lands_between_the_read_and_the_exit_is_not_counted() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        for (_, run) in &runs {
            let (db, id) = (env.app.db.clone(), run.clone());
            crate::lifecycle::race_point::arm("mark_run_exited_after_read", run, move || async move {
                sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&id).execute(&db).await.unwrap();
            });
            assert_eq!(env.app.mark_run_exited(run, "pane exited").await, RunExit::Recorded);
            // stop 的 `commit_stopped`：被 pane-exit 收成 exited 之後改標成 stopped。
            crate::lifecycle::run_state::relabel(&env.app.db, run, "exited", "stopped").await.unwrap();
        }
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty());
    }

    /// 對照：同樣的競速，stop 沒有接著把它改標回 `stopped`（外力先到、stop 沒做成），就照常算。
    #[tokio::test]
    async fn the_same_race_without_a_stop_relabel_is_counted() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        for (_, run) in &runs {
            let (db, id) = (env.app.db.clone(), run.clone());
            crate::lifecycle::race_point::arm("mark_run_exited_after_read", run, move || async move {
                sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&id).execute(&db).await.unwrap();
            });
            env.app.mark_run_exited(run, "pane exited").await;
        }
        assert!(eventually!(alerts(&env.app).await.len() == 1));
    }

    /// 維護窗口在結束前讀完之後、CAS 之前才開：以結束當下為準。
    #[tokio::test]
    async fn a_maintenance_window_opened_between_the_read_and_the_exit_covers_it() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        let db = env.app.db.clone();
        crate::lifecycle::race_point::arm("mark_run_exited_after_read", &runs[0].1, move || async move {
            sqlx::query("INSERT OR REPLACE INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,'2999-01-01T00:00:00.000Z','patrol','test')")
                .bind(db::now())
                .execute(&db)
                .await
                .unwrap();
        });
        for (_, run) in &runs {
            env.app.mark_run_exited(run, "agent not found during reconcile").await;
        }
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty(), "第一顆的 CAS 已在窗內；其餘兩顆也在維護中");
    }

    /// 過期但還沒被關的窗口不算維護中；告警探測不能順手把它關掉（沒有副作用）。
    #[tokio::test]
    async fn an_expired_maintenance_window_is_not_maintenance_and_is_left_alone() {
        let env = tt::env().await;
        sqlx::query("INSERT OR REPLACE INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,'2000-01-01T00:00:00.000Z','2000-01-01T00:30:00.000Z','patrol','test')")
            .execute(&env.app.db)
            .await
            .unwrap();
        let runs = running(&env, &["a", "b", "c"]).await;
        for (_, run) in &runs {
            env.app.mark_run_exited(run, "pane exited").await;
        }
        assert!(eventually!(alerts(&env.app).await.len() == 1));
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM herdr_maintenance").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(rows, 1, "窗口沒被探測關掉");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_notes WHERE kind = 'herdr_maintenance_expired'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(notes, 0);
    }

    #[tokio::test]
    async fn stopping_runs_and_children_are_not_counted() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        for (_, run) in &runs {
            sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(run).execute(&env.app.db).await.unwrap();
            env.app.mark_run_exited(run, "pane exited").await;
        }
        let kids = running(&env, &["k1", "k2", "k3"]).await;
        for (bot, run) in &kids {
            sqlx::query("UPDATE bots SET managed_by='child' WHERE id=?").bind(bot).execute(&env.app.db).await.unwrap();
            env.app.mark_run_exited(run, "pane exited").await;
        }
        let other = running(&env, &["x", "y", "z"]).await;
        for (_, run) in &other {
            env.app.mark_run_exited(run, "restart for the claude update failed").await;
        }
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty());
    }

    #[tokio::test]
    async fn the_same_bot_coming_back_and_going_again_is_one_bot() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "flappy").await;
        for _ in 0..3 {
            let run = tt::fake_run(&env.app, &bot.id).await;
            env.app.mark_run_exited(&run, "pane exited").await;
        }
        let other = running(&env, &["b"]).await;
        env.app.mark_run_exited(&other[0].1, "pane exited").await;
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty(), "3 次重開＋另一顆＝ 2 顆");
    }

    #[tokio::test]
    async fn exits_during_planned_maintenance_are_not_added_to_one_after_it() {
        let env = tt::env().await;
        let during = running(&env, &["m1", "m2", "m3"]).await;
        sqlx::query("INSERT OR REPLACE INTO herdr_maintenance (id, opened_at, until, opened_by, reason) VALUES (1,?,'2999-01-01T00:00:00.000Z','patrol','test')")
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        for (_, run) in &during {
            env.app.mark_run_exited(run, "agent not found during reconcile").await;
        }
        quiet_period().await;
        sqlx::query("DELETE FROM herdr_maintenance").execute(&env.app.db).await.unwrap();
        let after = running(&env, &["p1", "p2", "p3"]).await;
        env.app.mark_run_exited(&after[0].1, "pane exited").await;
        env.app.mark_run_exited(&after[1].1, "pane exited").await;
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty(), "維護期間的三顆不算：只有 2 顆在維護後");
        env.app.mark_run_exited(&after[2].1, "pane exited").await;
        assert!(eventually!(alerts(&env.app).await.len() == 1));
        let a = alerts(&env.app).await;
        let mut names: Vec<_> = a[0]["bot_names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, ["p1", "p2", "p3"]);
    }
}
