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
/// 補還原（`remote_trash::restore`）失敗的退避上限（#1427）：第一次失敗後等 `POLL_EVERY`，之後每次加倍，最多等這麼久。
pub const RESTORE_BACKOFF_CAP: Duration = Duration::from_secs(6 * 3600);
/// 補還原連續失敗到這個次數就放棄自動重試（#1427）：備份留著、發一則 `ops_alert`，之後不再 ssh，等人工處理。
pub const RESTORE_GIVE_UP_AFTER: i64 = 12;

/// 第 `attempts` 次失敗之後要等多久才再試（#1427）。
fn restore_backoff_for(attempts: i64) -> Duration {
    let shift = (attempts - 1).clamp(0, 16) as u32;
    POLL_EVERY.saturating_mul(1u32 << shift).min(RESTORE_BACKOFF_CAP)
}

/// 專案搬到別台（`projects.host` 改掉）的當下，把**舊主機上**還欠著清理的已刪 bot 的義務寫進來（#1320）。
/// 義務只從 `projects.host` 推，搬了家就找不到舊主機上的目錄。寫入 `projects.host` 的地方很多（投影、移交、repoint…），
/// 用 trigger 蓋住，漏一處就漏清。已經有記號的（欠著或已清）不動：同一顆 bot 只記一次，搬來搬去不重複寫。
const OWE_ON_HOST_MOVE: &str = "CREATE TRIGGER remote_bot_dir_purge_owed_on_host_move AFTER UPDATE OF host ON projects
  WHEN OLD.host IS NOT NEW.host AND OLD.host <> 'local'
BEGIN
  INSERT INTO remote_bot_dir_purges (bot_id, host, purged_at, attempts, last_error, next_attempt_at, updated_at)
  SELECT b.id, OLD.host, NULL, 0, NULL, NULL, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
    FROM bots b
   WHERE b.project_id = NEW.id AND b.deleted_at IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM remote_bot_dir_purges r WHERE r.bot_id = b.id);
END";

/// 專案被軟刪的當下，已刪的 bot 的義務記在它當時的主機上（#1320）。同一顆 bot 只記一次。
const OWE_ON_PROJECT_DELETE: &str = "CREATE TRIGGER remote_bot_dir_purge_owed_on_project_delete AFTER UPDATE OF deleted_at ON projects
  WHEN OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL AND NEW.host <> 'local'
BEGIN
  INSERT INTO remote_bot_dir_purges (bot_id, host, purged_at, attempts, last_error, next_attempt_at, updated_at)
  SELECT b.id, NEW.host, NULL, 0, NULL, NULL, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
    FROM bots b
   WHERE b.project_id = NEW.id AND b.deleted_at IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM remote_bot_dir_purges r WHERE r.bot_id = b.id);
END";

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
    // 補還原失敗的退避（#1427）：每 (主機, bot) 一列，連續失敗次數與下次可試的時間。
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS remote_restore_backoff (
           host TEXT NOT NULL,
           bot_id TEXT NOT NULL,
           attempts INTEGER NOT NULL DEFAULT 0,
           last_error TEXT,
           next_attempt_at TEXT,
           updated_at TEXT NOT NULL,
           PRIMARY KEY (host, bot_id)
         )",
    )
    .execute(pool)
    .await?;
    // trigger 要在表建好之後裝；`sync_trigger` 內容不同就換掉，舊庫也會拿到（#1320）。
    let mut conn = pool.acquire().await?;
    crate::db::sync_trigger(&mut conn, "remote_bot_dir_purge_owed_on_host_move", OWE_ON_HOST_MOVE).await?;
    crate::db::sync_trigger(&mut conn, "remote_bot_dir_purge_owed_on_project_delete", OWE_ON_PROJECT_DELETE).await?;
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
             ON CONFLICT(bot_id) DO UPDATE SET purged_at = excluded.purged_at, last_error = NULL,
               next_attempt_at = NULL, updated_at = excluded.updated_at
             WHERE remote_bot_dir_purges.host = excluded.host",
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
             ON CONFLICT(bot_id) DO UPDATE SET attempts = attempts + 1, last_error = excluded.last_error,
               next_attempt_at = excluded.next_attempt_at, updated_at = excluded.updated_at
             WHERE purged_at IS NULL AND remote_bot_dir_purges.host = excluded.host",
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

/// 這台遠端主機欠著清理的 bot（軟刪、沒有已清掉的記號，且歸屬在這台）。讀不到就回錯，呼叫端什麼都不刪。
/// 移交出去的專案（`handed_off_to`）不列：`purge_bot_dir` 本來就不動它們（#708），列出來只會佔名額（#1407）。收回移交後自然又列出。
/// 歸屬看記號（#1320）：有記號的只歸記在這台的（搬家後舊主機的仍歸舊主機）；還沒有記號的才看專案現在在哪台。
/// 沒試過的先來；試過失敗的依 `next_attempt_at` 排到後面（#1318）。`due_only` 為真時退避中的不列（定期掃描用）；
/// 為假就全部列出（主機剛連上時用，連上就是重試的時機）。
pub async fn pending(pool: &SqlitePool, host: &str, due_only: bool) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT b.id FROM bots b
           JOIN projects p ON p.id = b.project_id
           LEFT JOIN remote_bot_dir_purges r ON r.bot_id = b.id
          WHERE b.deleted_at IS NOT NULL
            AND p.handed_off_to IS NULL
            AND ((r.bot_id IS NULL AND p.host = ?)
                 OR (r.host = ? AND r.purged_at IS NULL AND (? = 0 OR r.next_attempt_at IS NULL OR r.next_attempt_at <= ?)))
          ORDER BY (r.next_attempt_at IS NOT NULL), r.next_attempt_at, b.id
          LIMIT ?",
    )
    .bind(host)
    .bind(host)
    .bind(due_only as i64)
    .bind(crate::db::now())
    .bind(PER_SWEEP)
    .fetch_all(pool)
    .await?)
}

/// 還原的 bot 忘掉「已清掉」的記號（issue #411）：之後再被刪一次，掃描才會再搬它的目錄。寫不進去只記 log。
/// 只忘掉記在 `host` 這台的（#1416）：專案改指別台後在新主機還原、那邊沒有備份，舊主機的待清記號不能跟著清掉（舊目錄還在）。
pub async fn forget(app: &impl crate::capabilities::Db, bot_id: &str, host: &str) {
    if let Err(e) = sqlx::query("DELETE FROM remote_bot_dir_purges WHERE bot_id = ? AND host = ?").bind(bot_id).bind(host).execute(app.db()).await {
        tracing::warn!(bot = %bot_id, error = %e, "could not clear the remote bot dir purge mark of a restored bot");
    }
}

/// 這顆 bot 的補還原退避狀態（#1427）：`Some((累計失敗次數, 現在可以試))`；沒有記號＝`None`（從來沒失敗過或已還原成功）。
pub async fn restore_backoff(pool: &SqlitePool, host: &str, bot_id: &str) -> Result<Option<(i64, bool)>> {
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT attempts, (next_attempt_at IS NULL OR next_attempt_at <= ?) FROM remote_restore_backoff WHERE host = ? AND bot_id = ?",
    )
    .bind(crate::db::now())
    .bind(host)
    .bind(bot_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(attempts, due)| (attempts, due != 0)))
}

/// 記下一次補還原失敗，回傳累計失敗次數（#1427）。下次可試的時間照 [`restore_backoff_for`] 排。
pub async fn record_restore_failure(pool: &SqlitePool, host: &str, bot_id: &str, error: &str) -> Result<i64> {
    let attempts: i64 = sqlx::query_scalar("SELECT attempts FROM remote_restore_backoff WHERE host = ? AND bot_id = ?")
        .bind(host)
        .bind(bot_id)
        .fetch_optional(pool)
        .await?
        .unwrap_or(0)
        + 1;
    let next = crate::db::iso_in(restore_backoff_for(attempts).as_secs() as i64);
    sqlx::query(
        "INSERT INTO remote_restore_backoff (host, bot_id, attempts, last_error, next_attempt_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(host, bot_id) DO UPDATE SET attempts = excluded.attempts, last_error = excluded.last_error,
           next_attempt_at = excluded.next_attempt_at, updated_at = excluded.updated_at",
    )
    .bind(host)
    .bind(bot_id)
    .bind(attempts)
    .bind(error)
    .bind(&next)
    .bind(crate::db::now())
    .execute(pool)
    .await?;
    Ok(attempts)
}

/// 還原成功（或這顆已經不需要還原）：退避記號清掉（#1427）。
pub async fn clear_restore_failure(pool: &SqlitePool, host: &str, bot_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM remote_restore_backoff WHERE host = ? AND bot_id = ?").bind(host).bind(bot_id).execute(pool).await?;
    Ok(())
}
