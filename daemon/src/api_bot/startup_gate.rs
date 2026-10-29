//! Boot-only recovery gate. No public operation can force admission open.
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

const PENDING: u8 = 0;
const READY: u8 = 1;
const QUARANTINED: u8 = 2;
const RUNNING: u8 = 3;

pub(crate) struct Gate(AtomicU8);
impl Gate {
    pub(crate) fn new() -> Self {
        Self(AtomicU8::new(PENDING))
    }
    pub(crate) fn ready(&self) -> bool {
        self.0.load(Ordering::Acquire) == READY
    }
}

pub(crate) fn barrier(
    app: &Arc<crate::state::App>,
) -> impl std::future::Future<Output = ()> + Send + '_ {
    Box::pin(barrier_body(app))
}
async fn barrier_body(app: &Arc<crate::state::App>) {
    if app
        .api_startup_gate
        .0
        .compare_exchange(PENDING, RUNNING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let outcome = if app.is_startup_ready() || app.shutdown.is_cancelled() {
        Err("api_startup_not_exclusive")
    } else {
        match sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM bots WHERE kind='claude-api')",
        )
        .fetch_one(&app.db)
        .await
        {
            Ok(false) => Ok(()),
            Ok(true) => match super::startup_recovery::recover_all(app).await {
                Ok(()) => super::startup_retention::prune(app).await,
                Err(e) => Err(e.code()),
            },
            Err(_) => Err("api_startup_storage"),
        }
    };
    let outcome = if app.is_startup_ready() || app.shutdown.is_cancelled() {
        Err("api_startup_not_exclusive")
    } else {
        outcome
    };
    let (state, reason) = match outcome {
        Ok(()) => (READY, "api_startup_ready"),
        Err(reason) => (QUARANTINED, reason),
    };
    app.api_startup_gate.0.store(state, Ordering::Release);
    tracing::info!(reason, "API bot startup recovery");
}
