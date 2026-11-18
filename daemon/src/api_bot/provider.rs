//! Provider 接點（openai-api 切片 A）：runner 與 adapter 只認這裡的中立型別與兩個 trait，
//! 原生的請求／回應／transcript 形狀由各 provider 的實作負責（目前只有 `anthropic`）。
//!
//! Transcript 由 provider 擁有：`runner::State` 的 `messages`／`responses` 是兩個**通用容器**，裡面放的是該 provider 的
//! 原生資料，不重新建模。`Message { role, content }`＝transcript 的一筆（Claude：一則訊息；OpenAI Responses：一個有序的
//! input／output item，`content` 是 item 的 JSON 原樣——`reasoning` 的 `encrypted_content`、`function_call` 的 `call_id`
//! 都留在裡面，`role` 放 item 的種類）。`Response { id, model, content, stop_reason, usage }`＝一次模型呼叫的原生終態
//! （`content`＝輸出 items／blocks、`usage`＝原生用量）。checkpoint 的 `provider` 標籤決定用哪個 [`Codec`] 解讀。
//!
//! Transcript（`runner::State` 的 `messages`／`responses`）以 provider **原生**格式保存，不重新建模：
//! `Message.content`、`Response.content`／`stop_reason`／`usage` 對 runner 是不透明的容器，只有該 provider 的
//! [`Codec`] 會讀寫裡面的內容。這樣舊 checkpoint 與送出的 request body 才能逐位元組不變。
use super::http;
use super::protocol::{Message, Response};
use super::quote::{QuoteError, QuoteProvider, QuotedRequest};
use super::runner::State;
use super::sse::StreamProgress;
use futures::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 模型要求執行的一個工具呼叫：`id`＝呼叫 id（call id）、`name`＝工具名、`input`＝參數（JSON 物件）。
pub use super::protocol::ToolCall;

/// 一個工具呼叫的結果，交給 provider 寫回 transcript。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub output: String,
    pub is_error: bool,
}

/// 模型這一次回應為什麼停下來。`Other` 帶 provider 的原字串（只用來回報，不拿來分支）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    Refusal,
    MaxTokens,
    Other(String),
}

/// provider 把一份回應收進 transcript 之後，給 runner 的中立視圖。
#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutput {
    /// 這一批要執行的工具（照回應裡的順序）。
    pub calls: Vec<ToolCall>,
    pub stop: StopReason,
    /// 回應帶了伺服器端壓縮：runner 存完 checkpoint 之後要呼叫 [`Codec::prune_after_compaction`]。
    pub compacted: bool,
    /// 伺服器端工具跑到一半暫停（Anthropic 的 `pause_turn`）：沒有工具要跑時直接重送就會接著做。
    pub resumable_pause: bool,
}

/// 回應內容不合法（工具格式錯、數量超過上限、工具 id 已經完成過）。transcript 不會被動到。
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidResponse;

/// 一批工具結果最多幾個（超過＝不合法的回應）。
pub const MAX_TOOL_CALLS_PER_RESPONSE: usize = 32;

/// Transcript 的原生編解碼：runner 只透過它讀寫 `State` 裡的原生內容。全部是純同步、不做 I/O。
pub trait Codec: Send + Sync {
    /// 驗證並把回應收進 transcript（原生 assistant 訊息＋`responses`）。失敗時 `state` 不變。
    fn accept(&self, state: &mut State, response: Response) -> Result<TurnOutput, InvalidResponse>;
    /// 把這一批目前為止的工具結果寫成 transcript 的最後一則（原生的工具結果訊息）。`replace_last`＝這一批的訊息
    /// 已經在 transcript 裡，整則換成新的內容；否則新增一則。`note` 是附在結果後面、給模型看的系統提示。
    fn put_tool_results(&self, state: &mut State, results: &[ToolResult], note: Option<&str>, replace_last: bool);
    /// 在下一次請求的最後一則使用者訊息後面附一段提示；最後一則不是可以附加的形狀就不動。
    fn append_note(&self, state: &mut State, note: &str);
    /// 伺服器端壓縮之後，把 transcript 修剪成 provider 還會用到的部分。
    fn prune_after_compaction(&self, state: &mut State);
    /// 回應裡給使用者看的文字（不含 thinking、工具內容）。
    fn visible_text(&self, response: &Response) -> String;
}

/// 送出一份已編碼請求所需的連線設定。
pub struct Transport<'a> {
    pub endpoint: &'a str,
    pub key: &'a str,
    pub timeout: Duration,
    pub max_response_bytes: usize,
}

/// 開一個 session 的中立選項：每個 provider 都有的三項，加上該 provider 自己的額外設定。
#[derive(Clone, Debug, PartialEq)]
pub struct SessionOptions {
    pub model: String,
    /// 推理強度；合法值與降級順序由 provider 決定（[`Provider::lower_effort`]）。
    pub effort: String,
    pub max_output_tokens: u32,
    pub extras: ProviderExtras,
}

/// provider 專屬、別家沒有對應的設定。
#[derive(Clone, Debug, PartialEq)]
pub enum ProviderExtras {
    None,
    Anthropic(super::anthropic::AnthropicExtras),
}

/// 一個模型供應商：自己持有 session 的選項，負責編碼請求、送出並解析串流終態與用量、錯誤分類、報價。
/// session 建立時固定一個。adapter 只透過這個 trait 與 [`QuotedRequest`] 操作，不碰任何原生型別。
pub trait Provider: Send + Sync {
    /// 穩定的識別字（寫進 checkpoint 與 session）：`"anthropic"`。
    fn id(&self) -> &'static str;
    fn codec(&self) -> &dyn Codec;
    fn model(&self) -> &str;
    /// 設定的推理強度（adapter 可能因為回合花費改用較低的一級再編碼）。
    fn effort(&self) -> &str;
    /// 比 `effort` 低一級（已經最低就回自己）；順序是這個 provider、這個模型自己的。
    fn lower_effort(&self, effort: &str) -> String;
    /// 正式環境的官方 endpoint（固定 HTTPS；測試另外注入 loopback）。
    fn official_endpoint(&self) -> &'static str;
    /// 把 transcript 編成**不可變**的請求：報價、request hash、實際送出都用回傳的這一份。每次都從完整的 transcript 編，
    /// 不依賴服務端留存。`previous`＝上一份回應（只用來收緊報價的輸入上界，不影響 body）。
    fn encode(&self, effort: &str, system: &Value, transcript: &[Message], previous: Option<&Response>) -> Result<Arc<dyn QuotedRequest>, QuoteError>;
    /// 送出已編碼的請求，回傳解析好的終態回應（含用量）。回傳 BoxFuture：不讓呼叫端的 poll 框變大。
    /// `request` 必須是同一個 provider 的 [`Self::encode`] 產生的，否則回 `InvalidRequest`、不送任何東西。
    fn send<'a>(
        &'a self,
        transport: Transport<'a>,
        request: &'a dyn QuotedRequest,
        cancel: &'a CancellationToken,
        progress: Option<&'a (dyn Fn(&StreamProgress) + Send + Sync)>,
    ) -> BoxFuture<'a, Result<Response, http::Error>>;
    /// 串流進行中的暫估花費（微美元）；估不出來回 `None`。
    fn running_cost_microusd(&self, progress: &StreamProgress) -> Option<i64>;
    /// 用量不可信時給人看的原因（`None`＝看不出問題）。
    fn usage_problem(&self, request: &dyn QuotedRequest, response: &Response) -> Option<String>;
    /// 上游明確拒絕、確定沒有處理也不計費的錯誤。
    fn definitive_rejection(&self, err: &http::Error) -> bool;
    /// 已成功收到的回應內容不合法的原因（`None`＝合法）。
    fn invalid_content_reason(&self, response: &Response, durable: &State) -> Option<String>;
    /// 這份 checkpoint 能不能用這個 provider 現在的模型接續。
    fn checkpoint_matches_model(&self, state: &State) -> bool;
    fn quote_provider(&self) -> Arc<dyn QuoteProvider>;
}

/// 依識別字取得 provider（session 建立時固定、resume／recovery 依 checkpoint 的標籤取回同一個）。不認得的回 `None`。
/// 選項不屬於這個 provider（`extras` 對不上）也回 `None`。
pub(crate) fn for_id(id: &str, options: SessionOptions, quote: Arc<dyn QuoteProvider>) -> Option<Arc<dyn Provider>> {
    match id {
        super::anthropic::ID => Some(Arc::new(super::anthropic::AnthropicProvider::new(super::anthropic::options_from(options)?, quote))),
        super::openai::provider::ID => Some(Arc::new(super::openai::provider::OpenAiProvider::new(options, quote)?)),
        _ => None,
    }
}

/// 這個識別字是不是已知的 provider（recovery 只認得出標籤、還不需要建 provider 的地方用）。
pub(crate) fn is_known(id: &str) -> bool {
    id == super::anthropic::ID
}
