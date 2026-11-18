//! 「閒下來自動重啟套用更新」的旗標（每顆 bot 一列，durable：daemon 重啟後保留）。
//!
//! 使用者在更新確認框對「現在都在忙」的 bot 按「閒下來自動重啟」時寫入；bot 下一次閒置時 daemon 照一鍵重啟的同一條
//! （resume 接回、鎖內再判一次閒置與背景工作）重啟它，重啟成功或更新已不需要套用就清掉。可由使用者取消。

use sqlx::SqlitePool;

/// 排上（已排的不改時間）。回實際新排的 bot id。只收存在、沒刪的 bot。
pub async fn schedule(pool: &SqlitePool, bot_ids: &[String]) -> Result<Vec<String>, sqlx::Error> {
    let mut added = Vec::new();
    for id in bot_ids {
        let r = sqlx::query(
            "INSERT OR IGNORE INTO bot_update_restart (bot_id, requested_at)
             SELECT id, ? FROM bots WHERE id = ? AND deleted_at IS NULL",
        )
        .bind(crate::db::now())
        .bind(id)
        .execute(pool)
        .await?;
        if r.rows_affected() > 0 {
            added.push(id.clone());
        }
    }
    Ok(added)
}

/// 取消。`bot_ids` 空＝全部。
pub async fn cancel(pool: &SqlitePool, bot_ids: &[String]) -> Result<(), sqlx::Error> {
    if bot_ids.is_empty() {
        sqlx::query("DELETE FROM bot_update_restart").execute(pool).await?;
    } else {
        for id in bot_ids {
            clear(pool, id).await?;
        }
    }
    Ok(())
}

/// 清掉一顆。回有沒有真的刪到。
pub async fn clear(pool: &SqlitePool, bot_id: &str) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM bot_update_restart WHERE bot_id = ?").bind(bot_id).execute(pool).await?.rows_affected() > 0)
}

pub async fn is_scheduled(pool: &SqlitePool, bot_id: &str) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT 1 FROM bot_update_restart WHERE bot_id = ?").bind(bot_id).fetch_optional(pool).await?.is_some())
}

/// 目前排著的 bot id（沒刪的 bot）。
pub async fn list(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT u.bot_id FROM bot_update_restart u JOIN bots b ON b.id = u.bot_id WHERE b.deleted_at IS NULL ORDER BY u.requested_at, u.bot_id",
    )
    .fetch_all(pool)
    .await
}
