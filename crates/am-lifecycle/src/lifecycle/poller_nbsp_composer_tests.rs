/// 2026-10-01 cf-ox-2（claude 2.1.285）：空框畫成 `❯` 接 U+00A0。以前只把空格與 tab 當空白，空框認不出來，
/// `composer_text` 往上走到使用者那則的回音、回報「輸入列有字」，倒回連三次 `composer_busy`。
#[test]
fn an_empty_box_drawn_with_a_no_break_space_is_empty() {
    let rule = "─".repeat(40);
    let screen = format!(
        "❯\u{a0}那不用\n  目前長這樣，優化下\n\n⏺ 先確認規則，再寫進任務說明。\n  ⎿ \u{a0}Interrupted · What should Claude do instead?\n\n{rule}\n❯\u{a0}\n{rule}\n  hunta | pt | OP5.5 M 75%\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n"
    );
    assert!(super::pane_awaits_input("claude", &screen));
    assert_eq!(super::composer_text("claude", &screen), None, "框是空的，不能往上撈到回音");
    let typed = screen.replace("{rule}\n❯\u{a0}\n".replace("{rule}", &rule).as_str(), &format!("{rule}\n❯\u{a0}half typed\n"));
    assert_eq!(super::composer_text("claude", &typed).as_deref(), Some("half typed"), "框裡真的有字照樣讀得到");
}
