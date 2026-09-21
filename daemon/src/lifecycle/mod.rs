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
pub(crate) mod relay_watch;
pub(crate) mod dead_panes;
mod queue;
mod setup;
mod start;
mod stop;
mod deferred_live;
mod slash;
mod delivery;
mod prompt;
mod pane_text;
mod poller;
mod transcript_origin;
mod screen;
mod limit_banner;
mod stuck_turns;
pub(crate) mod fence;
pub(crate) mod turn_controller;
mod interrupt_grace;
pub(crate) mod resume_gate;
pub(crate) mod restart_hold;
/// `runs.state` 轉移的唯一寫法，與寫不進去之後的重試（#135／#145／#146）。
mod run_state;
pub(crate) mod quota_hold;
pub(crate) mod start_send;
mod send_now;
mod interruption;
/// 送達結果寫不回 DB 時欠著的那一筆（#149）。
mod owed_delivery;
/// claude 把貼上的 prompt 包成 `<pasted_content>` 寫進 transcript（#218）。
pub(crate) mod pasted_content;
mod paste_check;
mod transitions;
/// issue #81 探索用的原型；`#[cfg(test)]` 整個檔案只在 `cargo test` 底下編，不進正式二進位
/// （見檔案頂端的說明與 docs/CLAUDE-NATIVE-TRANSPORT.md）。
#[cfg(test)]
mod native_transport_prototype;
/// issue #77 探索用的原型，同一個做法：`#[cfg(test)]` 整個檔案只在 `cargo test` 底下編，
/// 不進正式二進位（見檔案頂端的說明與 docs/ACTOR-RUNTIME-EVAL.md）。
#[cfg(test)]
mod actor_runtime_eval_prototype;
/// issue #92 的端到端情境：撞額度 → 換身分 → `--resume` 接回同一段 session（只在 `cargo test` 底下編）。
#[cfg(test)]
mod identity_switch_tests;
/// 競態的注入點，只在 `cargo test` 底下存在（見檔案頂端的說明）。
#[cfg(test)]
pub(crate) mod race_point;
/// #187：`purge_deleted_bot_dirs` 讀不到 run 的狀態時不刪目錄。
#[cfg(test)]
mod purge_dirs_tests;
/// #188：子 agent 不能走 stop + start；夾具（一顆有真 mock pane 的子 agent）給批次重啟的測試共用。
#[cfg(test)]
pub(crate) mod restart_kind_tests;

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
pub(crate) use deferred_live::{defer_live, is_busy_reason, schedule_deferred_live};
pub(crate) use delivery::*;
pub(crate) use start::*;
pub(crate) use start_send::{prompt_starting, withdraw_turn};
pub(crate) use stop::*;
pub(crate) use interrupt_grace::{note_user_interrupt_of, settle_interrupt_echo, FailureEvidence as InterruptFailureEvidence};
pub(crate) use interruption::{adopt_interrupted_on_restart, adopt_turns_of_ended_runs, adopt_unbound_send_nows, settle_locked as settle_interruption, Evidence as InterruptEvidence};
pub(crate) use owed_delivery::{owed_as_unknown, settle_locked as settle_owed_deliveries};
#[cfg(test)]
pub(crate) use interrupt_grace::{expect_interrupt_echo, note_user_interrupt, InterruptedTurn};
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
    /// 503：我們自己需要的一份狀態暫時讀不到（目前只有維護窗口的租約，issue #127），所以**不敢**往下做——
    /// 不是「herdr／DB 出錯」的統稱 502。body 是機器可讀的，帶 `retryable:true`、`sent:false`（一個字都沒送）。
    Unavailable(Value),
    /// 503：跟 `Unavailable` 相反——外面的副作用**已經做了**（agent 起來了、pane 關了），run 的狀態卻寫不進 DB
    /// （#145／#146）。不是「沒做」也不是「做好了」：重試已經排了，run 會照 herdr 的證據收斂。body 見 [`LcError::uncommitted`]。
    Uncommitted(Value),
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

    /// `{"error": what, "run_id", "retryable": true, "message", "detail"}`；`what` 是
    /// `start_state_uncommitted`／`stop_state_uncommitted`。
    pub(crate) fn uncommitted(what: &str, run_id: &str, message: &str, detail: impl std::fmt::Display) -> Self {
        LcError::Uncommitted(json!({
            "error": what, "run_id": run_id, "retryable": true, "message": message, "detail": detail.to_string(),
        }))
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
