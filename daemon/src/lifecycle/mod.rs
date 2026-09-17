//! Bot / Run lifecycle (SPEC §6.2–§6.4, §4.3). Every public entry point takes the per-bot lock.

use crate::capture::Capture;
use crate::config::{valid_id, ID_RE, LOCAL_HOST};
use crate::db;
use crate::herdr::{AgentStatus, HerdrClient, HerdrError};
use crate::hosts::{sh_quote, HostConn};
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

mod messages;
mod queue;
mod setup;
mod start;
mod stop;
mod slash;
mod delivery;
mod prompt;
mod poller;
mod screen;
mod limit_banner;
mod stuck_turns;
pub(crate) mod fence;
pub(crate) mod turn_controller;
mod interrupt_grace;
mod transitions;
/// issue #81 探索用的原型；`#[cfg(test)]` 整個檔案只在 `cargo test` 底下編，不進正式二進位
/// （見檔案頂端的說明與 docs/CLAUDE-NATIVE-TRANSPORT.md）。
#[cfg(test)]
mod native_transport_prototype;

/// Runs the daemon has typed into during **this** boot. `runs.pane_typed` is the durable record;
/// this is the conservative in-process copy, so a row that later becomes unreadable cannot send a
/// pane back to the herdr `agent.prompt` path that swallowed prompts (sol review round three #2).
static PANE_TYPED_MEMO: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();

pub(crate) fn remember_pane_typed(run_id: &str) {
    if let Ok(mut set) = PANE_TYPED_MEMO.get_or_init(Default::default).lock() {
        set.insert(run_id.to_string());
    }
}

pub(crate) fn pane_typed_memo(run_id: &str) -> bool {
    PANE_TYPED_MEMO.get_or_init(Default::default).lock().map(|s| s.contains(run_id)).unwrap_or(true)
}

pub(crate) use messages::*;
pub(crate) use poller::*;
pub(crate) use prompt::*;
pub(crate) use queue::*;
pub(crate) use screen::*;
pub(crate) use setup::*;
pub(crate) use slash::*;
pub(crate) use delivery::*;
pub(crate) use start::*;
pub(crate) use stop::*;
pub(crate) use interrupt_grace::{is_held as user_interrupt_held, note_user_interrupt};
pub(crate) use stuck_turns::{observe as observe_agent_status, spawn_stuck_turn_sweeper, sweep as sweep_stuck_turns};

#[derive(Debug)]
pub enum LcError {
    NotFound(String),
    Conflict(Value),
    Upstream(String),
    Bad(String),
    /// A 400 whose body is machine-readable rather than a message, e.g.
    /// `{"error":"remote_not_supported","host":"m4p"}`.
    BadValue(Value),
    /// 422: the request is well-formed and allowed, but this one can never be carried out as asked
    /// (e.g. a prompt too long to prove delivered). Machine-readable body; callers treat it as final.
    Unprocessable(Value),
    /// 403：請求本身沒問題，但**你不是可以做這件事的人**（目前只有租約的憑證比對）。
    /// 跟 409 分開：409 是「狀態不對，等一下再來」，403 重試一百次也一樣。
    Forbidden(Value),
}

impl LcError {
    pub fn conflict(reason: &str, extra: Value) -> Self {
        let mut o = json!({ "error": "conflict", "reason": reason });
        if let (Some(a), Some(b)) = (o.as_object_mut(), extra.as_object()) {
            for (k, v) in b {
                a.insert(k.clone(), v.clone());
            }
        }
        LcError::Conflict(o)
    }
}

pub type LcResult<T> = std::result::Result<T, LcError>;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

/// herdr's "pane exists but its shell is not ready yet" — a timing answer, not a failure.
/// Message fallback: some call sites only see the error flattened into a string.
fn pane_not_ready(e: &anyhow::Error) -> bool {
    if let Some(h) = e.downcast_ref::<HerdrError>() {
        if h.code == "agent_pane_busy" || h.message.contains("not an available shell") {
            return true;
        }
    }
    let s = e.to_string();
    s.contains("agent_pane_busy") || s.contains("not an available shell")
}

async fn client_for_run(app: &Arc<App>, run: &db::Run) -> LcResult<HerdrClient> {
    app.herdr_for_run(run)
        .await
        .ok_or_else(|| LcError::Upstream(format!("no Herdr session is available for run `{}`", run.id)))
}
