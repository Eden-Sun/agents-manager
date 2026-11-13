//! Direct global price snapshots for supported API bot models. Integer µUSD, fail closed on unknown models/usage.
use super::protocol::Response;
use super::quote::{QuoteError, QuoteProvider, RequestEnvelope, ReservationQuote, UsageQuote};
use serde_json::Value;

pub(super) struct DirectGlobal;

fn forbidden(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            // cache_control 只接受 5 分鐘 ephemeral（其餘 TTL 另有單價，未核准）。
            (key == "cache_control" && value != &serde_json::json!({"type": "ephemeral"}))
                || matches!(key.as_str(), "service_tier" | "inference_geo" | "container")
                || (key == "type" && value.as_str().is_some_and(|v| v.starts_with("server_") || v == "web_search" || v == "web_fetch"))
                || forbidden(value)
        }),
        Value::Array(items) => items.iter().any(forbidden),
        _ => false,
    }
}

/// 單價以「0.1 µUSD／token」為單位（每百萬 token 的美元數 ×10），整數運算、最後向上進位成 µUSD。
/// 各模型的標準 input／cache write／cache read／output 分列；fast 全部 2×。
struct Rates { input: i64, write_5m: i64, write_1h: i64, read: i64, output: i64, version: &'static str }
fn rates(model: &str) -> Result<Rates, QuoteError> {
    match model {
        "claude-opus-5-5" => Ok(Rates { input: 40, write_5m: 50, write_1h: 80, read: 4, output: 200, version: "claude-opus-5-5-global-2026-11-01" }),
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

/// 這個 request 輸入 token 的保守上界（不低估）；估不出來（超過 1M context）就拒絕。
pub(super) fn input_token_bound(request: &RequestEnvelope) -> Result<u64, QuoteError> {
    let bytes = serde_json::to_vec(&request.body()).map_err(|_| QuoteError::InvalidRequest)?.len() as u64;
    let bound = bytes.checked_add(INPUT_MARGIN_TOKENS).ok_or(QuoteError::InvalidRequest)?;
    if bound > MAX_CONTEXT_TOKENS { return Err(QuoteError::InvalidRequest); }
    Ok(bound)
}

/// 預留：整段輸入都按「快取寫入」最貴的價算（最壞情況），輸出按 max_tokens。
fn reserve_microusd(input_bound: u64, max_tokens: u32, speed: &str, rates: &Rates) -> Result<i64, QuoteError> {
    let factor = speed_factor(speed)?;
    let input = i64::try_from(input_bound).map_err(|_| QuoteError::InvalidRequest)?;
    input.checked_mul(rates.write_5m)
        .and_then(|n| i64::from(max_tokens).checked_mul(rates.output).and_then(|m| n.checked_add(m)))
        .and_then(|n| n.checked_mul(factor))
        .map(to_micro)
        .ok_or(QuoteError::InvalidRequest)
}

impl QuoteProvider for DirectGlobal {
    fn reserve(&self, request: &RequestEnvelope) -> Result<ReservationQuote, QuoteError> {
        let body = request.body();
        if forbidden(&body) {
            return Err(QuoteError::InvalidRequest);
        }
        let rates = rates(&request.options().model)?;
        Ok(ReservationQuote {
            reserved_microusd: reserve_microusd(input_token_bound(request)?, request.options().max_tokens, request.speed(), &rates)?,
            pricing_version: rates.version.into(),
        })
    }

    fn settle(&self, request: &RequestEnvelope, response: &Response) -> Result<UsageQuote, QuoteError> {
        let reservation = self.reserve(request)?;
        let rates = rates(&request.options().model)?;
        if response.model != request.options().model { return Err(QuoteError::UntrustedUsage); }
        let usage = response.usage.as_object().ok_or(QuoteError::UntrustedUsage)?;
        if usage.keys().any(|k| !matches!(k.as_str(), "input_tokens" | "output_tokens" | "speed" | "cache_creation_input_tokens" | "cache_read_input_tokens" | "cache_creation" | "server_tool_use" | "output_tokens_details" | "service_tier" | "inference_geo")) { return Err(QuoteError::UntrustedUsage); }
        let input = usage.get("input_tokens").and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage)?;
        let output = usage.get("output_tokens").and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage)?;
        let speed = usage.get("speed").and_then(Value::as_str).ok_or(QuoteError::UntrustedUsage)?;
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage);
        let cache_creation = count("cache_creation_input_tokens")?;
        let cache_read = count("cache_read_input_tokens")?;
        // `cache_creation` 細項（若有）必須只含兩個 TTL 欄位且總和等於 cache_creation_input_tokens；沒有細項就全部當 5 分鐘寫入。
        let (write_5m, write_1h) = match usage.get("cache_creation") {
            None | Some(Value::Null) => (cache_creation, 0),
            Some(Value::Object(m)) if m.keys().all(|k| matches!(k.as_str(), "ephemeral_1h_input_tokens" | "ephemeral_5m_input_tokens")) => {
                let part = |k: &str| m.get(k).and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage);
                let (h, f) = (part("ephemeral_1h_input_tokens")?, part("ephemeral_5m_input_tokens")?);
                if h.checked_add(f) != Some(cache_creation) { return Err(QuoteError::UntrustedUsage); }
                (f, h)
            }
            _ => return Err(QuoteError::UntrustedUsage),
        };
        let zero_fields = |value: Option<&Value>, names: &[&str]| -> bool {
            match value {
                None | Some(Value::Null) => true,
                Some(Value::Object(m)) => m.len() == names.len() && names.iter().all(|name| m.get(*name).and_then(Value::as_u64) == Some(0)),
                _ => false,
            }
        };
        let details_ok = match usage.get("output_tokens_details") {
            None | Some(Value::Null) => true,
            Some(Value::Object(m)) => m.len() == 1 && m.get("thinking_tokens").and_then(Value::as_u64).is_some_and(|n| n <= output),
            _ => false,
        };
        let neutral_metadata = |key: &str, accepted: &str| match usage.get(key) {
            None | Some(Value::Null) => true,
            Some(Value::String(v)) => v == accepted,
            _ => false,
        };
        let total_input = input.checked_add(cache_creation).and_then(|n| n.checked_add(cache_read)).ok_or(QuoteError::UntrustedUsage)?;
        if total_input > input_token_bound(request)? || output > u64::from(request.options().max_tokens)
            || (request.speed() == "standard" && speed != "standard")
            || !zero_fields(usage.get("server_tool_use"), &["web_fetch_requests", "web_search_requests"])
            || !details_ok || !neutral_metadata("service_tier", "standard") || !neutral_metadata("inference_geo", "global")
        { return Err(QuoteError::UntrustedUsage); }
        let actual_speed = match speed { "standard" => "standard", "fast" => "fast", _ => return Err(QuoteError::UntrustedUsage) };
        let tenths = [(input, rates.input), (write_5m, rates.write_5m), (write_1h, rates.write_1h), (cache_read, rates.read), (output, rates.output)]
            .into_iter()
            .try_fold(0_i64, |acc, (n, rate)| i64::try_from(n).ok().and_then(|n| n.checked_mul(rate)).and_then(|n| acc.checked_add(n)))
            .and_then(|n| n.checked_mul(speed_factor(actual_speed).ok()?))
            .ok_or(QuoteError::UntrustedUsage)?;
        let estimated_microusd = to_micro(tenths);
        if estimated_microusd > reservation.reserved_microusd { return Err(QuoteError::UntrustedUsage); }
        Ok(UsageQuote { estimated_microusd, actual_speed: Some(actual_speed) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_bot::protocol::{Message, Options};
    use serde_json::json;

    fn request(fast: bool, system: Value) -> RequestEnvelope {
        RequestEnvelope::new(Options { model: crate::config::API_BOT_MODEL.into(), effort: "high".into(), fast, max_tokens: 1024 }, system,
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
        // 100×$4 + 1000×$5（5 分鐘寫入）+ 2000×$0.4（讀取）+ 10×$20 = 400+5000+800+200 = 6400 µUSD
        assert_eq!(DirectGlobal.settle(&standard, &with(1000, 2000, Value::Null)).unwrap().estimated_microusd, 6400);
        let detailed = json!({"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":1000});
        assert_eq!(DirectGlobal.settle(&standard, &with(1000, 2000, detailed)).unwrap().estimated_microusd, 6400);
        // 1 小時寫入 2×：500×$8 = 4000
        let hour = json!({"ephemeral_1h_input_tokens":500,"ephemeral_5m_input_tokens":500});
        assert_eq!(DirectGlobal.settle(&standard, &with(1000, 0, hour)).unwrap().estimated_microusd, 400 + 2500 + 4000 + 200);
        // 細項總和與總數不符 → 不信任
        assert!(DirectGlobal.settle(&standard, &with(1000, 0, json!({"ephemeral_1h_input_tokens":1,"ephemeral_5m_input_tokens":1}))).is_err());
        // fast 實際單價 2×
        let mut f = with(0, 2000, Value::Null);
        f.usage["speed"] = json!("fast");
        assert_eq!(DirectGlobal.settle(&fast, &f).unwrap().estimated_microusd, (100 * 40 + 2000 * 4 + 10 * 200) * 2 / 10);
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
        let options = Options { model: crate::config::API_BOT_MODEL.into(), effort: "high".into(), fast: false, max_tokens: 1024 };
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
            let options = Options { model: crate::config::API_BOT_FABLE_MODEL.into(), effort: "high".into(), fast, max_tokens: 1024 };
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

}
