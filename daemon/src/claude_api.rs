//! 一次性 LLM 工作改走 Claude API key（SPEC §4.3d，issue #1919）。
//!
//! 三樣東西：共用的 Anthropic Messages client（[`MessagesClient`]／[`AnthropicHttpClient`]，#1870 的翻譯也走這一份）、
//! 各用途的入口 [`complete`]（兩層開關、保險絲、記帳本），以及 `[claude_api]` 的設定端點
//! （`/api/claude-api/settings`、`/ledger`）。各用途自己組 prompt、自己決定失敗時退回哪條路；這裡不重試、不排隊。
//! key 只存在 `key_file`（600，`crate::key_file`）：不進 config.toml、log、帳本與任何 API 回應。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{claude_model_id_ok, ClaudeApiUseState, ConfigFile, CLAUDE_API_EFFORTS, CLAUDE_API_MODELS, CLAUDE_API_USES};
use crate::key_file::{key_status, read_key, stage_key};
use crate::lc_error::LcError;
use crate::state::App;

const ANTHROPIC_VERSION: &str = "2023-06-01";
/// 帳本留最近這麼多筆；累計 token 不受這個上限影響。
const LEDGER_CAP: usize = 200;

// ───────────── 共用 client（原 changelog_translate 的那一份） ─────────────

/// 正式 client 與測試假貨的接縫。測試先 `set_client`，生產路徑才會打到網路。
pub trait MessagesClient: Send + Sync {
    fn complete<'a>(
        &'a self,
        req: &'a MessagesRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>>;
}

/// 一次 Messages API 請求。`effort` 空字串＝不送；`format` 是結構化輸出的 JSON schema。
pub struct MessagesRequest {
    pub endpoint: String,
    pub key: String,
    pub model: String,
    pub effort: String,
    pub max_tokens: u32,
    pub timeout_ms: u64,
    pub system: Option<String>,
    pub format: Option<Value>,
    pub prompt: String,
}

impl MessagesRequest {
    pub(crate) fn request_json(&self) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": [{ "role": "user", "content": self.prompt }],
        });
        if let Some(system) = &self.system {
            body["system"] = json!(system);
        }
        let mut output = serde_json::Map::new();
        if !self.effort.is_empty() {
            output.insert("effort".into(), json!(self.effort));
        }
        if let Some(schema) = &self.format {
            output.insert("format".into(), json!({ "type": "json_schema", "schema": schema }));
        }
        if !output.is_empty() {
            body["output_config"] = Value::Object(output);
        }
        body
    }
}

impl std::fmt::Debug for MessagesRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessagesRequest")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct Usage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_input_tokens: Option<i64>,
    pub cache_read_input_tokens: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct ClientOutcome {
    pub text: String,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Clone, Debug)]
pub enum ClientError {
    Timeout,
    RateLimited {
        retry_after: Option<String>,
        usage: Option<Usage>,
    },
    Http {
        status: u16,
        usage: Option<Usage>,
    },
}

pub struct AnthropicHttpClient;

impl MessagesClient for AnthropicHttpClient {
    fn complete<'a>(
        &'a self,
        req: &'a MessagesRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
        Box::pin(complete_http(req))
    }
}


fn complete_http(
    req: &MessagesRequest,
) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + '_>> {
    Box::pin(async move {
        #[cfg(test)]
        if !test_endpoint_is_loopback(&req.endpoint) {
            return Err(ClientError::Http {
                status: 0,
                usage: None,
            });
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(req.timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ClientError::Http {
                status: 0,
                usage: None,
            })?;
        let response = client
            .post(&req.endpoint)
            .header("x-api-key", &req.key)
            .header("anthropic-version", "2023-06-01")
            .json(&req.request_json())
            .send()
            .await;
        let response = match response {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return Err(ClientError::Timeout),
            Err(_) => {
                return Err(ClientError::Http {
                    status: 0,
                    usage: None,
                })
            }
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await.map_err(|_| ClientError::Http {
            status: status.as_u16(),
            usage: None,
        })?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let usage = parse_usage(&value);
        if status.as_u16() == 429 {
            return Err(ClientError::RateLimited { retry_after, usage });
        }
        if !status.is_success() {
            return Err(ClientError::Http {
                status: status.as_u16(),
                usage,
            });
        }
        Ok(ClientOutcome {
            text: message_text(&value),
            model: value
                .get("model")
                .and_then(|m| m.as_str())
                .map(str::to_string),
            stop_reason: value
                .get("stop_reason")
                .and_then(|m| m.as_str())
                .map(str::to_string),
            usage,
        })
    })
}

fn parse_usage(value: &Value) -> Option<Usage> {
    let usage = value.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    Some(Usage {
        input_tokens: json_i64(usage, "input_tokens"),
        output_tokens: json_i64(usage, "output_tokens"),
        cache_creation_input_tokens: json_i64(usage, "cache_creation_input_tokens"),
        cache_read_input_tokens: json_i64(usage, "cache_read_input_tokens"),
    })
}

fn json_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn message_text(value: &Value) -> String {
    value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| block.as_str().map(str::to_string))
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
fn test_endpoint_is_loopback(endpoint: &str) -> bool {
    let Some(rest) = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
    else {
        return false;
    };
    let hostport = rest.split('/').next().unwrap_or("");
    let host = hostport
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(hostport);
    let host = host.trim_matches(|c: char| c == '[' || c == ']');
    host == "127.0.0.1" || host == "::1"
}

// ───────────── 用途層：開關、保險絲、帳本 ─────────────

/// 一個用途的一次呼叫。模型與 effort 由設定決定，呼叫端不指定。
pub struct ClaudeRequest {
    /// 必須是 `CLAUDE_API_USES` 裡的 id。
    pub use_case: &'static str,
    pub system: Option<String>,
    pub prompt: String,
    pub max_tokens: u32,
    /// 要結構化輸出時給 JSON schema（`output_config.format`）。
    pub format: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct ClaudeReply {
    /// 所有 `text` 區塊串起來；thinking 區塊不收。
    pub text: String,
    pub model: String,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
    pub elapsed_ms: u64,
}

/// 失敗的分類。呼叫端只需要知道「要不要退回舊路徑」：任何一種都該退回，這裡不替它決定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeError {
    /// 總開關或這個用途的開關沒開，或用途不認得。沒送出請求、不記帳本。
    Disabled,
    /// key 檔讀不到／權限不對／是空的（原因不含 key）。沒送出、不記帳本。
    NoKey(String),
    /// 這個用途一小時內已經送了 `max_per_hour` 次。沒送出、不記帳本。
    Budget,
    /// 401／403：key 錯、被停用或沒有權限。
    Auth { status: u16 },
    /// 429；`retry_after_secs` 來自 `Retry-After`。
    RateLimited { retry_after_secs: Option<u64> },
    /// 5xx（含 529 overloaded）。
    Server { status: u16 },
    Timeout,
    /// 連不上、連線中斷、TLS 之類。
    Network,
    /// 其他 4xx（請求本身有問題，例如模型不存在、max_tokens 太大）。
    Rejected { status: u16 },
    /// 200 但內容不能用：沒有文字，或 `stop_reason` 是 `refusal`。
    BadResponse(String),
}

impl ClaudeError {
    /// 帳本與回應用的短標籤。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoKey(_) => "no_key",
            Self::Budget => "budget",
            Self::Auth { .. } => "auth",
            Self::RateLimited { .. } => "rate_limited",
            Self::Server { .. } => "server",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::Rejected { .. } => "rejected",
            Self::BadResponse(_) => "bad_response",
        }
    }

    fn from_client(e: ClientError) -> (Self, Option<Usage>) {
        match e {
            ClientError::Timeout => (Self::Timeout, None),
            ClientError::RateLimited { retry_after, usage } => {
                (Self::RateLimited { retry_after_secs: retry_after.and_then(|v| v.trim().parse().ok()) }, usage)
            }
            ClientError::Http { status: 0, usage } => (Self::Network, usage),
            ClientError::Http { status: s @ (401 | 403), usage } => (Self::Auth { status: s }, usage),
            ClientError::Http { status, usage } if status >= 500 => (Self::Server { status }, usage),
            ClientError::Http { status, usage } => (Self::Rejected { status }, usage),
        }
    }
}

impl std::fmt::Display for ClaudeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "claude api is disabled for this use"),
            Self::NoKey(reason) => write!(f, "no usable claude api key: {reason}"),
            Self::Budget => write!(f, "claude api hourly budget for this use is used up"),
            Self::Auth { status } => write!(f, "claude api rejected the key (HTTP {status})"),
            Self::RateLimited { retry_after_secs: Some(s) } => write!(f, "claude api rate limited, retry after {s}s"),
            Self::RateLimited { retry_after_secs: None } => write!(f, "claude api rate limited"),
            Self::Server { status } => write!(f, "claude api server error (HTTP {status})"),
            Self::Timeout => write!(f, "claude api timed out"),
            Self::Network => write!(f, "claude api unreachable"),
            Self::Rejected { status } => write!(f, "claude api rejected the request (HTTP {status})"),
            Self::BadResponse(why) => write!(f, "claude api returned an unusable response: {why}"),
        }
    }
}

impl std::error::Error for ClaudeError {}

/// 對外的入口：看兩層開關、查保險絲、讀 key、送**一次**請求、記帳本。不重試。
///
/// 開關：`[claude_api] enabled` 總開關與用途自己的 `enabled` 都要開；`changelog_translate` 是舊的一段，
/// 只看 `[changelog_translate] enabled`，不受總開關管。
pub async fn complete(app: &App, req: ClaudeRequest) -> Result<ClaudeReply, ClaudeError> {
    let cfg = app.cfg.get().await;
    complete_with(&cfg, &app.claude_api, req).await
}

pub async fn complete_with(cfg: &ConfigFile, api: &ClaudeApi, req: ClaudeRequest) -> Result<ClaudeReply, ClaudeError> {
    let Some(state) = cfg.claude_use_state(req.use_case) else { return Err(ClaudeError::Disabled) };
    let legacy = CLAUDE_API_USES.iter().any(|d| d.id == req.use_case && d.legacy_section.is_some());
    if !state.enabled || (!legacy && !cfg.claude_api.enabled) {
        return Err(ClaudeError::Disabled);
    }
    let key_file = if req.use_case == "changelog_translate" { cfg.changelog_translate_key_file() } else { cfg.claude_api.key_file.clone() };
    let key = read_key(&key_file).map_err(|e| ClaudeError::NoKey(e.to_string()))?;
    if !api.reserve(req.use_case, state.max_per_hour) {
        return Err(ClaudeError::Budget);
    }
    let request = MessagesRequest {
        endpoint: cfg.claude_api.endpoint.clone(),
        key,
        model: state.model.clone(),
        effort: state.effort.clone(),
        max_tokens: req.max_tokens,
        timeout_ms: cfg.claude_api.timeout_ms.max(1),
        system: req.system,
        format: req.format,
        prompt: req.prompt,
    };
    let started = Instant::now();
    let result = api.client().complete(&request).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let outcome = result.map_err(ClaudeError::from_client).and_then(|o| {
        let usage = o.usage.clone();
        if o.stop_reason.as_deref() == Some("refusal") {
            return Err((ClaudeError::BadResponse("refusal".into()), usage));
        }
        if o.text.trim().is_empty() {
            return Err((ClaudeError::BadResponse(format!("empty text (stop_reason {})", o.stop_reason.as_deref().unwrap_or("none"))), usage));
        }
        Ok(ClaudeReply { text: o.text, model: o.model.unwrap_or_else(|| state.model.clone()), stop_reason: o.stop_reason, usage: o.usage, elapsed_ms })
    });
    match outcome {
        Ok(reply) => {
            api.ledger.record(Row::new(req.use_case, &reply.model, elapsed_ms, None, reply.usage.as_ref(), reply.stop_reason.clone()));
            Ok(reply)
        }
        Err((e, usage)) => {
            api.ledger.record(Row::new(req.use_case, &state.model, elapsed_ms, Some(e.kind()), usage.as_ref(), None));
            Err(e)
        }
    }
}

/// 自己管預算的呼叫者用（changelog 翻譯，#1942）：只送出一次、把結果記進帳本，**不**占 `complete_with` 的每小時名額，
/// 也不讀 key、不看開關（呼叫端已經做過）。回傳原始結果，讓呼叫端保留自己的失敗分類與 usage。
/// `classify` 替一個「200 但不能用」的回覆回填帳本的錯誤分類（呼叫端的判斷，例如 stop_reason 或缺 usage）；`None` 就記成功。
pub async fn complete_recorded(
    api: &ClaudeApi,
    client: &dyn MessagesClient,
    req: &MessagesRequest,
    use_case: &str,
    classify: impl FnOnce(&ClientOutcome) -> Option<&'static str>,
) -> Result<ClientOutcome, ClientError> {
    let started = Instant::now();
    let result = client.complete(req).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let (model, stop_reason, usage, error_kind) = match &result {
        Ok(o) => (o.model.clone(), o.stop_reason.clone(), o.usage.clone(), classify(o)),
        Err(e) => {
            let (err, usage) = ClaudeError::from_client(e.clone());
            (None, None, usage, Some(err.kind()))
        }
    };
    api.ledger.record(Row::new(use_case, model.as_deref().unwrap_or(&req.model), elapsed_ms, error_kind, usage.as_ref(), stop_reason));
    result
}

/// App 上的一份：可注入的 client、用量帳本、每個用途的保險絲。
pub struct ClaudeApi {
    client: Mutex<Arc<dyn MessagesClient>>,
    pub ledger: Ledger,
    sent: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl ClaudeApi {
    pub fn new() -> Self {
        Self { client: Mutex::new(Arc::new(AnthropicHttpClient)), ledger: Ledger::default(), sent: Mutex::new(HashMap::new()) }
    }

    #[cfg(test)]
    pub fn set_client(&self, client: Arc<dyn MessagesClient>) {
        *self.client.lock().unwrap_or_else(|e| e.into_inner()) = client;
    }

    fn client(&self) -> Arc<dyn MessagesClient> {
        self.client.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 先占名額再送（跟 Jev 的 `reserve_slot` 同一個想法）：一小時內已占滿就不送。
    fn reserve(&self, use_case: &str, max_per_hour: u32) -> bool {
        let mut g = self.sent.lock().unwrap_or_else(|e| e.into_inner());
        let q = g.entry(use_case.to_string()).or_default();
        let now = Instant::now();
        while q.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(3600)) {
            q.pop_front();
        }
        if q.len() >= max_per_hour as usize {
            return false;
        }
        q.push_back(now);
        true
    }
}

impl Default for ClaudeApi {
    fn default() -> Self {
        Self::new()
    }
}

// ───────────── 帳本 ─────────────

/// 一次呼叫的記錄（不含 prompt 與回覆內容）。
#[derive(Clone, Debug)]
pub struct Row {
    pub at: String,
    pub use_case: String,
    pub model: String,
    pub elapsed_ms: u64,
    pub error_kind: Option<&'static str>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub stop_reason: Option<String>,
}

impl Row {
    fn new(use_case: &str, model: &str, elapsed_ms: u64, error_kind: Option<&'static str>, usage: Option<&Usage>, stop_reason: Option<String>) -> Self {
        let n = |v: Option<i64>| v.unwrap_or(0).max(0) as u64;
        Self {
            at: crate::db::now(),
            use_case: use_case.into(),
            model: model.into(),
            elapsed_ms,
            error_kind,
            input_tokens: usage.map_or(0, |u| n(u.input_tokens)),
            output_tokens: usage.map_or(0, |u| n(u.output_tokens)),
            stop_reason,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "at": self.at, "use": self.use_case, "model": self.model, "ms": self.elapsed_ms, "ok": self.error_kind.is_none(),
            "error_kind": self.error_kind, "input_tokens": self.input_tokens, "output_tokens": self.output_tokens,
            "stop_reason": self.stop_reason,
        })
    }
}

#[derive(Default)]
struct Totals {
    calls: u64,
    ok: u64,
    errors: u64,
    input_tokens: u64,
    output_tokens: u64,
}

impl Totals {
    fn add(&mut self, row: &Row) {
        self.calls += 1;
        if row.error_kind.is_none() {
            self.ok += 1;
        } else {
            self.errors += 1;
        }
        self.input_tokens += row.input_tokens;
        self.output_tokens += row.output_tokens;
    }

    fn to_json(&self) -> Value {
        json!({"calls": self.calls, "ok": self.ok, "errors": self.errors, "input_tokens": self.input_tokens, "output_tokens": self.output_tokens})
    }
}

/// 記憶體帳本：最近 [`LEDGER_CAP`] 筆＋整個行程期間的累計（總計與每個用途）。daemon 重啟就歸零。
#[derive(Default)]
pub struct Ledger {
    inner: Mutex<LedgerInner>,
}

#[derive(Default)]
struct LedgerInner {
    rows: VecDeque<Row>,
    total: Totals,
    by_use: BTreeMap<String, Totals>,
}

impl Ledger {
    pub fn record(&self, row: Row) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.total.add(&row);
        g.by_use.entry(row.use_case.clone()).or_default().add(&row);
        g.rows.push_front(row);
        g.rows.truncate(LEDGER_CAP);
    }

    /// `{totals, by_use, rows, since}`，rows 新的在前。
    pub fn snapshot(&self, limit: usize) -> Value {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let by_use: serde_json::Map<String, Value> = g.by_use.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
        json!({
            "totals": g.total.to_json(),
            "by_use": by_use,
            "rows": g.rows.iter().take(limit).map(Row::to_json).collect::<Vec<_>>(),
            "since": "daemon_start",
        })
    }
}

// ───────────── 設定與帳本的 HTTP 端點 ─────────────

fn settings_json(cfg: &ConfigFile) -> Value {
    let key = key_status(&cfg.claude_api.key_file);
    let uses: Vec<Value> = CLAUDE_API_USES
        .iter()
        .map(|d| {
            let s = cfg.claude_use_state(d.id).unwrap_or(ClaudeApiUseState { enabled: false, model: d.model.into(), effort: d.effort.into(), max_per_hour: d.max_per_hour });
            json!({
                "id": d.id, "label": d.label, "enabled": s.enabled, "model": s.model, "effort": s.effort, "max_per_hour": s.max_per_hour,
                "default_model": d.model, "default_effort": d.effort, "default_max_per_hour": d.max_per_hour,
                "legacy_section": d.legacy_section,
            })
        })
        .collect();
    json!({
        "enabled": cfg.claude_api.enabled,
        "key_present": key.is_ok(),
        "key_error": key.err(),
        "models": CLAUDE_API_MODELS,
        "efforts": CLAUDE_API_EFFORTS,
        "uses": uses,
    })
}

pub(crate) async fn get_settings(State(app): State<Arc<App>>) -> Json<Value> {
    Json(settings_json(&app.cfg.get().await))
}

#[derive(Deserialize)]
pub(crate) struct UseBody {
    pub enabled: Option<bool>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_per_hour: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct SettingsBody {
    pub enabled: Option<bool>,
    pub uses: Option<BTreeMap<String, UseBody>>,
    pub token: Option<String>,
}

fn validate(body: &SettingsBody) -> Result<(), LcError> {
    for (id, u) in body.uses.iter().flatten() {
        if !CLAUDE_API_USES.iter().any(|d| d.id == id) {
            return Err(LcError::Bad(format!("unknown use: {id}")));
        }
        if let Some(m) = u.model.as_deref() {
            if !claude_model_id_ok(m.trim()) {
                return Err(LcError::Bad(format!("bad model id: {m}")));
            }
        }
        if let Some(e) = u.effort.as_deref() {
            if !CLAUDE_API_EFFORTS.contains(&e.trim()) {
                return Err(LcError::Bad(format!("bad effort: {e}")));
            }
        }
        if u.max_per_hour.is_some_and(|n| !(1..=1000).contains(&n)) {
            return Err(LcError::Bad("max_per_hour must be 1..=1000".into()));
        }
    }
    Ok(())
}

/// 把 body 套到設定上（讀寫都走這一個，所以「要不要開」的判斷與實際寫入不會不一致）。
fn apply(cfg: &mut ConfigFile, body: &SettingsBody) {
    if let Some(enabled) = body.enabled {
        cfg.claude_api.enabled = enabled;
    }
    for (id, u) in body.uses.iter().flatten() {
        if id == "changelog_translate" {
            let t = &mut cfg.changelog_translate;
            if let Some(v) = u.enabled {
                t.enabled = v;
            }
            if let Some(v) = u.model.as_deref() {
                t.model = v.trim().to_string();
            }
            if let Some(v) = u.effort.as_deref() {
                t.effort = v.trim().to_string();
            }
            if let Some(v) = u.max_per_hour {
                t.max_per_hour = v;
            }
            continue;
        }
        let entry = cfg.claude_api.uses.entry(id.clone()).or_default();
        if let Some(v) = u.enabled {
            entry.enabled = v;
        }
        if let Some(v) = u.model.as_deref() {
            entry.model = v.trim().to_string();
        }
        if let Some(v) = u.effort.as_deref() {
            entry.effort = v.trim().to_string();
        }
        if let Some(v) = u.max_per_hour {
            entry.max_per_hour = v;
        }
    }
}

fn anything_on(cfg: &ConfigFile) -> (bool, Vec<bool>) {
    (cfg.claude_api.enabled, CLAUDE_API_USES.iter().map(|d| cfg.claude_use_state(d.id).is_some_and(|s| s.enabled)).collect())
}

pub(crate) async fn put_settings(State(app): State<Arc<App>>, Json(body): Json<SettingsBody>) -> Result<Json<Value>, LcError> {
    validate(&body)?;
    let prev = app.cfg.get().await;
    let key_file = prev.claude_api.key_file.clone();
    let staged = match body.token.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(token) => Some(stage_key(&key_file, token).map_err(|e| LcError::Bad(e.to_string()))?),
        None => None,
    };
    // 任何一層要從「關」變成「開」，key 就必須可用（新貼的算數）。
    let mut next = prev.clone();
    apply(&mut next, &body);
    let (prev_master, prev_uses) = anything_on(&prev);
    let (next_master, next_uses) = anything_on(&next);
    if (next_master && !prev_master) || next_uses.iter().zip(&prev_uses).any(|(n, p)| *n && !*p) {
        let checked = match &staged {
            Some(s) => key_status(&s.staged_path().to_string_lossy()),
            None => key_status(&key_file),
        };
        if let Err(reason) = checked {
            return Err(LcError::Conflict(json!({"error": "needs_key", "reason": reason})));
        }
    }
    let body = Arc::new(body);
    let b = body.clone();
    app.cfg
        .update(move |c| {
            apply(c, &b);
            Ok(())
        })
        .await
        .map_err(crate::api::cfg_err)?;
    if let Some(staged) = staged {
        if let Err(e) = staged.publish() {
            let _ = app
                .cfg
                .update(move |c| {
                    c.claude_api = prev.claude_api;
                    c.changelog_translate = prev.changelog_translate;
                    Ok(())
                })
                .await;
            return Err(LcError::Upstream(format!("key publish failed: {e}")));
        }
    }
    Ok(Json(settings_json(&app.cfg.get().await)))
}

#[derive(Deserialize)]
pub(crate) struct LedgerQuery {
    limit: Option<usize>,
}

pub(crate) async fn get_ledger(State(app): State<Arc<App>>, Query(q): Query<LedgerQuery>) -> Json<Value> {
    Json(app.claude_api.ledger.snapshot(q.limit.unwrap_or(20).clamp(1, LEDGER_CAP)))
}

pub fn routes() -> axum::Router<Arc<App>> {
    use axum::routing::get;
    axum::Router::new().route("/claude-api/settings", get(get_settings).put(put_settings)).route("/claude-api/ledger", get(get_ledger))
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        calls: AtomicUsize,
        seen: Mutex<Vec<Value>>,
        next: Mutex<VecDeque<Result<ClientOutcome, ClientError>>>,
    }

    impl Fake {
        fn new(steps: Vec<Result<ClientOutcome, ClientError>>) -> Arc<Self> {
            Arc::new(Self { calls: AtomicUsize::new(0), seen: Mutex::new(Vec::new()), next: Mutex::new(steps.into()) })
        }
    }

    impl MessagesClient for Fake {
        fn complete<'a>(&'a self, req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut body = req.request_json();
            body["_key_seen"] = json!(req.key);
            self.seen.lock().unwrap().push(body);
            let step = self.next.lock().unwrap().pop_front().expect("unexpected extra call");
            Box::pin(async move { step })
        }
    }

    fn ok(text: &str, input: i64, output: i64) -> Result<ClientOutcome, ClientError> {
        Ok(ClientOutcome {
            text: text.into(),
            model: Some("claude-sonnet-5-5".into()),
            stop_reason: Some("end_turn".into()),
            usage: Some(Usage { input_tokens: Some(input), output_tokens: Some(output), cache_creation_input_tokens: None, cache_read_input_tokens: None }),
        })
    }

    async fn app_with_key() -> (Arc<App>, std::path::PathBuf) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-claude-api-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let db = crate::app_ports_p1::open(&dir.join("t.sqlite3")).await.unwrap();
        let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let key = dir.join("key").to_string_lossy().into_owned();
        store
            .update(move |c| {
                c.claude_api.key_file = key;
                Ok(())
            })
            .await
            .unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        (App::new(db, client.clone(), client, store, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false), dir)
    }

    fn put(app: &Arc<App>, body: Value) -> impl Future<Output = Result<Json<Value>, LcError>> {
        put_settings(State(app.clone()), Json(serde_json::from_value(body).unwrap()))
    }

    fn req(use_case: &'static str) -> ClaudeRequest {
        ClaudeRequest { use_case, system: Some("sys".into()), prompt: "hello".into(), max_tokens: 300, format: None }
    }

    #[tokio::test]
    async fn a_call_needs_both_switches_and_a_key_and_is_recorded() {
        let (app, dir) = app_with_key().await;
        let fake = Fake::new(vec![ok("hi", 100, 20)]);
        app.claude_api.set_client(fake.clone());
        assert_eq!(complete(&app, req("changelog_review")).await.unwrap_err(), ClaudeError::Disabled, "預設全關");
        assert_eq!(complete(&app, req("nope")).await.unwrap_err(), ClaudeError::Disabled);

        let needs = put(&app, json!({"enabled": true})).await.unwrap_err();
        assert!(matches!(needs, LcError::Conflict(ref v) if v["error"] == "needs_key"), "沒 key 不給開");
        put(&app, json!({"enabled": true, "token": "sk-ant-test-0001"})).await.unwrap();
        assert_eq!(complete(&app, req("changelog_review")).await.unwrap_err(), ClaudeError::Disabled, "用途沒開");
        put(&app, json!({"uses": {"changelog_review": {"enabled": true}}})).await.unwrap();

        let reply = complete(&app, req("changelog_review")).await.unwrap();
        assert_eq!((reply.text.as_str(), reply.model.as_str()), ("hi", "claude-sonnet-5-5"));
        let seen = fake.seen.lock().unwrap()[0].clone();
        assert_eq!(seen["model"], "claude-sonnet-5-5");
        assert_eq!(seen["output_config"]["effort"], "medium");
        assert_eq!(seen["system"], "sys");
        assert_eq!(seen["_key_seen"], "sk-ant-test-0001");
        assert!(seen.get("temperature").is_none() && seen.get("tools").is_none());

        let snap = app.claude_api.ledger.snapshot(10);
        assert_eq!(snap["totals"]["calls"], 1);
        assert_eq!(snap["totals"]["input_tokens"], 100);
        assert_eq!(snap["rows"][0]["use"], "changelog_review");
        assert_eq!(snap["rows"][0]["ok"], true);
        assert!(!snap.to_string().contains("sk-ant"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn failures_are_classified_recorded_and_never_retried() {
        let (app, dir) = app_with_key().await;
        put(&app, json!({"enabled": true, "token": "sk-ant-test-0002", "uses": {"changelog_review": {"enabled": true, "model": "claude-haiku-5-5", "max_per_hour": 4}}})).await.unwrap();
        let steps = vec![
            Err(ClientError::Http { status: 401, usage: None }),
            Err(ClientError::RateLimited { retry_after: Some("7".into()), usage: None }),
            Err(ClientError::Http { status: 529, usage: None }),
            Err(ClientError::Timeout),
        ];
        let fake = Fake::new(steps);
        app.claude_api.set_client(fake.clone());
        let mut kinds = Vec::new();
        let mut errs = Vec::new();
        for _ in 0..4 {
            let e = complete(&app, req("changelog_review")).await.unwrap_err();
            kinds.push(e.kind());
            errs.push(e);
        }
        assert_eq!(kinds, ["auth", "rate_limited", "server", "timeout"]);
        assert_eq!(errs[1], ClaudeError::RateLimited { retry_after_secs: Some(7) });
        assert_eq!(fake.calls.load(Ordering::SeqCst), 4, "一個失敗只送一次");
        assert_eq!(fake.seen.lock().unwrap()[0]["model"], "claude-haiku-5-5");
        // 名額用完（max_per_hour = 4）：不送、不記帳本。
        assert_eq!(complete(&app, req("changelog_review")).await.unwrap_err(), ClaudeError::Budget);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 4);
        let snap = app.claude_api.ledger.snapshot(10);
        assert_eq!((snap["totals"]["calls"].clone(), snap["totals"]["errors"].clone()), (json!(4), json!(4)));
        assert_eq!(snap["rows"][0]["error_kind"], "timeout");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn empty_or_refused_replies_are_errors_not_successes() {
        let (app, dir) = app_with_key().await;
        put(&app, json!({"enabled": true, "token": "sk-ant-test-0003", "uses": {"changelog_review": {"enabled": true}}})).await.unwrap();
        let refusal = Ok(ClientOutcome { text: String::new(), model: None, stop_reason: Some("refusal".into()), usage: None });
        let fake = Fake::new(vec![ok("   ", 5, 1), refusal]);
        app.claude_api.set_client(fake);
        assert_eq!(complete(&app, req("changelog_review")).await.unwrap_err().kind(), "bad_response");
        assert_eq!(complete(&app, req("changelog_review")).await.unwrap_err().kind(), "bad_response");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_translate_use_keeps_its_old_section_and_ignores_the_master_switch() {
        let (app, dir) = app_with_key().await;
        put(&app, json!({"token": "sk-ant-test-0004"})).await.unwrap();
        let saved = put(&app, json!({"uses": {"changelog_translate": {"enabled": true, "model": "claude-sonnet-5-5"}}})).await.unwrap().0;
        let cfg = app.cfg.get().await;
        assert!(cfg.changelog_translate.enabled && !cfg.claude_api.enabled);
        assert_eq!(cfg.changelog_translate.model, "claude-sonnet-5-5");
        assert!(cfg.claude_api.uses.is_empty(), "舊用途不寫進 [claude_api.uses]");
        let t = saved["uses"].as_array().unwrap().iter().find(|u| u["id"] == "changelog_translate").unwrap().clone();
        assert_eq!((t["enabled"].clone(), t["model"].clone(), t["legacy_section"].clone()), (json!(true), json!("claude-sonnet-5-5"), json!("changelog_translate")));
        // 兩條路讀同一個 key 檔。
        assert_eq!(cfg.changelog_translate_key_file(), cfg.claude_api.key_file);
        let fake = Fake::new(vec![ok("{}", 1, 1)]);
        app.claude_api.set_client(fake);
        complete(&app, req("changelog_translate")).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn settings_never_return_the_key_and_reject_bad_input() {
        let (app, dir) = app_with_key().await;
        let saved = put(&app, json!({"enabled": true, "token": "sk-ant-test-0005"})).await.unwrap().0;
        assert_eq!(saved["key_present"], true);
        assert!(!saved.to_string().contains("sk-ant-test-0005"));
        assert!(!get_settings(State(app.clone())).await.0.to_string().contains("sk-ant-test-0005"));
        assert!(!std::fs::read_to_string(dir.join("config.toml")).unwrap().contains("sk-ant-test-0005"), "key 不進 config.toml");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("key")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        for bad in [
            json!({"uses": {"ghost": {"enabled": true}}}),
            json!({"uses": {"changelog_review": {"model": "gpt-4"}}}),
            json!({"uses": {"changelog_review": {"effort": "ultra"}}}),
            json!({"uses": {"changelog_review": {"max_per_hour": 0}}}),
        ] {
            assert!(matches!(put(&app, bad).await, Err(LcError::Bad(_))));
        }
        // 只關開關：key 還在。
        let off = put(&app, json!({"enabled": false, "token": "  "})).await.unwrap().0;
        assert_eq!((off["enabled"].clone(), off["key_present"].clone()), (json!(false), json!(true)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn request_body_has_no_sampling_params_and_debug_hides_the_key() {
        let r = MessagesRequest {
            endpoint: "http://127.0.0.1:9/x".into(),
            key: "sk-ant-secret".into(),
            model: "claude-haiku-5-5".into(),
            effort: String::new(),
            max_tokens: 10,
            timeout_ms: 1000,
            system: None,
            format: None,
            prompt: "p".into(),
        };
        let body = r.request_json();
        assert!(body.get("output_config").is_none() && body.get("system").is_none());
        assert!(!format!("{r:?}").contains("sk-ant-secret"));
    }
}

// #1944: additive full-content boundary; the existing one-shot clients above are unchanged.
pub(crate) use crate::api_bot::http::{FullMessagesRequest, Error as FullMessagesError};

#[allow(dead_code)]
pub(crate) async fn complete_full_messages(
    request: &FullMessagesRequest<'_>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<crate::api_bot::protocol::Response, FullMessagesError> {
    crate::api_bot::http::complete(request, cancel).await
}
