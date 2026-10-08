//! P1（DB／store）的 composition 端接縫：各 feature 自己那幾張表的 migration 清單與順序。
//!
//! `db` 只建共用的 schema（以及 `turns` 狀態守衛、保溫標記 trigger）；feature 表的 `migrate(pool)` 由這裡以**固定順序**提供給
//! [`crate::db::open_with`]。順序、失敗語意（任何一步回錯整個 open 就失敗、`user_version` 不蓋）與搬出來之前完全一樣——
//! 這份清單就是原本 `db::apply_migrations` 尾巴那 13 行。

use crate::db::{FeatureMigration, FeatureMigrations, MigrationFuture};
use sqlx::SqlitePool;

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
    crate::db::open_with(path, FEATURE_MIGRATIONS).await
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
    use std::sync::Mutex;

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

        let full = crate::app_ports_p1::open(&tmp_db("full")).await.unwrap();
        let names = tables(&full).await;
        for t in ["bots", "turns", "shared_bots", "panes", "bot_reads"] {
            assert!(names.iter().any(|n| n == t), "{t} 缺：{names:?}");
        }
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
