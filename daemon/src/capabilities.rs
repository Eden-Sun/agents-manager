//! 最上層以下的模組共用的「窄能力」：只拿它真正用到的那一樣，不拿整個 `App`（P0）。
//!
//! 每個 trait 只有一個意思（DB 連線、事件推送、資料目錄、設定、bot 鎖、bot 狀態推送、關機訊號、開機 id），`App` 在這個檔實作，
//! `Arc<T>` 也算（所以既有的 `(&app, …)` 呼叫端不用改）。主機表與實例 slug 沿用 [`crate::hosts::HostsAccess`]／
//! [`crate::hosts::HostInstance`]。feature 自己的狀態（額度表、偵測快取…）**不**放這裡：那是各 feature 的 Env trait 的事，放這裡會讓
//! feature 反過來依賴這個檔。
//!
//! 之後抽出獨立 crate 時，這些 trait 會跟著 `am-ports` 一起搬下去；現在先在 daemon 內把依賴方向理清。

use crate::config::ConfigStore;
use crate::state::App;
use serde_json::Value;
use sqlx::SqlitePool;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;


/// SQLite 連線池。
pub trait Db: Send + Sync {
    fn db(&self) -> &SqlitePool;
}

/// 推一個事件給 WS 客戶端（序號、重播環、敏感欄位清理都在實作那一側）。
pub trait Emit: Send + Sync {
    fn emit(&self, kind: &str, data: Value) -> impl Future<Output = ()> + Send;
}

/// daemon 的資料目錄。
pub trait DataDir: Send + Sync {
    fn data_dir(&self) -> &Path;
}

/// 設定檔（`config.toml`）的讀寫。
pub trait Cfg: Send + Sync {
    fn cfg(&self) -> &ConfigStore;
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
