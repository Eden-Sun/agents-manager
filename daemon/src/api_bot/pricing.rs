//! Direct global price snapshots for supported API bot models. Integer µUSD, fail closed on unknown models/usage.
use super::protocol::Response;
use super::quote::{QuoteError, QuoteProvider, RequestEnvelope, ReservationQuote, UsageQuote};
use serde_json::Value;

pub(super) struct DirectGlobal;

fn forbidden(value: &Value, allow_1h: bool, allow_search: bool, allow_fetch: bool) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            // cache_control 只接受 5 分鐘 ephemeral；這顆 bot 選了 1 小時才另放行 `ttl:"1h"`（其餘 TTL 仍拒絕）。
            (key == "cache_control" && value != &serde_json::json!({"type": "ephemeral"})
                    && !(allow_1h && value == &serde_json::json!({"type": "ephemeral", "ttl": "1h"})))
                || matches!(key.as_str(), "service_tier" | "inference_geo" | "container")
                || (key == "type" && value.as_str().is_some_and(|v| {
                    if v.starts_with("server_") {
                        !(allow_search || allow_fetch)
                    } else if v.starts_with("web_search") {
                        !allow_search
                    } else if v.starts_with("web_fetch") {
                        !allow_fetch
                    } else {
                        false
                    }
                }))
                || forbidden(value, allow_1h, allow_search, allow_fetch)
        }),
        Value::Array(items) => items.iter().any(|v| forbidden(v, allow_1h, allow_search, allow_fetch)),
        _ => false,
    }
}

/// 單價以「0.1 µUSD／token」為單位（每百萬 token 的美元數 ×10），整數運算、最後向上進位成 µUSD。
/// 各模型的標準 input／cache write／cache read／output 分列；fast 全部 2×。
struct Rates { input: i64, write_5m: i64, write_1h: i64, read: i64, output: i64, version: &'static str }
fn rates(model: &str) -> Result<Rates, QuoteError> {
    match model {
        "claude-opus-5-5" => Ok(Rates { input: 40, write_5m: 50, write_1h: 80, read: 2, output: 200, version: "claude-opus-5-5-global-2026-11-14" }),
        // https://platform.claude.com/docs/en/models/fable-5-2/overview — checked 2026-11-13.
        "claude-fable-5-2" => Ok(Rates { input: 80, write_5m: 100, write_1h: 160, read: 2, output: 400, version: "claude-fable-5-2-global-2026-11-13" }),
        _ => Err(QuoteError::InvalidRequest),
    }
}
/// 序列化後 body 的位元組數是 token 數的上界（一個 token 至少涵蓋 1 byte）；再加 tools 定義與特殊 token 的固定餘裕。
const INPUT_MARGIN_TOKENS: u64 = 8192;
const MAX_CONTEXT_TOKENS: u64 = 1_000_000;

fn speed_factor(speed: &str) -> Result<i64, QuoteError> {
    match speed {
        "standard" => Ok(1),
        "fast" => Ok(2),
        _ => Err(QuoteError::UntrustedUsage),
    }
}

fn to_micro(tenths: i64) -> i64 {
    (tenths + 9) / 10
}

fn media_adjusted_bytes(val: &Value) -> Result<u64, QuoteError> {
    let mut media_tokens: u64 = 0;
    let mut stripped = val.clone();
    strip_media_and_count(&mut stripped, &mut media_tokens)?;
    let text_bytes = serde_json::to_vec(&stripped)
        .map_err(|_| QuoteError::InvalidRequest)?
        .len() as u64;
    text_bytes.checked_add(media_tokens).ok_or(QuoteError::InvalidRequest)
}

fn strip_media_and_count(v: &mut Value, media_tokens: &mut u64) -> Result<(), QuoteError> {
    match v {
        Value::Object(map) => {
            let is_image = map.get("type").and_then(Value::as_str) == Some("image");
            let is_doc = map.get("type").and_then(Value::as_str) == Some("document");
            if is_image || is_doc {
                if let Some(source) = map.get_mut("source").and_then(Value::as_object_mut) {
                    if source.get("type").and_then(Value::as_str) == Some("base64") {
                        let data_len = source.get("data").and_then(Value::as_str).map(|s| s.len()).unwrap_or(0) as u64;
                        source.insert("data".into(), Value::String(String::new()));
                        let token_allowance = if is_image {
                            8_000
                        } else {
                            (data_len / 20).clamp(2_000, 300_000)
                        };
                        *media_tokens = media_tokens.checked_add(token_allowance).ok_or(QuoteError::InvalidRequest)?;
                    }
                }
            }
            for val in map.values_mut() {
                strip_media_and_count(val, media_tokens)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_media_and_count(item, media_tokens)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// 這個 request 輸入 token 的保守上界（不低估）；估不出來（超過 1M context）就拒絕。
/// 位元組上界過度保守（中文、多層轉義下 1 token 遠多於 1 byte，導致數十萬 token 即超過百萬 bytes）；
/// 若有前次回覆真實用量，則以前次 input token 加上新增內容的位元組數（1 token 至少 1 byte，新增位元組數
/// 是新增 token 數的嚴格上界）推估增量上界，兩者取較小值，既防止位元組誤殺，又絕不低估使成本保險絲失效。
pub(super) fn input_token_bound(request: &RequestEnvelope) -> Result<u64, QuoteError> {
    let bytes = media_adjusted_bytes(&request.body())?;
    let body_bound = bytes.checked_add(INPUT_MARGIN_TOKENS).ok_or(QuoteError::InvalidRequest)?;

    let bound = if let Some(prev) = request.previous_response() {
        if let Some(prev_tokens) = previous_usage_prefix(prev) {
            let prev_content_bytes = serde_json::to_vec(&prev.content).map(|v| v.len() as u64).unwrap_or(0);
            let last_msg_bytes = request.messages().last()
                .map(|m| media_adjusted_bytes(&m.content).unwrap_or(0))
                .unwrap_or(0);
            let delta_bytes = prev_content_bytes.saturating_add(last_msg_bytes);
            let incremental_bound = prev_tokens
                .saturating_add(delta_bytes)
                .saturating_add(INPUT_MARGIN_TOKENS);
            body_bound.min(incremental_bound)
        } else {
            body_bound
        }
    } else {
        body_bound
    };

    if bound > MAX_CONTEXT_TOKENS { return Err(QuoteError::InvalidRequest); }
    Ok(bound)
}

fn previous_usage_prefix(response: &Response) -> Option<u64> {
    if response.content.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("compaction"))
        || response.usage.as_object().and_then(|u| u.get("iterations")).and_then(Value::as_array).is_some_and(|arr| arr.iter().any(|it| it.get("type").and_then(Value::as_str) == Some("compaction")))
    {
        return None;
    }
    let usage = response.usage.as_object()?;
    let input = usage.get("input_tokens").and_then(Value::as_u64)?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cache_creation = count("cache_creation_input_tokens");
    let cache_read = count("cache_read_input_tokens");
    input.checked_add(cache_creation)?.checked_add(cache_read)
}

/// 預留：無前次回覆用量時，整段輸入按「快取寫入」算；有前次回覆用量時，前次 prefix 按快取讀取、新 tail 按寫入＋20% 餘裕。輸出按 max_tokens。
fn reserve_microusd(
    input_bound: u64,
    max_tokens: u32,
    speed: &str,
    rates: &Rates,
    previous_prefix: Option<u64>,
    ttl_1h: bool,
    web_search: bool,
) -> Result<i64, QuoteError> {
    // 沒命中時整段輸入按快取寫入價算；這顆 bot 選了 1 小時就按 1 小時寫入價（2×）。
    let write = if ttl_1h { rates.write_1h } else { rates.write_5m };
    let factor = speed_factor(speed)?;
    let input_tenths = if let Some(prefix) = previous_prefix {
        let prefix_tokens = prefix.min(input_bound);
        let tail_tokens = input_bound.saturating_sub(prefix);
        let prefix_cost = i64::try_from(prefix_tokens)
            .map_err(|_| QuoteError::InvalidRequest)?
            .checked_mul(rates.read)
            .ok_or(QuoteError::InvalidRequest)?;
        let tail_cost = i64::try_from(tail_tokens)
            .map_err(|_| QuoteError::InvalidRequest)?
            .checked_mul(write)
            .ok_or(QuoteError::InvalidRequest)?;
        let input_base = prefix_cost
            .checked_add(tail_cost)
            .ok_or(QuoteError::InvalidRequest)?;
        (input_base.checked_mul(120).ok_or(QuoteError::InvalidRequest)? + 99) / 100
    } else {
        let input = i64::try_from(input_bound).map_err(|_| QuoteError::InvalidRequest)?;
        input.checked_mul(write).ok_or(QuoteError::InvalidRequest)?
    };
    let output_tenths = i64::from(max_tokens)
        .checked_mul(rates.output)
        .ok_or(QuoteError::InvalidRequest)?;
    let total_tenths = input_tenths
        .checked_add(output_tenths)
        .ok_or(QuoteError::InvalidRequest)?;
    let total_with_factor = total_tenths
        .checked_mul(factor)
        .ok_or(QuoteError::InvalidRequest)?;
    let search_reserve = if web_search { 50_000 } else { 0 };
    to_micro(total_with_factor).checked_add(search_reserve).ok_or(QuoteError::InvalidRequest)
}

/// 串流中的「目前為止」估價（µUSD，只給畫面顯示，不進帳）：已回報的 input／cache 用量照單價算，
/// 輸出取「usage 回報值」與「已收字元／位元組粗估（約 3 字元 1 token）」較大者。
/// 不含壓縮 iteration（要等 message_delta 才知道）。模型不認得回 None。
pub(super) fn running_cost_microusd(model: &str, fast: bool, ttl_1h: bool, p: &super::sse::StreamProgress) -> Option<i64> {
    let rates = rates(model).ok()?;
    let speed = match p.speed.as_deref() {
        Some("fast") => 2,
        Some(_) => 1,
        None => if fast { 2 } else { 1 },
    };
    let est_out = (p.generated_chars as u64 + p.other_bytes as u64) / 3;
    let output = p.output_tokens.max(est_out);
    let tenths = [(p.input_tokens, rates.input), (p.cache_read_tokens, rates.read), (p.cache_creation_tokens, if ttl_1h { rates.write_1h } else { rates.write_5m }), (output, rates.output)]
        .into_iter()
        .try_fold(0_i64, |acc, (n, rate)| i64::try_from(n).ok().and_then(|n| n.checked_mul(rate)).and_then(|n| acc.checked_add(n)))?
        .checked_mul(speed)?;
    Some(to_micro(tenths))
}

impl QuoteProvider for DirectGlobal {
    fn reserve(&self, request: &RequestEnvelope) -> Result<ReservationQuote, QuoteError> {
        let body = request.body();
        if forbidden(&body, request.options().cache_ttl_1h, request.options().web_search, request.options().web_fetch) {
            return Err(QuoteError::InvalidRequest);
        }
        let rates = rates(&request.options().model)?;
        let prev_prefix = request.previous_response().and_then(previous_usage_prefix);
        Ok(ReservationQuote {
            reserved_microusd: reserve_microusd(
                input_token_bound(request)?,
                request.options().max_tokens,
                request.speed(),
                &rates,
                prev_prefix,
                request.options().cache_ttl_1h,
                request.options().web_search,
            )?,
            pricing_version: rates.version.into(),
        })
    }

    fn settle(&self, request: &RequestEnvelope, response: &Response) -> Result<UsageQuote, QuoteError> {
        self.reserve(request)?;
        let rates = rates(&request.options().model)?;
        if response.model != request.options().model { return Err(QuoteError::UntrustedUsage); }
        let usage = response.usage.as_object().ok_or(QuoteError::UntrustedUsage)?;
        if usage.keys().any(|k| !matches!(k.as_str(), "input_tokens" | "output_tokens" | "speed" | "cache_creation_input_tokens" | "cache_read_input_tokens" | "cache_creation" | "server_tool_use" | "output_tokens_details" | "service_tier" | "inference_geo" | "iterations" | "effort" | "auto_lowered_effort")) { return Err(QuoteError::UntrustedUsage); }
        let input = usage.get("input_tokens").and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage)?;
        let output = usage.get("output_tokens").and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage)?;
        let speed = usage.get("speed").and_then(Value::as_str).ok_or(QuoteError::UntrustedUsage)?;
        // 有 iterations 時以 iterations 為準（見下方「iterations 計價」）：頂層的快取欄位與 `cache_creation` 細項
        // 在壓縮那一輪會跟頂層數字對不上（實測：頂層 cache_creation_input_tokens=4332，細項 ephemeral_5m=130581＝壓縮那一輪的寫入），
        // 所以不拿它們驗證，也不拿來計價。
        let iter_items: Option<&Vec<Value>> = match usage.get("iterations") {
            Some(Value::Array(a)) if !a.is_empty() => Some(a),
            _ => None,
        };
        let count = |key: &str| match usage.get(key).and_then(Value::as_u64) {
            Some(n) => Ok(n),
            None if iter_items.is_some() => Ok(0),
            None => Err(QuoteError::UntrustedUsage),
        };
        let cache_creation = count("cache_creation_input_tokens")?;
        let cache_read = count("cache_read_input_tokens")?;
        // `cache_creation` 細項（若有）必須只含兩個 TTL 欄位且總和等於 cache_creation_input_tokens；沒有細項就全部當 5 分鐘寫入。
        let (write_5m, write_1h) = match usage.get("cache_creation") {
            _ if iter_items.is_some() => (0, 0),
            None | Some(Value::Null) => (cache_creation, 0),
            Some(Value::Object(m)) if m.keys().all(|k| matches!(k.as_str(), "ephemeral_1h_input_tokens" | "ephemeral_5m_input_tokens")) => {
                let part = |k: &str| m.get(k).and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage);
                let (h, f) = (part("ephemeral_1h_input_tokens")?, part("ephemeral_5m_input_tokens")?);
                if h.checked_add(f) != Some(cache_creation) { return Err(QuoteError::UntrustedUsage); }
                (f, h)
            }
            _ => return Err(QuoteError::UntrustedUsage),
        };
        let server_tool_use_ok = match usage.get("server_tool_use") {
            None | Some(Value::Null) => true,
            Some(Value::Object(m)) => {
                m.keys().all(|k| matches!(k.as_str(), "web_fetch_requests" | "web_search_requests"))
                    && m.get("web_search_requests").and_then(Value::as_u64).is_some_and(|n| n == 0 || request.options().web_search)
                    && m.get("web_fetch_requests").and_then(Value::as_u64).is_some_and(|n| n == 0 || request.options().web_fetch)
            }
            _ => false,
        };
        let details_ok = match usage.get("output_tokens_details") {
            None | Some(Value::Null) => true,
            Some(Value::Object(m)) => m.len() == 1 && m.get("thinking_tokens").and_then(Value::as_u64).is_some_and(|n| n <= output),
            _ => false,
        };
        let iterations_ok = match usage.get("iterations") {
            None | Some(Value::Null) => true,
            Some(Value::Array(items)) => items.iter().all(|it| it.is_object()),
            _ => false,
        };
        let neutral_metadata = |key: &str, accepted: &str| match usage.get(key) {
            None | Some(Value::Null) => true,
            Some(Value::String(v)) => v == accepted,
            _ => false,
        };
        let metadata_ok = server_tool_use_ok
            && details_ok && iterations_ok && neutral_metadata("service_tier", "standard") && neutral_metadata("inference_geo", "global")
            && !(request.speed() == "standard" && speed != "standard");
        let actual_speed = match speed { "standard" => "standard", "fast" => "fast", _ => return Err(QuoteError::UntrustedUsage) };
        let tenths = if let Some(items) = iter_items {
            // iterations 計價：每個 iteration 各自驗證、各自依類型計價後加總（官方：加總 iterations 才是實際計費；
            // 頂層 usage 不含壓縮那一輪）。`message` 是正常請求；`compaction` 是伺服器端壓縮的摘要輪，單價與一般請求相同
            // （input／快取讀寫／output），a2ce1e3f5 起如此計。快取寫入沒有細項時依這顆 bot 的 TTL 選擇計價。
            if !metadata_ok { return Err(QuoteError::UntrustedUsage); }
            let bound = input_token_bound(request)?;
            let mut acc = 0_i64;
            for it in items {
                let kind = it.get("type").and_then(Value::as_str).ok_or(QuoteError::UntrustedUsage)?;
                if !matches!(kind, "message" | "compaction") { return Err(QuoteError::UntrustedUsage); }
                let field = |k: &str, required: bool| match it.get(k) {
                    None | Some(Value::Null) if !required => Ok(0),
                    v => v.and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage),
                };
                let (i_in, i_out) = (field("input_tokens", true)?, field("output_tokens", true)?);
                let (i_read, i_write) = (field("cache_read_input_tokens", false)?, field("cache_creation_input_tokens", false)?);
                let total = i_in.checked_add(i_read).and_then(|n| n.checked_add(i_write)).ok_or(QuoteError::UntrustedUsage)?;
                if total > bound || (kind == "message" && i_out > u64::from(request.options().max_tokens)) { return Err(QuoteError::UntrustedUsage); }
                // 細項（若有）必須只含兩個 TTL 欄位且總和等於該 iteration 的 cache_creation_input_tokens。
                let (w5, w1) = match it.get("cache_creation") {
                    None | Some(Value::Null) => if request.options().cache_ttl_1h { (0, i_write) } else { (i_write, 0) },
                    Some(Value::Object(m)) if m.keys().all(|k| matches!(k.as_str(), "ephemeral_1h_input_tokens" | "ephemeral_5m_input_tokens")) => {
                        let part = |k: &str| m.get(k).and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage);
                        let (h, f) = (part("ephemeral_1h_input_tokens")?, part("ephemeral_5m_input_tokens")?);
                        if h.checked_add(f) != Some(i_write) { return Err(QuoteError::UntrustedUsage); }
                        (f, h)
                    }
                    _ => return Err(QuoteError::UntrustedUsage),
                };
                let part_cost = [(i_in, rates.input), (w5, rates.write_5m), (w1, rates.write_1h), (i_read, rates.read), (i_out, rates.output)]
                    .into_iter()
                    .try_fold(0_i64, |a, (n, rate)| i64::try_from(n).ok().and_then(|n| n.checked_mul(rate)).and_then(|n| a.checked_add(n)))
                    .ok_or(QuoteError::UntrustedUsage)?;
                acc = acc.checked_add(part_cost).ok_or(QuoteError::UntrustedUsage)?;
            }
            acc.checked_mul(speed_factor(actual_speed)?).ok_or(QuoteError::UntrustedUsage)?
        } else {
            let total_input = input.checked_add(cache_creation).and_then(|n| n.checked_add(cache_read)).ok_or(QuoteError::UntrustedUsage)?;
            if total_input > input_token_bound(request)? || output > u64::from(request.options().max_tokens) || !metadata_ok {
                return Err(QuoteError::UntrustedUsage);
            }
            [(input, rates.input), (write_5m, rates.write_5m), (write_1h, rates.write_1h), (cache_read, rates.read), (output, rates.output)]
                .into_iter()
                .try_fold(0_i64, |acc, (n, rate)| i64::try_from(n).ok().and_then(|n| n.checked_mul(rate)).and_then(|n| acc.checked_add(n)))
                .and_then(|n| n.checked_mul(speed_factor(actual_speed).ok()?))
                .ok_or(QuoteError::UntrustedUsage)?
        };
        let search_fee_microusd = usage.get("server_tool_use")
            .and_then(|v| v.get("web_search_requests"))
            .and_then(Value::as_u64)
            .map(|count| count as i64 * 10_000)
            .unwrap_or(0);
        let estimated_microusd = to_micro(tenths)
            .checked_add(search_fee_microusd)
            .ok_or(QuoteError::UntrustedUsage)?;
        // 實際費用超過預留（例如回合中途快取過期、整段按寫入價）仍是真實花費：照實記帳，不當成不可信用量。
        Ok(UsageQuote { estimated_microusd, actual_speed: Some(actual_speed) })
    }
}

/// 分析 API 回應中用量（usage）或結構不合法的原因；合法或非用量問題時回 `None`。
pub(crate) fn classify_usage_problem(request: &RequestEnvelope, response: &Response) -> Option<String> {
    if response.model != request.options().model {
        return Some(format!("回應模型與請求不符：{} != {}", response.model, request.options().model));
    }
    if rates(&request.options().model).is_err() {
        return Some(format!("不支援的模型：{}", request.options().model));
    }
    let Some(usage) = response.usage.as_object() else {
        return Some("用量欄位不是 JSON 物件".into());
    };
    for k in usage.keys() {
        if !matches!(
            k.as_str(),
            "input_tokens"
                | "output_tokens"
                | "speed"
                | "cache_creation_input_tokens"
                | "cache_read_input_tokens"
                | "cache_creation"
                | "server_tool_use"
                | "output_tokens_details"
                | "service_tier"
                | "inference_geo"
                | "iterations"
        ) {
            return Some(format!("用量包含未支援的欄位：{k}"));
        }
    }
    let input = match usage.get("input_tokens").and_then(Value::as_u64) {
        Some(n) => n,
        None => return Some("用量缺少或無效的 input_tokens".into()),
    };
    let output = match usage.get("output_tokens").and_then(Value::as_u64) {
        Some(n) => n,
        None => return Some("用量缺少或無效的 output_tokens".into()),
    };
    let speed = match usage.get("speed").and_then(Value::as_str) {
        Some(s) => s,
        None => return Some("用量缺少或無效的 speed".into()),
    };
    if !matches!(speed, "standard" | "fast") {
        return Some(format!("未知的 speed 設定：{speed}"));
    }
    if request.speed() == "standard" && speed != "standard" {
        return Some("請求為 standard 但回應 speed 非 standard".into());
    }

    let iter_items: Option<&Vec<Value>> = match usage.get("iterations") {
        Some(Value::Array(a)) if !a.is_empty() => Some(a),
        _ => None,
    };
    let count = |key: &str| match usage.get(key).and_then(Value::as_u64) {
        Some(n) => Ok(n),
        None if iter_items.is_some() => Ok(0),
        None => Err(()),
    };
    let cache_creation = match count("cache_creation_input_tokens") {
        Ok(n) => n,
        Err(_) => return Some("用量缺少 cache_creation_input_tokens".into()),
    };
    let cache_read = match count("cache_read_input_tokens") {
        Ok(n) => n,
        Err(_) => return Some("用量缺少 cache_read_input_tokens".into()),
    };

    if iter_items.is_none() {
        if let Some(cc) = usage.get("cache_creation") {
            if !cc.is_null() {
                let Some(m) = cc.as_object() else {
                    return Some("cache_creation 細項不是 JSON 物件".into());
                };
                if !m.keys().all(|k| matches!(k.as_str(), "ephemeral_1h_input_tokens" | "ephemeral_5m_input_tokens")) {
                    return Some("cache_creation 細項包含未支援的鍵".into());
                }
                let h = match m.get("ephemeral_1h_input_tokens").and_then(Value::as_u64) {
                    Some(n) => n,
                    None => return Some("cache_creation 細項缺少 ephemeral_1h_input_tokens".into()),
                };
                let f = match m.get("ephemeral_5m_input_tokens").and_then(Value::as_u64) {
                    Some(n) => n,
                    None => return Some("cache_creation 細項缺少 ephemeral_5m_input_tokens".into()),
                };
                if h.checked_add(f) != Some(cache_creation) {
                    return Some(format!("cache_creation 細項總和 ({}+{}) 與 cache_creation_input_tokens ({}) 不符", h, f, cache_creation));
                }
            }
        }
    }

    if let Some(stu) = usage.get("server_tool_use") {
        if !stu.is_null() {
            let Some(m) = stu.as_object() else {
                return Some("server_tool_use 不是物件".into());
            };
            for k in m.keys() {
                if !matches!(k.as_str(), "web_fetch_requests" | "web_search_requests") {
                    return Some(format!("server_tool_use 包含額外欄位：{k}"));
                }
            }
            if let Some(n) = m.get("web_search_requests") {
                match n.as_u64() {
                    Some(count) if count > 0 && !request.options().web_search => {
                        return Some(format!("未啟用 web_search 但回報 web_search_requests: {count}"));
                    }
                    Some(_) => {}
                    None => return Some("server_tool_use.web_search_requests 不是非負整數".into()),
                }
            } else {
                return Some("server_tool_use 缺少 web_search_requests".into());
            }
            if let Some(n) = m.get("web_fetch_requests") {
                match n.as_u64() {
                    Some(count) if count > 0 && !request.options().web_fetch => {
                        return Some(format!("未啟用 web_fetch 但回報 web_fetch_requests: {count}"));
                    }
                    Some(_) => {}
                    None => return Some("server_tool_use.web_fetch_requests 不是非負整數".into()),
                }
            } else {
                return Some("server_tool_use 缺少 web_fetch_requests".into());
            }
        }
    }

    if let Some(otd) = usage.get("output_tokens_details") {
        if !otd.is_null() {
            let Some(m) = otd.as_object() else {
                return Some("output_tokens_details 不是物件".into());
            };
            if m.len() != 1 {
                return Some("output_tokens_details 包含未預期的鍵".into());
            }
            let Some(thinking) = m.get("thinking_tokens").and_then(Value::as_u64) else {
                return Some("output_tokens_details 缺少 thinking_tokens".into());
            };
            if thinking > output {
                return Some(format!("thinking_tokens ({thinking}) 超出 output_tokens ({output})"));
            }
        }
    }

    if let Some(st) = usage.get("service_tier") {
        if !st.is_null() && st.as_str() != Some("standard") {
            return Some(format!("service_tier 不是 standard: {:?}", st));
        }
    }
    if let Some(ig) = usage.get("inference_geo") {
        if !ig.is_null() && ig.as_str() != Some("global") {
            return Some(format!("inference_geo 不是 global: {:?}", ig));
        }
    }

    let Ok(bound) = input_token_bound(request) else {
        return Some("無法計算請求 token 上限".into());
    };

    if let Some(items) = iter_items {
        for (idx, it) in items.iter().enumerate() {
            let Some(it_obj) = it.as_object() else {
                return Some(format!("iteration [{idx}] 不是物件"));
            };
            let Some(kind) = it_obj.get("type").and_then(Value::as_str) else {
                return Some(format!("iteration [{idx}] 缺少 type"));
            };
            if !matches!(kind, "message" | "compaction") {
                return Some(format!("iteration [{idx}] 類型不支援：{kind}"));
            }
            let Some(i_in) = it_obj.get("input_tokens").and_then(Value::as_u64) else {
                return Some(format!("iteration [{idx}] 缺少 input_tokens"));
            };
            let Some(i_out) = it_obj.get("output_tokens").and_then(Value::as_u64) else {
                return Some(format!("iteration [{idx}] 缺少 output_tokens"));
            };
            let i_read = it_obj.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
            let i_write = it_obj.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);
            let Some(total) = i_in.checked_add(i_read).and_then(|n| n.checked_add(i_write)) else {
                return Some(format!("iteration [{idx}] token 總和溢位"));
            };
            if total > bound {
                return Some(format!("iteration [{idx}] 輸入 token 總和 ({total}) 超出上限 ({bound})"));
            }
            if kind == "message" && i_out > u64::from(request.options().max_tokens) {
                return Some(format!("iteration [{idx}] 輸出 token ({i_out}) 超出 max_tokens ({})", request.options().max_tokens));
            }
            if let Some(cc) = it_obj.get("cache_creation") {
                if !cc.is_null() {
                    let Some(m) = cc.as_object() else {
                        return Some(format!("iteration [{idx}] cache_creation 不是物件"));
                    };
                    if !m.keys().all(|k| matches!(k.as_str(), "ephemeral_1h_input_tokens" | "ephemeral_5m_input_tokens")) {
                        return Some(format!("iteration [{idx}] cache_creation 包含未支援的鍵"));
                    }
                    let h = m.get("ephemeral_1h_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    let f = m.get("ephemeral_5m_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    if h.checked_add(f) != Some(i_write) {
                        return Some(format!("iteration [{idx}] cache_creation 細項總和 ({}+{}) 與 cache_creation_input_tokens ({}) 不符", h, f, i_write));
                    }
                }
            }
        }
    } else {
        let Some(total_input) = input.checked_add(cache_creation).and_then(|n| n.checked_add(cache_read)) else {
            return Some("輸入 token 總和溢位".into());
        };
        if total_input > bound {
            return Some(format!("輸入 token 總和 ({total_input}) 超出上限 ({bound})"));
        }
        if output > u64::from(request.options().max_tokens) {
            return Some(format!("輸出 token ({output}) 超出 max_tokens ({})", request.options().max_tokens));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_bot::protocol::{Message, Options};
    use serde_json::json;

    fn opt(model: &str, fast: bool, max_tokens: u32, compact_at: u32, cache_ttl_1h: bool) -> Options {
        Options {
            model: model.into(),
            effort: "high".into(),
            fast,
            max_tokens,
            compact_at_input_tokens: compact_at,
            clear_tool_uses_at_input_tokens: 0,
            clear_tool_uses_keep: 3,
            clear_tool_uses_clear_at_least: 5000,
            cache_ttl_1h,
            web_search: false,
            web_fetch: false,
        }
    }

    fn request(fast: bool, system: Value) -> RequestEnvelope {
        RequestEnvelope::new(opt(crate::config::API_BOT_MODEL, fast, 1024, 0, false), system,
            vec![Message { role: "user".into(), content: json!("fixture") }]).unwrap()
    }
    fn response(usage: Value) -> Response {
        Response { id: "fake".into(), model: "claude-opus-5-5".into(), content: vec![], stop_reason: "end_turn".into(), usage }
    }
    #[test]
    fn approved_quotes_are_integer_upper_bounds_and_reject_untrusted_usage() {
        let standard = request(false, json!("fixture"));
        let fast = request(true, json!("fixture"));
        // 預留依實際 request 估：輸入 = body 位元組數 + 固定餘裕，全部按快取寫入價；輸出 = max_tokens。
        let bound = input_token_bound(&standard).unwrap() as i64;
        assert!(bound < 10_000, "小 request 的輸入上界只有幾千 token");
        let expected = (bound * 50 + 1024 * 200 + 9) / 10;
        assert_eq!(DirectGlobal.reserve(&standard).unwrap().reserved_microusd, expected);
        assert!(expected < 100_000, "小 request 的單次預留只有幾美分");
        let fast_bound = input_token_bound(&fast).unwrap() as i64;
        assert_eq!(DirectGlobal.reserve(&fast).unwrap().reserved_microusd, ((fast_bound * 50 + 1024 * 200) * 2 + 9) / 10);
        let usage = response(json!({"input_tokens": 8, "output_tokens": 12, "speed": "standard", "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "cache_creation": {"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0}, "server_tool_use": {"web_fetch_requests":0,"web_search_requests":0}, "output_tokens_details":{"thinking_tokens":4}, "service_tier":"standard", "inference_geo":"global"}));
        assert_eq!(DirectGlobal.settle(&standard, &usage).unwrap().estimated_microusd, 272);
        assert!(DirectGlobal.settle(&fast, &usage).is_ok());
        for usage in [json!({"input_tokens": 8, "output_tokens": 12}), json!({"input_tokens": 8, "output_tokens": 12, "speed": "fast"}), json!({"input_tokens": 1_000_001, "output_tokens": 1, "speed": "standard"}), json!({"input_tokens": 1, "output_tokens": 1, "speed": "standard", "cache_read_input_tokens": 1}), json!({"input_tokens": 1, "output_tokens": 1, "speed": "standard", "cache_creation_input_tokens": 0, "cache_read_input_tokens": 2_000_000})] {
            assert!(DirectGlobal.settle(&standard, &response(usage)).is_err());
        }
        for field in ["cache_control", "service_tier", "inference_geo"] {
            assert!(DirectGlobal.reserve(&request(false, json!({"nested": [{field: "forbidden"}]}))).is_err());
        }
        let valid = json!({"input_tokens":8,"output_tokens":12,"speed":"standard", "cache_creation_input_tokens":0,"cache_read_input_tokens":0});
        for (field, value) in [
            ("cache_creation_input_tokens", json!(null)),
            ("cache_read_input_tokens", json!(1_000_001)),
            ("cache_creation", json!({"ephemeral_1h_input_tokens":1,"ephemeral_5m_input_tokens":0})),
            ("cache_creation", json!({"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0,"other":0})),
            ("server_tool_use", json!({"web_fetch_requests":0,"web_search_requests":1})),
            ("output_tokens_details", json!({"thinking_tokens":13})),
            ("service_tier", json!("priority")),
            ("inference_geo", json!("us")),
            ("unknown_modifier", json!(0)),
        ] {
            let mut changed = valid.clone();
            changed[field] = value;
            assert!(DirectGlobal.settle(&standard, &response(changed)).is_err(), "{field}");
        }
        let details = response(json!({"input_tokens":8,"output_tokens":12,"speed":"standard", "cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens_details":{"thinking_tokens":12}}));
        assert_eq!(DirectGlobal.settle(&standard, &details).unwrap().estimated_microusd, 272, "inclusive output is billed only once");
    }

    #[test]
    fn cache_reads_and_writes_are_priced_and_reserve_covers_the_worst_case() {
        let standard = request(false, json!("fixture"));
        let fast = request(true, json!("fixture"));
        let base = json!({"input_tokens":100,"output_tokens":10,"speed":"standard","service_tier":"standard","inference_geo":"global"});
        let with = |create: u64, read: u64, detail: Value| {
            let mut u = base.clone();
            u["cache_creation_input_tokens"] = json!(create);
            u["cache_read_input_tokens"] = json!(read);
            if !detail.is_null() { u["cache_creation"] = detail; }
            response(u)
        };
        // 100×$4 + 1000×$5（5 分鐘寫入）+ 2000×$0.2（讀取，Opus 5.5 是 0.05×）+ 10×$20 = 400+5000+400+200 = 6000 µUSD
        assert_eq!(DirectGlobal.settle(&standard, &with(1000, 2000, Value::Null)).unwrap().estimated_microusd, 6000);
        let detailed = json!({"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":1000});
        assert_eq!(DirectGlobal.settle(&standard, &with(1000, 2000, detailed)).unwrap().estimated_microusd, 6000);
        // 1 小時寫入 2×：500×$8 = 4000
        let hour = json!({"ephemeral_1h_input_tokens":500,"ephemeral_5m_input_tokens":500});
        assert_eq!(DirectGlobal.settle(&standard, &with(1000, 0, hour)).unwrap().estimated_microusd, 400 + 2500 + 4000 + 200);
        // 細項總和與總數不符 → 不信任
        assert!(DirectGlobal.settle(&standard, &with(1000, 0, json!({"ephemeral_1h_input_tokens":1,"ephemeral_5m_input_tokens":1}))).is_err());
        // fast 實際單價 2×
        let mut f = with(0, 2000, Value::Null);
        f.usage["speed"] = json!("fast");
        assert_eq!(DirectGlobal.settle(&fast, &f).unwrap().estimated_microusd, (100 * 40 + 2000 * 2 + 10 * 200) * 2 / 10);
    }

    #[test]
    fn only_the_five_minute_ephemeral_cache_control_is_allowed() {
        let ok = request(false, json!({"nested":[{"cache_control":{"type":"ephemeral"}}]}));
        assert!(DirectGlobal.reserve(&ok).is_ok());
        let long = request(false, json!({"nested":[{"cache_control":{"type":"ephemeral","ttl":"1h"}}]}));
        assert!(DirectGlobal.reserve(&long).is_err());
    }

    #[test]
    fn the_wire_body_marks_at_most_three_cache_breakpoints_and_never_changes_the_checkpoint_messages() {
        let options = opt(crate::config::API_BOT_MODEL, false, 1024, 0, false);
        let messages = vec![
            Message { role: "user".into(), content: json!("one") },
            Message { role: "assistant".into(), content: json!([{"type":"thinking","thinking":"t","signature":"s"}]) },
            Message { role: "user".into(), content: json!([{"type":"tool_result","tool_use_id":"a","content":"ok"}]) },
        ];
        let body = crate::api_bot::protocol::request(&options, &json!("system"), &messages);
        let text = body.to_string();
        assert_eq!(text.matches("cache_control").count(), 2, "system + 最後一則；前一則的 thinking 區塊不能帶");
        assert!(body["messages"][0]["content"].is_string());
        assert!(messages[2].content[0].get("cache_control").is_none());
    }
    #[test]
    fn fable_quotes_cover_standard_fast_and_its_distinct_cache_read_rate() {
        for fast in [false, true] {
            let options = opt(crate::config::API_BOT_FABLE_MODEL, fast, 1024, 0, false);
            let req = RequestEnvelope::new(options, json!("fixture"), vec![Message { role:"user".into(),content:json!("fixture") }]).unwrap();
            let q = DirectGlobal.reserve(&req).unwrap();
            assert_eq!(q.pricing_version, "claude-fable-5-2-global-2026-11-13");
            let factor = if fast { 2 } else { 1 };
            let expected = ((input_token_bound(&req).unwrap() as i64 * 100 + 1024 * 400) * factor + 9) / 10;
            assert_eq!(q.reserved_microusd, expected);
            let mut resp = response(json!({"input_tokens":100,"output_tokens":10,"speed":if fast {"fast"} else {"standard"},"cache_creation_input_tokens":1000,"cache_read_input_tokens":2000,"cache_creation":{"ephemeral_1h_input_tokens":500,"ephemeral_5m_input_tokens":500}}));
            resp.model = crate::config::API_BOT_FABLE_MODEL.into();
            // 100*$8 + 500*$10 + 500*$16 + 2000*$0.20 + 10*$40 = 14600 µUSD.
            assert_eq!(DirectGlobal.settle(&req, &resp).unwrap().estimated_microusd, 14600 * factor);
            resp.model = crate::config::API_BOT_MODEL.into();
            assert!(DirectGlobal.settle(&req, &resp).is_err(), "never price a mismatched model");
        }
    }

    #[test]
    fn first_turn_without_previous_usage_uses_legacy_formula() {
        let req = request(false, json!("fixture"));
        let bound = input_token_bound(&req).unwrap() as i64;
        let expected_legacy = (bound * 50 + 1024 * 200 + 9) / 10;
        let quote = DirectGlobal.reserve(&req).unwrap();
        assert_eq!(quote.reserved_microusd, expected_legacy);

        // Previous response without valid usage (no input_tokens) also falls back to legacy formula
        let mut invalid_prev_req = request(false, json!("fixture"));
        invalid_prev_req.set_previous_response(Some(response(json!({"invalid": true}))));
        let quote2 = DirectGlobal.reserve(&invalid_prev_req).unwrap();
        assert_eq!(quote2.reserved_microusd, expected_legacy);
    }

    #[test]
    fn turn_with_previous_usage_reserves_with_prefix_and_settles_cleanly() {
        let mut req = request(false, json!("fixture"));
        let prev = response(json!({
            "input_tokens": 2000,
            "cache_read_input_tokens": 1000,
            "cache_creation_input_tokens": 500,
            "output_tokens": 200,
            "speed": "standard"
        }));
        req.set_previous_response(Some(prev));

        let bound = input_token_bound(&req).unwrap() as i64;
        let prefix = 3500_i64; // 2000 + 1000 + 500
        assert!(bound > prefix);
        let prefix_tokens = prefix;
        let tail_tokens = bound - prefix;
        let prefix_cost = prefix_tokens * 2; // claude-opus-5-5 read rate = 2（$0.20/M）
        let tail_cost = tail_tokens * 50;   // claude-opus-5-5 write_5m rate = 50
        let input_base = prefix_cost + tail_cost;
        let input_with_margin = (input_base * 120 + 99) / 100;
        let expected_reserve = (input_with_margin + 1024 * 200 + 9) / 10;

        let quote = DirectGlobal.reserve(&req).unwrap();
        assert_eq!(quote.reserved_microusd, expected_reserve);

        // Actual usage for this turn is well below reserve:
        let actual = response(json!({
            "input_tokens": 100,
            "cache_read_input_tokens": 3500,
            "cache_creation_input_tokens": 100,
            "output_tokens": 150,
            "speed": "standard",
            "service_tier": "standard",
            "inference_geo": "global"
        }));
        let settled = DirectGlobal.settle(&req, &actual).unwrap();
        assert!(settled.estimated_microusd < quote.reserved_microusd);
    }

    #[test]
    fn long_session_reserve_is_much_lower_than_legacy_formula() {
        // In a long session, large conversation prefix was cached in previous turn
        let long_messages = vec![
            Message { role: "user".into(), content: json!("x".repeat(60_000)) },
            Message { role: "assistant".into(), content: json!("reply") },
            Message { role: "user".into(), content: json!("next question") },
        ];
        let options = opt(crate::config::API_BOT_MODEL, false, 1024, 0, false);
        let mut req = RequestEnvelope::new(options, json!("system"), long_messages).unwrap();

        let bound = input_token_bound(&req).unwrap() as i64;
        assert!(bound > 60_000);

        // Previous response usage recorded 60,000 prefix tokens
        let prev = response(json!({
            "input_tokens": 1000,
            "cache_read_input_tokens": 59_000,
            "cache_creation_input_tokens": 0,
            "output_tokens": 100,
            "speed": "standard"
        }));
        req.set_previous_response(Some(prev));

        let legacy_reserve = (bound * 50 + 1024 * 200 + 9) / 10;
        let new_reserve = DirectGlobal.reserve(&req).unwrap().reserved_microusd;

        // Legacy formula priced all 60k+ tokens at write_5m (50); new formula prices 60k at read (4).
        // 60k * 4 = 240k vs 60k * 50 = 3000k. Even with 20% margin, new reserve is less than half the old.
        assert!(new_reserve < legacy_reserve / 2, "new_reserve ({new_reserve}) should be much lower than legacy ({legacy_reserve})");

        // And normal actual usage settles well under the new reserve
        let actual = response(json!({
            "input_tokens": 500,
            "cache_read_input_tokens": 60_000,
            "cache_creation_input_tokens": 500,
            "output_tokens": 200,
            "speed": "standard",
            "service_tier": "standard",
            "inference_geo": "global"
        }));
        let settled = DirectGlobal.settle(&req, &actual).unwrap();
        assert!(settled.estimated_microusd < new_reserve);
    }

    #[test]
    fn compaction_usage_shape_is_accepted_by_settle_and_falls_back_to_first_turn_reserve() {
        let mut req = request(false, json!("fixture"));
        let bound = input_token_bound(&req).unwrap() as i64;
        let expected_legacy = (bound * 50 + 1024 * 200 + 9) / 10;

        // 1. Response containing a compaction block and iterations in usage
        let mut compact_resp = response(json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "speed": "standard",
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "service_tier": "standard",
            "inference_geo": "global",
            "iterations": [
                {
                    "type": "compaction",
                    "input_tokens": 4000,
                    "cache_read_input_tokens": 1000,
                    "cache_creation_input_tokens": 500,
                    "output_tokens": 300
                },
                {
                    "type": "message",
                    "input_tokens": 100,
                    "output_tokens": 50
                }
            ]
        }));
        compact_resp.content = vec![
            json!({"type": "compaction", "content": "Summary of previous turns"}),
            json!({"type": "text", "text": "Continuing after compaction"}),
        ];

        // Settle accepts the compaction usage shape
        let settled = DirectGlobal.settle(&req, &compact_resp).unwrap();
        // 頂層 100×40+50×200，壓縮 iteration 4000×40(input)+1000×4(read)+500×50(write)+300×200(output)，單位 0.1 µUSD。
        assert_eq!(settled.estimated_microusd, (14_000 + 160_000 + 2_000 + 25_000 + 60_000 + 9) / 10);

        // 2. Next turn's reserve: previous response had compaction, so prefix no longer applies
        req.set_previous_response(Some(compact_resp));
        let quote_after_compact = DirectGlobal.reserve(&req).unwrap();
        assert_eq!(
            quote_after_compact.reserved_microusd,
            expected_legacy,
            "must fall back to first-turn legacy formula when previous response was compacted"
        );
    }

    /// 2026-11-15 pt-hub 實際回應（context 到 150k 觸發伺服器端壓縮）：頂層 cache_creation 細項（130581）跟
    /// cache_creation_input_tokens（4332）對不上，以前被判不可信用量；現在以 iterations 為準。
    fn real_compaction_usage() -> Value {
        json!({
            "cache_creation": {"ephemeral_1h_input_tokens": 0, "ephemeral_5m_input_tokens": 130581},
            "cache_creation_input_tokens": 4332, "cache_read_input_tokens": 20653, "input_tokens": 2, "output_tokens": 533,
            "inference_geo": "global", "service_tier": "standard", "speed": "standard",
            "iterations": [
                {"type": "compaction", "cache_creation_input_tokens": 130581, "input_tokens": 38, "output_tokens": 8192},
                {"type": "message", "cache_creation_input_tokens": 4332, "cache_read_input_tokens": 20653, "input_tokens": 2, "output_tokens": 533}
            ]
        })
    }

    #[test]
    fn real_compaction_usage_with_mismatched_top_level_breakdown_is_priced_from_iterations() {
        let fable = crate::config::API_BOT_FABLE_MODEL;
        let req = RequestEnvelope::new(
            opt(fable, false, 32000, 150_000, false),
            json!("s"),
            // 大 request：位元組上界要涵蓋 130k token 的壓縮輸入。
            vec![Message { role: "user".into(), content: json!("x".repeat(400_000)) }],
        ).unwrap();
        let mut resp = response(real_compaction_usage());
        resp.model = fable.into();
        let q = DirectGlobal.settle(&req, &resp).expect("accepted");
        // Fable：input 80、5m 寫入 100、讀取 2、output 400（0.1µUSD/token）。
        // 壓縮：38×80 + 130581×100 + 8192×400；message：2×80 + 4332×100 + 20653×2 + 533×400。
        let tenths = 38 * 80 + 130_581 * 100 + 8192 * 400 + 2 * 80 + 4332 * 100 + 20_653 * 2 + 533 * 400;
        assert_eq!(q.estimated_microusd, (tenths + 9) / 10);
        assert!((1_600_000..1_800_000).contains(&q.estimated_microusd), "約 $1.7，不是預留上限 $6.53：{}", q.estimated_microusd);
    }

    #[test]
    fn iteration_entries_are_each_validated() {
        let fable = crate::config::API_BOT_FABLE_MODEL;
        let req = RequestEnvelope::new(
            opt(fable, false, 32000, 150_000, false),
            json!("s"), vec![Message { role: "user".into(), content: json!("x".repeat(400_000)) }],
        ).unwrap();
        for bad in [json!({"type": "mystery", "input_tokens": 1, "output_tokens": 1}), json!({"type": "message", "input_tokens": 1}), json!({"type": "message", "input_tokens": 1, "output_tokens": 99_999}), json!({"type": "compaction", "input_tokens": 1, "output_tokens": 1, "cache_creation_input_tokens": 5, "cache_creation": {"ephemeral_5m_input_tokens": 1}})] {
            let mut usage = real_compaction_usage();
            usage["iterations"] = json!([bad]);
            let mut resp = response(usage);
            resp.model = fable.into();
            assert!(DirectGlobal.settle(&req, &resp).is_err());
        }
    }

    #[test]
    fn running_cost_uses_reported_usage_and_char_estimate() {
        use crate::api_bot::sse::StreamProgress;
        let model = crate::config::API_BOT_MODEL;
        let p = StreamProgress { input_tokens: 1000, cache_read_tokens: 2000, cache_creation_tokens: 500, output_tokens: 10, generated_chars: 3000, ..Default::default() };
        // output 取 max(10, 3000/3=1000)=1000：1000×40 + 2000×2 + 500×50 + 1000×200 = 269000 tenths。
        assert_eq!(running_cost_microusd(model, false, false, &p), Some(26_900));
        assert_eq!(running_cost_microusd(model, true, false, &p), Some(53_800));
        let fast = StreamProgress { speed: Some("fast".into()), ..p.clone() };
        assert_eq!(running_cost_microusd(model, false, false, &fast), Some(53_800));
        // 回報的 output_tokens 較大時用回報值。
        let big = StreamProgress { output_tokens: 5000, ..p };
        assert_eq!(running_cost_microusd(model, false, false, &big), Some((1000 * 40 + 2000 * 2 + 500 * 50 + 5000 * 200 + 9) / 10));
        assert_eq!(running_cost_microusd("unknown-model", false, false, &StreamProgress::default()), None);
    }

    #[test]
    fn oversized_transcript_over_1m_bytes_with_compaction_is_pruned_and_quoted() {
        let fable = crate::config::API_BOT_FABLE_MODEL;
        // 模擬故障 bot 01M7AKSKXMB1A6RRQZFJFPMVKK：歷史訊息超過 1.5M bytes，
        // 中間觸發過 compaction，其後有新的對話。
        let mut messages = Vec::new();
        // 前面累積超過 1.5MB 的舊歷史
        for i in 0..15 {
            messages.push(Message {
                role: "user".into(),
                content: json!(format!("old user prompt {i}: {}", "A".repeat(100_000))),
            });
            messages.push(Message {
                role: "assistant".into(),
                content: json!([
                    {"type": "thinking", "thinking": "old thinking", "signature": format!("sig_{i}")},
                    {"type": "text", "text": format!("old reply {i}: {}", "B".repeat(5_000))}
                ]),
            });
        }
        let total_old_bytes: usize = messages.iter().map(|m| serde_json::to_vec(&m.content).unwrap().len()).sum();
        assert!(total_old_bytes > 1_500_000, "舊歷史總 bytes 必須大於 1.5MB：{total_old_bytes}");

        // 伺服器端 compaction 區塊
        messages.push(Message {
            role: "assistant".into(),
            content: json!([
                {"type": "thinking", "thinking": "pre-compact reasoning", "signature": "sig_pre"},
                {"type": "compaction", "content": "Summary of 15 rounds of conversation"},
                {"type": "thinking", "thinking": "post-compact reasoning", "signature": "sig_post"},
                {"type": "text", "text": "Ready for next instruction."}
            ]),
        });

        // 壓縮後的新對話
        messages.push(Message {
            role: "user".into(),
            content: json!("What should we do next?"),
        });

        let opts = opt(fable, false, 32000, 150_000, false);

        // 以前：RequestEnvelope 序列化整個未修剪 messages（> 1.5MB），input_token_bound 直接失敗拒絕估價。
        // 現在：修剪最新 compaction 區塊之前的內容，body 遠小於 1MB，估價成功。
        let req = RequestEnvelope::new(opts, json!("system"), messages).expect("envelope accepted");
        let bound = input_token_bound(&req).expect("input token bound calculated");
        assert!(bound < 50_000, "修剪後的 input token bound 應遠小於 1M tokens：{bound}");

        let quote = DirectGlobal.reserve(&req).expect("reservation quote succeeds");
        assert!(quote.reserved_microusd > 0);
    }

    #[test]
    fn opus_cache_read_is_twenty_cents_per_million_and_the_version_moved() {
        let req = request(false, json!("fixture"));
        // 1000 讀取 × $0.20/M = 200 µUSD（舊價 $0.40 會是 400）；另 100 input×$4 + 10 output×$20。
        let usage = json!({"input_tokens":100,"output_tokens":10,"speed":"standard","cache_creation_input_tokens":0,"cache_read_input_tokens":1000,"service_tier":"standard","inference_geo":"global"});
        assert_eq!(DirectGlobal.settle(&req, &response(usage)).unwrap().estimated_microusd, 400 + 200 + 200);
        assert_eq!(DirectGlobal.reserve(&req).unwrap().pricing_version, "claude-opus-5-5-global-2026-11-14");
    }

    #[test]
    fn one_hour_bots_may_carry_the_1h_cache_control_and_reserve_at_the_1h_write_rate() {
        let mk = |ttl_1h: bool| {
            let options = opt(crate::config::API_BOT_MODEL, false, 1024, 0, ttl_1h);
            RequestEnvelope::new(options, json!("fixture"), vec![Message { role: "user".into(), content: json!("fixture") }]).unwrap()
        };
        let (five, hour) = (mk(false), mk(true));
        // 請求體：1 小時才帶 ttl。
        assert!(five.body().to_string().contains(r#""cache_control":{"type":"ephemeral"}"#) && !five.body().to_string().contains(r#""ttl""#));
        assert!(hour.body().to_string().contains(r#""cache_control":{"ttl":"1h","type":"ephemeral"}"#));
        // 兩種都放行；1 小時預留用 1 小時寫入價（$8 對 $5），輸出部分相同。
        let (q5, q1) = (DirectGlobal.reserve(&five).unwrap(), DirectGlobal.reserve(&hour).unwrap());
        // 1 小時的 body 多了 `"ttl":"1h"` 幾個位元組，輸入上界各算各的。
        let (b5, b1) = (input_token_bound(&five).unwrap() as i64, input_token_bound(&hour).unwrap() as i64);
        assert!(b1 > b5);
        assert_eq!(q5.reserved_microusd, (b5 * 50 + 1024 * 200 + 9) / 10);
        assert_eq!(q1.reserved_microusd, (b1 * 80 + 1024 * 200 + 9) / 10);
        // 5 分鐘 bot 的 body 不准出現 1h（沿用既有規則）；1 小時 bot 也不准別的 TTL。
        let sneaky_1h = RequestEnvelope::new(
            opt(crate::config::API_BOT_MODEL, false, 1024, 0, false),
            json!({"nested":[{"cache_control":{"type":"ephemeral","ttl":"1h"}}]}), vec![Message { role: "user".into(), content: json!("x") }]).unwrap();
        assert!(DirectGlobal.reserve(&sneaky_1h).is_err());
        let other_ttl = RequestEnvelope::new(
            opt(crate::config::API_BOT_MODEL, false, 1024, 0, true),
            json!({"nested":[{"cache_control":{"type":"ephemeral","ttl":"24h"}}]}), vec![Message { role: "user".into(), content: json!("x") }]).unwrap();
        assert!(DirectGlobal.reserve(&other_ttl).is_err());
    }

    #[test]
    fn fixtures_regression_test() {
        let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/api_bot");
        assert!(fixture_dir.exists(), "fixture dir exists: {fixture_dir:?}");
        let entries = std::fs::read_dir(&fixture_dir).unwrap();
        let mut count = 0;
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            let fixture: Value = serde_json::from_str(&content).unwrap();
            let name = fixture["name"].as_str().unwrap_or("unnamed");
            let expected = fixture["expected"].as_str().expect("fixture must have expected");
            let model = fixture["model"].as_str().expect("fixture must have model");
            let opt_val = &fixture["request_options"];
            let req_opts = Options {
                model: opt_val.get("model").and_then(Value::as_str).unwrap_or(model).into(),
                effort: opt_val.get("effort").and_then(Value::as_str).unwrap_or("high").into(),
                fast: opt_val.get("fast").and_then(Value::as_bool).unwrap_or(false),
                max_tokens: opt_val.get("max_tokens").and_then(Value::as_u64).unwrap_or(32000) as u32,
                compact_at_input_tokens: opt_val.get("compact_at_input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
                clear_tool_uses_at_input_tokens: opt_val.get("clear_tool_uses_at_input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
                clear_tool_uses_keep: opt_val.get("clear_tool_uses_keep").and_then(Value::as_u64).unwrap_or(3) as u32,
                clear_tool_uses_clear_at_least: opt_val.get("clear_tool_uses_clear_at_least").and_then(Value::as_u64).unwrap_or(5000) as u32,
                cache_ttl_1h: opt_val.get("cache_ttl_1h").and_then(Value::as_bool).unwrap_or(false),
                web_search: opt_val.get("web_search").and_then(Value::as_bool).unwrap_or(false),
                web_fetch: opt_val.get("web_fetch").and_then(Value::as_bool).unwrap_or(false),
            };
            let resp: Response = serde_json::from_value(fixture["response"].clone()).unwrap();
            let messages: Vec<Message> = if let Some(msgs) = fixture.get("request_messages") {
                serde_json::from_value(msgs.clone()).unwrap()
            } else {
                let max_iter_tokens = resp.usage.get("iterations")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter().map(|it| {
                            let i_in = it.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                            let i_read = it.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                            let i_write = it.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                            i_in + i_read + i_write
                        }).max().unwrap_or(0)
                    }).unwrap_or(0);
                let top_tokens = resp.usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0)
                    + resp.usage.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0)
                    + resp.usage.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                let needed = max_iter_tokens.max(top_tokens);
                let prompt_len = (needed as usize * 3).max(100);
                vec![Message { role: "user".into(), content: json!("x".repeat(prompt_len)) }]
            };
            let envelope = RequestEnvelope::new(req_opts, json!("fixture system"), messages).unwrap();

            let quote = DirectGlobal.settle(&envelope, &resp);
            let classified = classify_usage_problem(&envelope, &resp);

            match expected {
                "accepted" => {
                    assert!(quote.is_ok(), "fixture {name} expected accepted, got quote err: {:?}", quote.err());
                    assert!(classified.is_none(), "fixture {name} expected accepted, got classified problem: {:?}", classified);
                }
                "rejected" => {
                    assert!(quote.is_err() || classified.is_some(), "fixture {name} expected rejected, but quote and classified succeeded");
                    if let Some(expected_sub) = fixture.get("expected_reason_contains").and_then(Value::as_str) {
                        let problem = classified.unwrap_or_default();
                        assert!(problem.contains(expected_sub), "fixture {name} expected reason to contain {:?}, got {:?}", expected_sub, problem);
                    }
                }
                other => panic!("unknown expected outcome: {other}"),
            }
            count += 1;
        }
        assert!(count >= 2, "expected at least 2 fixtures tested, found {count}");
    }

    #[test]
    fn server_tool_use_pricing_and_authorization() {
        let mut opts = Options {
            model: crate::config::API_BOT_MODEL.into(),
            effort: "high".into(),
            fast: false,
            max_tokens: 1024,
            compact_at_input_tokens: 0,
            cache_ttl_1h: false,
            web_search: false,
            web_fetch: false,
            ..Default::default()
        };
        let messages = vec![Message { role: "user".into(), content: json!("search something") }];
        let unauth_req = RequestEnvelope::new(opts.clone(), json!("sys"), messages.clone()).unwrap();

        // 1. web_search 未開：若 usage 帶 web_search_requests: 1 則報 UntrustedUsage
        let resp_search_1 = response(json!({
            "input_tokens": 10, "output_tokens": 10, "speed": "standard",
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
            "server_tool_use": {"web_search_requests": 1, "web_fetch_requests": 0},
            "service_tier": "standard", "inference_geo": "global"
        }));
        assert!(DirectGlobal.settle(&unauth_req, &resp_search_1).is_err());
        let problem = classify_usage_problem(&unauth_req, &resp_search_1).unwrap();
        assert!(problem.contains("未啟用 web_search"));

        // 2. web_fetch 未開：若 usage 帶 web_fetch_requests: 1 則報 UntrustedUsage
        let resp_fetch_1 = response(json!({
            "input_tokens": 10, "output_tokens": 10, "speed": "standard",
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
            "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 1},
            "service_tier": "standard", "inference_geo": "global"
        }));
        assert!(DirectGlobal.settle(&unauth_req, &resp_fetch_1).is_err());
        let problem_fetch = classify_usage_problem(&unauth_req, &resp_fetch_1).unwrap();
        assert!(problem_fetch.contains("未啟用 web_fetch"));

        // 3. 兩者皆開：正確計入 web search 費用（每次 10,000 µUSD），web fetch 不加收
        opts.web_search = true;
        opts.web_fetch = true;
        let auth_req = RequestEnvelope::new(opts, json!("sys"), messages).unwrap();
        let resp_auth = response(json!({
            "input_tokens": 100, "output_tokens": 20, "speed": "standard",
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
            "server_tool_use": {"web_search_requests": 3, "web_fetch_requests": 2},
            "service_tier": "standard", "inference_geo": "global"
        }));
        let quote = DirectGlobal.settle(&auth_req, &resp_auth).expect("settle ok");
        // Opus: 100 * $4 + 20 * $20 = 400 + 400 = 800 µUSD token 費用
        // 3 次 web_search = 3 * 10,000 = 30,000 µUSD
        // 總共 30,800 µUSD
        assert_eq!(quote.estimated_microusd, 30_800);
        assert!(classify_usage_problem(&auth_req, &resp_auth).is_none());
    }

    #[test]
    fn attachment_media_token_bound_calculation() {
        let opts = Options {
            model: crate::config::API_BOT_MODEL.into(),
            effort: "high".into(),
            fast: false,
            max_tokens: 1024,
            compact_at_input_tokens: 0,
            cache_ttl_1h: false,
            web_search: false,
            web_fetch: false,
            ..Default::default()
        };
        // 模擬 3MB 圖片附件（base64 約 400 萬字元）
        let img_b64 = "A".repeat(4_000_000);
        let messages = vec![Message {
            role: "user".into(),
            content: json!([
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": img_b64}},
                {"type": "text", "text": "請看這張圖片"}
            ]),
        }];
        let req = RequestEnvelope::new(opts.clone(), json!("sys"), messages).expect("envelope ok");
        let bound = input_token_bound(&req).expect("bound calculated");
        // 若以位元組直算會爆 400 萬（> 1M token 被拒）；調整後應在 8000 + margin 約 16,000 左右
        assert!(bound < 50_000, "圖片 token bound 應遠小於 1M context：{bound}");
        assert!(bound >= 8_000 + INPUT_MARGIN_TOKENS);

        // 模擬 4MB PDF 附件（base64 約 530 萬字元）
        let pdf_b64 = "B".repeat(5_300_000);
        let pdf_messages = vec![Message {
            role: "user".into(),
            content: json!([
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": pdf_b64}},
                {"type": "text", "text": "請總結這份 PDF"}
            ]),
        }];
        let pdf_req = RequestEnvelope::new(opts, json!("sys"), pdf_messages).expect("envelope ok");
        let pdf_bound = input_token_bound(&pdf_req).expect("pdf bound calculated");
        assert!(pdf_bound < MAX_CONTEXT_TOKENS, "PDF token bound 應小於 1M context：{pdf_bound}");
        assert!(pdf_bound >= 200_000, "PDF token bound 有保守估計：{pdf_bound}");
    }
}
