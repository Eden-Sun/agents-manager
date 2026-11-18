//! 切片 A（provider 接點）的相容性基準：同一份「舊」checkpoint（無 provider／版本標籤）在抽接點前後，
//! 產生位元組相同的 Claude request body、相同 hash、相同工具結果 transcript 與相同價格。
//! 期望值是在抽接點**之前**的程式（origin/main 4f3575ac6）上跑出來寫死的；之後不准為了讓測試過而改它們。
use super::pricing::DirectGlobal;
use super::protocol::{Message, Options, Response, ToolCall};
use super::quote::{QuoteProvider, RequestEnvelope};
use super::runner::{self, Halt, Journal, Model, State, Tools};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// 舊格式 checkpoint：沒有 provider／payload 版本欄位，含 thinking、tool_use、tool_result 與一次完整 response。
fn legacy_checkpoint() -> Value {
    json!({
        "messages": [
            {"role": "user", "content": "check the repo"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan", "signature": "sig-1"},
                {"type": "text", "text": "looking"},
                {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.txt\nb.txt", "is_error": false}
            ]}
        ],
        "responses": [{
            "id": "msg_1", "model": crate::config::API_BOT_MODEL,
            "content": [
                {"type": "thinking", "thinking": "plan", "signature": "sig-1"},
                {"type": "text", "text": "looking"},
                {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 20, "speed": "standard",
                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
        }],
        "api_pending": false,
        "tool_pending": null,
        "completed_tool_ids": ["toolu_1"],
        "system": "be brief"
    })
}

fn options() -> Options {
    Options { max_tokens: 4096, ..Options::default() }
}

fn envelope(state: &State) -> RequestEnvelope {
    let system = Value::String(state.system.clone().unwrap());
    RequestEnvelope::new(options(), system, state.messages.clone()).unwrap()
}

struct Script(Mutex<Vec<Response>>);
impl Model for Script {
    fn complete<'a>(&'a self, _: &'a [Message], _: &'a CancellationToken) -> BoxFuture<'a, Result<Response, super::http::Error>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().remove(0)) })
    }
}
struct EchoTools;
impl Tools for EchoTools {
    fn execute<'a>(&'a self, call: &'a ToolCall, _: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move { if call.id == "toolu_b" { Err("boom".into()) } else { Ok(format!("ok:{}", call.id)) } })
    }
}
struct Mem;
impl Journal for Mem {
    fn save<'a>(&'a self, _: &'a State) -> BoxFuture<'a, Result<(), ()>> { Box::pin(async { Ok(()) }) }
}
fn response(content: Vec<Value>, reason: &str) -> Response {
    Response {
        id: "msg_2".into(),
        model: crate::config::API_BOT_MODEL.into(),
        content,
        stop_reason: reason.into(),
        usage: json!({"input_tokens": 120, "output_tokens": 30, "speed": "standard", "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}),
    }
}

#[test]
fn a_legacy_checkpoint_loads_and_encodes_to_the_golden_claude_request() {
    let state: State = serde_json::from_value(legacy_checkpoint()).unwrap();
    let env = envelope(&state);
    // body：serde_json 預設的物件鍵序（preserve_order 未開時是字母序），逐字固定。
    let body = serde_json::to_string(&env.body()).unwrap();
    assert_eq!(body, GOLDEN_BODY, "Claude request body 必須與抽接點之前位元組相同");
    assert_eq!(env.hash(), GOLDEN_HASH, "request hash");
    // 報價與結算（價格）
    let quote = DirectGlobal.reserve(&env).unwrap();
    assert_eq!((quote.reserved_microusd, quote.pricing_version.as_str()), GOLDEN_RESERVE);
    let settled = DirectGlobal.settle(&env, &state.responses[0]).unwrap();
    assert_eq!((settled.estimated_microusd, settled.actual_speed), GOLDEN_SETTLE);
}

#[tokio::test]
async fn running_from_the_legacy_checkpoint_produces_the_golden_tool_transcript() {
    let mut state: State = serde_json::from_value(legacy_checkpoint()).unwrap();
    // 接著再來一輪：兩個工具（一成功一失敗），最後回文字。
    let model = Script(Mutex::new(vec![
        response(
            vec![
                json!({"type": "tool_use", "id": "toolu_a", "name": "bash", "input": {"command": "pwd"}}),
                json!({"type": "tool_use", "id": "toolu_b", "name": "bash", "input": {"command": "false"}}),
            ],
            "tool_use",
        ),
        response(vec![json!({"type": "text", "text": "done"})], "end_turn"),
    ]));
    let halt = runner::run(&model, &EchoTools, &Mem, &mut state, &CancellationToken::new(), 8).await.unwrap();
    assert_eq!(halt, Halt::EndTurn);
    let messages = serde_json::to_string(&state.messages).unwrap();
    assert_eq!(messages, GOLDEN_MESSAGES, "工具結果 transcript 必須與抽接點之前位元組相同");
    // 最後一份送出去的 body（第三次請求的內容）也固定。
    let last = envelope(&State { messages: state.messages[..state.messages.len() - 1].to_vec(), ..state.clone() });
    assert_eq!(last.hash(), GOLDEN_LAST_HASH);
}

fn notice_state() -> State {
    State { messages: vec![Message { role: "user".into(), content: json!("go") }], ..Default::default() }
}
fn notice_model() -> Script {
    Script(Mutex::new(vec![
        response(vec![json!({"type": "tool_use", "id": "toolu_n", "name": "bash", "input": {"command": "true"}})], "tool_use"),
        response(vec![json!({"type": "text", "text": "ok"})], "end_turn"),
    ]))
}

#[tokio::test]
async fn the_two_calls_left_notice_keeps_its_exact_wire_shape() {
    let mut state = notice_state();
    runner::run(&notice_model(), &EchoTools, &Mem, &mut state, &CancellationToken::new(), 3).await.unwrap();
    assert_eq!(serde_json::to_string(&state.messages).unwrap(), GOLDEN_NOTICE);
}

/// 第二份舊 checkpoint：工具批次已結清、下一次請求是軟期限的收尾請求（wrap-up）。
fn legacy_wrap_up_checkpoint() -> Value {
    json!({
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "refactor the module"}]},
            {"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": "opaque-blob"},
                {"type": "tool_use", "id": "toolu_w1", "name": "str_replace_based_edit_tool", "input": {"command": "view", "path": "/src/lib.rs"}},
                {"type": "tool_use", "id": "toolu_w2", "name": "bash", "input": {"command": "cargo test"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_w1", "content": "fn main() {}", "is_error": false},
                {"type": "tool_result", "tool_use_id": "toolu_w2", "content": "not executed: time limit reached", "is_error": true}
            ]}
        ],
        "responses": [{
            "id": "msg_w1", "model": crate::config::API_BOT_MODEL,
            "content": [
                {"type": "redacted_thinking", "data": "opaque-blob"},
                {"type": "tool_use", "id": "toolu_w1", "name": "str_replace_based_edit_tool", "input": {"command": "view", "path": "/src/lib.rs"}},
                {"type": "tool_use", "id": "toolu_w2", "name": "bash", "input": {"command": "cargo test"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5000, "output_tokens": 300, "speed": "standard",
                      "cache_creation_input_tokens": 1200, "cache_read_input_tokens": 800}
        }],
        "api_pending": false,
        "tool_pending": null,
        "completed_tool_ids": ["toolu_w1", "toolu_w2"]
    })
}

/// 記下每一次送給模型的 messages（也就是那一次 request 的內容）。
struct Recording(Mutex<Vec<Response>>, Mutex<Vec<Vec<Message>>>);
impl Model for Recording {
    fn complete<'a>(&'a self, messages: &'a [Message], _: &'a CancellationToken) -> BoxFuture<'a, Result<Response, super::http::Error>> {
        Box::pin(async move {
            self.1.lock().unwrap().push(messages.to_vec());
            Ok(self.0.lock().unwrap().remove(0))
        })
    }
}

#[tokio::test]
async fn a_wrap_up_request_from_a_legacy_checkpoint_matches_the_golden_request_and_price() {
    let mut state: State = serde_json::from_value(legacy_wrap_up_checkpoint()).unwrap();
    assert!(state.system.is_none(), "這份舊 checkpoint 沒有 system 欄位");
    let model = Recording(Mutex::new(vec![response(vec![json!({"type": "text", "text": "summary: half done"})], "end_turn")]), Mutex::new(Vec::new()));
    let halt = runner::run_with_wrap_up(&model, &EchoTools, &Mem, &mut state, &CancellationToken::new(), 8, &|| false, &|| true).await.unwrap();
    assert_eq!(halt, Halt::TimeLimit);
    let sent = model.1.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "收尾只送一次請求");
    // 收尾請求：1 小時快取、fast、開 web_search 的選項組合，system 是陣列形狀。
    let opts = Options { max_tokens: 2048, effort: "medium".into(), fast: true, cache_ttl_1h: true, web_search: true, ..Options::default() };
    let system = json!([{"type": "text", "text": "You are AG Man's API bot."}]);
    let env = RequestEnvelope::new(opts, system, sent[0].clone()).unwrap();
    assert_eq!(serde_json::to_string(&env.body()).unwrap(), GOLDEN_WRAP_BODY, "收尾請求的 body 必須位元組相同");
    assert_eq!(env.hash(), GOLDEN_WRAP_HASH);
    assert_eq!(env.speed(), "fast");
    let quote = DirectGlobal.reserve(&env).unwrap();
    assert_eq!((quote.reserved_microusd, quote.pricing_version.as_str()), GOLDEN_WRAP_RESERVE);
    let settled = DirectGlobal.settle(&env, state.responses.last().unwrap()).unwrap();
    assert_eq!((settled.estimated_microusd, settled.actual_speed), GOLDEN_WRAP_SETTLE);
    assert_eq!(serde_json::to_string(&state.messages).unwrap(), GOLDEN_WRAP_MESSAGES, "收尾後的 transcript");
    // checkpoint 本身（State 的序列化）也固定：舊格式讀進來再存，欄位與順序不變。
    let mut saved = serde_json::to_value(&state).unwrap();
    saved["completed_tool_ids"].as_array_mut().unwrap().sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    assert_eq!(serde_json::to_string(&saved).unwrap(), GOLDEN_WRAP_CHECKPOINT);
}

const GOLDEN_WRAP_BODY: &str = r##"{"max_tokens":2048,"messages":[{"content":[{"text":"refactor the module","type":"text"}],"role":"user"},{"content":[{"data":"opaque-blob","type":"redacted_thinking"},{"id":"toolu_w1","input":{"command":"view","path":"/src/lib.rs"},"name":"str_replace_based_edit_tool","type":"tool_use"},{"cache_control":{"ttl":"1h","type":"ephemeral"},"id":"toolu_w2","input":{"command":"cargo test"},"name":"bash","type":"tool_use"}],"role":"assistant"},{"content":[{"content":"fn main() {}","is_error":false,"tool_use_id":"toolu_w1","type":"tool_result"},{"content":"not executed: time limit reached","is_error":true,"tool_use_id":"toolu_w2","type":"tool_result"},{"cache_control":{"ttl":"1h","type":"ephemeral"},"text":"[AG Man] The time limit for this turn has been reached. Do NOT start any new action or tool call. Reply with a summary: what is done, what is not done, and how to continue.","type":"text"}],"role":"user"}],"model":"claude-opus-5-5","output_config":{"effort":"medium"},"speed":"fast","stream":true,"system":[{"cache_control":{"ttl":"1h","type":"ephemeral"},"text":"You are AG Man's API bot.","type":"text"}],"thinking":{"block_binding":{"prefix_mismatch_behavior":"drop_block"},"type":"adaptive"},"tool_choice":{"type":"auto"},"tools":[{"name":"bash","type":"bash_20250124"},{"name":"str_replace_based_edit_tool","type":"text_editor_20250728"},{"name":"web_search","type":"web_search_20260209"}]}"##;
const GOLDEN_WRAP_HASH: &str = "4cb2dcc10cce165d8020c43fd80d52f82a517a497b174168bc164dd11b67e27d";
const GOLDEN_WRAP_RESERVE: (i64, &str) = (285424, "claude-opus-5-5-global-2026-11-14");
const GOLDEN_WRAP_SETTLE: (i64, Option<&str>) = (1080, Some("standard"));
const GOLDEN_WRAP_MESSAGES: &str = r##"[{"role":"user","content":[{"text":"refactor the module","type":"text"}]},{"role":"assistant","content":[{"data":"opaque-blob","type":"redacted_thinking"},{"id":"toolu_w1","input":{"command":"view","path":"/src/lib.rs"},"name":"str_replace_based_edit_tool","type":"tool_use"},{"id":"toolu_w2","input":{"command":"cargo test"},"name":"bash","type":"tool_use"}]},{"role":"user","content":[{"content":"fn main() {}","is_error":false,"tool_use_id":"toolu_w1","type":"tool_result"},{"content":"not executed: time limit reached","is_error":true,"tool_use_id":"toolu_w2","type":"tool_result"},{"text":"[AG Man] The time limit for this turn has been reached. Do NOT start any new action or tool call. Reply with a summary: what is done, what is not done, and how to continue.","type":"text"}]},{"role":"assistant","content":[{"text":"summary: half done","type":"text"}]}]"##;
const GOLDEN_WRAP_CHECKPOINT: &str = r##"{"api_pending":false,"completed_tool_ids":["toolu_w1","toolu_w2"],"messages":[{"content":[{"text":"refactor the module","type":"text"}],"role":"user"},{"content":[{"data":"opaque-blob","type":"redacted_thinking"},{"id":"toolu_w1","input":{"command":"view","path":"/src/lib.rs"},"name":"str_replace_based_edit_tool","type":"tool_use"},{"id":"toolu_w2","input":{"command":"cargo test"},"name":"bash","type":"tool_use"}],"role":"assistant"},{"content":[{"content":"fn main() {}","is_error":false,"tool_use_id":"toolu_w1","type":"tool_result"},{"content":"not executed: time limit reached","is_error":true,"tool_use_id":"toolu_w2","type":"tool_result"},{"text":"[AG Man] The time limit for this turn has been reached. Do NOT start any new action or tool call. Reply with a summary: what is done, what is not done, and how to continue.","type":"text"}],"role":"user"},{"content":[{"text":"summary: half done","type":"text"}],"role":"assistant"}],"responses":[{"content":[{"data":"opaque-blob","type":"redacted_thinking"},{"id":"toolu_w1","input":{"command":"view","path":"/src/lib.rs"},"name":"str_replace_based_edit_tool","type":"tool_use"},{"id":"toolu_w2","input":{"command":"cargo test"},"name":"bash","type":"tool_use"}],"id":"msg_w1","model":"claude-opus-5-5","stop_reason":"tool_use","usage":{"cache_creation_input_tokens":1200,"cache_read_input_tokens":800,"input_tokens":5000,"output_tokens":300,"speed":"standard"}},{"content":[{"text":"summary: half done","type":"text"}],"id":"msg_2","model":"claude-opus-5-5","stop_reason":"end_turn","usage":{"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"input_tokens":120,"output_tokens":30,"speed":"standard"}}],"tool_pending":null}"##;

const GOLDEN_BODY: &str = r##"{"max_tokens":4096,"messages":[{"content":"check the repo","role":"user"},{"content":[{"signature":"sig-1","thinking":"plan","type":"thinking"},{"text":"looking","type":"text"},{"cache_control":{"type":"ephemeral"},"id":"toolu_1","input":{"command":"ls"},"name":"bash","type":"tool_use"}],"role":"assistant"},{"content":[{"cache_control":{"type":"ephemeral"},"content":"a.txt\nb.txt","is_error":false,"tool_use_id":"toolu_1","type":"tool_result"}],"role":"user"}],"model":"claude-opus-5-5","output_config":{"effort":"high"},"stream":true,"system":[{"cache_control":{"type":"ephemeral"},"text":"be brief","type":"text"}],"thinking":{"block_binding":{"prefix_mismatch_behavior":"drop_block"},"type":"adaptive"},"tool_choice":{"type":"auto"},"tools":[{"name":"bash","type":"bash_20250124"},{"name":"str_replace_based_edit_tool","type":"text_editor_20250728"}]}"##;
const GOLDEN_HASH: &str = "df06ad30f92e56547fb527d11768424664b304c2462d3eb10302424037f75861";
const GOLDEN_RESERVE: (i64, &str) = (127165, "claude-opus-5-5-global-2026-11-14");
const GOLDEN_SETTLE: (i64, Option<&str>) = (800, Some("standard"));
const GOLDEN_MESSAGES: &str = r##"[{"role":"user","content":"check the repo"},{"role":"assistant","content":[{"signature":"sig-1","thinking":"plan","type":"thinking"},{"text":"looking","type":"text"},{"id":"toolu_1","input":{"command":"ls"},"name":"bash","type":"tool_use"}]},{"role":"user","content":[{"content":"a.txt\nb.txt","is_error":false,"tool_use_id":"toolu_1","type":"tool_result"}]},{"role":"assistant","content":[{"id":"toolu_a","input":{"command":"pwd"},"name":"bash","type":"tool_use"},{"id":"toolu_b","input":{"command":"false"},"name":"bash","type":"tool_use"}]},{"role":"user","content":[{"content":"ok:toolu_a","is_error":false,"tool_use_id":"toolu_a","type":"tool_result"},{"content":"boom","is_error":true,"tool_use_id":"toolu_b","type":"tool_result"}]},{"role":"assistant","content":[{"text":"done","type":"text"}]}]"##;
const GOLDEN_LAST_HASH: &str = "48515e5ecb21ad70c5b0323c8c53732d86c6c2f0e3e92dbd89f0fb9c303f2ace";
const GOLDEN_NOTICE: &str = r##"[{"role":"user","content":"go"},{"role":"assistant","content":[{"id":"toolu_n","input":{"command":"true"},"name":"bash","type":"tool_use"}]},{"role":"user","content":[{"content":"ok:toolu_n","is_error":false,"tool_use_id":"toolu_n","type":"tool_result"},{"text":"[AG Man] You have 2 API calls left in this turn. Stop starting new work: write a progress summary (what is done, findings so far, what remains and how to continue) as your reply.","type":"text"}]},{"role":"assistant","content":[{"text":"ok","type":"text"}]}]"##;
