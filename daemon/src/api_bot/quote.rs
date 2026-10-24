//! Complete immutable request identity and an explicitly unapproved pricing seam.
//! No body, key, or provider error is formatted into diagnostics here.
use super::protocol::{self, Message, Options, Response};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Owns every input to the sole wire builder. Accessors cannot mutate quoted inputs.
/// Deliberately no Debug or Serialize implementation: requests contain private content.
pub(crate) struct RequestEnvelope {
    options: Options,
    system: Value,
    messages: Vec<Message>,
    hash: String,
}
impl RequestEnvelope {
    pub(crate) fn new(options: Options, system: Value, messages: Vec<Message>) -> Result<Self, QuoteError> {
        options.validate().map_err(|_| QuoteError::InvalidRequest)?;
        if messages.is_empty() { return Err(QuoteError::InvalidRequest); }
        let body = protocol::request(&options, &system, &messages);
        // Recursively sort object keys, independent of serde_json feature unification.
        let canonical = canonicalize(&body);
        let bytes = serde_json::to_vec(&canonical).map_err(|_| QuoteError::InvalidRequest)?;
        let hash = format!("{:x}", Sha256::digest(bytes));
        Ok(Self { options, system, messages, hash })
    }
    pub(crate) fn hash(&self) -> &str { &self.hash }
    pub(crate) fn options(&self) -> &Options { &self.options }
    pub(crate) fn system(&self) -> &Value { &self.system }
    pub(crate) fn messages(&self) -> &[Message] { &self.messages }
    pub(crate) fn speed(&self) -> &'static str { if self.options.fast { "fast" } else { "standard" } }
    pub(crate) fn body(&self) -> Value { protocol::request(&self.options, &self.system, &self.messages) }
}
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort_unstable();
            let mut sorted = serde_json::Map::new();
            for key in keys { sorted.insert(key.clone(), canonicalize(&map[key])); }
            Value::Object(sorted)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        _ => value.clone(),
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuoteError { Unapproved, InvalidRequest, UntrustedUsage }
/// Integer microdollars; never inferred from checkpoint bytes or an unapproved rate.
pub(crate) struct ReservationQuote {
    pub(crate) reserved_microusd: i64,
    pub(crate) pricing_version: String,
}
pub(crate) struct UsageQuote {
    pub(crate) estimated_microusd: i64,
    pub(crate) actual_speed: Option<&'static str>,
}
/// Implementations must validate complete usage, not treat missing token counts as zero.
/// Both methods are pure: no HTTP, key access, or speculative token-count requests.
pub(crate) trait QuoteProvider: Send + Sync {
    fn reserve(&self, request: &RequestEnvelope) -> Result<ReservationQuote, QuoteError>;
    fn settle(&self, request: &RequestEnvelope, response: &Response) -> Result<UsageQuote, QuoteError>;
}
/// The only production provider in C1. Approval of prices and token bounds belongs to D.
pub(crate) struct FailClosed;
impl QuoteProvider for FailClosed {
    fn reserve(&self, _: &RequestEnvelope) -> Result<ReservationQuote, QuoteError> { Err(QuoteError::Unapproved) }
    fn settle(&self, _: &RequestEnvelope, _: &Response) -> Result<UsageQuote, QuoteError> { Err(QuoteError::Unapproved) }
}
