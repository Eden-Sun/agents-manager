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
    });
}

/// 測試：這個 `App` 還有幾輪背景補開沒做完。
#[cfg(test)]
pub(crate) fn pending_count(app: &Arc<App>) -> usize {
    pending().lock().unwrap_or_else(|e| e.into_inner()).get(&pending_key(app)).copied().unwrap_or(0)
}

/// 測試：等這個 `App` 的背景補開做完（上限 60 秒）。
#[cfg(test)]
pub(crate) async fn quiesce(app: &Arc<App>) {
    for _ in 0..6000 {
        if pending_count(app) == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("背景補開 60 秒還沒做完");
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
    let payload = json!({
        "bot_id": bot.id, "name": bot.name, "host": host, "lost_run_id": l.run_id,
        "reason": "agent_not_found_during_reconcile", "outcome": outcome, "error": error,
    });
    if let Err(e) = crate::supervisor::store::push_inbox(&app.db, &format!("bot_lost:{}:{}", bot.id, l.run_id), "bot_lost", None, Some(&bot.id), None, &payload).await {
        tracing::warn!(bot = %bot.name, error = ?e, "autostart revive: could not write the bot_lost inbox event");
    }
    Ok(())
}
