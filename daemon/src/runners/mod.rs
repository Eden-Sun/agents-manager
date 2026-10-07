//! 上層 runner / handler 聚合模組（r5a8 + r5a9）。
//!
//! 封裝對 `App` 與 composition 的依賴，作為背景工作與 HTTP 路由入口，
//! 下層模組只保留核心邏輯。

pub mod ask_answers;
pub(crate) mod app_ports_p11;
pub(crate) mod app_ports_p2;
pub mod autostart_revive;
pub mod background_hook;
pub mod background_jobs;
pub mod build_scheduler;
pub mod bulk_restart;
pub mod child_alerts;
pub(crate) mod claude_child_log;
pub mod child_done;
pub mod child_retire;
pub mod codex_live;
pub mod codex_model_migration;
pub mod credential_spawn;
pub mod dangerous_rm;
pub mod default_session;
pub mod events;
pub mod herdr_maintenance;
pub mod herdr_version;
pub mod github;
pub mod hook_inbox;
pub mod hookrecv;
pub mod judge;
pub mod login_assist;
pub mod login_prompt;
pub mod models;
pub mod pane_identity;
pub mod panes;
pub mod pending_question;
pub mod primary_keep_warm;
pub mod prompt_cache;
pub mod prompt_suggestion;
pub mod quota_agy;
pub mod quota;
pub mod quota_claude;
pub mod quota_grok;
pub mod reconcile;
pub mod release_triage;
pub mod remote_purge;
pub mod remote_trash;
pub mod restart_intents;
pub mod rewind;
pub mod shim_refresh;
pub mod session_paused;
pub mod share_admin;
pub(crate) mod s6_l;
pub mod mission;
pub mod supervisor;
pub(crate) mod s6_l2;
pub mod tui_prompts;
pub mod update_watch;
pub mod upstream_update;

#[cfg(test)]
#[path = "am_base_tests.rs"]
pub(crate) mod am_base_tests;
