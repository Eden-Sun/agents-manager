//! `primary_keep_warm` runner。

use crate::state::App;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TICK_EVERY: Duration = Duration::from_secs(30);
static SWEEPING: AtomicBool = AtomicBool::new(false);
static LAST_SWEEP: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// 持有「巡邏進行中」旗標；`Drop` 時才放掉。sweep 的 future panic 時 tokio 只吞掉 panic，
/// 這個 guard 仍會在 unwind 時被丟掉，旗標不會永遠卡在 true（同 idle_sleep 的 `SweepGuard`，issue #975）。
struct SweepGuard;

impl SweepGuard {
    fn take() -> Option<Self> {
        (!SWEEPING.swap(true, Ordering::SeqCst)).then_some(Self)
    }
}

impl Drop for SweepGuard {
    fn drop(&mut self) {
        SWEEPING.store(false, Ordering::SeqCst);
    }
}

/// 控制迴圈每一拍呼叫一次；真正的巡邏最多每 [`TICK_EVERY`] 一次，丟到背景跑（送 prompt 要等 pane，不卡住迴圈）。
pub fn tick(app: &Arc<App>) {
    if cfg!(test) || app.shutdown.is_cancelled() {
        return;
    }
    let now = Instant::now();
    // guard 要活到 spawn 進去的 task 裡，不能在這個區塊結束時就放掉。
    let guard = {
        let mut last = LAST_SWEEP.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|l| now.checked_duration_since(l).is_some_and(|d| d < TICK_EVERY)) {
            return;
        }
        let Some(guard) = SweepGuard::take() else {
            return;
        };
        *last = Some(now);
        guard
    };
    let app = app.clone();
    let tasks = app.background_tasks.clone();
    tasks.spawn(async move {
        let _guard = guard;
        crate::app_ports_r2a8::primary_keep_warm_sweep(&app).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // 全域旗標只有這一條測試碰，不會和其他測試搶。
    #[test]
    fn the_sweep_flag_is_released_when_the_sweep_panics() {
        let g = SweepGuard::take().expect("free");
        assert!(SweepGuard::take().is_none());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _g = g;
            panic!("boom");
        }));
        assert!(r.is_err());
        let again = SweepGuard::take();
        assert!(again.is_some(), "panic 之後旗標要放掉");
    }
}
