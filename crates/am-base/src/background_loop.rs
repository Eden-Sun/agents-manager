//! Supervision for long-lived daemon background loops.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[cfg(not(any(test, feature = "test-hooks")))]
const PANIC_RETRY: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];
#[cfg(any(test, feature = "test-hooks"))]
const PANIC_RETRY: [Duration; 5] = [Duration::from_millis(5); 5];
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// 掛在 App 的 task tracker 下、還沒結束的工作名字（#1463）。關機等待逾時時用它記 log，讓人知道是哪一個卡住。
/// 行程裡只有一個 App（測試裡多個 App 共用這張表，名字只拿來記 log 與比對）。
fn unfinished() -> &'static std::sync::Mutex<std::collections::BTreeMap<u64, &'static str>> {
    static V: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<u64, &'static str>>> = std::sync::OnceLock::new();
    V.get_or_init(Default::default)
}

/// 登記的守衛：工作結束、被 abort、或 panic 時（future 被丟掉）都會把登記拿掉。
struct Registered(u64);

impl Drop for Registered {
    fn drop(&mut self) {
        unfinished().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// 掛進 `tracker` 下的工作，並登記名字（#1463）。跟直接 `tracker.spawn` 行為一樣，只多了名字可以在關機逾時時列出來。
pub fn spawn_named<F>(tracker: &tokio_util::task::TaskTracker, name: &'static str, fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    unfinished().lock().unwrap_or_else(|e| e.into_inner()).insert(id, name);
    tracker.spawn(async move {
        let _registered = Registered(id);
        fut.await
    })
}

/// 現在還沒結束的工作名字（關機等待逾時時記進 log）。
pub fn unfinished_names() -> Vec<&'static str> {
    unfinished().lock().unwrap_or_else(|e| e.into_inner()).values().copied().collect()
}

/// Start a loop under the App task tracker. Panics restart with bounded backoff; shutdown gives
/// the current pass time to leave at its next cancellation check before aborting it as a last resort.
pub fn spawn_restartable<F, Fut>(app: &(impl crate::capabilities::BgTasks + crate::capabilities::Shutdown), name: &'static str, factory: F)
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if app.shutdown().is_cancelled() {
        return;
    }
    let shutdown = app.shutdown().clone();
    spawn_named(app.background_tasks(), name, async move {
        restart_loop(shutdown, name, factory).await;
    });
}

/// 固定週期的迴圈（#924）：先睡 `first_delay`、做一次 `tick`、之後每 `every` 一次；panic 由 [`spawn_restartable`] 退避重啟
/// （重啟後從 `first_delay` 再開始），睡覺時看 shutdown，所以關機不必等滿一個週期。
/// `tick` 在每一輪拿到 `Arc<H>`；要跨輪保留的狀態放在 `H` 或 `tick` 捕捉的 `Arc<…>` 裡。
pub fn spawn_periodic<H, F, Fut>(app: &Arc<H>, name: &'static str, every: Duration, first_delay: Duration, tick: F)
where
    H: crate::capabilities::BgTasks + crate::capabilities::Shutdown + Send + Sync + 'static,
    F: Fn(Arc<H>) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let handle = app.clone();
    spawn_restartable(&**app, name, move || {
        let app = handle.clone();
        let tick = tick.clone();
        async move {
            let shutdown = crate::capabilities::Shutdown::shutdown(&*app).clone();
            let mut delay = first_delay;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
                tick(app.clone()).await;
                delay = every;
            }
        }
    });
}

/// Run one long-lived loop and restart it if its task unwinds from a panic.
pub async fn restart_loop<F, Fut>(shutdown: CancellationToken, name: &'static str, factory: F)
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let factory = Arc::new(factory);
    let mut failures = 0usize;
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let make = factory.clone();
        let mut task = tokio::spawn(async move { make().await });
        tokio::select! {
            result = &mut task => match result {
                Ok(()) => return,
                Err(error) if error.is_panic() => {
                    let delay = PANIC_RETRY[failures.min(PANIC_RETRY.len() - 1)];
                    failures = failures.saturating_add(1);
                    tracing::error!(loop_name = name, panic = ?error, retry_ms = delay.as_millis(), "background loop panicked; restarting");
                    tokio::select! {
                        _ = shutdown.cancelled() => return,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                Err(error) => {
                    tracing::warn!(loop_name = name, error = ?error, "background loop task was cancelled; stopping its supervisor");
                    return;
                }
            },
            _ = shutdown.cancelled() => {
                if tokio::time::timeout(SHUTDOWN_GRACE, &mut task).await.is_err() {
                    tracing::warn!(loop_name = name, grace_seconds = SHUTDOWN_GRACE.as_secs(), "background loop did not reach its shutdown boundary; aborting");
                    task.abort();
                    let _ = task.await;
                }
                return;
            }
        }
    }
}
