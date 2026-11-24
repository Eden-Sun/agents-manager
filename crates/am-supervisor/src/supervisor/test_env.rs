//! #1862 D：am-supervisor 的 crate-only 測試用窄環境（不需要 daemon `App`、herdr、事件匯流排）。
//!
//! `TestSupervisorEnv` 只實作這批資料／流程測試實際用到的既有 trait（`Db`／`Emit`／`DataDir`／`Cfg`）：
//! 資料庫是真的暫存 SQLite（`db::open_with(path, [supervisor::store migration])`，與正式啟動同一份共用 schema 加上 supervisor 自己的
//! migration，沒有複製 SQL）；設定是真的 `ConfigStore`（daemon 版的投影驗證與稽核 hooks 這裡是 no-op）。
//! `emit` 不是 no-op：記下每個事件（`events()` 可讀），`current_seq` 是遞增的序號；它沒有 WebSocket 客戶端可推。
//! 要 herdr、額度表、prompt 路徑、runner 的測試留在 daemon。
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use am_base::capabilities::{Cfg, DataDir, Db, Emit};
use am_base::config::{ConfigChangeHooks, ConfigFile, ConfigStore};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::db;

struct NoHooks;
impl ConfigChangeHooks for NoHooks {
    fn validate_projection(&self, _next: &ConfigFile) -> anyhow::Result<()> {
        Ok(())
    }
    fn audit_external_change(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
    fn audit_reload_unchanged(&self, _at: &'static std::panic::Location<'static>, _path: &Path) {}
    fn audit_write(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
}

pub struct TestSupervisorEnv {
    pub db: SqlitePool,
    pub cfg: ConfigStore,
    data_dir: PathBuf,
    events: StdMutex<Vec<(String, Value)>>,
    seq: AtomicU64,
}

/// 同名同形於 daemon `state::App`（只留這批測試用到的欄位）。
pub type App = TestSupervisorEnv;

impl TestSupervisorEnv {
    /// 目前為止 `emit` 過的事件（種類、資料）。
    #[allow(dead_code)]
    pub fn events(&self) -> Vec<(String, Value)> {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Db for TestSupervisorEnv {
    fn db(&self) -> &SqlitePool {
        &self.db
    }
}

impl Emit for TestSupervisorEnv {
    fn emit(&self, kind: &str, data: Value) -> impl Future<Output = ()> + Send {
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push((kind.to_string(), data));
        async {}
    }
    fn current_seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }
}

impl DataDir for TestSupervisorEnv {
    fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

impl Cfg for TestSupervisorEnv {
    fn cfg(&self) -> &ConfigStore {
        &self.cfg
    }
}

pub fn scratch_dir(prefix: &str) -> PathBuf {
    am_base::test_dirs::scratch_dir(prefix)
}

fn supervisor_migrations() -> am_base::db::FeatureMigrations {
    fn run(pool: &SqlitePool) -> am_base::db::MigrationFuture<'_> {
        Box::pin(super::store::migrate(pool))
    }
    &[("supervisor::store", run as am_base::db::FeatureMigration)]
}

/// 開 DB 並套上共用 schema＋`supervisor::store` migration（daemon 版 `app_ports_p1::open` 還套其他 feature 的 migration，這批測試不碰）。
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    db::open_with(path, supervisor_migrations()).await
}

/// 一個全新的測試環境：暫存目錄＋真 DB＋真 config。
pub async fn app_in(dir: &Path) -> Arc<TestSupervisorEnv> {
    let pool = open(&dir.join("test.sqlite")).await.unwrap();
    let cfg = ConfigStore::load_with_hooks(dir.join("config.toml"), Arc::new(NoHooks)).await.unwrap();
    Arc::new(TestSupervisorEnv { db: pool, cfg, data_dir: dir.to_path_buf(), events: Default::default(), seq: AtomicU64::new(0) })
}

/// 同 daemon `testing::arm_foreign_writer`：在 race point 上從另一條連線寫一筆（只有測試用的表），看守衛有沒有擋住。
pub fn arm_foreign_writer(file: &Path, point: &'static str, key: &str) -> Arc<StdMutex<Option<bool>>> {
    use sqlx::ConnectOptions;
    let seen = Arc::new(StdMutex::new(None::<bool>));
    let (file, cell) = (file.to_path_buf(), seen.clone());
    am_base::race_point::arm(point, key, move || async move {
        let mut other = sqlx::sqlite::SqliteConnectOptions::new().filename(&file).busy_timeout(std::time::Duration::ZERO).connect().await.unwrap();
        let wrote = async {
            sqlx::query("BEGIN IMMEDIATE").execute(&mut other).await?;
            sqlx::query("CREATE TABLE IF NOT EXISTS am_test_foreign_writes (at TEXT NOT NULL)").execute(&mut other).await?;
            sqlx::query("INSERT INTO am_test_foreign_writes (at) VALUES (?)").bind(db::now()).execute(&mut other).await?;
            sqlx::query("COMMIT").execute(&mut other).await?;
            Ok::<_, sqlx::Error>(())
        }
        .await
        .is_ok();
        *cell.lock().unwrap() = Some(wrote);
    });
    seen
}
