//! Responses API 的請求編碼、SSE 組裝、終態與錯誤分類（中立型別，無網路、無 DB、無 key）。
//!
//! 契約（設計 plan-openai-api.md 第 2 節）：
//! - `POST /v1/responses`，`stream:true`、`store:false`；每次從 checkpoint 組**完整** `input`
//!   （user message、上一輪原樣的 output items——含 reasoning 的 `encrypted_content`、assistant message、`function_call`——
//!   以及工具結果 `function_call_output {call_id, output}`），不依賴 `previous_response_id`。
//! - 工具配對用 **`call_id`**，不是 output item 的 `id`。同一 response 的 `call_id` 不可重複。
//! - 串流只用來組裝；`response.completed` 的完整 response 才是權威（與串流對不上＝協定錯誤，不採用）。
//!   `failed`／`incomplete`／`error`／無終態 EOF 各自分開；`max_output_tokens` 未完成可辨識，**半截的工具參數永不執行**。
//! - 加密 reasoning 只供續傳：[`ReplayItem`] 的 `Debug` 遮掉內容，不進對話、log、WS。

use super::pricing::{Usage, UsageError};
use super::sse::{SseError, SseParser};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};

pub(crate) const RESPONSES_PATH: &str = "/v1/responses";

// ───────────────────────────── effort ─────────────────────────────

/// gpt-6-luna 的 `reasoning.effort`（依序由低到高）。
pub(crate) const LUNA_EFFORTS: [&str; 6] = ["none", "low", "medium", "high", "xhigh", "max"];
pub(crate) const DEFAULT_EFFORT: &str = "medium";

/// 這個 API 模型允許的 effort 清單；不認得的模型回 `None`（呼叫端在 HTTP 前拒絕）。
pub(crate) fn efforts_for_model(model: &str) -> Option<&'static [&'static str]> {
    match model {
        "gpt-6-luna" => Some(&LUNA_EFFORTS),
        _ => None,
    }
}

/// 驗證並回要送的 effort；沒指定就用預設 `medium`。
pub(crate) fn resolve_effort(model: &str, requested: Option<&str>) -> Result<&'static str, CodecError> {
    let list = efforts_for_model(model).ok_or_else(|| CodecError::UnsupportedModel(model.to_string()))?;
    let want = requested.unwrap_or(DEFAULT_EFFORT);
    list.iter().copied().find(|e| *e == want).ok_or_else(|| CodecError::UnsupportedEffort { model: model.to_string(), effort: want.to_string() })
}

// ───────────────────────────── 錯誤型別 ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CodecError {
    UnsupportedModel(String),
    UnsupportedEffort { model: String, effort: String },
    /// 請求的 input 有 `function_call` 沒有對應的 `function_call_output`（或反過來、重複）。
    UnpairedToolCall(String),
    /// replay 的 item 不是我們保存過的種類。
    BadReplayItem(String),
    DuplicateCallId(String),
    /// 工具參數不是合法 JSON 物件：整個回應不採用，工具不執行。
    BadToolArguments(String),
    /// 事件順序、JSON、型別或與 `response.completed` 不一致。
    Protocol(String),
    Sse(SseError),
}

// ───────────────────────────── 請求 ─────────────────────────────

/// 上一輪原樣的 output item（message／reasoning／function_call），用來重播。內容（特別是 `encrypted_content`）不進 Debug。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ReplayItem(Value);

impl std::fmt::Debug for ReplayItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = self.0.get("type").and_then(Value::as_str).unwrap_or("?");
        write!(f, "ReplayItem({kind}, <redacted>)")
    }
}

impl ReplayItem {
    pub(crate) fn new(item: Value) -> Result<Self, CodecError> {
        match item.get("type").and_then(Value::as_str) {
            Some("message" | "reasoning" | "function_call") if item.is_object() => Ok(Self(item)),
            other => Err(CodecError::BadReplayItem(format!("unsupported item type {other:?}"))),
        }
    }
    pub(crate) fn kind(&self) -> &str {
        self.0.get("type").and_then(Value::as_str).unwrap_or("")
    }
    pub(crate) fn call_id(&self) -> Option<&str> {
        self.0.get("call_id").and_then(Value::as_str)
    }
    /// 存進 checkpoint 用（私人 DB 欄位）；呼叫端不得拿去 log。
    pub(crate) fn to_checkpoint_value(&self) -> &Value {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InputItem {
    User(String),
    Replay(ReplayItem),
    /// `function_call_output`：用 `call_id` 配對。
    ToolOutput { call_id: String, output: String },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON schema（object）。
    pub parameters: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputItem>,
    pub tools: Vec<ToolSpec>,
    /// `None`＝預設 `medium`。
    pub effort: Option<String>,
    pub max_output_tokens: u64,
}

impl Request {
    /// 編成要送的 JSON（位元組）。同一個 `Request` 永遠編出相同位元組（報價、hash、送出用同一份）。
    pub(crate) fn body(&self) -> Result<Vec<u8>, CodecError> {
        let effort = resolve_effort(&self.model, self.effort.as_deref())?;
        self.check_pairing()?;
        let input: Vec<Value> = self
            .input
            .iter()
            .map(|item| match item {
                InputItem::User(text) => json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}),
                InputItem::Replay(r) => r.0.clone(),
                InputItem::ToolOutput { call_id, output } => json!({"type": "function_call_output", "call_id": call_id, "output": output}),
            })
            .collect();
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false}))
            .collect();
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("instructions".into(), json!(self.instructions));
        body.insert("input".into(), Value::Array(input));
        body.insert("reasoning".into(), json!({"effort": effort}));
        body.insert("max_output_tokens".into(), json!(self.max_output_tokens));
        body.insert("stream".into(), json!(true));
        body.insert("store".into(), json!(false));
        // store:false 時要續傳 reasoning，必須要求回傳加密內容。
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
            body.insert("parallel_tool_calls".into(), json!(true));
        }
        serde_json::to_vec(&Value::Object(body)).map_err(|e| CodecError::Protocol(e.to_string()))
    }

    /// 每個重播的 `function_call` 必須剛好有一個 `function_call_output`（用 `call_id`），輸出不能孤兒或重複。
    fn check_pairing(&self) -> Result<(), CodecError> {
        let mut calls = HashSet::new();
        let mut outputs = HashSet::new();
        for item in &self.input {
            match item {
                InputItem::Replay(r) if r.kind() == "function_call" => {
                    let id = r.call_id().ok_or_else(|| CodecError::BadReplayItem("function_call without call_id".into()))?;
                    if !calls.insert(id.to_string()) {
                        return Err(CodecError::DuplicateCallId(id.to_string()));
                    }
                }
                InputItem::ToolOutput { call_id, .. } => {
                    if !calls.contains(call_id) || !outputs.insert(call_id.clone()) {
                        return Err(CodecError::UnpairedToolCall(call_id.clone()));
                    }
                }
                _ => {}
            }
        }
        match calls.difference(&outputs).next() {
            Some(id) => Err(CodecError::UnpairedToolCall(id.clone())),
            None => Ok(()),
        }
    }
}

// ───────────────────────────── 輸出型別 ─────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolCall {
    /// 配對用（`function_call_output.call_id`）。
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopKind {
    EndTurn,
    ToolUse,
    /// 模型拒答（`refusal` content part）：文字在 `ModelOutput::refusal`，不是一般 assistant 文字。
    Refusal,
}

/// 一次成功（或可用）的模型輸出。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelOutput {
    pub response_id: Option<String>,
    /// 可見的 assistant 文字（不含 reasoning）。
    pub text: String,
    pub refusal: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// 下一次呼叫要原樣重播的有序 items（含 reasoning）。
    pub replay: Vec<ReplayItem>,
    /// `Err(Missing)`＝回應沒帶 usage：費用未知，**不能當 0**。
    pub usage: Result<Usage, UsageError>,
    pub stop: StopKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IncompleteReason {
    MaxOutputTokens,
    ContentFilter,
    Other(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Outcome {
    Completed(ModelOutput),
    /// 未完成：`output.tool_calls` 永遠是空的（半截參數不執行）；`partial_tool_call` 說明有沒有半截呼叫。
    Incomplete { reason: IncompleteReason, partial_tool_call: bool, output: ModelOutput },
    /// `response.failed` 或 `error` 事件。
    Failed { code: String, message: String, class: ErrorClass },
    /// 沒有終態事件就 EOF：送出後的斷流，結果未知。
    Truncated,
}

/// 串流進行中的暫態 UI 更新（不是終態、不入帳）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Update {
    TextDelta(String),
}

// ───────────────────────────── 組裝 ─────────────────────────────

#[derive(Default)]
struct ItemState {
    item: Value,
    text: String,
    args: String,
    args_done: Option<String>,
}

pub(crate) struct ResponseAssembler {
    parser: SseParser,
    items: BTreeMap<u64, ItemState>,
    response_id: Option<String>,
    saw_created: bool,
    terminal: Option<Outcome>,
}

impl Default for ResponseAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseAssembler {
    pub(crate) fn new() -> Self {
        Self { parser: SseParser::new(), items: BTreeMap::new(), response_id: None, saw_created: false, terminal: None }
    }

    /// 餵一塊網路位元組（可任意切塊）。回這一塊產生的暫態 UI 更新。
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Update>, CodecError> {
        let events = self.parser.feed(bytes).map_err(CodecError::Sse)?;
        let mut updates = Vec::new();
        for ev in events {
            if ev.data.trim() == "[DONE]" {
                continue;
            }
            let data: Value = serde_json::from_str(&ev.data).map_err(|e| CodecError::Protocol(format!("event is not JSON: {e}")))?;
            let kind = data.get("type").and_then(Value::as_str).map(str::to_string).or(ev.event.clone()).unwrap_or_default();
            if self.terminal.is_some() {
                continue; // 終態之後的事件不採用
            }
            self.on_event(&kind, &data, &mut updates)?;
        }
        Ok(updates)
    }

    /// 串流結束（EOF）。有終態就回它；沒有就是斷流 `Truncated`（半截事件也算）。
    pub(crate) fn finish(self) -> Result<Outcome, CodecError> {
        Ok(self.terminal.unwrap_or(Outcome::Truncated))
    }

    fn index(data: &Value) -> Result<u64, CodecError> {
        data.get("output_index").and_then(Value::as_u64).ok_or_else(|| CodecError::Protocol("event without output_index".into()))
    }

    fn on_event(&mut self, kind: &str, data: &Value, updates: &mut Vec<Update>) -> Result<(), CodecError> {
        match kind {
            "response.created" => {
                self.saw_created = true;
                self.response_id = data.pointer("/response/id").and_then(Value::as_str).map(str::to_string);
            }
            "response.output_item.added" => {
                let idx = Self::index(data)?;
                let item = data.get("item").cloned().ok_or_else(|| CodecError::Protocol("output_item.added without item".into()))?;
                self.items.insert(idx, ItemState { item, ..Default::default() });
            }
            "response.output_text.delta" => {
                let idx = Self::index(data)?;
                let delta = data.get("delta").and_then(Value::as_str).ok_or_else(|| CodecError::Protocol("text delta without delta".into()))?;
                self.items.entry(idx).or_default().text.push_str(delta);
                updates.push(Update::TextDelta(delta.to_string()));
            }
            "response.function_call_arguments.delta" => {
                let idx = Self::index(data)?;
                let delta = data.get("delta").and_then(Value::as_str).ok_or_else(|| CodecError::Protocol("arguments delta without delta".into()))?;
                self.items.entry(idx).or_default().args.push_str(delta);
            }
            "response.function_call_arguments.done" => {
                let idx = Self::index(data)?;
                let args = data.get("arguments").and_then(Value::as_str).ok_or_else(|| CodecError::Protocol("arguments.done without arguments".into()))?;
                let st = self.items.entry(idx).or_default();
                if !st.args.is_empty() && st.args != args {
                    return Err(CodecError::Protocol("arguments.done disagrees with the streamed fragments".into()));
                }
                st.args_done = Some(args.to_string());
            }
            "response.output_item.done" => {
                let idx = Self::index(data)?;
                let item = data.get("item").cloned().ok_or_else(|| CodecError::Protocol("output_item.done without item".into()))?;
                let st = self.items.entry(idx).or_default();
                st.item = item;
            }
            "response.completed" => {
                let resp = data.get("response").ok_or_else(|| CodecError::Protocol("completed without response".into()))?;
                if resp.get("status").and_then(Value::as_str).is_some_and(|s| s != "completed") {
                    return Err(CodecError::Protocol("completed event carries a non-completed status".into()));
                }
                let output = self.build_output(resp, true)?;
                self.terminal = Some(Outcome::Completed(output));
            }
            "response.incomplete" => {
                let resp = data.get("response").ok_or_else(|| CodecError::Protocol("incomplete without response".into()))?;
                let reason = match resp.pointer("/incomplete_details/reason").and_then(Value::as_str) {
                    Some("max_output_tokens") => IncompleteReason::MaxOutputTokens,
                    Some("content_filter") => IncompleteReason::ContentFilter,
                    Some(other) => IncompleteReason::Other(other.to_string()),
                    None => IncompleteReason::Other("unknown".into()),
                };
                // 半截的工具呼叫：串流裡有 function_call 但參數沒收完整，或最終 output 裡有 function_call。
                let streamed_partial = self
                    .items
                    .values()
                    .any(|s| s.item.get("type").and_then(Value::as_str) == Some("function_call") && s.args_done.is_none() && !Self::item_is_done(&s.item));
                let (mut output, final_has_call) = self.build_incomplete_output(resp)?;
                let partial_tool_call = streamed_partial || final_has_call;
                // 未完成的回應永遠不執行工具（即使其中有看起來完整的呼叫：它們屬於被截斷的回合）。
                output.tool_calls.clear();
                output.stop = StopKind::EndTurn;
                self.terminal = Some(Outcome::Incomplete { reason, partial_tool_call, output });
            }
            "response.failed" => {
                let err = data.pointer("/response/error").cloned().unwrap_or(Value::Null);
                self.terminal = Some(Self::failed(&err));
            }
            "error" => {
                self.terminal = Some(Self::failed(data));
            }
            // reasoning summary、content_part、rate_limits、in_progress、queued…：不影響組裝。
            _ => {}
        }
        Ok(())
    }

    fn item_is_done(item: &Value) -> bool {
        item.get("status").and_then(Value::as_str) == Some("completed")
    }

    fn failed(err: &Value) -> Outcome {
        let code = err.get("code").and_then(Value::as_str).unwrap_or("unknown").to_string();
        let message = err.get("message").and_then(Value::as_str).unwrap_or("").to_string();
        let kind = err.get("type").and_then(Value::as_str).unwrap_or("");
        let class = classify_error(&code, kind);
        Outcome::Failed { code, message, class }
    }

    /// 未完成的 response：可見文字與 usage 照收，但 function_call 的（可能是半截的）參數不解析、不產生可執行的呼叫。
    fn build_incomplete_output(&self, resp: &Value) -> Result<(ModelOutput, bool), CodecError> {
        let items: Vec<Value> = match resp.get("output").and_then(Value::as_array) {
            Some(items) => items.clone(),
            None => self.items.values().map(|s| s.item.clone()).collect(),
        };
        let mut text = String::new();
        let mut refusal: Option<String> = None;
        let mut replay = Vec::new();
        let mut has_call = false;
        for item in &items {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => text.push_str(part.get("text").and_then(Value::as_str).unwrap_or("")),
                            Some("refusal") => refusal.get_or_insert_with(String::new).push_str(part.get("refusal").and_then(Value::as_str).unwrap_or("")),
                            _ => {}
                        }
                    }
                    replay.push(ReplayItem::new(item.clone())?);
                }
                Some("reasoning") => replay.push(ReplayItem::new(item.clone())?),
                // 半截的 function_call 不進 replay：沒有配對的輸出，重播會讓下一次請求變成不合法。
                Some("function_call") => has_call = true,
                _ => {}
            }
        }
        let output = ModelOutput {
            response_id: resp.get("id").and_then(Value::as_str).map(str::to_string).or_else(|| self.response_id.clone()),
            text,
            refusal,
            tool_calls: Vec::new(),
            replay,
            usage: Usage::from_response(resp.get("usage")),
            stop: StopKind::EndTurn,
        };
        Ok((output, has_call))
    }

    /// 從 completed 的 response 物件建 `ModelOutput`，並與串流累積的內容核對。
    fn build_output(&self, resp: &Value, cross_check: bool) -> Result<ModelOutput, CodecError> {
        let output = match resp.get("output").and_then(Value::as_array) {
            Some(items) if !items.is_empty() => items.clone(),
            _ => return Err(CodecError::Protocol("completed response has no output".into())),
        };
        let mut text = String::new();
        let mut refusal: Option<String> = None;
        let mut tool_calls = Vec::new();
        let mut replay = Vec::new();
        let mut seen_calls = HashSet::new();
        for item in &output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => text.push_str(part.get("text").and_then(Value::as_str).unwrap_or("")),
                            Some("refusal") => refusal.get_or_insert_with(String::new).push_str(part.get("refusal").and_then(Value::as_str).unwrap_or("")),
                            _ => {}
                        }
                    }
                }
                Some("reasoning") => {}
                Some("function_call") => {
                    let call_id = item.get("call_id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| CodecError::Protocol("function_call without call_id".into()))?;
                    let name = item.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| CodecError::Protocol("function_call without name".into()))?;
                    if !seen_calls.insert(call_id.to_string()) {
                        return Err(CodecError::DuplicateCallId(call_id.to_string()));
                    }
                    let raw = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                    let arguments: Value = serde_json::from_str(raw).map_err(|_| CodecError::BadToolArguments(call_id.to_string()))?;
                    if !arguments.is_object() {
                        return Err(CodecError::BadToolArguments(call_id.to_string()));
                    }
                    tool_calls.push(ToolCall { call_id: call_id.to_string(), name: name.to_string(), arguments });
                }
                other => return Err(CodecError::Protocol(format!("unsupported output item type {other:?}"))),
            }
            replay.push(ReplayItem::new(item.clone())?);
        }
        if cross_check {
            self.cross_check(&output, &tool_calls, &text)?;
        }
        let stop = if !tool_calls.is_empty() {
            StopKind::ToolUse
        } else if refusal.is_some() {
            StopKind::Refusal
        } else {
            StopKind::EndTurn
        };
        Ok(ModelOutput {
            response_id: resp.get("id").and_then(Value::as_str).map(str::to_string).or_else(|| self.response_id.clone()),
            text,
            refusal,
            tool_calls,
            replay,
            usage: Usage::from_response(resp.get("usage")),
            stop,
        })
    }

    /// completed 的完整 response 與串流累積的內容必須一致（文字、每個工具呼叫的參數）。
    fn cross_check(&self, output: &[Value], calls: &[ToolCall], text: &str) -> Result<(), CodecError> {
        let streamed_text: String = self.items.values().map(|s| s.text.as_str()).collect();
        if !streamed_text.is_empty() && streamed_text != text {
            return Err(CodecError::Protocol("completed text disagrees with the streamed text".into()));
        }
        for st in self.items.values() {
            if st.item.get("type").and_then(Value::as_str) != Some("function_call") {
                continue;
            }
            let streamed = st.args_done.as_deref().or(if st.args.is_empty() { None } else { Some(st.args.as_str()) });
            let Some(streamed) = streamed else { continue };
            let call_id = st.item.get("call_id").and_then(Value::as_str).unwrap_or("");
            let final_args = output.iter().find(|i| i.get("call_id").and_then(Value::as_str) == Some(call_id)).and_then(|i| i.get("arguments")).and_then(Value::as_str);
            if final_args != Some(streamed) || !calls.iter().any(|c| c.call_id == call_id) {
                return Err(CodecError::Protocol("completed tool call disagrees with the streamed arguments".into()));
            }
        }
        Ok(())
    }
}

// ───────────────────────────── 錯誤分類 ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ErrorClass {
    /// 401：金鑰無效或缺（具名）。
    Auth,
    /// 403：沒有權限（具名）。
    Permission,
    /// 模型不存在／不可用／內容被模型政策拒絕（具名，附 code）。
    ModelRejected { code: String },
    /// 429 暫時限速：`retry_after_secs` 是服務端給的等待秒數（沒給就是 `None`）。
    RateLimited { retry_after_secs: Option<u64> },
    /// 429 `insufficient_quota`：額度用完，不是等一下就好。
    InsufficientQuota,
    /// 其他明確拒絕的 4xx（請求本身有問題）。
    InvalidRequest { code: String },
    /// 送出後 timeout／斷流／5xx／無法證明沒計費：保留 `unknown` 與預留，**不自動重送**。
    Unknown,
}

impl ErrorClass {
    /// 這一類失敗之後費用與結果都未知（呼叫端要保留預留，不能退錢、不能偷偷重送）。
    pub(crate) fn outcome_unknown(&self) -> bool {
        matches!(self, ErrorClass::Unknown)
    }
}

/// 依 error 的 `code`／`type` 分類（HTTP body、`error` 事件、`response.failed` 共用）。
pub(crate) fn classify_error(code: &str, kind: &str) -> ErrorClass {
    match (code, kind) {
        ("insufficient_quota", _) | (_, "insufficient_quota") => ErrorClass::InsufficientQuota,
        ("rate_limit_exceeded", _) | (_, "rate_limit_exceeded") | ("rate_limit", _) => ErrorClass::RateLimited { retry_after_secs: None },
        ("invalid_api_key", _) | ("incorrect_api_key", _) | (_, "authentication_error") => ErrorClass::Auth,
        ("permission_denied", _) | (_, "permission_error") => ErrorClass::Permission,
        ("model_not_found", _) | ("model_not_available", _) | ("invalid_model", _) | ("invalid_prompt", _) | ("content_filter", _) | ("refusal", _) => {
            ErrorClass::ModelRejected { code: code.to_string() }
        }
        ("server_error", _) | ("overloaded", _) | ("", "server_error") => ErrorClass::Unknown,
        _ if !code.is_empty() => ErrorClass::InvalidRequest { code: code.to_string() },
        _ => ErrorClass::Unknown,
    }
}

/// HTTP 回應（沒有串流）的分類。`retry_after` 是 `Retry-After` header 原文（只認整數秒）。
pub(crate) fn classify_http(status: u16, body: &str, retry_after: Option<&str>) -> ErrorClass {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let err = parsed.get("error").cloned().unwrap_or(Value::Null);
    let code = err.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = err.get("type").and_then(Value::as_str).unwrap_or("");
    match status {
        401 => ErrorClass::Auth,
        403 => ErrorClass::Permission,
        429 => match classify_error(code, kind) {
            ErrorClass::InsufficientQuota => ErrorClass::InsufficientQuota,
            _ => ErrorClass::RateLimited { retry_after_secs: retry_after.and_then(|v| v.trim().parse::<u64>().ok()) },
        },
        404 | 400 | 422 => match classify_error(code, kind) {
            ErrorClass::Unknown => ErrorClass::InvalidRequest { code: code.to_string() },
            other => other,
        },
        s if (500..600).contains(&s) => ErrorClass::Unknown,
        // 408／409／425 等「送沒送到不確定」的歸 unknown；其餘明確的 4xx 是請求問題。
        408 | 409 | 425 => ErrorClass::Unknown,
        _ => match classify_error(code, kind) {
            ErrorClass::Unknown => ErrorClass::InvalidRequest { code: code.to_string() },
            other => other,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportFailure {
    /// 送出後等不到回應或串流停住。
    Timeout,
    /// 送出後連線被切斷（或本機取消）。
    Disconnected,
}

/// 送出後的傳輸失敗一律是 `Unknown`：無法證明沒計費，不重送。
pub(crate) fn classify_transport(_failure: TransportFailure) -> ErrorClass {
    ErrorClass::Unknown
}
