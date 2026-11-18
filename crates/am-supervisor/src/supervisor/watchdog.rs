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

use std::collections::HashMap;
use std::time::{Duration, Instant};

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

/// In-process monotonic companion for persisted retry timestamps. The database timestamp is
/// still authoritative across daemon restarts; once observed or scheduled in this process, a
/// backward wall-clock adjustment cannot extend its remaining delay.
#[derive(Debug, Default)]
pub struct DeadlineCache {
    entries: HashMap<&'static str, (String, Instant)>,
}

impl DeadlineCache {
    fn schedule(&mut self, lane: &'static str, at: &str, due: Instant) {
        self.entries.insert(lane, (at.to_string(), due));
    }

    /// 測試接縫（#1862 S1）：`schedule` 有 production 呼叫，維持私有；daemon 的 `supervisor_tests` 只透過這個轉呼叫。`#[doc(hidden)]`，正式 build 不編。
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn schedule_for_test(&mut self, lane: &'static str, at: &str, due: Instant) {
        self.schedule(lane, at, due)
    }

    fn clear(&mut self, lane: &'static str) {
        self.entries.remove(lane);
    }

    fn past_at(&mut self, lane: &'static str, at: &str, wall: chrono::DateTime<chrono::Utc>, mono: Instant) -> bool {
        let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(at) else { return true };
        let wall_due = parsed <= wall;
        if wall_due {
            return true;
        }
        if let Some((cached_at, deadline)) = self.entries.get(lane) {
            if cached_at == at {
                return mono >= *deadline;
            }
        }
        let remaining = (parsed.with_timezone(&chrono::Utc) - wall).to_std().unwrap_or_default();
        if let Some(deadline) = mono.checked_add(remaining) {
            self.schedule(lane, at, deadline);
            mono >= deadline
        } else {
            false
        }
    }

    /// 測試接縫（#1862 S1）：`past_at` 有 production 呼叫，維持私有；daemon 的 `supervisor_tests` 只透過這個轉呼叫。`#[doc(hidden)]`，正式 build 不編。
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn past_at_for_test(&mut self, lane: &'static str, at: &str, wall: chrono::DateTime<chrono::Utc>, mono: Instant) -> bool {
        self.past_at(lane, at, wall, mono)
    }
}

pub fn scheduled_past(app: &impl crate::supervisor::watchdog::WatchdogDeadlines, lane: &'static str, at: &str) -> bool {
    app.watchdog_deadlines()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .past_at(lane, at, chrono::Utc::now(), Instant::now())
}

pub fn remember_schedule(app: &impl crate::supervisor::watchdog::WatchdogDeadlines, lane: &'static str, at: &str, delay: Duration) {
    app.watchdog_deadlines()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .schedule(lane, at, Instant::now() + delay);
}

pub fn clear_schedule(app: &impl crate::supervisor::watchdog::WatchdogDeadlines, lane: &'static str) {
    app.watchdog_deadlines()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear(lane);
}

/// One controller tick's worth of watching.
pub async fn tick(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::supervisor::watchdog::WatchdogDeadlines + crate::supervisor::ports::TurnOps + crate::supervisor::ports::ControllerRuntime + crate::supervisor::ports::QuotaOps + crate::supervisor::ports::JudgeOps + Clone + 'static)) {
    let Ok(sup) = store::get_or_init(app.db()).await else { return };
    let Some(bot_id) = sup.bot_id.clone() else { return };
    // 讀不到 liveness ＝ 不知道，不是「已停止」：不排程、不計次、不動任何 watchdog 狀態，下一個 tick 再看（#249）。
    let liveness = match super::manager_liveness(app.db(), &bot_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = ?e, "supervisor watchdog cannot read manager liveness; skipping this tick");
            return;
        }
    };
    let is_past = |at: &str| scheduled_past(app, "patrol", at);
    match plan(&sup, liveness, is_past) {
        Plan::Idle => {
            // Seen answering prompts again: the streak is over. `starting` is not enough — a
            // CLI that opens and dies would otherwise reset the count on every attempt.
            if matches!(liveness, "idle" | "busy") && (sup.watchdog_attempts > 0 || sup.watchdog_next_at.is_some()) {
                let _ = store::set_watchdog(app.db(), 0, None).await;
                clear_schedule(app, "patrol");
            }
        }
        Plan::Wait { schedule: true } => {
            let wait = backoff_secs(sup.watchdog_attempts);
            tracing::info!(attempts = sup.watchdog_attempts, wait, "supervisor is down and wanted; scheduling an automatic start");
            let at = iso_in(wait);
            if store::set_watchdog(app.db(), sup.watchdog_attempts, Some(&at)).await.is_ok() {
                remember_schedule(app, "patrol", &at, Duration::from_secs(wait));
            }
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
async fn report_gave_up(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), why: &str) {
    report_gave_up_as(app, GaveUpWho::Manager, why).await;
}

#[cfg(any(test, feature = "test-hooks"))]
/// 測試接縫（#1862 S1）：`report_gave_up` 在模組外不可見，daemon 的 `supervisor_tests` 只透過這個轉呼叫；`#[doc(hidden)]`，正式 build 不編。
#[doc(hidden)]
pub async fn report_gave_up_for_test(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), why: &str) {
    report_gave_up(app, why).await
}

/// Whose watchdog gave up: the supervisor (manager) or the AGM responder. Both run the same
/// mark → key-from-marker → inbox event → best-effort extras sequence; only the strings and the
/// store they touch differ. The event keys / kinds / payloads below are dedupe contracts: keep them.
#[derive(Clone, Copy)]
pub(super) enum GaveUpWho {
    Manager,
    Responder,
}

pub(super) async fn report_gave_up_as(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), who: GaveUpWho, why: &str) {
    use super::roles::{self, Role};
    // The marker means "owed a notification", not "notified" (#251). The key comes from the persisted
    // marker, never from a degraded read: no marker / unreadable row = retry next tick.
    let at = match who {
        GaveUpWho::Manager => {
            let _ = store::mark_watchdog_gave_up(app.db(), why).await;
            store::get_or_init(app.db()).await.ok().and_then(|s| s.watchdog_gave_up_at)
        }
        GaveUpWho::Responder => {
            let _ = roles::mark_watchdog_gave_up(app.db(), Role::Responder, why).await;
            roles::get(app.db(), Role::Responder).await.ok().and_then(|r| r.watchdog_gave_up_at)
        }
    };
    let Some(at) = at else { return };
    let (key, kind, payload) = match who {
        GaveUpWho::Manager => (
            format!("watchdog:gave_up:{at}"),
            "watchdog_gave_up",
            serde_json::json!({
                "attempts": MAX_ATTEMPTS,
                "why": why,
                "gave_up_at": at,
                "action": "手動 `bin/agm supervisor-start`；自動重試不會再發生，直到有人重新啟動它",
            }),
        ),
        GaveUpWho::Responder => (
            format!("responder_watchdog:gave_up:{at}"),
            "responder_watchdog_gave_up",
            serde_json::json!({"why": why, "gave_up_at": at, "action": "`bin/agm responder-start`；bot 申請留在 inbox，等超過 5 分鐘的核准會改派給巡檢裁示"}),
        ),
    };
    // Err = not durable yet, retry next tick; Ok(None) = already delivered on an earlier tick.
    let Ok(Some(_)) = store::push_inbox(app.db(), &key, kind, None, None, None, &payload).await else { return };
    match who {
        GaveUpWho::Manager => {
            tracing::error!(attempts = MAX_ATTEMPTS, why, "supervisor watchdog gave up; the manager stays down");
            let _ = store::set_status_detail(
                app.db(),
                Some(&format!("watchdog 連續 {MAX_ATTEMPTS} 次自動啟動後仍沒有活著，已停止重試；請手動 supervisor-start。原因：{why}")),
            )
            .await;
            app.emit("supervisor_changed", serde_json::json!({"watchdog": "gave_up", "why": why})).await;
        }
        GaveUpWho::Responder => {
            tracing::error!(why, "AGM responder watchdog gave up");
            let _ = roles::set_status_detail(app.db(), Role::Responder, Some(&format!("watchdog 已停止重試；請手動 responder-start。原因：{why}"))).await;
            app.emit("supervisor_changed", serde_json::json!({"responder": "watchdog_gave_up"})).await;
        }
    }
}

async fn start(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::supervisor::watchdog::WatchdogDeadlines + crate::supervisor::ports::TurnOps + crate::supervisor::ports::ControllerRuntime + crate::supervisor::ports::QuotaOps + crate::supervisor::ports::JudgeOps + Clone + 'static), bot_id: &str, failures: i64) {
    let _g = super::lock().await;
    // Re-read under the lock: `start`, `stop` or a bulk restart may have moved it meanwhile.
    let Ok(sup) = store::get_or_init(app.db()).await else { return };
    let Ok(liveness) = super::manager_liveness(app.db(), bot_id).await else { return };
    if plan(&sup, liveness, |at: &str| scheduled_past(app, "patrol", at)) != Plan::Start {
        return;
    }
    let attempt = failures + 1;
    let detail = format!("watchdog 自動重新啟動（第 {attempt} 次）");
    match super::start_manager(app, Some(&detail)).await {
        Ok(()) => {
            tracing::info!(attempt, "supervisor watchdog started the manager");
            // Counted until it is seen idle/busy: a CLI that dies right after opening keeps
            // climbing the backoff instead of restarting every 30 seconds.
            let wait = backoff_secs(attempt);
            let at = iso_in(wait);
            if store::set_watchdog(app.db(), attempt, Some(&at)).await.is_ok() {
                remember_schedule(app, "patrol", &at, Duration::from_secs(wait));
            }
        }
        Err(e) => {
            let why = format!("{e:?}");
            tracing::warn!(attempt, error = %why, "supervisor watchdog failed to start the manager");
            if attempt >= MAX_ATTEMPTS {
                let _ = store::set_watchdog(app.db(), attempt, None).await;
                clear_schedule(app, "patrol");
                // Same reporting as the "started but died" path: one durable event, once.
                report_gave_up(app, &why).await;
            } else {
                let wait = backoff_secs(attempt);
                let at = iso_in(wait);
                if store::set_watchdog(app.db(), attempt, Some(&at)).await.is_ok() {
                    remember_schedule(app, "patrol", &at, Duration::from_secs(wait));
                }
                let _ = store::set_status_detail(app.db(), Some(&format!("watchdog 自動啟動失敗（第 {attempt} 次）：{why}"))).await;
            }
        }
    }
}
/// 總管看門狗的到期快取。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait WatchdogDeadlines: Send + Sync {
    fn watchdog_deadlines(&self) -> &std::sync::Mutex<crate::supervisor::watchdog::DeadlineCache>;
}
