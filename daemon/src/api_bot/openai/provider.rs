//! OpenAI Responses API 接上 provider 接點（openai-api 切片 D1）。**尚無 production caller**：不新增 bot kind、
//! 不碰 native start／recovery／設定／帳本；key 由呼叫端以 [`Transport`] 傳入。
//!
//! Transcript（`runner::State::messages`）存的是 OpenAI 原生的 input items，一筆一個 [`Message`]：
//!
//! | `role` | `content` | 送出時 |
//! |---|---|---|
//! | `user` | 字串 | `message`（`input_text`） |
//! | `item` | 上一輪原樣的 output item（`message`／`reasoning`／`function_call`） | 原樣重播（含 `encrypted_content`） |
//! | `tool_results` | `{"results":[{call_id,output,is_error}], "note": 可省}` | 每筆一個 `function_call_output {call_id, output}`，`note` 接在後面當一則 user 訊息 |
//!
//! 每次請求都從完整 transcript 重編（`store:false`，不用 `previous_response_id`），所以重啟後從 checkpoint 接續時
//! 編出來的位元組與沒中斷時相同。加密 reasoning 只留在 `item` 的內容裡續傳：不進可見文字，本檔的型別沒有會印出內容的 `Debug`。
use super::codec::{self, CodecError, ErrorClass, InputItem, Outcome, ReplayItem, ResponseAssembler, ToolSpec, Update};
use super::pricing::{self, Usage};
use crate::api_bot::http;
use crate::api_bot::protocol::{Message, Response, ToolCall};
use crate::api_bot::provider::{Codec, InvalidResponse, Provider, ProviderExtras, SessionOptions, StopReason, ToolResult, Transport, TurnOutput, MAX_TOOL_CALLS_PER_RESPONSE};
use crate::api_bot::quote::{QuoteError, QuoteProvider, QuotedRequest, ReservationQuote, UsageQuote};
use crate::api_bot::runner::State;
use crate::api_bot::sse::{StreamProgress, TAIL_CHARS};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const ID: &str = "openai";
/// 官方主機與 endpoint：正式建置只准連這一台的 443。
pub const OFFICIAL_HOST: &str = "api.openai.com";
pub const OFFICIAL_ENDPOINT: &str = "https://api.openai.com/v1/responses";

/// 本機執行、宣告給模型的 function tools（與 Claude 那邊同一組執行器，參數欄位相同）。
pub const CLIENT_TOOL_NAMES: &[&str] = &["bash", "str_replace_based_edit_tool"];

const ROLE_USER: &str = "user";
const ROLE_ITEM: &str = "item";
const ROLE_TOOL_RESULTS: &str = "tool_results";
/// 第一版只有標準計價層級（寫進 attempt 的 `speed`）。
const SERVICE_TIER: &str = "standard";
/// `Response::stop_reason` 的值（這個 provider 自己的字彙，只有本檔會解讀）。
const STOP_COMPLETED: &str = "completed";
const STOP_REFUSAL: &str = "refusal";
const STOP_MAX_OUTPUT: &str = "incomplete:max_output_tokens";

fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command in the bot's workspace and return its output.".into(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
        },
        ToolSpec {
            name: "str_replace_based_edit_tool".into(),
            description: "View, create or edit a text file. `command` is one of view, create, str_replace, insert.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["view", "create", "str_replace", "insert"]},
                    "path": {"type": "string"},
                    "file_text": {"type": "string"},
                    "old_str": {"type": "string"},
                    "new_str": {"type": "string"},
                    "insert_line": {"type": "integer"},
                    "view_range": {"type": "array", "items": {"type": "integer"}}
                },
                "required": ["command", "path"]
            }),
        },
    ]
}

/// transcript → 要送的 input items。出現不認得的形狀（例如 Claude 的 transcript）就拒絕，不猜。
fn input_items(transcript: &[Message]) -> Result<Vec<InputItem>, CodecError> {
    let bad = |why: &str| CodecError::BadReplayItem(why.to_string());
    let mut items = Vec::new();
    for entry in transcript {
        match entry.role.as_str() {
            ROLE_USER => items.push(InputItem::User(entry.content.as_str().ok_or_else(|| bad("user entry is not text"))?.to_string())),
            ROLE_ITEM => items.push(InputItem::Replay(ReplayItem::new(entry.content.clone())?)),
            ROLE_TOOL_RESULTS => {
                let results = entry.content.get("results").and_then(Value::as_array).ok_or_else(|| bad("tool results entry without results"))?;
                for result in results {
                    let call_id = result.get("call_id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| bad("tool result without call_id"))?;
                    let output = result.get("output").and_then(Value::as_str).ok_or_else(|| bad("tool result without output"))?;
                    items.push(InputItem::ToolOutput { call_id: call_id.to_string(), output: output.to_string() });
                }
                if let Some(note) = entry.content.get("note").and_then(Value::as_str) {
                    items.push(InputItem::User(note.to_string()));
                }
            }
            _ => return Err(bad("transcript entry is not an OpenAI item")),
        }
    }
    Ok(items)
}

/// 回應裡要執行的工具呼叫（照順序）。`call_id` 重複、不是宣告過的工具、參數不是 JSON 物件都不採用。
fn tool_calls(response: &Response) -> Result<Vec<ToolCall>, &'static str> {
    let mut calls = Vec::new();
    let mut seen = HashSet::new();
    for item in &response.content {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let call_id = item.get("call_id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("function_call without call_id")?;
                let name = item.get("name").and_then(Value::as_str).ok_or("function_call without name")?;
                if !seen.insert(call_id.to_string()) {
                    return Err("duplicate call_id");
                }
                if !CLIENT_TOOL_NAMES.contains(&name) {
                    return Err("unsupported tool");
                }
                let input: Value = serde_json::from_str(item.get("arguments").and_then(Value::as_str).unwrap_or("")).map_err(|_| "tool arguments are not JSON")?;
                if !input.is_object() {
                    return Err("tool arguments are not an object");
                }
                calls.push(ToolCall { id: call_id.to_string(), name: name.to_string(), input });
            }
            Some("message" | "reasoning") => {}
            _ => return Err("unsupported output item"),
        }
    }
    // 未完成的回應（被 max_output_tokens 截斷等）永遠不執行工具。
    if response.stop_reason != STOP_COMPLETED && !calls.is_empty() {
        return Err("tool call in a response that did not complete");
    }
    Ok(calls)
}

/// OpenAI 的 transcript 編解碼。
pub struct OpenAiCodec;

impl Codec for OpenAiCodec {
    fn accept(&self, state: &mut State, response: Response) -> Result<TurnOutput, InvalidResponse> {
        let calls = tool_calls(&response).map_err(|_| InvalidResponse)?;
        if calls.len() > MAX_TOOL_CALLS_PER_RESPONSE || calls.iter().any(|c| state.completed_tool_ids.contains(&c.id)) {
            return Err(InvalidResponse);
        }
        // 重播用的 item 先全部驗過，transcript 才動（失敗時 state 不變）。
        let mut entries = Vec::with_capacity(response.content.len());
        for item in &response.content {
            ReplayItem::new(item.clone()).map_err(|_| InvalidResponse)?;
            entries.push(Message { role: ROLE_ITEM.into(), content: item.clone() });
        }
        let stop = match response.stop_reason.as_str() {
            STOP_COMPLETED => StopReason::EndTurn,
            STOP_REFUSAL => StopReason::Refusal,
            STOP_MAX_OUTPUT => StopReason::MaxTokens,
            other => StopReason::Other(other.to_string()),
        };
        state.messages.extend(entries);
        state.responses.push(response);
        Ok(TurnOutput { calls, stop, compacted: false, resumable_pause: false })
    }

    fn put_tool_results(&self, state: &mut State, results: &[ToolResult], note: Option<&str>, replace_last: bool) {
        let mut content = json!({
            "results": results.iter().map(|r| json!({"call_id": r.call_id, "output": r.output, "is_error": r.is_error})).collect::<Vec<_>>(),
        });
        if let Some(note) = note {
            content["note"] = json!(note);
        }
        if replace_last {
            state.messages.last_mut().expect("tool result batch exists").content = content;
        } else {
            state.messages.push(Message { role: ROLE_TOOL_RESULTS.into(), content });
        }
    }

    fn append_note(&self, state: &mut State, note: &str) {
        if let Some(last) = state.messages.last_mut() {
            if last.role == ROLE_TOOL_RESULTS && last.content.is_object() {
                let joined = match last.content.get("note").and_then(Value::as_str) {
                    Some(existing) => format!("{existing}\n{note}"),
                    None => note.to_string(),
                };
                last.content["note"] = json!(joined);
            }
        }
    }

    fn prune_after_compaction(&self, _state: &mut State) {}

    fn unpaired_tool_calls(&self, state: &State) -> Result<Vec<String>, ()> {
        let mut declared: Vec<String> = Vec::new();
        let mut returned = HashSet::new();
        for entry in &state.messages {
            match entry.role.as_str() {
                ROLE_ITEM if entry.content.get("type").and_then(Value::as_str) == Some("function_call") => {
                    declared.push(entry.content.get("call_id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or(())?.to_string());
                }
                ROLE_TOOL_RESULTS => {
                    for result in entry.content.get("results").and_then(Value::as_array).ok_or(())? {
                        returned.insert(result.get("call_id").and_then(Value::as_str).ok_or(())?.to_string());
                    }
                }
                ROLE_USER | ROLE_ITEM => {}
                _ => return Err(()),
            }
        }
        Ok(declared.into_iter().filter(|id| !returned.contains(id)).collect())
    }

    /// Responses API 要求每個 `function_call` 都有對應的 `function_call_output`：被中斷的那一批裡**每一個**還沒有結果的呼叫都補上
    /// （不只 `tool_pending` 那一個），不然下一次請求在送出前就會被配對檢查擋下。
    fn settle_unpaired_tool_calls(&self, state: &mut State, note: &str) -> usize {
        let Ok(unpaired) = self.unpaired_tool_calls(state) else { return 0 };
        if unpaired.is_empty() {
            return 0;
        }
        let added: Vec<Value> = unpaired.iter().map(|id| json!({"call_id": id, "output": note, "is_error": true})).collect();
        // 這一批已經有部分結果（最後一筆就是 tool_results）就接在後面；否則開一筆新的。
        match state.messages.last_mut().filter(|m| m.role == ROLE_TOOL_RESULTS).and_then(|m| m.content.get_mut("results")).and_then(Value::as_array_mut) {
            Some(results) => results.extend(added),
            None => state.messages.push(Message { role: ROLE_TOOL_RESULTS.into(), content: json!({ "results": added }) }),
        }
        state.completed_tool_ids.extend(unpaired.iter().cloned());
        unpaired.len()
    }

    fn visible_text(&self, response: &Response) -> String {
        let mut text = String::new();
        for item in &response.content {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue; // reasoning（含 encrypted_content）與 function_call 不是給使用者看的
            }
            for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                match part.get("type").and_then(Value::as_str) {
                    Some("output_text") => text.push_str(part.get("text").and_then(Value::as_str).unwrap_or("")),
                    Some("refusal") => text.push_str(part.get("refusal").and_then(Value::as_str).unwrap_or("")),
                    _ => {}
                }
            }
        }
        text
    }
}

/// 一份已編碼、不可變的 Responses 請求：報價、hash、送出都用這一份位元組。刻意沒有 `Debug`／`Serialize`（內容是私密的）。
pub(crate) struct OpenAiRequest {
    wire: Vec<u8>,
    hash: String,
    model: String,
    max_output_tokens: u64,
    /// 輸入 token 的可證上界（`pricing::input_token_bound`：body 位元組數加固定餘裕，以 context window 封頂）。
    input_bound: u64,
}

impl OpenAiRequest {
    pub(crate) fn wire(&self) -> &[u8] {
        &self.wire
    }
    fn of(request: &dyn QuotedRequest) -> Result<&OpenAiRequest, QuoteError> {
        request.as_any().downcast_ref::<OpenAiRequest>().ok_or(QuoteError::InvalidRequest)
    }
    fn input_token_bound(&self) -> u64 {
        self.input_bound
    }
}

impl QuotedRequest for OpenAiRequest {
    fn hash(&self) -> &str {
        &self.hash
    }
    fn speed(&self) -> &'static str {
        SERVICE_TIER
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `Response::usage` 裡存的正規化用量（由 [`usage_value`] 產生）；`null`＝上游沒給用量。
fn usage_of(response: &Response) -> Result<Usage, QuoteError> {
    let u = response.usage.as_object().ok_or(QuoteError::UntrustedUsage)?;
    let field = |key: &str| u.get(key).and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage);
    let usage = Usage {
        input_tokens: field("input_tokens")?,
        cached_tokens: field("cached_tokens")?,
        cache_write_tokens: field("cache_write_tokens")?,
        cache_write_12h_tokens: field("cache_write_12h_tokens")?,
        output_tokens: field("output_tokens")?,
        reasoning_tokens: field("reasoning_tokens")?,
    };
    if usage.cached_tokens.checked_add(usage.cache_write_tokens).is_none_or(|n| n > usage.input_tokens)
        || usage.cache_write_12h_tokens > usage.cache_write_tokens
        || usage.reasoning_tokens > usage.output_tokens
    {
        return Err(QuoteError::UntrustedUsage);
    }
    Ok(usage)
}

/// 上游回報的模型是不是我們要求的那一個：完全相同，或是它帶日期／版本的快照名（`<要求的>-…`）。
fn model_matches(requested: &str, upstream: &str) -> bool {
    upstream == requested || upstream.strip_prefix(requested).is_some_and(|rest| rest.starts_with('-'))
}

fn usage_value(usage: &Usage, service_tier: Option<&str>) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "cached_tokens": usage.cached_tokens,
        "cache_write_tokens": usage.cache_write_tokens,
        "cache_write_12h_tokens": usage.cache_write_12h_tokens,
        "output_tokens": usage.output_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
        "service_tier": service_tier,
    })
}

/// 標準層級的報價：預留用可證上界（`pricing::quote_upper_bound_microusd`），結算用實際用量。純計算，不做 I/O。
pub(crate) struct OpenAiQuote;

impl QuoteProvider for OpenAiQuote {
    fn reserve(&self, request: &dyn QuotedRequest) -> Result<ReservationQuote, QuoteError> {
        let request = OpenAiRequest::of(request)?;
        let table = pricing::price_table(&request.model).ok_or(QuoteError::Unapproved)?;
        // 上界報價：輸入上界全部以最高的輸入單價計（含 12 小時快取寫入）、輸出用滿 max_output_tokens，超過長文門檻用長文價。
        let reserved = match pricing::quote_upper_bound(&request.model, request.input_token_bound(), request.max_output_tokens) {
            Ok(bound) => i64::try_from(bound).map_err(|_| QuoteError::InvalidRequest)?,
            Err(pricing::PriceError::UnknownModel) => return Err(QuoteError::Unapproved),
            Err(_) => return Err(QuoteError::InvalidRequest),
        };
        Ok(ReservationQuote { reserved_microusd: reserved.max(1), pricing_version: format!("{}-{SERVICE_TIER}-{}", table.model, table.effective_date) })
    }

    fn settle(&self, request: &dyn QuotedRequest, response: &Response) -> Result<UsageQuote, QuoteError> {
        let request = OpenAiRequest::of(request)?;
        pricing::price_table(&request.model).ok_or(QuoteError::Unapproved)?;
        if response.model != request.model {
            return Err(QuoteError::UntrustedUsage);
        }
        let usage = usage_of(response)?;
        if usage.input_tokens > request.input_token_bound() || usage.output_tokens > request.max_output_tokens {
            return Err(QuoteError::UntrustedUsage);
        }
        // 回報的 service tier 不是標準層級就沒有核准的價（UnsupportedTier）：一樣是費用未知，不照標準價算。
        let tier = response.usage.get("service_tier").and_then(Value::as_str);
        let cost = pricing::settle(&request.model, &usage, tier).ok().and_then(|c| i64::try_from(c).ok()).ok_or(QuoteError::UntrustedUsage)?;
        Ok(UsageQuote { estimated_microusd: cost, actual_speed: Some(SERVICE_TIER) })
    }
}

/// 送出後的失敗對應到共用的傳輸錯誤。
fn http_error(status: u16, body: &str, retry_after: Option<&str>, request_id: Option<String>) -> http::Error {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let message = parsed
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(|m| crate::judge::mask(m).chars().take(500).collect::<String>())
        .filter(|m| !m.is_empty());
    let code = parsed.pointer("/error/code").and_then(Value::as_str).map(str::to_string).filter(|c| !c.is_empty());
    match codec::classify_http(status, body, retry_after) {
        ErrorClass::RateLimited { retry_after_secs } => http::Error::RateLimited { retry_after_secs },
        ErrorClass::InsufficientQuota => http::Error::Http { status, kind: Some("insufficient_quota".into()), message, request_id },
        ErrorClass::Auth => http::Error::Http { status, kind: Some("authentication_error".into()), message, request_id },
        ErrorClass::Permission => http::Error::Http { status, kind: Some("permission_error".into()), message, request_id },
        ErrorClass::ModelRejected { code } | ErrorClass::InvalidRequest { code } => http::Error::Http { status, kind: Some(code).filter(|c| !c.is_empty()), message, request_id },
        ErrorClass::Unknown => http::Error::Http { status, kind: code, message, request_id },
    }
}

/// HTTP 200 之後串流組不起來：都不是確定沒處理的拒絕。completed 卻沒有可信的 usage 另外標明——內容不採用、費用未知。
fn codec_error(err: CodecError) -> http::Error {
    match err {
        CodecError::MissingUsage => http::Error::InvalidResponseDetail("openai response completed without usage".into()),
        _ => http::Error::InvalidResponse,
    }
}

/// `[openai_api]` 能不能用：總開關要開、key 檔要讀得到。不能用時回原因（不含 key），呼叫端回具名錯誤 `openai_not_configured`。
pub(crate) fn configured(cfg: &crate::config::ConfigFile) -> Result<(), String> {
    if !cfg.openai_api.enabled {
        return Err("[openai_api] enabled 沒有開".into());
    }
    crate::key_file::key_status(&cfg.openai_api.key_file)
}

pub(crate) struct OpenAiProvider {
    model: String,
    effort: String,
    max_output_tokens: u64,
    quote_provider: Arc<dyn QuoteProvider>,
}

/// provider 建不起來的具名原因（不含使用者文字；細節給 WARN log，stable reason 給 turn 的 `start_error`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProviderInitError {
    Extras,
    MaxOutputTokens,
    UnsupportedModel(String),
    UnsupportedEffort { model: String, effort: String },
    NoPriceTable(String),
}

impl ProviderInitError {
    /// 穩定的代碼（給 turn 的 `start_error` 與 log 的 reason 欄位用）。
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::Extras => "extras_unsupported",
            Self::MaxOutputTokens => "max_output_tokens_zero",
            Self::UnsupportedModel(_) => "unsupported_model",
            Self::UnsupportedEffort { .. } => "unsupported_effort",
            Self::NoPriceTable(_) => "no_price_table",
        }
    }
}

impl OpenAiProvider {
    /// `options.extras` 必須是 `None`（OpenAI 第一版沒有專屬設定）；模型與 effort 要在 am-config 的清單裡（唯一來源）。
    pub(crate) fn new(options: SessionOptions, quote_provider: Arc<dyn QuoteProvider>) -> Result<Self, ProviderInitError> {
        if options.extras != ProviderExtras::None {
            return Err(ProviderInitError::Extras);
        }
        if options.max_output_tokens == 0 {
            return Err(ProviderInitError::MaxOutputTokens);
        }
        let effort = codec::resolve_effort(&options.model, Some(&options.effort)).map_err(|e| match e {
            CodecError::UnsupportedEffort { model, effort } => ProviderInitError::UnsupportedEffort { model, effort },
            _ => ProviderInitError::UnsupportedModel(options.model.clone()),
        })?;
        if pricing::price_table(&options.model).is_none() {
            return Err(ProviderInitError::NoPriceTable(options.model.clone()));
        }
        Ok(Self { model: options.model, effort: effort.to_string(), max_output_tokens: u64::from(options.max_output_tokens), quote_provider })
    }

    /// 用內建標準價表報價的 provider。
    pub(crate) fn with_standard_pricing(options: SessionOptions) -> Result<Self, ProviderInitError> {
        Self::new(options, Arc::new(OpenAiQuote))
    }
}

impl OpenAiProvider {
    /// 這份 transcript 對應的 codec 請求（還沒編碼）。[`Provider::encode`] 編的就是它；測試拿它對 `pricing::quote_request` 核對報價。
    pub(crate) fn request_for(&self, effort: &str, system: &Value, transcript: &[Message]) -> Result<codec::Request, QuoteError> {
        if transcript.is_empty() {
            return Err(QuoteError::InvalidRequest);
        }
        let instructions = match system {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        Ok(codec::Request {
            model: self.model.clone(),
            instructions,
            input: input_items(transcript).map_err(|_| QuoteError::InvalidRequest)?,
            tools: tool_specs(),
            effort: Some(effort.to_string()),
            max_output_tokens: self.max_output_tokens,
        })
    }
}

impl Provider for OpenAiProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn codec(&self) -> &dyn Codec {
        &OpenAiCodec
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn effort(&self) -> &str {
        &self.effort
    }

    fn lower_effort(&self, effort: &str) -> String {
        let list = codec::efforts_for_model(&self.model).unwrap_or(&[]);
        match list.iter().position(|e| *e == effort) {
            Some(i) if i > 0 => list[i - 1].to_string(),
            _ => effort.to_string(),
        }
    }

    fn official_endpoint(&self) -> &'static str {
        OFFICIAL_ENDPOINT
    }

    fn encode(&self, effort: &str, system: &Value, transcript: &[Message], _previous: Option<&Response>) -> Result<Arc<dyn QuotedRequest>, QuoteError> {
        let request = self.request_for(effort, system, transcript)?;
        // 配對錯、重複 call_id、不支援的 effort 都在這裡（HTTP 之前）拒絕。
        let wire = request.body().map_err(|_| QuoteError::InvalidRequest)?;
        let hash = format!("{:x}", Sha256::digest(&wire));
        // 位元組上界只用來預留，超過 context window 不在這裡拒絕（上游會拒絕，且不計費）。
        let input_bound = pricing::input_token_bound(&self.model, wire.len()).map_err(|_| QuoteError::InvalidRequest)?;
        Ok(Arc::new(OpenAiRequest { wire, hash, model: self.model.clone(), max_output_tokens: self.max_output_tokens, input_bound }))
    }

    fn send<'a>(
        &'a self,
        transport: Transport<'a>,
        request: &'a dyn QuotedRequest,
        cancel: &'a CancellationToken,
        progress: Option<&'a (dyn Fn(&StreamProgress) + Send + Sync)>,
    ) -> BoxFuture<'a, Result<Response, http::Error>> {
        Box::pin(async move {
            let request = OpenAiRequest::of(request).map_err(|_| http::Error::InvalidRequest)?;
            post(&transport, request, cancel, progress).await
        })
    }

    fn running_cost_microusd(&self, _progress: &StreamProgress) -> Option<i64> {
        None
    }

    fn usage_problem(&self, request: &dyn QuotedRequest, response: &Response) -> Option<String> {
        let request = OpenAiRequest::of(request).ok()?;
        if response.model != request.model {
            return Some(format!("回應模型與請求不符：{} != {}", response.model, request.model));
        }
        if response.usage.is_null() {
            return Some("回應沒有帶用量（usage），費用未知".into());
        }
        if let Some(tier) = response.usage.get("service_tier").and_then(Value::as_str).filter(|t| !pricing::is_standard_tier(Some(t))) {
            return Some(format!("回應的 service tier（{tier}）不是標準層級，沒有核准的價"));
        }
        let Ok(usage) = usage_of(response) else { return Some("用量欄位缺漏或互相矛盾".into()) };
        if usage.input_tokens > request.input_token_bound() {
            return Some(format!("輸入 token ({}) 超出請求的上界 ({})", usage.input_tokens, request.input_token_bound()));
        }
        if usage.output_tokens > request.max_output_tokens {
            return Some(format!("輸出 token ({}) 超出 max_output_tokens ({})", usage.output_tokens, request.max_output_tokens));
        }
        None
    }

    /// 上游明確拒絕、確定沒有處理：4xx（408／409／425 這種送沒送到不確定的除外）與 429。5xx、逾時、斷流、
    /// HTTP 200 之後的串流錯誤都不算。
    fn definitive_rejection(&self, err: &http::Error) -> bool {
        match err {
            http::Error::RateLimited { .. } => true,
            http::Error::Http { status, .. } => (400..500).contains(status) && !matches!(*status, 408 | 409 | 425),
            _ => false,
        }
    }

    fn invalid_content_reason(&self, response: &Response, durable: &State) -> Option<String> {
        if response.model != self.model {
            return Some(format!("response model {} != requested {}", response.model, self.model));
        }
        let calls = match tool_calls(response) {
            Ok(calls) => calls,
            Err(why) => return Some(format!("tool calls: {why}")),
        };
        if calls.len() > MAX_TOOL_CALLS_PER_RESPONSE {
            return Some(format!("too many tool calls: {}", calls.len()));
        }
        if let Some(c) = calls.iter().find(|c| durable.completed_tool_ids.contains(&c.id)) {
            return Some(format!("tool id already completed: {}", c.id));
        }
        None
    }

    fn checkpoint_matches_model(&self, state: &State) -> bool {
        state.provider_id() == ID && state.responses.iter().all(|r| codec::efforts_for_model(&r.model).is_some())
    }

    fn quote_provider(&self) -> Arc<dyn QuoteProvider> {
        self.quote_provider.clone()
    }
}

/// 送出一份已編碼的請求並把串流組成終態。只有這裡碰網路；不印 body、不印 key。
/// 錯誤裡的文字（上游 `message`、`request_id` 等）會先把 key 換成 `<redacted>`：上游 401 有時會把 key 回吐在錯誤訊息裡，
/// 而錯誤會一路流到 log、WS 與使用者看得到的通知，不能靠「上游不會那樣做」。
async fn post(
    transport: &Transport<'_>,
    request: &OpenAiRequest,
    cancel: &CancellationToken,
    progress: Option<&(dyn Fn(&StreamProgress) + Send + Sync)>,
) -> Result<Response, http::Error> {
    post_inner(transport, request, cancel, progress).await.map_err(|e| scrub_error(e, transport.key))
}

/// 把錯誤裡任何出現的 key 換掉（key 空字串時不動，免得每個字元之間都被插進去）。
pub(crate) fn scrub_error(e: http::Error, key: &str) -> http::Error {
    let clean = |s: Option<String>| match s {
        Some(v) if !key.is_empty() => Some(v.replace(key, "<redacted>")),
        other => other,
    };
    match e {
        http::Error::Http { status, kind, message, request_id } => {
            http::Error::Http { status, kind: clean(kind), message: clean(message), request_id: clean(request_id) }
        }
        http::Error::StreamAborted { kind, message } => http::Error::StreamAborted { kind: clean(kind), message: clean(message) },
        http::Error::InvalidResponseDetail(v) if !key.is_empty() => http::Error::InvalidResponseDetail(v.replace(key, "<redacted>")),
        other => other,
    }
}

async fn post_inner(
    transport: &Transport<'_>,
    request: &OpenAiRequest,
    cancel: &CancellationToken,
    progress: Option<&(dyn Fn(&StreamProgress) + Send + Sync)>,
) -> Result<Response, http::Error> {
    use http::Error;
    if transport.key.trim().is_empty() || transport.timeout.is_zero() || !(1..=8 * 1024 * 1024).contains(&transport.max_response_bytes) {
        return Err(Error::InvalidRequest);
    }
    let url = reqwest::Url::parse(transport.endpoint).map_err(|_| Error::InvalidRequest)?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(Error::InvalidRequest);
    }
    #[cfg(not(test))]
    if url.scheme() != "https" || url.host_str() != Some(OFFICIAL_HOST) || url.port_or_known_default() != Some(443) {
        return Err(Error::InvalidRequest);
    }
    // 測試只連得到字面上的 loopback；DNS 名稱與真的 API endpoint 一律拒絕。
    #[cfg(test)]
    if url.scheme() != "http" || !url.host_str().is_some_and(|h| h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())) {
        return Err(Error::InvalidRequest);
    }
    // 不跟隨任何 redirect（跨站 redirect 會把 Authorization 帶去別處）。
    let builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(transport.timeout);
    #[cfg(test)]
    let builder = builder.no_proxy();
    let client = builder.build().map_err(|_| Error::InvalidRequest)?;
    let transport_error = |e: reqwest::Error| if e.is_timeout() { Error::Timeout } else { Error::TransportUnknown };
    let operation = async {
        let mut response = client
            .post(url)
            .bearer_auth(transport.key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .body(request.wire.clone())
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            let (retry_after, request_id) = (header("retry-after"), header("x-request-id"));
            let mut bytes = Vec::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                let take = chunk.len().min(4096_usize.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&chunk[..take]);
                if bytes.len() >= 4096 {
                    break;
                }
            }
            let err = http_error(status, &String::from_utf8_lossy(&bytes), retry_after.as_deref(), request_id);
            if let Error::Http { status, kind, request_id, .. } = &err {
                tracing::warn!(status, kind = ?kind, request_id = ?request_id, "openai api http error response");
            }
            return Err(err);
        }
        let mut assembler = ResponseAssembler::new();
        let mut shown = StreamProgress::default();
        let mut last_report = tokio::time::Instant::now();
        let mut total = 0_usize;
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            total = total.saturating_add(chunk.len());
            if total > transport.max_response_bytes {
                return Err(Error::TooLarge);
            }
            let updates = assembler.feed(&chunk).map_err(codec_error)?;
            if let Some(report) = progress {
                // 與 Claude（http.rs）同一個節奏：每個 chunk 都看一眼，距上次報告滿 PROGRESS_INTERVAL 才報；
                // 文字、思考（reasoning 項目開始）、函式呼叫參數都算進度，尾端文字與 Claude 一樣只留 TAIL_CHARS。
                for update in updates {
                    match update {
                        Update::TextDelta(delta) => {
                            shown.generated_chars += delta.chars().count();
                            shown.text_tail.push_str(&delta);
                        }
                        Update::Activity(bytes) => shown.other_bytes += bytes,
                    }
                }
                if last_report.elapsed() >= http::PROGRESS_INTERVAL {
                    last_report = tokio::time::Instant::now();
                    let keep = shown.text_tail.chars().count().saturating_sub(TAIL_CHARS);
                    shown.text_tail = shown.text_tail.chars().skip(keep).collect();
                    report(&shown);
                }
            }
        }
        let (output, stop_reason) = match assembler.finish().map_err(codec_error)? {
            Outcome::Completed(output) => {
                let stop = if output.stop == codec::StopKind::Refusal { STOP_REFUSAL } else { STOP_COMPLETED };
                (output, stop.to_string())
            }
            Outcome::Incomplete { reason, output, .. } => {
                let reason = match reason {
                    codec::IncompleteReason::MaxOutputTokens => "max_output_tokens".to_string(),
                    codec::IncompleteReason::ContentFilter => "content_filter".to_string(),
                    codec::IncompleteReason::Other(other) => other,
                };
                (output, format!("incomplete:{reason}"))
            }
            // HTTP 200 之後才失敗：模型可能已經產生並計費，不是確定沒處理的拒絕。
            Outcome::Failed { code, message, .. } => {
                let message = Some(crate::judge::mask(&message).chars().take(500).collect::<String>()).filter(|m| !m.is_empty());
                return Err(Error::StreamAborted { kind: Some(code).filter(|c| !c.is_empty()), message });
            }
            Outcome::Truncated => return Err(Error::TransportUnknown),
        };
        // 上游實際回報的模型：是我們要求的（或它的快照名）就記成要求的那個 id，原名留在 usage 的 `upstream_model`；
        // 回報了別的模型就原樣帶出去，後面的內容檢查（`invalid_content_reason`）與結算會拒絕它。沒回報＝當作要求的那個。
        let upstream_model = output.model.clone();
        let model = match &upstream_model {
            Some(upstream) if !model_matches(&request.model, upstream) => upstream.clone(),
            _ => request.model.clone(),
        };
        let mut usage = output.usage.as_ref().map(|usage| usage_value(usage, output.service_tier.as_deref())).unwrap_or(Value::Null);
        if let (Some(obj), Some(upstream)) = (usage.as_object_mut(), &upstream_model) {
            obj.insert("upstream_model".into(), json!(upstream));
        }
        Ok(Response {
            id: output.response_id.unwrap_or_default(),
            model,
            content: output.replay.iter().map(|item| item.to_checkpoint_value().clone()).collect(),
            stop_reason,
            // 只有未完成的回應可能沒有用量（completed 缺 usage 已經在組裝時拒絕）：存 `null`，結算回 UntrustedUsage，不當成 0。
            usage,
        })
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = tokio::time::timeout(transport.timeout, operation) => result.map_err(|_| Error::Timeout)?,
    }
}
