//! OpenAI Responses API 的單發文字補全（openai-api 切片 F）：給 `claude_api` 的用途層當另一個 [`MessagesClient`]。
//!
//! 一次請求＝`instructions`（system）＋一則 user 訊息，沒有工具、`store:false`、串流組裝。編碼、SSE 與錯誤分類用
//! `api_bot::openai::codec`，計價用 `api_bot::openai::pricing`。回傳沿用 Claude 那邊的 [`ClientOutcome`]／[`Usage`] 形狀，
//! 呼叫端（翻譯）不用知道差別。
//!
//! **費用不明就不是成功**：回應沒有可信的用量、回報的不是標準計價層級、或有我們沒要求的 12 小時快取寫入時，
//! 回一個沒有文字的結果（`stop_reason` 寫明原因），用途層會記成 `bad_response`、不記花費，不會當成 0 元的成功。
//! key 只放在 `Authorization` header，不進任何 log 或錯誤。
use crate::api_bot::openai::codec::{self, CodecError, ErrorClass, IncompleteReason, InputItem, Outcome, ResponseAssembler, StopKind};
use crate::api_bot::openai::pricing;
use crate::claude_api::{ClientError, ClientOutcome, MessagesClient, MessagesRequest, Usage};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub(crate) use crate::api_bot::openai::provider::{OFFICIAL_ENDPOINT, OFFICIAL_HOST};

/// 回應上限（位元組）：翻譯的輸出遠小於這個數，超過就當成壞掉的回應。
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub struct OpenAiTextClient;

impl MessagesClient for OpenAiTextClient {
    fn complete<'a>(&'a self, req: &'a MessagesRequest) -> Pin<Box<dyn Future<Output = Result<ClientOutcome, ClientError>> + Send + 'a>> {
        Box::pin(complete(req))
    }
}

/// OpenAI 的用量 → 用途層共用的 [`Usage`]（Claude 的欄位語意：`input_tokens` 不含快取）。
pub(crate) fn to_use_usage(u: &pricing::Usage) -> Usage {
    let n = |v: u64| i64::try_from(v).ok();
    Usage { input_tokens: n(u.ordinary_input()), output_tokens: n(u.output_tokens), cache_creation_input_tokens: n(u.cache_write_tokens), cache_read_input_tokens: n(u.cached_tokens) }
}

/// [`to_use_usage`] 的反向（計價用）；欄位缺漏或為負回 `None`。
pub(crate) fn from_use_usage(u: &Usage) -> Option<pricing::Usage> {
    let n = |v: Option<i64>| u64::try_from(v.unwrap_or(0)).ok();
    let (input, output, write, read) = (n(u.input_tokens)?, n(u.output_tokens)?, n(u.cache_creation_input_tokens)?, n(u.cache_read_input_tokens)?);
    Some(pricing::Usage { input_tokens: input.checked_add(write)?.checked_add(read)?, cached_tokens: read, cache_write_tokens: write, cache_write_12h_tokens: 0, output_tokens: output, reasoning_tokens: 0 })
}

/// 這個 OpenAI 模型、這份用量的微美元（標準層級）；沒有價表或金額算不出來回 `None`。
pub(crate) fn cost_microusd(model: &str, u: &Usage) -> Option<i64> {
    pricing::settle(model, &from_use_usage(u)?, None).ok().and_then(|c| i64::try_from(c).ok())
}

/// 內容不能用（費用不明或協定錯誤）：沒有文字，`stop_reason` 說明原因。
fn unusable(req: &MessagesRequest, why: &str) -> ClientOutcome {
    ClientOutcome { text: String::new(), model: Some(req.model.clone()), stop_reason: Some(why.to_string()), usage: None }
}

fn class_error(class: ErrorClass) -> ClientError {
    match class {
        ErrorClass::Auth => ClientError::Http { status: 401, usage: None },
        ErrorClass::Permission => ClientError::Http { status: 403, usage: None },
        ErrorClass::RateLimited { retry_after_secs } => ClientError::RateLimited { retry_after: retry_after_secs.map(|s| s.to_string()), usage: None },
        // 額度用完不是等一下就好：不回 RateLimited。
        ErrorClass::InsufficientQuota => ClientError::Http { status: 429, usage: None },
        ErrorClass::ModelRejected { .. } | ErrorClass::InvalidRequest { .. } => ClientError::Http { status: 400, usage: None },
        ErrorClass::Unknown => ClientError::Http { status: 502, usage: None },
    }
}

async fn complete(req: &MessagesRequest) -> Result<ClientOutcome, ClientError> {
    let refused = || ClientError::Http { status: 0, usage: None };
    let url = reqwest::Url::parse(&req.endpoint).map_err(|_| refused())?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() || req.key.trim().is_empty() {
        return Err(refused());
    }
    #[cfg(not(test))]
    if url.scheme() != "https" || url.host_str() != Some(OFFICIAL_HOST) || url.port_or_known_default() != Some(443) {
        return Err(refused());
    }
    // 測試只連得到字面上的 loopback。
    #[cfg(test)]
    if url.scheme() != "http" || !url.host_str().is_some_and(|h| h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())) {
        return Err(refused());
    }
    // 結構化輸出（JSON schema）這條路徑沒有接：寧可拒絕，不送一個會被忽略的格式要求。
    if req.format.is_some() {
        return Err(ClientError::Http { status: 400, usage: None });
    }
    let request = codec::Request {
        model: req.model.clone(),
        instructions: req.system.clone().unwrap_or_default(),
        input: vec![InputItem::User(req.prompt.clone())],
        tools: Vec::new(),
        effort: Some(req.effort.trim()).filter(|e| !e.is_empty()).map(str::to_string),
        max_output_tokens: u64::from(req.max_tokens),
    };
    let body = request.body().map_err(|_| ClientError::Http { status: 400, usage: None })?;
    // 不跟隨任何 redirect（會把 Authorization 帶去別處）。
    let builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_millis(req.timeout_ms.max(1)));
    #[cfg(test)]
    let builder = builder.no_proxy();
    let client = builder.build().map_err(|_| refused())?;
    let transport = |e: reqwest::Error| if e.is_timeout() { ClientError::Timeout { usage: None } } else { ClientError::Http { status: 0, usage: None } };
    let mut response = client
        .post(url)
        .bearer_auth(&req.key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .body(body)
        .send()
        .await
        .map_err(transport)?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
        let mut bytes = Vec::new();
        while let Ok(Some(chunk)) = response.chunk().await {
            let take = chunk.len().min(4096_usize.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&chunk[..take]);
            if bytes.len() >= 4096 {
                break;
            }
        }
        return Err(match codec::classify_http(status, &String::from_utf8_lossy(&bytes), retry_after.as_deref()) {
            ErrorClass::RateLimited { .. } => ClientError::RateLimited { retry_after, usage: None },
            _ => ClientError::Http { status, usage: None },
        });
    }
    let mut assembler = ResponseAssembler::new();
    let mut total = 0_usize;
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        total = total.saturating_add(chunk.len());
        if total > MAX_RESPONSE_BYTES {
            return Ok(unusable(req, "response_too_large"));
        }
        match assembler.feed(&chunk) {
            Ok(_) => {}
            Err(CodecError::MissingUsage) => return Ok(unusable(req, "missing_usage")),
            Err(_) => return Ok(unusable(req, "protocol_error")),
        }
    }
    let (output, stop_reason) = match assembler.finish() {
        Ok(Outcome::Completed(output)) => {
            let stop = if output.stop == StopKind::Refusal { "refusal" } else { "end_turn" };
            (output, stop)
        }
        Ok(Outcome::Incomplete { reason: IncompleteReason::MaxOutputTokens, output, .. }) => (output, "max_tokens"),
        Ok(Outcome::Incomplete { .. }) => return Ok(unusable(req, "incomplete")),
        Ok(Outcome::Failed { class, .. }) => return Err(class_error(class)),
        Ok(Outcome::Truncated) => return Err(ClientError::Http { status: 0, usage: None }),
        Err(CodecError::MissingUsage) => return Ok(unusable(req, "missing_usage")),
        Err(_) => return Ok(unusable(req, "protocol_error")),
    };
    // 費用算得出來才算成功：要有用量、標準層級、沒有 12 小時快取寫入（我們沒要求，照預設期限算會少算）。
    let Ok(usage) = output.usage else { return Ok(unusable(req, "missing_usage")) };
    if usage.cache_write_12h_tokens > 0 || pricing::settle(&req.model, &usage, output.service_tier.as_deref()).is_err() {
        return Ok(unusable(req, "unpriced_usage"));
    }
    let text = match (&output.refusal, stop_reason) {
        (Some(refusal), "refusal") => refusal.clone(),
        _ => output.text,
    };
    Ok(ClientOutcome { text, model: Some(req.model.clone()), stop_reason: Some(stop_reason.to_string()), usage: Some(to_use_usage(&usage)) })
}
