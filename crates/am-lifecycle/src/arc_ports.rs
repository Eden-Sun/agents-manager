//! Blanket adapters let daemon-owned Arc contexts use lifecycle-owned narrow ports.
#![allow(unused_imports)]

mod arc_port_0_spoolfoldstuck {
    use crate::hookrecv::*;
    use crate::events::ports::{ApiPort, HandoffRepo, MessageTxOps, ProviderPort, QuotaCommands, TurnCommands, TurnConnOps, TurnFenceOps};
    use crate::ask_answers;
    use crate::db;
    use crate::config::{valid_id, ID_RE};
    use crate::lifecycle;
    use crate::hosts::sh_quote;
    impl<T: crate::hookrecv::SpoolFoldStuck + ?Sized> crate::hookrecv::SpoolFoldStuck for std::sync::Arc<T> {
        fn spool_fold_stuck(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (u32, i64)>> { <T as crate::hookrecv::SpoolFoldStuck>::spool_fold_stuck(self.as_ref()) }
    }
}

mod arc_port_1_classifyfailures {
    use crate::hookrecv::*;
    use crate::events::ports::{ApiPort, HandoffRepo, MessageTxOps, ProviderPort, QuotaCommands, TurnCommands, TurnConnOps, TurnFenceOps};
    use crate::ask_answers;
    use crate::db;
    use crate::config::{valid_id, ID_RE};
    use crate::lifecycle;
    use crate::hosts::sh_quote;
    impl<T: crate::hookrecv::ClassifyFailures + ?Sized> crate::hookrecv::ClassifyFailures for std::sync::Arc<T> {
        fn classify_failures(&self) -> &std::sync::atomic::AtomicU32 { <T as crate::hookrecv::ClassifyFailures>::classify_failures(self.as_ref()) }
    }
}

mod arc_port_2_hookhost {
    use crate::hookrecv::*;
    use crate::events::ports::{ApiPort, HandoffRepo, MessageTxOps, ProviderPort, QuotaCommands, TurnCommands, TurnConnOps, TurnFenceOps};
    use crate::ask_answers;
    use crate::db;
    use crate::config::{valid_id, ID_RE};
    use crate::lifecycle;
    use crate::hosts::sh_quote;
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::hookrecv::HookHost + ?Sized> crate::hookrecv::HookHost for std::sync::Arc<T> {
        fn wake_hook_inbox(&self) { <T as crate::hookrecv::HookHost>::wake_hook_inbox(self.as_ref()) }
        fn hook_bot_dir(&self, bot_id: &str) -> Result<std::path::PathBuf> { <T as crate::hookrecv::HookHost>::hook_bot_dir(self.as_ref(), bot_id) }
        fn after_turn_end<'a>(&'a self, body: &'a HookBody) -> impl std::future::Future<Output = ()> + Send + 'a { <T as crate::hookrecv::HookHost>::after_turn_end(self.as_ref(), body) }
        fn background_stop<'a>(&'a self, run: &'a db::Run, payload: &'a Value) -> impl std::future::Future<Output = ()> + Send + 'a { <T as crate::hookrecv::HookHost>::background_stop(self.as_ref(), run, payload) }
        fn transcript_allowed<'a>(&'a self, bot: &'a db::Bot, path: &'a str) -> impl std::future::Future<Output = bool> + Send + 'a { <T as crate::hookrecv::HookHost>::transcript_allowed(self.as_ref(), bot, path) }
        fn local_transcript_allowed<'a>(&'a self, bot: &'a db::Bot, path: &'a str) -> impl std::future::Future<Output = bool> + Send + 'a { <T as crate::hookrecv::HookHost>::local_transcript_allowed(self.as_ref(), bot, path) }
    }
}

mod arc_port_3_panewatchers {
    use crate::events::*;
    use std::sync::Arc;
    use std::time::Duration;
    impl<T: crate::events::PaneWatchers + ?Sized> crate::events::PaneWatchers for std::sync::Arc<T> {
        fn pane_watchers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<(String, String, String), tokio::task::JoinHandle<()>>> { <T as crate::events::PaneWatchers>::pane_watchers(self.as_ref()) }
    }
}

mod arc_port_4_turncommands {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::TurnCommands + ?Sized> crate::events::ports::TurnCommands for std::sync::Arc<T> {
        async fn emit_message_added(&self, bot_id: &str, message: db::Message) { <T as crate::events::ports::TurnCommands>::emit_message_added(self.as_ref(), bot_id, message).await }
        async fn emit_turn(&self, turn_id: &str) { <T as crate::events::ports::TurnCommands>::emit_turn(self.as_ref(), turn_id).await }
        fn schedule_flush_queued(&self, bot_id: &str) { <T as crate::events::ports::TurnCommands>::schedule_flush_queued(self.as_ref(), bot_id) }
        fn schedule_deferred_live(&self, bot_id: &str) { <T as crate::events::ports::TurnCommands>::schedule_deferred_live(self.as_ref(), bot_id) }
        fn schedule_codex_notice_capture(&self, bot_id: &str, run_id: &str) { <T as crate::events::ports::TurnCommands>::schedule_codex_notice_capture(self.as_ref(), bot_id, run_id) }
        fn poke_resume_nudge(&self, bot_id: &str) { <T as crate::events::ports::TurnCommands>::poke_resume_nudge(self.as_ref(), bot_id) }
        async fn cancel_stall(&self, run_id: &str) { <T as crate::events::ports::TurnCommands>::cancel_stall(self.as_ref(), run_id).await }
        async fn arm_fallback(&self, run_id: &str, bot_id: &str) { <T as crate::events::ports::TurnCommands>::arm_fallback(self.as_ref(), run_id, bot_id).await }
        async fn arm_progress(&self, run_id: &str, bot_id: &str, turn_id: &str) { <T as crate::events::ports::TurnCommands>::arm_progress(self.as_ref(), run_id, bot_id, turn_id).await }
        async fn has_progress_poller(&self, run_id: &str) -> bool { <T as crate::events::ports::TurnCommands>::has_progress_poller(self.as_ref(), run_id).await }
        async fn arm_stall(&self, run_id: &str, bot_id: &str, turn_id: &str) { <T as crate::events::ports::TurnCommands>::arm_stall(self.as_ref(), run_id, bot_id, turn_id).await }
        async fn begin_external_turn(&self, run: &db::Run) { <T as crate::events::ports::TurnCommands>::begin_external_turn(self.as_ref(), run).await }
        async fn mark_run_exited(&self, run_id: &str, reason: &str) -> RunExit { <T as crate::events::ports::TurnCommands>::mark_run_exited(self.as_ref(), run_id, reason).await }
        async fn context_lost(&self, bot: &db::Bot, why: &str, failed_session: Option<&str>) -> LcResult<()> { <T as crate::events::ports::TurnCommands>::context_lost(self.as_ref(), bot, why, failed_session).await }
        async fn retire_context_lost(&self, bot_id: &str, session_id: &str) { <T as crate::events::ports::TurnCommands>::retire_context_lost(self.as_ref(), bot_id, session_id).await }
        async fn settle_interruption(&self, bot_id: &str, evidence: InterruptEvidence) -> Result<()> { <T as crate::events::ports::TurnCommands>::settle_interruption(self.as_ref(), bot_id, evidence).await }
        async fn settle_owed_deliveries(&self, bot_id: &str) -> Result<()> { <T as crate::events::ports::TurnCommands>::settle_owed_deliveries(self.as_ref(), bot_id).await }
        async fn settle_interrupt_echo(
            &self,
            bot_id: &str,
            run_id: &str,
            ev: &InterruptFailureEvidence<'_>,
            in_flight: Option<&db::Turn>,
        ) -> Result<bool> { <T as crate::events::ports::TurnCommands>::settle_interrupt_echo(self.as_ref(), bot_id, run_id, ev, in_flight).await }
        async fn start_bot(&self, bot_id: &str) -> LcResult<String> { <T as crate::events::ports::TurnCommands>::start_bot(self.as_ref(), bot_id).await }
        async fn start_bot_locked_with(&self, bot_id: &str, opts: StartOpts) -> LcResult<String> { <T as crate::events::ports::TurnCommands>::start_bot_locked_with(self.as_ref(), bot_id, opts).await }
        async fn resume_after_boot(&self, host: &str) -> usize { <T as crate::events::ports::TurnCommands>::resume_after_boot(self.as_ref(), host).await }
        async fn adopt_unbound_send_nows(&self, boot: &str) -> bool { <T as crate::events::ports::TurnCommands>::adopt_unbound_send_nows(self.as_ref(), boot).await }
        async fn rearm_queue_retries(&self) -> Result<usize> { <T as crate::events::ports::TurnCommands>::rearm_queue_retries(self.as_ref()).await }
        async fn adopt_turns_of_ended_runs(&self, boot: &str) -> bool { <T as crate::events::ports::TurnCommands>::adopt_turns_of_ended_runs(self.as_ref(), boot).await }
        async fn rearm_queued_prompt_restamps(&self) -> Result<()> { <T as crate::events::ports::TurnCommands>::rearm_queued_prompt_restamps(self.as_ref()).await }
        async fn adopt_interrupted_on_restart(&self, run: &db::Run, turn: &db::Turn) -> Result<bool> { <T as crate::events::ports::TurnCommands>::adopt_interrupted_on_restart(self.as_ref(), run, turn).await }
        fn spawn_adopted_capture(&self, run_id: &str, bot_id: &str) { <T as crate::events::ports::TurnCommands>::spawn_adopted_capture(self.as_ref(), run_id, bot_id) }
        async fn sweep_stuck_turns(&self, host: Option<&str>) -> Vec<String> { <T as crate::events::ports::TurnCommands>::sweep_stuck_turns(self.as_ref(), host).await }
        async fn close_after_session_paused(&self, run_id: &str, expected_turn_id: &str) -> Option<String> { <T as crate::events::ports::TurnCommands>::close_after_session_paused(self.as_ref(), run_id, expected_turn_id).await }
        async fn close_pane_and_tab(&self, client: &crate::herdr::HerdrClient, workspace_id: Option<&str>, tab_id: Option<&str>, pane_id: &str) { <T as crate::events::ports::TurnCommands>::close_pane_and_tab(self.as_ref(), client, workspace_id, tab_id, pane_id).await }
        fn observe_agent_status(&self, run_id: &str, agent_status: &str) { <T as crate::events::ports::TurnCommands>::observe_agent_status(self.as_ref(), run_id, agent_status) }
        async fn insert_message(&self, conversation_id: &str, turn_id: Option<&str>, role: &str, content: &str, source: &str, incomplete: bool, snapshot: Option<&str>) -> Result<db::Message> { <T as crate::events::ports::TurnCommands>::insert_message(self.as_ref(), conversation_id, turn_id, role, content, source, incomplete, snapshot).await }
        async fn prompt_relayed_queueable(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>) -> LcResult<PromptOut> { <T as crate::events::ports::TurnCommands>::prompt_relayed_queueable(self.as_ref(), bot_id, text, client_request_id, relay_from).await }
    }
}

mod arc_port_7_turnfenceops {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::TurnFenceOps + ?Sized> crate::events::ports::TurnFenceOps for std::sync::Arc<T> {
        async fn classify_event_owner(&self, bot_id: &str, run: &db::Run, ev: EventIdentity<'_>) -> Ownership { <T as crate::events::ports::TurnFenceOps>::classify_event_owner(self.as_ref(), bot_id, run, ev).await }
    }
}

mod arc_port_8_quotacommands {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::QuotaCommands + ?Sized> crate::events::ports::QuotaCommands for std::sync::Arc<T> {
        async fn clear_limit_hit_for_bot(&self, bot: &db::Bot) { <T as crate::events::ports::QuotaCommands>::clear_limit_hit_for_bot(self.as_ref(), bot).await }
        async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String { <T as crate::events::ports::QuotaCommands>::quota_base_for_host(self.as_ref(), host, kind, identity).await }
        async fn set_quota(&self, host: &str, base: &str, q: Quota) { <T as crate::events::ports::QuotaCommands>::set_quota(self.as_ref(), host, base, q).await }
        async fn mark_claude_limit_hit(&self, bot: &db::Bot, line: &str) -> Result<()> { <T as crate::events::ports::QuotaCommands>::mark_claude_limit_hit(self.as_ref(), bot, line).await }
        async fn mark_agy_limit_hit(&self, bot: &db::Bot, line: &str) -> Result<()> { <T as crate::events::ports::QuotaCommands>::mark_agy_limit_hit(self.as_ref(), bot, line).await }
    }
}

mod arc_port_9_supervisorsignals {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::SupervisorSignals + ?Sized> crate::events::ports::SupervisorSignals for std::sync::Arc<T> {
        fn observe_idle_status(&self, run_id: &str, status: &str) { <T as crate::events::ports::SupervisorSignals>::observe_idle_status(self.as_ref(), run_id, status) }
    }
}

mod arc_port_10_handoffrepo {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::HandoffRepo + ?Sized> crate::events::ports::HandoffRepo for std::sync::Arc<T> {
        async fn bot_handed_off_to(&self, bot_id: &str) -> Result<Option<String>> { <T as crate::events::ports::HandoffRepo>::bot_handed_off_to(self.as_ref(), bot_id).await }
        async fn handoff_footprint(&self, host: &str) -> Result<Footprint> { <T as crate::events::ports::HandoffRepo>::handoff_footprint(self.as_ref(), host).await }
    }
}

mod arc_port_12_botopsport {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::BotOpsPort + ?Sized> crate::events::ports::BotOpsPort for std::sync::Arc<T> {
        async fn recover_restart_intents(&self, host: &str) { <T as crate::events::ports::BotOpsPort>::recover_restart_intents(self.as_ref(), host).await }
        async fn recover_delete_intents(&self, host: &str) { <T as crate::events::ports::BotOpsPort>::recover_delete_intents(self.as_ref(), host).await }
        async fn recover_promote_intents(&self, host: &str) { <T as crate::events::ports::BotOpsPort>::recover_promote_intents(self.as_ref(), host).await }
        async fn scan_panes_snapshot(&self, host: &str, snapshot: &Value) -> Result<ScanOutcome> { <T as crate::events::ports::BotOpsPort>::scan_panes_snapshot(self.as_ref(), host, snapshot).await }
        async fn gc_panes(&self, host: &str) -> Result<usize> { <T as crate::events::ports::BotOpsPort>::gc_panes(self.as_ref(), host).await }
        async fn notify_unowned_and_orphans(&self, host: &str) -> Result<usize> { <T as crate::events::ports::BotOpsPort>::notify_unowned_and_orphans(self.as_ref(), host).await }
    }
}

mod arc_port_14_hostsideport {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::HostSidePort + ?Sized> crate::events::ports::HostSidePort for std::sync::Arc<T> {
        async fn refresh_herdr_version(&self, host: &str) { <T as crate::events::ports::HostSidePort>::refresh_herdr_version(self.as_ref(), host).await }
        async fn herdr_maintenance_active(&self) -> Result<Option<Window>> { <T as crate::events::ports::HostSidePort>::herdr_maintenance_active(self.as_ref()).await }
        fn spawn_detect_github_host(&self, host: String) { <T as crate::events::ports::HostSidePort>::spawn_detect_github_host(self.as_ref(), host) }
        async fn emit_daemon_status(&self) { <T as crate::events::ports::HostSidePort>::emit_daemon_status(self.as_ref()).await }
        async fn set_default_connected(&self, connected: bool) { <T as crate::events::ports::HostSidePort>::set_default_connected(self.as_ref(), connected).await }
        async fn drain_remote_coalesced(&self, host: &str, bot_id: &str) -> Result<usize> { <T as crate::events::ports::HostSidePort>::drain_remote_coalesced(self.as_ref(), host, bot_id).await }
    }
}

mod arc_port_15_providerport {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::ProviderPort + ?Sized> crate::events::ports::ProviderPort for std::sync::Arc<T> {
        async fn adopt_statusline_model(&self, run: &db::Run, payload: &Value) { <T as crate::events::ports::ProviderPort>::adopt_statusline_model(self.as_ref(), run, payload).await }
        async fn login_on_auth_failure(&self, bot: &db::Bot) { <T as crate::events::ports::ProviderPort>::login_on_auth_failure(self.as_ref(), bot).await }
        async fn login_on_turn_ok(&self, bot: &db::Bot) { <T as crate::events::ports::ProviderPort>::login_on_turn_ok(self.as_ref(), bot).await }
        fn codex_migration_on_blocked(&self, run: &db::Run) { <T as crate::events::ports::ProviderPort>::codex_migration_on_blocked(self.as_ref(), run) }
        fn prompt_suggestion_on_idle(&self, run: &db::Run) { <T as crate::events::ports::ProviderPort>::prompt_suggestion_on_idle(self.as_ref(), run) }
        async fn dismiss_survey_if_shown(&self, run: &db::Run) -> bool { <T as crate::events::ports::ProviderPort>::dismiss_survey_if_shown(self.as_ref(), run).await }
    }
}

mod arc_port_16_apiport {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::ApiPort + ?Sized> crate::events::ports::ApiPort for std::sync::Arc<T> {
        async fn state_json(&self) -> std::result::Result<Value, LcError> { <T as crate::events::ports::ApiPort>::state_json(self.as_ref()).await }
        fn ct_eq(&self, a: &str, b: &str) -> bool { <T as crate::events::ports::ApiPort>::ct_eq(self.as_ref(), a, b) }
    }
}

mod arc_port_17_reconcilecommands {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::ReconcileCommands + ?Sized> crate::events::ports::ReconcileCommands for std::sync::Arc<T> {
        async fn reconcile_host(&self, host: &str) -> Result<()> { <T as crate::events::ports::ReconcileCommands>::reconcile_host(self.as_ref(), host).await }
        async fn autostart_after_reconcile(&self, host: &str, reconciled: bool) -> bool { <T as crate::events::ports::ReconcileCommands>::autostart_after_reconcile(self.as_ref(), host, reconciled).await }
        fn schedule_deferred_pass(&self, host: &str) { <T as crate::events::ports::ReconcileCommands>::schedule_deferred_pass(self.as_ref(), host) }
        async fn sync_default_session(&self) -> Result<()> { <T as crate::events::ports::ReconcileCommands>::sync_default_session(self.as_ref()).await }
        fn session_paused_on_idle(&self, run: &db::Run) { <T as crate::events::ports::ReconcileCommands>::session_paused_on_idle(self.as_ref(), run) }
    }
}

mod arc_port_18_ingresscommands {
    use crate::events::ports::*;
    use crate::db;
    use crate::handoff::Footprint;
    use crate::herdr_maintenance::Window;
    use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
    use crate::lifecycle::fence::{EventIdentity, Ownership};
    use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
    use crate::quota::Quota;
    use anyhow::Result;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::future::Future;
    impl<T: crate::events::ports::IngressCommands + ?Sized> crate::events::ports::IngressCommands for std::sync::Arc<T> {
        async fn watch_pane_on_session(&self, host: &str, session: &str, pane_id: &str) { <T as crate::events::ports::IngressCommands>::watch_pane_on_session(self.as_ref(), host, session, pane_id).await }
        async fn unwatch_pane_on_session(&self, host: &str, session: &str, pane_id: &str) { <T as crate::events::ports::IngressCommands>::unwatch_pane_on_session(self.as_ref(), host, session, pane_id).await }
        fn retire_child<'a>(
            &'a self,
            bot_id: &'a str,
            why: &'static str,
            mode: RetireMode,
        ) -> impl Future<Output = Result<RetireOutcome>> + 'a { <T as crate::events::ports::IngressCommands>::retire_child(self.as_ref(), bot_id, why, mode) }
        async fn retirement_block(&self, bot_id: &str) -> Result<Option<String>> { <T as crate::events::ports::IngressCommands>::retirement_block(self.as_ref(), bot_id).await }
        async fn prune_stale_spawn_hints(&self) { <T as crate::events::ports::IngressCommands>::prune_stale_spawn_hints(self.as_ref()).await }
        async fn spawn_hints_for_host(&self, host: &str) -> Result<HashMap<String, String>> { <T as crate::events::ports::IngressCommands>::spawn_hints_for_host(self.as_ref(), host).await }
        async fn consume_spawn_hint(&self, host: &str, pane_id: &str) { <T as crate::events::ports::IngressCommands>::consume_spawn_hint(self.as_ref(), host, pane_id).await }
    }
}

mod arc_port_20_historysource {
    use crate::codex_history::*;
    use futures::future::BoxFuture;
    impl<T: crate::codex_history::HistorySource + ?Sized> crate::codex_history::HistorySource for std::sync::Arc<T> {
        fn supports(&self, host: &str) -> bool { <T as crate::codex_history::HistorySource>::supports(self.as_ref(), host) }
        fn open<'a>(&'a self, binding: &'a Binding, program: &'a str) -> BoxFuture<'a, Result<Box<dyn HistoryConn>, HistoryError>> { <T as crate::codex_history::HistorySource>::open(self.as_ref(), binding, program) }
    }
}

mod arc_port_21_codexhistoryhost {
    use crate::codex_history::*;
    use crate::db;
    use std::{path::PathBuf, sync::Arc};
    impl<T: crate::codex_history::CodexHistoryHost + ?Sized> crate::codex_history::CodexHistoryHost for std::sync::Arc<T> {
        fn enabled(&self) -> impl std::future::Future<Output = bool> + Send + '_ { <T as crate::codex_history::CodexHistoryHost>::enabled(self.as_ref()) }
        fn source(&self) -> Option<Arc<dyn HistorySource>> { <T as crate::codex_history::CodexHistoryHost>::source(self.as_ref()) }
        fn bot_host<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = Option<String>> + Send + 'a { <T as crate::codex_history::CodexHistoryHost>::bot_host(self.as_ref(), bot_id) }
        fn codex_home<'a>(&'a self, bot: &'a db::Bot) -> impl std::future::Future<Output = Option<PathBuf>> + Send + 'a { <T as crate::codex_history::CodexHistoryHost>::codex_home(self.as_ref(), bot) }
        fn codex_program(&self) -> impl std::future::Future<Output = String> + Send + '_ { <T as crate::codex_history::CodexHistoryHost>::codex_program(self.as_ref()) }
    }
}

mod arc_port_22_codexhistorystate {
    use crate::codex_history::*;
    impl<T: crate::codex_history::CodexHistoryState + ?Sized> crate::codex_history::CodexHistoryState for std::sync::Arc<T> {
        fn codex_history(&self) -> &crate::codex_history::HistoryHook { <T as crate::codex_history::CodexHistoryState>::codex_history(self.as_ref()) }
    }
}

mod arc_port_23_maintenanceport {
    use crate::lifecycle::send_now::ports::*;
    use crate::attach::Attachment;
    use crate::codex_history::Mark;
    use crate::db;
    use crate::lifecycle::{LcError, LcResult};
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::send_now::ports::MaintenancePort + ?Sized> crate::lifecycle::send_now::ports::MaintenancePort for std::sync::Arc<T> {
        const UNREADABLE_RETRY_SECS: i64 = <T as crate::lifecycle::send_now::ports::MaintenancePort>::UNREADABLE_RETRY_SECS;
        fn window_held(&self) -> impl std::future::Future<Output = std::result::Result<Option<WindowHeld>, WindowUnreadable>> + Send { <T as crate::lifecycle::send_now::ports::MaintenancePort>::window_held(self.as_ref()) }
    }
}

mod arc_port_24_idlesleepport {
    use crate::lifecycle::send_now::ports::*;
    use crate::attach::Attachment;
    use crate::codex_history::Mark;
    use crate::db;
    use crate::lifecycle::{LcError, LcResult};
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::send_now::ports::IdleSleepPort + ?Sized> crate::lifecycle::send_now::ports::IdleSleepPort for std::sync::Arc<T> {
        fn idle_sleep_wake<'a>(&'a self, bot_id: &'a str, why: &'a str) -> impl std::future::Future<Output = Result<bool>> + Send + 'a { <T as crate::lifecycle::send_now::ports::IdleSleepPort>::idle_sleep_wake(self.as_ref(), bot_id, why) }
        fn idle_sleep_wake_locked<'a>(&'a self, bot_id: &'a str, why: &'a str) -> impl std::future::Future<Output = Result<bool>> + Send + 'a { <T as crate::lifecycle::send_now::ports::IdleSleepPort>::idle_sleep_wake_locked(self.as_ref(), bot_id, why) }
    }
}

mod arc_port_25_supervisorsendrepo {
    use crate::lifecycle::send_now::ports::*;
    use crate::attach::Attachment;
    use crate::codex_history::Mark;
    use crate::db;
    use crate::lifecycle::{LcError, LcResult};
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::send_now::ports::SupervisorSendRepo + ?Sized> crate::lifecycle::send_now::ports::SupervisorSendRepo for std::sync::Arc<T> {
        fn assignment_by_turn<'a>(&'a self, turn_id: &'a str) -> impl std::future::Future<Output = Result<Option<AssignmentState>>> + Send + 'a { <T as crate::lifecycle::send_now::ports::SupervisorSendRepo>::assignment_by_turn(self.as_ref(), turn_id) }
        fn load_owned(&self) -> impl std::future::Future<Output = Result<crate::projection::Owned>> + Send { <T as crate::lifecycle::send_now::ports::SupervisorSendRepo>::load_owned(self.as_ref()) }
    }
}

mod arc_port_26_handoffsendrepo {
    use crate::lifecycle::send_now::ports::*;
    use crate::attach::Attachment;
    use crate::codex_history::Mark;
    use crate::db;
    use crate::lifecycle::{LcError, LcResult};
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::send_now::ports::HandoffSendRepo + ?Sized> crate::lifecycle::send_now::ports::HandoffSendRepo for std::sync::Arc<T> {
        fn bot_handed_off_to<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = Result<Option<String>>> + Send + 'a { <T as crate::lifecycle::send_now::ports::HandoffSendRepo>::bot_handed_off_to(self.as_ref(), bot_id) }
        fn refuse_handed_off<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = LcResult<()>> + Send + 'a { <T as crate::lifecycle::send_now::ports::HandoffSendRepo>::refuse_handed_off(self.as_ref(), bot_id) }
    }
}

mod arc_port_27_sharesendrepo {
    use crate::lifecycle::send_now::ports::*;
    use crate::attach::Attachment;
    use crate::codex_history::Mark;
    use crate::db;
    use crate::lifecycle::{LcError, LcResult};
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::send_now::ports::ShareSendRepo + ?Sized> crate::lifecycle::send_now::ports::ShareSendRepo for std::sync::Arc<T> {
        fn resolve_share_token<'a>(&'a self, token: &'a str) -> impl std::future::Future<Output = std::result::Result<Option<String>, sqlx::Error>> + Send + 'a { <T as crate::lifecycle::send_now::ports::ShareSendRepo>::resolve_share_token(self.as_ref(), token) }
        fn touch_share<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = ()> + Send + 'a { <T as crate::lifecycle::send_now::ports::ShareSendRepo>::touch_share(self.as_ref(), bot_id) }
        fn is_share_bot<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = std::result::Result<bool, sqlx::Error>> + Send + 'a { <T as crate::lifecycle::send_now::ports::ShareSendRepo>::is_share_bot(self.as_ref(), bot_id) }
    }
}

mod arc_port_28_attachsendport {
    use crate::lifecycle::send_now::ports::*;
    use crate::attach::Attachment;
    use crate::codex_history::Mark;
    use crate::db;
    use crate::lifecycle::{LcError, LcResult};
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::send_now::ports::AttachSendPort + ?Sized> crate::lifecycle::send_now::ports::AttachSendPort for std::sync::Arc<T> {
        async fn resolve_attachments(&self, bot_id: &str, ids: &[String]) -> Result<Vec<Attachment>> { <T as crate::lifecycle::send_now::ports::AttachSendPort>::resolve_attachments(self.as_ref(), bot_id, ids).await }
        async fn bind_attachments(&self, message_id: &str, items: &[Attachment]) -> Result<()> { <T as crate::lifecycle::send_now::ports::AttachSendPort>::bind_attachments(self.as_ref(), message_id, items).await }
    }
}

mod arc_port_31_panewatchport {
    use crate::lifecycle::send_now::ports::*;
    use crate::attach::Attachment;
    use crate::codex_history::Mark;
    use crate::db;
    use crate::lifecycle::{LcError, LcResult};
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::send_now::ports::PaneWatchPort + ?Sized> crate::lifecycle::send_now::ports::PaneWatchPort for std::sync::Arc<T> {
        fn unwatch_pane_on_session(
            &self,
            host: &str,
            session: &str,
            pane_id: &str,
        ) -> impl std::future::Future<Output = ()> + Send { <T as crate::lifecycle::send_now::ports::PaneWatchPort>::unwatch_pane_on_session(self.as_ref(), host, session, pane_id) }
    }
}

mod arc_port_32_codexsendport {
    use crate::lifecycle::send_now::ports::*;
    use crate::attach::Attachment;
    use crate::codex_history::Mark;
    use crate::db;
    use crate::lifecycle::{LcError, LcResult};
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::send_now::ports::CodexSendPort + ?Sized> crate::lifecycle::send_now::ports::CodexSendPort for std::sync::Arc<T> {
        async fn observe_codex_screen(&self, run: &db::Run, screen: &str) { <T as crate::lifecycle::send_now::ports::CodexSendPort>::observe_codex_screen(self.as_ref(), run, screen).await }
        async fn close_codex_picker(&self, client: &crate::herdr::HerdrClient, pane_id: &str) -> bool { <T as crate::lifecycle::send_now::ports::CodexSendPort>::close_codex_picker(self.as_ref(), client, pane_id).await }
        fn codex_running_version(&self, run_id: &str) -> Option<String> { <T as crate::lifecycle::send_now::ports::CodexSendPort>::codex_running_version(self.as_ref(), run_id) }
        fn codex_history_mark<'a>(&'a self, bot: &'a db::Bot, run: &'a db::Run) -> impl std::future::Future<Output = Option<Mark>> + Send + 'a { <T as crate::lifecycle::send_now::ports::CodexSendPort>::codex_history_mark(self.as_ref(), bot, run) }
        fn codex_prompt_landed<'a>(
            &'a self,
            mark: &'a Mark,
            conn: &'a mut Option<Box<dyn crate::codex_history::HistoryConn>>,
            text: &'a str,
        ) -> impl std::future::Future<Output = bool> + Send + 'a { <T as crate::lifecycle::send_now::ports::CodexSendPort>::codex_prompt_landed(self.as_ref(), mark, conn, text) }
    }
}

mod arc_port_33_sendenvport {
    use crate::lifecycle::send_now::ports::*;
    use crate::attach::Attachment;
    use crate::codex_history::Mark;
    use crate::db;
    use crate::lifecycle::{LcError, LcResult};
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::send_now::ports::SendEnvPort + ?Sized> crate::lifecycle::send_now::ports::SendEnvPort for std::sync::Arc<T> {
        async fn dangerous_rm_notify_once(&self, run: &db::Run, rm: &crate::tui_prompts::DangerousRm) -> bool { <T as crate::lifecycle::send_now::ports::SendEnvPort>::dangerous_rm_notify_once(self.as_ref(), run, rm).await }
        async fn note_keep_warm_prompt(&self, bot_id: &str, client_request_id: &str) { <T as crate::lifecycle::send_now::ports::SendEnvPort>::note_keep_warm_prompt(self.as_ref(), bot_id, client_request_id).await }
    }
}

mod arc_port_34_judgefuse {
    use crate::judge::*;
    use std::time::{Duration, Instant};
    use anyhow::{anyhow, Result};
    use serde_json::{json, Value};
    use sqlx::SqlitePool;
    use crate::config::JudgeCfg;
    impl<T: crate::judge::JudgeFuse + ?Sized> crate::judge::JudgeFuse for std::sync::Arc<T> {
        fn judge_fuse(&self) -> &tokio::sync::Mutex<()> { <T as crate::judge::JudgeFuse>::judge_fuse(self.as_ref()) }
    }
}

mod arc_port_35_surveyrevisions {
    use crate::tui_prompts::*;
    use crate::db;
    use crate::herdr::{HerdrClient, PaneRead};
    use am_core::{PaneReadSource, SessionId};
    use am_ports::RunPaneReader;
    use std::time::Duration;
    impl<T: crate::tui_prompts::SurveyRevisions + ?Sized> crate::tui_prompts::SurveyRevisions for std::sync::Arc<T> {
        fn survey_revisions(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>> { <T as crate::tui_prompts::SurveyRevisions>::survey_revisions(self.as_ref()) }
    }
}

mod arc_port_36_jobcounts {
    use crate::background_jobs::*;
    use crate::db;
    use serde_json::Value;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use std::time::Duration;
    impl<T: crate::background_jobs::JobCounts + ?Sized> crate::background_jobs::JobCounts for std::sync::Arc<T> {
        fn background_jobs(&self) -> &crate::background_jobs::Counts { <T as crate::background_jobs::JobCounts>::background_jobs(self.as_ref()) }
    }
}

mod arc_port_37_handoffsessionrepo {
    use crate::lifecycle::start::ports::*;
    use std::future::Future;
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::start::ports::HandoffSessionRepo + ?Sized> crate::lifecycle::start::ports::HandoffSessionRepo for std::sync::Arc<T> {
        fn bot_handed_off_to<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = Result<Option<String>>> + Send + 'a { <T as crate::lifecycle::start::ports::HandoffSessionRepo>::bot_handed_off_to(self.as_ref(), bot_id) }
        fn refuse_handed_off<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = crate::lifecycle::LcResult<()>> + Send + 'a { <T as crate::lifecycle::start::ports::HandoffSessionRepo>::refuse_handed_off(self.as_ref(), bot_id) }
    }
}

mod arc_port_38_sharesessionrepo {
    use crate::lifecycle::start::ports::*;
    use std::future::Future;
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::start::ports::ShareSessionRepo + ?Sized> crate::lifecycle::start::ports::ShareSessionRepo for std::sync::Arc<T> {
        fn restricted_workspace<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = std::result::Result<Option<String>, sqlx::Error>> + Send + 'a { <T as crate::lifecycle::start::ports::ShareSessionRepo>::restricted_workspace(self.as_ref(), bot_id) }
        fn caged_workspace<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = std::result::Result<Option<String>, sqlx::Error>> + Send + 'a { <T as crate::lifecycle::start::ports::ShareSessionRepo>::caged_workspace(self.as_ref(), bot_id) }
    }
}

mod arc_port_39_restartintentrepo {
    use crate::lifecycle::start::ports::*;
    use std::future::Future;
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::start::ports::RestartIntentRepo + ?Sized> crate::lifecycle::start::ports::RestartIntentRepo for std::sync::Arc<T> {
        async fn prepare_restart_intent(&self, subject_id: &str, host: &str, payload: &Value, ttl_secs: i64, boot: &str) -> Result<String> { <T as crate::lifecycle::start::ports::RestartIntentRepo>::prepare_restart_intent(self.as_ref(), subject_id, host, payload, ttl_secs, boot).await }
        async fn complete_intent(&self, id: &str) -> Result<bool> { <T as crate::lifecycle::start::ports::RestartIntentRepo>::complete_intent(self.as_ref(), id).await }
        async fn abandon_intent(&self, id: &str, why: &str) -> Result<bool> { <T as crate::lifecycle::start::ports::RestartIntentRepo>::abandon_intent(self.as_ref(), id, why).await }
        async fn fail_intent(&self, id: &str, err: &str) -> Result<bool> { <T as crate::lifecycle::start::ports::RestartIntentRepo>::fail_intent(self.as_ref(), id, err).await }
    }
}

mod arc_port_40_panewatchport {
    use crate::lifecycle::start::ports::*;
    use std::future::Future;
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::start::ports::PaneWatchPort + ?Sized> crate::lifecycle::start::ports::PaneWatchPort for std::sync::Arc<T> {
        async fn unwatch_pane_on_session(&self, host: &str, session: &str, pane_id: &str) { <T as crate::lifecycle::start::ports::PaneWatchPort>::unwatch_pane_on_session(self.as_ref(), host, session, pane_id).await }
    }
}

mod arc_port_41_remotecleanupport {
    use crate::lifecycle::start::ports::*;
    use std::future::Future;
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::start::ports::RemoteCleanupPort + ?Sized> crate::lifecycle::start::ports::RemoteCleanupPort for std::sync::Arc<T> {
        async fn record_remote_purge(&self, bot_id: &str, host: &str, ok: bool, error: Option<&str>) { <T as crate::lifecycle::start::ports::RemoteCleanupPort>::record_remote_purge(self.as_ref(), bot_id, host, ok, error).await }
        async fn move_remote_bot_dir_to_trash(&self, conn: &crate::hosts::HostConn, bot_id: &str) -> Result<Option<String>> { <T as crate::lifecycle::start::ports::RemoteCleanupPort>::move_remote_bot_dir_to_trash(self.as_ref(), conn, bot_id).await }
    }
}

mod arc_port_42_previewport {
    use crate::lifecycle::start::ports::*;
    use std::future::Future;
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::start::ports::PreviewPort + ?Sized> crate::lifecycle::start::ports::PreviewPort for std::sync::Arc<T> {
        async fn stop_preview_for_bot(&self, bot_id: &str) -> bool { <T as crate::lifecycle::start::ports::PreviewPort>::stop_preview_for_bot(self.as_ref(), bot_id).await }
    }
}

mod arc_port_43_sessionproviderport {
    use crate::lifecycle::start::ports::*;
    use std::future::Future;
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::start::ports::SessionProviderPort + ?Sized> crate::lifecycle::start::ports::SessionProviderPort for std::sync::Arc<T> {
        fn claude_live_start_fresh(&self, run_id: &str) { <T as crate::lifecycle::start::ports::SessionProviderPort>::claude_live_start_fresh(self.as_ref(), run_id) }
        fn models_list<'a>(&'a self, host: &'a str, kind: &'a str, identity: Option<&'a str>, refresh: bool) -> impl std::future::Future<Output = Result<Value>> + Send + 'a { <T as crate::lifecycle::start::ports::SessionProviderPort>::models_list(self.as_ref(), host, kind, identity, refresh) }
    }
}

mod arc_port_44_shiminstallport {
    use crate::lifecycle::start::ports::*;
    use std::future::Future;
    use am_ports::{EventSink, TurnEvents};
    use anyhow::Result;
    use serde_json::Value;
    impl<T: crate::lifecycle::start::ports::ShimInstallPort + ?Sized> crate::lifecycle::start::ports::ShimInstallPort for std::sync::Arc<T> {
        fn install_local_herdr_shim(&self, bot_dir: &std::path::Path) -> std::io::Result<std::path::PathBuf> { <T as crate::lifecycle::start::ports::ShimInstallPort>::install_local_herdr_shim(self.as_ref(), bot_dir) }
        fn install_local_cargo_shim(&self, bot_dir: &std::path::Path) -> std::io::Result<std::path::PathBuf> { <T as crate::lifecycle::start::ports::ShimInstallPort>::install_local_cargo_shim(self.as_ref(), bot_dir) }
        fn install_remote_herdr_shim<'a>(
            &'a self,
            conn: &'a crate::hosts::HostConn,
            remote_bot_dir: &'a str,
        ) -> impl std::future::Future<Output = Result<String>> + Send + 'a { <T as crate::lifecycle::start::ports::ShimInstallPort>::install_remote_herdr_shim(self.as_ref(), conn, remote_bot_dir) }
        fn install_remote_cargo_shim<'a>(
            &'a self,
            conn: &'a crate::hosts::HostConn,
            remote_bot_dir: &'a str,
        ) -> impl std::future::Future<Output = Result<String>> + Send + 'a { <T as crate::lifecycle::start::ports::ShimInstallPort>::install_remote_cargo_shim(self.as_ref(), conn, remote_bot_dir) }
    }
}

mod arc_port_45_progresspollers {
    use crate::lifecycle::poller::*;
    impl<T: crate::lifecycle::poller::ProgressPollers + ?Sized> crate::lifecycle::poller::ProgressPollers for std::sync::Arc<T> {
        fn progress_pollers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>> { <T as crate::lifecycle::poller::ProgressPollers>::progress_pollers(self.as_ref()) }
    }
}

mod arc_port_46_stalltimers {
    use crate::lifecycle::poller::*;
    impl<T: crate::lifecycle::poller::StallTimers + ?Sized> crate::lifecycle::poller::StallTimers for std::sync::Arc<T> {
        fn stall_timers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>> { <T as crate::lifecycle::poller::StallTimers>::stall_timers(self.as_ref()) }
    }
}

mod arc_port_47_progressemitted {
    use crate::lifecycle::poller::*;
    impl<T: crate::lifecycle::poller::ProgressEmitted + ?Sized> crate::lifecycle::poller::ProgressEmitted for std::sync::Arc<T> {
        fn progress_emitted(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> { <T as crate::lifecycle::poller::ProgressEmitted>::progress_emitted(self.as_ref()) }
    }
}

mod arc_port_48_fallbacktimers {
    use crate::lifecycle::poller::*;
    impl<T: crate::lifecycle::poller::FallbackTimers + ?Sized> crate::lifecycle::poller::FallbackTimers for std::sync::Arc<T> {
        fn fallback_timers(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, u64>> { <T as crate::lifecycle::poller::FallbackTimers>::fallback_timers(self.as_ref()) }
    }
}

mod arc_port_49_deadpanesservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::DeadPanesServices + ?Sized> crate::lifecycle::s6_ports::DeadPanesServices for std::sync::Arc<T> {
        fn maintenance_window_active(&self) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::DeadPanesServices>::maintenance_window_active(self.as_ref()) }
    }
}

mod arc_port_50_transcriptoriginservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::TranscriptOriginServices + ?Sized> crate::lifecycle::s6_ports::TranscriptOriginServices for std::sync::Arc<T> {
        fn started_by_the_cli_itself(
            &self,
            bot: &db::Bot,
            transcript_path: Option<&str>,
        ) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::TranscriptOriginServices>::started_by_the_cli_itself(self.as_ref(), bot, transcript_path) }
    }
}

mod arc_port_51_stuckturnservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::StuckTurnServices + ?Sized> crate::lifecycle::s6_ports::StuckTurnServices for std::sync::Arc<T> {
        fn retain_supervisor_runs(&self, active: &[String]) { <T as crate::lifecycle::s6_ports::StuckTurnServices>::retain_supervisor_runs(self.as_ref(), active) }
        fn sweep_child_alerts(&self) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StuckTurnServices>::sweep_child_alerts(self.as_ref()) }
        fn sweep_child_done(&self) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StuckTurnServices>::sweep_child_done(self.as_ref()) }
        fn local_transcript_allowed(&self, bot: &db::Bot, raw_path: &str) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::StuckTurnServices>::local_transcript_allowed(self.as_ref(), bot, raw_path) }
        fn codex_exact_reply(&self, bot: &db::Bot, run: &db::Run, sent: &[String]) -> impl Future<Output = Option<String>> + Send { <T as crate::lifecycle::s6_ports::StuckTurnServices>::codex_exact_reply(self.as_ref(), bot, run, sent) }
        fn codex_home(&self, bot: &db::Bot) -> impl Future<Output = Option<PathBuf>> + Send { <T as crate::lifecycle::s6_ports::StuckTurnServices>::codex_home(self.as_ref(), bot) }
        fn emit_turn(&self, turn_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StuckTurnServices>::emit_turn(self.as_ref(), turn_id) }
    }
}

mod arc_port_52_setupshareservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::SetupShareServices + ?Sized> crate::lifecycle::s6_ports::SetupShareServices for std::sync::Arc<T> {
        fn cage_settings(&self, settings: &mut Value, workspace: &str, env: &Value) { <T as crate::lifecycle::s6_ports::SetupShareServices>::cage_settings(self.as_ref(), settings, workspace, env) }
    }
}

mod arc_port_53_startservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::StartServices + ?Sized> crate::lifecycle::s6_ports::StartServices for std::sync::Arc<T> {
        fn lock_bot_for_start<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = Box<dyn am_ports::BotLockGuard + 'static>> + Send + 'a { <T as crate::lifecycle::s6_ports::StartServices>::lock_bot_for_start(self.as_ref(), bot_id) }
        fn session_for_bot(&self, bot: &db::Bot, host: &str) -> impl Future<Output = Option<String>> + Send { <T as crate::lifecycle::s6_ports::StartServices>::session_for_bot(self.as_ref(), bot, host) }
        fn prepare_restricted_bot(
            &self,
            bot: &db::Bot,
            host: &str,
        ) -> impl Future<Output = Result<Option<String>, LcError>> + Send { <T as crate::lifecycle::s6_ports::StartServices>::prepare_restricted_bot(self.as_ref(), bot, host) }
        fn cage_environment<'a>(
            &'a self,
            env: &'a mut Value,
            bot: &'a db::Bot,
        ) -> impl Future<Output = ()> + Send + 'a { <T as crate::lifecycle::s6_ports::StartServices>::cage_environment(self.as_ref(), env, bot) }
        fn install_restricted_prompt(
            &self,
            bot: &db::Bot,
            workspace: &str,
            env: &Value,
        ) -> anyhow::Result<PathBuf> { <T as crate::lifecycle::s6_ports::StartServices>::install_restricted_prompt(self.as_ref(), bot, workspace, env) }
        fn restricted_launch_args(&self, env: &Value, prompt: &Path) -> Vec<String> { <T as crate::lifecycle::s6_ports::StartServices>::restricted_launch_args(self.as_ref(), env, prompt) }
        fn pane_env(
            &self,
            bot: &db::Bot,
            host: &str,
            run_id: &str,
            agent_name: &str,
            shim_dir: Option<&str>,
            fence: Option<&crate::hosts::HostFence>,
        ) -> impl Future<Output = anyhow::Result<Value>> + Send { <T as crate::lifecycle::s6_ports::StartServices>::pane_env(self.as_ref(), bot, host, run_id, agent_name, shim_dir, fence) }
        fn pretrust_for_start(&self, bot: &db::Bot, host: &str, cwd: &str) -> impl Future<Output = Vec<String>> + Send { <T as crate::lifecycle::s6_ports::StartServices>::pretrust_for_start(self.as_ref(), bot, host, cwd) }
        fn apply_grok_startup_effort(
            &self,
            bot: &db::Bot,
            run_id: &str,
            pane_id: &str,
            client: &crate::herdr::HerdrClient,
        ) -> impl Future<Output = std::result::Result<(), String>> + Send { <T as crate::lifecycle::s6_ports::StartServices>::apply_grok_startup_effort(self.as_ref(), bot, run_id, pane_id, client) }
        fn flush_once_running(&self, bot_id: &str, run_id: &str) { <T as crate::lifecycle::s6_ports::StartServices>::flush_once_running(self.as_ref(), bot_id, run_id) }
        #[cfg(test)]
        fn spawn_adopted_capture(&self, run_id: &str, bot_id: &str) { <T as crate::lifecycle::s6_ports::StartServices>::spawn_adopted_capture(self.as_ref(), run_id, bot_id) }
        fn watch_pane_on_session(&self, host: &str, session: &str, pane: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StartServices>::watch_pane_on_session(self.as_ref(), host, session, pane) }
        fn session_connected_with_host_fence(&self, fence: &crate::hosts::HostFence, session: &str) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::StartServices>::session_connected_with_host_fence(self.as_ref(), fence, session) }
        fn emit_lifecycle_event(&self, kind: &str, bot: Option<&str>, payload: Value) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StartServices>::emit_lifecycle_event(self.as_ref(), kind, bot, payload) }
    }
}

mod arc_port_54_identityaccess {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::IdentityAccess + ?Sized> crate::lifecycle::s6_ports::IdentityAccess for std::sync::Arc<T> {
        fn identity_for_host(
            &self,
            host: &str,
            name: &str,
        ) -> impl Future<Output = Option<crate::config::IdentityCfg>> + Send { <T as crate::lifecycle::s6_ports::IdentityAccess>::identity_for_host(self.as_ref(), host, name) }
        fn cached_identity_logged_out(&self, host: &str, name: &str) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::IdentityAccess>::cached_identity_logged_out(self.as_ref(), host, name) }
        fn recheck_identity_login(&self, host: &str, name: &str) -> impl Future<Output = Option<bool>> + Send { <T as crate::lifecycle::s6_ports::IdentityAccess>::recheck_identity_login(self.as_ref(), host, name) }
    }
}

mod arc_port_55_turnerrorservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::TurnErrorServices + ?Sized> crate::lifecycle::s6_ports::TurnErrorServices for std::sync::Arc<T> {
        fn next_reset_for_bot(&self, bot: &db::Bot) -> impl Future<Output = Option<String>> + Send { <T as crate::lifecycle::s6_ports::TurnErrorServices>::next_reset_for_bot(self.as_ref(), bot) }
        fn read_run_pane_recent_unwrapped(
            &self,
            run: &db::Run,
            pane: &str,
            lines: usize,
        ) -> impl Future<Output = anyhow::Result<Option<String>>> + Send { <T as crate::lifecycle::s6_ports::TurnErrorServices>::read_run_pane_recent_unwrapped(self.as_ref(), run, pane, lines) }
        fn host_utc_offset_secs(&self, host: &str) -> impl Future<Output = Option<i32>> + Send { <T as crate::lifecycle::s6_ports::TurnErrorServices>::host_utc_offset_secs(self.as_ref(), host) }
        fn turn_changed(&self, turn_id: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::TurnErrorServices>::turn_changed(self.as_ref(), turn_id) }
    }
}

mod arc_port_56_turnerrorquotaaccess {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::TurnErrorQuotaAccess + ?Sized> crate::lifecycle::s6_ports::TurnErrorQuotaAccess for std::sync::Arc<T> {
        fn resolve_key<'a>(
            &'a self,
            host: &'a str,
            provider: &'a str,
            identity: Option<&'a str>,
        ) -> impl Future<Output = Result<am_core::QuotaKey, am_core::PortError>> + Send + 'a { <T as crate::lifecycle::s6_ports::TurnErrorQuotaAccess>::resolve_key(self.as_ref(), host, provider, identity) }
        fn snapshot<'a>(
            &'a self,
            key: &'a am_core::QuotaKey,
        ) -> impl Future<Output = Result<Option<am_core::QuotaSnapshot>, am_core::PortError>> + Send + 'a { <T as crate::lifecycle::s6_ports::TurnErrorQuotaAccess>::snapshot(self.as_ref(), key) }
        fn store_snapshot<'a>(
            &'a self,
            key: &'a am_core::QuotaKey,
            snapshot: am_core::QuotaSnapshot,
        ) -> impl Future<Output = Result<(), am_core::PortError>> + Send + 'a { <T as crate::lifecycle::s6_ports::TurnErrorQuotaAccess>::store_snapshot(self.as_ref(), key, snapshot) }
    }
}

mod arc_port_57_runstateservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::RunStateServices + ?Sized> crate::lifecycle::s6_ports::RunStateServices for std::sync::Arc<T> {
        fn reconcile_host(&self, host: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::RunStateServices>::reconcile_host(self.as_ref(), host) }
        fn finish_stop(&self, run_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::RunStateServices>::finish_stop(self.as_ref(), run_id) }
    }
}

mod arc_port_58_liveapplydebtcontext {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::LiveApplyDebtContext + ?Sized> crate::lifecycle::s6_ports::LiveApplyDebtContext for std::sync::Arc<T> {
        fn stamp_live_revision(&self, pool: &sqlx::SqlitePool, run_id: &str) -> impl Future<Output = Result<bool, sqlx::Error>> + Send { <T as crate::lifecycle::s6_ports::LiveApplyDebtContext>::stamp_live_revision(self.as_ref(), pool, run_id) }
    }
}

mod arc_port_59_deferredlivecontext {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::DeferredLiveContext + ?Sized> crate::lifecycle::s6_ports::DeferredLiveContext for std::sync::Arc<T> {
        fn apply_live_setting_with_revision(
            &self,
            bot_id: &str,
            fields: &[&'static str],
            baseline_rev: &str,
            target_rev: &str,
        ) -> impl Future<Output = crate::lifecycle::LiveApplyOutcome> + Send { <T as crate::lifecycle::s6_ports::DeferredLiveContext>::apply_live_setting_with_revision(self.as_ref(), bot_id, fields, baseline_rev, target_rev) }
    }
}

mod arc_port_60_groktranscriptservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::GrokTranscriptServices + ?Sized> crate::lifecycle::s6_ports::GrokTranscriptServices for std::sync::Arc<T> {
        fn host_shell(&self, host: &str, script: &str) -> impl Future<Output = anyhow::Result<String>> + Send { <T as crate::lifecycle::s6_ports::GrokTranscriptServices>::host_shell(self.as_ref(), host, script) }
        fn grok_home_for(&self, bot: &db::Bot, host: &str) -> impl Future<Output = anyhow::Result<String>> + Send { <T as crate::lifecycle::s6_ports::GrokTranscriptServices>::grok_home_for(self.as_ref(), bot, host) }
        fn pids_in_pane(&self, host: &str, pane: &str, session: Option<&str>) -> impl Future<Output = Vec<i32>> + Send { <T as crate::lifecycle::s6_ports::GrokTranscriptServices>::pids_in_pane(self.as_ref(), host, pane, session) }
        fn agy_session_load(&self, run: &db::Run, host: &str) -> impl Future<Output = anyhow::Result<Option<(String, String)>>> + Send { <T as crate::lifecycle::s6_ports::GrokTranscriptServices>::agy_session_load(self.as_ref(), run, host) }
        fn consume_resume_session(&self, bot: &db::Bot, run: &db::Run, session: Option<&str>) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::GrokTranscriptServices>::consume_resume_session(self.as_ref(), bot, run, session) }
        fn agy_session_record_status(&self, bot: &db::Bot, run: &db::Run, transcript: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::GrokTranscriptServices>::agy_session_record_status(self.as_ref(), bot, run, transcript) }
        fn emit_turn(&self, turn_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::GrokTranscriptServices>::emit_turn(self.as_ref(), turn_id) }
        fn claude_herdr_session(&self, run: &db::Run) -> impl Future<Output = Option<String>> + Send { <T as crate::lifecycle::s6_ports::GrokTranscriptServices>::claude_herdr_session(self.as_ref(), run) }
        fn claude_config_roots(&self, bot: &db::Bot, run: &db::Run, host: &str) -> impl Future<Output = Vec<String>> + Send { <T as crate::lifecycle::s6_ports::GrokTranscriptServices>::claude_config_roots(self.as_ref(), bot, run, host) }
    }
}

mod arc_port_61_quotaholdservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::QuotaHoldServices + ?Sized> crate::lifecycle::s6_ports::QuotaHoldServices for std::sync::Arc<T> {
        fn try_limit_hit_for_bot(&self, bot: &db::Bot) -> impl Future<Output = anyhow::Result<Option<crate::quota::LimitHit>>> + Send { <T as crate::lifecycle::s6_ports::QuotaHoldServices>::try_limit_hit_for_bot(self.as_ref(), bot) }
        fn running_model(&self, bot: &db::Bot) -> impl Future<Output = Option<String>> + Send { <T as crate::lifecycle::s6_ports::QuotaHoldServices>::running_model(self.as_ref(), bot) }
        fn billing_identity(&self, bot: &db::Bot) -> impl Future<Output = anyhow::Result<Option<String>>> + Send { <T as crate::lifecycle::s6_ports::QuotaHoldServices>::billing_identity(self.as_ref(), bot) }
        fn limit_cleared_since(&self, bot: &db::Bot, since: chrono::DateTime<chrono::Utc>) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::QuotaHoldServices>::limit_cleared_since(self.as_ref(), bot, since) }
        fn host_target(&self, host: &str) -> impl Future<Output = Option<String>> + Send { <T as crate::lifecycle::s6_ports::QuotaHoldServices>::host_target(self.as_ref(), host) }
        fn restore_limit_hit(&self, host: &str, base: &str, hit: crate::quota::LimitHit) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::QuotaHoldServices>::restore_limit_hit(self.as_ref(), host, base, hit) }
        fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> impl Future<Output = String> + Send { <T as crate::lifecycle::s6_ports::QuotaHoldServices>::quota_base_for_host(self.as_ref(), host, kind, identity) }
        fn schedule_queue_flush(&self, bot_id: &str) { <T as crate::lifecycle::s6_ports::QuotaHoldServices>::schedule_queue_flush(self.as_ref(), bot_id) }
        fn schedule_flush_retry(&self, bot_id: &str, delay: std::time::Duration) { <T as crate::lifecycle::s6_ports::QuotaHoldServices>::schedule_flush_retry(self.as_ref(), bot_id, delay) }
    }
}

mod arc_port_62_busysendservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::BusySendServices + ?Sized> crate::lifecycle::s6_ports::BusySendServices for std::sync::Arc<T> {
        fn prompt_message_added(&self, bot_id: &str, message_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::BusySendServices>::prompt_message_added(self.as_ref(), bot_id, message_id) }
        fn turn_changed(&self, turn_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::BusySendServices>::turn_changed(self.as_ref(), turn_id) }
    }
}

mod arc_port_63_codexsteerservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::CodexSteerServices + ?Sized> crate::lifecycle::s6_ports::CodexSteerServices for std::sync::Arc<T> {
        fn plan_delivery(
            &self,
            client: &crate::lifecycle::RunClient,
            run: &db::Run,
            bot: &db::Bot,
            text: &str,
            force_pane: bool,
            waited_for_log: bool,
        ) -> impl Future<Output = anyhow::Result<Result<crate::lifecycle::delivery::Plan, crate::lifecycle::delivery::Delivered>>> + Send { <T as crate::lifecycle::s6_ports::CodexSteerServices>::plan_delivery(self.as_ref(), client, run, bot, text, force_pane, waited_for_log) }
        fn execute_delivery(
            &self,
            client: &crate::lifecycle::RunClient,
            run: &db::Run,
            bot: &db::Bot,
            text: &str,
            plan: crate::lifecycle::delivery::Plan,
        ) -> impl Future<Output = anyhow::Result<crate::lifecycle::delivery::Delivered>> + Send { <T as crate::lifecycle::s6_ports::CodexSteerServices>::execute_delivery(self.as_ref(), client, run, bot, text, plan) }
    }
}

mod arc_port_64_sendnowservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::SendNowServices + ?Sized> crate::lifecycle::s6_ports::SendNowServices for std::sync::Arc<T> {
        fn prepare_delivery(
            &self,
            client: &crate::lifecycle::RunClient,
            run: &db::Run,
            bot: &db::Bot,
            text: &str,
            plan: crate::lifecycle::delivery::Plan,
        ) -> impl Future<Output = Result<crate::lifecycle::delivery::Ready, crate::lifecycle::delivery::Delivered>> + Send { <T as crate::lifecycle::s6_ports::SendNowServices>::prepare_delivery(self.as_ref(), client, run, bot, text, plan) }
        fn type_delivery_text(
            &self,
            client: &crate::lifecycle::RunClient,
            run: &db::Run,
            bot: &db::Bot,
            text: &str,
            ready: crate::lifecycle::delivery::Ready,
        ) -> impl Future<Output = anyhow::Result<crate::lifecycle::delivery::Typing>> + Send { <T as crate::lifecycle::s6_ports::SendNowServices>::type_delivery_text(self.as_ref(), client, run, bot, text, ready) }
        fn send_now_landed(
            &self,
            client: &crate::herdr::HerdrClient,
            run: &db::Run,
            bot: &db::Bot,
            text: &str,
            typed: &crate::lifecycle::delivery::Typed,
        ) -> impl Future<Output = crate::lifecycle::delivery::Landed> + Send { <T as crate::lifecycle::s6_ports::SendNowServices>::send_now_landed(self.as_ref(), client, run, bot, text, typed) }
        fn confirm_send_now(
            &self,
            client: &crate::herdr::HerdrClient,
            run: &db::Run,
            bot: &db::Bot,
            text: &str,
            typed: &crate::lifecycle::delivery::Typed,
        ) -> impl Future<Output = anyhow::Result<crate::lifecycle::delivery::Delivered>> + Send { <T as crate::lifecycle::s6_ports::SendNowServices>::confirm_send_now(self.as_ref(), client, run, bot, text, typed) }
    }
}

mod arc_port_65_suggestionservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::SuggestionServices + ?Sized> crate::lifecycle::s6_ports::SuggestionServices for std::sync::Arc<T> {
        fn submit_gates(
            &self,
            bot_id: &str,
            bot: &db::Bot,
            conversation_id: &str,
        ) -> impl Future<Output = Result<db::Run, LcError>> + Send { <T as crate::lifecycle::s6_ports::SuggestionServices>::submit_gates(self.as_ref(), bot_id, bot, conversation_id) }
        fn client_for_run(&self, run: &db::Run) -> impl Future<Output = Result<crate::lifecycle::RunClient, LcError>> + Send { <T as crate::lifecycle::s6_ports::SuggestionServices>::client_for_run(self.as_ref(), run) }
        fn submit_locked(
            &self,
            bot_id: &str,
            token: &str,
            client_request_id: &str,
        ) -> impl Future<Output = Result<crate::lifecycle::PromptOut, crate::lifecycle::composer_draft::SubmitDraftError>> + Send { <T as crate::lifecycle::s6_ports::SuggestionServices>::submit_locked(self.as_ref(), bot_id, token, client_request_id) }
    }
}

mod arc_port_66_resumenudgeservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::ResumeNudgeServices + ?Sized> crate::lifecycle::s6_ports::ResumeNudgeServices for std::sync::Arc<T> {
        fn identity_config_dir(
            &self,
            host: &str,
            identity: Option<&str>,
        ) -> impl Future<Output = anyhow::Result<String>> + Send { <T as crate::lifecycle::s6_ports::ResumeNudgeServices>::identity_config_dir(self.as_ref(), host, identity) }
        fn queue_nudge(
            &self,
            conversation_id: &str,
            bot_id: &str,
            text: &str,
            client_request_id: &str,
            relay_from: &str,
        ) -> impl Future<Output = anyhow::Result<crate::lifecycle::PromptOut>> + Send { <T as crate::lifecycle::s6_ports::ResumeNudgeServices>::queue_nudge(self.as_ref(), conversation_id, bot_id, text, client_request_id, relay_from) }
        fn schedule_queue_flush(&self, bot_id: &str) { <T as crate::lifecycle::s6_ports::ResumeNudgeServices>::schedule_queue_flush(self.as_ref(), bot_id) }
    }
}

mod arc_port_67_screenservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::ScreenServices + ?Sized> crate::lifecycle::s6_ports::ScreenServices for std::sync::Arc<T> {
        fn read_run_pane_recent_unwrapped(
            &self,
            run: &db::Run,
            pane: &str,
            lines: usize,
        ) -> impl Future<Output = anyhow::Result<Option<(String, u64)>>> + Send { <T as crate::lifecycle::s6_ports::ScreenServices>::read_run_pane_recent_unwrapped(self.as_ref(), run, pane, lines) }
        fn insert_system_notice(&self, conversation_id: &str, notice: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::ScreenServices>::insert_system_notice(self.as_ref(), conversation_id, notice) }
        fn post_codex_security_banner_notice(&self, run: &db::Run, notice: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::ScreenServices>::post_codex_security_banner_notice(self.as_ref(), run, notice) }
        fn push_codex_security_banner_alert(&self, run: &db::Run, reason: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::ScreenServices>::push_codex_security_banner_alert(self.as_ref(), run, reason) }
        fn shadow_limit_hit(&self, sample: crate::judge::Sample) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::ScreenServices>::shadow_limit_hit(self.as_ref(), sample) }
        fn mark_codex_limit_hit(&self, bot: &db::Bot, notice: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::ScreenServices>::mark_codex_limit_hit(self.as_ref(), bot, notice) }
        fn fail_in_flight_turn(&self, turn_id: &str, note: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::ScreenServices>::fail_in_flight_turn(self.as_ref(), turn_id, note) }
        fn apply_codex_limit_hit_quota(
            &self,
            host: &str,
            base: &str,
            hit: crate::quota::LimitHit,
        ) -> impl Future<Output = crate::quota::LimitHit> + Send { <T as crate::lifecycle::s6_ports::ScreenServices>::apply_codex_limit_hit_quota(self.as_ref(), host, base, hit) }
    }
}

mod arc_port_68_stopservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::StopServices + ?Sized> crate::lifecycle::s6_ports::StopServices for std::sync::Arc<T> {
        fn lock_bot_for_stop<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = Box<dyn am_ports::BotLockGuard + 'static>> + Send + 'a { <T as crate::lifecycle::s6_ports::StopServices>::lock_bot_for_stop(self.as_ref(), bot_id) }
        fn notify_bot_status(&self, bot_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StopServices>::notify_bot_status(self.as_ref(), bot_id) }
        fn publish_lifecycle_event(&self, kind: &str, payload: Value) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StopServices>::publish_lifecycle_event(self.as_ref(), kind, payload) }
        fn refresh_background_jobs(&self, run: &db::Run, kind: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StopServices>::refresh_background_jobs(self.as_ref(), run, kind) }
        fn background_jobs_count(&self, run_id: &str) -> Option<u32> { <T as crate::lifecycle::s6_ports::StopServices>::background_jobs_count(self.as_ref(), run_id) }
        fn fail_in_flight(&self, run_id: &str, note: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::StopServices>::fail_in_flight(self.as_ref(), run_id, note) }
        fn note_user_interrupt_of(
            &self,
            bot: &db::Bot,
            run: &db::Run,
            in_flight: Option<&db::Turn>,
        ) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StopServices>::note_user_interrupt_of(self.as_ref(), bot, run, in_flight) }
        fn announce_revoked(&self, turn_id: &str, revoked: crate::lifecycle::Revoked) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::StopServices>::announce_revoked(self.as_ref(), turn_id, revoked) }
        fn revoke_orphaned_queued_turns(&self, bot_id: &str, why: &str) -> impl Future<Output = Vec<String>> + Send { <T as crate::lifecycle::s6_ports::StopServices>::revoke_orphaned_queued_turns(self.as_ref(), bot_id, why) }
        fn stop_preview(&self, bot_id: &str, fence: Option<&crate::hosts::HostFence>) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::StopServices>::stop_preview(self.as_ref(), bot_id, fence) }
        fn validate_workspace_path(&self, data_dir: &Path, workspace: &str) -> impl Future<Output = Option<PathBuf>> + Send { <T as crate::lifecycle::s6_ports::StopServices>::validate_workspace_path(self.as_ref(), data_dir, workspace) }
        fn is_shared_host(&self, host: &str) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::StopServices>::is_shared_host(self.as_ref(), host) }
    }
}

mod arc_port_69_queueservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::QueueServices + ?Sized> crate::lifecycle::s6_ports::QueueServices for std::sync::Arc<T> {
        fn turn_changed(&self, turn_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::turn_changed(self.as_ref(), turn_id) }
        fn bot_status_changed(&self, bot_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::bot_status_changed(self.as_ref(), bot_id) }
        fn announce_revoked(&self, turn_id: &str, revoked: crate::lifecycle::queue::Revoked) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::announce_revoked(self.as_ref(), turn_id, revoked) }
        fn note_run_gone(&self, bot_id: &str, why: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::note_run_gone(self.as_ref(), bot_id, why) }
        fn resume_gate(
            &self,
            bot: &db::Bot,
            run: &db::Run,
            conversation_id: &str,
        ) -> impl Future<Output = crate::lifecycle::resume_gate::Gate> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::resume_gate(self.as_ref(), bot, run, conversation_id) }
        fn interrupt_grace_left(
            &self,
            bot: &db::Bot,
            run: &db::Run,
            conversation_id: &str,
        ) -> impl Future<Output = Option<std::time::Duration>> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::interrupt_grace_left(self.as_ref(), bot, run, conversation_id) }
        fn pane_ready_for_prompt(&self, bot: &db::Bot, run: &db::Run, conversation_id: &str) -> impl Future<Output = Result<(), crate::lifecycle::LcError>> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::pane_ready_for_prompt(self.as_ref(), bot, run, conversation_id) }
        fn deliver_queued_prompt(
            &self,
            client: &crate::lifecycle::RunClient,
            run: &db::Run,
            bot: &db::Bot,
            text: &str,
            waited_for_log: bool,
        ) -> impl Future<Output = anyhow::Result<crate::lifecycle::delivery::Delivered>> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::deliver_queued_prompt(self.as_ref(), client, run, bot, text, waited_for_log) }
        fn fail_in_flight(&self, run_id: &str, note: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::fail_in_flight(self.as_ref(), run_id, note) }
        fn fail_in_flight_or_owe(&self, run_id: &str, note: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::fail_in_flight_or_owe(self.as_ref(), run_id, note) }
        fn close_owed(&self, bot_id: &str, turn_id: &str, delivery: &'static str, note: &str) -> impl Future<Output = anyhow::Result<bool>> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::close_owed(self.as_ref(), bot_id, turn_id, delivery, note) }
        fn put_back_owed(&self, bot_id: &str, conversation_id: &str, turn_id: &str, reason: &str, wait_key: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::put_back_owed(self.as_ref(), bot_id, conversation_id, turn_id, reason, wait_key) }
        fn delivered_owed(&self, bot_id: &str, turn_id: &str, record: crate::lifecycle::DeliveryRecord) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::delivered_owed(self.as_ref(), bot_id, turn_id, record) }
        fn arm_stall_and_progress(&self, run_id: &str, bot_id: &str, turn_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::QueueServices>::arm_stall_and_progress(self.as_ref(), run_id, bot_id, turn_id) }
        fn schedule_reconcile_settle(&self, run_id: &str, stuck: &str) { <T as crate::lifecycle::s6_ports::QueueServices>::schedule_reconcile_settle(self.as_ref(), run_id, stuck) }
    }
}

mod arc_port_70_oweddeliveryservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::OwedDeliveryServices + ?Sized> crate::lifecycle::s6_ports::OwedDeliveryServices for std::sync::Arc<T> {
        fn mark_delivery(&self, turn_id: &str, record: crate::lifecycle::DeliveryRecord, delivered_at: &str) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::OwedDeliveryServices>::mark_delivery(self.as_ref(), turn_id, record, delivered_at) }
        fn queue_put_back(
            &self,
            bot_id: &str,
            conversation_id: &str,
            turn_id: &str,
            reason: &str,
            wait_key: &str,
        ) -> impl Future<Output = anyhow::Result<()>> + Send { <T as crate::lifecycle::s6_ports::OwedDeliveryServices>::queue_put_back(self.as_ref(), bot_id, conversation_id, turn_id, reason, wait_key) }
        fn message_added(&self, bot_id: &str, message: db::Message) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::OwedDeliveryServices>::message_added(self.as_ref(), bot_id, message) }
        fn turn_changed(&self, turn_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::OwedDeliveryServices>::turn_changed(self.as_ref(), turn_id) }
    }
}

mod arc_port_71_interruptionservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::InterruptionServices + ?Sized> crate::lifecycle::s6_ports::InterruptionServices for std::sync::Arc<T> {
        fn log_interrupted_since(&self, bot: &db::Bot, run: &db::Run, since: chrono::DateTime<chrono::Utc>) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::InterruptionServices>::log_interrupted_since(self.as_ref(), bot, run, since) }
        fn log_interrupted_after(&self, bot: &db::Bot, run: &db::Run, sent: &[String]) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::InterruptionServices>::log_interrupted_after(self.as_ref(), bot, run, sent) }
        fn log_shows_prompt_since(
            &self,
            bot: &db::Bot,
            run: &db::Run,
            prompt: &str,
            since: chrono::DateTime<chrono::Utc>,
        ) -> impl Future<Output = bool> + Send { <T as crate::lifecycle::s6_ports::InterruptionServices>::log_shows_prompt_since(self.as_ref(), bot, run, prompt, since) }
    }
}

mod arc_port_72_relaywatchservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::RelayWatchServices + ?Sized> crate::lifecycle::s6_ports::RelayWatchServices for std::sync::Arc<T> {
        fn arm_progress(&self, run_id: &str, bot_id: &str, turn_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::RelayWatchServices>::arm_progress(self.as_ref(), run_id, bot_id, turn_id) }
        fn turn_changed(&self, turn_id: &str) -> impl Future<Output = ()> + Send { <T as crate::lifecycle::s6_ports::RelayWatchServices>::turn_changed(self.as_ref(), turn_id) }
        fn mark_run_exited(&self, run_id: &str, reason: &str) -> impl Future<Output = crate::lc_error::RunExit> + Send { <T as crate::lifecycle::s6_ports::RelayWatchServices>::mark_run_exited(self.as_ref(), run_id, reason) }
        fn sweep_dead_panes(&self) -> impl Future<Output = Vec<String>> + Send { <T as crate::lifecycle::s6_ports::RelayWatchServices>::sweep_dead_panes(self.as_ref()) }
        fn spawn_relay_watch(&self, watch: crate::lifecycle::relay_watch::Watch) { <T as crate::lifecycle::s6_ports::RelayWatchServices>::spawn_relay_watch(self.as_ref(), watch) }
    }
}

mod arc_port_73_turneventhostservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::TurnEventHostServices + ?Sized> crate::lifecycle::s6_ports::TurnEventHostServices for std::sync::Arc<T> {
        fn child_done_after_completed_turn(&self, turn_id: &str) { <T as crate::lifecycle::s6_ports::TurnEventHostServices>::child_done_after_completed_turn(self.as_ref(), turn_id) }
        fn publish_lifecycle_turn(&self, bot_id: &str, turn_id: &str, status: &str, delivery: &str) { <T as crate::lifecycle::s6_ports::TurnEventHostServices>::publish_lifecycle_turn(self.as_ref(), bot_id, turn_id, status, delivery) }
    }
}

mod arc_port_74_interruptgracehostservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::InterruptGraceHostServices + ?Sized> crate::lifecycle::s6_ports::InterruptGraceHostServices for std::sync::Arc<T> {
        fn codex_interrupted_after<'a>(
            &'a self,
            bot: &'a db::Bot,
            run: &'a db::Run,
            sent: &'a [String],
        ) -> impl Future<Output = bool> + Send + 'a { <T as crate::lifecycle::s6_ports::InterruptGraceHostServices>::codex_interrupted_after(self.as_ref(), bot, run, sent) }
    }
}

mod arc_port_75_restartholdhostservices {
    use crate::lifecycle::s6_ports::*;
    use crate::{db, lifecycle::LcError};
    use serde_json::Value;
    use std::{future::Future, path::{Path, PathBuf}};
    impl<T: crate::lifecycle::s6_ports::RestartHoldHostServices + ?Sized> crate::lifecycle::s6_ports::RestartHoldHostServices for std::sync::Arc<T> {
        fn open_restart_intents(&self) -> impl Future<Output = anyhow::Result<Vec<OpenRestartIntent>>> + Send { <T as crate::lifecycle::s6_ports::RestartHoldHostServices>::open_restart_intents(self.as_ref()) }
    }
}

mod arc_port_76_stuckpanereader {
    use crate::judge::stuck::*;
    use std::time::Instant;
    use anyhow::{anyhow, Result};
    use serde_json::{json, Value};
    impl<T: crate::judge::stuck::StuckPaneReader + ?Sized> crate::judge::stuck::StuckPaneReader for std::sync::Arc<T> {
        fn read_pane_plain_text(&self, client: &crate::herdr::HerdrClient, pane_id: &str, kind: &str) -> impl std::future::Future<Output = Result<String>> + Send { <T as crate::judge::stuck::StuckPaneReader>::read_pane_plain_text(self.as_ref(), client, pane_id, kind) }
    }
}

mod arc_port_77_reportnotifier {
    use crate::judge::report::*;
    use std::time::Instant;
    use anyhow::{anyhow, Result};
    use serde_json::{json, Value};
    impl<T: crate::judge::report::ReportNotifier + ?Sized> crate::judge::report::ReportNotifier for std::sync::Arc<T> {
        fn insert_system_message(&self, conv_id: &str, note: &str) -> impl std::future::Future<Output = Result<crate::db::Message>> + Send { <T as crate::judge::report::ReportNotifier>::insert_system_message(self.as_ref(), conv_id, note) }
    }
}

mod arc_port_78_ingress_db {
    use crate::events::ports::{BotOpsRepo, SupervisorRepo};
    use anyhow::Result;
    use serde_json::Value;

    impl<T: SupervisorRepo + ?Sized> SupervisorRepo for std::sync::Arc<T> {
        const SUPERVISOR_ID: &'static str = T::SUPERVISOR_ID;

        async fn push_inbox(
            &self,
            event_key: &str,
            kind: &str,
            assignment_id: Option<&str>,
            bot_id: Option<&str>,
            turn_id: Option<&str>,
            payload: &Value,
        ) -> Result<Option<String>> {
            <T as SupervisorRepo>::push_inbox(self.as_ref(), event_key, kind, assignment_id, bot_id, turn_id, payload).await
        }

        async fn load_owned(&self) -> Result<crate::projection::Owned> {
            <T as SupervisorRepo>::load_owned(self.as_ref()).await
        }

        fn open_states_sql() -> String {
            <T as SupervisorRepo>::open_states_sql()
        }
    }

    impl<T: BotOpsRepo + ?Sized> BotOpsRepo for std::sync::Arc<T> {
        async fn has_open_restart_for_run(&self, host: &str, bot_id: &str, run_id: &str) -> Result<bool> {
            <T as BotOpsRepo>::has_open_restart_for_run(self.as_ref(), host, bot_id, run_id).await
        }

        async fn is_share_bot(&self, bot_id: &str) -> std::result::Result<bool, sqlx::Error> {
            <T as BotOpsRepo>::is_share_bot(self.as_ref(), bot_id).await
        }
    }
}

mod arc_port_79_default_sync_lock {
    use crate::default_session::{DefaultSessionProjection, DefaultSyncLock};
    use std::future::Future;
    use std::pin::Pin;

    impl<T: DefaultSyncLock + ?Sized> DefaultSyncLock for std::sync::Arc<T> {
        fn default_sync_lock(&self) -> &tokio::sync::Mutex<()> {
            <T as DefaultSyncLock>::default_sync_lock(self.as_ref())
        }
    }

    impl<T: DefaultSessionProjection + ?Sized> DefaultSessionProjection for std::sync::Arc<T> {
        fn update_and_project<'a, U, F>(&'a self, update: F) -> Pin<Box<dyn Future<Output = anyhow::Result<U>> + Send + 'a>>
        where
            F: FnOnce(&mut crate::config::ConfigFile) -> anyhow::Result<U> + Send + 'a,
            U: Send + 'a,
        {
            <T as DefaultSessionProjection>::update_and_project(self.as_ref(), update)
        }
    }
}

impl<T> crate::lifecycle::LcHost for std::sync::Arc<T> where
    std::sync::Arc<T>: crate::lifecycle::start::StartContext
        + crate::lifecycle::s6_ports::StuckTurnContext
        + crate::lifecycle::s6_ports::DeferredLiveContext
        + crate::lifecycle::s6_ports::SendNowContext
        + crate::lifecycle::s6_ports::TurnEventHostServices
        + crate::lifecycle::s6_ports::InterruptGraceHostServices
        + crate::lifecycle::s6_ports::RestartHoldHostServices
        + crate::lifecycle::s6_ports::TranscriptOriginServices
        + crate::lifecycle::s6_ports::TurnErrorContext
        + crate::lifecycle::s6_ports::BusySendServices
        + crate::lifecycle::s6_ports::CodexSteerContext
        + crate::lifecycle::s6_ports::LiveApplyDebtContext
        + crate::lifecycle::s6_ports::GrokTranscriptContext
        + crate::lifecycle::poller::ProgressPollers
        + crate::lifecycle::poller::StallTimers
        + crate::lifecycle::poller::ProgressEmitted
        + crate::lifecycle::poller::FallbackTimers
        + crate::lifecycle::send_now::ports::AttachSendPort
        + crate::lifecycle::send_now::ports::CodexSendPort
        + crate::lifecycle::send_now::ports::IdleSleepPort
        + crate::lifecycle::send_now::ports::MaintenancePort
        + crate::lifecycle::send_now::ports::SendEnvPort
        + crate::lifecycle::send_now::ports::ShareSendRepo
        + crate::login_prompt::LoginNeeded
{}
