//! P0：把 `App` 上「feature 自己的狀態欄位」包成各 feature 自己定義的單一能力 trait（`App` 只在這個 composition 檔出現）。
//! 這些 trait 各在擁有該狀態的模組裡，所以 feature 不必依賴 `capabilities`／`state`；欄位照舊住在 `App`，行為不變。

use crate::state::App;
use std::sync::Arc;

impl crate::quota::QuotaTables for App {
    fn quotas(&self) -> &tokio::sync::Mutex<std::collections::BTreeMap<String, crate::quota::Quota>> {
        &self.quotas
    }
}

impl crate::quota::QuotaStaleKeys for App {
    fn quota_stale(&self) -> &tokio::sync::Mutex<std::collections::BTreeSet<String>> {
        &self.quota_stale
    }
}

impl crate::quota::HostIdentities for App {
    async fn identity_for_host(&self, host: &str, name: &str) -> Option<crate::config::IdentityCfg> {
        crate::tools::identity_for_host_ref(self, host, name).await
    }
    async fn host_tools_detected(&self, host: &str) -> bool {
        self.tools.lock().await.contains_key(host)
    }
}

impl crate::background_jobs::JobCounts for App {
    fn background_jobs(&self) -> &crate::background_jobs::Counts {
        &self.background_jobs
    }
}
impl<T: crate::background_jobs::JobCounts + ?Sized> crate::background_jobs::JobCounts for Arc<T> {
    fn background_jobs(&self) -> &crate::background_jobs::Counts {
        (**self).background_jobs()
    }
}

impl crate::background_hook::HookSnapshots for App {
    fn background_hook(&self) -> &crate::background_hook::Snapshots {
        &self.background_hook
    }
}

impl crate::deploy_wait::DeployWaitState for App {
    fn deploy_wait(&self) -> &std::sync::Mutex<Option<crate::deploy_wait::Wait>> {
        &self.deploy_wait
    }
}
impl<T: crate::deploy_wait::DeployWaitState + ?Sized> crate::deploy_wait::DeployWaitState for Arc<T> {
    fn deploy_wait(&self) -> &std::sync::Mutex<Option<crate::deploy_wait::Wait>> {
        (**self).deploy_wait()
    }
}

impl crate::api::shell::HostShells for App {
    fn host_shells(&self) -> &crate::api::shell::Registry {
        &self.host_shells
    }
}
impl<T: crate::api::shell::HostShells + ?Sized> crate::api::shell::HostShells for Arc<T> {
    fn host_shells(&self) -> &crate::api::shell::Registry {
        (**self).host_shells()
    }
}

impl crate::login_assist::LoginPanes for App {
    fn login_panes(&self) -> &crate::login_assist::Registry {
        &self.login_panes
    }
}

impl crate::login_prompt::LoginNeeded for App {
    fn login_needed(&self) -> &crate::login_prompt::Registry {
        &self.login_needed
    }
}

impl crate::gh_auth::GhDevice for App {
    fn gh_device(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::gh_auth::DeviceSession>> {
        &self.gh_device
    }
}
impl<T: crate::gh_auth::GhDevice + ?Sized> crate::gh_auth::GhDevice for Arc<T> {
    fn gh_device(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::gh_auth::DeviceSession>> {
        (**self).gh_device()
    }
}

impl crate::judge::JudgeFuse for App {
    fn judge_fuse(&self) -> &tokio::sync::Mutex<()> {
        &self.judge_fuse
    }
}
impl<T: crate::judge::JudgeFuse + ?Sized> crate::judge::JudgeFuse for Arc<T> {
    fn judge_fuse(&self) -> &tokio::sync::Mutex<()> {
        (**self).judge_fuse()
    }
}

impl crate::supervisor::role_faults::RoleFaultTable for App {
    fn role_faults(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::supervisor::role_faults::RoleFault>> {
        &self.role_faults
    }
}
impl<T: crate::supervisor::role_faults::RoleFaultTable + ?Sized> crate::supervisor::role_faults::RoleFaultTable for Arc<T> {
    fn role_faults(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::supervisor::role_faults::RoleFault>> {
        (**self).role_faults()
    }
}

impl crate::github::GithubCache for App {
    fn github(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, Option<crate::github::GithubInfo>>> {
        &self.github
    }
}

impl crate::tools::ToolsTable for App {
    fn tools(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::tools::HostTools>> {
        &self.tools
    }
}

impl crate::build_scheduler::BuildSlotLock for App {
    fn build_slot_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.build_slot_lock
    }
}

impl crate::supervisor::watchdog::WatchdogDeadlines for App {
    fn watchdog_deadlines(&self) -> &std::sync::Mutex<crate::supervisor::watchdog::DeadlineCache> {
        &self.watchdog_deadlines
    }
}
impl<T: crate::supervisor::watchdog::WatchdogDeadlines + ?Sized> crate::supervisor::watchdog::WatchdogDeadlines for Arc<T> {
    fn watchdog_deadlines(&self) -> &std::sync::Mutex<crate::supervisor::watchdog::DeadlineCache> {
        (**self).watchdog_deadlines()
    }
}

impl crate::upstream_update::UpstreamWatch for App {
    fn upstream_watch(&self) -> &crate::upstream_update::Watch {
        &self.upstream_watch
    }
}

impl crate::tui_prompts::SurveyRevisions for App {
    fn survey_revisions(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>> {
        &self.survey_revisions
    }
}
impl<T: crate::tui_prompts::SurveyRevisions + ?Sized> crate::tui_prompts::SurveyRevisions for Arc<T> {
    fn survey_revisions(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>> {
        (**self).survey_revisions()
    }
}

impl crate::shim_refresh::RemoteShimStale for App {
    fn remote_shim_stale(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, String>> {
        &self.remote_shim_stale
    }
}

impl crate::lifecycle::poller::ProgressPollers for App {
    fn progress_pollers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>> {
        &self.progress_pollers
    }
}
impl<T: crate::lifecycle::poller::ProgressPollers + ?Sized> crate::lifecycle::poller::ProgressPollers for Arc<T> {
    fn progress_pollers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>> {
        (**self).progress_pollers()
    }
}

impl crate::credential_spawn::CredentialSpawnGate for App {
    fn credential_spawn_gate(&self) -> &std::sync::Mutex<crate::credential_spawn::Gate> {
        &self.credential_spawn_gate
    }
}

impl crate::changelog::ChangelogState for App {
    fn changelog(&self) -> &crate::changelog::ChangelogCache {
        &self.changelog
    }
}

impl crate::events::PaneWatchers for App {
    fn pane_watchers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<(String, String, String), tokio::task::JoinHandle<()>>> {
        &self.pane_watchers
    }
}
impl<T: crate::events::PaneWatchers + ?Sized> crate::events::PaneWatchers for Arc<T> {
    fn pane_watchers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<(String, String, String), tokio::task::JoinHandle<()>>> {
        (**self).pane_watchers()
    }
}

impl crate::herdr::LocalHerdr for App {
    fn default_herdr(&self) -> &crate::herdr::HerdrClient {
        &self.default_herdr
    }
    fn herdr_session(&self) -> &String {
        &self.herdr_session
    }
    fn default_connected(&self) -> &std::sync::atomic::AtomicBool {
        &self.default_connected
    }
}


impl crate::lifecycle::poller::StallTimers for App {
    fn stall_timers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>> {
        &self.stall_timers
    }
}
impl<T: crate::lifecycle::poller::StallTimers + ?Sized> crate::lifecycle::poller::StallTimers for Arc<T> {
    fn stall_timers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>> {
        (**self).stall_timers()
    }
}


impl crate::lifecycle::poller::ProgressEmitted for App {
    fn progress_emitted(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
        &self.progress_emitted
    }
}
impl<T: crate::lifecycle::poller::ProgressEmitted + ?Sized> crate::lifecycle::poller::ProgressEmitted for Arc<T> {
    fn progress_emitted(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
        (**self).progress_emitted()
    }
}

impl crate::lifecycle::poller::FallbackTimers for App {
    fn fallback_timers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>> {
        &self.fallback_timers
    }
}
impl<T: crate::lifecycle::poller::FallbackTimers + ?Sized> crate::lifecycle::poller::FallbackTimers for Arc<T> {
    fn fallback_timers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>> {
        (**self).fallback_timers()
    }
}

impl crate::models::ModelsCache for App {
    fn models_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, serde_json::Value)>> {
        &self.models_cache
    }
}





impl crate::hookrecv::SpoolFoldStuck for App {
    fn spool_fold_stuck(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (u32, i64)>> {
        &self.spool_fold_stuck
    }
}
impl<T: crate::hookrecv::SpoolFoldStuck + ?Sized> crate::hookrecv::SpoolFoldStuck for Arc<T> {
    fn spool_fold_stuck(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (u32, i64)>> {
        (**self).spool_fold_stuck()
    }
}

impl crate::login_assist::LoginReservations for App {
    fn login_reservations(&self) -> &crate::login_assist::Reservations {
        &self.login_reservations
    }
}



impl crate::hookrecv::ClassifyFailures for App {
    fn classify_failures(&self) -> &std::sync::atomic::AtomicU32 {
        &self.classify_failures
    }
}
impl<T: crate::hookrecv::ClassifyFailures + ?Sized> crate::hookrecv::ClassifyFailures for Arc<T> {
    fn classify_failures(&self) -> &std::sync::atomic::AtomicU32 {
        (**self).classify_failures()
    }
}

impl crate::host_baseline::HostBaselineTable for App {
    fn host_baseline(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, crate::host_baseline::BaselineReport>> {
        &self.host_baseline
    }
}

impl crate::default_session::DefaultSyncLock for App {
    fn default_sync_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.default_sync_lock
    }
}
impl<T: crate::default_session::DefaultSyncLock + ?Sized> crate::default_session::DefaultSyncLock for Arc<T> {
    fn default_sync_lock(&self) -> &tokio::sync::Mutex<()> {
        (**self).default_sync_lock()
    }
}


impl crate::github::SubmodulesCache for App {
    fn submodules_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Vec<crate::github::Submodule>)>> {
        &self.submodules_cache
    }
}

impl crate::github::IssuesCache for App {
    fn issues_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, serde_json::Value)>> {
        &self.issues_cache
    }
}

impl crate::kind_probe::KindProbeState for App {
    fn kind_probe(&self) -> &crate::kind_probe::KindProbeHook {
        &self.kind_probe
    }
}





impl crate::codex_history::CodexHistoryState for App {
    fn codex_history(&self) -> &crate::codex_history::HistoryHook {
        &self.codex_history
    }
}
impl<T: crate::codex_history::CodexHistoryState + ?Sized> crate::codex_history::CodexHistoryState for Arc<T> {
    fn codex_history(&self) -> &crate::codex_history::HistoryHook {
        (**self).codex_history()
    }
}




impl crate::api::shell::HostShellOpenLocks for App {
    fn host_shell_open_locks(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>> {
        &self.host_shell_open_locks
    }
}
impl<T: crate::api::shell::HostShellOpenLocks + ?Sized> crate::api::shell::HostShellOpenLocks for Arc<T> {
    fn host_shell_open_locks(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>> {
        (**self).host_shell_open_locks()
    }
}

impl crate::capabilities::Db for App {
    fn db(&self) -> &sqlx::SqlitePool {
        &self.db
    }
}
impl crate::capabilities::Emit for App {
    fn emit(&self, kind: &str, data: serde_json::Value) -> impl std::future::Future<Output = ()> + Send {
        App::emit(self, kind, data)
    }
    fn current_seq(&self) -> u64 {
        App::current_seq(self)
    }
}
impl crate::capabilities::DataDir for App {
    fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }
}
impl crate::capabilities::Cfg for App {
    fn cfg(&self) -> &crate::config::ConfigStore {
        &self.cfg
    }
}
impl crate::capabilities::BotLocks for App {
    fn bot_lock(&self, bot_id: &str) -> impl std::future::Future<Output = Arc<tokio::sync::Mutex<()>>> + Send {
        App::bot_lock(self, bot_id)
    }
}
impl crate::capabilities::BotStatusEmit for App {
    fn emit_bot_status(&self, bot_id: &str) -> impl std::future::Future<Output = ()> + Send {
        App::emit_bot_status(self, bot_id)
    }
}
impl crate::capabilities::BootId for App {
    fn boot_id(&self) -> &str {
        &self.boot_id
    }
}
impl crate::capabilities::HerdrRoutes for App {
    fn session_for_host(&self, host: &str) -> impl std::future::Future<Output = Option<String>> + Send {
        App::session_for_host(self, host)
    }
    fn herdr_for_session(&self, host: &str, session: &str) -> impl std::future::Future<Output = Option<crate::herdr::HerdrClient>> + Send {
        App::herdr_for_session(self, host, session)
    }
    fn session_connected(&self, host: &str, session: &str) -> impl std::future::Future<Output = bool> + Send {
        App::session_connected(self, host, session)
    }
    fn bot_connected(&self, bot_id: &str) -> impl std::future::Future<Output = bool> + Send {
        App::bot_connected(self, bot_id)
    }
    fn session_for_run(&self, run: &crate::db::Run) -> impl std::future::Future<Output = Option<String>> + Send {
        App::session_for_run(self, run)
    }
    fn herdr_for_run(&self, run: &crate::db::Run) -> impl std::future::Future<Output = Option<crate::herdr::HerdrClient>> + Send {
        App::herdr_for_run(self, run)
    }
    fn host_connected(&self, host: &str) -> impl std::future::Future<Output = bool> + Send {
        App::host_connected(self, host)
    }
    fn herdr_for(&self, host: &str) -> impl std::future::Future<Output = Option<crate::herdr::HerdrClient>> + Send {
        App::herdr_for(self, host)
    }
    fn session_for_bot_with_host_fence(
        &self,
        bot: &crate::db::Bot,
        host: &str,
        fence: &crate::hosts::HostFence,
    ) -> impl std::future::Future<Output = Option<String>> + Send {
        App::session_for_bot_with_host_fence(self, bot, host, fence)
    }
    fn herdr_for_host_fence(&self, fence: &crate::hosts::HostFence, session: &str) -> impl std::future::Future<Output = Option<crate::herdr::HerdrClient>> + Send {
        App::herdr_for_host_fence(self, fence, session)
    }
}
impl crate::capabilities::Shutdown for App { fn shutdown(&self) -> &tokio_util::sync::CancellationToken { &self.shutdown } }
impl crate::capabilities::BgTasks for App { fn background_tasks(&self) -> &tokio_util::task::TaskTracker { &self.background_tasks } }
impl crate::capabilities::ExePath for App { fn exe(&self) -> &std::path::Path { &self.exe } }
impl crate::capabilities::ListenPort for App { fn port(&self) -> u16 { self.port } }
impl crate::capabilities::Isolation for App {
    fn isolated(&self) -> bool {
        App::isolated(self)
    }
}
impl crate::capabilities::UiToken for App {
    fn ui_token(&self) -> &String {
        &self.ui_token
    }
}

impl crate::judge::stuck::StuckPaneReader for App {
    async fn read_pane_plain_text(&self, client: &crate::herdr::HerdrClient, pane_id: &str, kind: &str) -> anyhow::Result<String> {
        crate::app_ports_p12::read_pane_plain_text(client, pane_id, kind).await
    }
}
impl<T: crate::judge::stuck::StuckPaneReader + ?Sized> crate::judge::stuck::StuckPaneReader for Arc<T> {
    async fn read_pane_plain_text(&self, client: &crate::herdr::HerdrClient, pane_id: &str, kind: &str) -> anyhow::Result<String> {
        (**self).read_pane_plain_text(client, pane_id, kind).await
    }
}

impl crate::judge::report::ReportNotifier for App {
    async fn insert_system_message(&self, conv_id: &str, note: &str) -> anyhow::Result<crate::db::Message> {
        crate::app_ports_p12::insert_system_message(self, conv_id, note).await
    }
}
impl<T: crate::judge::report::ReportNotifier + ?Sized> crate::judge::report::ReportNotifier for Arc<T> {
    async fn insert_system_message(&self, conv_id: &str, note: &str) -> anyhow::Result<crate::db::Message> {
        (**self).insert_system_message(conv_id, note).await
    }
}

impl crate::reconcile::PaneIdentitySync for Arc<App> {
    async fn sync_child_identity(&self, host: &str, bot: &crate::db::Bot, pane_id: &str, pid: Option<i64>) {
        crate::runners::pane_identity::sync_child_identity(self, host, bot, pane_id, pid).await;
    }
    async fn claude_default_effort(&self, host: &str, identity: Option<&str>, alias: &str) -> anyhow::Result<String> {
        crate::models::claude_default_effort(self, host, identity, alias).await
    }
}


