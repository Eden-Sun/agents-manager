
    use crate::db::{ulid, SCHEMA_HISTORY, SCHEMA_VERSION};
    use crate::runners::am_base_tests::{apply_migrations, composition_features, open_test_db as open};
    use super::*;

    async fn fresh() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        apply_migrations(&pool).await.unwrap();
        pool
    }

    fn tmp_db() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-schema-guard-{}", ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite3");
        (dir, path)
    }

    /// issue #72：`SCHEMA_VERSION` 要跟 migrate 實際建出來的 schema 綁在一起。紅了就照錯誤訊息在
    /// `SCHEMA_HISTORY` 最後加一行，不要改既有那一行。
    #[tokio::test]
    async fn the_schema_migrate_builds_is_pinned_to_schema_version() {
        let objects = expected(composition_features()).await.unwrap();
        let fp = fingerprint(objects);
        if let Err(e) = check_pinned(&fp, SCHEMA_HISTORY) {
            panic!("{e}\n\n現在的 schema（全新 DB）：\n{}", dump(objects));
        }
        assert_eq!(SCHEMA_VERSION, SCHEMA_HISTORY.last().unwrap().0);
        // 標準答案涵蓋的不只 `db::SCHEMA`：交易 commit 之後才跑的子模組，與它們的索引、trigger 都要在裡面。
        for (kind, name) in [
            ("table", "supervisor_assignments"),
            ("trigger", "supervisor_assignments_status_transition"),
            ("trigger", "turns_status_transition"),
            ("index", "hook_events_dedupe"),
            ("index", "missions_one_open_child"),
            ("table", "panes"),
            ("table", "build_slots"),
            ("table", "release_triage"),
        ] {
            assert!(objects.iter().any(|o| o.kind == kind && o.name == name), "標準答案少了 {kind} {name}");
        }
    }

    /// 審查留言的驗收項：只改子模組的 schema（表、欄位、索引、trigger、約束）、忘了升 `SCHEMA_VERSION`，
    /// 測試要紅；照訊息升版之後就過。只改排版、註解不算 schema 變更，不能逼人白升一版。
    #[tokio::test]
    async fn a_submodule_schema_change_without_a_version_bump_is_caught() {
        let base = fingerprint(&read_objects(&fresh().await).await.unwrap());
        for (what, ddl) in [
            ("子模組多一個索引", vec!["CREATE INDEX panes_by_owner ON panes(owner_bot_id)"]),
            ("子模組多一欄", vec!["ALTER TABLE build_slots ADD COLUMN note TEXT"]),
            ("子模組多一張表", vec!["CREATE TABLE herdr_maintenance_log (id INTEGER PRIMARY KEY)"]),
            (
                "子模組的索引換了定義",
                vec![
                    "DROP INDEX hook_events_pending",
                    "CREATE INDEX hook_events_pending ON hook_events(processed_at)",
                ],
            ),
            (
                "子模組的守衛 trigger 換了內容",
                vec![
                    "DROP TRIGGER supervisor_assignments_status_transition",
                    "CREATE TRIGGER supervisor_assignments_status_transition BEFORE UPDATE OF status ON supervisor_assignments
                       WHEN OLD.status IS NOT NEW.status BEGIN SELECT RAISE(ABORT, 'x'); END",
                ],
            ),
            (
                "子模組的約束變了",
                vec![
                    "DROP TABLE herdr_maintenance",
                    "CREATE TABLE herdr_maintenance (id INTEGER PRIMARY KEY CHECK (id IN (1, 2)), opened_at TEXT NOT NULL,
                       until TEXT NOT NULL, opened_by TEXT NOT NULL, reason TEXT)",
                ],
            ),
        ] {
            let pool = fresh().await;
            for s in &ddl {
                sqlx::query(s).execute(&pool).await.unwrap_or_else(|e| panic!("{what}：{s}：{e}"));
            }
            let fp = fingerprint(&read_objects(&pool).await.unwrap());
            assert_ne!(fp, base, "{what}：指紋要變");
            let err = check_pinned(&fp, SCHEMA_HISTORY).expect_err(what).to_string();
            let next = SCHEMA_VERSION + 1;
            assert!(err.contains("SCHEMA_HISTORY") && err.contains(&format!("({next}, \"{fp}\")")), "{what}：要講清楚加哪一行：{err}");
            let bumped: Vec<(i64, &str)> = SCHEMA_HISTORY.iter().copied().chain([(next, fp.as_str())]).collect();
            check_pinned(&fp, &bumped).unwrap_or_else(|e| panic!("{what}：升版之後要過：{e}"));
            pool.close().await;
        }

        // 只改排版與註解：同一個索引照舊版的排版重建，指紋不變。
        let pool = fresh().await;
        sqlx::query("DROP INDEX missions_one_open_child").execute(&pool).await.unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX missions_one_open_child
               -- 舊版寫在 DDL 字串裡的排版
               ON missions ( parent_mission_id )
               WHERE parent_mission_id IS NOT NULL   AND completed_at IS NULL AND cancelled_at IS NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(fingerprint(&read_objects(&pool).await.unwrap()), base, "排版不是 schema 變更");
        pool.close().await;
    }

    #[test]
    fn a_rewritten_history_is_refused() {
        assert!(check_pinned("a", &[]).is_err());
        let err = check_pinned("b", &[(5, "a"), (7, "b")]).unwrap_err().to_string();
        assert!(err.contains("逐一"), "{err}");
        check_pinned("b", &[(5, "a"), (6, "b")]).unwrap();
    }

    #[test]
    fn normalizing_keeps_quoted_text_and_drops_layout() {
        assert_eq!(
            normalize_sql("CREATE INDEX x\n  ON t ( a , b ) -- note 'unbalanced\n  WHERE s  =  'a  b' AND n != 'it''s'"),
            "CREATE INDEX x ON t(a,b)WHERE s='a  b' AND n!='it''s'"
        );
        assert_ne!(normalize_sql("CHECK (s IN ('a','b'))"), normalize_sql("CHECK (s IN ('a','c'))"));
    }

    /// 審查留言的缺口：子模組的表（交易 commit 之後才建）加了欄位卻沒補 ALTER，以前的檢查只看
    /// `db::SCHEMA` 宣告的欄位，抓不到，到使用者那裡才炸。
    #[tokio::test]
    async fn a_submodule_column_the_migrate_forgot_is_caught_on_an_existing_db() {
        let (dir, path) = tmp_db();
        {
            let pool = open(&path).await.unwrap();
            // 做成「`build_scheduler::migrate` 的 CREATE TABLE 多了 purpose，卻沒有補 ALTER」的舊 DB 形狀。
            sqlx::query("ALTER TABLE build_slots DROP COLUMN purpose").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let err = open(&path).await.expect_err("少欄位的舊 DB 不該靜靜開起來").to_string();
        assert!(err.contains("schema drift") && err.contains("build_slots.purpose"), "{err}");
        assert!(err.contains("ALTER TABLE build_slots ADD COLUMN"), "要告訴人怎麼修：{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 欄位名都在，但型別或預設值跟程式不同：以前只比欄位名，這種漂移一路放行。
    #[tokio::test]
    async fn a_column_whose_type_or_default_drifted_is_caught_on_an_existing_db() {
        let (dir, path) = tmp_db();
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("ALTER TABLE build_slots DROP COLUMN purpose").execute(&pool).await.unwrap();
            sqlx::query("ALTER TABLE build_slots ADD COLUMN purpose BLOB DEFAULT 'zzz'").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let err = open(&path).await.expect_err("型別／預設值漂移不該靜靜開起來").to_string();
        assert!(err.contains("schema drift") && err.contains("build_slots") && err.contains("purpose"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 子模組用 `CREATE INDEX IF NOT EXISTS` 改了索引定義：舊 DB 裡那一份不會被換掉。
    #[tokio::test]
    async fn an_index_whose_definition_changed_is_caught_on_an_existing_db() {
        let (dir, path) = tmp_db();
        {
            let pool = open(&path).await.unwrap();
            sqlx::query("DROP INDEX hook_events_pending").execute(&pool).await.unwrap();
            sqlx::query("CREATE INDEX hook_events_pending ON hook_events(processed_at)").execute(&pool).await.unwrap();
            pool.close().await;
        }
        let err = open(&path).await.expect_err("索引定義不同要講出來").to_string();
        assert!(err.contains("schema drift") && err.contains("index hook_events_pending"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// trigger 內容不同也算漂移。正式的守衛都走 `db::sync_trigger`（開 DB 就換回來），所以直接對帳，不經過 migrate。
    #[tokio::test]
    async fn a_trigger_whose_body_changed_is_drift() {
        let pool = fresh().await;
        check_drift(&pool).await.expect("全新 DB 自己跟自己比，沒有漂移");
        sqlx::query("DROP TRIGGER runs_agent_status_since").execute(&pool).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER runs_agent_status_since AFTER UPDATE OF agent_status ON runs
             BEGIN UPDATE runs SET agent_status_since = 'x' WHERE id = NEW.id; END",
        )
        .execute(&pool)
        .await
        .unwrap();
        let err = check_drift(&pool).await.expect_err("trigger 內容不同").to_string();
        assert!(err.contains("trigger runs_agent_status_since"), "{err}");
        pool.close().await;
    }

    /// issue #470：表的 `CHECK` 只存在 `sqlite_master.sql`，`pragma_table_info` 看不到，而表以前
    /// 從來不比 DDL 原文。於是「程式放寬了一個 CHECK」之後，既有資料庫會安靜地留著舊約束——
    /// 全新 DB 正常、測試全綠（測試都是新 DB），只有正式機在寫新值時炸 `CHECK constraint failed`。
    ///
    /// `herdr_maintenance` 是獨立的表（沒有任何 FK 指向它），所以測試裡重建得起來。
    #[tokio::test]
    async fn a_tables_check_constraint_that_drifted_is_caught_on_an_existing_db() {
        // 1) 舊 DB 的 CHECK 跟程式不一樣（程式是 `CHECK (id = 1)`）。
        let (dir, path) = tmp_db();
        {
            let pool = open(&path).await.unwrap();
            for s in [
                "DROP TABLE herdr_maintenance",
                "CREATE TABLE herdr_maintenance (id INTEGER PRIMARY KEY CHECK (id IN (1, 2)), opened_at TEXT NOT NULL,
                   until TEXT NOT NULL, opened_by TEXT NOT NULL, reason TEXT)",
            ] {
                sqlx::query(s).execute(&pool).await.unwrap();
            }
            pool.close().await;
        }
        let err = open(&path).await.expect_err("CHECK 不同不該靜靜開起來").to_string();
        assert!(err.contains("schema drift"), "{err}");
        assert!(err.contains("herdr_maintenance"), "訊息要指名是哪張表：{err}");
        // `normalize_sql` 會把空白收緊：`CHECK (id = 1)` → `CHECK(id=1)`。
        assert!(err.contains("CHECK(id=1)"), "訊息要指名少了哪個子句：{err}");
        assert!(err.contains("CHECK(id IN(1,2))"), "訊息要指名多了哪個子句：{err}");
        std::fs::remove_dir_all(&dir).unwrap();

        // 2) 舊 DB 根本沒有那個 CHECK（＝程式後來才加上／放寬的那個方向）。
        let (dir, path) = tmp_db();
        {
            let pool = open(&path).await.unwrap();
            for s in [
                "DROP TABLE herdr_maintenance",
                "CREATE TABLE herdr_maintenance (id INTEGER PRIMARY KEY, opened_at TEXT NOT NULL,
                   until TEXT NOT NULL, opened_by TEXT NOT NULL, reason TEXT)",
            ] {
                sqlx::query(s).execute(&pool).await.unwrap();
            }
            pool.close().await;
        }
        let err = open(&path).await.expect_err("少一個 CHECK 也是漂移").to_string();
        assert!(err.contains("herdr_maintenance") && err.contains("CHECK(id=1)"), "{err}");
        // 修法要講重建表，不要叫人去 ALTER（CHECK 改不了）。
        assert!(err.contains("重建表"), "訊息要講清楚修法：{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 只挑帶約束的子句比，所以多一欄、少一欄、換順序都不是漂移——那些本來就由 `columns`／`col_defs` 管。
    #[test]
    fn only_constraint_clauses_count_as_a_tables_definition() {
        let c = |sql: &str| constraint_defs(&normalize_sql(sql));
        assert_eq!(c("CREATE TABLE t (a TEXT, b INTEGER)"), Vec::<String>::new(), "沒有約束就沒有子句");
        // `ALTER TABLE ADD COLUMN` 會把新欄位接在 SQLite 存的建表語句尾巴：不能因此誤報。
        assert_eq!(
            c("CREATE TABLE t (a TEXT CHECK (a IN ('x')), b INTEGER)"),
            c("CREATE TABLE t (a TEXT CHECK (a IN ('x')), b INTEGER, c TEXT)"),
            "多一個沒有約束的欄位不算漂移"
        );
        // 順序不算：比的是集合。
        assert_eq!(
            c("CREATE TABLE t (a TEXT CHECK (a IN ('x')), b TEXT CHECK (b > 0))"),
            c("CREATE TABLE t (b TEXT CHECK (b > 0), a TEXT CHECK (a IN ('x')))"),
            "換順序不算漂移"
        );
        // 欄位層級的 FK 算（issue #474：pre-v2 的 `bot_previews` 少了它，migrate 現在補得回來）。
        assert_ne!(
            c("CREATE TABLE t (a TEXT PRIMARY KEY)"),
            c("CREATE TABLE t (a TEXT PRIMARY KEY REFERENCES u(id))"),
            "少一個外鍵是漂移"
        );
        // 但光是 PRIMARY KEY 不算：那一項由 `col_defs` 的主鍵序管。
        assert_eq!(c("CREATE TABLE t (a TEXT PRIMARY KEY)"), Vec::<String>::new(), "PRIMARY KEY 自己不算子句");
        // 真的改了約束就要看得出來。
        assert_ne!(
            c("CREATE TABLE t (a TEXT CHECK (a IN ('x')))"),
            c("CREATE TABLE t (a TEXT CHECK (a IN ('x','y')))"),
            "放寬 CHECK 要算漂移"
        );
        // CHECK 裡面的逗號不能把子句切斷（括號深度）。
        assert_eq!(c("CREATE TABLE t (a TEXT CHECK (a IN ('x','y')), b TEXT)").len(), 1);
        // 字串字面值裡的**不成對**括號不能算進深度（i264 審 #470）：不跳引號的話，`')'` 那個右括號
        // 會把深度歸零、當成整張表的結尾，第二欄的子句就此消失——而後果是連全新 DB 都開不起來。
        let quoted = c("CREATE TABLE t (a TEXT CHECK (a <> ')'), b TEXT CHECK (b <> '('))");
        assert_eq!(quoted.len(), 2, "字面值裡的括號不算深度：{quoted:?}");
        // `normalize_sql` 會把空白收緊：`a <> ')'` → `a<>')'`。
        assert!(quoted.iter().any(|d| d.contains("a<>")) && quoted.iter().any(|d| d.contains("b<>")), "{quoted:?}");
        // 連著兩個引號是跳脫、不是結束（同 `normalize_sql`）。
        let escaped = c("CREATE TABLE t (a TEXT CHECK (a <> 'it''s )'), b TEXT CHECK (b <> 'x'))");
        assert_eq!(escaped.len(), 2, "跳脫的引號不能提早結束字面值：{escaped:?}");
        // 表層級的 FK 與 UNIQUE 也算。
        let table_level = c("CREATE TABLE t (a TEXT, b TEXT, UNIQUE(a, b), FOREIGN KEY(a) REFERENCES u(id))");
        assert_eq!(table_level.len(), 2, "{table_level:?}");
    }

    /// 正式 DB 開不起來的代價很大：舊版用別的排版建過的索引、已移除功能留下的表、索引、trigger，都不是漂移。
    #[tokio::test]
    async fn old_layouts_and_leftovers_are_not_drift() {
        let (dir, path) = tmp_db();
        {
            let pool = open(&path).await.unwrap();
            for s in [
                "DROP INDEX missions_one_open_child",
                "CREATE UNIQUE INDEX IF NOT EXISTS missions_one_open_child
                   ON missions(parent_mission_id)
                   WHERE parent_mission_id IS NOT NULL AND completed_at IS NULL AND cancelled_at IS NULL",
                "CREATE TABLE teams (id TEXT PRIMARY KEY, name TEXT)",
                "CREATE INDEX teams_name ON teams(name)",
                "CREATE TRIGGER teams_touch AFTER UPDATE ON teams BEGIN SELECT 1; END",
                "ALTER TABLE bots ADD COLUMN team_id TEXT",
            ] {
                sqlx::query(s).execute(&pool).await.unwrap();
            }
            pool.close().await;
        }
        let pool = open(&path).await.expect("排版不同、多出來的舊東西都照常開");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 指紋只取決於我們自己的輸入（`dump` 的文字＋自己寫的 FNV-1a，不是 `DefaultHasher`），跟機器、sqlite 版本、Rust 版本無關：
    /// 對一份固定的物件清單要永遠算出同一個值。（2026-09-20 pin 測試在遠端 builder 上紅，查下來 macOS 與 linux 算出一樣的值，
    /// 紅的原因是 a7dc0b74／4cbacecc 加了表沒升版——守衛沒壞，見 #363。）
    #[test]
    fn the_fingerprint_is_a_pure_function_of_our_own_ddl_text() {
        let objs = vec![
            SchemaObject { kind: "table".into(), name: "t".into(), table: "t".into(), sql: "CREATE TABLE t(a INTEGER)".into(), columns: vec!["a".into()], col_defs: vec![] },
            SchemaObject { kind: "index".into(), name: "i".into(), table: "t".into(), sql: "CREATE INDEX i ON t(a)".into(), columns: vec![], col_defs: vec![] },
        ];
        assert_eq!(fingerprint(&objs), "6fc5e5287677b2e6");
        assert_eq!(fingerprint(&objs), fingerprint(&objs.clone()));
    }
