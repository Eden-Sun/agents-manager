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
    Http(u16),
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
        let mut response = client
            .post(url)
            .header("x-api-key", req.key)
            .header("anthropic-version", "2023-06-01")
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
            return Err(Error::Http(status));
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
        parsed.tools().map_err(|_| Error::InvalidResponse)?;
        Ok(parsed)
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        result = tokio::time::timeout(req.timeout, operation) => result.map_err(|_| Error::Timeout)?,
    }
}
