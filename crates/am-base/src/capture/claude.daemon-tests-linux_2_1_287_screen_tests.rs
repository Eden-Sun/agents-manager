
    use super::*;

    const IDLE: &str = include_str!("../../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-linux-idle.txt");
    const WORKING: &str = include_str!("../../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-linux-working.txt");
    const FINISHED: &str = include_str!("../../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-linux-finished.txt");

    #[test]
    fn busy_only_while_the_spinner_row_is_up() {
        assert!(ClaudeCapture.still_busy(WORKING));
        assert!(!ClaudeCapture.still_busy(FINISHED));
        assert!(!ClaudeCapture.still_busy(IDLE));
    }

    #[test]
    fn an_empty_composer_awaits_input() {
        assert!(ClaudeCapture.awaits_input(IDLE));
        assert!(ClaudeCapture.awaits_input(FINISHED));
    }

    /// 回覆是 `● PONG`；完成行 `✻ Churned for 9s · done 3:03 PM` 不進回覆。
    #[test]
    fn the_reply_is_read_after_a_dot_marker() {
        assert_eq!(ClaudeCapture.extract_reply(FINISHED).as_deref(), Some("PONG"));
    }

    /// 回合中畫面底下只有「● Sleeping for six seconds / ⎿  $ sleep 6」這個工具列（新的「描述＋指令」寫法，沒有 `Bash(…)`）：
    /// 照「整回合只有工具呼叫就取最後一個」的老規則，不會把 spinner 或輸入框 chrome 帶進來。
    #[test]
    fn a_running_tool_row_does_not_pull_in_the_spinner_or_the_composer() {
        let reply = ClaudeCapture.extract_reply(WORKING).unwrap();
        assert_eq!(reply, "Sleeping for six seconds\n  ⎿  $ sleep 6");
    }

    #[test]
    fn activity_is_the_spinner_row() {
        assert_eq!(ClaudeCapture.activity(WORKING).as_deref(), Some("Frosting… (2s · ↓ 77 tokens)"));
    }
