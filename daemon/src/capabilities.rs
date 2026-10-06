//! 最上層以下的模組共用的「窄能力」：只拿它真正用到的那一樣，不拿整個 `App`（P0）。
//!
//! 每個 trait 只有一個意思（DB 連線、事件推送、資料目錄、設定、bot 鎖、bot 狀態推送、關機訊號、開機 id），`App` 在這個檔實作，
//! `Arc<T>` 也算（所以既有的 `(&app, …)` 呼叫端不用改）。主機表與實例 slug 沿用 [`crate::hosts::HostsAccess`]／
//! [`crate::hosts::HostInstance`]。feature 自己的狀態（額度表、偵測快取…）**不**放這裡：那是各 feature 的 Env trait 的事，放這裡會讓
//! feature 反過來依賴這個檔。
//!
//! 之後抽出獨立 crate 時，這些 trait 會跟著 `am-ports` 一起搬下去；現在先在 daemon 內把依賴方向理清。

use crate::config::ConfigStore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use crate::herdr::HerdrClient;
use crate::state::App;
use serde_json::Value;
use sqlx::SqlitePool;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;


/// SQLite 連線池。
pub trait Db: Send + Sync {
    fn db(&self) -> &SqlitePool;
}

/// 推一個事件給 WS 客戶端（序號、重播環、敏感欄位清理都在實作那一側）。
pub trait Emit: Send + Sync {
    fn emit(&self, kind: &str, data: Value) -> impl Future<Output = ()> + Send;
    /// 目前事件序號（最近一則 `emit` 取到的）。
    fn current_seq(&self) -> u64;
}

/// daemon 的資料目錄。
pub trait DataDir: Send + Sync {
    fn data_dir(&self) -> &Path;
    /// 這顆 bot 在資料目錄底下的專屬目錄（id 不合法回 `Err`）。跟 `App::bot_dir` 同一條規則。
    fn bot_dir(&self, bot_id: &str) -> anyhow::Result<std::path::PathBuf> {
        if !crate::config::valid_id(bot_id) {
            anyhow::bail!("invalid bot id `{bot_id}` (must match {})", crate::config::ID_RE);
        }
        Ok(self.data_dir().join("bots").join(bot_id))
    }
}

/// 設定檔（`config.toml`）的讀寫。
pub trait Cfg: Send + Sync {
    fn cfg(&self) -> &ConfigStore;
}

/// 每顆 bot 一把的互斥鎖。
pub trait BotLocks: Send + Sync {
    fn bot_lock(&self, bot_id: &str) -> impl Future<Output = Arc<Mutex<()>>> + Send;
}

/// 重算並推出一顆 bot 的狀態（`bot_status` 事件）。
pub trait BotStatusEmit: Send + Sync {
    fn emit_bot_status(&self, bot_id: &str) -> impl Future<Output = ()> + Send;
}

/// 這個 daemon 行程的開機 id。
pub trait BootId: Send + Sync {
    fn boot_id(&self) -> &str;
}

impl Db for App {
    fn db(&self) -> &SqlitePool {
        &self.db
    }
}
impl Emit for App {
    fn emit(&self, kind: &str, data: Value) -> impl Future<Output = ()> + Send {
        App::emit(self, kind, data)
    }
    fn current_seq(&self) -> u64 {
        App::current_seq(self)
    }
}
impl DataDir for App {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}
impl Cfg for App {
    fn cfg(&self) -> &ConfigStore {
        &self.cfg
    }
}
impl BotLocks for App {
    fn bot_lock(&self, bot_id: &str) -> impl Future<Output = Arc<Mutex<()>>> + Send {
        App::bot_lock(self, bot_id)
    }
}
impl BotStatusEmit for App {
    fn emit_bot_status(&self, bot_id: &str) -> impl Future<Output = ()> + Send {
        App::emit_bot_status(self, bot_id)
    }
}
impl BootId for App {
    fn boot_id(&self) -> &str {
        &self.boot_id
    }
}

impl<T: Db + ?Sized> Db for Arc<T> {
    fn db(&self) -> &SqlitePool {
        (**self).db()
    }
}
impl<T: Emit + ?Sized> Emit for Arc<T> {
    fn emit(&self, kind: &str, data: Value) -> impl Future<Output = ()> + Send {
        (**self).emit(kind, data)
    }
    fn current_seq(&self) -> u64 {
        (**self).current_seq()
    }
}
impl<T: DataDir + ?Sized> DataDir for Arc<T> {
    fn data_dir(&self) -> &Path {
        (**self).data_dir()
    }
}
impl<T: Cfg + ?Sized> Cfg for Arc<T> {
    fn cfg(&self) -> &ConfigStore {
        (**self).cfg()
    }
}
impl<T: BotLocks + ?Sized> BotLocks for Arc<T> {
    fn bot_lock(&self, bot_id: &str) -> impl Future<Output = Arc<Mutex<()>>> + Send {
        (**self).bot_lock(bot_id)
    }
}
impl<T: BotStatusEmit + ?Sized> BotStatusEmit for Arc<T> {
    fn emit_bot_status(&self, bot_id: &str) -> impl Future<Output = ()> + Send {
        (**self).emit_bot_status(bot_id)
    }
}
impl<T: BootId + ?Sized> BootId for Arc<T> {
    fn boot_id(&self) -> &str {
        (**self).boot_id()
    }
}

/// 「這個 host／bot／run 現在該連哪個 herdr session、哪條 client、連著沒有」。
pub trait HerdrRoutes: Send + Sync {
    fn session_for_host(&self, host: &str) -> impl Future<Output = Option<String>> + Send;
    fn herdr_for_session(&self, host: &str, session: &str) -> impl Future<Output = Option<HerdrClient>> + Send;
    fn session_connected(&self, host: &str, session: &str) -> impl Future<Output = bool> + Send;
    fn bot_connected(&self, bot_id: &str) -> impl Future<Output = bool> + Send;
    fn session_for_run(&self, run: &crate::db::Run) -> impl Future<Output = Option<String>> + Send;
    fn herdr_for_run(&self, run: &crate::db::Run) -> impl Future<Output = Option<HerdrClient>> + Send;
    fn host_connected(&self, host: &str) -> impl Future<Output = bool> + Send;
    fn herdr_for(&self, host: &str) -> impl Future<Output = Option<HerdrClient>> + Send;
    fn session_for_bot_with_host_fence(
        &self,
        bot: &crate::db::Bot,
        host: &str,
        fence: &crate::hosts::HostFence,
    ) -> impl Future<Output = Option<String>> + Send;
    fn herdr_for_host_fence(&self, fence: &crate::hosts::HostFence, session: &str) -> impl Future<Output = Option<HerdrClient>> + Send;
}

impl HerdrRoutes for App {
    fn session_for_host(&self, host: &str) -> impl Future<Output = Option<String>> + Send {
        App::session_for_host(self, host)
    }
    fn herdr_for_session(&self, host: &str, session: &str) -> impl Future<Output = Option<HerdrClient>> + Send {
        App::herdr_for_session(self, host, session)
    }
    fn session_connected(&self, host: &str, session: &str) -> impl Future<Output = bool> + Send {
        App::session_connected(self, host, session)
    }
    fn bot_connected(&self, bot_id: &str) -> impl Future<Output = bool> + Send {
        App::bot_connected(self, bot_id)
    }
    fn session_for_run(&self, run: &crate::db::Run) -> impl Future<Output = Option<String>> + Send {
        App::session_for_run(self, run)
    }
    fn herdr_for_run(&self, run: &crate::db::Run) -> impl Future<Output = Option<HerdrClient>> + Send {
        App::herdr_for_run(self, run)
    }
    fn host_connected(&self, host: &str) -> impl Future<Output = bool> + Send {
        App::host_connected(self, host)
    }
    fn herdr_for(&self, host: &str) -> impl Future<Output = Option<HerdrClient>> + Send {
        App::herdr_for(self, host)
    }
    fn session_for_bot_with_host_fence(
        &self,
        bot: &crate::db::Bot,
        host: &str,
        fence: &crate::hosts::HostFence,
    ) -> impl Future<Output = Option<String>> + Send {
        App::session_for_bot_with_host_fence(self, bot, host, fence)
    }
    fn herdr_for_host_fence(&self, fence: &crate::hosts::HostFence, session: &str) -> impl Future<Output = Option<HerdrClient>> + Send {
        App::herdr_for_host_fence(self, fence, session)
    }
}

impl<T: HerdrRoutes + ?Sized> HerdrRoutes for Arc<T> {
    fn session_for_host(&self, host: &str) -> impl Future<Output = Option<String>> + Send {
        (**self).session_for_host(host)
    }
    fn herdr_for_session(&self, host: &str, session: &str) -> impl Future<Output = Option<HerdrClient>> + Send {
        (**self).herdr_for_session(host, session)
    }
    fn session_connected(&self, host: &str, session: &str) -> impl Future<Output = bool> + Send {
        (**self).session_connected(host, session)
    }
    fn bot_connected(&self, bot_id: &str) -> impl Future<Output = bool> + Send {
        (**self).bot_connected(bot_id)
    }
    fn session_for_run(&self, run: &crate::db::Run) -> impl Future<Output = Option<String>> + Send {
        (**self).session_for_run(run)
    }
    fn herdr_for_run(&self, run: &crate::db::Run) -> impl Future<Output = Option<HerdrClient>> + Send {
        (**self).herdr_for_run(run)
    }
    fn host_connected(&self, host: &str) -> impl Future<Output = bool> + Send {
        (**self).host_connected(host)
    }
    fn herdr_for(&self, host: &str) -> impl Future<Output = Option<HerdrClient>> + Send {
        (**self).herdr_for(host)
    }
    fn session_for_bot_with_host_fence(
        &self,
        bot: &crate::db::Bot,
        host: &str,
        fence: &crate::hosts::HostFence,
    ) -> impl Future<Output = Option<String>> + Send {
        (**self).session_for_bot_with_host_fence(bot, host, fence)
    }
    fn herdr_for_host_fence(&self, fence: &crate::hosts::HostFence, session: &str) -> impl Future<Output = Option<HerdrClient>> + Send {
        (**self).herdr_for_host_fence(fence, session)
    }
}

/// 關機訊號：背景迴圈用它收尾。
pub trait Shutdown: Send + Sync {
    fn shutdown(&self) -> &CancellationToken;
}
/// 追蹤背景任務，關機時等它們收完。
pub trait BgTasks: Send + Sync {
    fn background_tasks(&self) -> &TaskTracker;
}
/// 這顆 daemon 執行檔的路徑。
pub trait ExePath: Send + Sync {
    fn exe(&self) -> &Path;
}
/// 管理 API 的 port。
pub trait ListenPort: Send + Sync {
    fn port(&self) -> u16;
}
impl Shutdown for App { fn shutdown(&self) -> &CancellationToken { &self.shutdown } }
impl BgTasks for App { fn background_tasks(&self) -> &TaskTracker { &self.background_tasks } }
impl ExePath for App { fn exe(&self) -> &Path { &self.exe } }
impl ListenPort for App { fn port(&self) -> u16 { self.port } }
impl<T: Shutdown + ?Sized> Shutdown for Arc<T> { fn shutdown(&self) -> &CancellationToken { (**self).shutdown() } }
impl<T: BgTasks + ?Sized> BgTasks for Arc<T> { fn background_tasks(&self) -> &TaskTracker { (**self).background_tasks() } }
impl<T: ExePath + ?Sized> ExePath for Arc<T> { fn exe(&self) -> &Path { (**self).exe() } }
impl<T: ListenPort + ?Sized> ListenPort for Arc<T> { fn port(&self) -> u16 { (**self).port() } }

#[cfg(test)]
mod tests {
    use super::*;

    async fn uses_only_narrow_capabilities(app: &(impl Db + DataDir + Emit)) -> (bool, std::path::PathBuf) {
        app.emit("capability_probe", serde_json::json!({"ok": true})).await;
        let ping: i64 = sqlx::query_scalar("SELECT 1").fetch_one(app.db()).await.unwrap();
        (ping == 1, app.data_dir().to_path_buf())
    }

    /// `Arc<App>` 與 `App` 都能當窄能力傳進去，而且指到同一份資源（既有呼叫端不用改）。
    #[tokio::test]
    async fn app_and_arc_app_satisfy_the_same_narrow_capabilities() {
        let e = crate::testing::env().await;
        let mut rx = e.app.subscribe();
        let (ok, dir) = uses_only_narrow_capabilities(&e.app).await;
        assert!(ok);
        assert_eq!(dir, e.app.data_dir);
        let (ok2, _) = uses_only_narrow_capabilities(&*e.app).await;
        assert!(ok2);
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, "capability_probe");
    }
}

/// 這個 `App` 是不是隔離模式（不碰真的 herdr）。
pub trait Isolation: Send + Sync {
    fn isolated(&self) -> bool;
}
impl Isolation for App {
    fn isolated(&self) -> bool {
        App::isolated(self)
    }
}
impl<T: Isolation + ?Sized> Isolation for Arc<T> {
    fn isolated(&self) -> bool {
        (**self).isolated()
    }
}

/// 管理 API 的 UI token（`X-AM-Token`）。
pub trait UiToken: Send + Sync {
    fn ui_token(&self) -> &String;
}
impl UiToken for App {
    fn ui_token(&self) -> &String {
        &self.ui_token
    }
}
impl<T: UiToken + ?Sized> UiToken for Arc<T> {
    fn ui_token(&self) -> &String {
        (**self).ui_token()
    }
}
