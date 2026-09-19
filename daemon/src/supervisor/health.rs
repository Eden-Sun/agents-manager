//! Cheap, deterministic health summary for AGM and the UI.

use crate::lifecycle::LcError;
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// `gh auth status` 要打網路，health 又常被輪詢：60 秒內沿用上一次的結果。
async fn release_triage_health(app: &Arc<App>) -> Value {
    static CACHE: tokio::sync::Mutex<Option<(std::time::Instant, Value)>> = tokio::sync::Mutex::const_new(None);
    let cfg = app.cfg.get().await.release_triage;
    let mut c = CACHE.lock().await;
    if let Some((at, v)) = c.as_ref() {
        if at.elapsed() < std::time::Duration::from_secs(60) {
            return v.clone();
        }
    }
    let v = crate::release_triage::issue::health_probe(&cfg).await.unwrap_or(Value::Null);
    *c = Some((std::time::Instant::now(), v.clone()));
    v
}

pub async fn snapshot(app: &Arc<App>) -> Result<Value, LcError> {
    let bots = crate::db::live_bots(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let mut running = 0usize;
    let mut busy = 0usize;
    let mut stopped = 0usize;
    for bot in &bots {
        match crate::db::active_run(&app.db, &bot.id).await.map_err(|e| LcError::Upstream(e.to_string()))? {
            Some(run) => {
                running += 1;
                if run.agent_status == "working" || run.agent_status == "blocked" { busy += 1; }
            }
            None => stopped += 1,
        }
    }
    let supervisor = crate::supervisor::status_json(app).await?;
    let supervisor_status = supervisor.get("status").and_then(Value::as_str).unwrap_or("unknown");
    let manager_severity = if supervisor_status == "failed" || supervisor_status == "waiting_quota" {
        "critical"
    } else if supervisor_status == "not_configured" || supervisor_status == "stopped" || !app.connected.load(std::sync::atomic::Ordering::SeqCst) {
        "degraded"
    } else { "healthy" };
    // 協調者自己一格，跟巡檢分開：協調者等額度或倒了，不是「AGM 不能用」——使用者入口還在。
    let responder = supervisor.get("responder").cloned().unwrap_or(Value::Null);
    let responder_status = responder.get("status").and_then(Value::as_str).unwrap_or("not_configured");
    let responder_severity = responder_severity(&responder);
    let hosts = app.hosts.list().await;
    let disconnected_hosts = hosts.iter().filter(|h| !h.is_connected()).count();
    let system = crate::supervisor::incidents::system_health(app).await;
    let system_severity = system.get("status").and_then(Value::as_str).unwrap_or("unknown");
    // The compat projection. `status` used to mean "is AGM all right", and a caller that only
    // reads this field must not be told everything is fine while a host is down — so it is now
    // the worse of the two halves, and the halves are published next to it. See docs/SPEC.md §18.
    // 協調者也算進頂層：它是 bot 申請、核准請求與所有 mission 事件的**唯一**收件人，
    // 它卡住時使用者入口顯示 healthy、沒有任何人被叫醒，申請可以躺好幾天（review 2026-09-16）。
    // 「兩個問題兩個答案」的分格照舊留著，頂層只是不再漏掉這一半。
    let core = crate::supervisor::incidents::worst(manager_severity, system_severity);
    let severity = crate::supervisor::incidents::worst(&core, responder_severity);
    Ok(json!({
        "status": severity,
        "checked_at": crate::db::now(),
        // Two questions, two answers: whether the manager can work, and whether the system
        // around it is intact. Folding them into one number is what let `healthy` mean neither.
        "manager_health": {
            "status": manager_severity,
            "supervisor_status": supervisor_status,
            "daemon_connected": app.connected.load(std::sync::atomic::Ordering::SeqCst),
        },
        "system_health": system,
        "responder_health": {
            "status": responder_severity,
            "responder_status": responder_status,
            "inbox_open": responder.get("inbox_open"),
            "wake_pending": responder.get("wake_pending"),
            "retry_at": responder.pointer("/stats/notify_next_at"),
        },
        "daemon": {"connected": app.connected.load(std::sync::atomic::Ordering::SeqCst)},
        "supervisor": supervisor,
        "bots": {"total": bots.len(), "running": running, "busy": busy, "stopped": stopped},
        "quota": crate::quota::snapshot(app).await,
        // 「接下來要做什麼、什麼一直做不成」。到期動作本來就都落在 DB 上（各自掛在自己那張表），
        // 只是以前要看得翻六張表；issue #75 驗收第 5 條。
        "due_actions": crate::due_actions::snapshot(app).await,
        // Two numbers, not one sum: an assignment still running and a notification nobody
        // acked are different kinds of "owed", and adding them hid a 464-event backlog.
        "pending_assignments": crate::supervisor::store::open_assignment_count(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "awaiting_review": crate::supervisor::store::awaiting_review_count(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "inbox_open": crate::supervisor::store::open_inbox_count(&app.db).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "hosts": {"total": hosts.len(), "disconnected": disconnected_hosts},
        // issue #204：`[release_triage] publish = true` 時 gh 沒登入要在這裡看得到（publish = false 為 null）。
        "release_triage": release_triage_health(app).await,
    }))
}

/// 協調者那一格的嚴重度。
///
/// 「建立過」就一直攔 bot 的申請（SPEC §18.15），所以它**沒在跑**時——setup 完還沒 start、start 失敗、
/// 使用者手動 stop、bot 被刪——只要佇列裡有會叫醒它的事件，就是有人在等而沒有人會被叫醒。以前這種
/// 狀態只要 `desired_running=0` 就算 healthy，申請、核准、mission 事件無限期累積（review 2026-09-16 c3 M1）。
/// degraded 會讓 `health_changed` 入列並叫醒巡檢。
pub fn responder_severity(responder: &Value) -> &'static str {
    let status = responder.get("status").and_then(Value::as_str).unwrap_or("not_configured");
    let waiting = responder.get("wake_pending").and_then(Value::as_i64).unwrap_or(0) > 0;
    match status {
        "not_configured" | "idle" | "busy" | "starting" => "healthy",
        "waiting_quota" => "degraded",
        _ if responder.pointer("/desired_running").and_then(Value::as_bool) == Some(true) => "degraded",
        _ if waiting => "degraded",
        _ => "healthy",
    }
}

/// What the inbox debounce keys on. `idle` and `busy` are one state here: the manager going
/// busy on its own turn is not news the manager needs an inbox event about.
pub fn inbox_state(supervisor_status: &str) -> &str {
    match supervisor_status {
        "idle" | "busy" => "running",
        other => other,
    }
}

/// What a `health_changed` inbox event is keyed on: the manager's severity **and** the responder's.
///
/// The responder is the only recipient of bot requests, approvals and mission events. Keying on the
/// manager alone meant a responder stuck on `waiting_quota` turned the UI `degraded` while no event
/// was ever queued and nobody was woken — requests could sit for days (review 2026-09-16 #7).
/// A snapshot from before `responder_health` existed reads as `healthy` there.
pub fn inbox_severity(snapshot: &Value) -> String {
    let manager = snapshot.pointer("/manager_health/status").and_then(Value::as_str).unwrap_or("unknown");
    let responder = snapshot.pointer("/responder_health/status").and_then(Value::as_str).unwrap_or("healthy");
    format!("{manager}/{responder}")
}

/// No one is there to read an event: it is not queued, and whatever changed meanwhile is
/// folded into the one snapshot sent when the manager is back.
fn manager_down(inbox_state: &str) -> bool {
    matches!(inbox_state, "stopped" | "starting" | "not_configured")
}

/// Decides which health ticks become `health_changed` inbox events.
///
/// 2026-09-10: the fingerprint included the busy / running / pending counters, so a night of
/// bots starting and finishing put 464 events in front of a manager that was not even up. Only
/// the severity and the manager's own state count now, and nothing is queued while it is down.
#[derive(Debug, Default)]
pub struct Debounce {
    last_pushed: Option<(String, String)>,
    /// Something changed while the manager was down; it gets one snapshot when it is back.
    suppressed: bool,
}

impl Debounce {
    /// `true` = queue this tick's snapshot.
    pub fn observe(&mut self, severity: &str, supervisor_status: &str) -> bool {
        let state = inbox_state(supervisor_status);
        let key = (severity.to_string(), state.to_string());
        let changed = self.last_pushed.as_ref() != Some(&key);
        if manager_down(state) {
            if changed {
                self.suppressed = true;
            }
            return false;
        }
        if changed || self.suppressed {
            self.last_pushed = Some(key);
            self.suppressed = false;
            return true;
        }
        false
    }
}

/// Poll health outside the assignment controller. The daemon emits every fingerprint change to
/// the UI, and queues a durable inbox event only when the *state* changes (see [`Debounce`]),
/// so AGM can reason about it without `/loop` and without wading through counters.
pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut previous = String::new();
        let mut debounce = Debounce::default();
        let mut detector = crate::supervisor::incidents::Detector::default();
        loop {
            tick.tick().await;
            // Incidents first: the snapshot below reports what this pass decided, so a fault
            // and the health reading that mentions it never disagree by one tick.
            crate::supervisor::incidents::sweep(&app, &mut detector).await;
            let Ok(snapshot) = snapshot(&app).await else { continue };
            let status = snapshot.get("status").and_then(Value::as_str).unwrap_or("unknown").to_string();
            let sup_status = snapshot.pointer("/supervisor/status").and_then(Value::as_str).unwrap_or("unknown").to_string();
            let fingerprint = serde_json::json!({
                "status": status,
                "supervisor": sup_status,
                "running": snapshot.pointer("/bots/running"),
                "busy": snapshot.pointer("/bots/busy"),
                "pending": snapshot.get("pending_assignments"),
                "awaiting_review": snapshot.get("awaiting_review"),
                "inbox_open": snapshot.get("inbox_open"),
                "disconnected": snapshot.pointer("/hosts/disconnected"),
                "incidents": snapshot.pointer("/system_health/open_incidents"),
            }).to_string();
            if fingerprint != previous {
                previous = fingerprint;
                let _ = app.emit("supervisor_health", snapshot.clone()).await;
            }
            // Keyed on the manager's and the responder's halves. System faults have their own
            // durable incidents with their own one-event-per-transition rule; letting them move this
            // key too would tell the manager the same thing twice.
            let severity = inbox_severity(&snapshot);
            if !debounce.observe(&severity, &sup_status) {
                continue;
            }
            let key = format!("health:{severity}:{}:{}", inbox_state(&sup_status), chrono::Utc::now().timestamp());
            let bot_id = snapshot.pointer("/supervisor/bot_id").and_then(Value::as_str);
            let _ = crate::supervisor::store::push_inbox(
                &app.db, &key, "health_changed", None, bot_id, None, &snapshot,
            ).await;
            tracing::info!(status, supervisor = %sup_status, "supervisor health changed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_do_not_make_events_only_state_does() {
        let mut d = Debounce::default();
        assert!(d.observe("healthy", "idle"), "the first reading after boot is news");
        // Ticks with different bot counts land here as the same (severity, state): nothing.
        assert!(!d.observe("healthy", "idle"));
        assert!(!d.observe("healthy", "busy"), "the manager's own busy/idle is not a state change");
        assert!(d.observe("degraded", "idle"));
        assert!(d.observe("healthy", "idle"));
        assert!(d.observe("critical", "waiting_quota"));
    }

    #[test]
    fn nothing_is_queued_while_the_manager_is_down_and_one_snapshot_when_it_is_back() {
        let mut d = Debounce::default();
        assert!(d.observe("healthy", "idle"));
        // 5.5 hours of 30-second ticks against a dead manager: zero events, not 464.
        for _ in 0..660 {
            assert!(!d.observe("degraded", "stopped"));
        }
        assert!(!d.observe("degraded", "starting"));
        assert!(d.observe("healthy", "idle"), "one snapshot once it is back, even to the same state");
        assert!(!d.observe("healthy", "idle"));
    }

    /// 協調者卡在 `waiting_quota`（degraded）而巡檢好好的：以前 debounce 只看巡檢那一半，一則事件都不會有。
    #[test]
    fn a_responder_going_degraded_is_news_even_when_the_manager_is_fine() {
        let snap = |m: &str, r: Option<&str>| {
            let mut v = json!({"manager_health": {"status": m}});
            if let Some(r) = r {
                v["responder_health"] = json!({"status": r});
            }
            v
        };
        let mut d = Debounce::default();
        assert!(d.observe(&inbox_severity(&snap("healthy", Some("healthy"))), "idle"));
        assert!(d.observe(&inbox_severity(&snap("healthy", Some("degraded"))), "idle"), "responder waiting_quota must queue an event");
        assert!(!d.observe(&inbox_severity(&snap("healthy", Some("degraded"))), "busy"));
        assert!(d.observe(&inbox_severity(&snap("healthy", Some("healthy"))), "idle"), "and its recovery too");
        // 舊 snapshot 沒有 responder_health：當 healthy，不會憑空多一則。
        assert_eq!(inbox_severity(&snap("healthy", None)), inbox_severity(&snap("healthy", Some("healthy"))));
    }

    /// 協調者建立過但沒在跑（setup 完沒 start、start 失敗、手動 stop、bot 被刪）：佇列裡有會叫醒它的事件
    /// 就不是 healthy——沒有人會被叫醒（review 2026-09-16 c3 M1）。沒有待辦的停著仍是 healthy。
    #[test]
    fn a_responder_that_is_not_running_while_requests_wait_is_degraded() {
        let r = |status: &str, desired: bool, waiting: i64| json!({"status": status, "desired_running": desired, "wake_pending": waiting});
        assert_eq!(responder_severity(&r("stopped", false, 0)), "healthy", "停著、沒人在等：不是故障");
        assert_eq!(responder_severity(&r("stopped", false, 2)), "degraded");
        assert_eq!(responder_severity(&r("missing", false, 1)), "degraded");
        assert_eq!(responder_severity(&r("stopped", true, 0)), "degraded", "想要它跑卻停著，照舊是 degraded");
        assert_eq!(responder_severity(&r("idle", false, 5)), "healthy", "在跑就會被叫醒");
        assert_eq!(responder_severity(&r("not_configured", false, 3)), "healthy", "沒建立：事件歸巡檢");
        assert_eq!(responder_severity(&json!({"status": "stopped"})), "healthy", "舊形狀沒有 wake_pending");
    }

    /// 端到端：setup 過、從沒 start，bot 送來一筆申請 → 健康頂層不再是 healthy。
    #[tokio::test]
    async fn a_request_queued_for_a_responder_that_never_started_shows_up_in_health() {
        use crate::supervisor::bot_requests::{self, flow_tests, ReplyMark};
        let app = flow_tests::app().await;
        flow_tests::configure_responder(&app).await;
        let snap = snapshot(&app).await.unwrap();
        assert_eq!(snap.pointer("/responder_health/status").and_then(Value::as_str), Some("healthy"));
        bot_requests::intercept(&app, "patrol", "w1", "請核准重建 abc", Some("r-1"), &[], true, "api", ReplyMark::default()).await.unwrap().unwrap();
        let snap = snapshot(&app).await.unwrap();
        assert_eq!(snap.pointer("/responder_health/status").and_then(Value::as_str), Some("degraded"), "{snap}");
        assert_eq!(snap.pointer("/responder_health/wake_pending").and_then(Value::as_i64), Some(1));
        assert_ne!(snap.get("status").and_then(Value::as_str), Some("healthy"));
    }

    #[test]
    fn a_manager_that_was_never_up_gets_its_first_snapshot_when_it_is() {
        let mut d = Debounce::default();
        assert!(!d.observe("degraded", "not_configured"));
        assert!(!d.observe("degraded", "stopped"));
        assert!(d.observe("healthy", "busy"));
    }
}
