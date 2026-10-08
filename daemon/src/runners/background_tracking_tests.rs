//! #924：daemon 起的背景 runner 都掛在 `background_tasks` 下（panic 後由 supervisor 重啟、關機時收得乾淨）。
//! 每條一個 `<runner>_is_tracked_and_stops_at_shutdown`；`hook_inbox`、`build_scheduler` 再驗「一輪 tick panic 之後迴圈照常」。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::state::App;
use crate::testing as tt;

/// 碰 `PANIC_NEXT_TICK`／`TICKS`（全域）的測試一次一個：別的測試開的 worker 不能取走這邊的 panic 旗標、也不能灌水 tick 計數。
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 開出去的迴圈掛進 `background_tasks`（+1），關機時 2 秒內收完。`settle`：先讓出幾次執行權，讓迴圈跑到第一個睡覺點，
/// 證明睡覺時有看 shutdown；第一輪就會真的做事（探測 CLI 之類）的 runner 不 settle——它們在第一輪前就被 shutdown 擋下。
async fn tracked_and_stops(settle: bool, spawn: impl FnOnce(Arc<App>)) {
    let env = tt::env().await;
    let app = env.app.clone();
    let before = app.background_tasks.len();
    spawn(app.clone());
    assert_eq!(app.background_tasks.len(), before + 1, "沒有掛進 background_tasks");
    if settle {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }
    app.shutdown.cancel();
    app.background_tasks.close();
    tokio::time::timeout(Duration::from_secs(2), app.background_tasks.wait())
        .await
        .expect("關機時迴圈要在 2 秒內離開，不是等睡完");
}

#[tokio::test]
async fn hook_inbox_worker_is_tracked_and_stops_at_shutdown() {
    let _serial = SERIAL.lock().await;
    tracked_and_stops(true, crate::runners::hook_inbox::spawn_worker).await;
}

#[tokio::test]
async fn survey_watcher_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(true, crate::runners::tui_prompts::spawn_survey_watcher).await;
}

#[tokio::test]
async fn update_watcher_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(true, crate::runners::update_watch::spawn_update_watcher).await;
}

#[tokio::test]
async fn upstream_update_watcher_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(true, crate::runners::upstream_update::spawn).await;
}

#[tokio::test]
async fn build_scheduler_sweeper_is_tracked_and_stops_at_shutdown() {
    let _serial = SERIAL.lock().await;
    tracked_and_stops(true, crate::runners::build_scheduler::spawn_sweeper).await;
}

#[tokio::test]
async fn github_host_scan_is_tracked() {
    tracked_and_stops(false, crate::runners::github::spawn_detect_all).await;
}

#[tokio::test]
async fn codex_quota_poller_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(false, crate::runners::quota::spawn_codex_poller).await;
}

#[tokio::test]
async fn claude_quota_poller_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(false, crate::runners::quota_claude::spawn_claude_poller).await;
}

#[tokio::test]
async fn grok_quota_poller_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(false, crate::runners::quota_grok::spawn_grok_poller).await;
}

#[tokio::test]
async fn agy_quota_poller_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(false, crate::runners::quota_agy::spawn_agy_poller).await;
}

#[tokio::test]
async fn agy_login_watcher_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(true, crate::runners::quota_agy::spawn_agy_login_watcher).await;
}

#[tokio::test]
async fn remote_purge_poller_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(true, crate::runners::remote_purge::spawn_poller).await;
}

#[tokio::test]
async fn remote_purge_sweep_is_tracked() {
    tracked_and_stops(false, |app| crate::runners::remote_purge::spawn_sweep(app, "no-such-host".to_string())).await;
}

#[tokio::test]
async fn herdr_version_poller_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(true, crate::runners::herdr_version::spawn_poller).await;
}

#[tokio::test]
async fn default_session_poller_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(true, crate::runners::default_session::spawn_poller).await;
}

#[tokio::test]
async fn mission_relay_is_tracked_and_stops_at_shutdown() {
    tracked_and_stops(true, crate::runners::mission::spawn_relay).await;
}

/// 第一輪 tick panic 之後 supervisor 重啟迴圈，後面的輪次照常（`PANIC_RETRY` 在 test 是 5ms）。
#[tokio::test]
async fn hook_inbox_worker_survives_a_panic_in_one_tick() {
    let _serial = SERIAL.lock().await;
    use crate::runners::hook_inbox::{PANIC_NEXT_TICK, TICKS};
    let env = tt::env().await;
    let app = env.app.clone();
    TICKS.store(0, Ordering::SeqCst);
    PANIC_NEXT_TICK.store(true, Ordering::SeqCst);
    crate::runners::hook_inbox::spawn_worker(app.clone());
    assert!(tt::eventually!(!PANIC_NEXT_TICK.load(Ordering::SeqCst)), "第一輪 tick 要 panic（旗標被取走）");
    // panic 的那一輪不計；重啟後的輪次要出現，而且醒著的 worker 仍然回應喚醒（再多一輪）。
    assert!(tt::eventually!(TICKS.load(Ordering::SeqCst) >= 1), "panic 之後迴圈要重啟");
    let seen = TICKS.load(Ordering::SeqCst);
    app.hook_inbox_wake.notify_one();
    assert!(tt::eventually!(TICKS.load(Ordering::SeqCst) > seen), "重啟後的 worker 仍會被喚醒");
    app.shutdown.cancel();
    app.background_tasks.close();
    tokio::time::timeout(Duration::from_secs(2), app.background_tasks.wait()).await.expect("關機要收乾淨");
}

#[tokio::test]
async fn build_scheduler_sweeper_survives_a_panic_in_one_tick() {
    let _serial = SERIAL.lock().await;
    use crate::runners::build_scheduler::{PANIC_NEXT_TICK, TICKS};
    let env = tt::env().await;
    let app = env.app.clone();
    TICKS.store(0, Ordering::SeqCst);
    PANIC_NEXT_TICK.store(true, Ordering::SeqCst);
    crate::runners::build_scheduler::spawn_sweeper(app.clone());
    assert!(tt::eventually!(!PANIC_NEXT_TICK.load(Ordering::SeqCst)), "第一輪 sweep 要 panic");
    assert!(tt::eventually!(TICKS.load(Ordering::SeqCst) >= 2), "panic 之後的 sweep 照常發生");
    app.shutdown.cancel();
    app.background_tasks.close();
    tokio::time::timeout(Duration::from_secs(2), app.background_tasks.wait()).await.expect("關機要收乾淨");
}
