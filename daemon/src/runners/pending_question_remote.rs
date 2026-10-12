//! 遠端主機的 claude bot 的 `GET /pending-question`（issue #1476）：對話檔在那台機器上，經 §12.5b 已有的
//! 那條 `sh`／ssh（`lifecycle::claude_child_log::pending_tail`）只讀檔尾，不另開通道。
//!
//! 網頁每 5 秒輪詢，「需要回應」的視窗與面板各一份，所以這裡有兩道保護：
//! * **逾時**：整段（含查遠端 `$HOME`）最多 [`REMOTE_READ_TIMEOUT`]，超過當沒讀到（`None`），請求不會卡在 ssh 上；
//! * **節流**：同一個 run generation、host connection 與 live session 在 [`FRESH_FOR`] 內不再打遠端，直接用上一次的結果（讀失敗也算，不會在主機不通時每次都重試）。
//!   同時進來的請求排在同一把鎖後面，只有第一個真的去讀。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::db;
use crate::state::App;

/// 一次遠端讀取（含查 `$HOME`）最多等多久。
pub const REMOTE_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 同一個 run 的結果多久內重複使用。比網頁的輪詢間隔（5 秒）短一點：每一輪都看得到新的提問，但雙視窗不會各打一次。
pub const FRESH_FOR: Duration = Duration::from_secs(4);
/// 快取最多留幾個 run；超過就丟掉沒人在用而且過期的。
const MAX_RUNS: usize = 256;

type Slot = Arc<tokio::sync::Mutex<Option<(Instant, Option<String>)>>>;

/// 每個 run 一格；格子本身是一把鎖，鎖住的期間就是「有人正在讀」。
#[derive(Default)]
pub struct TailCache {
    slots: Mutex<HashMap<String, Slot>>,
}

impl TailCache {
    fn slot(&self, key: &str, now: Instant) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots.len() >= MAX_RUNS && !slots.contains_key(key) {
            // 沒人握著（只有 map 自己一個參照）、而且過期的才丟；正在讀的、剛讀完的留著。
            slots.retain(|_, s| Arc::strong_count(s) > 1 || s.try_lock().map(|g| g.as_ref().is_some_and(|(at, _)| now.duration_since(*at) < FRESH_FOR)).unwrap_or(true));
        }
        slots.entry(key.to_string()).or_default().clone()
    }

    /// 新鮮的結果直接回；否則 `fetch` 一次（同時進來的排隊，後面的看到前面的結果）。`clock` 讓測試不必真的等。
    /// 第二個值是這次有沒有真的去讀：`false` 表示拿的是快取。
    pub async fn read_through<F, Fut>(&self, key: &str, clock: impl Fn() -> Instant, fetch: F) -> (Option<String>, bool)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Option<String>>,
    {
        let slot = self.slot(key, clock());
        let mut guard = slot.lock().await;
        if let Some((at, value)) = guard.as_ref() {
            if clock().saturating_duration_since(*at) < FRESH_FOR {
                return (value.clone(), false);
            }
        }
        let value = fetch().await;
        *guard = Some((clock(), value.clone()));
        (value, true)
    }
}

fn cache() -> &'static TailCache {
    static CACHE: OnceLock<TailCache> = OnceLock::new();
    CACHE.get_or_init(TailCache::default)
}

/// Cache identity must include the run generation, host fence, pane, and live Claude session.
pub(crate) fn cache_key_parts(run_id: &str, started_at: &str, pane: Option<&str>, host: &str, host_id: &str, host_generation: u64, session: &str) -> String {
    format!("{run_id}\0{started_at}\0{}\0{host}\0{host_id}\0{host_generation}\0{session}", pane.unwrap_or(""))
}

// A per-run clock lets composition tests keep the polling cache fixed while
// independently changing the remote transcript. Production always uses Instant::now.
fn cache_now(_run_id: &str) -> Instant {
    #[cfg(test)]
    if let Some(at) = test_clocks().lock().unwrap().get(_run_id).copied() {
        return at;
    }
    Instant::now()
}

#[cfg(test)]
fn test_clocks() -> &'static Mutex<HashMap<String, Instant>> {
    static CLOCKS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    CLOCKS.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) struct FixedClock(String);

#[cfg(test)]
impl Drop for FixedClock {
    fn drop(&mut self) {
        test_clocks().lock().unwrap().remove(&self.0);
    }
}

#[cfg(test)]
pub(crate) fn fixed_clock(run_id: &str) -> FixedClock {
    test_clocks().lock().unwrap().insert(run_id.to_string(), Instant::now());
    FixedClock(run_id.to_string())
}

/// 逾時就是 `None`；讀失敗也是 `None`（只記 debug，網頁照舊用畫面）。
pub async fn bounded<Fut>(timeout: Duration, read: Fut) -> Option<String>
where
    Fut: Future<Output = anyhow::Result<Option<String>>>,
{
    match tokio::time::timeout(timeout, read).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "pending-question: remote transcript read failed");
            None
        }
        Err(_) => {
            tracing::debug!("pending-question: remote transcript read timed out");
            None
        }
    }
}

/// 遠端 run 的對話檔尾段（最後 [`crate::pending_question::TAIL_BYTES`]）；讀不到、路徑不在允許範圍、逾時＝`None`。
/// 第二個值是 `true` 表示這次是現讀的（不是快取），呼叫端才能拿它當有效性檢查的證據。
/// live session 的任一道檢查失敗（查不到、形狀不對、讀前讀後換了 session）一律回 `(None, false)`。
pub async fn tail(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str) -> (Option<String>, bool) {
    if !run_is_current(app, bot, run, host).await {
        return (None, false);
    }
    let Some(fence) = app.hosts.fence(host).await else { return (None, false) };
    let Some(session) = crate::runners::claude_child_log::herdr_session(app, run)
        .await
        .filter(|sid| crate::lifecycle::grok_transcript::valid_session_id(sid))
    else {
        return (None, false);
    };
    if !app.hosts.is_current(&fence).await || !run_is_current(app, bot, run, host).await {
        return (None, false);
    }
    let key = cache_key_parts(
        &run.id,
        &run.started_at,
        run.pane_id.as_deref(),
        host,
        &fence.authority_scope(),
        fence.generation(),
        &session,
    );
    // 讀完之後發現 session 或 run 已經換了：這次的檔尾不是這個 session 的，不能當證據，也不能讓呼叫端沿用。
    let moved = AtomicBool::new(false);
    let (value, fresh) = cache()
        .read_through(&key, || cache_now(&run.id), || async {
            let tail = bounded(
                REMOTE_READ_TIMEOUT,
                crate::lifecycle::claude_child_log::pending_tail_for_session(app, bot, run, host, &session, crate::pending_question::TAIL_BYTES),
            )
            .await;
            if !app.hosts.is_current(&fence).await || !run_is_current(app, bot, run, host).await {
                moved.store(true, Ordering::SeqCst);
                return None;
            }
            let live = crate::runners::claude_child_log::herdr_session(app, run)
                .await
                .filter(|sid| crate::lifecycle::grok_transcript::valid_session_id(sid));
            if live.as_deref() != Some(session.as_str()) {
                moved.store(true, Ordering::SeqCst);
                return None;
            }
            tail
        })
        .await;
    if moved.load(Ordering::SeqCst) {
        return (None, false);
    }
    (value, fresh)
}

async fn run_is_current(app: &Arc<App>, bot: &db::Bot, run: &db::Run, host: &str) -> bool {
    if !matches!(run.state.as_str(), "starting" | "running") {
        return false;
    }
    let Ok(Some(current)) = db::run(&app.db, &run.id).await else { return false };
    if current.bot_id != bot.id
        || current.state != run.state
        || current.started_at != run.started_at
        || current.pane_id != run.pane_id
    {
        return false;
    }
    matches!(db::bot_host(&app.db, &bot.id).await, Ok(current_host) if current_host == host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 不碰系統時鐘：`Instant` 只是一個起點加上我們給的位移。
    fn clock_at(base: Instant, offset: &Arc<Mutex<Duration>>) -> impl Fn() -> Instant {
        let offset = offset.clone();
        move || base + *offset.lock().unwrap()
    }

    async fn read(cache: &TailCache, key: &str, base: Instant, offset: &Arc<Mutex<Duration>>, reads: &Arc<AtomicUsize>, value: &'static str) -> Option<String> {
        let reads = reads.clone();
        cache
            .read_through(key, clock_at(base, offset), move || async move {
                reads.fetch_add(1, Ordering::SeqCst);
                Some(value.to_string())
            })
            .await
            .0
    }

    /// 連續請求不會每次都打遠端：新鮮期間內用上一次的結果，過了才再讀；各 run 各一格。
    #[tokio::test]
    async fn consecutive_requests_reuse_the_last_read_until_it_is_stale() {
        let cache = TailCache::default();
        let (base, offset) = (Instant::now(), Arc::new(Mutex::new(Duration::ZERO)));
        let reads = Arc::new(AtomicUsize::new(0));
        assert_eq!(read(&cache, "run-a", base, &offset, &reads, "v1").await.as_deref(), Some("v1"));
        *offset.lock().unwrap() = FRESH_FOR - Duration::from_millis(1);
        assert_eq!(read(&cache, "run-a", base, &offset, &reads, "v2").await.as_deref(), Some("v1"), "新鮮期間內不再打遠端");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(read(&cache, "run-b", base, &offset, &reads, "b1").await.as_deref(), Some("b1"), "別的 run 各自一格");
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        *offset.lock().unwrap() = FRESH_FOR;
        assert_eq!(read(&cache, "run-a", base, &offset, &reads, "v2").await.as_deref(), Some("v2"), "過期了才再讀");
        assert_eq!(reads.load(Ordering::SeqCst), 3);
    }

    /// 讀失敗（`None`）也算一次結果：主機不通時不會每個請求都重試。
    #[tokio::test]
    async fn a_failed_read_is_not_retried_within_the_fresh_window() {
        let cache = TailCache::default();
        let (base, offset) = (Instant::now(), Arc::new(Mutex::new(Duration::ZERO)));
        let reads = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let reads = reads.clone();
            let v = cache
                .read_through("run-x", clock_at(base, &offset), move || async move {
                    reads.fetch_add(1, Ordering::SeqCst);
                    None
                })
                .await
                .0;
            assert_eq!(v, None);
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    /// 同時進來的請求只有一個真的去讀，其他的等它、用它的結果。
    #[tokio::test]
    async fn concurrent_requests_share_one_read() {
        let cache = Arc::new(TailCache::default());
        let base = Instant::now();
        let reads = Arc::new(AtomicUsize::new(0));
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let gate = Arc::new(tokio::sync::Mutex::new(Some(gate)));
        let first = {
            let (cache, reads, gate) = (cache.clone(), reads.clone(), gate.clone());
            tokio::spawn(async move {
                cache
                    .read_through("run-c", move || base, move || async move {
                        reads.fetch_add(1, Ordering::SeqCst);
                        let g = gate.lock().await.take().unwrap();
                        let _ = g.await;
                        Some("tail".to_string())
                    })
                    .await
            })
        };
        // 第一個在 fetch 裡等 gate 時，第二個進來排在同一格的鎖後面。
        while reads.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let second = {
            let (cache, reads) = (cache.clone(), reads.clone());
            tokio::spawn(async move {
                cache
                    .read_through("run-c", move || base, move || async move {
                        reads.fetch_add(1, Ordering::SeqCst);
                        Some("second".to_string())
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        release.send(()).unwrap();
        assert_eq!(first.await.unwrap().0.as_deref(), Some("tail"));
        assert_eq!(second.await.unwrap().0.as_deref(), Some("tail"));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    /// 遠端慢到逾時、或讀失敗：`None`，不是錯誤也不會卡住。（暫停時鐘：逾時用虛擬時間，不真的等。）
    #[tokio::test(start_paused = true)]
    async fn a_slow_or_failing_remote_read_is_none() {
        assert_eq!(bounded(REMOTE_READ_TIMEOUT, std::future::pending()).await, None);
        assert_eq!(bounded(REMOTE_READ_TIMEOUT, async { Err(anyhow::anyhow!("ssh: connection reset")) }).await, None);
        assert_eq!(bounded(REMOTE_READ_TIMEOUT, async { Ok(Some("x".to_string())) }).await.as_deref(), Some("x"));
    }
}
