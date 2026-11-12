use super::{composer_holds_prompt, is_our_folded_paste};

/// codex-cli 0.164.0（`codex --version`）實機畫面。派工單寫的是 0.163，本機實際是 0.164.0，以這個為準。
/// 擷取（2026-10-24）：拋棄式 pane（`herdr pane split --direction down`）裡，`mktemp -d` 的目錄下跑 `codex`，
/// 把 [`codex_paste_text`] 那 30 行（1752 字，含結尾換行）用 `herdr pane send-text` 包進 bracketed paste
/// （`ESC[200~` … `ESC[201~`，中間每段 ≤1000 位元組，同 daemon 的 `pane_send_text` 分段）。
/// 沒有 bracketed paste 時 codex 不摺、整段當成打字輸入，所以這個畫面只在真的貼上時出現。
/// 用 `herdr pane read` 讀整個 pane（純文字），欄寬約 209（footer 那列填滿）。沒有按 Enter，擷取完 pane 已關、codex 行程已結束。

pub(crate) const CODEX_FOLDED_PASTE_SCREEN: &str = r#"  /init - create an AGENTS.md file with instructions for Codex
  /status - show current session configuration
  /permissions - choose what Codex is allowed to do
  /model - choose what model and reasoning effort to use
  /review - review any changes and find issues


› [Pasted Content 1752 chars]

  GPT-6.1-Sol default · /tmp/tmp.7VTjHKz1x1
                                                                                                                                                                                         ⚠ 1 warning · f2 to view"#;
/// [`CODEX_FOLDED_PASTE_SCREEN`] 摺起來的那段：`fixture line N: …\n`，N = 1..=30。
pub(crate) fn codex_paste_text() -> String {
    (1..=30).map(|i| format!("fixture line {i}: 這是摺疊貼上的測試第 {i} 行，用來確認 codex 會把長段貼上摺起來，不要送出\n")).collect()
}

#[test]
fn the_fixture_folds_to_the_length_we_sent() {
    assert_eq!(codex_paste_text().chars().count(), 1752);
    assert!(CODEX_FOLDED_PASTE_SCREEN.contains("[Pasted Content 1752 chars]"));
}

#[test]
fn a_codex_fold_of_our_text_is_ours() {
    assert!(composer_holds_prompt("codex", CODEX_FOLDED_PASTE_SCREEN, &codex_paste_text()));
}

#[test]
fn a_codex_fold_of_a_different_length_is_not_ours() {
    let sent = codex_paste_text();
    assert!(!composer_holds_prompt("codex", CODEX_FOLDED_PASTE_SCREEN, sent.trim_end()), "少了結尾換行，標籤的 1752 對不上");
    assert!(!composer_holds_prompt("codex", CODEX_FOLDED_PASTE_SCREEN, &format!("{sent}x")), "多一個字");
    let other = CODEX_FOLDED_PASTE_SCREEN.replace("1752", "999");
    assert!(!composer_holds_prompt("codex", &other, &codex_paste_text()), "別人的摺疊、字數不同");
}

#[test]
fn text_typed_after_or_beside_the_fold_is_not_ours() {
    let typed = CODEX_FOLDED_PASTE_SCREEN.replace("[Pasted Content 1752 chars]", "[Pasted Content 1752 chars] 還要加一句");
    assert!(!composer_holds_prompt("codex", &typed, &codex_paste_text()));
    let two = CODEX_FOLDED_PASTE_SCREEN.replace("› [Pasted Content 1752 chars]", "› [Pasted Content 1752 chars]\n  [Pasted Content 1752 chars]");
    assert!(!composer_holds_prompt("codex", &two, &codex_paste_text()), "框裡有兩個摺疊貼上");
}

/// 多位元組的 prompt：`codex_paste_text()` 含中文，1752 是字元數（`chars().count()`），位元組是 3552。
/// 標籤是字元數，所以 1752 算我們的；寫成位元組數 3552 就對不上，不算。
#[test]
fn a_multibyte_codex_fold_counts_characters_not_bytes() {
    let sent = codex_paste_text();
    assert_eq!((sent.chars().count(), sent.len()), (1752, 3552), "fixture 的字元數與位元組數");
    assert!(composer_holds_prompt("codex", CODEX_FOLDED_PASTE_SCREEN, &sent), "標籤 1752 chars = 字元數");
    let as_bytes = CODEX_FOLDED_PASTE_SCREEN.replace("1752 chars", "3552 chars");
    assert!(!composer_holds_prompt("codex", &as_bytes, &sent), "3552 是位元組數，不是 codex 的數字");
}

/// claude 的 `[Pasted text #N +M lines]` 照舊：M 是換行數（回歸）。
#[test]
fn claude_fold_line_count_still_decides_as_before() {
    assert!(is_our_folded_paste("claude", "[Pasted text #7 +2 lines]", "a\nb\nc"));
    assert!(!is_our_folded_paste("claude", "[Pasted text #7 +3 lines]", "a\nb\nc"));
}

#[test]
fn a_zero_length_fold_is_never_ours() {
    assert!(!is_our_folded_paste("codex", "[Pasted Content 0 chars]", ""));
}

#[test]
fn each_cli_only_recognises_its_own_fold_format() {
    let claude = "[Pasted text #1 +2 lines]";
    let sent = "a\nb\nc";
    assert!(is_our_folded_paste("claude", claude, sent));
    assert!(!is_our_folded_paste("codex", claude, sent), "claude 的標籤在 codex 的框裡不算");
    assert!(!is_our_folded_paste("claude", "[Pasted Content 5 chars]", "abcde"), "codex 的標籤在 claude 的框裡不算");
    assert!(!is_our_folded_paste("claude", "[Pasted text #1 +3 lines]", sent), "行數不符");
    assert!(!is_our_folded_paste("claude", "[Pasted text #1 +2 lines] 補一句", sent));
}
