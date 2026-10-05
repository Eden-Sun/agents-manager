//! supervisor 對其他 feature 的窄介面（crate 拆分第 3 步 P6，行為不變）。
//!
//! supervisor 的 production 程式碼不再直接 `crate::lifecycle::…`／`crate::quota::…`／`crate::mission::…`／`crate::judge::…`
//! 等呼叫別的 feature：每個外部能力是這裡的一個小 trait（一個領域一個），由 `App` 實作在 `app_ports_p6.rs`，
//! 實作逐行委派給原本被呼叫的函式，所以回合、額度、任務、鎖與事件的語意不變。呼叫端寫成 `app.stop_bot(&id)`，
//! 跟原本 `lifecycle::stop_bot(app, &id)` 同一個 await 點、同一個錯誤型別。
//!
//! 為什麼不是 `am-ports` 的 `TurnControl`／`QuotaAccess`／`EventSink`：那份合約以 am-core 的值型別（`PromptRequest`、`QuotaKey`、
//! `EventEnvelope`）表達，而 supervisor 現行用的是帶 `LcError`／`PromptOut`／`db::Bot`／`quota::LimitHit` 的細節（含「讀不到」與
//! 「409 排隊」的區分）；硬塞進合約會丟掉這些分類。這些 trait 是合約之上的 daemon 側過渡介面：型別（`LcError`、`db::*`、`quota::*`）
//! 搬進 am-core／am-store 之後，簽名換成合約型別、trait 搬進 `am-ports`（`crates/am-ports/src/supervisor.rs`）。
//!
//! 刻意不做的事：不提供「包住整個 App 的 context」，每個 trait 只含一個領域的方法；`async fn in trait` 只用在這些具體實作的
//! 靜態呼叫（`App` 是唯一實作者），回傳的 future 沿用原函式的 `Send`。

#![allow(async_fn_in_trait)]

use crate::config::IdentityCfg;
use crate::db;
use crate::lifecycle::{LcResult, PromptOut, Revoked, StartOpts};
use crate::quota::{LimitHit, Quota};
use serde_json::Value;
use std::future::Future;
use std::time::Duration;

/// lifecycle：啟停、送字、排隊撤回。語意＝同名的 `crate::lifecycle` 函式。
pub trait TurnOps {
    async fn start_bot(&self, bot_id: &str) -> LcResult<String>;
    async fn start_bot_locked_with(&self, bot_id: &str, opts: StartOpts) -> LcResult<String>;
    async fn stop_bot(&self, bot_id: &str) -> LcResult<bool>;
    async fn stop_bot_locked_if_idle(&self, bot_id: &str) -> LcResult<bool>;
    async fn prompt_control_plane(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>) -> LcResult<PromptOut>;
    async fn prompt_relayed(
        &self,
        bot_id: &str,
        text: &str,
        client_request_id: &str,
        attachment_ids: &[String],
        relay_from: Option<&str>,
    ) -> LcResult<PromptOut>;
    async fn prompt_relayed_queueable(&self, bot_id: &str, text: &str, client_request_id: &str, relay_from: Option<&str>) -> LcResult<PromptOut>;
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
    ) -> anyhow::Result<db::Message>;
    async fn revoke_queued_turn(&self, turn_id: &str, why: &str) -> anyhow::Result<bool>;
    async fn announce_revoked(&self, turn_id: &str, revoked: Revoked);
    async fn assignment_withdrawal(&self, turn_id: &str) -> anyhow::Result<Option<String>>;
    fn schedule_flush_retry(&self, bot_id: &str, delay: Duration);
    /// 排著的這一則被額度擋住嗎（`lifecycle::quota_hold::blocking_hit`）。
    async fn blocking_quota_hit(&self, bot: &db::Bot, turn_id: &str) -> Option<LimitHit>;
}

/// quota：撞限、重置時間、帳號與模型解析。語意＝同名的 `crate::quota` 函式。
pub trait QuotaOps {
    async fn try_limit_hit_for_bot(&self, bot: &db::Bot) -> anyhow::Result<Option<LimitHit>>;
    async fn next_reset_for_bot(&self, bot: &db::Bot) -> Option<String>;
    async fn billing_identity(&self, bot: &db::Bot) -> anyhow::Result<Option<String>>;
    async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String;
    async fn seed_limit_hit(&self, host: &str, base: &str, until: &str, message: &str, bucket: Option<String>) -> bool;
    async fn running_model(&self, bot: &db::Bot) -> Option<String>;
    async fn limit_cleared_since(&self, bot: &db::Bot, since: chrono::DateTime<chrono::Utc>) -> bool;
    /// `GET /api/supervisor` 的 `quota` 欄（全部讀數的快照）。
    async fn quota_snapshot_json(&self) -> Value;
    /// 記憶體裡這把 key 的讀數（key 要是 `quota::quota_key` 算出來的完整 key）。
    async fn quota_reading(&self, key: &str) -> Option<Quota>;
}

/// mission：派工身分候選、暫停、事件、背景掃描。語意＝同名的 `crate::mission` 函式。
pub trait MissionOps {
    async fn mission(&self, mission_id: &str) -> anyhow::Result<Option<crate::mission::store::Mission>>;
    async fn mission_candidates(&self, host: &str, kind: &str) -> anyhow::Result<Vec<(String, bool, Option<Quota>)>>;
    async fn mission_billing_identity(&self, host: &str, bot: &db::Bot) -> anyhow::Result<Option<String>>;
    async fn mission_add_event(&self, mission_id: &str, kind: &str, text: &str, relay_from: Option<&str>, payload: &Value) -> anyhow::Result<()>;
    /// 在呼叫端已開的交易裡暫停任務（`mission::store::pause_on`）。
    async fn mission_pause_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        mission_id: &str,
        reason: &str,
        detail: Option<&str>,
        text: &str,
        payload: &Value,
    ) -> anyhow::Result<bool>;
    async fn ensure_can_assign(&self, mission_id: &str, role: &str) -> Result<(), crate::lifecycle::LcError>;
    async fn mission_next_json(&self, mission_id: &str) -> Value;
    async fn mission_wake_stalled(&self);
    async fn mission_sweep_closed_temp_bots(&self);
}

/// judge（Jev 影子報告、卡住掃描、撞單排程）。語意＝同名的 `crate::judge` 函式。
pub trait JudgeOps {
    async fn judge_shadow_settled(&self, assignment_id: &str, bot_id: &str, turn_id: Option<&str>, turn_status: &str, result: Option<&str>);
    fn judge_stuck_sweep(&self);
    async fn judge_schedule_assignment(&self, assignment_id: &str);
}

/// 對主機、行程與其他排程器的唯讀觀察／觸發。語意＝同名的原函式。
pub trait HostProbes {
    async fn pane_shows_login_problem(&self, run: &db::Run) -> Option<bool>;
    /// `memproc::dump`：整台（或遠端主機）的行程樹文字。
    async fn process_dump(&self, host: &str) -> anyhow::Result<String>;
    async fn identity_for_host(&self, host: &str, name: &str) -> Option<IdentityCfg>;
    /// 線上版本落後 main 的資訊（`deploy_now::behind` 對 `deploy_now::Ctx::of(app)` 的 repo 與線上 sha）。
    async fn deploy_behind(&self) -> Result<Value, String>;
    /// 版本發布分流的健康探針（`release_triage::issue::health_probe`）；設定由呼叫端讀好傳入。
    async fn release_triage_health_probe(&self, cfg: &crate::config::ReleaseTriageCfg) -> Option<Value>;
    async fn due_actions_snapshot(&self) -> Value;
    fn primary_keep_warm_tick(&self);
    /// 帶重啟的長命迴圈（`background_loop::restart_loop`，以 App 的 shutdown token 結束）。
    async fn restart_loop<F, Fut>(&self, name: &'static str, factory: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static;
    /// 帶重啟的背景迴圈（`background_loop::spawn_restartable`）。
    fn spawn_restartable<F, Fut>(&self, name: &'static str, factory: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static;
    async fn is_share_bot(&self, bot_id: &str) -> anyhow::Result<bool>;
}

/// 本機記憶體帳的唯讀觀察（只讀 `&App`）：背景工作帳（`background_jobs`）與「現在換版」的放寬旗標（`deploy_wait`）。
pub trait LocalAccountView {
    fn background_jobs_known(&self, run_id: &str) -> Option<u32>;
    fn background_jobs_duration(&self, run_id: &str) -> Option<(i64, i64, bool)>;
    /// 使用者按「現在換版」要放寬的那一筆核准（`deploy_wait::user_escalated_for`）。
    fn deploy_user_escalated_for(&self, approval: &crate::supervisor::store::Approval) -> bool;
}

#[cfg(test)]
mod tests {
    /// supervisor 的 production 程式碼（不含 `#[cfg(test)]` 的項目）不再直接呼叫其他 feature 的函式：這些呼叫只准出現在
    /// `app_ports_p6.rs`。純值型別／純函式（`quota::Bucket`、`lifecycle::LcError`、`mission::pick`…）不在此限，它們是之後搬進
    /// am-core 的東西，不是「呼叫另一個 feature」。
    const SOURCES: &[(&str, &str)] = &[
        ("api.rs", include_str!("api.rs")),
        ("bot_requests.rs", include_str!("bot_requests.rs")),
        ("cli_refresh.rs", include_str!("cli_refresh.rs")),
        ("controller.rs", include_str!("controller.rs")),
        ("failover.rs", include_str!("failover.rs")),
        ("health.rs", include_str!("health.rs")),
        ("idle_sleep.rs", include_str!("idle_sleep.rs")),
        ("incidents.rs", include_str!("incidents.rs")),
        ("maintenance.rs", include_str!("maintenance.rs")),
        ("mod.rs", include_str!("mod.rs")),
        ("remote.rs", include_str!("remote.rs")),
        ("responder.rs", include_str!("responder.rs")),
        ("responder_api.rs", include_str!("responder_api.rs")),
        ("role_faults.rs", include_str!("role_faults.rs")),
        ("setup.rs", include_str!("setup.rs")),
        ("timing.rs", include_str!("timing.rs")),
        ("watchdog.rs", include_str!("watchdog.rs")),
        ("../supervisor_owned.rs", include_str!("../supervisor_owned.rs")),
        ("../supervisor_evidence.rs", include_str!("../supervisor_evidence.rs")),
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
    const FORBIDDEN_CALLS: &[&str] = &[
        "lifecycle::start_bot(",
        "lifecycle::start_bot_locked_with(",
        "lifecycle::stop_bot(",
        "lifecycle::stop_bot_locked_if_idle(",
        "lifecycle::prompt(",
        "lifecycle::prompt_relayed(",
        "lifecycle::prompt_relayed_queueable(",
        "lifecycle::prompt_control_plane(",
        "lifecycle::insert_message(",
        "lifecycle::revoke_queued_turn(",
        "lifecycle::announce_revoked(",
        "lifecycle::assignment_withdrawal(",
        "lifecycle::schedule_flush_retry(",
        "quota_hold::blocking_hit(",
        "quota::try_limit_hit_for_bot(",
        "quota::limit_hit_for_bot(",
        "quota::next_reset_for_bot(",
        "quota::billing_identity(",
        "quota::quota_base_for_host(",
        "quota::seed_limit_hit(",
        "quota::running_model(",
        "quota::limit_cleared_since(",
        "quota::clear_limit_hit(",
        "quota::snapshot(",
        "mission::candidates(",
        "mission::billing_identity_named(",
        "mission::workflow::",
        "mission::api::",
        "mission::store::get(",
        "mission::store::add_event(",
        "mission::store::pause_on(",
        "judge::report::",
        "judge::stuck::",
        "judge::collision::",
        "tui_prompts::shows_login_problem(",
        "memproc::dump(",
        "tools::identity_for_host(",
        "deploy_now::",
        "deploy_wait::",
        "due_actions::",
        "primary_keep_warm::",
        "background_loop::",
        "background_jobs::",
        "share::store::",
        "release_triage::issue::",
    ];

    #[test]
    fn supervisor_production_code_reaches_other_features_only_through_ports() {
        let mut offenders = Vec::new();
        for (file, src) in SOURCES {
            let mask = test_mask(src);
            for (n, line) in src.lines().enumerate() {
                if mask[n] || line.trim_start().starts_with("//") {
                    continue;
                }
                for pat in FORBIDDEN_CALLS {
                    if line.contains(pat) {
                        offenders.push(format!("{file}:{}: {pat}  ← {}", n + 1, line.trim()));
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "這些呼叫要走 supervisor::ports（由 app_ports_p6.rs 委派）：\n{}", offenders.join("\n"));
    }

    /// 反向確認：護欄本身不是空的——被擋的樣式真的會在 adapter 裡出現（改名或搬走時這條會先紅，提醒更新清單）。
    #[test]
    fn the_adapter_still_calls_what_the_guard_forbids_elsewhere() {
        let adapter = include_str!("../app_ports_p6.rs");
        for pat in ["lifecycle::stop_bot(", "quota::try_limit_hit_for_bot(", "mission::workflow::wake_stalled(", "judge::report::shadow_settled(", "memproc::dump("] {
            assert!(adapter.contains(pat), "{pat} 應該在 app_ports_p6.rs");
        }
        assert!(test_mask("fn a() {}\n#[cfg(test)]\nmod t {\n    fn b() {}\n}\nfn c() {}\n") == vec![false, true, true, true, true, false]);
    }
}
