use super::*;

#[test]
fn codex_title_turn_is_ignored() {
    let p = serde_json::json!({
        "type": "agent-turn-complete", "thread-id": "t", "turn-id": "u",
        "input-messages": ["Generate a concise, single-line task title of at most 36 characters …"],
        "last-assistant-message": "{\"title\":\"Reply with MERGED-OK\"}"
    });
    assert!(matches!(classify("codex", &p), HookKind::Ignore(_)));
    let real = serde_json::json!({
        "type": "agent-turn-complete", "thread-id": "t", "turn-id": "v",
        "input-messages": ["Reply with exactly MERGED-OK"], "last-assistant-message": "MERGED-OK"
    });
    assert!(matches!(classify("codex", &real), HookKind::TurnComplete { .. }));
}

/// 稽核：使用者真的要求「只回一個 title 的 JSON」時，助理回覆剛好就是 `{"title":…}`。以前只看回覆長相就當成標題回合丟掉——
/// 這個真回合的 Stop 永遠不認領，只剩終端備援（`completed_fallback`）收尾。標題回合的 input 是標題指令，不是使用者的話。
#[test]
fn a_real_turn_whose_reply_is_a_title_object_is_not_a_title_turn() {
    let real = serde_json::json!({
        "type": "agent-turn-complete", "thread-id": "t", "turn-id": "v",
        "input-messages": ["把這篇文章的結論用 JSON 回我，只要一個欄位"],
        "last-assistant-message": "{\"title\":\"結論\"}"
    });
    assert!(matches!(classify("codex", &real), HookKind::TurnComplete { .. }), "{:?}", classify("codex", &real));
    // 沒有 input-messages（取不到）時，回覆長相仍是唯一線索：照舊當標題回合。
    let no_input = serde_json::json!({
        "type": "agent-turn-complete", "thread-id": "t", "turn-id": "w",
        "last-assistant-message": "{\"title\":\"x\"}"
    });
    assert!(matches!(classify("codex", &no_input), HookKind::Ignore(_)));
}
