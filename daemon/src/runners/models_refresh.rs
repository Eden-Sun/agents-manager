//! 模型清單的背景刷新（issue #1890）。
//!
//! 一般 Bot 的 `GET /api/models` 只讀 10 分鐘內的快取；未命中或過期回 503 `model_cache_miss`，且請求本身不啟動 CLI（#809）。
//! 這裡讓 daemon 在那次 miss 時**自己**排一趟背景刷新（就是 `refresh=1` 的同一條管線）。Bot 仍只拿到 503，
//! 稍後重試就會拿到新快取；Bot 不會因此取得 `refresh=1` 權限（403 `user_only` 照舊）。
//!
//! - single-flight：同一格（host+kind+identity）同時最多一趟，排隊或執行中的重複 miss 不另排。
//! - 限流：同一格每 [`MIN_INTERVAL`] 最多開始一趟（成功或失敗都算）。
//! - 併發：全域最多 [`MAX_CONCURRENT`] 趟，其餘排隊。
//! - 逾時：每趟有上限 [`FETCH_TIMEOUT`]，遠端走 ssh 不會無限等。
//! - 退避：失敗後 60 秒起算、每失敗一次加倍，上限 15 分鐘；退避期間的 miss 不打 ssh 也不跑 CLI。
//! - 範圍：只排 host 存在、且 identity（若有）是該 host 設定過、kind 相符的格子；任意字串不會變成快取鍵。
//!
//! 狀態只在記憶體（同 `role_faults`，SPEC §18.9 的原則）：重啟後歸零，下一次 miss 重新判定。

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{Mutex, Semaphore};

use crate::state::App;

/// 全域同時執行的刷新數。每一趟可能跑一個 CLI 或 ssh，所以不開放無上限。
pub const MAX_CONCURRENT: usize = 2;
/// 同一格兩次開始之間的最短間隔（coordinator 裁示：每組每分鐘最多一次）。
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);
/// 單趟刷新的上限。遠端 ssh 逾時也在這裡收掉，不能讓一趟無限佔住併發名額。
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(90);
const BACKOFF_BASE: Duration = Duration::from_secs(60);
const BACKOFF_CAP: Duration = Duration::from_secs(15 * 60);
/// 排隊或執行中超過這段時間仍沒有結束（任務被砍掉、卡住）就視為過期，允許重新排程。
const STUCK_AFTER: Duration = Duration::from_secs(10 * 60);
/// 排隊／執行中回給呼叫端的重試提示（秒）。
const POLL_HINT: Duration = Duration::from_secs(5);

/// 失敗第 `failures` 次之後的退避：60 秒、120 秒、240 秒…，最多 15 分鐘。
pub fn backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(16);
    BACKOFF_BASE.saturating_mul(1 << shift).min(BACKOFF_CAP)
}

/// 快取格子的鍵，與 `crate::models` 的 `host/kind/identity` 同形。
pub fn cache_key(host: &str, kind: &str, identity: Option<&str>) -> String {
    format!("{host}/{kind}/{}", identity.unwrap_or(""))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshState {
    /// 沒有進行中的刷新，也不在限流或退避內。
    None,
    Queued,
    Running,
    /// 同一格剛開始過一趟，還在 [`MIN_INTERVAL`] 內。
    Waiting,
    /// 上一趟失敗，還在退避內。
    Failed,
}

impl RefreshState {
    fn name(self) -> &'static str {
        match self {
            RefreshState::None => "none",
            RefreshState::Queued => "queued",
            RefreshState::Running => "running",
            RefreshState::Waiting => "waiting",
            RefreshState::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Status {
    pub state: RefreshState,
    pub retry_after: Option<Duration>,
    pub error: Option<String>,
}

impl Status {
    fn none() -> Self {
        Self { state: RefreshState::None, retry_after: None, error: None }
    }

    pub fn json(&self, host: &str, kind: &str, identity: Option<&str>) -> Value {
        json!({
            "host": host,
            "kind": kind,
            "identity": identity,
            "state": self.state.name(),
            "retry_after_secs": self.retry_after.map(|d| d.as_secs_f64().ceil() as u64),
            "error": self.error,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InFlight {
    Queued,
    Running,
}

struct Slot {
    in_flight: Option<InFlight>,
    /// `in_flight` 進入目前狀態的時間，用來判定卡住。
    since: Instant,
    /// 最近一次開始的時間，用來限流。
    last_started: Option<Instant>,
    failures: u32,
    /// 上一次失敗後最早可以再試的時間。
    retry_at: Option<Instant>,
    error: Option<String>,
}

impl Slot {
    fn new(now: Instant) -> Self {
        Self { in_flight: None, since: now, last_started: None, failures: 0, retry_at: None, error: None }
    }
}

/// 一格現在能不能開始一趟；`Some` 是不能開始、以及要顯示的狀態。
enum Gate {
    Start,
    Hold(RefreshState, Duration),
}

fn gate(slot: &Slot, now: Instant, min_interval: Duration) -> Gate {
    if let Some(phase) = slot.in_flight {
        if now.saturating_duration_since(slot.since) <= STUCK_AFTER {
            let state = if phase == InFlight::Queued { RefreshState::Queued } else { RefreshState::Running };
            return Gate::Hold(state, POLL_HINT);
        }
    }
    if let Some(at) = slot.retry_at.filter(|at| *at > now) {
        return Gate::Hold(RefreshState::Failed, at - now);
    }
    if let Some(started) = slot.last_started {
        let next = started + min_interval;
        if next > now {
            return Gate::Hold(RefreshState::Waiting, next - now);
        }
    }
    Gate::Start
}

fn gate_status(slot: &Slot, now: Instant, min_interval: Duration) -> Status {
    match gate(slot, now, min_interval) {
        Gate::Start => Status::none(),
        Gate::Hold(state, wait) => Status {
            state,
            retry_after: Some(wait),
            error: if state == RefreshState::Failed { slot.error.clone() } else { None },
        },
    }
}

/// 刷新排程器：每格的去重、限流、退避，加上全域併發許可。
pub struct Ledger {
    slots: Mutex<HashMap<String, Slot>>,
    permits: Semaphore,
    min_interval: Duration,
    fetch_timeout: Duration,
}

impl Ledger {
    pub fn new() -> Self {
        Self::with_timings(MIN_INTERVAL, FETCH_TIMEOUT)
    }

    /// 測試用：縮短限流與逾時，不必等真的一分鐘。
    pub fn with_timings(min_interval: Duration, fetch_timeout: Duration) -> Self {
        Self { slots: Mutex::new(HashMap::new()), permits: Semaphore::new(MAX_CONCURRENT), min_interval, fetch_timeout }
    }

    /// 要求刷新一格。`fetch` 是實際的刷新動作（本機或遠端 CLI）。只有沒有進行中、也不在限流或退避內時才排程。
    /// 回傳這一次呼叫之後的狀態：排程了就是 `Queued`。
    pub async fn schedule<F, Fut>(this: &Arc<Self>, key: String, fetch: F) -> Status
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let now = Instant::now();
        {
            let mut slots = this.slots.lock().await;
            let slot = slots.entry(key.clone()).or_insert_with(|| Slot::new(now));
            if let Gate::Hold(..) = gate(slot, now, this.min_interval) {
                return gate_status(slot, now, this.min_interval);
            }
            slot.in_flight = Some(InFlight::Queued);
            slot.since = now;
            slot.last_started = Some(now);
        }
        tokio::spawn(Box::pin(Self::run(this.clone(), key, fetch)));
        Status { state: RefreshState::Queued, retry_after: Some(POLL_HINT), error: None }
    }

    /// 這一格目前的狀態，不改動任何東西。
    pub async fn status(&self, key: &str) -> Status {
        let now = Instant::now();
        match self.slots.lock().await.get(key) {
            None => Status::none(),
            Some(slot) => gate_status(slot, now, self.min_interval),
        }
    }

    async fn run<F, Fut>(this: Arc<Self>, key: String, fetch: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let Ok(_permit) = this.permits.acquire().await else { return };
        this.set_in_flight(&key, InFlight::Running).await;
        let result = match tokio::time::timeout(this.fetch_timeout, fetch()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(_) => Err(format!("refresh timed out after {}s", this.fetch_timeout.as_secs())),
        };
        this.finish(&key, result).await;
    }

    async fn set_in_flight(&self, key: &str, phase: InFlight) {
        if let Some(slot) = self.slots.lock().await.get_mut(key) {
            slot.in_flight = Some(phase);
            slot.since = Instant::now();
        }
    }

    async fn finish(&self, key: &str, result: Result<(), String>) {
        let now = Instant::now();
        let mut slots = self.slots.lock().await;
        let Some(slot) = slots.get_mut(key) else { return };
        slot.in_flight = None;
        slot.since = now;
        match result {
            Ok(()) => {
                slot.failures = 0;
                slot.retry_at = None;
                slot.error = None;
            }
            Err(error) => {
                slot.failures += 1;
                let wait = backoff(slot.failures);
                slot.retry_at = Some(now + wait);
                let error: String = error.chars().take(300).collect();
                tracing::warn!(key, failures = slot.failures, retry_in_s = wait.as_secs(), error = %error, "model list refresh failed");
                slot.error = Some(error);
            }
        }
    }
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

static LEDGER: OnceLock<Arc<Ledger>> = OnceLock::new();

fn ledger() -> &'static Arc<Ledger> {
    LEDGER.get_or_init(|| Arc::new(Ledger::new()))
}

/// 一般 Bot 的 `GET /api/models` 未命中或過期時呼叫：身份合法就排一趟背景刷新，回傳這格的狀態給 503 body。
/// 請求本身不等刷新結果；Bot 稍後重試即可。
pub async fn on_cache_miss(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>) -> Value {
    let key = cache_key(host, kind, identity);
    let status = if identity_is_known(app, host, kind, identity).await {
        let (app, host_owned, kind_owned, identity_owned) = (app.clone(), host.to_string(), kind.to_string(), identity.map(str::to_string));
        Ledger::schedule(ledger(), key, move || async move {
            crate::runners::models::list(&app, &host_owned, &kind_owned, identity_owned.as_deref(), true).await.map(|_| ())
        })
        .await
    } else {
        Status::none()
    };
    status.json(host, kind, identity)
}

async fn identity_is_known(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>) -> bool {
    match identity {
        None => true,
        Some(name) => crate::tools::identity_for_host(app, host, name).await.is_some_and(|idn| idn.kind == kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key() -> String {
        cache_key("local", "codex", None)
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        assert_eq!(backoff(1), Duration::from_secs(60));
        assert_eq!(backoff(2), Duration::from_secs(120));
        assert_eq!(backoff(3), Duration::from_secs(240));
        assert_eq!(backoff(4), Duration::from_secs(480));
        assert_eq!(backoff(5), BACKOFF_CAP, "第五次起封頂 15 分鐘");
        assert_eq!(backoff(u32::MAX), BACKOFF_CAP, "連續失敗很多次也不溢位");
    }

    /// single-flight：一趟還沒結束時，同一格的 miss 不另排；fetch 只跑一次。
    #[tokio::test]
    async fn one_cell_runs_at_most_one_refresh_at_a_time() {
        let ledger = Arc::new(Ledger::with_timings(Duration::ZERO, Duration::from_secs(5)));
        let calls = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let c = calls.clone();
        let first = Ledger::schedule(&ledger, key(), move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            let _ = release_rx.await;
            Ok(())
        })
        .await;
        assert_eq!(first.state, RefreshState::Queued);
        let c2 = calls.clone();
        let again = Ledger::schedule(&ledger, key(), move || async move {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(again.state, RefreshState::Queued | RefreshState::Running), "{again:?}");
        let _ = release_tx.send(());
        let idle = crate::testing::eventually!(ledger.status(&key()).await.state == RefreshState::None);
        assert!(idle, "一趟結束後格子回到 none");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "single-flight：第二個 closure 不能執行");
    }

    /// 限流：同一格一趟結束後，MIN_INTERVAL 內不再開始；不同格互不影響。
    #[tokio::test]
    async fn one_cell_starts_at_most_once_per_min_interval() {
        let ledger = Arc::new(Ledger::with_timings(Duration::from_secs(3600), Duration::from_secs(5)));
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        Ledger::schedule(&ledger, key(), move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        // 一趟結束後仍在限流內，狀態是 waiting（不是 none）。
        assert!(crate::testing::eventually!(ledger.status(&key()).await.state == RefreshState::Waiting));
        let c2 = calls.clone();
        let held = Ledger::schedule(&ledger, key(), move || async move {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert_eq!(held.state, RefreshState::Waiting);
        assert!(held.retry_after.is_some_and(|d| d > Duration::from_secs(3000)), "{held:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let other = cache_key("local", "grok", None);
        let c = calls.clone();
        let started = Ledger::schedule(&ledger, other, move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert_eq!(started.state, RefreshState::Queued, "另一格不受限流影響");
    }

    /// 失敗：記下錯誤、進入退避；退避內的 miss 不再跑 fetch（不打 ssh）；退避期滿才允許再試。
    #[tokio::test]
    async fn a_failed_refresh_backs_off_and_never_probes_during_backoff() {
        let ledger = Arc::new(Ledger::with_timings(Duration::ZERO, Duration::from_secs(5)));
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        Ledger::schedule(&ledger, key(), move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("codex app-server exploded")
        })
        .await;
        assert!(crate::testing::eventually!(ledger.status(&key()).await.state == RefreshState::Failed));
        let failed = ledger.status(&key()).await;
        assert!(failed.error.as_deref().is_some_and(|e| e.contains("exploded")), "{failed:?}");
        assert!(failed.retry_after.is_some_and(|d| d > Duration::from_secs(30)), "{failed:?}");

        let c2 = calls.clone();
        let during = Ledger::schedule(&ledger, key(), move || async move {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert_eq!(during.state, RefreshState::Failed);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "退避內不跑 fetch");
        assert_eq!(ledger.slots.lock().await.get(&key()).map(|s| s.failures), Some(1), "退避內失敗次數不變");
    }

    /// 遠端 ssh 之類卡住的刷新，逾時就收掉、記成失敗並退避，不佔住併發名額。
    #[tokio::test]
    async fn a_hung_refresh_times_out_and_counts_as_a_failure() {
        let ledger = Arc::new(Ledger::with_timings(Duration::ZERO, Duration::from_millis(50)));
        Ledger::schedule(&ledger, key(), || async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(())
        })
        .await;
        assert!(crate::testing::eventually!(ledger.status(&key()).await.state == RefreshState::Failed));
        let failed = ledger.status(&key()).await;
        assert!(failed.error.as_deref().is_some_and(|e| e.contains("timed out")), "{failed:?}");
        // 名額已還：另一格立刻能跑完。
        let other = cache_key("local", "grok", None);
        Ledger::schedule(&ledger, other.clone(), || async { Ok(()) }).await;
        assert!(crate::testing::eventually!(ledger.status(&other).await.state == RefreshState::None));
    }

    /// 卡住超過 STUCK_AFTER 的排隊／執行中格子允許重排，不然一個壞掉的格子會永遠回 queued。
    #[tokio::test]
    async fn a_stuck_in_flight_cell_is_rescheduled() {
        let ledger = Arc::new(Ledger::with_timings(Duration::ZERO, Duration::from_secs(5)));
        let old = Instant::now().checked_sub(STUCK_AFTER + Duration::from_secs(1)).unwrap();
        ledger.slots.lock().await.insert(
            key(),
            Slot { in_flight: Some(InFlight::Running), since: old, last_started: Some(old), failures: 0, retry_at: None, error: None },
        );
        let status = Ledger::schedule(&ledger, key(), || async { Ok(()) }).await;
        assert_eq!(status.state, RefreshState::Queued);
    }
}
