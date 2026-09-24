//! controller tick 每一段與全域鎖的耗時（issue #473 第一步）。
//!
//! #473 提了兩個設計取捨（tick 沒有總時限、握著全域鎖做 herdr 送出），但兩條都沒有數字：
//! 沒有 herdr 變慢時的 tick 時長樣本，也沒有 API 等鎖的分佈。協調者裁示先量測——
//! **這個模組只記錄，不改任何行為、順序或鎖的範圍**。
//!
//! 三件事：
//!
//! 1. **滾動統計**：每個 key 留最近 [`WINDOW`] 的樣本，`/api/supervisor/health` 的 `timing` 給
//!    p50／p95／max（樣本本身不對外，只有統計）。窗口之外再加一道 [`MAX_SAMPLES`] 上限——鎖一秒可能
//!    被拿好幾次，只靠時間窗口的話記憶體會跟著流量走。
//! 2. **只記超標的**：單段 > [`SLOW_SEGMENT`]、整拍 > [`SLOW_TICK`]、等鎖 > [`SLOW_LOCK_WAIT`] 才寫一行
//!    `daemon.log`。正常的一拍一個字都不寫——每拍都寫等於把 log 變成沒人看的噪音。
//! 3. **開銷可忽略**：記一筆是兩次 `Instant::now()` ＋ 一次 `Vec::push`（`std::sync::Mutex`，不跨 await）。
//!    **不多做任何 herdr RPC**；連「現在有幾顆 bot」都只在**真的要寫那一行 log 時**才去查一次 DB。

use crate::state::App;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 統計只看最近這麼久。
pub const WINDOW: Duration = Duration::from_secs(60 * 60);
/// 每個 key 最多留幾筆樣本（窗口之外的第二道上限，見模組說明）。
const MAX_SAMPLES: usize = 2_000;

/// tick 單段超過這麼久才寫 log。
pub const SLOW_SEGMENT: Duration = Duration::from_secs(2);
/// 整拍超過這麼久才寫 log（＝`controller::TICK`，一拍做不完就開始落後）。
pub const SLOW_TICK: Duration = Duration::from_secs(10);
/// 等鎖超過這麼久才寫 log。
pub const SLOW_LOCK_WAIT: Duration = Duration::from_secs(1);

/// 整拍的 key（`_` 開頭，排序時跟各段分得開）。
pub const TICK_TOTAL: &str = "tick:_total";

fn samples() -> &'static Mutex<HashMap<String, VecDeque<(Instant, u64)>>> {
    static S: OnceLock<Mutex<HashMap<String, VecDeque<(Instant, u64)>>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// 記一筆。鎖只在這幾行裡，不跨 await；毒掉的鎖照用（量測不該讓功能掛掉）。
pub fn record(key: &str, took: Duration) {
    let now = Instant::now();
    let mut g = samples().lock().unwrap_or_else(|e| e.into_inner());
    let q = g.entry(key.to_string()).or_default();
    q.push_back((now, took.as_millis().min(u64::MAX as u128) as u64));
    // 先丟窗口外的，再丟超量的最舊那幾筆。
    while q.front().is_some_and(|(at, _)| now.duration_since(*at) > WINDOW) {
        q.pop_front();
    }
    while q.len() > MAX_SAMPLES {
        q.pop_front();
    }
}

/// 排序過的樣本取百分位；`p` 是 0..=100。空的回 0。
fn pct(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // 最近鄰：p95 要的是「95% 的樣本不比它大」，不是內插出一個沒發生過的數字。
    let idx = (sorted.len() * p).div_ceil(100).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

/// `{key: {count, p50_ms, p95_ms, max_ms}}`，只給統計不給樣本。
///
/// **握著 mutex 的只有「複製出來」那一段**，排序與算百分位都在鎖外面（i263 審 #473）：這支掛在
/// `GET /api/supervisor/health` 上、UI 會高頻輪詢，而 60 幾個 key × 最多 [`MAX_SAMPLES`] 筆的排序
/// 若在鎖內做，每一次輪詢都會讓當下要記帳的人（包括正在放全域鎖的那一位）排在它後面。
/// 複製的成本是一次 memcpy，比排序便宜得多，而且不擋別人寫。
pub fn snapshot() -> Value {
    let now = Instant::now();
    // 鎖內只做這一件事：把窗口內的數字抄出來。
    let copied: Vec<(String, Vec<u64>)> = {
        let g = samples().lock().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .map(|(key, q)| {
                let ms: Vec<u64> = q.iter().filter(|(at, _)| now.duration_since(*at) <= WINDOW).map(|(_, ms)| *ms).collect();
                (key.clone(), ms)
            })
            .collect()
    };
    let mut out = serde_json::Map::new();
    for (key, mut ms) in copied {
        if ms.is_empty() {
            continue;
        }
        ms.sort_unstable();
        out.insert(
            key,
            json!({
                "count": ms.len(),
                "p50_ms": pct(&ms, 50),
                "p95_ms": pct(&ms, 95),
                "max_ms": *ms.last().unwrap_or(&0),
            }),
        );
    }
    json!({"window_secs": WINDOW.as_secs(), "stats": Value::Object(out)})
}

/// 要寫 log 了才去問「現在有幾顆 bot」——正常的一拍不會走到這裡，所以這一次查詢不算在常態開銷裡。
/// 讀不到就回 `-1`（量測讀不到不該變成錯誤）。
async fn bots_now(app: &Arc<App>) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM bots WHERE deleted_at IS NULL")
        .fetch_one(&app.db)
        .await
        .unwrap_or(-1)
}

/// 包一段 tick：原樣 `await` 那個 future（順序、鎖的範圍都不變），只在前後各取一次時間。
/// 超過 [`SLOW_SEGMENT`] 才寫一行 warn。
pub async fn seg<T>(app: &Arc<App>, name: &'static str, fut: impl std::future::Future<Output = T>) -> T {
    let started = Instant::now();
    let out = fut.await;
    note_segment(app, name, started.elapsed()).await;
    out
}

/// [`seg`] 量完之後的那一半：進統計，超標才寫 log。時間由呼叫端給，所以門檻測得動
/// （`tokio::time::pause` 只停得了 tokio 的時鐘，`std::time::Instant` 照走）。
pub async fn note_segment(app: &Arc<App>, name: &str, took: Duration) {
    record(&format!("tick:{name}"), took);
    if took >= SLOW_SEGMENT {
        let bots = bots_now(app).await;
        tracing::warn!(segment = name, bots, took_ms = took.as_millis() as u64, "supervisor tick segment was slow");
    }
}

/// 一拍做完了。超過 [`SLOW_TICK`]（＝心跳間隔）才寫：那表示這一拍已經開始落後。
pub async fn note_tick(app: &Arc<App>, took: Duration) {
    record(TICK_TOTAL, took);
    if took >= SLOW_TICK {
        let bots = bots_now(app).await;
        tracing::warn!(
            bots,
            took_ms = took.as_millis() as u64,
            budget_ms = SLOW_TICK.as_millis() as u64,
            "supervisor tick took longer than one heartbeat"
        );
    }
}

/// 等到全域鎖了。`at` 是**哪個拿鎖點**（`lock()` 用 `#[track_caller]` 抓的）。
pub fn note_lock_wait(at: &'static std::panic::Location<'static>, waited: Duration) {
    record(&format!("lock:wait:{}", site(at)), waited);
    if waited >= SLOW_LOCK_WAIT {
        tracing::warn!(site = %site(at), waited_ms = waited.as_millis() as u64, "waited a long time for the supervisor lock");
    }
}

/// 放掉全域鎖了。持有時間本身不寫 log（握久不一定是問題，等的人久才是），只進統計。
pub fn note_lock_hold(at: &'static std::panic::Location<'static>, held: Duration) {
    // 測試探針：讓測試看得到「進到記帳這一步的當下」全域鎖放了沒。正式 build 沒有這段。
    #[cfg(test)]
    {
        let probe = hold_probe().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = probe.as_ref() {
            f(at);
        }
    }
    record(&format!("lock:hold:{}", site(at)), held);
}

/// 測試用探針：`note_lock_hold` 進來時先叫它一次。
///
/// 用它而不是「佔住樣本表再看鎖放了沒」（原本的寫法，i204 審 #473）：那張表是行程共用的，佔住它會讓
/// **平行跑的其他測試**卡在 `record()` 上，整樹跑就會偶發紅。探針只在自己那個拿鎖點觸發（呼叫端比對
/// `at`），其餘的 drop 進來看一眼就走，誰也不擋。
#[cfg(test)]
type HoldProbe = Box<dyn Fn(&'static std::panic::Location<'static>) + Send + Sync>;

#[cfg(test)]
fn hold_probe() -> &'static Mutex<Option<HoldProbe>> {
    static P: OnceLock<Mutex<Option<HoldProbe>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// 裝上探針；回傳的守衛在 drop 時拆掉，測試失敗也不會留給別人。
#[cfg(test)]
pub(crate) fn probe_lock_hold(f: HoldProbe) -> ProbeGuard {
    *hold_probe().lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    ProbeGuard
}

#[cfg(test)]
pub(crate) struct ProbeGuard;

#[cfg(test)]
impl Drop for ProbeGuard {
    fn drop(&mut self) {
        *hold_probe().lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// key 只用**檔名**，不帶行號（i204 審 #473）：行號一改版就變，跨版本就比不了 p95——而這張表存在的
/// 理由正是跨版本比。代價是同一個檔裡的多個拿鎖點會合成一筆分佈（`supervisor/api.rs` 有八個），
/// 但 #473 要回答的是「controller 的 dispatch 跟 API handler 誰在等」，檔名這一層剛好分得出來。
fn site(at: &'static std::panic::Location<'static>) -> String {
    at.file().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 這張表是**行程共用**的，而且測試是平行跑的：所以每個測試只用自己的 key、**不清表**
    /// （清表會洗掉別的測試正在量的東西，就是典型的整樹偶發紅）。
    fn stats(key: &str) -> Value {
        snapshot()["stats"][key].clone()
    }

    /// 「沒寫那一行」而不是「什麼都沒寫」：同一條執行緒上 sqlx 之類也可能寫 DEBUG，
    /// 斷言整個 buffer 是空的會因為不相干的輸出而紅。
    fn logged(text: &str, needle: &str) -> bool {
        text.lines().any(|l| l.contains(needle))
    }

    /// 百分位取最近鄰：p95 要的是真的發生過的那個數字。
    #[test]
    fn percentiles_pick_a_sample_that_really_happened() {
        let one_to_hundred: Vec<u64> = (1..=100).collect();
        assert_eq!(pct(&one_to_hundred, 50), 50);
        assert_eq!(pct(&one_to_hundred, 95), 95);
        assert_eq!(pct(&one_to_hundred, 100), 100);
        assert_eq!(pct(&[7], 50), 7);
        assert_eq!(pct(&[7], 95), 7);
        assert_eq!(pct(&[], 50), 0, "沒有樣本回 0，不是 panic");
        // 樣本數不整除時 p95 仍要落在實際樣本上（不內插）。
        assert_eq!(pct(&[1, 2, 3], 95), 3);
        assert_eq!(pct(&[1, 2, 3], 50), 2);
    }

    /// 統計是對的，而且只給統計不給樣本。
    #[test]
    fn the_snapshot_reports_p50_p95_and_max() {
        for ms in [10u64, 20, 30, 40, 1_000] {
            record("tick:demo", Duration::from_millis(ms));
        }
        let s = stats("tick:demo");
        assert_eq!(s["count"], json!(5));
        assert_eq!(s["p50_ms"], json!(30));
        assert_eq!(s["p95_ms"], json!(1_000));
        assert_eq!(s["max_ms"], json!(1_000));
        assert!(s.get("samples").is_none(), "只給統計：{s}");
        assert_eq!(snapshot()["window_secs"], json!(3_600));
    }

    /// 每個 key 各自統計，互不混到一起。
    #[test]
    fn keys_are_counted_separately() {
        record("lock:wait:a.rs:1", Duration::from_millis(5));
        record("lock:wait:a.rs:1", Duration::from_millis(15));
        record("lock:hold:a.rs:1", Duration::from_millis(900));
        assert_eq!(stats("lock:wait:a.rs:1")["count"], json!(2));
        assert_eq!(stats("lock:wait:a.rs:1")["max_ms"], json!(15));
        assert_eq!(stats("lock:hold:a.rs:1")["max_ms"], json!(900));
    }

    /// 樣本有上限：長時間跑下來記憶體不會跟著流量走。
    #[test]
    fn samples_are_capped_per_key() {
        for i in 0..(MAX_SAMPLES + 500) {
            record("tick:flood", Duration::from_millis(i as u64));
        }
        let s = stats("tick:flood");
        assert_eq!(s["count"], json!(MAX_SAMPLES), "超量時丟最舊的");
        // 丟掉的是最舊（最小）那幾筆，最大值還在。
        assert_eq!(s["max_ms"], json!((MAX_SAMPLES + 499) as u64));
    }

    /// 門檻是常數，不是隨手寫的數字：#473 要的是「單段 >2 秒、整拍 >TICK(10 秒)、等鎖 >1 秒」。
    #[test]
    fn the_thresholds_are_the_ones_the_ticket_asked_for() {
        assert_eq!(SLOW_SEGMENT, Duration::from_secs(2));
        assert_eq!(SLOW_TICK, Duration::from_secs(10), "跟 controller::TICK 一樣");
        assert_eq!(SLOW_LOCK_WAIT, Duration::from_secs(1));
    }

    // ── 「只記超標的」：用 `config_audit::capture` 真的去讀寫出來的那幾行 ──

    /// 沒超標的一段**一個字都不寫**，但統計要記到；超標才寫一行，而且帶段名、bot 數、耗時。
    #[tokio::test(flavor = "current_thread")]
    async fn a_segment_is_only_logged_when_it_goes_over_the_budget() {
        let e = crate::testing::env().await;
        let (logs, _guard) = crate::config_audit::capture::start();
        const SLOW_LINE: &str = "supervisor tick segment was slow";

        // 真的跑一次 `seg`：確認它有量、而且快的那一段不寫 log。
        seg(&e.app, "quick", async {}).await;
        assert!(!logged(&logs.text(), SLOW_LINE), "正常的一段不該寫那一行：{:?}", logs.text());
        assert_eq!(stats("tick:quick")["count"], json!(1), "沒寫 log 不代表沒量");

        // 門檻本身用明確的耗時驗（`seg` 量的是 `std::time::Instant`，`tokio::time::pause` 停不了它，
        // 真的睡 2 秒又太慢）：剛好差 1 毫秒不寫，到門檻才寫。
        note_segment(&e.app, "just-under", SLOW_SEGMENT - Duration::from_millis(1)).await;
        assert!(!logged(&logs.text(), SLOW_LINE), "差一毫秒也不寫：{:?}", logs.text());

        note_segment(&e.app, "slow", SLOW_SEGMENT).await;

        let text = logs.text();
        let line = text.lines().find(|l| l.contains(SLOW_LINE)).unwrap_or_else(|| panic!("{text}"));
        assert!(line.contains("segment=\"slow\"") || line.contains("segment=slow"), "要寫哪一段：{line}");
        assert!(line.contains("bots="), "要帶 bot 數：{line}");
        assert!(line.contains("took_ms="), "要帶耗時：{line}");
        assert!(!text.contains("quick") && !text.contains("just-under"), "沒超標的那幾段不該出現：{text}");
        assert_eq!(stats("tick:just-under")["count"], json!(1), "沒超標的照樣進統計");
    }

    /// 整拍同理：沒超過一個心跳就不寫。
    #[tokio::test(flavor = "current_thread")]
    async fn a_tick_is_only_logged_when_it_outruns_one_heartbeat() {
        let e = crate::testing::env().await;
        let (logs, _guard) = crate::config_audit::capture::start();
        const SLOW_LINE: &str = "longer than one heartbeat";
        let before = stats(TICK_TOTAL)["count"].as_u64().unwrap_or(0);

        note_tick(&e.app, SLOW_TICK - Duration::from_millis(1)).await;
        assert!(!logged(&logs.text(), SLOW_LINE), "剛好沒超過就不寫：{:?}", logs.text());

        note_tick(&e.app, SLOW_TICK).await;
        let text = logs.text();
        let line = text.lines().find(|l| l.contains(SLOW_LINE)).unwrap_or_else(|| panic!("{text}"));
        assert!(line.contains("bots=") && line.contains("took_ms="), "{line}");
        assert_eq!(text.lines().filter(|l| l.contains(SLOW_LINE)).count(), 1, "只有超標那一次寫：{text}");
        let after = stats(TICK_TOTAL)["count"].as_u64().unwrap_or(0);
        assert_eq!(after - before, 2, "兩次都要進統計，只有一次進 log");
    }

    /// 等鎖：超過 1 秒才寫，而且要寫出是**哪個拿鎖點**。持有時間只進統計、不寫 log。
    #[tokio::test(flavor = "current_thread")]
    async fn only_a_long_lock_wait_is_logged_and_it_names_the_site() {
        let (logs, _guard) = crate::config_audit::capture::start();
        const SLOW_LINE: &str = "waited a long time for the supervisor lock";
        let at = std::panic::Location::caller();
        // key 現在只有檔名（i204 審 #473），所以同一個檔裡的別條測試（那條 drop 順序的會真的拿一次鎖）
        // 會寫進同一格：比增量，不比絕對值。
        let count = |key: String| snapshot()["stats"][key]["count"].as_u64().unwrap_or(0);
        let wait_key = format!("lock:wait:{}", site(at));
        let before = count(wait_key.clone());

        note_lock_wait(at, SLOW_LOCK_WAIT - Duration::from_millis(1));
        note_lock_hold(at, Duration::from_secs(30));
        assert!(!logged(&logs.text(), SLOW_LINE), "沒等久的不寫；握久本身也不寫：{:?}", logs.text());

        note_lock_wait(at, SLOW_LOCK_WAIT);
        let text = logs.text();
        let line = text.lines().find(|l| l.contains(SLOW_LINE)).unwrap_or_else(|| panic!("{text}"));
        assert!(line.contains(at.file()), "要指出是哪個拿鎖點：{line}");
        assert!(line.contains("waited_ms="), "{line}");

        assert_eq!(count(wait_key) - before, 2, "兩次都要進統計，只有一次進 log");
        // hold 的 30 秒遠比別條測試寫進來的任何一筆大，比 max 仍然穩。
        assert_eq!(snapshot()["stats"][format!("lock:hold:{}", site(at))]["max_ms"], json!(30_000));
    }

    /// i263 審 #473：`OpGuard` 的 drop 必須**先放全域鎖、再記帳**。
    ///
    /// 記帳要拿 timing 這張表的 mutex，而 `snapshot()`（掛在 UI 高頻輪詢的 health 端點）拿同一把。
    /// 順序反過來的話，放鎖就得排在統計後面——等全域鎖的 API 跟著慢，而且量出來的 hold 還不含
    /// 被自己拖長的那一段，對「握著全域鎖做 herdr 送出是不是太久」剛好系統性偏樂觀。
    ///
    /// 驗法是**探針**，不是「佔住樣本表再看鎖放了沒」（i204 審 #473：那會讓平行跑的其他測試卡在
    /// `record()` 上，整樹跑偶發紅）。探針只認自己這個拿鎖點，別人的 drop 進來看一眼就走。
    #[tokio::test]
    async fn dropping_the_guard_frees_the_op_lock_before_it_books_the_hold() {
        let seen: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let mine = std::panic::Location::caller();
        let (sink, at) = (seen.clone(), mine);
        let _probe = probe_lock_hold(Box::new(move |who| {
            // 只管自己那一把：平行跑的別的測試也會 drop guard，進來的不是我的就不看。
            if who.file() != at.file() {
                return;
            }
            *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(crate::supervisor::op_lock_is_free());
        }));

        let guard = crate::supervisor::lock().await;
        assert!(!crate::supervisor::op_lock_is_free(), "前提：鎖在手上");
        drop(guard);

        let observed = *seen.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(observed, Some(true), "記帳那一刻全域鎖必須已經放掉：drop 要先放鎖、再記帳");
    }
}
