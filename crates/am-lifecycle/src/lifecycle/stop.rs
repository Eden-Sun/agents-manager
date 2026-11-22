//! Stopping, interrupting and aborting: the ways a run or a turn ends on purpose.

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::capabilities::{BotLocks, Db};
use crate::hosts::HostsAccess;
use super::*;
use super::s6_ports::{StopContext, StopServices};
use super::start::ports::{HandoffSessionRepo, RemoteCleanupPort, ShareSessionRepo};

pub async fn stop_bot(app: &impl StopContext, bot_id: &str) -> LcResult<bool> {
    let _g = app.lock_bot_for_stop(bot_id).await;
    stop_bot_locked(app, bot_id).await
}

/// Is this run sitting in the user's own herdr `default` session (SPEC §6.5.1)?
pub fn in_default_session(run: &db::Run) -> bool {
    run.herdr_session.as_deref() == Some("default")
}

/// SPEC §6.5.1: a bot from the user's `default` session is observed, never driven.
pub fn refuse_default_session(bot: &db::Bot) -> LcResult<()> {
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
pub async fn stop_bot_locked(app: &impl StopContext, bot_id: &str) -> LcResult<bool> {
    stop_locked(app, bot_id, false, false, false, None, None).await
}

/// 閒置回收用的 [`stop_bot_locked`]：記 `stopping` 的那一步同時是**收機許可**（issue #144）——`agent_status` 還是
/// `idle`、這個 run 沒有 in-flight 回合、這顆 bot 沒有排隊中的回合，才准停；否則什麼都不動，回 409 `no_longer_idle`。
///
/// bot 鎖擋得住經 daemon 進來的新工作，擋不住使用者直接在 pane 裡打字：那條路是 `events::handle_status`，不拿鎖就寫
/// `agent_status`。巡邏在鎖裡最後一次讀到 idle 之後、停機之前被那一句插進來的話，以前照樣 ctrl+c、關 pane。條件寫在
/// 同一句 UPDATE 裡，跟那一句寫入由 SQLite 排序：它先落地，這裡 0 rows 不停；這裡先落地，run 已經是 `stopping`，
/// `begin_external_turn`（在鎖裡看 `state == running`）就不會替一個正在關的 pane 開回合。
pub async fn stop_bot_locked_if_idle(app: &impl StopContext, bot_id: &str) -> LcResult<bool> {
    stop_locked(app, bot_id, false, true, false, None, None).await
}

/// 重啟那一半的 stop。差別只在 `stopped` 寫不進去之後的重試：重啟沒把 bot 開回來不是「使用者要它停」
/// （同 `left_down_by_restart`），所以交給對帳照證據收成 `exited`，不補記 `stopped`。
pub async fn stop_for_restart_locked(app: &impl StopContext, bot_id: &str) -> LcResult<bool> {
    stop_locked(app, bot_id, true, false, false, None, None).await
}

pub async fn stop_for_restart_locked_with_host_fence(
    app: &impl StopContext,
    bot_id: &str,
    fence: &crate::hosts::HostFence,
) -> LcResult<bool> {
    stop_locked(app, bot_id, true, false, false, Some(fence), None).await
}

/// 一鍵重啟（`require_idle`）那一半的 stop：記 `stopping` 的那一步同時是**「還是閒著才准停」的許可**（#346，同 [`stop_bot_locked_if_idle`]），
/// 使用者剛在 pane 裡打字（`handle_status` 不拿鎖）落在鎖裡看過閒置之後，這一句 UPDATE 輸了、什麼都不動，回 409 `no_longer_idle`。
pub async fn stop_for_restart_if_idle_locked(app: &impl StopContext, bot_id: &str, refuse_background_jobs: bool) -> LcResult<bool> {
    stop_locked(app, bot_id, true, true, refuse_background_jobs, None, None).await
}

pub async fn stop_for_restart_if_idle_locked_with_host_fence(
    app: &impl StopContext,
    bot_id: &str,
    refuse_background_jobs: bool,
    fence: &crate::hosts::HostFence,
) -> LcResult<bool> {
    stop_locked(app, bot_id, true, true, refuse_background_jobs, Some(fence), None).await
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

/// #1465：debug 建置下呼叫端的框會放進整個 future 值（四支 `stop_for_restart_*` 各一份）；這裡 `Box::pin`，呼叫端只留指標。
fn stop_locked<'a>(
    app: &'a impl StopContext,
    bot_id: &'a str,
    for_restart: bool,
    only_if_idle: bool,
    refuse_background_jobs: bool,
    host_fence: Option<&'a crate::hosts::HostFence>,
    notified_idle_at: Option<&'a str>,
) -> impl std::future::Future<Output = LcResult<bool>> + 'a {
    Box::pin(stop_locked_body(app, bot_id, for_restart, only_if_idle, refuse_background_jobs, host_fence, notified_idle_at))
}

async fn stop_locked_body(
    app: &impl StopContext,
    bot_id: &str,
    for_restart: bool,
    only_if_idle: bool,
    refuse_background_jobs: bool,
    host_fence: Option<&crate::hosts::HostFence>,
    notified_idle_at: Option<&str>,
) -> LcResult<bool> {
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    app.refuse_handed_off(bot_id).await?;
    if crate::config::is_api_kind(&bot.kind) {
        if for_restart {
            return Err(LcError::conflict(
                "api_bot_restart_unsupported",
                json!({"bot_id": bot_id}),
            ));
        }
        if !crate::lifecycle::api_bot_port::ApiBotPort::api_recovery_ready(app) {
            return Err(LcError::Unavailable(json!({"error":"api_startup_quarantined", "message":"修復 API bot 恢復問題後，正常重啟 daemon 再試", "retryable":true})));
        }
        let Some(run) = db::active_run(app.db(), bot_id).await.map_err(up)? else {
            return Ok(false);
        };
        if only_if_idle && refuse_background_jobs {
            app.refresh_background_jobs(&run, &bot.kind).await;
            if let Some(n) = app.background_jobs_count(&run.id).filter(|n| *n > 0) {
                return Err(LcError::conflict(
                    "not_idle",
                    json!({"bot_id": bot_id, "busy": "background_jobs", "background_jobs": n}),
                ));
            }
        }
        use super::api_bot_port::{ApiBeginStopError, ApiBeginStopMode, ApiBeginStopResult, ApiBotPort, ApiRunKey};
        let key = ApiRunKey { bot_id: bot_id.to_string(), run_id: run.id.clone() };
        let mode = if only_if_idle { ApiBeginStopMode::Idle } else {
            ApiBeginStopMode::User { withdraw: !super::restart_hold::in_progress(bot_id) }
        };
        let (moved, withdrawn) = match app.api_run_begin_stop(&key, mode).await {
            Ok(ApiBeginStopResult::Applied { withdrawn }) => (super::run_state::Moved::Applied, withdrawn),
            Ok(ApiBeginStopResult::Lost) => (super::run_state::Moved::Lost, Vec::new()),
            Err(error) => {
                let reason = match error {
                    ApiBeginStopError::Storage => "storage",
                    ApiBeginStopError::IdentityMismatch => "identity_mismatch",
                    ApiBeginStopError::Busy => "busy",
                    ApiBeginStopError::Ambiguous => "unconfirmed",
                };
                return Err(LcError::Upstream(format!("api_bot_stop_unconfirmed:{reason}")));
            }
        };
        for (turn_id, revoked) in withdrawn {
            app.announce_revoked(&turn_id, revoked).await;
        }
        match moved {
            super::run_state::Moved::Applied => {}
            super::run_state::Moved::Lost if only_if_idle => {
                app.notify_bot_status(bot_id).await;
                return Err(LcError::conflict("no_longer_idle", json!({"bot_id": bot_id, "run_id": run.id})));
            }
            super::run_state::Moved::Lost => {
                app.notify_bot_status(bot_id).await;
                return Ok(false);
            }
        }
        app.notify_bot_status(bot_id).await;
        let Some(turn) = db::in_flight_turn(app.db(), &run.id).await.map_err(up)? else {
            let key = crate::lifecycle::api_bot_port::ApiRunKey {
                bot_id: bot_id.to_string(),
                run_id: run.id.clone(),
            };
            let proof = crate::lifecycle::api_bot_port::ApiBotPort::api_run_stop_proof(app, &key)
                .await;
            match proof {
                Ok(proof) if proof.key == key => {}
                Ok(_) => {
                    tracing::warn!(bot = bot_id, run = %run.id, error_code = "identity_mismatch", "API bot stop received proof for another run");
                    return Err(LcError::Upstream("api_bot_stop_unconfirmed:identity_mismatch".into()));
                }
                Err(error) => {
                    let code = match error {
                        crate::lifecycle::api_bot_port::ApiBotError::Busy => "busy",
                        crate::lifecycle::api_bot_port::ApiBotError::IdentityMismatch => "identity_mismatch",
                        crate::lifecycle::api_bot_port::ApiBotError::Unconfirmed => "unconfirmed",
                        crate::lifecycle::api_bot_port::ApiBotError::Upstream => "upstream",
                    };
                    tracing::warn!(bot = bot_id, run = %run.id, error_code = code, "API bot stop did not confirm the idle runner proof");
                    return Err(LcError::Upstream(format!("api_bot_stop_unconfirmed:{code}")));
                }
            }
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::race_point::hit("api_stop_proof_before_recover", &run.id).await;
            if db::api_bot::recover_unknown(app.db(), &run.id).await.is_err() {
                tracing::warn!(bot = bot_id, run = %run.id, error_code = "recover_unknown", "API bot stop could not recover unknown attempts");
                return Err(LcError::Upstream("api_bot_stop_unconfirmed:recover_unknown".into()));
            }
            match commit_stopped(app, &run.id, false).await {
                Ok(StopCommit::Applied) => {
                    app.revoke_orphaned_queued_turns(bot_id, "bot 已被停止").await;
                }
                Ok(StopCommit::Relabelled | StopCommit::Lost) => {}
                Err(_) => {
                    tracing::warn!(bot = bot_id, run = %run.id, error_code = "stop_commit", "API bot stop could not record the stopped run");
                    super::run_state::schedule_settle(app, &run.id, super::run_state::Settle::FinishStop);
                    return Err(LcError::Upstream("api_bot_stop_unconfirmed:stop_commit".into()));
                }
            }
            app.notify_bot_status(bot_id).await;
            return Ok(true);
        };
        let key = crate::lifecycle::api_bot_port::ApiTurnKey {
            bot_id: bot_id.to_string(),
            run_id: run.id.clone(),
            turn_id: turn.id.clone(),
        };
        if let Err(error) = crate::lifecycle::api_bot_port::ApiBotPort::api_run_cancel_and_join(app, &key).await {
            let code = match error {
                crate::lifecycle::api_bot_port::ApiBotError::Busy => "busy",
                crate::lifecycle::api_bot_port::ApiBotError::IdentityMismatch => "identity_mismatch",
                crate::lifecycle::api_bot_port::ApiBotError::Unconfirmed => "unconfirmed",
                crate::lifecycle::api_bot_port::ApiBotError::Upstream => "upstream",
            };
            tracing::warn!(bot = bot_id, run = %run.id, turn = %turn.id, error_code = code, "API bot stop did not confirm the runner join");
            return Err(LcError::Upstream(format!("api_bot_stop_unconfirmed:{code}")));
        }
        if db::api_bot::recover_unknown(app.db(), &run.id).await.is_err() {
            tracing::warn!(bot = bot_id, run = %run.id, error_code = "recover_unknown", "API bot stop could not recover unknown attempts");
            return Err(LcError::Upstream("api_bot_stop_unconfirmed:recover_unknown".into()));
        }
        if app.fail_in_flight(&run.id, "run stopped by user").await.is_err() {
            tracing::warn!(bot = bot_id, run = %run.id, error_code = "turn_settle", "API bot stop could not settle the in-flight turn");
            return Err(LcError::Upstream("api_bot_stop_unconfirmed:turn_settle".into()));
        }
        match commit_stopped(app, &run.id, false).await {
            Ok(StopCommit::Applied) => {
                app.revoke_orphaned_queued_turns(bot_id, "bot 已被停止").await;
            }
            Ok(StopCommit::Relabelled | StopCommit::Lost) => {}
            Err(_) => {
                tracing::warn!(bot = bot_id, run = %run.id, error_code = "stop_commit", "API bot stop could not record the stopped run");
                super::run_state::schedule_settle(app, &run.id, super::run_state::Settle::FinishStop);
                return Err(LcError::Upstream("api_bot_stop_unconfirmed:stop_commit".into()));
            }
        }
        app.notify_bot_status(bot_id).await;
        return Ok(true);
    }
    let Some(run) = db::active_run(app.db(), bot_id).await.map_err(up)? else {
        match host_fence {
            Some(fence) => {
                #[cfg(all(test, feature = "daemon-test-harness"))]
                super::race_point::hit("restart_before_no_run_preview_cleanup", bot_id).await;
                if !app.stop_preview(bot_id, Some(fence)).await {
                    tracing::warn!(bot = %bot.name, host = %fence.conn().name, reason = "superseded", "skipped preview cleanup because the scoped host authority changed");
                    return Err(LcError::conflict("host_superseded", json!({"bot_id": bot_id, "host": fence.conn().name})));
                }
            }
            None => {
                let _ = app.stop_preview(bot_id, None).await;
            }
        }
        return Ok(false);
    };
    let host = db::bot_host(app.db(), bot_id).await.map_err(up)?;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("restart_before_stop_client_lookup", bot_id).await;
    let client = match host_fence {
        Some(fence) => client_for_run_with_host_fence(app, &run, fence).await?,
        None => client_for_run(app, &run).await?.client,
    };
    let target = super::dead_panes::execution_target(&client, &run, &bot).await?;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    {
        super::race_point::hit("stop_before_stopping", bot_id).await;
    }

    if only_if_idle && refuse_background_jobs {
        app.refresh_background_jobs(&run, &bot.kind).await;
        if let Some(n) = app.background_jobs_count(&run.id).filter(|n| *n > 0) {
            return Err(LcError::conflict(
                "not_idle",
                json!({"bot_id": bot_id, "busy": "background_jobs", "background_jobs": n}),
            ));
        }
    }

    let (moved, withdrawn) = if only_if_idle {
        (admit_idle_stop_with_notice(app.db(), &run.id, bot_id, for_restart, notified_idle_at).await.map_err(up)?, Vec::new())
    } else {
        let withdraw = !for_restart && !super::restart_hold::in_progress(bot_id);
        begin_stop(app, &run.id, bot_id, withdraw).await.map_err(up)?
    };
    for (turn_id, revoked) in withdrawn {
        app.announce_revoked(&turn_id, revoked).await;
    }
    match moved {
        super::run_state::Moved::Applied => {}
        super::run_state::Moved::Lost if only_if_idle => {
            app.notify_bot_status(bot_id).await;
            return Err(LcError::conflict("no_longer_idle", json!({"bot_id": bot_id, "run_id": run.id})));
        }
        super::run_state::Moved::Lost => {
            tracing::info!(bot = %bot.name, run = %run.id, "stop: another path ended the run first; nothing left to stop");
            app.notify_bot_status(bot_id).await;
            return Ok(false);
        }
    }
    app.notify_bot_status(bot_id).await;
    let stop_signal = async {
        if let Err(e) = app.fail_in_flight(&run.id, "run stopped by user").await {
            return Err(turn_unwritable(app, bot_id, &run.id, "停", e).await);
        }
        for _ in 0..2 {
            let _ = client.agent_send_keys(&target, &["ctrl+c".to_string()]).await;
            tokio::time::sleep(pane_settle(Duration::from_millis(500))).await;
        }
        Ok(())
    };
    let stop_signal = match host_fence {
        Some(fence) => app.hosts().run_if_current(fence, stop_signal).await,
        None => Some(stop_signal.await),
    };
    match stop_signal {
        Some(Ok(())) => {}
        Some(Err(e)) => return Err(e),
        None => {
            back_to_running(app, &run.id).await;
            app.notify_bot_status(bot_id).await;
            return Err(LcError::conflict("host_superseded", json!({"bot_id": bot_id, "host": host_fence.unwrap().conn().name})));
        }
    }

    match host_fence {
        Some(fence) => {
            if !app.stop_preview(bot_id, Some(fence)).await {
                tracing::info!(bot = %bot.name, host = %host, "skipped preview cleanup after the scoped host authority was superseded");
            }
        }
        None => {
            let _ = app.stop_preview(bot_id, None).await;
        }
    }
    let mut gone = false;
    for _ in 0..20 {
        let agent = client.agent_get(&target).await;
        let pane = match run.pane_id.as_deref() {
            Some(p) => Some(client.pane_get(p).await),
            None => None,
        };
        let confirmed_gone = match pane {
            Some(Ok(None)) => true,
            Some(Ok(Some(pane))) => pane.agent.is_none() && matches!(agent, Ok(None)),
            _ => false,
        };
        if confirmed_gone {
            gone = true;
            break;
        }
        tokio::time::sleep(pane_settle(Duration::from_millis(500))).await;
    }
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
        if outcome == StopOutcome::StillAlive {
            back_to_running(app, &run.id).await;
        } else {
            super::run_state::schedule_settle(app, &run.id, super::run_state::Settle::Reconcile { stuck: "stopping".into() });
        }
        app.notify_bot_status(bot_id).await;
        return Err(LcError::Upstream(format!(
            "stop_not_confirmed: agent `{target}` did not exit ({outcome:?}); the run is not recorded as stopped"
        )));
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    {
        super::race_point::hit("stop_before_stopped", bot_id).await;
    }
    match commit_stopped(app, &run.id, for_restart).await {
        Ok(StopCommit::Applied) => after_stop(app, bot_id, &run, &host).await,
        Ok(StopCommit::Relabelled | StopCommit::Lost) => {}
        Err(e) => {
            tracing::warn!(bot = %bot.name, run = %run.id, error = %e, "the agent is stopped but its run could not be recorded as stopped");
            let how = if for_restart {
                super::run_state::Settle::Reconcile { stuck: "stopping".into() }
            } else {
                super::run_state::Settle::FinishStop
            };
            super::run_state::schedule_settle(app, &run.id, how);
            app.notify_bot_status(bot_id).await;
            return Err(LcError::uncommitted(
                "stop_state_uncommitted",
                &run.id,
                "agent 已經停了，但 run 的狀態寫不進 DB；已排重試，會補記成停止",
                e,
            ));
        }
    }
    app.notify_bot_status(bot_id).await;
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
#[doc(hidden)]
pub fn api_begin_stop_transaction<'a>(pool: &'a sqlx::SqlitePool, key: &'a super::api_bot_port::ApiRunKey, mode: super::api_bot_port::ApiBeginStopMode, shutdown: &'a tokio_util::sync::CancellationToken) -> impl std::future::Future<Output = Result<super::api_bot_port::ApiBeginStopResult, super::api_bot_port::ApiBeginStopError>> + Send + 'a {
    Box::pin(api_begin_stop_transaction_body(pool, key, mode, shutdown))
}

async fn api_begin_stop_transaction_body(pool: &sqlx::SqlitePool, key: &super::api_bot_port::ApiRunKey, mode: super::api_bot_port::ApiBeginStopMode, shutdown: &tokio_util::sync::CancellationToken) -> Result<super::api_bot_port::ApiBeginStopResult, super::api_bot_port::ApiBeginStopError> {
    use super::api_bot_port::{ApiBeginStopError as E, ApiBeginStopMode as M, ApiBeginStopResult as R};
    if shutdown.is_cancelled() { return Err(E::Busy); }
    let mut tx = db::begin_write(pool).await.map_err(|_| E::Storage)?;
    let result: Result<R, E> = async {
        if shutdown.is_cancelled() { return Err(E::Busy); }
        let state: Option<String> = sqlx::query_scalar(concat!("SELECT r.state FROM bots b JOIN runs r ON r.bot_id=b.id WHERE b.id=? AND b.kind IN ", crate::config::api_kinds_sql!(), " AND b.deleted_at IS NULL AND r.id=?"))
            .bind(&key.bot_id).bind(&key.run_id).fetch_optional(&mut *tx).await.map_err(|_| E::Storage)?;
        let Some(state) = state else { return Err(E::IdentityMismatch); };
        if !matches!(state.as_str(), "starting" | "running" | "stopping") { return Ok(R::Lost); }
        if shutdown.is_cancelled() { return Err(E::Busy); }
        match mode {
            M::User { withdraw } => {
                let moved = super::run_state::transition_on(&mut tx, &key.run_id, super::run_state::LIVE, "stopping", None).await.map_err(|_| E::Storage)?;
                if moved == super::run_state::Moved::Lost { return Ok(R::Lost); }
                let withdrawn = if withdraw { super::start_send::withdraw_waiting_tx(&mut tx, &key.bot_id).await.map_err(|_| E::Storage)? } else { Vec::new() };
                Ok(R::Applied { withdrawn })
            }
            M::Idle => {
                let count = sqlx::query(concat!("UPDATE runs SET state='stopping' WHERE id=? AND bot_id=? AND state='running' AND agent_status='idle' AND EXISTS(SELECT 1 FROM bots b WHERE b.id=runs.bot_id AND b.kind IN ", crate::config::api_kinds_sql!(), " AND b.deleted_at IS NULL) AND NOT EXISTS(SELECT 1 FROM turns t WHERE t.run_id=runs.id AND t.status='in_flight') AND NOT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.status='queued')"))
                    .bind(&key.run_id).bind(&key.bot_id).bind(&key.bot_id).execute(&mut *tx).await.map_err(|_| E::Storage)?.rows_affected();
                Ok(if count == 1 { R::Applied { withdrawn: Vec::new() } } else { R::Lost })
            }
        }
    }.await;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("api_begin_stop_before_commit", &key.run_id).await;
    let result = if shutdown.is_cancelled() { Err(E::Busy) } else { result };
    match result {
        Ok(result) => {
            let commit_result = tx.commit().await;
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::race_point::hit("api_begin_stop_after_commit", &key.run_id).await;
            if commit_result.is_err() || shutdown.is_cancelled() { return Err(E::Ambiguous); }
            Ok(result)
        }
        Err(error) => { tx.rollback().await.map_err(|_| E::Ambiguous)?; Err(error) }
    }
}

async fn begin_stop(
    app: &impl crate::capabilities::Db,
    run_id: &str,
    bot_id: &str,
    withdraw: bool,
) -> anyhow::Result<(super::run_state::Moved, Vec<(String, Revoked)>)> {
    let mut tx = app.db().begin().await?;
    let moved = super::run_state::transition_on(&mut tx, run_id, super::run_state::LIVE, "stopping", None).await?;
    if moved == super::run_state::Moved::Lost {
        return Ok((moved, Vec::new()));
    }
    let withdrawn = if withdraw { super::start_send::withdraw_waiting_tx(&mut tx, bot_id).await? } else { Vec::new() };
    tx.commit().await?;
    Ok((moved, withdrawn))
}

/// [`stop_bot_locked_if_idle`] 的許可：跟 `transition(LIVE → stopping)` 同一步，多三個條件，都在同一句 UPDATE 裡。
///
/// `allow_queued`：重啟那一半（#346）排隊中的訊息不擋——`restart_hold` 本來就把它們留給新的 run；閒置回收（收起來不再送）才擋。
pub async fn admit_idle_stop(db: &sqlx::SqlitePool, run_id: &str, bot_id: &str, allow_queued: bool) -> Result<super::run_state::Moved, sqlx::Error> {
    admit_idle_stop_with_notice(db, run_id, bot_id, allow_queued, None).await
}

pub(crate) async fn admit_idle_stop_with_notice(db: &sqlx::SqlitePool, run_id: &str, bot_id: &str, allow_queued: bool, idle_at: Option<&str>) -> Result<super::run_state::Moved, sqlx::Error> {
    let queued = if allow_queued {
        ""
    } else {
        "AND NOT EXISTS (SELECT 1 FROM turns t JOIN conversations c ON c.id = t.conversation_id
                             WHERE c.bot_id = ? AND t.status = 'queued')"
    };
    let notice = if idle_at.is_some() {
        "AND EXISTS (SELECT 1 FROM child_done_state d WHERE d.child_id=runs.bot_id AND d.active=0 AND d.idle_at=? AND d.notified_at IS NOT NULL AND d.pending_turn_id IS NULL AND d.notified_background=0 AND (runs.agent_status_since IS NULL OR julianday(runs.agent_status_since)<=julianday(d.idle_at)) AND NOT EXISTS(SELECT 1 FROM child_done_sources s WHERE s.child_id=d.child_id AND s.covered=0) AND NOT EXISTS(SELECT 1 FROM relay_requests r JOIN bots b ON b.id=r.to_bot_id WHERE b.id=d.child_id AND b.parent_bot_id=r.from_bot_id AND (r.state IN ('pending','delivered','answered') OR (r.state='failed' AND r.notified_at IS NULL AND (r.alerted_at IS NULL OR r.error LIKE 'notification delivery unconfirmed%' OR EXISTS(SELECT 1 FROM turns rt JOIN conversations rc ON rc.id=rt.conversation_id AND rc.bot_id=b.id WHERE (rt.id=r.to_turn_id OR rt.client_request_id='relay-request:'||r.id) AND NOT EXISTS(SELECT 1 FROM child_done_sources cs WHERE cs.child_id=b.id AND cs.turn_id=rt.id AND cs.covered=1)))))) AND NOT EXISTS(SELECT 1 FROM relay_requests r JOIN bots b ON b.id=r.to_bot_id JOIN turns rt ON (rt.id=r.to_turn_id OR rt.client_request_id='relay-request:'||r.id) JOIN conversations rc ON rc.id=rt.conversation_id AND rc.bot_id=b.id WHERE b.id=d.child_id AND b.parent_bot_id=r.from_bot_id AND r.notified_at IS NOT NULL AND NOT EXISTS(SELECT 1 FROM child_done_sources cs WHERE cs.child_id=b.id AND cs.turn_id=rt.id AND cs.covered=1)) AND NOT EXISTS(SELECT 1 FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=d.child_id AND t.status IN ('completed','completed_fallback','failed') AND t.rowid>d.covered_rowid))"
    } else { "" };
    let sql = format!(
        "UPDATE runs SET state = 'stopping'
          WHERE id = ? AND state = 'running' AND agent_status = 'idle'
            AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.run_id = runs.id AND t.status = 'in_flight')
            {queued}
            {notice}"
    );
    let mut q = sqlx::query(&sql).bind(run_id);
    if !allow_queued {
        q = q.bind(bot_id);
    }
    if let Some(idle) = idle_at { q = q.bind(idle); }
    let r = q.execute(db).await?;
    Ok(if r.rows_affected() == 0 { super::run_state::Moved::Lost } else { super::run_state::Moved::Applied })
}

/// `stopping → stopped`。CAS 輸了而且是被不拿 bot 鎖的 pane-exit 事件收成 `exited`（多半是 stop 自己關的 pane
/// 觸發的）：改標成 `stopped`（`run_state::relabel`）——兩個都是終態、不復活任何東西，但 `stopped` 才是「使用者要它停」
/// 的紀錄（#131，incident 探針靠它分辨）。改標寫不進去回錯，不當成 CAS 輸了（#146 重開 A）。
///
/// 重啟那一半（`for_restart`）不改標：重啟不是「使用者要它停」——開不回來時要的正是 `exited`（同 `left_down_by_restart`），
/// 開回來了舊 run 的標籤也不再代表 bot。
async fn commit_stopped(app: &impl crate::capabilities::Db, run_id: &str, for_restart: bool) -> Result<StopCommit, sqlx::Error> {
    match super::run_state::transition(app.db(), run_id, &["stopping"], "stopped", None).await? {
        super::run_state::Moved::Applied => Ok(StopCommit::Applied),
        super::run_state::Moved::Lost if for_restart => Ok(StopCommit::Lost),
        super::run_state::Moved::Lost => match super::run_state::relabel(app.db(), run_id, "exited", "stopped").await? {
            super::run_state::Moved::Applied => Ok(StopCommit::Relabelled),
            super::run_state::Moved::Lost => Ok(StopCommit::Lost),
        },
    }
}

/// 終態寫進去之後才做的收尾（跟 #135 同一條：先有 durable 的終態，才撤佇列、拆 watcher）。
async fn after_stop(app: &impl StopContext, bot_id: &str, run: &db::Run, host: &str) {
    app.revoke_orphaned_queued_turns(bot_id, "bot 已被停止").await;
    if let Some(p) = run.pane_id.as_deref() {
        if let Some(session) = app.session_for_run(run).await {
            app.unwatch_pane_on_session(host, &session, p).await;
        }
    }
}

/// 停不下來（agent 還在）：`stopping → running` 放回去。留在 `stopping` 會讓 prompt 409、start 拒絕，
/// default session 的 run 連對帳都不救（2026-09-12 review #1）。寫不進去就排重試。
/// 停（或原地重啟）之前，在飛的那一筆收不成（#156）：外面還一步都沒動——agent 沒被打斷、那一回合真的還在跑。
/// run 放回 running，回 503 可重試。不是 `Uncommitted`：那是「外面已經做了、DB 沒寫成」，這裡外面什麼都還沒做。
pub async fn turn_unwritable(app: &impl StopContext, bot_id: &str, run_id: &str, what: &str, e: anyhow::Error) -> LcError {
    tracing::warn!(bot = bot_id, run = run_id, error = %e, "the in-flight turn could not be closed; not touching the agent");
    back_to_running(app, run_id).await;
    app.notify_bot_status(bot_id).await;
    let turn = db::in_flight_turn(app.db(), run_id).await.ok().flatten().map(|t| t.id);
    LcError::Unavailable(json!({
        "error": "turn_state_unwritable", "run_id": run_id, "turn_id": turn, "retryable": true, "retry_after_secs": 5,
        "message": format!("在飛的那一回合寫不進 DB，沒有{what}：agent 照常在跑（那一回合也還在跑），稍後再試。"),
        "detail": format!("{e:#}"),
    }))
}

pub async fn back_to_running(app: &impl StopContext, run_id: &str) {
    if let Err(e) = super::run_state::transition(app.db(), run_id, &["stopping"], "running", None).await {
        tracing::warn!(run = run_id, error = %e, "could not put a run that did not stop back to running");
        super::run_state::schedule_settle(app, run_id, super::run_state::Settle::BackToRunning);
    }
}

/// [`run_state::Settle::FinishStop`] 的重試：stop 在外面已經做完，補記 `stopped` 與它之後的收尾。
/// 在 bot 鎖裡做；只動仍停在 `stopping`（或被 pane-exit 收成 `exited`、還沒改標）的這一顆。
pub async fn finish_stop(app: &impl StopContext, run_id: &str) {
    let Ok(Some(run)) = db::run(app.db(), run_id).await else { return };
    let _g = app.lock_bot_for_stop(&run.bot_id).await;
    let host = match db::bot_host(app.db(), &run.bot_id).await {
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
    app.notify_bot_status(&run.bot_id).await;
}

/// stop (if running) + start. Used to make edited `model` / `args` / `identity` / `env` take effect.
pub async fn run_alive(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), run: &db::Run, bot: &db::Bot) -> bool {
    let target = db::run_target(run, bot);
    if target.is_empty() { return false; }
    let pane = target.as_str();
    let Ok(client) = client_for_run(app, run).await else { return true };
    if super::dead_panes::pane_id_reused(&client, run).await { return false; }
    match client.pane_get(pane).await {
        Ok(None) => return false,
        Ok(Some(p)) if p.agent.is_some() => return true,
        _ => {}
    }
    !matches!(client.agent_get(pane).await, Ok(None))
}

/// #61: one-time purge of `bots/<id>/` for soft-deleted bots with no live run. Dirs no bot row
/// claims are left alone (rt-87's `bots-orphan-backup-2026-09-10/` is outside `bots/`).
pub async fn purge_deleted_bot_dirs(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + HandoffSessionRepo + StopServices)) -> usize {
    let mut removed = 0;
    let root = app.data_dir().join("bots");
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_string) else { continue };
            let deleted: Option<Option<String>> = match sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id = ?")
                .bind(&id)
                .fetch_optional(app.db())
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
            if !matches!(app.bot_handed_off_to(&id).await, Ok(None)) {
                continue;
            }
            match db::active_run(app.db(), &id).await {
                Ok(None) => {}
                Ok(Some(_)) => continue,
                Err(e) => {
                    tracing::warn!(bot = %id, error = %e, "could not read whether a deleted bot still has a live run; leaving its directory for the next start");
                    continue;
                }
            }
            match crate::bot_trash::move_in(app.data_dir(), &id, &entry.path()) {
                Ok(_) => removed += 1,
                Err(e) => tracing::warn!(dir = %entry.path().display(), error = %e, "could not move a deleted bot's directory to bots-trash"),
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "moved bots/<id>/ directories left behind by deleted bots to bots-trash");
    }
    let restricted_deleted: Vec<(String, String)> = sqlx::query_as(
        "SELECT s.bot_id, s.workspace FROM shared_bots s
         JOIN bots b ON b.id = s.bot_id
         JOIN projects p ON p.id = b.project_id
         WHERE b.deleted_at IS NOT NULL AND s.profile = 'restricted' AND p.host = 'local'",
    )
    .fetch_all(app.db())
    .await
    .unwrap_or_default();

    for (id, ws) in restricted_deleted {
        if !matches!(app.bot_handed_off_to(&id).await, Ok(None)) {
            continue;
        }
        match db::active_run(app.db(), &id).await {
            Ok(None) => {}
            _ => continue,
        }
        if let Some(ws_path) = app.validate_workspace_path(app.data_dir(), &ws).await {
            match crate::bot_trash::move_in_kind(app.data_dir(), &id, Some(crate::bot_trash::SHARE_WORKSPACE), &ws_path) {
                Ok(Some(to)) => {
                    tracing::info!(bot = %id, dir = %ws_path.display(), trash = %to.display(), "moved leftover restricted share workspace to bots-trash");
                    removed += 1;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(bot = %id, dir = %ws_path.display(), error = %e, "could not move leftover restricted share workspace to bots-trash"),
            }
        }
    }
    let known = crate::bot_trash::known_bot_ids(app.db()).await;
    let (expired, evicted) = crate::bot_trash::gc_once(app.data_dir(), crate::bot_trash::keep_duration(), crate::bot_trash::MAX_BYTES, &known).await;
    if expired > 0 || evicted > 0 {
        tracing::info!(expired, evicted, days = crate::bot_trash::KEEP_DAYS, "removed expired or over-cap bots-trash entries");
    }
    removed
}

/// 回 `true`＝目錄確定沒了（含本來就不在）；`false`＝沒清成（id 不合法、主機不明、ssh 失敗、I/O 錯），呼叫端不能當成清掉了。
pub async fn purge_bot_dir(
    app: &(impl crate::capabilities::DataDir
        + crate::capabilities::Db
        + HostsAccess
        + RemoteCleanupPort
        + HandoffSessionRepo
        + ShareSessionRepo
        + StopServices),
    bot_id: &str,
    host: &str,
) -> bool {
    if !valid_id(bot_id) {
        tracing::warn!(host, bot = %bot_id, "invalid bot id; bot config dir left in place");
        return false;
    }
    if !matches!(app.bot_handed_off_to(bot_id).await, Ok(None)) {
        tracing::info!(host, bot = %bot_id, "bot of a handed-off project (or unreadable); bot config dir left in place");
        return false;
    }
    let att = crate::bot_trash::attachments_dir(crate::capabilities::DataDir::data_dir(app), bot_id);
    match crate::bot_trash::move_in_kind(crate::capabilities::DataDir::data_dir(app), bot_id, Some(crate::bot_trash::ATTACHMENTS), &att) {
        Ok(Some(to)) => tracing::info!(bot = %bot_id, dir = %att.display(), trash = %to.display(), "moved bot attachments to bots-trash"),
        Ok(None) => {}
        Err(e) => tracing::warn!(bot = %bot_id, dir = %att.display(), error = %e, "could not move bot attachments to bots-trash"),
    }
    if host == LOCAL_HOST {
        if let Ok(Some(ws)) = app.restricted_workspace(bot_id).await {
            if let Some(ws_path) = app.validate_workspace_path(crate::capabilities::DataDir::data_dir(app), &ws).await {
                match crate::bot_trash::move_in_kind(crate::capabilities::DataDir::data_dir(app), bot_id, Some(crate::bot_trash::SHARE_WORKSPACE), &ws_path) {
                    Ok(Some(to)) => tracing::info!(bot = %bot_id, dir = %ws_path.display(), trash = %to.display(), "moved restricted share workspace to bots-trash"),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(bot = %bot_id, dir = %ws_path.display(), error = %e, "could not move restricted share workspace to bots-trash"),
                }
            }
        }
    }
    if host == LOCAL_HOST {
        let Ok(dir) = app.bot_dir(bot_id) else { return false };
        return match crate::bot_trash::move_in(crate::capabilities::DataDir::data_dir(app), bot_id, &dir) {
            Ok(Some(to)) => {
                tracing::info!(dir = %dir.display(), trash = %to.display(), "moved bot config dir to bots-trash");
                true
            }
            Ok(None) => true,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "could not move bot config dir to bots-trash");
                false
            }
        };
    }
    if app.is_shared_host(host).await {
        tracing::info!(host, bot = %bot_id, "shared-session host; remote bot dir left in place");
        return false;
    }
    let Some(conn) = app.hosts().get(host).await else {
        tracing::warn!(host, bot = %bot_id, "unknown host; remote bot dir left in place");
        RemoteCleanupPort::record_remote_purge(app, bot_id, host, false, Some("unknown host")).await;
        return false;
    };
    match RemoteCleanupPort::move_remote_bot_dir_to_trash(app, &conn, bot_id).await {
        Ok(to) => {
            tracing::info!(host, bot = %bot_id, trash = ?to, "moved remote bot config dir to bots-trash");
            RemoteCleanupPort::record_remote_purge(app, bot_id, host, true, None).await;
            true
        }
        Err(e) => {
            let msg = format!("{e:#}");
            tracing::warn!(host, bot = %bot_id, error = %msg, "could not move remote bot config dir to bots-trash");
            RemoteCleanupPort::record_remote_purge(app, bot_id, host, false, Some(&msg)).await;
            false
        }
    }
}


/// [`interrupt_turn`] 不綁哪一筆（測試用；API 走 `interrupt_turn`）。
#[cfg(all(test, feature = "daemon-test-harness"))]
pub async fn interrupt_bot(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    interrupt_turn(app, bot_id, None).await
}

/// 送 Esc 打斷這顆 bot 正在跑的回合。`expect_turn` 給了就只打斷那一筆：它已經不在飛（別的路收掉了、下一回合已經開始）就不按 Esc，
/// 回 409 `turn_not_in_flight`。重試上一次中斷時帶上它，才不會誤傷下一回合（#147）。
///
/// Esc 與「把回合收成 failed」是兩半（見 `interruption`）：Esc 沒進 pane 就什麼都不動；進了而 DB 寫不進去，
/// 回 503 `interrupt_state_uncommitted`（不是普通的成功），欠著的收尾之後補——同一筆的重試**不再按** Esc；
/// 不知道 Esc 進了沒有，回合留在 in_flight，等它的回聲。
pub async fn interrupt_turn(app: &impl StopContext, bot_id: &str, expect_turn: Option<&str>) -> LcResult<()> {
    let seen = if expect_turn.is_none() { in_flight_at_arrival(app, bot_id).await? } else { None };
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("interrupt_after_arrival", bot_id).await;
    let _g = app.lock_bot_for_stop(bot_id).await;
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    if crate::config::is_api_kind(&bot.kind) {
        return interrupt_api_turn(app, bot_id, &run, expect_turn, seen.as_ref()).await;
    }
    let owed = super::interruption::owed_turn(bot_id);
    let settled = super::interruption::settle_locked(app, bot_id, super::interruption::Evidence::Nothing).await;
    let in_flight = db::in_flight_turn(app.db(), &run.id).await.map_err(up)?;
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
    if let (None, None, Some(seen), Some(now)) = (expect_turn, owed.as_deref(), seen.as_ref(), in_flight.as_ref()) {
        let live_pane_work = seen.is_none() && now.origin == "external";
        if seen.as_deref() != Some(now.id.as_str()) && !live_pane_work {
            tracing::info!(bot = %bot_id, seen = ?seen, now = %now.id, "打斷的請求到達時在飛的那一筆已經結束，接著在飛的是別的回合：不按 Esc");
            return Err(LcError::conflict(
                "turn_not_in_flight",
                json!({"turn_id": seen, "in_flight_turn_id": now.id, "esc_sent": false}),
            ));
        }
    }
    if let Some(t) = in_flight.as_ref().filter(|t| super::interruption::owes(bot_id, &t.id)) {
        return Err(super::interruption::uncommitted(&run.id, &t.id, settled.as_ref().err()));
    }
    let client = client_for_run(app, &run).await?;
    let target = super::dead_panes::execution_target(&client, &run, &bot).await?;
    let esc_at = chrono::Utc::now();
    let sent = client.agent_send_keys(&target, &["esc".to_string()]).await;
    let fate = super::interruption::key_fate(&sent);
    if fate == super::interruption::KeyFate::NotApplied {
        return Err(up(sent.err().map(|e| format!("{e:#}")).unwrap_or_default()));
    }
    app.note_user_interrupt_of(&bot, &run, in_flight.as_ref()).await;
    if fate == super::interruption::KeyFate::Unknown {
        if let Some(t) = in_flight.as_ref() {
            match super::interruption::unconfirmed(app, bot_id, &run.id, &t.id, super::interruption::INTERRUPT_NOTE, esc_at).await {
                Ok(false) => {}
                Ok(true) => {
                    clear_restored_prompt(app, &client, &run, &bot, in_flight.as_ref().and_then(|t| t.prompt_text.as_deref())).await;
                    return Ok(());
                }
                Err(e) => {
                    clear_restored_prompt(app, &client, &run, &bot, in_flight.as_ref().and_then(|t| t.prompt_text.as_deref())).await;
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
            clear_restored_prompt(app, &client, &run, &bot, in_flight.as_ref().and_then(|t| t.prompt_text.as_deref())).await;
            return Err(super::interruption::uncommitted(&run.id, &t.id, Some(&e)));
        }
    }
    clear_restored_prompt(app, &client, &run, &bot, in_flight.as_ref().and_then(|t| t.prompt_text.as_deref())).await;
    Ok(())
}

/// claude-api 的強制中斷：只打這一筆 in-flight 的回合（`expect_turn`／到達當下看到的那一筆，語意同 Esc 路徑），
/// 它已經不在飛就回 409 `turn_not_in_flight`。成功＝runner 已 join、回合已收成 failed（系統訊息「使用者強制中斷…」），run 仍 running。
async fn interrupt_api_turn(
    app: &impl StopContext,
    bot_id: &str,
    run: &db::Run,
    expect_turn: Option<&str>,
    seen: Option<&Option<String>>,
) -> LcResult<()> {
    let in_flight = db::in_flight_turn(app.db(), &run.id).await.map_err(up)?;
    let gone = |want: Option<&str>| {
        LcError::conflict(
            "turn_not_in_flight",
            json!({"turn_id": want, "in_flight_turn_id": in_flight.as_ref().map(|t| t.id.clone()), "esc_sent": false}),
        )
    };
    let Some(turn) = in_flight.as_ref() else { return Err(gone(expect_turn)) };
    if let Some(want) = expect_turn {
        if want != turn.id {
            return Err(gone(Some(want)));
        }
    }
    if let Some(Some(seen)) = seen {
        if *seen != turn.id {
            return Err(gone(Some(seen)));
        }
    }
    let key = crate::lifecycle::api_bot_port::ApiTurnKey { bot_id: bot_id.to_string(), run_id: run.id.clone(), turn_id: turn.id.clone() };
    use crate::lifecycle::api_bot_port::ApiBotError as E;
    match crate::lifecycle::api_bot_port::ApiBotPort::api_turn_interrupt(app, &key).await {
        Ok(crate::lifecycle::api_bot_port::ApiRunCompletion::Completed) => Err(gone(Some(&turn.id))),
        Ok(_) => Ok(()),
        Err(E::IdentityMismatch) => Err(gone(Some(&turn.id))),
        Err(e) => {
            let code = match e { E::Busy => "busy", E::Unconfirmed => "unconfirmed", E::Upstream => "upstream", E::IdentityMismatch => "identity_mismatch" };
            tracing::warn!(bot = bot_id, run = %run.id, turn = %turn.id, error_code = code, "API bot interrupt did not confirm");
            Err(LcError::Upstream(format!("api_bot_interrupt_unconfirmed:{code}")))
        }
    }
}

/// 打斷的請求到達那一刻，這顆 bot 在飛的是哪一筆（沒指名 `turn_id` 時用）。讀取失敗會回 503、不送 Esc。
/// `Ok(Some(None))`＝那一刻沒有「使用者看得到在跑」的回合；`Ok(Some(Some(id)))`＝那一筆。
///
/// 排隊的 prompt 剛被 flush 認領、還在打字時不算：`claimed_from_queue` 記來源，`delivery='pending'` 記還沒送完。
/// 直接送出的回合在打字前也寫 `auto_resend=0`，但來源標記為 0，所以仍是使用者剛送、可以打斷的那一筆。
async fn in_flight_at_arrival(app: &impl crate::capabilities::Db, bot_id: &str) -> LcResult<Option<Option<String>>> {
    let run = db::active_run(app.db(), bot_id).await.map_err(|e| interrupt_arrival_unavailable(bot_id, "active_run", e))?;
    let Some(run) = run else { return Ok(Some(None)) };
    let turn = db::in_flight_turn(app.db(), &run.id).await.map_err(|e| interrupt_arrival_unavailable(bot_id, "in_flight_turn", e))?;
    Ok(Some(turn.filter(|t| !(t.delivery == "pending" && t.claimed_from_queue != 0)).map(|t| t.id)))
}

fn interrupt_arrival_unavailable(bot_id: &str, read: &str, e: impl std::fmt::Display) -> LcError {
    tracing::warn!(bot = %bot_id, read, error = %e, "interrupt arrival snapshot could not be read; not sending Esc");
    LcError::Unavailable(json!({
        "error": "interrupt_state_unavailable", "bot_id": bot_id, "read": read, "retryable": true, "sent": false, "retry_after_secs": 5,
        "message": "按下中斷時讀不到正在執行的回合，沒有送 Esc；稍後重試。", "detail": e.to_string(),
    }))
}

/// claude 在吐出第一個字前被 `esc` 打斷，會把 prompt 放回輸入框（2026-09-08 實測），下一則
/// 貼上會黏在後面。只清這筆被中斷回合的 prompt；草稿不同、讀不到或 run/pane/session 或畫面改變就不動。只做 claude，失敗不報錯。
///
/// 讀法跟送出前的檢查一樣（#581）：以前讀 `visible` 80 列、放回來的長 prompt 撐高的框找不到框頂，
/// 判成「框是空的」沒按，框就一直卡著那段字。按完重讀一次，框沒空就留一筆 log（不再按：空框的 `ctrl+c` 是「再按一次離開」）。
async fn clear_restored_prompt(app: &impl crate::capabilities::Db, client: &HerdrClient, run: &db::Run, bot: &db::Bot, expected_prompt: Option<&str>) {
    use super::delivery::{box_state, read_styled, read_styled_snapshot, BoxState, DELIVER_SCAN_LINES, SCAN_SOURCE};
    if bot.kind != "claude" {
        return;
    }
    let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) else { return };
    let Some(expected_prompt) = expected_prompt.filter(|p| !p.trim().is_empty()) else { return };
    tokio::time::sleep(pane_settle(Duration::from_millis(400))).await;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("abort_after_settle", &bot.id).await;
    if !abort_binding_is_current(app, run).await {
        return;
    }
    let Ok(candidate) = read_styled_snapshot(client, pane, SCAN_SOURCE, DELIVER_SCAN_LINES).await else {
        tracing::warn!(run = %run.id, "interrupt: could not read the pane to look for a restored prompt; nothing cleared");
        return;
    };
    if candidate.pane_id != pane || candidate.source != SCAN_SOURCE {
        return;
    }
    if !restored_prompt_matches(&bot.kind, &candidate.text, expected_prompt) {
        tracing::info!(run = %run.id, state = ?box_state(&bot.kind, &candidate.text),
                       "interrupt: composer holds no exact copy of the interrupted prompt; not clearing");
        return;
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("abort_before_clear_key", &bot.id).await;

    let Ok(fresh) = read_styled_snapshot(client, pane, SCAN_SOURCE, DELIVER_SCAN_LINES).await else { return };
    if fresh.pane_id != pane
        || !super::composer_draft::same_snapshot(&bot.kind, &candidate, &fresh)
        || !restored_prompt_matches(&bot.kind, &fresh.text, expected_prompt)
        || !abort_binding_is_current(app, run).await
    {
        return;
    }
    if let Err(e) = client.pane_send_keys(pane, &["ctrl+c"]).await {
        tracing::warn!(run = %run.id, error = ?e, "could not clear the restored prompt from the composer");
        return;
    }
    tokio::time::sleep(pane_settle(Duration::from_millis(400))).await;
    let after = read_styled(client, pane, SCAN_SOURCE, DELIVER_SCAN_LINES).await.map(|s| box_state(&bot.kind, &s));
    match after {
        Ok(BoxState::Empty) => tracing::info!(run = %run.id, "interrupt put the prompt back into the composer; cleared it"),
        other => tracing::warn!(run = %run.id, state = ?other.ok(), "cleared the restored prompt but the composer did not read back empty"),
    }
}

/// A clear is authorized only by the exact prompt captured from this run's in-flight turn.
/// `recent_unwrapped` and the composer parser handle TUI framing; only try a tab-removed candidate
/// when the original prompt actually had a tab (the known Claude TUI tab-loss behavior).
fn restored_prompt_matches(kind: &str, screen: &str, expected: &str) -> bool {
    let Some(actual) = super::poller::composer_text_whole(kind, screen) else { return false };
    same_restored_prompt(&actual, expected)
}

fn same_restored_prompt(actual: &str, expected: &str) -> bool {
    let actual = super::pasted_content::original(actual.trim());
    let expected = super::pasted_content::original(expected.trim());
    if actual == expected {
        return true;
    }
    expected.contains('\t') && actual == expected.replace('\t', "")
}

async fn abort_binding_is_current(app: &impl crate::capabilities::Db, run: &db::Run) -> bool {
    let Ok(Some(current)) = db::active_run(app.db(), &run.bot_id).await else { return false };
    current.id == run.id
        && current.state == "running"
        && current.pane_id == run.pane_id
        && current.herdr_session == run.herdr_session
        && current.native_session_id == run.native_session_id
        && current.transcript_path == run.transcript_path
}

/// 強制中止在對話裡留的那一則：一次收好幾筆時帶筆數（#581）。
fn abort_note(count: usize) -> String {
    if count > 1 {
        format!("回合已由使用者強制中止（共 {count} 筆）")
    } else {
        "回合已由使用者強制中止".to_string()
    }
}

/// 強制結束目前回合（`POST /api/bots/:id/abort`）。和 [`interrupt_turn`] 相反，先保證 DB 解開、
/// 送 `esc` 只是盡力（`keys_sent`）：in-flight 標 failed、`delivery = unknown` 也一併收（§6.3，
/// 同樣鎖輸入框）。沒有 active run 不算錯——那正是最需要這支的情況。
pub async fn abort_turns(app: &impl StopContext, bot_id: &str) -> LcResult<Value> {
    let _g = app.lock_bot_for_stop(bot_id).await;
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?;
    if let Err(e) = super::interruption::settle_locked(app, bot_id, super::interruption::Evidence::Nothing).await {
        tracing::warn!(bot = %bot.name, error = %e, "上一次打斷欠著的收尾還是寫不進去");
    }

    let in_flight = match run.as_ref() {
        Some(r) => db::in_flight_turn(app.db(), &r.id).await.map_err(up)?,
        None => None,
    };

    let mut keys_sent = false;
    let mut key_error: Option<String> = None;
    let mut client: Option<HerdrClient> = None;
    if let Some(r) = run.as_ref() {
        match client_for_run(app, r).await {
            Ok(c) => match async {
                let target = super::dead_panes::execution_target(&c, r, &bot).await
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                c.agent_send_keys(&target, &["esc".to_string()]).await
            }.await {
                Ok(()) => {
                    keys_sent = true;
                    client = Some(c.client.clone());
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
        app.note_user_interrupt_of(&bot, r, in_flight.as_ref()).await;
        if let Some(t) = &in_flight {
            let more: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id = t.conversation_id
                 WHERE c.bot_id = ? AND t.id <> ? AND (t.status = 'in_flight' OR t.delivery = 'unknown')",
            )
            .bind(bot_id)
            .bind(&t.id)
            .fetch_one(app.db())
            .await
            .map_err(up)?;
            let note = abort_note(1 + usize::try_from(more).unwrap_or(0));
            if let Err(e) = super::interruption::interrupted(app, bot_id, &r.id, &t.id, &note).await {
                return Err(LcError::Upstream(format!("回合 {} 沒收成（稍後自動補上）：{e:#}", t.id)));
            }
        }
        if let Some(c) = client.as_ref() {
            clear_restored_prompt(app, c, r, &bot, in_flight.as_ref().and_then(|t| t.prompt_text.as_deref())).await;
        }
    }
    let unknown = sqlx::query_as::<_, db::Turn>(
        "SELECT t.* FROM turns t JOIN conversations c ON c.id = t.conversation_id
         WHERE c.bot_id = ? AND (t.status = 'in_flight' OR t.delivery = 'unknown')",
    )
    .bind(bot_id)
    .fetch_all(app.db())
    .await
    .map_err(up)?;
    let turns: Vec<db::Turn> = unknown.into_iter().filter(|t| !(aborted.contains(&t.id) && t.delivery != "unknown")).collect();
    if !turns.is_empty() {
        let mut tx = app.db().begin().await.map_err(up)?;
        for t in &turns {
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
        }
        let fresh: Vec<&db::Turn> = turns.iter().filter(|t| !aborted.contains(&t.id)).collect();
        let note = match fresh.first() {
            Some(first) if in_flight.is_none() => Some(
                insert_message_tx(&mut tx, &first.conversation_id, Some(&first.id), "system", &abort_note(fresh.len()), "system", false, None)
                    .await
                    .map_err(up)?,
            ),
            _ => None,
        };
        tx.commit().await.map_err(up)?;
        if let Some(m) = note {
            app.publish_lifecycle_event("message_added", json!({ "bot_id": bot_id, "message": m })).await;
        }
        aborted.extend(fresh.iter().map(|t| t.id.clone()));
        for t in &turns {
            app.turn_changed(&t.id).await;
        }
    }

    tracing::info!(bot = %bot.name, keys_sent, aborted = aborted.len(), "turn(s) force-aborted by user");
    Ok(json!({"aborted": aborted, "keys_sent": keys_sent, "key_error": key_error}))
}

/// Give a running pre one-bot-one-tab bot its own tab. A move, not a restart: herdr keeps
/// `pane_id` across `pane.move` (0.8.2), so mapping, poller and in-flight turn carry on.
/// Idempotent: re-moving a solo pane would rebuild a tab and renumber the user's tab bar.
pub async fn move_pane_to_own_tab(app: &impl StopContext, bot_id: &str) -> LcResult<()> {
    let _g = app.lock_bot_for_stop(bot_id).await;
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let pane_id = run
        .pane_id
        .clone()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| LcError::NotFound("pane".into()))?;
    let client = client_for_run(app, &run).await?;

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
        if !previous.is_empty() && previous != new_tab {
            close_tab_if_empty(&client, &workspace_id, &previous).await;
        }
        new_tab
    };

    sqlx::query("UPDATE runs SET workspace_id = ?, tab_id = ? WHERE id = ?")
        .bind(&workspace_id)
        .bind(&tab_id)
        .bind(&run.id)
        .execute(app.db())
        .await
        .map_err(up)?;
    app.notify_bot_status(bot_id).await;
    Ok(())
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
#[path = "force_abort_tests.rs"]
pub mod force_abort_tests;

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    use super::*;
}
