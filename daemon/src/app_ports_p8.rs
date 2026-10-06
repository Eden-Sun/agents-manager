//! P8 hook／事件／對帳的窄介面由 `App`／`SqlitePool`／交易連線實作的地方（crate 拆分第 3 步）。每個方法逐行委派給入口與對帳原本呼叫
//! 的函式，不加任何邏輯、不多一個 await、不改鎖的範圍，所以行為與錯誤型別不變（重送、同一回合鎖、耐久事件的「只發生一次」
//! 都留在被委派的函式裡）。介面在 `ingress_ports.rs`；這個檔案是 P8 唯一還知道 lifecycle／quota／supervisor／panes… 實作細節的地方
//! （composition root 側的 adapter，不屬於未來的 am-ingress／am-reconcile）。
//!
//! 模組掛在 `events.rs`（`#[path]`），不碰 `lib.rs`。

use crate::db;
use crate::handoff::Footprint;
use crate::herdr_maintenance::Window;
use crate::events::ports::{
    ApiPort, BotOpsPort, BotOpsRepo, HandoffConnRepo, HandoffRepo, HostSidePort, IngressCommands, IntentConnRepo, MessageTxOps,
    ProviderPort, QuotaCommands, ReconcileCommands, SupervisorRepo, SupervisorSignals, TurnCommands, TurnConnOps, TurnFenceOps,
};
use crate::lifecycle::fence::{Admitted, EventIdentity, Ownership};
use crate::lifecycle::turn_controller::{self, NativeEvidence, Outcome};
use crate::lifecycle::{self, InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
use crate::panes::ScanOutcome;
use crate::quota::{self, Quota};
use crate::state::App;
use anyhow::Result;
use serde_json::Value;
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

impl TurnCommands for Arc<App> {
    async fn emit_message_added(&self, bot_id: &str, message: db::Message) {
        lifecycle::emit_message_added(self, bot_id, message).await
    }
    async fn emit_turn(&self, turn_id: &str) {
        lifecycle::emit_turn(self, turn_id).await
    }
    fn schedule_flush_queued(&self, bot_id: &str) {
        lifecycle::schedule_flush_queued(self, bot_id)
    }
    fn schedule_deferred_live(&self, bot_id: &str) {
        lifecycle::schedule_deferred_live(self, bot_id)
    }
    fn schedule_codex_notice_capture(&self, bot_id: &str, run_id: &str) {
        lifecycle::schedule_codex_notice_capture(self, bot_id, run_id)
    }
    fn poke_resume_nudge(&self, bot_id: &str) {
        lifecycle::poke_resume_nudge(self, bot_id)
    }
    async fn cancel_stall(&self, run_id: &str) {
        lifecycle::cancel_stall(self, run_id).await
    }
    async fn arm_fallback(&self, run_id: &str, bot_id: &str) {
        lifecycle::arm_fallback(self, run_id, bot_id).await
    }
    async fn arm_progress(&self, run_id: &str, bot_id: &str, turn_id: &str) {
        lifecycle::arm_progress(self, run_id, bot_id, turn_id).await
    }
    async fn arm_stall(&self, run_id: &str, bot_id: &str, turn_id: &str) {
        lifecycle::arm_stall(self, run_id, bot_id, turn_id).await
    }
    async fn begin_external_turn(&self, run: &db::Run) {
        lifecycle::begin_external_turn(self, run).await
    }
    async fn mark_run_exited(&self, run_id: &str, reason: &str) -> RunExit {
        lifecycle::mark_run_exited(self, run_id, reason).await
    }
    async fn context_lost(&self, bot: &db::Bot, why: &str, failed_session: Option<&str>) -> LcResult<()> {
        lifecycle::context_lost(self, bot, why, failed_session).await
    }
    async fn retire_context_lost(&self, bot_id: &str, session_id: &str) {
        lifecycle::retire_context_lost(self, bot_id, session_id).await
    }
    async fn settle_interruption(&self, bot_id: &str, evidence: InterruptEvidence) -> Result<()> {
        lifecycle::settle_interruption(self, bot_id, evidence).await
    }
    async fn settle_owed_deliveries(&self, bot_id: &str) -> Result<()> {
        lifecycle::settle_owed_deliveries(self, bot_id).await
    }
    async fn settle_interrupt_echo(
        &self,
        bot_id: &str,
        run_id: &str,
        ev: &InterruptFailureEvidence<'_>,
        in_flight: Option<&db::Turn>,
    ) -> Result<bool> {
        lifecycle::settle_interrupt_echo(self, bot_id, run_id, ev, in_flight).await
    }
    async fn start_bot(&self, bot_id: &str) -> LcResult<String> {
        lifecycle::start_bot(self, bot_id).await
    }
    async fn start_bot_locked_with(&self, bot_id: &str, opts: StartOpts) -> LcResult<String> {
        lifecycle::start_bot_locked_with(self, bot_id, opts).await
    }
    async fn resume_after_boot(&self, host: &str) -> usize {
        lifecycle::start_send::resume_after_boot(self, host).await
    }
    async fn adopt_unbound_send_nows(&self, boot: &str) -> bool {
        lifecycle::adopt_unbound_send_nows(self, boot).await
    }
    async fn rearm_queue_retries(&self) -> Result<usize> {
        lifecycle::rearm_queue_retries(self).await
    }
    async fn adopt_turns_of_ended_runs(&self, boot: &str) -> bool {
        lifecycle::adopt_turns_of_ended_runs(self, boot).await
    }
    async fn rearm_queued_prompt_restamps(&self) -> Result<()> {
        lifecycle::rearm_queued_prompt_restamps(self).await
    }
    async fn adopt_interrupted_on_restart(&self, run: &db::Run, turn: &db::Turn) -> Result<bool> {
        lifecycle::adopt_interrupted_on_restart(self, run, turn).await
    }
    fn spawn_adopted_capture(&self, run_id: &str, bot_id: &str) {
        lifecycle::spawn_adopted_capture(self, run_id, bot_id)
    }
    async fn sweep_stuck_turns(&self, host: Option<&str>) -> Vec<String> {
        lifecycle::sweep_stuck_turns(self, host).await
    }
    async fn close_after_session_paused(&self, run_id: &str, expected_turn_id: &str) -> Option<String> {
        lifecycle::close_after_session_paused(self, run_id, expected_turn_id).await
    }
    async fn close_pane_and_tab(&self, client: &crate::herdr::HerdrClient, workspace_id: Option<&str>, tab_id: Option<&str>, pane_id: &str) {
        lifecycle::close_pane_and_tab(client, workspace_id, tab_id, pane_id).await
    }
    fn observe_agent_status(&self, run_id: &str, agent_status: &str) {
        lifecycle::observe_agent_status(run_id, agent_status)
    }
    async fn insert_message(
        &self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
    ) -> Result<db::Message> {
        lifecycle::insert_message(self, conversation_id, turn_id, role, content, source, incomplete, snapshot).await
    }
    async fn prompt_relayed_queueable(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>) -> LcResult<PromptOut> {
        lifecycle::prompt_relayed_queueable(self, bot_id, text, client_request_id, relay_from).await
    }
}

impl TurnConnOps for SqliteConnection {
    async fn set_status_on(&mut self, turn_id: &str, from: &str, to: &str, why: &str) -> Result<Outcome> {
        turn_controller::set_status_on(self, turn_id, from, to, why).await
    }
    async fn complete_with_native_evidence(&mut self, turn_id: &str, admitted: &Admitted, ev: NativeEvidence<'_>) -> Result<Outcome> {
        turn_controller::complete_with_native_evidence(self, turn_id, admitted, ev).await
    }
    async fn fail_with_native_evidence(&mut self, turn_id: &str, admitted: &Admitted, ev: NativeEvidence<'_>) -> Result<Outcome> {
        turn_controller::fail_with_native_evidence(self, turn_id, admitted, ev).await
    }
}

impl MessageTxOps for Transaction<'_, Sqlite> {
    async fn insert_message_tx(
        &mut self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
    ) -> Result<db::Message> {
        lifecycle::insert_message_tx(self, conversation_id, turn_id, role, content, source, incomplete, snapshot).await
    }
    async fn insert_message_relayed_tx(
        &mut self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
        relay_from: Option<&str>,
    ) -> Result<db::Message> {
        lifecycle::insert_message_relayed_tx(self, conversation_id, turn_id, role, content, source, incomplete, snapshot, relay_from).await
    }
}

impl TurnFenceOps for SqlitePool {
    async fn classify_event_owner(&self, bot_id: &str, run: &db::Run, ev: EventIdentity<'_>) -> Ownership {
        lifecycle::fence::classify(self, bot_id, run, ev).await
    }
}

impl QuotaCommands for Arc<App> {
    async fn clear_limit_hit_for_bot(&self, bot: &db::Bot) {
        quota::clear_limit_hit_for_bot(self, bot).await
    }
    async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String {
        quota::quota_base_for_host(self, host, kind, identity).await
    }
    async fn set_quota(&self, host: &str, base: &str, q: Quota) {
        quota::set(self, host, base, q).await
    }
    async fn mark_claude_limit_hit(&self, bot: &db::Bot, line: &str) -> Result<()> {
        crate::turn_error::mark_claude_limit_hit(self, bot, line).await
    }
    async fn mark_agy_limit_hit(&self, bot: &db::Bot, line: &str) -> Result<()> {
        crate::turn_error::mark_agy_limit_hit(self, bot, line).await
    }
}

impl SupervisorRepo for SqlitePool {
    const SUPERVISOR_ID: &'static str = crate::supervisor::store::SUPERVISOR_ID;
    async fn push_inbox(
        &self,
        event_key: &str,
        kind: &str,
        assignment_id: Option<&str>,
        bot_id: Option<&str>,
        turn_id: Option<&str>,
        payload: &Value,
    ) -> Result<Option<String>> {
        crate::supervisor::store::push_inbox(self, event_key, kind, assignment_id, bot_id, turn_id, payload).await
    }
    async fn load_owned(&self) -> Result<crate::supervisor_owned::Owned> {
        crate::supervisor_owned::load(self).await
    }
    fn open_states_sql() -> String {
        crate::supervisor::store::sql_list(&crate::supervisor::store::OPEN_STATES)
    }
}

impl SupervisorSignals for Arc<App> {
    fn observe_idle_status(&self, run_id: &str, status: &str) {
        crate::supervisor::idle_sleep::observe_status(run_id, status)
    }
}

impl HandoffRepo for SqlitePool {
    async fn bot_handed_off_to(&self, bot_id: &str) -> Result<Option<String>> {
        crate::handoff::bot_handed_off_to(self, bot_id).await
    }
    async fn handoff_footprint(&self, host: &str) -> Result<Footprint> {
        crate::handoff::footprint(self, host).await
    }
}

impl HandoffConnRepo for SqliteConnection {
    async fn bot_handed_off_to_on(&mut self, bot_id: &str) -> Result<Option<String>> {
        crate::handoff::bot_handed_off_to_on(self, bot_id).await
    }
}

impl BotOpsPort for Arc<App> {
    async fn recover_restart_intents(&self, host: &str) {
        crate::restart_intents::recover_host(self, host).await
    }
    async fn recover_delete_intents(&self, host: &str) {
        crate::delete_intents::recover_host(self, host).await
    }
    async fn recover_promote_intents(&self, host: &str) {
        crate::promote_intents::recover_host(self, host).await
    }
    async fn scan_panes_snapshot(&self, host: &str, snapshot: &Value) -> Result<ScanOutcome> {
        crate::panes::scan_snapshot(self, host, snapshot).await
    }
    async fn gc_panes(&self, host: &str) -> Result<usize> {
        crate::panes::gc_host(self, host).await
    }
    async fn notify_unowned_and_orphans(&self, host: &str) -> Result<usize> {
        crate::panes::notify_unowned_and_orphans(self, host).await
    }
}

impl BotOpsRepo for SqlitePool {
    async fn has_open_restart_for_run(&self, host: &str, bot_id: &str, run_id: &str) -> Result<bool> {
        crate::restart_intents::has_open_restart_for_run(self, host, bot_id, run_id).await
    }
    async fn is_share_bot(&self, bot_id: &str) -> std::result::Result<bool, sqlx::Error> {
        crate::share::store::is_share_bot(self, bot_id).await
    }
}

impl IntentConnRepo for SqliteConnection {
    async fn record_done_intent(&mut self, kind: &str, subject_id: &str, host: &str, payload: &Value) -> Result<String> {
        crate::intents::record_done(self, kind, subject_id, host, payload).await
    }
}

impl HostSidePort for Arc<App> {
    async fn refresh_herdr_version(&self, host: &str) {
        crate::herdr_version::refresh(self, host).await
    }
    async fn herdr_maintenance_active(&self) -> Result<Option<Window>> {
        crate::herdr_maintenance::active(self).await
    }
    fn spawn_detect_github_host(&self, host: String) {
        crate::github::spawn_detect_host(self.clone(), host)
    }
    async fn emit_daemon_status(&self) {
        crate::state::emit_daemon_status(self).await
    }
    async fn set_default_connected(&self, connected: bool) {
        crate::state::set_default_connected(self, connected).await
    }
    async fn drain_remote_coalesced(&self, host: &str, bot_id: &str) -> Result<usize> {
        crate::hookrecv::drain_remote_coalesced(self, host, bot_id).await
    }
}

impl ProviderPort for Arc<App> {
    async fn adopt_statusline_model(&self, run: &db::Run, payload: &Value) {
        crate::claude_live::adopt_statusline_model(self, run, payload).await
    }
    async fn login_on_auth_failure(&self, bot: &db::Bot) {
        crate::login_prompt::on_auth_failure(self, bot).await
    }
    async fn login_on_turn_ok(&self, bot: &db::Bot) {
        crate::login_prompt::on_turn_ok(self, bot).await
    }
    fn codex_migration_on_blocked(&self, run: &db::Run) {
        crate::codex_model_migration::on_blocked(self, run)
    }
    fn prompt_suggestion_on_idle(&self, run: &db::Run) {
        crate::prompt_suggestion::on_idle(self, run)
    }
    async fn dismiss_survey_if_shown(&self, run: &db::Run) -> bool {
        crate::tui_prompts::dismiss_if_survey(self, run).await
    }
}

impl ApiPort for Arc<App> {
    async fn state_json(&self) -> std::result::Result<Value, LcError> {
        crate::api::state_json(self).await
    }
    fn ct_eq(&self, a: &str, b: &str) -> bool {
        crate::api::ct_eq(a, b)
    }
}

impl ReconcileCommands for Arc<App> {
    async fn reconcile_host(&self, host: &str) -> Result<()> {
        crate::reconcile::reconcile_host(self, host).await
    }
    async fn autostart_after_reconcile(&self, host: &str, reconciled: bool) -> bool {
        crate::reconcile::autostart_after_reconcile(self, host, reconciled).await
    }
    fn schedule_deferred_pass(&self, host: &str) {
        crate::reconcile::schedule_deferred_pass(self, host)
    }
    async fn sync_default_session(&self) -> Result<()> {
        crate::default_session::sync(self).await
    }
    fn session_paused_on_idle(&self, run: &db::Run) {
        crate::session_paused::on_idle(self, run)
    }
}

impl IngressCommands for Arc<App> {
    async fn watch_pane_on_session(&self, host: &str, session: &str, pane_id: &str) {
        crate::events::watch_pane_on_session(self, host, session, pane_id).await
    }
    async fn unwatch_pane_on_session(&self, host: &str, session: &str, pane_id: &str) {
        crate::events::unwatch_pane_on_session(self, host, session, pane_id).await
    }
    #[track_caller]
    fn retire_child<'a>(
        &'a self,
        bot_id: &'a str,
        why: &'static str,
        mode: crate::child_retire::Mode,
    ) -> impl Future<Output = Result<crate::child_retire::Outcome>> + 'a {
        crate::child_retire::retire(self, bot_id, why, mode)
    }
    async fn retirement_block(&self, bot_id: &str) -> Result<Option<String>> {
        crate::child_reconcile_safety::retirement_block(&self.db, bot_id).await
    }
    async fn prune_stale_spawn_hints(&self) {
        crate::spawn_hints::prune_stale(self).await
    }
    async fn spawn_hints_for_host(&self, host: &str) -> Result<HashMap<String, String>> {
        crate::spawn_hints::for_host(self, host).await
    }
    async fn consume_spawn_hint(&self, host: &str, pane_id: &str) {
        crate::spawn_hints::consume(self, host, pane_id).await
    }
}
