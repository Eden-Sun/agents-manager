//! Schema 的全貌與它的兩道守衛（issue #72）。
//!
//! 一顆 DB 的 schema 不只 `db::SCHEMA`：還有 ALTER 名單、`db::sync_trigger` 裝的 trigger，以及 `db::migrate` 的交易
//! commit 之後才跑的各子模組 migrate（`supervisor::store`、`read_marks`、`panes`、`herdr_maintenance`、`mission::store`、
//! `hook_inbox`、`build_scheduler`、`release_triage::ledger`…）各自建的表、索引、trigger。這裡不解析任何一份 DDL 原文，
//! 而是拿**一顆全新的 in-memory DB 跑同一套 migrate**，讀它的 `sqlite_master` 當標準答案——以後多一個子模組、或在
//! 任何地方多建一個物件，都自動涵蓋，不必記得來這裡登記。
//!
//! 1. [`check_drift`]（每次開 DB）：`CREATE … IF NOT EXISTS` 對既有 DB 是 no-op，所以標準答案裡有、既有 DB 卻沒有
//!    （或定義不同）的東西，就是某個 migrate 漏了升級步驟。在這裡講清楚，不要等到某條少走的路徑才炸成
//!    sqlx 的 column-not-found，或是守衛默默停在舊規則。
//!
//!    「定義不同」對索引／trigger／view 是整段 DDL 原文比對；對**表**比的是欄位（名字、型別、NOT NULL、
//!    預設值、主鍵序）**加上約束子句**（`CHECK`／`UNIQUE`／`FOREIGN KEY`／`REFERENCES`，見
//!    [`constraint_defs`]）。表不整段比是刻意的：`ALTER TABLE ADD COLUMN` 會改寫 SQLite 存的建表語句，
//!    升級過的資料庫跟全新的在欄位順序與排版上本來就不會一樣（issue #470）。
//! 2. [`fingerprint`]＋`db::SCHEMA_HISTORY`（測試）：標準答案一變指紋就變，測試逼著 `SCHEMA_VERSION` 跟著升——
//!    只改子模組的索引／trigger／約束也一樣，不靠人記得。

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tokio::sync::OnceCell;

/// `sqlite_master` 裡一個由程式建出來的物件（表、索引、trigger、view）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SchemaObject {
    kind: String,
    name: String,
    table: String,
    /// [`normalize_sql`] 過的建立語句。
    sql: String,
    /// 只有表有：實際的欄位名。
    columns: Vec<String>,
    /// 只有表有：每個欄位的（名字, 型別, NOT NULL, 預設值原文, 主鍵序），給漂移核對比型別與預設值（#307）。
    col_defs: Vec<ColDef>,
}

/// `pragma_table_info` 的一列（不含 cid）。
type ColDef = (String, String, i64, Option<String>, i64);

/// 讀出這個 DB 的全部 schema 物件，照（種類, 名字）排好。SQLite 自己的（`sqlite_sequence`、`sqlite_stat1`、
/// `sqlite_autoindex_*`——後者 `sql` 是 NULL）不算。
pub(super) async fn read_objects(pool: &SqlitePool) -> Result<Vec<SchemaObject>> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT type, name, tbl_name, sql FROM sqlite_master
          WHERE sql IS NOT NULL AND substr(name, 1, 7) != 'sqlite_'
          ORDER BY type, name",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (kind, name, table, sql) in rows {
        let col_defs: Vec<ColDef> = if kind == "table" {
            sqlx::query_as("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?) ORDER BY cid").bind(&name).fetch_all(pool).await?
        } else {
            Vec::new()
        };
        let columns = col_defs.iter().map(|c| c.0.clone()).collect();
        out.push(SchemaObject { kind, name, table, sql: normalize_sql(&sql), columns, col_defs });
    }
    Ok(out)
}

/// 標準答案：全新的 in-memory DB 跑完 `db::apply_migrations` 之後的樣子。一個行程只算一次。
async fn expected() -> Result<&'static [SchemaObject]> {
    static EXPECTED: OnceCell<Vec<SchemaObject>> = OnceCell::const_new();
    let objects = EXPECTED
        .get_or_try_init(|| async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await?;
            super::apply_migrations(&pool).await.context("在全新的 in-memory DB 上跑 migrate（schema 的標準答案）")?;
            let objects = read_objects(&pool).await;
            pool.close().await;
            objects
        })
        .await?;
    Ok(objects)
}

/// migrate 的最後一步：標準答案裡的每個物件，這個 DB 都要有，而且長得一樣。
///
/// - 表：要在，標準答案的欄位一個都不能少，而且每欄的型別、NOT NULL、預設值、主鍵序要相同（#307）。表本身的原文不比——舊 DB 的表是 `ALTER TABLE ADD COLUMN` 一欄一欄
///   補出來的，原文跟全新建的本來就不同；約束（CHECK／UNIQUE）要改就得重建表，那是非 additive 的 migration，
///   `SCHEMA_VERSION` 的說明寫了到時候怎麼辦。
/// - 索引、trigger、view：要在，而且正規化後的定義相同。`CREATE … IF NOT EXISTS` 不會改掉既有的定義，
///   改了定義卻沒寫升級步驟，舊 DB 就一直是舊的。
///
/// 標準答案裡沒有的（已移除功能留下的 `teams`、`team_*`，測試自己裝的故障 trigger）不管。
pub(super) async fn check_drift(pool: &SqlitePool) -> Result<()> {
    let problems = drift(expected().await?, &read_objects(pool).await?);
    anyhow::ensure!(problems.is_empty(), "schema drift：{}", problems.join("\n"));
    Ok(())
}

fn drift(want: &[SchemaObject], have: &[SchemaObject]) -> Vec<String> {
    let mut problems = Vec::new();
    for w in want {
        let Some(h) = have.iter().find(|h| h.kind == w.kind && h.name == w.name) else {
            problems.push(format!("程式會建 {} {}，這個資料庫卻沒有：`{}`", w.kind, w.name, w.sql));
            continue;
        };
        if w.kind == "table" {
            let missing: Vec<String> = w
                .columns
                .iter()
                .filter(|c| !h.columns.iter().any(|x| x.eq_ignore_ascii_case(c)))
                .map(|c| format!("{}.{c}", w.name))
                .collect();
            // 欄位在，但型別／NOT NULL／預設值／主鍵不同（#307）：以前只比欄位名，這種漂移（例如舊 DB 的欄位是 TEXT、
            // 程式以為 INTEGER DEFAULT 0）會一路放行，讀出來的值型別錯或預設值不同。
            for wc in &w.col_defs {
                let Some(hc) = h.col_defs.iter().find(|x| x.0.eq_ignore_ascii_case(&wc.0)) else { continue };
                let (wt, ht) = (wc.1.to_ascii_uppercase(), hc.1.to_ascii_uppercase());
                let (wd, hd) = (wc.3.as_deref().map(str::trim), hc.3.as_deref().map(str::trim));
                if wt != ht || wc.2 != hc.2 || wd != hd || wc.4 != hc.4 {
                    problems.push(format!(
                        "表 {} 的欄位 {} 定義跟程式不同——這個資料庫：型別 `{}`、NOT NULL {}、預設 {:?}、主鍵序 {}；程式：型別 `{}`、NOT NULL {}、預設 {:?}、主鍵序 {}。\
                         改型別／預設值要重建表，是非 additive 的 migration，請在 migrate 明確處理。",
                        w.name, wc.0, hc.1, hc.2, hc.3, hc.4, wc.1, wc.2, wc.3, wc.4
                    ));
                }
            }
            if !missing.is_empty() {
                problems.push(format!(
                    "程式建的表有 {} 但這個資料庫沒有。CREATE TABLE IF NOT EXISTS 對既有 DB 不做事，\
                     請在建 {} 的那個 migrate 補一條 `ALTER TABLE {} ADD COLUMN …`（既有列要能留白）。",
                    missing.join("、"),
                    w.name,
                    w.name
                ));
            }
            // 約束（issue #470）：`pragma_table_info` 看不到 `CHECK`／表層級的 `UNIQUE`／`FOREIGN KEY`，
            // 而表以前從來不比 DDL 原文（只有下面的 `else if` 分支比，表走不到那裡）。於是放寬一個 CHECK
            // 之後：全新 DB 正常、測試全綠（測試都是新 DB），既有 DB 安靜地留著舊約束，只有正式機在寫
            // 新值時炸 `CHECK constraint failed`。
            let (want_c, have_c) = (constraint_defs(&w.sql), constraint_defs(&h.sql));
            if want_c != have_c {
                let only_in = |a: &[String], b: &[String]| {
                    let missing: Vec<&str> = a.iter().filter(|c| !b.contains(c)).map(String::as_str).collect();
                    if missing.is_empty() { "（沒有）".to_string() } else { missing.join("；") }
                };
                problems.push(format!(
                    "表 {} 的約束跟程式不同——這個資料庫少了：{}；多了：{}。`CREATE TABLE IF NOT EXISTS` 不會換掉既有的定義，\
                     而 CHECK 沒辦法用 `ALTER TABLE` 改：要在建 {} 的那個 migrate 裡重建表\
                     （`CREATE …_new` ＋ `INSERT …SELECT` ＋ `DROP` ＋ `RENAME`），並先處理違反新約束的舊資料。",
                    w.name,
                    only_in(&want_c, &have_c),
                    only_in(&have_c, &want_c),
                    w.name
                ));
            }
        } else if h.sql != w.sql {
            problems.push(format!(
                "{} {} 的定義跟程式不同——這個資料庫：`{}`；程式：`{}`。`CREATE … IF NOT EXISTS` 不會換掉既有的定義，\
                 要在 migrate 裡明確換掉（trigger 用 `db::sync_trigger`；索引 DROP 再建，UNIQUE 的要先處理違反它的舊資料）。",
                w.kind, w.name, h.sql, w.sql
            ));
        }
    }
    problems
}

/// 一行一個物件（種類、名字、所屬的表、正規化後的建立語句），排序固定。測試失敗時印出來給人看差在哪。
#[cfg(test)]
pub(super) fn dump(objects: &[SchemaObject]) -> String {
    objects.iter().map(|o| format!("{} {} ON {}: {}\n", o.kind, o.name, o.table, o.sql)).collect()
}

/// [`dump`] 的 FNV-1a 64（不能用 `DefaultHasher`：它不保證跨 Rust 版本穩定，指紋要釘在原始碼裡）。
/// 排版與 `--` 註解不算；欄位、型別、預設值、約束、索引、trigger 內容，哪一個 migrate 建的都算。
#[cfg(test)]
pub(super) fn fingerprint(objects: &[SchemaObject]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in dump(objects).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// 現在的指紋要等於 `history` 最後一行（＝`SCHEMA_VERSION` 那一版）釘住的指紋。
#[cfg(test)]
pub(super) fn check_pinned(fingerprint: &str, history: &[(i64, &str)]) -> Result<()> {
    let Some(&(version, pinned)) = history.last() else { anyhow::bail!("SCHEMA_HISTORY 是空的") };
    for pair in history.windows(2) {
        anyhow::ensure!(pair[1].0 == pair[0].0 + 1, "SCHEMA_HISTORY 的版本號要逐一往上加：{} 後面接了 {}", pair[0].0, pair[1].0);
    }
    anyhow::ensure!(
        fingerprint == pinned,
        "schema 變了，SCHEMA_VERSION 卻還是 {version}（v{version} 釘的指紋是 {pinned}，現在是 {fingerprint}）。\
         在 db.rs 的 SCHEMA_HISTORY 最後加一行 `({next}, \"{fingerprint}\")`；不要改既有那一行——舊 binary 只看版本號\
         判斷認不認得這個資料庫，版本號沒動，它就會照開一個它不懂的 schema（只改索引、trigger、約束也一樣）。",
        next = version + 1
    );
    Ok(())
}

/// `sqlite_master.sql` 存的是當初送進去的原文（只拿掉 `IF NOT EXISTS`）。同一個索引被不同排版的舊版建過
/// （例如從 DDL 字串搬進 Rust 陣列、換行縮排不同）不能被當成定義不同：拿掉 `--` 註解、空白壓成一格、
/// 括號／逗號／比較符號兩側的空白拿掉。引號裡的字元原樣保留。
/// 一張表 DDL 裡帶約束的那幾段（issue #470）：`CHECK`、表層級的 `UNIQUE(…)` 與 `FOREIGN KEY(…)`。
///
/// 比的是**子句的集合**，不是整段 DDL：`ALTER TABLE … ADD COLUMN` 會把新欄位接在 SQLite 存的
/// 建表語句尾巴，跟 `SCHEMA` 裡寫的位置不一定一樣，整段比會在每一顆升級過的資料庫上誤報
/// （`old_layouts_and_leftovers_are_not_drift` 釘著這件事）。沒有約束關鍵字的欄位定義一律丟掉，
/// 所以多一欄、少一欄、換順序都不算漂移——那些本來就由 `columns`／`col_defs` 管。
///
/// 欄位層級的 `REFERENCES` **現在也比**（issue #474）：#470 當時把它排除掉，因為 pre-v2 的
/// `bot_previews` 少了 `REFERENCES bots(id)`，算成漂移會讓那種資料庫從此開不起來。#474 在
/// `db::rebuild_bot_previews_fk` 把它重建補回去之後，那個例外就不需要了——少一個外鍵是真的漂移，
/// 而且現在有 migrate 修得好。
///
/// **仍然不比欄位層級的 `PRIMARY KEY`**：已經由 `col_defs` 的主鍵序管，重複比只是多一條噪音。
///
/// 欄位層級的 `UNIQUE`（`name TEXT UNIQUE`，沒有括號）也不在內：它建出來的是 autoindex，
/// `read_objects` 的 `sql IS NOT NULL` 本來就撈不到，比不了。
///
/// 輸入是 [`normalize_sql`] 過的原文（`--` 註解與多餘空白都已經沒了，`UNIQUE (a, b)` 會變成
/// `UNIQUE(a,b)`，所以帶括號的關鍵字直接比字串就分得出表層級與欄位層級）。
fn constraint_defs(sql: &str) -> Vec<String> {
    const KEYWORDS: [&str; 4] = ["CHECK", "UNIQUE(", "FOREIGN KEY(", "REFERENCES"];
    let Some(open_paren) = sql.find('(') else { return Vec::new() };
    let body = &sql[open_paren + 1..];
    let mut defs: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut start: usize = 0;
    for (i, c) in body.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => {
                defs.push(&body[start..i]);
                break;
            }
            ')' => depth -= 1,
            ',' if depth == 0 => {
                defs.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    let mut out: Vec<String> = defs
        .into_iter()
        .map(str::trim)
        .filter(|d| {
            let upper = d.to_ascii_uppercase();
            KEYWORDS.iter().any(|k| upper.contains(*k))
        })
        .map(str::to_string)
        .collect();
    out.sort();
    out
}

fn normalize_sql(sql: &str) -> String {
    const TIGHT: &[char] = &['(', ')', ',', ';', '=', '<', '>', '!'];
    let mut out = String::with_capacity(sql.len());
    let mut space = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek() == Some(&'-') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            space = true;
            continue;
        }
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() && !out.ends_with(TIGHT) && !TIGHT.contains(&c) {
            out.push(' ');
        }
        space = false;
        out.push(c);
        if c == '\'' || c == '"' {
            // 引號裡原樣照抄；連著兩個引號是跳脫，不是結束。
            while let Some(q) = chars.next() {
                out.push(q);
                if q == c {
                    if chars.peek() == Some(&c) {
                        out.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{apply_migrations, open, ulid, SCHEMA_HISTORY, SCHEMA_VERSION};
    use super::*;

    async fn fresh() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        apply_migrations(&pool).await.unwrap();
        pool
    }

    fn tmp_db() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("am-schema-guard-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.sqlite3");
        (dir, path)
    }

    /// issue #72：`SCHEMA_VERSION` 要跟 migrate 實際建出來的 schema 綁在一起。紅了就照錯誤訊息在
    /// `SCHEMA_HISTORY` 最後加一行，不要改既有那一行。
    #[tokio::test]
    async fn the_schema_migrate_builds_is_pinned_to_schema_version() {
        let objects = expected().await.unwrap();
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
}
