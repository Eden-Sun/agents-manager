//! P6 supervisor 的窄介面由 `App` 實作的地方（crate 拆分第 3 步）。每個方法逐行委派給 supervisor 原本呼叫的函式，不加任何邏輯，
//! 所以行為與錯誤型別不變。介面本身在 `supervisor/ports.rs`；這個檔案是 supervisor 唯一還知道 lifecycle／quota／mission／judge…
//! 實作細節的地方（composition root 側的 adapter，不屬於未來的 am-supervisor-runtime）。
//!
//! 模組掛在 `supervisor/mod.rs`（`#[path]`），不碰 `lib.rs`，免得跟其他包各自加 `mod` 行互相衝突。

use crate::config::IdentityCfg;
use crate::db;
use crate::lifecycle::{self, LcResult, PromptOut, Revoked, StartOpts};
use crate::quota::{self, LimitHit, Quota};
use crate::state::App;
use crate::supervisor::ports::{HostProbes, JudgeOps, LocalAccountView, MissionOps, QuotaOps, TurnOps};
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

impl TurnOps for Arc<App> {
    async fn start_bot(&self, bot_id: &str) -> LcResult<String> {
        lifecycle::start_bot(self, bot_id).await
    }
    async fn start_bot_locked_with(&self, bot_id: &str, opts: StartOpts) -> LcResult<String> {
        lifecycle::start_bot_locked_with(self, bot_id, opts).await
    }
    async fn stop_bot(&self, bot_id: &str) -> LcResult<bool> {
        lifecycle::stop_bot(self, bot_id).await
    }
    async fn stop_bot_locked_if_idle(&self, bot_id: &str) -> LcResult<bool> {
        lifecycle::stop_bot_locked_if_idle(self, bot_id).await
    }
    async fn prompt_control_plane(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>) -> LcResult<PromptOut> {
        lifecycle::prompt_control_plane(self, bot_id, text, client_request_id, relay_from).await
    }
    async fn prompt_relayed(
        &self,
        bot_id: &str,
        text: &str,
        client_request_id: &str,
        attachment_ids: &[String],
        relay_from: Option<&str>,
    ) -> LcResult<PromptOut> {
        lifecycle::prompt_relayed(self, bot_id, text, client_request_id, attachment_ids, relay_from).await
    }
    async fn prompt_relayed_queueable(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>) -> LcResult<PromptOut> {
        lifecycle::prompt_relayed_queueable(self, bot_id, text, client_request_id, relay_from).await
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
    ) -> anyhow::Result<db::Message> {
        lifecycle::insert_message(self, conversation_id, turn_id, role, content, source, incomplete, snapshot).await
    }
    async fn revoke_queued_turn(&self, turn_id: &str, why: &str) -> anyhow::Result<bool> {
        lifecycle::revoke_queued_turn(self, turn_id, why).await
    }
    async fn announce_revoked(&self, turn_id: &str, revoked: Revoked) {
        lifecycle::announce_revoked(self, turn_id, revoked).await
    }
    async fn assignment_withdrawal(&self, turn_id: &str) -> anyhow::Result<Option<String>> {
        lifecycle::assignment_withdrawal(self, turn_id).await
    }
    fn schedule_flush_retry(&self, bot_id: &str, delay: Duration) {
        lifecycle::schedule_flush_retry(self, bot_id, delay)
    }
    async fn blocking_quota_hit(&self, bot: &db::Bot, turn_id: &str) -> Option<LimitHit> {
        lifecycle::quota_hold::blocking_hit(self, bot, turn_id).await
    }
}

impl QuotaOps for Arc<App> {
    async fn try_limit_hit_for_bot(&self, bot: &db::Bot) -> anyhow::Result<Option<LimitHit>> {
        crate::runners::quota::try_limit_hit_for_bot(self, bot).await
    }
    async fn next_reset_for_bot(&self, bot: &db::Bot) -> Option<String> {
        crate::runners::quota::next_reset_for_bot(self, bot).await
    }
    async fn billing_identity(&self, bot: &db::Bot) -> anyhow::Result<Option<String>> {
        quota::billing_identity(self, bot).await
    }
    async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String {
        quota::quota_base_for_host(self, host, kind, identity).await
    }
    async fn seed_limit_hit(&self, host: &str, base: &str, until: &str, message: &str, bucket: Option<String>) -> bool {
        quota::seed_limit_hit(self, host, base, until, message, bucket).await
    }
    async fn running_model(&self, bot: &db::Bot) -> Option<String> {
        quota::running_model(self, bot).await
    }
    async fn limit_cleared_since(&self, bot: &db::Bot, since: chrono::DateTime<chrono::Utc>) -> bool {
        crate::runners::quota::limit_cleared_since(self, bot, since).await
    }
    async fn quota_snapshot_json(&self) -> Value {
        quota::snapshot(self).await
    }
    async fn quota_reading(&self, key: &str) -> Option<Quota> {
        self.quotas.lock().await.get(key).cloned()
    }
}

impl MissionOps for Arc<App> {
    async fn mission(&self, mission_id: &str) -> anyhow::Result<Option<crate::mission::store::Mission>> {
        crate::mission::store::get(&self.db, mission_id).await
    }
    async fn mission_candidates(&self, host: &str, kind: &str) -> anyhow::Result<Vec<(String, bool, Option<Quota>)>> {
        crate::mission::candidates(self, host, kind).await
    }
    async fn mission_billing_identity(&self, host: &str, bot: &db::Bot) -> anyhow::Result<Option<String>> {
        crate::mission::billing_identity_named(self, host, bot).await
    }
    async fn mission_add_event(&self, mission_id: &str, kind: &str, text: &str, relay_from: Option<&str>, payload: &Value) -> anyhow::Result<()> {
        crate::mission::store::add_event(&self.db, mission_id, kind, text, relay_from, payload).await.map(|_| ())
    }
    async fn mission_pause_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        mission_id: &str,
        reason: &str,
        detail: Option<&str>,
        text: &str,
        payload: &Value,
    ) -> anyhow::Result<bool> {
        crate::mission::store::pause_on(tx, mission_id, reason, detail, text, payload).await
    }
    async fn ensure_can_assign(&self, mission_id: &str, role: &str) -> Result<(), crate::lifecycle::LcError> {
        crate::mission::workflow::ensure_can_assign(self, mission_id, role).await
    }
    async fn mission_next_json(&self, mission_id: &str) -> Value {
        crate::mission::workflow::next_json(self, mission_id).await
    }
    async fn mission_wake_stalled(&self) {
        crate::mission::workflow::wake_stalled(self).await
    }
    async fn mission_sweep_closed_temp_bots(&self) {
        crate::mission::api::sweep_closed_mission_temp_bots(self).await
    }
}

impl JudgeOps for Arc<App> {
    async fn judge_shadow_settled(&self, assignment_id: &str, bot_id: &str, turn_id: Option<&str>, turn_status: &str, result: Option<&str>) {
        crate::runners::judge::shadow_settled(self, assignment_id, bot_id, turn_id, turn_status, result).await
    }
    fn judge_stuck_sweep(&self) {
        crate::runners::judge::sweep(self)
    }
    async fn judge_schedule_assignment(&self, assignment_id: &str) {
        crate::judge::collision::schedule_assignment(self, assignment_id).await
    }
}

impl HostProbes for Arc<App> {
    async fn pane_shows_login_problem(&self, run: &db::Run) -> Option<bool> {
        crate::app_ports_p4::shows_login_problem(self, run).await
    }
    async fn process_dump(&self, host: &str) -> anyhow::Result<String> {
        crate::memproc::dump(self, host).await
    }
    async fn identity_for_host(&self, host: &str, name: &str) -> Option<IdentityCfg> {
        crate::tools::identity_for_host(self, host, name).await
    }
    async fn deploy_behind(&self) -> Result<Value, String> {
        let ctx = crate::deploy_now::Ctx::of(self);
        crate::deploy_now::behind(&ctx.repo, &ctx.live_sha).await
    }
    async fn release_triage_health_probe(&self, cfg: &crate::config::ReleaseTriageCfg) -> Option<Value> {
        crate::release_triage::issue::health_probe(cfg).await
    }
    async fn due_actions_snapshot(&self) -> Value {
        crate::due_actions::snapshot(self).await
    }
    fn primary_keep_warm_tick(&self) {
        crate::runners::primary_keep_warm::tick(self)
    }
    async fn restart_loop<F, Fut>(&self, name: &'static str, factory: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        crate::background_loop::restart_loop(self.shutdown.clone(), name, factory).await
    }
    fn spawn_restartable<F, Fut>(&self, name: &'static str, factory: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        crate::background_loop::spawn_restartable(self, name, factory)
    }
    async fn is_share_bot(&self, bot_id: &str) -> anyhow::Result<bool> {
        Ok(crate::share::store::is_share_bot(&self.db, bot_id).await?)
    }
}

impl<T: LocalAccountView + ?Sized> LocalAccountView for Arc<T> {
    fn background_jobs_known(&self, run_id: &str) -> Option<u32> {
        (**self).background_jobs_known(run_id)
    }
    fn background_jobs_duration(&self, run_id: &str) -> Option<(i64, i64, bool)> {
        (**self).background_jobs_duration(run_id)
    }
    fn deploy_user_escalated_for(&self, approval: &crate::supervisor::store::Approval) -> bool {
        (**self).deploy_user_escalated_for(approval)
    }
}

impl LocalAccountView for App {
    fn background_jobs_known(&self, run_id: &str) -> Option<u32> {
        crate::background_jobs::known(self, run_id)
    }
    fn background_jobs_duration(&self, run_id: &str) -> Option<(i64, i64, bool)> {
        crate::background_jobs::duration(self, run_id)
    }
    fn deploy_user_escalated_for(&self, approval: &crate::supervisor::store::Approval) -> bool {
        crate::deploy_wait::user_escalated_for(self, approval)
    }
}
