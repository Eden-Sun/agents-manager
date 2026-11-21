//! 進度節奏（與 Claude 一致）：假 upstream 以固定節奏送 chunk，記下每次進度回呼的時間與內容。
//! 規則：距上次報告滿 `http::PROGRESS_INTERVAL`（1 秒）才報；工具參數與 reasoning 的串流也算進度；尾端文字上限 `TAIL_CHARS`。

use super::provider::OpenAiProvider;
use crate::api_bot::protocol::Message;
use crate::api_bot::provider::{Codec, Provider, ProviderExtras, SessionOptions, Transport};
use crate::api_bot::sse::{StreamProgress, TAIL_CHARS};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn event(data: &Value) -> Vec<u8> {
    format!("event: {}\ndata: {data}\n\n", data["type"].as_str().unwrap()).into_bytes()
}

/// 依腳本逐段送出：每段前等待指定時間，讓 chunk 落在不同的節奏上。
fn paced_body(script: Vec<(Duration, Vec<u8>)>) -> axum::body::Body {
    let stream = futures::stream::unfold(script.into_iter(), |mut it| async move {
        let (wait, bytes) = it.next()?;
        tokio::time::sleep(wait).await;
        Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(bytes)), it))
    });
    axum::body::Body::from_stream(stream)
}

async fn paced_server(script: Vec<(Duration, Vec<u8>)>) -> (String, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}/v1/responses", listener.local_addr().unwrap().port());
    let stop = CancellationToken::new();
    let script = Arc::new(Mutex::new(Some(script)));
    let app = axum::Router::new().route(
        "/v1/responses",
        axum::routing::post(move || {
            let script = script.clone();
            async move {
                let body = paced_body(script.lock().unwrap().take().unwrap_or_default());
                axum::response::Response::builder().status(StatusCode::OK).header("content-type", "text/event-stream").body(body).unwrap()
            }
        }),
    );
    let child = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).with_graceful_shutdown(async move { child.cancelled().await }).await;
    });
    (endpoint, stop)
}

/// 一次回合的串流：先 reasoning＋函式呼叫開始（0 秒）→ 參數分兩段（1.1 秒後，兩段緊接）→ 文字 3000 字＋完成（再 1.1 秒後）。
fn script() -> Vec<(Duration, Vec<u8>)> {
    let t1 = Duration::from_millis(1100);
    let text = "a".repeat(3000);
    let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "ENC"});
    let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "bash", "arguments": "{\"command\":\"true\"}", "status": "completed"});
    let message = json!({"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": text.clone(), "annotations": []}]});
    let usage = json!({"input_tokens": 900, "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}, "output_tokens": 3000, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 3900});
    let mut first = Vec::new();
    first.extend(event(&json!({"type": "response.created", "response": {"id": "resp_p"}})));
    first.extend(event(&json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}})));
    first.extend(event(&json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "bash", "arguments": ""}})));
    let mut args1 = Vec::new();
    args1.extend(event(&json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"command\":\""})));
    let mut args2 = Vec::new();
    args2.extend(event(&json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "true\"}"})));
    let mut last = Vec::new();
    last.extend(event(&json!({"type": "response.function_call_arguments.done", "output_index": 1, "arguments": "{\"command\":\"true\"}"})));
    last.extend(event(&json!({"type": "response.output_item.done", "output_index": 1, "item": call.clone()})));
    last.extend(event(&json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning.clone()})));
    last.extend(event(&json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}})));
    last.extend(event(&json!({"type": "response.output_text.delta", "output_index": 2, "delta": text})));
    last.extend(event(&json!({"type": "response.output_item.done", "output_index": 2, "item": message.clone()})));
    last.extend(event(&json!({"type": "response.completed", "response": {"id": "resp_p", "status": "completed", "output": [reasoning, call, message], "usage": usage}})));
    vec![(Duration::ZERO, first), (t1, args1), (Duration::ZERO, args2), (t1, last)]
}

#[tokio::test]
async fn progress_reports_follow_the_claude_cadence_and_include_tool_argument_streams() {
    let (endpoint, stop) = paced_server(script()).await;
    let provider = Arc::new(
        OpenAiProvider::with_standard_pricing(SessionOptions { model: "gpt-6-luna".into(), effort: "medium".into(), max_output_tokens: 4096, extras: ProviderExtras::None }).expect("approved model"),
    );
    let messages = vec![Message { role: "user".into(), content: json!("hi") }];
    let request = provider.encode("medium", &json!("sys"), &messages, None).unwrap();
    let cancel = CancellationToken::new();
    let transport = Transport { endpoint: &endpoint, key: "sk-test-progress", timeout: Duration::from_secs(10), max_response_bytes: 1024 * 1024 };
    let reports: Arc<Mutex<Vec<(Instant, StreamProgress)>>> = Arc::default();
    let sink = reports.clone();
    let report = move |p: &StreamProgress| sink.lock().unwrap().push((Instant::now(), p.clone()));
    let start = Instant::now();
    let result = provider.send(transport, request.as_ref(), &cancel, Some(&report)).await;
    stop.cancel();
    assert!(result.is_ok(), "the scripted turn completes: {result:?}");

    let reports = reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 2, "one report per elapsed interval, not per chunk: {:?}", reports.iter().map(|(_, p)| p.other_bytes).collect::<Vec<_>>());
    assert!(reports[0].0.duration_since(start) >= Duration::from_secs(1), "the first report waits a full interval");
    let gap = reports[1].0.duration_since(reports[0].0);
    assert!(gap >= Duration::from_secs(1), "consecutive reports are at least one interval apart: {gap:?}");
    assert!(reports[0].1.other_bytes > 0, "tool-argument bytes count as progress");
    assert_eq!(reports[0].1.generated_chars, 0, "no text yet at the first report");
    assert_eq!(reports[1].1.generated_chars, 3000);
    assert_eq!(reports[1].1.text_tail.chars().count(), TAIL_CHARS, "tail is capped like Claude's");
}
