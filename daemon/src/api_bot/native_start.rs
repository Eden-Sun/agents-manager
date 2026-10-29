//! Tracked native API start, including exact-target retirement under the same bot lock.
use crate::lifecycle::api_bot_port::{ApiNativeStartError as E, ApiNativeStartMode as Mode, ApiNativeStartResult};
use std::{future::Future, sync::Arc};

pub(super) fn start<'a>(app: &'a Arc<crate::state::App>, bot: &'a str, mode: Mode)
    -> impl Future<Output = Result<ApiNativeStartResult, E>> + Send + 'a {
    let app = app.clone();
    let bot = bot.to_owned();
    let tasks = app.background_tasks.clone();
    Box::pin(async move {
        let task = tasks.spawn(Box::pin(async move { start_body(&app, &bot, mode).await }));
        task.await.unwrap_or(Err(E::Unconfirmed))
    })
}

fn start_body<'a>(app: &'a Arc<crate::state::App>, bot: &'a str, mode: Mode) -> impl Future<Output = Result<ApiNativeStartResult, E>> + Send + 'a {
    Box::pin(start_body_inner(app, bot, mode))
}
async fn start_body_inner(app: &Arc<crate::state::App>, bot: &str, mode: Mode) -> Result<ApiNativeStartResult, E> {
    if !app.api_startup_gate.ready() || app.shutdown.is_cancelled() { return Err(E::Unconfirmed); }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    let lock = {
        let mut future = Box::pin(super::explicit_retirement::ApiOwnedBotLock::acquire(app, bot));
        let mut notified = false;
        std::future::poll_fn(|cx| {
            let result = future.as_mut().poll(cx);
            if result.is_pending() && !notified {
                notified = true;
                if let Some(tx) = pending_observers().lock().unwrap().remove(bot) { let _ = tx.send(()); }
            }
            result
        }).await.map_err(|_| E::Unconfirmed)?
    };
    #[cfg(not(all(test, feature = "daemon-test-harness")))]
    let lock = super::explicit_retirement::ApiOwnedBotLock::acquire(app, bot).await.map_err(|_| E::Unconfirmed)?;
    if app.api_bot_runtime.explicit_retire_quarantined(bot) { return Err(E::Unconfirmed); }
    app.api_bot_runtime.native_start_dependencies_ready(app, bot).await.map_err(|_| E::Unconfirmed)?;
    let (cleared, required) = match mode {
        Mode::Resume { required } => (false, required),
        Mode::Fresh(target) => {
            super::explicit_retirement::retire_body_locked(app, &lock, &target).await.map_err(|_| E::Unconfirmed)?;
            (true, false)
        }
    };
    match create_run(app, bot, required, cleared).await {
        Ok((run_id, resumed)) => Ok(ApiNativeStartResult { run_id, resumed }),
        Err(()) if cleared => Err(E::ClearedButStartUnconfirmed),
        Err(()) => Err(E::Unconfirmed),
    }
}

fn create_run<'a>(app: &'a crate::state::App, bot: &'a str, required: bool, cleared: bool) -> impl Future<Output = Result<(String, bool), ()>> + Send + 'a {
    Box::pin(create_run_inner(app, bot, required, cleared))
}
async fn create_run_inner(app: &crate::state::App, bot: &str, required: bool, cleared: bool) -> Result<(String, bool), ()> {
    if !app.api_startup_gate.ready() || app.shutdown.is_cancelled() || app.api_bot_runtime.explicit_retire_quarantined(bot) { return Err(()); }
    let id = crate::db::ulid();
    let key = crate::lifecycle::api_bot_port::ApiRunKey { bot_id: bot.to_owned(), run_id: id.clone() };
    let guard = tokio::select! {
        biased;
        _ = app.shutdown.cancelled() => return Err(()),
        result = tokio::time::timeout(std::time::Duration::from_secs(3), app.api_bot_runtime.native_start_guard(&key)) => result.map_err(|_| ())?.map_err(|_| ())?,
    };
    let mut tx = crate::db::begin_write(&app.db).await.map_err(|_| ())?;
    let session: Option<Option<String>> = sqlx::query_scalar("SELECT owner_run_id FROM api_bot_sessions WHERE bot_id=? AND retired_at IS NULL")
        .bind(bot).fetch_optional(&mut *tx).await.map_err(|_| ())?;
    let resumed = match session {
        Some(Some(old)) if !cleared => { super::native_resume::prove_on(&mut tx, bot, &old, super::native_resume::ProofMode::StartBeforeRun).await.map_err(|_| ())?; true },
        None if !required => false,
        _ => return Err(()),
    };
    let inserted = sqlx::query("INSERT INTO runs(id,bot_id,state,agent_status,started_at,host) SELECT ?,b.id,'starting','unknown',?,'local' FROM bots b JOIN projects p ON p.id=b.project_id WHERE b.id=? AND b.kind='claude-api' AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.host='local' AND p.handed_off_to IS NULL AND b.managed_by!='child' AND NOT EXISTS(SELECT 1 FROM runs r WHERE r.bot_id=b.id AND r.state IN ('starting','running','stopping'))")
        .bind(&id).bind(crate::db::now()).bind(bot).execute(&mut *tx).await.map_err(|_| ())?;
    if inserted.rows_affected() != 1 { return Err(()); }
    if !matches!(crate::lifecycle::run_state::transition_on(&mut tx, &id, &["starting"], "running", Some("idle")).await, Ok(crate::lifecycle::run_state::Moved::Applied)) { return Err(()); }
    if app.shutdown.is_cancelled() { tx.rollback().await.map_err(|_| ())?; return Err(()); }
    if tx.commit().await.is_err() {
        let _ = sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM runs WHERE id=? AND bot_id=?)").bind(&id).bind(bot).fetch_one(&app.db).await;
        return Err(());
    }
    drop(guard);
    Ok((id, resumed))
}

#[cfg(all(test, feature = "daemon-test-harness"))]
fn pending_observers() -> &'static std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>> {
    static MAP: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>>> = std::sync::OnceLock::new();
    MAP.get_or_init(Default::default)
}
#[cfg(all(test, feature = "daemon-test-harness"))]
pub(super) fn observe_next_lock_pending(bot: &str) -> tokio::sync::oneshot::Receiver<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    assert!(pending_observers().lock().unwrap().insert(bot.to_owned(), tx).is_none());
    rx
}
