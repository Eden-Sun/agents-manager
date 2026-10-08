//! agents-managerd 的函式庫；`am-base` 模組由此 re-export，`main.rs` 只做 CLI 解析與派發。


pub mod runners;
pub use am_base::config;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/arc_ports.rs"]
mod am_lifecycle_arc_ports;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/ports_impl.rs"]
mod am_lifecycle_ports_impl;
#[cfg(test)]
#[path = "../../crates/am-supervisor/src/app_arc_ports.rs"]
mod am_supervisor_arc_ports;
#[cfg(not(test))]
pub use am_lifecycle::{
    ask_answers,
    background_jobs,
    blocked_reason,
    child_alerts,
    child_done,
    codex_history,
    codex_live,
    codex_model_migration,
    daemon_notice,
    default_session,
    dangerous_rm,
    events,
    handoff,
    hookrecv,
    judge,
    lifecycle,
    tui_prompts,
    turn_error,
};
#[cfg(not(test))]
pub use am_supervisor::{build_info, mission, relay_auth, supervisor};
// App-backed unit tests still belong to the daemon crate. Under `cargo test`, compile the same
// source modules at the crate root so their `crate::` paths and the App adapters share one set of
// types. Production builds use the extracted crate re-exports above.
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/ask_answers.rs"]
pub mod ask_answers;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/background_jobs.rs"]
pub mod background_jobs;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/blocked_reason.rs"]
pub mod blocked_reason;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/child_alerts.rs"]
pub mod child_alerts;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/child_done.rs"]
pub mod child_done;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/codex_history.rs"]
pub mod codex_history;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/codex_live.rs"]
pub mod codex_live;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/codex_model_migration.rs"]
pub mod codex_model_migration;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/daemon_notice.rs"]
pub mod daemon_notice;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/default_session.rs"]
pub mod default_session;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/dangerous_rm.rs"]
pub mod dangerous_rm;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/events.rs"]
pub mod events;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/handoff.rs"]
pub mod handoff;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/hookrecv.rs"]
pub mod hookrecv;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/judge.rs"]
pub mod judge;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/lifecycle/mod.rs"]
pub mod lifecycle;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/tui_prompts.rs"]
pub mod tui_prompts;
#[cfg(test)]
#[path = "../../crates/am-lifecycle/src/turn_error.rs"]
pub mod turn_error;
pub use am_base::{
    agent_relay,
    agy_remote,
    agy_screen,
    agy_support,
    assets,
    attach,
    background_hook,
    background_loop,
    bot_input,
    bot_trash,
    build_scheduler,
    bulk_restart,
    cache_clock,
    capabilities,
    capture,
    cargo_shim,
    changelog,
    child_reconcile_safety,
    child_runtime,
    claude_live,
    claude_mode,
    codex_update,
    composer_parse,
    config_audit,
    codex_status,
    credential_spawn,
    db,
    exec_retry,
    fork_ops,
    git_sh,
    github,
    herdr,
    herdr_maintenance,
    herdr_shim,
    herdr_unit,
    herdr_update,
    herdr_version,
    home,
    hook_body,
    hook_cmd,
    hook_inbox,
    host_baseline,
    hosts,
    kids_cache,
    kind_probe,
    launch_rev,
    lc_error,
    linux_proc,
    local_image,
    local_sh,
    login_assist,
    login_prompt,
    memproc,
    memstat,
    models,
    outbox,
    outbox_remote,
    pane_identity,
    pasted_content,
    pending_question,
    preview_bind,
    primary_keep_warm,
    private_files,
    probe_ws,
    prompt_cache,
    prompt_suggestion,
    quota,
    quota_agy,
    quota_claude,
    quota_grok,
    release_triage,
    remote_health,
    remote_purge,
    remote_trash,
    request_id,
    restart_coalesce,
    restart_intents,
    rewind,
    session_paused,
    shared_host,
    shim_path,
    shim_refresh,
    spawn_hints,
    statusline_cmd,
    supervisor_inbox,
    tools,
    transcript_read,
    trust,
    trusted_open,
    update_watch,
    upstream_update,
};


#[cfg(test)]
pub use am_base::race_point;
mod projection;
mod identity_kind;
mod child_retire;
pub mod due_actions;
mod panes;
mod supervisor_owned;
mod deleted_bots;
mod app_ports_p6;
mod app_ports_p7;
#[cfg(test)]
#[path = "../../crates/am-supervisor/src/build_info.rs"]
pub mod build_info;
#[cfg(test)]
#[path = "../../crates/am-supervisor/src/relay_auth.rs"]
pub mod relay_auth;
mod app_ports_r2a9;
mod api;
#[cfg(test)]
mod bot_read_scope_tests;
#[cfg(test)]
mod ws_event_docs_tests;
mod bot_state;
mod intents;
mod delete_intents;
mod deploy_now;
mod deploy_wait;
mod drafts;
mod promote_intents;
mod autostart_revive;
mod claude_review;
mod cli_update;
mod app_ports_p0;
mod app_ports_p1;
mod app_ports_p10;
pub(crate) mod app_ports_p4;
pub(crate) mod app_ports_p4obs;
pub(crate) mod app_ports_p4send;
pub(crate) mod app_ports_p4state;
pub(crate) mod app_ports_p9;
mod app_ports_p12;
mod app_ports_p13;
mod app_ports_p3;
mod app_ports_p5;
pub(crate) mod app_ports_p8;
mod app_ports_r2a8;
mod grok_live;
mod fork;
mod remote_perms;
mod promote;
mod gh_auth;
mod git_quick;
mod group;
#[cfg(not(test))]
pub use am_share::share;
// App-backed share tests (tests.rs etc.) belong to the daemon crate: under `cargo test` compile the
// same sources at the crate root so `crate::` paths and the App adapters share one set of types.
#[cfg(test)]
#[path = "../../crates/am-share/src/share/mod.rs"]
pub mod share;
mod herdr_upgrade;
mod swap_window;
#[cfg(test)]
#[path = "../../crates/am-supervisor/src/mission/mod.rs"]
pub mod mission;
mod preview;
mod primary_order;
mod pane_probe;
mod agy_install;
mod read_marks;
pub mod remote_cargo;
mod reconcile;
mod state;
#[cfg(test)]
#[path = "../../crates/am-supervisor/src/supervisor/mod.rs"]
pub mod supervisor;
mod supervisor_evidence;
mod startup;
mod service_auth;
#[cfg(test)]
mod ingress_ports;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod test_home;
#[cfg(test)]
mod timestamp_compat_tests;
#[cfg(test)]
mod same_ms_order_tests;
pub mod serve;
pub mod cli;
pub use cli::{Cli, Cmd};
