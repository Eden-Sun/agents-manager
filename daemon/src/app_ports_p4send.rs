//! am-turn-send 的窄介面由 `App`／`SqlitePool`／交易連線實作的地方（crate 拆分第 3 步 P4send）。每個方法逐行委派給 send 這組
//! 原本呼叫的函式，不加任何邏輯、不多一個 await、不改鎖的範圍，所以送出／排隊／冪等的語意不變。介面在 `send_ports.rs`（本檔也實作
//! `am-ports` 合約缺的 `BotLock` adapter）；這個檔案是 send 唯一還知道 supervisor／handoff／share／attach／codex_*… 實作細節的地方
//! （composition root 側的 adapter，不屬於未來的 am-turn-send）。
//!
//! 模組掛在 `lifecycle/send_now.rs`（`#[path]`），不碰 `lifecycle/mod.rs` 與 `lib.rs`。

use crate::attach::Attachment;
use crate::codex_history::{HistoryConn, Mark};
use crate::db;
use crate::lifecycle::LcResult;
use super::ports::{
    AttachConnPort, AttachSendPort, AttachTxPort, CodexSendPort, HandoffSendRepo, IdleSleepPort, MaintenancePort, PaneWatchPort,
    SendEnvPort, ShareSendRepo, SupervisorSendRepo,
};
use crate::state::App;
use crate::supervisor::maintenance::{WindowHeld, WindowUnreadable};
use am_core::{BotId, PortError};
use am_ports::{BotLock, BotLockGuard};
use anyhow::Result;
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};
use std::future::Future;
use std::sync::Arc;

/// `am_ports::BotLock` 的 App 實作：就是 `App::bot_lock` 那把 per-bot 互斥鎖（同一格 `Arc<Mutex<()>>`）。
/// 守衛持有 `OwnedMutexGuard`，所以 `App::retain_bot_locks` 看到的 `Arc` 計數跟原本握著 `lock` 的寫法一樣（有人握著或等著就不清）。
pub(crate) struct AppBotLock<'a, A> {
    app: &'a A,
}

impl<'a, A: crate::capabilities::BotLocks> AppBotLock<'a, A> {
    pub(crate) fn new(app: &'a A) -> Self {
        Self { app }
    }
}

struct Held(#[allow(dead_code)] tokio::sync::OwnedMutexGuard<()>);
impl BotLockGuard for Held {}

impl<A: crate::capabilities::BotLocks> BotLock for AppBotLock<'_, A> {
    fn lock_bot<'a>(&'a self, bot: &'a BotId) -> impl Future<Output = std::result::Result<Box<dyn BotLockGuard + 'a>, PortError>> + Send + 'a {
        async move {
            let lock = self.app.bot_lock(bot).await;
            let guard = lock.lock_owned().await;
            Ok(Box::new(Held(guard)) as Box<dyn BotLockGuard + 'a>)
        }
    }
}

impl MaintenancePort for Arc<App> {
    const UNREADABLE_RETRY_SECS: i64 = crate::supervisor::maintenance::UNREADABLE_RETRY_SECS;
    async fn window_held(&self) -> std::result::Result<Option<WindowHeld>, WindowUnreadable> {
        crate::supervisor::maintenance::window_held(self).await
    }
}

impl IdleSleepPort for Arc<App> {
    async fn idle_sleep_wake(&self, bot_id: &str, why: &str) -> Result<bool> {
        crate::supervisor::idle_sleep::wake(self, bot_id, why).await
    }
    async fn idle_sleep_wake_locked(&self, bot_id: &str, why: &str) -> Result<bool> {
        crate::supervisor::idle_sleep::wake_locked(self, bot_id, why).await
    }
}

impl SupervisorSendRepo for SqlitePool {
    async fn assignment_by_turn(&self, turn_id: &str) -> Result<Option<crate::supervisor::store::Assignment>> {
        crate::supervisor::store::assignment_by_turn(self, turn_id).await
    }
    async fn load_owned(&self) -> Result<crate::supervisor_owned::Owned> {
        crate::supervisor_owned::load(self).await
    }
}

impl HandoffSendRepo for SqlitePool {
    async fn bot_handed_off_to(&self, bot_id: &str) -> Result<Option<String>> {
        crate::handoff::bot_handed_off_to(self, bot_id).await
    }
    async fn refuse_handed_off(&self, bot_id: &str) -> LcResult<()> {
        crate::handoff::refuse(self, bot_id).await
    }
}

impl ShareSendRepo for SqlitePool {
    async fn resolve_share_token(&self, token: &str) -> std::result::Result<Option<String>, sqlx::Error> {
        crate::share::store::resolve(self, token).await
    }
    async fn touch_share(&self, bot_id: &str) {
        crate::share::store::touch(self, bot_id).await
    }
    async fn is_share_bot(&self, bot_id: &str) -> std::result::Result<bool, sqlx::Error> {
        crate::share::store::is_share_bot(self, bot_id).await
    }
}

impl AttachSendPort for Arc<App> {
    async fn resolve_attachments(&self, bot_id: &str, ids: &[String]) -> Result<Vec<Attachment>> {
        crate::attach::resolve(self, bot_id, ids).await
    }
    async fn bind_attachments(&self, message_id: &str, items: &[Attachment]) -> Result<()> {
        crate::attach::bind(self, message_id, items).await
    }
}

impl AttachTxPort for Transaction<'_, Sqlite> {
    async fn bind_attachments_tx(&mut self, message_id: &str, items: &[Attachment]) -> Result<()> {
        crate::attach::bind_tx(self, message_id, items).await
    }
}

impl AttachConnPort for SqliteConnection {
    async fn unbind_attachment_message(&mut self, msg_id: &str, turn_id: &str) -> Result<()> {
        crate::attach::unbind_message(self, msg_id, turn_id).await
    }
}

impl PaneWatchPort for Arc<App> {
    async fn unwatch_pane_on_session(&self, host: &str, session: &str, pane_id: &str) {
        crate::events::unwatch_pane_on_session(self, host, session, pane_id).await
    }
}

impl CodexSendPort for Arc<App> {
    async fn observe_codex_screen(&self, run: &db::Run, screen: &str) {
        crate::codex_model_migration::observe_screen(self, run, screen).await
    }
    async fn close_codex_picker(&self, client: &crate::herdr::HerdrClient, pane_id: &str) -> bool {
        crate::codex_live::close_picker(client, pane_id).await
    }
    fn codex_running_version(&self, run_id: &str) -> Option<String> {
        crate::codex_update::running_version_of(run_id)
    }
    async fn codex_history_mark(&self, bot: &db::Bot, run: &db::Run) -> Option<Mark> {
        crate::codex_history::mark(self, bot, run).await
    }
    async fn codex_prompt_landed(&self, mark: &Mark, conn: &mut Option<Box<dyn HistoryConn>>, text: &str) -> bool {
        crate::codex_history::prompt_landed(self, mark, conn, text).await
    }
}

impl SendEnvPort for Arc<App> {
    async fn dangerous_rm_notify_once(&self, run: &db::Run, rm: &crate::tui_prompts::DangerousRm) -> bool {
        crate::dangerous_rm::notify_once(self, run, rm).await
    }
    async fn note_keep_warm_prompt(&self, bot_id: &str, client_request_id: &str) {
        crate::primary_keep_warm::note_prompt(self, bot_id, client_request_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::app_ports_p4::AppEventSink;
    use crate::testing as tt;
    use serde_json::json;

    /// 走合約的 `BotLock` 拿到的就是 `App::bot_lock` 那把鎖：握著的時候原本的拿法進不去，放掉才進得去；
    /// 握著期間 `retain_bot_locks` 也不能把這格清掉（跟原本握著 `lock` 的寫法同一個 `Arc` 計數規則）。
    #[tokio::test]
    async fn the_contract_bot_lock_is_the_apps_own_per_bot_mutex() {
        let env = tt::env().await;
        let app = env.app.clone();
        let locks = AppBotLock::new(&app);
        let bot: BotId = "bot-lock-x".into();
        let guard = locks.lock_bot(&bot).await.unwrap();

        let same = app.bot_lock(&bot).await;
        assert!(same.try_lock().is_err(), "握著的時候別的拿法進不去");
        app.retain_bot_locks(&[]).await;
        assert!(app.bot_lock(&bot).await.try_lock().is_err(), "握著期間不能被 retain 清掉");

        drop(guard);
        assert!(same.try_lock().is_ok(), "放掉就進得去");
    }

    /// 兩個人搶同一顆：第二個一定等到第一個放掉（順序語意跟原本的 `lock().await` 一樣）。
    #[tokio::test]
    async fn a_second_holder_waits_for_the_first() {
        let env = tt::env().await;
        let app = env.app.clone();
        let locks = AppBotLock::new(&app);
        let bot: BotId = "bot-lock-y".into();
        let first = locks.lock_bot(&bot).await.unwrap();
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(100), locks.lock_bot(&bot)).await;
        assert!(waiting.is_err(), "第二個要等");
        drop(first);
        assert!(tokio::time::timeout(std::time::Duration::from_secs(2), locks.lock_bot(&bot)).await.is_ok());
    }

    /// `emit_object` 發出去的事件跟原本的 `app.emit(kind, payload)` 一樣：同一個 kind、同一份 payload。
    #[tokio::test]
    async fn emit_object_sends_the_same_event_app_emit_did() {
        let env = tt::env().await;
        let app = env.app.clone();
        let mut rx = app.subscribe();
        let payload = json!({"reason": "turn_retracted", "turn_id": "t1", "n": 3});
        crate::lifecycle::send_now::ports::emit_object(&AppEventSink::new(&app), "resync", None, payload.clone()).await;
        let ev = rx.recv().await.unwrap();
        assert_eq!((ev.kind.as_str(), &ev.data), ("resync", &payload));
    }
}
