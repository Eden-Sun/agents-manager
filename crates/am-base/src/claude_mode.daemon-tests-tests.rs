
    use super::*;

    #[test]
    fn the_four_permission_mode_rows_match_and_a_paused_reply_does_not() {
        for row in [
            "⏸ manual mode on · ← for agents",
            "⏸ manual mode on · ? for shortcuts",
            "⏸ plan mode on (shift+tab to cycle) · ← for agents",
            "⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
            "bypass permissions on · ← for agents",
            "permissions on (shift+tab to cycle)",
            "(Shift+Tab to cycle) · ← for agents",
        ] {
            assert!(is_mode_row(row), "{row}");
        }
        for reply in [
            "⏸ 暫停：等使用者決定 · mode on 的說明",
            "⏸ 暫停部署",
            "⏸ plan mode on the left is still default",
            "⏸ Quoted status: plan mode on",
            "⏸ The manual mode on label is confusing here.",
            "The manual mode on label is confusing here.",
            "我把 bypass permissions on 這個模式關掉了",
        ] {
            assert!(!is_mode_row(reply), "{reply}");
        }
        let fixture = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.288-manual-mode-finished.txt");
        let row = fixture.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
        assert!(is_mode_row(row), "{row}");
    }
