//! #581：強制中止之後輸入框「讀不到」、每則都 409 `composer_unreadable`，對話裡一次冒出好幾則一樣的中止說明。
//!
//! 真的經過（2026-09-27 wits-pro，隔離 herdr session＋真 claude 2.1.281 重現）：回合還沒吐字就被 `esc` 打斷，
//! claude 把整段 prompt 放回框裡；框長到二十幾列，框頂的 `❯` 落在倒數 24 列之外。清框那一步判成「框是空的」沒按
//! `ctrl+c`，送出前的檢查也找不到框——字明明在框裡，卻回 `composer_unreadable`，網頁連清草稿的動作都拿不到。
//! 草稿一律用中性假字；真畫面是 `fixtures/claude-2.1.281-tall-draft-after-interrupt.ansi`。

use super::*;
const TALL_DRAFT: &str = include_str!("fixtures/claude-2.1.281-tall-draft-after-interrupt.ansi");
/// 放回框裡的那段有幾列：比 [`COMPOSER_TAIL`] 高，框頂的 `❯` 才會落在預設的範圍之外。
const DRAFT_ROWS: usize = 30;
fn tall_draft() -> Vec<String> {
    (1..=DRAFT_ROWS).map(|n| format!("fixture restored row {n}")).collect()
}
/// 真畫面：框頂的 `❯` 在倒數 24 列之外，框裡的字照樣讀得出來、判成有草稿（不是讀不到）。
#[test]
fn a_real_tall_restored_draft_reads_as_a_draft() {
    let lines: Vec<&str> = TALL_DRAFT.lines().collect();
    let marker = lines.iter().rposition(|l| delivery::strip_ansi(l).starts_with('❯')).unwrap();
    assert!(lines.len() - marker > COMPOSER_TAIL, "fixture 的框頂要真的在預設範圍之外（不然測不到什麼）");
    assert_eq!(box_state("claude", TALL_DRAFT), BoxState::NonEmpty);
    let text = composer_text("claude", TALL_DRAFT).unwrap();
    assert!(text.starts_with("fix\n這是一段合成的多行貼上"), "{text}");
}
/// 框頂那列不是 `❯`（例如對話框、或框裡有一整列 `─` 把框切斷）：不猜，照舊判讀不到。
#[test]
fn a_tall_frame_without_the_prompt_mark_on_top_stays_unreadable() {
    let lines: Vec<&str> = TALL_DRAFT.lines().collect();
    let marker = lines.iter().rposition(|l| delivery::strip_ansi(l).starts_with('❯')).unwrap();
    let mut cut: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    cut[marker] = cut[marker].replacen('❯', " ", 1);
    let cut = cut.join("\n");
    assert_eq!(box_state("claude", &cut), BoxState::Unready);
    assert_eq!(composer_text("claude", &cut), None);
}
#[test]
fn restored_prompt_match_keeps_blank_rows_from_the_composer() {
    let mut lines: Vec<String> = TALL_DRAFT.lines().map(str::to_string).collect();
    let marker = lines.iter().rposition(|line| delivery::strip_ansi(line).starts_with('❯')).unwrap();
    lines.insert(marker + 1, String::new());
    let screen = lines.join("\n");
    let expected = composer_text_whole("claude", TALL_DRAFT).unwrap().replacen("fix\n", "fix\n\n", 1);

    assert_eq!(composer_text_whole("claude", &screen).as_deref(), Some(expected.as_str()));
    assert!(restored_prompt_matches("claude", &screen, &expected), "the full restored prompt must match, including its blank row");
    let edited = screen.replace("另一行補充說明", "使用者修改過的補充說明");
    assert!(!restored_prompt_matches("claude", &edited, &expected), "an edited paragraph must not inherit the first paragraph's match");
}
#[test]
fn restored_prompt_identity_preserves_word_boundaries_and_only_allows_known_tab_loss() {
    assert!(same_restored_prompt("echo a b", "echo a b"));
    assert!(!same_restored_prompt("echo\n  a b", "echo a b"), "hard line breaks remain part of the prompt");
    assert!(!same_restored_prompt("echo ab", "echo a b"), "ordinary spaces separate tokens");
    assert!(same_restored_prompt("進貨單每張", "進貨單\t每張"), "the Claude TUI is known to drop a literal Tab");
    assert!(!same_restored_prompt("進貨單每張", "進貨單 每張"), "ordinary spaces are not dropped");
}