//! Full-content Messages transport, separate from the existing text-only client contract.
use super::protocol::{self, Message, Options, Response};
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

pub async fn complete(
    req: &FullMessagesRequest<'_>,
    cancel: &CancellationToken,
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
