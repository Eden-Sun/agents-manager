use super::prompt_never_reached_screen;

/// 2026-09-14 w1HJ:pH（wits-c1-op-xh）turn 01M2F9DRR09TBHHYZA211ZRDHY 被判 stall 時的真實畫面：
/// `/effort max` 的回饋加上空輸入列，使用者的「think more if can be even faster」一個字都沒有。
const EFFORT_MAX_LOST: &str = "  Any screen that mounts many MUI inputs at once will hit this. If other pages also stall,
  the same default can go into the app-wide theme. Everything is still uncommitted: this fix,
  the round-2 performance changes, and the iStore multi-page parser fix.

✻ Sautéed for 15m 9s · done 2:18 PM

❯ /effort max
  ⎿  Set effort level to max (this session only): Maximum capability with deepest reasoning.
     May use excessive tokens resulting in long response times or overthinking. Use sparingly
     for the hardest tasks.
                                                                              615330 tokens
─────────────────────────────────────────────────────────────────────────────────────────────
❯
─────────────────────────────────────────────────────────────────────────────────────────────
  user. | web | OP5 61% | 5h:53%(rst 2h 35m) | 7d:95%(rst 6d 21h) | F5:100%
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";

fn sent(s: &str) -> Vec<String> {
    vec![s.to_string()]
}

/// claude 現行框（兩條全寬框線夾著 `❯`）與它的 `Try "…"` 佔位字：都是空框（sol 第八輪 #2）。
#[test]
fn the_current_claude_frame_and_its_placeholder_are_empty() {
    use crate::lifecycle::{box_state, BoxState};
    assert_eq!(box_state("claude", EFFORT_MAX_LOST), BoxState::Empty, "w1HJ:pH 的真實空框");
    // 實機 idle claude 的輸入列是 `❯` 加一個不斷行空白（ANSI 讀法還帶 `\r`）。
    let nbsp = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯\u{a0}\r\n", 1);
    assert_eq!(box_state("claude", &nbsp), BoxState::Empty);
    // 佔位字只有畫成 dim 才算；純文字的 `Try "…"` 可能是有人打的。
    let plain = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯ Try \"refactor <filepath>\"\n", 1);
    assert_eq!(box_state("claude", &plain), BoxState::NonEmpty);
    let dim = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯\u{a0}\u{1b}[2mTry \"refactor <filepath>\"\u{1b}[22m\n", 1);
    assert_eq!(box_state("claude", &dim), BoxState::Empty);
    let typed = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯ Try \"refactor\" please\n", 1);
    assert_eq!(box_state("claude", &typed), BoxState::NonEmpty);
    // marker 後只有空白是終端留下的 padding（2026-09-23：`/model` 套用後 `❯ ` 後面跟著二十幾個空格，閒置的 bot
    // 全被回 composer_busy）；真的只打了一格再送 prompt，最多是 prompt 前面多一個空白。
    let one_space = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯  \n", 1);
    assert_eq!(box_state("claude", &one_space), BoxState::Empty, "只有空白是 padding");
}

#[test]
fn the_effort_max_incident_is_a_prompt_that_never_arrived() {
    assert!(prompt_never_reached_screen("claude", EFFORT_MAX_LOST, &sent("think more if can be even faster")));
}

#[test]
fn a_submitted_prompt_is_not_resent() {
    // claude 收下了：`❯ …` 印在對話區、輸入列又空了。重送就是重複派工。
    let accepted = EFFORT_MAX_LOST.replacen(
        "─────────────────────────────────────────────────────────────────────────────────────────────\n❯\n",
        "❯ think more if can be even faster\n─────────────────────────────────────────────────────────────────────────────────────────────\n❯\n",
        1,
    );
    assert!(!prompt_never_reached_screen("claude", &accepted, &sent("think more if can be even faster")));
    // 排隊中的訊息（`  ❯ …`）一樣算到了。
    let queued = EFFORT_MAX_LOST.replacen("615330 tokens", "615330 tokens\n  ❯ think more if can be even faster", 1);
    assert!(!prompt_never_reached_screen("claude", &queued, &sent("think more if can be even faster")));
}

#[test]
fn text_still_in_the_box_is_the_enter_nudges_job_not_a_resend() {
    let in_box = EFFORT_MAX_LOST.replacen("\n❯\n", "\n❯ think more if can be even faster\n", 1);
    assert!(!prompt_never_reached_screen("claude", &in_box, &sent("think more if can be even faster")));
}

#[test]
fn a_prompt_too_short_to_look_for_is_never_resent() {
    // 兩個字的 prompt 在任何畫面都找得到／找不到都不可靠：證明不了「沒到」就不重送。
    assert!(!prompt_never_reached_screen("claude", EFFORT_MAX_LOST, &sent("go")));
    assert!(!prompt_never_reached_screen("claude", EFFORT_MAX_LOST, &[]));
}

#[test]
fn a_long_prompt_matches_on_its_head_even_when_wrapped() {
    // 長訊息在畫面上會折行；比對去掉空白後的開頭，折在哪裡都一樣。
    let long = "please make the offline quote import even faster and report the numbers again";
    let wrapped = EFFORT_MAX_LOST.replacen("615330 tokens", "615330 tokens\n❯ please make the offline\n  quote import even faster", 1);
    assert!(!prompt_never_reached_screen("claude", &wrapped, &sent(long)));
    assert!(prompt_never_reached_screen("claude", EFFORT_MAX_LOST, &sent(long)));
}
