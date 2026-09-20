//! Stopping, interrupting and aborting: the ways a run or a turn ends on purpose.

use super::*;

pub async fn stop_bot(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    stop_bot_locked(app, bot_id).await
}

/// Is this run sitting in the user's own herdr `default` session (SPEC §6.5.1)?
pub fn in_default_session(run: &db::Run) -> bool {
    run.herdr_session.as_deref() == Some("default")
}

/// SPEC §6.5.1: a bot from the user's `default` session is observed, never driven — start
/// would create a workspace in their session, restart would close their pane (review 2026-09-12 #4).
pub(crate) fn refuse_default_session(bot: &db::Bot) -> LcResult<()> {
    if bot.herdr_session.as_deref() == Some("default") {
        return Err(LcError::conflict(
            "default_session",
            json!({"bot_id": bot.id,
                   "message": "這顆是從你自己的 herdr default session 匯入的，daemon 只觀察、不替它開或關 pane：要重啟請在那個終端裡自己做。"}),
        ));
    }
    Ok(())
}

/// [`stop_bot`] with the lock already held, so a restart stops and starts under one guard.
pub async fn stop_bot_locked(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    stop_locked(app, bot_id, false, false).await
}

/// 閒置回收用的 [`stop_bot_locked`]：記 `stopping` 的那一步同時是**收機許可**（issue #144）——`agent_status` 還是
/// `idle`、這個 run 沒有 in-flight 回合、這顆 bot 沒有排隊中的回合，才准停；否則什麼都不動，回 409 `no_longer_idle`。
///
/// bot 鎖擋得住經 daemon 進來的新工作，擋不住使用者直接在 pane 裡打字：那條路是 `events::handle_status`，不拿鎖就寫
/// `agent_status`。巡邏在鎖裡最後一次讀到 idle 之後、停機之前被那一句插進來的話，以前照樣 ctrl+c、關 pane。條件寫在
/// 同一句 UPDATE 裡，跟那一句寫入由 SQLite 排序：它先落地，這裡 0 rows 不停；這裡先落地，run 已經是 `stopping`，
/// `begin_external_turn`（在鎖裡看 `state == running`）就不會替一個正在關的 pane 開回合。
pub async fn stop_bot_locked_if_idle(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    stop_locked(app, bot_id, false, true).await
}

/// 重啟那一半的 stop。差別只在 `stopped` 寫不進去之後的重試：重啟沒把 bot 開回來不是「使用者要它停」
/// （同 `left_down_by_restart`），所以交給對帳照證據收成 `exited`，不補記 `stopped`。
pub(crate) async fn stop_for_restart_locked(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    stop_locked(app, bot_id, true, false).await
}

/// ctrl+c（必要時關 pane）之後，外面的 agent 到底怎麼了（#146）。只有前兩種能記成 `stopped`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopOutcome {
    /// agent 自己退出了（或 pane 已經不在）。
    Gone,
    /// 沒退出，pane 被強制關掉、確認不在了。
    ForcedClosed,
    /// agent 還在：default session 不能關使用者的 pane，或 pane 關不掉。
    StillAlive,
    /// 問不到 herdr，不知道。
    Unknown,
}

async fn stop_locked(app: &Arc<App>, bot_id: &str, for_restart: bool, only_if_idle: bool) -> LcResult<bool> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? else {
        // agent 自己退了、預覽還掛著的話也一併收（§6.12）。
        crate::preview::stop_for_bot(app, bot_id).await;
        return Ok(false);
    };
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    let client = client_for_run(app, &run).await?;
    // 讀完 active run、還沒記 `stopping` 的那一瞬（測試在這裡插進不拿 bot 鎖的 pane-exit 事件）。
    #[cfg(test)]
    {
        super::race_point::hit("stop_before_stopping", bot_id).await;
    }

    // 先把「正在停」記下來，才有權動外面（#146）：記不下來就一步都不做——不收 in-flight、不送 ctrl+c、
    // 不關 pane、不撤佇列。讀完 active run 之後被不拿鎖的 pane-exit 事件先收掉的話（CAS 輸了），收尾歸那條路。
    // 來源含 `stopping`：上一次沒停成（寫不進 `stopped`、agent 沒退出）的 stop 可以原樣再按一次。
    // 使用者的 stop 在同一個交易撤回等它起來才送的那幾則（[`begin_stop`]，#199）。閒置回收的許可本來就要求沒有排著的。
    let (moved, withdrawn) = if only_if_idle {
        (admit_idle_stop(&app.db, &run.id, bot_id).await.map_err(up)?, Vec::new())
    } else {
        let withdraw = !for_restart && !super::restart_hold::in_progress(bot_id);
        begin_stop(app, &run.id, bot_id, withdraw).await.map_err(up)?
    };
    for (turn_id, revoked) in withdrawn {
        announce_revoked(app, &turn_id, revoked).await;
    }
    match moved {
        super::run_state::Moved::Applied => {}
        // 閒置回收的許可沒過：它不再閒著（或已經被別的路停掉）。外面一步都沒動。
        super::run_state::Moved::Lost if only_if_idle => {
            app.emit_bot_status(bot_id).await;
            return Err(LcError::conflict("no_longer_idle", json!({"bot_id": bot_id, "run_id": run.id})));
        }
        super::run_state::Moved::Lost => {
            tracing::info!(bot = %bot.name, run = %run.id, "stop: another path ended the run first; nothing left to stop");
            app.emit_bot_status(bot_id).await;
            return Ok(false);
        }
    }
    app.emit_bot_status(bot_id).await;
    // 在飛的那一筆先收，收不成就一步都不動外面（#156）：這時 agent 還沒被打斷，那一回合真的還在跑。
    if let Err(e) = fail_in_flight(app, &run.id, "run stopped by user").await {
        return Err(turn_unwritable(app, bot_id, &run.id, "停", e).await);
    }

    // 預覽的 pane 跟 agent 同一個 tab：先收，agent 的 pane 關掉時那個 tab 才會是空的（§6.12）。
    crate::preview::stop_for_bot(app, bot_id).await;
    let target = db::run_target(&run, &bot);
    for _ in 0..2 {
        let _ = client.agent_send_keys(&target, &["ctrl+c".to_string()]).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // 只認 herdr 明確說「不在」：RPC 失敗不是退出的證據。
    let mut gone = false;
    for _ in 0..20 {
        let agent = client.agent_get(&target).await;
        let pane = match run.pane_id.as_deref() {
            Some(p) => Some(client.pane_get(p).await),
            None => None,
        };
        if matches!(agent, Ok(None)) || matches!(pane, Some(Ok(None))) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Close the Run's pane (and its tab if owned) so no bare shell lingers — except in the user's
    // `default` session (SPEC §6.5.1): only ctrl+c; closing took their terminal (review 2026-09-12 #4).
    let outcome = if in_default_session(&run) {
        if gone {
            StopOutcome::Gone
        } else {
            tracing::warn!(bot = %bot.name, "agent did not exit within 10s; its pane is the user's own and is left open");
            StopOutcome::StillAlive
        }
    } else if let Some(p) = run.pane_id.as_deref() {
        close_pane_and_tab(&client, run.workspace_id.as_deref(), run.tab_id.as_deref(), p).await;
        if gone {
            StopOutcome::Gone
        } else {
            tracing::warn!(bot = %bot.name, "agent did not exit within 10s; closing its pane forcibly");
            match client.pane_get(p).await {
                Ok(None) => StopOutcome::ForcedClosed,
                Ok(Some(_)) => StopOutcome::StillAlive,
                Err(_) => StopOutcome::Unknown,
            }
        }
    } else if gone {
        StopOutcome::Gone
    } else {
        StopOutcome::Unknown
    };
    if matches!(outcome, StopOutcome::StillAlive | StopOutcome::Unknown) {
        // agent 可能還活著：不能記成 `stopped`、不撤佇列（#146 驗收 3）。確定還在就放回 `running`；
        // 問不到就留 `stopping`，讓對帳照證據收（看得到 agent 放回 running，看不到收成 exited）。
        if outcome == StopOutcome::StillAlive {
            back_to_running(app, &run.id).await;
        } else {
            super::run_state::schedule_settle(app, &run.id, super::run_state::Settle::Reconcile { stuck: "stopping".into() });
        }
        app.emit_bot_status(bot_id).await;
        return Err(LcError::Upstream(format!(
            "stop_not_confirmed: agent `{target}` did not exit ({outcome:?}); the run is not recorded as stopped"
        )));
    }
    // pane 關了、`stopped` 還沒記下的那一瞬（herdr 的 pane_closed 事件會在這裡搶進來）。
    #[cfg(test)]
    {
        super::race_point::hit("stop_before_stopped", bot_id).await;
    }
    match commit_stopped(app, &run.id, for_restart).await {
        Ok(StopCommit::Applied) => after_stop(app, bot_id, &run, &host).await,
        // 被 pane-exit 事件收成 `exited`（已改標成 `stopped`）：一般的收尾那條路做過了，不做第二份（stop 才有的撤回
        // 在記 `stopping` 時就跟著做了）。
        Ok(StopCommit::Relabelled | StopCommit::Lost) => {}
        // agent 已經停了，終態卻寫不進去（#146 驗收 2）：不回一般的成功，佇列與 watcher 等終態成立再動，排重試。
        // 被 pane-exit 先收成 `exited`、改標回 `stopped` 寫不進去也是這一支（#146 重開 A）：留著 `exited` 就是在說
        // 「它自己掛了」，autostart 的 bot 會被報 `bot_stopped`。重試（`FinishStop`）照樣補改標。
        Err(e) => {
            tracing::warn!(bot = %bot.name, run = %run.id, error = %e, "the agent is stopped but its run could not be recorded as stopped");
            let how = if for_restart {
                super::run_state::Settle::Reconcile { stuck: "stopping".into() }
            } else {
                super::run_state::Settle::FinishStop
            };
            super::run_state::schedule_settle(app, &run.id, how);
            app.emit_bot_status(bot_id).await;
            return Err(LcError::uncommitted(
                "stop_state_uncommitted",
                &run.id,
                "agent 已經停了，但 run 的狀態寫不進 DB；已排重試，會補記成停止",
                e,
            ));
        }
    }
    app.emit_bot_status(bot_id).await;
    Ok(true)
}

/// [`commit_stopped`] 做成了什麼。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopCommit {
    /// `stopping → stopped`：收尾全部歸這裡（[`after_stop`]）。
    Applied,
    /// 被 pane-exit 事件先收成 `exited`，改標回 `stopped`：一般的收尾那條路做過了。
    Relabelled,
    /// 兩者都不是：別的路徑已經收成別的樣子，什麼都不做。
    Lost,
}

/// `LIVE → stopping`；`withdraw` 時同一個交易撤回還在等它起來才送的那幾則（`start_send::withdraw_waiting_tx`，#199）：
/// 要嘛「正在停」與撤回都成立，要嘛都不成立——撤回寫不進去就連 `stopping` 都不記，外面一步都不動（跟 `stopping` 寫不進去
/// 同一條路，502）。撤回放在這裡、不放在記 `stopped` 那一步：`stopped` 寫不進去時 run 停在 `stopping`，重試補上之前
/// daemon 重啟的話，開機對帳把它收成 `exited`，`start_send::resume_after_boot` 看到沒有 run 就替它啟動——那一則照樣送出去。
/// 跟著第一個 durable 的「使用者要它停」一起落地，之後不管怎麼收斂都送不出去。CAS 輸了什麼都不寫。
async fn begin_stop(
    app: &Arc<App>,
    run_id: &str,
    bot_id: &str,
    withdraw: bool,
) -> anyhow::Result<(super::run_state::Moved, Vec<(String, Revoked)>)> {
    let mut tx = app.db.begin().await?;
    let moved = super::run_state::transition_on(&mut tx, run_id, super::run_state::LIVE, "stopping", None).await?;
    if moved == super::run_state::Moved::Lost {
        return Ok((moved, Vec::new()));
    }
    let withdrawn = if withdraw { super::start_send::withdraw_waiting_tx(&mut tx, bot_id).await? } else { Vec::new() };
    tx.commit().await?;
    Ok((moved, withdrawn))
}

/// [`stop_bot_locked_if_idle`] 的許可：跟 `transition(LIVE → stopping)` 同一步，多三個條件，都在同一句 UPDATE 裡。
async fn admit_idle_stop(db: &sqlx::SqlitePool, run_id: &str, bot_id: &str) -> Result<super::run_state::Moved, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE runs SET state = 'stopping'
          WHERE id = ? AND state = 'running' AND agent_status = 'idle'
            AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.run_id = runs.id AND t.status = 'in_flight')
            AND NOT EXISTS (SELECT 1 FROM turns t JOIN conversations c ON c.id = t.conversation_id
                             WHERE c.bot_id = ? AND t.status = 'queued')",
    )
    .bind(run_id)
    .bind(bot_id)
    .execute(db)
    .await?;
    Ok(if r.rows_affected() == 0 { super::run_state::Moved::Lost } else { super::run_state::Moved::Applied })
}

/// `stopping → stopped`。CAS 輸了而且是被不拿 bot 鎖的 pane-exit 事件收成 `exited`（多半是 stop 自己關的 pane
/// 觸發的）：改標成 `stopped`（`run_state::relabel`）——兩個都是終態、不復活任何東西，但 `stopped` 才是「使用者要它停」
/// 的紀錄（#131，incident 探針靠它分辨）。改標寫不進去回錯，不當成 CAS 輸了（#146 重開 A）。
///
/// 重啟那一半（`for_restart`）不改標：重啟不是「使用者要它停」——開不回來時要的正是 `exited`（同 `left_down_by_restart`），
/// 開回來了舊 run 的標籤也不再代表 bot。
async fn commit_stopped(app: &Arc<App>, run_id: &str, for_restart: bool) -> Result<StopCommit, sqlx::Error> {
    match super::run_state::transition(&app.db, run_id, &["stopping"], "stopped", None).await? {
        super::run_state::Moved::Applied => Ok(StopCommit::Applied),
        super::run_state::Moved::Lost if for_restart => Ok(StopCommit::Lost),
        super::run_state::Moved::Lost => match super::run_state::relabel(&app.db, run_id, "exited", "stopped").await? {
            super::run_state::Moved::Applied => Ok(StopCommit::Relabelled),
            super::run_state::Moved::Lost => Ok(StopCommit::Lost),
        },
    }
}

/// 終態寫進去之後才做的收尾（跟 #135 同一條：先有 durable 的終態，才撤佇列、拆 watcher）。
async fn after_stop(app: &Arc<App>, bot_id: &str, run: &db::Run, host: &str) {
    // 停掉之後沒有人會送它排著的 queued：收掉，不留著佔名額、擋 restart safety（AGM 2026-09-16）。
    revoke_orphaned_queued_turns(app, bot_id, "bot 已被停止").await;
    if let Some(p) = run.pane_id.as_deref() {
        if let Some(session) = app.session_for_run(run).await {
            crate::events::unwatch_pane_on_session(app, host, &session, p).await;
        }
    }
}

/// 停不下來（agent 還在）：`stopping → running` 放回去。留在 `stopping` 會讓 prompt 409、start 拒絕，
/// default session 的 run 連對帳都不救（2026-09-12 review #1）。寫不進去就排重試。
/// 停（或原地重啟）之前，在飛的那一筆收不成（#156）：外面還一步都沒動——agent 沒被打斷、那一回合真的還在跑。
/// run 放回 running，回 503 可重試。不是 `Uncommitted`：那是「外面已經做了、DB 沒寫成」，這裡外面什麼都還沒做。
pub(crate) async fn turn_unwritable(app: &Arc<App>, bot_id: &str, run_id: &str, what: &str, e: anyhow::Error) -> LcError {
    tracing::warn!(bot = bot_id, run = run_id, error = %e, "the in-flight turn could not be closed; not touching the agent");
    back_to_running(app, run_id).await;
    app.emit_bot_status(bot_id).await;
    let turn = db::in_flight_turn(&app.db, run_id).await.ok().flatten().map(|t| t.id);
    LcError::Unavailable(json!({
        "error": "turn_state_unwritable", "run_id": run_id, "turn_id": turn, "retryable": true, "retry_after_secs": 5,
        "message": format!("在飛的那一回合寫不進 DB，沒有{what}：agent 照常在跑（那一回合也還在跑），稍後再試。"),
        "detail": format!("{e:#}"),
    }))
}

pub(crate) async fn back_to_running(app: &Arc<App>, run_id: &str) {
    if let Err(e) = super::run_state::transition(&app.db, run_id, &["stopping"], "running", None).await {
        tracing::warn!(run = run_id, error = %e, "could not put a run that did not stop back to running");
        super::run_state::schedule_settle(app, run_id, super::run_state::Settle::BackToRunning);
    }
}

/// [`run_state::Settle::FinishStop`] 的重試：stop 在外面已經做完，補記 `stopped` 與它之後的收尾。
/// 在 bot 鎖裡做；只動仍停在 `stopping`（或被 pane-exit 收成 `exited`、還沒改標）的這一顆。
pub(crate) async fn finish_stop(app: &Arc<App>, run_id: &str) {
    let Ok(Some(run)) = db::run(&app.db, run_id).await else { return };
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    // 收尾要拆那台主機上的 watcher：讀不到主機就這一輪什麼都不寫，下一輪重試（不退回 local——拆錯台，這台的 watcher 就留著）。
    let host = match db::bot_host(&app.db, &run.bot_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(run = run_id, error = %e, "cannot read the host of a run whose stop is being recorded; retrying");
            return;
        }
    };
    match commit_stopped(app, run_id, false).await {
        Ok(StopCommit::Applied) => after_stop(app, &run.bot_id, &run, &host).await,
        Ok(StopCommit::Relabelled | StopCommit::Lost) => {}
        Err(e) => tracing::warn!(run = run_id, error = %e, "retrying the stopped record failed"),
    }
    app.emit_bot_status(&run.bot_id).await;
}

/// stop (if running) + start. Used to make edited `model` / `args` / `identity` / `env` take effect.
pub async fn run_alive(app: &Arc<App>, run: &db::Run, bot: &db::Bot) -> bool {
    let Some(pane) = run.pane_id.as_deref() else { return false };
    let Ok(client) = client_for_run(app, run).await else { return true };
    match client.pane_get(pane).await {
        Ok(None) => return false,
        // The pane says an agent is in it: `agent.get` by name can briefly miss after a same-named
        // restart on a new pane (2026-09-11).
        Ok(Some(p)) if p.agent.is_some() => return true,
        _ => {}
    }
    !matches!(client.agent_get(&db::run_target(run, bot)).await, Ok(None))
}

/// #61: one-time purge of `bots/<id>/` for soft-deleted bots with no live run. Dirs no bot row
/// claims are left alone (rt-87's `bots-orphan-backup-2026-09-10/` is outside `bots/`).
pub async fn purge_deleted_bot_dirs(app: &Arc<App>) -> usize {
    let root = app.data_dir.join("bots");
    let Ok(entries) = std::fs::read_dir(&root) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else { continue };
        // 只有「確定是軟刪」而且「確定沒有 active run」才有權刪：讀不到（DB 一時忙、I/O 錯）不等於沒有，留著、記一行，
        // 下一次啟動再判斷（清理可重入）。刪掉還在跑的 bot 的目錄會拿走它的 hook／shim／spool，補不回來。
        let deleted: Option<Option<String>> = match sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id = ?")
            .bind(&id)
            .fetch_optional(&app.db)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(bot = %id, error = %e, "could not read whether a bots/<id>/ directory's bot is deleted; leaving it for the next start");
                continue;
            }
        };
        if !matches!(deleted, Some(Some(_))) {
            continue;
        }
        match db::active_run(&app.db, &id).await {
            Ok(None) => {}
            Ok(Some(_)) => continue,
            Err(e) => {
                tracing::warn!(bot = %id, error = %e, "could not read whether a deleted bot still has a live run; leaving its directory for the next start");
                continue;
            }
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(dir = %entry.path().display(), error = %e, "could not remove a deleted bot's directory"),
        }
    }
    if removed > 0 {
        tracing::info!(removed, "removed bots/<id>/ directories left behind by deleted bots");
    }
    removed
}

/// 回 `true`＝目錄確定沒了（含本來就不在）；`false`＝沒清成（id 不合法、主機不明、ssh 失敗、I/O 錯），呼叫端不能當成清掉了。
pub async fn purge_bot_dir(app: &Arc<App>, bot_id: &str, host: &str) -> bool {
    if !valid_id(bot_id) {
        tracing::warn!(host, bot = %bot_id, "invalid bot id; bot config dir left in place");
        return false;
    }
    if host == LOCAL_HOST {
        let Ok(dir) = app.bot_dir(bot_id) else { return false };
        return match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                tracing::info!(dir = %dir.display(), "removed bot config dir");
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "could not remove bot config dir");
                false
            }
        };
    }
    let Some(conn) = app.hosts.get(host).await else {
        tracing::warn!(host, bot = %bot_id, "unknown host; remote bot dir left in place");
        return false;
    };
    let res = async {
        let p = remote_bot_dir(&conn, bot_id).await?;
        conn.ssh_exec(&format!("rm -rf {}\n", sh_quote(&p.dir))).await?;
        Ok::<_, anyhow::Error>(p.dir)
    }
    .await;
    match res {
        Ok(dir) => {
            tracing::info!(host, %dir, "removed remote bot config dir");
            true
        }
        Err(e) => {
            tracing::warn!(host, bot = %bot_id, error = %format!("{e:#}"), "could not remove remote bot config dir");
            false
        }
    }
}

#[cfg(test)]
mod bot_dir_safety_tests {
    use super::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn invalid_ids_do_not_access_or_remove_local_bot_dirs() {
        let env = tt::env().await;
        let protected = env.app.data_dir.join("bots").join("keep");
        std::fs::create_dir_all(&protected).unwrap();

        for id in ["../..", "foo/bar", r"..\..", ""] {
            assert!(env.app.bot_dir(id).is_err(), "invalid id was accepted: {id:?}");
            purge_bot_dir(&env.app, id, LOCAL_HOST).await;
            assert!(protected.exists(), "purge touched the protected directory for {id:?}");
        }
    }
}

/// [`interrupt_turn`] 不綁哪一筆（測試用；API 走 `interrupt_turn`）。
#[cfg(test)]
pub async fn interrupt_bot(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    interrupt_turn(app, bot_id, None).await
}

/// 送 Esc 打斷這顆 bot 正在跑的回合。`expect_turn` 給了就只打斷那一筆：它已經不在飛（別的路收掉了、下一回合已經開始）就不按 Esc，
/// 回 409 `turn_not_in_flight`。重試上一次中斷時帶上它，才不會誤傷下一回合（#147）。
///
/// Esc 與「把回合收成 failed」是兩半（見 `interruption`）：Esc 沒進 pane 就什麼都不動；進了而 DB 寫不進去，
/// 回 503 `interrupt_state_uncommitted`（不是普通的成功），欠著的收尾之後補——同一筆的重試**不再按** Esc；
/// 不知道 Esc 進了沒有，回合留在 in_flight，等它的回聲。
pub async fn interrupt_turn(app: &Arc<App>, bot_id: &str, expect_turn: Option<&str>) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    // 上一次打斷欠著的收尾先補：補完之後還在飛的，才是真的還在跑的那一筆。
    let owed = super::interruption::owed_turn(bot_id);
    let settled = super::interruption::settle_locked(app, bot_id, super::interruption::Evidence::Nothing).await;
    let in_flight = db::in_flight_turn(&app.db, &run.id).await.map_err(up)?;
    // 欠著的那一筆剛剛補上（或已經被別的路收掉）：這一次就是上一次中斷的重試，中斷已經完成——不再按 Esc。
    // 沒指名哪一筆時，只有 run 上已經沒有別的在飛才算（#166）：欠著的帳不一定是 Esc 記的——插隊送出的帳一結清，
    // 新的那一則就掛上 run、claude 正在做它，這一次的 Esc 是要打斷它。
    if let Some(t) = owed.as_deref() {
        let done = settled.is_ok() && in_flight.as_ref().map(|x| x.id.as_str()) != Some(t);
        if done && (expect_turn == Some(t) || (expect_turn.is_none() && in_flight.is_none())) {
            return Ok(());
        }
    }
    if let Some(want) = expect_turn {
        if in_flight.as_ref().map(|t| t.id.as_str()) != Some(want) {
            return Err(LcError::conflict(
                "turn_not_in_flight",
                json!({"turn_id": want, "in_flight_turn_id": in_flight.as_ref().map(|t| t.id.clone()), "esc_sent": false}),
            ));
        }
    }
    // 這一筆的 Esc 已經生效、只是狀態還沒寫成：不再按（claude 閒著時連按兩次 Esc 會跳 rewind 選單）。
    if let Some(t) = in_flight.as_ref().filter(|t| super::interruption::owes(bot_id, &t.id)) {
        return Err(super::interruption::uncommitted(&run.id, &t.id, settled.as_ref().err()));
    }
    let target = db::run_target(&run, &bot);
    let client = client_for_run(app, &run).await?;
    // herdr 沒回時要拿它去 log 裡找這次 Esc 留下的中斷紀錄（#223）：取在按鍵**之前**。
    let esc_at = chrono::Utc::now();
    let sent = client.agent_send_keys(&target, &["esc".to_string()]).await;
    let fate = super::interruption::key_fate(&sent);
    if fate == super::interruption::KeyFate::NotApplied {
        // herdr 沒收下：什麼都沒發生，回合照舊在飛。
        return Err(up(sent.err().map(|e| format!("{e:#}")).unwrap_or_default()));
    }
    // 先記接管，再收 in-flight：收掉會 emit turn → 觸發 flush，順序反過來排隊的派工就搶進去了。
    // 同時記下被中斷的是哪一回合：它的 `StopFailure` 回聲才認得出來，新回合的失敗不會被當成回聲（#117）。
    note_user_interrupt_of(app, &bot, &run, in_flight.as_ref()).await;
    if fate == super::interruption::KeyFate::Unknown {
        // 不知道 Esc 進了沒有：不假定打斷。claude 2.1.276～2.1.278 按 Esc 不送任何 hook（#223），回聲等不到——
        // 趁還握著鎖先看 log 裡有沒有這次的中斷紀錄（§4.3 備援等的是同一把鎖）；還看不到就留在 in_flight 等證據。
        if let Some(t) = in_flight.as_ref() {
            match super::interruption::unconfirmed(app, bot_id, &run.id, &t.id, super::interruption::INTERRUPT_NOTE, esc_at).await {
                Ok(false) => {}
                Ok(true) => {
                    clear_restored_prompt(&client, &run, &bot).await;
                    return Ok(());
                }
                Err(e) => {
                    clear_restored_prompt(&client, &run, &bot).await;
                    return Err(super::interruption::uncommitted(&run.id, &t.id, Some(&e)));
                }
            }
        }
        return Err(LcError::conflict(
            "interrupt_unconfirmed",
            json!({"run_id": run.id, "turn_id": in_flight.as_ref().map(|t| t.id.clone()), "esc_sent": "unknown", "retryable": true,
                   "error": sent.err().map(|e| format!("{e:#}")),
                   "message": "Esc 送出去了但 herdr 沒有回，不知道進了沒有；回合先不收，等 log 裡出現中斷紀錄（或它的回聲）、或它自己結束。"}),
        ));
    }
    if let Some(t) = in_flight.as_ref() {
        if let Err(e) = super::interruption::interrupted(app, bot_id, &run.id, &t.id, super::interruption::INTERRUPT_NOTE).await {
            // Esc 確實進去了：框照樣要清。
            clear_restored_prompt(&client, &run, &bot).await;
            return Err(super::interruption::uncommitted(&run.id, &t.id, Some(&e)));
        }
    }
    clear_restored_prompt(&client, &run, &bot).await;
    Ok(())
}

/// claude 在吐出第一個字前被 `esc` 打斷，會把 prompt 放回輸入框（2026-09-08 實測），下一則
/// 貼上會黏在後面。中斷後 composer 有字就 `ctrl+c` 清掉（有字時只清不退出）。只做 claude，失敗不報錯。
async fn clear_restored_prompt(client: &HerdrClient, run: &db::Run, bot: &db::Bot) {
    if bot.kind != "claude" {
        return;
    }
    let Some(pane) = run.pane_id.as_deref() else { return };
    tokio::time::sleep(Duration::from_millis(400)).await;
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    if composer_text(&bot.kind, &read.text).is_none() {
        return;
    }
    match client.pane_send_keys(pane, &["ctrl+c"]).await {
        Ok(()) => tracing::info!(run = %run.id, "interrupt put the prompt back into the composer; cleared it"),
        Err(e) => tracing::warn!(run = %run.id, error = ?e, "could not clear the restored prompt from the composer"),
    }
}

/// 強制結束目前回合（`POST /api/bots/:id/abort`）。和 [`interrupt_turn`] 相反，先保證 DB 解開、
/// 送 `esc` 只是盡力（`keys_sent`）：in-flight 標 failed、`delivery = unknown` 也一併收（§6.3，
/// 同樣鎖輸入框）。沒有 active run 不算錯——那正是最需要這支的情況。
pub async fn abort_turns(app: &Arc<App>, bot_id: &str) -> LcResult<Value> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?;
    // 上一次打斷欠著的先補（#147）；補不上也照樣往下收——強制中止本來就是先保證 DB 解開。
    if let Err(e) = super::interruption::settle_locked(app, bot_id, super::interruption::Evidence::Nothing).await {
        tracing::warn!(bot = %bot.name, error = %e, "上一次打斷欠著的收尾還是寫不進去");
    }

    // 在飛的那一筆在按 esc **之前**讀（#208）：讀不到就一步都不做。以前放在 esc 之後、讀錯當成「沒有」——esc 已經送出去，
    // 打斷的紀錄（`note_user_interrupt_of`、`interrupted`）卻沒記，那一回合的 StopFailure 回聲之後會被當成真的失敗。
    let in_flight = match run.as_ref() {
        Some(r) => db::in_flight_turn(&app.db, &r.id).await.map_err(up)?,
        None => None,
    };

    let mut keys_sent = false;
    let mut key_error: Option<String> = None;
    let mut client: Option<HerdrClient> = None;
    if let Some(r) = run.as_ref() {
        let target = db::run_target(r, &bot);
        match client_for_run(app, r).await {
            Ok(c) => match c.agent_send_keys(&target, &["esc".to_string()]).await {
                Ok(()) => {
                    keys_sent = true;
                    client = Some(c);
                }
                Err(e) => key_error = Some(format!("{e:#}")),
            },
            Err(e) => key_error = Some(format!("{e:?}")),
        }
    }

    let mut aborted: Vec<String> = Vec::new();
    if let Some(r) = run.as_ref() {
        if let Some(t) = &in_flight {
            aborted.push(t.id.clone());
        }
        // 強制中止也是使用者要接手：排著的派工照樣不撤，只是先讓使用者拿回輸入框（§4.4a）。
        note_user_interrupt_of(app, &bot, r, in_flight.as_ref()).await;
        // 收不掉就不是成功（#147）：以前 `fail_in_flight` 把錯吞掉，下面的迴圈又因為它在 `aborted` 裡而跳過它，
        // 回 200 `aborted:[它]`、它卻還在飛。記成欠著，之後補。
        if let Some(t) = &in_flight {
            if let Err(e) = super::interruption::interrupted(app, bot_id, &r.id, &t.id, "回合已由使用者強制中止").await {
                return Err(LcError::Upstream(format!("回合 {} 沒收成（稍後自動補上）：{e:#}", t.id)));
            }
        }
        if let Some(c) = client.as_ref() {
            clear_restored_prompt(c, r, &bot).await;
        }
    }
    // Unknown-delivery turns live on the bot, not a run: a stopped run can leave one behind.
    let unknown = sqlx::query_as::<_, db::Turn>(
        "SELECT t.* FROM turns t JOIN conversations c ON c.id = t.conversation_id
         WHERE c.bot_id = ? AND (t.status = 'in_flight' OR t.delivery = 'unknown')",
    )
    .bind(bot_id)
    .fetch_all(&app.db)
    .await
    .map_err(up)?;
    for t in unknown {
        if aborted.contains(&t.id) && t.delivery != "unknown" {
            continue;
        }
        // 上面那句 SELECT 撈的是「in_flight **或** delivery='unknown'」，所以這裡拿到的不一定還在飛：
        // §4.3 的備援關掉的回合（`completed_fallback`）不會動 `delivery`，所以它可以是已經收好、
        // 但 `delivery` 還停在 `unknown` 的狀態。那種的**只清掉 `unknown` 這個停車位**（它是擋住下一則
        // prompt 的東西，見 `prompt_inner` 的前置檢查），不改它的 status——回合已經收好了，
        // 把它改寫成 failed 會讓使用者看到一筆「失敗」的回合，而它其實答完了。
        // （`turn_controller` 的轉移表也沒有 `completed_fallback -> failed` 這條邊，issue #68。）
        // 收掉與說明同一個交易（#208，同 `fail_in_flight`）：以前說明寫不進去被 `let _` 吞掉，回合收了、對話裡卻沒有一句話。
        let mut tx = app.db.begin().await.map_err(up)?;
        if t.status == "in_flight" {
            super::turn_controller::fail_on(&mut tx, &t.id, super::turn_controller::DeliveryOnFail::FailedIfUnknown, "使用者強制中止")
                .await
                .map_err(up)?;
        } else if t.delivery == "unknown" {
            sqlx::query("UPDATE turns SET delivery='failed' WHERE id=? AND delivery='unknown'")
                .bind(&t.id)
                .execute(&mut *tx)
                .await
                .map_err(up)?;
        }
        let note = if aborted.contains(&t.id) {
            None
        } else {
            Some(insert_message_tx(&mut tx, &t.conversation_id, Some(&t.id), "system", "回合已由使用者強制中止", "system", false, None).await.map_err(up)?)
        };
        tx.commit().await.map_err(up)?;
        if let Some(m) = note {
            emit_message_added(app, bot_id, m).await;
            aborted.push(t.id.clone());
        }
        emit_turn(app, &t.id).await;
    }

    tracing::info!(bot = %bot.name, keys_sent, aborted = aborted.len(), "turn(s) force-aborted by user");
    Ok(json!({"aborted": aborted, "keys_sent": keys_sent, "key_error": key_error}))
}

/// Give a running pre one-bot-one-tab bot its own tab. A move, not a restart: herdr keeps
/// `pane_id` across `pane.move` (0.8.2), so mapping, poller and in-flight turn carry on.
/// Idempotent: re-moving a solo pane would rebuild a tab and renumber the user's tab bar.
pub async fn move_pane_to_own_tab(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let pane_id = run
        .pane_id
        .clone()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| LcError::NotFound("pane".into()))?;
    let client = client_for_run(app, &run).await?;

    // `runs.tab_id` is NULL for old runs and stale if the user dragged the pane.
    let pane = client.pane_get(&pane_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("pane".into()))?;
    let workspace_id = pane.workspace_id.clone();
    let current_tab = pane.tab_id.clone();

    let solo = client
        .tab_list(&workspace_id)
        .await
        .map_err(up)?
        .into_iter()
        .find(|t| t.tab_id == current_tab)
        .map(|t| t.pane_count <= 1)
        .unwrap_or(false);
    let tab_id = if solo {
        current_tab
    } else {
        let (new_tab, previous) = client.pane_move_to_new_tab(&pane_id, &tab_label(&bot)).await.map_err(up)?;
        // The shared tidy-up is idempotent and the one place that decides a tab may go.
        if !previous.is_empty() && previous != new_tab {
            close_tab_if_empty(&client, &workspace_id, &previous).await;
        }
        new_tab
    };

    sqlx::query("UPDATE runs SET workspace_id = ?, tab_id = ? WHERE id = ?")
        .bind(&workspace_id)
        .bind(&tab_id)
        .bind(&run.id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    app.emit_bot_status(bot_id).await;
    Ok(())
}

#[cfg(test)]
mod abort_tests {
    use super::*;

    /// `abort_turns` must work when `esc` cannot be delivered (no herdr here): the turn still leaves `in_flight`.
    #[tokio::test]
    async fn unlocks_even_when_the_keys_cannot_be_sent() {
        let dir = std::env::temp_dir().join(format!("am-abort-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
        let app = crate::state::App::new(
            pool,
            client.clone(),
            client,
            cfg,
            dir.clone(),
            dir.join("agents-managerd"),
            7799,
            "t".into(),
            "test".into(),
            false,
        );

        let (pid, bid, rid, cid) = (db::ulid(), db::ulid(), db::ulid(), db::ulid());
        let now = db::now();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,'local',?)")
            .bind(&pid).bind("/tmp/p").bind("p").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?,?,?,?)")
            .bind(&bid).bind(&pid).bind("b").bind("claude").bind("tok").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id, bot_id, state, pane_id, started_at) VALUES (?,?,'running','w1:p1',?)")
            .bind(&rid).bind(&bid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?)")
            .bind(&cid).bind(&bid).bind(&now).execute(&app.db).await.unwrap();
        // Both an in-flight turn and an unknown-delivery one block the next prompt.
        let (t_flight, t_unknown) = (db::ulid(), db::ulid());
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&t_flight).bind(&cid).bind(&rid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','failed','unknown',?)")
            .bind(&t_unknown).bind(&cid).bind(&rid).bind(&now).execute(&app.db).await.unwrap();
        // §4.3 的備援關掉的回合不會動 `delivery`：這種「已經收好、但 delivery 停在 unknown」的
        // 一樣擋住下一則 prompt，也一樣要被解開——但它的 status 不可以被改寫成 failed
        // （回合其實答完了，而且 `completed_fallback -> failed` 不是合法邊，issue #68）。
        let t_fallback = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, completed_at, created_at) VALUES (?,?,?,'web','completed_fallback','unknown',?,?)")
            .bind(&t_fallback).bind(&cid).bind(&rid).bind(&now).bind(&now).execute(&app.db).await.unwrap();

        let out = abort_turns(&app, &bid).await.expect("abort must not fail just because the keys did");
        assert_eq!(out["keys_sent"], false, "no herdr behind the socket");
        let aborted = out["aborted"].as_array().unwrap();
        assert_eq!(aborted.len(), 3, "in-flight、unknown-delivery 與備援關掉的那一筆: {out}");
        let fallback: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_fallback).fetch_one(&app.db).await.unwrap();
        assert_eq!(
            fallback,
            ("completed_fallback".into(), "failed".into()),
            "只解開擋住下一則 prompt 的 unknown，不把一筆已經收好的回合改寫成 failed",
        );

        let flight: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_flight).fetch_one(&app.db).await.unwrap();
        assert_eq!(flight, ("failed".into(), "ok".into()), "in-flight turn is closed, delivery untouched");
        let unknown: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_unknown).fetch_one(&app.db).await.unwrap();
        assert_eq!(unknown, ("failed".into(), "failed".into()), "unknown delivery is resolved, not left to block");
        assert!(db::in_flight_turn(&app.db, &rid).await.unwrap().is_none(), "nothing left in flight");

        // Idempotent: nothing to abort the second time round.
        let again = abort_turns(&app, &bid).await.unwrap();
        assert_eq!(again["aborted"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod default_session_tests {
    //! SPEC §6.5.1: a run in the user's `default` session is observed; its pane is never closed
    //! or re-created (review 2026-09-12 #4).
    use super::*;
    use crate::testing as tt;

    async fn imported_bot(env: &tt::Env) -> (String, String, crate::herdr::PaneInfo) {
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "mine", json!({})).await.unwrap();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, herdr_session, created_at)
             VALUES (?,?,'mine','claude','[]',0,0,'tok','default',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,'mine','default',1,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "mine", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];
        (bot_id, run_id, pane)
    }

    /// Stop sends ctrl+c and ends the run, but the user's pane and tab stay exactly as they were.
    #[tokio::test]
    async fn stop_never_closes_the_users_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, pane) = imported_bot(&env).await;

        assert!(stop_bot(&app, &bot_id).await.unwrap());

        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, "stopped");
        let methods = env.herdr.methods();
        assert!(methods.iter().any(|m| m == "agent.send_keys"), "the agent was asked to exit");
        assert!(!methods.iter().any(|m| m == "pane.close" || m == "tab.close"), "{methods:?}");
        assert!(env.herdr.tab(&pane.tab_id).unwrap().panes.contains(&pane.pane_id), "the pane is still there");
    }

    /// Start / restart refused with a UI reason, before any ctrl+c.
    #[tokio::test]
    async fn start_and_restart_are_refused_before_touching_the_agent() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, _pane) = imported_bot(&env).await;

        let reason = |e: LcError| match e {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
            other => panic!("expected 409, got {other:?}"),
        };
        assert_eq!(reason(restart_bot(&app, &bot_id).await.unwrap_err()), "default_session");
        assert_eq!(reason(restart_bot_with(&app, &bot_id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap_err()), "default_session");
        assert_eq!(db::active_run(&app.db, &bot_id).await.unwrap().map(|r| r.id), Some(run_id.clone()), "still running");
        assert!(!env.herdr.methods().iter().any(|m| m == "agent.send_keys"), "no ctrl+c was sent");

        // With no run at all, `start` is what the sidebar button would call.
        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
            .bind(db::now())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        assert_eq!(reason(start_bot(&app, &bot_id).await.unwrap_err()), "default_session");
        let creates = env.herdr.methods().iter().filter(|m| *m == "workspace.create").count();
        assert_eq!(creates, 1, "only the fixture's own workspace.create; the daemon made none in the user's session");
    }
}


#[cfg(test)]
mod stop_commit_tests {
    //! #146：`stopping`／`stopped` 寫不進去時，stop（與子 agent 原地重啟）不能照做破壞性的副作用、不能回成功。
    use super::super::run_state as rs;
    use super::*;
    use crate::testing as tt;

    fn since(env: &tt::Env, n: usize) -> Vec<String> {
        env.herdr.methods().into_iter().skip(n).collect()
    }

    async fn state(app: &Arc<App>, run: &str) -> String {
        db::run(&app.db, run).await.unwrap().unwrap().state
    }

    /// 一顆在跑的 bot：自己的 tab、pane、有名字的 agent（mock 收到 ctrl+c 就讓它離開）。不走 `start_bot`：
    /// 遠端編譯機沒裝 claude，preflight 會擋（#139）。
    async fn running_bot(env: &tt::Env) -> (String, String) {
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'proj-alfa','test',?)",
        )
        .bind(&run)
        .bind(&bot.id)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        env.herdr.agents.lock().unwrap().push(json!({
            "name": "proj-alfa", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"}));
        (bot.id, run)
    }

    /// 驗收 1：`stopping` 記不下來 → 一步都不做：不收 in-flight、不送 ctrl+c、不關 pane、不撤佇列。
    #[tokio::test]
    async fn a_stop_that_cannot_record_stopping_does_nothing_to_the_agent() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let in_flight = rs::a_turn(&app, &bot, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot, None, "queued").await;
        let watcher = rs::watch_run_pane(&app, &run).await;
        rs::refuse_run_state(&app, "stopping").await;
        let n = env.herdr.methods().len();

        let err = stop_bot(&app, &bot).await.expect_err("沒記下「正在停」就不是在停");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued");
        assert!(rs::watched(&app, &watcher).await);
    }

    /// 驗收 2：pane 真的關了，`stopped` 卻寫不進去——不回一般的成功；終態還沒成立，佇列與 watcher 先不動。
    /// DB 恢復後補記 `stopped`（使用者要它停），收尾照常、只做一次。
    #[tokio::test]
    async fn a_stop_whose_stopped_state_cannot_be_recorded_is_not_reported_as_done() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let pane = db::run(&app.db, &run).await.unwrap().unwrap().pane_id.unwrap();
        let queued = rs::a_turn(&app, &bot, None, "queued").await;
        let watcher = rs::watch_run_pane(&app, &run).await;
        rs::refuse_run_state(&app, "stopped").await;

        match stop_bot(&app, &bot).await {
            Err(LcError::Uncommitted(v)) => assert_eq!((v["error"].as_str(), v["run_id"].as_str()), (Some("stop_state_uncommitted"), Some(run.as_str()))),
            other => panic!("DB 沒記下 stopped，要 503 stop_state_uncommitted，拿到 {other:?}"),
        }
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        assert!(client.pane_get(&pane).await.unwrap().is_none(), "pane 真的關了");
        assert_eq!(state(&app, &run).await, "stopping");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "終態還沒寫進去，佇列先不撤");
        assert!(rs::watched(&app, &watcher).await);
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::FinishStop], "排了補記的重試");

        rs::accept_run_state(&app, "stopped").await;
        assert!(rs::settle_once(&app, &run, &rs::Settle::FinishStop).await);
        assert_eq!(state(&app, &run).await, "stopped", "使用者要它停");
        assert_eq!(rs::turn_status(&app, &queued).await, "failed");
        assert_eq!(rs::system_notes(&app, &queued).await, 1);
        assert!(!rs::watched(&app, &watcher).await);
    }

    /// 補記 `stopped` 的重試（`FinishStop`）讀不到主機：這一輪什麼都不寫——收尾要拆那台主機上的 watcher，以前退回 local，
    /// 遠端那台的 watcher 就留著。讀得到之後照常補記、收尾。
    #[tokio::test]
    async fn a_finish_stop_retry_that_cannot_read_the_host_records_nothing_yet() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let watcher = rs::watch_run_pane(&app, &run).await;
        rs::refuse_run_state(&app, "stopped").await;
        assert!(stop_bot(&app, &bot).await.is_err());
        rs::accept_run_state(&app, "stopped").await;

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        assert!(!rs::settle_once(&app, &run, &rs::Settle::FinishStop).await, "讀不到主機：還沒補上");
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
        assert_eq!(state(&app, &run).await, "stopping", "一步都沒寫");
        assert!(rs::watched(&app, &watcher).await);

        assert!(rs::settle_once(&app, &run, &rs::Settle::FinishStop).await);
        assert_eq!(state(&app, &run).await, "stopped");
        assert!(!rs::watched(&app, &watcher).await, "拆的是那台主機上的 watcher");
    }

    /// #208：遠端 bot 的 `FinishStop` 補記時讀不到主機。以前退回 local 去拆 watcher——拆掉的是本機同名 session、同 pane id
    /// 那顆 bot 的（它從此收不到狀態事件、回合收不掉），遠端該拆的反而留著。現在這一輪什麼都不寫；讀得到才補記、拆遠端那一個。
    #[tokio::test]
    async fn a_finish_stop_that_cannot_read_the_host_never_unwatches_a_local_pane_of_the_same_name() {
        let env = tt::env().await;
        let app = env.app.clone();
        let remote = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'remote1', ?)")
            .bind(&remote)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let far = tt::claude_bot(&app, &remote, "far").await;
        let near = tt::claude_bot(&app, &env.project_id, "near").await;
        let far_run = tt::fake_run(&app, &far.id).await;
        let near_run = tt::fake_run(&app, &near.id).await;
        sqlx::query("UPDATE runs SET pane_id='p-same' WHERE id IN (?, ?)").bind(&far_run).bind(&near_run).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&far_run).execute(&app.db).await.unwrap();
        let key = |host: &str| (host.to_string(), "test".to_string(), "p-same".to_string());
        {
            let mut w = app.pane_watchers.lock().await;
            w.insert(key(LOCAL_HOST), tokio::spawn(std::future::pending::<()>()));
            w.insert(key("remote1"), tokio::spawn(std::future::pending::<()>()));
        }

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        assert!(!rs::settle_once(&app, &far_run, &rs::Settle::FinishStop).await, "讀不到主機：這一輪不補");
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
        assert_eq!(state(&app, &far_run).await, "stopping");
        assert!(rs::watched(&app, &key(LOCAL_HOST)).await && rs::watched(&app, &key("remote1")).await, "兩個 watcher 都沒動");

        assert!(rs::settle_once(&app, &far_run, &rs::Settle::FinishStop).await);
        assert_eq!(state(&app, &far_run).await, "stopped");
        assert!(!rs::watched(&app, &key("remote1")).await, "拆的是遠端那一個");
        assert!(rs::watched(&app, &key(LOCAL_HOST)).await, "本機同名的那顆照舊收得到狀態事件");
    }

    /// #208（同檔同類）：強制中止讀不到在飛的那一筆——以前 esc 已經送出去了，讀錯又當成「沒有」，打斷的紀錄沒記，
    /// 那一回合的 StopFailure 回聲之後會被當成真的失敗。現在先讀再按：讀不到就一步都不做。
    #[tokio::test]
    async fn a_force_abort_that_cannot_read_the_in_flight_turn_sends_no_esc() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        rs::a_turn(&app, &bot, Some(&run), "in_flight").await;
        let n = env.herdr.methods().len();
        sqlx::query("ALTER TABLE turns RENAME TO turns_unreadable").execute(&app.db).await.unwrap();
        assert!(abort_turns(&app, &bot).await.is_err());
        sqlx::query("ALTER TABLE turns_unreadable RENAME TO turns").execute(&app.db).await.unwrap();
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys"), "讀不到就不按 esc：{calls:?}");
    }

    /// #208（同檔同類）：強制中止收掉 `delivery = unknown` 的那一筆，說明寫不進去——以前 `let _` 吞掉，回合解開了、對話裡卻沒有
    /// 一句話，API 照回 `aborted`。現在收掉與說明同一個交易：寫不進去就兩個都沒發生、回錯；寫得進去再按一次，一起成立。
    #[tokio::test]
    async fn a_force_abort_whose_note_cannot_be_written_changes_nothing() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "unknown").await;
        let turn = rs::a_turn(&app, &bot.id, None, "failed").await;
        sqlx::query("UPDATE turns SET delivery='unknown' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        sqlx::query(&format!(
            "CREATE TRIGGER refuse_abort_note BEFORE INSERT ON messages WHEN NEW.turn_id = '{turn}' BEGIN SELECT RAISE(ABORT, 'injected'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let delivery = || {
            let app = app.clone();
            let turn = turn.clone();
            async move { sqlx::query_scalar::<_, String>("SELECT delivery FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap() }
        };

        assert!(abort_turns(&app, &bot.id).await.is_err(), "說明寫不進去：不回 aborted");
        assert_eq!(delivery().await, "unknown", "解開也沒發生");
        assert_eq!(rs::system_notes(&app, &turn).await, 0);

        sqlx::query("DROP TRIGGER refuse_abort_note").execute(&app.db).await.unwrap();
        let out = abort_turns(&app, &bot.id).await.unwrap();
        assert_eq!(out["aborted"], json!([turn.clone()]));
        assert_eq!((delivery().await, rs::system_notes(&app, &turn).await), ("failed".to_string(), 1));
    }

    /// 驗收 2 的另一半：重試還沒補上 daemon 就重啟了（排的重試跟著沒了）——開機的對帳照證據把它收掉，
    /// 不會留一顆擋住 start 的 `stopping`。
    #[tokio::test]
    async fn a_stop_left_in_stopping_is_settled_by_the_reconcile_after_a_restart() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        rs::refuse_run_state(&app, "stopped").await;
        assert!(stop_bot(&app, &bot).await.is_err());
        rs::accept_run_state(&app, "stopped").await;

        crate::reconcile::reconcile_host(&app, LOCAL_HOST).await.unwrap();
        assert!(db::active_run(&app.db, &bot).await.unwrap().is_none(), "不再擋住下一次 start：{}", state(&app, &run).await);
    }

    /// 驗收 3：default session 的 pane 不能強制關；agent 對 ctrl+c 沒反應、還活著，就不能記成 `stopped`。
    #[tokio::test]
    async fn a_default_session_agent_that_ignores_ctrl_c_is_not_recorded_as_stopped() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "mine", json!({})).await.unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "mine").await.id;
        sqlx::query("UPDATE bots SET herdr_session='default' WHERE id=?").bind(&bot).execute(&app.db).await.unwrap();
        let run = db::ulid();
        // herdr 的目標寫成 pane id：mock 只照名字在 ctrl+c 時移除 agent，這顆就像不理 ctrl+c 的 agent。
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'default',1,?)",
        )
        .bind(&run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(&pane.pane_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "mine", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];
        let queued = rs::a_turn(&app, &bot, None, "queued").await;

        let err = stop_bot(&app, &bot).await.expect_err("agent 還活著，不是停好了");
        assert!(matches!(&err, LcError::Upstream(m) if m.starts_with("stop_not_confirmed")), "{err:?}");
        assert_eq!(state(&app, &run).await, "running", "agent 還在，run 放回 running");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued");
        assert!(!env.herdr.methods().iter().any(|m| m == "pane.close"), "使用者的 pane 照舊不關");
    }

    /// 驗收 4：讀完 active run 之後，不拿 bot 鎖的 pane-exit 事件先把它收成 `exited`——stop 輸了 CAS，
    /// 不再對它送 ctrl+c／關 pane，也不把它拉回 `stopping`。
    #[tokio::test]
    async fn a_stop_that_finds_its_run_already_ended_does_not_touch_the_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let (app2, run2) = (app.clone(), run.clone());
        super::super::race_point::arm("stop_before_stopping", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });
        let n = env.herdr.methods().len();

        assert!(!stop_bot(&app, &bot).await.unwrap(), "沒有東西要停");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "exited", "終態不被拉回 stopping");
    }

    /// stop 自己關的 pane 觸發 herdr 的 pane_closed 事件，事件那邊的 `mark_run_exited` 搶在 stop 記
    /// `stopped` 之前寫了 `exited`：仍然記成使用者要的 `stopped`（autostart 的 bot 才不會被報 `bot_stopped`），
    /// 收尾不做第二份。
    #[tokio::test]
    async fn a_pane_exit_that_lands_during_a_stop_still_leaves_it_recorded_as_a_user_stop() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let queued = rs::a_turn(&app, &bot, None, "queued").await;
        let (app2, run2) = (app.clone(), run.clone());
        super::super::race_point::arm("stop_before_stopped", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });

        assert!(stop_bot(&app, &bot).await.unwrap());
        assert_eq!(state(&app, &run).await, "stopped");
        assert_eq!(rs::turn_status(&app, &queued).await, "failed");
        assert_eq!(rs::system_notes(&app, &queued).await, 1, "撤一次");
    }

    /// #146 重開 A：stop 自己關的 pane 觸發的 pane-exit 事件先寫了 `exited`，改標回 `stopped` 卻寫不進去。以前被當成 CAS
    /// 輸了：回一般的成功、不排重試，DB 永遠留 `exited`——autostart 的 bot 從此被 incident 探針報成「自己掛了」。現在回
    /// 503 `stop_state_uncommitted`、排 `FinishStop`；DB 恢復後改標成 `stopped`、探針不再報，stop 才有的撤回也在那時補上。
    #[tokio::test]
    async fn a_stop_whose_relabel_after_a_pane_exit_cannot_be_written_is_not_reported_as_done() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        sqlx::query("UPDATE bots SET autostart=1 WHERE id=?").bind(&bot).execute(&app.db).await.unwrap();
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(&app.db).await.unwrap();
        let (app2, run2) = (app.clone(), run.clone());
        super::super::race_point::arm("stop_before_stopped", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });
        rs::refuse_run_state(&app, "stopped").await;

        match stop_bot(&app, &bot).await {
            Err(LcError::Uncommitted(v)) => assert_eq!((v["error"].as_str(), v["run_id"].as_str()), (Some("stop_state_uncommitted"), Some(run.as_str()))),
            other => panic!("改標寫不進去不是停好了，要 503 stop_state_uncommitted，拿到 {other:?}"),
        }
        assert_eq!(state(&app, &run).await, "exited");
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::FinishStop], "排了補改標的重試");
        assert!(rs::bot_stopped_reported(&app, &bot).await, "前提：留著 exited，探針說它自己掛了");
        assert_eq!(rs::turn_status(&app, &waiting).await, "failed", "等它起來的那一則在記 `stopping` 時就一起撤了（#199）");

        rs::accept_run_state(&app, "stopped").await;
        assert!(rs::settle_once(&app, &run, &rs::Settle::FinishStop).await);
        assert_eq!(state(&app, &run).await, "stopped", "使用者要它停");
        assert!(!rs::bot_stopped_reported(&app, &bot).await, "故意停的 autostart bot 不是 outage");
        assert_eq!(rs::system_notes(&app, &waiting).await, 1, "只撤一次");
    }

    /// 重啟那一半不改標：重啟不是「使用者要它停」。pane-exit 先寫了 `exited` 就留著（開不回來時要的正是它），重啟照常往下走。
    #[tokio::test]
    async fn a_restart_whose_old_run_a_pane_exit_ended_keeps_it_exited() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let (app2, run2) = (app.clone(), run.clone());
        super::super::race_point::arm("stop_before_stopped", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });
        assert!(stop_for_restart_locked(&app, &bot).await.unwrap(), "pane 關了、舊 run 收掉了：重啟照常往下走");
        assert_eq!(state(&app, &run).await, "exited", "不改標成 stopped：沒開回來時探針才看得到");
        assert!(rs::scheduled(&run).is_empty(), "沒有要補的");
    }

    /// #199：使用者按停止時，等它起來才送的那一則撤不掉（DB 寫不進去）——撤回跟記 `stopping` 同一個交易，所以連停都不算開始：
    /// 回 502、run 照舊 running、不送 ctrl+c、不關 pane、那一則還在。寫得進去之後再按一次，兩個一起成立。
    #[tokio::test]
    async fn a_stop_whose_withdrawal_cannot_be_written_does_nothing_at_all() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(&app.db).await.unwrap();
        rs::refuse_turn_close(&app, &waiting).await;
        let n = env.herdr.methods().len();

        let err = stop_bot(&app, &bot).await.expect_err("撤回寫不進去：不算開始停");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "{calls:?}");
        assert_eq!((state(&app, &run).await, rs::turn_status(&app, &waiting).await), ("running".to_string(), "queued".to_string()), "兩個都沒成立");

        rs::accept_turn_close(&app).await;
        assert!(stop_bot(&app, &bot).await.unwrap());
        assert_eq!((state(&app, &run).await, rs::turn_status(&app, &waiting).await), ("stopped".to_string(), "failed".to_string()));
        assert_eq!(rs::system_notes(&app, &waiting).await, 1);
    }

    /// #199 驗收 2：撤回跟著 `stopping` 落地之後，`stopped` 寫不進去、重試補上之前 daemon 就重啟——開機對帳把那顆收成 `exited`，
    /// 等它起來的那一則已經撤了，開機不替它啟動、永遠不送。（撤回若放在記 `stopped` 那一步，這裡就會被開機再啟動、送出去。）
    #[tokio::test]
    async fn a_withdrawal_made_with_stopping_survives_a_restart_before_stopped_is_recorded() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(&app.db).await.unwrap();
        rs::refuse_run_state(&app, "stopped").await;
        assert!(matches!(stop_bot(&app, &bot).await, Err(LcError::Uncommitted(_))));
        assert_eq!((state(&app, &run).await, rs::turn_status(&app, &waiting).await), ("stopping".to_string(), "failed".to_string()));
        rs::accept_run_state(&app, "stopped").await;

        let fresh = tt::restart_app(&env).await;
        crate::reconcile::reconcile_host(&fresh, LOCAL_HOST).await.unwrap();
        assert!(db::active_run(&fresh.db, &bot).await.unwrap().is_none(), "開機對帳收掉了卡在 stopping 的 run");
        assert_eq!(super::super::start_send::resume_after_boot(&fresh, LOCAL_HOST).await, 0, "沒有在等它起來的：不替它啟動");
        assert_eq!(rs::turn_status(&fresh, &waiting).await, "failed");
    }

    /// 重啟那一半的 stop 不撤等它起來的訊息：那段沒有 run 是暫時的（`restart_hold`），重啟不是「不要了」。
    #[tokio::test]
    async fn a_restart_does_not_withdraw_what_waits_for_the_start() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _run) = running_bot(&env).await;
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(&app.db).await.unwrap();
        let restarting = super::super::restart_hold::begin(&bot);
        assert!(stop_for_restart_locked(&app, &bot).await.unwrap());
        drop(restarting);
        assert_eq!(rs::turn_status(&app, &waiting).await, "queued", "重啟不是不要了");
    }

    /// 同一個競態，排著的是「bot 沒在跑時送、等它起來」的那一種（#122 的 `awaits_start`）：撤孤兒那一支刻意不撤它，
    /// 只有使用者的 stop 會撤——改標成 `stopped` 的這條路也要補上，不留到下次啟動又送出去。
    #[tokio::test]
    async fn a_user_stop_that_raced_a_pane_exit_still_withdraws_what_waited_for_the_start() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let waiting = rs::a_turn(&app, &bot, None, "queued").await;
        sqlx::query("UPDATE turns SET awaits_start=1 WHERE id=?").bind(&waiting).execute(&app.db).await.unwrap();
        let (app2, run2) = (app.clone(), run.clone());
        super::super::race_point::arm("stop_before_stopped", &bot, move || async move {
            mark_run_exited(&app2, &run2, "pane exited").await;
        });

        assert!(stop_bot(&app, &bot).await.unwrap());
        assert_eq!(state(&app, &run).await, "stopped");
        assert_eq!(rs::turn_status(&app, &waiting).await, "failed", "使用者停了，等它起來的那一則撤回");
        assert_eq!(rs::system_notes(&app, &waiting).await, 1);
    }

    /// 子 agent：父開的 pane 裡一顆名字叫 `proj-alfa-ui` 的 agent，run 在跑。
    async fn a_child(env: &tt::Env) -> (String, String, crate::herdr::PaneInfo) {
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let parent = tt::claude_bot(&app, &env.project_id, "alfa").await;
        let kid = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
             VALUES (?,?,'ui','claude','[]',0,0,'tok','child',?,?)",
        )
        .bind(&kid)
        .bind(&env.project_id)
        .bind(&parent.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,'proj-alfa-ui','test',1,?)",
        )
        .bind(&run)
        .bind(&kid)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "proj-alfa-ui", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];
        (kid, run, pane)
    }

    /// #156：in-flight 那一筆收不成（寫不進去）就不停——這時 agent 還沒被打斷、那一回合真的還在跑。不送 ctrl+c、
    /// 不關 pane、不撤佇列，run 放回 running，回 503（不是普通的成功，也不假裝回合收掉了）。DB 好了再按一次照常停，
    /// 收尾各只做一次。
    #[tokio::test]
    async fn a_stop_whose_in_flight_turn_cannot_be_closed_does_not_touch_the_agent() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = running_bot(&env).await;
        let in_flight = rs::a_turn(&app, &bot, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot, None, "queued").await;
        let watcher = rs::watch_run_pane(&app, &run).await;
        rs::refuse_turn_close(&app, &in_flight).await;
        let n = env.herdr.methods().len();

        let err = stop_bot(&app, &bot).await.expect_err("回合收不成就沒有停");
        let LcError::Unavailable(body) = &err else { panic!("要 503、可重試：{err:?}") };
        assert_eq!(body["error"], "turn_state_unwritable", "{body}");
        assert_eq!(body["turn_id"], in_flight.as_str(), "{body}");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "running", "沒停就還是 running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight");
        assert_eq!(rs::system_notes(&app, &in_flight).await, 0);
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "沒停就不撤");
        assert!(rs::watched(&app, &watcher).await);

        rs::accept_turn_close(&app).await;
        assert!(stop_bot(&app, &bot).await.expect("DB 好了照常停"));
        assert_eq!(state(&app, &run).await, "stopped");
        assert_eq!((rs::turn_status(&app, &in_flight).await, rs::system_notes(&app, &in_flight).await), ("failed".to_string(), 1));
        assert_eq!(rs::turn_status(&app, &queued).await, "failed");
    }

    /// #156：子 agent 原地重啟一樣——舊 run 的 in-flight 收不成就不送 ctrl+c、不重開，run 放回 running。
    #[tokio::test]
    async fn a_child_restart_whose_in_flight_turn_cannot_be_closed_does_not_touch_the_child() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, run, _pane) = a_child(&env).await;
        let in_flight = rs::a_turn(&app, &kid, Some(&run), "in_flight").await;
        rs::refuse_turn_close(&app, &in_flight).await;
        let n = env.herdr.methods().len();

        let err = restart_child_in_pane(&app, &kid).await.expect_err("回合收不成就不重啟");
        let LcError::Unavailable(body) = &err else { panic!("要 503、可重試：{err:?}") };
        assert_eq!(body["error"], "turn_state_unwritable", "{body}");
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "agent.start"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight");
    }

    /// #146 留言：子 agent 原地重啟的舊 run 走同一套——`stopping` 記不下來就不收 in-flight、不送 ctrl+c。
    #[tokio::test]
    async fn a_child_restart_that_cannot_record_stopping_does_not_touch_the_child() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, run, _pane) = a_child(&env).await;
        let in_flight = rs::a_turn(&app, &kid, Some(&run), "in_flight").await;
        rs::refuse_run_state(&app, "stopping").await;
        let n = env.herdr.methods().len();

        assert!(restart_child_in_pane(&app, &kid).await.is_err());
        let calls = since(&env, n);
        assert!(!calls.iter().any(|m| m == "agent.send_keys" || m == "agent.start"), "{calls:?}");
        assert_eq!(state(&app, &run).await, "running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight");
    }

    /// #146 留言：舊 run 的 `stopped` 沒寫進去之前不能寫新 run、不能 `agent.start`——不是靠 active-run 唯一索引
    /// 碰巧擋住。失敗照 #129：沒開回來的重啟放掉 `restart_hold`，舊 run 收成終態之後排著的派工才照舊撤。
    #[tokio::test]
    async fn a_child_restart_starts_no_replacement_before_the_old_run_is_recorded_stopped() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, run, _pane) = a_child(&env).await;
        let queued = rs::a_turn(&app, &kid, None, "queued").await;
        rs::refuse_run_state(&app, "stopped").await;

        let err = restart_child_in_pane(&app, &kid).await.expect_err("舊 run 沒記下 stopped");
        assert!(matches!(&err, LcError::Uncommitted(v) if v["error"] == "stop_state_uncommitted"), "明確擋下，不是撞唯一索引：{err:?}");
        assert!(!env.herdr.methods().iter().any(|m| m == "agent.start"));
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&kid).fetch_one(&app.db).await.unwrap();
        assert_eq!(runs, 1, "沒有寫新 run");
        assert_eq!(state(&app, &run).await, "stopping");
        assert!(!super::super::restart_hold::in_progress(&kid), "重啟結束了");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "舊 run 還沒成終態，不撤");
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::Reconcile { stuck: "stopping".into() }]);

        rs::accept_run_state(&app, "stopped").await;
        assert!(rs::settle_once(&app, &run, &rs::Settle::Reconcile { stuck: "stopping".into() }).await);
        assert!(db::active_run(&app.db, &kid).await.unwrap().is_none());
        assert_eq!(rs::turn_status(&app, &queued).await, "failed", "沒開回來：照 #129 撤掉");
    }

    /// 子 agent 在原 pane 裡起來了，新 run 的 `running` 卻寫不進去：同 #145，不回成功、排重試。
    #[tokio::test]
    async fn a_child_restart_whose_new_run_cannot_be_recorded_running_is_not_reported_as_done() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, _run, _pane) = a_child(&env).await;
        rs::refuse_run_state(&app, "running").await;

        let err = restart_child_in_pane(&app, &kid).await.expect_err("新 run 沒記下 running");
        assert!(matches!(&err, LcError::Uncommitted(v) if v["error"] == "start_state_uncommitted"), "{err:?}");
        let fresh = db::active_run(&app.db, &kid).await.unwrap().expect("新 run 還在");
        assert_eq!(fresh.state, "starting");
        assert_eq!(rs::scheduled(&fresh.id), vec![rs::Settle::Reconcile { stuck: "starting".into() }]);
    }

    /// 同上，AGM 有一則排著的派工：重啟中不撤（#129），但對帳把新 run 收成 `running` 不會叫 flush，它起來時的 idle 邊又早在
    /// `starting` 就過了——要自己等它收斂再叫，不然那一則要等 30 分鐘的排隊保險絲把它撤掉。
    #[tokio::test]
    async fn a_child_restart_whose_new_run_cannot_be_recorded_running_still_flushes_the_queue_once_it_settles() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, _run, _pane) = a_child(&env).await;
        let queued = rs::a_turn(&app, &kid, None, "queued").await;
        rs::refuse_run_state(&app, "running").await;

        restart_child_in_pane(&app, &kid).await.expect_err("新 run 沒記下 running");
        let fresh = db::active_run(&app.db, &kid).await.unwrap().expect("新 run 還在");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "重啟中：不當孤兒撤");
        assert!(super::super::start_send::watching_for_running(&fresh.id), "排了「收成 running 就叫 flush」");
    }

    /// 子 agent 不理 ctrl+c（10 秒後放棄），`stopping → running` 放回去也寫不進去：排重試，DB 恢復後放回。
    #[tokio::test]
    async fn a_child_restart_that_gives_up_retries_putting_the_run_back() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (kid, run, _pane) = a_child(&env).await;
        // 名字清掉的 agent：mock 只照名字在 ctrl+c 時移除，這顆就像不理 ctrl+c。
        env.herdr.agents.lock().unwrap()[0]["name"] = serde_json::Value::Null;
        rs::refuse_run_state(&app, "running").await;

        assert!(restart_child_in_pane(&app, &kid).await.is_err());
        assert_eq!(state(&app, &run).await, "stopping");
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::BackToRunning]);

        rs::accept_run_state(&app, "running").await;
        assert!(rs::settle_once(&app, &run, &rs::Settle::BackToRunning).await);
        assert_eq!(state(&app, &run).await, "running");
    }
}
