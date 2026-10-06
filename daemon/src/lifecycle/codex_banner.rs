//! codex 帳號安全提醒橫幅顯示中時擋住派送、通知人（#782，#779 後續）。
//!
//! 橫幅（`› 1. Set up security`／`Press a number to choose · …`，辨識見 [`super::codex_inline_banner`]）顯示中時，
//! 輸入框看起來是空的、可以打字，但 codex 會把 prompt 開頭的數字當成「選第 N 項」吃掉，殘字留在框裡卡住之後的派送。
//! 使用者裁示（2026-10-03）：**不要**自動按 Esc 或任何鍵關橫幅；送字前看到就整則不送（可重試的 `NotAttempted`），
//! 並在 bot 的對話裡留一則 system 通知請人處理，同時推一則 `ops_alert` 進 AGM inbox 給巡檢（#789）。
//! 同一個 run 的同一次橫幅只講一次：佇列每次重試都會再擋一次。

use super::*;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// 被橫幅擋下時 `NotAttempted` 的 reason（API 回 409 帶這個 reason，佇列照可重試的規則放回去）。
pub(crate) const REASON: &str = "codex_security_banner";

pub(crate) const NOTICE: &str = "codex 畫面上有帳號安全提醒橫幅（`Press a number to choose`）。橫幅開著時打字，開頭的數字會被 codex 當成選項吃掉，\
     所以這則訊息沒有送出（一個字都沒打）；daemon 也不會替你按 Esc 或任何鍵。請到「終端」處理橫幅（選一個選項，或按 Esc 關掉），\
     關掉後排著的訊息會照常重試送出。";

/// 已經通知過、橫幅還沒被看到關掉的 run。
fn open() -> &'static Mutex<HashSet<String>> {
    static OPEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    OPEN.get_or_init(Default::default)
}

/// 畫面上有沒有「按數字選」的 inline banner。`screen` 可以是 `format: ansi` 讀到的：去掉樣式，codex 的點字動畫粒子
/// （落在空白格上）也抹回空白，不然空白列不空、結構對不上就漏掉。只有資訊、沒有選項的 banner（`esc to dismiss ·
/// type to continue`）不吃數字，不擋。
pub(crate) fn blocks_typing(screen: &str) -> bool {
    let plain = blank_codex_particles(screen);
    let lines: Vec<&str> = plain.lines().collect();
    codex_inline_banner(&lines)
        .is_some_and(|r| lines[r].iter().any(|l| l.trim_start().starts_with("Press a number to choose")))
}

/// 送字前看到橫幅：第一次寫一則通知（`message_added` 事件推給網頁），也推一則 inbox 事件給巡檢（[`alert`]）。
/// 之後同一次橫幅不再講；看到橫幅不在了就忘掉，下次再出現會再講一次。寫不進去只記 log：擋住派送本身不受影響。
pub(crate) async fn observe(app: &Arc<App>, run: &db::Run, shown: bool) {
    if !shown {
        open().lock().unwrap().remove(&run.id);
        return;
    }
    if !open().lock().unwrap().insert(run.id.clone()) {
        return;
    }
    tracing::warn!(run = %run.id, bot = %run.bot_id, "codex 帳號安全提醒橫幅擋住派送，等人處理（不自動關）");
    super::poller::app_ports_p4obs::post_codex_security_banner_notice(app, run, NOTICE).await;
    super::poller::app_ports_p4obs::push_codex_security_banner_alert(app, run, REASON).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const BANNER: &str = include_str!("fixtures/codex-0.159.3-security-setup-banner.txt");
    const BANNER_ANSI: &str = include_str!("fixtures/codex-0.159.3-security-setup-banner.ansi");

    #[test]
    fn the_security_banner_blocks_typing_in_plain_and_styled_reads() {
        assert!(blocks_typing(BANNER));
        assert!(blocks_typing(BANNER_ANSI));
        // 動畫粒子落在橫幅周圍的空白列上：抹掉之後照樣認得。
        let sparkled = BANNER_ANSI.replacen("\n\n\u{1b}[1m\u{1b}[36m", "\n\u{1b}[38;2;90;90;200m⠂\u{1b}[0m\n\u{1b}[1m\u{1b}[36m", 1);
        assert_ne!(sparkled, BANNER_ANSI, "要真的放進一顆粒子");
        assert!(blocks_typing(&sparkled));
    }

    #[test]
    fn a_screen_without_a_choice_banner_is_not_blocked() {
        assert!(!blocks_typing("› Reply with PONG\n\n• PONG\n\n› \n\n  gpt-6.1-sol default · /tmp/x\n"));
        // 只有資訊、沒有選項：打字就是繼續，數字不會被吃。
        let info = "• PONG\n\n\n  Heads up\n  Something changed on your account.\n\n  esc to dismiss · type to continue\n\n› Ask Codex to do anything\n";
        assert!(!blocks_typing(info));
        // 回覆裡照抄了橫幅的字（結構不完整）不是橫幅。
        let quoted = "› 貼一下提醒的原文\n\n• 原文如下：\n  Keep using Daybreak mode\n  Press a number to choose · esc to dismiss · type to continue\n\n› Ask Codex to do anything\n";
        assert!(!blocks_typing(quoted));
    }
}
