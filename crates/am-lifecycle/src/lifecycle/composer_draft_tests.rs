//! 輸入框卡著草稿（409 `composer_busy`）的兩個動作：送出框裡那段、清掉再送我這則。
//! 草稿一律用中性假字；真畫面來自 2026-09-26 隔離 herdr session 的實抓（`fixtures/*-draft.ansi`／`*-cleared.ansi`）。

use super::*;

const CLAUDE_DRAFT: &str = include_str!("fixtures/claude-2.1.281-draft.ansi");
const CLAUDE_CLEARED: &str = include_str!("fixtures/claude-2.1.281-draft-cleared.ansi");
const CLAUDE_MULTI: &str = include_str!("fixtures/claude-2.1.281-multiline-draft.ansi");
const CLAUDE_MULTI_CLEARED: &str = include_str!("fixtures/claude-2.1.281-multiline-cleared.ansi");
const CLAUDE_LONG: &str = include_str!("fixtures/claude-2.1.281-folded-draft.ansi");
const CODEX_DRAFT: &str = include_str!("fixtures/codex-0.155-draft.ansi");
const CODEX_CLEARED: &str = include_str!("fixtures/codex-0.155-draft-cleared.ansi");
const CODEX_MULTI: &str = include_str!("fixtures/codex-0.155-multiline-draft.ansi");
const CODEX_MULTI_CLEARED: &str = include_str!("fixtures/codex-0.155-multiline-cleared.ansi");
const GROK_DRAFT: &str = include_str!("fixtures/grok-1.0.41-draft.ansi");
const GROK_CLEARED: &str = include_str!("fixtures/grok-1.0.41-draft-cleared.ansi");
const GROK_MULTI: &str = include_str!("fixtures/grok-1.0.41-multiline-draft.ansi");
const GROK_MULTI_CLEARED: &str = include_str!("fixtures/grok-1.0.41-multiline-cleared.ansi");
const ONE_LINE: &str = "fixture draft alpha beta gamma";
const THREE_LINES: &str = "fixture line one\nfixture line two\nfixture line three";
/// 三種 kind、單行與多行：框裡的字讀得出來（就是 409 回的 `draft`），`ctrl+c` 之後重讀是空框（清框成功的判準）。
#[test]
fn every_kind_reads_its_real_draft_and_its_real_cleared_box() {
    for (kind, draft, cleared, text) in [
        ("claude", CLAUDE_DRAFT, CLAUDE_CLEARED, ONE_LINE),
        ("claude", CLAUDE_MULTI, CLAUDE_MULTI_CLEARED, THREE_LINES),
        ("codex", CODEX_DRAFT, CODEX_CLEARED, ONE_LINE),
        ("codex", CODEX_MULTI, CODEX_MULTI_CLEARED, THREE_LINES),
        ("grok", GROK_DRAFT, GROK_CLEARED, ONE_LINE),
        ("grok", GROK_MULTI, GROK_MULTI_CLEARED, THREE_LINES),
    ] {
        assert_eq!(box_state(kind, draft), BoxState::NonEmpty, "{kind}: {text:?}");
        assert_eq!(composer_text(kind, draft).as_deref(), Some(text), "{kind}");
        assert_eq!(box_state(kind, cleared), BoxState::Empty, "{kind}: cleared");
        assert_eq!(composer_text(kind, cleared), None, "{kind}: cleared");
    }
}
/// claude 2.1.281 十四列的長段貼上照樣整段畫在框裡：讀得到頭尾，清掉之後跟單行一樣是空框（同一張 cleared）。
#[test]
fn a_long_claude_draft_reads_whole() {
    assert_eq!(box_state("claude", CLAUDE_LONG), BoxState::NonEmpty);
    let text = composer_text("claude", CLAUDE_LONG).unwrap();
    assert!(text.starts_with("fixture folded row 1\n") && text.ends_with("fixture folded row 14"), "{text}");
}
/// 草稿兩段之間空一行：給人看、算 token、被清掉的要是同一整段，不是讀到第一個空白列就停（使用者同意清的只是前半段）。
#[test]
fn a_claude_draft_with_a_blank_row_reads_whole() {
    let screen = CLAUDE_MULTI.replace("fixture line one\r\n", "fixture line one\r\n\r\n");
    assert_ne!(screen, CLAUDE_MULTI, "fixture 換行的寫法變了，這個測試要跟著改");
    assert_eq!(composer_text("claude", &screen).as_deref(), Some("fixture line one"), "舊讀法：停在空白列");
    assert_eq!(composer_text_whole("claude", &screen).as_deref(), Some("fixture line one\n\nfixture line two\nfixture line three"));
    assert_eq!(composer_text_whole("claude", CLAUDE_MULTI).as_deref(), Some(THREE_LINES));
    for (kind, draft) in [("codex", CODEX_MULTI), ("grok", GROK_MULTI)] {
        assert_eq!(composer_text_whole(kind, draft), composer_text(kind, draft), "{kind}");
    }
}
#[test]
fn only_verified_kinds_get_the_actions() {
    for kind in ["claude", "codex", "grok"] {
        assert_eq!(actions(kind), ["submit", "clear"], "{kind}");
    }
    assert!(actions("shell").is_empty());
}
#[test]
fn a_long_draft_is_cut_for_display_but_identified_by_full_text_and_run_pane() {
    let long = "字".repeat(DRAFT_SHOWN_CHARS + 20);
    let (text, cut) = shown(&long);
    assert_eq!(text.chars().count(), DRAFT_SHOWN_CHARS);
    assert!(cut);
    let token = draft_token("run-1", "pane-1", &long);
    assert_eq!(token.len(), 64);
    assert!(same_draft(&token, "run-1", "pane-1", &long));
    assert!(!same_draft(&token, "run-1", "pane-1", &format!("{long}changed suffix")));
    assert!(!same_draft(&token, "run-2", "pane-1", &long));
    assert!(!same_draft(&token, "run-1", "pane-2", &long));
    assert_eq!(shown("短短一句"), ("短短一句".into(), false));
}
#[test]
fn a_full_draft_token_changes_when_only_the_unshown_suffix_changes() {
    let prefix = "a".repeat(DRAFT_SHOWN_CHARS);
    let one = draft_token("run-1", "pane-1", &format!("{prefix}suffix one"));
    let two = draft_token("run-1", "pane-1", &format!("{prefix}suffix two"));
    assert_ne!(one, two);
}
#[test]
fn nr1913_snapshot_checks_content_and_metadata_even_with_zero_revision() {
    let before = crate::herdr::PaneRead { pane_id: "pane-a".into(), source: "visible".into(), format: "ansi".into(), text: "────\n❯ draft\n────".into(), revision: 0, truncated: false };
    assert!(same_snapshot("claude", &before, &before));
    for field in ["pane", "source", "format", "text"] {
        let mut after = before.clone();
        match field {
            "pane" => after.pane_id = "other".into(),
            "source" => after.source = "recent".into(),
            "format" => after.format = "text".into(),
            _ => after.text = "changed".into(),
        }
        assert!(!same_snapshot("claude", &before, &after), "{field}");
    }
    let long = crate::herdr::PaneRead { truncated: true, ..before.clone() };
    assert!(same_snapshot("claude", &long, &long));
    assert!(same_snapshot("claude", &before, &long));
}
#[test]
fn nr1936_composer_frame_ignores_background_but_rejects_draft_geometry_and_metadata_changes() {
    for (kind, text) in [("claude", CLAUDE_MULTI), ("codex", CODEX_MULTI), ("grok", GROK_MULTI), ("agy", "────\n> draft\n────")] {
        let real = crate::herdr::PaneRead { pane_id: "pane".into(), source: "visible".into(), format: "ansi".into(), text: text.into(), revision: 0, truncated: false };
        assert!(same_snapshot(kind, &real, &real), "{kind}: real multiline frame must be recognised");
    }
    let before = crate::herdr::PaneRead { pane_id: "pane".into(), source: "visible".into(), format: "ansi".into(),
        text: "✻ Crunching… (1s · esc to interrupt)\n────\n❯ draft\n────\n u | proj | model | 5h:90%(rst 2h 3m) | 7d:90%".into(), revision: 0, truncated: false };
    let mut after = before.clone();
    after.text = after.text.replace('✻', "✽").replace("(1s ·", "(2s ·").replace("2h 3m", "2h 2m");
    after.revision = 99;
    assert!(same_snapshot("claude", &before, &after));
    for text in [after.text.replace("draft", "other draft"), after.text.replace("────", "─────"), format!("new row\n{}", after.text)] {
        let mut changed = after.clone();
        changed.text = text;
        assert!(!same_snapshot("claude", &before, &changed));
    }
    let mut clipped = after.clone();
    clipped.text = "❯ draft".into();
    assert!(!same_snapshot("claude", &before, &clipped));
}
