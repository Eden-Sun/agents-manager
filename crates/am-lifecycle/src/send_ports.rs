//! am-turn-send（`lifecycle/{prompt,queue,delivery,send_now,start_send,busy_send}.rs`）對其他 feature 的窄介面
//! （crate 拆分第 3 步 P4send，行為不變）。
//!
//! 這六個檔的 production 程式碼不再直接 `crate::supervisor::…`／`crate::handoff::…`／`crate::share::store::…`／
//! `crate::attach::…`／`crate::codex_*`／`crate::trust`／`crate::events` 等呼叫別的 feature：每個外部能力是這裡的一個小 trait
//! （一個領域一個），由 `App`／`SqlitePool`／交易連線實作在 `app_ports_p4send.rs`，實作逐行委派給原本被呼叫的函式，
//! 所以送出、排隊、冪等的語意（鎖範圍、await 點、錯誤型別）完全不變。
//!
//! 事件、bot 鎖走 `am-ports` 合約（`EventSink`／`TurnEvents`／`BotLock`），不在這裡重定義：合約沒有的能力才寫成本檔的 trait。
//! 合約缺的（見回報）：pane 的 `send_keys`／`read_ansi`／`agent_get`／`size`／`viewport_rows`、`HostFence` 是否仍為當前世代。
//!
//! 刻意不做的事：不提供「包住整個 App 的 context」，每個 trait 只含一個領域；`async fn in trait` 只用在這些具體實作的靜態呼叫。
//! 方法名跟 `App` 的 inherent 方法不同名，免得 inherent 優先、呼叫端悄悄繞過（護欄測試也擋）。

#![allow(async_fn_in_trait)]

use crate::attach::Attachment;
use crate::codex_history::Mark;
use crate::db;
use crate::lifecycle::{LcError, LcResult};
use am_ports::{EventSink, TurnEvents};
use anyhow::Result;
use serde_json::Value;

/// A maintenance window value at the send boundary; owned here so lifecycle does not depend on supervisor.
#[derive(Debug, Clone)]
pub struct WindowHeld {
    pub resource: &'static str,
    pub owner: String,
    pub fence: i64,
    pub expires_at: String,
}

impl WindowHeld {
    pub fn detail(&self) -> Value {
        let now = crate::db::now();
        let retry_after_secs = self.retry_after_secs(&now);
        serde_json::json!({
            "reason": "maintenance_window",
            "resource": self.resource,
            "held_by": self.owner,
            "fence": self.fence,
            "expires_at": self.expires_at,
            "retry_after_secs": retry_after_secs,
            "retryable": true,
            "sent": false,
            "message": format!("{} 正在進行維護（{} 窗口），到 {} 為止不送新的 prompt；窗口關閉或過期就自動恢復。", self.owner, self.resource, self.expires_at),
        })
    }

    pub fn retry_after_secs(&self, now: &str) -> i64 {
        match (chrono::DateTime::parse_from_rfc3339(now), chrono::DateTime::parse_from_rfc3339(&self.expires_at)) {
            (Ok(now), Ok(expires)) => (expires - now).num_seconds().max(0),
            _ => 0,
        }
    }

    pub fn refusal(&self) -> LcError {
        LcError::Conflict(self.detail())
    }
}

pub const UNREADABLE_RETRY_SECS: i64 = 10;

#[derive(Debug, Clone)]
pub struct WindowUnreadable {
    pub resource: &'static str,
    pub error: String,
}

impl WindowUnreadable {
    pub fn detail(&self) -> Value {
        serde_json::json!({
            "reason": "maintenance_state_unavailable",
            "resource": self.resource,
            "retry_after_secs": UNREADABLE_RETRY_SECS,
            "retryable": true,
            "sent": false,
            "message": format!("讀不到 {} 維護窗口的狀態，所以不送新的 prompt（不確定有沒有窗口）；稍後原樣重送即可。", self.resource),
        })
    }

    pub fn refusal(&self) -> LcError {
        LcError::Unavailable(self.detail())
    }
}

#[derive(Debug, Clone)]
pub struct AssignmentState {
    pub id: String,
    pub status: String,
    pub review_reason: Option<String>,
    pub error: Option<String>,
}

/// 維護窗口（原 `supervisor::maintenance`）。
pub trait MaintenancePort {
    /// 讀不到窗口時呼叫端的重試間隔（秒）：`supervisor::maintenance::UNREADABLE_RETRY_SECS`。
    const UNREADABLE_RETRY_SECS: i64;
    fn window_held(&self) -> impl std::future::Future<Output = std::result::Result<Option<WindowHeld>, WindowUnreadable>> + Send;
}

/// 閒置休眠的叫醒（原 `supervisor::idle_sleep`）。
pub trait IdleSleepPort {
    fn idle_sleep_wake<'a>(&'a self, bot_id: &'a str, why: &'a str) -> impl std::future::Future<Output = Result<bool>> + Send + 'a;
    /// 呼叫端已握著這顆 bot 的鎖。
    fn idle_sleep_wake_locked<'a>(&'a self, bot_id: &'a str, why: &'a str) -> impl std::future::Future<Output = Result<bool>> + Send + 'a;
}

/// supervisor／交辦的讀庫（原 `supervisor::store`、`supervisor_owned`）。實作在 `SqlitePool` 上。
pub trait SupervisorSendRepo {
    fn assignment_by_turn<'a>(&'a self, turn_id: &'a str) -> impl std::future::Future<Output = Result<Option<AssignmentState>>> + Send + 'a;
    fn load_owned(&self) -> impl std::future::Future<Output = Result<crate::projection::Owned>> + Send;
}

/// 專案移交（原 `handoff`）。實作在 `SqlitePool` 上。
pub trait HandoffSendRepo {
    fn bot_handed_off_to<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = Result<Option<String>>> + Send + 'a;
    /// `handoff::refuse`：移交出去的 bot 不收這則。
    fn refuse_handed_off<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = LcResult<()>> + Send + 'a;
}

/// 分享 bot（原 `share::store`）。實作在 `SqlitePool` 上。
pub trait ShareSendRepo {
    fn resolve_share_token<'a>(&'a self, token: &'a str) -> impl std::future::Future<Output = std::result::Result<Option<String>, sqlx::Error>> + Send + 'a;
    fn touch_share<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = ()> + Send + 'a;
    fn is_share_bot<'a>(&'a self, bot_id: &'a str) -> impl std::future::Future<Output = std::result::Result<bool, sqlx::Error>> + Send + 'a;
}

/// 附件（原 `attach::{resolve, bind}`）。
pub trait AttachSendPort {
    async fn resolve_attachments(&self, bot_id: &str, ids: &[String]) -> Result<Vec<Attachment>>;
    async fn bind_attachments(&self, message_id: &str, items: &[Attachment]) -> Result<()>;
}

/// 附件，寫在呼叫端的交易裡（原 `attach::bind_tx`）。
pub trait AttachTxPort {
    async fn bind_attachments_tx(&mut self, message_id: &str, items: &[Attachment]) -> Result<()>;
}

/// 附件，在呼叫端的交易連線上解除綁定（原 `attach::unbind_message`）。
pub trait AttachConnPort {
    async fn unbind_attachment_message(&mut self, msg_id: &str, turn_id: &str) -> Result<()>;
}

/// pane 的 watch（原 `events::unwatch_pane_on_session`）。
pub trait PaneWatchPort {
    fn unwatch_pane_on_session(
        &self,
        host: &str,
        session: &str,
        pane_id: &str,
    ) -> impl std::future::Future<Output = ()> + Send;
}

/// codex 的選單／歷史（原 `codex_live`、`codex_update`、`codex_model_migration`、`codex_history`）。
pub trait CodexSendPort {
    async fn observe_codex_screen(&self, run: &db::Run, screen: &str);
    /// `codex_live::close_picker`：關掉 pane 裡的選單（只送 Escape 類的鍵，回有沒有關成）。
    async fn close_codex_picker(&self, client: &crate::herdr::HerdrClient, pane_id: &str) -> bool;
    fn codex_running_version(&self, run_id: &str) -> Option<String>;
    fn codex_history_mark<'a>(&'a self, bot: &'a db::Bot, run: &'a db::Run) -> impl std::future::Future<Output = Option<Mark>> + Send + 'a;
    fn codex_prompt_landed<'a>(
        &'a self,
        mark: &'a Mark,
        conn: &'a mut Option<Box<dyn crate::codex_history::HistoryConn>>,
        text: &'a str,
    ) -> impl std::future::Future<Output = bool> + Send + 'a;
}

/// 送出前後會碰到的其他 feature（原 `dangerous_rm`、`primary_keep_warm`）。
pub trait SendEnvPort {
    async fn dangerous_rm_notify_once(&self, run: &db::Run, rm: &crate::tui_prompts::DangerousRm) -> bool;
    async fn note_keep_warm_prompt(&self, bot_id: &str, client_request_id: &str);
}

/// 發一則 JSON object 事件（原 `app.emit(kind, json!(…))`）。`payload` 一定是 object，失敗只記 log（原本的 emit 沒有失敗路徑）。
pub async fn emit_object<E: EventSink>(events: &E, kind: &str, bot_id: Option<&str>, payload: Value) {
    let envelope = am_core::EventEnvelope { kind: kind.to_string(), bot_id: bot_id.map(str::to_string), payload_json: payload.to_string() };
    if let Err(error) = events.emit(envelope).await {
        tracing::warn!(kind, error = ?error, "event not emitted");
    }
}

/// 重算並送出這顆 bot 的現況投影（原 `app.emit_bot_status`）。
pub async fn bot_status<E: EventSink>(events: &E, bot_id: &str) {
    if let Err(error) = events.bot_status_changed(bot_id).await {
        tracing::warn!(bot = bot_id, error = ?error, "bot status not emitted");
    }
}

/// DB commit 之後發回合狀態（原 `lifecycle::emit_turn`）。
pub async fn turn_changed<E: TurnEvents>(events: &E, turn_id: &str) {
    if let Err(error) = events.turn_changed(&turn_id.to_string()).await {
        tracing::warn!(turn = turn_id, error = ?error, "turn change not announced");
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    /// 受護欄管的 send 組 production 檔。
    const SOURCES: &[(&str, &str)] = &[
        ("lifecycle/prompt.rs", include_str!("lifecycle/prompt.rs")),
        ("lifecycle/queue.rs", include_str!("lifecycle/queue.rs")),
        ("lifecycle/delivery.rs", include_str!("lifecycle/delivery.rs")),
        ("lifecycle/send_now.rs", include_str!("lifecycle/send_now.rs")),
        ("lifecycle/start_send.rs", include_str!("lifecycle/start_send.rs")),
        ("lifecycle/busy_send.rs", include_str!("lifecycle/busy_send.rs")),
    ];

    /// 每一行是否落在 `#[cfg(test)]` 項目裡（從屬性那行到項目的大括號收尾）。
    fn test_mask(src: &str) -> Vec<bool> {
        let lines: Vec<&str> = src.lines().collect();
        let mut mask = vec![false; lines.len()];
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim_start().starts_with("#[cfg(test)]")
                || lines[i].trim_start().starts_with("#[cfg(all(test, feature = \"daemon-test-harness\"))]")
            {
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

    /// 以「呼叫」的形狀擋：函式名後面接 `(`（`UNREADABLE_RETRY_SECS` 是常數，照名字擋）。
    const FORBIDDEN: &[&str] = &[
        "supervisor::maintenance::window_held(",
        "supervisor::maintenance::UNREADABLE_RETRY_SECS",
        "supervisor::idle_sleep::wake(",
        "supervisor::idle_sleep::wake_locked(",
        "supervisor::store::assignment_by_turn(",
        "supervisor_owned::load(",
        "handoff::bot_handed_off_to(",
        "handoff::refuse(",
        "share::store::resolve(",
        "share::store::touch(",
        "share::store::is_share_bot(",
        "attach::resolve(",
        "attach::bind(",
        "attach::bind_tx(",
        "attach::unbind_message(",
        "events::unwatch_pane_on_session(",
        "codex_model_migration::observe_screen(",
        "codex_live::close_picker(",
        "codex_update::running_version_of(",
        "codex_history::mark(",
        "codex_history::prompt_landed(",
        "dangerous_rm::notify_once(",
        "primary_keep_warm::note_prompt(",
        // 事件、bot 鎖走 am-ports 合約的 adapter。
        "emit_turn(",
        "app.emit(",
        "app.emit_bot_status(",
        "app.bot_lock(",
    ];

    #[test]
    fn send_production_code_reaches_other_features_only_through_ports() {
        let mut offenders = Vec::new();
        for (file, src) in SOURCES {
            let mask = test_mask(src);
            for (n, line) in src.lines().enumerate() {
                if mask[n] || line.trim_start().starts_with("//") {
                    continue;
                }
                for pat in FORBIDDEN {
                    if line.contains(pat) {
                        offenders.push(format!("{file}:{}: {pat}  ← {}", n + 1, line.trim()));
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "這些呼叫要走 send_ports／am-ports adapter（由 app_ports_p4send.rs 委派）：\n{}", offenders.join("\n"));
    }

    /// 反向確認：護欄禁的呼叫真的在 adapter 裡（改名或搬走時這條先紅，提醒更新清單）。
    #[test]
    fn the_adapter_still_calls_what_the_guard_forbids_elsewhere() {
        let adapter = include_str!("../../../daemon/src/app_ports_p4send.rs");
        let lifecycle_adapter = include_str!("ports_impl.rs");
        for pat in [
            "maintenance::window_held(",
            "maintenance::UNREADABLE_RETRY_SECS",
            "idle_sleep::wake(",
            "idle_sleep::wake_locked(",
            "store::assignment_by_turn(",
            "supervisor_owned::load(",
            "handoff::bot_handed_off_to(",
            "handoff::refuse(",
            "store::resolve(",
            "store::touch(",
            "store::is_share_bot(",
            "attach::resolve(",
            "attach::bind(",
            "events::unwatch_pane_on_session(",
            "observe_screen(",
            "close_picker(",
            "running_version_of(",
            "codex_history::mark(",
            "prompt_landed(",
            "notify_once(",
            "note_prompt(",
            "bot_lock(",
        ] {
            assert!(adapter.contains(pat), "adapter 不再含 {pat}");
        }
        for pat in ["attach::bind_tx(", "attach::unbind_message("] {
            assert!(lifecycle_adapter.contains(pat), "am-lifecycle 的交易 adapter 不再委派 {pat}");
        }
    }

    #[test]
    fn test_mask_skips_cfg_test_items() {
        assert!(test_mask("fn a() {}\n#[cfg(test)]\nmod t {\n    fn b() {}\n}\nfn c() {}\n") == vec![false, true, true, true, true, false]);
        assert!(test_mask("fn a() {}\n#[cfg(all(test, feature = \"daemon-test-harness\"))]\nmod t {\n    fn b() {}\n}\nfn c() {}\n") == vec![false, true, true, true, true, false]);
    }
}
