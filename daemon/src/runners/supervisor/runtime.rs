//! App-backed supervisor controller loop and lifecycle adapters.

use crate::lifecycle::{self, LcError};
use crate::state::{App, TurnEvent};
use crate::capabilities::{Cfg, Db, Emit};
use crate::supervisor::controller::{
    apply_quota_policy, backfill_quota_limits, block_stale_queues, current, drain_queue, is_manager,
    iso_in, late_reply_for_turn, note_classify_result, notify, on_turn_done, past, quota_reset_at,
    reconcile, recover_unacked, resume_quota_blocked, MAX_AUTO_SWITCHES, SWITCH_COOLDOWN_SECS,
};
use crate::supervisor::ports::{HostProbes, JudgeOps, TurnOps};
use crate::supervisor::store;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------- the loop

/// The generation whose controller loop was spawned last (`-1` = none yet).
pub(crate) static LIVE_GENERATION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

/// A failed generation read must not turn into a retirement decision or a tight DB retry loop.
const CURRENT_READ_RETRY: Duration = Duration::from_secs(2);

#[cfg(test)]
pub(crate) static TEST_PANIC_CONTROLLER_GENERATION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

#[cfg(test)]
pub(crate) static TEST_CONTROLLER_ENTRIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static TEST_CURRENT_READ_FAILURE_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const TICK: Duration = Duration::from_secs(10);

struct LiveGenerationGuard {
    generation: i64,
}

impl Drop for LiveGenerationGuard {
    fn drop(&mut self) {
        // A retiring older loop must never release the latch installed by a newer generation.
        let _ = LIVE_GENERATION.compare_exchange(self.generation, -1, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Start the controller for `generation`. An older controller notices the mismatch on its
/// next tick and stops, so a model switch never leaves two of them sending notifications.
pub(crate) fn spawn(app: Arc<App>, generation: i64) {
    if app.shutdown.is_cancelled() {
        return;
    }
    // One loop per generation. `start` after a `stop` keeps the generation, and the watchdog
    // goes through the same start path: without this every restart added a loop.
    if LIVE_GENERATION.swap(generation, std::sync::atomic::Ordering::SeqCst) == generation {
        return;
    }
    let latch = LiveGenerationGuard { generation };
    let loop_app = app.clone();
    crate::background_loop::spawn_named(&app.background_tasks, "supervisor controller", async move {
        let _latch = latch;
        let supervisor = loop_app.clone();
        supervisor.restart_loop("supervisor controller", move || {
            let app = loop_app.clone();
            async move { controller_loop(app, generation).await }
        }).await;
    });
}

async fn controller_loop(app: Arc<App>, generation: i64) {
        #[cfg(test)]
        TEST_CONTROLLER_ENTRIES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        if TEST_PANIC_CONTROLLER_GENERATION
            .compare_exchange(generation, -1, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst)
            .is_ok()
        {
            panic!("test-injected supervisor controller panic");
        }
        let mut turns = app.subscribe_turns();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 「還在等額度」那幾格不在這裡回填：這裡每次換 generation 都會跑，而且身份還沒偵測完、key 算不準。
        // 回填掛在 `tools::detect` 之後（`backfill_quota_limits_once`）；回填之前 `resume_quota_blocked`
        // 照樣不會把 `resume_at` 還沒到的交辦放出去。
        // Startup reconciliation: results that arrived while the daemon was down.
        reconcile_boxed(&app).await;
        loop {
            match current(&app.db, generation).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!(generation, "supervisor controller retiring: newer generation took over");
                    return;
                }
                Err(error) => {
                    #[cfg(test)]
                    TEST_CURRENT_READ_FAILURE_SEEN.store(true, std::sync::atomic::Ordering::SeqCst);
                    tracing::warn!(error = ?error, generation, "supervisor generation could not be read; skipping this controller tick");
                    tokio::select! {
                        _ = app.shutdown.cancelled() => return,
                        _ = tokio::time::sleep(CURRENT_READ_RETRY) => {}
                    }
                    continue;
                }
            }
            tokio::select! {
                _ = app.shutdown.cancelled() => return,
                ev = turns.recv() => match ev {
                    Ok(ev) => handle_turn_event(&app, ev).await,
                    // Lagged: the bus dropped events, so fall back on the database, which
                    // never does.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "supervisor missed turn events; reconciling from the database");
                        reconcile_boxed(&app).await;
                    }
                    Err(_) => return,
                },
                _ = tick.tick() => tick_once(&app).await,
            }
        }
}

/// 控制迴圈裡「一段」的 future 型別：在各段自己的函式框裡建好、裝箱，呼叫端的 poll 框只留一個指標（#1465）。
type Boxed<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

/// 一拍（`TICK` 到了）要做的全部事。從迴圈拆出來，守門測試才能直接在 1 MiB 的執行緒上跑一拍（#1465）。
pub(crate) fn tick_once(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(async move {
        // issue #473：每一段與整拍各記一次耗時。`timing::seg` 只是在原地 await 同一個
        // future，順序、鎖的範圍一個字都沒變；超標才寫 log（見 `supervisor/timing.rs`）。
        //
        // 每一段各是一支函式（#1465）：debug 建置下 async fn 的 poll 框會替裡面每個 await 的 future 各留一份暫存，
        // 二十幾段全寫在這裡，這個迴圈的 poll 框就有 900 KiB 以上，光是被 poll 一次就吃掉 1 MiB 執行緒的絕大部分。
        // 分成函式、future 在各自的框裡建好再裝箱，這裡只留一個指標；順序與每段的內容不變。
        let tick_started = std::time::Instant::now();
        tick_quota_policy(app).await;
        tick_reconcile(app).await;
        tick_resume_quota_blocked(app).await;
        tick_block_stale_queues(app).await;
        // 丟背景（issue #480）：一輪要做 herdr 讀畫面與外部 Jev 呼叫，await 會拖住整拍。
        // timing 的區段留著（i406 的量測靠它列齊每一段），現在量到的是「派出去」本身，
        // 幾乎是 0——那就是實話，這一段不再佔 tick 的時間了。
        super::timing::seg(app.as_ref(), "judge_stuck_sweep", async { app.as_ref().judge_stuck_sweep() }).await;
        tick_drain_queue(app).await;
        // Before pushing anything new: give back the notifications that went out
        // and were never answered. A delivered event nobody acked is still owed.
        tick_recover_unacked(app).await;
        tick_roles(app).await;
        tick_failover(app).await;
        tick_notify(app).await;
        tick_responder_notify(app).await;
        tick_watchdog(app).await;
        tick_responder_watchdog(app).await;
        // 閒置太久的 bot 收起來省 RAM（§6.11）。巡邏自己節流成每分鐘一次，
        // 而且丟到背景跑——停一顆最久要等 agent 十秒，不能卡住這條迴圈。
        super::timing::seg(app, "idle_sleep", async { super::idle_sleep::tick(app) }).await;
        // 主力 bot 的 prompt cache 保溫（58 分）與熱壓（110 分），SPEC §6.5k。自己節流成 30 秒一次、丟背景跑。
        super::timing::seg(app.as_ref(), "primary_keep_warm", async { app.as_ref().primary_keep_warm_tick() }).await;
        super::timing::note_tick(app.as_ref(), tick_started.elapsed()).await;
    })
}

fn reconcile_boxed(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(reconcile(app.as_ref()))
}

/// 一則回合事件：回合結束就結案、補遲到的回覆。
fn handle_turn_event(app: &Arc<App>, ev: TurnEvent) -> Boxed<'_> {
    Box::pin(async move {
        if ev.is_done() {
            // Ignore the manager's own turns: a notification about its own
            // reply is how you build an agent that talks to itself forever.
            if !is_manager(&app.db, &ev.bot_id).await {
                on_turn_done(app.as_ref(), &ev.turn_id, &ev.status).await;
                late_reply_for_turn(app.as_ref(), &ev.turn_id, &ev.status).await;
            }
        }
    })
}

fn tick_quota_policy(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app.as_ref(), "quota_policy", async move {
        let _g = super::lock().await;
        match apply_quota_policy(app.as_ref()).await {
            // Mid-turn: the next tick asks again.
            Err(LcError::Conflict(_)) | Ok(_) => {}
            Err(e) => tracing::warn!(error = ?e, "supervisor quota policy failed"),
        }
    }))
}

fn tick_reconcile(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app.as_ref(), "reconcile", reconcile(app.as_ref())))
}

fn tick_resume_quota_blocked(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app.as_ref(), "resume_quota_blocked", resume_quota_blocked(app.as_ref())))
}

fn tick_block_stale_queues(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app.as_ref(), "block_stale_queues", block_stale_queues(app.as_ref())))
}

fn tick_drain_queue(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app.as_ref(), "drain_queue", drain_queue(app.as_ref())))
}

fn tick_recover_unacked(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app.as_ref(), "recover_unacked", recover_unacked(app.as_ref())))
}

/// 先分角色、合併重複，再各自決定要不要叫醒（roles.rs）。沒有要叫醒的事件時，
/// 這一段只讀寫資料庫，不開任何模型回合。
fn tick_roles(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(async move {
        // 失敗要記，而且**這一拍照跑下去**（#472）：這兩支失敗時，事件會留在
        // `role IS NULL`，而 `due_for` 三個分支全都比 `roles::OWNER`，
        // 所以那些事件對兩個通知者同時隱形——沒人被叫醒、`notify_attempts` 也不會累積
        // （走不到 `notify_exhausted`）。以前這裡是 `let _ =`，連一行 log 都沒有。
        // 整拍不因此中止：後面幾段（watchdog、idle_sleep）跟分類無關，
        // 為了一個分類失敗把它們一起停掉只會多壞一件事。
        note_classify_result(app.as_ref(), super::timing::seg(app.as_ref(), "roles_classify", super::roles::classify(&app.db)).await);
        if let Err(e) = super::timing::seg(app.as_ref(), "roles_coalesce_patrol", super::roles::coalesce_patrol(&app.db)).await {
            // 合併失敗只是「重複的 health_changed 沒被收掉」，事件本身照樣送得出去，
            // 所以記一行就好，不進 incident。
            tracing::warn!(error = ?e, "巡檢的重複事件沒合併成功（roles::coalesce_patrol 失敗）");
        }
    })
}

/// issue #421：協調者不可用而核准等超過 5 分鐘 → 改派給巡檢並叫醒它。
/// 放在 notify 之前：改派完這一拍就由巡檢的 notify 送出去，不必再等一拍。
fn tick_failover(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(async move {
        let unavailable = super::timing::seg(app.as_ref(), "responder_unavailable", super::failover::responder_unavailable(app.as_ref())).await;
        super::timing::seg(app.as_ref(), "reassign_stale_approvals", super::failover::reassign_stale_approvals(app.as_ref(), unavailable)).await;
    })
}

fn tick_notify(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app.as_ref(), "notify", notify(app.as_ref())))
}

fn tick_responder_notify(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app.as_ref(), "responder_notify", super::responder::notify(app.as_ref())))
}

fn tick_watchdog(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app, "watchdog", super::watchdog::tick(app)))
}

fn tick_responder_watchdog(app: &Arc<App>) -> Boxed<'_> {
    Box::pin(super::timing::seg(app, "responder_watchdog", super::responder::watchdog_tick(app)))
}

/// 開機讀不到 supervisor 列時的重試間隔，最後一個之後一直用最後一個（#300）。
const RESPAWN_RETRY: [Duration; 4] = [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(60)];

/// Called once from `serve`: bring the controller back for whatever generation is on disk.
///
/// 讀不到（DB 開機當下忙一次）不能就此放棄：這顆 controller 起不來，AGM 的派送、驗收通知、額度重送整個行程期間都不動，
/// 又沒有任何 log。記一行，背景重試到讀得到為止（#300）。`spawn` 自己一個 generation 只起一條迴圈，重試不會疊。
pub(crate) async fn respawn(app: &Arc<App>) {
    respawn_with(app, &RESPAWN_RETRY).await
}

pub(crate) async fn respawn_with(app: &Arc<App>, delays: &'static [Duration]) {
    match store::get_or_init(&app.db).await {
        Ok(sup) => {
            if sup.bot_id.is_some() {
                spawn(app.clone(), sup.generation);
            }
        }
        Err(e) => {
            tracing::warn!(error = ?e, "cannot read the supervisor row at startup; the AGM controller will be retried in the background");
            let loop_app = app.clone();
            app.spawn_restartable("supervisor controller startup retry", move || {
                let app = loop_app.clone();
                let shutdown = app.shutdown.clone();
                async move {
                    for attempt in 0usize.. {
                        tokio::select! {
                            _ = shutdown.cancelled() => return,
                            _ = tokio::time::sleep(delays[attempt.min(delays.len() - 1)]) => {}
                        }
                        match store::get_or_init(&app.db).await {
                            Ok(sup) => {
                                tracing::info!(attempt, "supervisor row readable again; starting the AGM controller");
                                if sup.bot_id.is_some() {
                                    spawn(app.clone(), sup.generation);
                                }
                                return;
                            }
                            Err(e) => tracing::warn!(attempt, error = ?e, "supervisor row still unreadable; will retry"),
                        }
                    }
                }
            });
        }
    }
}

/// Called after a host's identity table is populated; each host is backfilled once per process.
pub(crate) async fn backfill_quota_limits_once(app: &Arc<App>, host: &str) {
    static DONE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    let key = format!("{}\u{0}{host}", app.data_dir.display());
    if !DONE.get_or_init(Default::default).lock().unwrap().insert(key.clone()) {
        return;
    }
    let started = chrono::DateTime::parse_from_rfc3339(&crate::build_info::started_at()).ok().map(|t| t.with_timezone(&chrono::Utc));
    if let Err(error) = backfill_quota_limits(app, host, started).await {
        tracing::warn!(host, error = ?error, "開機回填讀不到它要的東西：這台主機這一輪不算回填完，稍後重跑");
        DONE.get_or_init(Default::default).lock().unwrap().remove(&key);
        retry_backfill_quota_limits(app, host);
    }
}

fn retry_backfill_quota_limits(app: &Arc<App>, host: &str) {
    if cfg!(test) || app.shutdown.is_cancelled() {
        return;
    }
    let (app, host) = (app.clone(), host.to_string());
    let loop_app = app.clone();
    app.spawn_restartable("supervisor quota backfill retry", move || {
        let app = loop_app.clone();
        let host = host.clone();
        async move {
            tokio::select! {
                _ = app.shutdown.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            tokio::select! {
                _ = app.shutdown.cancelled() => return,
                _ = backfill_quota_limits_once(&app, &host) => {}
            }
        }
    });
}



// ---------------------------------------------------------------- candidate switching

/// Switch the manager to `next`. `Ok(false)` = refused, with the reason already recorded.
/// The caller holds [`super::lock`].
///
/// Bounded on purpose: `cc0` is a single account, so both candidates share one quota pool. If
/// the first switch does not help, a second one is not going to, and an unbounded loop would
/// just burn the account's remaining capacity flapping between two models.
///
/// Only an idle manager (no turn in flight) or a stopped one is switched: `/model` lands in
/// the input line, and mid-turn it eats the user's next message (b95142a). A busy manager gets
/// 409 `busy`; the controller simply asks again next tick. When the live `/model` does not
/// take on an idle session (the confirmation would not close, the gate refused), the session
/// is restarted on the new model rather than left running the old one under a record that
/// says otherwise.
pub async fn switch_candidate(
    app: &Arc<App>,
    next: &str,
    reason: &str,
    reset_at: Option<&str>,
) -> Result<bool, LcError> {
    let sup = store::get_or_init(&app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let Some(bot_id) = sup.bot_id.clone() else {
        return Err(LcError::conflict("supervisor is not set up", json!({"reason": "not_configured"})));
    };
    if sup.active_model == next {
        return Ok(false);
    }
    let liveness = super::manager_liveness(&app.db(), &bot_id).await?;
    if !matches!(liveness, "idle" | "stopped") {
        return Err(LcError::conflict(
            "the supervisor is mid-turn; a model switch now would eat its next message",
            json!({"reason": "busy", "liveness": liveness}),
        ));
    }
    // The window is over: this is a genuinely new failure, so it gets its own budget.
    if sup.cooldown_until.as_deref().is_some_and(past) {
        let _ = store::clear_fallback_budget(&app.db()).await;
    }
    let sup = store::get_or_init(&app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if sup.fallback_tries >= MAX_AUTO_SWITCHES && sup.cooldown_until.as_deref().is_some_and(|t| !past(t)) {
        // Both candidates have now been tried inside one window. Say so instead of promising
        // that the next switch will find quota that is not there.
        // Record when the account's window is said to reset, if the poller knows. Unknown
        // stays unknown: an absent reading is not "available again now".
        let _ = store::set_quota_reset(&app.db(), quota_reset_at(app, &sup.identity).await.as_deref()).await;
        let _ = store::set_status(
            &app.db(),
            "waiting_quota",
            Some("cc0 的兩個候選都試過了，等額度恢復再重試（不會再自動切換）"),
        )
        .await;
        app.emit("supervisor_changed", json!({"status": "waiting_quota"})).await;
        return Ok(false);
    }

    let next = super::setup::candidate(next);
    let model = super::setup::model_arg(next).to_string();
    // Config first, so a restart comes up on the new candidate even if the live switch fails.
    #[cfg(test)]
    crate::lifecycle::race_point::hit("supervisor_switch_before_config", &bot_id).await;
    let bid = bot_id.clone();
    let m2 = model.clone();
    let effort = sup.effort.clone();
    // 寫不進去就整個不算數（#1646）：不套用、不記 active_model，下一輪 tick 還會重新決定、再試。
    crate::projection::update_and_project(&app.cfg(), &app.db(), move |cfg| {
        for p in cfg.projects.iter_mut() {
            if let Some(b) = p.bots.iter_mut().find(|b| b.id.as_deref() == Some(bid.as_str())) {
                b.model = Some(m2.clone());
                // Effort is re-asserted with every switch: `low` is fixed by the plan, and
                // a model change is exactly where it would otherwise be forgotten.
                b.effort = Some(effort.clone());
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| LcError::Upstream(format!("could not record the model switch in config.toml; not switched, will retry: {e:#}")))?;

    // Apply it to the session that is already running, if there is one. `send_slash_line`
    // answers claude's "Switch model?" confirmation and backs out (Err → false) if it will
    // not close, so `applied` means the session really is on the new model.
    let applied =
        liveness == "idle" && lifecycle::apply_live_setting(app, &bot_id, &["model", "effort"]).await.is_none();
    let gen = store::set_active_model(&app.db(), next, Some(&iso_in(SWITCH_COOLDOWN_SECS)))
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    let _ = store::set_quota_reset(&app.db(), reset_at).await;
    let mut restarted = false;
    if !applied && liveness == "idle" {
        // Still idle? Then a restart costs only the session, and a manager running the old
        // model under a record that says the new one is the worse outcome.
        if super::manager_liveness(&app.db(), &bot_id).await.unwrap_or("busy") == "idle" {
            if let Err(e) = app.stop_bot(&bot_id).await {
                tracing::warn!(error = ?e, "could not stop the supervisor to apply the model switch");
            }
            match super::start_manager(app, Some(&format!("switched to {next}: {reason}（重啟套用）"))).await {
                Ok(()) => restarted = true,
                // `desired_running` is still set: the watchdog brings it back on the new model.
                Err(e) => tracing::warn!(error = ?e, "restart after the model switch failed; leaving it to the watchdog"),
            }
        }
    }
    if !restarted {
        let _ = store::set_status(&app.db(), "", Some(&format!("switched to {next}: {reason}"))).await;
    }
    tracing::info!(model = next, reason, applied, restarted, "supervisor candidate switched");
    app.emit("supervisor_changed", json!({"model": next, "generation": gen, "applied_live": applied, "restarted": restarted})).await;
    spawn(app.clone(), gen);
    Ok(true)
}

#[cfg(test)]
mod switch_write_tests {
    use super::*;
    use crate::testing as tt;

    /// 切換模型時 config.toml 寫不進去（這裡在寫入前把檔案換成寫到一半的內容）：不能照樣記成「已切換」。
    /// 以前 `let _ =` 吞掉錯誤，`active_model` 還是先記成新模型，之後的 tick 看到它已經是新的就不再重試，
    /// 而重啟讀的是 config 裡還停在舊模型的那一份。
    #[tokio::test]
    async fn a_switch_whose_config_write_fails_is_not_recorded_as_done() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "agm-mgr").await;
        store::get_or_init(&e.app.db).await.unwrap();
        store::set_env(&e.app.db, &bot.id, &e.project_id, "/tmp").await.unwrap();
        let path = e.app.cfg.path.clone();
        crate::lifecycle::race_point::arm("supervisor_switch_before_config", &bot.id, move || async move {
            std::fs::write(&path, "[[[ 寫到一半的設定").unwrap();
        });

        let result = switch_candidate(&e.app, "opus", "test", None).await;
        assert!(result.is_err(), "config 寫不進去，這次切換不能算數：{result:?}");
        assert_eq!(store::get_or_init(&e.app.db).await.unwrap().active_model, "fable", "沒切成，下一輪 tick 還會再決定、再試");
    }
}
