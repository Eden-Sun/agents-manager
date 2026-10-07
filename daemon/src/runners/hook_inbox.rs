//! `hook_inbox` 背景工作與 drain_once runner。

use crate::app_ports_r2a9::HookProcessor;
use crate::db;
use crate::hook_body::HookBody;
use crate::hook_inbox::{
    mark_dead, mark_done, mark_failed, pending, prune, unparseable_body_reason,
};
use crate::state::App;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

const POLL_EVERY: Duration = Duration::from_secs(5);
const BATCH: i64 = 64;

/// 唯一的消費者。`receive` / drain 只負責 commit 之後叫醒它，不自己處理——單一消費者才不必為
/// 「同一列被兩邊同時處理」另外加 claim 欄位。
pub fn spawn_worker(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            // 先做一輪再等：daemon 重啟後把上一輪沒處理完的補回來，就是這一行。
            match drain_once(&app).await {
                Ok(n) if n > 0 => tracing::info!(processed = n, "hook inbox drained"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = ?e, "hook inbox drain failed"),
            }
            if let Err(e) = prune(&app.db).await {
                tracing::debug!(error = ?e, "hook inbox prune failed");
            }
            tokio::select! {
                _ = app.hook_inbox_wake.notified() => {}
                _ = tokio::time::sleep(POLL_EVERY) => {}
            }
        }
    });
}

/// 處理一批。回傳這一輪真的處理完幾列。
pub async fn drain_once(app: &Arc<App>) -> Result<usize> {
    let mut done = 0usize;
    loop {
        let rows = pending(&app.db, &db::now(), BATCH).await?;
        if rows.is_empty() {
            return Ok(done);
        }
        let batch = rows.len();
        for row in rows {
            match serde_json::from_str::<HookBody>(&row.body_json) {
                Ok(body) => match app.process_hook(&body, Some(&row.id)).await {
                    Ok(()) => {
                        mark_done(&app.db, &row.id).await?;
                        done += 1;
                    }
                    Err(e) => {
                        let attempts = row.attempts + 1;
                        tracing::warn!(id = %row.id, attempts, error = ?e, "hook event failed; will retry");
                        mark_failed(&app.db, &row.id, attempts, &format!("{e:#}")).await?;
                    }
                },
                Err(_) => {
                    tracing::error!(id = %row.id, "hook event body unparseable; dropped");
                    mark_dead(&app.db, &row.id, unparseable_body_reason()).await?;
                }
            }
        }
        // 這一批沒滿就沒有下一批了；滿了就繼續，避免一次喚醒只吃 BATCH 列。
        if batch < BATCH as usize {
            return Ok(done);
        }
    }
}
