//! `OpenAiProvider` 接在 provider 接點上的整回合測試：假的 loopback SSE server，不打真的 API、不用真的 key。
use super::provider::{OpenAiCodec, OpenAiProvider, OpenAiQuote, OpenAiRequest, ID, OFFICIAL_ENDPOINT};
use crate::api_bot::http;
use crate::api_bot::protocol::{Message, Options, Response, ToolCall};
use crate::api_bot::provider::{self, Codec, Provider, ProviderExtras, SessionOptions, Transport};
use crate::api_bot::quote::{QuoteError, QuoteProvider, QuotedRequest, RequestEnvelope};
use crate::api_bot::runner::{self, Halt, Journal, Model, State, Tools};
use axum::http::{HeaderMap, StatusCode};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const FAKE_KEY: &str = "sk-test-FAKE-not-a-real-key";
const ENC: &str = "ENC-SECRET-reasoning-blob";

// ───────────────────────────── 假 server ─────────────────────────────

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}
impl Reply {
    fn sse(body: Vec<u8>) -> Self {
        Self { status: 200, headers: vec![("content-type", "text/event-stream".into())], body }
    }
    fn json(status: u16, body: Value) -> Self {
        Self { status, headers: vec![("content-type", "application/json".into())], body: body.to_string().into_bytes() }
    }
}

#[derive(Default)]
struct Seen {
    bodies: Vec<Vec<u8>>,
    auth: Vec<String>,
}

struct Server {
    endpoint: String,
    seen: Arc<Mutex<Seen>>,
    stop: CancellationToken,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// 每收到一個 POST 就照順序回一份腳本；腳本用完回 500。
async fn serve(replies: Vec<Reply>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}/v1/responses", listener.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Seen::default()));
    let script = Arc::new(Mutex::new(replies.into_iter().collect::<std::collections::VecDeque<_>>()));
    let (seen_in, stop) = (seen.clone(), CancellationToken::new());
    let app = axum::Router::new().route(
        "/v1/responses",
        axum::routing::post(move |headers: HeaderMap, body: axum::body::Bytes| {
            let (seen, script) = (seen_in.clone(), script.clone());
            async move {
                {
                    let mut seen = seen.lock().unwrap();
                    seen.bodies.push(body.to_vec());
                    seen.auth.push(headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
                }
                let reply = script.lock().unwrap().pop_front().unwrap_or(Reply::json(500, json!({"error": {"code": "server_error"}})));
                let mut response = axum::response::Response::builder().status(StatusCode::from_u16(reply.status).unwrap());
                for (name, value) in reply.headers {
                    response = response.header(name, value);
                }
                response.body(axum::body::Body::from(reply.body)).unwrap()
            }
        }),
    );
    let child = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).with_graceful_shutdown(async move { child.cancelled().await }).await;
    });
    Server { endpoint, seen, stop }
}

// ───────────────────────────── SSE 腳本 ─────────────────────────────

fn sse(events: &[Value]) -> Vec<u8> {
    let mut out = String::new();
    for e in events {
        out.push_str(&format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()));
    }
    out.into_bytes()
}

fn usage(input: u64, cached: u64, write: u64, output: u64, reasoning: u64) -> Value {
    json!({"input_tokens": input, "input_tokens_details": {"cached_tokens": cached, "cache_write_tokens": write},
           "output_tokens": output, "output_tokens_details": {"reasoning_tokens": reasoning}, "total_tokens": input + output})
}
fn reasoning_item() -> Value {
    json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": ENC})
}
fn message_item(text: &str) -> Value {
    json!({"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]})
}
fn call_item(id: &str, call_id: &str, name: &str, args: &str) -> Value {
    json!({"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": args, "status": "completed"})
}

const ARGS_A: &str = r#"{"command":"ls"}"#;
const ARGS_B: &str = r#"{"command":"view","path":"/work/a.txt"}"#;

/// 第一次回應：加密 reasoning ＋ 同一個 response 兩個工具呼叫，參數交錯分片（A1 B1 A2 B2）。
fn two_tools_stream(usage_value: Value) -> Vec<u8> {
    let out = vec![reasoning_item(), call_item("fc_a", "call_A", "bash", ARGS_A), call_item("fc_b", "call_B", "str_replace_based_edit_tool", ARGS_B)];
    sse(&[
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item()}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "fc_a", "call_id": "call_A", "name": "bash", "arguments": ""}}),
        json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "id": "fc_b", "call_id": "call_B", "name": "str_replace_based_edit_tool", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": &ARGS_A[..9]}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": &ARGS_B[..14]}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": &ARGS_A[9..]}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": &ARGS_B[14..]}),
        json!({"type": "response.function_call_arguments.done", "output_index": 1, "arguments": ARGS_A}),
        json!({"type": "response.function_call_arguments.done", "output_index": 2, "arguments": ARGS_B}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": call_item("fc_a", "call_A", "bash", ARGS_A)}),
        json!({"type": "response.output_item.done", "output_index": 2, "item": call_item("fc_b", "call_B", "str_replace_based_edit_tool", ARGS_B)}),
        json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "output": out, "usage": usage_value}}),
    ])
}

fn final_text_stream(text: &str) -> Vec<u8> {
    let msg = message_item(text);
    sse(&[
        json!({"type": "response.created", "response": {"id": "resp_2"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": text}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": msg.clone()}),
        json!({"type": "response.completed", "response": {"id": "resp_2", "status": "completed", "output": [msg], "usage": usage(900, 300, 100, 30, 0)}}),
    ])
}

// ───────────────────────────── runner 的三個接點 ─────────────────────────────

fn luna_options() -> SessionOptions {
    SessionOptions { model: "gpt-6-luna".into(), effort: "medium".into(), max_output_tokens: 4096, extras: ProviderExtras::None }
}
fn luna() -> Arc<OpenAiProvider> {
    Arc::new(OpenAiProvider::with_standard_pricing(luna_options()).expect("approved model"))
}
fn transport(endpoint: &str) -> Transport<'_> {
    Transport { endpoint, key: FAKE_KEY, timeout: Duration::from_secs(5), max_response_bytes: 1024 * 1024 }
}

/// runner 的 `Model`：每次都從完整 transcript 編碼，再送出同一份；記下每次送出的位元組與已編碼請求。
struct Wire {
    provider: Arc<OpenAiProvider>,
    endpoint: String,
    encoded: Mutex<Vec<Arc<dyn QuotedRequest>>>,
}
impl Wire {
    fn new(provider: Arc<OpenAiProvider>, endpoint: &str) -> Self {
        Self { provider, endpoint: endpoint.to_string(), encoded: Mutex::new(Vec::new()) }
    }
    fn sent(&self) -> Vec<Vec<u8>> {
        self.encoded.lock().unwrap().iter().map(|r| r.as_any().downcast_ref::<OpenAiRequest>().unwrap().wire().to_vec()).collect()
    }
}
impl Model for Wire {
    fn complete<'a>(&'a self, messages: &'a [Message], cancel: &'a CancellationToken) -> BoxFuture<'a, Result<Response, http::Error>> {
        Box::pin(async move {
            let request = self.provider.encode(self.provider.effort(), &json!("You are a careful assistant."), messages, None).map_err(|_| http::Error::InvalidRequest)?;
            self.encoded.lock().unwrap().push(request.clone());
            self.provider.send(transport(&self.endpoint), request.as_ref(), cancel, None).await
        })
    }
}

struct EchoTools(Mutex<Vec<ToolCall>>);
impl Tools for EchoTools {
    fn execute<'a>(&'a self, call: &'a ToolCall, _: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(call.clone());
            if call.id == "call_B" {
                Err("no such file".into())
            } else {
                Ok("a.txt\nb.txt".into())
            }
        })
    }
}

/// 每一份 checkpoint 都以 JSON 存下來（跟真的 journal 一樣是序列化後的 state）。
#[derive(Default)]
struct Snapshots(Mutex<Vec<String>>);
impl Journal for Snapshots {
    fn save<'a>(&'a self, state: &'a State) -> BoxFuture<'a, Result<(), ()>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(serde_json::to_string(state).map_err(|_| ())?);
            Ok(())
        })
    }
}

fn fresh_state() -> State {
    State {
        messages: vec![Message { role: "user".into(), content: json!("list the workspace and show a.txt") }],
        v: Some(2),
        provider: Some(ID.into()),
        ..Default::default()
    }
}

async fn run(wire: &Wire, tools: &EchoTools, journal: &Snapshots, state: &mut State) -> Result<Halt, runner::Error> {
    runner::run_with_codec(wire.provider.codec(), wire, tools, journal, state, &CancellationToken::new(), 8, &|| false, &|| false).await
}

/// 一整個回合（兩個工具 → 工具結果 → 最終文字）。回傳（server 收到的 body、每次送出的位元組、最後的 state、checkpoint 們）。
async fn full_turn() -> (Vec<Value>, Vec<Vec<u8>>, State, Vec<String>, Vec<ToolCall>) {
    let server = serve(vec![Reply::sse(two_tools_stream(usage(1200, 200, 100, 80, 30))), Reply::sse(final_text_stream("workspace has two files"))]).await;
    let wire = Wire::new(luna(), &server.endpoint);
    let (tools, journal) = (EchoTools(Mutex::new(Vec::new())), Snapshots::default());
    let mut state = fresh_state();
    assert_eq!(run(&wire, &tools, &journal, &mut state).await.unwrap(), Halt::EndTurn);
    let seen = server.seen.lock().unwrap();
    assert!(seen.auth.iter().all(|a| a == &format!("Bearer {FAKE_KEY}")), "the injected key is sent as a bearer token");
    let bodies = seen.bodies.iter().map(|b| serde_json::from_slice(b).unwrap()).collect();
    assert_eq!(seen.bodies, wire.sent(), "the bytes on the wire are the encoded request, unchanged");
    let executed = tools.0.lock().unwrap().clone();
    let snapshots = journal.0.lock().unwrap().clone();
    (bodies, wire.sent(), state, snapshots, executed)
}

// ───────────────────────────── 測試 ─────────────────────────────

#[tokio::test]
async fn a_turn_with_two_interleaved_tool_calls_sends_the_results_back_and_ends_with_text() {
    let (bodies, _, state, _, executed) = full_turn().await;
    assert_eq!(bodies.len(), 2);

    // 兩個工具照回應的順序執行，參數是組起來的完整 JSON，id 是 call_id（不是 output item 的 id）。
    assert_eq!(executed.iter().map(|c| (c.id.as_str(), c.name.as_str())).collect::<Vec<_>>(), [("call_A", "bash"), ("call_B", "str_replace_based_edit_tool")]);
    assert_eq!(executed[0].input, json!({"command": "ls"}));
    assert_eq!(executed[1].input, json!({"command": "view", "path": "/work/a.txt"}));

    // 第一次請求：無狀態、串流、要求回傳加密 reasoning、宣告兩個工具。
    let first = &bodies[0];
    assert_eq!((first["store"].clone(), first["stream"].clone(), first["model"].clone()), (json!(false), json!(true), json!("gpt-6-luna")));
    assert_eq!(first["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(first["reasoning"]["effort"], "medium");
    assert_eq!(first["instructions"], "You are a careful assistant.");
    assert_eq!(first["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect::<Vec<_>>(), ["bash", "str_replace_based_edit_tool"]);
    assert!(first.get("previous_response_id").is_none());
    assert_eq!(first["input"].as_array().unwrap().len(), 1);

    // 第二次請求：完整重播（user → 加密 reasoning 原樣 → 兩個 function_call 原樣），再接兩個用 call_id 配對的工具結果。
    let input = bodies[1]["input"].as_array().unwrap();
    assert_eq!(input.iter().map(|i| i["type"].as_str().unwrap()).collect::<Vec<_>>(), ["message", "reasoning", "function_call", "function_call", "function_call_output", "function_call_output"]);
    assert_eq!(input[1], reasoning_item(), "the reasoning item is replayed byte-for-byte, encrypted content included");
    assert_eq!((input[2]["call_id"].as_str(), input[3]["call_id"].as_str()), (Some("call_A"), Some("call_B")));
    assert_eq!(input[4], json!({"type": "function_call_output", "call_id": "call_A", "output": "a.txt\nb.txt"}));
    assert_eq!(input[5], json!({"type": "function_call_output", "call_id": "call_B", "output": "no such file"}));

    // 最終文字是可見文字；transcript 裡的工具結果保留 is_error。
    assert_eq!(state.responses.len(), 2);
    assert_eq!(OpenAiCodec.visible_text(state.responses.last().unwrap()), "workspace has two files");
    let batch = state.messages.iter().find(|m| m.role == "tool_results").unwrap();
    assert_eq!(batch.content["results"][1], json!({"call_id": "call_B", "output": "no such file", "is_error": true}));
    assert!(state.completed_tool_ids.contains("call_A") && state.completed_tool_ids.contains("call_B"));
    assert!(state.tool_pending.is_none() && !state.api_pending);
}

/// 中斷後從 checkpoint 接續：把「工具結果都落盤了」的那份 checkpoint 讀回來（新的 provider、新的 server），
/// 下一次請求的位元組與沒中斷時的第二次請求完全相同，回合照常結束。
#[tokio::test]
async fn resuming_from_a_checkpoint_sends_the_same_bytes_as_the_uninterrupted_turn() {
    let (_, sent, finished, snapshots, _) = full_turn().await;
    // 工具批次結清、還沒開始下一次模型呼叫的那一份（之後 daemon 掛掉）。
    let durable = snapshots
        .iter()
        .rev()
        .map(|raw| serde_json::from_str::<State>(raw).unwrap())
        .find(|s| !s.api_pending && s.tool_pending.is_none() && s.responses.len() == 1 && s.messages.last().is_some_and(|m| m.role == "tool_results"))
        .expect("a settled checkpoint between the two model calls");
    let mut state = durable;
    assert_eq!((state.v, state.provider_id()), (Some(2), ID), "the checkpoint carries the session's provider");

    let server = serve(vec![Reply::sse(final_text_stream("workspace has two files"))]).await;
    let provider = provider::for_id(state.provider_id(), luna_options(), Arc::new(OpenAiQuote)).expect("the session's provider");
    assert!(provider.checkpoint_matches_model(&state));
    let wire = Wire::new(luna(), &server.endpoint);
    let (tools, journal) = (EchoTools(Mutex::new(Vec::new())), Snapshots::default());
    assert_eq!(run(&wire, &tools, &journal, &mut state).await.unwrap(), Halt::EndTurn);

    assert!(tools.0.lock().unwrap().is_empty(), "tools that already completed are never run again");
    assert_eq!(wire.sent(), vec![sent[1].clone()], "the resumed request is byte-identical to the uninterrupted second request");
    assert_eq!(serde_json::to_string(&state.messages).unwrap(), serde_json::to_string(&finished.messages).unwrap());
    // 同一份 transcript 再編一次，hash 也相同（報價、送出、核對用的是同一份）。
    let again = wire.provider.encode("medium", &json!("You are a careful assistant."), &finished.messages[..finished.messages.len() - 1], None).unwrap();
    assert_eq!(again.hash(), wire.encoded.lock().unwrap()[0].hash());
}

#[tokio::test]
async fn a_checkpoint_or_request_of_another_provider_is_refused_without_any_network_call() {
    let server = serve(vec![]).await;
    let quote: Arc<dyn QuoteProvider> = Arc::new(OpenAiQuote);
    // 選項不屬於這個 provider：建不出來。
    assert!(provider::for_id(ID, crate::api_bot::anthropic::session_options(Options::default()), quote.clone()).is_none());
    assert!(provider::for_id(crate::api_bot::anthropic::ID, luna_options(), quote.clone()).is_none());
    assert!(provider::for_id("nope", luna_options(), quote.clone()).is_none());

    // Claude 的 checkpoint（有標籤或沒標籤的舊格式）不能用 OpenAI 接續，Claude 形狀的 transcript 也編不出請求。
    let openai = luna();
    let claude_messages = vec![
        Message { role: "user".into(), content: json!("hi") },
        Message { role: "assistant".into(), content: json!([{"type": "text", "text": "hello"}]) },
    ];
    let legacy = State { messages: claude_messages.clone(), ..Default::default() };
    let tagged = State { v: Some(2), provider: Some(crate::api_bot::anthropic::ID.into()), ..legacy.clone() };
    assert!(!openai.checkpoint_matches_model(&legacy) && !openai.checkpoint_matches_model(&tagged));
    assert!(openai.encode("medium", &json!("sys"), &claude_messages, None).is_err());

    // 反過來：OpenAI 的 checkpoint 與請求也進不了 Claude 那一支。
    let claude = provider::for_id(crate::api_bot::anthropic::ID, crate::api_bot::anthropic::session_options(Options::default()), Arc::new(crate::api_bot::quote::FailClosed)).unwrap();
    let openai_request = openai.encode("medium", &json!("sys"), &fresh_state().messages, None).unwrap();
    let cancel = CancellationToken::new();
    assert_eq!(claude.send(transport(&server.endpoint), openai_request.as_ref(), &cancel, None).await.unwrap_err(), http::Error::InvalidRequest);
    let claude_request = RequestEnvelope::new(Options::default(), json!("sys"), vec![Message { role: "user".into(), content: json!("hi") }]).unwrap();
    assert_eq!(openai.send(transport(&server.endpoint), &claude_request, &cancel, None).await.unwrap_err(), http::Error::InvalidRequest);
    // 報價也不跨 provider。
    assert!(matches!(OpenAiQuote.reserve(&claude_request), Err(QuoteError::InvalidRequest)));
    assert!(matches!(crate::api_bot::pricing::DirectGlobal.reserve(openai_request.as_ref()), Err(QuoteError::InvalidRequest)));
    assert!(server.seen.lock().unwrap().bodies.is_empty(), "nothing reached the network");
}

/// 加密 reasoning 只續傳：不在可見文字裡，也不在這條路徑上任何會被印出來的型別的 Debug 裡。
#[tokio::test]
async fn encrypted_reasoning_is_replayed_but_never_visible_or_debug_printed() {
    let (bodies, sent, state, _, _) = full_turn().await;
    assert!(String::from_utf8_lossy(&sent[1]).contains(ENC), "it is carried to the next request");
    assert!(bodies[1].to_string().contains(ENC));
    for response in &state.responses {
        assert!(!OpenAiCodec.visible_text(response).contains(ENC));
    }
    // 錯誤與中立視圖都不帶內容：QuotedRequest 只露出 hash／tier／model。
    let request = luna().encode("medium", &json!("sys"), &state.messages[..state.messages.len() - 1], None).unwrap();
    assert!(!format!("{} {} {}", request.hash(), request.speed(), request.model()).contains(ENC));
    let replay = super::codec::ReplayItem::new(reasoning_item()).unwrap();
    assert!(!format!("{replay:?}").contains(ENC));
    // 上游把內容塞進錯誤訊息也不會原樣帶出超過 500 字，且錯誤型別不含請求內容。
    let server = serve(vec![Reply::json(400, json!({"error": {"code": "invalid_request_error", "message": "bad input"}}))]).await;
    let cancel = CancellationToken::new();
    let err = luna().send(transport(&server.endpoint), request.as_ref(), &cancel, None).await.unwrap_err();
    assert!(!format!("{err:?}").contains(ENC) && !format!("{err:?}").contains(FAKE_KEY));
}

#[tokio::test]
async fn quotes_reserve_an_upper_bound_and_settle_on_the_actual_usage() {
    let (_, _, state, _, _) = full_turn().await;
    let provider = luna();
    let request = provider.encode("medium", &json!("You are a careful assistant."), &state.messages[..1], None).unwrap();
    let wire_len = request.as_any().downcast_ref::<OpenAiRequest>().unwrap().wire().len() as u64;
    let reserved = OpenAiQuote.reserve(request.as_ref()).unwrap();
    // 上界：（位元組數＋固定餘裕）× 最貴的輸入單價（12 小時快取寫入）＋ max_output × 輸出單價，進位到微美元。
    let bound = wire_len + super::pricing::INPUT_MARGIN_TOKENS;
    let expected = (bound as u128 * 250_000 + 4096_u128 * 500_000).div_ceil(1_000_000) as i64;
    assert_eq!(Ok(expected as u64), super::pricing::quote_upper_bound("gpt-6-luna", bound, 4096));
    assert_eq!((reserved.reserved_microusd, reserved.pricing_version.as_str()), (expected, "gpt-6-luna-standard-2026-11-18"));
    assert_eq!((request.speed(), request.model()), ("standard", "gpt-6-luna"));

    // 結算：普通輸入 900、快取讀 200、快取寫 100、輸出 80（reasoning 30 已含在輸出裡，不重算）。
    let settled = OpenAiQuote.settle(request.as_ref(), &state.responses[0]).unwrap();
    let cost = (900_u128 * 100_000 + 200 * 10_000 + 100 * 125_000 + 80 * 500_000).div_ceil(1_000_000) as i64;
    assert_eq!((settled.estimated_microusd, settled.actual_speed), (cost, Some("standard")));
    assert!(settled.estimated_microusd <= reserved.reserved_microusd);
    assert!(provider.usage_problem(request.as_ref(), &state.responses[0]).is_none());

    // 不可信的用量不結算：超出請求上界、超出 max_output、模型不符。
    let with_usage = |u: Value| Response { usage: u, ..state.responses[0].clone() };
    let too_much_input = with_usage(json!({"input_tokens": bound + 1, "cached_tokens": 0, "cache_write_tokens": 0, "cache_write_12h_tokens": 0, "output_tokens": 1, "reasoning_tokens": 0}));
    let too_much_output = with_usage(json!({"input_tokens": 10, "cached_tokens": 0, "cache_write_tokens": 0, "cache_write_12h_tokens": 0, "output_tokens": 5000, "reasoning_tokens": 0}));
    for bad in [&too_much_input, &too_much_output] {
        assert!(matches!(OpenAiQuote.settle(request.as_ref(), bad), Err(QuoteError::UntrustedUsage)));
        assert!(provider.usage_problem(request.as_ref(), bad).is_some());
    }
    let other_model = Response { model: "gpt-other".into(), ..state.responses[0].clone() };
    assert!(matches!(OpenAiQuote.settle(request.as_ref(), &other_model), Err(QuoteError::UntrustedUsage)));
    // 非標準的 service tier 沒有核准的價：費用未知，不照標準價結算。
    let mut priority = state.responses[0].clone();
    priority.usage["service_tier"] = json!("priority");
    assert!(matches!(OpenAiQuote.settle(request.as_ref(), &priority), Err(QuoteError::UntrustedUsage)));
    assert!(provider.usage_problem(request.as_ref(), &priority).is_some_and(|why| why.contains("priority")));
    let mut default_tier = state.responses[0].clone();
    default_tier.usage["service_tier"] = json!("default");
    assert_eq!(OpenAiQuote.settle(request.as_ref(), &default_tier).unwrap().estimated_microusd, cost);
}

/// completed 的回應沒帶 usage：整個回應不採用（費用算不出來），而且不是「確定沒處理」——attempt 留成未知，不當成 0 元。
#[tokio::test]
async fn a_completed_response_without_usage_is_refused_and_never_settled_as_free() {
    let server = serve(vec![Reply::sse(two_tools_stream(Value::Null))]).await;
    let provider = luna();
    let request = provider.encode("medium", &json!("sys"), &fresh_state().messages, None).unwrap();
    let cancel = CancellationToken::new();
    let err = provider.send(transport(&server.endpoint), request.as_ref(), &cancel, None).await.unwrap_err();
    assert!(matches!(&err, http::Error::InvalidResponseDetail(why) if why.contains("usage")), "{err:?}");
    assert!(!provider.definitive_rejection(&err), "the model may have run and billed");
    // 未完成的回應可能沒有用量（存成 null）：內容可以收，但結算拒絕。
    let unbilled = Response { id: "r".into(), model: "gpt-6-luna".into(), content: vec![message_item("partial")], stop_reason: "incomplete:max_output_tokens".into(), usage: Value::Null };
    assert!(provider.invalid_content_reason(&unbilled, &State::default()).is_none(), "the content itself is fine");
    assert!(matches!(OpenAiQuote.settle(request.as_ref(), &unbilled), Err(QuoteError::UntrustedUsage)));
    assert!(provider.usage_problem(request.as_ref(), &unbilled).is_some_and(|why| why.contains("usage")));
}

#[tokio::test]
async fn upstream_failures_are_classified_and_only_clear_rejections_are_definitive() {
    let provider = luna();
    let request = provider.encode("medium", &json!("sys"), &fresh_state().messages, None).unwrap();
    let cancel = CancellationToken::new();
    let send = |reply: Reply| {
        let (provider, request, cancel) = (provider.clone(), request.clone(), cancel.clone());
        async move {
            let server = serve(vec![reply]).await;
            let result = provider.send(transport(&server.endpoint), request.as_ref(), &cancel, None).await;
            let calls = server.seen.lock().unwrap().bodies.len();
            (result, calls)
        }
    };
    let kind_of = |err: &http::Error| match err {
        http::Error::Http { status, kind, .. } => (*status, kind.clone()),
        other => panic!("expected an HTTP error, got {other:?}"),
    };

    let (auth, _) = send(Reply::json(401, json!({"error": {"code": "invalid_api_key", "message": "Incorrect API key provided: sk-test-FAKE…"}}))).await;
    let auth = auth.unwrap_err();
    assert_eq!(kind_of(&auth), (401, Some("authentication_error".into())));
    assert!(provider.definitive_rejection(&auth));

    let (forbidden, _) = send(Reply::json(403, json!({"error": {"type": "permission_error"}}))).await;
    assert_eq!(kind_of(&forbidden.unwrap_err()), (403, Some("permission_error".into())));

    let mut limited = Reply::json(429, json!({"error": {"code": "rate_limit_exceeded"}}));
    limited.headers.push(("retry-after", "17".into()));
    let (limited, _) = send(limited).await;
    let limited = limited.unwrap_err();
    assert_eq!(limited, http::Error::RateLimited { retry_after_secs: Some(17) });
    assert!(provider.definitive_rejection(&limited));

    let (quota, _) = send(Reply::json(429, json!({"error": {"code": "insufficient_quota"}}))).await;
    let quota = quota.unwrap_err();
    assert_eq!(kind_of(&quota), (429, Some("insufficient_quota".into())), "out of quota is not a wait-and-retry rate limit");
    assert!(provider.definitive_rejection(&quota));

    let (model, _) = send(Reply::json(404, json!({"error": {"code": "model_not_found"}}))).await;
    assert_eq!(kind_of(&model.unwrap_err()), (404, Some("model_not_found".into())));

    // 送沒送到不確定的都不算確定拒絕：5xx、408、HTTP 200 之後的 failed、沒有終態就斷流。
    let (server_error, _) = send(Reply::json(503, json!({"error": {"code": "server_error"}}))).await;
    assert!(!provider.definitive_rejection(&server_error.unwrap_err()));
    let (timeout, _) = send(Reply::json(408, json!({}))).await;
    assert!(!provider.definitive_rejection(&timeout.unwrap_err()));
    let failed = sse(&[json!({"type": "response.created", "response": {"id": "r"}}), json!({"type": "response.failed", "response": {"error": {"code": "server_error", "message": "boom"}}})]);
    let (failed, _) = send(Reply::sse(failed)).await;
    let failed = failed.unwrap_err();
    assert!(matches!(&failed, http::Error::StreamAborted { kind: Some(k), .. } if k == "server_error"));
    assert!(!provider.definitive_rejection(&failed));
    let (cut, _) = send(Reply::sse(sse(&[json!({"type": "response.created", "response": {"id": "r"}})]))).await;
    let cut = cut.unwrap_err();
    assert_eq!(cut, http::Error::TransportUnknown);
    assert!(!provider.definitive_rejection(&cut));

    // 被 max_output_tokens 截斷：可辨識的未完成，半截的工具呼叫不執行。
    let cut_call = json!({"type": "function_call", "id": "fc_x", "call_id": "call_X", "name": "bash", "arguments": "{\"comm", "status": "incomplete"});
    let incomplete = sse(&[
        json!({"type": "response.created", "response": {"id": "r"}}),
        json!({"type": "response.incomplete", "response": {"id": "r", "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
               "output": [message_item("partial"), cut_call], "usage": usage(100, 0, 0, 4096, 0)}}),
    ]);
    let (incomplete, _) = send(Reply::sse(incomplete)).await;
    let incomplete = incomplete.unwrap();
    let mut state = fresh_state();
    let out = OpenAiCodec.accept(&mut state, incomplete).unwrap();
    assert_eq!((out.calls.len(), out.stop), (0, provider::StopReason::MaxTokens));
    assert!(state.messages.iter().all(|m| m.content.get("type").and_then(Value::as_str) != Some("function_call")), "the half call is not kept for replay");
}

/// 決定（2026-11-18）：串流已經開始（HTTP 200）之後收到 `response.failed` 或 `error`，不管錯誤代碼看起來多明確，
/// 一律不是「確定沒處理」——attempt 記 unknown、保留預留。只有 HTTP 回應本身就是 401／403／400 類才算明確拒絕。
#[tokio::test]
async fn a_failure_after_the_stream_started_is_never_a_definitive_rejection_whatever_its_code() {
    let provider = luna();
    let request = provider.encode("medium", &json!("sys"), &fresh_state().messages, None).unwrap();
    let cancel = CancellationToken::new();
    let created = json!({"type": "response.created", "response": {"id": "r"}});
    for (label, tail) in [
        ("error event, auth code", json!({"type": "error", "code": "invalid_api_key", "message": "bad key"})),
        ("error event, quota code", json!({"type": "error", "code": "insufficient_quota", "message": "no credit"})),
        ("error event, request code", json!({"type": "error", "code": "invalid_request_error", "message": "bad"})),
        ("failed, model rejected", json!({"type": "response.failed", "response": {"error": {"code": "model_not_found", "message": "nope"}}})),
        ("failed, rate limit", json!({"type": "response.failed", "response": {"error": {"code": "rate_limit_exceeded", "message": "slow down"}}})),
    ] {
        let server = serve(vec![Reply::sse(sse(&[created.clone(), tail]))]).await;
        let err = provider.send(transport(&server.endpoint), request.as_ref(), &cancel, None).await.unwrap_err();
        assert!(matches!(&err, http::Error::StreamAborted { kind: Some(_), .. }), "{label}: {err:?}");
        assert!(!provider.definitive_rejection(&err), "{label}: the reservation must be kept");
    }
    // 對照：同樣的代碼出現在 HTTP 回應本身（串流開始之前）才是明確拒絕。
    for (status, code) in [(401, "invalid_api_key"), (403, "permission_denied"), (400, "invalid_request_error")] {
        let server = serve(vec![Reply::json(status, json!({"error": {"code": code, "message": "x"}}))]).await;
        let err = provider.send(transport(&server.endpoint), request.as_ref(), &cancel, None).await.unwrap_err();
        assert!(matches!(&err, http::Error::Http { status: s, .. } if *s == status), "{code}: {err:?}");
        assert!(provider.definitive_rejection(&err), "{code}");
    }
}

/// 決定（2026-11-18）：usage 缺 `cached_tokens`／`cache_write_tokens` 視為 0、照常結算；缺 `input_tokens` 或 `output_tokens`
/// 就是 MissingUsage——回應不採用、費用未知、保留預留（不是明確拒絕）。
#[tokio::test]
async fn usage_without_cache_details_settles_as_zero_but_missing_input_or_output_tokens_is_missing_usage() {
    let provider = luna();
    let request = provider.encode("medium", &json!("sys"), &fresh_state().messages, None).unwrap();
    let cancel = CancellationToken::new();

    // 沒有任何快取分項：快取讀寫都當 0，全部是普通輸入。
    let server = serve(vec![Reply::sse(two_tools_stream(json!({"input_tokens": 1000, "output_tokens": 40})))]).await;
    let response = provider.send(transport(&server.endpoint), request.as_ref(), &cancel, None).await.unwrap();
    assert_eq!((response.usage["cached_tokens"].as_u64(), response.usage["cache_write_tokens"].as_u64()), (Some(0), Some(0)));
    let settled = OpenAiQuote.settle(request.as_ref(), &response).unwrap();
    assert_eq!(settled.estimated_microusd, (1000_u128 * 100_000 + 40 * 500_000).div_ceil(1_000_000) as i64);
    assert!(provider.usage_problem(request.as_ref(), &response).is_none());

    for (label, usage_value) in [
        ("no input_tokens", json!({"output_tokens": 40, "input_tokens_details": {"cached_tokens": 0}})),
        ("no output_tokens", json!({"input_tokens": 1000})),
        ("empty usage", json!({})),
    ] {
        let server = serve(vec![Reply::sse(two_tools_stream(usage_value))]).await;
        let err = provider.send(transport(&server.endpoint), request.as_ref(), &cancel, None).await.unwrap_err();
        assert!(matches!(&err, http::Error::InvalidResponseDetail(why) if why.contains("usage")), "{label}: {err:?}");
        assert!(!provider.definitive_rejection(&err), "{label}: the reservation must be kept");
    }
}

/// redirect 一律不跟：Authorization 不會被帶到別的站。
#[tokio::test]
async fn a_redirect_is_never_followed() {
    let elsewhere = serve(vec![Reply::sse(final_text_stream("stolen"))]).await;
    let mut redirect = Reply::json(307, json!({}));
    redirect.headers.push(("location", elsewhere.endpoint.clone()));
    let server = serve(vec![redirect]).await;
    let provider = luna();
    let request = provider.encode("medium", &json!("sys"), &fresh_state().messages, None).unwrap();
    let cancel = CancellationToken::new();
    let err = provider.send(transport(&server.endpoint), request.as_ref(), &cancel, None).await.unwrap_err();
    assert!(matches!(err, http::Error::Http { status: 307, .. }), "{err:?}");
    assert!(!provider.definitive_rejection(&err));
    assert!(elsewhere.seen.lock().unwrap().bodies.is_empty(), "the redirect target never saw the request or the key");
    // 非 loopback、帶帳密或 fragment 的 endpoint 在送出前就拒絕。
    for bad in ["http://example.com/v1/responses", "http://user:pw@127.0.0.1:9/v1/responses", "http://127.0.0.1:9/v1/responses#x", OFFICIAL_ENDPOINT] {
        assert_eq!(provider.send(transport(bad), request.as_ref(), &cancel, None).await.unwrap_err(), http::Error::InvalidRequest, "{bad}");
    }
    let no_key = Transport { key: " ", ..transport(&server.endpoint) };
    assert_eq!(provider.send(no_key, request.as_ref(), &cancel, None).await.unwrap_err(), http::Error::InvalidRequest);
}

#[test]
fn the_provider_reports_its_identity_effort_ladder_and_refuses_unapproved_options() {
    let provider = luna();
    assert_eq!((provider.id(), provider.model(), provider.effort(), provider.official_endpoint()), ("openai", "gpt-6-luna", "medium", "https://api.openai.com/v1/responses"));
    // 依 luna 的清單一級一級往下，到底就停。
    let ladder: Vec<String> = std::iter::successors(Some("max".to_string()), |e| Some(provider.lower_effort(e)).filter(|lower| lower != e)).collect();
    assert_eq!(ladder, ["max", "xhigh", "high", "medium", "low", "none"]);
    assert_eq!(provider.lower_effort("bogus"), "bogus");
    for bad in [
        SessionOptions { model: "gpt-unknown".into(), ..luna_options() },
        SessionOptions { effort: "ultra".into(), ..luna_options() },
        SessionOptions { max_output_tokens: 0, ..luna_options() },
    ] {
        assert!(OpenAiProvider::with_standard_pricing(bad).is_none());
    }
    // 不支援的 effort、空的 transcript、配對不上的工具結果都在 HTTP 之前拒絕。
    assert!(provider.encode("ultra", &json!("sys"), &fresh_state().messages, None).is_err());
    assert!(provider.encode("medium", &json!("sys"), &[], None).is_err());
    let orphan = vec![
        Message { role: "user".into(), content: json!("hi") },
        Message { role: "tool_results".into(), content: json!({"results": [{"call_id": "call_Z", "output": "x", "is_error": false}]}) },
    ];
    assert!(provider.encode("medium", &json!("sys"), &orphan, None).is_err());
}

#[test]
fn the_codec_rejects_duplicate_or_already_completed_call_ids_and_keeps_notes_with_the_results() {
    let response = |items: Vec<Value>| Response { id: "r".into(), model: "gpt-6-luna".into(), content: items, stop_reason: "completed".into(), usage: Value::Null };
    let mut state = fresh_state();
    let before = state.messages.len();
    let dup = response(vec![call_item("fc_1", "call_A", "bash", ARGS_A), call_item("fc_2", "call_A", "bash", ARGS_A)]);
    assert!(OpenAiCodec.accept(&mut state, dup).is_err());
    let unknown_tool = response(vec![call_item("fc_1", "call_A", "rm_rf", ARGS_A)]);
    assert!(OpenAiCodec.accept(&mut state, unknown_tool).is_err());
    let bad_args = response(vec![call_item("fc_1", "call_A", "bash", "[1,2]")]);
    assert!(OpenAiCodec.accept(&mut state, bad_args).is_err());
    state.completed_tool_ids.insert("call_A".into());
    assert!(OpenAiCodec.accept(&mut state, response(vec![call_item("fc_1", "call_A", "bash", ARGS_A)])).is_err());
    assert_eq!((state.messages.len(), state.responses.len()), (before, 0), "a rejected response leaves the transcript untouched");

    // 提示跟著工具結果走：送出時接在結果後面當一則 user 訊息。
    state.completed_tool_ids.clear();
    OpenAiCodec.accept(&mut state, response(vec![call_item("fc_1", "call_A", "bash", ARGS_A)])).unwrap();
    let result = provider::ToolResult { call_id: "call_A".into(), output: "ok".into(), is_error: false };
    OpenAiCodec.put_tool_results(&mut state, std::slice::from_ref(&result), None, false);
    OpenAiCodec.put_tool_results(&mut state, std::slice::from_ref(&result), Some("two calls left"), true);
    OpenAiCodec.append_note(&mut state, "wrap up now");
    let body: Value = serde_json::from_slice(luna().encode("medium", &json!("sys"), &state.messages, None).unwrap().as_any().downcast_ref::<OpenAiRequest>().unwrap().wire()).unwrap();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[input.len() - 2]["type"], "function_call_output");
    assert_eq!(input[input.len() - 1]["content"][0]["text"], "two calls left\nwrap up now");
}

/// 新的 async 路徑（編碼 → 送出 → 組串流 → 工具 → 再送）在 1 MiB 的執行緒上跑得完。
#[test]
fn a_full_openai_turn_fits_in_a_one_mib_stack() {
    std::thread::Builder::new()
        .name("openai-stack-budget".into())
        .stack_size(1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let (bodies, _, state, _, executed) = full_turn().await;
                assert_eq!((bodies.len(), executed.len(), state.responses.len()), (2, 2, 2));
            })
        })
        .unwrap()
        .join()
        .expect("the turn overflowed the 1 MiB stack or panicked");
}
