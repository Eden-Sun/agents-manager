//! 遮蔽 Debug（openai 與 claude 共用）：`{:?}` 的結果不得含 transcript 內容或 OpenAI 加密 reasoning。
//! 哨兵值只放在內容欄位；斷言輸出不含它，也不含 `encrypted_content` 的值。

use super::protocol::{Message, Response};
use super::runner::State;
use serde_json::json;

const SENTINEL: &str = "sk-test-SENTINEL-XYZ-user-text";
const ENC: &str = "ENC-SENTINEL-reasoning-blob-42";

fn assert_redacted(text: &str) {
    assert!(!text.contains(SENTINEL), "使用者文字洩漏：{text}");
    assert!(!text.contains(ENC), "encrypted_content 洩漏：{text}");
    assert!(!text.contains("encrypted_content"), "欄位名也不該出現：{text}");
}

#[test]
fn message_debug_shows_shape_not_content() {
    let m = Message {
        role: "user".into(),
        content: json!([
            {"type": "text", "text": SENTINEL},
            {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": ENC},
        ]),
    };
    let text = format!("{m:?}");
    assert_redacted(&text);
    assert!(text.contains("role: \"user\""), "{text}");
    assert!(text.contains("len=2"), "筆數要看得到：{text}");
    assert!(text.contains("reasoning"), "item 種類要看得到：{text}");
}

#[test]
fn message_debug_string_content_only_shows_length() {
    let m = Message { role: "assistant".into(), content: json!(SENTINEL) };
    let text = format!("{m:?}");
    assert_redacted(&text);
    assert!(text.contains(&format!("len={}", SENTINEL.len())), "{text}");
}

#[test]
fn response_debug_hides_content_and_usage() {
    let r = Response {
        id: "resp_1".into(),
        model: "gpt-6-luna".into(),
        content: vec![json!({"type": "reasoning", "encrypted_content": ENC}), json!({"type": "text", "text": SENTINEL})],
        stop_reason: "end_turn".into(),
        usage: json!({"input_tokens": 3, "secret": SENTINEL}),
    };
    let text = format!("{r:?}");
    assert_redacted(&text);
    assert!(text.contains("gpt-6-luna"), "model 可以印：{text}");
}

#[test]
fn state_debug_counts_messages_and_hides_transcript() {
    let s = State {
        messages: vec![Message { role: "user".into(), content: json!(SENTINEL) }],
        responses: vec![Response {
            id: "r".into(),
            model: "m".into(),
            content: vec![json!({"type": "reasoning", "encrypted_content": ENC})],
            stop_reason: "end_turn".into(),
            usage: json!({}),
        }],
        system: Some(SENTINEL.into()),
        provider: Some("openai".into()),
        ..Default::default()
    };
    let text = format!("{s:?}");
    assert_redacted(&text);
    assert!(text.contains("1 messages"), "{text}");
    assert!(text.contains("1 responses"), "{text}");
    assert!(text.contains("provider: Some(\"openai\")"), "{text}");
}
