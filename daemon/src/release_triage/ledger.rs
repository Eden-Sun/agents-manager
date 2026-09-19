//! 分診帳本（issue #204 §1）：每個 `(kind, version)` 一列，記下那一版逐條的原文、分桶、命中的規則、
//! 模型的 verdict 與開出去的 issue——要能回答「2.1.277 那條 X 當初為什麼沒開」，所以不放記憶體、不放 `*.last`。
//!
//! 狀態機：
//! ```text
//! （抓到新版）→ pending ─派給模型→ dispatched ─verdict 進來→ judged ─issue 開完→ published
//!                 ↑                    │ 6 小時沒回：attempts+1，回 pending；第 3 次 → failed
//!                 └────────────────────┘
//! 沒有 kept／unmatched 的版本（含第一次跑記下的基準）直接是 empty。
//! ```
//! `judged` 停著代表 verdict 已存、issue 還沒（或沒全）開完（`publish = false`、gh 失敗、被上限擋下），
//! 重試只重跑 publish，不重派模型。

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use super::{Bucket, Entry};

/// `dispatched` 超過這麼久沒有 verdict 就當 bot 撞限沒回，退回 `pending`（§3）。
pub const DISPATCH_STALE_HOURS: i64 = 6;
/// 退回 `pending` 累計到這個次數就標 `failed`（kick 端看到 failed 要 `ops_alert`）。
pub const MAX_ATTEMPTS: i64 = 3;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS release_triage (
           kind TEXT NOT NULL,
           version TEXT NOT NULL,
           status TEXT NOT NULL CHECK (status IN ('pending','dispatched','judged','published','empty','failed')),
           -- [{id,text,bucket,categories,rules}]：切條當下的快照，規則之後改了也不追溯。
           entries_json TEXT NOT NULL DEFAULT '[]',
           -- 模型交回、daemon 驗過的整份 verdict（逐條結論＋issue 提案），事後能逐條複查。
           verdicts_json TEXT,
           -- [{marker,entry_ids,number,url,created_at,comment}]：開出去（或只留言）的 issue，名字沿用 issue #204。
           issue_numbers_json TEXT NOT NULL DEFAULT '[]',
           dispatched_at TEXT,
           attempts INTEGER NOT NULL DEFAULT 0,
           publish_error TEXT,
           created_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           PRIMARY KEY (kind, version)
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub fn now_ts() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Dispatched,
    Judged,
    Published,
    Empty,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Dispatched => "dispatched",
            Status::Judged => "judged",
            Status::Published => "published",
            Status::Empty => "empty",
            Status::Failed => "failed",
        }
    }
    fn parse(s: &str) -> Result<Status> {
        Ok(match s {
            "pending" => Status::Pending,
            "dispatched" => Status::Dispatched,
            "judged" => Status::Judged,
            "published" => Status::Published,
            "empty" => Status::Empty,
            "failed" => Status::Failed,
            other => return Err(anyhow!("帳本裡有不認得的狀態 `{other}`")),
        })
    }
}

/// 開出去（或只留言在既有 issue 上）的一筆，存在 `issue_numbers_json`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRef {
    /// `<kind>@<version>#<id>[,<id>]`——issue 內文結尾隱藏標記的內容，去重的鍵。
    pub marker: String,
    pub entry_ids: Vec<String>,
    pub number: i64,
    #[serde(default)]
    pub url: String,
    pub created_at: String,
    /// true＝`duplicate_of`：只在既有 issue 留言，沒有開新的（不計入每日上限）。
    #[serde(default)]
    pub comment: bool,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub kind: String,
    pub version: String,
    pub status: Status,
    pub entries: Vec<Entry>,
    pub verdicts: Option<serde_json::Value>,
    pub issues: Vec<IssueRef>,
    pub dispatched_at: Option<String>,
    pub attempts: i64,
    pub publish_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(FromRow)]
struct RawRow {
    kind: String,
    version: String,
    status: String,
    entries_json: String,
    verdicts_json: Option<String>,
    issue_numbers_json: String,
    dispatched_at: Option<String>,
    attempts: i64,
    publish_error: Option<String>,
    created_at: String,
    updated_at: String,
}

impl RawRow {
    fn into_row(self) -> Result<Row> {
        Ok(Row {
            status: Status::parse(&self.status)?,
            entries: serde_json::from_str(&self.entries_json).map_err(|e| anyhow!("{} {} 的 entries_json 壞了：{e}", self.kind, self.version))?,
            verdicts: self.verdicts_json.as_deref().map(serde_json::from_str).transpose()?,
            issues: serde_json::from_str(&self.issue_numbers_json).map_err(|e| anyhow!("{} {} 的 issue_numbers_json 壞了：{e}", self.kind, self.version))?,
            kind: self.kind,
            version: self.version,
            dispatched_at: self.dispatched_at,
            attempts: self.attempts,
            publish_error: self.publish_error,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

const COLS: &str = "kind, version, status, entries_json, verdicts_json, issue_numbers_json, dispatched_at, attempts, publish_error, created_at, updated_at";

pub async fn get(pool: &SqlitePool, kind: &str, version: &str) -> Result<Option<Row>> {
    let raw: Option<RawRow> = sqlx::query_as(&format!("SELECT {COLS} FROM release_triage WHERE kind = ? AND version = ?"))
        .bind(kind)
        .bind(version)
        .fetch_optional(pool)
        .await?;
    raw.map(RawRow::into_row).transpose()
}

/// `kind`／`version` 都是選填的篩選；新版在前。
pub async fn list(pool: &SqlitePool, kind: Option<&str>, version: Option<&str>) -> Result<Vec<Row>> {
    let raws: Vec<RawRow> =
        sqlx::query_as(&format!("SELECT {COLS} FROM release_triage WHERE (?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR version = ?2)"))
            .bind(kind)
            .bind(version)
            .fetch_all(pool)
            .await?;
    let mut rows = raws.into_iter().map(RawRow::into_row).collect::<Result<Vec<_>>>()?;
    rows.sort_by(|a, b| {
        crate::changelog::parse_version(&b.version)
            .cmp(&crate::changelog::parse_version(&a.version))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    Ok(rows)
}

pub async fn rows_with_status(pool: &SqlitePool, kind: &str, status: Status) -> Result<Vec<Row>> {
    Ok(list(pool, Some(kind), None).await?.into_iter().filter(|r| r.status == status).collect())
}

/// 帳本裡已分診（含基準）的最大版本，數值比較（`0.9.0 < 0.10.0`）。
pub async fn max_version(pool: &SqlitePool, kind: &str) -> Result<Option<String>> {
    let versions: Vec<String> = sqlx::query_scalar("SELECT version FROM release_triage WHERE kind = ?").bind(kind).fetch_all(pool).await?;
    Ok(versions.into_iter().filter_map(|v| crate::changelog::parse_version(&v).map(|p| (p, v))).max().map(|(_, v)| v))
}

/// 第一次跑：把磁碟上的版本記成 `empty` 基準。已經有這一列就不動。
pub async fn insert_baseline(pool: &SqlitePool, kind: &str, version: &str) -> Result<()> {
    let now = now_ts();
    sqlx::query("INSERT OR IGNORE INTO release_triage (kind, version, status, created_at, updated_at) VALUES (?, ?, 'empty', ?, ?)")
        .bind(kind)
        .bind(version)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
    Ok(())
}

/// 新版一列。已存在就不動（不重算、不覆蓋 verdict）；沒有 kept／unmatched 的版本直接 `empty`。
pub async fn insert_version(pool: &SqlitePool, kind: &str, version: &str, entries: &[Entry]) -> Result<bool> {
    let status = if entries.iter().any(|e| e.bucket != Bucket::Dropped) { Status::Pending } else { Status::Empty };
    let now = now_ts();
    let r = sqlx::query(
        "INSERT OR IGNORE INTO release_triage (kind, version, status, entries_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(kind)
    .bind(version)
    .bind(status.as_str())
    .bind(serde_json::to_string(entries)?)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// kick 派出交辦後呼叫：`pending` → `dispatched`（CAS，只動真的還是 `pending` 的）。回傳實際轉換的筆數。
pub async fn mark_dispatched(pool: &SqlitePool, kind: &str, versions: &[String]) -> Result<u64> {
    let now = now_ts();
    let mut n = 0;
    for v in versions {
        n += sqlx::query(
            "UPDATE release_triage SET status = 'dispatched', dispatched_at = ?, updated_at = ? WHERE kind = ? AND version = ? AND status = 'pending'",
        )
        .bind(&now)
        .bind(&now)
        .bind(kind)
        .bind(v)
        .execute(pool)
        .await?
        .rows_affected();
    }
    Ok(n)
}

/// `dispatched` 超過 [`DISPATCH_STALE_HOURS`] 沒有 verdict：`attempts+1` 退回 `pending`，累計 [`MAX_ATTEMPTS`] 次標 `failed`。
/// 回傳被動到的版本與新狀態。
pub async fn requeue_stale(pool: &SqlitePool, kind: &str) -> Result<Vec<(String, Status)>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(DISPATCH_STALE_HOURS);
    let mut moved = Vec::new();
    for row in rows_with_status(pool, kind, Status::Dispatched).await? {
        let stale = row
            .dispatched_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map_or(true, |t| t.with_timezone(&chrono::Utc) < cutoff);
        if !stale {
            continue;
        }
        let attempts = row.attempts + 1;
        let next = if attempts >= MAX_ATTEMPTS { Status::Failed } else { Status::Pending };
        let r = sqlx::query(
            "UPDATE release_triage SET status = ?, attempts = ?, dispatched_at = NULL, updated_at = ? WHERE kind = ? AND version = ? AND status = 'dispatched'",
        )
        .bind(next.as_str())
        .bind(attempts)
        .bind(now_ts())
        .bind(kind)
        .bind(&row.version)
        .execute(pool)
        .await?;
        if r.rows_affected() == 1 {
            moved.push((row.version, next));
        }
    }
    Ok(moved)
}

/// verdict 進來：`pending`／`dispatched`／`failed` → `next`（`judged` 或沒有任何提案時的 `empty`），CAS。
/// `failed` 也收：bot 撞限退了三次之後人工補交一份，不必為此重設 attempts。回傳是不是真的寫進去了。
pub async fn save_verdicts(pool: &SqlitePool, kind: &str, version: &str, verdicts: &serde_json::Value, next: Status) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE release_triage SET status = ?, verdicts_json = ?, dispatched_at = NULL, publish_error = NULL, updated_at = ?
         WHERE kind = ? AND version = ? AND status IN ('pending','dispatched','failed')",
    )
    .bind(next.as_str())
    .bind(serde_json::to_string(verdicts)?)
    .bind(now_ts())
    .bind(kind)
    .bind(version)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// publish 之後寫回：目前的 issue 清單、狀態（`judged` 或 `published`）與錯誤／備註。只動 `judged` 的列
/// （重複 publish 不會把 `published` 改回去）。
pub async fn save_publish(pool: &SqlitePool, kind: &str, version: &str, issues: &[IssueRef], status: Status, note: Option<&str>) -> Result<bool> {
    let r = sqlx::query(
        "UPDATE release_triage SET status = ?, issue_numbers_json = ?, publish_error = ?, updated_at = ?
         WHERE kind = ? AND version = ? AND status = 'judged'",
    )
    .bind(status.as_str())
    .bind(serde_json::to_string(issues)?)
    .bind(note)
    .bind(now_ts())
    .bind(kind)
    .bind(version)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// 最近 24 小時內**新開**的 issue 數（不含只留言／找到既有的），全部 kind、全部版本合計。
pub async fn created_in_last_day(pool: &SqlitePool) -> Result<usize> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(24);
    Ok(list(pool, None, None)
        .await?
        .iter()
        .flat_map(|r| r.issues.iter())
        .filter(|i| !i.comment)
        .filter(|i| chrono::DateTime::parse_from_rfc3339(&i.created_at).is_ok_and(|t| t.with_timezone(&chrono::Utc) >= cutoff))
        .count())
}
