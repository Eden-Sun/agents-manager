
    /// 同一毫秒內 `db::ulid()` 要照產生順序遞增（行程內、跨執行緒也一樣），`ORDER BY created_at, id` 與前端的 `(created_at, id)`
    /// 才不會把同毫秒的兩列排反。`Ulid::new()` 做不到：同一毫秒內的隨機段是亂的。
    #[test]
    fn ulids_from_one_process_ascend_within_a_millisecond() {
        let batches: Vec<Vec<String>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..4).map(|_| s.spawn(|| (0..5000).map(|_| ulid()).collect::<Vec<_>>())).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for b in &batches {
            assert!(b.windows(2).all(|w| w[0] < w[1]), "同一個執行緒產生的 id 必須嚴格遞增");
        }
        let mut all: Vec<&String> = batches.iter().flatten().collect();
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n, "跨執行緒也不能重複");
        // 對照：沒有這層的話，同一毫秒內會有降序（這個 crate 的隨機段不遞增）。
        let plain: Vec<String> = (0..5000).map(|_| ulid::Ulid::new().to_string()).collect();
        assert!(plain.windows(2).any(|w| w[0][..10] == w[1][..10] && w[0] > w[1]), "前提：Ulid::new() 在同一毫秒內會降序");
    }

    /// issue #101：時間戳只有**一種**格式，而且那個格式必須讓「字典序＝時間序」。
    ///
    /// 很多判斷是拿這些字串在 SQL 裡直接比大小的，所以這不是風格問題：
    /// 寬度一變（秒 vs 毫秒）同一秒內就會比錯，格式一變成帶位移（`+08:00`）會差到**幾小時**。
    #[test]
    fn every_timestamp_has_the_one_canonical_shape() {
        let samples = [now(), iso_in(0), iso_in(60), iso_in(-60), iso_at(chrono::Utc::now())];
        for s in &samples {
            assert_eq!(s.len(), 24, "固定寬度才能比字串：{s}");
            assert!(s.ends_with('Z'), "一律 UTC 的 Z，不可以是 +08:00 這種：{s}");
            assert_eq!(&s[10..11], "T", "{s}");
            assert_eq!(&s[19..20], ".", "到毫秒：{s}");
            // 真的是這個時間，不是長得像而已。
            chrono::DateTime::parse_from_rfc3339(s).unwrap_or_else(|e| panic!("{s} 解不開：{e}"));
        }
    }

    /// 生產程式碼**只准**在 `db.rs` 決定時間戳格式（issue #101）。
    ///
    /// 上面兩條只證明 `db::` 這幾支對；要是別的模組自己 `to_rfc3339_opts(Secs)`，那兩條照樣綠。
    /// 這一條掃原始碼把那條路堵死——今晚的教訓：守衛沒被測到，跟沒有守衛是一樣的。
    /// `#[cfg(test)]` 之後的不算：測試本來就要造舊格式的資料（`roles.rs` 那條混存測試就是）。
    #[test]
    fn only_db_rs_decides_the_timestamp_format() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);
        let mut strays: Vec<String> = Vec::new();
        for f in files {
            let at = f.strip_prefix(&root).unwrap().display().to_string();
            if at == "db.rs" {
                continue; // 格式就是在這裡定義的
            }
            let src = std::fs::read_to_string(&f).unwrap();
            // 測試區塊以後不管：測試要造舊格式的列才測得到混存。
            let prod = src.split("#[cfg(test)]").next().unwrap_or("");
            for (i, line) in prod.lines().enumerate() {
                // 只擋真的會出事的兩種：
                //  - `Secs`：寬度跟毫秒不一樣，同一秒內字串比就會判錯（本 issue 的病灶）。
                //  - 裸的 `to_rfc3339()`：產出 `+00:00` 而不是 `Z`，字典序跟時間序會差到**幾小時**。
                // 直接寫 `Millis` 的雖然該改用 `db::` 的三支，但寬度是對的、不會判錯，先不擋。
                let bad = line.contains("SecondsFormat::Secs")
                    || (line.contains("to_rfc3339()") && !line.contains("to_rfc3339_opts"));
                if bad {
                    strays.push(format!("{at}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        assert!(
            strays.is_empty(),
            "時間戳一律用 db::now()／db::iso_in()／db::iso_at()（固定寬度、以 Z 結尾）。\n\
             `SecondsFormat::Secs` 同一秒內會判錯；裸的 `to_rfc3339()` 產出 +00:00，字典序會差到幾小時：\n{}",
            strays.join("\n")
        );
    }

    /// 字典序要等於時間序——這是所有 `WHERE ... <= ?` 成立的前提。
    #[test]
    fn lexicographic_order_is_chronological_order() {
        let base = chrono::DateTime::parse_from_rfc3339("2026-09-18T07:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let mut prev = iso_at(base - chrono::Duration::days(400));
        for ms in [1i64, 999, 1_000, 60_000, 3_600_000, 86_400_000] {
            let cur = iso_at(base + chrono::Duration::milliseconds(ms));
            assert!(prev < cur, "{prev} 應該排在 {cur} 前面");
            prev = cur;
        }
        // 跨年、跨月也要成立（補零）。
        assert!(iso_at(base) < iso_at(base + chrono::Duration::days(200)));
    }

    use super::*;

    fn tmp_dir() -> std::path::PathBuf {
        let d = crate::testing::track(std::env::temp_dir().join(format!("am-db-test-{}", ulid())));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 還沒 migrate 的 WAL 檔（跟 [`open`] 同樣的連線設定），連同它的 race point 鑰匙。
    async fn bare_wal_pool() -> (SqlitePool, String) {
        let file = tmp_dir().join("t.sqlite3");
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", file.display()))
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
        let key = file_key(&mut *pool.acquire().await.unwrap()).await;
        (pool, key)
    }

    /// #831：migrate 讀完 `user_version`、還沒套 schema 的那一瞬，另一個 writer（換版時還沒收完的舊 daemon）commit 了一筆。
    /// deferred 交易這時第一句 DDL 直接 517，daemon 起不來；寫鎖從讀之前就拿著，對方等，migrate 照樣套完。
    #[tokio::test]
    async fn an_unrelated_writer_between_the_version_read_and_the_schema_does_not_fail_the_migration() {
        let (pool, key) = bare_wal_pool().await;
        let other = crate::testing::arm_foreign_writer(Path::new(&key), "schema_after_version_read", &key);
        apply_migrations(&pool).await.expect("an unrelated writer must not fail the migration");
        assert_eq!(*other.lock().unwrap(), Some(false), "the migration holds the write lock from its version read on; the other writer waits");
        assert!(columns(&pool, "turns").await.contains(&"awaits_idle".to_string()));
        pool.close().await;
    }

    async fn columns(pool: &SqlitePool, table: &str) -> Vec<String> {
        sqlx::query_scalar::<_, String>(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// agy：v41 以前的庫，`bots.kind` 的 CHECK 只收三種。開機時就地放寬成四種：既有的 bot 與指著它的列原樣在、外鍵沒壞、
    /// 之後收得下 `agy`，而且放寬後的定義跟全新 DB 一字不差（`schema_guard`）；再開一次不動。
    #[tokio::test]
    async fn opening_a_pre_agy_db_widens_the_bots_kind_check_in_place() {
        let dir = tmp_dir();
        let path = dir.join("pre-agy.sqlite3");
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let old = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
        for stmt in SCHEMA.replace("'claude','codex','grok','agy'", "'claude','codex','grok'").split(";\n") {
            let s = stmt.trim();
            if !s.is_empty() {
                sqlx::query(s).execute(&old).await.unwrap();
            }
        }
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES ('p1','/tmp/p','p','local','2026-10-01T00:00:00Z')").execute(&old).await.unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','old','grok','tok','2026-10-01T00:00:00Z')").execute(&old).await.unwrap();
        sqlx::query("INSERT INTO runs (id, bot_id, state, started_at) VALUES ('r1','b1','stopped','2026-10-01T00:00:00Z')").execute(&old).await.unwrap();
        assert!(
            sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b2','p1','x','agy','tok','2026-10-01T00:00:00Z')").execute(&old).await.is_err(),
            "the old CHECK refuses agy"
        );
        sqlx::query("PRAGMA user_version = 41").execute(&old).await.unwrap();
        old.close().await;

        let current = open(&path).await.expect("a pre-agy DB must open");
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b2','p1','new','agy','tok','2026-10-01T00:00:00Z')")
            .execute(&current)
            .await
            .expect("the widened CHECK accepts agy");
        assert!(
            sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b3','p1','bad','gemini','tok','2026-10-01T00:00:00Z')").execute(&current).await.is_err(),
            "unknown kinds are still refused"
        );
        let kept: Vec<(String, String)> = sqlx::query_as("SELECT id, kind FROM bots ORDER BY id").fetch_all(&current).await.unwrap();
        assert_eq!(kept, [("b1".into(), "grok".into()), ("b2".into(), "agy".into())]);
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = 'b1'").fetch_one(&current).await.unwrap();
        assert_eq!(runs, 1, "the child table still points at bots");
        let violations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check").fetch_one(&current).await.unwrap();
        assert_eq!(violations, 0);
        let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&current).await.unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        crate::runners::am_base_tests::check_drift(&current).await.expect("the widened table must match schema_guard");
        current.close().await;
        // Idempotent: a second open does not touch it.
        let again = open(&path).await.expect("reopen");
        let sql: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'bots'").fetch_one(&again).await.unwrap();
        assert_eq!(sql.matches("'agy'").count(), 1);
        again.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// issue #635: migrate the official v29 DDL, including its bots foreign key, rows and
    /// explicit sqlite_master objects, to a host-scoped key.
    #[tokio::test]
    async fn opening_v29_rebuilds_spawn_hints_key_with_host_scope() {
        let dir = tmp_dir();
        let path = dir.join("spawn-hints.sqlite3");
        let old = open(&path).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp/p','p',?)")
            .bind(now())
            .execute(&old)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)")
            .bind(now())
            .execute(&old)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b2','p','b2','claude','t2',?)")
            .bind(now())
            .execute(&old)
            .await
            .unwrap();

        // This is the exact official v29 definition. It has host already, but its pane_id-only
        // primary key means a valid v29 DB cannot yet hold the same pane id on two hosts.
        sqlx::query("DROP TABLE spawn_hints").execute(&old).await.unwrap();
        sqlx::query(
            "CREATE TABLE spawn_hints (pane_id TEXT PRIMARY KEY, host TEXT NOT NULL, bot_id TEXT NOT NULL REFERENCES bots(id), created_at TEXT NOT NULL)",
        )
        .execute(&old)
        .await
        .unwrap();
        let rows = [
            ("w1:p2", "local", "b", "2026-09-20T10:00:00.000Z"),
            ("w2:p2", "remote-a", "b2", "2026-09-20T10:01:00.000Z"),
            ("w3:p3", "remote-b", "b", "2026-09-20T10:02:00.000Z"),
        ];
        for (pane_id, host, bot_id, created_at) in rows {
            sqlx::query("INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES (?, ?, ?, ?)")
                .bind(pane_id)
                .bind(host)
                .bind(bot_id)
                .bind(created_at)
                .execute(&old)
                .await
                .unwrap();
        }
        // Official v29 has no explicit spawn_hints index or trigger. Synthetic objects prove the
        // rebuild restores any objects that a future or operator-managed v29 database may have.
        sqlx::query("CREATE INDEX spawn_hints_test_host_created ON spawn_hints(host, created_at)")
            .execute(&old)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER spawn_hints_test_insert AFTER INSERT ON spawn_hints BEGIN SELECT 1; END",
        )
        .execute(&old)
        .await
        .unwrap();
        sqlx::query("PRAGMA user_version = 29").execute(&old).await.unwrap();

        // Fail in the middle of the transactional rebuild, immediately after DROP. Reopening a
        // raw pool must still show the complete committed v29 table and all its data/objects.
        let err = apply_migrations_failing_after_spawn_hints_drop(&old).await.expect_err("injected failure should abort migration");
        assert!(err.to_string().contains("injected failure after dropping old spawn_hints table"), "{err}");
        old.close().await;

        let inspect_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display())).unwrap()
            .create_if_missing(false)
            .foreign_keys(true);
        let inspect = SqlitePoolOptions::new().max_connections(1).connect_with(inspect_opts).await.unwrap();
        let old_ddl: String = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type='table' AND name='spawn_hints'")
            .fetch_one(&inspect)
            .await
            .unwrap();
        assert_eq!(
            old_ddl,
            "CREATE TABLE spawn_hints (pane_id TEXT PRIMARY KEY, host TEXT NOT NULL, bot_id TEXT NOT NULL REFERENCES bots(id), created_at TEXT NOT NULL)"
        );
        let rollback_rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT pane_id, host, bot_id, created_at FROM spawn_hints ORDER BY pane_id",
        )
        .fetch_all(&inspect)
        .await
        .unwrap();
        assert_eq!(rollback_rows, rows.map(|(pane_id, host, bot_id, created_at)| {
            (pane_id.into(), host.into(), bot_id.into(), created_at.into())
        }));
        let rollback_objects: Vec<(String, String)> = sqlx::query_as(
            "SELECT type, name FROM sqlite_master WHERE tbl_name='spawn_hints' AND type IN ('index','trigger') ORDER BY type, name",
        )
        .fetch_all(&inspect)
        .await
        .unwrap();
        assert_eq!(
            rollback_objects,
            [
                ("index".into(), "spawn_hints_test_host_created".into()),
                ("index".into(), "sqlite_autoindex_spawn_hints_1".into()),
                ("trigger".into(), "spawn_hints_test_insert".into()),
            ]
        );
        let rollback_fk: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT \"table\", \"from\", \"to\" FROM pragma_foreign_key_list('spawn_hints')",
        )
        .fetch_all(&inspect)
        .await
        .unwrap();
        assert_eq!(rollback_fk, [("bots".into(), "bot_id".into(), "id".into())]);
        let leftover_new_table: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='spawn_hints_new'",
        )
        .fetch_one(&inspect)
        .await
        .unwrap();
        assert_eq!(leftover_new_table, 0);
        let rollback_version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&inspect).await.unwrap();
        assert_eq!(rollback_version, 29);
        inspect.close().await;

        let current = open(&path).await.expect("v29 DB should migrate on open");
        let pk: Vec<(String, i64)> = sqlx::query_as("SELECT name, pk FROM pragma_table_info('spawn_hints') WHERE pk > 0 ORDER BY pk")
            .fetch_all(&current)
            .await
            .unwrap();
        assert_eq!(pk, [("host".into(), 1), ("pane_id".into(), 2)]);
        let actual_rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT pane_id, host, bot_id, created_at FROM spawn_hints ORDER BY pane_id",
        )
            .fetch_all(&current)
            .await
            .unwrap();
        assert_eq!(actual_rows, rows.map(|(pane_id, host, bot_id, created_at)| {
            (pane_id.into(), host.into(), bot_id.into(), created_at.into())
        }));
        let fks: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT \"table\", \"from\", \"to\" FROM pragma_foreign_key_list('spawn_hints')",
        )
        .fetch_all(&current)
        .await
        .unwrap();
        assert_eq!(fks, [("bots".into(), "bot_id".into(), "id".into())]);
        let objects: Vec<(String, String)> = sqlx::query_as(
            "SELECT type, name FROM sqlite_master WHERE tbl_name='spawn_hints' AND type IN ('index','trigger') ORDER BY type, name",
        )
        .fetch_all(&current)
        .await
        .unwrap();
        assert_eq!(
            objects,
            [
                ("index".into(), "spawn_hints_test_host_created".into()),
                ("index".into(), "sqlite_autoindex_spawn_hints_1".into()),
                ("trigger".into(), "spawn_hints_test_insert".into()),
            ]
        );

        // Same pane ids on different hosts now coexist after migrating the official v29 fixture.
        for (host, bot_id) in [("local", "b2"), ("remote-a", "b")] {
            sqlx::query("INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES ('same:pane', ?, ?, ?)")
                .bind(host)
                .bind(bot_id)
                .bind("2026-09-20T11:00:00.000Z")
                .execute(&current)
                .await
                .unwrap();
        }
        let cross_host_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM spawn_hints WHERE pane_id='same:pane'")
            .fetch_one(&current)
            .await
            .unwrap();
        assert_eq!(cross_host_rows, 2);

        let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&current).await.unwrap();
        assert_eq!(version, SCHEMA_VERSION, "migrated v29 DB must be stamped with the current version");
        crate::runners::am_base_tests::check_drift(&current).await.expect("migrated v29 DB must match schema_guard");
        current.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The official v29 PK prevents duplicate pane ids, but salvage duplicate-shaped legacy data
    /// defensively: retain the newest row per (host, pane_id), while preserving same ids on other hosts.
    #[tokio::test]
    async fn opening_unconstrained_v29_spawn_hints_deduplicates_by_host_and_keeps_newest() {
        let dir = tmp_dir();
        let path = dir.join("spawn-hints-duplicates.sqlite3");
        let old = open(&path).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp/p','p',?)")
            .bind(now())
            .execute(&old)
            .await
            .unwrap();
        for (id, token) in [("b", "t"), ("b2", "t2")] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?, 'p', ?, 'claude', ?, ?)")
                .bind(id)
                .bind(id)
                .bind(token)
                .bind(now())
                .execute(&old)
                .await
                .unwrap();
        }
        sqlx::query("DROP TABLE spawn_hints").execute(&old).await.unwrap();
        sqlx::query(
            "CREATE TABLE spawn_hints (pane_id TEXT NOT NULL, host TEXT NOT NULL, bot_id TEXT NOT NULL REFERENCES bots(id), created_at TEXT NOT NULL)",
        )
        .execute(&old)
        .await
        .unwrap();
        for (pane_id, host, bot_id, created_at) in [
            ("same:pane", "local", "b", "2026-09-20T10:00:00.000Z"),
            ("same:pane", "local", "b2", "2026-09-20T10:02:00.000Z"),
            ("same:pane", "remote-a", "b", "2026-09-20T10:01:00.000Z"),
        ] {
            sqlx::query("INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES (?, ?, ?, ?)")
                .bind(pane_id)
                .bind(host)
                .bind(bot_id)
                .bind(created_at)
                .execute(&old)
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version = 29").execute(&old).await.unwrap();
        old.close().await;

        let current = open(&path).await.expect("duplicate rows must be deduplicated, not abort migration");
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT pane_id, host, bot_id, created_at FROM spawn_hints ORDER BY host",
        )
        .fetch_all(&current)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                ("same:pane".into(), "local".into(), "b2".into(), "2026-09-20T10:02:00.000Z".into()),
                ("same:pane".into(), "remote-a".into(), "b".into(), "2026-09-20T10:01:00.000Z".into()),
            ]
        );
        crate::runners::am_base_tests::check_drift(&current).await.expect("deduplicated migration must match schema_guard");
        current.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 往 `SCHEMA` 加欄位卻忘了補 ALTER 名單：以前在開發者機器上一律是綠的（每個測試都開新 DB），
    /// 到使用者那裡才炸成 `SELECT *` 的 FromRow 失敗、daemon 起不來。現在 migrate 自己對帳。
    #[tokio::test]
    async fn a_column_the_alter_list_forgot_is_caught_before_the_user_sees_it() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-drift-{}", ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        // 舊資料庫：`bots` 少了一堆後來才加的欄位，而且 CREATE TABLE IF NOT EXISTS 不會補。
        {
            let old = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{}?mode=rwc", path.display()))
                .await
                .unwrap();
            // 少的是 `env_json`：它在 SCHEMA 裡、不在 ALTER 名單裡，也沒有索引用到它——
            // 正好是「加欄位忘了補 ALTER」會留下的形狀。索引要用的欄位照給，才測得到這個檢查本身。
            sqlx::query(
                "CREATE TABLE bots (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, name TEXT NOT NULL,
                   kind TEXT NOT NULL, model TEXT, effort TEXT, fast INTEGER NOT NULL DEFAULT 0, persona TEXT,
                   args_json TEXT NOT NULL DEFAULT '[]', autostart INTEGER NOT NULL DEFAULT 0,
                   inject_hooks INTEGER NOT NULL DEFAULT 1, auto_approve INTEGER NOT NULL DEFAULT 1,
                   identity TEXT, managed_by TEXT NOT NULL DEFAULT 'user', cwd TEXT, herdr_session TEXT,
                   parent_bot_id TEXT, hook_token TEXT NOT NULL, deleted_at TEXT, created_at TEXT NOT NULL,
                   is_primary INTEGER NOT NULL DEFAULT 0, position INTEGER NOT NULL DEFAULT 0)",
            )
            .execute(&old)
            .await
            .unwrap();
            old.close().await;
        }
        let err = open(&path).await.expect_err("少欄位的舊 DB 不該靜靜開起來").to_string();
        assert!(err.contains("schema drift"), "{err}");
        assert!(err.contains("bots."), "錯誤訊息要指名是哪張表：{err}");
        assert!(err.contains("ALTER TABLE"), "要告訴人怎麼修：{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 一個字都沒寫進去的那次重送要退還額度：`MAX_PROMPT_RESENDS` 是 1，
    /// 被「框裡剛好有字」這種兩秒後就消失的原因吃掉，等於永遠補救不了。
    #[tokio::test]
    async fn a_resend_that_wrote_nothing_gives_the_budget_back() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-refund-{}", ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = open(&dir.join("t.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
        let conv = conversation_id(&pool, "b").await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES ('t',?,'web','in_flight','ok',?)")
            .bind(&conv).bind(now()).execute(&pool).await.unwrap();

        assert!(claim_resend(&pool, "t", 1).await.unwrap(), "第一次拿得到");
        assert!(!claim_resend(&pool, "t", 1).await.unwrap(), "額度只有一次");
        refund_resend(&pool, "t").await;
        assert!(claim_resend(&pool, "t", 1).await.unwrap(), "退還之後還有一次");
        refund_resend(&pool, "t").await;
        refund_resend(&pool, "t").await;
        let n: i64 = sqlx::query_scalar("SELECT resend_count FROM turns WHERE id='t'").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "退還不會退成負數");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// issue #93：前端算「跑了多久」的起點要用這一欄，不能自己用瀏覽器時鐘瞎猜。重複寫同一個值
    /// （pane 又印了一行一樣的狀態）不能推遲起點；真的變了（包含繞了一圈回到原值）才推進。
    #[tokio::test]
    async fn agent_status_since_only_moves_when_the_status_actually_changes() {
        let dir = tmp_dir();
        let pool = open(&dir.join("t.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,started_at) VALUES ('r','b','running',?)").bind(now()).execute(&pool).await.unwrap();
        let since0: Option<String> = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert_eq!(since0, None, "剛建的 run 還沒真的變過狀態");

        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='r'").execute(&pool).await.unwrap();
        let since1: String = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert!(!since1.is_empty());

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='r'").execute(&pool).await.unwrap();
        let since2: String = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert_eq!(since1, since2, "同值重寫（重複的 pane 狀態行）不算改變，起點不動");

        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id='r'").execute(&pool).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='r'").execute(&pool).await.unwrap();
        let since3: String = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert_ne!(since1, since3, "又轉回 working：這是新的一段連續 working，起點要跟著換");

        let r = sqlx::query_as::<_, Run>("SELECT * FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert_eq!(r.agent_status_since.as_deref(), Some(since3.as_str()), "FromRow 讀得到新欄位");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// issue #88：`attachments.state` 是後補的欄位，舊 DB（沒有這一欄）打開時要補上，而且舊列（都是
    /// 舊流程「檔案寫完才 insert」留下來的，insert 成功就代表已經完整）一律回填成 `'ready'`，不能變成
    /// `NULL` 或別的預設值被 `resolve`/`read`/`bind` 擋掉。
    #[tokio::test]
    async fn an_old_database_gains_attachments_state_and_backfills_ready() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-attach-state-{}", ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)")
                .bind(now())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO attachments (id,bot_id,name,mime,size,local_path,agent_path,host,created_at)
                 VALUES ('a','b','n','image/png',1,'/l','/r','local',?)",
            )
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
            // 做成上一版的形狀：這一欄還不存在。
            sqlx::query("ALTER TABLE attachments DROP COLUMN state").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "attachments", "state").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB 照常開起來");
        assert!(has_column(&pool, "attachments", "state").await.unwrap(), "開的時候補上");
        let state: String = sqlx::query_scalar("SELECT state FROM attachments WHERE id='a'").fetch_one(&pool).await.unwrap();
        assert_eq!(state, "ready", "舊流程 insert 成功就代表檔案已經寫完，回填成 ready 而不是留白");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 升級路徑：trigger 是在 ALTER 名單補完欄位之後才建的，不能反過來（trigger body 引用一個舊
    /// DB 當下還沒有的欄位）。
    #[tokio::test]
    async fn an_old_database_without_the_column_still_gets_a_working_trigger() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-status-since-upgrade-{}", ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,started_at) VALUES ('r','b','running',?)").bind(now()).execute(&pool).await.unwrap();
            // 做成上一版的形狀：欄位跟 trigger 都還不存在。
            sqlx::query("DROP TRIGGER IF EXISTS runs_agent_status_since").execute(&pool).await.unwrap();
            sqlx::query("ALTER TABLE runs DROP COLUMN agent_status_since").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "runs", "agent_status_since").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB（缺欄位也缺 trigger）照常開起來");
        assert!(has_column(&pool, "runs", "agent_status_since").await.unwrap(), "開的時候補上欄位");
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id='r'").execute(&pool).await.unwrap();
        let since: Option<String> = sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id='r'").fetch_one(&pool).await.unwrap();
        assert!(since.is_some(), "trigger 也補上了，不是只有欄位");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// schema 變更（additive）`turns.delivered_at`：沒有這一欄的舊 DB 開起來會補上，舊列是 NULL，`SELECT *` 照樣讀得進 `Turn`。
    #[tokio::test]
    async fn an_old_database_gains_turns_delivered_at_on_open() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-delivered-at-{}", ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
            let conv = conversation_id(&pool, "b").await.unwrap();
            sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,created_at) VALUES ('t',?,'web','completed','ok',?)")
                .bind(&conv).bind(now()).execute(&pool).await.unwrap();
            // 做成上一版的形狀：這一欄還不存在。
            sqlx::query("ALTER TABLE turns DROP COLUMN delivered_at").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "turns", "delivered_at").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB 照常開起來");
        assert!(has_column(&pool, "turns", "delivered_at").await.unwrap(), "開的時候補上");
        let t = sqlx::query_as::<_, Turn>("SELECT * FROM turns WHERE id='t'").fetch_one(&pool).await.unwrap();
        assert_eq!(t.delivered_at, None, "舊列沒有送出時間：重啟補 watchdog 退回看 created_at");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// schema 變更（additive）`bots.instruction_files`（issue #213，已棄用不再讀寫）：沒有這一欄的舊 DB 開起來仍會補上，
    /// `SELECT *` 照樣讀得進 `Bot`。少了這條 ALTER，`check_schema_drift` 會讓 daemon 起不來。
    #[tokio::test]
    async fn an_old_database_gains_bots_instruction_files_on_open() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-instruction-files-{}", ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now()).execute(&pool).await.unwrap();
            // 做成上一版的形狀：這一欄還不存在。
            sqlx::query("ALTER TABLE bots DROP COLUMN instruction_files").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "bots", "instruction_files").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB 照常開起來");
        assert!(has_column(&pool, "bots", "instruction_files").await.unwrap(), "開的時候補上");
        let b = bot(&pool, "b").await.unwrap().expect("舊列還在");
        assert_eq!(b.name, "b");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// schema 變更（additive）`runs.runtime_identity`（issue #238）：沒有這一欄的舊 DB 開起來會補上，舊列是 NULL
    /// （＝沒記，額度照 bot 設定的身分算，跟加這一欄之前一樣），`SELECT *` 照樣讀得進 `Run`。
    #[tokio::test]
    async fn an_old_database_gains_runs_runtime_identity_on_open() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-runtime-identity-{}", ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite3");
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,identity,created_at) VALUES ('b','p','b','claude','t','cc1',?)").bind(now()).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,started_at) VALUES ('r','b','running',?)").bind(now()).execute(&pool).await.unwrap();
            // 做成上一版的形狀：這一欄還不存在。
            sqlx::query("ALTER TABLE runs DROP COLUMN runtime_identity").execute(&pool).await.unwrap();
            assert!(!has_column(&pool, "runs", "runtime_identity").await.unwrap());
            pool.close().await;
        }
        let pool = open(&path).await.expect("舊 DB 照常開起來");
        assert!(has_column(&pool, "runs", "runtime_identity").await.unwrap(), "開的時候補上");
        let r = active_run(&pool, "b").await.unwrap().expect("舊列還在");
        assert_eq!((r.runtime_identity.clone(), r.started_identity()), (None, None), "舊列沒記：照 bot 設定的身分算");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 「這台有哪些身分在跑」（claude 探測拿它當登入證據）看的是 run 實際的身分（#238）：改了設定還沒重啟的是舊的那個；
    /// run 沒記的照 bot 設定的；起來時沒有身分的不算。
    #[tokio::test]
    async fn live_identities_are_the_ones_the_runs_started_with() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-live-identities-{}", ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = open(&dir.join("db.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
        for (bot, identity, run_identity) in [("a", Some("cc2"), Some("cc1")), ("b", Some("cc3"), None), ("c", Some("cc4"), Some(""))] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,identity,created_at) VALUES (?,'p',?,'claude','t',?,?)")
                .bind(bot)
                .bind(bot)
                .bind(identity)
                .bind(now())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO runs (id,bot_id,state,runtime_identity,started_at) VALUES (?,?,'running',?,?)")
                .bind(format!("r-{bot}"))
                .bind(bot)
                .bind(run_identity)
                .bind(now())
                .execute(&pool)
                .await
                .unwrap();
        }
        let live: Vec<String> = live_identities_on_host(&pool, crate::config::LOCAL_HOST).await.unwrap().into_iter().collect();
        assert_eq!(live, vec!["cc1".to_string(), "cc3".to_string()]);
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn open_is_idempotent() {
        let dir = tmp_dir();
        let file = dir.join("new.sqlite3");
        let p1 = open(&file).await.unwrap();
        let before = columns(&p1, "bots").await;
        p1.close().await;
        let p2 = open(&file).await.unwrap();
        assert_eq!(columns(&p2, "bots").await, before);
        p2.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// issue #474：pre-v2 的 `bot_previews` 沒有 `REFERENCES bots(id)`，外鍵又不能用 ALTER 加回去。
    /// migrate 要重建表補上，而且**不能把資料洗掉**——除了指不到 bot 的孤兒列（新表帶 FK、
    /// `foreign_keys` 開著，不濾掉的話 `INSERT … SELECT` 會整批失敗、整個 migrate 回滾）。
    ///
    /// 連同這張表自己的索引與 trigger 也要活下來：`DROP TABLE` 會一起帶走它們、`RENAME` 不會還回來
    /// （i406 在 #474 提的前瞻陷阱）。今天的 `bot_previews` 一個都沒有，所以測試自己造兩個。
    #[tokio::test]
    async fn v19_rebuilds_a_pre_v2_bot_previews_to_restore_its_foreign_key() {
        let dir = tmp_dir();
        let file = dir.join("pre-v2.sqlite3");
        // 先讓 migrate 建出完整的 schema，再把 `bot_previews` 換成 pre-v2 的形狀（沒有 FK）。
        // 手寫整份 `projects`／`bots` 會漏掉後來加的欄位（`projects.host` 之類），那些欄位上還有索引，
        // 下一次開 DB 就會炸在 `CREATE UNIQUE INDEX … ON projects(host, path)`——那是測試自己寫壞，
        // 不是 migrate 的問題。這樣做也更接近正式那顆：跑過歷代 migrate、只有那個外鍵一直缺。
        {
            let pool = open(&file).await.unwrap();
            for stmt in [
                "INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p','2026-01-01T00:00:00.000Z')",
                "INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b1','p','one','claude','t','2026-01-01T00:00:00.000Z')",
                "DROP TABLE bot_previews",
                "CREATE TABLE bot_previews (bot_id TEXT PRIMARY KEY, host TEXT NOT NULL, pane_id TEXT, port INTEGER, dir TEXT,
                   status TEXT NOT NULL, error TEXT, started_at TEXT, updated_at TEXT NOT NULL)",
                "INSERT INTO bot_previews (bot_id,host,pane_id,port,dir,status,error,started_at,updated_at)
                   VALUES ('b1','local','w1:p1',5173,'/tmp/x','running',NULL,'2026-01-01T00:00:00.000Z','2026-01-02T00:00:00.000Z')",
                // 孤兒：指不到任何 bot，補外鍵時要被丟掉。
                "INSERT INTO bot_previews (bot_id,host,pane_id,port,dir,status,error,started_at,updated_at)
                   VALUES ('gone','local','w1:p9',5174,'/tmp/y','off',NULL,NULL,'2026-01-02T00:00:00.000Z')",
                // 這張表自己的索引與 trigger：`DROP TABLE` 會一起帶走，重建完要照原樣回來（i406 在 #474
                // 提的前瞻陷阱）。今天的 `bot_previews` 一個都沒有，所以這裡自己造兩個來釘住行為。
                "CREATE INDEX bot_previews_by_host ON bot_previews(host)",
                "CREATE TRIGGER bot_previews_touch AFTER UPDATE ON bot_previews
                   BEGIN UPDATE bot_previews SET updated_at = updated_at WHERE bot_id = NEW.bot_id; END",
            ] {
                sqlx::query(stmt).execute(&pool).await.unwrap_or_else(|e| panic!("{stmt}: {e}"));
            }
            let fks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_list('bot_previews')").fetch_one(&pool).await.unwrap();
            assert_eq!(fks, 0, "前提：這時的 bot_previews 沒有外鍵");
            pool.close().await;
        }
        let pool = open(&file).await.expect("pre-v2 形狀的 bot_previews 要開得起來並且自己補好");
        let fks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_list('bot_previews')").fetch_one(&pool).await.unwrap();
        assert_eq!(fks, 1, "重建之後要有 bots(id) 那個外鍵");
        // 指得到 bot 的那一列要原封不動，連 ALTER 後來補的欄位都要有預設值。
        let row: (String, String, Option<i64>, String, String, String) = sqlx::query_as(
            "SELECT bot_id, host, port, status, updated_at, source FROM bot_previews",
        )
        .fetch_one(&pool)
        .await
        .expect("只該剩一列");
        assert_eq!(row.0, "b1");
        assert_eq!(row.1, "local");
        assert_eq!(row.2, Some(5173), "資料不能在重建時掉字");
        assert_eq!(row.3, "running");
        assert_eq!(row.4, "2026-01-02T00:00:00.000Z");
        assert_eq!(row.5, "spawned", "ALTER 補的欄位照樣拿到預設值");
        // 索引與 trigger 要原樣回來：`DROP TABLE` 帶走它們，`RENAME` 不會還回來。
        let extras: Vec<(String, String)> = sqlx::query_as(
            "SELECT type, name FROM sqlite_master
              WHERE tbl_name='bot_previews' AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            extras,
            vec![("index".to_string(), "bot_previews_by_host".to_string()), ("trigger".to_string(), "bot_previews_touch".to_string())],
            "重建前有的索引與 trigger 要一個不少"
        );
        // 重跑是 no-op：已經有 FK 就不再重建（否則每次開機都洗一次表）。
        pool.close().await;
        let pool = open(&file).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bot_previews").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1, "第二次開不該再動它");
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn v15_remaps_deprecated_models_including_soft_deleted_bots_idempotently() {
        let dir = tmp_dir();
        let pool = open(&dir.join("model-remap.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now()).execute(&pool).await.unwrap();
        for (id, kind, model, deleted) in [
            ("c1", "codex", "gpt-5.6-sol", None),
            ("c2", "codex", "gpt-5.6-terra", Some(now())),
            ("c3", "codex", "gpt-5.6-luna", None),
            ("a1", "claude", "opus", None),
            ("a2", "claude", "claude-opus-4-1", None),
        ] {
            sqlx::query("INSERT INTO bots (id,project_id,name,kind,model,hook_token,deleted_at,created_at) VALUES (?,'p',?,?,?,'t',?,?)")
                .bind(id).bind(id).bind(kind).bind(model).bind(deleted).bind(now()).execute(&pool).await.unwrap();
        }
        sqlx::query("PRAGMA user_version = 14").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        let values: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id,model FROM bots ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(values, vec![
            ("a1".into(), Some("claude-opus-5-5".into())),
            ("a2".into(), Some("claude-opus-4-1".into())),
            ("c1".into(), Some("gpt-6-sol".into())),
            ("c2".into(), Some("gpt-6-sol".into())),
            ("c3".into(), Some("gpt-6-luna".into())),
        ]);
        let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        // 蓋的是這顆 binary 的版本（之後再升版也一樣），不是寫死 15。
        assert_eq!(version, SCHEMA_VERSION);
        migrate(&pool).await.unwrap();
        let again: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id,model FROM bots ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(again, values);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 版本戳記要在子模組 migrate 與漂移核對之後才蓋：主交易 commit 了、子模組才失敗，DB 不能已經是新版
    /// （否則回滾用的舊 binary 被版本閘擋住）。
    #[tokio::test]
    async fn a_failed_submodule_migrate_leaves_the_version_unstamped() {
        let dir = tmp_dir();
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect(&format!("sqlite://{}?mode=rwc", dir.join("db.sqlite3").display())).await.unwrap();
        // hook_inbox::migrate 要建 index hook_events_dedupe；名字先被一張表占走，它會失敗，而主交易那時已經 commit。
        sqlx::query("CREATE TABLE hook_events_dedupe (x INTEGER)").execute(&pool).await.unwrap();
        assert!(migrate(&pool).await.is_err());
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, 0, "子模組沒 migrate 成功，版本戳記不能已經蓋上");
        sqlx::query("DROP TABLE hook_events_dedupe").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #58：SCHEMA／additive ALTER 中途失敗要整批回滾，不能留下「有些表建了、有些沒有」的半套
    /// schema——不然重跑會在同一個位置一直卡住，中途也不該讓任何讀者看到不一致的畫面。
    #[tokio::test]
    async fn a_failed_schema_migration_rolls_back_instead_of_leaving_half_a_schema() {
        let dir = tmp_dir();
        let path = dir.join("db.sqlite3");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        // 故障注入：`runs_pane` 這個名字先被一張普通表占走。索引與表共用同一個命名空間，
        // SCHEMA 跑到 `CREATE INDEX IF NOT EXISTS runs_pane ON runs(pane_id)` 會因為名字已經
        // 是一張表而報錯——這一步落在 `projects`／`bots`／`runs` 都已經在這次呼叫裡新建、
        // `runs_one_active` 也建完之後，剛好測得到「前面明明成功的東西」有沒有跟著回滾。
        sqlx::query("CREATE TABLE runs_pane (x INTEGER)").execute(&pool).await.unwrap();

        let err = migrate(&pool).await.expect_err("撞到命名衝突要失敗，不能靜靜吞掉");
        assert!(err.to_string().contains("runs_pane"), "錯誤要指名是哪句 DDL：{err}");

        for table in ["projects", "bots", "runs"] {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(n, 0, "{table} 在失敗的這次呼叫裡新建，沒有 transaction 的話會留下來；有了就該跟著回滾");
        }
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, 0, "schema 沒套用成功，版本戳記要跟著回滾，不能宣稱已經是這個版本");

        // 修好衝突，重跑：可重入，這次要乾淨地跑完，並且通過完整性檢查。
        sqlx::query("DROP TABLE runs_pane").execute(&pool).await.unwrap();
        migrate(&pool).await.expect("修好之後重跑要成功");
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check").fetch_one(&pool).await.unwrap();
        assert_eq!(integrity, "ok");
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, SCHEMA_VERSION, "這次真的套用成功了，版本戳記要跟著更新");

        // 再跑一次：可重入，結果要一樣（不會因為東西都已經在了就出錯，也不會重複建東西）。
        let cols_before = columns(&pool, "bots").await;
        migrate(&pool).await.expect("再跑一次也要成功（可重入）");
        assert_eq!(columns(&pool, "bots").await, cols_before);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// issue #72：舊 binary 開到被更新版動過的 DB 要拒絕啟動，不能拿舊的欄位假設去讀一個看不懂
    /// 的資料庫——這是目前 `CREATE TABLE IF NOT EXISTS` 完全偵測不到的一種壞情況。
    #[tokio::test]
    async fn a_db_stamped_by_a_newer_binary_refuses_an_older_one() {
        let dir = tmp_dir();
        let path = dir.join("db.sqlite3");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        migrate(&pool).await.unwrap();
        // 假裝這個檔案被一顆懂得更多欄位的未來版 binary 動過。
        let future = SCHEMA_VERSION + 1;
        sqlx::query(&format!("PRAGMA user_version = {future}")).execute(&pool).await.unwrap();

        let err = migrate(&pool).await.expect_err("DB 比這顆 binary 認得的新，要拒絕啟動");
        assert!(err.to_string().contains(&future.to_string()) && err.to_string().contains(&SCHEMA_VERSION.to_string()), "錯誤要講清楚兩個版本號：{err}");

        // 拒絕啟動不能順便把版本號改回來，也不能動任何 schema。
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(v, future);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回滾的真實情形：新版 binary 升了 schema（多一張舊 binary 不認得的表、版本號比較大）之後被換回舊 binary，
    /// 走的是真的 `open`（不是只呼叫 `migrate`）。舊 binary 要拒絕啟動，而且**一個位元組的資料都不能動**：
    /// 不能刪掉不認得的表、不能把版本號蓋回去、不能少任何一列——回滾的人還要靠這份 DB 往前修。
    #[tokio::test]
    async fn a_rolled_back_binary_opening_a_newer_db_refuses_and_changes_nothing() {
        let dir = tmp_dir();
        let path = dir.join("db.sqlite3");
        drop(open(&path).await.unwrap());
        let raw = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
        let future = SCHEMA_VERSION + 2;
        sqlx::query("CREATE TABLE from_the_future (id INTEGER PRIMARY KEY, note TEXT)").execute(&raw).await.unwrap();
        sqlx::query("INSERT INTO from_the_future (note) VALUES ('only the new binary understands this')").execute(&raw).await.unwrap();
        sqlx::query(&format!("PRAGMA user_version = {future}")).execute(&raw).await.unwrap();
        let before: Vec<(String, String)> = sqlx::query_as("SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name").fetch_all(&raw).await.unwrap();
        raw.close().await;

        for attempt in 0..2 {
            let err = open(&path).await.expect_err("舊 binary 開比它新的 DB 要拒絕啟動");
            let msg = format!("{err:#}");
            assert!(msg.contains(&future.to_string()) && msg.contains(&SCHEMA_VERSION.to_string()), "第 {attempt} 次：要講清楚兩個版本號：{msg}");
        }

        let raw = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect(&format!("sqlite://{}", path.display())).await.unwrap();
        let after: Vec<(String, String)> = sqlx::query_as("SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name").fetch_all(&raw).await.unwrap();
        assert_eq!(before, after, "拒絕啟動不能動任何 schema 物件（含不認得的表、trigger）");
        let v: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&raw).await.unwrap();
        assert_eq!(v, future, "版本號要留著，往前修的那顆才認得");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM from_the_future").fetch_one(&raw).await.unwrap();
        assert_eq!(notes, 1, "新版寫的資料一列都不能少");
        raw.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 版本號功能上線前建立的舊資料庫（`user_version` 從沒被設過，SQLite 預設 0）一樣要能升上來，
    /// 而且升級之後可重入：同一版重跑版本號不變、不報錯（issue #72 驗收項）。
    #[tokio::test]
    async fn an_old_unversioned_db_upgrades_and_stays_reentrant() {
        let dir = tmp_dir();
        let path = dir.join("db.sqlite3");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        let before: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(before, 0, "全新檔案／版本號功能上線前的舊 DB，SQLite 預設就是 0");

        migrate(&pool).await.unwrap();
        let after: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(after, SCHEMA_VERSION);

        // 可重入：同一版再跑一次不報錯、版本號不變。
        migrate(&pool).await.expect("同一版重跑不該失敗");
        let again: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await.unwrap();
        assert_eq!(again, SCHEMA_VERSION);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn conversation_id_is_race_safe() {
        let dir = tmp_dir();
        let pool = open(&dir.join("conversation-race.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','bot','claude','tok',?)")
            .bind(now())
            .execute(&pool)
            .await
            .unwrap();

        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let pool = pool.clone();
            calls.spawn(async move { conversation_id(&pool, "b1").await });
        }

        let mut ids = Vec::new();
        while let Some(result) = calls.join_next().await {
            ids.push(result.unwrap().unwrap());
        }
        assert_eq!(ids.len(), 20);
        assert!(ids.iter().all(|id| id == &ids[0]));

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE bot_id = 'b1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// issue #461：同一毫秒的兩個 run，要接回的是**後寫進去的那一個**。
    ///
    /// `started_at` 只到毫秒（`now()`），而一顆 bot 快速重啟（stop 完馬上 start，
    /// `restart?resume=native` 就是這條路）會讓兩個 run 擠進同一毫秒。
    /// 那時 ULID 的亂數段不保證遞增，所以 `id` 的字典序跟寫入順序可能相反——
    /// 這裡故意把**後寫的那一筆給比較小的 id**，把那個情況釘死。
    ///
    /// 挑錯的後果不是少接回一次，是 `--resume` 進另一段對話，之後訊息都落在那段裡。
    #[tokio::test]
    async fn two_runs_in_the_same_millisecond_resume_the_one_written_last() {
        let dir = tmp_dir();
        let pool = open(&dir.join("samems.sqlite3")).await.unwrap();
        let at = now();
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(&at).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','pm','claude','tok',?)")
            .bind(&at).execute(&pool).await.unwrap();

        // 同一個 started_at，而且 id 的字典序跟寫入順序**相反**：
        // 先寫 `r-zzz`（舊的那次 run），後寫 `r-aaa`（真正最後那次）。
        let same = "2026-09-07T05:00:00.123Z";
        for (id, native) in [("r-zzz", "native-earlier"), ("r-aaa", "native-latest")] {
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?, 'b1', 'exited', 'unknown', ?, '/tmp/t.jsonl', ?, ?)",
            )
            .bind(id).bind(native).bind(same).bind(same)
            .execute(&pool).await.unwrap();
        }

        // 修好的寫法（`, rowid DESC`）：拿到後寫的那一個。
        let got = last_native_session(&pool, "b1").await.unwrap().map(|(sid, _)| sid);
        assert_eq!(got.as_deref(), Some("native-latest"), "同毫秒時要接回後寫進去的那一個 run");
        assert_eq!(last_native_session_id(&pool, "b1").await.unwrap().as_deref(), Some("native-latest"));

        // 舊寫法為什麼不行，在同一份資料上直接證明：`id DESC` 會挑到先寫的那一筆
        // （`r-zzz` > `r-aaa`），也就是**另一段對話**。沒有第二鍵的版本更糟——連決定性都沒有。
        let by_id: Option<String> = sqlx::query_scalar(
            "SELECT native_session_id FROM runs WHERE bot_id='b1' AND native_session_id IS NOT NULL
              ORDER BY started_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&pool).await.unwrap();
        assert_eq!(by_id.as_deref(), Some("native-earlier"), "舊寫法（id DESC）挑到的正是錯的那一段");
        assert_ne!(by_id, got, "兩種寫法在這份資料上必須不同，否則這條測試證明不了任何事");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Active runs and ended runs without an id must not steal the continuation slot.
    #[tokio::test]
    async fn last_native_session_id_uses_the_latest_ended_run() {
        let dir = tmp_dir();
        let pool = open(&dir.join("sessions.sqlite3")).await.unwrap();
        let at = now();
        sqlx::query("INSERT INTO projects (id, path, label, created_at) VALUES ('p1','/tmp/p','p',?)")
            .bind(&at)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES ('b1','p1','pm','claude','tok',?)")
            .bind(&at)
            .execute(&pool)
            .await
            .unwrap();
        for (id, state, native, started, ended) in [
            ("r-old", "stopped", Some("native-old"), "2026-09-07T00:00:00Z", Some("2026-09-07T00:01:00Z")),
            ("r-active", "running", Some("native-active"), "2026-09-07T02:00:00Z", None),
            ("r-new", "exited", Some("native-new"), "2026-09-07T03:00:00Z", Some("2026-09-07T03:01:00Z")),
            ("r-no-id", "stopped", None, "2026-09-07T04:00:00Z", Some("2026-09-07T04:01:00Z")),
        ] {
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
                 VALUES (?,?, 'stopped', 'unknown', ?, ?, ?)",
            )
            .bind(id)
            .bind("b1")
            .bind(native)
            .bind(started)
            .bind(ended)
            .execute(&pool)
            .await
            .unwrap();
            if state != "stopped" {
                sqlx::query("UPDATE runs SET state=? WHERE id=?").bind(state).bind(id).execute(&pool).await.unwrap();
            }
        }
        assert_eq!(last_native_session_id(&pool, "b1").await.unwrap().as_deref(), Some("native-new"));
        assert_eq!(last_native_session_id(&pool, "missing").await.unwrap(), None);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// See `quota_claude::should_probe_identity`.
    #[tokio::test]
    async fn live_identities_are_per_host_and_only_count_active_runs() {
        let dir = tmp_dir();
        let pool = open(&dir.join("live.sqlite3")).await.unwrap();
        for (id, host) in [("pl", "local"), ("pm", "m4p")] {
            sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)")
                .bind(id).bind(format!("/tmp/{id}")).bind(id).bind(host).bind(now())
                .execute(&pool).await.unwrap();
        }
        // (bot, project, identity, run state)
        let bots = [
            ("b1", "pm", "cc1", "running"),
            ("b2", "pm", "cc2", "stopped"),
            ("b3", "pl", "cc3", "running"),
            ("b4", "pm", "", "running"),
            ("b5", "pm", "cc4", "starting"),
        ];
        for (b, p, ident, state) in bots {
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, hook_token, identity, created_at) VALUES (?,?,?,'claude','tok',?,?)",
            )
            .bind(b).bind(p).bind(b).bind(ident).bind(now())
            .execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO runs (id, bot_id, state, started_at) VALUES (?,?,?,?)")
                .bind(format!("r{b}")).bind(b).bind(state).bind(now())
                .execute(&pool).await.unwrap();
        }
        let live = live_identities_on_host(&pool, "m4p").await.unwrap();
        assert_eq!(live, ["cc1".to_string(), "cc4".to_string()].into_iter().collect());
        assert_eq!(live_identities_on_host(&pool, "local").await.unwrap(), ["cc3".to_string()].into_iter().collect());

        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = 'b1'").bind(now()).execute(&pool).await.unwrap();
        assert_eq!(live_identities_on_host(&pool, "m4p").await.unwrap(), ["cc4".to_string()].into_iter().collect());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
