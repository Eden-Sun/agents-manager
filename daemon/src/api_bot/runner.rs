use super::protocol::{Message, Response, ToolCall};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

pub trait Model: Send + Sync {
    fn complete<'a>(
        &'a self,
        messages: &'a [Message],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Response, super::http::Error>>;
}
pub trait Tools: Send + Sync {
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>>;
}
pub trait Journal: Send + Sync {
    /// Return Ok ONLY after this entire checkpoint is durable. A failures must stop execution.
    fn save<'a>(&'a self, state: &'a State) -> BoxFuture<'a, Result<(), ()>>;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub messages: Vec<Message>,
    pub responses: Vec<Response>,
    /// An accepted request with no saved response has unknown delivery/billing after a crash.
    pub api_pending: bool,
    /// A tool with no saved result may already have performed its side effect. Never replay.
    pub tool_pending: Option<String>,
    pub completed_tool_ids: std::collections::HashSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Halt {
    EndTurn,
    Refusal,
    MaxTokens,
    MaxCallsReached,
    /// 軟期限到：不再開始新動作，已用一次收尾請求讓模型寫出摘要。
    TimeLimit,
    Other(String),
}
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    PendingUnknown,
    Cancelled,
    Budget,
    Journal,
    InvalidResponse,
    Model(super::http::Error),
}

/// Project only user-visible text blocks from a completed provider response.
///
/// The checkpoint keeps the full protocol content for continuation; this projection is only for
/// the ordinary assistant message and deliberately drops thinking/signatures/tool payloads.
pub(crate) fn final_text(response: &Response) -> String {
    let mut text = String::new();
    for block in &response.content {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(part) = block.get("text").and_then(Value::as_str) {
                text.push_str(part);
            }
        }
    }
    text
}

/// No automatic retry/fallback or crash replay. The host must fence run/generation and persist
/// these checkpoints before exposing a production entry point (not included in A).
pub async fn run(
    model: &impl Model,
    tools: &impl Tools,
    journal: &impl Journal,
    state: &mut State,
    cancel: &CancellationToken,
    max_calls: usize,
) -> Result<Halt, Error> {
    run_interruptible(model, tools, journal, state, cancel, max_calls, &|| false).await
}

/// 使用者強制中斷時（不是 run 停止、不是當機），把取消當下進行中的工具批次結清：
/// 已經回來的那一個工具寫真實輸出（加註 `[interrupted by user]`、is_error），這一批還沒開始的每一個寫
/// `not executed: interrupted by user`，全部標完成、`tool_pending=None` 後存 checkpoint，下一回合才送得出去。
/// 只有「工具真的回來了」才會走到這裡；沒有記下結果就不放寬 `tool_pending`。
fn settle_interrupted_batch(
    state: &mut State,
    results: &mut Vec<Value>,
    pushed: bool,
    finished: Option<(&str, String)>,
    rest: &[crate::api_bot::protocol::ToolCall],
    note: &str,
) {
    if let Some((id, output)) = finished {
        results.push(json!({"type":"tool_result","tool_use_id":id,"content":format!("{output}\n[interrupted by user]"),"is_error":true}));
        state.completed_tool_ids.insert(id.to_string());
    }
    for call in rest {
        results.push(json!({"type":"tool_result","tool_use_id":call.id,"content":format!("not executed: {note}"),"is_error":true}));
        state.completed_tool_ids.insert(call.id.clone());
    }
    if results.is_empty() {
        return;
    }
    if pushed {
        state.messages.last_mut().expect("tool result batch exists").content = json!(results);
    } else {
        state.messages.push(Message { role: "user".into(), content: json!(results) });
    }
    state.tool_pending = None;
}

pub async fn run_interruptible(
    model: &impl Model,
    tools: &impl Tools,
    journal: &impl Journal,
    state: &mut State,
    cancel: &CancellationToken,
    max_calls: usize,
    user_interrupted: &(dyn Fn() -> bool + Sync),
) -> Result<Halt, Error> {
    run_with_wrap_up(model, tools, journal, state, cancel, max_calls, user_interrupted, &|| false).await
}

const WRAP_UP_NOTE: &str = "[AG Man] The time limit for this turn has been reached. Do NOT start any new action or tool call. Reply with a summary: what is done, what is not done, and how to continue.";

/// 加一則收尾提示給下一次請求：附在最後一則（工具結果）user 訊息上；最後一則不是陣列內容就不加。
fn push_wrap_up_note(state: &mut State) {
    if let Some(last) = state.messages.last_mut() {
        if last.role == "user" {
            if let Some(arr) = last.content.as_array_mut() {
                arr.push(json!({"type": "text", "text": WRAP_UP_NOTE}));
            }
        }
    }
}

/// 同 [`run_interruptible`]，另有軟期限 `wrap_up`：回傳 true 之後不再執行新的工具，
/// 剩下的工具結清成 `not executed`，再送**一次**收尾請求（附提示，要模型寫摘要），結束於 [`Halt::TimeLimit`]。
/// 進行中的請求與工具不被中斷（硬期限由呼叫端另外取消）；收尾請求不佔 `max_calls`。
pub async fn run_with_wrap_up(
    model: &impl Model,
    tools: &impl Tools,
    journal: &impl Journal,
    state: &mut State,
    cancel: &CancellationToken,
    max_calls: usize,
    user_interrupted: &(dyn Fn() -> bool + Sync),
    wrap_up: &(dyn Fn() -> bool + Sync),
) -> Result<Halt, Error> {
    if state.api_pending || state.tool_pending.is_some() {
        return Err(Error::PendingUnknown);
    }
    if !(1..=64).contains(&max_calls) || state.messages.is_empty() {
        return Err(Error::Budget);
    }
    let mut wrapping_up = false;
    for call_idx in 0..=max_calls {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !wrapping_up && wrap_up() {
            wrapping_up = true;
            push_wrap_up_note(state);
        }
        if call_idx >= max_calls && !wrapping_up {
            break;
        }
        state.api_pending = true;
        checkpoint(journal, state).await?;
        let response = model
            .complete(&state.messages, cancel)
            .await
            .map_err(Error::Model)?;
        let calls = response.tools().map_err(|_| Error::InvalidResponse)?;
        if calls.len() > 32
            || calls
                .iter()
                .any(|c| state.completed_tool_ids.contains(&c.id))
        {
            return Err(Error::InvalidResponse);
        }
        state.messages.push(Message {
            role: "assistant".into(),
            content: json!(response.content),
        });
        let reason = response.stop_reason.clone();
        state.responses.push(response);
        state.api_pending = false;
        // There may be tools not yet started: keep a pending marker until ALL results are
        // durably committed. Crash recovery must not send an incomplete assistant tool batch.
        state.tool_pending = calls.first().map(|c| c.id.clone());
        checkpoint(journal, state).await?;
        if wrapping_up && !calls.is_empty() {
            // 收尾請求還是要工具：不執行，結清後結束（對話維持合法）。
            let mut results: Vec<Value> = Vec::new();
            settle_interrupted_batch(state, &mut results, false, None, &calls, "time limit reached");
            checkpoint(journal, state).await?;
            return Ok(Halt::TimeLimit);
        }
        if calls.is_empty() {
            if wrapping_up && reason == "end_turn" {
                return Ok(Halt::TimeLimit);
            }
            return Ok(match reason.as_str() {
                "end_turn" => Halt::EndTurn,
                "refusal" => Halt::Refusal,
                "max_tokens" => Halt::MaxTokens,
                _ => Halt::Other(reason),
            });
        }
        let mut results: Vec<Value> = Vec::new();
        let mut out_of_time = false;
        for (idx, call) in calls.iter().enumerate() {
            // 軟期限：不開始新工具，這一批剩下的結清，下一輪送收尾請求。
            if !user_interrupted() && wrap_up() && !cancel.is_cancelled() {
                let pushed = !results.is_empty();
                settle_interrupted_batch(state, &mut results, pushed, None, &calls[idx..], "time limit reached");
                out_of_time = true;
                break;
            }
            if cancel.is_cancelled() {
                // 前一個工具已經回來並記下結果（tool_pending 只是批次還沒收尾）；使用者中斷就把剩下的結清。
                if user_interrupted() {
                    let pushed = !results.is_empty();
                    settle_interrupted_batch(state, &mut results, pushed, None, &calls[idx..], "interrupted by user");
                    checkpoint(journal, state).await?;
                }
                return Err(Error::Cancelled);
            }
            state.tool_pending = Some(call.id.clone());
            checkpoint(journal, state).await?;
            let result = tools.execute(call, cancel).await;
            if cancel.is_cancelled() {
                if user_interrupted() {
                    let output = match result {
                        Ok(s) => s,
                        Err(e) => e,
                    };
                    let pushed = !results.is_empty();
                    settle_interrupted_batch(state, &mut results, pushed, Some((&call.id, output)), &calls[idx + 1..], "interrupted by user");
                    checkpoint(journal, state).await?;
                }
                return Err(Error::Cancelled);
            }
            let (content, is_error) = match result {
                Ok(s) => (s, false),
                Err(e) => (e, true),
            };
            results.push(json!({"type":"tool_result", "tool_use_id":call.id,"content":content,"is_error":is_error}));
            state.completed_tool_ids.insert(call.id.clone());
            // Keep partial results in the transcript too, so a durable checkpoint cannot lose
            // the output of a side effect. Only the final batch is submitted to the model.
            if results.len() == 1 {
                state.messages.push(Message {
                    role: "user".into(),
                    content: json!(results),
                });
            } else {
                state
                    .messages
                    .last_mut()
                    .expect("tool result batch exists")
                    .content = json!(results);
            }
            checkpoint(journal, state).await?;
        }
        if out_of_time {
            state.tool_pending = None;
            checkpoint(journal, state).await?;
            continue;
        }
        let remaining_calls = max_calls.saturating_sub(call_idx + 1);
        if remaining_calls == 2 {
            results.push(json!({
                "type": "text",
                "text": "[AG Man] You have 2 API calls left in this turn. Stop starting new work: write a progress summary (what is done, findings so far, what remains and how to continue) as your reply."
            }));
            state
                .messages
                .last_mut()
                .expect("tool result batch exists")
                .content = json!(results);
        }
        state.tool_pending = None;
        checkpoint(journal, state).await?;
    }
    Ok(Halt::MaxCallsReached)
}

async fn checkpoint(journal: &impl Journal, state: &mut State) -> Result<(), Error> {
    if journal.save(state).await.is_err() {
        // Also poison the in-memory state: callers cannot reuse an uncommitted transcript.
        state.api_pending = true;
        return Err(Error::Journal);
    }
    Ok(())
}
