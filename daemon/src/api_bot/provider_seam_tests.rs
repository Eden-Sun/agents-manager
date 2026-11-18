//! 切片 A（provider 接點）的相容性基準：同一份「舊」checkpoint（無 provider／版本標籤）在抽接點前後，
//! 產生位元組相同的 Claude request body、相同 hash、相同工具結果 transcript 與相同價格。
//! 期望值是在抽接點**之前**的程式上跑出來寫死的；之後不准為了讓測試過而改它們。
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

/// 經 provider 接點編碼（`with_provider`）與預設路徑（`new`）逐位元組相同；報價與 hash 也相同。
#[test]
fn the_provider_path_encodes_the_same_bytes_hash_and_price() {
    use super::provider::AnthropicProvider;
    let state: State = serde_json::from_value(legacy_checkpoint()).unwrap();
    let provider = AnthropicProvider::new(std::sync::Arc::new(DirectGlobal));
    let system = Value::String(state.system.clone().unwrap());
    let env = RequestEnvelope::with_provider(&provider, options(), system, state.messages.clone()).unwrap();
    assert_eq!(serde_json::to_string(env.body_ref()).unwrap(), GOLDEN_BODY);
    assert_eq!(env.hash(), GOLDEN_HASH);
    // 報價、hash、實際送出的 body 是同一份編碼：hash 就是這份 body 的 canonical sha256。
    let q = super::provider::ApiProvider::quote(&provider);
    let quote = q.reserve(&env).unwrap();
    assert_eq!((quote.reserved_microusd, quote.pricing_version.as_str()), GOLDEN_RESERVE);
    let settled = q.settle(&env, &state.responses[0]).unwrap();
    assert_eq!((settled.estimated_microusd, settled.actual_speed), GOLDEN_SETTLE);
}

/// 舊 checkpoint（無標籤）原樣序列化不多出欄位；補標籤後才有，而且 `Checkpoint::new` 兩種都收。
#[test]
fn legacy_checkpoints_stay_untagged_until_stamped_and_both_shapes_validate() {
    use super::provider::{check_checkpoint_tag, stamp_checkpoint, AnthropicProvider};
    let legacy = legacy_checkpoint();
    let mut state: State = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(&state).unwrap(), legacy, "沒有標籤的 checkpoint 讀進來再寫出去不多不少");
    let provider = AnthropicProvider::codec_only();
    check_checkpoint_tag(&state, &provider).unwrap();
    stamp_checkpoint(&mut state, &provider);
    let stamped = serde_json::to_value(&state).unwrap();
    assert_eq!(stamped["provider"], "anthropic");
    assert_eq!(stamped["payload_version"], 1);
    am_base::db::api_bot::Checkpoint::new(&legacy).unwrap();
    am_base::db::api_bot::Checkpoint::new(&stamped).unwrap();
    for bad in [json!({"provider": "openai"}), json!({"payload_version": 0}), json!({"payload_version": 2})] {
        let mut v = legacy.clone();
        v.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
        let st: State = serde_json::from_value(v).unwrap();
        assert!(check_checkpoint_tag(&st, &provider).is_err(), "{:?}", st.provider);
    }
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

const GOLDEN_BODY: &str = r##"{"max_tokens":4096,"messages":[{"content":"check the repo","role":"user"},{"content":[{"signature":"sig-1","thinking":"plan","type":"thinking"},{"text":"looking","type":"text"},{"cache_control":{"type":"ephemeral"},"id":"toolu_1","input":{"command":"ls"},"name":"bash","type":"tool_use"}],"role":"assistant"},{"content":[{"cache_control":{"type":"ephemeral"},"content":"a.txt\nb.txt","is_error":false,"tool_use_id":"toolu_1","type":"tool_result"}],"role":"user"}],"model":"claude-opus-5-5","output_config":{"effort":"high"},"stream":true,"system":[{"cache_control":{"type":"ephemeral"},"text":"be brief","type":"text"}],"thinking":{"block_binding":{"prefix_mismatch_behavior":"drop_block"},"type":"adaptive"},"tool_choice":{"type":"auto"},"tools":[{"name":"bash","type":"bash_20250124"},{"name":"str_replace_based_edit_tool","type":"text_editor_20250728"}]}"##;
const GOLDEN_HASH: &str = "df06ad30f92e56547fb527d11768424664b304c2462d3eb10302424037f75861";
const GOLDEN_RESERVE: (i64, &str) = (127165, "claude-opus-5-5-global-2026-11-14");
const GOLDEN_SETTLE: (i64, Option<&str>) = (800, Some("standard"));
const GOLDEN_MESSAGES: &str = r##"[{"role":"user","content":"check the repo"},{"role":"assistant","content":[{"signature":"sig-1","thinking":"plan","type":"thinking"},{"text":"looking","type":"text"},{"id":"toolu_1","input":{"command":"ls"},"name":"bash","type":"tool_use"}]},{"role":"user","content":[{"content":"a.txt\nb.txt","is_error":false,"tool_use_id":"toolu_1","type":"tool_result"}]},{"role":"assistant","content":[{"id":"toolu_a","input":{"command":"pwd"},"name":"bash","type":"tool_use"},{"id":"toolu_b","input":{"command":"false"},"name":"bash","type":"tool_use"}]},{"role":"user","content":[{"content":"ok:toolu_a","is_error":false,"tool_use_id":"toolu_a","type":"tool_result"},{"content":"boom","is_error":true,"tool_use_id":"toolu_b","type":"tool_result"}]},{"role":"assistant","content":[{"text":"done","type":"text"}]}]"##;
const GOLDEN_LAST_HASH: &str = "48515e5ecb21ad70c5b0323c8c53732d86c6c2f0e3e92dbd89f0fb9c303f2ace";
const GOLDEN_NOTICE: &str = r##"[{"role":"user","content":"go"},{"role":"assistant","content":[{"id":"toolu_n","input":{"command":"true"},"name":"bash","type":"tool_use"}]},{"role":"user","content":[{"content":"ok:toolu_n","is_error":false,"tool_use_id":"toolu_n","type":"tool_result"},{"text":"[AG Man] You have 2 API calls left in this turn. Stop starting new work: write a progress summary (what is done, findings so far, what remains and how to continue) as your reply.","type":"text"}]},{"role":"assistant","content":[{"text":"ok","type":"text"}]}]"##;
