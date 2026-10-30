//! Diagnose pool starvation independently of the DB-backed runners that stop making progress.
use std::{sync::Arc, time::Duration};

use crate::state::App;

const PROBE_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_secs(30);

pub fn spawn(app: Arc<App>) {
    crate::background_loop::spawn_restartable(&app, "sqlite pool health", {
        let app = app.clone();
        move || {
            let app = app.clone();
            async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = app.shutdown.cancelled() => return,
                        _ = tokio::time::sleep(POLL) => {}
                    }
                    // Do not add a query or a waiting borrower to a healthy pool.
                    if app.db.num_idle() > 0 {
                        continue;
                    }
                    tokio::select! {
                        biased;
                        _ = app.shutdown.cancelled() => return,
                        _ = probe(&app.db, PROBE_WAIT) => {}
                    }
                }
            }
        }
    });
}

async fn probe(pool: &sqlx::SqlitePool, wait: Duration) -> bool {
    let started = std::time::Instant::now();
    let error = match tokio::time::timeout(wait, pool.acquire()).await {
        Ok(Ok(connection)) => {
            drop(connection);
            return true;
        }
        Ok(Err(error)) => error,
        Err(_) => sqlx::Error::PoolTimedOut,
    };
    tracing::warn!(
        consumer = "sqlite_pool_health", %error,
        size = pool.size(), idle = pool.num_idle(),
        max = pool.options().get_max_connections(),
        waited_ms = started.elapsed().as_millis() as u64,
        "sqlite pool acquisition failed; connections may be held or queries stalled"
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquisition_probe_detects_exhaustion_and_recovery() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let held = pool.acquire().await.unwrap();
        assert!(!probe(&pool, Duration::from_millis(10)).await);
        drop(held);
        assert!(probe(&pool, Duration::from_secs(2)).await);
        pool.close().await;
    }

    #[tokio::test]
    async fn pool_health_is_tracked_and_stops_at_shutdown() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let before = app.background_tasks.len();
        spawn(app.clone());
        assert_eq!(app.background_tasks.len(), before + 1);
        tokio::task::yield_now().await;
        app.shutdown.cancel();
        app.background_tasks.close();
        tokio::time::timeout(Duration::from_secs(2), app.background_tasks.wait())
            .await
            .unwrap();
    }
}
