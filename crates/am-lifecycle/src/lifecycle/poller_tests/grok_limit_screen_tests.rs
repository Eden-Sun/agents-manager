//! #222：grok 撞週限。6d5e74ab 讓掃描（`capture_codex_usage_notices`）認得那兩句，這裡驗**它有沒有被叫到**：
//! herdr 把這張畫面判成 `blocked`（不是 working→idle），備援與掃描都靠 idle 邊觸發，blocked 的 bot 沒人看。
use super::*;

const WEEKLY: &str = "You hit your weekly limit.";
const BALANCE: &str = "Turn failed: Request failed (402): Grok Build usage balance exhausted";

/// 票上貼的網頁「終端擷取」原文（2026-09-19 16:26，grok 1.0.34）。
const TICKET_SCREEN: &str = "\
┃  You hit your weekly limit.
┃
┃  1 (○) Upgrade tier      Upgrade to a higher tier for more usage
┃  2 (○) Buy more credits  Purchase credits to keep using Grok Build
┃  3 (○) Try Again         Resubmit the last prompt once you have usage again
┃
┃  ↑/↓ navigate · y copy
Enter:submit
Tab:next answer  |  Esc:scrollback  |  Shift+x:dismiss
";
/// 6d5e74ab 用的 w168:pB7 真畫面（帶 402 那一句與縮排）。
const PB7_SCREEN: &str = include_str!("../fixtures/grok_limit_hit.txt");

/// #227（r1 的重現）：grok 畫面上印出的別人的輸出——原始碼、diff、測試斷言訊息——含有那幾句，不是撞限。
#[test]
fn grok_source_lines_are_not_limit_hits() {
    let lines = [
        r#"daemon/src/lifecycle/screen.rs:439:        || low.contains("you hit your weekly limit")"#,
        r#"+  ┃  Turn failed: Request failed (402): Grok Build usage balance exhausted"#,
        r#"thread 'x' panicked at src/a.rs:1:1: expected no hit: ["You hit your weekly limit."]"#,
        r#"    assert!(lines.iter().any(|l| l.contains("usage balance exhausted")), "{lines:?}");"#,
        "Request failed (402): Grok Build usage balance exhausted",
        "The 402 error says usage balance exhausted, so you hit your weekly limit.",
    ];
    let hits: Vec<&str> = lines.iter().copied().filter(|l| grok_limit_hit_line(l).is_some()).collect();
    assert!(hits.is_empty(), "{hits:#?}");
    assert!(grok_limit_hit_line("  ┃  You hit your weekly limit.").is_some());
    assert!(grok_limit_hit_line("  ┃  You've hit your weekly limit.").is_some());
    assert!(grok_limit_hit_line("  ┃  Turn failed: Request failed (402): Grok Build usage balance exhausted").is_some());
}

/// #227：cat 出來的 fixture 一字不差（連框線都在），但它不是畫面最底下那張會擋住輸入列的選單——底下還有 bot 自己的回覆與輸入列。
/// 真選單是 modal：標題、兩個以上的選項、之後只剩選單自己的說明列。
#[test]
fn output_that_quotes_the_limit_screen_is_not_a_limit() {
    let cat = format!("❯ cat daemon/src/lifecycle/fixtures/grok_limit_hit.txt\n{PB7_SCREEN}\n⏺ 這是 6d5e74ab 的 fixture，我在讀它。\n────\n❯\n────\n");
    let ticket = format!("❯ 貼一下票上的原文\n{TICKET_SCREEN}\n⏺ 好了。\n❯\n");
    let diff = "❯ git diff\n+  ┃  You hit your weekly limit.\n+  ┃  Turn failed: Request failed (402): Grok Build usage balance exhausted\n⏺ 看完了\n❯\n";
    let test_out = "❯ cargo test\nthread 'x' panicked at src/a.rs:1:1: expected no hit: [\"You hit your weekly limit.\"]\n⏺ 紅了\n❯\n";
    let bare = "❯ echo\n┃  You hit your weekly limit.\n⏺ 我把這句印出來看看\n";
    let title_only = "┃  You hit your weekly limit.\n┃\n┃  ↑/↓ navigate · y copy   Enter:submit\n";
    let one_option = "┃  You hit your weekly limit.\n┃  3 (○) Try Again         Resubmit the last prompt once you have usage again\n┃  ↑/↓ navigate · y copy\n";
    for (what, screen) in [
        ("cat", cat.as_str()),
        ("票上原文貼在對話裡", ticket.as_str()),
        ("diff", diff),
        ("測試輸出", test_out),
        ("只有句子", bare),
        ("標題加說明列、沒有選項", title_only),
        ("只有一個選項", one_option),
    ] {
        let lines = grok_limit_notice_lines(screen);
        assert!(lines.is_empty(), "{what}：{lines:?}");
    }
}

/// 真選單前面就算捲著引用同一句話的輸出，還是要認（而且只記那張選單，不記上面引用的）。
#[test]
fn the_real_menu_is_read_under_quoted_scrollback() {
    let filler: String = (0..24).map(|i| format!("⏺ 之後又做了第 {i} 件事\n")).collect();
    let screen = format!("❯ rg limit\n┃  You hit your daily limit.\n┃  Turn failed: Request failed (402): Grok Build usage balance exhausted\n{filler}{TICKET_SCREEN}");
    assert_eq!(grok_limit_notice_lines(&screen), vec![WEEKLY.to_string()]);
    let pb7 = grok_limit_notice_lines(PB7_SCREEN);
    assert_eq!(pb7.len(), 2, "{pb7:?}");
}

/// 票上要求：同一選單的其他變體（session／daily limit）也要認。6d5e74ab 的辨識只有 `weekly` 與 402 兩句。
#[test]
fn the_other_limit_variants_the_ticket_names_are_read_too() {
    for (title, window) in [
        ("You hit your session limit.", (Some("five_hour"), 5)),
        ("You hit your 5-hour limit.", (Some("five_hour"), 5)),
        ("You hit your daily limit.", (None, 24)),
        ("You hit your monthly limit.", (None, 5)),
        ("You've hit your weekly limit.", (Some("seven_day"), 168)),
    ] {
        let screen = TICKET_SCREEN.replace("You hit your weekly limit.", title);
        let lines = grok_limit_notice_lines(&screen);
        assert_eq!(lines, vec![title.to_string()], "{title}");
        assert_eq!(grok_limit_window(&lines[0]), window, "{title}");
    }
    assert!(grok_limit_hit_line("Then you hit your daily limit and stopped").is_none());
    assert!(grok_limit_hit_line("You hit your daily limits, apparently").is_none());
    assert!(grok_limit_hit_line("You hit your limit").is_none(), "沒有說是哪個窗");
}

#[test]
fn the_window_a_grok_line_names() {
    assert_eq!(grok_limit_window("┃  You hit your weekly limit."), (Some("seven_day"), 168));
    assert_eq!(grok_limit_window("You hit your session limit."), (Some("five_hour"), 5));
    assert_eq!(grok_limit_window(BALANCE), (None, 5));
    let lines = grok_limit_notice_lines(PB7_SCREEN);
    assert_eq!(lines.first().map(String::as_str), Some(WEEKLY), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("usage balance exhausted")));
}

