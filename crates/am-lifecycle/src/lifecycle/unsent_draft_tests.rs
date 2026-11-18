//! #1897：送達證不出來、字還留在框裡的回合，idle 收尾之前先補按一次 Enter；沒用才收成 failed＋ops_alert。
//! 全部走真的 `stuck_turns::sweep_at` 與 mock herdr 的 codex pane（`pane.read` 畫出框、`Enter` 會把框裡的字送進對話）。

use super::*;
use crate::lifecycle::stuck_turns::{observe_at, sweep_at};
/// #1984 D：日誌的框形狀只有結構特徵與固定字樣，不含畫面文字，長度有上限。
#[test]
fn the_shape_log_carries_structure_and_never_the_screen_text() {
    let screen = "› [Pasted Content 1913 chars]\n\ngpt-6-astra secret-model\n";
    let shape = box_shape("codex", screen, &"字".repeat(7), crate::composer_parse::BoxState::NonEmpty);
    assert!(shape.contains("folded_chars=1913"), "{shape}");
    assert!(shape.contains("sent_chars=7"), "{shape}");
    assert!(shape.contains("glyph_row=0") && shape.contains("from_bottom=3") && shape.contains("glyph_in_tail=true"), "{shape}");
    assert!(!shape.contains("Pasted") && !shape.contains("secret"), "不得含畫面文字：{shape}");
    let styled = "\u{1b}[36m›\u{1b}[0m dispatch\n\nfooter\n";
    assert!(box_shape("codex", styled, "dispatch", crate::composer_parse::BoxState::NonEmpty).contains("glyph_row=0"));
    let long = "x\n".repeat(5000);
    assert!(box_shape("codex", &long, "", crate::composer_parse::BoxState::Unready).chars().count() <= DIAGNOSE_MAX_CHARS);
}
