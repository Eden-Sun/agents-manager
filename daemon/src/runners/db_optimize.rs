//! `PRAGMA optimize`：啟動時一次、之後每 24 小時一次（db-perf 2026-11-26）。
//! 沒有統計資料（`sqlite_stat1` 從未建過）的表會在這裡做 ANALYZE；`analysis_limit` 讓大表只取樣，不會在啟動時整張掃。
use std::{sync::Arc, time::Duration};

use crate::state::App;

const EVERY: Duration = Duration::from_secs(24 * 3600);

pub fn spawn(app: Arc<App>) {
    crate::background_loop::spawn_restartable(&app, "sqlite optimize", {
        let app = app.clone();
        move || {
            let app = app.clone();
            async move {
                loop {
                    if let Err(e) = optimize(&app.db).await {
                        tracing::warn!(error = %e, "PRAGMA optimize failed");
                    }
                    tokio::select! {
                        biased;
                        _ = app.shutdown.cancelled() => return,
                        _ = tokio::time::sleep(EVERY) => {}
                    }
                }
            }
        }
    });
}

pub async fn optimize(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let mut conn = pool.acquire().await?;
    sqlx::query("PRAGMA analysis_limit = 1000").execute(&mut *conn).await?;
    sqlx::query("PRAGMA optimize").execute(&mut *conn).await?;
    Ok(())
}
