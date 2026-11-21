//! 本機 fake SSE：不碰網路、不讀 key、不接任何 production 入口。
use super::codec::*;
use super::pricing::*;
use super::sse::*;
use serde_json::{json, Value};

const ENC_1: &str = "ENC-SECRET-reasoning-1";

fn sse(events: &[Value]) -> Vec<u8> {
    let mut out = String::new();
    for e in events {
        let kind = e["type"].as_str().unwrap();
        out.push_str(&format!("event: {kind}\ndata: {e}\n\n"));
    }
    out.into_bytes()
}

/// 把位元組切成固定大小的塊餵進去（含切在 UTF-8 字元與換行中間）。
fn run(bytes: &[u8], chunk: usize) -> Result<(Outcome, Vec<Update>), CodecError> {
    let mut asm = ResponseAssembler::new();
    let mut updates = Vec::new();
    for c in bytes.chunks(chunk.max(1)) {
        updates.extend(asm.feed(c)?);
    }
    Ok((asm.finish()?, updates))
}

fn usage(input: u64, cached: u64, write: u64, output: u64, reasoning: u64) -> Value {
    json!({"input_tokens": input, "input_tokens_details": {"cached_tokens": cached, "cache_write_tokens": write},
           "output_tokens": output, "output_tokens_details": {"reasoning_tokens": reasoning}, "total_tokens": input + output})
}

fn reasoning_item() -> Value {
    json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": ENC_1})
}
fn message_item(text: &str) -> Value {
    json!({"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]})
}
fn call_item(id: &str, call_id: &str, name: &str, args: &str) -> Value {
    json!({"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": args, "status": "completed"})
}

/// 第一回合：reasoning ＋ 一段文字 ＋ 同一 response 兩個 function_call（參數交錯分片）。
fn two_tool_stream() -> Vec<u8> {
    let out = vec![
        reasoning_item(),
        message_item("我先讀兩個檔案"),
        call_item("fc_a", "call_A", "read_file", r#"{"path":"a.txt"}"#),
        call_item("fc_b", "call_B", "read_file", r#"{"path":"b.txt"}"#),
    ];
    let completed = json!({"id": "resp_1", "status": "completed", "output": out, "usage": usage(1200, 1000, 0, 90, 60)});
    let events = vec![
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item()}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 1, "delta": "我先讀"}),
        json!({"type": "response.output_text.delta", "output_index": 1, "delta": "兩個檔案"}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": message_item("我先讀兩個檔案")}),
        json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "id": "fc_a", "call_id": "call_A", "name": "read_file", "arguments": ""}}),
        json!({"type": "response.output_item.added", "output_index": 3, "item": {"type": "function_call", "id": "fc_b", "call_id": "call_B", "name": "read_file", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"path\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 3, "delta": "{\"path\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "\"a.txt\"}"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 3, "delta": "\"b.txt\"}"}),
        json!({"type": "response.function_call_arguments.done", "output_index": 2, "arguments": r#"{"path":"a.txt"}"#}),
        json!({"type": "response.function_call_arguments.done", "output_index": 3, "arguments": r#"{"path":"b.txt"}"#}),
        json!({"type": "response.output_item.done", "output_index": 2, "item": call_item("fc_a", "call_A", "read_file", r#"{"path":"a.txt"}"#)}),
        json!({"type": "response.output_item.done", "output_index": 3, "item": call_item("fc_b", "call_B", "read_file", r#"{"path":"b.txt"}"#)}),
        json!({"type": "response.completed", "response": completed}),
    ];
    sse(&events)
}

fn final_text_stream(text: &str) -> Vec<u8> {
    let msg = message_item(text);
    sse(&[
        json!({"type": "response.created", "response": {"id": "resp_2"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": text}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": msg.clone()}),
        json!({"type": "response.completed", "response": {"id": "resp_2", "status": "completed", "output": [msg], "usage": usage(1500, 1200, 100, 30, 0)}}),
    ])
}

fn completed(outcome: Outcome) -> ModelOutput {
    match outcome {
        Outcome::Completed(o) => o,
        other => panic!("expected Completed, got {other:?}"),
    }
}

fn luna_request(input: Vec<InputItem>) -> Request {
    Request {
        model: "gpt-6-luna".into(),
        instructions: "你是顧問".into(),
        input,
        tools: vec![ToolSpec { name: "read_file".into(), description: "read".into(), parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}) }],
        effort: None,
        max_output_tokens: 4096,
    }
}

#[test]
fn ask_two_tools_then_final_text_with_interleaved_argument_fragments() {
    let bytes = two_tool_stream();
    let (ref_outcome, ref_updates) = run(&bytes, bytes.len()).unwrap();
    for chunk in [1, 3, 7, 64] {
        let (o, u) = run(&bytes, chunk).unwrap();
        assert_eq!(o, ref_outcome, "chunk={chunk}");
        assert_eq!(u, ref_updates, "chunk={chunk}");
    }
    let out = completed(ref_outcome);
    let texts: Vec<Update> = ref_updates.iter().filter(|u| matches!(u, Update::TextDelta(_))).cloned().collect();
    assert_eq!(texts, vec![Update::TextDelta("我先讀".into()), Update::TextDelta("兩個檔案".into())], "暫態 UI 的文字 delta");
    assert_eq!(out.text, "我先讀兩個檔案");
    assert_eq!(out.stop, StopKind::ToolUse);
    assert_eq!(out.response_id.as_deref(), Some("resp_1"));
    let calls: Vec<_> = out.tool_calls.iter().map(|c| (c.call_id.as_str(), c.name.as_str(), c.arguments.clone())).collect();
    assert_eq!(calls, vec![("call_A", "read_file", json!({"path": "a.txt"})), ("call_B", "read_file", json!({"path": "b.txt"}))]);
    assert_eq!(out.replay.len(), 4, "reasoning、message、兩個 function_call 全部有序保存");
    let u = out.usage.clone().unwrap();
    assert_eq!((u.input_tokens, u.cached_tokens, u.output_tokens, u.reasoning_tokens), (1200, 1000, 90, 60));

    let mut input = vec![InputItem::User("請比較 a.txt 與 b.txt".into())];
    input.extend(out.replay.iter().cloned().map(InputItem::Replay));
    input.push(InputItem::ToolOutput { call_id: "call_A".into(), output: "內容 A".into() });
    input.push(InputItem::ToolOutput { call_id: "call_B".into(), output: "內容 B".into() });
    let body: Value = serde_json::from_slice(&luna_request(input).body().unwrap()).unwrap();
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["reasoning"]["effort"], json!("medium"));
    assert_eq!(body["max_output_tokens"], json!(4096));
    let items = body["input"].as_array().unwrap();
    assert_eq!(items.len(), 7);
    assert_eq!(items[0]["role"], json!("user"));
    assert_eq!(items[1], reasoning_item(), "reasoning item 連 encrypted_content 都原樣重播");
    assert_eq!(items[2], message_item("我先讀兩個檔案"), "assistant item 原欄位（id／status／annotations）不丟");
    assert_eq!(items[3]["call_id"], json!("call_A"));
    assert_eq!(items[4]["call_id"], json!("call_B"));
    assert_eq!(items[5], json!({"type": "function_call_output", "call_id": "call_A", "output": "內容 A"}));
    assert_eq!(items[6], json!({"type": "function_call_output", "call_id": "call_B", "output": "內容 B"}));
    assert_eq!(body["tools"][0]["type"], json!("function"));

    let (o2, _) = run(&final_text_stream("A 與 B 相同"), 5).unwrap();
    let last = completed(o2);
    assert_eq!((last.stop, last.text.as_str(), last.tool_calls.len()), (StopKind::EndTurn, "A 與 B 相同", 0));
}

#[test]
fn the_same_request_always_encodes_to_the_same_bytes() {
    let r = luna_request(vec![InputItem::User("hi".into())]);
    assert_eq!(r.body().unwrap(), r.body().unwrap(), "報價、hash、送出用同一份位元組");
}

#[test]
fn encrypted_reasoning_never_shows_up_in_debug_output() {
    let out = completed(run(&two_tool_stream(), 16).unwrap().0);
    for text in [format!("{out:?}"), format!("{:?}", out.replay), format!("{:?}", InputItem::Replay(out.replay[0].clone()))] {
        assert!(!text.contains(ENC_1), "encrypted_content 洩漏：{text}");
    }
}

fn replay_call(call_id: &str) -> InputItem {
    InputItem::Replay(ReplayItem::new(call_item(&format!("fc_{call_id}"), call_id, "read_file", "{}")).unwrap())
}

#[test]
fn tool_results_pair_by_call_id_and_unpaired_or_duplicate_ones_are_refused() {
    let ok = luna_request(vec![InputItem::User("q".into()), replay_call("c1"), InputItem::ToolOutput { call_id: "c1".into(), output: "x".into() }]);
    assert!(ok.body().is_ok());
    let missing = luna_request(vec![InputItem::User("q".into()), replay_call("c1")]);
    assert_eq!(missing.body(), Err(CodecError::UnpairedToolCall("c1".into())));
    let orphan = luna_request(vec![InputItem::User("q".into()), replay_call("c1"), InputItem::ToolOutput { call_id: "fc_c1".into(), output: "x".into() }]);
    assert_eq!(orphan.body(), Err(CodecError::UnpairedToolCall("fc_c1".into())));
    let twice = luna_request(vec![replay_call("c1"), InputItem::ToolOutput { call_id: "c1".into(), output: "x".into() }, InputItem::ToolOutput { call_id: "c1".into(), output: "y".into() }]);
    assert_eq!(twice.body(), Err(CodecError::UnpairedToolCall("c1".into())));
    let dup = luna_request(vec![replay_call("c1"), replay_call("c1")]);
    assert_eq!(dup.body(), Err(CodecError::DuplicateCallId("c1".into())));
}

#[test]
fn replay_items_must_be_kinds_we_saved() {
    assert!(ReplayItem::new(json!({"type": "web_search_call", "id": "x"})).is_err());
    assert!(ReplayItem::new(json!("not an object")).is_err());
}

#[test]
fn reasoning_effort_is_validated_per_model_with_medium_as_default() {
    assert_eq!(crate::config::OPENAI_USE_EFFORTS, ["none", "low", "medium", "high", "xhigh", "max"]);
    assert_eq!(resolve_effort("gpt-6-luna", None), Ok("medium"));
    for e in crate::config::OPENAI_USE_EFFORTS.iter().copied() {
        assert_eq!(resolve_effort("gpt-6-luna", Some(e)), Ok(e));
    }
    assert_eq!(resolve_effort("gpt-6-luna", Some("minimal")), Err(CodecError::UnsupportedEffort { model: "gpt-6-luna".into(), effort: "minimal".into() }));
    assert_eq!(resolve_effort("gpt-9", Some("low")), Err(CodecError::UnsupportedModel("gpt-9".into())));
    let mut r = luna_request(vec![InputItem::User("q".into())]);
    r.effort = Some("xhigh".into());
    let body: Value = serde_json::from_slice(&r.body().unwrap()).unwrap();
    assert_eq!(body["reasoning"]["effort"], json!("xhigh"));
    r.effort = Some("turbo".into());
    assert!(r.body().is_err(), "不在清單的 effort 在 HTTP 前拒絕");
}

#[test]
fn a_duplicate_call_id_in_one_response_is_a_protocol_error_and_nothing_runs() {
    let out = vec![call_item("fc_1", "dup", "read_file", "{}"), call_item("fc_2", "dup", "read_file", "{}")];
    let bytes = sse(&[json!({"type": "response.completed", "response": {"id": "r", "status": "completed", "output": out, "usage": usage(10, 0, 0, 5, 0)}})]);
    assert_eq!(run(&bytes, 9).map(|_| ()), Err(CodecError::DuplicateCallId("dup".into())));
}

#[test]
fn a_refusal_is_its_own_stop_kind_and_not_assistant_text() {
    let msg = json!({"type": "message", "id": "m", "role": "assistant", "content": [{"type": "refusal", "refusal": "我不能協助這個"}]});
    let bytes = sse(&[json!({"type": "response.completed", "response": {"id": "r", "status": "completed", "output": [msg], "usage": usage(10, 0, 0, 5, 0)}})]);
    let out = completed(run(&bytes, 11).unwrap().0);
    assert_eq!(out.stop, StopKind::Refusal);
    assert_eq!(out.refusal.as_deref(), Some("我不能協助這個"));
    assert_eq!(out.text, "");
    assert!(out.tool_calls.is_empty());
}

#[test]
fn max_output_tokens_is_a_recognizable_incomplete_and_half_tool_arguments_never_execute() {
    let bytes = sse(&[
        json!({"type": "response.created", "response": {"id": "r"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "id": "fc", "call_id": "c1", "name": "read_file", "arguments": "", "status": "in_progress"}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"path\":\"a."}),
        json!({"type": "response.incomplete", "response": {"id": "r", "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "function_call", "id": "fc", "call_id": "c1", "name": "read_file", "arguments": "{\"path\":\"a.", "status": "incomplete"}],
            "usage": usage(100, 0, 0, 4096, 4000)}}),
    ]);
    let (outcome, _) = run(&bytes, 13).unwrap_or_else(|e| panic!("{e:?}"));
    match outcome {
        Outcome::Incomplete { reason, partial_tool_call, output } => {
            assert_eq!(reason, IncompleteReason::MaxOutputTokens);
            assert!(partial_tool_call);
            assert!(output.tool_calls.is_empty(), "半截工具參數永不執行");
            assert!(output.replay.is_empty(), "沒有配對輸出的半截呼叫不進 replay");
            assert_eq!(output.usage.clone().unwrap().output_tokens, 4096, "有可信 usage 仍結算");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn incomplete_for_content_filter_is_distinguished_from_max_tokens() {
    let msg = message_item("被截斷");
    let bytes = sse(&[json!({"type": "response.incomplete", "response": {"id": "r", "status": "incomplete", "incomplete_details": {"reason": "content_filter"}, "output": [msg], "usage": usage(10, 0, 0, 3, 0)}})]);
    match run(&bytes, 8).unwrap().0 {
        Outcome::Incomplete { reason, partial_tool_call, output } => {
            assert_eq!(reason, IncompleteReason::ContentFilter);
            assert!(!partial_tool_call);
            assert_eq!(output.text, "被截斷");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn failed_and_error_events_carry_a_classified_reason() {
    let failed = sse(&[json!({"type": "response.failed", "response": {"id": "r", "status": "failed", "error": {"code": "insufficient_quota", "message": "You exceeded your current quota"}}})]);
    match run(&failed, 17).unwrap().0 {
        Outcome::Failed { code, class, .. } => assert_eq!((code.as_str(), class), ("insufficient_quota", ErrorClass::InsufficientQuota)),
        other => panic!("{other:?}"),
    }
    let error = sse(&[json!({"type": "error", "code": "server_error", "message": "boom"})]);
    match run(&error, 5).unwrap().0 {
        Outcome::Failed { class, .. } => assert!(class.outcome_unknown()),
        other => panic!("{other:?}"),
    }
    let refused = sse(&[json!({"type": "response.failed", "response": {"error": {"code": "invalid_prompt", "message": "flagged"}}})]);
    match run(&refused, 99).unwrap().0 {
        Outcome::Failed { class, .. } => assert_eq!(class, ErrorClass::ModelRejected { code: "invalid_prompt".into() }),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_stream_without_a_terminal_event_is_truncated_even_in_the_middle_of_an_event() {
    let bytes = two_tool_stream();
    let needle = b"event: response.completed";
    let cut = bytes.windows(needle.len()).position(|w| w == needle).unwrap();
    assert_eq!(run(&bytes[..cut], 10).unwrap().0, Outcome::Truncated);
    assert_eq!(run(&bytes[..bytes.len() - 20], 10).unwrap().0, Outcome::Truncated);
    assert_eq!(run(b"", 1).unwrap().0, Outcome::Truncated);
    assert!(matches!(run(&bytes, 10).unwrap().0, Outcome::Completed(_)));
}

#[test]
fn a_completed_response_without_trustworthy_usage_is_refused_not_priced_as_unknown() {
    let msg = message_item("好");
    let stream = |usage: Option<Value>| {
        let mut resp = json!({"id": "r", "status": "completed", "output": [msg.clone()]});
        if let Some(usage) = usage {
            resp["usage"] = usage;
        }
        sse(&[json!({"type": "response.completed", "response": resp})])
    };
    assert!(matches!(run(&stream(Some(usage(10, 0, 0, 5, 0))), 6).unwrap().0, Outcome::Completed(_)));
    for bad in [None, Some(Value::Null), Some(usage(100, 80, 40, 10, 0)), Some(json!({"input_tokens": 10}))] {
        for chunk in [1, 6, 4096] {
            assert_eq!(run(&stream(bad.clone()), chunk).map(|_| ()), Err(CodecError::MissingUsage), "{bad:?}");
        }
    }
    let text = String::from_utf8(two_tool_stream()).unwrap();
    let at = text.rfind("data: ").unwrap();
    let mut last: Value = serde_json::from_str(text[at + 6..].trim_end()).unwrap();
    assert!(last["response"].as_object_mut().unwrap().remove("usage").is_some());
    let stripped = format!("{}data: {last}\n\n", &text[..at]);
    assert_eq!(run(stripped.as_bytes(), 7).map(|_| ()), Err(CodecError::MissingUsage));
    assert_eq!(Usage::from_response(Some(&Value::Null)), Err(UsageError::Missing));
    let incomplete = sse(&[json!({"type": "response.incomplete", "response": {"id": "r", "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": []}})]);
    match run(&incomplete, 8).unwrap().0 {
        Outcome::Incomplete { output, .. } => assert_eq!(output.usage, Err(UsageError::Missing)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn completed_must_agree_with_what_was_streamed() {
    let text = String::from_utf8(two_tool_stream()).unwrap();
    let needle = r#""arguments":"{\"path\":\"b.txt\"}","call_id":"call_B""#;
    let at = text.rfind(needle).expect("completed 事件裡有 call_B");
    let mut tampered = text.clone();
    tampered.replace_range(at..at + needle.len(), r#""arguments":"{\"path\":\"EVIL\"}","call_id":"call_B""#);
    assert!(matches!(run(tampered.as_bytes(), 32), Err(CodecError::Protocol(_))));
    let needle = "\"text\":\"我先讀兩個檔案\"";
    let at = text.rfind(needle).unwrap();
    let mut tampered_text = text.clone();
    tampered_text.replace_range(at..at + needle.len(), "\"text\":\"別的字\"");
    assert!(matches!(run(tampered_text.as_bytes(), 32), Err(CodecError::Protocol(_))));
}

#[test]
fn tool_arguments_must_be_a_json_object() {
    for bad in ["{\"path\":", "[1]", "null", ""] {
        let bytes = sse(&[json!({"type": "response.completed", "response": {"id": "r", "status": "completed", "output": [call_item("fc", "c1", "read_file", bad)], "usage": usage(1, 0, 0, 1, 0)}})]);
        assert_eq!(run(&bytes, 7).map(|_| ()), Err(CodecError::BadToolArguments("c1".into())), "args={bad:?}");
    }
}

#[test]
fn http_errors_are_classified_by_kind() {
    let body = |code: &str, kind: &str| json!({"error": {"code": code, "type": kind, "message": "m"}}).to_string();
    assert_eq!(classify_http(401, &body("invalid_api_key", "invalid_request_error"), None), ErrorClass::Auth);
    assert_eq!(classify_http(403, &body("", "permission_error"), None), ErrorClass::Permission);
    assert_eq!(classify_http(400, &body("model_not_found", "invalid_request_error"), None), ErrorClass::ModelRejected { code: "model_not_found".into() });
    assert_eq!(classify_http(404, &body("model_not_found", ""), None), ErrorClass::ModelRejected { code: "model_not_found".into() });
    assert_eq!(classify_http(429, &body("insufficient_quota", "insufficient_quota"), Some("30")), ErrorClass::InsufficientQuota);
    assert_eq!(classify_http(429, &body("rate_limit_exceeded", "requests"), Some("7")), ErrorClass::RateLimited { retry_after_secs: Some(7) });
    assert_eq!(classify_http(429, "not json", None), ErrorClass::RateLimited { retry_after_secs: None });
    assert_eq!(classify_http(429, &body("rate_limit_exceeded", ""), Some("Wed, 21 Oct")), ErrorClass::RateLimited { retry_after_secs: None }, "只認整數秒");
    assert_eq!(classify_http(400, &body("invalid_value", "invalid_request_error"), None), ErrorClass::InvalidRequest { code: "invalid_value".into() });
    for s in [500, 502, 503, 504, 408, 409] {
        assert!(classify_http(s, "", None).outcome_unknown(), "status {s}");
    }
    for f in [TransportFailure::Timeout, TransportFailure::Disconnected] {
        assert!(classify_transport(f).outcome_unknown());
    }
    for named in [ErrorClass::Auth, ErrorClass::Permission, ErrorClass::InsufficientQuota, ErrorClass::RateLimited { retry_after_secs: None }] {
        assert!(!named.outcome_unknown());
    }
}

#[test]
fn sse_parser_handles_crlf_comments_multiline_data_and_any_split() {
    let raw = b": keep-alive\r\nevent: a\r\ndata: line1\r\ndata: line2\r\n\r\nid: 7\nevent: b\ndata: {\"x\":1}\n\ndata: tail\r\r";
    let expect = vec![
        SseEvent { event: Some("a".into()), data: "line1\nline2".into() },
        SseEvent { event: Some("b".into()), data: "{\"x\":1}".into() },
        SseEvent { event: None, data: "tail".into() },
    ];
    for chunk in 1..=raw.len() {
        let mut p = SseParser::new();
        let mut got = Vec::new();
        for c in raw.chunks(chunk) {
            got.extend(p.feed(c).unwrap());
        }
        assert_eq!(got, expect, "chunk={chunk}");
        assert!(!p.has_partial());
    }
    let mut p = SseParser::new();
    assert!(p.feed(b"event: x\ndata: partial").unwrap().is_empty());
    assert!(p.has_partial());
}

#[test]
fn sse_parser_bounds_memory_and_rejects_invalid_utf8() {
    let mut p = SseParser::new();
    let mut big = b"data: ".to_vec();
    big.extend(std::iter::repeat_n(b'a', MAX_LINE_BYTES + 1));
    assert_eq!(p.feed(&big), Err(SseError::TooLarge));
    let mut p = SseParser::new();
    assert_eq!(p.feed(b"data: \xff\xfe\n\n"), Err(SseError::InvalidUtf8));
    let mut p = SseParser::new();
    let line = format!("data: {}\n", "b".repeat(1024 * 1024));
    let mut res = Ok(Vec::new());
    for _ in 0..9 {
        res = p.feed(line.as_bytes());
        if res.is_err() {
            break;
        }
    }
    assert_eq!(res, Err(SseError::TooLarge));
}

/// "0.10" → 100_000（微美元／百萬 tokens）：USD 十進位字串 × 1_000_000，整數運算。
fn usd_str_to_micro(s: &str) -> u64 {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    assert!(frac.len() <= 6, "價表最多到微美元");
    let frac = format!("{frac:0<6}");
    whole.parse::<u64>().unwrap() * 1_000_000 + frac.parse::<u64>().unwrap()
}

#[test]
fn the_price_table_matches_the_dated_fixture() {
    for (model, raw) in [
        ("gpt-6-luna", include_str!("fixtures/gpt-6-luna-pricing-2026-11-18.json")),
        ("gpt-6.2-astra", include_str!("fixtures/gpt-6.2-astra-pricing-2026-11-19.json")),
        ("gpt-6.2-sol", include_str!("fixtures/gpt-6.2-sol-pricing-2026-11-19.json")),
    ] {
        let fixture: Value = serde_json::from_str(raw).unwrap();
        let p = &fixture["per_million_tokens_usd"];
        let t = price_table(model).unwrap_or_else(|| panic!("{model} 有價"));
        assert_eq!(t.model, fixture["model"].as_str().unwrap());
        assert_eq!(t.effective_date, fixture["effective_date"].as_str().unwrap());
        assert_eq!(t.input, usd_str_to_micro(p["input"].as_str().unwrap()), "{model} input");
        assert_eq!(t.cache_read, usd_str_to_micro(p["cache_read"].as_str().unwrap()), "{model} cache_read");
        assert_eq!(t.cache_write, usd_str_to_micro(p["cache_write_default"].as_str().unwrap()), "{model} cache_write");
        assert_eq!(t.cache_write_12h, usd_str_to_micro(p["cache_write_12h"].as_str().unwrap()), "{model} cache_write_12h");
        assert_eq!(t.output, usd_str_to_micro(p["output"].as_str().unwrap()), "{model} output");
        assert_eq!(t.cache_write_12h * 2, t.input * 5, "{model}：12 小時寫入＝輸入價 ×2.5");
        assert_eq!(Some(t.long_context_over), fixture["long_context"]["over_input_tokens"].as_u64());
        if let Some(w) = fixture["context_window"].as_u64() {
            assert_eq!(t.context_window, w);
        }
        assert_eq!(t.context_window, 1_050_000, "{model} context window");
    }
    assert_eq!(price_table("gpt-6-luna").map(|t| (t.input, t.cache_read, t.cache_write, t.output)), Some((100_000, 10_000, 125_000, 500_000)));
    assert_eq!(price_table("gpt-6.2-astra").map(|t| (t.input, t.cache_read, t.cache_write, t.output)), Some((8_000_000, 400_000, 10_000_000, 40_000_000)));
    assert_eq!(price_table("gpt-6.2-sol").map(|t| (t.input, t.cache_read, t.cache_write, t.output)), Some((2_000_000, 100_000, 2_500_000, 10_000_000)));
    assert!(price_table("gpt-unknown").is_none(), "缺價＝找不到，呼叫端在 HTTP 前拒絕");
}

/// astra／sol 的精確結算（微美元）：短文、長文（輸入超過 272K，輸入與快取 ×2、輸出 ×1.5）、12 小時寫入。
#[test]
fn astra_and_sol_settle_to_the_exact_micro_dollar() {
    let astra = price_table("gpt-6.2-astra").unwrap();
    let sol = price_table("gpt-6.2-sol").unwrap();
    let short = Usage { input_tokens: 1000, cached_tokens: 0, cache_write_tokens: 0, cache_write_12h_tokens: 0, output_tokens: 100, reasoning_tokens: 0 };
    assert_eq!(cost_microusd(astra, &short), Some(12_000), "1000×$8＋100×$40 = $0.012");
    let long = Usage { input_tokens: 300_000, cached_tokens: 100_000, cache_write_tokens: 0, cache_write_12h_tokens: 0, output_tokens: 1000, reasoning_tokens: 0 };
    assert_eq!(cost_microusd(astra, &long), Some(3_340_000), "長文：(200K×$8＋100K×$0.4)×2＋1K×$40×1.5 = $3.34");
    let twelve = Usage { input_tokens: 1000, cached_tokens: 0, cache_write_tokens: 1000, cache_write_12h_tokens: 1000, output_tokens: 0, reasoning_tokens: 0 };
    assert_eq!(cost_microusd(sol, &twelve), Some(5_000), "12 小時寫入 1K×$5（sol）");
    let mixed = Usage { input_tokens: 2000, cached_tokens: 500, cache_write_tokens: 500, cache_write_12h_tokens: 500, output_tokens: 100, reasoning_tokens: 0 };
    assert_eq!(cost_microusd(astra, &mixed), Some(22_200), "1K×$8＋0.5K×$0.4＋0.5K×$20＋0.1K×$40 = 8000＋200＋10000＋4000");
}

fn cost(u: Value) -> u64 {
    let usage = Usage::from_response(Some(&u)).unwrap();
    cost_microusd(&GPT_6_LUNA_2026_11_18, &usage).unwrap()
}

#[test]
fn cost_splits_ordinary_input_cache_read_cache_write_and_output() {
    let u = usage(10_000, 6_000, 1_000, 2_000, 1_500);
    let parsed = Usage::from_response(Some(&u)).unwrap();
    assert_eq!(parsed.ordinary_input(), 3_000);
    assert_eq!(cost_microusd(&GPT_6_LUNA_2026_11_18, &parsed), Some(1_485));
    assert_eq!(cost(usage(200_000, 0, 0, 0, 0)), 20_000, "20 萬普通輸入 = $0.02（$0.10／百萬）");
    assert_eq!(cost(usage(200_000, 200_000, 0, 0, 0)), 2_000, "20 萬快取讀 = $0.002（$0.01／百萬）");
    assert_eq!(cost(usage(200_000, 0, 200_000, 0, 0)), 25_000, "20 萬預設寫 = $0.025（$0.125／百萬）");
    assert_eq!(cost(usage(0, 0, 0, 1_000_000, 0)), 500_000, "100 萬輸出 = $0.50");
}

#[test]
fn reasoning_tokens_are_already_inside_output_and_are_not_charged_again() {
    let with = cost(usage(2_000, 0, 0, 10_000, 9_000));
    let without = cost(usage(2_000, 0, 0, 10_000, 0));
    assert_eq!(with, without);
    assert_eq!(with, 200 + 5_000, "2000 輸入 + 10000 輸出，reasoning 不另計");
}

#[test]
fn a_cheap_short_turn_is_never_rounded_down_to_zero() {
    assert_eq!(cost(usage(1, 0, 0, 0, 0)), 1, "一個 token 的 $0.0000001 進位成 1 微美元");
    assert_eq!(cost(usage(0, 0, 0, 1, 0)), 1);
    assert_eq!(cost(usage(3, 0, 0, 2, 0)), 2, "(3×0.1 + 2×0.5) = 1.3 微美元 → 2");
    assert_eq!(cost(usage(0, 0, 0, 0, 0)), 0, "真的沒有用量才是 0");
    assert_eq!(cost(usage(2, 1, 0, 0, 0)), 1);
}

#[test]
fn inconsistent_usage_is_refused_instead_of_priced() {
    for bad in [
        usage(100, 80, 40, 10, 0),
        usage(100, 0, 0, 10, 11),
        json!({"input_tokens": 10, "output_tokens": 5, "total_tokens": 99}),
        json!({"input_tokens": 10}),
        json!({"input_tokens": -1, "output_tokens": 5}),
        json!("usage"),
    ] {
        assert!(matches!(Usage::from_response(Some(&bad)), Err(UsageError::Inconsistent(_))), "{bad}");
    }
    let min = Usage::from_response(Some(&json!({"input_tokens": 10, "output_tokens": 5}))).unwrap();
    assert_eq!((min.ordinary_input(), min.cached_tokens, min.cache_write_tokens), (10, 0, 0));
}

#[test]
fn cost_arithmetic_never_wraps_around() {
    let huge = Usage { input_tokens: u64::MAX, cached_tokens: 0, cache_write_tokens: 0, cache_write_12h_tokens: 0, output_tokens: u64::MAX, reasoning_tokens: 0 };
    let exact = (u128::from(u64::MAX) * 100_000 * 4 + u128::from(u64::MAX) * 500_000 * 3).div_ceil(2_000_000);
    assert_eq!(cost_microusd(&GPT_6_LUNA_2026_11_18, &huge), u64::try_from(exact).ok());
}

fn usage_with_ttl(input: u64, cached: u64, w30: u64, w12: u64, output: u64) -> Value {
    let mut u = usage(input, cached, w30 + w12, output, 0);
    u["input_tokens_details"]["cache_write_tokens_details"] = json!({"ttl_30m_tokens": w30, "ttl_12h_tokens": w12});
    u
}

#[test]
fn input_over_272k_prices_the_whole_request_at_the_long_context_rate() {
    assert_eq!(cost(usage(272_000, 0, 0, 1_000, 0)), 27_200 + 500);
    assert_eq!(cost(usage(272_001, 0, 0, 1_000, 0)), 54_401 + 750, "272001×0.2 = 54400.2 → 與輸出相加後進位一次");
    assert_eq!(cost(usage(300_000, 100_000, 0, 1_001, 0)), 42_751);
    assert_eq!(cost(usage(300_000, 0, 300_000, 0, 0)), 75_000);
}

#[test]
fn twelve_hour_cache_writes_are_priced_separately_and_lifetimes_must_add_up() {
    let u = usage_with_ttl(12_000, 4_000, 3_000, 4_000, 500);
    let parsed = Usage::from_response(Some(&u)).unwrap();
    assert_eq!((parsed.cache_write_tokens, parsed.cache_write_12h_tokens, parsed.ordinary_input()), (7_000, 4_000, 1_000));
    assert_eq!(cost_microusd(&GPT_6_LUNA_2026_11_18, &parsed), Some(1_765));
    assert_eq!(cost(usage_with_ttl(12_000, 4_000, 7_000, 0, 500)), 1_265);
    assert_eq!(cost(usage(12_000, 4_000, 7_000, 500, 0)), 1_265, "沒有分項＝預設期限");

    let mut off = usage_with_ttl(12_000, 4_000, 3_000, 4_000, 500);
    off["input_tokens_details"]["cache_write_tokens"] = json!(7_001);
    off["total_tokens"] = json!(12_500);
    let mut unknown = usage_with_ttl(12_000, 4_000, 3_000, 4_000, 500);
    unknown["input_tokens_details"]["cache_write_tokens_details"]["ttl_7d_tokens"] = json!(0);
    let mut not_object = usage(12_000, 4_000, 7_000, 500, 0);
    not_object["input_tokens_details"]["cache_write_tokens_details"] = json!(7_000);
    for bad in [off, unknown, not_object] {
        assert!(matches!(Usage::from_response(Some(&bad)), Err(UsageError::Inconsistent(_))), "{bad}");
    }
    let broken = Usage { input_tokens: 10, cached_tokens: 0, cache_write_tokens: 2, cache_write_12h_tokens: 3, output_tokens: 0, reasoning_tokens: 0 };
    assert_eq!(cost_microusd(&GPT_6_LUNA_2026_11_18, &broken), None);
}

#[test]
fn the_reservation_quote_is_an_upper_bound_on_any_settlement() {
    let t = &GPT_6_LUNA_2026_11_18;
    let quote = |input, output| quote_upper_bound_microusd(t, input, output).unwrap();
    for (input, output) in [(1, 1), (9_000, 4_096), (272_000, 128_000), (272_001, 128_000), (1_050_000, 128_000)] {
        let worst = cost(usage_with_ttl(input, 0, 0, input, output));
        assert_eq!(quote(input, output), worst, "input={input}");
        for cheaper in [usage(input, 0, 0, output, 0), usage(input, input, 0, output, 0), usage(input, 0, input, output / 2, 0), usage(input / 2, 0, 0, output, 0)] {
            assert!(cost(cheaper.clone()) <= quote(input, output), "{cheaper}");
        }
    }
    assert_eq!(quote(1, 0), 1, "再小的預留也不是 0");
    assert_eq!(quote(0, 0), 0);
    assert_eq!(quote(272_000, 0), 68_000);
    assert_eq!(quote(272_001, 0), 136_001);
    assert_eq!(quote_upper_bound_microusd(t, u64::MAX, u64::MAX), u64::try_from((u128::from(u64::MAX) * 250_000 * 4 + u128::from(u64::MAX) * 500_000 * 3).div_ceil(2_000_000)).ok());
}

#[test]
fn the_service_tier_is_reported_and_only_the_standard_tier_has_a_price() {
    let msg = message_item("好");
    let stream = |tier: Value| {
        let mut resp = json!({"id": "r", "status": "completed", "output": [msg.clone()], "usage": usage(10, 0, 0, 5, 0)});
        if !tier.is_null() {
            resp["service_tier"] = tier;
        }
        completed(run(&sse(&[json!({"type": "response.completed", "response": resp})]), 9).unwrap().0).service_tier
    };
    assert_eq!(stream(Value::Null), None);
    assert_eq!(stream(json!("default")).as_deref(), Some("default"));
    assert_eq!(stream(json!("priority")).as_deref(), Some("priority"));
    assert!(is_standard_tier(None) && is_standard_tier(Some("default")));
    for other in ["flex", "priority", "fast", "auto", ""] {
        assert!(!is_standard_tier(Some(other)), "{other}");
    }
}

#[test]
fn any_chunking_and_any_line_ending_assemble_to_the_same_outcome() {
    let bytes = two_tool_stream();
    let reference = run(&bytes, bytes.len()).unwrap();
    for seed in 0..64u64 {
        let mut state = seed;
        let mut asm = ResponseAssembler::new();
        let mut updates = Vec::new();
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let (chunk, tail) = rest.split_at((1 + (state >> 33) as usize % 97).min(rest.len()));
            rest = tail;
            updates.extend(asm.feed(chunk).unwrap());
        }
        assert_eq!((asm.finish().unwrap(), updates), reference, "seed={seed}");
    }
    let text = String::from_utf8(bytes).unwrap();
    for eol in ["\r\n", "\r"] {
        let other = text.replace('\n', eol);
        for chunk in [1, 2, 7, other.len()] {
            assert_eq!(run(other.as_bytes(), chunk).unwrap(), reference, "eol={eol:?} chunk={chunk}");
        }
    }
}

#[test]
fn bytes_after_the_terminal_event_cannot_overturn_the_result() {
    let mut bytes = final_text_stream("好");
    let reference = run(&bytes, bytes.len()).unwrap().0;
    bytes.extend_from_slice(b"data: {not json\n\ndata: [DONE]\n\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{}}\n\n");
    for chunk in [1, 7, bytes.len()] {
        assert_eq!(run(&bytes, chunk).unwrap().0, reference, "chunk={chunk}");
    }
    assert!(matches!(run(b"data: {not json\n\n", 3), Err(CodecError::Protocol(_))));
}

#[test]
fn a_completed_response_carrying_an_unfinished_tool_call_runs_nothing() {
    for status in ["in_progress", "incomplete"] {
        let mut call = call_item("fc", "c1", "read_file", "{}");
        call["status"] = json!(status);
        let ok = call_item("fc0", "c0", "read_file", "{}");
        let bytes = sse(&[json!({"type": "response.completed", "response": {"id": "r", "status": "completed", "output": [ok, call], "usage": usage(1, 0, 0, 1, 0)}})]);
        assert_eq!(run(&bytes, 7).map(|_| ()), Err(CodecError::BadToolArguments("c1".into())), "status={status}");
    }
}

#[test]
fn quotes_newlines_and_backslashes_in_the_request_are_escaped_not_spliced() {
    let tricky = "He said \"hi\"\nline2\t\\ \u{1}\",\"store\":true,\"x\":\"結束";
    let mut r = luna_request(vec![InputItem::User(tricky.into())]);
    r.instructions = tricky.into();
    let bytes = r.body().unwrap();
    let raw = std::str::from_utf8(&bytes).unwrap();
    assert!(raw.contains(r#"He said \"hi\"\nline2\t\\ \u0001\",\"store\":true"#), "{raw}");
    assert!(!raw.contains('\n'), "body 是單行 JSON");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["instructions"], json!(tricky));
    assert_eq!(body["input"][0]["content"][0]["text"], json!(tricky));
    assert_eq!(body["store"], json!(false), "內容裡的字樣不能變成欄位");
    assert_eq!(r.body().unwrap(), bytes);
}

#[test]
fn settle_refuses_unknown_models_and_non_standard_tiers() {
    let u = Usage::from_response(Some(&usage_with_ttl(12_000, 4_000, 3_000, 4_000, 500))).unwrap();
    assert_eq!(settle("gpt-6-luna", &u, None), Ok(1_765));
    assert_eq!(settle("gpt-6-luna", &u, Some("default")), Ok(1_765));
    for tier in ["flex", "priority", "fast", "auto", ""] {
        assert_eq!(settle("gpt-6-luna", &u, Some(tier)), Err(PriceError::UnsupportedTier), "{tier}");
    }
    assert_eq!(settle("gpt-unknown", &u, None), Err(PriceError::UnknownModel));
    assert_eq!(settle("gpt-6-luna-2026-11-01", &u, None), Err(PriceError::UnknownModel), "價表只認請求的模型 id");
    let tiny = Usage::from_response(Some(&usage(7, 0, 0, 1, 0))).unwrap();
    assert_eq!(settle("gpt-6-luna", &tiny, None), Ok(2), "0.7 + 0.5 = 1.2 微美元 → 2，不是 0");
    let broken = Usage { input_tokens: 10, cached_tokens: 8, cache_write_tokens: 8, cache_write_12h_tokens: 0, output_tokens: 0, reasoning_tokens: 0 };
    assert_eq!(settle("gpt-6-luna", &broken, None), Err(PriceError::Overflow), "分項超過總數");
    let msg = message_item("好");
    let bytes = sse(&[json!({"type": "response.completed", "response": {"id": "r", "status": "completed", "service_tier": "priority", "output": [msg], "usage": usage(10, 0, 0, 5, 0)}})]);
    let out = completed(run(&bytes, 9).unwrap().0);
    assert_eq!(settle("gpt-6-luna", out.usage.as_ref().unwrap(), out.service_tier.as_deref()), Err(PriceError::UnsupportedTier));
}

#[test]
fn quote_request_bounds_the_bytes_that_are_actually_sent() {
    let request = luna_request(vec![InputItem::User("hi".into())]);
    let wire = request.body().unwrap();
    let bound = input_token_bound("gpt-6-luna", wire.len()).unwrap();
    assert_eq!(bound, wire.len() as u64 + INPUT_MARGIN_TOKENS);
    let quote = quote_request(&request).unwrap();
    assert_eq!(Ok(quote), quote_upper_bound("gpt-6-luna", bound, 4096));
    let worst = Usage::from_response(Some(&usage_with_ttl(bound, 0, 0, bound, 4096))).unwrap();
    assert_eq!(settle("gpt-6-luna", &worst, None), Ok(quote));
    let longer = luna_request(vec![InputItem::User("hi".repeat(5_000))]);
    assert!(quote_request(&longer).unwrap() > quote);
    assert_eq!(quote_request(&request), Ok(quote));

    let mut unknown = luna_request(vec![InputItem::User("hi".into())]);
    unknown.model = "gpt-unknown".into();
    assert_eq!(quote_request(&unknown), Err(PriceError::UnknownModel));
    let mut bad_effort = luna_request(vec![InputItem::User("hi".into())]);
    bad_effort.effort = Some("minimal".into());
    assert_eq!(quote_request(&bad_effort), Err(PriceError::InvalidRequest));
    let orphan = luna_request(vec![InputItem::ToolOutput { call_id: "nope".into(), output: "x".into() }]);
    assert_eq!(quote_request(&orphan), Err(PriceError::InvalidRequest));
    let big = luna_request(vec![InputItem::User("x".repeat(1_050_000))]);
    assert_eq!(quote_request(&big), quote_upper_bound("gpt-6-luna", 1_050_000, big.max_output_tokens));

    assert_eq!(input_token_bound("gpt-6-luna", 1_050_000 - 8192), Ok(1_050_000));
    assert_eq!(input_token_bound("gpt-6-luna", 1_050_000 - 8191), Ok(1_050_000), "封頂在 context window");
    assert_eq!(input_token_bound("gpt-6-luna", 10_000_000), Ok(1_050_000));
    assert_eq!(input_token_bound("gpt-6-luna", 100), Ok(100 + 8192), "小請求仍用位元組上界");
    assert_eq!(input_token_bound("gpt-unknown", 1), Err(PriceError::UnknownModel));
    assert_eq!(quote_upper_bound("gpt-unknown", 1, 1), Err(PriceError::UnknownModel));
    assert_eq!(quote_upper_bound("gpt-6-luna", u64::MAX, u64::MAX), Err(PriceError::Overflow));
    assert_eq!(quote_upper_bound("gpt-6-luna", 272_001, 0), Ok(136_001), "上界跨過 272K 用長文價");
}
