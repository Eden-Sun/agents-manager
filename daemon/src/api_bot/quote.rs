//! Complete immutable request identity and an explicitly unapproved pricing seam.
//! No body, key, or provider error is formatted into diagnostics here.
use super::protocol::{self, Message, Options, Response};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 已編碼、不可變的請求：送上線的 body 位元組、同一份 body 的 JSON 值（報價量大小用）、request hash、beta header。
/// 建好之後沒有任何可變的入口；報價、算 hash、實際送出都讀這一份。沒有 Debug／Serialize：內容是私密的。
pub(crate) struct EncodedRequest {
    body: Value,
    wire: Vec<u8>,
    hash: String,
    beta_header: String,
}
impl EncodedRequest {
    /// 送上線的位元組（`serde_json::to_vec(body)`，與 reqwest `.json()` 產生的相同）。
    pub(crate) fn wire(&self) -> &[u8] {
        &self.wire
    }
    pub(crate) fn body(&self) -> &Value {
        &self.body
    }
    pub(crate) fn hash(&self) -> &str {
        &self.hash
    }
    pub(crate) fn beta_header(&self) -> &str {
        &self.beta_header
    }
}

/// Owns every input to the sole wire builder. Accessors cannot mutate quoted inputs.
/// Deliberately no Debug or Serialize implementation: requests contain private content.
#[derive(Clone)]
pub(crate) struct RequestEnvelope {
    options: Options,
    system: Value,
    messages: Vec<Message>,
    encoded: std::sync::Arc<EncodedRequest>,
    previous_response: Option<Response>,
}
impl RequestEnvelope {
    pub(crate) fn new(
        options: Options,
        system: Value,
        messages: Vec<Message>,
    ) -> Result<Self, QuoteError> {
        options.validate().map_err(|_| QuoteError::InvalidRequest)?;
        if messages.is_empty() {
            return Err(QuoteError::InvalidRequest);
        }
        // 唯一一次編碼：之後報價、hash、送出都用這份，不再呼叫 `protocol::request`。
        let body = protocol::request(&options, &system, &messages);
        let wire = serde_json::to_vec(&body).map_err(|_| QuoteError::InvalidRequest)?;
        // Recursively sort object keys, independent of serde_json feature unification.
        let canonical = canonicalize(&body);
        let bytes = serde_json::to_vec(&canonical).map_err(|_| QuoteError::InvalidRequest)?;
        let hash = format!("{:x}", Sha256::digest(bytes));
        let beta_header = protocol::beta_header(&options, &messages);
        Ok(Self {
            options,
            system,
            messages,
            encoded: std::sync::Arc::new(EncodedRequest { body, wire, hash, beta_header }),
            previous_response: None,
        })
    }
    pub(crate) fn encoded(&self) -> &EncodedRequest {
        &self.encoded
    }
    /// 送上線的位元組。
    pub(crate) fn wire(&self) -> &[u8] {
        self.encoded.wire()
    }
    pub(crate) fn beta_header(&self) -> &str {
        self.encoded.beta_header()
    }
    pub(crate) fn hash(&self) -> &str {
        self.encoded.hash()
    }
    pub(crate) fn options(&self) -> &Options {
        &self.options
    }
    pub(crate) fn system(&self) -> &Value {
        &self.system
    }
    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }
    pub(crate) fn speed(&self) -> &'static str {
        if self.options.fast {
            "fast"
        } else {
            "standard"
        }
    }
    /// 已編碼的 body（JSON 值）；不重新組。
    pub(crate) fn body(&self) -> &Value {
        self.encoded.body()
    }
    pub(crate) fn previous_response(&self) -> Option<&Response> {
        self.previous_response.as_ref()
    }
    pub(crate) fn set_previous_response(&mut self, prev: Option<Response>) {
        self.previous_response = prev;
    }
    pub(crate) fn with_previous_response(mut self, prev: Option<Response>) -> Self {
        self.previous_response = prev;
        self
    }
}
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_unstable();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonicalize(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        _ => value.clone(),
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuoteError {
    Unapproved,
    InvalidRequest,
    UntrustedUsage,
}
/// Integer microdollars; never inferred from checkpoint bytes or an unapproved rate.
pub(crate) struct ReservationQuote {
    pub(crate) reserved_microusd: i64,
    pub(crate) pricing_version: String,
}
#[derive(Clone)]
pub(crate) struct UsageQuote {
    pub(crate) estimated_microusd: i64,
    pub(crate) actual_speed: Option<&'static str>,
}
/// Implementations must validate complete usage, not treat missing token counts as zero.
/// Both methods are pure: no HTTP, key access, or speculative token-count requests.
pub(crate) trait QuoteProvider: Send + Sync {
    fn reserve(&self, request: &RequestEnvelope) -> Result<ReservationQuote, QuoteError>;
    fn settle(
        &self,
        request: &RequestEnvelope,
        response: &Response,
    ) -> Result<UsageQuote, QuoteError>;
}
/// The only production provider in C1. Approval of prices and token bounds belongs to D.
pub(crate) struct FailClosed;
impl QuoteProvider for FailClosed {
    fn reserve(&self, _: &RequestEnvelope) -> Result<ReservationQuote, QuoteError> {
        Err(QuoteError::Unapproved)
    }
    fn settle(&self, _: &RequestEnvelope, _: &Response) -> Result<UsageQuote, QuoteError> {
        Err(QuoteError::Unapproved)
    }
}
