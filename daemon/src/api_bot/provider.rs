//! Provider 接點（openai-api 計畫切片 A）：`ApiBotAdapter` 與 runner 只認這份中立契約，
//! 請求編碼、串流終態與 usage、錯誤分類、報價、以及「把中立的工具結果／提示文字轉成原生訊息」都由 provider 實作。
//! 目前只有 Anthropic（現有 http.rs／sse.rs／protocol.rs／quote.rs 的行為原封搬進來；protocol.rs 留作 Anthropic codec）。
//!
//! 不變條件（每個 provider 都要守）：
//! - 報價、算 request hash、實際送出的 body 是**同一份**已編碼請求（[`RequestEnvelope`] 建立時編碼一次、之後只讀）。
//! - runner 不認任何原生格式：它只看可見文字、工具呼叫、工具結果、終止原因；原生 transcript（`State.messages`／`responses`）
//!   由 provider 讀寫，checkpoint 以 `State.provider`／`State.payload_version` 標明格式（無標籤＝舊的 Claude 格式）。
use super::http;
use super::protocol::{self, Message, Options, Response, ToolCall};
use super::quote::{FailClosed, QuoteError, QuoteProvider};
use super::runner::State;
use super::sse::StreamProgress;
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// session 建立時固定的 provider；resume／recovery 驗證 checkpoint 的標籤與它一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderId {
    Anthropic,
}
impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Anthropic => "anthropic",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(ProviderId::Anthropic),
            _ => None,
        }
    }
    /// bot kind 固定 provider（`claude-api` → Anthropic）。
    pub fn for_bot_kind(kind: &str) -> Option<Self> {
        match kind {
            "claude-api" => Some(ProviderId::Anthropic),
            _ => None,
        }
    }
}

/// 現行 checkpoint payload 版本。無標籤的舊 checkpoint 一律按 Anthropic 現行格式讀（等同 v1）。
pub const ANTHROPIC_PAYLOAD_VERSION: u32 = 1;

/// 一次工具執行的結果（中立）：runner 只產這個，provider 轉成原生的 tool_result。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutcome {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// 回應的終止原因（中立）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HaltKind {
    EndTurn,
    Refusal,
    MaxTokens,
    Other(String),
}

/// 錯誤分類：`DefinitiveRejection`＝確定沒有被處理也不計費（記 $0、收掉 pending）；`Unknown`＝可能已處理，維持 unknown。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    DefinitiveRejection,
    Unknown,
}

/// 送出一次請求所需的一切（body 是已編碼、已報價、已算過 hash 的那一份）。沒有 Debug：含私人內容與金鑰。
pub struct SendParams<'a> {
    pub endpoint: &'a str,
    pub key: &'a str,
    pub body: &'a Value,
    pub options: &'a Options,
    pub messages: &'a [Message],
    pub timeout: std::time::Duration,
    pub max_response_bytes: usize,
}

pub type ProgressFn<'a> = Option<&'a (dyn Fn(&StreamProgress) + Send + Sync)>;

pub trait ApiProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn payload_version(&self) -> u32;

    /// 編碼出要送的原生 body（純函式、不碰網路與金鑰）。
    fn encode(&self, options: &Options, system: &Value, messages: &[Message]) -> Result<Value, QuoteError>;
    /// 送出（一個 permit 一次 POST）並解析串流終態與 usage。用 BoxFuture：poll 框不放大 adapter 的 future。
    fn send<'a>(&'a self, params: SendParams<'a>, cancel: &'a CancellationToken, progress: ProgressFn<'a>) -> BoxFuture<'a, Result<Response, http::Error>>;
    fn classify_error(&self, err: &http::Error) -> ErrorClass;
    fn quote(&self) -> Arc<dyn QuoteProvider>;

    // ── runner 用的中立操作 ──
    fn tool_calls(&self, response: &Response) -> Result<Vec<ToolCall>, &'static str>;
    fn visible_text(&self, response: &Response) -> String;
    fn assistant_message(&self, response: &Response) -> Message;
    fn tool_results_message(&self, outcomes: &[ToolOutcome]) -> Message;
    /// 在最後一則（工具結果）user 訊息尾端補一段提示文字；那一則不是可附加的形狀就不加並回 false。
    fn append_notice(&self, message: &mut Message, text: &str) -> bool;
    fn halt_kind(&self, response: &Response) -> HaltKind;
    /// 伺服器端工具暫停後要不要自動重送延續（Anthropic 的 `pause_turn`）。
    fn continues_after(&self, response: &Response) -> bool;
    /// 回應含壓縮區塊時，壓縮後 transcript 要修剪（回傳 `Some` 才要改寫並存 checkpoint）。
    fn prune_after_response(&self, response: &Response, messages: &[Message]) -> Option<Vec<Message>>;
}

/// 舊 checkpoint 沒有標籤＝Anthropic v1；有標籤就必須與目前 provider 一致、版本不得比目前新。
pub fn check_checkpoint_tag(state: &State, provider: &dyn ApiProvider) -> Result<(), ()> {
    if let Some(tag) = &state.provider {
        if ProviderId::parse(tag) != Some(provider.id()) {
            return Err(());
        }
    }
    if state.payload_version.is_some_and(|v| v == 0 || v > provider.payload_version()) {
        return Err(());
    }
    Ok(())
}

/// 把標籤寫進 checkpoint（只補沒有的；有的已由 [`check_checkpoint_tag`] 驗證過）。
pub fn stamp_checkpoint(state: &mut State, provider: &dyn ApiProvider) {
    state.provider.get_or_insert_with(|| provider.id().as_str().to_string());
    state.payload_version.get_or_insert(provider.payload_version());
}

// ────────────────────────────── Anthropic ──────────────────────────────

/// Anthropic Messages API：現有 protocol.rs／http.rs／sse.rs 的行為，一字不改地放在接點後面。
pub struct AnthropicProvider {
    quote: Option<Arc<dyn QuoteProvider>>,
}
impl AnthropicProvider {
    pub fn new(quote: Arc<dyn QuoteProvider>) -> Self {
        Self { quote: Some(quote) }
    }
    /// 只有編碼／轉換，沒有報價（報價一律 fail-closed）：給不經 adapter 的 runner 測試用的預設值。
    pub const fn codec_only() -> Self {
        Self { quote: None }
    }
}

/// `Model::provider()` 的預設值：只做 Anthropic 編碼與中立轉換。
pub static DEFAULT_ANTHROPIC: AnthropicProvider = AnthropicProvider::codec_only();

impl ApiProvider for AnthropicProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Anthropic
    }
    fn payload_version(&self) -> u32 {
        ANTHROPIC_PAYLOAD_VERSION
    }
    fn encode(&self, options: &Options, system: &Value, messages: &[Message]) -> Result<Value, QuoteError> {
        options.validate().map_err(|_| QuoteError::InvalidRequest)?;
        if messages.is_empty() {
            return Err(QuoteError::InvalidRequest);
        }
        Ok(protocol::request(options, system, messages))
    }
    fn send<'a>(&'a self, params: SendParams<'a>, cancel: &'a CancellationToken, progress: ProgressFn<'a>) -> BoxFuture<'a, Result<Response, http::Error>> {
        Box::pin(async move {
            let no_system = Value::Null; // body 已編碼；system 只用於編碼，送出時不再重組
            let req = http::FullMessagesRequest {
                endpoint: params.endpoint,
                key: params.key,
                options: params.options,
                system: &no_system,
                messages: params.messages,
                timeout: params.timeout,
                max_response_bytes: params.max_response_bytes,
            };
            http::complete_encoded(&req, params.body, cancel, progress).await
        })
    }
    fn classify_error(&self, err: &http::Error) -> ErrorClass {
        if super::adapter::definitive_rejection(err) {
            ErrorClass::DefinitiveRejection
        } else {
            ErrorClass::Unknown
        }
    }
    fn quote(&self) -> Arc<dyn QuoteProvider> {
        self.quote.clone().unwrap_or_else(|| Arc::new(FailClosed))
    }
    fn tool_calls(&self, response: &Response) -> Result<Vec<ToolCall>, &'static str> {
        response.tools()
    }
    fn visible_text(&self, response: &Response) -> String {
        super::runner::final_text(response)
    }
    fn assistant_message(&self, response: &Response) -> Message {
        Message { role: "assistant".into(), content: json!(response.content) }
    }
    fn tool_results_message(&self, outcomes: &[ToolOutcome]) -> Message {
        let blocks: Vec<Value> = outcomes
            .iter()
            .map(|o| json!({"type": "tool_result", "tool_use_id": o.call_id, "content": o.content, "is_error": o.is_error}))
            .collect();
        Message { role: "user".into(), content: json!(blocks) }
    }
    fn append_notice(&self, message: &mut Message, text: &str) -> bool {
        if message.role != "user" {
            return false;
        }
        match message.content.as_array_mut() {
            Some(arr) => {
                arr.push(json!({"type": "text", "text": text}));
                true
            }
            None => false,
        }
    }
    fn halt_kind(&self, response: &Response) -> HaltKind {
        match response.stop_reason.as_str() {
            "end_turn" => HaltKind::EndTurn,
            "refusal" => HaltKind::Refusal,
            "max_tokens" => HaltKind::MaxTokens,
            other => HaltKind::Other(other.to_string()),
        }
    }
    fn continues_after(&self, response: &Response) -> bool {
        // Anthropic 伺服器端工具（web_search、web_fetch 等）執行滿 server-side iterations 時以 `pause_turn` 暫停；自動重送延續。
        response.stop_reason == "pause_turn"
            && response.content.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("server_tool_use"))
    }
    fn prune_after_response(&self, response: &Response, messages: &[Message]) -> Option<Vec<Message>> {
        let has_compaction = response.content.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("compaction"));
        has_compaction.then(|| protocol::prune_messages_for_compaction(messages))
    }
}
