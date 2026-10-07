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

/// `sqlite_master` 裡一個由程式建出來的物件（表、索引、trigger、view）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaObject {
    pub kind: String,
    pub name: String,
    pub table: String,
    /// [`normalize_sql`] 過的建立語句。
    pub sql: String,
    /// 只有表有：實際的欄位名。
    pub columns: Vec<String>,
    /// 只有表有：每個欄位的（名字, 型別, NOT NULL, 預設值原文, 主鍵序），給漂移核對比型別與預設值（#307）。
    pub col_defs: Vec<ColDef>,
}

/// `pragma_table_info` 的一列（不含 cid）。
pub type ColDef = (String, String, i64, Option<String>, i64);

/// 讀出這個 DB 的全部 schema 物件，照（種類, 名字）排好。SQLite 自己的（`sqlite_sequence`、`sqlite_stat1`、
/// `sqlite_autoindex_*`——後者 `sql` 是 NULL）不算。
pub async fn read_objects(pool: &SqlitePool) -> Result<Vec<SchemaObject>> {
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

/// 標準答案：全新的 in-memory DB 跑完 `db::apply_migrations_with` 之後的樣子。同一份 feature 清單（以名字串起來當鍵）一個行程只算一次：
/// 正式行程只有 composition 層那一份，所以跟以前「整個行程只算一次」一樣；測試拿別的清單開庫時各有各的標準答案。
pub async fn expected(features: super::FeatureMigrations) -> Result<&'static [SchemaObject]> {
    static EXPECTED: tokio::sync::Mutex<Vec<(String, &'static [SchemaObject])>> = tokio::sync::Mutex::const_new(Vec::new());
    let key = features.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(",");
    let mut cache = EXPECTED.lock().await;
    if let Some((_, objects)) = cache.iter().find(|(k, _)| *k == key) {
        return Ok(objects);
    }
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await?;
    super::apply_migrations_with(&pool, features).await.context("在全新的 in-memory DB 上跑 migrate（schema 的標準答案）")?;
    let objects = read_objects(&pool).await;
    pool.close().await;
    let objects: &'static [SchemaObject] = Box::leak(objects?.into_boxed_slice());
    cache.push((key, objects));
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
pub async fn check_drift_with(pool: &SqlitePool, features: super::FeatureMigrations) -> Result<()> {
    let problems = drift(expected(features).await?, &read_objects(pool).await?);
    anyhow::ensure!(problems.is_empty(), "schema drift：{}", problems.join("\n"));
    Ok(())
}

pub fn drift(want: &[SchemaObject], have: &[SchemaObject]) -> Vec<String> {
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
#[cfg(any(test, feature = "test-hooks"))]
pub fn dump(objects: &[SchemaObject]) -> String {
    objects.iter().map(|o| format!("{} {} ON {}: {}\n", o.kind, o.name, o.table, o.sql)).collect()
}

/// [`dump`] 的 FNV-1a 64（不能用 `DefaultHasher`：它不保證跨 Rust 版本穩定，指紋要釘在原始碼裡）。
/// 排版與 `--` 註解不算；欄位、型別、預設值、約束、索引、trigger 內容，哪一個 migrate 建的都算。
#[cfg(any(test, feature = "test-hooks"))]
pub fn fingerprint(objects: &[SchemaObject]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in dump(objects).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// 現在的指紋要等於 `history` 最後一行（＝`SCHEMA_VERSION` 那一版）釘住的指紋。
#[cfg(any(test, feature = "test-hooks"))]
pub fn check_pinned(fingerprint: &str, history: &[(i64, &str)]) -> Result<()> {
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
pub fn constraint_defs(sql: &str) -> Vec<String> {
    const KEYWORDS: [&str; 4] = ["CHECK", "UNIQUE(", "FOREIGN KEY(", "REFERENCES"];
    let Some(open_paren) = sql.find('(') else { return Vec::new() };
    let body = &sql[open_paren + 1..];
    let mut defs: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut start: usize = 0;
    // 字串字面值裡的括號不算括號（i264 審 #470 提的）。今天的 SCHEMA 沒有這種字面值，但切錯的後果
    // 不對稱：子句對不上 → `drift` 報問題 → `check_drift` 的 `ensure!` → **連全新 DB 都開不起來**，
    // 而且訊息會說「表 X 的約束跟程式不同」，把人往資料庫的方向帶。跳引號的規則同 `normalize_sql`：
    // 連著兩個引號是跳脫，不是結束。
    let mut in_quote: Option<char> = None;
    let mut it = body.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if let Some(q) = in_quote {
            if c == q {
                if it.peek().map(|(_, n)| *n) == Some(q) {
                    it.next();
                } else {
                    in_quote = None;
                }
            }
            continue;
        }
        match c {
            '\'' | '"' => in_quote = Some(c),
            '(' => depth += 1,
            ')' if depth == 0 => {
                defs.push(&body[start..i]);
                break;
            }
            ')' => depth -= 1,
            ',' if depth == 0 => {
                defs.push(&body[start..i]);
                start = i + c.len_utf8();
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

pub fn normalize_sql(sql: &str) -> String {
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
