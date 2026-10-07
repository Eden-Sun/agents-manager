//! hook／事件入口與對帳（P8）對其他 feature 的窄介面（crate 拆分第 3 步，行為不變）。
//!
//! `hookrecv`／`events`／`reconcile`／`child_*`… 的 production 程式碼不再直接 `crate::lifecycle::…`、`crate::quota::…`、
//! `crate::supervisor::…`、`crate::panes::…` 等呼叫別的 feature；入口與對帳兩組之間（`events` ⇄ `reconcile`）也不再互相直呼。
//! 每個外部能力是這裡的一個小 trait（一個領域一個），由 `App`／`SqlitePool`／交易連線實作在 `app_ports_p8.rs`，
//! 實作逐行委派給原本被呼叫的函式——**同一個 await 點、同一個鎖範圍、同一個錯誤型別**，所以重送、同一回合鎖與耐久事件
//! 的「只發生一次」語意完全沒動：這裡沒有任何新邏輯、新 await、新鎖。呼叫端寫成 `app.mark_run_exited(&id, why)`，
//! 跟原本 `lifecycle::mark_run_exited(app, &id, why)` 同一步。
//!
//! 為什麼不是 `am-ports` 的 `TurnControl`／`TurnEvents`／`EventSink`：合約以 am-core 的值型別表達，而入口這邊用的是帶
//! `LcError`／`RunExit`／`Outcome`／交易連線的細節（hook 重送的「欠著的收尾」、`mark_run_exited` 的四種結果、
//! 在同一個寫入交易裡設回合狀態），硬塞進合約會丟掉這些分類。這些 trait 是合約之上的 daemon 側過渡介面：型別搬進
//! am-core／am-store 之後，簽名換成合約型別、trait 搬進 `am-ports`。
//!
//! 刻意不做的事：不提供「包住整個 App 的 context」，每個 trait 只含一個領域；`async fn in trait` 只用在這些具體實作的
//! 靜態呼叫。方法名刻意跟 `App` 的 inherent 方法不同名，免得 inherent 優先、呼叫端悄悄繞過這道介面（護欄測試也擋）。
//!
//! 還沒切掉的邊（理由見回報）：型別層（`LcError`、`RunExit`、`turn_controller::{Outcome, NativeEvidence}`、`fence::*`、
//! `db::*`、`herdr::{HerdrClient, AgentInfo, Event}`、`state::{App, WsEvent}`）；純規則／純解析（`tui_prompts`、`agy_support`、
//! `quota_from_statusline`、`is_quota_exhaustion`、`pasted_content`…）；下層 adapter（`config`、`projection`、`config_audit`、
//! `shared_host`、`pane_identity`、`hosts::sh_quote`、`transcript_read`、`private_files`、`local_sh`、`linux_proc`、`probe_ws`）。

#![allow(async_fn_in_trait)]

use crate::db;
use crate::handoff::Footprint;
use crate::herdr_maintenance::Window;
use crate::lifecycle::{InterruptEvidence, InterruptFailureEvidence, LcError, LcResult, PromptOut, RunExit, StartOpts};
use crate::lifecycle::fence::{EventIdentity, Ownership};
use crate::lifecycle::turn_controller::{NativeEvidence, Outcome};
use crate::panes::ScanOutcome;
use crate::quota::Quota;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;

/// lifecycle：回合的收尾、排程、啟停與訊息。語意＝同名的 `crate::lifecycle` 函式。
pub(crate) trait TurnCommands {
    async fn emit_message_added(&self, bot_id: &str, message: db::Message);
    async fn emit_turn(&self, turn_id: &str);
    fn schedule_flush_queued(&self, bot_id: &str);
    fn schedule_deferred_live(&self, bot_id: &str);
    fn schedule_codex_notice_capture(&self, bot_id: &str, run_id: &str);
    fn poke_resume_nudge(&self, bot_id: &str);
    async fn cancel_stall(&self, run_id: &str);
    async fn arm_fallback(&self, run_id: &str, bot_id: &str);
    async fn arm_progress(&self, run_id: &str, bot_id: &str, turn_id: &str);
    async fn has_progress_poller(&self, run_id: &str) -> bool;
    async fn arm_stall(&self, run_id: &str, bot_id: &str, turn_id: &str);
    async fn begin_external_turn(&self, run: &db::Run);
    /// `mark_run_exited`：結束 run 的唯一寫入；四種結果（記成／早已結束／沒寫進去／欠著的收尾）呼叫端要分得開。
    async fn mark_run_exited(&self, run_id: &str, reason: &str) -> RunExit;
    async fn context_lost(&self, bot: &db::Bot, why: &str, failed_session: Option<&str>) -> LcResult<()>;
    async fn retire_context_lost(&self, bot_id: &str, session_id: &str);
    async fn settle_interruption(&self, bot_id: &str, evidence: InterruptEvidence) -> Result<()>;
    async fn settle_owed_deliveries(&self, bot_id: &str) -> Result<()>;
    async fn settle_interrupt_echo(
        &self,
        bot_id: &str,
        run_id: &str,
        ev: &InterruptFailureEvidence<'_>,
        in_flight: Option<&db::Turn>,
    ) -> Result<bool>;
    async fn start_bot(&self, bot_id: &str) -> LcResult<String>;
    async fn start_bot_locked_with(&self, bot_id: &str, opts: StartOpts) -> LcResult<String>;
    async fn start_bot_locked(&self, bot_id: &str) -> LcResult<String> {
        self.start_bot_locked_with(bot_id, StartOpts::default()).await
    }
    async fn resume_after_boot(&self, host: &str) -> usize;
    async fn adopt_unbound_send_nows(&self, boot: &str) -> bool;
    async fn rearm_queue_retries(&self) -> Result<usize>;
    async fn adopt_turns_of_ended_runs(&self, boot: &str) -> bool;
    async fn rearm_queued_prompt_restamps(&self) -> Result<()>;
    async fn adopt_interrupted_on_restart(&self, run: &db::Run, turn: &db::Turn) -> Result<bool>;
    fn spawn_adopted_capture(&self, run_id: &str, bot_id: &str);
    async fn sweep_stuck_turns(&self, host: Option<&str>) -> Vec<String>;
    async fn close_after_session_paused(&self, run_id: &str, expected_turn_id: &str) -> Option<String>;
    /// `lifecycle::close_pane_and_tab`：收一個 pane 與它的 tab（沒有 `App` 的依賴，但屬於 lifecycle 的啟停）。
    async fn close_pane_and_tab(&self, client: &crate::herdr::HerdrClient, workspace_id: Option<&str>, tab_id: Option<&str>, pane_id: &str);
    /// `lifecycle::observe_agent_status`（stuck-turn 偵測的觀察點）。
    fn observe_agent_status(&self, run_id: &str, agent_status: &str);
    #[allow(clippy::too_many_arguments)]
    async fn insert_message(
        &self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
    ) -> Result<db::Message>;
    async fn prompt_relayed_queueable(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>) -> LcResult<PromptOut>;
}

/// 在同一個寫入交易（連線）裡設回合狀態（原 `lifecycle::turn_controller::*`）。實作在 `SqliteConnection` 上，
/// `Transaction` 自動 deref 過去：呼叫端手上的 `tx` 就是原本傳進去的那條連線，交易邊界一點都不變。
pub(crate) trait TurnConnOps {
    async fn set_status_on(&mut self, turn_id: &str, from: &str, to: &str, why: &str) -> Result<Outcome>;
    async fn complete_with_native_evidence(&mut self, turn_id: &str, admitted: &crate::lifecycle::fence::Admitted, ev: NativeEvidence<'_>) -> Result<Outcome>;
    async fn fail_with_native_evidence(&mut self, turn_id: &str, admitted: &crate::lifecycle::fence::Admitted, ev: NativeEvidence<'_>) -> Result<Outcome>;
}

/// 在同一個寫入交易裡插訊息（原 `lifecycle::insert_message_tx`／`insert_message_relayed_tx`）。
pub(crate) trait MessageTxOps {
    #[allow(clippy::too_many_arguments)]
    async fn insert_message_tx(
        &mut self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
    ) -> Result<db::Message>;
    #[allow(clippy::too_many_arguments)]
    async fn insert_message_relayed_tx(
        &mut self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
        relay_from: Option<&str>,
    ) -> Result<db::Message>;
}

/// 事件歸屬判斷（原 `lifecycle::fence::classify`）。
pub(crate) trait TurnFenceOps {
    async fn classify_event_owner(&self, bot_id: &str, run: &db::Run, ev: EventIdentity<'_>) -> Ownership;
}

/// quota：限額讀寫與撞限標記（原 `quota::*`、`turn_error::mark_*_limit_hit`）。
pub(crate) trait QuotaCommands {
    async fn clear_limit_hit_for_bot(&self, bot: &db::Bot);
    async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String;
    /// `quota::set`。
    async fn set_quota(&self, host: &str, base: &str, q: Quota);
    async fn mark_claude_limit_hit(&self, bot: &db::Bot, line: &str) -> Result<()>;
    async fn mark_agy_limit_hit(&self, bot: &db::Bot, line: &str) -> Result<()>;
}

/// supervisor 的收件匣與擁有關係（原 `supervisor::store::*`、`supervisor_owned::load`）。實作在 `SqlitePool` 上。
pub(crate) trait SupervisorRepo {
    /// `supervisor::store::SUPERVISOR_ID`。
    const SUPERVISOR_ID: &'static str;
    async fn push_inbox(
        &self,
        event_key: &str,
        kind: &str,
        assignment_id: Option<&str>,
        bot_id: Option<&str>,
        turn_id: Option<&str>,
        payload: &Value,
    ) -> Result<Option<String>>;
    async fn load_owned(&self) -> Result<crate::projection::Owned>;
    /// `supervisor::store::sql_list(&OPEN_STATES)`：未結案交辦狀態的 SQL `IN (…)` 清單（清單只准有一份）。
    fn open_states_sql() -> String;
}

/// supervisor 對狀態變化的觀察點（原 `supervisor::idle_sleep::observe_status`）。
pub(crate) trait SupervisorSignals {
    fn observe_idle_status(&self, run_id: &str, status: &str);
}

/// 專案移交（原 `handoff::*`）。實作在 `SqlitePool` 上。
pub(crate) trait HandoffRepo {
    async fn bot_handed_off_to(&self, bot_id: &str) -> Result<Option<String>>;
    async fn handoff_footprint(&self, host: &str) -> Result<Footprint>;
}

/// 專案移交，在交易連線上讀（原 `handoff::bot_handed_off_to_on`）。
pub(crate) trait HandoffConnRepo {
    async fn bot_handed_off_to_on(&mut self, bot_id: &str) -> Result<Option<String>>;
}

/// bot／專案操作的 intent 與 pane 掃描（原 `*_intents`、`panes`、`share::store::is_share_bot`）。
pub(crate) trait BotOpsPort {
    async fn recover_restart_intents(&self, host: &str);
    async fn recover_delete_intents(&self, host: &str);
    async fn recover_promote_intents(&self, host: &str);
    async fn scan_panes_snapshot(&self, host: &str, snapshot: &Value) -> Result<ScanOutcome>;
    async fn gc_panes(&self, host: &str) -> Result<usize>;
    async fn notify_unowned_and_orphans(&self, host: &str) -> Result<usize>;
}

/// 同上，讀庫的那幾個（實作在 `SqlitePool` 上）。
pub(crate) trait BotOpsRepo {
    async fn has_open_restart_for_run(&self, host: &str, bot_id: &str, run_id: &str) -> Result<bool>;
    async fn is_share_bot(&self, bot_id: &str) -> std::result::Result<bool, sqlx::Error>;
}

/// intent 完成紀錄，寫在呼叫端的交易裡（原 `intents::record_done`）。
pub(crate) trait IntentConnRepo {
    async fn record_done_intent(&mut self, kind: &str, subject_id: &str, host: &str, payload: &Value) -> Result<String>;
}

/// 主機與 herdr 的維運（原 `herdr_version`、`herdr_maintenance`、`github`、`state` 的連線狀態）。
pub(crate) trait HostSidePort {
    async fn refresh_herdr_version(&self, host: &str);
    async fn herdr_maintenance_active(&self) -> Result<Option<Window>>;
    fn spawn_detect_github_host(&self, host: String);
    async fn emit_daemon_status(&self);
    async fn set_default_connected(&self, connected: bool);
    async fn drain_remote_coalesced(&self, host: &str, bot_id: &str) -> Result<usize>;
}

/// provider／登入／預覽的觀察點（原 `claude_live`、`login_prompt`、`codex_model_migration`、`prompt_suggestion`、`tui_prompts`）。
pub(crate) trait ProviderPort {
    async fn adopt_statusline_model(&self, run: &db::Run, payload: &Value);
    async fn login_on_auth_failure(&self, bot: &db::Bot);
    async fn login_on_turn_ok(&self, bot: &db::Bot);
    fn codex_migration_on_blocked(&self, run: &db::Run);
    fn prompt_suggestion_on_idle(&self, run: &db::Run);
    /// `tui_prompts::dismiss_if_survey`：畫面上是意見調查就代按關掉，回有沒有關。
    async fn dismiss_survey_if_shown(&self, run: &db::Run) -> bool;
}

/// 主 API 的共用 helper（原 `api::state_json`、`api::ct_eq`）。
pub(crate) trait ApiPort {
    async fn state_json(&self) -> std::result::Result<Value, LcError>;
    /// 常數時間字串比較（token 比對用，不能換成會短路的 `==`）。
    fn ct_eq(&self, a: &str, b: &str) -> bool;
}

/// 入口（`events`、`blocked_reason`）要對帳／同步時的命令（原 `reconcile::*`、`default_session::sync`、`session_paused::*`）。
/// 入口不直接呼叫對帳：事件→命令的方向由這個 trait 表達。
pub(crate) trait ReconcileCommands {
    async fn reconcile_host(&self, host: &str) -> Result<()>;
    async fn autostart_after_reconcile(&self, host: &str, reconciled: bool) -> bool;
    fn schedule_deferred_pass(&self, host: &str);
    async fn sync_default_session(&self) -> Result<()>;
    fn session_paused_on_idle(&self, run: &db::Run);
}

/// 對帳（`reconcile`、`default_session`）要入口做的事（原 `events::{watch,unwatch}_pane_on_session`、`child_retire`、
/// `child_reconcile_safety`、`spawn_hints`）。
pub(crate) trait IngressCommands {
    async fn watch_pane_on_session(&self, host: &str, session: &str, pane_id: &str);
    async fn unwatch_pane_on_session(&self, host: &str, session: &str, pane_id: &str);
    /// `child_retire::retire`。刻意不是 `async fn`：`#[track_caller]` 在 `async fn` 上抓不到呼叫端，
    /// 退役 log 的 `caller=`（issue #406）要的是「哪一行叫的」。
    #[track_caller]
    fn retire_child<'a>(
        &'a self,
        bot_id: &'a str,
        why: &'static str,
        mode: crate::child_retire::Mode,
    ) -> impl Future<Output = Result<crate::child_retire::Outcome>> + 'a;
    async fn retirement_block(&self, bot_id: &str) -> Result<Option<String>>;
    async fn prune_stale_spawn_hints(&self);
    async fn spawn_hints_for_host(&self, host: &str) -> Result<HashMap<String, String>>;
    async fn consume_spawn_hint(&self, host: &str, pane_id: &str);
}

#[cfg(test)]
mod tests {
    /// 入口那一組與對帳那一組的 production 檔（組內互相呼叫照舊，組與組之間只准走 [`super::ReconcileCommands`]／[`super::IngressCommands`]）。
    const INGRESS: &[(&str, &str)] = &[
        ("ask_answers.rs", include_str!("ask_answers.rs")),
        ("background_hook.rs", include_str!("../../crates/am-base/src/background_hook.rs")),
        ("background_jobs.rs", include_str!("background_jobs.rs")),
        ("background_loop.rs", include_str!("../../crates/am-base/src/background_loop.rs")),
        ("blocked_reason.rs", include_str!("blocked_reason.rs")),
        ("bot_state.rs", include_str!("bot_state.rs")),
        ("child_alerts.rs", include_str!("child_alerts.rs")),
        ("child_done.rs", include_str!("child_done.rs")),
        ("child_reconcile_safety.rs", include_str!("../../crates/am-base/src/child_reconcile_safety.rs")),
        ("child_retire.rs", include_str!("child_retire.rs")),
        ("child_runtime.rs", include_str!("../../crates/am-base/src/child_runtime.rs")),
        ("events.rs", include_str!("events.rs")),
        ("hook_cmd.rs", include_str!("../../crates/am-base/src/hook_cmd.rs")),
        ("hook_inbox.rs", include_str!("../../crates/am-base/src/hook_inbox.rs")),
        ("hookrecv.rs", include_str!("hookrecv.rs")),
        ("pending_question.rs", include_str!("../../crates/am-base/src/pending_question.rs")),
        ("spawn_hints.rs", include_str!("../../crates/am-base/src/spawn_hints.rs")),
        ("dangerous_rm.rs", include_str!("dangerous_rm.rs")),
    ];
    const RECONCILE: &[(&str, &str)] = &[
        ("reconcile.rs", include_str!("reconcile.rs")),
        ("autostart_revive.rs", include_str!("autostart_revive.rs")),
        ("default_session.rs", include_str!("default_session.rs")),
        ("due_actions.rs", include_str!("due_actions.rs")),
        ("session_paused.rs", include_str!("../../crates/am-base/src/session_paused.rs")),
        ("kids_cache.rs", include_str!("../../crates/am-base/src/kids_cache.rs")),
        ("read_marks.rs", include_str!("read_marks.rs")),
    ];

    /// 每一行是否落在 `#[cfg(test)]` 項目裡（從屬性那行到項目的大括號收尾）。
    fn test_mask(src: &str) -> Vec<bool> {
        let lines: Vec<&str> = src.lines().collect();
        let mut mask = vec![false; lines.len()];
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim_start().starts_with("#[cfg(test)]") {
                let (mut depth, mut seen, mut j) = (0i32, false, i);
                while j < lines.len() {
                    mask[j] = true;
                    depth += lines[j].matches('{').count() as i32 - lines[j].matches('}').count() as i32;
                    seen |= lines[j].contains('{');
                    if seen && depth <= 0 {
                        break;
                    }
                    if !seen && lines[j].trim_end().ends_with(';') && !lines[j].trim_start().starts_with("#[") {
                        break;
                    }
                    j += 1;
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        mask
    }

    /// 以「呼叫」的形狀擋：函式名後面接 `(`。
    fn feature_calls() -> Vec<String> {
        let mut v: Vec<String> = [
            "emit_message_added", "emit_turn", "schedule_flush_queued", "schedule_deferred_live", "schedule_codex_notice_capture",
            "poke_resume_nudge", "cancel_stall", "arm_fallback", "arm_progress", "arm_stall", "begin_external_turn", "mark_run_exited",
            "context_lost", "retire_context_lost", "settle_interruption", "settle_owed_deliveries", "settle_interrupt_echo",
            "start_bot", "start_bot_locked_with", "adopt_unbound_send_nows", "rearm_queue_retries", "adopt_turns_of_ended_runs",
            "rearm_queued_prompt_restamps", "adopt_interrupted_on_restart", "spawn_adopted_capture", "sweep_stuck_turns",
            "close_after_session_paused", "close_pane_and_tab", "observe_agent_status", "insert_message", "insert_message_tx",
            "insert_message_relayed_tx", "prompt_relayed_queueable",
        ]
        .iter()
        .map(|n| format!("lifecycle::{n}("))
        .collect();
        v.extend(
            [
                "lifecycle::start_send::resume_after_boot(",
                "turn_controller::set_status_on(",
                "turn_controller::complete_with_native_evidence(",
                "turn_controller::fail_with_native_evidence(",
                "fence::classify(",
                "quota::clear_limit_hit_for_bot(",
                "quota::quota_base_for_host(",
                "quota::set(",
                "turn_error::mark_claude_limit_hit(",
                "turn_error::mark_agy_limit_hit(",
                "supervisor::store::push_inbox(",
                "supervisor::store::sql_list(",
                "supervisor_owned::load(",
                "idle_sleep::observe_status(",
                "handoff::bot_handed_off_to(",
                "handoff::bot_handed_off_to_on(",
                "handoff::footprint(",
                "restart_intents::recover_host(",
                "delete_intents::recover_host(",
                "promote_intents::recover_host(",
                "restart_intents::has_open_restart_for_run(",
                "panes::scan_snapshot(",
                "panes::gc_host(",
                "panes::notify_unowned_and_orphans(",
                "share::store::is_share_bot(",
                "intents::record_done(",
                "runners::herdr_version::refresh(",
                "herdr_maintenance::active(",
                "runners::github::spawn_detect_host(",
                "state::emit_daemon_status(",
                "state::set_default_connected(",
                "claude_live::adopt_statusline_model(",
                "login_prompt::on_auth_failure(",
                "login_prompt::on_turn_ok(",
                "codex_model_migration::on_blocked(",
                "prompt_suggestion::on_idle(",
                "tui_prompts::dismiss_if_survey(",
                "api::state_json(",
                "api::ct_eq(",
                "SUPERVISOR_ID",
            ]
            .map(String::from),
        );
        v
    }

    const TO_RECONCILE: &[&str] = &[
        "reconcile::reconcile_host(",
        "reconcile::autostart_after_reconcile(",
        "reconcile::schedule_deferred_pass(",
        "default_session::sync(",
        "session_paused::on_idle(",
    ];
    const TO_INGRESS: &[&str] = &[
        "events::watch_pane_on_session(",
        "events::unwatch_pane_on_session(",
        "child_retire::retire(",
        "child_reconcile_safety::retirement_block(",
        "spawn_hints::prune_stale(",
        "spawn_hints::for_host(",
        "spawn_hints::consume(",
    ];

    fn offenders(files: &[(&str, &str)], forbidden: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for (file, src) in files {
            let mask = test_mask(src);
            for (n, line) in src.lines().enumerate() {
                if mask[n] || line.trim_start().starts_with("//") {
                    continue;
                }
                for pat in forbidden {
                    // `<SqlitePool as SupervisorRepo>::SUPERVISOR_ID` 是走介面的寫法，放行。
                    if line.contains(pat.as_str()) && !(pat == "SUPERVISOR_ID" && line.contains("SupervisorRepo")) {
                        out.push(format!("{file}:{}: {pat}  ← {}", n + 1, line.trim()));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn ingress_and_reconcile_reach_other_features_only_through_ports() {
        let all: Vec<(&str, &str)> = INGRESS.iter().chain(RECONCILE.iter()).copied().collect();
        let found = offenders(&all, &feature_calls());
        assert!(found.is_empty(), "這些呼叫要走 events::ports（由 app_ports_p8.rs 委派）：\n{}", found.join("\n"));
    }

    #[test]
    fn ingress_and_reconcile_do_not_call_each_other_directly() {
        let to_reconcile: Vec<String> = TO_RECONCILE.iter().map(|s| s.to_string()).collect();
        let to_ingress: Vec<String> = TO_INGRESS.iter().map(|s| s.to_string()).collect();
        let mut found = offenders(INGRESS, &to_reconcile);
        found.extend(offenders(RECONCILE, &to_ingress));
        assert!(found.is_empty(), "入口與對帳之間要走 ReconcileCommands／IngressCommands：\n{}", found.join("\n"));
    }

    /// 反向確認：護欄禁的呼叫真的在 adapter 裡（改名或搬走時這條先紅，提醒更新清單）。
    #[test]
    fn the_adapter_still_calls_what_the_guards_forbid_elsewhere() {
        let adapter = include_str!("app_ports_p8.rs");
        let mut pats = feature_calls();
        pats.retain(|p| p != "SUPERVISOR_ID" && p != "lifecycle::start_send::resume_after_boot(");
        for p in pats.iter().map(String::as_str).chain(TO_RECONCILE.iter().copied()).chain(TO_INGRESS.iter().copied()) {
            // adapter 在 `use crate::lifecycle::{self, …}` 之後寫 `lifecycle::foo(`，其餘寫成完整路徑；兩種都以子字串比對。
            let short = p.rsplit("::").next().unwrap_or(p);
            let module = p.trim_end_matches('(').rsplit("::").nth(1).unwrap_or("");
            let probe = format!("{module}::{short}");
            assert!(adapter.contains(&probe), "adapter 不再含 {probe}");
        }
        assert!(adapter.contains("SUPERVISOR_ID") && adapter.contains("resume_after_boot("));
    }

    #[test]
    fn test_mask_skips_cfg_test_items() {
        assert!(test_mask("fn a() {}\n#[cfg(test)]\nmod t {\n    fn b() {}\n}\nfn c() {}\n") == vec![false, true, true, true, true, false]);
    }
}
