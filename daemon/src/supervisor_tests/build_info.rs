//! #1862 S3b：從 `crates/am-supervisor/src/build_info.rs` 搬來的 daemon 整合測試；``build_info::daemon_wiring_smoke::…` → `supervisor_tests::build_info::daemon_wiring_smoke::…``。
//! 測試本文與斷言不變；私有項由 am-supervisor 的 `*_for_test`（test-hooks）轉呼叫，下面用別名維持原名。

#![allow(unused_imports)]

mod daemon_wiring_smoke {
    #[test]
    fn extracted_features_and_the_cli_remain_wired_into_the_daemon() {
        use clap::CommandFactory as _;
        assert_eq!(crate::Cli::command().get_version(), Some(crate::build_info::VERSION));
        let payload = serde_json::json!({});
        assert!(crate::ask_answers::carried(&payload).is_empty());
        assert!(am_lifecycle::ask_answers::carried(&payload).is_empty());
        assert!(crate::share::svg_check::check("<svg/>").is_ok());
        assert!(am_share::share::svg_check::check("<svg/>").is_ok());
        assert_eq!(crate::supervisor::assignment_state::ALL.len(), am_supervisor::supervisor::assignment_state::ALL.len());
    }
}
