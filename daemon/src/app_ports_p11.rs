//! P11 Bot/project operations seam adapter.
//!
//! Provides narrow port helpers for bot and project operations (intents,
//! trash, promote, panes, rewind, fork, etc.) so they do not directly
//! depend on lifecycle, share, reconcile, api, or supervisor internals.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;
use crate::state::App;
use crate::db;
use crate::lifecycle::{self, LcError};
use serde_json::Value;

// ============================================================================
// Reconcile Ports
// ============================================================================

/// Backoff retry delay for recovery / intents loops.
pub fn recovery_retry_delay(attempt: usize) -> Duration {
    crate::reconcile::recovery_retry_delay(attempt)
}

// ============================================================================
// Share Ports
// ============================================================================

/// Revokes sharing for a single bot.
pub async fn revoke_bot_share(app: &Arc<App>, bot_id: &str) -> Result<(), sqlx::Error> {
    crate::share::revoke_bot_share(app, bot_id).await
}

/// Revokes all shares belonging to a project.
pub async fn revoke_project_shares(app: &Arc<App>, project_id: &str) -> Result<(), sqlx::Error> {
    crate::share::revoke_project_shares(app, project_id).await
}

// ============================================================================
// Lifecycle Ports
// ============================================================================

/// Closes a herdr pane and its parent tab.
pub async fn close_pane_and_tab(
    client: &crate::herdr::HerdrClient,
    workspace_id: Option<&str>,
    tab_id: Option<&str>,
    pane_id: &str,
) {
    lifecycle::close_pane_and_tab(client, workspace_id, tab_id, pane_id).await;
}

/// Purges bot directory (local or remote).
pub async fn purge_bot_dir(app: &Arc<App>, bot_id: &str, host: &str) -> bool {
    lifecycle::purge_bot_dir(app, bot_id, host).await
}

/// Checks if a run is sitting in default herdr session.
pub fn in_default_session(run: &crate::db::Run) -> bool {
    lifecycle::in_default_session(run)
}

/// Resolves remote bot directory info.
pub async fn remote_bot_dir(
    conn: &crate::hosts::HostConn,
    bot_id: &str,
) -> anyhow::Result<crate::lifecycle::RemoteHookPaths> {
    lifecycle::remote_bot_dir(conn, bot_id).await
}

// ============================================================================
// API Ports
// ============================================================================

/// Acquires bot locks in deterministic sorted order.
pub async fn lock_bots_in_order(
    app: &Arc<App>,
    ids: Vec<String>,
) -> (Vec<String>, Vec<tokio::sync::OwnedMutexGuard<()>>) {
    crate::api::lock_bots_in_order(app, ids).await
}

/// Soft deletes a child bot and revokes its share.
pub async fn soft_delete_child(app: &Arc<App>, bot_id: &str) -> Result<(), sqlx::Error> {
    crate::api::soft_delete_child(app, bot_id).await
}

/// Stops a bot while holding its lock during deletion.
pub async fn stop_for_delete_locked(app: &Arc<App>, bot_id: &str) -> Result<(), &'static str> {
    crate::api::stop_for_delete_locked(app, bot_id).await
}

/// Lists live bot names for a project.
pub async fn live_bot_names(pool: &sqlx::SqlitePool, project_id: &str) -> Result<Vec<String>, sqlx::Error> {
    crate::api::live_bot_names(pool, project_id).await
}

/// Generates next free name with numeric suffix if taken.
pub fn next_free_name(wanted: &str, taken: &dyn Fn(&str) -> bool) -> String {
    crate::api::next_free_name(wanted, taken)
}

/// Gets shell client for a host.
pub async fn client_for(
    app: &Arc<App>,
    host: &str,
) -> Result<(crate::herdr::HerdrClient, String), LcError> {
    crate::api::shell::client_for(app, host).await
}

/// Finds all descendant children of a bot.
pub async fn descendant_children(app: &Arc<App>, root: &str) -> anyhow::Result<Vec<db::Bot>> {
    crate::api::descendant_children(app, root).await
}

// ============================================================================
// Supervisor Ports
// ============================================================================

pub const RELEASED_REASON: &str = crate::supervisor::maintenance::RELEASED_REASON;
pub const FORCE_RELEASED_REASON: &str = crate::supervisor::maintenance::FORCE_RELEASED_REASON;

pub fn waited_secs(updated_at: &str, now: &str) -> i64 {
    crate::supervisor::maintenance::waited_secs(updated_at, now)
}

pub async fn supervisor_lock() -> crate::supervisor::OpGuard {
    crate::supervisor::lock().await
}

pub async fn push_inbox(
    pool: &sqlx::SqlitePool,
    key: &str,
    event: &str,
    assignment_id: Option<&str>,
    bot_id: Option<&str>,
    run_id: Option<&str>,
    payload: &Value,
) -> anyhow::Result<Option<String>> {
    crate::supervisor::store::push_inbox(pool, key, event, assignment_id, bot_id, run_id, payload).await
}

pub async fn push_inbox_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    key: &str,
    event: &str,
    assignment_id: Option<&str>,
    bot_id: Option<&str>,
    run_id: Option<&str>,
    payload: &Value,
) -> anyhow::Result<Option<String>> {
    crate::supervisor::store::push_inbox_tx(tx, key, event, assignment_id, bot_id, run_id, payload).await
}

// ============================================================================
// Test Support / Race Points
// ============================================================================

#[cfg(test)]
pub mod race_point {
    use std::future::Future;

    pub fn arm<F, Fut>(point: &'static str, key: &str, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        crate::lifecycle::race_point::arm(point, key, f);
    }

    pub async fn hit(point: &'static str, key: &str) {
        crate::lifecycle::race_point::hit(point, key).await;
    }
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;

    pub async fn restore_bot(app: Arc<App>, bot_id: String) -> Result<axum::response::Response, LcError> {
        crate::api::restore_bot(axum::extract::State(app), axum::extract::Path(bot_id)).await
    }

    pub async fn reconcile_host(app: &Arc<App>, host: &str) -> anyhow::Result<()> {
        crate::reconcile::reconcile_host(app, host).await
    }

    pub async fn autostart_after_reconcile(app: &Arc<App>, host: &str, caught_up: bool) {
        crate::reconcile::autostart_after_reconcile(app, host, caught_up).await;
    }

    pub async fn supervisor_store_inbox(pool: &sqlx::SqlitePool, limit: i64) -> anyhow::Result<Vec<crate::supervisor::store::InboxEvent>> {
        crate::supervisor::store::inbox(pool, limit).await
    }

    pub async fn supervisor_store_get_or_init(pool: &sqlx::SqlitePool) -> anyhow::Result<crate::supervisor::store::Supervisor> {
        crate::supervisor::store::get_or_init(pool).await
    }

    pub async fn supervisor_store_set_desired_running(pool: &sqlx::SqlitePool, running: bool) -> anyhow::Result<()> {
        crate::supervisor::store::set_desired_running(pool, running).await
    }

    pub const SUPERVISOR_ID: &str = crate::supervisor::store::SUPERVISOR_ID;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_free_name_increments() {
        let taken = |n: &str| n == "bot" || n == "bot-1";
        assert_eq!(next_free_name("bot", &taken), "bot-2");
    }

    #[test]
    fn recovery_retry_delay_returns_duration() {
        let d = recovery_retry_delay(0);
        assert!(d.as_millis() > 0);
    }
}
