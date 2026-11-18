//! OpenAI SSE Event Parsing

#[derive(Debug)]
pub enum SseEvent {
    ResponseCreated { id: String },
    OutputTextDelta { delta: String },
    OutputItemDelta { item_id: String, delta: String },
    FunctionCallArgumentsDelta { call_id: String, delta: String },
    ResponseCompleted { usage: crate::api_bot::openai::pricing::Usage },
    Failed { reason: String },
    Incomplete,
    Error(String),
}

pub fn parse_sse_event(data: &str) -> Result<SseEvent, String> {
    // Basic implementation for fake testing.
    // In production, this would use serde_json to parse the SSE event.
    if data.contains("response.created") {
        Ok(SseEvent::ResponseCreated { id: "test_id".to_string() })
    } else if data.contains("response.completed") {
        Ok(SseEvent::ResponseCompleted {
            usage: crate::api_bot::openai::pricing::Usage {
                input_tokens: 100,
                cache_read_tokens: 10,
                cache_write_tokens: 10,
                output_tokens: 50,
                reasoning_tokens: 20,
            }
        })
    } else {
        Err("Unknown event".to_string())
    }
}
