use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(test)]
use std::time::Duration;
use serde_json::json;
use crate::autostart_revive::{eligible_lost_bot, take_slot, Lost};
use crate::events::ports::{HostSidePort, TurnCommands};
use crate::state::{App, AutostartHostStatus};

/// 還沒做完的背景補開，依 `App` 分開記（測試用它等補開收尾；平行的測試各有各的 `App`，不能共用一個計數）。
static PENDING: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();

fn pending() -> &'static Mutex<HashMap<usize, usize>> {
    PENDING.get_or_init(Default::default)
}

fn pending_key(app: &Arc<App>) -> usize {
    Arc::as_ptr(app) as usize
}

/// 對帳一輪做完（host 的 pass 鎖已放開）之後呼叫：**丟到背景**，不讓對帳呼叫端等。
pub(crate) fn spawn_revive(app: &Arc<App>, host: &str, lost: Vec<Lost>) {
    if lost.is_empty() {
        return;
    }
    let key = pending_key(app);
    *pending().lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_default() += 1;
    let (app, host) = (app.clone(), host.to_string());
    tokio::spawn(async move {
        revive(&app, &host, lost).await;
        let mut map = pending().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                map.remove(&key);
            }
        }
        drop(map);
        #[cfg(test)]
        revive_done().notify_waiters();
    });
}

/// 測試：這個 `App` 還有幾輪背景補開沒做完。
#[cfg(test)]
pub(crate) fn pending_count(app: &Arc<App>) -> usize {
    pending().lock().unwrap_or_else(|e| e.into_inner()).get(&pending_key(app)).copied().unwrap_or(0)
}

/// 測試：任何一輪背景補開做完就叫醒等的人（`quiesce` 靠它，不靠輪詢計時）。
#[cfg(test)]
fn revive_done() -> &'static tokio::sync::Notify {
    static N: OnceLock<tokio::sync::Notify> = OnceLock::new();
    N.get_or_init(Default::default)
}

/// 測試：等這個 `App` 的背景補開都做完。事件驅動：補開收尾時 `notify_waiters`，這裡醒來重看計數（別的 `App` 的補開收尾也會叫醒，
/// 重看就好）；沒有「等幾秒」的判斷。上限只是補開真的卡死時別讓整個測試行程掛著（正常路徑不會碰到）。
#[cfg(test)]
pub(crate) async fn quiesce(app: &Arc<App>) {
    let wait = async {
        loop {
            let notified = revive_done().notified();
            tokio::pin!(notified);
            // 先登記再看計數：看完計數到開始等之間補開收尾，也不會漏掉叫醒。
            notified.as_mut().enable();
            if pending_count(app) == 0 {
                return;
            }
            notified.await;
        }
    };
    if tokio::time::timeout(Duration::from_secs(600), wait).await.is_err() {
        panic!("背景補開 600 秒還沒做完（卡死）");
    }
}

/// 定時掃描最久往回看多久（run 收成 `pane gone` 之後這段時間內還沒被接回來才補開）。
const PANE_GONE_LOOKBACK_SECS: i64 = 30 * 60;
const PANE_GONE_SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// herdr 非計畫重啟之後，定時掃描（`dead_panes` 每 60 秒、`relay_watch` 每 2 秒）可能搶在對帳前問到剛起來、空的 herdr，
/// 把 run 收成 `pane gone`；對帳那頭看到的已經是 exited，不再算遺失，bot 就一直停著（#914）。
/// 這裡撈「最近 30 分內、最後一個 run 是 `exited/pane gone` 的 autostart 非 child bot」，交給既有的 [`spawn_revive`]
/// （開機那一輪、維護窗口、`take_slot` 退避與 `bot_lost` inbox 都在裡面）。使用者關 pane（`pane exited`）、手動停（`stopped`）不撈。
pub(crate) async fn sweep_lost_by_pane_gone(app: &Arc<App>) {
    let cutoff = crate::db::iso_in(-PANE_GONE_LOOKBACK_SECS);
    let rows: Result<Vec<(String, String, String)>, _> = sqlx::query_as(
        "SELECT r.bot_id, r.id, p.host
           FROM runs r
           JOIN bots b ON b.id = r.bot_id
           JOIN projects p ON p.id = b.project_id
          WHERE r.state = 'exited' AND r.exit_reason = ? AND r.ended_at >= ?
            AND b.autostart = 1 AND b.managed_by != 'child' AND b.deleted_at IS NULL
            AND r.id = (SELECT id FROM runs WHERE bot_id = b.id ORDER BY started_at DESC, rowid DESC LIMIT 1)",
    )
    .bind(crate::autostart_revive::PANE_GONE_REASON)
    .bind(cutoff)
    .fetch_all(&app.db)
    .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = ?e, "autostart revive: could not scan for bots whose pane vanished");
            return;
        }
    };
    let mut by_host: std::collections::BTreeMap<String, Vec<Lost>> = Default::default();
    for (bot_id, run_id, host) in rows {
        by_host.entry(host).or_default().push(Lost { bot_id, run_id });
    }
    for (host, lost) in by_host {
        spawn_revive(app, &host, lost);
    }
}

/// 每 30 秒掃一次 [`sweep_lost_by_pane_gone`]。
pub fn spawn_pane_gone_sweeper(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PANE_GONE_SWEEP_EVERY).await;
            sweep_lost_by_pane_gone(&app).await;
        }
    });
}

/// 對帳一輪做完（host 的 pass 鎖已放開）之後呼叫。
pub(crate) async fn revive(app: &Arc<App>, host: &str, lost: Vec<Lost>) {
    if lost.is_empty() {
        return;
    }
    let boot_pass_done = app.autostart_hosts.lock().unwrap_or_else(|e| e.into_inner()).get(host) == Some(&AutostartHostStatus::Done);
    if !boot_pass_done {
        return;
    }
    match app.herdr_maintenance_active().await {
        Ok(None) => {}
        Ok(Some(_)) => return,
        Err(e) => {
            tracing::warn!(host, error = ?e, "autostart revive: cannot read the herdr maintenance state; leaving the lost bots to the bot_stopped probe");
            return;
        }
    }
    for l in lost {
        if let Err(e) = revive_one(app, host, &l).await {
            tracing::warn!(host, bot = %l.bot_id, error = ?e, "autostart revive: could not decide for this bot; leaving it to the bot_stopped probe");
        }
    }
}

async fn revive_one(app: &Arc<App>, host: &str, l: &Lost) -> anyhow::Result<()> {
    // Check once before the lock to keep the common no-op path cheap, then check again while holding
    // the per-bot lock. A user start/stop in between must cancel this background recovery.
    if eligible_lost_bot(app.as_ref(), host, l).await?.is_none() {
        return Ok(());
    }
    #[cfg(test)]
    crate::race_point::hit("autostart_revive_before_start", &l.bot_id).await;

    let lock = app.bot_lock(&l.bot_id).await;
    let guard = lock.lock_owned().await;
    let Some(bot) = eligible_lost_bot(app.as_ref(), host, l).await? else { return Ok(()) };
    let (outcome, error) = if !take_slot(app.as_ref(), &bot.id).await? {
        drop(guard);
        tracing::warn!(host, bot = %bot.name, "autostart revive: lost again within the backoff window; not restarting, only reporting");
        ("backoff", None)
    } else {
        tracing::info!(host, bot = %bot.name, "autostart revive: the agent was lost (herdr restart or reconnect); starting it again");
        // The detached start owns the bot lock through pane creation; cancelling this caller must
        // not leave an unguarded start that can race a user stop.
        let (start_app, bot_id) = (app.clone(), bot.id.clone());
        match tokio::spawn(async move {
            let _guard = guard;
            start_app.start_bot_locked(&bot_id).await
        })
        .await
        {
            Ok(Ok(_)) => ("restarted", None),
            Ok(Err(e)) => ("failed", Some(format!("{e:?}"))),
            Err(e) => ("failed", Some(e.to_string())),
        }
    };
    let exit_reason: Option<String> = sqlx::query_scalar::<_, Option<String>>("SELECT exit_reason FROM runs WHERE id = ?").bind(&l.run_id).fetch_optional(&app.db).await?.flatten();
    let payload = json!({
        "bot_id": bot.id, "name": bot.name, "host": host, "lost_run_id": l.run_id,
        "reason": crate::autostart_revive::reason_slug(exit_reason.as_deref().unwrap_or(crate::autostart_revive::LOST_REASON)),
        "outcome": outcome, "error": error,
    });
    if let Err(e) = crate::supervisor::store::push_inbox(&app.db, &format!("bot_lost:{}:{}", bot.id, l.run_id), "bot_lost", None, Some(&bot.id), None, &payload).await {
        tracing::warn!(bot = %bot.name, error = ?e, "autostart revive: could not write the bot_lost inbox event");
    }
    Ok(())
}
