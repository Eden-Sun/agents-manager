//! am-turn-session（`lifecycle/{start,stop,setup,agy_hook,agy_session,grok_hook,codex_steer}.rs`）對其他 feature 的窄介面
//! （crate 拆分第 3 步 P4sess，行為不變）。
//!
//! 這幾個檔的 production 程式碼不再直接 `crate::handoff::…`／`crate::share::store::…`／`crate::intents::…`／`crate::events::…`／
//! `crate::remote_purge`／`crate::preview`／`crate::claude_live`／`crate::models`／`herdr_shim`／`cargo_shim` 等呼叫別的 feature：
//! 每個外部能力是這裡的一個小 trait（一個領域一個），由 `App`／`SqlitePool` 實作在 `app_ports_p4sess.rs`，實作逐行委派給原本被呼叫的
//! 函式，所以啟停、session 建立、resume gate 與 `HostFence` 的語意（鎖範圍、await 點、錯誤型別）完全不變。
//!
//! 事件、bot 鎖走 `am-ports` 合約（`EventSink`／`TurnEvents`／`BotLock`），不在這裡重定義。合約缺的（見回報）：
//! `HostFence` 的「是否仍是當前世代」、`run_if_current`、遠端 ssh 執行、`client_for_run*`、pane 的 `agent_start`／`agent_get` 等
//! 啟停需要的 host／herdr 能力——這些仍是 P3 的 `hosts`／`herdr` 型別，語意不能改，所以沒有包。
//!
//! 刻意不做的事：不提供「包住整個 App 的 context」，每個 trait 只含一個領域；`async fn in trait` 只用在這些具體實作的靜態呼叫。

#![allow(async_fn_in_trait)]

use std::future::Future;
use am_ports::{EventSink, TurnEvents};
use anyhow::Result;
use serde_json::Value;

/// 專案移交（原 `handoff`）。實作在 `SqlitePool` 上。
pub trait HandoffSessionRepo {
    fn bot_handed_off_to<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = Result<Option<String>>> + Send + 'a;
    /// `handoff::refuse`：移交出去的 bot 不收這個動作。
    fn refuse_handed_off<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = crate::lifecycle::LcResult<()>> + Send + 'a;
}

/// 分享 bot 的工作區（原 `share::store`）。實作在 `SqlitePool` 上。
pub trait ShareSessionRepo {
    fn restricted_workspace<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = std::result::Result<Option<String>, sqlx::Error>> + Send + 'a;
    fn caged_workspace<'a>(&'a self, bot_id: &'a str) -> impl Future<Output = std::result::Result<Option<String>, sqlx::Error>> + Send + 'a;
}

/// 重啟的持久 intent（原 `intents::{prepare_restart, complete, abandon, fail}`）。實作在 `SqlitePool` 上。
pub trait RestartIntentRepo {
    async fn prepare_restart_intent(&self, subject_id: &str, host: &str, payload: &Value, ttl_secs: i64, boot: &str) -> Result<String>;
    async fn complete_intent(&self, id: &str) -> Result<bool>;
    async fn abandon_intent(&self, id: &str, why: &str) -> Result<bool>;
    async fn fail_intent(&self, id: &str, err: &str) -> Result<bool>;
}

/// pane 的 watch（原 `events::unwatch_pane_on_session`）。
pub trait PaneWatchPort {
    async fn unwatch_pane_on_session(&self, host: &str, session: &str, pane_id: &str);
}

/// 刪除 bot 時的遠端善後（原 `remote_purge`、`remote_trash`）。
pub trait RemoteCleanupPort {
    async fn record_remote_purge(&self, bot_id: &str, host: &str, ok: bool, error: Option<&str>);
    /// `remote_trash::move_in`：把遠端 bot 目錄搬進垃圾桶，回搬到哪（沒東西搬回 `None`）。
    async fn move_remote_bot_dir_to_trash(&self, conn: &crate::hosts::HostConn, bot_id: &str) -> Result<Option<String>>;
}

/// 預覽（原 `preview::stop_for_bot`）。
pub trait PreviewPort {
    /// 停掉這顆 bot 的預覽，回有沒有停到東西。
    async fn stop_preview_for_bot(&self, bot_id: &str) -> bool;
}

/// provider 與模型（原 `claude_live::start_fresh`、`models::list`）。
pub trait SessionProviderPort {
    /// run 起了一個新的 claude 行程：清掉上一個行程的即時狀態。
    fn claude_live_start_fresh(&self, run_id: &str);
    fn models_list<'a>(&'a self, host: &'a str, kind: &'a str, identity: Option<&'a str>, refresh: bool) -> impl std::future::Future<Output = Result<Value>> + Send + 'a;
}

/// 啟動時裝 shim（原 `herdr_shim`、`cargo_shim`）。
pub trait ShimInstallPort {
    fn install_local_herdr_shim(&self, bot_dir: &std::path::Path) -> std::io::Result<std::path::PathBuf>;
    fn install_local_cargo_shim(&self, bot_dir: &std::path::Path) -> std::io::Result<std::path::PathBuf>;
    /// `pkill`／`killall` 的廣域 kill 守門（#1884）。
    fn install_local_kill_shim(&self, bot_dir: &std::path::Path) -> std::io::Result<std::path::PathBuf>;
    fn install_remote_herdr_shim<'a>(
        &'a self,
        conn: &'a crate::hosts::HostConn,
        remote_bot_dir: &'a str,
    ) -> impl std::future::Future<Output = Result<String>> + Send + 'a;
    fn install_remote_cargo_shim<'a>(
        &'a self,
        conn: &'a crate::hosts::HostConn,
        remote_bot_dir: &'a str,
    ) -> impl std::future::Future<Output = Result<String>> + Send + 'a;
    fn install_remote_kill_shim<'a>(
        &'a self,
        conn: &'a crate::hosts::HostConn,
        remote_bot_dir: &'a str,
    ) -> impl std::future::Future<Output = Result<String>> + Send + 'a;
    /// 啟動前：claude 身分目錄的 herdr SessionStart hook 缺就補裝；補不上回一句警告（寫進啟動回應，仍照常啟動）。預設不檢查。
    fn claude_hook_preflight<'a>(&'a self, _bot: &'a crate::db::Bot, _host: &'a str) -> impl std::future::Future<Output = Option<String>> + Send + 'a {
        async { None }
    }
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
