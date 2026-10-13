//! 專案排程（使用者 2026-10-13）。
//!
//! - `time`：四種模式的時間計算（純函式）。
//! - `store`：schedules／schedule_occurrences 的 DB 操作。
//! - `runner`：tick（到期→帳本→綁 turn），以及停用／編輯／刪除的撤回。
//! - `api`：User-only HTTP。
//!
//! 併發與鎖序見 `runner.rs` 開頭；occurrence 在綁定的 turn 終態前都算未結束。

pub mod api;
pub mod runner;
pub mod store;
pub mod time;

#[cfg(test)]
mod tests;

use crate::state::App;
use std::sync::Arc;

/// 啟動排程 poller（tracked 的 periodic，關機時一起停）。啟動後 5 秒先跑一輪，等於啟動對帳。
pub fn spawn_poller(app: Arc<App>) {
    crate::background_loop::spawn_periodic(&app, "schedules", runner::TICK_EVERY, std::time::Duration::from_secs(5), |app| async move {
        if let Err(e) = runner::tick(&app).await {
            tracing::warn!(error = ?e, "schedules tick failed");
        }
    });
}
