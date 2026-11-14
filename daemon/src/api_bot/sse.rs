//! Messages API streaming（SSE）組裝器：把事件串重組成與非串流回應相同的 `Response`，
//! 讓 pricing／receipt／checkpoint 不必知道有串流。只做解析與組裝，不碰網路。
use super::protocol::Response;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// 進度快照尾端最多保留的字元數。
pub(crate) const TAIL_CHARS: usize = 2000;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamError {
    /// 串流中的 `error` 事件（type／message 原樣，未遮罩）。
    Api { kind: String, message: String },
    /// 事件順序、JSON 或內容不合法，或串流提前結束。
    Invalid,
}

/// 串流進行中的累計數字，給即時進度與估價。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StreamProgress {
    /// 目前為止的文字（只留尾端 `TAIL_CHARS` 字元）。
    pub text_tail: String,
    /// 已收到的文字 delta 字元數（含 thinking，不含 tool 參數）。
    pub generated_chars: usize,
    /// 已收到的 tool 參數／compaction 內容位元組數。
    pub other_bytes: usize,
    /// usage 目前回報的 input_tokens（message_start）。
    pub input_tokens: u64,
    /// usage 目前回報的 output_tokens（message_delta 才會更新，是累計值）。
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// usage 的 `speed`（`standard`／`fast`）；message_start 沒帶就是 None。
    pub speed: Option<String>,
}

pub(crate) struct StreamAssembler {
    buf: Vec<u8>,
    data: Vec<String>,
    id: Option<String>,
    model: Option<String>,
    usage: Map<String, Value>,
    stop_reason: Option<String>,
    blocks: Vec<Value>,
    partial_json: HashMap<usize, String>,
    stopped: bool,
    progress: StreamProgress,
}

impl StreamAssembler {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            data: Vec::new(),
            id: None,
            model: None,
            usage: Map::new(),
            stop_reason: None,
            blocks: Vec::new(),
            partial_json: HashMap::new(),
            stopped: false,
            progress: StreamProgress::default(),
        }
    }

    pub(crate) fn progress(&self) -> &StreamProgress {
        &self.progress
    }

    /// 餵一段位元組（可在任意位置切開，包含事件中間與 UTF-8 字元中間）。
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<(), StreamError> {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&line)?;
        }
        Ok(())
    }

    fn line(&mut self, line: &[u8]) -> Result<(), StreamError> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line[0] == b':' {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|b| *b == b':') {
            Some(i) => {
                let v = &line[i + 1..];
                (&line[..i], v.strip_prefix(b" ").unwrap_or(v))
            }
            None => (line, &line[line.len()..]),
        };
        if field == b"data" {
            self.data.push(String::from_utf8(value.to_vec()).map_err(|_| StreamError::Invalid)?);
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Result<(), StreamError> {
        if self.data.is_empty() {
            return Ok(());
        }
        let raw = self.data.join("\n");
        self.data.clear();
        let event: Value = serde_json::from_str(&raw).map_err(|_| StreamError::Invalid)?;
        self.event(&event)
    }

    fn event(&mut self, ev: &Value) -> Result<(), StreamError> {
        if self.stopped {
            return Err(StreamError::Invalid);
        }
        match ev.get("type").and_then(Value::as_str).ok_or(StreamError::Invalid)? {
            "ping" => {}
            "error" => {
                let err = ev.get("error");
                let get = |k: &str| err.and_then(|e| e.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
                return Err(StreamError::Api { kind: get("type"), message: get("message") });
            }
            "message_start" => {
                if self.id.is_some() {
                    return Err(StreamError::Invalid);
                }
                let m = ev.get("message").and_then(Value::as_object).ok_or(StreamError::Invalid)?;
                self.id = Some(m.get("id").and_then(Value::as_str).ok_or(StreamError::Invalid)?.to_string());
                self.model = Some(m.get("model").and_then(Value::as_str).ok_or(StreamError::Invalid)?.to_string());
                if let Some(Value::Object(u)) = m.get("usage") {
                    self.merge_usage(u);
                }
                if let Some(r) = m.get("stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_string());
                }
            }
            "content_block_start" => {
                self.require_started()?;
                let index = Self::index(ev)?;
                if index != self.blocks.len() {
                    return Err(StreamError::Invalid);
                }
                let block = ev.get("content_block").filter(|b| b.is_object()).ok_or(StreamError::Invalid)?;
                block.get("type").and_then(Value::as_str).ok_or(StreamError::Invalid)?;
                self.blocks.push(block.clone());
            }
            "content_block_delta" => {
                self.require_started()?;
                let index = Self::index(ev)?;
                let delta = ev.get("delta").filter(|d| d.is_object()).ok_or(StreamError::Invalid)?;
                self.apply_delta(index, delta)?;
            }
            "content_block_stop" => {
                self.require_started()?;
                let index = Self::index(ev)?;
                if index >= self.blocks.len() {
                    return Err(StreamError::Invalid);
                }
                if let Some(json) = self.partial_json.remove(&index) {
                    let input: Value = if json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&json).map_err(|_| StreamError::Invalid)?
                    };
                    self.blocks[index]["input"] = input;
                }
            }
            "message_delta" => {
                self.require_started()?;
                if let Some(d) = ev.get("delta").and_then(Value::as_object) {
                    if let Some(r) = d.get("stop_reason").and_then(Value::as_str) {
                        self.stop_reason = Some(r.to_string());
                    }
                }
                if let Some(Value::Object(u)) = ev.get("usage") {
                    self.merge_usage(u);
                }
            }
            "message_stop" => {
                self.require_started()?;
                self.stopped = true;
            }
            // 未知事件型別要容忍（官方版本政策）。
            _ => {}
        }
        Ok(())
    }

    fn require_started(&self) -> Result<(), StreamError> {
        if self.id.is_some() { Ok(()) } else { Err(StreamError::Invalid) }
    }

    fn index(ev: &Value) -> Result<usize, StreamError> {
        ev.get("index").and_then(Value::as_u64).map(|n| n as usize).ok_or(StreamError::Invalid)
    }

    /// message_delta 的 usage 是累計值：同名欄位直接覆蓋，不存在的欄位（如 speed）沿用 message_start。
    fn merge_usage(&mut self, u: &Map<String, Value>) {
        for (k, v) in u {
            if v.is_null() && self.usage.contains_key(k) {
                continue;
            }
            self.usage.insert(k.clone(), v.clone());
        }
        let n = |k: &str| self.usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        self.progress.input_tokens = n("input_tokens");
        self.progress.output_tokens = n("output_tokens");
        self.progress.cache_read_tokens = n("cache_read_input_tokens");
        self.progress.cache_creation_tokens = n("cache_creation_input_tokens");
        self.progress.speed = self.usage.get("speed").and_then(Value::as_str).map(str::to_string);
    }

    fn apply_delta(&mut self, index: usize, delta: &Value) -> Result<(), StreamError> {
        let block = self.blocks.get_mut(index).ok_or(StreamError::Invalid)?;
        let str_of = |k: &str| delta.get(k).and_then(Value::as_str);
        match delta.get("type").and_then(Value::as_str).ok_or(StreamError::Invalid)? {
            "text_delta" => {
                let t = str_of("text").ok_or(StreamError::Invalid)?;
                append(block, "text", t)?;
                self.progress.generated_chars += t.chars().count();
                push_tail(&mut self.progress.text_tail, t);
            }
            "thinking_delta" => {
                let t = str_of("thinking").ok_or(StreamError::Invalid)?;
                append(block, "thinking", t)?;
                self.progress.generated_chars += t.chars().count();
            }
            "signature_delta" => {
                let s = str_of("signature").ok_or(StreamError::Invalid)?;
                block["signature"] = Value::String(s.to_string());
            }
            "input_json_delta" => {
                let p = str_of("partial_json").ok_or(StreamError::Invalid)?;
                self.partial_json.entry(index).or_default().push_str(p);
                self.progress.other_bytes += p.len();
            }
            "compaction_delta" => {
                // 官方串流頁沒有描述；on-demand 頁說整個區塊一次到齊，threshold 頁說單一 delta。兩種都吃。
                match delta.get("content") {
                    Some(Value::String(s)) => {
                        let cur = block.get("content").and_then(Value::as_str).unwrap_or("").to_string();
                        block["content"] = Value::String(cur + s);
                        self.progress.other_bytes += s.len();
                    }
                    Some(Value::Null) | None => {}
                    _ => return Err(StreamError::Invalid),
                }
            }
            "citations_delta" => {
                let c = delta.get("citation").ok_or(StreamError::Invalid)?.clone();
                match block.get_mut("citations") {
                    Some(Value::Array(a)) => a.push(c),
                    _ => block["citations"] = json!([c]),
                }
            }
            // 未知 delta 型別：忽略，內容以 content_block_stop／最終區塊為準。
            _ => {}
        }
        Ok(())
    }

    /// 串流結束後取最終回應；沒收到 message_stop 視為提前中斷。
    pub(crate) fn finish(mut self) -> Result<Response, StreamError> {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            self.line(&rest)?;
        }
        self.dispatch()?;
        if !self.stopped || !self.partial_json.is_empty() {
            return Err(StreamError::Invalid);
        }
        Ok(Response {
            id: self.id.ok_or(StreamError::Invalid)?,
            model: self.model.ok_or(StreamError::Invalid)?,
            content: self.blocks,
            stop_reason: self.stop_reason.ok_or(StreamError::Invalid)?,
            usage: Value::Object(self.usage),
        })
    }
}

fn append(block: &mut Value, key: &str, add: &str) -> Result<(), StreamError> {
    let cur = match block.get(key) {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        _ => return Err(StreamError::Invalid),
    };
    block[key] = Value::String(cur + add);
    Ok(())
}

fn push_tail(tail: &mut String, add: &str) {
    tail.push_str(add);
    let n = tail.chars().count();
    if n > TAIL_CHARS {
        *tail = tail.chars().skip(n - TAIL_CHARS).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(events: &[(&str, Value)]) -> Vec<u8> {
        let mut out = String::new();
        for (name, data) in events {
            out.push_str(&format!("event: {name}\ndata: {data}\n\n"));
        }
        out.into_bytes()
    }

    fn start() -> (&'static str, Value) {
        ("message_start", json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"usage":{"input_tokens":25,"output_tokens":1,"speed":"standard","service_tier":"standard"}}}))
    }
    fn stop() -> (&'static str, Value) {
        ("message_stop", json!({"type":"message_stop"}))
    }
    fn mdelta(reason: &str, out: u64) -> (&'static str, Value) {
        ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":{"output_tokens":out}}))
    }
    fn assemble(chunks: &[&[u8]]) -> Result<Response, StreamError> {
        let mut a = StreamAssembler::new();
        for c in chunks {
            a.feed(c)?;
        }
        a.finish()
    }

    #[test]
    fn text_stream_matches_nonstreaming_message() {
        let body = sse(&[
            start(),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            ("ping", json!({"type":"ping"})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" 世界"}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":0})),
            mdelta("end_turn", 15),
            stop(),
        ]);
        let r = assemble(&[&body]).unwrap();
        assert_eq!(r.id, "msg_1");
        assert_eq!(r.model, "claude-opus-5-5");
        assert_eq!(r.stop_reason, "end_turn");
        assert_eq!(r.content, vec![json!({"type":"text","text":"Hello 世界"})]);
        // message_delta 的累計 output_tokens 覆蓋 message_start；speed 等沿用。
        assert_eq!(r.usage, json!({"input_tokens":25,"output_tokens":15,"speed":"standard","service_tier":"standard"}));
    }

    #[test]
    fn arbitrary_chunk_splits_give_same_result() {
        let body = sse(&[
            start(),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"héllo 世界"}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":0})),
            mdelta("end_turn", 3),
            stop(),
        ]);
        let whole = assemble(&[&body]).unwrap();
        for size in [1usize, 2, 3, 7, 13] {
            let chunks: Vec<&[u8]> = body.chunks(size).collect();
            let r = assemble(&chunks).unwrap();
            assert_eq!(serde_json::to_value(&r).unwrap(), serde_json::to_value(&whole).unwrap(), "chunk size {size}");
        }
    }

    #[test]
    fn crlf_comments_and_missing_final_blank_line_are_accepted() {
        let mut body = String::from(": keepalive\r\n");
        for (name, data) in [start(), ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"x"}})), mdelta("end_turn", 2)] {
            body.push_str(&format!("event: {name}\r\ndata: {data}\r\n\r\n"));
        }
        body.push_str(&format!("event: message_stop\ndata: {}", stop().1));
        let r = assemble(&[body.as_bytes()]).unwrap();
        assert_eq!(r.content, vec![json!({"type":"text","text":"x"})]);
    }

    #[test]
    fn tool_use_input_json_is_assembled_across_deltas() {
        let body = sse(&[
            start(),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Running."}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash","input":{}}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"ls "}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"-la\"}"}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":1})),
            mdelta("tool_use", 40),
            stop(),
        ]);
        let r = assemble(&[&body]).unwrap();
        assert_eq!(r.stop_reason, "tool_use");
        assert_eq!(r.content[1], json!({"type":"tool_use","id":"toolu_1","name":"bash","input":{"command":"ls -la"}}));
        let calls = r.tools().unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn tool_use_without_arguments_gets_empty_object() {
        let body = sse(&[
            start(),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_2","name":"bash","input":{}}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":0})),
            mdelta("tool_use", 5),
            stop(),
        ]);
        assert_eq!(assemble(&[&body]).unwrap().content[0]["input"], json!({}));
    }

    #[test]
    fn thinking_with_signature_and_compaction_blocks() {
        let body = sse(&[
            start(),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step 1. "}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step 2."}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQB"}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"abc"}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":1})),
            ("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"compaction","content":null}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":2,"delta":{"type":"compaction_delta","content":"Summary."}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":2})),
            ("content_block_start", json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"done"}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":3})),
            ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9,"iterations":[{"type":"compaction","input_tokens":100,"output_tokens":20},{"type":"message","input_tokens":30,"output_tokens":9}]}})),
            stop(),
        ]);
        let r = assemble(&[&body]).unwrap();
        assert_eq!(r.content[0], json!({"type":"thinking","thinking":"step 1. step 2.","signature":"EqQB"}));
        assert_eq!(r.content[1], json!({"type":"redacted_thinking","data":"abc"}));
        assert_eq!(r.content[2], json!({"type":"compaction","content":"Summary."}));
        assert_eq!(r.content[3], json!({"type":"text","text":"done"}));
        assert_eq!(r.usage["iterations"][0]["type"], "compaction");
        assert_eq!(r.usage["speed"], "standard");
    }

    #[test]
    fn whole_compaction_block_without_delta_is_kept() {
        let body = sse(&[
            start(),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"compaction","content":"Whole.","signature":"s"}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":0})),
            mdelta("end_turn", 4),
            stop(),
        ]);
        assert_eq!(assemble(&[&body]).unwrap().content[0], json!({"type":"compaction","content":"Whole.","signature":"s"}));
    }

    #[test]
    fn error_event_mid_stream_is_reported_with_kind() {
        let body = sse(&[
            start(),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            ("error", json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})),
        ]);
        let err = assemble(&[&body]).unwrap_err();
        assert_eq!(err, StreamError::Api { kind: "overloaded_error".into(), message: "Overloaded".into() });
    }

    #[test]
    fn truncated_or_malformed_streams_are_invalid() {
        // 沒有 message_stop
        let body = sse(&[start(), mdelta("end_turn", 1)]);
        assert_eq!(assemble(&[&body]).unwrap_err(), StreamError::Invalid);
        // 沒有 message_start
        let body = sse(&[("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}))]);
        assert_eq!(assemble(&[&body]).unwrap_err(), StreamError::Invalid);
        // 壞掉的 JSON
        assert_eq!(assemble(&[b"data: {not json}\n\n"]).unwrap_err(), StreamError::Invalid);
        // index 跳號
        let body = sse(&[start(), ("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}))]);
        assert_eq!(assemble(&[&body]).unwrap_err(), StreamError::Invalid);
        // tool 參數 JSON 壞掉
        let body = sse(&[
            start(),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"bash","input":{}}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":0})),
        ]);
        assert_eq!(assemble(&[&body]).unwrap_err(), StreamError::Invalid);
        // message_stop 之後還有事件
        let body = sse(&[start(), mdelta("end_turn", 1), stop(), ("ping", json!({"type":"ping"}))]);
        assert_eq!(assemble(&[&body]).unwrap_err(), StreamError::Invalid);
    }

    #[test]
    fn unknown_events_and_deltas_are_ignored() {
        let body = sse(&[
            start(),
            ("future_event", json!({"type":"future_event","x":1})),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"future_delta","y":2}})),
            ("content_block_stop", json!({"type":"content_block_stop","index":0})),
            mdelta("end_turn", 1),
            stop(),
        ]);
        assert_eq!(assemble(&[&body]).unwrap().content, vec![json!({"type":"text","text":""})]);
    }

    #[test]
    fn progress_tracks_tail_chars_and_reported_usage() {
        let mut a = StreamAssembler::new();
        a.feed(&sse(&[
            start(),
            ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a".repeat(TAIL_CHARS + 50)}})),
            ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"END"}})),
        ])).unwrap();
        let p = a.progress();
        assert_eq!(p.input_tokens, 25);
        assert_eq!(p.generated_chars, TAIL_CHARS + 53);
        assert_eq!(p.text_tail.chars().count(), TAIL_CHARS);
        assert!(p.text_tail.ends_with("aEND"));
    }
}
