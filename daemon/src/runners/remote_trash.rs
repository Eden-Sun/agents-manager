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
    let dir = crate::lifecycle::remote_bot_dir(conn, bot_id).await?.dir;
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
    crate::remote_purge::forget(app, bot_id).await;
    let Some(conn) = app.hosts.get(&host).await else {
        tracing::warn!(host, bot = %bot_id, "unknown host; remote bot dir not restored from bots-trash");
        return;
    };
    let dir = match crate::lifecycle::remote_bot_dir(&conn, bot_id).await {
        Ok(paths) => paths.dir,
        Err(e) => {
            tracing::warn!(host, bot = %bot_id, error = %format!("{e:#}"), "could not resolve remote bot directory for restore");
            return;
        }
    };
    match crate::remote_trash::restore(&conn, bot_id, &dir, &remote_root()).await {
        Ok(Some(from)) => tracing::info!(host, bot = %bot_id, %from, "restored remote bot config dir from bots-trash"),
        Ok(None) => {}
        Err(e) => tracing::warn!(host, bot = %bot_id, error = %format!("{e:#}"), "could not restore remote bot config dir from bots-trash"),
    }
}

/// 主機連上時清一次過期的。
pub async fn gc_host(app: &Arc<App>, host: &str) {
    let Some(conn) = app.hosts.get(host).await else { return };
    if conn.is_local() {
        return;
    }
    match crate::remote_trash::gc(
        &conn,
        &remote_root(),
        Duration::from_secs(crate::bot_trash::KEEP_DAYS * 86_400),
        crate::bot_trash::MAX_BYTES,
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
