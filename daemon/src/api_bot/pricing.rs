//! Approved 2026-10-29 direct global Opus 5.5 price snapshot.
use super::protocol::Response;
use super::quote::{QuoteError, QuoteProvider, RequestEnvelope, ReservationQuote, UsageQuote};
use serde_json::Value;

pub(super) struct Opus55Global;

fn forbidden(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(key.as_str(), "cache_control" | "service_tier" | "inference_geo" | "container")
                || (key == "type" && value.as_str().is_some_and(|v| v.starts_with("server_") || v == "web_search" || v == "web_fetch"))
                || forbidden(value)
        }),
        Value::Array(items) => items.iter().any(forbidden),
        _ => false,
    }
}

fn rates(speed: &str) -> Result<(i64, i64), QuoteError> {
    match speed {
        "standard" => Ok((4, 20)),
        "fast" => Ok((8, 40)),
        _ => Err(QuoteError::UntrustedUsage),
    }
}

fn cost(input: u64, output: u64, speed: &str) -> Result<i64, QuoteError> {
    let (i, o) = rates(speed)?;
    let input = i64::try_from(input).map_err(|_| QuoteError::UntrustedUsage)?;
    let output = i64::try_from(output).map_err(|_| QuoteError::UntrustedUsage)?;
    input.checked_mul(i).and_then(|n| output.checked_mul(o).and_then(|m| n.checked_add(m))).ok_or(QuoteError::UntrustedUsage)
}

impl QuoteProvider for Opus55Global {
    fn reserve(&self, request: &RequestEnvelope) -> Result<ReservationQuote, QuoteError> {
        let body = request.body();
        if body.get("model").and_then(Value::as_str) != Some("claude-opus-5-5") || forbidden(&body) {
            return Err(QuoteError::InvalidRequest);
        }
        Ok(ReservationQuote {
            reserved_microusd: cost(1_000_000, u64::from(request.options().max_tokens), request.speed())?,
            pricing_version: "claude-opus-5-5-global-2026-10-29".into(),
        })
    }

    fn settle(&self, request: &RequestEnvelope, response: &Response) -> Result<UsageQuote, QuoteError> {
        let reservation = self.reserve(request)?;
        if response.model != "claude-opus-5-5" { return Err(QuoteError::UntrustedUsage); }
        let usage = response.usage.as_object().ok_or(QuoteError::UntrustedUsage)?;
        if usage.keys().any(|k| !matches!(k.as_str(), "input_tokens" | "output_tokens" | "speed" | "cache_creation_input_tokens" | "cache_read_input_tokens" | "cache_creation" | "server_tool_use" | "output_tokens_details" | "service_tier" | "inference_geo")) { return Err(QuoteError::UntrustedUsage); }
        let input = usage.get("input_tokens").and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage)?;
        let output = usage.get("output_tokens").and_then(Value::as_u64).ok_or(QuoteError::UntrustedUsage)?;
        let speed = usage.get("speed").and_then(Value::as_str).ok_or(QuoteError::UntrustedUsage)?;
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
        if input > 1_000_000 || output > u64::from(request.options().max_tokens)
            || (request.speed() == "standard" && speed != "standard")
            || ["cache_creation_input_tokens", "cache_read_input_tokens"].iter().any(|k| usage.get(*k).and_then(Value::as_u64) != Some(0))
            || !zero_fields(usage.get("cache_creation"), &["ephemeral_1h_input_tokens", "ephemeral_5m_input_tokens"])
            || !zero_fields(usage.get("server_tool_use"), &["web_fetch_requests", "web_search_requests"])
            || !details_ok || !neutral_metadata("service_tier", "standard") || !neutral_metadata("inference_geo", "global")
        { return Err(QuoteError::UntrustedUsage); }
        let actual_speed = match speed { "standard" => "standard", "fast" => "fast", _ => return Err(QuoteError::UntrustedUsage) };
        let estimated_microusd = cost(input, output, actual_speed)?;
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
        RequestEnvelope::new(Options { effort: "high".into(), fast, max_tokens: 1024 }, system,
            vec![Message { role: "user".into(), content: json!("fixture") }]).unwrap()
    }
    fn response(usage: Value) -> Response {
        Response { id: "fake".into(), model: "claude-opus-5-5".into(), content: vec![], stop_reason: "end_turn".into(), usage }
    }
    #[test]
    fn approved_quotes_are_integer_upper_bounds_and_reject_untrusted_usage() {
        let standard = request(false, json!("fixture"));
        let fast = request(true, json!("fixture"));
        assert_eq!(Opus55Global.reserve(&standard).unwrap().reserved_microusd, 4_020_480);
        assert_eq!(Opus55Global.reserve(&fast).unwrap().reserved_microusd, 8_040_960);
        let usage = response(json!({"input_tokens": 8, "output_tokens": 12, "speed": "standard", "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "cache_creation": {"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0}, "server_tool_use": {"web_fetch_requests":0,"web_search_requests":0}, "output_tokens_details":{"thinking_tokens":4}, "service_tier":"standard", "inference_geo":"global"}));
        assert_eq!(Opus55Global.settle(&standard, &usage).unwrap().estimated_microusd, 272);
        assert!(Opus55Global.settle(&fast, &usage).is_ok());
        for usage in [json!({"input_tokens": 8, "output_tokens": 12}), json!({"input_tokens": 8, "output_tokens": 12, "speed": "fast"}), json!({"input_tokens": 1_000_001, "output_tokens": 1, "speed": "standard"}), json!({"input_tokens": 1, "output_tokens": 1, "speed": "standard", "cache_read_input_tokens": 1})] {
            assert!(Opus55Global.settle(&standard, &response(usage)).is_err());
        }
        for field in ["cache_control", "service_tier", "inference_geo"] {
            assert!(Opus55Global.reserve(&request(false, json!({"nested": [{field: "forbidden"}]}))).is_err());
        }
        let valid = json!({"input_tokens":8,"output_tokens":12,"speed":"standard", "cache_creation_input_tokens":0,"cache_read_input_tokens":0});
        for (field, value) in [
            ("cache_creation_input_tokens", json!(null)),
            ("cache_read_input_tokens", json!(1)),
            ("cache_creation", json!({"ephemeral_1h_input_tokens":1,"ephemeral_5m_input_tokens":0})),
            ("server_tool_use", json!({"web_fetch_requests":0,"web_search_requests":1})),
            ("output_tokens_details", json!({"thinking_tokens":13})),
            ("service_tier", json!("priority")),
            ("inference_geo", json!("us")),
            ("unknown_modifier", json!(0)),
        ] {
            let mut changed = valid.clone();
            changed[field] = value;
            assert!(Opus55Global.settle(&standard, &response(changed)).is_err(), "{field}");
        }
        let details = response(json!({"input_tokens":8,"output_tokens":12,"speed":"standard", "cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens_details":{"thinking_tokens":12}}));
        assert_eq!(Opus55Global.settle(&standard, &details).unwrap().estimated_microusd, 272, "inclusive output is billed only once");
    }
}
