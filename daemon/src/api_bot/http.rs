//! Full-content Messages transport, separate from the existing text-only client contract.
use super::protocol::{self, Message, Options, Response};
use super::sse::{StreamAssembler, StreamError, StreamProgress};
use serde_json::Value;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct FullMessagesRequest<'a> {
    pub endpoint: &'a str,
    pub key: &'a str,
    pub options: &'a Options,
    pub system: &'a Value,
    pub messages: &'a [Message],
    pub timeout: Duration,
    pub max_response_bytes: usize,
}
// No Debug: endpoint/key/system/messages may contain credentials or private instructions.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidRequest,
    Cancelled,
    Timeout,
    TransportUnknown,
    RateLimited { retry_after_secs: Option<u64> },
    Http {
        status: u16,
        kind: Option<String>,
        message: Option<String>,
        request_id: Option<String>,
    },
    /// HTTP 200 之後，串流中途收到 `error` 事件：模型可能已經產生並計費，所以**不是**確定沒處理的拒絕
    /// （不可當成 4xx／429／529 記 $0；附帶的 `kind`／`message` 只給使用者看原因）。
    StreamAborted { kind: Option<String>, message: Option<String> },
    TooLarge,
    InvalidResponse,
    InvalidResponseDetail(String),
}

/// 串流進度回呼的最短間隔。
/// 串流進度回呼的最短間隔（Claude 與 OpenAI 共用，節奏一致）。
pub(crate) const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// 串流中的 `error` 事件（已是 HTTP 200 之後）→ `StreamAborted`，不套用 HTTP 狀態碼的「確定沒處理」語意。
fn stream_error(e: StreamError) -> Error {
    match e {
        StreamError::Invalid => Error::InvalidResponse,
        StreamError::Api { kind, message } => {
            let kind = Some(kind.trim().to_string()).filter(|k| !k.is_empty());
            let message = Some(crate::judge::mask(&message).chars().take(500).collect::<String>()).filter(|m| !m.is_empty());
            tracing::warn!(kind = ?kind, message = ?message, "api bot stream error event");
            Error::StreamAborted { kind, message }
        }
    }
}

pub async fn complete(
    req: &FullMessagesRequest<'_>,
    cancel: &CancellationToken,
) -> Result<Response, Error> {
    complete_with_progress(req, cancel, None).await
}

/// 與 `complete` 相同，但串流進行中會（最多每秒一次）呼叫 `progress`。
/// 這個入口自己編碼 body（測試與 fixture 用）；正式路徑用 [`complete_encoded`]，送出報價與 hash 用的那一份位元組。
pub async fn complete_with_progress(
    req: &FullMessagesRequest<'_>,
    cancel: &CancellationToken,
    progress: Option<&(dyn Fn(&StreamProgress) + Send + Sync)>,
) -> Result<Response, Error> {
    let body = serde_json::to_vec(&protocol::request(req.options, req.system, req.messages)).map_err(|_| Error::InvalidRequest)?;
    let beta = protocol::beta_header(req.options, req.messages);
    let wire = Wire { endpoint: req.endpoint, key: req.key, options: req.options, has_messages: !req.messages.is_empty(), beta: &beta, body: &body, official_host: super::anthropic::OFFICIAL_HOST, timeout: req.timeout, max_response_bytes: req.max_response_bytes };
    Box::pin(send(&wire, cancel, progress)).await
}

/// 已編碼的請求：body 位元組、beta header 都取自 [`RequestEnvelope`]——跟報價、request hash 是同一份，不再重新組 JSON。
pub struct EncodedMessagesRequest<'a> {
    pub endpoint: &'a str,
    pub key: &'a str,
    pub request: &'a super::quote::RequestEnvelope,
    /// provider 的官方主機（正式建置只准連這一台的 443）。
    pub official_host: &'static str,
    pub timeout: Duration,
    pub max_response_bytes: usize,
}

pub async fn complete_encoded(
    req: &EncodedMessagesRequest<'_>,
    cancel: &CancellationToken,
    progress: Option<&(dyn Fn(&StreamProgress) + Send + Sync)>,
) -> Result<Response, Error> {
    let wire = Wire {
        endpoint: req.endpoint,
        key: req.key,
        options: req.request.options(),
        has_messages: !req.request.messages().is_empty(),
        beta: req.request.beta_header(),
        body: req.request.wire(),
        official_host: req.official_host,
        timeout: req.timeout,
        max_response_bytes: req.max_response_bytes,
    };
    Box::pin(send(&wire, cancel, progress)).await
}

/// 真正上線的那一份：兩個入口都收斂到這裡。
struct Wire<'a> {
    endpoint: &'a str,
    key: &'a str,
    options: &'a Options,
    has_messages: bool,
    beta: &'a str,
    body: &'a [u8],
    #[cfg_attr(test, allow(dead_code))]
    official_host: &'static str,
    timeout: Duration,
    max_response_bytes: usize,
}

async fn send(
    req: &Wire<'_>,
    cancel: &CancellationToken,
    progress: Option<&(dyn Fn(&StreamProgress) + Send + Sync)>,
) -> Result<Response, Error> {
    req.options.validate().map_err(|_| Error::InvalidRequest)?;
    if req.key.trim().is_empty()
        || !req.has_messages
        || req.timeout.is_zero()
        || !(1..=8 * 1024 * 1024).contains(&req.max_response_bytes)
    {
        return Err(Error::InvalidRequest);
    }
    let url = reqwest::Url::parse(req.endpoint).map_err(|_| Error::InvalidRequest)?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(Error::InvalidRequest);
    }
    #[cfg(not(test))]
    if url.scheme() != "https"
        || url.host_str() != Some(req.official_host)
        || url.port_or_known_default() != Some(443)
    {
        return Err(Error::InvalidRequest);
    }
    // Tests can ONLY reach literal loopback. DNS names and real API endpoints are refused.
    #[cfg(test)]
    if url.scheme() != "http"
        || !url.host_str().is_some_and(|h| {
            h.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        })
    {
        return Err(Error::InvalidRequest);
    }
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(req.timeout);
    // Even a developer's HTTP_PROXY must not send fixture traffic outside loopback.
    #[cfg(test)]
    let builder = builder.no_proxy();
    let client = builder.build().map_err(|_| Error::InvalidRequest)?;
    let operation = async {
        // `.json(value)` 等於 Content-Type: application/json ＋ serde_json::to_vec(value)；這裡送的就是事先編好的那一份。
        let mut response = client
            .post(url)
            .header("x-api-key", req.key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", req.beta)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(req.body.to_vec())
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    Error::Timeout
                } else {
                    Error::TransportUnknown
                }
            })?;
        let status = response.status().as_u16();
        if status == 429 {
            return Err(Error::RateLimited {
                retry_after_secs: response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok()),
            });
        }
        if !(200..300).contains(&status) {
            let request_id = response
                .headers()
                .get("request-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let mut bytes = Vec::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                let remaining = 4096_usize.saturating_sub(bytes.len());
                if remaining == 0 {
                    break;
                }
                let take = chunk.len().min(remaining);
                bytes.extend_from_slice(&chunk[..take]);
                if bytes.len() >= 4096 {
                    break;
                }
            }
            let (kind, message) = if let Ok(val) = serde_json::from_slice::<Value>(&bytes) {
                let (raw_kind, raw_message) = if let Some(err) = val.get("error").and_then(Value::as_object) {
                    (
                        err.get("type").and_then(Value::as_str),
                        err.get("message").and_then(Value::as_str),
                    )
                } else {
                    (None, None)
                };
                let kind = raw_kind.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
                let message = raw_message.map(|m| {
                    let masked = crate::judge::mask(m);
                    masked.chars().take(500).collect::<String>()
                }).filter(|m| !m.is_empty());
                (kind, message)
            } else {
                (None, None)
            };
            tracing::warn!(status, kind = ?kind, message = ?message, request_id = ?request_id, "api bot http error response");
            return Err(Error::Http {
                status,
                kind,
                message,
                request_id,
            });
        }
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/event-stream"));
        if is_sse {
            let mut assembler = StreamAssembler::new();
            let mut total = 0_usize;
            let mut last_report = tokio::time::Instant::now();
            while let Some(chunk) = response.chunk().await.map_err(|e| {
                if e.is_timeout() {
                    Error::Timeout
                } else {
                    Error::TransportUnknown
                }
            })? {
                total = total.saturating_add(chunk.len());
                if total > req.max_response_bytes {
                    return Err(Error::TooLarge);
                }
                assembler.feed(&chunk).map_err(stream_error)?;
                if let Some(report) = progress {
                    if last_report.elapsed() >= PROGRESS_INTERVAL {
                        last_report = tokio::time::Instant::now();
                        report(assembler.progress());
                    }
                }
            }
            let parsed = assembler.finish().map_err(stream_error)?;
            // 模型與工具內容的驗證交給 adapter：這裡拒絕會丟掉已收到的 usage，attempt 只能留成 unknown。
            return Ok(parsed);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            if e.is_timeout() {
                Error::Timeout
            } else {
                Error::TransportUnknown
            }
        })? {
            if bytes.len().saturating_add(chunk.len()) > req.max_response_bytes {
                return Err(Error::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        let parsed: Response =
            serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)?;
        Ok(parsed)
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = tokio::time::timeout(req.timeout, operation) => result.map_err(|_| Error::Timeout)?,
    }
}
