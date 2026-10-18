//! P1（DB／store）的 composition 端接縫：各 feature 自己那幾張表的 migration 清單與順序。
//!
//! `db` 只建共用的 schema（以及 `turns` 狀態守衛、保溫標記 trigger）；feature 表的 `migrate(pool)` 由這裡以**固定順序**提供給
//! [`crate::db::open_with`]。順序、失敗語意（任何一步回錯整個 open 就失敗、`user_version` 不蓋）與搬出來之前完全一樣——
//! 這份清單就是原本 `db::apply_migrations` 尾巴那 13 行。

use crate::db::{FeatureMigration, FeatureMigrations, MigrationFuture};
use sqlx::SqlitePool;

#[cfg(test)]
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
#[cfg(test)]
use std::{
    collections::HashMap,
    io::Write,
    path::Path,
    sync::{Arc, OnceLock},
};

/// 一支 `migrate(pool)` 包成 db 認得的函式指標（不擷取任何東西，所以能放進 `static`）。
macro_rules! feature {
    ($name:literal, $migrate:path) => {{
        fn run(pool: &SqlitePool) -> MigrationFuture<'_> {
            Box::pin($migrate(pool))
        }
        ($name, run as FeatureMigration)
    }};
}

/// 開 DB 並套上全部 migration（共用 schema＋這份 feature 清單）。正式啟動走 `startup::open_instance`（明確傳這份清單給 `db::open_with`）；
/// 測試與工具路徑用這支，省得每個呼叫端都帶清單。
#[cfg(test)]
pub(crate) async fn open(path: &std::path::Path) -> anyhow::Result<SqlitePool> {
    if path.exists() {
        return open_uncached(path).await;
    }

    let template = test_schema_template().await?;
    if path.exists() {
        return open_uncached(path).await;
    }
    if !publish_test_template(path, &template)? {
        return open_uncached(path).await;
    }
    open_template_copy(path).await
}

#[cfg(test)]
fn publish_test_template(path: &Path, template: &[u8]) -> std::io::Result<bool> {
    // Publish only a complete temporary file, and let hard_link fail if another opener won.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "database path has no file name")
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        crate::db::ulid()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let write = file.write_all(template);
    drop(file);

    let publish = match write {
        Err(error) => Err(error),
        Ok(()) => match std::fs::hard_link(&temporary, path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        },
    };
    let _ = std::fs::remove_file(temporary);
    publish
}

#[cfg(test)]
pub(crate) async fn open_uncached(path: &std::path::Path) -> anyhow::Result<SqlitePool> {
    crate::db::open_with(path, FEATURE_MIGRATIONS).await
}

#[cfg(test)]
async fn open_template_copy(path: &std::path::Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(10));
    Ok(SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?)
}

#[cfg(test)]
async fn test_schema_template() -> anyhow::Result<Arc<[u8]>> {
    static TEMPLATES: OnceLock<tokio::sync::Mutex<HashMap<String, Arc<[u8]>>>> = OnceLock::new();
    let expected = crate::db::schema_guard::expected(FEATURE_MIGRATIONS).await?;
    let fingerprint = crate::db::schema_guard::fingerprint(expected);
    let history = crate::db::SCHEMA_HISTORY
        .iter()
        .map(|(version, hash)| format!("{version}:{hash}"))
        .collect::<Vec<_>>()
        .join(",");
    let features = FEATURE_MIGRATIONS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(",");
    let key = format!(
        "version={}:history={history}:features={features}:schema={fingerprint}",
        crate::db::SCHEMA_VERSION
    );
    let cache = TEMPLATES.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
    let mut cache = cache.lock().await;
    if let Some(template) = cache.get(&key) {
        return Ok(template.clone());
    }

    // Use the normal full migration and schema check before serializing the stamped schema.
    let options = SqliteConnectOptions::new()
        .in_memory(true)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    crate::db::migrate_for_test(&pool, FEATURE_MIGRATIONS).await?;
    let mut connection = pool.acquire().await?;
    let template: Arc<[u8]> = Arc::from(&*connection.serialize(None).await?);
    drop(connection);
    pool.close().await;
    cache.insert(key, template.clone());
    Ok(template)
}

/// 順序即執行順序：先動 `supervisor::store`（它建的表別的 migration 可能引用），最後 `share::store`。
pub(crate) static FEATURE_MIGRATIONS: FeatureMigrations = &[
    feature!("supervisor::store", crate::supervisor::store::migrate),
    feature!("read_marks", crate::read_marks::migrate),
    feature!("fork_ops", crate::fork_ops::migrate),
    feature!("remote_purge", crate::remote_purge::migrate),
    feature!("panes", crate::panes::migrate),
    feature!("herdr_maintenance", crate::herdr_maintenance::migrate),
    feature!("mission::store", crate::mission::store::migrate),
    feature!("hook_inbox", crate::hook_inbox::migrate),
    feature!("build_scheduler", crate::build_scheduler::migrate),
    feature!("release_triage::ledger", crate::release_triage::ledger::migrate),
    feature!("judge", crate::judge::migrate),
    feature!("cli_update", crate::cli_update::migrate),
    feature!("codex_steer", crate::lifecycle::codex_steer::migrate),
    feature!("share::store", crate::share::store::migrate),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::sync::{Barrier, Mutex};

    fn tmp_db(tag: &str) -> std::path::PathBuf {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-p1-{tag}-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("t.sqlite3")
    }

    async fn tables(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").fetch_all(pool).await.unwrap()
    }

    /// 清單照原本的順序（`apply_migrations` 尾巴那 13 行，之後新增的 `codex_steer` 插在 `cli_update` 與 `share::store` 之間）。
    #[test]
    fn the_feature_migrations_keep_their_original_order() {
        let names: Vec<&str> = FEATURE_MIGRATIONS.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "supervisor::store",
                "read_marks",
                "fork_ops",
                "remote_purge",
                "panes",
                "herdr_maintenance",
                "mission::store",
                "hook_inbox",
                "build_scheduler",
                "release_triage::ledger",
                "judge",
                "cli_update",
                "codex_steer",
                "share::store"
            ]
        );
    }

    /// issue #1144：資料目錄含 `?`、`%XX` 時開的必須是那個檔（以前 URL 解析會把 `?mode=memory` 當參數、`%41` 解成 `A`）。
    #[tokio::test]
    async fn a_database_path_with_url_metacharacters_opens_that_exact_file() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-db-path-{}", db::ulid())));
        let sub = dir.join("weird%41?mode=memory");
        std::fs::create_dir_all(&sub).unwrap();
        let path = sub.join("am.db");
        let pool = db::open_with(&path, &[]).await.expect("路徑含 URL 特殊字元也要開得起來");
        assert!(path.exists(), "開的就是這個檔");
        assert!(!dir.join("weirdA").exists(), "%41 不能被 percent-decode 到別的路徑");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0);
    }

    /// db 自己只認共用 schema：不帶 feature 清單就只有共用的表（而且自己對自己的標準答案不會報漂移）；`db::open` 帶 composition 的清單，什麼都有。
    #[tokio::test]
    async fn db_alone_has_only_the_shared_schema_and_open_adds_every_feature_table() {
        let core = db::open_with(&tmp_db("core"), &[]).await.expect("shared schema alone is a consistent database");
        let core_tables = tables(&core).await;
        for t in ["bots", "turns", "runs", "messages"] {
            assert!(core_tables.iter().any(|n| n == t), "{t} 缺：{core_tables:?}");
        }
        for t in ["shared_bots", "panes", "bot_reads"] {
            assert!(!core_tables.iter().any(|n| n == t), "db 單獨不該建 feature 的表 {t}");
        }

        let full = crate::app_ports_p1::open_uncached(&tmp_db("full")).await.unwrap();
        let names = tables(&full).await;
        for t in ["bots", "turns", "shared_bots", "panes", "bot_reads"] {
            assert!(names.iter().any(|n| n == t), "{t} 缺：{names:?}");
        }
    }

    #[tokio::test]
    async fn fresh_test_opens_copy_an_independent_current_schema_template() {
        let first_path = tmp_db("template-first");
        let second_path = tmp_db("template-second");
        let first = open(&first_path).await.unwrap();
        let second = open(&second_path).await.unwrap();

        crate::db::schema_guard::check_drift_with(&first, FEATURE_MIGRATIONS).await.unwrap();
        crate::db::schema_guard::check_drift_with(&second, FEATURE_MIGRATIONS).await.unwrap();
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&first)
            .await
            .unwrap();
        assert_eq!(version, crate::db::SCHEMA_VERSION);
        sqlx::query(
            "INSERT INTO projects (id,path,label,created_at) VALUES ('template-only','/tmp/template','template',?)",
        )
        .bind(crate::db::now())
        .execute(&first)
        .await
        .unwrap();
        let second_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE id='template-only'")
                .fetch_one(&second)
                .await
                .unwrap();
        assert_eq!(second_count, 0, "each copied database is independently writable");
        first.close().await;
        second.close().await;
        let _ = std::fs::remove_file(first_path);
        let _ = std::fs::remove_file(second_path);
    }

    #[tokio::test]
    async fn concurrent_template_publishers_do_not_overwrite_committed_data() {
        let path = tmp_db("template-publish-race");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TABLE committed (value INTEGER NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO committed (value) VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let publishers = 8;
        let barrier = Arc::new(Barrier::new(publishers));
        let mut tasks = Vec::with_capacity(publishers);
        for _ in 0..publishers {
            let path = path.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::task::spawn_blocking(move || {
                barrier.wait();
                publish_test_template(&path, b"replacement template")
            }));
        }
        for task in tasks {
            assert!(
                !task.await.unwrap().unwrap(),
                "an existing database must never be replaced"
            );
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(false),
            )
            .await
            .unwrap();
        let value: i64 = sqlx::query_scalar("SELECT value FROM committed")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(value, 1);
        pool.close().await;
        let _ = std::fs::remove_file(path);
    }

    /// 失敗語意：任何一個 feature migration 回錯，`open` 就失敗，`user_version` 不蓋（回滾用的舊 binary 不會被版本閘擋住）。
    #[tokio::test]
    async fn a_failing_feature_migration_fails_the_open_and_leaves_the_version_unstamped() {
        static CALLS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        fn first(_: &SqlitePool) -> MigrationFuture<'_> {
            Box::pin(async {
                CALLS.lock().unwrap().push("first");
                Ok(())
            })
        }
        fn boom(_: &SqlitePool) -> MigrationFuture<'_> {
            Box::pin(async {
                CALLS.lock().unwrap().push("boom");
                anyhow::bail!("feature boom")
            })
        }
        fn never(_: &SqlitePool) -> MigrationFuture<'_> {
            Box::pin(async {
                CALLS.lock().unwrap().push("never");
                Ok(())
            })
        }
        let path = tmp_db("fail");
        let err = db::open_with(&path, &[("first", first), ("boom", boom), ("never", never)]).await.err().expect("must fail").to_string();
        assert!(err.contains("feature boom"), "{err}");
        assert_eq!(*CALLS.lock().unwrap(), ["first", "boom"], "出錯就停，後面的不跑");
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display())).await.unwrap();
        let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(version, 0, "失敗時不能宣稱已經是新版");
    }
}
