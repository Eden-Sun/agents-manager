//! P13 Provider／login／preview seam adapter.
//!
//! Provides narrow port helpers for provider, login, and preview modules so they do not
//! directly depend on lifecycle, supervisor, api, or share internals.

#![allow(dead_code)]

use std::sync::Arc;
use crate::state::App;
pub(crate) use crate::lifecycle::{LcError, LcResult};

/// Constant-time string equality helper.
pub fn ct_eq(a: &str, b: &str) -> bool {
    crate::api::ct_eq(a, b)
}

/// Checks if a bot principal is forbidden from spawning panes (restricted share bot).
pub async fn refuses_bot_principal(pool: &sqlx::SqlitePool, bot_id: &str) -> bool {
    crate::share::refuses_bot_principal(pool, bot_id).await
}

/// Notes the purpose of a created pane on the host.
pub async fn note_purpose(
    app: &Arc<App>,
    host: &str,
    pane_id: &str,
    bot: &crate::db::Bot,
    purpose: &str,
) -> anyhow::Result<()> {
    crate::panes::note_purpose(app, host, pane_id, bot, purpose).await
}

/// Emits a host-changed event using the given host fence.
pub async fn emit_host_changed(app: &Arc<App>, fence: &crate::hosts::HostFence) {
    crate::state::emit_host_changed(app, fence).await;
}

/// Reads shell screen text for login assistance.
pub async fn shell_read(
    app: &Arc<App>,
    host: &str,
    pane_id: &str,
    source: &str,
    lines: u32,
) -> LcResult<serde_json::Value> {
    crate::api::shell::read(app, host, pane_id, source, lines).await
}

/// Sends text to a login shell pane.
pub async fn shell_send_text(
    app: &Arc<App>,
    host: &str,
    pane_id: &str,
    text: &str,
    enter: bool,
) -> LcResult<()> {
    crate::api::shell::send_text(app, host, pane_id, text, enter).await
}

/// Closes a pane and its enclosing tab if empty.
pub async fn close_pane_and_tab(
    client: &crate::herdr::HerdrClient,
    workspace_id: Option<&str>,
    tab_id: Option<&str>,
    pane_id: &str,
) {
    crate::lifecycle::close_pane_and_tab(client, workspace_id, tab_id, pane_id).await;
}

/// Validates that a preview is not being started in the default session.
pub fn refuse_default_session(bot: &crate::db::Bot) -> LcResult<()> {
    crate::lifecycle::refuse_default_session(bot)
}

/// Inserts a message into a conversation.
#[allow(clippy::too_many_arguments)]
pub async fn insert_message(
    app: &Arc<App>,
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    author: &str,
    incomplete: bool,
    snapshot: Option<&str>,
) -> anyhow::Result<crate::db::Message> {
    crate::lifecycle::insert_message(app, conversation_id, turn_id, role, content, author, incomplete, snapshot).await
}

/// Schedules a flush of queued messages for a bot.
pub fn schedule_flush_queued(app: &Arc<App>, bot_id: &str) {
    crate::lifecycle::schedule_flush_queued(app, bot_id);
}

/// Clears child alert notices for a bot.
pub fn child_alerts_forget(bot_id: &str) {
    crate::child_alerts::forget(bot_id);
}

/// Reads the scheduled flush count in tests.
#[cfg(test)]
pub fn take_scheduled_flush_count(bot_id: &str) -> usize {
    crate::lifecycle::take_scheduled_flush_count(bot_id)
}

/// Resolves the local Codex home directory.
pub async fn codex_home(app: &Arc<App>, bot: &crate::db::Bot) -> Option<std::path::PathBuf> {
    crate::lifecycle::codex_home(app, bot).await
}

/// Follows a runtime change into child bot settings.
pub async fn child_runtime_follow(
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit),
    bot_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> anyhow::Result<()> {
    crate::child_runtime::follow(app, bot_id, model, effort).await.map_err(Into::into)
}

/// Reads styled pane snapshot as text.
pub async fn read_styled(
    client: &crate::herdr::HerdrClient,
    pane_id: &str,
    source: &str,
    lines: u32,
) -> anyhow::Result<String> {
    crate::lifecycle::read_styled(client, pane_id, source, lines).await
}

/// Reads styled pane snapshot structured record.
pub async fn read_styled_snapshot(
    client: &crate::herdr::HerdrClient,
    pane_id: &str,
    source: &str,
    lines: u32,
) -> anyhow::Result<crate::herdr::PaneRead> {
    crate::lifecycle::read_styled_snapshot(client, pane_id, source, lines).await
}

pub(crate) use crate::lifecycle::BoxState;

/// Evaluates prompt box state.
pub fn box_state(kind: &str, text: &str) -> BoxState {
    crate::lifecycle::box_state(kind, text)
}

#[cfg(test)]
pub mod race_point {
    use std::future::Future;

    pub async fn hit(point: &'static str, key: &str) {
        crate::lifecycle::race_point::hit(point, key).await;
    }

    pub fn arm<F, Fut>(point: &'static str, key: &str, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        crate::lifecycle::race_point::arm(point, key, f);
    }
}

// Supervisor delegation helpers for claude_review
pub const SUPERVISOR_ID: &str = crate::supervisor::store::SUPERVISOR_ID;
pub const OPEN_STATES: [&str; 6] = crate::supervisor::store::OPEN_STATES;
pub type Assignment = crate::supervisor::store::Assignment;
pub type Supervisor = crate::supervisor::store::Supervisor;
pub type ReplyMark<'a> = crate::supervisor::bot_requests::ReplyMark<'a>;

/// Resolves the responder bot from the database.
pub async fn responder_bot(pool: &sqlx::SqlitePool) -> anyhow::Result<Option<crate::db::Bot>> {
    crate::supervisor::roles::responder_bot(pool).await
}

/// Dispatches an assignment to the supervisor.
#[allow(clippy::too_many_arguments)]
pub async fn supervisor_assign(
    app: &Arc<App>,
    target_bot_id: &str,
    text: &str,
    client_request_id: &str,
    source_turn_id: Option<&str>,
    ownership: &[String],
    follow_up_of: Option<&str>,
    expects_review: bool,
    mission: Option<(&str, &str)>,
    review_role: Option<crate::supervisor::roles::Role>,
    actor: Option<crate::supervisor::roles::Role>,
    reply: ReplyMark<'_>,
) -> Result<serde_json::Value, LcError> {
    crate::supervisor::assign(
        app,
        target_bot_id,
        text,
        client_request_id,
        source_turn_id,
        ownership,
        follow_up_of,
        expects_review,
        mission,
        review_role,
        actor,
        reply,
    )
    .await
}

/// Finds an assignment by its client request ID.
pub async fn assignment_by_crid(
    pool: &sqlx::SqlitePool,
    crid: &str,
) -> anyhow::Result<Option<Assignment>> {
    crate::supervisor::store::assignment_by_crid(pool, crid).await
}

/// Finds an assignment by ID.
pub async fn assignment(
    pool: &sqlx::SqlitePool,
    id: &str,
) -> anyhow::Result<Option<Assignment>> {
    crate::supervisor::store::assignment(pool, id).await
}

/// Initializes or retrieves the supervisor singleton row.
pub async fn supervisor_get_or_init(
    pool: &sqlx::SqlitePool,
) -> anyhow::Result<Supervisor> {
    crate::supervisor::store::get_or_init(pool).await
}

#[cfg(test)]
pub fn supervisor_event_key(from: &str, crid: Option<&str>, fingerprint: &str, anchor_unix: i64) -> String {
    crate::supervisor::bot_requests::event_key(from, crid, fingerprint, anchor_unix)
}

#[cfg(test)]
pub async fn supervisor_push_inbox(
    pool: &sqlx::SqlitePool,
    event_key: &str,
    kind: &str,
    bot_id: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    payload: &serde_json::Value,
) -> anyhow::Result<Option<String>> {
    crate::supervisor::store::push_inbox(pool, event_key, kind, bot_id, from, to, payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq("tok_xyz", "tok_xyz"));
        assert!(!ct_eq("tok_xyz", "tok_abc"));
    }

    #[test]
    fn box_state_delegates() {
        assert_eq!(box_state("codex", ""), BoxState::Unready);
    }
}
