//! `reconcile` runner：全域與單機對帳、autostart 與開機恢復入口。

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;

use crate::capabilities::HerdrRoutes;
use crate::config::LOCAL_HOST;
use crate::db;
use crate::events::ports::{BotOpsPort, TurnCommands};
use crate::state::{App, AutostartHostStatus};

const DEFERRED_PASS_DELAY: std::time::Duration =
    if cfg!(test) { std::time::Duration::from_millis(50) } else { std::time::Duration::from_secs(15) };

pub async fn reconcile(app: &Arc<App>) -> Result<()> {
    let mut first_err = None;
    for host in app.hosts.names().await {
        if let Err(e) = reconcile_host(app, &host).await {
            tracing::error!(host = %host, error = ?e, "reconcile failed");
            if host == LOCAL_HOST && first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub async fn reconcile_host(app: &Arc<App>, host: &str) -> Result<()> {
    let lost = {
        let lock = crate::reconcile::host_pass_lock(app.as_ref(), host).await;
        let _pass = lock.lock().await;
        crate::reconcile::reconcile_host_locked(app, host).await?
    };
    crate::autostart_revive::spawn_revive(app, host, lost);
    Ok(())
}

pub fn schedule_deferred_pass(app: &Arc<App>, host: &str) {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    let pending = PENDING.get_or_init(Default::default);
    let key = format!("{}\u{0}{host}", app.data_dir.display());
    if !pending.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone()) {
        return;
    }
    let (app, host) = (app.clone(), host.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(DEFERRED_PASS_DELAY).await;
        pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        if let Err(e) = reconcile_host(&app, &host).await {
            if app.session_for_host(&host).await.is_some() && app.host_connected(&host).await {
                tracing::warn!(host = %host, error = ?e, "deferred reconcile pass failed; trying again");
                schedule_deferred_pass(&app, &host);
            }
        }
    });
}

async fn autostart_pass(app: &Arc<App>, host: &str, owed: &mut Option<HashSet<String>>, since: &str) -> bool {
    let bots = match db::live_bots(&app.db).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(host, error = ?e, "autostart: cannot list bots; none started this pass, will try again");
            return false;
        }
    };
    let mut still = HashSet::new();
    for bot in bots {
        if bot.autostart != 1 || owed.as_ref().is_some_and(|o| !o.contains(&bot.id)) {
            continue;
        }
        if let Err(e) = autostart_one(app, host, &bot, since).await {
            tracing::warn!(host, bot = %bot.name, error = ?e, "autostart: cannot tell whether this bot should start; will look again");
            still.insert(bot.id.clone());
        }
    }
    let done = still.is_empty();
    *owed = Some(still);
    done
}

async fn autostart_one(app: &Arc<App>, host: &str, bot: &db::Bot, since: &str) -> Result<()> {
    let bot_host = db::bot_host(&app.db, &bot.id).await?;
    if bot_host != host {
        return Ok(());
    }
    if !app.host_connected(host).await {
        tracing::info!(bot = %bot.name, host, "autostart skipped: host not connected");
        return Err(anyhow::anyhow!("host is not connected"));
    }
    if db::active_run(&app.db, &bot.id).await?.is_some() {
        return Ok(());
    }
    let touched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ? AND started_at >= ?")
        .bind(&bot.id)
        .bind(since)
        .fetch_one(&app.db)
        .await?;
    if touched > 0 {
        return Ok(());
    }
    tracing::info!(bot = %bot.name, host, "autostart");
    let start_app = app.clone();
    let bot_id = bot.id.clone();
    match tokio::spawn(async move { start_app.start_bot(&bot_id).await }).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::error!(bot = %bot.name, error = ?e, "autostart failed");
            if !app.host_connected(host).await {
                return Err(anyhow::anyhow!("host disconnected while starting bot"));
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub(crate) struct AutostartClaim {
    app: Arc<App>,
    host: String,
    pub(crate) since: String,
    completed: bool,
}

impl AutostartClaim {
    pub(crate) fn begin(app: &Arc<App>, host: &str) -> Option<Self> {
        let mut statuses = app.autostart_hosts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if statuses.contains_key(host) {
            return None;
        }
        let since = app
            .autostart_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(host.to_string())
            .or_insert_with(db::now)
            .clone();
        statuses.insert(host.to_string(), AutostartHostStatus::InProgress);
        Some(Self { app: app.clone(), host: host.to_string(), since, completed: false })
    }

    pub(crate) fn complete(mut self) {
        self.app
            .autostart_hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.host.clone(), AutostartHostStatus::Done);
        self.app.autostart_since.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&self.host);
        self.completed = true;
    }
}

impl Drop for AutostartClaim {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut statuses = self.app.autostart_hosts.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if statuses.get(&self.host) == Some(&AutostartHostStatus::InProgress) {
            statuses.remove(&self.host);
        }
    }
}

pub async fn autostart_after_reconcile(app: &Arc<App>, host: &str, reconciled: bool) -> bool {
    if !reconciled {
        tracing::warn!(host, "autostart skipped: reconcile did not succeed; will retry on the next successful connect");
        return false;
    }
    app.recover_restart_intents(host).await;
    app.recover_delete_intents(host).await;
    app.recover_promote_intents(host).await;
    let Some(claim) = AutostartClaim::begin(app, host) else {
        tracing::info!(host, "autostart already ran for this host in this daemon's lifetime; not restarting stopped bots");
        return false;
    };
    let since = claim.since.clone();
    let mut owed = None;
    if !autostart_pass(app, host, &mut owed, &since).await {
        let (app, host) = (app.clone(), host.to_string());
        tokio::spawn(async move {
            let claim = claim;
            for attempt in 0.. {
                tokio::time::sleep(crate::reconcile::recovery_retry_delay(attempt)).await;
                if autostart_pass(&app, &host, &mut owed, &since).await {
                    tracing::info!(host = %host, "autostart caught up");
                    claim.complete();
                    return;
                }
            }
        });
    } else {
        claim.complete();
    }
    app.resume_after_boot(host).await;
    true
}

pub async fn rearm_progress(app: &Arc<App>) {
    let mut owed = crate::reconcile::Recovery::new();
    if owed.pass(app).await {
        return;
    }
    tracing::warn!("startup recovery could not read everything it needs; retrying in the background until it can");
    let app = app.clone();
    tokio::spawn(async move {
        for attempt in 0.. {
            tokio::time::sleep(crate::reconcile::recovery_retry_delay(attempt)).await;
            if owed.pass(&app).await {
                tracing::info!(retries = attempt + 1, "startup recovery caught up");
                return;
            }
        }
    });
}
