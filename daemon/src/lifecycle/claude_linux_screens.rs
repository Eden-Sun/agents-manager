//! claude 2.1.287 在 Linux 的真畫面（2026-10-02，tmux 120x40，`--dangerously-skip-permissions`）餵給 lifecycle 的畫面解析。
//! 其他 fixture 都是 macOS 抓的（字頭 `⏺`）；Linux 是 `●`，而且短對話的輸入框釘在畫面底、狀態列跟它之間是一整段空白。
//! `draft` 是打了字還沒送。

use super::{box_state, BoxState};
use crate::lifecycle::poller::pane_still_busy;
use crate::lifecycle::screen::{clean_screen, extract_reply, last_prompt_echo_text};

const IDLE_ANSI: &str = include_str!("fixtures/claude-2.1.287-linux-idle.ansi");
const DRAFT_ANSI: &str = include_str!("fixtures/claude-2.1.287-linux-draft.ansi");
const WORKING: &str = include_str!("fixtures/claude-2.1.287-linux-working.txt");
const WORKING_ANSI: &str = include_str!("fixtures/claude-2.1.287-linux-working.ansi");
const FINISHED: &str = include_str!("fixtures/claude-2.1.287-linux-finished.txt");
const FINISHED_ANSI: &str = include_str!("fixtures/claude-2.1.287-linux-finished.ansi");

#[test]
fn the_composer_state_reads_right_on_every_linux_screen() {
    assert_eq!(box_state("claude", IDLE_ANSI), BoxState::Empty, "idle");
    assert_eq!(box_state("claude", FINISHED_ANSI), BoxState::Empty, "finished");
    assert_eq!(box_state("claude", WORKING_ANSI), BoxState::Empty, "working（回合中輸入框是空的）");
    assert_eq!(box_state("claude", DRAFT_ANSI), BoxState::NonEmpty, "打了字還沒送");
}

/// #775：2.1.287 改了淺色主題的輸入框框線與舊訊息前 `❯` 的對比。同一版在淺色／深色主題各擷一次帶顏色的畫面
/// （2026-10-03，tmux 120x40，`--settings '{"theme":"light"}'`／`dark`，default 權限模式、沒有 statusLine）：輸入框的 `❯` 是預設前景色、
/// `Try "…"` 提示是 SGR 2 dim，兩種主題都要讀成空框；打了字是草稿；回合結束後舊訊息那列帶底色的 `❯` 不是輸入框。
#[test]
fn the_composer_hint_reads_right_on_light_and_dark_themes() {
    const LIGHT_IDLE: &str = include_str!("fixtures/claude-2.1.287-light-idle.ansi");
    const LIGHT_DRAFT: &str = include_str!("fixtures/claude-2.1.287-light-draft.ansi");
    const LIGHT_FINISHED: &str = include_str!("fixtures/claude-2.1.287-light-finished.ansi");
    const DARK_IDLE_HINT: &str = include_str!("fixtures/claude-2.1.287-dark-idle-hint.ansi");
    assert_eq!(box_state("claude", LIGHT_IDLE), BoxState::Empty, "淺色：灰提示不是草稿");
    assert_eq!(box_state("claude", DARK_IDLE_HINT), BoxState::Empty, "深色：灰提示不是草稿");
    assert_eq!(box_state("claude", LIGHT_DRAFT), BoxState::NonEmpty, "淺色：打了字");
    assert_eq!(box_state("claude", LIGHT_FINISHED), BoxState::Empty, "淺色：回合結束");
    let plain: String = LIGHT_FINISHED.lines().map(crate::lifecycle::delivery::strip_ansi).collect::<Vec<_>>().join("\n");
    assert_eq!(last_prompt_echo_text("claude", &plain).as_deref(), Some("Reply with exactly PONG"));
}

#[test]
fn busy_while_the_spinner_is_up_and_not_after() {
    assert!(pane_still_busy(WORKING));
    assert!(!pane_still_busy(FINISHED));
}

#[test]
fn the_fallback_reply_is_the_text_not_the_status_rows() {
    assert_eq!(extract_reply("claude", FINISHED).as_deref(), Some("PONG"));
    let cleaned = clean_screen("claude", FINISHED).unwrap();
    assert!(cleaned.contains("PONG") && !cleaned.contains("Churned") && !cleaned.contains("bypass permissions"), "{cleaned:?}");
}

#[test]
fn the_prompt_echo_is_found_on_a_dot_marker_screen() {
    let echo = last_prompt_echo_text("claude", FINISHED).expect("有回音");
    assert!(echo.starts_with("Run the shell command: sleep 6"), "{echo}");
}

/// codex 0.160.0 的真畫面（2026-10-02，tmux 120x40，已信任的目錄，沒送任何訊息）：歡迎畫面是一整面字元畫，
/// 狀態列是 `GPT-6.1-Sol default · <cwd>`（`default` ＝ 沒指定推理強度，不是 low／medium…），輸入框上面有帳號的
/// `⚠ weekly limit: only <1% left` 提示，footer 右邊有 `⚠ 2 warnings · f2 to view`。
mod codex_0_160 {
    use super::*;
    use crate::codex_live::parse_status_line;

    const IDLE: &str = include_str!("fixtures/codex-0.160-idle.txt");
    const IDLE_ANSI: &str = include_str!("fixtures/codex-0.160-idle.ansi");

    #[test]
    fn the_idle_screen_is_an_empty_composer_and_not_busy() {
        assert_eq!(box_state("codex", IDLE_ANSI), BoxState::Empty);
        assert!(!pane_still_busy(IDLE));
    }

    /// 還沒跑完一回合時狀態列沒有 `Context`，讀不到是對的（不能把 `default` 當成強度、也不能把 warnings 那行當狀態列）。
    #[test]
    fn the_status_footer_without_context_reads_as_nothing() {
        assert_eq!(parse_status_line(IDLE), None);
    }

    /// 跑完一回合後的狀態列 `GPT-6.1-Sol default · <cwd> · Context 3% used · 5h 82% left`：模型轉小寫，`default` 不是強度。
    #[test]
    fn the_status_footer_with_context_keeps_the_model_and_no_effort() {
        let screen = "› Ask Codex to do anything\n\n  GPT-6.1-Sol default · ~/project/agents-manager · Context 3% used · 5h 82% left · weekly 97% left\n";
        let seen = parse_status_line(screen).expect("讀得到");
        assert_eq!(seen.model, "gpt-6.1-sol");
        assert_eq!(seen.effort, None);
        assert!(!seen.fast);
    }
}
