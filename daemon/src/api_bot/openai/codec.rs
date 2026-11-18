//! OpenAI Response Codec

use crate::api_bot::openai::pricing;

pub struct OpenAIRequest {
    pub instructions: String,
    pub reasoning_effort: String,
}

pub struct OpenAIResponse {
    pub text: String,
    pub usage: pricing::Usage,
    pub cost_microusd: u64,
}

pub fn encode_request(req: &OpenAIRequest) -> String {
    format!(r#"{{"instructions": "{}", "reasoning_effort": "{}"}}"#, req.instructions, req.reasoning_effort)
}

pub fn decode_response(data: &str, usage: pricing::Usage) -> OpenAIResponse {
    let cost = pricing::calculate_cost(&usage);
    OpenAIResponse {
        text: data.to_string(),
        usage,
        cost_microusd: cost,
    }
}
