//! 同一台 host 短時間內有多顆頂層 bot 的 agent 同時「不見」（#1884）。
//!
//! 對帳／pane 事件／定時掃描發現 run 的 agent 不在，只會把 run 收成 exited。單顆是常態，但同一台 host 在
//! 幾十秒內一口氣掉三顆以上不同的 bot，多半是外力：某顆 bot 的 `pkill`／`killall` 範圍過大、OOM、整台重啟。
//! 這裡推一則 `ops_alert` 給巡檢（只告警，不重啟、不動 autostart 補開）。
//!
//! **證據來自轉移本身**：`lifecycle::mark_run_exited_proved` 的 CAS 自己回報「這一次由我寫下 exited」（`ExitProof`：轉移前是不是
//! `stopping`、轉移當下的時間）；CAS 輸給別的路徑的、寫不進去的沒有 proof。定時掃描用 `dead_panes::sweep_proved`，只回自己成功轉移的 run。
//! 所以入口（[`mark_run_exited`] 與 `TurnCommands for App` 的 wrapper、[`after_sweep`]）不讀任何 DB、不 await 額外工作：
//! 原因不在清單、或轉移前是 `stopping`（stop／restart／更新重啟）就丟掉，其餘 spawn 一個任務查 bot（頂層、未刪、host）後記一筆。
//! 時間用 proof 的 `at`（事件時間），沒有任何延遲或事後推論。
//!
//! 記帳本身（[`record_at`]）只動記憶體：不 await、不碰 DB、std Mutex 臨界區有界，每 host 最多 [`MAX_RECORDS`] 筆，歷史留 [`RETAIN`]；
//! 等著查 bot 的任務最多 [`MAX_PENDING`] 個，超過的不記（那表示同時已經掉了一大波）。
//!
//! **維護歷史只在達門檻之後才讀**：達門檻時另 spawn 一個任務，讀 `herdr_maintenance` 那一列與關窗時寫的 `supervisor_notes`
//! （`herdr_maintenance_end`／`_expired`／`_superseded`，同一個交易刪窗口，所以窗口已被刪也查得到），把轉移當下（`opened_at <= at < 關窗`）
//! 在維護中的那幾筆從名單與記帳裡拿掉，剩下仍達門檻才推。純 SELECT，**不呼叫** `herdr_maintenance::active`（它在窗口過期時會同步
//! 退休子 agent、寫交易、發事件）。過期還沒被關的窗口不算維護；讀不到維護狀態當在維護。

use crate::lifecycle::ExitProof;
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
/// 歷史留多久：窗口加上記帳的延遲還要有餘裕（事件時間比記到帳的時間早）。
const RETAIN: Duration = Duration::from_secs(60);
/// 每台 host 最多記幾筆（窗口內的上限，防記憶體長大）。
const MAX_RECORDS: usize = 256;
/// 同時最多幾個等著查 bot 的任務。
const MAX_PENDING: usize = 256;
/// `dead_panes::sweep` 收 run 用的原因。
const SWEEP_REASON: &str = "pane gone";
/// 關窗的稽核 note（同一個交易刪窗口，所以現存那一列沒了，這裡還查得到）。
const CLOSE_NOTES: [&str; 3] = ["herdr_maintenance_end", "herdr_maintenance_expired", "herdr_maintenance_superseded"];
/// 一次最多讀幾筆關窗 note。
const MAX_CLOSE_NOTES: i64 = 32;

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
///
/// 照**事件時間**（`seen`）判斷，不照記到帳的時間：任一段長 [`WINDOW`] 的時間裡有 ≥ [`THRESHOLD`] 顆不同的 bot、而且包含這一筆，才算一波。
/// 記帳會晚到（查 bot 的任務排程），所以第 0／14／28 秒各掉一顆、第 31 秒才記到第三顆時，前兩顆還留著（歷史留 [`RETAIN`]）。
/// 同一顆 bot 只留最新的一筆（重開再掉不加數）；比保留期還舊的這一筆不記。
pub(crate) fn record_at(key: &str, gone: Gone, now: Instant) -> Option<Vec<Gone>> {
    if now.saturating_duration_since(gone.seen) >= RETAIN {
        return None;
    }
    let mut table = table().lock().unwrap_or_else(|e| e.into_inner());
    let st = table.entry(key.to_string()).or_default();
    st.exits.retain(|g| now.saturating_duration_since(g.seen) < RETAIN);
    if st.exits.iter().any(|g| g.bot_id == gone.bot_id && g.seen >= gone.seen) {
        return None;
    }
    st.exits.retain(|g| g.bot_id != gone.bot_id);
    let seen = gone.seen;
    st.exits.push(gone);
    st.exits.sort_by_key(|g| g.seen);
    if st.exits.len() > MAX_RECORDS {
        let excess = st.exits.len() - MAX_RECORDS;
        st.exits.drain(..excess);
    }
    if st.quiet_until.is_some_and(|q| now < q) {
        return None;
    }
    st.quiet_until = None;
    // 以每一筆為起點的 WINDOW 長區間，要包含這一筆而且有 ≥ THRESHOLD 筆。
    let wave = (0..st.exits.len()).find_map(|i| {
        let start = st.exits[i].seen;
        let span: Vec<&Gone> = st.exits[i..].iter().take_while(|g| g.seen.saturating_duration_since(start) < WINDOW).collect();
        (span.len() >= THRESHOLD && seen >= start && seen.saturating_duration_since(start) < WINDOW).then(|| span.into_iter().cloned().collect::<Vec<_>>())
    })?;
    st.quiet_until = Some(now + COOLDOWN);
    Some(wave)
}

/// 把這幾筆從記帳拿掉（轉移當下在維護中的）。
fn forget(key: &str, gone: &[Gone]) {
    if let Some(st) = table().lock().unwrap_or_else(|e| e.into_inner()).get_mut(key) {
        st.exits.retain(|g| !gone.iter().any(|d| d.run_id == g.run_id));
    }
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

/// wrapper 的用法：`lifecycle::mark_run_exited_proved` → `after_exit`。這一條路真的寫下 exited、轉移前不是 `stopping`、原因在清單內才記。
pub(crate) fn after_exit(app: &Arc<App>, run_id: &str, reason: &str, proof: Option<ExitProof>) {
    if let Some(proof) = proof.filter(|p| !p.from_stopping && COUNTED_REASONS.contains(&reason)) {
        note_later(app, vec![(run_id.to_string(), proof.at)]);
    }
}

/// 三個步驟包成一個呼叫（`RelayWatchServices` 的 wrapper 用）。回傳值就是原本的回傳值。
pub(crate) async fn mark_run_exited(app: &Arc<App>, run_id: &str, reason: &str) -> crate::lc_error::RunExit {
    let (exit, proof) = crate::lifecycle::mark_run_exited_proved(app, run_id, reason).await;
    after_exit(app, run_id, reason, proof);
    exit
}

/// `dead_panes::sweep_proved` 回傳的（只有這一次自己寫下 exited 的 run，各帶自己的證據）。
pub(crate) fn after_sweep(app: &Arc<App>, swept: Vec<(String, ExitProof)>) {
    let runs: Vec<_> = swept.into_iter().filter(|(_, p)| !p.from_stopping).map(|(id, p)| (id, p.at)).collect();
    if !runs.is_empty() {
        note_later(app, runs);
    }
}

static PENDING: AtomicUsize = AtomicUsize::new(0);

/// 等著查 bot 的任務的名額；任務結束（含 panic 展開）就還。
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

/// spawn 一個任務查 bot 再記帳；收尾那一邊不 await、不讀 DB。
fn note_later(app: &Arc<App>, runs: Vec<(String, String)>) {
    let Some(slot) = Slot::take() else {
        tracing::debug!("gone burst: too many exits waiting to be recorded; skipped");
        return;
    };
    let app = app.clone();
    crate::background_loop::spawn_named(
        &app.background_tasks.clone(),
        "gone burst note",
        Box::pin(async move {
            let _slot = slot;
            for (run_id, at) in runs {
                let bot: Option<(String,)> = sqlx::query_as("SELECT bot_id FROM runs WHERE id = ?").bind(&run_id).fetch_optional(&app.db).await.ok().flatten();
                let Some((bot_id,)) = bot else { continue };
                if let Some(who) = who(&app, &bot_id).await {
                    note(&app, who, run_id, at);
                }
            }
        }),
    );
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
            // 達門檻才讀維護歷史（不在記帳的路徑上）。
            let Some(wave) = outside_maintenance(&app, &key, wave).await else { return };
            if let Err(e) = push(&app, &host, &wave).await {
                clear_quiet(&key);
                tracing::error!(host = %host, error = ?e, "could not queue the agents_gone_burst ops_alert");
            }
        }),
    );
}

/// 維護窗口：`[opened_at, 結束)`，結束是 `until` 與關窗時間較早的那個。
type Span = (String, String);

/// 現存的窗口那一列，加上 `since` 之後才關掉的幾扇（關窗 note）。純讀、成本有界（一列加最多 [`MAX_CLOSE_NOTES`] 筆 note）；
/// 讀不到回 `None`。
async fn maintenance_spans(app: &Arc<App>, since: &str) -> Option<Vec<Span>> {
    let mut spans = Vec::new();
    if let Some(w) = crate::herdr_maintenance::row(&app.db).await.ok()? {
        spans.push((w.opened_at, w.until));
    }
    let notes: Vec<(String, String)> = sqlx::query_as(
        "SELECT body, created_at FROM supervisor_notes WHERE kind IN (?, ?, ?) AND created_at >= ? ORDER BY created_at LIMIT ?",
    )
    .bind(CLOSE_NOTES[0])
    .bind(CLOSE_NOTES[1])
    .bind(CLOSE_NOTES[2])
    .bind(since)
    .bind(MAX_CLOSE_NOTES)
    .fetch_all(&app.db)
    .await
    .ok()?;
    for (body, closed_at) in notes {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let (Some(opened_at), Some(until)) = (v["opened_at"].as_str(), v["until"].as_str()) else { continue };
        let end = if crate::db::cmp_ts(&closed_at, until).is_lt() { closed_at } else { until.to_string() };
        spans.push((opened_at.to_string(), end));
    }
    Some(spans)
}

fn in_span(spans: &[Span], at: &str) -> bool {
    spans.iter().any(|(from, to)| crate::db::cmp_ts(from, at).is_le() && crate::db::cmp_ts(at, to).is_lt())
}

/// 把轉移當下在維護中的幾筆拿掉（名單與記帳都拿掉，維護期間的退出不進窗）；剩下仍達門檻才回名單，否則解除冷卻、回 `None`。
/// 讀不到維護歷史當全部在維護。
async fn outside_maintenance(app: &Arc<App>, key: &str, wave: Vec<Gone>) -> Option<Vec<Gone>> {
    let earliest = wave.iter().map(|g| g.at.as_str()).min().unwrap_or_default().to_string();
    let spans = maintenance_spans(app, &earliest).await;
    let (kept, dropped): (Vec<Gone>, Vec<Gone>) = wave.into_iter().partition(|g| spans.as_deref().is_some_and(|s| !in_span(s, &g.at)));
    if !dropped.is_empty() {
        forget(key, &dropped);
    }
    if kept.len() < THRESHOLD {
        clear_quiet(key);
        return None;
    }
    Some(kept)
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
    fn exits_at_0_14_and_28_seconds_are_one_wave_even_when_confirmed_late() {
        let k = "t-late";
        let t = Instant::now();
        let at = |s: u64| t + Duration::from_secs(s);
        // 事件時間 0／14／28，第三顆到第 31 秒才確認到：前兩顆還在歷史裡。
        assert!(record_at(k, gone("a", at(0)), at(3)).is_none());
        assert!(record_at(k, gone("b", at(14)), at(17)).is_none());
        let wave = record_at(k, gone("c", at(28)), at(31)).expect("28 秒內三顆");
        assert_eq!(wave.len(), 3);
    }

    #[test]
    fn exits_more_than_a_window_apart_by_event_time_are_not_one_wave() {
        let k = "t-apart";
        let t = Instant::now();
        let at = |s: u64| t + Duration::from_secs(s);
        record_at(k, gone("a", at(0)), at(3));
        record_at(k, gone("b", at(14)), at(17));
        assert!(record_at(k, gone("c", at(31)), at(34)).is_none(), "a 與 c 相隔 31 秒，b、c 只有兩顆");
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
        tokio::time::sleep(Duration::from_millis(400)).await;
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

    /// `dead_panes::sweep` 把 `AlreadyEnded` 也放進它的回傳：使用者 stop 在讀完之後先完成（兩步 CAS 都輸）、
    /// 或讀完之後被 stop 改成 `stopping`，都不歸掃描。走真的 sweep wrapper。
    #[tokio::test]
    async fn the_sweep_wrapper_does_not_count_runs_another_path_or_a_stop_ended() {
        use crate::lifecycle::s6_ports::RelayWatchServices;
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        for (n, state) in [(0, "stopped"), (1, "stopping")] {
            let (db, id) = (env.app.db.clone(), runs[n].1.clone());
            crate::lifecycle::race_point::arm("mark_run_exited_after_read", &runs[n].1, move || async move {
                sqlx::query("UPDATE runs SET state=?, ended_at=? WHERE id=?").bind(state).bind(db::now()).bind(&id).execute(&db).await.unwrap();
            });
        }
        let swept = env.app.sweep_dead_panes().await;
        assert_eq!(swept.len(), 2, "只回有證據的：b（stopping 轉過來的，有證據但不計）與 c；a 兩步都輸，沒有: {swept:?}");
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty(), "a 兩步都輸、b 是從 stopping 轉的，只剩 c 一顆");
    }

    #[tokio::test]
    async fn sweep_exits_are_stamped_when_they_happened_not_when_the_sweep_returned() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        let swept: Vec<(String, ExitProof)> = runs
            .iter()
            .enumerate()
            // 一顆在一分鐘前、其餘剛剛：窗是 30 秒，不能因為同一輪掃描一起回傳就算同一波。
            .map(|(n, (_, id))| (id.clone(), ExitProof { from_stopping: false, at: db::iso_in(if n == 0 { -60 } else { 0 }) }))
            .collect();
        after_sweep(&env.app, swept);
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty(), "一分鐘前那顆在窗外，只剩 2 顆");
    }

    /// 結束前讀到 `running`、CAS 之前 stop／restart 才寫下 `stopping`：CAS 自己回報來源是 stopping，
    /// 不需要等 stop 改標（restart 本來就不改標，stop 也可能慢很久）。
    #[tokio::test]
    async fn a_stop_or_restart_that_lands_between_the_read_and_the_exit_is_not_counted_whether_or_not_it_relabels() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        for (n, (_, run)) in runs.iter().enumerate() {
            let (db, id) = (env.app.db.clone(), run.clone());
            crate::lifecycle::race_point::arm("mark_run_exited_after_read", run, move || async move {
                sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&id).execute(&db).await.unwrap();
            });
            assert_eq!(env.app.mark_run_exited(run, "pane exited").await, RunExit::Recorded);
            if n == 0 {
                // stop：之後改標回 stopped；其餘（restart）一直是 exited。
                crate::lifecycle::run_state::relabel(&env.app.db, run, "exited", "stopped").await.unwrap();
            }
        }
        quiet_period().await;
        assert!(alerts(&env.app).await.is_empty());
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
        let w = crate::runners::herdr_maintenance::open_as(&env.app, 30, "patrol", "test").await.unwrap().unwrap();
        for (_, run) in &during {
            env.app.mark_run_exited(run, "agent not found during reconcile").await;
        }
        // 窗口在背景確認讀到之前就被關掉（列已刪）：證據要從關窗的稽核 note 找回來。
        crate::runners::herdr_maintenance::close_as(&env.app, &w, "patrol", None).await.unwrap();
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

    /// 窗口在退出**之後**才開、又關：退出當下沒有在維護，不能因為歷史裡有這一扇就不算。
    #[tokio::test]
    async fn a_window_opened_and_closed_after_the_exits_does_not_hide_them() {
        let env = tt::env().await;
        let runs = running(&env, &["a", "b", "c"]).await;
        for (_, run) in &runs {
            env.app.mark_run_exited(run, "pane exited").await;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        let w = crate::runners::herdr_maintenance::open_as(&env.app, 30, "patrol", "test").await.unwrap().unwrap();
        crate::runners::herdr_maintenance::close_as(&env.app, &w, "patrol", None).await.unwrap();
        assert!(eventually!(alerts(&env.app).await.len() == 1));
    }
}
