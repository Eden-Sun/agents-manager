//! `Arc<T>` forwarding for ports whose composition host is stored behind an `Arc`.

use crate::supervisor::ports::{
    BotLamp, CandidateSwitch, ClassifyFailureState, ConfigProjection, ControllerRuntime, DaemonConnection, HostProbes, IncidentState,
    JudgeOps, QuotaOps, TurnOps,
};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

impl<T: TurnOps + ?Sized> TurnOps for Arc<T> {
    async fn start_bot(&self, bot_id: &str) -> crate::lifecycle::LcResult<String> {
        (**self).start_bot(bot_id).await
    }
    async fn start_bot_locked_with(&self, bot_id: &str, opts: crate::lifecycle::StartOpts) -> crate::lifecycle::LcResult<String> {
        (**self).start_bot_locked_with(bot_id, opts).await
    }
    async fn stop_bot(&self, bot_id: &str) -> crate::lifecycle::LcResult<bool> {
        (**self).stop_bot(bot_id).await
    }
    fn stop_bot_locked_if_idle<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = crate::lifecycle::LcResult<bool>> + Send + 'a {
        (**self).stop_bot_locked_if_idle(bot_id)
    }
    async fn prompt_control_plane(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>) -> crate::lifecycle::LcResult<crate::lifecycle::PromptOut> {
        (**self).prompt_control_plane(bot_id, text, client_request_id, relay_from).await
    }
    async fn prompt_relayed(
        &self,
        bot_id: &str,
        text: &str,
        client_request_id: &str,
        attachment_ids: &[String],
        relay_from: Option<&str>,
    ) -> crate::lifecycle::LcResult<crate::lifecycle::PromptOut> {
        (**self).prompt_relayed(bot_id, text, client_request_id, attachment_ids, relay_from).await
    }
    async fn prompt_relayed_queueable(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>, queue_class: crate::lifecycle::QueueClass) -> crate::lifecycle::LcResult<crate::lifecycle::PromptOut> {
        (**self).prompt_relayed_queueable(bot_id, text, client_request_id, relay_from, queue_class).await
    }
    fn insert_message<'a>(
        &'a self,
        conversation_id: &'a str,
        turn_id: Option<&'a str>,
        role: &'a str,
        content: &'a str,
        source: &'a str,
        incomplete: bool,
        snapshot: Option<&'a str>,
    ) -> impl Future<Output = anyhow::Result<crate::db::Message>> + Send + 'a {
        (**self).insert_message(conversation_id, turn_id, role, content, source, incomplete, snapshot)
    }
    async fn revoke_queued_turn(&self, turn_id: &str, why: &str) -> anyhow::Result<bool> {
        (**self).revoke_queued_turn(turn_id, why).await
    }
    async fn announce_revoked(&self, turn_id: &str, revoked: crate::lifecycle::Revoked) {
        (**self).announce_revoked(turn_id, revoked).await
    }
    async fn assignment_withdrawal(&self, turn_id: &str) -> anyhow::Result<Option<String>> {
        (**self).assignment_withdrawal(turn_id).await
    }
    fn schedule_flush_retry(&self, bot_id: &str, delay: Duration) {
        (**self).schedule_flush_retry(bot_id, delay)
    }
    async fn blocking_quota_hit(&self, bot: &crate::db::Bot, turn_id: &str) -> Option<crate::quota::LimitHit> {
        (**self).blocking_quota_hit(bot, turn_id).await
    }
}

impl<T: ConfigProjection + ?Sized> ConfigProjection for Arc<T> {
    fn update_and_project<'a, F, U>(&'a self, f: F) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<U>> + Send + 'a>>
    where
        F: FnOnce(&mut crate::config::ConfigFile) -> anyhow::Result<U> + Send + 'a,
        U: Send + 'a,
    {
        (**self).update_and_project(f)
    }
}

impl<T: QuotaOps + ?Sized> QuotaOps for Arc<T> {
    async fn try_limit_hit_for_bot(&self, bot: &crate::db::Bot) -> anyhow::Result<Option<crate::quota::LimitHit>> {
        (**self).try_limit_hit_for_bot(bot).await
    }
    async fn next_reset_for_bot(&self, bot: &crate::db::Bot) -> Option<String> {
        (**self).next_reset_for_bot(bot).await
    }
    async fn billing_identity(&self, bot: &crate::db::Bot) -> anyhow::Result<Option<String>> {
        <T as QuotaOps>::billing_identity(self.as_ref(), bot).await
    }
    async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String {
        <T as QuotaOps>::quota_base_for_host(self.as_ref(), host, kind, identity).await
    }
    async fn seed_limit_hit(&self, host: &str, base: &str, until: &str, message: &str, bucket: Option<String>) -> bool {
        (**self).seed_limit_hit(host, base, until, message, bucket).await
    }
    async fn running_model(&self, bot: &crate::db::Bot) -> Option<String> {
        (**self).running_model(bot).await
    }
    async fn limit_cleared_since(&self, bot: &crate::db::Bot, since: chrono::DateTime<chrono::Utc>) -> bool {
        (**self).limit_cleared_since(bot, since).await
    }
    async fn quota_snapshot_json(&self) -> serde_json::Value {
        (**self).quota_snapshot_json().await
    }
    async fn quota_reading(&self, key: &str) -> Option<crate::quota::Quota> {
        (**self).quota_reading(key).await
    }
}

impl<T: JudgeOps + ?Sized> JudgeOps for Arc<T> {
    async fn judge_shadow_settled(&self, assignment_id: &str, bot_id: &str, turn_id: Option<&str>, turn_status: &str, result: Option<&str>) {
        (**self).judge_shadow_settled(assignment_id, bot_id, turn_id, turn_status, result).await
    }
    fn judge_stuck_sweep(&self) {
        (**self).judge_stuck_sweep()
    }
    async fn judge_schedule_assignment(&self, assignment_id: &str) {
        (**self).judge_schedule_assignment(assignment_id).await
    }
}

impl<T: HostProbes + ?Sized> HostProbes for Arc<T> {
    async fn pane_shows_login_problem(&self, run: &crate::db::Run) -> Option<bool> {
        (**self).pane_shows_login_problem(run).await
    }
    fn process_dump<'a>(&'a self, host: &'a str) -> impl Future<Output = anyhow::Result<String>> + Send + 'a {
        (**self).process_dump(host)
    }
    async fn identity_for_host(&self, host: &str, name: &str) -> Option<crate::config::IdentityCfg> {
        (**self).identity_for_host(host, name).await
    }
    async fn deploy_behind(&self) -> Result<serde_json::Value, String> {
        (**self).deploy_behind().await
    }
    async fn release_triage_health_probe(&self, cfg: &crate::config::ReleaseTriageCfg) -> Option<serde_json::Value> {
        (**self).release_triage_health_probe(cfg).await
    }
    async fn due_actions_snapshot(&self) -> serde_json::Value {
        (**self).due_actions_snapshot().await
    }
    fn primary_keep_warm_tick(&self) {
        (**self).primary_keep_warm_tick()
    }
    async fn restart_loop<F, Fut>(&self, name: &'static str, factory: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        (**self).restart_loop(name, factory).await
    }
    fn spawn_restartable<F, Fut>(&self, name: &'static str, factory: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        (**self).spawn_restartable(name, factory)
    }
    fn is_share_bot<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = anyhow::Result<bool>> + Send + 'a {
        (**self).is_share_bot(bot_id)
    }
}

impl<T: ControllerRuntime + ?Sized> ControllerRuntime for Arc<T> {
    fn spawn_supervisor_controller(&self, generation: i64) {
        (**self).spawn_supervisor_controller(generation)
    }
}

impl<T: CandidateSwitch + ?Sized> CandidateSwitch for Arc<T> {
    async fn switch_supervisor_candidate(&self, next: &str, reason: &str, reset_at: Option<&str>) -> Result<bool, crate::lifecycle::LcError> {
        (**self).switch_supervisor_candidate(next, reason, reset_at).await
    }
}

impl<T: ClassifyFailureState + ?Sized> ClassifyFailureState for Arc<T> {
    fn classify_failure_count(&self) -> &std::sync::atomic::AtomicU32 {
        (**self).classify_failure_count()
    }
}

impl<T: IncidentState + ?Sized> IncidentState for Arc<T> {
    fn spool_fold_stuck(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (u32, i64)>> {
        (**self).spool_fold_stuck()
    }
    fn remote_shim_stale(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, String>> {
        (**self).remote_shim_stale()
    }
}

impl<T: DaemonConnection + ?Sized> DaemonConnection for Arc<T> {
    fn daemon_connected(&self) -> bool {
        (**self).daemon_connected()
    }
}

impl<T: BotLamp + ?Sized> BotLamp for Arc<T> {
    fn bot_lamp<'a>(&'a self, bot_id: &'a str, run: Option<&'a crate::db::Run>) -> impl Future<Output = &'static str> + Send + 'a {
        (**self).bot_lamp(bot_id, run)
    }
}

impl<T: crate::supervisor::role_faults::RoleFaultTable + ?Sized> crate::supervisor::role_faults::RoleFaultTable for Arc<T> {
    fn role_faults(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::supervisor::role_faults::RoleFault>> {
        (**self).role_faults()
    }
}

impl<T: crate::supervisor::watchdog::WatchdogDeadlines + ?Sized> crate::supervisor::watchdog::WatchdogDeadlines for Arc<T> {
    fn watchdog_deadlines(&self) -> &std::sync::Mutex<crate::supervisor::watchdog::DeadlineCache> {
        (**self).watchdog_deadlines()
    }
}
