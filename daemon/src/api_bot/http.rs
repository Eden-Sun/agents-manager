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
    TooLarge,
    InvalidResponse,
}

/// 串流進度回呼的最短間隔。
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// 串流中的 `error` 事件對應回與非串流 HTTP 錯誤相同的 `Error` 種類。
fn stream_error(e: StreamError) -> Error {
    match e {
        StreamError::Invalid => Error::InvalidResponse,
        StreamError::Api { kind, message } => {
            if kind == "rate_limit_error" {
                return Error::RateLimited { retry_after_secs: None };
            }
            let status = match kind.as_str() {
                "invalid_request_error" => 400,
                "authentication_error" => 401,
                "billing_error" => 402,
                "permission_error" => 403,
                "not_found_error" => 404,
                "request_too_large" => 413,
                "timeout_error" => 504,
                "overloaded_error" => 529,
                _ => 500,
            };
            let kind = Some(kind.trim().to_string()).filter(|k| !k.is_empty());
            let message = Some(crate::judge::mask(&message).chars().take(500).collect::<String>()).filter(|m| !m.is_empty());
            tracing::warn!(status, kind = ?kind, message = ?message, "api bot stream error event");
            Error::Http { status, kind, message, request_id: None }
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
pub async fn complete_with_progress(
    req: &FullMessagesRequest<'_>,
    cancel: &CancellationToken,
    progress: Option<&(dyn Fn(&StreamProgress) + Send + Sync)>,
) -> Result<Response, Error> {
    req.options.validate().map_err(|_| Error::InvalidRequest)?;
    if req.key.trim().is_empty()
        || req.messages.is_empty()
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
        || url.host_str() != Some("api.anthropic.com")
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
        let beta_header = if req.options.compact_at_input_tokens > 0 {
            format!("{},{}", protocol::THINKING_BINDING_BETA, protocol::COMPACT_BETA)
        } else {
            protocol::THINKING_BINDING_BETA.to_string()
        };
        let mut response = client
            .post(url)
            .header("x-api-key", req.key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", beta_header)
            .json(&protocol::request(req.options, req.system, req.messages))
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
            if parsed.model != req.options.model { return Err(Error::InvalidResponse); }
            parsed.tools().map_err(|_| Error::InvalidResponse)?;
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
        if parsed.model != req.options.model { return Err(Error::InvalidResponse); }
        parsed.tools().map_err(|_| Error::InvalidResponse)?;
        Ok(parsed)
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = tokio::time::timeout(req.timeout, operation) => result.map_err(|_| Error::Timeout)?,
    }
}
