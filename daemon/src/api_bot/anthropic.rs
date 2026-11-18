//! Anthropic（Claude Messages API）的 provider 實作：transcript 的原生形狀、請求編碼（`protocol.rs`）、
//! 傳輸（`http.rs`／`sse.rs`）與報價都在這裡接上 [`super::provider`] 的中立介面。行為與抽接點之前逐位元組相同。
use super::http;
use super::protocol::{self, Message, Options, Response};
use super::provider::{Codec, InvalidResponse, Provider, StopReason, ToolResult, Transport, TurnOutput, MAX_TOOL_CALLS_PER_RESPONSE};
use super::quote::{QuoteError, QuoteProvider, RequestEnvelope};
use super::runner::State;
use super::sse::StreamProgress;
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const ID: &str = "anthropic";

fn has_block(response: &Response, kind: &str) -> bool {
    response.content.iter().any(|b| b.get("type").and_then(Value::as_str) == Some(kind))
}

/// Claude 的 transcript 形狀：assistant 訊息＝回應的 content 區塊原樣；工具結果＝一則 user 訊息、裡面是 `tool_result` 區塊。
pub struct AnthropicCodec;

impl Codec for AnthropicCodec {
    fn accept(&self, state: &mut State, response: Response) -> Result<TurnOutput, InvalidResponse> {
        let calls = response.tools().map_err(|_| InvalidResponse)?;
        if calls.len() > MAX_TOOL_CALLS_PER_RESPONSE || calls.iter().any(|c| state.completed_tool_ids.contains(&c.id)) {
            return Err(InvalidResponse);
        }
        let stop = match response.stop_reason.as_str() {
            "end_turn" => StopReason::EndTurn,
            "refusal" => StopReason::Refusal,
            "max_tokens" => StopReason::MaxTokens,
            other => StopReason::Other(other.to_string()),
        };
        let out = TurnOutput {
            calls,
            stop,
            compacted: has_block(&response, "compaction"),
            // 伺服器端工具（web_search、web_fetch 等）執行滿 server-side iterations 時以 `pause_turn` 暫停。
            resumable_pause: response.stop_reason == "pause_turn" && has_block(&response, "server_tool_use"),
        };
        state.messages.push(Message { role: "assistant".into(), content: json!(response.content) });
        state.responses.push(response);
        Ok(out)
    }

    fn put_tool_results(&self, state: &mut State, results: &[ToolResult], note: Option<&str>, replace_last: bool) {
        let mut blocks: Vec<Value> = results
            .iter()
            .map(|r| json!({"type":"tool_result","tool_use_id":r.call_id,"content":r.output,"is_error":r.is_error}))
            .collect();
        if let Some(note) = note {
            blocks.push(json!({"type": "text", "text": note}));
        }
        if replace_last {
            state.messages.last_mut().expect("tool result batch exists").content = json!(blocks);
        } else {
            state.messages.push(Message { role: "user".into(), content: json!(blocks) });
        }
    }

    fn append_note(&self, state: &mut State, note: &str) {
        if let Some(last) = state.messages.last_mut() {
            if last.role == "user" {
                if let Some(arr) = last.content.as_array_mut() {
                    arr.push(json!({"type": "text", "text": note}));
                }
            }
        }
    }

    fn prune_after_compaction(&self, state: &mut State) {
        state.messages = protocol::prune_messages_for_compaction(&state.messages);
    }

    fn visible_text(&self, response: &Response) -> String {
        let mut text = String::new();
        for block in &response.content {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(part) = block.get("text").and_then(Value::as_str) {
                    text.push_str(part);
                }
            }
        }
        text
    }
}

pub(crate) fn checkpoint_matches_model(state: &State, model: &str) -> bool {
    // 兩邊都必須是支援的 claude-api 模型（如 claude-opus-5-5、claude-fable-5-2）。
    // 請求已帶 thinking.block_binding.prefix_mismatch_behavior = "drop_block"，
    // 支援模型之間的 checkpoint 可安全接續。
    if !am_config::api_bot_model_supported(model) {
        return false;
    }
    if state.responses.iter().any(|r| !am_config::api_bot_model_supported(&r.model)) {
        return false;
    }
    !(state.responses.is_empty()
        && state.messages.iter().any(|m| {
            m.content.as_array().is_some_and(|blocks| {
                blocks.iter().any(|b| {
                    matches!(
                        b.get("type").and_then(Value::as_str),
                        Some("thinking" | "redacted_thinking")
                    )
                })
            })
        }))
}

/// API 明確拒絕、確定沒有處理（也不計費）的錯誤：4xx（408 逾時除外）、429、529 過載。
/// 逾時、連線中斷、回應過大或讀壞都不算：那些不知道上游有沒有處理。
pub(crate) fn definitive_rejection(err: &http::Error) -> bool {
    match err {
        http::Error::RateLimited { .. } => true,
        http::Error::Http { status, .. } => (400..500).contains(status) && *status != 408 || *status == 529,
        _ => false,
    }
}

/// 已成功收到的回應內容不合法的原因（`None`＝合法）；規則與 [`AnthropicCodec::accept`] 的檢查相同。
pub(crate) fn invalid_content_reason(resp: &Response, durable: &State, model: &str) -> Option<String> {
    if resp.model != model {
        return Some(format!("response model {} != requested {model}", resp.model));
    }
    let calls = match resp.tools() {
        Ok(calls) => calls,
        Err(why) => return Some(format!("tools(): {why}")),
    };
    if calls.len() > MAX_TOOL_CALLS_PER_RESPONSE {
        return Some(format!("too many tool calls: {}", calls.len()));
    }
    if let Some(c) = calls.iter().find(|c| durable.completed_tool_ids.contains(&c.id)) {
        return Some(format!("tool id already completed: {}", c.id));
    }
    None
}

pub(crate) struct AnthropicProvider {
    quote_provider: Arc<dyn QuoteProvider>,
}

impl AnthropicProvider {
    pub(crate) fn new(quote_provider: Arc<dyn QuoteProvider>) -> Self {
        Self { quote_provider }
    }
}

impl Provider for AnthropicProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn codec(&self) -> &dyn Codec {
        &AnthropicCodec
    }

    fn encode(&self, options: Options, system: Value, messages: Vec<Message>) -> Result<RequestEnvelope, QuoteError> {
        RequestEnvelope::new(options, system, messages)
    }

    fn send<'a>(
        &'a self,
        transport: Transport<'a>,
        request: &'a RequestEnvelope,
        cancel: &'a CancellationToken,
        progress: Option<&'a (dyn Fn(&StreamProgress) + Send + Sync)>,
    ) -> BoxFuture<'a, Result<Response, http::Error>> {
        Box::pin(async move {
            let req = http::EncodedMessagesRequest {
                endpoint: transport.endpoint,
                key: transport.key,
                request,
                timeout: transport.timeout,
                max_response_bytes: transport.max_response_bytes,
            };
            http::complete_encoded(&req, cancel, progress).await
        })
    }

    fn definitive_rejection(&self, err: &http::Error) -> bool {
        definitive_rejection(err)
    }

    fn invalid_content_reason(&self, response: &Response, durable: &State, model: &str) -> Option<String> {
        invalid_content_reason(response, durable, model)
    }

    fn checkpoint_matches_model(&self, state: &State, model: &str) -> bool {
        checkpoint_matches_model(state, model)
    }

    fn quote_provider(&self) -> Arc<dyn QuoteProvider> {
        self.quote_provider.clone()
    }
}
