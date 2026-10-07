use serde::Serialize;
use serde_json::Value;

/// A sequenced event broadcast to connected UI clients.
#[derive(Debug, Clone, Serialize)]
pub struct WsEvent {
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
}
