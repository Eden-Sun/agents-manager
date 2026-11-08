//! One tool-free Messages stream. The shared timeout bounds connection/idle time,
//! while a separate ten-minute deadline prevents endless pings from holding a slot.
use super::{ClientError, ClientOutcome, MessagesClient, TranslateRequest, Usage};
use serde_json::{json, Value};
use std::{future::Future, pin::Pin, time::Duration};

const MAX_STREAM_DURATION: Duration = Duration::from_secs(10 * 60);

pub struct StreamingClient;

impl MessagesClient for StreamingClient {
    fn complete<'a>(&'a self, req: &'a TranslateRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
        Box::pin(complete(req))
    }
}

/// Synchronous Messages maxima (not the larger batch-only beta limits).
/// https://platform.claude.com/docs/en/about-claude/models/overview
pub fn model_output_tokens(model: &str) -> u32 {
    const MODELS: &[(&str, u32)] = &[
        ("claude-fable-5-1", 128_000), ("claude-fable-5", 128_000), ("claude-opus-5-5", 128_000),
        ("claude-opus-5", 128_000), ("claude-sonnet-5-5", 128_000),
        ("claude-sonnet-5", 128_000), ("claude-haiku-5-5", 128_000),
        ("claude-opus-4-8", 128_000), ("claude-opus-4-7", 128_000),
        ("claude-opus-4-6", 128_000), ("claude-sonnet-4-6", 128_000),
        ("claude-sonnet-4-5", 64_000), ("claude-sonnet-4-0", 64_000),
        ("claude-sonnet-4", 64_000), ("claude-haiku-4-5", 64_000),
        ("claude-opus-4-5", 64_000), ("claude-3-7-sonnet", 64_000),
        ("claude-opus-4-1", 32_000), ("claude-opus-4-0", 32_000), ("claude-opus-4", 32_000),
        ("claude-3-5-sonnet", 8192), ("claude-3-5-haiku", 8192),
        ("claude-3-opus", 4096), ("claude-3-sonnet", 4096), ("claude-3-haiku", 4096),
    ];
    MODELS.iter().find_map(|(id, tokens)| {
        let known = model == *id || model.strip_prefix(*id).is_some_and(|suffix| {
            suffix == "-latest" || suffix.strip_prefix('-').is_some_and(|date| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()))
        });
        known.then_some(*tokens)
    }).unwrap_or(64_000) // Conservative large allowance for unknown/custom IDs.
}

#[derive(Default)]
struct Events {
    pending: Vec<u8>,
    text: String,
    model: Option<String>,
    stop: Option<String>,
    usage: Option<Usage>,
    done: bool,
}

impl Events {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), ClientError> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = std::str::from_utf8(&line).map_err(|_| self.failed())?.trim_end();
            let Some(data) = line.strip_prefix("data:") else { continue };
            let event: Value = serde_json::from_str(data.trim_start()).map_err(|_| self.failed())?;
            match event["type"].as_str() {
                Some("message_start") => {
                    let m = &event["message"];
                    self.model = m["model"].as_str().map(str::to_owned);
                    self.usage = super::super::claude_api::parse_usage(m);
                    // The initial output count is provisional (usually zero), not final spend.
                    if let Some(usage) = &mut self.usage { usage.output_tokens = None; }
                }
                Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                    let text = event["delta"]["text"].as_str().ok_or_else(|| self.failed())?;
                    self.text.push_str(text);
                }
                Some("message_delta") => {
                    if let Some(stop) = event["delta"]["stop_reason"].as_str() { self.stop = Some(stop.into()); }
                    if let Some(delta) = super::super::claude_api::parse_usage(&event) {
                        if let Some(usage) = &mut self.usage {
                            if delta.input_tokens.is_some() { usage.input_tokens = delta.input_tokens; }
                            if delta.output_tokens.is_some() { usage.output_tokens = delta.output_tokens; }
                            if delta.cache_creation_input_tokens.is_some() { usage.cache_creation_input_tokens = delta.cache_creation_input_tokens; }
                            if delta.cache_read_input_tokens.is_some() { usage.cache_read_input_tokens = delta.cache_read_input_tokens; }
                        } else { self.usage = Some(delta); }
                    }
                }
                Some("message_stop") => self.done = true,
                Some("error") => return Err(self.failed()),
                _ => {} // pings, thinking/signature deltas and future metadata are not text.
            }
        }
        Ok(())
    }
    fn failed(&self) -> ClientError { ClientError::Http { status: 502, usage: self.usage.clone() } }
    fn finish(self) -> Result<ClientOutcome, ClientError> {
        if !self.done { return Err(self.failed()); }
        Ok(ClientOutcome { text: self.text, model: self.model, stop_reason: self.stop, usage: self.usage })
    }
}

async fn complete(req: &TranslateRequest) -> Result<ClientOutcome, ClientError> {
    complete_with_total(req, MAX_STREAM_DURATION).await
}

/// Dropping the timed-out future cancels its HTTP request before the hub releases the slot.
/// No continuation or replacement request is issued here. The stream state lives outside the
/// timed future, so usage already received survives a total-deadline timeout as well.
pub(super) async fn complete_with_total(req: &TranslateRequest, total: Duration) -> Result<ClientOutcome, ClientError> {
    let mut events = Events::default();
    let outcome = tokio::time::timeout(total, complete_stream(req, &mut events)).await;
    match outcome {
        Ok(result) => result,
        Err(_) => Err(ClientError::Timeout { usage: events.usage.take() }),
    }
}

async fn complete_stream(req: &TranslateRequest, events: &mut Events) -> Result<ClientOutcome, ClientError> {
    #[cfg(test)]
    if !crate::claude_api::test_endpoint_is_loopback(&req.endpoint)
        && std::env::var("CHANGELOG_TRANSLATION_PAID_PROBE").as_deref() != Ok("1") {
        return Err(ClientError::Http { status: 0, usage: None });
    }
    let idle = Duration::from_millis(req.timeout_ms.max(1));
    let client = reqwest::Client::builder().connect_timeout(idle)
        .redirect(reqwest::redirect::Policy::none()).build()
        .map_err(|_| ClientError::Http { status: 0, usage: None })?;
    let mut body = req.request_json();
    body["stream"] = json!(true);
    let mut response = tokio::time::timeout(idle, client.post(&req.endpoint)
        .header("x-api-key", &req.key).header("anthropic-version", "2023-06-01")
        .json(&body).send()).await.map_err(|_| ClientError::Timeout { usage: None })?
        .map_err(|_| ClientError::Http { status: 0, usage: None })?;
    let status = response.status().as_u16();
    if status == 429 {
        return Err(ClientError::RateLimited { retry_after: response.headers().get("retry-after")
            .and_then(|v| v.to_str().ok()).map(str::to_owned), usage: None });
    }
    if !response.status().is_success() { return Err(ClientError::Http { status, usage: None }); }
    loop {
        let chunk = tokio::time::timeout(idle, response.chunk()).await
            .map_err(|_| ClientError::Timeout { usage: events.usage.clone() })?
            .map_err(|_| events.failed())?;
        let Some(chunk) = chunk else { break };
        events.feed(&chunk)?;
        if events.done { break; }
    }
    std::mem::take(events).finish()
}

#[cfg(test)]
pub(super) async fn endless_ping_server() -> (String, std::sync::Arc<tokio::sync::Notify>, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/messages", listener.local_addr().unwrap());
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
        signal.notify_one();
        while socket.write_all(b"data: {\"type\":\"ping\"}\n\n").await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // A replacement/overlapping request would already be queued on this listener.
        assert!(tokio::time::timeout(Duration::from_millis(50), listener.accept()).await.is_err());
    });
    (endpoint, entered, server)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire() -> String {
        [json!({"type":"message_start","message":{"model":"claude-sonnet-5-5","usage":{"input_tokens":123,"output_tokens":0,"cache_read_input_tokens":4}}}),
         json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"完整繁中"}}),
         json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":9000}}),
         json!({"type":"message_stop"})].iter().map(|v| format!("event: ignored\r\ndata: {v}\r\n\r\n")).collect()
    }

    #[test]
    fn split_utf8_crlf_and_usage_merge_keep_real_stop_reason() {
        let mut events = Events::default();
        for byte in wire().as_bytes() { events.feed(&[*byte]).unwrap(); }
        let out = events.finish().unwrap();
        assert_eq!(out.text, "完整繁中");
        assert_eq!(out.stop_reason.as_deref(), Some("max_tokens"));
        let usage = out.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(123));
        assert_eq!(usage.output_tokens, Some(9000));
        assert_eq!(usage.cache_read_input_tokens, Some(4));
        assert!(Events::default().finish().is_err(), "EOF is not completion");
        assert!(Events::default().feed(b"data: {bad}\n").is_err());
        assert!(Events::default().feed(b"data: {\"type\":\"error\"}\n").is_err());
    }

    #[tokio::test]
    async fn endless_pings_hit_total_deadline_and_cancel_without_resending() {
        assert_eq!(MAX_STREAM_DURATION, Duration::from_secs(600));
        let (endpoint, _, server) = endless_ping_server().await;
        let req = TranslateRequest { endpoint, key:"mock".into(), model:"claude-sonnet-5-5".into(), effort:"low".into(),
            max_tokens:128_000, timeout_ms:1000, system:None, format:None, prompt:"mock".into() };
        let result = tokio::time::timeout(Duration::from_secs(2), complete_with_total(&req, Duration::from_millis(200))).await.unwrap();
        assert!(matches!(result, Err(ClientError::Timeout { usage: None })));
        tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
    }

    /// Accepts one connection, sends `partial`, then keeps the socket open and stalls.
    /// Any replacement request would reach the listener during the stall; none may.
    #[cfg(test)]
    async fn stalled_stream_server(partial: String) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/messages", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
            socket.write_all(partial.as_bytes()).await.unwrap();
            assert!(tokio::time::timeout(Duration::from_millis(400), listener.accept()).await.is_err());
            drop(socket);
        });
        (endpoint, server)
    }

    #[tokio::test]
    async fn stalled_stream_keeps_known_usage_on_idle_and_total_timeouts() {
        let partial = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"message_start","message":{"model":"claude-sonnet-5-5","usage":{"input_tokens":123,"output_tokens":0,"cache_read_input_tokens":4}}}),
            json!({"type":"message_delta","delta":{},"usage":{"output_tokens":77}}),
        );
        // Idle timeout fires first in the first case; the total deadline fires first in the second.
        for (idle_ms, total) in [(150u64, Duration::from_secs(5)), (5_000, Duration::from_millis(200))] {
            let (endpoint, server) = stalled_stream_server(partial.clone()).await;
            let req = TranslateRequest { endpoint, key:"mock".into(), model:"claude-sonnet-5-5".into(), effort:"low".into(),
                max_tokens:128_000, timeout_ms:idle_ms, system:None, format:None, prompt:"mock".into() };
            let err = match complete_with_total(&req, total).await {
                Err(e) => e,
                Ok(_) => panic!("stalled stream must not complete"),
            };
            let usage = match err {
                ClientError::Timeout { usage: Some(u) } => u,
                other => panic!("expected timeout with usage, got {other:?}"),
            };
            assert_eq!(usage.input_tokens, Some(123));
            assert_eq!(usage.output_tokens, Some(77));
            assert_eq!(usage.cache_read_input_tokens, Some(4));
            assert_eq!(usage.cache_creation_input_tokens, None, "unknown fields stay unknown");
            server.await.unwrap();
        }
    }

    #[test]
    fn legacy_caps_never_override_model_maxima() {
        assert_eq!(model_output_tokens("claude-sonnet-5-5"), 128_000);
        assert_eq!(model_output_tokens("claude-haiku-5-5"), 128_000);
        assert_eq!(model_output_tokens("claude-sonnet-4-5-20250929"), 64_000);
        assert_eq!(model_output_tokens("claude-opus-4-1-20250805"), 32_000);
        assert_eq!(model_output_tokens("claude-3-5-sonnet-20241022"), 8192);
        assert_eq!(model_output_tokens("custom-future-model"), 64_000);
        assert_eq!(model_output_tokens("claude-sonnet-50"), 64_000);
        assert_eq!(model_output_tokens("claude-sonnet-4-6"), 128_000);
    }

    #[tokio::test]
    async fn active_stream_outlives_total_timeout_without_a_second_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/messages", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..end]);
                    let len: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap();
                    if bytes.len() >= end + 4 + len {
                        let body: Value = serde_json::from_slice(&bytes[end+4..end+4+len]).unwrap();
                        assert_eq!(body["stream"], true);
                        assert_eq!(body["max_tokens"], 128_000);
                        assert!(body.get("tools").is_none());
                        break;
                    }
                }
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
            for _ in 0..4 {
                socket.write_all(b"data: {\"type\":\"ping\"}\n\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            socket.write_all(wire().as_bytes()).await.unwrap();
        });
        let req = TranslateRequest { endpoint, key:"mock".into(), model:"claude-sonnet-5-5".into(), effort:"low".into(),
            max_tokens:128_000, timeout_ms:250, system:None, format:None, prompt:"mock".into() };
        let start = std::time::Instant::now();
        let out = StreamingClient.complete(&req).await.unwrap();
        assert!(start.elapsed() > Duration::from_millis(250));
        assert_eq!(out.text, "完整繁中");
        server.await.unwrap();
    }
}
