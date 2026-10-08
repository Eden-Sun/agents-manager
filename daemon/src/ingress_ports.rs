//! Compile-time architecture guards for the P8 ports now owned by am-lifecycle.
#[cfg(test)]
use crate::events::ports::{IngressCommands, ReconcileCommands};

#[cfg(test)]
mod tests {
    /// 入口那一組與對帳那一組的 production 檔（組內互相呼叫照舊，組與組之間只准走 [`super::ReconcileCommands`]／[`super::IngressCommands`]）。
    const INGRESS: &[(&str, &str)] = &[
        ("ask_answers.rs", include_str!("../../crates/am-lifecycle/src/ask_answers.rs")),
        ("background_hook.rs", include_str!("../../crates/am-base/src/background_hook.rs")),
        ("background_jobs.rs", include_str!("../../crates/am-lifecycle/src/background_jobs.rs")),
        ("background_loop.rs", include_str!("../../crates/am-base/src/background_loop.rs")),
        ("blocked_reason.rs", include_str!("../../crates/am-lifecycle/src/blocked_reason.rs")),
        ("bot_state.rs", include_str!("bot_state.rs")),
        ("child_alerts.rs", include_str!("../../crates/am-lifecycle/src/child_alerts.rs")),
        ("child_done.rs", include_str!("../../crates/am-lifecycle/src/child_done.rs")),
        ("child_reconcile_safety.rs", include_str!("../../crates/am-base/src/child_reconcile_safety.rs")),
        ("child_retire.rs", include_str!("child_retire.rs")),
        ("child_runtime.rs", include_str!("../../crates/am-base/src/child_runtime.rs")),
        ("events.rs", include_str!("../../crates/am-lifecycle/src/events.rs")),
        ("hook_cmd.rs", include_str!("../../crates/am-base/src/hook_cmd.rs")),
        ("hook_inbox.rs", include_str!("../../crates/am-base/src/hook_inbox.rs")),
        ("hookrecv.rs", include_str!("../../crates/am-lifecycle/src/hookrecv.rs")),
        ("pending_question.rs", include_str!("../../crates/am-base/src/pending_question.rs")),
        ("spawn_hints.rs", include_str!("../../crates/am-base/src/spawn_hints.rs")),
        ("dangerous_rm.rs", include_str!("../../crates/am-lifecycle/src/dangerous_rm.rs")),
    ];
    const RECONCILE: &[(&str, &str)] = &[
        ("reconcile.rs", include_str!("reconcile.rs")),
        ("autostart_revive.rs", include_str!("autostart_revive.rs")),
        ("default_session.rs", include_str!("../../crates/am-lifecycle/src/default_session.rs")),
        ("due_actions.rs", include_str!("due_actions.rs")),
        ("session_paused.rs", include_str!("../../crates/am-base/src/session_paused.rs")),
        ("kids_cache.rs", include_str!("../../crates/am-base/src/kids_cache.rs")),
        ("read_marks.rs", include_str!("read_marks.rs")),
    ];

    /// 每一行是否落在 `#[cfg(test)]` 項目裡（從屬性那行到項目的大括號收尾）。
    fn test_mask(src: &str) -> Vec<bool> {
        let lines: Vec<&str> = src.lines().collect();
        let mut mask = vec![false; lines.len()];
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim_start().starts_with("#[cfg(test)]")
                || lines[i].trim_start().starts_with("#[cfg(all(test, feature = \"daemon-test-harness\"))]")
            {
                let (mut depth, mut seen, mut j) = (0i32, false, i);
                while j < lines.len() {
                    mask[j] = true;
                    depth += lines[j].matches('{').count() as i32 - lines[j].matches('}').count() as i32;
                    seen |= lines[j].contains('{');
                    if seen && depth <= 0 {
                        break;
                    }
                    if !seen && lines[j].trim_end().ends_with(';') && !lines[j].trim_start().starts_with("#[") {
                        break;
                    }
                    j += 1;
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        mask
    }

    /// 以「呼叫」的形狀擋：函式名後面接 `(`。
    fn feature_calls() -> Vec<String> {
        let mut v: Vec<String> = [
            "emit_message_added", "emit_turn", "schedule_flush_queued", "schedule_deferred_live", "schedule_codex_notice_capture",
            "poke_resume_nudge", "cancel_stall", "arm_fallback", "arm_progress", "arm_stall", "begin_external_turn", "mark_run_exited",
            "context_lost", "retire_context_lost", "settle_interruption", "settle_owed_deliveries", "settle_interrupt_echo",
            "start_bot", "start_bot_locked_with", "adopt_unbound_send_nows", "rearm_queue_retries", "adopt_turns_of_ended_runs",
            "rearm_queued_prompt_restamps", "adopt_interrupted_on_restart", "spawn_adopted_capture", "sweep_stuck_turns",
            "close_after_session_paused", "close_pane_and_tab", "observe_agent_status", "insert_message", "insert_message_tx",
            "insert_message_relayed_tx", "prompt_relayed_queueable",
        ]
        .iter()
        .map(|n| format!("lifecycle::{n}("))
        .collect();
        v.extend(
            [
                "lifecycle::start_send::resume_after_boot(",
                "turn_controller::set_status_on(",
                "turn_controller::complete_with_native_evidence(",
                "turn_controller::fail_with_native_evidence(",
                "fence::classify(",
                "quota::clear_limit_hit_for_bot(",
                "quota::quota_base_for_host(",
                "quota::set(",
                "turn_error::mark_claude_limit_hit(",
                "turn_error::mark_agy_limit_hit(",
                "supervisor::store::push_inbox(",
                "supervisor::store::sql_list(",
                "supervisor_owned::load(",
                "idle_sleep::observe_status(",
                "handoff::bot_handed_off_to(",
                "handoff::bot_handed_off_to_on(",
                "handoff::footprint(",
                "restart_intents::recover_host(",
                "delete_intents::recover_host(",
                "promote_intents::recover_host(",
                "restart_intents::has_open_restart_for_run(",
                "panes::scan_snapshot(",
                "panes::gc_host(",
                "panes::notify_unowned_and_orphans(",
                "share::store::is_share_bot(",
                "intents::record_done(",
                "runners::herdr_version::refresh(",
                "herdr_maintenance::active(",
                "runners::github::spawn_detect_host(",
                "state::emit_daemon_status(",
                "state::set_default_connected(",
                "claude_live::adopt_statusline_model(",
                "login_prompt::on_auth_failure(",
                "login_prompt::on_turn_ok(",
                "codex_model_migration::on_blocked(",
                "prompt_suggestion::on_idle(",
                "tui_prompts::dismiss_if_survey(",
                "api::state_json(",
                "api::ct_eq(",
                "SUPERVISOR_ID",
            ]
            .map(String::from),
        );
        v
    }

    const TO_RECONCILE: &[&str] = &[
        "reconcile::reconcile_host(",
        "reconcile::autostart_after_reconcile(",
        "reconcile::schedule_deferred_pass(",
        "default_session::sync(",
        "session_paused::on_idle(",
    ];
    const TO_INGRESS: &[&str] = &[
        "events::watch_pane_on_session(",
        "events::unwatch_pane_on_session(",
        "child_retire::retire_at(",
        "child_reconcile_safety::retirement_block(",
        "spawn_hints::prune_stale(",
        "spawn_hints::for_host(",
        "spawn_hints::consume(",
    ];

    fn offenders(files: &[(&str, &str)], forbidden: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for (file, src) in files {
            let mask = test_mask(src);
            for (n, line) in src.lines().enumerate() {
                if mask[n] || line.trim_start().starts_with("//") {
                    continue;
                }
                for pat in forbidden {
                    // `<App as SupervisorRepo>::SUPERVISOR_ID` 是走介面的寫法，放行。
                    if line.contains(pat.as_str()) && !(pat == "SUPERVISOR_ID" && line.contains("SupervisorRepo")) {
                        out.push(format!("{file}:{}: {pat}  ← {}", n + 1, line.trim()));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn ingress_and_reconcile_reach_other_features_only_through_ports() {
        let all: Vec<(&str, &str)> = INGRESS.iter().chain(RECONCILE.iter()).copied().collect();
        let found = offenders(&all, &feature_calls());
        assert!(found.is_empty(), "這些呼叫要走 events::ports（由 app_ports_p8.rs 委派）：\n{}", found.join("\n"));
    }

    #[test]
    fn ingress_and_reconcile_do_not_call_each_other_directly() {
        let to_reconcile: Vec<String> = TO_RECONCILE.iter().map(|s| s.to_string()).collect();
        let to_ingress: Vec<String> = TO_INGRESS.iter().map(|s| s.to_string()).collect();
        let mut found = offenders(INGRESS, &to_reconcile);
        found.extend(offenders(RECONCILE, &to_ingress));
        assert!(found.is_empty(), "入口與對帳之間要走 ReconcileCommands／IngressCommands：\n{}", found.join("\n"));
    }

    /// 反向確認：護欄禁的呼叫真的在 adapter 裡（改名或搬走時這條先紅，提醒更新清單）。
    #[test]
    fn the_adapter_still_calls_what_the_guards_forbid_elsewhere() {
        let adapter = include_str!("app_ports_p8.rs");
        let lifecycle_adapter = include_str!("../../crates/am-lifecycle/src/ports_impl.rs");
        let mut pats = feature_calls();
        pats.retain(|p| p != "SUPERVISOR_ID" && p != "lifecycle::start_send::resume_after_boot(");
        for p in pats.iter().map(String::as_str).chain(TO_RECONCILE.iter().copied()).chain(TO_INGRESS.iter().copied()) {
            // adapter 在 `use crate::lifecycle::{self, …}` 之後寫 `lifecycle::foo(`，其餘寫成完整路徑；兩種都以子字串比對。
            let short = p.rsplit("::").next().unwrap_or(p);
            let module = p.trim_end_matches('(').rsplit("::").nth(1).unwrap_or("");
            let probe = format!("{module}::{short}");
            assert!(adapter.contains(&probe) || lifecycle_adapter.contains(&probe), "P8 adapter 不再含 {probe}");
        }
        assert!(adapter.contains("SUPERVISOR_ID") && adapter.contains("resume_after_boot("));
    }

    #[test]
    fn test_mask_skips_cfg_test_items() {
        assert!(test_mask("fn a() {}\n#[cfg(test)]\nmod t {\n    fn b() {}\n}\nfn c() {}\n") == vec![false, true, true, true, true, false]);
        assert!(test_mask("fn a() {}\n#[cfg(all(test, feature = \"daemon-test-harness\"))]\nmod t {\n    fn b() {}\n}\nfn c() {}\n") == vec![false, true, true, true, true, false]);
    }
}
