#[cfg(test)]
mod tests {
    use super::pricing::{calculate_cost, Usage};
    use super::sse::{parse_sse_event, SseEvent};
    use super::codec::{encode_request, decode_response, OpenAIRequest};

    #[test]
    fn test_pricing() {
        let usage = Usage {
            input_tokens: 1_000_000,
            cache_read_tokens: 100_000,
            cache_write_tokens: 100_000,
            output_tokens: 100_000,
            reasoning_tokens: 50_000,
        };
        assert_eq!(calculate_cost(&usage), 143_500);
    }

    #[test]
    fn test_sse_parsing() {
        assert!(parse_sse_event("data: response.created").is_ok());
        assert!(parse_sse_event("data: response.completed").is_ok());
    }

    #[test]
    fn test_codec() {
        let req = OpenAIRequest {
            instructions: "Hello".to_string(),
            reasoning_effort: "medium".to_string(),
        };
        let encoded = encode_request(&req);
        assert!(encoded.contains("Hello"));
        
        let usage = Usage {
            input_tokens: 10,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 10,
            reasoning_tokens: 0,
        };
        let response = decode_response("World", usage);
        assert_eq!(response.text, "World");
    }
}
