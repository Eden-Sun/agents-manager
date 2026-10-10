//! P8 hook／事件／對帳的窄介面由 `App`／`SqlitePool`／交易連線實作的地方（crate 拆分第 3 步）。每個方法逐行委派給入口與對帳原本呼叫
//! 的函式，不加任何邏輯、不多一個 await、不改鎖的範圍，所以行為與錯誤型別不變（重送、同一回合鎖、耐久事件的「只發生一次」
//! 都留在被委派的函式裡）。介面在 `ingress_ports.rs`；這個檔案是 P8 唯一還知道 lifecycle／quota／supervisor／panes… 實作細節的地方
//! （composition root 側的 adapter，不屬於未來的 am-ingress／am-reconcile）。
//!
//! 模組掛在 `events.rs`（`#[path]`），不碰 `lib.rs`。

use crate::db;
use crate::herdr_maintenance::Window;
use crate::events::ports::{
    ApiPort, BotOpsPort, BotOpsRepo, HostSidePort, IngressCommands, ProviderPort, QuotaCommands, ReconcileCommands,
    SupervisorRepo, SupervisorSignals, TurnCommands,
};
use crate::child_retire::IntentConnRepo;
use crate::lifecycle::{self, InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
use crate::panes::{PaneOwnership, PaneRuntime, ScanOutcome};
use crate::quota::{self, Quota};
use crate::state::App;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;

impl IntentConnRepo for sqlx::SqliteConnection {
    async fn record_done_intent(
        &mut self,
        kind: &str,
        subject_id: &str,
        host: &str,
        payload: &Value,
    ) -> Result<String> {
        crate::intents::record_done(self, kind, subject_id, host, payload).await
    }
}

impl TurnCommands for App {
    async fn emit_message_added(&self, bot_id: &str, message: db::Message) {
        lifecycle::emit_message_added(&self.shared(), bot_id, message).await
    }
    async fn emit_turn(&self, turn_id: &str) {
        lifecycle::emit_turn(&self.shared(), turn_id).await
    }
    fn schedule_flush_queued(&self, bot_id: &str) {
        lifecycle::schedule_flush_queued(&self.shared(), bot_id)
    }
    fn schedule_deferred_live(&self, bot_id: &str) {
        lifecycle::schedule_deferred_live(&self.shared(), bot_id)
    }
    fn schedule_codex_notice_capture(&self, bot_id: &str, run_id: &str) {
        lifecycle::schedule_codex_notice_capture(&self.shared(), bot_id, run_id)
    }
    fn poke_resume_nudge(&self, bot_id: &str) {
        lifecycle::poke_resume_nudge(&self.shared(), bot_id)
    }
    async fn cancel_stall(&self, run_id: &str) {
        lifecycle::cancel_stall(&self.shared(), run_id).await
    }
    async fn arm_fallback(&self, run_id: &str, bot_id: &str) {
        lifecycle::arm_fallback(&self.shared(), run_id, bot_id).await
    }
    async fn arm_progress(&self, run_id: &str, bot_id: &str, turn_id: &str) {
        lifecycle::arm_progress(&self.shared(), run_id, bot_id, turn_id).await
    }
    async fn has_progress_poller(&self, run_id: &str) -> bool {
        self.progress_pollers.lock().await.contains_key(run_id)
    }
    async fn arm_stall(&self, run_id: &str, bot_id: &str, turn_id: &str) {
        lifecycle::arm_stall(&self.shared(), run_id, bot_id, turn_id).await
    }
    async fn begin_external_turn(&self, run: &db::Run) {
        lifecycle::begin_external_turn(&self.shared(), run).await
    }
    async fn mark_run_exited(&self, run_id: &str, reason: &str) -> RunExit {
        lifecycle::mark_run_exited(&self.shared(), run_id, reason).await
    }
    async fn context_lost(&self, bot: &db::Bot, why: &str, failed_session: Option<&str>) -> LcResult<()> {
        lifecycle::context_lost(&self.shared(), bot, why, failed_session).await
    }
    async fn retire_context_lost(&self, bot_id: &str, session_id: &str) {
        lifecycle::retire_context_lost(&self.shared(), bot_id, session_id).await
    }
    async fn settle_interruption(&self, bot_id: &str, evidence: InterruptEvidence) -> Result<()> {
        lifecycle::settle_interruption(&self.shared(), bot_id, evidence).await
    }
    async fn settle_owed_deliveries(&self, bot_id: &str) -> Result<()> {
        lifecycle::settle_owed_deliveries(&self.shared(), bot_id).await
    }
    async fn settle_interrupt_echo(
        &self,
        bot_id: &str,
        run_id: &str,
        ev: &InterruptFailureEvidence<'_>,
        in_flight: Option<&db::Turn>,
    ) -> Result<bool> {
        lifecycle::settle_interrupt_echo(&self.shared(), bot_id, run_id, ev, in_flight).await
    }
    async fn start_bot(&self, bot_id: &str) -> LcResult<String> {
        lifecycle::start_bot(&self.shared(), bot_id).await
    }
    async fn start_bot_locked_with(&self, bot_id: &str, opts: StartOpts) -> LcResult<String> {
        lifecycle::start_bot_locked_with(&self.shared(), bot_id, opts).await
    }
    async fn resume_after_boot(&self, host: &str) -> usize {
        lifecycle::start_send::resume_after_boot(&self.shared(), host).await
    }
    async fn adopt_unbound_send_nows(&self, boot: &str) -> bool {
        lifecycle::adopt_unbound_send_nows(&self.shared(), boot).await
    }
    async fn rearm_queue_retries(&self) -> Result<usize> {
        lifecycle::rearm_queue_retries(&self.shared()).await
    }
    async fn adopt_turns_of_ended_runs(&self, boot: &str) -> bool {
        lifecycle::adopt_turns_of_ended_runs(&self.shared(), boot).await
    }
    async fn rearm_queued_prompt_restamps(&self) -> Result<()> {
        lifecycle::rearm_queued_prompt_restamps(&self.shared()).await
    }
    async fn adopt_interrupted_on_restart(&self, run: &db::Run, turn: &db::Turn) -> Result<bool> {
        lifecycle::adopt_interrupted_on_restart(&self.shared(), run, turn).await
    }
    fn spawn_adopted_capture(&self, run_id: &str, bot_id: &str) {
        lifecycle::spawn_adopted_capture(&self.shared(), run_id, bot_id)
    }
    async fn sweep_stuck_turns(&self, host: Option<&str>) -> Vec<String> {
        lifecycle::sweep_stuck_turns(&self.shared(), host).await
    }
    async fn close_after_session_paused(&self, run_id: &str, expected_turn_id: &str) -> Option<String> {
        lifecycle::close_after_session_paused(&self.shared(), run_id, expected_turn_id).await
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
        lifecycle::insert_message(&self.shared(), conversation_id, turn_id, role, content, source, incomplete, snapshot).await
    }
    async fn prompt_relayed_queueable(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>) -> LcResult<PromptOut> {
        lifecycle::prompt_relayed_queueable(&self.shared(), bot_id, text, client_request_id, relay_from).await
    }
}

impl PaneRuntime for App {
    fn pane_host_ownership<'a>(&'a self, host: &'a str) -> impl Future<Output = Result<Option<PaneOwnership>>> + Send + 'a {
        async move {
            if !crate::shared_host::is_shared(&self.shared(), host).await {
                return Ok(None);
            }
            let owned = crate::shared_host::owned(&self.shared(), host).await?;
            Ok(Some(PaneOwnership { workspaces: owned.workspaces, tabs: owned.tabs, panes: owned.panes }))
        }
    }

    fn pane_shell_client<'a>(&'a self, host: &'a str) -> impl Future<Output = Result<crate::herdr::HerdrClient>> + Send + 'a {
        async move {
            crate::api::shell::client_for(&self.shared(), host)
                .await
                .map(|(client, _)| client)
                .map_err(|e| anyhow::anyhow!("{e:?}"))
        }
    }

    fn close_pane_and_tab<'a>(
        &'a self,
        client: &'a crate::herdr::HerdrClient,
        workspace_id: Option<&'a str>,
        tab_id: Option<&'a str>,
        pane_id: &'a str,
    ) -> impl Future<Output = ()> + Send + 'a {
        crate::lifecycle::close_pane_and_tab(client, workspace_id, tab_id, pane_id)
    }

    fn push_pane_inbox<'a>(
        &'a self,
        key: &'a str,
        event: &'a str,
        payload: &'a Value,
    ) -> impl Future<Output = Result<Option<String>>> + Send + 'a {
        crate::supervisor::store::push_inbox(&self.db, key, event, None, None, None, payload)
    }
}

impl QuotaCommands for App {
    async fn clear_limit_hit_for_bot(&self, bot: &db::Bot) {
        crate::runners::quota::clear_limit_hit_for_bot(&self.shared(), bot).await
    }
    async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String {
        quota::quota_base_for_host(&self.shared(), host, kind, identity).await
    }
    async fn set_quota(&self, host: &str, base: &str, q: Quota) {
        quota::set(&self.shared(), host, base, q).await
    }
    async fn mark_claude_limit_hit(&self, bot: &db::Bot, line: &str) -> Result<()> {
        crate::turn_error::mark_claude_limit_hit(&self.shared(), bot, line).await
    }
    async fn mark_agy_limit_hit(&self, bot: &db::Bot, line: &str) -> Result<()> {
        crate::turn_error::mark_agy_limit_hit(&self.shared(), bot, line).await
    }
}

impl SupervisorRepo for App {
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
        crate::supervisor::store::push_inbox(&self.db, event_key, kind, assignment_id, bot_id, turn_id, payload).await
    }

    async fn load_owned(&self) -> Result<crate::projection::Owned> {
        crate::supervisor_owned::load(&self.db).await
    }

    fn open_states_sql() -> String {
        crate::supervisor::store::sql_list(&crate::supervisor::store::OPEN_STATES)
    }
}

impl BotOpsRepo for App {
    async fn has_open_restart_for_run(&self, host: &str, bot_id: &str, run_id: &str) -> Result<bool> {
        crate::restart_intents::has_open_restart_for_run(&self.db, host, bot_id, run_id).await
    }

    async fn is_share_bot(&self, bot_id: &str) -> std::result::Result<bool, sqlx::Error> {
        crate::share::store::is_share_bot(&self.db, bot_id).await
    }
}



impl SupervisorSignals for App {
    fn observe_idle_status(&self, run_id: &str, status: &str) {
        crate::supervisor::idle_sleep::observe_status(run_id, status)
    }
}

impl BotOpsPort for App {
    async fn recover_restart_intents(&self, host: &str) {
        crate::runners::restart_intents::recover_host(&self.shared(), host).await
    }
    async fn recover_delete_intents(&self, host: &str) {
        crate::delete_intents::recover_host(&self.shared(), host).await
    }
    async fn recover_promote_intents(&self, host: &str) {
        crate::promote_intents::recover_host(&self.shared(), host).await
    }
    async fn scan_panes_snapshot(&self, host: &str, snapshot: &Value) -> Result<ScanOutcome> {
        crate::panes::scan_snapshot(&self.shared(), host, snapshot).await
    }
    async fn gc_panes(&self, host: &str) -> Result<usize> {
        crate::panes::gc_host(&self.shared(), host).await
    }
    async fn notify_unowned_and_orphans(&self, host: &str) -> Result<usize> {
        crate::panes::notify_unowned_and_orphans(&self.shared(), host).await
    }
}





impl HostSidePort for App {
    async fn refresh_herdr_version(&self, host: &str) {
        crate::runners::herdr_version::refresh(&self.shared(), host).await
    }
    async fn herdr_maintenance_active(&self) -> Result<Option<Window>> {
        crate::runners::herdr_maintenance::active(&self.shared()).await
    }
    fn spawn_detect_github_host(&self, host: String) {
        crate::runners::github::spawn_detect_host(self.shared(), host)
    }
    async fn emit_daemon_status(&self) {
        crate::state::emit_daemon_status(&self.shared()).await
    }
    async fn set_default_connected(&self, connected: bool) {
        crate::state::set_default_connected(&self.shared(), connected).await
    }
    async fn drain_remote_coalesced(&self, host: &str, bot_id: &str) -> Result<usize> {
        crate::runners::hookrecv::drain_remote_coalesced(&self.shared(), host, bot_id).await
    }
}

impl ProviderPort for App {
    async fn adopt_statusline_model(&self, run: &db::Run, payload: &Value) {
        crate::claude_live::adopt_statusline_model(&self.shared(), run, payload).await
    }
    async fn login_on_auth_failure(&self, bot: &db::Bot, admitted: Option<&crate::hosts::HostFence>) {
        crate::runners::login_prompt::on_auth_failure(&self.shared(), bot, admitted).await
    }
    async fn admitted_host_fence(&self, bot: &db::Bot) -> Option<crate::hosts::HostFence> {
        let host = db::bot_host(&self.db, &bot.id).await.ok()?;
        self.hosts.fence(&host).await
    }
    async fn login_on_turn_ok(&self, bot: &db::Bot, admitted: Option<&crate::hosts::HostFence>) {
        crate::runners::login_prompt::on_turn_ok(&self.shared(), bot, admitted).await
    }
    fn codex_migration_on_blocked(&self, run: &db::Run) {
        crate::runners::codex_model_migration::on_blocked(&self.shared(), run)
    }
    fn prompt_suggestion_on_idle(&self, run: &db::Run) {
        crate::runners::prompt_suggestion::on_idle(&self.shared(), run)
    }
    async fn dismiss_survey_if_shown(&self, run: &db::Run) -> bool {
        crate::tui_prompts::dismiss_if_survey(&self.shared(), run).await
    }
}

impl ApiPort for App {
    async fn state_json(&self) -> std::result::Result<Value, LcError> {
        crate::api::state_json(&self.shared()).await
    }
    fn ct_eq(&self, a: &str, b: &str) -> bool {
        crate::api::ct_eq(a, b)
    }
}

impl ReconcileCommands for App {
    async fn reconcile_host(&self, host: &str) -> Result<()> {
        crate::runners::reconcile::reconcile_host(&self.shared(), host).await
    }
    async fn autostart_after_reconcile(&self, host: &str, reconciled: bool) -> bool {
        crate::runners::reconcile::autostart_after_reconcile(&self.shared(), host, reconciled).await
    }
    fn schedule_deferred_pass(&self, host: &str) {
        crate::runners::reconcile::schedule_deferred_pass(&self.shared(), host)
    }
    async fn sync_default_session(&self) -> Result<()> {
        crate::default_session::sync(&self.shared()).await
    }
    fn session_paused_on_idle(&self, run: &db::Run) {
        crate::runners::session_paused::on_idle(&self.shared(), run)
    }
}

impl IngressCommands for App {
    async fn watch_pane_on_session(&self, host: &str, session: &str, pane_id: &str) {
        crate::runners::events::watch_pane_on_session(&self.shared(), host, session, pane_id).await
    }
    async fn unwatch_pane_on_session(&self, host: &str, session: &str, pane_id: &str) {
        crate::events::unwatch_pane_on_session(&self.shared(), host, session, pane_id).await
    }
    #[track_caller]
    fn retire_child<'a>(
        &'a self,
        bot_id: &'a str,
        why: &'static str,
        mode: crate::child_retire::Mode,
    ) -> impl Future<Output = Result<crate::child_retire::Outcome>> + 'a {
        let app = self.shared();
        let caller = std::panic::Location::caller();
        async move { crate::runners::child_retire::retire_at(&app, bot_id, why, mode, caller).await }
    }
    async fn retirement_block(&self, bot_id: &str) -> Result<Option<String>> {
        crate::child_reconcile_safety::retirement_block(&self.db, bot_id).await
    }
    async fn prune_stale_spawn_hints(&self) {
        crate::spawn_hints::prune_stale(self).await
    }
    async fn spawn_hints_for_host(&self, host: &str) -> Result<HashMap<String, String>> {
        crate::spawn_hints::for_host(&self.shared(), host).await
    }
    async fn consume_spawn_hint(&self, host: &str, pane_id: &str) {
        crate::spawn_hints::consume(&self.shared(), host, pane_id).await
    }
}
