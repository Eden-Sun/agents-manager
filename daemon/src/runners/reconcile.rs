//! `reconcile` runner：全域與單機對帳、autostart 與開機恢復入口。

use std::collections::{HashMap, HashSet};
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
    crate::runners::autostart_revive::spawn_revive(app, host, lost);
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

/// 這顆 bot 的 autostart 這次沒辦法下結論、要再試。帶著 `since` 之後這顆 bot 現有的 run 數（`.0`）：
/// 失敗的 `start_bot` 會先寫一列 run 再收成 exited，重試時「有人動過它」的判斷要扣掉這些自己留下的，不然會把自己的失敗
/// 當成別人動過、永遠不再試。
#[derive(Debug)]
struct OwedAfter(i64, String);

impl std::fmt::Display for OwedAfter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for OwedAfter {}

async fn runs_since(app: &Arc<App>, bot_id: &str, since: &str) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ? AND started_at >= ?").bind(bot_id).bind(since).fetch_one(&app.db).await
}

/// 失敗要再試的路徑都帶回「現在有幾列 run」：其中包含這次失敗的起動自己寫的那一列。
async fn owed_after(app: &Arc<App>, bot_id: &str, since: &str, fallback: i64, why: String) -> anyhow::Error {
    anyhow::Error::new(OwedAfter(runs_since(app, bot_id, since).await.unwrap_or(fallback), why))
}

/// `owed`：`None`＝第一輪（全部都要看）；`Some`＝還欠著的 bot id → 我們自己失敗的 run 數（見 [`OwedAfter`]）。
async fn autostart_pass(app: &Arc<App>, host: &str, owed: &mut Option<HashMap<String, i64>>, since: &str) -> bool {
    let bots = match db::live_bots(&app.db).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(host, error = ?e, "autostart: cannot list bots; none started this pass, will try again");
            return false;
        }
    };
    let mut still = HashMap::new();
    for bot in bots {
        if bot.autostart != 1 || owed.as_ref().is_some_and(|o| !o.contains_key(&bot.id)) {
            continue;
        }
        let own_runs = owed.as_ref().and_then(|o| o.get(&bot.id)).copied().unwrap_or(0);
        if let Err(e) = autostart_one(app, host, &bot, since, own_runs).await {
            tracing::warn!(host, bot = %bot.name, error = ?e, "autostart: cannot tell whether this bot should start; will look again");
            let own_runs = e.downcast_ref::<OwedAfter>().map_or(own_runs, |o| o.0);
            still.insert(bot.id.clone(), own_runs);
        }
    }
    let done = still.is_empty();
    *owed = Some(still);
    done
}

async fn autostart_one(app: &Arc<App>, host: &str, bot: &db::Bot, since: &str, own_runs: i64) -> Result<()> {
    let bot_host = db::bot_host(&app.db, &bot.id).await?;
    if bot_host != host {
        return Ok(());
    }
    // 這一輪看到的連線世代：start 失敗時用它分辨「主機中途斷了／被換掉」與「bot 本身起不來」。
    let Some(fence) = app.hosts.fence(host).await else {
        return Err(anyhow::anyhow!("host gone"));
    };
    if !app.host_connected(host).await {
        tracing::info!(bot = %bot.name, host, "autostart skipped: host not connected");
        return Err(anyhow::anyhow!("host is not connected"));
    }
    if db::active_run(&app.db, &bot.id).await?.is_some() {
        return Ok(());
    }
    if runs_since(app, &bot.id, since).await? > own_runs {
        return Ok(());
    }
    tracing::info!(bot = %bot.name, host, "autostart");
    let start_app = app.clone();
    let bot_id = bot.id.clone();
    match tokio::spawn(async move { start_app.start_bot(&bot_id).await }).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::error!(bot = %bot.name, error = ?e, "autostart failed");
            if !app.hosts.is_current(&fence).await || !app.host_connected(host).await {
                return Err(owed_after(app, &bot.id, since, own_runs, "host disconnected while starting bot".into()).await);
            }
            // herdr／ssh 暫時不通、或暫時讀不到狀態：不是「這顆 bot 不該起」，記在 still 讓背景重試。
            // Uncommitted（副作用已做、run 會自己收斂）與 Bad／NotFound／Conflict／Forbidden／Unprocessable
            // （不該起、已在起）維持 Ok：再試也不會不一樣。
            if matches!(e, crate::lifecycle::LcError::Upstream(_) | crate::lifecycle::LcError::Unavailable(_)) {
                return Err(owed_after(app, &bot.id, since, own_runs, format!("autostart could not start the bot yet: {e:?}")).await);
            }
        }
        Err(e) => return Err(owed_after(app, &bot.id, since, own_runs, e.to_string()).await),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    /// #888：start 失敗是 herdr 暫時不通（Upstream）、主機仍連線中 → 這一輪記進 owed、claim 不完成、背景會再試；
    /// 再試時自己失敗留下的那列 run 不算「有人動過它」。
    #[tokio::test]
    async fn an_upstream_start_failure_on_a_connected_host_is_owed_and_retried() {
        let env = tt::env().await;
        let mut ids = Vec::new();
        for name in ["autostart-owed-a", "autostart-owed-b"] {
            let b = tt::claude_bot(&env.app, &env.project_id, name).await;
            sqlx::query("UPDATE bots SET autostart = 1 WHERE id = ?").bind(&b.id).execute(&env.app.db).await.unwrap();
            ids.push(b.id);
        }
        env.herdr.fail_next("agent.start", tt::Fault::Refuse);
        let claim = AutostartClaim::begin(&env.app, LOCAL_HOST).expect("fresh autostart claim");
        let mut owed = None;

        assert!(!autostart_pass(&env.app, LOCAL_HOST, &mut owed, &claim.since).await, "有一顆起不來：這一輪不算完成");
        let first = owed.clone().unwrap();
        assert_eq!(first.len(), 1, "只有失敗的那顆欠著：{first:?}");
        let failed = first.keys().next().unwrap().clone();
        let other = ids.iter().find(|i| **i != failed).unwrap();
        assert!(db::active_run(&env.app.db, &failed).await.unwrap().is_none());
        assert!(db::active_run(&env.app.db, other).await.unwrap().is_some(), "另一顆照常起來");
        assert_eq!(
            env.app.autostart_hosts.lock().unwrap().get(LOCAL_HOST),
            Some(&AutostartHostStatus::InProgress),
            "claim 沒完成"
        );

        // 背景重試：只看欠著的那顆，自己失敗的 run 不算別人動過它；herdr 好了就起來。
        assert!(autostart_pass(&env.app, LOCAL_HOST, &mut owed, &claim.since).await);
        assert!(db::active_run(&env.app.db, &failed).await.unwrap().is_some(), "重試之後起來了");
        claim.complete();
    }
}
