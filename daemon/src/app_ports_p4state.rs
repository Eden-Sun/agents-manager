//! P4 am-turn-state subgroup seam adapter (p4state).
//!
//! Provides narrow port implementations and helpers for the am-turn-state
//! modules (run_state, transitions, quota_hold, deferred_live, live_apply_debt,
//! owed_delivery, interrupt_grace, interruption, stuck_turns, restart_hold,
//! resume_gate, resume_nudge) so that direct dependencies on App, external modules
//! (reconcile, intents, quota, launch_rev, codex_history, herdr, etc.)
//! are mediated through narrow interfaces.

#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;
use crate::state::App;
use am_core::{BotId, EventEnvelope, EventSeq, PortError, TurnEvent, TurnId};
use am_ports::{BotLock, BotLockGuard, DbContext, EventSink, TurnEvents};

// ============================================================================
// Core Ports Implementations
// ============================================================================

/// Database context helper for SqlitePool.
pub fn db_context(app: &impl crate::capabilities::Db) -> DbContext<sqlx::SqlitePool> {
    DbContext::new(app.db().clone())
}

/// App-side adapter implementing `am_ports::BotLock`.
pub struct AppBotLock<'a, A> {
    app: &'a A,
}

impl<'a, A: crate::capabilities::BotLocks> AppBotLock<'a, A> {
    pub fn new(app: &'a A) -> Self {
        Self { app }
    }
}

struct ConcreteBotLockGuard(tokio::sync::OwnedMutexGuard<()>);
impl BotLockGuard for ConcreteBotLockGuard {}

impl<A: crate::capabilities::BotLocks> BotLock for AppBotLock<'_, A> {
    fn lock_bot<'a>(
        &'a self,
        bot: &'a BotId,
    ) -> impl Future<Output = Result<Box<dyn BotLockGuard + 'a>, PortError>> + Send + 'a {
        async move {
            let lock = self.app.bot_lock(bot).await;
            let guard = lock.lock_owned().await;
            Ok(Box::new(ConcreteBotLockGuard(guard)) as Box<dyn BotLockGuard + 'a>)
        }
    }
}

/// App-side adapter implementing `am_ports::EventSink`.
pub struct AppEventSink<'a, A> {
    app: &'a A,
}

impl<'a, A: crate::capabilities::Emit + crate::capabilities::BotStatusEmit> AppEventSink<'a, A> {
    pub fn new(app: &'a A) -> Self {
        Self { app }
    }
}

impl<A: crate::capabilities::Emit + crate::capabilities::BotStatusEmit> EventSink for AppEventSink<'_, A> {
    fn emit<'a>(
        &'a self,
        event: EventEnvelope,
    ) -> impl Future<Output = Result<EventSeq, PortError>> + Send + 'a {
        async move {
            let payload: serde_json::Value =
                serde_json::from_str(&event.payload_json).map_err(|error| {
                    PortError::InvalidInput(format!("event payload is not JSON: {error}"))
                })?;
            if !payload.is_object() {
                return Err(PortError::InvalidInput(
                    "event payload must be a JSON object".into(),
                ));
            }
            self.app.emit(&event.kind, payload).await;
            Ok(self.app.current_seq())
        }
    }

    fn bot_status_changed<'a>(
        &'a self,
        bot: &'a str,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            self.app.emit_bot_status(bot).await;
            Ok(())
        }
    }
}

/// App-side adapter implementing `am_ports::TurnEvents`.
pub struct AppTurnEvents<'a> {
    app: &'a Arc<App>,
}

impl<'a> AppTurnEvents<'a> {
    pub fn new(app: &'a Arc<App>) -> Self {
        Self { app }
    }

    async fn publish_turn_row(
        &self,
        turn_id: &str,
        delivery: Option<&str>,
    ) -> Result<(), PortError> {
        let row = sqlx::query_as::<_, (String, String, String)>(
            "SELECT c.bot_id, t.status, t.delivery FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE t.id=?",
        )
        .bind(turn_id)
        .fetch_optional(&self.app.db)
        .await
        .map_err(|error| PortError::Unavailable(error.to_string()))?;
        if let Some((bot_id, status, current_delivery)) = row {
            self.app.publish_turn(crate::state::TurnEvent {
                bot_id,
                turn_id: turn_id.to_string(),
                status,
                delivery: delivery.unwrap_or(&current_delivery).to_string(),
            });
        }
        Ok(())
    }
}

impl TurnEvents for AppTurnEvents<'_> {
    fn publish<'a>(
        &'a self,
        event: TurnEvent,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            match event {
                TurnEvent::Completed { run_id, turn_id, .. } => {
                    let turn_id = match turn_id {
                        Some(turn_id) => Some(turn_id),
                        None => sqlx::query_scalar::<_, String>(
                            "SELECT id FROM turns WHERE run_id=? ORDER BY created_at DESC, rowid DESC LIMIT 1",
                        )
                        .bind(run_id)
                        .fetch_optional(&self.app.db)
                        .await
                        .map_err(|error| PortError::Unavailable(error.to_string()))?,
                    };
                    if let Some(turn_id) = turn_id {
                        self.publish_turn_row(&turn_id, None).await?;
                    }
                }
                TurnEvent::DeliveryChanged { turn_id, state } => {
                    self.publish_turn_row(&turn_id, Some(&state)).await?;
                }
            }
            Ok(())
        }
    }

    fn turn_changed<'a>(
        &'a self,
        turn: &'a TurnId,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            crate::lifecycle::emit_turn(self.app, turn).await;
            Ok(())
        }
    }
}

// ============================================================================
// Narrow External Domain Adapters
// ============================================================================

// 1. Reconcile
pub async fn reconcile_host(app: &Arc<App>, host: &str) -> anyhow::Result<()> {
    crate::reconcile::reconcile_host(app, host).await.map(|_| ())
}

// 2. Lifecycle finish_stop
pub async fn finish_stop(app: &Arc<App>, run_id: &str) {
    crate::lifecycle::finish_stop(app, run_id).await;
}

// 3. Quota operations
pub async fn try_limit_hit_for_bot(
    app: &Arc<App>,
    bot: &crate::db::Bot,
) -> anyhow::Result<Option<crate::quota::LimitHit>> {
    crate::quota::try_limit_hit_for_bot(app, bot).await
}

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
    crate::quota::limit_cleared_since(app, bot, since).await
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
    app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables),
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

pub fn boot_id(app: &impl crate::capabilities::BootId) -> String {
    app.boot_id().to_string()
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

pub async fn emit_bot_changed(app: &impl crate::capabilities::Emit, bot_id: &str) {
    app.emit("bot_changed", serde_json::json!({ "bot_id": bot_id })).await;
}

// 5. Live Apply Debt / Launch Rev
pub async fn stamp_live_revision(pool: &sqlx::SqlitePool, run_id: &str) -> Result<bool, sqlx::Error> {
    crate::launch_rev::stamp_live_revision(pool, run_id).await
}

// 6. Turn Controller & Message insertion
pub async fn turn_fail_on(
    conn: &mut sqlx::SqliteConnection,
    turn_id: &str,
    delivery: crate::lifecycle::turn_controller::DeliveryOnFail,
    note: &str,
) -> anyhow::Result<crate::lifecycle::turn_controller::Outcome> {
    crate::lifecycle::turn_controller::fail_on(conn, turn_id, delivery, note).await
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_message_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    conv_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    origin: &str,
    incomplete: bool,
    sender: Option<&str>,
) -> anyhow::Result<crate::db::Message> {
    crate::lifecycle::insert_message_tx(tx, conv_id, turn_id, role, content, origin, incomplete, sender).await
}

pub async fn emit_message_added(app: &impl crate::capabilities::Emit, bot_id: &str, message: crate::db::Message) {
    crate::lifecycle::emit_message_added(app, bot_id, message).await
}

pub async fn emit_turn(app: &Arc<App>, turn_id: &str) {
    crate::lifecycle::emit_turn(app, turn_id).await
}

pub async fn mark_delivery(
    app: &Arc<impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit + 'static>,
    turn_id: &str,
    rec: crate::lifecycle::DeliveryRecord,
    delivered_at: &str,
) -> anyhow::Result<()> {
    crate::lifecycle::mark_delivery(app, turn_id, rec, delivered_at).await
}

pub async fn queue_put_back(
    app: &Arc<App>,
    bot_id: &str,
    conv: &str,
    turn_id: &str,
    reason: &str,
    wait_key: &str,
) -> anyhow::Result<()> {
    crate::lifecycle::put_back(app, bot_id, conv, turn_id, reason, wait_key).await
}

// 7. Transcript & Codex History
pub async fn local_transcript_allowed(app: &Arc<App>, bot: &crate::db::Bot, raw_path: &str) -> bool {
    crate::transcript_read::local_transcript_allowed(app, bot, raw_path).await
}

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
pub async fn herdr_client_for_run(app: &impl crate::capabilities::HerdrRoutes, run: &crate::db::Run) -> Option<crate::herdr::HerdrClient> {
    app.herdr_for_run(run).await
}

// 9. Stuck Turns helpers
pub async fn flush_queued_locked(
    app: &Arc<App>,
    bot_id: &str,
) -> anyhow::Result<()> {
    crate::lifecycle::flush_queued_locked(app, bot_id).await
}

// 10. Intents & Owner
pub async fn open_restart_intents(pool: &sqlx::SqlitePool) -> anyhow::Result<Vec<crate::intents::Intent>> {
    crate::intents::open(pool).await
}

pub fn owner_id(app: &impl crate::capabilities::DataDir) -> String {
    app.data_dir().display().to_string()
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
    model: Option<&str>,
    relay: crate::lifecycle::RelaySrc<'_>,
) -> crate::lifecycle::LcResult<crate::lifecycle::PromptOut> {
    crate::lifecycle::queue_for_next_turn(app, conv, bot_id, text, plain_text, client_request_id, model, relay).await
}

pub fn schedule_flush_queued(app: &Arc<App>, bot_id: &str) {
    crate::lifecycle::schedule_flush_queued(app, bot_id);
}

pub async fn identity_config_dir(app: &Arc<impl crate::hosts::HostsAccess + crate::tools::ToolsEnv + 'static>, host: &str, identity: Option<&str>) -> anyhow::Result<String> {
    crate::lifecycle::start::identity_config_dir(app, host, identity).await
}
