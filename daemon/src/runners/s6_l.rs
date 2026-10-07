//! App-side callbacks for the P6a lifecycle seam.

use crate::{app_ports_p4state, lifecycle::s6_ports, state::App};
pub(crate) use crate::lifecycle::start::ports;
use serde_json::Value;
use std::{path::{Path, PathBuf}, sync::Arc};

#[path = "../app_ports_p4sess.rs"]
pub(crate) mod app_ports_p4sess;

pub(crate) async fn remote_bot_dir(
    conn: &crate::hosts::HostConn,
    bot_id: &str,
) -> anyhow::Result<crate::lifecycle::RemoteHookPaths> {
    crate::lifecycle::remote_bot_dir_for(conn, bot_id, crate::startup::instance().as_deref()).await
}

impl s6_ports::DeadPanesServices for Arc<App> {
    async fn maintenance_window_active(&self) -> bool {
        !matches!(crate::runners::herdr_maintenance::active(self).await, Ok(None))
    }
}

impl s6_ports::TranscriptOriginServices for Arc<App> {
    async fn started_by_the_cli_itself(
        &self,
        bot: &crate::db::Bot,
        transcript_path: Option<&str>,
    ) -> bool {
        crate::lifecycle::poller::app_ports_p4obs::started_by_the_cli_itself(self, bot, transcript_path).await
    }
}

impl s6_ports::StuckTurnServices for Arc<App> {
    fn retain_supervisor_runs(&self, active: &[String]) {
        crate::supervisor::idle_sleep::retain_runs(active);
    }

    async fn sweep_child_alerts(&self) {
        let _ = crate::runners::child_alerts::sweep(self).await;
    }

    async fn sweep_child_done(&self) {
        let _ = crate::runners::child_done::sweep(self).await;
    }

    async fn local_transcript_allowed(&self, bot: &crate::db::Bot, raw_path: &str) -> bool {
        crate::app_ports_p5::local_transcript_allowed(self, bot, raw_path).await
    }

    async fn codex_exact_reply(
        &self,
        bot: &crate::db::Bot,
        run: &crate::db::Run,
        sent: &[String],
    ) -> Option<String> {
        crate::codex_history::exact_reply(self, bot, run, sent).await
    }

    async fn codex_home(&self, bot: &crate::db::Bot) -> Option<PathBuf> {
        crate::app_ports_p4state::codex_home(self, bot).await
    }

    async fn emit_turn(&self, turn_id: &str) {
        crate::lifecycle::emit_turn(self, turn_id).await;
    }
}

impl s6_ports::SetupShareServices for Arc<App> {
    fn cage_settings(&self, settings: &mut Value, workspace: &str, env: &Value) {
        crate::share::cage::cage_settings(settings, workspace, env);
    }
}

impl s6_ports::IdentityAccess for Arc<App> {
    async fn identity_for_host(&self, host: &str, name: &str) -> Option<crate::config::IdentityCfg> {
        crate::tools::identity_for_host(self, host, name).await
    }

    async fn cached_identity_logged_out(&self, host: &str, name: &str) -> bool {
        self.tools.lock().await.get(host).and_then(|tools| tools.identities.get(name)).is_some_and(|identity| identity.logged_in == Some(false))
    }

    async fn recheck_identity_login(&self, host: &str, name: &str) -> Option<bool> {
        crate::tools::recheck_identity_login(self, host, name).await
    }
}

impl s6_ports::StartServices for Arc<App> {
    fn lock_bot_for_start<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = Box<dyn am_ports::BotLockGuard + 'static>> + Send + 'a {
        async move { app_ports_p4sess::lock_bot_owned(self, bot_id).await }
    }

    async fn session_for_bot(&self, bot: &crate::db::Bot, host: &str) -> Option<String> {
        App::session_for_bot(self, bot, host).await
    }

    async fn prepare_restricted_bot(
        &self,
        bot: &crate::db::Bot,
        host: &str,
    ) -> Result<Option<String>, crate::lifecycle::LcError> {
        crate::share::cage::prepare(self, bot, host).await
    }

    async fn cage_environment(&self, env: &mut Value, bot: &crate::db::Bot) {
        let identity_env = crate::share::cage::identity_env(self, bot).await;
        crate::share::cage::cage_env(env, &identity_env, &crate::share::cage::local_home());
    }

    fn install_restricted_prompt(
        &self,
        bot: &crate::db::Bot,
        workspace: &str,
        env: &Value,
    ) -> anyhow::Result<PathBuf> {
        crate::share::cage::install_prompt(self, bot, workspace, env)
    }

    fn restricted_launch_args(&self, env: &Value, prompt: &Path) -> Vec<String> {
        crate::share::cage::launch_args(env, prompt)
    }

    async fn pane_env(
        &self,
        bot: &crate::db::Bot,
        host: &str,
        run_id: &str,
        agent_name: &str,
        shim_dir: Option<&str>,
        fence: Option<&crate::hosts::HostFence>,
    ) -> anyhow::Result<Value> {
        match fence {
            Some(fence) => crate::lifecycle::setup::pane_env_for_fence(self, bot, host, run_id, agent_name, shim_dir, fence).await,
            None => crate::lifecycle::setup::pane_env(self, bot, host, run_id, agent_name, shim_dir).await,
        }
    }

    async fn pretrust_for_start(&self, bot: &crate::db::Bot, host: &str, cwd: &str) -> Vec<String> {
        crate::trust::pretrust_for_start(self, bot, host, cwd).await
    }

    async fn apply_grok_startup_effort(
        &self,
        bot: &crate::db::Bot,
        run_id: &str,
        pane_id: &str,
        client: &crate::herdr::HerdrClient,
    ) -> Result<(), String> {
        crate::lifecycle::apply_grok_startup_effort(self, bot, run_id, pane_id, client).await
    }

    fn flush_once_running(&self, bot_id: &str, run_id: &str) {
        crate::lifecycle::start_send::flush_once_running(self, bot_id, run_id);
    }

    #[cfg(test)]
    fn spawn_adopted_capture(&self, run_id: &str, bot_id: &str) {
        crate::lifecycle::poller::spawn_adopted_capture(self, run_id, bot_id);
    }

    async fn watch_pane_on_session(&self, host: &str, session: &str, pane: &str) {
        crate::runners::events::watch_pane_on_session(self, host, session, pane).await;
    }

    async fn session_connected_with_host_fence(&self, fence: &crate::hosts::HostFence, session: &str) -> bool {
        App::session_connected_with_host_fence(self, fence, session).await
    }

    async fn emit_lifecycle_event(&self, kind: &str, bot: Option<&str>, payload: Value) {
        crate::lifecycle::start::ports::emit_object(&crate::app_ports_p4::AppEventSink::new(self), kind, bot, payload).await;
    }
}

impl s6_ports::TurnErrorServices for Arc<App> {
    async fn next_reset_for_bot(&self, bot: &crate::db::Bot) -> Option<String> {
        crate::runners::quota::next_reset_for_bot(self, bot).await
    }

    async fn read_run_pane_recent_unwrapped(
        &self,
        run: &crate::db::Run,
        pane: &str,
        lines: usize,
    ) -> anyhow::Result<Option<String>> {
        crate::app_ports_p4::AppHerdrPort::new(self)
            .read_run_pane(
                run,
                pane,
                am_core::PaneReadSource::RecentUnwrapped,
                u32::try_from(lines).unwrap_or(u32::MAX),
            )
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))
    }

    async fn host_utc_offset_secs(&self, host: &str) -> Option<i32> {
        self.tools.lock().await.get(host).and_then(|tools| tools.utc_offset_secs)
    }

    async fn turn_changed(&self, turn_id: &str) -> anyhow::Result<()> {
        use am_ports::TurnEvents;
        crate::app_ports_p4::AppTurnEvents::new(self)
            .turn_changed(&turn_id.to_string())
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

impl s6_ports::TurnErrorQuotaAccess for Arc<App> {
    fn resolve_key<'a>(
        &'a self,
        host: &'a str,
        provider: &'a str,
        identity: Option<&'a str>,
    ) -> impl std::future::Future<Output = Result<am_core::QuotaKey, am_core::PortError>> + Send + 'a {
        <App as am_ports::QuotaAccess>::resolve_key(self.as_ref(), host, provider, identity)
    }

    fn snapshot<'a>(
        &'a self,
        key: &'a am_core::QuotaKey,
    ) -> impl std::future::Future<Output = Result<Option<am_core::QuotaSnapshot>, am_core::PortError>> + Send + 'a {
        <App as am_ports::QuotaAccess>::snapshot(self.as_ref(), key)
    }

    fn store_snapshot<'a>(
        &'a self,
        key: &'a am_core::QuotaKey,
        snapshot: am_core::QuotaSnapshot,
    ) -> impl std::future::Future<Output = Result<(), am_core::PortError>> + Send + 'a {
        <App as am_ports::QuotaAccess>::store_snapshot(self.as_ref(), key, snapshot)
    }

}

impl s6_ports::QuotaHoldServices for Arc<App> {
    async fn try_limit_hit_for_bot(&self, bot: &crate::db::Bot) -> anyhow::Result<Option<crate::quota::LimitHit>> {
        crate::runners::quota::try_limit_hit_for_bot(self, bot).await
    }

    async fn running_model(&self, bot: &crate::db::Bot) -> Option<String> {
        app_ports_p4state::running_model(self, bot).await
    }

    async fn billing_identity(&self, bot: &crate::db::Bot) -> anyhow::Result<Option<String>> {
        app_ports_p4state::billing_identity(self, bot).await
    }

    async fn limit_cleared_since(&self, bot: &crate::db::Bot, since: chrono::DateTime<chrono::Utc>) -> bool {
        app_ports_p4state::limit_cleared_since(self, bot, since).await
    }

    async fn host_target(&self, host: &str) -> Option<String> {
        app_ports_p4state::host_target(self, host).await
    }

    async fn restore_limit_hit(&self, host: &str, base: &str, hit: crate::quota::LimitHit) -> bool {
        app_ports_p4state::restore_limit_hit(self.as_ref(), host, base, hit).await
    }

    async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String {
        app_ports_p4state::quota_base_for_host(self, host, kind, identity).await
    }

    fn schedule_queue_flush(&self, bot_id: &str) {
        crate::lifecycle::queue::schedule_flush_queued(self, bot_id);
    }

    fn schedule_flush_retry(&self, bot_id: &str, delay: std::time::Duration) {
        crate::lifecycle::queue::schedule_flush_retry(self, bot_id, delay);
    }
}

impl s6_ports::RunStateServices for Arc<App> {
    async fn reconcile_host(&self, host: &str) -> anyhow::Result<()> {
        app_ports_p4state::reconcile_host(self, host).await
    }

    async fn finish_stop(&self, run_id: &str) {
        app_ports_p4state::finish_stop(self, run_id).await;
    }
}

impl s6_ports::LiveApplyDebtContext for Arc<App> {
    async fn stamp_live_revision(&self, pool: &sqlx::SqlitePool, run_id: &str) -> Result<bool, sqlx::Error> {
        app_ports_p4state::stamp_live_revision(pool, run_id).await
    }
}

impl s6_ports::DeferredLiveContext for Arc<App> {
    async fn apply_live_setting_with_revision(
        &self,
        bot_id: &str,
        fields: &[&'static str],
        baseline_rev: &str,
        target_rev: &str,
    ) -> crate::lifecycle::LiveApplyOutcome {
        app_ports_p4state::apply_live_setting_with_revision(self, bot_id, fields, baseline_rev, target_rev).await
    }
}

impl s6_ports::GrokTranscriptServices for Arc<App> {
    async fn host_shell(&self, host: &str, script: &str) -> anyhow::Result<String> {
        crate::lifecycle::poller::app_ports_p4obs::host_sh(self, host, script).await
    }

    async fn grok_home_for(&self, bot: &crate::db::Bot, host: &str) -> anyhow::Result<String> {
        crate::lifecycle::poller::app_ports_p4obs::grok_home_for(self, bot, host).await
    }

    async fn pids_in_pane(&self, host: &str, pane: &str, session: Option<&str>) -> Vec<i32> {
        crate::lifecycle::poller::app_ports_p4obs::pids_in_pane(self, host, pane, session).await
    }

    async fn agy_session_load(&self, run: &crate::db::Run, host: &str) -> anyhow::Result<Option<(String, String)>> {
        crate::lifecycle::poller::app_ports_p4obs::agy_session_load(self, run, host).await
    }

    async fn consume_resume_session(
        &self,
        bot: &crate::db::Bot,
        run: &crate::db::Run,
        session: Option<&str>,
    ) -> anyhow::Result<()> {
        crate::lifecycle::poller::app_ports_p4obs::consume_resume_session(self, bot, run, session).await
    }

    async fn agy_session_record_status(&self, bot: &crate::db::Bot, run: &crate::db::Run, transcript: &str) {
        crate::lifecycle::poller::app_ports_p4obs::agy_session_record_status(self, bot, run, transcript).await;
    }

    async fn emit_turn(&self, turn_id: &str) {
        crate::lifecycle::emit_turn(self, turn_id).await;
    }

    async fn claude_herdr_session(&self, run: &crate::db::Run) -> Option<String> {
        crate::runners::claude_child_log::herdr_session(self, run).await
    }

    async fn claude_config_roots(&self, bot: &crate::db::Bot, run: &crate::db::Run, host: &str) -> Vec<String> {
        crate::runners::claude_child_log::config_roots(self, bot, run, host).await
    }
}

impl s6_ports::BusySendServices for Arc<App> {
    async fn prompt_message_added(&self, bot_id: &str, message_id: &str) {
        crate::lifecycle::prompt::emit_prompt_message(self, bot_id, message_id).await;
    }

    async fn turn_changed(&self, turn_id: &str) {
        crate::lifecycle::send_now::ports::turn_changed(&crate::app_ports_p4::AppTurnEvents::new(self), turn_id).await;
    }
}

impl s6_ports::CodexSteerServices for Arc<App> {
    async fn plan_delivery(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &crate::db::Run,
        bot: &crate::db::Bot,
        text: &str,
        force_pane: bool,
        waited_for_log: bool,
    ) -> anyhow::Result<Result<crate::lifecycle::delivery::Plan, crate::lifecycle::delivery::Delivered>> {
        crate::lifecycle::delivery::plan_delivery(self, client, run, bot, text, force_pane, waited_for_log).await
    }

    async fn execute_delivery(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &crate::db::Run,
        bot: &crate::db::Bot,
        text: &str,
        plan: crate::lifecycle::delivery::Plan,
    ) -> anyhow::Result<crate::lifecycle::delivery::Delivered> {
        crate::lifecycle::delivery::execute_delivery(self, client, run, bot, text, plan).await
    }
}

impl s6_ports::SendNowServices for Arc<App> {
    async fn prepare_delivery(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &crate::db::Run,
        bot: &crate::db::Bot,
        text: &str,
        plan: crate::lifecycle::delivery::Plan,
    ) -> Result<crate::lifecycle::delivery::Ready, crate::lifecycle::delivery::Delivered> {
        crate::lifecycle::delivery::prepare_delivery(self, client, run, bot, text, plan).await
    }

    async fn type_delivery_text(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &crate::db::Run,
        bot: &crate::db::Bot,
        text: &str,
        ready: crate::lifecycle::delivery::Ready,
    ) -> anyhow::Result<crate::lifecycle::delivery::Typing> {
        crate::lifecycle::delivery::type_text(self, client, run, bot, text, ready).await
    }

    async fn send_now_landed(
        &self,
        client: &crate::herdr::HerdrClient,
        run: &crate::db::Run,
        bot: &crate::db::Bot,
        text: &str,
        typed: &crate::lifecycle::delivery::Typed,
    ) -> crate::lifecycle::delivery::Landed {
        crate::lifecycle::delivery::submit_landed(self, client, run, bot, text, typed).await
    }

    async fn confirm_send_now(
        &self,
        client: &crate::herdr::HerdrClient,
        run: &crate::db::Run,
        bot: &crate::db::Bot,
        text: &str,
        typed: &crate::lifecycle::delivery::Typed,
    ) -> anyhow::Result<crate::lifecycle::delivery::Delivered> {
        crate::lifecycle::delivery::confirm_submitted(self, client, run, bot, text, typed).await
    }
}

impl s6_ports::SuggestionServices for Arc<App> {
    async fn submit_gates(
        &self,
        bot_id: &str,
        bot: &crate::db::Bot,
        conversation_id: &str,
    ) -> Result<crate::db::Run, crate::lifecycle::LcError> {
        crate::lifecycle::composer_draft::submit_gates(self, bot_id, bot, conversation_id).await
    }

    async fn client_for_run(&self, run: &crate::db::Run) -> Result<crate::lifecycle::RunClient, crate::lifecycle::LcError> {
        crate::lifecycle::client_for_run(self, run).await
    }

    async fn submit_locked(
        &self,
        bot_id: &str,
        token: &str,
        client_request_id: &str,
    ) -> Result<crate::lifecycle::PromptOut, crate::lifecycle::composer_draft::SubmitDraftError> {
        crate::lifecycle::composer_draft::submit_locked(self, bot_id, token, client_request_id).await
    }
}

impl s6_ports::ResumeNudgeServices for Arc<App> {
    async fn identity_config_dir(&self, host: &str, identity: Option<&str>) -> anyhow::Result<String> {
        app_ports_p4state::identity_config_dir(self, host, identity).await
    }

    async fn queue_nudge(
        &self,
        conversation_id: &str,
        bot_id: &str,
        text: &str,
        client_request_id: &str,
        relay_from: &str,
    ) -> anyhow::Result<crate::lifecycle::PromptOut> {
        let relay = crate::lifecycle::prompt::RelaySrc::trusted(Some(relay_from));
        app_ports_p4state::queue_for_next_turn(self, conversation_id, bot_id, text, text, client_request_id, None, relay)
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))
    }

    fn schedule_queue_flush(&self, bot_id: &str) {
        app_ports_p4state::schedule_flush_queued(self, bot_id);
    }
}

impl s6_ports::ScreenServices for Arc<App> {
    async fn read_run_pane_recent_unwrapped(
        &self,
        run: &crate::db::Run,
        pane: &str,
        lines: usize,
    ) -> anyhow::Result<Option<(String, u64)>> {
        let Some(read) = crate::lifecycle::poller::app_ports_p4obs::read_pane_recent_unwrapped(
            self,
            run,
            pane,
            u32::try_from(lines).unwrap_or(u32::MAX),
        )
        .await?
        else {
            return Ok(None);
        };
        Ok(Some((read.text, read.revision)))
    }

    async fn insert_system_notice(&self, conversation_id: &str, notice: &str) -> anyhow::Result<()> {
        crate::lifecycle::messages::insert_message(self, conversation_id, None, "system", notice, "system", false, None).await?;
        Ok(())
    }

    async fn shadow_limit_hit(&self, sample: crate::judge::Sample) {
        crate::lifecycle::poller::app_ports_p4obs::shadow_limit_hit(self, sample).await;
    }

    async fn mark_codex_limit_hit(&self, bot: &crate::db::Bot, notice: &str) -> anyhow::Result<()> {
        crate::lifecycle::poller::app_ports_p4obs::mark_codex_limit_hit(self, bot, notice).await
    }

    async fn fail_in_flight_turn(&self, turn_id: &str, note: &str) -> anyhow::Result<()> {
        crate::lifecycle::poller::app_ports_p4obs::fail_in_flight_turn(self, turn_id, note).await
    }

    async fn apply_codex_limit_hit_quota(
        &self,
        host: &str,
        base: &str,
        hit: crate::quota::LimitHit,
    ) -> crate::quota::LimitHit {
        crate::lifecycle::poller::app_ports_p4obs::apply_codex_limit_hit_quota(self, host, base, hit).await
    }
}

impl s6_ports::StopServices for Arc<App> {
    fn lock_bot_for_stop<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = Box<dyn am_ports::BotLockGuard + 'static>> + Send + 'a {
        async move { app_ports_p4sess::lock_bot_owned(self, bot_id).await }
    }

    async fn notify_bot_status(&self, bot_id: &str) {
        crate::lifecycle::start::ports::bot_status(&crate::app_ports_p4::AppEventSink::new(self), bot_id).await;
    }

    async fn publish_lifecycle_event(&self, kind: &str, payload: Value) {
        crate::lifecycle::start::ports::emit_object(&crate::app_ports_p4::AppEventSink::new(self), kind, None, payload).await;
    }

    async fn refresh_background_jobs(&self, run: &crate::db::Run, kind: &str) {
        crate::runners::background_jobs::refresh(self, run, kind).await;
    }

    fn background_jobs_count(&self, run_id: &str) -> Option<u32> {
        crate::background_jobs::known(self, run_id)
    }

    async fn fail_in_flight(&self, run_id: &str, note: &str) -> anyhow::Result<()> {
        crate::lifecycle::fail_in_flight(self, run_id, note).await
    }

    async fn note_user_interrupt_of(
        &self,
        bot: &crate::db::Bot,
        run: &crate::db::Run,
        in_flight: Option<&crate::db::Turn>,
    ) {
        crate::lifecycle::interrupt_grace::note_user_interrupt_of(self, bot, run, in_flight).await;
    }

    async fn announce_revoked(&self, turn_id: &str, revoked: crate::lifecycle::Revoked) {
        crate::lifecycle::announce_revoked(self, turn_id, revoked).await;
    }

    async fn revoke_orphaned_queued_turns(&self, bot_id: &str, why: &str) -> Vec<String> {
        crate::lifecycle::revoke_orphaned_queued_turns(self, bot_id, why).await
    }

    async fn stop_preview(&self, bot_id: &str, fence: Option<&crate::hosts::HostFence>) -> bool {
        use crate::lifecycle::start::ports::PreviewPort;
        let stop = PreviewPort::stop_preview_for_bot(self, bot_id);
        match fence {
            Some(fence) => crate::hosts::HostsAccess::hosts(self).run_if_current(fence, stop).await.is_some(),
            None => {
                stop.await;
                true
            }
        }
    }

    fn validate_workspace_path(&self, data_dir: &Path, workspace: &str) -> Option<PathBuf> {
        crate::share::folder::validate_workspace_path(data_dir, workspace)
    }

    async fn is_shared_host(&self, host: &str) -> bool {
        crate::shared_host::is_shared(self, host).await
    }

}

impl s6_ports::QueueServices for Arc<App> {
    async fn turn_changed(&self, turn_id: &str) {
        crate::lifecycle::send_now::ports::turn_changed(&crate::app_ports_p4::AppTurnEvents::new(self), turn_id).await;
    }

    async fn bot_status_changed(&self, bot_id: &str) {
        crate::lifecycle::send_now::ports::bot_status(&crate::app_ports_p4::AppEventSink::new(self), bot_id).await;
    }

    async fn announce_revoked(&self, turn_id: &str, revoked: crate::lifecycle::queue::Revoked) {
        crate::lifecycle::send_now::ports::emit_object(
            &crate::app_ports_p4::AppEventSink::new(self),
            "message_added",
            Some(&revoked.bot_id),
            serde_json::json!({ "bot_id": revoked.bot_id, "message": revoked.message }),
        )
        .await;
        crate::lifecycle::send_now::ports::turn_changed(&crate::app_ports_p4::AppTurnEvents::new(self), turn_id).await;
    }

    async fn note_run_gone(&self, bot_id: &str, why: &str) {
        crate::lifecycle::start_send::note_run_gone(self, bot_id, why).await;
    }

    async fn resume_gate(
        &self,
        bot: &crate::db::Bot,
        run: &crate::db::Run,
        conversation_id: &str,
    ) -> crate::lifecycle::resume_gate::Gate {
        crate::lifecycle::resume_gate::check(self, bot, run, conversation_id).await
    }

    async fn interrupt_grace_left(
        &self,
        bot: &crate::db::Bot,
        run: &crate::db::Run,
        conversation_id: &str,
    ) -> Option<std::time::Duration> {
        crate::lifecycle::interrupt_grace::hold(self, bot, run, conversation_id).await
    }

    async fn pane_ready_for_prompt(
        &self,
        bot: &crate::db::Bot,
        run: &crate::db::Run,
        conversation_id: &str,
    ) -> Result<(), crate::lifecycle::LcError> {
        crate::lifecycle::prompt::pane_ready_for_prompt(self, bot, run, conversation_id).await
    }

    async fn deliver_queued_prompt(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &crate::db::Run,
        bot: &crate::db::Bot,
        text: &str,
        waited_for_log: bool,
    ) -> anyhow::Result<crate::lifecycle::delivery::Delivered> {
        crate::lifecycle::deliver_prompt(self, client, run, bot, text, false, waited_for_log).await
    }

    async fn fail_in_flight(&self, run_id: &str, note: &str) -> anyhow::Result<()> {
        let Some((turn, bot)) = sqlx::query_as::<_, (String, String)>(
            "SELECT t.id, c.bot_id FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE t.run_id=? AND t.status='in_flight'",
        )
        .bind(run_id)
        .fetch_optional(&self.db)
        .await?
        else {
            return Ok(());
        };
        crate::lifecycle::interruption::close_turn(self, &bot, run_id, &turn, note).await
    }

    async fn fail_in_flight_or_owe(&self, run_id: &str, note: &str) -> anyhow::Result<()> {
        let Some((turn, bot)) = sqlx::query_as::<_, (String, String)>(
            "SELECT t.id, c.bot_id FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE t.run_id=? AND t.status='in_flight'",
        )
        .bind(run_id)
        .fetch_optional(&self.db)
        .await?
        else {
            return Ok(());
        };
        crate::lifecycle::interruption::interrupted(self, &bot, run_id, &turn, note).await
    }

    async fn close_owed(&self, bot_id: &str, turn_id: &str, delivery: &'static str, note: &str) -> anyhow::Result<bool> {
        crate::lifecycle::owed_delivery::closed(self, bot_id, turn_id, delivery, note).await
    }

    async fn put_back_owed(&self, bot_id: &str, conversation_id: &str, turn_id: &str, reason: &str, wait_key: &str) -> anyhow::Result<()> {
        crate::lifecycle::owed_delivery::put_back(self, bot_id, conversation_id, turn_id, reason, wait_key).await
    }

    async fn delivered_owed(&self, bot_id: &str, turn_id: &str, record: crate::lifecycle::DeliveryRecord) -> anyhow::Result<()> {
        crate::lifecycle::owed_delivery::delivered(self, bot_id, turn_id, record).await
    }

    async fn arm_stall_and_progress(&self, run_id: &str, bot_id: &str, turn_id: &str) {
        crate::lifecycle::poller::arm_stall(self, run_id, bot_id, turn_id).await;
        crate::lifecycle::poller::arm_progress(self, run_id, bot_id, turn_id).await;
    }

    fn schedule_reconcile_settle(&self, run_id: &str, stuck: &str) {
        crate::lifecycle::run_state::schedule_settle(self, run_id, crate::lifecycle::run_state::Settle::Reconcile { stuck: stuck.to_string() });
    }
}

impl s6_ports::OwedDeliveryServices for Arc<App> {
    async fn mark_delivery(
        &self,
        turn_id: &str,
        record: crate::lifecycle::DeliveryRecord,
        delivered_at: &str,
    ) -> anyhow::Result<()> {
        crate::lifecycle::mark_delivery(self, turn_id, record, delivered_at).await
    }

    async fn queue_put_back(
        &self,
        bot_id: &str,
        conversation_id: &str,
        turn_id: &str,
        reason: &str,
        wait_key: &str,
    ) -> anyhow::Result<()> {
        crate::lifecycle::queue::put_back(self, bot_id, conversation_id, turn_id, reason, wait_key).await
    }

    async fn message_added(&self, bot_id: &str, message: crate::db::Message) {
        crate::lifecycle::messages::emit_message_added(self, bot_id, message).await;
    }

    async fn turn_changed(&self, turn_id: &str) {
        crate::lifecycle::send_now::ports::turn_changed(&crate::app_ports_p4::AppTurnEvents::new(self), turn_id).await;
    }
}

impl s6_ports::InterruptionServices for Arc<App> {
    async fn log_interrupted_since(
        &self,
        bot: &crate::db::Bot,
        run: &crate::db::Run,
        since: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        crate::lifecycle::interrupt_grace::log_interrupted_since(self, bot, run, since).await
    }

    async fn log_interrupted_after(&self, bot: &crate::db::Bot, run: &crate::db::Run, sent: &[String]) -> bool {
        crate::lifecycle::interrupt_grace::log_interrupted_after(self, bot, run, sent).await
    }

    async fn log_shows_prompt_since(
        &self,
        bot: &crate::db::Bot,
        run: &crate::db::Run,
        prompt: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        crate::lifecycle::interrupt_grace::log_shows_prompt_since(self, bot, run, prompt, since).await
    }

}

impl s6_ports::RelayWatchServices for Arc<App> {
    async fn arm_progress(&self, run_id: &str, bot_id: &str, turn_id: &str) {
        crate::lifecycle::poller::arm_progress(self, run_id, bot_id, turn_id).await;
    }

    async fn turn_changed(&self, turn_id: &str) {
        crate::lifecycle::send_now::ports::turn_changed(&crate::app_ports_p4::AppTurnEvents::new(self), turn_id).await;
    }

    async fn mark_run_exited(&self, run_id: &str, reason: &str) -> crate::lc_error::RunExit {
        crate::lifecycle::queue::mark_run_exited(self, run_id, reason).await
    }

    async fn sweep_dead_panes(&self) -> Vec<String> {
        crate::lifecycle::dead_panes::sweep(self).await
    }

    fn spawn_relay_watch(&self, watch: crate::lifecycle::relay_watch::Watch) {
        let tracker = self.background_tasks.clone();
        let app = self.clone();
        tracker.spawn(async move {
            let mut watch = watch;
            loop {
                tokio::select! {
                    _ = app.shutdown.cancelled() => break,
                    _ = tokio::time::sleep(crate::lifecycle::relay_watch::POLL) => {}
                }
                if crate::lifecycle::relay_watch::step(&app, &mut watch, std::time::Instant::now()).await
                    == crate::lifecycle::relay_watch::Step::Done
                {
                    break;
                }
            }
        });
    }
}
