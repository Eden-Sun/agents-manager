/// 受護欄管的 session 組 production 檔。
const SOURCES: &[(&str, &str)] = &[
    ("lifecycle/start.rs", include_str!("../../../crates/am-lifecycle/src/lifecycle/start.rs")),
    ("lifecycle/stop.rs", include_str!("../../../crates/am-lifecycle/src/lifecycle/stop.rs")),
    ("lifecycle/setup.rs", include_str!("../../../crates/am-lifecycle/src/lifecycle/setup.rs")),
    ("lifecycle/agy_hook.rs", include_str!("../../../crates/am-lifecycle/src/lifecycle/agy_hook.rs")),
    ("lifecycle/agy_session.rs", include_str!("../../../crates/am-lifecycle/src/lifecycle/agy_session.rs")),
    ("lifecycle/grok_hook.rs", include_str!("../../../crates/am-lifecycle/src/lifecycle/grok_hook.rs")),
    ("lifecycle/codex_steer.rs", include_str!("../../../crates/am-lifecycle/src/lifecycle/codex_steer.rs")),
];

/// 每一行是否落在 `#[cfg(test)]` 項目裡（從屬性那行到項目的大括號收尾）。
use am_base::source_mask::test_mask;

/// 以「呼叫」的形狀擋：函式名後面接 `(`。
const FORBIDDEN: &[&str] = &[
    "handoff::bot_handed_off_to(",
    "handoff::refuse(",
    "share::store::restricted_workspace(",
    "share::store::caged_workspace(",
    "intents::prepare_restart(",
    "intents::complete(",
    "intents::abandon(",
    "intents::fail(",
    "events::unwatch_pane_on_session(",
    "remote_purge::record(",
    "remote_trash::move_in(",
    "preview::stop_for_bot(",
    "claude_live::start_fresh(",
    "models::list(",
    "herdr_shim::install_local(",
    "herdr_shim::install_remote(",
    "cargo_shim::install_local(",
    "cargo_shim::install_remote(",
    "kill_shim::install_local(",
    "kill_shim::install_remote(",
    "emit_turn(",
    "emit_message_added(",
    "app.emit(",
    "app.emit_bot_status(",
    "app.bot_lock(",
];

#[test]
fn session_production_code_reaches_other_features_only_through_ports() {
    let mut offenders = Vec::new();
    for (file, src) in SOURCES {
        let mask = test_mask(src);
        for (n, line) in src.lines().enumerate() {
            if mask[n] || line.trim_start().starts_with("//") {
                continue;
            }
            for pat in FORBIDDEN {
                if line.contains(pat) {
                    offenders.push(format!("{file}:{}: {pat}  ← {}", n + 1, line.trim()));
                }
            }
        }
    }
    assert!(offenders.is_empty(), "這些呼叫要走 session_ports／am-ports adapter（由 app_ports_p4sess.rs 委派）：\n{}", offenders.join("\n"));
}

/// 反向確認：護欄禁的呼叫真的在 adapter 裡（改名或搬走時這條先紅，提醒更新清單）。
#[test]
fn the_adapter_still_calls_what_the_guard_forbids_elsewhere() {
    let adapter = include_str!("../app_ports_p4sess.rs");
    for pat in [
        "handoff::bot_handed_off_to(",
        "handoff::refuse(",
        "store::restricted_workspace(",
        "store::caged_workspace(",
        "intents::prepare_restart(",
        "intents::complete(",
        "intents::abandon(",
        "intents::fail(",
        "events::unwatch_pane_on_session(",
        "remote_purge::record(",
        "remote_trash::move_in(",
        "preview::stop_for_bot(",
        "claude_live::start_fresh(",
        "models::list(",
        "herdr_shim::install_local(",
        "herdr_shim::install_remote(",
        "cargo_shim::install_local(",
        "cargo_shim::install_remote(",
    "kill_shim::install_local(",
    "kill_shim::install_remote(",
        "bot_lock(",
    ] {
        assert!(adapter.contains(pat), "adapter 不再含 {pat}");
    }
}
