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
        // 拿不到鎖就這一輪跳過、欠著，下一輪再來（#1301）：這顆正被別的路握著（還原、刪除、啟動），不能在這裡無限等，
        // 否則同一輪掃到的別台主機、以及這台後面的 bot 都跟著停住。
        let Ok(_guard) = app.bot_lock(&id).await.try_lock_owned() else {
            tracing::debug!(host, bot = %id, "bot lock is busy; leaving its remote directory for the next sweep");
            kept += 1;
            continue;
        };
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
        sweep_connected_hosts(&app).await;
    });
}

/// 同一輪掃所有連著的遠端主機（poller 與測試共用）。每台各自一個 task：一台卡住不拖住別台（#1301）。
pub async fn sweep_connected_hosts(app: &Arc<App>) {
    let mut set = tokio::task::JoinSet::new();
    for name in app.hosts.names().await {
        let Some(conn) = app.hosts.get(&name).await else { continue };
        if !conn.is_local() && conn.is_connected() {
            let app = app.clone();
            set.spawn(async move {
                sweep(&app, &name).await;
            });
        }
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            tracing::warn!(error = %e, "remote directory sweep task failed");
        }
    }
}
