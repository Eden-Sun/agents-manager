//! Startup-only retention after successful exclusive recovery.
use std::{future::Future, sync::Arc};

pub(super) fn prune(app: &Arc<crate::state::App>) -> impl Future<Output = Result<(), &'static str>> + Send + '_ {
    Box::pin(async move {
        match crate::db::api_bot::prune_unless_unresolved(&app.db).await {
            Ok(crate::db::api_bot::StartupPrune::Pruned(_)) => Ok(()),
            Ok(crate::db::api_bot::StartupPrune::Blocked) => Err("api_startup_retention_unresolved"),
            Err(_) => Err("api_startup_retention_storage"),
        }
    })
}
