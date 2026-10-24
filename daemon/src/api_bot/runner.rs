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
}
#[derive(Debug, PartialEq, Eq)]
pub enum Halt {
    EndTurn,
    Refusal,
    MaxTokens,
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
    if state.api_pending || state.tool_pending.is_some() {
        return Err(Error::PendingUnknown);
    }
    if !(1..=64).contains(&max_calls) || state.messages.is_empty() {
        return Err(Error::Budget);
    }
    for _ in 0..max_calls {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
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
        if calls.is_empty() {
            return Ok(match reason.as_str() {
                "end_turn" => Halt::EndTurn,
                "refusal" => Halt::Refusal,
                "max_tokens" => Halt::MaxTokens,
                _ => Halt::Other(reason),
            });
        }
        let mut results: Vec<Value> = Vec::new();
        for call in calls {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            state.tool_pending = Some(call.id.clone());
            checkpoint(journal, state).await?;
            let result = tools.execute(&call, cancel).await;
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let (content, is_error) = match result {
                Ok(s) => (s, false),
                Err(e) => (e, true),
            };
            results.push(json!({"type":"tool_result", "tool_use_id":call.id,"content":content,"is_error":is_error}));
            state.completed_tool_ids.insert(call.id);
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
        state.tool_pending = None;
        checkpoint(journal, state).await?;
    }
    Err(Error::Budget)
}

async fn checkpoint(journal: &impl Journal, state: &mut State) -> Result<(), Error> {
    if journal.save(state).await.is_err() {
        // Also poison the in-memory state: callers cannot reuse an uncommitted transcript.
        state.api_pending = true;
        return Err(Error::Journal);
    }
    Ok(())
}
