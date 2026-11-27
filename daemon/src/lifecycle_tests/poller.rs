#![allow(unused_imports)]

#[path = "poller/abandon_tests.rs"]
mod abandon_tests;
#[path = "poller/agy_activity_tests.rs"]
mod agy_activity_tests;
#[path = "poller/agy_not_idle_tests.rs"]
mod agy_not_idle_tests;
#[path = "poller/codex_0155_fallback_tests.rs"]
mod codex_0155_fallback_tests;
#[path = "poller/codex_limit_fallback_tests.rs"]
mod codex_limit_fallback_tests;
#[path = "poller/codex_limit_quote_tests.rs"]
mod codex_limit_quote_tests;
#[path = "poller/codex_session_refresh.rs"]
mod codex_session_refresh;
#[path = "poller/fallback_close_effects_tests.rs"]
mod fallback_close_effects_tests;
#[path = "poller/fallback_binding_tests.rs"]
mod fallback_binding_tests;
#[path = "poller/grok_limit_screen_tests.rs"]
mod grok_limit_screen_tests;
#[path = "poller/hookless_capture_tests.rs"]
mod hookless_capture_tests;
#[path = "poller/issue_17_tests.rs"]
mod issue_17_tests;
#[path = "poller/menu_fallback_tests.rs"]
mod menu_fallback_tests;
#[path = "poller/observed_status.rs"]
mod observed_status;
#[path = "poller/progress_poll_tests.rs"]
pub mod progress_poll_tests;
#[path = "poller/reply_window_tests.rs"]
mod reply_window_tests;

mod no_reply_notice_tests {
    use crate::lifecycle::poller::test_seams::is_no_reply_notice;

    #[test]
    fn identifies_no_reply_notices() {
        assert!(is_no_reply_notice("[lead 通知，不需回覆] 共用規則已新增（使用者 2026-11-14）... 照做即可，不用回覆。"));
        assert!(is_no_reply_notice("這則是 daemon 的通知，不用回覆 daemon；看完照回報接著做你的事"));
        assert!(is_no_reply_notice("（只記錄，不需回覆）"));
        assert!(is_no_reply_notice("Notice: no reply needed"));
        assert!(!is_no_reply_notice("請修正第 4、5 號問題"));
    }
}

