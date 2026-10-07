//! `primary_keep_warm` runner。

use crate::state::App;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TICK_EVERY: Duration = Duration::from_secs(30);
static SWEEPING: AtomicBool = AtomicBool::new(false);
static LAST_SWEEP: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// 控制迴圈每一拍呼叫一次；真正的巡邏最多每 [`TICK_EVERY`] 一次，丟到背景跑（送 prompt 要等 pane，不卡住迴圈）。
pub fn tick(app: &Arc<App>) {
    if cfg!(test) || app.shutdown.is_cancelled() {
        return;
    }
    let now = Instant::now();
    {
        let mut last = LAST_SWEEP.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|l| now.checked_duration_since(l).is_some_and(|d| d < TICK_EVERY)) {
            return;
        }
        if SWEEPING.swap(true, Ordering::SeqCst) {
            return;
        }
        *last = Some(now);
    }
    let app = app.clone();
    let tasks = app.background_tasks.clone();
    tasks.spawn(async move {
        crate::app_ports_r2a8::primary_keep_warm_sweep(&app).await;
        SWEEPING.store(false, Ordering::SeqCst);
    });
}
