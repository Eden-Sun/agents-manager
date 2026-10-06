//! Bot / Run lifecycle (SPEC §6.2–§6.4, §4.3). Every public entry point takes the per-bot lock.

// P4 composition adapter: App is the only layer that translates the narrow turn port into the
// existing lifecycle entry points. Keep the adapter outside this directory while assigning it
// to the lifecycle seam; App/state itself remains untouched.
// P6 will be the first in-tree consumer; until then this crate-local adapter is intentionally unused.
#[allow(dead_code)]
#[path = "../app_ports_p4.rs"]
pub mod app_ports_p4;

use crate::capture::Capture;
use crate::config::{valid_id, ID_RE, LOCAL_HOST};
use crate::db;
use crate::herdr::{AgentStatus, HerdrClient, HerdrError};
use crate::hosts::{sh_quote, HostConn, HostFence};
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Client and host authority captured together for one run. Resolving the host name again later
/// could silently substitute a replacement connection after a repoint or reconnect.
#[derive(Clone)]
pub(crate) struct RunClient {
    pub(crate) client: HerdrClient,
    pub(crate) fence: HostFence,
}

impl std::ops::Deref for RunClient {
    type Target = HerdrClient;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

mod messages;
pub(crate) mod relay_watch;
pub(crate) mod dead_panes;
mod queue;
pub(crate) mod setup;
pub(crate) mod agent_md;
pub(crate) mod grok_hook;
pub(crate) mod agy_hook;
pub(crate) mod agy_session;
#[cfg(test)]
mod agy_tests;
pub(crate) mod grok_transcript;
#[cfg(test)]
mod grok_hook_tests;
#[cfg(test)]
mod suggestion_list_tests;
pub(crate) mod start;
mod stop;
mod deferred_live;
mod live_apply_debt;
mod slash;
mod delivery;
mod codex_banner;
mod composer_draft;
mod suggestion;
mod busy_send;
/// 「輸入框有沒有字」的同一支判斷，給 lifecycle 以外的地方（judge 的卡住畫面）用。
pub(crate) use delivery::{plain_without_hints, read_styled};
#[allow(unused_imports)]
pub(crate) use crate::composer_parse::{composer_text, composer_text_whole, prompt_suggestion};
mod prompt;
pub(crate) async fn rearm_queued_prompt_restamps(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::Emit + crate::capabilities::BotStatusEmit)) -> anyhow::Result<()> {
    prompt::rearm_queued_prompt_restamps(app).await
}
mod pane_text;
pub(crate) mod poller;
pub(crate) mod transcript_origin;
pub(crate) use transcript_origin::starter_origin_kind_at;
mod screen;
pub(crate) mod transcript_stage;
#[cfg(test)]
mod claude_linux_screens;
mod limit_banner;
mod stuck_turns;
pub(crate) mod fence;
pub(crate) mod turn_controller;
mod interrupt_grace;
pub(crate) mod resume_gate;
/// 忙到一半被重啟的 claude 接回後補一句續行提示（claude 2.1.281 不再補隱藏的 Continue）。
mod resume_nudge;
/// daemon 自己排進佇列的通知：較短的重試上限、可以撤回（#562）。
pub(crate) use crate::daemon_notice;
pub(crate) use resume_nudge::poke as poke_resume_nudge;
pub(crate) mod restart_hold;
/// `runs.state` 轉移的唯一寫法，與寫不進去之後的重試（#135／#145／#146）。
mod run_state;
pub(crate) mod quota_hold;
pub(crate) mod start_send;
pub(crate) mod send_now;
/// codex 的 send_now：steer 進進行中的回合（issue #748，預設關的 canary）。
mod codex_steer;
mod interruption;
/// 送達結果寫不回 DB 時欠著的那一筆（#149）。
mod owed_delivery;
/// claude 把貼上的 prompt 包成 `<pasted_content>` 寫進 transcript（#218）。
pub(crate) use crate::pasted_content;
pub(crate) mod paste_check;
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
pub(crate) use crate::race_point;
/// #187：`purge_deleted_bot_dirs` 讀不到 run 的狀態時不刪目錄。
#[cfg(test)]
mod purge_dirs_tests;
/// #188：子 agent 不能走 stop + start；夾具（一顆有真 mock pane 的子 agent）給批次重啟的測試共用。
#[cfg(test)]
pub(crate) mod restart_kind_tests;
#[cfg(test)]
mod herdr_review_tests;

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

/// 不在 `active` 裡的 run（結束了）不留帳：run id 每次都是新的，只記不清的話長跑的 daemon 裡這張表只增不減。
pub(crate) fn retain_pane_typed(active: &[String]) {
    if let Ok(mut set) = PANE_TYPED_MEMO.get_or_init(Default::default).lock() {
        set.retain(|id| active.contains(id));
    }
}

/// 沒了的 bot（刪掉、退役的 child）在 lifecycle 各模組的 per-bot 記憶體帳：它們只在「結清／套用／disarm」時才清，
/// bot 先沒了就沒有人會再來清。
#[cfg(not(test))]
pub(crate) fn retain_bot_state(live: &[String]) {
    interrupt_grace::retain_bots(live);
    interruption::retain_bots(live);
    owed_delivery::retain_bots(live);
    resume_nudge::retain_bots(live);
    deferred_live::retain_bots(live);
}

#[cfg(test)]
mod pane_typed_memo_tests {
    use super::*;

    #[test]
    fn an_ended_runs_pane_typed_memo_is_dropped() {
        remember_pane_typed("memo-gone");
        remember_pane_typed("memo-kept");
        retain_pane_typed(&["memo-kept".to_string()]);
        assert!(!pane_typed_memo("memo-gone"), "結束的 run 不留帳");
        assert!(pane_typed_memo("memo-kept"));
        retain_pane_typed(&[]);
    }
}

pub(crate) use messages::*;
pub(crate) use poller::*;
pub(crate) use prompt::*;
pub(crate) use queue::*;
pub(crate) use screen::*;
pub(crate) use setup::*;
pub(crate) use slash::*;
pub(crate) use deferred_live::{defer_live, is_busy_reason, is_deferred, schedule_deferred_live};
#[cfg(test)]
pub(crate) use deferred_live::apply_deferred_once;

pub(crate) async fn recover_live_apply_debts(app: &Arc<App>) {
    live_apply_debt::recover(app).await;
}

#[cfg(test)]
pub(crate) use live_apply_debt::retry_once as retry_live_apply_bookkeeping_once;
pub(crate) use delivery::*;
pub(crate) use start::*;
pub(crate) use start_send::{prompt_starting_or_queue, prompt_starting_or_queue_with_share_token, withdraw_turn};
pub(crate) use composer_draft::submit as submit_composer_draft;
pub(crate) use suggestion::accept as accept_prompt_suggestion;
pub(crate) use stop::*;
pub(crate) use interrupt_grace::{note_user_interrupt_of, settle_interrupt_echo, FailureEvidence as InterruptFailureEvidence};
pub(crate) use interruption::{adopt_interrupted_on_restart, adopt_turns_of_ended_runs, adopt_unbound_send_nows, settle_locked as settle_interruption, Evidence as InterruptEvidence};
pub(crate) use owed_delivery::{owed_as_unknown, settle_locked as settle_owed_deliveries};
#[cfg(test)]
pub(crate) use interrupt_grace::{expect_interrupt_echo, note_user_interrupt, InterruptedTurn};
pub(crate) use stuck_turns::{close_after_session_paused, observe as observe_agent_status, spawn_stuck_turn_sweeper, sweep as sweep_stuck_turns};

pub use crate::lc_error::{LcError, LcResult, RunExit};

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

async fn client_for_run(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run: &db::Run) -> LcResult<RunClient> {
    let no_client = || {
        LcError::Upstream(format!(
            "no Herdr session is available for run `{}`",
            run.id
        ))
    };
    // #708：移交出去的專案一個 pane RPC 都不打（keys／text／interrupt／login／搬 tab……都經過這裡）。
    crate::handoff::refuse(app.db(), &run.bot_id).await?;
    let host = db::bot_host(app.db(), &run.bot_id).await.map_err(up)?;
    let fence = app.hosts().fence(&host).await.ok_or_else(no_client)?;
    // Reuse #594's exact-fence resolver: it verifies the run still names this host before and after
    // session selection and gets the client from the captured HostConn. Do not look it up again by name.
    let client = client_for_run_with_host_fence(app, run, &fence)
        .await
        .map_err(|error| match error {
            LcError::Conflict(body)
                if body.get("reason").and_then(Value::as_str) == Some("host_superseded") =>
            {
                LcError::conflict(
                    "host_changed",
                    json!({"run_id": run.id, "retryable": true, "sent": false}),
                )
            }
            other => other,
        })?;
    Ok(RunClient { client, fence })
}

async fn client_for_run_with_host_fence(
    app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess),
    run: &db::Run,
    fence: &crate::hosts::HostFence,
) -> LcResult<HerdrClient> {
    let host = db::bot_host(app.db(), &run.bot_id).await.map_err(up)?;
    let stale = || {
        LcError::conflict(
            "host_superseded",
            json!({"bot_id": run.bot_id, "host": fence.conn().name}),
        )
    };
    if host != fence.conn().name || !app.hosts().is_current(fence).await {
        return Err(stale());
    }
    let session = if let Some(session) = run.herdr_session.as_deref().filter(|s| !s.is_empty()) {
        session.to_string()
    } else {
        let bot = db::bot(app.db(), &run.bot_id)
            .await
            .map_err(up)?
            .ok_or_else(|| LcError::NotFound("bot".into()))?;
        app.session_for_bot_with_host_fence(&bot, &host, fence)
            .await
            .ok_or_else(stale)?
    };
    let client = match app.herdr_for_host_fence(fence, &session).await {
        Some(client) => client,
        None => {
            // A configured fence with an unusable run session is the old upstream failure, not
            // evidence that this host generation was superseded. Recheck authority so a stale
            // fence still wins when the client lookup raced a reconnect or repoint.
            if db::bot_host(app.db(), &run.bot_id).await.map_err(up)? == host
                && app.hosts().is_current(fence).await
            {
                return Err(LcError::Upstream(format!(
                    "no Herdr session is available for run `{}`",
                    run.id
                )));
            }
            return Err(stale());
        }
    };
    if db::bot_host(app.db(), &run.bot_id).await.map_err(up)? != host
        || !app.hosts().is_current(fence).await
    {
        return Err(stale());
    }
    Ok(client)
}
