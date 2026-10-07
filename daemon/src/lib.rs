//! agents-managerd 的函式庫；`am-base` 模組由此 re-export，`main.rs` 只做 CLI 解析與派發。


pub mod runners;
pub use am_base::config;
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
mod ask_answers;
mod identity_kind;
mod blocked_reason;
mod child_alerts;
mod child_done;
mod child_retire;
pub mod daemon_notice;
mod dangerous_rm;
mod default_session;
mod due_actions;
mod events;
mod handoff;
mod tui_prompts;
mod background_jobs;
mod codex_live;
mod codex_model_migration;
mod panes;
mod projection;
mod supervisor_owned;
mod relay_auth;
mod deleted_bots;
mod build_info;
mod app_ports_r2a9;
mod api;
#[cfg(test)]
mod bot_read_scope_tests;
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
mod codex_history;
mod app_ports_p0;
mod app_ports_p1;
mod app_ports_p10;
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
mod share;
mod herdr_upgrade;
mod judge;
mod hookrecv;
mod lifecycle;
mod swap_window;
mod mission;
mod preview;
mod primary_order;
mod pane_probe;
mod agy_install;
mod read_marks;
pub mod remote_cargo;
mod reconcile;
mod state;
mod supervisor;
mod supervisor_evidence;
mod startup;
mod service_auth;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod test_home;
#[cfg(test)]
mod timestamp_compat_tests;
#[cfg(test)]
mod same_ms_order_tests;
mod turn_error;
pub mod serve;
pub mod cli;
pub use cli::{Cli, Cmd};
