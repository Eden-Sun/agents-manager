//! 遠端已刪 bot 目錄清理的排程與執行入口。

use crate::state::App;
use std::sync::Arc;
use crate::remote_purge::POLL_EVERY;

/// 掃一台遠端主機：回 `(清掉, 留著)`。資料查詢與結果記錄留在下層模組。
pub async fn sweep(app: &Arc<App>, host: &str) -> (usize, usize) {
    if host == crate::config::LOCAL_HOST {
        return (0, 0);
    }
    if crate::shared_host::is_shared(app, host).await {
        tracing::debug!(host, "shared-session host: remote bot directories and trash left alone");
        return (0, 0);
    }
    crate::runners::remote_trash::gc_host(app, host).await;
    let ids = match crate::remote_purge::pending(&app.db, host).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(host, error = %e, "could not read which deleted bots still owe a remote directory purge; nothing removed");
            return (0, 0);
        }
    };
    #[cfg(test)]
    crate::runners::app_ports_p11::race_point::hit("remote_sweep_after_pending", host).await;
    let (mut purged, mut kept) = (0, 0);
    for id in ids {
        let _guard = app.bot_lock(&id).await.lock_owned().await;
        match crate::db::bot(&app.db, &id).await {
            Ok(Some(b)) if b.deleted_at.is_none() => {
                tracing::info!(bot = %id, host, "the bot was restored while this sweep was running; leaving its directory alone");
                continue;
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                kept += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(bot = %id, host, error = %e, "could not re-read whether a bot is still deleted; leaving its directory");
                kept += 1;
                continue;
            }
        }
        match crate::db::active_run(&app.db, &id).await {
            Ok(None) => {}
            Ok(Some(_)) => {
                crate::remote_purge::record(app, &id, host, false, Some("run_still_active")).await;
                kept += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(bot = %id, host, error = %e, "could not read whether a deleted remote bot still has a live run; leaving its directory");
                kept += 1;
                continue;
            }
        }
        if crate::lifecycle::purge_bot_dir(app, &id, host).await {
            purged += 1;
        } else {
            kept += 1;
        }
    }
    if purged + kept > 0 {
        tracing::info!(host, purged, kept, "swept remote directories of deleted bots");
    }
    (purged, kept)
}

/// 主機連上（含重連）那一刻背景掃一次。
pub fn spawn_sweep(app: Arc<App>, host: String) {
    // 一次性：掛在 `background_tasks` 下（#924），關機的 `wait()` 等得到它。
    if app.shutdown.is_cancelled() {
        return;
    }
    let tracker = app.background_tasks.clone();
    tracker.spawn(async move {
        sweep(&app, &host).await;
    });
}

/// 連著的遠端主機定期再掃。
pub fn spawn_poller(app: Arc<App>) {
    crate::background_loop::spawn_periodic(&app, "remote purge poller", POLL_EVERY, POLL_EVERY, |app| async move {
        for name in app.hosts.names().await {
            let Some(conn) = app.hosts.get(&name).await else { continue };
            if !conn.is_local() && conn.is_connected() {
                sweep(&app, &name).await;
            }
        }
    });
}
