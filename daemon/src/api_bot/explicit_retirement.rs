//! Exact-target retirement; deletion is never completion or join evidence.
use std::{future::Future, sync::Arc};
use crate::lifecycle::api_bot_port::{ApiFreshError as E, ApiFreshResult as R, ApiFreshTarget};
use super::retention::RetentionCandidate;

#[cfg(all(test, feature = "daemon-test-harness"))]
fn lost_ack_faults() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static FAULTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    FAULTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}
#[cfg(all(test, feature = "daemon-test-harness"))]
pub(super) fn arm_lost_ack_for_test(session: &str) {
    assert!(lost_ack_faults().lock().unwrap().insert(session.to_owned()));
}

pub(super) fn retire<'a>(app: &'a Arc<crate::state::App>, bot: &'a str, target: &'a ApiFreshTarget) -> impl Future<Output = Result<R, E>> + Send + 'a {
    let app = app.clone();
    let bot = bot.to_owned();
    let target = ApiFreshTarget { session_id: target.session_id.clone(), generation: target.generation };
    let tasks = app.background_tasks.clone();
    Box::pin(async move {
        let task = tasks.spawn(Box::pin(async move { retire_body(&app, &bot, &target).await }));
        task.await.unwrap_or(Err(E::Unconfirmed))
    })
}

async fn candidate(conn: &mut sqlx::SqliteConnection, bot: &str, target: &ApiFreshTarget) -> Result<Option<RetentionCandidate>, E> {
    sqlx::query_as("SELECT s.bot_id,s.id,s.owner_run_id,s.owner_turn_id,s.generation,s.revision,s.expires_at,s.retired_at,b.deleted_at FROM api_bot_sessions s JOIN bots b ON b.id=s.bot_id WHERE s.id=? AND s.bot_id=? AND s.generation=? AND b.kind='claude-api' AND b.deleted_at IS NULL AND s.retired_at IS NULL")
        .bind(&target.session_id).bind(bot).bind(target.generation).fetch_optional(conn).await.map_err(|_| E::Storage)
}

pub(super) struct ApiOwnedBotLock {
    bot: String,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl ApiOwnedBotLock {
    pub(super) async fn acquire(app: &Arc<crate::state::App>, bot: &str) -> Result<Self, E> {
        let lock = crate::capabilities::BotLocks::bot_lock(app.as_ref(), bot).await;
        let guard = tokio::select! {
            biased;
            _ = app.shutdown.cancelled() => return Err(E::Busy),
            guard = lock.lock_owned() => guard,
        };
        Ok(Self { bot: bot.to_owned(), _guard: guard })
    }
}

async fn retire_body(app: &Arc<crate::state::App>, bot: &str, target: &ApiFreshTarget) -> Result<R, E> {
    if !app.api_startup_gate.ready() || app.shutdown.is_cancelled() { return Err(E::Busy); }
    if target.session_id.is_empty() || target.generation <= 0 { return Err(E::IdentityMismatch); }
    let lifecycle = ApiOwnedBotLock::acquire(app, bot).await?;
    retire_body_locked(app, &lifecycle, target).await
}

pub(super) fn retire_body_locked<'a>(app: &'a Arc<crate::state::App>, lifecycle: &'a ApiOwnedBotLock, target: &'a ApiFreshTarget) -> impl Future<Output = Result<R, E>> + Send + 'a {
    Box::pin(retire_body_locked_inner(app, lifecycle, target))
}

async fn retire_body_locked_inner(app: &Arc<crate::state::App>, lifecycle: &ApiOwnedBotLock, target: &ApiFreshTarget) -> Result<R, E> {
    let bot = lifecycle.bot.as_str();
    if !app.api_startup_gate.ready() || app.shutdown.is_cancelled() { return Err(E::Busy); }
    if target.session_id.is_empty() || target.generation <= 0 { return Err(E::IdentityMismatch); }
    if app.api_bot_runtime.explicit_retire_quarantined(bot) { return Err(E::Unconfirmed); }
    let mut conn = app.db.acquire().await.map_err(|_| E::Storage)?;
    let initial = candidate(&mut conn, bot, target).await?.ok_or(E::Unconfirmed)?;
    drop(conn);
    let (Some(owner_run), Some(_)) = (initial.2.as_ref(), initial.3.as_ref()) else { return Err(E::Unresolved); };
    if let Some(active) = crate::db::active_run(&app.db, bot).await.map_err(|_| E::Storage)? {
        if active.id != *owner_run { return Err(E::Busy); }
        crate::lifecycle::stop::stop_bot_locked(app, bot).await.map_err(|_| E::Unconfirmed)?;
    }
    let mut conn = app.db.acquire().await.map_err(|_| E::Storage)?;
    let snapshot = candidate(&mut conn, bot, target).await?.ok_or(E::Unconfirmed)?;
    drop(conn);
    if snapshot.2 != initial.2 || snapshot.3 != initial.3 { return Err(E::Unconfirmed); }
    let guard = app.api_bot_runtime.explicit_retire_guard(app, &snapshot).await?;
    let token = app.api_bot_runtime.begin_explicit_retire(&guard).ok_or(E::Unconfirmed)?;
    let mut tx = match crate::db::begin_write(&app.db).await {
        Ok(tx) => tx,
        Err(_) => {
            if !app.shutdown.is_cancelled() { app.api_bot_runtime.clear_explicit_retire(&guard, &token); }
            return Err(E::Storage);
        }
    };
    let action = async {
        let current = candidate(&mut tx, bot, target).await?;
        if current.as_ref() != Some(&snapshot) { return Err(E::Unconfirmed); }
        if let Err(reason) = super::retention::prove(&mut tx, &snapshot).await {
            return Err(if reason == "api_retention_storage" { E::Storage } else { E::Unresolved });
        }
        let changed = crate::db::api_bot::explicit_retire_delete_on(&mut tx, &snapshot, &crate::db::now()).await.map_err(|_| E::Storage)?;
        if !changed { return Err(E::Unconfirmed); }
        Ok(())
    }.await;
    if let Err(error) = action {
        if tx.rollback().await.is_err() { return Err(E::Unconfirmed); }
        if !app.shutdown.is_cancelled() && !app.api_bot_runtime.clear_explicit_retire(&guard, &token) { return Err(E::Unconfirmed); }
        return Err(error);
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("api_fresh_before_commit", &target.session_id).await;
    if app.shutdown.is_cancelled() {
        if tx.rollback().await.is_err() { return Err(E::Unconfirmed); }
        return Err(E::Busy);
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    let commit_result = {
        use std::future::Future;
        let mut commit = Box::pin(tx.commit());
        let first = std::future::poll_fn(|cx| std::task::Poll::Ready(commit.as_mut().poll(cx))).await;
        match first {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => {
                crate::lifecycle::race_point::hit("api_fresh_commit_polled", &target.session_id).await;
                commit.await
            }
        }
    };
    #[cfg(not(all(test, feature = "daemon-test-harness")))]
    let commit_result = tx.commit().await;
    let mut commit_failed = commit_result.is_err();
    #[cfg(all(test, feature = "daemon-test-harness"))]
    if !commit_failed && lost_ack_faults().lock().unwrap().remove(&target.session_id) { commit_failed = true; }
    if commit_failed {
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("api_fresh_before_readback", &target.session_id).await;
        let result = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM api_bot_sessions WHERE id=?)").bind(&target.session_id).fetch_one(&app.db).await;
        match result {
            Err(_) => {
                tracing::warn!(reason="api_explicit_retire_commit_unconfirmed", "API session retirement unconfirmed");
                return Err(E::Unconfirmed);
            }
            Ok(true) => {
                tracing::warn!(reason="api_explicit_retire_commit_unconfirmed", "API session retirement unconfirmed");
                return Err(E::Unconfirmed);
            }
            Ok(false) => {}
        }
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("api_fresh_after_commit", &target.session_id).await;
    if !app.api_bot_runtime.finish_explicit_retire(&guard, &token) { return Err(E::Unconfirmed); }
    Ok(R::Cleared)
}
