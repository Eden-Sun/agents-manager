//! codex 0.169+ 啟動時的 hook 審核框（上游 `startup_hooks_review.rs`，rust-v0.169.1）。
//!
//! 有未信任（或內容改過）的 hook 時，codex TUI 會停在「Hooks need review」選單等鍵盤。選項 2 會信任**所有**待審 hook，
//! 所以不能自動按任何鍵（Enter 確認、Esc 略過、1／2／3 都不碰）。這裡只辨識畫面、擋住送字；標 blocked、聊天室提示、通知上層、框關掉還原都走 `codex_model_migration::Dialog::HookReview`。
//! 辨識要求標題、三個選項、結尾的操作列依序出現（同一個選單的結構），不靠單一字句，所以對話裡引用的同一句話不會誤判。

/// 被審核框擋下時 `NotAttempted` 的 reason（佇列照可重試的規則放回去，不送字）。
pub const REASON: &str = "codex_hook_review_pending";

/// 畫面上的審核框是否開著（已去 ANSI）。
pub fn review_open(screen: &str) -> bool {
    let lines: Vec<String> = screen
        .lines()
        .map(|l| crate::composer_parse::strip_ansi(l).trim().trim_start_matches('›').trim().to_string())
        .collect();
    lines.iter().enumerate().any(|(heading, line)| {
        if line != "Hooks need review" {
            return false;
        }
        // 標題後面的一小段：選項 1、2、3 依序出現，接著是操作列（`enter confirm · esc skip`）。
        let window = &lines[heading + 1..lines.len().min(heading + 12)];
        let at = |pred: &dyn Fn(&str) -> bool| window.iter().position(|l| pred(l));
        let (Some(one), Some(two), Some(three)) = (
            at(&|l| l.starts_with("1. Review hooks")),
            at(&|l| l == "2. Trust all and continue"),
            at(&|l| l.starts_with("3. Continue without trusting")),
        ) else {
            return false;
        };
        let footer = at(&|l| l.contains("enter confirm") && l.contains("esc skip"));
        // 審核框開著時，操作列就是畫面最後一行（框底下不會有 composer 或狀態列）。對話裡印出同一段文字
        // （例如 agent 自己 cat 了這個 fixture）時底下還有輸入框，不算開著，才不會把正常畫面一直當成審核框擋住。
        let nothing_below = |f: usize| lines[heading + 1 + f + 1..].iter().all(|l| l.is_empty());
        one < two && two < three && footer.is_some_and(|f| f > three && f - three <= 4 && nothing_below(f))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &str = include_str!("fixtures/codex-0.169.1-hooks-review.txt");
    const ANSI: &str = include_str!("fixtures/codex-0.169.1-hooks-review.ansi");

    #[test]
    fn upstream_snapshot_is_open() {
        assert!(review_open(PLAIN));
    }

    #[test]
    fn colored_screen_with_sgr_and_resets_is_open() {
        assert!(review_open(ANSI));
    }

    #[test]
    fn heading_quoted_in_a_transcript_is_not_open() {
        let screen = "• 我剛才看到 Hooks need review 這個字樣，先不管它。\n\n› \n  gpt-5 · ~/project\n";
        assert!(!review_open(screen));
    }

    #[test]
    fn options_without_the_footer_are_not_open() {
        let screen: String = PLAIN.lines().filter(|l| !l.contains("enter confirm")).collect::<Vec<_>>().join("\n");
        assert!(!review_open(&screen));
    }

    /// 對話裡印出整個審核框（例如 agent cat 了 fixture、或貼了截圖文字），底下是正常的輸入框：不是開著的審核框。
    #[test]
    fn the_whole_box_quoted_above_a_live_composer_is_not_open() {
        let screen = format!("{PLAIN}\n\n› Ask Codex to do anything\n\n  gpt-5 · ~/project\n");
        assert!(!review_open(&screen));
        let ansi = format!("{ANSI}\n\n› \n  gpt-5 · ~/project\n");
        assert!(!review_open(&ansi));
    }

    /// 真的審核框底下只有空白列（pane 比框高時）：仍然算開著。
    #[test]
    fn trailing_blank_rows_keep_it_open() {
        assert!(review_open(&format!("{PLAIN}\n\n   \n\n")));
    }

    #[test]
    fn normal_composer_is_not_open() {
        assert!(!review_open("• Done.\n\n› \n  gpt-5 · ~/project\n"));
    }
}
