//! herdr 斷線重連或 server 重啟之後，把「因為 agent 不見而被對帳收成 exited」的 autostart bot 再起一次。
//!
//! 對帳發現 run 的 agent 不在（`agent not found during reconcile`）只有兩種情況：herdr 的 pane／server 在 daemon 沒看到事件時
//! 掉了（herdr 更新、當機、daemon 斷線那段 pane 被關）。使用者手動停（`stopped`）、在 herdr 裡關 pane（`pane exited` 事件）
//! 都不走這條，所以這裡只看這個原因。以前這種 bot 要等 `bot_stopped` 探針（300 秒）才報、而且 autostart 每台主機一生只跑一次，
//! 不會自己回來（2026-10-02 herdr 0.9.3 更新後實際發生）。
//!
//! * 只在開機那一輪 autostart 跑完之後才動（`autostart_hosts` 是 `Done`）：開機那一輪由 autostart 負責，不搶著起第二次。
//! * herdr 計畫中的維護期間不動（`herdr_maintenance`）：那邊自己會把 bot 接回來。
//! * 起之前再驗一次：bot 還在、還是 autostart、沒有 active run、最後一個 run 就是這次被收掉的那個而且退出原因是對帳記的
//!   （使用者在中間做了任何事＝有新 run＝不碰；`pane exited` 等別的路收的＝使用者關的，不碰）。
//! * 對帳那頭只有「這一輪自己記下 exited」才算遺失（`AlreadyEnded`＝別的路先收了，不是 herdr 掉的）；補開丟到背景一顆一顆做，不卡對帳。
//! * 退避：同一顆 bot 30 分鐘內最多重開 3 次；herdr 一直掛時只通知、不再開（避免無限重開）。
//! * 每次遺失都推一則 supervisor inbox `bot_lost`（巡檢收、叫醒），帶 `outcome`：`restarted`／`failed`／`backoff`，不等探針。
use crate::events::ports::{HostSidePort, SupervisorRepo, TurnCommands};
use crate::db;
use crate::state::{App, AutostartHostStatus};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 這一輪對帳收掉的 autostart bot（原因是 agent 不見）。
#[derive(Debug, Clone)]
pub(crate) struct Lost {
    pub bot_id: String,
    pub run_id: String,
}

/// 對帳收掉 run 時記的退出原因（`reconcile.rs`）；只有這個原因才是「herdr 掉了 agent」。
const LOST_REASON: &str = "agent not found during reconcile";

const MAX_RESTARTS: usize = 3;
const WINDOW: Duration = Duration::from_secs(30 * 60);

/// 30 分鐘內的啟動嘗試記在 durable `bot_lost` inbox 事件裡；daemon 重啟不能把退避額度歸零。
async fn take_slot(app: &impl crate::capabilities::Db, bot_id: &str) -> anyhow::Result<bool> {
    let cutoff = db::iso_in(-(WINDOW.as_secs() as i64));
    let hits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM supervisor_inbox
         WHERE bot_id = ? AND kind = 'bot_lost' AND created_at >= ?
           AND json_extract(payload_json, '$.outcome') IN ('restarted', 'failed')",
    )
    .bind(bot_id)
    .bind(cutoff)
    .fetch_one(app.db())
    .await?;
    Ok(hits < MAX_RESTARTS as i64)
}

/// 還沒做完的背景補開，依 `App` 分開記（測試用它等補開收尾；平行的測試各有各的 `App`，不能共用一個計數）。
static PENDING: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();

fn pending() -> &'static Mutex<HashMap<usize, usize>> {
    PENDING.get_or_init(Default::default)
}

fn pending_key(app: &Arc<App>) -> usize {
    Arc::as_ptr(app) as usize
}

/// 對帳一輪做完（host 的 pass 鎖已放開）之後呼叫：**丟到背景**，不讓對帳呼叫端等。
///
/// 補開是一顆一顆 `start_bot`（每顆要開 pane、等 CLI 起來，遠端還要 ssh，一顆動輒十幾秒）。對帳是 supervisor 連上之後的第一步：
/// 後面的全域事件訂閱、spool 補放、工具偵測、autostart 都排在它後面，herdr 一次更新掉了十顆 bot 的話，
/// 同步等著補開就是整台主機的事件訂閱晚好幾分鐘才建。一顆一顆補（不並行）是刻意的：不要同時對 herdr 與額度開十個 CLI。
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
    if eligible_lost_bot(app, host, l).await?.is_none() {
        return Ok(());
    }
    #[cfg(test)]
    crate::lifecycle::race_point::hit("autostart_revive_before_start", &l.bot_id).await;

    let lock = app.bot_lock(&l.bot_id).await;
    let guard = lock.lock_owned().await;
    let Some(bot) = eligible_lost_bot(app, host, l).await? else { return Ok(()) };
    let (outcome, error) = if !take_slot(app, &bot.id).await? {
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
            start_app.start_bot_locked_with(&bot_id, crate::lifecycle::StartOpts::default()).await
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
    if let Err(e) = app.db.push_inbox(&format!("bot_lost:{}:{}", bot.id, l.run_id), "bot_lost", None, Some(&bot.id), None, &payload).await {
        tracing::warn!(bot = %bot.name, error = ?e, "autostart revive: could not write the bot_lost inbox event");
    }
    Ok(())
}

/// Only the run reconcile just lost, still on the same host, is eligible for revival. Call before
/// and after acquiring the bot lock.
async fn eligible_lost_bot(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes), host: &str, l: &Lost) -> anyhow::Result<Option<db::Bot>> {
    let Some(bot) = db::bot(app.db(), &l.bot_id).await? else { return Ok(None) };
    if bot.deleted_at.is_some() || bot.autostart != 1 || bot.managed_by == "child" {
        return Ok(None);
    }
    if db::bot_host(app.db(), &bot.id).await? != host || !app.host_connected(host).await || db::active_run(app.db(), &bot.id).await?.is_some() {
        return Ok(None);
    }
    let last: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT id, state, exit_reason FROM runs WHERE bot_id = ? ORDER BY started_at DESC, rowid DESC LIMIT 1")
            .bind(&bot.id)
            .fetch_optional(app.db())
            .await?;
    // 使用者關 pane 或其他路徑收掉的 run，都不是這次對帳發現的遺失。
    if last.as_ref().map(|(id, state, why)| (id.as_str(), state.as_str(), why.as_deref())) != Some((l.run_id.as_str(), "exited", Some(LOST_REASON))) {
        return Ok(None);
    }
    Ok(Some(bot))
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
        super::quiesce(&env.app).await;

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
        super::quiesce(&env.app).await;

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
        super::quiesce(&env.app).await;

        assert!(db::active_run(&env.app.db, &bot.id).await.unwrap().is_none());
        assert!(bot_lost_events(&env.app, &bot.id).await.is_empty());
    }

    /// 使用者在 herdr 裡關掉 pane：`pane exited` 事件那條路把 run 收成 exited（原因 `pane exited`）。對帳剛好同一瞬也發現 agent 不見——
    /// 它讀完 run、還沒寫 exited 的那一瞬被事件搶先（CAS 輸了，`AlreadyEnded`）。那是使用者要它停，不是 herdr 掉的：不能拉起來
    /// （SPEC 寫「在 herdr 裡關 pane 都不走這條」）。以前對帳在 `AlreadyEnded` 也把它記成遺失，`revive_one` 又只看「最後一個 run 是 exited」。
    #[tokio::test]
    async fn a_pane_the_user_closed_is_not_revived_when_the_event_wins_the_race() {
        let env = tt::env().await;
        let bot = autostart_bot(&env, "closed").await;
        crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        autostart_done(&env.app);
        let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap().id;
        herdr_forgets_everything(&env).await;
        let app = env.app.clone();
        let run2 = run.clone();
        crate::lifecycle::race_point::arm("mark_run_exited_after_read", &run, move || async move {
            // pane-exited 事件那條路（events.rs）：在對帳讀完 run、還沒寫 exited 的那一瞬先收掉它。
            crate::lifecycle::mark_run_exited(&app, &run2, "pane exited").await;
        });

        crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();
        super::quiesce(&env.app).await;

        let runs = runs(&env.app, &bot.id).await;
        assert_eq!(runs, vec![("exited".to_string(), Some("pane exited".to_string()))], "使用者關的 pane 不能被拉起來：{runs:?}");
        assert!(db::active_run(&env.app.db, &bot.id).await.unwrap().is_none());
        assert!(bot_lost_events(&env.app, &bot.id).await.is_empty(), "不是 herdr 掉的，不推 bot_lost");
    }

    /// 補開是一顆一顆 `start_bot`，動輒十幾秒；對帳是 supervisor 連上之後的第一步（後面還有事件訂閱、spool 補放、工具偵測）。
    /// 補開卡住（這裡用一把被測試握著的 bot 鎖讓 `start_bot` 一直等）時，對帳呼叫端照樣要回來——不靠時間：
    /// 對帳回來的當下補開還沒做完（還在等鎖、沒有新 run），放開鎖之後它才做完。
    #[tokio::test]
    async fn a_stuck_revive_does_not_hold_up_the_reconcile_caller() {
        let env = tt::env().await;
        let bot = autostart_bot(&env, "slow").await;
        crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        autostart_done(&env.app);
        let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap().id;
        herdr_forgets_everything(&env).await;
        let release = Arc::new(tokio::sync::Notify::new());
        let (app, bot_id, gate) = (env.app.clone(), bot.id.clone(), release.clone());
        // 對帳讀完 run 的那一瞬排一個等這顆 bot 鎖的 task：它排在對帳 pass 後面，pass 一放開就拿到、握到測試放手，補開的 start_bot 因此卡住。
        crate::lifecycle::race_point::arm("mark_run_exited_after_read", &run, move || async move {
            tokio::spawn(async move {
                let lock = app.bot_lock(&bot_id).await;
                let _g = lock.lock().await;
                gate.notified().await;
            });
            tokio::task::yield_now().await;
        });

        tokio::time::timeout(std::time::Duration::from_secs(20), crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST))
            .await
            .expect("對帳不能等補開")
            .unwrap();
        assert_eq!(super::pending_count(&env.app), 1, "對帳回來的時候補開還沒做完");
        assert!(db::active_run(&env.app.db, &bot.id).await.unwrap().is_none(), "還在等鎖：沒有新 run");

        release.notify_one();
        super::quiesce(&env.app).await;
        let events = bot_lost_events(&env.app, &bot.id).await;
        assert_eq!(events.len(), 1, "補開照樣做完：{events:?}");
        assert_eq!(events[0]["outcome"], "restarted");
    }

    /// 遠端主機睡著／斷線：herdr 那頭連不上不是「agent 被 herdr 清掉」。對帳讀不到 snapshot 就整輪跳過（什麼都不收、不補開、不推 inbox），
    /// 連回來之後真的對到帳，才分得出誰活著、誰真的掉了。
    #[tokio::test]
    async fn an_unreachable_remote_host_loses_nothing_and_revives_nothing() {
        let env = tt::env().await;
        let host = "revive-asleep";
        let cfg = config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        assert!(!conn.is_connected());
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let bot = autostart_bot(&env, "far").await;
        let run = tt::fake_run(&env.app, &bot.id).await;
        env.app.autostart_hosts.lock().unwrap().insert(host.to_string(), AutostartHostStatus::Done);

        assert!(crate::reconcile::reconcile_host(&env.app, host).await.is_err(), "連不上：整輪跳過");
        super::quiesce(&env.app).await;

        let still: Option<db::Run> = db::active_run(&env.app.db, &bot.id).await.unwrap();
        assert_eq!(still.map(|r| r.id), Some(run), "run 沒被收");
        assert_eq!(runs(&env.app, &bot.id).await.len(), 1, "沒有補開");
        assert!(bot_lost_events(&env.app, &bot.id).await.is_empty(), "沒有 inbox");
    }

    /// Reconcile can succeed and the remote host can disconnect while the background revival waits
    /// for the bot lock. That is a skip, not a failed start that spends one of the retry slots.
    #[tokio::test]
    async fn a_remote_host_that_disconnects_before_revival_is_skipped_without_an_attempt() {
        use std::sync::atomic::Ordering;

        let env = tt::env().await;
        let host = "revive-late-offline";
        let cfg = config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: "127.0.0.1".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        conn.connected.store(true, Ordering::SeqCst);
        sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(host).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        let bot = autostart_bot(&env, "late-offline").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        crate::lifecycle::mark_run_exited(&env.app, &run_id, super::LOST_REASON).await;
        env.app.autostart_hosts.lock().unwrap().insert(host.to_string(), AutostartHostStatus::Done);

        let conn_after_check = conn.clone();
        crate::lifecycle::race_point::arm("autostart_revive_before_start", &bot.id, move || async move {
            conn_after_check.connected.store(false, Ordering::SeqCst);
        });
        super::revive(&env.app, host, vec![super::Lost { bot_id: bot.id.clone(), run_id }]).await;

        assert_eq!(runs(&env.app, &bot.id).await.len(), 1, "offline host is not restarted");
        assert!(bot_lost_events(&env.app, &bot.id).await.is_empty(), "offline is skipped, not counted as a failed attempt");
    }

    /// 同一件事的第二道防線：就算有人（或之後的改動）把這顆 run 當成遺失交給 `revive`，`revive_one` 也要確認
    /// 那顆 run 的退出原因真的是對帳記的「agent 不見」，而不只是「最後一個 run 是 exited」。
    #[tokio::test]
    async fn revive_only_trusts_a_run_whose_recorded_exit_is_the_reconcile_one() {
        let env = tt::env().await;
        let bot = autostart_bot(&env, "claimed").await;
        crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        autostart_done(&env.app);
        let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap().id;
        crate::lifecycle::mark_run_exited(&env.app, &run, "pane exited").await;

        super::revive(&env.app, config::LOCAL_HOST, vec![super::Lost { bot_id: bot.id.clone(), run_id: run.clone() }]).await;

        assert_eq!(runs(&env.app, &bot.id).await.len(), 1, "原因是 pane exited：不拉起來");
        assert!(bot_lost_events(&env.app, &bot.id).await.is_empty());
    }

    /// 使用者可在補開完成前啟動又停止 bot。資格檢查與 `start_bot` 之間要共用 bot lock 並重驗，不能推翻這次停機。
    #[tokio::test]
    async fn an_autostart_revival_does_not_override_a_user_start_then_stop_race() {
        let env = tt::env().await;
        let bot = autostart_bot(&env, "revive-user-stop-race").await;
        crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        autostart_done(&env.app);
        let lost_run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap().id;
        crate::lifecycle::mark_run_exited(&env.app, &lost_run, super::LOST_REASON).await;

        let (app, bot_id) = (env.app.clone(), bot.id.clone());
        crate::lifecycle::race_point::arm("autostart_revive_before_start", &bot.id, move || async move {
            let user_run = crate::lifecycle::start_bot(&app, &bot_id).await.unwrap();
            crate::lifecycle::stop_bot(&app, &bot_id).await.unwrap();
            assert_ne!(user_run, "");
        });
        super::revive(&env.app, config::LOCAL_HOST, vec![super::Lost { bot_id: bot.id.clone(), run_id: lost_run }]).await;

        assert_eq!(runs(&env.app, &bot.id).await.len(), 2, "補開不能多啟動第三個 run");
        assert!(db::active_run(&env.app.db, &bot.id).await.unwrap().is_none(), "使用者的 stop 保持有效");
        assert!(bot_lost_events(&env.app, &bot.id).await.is_empty(), "使用者接手後不算 revive attempt");
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
        super::quiesce(&env.app).await;
        }
        let runs = runs(&env.app, &bot.id).await;
        assert_eq!(runs.len(), 4, "原本那顆＋最多 3 次重開：{runs:?}");
        let events = bot_lost_events(&env.app, &bot.id).await;
        assert_eq!(events.len(), 4, "每次遺失都通知（第 5 輪已經沒有 run 可丟）：{events:?}");
        assert_eq!(events.iter().filter(|e| e["outcome"] == "restarted").count(), 3);
        assert_eq!(events.iter().filter(|e| e["outcome"] == "backoff").count(), 1);
        assert!(db::active_run(&env.app.db, &bot.id).await.unwrap().is_none(), "退避之後留在停掉的狀態，交給 supervisor");
    }

    #[tokio::test]
    async fn the_restart_limit_survives_a_daemon_restart() {
        let env = tt::env().await;
        let bot = autostart_bot(&env, "flappy-restart").await;
        crate::lifecycle::start_bot(&env.app, &bot.id).await.unwrap();
        autostart_done(&env.app);
        // Restore the durable history a fresh daemon would see after a restart; its process-local map starts empty.
        for attempt in 0..super::MAX_RESTARTS {
            crate::supervisor::store::push_inbox(
                &env.app.db,
                &format!("bot_lost:{}:prior-{attempt}", bot.id),
                "bot_lost",
                None,
                Some(&bot.id),
                None,
                &serde_json::json!({"outcome": "restarted"}),
            )
            .await
            .unwrap();
        }
        herdr_forgets_everything(&env).await;
        crate::reconcile::reconcile_host(&env.app, config::LOCAL_HOST).await.unwrap();
        super::quiesce(&env.app).await;

        assert!(db::active_run(&env.app.db, &bot.id).await.unwrap().is_none(), "重啟 daemon 不該重設 30 分鐘內 3 次的上限");
        let events = bot_lost_events(&env.app, &bot.id).await;
        assert_eq!(events.len(), super::MAX_RESTARTS + 1, "第 4 次遺失只留告警：{events:?}");
        assert_eq!(events.iter().filter(|e| e["outcome"] == "restarted").count(), super::MAX_RESTARTS);
        assert_eq!(events.iter().filter(|e| e["outcome"] == "backoff").count(), 1);
    }
}
