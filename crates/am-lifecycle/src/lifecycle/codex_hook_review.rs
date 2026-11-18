//! codex 0.169+ 啟動時的 hook 審核框（上游 `startup_hooks_review.rs`，rust-v0.169.1）。
//!
//! 有未信任（或內容改過）的 hook 時，codex TUI 會停在「Hooks need review」選單等鍵盤。選項 2 會信任**所有**待審 hook，
//! 所以不能自動按任何鍵（Enter 確認、Esc 略過、1／2／3 都不碰）。這裡只辨識畫面、擋住送字，並通知人。
//! 辨識要求標題、三個選項、結尾的操作列依序出現（同一個選單的結構），不靠單一字句，所以對話裡引用的同一句話不會誤判。

/// 被審核框擋下時 `NotAttempted` 的 reason（佇列照可重試的規則放回去，不送字）。
pub const REASON: &str = "codex_hook_review_pending";

/// 已經通知過、審核框還沒被看到關掉的 run（同一次審核框只講一次，佇列每次重試都會再擋一次）。
fn notified() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static NOTIFIED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    NOTIFIED.get_or_init(Default::default)
}

pub const NOTICE: &str = "codex 停在「Hooks need review」審核框（有新的或改過的 hook 還沒信任）。框開著時送出的字會被框吃掉，\
     所以這則訊息沒有送出（一個字都沒打）；daemon 也不會替你按任何鍵（包括「Trust all」）。請到「終端」處理審核框，\
     處理完後排著的訊息會照常重試送出。";

/// 審核框擋住派送時呼叫：第一次看到寫一則通知；框不在了就忘掉，下次出現再講一次。
pub async fn observe(app: &impl crate::lifecycle::s6_ports::ScreenServices, run: &crate::db::Run, shown: bool) {
    if !shown {
        notified().lock().unwrap().remove(&run.id);
        return;
    }
    if !notified().lock().unwrap().insert(run.id.clone()) {
        return;
    }
    tracing::warn!(run = %run.id, bot = %run.bot_id, "codex hook review screen holds delivery; waiting for a human (never auto-accepted)");
    app.post_codex_hook_review_notice(run, NOTICE).await;
}

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
        one < two && two < three && footer.is_some_and(|f| f > three && f - three <= 4)
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

    #[test]
    fn normal_composer_is_not_open() {
        assert!(!review_open("• Done.\n\n› \n  gpt-5 · ~/project\n"));
    }
}
