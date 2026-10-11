//! 遠端 bots-trash 的 App 與實例路徑接線。

use crate::state::App;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

fn remote_root() -> String {
    crate::startup::remote_root_for(crate::startup::instance().as_deref())
}

/// 將遠端 bot 目錄送進回收區。
pub async fn move_in(conn: &crate::hosts::HostConn, bot_id: &str) -> Result<Option<String>> {
    let dir = crate::runners::s6_l::remote_bot_dir(conn, bot_id).await?.dir;
    crate::remote_trash::move_in(conn, bot_id, &dir, &remote_root()).await
}

/// `restore_bot` 用：還原遠端目錄並清掉已清理標記。
pub async fn restore_for(app: &Arc<App>, bot_id: &str) {
    let host = match crate::db::bot_host(&app.db, bot_id).await {
        Ok(h) if h != crate::config::LOCAL_HOST => h,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = %e, "could not read the bot's host; remote bots-trash not restored");
            return;
        }
    };
    // 清理記號（`remote_purge`）只有還原真的成功才忘掉（#1307）：沒成功就留著，之後的補還原與掃描還認得它。
    let Some(conn) = app.hosts.get(&host).await else {
        tracing::warn!(host, bot = %bot_id, "unknown host; remote bot dir not restored from bots-trash");
        return;
    };
    let dir = match crate::runners::s6_l::remote_bot_dir(&conn, bot_id).await {
        Ok(paths) => paths.dir,
        Err(e) => {
            tracing::warn!(host, bot = %bot_id, error = %format!("{e:#}"), "could not resolve remote bot directory for restore");
            return;
        }
    };
    match crate::remote_trash::restore(&conn, bot_id, &dir, &remote_root()).await {
        Ok(Some(from)) => {
            tracing::info!(host, bot = %bot_id, %from, "restored remote bot config dir from bots-trash");
            crate::remote_purge::forget(app, bot_id).await;
        }
        Ok(None) => crate::remote_purge::forget(app, bot_id).await,
        Err(e) => tracing::warn!(host, bot = %bot_id, error = %format!("{e:#}"), "could not restore remote bot config dir from bots-trash"),
    }
}

/// 主機連上時先補還原（#1307）：離線時的還原只留下回收區那份，沒有別的記號可以接著做。這裡直接看 DB：
/// 這台主機上還活著的 bot，它的 `bots/<id>` 不在、回收區有它的備份，就搬回來。
/// 移交出去的專案不補（§6.5h，#1405）：接手的那顆 daemon 刪掉的 bot 目錄，這顆不能搬回來。
/// 每顆在 bot 鎖裡重讀 `deleted_at`（#1405）：名單讀完之後才被刪的，不還原。
/// 先列一次回收區的名字（#1406）：沒有備份的活 bot 不為它 ssh；回收區是空的，這一輪就只有這一趟 ssh。
/// 回傳 `None`＝連清單都拿不到，這一輪不清；`Some(failed)`＝還原失敗的那幾顆（它們的備份這一輪不清）。
async fn restore_pending(app: &Arc<App>, host: &str, conn: &crate::hosts::HostConn) -> Option<HashSet<String>> {
    let ids: Vec<String> = match sqlx::query_scalar::<_, String>(
        "SELECT b.id FROM bots b JOIN projects p ON p.id = b.project_id WHERE p.host = ? AND p.handed_off_to IS NULL AND b.deleted_at IS NULL",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(host, error = %e, "could not list the live bots of the host; pending restores not tried");
            return None;
        }
    };
    let backed: HashSet<String> = match crate::remote_trash::backed_up_ids(conn, &remote_root()).await {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            tracing::warn!(host, error = %format!("{e:#}"), "could not list the remote bots-trash; pending restores not tried");
            return None;
        }
    };
    #[cfg(test)]
    crate::runners::app_ports_p11::race_point::hit("remote_restore_after_list", host).await;
    let mut failed = HashSet::new();
    for id in ids.iter().filter(|id| backed.contains(*id)) {
        // 拿不到鎖就這一輪跳過（別的路握著，不算失敗，下一輪再來）；拿到才在鎖裡重讀 `deleted_at`，
        // 鎖握到 `restore` 與 `forget` 做完（同 `remote_purge::sweep`，#511／#1301）。
        let Ok(_guard) = app.bot_lock(id).await.try_lock_owned() else {
            tracing::debug!(host, bot = %id, "bot lock is busy; its pending restore waits for the next round");
            continue;
        };
        match crate::db::bot(&app.db, id).await {
            Ok(Some(b)) if b.deleted_at.is_none() => {}
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(host, bot = %id, error = %e, "could not re-read whether a bot is still live; not restoring it");
                failed.insert(id.clone());
                continue;
            }
        }
        let dir = match crate::runners::s6_l::remote_bot_dir(conn, id).await {
            Ok(paths) => paths.dir,
            Err(e) => {
                tracing::warn!(host, bot = %id, error = %format!("{e:#}"), "could not resolve remote bot directory for a pending restore");
                failed.insert(id.clone());
                continue;
            }
        };
        match crate::remote_trash::restore(conn, id, &dir, &remote_root()).await {
            Ok(Some(from)) => {
                tracing::info!(host, bot = %id, %from, "restored a pending remote bot config dir from bots-trash");
                crate::remote_purge::forget(app, id).await;
            }
            Ok(None) => crate::remote_purge::forget(app, id).await,
            Err(e) => {
                tracing::warn!(host, bot = %id, error = %format!("{e:#}"), "could not restore a pending remote bot config dir; keeping its bots-trash copy");
                failed.insert(id.clone());
            }
        }
    }
    forget_marks_without_backup(app, host, &backed).await;
    Some(failed)
}

/// 活著、沒有任何回收區備份的 bot，它的「已清掉」記號沒有用了：忘掉（#1406）。一句 SQL 做完，子查詢只挑
/// 這台主機上**還活著、沒移交**（§6.5h）的 bot，所以名單讀完之後才被刪的那顆不會被忘掉記號。寫不進去只記 log。
async fn forget_marks_without_backup(app: &Arc<App>, host: &str, backed: &HashSet<String>) {
    let mut sql = "DELETE FROM remote_bot_dir_purges WHERE bot_id IN (SELECT b.id FROM bots b JOIN projects p ON p.id = b.project_id \
                   WHERE p.host = ? AND p.handed_off_to IS NULL AND b.deleted_at IS NULL)"
        .to_string();
    if !backed.is_empty() {
        sql.push_str(&format!(" AND bot_id NOT IN ({})", vec!["?"; backed.len()].join(", ")));
    }
    let mut q = sqlx::query(&sql).bind(host);
    for id in backed {
        q = q.bind(id.as_str());
    }
    if let Err(e) = q.execute(&app.db).await {
        tracing::warn!(host, error = %e, "could not clear the purge marks of live bots without a bots-trash copy");
    }
}

/// 主機連上時：先補還原欠著的，再清一次過期的。補還原的清單拿不到就這一輪不清，免得過期清掉還沒還原的備份（#1307）。
/// 還原失敗的那幾顆，它們的備份這一輪不算「我們放的」：不過期、不計入總量、不被淘汰；其他的照常清（#1406）。
pub async fn gc_host(app: &Arc<App>, host: &str) {
    let Some(conn) = app.hosts.get(host).await else { return };
    if conn.is_local() {
        return;
    }
    let Some(failed) = restore_pending(app, host, &conn).await else {
        tracing::warn!(host, "pending remote restores could not be listed; bots-trash not cleaned this round");
        return;
    };
    if !failed.is_empty() {
        tracing::warn!(host, failed = failed.len(), "some pending remote restores failed; their bots-trash copies are kept this round");
    }
    let known: Vec<String> = crate::bot_trash::known_bot_ids(&app.db).await.into_iter().filter(|id| !failed.contains(id)).collect();
    match crate::remote_trash::gc(
        &conn,
        &remote_root(),
        Duration::from_secs(crate::bot_trash::KEEP_DAYS * 86_400),
        crate::bot_trash::MAX_BYTES,
        &known,
    )
    .await
    {
        Ok((0, 0)) => {}
        Ok((expired, evicted)) => {
            if evicted > 0 {
                tracing::warn!(host, expired, evicted, "remote bots-trash is over its size cap; removed the oldest entries");
            } else {
                tracing::info!(host, expired, "removed expired remote bots-trash entries");
            }
        }
        Err(e) => tracing::warn!(host, error = %format!("{e:#}"), "could not clean the remote bots-trash"),
    }
}
