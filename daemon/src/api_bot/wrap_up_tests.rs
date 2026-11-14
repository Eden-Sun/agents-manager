//! 回合軟期限（`turn_timeout_secs`）：到了不再開始新動作、送一次收尾請求要摘要；進行中的請求與工具不被砍。
use super::protocol::{Message, Response, ToolCall};
use super::runner::{self, Halt, Journal, Model, State, Tools};
use futures::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn response(content: Vec<Value>, reason: &str) -> Response {
    Response {
        id: "msg_1".into(),
        model: crate::config::API_BOT_MODEL.into(),
        content,
        stop_reason: reason.into(),
        usage: json!({"input_tokens": 1, "output_tokens": 1}),
    }
}
fn tool(id: &str) -> Value {
    json!({"type": "tool_use", "id": id, "name": "bash", "input": {"command": "true"}})
}
fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}
fn initial() -> State {
    State { messages: vec![Message { role: "user".into(), content: json!("go") }], ..Default::default() }
}

/// 照順序回覆；`flip_in_first_request`＝第一次請求進行中（回覆前）把軟期限打開。
struct Script {
    replies: Mutex<Vec<Response>>,
    sent: Mutex<Vec<Vec<Message>>>,
    calls: AtomicUsize,
    soft: Arc<AtomicBool>,
    flip_in_first_request: bool,
    cancelled_seen: AtomicBool,
}
impl Model for Script {
    fn complete<'a>(&'a self, messages: &'a [Message], cancel: &'a CancellationToken) -> BoxFuture<'a, Result<Response, super::http::Error>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.sent.lock().unwrap().push(messages.to_vec());
            if n == 0 && self.flip_in_first_request {
                tokio::time::sleep(Duration::from_millis(30)).await;
                self.soft.store(true, Ordering::SeqCst);
            }
            if cancel.is_cancelled() {
                self.cancelled_seen.store(true, Ordering::SeqCst);
            }
            Ok(self.replies.lock().unwrap().remove(0))
        })
    }
}
/// 第一個工具跑完時把軟期限打開。
struct FlipTools {
    ran: Mutex<Vec<String>>,
    soft: Arc<AtomicBool>,
    flip_after_first: bool,
}
impl Tools for FlipTools {
    fn execute<'a>(&'a self, call: &'a ToolCall, _: &'a CancellationToken) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.ran.lock().unwrap().push(call.id.clone());
            if self.flip_after_first {
                self.soft.store(true, Ordering::SeqCst);
            }
            Ok(format!("ok:{}", call.id))
        })
    }
}
#[derive(Default)]
struct Mem;
impl Journal for Mem {
    fn save<'a>(&'a self, _: &'a State) -> BoxFuture<'a, Result<(), ()>> {
        Box::pin(async { Ok(()) })
    }
}

fn setup(replies: Vec<Response>, flip_in_request: bool, flip_after_tool: bool) -> (Script, FlipTools, Arc<AtomicBool>) {
    let soft = Arc::new(AtomicBool::new(false));
    let model = Script { replies: Mutex::new(replies), sent: Mutex::new(vec![]), calls: AtomicUsize::new(0), soft: soft.clone(), flip_in_first_request: flip_in_request, cancelled_seen: AtomicBool::new(false) };
    let tools = FlipTools { ran: Mutex::new(vec![]), soft: soft.clone(), flip_after_first: flip_after_tool };
    (model, tools, soft)
}

#[tokio::test]
async fn soft_deadline_mid_batch_skips_remaining_tools_then_asks_for_a_summary() {
    let (model, tools, soft) = setup(
        vec![response(vec![tool("t1"), tool("t2")], "tool_use"), response(vec![text("已完成 t1；t2 未做")], "end_turn")],
        false,
        true,
    );
    let mut state = initial();
    let s = soft.clone();
    let halt = runner::run_with_wrap_up(&model, &tools, &Mem, &mut state, &CancellationToken::new(), 5, &|| false, &move || s.load(Ordering::SeqCst)).await;
    assert_eq!(halt, Ok(Halt::TimeLimit));
    assert_eq!(*tools.ran.lock().unwrap(), vec!["t1".to_string()], "軟期限後不再開始新工具");
    assert_eq!(model.calls.load(Ordering::SeqCst), 2, "一次收尾請求");
    let second = &model.sent.lock().unwrap()[1];
    let last = second.last().unwrap().content.as_array().unwrap().clone();
    assert_eq!(last[0]["tool_use_id"], "t1");
    assert_eq!(last[1]["tool_use_id"], "t2");
    assert!(last[1]["content"].as_str().unwrap().contains("time limit reached"), "沒做的工具標成 not executed");
    assert!(last.last().unwrap()["text"].as_str().unwrap().contains("time limit"), "收尾請求附提示");
    assert_eq!(runner::final_text(state.responses.last().unwrap()), "已完成 t1；t2 未做", "摘要存成最後回覆");
    assert!(!state.api_pending && state.tool_pending.is_none());
}

#[tokio::test]
async fn request_in_flight_when_the_soft_deadline_passes_is_not_cut() {
    let (model, tools, soft) = setup(
        vec![response(vec![tool("t1")], "tool_use"), response(vec![text("摘要")], "end_turn")],
        true,
        false,
    );
    let mut state = initial();
    let cancel = CancellationToken::new();
    let s = soft.clone();
    let halt = runner::run_with_wrap_up(&model, &tools, &Mem, &mut state, &cancel, 5, &|| false, &move || s.load(Ordering::SeqCst)).await;
    assert_eq!(halt, Ok(Halt::TimeLimit));
    assert!(!cancel.is_cancelled() && !model.cancelled_seen.load(Ordering::SeqCst), "軟期限不取消任何東西");
    assert_eq!(state.responses.len(), 2, "進行中的請求的回覆有存下來");
    assert!(tools.ran.lock().unwrap().is_empty(), "那次回覆要求的工具在軟期限後不執行");
}

#[tokio::test]
async fn wrap_up_request_that_still_asks_for_tools_is_settled_not_executed() {
    let (model, tools, soft) = setup(
        vec![response(vec![tool("t1")], "tool_use"), response(vec![tool("t2")], "tool_use")],
        false,
        true,
    );
    let mut state = initial();
    let s = soft.clone();
    let halt = runner::run_with_wrap_up(&model, &tools, &Mem, &mut state, &CancellationToken::new(), 5, &|| false, &move || s.load(Ordering::SeqCst)).await;
    assert_eq!(halt, Ok(Halt::TimeLimit));
    assert_eq!(*tools.ran.lock().unwrap(), vec!["t1".to_string()]);
    let last = state.messages.last().unwrap();
    assert_eq!(last.content[0]["tool_use_id"], "t2");
    assert!(last.content[0]["content"].as_str().unwrap().contains("time limit reached"));
    assert!(state.tool_pending.is_none(), "對話維持合法，下一回合送得出去");
}

#[tokio::test]
async fn without_a_soft_deadline_behavior_is_unchanged() {
    let (model, tools, _soft) = setup(vec![response(vec![tool("t1")], "tool_use"), response(vec![text("done")], "end_turn")], false, false);
    let mut state = initial();
    let halt = runner::run_interruptible(&model, &tools, &Mem, &mut state, &CancellationToken::new(), 5, &|| false).await;
    assert_eq!(halt, Ok(Halt::EndTurn));
    assert_eq!(tools.ran.lock().unwrap().len(), 1);
}

#[test]
fn deadlines_are_85_percent_soft_and_120_seconds_past_the_limit_hard() {
    let (soft, hard) = super::runtime::turn_deadlines(1800);
    assert_eq!((soft, hard), (Duration::from_secs(1530), Duration::from_secs(1920)));
    let (soft, hard) = super::runtime::turn_deadlines(300);
    assert_eq!((soft, hard), (Duration::from_secs(255), Duration::from_secs(420)));
}

#[test]
fn failure_text_for_a_hard_timeout_names_the_limit_and_where_to_change_it() {
    let text = super::completion::failure_explanation("turn_timeout:1800");
    assert!(text.contains("1800") && text.contains("claude-api"), "{text}");
}
