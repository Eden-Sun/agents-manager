//! herdr 斷線重連或 server 重啟之後，把「因為 agent 不見而被對帳收成 exited」的 autostart bot 再起一次。
//!
//! 對帳發現 run 的 agent 不在（`agent not found during reconcile`）只有兩種情況：herdr 的 pane／server 在 daemon 沒看到事件時
//! 掉了（herdr 更新、當機、daemon 斷線那段 pane 被關）。使用者手動停（`stopped`）、在 herdr 裡關 pane（`pane exited` 事件）
//! 都不走這條，所以這裡只看這個原因。以前這種 bot 要等 `bot_stopped` 探針（300 秒）才報、而且 autostart 每台主機一生只跑一次，
//! 不會自己回來（2026-10-02 herdr 0.9.3 更新後實際發生）。
//!
//! * 只在開機那一輪 autostart 跑完之後才動（`autostart_hosts` 是 `Done`）：開機那一輪由 autostart 負責，不搶著起第二次。
//! * herdr 計畫中的維護期間不動（`herdr_maintenance`）：那邊自己會把 bot 接回來。
//! * 起之前再驗一次：bot 還在、還是 autostart、沒有 active run、最後一個 run 就是這次被收掉的那個
//!   （使用者在中間做了任何事＝有新 run＝不碰）。
//! * 退避：同一顆 bot 30 分鐘內最多重開 3 次；herdr 一直掛時只通知、不再開（避免無限重開）。
//! * 每次遺失都推一則 supervisor inbox `bot_lost`（巡檢收、叫醒），帶 `outcome`：`restarted`／`failed`／`backoff`，不等探針。
use crate::db;
use crate::state::{App, AutostartHostStatus};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 這一輪對帳收掉的 autostart bot（原因是 agent 不見）。
#[derive(Debug, Clone)]
pub(crate) struct Lost {
    pub bot_id: String,
    pub run_id: String,
}

const MAX_RESTARTS: usize = 3;
const WINDOW: Duration = Duration::from_secs(30 * 60);

fn attempts() -> &'static Mutex<HashMap<String, Vec<Instant>>> {
    static A: OnceLock<Mutex<HashMap<String, Vec<Instant>>>> = OnceLock::new();
    A.get_or_init(Default::default)
}

/// 退避窗內還有名額就記一次並回 `true`。
fn take_slot(bot_id: &str) -> bool {
    let mut map = attempts().lock().unwrap_or_else(|e| e.into_inner());
    let hits = map.entry(bot_id.to_string()).or_default();
    hits.retain(|t| t.elapsed() < WINDOW);
    if hits.len() >= MAX_RESTARTS {
        return false;
    }
    hits.push(Instant::now());
    true
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
    match crate::herdr_maintenance::active(app).await {
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
    let Some(bot) = db::bot(&app.db, &l.bot_id).await? else { return Ok(()) };
    if bot.deleted_at.is_some() || bot.autostart != 1 || bot.managed_by == "child" {
        return Ok(());
    }
    if db::active_run(&app.db, &bot.id).await?.is_some() {
        return Ok(());
    }
    let last: Option<(String, String)> = sqlx::query_as("SELECT id, state FROM runs WHERE bot_id = ? ORDER BY started_at DESC, rowid DESC LIMIT 1")
        .bind(&bot.id)
        .fetch_optional(&app.db)
        .await?;
    if last.as_ref().map(|(id, state)| (id.as_str(), state.as_str())) != Some((l.run_id.as_str(), "exited")) {
        return Ok(());
    }
    let (outcome, error) = if !take_slot(&bot.id) {
        tracing::warn!(host, bot = %bot.name, "autostart revive: lost again within the backoff window; not restarting, only reporting");
        ("backoff", None)
    } else {
        tracing::info!(host, bot = %bot.name, "autostart revive: the agent was lost (herdr restart or reconnect); starting it again");
        // 同 autostart_one：起的過程不跟著對帳 task 被取消。
        let (start_app, bot_id) = (app.clone(), bot.id.clone());
        match tokio::spawn(async move { crate::lifecycle::start_bot(&start_app, &bot_id).await }).await {
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

#[cfg(test)]
mod tests {
    use crate::state::AutostartHostStatus;
    use crate::testing as tt;
    use crate::{config, db};
    use std::sync::Arc;

    async fn autostart_bot(env: &tt::Env, name: &str) -> db::Bot {
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET autostart = 1 WHERE id = ?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        bot
    }

    fn autostart_done(app: &Arc<crate::state::App>) {
        app.autostart_hosts.lock().unwrap().insert(config::LOCAL_HOST.to_string(), AutostartHostStatus::Done);
    }

    /// herdr 的 session 被清空（server 重啟）：herdr 上什麼都沒有，但回的是一份有效的空清單。
    async fn herdr_forgets_everything(env: &tt::Env) {
        env.herdr.agents.lock().unwrap().clear();
        env.herdr.workspaces.lock().unwrap().clear();
        env.herdr.tabs.lock().unwrap().clear();
    }

    async fn runs(app: &Arc<crate::state::App>, bot_id: &str) -> Vec<(String, Option<String>)> {
        sqlx::query_as("SELECT state, exit_reason FROM runs WHERE bot_id = ? ORDER BY started_at, rowid").bind(bot_id).fetch_all(&app.db).await.unwrap()
    }

    async fn bot_lost_events(app: &Arc<crate::state::App>, bot_id: &str) -> Vec<serde_json::Value> {
        crate::supervisor::store::inbox(&app.db, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "bot_lost" && e.bot_id.as_deref() == Some(bot_id))
            .map(|e| serde_json::from_str(&e.payload_json).unwrap())
            .collect()
    }

    /// 今天 herdr 0.9.3 更新之後的情境：autostart 的 bot 因為「agent not found during reconcile」被收成 exited，
    /// 這一輪對帳完就要再起一次，而且立刻通知 supervisor（不等 300 秒的 bot_stopped 探針）。
    #[tokio::test]
    async fn an_autostart_bot_lost_in_a_herdr_restart_is_started_again_and_the_supervisor_is_told() {
        let env = tt::env().await;
        let bot = autostart_bot(&env, "auto").await;
        crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        autostart_done(&env.app);
        herdr_forgets_everything(&env).await;

        crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();

        let runs = runs(&env.app, &bot.id).await;
        assert_eq!(runs.len(), 2, "舊的收掉、新的起來：{runs:?}");
        assert_eq!(runs[0], ("exited".into(), Some("agent not found during reconcile".into())));
        assert_eq!(runs[1].0, "running");
        let events = bot_lost_events(&env.app, &bot.id).await;
        assert_eq!(events.len(), 1, "立刻推一則 inbox：{events:?}");
        assert_eq!(events[0]["outcome"], "restarted");
    }

    /// 不是 autostart 的 bot、手動停掉的 bot、對帳之前就沒在跑的 bot：不能被拉起來。
    #[tokio::test]
    async fn bots_that_were_not_expected_to_run_are_left_down() {
        let env = tt::env().await;
        let plain = tt::claude_bot(&env.app, &env.project_id, "plain").await;
        let stopped = autostart_bot(&env, "stopped").await;
        crate::lifecycle::start_bot(&env.app, &plain.id).await.unwrap();
        crate::lifecycle::start_bot(&env.app, &stopped.id).await.unwrap();
        crate::lifecycle::stop_bot(&env.app, &stopped.id).await.unwrap();
        autostart_done(&env.app);
        herdr_forgets_everything(&env).await;

        crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();

        assert!(db::active_run(&env.app.db, &plain.id).await.unwrap().is_none(), "沒開 autostart：不拉起來");
        assert!(db::active_run(&env.app.db, &stopped.id).await.unwrap().is_none(), "使用者停掉的：不拉起來");
        assert!(bot_lost_events(&env.app, &stopped.id).await.is_empty());
        assert_eq!(runs(&env.app, &stopped.id).await.len(), 1);
    }

    /// 開機那一輪 autostart 還沒跑完前，由 autostart 負責，不在對帳裡搶著起（同一顆 bot 不能被起兩次）。
    #[tokio::test]
    async fn before_the_boot_autostart_pass_the_reconcile_leaves_starting_to_it() {
        let env = tt::env().await;
        let bot = autostart_bot(&env, "boot").await;
        crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        herdr_forgets_everything(&env).await;

        crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();

        assert!(db::active_run(&env.app.db, &bot.id).await.unwrap().is_none());
        assert!(bot_lost_events(&env.app, &bot.id).await.is_empty());
    }

    /// herdr 一直在掛（每次起來又掉）：同一顆 bot 30 分鐘內最多重開 3 次，之後只通知、不再開。
    #[tokio::test]
    async fn a_bot_that_keeps_getting_lost_is_restarted_a_few_times_then_only_reported() {
        let env = tt::env().await;
        let bot = autostart_bot(&env, "flappy").await;
        crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        autostart_done(&env.app);
        for _ in 0..5 {
            herdr_forgets_everything(&env).await;
            crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();
        }
        let runs = runs(&env.app, &bot.id).await;
        assert_eq!(runs.len(), 4, "原本那顆＋最多 3 次重開：{runs:?}");
        let events = bot_lost_events(&env.app, &bot.id).await;
        assert_eq!(events.len(), 4, "每次遺失都通知（第 5 輪已經沒有 run 可丟）：{events:?}");
        assert_eq!(events.iter().filter(|e| e["outcome"] == "restarted").count(), 3);
        assert_eq!(events.iter().filter(|e| e["outcome"] == "backoff").count(), 1);
        assert!(db::active_run(&env.app.db, &bot.id).await.unwrap().is_none(), "退避之後留在停掉的狀態，交給 supervisor");
    }
}
