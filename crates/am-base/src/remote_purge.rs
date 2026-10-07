//! 遠端已刪 bot 的 `bots/<id>/` 目錄清理（issue #349）：本機那份開機時掃 `data_dir/bots`（`purge_deleted_bot_dirs`），
//! 遠端的目錄在別台機器上、開機掃不到，刪除 handler 的一次性 ssh purge 若在送出之前 daemon 就死了，那個目錄
//! （hook 設定／token、shim、spool）就沒人會再回頭清。
//!
//! 「欠的清理」**從 DB 推得出來**：軟刪的 bot ＋ 它專案的 host（專案軟刪後列還在）＋ 沒有「已清掉」的記號。
//! 所以刪除 commit 與 purge 之間任何時刻死掉都不會忘記；主機連上（含重連）與定期輪詢時掃一次，ssh 失敗就留著下次再試。
//! `remote_bot_dir_purges` 只記結果：清掉了（`purged_at`）、或欠著的失敗次數／原因（給 `due_actions` 看）。
//! 原則跟本機一樣 fail closed：DB 讀不到、run 讀不到或還活著就不刪；host 一律取自專案列，**不明就不動，不退回本機**。
use anyhow::Result;
use sqlx::SqlitePool;
use std::time::Duration;

/// 主機連著時的重試節奏；連上那一刻另外掃一次。
pub const POLL_EVERY: Duration = Duration::from_secs(5 * 60);
/// 一輪最多處理幾顆：舊的軟刪 bot 一次全清會佔住連線很久，剩下的下一輪接著做。
const PER_SWEEP: i64 = 100;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS remote_bot_dir_purges (
           bot_id TEXT PRIMARY KEY,
           host TEXT NOT NULL,
           purged_at TEXT,
           attempts INTEGER NOT NULL DEFAULT 0,
           last_error TEXT,
           next_attempt_at TEXT,
           updated_at TEXT NOT NULL
         )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 記下一次 purge 的結果（`purge_bot_dir` 的遠端分支呼叫，刪除 handler 與掃描共用）。寫不進去只是少一筆記號：
/// 下一輪從 DB 重新推導、再搬一次（冪等）。
pub async fn record(app: &impl crate::capabilities::Db, bot_id: &str, host: &str, ok: bool, error: Option<&str>) {
    let now = crate::db::now();
    let res = if ok {
        sqlx::query(
            "INSERT INTO remote_bot_dir_purges (bot_id, host, purged_at, attempts, last_error, next_attempt_at, updated_at)
             VALUES (?, ?, ?, 0, NULL, NULL, ?)
             ON CONFLICT(bot_id) DO UPDATE SET host = excluded.host, purged_at = excluded.purged_at, last_error = NULL,
               next_attempt_at = NULL, updated_at = excluded.updated_at",
        )
        .bind(bot_id)
        .bind(host)
        .bind(&now)
        .bind(&now)
        .execute(app.db())
        .await
    } else {
        sqlx::query(
            "INSERT INTO remote_bot_dir_purges (bot_id, host, purged_at, attempts, last_error, next_attempt_at, updated_at)
             VALUES (?, ?, NULL, 1, ?, ?, ?)
             ON CONFLICT(bot_id) DO UPDATE SET host = excluded.host, attempts = attempts + 1, last_error = excluded.last_error,
               next_attempt_at = excluded.next_attempt_at, updated_at = excluded.updated_at
             WHERE purged_at IS NULL",
        )
        .bind(bot_id)
        .bind(host)
        .bind(error.unwrap_or("purge failed"))
        .bind(crate::db::iso_in(POLL_EVERY.as_secs() as i64))
        .bind(&now)
        .execute(app.db())
        .await
    };
    if let Err(e) = res {
        tracing::warn!(bot = %bot_id, host, error = %e, "could not record the remote bot dir purge result; it will be re-derived");
    }
}

/// 這台遠端主機欠著清理的 bot（軟刪、專案在這台、沒有已清掉的記號）。讀不到就回錯，呼叫端什麼都不刪。
pub async fn pending(pool: &SqlitePool, host: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT b.id FROM bots b JOIN projects p ON p.id = b.project_id
          WHERE p.host = ? AND b.deleted_at IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM remote_bot_dir_purges r WHERE r.bot_id = b.id AND r.purged_at IS NOT NULL)
          ORDER BY b.id LIMIT ?",
    )
    .bind(host)
    .bind(PER_SWEEP)
    .fetch_all(pool)
    .await?)
}

/// 還原的 bot 忘掉「已清掉」的記號（issue #411）：之後再被刪一次，掃描才會再搬它的目錄。寫不進去只記 log。
pub async fn forget(app: &impl crate::capabilities::Db, bot_id: &str) {
    if let Err(e) = sqlx::query("DELETE FROM remote_bot_dir_purges WHERE bot_id = ?").bind(bot_id).execute(app.db()).await {
        tracing::warn!(bot = %bot_id, error = %e, "could not clear the remote bot dir purge mark of a restored bot");
    }
}
