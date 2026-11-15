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
    pub clear_tool_uses_at_input_tokens: u32,
    pub clear_tool_uses_keep: u32,
    pub clear_tool_uses_clear_at_least: u32,
    /// 這顆 bot 的快取保留：true＝1 小時（`cache_control.ttl="1h"`，寫入 2×），false＝預設 5 分鐘。
    pub cache_ttl_1h: bool,
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
        if self.clear_tool_uses_at_input_tokens != 0 && !(20_000..=900_000).contains(&self.clear_tool_uses_at_input_tokens) {
            return Err("invalid clear_tool_uses threshold");
        }
        if self.clear_tool_uses_at_input_tokens != 0 {
            if !(1..=50).contains(&self.clear_tool_uses_keep) {
                return Err("invalid clear_tool_uses keep count");
            }
            if !(1000..=100_000).contains(&self.clear_tool_uses_clear_at_least) {
                return Err("invalid clear_tool_uses clear_at_least tokens");
            }
        }
        Ok(())
    }
}

/// 降一級 effort（max → xhigh → high → medium → low；low 維持 low）。
pub fn lower_effort_level(effort: &str) -> &'static str {
    match effort.to_ascii_lowercase().as_str() {
        "max" => "xhigh",
        "xhigh" => "high",
        "high" => "medium",
        "medium" => "low",
        "low" => "low",
        _ => "medium",
    }
}

/// Prompt caching（5 分鐘 ephemeral）：斷點放在 system 末尾（涵蓋 tools＋system）、最後一則訊息與其前一則的最後一個 block，
/// 共 3 個（上限 4）。下一次呼叫時前一次的「最後一則」就是前綴，因此讀得到快取。斷點只加在送出的 body，不進 checkpoint。
fn ephemeral(ttl_1h: bool) -> Value {
    if ttl_1h {
        json!({"type": "ephemeral", "ttl": "1h"})
    } else {
        json!({"type": "ephemeral"})
    }
}

fn mark_last_block(content: &Value, ttl_1h: bool) -> Value {
    match content {
        Value::String(text) if !text.is_empty() => {
            json!([{"type": "text", "text": text, "cache_control": ephemeral(ttl_1h)}])
        }
        Value::Array(blocks) => {
            let mut blocks = blocks.clone();
            if let Some(Value::Object(last)) = blocks.last_mut() {
                // thinking 區塊不能帶 cache_control。
                if !matches!(last.get("type").and_then(Value::as_str), Some("thinking" | "redacted_thinking")) {
                    last.insert("cache_control".into(), ephemeral(ttl_1h));
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
/// Server-side context editing（clear_tool_uses）beta header 值。
pub const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";
/// Compaction 自訂摘要指示詞。
pub const COMPACT_INSTRUCTIONS: &str = "Keep the user's goal, decisions made, files and findings so far, open questions, and what remains to do.";

/// 檢查最後一則使用者訊息是否為主動要求壓縮（`/compact`）。
pub fn is_compact_request(messages: &[Message]) -> bool {
    messages.last().is_some_and(|m| {
        m.role == "user" && match &m.content {
            Value::String(s) => s.trim() == "/compact",
            Value::Array(blocks) => blocks.iter().any(|b| {
                b.get("type").and_then(Value::as_str) == Some("text")
                    && b.get("text").and_then(Value::as_str).is_some_and(|t| t.trim() == "/compact")
            }),
            _ => false,
        }
    })
}

/// 修剪訊息歷史：依 Anthropic compaction（beta compact-2026-01-12）語意，
/// 最新 compaction 區塊之前的訊息伺服器不再使用。
/// 送出與 checkpoint 只保留最新 compaction 區塊（含）之後的內容。
pub fn prune_messages_for_compaction(messages: &[Message]) -> Vec<Message> {
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

    let mut pruned = Vec::new();
    if let Some(m) = messages.get(comp_m_idx) {
        if let Value::Array(blocks) = &m.content {
            let kept_blocks: Vec<Value> = blocks[comp_b_idx..].to_vec();
            pruned.push(Message {
                role: m.role.clone(),
                content: Value::Array(kept_blocks),
            });
        } else {
            pruned.push(m.clone());
        }
    }
    pruned.extend_from_slice(&messages[comp_m_idx + 1..]);
    pruned
}

pub fn request(options: &Options, system: &Value, messages: &[Message]) -> Value {
    let pruned_messages = prune_messages_for_compaction(messages);
    let system = mark_last_block(system, options.cache_ttl_1h);
    let last = pruned_messages.len().saturating_sub(1);
    let messages: Vec<Message> = pruned_messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if i == last || (i + 2 == pruned_messages.len()) {
                Message { role: m.role.clone(), content: mark_last_block(&m.content, options.cache_ttl_1h) }
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
    // 串流：長回合才看得到進度；回應由 sse.rs 組回與非串流相同的 Response。
    body["stream"] = Value::Bool(true);
    let mut edits = Vec::new();
    if options.compact_at_input_tokens > 0 || is_compact_request(&messages) {
        let trigger_val = if is_compact_request(&messages) { 1 } else { options.compact_at_input_tokens };
        edits.push(json!({
            "type": "compact_20260112",
            "trigger": {
                "type": "input_tokens",
                "value": trigger_val,
            },
            "instructions": COMPACT_INSTRUCTIONS,
        }));
    }
    if options.clear_tool_uses_at_input_tokens > 0 {
        edits.push(json!({
            "type": "clear_tool_uses_20250919",
            "trigger": {
                "type": "input_tokens",
                "value": options.clear_tool_uses_at_input_tokens,
            },
            "keep": {
                "type": "tool_uses",
                "value": options.clear_tool_uses_keep,
            },
            "clear_at_least": {
                "type": "input_tokens",
                "value": options.clear_tool_uses_clear_at_least,
            },
        }));
    }
    if !edits.is_empty() {
        body["context_management"] = json!({
            "edits": edits,
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
            clear_tool_uses_at_input_tokens: 0,
            clear_tool_uses_keep: 3,
            clear_tool_uses_clear_at_least: 5000,
            cache_ttl_1h: false,
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
        opts.compact_at_input_tokens = 0;

        // clear_tool_uses validation
        opts.clear_tool_uses_at_input_tokens = 20_000;
        assert!(opts.validate().is_ok());
        opts.clear_tool_uses_at_input_tokens = 19_999;
        assert!(opts.validate().is_err());
        opts.clear_tool_uses_at_input_tokens = 900_001;
        assert!(opts.validate().is_err());
        opts.clear_tool_uses_at_input_tokens = 100_000;
        opts.clear_tool_uses_keep = 0;
        assert!(opts.validate().is_err());
        opts.clear_tool_uses_keep = 3;
        opts.clear_tool_uses_clear_at_least = 500;
        assert!(opts.validate().is_err());
        opts.clear_tool_uses_clear_at_least = 5000;
        assert!(opts.validate().is_ok());
    }

    #[test]
    fn context_management_included_only_when_compaction_or_clear_enabled() {
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
            clear_tool_uses_at_input_tokens: 0,
            clear_tool_uses_keep: 3,
            clear_tool_uses_clear_at_least: 5000,
            cache_ttl_1h: false,
        };
        let body_none = request(&opts, &system, &messages);
        assert!(body_none.get("context_management").is_none());

        // Compaction only
        opts.compact_at_input_tokens = 150_000;
        let body_compact = request(&opts, &system, &messages);
        let cm = body_compact.get("context_management").expect("context_management should be present");
        let edits = cm["edits"].as_array().expect("edits array");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["type"], "compact_20260112");
        assert_eq!(edits[0]["trigger"]["type"], "input_tokens");
        assert_eq!(edits[0]["trigger"]["value"], 150_000);
        assert_eq!(edits[0]["instructions"], COMPACT_INSTRUCTIONS);

        // Clear tool uses only
        opts.compact_at_input_tokens = 0;
        opts.clear_tool_uses_at_input_tokens = 100_000;
        let body_clear = request(&opts, &system, &messages);
        let cm = body_clear.get("context_management").expect("context_management present");
        let edits = cm["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["type"], "clear_tool_uses_20250919");
        assert_eq!(edits[0]["trigger"]["value"], 100_000);
        assert_eq!(edits[0]["keep"]["value"], 3);
        assert_eq!(edits[0]["clear_at_least"]["value"], 5000);

        // Co-existence of both compaction and clear_tool_uses
        opts.compact_at_input_tokens = 150_000;
        let body_both = request(&opts, &system, &messages);
        let cm = body_both.get("context_management").expect("context_management present");
        let edits = cm["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0]["type"], "compact_20260112");
        assert_eq!(edits[1]["type"], "clear_tool_uses_20250919");

        // /compact user prompt forces compaction trigger value 1 even when threshold is 0
        opts.compact_at_input_tokens = 0;
        opts.clear_tool_uses_at_input_tokens = 0;
        let compact_msgs = vec![Message { role: "user".into(), content: json!("/compact") }];
        let body_manual_compact = request(&opts, &system, &compact_msgs);
        let cm = body_manual_compact.get("context_management").expect("context_management present");
        let edits = cm["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["type"], "compact_20260112");
        assert_eq!(edits[0]["trigger"]["value"], 1);
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
            clear_tool_uses_at_input_tokens: 0,
            clear_tool_uses_keep: 3,
            clear_tool_uses_clear_at_least: 5000,
            cache_ttl_1h: false,
        };
        let body = request(&opts, &system, &messages);
        let sent_msgs = body["messages"].as_array().unwrap();

        // Compaction 之前的訊息全丟棄；只保留最新 compaction 區塊（含）之後的內容。
        // 原本 5 則訊息，修剪後只剩 2 則：
        // 第 0 則：Turn 2 assistant message 修剪掉 compaction 前的 thinking，保留 compaction 及其後 blocks
        // 第 1 則：Turn 3 user message
        assert_eq!(sent_msgs.len(), 2);
        let turn2_blocks = sent_msgs[0]["content"].as_array().unwrap();
        assert_eq!(turn2_blocks.len(), 3);
        assert_eq!(turn2_blocks[0]["type"], "compaction");
        assert_eq!(turn2_blocks[0]["content"], "summary of conversation");
        assert_eq!(turn2_blocks[1]["type"], "thinking");
        assert_eq!(turn2_blocks[1]["thinking"], "fresh reasoning after compaction");
        assert_eq!(turn2_blocks[2]["type"], "text");
        assert_eq!(turn2_blocks[2]["text"], "fresh reply");

        assert_eq!(sent_msgs[1]["role"], "user");
        let turn3_blocks = sent_msgs[1]["content"].as_array().unwrap();
        assert_eq!(turn3_blocks[0]["type"], "text");
        assert_eq!(turn3_blocks[0]["text"], "third question");
    }

    #[test]
    fn prune_messages_keeps_only_latest_compaction_and_preserves_tool_pairs() {
        let messages = vec![
            Message { role: "user".into(), content: json!("q1") },
            Message { role: "assistant".into(), content: json!([
                {"type": "compaction", "content": "summary 1"},
                {"type": "text", "text": "a1"}
            ])},
            Message { role: "user".into(), content: json!("q2") },
            Message { role: "assistant".into(), content: json!([
                {"type": "thinking", "thinking": "old thinking", "signature": "sig_old"},
                {"type": "compaction", "content": "summary 2"},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}
            ])},
            Message { role: "user".into(), content: json!([
                {"type": "tool_result", "tool_use_id": "t1", "content": "file.txt", "is_error": false}
            ])},
        ];
        let pruned = prune_messages_for_compaction(&messages);
        assert_eq!(pruned.len(), 2, "只有最新 compaction 及之後的 tool_result：{pruned:?}");
        assert_eq!(pruned[0].role, "assistant");
        let asst_blocks = pruned[0].content.as_array().unwrap();
        assert_eq!(asst_blocks.len(), 2);
        assert_eq!(asst_blocks[0]["type"], "compaction");
        assert_eq!(asst_blocks[0]["content"], "summary 2");
        assert_eq!(asst_blocks[1]["type"], "tool_use");
        assert_eq!(asst_blocks[1]["id"], "t1");
        assert_eq!(pruned[1].role, "user");
        let user_blocks = pruned[1].content.as_array().unwrap();
        assert_eq!(user_blocks[0]["type"], "tool_result");
        assert_eq!(user_blocks[0]["tool_use_id"], "t1");
    }

    #[test]
    fn one_hour_cache_ttl_marks_every_breakpoint_with_ttl_1h() {
        let messages = vec![Message { role: "user".into(), content: json!("hello") }];
        let mut opts = Options {
            model: crate::config::API_BOT_MODEL.into(),
            effort: "medium".into(),
            fast: false,
            max_tokens: 4096,
            compact_at_input_tokens: 0,
            clear_tool_uses_at_input_tokens: 0,
            clear_tool_uses_keep: 3,
            clear_tool_uses_clear_at_least: 5000,
            cache_ttl_1h: false,
        };
        let five = request(&opts, &json!("system"), &messages).to_string();
        assert!(five.contains(r#""cache_control":{"type":"ephemeral"}"#) && !five.contains("1h"));
        opts.cache_ttl_1h = true;
        let hour = request(&opts, &json!("system"), &messages);
        let text = hour.to_string();
        assert_eq!(text.matches(r#""cache_control":{"ttl":"1h","type":"ephemeral"}"#).count(), text.matches("cache_control").count(), "每個斷點都帶 1h：{text}");
        assert!(text.matches("cache_control").count() >= 2);
    }

    #[test]
    fn effort_levels_downgrade_correctly() {
        assert_eq!(lower_effort_level("max"), "xhigh");
        assert_eq!(lower_effort_level("xhigh"), "high");
        assert_eq!(lower_effort_level("high"), "medium");
        assert_eq!(lower_effort_level("medium"), "low");
        assert_eq!(lower_effort_level("low"), "low");
        assert_eq!(lower_effort_level("HIGH"), "medium");
        assert_eq!(lower_effort_level("unknown"), "medium");
    }
}
