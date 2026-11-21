//! OpenAI 傳輸層的 key 外洩測試：假 key（sentinel）不得出現在 log、錯誤的文字與 Debug 裡。
//! 401 回吐 key、串流成功、串流中途 error 三種回應都跑一次；loopback 假 server，不打真的 API。

use super::provider::OpenAiProvider;
use crate::api_bot::protocol::Message;
use crate::api_bot::provider::{Codec, Provider, ProviderExtras, SessionOptions, Transport};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use crate::api_bot::LogSink;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const SENTINEL: &str = "sk-test-SENTINEL-XYZ";
const ENC: &str = "ENC-SENTINEL-reasoning-blob-42";

/// 假 server：每次 POST 照順序回一份腳本（status、body）；腳本用完回 500。
struct Server {
    endpoint: String,
    stop: CancellationToken,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
async fn serve(replies: Vec<(u16, Vec<u8>, &'static str)>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}/v1/responses", listener.local_addr().unwrap().port());
    let script = Arc::new(Mutex::new(replies.into_iter().collect::<std::collections::VecDeque<_>>()));
    let stop = CancellationToken::new();
    let app = axum::Router::new().route(
        "/v1/responses",
        axum::routing::post(move |_headers: HeaderMap, _body: axum::body::Bytes| {
            let script = script.clone();
            async move {
                let (status, body, ctype) = script.lock().unwrap().pop_front().unwrap_or((500, b"{}".to_vec(), "application/json"));
                axum::response::Response::builder()
                    .status(StatusCode::from_u16(status).unwrap())
                    .header("content-type", ctype)
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }
        }),
    );
    let child = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).with_graceful_shutdown(async move { child.cancelled().await }).await;
    });
    Server { endpoint, stop }
}

fn luna() -> Arc<OpenAiProvider> {
    Arc::new(OpenAiProvider::with_standard_pricing(SessionOptions { model: "gpt-6-luna".into(), effort: "medium".into(), max_output_tokens: 4096, extras: ProviderExtras::None }).expect("approved model"))
}

fn sse(events: &[Value]) -> Vec<u8> {
    let mut out = String::new();
    for e in events {
        out.push_str(&format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()));
    }
    out.into_bytes()
}

/// 跑一次 send，同時擷取這段期間所有 log。回傳（結果的 Debug／Display、log 文字）。
async fn send_capturing_logs(key: &str, reply: Vec<(u16, Vec<u8>, &'static str)>) -> (String, String, bool) {
    let sink = LogSink::default();
    let subscriber = tracing_subscriber::fmt().with_writer(sink.clone()).with_max_level(tracing::Level::TRACE).with_ansi(false).finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = serve(reply).await;
    let provider = luna();
    let messages = vec![Message { role: "user".into(), content: json!("hi") }];
    let request = provider.encode("medium", &json!("sys"), &messages, None).unwrap();
    let cancel = CancellationToken::new();
    let transport = Transport { endpoint: &server.endpoint, key, timeout: Duration::from_secs(5), max_response_bytes: 1024 * 1024 };
    let result = provider.send(transport, request.as_ref(), &cancel, None).await;
    let shown = match &result {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("{e:?}"),
    };
    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    (shown, logs, result.is_ok())
}

#[tokio::test]
async fn a_401_that_echoes_the_key_leaks_it_into_neither_log_nor_error() {
    let body = json!({"error": {"code": "invalid_api_key", "type": "invalid_request_error", "message": format!("Incorrect API key provided: {SENTINEL}.")}});
    let (shown, logs, ok) = send_capturing_logs(SENTINEL, vec![(401, body.to_string().into_bytes(), "application/json")]).await;
    assert!(!ok);
    assert!(!shown.contains(SENTINEL), "錯誤文字洩漏 key：{shown}");
    assert!(!logs.contains(SENTINEL), "log 洩漏 key：{logs}");
    assert!(shown.contains("redacted"), "應留下遮蔽標記，方便查：{shown}");
}

#[tokio::test]
async fn a_successful_turn_with_encrypted_reasoning_leaks_neither_key_nor_reasoning_into_log() {
    let msg = json!({"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "好", "annotations": []}]});
    let stream = sse(&[
        json!({"type": "response.created", "response": {"id": "resp_2"}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": ENC}}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": msg.clone()}),
        json!({"type": "response.completed", "response": {"id": "resp_2", "status": "completed", "output": [{"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": ENC}, msg], "usage": {"input_tokens": 900, "input_tokens_details": {"cached_tokens": 300, "cache_write_tokens": 0}, "output_tokens": 30, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 930}}}),
    ]);
    let (shown, logs, ok) = send_capturing_logs(SENTINEL, vec![(200, stream, "text/event-stream")]).await;
    assert!(ok, "{shown}");
    assert!(!logs.contains(SENTINEL), "log 洩漏 key：{logs}");
    assert!(!logs.contains(ENC) && !logs.contains("encrypted_content"), "log 洩漏加密 reasoning：{logs}");
    assert!(!shown.contains(ENC), "{shown}");
}

#[tokio::test]
async fn a_stream_that_fails_midway_and_echoes_the_key_leaks_it_nowhere() {
    let stream = sse(&[
        json!({"type": "response.created", "response": {"id": "resp_3"}}),
        json!({"type": "error", "code": "server_error", "message": format!("upstream failed for {SENTINEL}")}),
    ]);
    let (shown, logs, ok) = send_capturing_logs(SENTINEL, vec![(200, stream, "text/event-stream")]).await;
    assert!(!ok);
    assert!(!shown.contains(SENTINEL) && !logs.contains(SENTINEL), "{shown}\n{logs}");
}
