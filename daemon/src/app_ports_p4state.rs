//! P4 am-turn-state subgroup seam adapter (p4state).
//!
//! Provides narrow port implementations and helpers for the am-turn-state
//! modules (run_state, transitions, quota_hold, deferred_live, live_apply_debt,
//! owed_delivery, interrupt_grace, interruption, stuck_turns, restart_hold,
//! resume_gate, resume_nudge) so that direct dependencies on App, external modules
//! (reconcile, intents, quota, launch_rev, codex_history, herdr, etc.)
//! are mediated through narrow interfaces.

use std::sync::Arc;
use crate::state::App;

// ============================================================================
// Narrow External Domain Adapters
// ============================================================================

// 1. Reconcile
pub async fn reconcile_host(app: &Arc<App>, host: &str) -> anyhow::Result<()> {
    crate::runners::reconcile::reconcile_host(app, host).await.map(|_| ())
}

// 2. Lifecycle finish_stop
pub async fn finish_stop(app: &Arc<App>, run_id: &str) {
    crate::lifecycle::finish_stop(app, run_id).await;
}

// 3. Quota operations
pub async fn running_model(app: &impl crate::capabilities::Db, bot: &crate::db::Bot) -> Option<String> {
    crate::quota::running_model(app, bot).await
}

pub async fn billing_identity(
    app: &impl crate::capabilities::Db,
    bot: &crate::db::Bot,
) -> anyhow::Result<Option<String>> {
    crate::quota::billing_identity(app, bot).await
}

pub async fn limit_cleared_since(
    app: &Arc<App>,
    bot: &crate::db::Bot,
    since: chrono::DateTime<chrono::Utc>,
) -> bool {
    crate::runners::quota::limit_cleared_since(app, bot, since).await
}

pub async fn quota_base_for_host(
    app: &Arc<App>,
    host: &str,
    kind: &str,
    identity: Option<&str>,
) -> String {
    crate::quota::quota_base_for_host(app, host, kind, identity).await
}

pub async fn restore_limit_hit(
    app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables),
    host: &str,
    base: &str,
    hit: crate::quota::LimitHit,
) -> bool {
    crate::quota::restore_limit_hit(app, host, base, hit).await
}

pub async fn host_target(app: &impl crate::hosts::HostsAccess, host: &str) -> Option<String> {
    if host == crate::config::LOCAL_HOST {
        return Some(host.to_string());
    }
    let conn = app.hosts().get(host).await?;
    let cfg = conn.cfg.as_ref()?;
    Some(format!("{}:{}/{}", cfg.ssh, cfg.ssh_port, cfg.herdr_session))
}

// 4. Deferred Live / Live Apply
pub async fn apply_live_setting_with_revision(
    app: &Arc<App>,
    bot_id: &str,
    fields: &[&'static str],
    baseline_rev: &str,
    target_rev: &str,
) -> crate::lifecycle::LiveApplyOutcome {
    crate::lifecycle::apply_live_setting_with_revision(app, bot_id, fields, baseline_rev, target_rev).await
}

// 5. Live Apply Debt / Launch Rev
pub async fn stamp_live_revision(pool: &sqlx::SqlitePool, run_id: &str) -> Result<bool, sqlx::Error> {
    crate::launch_rev::stamp_live_revision(pool, run_id).await
}

// 6. Turn Controller & Message insertion
// 7. Transcript & Codex History
pub async fn codex_home(app: &Arc<impl crate::tools::ToolsEnv + 'static>, bot: &crate::db::Bot) -> Option<std::path::PathBuf> {
    crate::lifecycle::codex_home(app, bot).await
}

pub async fn codex_interrupted_after(
    app: &Arc<App>,
    bot: &crate::db::Bot,
    run: &crate::db::Run,
    sent: &[String],
) -> bool {
    crate::codex_history::interrupted_after(app, bot, run, sent).await
}

// 8. Herdr client for run
// 9. Stuck Turns helpers
// 10. Intents & Owner
pub async fn open_restart_intents(pool: &sqlx::SqlitePool) -> anyhow::Result<Vec<crate::intents::Intent>> {
    crate::intents::open(pool).await
}

// 11. Resume nudge helpers
#[allow(clippy::too_many_arguments)]
pub async fn queue_for_next_turn(
    app: &Arc<App>,
    conv: &str,
    bot_id: &str,
    text: &str,
    plain_text: &str,
    client_request_id: &str,
    relay: crate::lifecycle::RelaySrc<'_>,
) -> crate::lifecycle::LcResult<crate::lifecycle::PromptOut> {
    crate::lifecycle::queue_for_next_turn(app, conv, bot_id, text, plain_text, client_request_id, relay).await
}

pub fn schedule_flush_queued(app: &Arc<App>, bot_id: &str) {
    crate::lifecycle::schedule_flush_queued(app, bot_id);
}

pub async fn identity_config_dir(app: &(impl crate::hosts::HostsAccess + crate::lifecycle::s6_ports::IdentityAccess), host: &str, identity: Option<&str>) -> anyhow::Result<String> {
    crate::lifecycle::start::identity_config_dir(app, host, identity).await
}
