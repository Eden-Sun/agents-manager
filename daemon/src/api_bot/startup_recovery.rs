//! Inert durable-receipt recovery core. D3b must provide exclusive boot ordering before callers exist.
//! A checkpoint or an empty registry is never completion evidence.
use super::{
    completion, receipt,
    runtime::{StartupCheckpoint, StartupError, StartupRestore},
};
use crate::lifecycle::api_bot_port::ApiTurnKey;
use std::sync::Arc;

pub(super) enum Error {
    Bounds,
    NotStartup,
    Storage,
    Unresolved,
}
impl Error {
    pub(super) fn code(&self) -> &'static str {
        match self {
            Self::Bounds => "api_startup_bounds",
            Self::NotStartup => "api_startup_not_exclusive",
            Self::Storage => "api_startup_storage",
            Self::Unresolved => "api_startup_unresolved",
        }
    }
}
/// Numbers only; deliberately no Debug/Serialize, as with the enclosing private page/cursor.
#[derive(Default)]
pub(super) struct Counts {
    pub(super) scanned: usize,
    pub(super) applied: usize,
    pub(super) already_applied: usize,
    pub(super) restored: usize,
    pub(super) already_current: usize,
    pub(super) unresolved: usize,
    pub(super) fenced: usize,
}
pub(super) struct Page {
    pub(super) counts: Counts,
    pub(super) next: Option<receipt::StartupCursor>,
}

pub(super) fn recover_receipt_page<'a>(
    app: &'a Arc<crate::state::App>,
    after: Option<&'a receipt::StartupCursor>,
    limit: usize,
) -> impl std::future::Future<Output = Result<Page, Error>> + Send + 'a {
    Box::pin(recover_page_body(app, after, limit, true))
}
async fn recover_page_body(
    app: &Arc<crate::state::App>,
    after: Option<&receipt::StartupCursor>,
    limit: usize,
    restore: bool,
) -> Result<Page, Error> {
    if app.is_startup_ready() {
        return Err(Error::NotStartup);
    }
    let page = receipt::startup_page(&app.db, after, limit)
        .await
        .map_err(|e| match e {
            receipt::Error::Bounds => Error::Bounds,
            _ => Error::Storage,
        })?;
    let mut counts = Counts::default();
    for stored in page.rows {
        counts.scanned += 1;
        let item = stored.item;
        let guard = match app.api_bot_runtime.startup_guard(app, &item.identity).await {
            Ok(guard) => guard,
            Err(StartupError::Busy) => {
                counts.unresolved += 1;
                continue;
            }
            Err(StartupError::NotStartup) => return Err(Error::NotStartup),
            Err(StartupError::Storage) => return Err(Error::Storage),
        };
        // Terminal Applied history is read-only evidence for skipping this artifact only.
        if !restore && matches!(stored.state, receipt::State::Applied) {
            match super::startup_history::classify(&app.db, &item).await? {
                super::startup_history::StartupHistoricalApplied::Skip => continue,
                super::startup_history::StartupHistoricalApplied::Unresolved => { counts.unresolved += 1; continue; },
                super::startup_history::StartupHistoricalApplied::Live => {},
            }
        }
        match app
            .api_bot_runtime
            .startup_checkpoint_status(app, &item.identity, item.revision, &guard)
            .await
            .map_err(|_| Error::Storage)?
        {
            StartupCheckpoint::Unresolved => {
                counts.unresolved += 1;
                continue;
            }
            StartupCheckpoint::Fenced => {
                if !restore {
                    counts.unresolved += 1;
                    continue;
                }
                if matches!(stored.state, receipt::State::Pending) {
                    receipt::fence_stale(&app.db, &item)
                        .await
                        .map_err(|_| Error::Storage)?;
                }
                counts.fenced += 1;
                continue;
            }
            StartupCheckpoint::Safe => {}
        }
        let key = ApiTurnKey {
            bot_id: item.identity.bot_id.clone(),
            run_id: item.identity.run_id.clone(),
            turn_id: item.identity.turn_id.clone(),
        };
        let confirmed = match stored.state {
            receipt::State::Pending => match completion::consume_receipt(app, &key, &item).await {
                Ok(completion::Outcome::Committed(_)) => {
                    counts.applied += 1;
                    true
                }
                Ok(completion::Outcome::Fenced) => {
                    counts.fenced += 1;
                    false
                }
                Err(completion::StoreError::Retryable) => return Err(Error::Storage),
                Err(completion::StoreError::Ambiguous) => {
                    confirm_applied(app, &item, &mut counts).await?
                }
            },
            receipt::State::Applied => confirm_applied(app, &item, &mut counts).await?,
            receipt::State::Fenced => {
                counts.fenced += 1;
                false
            }
        };
        if !confirmed || !restore {
            continue;
        }
        match app
            .api_bot_runtime
            .restore_startup_cursor(app, &item.identity, item.revision, &guard)
            .await
            .map_err(|_| Error::Storage)?
        {
            StartupRestore::Restored => counts.restored += 1,
            StartupRestore::AlreadyCurrent => counts.already_current += 1,
            StartupRestore::Fenced => counts.fenced += 1,
            StartupRestore::Unresolved => counts.unresolved += 1,
        }
    }
    Ok(Page {
        counts,
        next: page.next,
    })
}
async fn confirm_applied(
    app: &Arc<crate::state::App>,
    item: &receipt::Pending,
    counts: &mut Counts,
) -> Result<bool, Error> {
    match completion::probe_after_join(app, &item.identity, &item.payload)
        .await
        .map_err(|_| Error::Storage)?
    {
        completion::Probe::Applied(_) => {
            counts.already_applied += 1;
            Ok(true)
        }
        completion::Probe::Stale(_) => {
            receipt::fence_stale(&app.db, item)
                .await
                .map_err(|_| Error::Storage)?;
            counts.fenced += 1;
            Ok(false)
        }
        _ => {
            counts.unresolved += 1;
            Ok(false)
        }
    }
}

/// Inert boot candidate: receipt processing without installing a runtime cursor.
pub(super) async fn recover_all(app: &Arc<crate::state::App>) -> Result<(), Error> {
    if app.is_startup_ready() {
        return Err(Error::NotStartup);
    }
    let mut after = None;
    loop {
        let page = recover_page_body(app, after.as_ref(), 64, false).await?;
        if page.counts.unresolved != 0 || page.counts.fenced != 0 {
            return Err(Error::Unresolved);
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    super::startup_abandonment::abandon_all(app).await
}
