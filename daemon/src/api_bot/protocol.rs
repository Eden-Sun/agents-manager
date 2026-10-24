use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Content remains opaque JSON: thinking/signatures/unknown extension fields must round-trip.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: String,
    pub content: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub content: Vec<Value>,
    pub stop_reason: String,
    pub usage: Value,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub effort: String,
    pub fast: bool,
    pub max_tokens: u32,
}

impl Options {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !matches!(
            self.effort.as_str(),
            "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Err("invalid effort");
        }
        if self.max_tokens == 0 || self.max_tokens > 64_000 {
            return Err("invalid output token limit");
        }
        Ok(())
    }
}

pub fn request(options: &Options, system: &Value, messages: &[Message]) -> Value {
    let mut body = json!({
        "model": "claude-opus-5-5", "max_tokens": options.max_tokens,
        "output_config": {"effort": options.effort},
        "system": system, "messages": messages,
        "tools": [
            {"type": "bash_20250124", "name": "bash"},
            {"type": "text_editor_20250728", "name": "str_replace_based_edit_tool"}
        ], "tool_choice": {"type": "auto"}
    });
    if options.fast {
        body["speed"] = json!("fast");
    }
    body
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

impl Response {
    /// Validate the whole response before executing any tool, including later blocks.
    pub fn tools(&self) -> Result<Vec<ToolCall>, &'static str> {
        if self.id.is_empty() || self.model != "claude-opus-5-5" || !self.usage.is_object() {
            return Err("invalid response metadata");
        }
        let mut calls = Vec::new();
        let mut ids = std::collections::HashSet::new();
        for block in &self.content {
            let kind = block
                .get("type")
                .and_then(Value::as_str)
                .ok_or("invalid content block")?;
            if kind == "tool_use" {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or("missing tool id")?;
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("missing tool name")?;
                if !ids.insert(id.to_owned())
                    || !matches!(name, "bash" | "str_replace_based_edit_tool")
                {
                    return Err("duplicate or unsupported tool");
                }
                let input = block
                    .get("input")
                    .filter(|v| v.is_object())
                    .ok_or("invalid tool input")?;
                calls.push(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    input: input.clone(),
                });
            }
        }
        if (self.stop_reason == "tool_use") != !calls.is_empty() {
            return Err("stop reason and tools disagree");
        }
        Ok(calls)
    }
}
