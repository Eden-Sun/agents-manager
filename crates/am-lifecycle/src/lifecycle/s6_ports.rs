//! P6a lifecycle callbacks whose implementations belong to the daemon composition layer.

#![allow(async_fn_in_trait)]

use crate::{db, lifecycle::LcError};
use serde_json::Value;
use std::{future::Future, path::{Path, PathBuf}};

pub trait DeadPanesServices: Send + Sync {
    fn maintenance_window_active(&self) -> impl Future<Output = bool> + Send;
}

pub trait TranscriptOriginServices: Send + Sync {
    fn started_by_the_cli_itself(
        &self,
        bot: &db::Bot,
        transcript_path: Option<&str>,
    ) -> impl Future<Output = bool> + Send;
}

pub trait StuckTurnServices: Send + Sync {
    fn retain_supervisor_runs(&self, active: &[String]);
    fn sweep_child_alerts(&self) -> impl Future<Output = ()> + Send;
    fn sweep_child_done(&self) -> impl Future<Output = ()> + Send;
    fn local_transcript_allowed(&self, bot: &db::Bot, raw_path: &str) -> impl Future<Output = bool> + Send;
    fn codex_exact_reply(&self, bot: &db::Bot, run: &db::Run, sent: &[String]) -> impl Future<Output = Option<String>> + Send;
    fn codex_home(&self, bot: &db::Bot) -> impl Future<Output = Option<PathBuf>> + Send;
    fn emit_turn(&self, turn_id: &str) -> impl Future<Output = ()> + Send;
}

pub trait StuckTurnContext:
    crate::capabilities::Db
    + crate::capabilities::BotLocks
    + crate::capabilities::Emit
    + crate::hosts::HostsAccess
    + DeadPanesServices
    + QueueContext
    + StuckTurnServices
    + Clone
    + Send
    + Sync
    + 'static
{}

impl<T> StuckTurnContext for T where
    T: crate::capabilities::Db
        + crate::capabilities::BotLocks
        + crate::capabilities::Emit
        + crate::hosts::HostsAccess
        + DeadPanesServices
        + QueueContext
        + StuckTurnServices
        + Clone
        + Send
        + Sync
        + 'static
{}

pub trait SetupShareServices: Send + Sync {
    fn cage_settings(&self, settings: &mut Value, workspace: &str, env: &Value);
    /// 寫一個私有檔進遠端 bot 目錄（0600），寫完比對 sha256；不符是錯誤（不啟動）。寫入綁在呼叫端捕捉的主機權威上（#1026）。
    fn write_remote_private_file<'a>(
        &'a self,
        fence: &'a am_base::hosts::HostFence,
        dir: &'a str,
        name: &'a str,
        data: &'a [u8],
    ) -> impl Future<Output = anyhow::Result<()>> + Send + 'a;
}

pub trait StartServices: SetupShareServices + Send + Sync {
    fn lock_bot_for_start<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = Box<dyn am_ports::BotLockGuard + 'static>> + Send + 'a;
    fn session_for_bot(&self, bot: &db::Bot, host: &str) -> impl Future<Output = Option<String>> + Send;
    fn prepare_restricted_bot(
        &self,
        bot: &db::Bot,
        host: &str,
    ) -> impl Future<Output = Result<Option<String>, LcError>> + Send;
    fn cage_environment<'a>(
        &'a self,
        env: &'a mut Value,
        bot: &'a db::Bot,
        host: &'a str,
    ) -> impl Future<Output = ()> + Send + 'a;
    /// 系統提示寫好後的路徑（遠端就是那台的路徑）。
    fn install_restricted_prompt<'a>(
        &'a self,
        bot: &'a db::Bot,
        host: &'a str,
        workspace: &'a str,
        env: &'a Value,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'a;
    fn restricted_launch_args(&self, env: &Value, prompt: &Path) -> Vec<String>;
    fn pane_env(
        &self,
        bot: &db::Bot,
        host: &str,
        run_id: &str,
        agent_name: &str,
        shim_dir: Option<&str>,
        fence: Option<&crate::hosts::HostFence>,
    ) -> impl Future<Output = anyhow::Result<Value>> + Send;
    fn pretrust_for_start(&self, bot: &db::Bot, host: &str, cwd: &str) -> impl Future<Output = Vec<String>> + Send;
    fn apply_grok_startup_effort(
        &self,
        bot: &db::Bot,
        run_id: &str,
        pane_id: &str,
        client: &crate::herdr::HerdrClient,
    ) -> impl Future<Output = std::result::Result<(), String>> + Send;
    fn flush_once_running(&self, bot_id: &str, run_id: &str);
    #[cfg(test)]
    fn spawn_adopted_capture(&self, run_id: &str, bot_id: &str);
    fn watch_pane_on_session(&self, host: &str, session: &str, pane: &str) -> impl Future<Output = ()> + Send;
    fn session_connected_with_host_fence(&self, fence: &crate::hosts::HostFence, session: &str) -> impl Future<Output = bool> + Send;
    fn emit_lifecycle_event(&self, kind: &str, bot: Option<&str>, payload: Value) -> impl Future<Output = ()> + Send;
}

pub trait IdentityAccess: Send + Sync {
    fn identity_for_host(
        &self,
        host: &str,
        name: &str,
    ) -> impl Future<Output = Option<crate::config::IdentityCfg>> + Send;
    fn cached_identity_logged_out(&self, host: &str, name: &str) -> impl Future<Output = bool> + Send;
    fn recheck_identity_login(&self, host: &str, name: &str) -> impl Future<Output = Option<bool>> + Send;
}

pub trait TurnErrorServices: Send + Sync {
    fn next_reset_for_bot(&self, bot: &db::Bot) -> impl Future<Output = Option<String>> + Send;
    fn read_run_pane_recent_unwrapped(
        &self,
        run: &db::Run,
        pane: &str,
        lines: usize,
    ) -> impl Future<Output = anyhow::Result<Option<String>>> + Send;
    fn host_utc_offset_secs(&self, host: &str) -> impl Future<Output = Option<i32>> + Send;
    fn turn_changed(&self, turn_id: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
}

pub trait TurnErrorQuotaAccess: Send + Sync {
    fn resolve_key<'a>(
        &'a self,
        host: &'a str,
        provider: &'a str,
        identity: Option<&'a str>,
    ) -> impl Future<Output = Result<am_core::QuotaKey, am_core::PortError>> + Send + 'a;
    fn snapshot<'a>(
        &'a self,
        key: &'a am_core::QuotaKey,
    ) -> impl Future<Output = Result<Option<am_core::QuotaSnapshot>, am_core::PortError>> + Send + 'a;
    fn store_snapshot<'a>(
        &'a self,
        key: &'a am_core::QuotaKey,
        snapshot: am_core::QuotaSnapshot,
    ) -> impl Future<Output = Result<(), am_core::PortError>> + Send + 'a;
}

pub trait TurnErrorContext:
    crate::capabilities::Db
    + crate::capabilities::Emit
    + crate::capabilities::BotStatusEmit
    + crate::capabilities::BootId
    + TurnErrorQuotaAccess
    + QuotaHoldServices
    + TurnErrorServices
    + Send
    + Sync
{}

impl<T> TurnErrorContext for T where
    T: crate::capabilities::Db
        + crate::capabilities::Emit
        + crate::capabilities::BotStatusEmit
        + crate::capabilities::BootId
        + TurnErrorQuotaAccess
        + QuotaHoldServices
        + TurnErrorServices
        + Send
        + Sync
{}

pub trait RunStateServices:
    crate::capabilities::Db
    + crate::capabilities::BotLocks
    + crate::capabilities::BotStatusEmit
    + Send
    + Sync
    + 'static
{
    fn reconcile_host(&self, host: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn finish_stop(&self, run_id: &str) -> impl Future<Output = ()> + Send;
}

pub trait LiveApplyDebtContext:
    crate::capabilities::Db
    + crate::capabilities::Emit
    + crate::capabilities::BotStatusEmit
    + Send
    + Sync
    + 'static
{
    fn stamp_live_revision(&self, pool: &sqlx::SqlitePool, run_id: &str) -> impl Future<Output = Result<bool, sqlx::Error>> + Send;
}

pub trait DeferredLiveContext:
    crate::capabilities::Db + crate::capabilities::Emit + Send + Sync + 'static
{
    fn apply_live_setting_with_revision(
        &self,
        bot_id: &str,
        fields: &[&'static str],
        baseline_rev: &str,
        target_rev: &str,
    ) -> impl Future<Output = super::LiveApplyOutcome> + Send;
}

pub trait GrokTranscriptServices: Send + Sync {
    fn host_shell(&self, host: &str, script: &str) -> impl Future<Output = anyhow::Result<String>> + Send;
    fn grok_home_for(&self, bot: &db::Bot, host: &str) -> impl Future<Output = anyhow::Result<String>> + Send;
    fn pids_in_pane(&self, host: &str, pane: &str, session: Option<&str>) -> impl Future<Output = Vec<i32>> + Send;
    fn agy_session_load(&self, run: &db::Run, host: &str) -> impl Future<Output = anyhow::Result<Option<(String, String)>>> + Send;
    fn consume_resume_session(&self, bot: &db::Bot, run: &db::Run, session: Option<&str>) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn agy_session_record_status(&self, bot: &db::Bot, run: &db::Run, transcript: &str) -> impl Future<Output = ()> + Send;
    fn emit_turn(&self, turn_id: &str) -> impl Future<Output = ()> + Send;
    /// herdr 綁在這顆 run 的 claude agent 上的 session id（沒綁＝`None`；#878）。
    fn claude_herdr_session(&self, run: &db::Run) -> impl Future<Output = Option<String>> + Send;
    /// 這顆 claude run 的 config 目錄候選（`projects/` 的上一層），依序找（#878）。
    fn claude_config_roots(&self, bot: &db::Bot, run: &db::Run, host: &str) -> impl Future<Output = Vec<String>> + Send;
}

pub trait GrokTranscriptContext:
    crate::capabilities::Db
    + crate::capabilities::BotStatusEmit
    + crate::capabilities::Emit
    + crate::hosts::HostsAccess
    + GrokTranscriptServices
    + Clone
    + Send
    + Sync
    + 'static
{}

impl<T> GrokTranscriptContext for T where
    T: crate::capabilities::Db
        + crate::capabilities::BotStatusEmit
        + crate::capabilities::Emit
        + crate::hosts::HostsAccess
        + GrokTranscriptServices
        + Clone
        + Send
        + Sync
        + 'static
{}

pub trait QuotaHoldServices: Send + Sync {
    fn try_limit_hit_for_bot(&self, bot: &db::Bot) -> impl Future<Output = anyhow::Result<Option<crate::quota::LimitHit>>> + Send;
    fn running_model(&self, bot: &db::Bot) -> impl Future<Output = Option<String>> + Send;
    fn billing_identity(&self, bot: &db::Bot) -> impl Future<Output = anyhow::Result<Option<String>>> + Send;
    fn limit_cleared_since(&self, bot: &db::Bot, since: chrono::DateTime<chrono::Utc>) -> impl Future<Output = bool> + Send;
    fn host_target(&self, host: &str) -> impl Future<Output = Option<String>> + Send;
    fn restore_limit_hit(&self, host: &str, base: &str, hit: crate::quota::LimitHit) -> impl Future<Output = bool> + Send;
    fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> impl Future<Output = String> + Send;
    fn schedule_queue_flush(&self, bot_id: &str);
    fn schedule_flush_retry(&self, bot_id: &str, delay: std::time::Duration);
}

pub trait QuotaHoldContext:
    crate::capabilities::Db + crate::capabilities::BootId + QuotaHoldServices + Clone + Send + Sync + 'static
{}

impl<T> QuotaHoldContext for T where
    T: crate::capabilities::Db + crate::capabilities::BootId + QuotaHoldServices + Clone + Send + Sync + 'static
{}

pub trait BusySendServices: crate::lifecycle::send_now::ports::SupervisorSendRepo + Send + Sync {
    fn prompt_message_added(&self, bot_id: &str, message_id: &str) -> impl Future<Output = ()> + Send;
    fn turn_changed(&self, turn_id: &str) -> impl Future<Output = ()> + Send;
}

pub trait CodexSteerServices: Send + Sync {
    fn plan_delivery(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &db::Run,
        bot: &db::Bot,
        text: &str,
        force_pane: bool,
        waited_for_log: bool,
    ) -> impl Future<Output = anyhow::Result<Result<crate::lifecycle::delivery::Plan, crate::lifecycle::delivery::Delivered>>> + Send;
    fn execute_delivery(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &db::Run,
        bot: &db::Bot,
        text: &str,
        plan: crate::lifecycle::delivery::Plan,
    ) -> impl Future<Output = anyhow::Result<crate::lifecycle::delivery::Delivered>> + Send;
}

pub trait CodexSteerContext:
    crate::capabilities::Db + crate::capabilities::Emit + CodexSteerServices + Send + Sync
{}

impl<T> CodexSteerContext for T where T: crate::capabilities::Db + crate::capabilities::Emit + CodexSteerServices + Send + Sync {}

pub trait SendNowServices: Send + Sync {
    fn prepare_delivery(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &db::Run,
        bot: &db::Bot,
        text: &str,
        plan: crate::lifecycle::delivery::Plan,
    ) -> impl Future<Output = Result<crate::lifecycle::delivery::Ready, crate::lifecycle::delivery::Delivered>> + Send;
    fn type_delivery_text(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &db::Run,
        bot: &db::Bot,
        text: &str,
        ready: crate::lifecycle::delivery::Ready,
    ) -> impl Future<Output = anyhow::Result<crate::lifecycle::delivery::Typing>> + Send;
    fn send_now_landed(
        &self,
        client: &crate::herdr::HerdrClient,
        run: &db::Run,
        bot: &db::Bot,
        text: &str,
        typed: &crate::lifecycle::delivery::Typed,
    ) -> impl Future<Output = crate::lifecycle::delivery::Landed> + Send;
    fn confirm_send_now(
        &self,
        client: &crate::herdr::HerdrClient,
        run: &db::Run,
        bot: &db::Bot,
        text: &str,
        typed: &crate::lifecycle::delivery::Typed,
    ) -> impl Future<Output = anyhow::Result<crate::lifecycle::delivery::Delivered>> + Send;
}

pub trait SendNowContext: InterruptionContext + SendNowServices {}

impl<T> SendNowContext for T where T: InterruptionContext + SendNowServices {}

pub trait SuggestionServices: Send + Sync {
    fn submit_gates(
        &self,
        bot_id: &str,
        bot: &db::Bot,
        conversation_id: &str,
    ) -> impl Future<Output = Result<db::Run, LcError>> + Send;
    fn client_for_run(&self, run: &db::Run) -> impl Future<Output = Result<crate::lifecycle::RunClient, LcError>> + Send;
    fn submit_locked(
        &self,
        bot_id: &str,
        token: &str,
        client_request_id: &str,
    ) -> impl Future<Output = Result<crate::lifecycle::PromptOut, crate::lifecycle::composer_draft::SubmitDraftError>> + Send;
}

pub trait SuggestionContext:
    crate::capabilities::Db
    + crate::capabilities::BotLocks
    + crate::capabilities::BotStatusEmit
    + InterruptionContext
    + OwedDeliveryContext
    + SuggestionServices
    + Send
    + Sync
{}

impl<T> SuggestionContext for T where
    T: crate::capabilities::Db
        + crate::capabilities::BotLocks
        + crate::capabilities::BotStatusEmit
        + InterruptionContext
        + OwedDeliveryContext
        + SuggestionServices
        + Send
        + Sync
{}

pub trait ResumeNudgeServices: Send + Sync {
    fn identity_config_dir(
        &self,
        host: &str,
        identity: Option<&str>,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;
    fn queue_nudge(
        &self,
        conversation_id: &str,
        bot_id: &str,
        text: &str,
        client_request_id: &str,
        relay_from: &str,
    ) -> impl Future<Output = anyhow::Result<crate::lifecycle::PromptOut>> + Send;
    fn schedule_queue_flush(&self, bot_id: &str);
}

pub trait ResumeNudgeContext:
    crate::capabilities::Db + crate::capabilities::BotLocks + crate::hosts::HostsAccess + ResumeNudgeServices + Clone + Send + Sync + 'static
{}

impl<T> ResumeNudgeContext for T where
    T: crate::capabilities::Db + crate::capabilities::BotLocks + crate::hosts::HostsAccess + ResumeNudgeServices + Clone + Send + Sync + 'static
{}

pub trait ScreenServices: Send + Sync {
    fn read_run_pane_recent_unwrapped(
        &self,
        run: &db::Run,
        pane: &str,
        lines: usize,
    ) -> impl Future<Output = anyhow::Result<Option<(String, u64)>>> + Send;
    fn insert_system_notice(&self, conversation_id: &str, notice: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn post_codex_security_banner_notice(&self, run: &db::Run, notice: &str) -> impl Future<Output = ()> + Send;
    fn push_codex_security_banner_alert(&self, run: &db::Run, reason: &str) -> impl Future<Output = ()> + Send;
    fn shadow_limit_hit(&self, sample: crate::judge::Sample) -> impl Future<Output = ()> + Send;
    fn mark_codex_limit_hit(&self, bot: &db::Bot, notice: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn fail_in_flight_turn(&self, turn_id: &str, note: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn apply_codex_limit_hit_quota(
        &self,
        host: &str,
        base: &str,
        hit: crate::quota::LimitHit,
    ) -> impl Future<Output = crate::quota::LimitHit> + Send;
}

pub trait ScreenContext: crate::capabilities::Db + crate::capabilities::BotLocks + ScreenServices + Clone + Send + Sync + 'static {}

impl<T> ScreenContext for T where T: crate::capabilities::Db + crate::capabilities::BotLocks + ScreenServices + Clone + Send + Sync + 'static {}

pub trait StopServices: Send + Sync {
    fn lock_bot_for_stop<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = Box<dyn am_ports::BotLockGuard + 'static>> + Send + 'a;
    fn notify_bot_status(&self, bot_id: &str) -> impl Future<Output = ()> + Send;
    fn publish_lifecycle_event(&self, kind: &str, payload: Value) -> impl Future<Output = ()> + Send;
    fn refresh_background_jobs(&self, run: &db::Run, kind: &str) -> impl Future<Output = ()> + Send;
    fn background_jobs_count(&self, run_id: &str) -> Option<u32>;
    fn fail_in_flight(&self, run_id: &str, note: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn note_user_interrupt_of(
        &self,
        bot: &db::Bot,
        run: &db::Run,
        in_flight: Option<&db::Turn>,
    ) -> impl Future<Output = ()> + Send;
    fn announce_revoked(&self, turn_id: &str, revoked: crate::lifecycle::Revoked) -> impl Future<Output = ()> + Send;
    fn revoke_orphaned_queued_turns(&self, bot_id: &str, why: &str) -> impl Future<Output = Vec<String>> + Send;
    fn stop_preview(&self, bot_id: &str, fence: Option<&crate::hosts::HostFence>) -> impl Future<Output = bool> + Send;
    /// 受限分享 bot 的工作目錄裡，哪些是 daemon 自己建的、可以收進回收區；使用者的既有資料夾回 `None`（#900）。
    fn validate_workspace_path(&self, data_dir: &Path, workspace: &str) -> impl Future<Output = Option<PathBuf>> + Send;
    fn is_shared_host(&self, host: &str) -> impl Future<Output = bool> + Send;
}

pub trait StopContext:
    crate::capabilities::Db
    + crate::capabilities::DataDir
    + crate::capabilities::BotLocks
    + crate::capabilities::BotStatusEmit
    + crate::capabilities::Emit
    + crate::capabilities::HerdrRoutes
    + crate::hosts::HostsAccess
    + crate::lifecycle::start::ports::PaneWatchPort
    + crate::lifecycle::start::ports::RemoteCleanupPort
    + crate::lifecycle::start::ports::HandoffSessionRepo
    + crate::lifecycle::start::ports::ShareSessionRepo
    + RunStateServices
    + InterruptionContext
    + OwedDeliveryContext
    + StopServices
    + Clone
    + Send
    + Sync
    + 'static
{}

impl<T> StopContext for T where
    T: crate::capabilities::Db
        + crate::capabilities::DataDir
        + crate::capabilities::BotLocks
        + crate::capabilities::BotStatusEmit
        + crate::capabilities::Emit
        + crate::capabilities::HerdrRoutes
        + crate::hosts::HostsAccess
        + crate::lifecycle::start::ports::PaneWatchPort
        + crate::lifecycle::start::ports::RemoteCleanupPort
        + crate::lifecycle::start::ports::HandoffSessionRepo
        + crate::lifecycle::start::ports::ShareSessionRepo
        + RunStateServices
        + InterruptionContext
        + OwedDeliveryContext
        + StopServices
        + Clone
        + Send
        + Sync
        + 'static
{}

pub trait QueueServices: Send + Sync {
    fn turn_changed(&self, turn_id: &str) -> impl Future<Output = ()> + Send;
    fn bot_status_changed(&self, bot_id: &str) -> impl Future<Output = ()> + Send;
    fn announce_revoked(&self, turn_id: &str, revoked: crate::lifecycle::queue::Revoked) -> impl Future<Output = ()> + Send;
    fn note_run_gone(&self, bot_id: &str, why: &str) -> impl Future<Output = ()> + Send;
    fn resume_gate(
        &self,
        bot: &db::Bot,
        run: &db::Run,
        conversation_id: &str,
    ) -> impl Future<Output = crate::lifecycle::resume_gate::Gate> + Send;
    fn interrupt_grace_left(
        &self,
        bot: &db::Bot,
        run: &db::Run,
        conversation_id: &str,
    ) -> impl Future<Output = Option<std::time::Duration>> + Send;
    fn pane_ready_for_prompt(&self, bot: &db::Bot, run: &db::Run, conversation_id: &str) -> impl Future<Output = Result<(), crate::lifecycle::LcError>> + Send;
    fn deliver_queued_prompt(
        &self,
        client: &crate::lifecycle::RunClient,
        run: &db::Run,
        bot: &db::Bot,
        text: &str,
        waited_for_log: bool,
    ) -> impl Future<Output = anyhow::Result<crate::lifecycle::delivery::Delivered>> + Send;
    fn fail_in_flight(&self, run_id: &str, note: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn fail_in_flight_or_owe(&self, run_id: &str, note: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn close_owed(&self, bot_id: &str, turn_id: &str, delivery: &'static str, note: &str) -> impl Future<Output = anyhow::Result<bool>> + Send;
    fn put_back_owed(&self, bot_id: &str, conversation_id: &str, turn_id: &str, reason: &str, wait_key: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn delivered_owed(&self, bot_id: &str, turn_id: &str, record: crate::lifecycle::DeliveryRecord) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn arm_stall_and_progress(&self, run_id: &str, bot_id: &str, turn_id: &str) -> impl Future<Output = ()> + Send;
    fn schedule_reconcile_settle(&self, run_id: &str, stuck: &str);
}

pub trait QueueContext:
    crate::capabilities::Db
    + crate::capabilities::HerdrRoutes
    + crate::hosts::HostsAccess
    + crate::capabilities::BotLocks
    + crate::lifecycle::send_now::ports::MaintenancePort
    + crate::lifecycle::send_now::ports::PaneWatchPort
    + crate::lifecycle::send_now::ports::HandoffSendRepo
    + crate::lifecycle::send_now::ports::SupervisorSendRepo
    + QuotaHoldContext
    + QueueServices
    + Clone
    + Send
    + Sync
    + 'static
{}

pub trait OwedDeliveryServices: Send + Sync {
    fn mark_delivery(&self, turn_id: &str, record: crate::lifecycle::DeliveryRecord, delivered_at: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn queue_put_back(
        &self,
        bot_id: &str,
        conversation_id: &str,
        turn_id: &str,
        reason: &str,
        wait_key: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn message_added(&self, bot_id: &str, message: db::Message) -> impl Future<Output = ()> + Send;
    fn turn_changed(&self, turn_id: &str) -> impl Future<Output = ()> + Send;
}

pub trait OwedDeliveryContext:
    crate::capabilities::Db + crate::capabilities::BotLocks + OwedDeliveryServices + Clone + Send + Sync + 'static
{}

impl<T> OwedDeliveryContext for T where
    T: crate::capabilities::Db + crate::capabilities::BotLocks + OwedDeliveryServices + Clone + Send + Sync + 'static
{}

pub trait InterruptionServices: Send + Sync {
    fn log_interrupted_since(&self, bot: &db::Bot, run: &db::Run, since: chrono::DateTime<chrono::Utc>) -> impl Future<Output = bool> + Send;
    fn log_interrupted_after(&self, bot: &db::Bot, run: &db::Run, sent: &[String]) -> impl Future<Output = bool> + Send;
    fn log_shows_prompt_since(
        &self,
        bot: &db::Bot,
        run: &db::Run,
        prompt: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> impl Future<Output = bool> + Send;
}

pub trait InterruptionContext:
    crate::capabilities::Db
    + crate::capabilities::Emit
    + crate::capabilities::BotLocks
    + crate::capabilities::HerdrRoutes
    + crate::hosts::HostsAccess
    + InterruptionServices
    + OwedDeliveryServices
    + Clone
    + Send
    + Sync
    + 'static
{}

impl<T> InterruptionContext for T where
    T: crate::capabilities::Db
        + crate::capabilities::Emit
        + crate::capabilities::BotLocks
        + crate::capabilities::HerdrRoutes
        + crate::hosts::HostsAccess
        + InterruptionServices
        + OwedDeliveryServices
        + Clone
        + Send
        + Sync
        + 'static
{}

pub trait RelayWatchServices: Send + Sync {
    fn arm_progress(&self, run_id: &str, bot_id: &str, turn_id: &str) -> impl Future<Output = ()> + Send;
    fn turn_changed(&self, turn_id: &str) -> impl Future<Output = ()> + Send;
    fn mark_run_exited(&self, run_id: &str, reason: &str) -> impl Future<Output = crate::lc_error::RunExit> + Send;
    fn sweep_dead_panes(&self) -> impl Future<Output = Vec<String>> + Send;
    fn spawn_relay_watch(&self, watch: crate::lifecycle::relay_watch::Watch);
}

pub trait RelayWatchContext:
    crate::capabilities::Db
    + crate::capabilities::BotLocks
    + crate::capabilities::Emit
    + crate::capabilities::HerdrRoutes
    + crate::hosts::HostsAccess
    + RelayWatchServices
    + Clone
    + Send
    + Sync
    + 'static
{}

impl<T> RelayWatchContext for T where
    T: crate::capabilities::Db
        + crate::capabilities::BotLocks
        + crate::capabilities::Emit
        + crate::capabilities::HerdrRoutes
        + crate::hosts::HostsAccess
        + RelayWatchServices
        + Clone
        + Send
        + Sync
        + 'static
{}

pub trait TurnEventHostServices: Send + Sync {
    fn child_done_after_completed_turn(&self, turn_id: &str);
    fn publish_lifecycle_turn(&self, bot_id: &str, turn_id: &str, status: &str, delivery: &str);
}

pub trait InterruptGraceHostServices: Send + Sync {
    fn codex_interrupted_after<'a>(
        &'a self,
        bot: &'a db::Bot,
        run: &'a db::Run,
        sent: &'a [String],
    ) -> impl Future<Output = bool> + Send + 'a;
}

#[derive(Debug, Clone)]
pub struct OpenRestartIntent {
    pub id: String,
    pub kind: String,
    pub subject_id: String,
}

pub trait RestartHoldHostServices: Send + Sync {
    fn open_restart_intents(&self) -> impl Future<Output = anyhow::Result<Vec<OpenRestartIntent>>> + Send;
}

impl<T> QueueContext for T where
    T: crate::capabilities::Db
        + crate::capabilities::HerdrRoutes
        + crate::hosts::HostsAccess
        + crate::capabilities::BotLocks
        + crate::lifecycle::send_now::ports::MaintenancePort
        + crate::lifecycle::send_now::ports::PaneWatchPort
        + crate::lifecycle::send_now::ports::HandoffSendRepo
        + crate::lifecycle::send_now::ports::SupervisorSendRepo
        + QuotaHoldContext
        + QueueServices
        + Clone
        + Send
        + Sync
        + 'static
{}
