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
    app.background_tasks().spawn(async move {
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
