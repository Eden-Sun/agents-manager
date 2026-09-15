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
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    let existed: Option<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' AND name='bot_reads'")
        .fetch_optional(pool)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS bot_reads (
           bot_id TEXT PRIMARY KEY, read_at TEXT NOT NULL, message_id TEXT NOT NULL DEFAULT ''
         )",
    )
    .execute(pool)
    .await?;
    if existed.is_none() {
        sqlx::query("INSERT OR IGNORE INTO bot_reads (bot_id, read_at, message_id) SELECT id, ?, '' FROM bots")
            .bind(crate::db::now())
            .execute(pool)
            .await?;
    }
    Ok(())
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

/// 只往前推：另一台裝置送來較舊的標記（離線很久才同步）不能把已讀退回未讀。
pub async fn mark(pool: &SqlitePool, bot_id: &str, at: &str, message_id: &str) -> Result<ReadMark> {
    sqlx::query(
        "INSERT INTO bot_reads (bot_id, read_at, message_id) VALUES (?, ?, ?)
         ON CONFLICT(bot_id) DO UPDATE SET read_at = excluded.read_at, message_id = excluded.message_id
          WHERE excluded.read_at > bot_reads.read_at
             OR (excluded.read_at = bot_reads.read_at AND excluded.message_id > bot_reads.message_id)",
    )
    .bind(bot_id)
    .bind(at)
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
}
