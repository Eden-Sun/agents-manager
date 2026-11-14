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
    pub model: String,
    pub effort: String,
    pub fast: bool,
    pub max_tokens: u32,
    pub compact_at_input_tokens: u32,
}

impl Options {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !crate::config::api_bot_model_supported(&self.model) { return Err("unsupported model"); }
        if !matches!(
            self.effort.as_str(),
            "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Err("invalid effort");
        }
        if self.max_tokens == 0 || self.max_tokens > 64_000 {
            return Err("invalid output token limit");
        }
        if self.compact_at_input_tokens != 0 && !(50_000..=900_000).contains(&self.compact_at_input_tokens) {
            return Err("invalid compaction threshold");
        }
        Ok(())
    }
}

/// Prompt caching（5 分鐘 ephemeral）：斷點放在 system 末尾（涵蓋 tools＋system）、最後一則訊息與其前一則的最後一個 block，
/// 共 3 個（上限 4）。下一次呼叫時前一次的「最後一則」就是前綴，因此讀得到快取。斷點只加在送出的 body，不進 checkpoint。
fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

fn mark_last_block(content: &Value) -> Value {
    match content {
        Value::String(text) if !text.is_empty() => {
            json!([{"type": "text", "text": text, "cache_control": ephemeral()}])
        }
        Value::Array(blocks) => {
            let mut blocks = blocks.clone();
            if let Some(Value::Object(last)) = blocks.last_mut() {
                // thinking 區塊不能帶 cache_control。
                if !matches!(last.get("type").and_then(Value::as_str), Some("thinking" | "redacted_thinking")) {
                    last.insert("cache_control".into(), ephemeral());
                }
            }
            Value::Array(blocks)
        }
        other => other.clone(),
    }
}

/// `thinking.block_binding` 需要的 beta header 值。
pub const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
/// Server-side compaction beta header 值。
pub const COMPACT_BETA: &str = "compact-2026-01-12";
/// Compaction 自訂摘要指示詞。
pub const COMPACT_INSTRUCTIONS: &str = "Keep the user's goal, decisions made, files and findings so far, open questions, and what remains to do.";

fn strip_thinking_before_compaction(messages: &[Message]) -> Vec<Message> {
    let mut last_compaction = None;
    for (m_idx, m) in messages.iter().enumerate() {
        if let Value::Array(blocks) = &m.content {
            for (b_idx, b) in blocks.iter().enumerate() {
                if b.get("type").and_then(Value::as_str) == Some("compaction") {
                    last_compaction = Some((m_idx, b_idx));
                }
            }
        }
    }
    let Some((comp_m_idx, comp_b_idx)) = last_compaction else {
        return messages.to_vec();
    };
    messages
        .iter()
        .enumerate()
        .map(|(m_idx, m)| {
            if m_idx > comp_m_idx {
                m.clone()
            } else if let Value::Array(blocks) = &m.content {
                let filtered: Vec<Value> = blocks
                    .iter()
                    .enumerate()
                    .filter_map(|(b_idx, b)| {
                        let is_before = m_idx < comp_m_idx || b_idx < comp_b_idx;
                        let is_thinking = matches!(
                            b.get("type").and_then(Value::as_str),
                            Some("thinking" | "redacted_thinking")
                        );
                        if is_before && is_thinking {
                            None
                        } else {
                            Some(b.clone())
                        }
                    })
                    .collect();
                Message {
                    role: m.role.clone(),
                    content: Value::Array(filtered),
                }
            } else {
                m.clone()
            }
        })
        .collect()
}

pub fn request(options: &Options, system: &Value, messages: &[Message]) -> Value {
    let stripped_messages = strip_thinking_before_compaction(messages);
    let system = mark_last_block(system);
    let last = stripped_messages.len().saturating_sub(1);
    let messages: Vec<Message> = stripped_messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if i == last || (i + 2 == stripped_messages.len()) {
                Message { role: m.role.clone(), content: mark_last_block(&m.content) }
            } else {
                m.clone()
            }
        })
        .collect();
    let mut body = json!({
        "model": options.model, "max_tokens": options.max_tokens,
        "output_config": {"effort": options.effort},
        "system": system, "messages": messages,
        "tools": [
            {"type": "bash_20250124", "name": "bash"},
            {"type": "text_editor_20250728", "name": "str_replace_based_edit_tool"}
        ], "tool_choice": {"type": "auto"}
    });
    // Opus 5.5／Fable 5.2 的 thinking 區塊綁定產生它的那段對話（system＋前文）。system 每回合重組
    // （agent md 從設定檔、遠端主機重讀），只要一個位元組不同，下一回合整段歷史就被 API 以 400 拒絕、bot 從此卡死。
    // drop_block：前綴對不上的 thinking 區塊由 API 丟掉、對話照常進行（需 `THINKING_BINDING_BETA` header）。
    body["thinking"] = json!({"type":"adaptive","block_binding":{"prefix_mismatch_behavior":"drop_block"}});
    if options.fast {
        body["speed"] = json!("fast");
    }
    if options.compact_at_input_tokens > 0 {
        body["context_management"] = json!({
            "edits": [
                {
                    "type": "compact_20260112",
                    "trigger": {
                        "type": "input_tokens",
                        "value": options.compact_at_input_tokens,
                    },
                    "instructions": COMPACT_INSTRUCTIONS,
                }
            ]
        });
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
        if self.id.is_empty() || !crate::config::api_bot_model_supported(&self.model) || !self.usage.is_object() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_validation_handles_compact_at_input_tokens() {
        let mut opts = Options {
            model: crate::config::API_BOT_MODEL.into(),
            effort: "medium".into(),
            fast: false,
            max_tokens: 4096,
            compact_at_input_tokens: 0,
        };
        assert!(opts.validate().is_ok());
        opts.compact_at_input_tokens = 50_000;
        assert!(opts.validate().is_ok());
        opts.compact_at_input_tokens = 150_000;
        assert!(opts.validate().is_ok());
        opts.compact_at_input_tokens = 900_000;
        assert!(opts.validate().is_ok());
        opts.compact_at_input_tokens = 49_999;
        assert!(opts.validate().is_err());
        opts.compact_at_input_tokens = 900_001;
        assert!(opts.validate().is_err());
    }

    #[test]
    fn context_management_included_only_when_compaction_enabled() {
        let system = json!("system");
        let messages = vec![Message {
            role: "user".into(),
            content: json!("hello"),
        }];
        let mut opts = Options {
            model: crate::config::API_BOT_MODEL.into(),
            effort: "medium".into(),
            fast: false,
            max_tokens: 4096,
            compact_at_input_tokens: 0,
        };
        let body_no_compact = request(&opts, &system, &messages);
        assert!(body_no_compact.get("context_management").is_none());

        opts.compact_at_input_tokens = 150_000;
        let body_compact = request(&opts, &system, &messages);
        let cm = body_compact.get("context_management").expect("context_management should be present");
        let edits = cm["edits"].as_array().expect("edits array");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["type"], "compact_20260112");
        assert_eq!(edits[0]["trigger"]["type"], "input_tokens");
        assert_eq!(edits[0]["trigger"]["value"], 150_000);
        assert_eq!(edits[0]["instructions"], COMPACT_INSTRUCTIONS);
    }

    #[test]
    fn thinking_blocks_before_compaction_are_stripped() {
        let system = json!("system");
        let messages = vec![
            Message {
                role: "user".into(),
                content: json!("first question"),
            },
            Message {
                role: "assistant".into(),
                content: json!([
                    {"type": "thinking", "thinking": "early reasoning", "signature": "sig1"},
                    {"type": "text", "text": "early reply"}
                ]),
            },
            Message {
                role: "user".into(),
                content: json!("second question"),
            },
            Message {
                role: "assistant".into(),
                content: json!([
                    {"type": "thinking", "thinking": "reasoning before compaction", "signature": "sig2"},
                    {"type": "compaction", "content": "summary of conversation"},
                    {"type": "thinking", "thinking": "fresh reasoning after compaction", "signature": "sig3"},
                    {"type": "text", "text": "fresh reply"}
                ]),
            },
            Message {
                role: "user".into(),
                content: json!("third question"),
            },
        ];
        let opts = Options {
            model: crate::config::API_BOT_MODEL.into(),
            effort: "medium".into(),
            fast: false,
            max_tokens: 4096,
            compact_at_input_tokens: 150_000,
        };
        let body = request(&opts, &system, &messages);
        let sent_msgs = body["messages"].as_array().unwrap();

        // Turn 1 assistant message: thinking block is stripped, text block remains
        let turn1_blocks = sent_msgs[1]["content"].as_array().unwrap();
        assert_eq!(turn1_blocks.len(), 1);
        assert_eq!(turn1_blocks[0]["type"], "text");
        assert_eq!(turn1_blocks[0]["text"], "early reply");

        // Turn 2 assistant message: thinking before compaction is stripped,
        // compaction block and subsequent thinking/text are kept
        let turn2_blocks = sent_msgs[3]["content"].as_array().unwrap();
        assert_eq!(turn2_blocks.len(), 3);
        assert_eq!(turn2_blocks[0]["type"], "compaction");
        assert_eq!(turn2_blocks[0]["content"], "summary of conversation");
        assert_eq!(turn2_blocks[1]["type"], "thinking");
        assert_eq!(turn2_blocks[1]["thinking"], "fresh reasoning after compaction");
        assert_eq!(turn2_blocks[2]["type"], "text");
        assert_eq!(turn2_blocks[2]["text"], "fresh reply");
    }
}
