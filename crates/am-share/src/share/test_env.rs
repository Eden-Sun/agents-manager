//! #1862 遷移 B：am-share 的 crate-only 測試用窄環境（不需要 daemon `App`、herdr、事件匯流排與 prompt 路徑）。
//!
//! `TestShareEnv` 只實作分享測試實際用到的既有 trait（`ShareStorage`／`OutboxEnv`／`OutboxRemoteEnv`／`SiteEnv`／`SvgCheckEnv`／
//! `PortalEnv`，以及 `Db`／`DataDir`／`Cfg`）。資料庫是真的暫存 SQLite（`db::open_with(path, [share::store migration])`，與正式啟動同一份共用 schema 加上分享自己的 migration）；
//! 設定是真的 `ConfigStore`；主機登記是真的 `HostManager`（本機 herdr socket 指向不存在的路徑，碰本機 herdr 會明確失敗）。
//! 沒有設定的外部服務**不會預設成功**：`remind_bot`、`send_share_message`、`share_status` 一律回明確的失敗（或 panic 並寫明原因），
//! 要驗這些流程的測試留在 daemon（`daemon-test-harness`）。
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use am_base::capabilities::{Cfg, DataDir, Db};
use am_base::config::{ConfigChangeHooks, ConfigFile, ConfigStore};
use am_base::hosts::{HostConn, HostFence, HostManager};
use am_base::outbox::{BotLookup, BotPlace, OutboxEnv, ShareStorage};
use am_base::ws_event::WsEvent;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use crate::db;
use crate::share::portal::{PortalEnv, SendOutcome};
use crate::share::svg_check::SvgCheckEnv;

/// daemon 版的 hooks 驗投影、寫稽核；這裡都不需要（測試只讀寫 `[share]` 之類的欄位）。
struct NoHooks;
impl ConfigChangeHooks for NoHooks {
    fn validate_projection(&self, _next: &ConfigFile) -> anyhow::Result<()> {
        Ok(())
    }
    fn audit_external_change(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
    fn audit_reload_unchanged(&self, _at: &'static std::panic::Location<'static>, _path: &Path) {}
    fn audit_write(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
}

pub struct TestShareEnv {
    pub db: SqlitePool,
    pub cfg: ConfigStore,
    pub hosts: HostManager,
    data_dir: PathBuf,
    events: broadcast::Sender<WsEvent>,
    bot_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl am_base::hosts::HostsAccess for TestShareEnv {
    fn hosts(&self) -> &HostManager {
        &self.hosts
    }
}

impl am_base::hosts::HostInstance for TestShareEnv {
    /// 測試一律是正式實例的路徑形狀（沒有隔離 slug）。
    fn instance(&self) -> Option<String> {
        None
    }
}

/// 只為了 `HostManager::replace_remote_for_test`（同名主機改指）：它只用到 `instance()` 與 `forget_host_observations`。
/// 這個環境沒有「以主機名為鍵的觀測快取」，所以 `forget_host_observations` 是真的沒有東西可清（不是預設成功）；
/// 其餘連線生命週期的事（連上後的整串對帳、推事件、ping）這裡根本不該被觸發，被呼叫就明確失敗。
impl am_base::hosts::HostHooks for TestShareEnv {
    fn is_shared_host(_app: &Arc<Self>, _host: &str) -> impl Future<Output = bool> + Send {
        async { panic!("TestShareEnv: is_shared_host is not configured") }
    }
    fn host_changed(_app: &Arc<Self>, _fence: &HostFence) -> impl Future<Output = ()> + Send {
        async { panic!("TestShareEnv: host_changed is not configured") }
    }
    fn set_local_herdr_connected(&self, _ok: bool) {
        panic!("TestShareEnv: set_local_herdr_connected is not configured")
    }
    fn host_connected(_app: Arc<Self>, _host: String) -> impl Future<Output = ()> + Send {
        async { panic!("TestShareEnv: host_connected is not configured (the connection supervisor never runs here)") }
    }
    fn forget_host_observations(_app: &Arc<Self>, _host: &str) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn host_removed(_app: &Arc<Self>, _host: &str) -> impl Future<Output = ()> + Send {
        async { panic!("TestShareEnv: host_removed is not configured") }
    }
}

/// 同名同形於 daemon `testing::Env`（只留分享測試用到的欄位）。
pub type App = TestShareEnv;

pub struct Env {
    pub app: Arc<TestShareEnv>,
    pub project_id: String,
}

pub fn scratch_dir(prefix: &str) -> PathBuf {
    am_base::test_dirs::scratch_dir(prefix)
}

/// 只套分享需要的 feature migration（`share::store`）；其他 feature 的表（supervisor、judge…）分享測試不碰。
fn share_migrations() -> am_base::db::FeatureMigrations {
    fn run(pool: &SqlitePool) -> am_base::db::MigrationFuture<'_> {
        Box::pin(crate::share::store::migrate(pool))
    }
    &[("share::store", run as am_base::db::FeatureMigration)]
}

pub async fn env() -> Env {
    let dir = scratch_dir("am-share-env");
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let pool = db::open_with(&data.join("db.sqlite3"), share_migrations()).await.unwrap();
    let cfg = ConfigStore::load_with_hooks(data.join("config.toml"), Arc::new(NoHooks)).await.unwrap();
    let (events, _) = broadcast::channel(64);
    let app = TestShareEnv {
        db: pool,
        cfg,
        // 本機 herdr 指向不存在的 socket：分享測試不碰本機 herdr，真的碰到會連線失敗。
        hosts: HostManager::new(am_base::herdr::HerdrClient::new(data.join("no-herdr.sock"))),
        data_dir: data,
        events,
        bot_locks: Default::default(),
    };
    let project_id = db::ulid();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?, 'local', ?)")
        .bind(&project_id)
        .bind(repo.to_string_lossy().to_string())
        .bind("proj")
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    Env { app: Arc::new(app), project_id }
}

/// 同 daemon `testing::claude_bot`：直接寫進 DB。
pub async fn claude_bot(app: &Arc<TestShareEnv>, project_id: &str, name: &str) -> db::Bot {
    let id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
         VALUES (?,?,?,'claude','[]',0,1,'tok','user',?)",
    )
    .bind(&id)
    .bind(project_id)
    .bind(name)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    db::bot(&app.db, &id).await.unwrap().unwrap()
}

impl Db for TestShareEnv {
    fn db(&self) -> &SqlitePool {
        &self.db
    }
}

impl DataDir for TestShareEnv {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

impl Cfg for TestShareEnv {
    fn cfg(&self) -> &ConfigStore {
        &self.cfg
    }
}

impl ShareStorage for TestShareEnv {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    fn db_pool(&self) -> &SqlitePool {
        &self.db
    }
}

impl OutboxEnv for TestShareEnv {
    /// 同 daemon `App` 的實作（bot→專案兩步查詢，結果分得出是哪一步沒有）。
    fn bot_place(&self, bot_id: &str) -> impl Future<Output = Result<BotPlace, BotLookup>> + Send {
        let bot_id = bot_id.to_string();
        async move {
            let bot = match db::bot(&self.db, &bot_id).await {
                Ok(Some(bot)) => bot,
                Ok(None) => return Err(BotLookup::BotMissing),
                Err(_) => return Err(BotLookup::BotUnavailable),
            };
            let project = match db::project(&self.db, &bot.project_id).await {
                Ok(Some(project)) => project,
                Ok(None) => return Err(BotLookup::ProjectMissing),
                Err(_) => return Err(BotLookup::ProjectUnavailable),
            };
            Ok(BotPlace { host: project.host, project_path: project.path, cwd: bot.cwd })
        }
    }
}

impl am_base::outbox_remote::OutboxRemoteEnv for TestShareEnv {
    fn host_conn(&self, host: &str) -> impl Future<Output = Option<Arc<HostConn>>> + Send {
        let host = host.to_string();
        async move { self.hosts.get(&host).await }
    }
    fn instance(&self) -> Option<String> {
        None
    }
}

impl crate::share::site::SiteEnv for TestShareEnv {
    fn db_pool(&self) -> &SqlitePool {
        &self.db
    }
    fn host_conn(&self, host: &str) -> impl Future<Output = Option<Arc<HostConn>>> + Send {
        let host = host.to_string();
        async move { self.hosts.get(&host).await }
    }
    fn host_fence(&self, host: &str) -> impl Future<Output = Option<HostFence>> + Send {
        let host = host.to_string();
        async move { self.hosts.fence(&host).await }
    }
    fn instance(&self) -> Option<String> {
        None
    }
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

impl SvgCheckEnv for TestShareEnv {
    fn remind_bot(_app: &Arc<Self>, _bot_id: &str, _text: &str, _crid: &str) -> impl Future<Output = Result<(), String>> + Send {
        async { Err("TestShareEnv: remind_bot is not configured (needs the daemon prompt path)".to_string()) }
    }
}

impl PortalEnv for TestShareEnv {
    fn share_base_url(&self) -> impl Future<Output = Option<String>> + Send {
        async move { self.cfg.get().await.share.base() }
    }
    fn share_listen(&self) -> impl Future<Output = Option<String>> + Send {
        async move { self.cfg.get().await.share.listen.clone() }
    }
    fn bot_mutex(&self, bot_id: &str) -> impl Future<Output = Arc<tokio::sync::Mutex<()>>> + Send {
        let bot_id = bot_id.to_string();
        async move { self.bot_locks.lock().await.entry(bot_id).or_default().clone() }
    }
    fn share_status(&self, bot_id: &str) -> impl Future<Output = &'static str> + Send {
        let _ = bot_id;
        async { panic!("TestShareEnv: share_status is not configured (needs run/lamp state from the daemon)") }
    }
    fn subscribe_events(&self) -> broadcast::Receiver<WsEvent> {
        self.events.subscribe()
    }
    fn share_storage_full(&self, bot_id: &str) -> impl Future<Output = Result<bool, crate::share::budget::Unavailable>> + Send {
        let bot_id = bot_id.to_string();
        async move { crate::share::budget::is_full(self, &bot_id).await }
    }
    fn send_share_message(_app: &Arc<Self>, _bot_id: &str, _composed: &str, _crid: &str, _token: &str) -> impl Future<Output = SendOutcome> + Send {
        async { SendOutcome::Failed("TestShareEnv: send_share_message is not configured (needs the daemon prompt path)".to_string()) }
    }
}
