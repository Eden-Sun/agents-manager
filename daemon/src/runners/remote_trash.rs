//! 遠端 bots-trash 的 App 與實例路徑接線。

use crate::state::App;
use anyhow::Result;
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
/// 這台主機上還活著的 bot，它的 `bots/<id>` 不在、回收區有它的備份，就搬回來。全部處理完回 `true`；有一顆失敗就回 `false`。
async fn restore_pending(app: &Arc<App>, host: &str, conn: &crate::hosts::HostConn) -> bool {
    let ids: Vec<String> = match sqlx::query_scalar::<_, String>(
        "SELECT b.id FROM bots b JOIN projects p ON p.id = b.project_id WHERE p.host = ? AND b.deleted_at IS NULL",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(host, error = %e, "could not list the live bots of the host; pending restores not tried");
            return false;
        }
    };
    let mut all_ok = true;
    for id in ids {
        let dir = match crate::runners::s6_l::remote_bot_dir(conn, &id).await {
            Ok(paths) => paths.dir,
            Err(e) => {
                tracing::warn!(host, bot = %id, error = %format!("{e:#}"), "could not resolve remote bot directory for a pending restore");
                all_ok = false;
                continue;
            }
        };
        match crate::remote_trash::restore(conn, &id, &dir, &remote_root()).await {
            Ok(Some(from)) => {
                tracing::info!(host, bot = %id, %from, "restored a pending remote bot config dir from bots-trash");
                crate::remote_purge::forget(app, &id).await;
            }
            Ok(None) => crate::remote_purge::forget(app, &id).await,
            Err(e) => {
                tracing::warn!(host, bot = %id, error = %format!("{e:#}"), "could not restore a pending remote bot config dir; keeping its bots-trash copy");
                all_ok = false;
            }
        }
    }
    all_ok
}

/// 主機連上時：先補還原欠著的，再清一次過期的。補還原沒全部成功就這一輪不清，免得過期清掉還沒還原的備份（#1307）。
pub async fn gc_host(app: &Arc<App>, host: &str) {
    let Some(conn) = app.hosts.get(host).await else { return };
    if conn.is_local() {
        return;
    }
    if !restore_pending(app, host, &conn).await {
        tracing::warn!(host, "pending remote restores did not all succeed; bots-trash not cleaned this round");
        return;
    }
    let known: Vec<String> = crate::bot_trash::known_bot_ids(&app.db).await.into_iter().collect();
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
