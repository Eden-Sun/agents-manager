//! Bring the manager back when it dies.
//!
//! 2026-09-10 23:02Z: AGM was killed and nothing started it again for five and a half hours —
//! the one bot whose job is to notice a bot is down was the bot that was down. So the daemon
//! watches it. The rule is narrow on purpose: only a manager that is *wanted* (started once by
//! a human, not stopped since through `POST /supervisor/stop`) and not parked on
//! `waiting_quota` is restarted; a stop the user asked for stays stopped, and a fresh `setup`
//! stays down until someone starts it.
//!
//! Every decision is pure ([`plan`]) over the supervisor row plus the manager's liveness, so
//! the loop in `controller` just executes it. State lives in the row (`watchdog_attempts`,
//! `watchdog_next_at`), not in the task: two controller loops of the same generation see the
//! same count, and a daemon restart does not reset a failure streak to zero.

use crate::state::App;
use std::sync::Arc;

use super::store::{self, Supervisor};

/// Seconds between one failed start and the next. Flat after the fourth: a manager that will
/// not come up is retried every five minutes until [`MAX_ATTEMPTS`], not forever.
pub const BACKOFF_SECS: [u64; 4] = [30, 60, 120, 300];
/// Consecutive failed automatic starts before the watchdog reports and stops.
pub const MAX_ATTEMPTS: i64 = 5;

pub fn backoff_secs(failures: i64) -> u64 {
    let i = (failures.max(0) as usize).min(BACKOFF_SECS.len() - 1);
    BACKOFF_SECS[i]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to do: not wanted, not stopped, or parked on quota.
    Idle,
    /// Stopped and wanted, but the backoff has not run out yet. `schedule` = the next attempt
    /// still has to be written down (first observation of the outage).
    Wait { schedule: bool },
    /// The backoff ran out: start it now.
    Start,
    /// [`MAX_ATTEMPTS`] failures in a row: reported, no further retries until a human starts it.
    GaveUp,
}

/// `liveness` is [`super::manager_liveness`]: `stopped` | `starting` | `busy` | `idle`.
pub fn plan(sup: &Supervisor, liveness: &str, now_past: impl Fn(&str) -> bool) -> Plan {
    plan_for(
        Watched {
            configured: sup.bot_id.is_some(),
            wanted: sup.wants_running(),
            waiting_quota: sup.status == "waiting_quota",
            attempts: sup.watchdog_attempts,
            next_at: sup.watchdog_next_at.as_deref(),
        },
        liveness,
        now_past,
    )
}

/// 看門狗要看的那幾個欄位。巡檢的在 `supervisors`、協調者的在 `supervisor_roles`，規則同一套。
#[derive(Debug, Clone, Copy)]
pub struct Watched<'a> {
    pub configured: bool,
    pub wanted: bool,
    pub waiting_quota: bool,
    pub attempts: i64,
    pub next_at: Option<&'a str>,
}

pub fn plan_for(w: Watched<'_>, liveness: &str, now_past: impl Fn(&str) -> bool) -> Plan {
    if !w.configured || !w.wanted || w.waiting_quota {
        return Plan::Idle;
    }
    if liveness != "stopped" {
        return Plan::Idle;
    }
    if w.attempts >= MAX_ATTEMPTS {
        return Plan::GaveUp;
    }
    match w.next_at {
        None => Plan::Wait { schedule: true },
        Some(t) if !now_past(t) => Plan::Wait { schedule: false },
        Some(_) => Plan::Start,
    }
}

pub(super) fn iso_in(secs: u64) -> String {
    crate::db::iso_in(secs as i64)
}

pub(super) fn past(iso: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => t <= chrono::Utc::now(),
        Err(_) => true,
    }
}

/// One controller tick's worth of watching.
pub async fn tick(app: &Arc<App>) {
    let Ok(sup) = store::get_or_init(&app.db).await else { return };
    let Some(bot_id) = sup.bot_id.clone() else { return };
    // 讀不到 liveness ＝ 不知道，不是「已停止」：不排程、不計次、不動任何 watchdog 狀態，下一個 tick 再看（#249）。
    let liveness = match super::manager_liveness(app, &bot_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, "supervisor watchdog cannot read manager liveness; skipping this tick");
            return;
        }
    };
    match plan(&sup, liveness, past) {
        Plan::Idle => {
            // Seen answering prompts again: the streak is over. `starting` is not enough — a
            // CLI that opens and dies would otherwise reset the count on every attempt.
            if matches!(liveness, "idle" | "busy") && (sup.watchdog_attempts > 0 || sup.watchdog_next_at.is_some()) {
                let _ = store::set_watchdog(&app.db, 0, None).await;
            }
        }
        Plan::Wait { schedule: true } => {
            let wait = backoff_secs(sup.watchdog_attempts);
            tracing::info!(attempts = sup.watchdog_attempts, wait, "supervisor is down and wanted; scheduling an automatic start");
            let _ = store::set_watchdog(&app.db, sup.watchdog_attempts, Some(&iso_in(wait))).await;
        }
        Plan::Wait { schedule: false } => {}
        // The manager is down, wanted, and out of automatic retries. Before 2026-09-13 this
        // branch did *nothing*: the give-up was re-derived silently on every tick and nobody
        // was ever told. The worst case is the one that produced it — the fifth start returns
        // Ok and the CLI dies a second later, so no error path runs at all.
        Plan::GaveUp => {
            let why = sup
                .watchdog_last_error
                .clone()
                .unwrap_or_else(|| format!("自動啟動 {MAX_ATTEMPTS} 次後仍未持續存活（最後一次啟動有回 Ok，但 CLI 隨即結束）"));
            report_gave_up(app, &why).await;
        }
        Plan::Start => start(app, &bot_id, sup.watchdog_attempts).await,
    }
}

/// Say once, durably, that the watchdog has stopped trying.
///
/// Three channels because they answer different questions: `status_detail` for anyone looking
/// at the supervisor now, an SSE event for an open UI, and an **inbox event** so AGM finds out
/// even though the thing that would normally tell it is the thing that is down — the routing table
/// hands `watchdog_gave_up` to the responder, the half that is still up (`roles::route`). The write is
/// Every `GaveUp` tick lands here: the marker means "owed a notification", not "notified" (#251).
/// The inbox event is the durable half, keyed by the persisted marker time and inserted with
/// `INSERT OR IGNORE`, so retrying each tick until it lands costs nothing and yields exactly one
/// event per give-up episode. `status_detail`, the log line and the SSE event are best-effort and
/// ride along with the tick that actually inserts the event.
async fn report_gave_up(app: &Arc<App>, why: &str) {
    let _ = store::mark_watchdog_gave_up(&app.db, why).await;
    // The key comes from the persisted marker, never from a degraded read: no marker or an
    // unreadable row means "try again next tick", not an empty-key event.
    let Some(at) = store::get_or_init(&app.db).await.ok().and_then(|s| s.watchdog_gave_up_at) else { return };
    // One durable event per give-up. The key is the moment it happened, so a later outage
    // (after a recovery clears the marker) is a new event rather than a silenced duplicate.
    let pushed = store::push_inbox(
        &app.db,
        &format!("watchdog:gave_up:{at}"),
        "watchdog_gave_up",
        None,
        None,
        None,
        &serde_json::json!({
            "attempts": MAX_ATTEMPTS,
            "why": why,
            "gave_up_at": at,
            "action": "手動 `bin/agm supervisor-start`；自動重試不會再發生，直到有人重新啟動它",
        }),
    )
    .await;
    // Err = not durable yet, retry next tick; Ok(None) = already delivered on an earlier tick.
    let Ok(Some(_)) = pushed else { return };
    tracing::error!(attempts = MAX_ATTEMPTS, why, "supervisor watchdog gave up; the manager stays down");
    let _ = store::set_status_detail(
        &app.db,
        Some(&format!(
            "watchdog 連續 {MAX_ATTEMPTS} 次自動啟動後仍沒有活著，已停止重試；請手動 supervisor-start。原因：{why}"
        )),
    )
    .await;
    app.emit("supervisor_changed", serde_json::json!({"watchdog": "gave_up", "why": why})).await;
}

async fn start(app: &Arc<App>, bot_id: &str, failures: i64) {
    let _g = super::lock().await;
    // Re-read under the lock: `start`, `stop` or a bulk restart may have moved it meanwhile.
    let Ok(sup) = store::get_or_init(&app.db).await else { return };
    let Ok(liveness) = super::manager_liveness(app, bot_id).await else { return };
    if plan(&sup, liveness, past) != Plan::Start {
        return;
    }
    let attempt = failures + 1;
    let detail = format!("watchdog 自動重新啟動（第 {attempt} 次）");
    match super::start_manager(app, Some(&detail)).await {
        Ok(()) => {
            tracing::info!(attempt, "supervisor watchdog started the manager");
            // Counted until it is seen idle/busy: a CLI that dies right after opening keeps
            // climbing the backoff instead of restarting every 30 seconds.
            let _ = store::set_watchdog(&app.db, attempt, Some(&iso_in(backoff_secs(attempt)))).await;
        }
        Err(e) => {
            let why = format!("{e:?}");
            tracing::warn!(attempt, error = %why, "supervisor watchdog failed to start the manager");
            if attempt >= MAX_ATTEMPTS {
                let _ = store::set_watchdog(&app.db, attempt, None).await;
                // Same reporting as the "started but died" path: one durable event, once.
                report_gave_up(app, &why).await;
            } else {
                let _ = store::set_watchdog(&app.db, attempt, Some(&iso_in(backoff_secs(attempt)))).await;
                let _ = store::set_status_detail(&app.db, Some(&format!("watchdog 自動啟動失敗（第 {attempt} 次）：{why}"))).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sup(wanted: bool, status: &str, attempts: i64, next: Option<&str>) -> Supervisor {
        Supervisor {
            id: "AGM".into(),
            bot_id: Some("b1".into()),
            project_id: None,
            cwd: None,
            identity: "cc0".into(),
            effort: "low".into(),
            active_model: "fable".into(),
            generation: 1,
            status: status.into(),
            status_detail: None,
            fallback_tries: 0,
            cooldown_until: None,
            quota_reset_at: None,
            remote_status: "unknown".into(),
            remote_url: None,
            summary: None,
            summary_version: 0,
            desired_running: i64::from(wanted),
            watchdog_attempts: attempts,
            watchdog_next_at: next.map(String::from),
            last_notify_at: None,
            persona_text: None,
            persona_version: 0,
            persona_hash: None,
            persona_source: None,
            persona_updated_at: None,
            persona_seed_hash: None,
            remote_source: None,
            remote_observed_at: None,
            remote_session_id: None,
            remote_actor: None,
            remote_evidence: None,
            watchdog_gave_up_at: None,
            watchdog_last_error: None,
            created_at: "now".into(),
            updated_at: "now".into(),
        }
    }

    #[test]
    fn the_backoff_is_30_60_120_300_and_then_flat() {
        assert_eq!([0, 1, 2, 3, 4, 9].map(backoff_secs), [30, 60, 120, 300, 300, 300]);
    }

    #[test]
    fn a_manager_the_user_stopped_stays_stopped() {
        assert_eq!(plan(&sup(false, "", 0, None), "stopped", |_| true), Plan::Idle);
        // Never started at all (fresh setup): not the watchdog's to bring up.
        let mut s = sup(false, "", 0, None);
        s.generation = 0;
        assert_eq!(plan(&s, "stopped", |_| true), Plan::Idle);
    }

    #[test]
    fn waiting_for_quota_is_not_an_outage() {
        assert_eq!(plan(&sup(true, "waiting_quota", 0, None), "stopped", |_| true), Plan::Idle);
    }

    #[test]
    fn a_running_manager_needs_nothing() {
        for l in ["idle", "busy", "starting"] {
            assert_eq!(plan(&sup(true, "", 2, Some("x")), l, |_| true), Plan::Idle, "{l}");
        }
    }

    #[test]
    fn the_first_observation_schedules_and_the_deadline_starts() {
        assert_eq!(plan(&sup(true, "", 0, None), "stopped", |_| true), Plan::Wait { schedule: true });
        assert_eq!(plan(&sup(true, "", 0, Some("later")), "stopped", |_| false), Plan::Wait { schedule: false });
        assert_eq!(plan(&sup(true, "", 0, Some("earlier")), "stopped", |_| true), Plan::Start);
    }

    #[test]
    fn five_failures_and_it_stops_trying() {
        assert_eq!(plan(&sup(true, "", 4, Some("earlier")), "stopped", |_| true), Plan::Start);
        assert_eq!(plan(&sup(true, "", 5, None), "stopped", |_| true), Plan::GaveUp);
        assert_eq!(plan(&sup(true, "", 5, Some("earlier")), "stopped", |_| true), Plan::GaveUp);
    }

    /// The nastiest shape of giving up, and the one that used to be completely silent: the
    /// fifth start *succeeds*, so no error path runs, and then the CLI dies a second later.
    /// Every tick after that lands on `GaveUp` — which has to keep meaning "stop retrying"
    /// while the reporting happens exactly once (that half is `report_gave_up`, guarded by
    /// `store::mark_watchdog_gave_up`).
    #[test]
    fn a_start_that_succeeded_and_then_died_still_ends_in_give_up_on_every_later_tick() {
        // Attempt 5 returned Ok, so the row holds attempts=5 with a backoff deadline.
        let after_ok = sup(true, "", 5, Some("2026-09-13T00:00:00Z"));
        // The CLI died: the manager is stopped again, and the deadline has since passed.
        for _ in 0..10 {
            assert_eq!(plan(&after_ok, "stopped", |_| true), Plan::GaveUp, "no more automatic starts");
        }
        // Seen alive again → `tick` resets the streak (Plan::Idle), which is what clears the
        // give-up marker, so a later outage is reported as a new one.
        assert_eq!(plan(&after_ok, "idle", |_| true), Plan::Idle);
        assert_eq!(plan(&after_ok, "busy", |_| true), Plan::Idle);
        // And a human starting it by hand resets `watchdog_attempts` to 0 (see
        // `store::set_desired_running`), which puts it back in the ordinary flow.
        assert_eq!(plan(&sup(true, "", 0, None), "stopped", |_| true), Plan::Wait { schedule: true });
    }
    /// #249：讀不到 manager liveness（`runs`／`turns` 的 SELECT 壞掉）不能當成「已停止」。
    /// 修前：活著的 manager 遇到一次 DB 讀故障，看門狗就當它掛了、排定自動重啟（`watchdog_next_at`
    /// 被寫上）。修後這個 tick 什麼都不動，DB 恢復後下一個 tick 看到真的狀態。
    #[tokio::test]
    async fn an_unreadable_liveness_is_not_an_outage() {
        for table in ["runs", "turns"] {
            let env = crate::testing::env().await;
            let app = env.app.clone();
            let bot = crate::testing::claude_bot(&app, &env.project_id, "agm-mgr").await.id;
            crate::testing::fake_run(&app, &bot).await;
            store::get_or_init(&app.db).await.unwrap();
            sqlx::query("UPDATE supervisors SET bot_id=?, desired_running=1, generation=1, status='' WHERE id=?")
                .bind(&bot)
                .bind(store::SUPERVISOR_ID)
                .execute(&app.db)
                .await
                .unwrap();

            crate::testing::make_table_unreadable(&app, table).await;
            tick(&app).await;
            crate::testing::make_table_readable(&app, table).await;
            let s = store::get_or_init(&app.db).await.unwrap();
            assert_eq!(s.watchdog_next_at, None, "{table} 讀不到：不可排定重啟");
            assert_eq!(s.watchdog_attempts, 0, "{table} 讀不到：不可計次");
            assert!(s.watchdog_gave_up_at.is_none());

            // 恢復後下一個 tick 看到活著的 manager，照常沒事。
            tick(&app).await;
            let s = store::get_or_init(&app.db).await.unwrap();
            assert_eq!(s.watchdog_next_at, None, "{table} 恢復後 manager 是活的");
        }
    }
    /// #251：標記寫進去了、inbox 事件卻寫不進去（一次暫時性故障）——之後的 tick 要補上，而且只有一則、
    /// key 帶真正的標記時間。修前第二次呼叫看到標記已存在就直接 return，通知永遠少一則。
    #[tokio::test]
    async fn a_failed_inbox_write_is_retried_until_exactly_one_event_exists() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        store::get_or_init(&app.db).await.unwrap();

        crate::testing::make_table_unreadable(&app, "supervisor_inbox").await;
        report_gave_up(&app, "boom").await;
        crate::testing::make_table_readable(&app, "supervisor_inbox").await;
        let s = store::get_or_init(&app.db).await.unwrap();
        assert!(s.watchdog_gave_up_at.is_some(), "標記已經寫下");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='watchdog_gave_up'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 0, "故障那一輪沒有事件");

        // 下一個 tick、沒有人介入：補上。再多幾輪也只有一則。
        for _ in 0..3 {
            report_gave_up(&app, "boom").await;
        }
        let keys: Vec<String> =
            sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE kind='watchdog_gave_up'").fetch_all(&app.db).await.unwrap();
        assert_eq!(keys, vec![format!("watchdog:gave_up:{}", s.watchdog_gave_up_at.unwrap())]);
    }
}
