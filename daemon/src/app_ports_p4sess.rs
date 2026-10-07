//! am-turn-session 的窄介面由 `App`／`SqlitePool` 實作的地方（crate 拆分第 3 步 P4sess）。每個方法逐行委派給 session 這組原本呼叫的
//! 函式，不加任何邏輯、不多一個 await、不改鎖的範圍，所以啟停、session 建立、resume gate 與 `HostFence` 的語意不變。介面在
//! `session_ports.rs`（本檔也實作 `am-ports` 合約的 `BotLock` adapter）；這個檔案是 session 唯一還知道 handoff／share／intents／
//! remote_purge／preview／models／shim… 實作細節的地方（composition root 側的 adapter，不屬於未來的 am-turn-session）。
//!
//! 模組掛在 `lifecycle/start.rs`（`#[path]`），不碰 `lifecycle/mod.rs` 與 `lib.rs`。

use super::ports::{
    HandoffSessionRepo, PaneWatchPort, PreviewPort, RemoteCleanupPort, RestartIntentRepo, SessionProviderPort, ShareSessionRepo,
    ShimInstallPort,
};
use crate::lifecycle::LcResult;
use crate::state::App;
use am_core::{BotId, PortError};
use am_ports::{BotLock, BotLockGuard};
use anyhow::Result;
use serde_json::Value;
use sqlx::SqlitePool;
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

impl HandoffSessionRepo for SqlitePool {
    async fn bot_handed_off_to(&self, bot_id: &str) -> Result<Option<String>> {
        crate::handoff::bot_handed_off_to(self, bot_id).await
    }
    async fn refuse_handed_off(&self, bot_id: &str) -> LcResult<()> {
        crate::handoff::refuse(self, bot_id).await
    }
}

impl ShareSessionRepo for SqlitePool {
    async fn restricted_workspace(&self, bot_id: &str) -> std::result::Result<Option<String>, sqlx::Error> {
        crate::share::store::restricted_workspace(self, bot_id).await
    }
    async fn caged_workspace(&self, bot_id: &str) -> std::result::Result<Option<String>, sqlx::Error> {
        crate::share::store::caged_workspace(self, bot_id).await
    }
}

impl RestartIntentRepo for SqlitePool {
    async fn prepare_restart_intent(&self, subject_id: &str, host: &str, payload: &Value, ttl_secs: i64, boot: &str) -> Result<String> {
        crate::intents::prepare_restart(self, subject_id, host, payload, ttl_secs, boot).await
    }
    async fn complete_intent(&self, id: &str) -> Result<bool> {
        crate::intents::complete(self, id).await
    }
    async fn abandon_intent(&self, id: &str, why: &str) -> Result<bool> {
        crate::intents::abandon(self, id, why).await
    }
    async fn fail_intent(&self, id: &str, err: &str) -> Result<bool> {
        crate::intents::fail(self, id, err).await
    }
}

impl PaneWatchPort for Arc<App> {
    async fn unwatch_pane_on_session(&self, host: &str, session: &str, pane_id: &str) {
        crate::events::unwatch_pane_on_session(self, host, session, pane_id).await
    }
}

impl RemoteCleanupPort for Arc<App> {
    async fn record_remote_purge(&self, bot_id: &str, host: &str, ok: bool, error: Option<&str>) {
        crate::remote_purge::record(self, bot_id, host, ok, error).await
    }
    async fn move_remote_bot_dir_to_trash(&self, conn: &crate::hosts::HostConn, bot_id: &str) -> Result<Option<String>> {
        crate::remote_trash::move_in(conn, bot_id).await
    }
}

impl PreviewPort for Arc<App> {
    async fn stop_preview_for_bot(&self, bot_id: &str) -> bool {
        crate::preview::stop_for_bot(self, bot_id).await
    }
}

impl SessionProviderPort for Arc<App> {
    fn claude_live_start_fresh(&self, run_id: &str) {
        crate::claude_live::start_fresh(run_id)
    }
    async fn models_list(&self, host: &str, kind: &str, identity: Option<&str>, refresh: bool) -> Result<Value> {
        crate::runners::models::list(self, host, kind, identity, refresh).await
    }
}

impl ShimInstallPort for Arc<App> {
    fn install_local_herdr_shim(&self, bot_dir: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
        crate::herdr_shim::install_local(bot_dir)
    }
    fn install_local_cargo_shim(&self, bot_dir: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
        crate::cargo_shim::install_local(bot_dir)
    }
    async fn install_remote_herdr_shim(&self, conn: &crate::hosts::HostConn, remote_bot_dir: &str) -> Result<String> {
        crate::herdr_shim::install_remote(conn, remote_bot_dir).await
    }
    async fn install_remote_cargo_shim(&self, conn: &crate::hosts::HostConn, remote_bot_dir: &str) -> Result<String> {
        crate::cargo_shim::install_remote(conn, remote_bot_dir).await
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
        let bot: BotId = "sess-lock-x".into();
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
        let bot: BotId = "sess-lock-y".into();
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
        let payload = json!({"reason": "context_lost_retired", "bot_id": "b1"});
        crate::lifecycle::start::ports::emit_object(&AppEventSink::new(&app), "resync", None, payload.clone()).await;
        let ev = rx.recv().await.unwrap();
        assert_eq!((ev.kind.as_str(), &ev.data), ("resync", &payload));
    }
}
