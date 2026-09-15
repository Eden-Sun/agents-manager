//! 每顆 bot 的已讀位置存在 daemon（2026-09-15 使用者：「手機桌機主力區不一樣，少了完成的訊息」）。
//!
//! 以前已讀標記與未讀數只在各瀏覽器的 localStorage，而且只在頁面開著時收到「回合完成」才 +1：手機分頁被
//! 系統凍結的那段時間完成的回合永遠不算，在桌機讀過的手機也不知道。改成一份共用的標記，未讀數由 daemon 從
//! 訊息表算（單位同前端：assistant 訊息依回合去重），`/api/state` 每顆 bot 帶 `unread` 與 `read_mark`。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::lifecycle::LcError;
use crate::state::App;

/// 第一次建表時把既有 bot 的標記設在「現在」：舊資料全部當已讀，不讓升級後每顆 bot 冒出上百則未讀。
///
/// 檢查、建表、補標記與修正舊標記在同一個交易裡：以前建表成功但 seed 失敗時，下次啟動看到表已存在就永遠不補，
/// 升級前的訊息全部變未讀。
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin().await?;
    let existed: Option<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' AND name='bot_reads'")
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS bot_reads (
           bot_id TEXT PRIMARY KEY, read_at TEXT NOT NULL, message_id TEXT NOT NULL DEFAULT ''
         )",
    )
    .execute(&mut *tx)
    .await?;
    let now = crate::db::now();
    if existed.is_none() {
        sqlx::query("INSERT OR IGNORE INTO bot_reads (bot_id, read_at, message_id) SELECT id, ?, '' FROM bots")
            .bind(&now)
            .execute(&mut *tx)
            .await?;
    } else {
        // 修正正規化之前寫進去的標記（帶時區位移、不同精度或未來時間），否則字串比較會一直算錯。
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT bot_id, read_at FROM bot_reads").fetch_all(&mut *tx).await?;
        for (bot_id, at) in rows {
            // 不是 RFC 3339 的值保留原樣：API 從不寫入這種值，猜成 now 會把未讀默默清光，猜成最早又會全部冒成未讀。
            let Some(fixed) = normalize_at(&at, &now) else {
                tracing::warn!(bot_id, read_at = at, "bot_reads.read_at is not RFC 3339; left unchanged");
                continue;
            };
            if fixed != at {
                sqlx::query("UPDATE bot_reads SET read_at = ? WHERE bot_id = ?").bind(&fixed).bind(&bot_id).execute(&mut *tx).await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

/// 讀標時間戳轉成訊息 `created_at` 的格式（UTC、毫秒、`Z`），SQL 才能直接用字串比較。
///
/// `+08:00` 與 `Z`、有無毫秒是同一時刻卻排序不同：`10:00:00+08:00`（＝02Z）會大於 `03:00:00Z`，把較新的訊息算成已讀，
/// 之後真正的 `03:30Z` 也推不動。晚於 `now` 的值夾到 `now`：訊息時間都由 daemon 產生，讀到的不可能在未來，
/// client 時鐘錯送來的未來值若照存，只往前推的標記會永久蓋掉之後所有訊息。不是 RFC 3339 回 `None`。
pub fn normalize_at(at: &str, now: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(at.trim()).ok()?.with_timezone(&chrono::Utc);
    let at = at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Some(if at.as_str() > now { now.to_string() } else { at })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadMark {
    pub at: String,
    pub message_id: String,
}

pub async fn marks(pool: &SqlitePool) -> Result<HashMap<String, ReadMark>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT bot_id, read_at, message_id FROM bot_reads").fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(b, at, message_id)| (b, ReadMark { at, message_id })).collect())
}

/// 未讀回合數：標記之後的 assistant 訊息，依 `turn_id` 去重（沒 turn 的各算一則）。沒有標記＝全部未讀，
/// 同前端（新 bot 本來就沒幾則）。時間戳相同時只有標記那一則算已讀。
pub async fn unread_counts(pool: &SqlitePool) -> Result<HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT c.bot_id, COUNT(DISTINCT COALESCE(m.turn_id, 'msg:' || m.id))
           FROM messages m
           JOIN conversations c ON c.id = m.conversation_id
           LEFT JOIN bot_reads r ON r.bot_id = c.bot_id
          WHERE m.role = 'assistant'
            AND (r.bot_id IS NULL OR m.created_at > r.read_at OR (m.created_at = r.read_at AND m.id <> r.message_id))
          GROUP BY c.bot_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// 只往前推：另一台裝置送來較舊的標記（離線很久才同步）不能把已讀退回未讀。`at` 先經 [`normalize_at`]。
pub async fn mark(pool: &SqlitePool, bot_id: &str, at: &str, message_id: &str) -> Result<ReadMark> {
    let at = normalize_at(at, &crate::db::now()).ok_or_else(|| anyhow::anyhow!("at must be an RFC 3339 timestamp"))?;
    sqlx::query(
        "INSERT INTO bot_reads (bot_id, read_at, message_id) VALUES (?, ?, ?)
         ON CONFLICT(bot_id) DO UPDATE SET read_at = excluded.read_at, message_id = excluded.message_id
          WHERE excluded.read_at > bot_reads.read_at
             OR (excluded.read_at = bot_reads.read_at AND excluded.message_id > bot_reads.message_id)",
    )
    .bind(bot_id)
    .bind(&at)
    .bind(message_id)
    .execute(pool)
    .await?;
    let (at, message_id): (String, String) = sqlx::query_as("SELECT read_at, message_id FROM bot_reads WHERE bot_id = ?")
        .bind(bot_id)
        .fetch_one(pool)
        .await?;
    Ok(ReadMark { at, message_id })
}

#[derive(Deserialize, Default)]
pub struct MarkIn {
    /// 讀到的最後一則訊息的 `created_at`；省略＝現在。
    pub at: Option<String>,
    pub message_id: Option<String>,
}

/// `POST /api/bots/{id}/read`
pub async fn post(State(app): State<Arc<App>>, Path(id): Path<String>, body: Option<Json<MarkIn>>) -> Result<Json<Value>, LcError> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    if crate::db::bot(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?.is_none() {
        return Err(LcError::NotFound("bot".into()));
    }
    let at = b.at.filter(|s| !s.trim().is_empty()).unwrap_or_else(crate::db::now);
    if chrono::DateTime::parse_from_rfc3339(&at).is_err() {
        return Err(LcError::Bad("at must be an RFC 3339 timestamp".into()));
    }
    let m = mark(&app.db, &id, &at, b.message_id.as_deref().unwrap_or("")).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let unread = unread_counts(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?.get(&id).copied().unwrap_or(0);
    let out = json!({"bot_id": id, "read_mark": {"at": m.at, "id": m.message_id}, "unread": unread});
    app.emit("bot_read", out.clone()).await;
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> (SqlitePool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("am-read-marks-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        (crate::db::open(&dir.join("t.sqlite3")).await.unwrap(), dir)
    }

    async fn seed(pool: &SqlitePool) {
        let now = "2026-09-15T00:00:00.000Z";
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)").bind(now).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','t',?)").bind(now).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO conversations (id,bot_id,created_at) VALUES ('c','b',?)").bind(now).execute(pool).await.unwrap();
        for (tid, at) in [("t1", "2026-09-15T01:00:00.000Z"), ("t2", "2026-09-15T02:00:00.000Z"), ("t3", "2026-09-15T03:00:00.000Z")] {
            sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,created_at,completed_at) VALUES (?,'c','web','completed',?,?)")
                .bind(tid).bind(at).bind(at).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at) VALUES (?,'c',?,'user','q','web',?)")
                .bind(format!("u-{tid}")).bind(tid).bind(at).execute(pool).await.unwrap();
            // 同一回合兩則 assistant：算一個回合。
            for k in 0..2 {
                sqlx::query("INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at) VALUES (?,'c',?,'assistant','a','hook',?)")
                    .bind(format!("a{k}-{tid}")).bind(tid).bind(at).execute(pool).await.unwrap();
            }
        }
    }

    /// 裝置 A 讀到 t2，裝置 B（手機，分頁睡著沒收到事件）照樣看到只剩 1 則；舊標記不能倒退；同時間戳只有標記那則算讀過。
    #[tokio::test]
    async fn one_shared_mark_counts_turns_after_it_and_never_moves_back() {
        let (pool, dir) = pool().await;
        seed(&pool).await;
        sqlx::query("DELETE FROM bot_reads").execute(&pool).await.unwrap();
        assert_eq!(unread_counts(&pool).await.unwrap().get("b"), Some(&3), "沒有標記＝全部未讀，依回合去重");
        mark(&pool, "b", "2026-09-15T02:00:00.000Z", "a1-t2").await.unwrap();
        assert_eq!(unread_counts(&pool).await.unwrap().get("b"), Some(&2), "同時間戳的 a0-t2 仍未讀 + t3");
        let m = mark(&pool, "b", "2026-09-15T01:00:00.000Z", "a1-t1").await.unwrap();
        assert_eq!(m.at, "2026-09-15T02:00:00.000Z", "較舊的標記不倒退");
        mark(&pool, "b", "2026-09-15T03:00:01.000Z", "").await.unwrap();
        assert_eq!(unread_counts(&pool).await.unwrap().get("b"), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn creating_the_table_marks_existing_bots_read_but_a_rerun_does_not_reset() {
        let (pool, dir) = pool().await;
        seed(&pool).await;
        sqlx::query("DROP TABLE bot_reads").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        assert_eq!(unread_counts(&pool).await.unwrap().get("b"), None, "升級時舊訊息都算已讀");
        sqlx::query("UPDATE bot_reads SET read_at='2026-09-15T00:30:00.000Z'").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        assert_eq!(unread_counts(&pool).await.unwrap().get("b"), Some(&3), "再跑一次 migrate 不重設標記");
        std::fs::remove_dir_all(dir).ok();
    }

    async fn unread(pool: &SqlitePool) -> Option<i64> {
        unread_counts(pool).await.unwrap().get("b").copied()
    }

    /// `10:00+08:00` 就是 02Z：不能把 03Z 的新回合算成已讀，之後 03:30Z 也要推得動；沒毫秒的 02Z 與 `.000Z` 同一刻。
    #[tokio::test]
    async fn offset_and_precision_variants_of_the_same_instant_compare_equal() {
        let (pool, dir) = pool().await;
        seed(&pool).await;
        let m = mark(&pool, "b", "2026-09-15T10:00:00+08:00", "a1-t2").await.unwrap();
        assert_eq!(m.at, "2026-09-15T02:00:00.000Z");
        assert_eq!(unread(&pool).await, Some(2), "a0-t2 + t3 未讀，t3 不因 +08:00 字串較大被吃掉");
        let m = mark(&pool, "b", "2026-09-15T03:30:00Z", "").await.unwrap();
        assert_eq!(m.at, "2026-09-15T03:30:00.000Z", "後送的較新 UTC 標記要能前推");
        assert_eq!(unread(&pool).await, None);
        let m = mark(&pool, "b", "2026-09-15T11:00:00+08:00", "").await.unwrap();
        assert_eq!(m.at, "2026-09-15T03:30:00.000Z", "較舊（03Z）的離線標記不倒退，即使字串看起來比較大");

        sqlx::query("DELETE FROM bot_reads").execute(&pool).await.unwrap();
        mark(&pool, "b", "2026-09-15T02:00:00Z", "a1-t2").await.unwrap();
        assert_eq!(unread(&pool).await, Some(2), "無毫秒的 02Z 與訊息的 .000Z 同一刻，不能把 a0-t2 算已讀");
        mark(&pool, "b", "2026-09-15T02:00:00.000999Z", "a1-t2").await.unwrap();
        assert_eq!(unread(&pool).await, Some(2), "次毫秒精度截到毫秒，不前推也不改變結果");
        assert!(mark(&pool, "b", "yesterday", "").await.is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    /// client 時鐘錯送來 2099 年：夾到 daemon 現在，之後的新訊息仍算未讀，正常標記仍能前推。
    #[tokio::test]
    async fn a_future_mark_is_clamped_so_later_messages_still_count() {
        let (pool, dir) = pool().await;
        seed(&pool).await;
        let before = crate::db::now();
        let m = mark(&pool, "b", "2099-01-01T00:00:00Z", "").await.unwrap();
        assert!(m.at >= before && m.at <= crate::db::now(), "夾到現在：{}", m.at);
        assert_eq!(unread(&pool).await, None, "已存在的訊息都算讀過");
        let later = (chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at) VALUES ('late','c',NULL,'assistant','a','hook',?)")
            .bind(&later).execute(&pool).await.unwrap();
        assert_eq!(unread(&pool).await, Some(1), "未來標記不能永久蓋掉之後的訊息");

        // 修正前已經寫進 DB 的未來值／時區位移標記，啟動 migrate 時修掉。
        sqlx::query("UPDATE bot_reads SET read_at='2099-01-01T00:00:00+08:00'").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        let at: String = sqlx::query_scalar("SELECT read_at FROM bot_reads WHERE bot_id='b'").fetch_one(&pool).await.unwrap();
        assert!(at <= crate::db::now() && at.ends_with('Z'), "migrate 把污染的標記夾回現在：{at}");
        assert_eq!(unread(&pool).await, Some(1));
        sqlx::query("UPDATE bot_reads SET read_at='2026-09-15T10:00:00+08:00'").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        assert_eq!(unread(&pool).await, Some(3), "舊的 +08:00 標記正規化成 02Z、id 空：同刻的 t2、t3 與 late 未讀");

        sqlx::query("UPDATE bot_reads SET read_at='not a time'").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        let at: String = sqlx::query_scalar("SELECT read_at FROM bot_reads WHERE bot_id='b'").fetch_one(&pool).await.unwrap();
        assert_eq!(at, "not a time", "無法解析的舊值不猜，保留原樣");
        std::fs::remove_dir_all(dir).ok();
    }

    /// 建表後 seed 失敗要整筆回滾：表不能留下來，否則下次啟動當作「已存在」永遠不補，升級前的訊息全變未讀。
    #[tokio::test]
    async fn a_failed_seed_rolls_back_the_table_so_the_next_start_seeds_again() {
        let (pool, dir) = pool().await;
        seed(&pool).await;
        sqlx::query("DROP TABLE bot_reads").execute(&pool).await.unwrap();
        // 故障注入：seed 讀的 bots 暫時不存在，建表那步已成功後才失敗。
        sqlx::query("ALTER TABLE bots RENAME TO bots_moved").execute(&pool).await.unwrap();
        assert!(migrate(&pool).await.is_err());
        let table: Option<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' AND name='bot_reads'")
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert_eq!(table, None, "seed 失敗時建表一起回滾");
        sqlx::query("ALTER TABLE bots_moved RENAME TO bots").execute(&pool).await.unwrap();
        migrate(&pool).await.unwrap();
        assert_eq!(unread(&pool).await, None, "下次啟動重新建表並補標記，舊訊息算已讀");
        std::fs::remove_dir_all(dir).ok();
    }
}
