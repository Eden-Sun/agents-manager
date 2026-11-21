//! Cheap, deterministic health summary for AGM and the UI.

use crate::lifecycle::LcError;
use sqlx::SqlitePool;
use serde_json::{json, Value};

#[cfg(test)]
struct ProbeSlot {
    at: Option<std::time::Instant>,
    value: Value,
    refreshing: bool,
}

/// 快取 `ttl` 內直接回；有人正在探測就回上一份（沒有就是 Null），不排隊；探測最多等 `limit`。
#[cfg(test)]
async fn cached_probe<F, Fut>(slot: &std::sync::Mutex<ProbeSlot>, ttl: std::time::Duration, limit: std::time::Duration, probe: F) -> Value
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Value>,
{
    {
        let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
        if s.at.is_some_and(|at| at.elapsed() < ttl) || s.refreshing {
            return s.value.clone();
        }
        s.refreshing = true;
    }
    struct Reset<'a>(&'a std::sync::Mutex<ProbeSlot>);
    impl Drop for Reset<'_> {
        fn drop(&mut self) {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).refreshing = false;
        }
    }
    let _reset = Reset(slot);
    let fresh = tokio::time::timeout(limit, probe()).await.ok();
    let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = fresh {
        s.value = v;
    }
    s.at = Some(std::time::Instant::now());
    s.value.clone()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReleaseTriageFingerprint {
    pub publish: bool,
    pub gh_bin: Option<String>,
    pub repo: Option<String>,
}

impl From<&crate::config::ReleaseTriageCfg> for ReleaseTriageFingerprint {
    fn from(cfg: &crate::config::ReleaseTriageCfg) -> Self {
        Self {
            publish: cfg.publish,
            gh_bin: cfg.gh_bin.clone(),
            repo: cfg.repo.clone(),
        }
    }
}

struct AppProbeSlot {
    fingerprint: ReleaseTriageFingerprint,
    epoch: u64,
    at: Option<std::time::Instant>,
    value: Value,
    refreshing: bool,
}

static RELEASE_TRIAGE_SLOTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, AppProbeSlot>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 格子數上限（#1324）：每個資料目錄一格，但短命的執行個體（測試、臨時部署）用完不能讓表永遠長下去。
const RELEASE_TRIAGE_SLOT_CAP: usize = 16;

/// 超過上限就丟掉最久沒用、而且沒有探測在跑的格子。正在探測的不丟：它的結果要寫回自己的格子，epoch 才對得上（#1159）。
fn evict_idle_slots(map: &mut std::collections::HashMap<std::path::PathBuf, AppProbeSlot>) {
    while map.len() > RELEASE_TRIAGE_SLOT_CAP {
        let victim = map.iter().filter(|(_, s)| !s.refreshing).min_by_key(|(_, s)| s.at).map(|(k, _)| k.clone());
        match victim {
            Some(k) => {
                map.remove(&k);
            }
            None => break,
        }
    }
}

/// `gh auth status` 要打網路，health 又常被輪詢：60 秒內沿用上一次的結果，探測最多等 15 秒（issue #1118）。
/// 快取以 App 資料目錄與 release_triage 設定指紋隔離，設定變更時強制失效，
/// 且進行中的探測綁定開始時的設定代數（issue #1159）。
pub async fn release_triage_health(
    app: &(impl crate::capabilities::DataDir + crate::capabilities::Cfg + crate::supervisor::ports::HostProbes),
) -> Value {
    let data_dir = app.data_dir().to_path_buf();
    let cfg = app.cfg().get().await.release_triage;
    let current_fp = ReleaseTriageFingerprint::from(&cfg);
    let ttl = std::time::Duration::from_secs(60);
    let limit = std::time::Duration::from_secs(15);

    let my_epoch = {
        let mut map = RELEASE_TRIAGE_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        match map.get_mut(&data_dir) {
            Some(slot) => {
                if slot.fingerprint != current_fp {
                    slot.fingerprint = current_fp.clone();
                    slot.epoch = slot.epoch.wrapping_add(1);
                    slot.at = None;
                    slot.value = Value::Null;
                    slot.refreshing = true;
                    slot.epoch
                } else if slot.at.is_some_and(|at| at.elapsed() < ttl) || slot.refreshing {
                    return slot.value.clone();
                } else {
                    slot.refreshing = true;
                    slot.epoch
                }
            }
            None => {
                let slot = AppProbeSlot {
                    fingerprint: current_fp.clone(),
                    epoch: 1,
                    at: None,
                    value: Value::Null,
                    refreshing: true,
                };
                map.insert(data_dir.clone(), slot);
                evict_idle_slots(&mut map);
                1
            }
        }
    };

    struct ResetGuard {
        data_dir: std::path::PathBuf,
        epoch: u64,
    }
    impl Drop for ResetGuard {
        fn drop(&mut self) {
            let mut map = RELEASE_TRIAGE_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = map.get_mut(&self.data_dir) {
                if slot.epoch == self.epoch {
                    slot.refreshing = false;
                }
            }
        }
    }
    let _reset = ResetGuard {
        data_dir: data_dir.clone(),
        epoch: my_epoch,
    };

    let fresh = tokio::time::timeout(limit, async {
        app.release_triage_health_probe(&cfg).await.unwrap_or(Value::Null)
    })
    .await
    .ok();

    let mut map = RELEASE_TRIAGE_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = map.get_mut(&data_dir) {
        if slot.epoch == my_epoch && slot.fingerprint == current_fp {
            if let Some(v) = fresh {
                slot.value = v;
            }
            slot.at = Some(std::time::Instant::now());
            slot.refreshing = false;
            return slot.value.clone();
        }
    }
    fresh.unwrap_or(Value::Null)
}

/// 一顆角色 bot（巡檢／協調者）現在能不能用，給「核准該不該改派」這類決定當依據（issue #421）。
///
/// #420 之後 daemon 已經看得出協調者停在登入失效（`responder::notify` 送不出去時讀畫面，
/// 標 `supervisor_roles.status = 'needs_login'`），但只有一個字串，呼叫端要自己拼判斷。
/// 這裡把它收成一個型別，重點是**四個值**而不是 bool。
///
/// `Unknown` 一定要跟 `Unavailable` 分開：沒有證據就把核准從一顆健康的協調者手上搬走，
/// 比晚幾分鐘更糟。呼叫端只在 `Unavailable` 動作，`Unknown` 當作「這一拍不動」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleState {
    /// 這個角色還沒建立：核准本來就歸巡檢，沒有東西要改派。
    NotConfigured,
    Available,
    /// 不可用，附一個**穩定的**原因字串（見 `REASON_*`）——會被寫進 inbox payload 與 SPEC，別改字。
    Unavailable(&'static str),
    /// 讀不到（DB 錯）。不是「它壞了」，也不是「它好了」。
    Unknown,
}

/// claude 停在 `Not logged in · Please run /login`（或 Keychain 鎖著）。
/// 由 `responder::notify` 在送不出去時看畫面判定（#420）；**看不到畫面時它不會標**，
/// 所以這裡讀到的「沒有 needs_login」是「沒有證據說它壞了」，不是「證明它是好的」。
pub const REASON_NEEDS_LOGIN: &str = "needs_login";
/// 撞額度。
pub const REASON_WAITING_QUOTA: &str = "waiting_quota";
/// 登記過、卻沒有 active run。
pub const REASON_NO_RUN: &str = "no_run";

/// 協調者能不能用。
pub async fn responder_state(app: &(impl crate::capabilities::Db + crate::supervisor::role_faults::RoleFaultTable)) -> RoleState {
    role_state(app, crate::supervisor::roles::Role::Responder).await
}

/// 巡檢與協調者共用同一套判斷。順序是「最確定的證據先講」。
///
/// 只讀 DB 與記憶體，**不在這裡讀畫面**：`/api/supervisor/health` 與 `/api/supervisor/responder`
/// 都會被 UI 高頻輪詢，每次都去抓一次 pane 會把 herdr 打爆。畫面有兩條路看：`responder::notify`
/// 在**送不出去時**看一次（結論落在 `supervisor_roles.status`，#420），以及 health 的 30 秒 tick
/// 對兩個角色各看一次（結論落在 `App.role_faults`，#427——巡檢只有這一條路看得到）。
pub async fn role_state(app: &(impl crate::capabilities::Db + crate::supervisor::role_faults::RoleFaultTable), role: crate::supervisor::roles::Role) -> RoleState {
    let row = match crate::supervisor::roles::record(app.db(), role).await {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!(role = role.as_str(), error = ?e, "role_state：讀不到角色狀態");
            return RoleState::Unknown;
        }
    };
    let Some(bot_id) = row.bot_id.clone() else { return RoleState::NotConfigured };
    match row.status.as_str() {
        "needs_login" => return RoleState::Unavailable(REASON_NEEDS_LOGIN),
        "waiting_quota" => return RoleState::Unavailable(REASON_WAITING_QUOTA),
        _ => {}
    }
    match crate::db::bot(app.db(), &bot_id).await {
        Ok(Some(bot)) if bot.deleted_at.is_none() => {}
        Ok(_) => return RoleState::Unavailable(REASON_NO_RUN),
        Err(e) => {
            tracing::warn!(role = role.as_str(), error = ?e, "role_state：讀不到那顆 bot");
            return RoleState::Unknown;
        }
    }
    match crate::db::active_run(app.db(), &bot_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return RoleState::Unavailable(REASON_NO_RUN),
        Err(e) => {
            tracing::warn!(role = role.as_str(), error = ?e, "role_state：讀不到 active run");
            return RoleState::Unknown;
        }
    }
    if let Some(reason) = crate::supervisor::role_faults::reason(app, role).await {
        return RoleState::Unavailable(reason);
    }
    RoleState::Available
}

impl RoleState {
    /// 不可用的原因，給 API／incident detail／inbox payload 用。只有 `Unavailable` 有值。
    pub fn reason(self) -> Option<&'static str> {
        match self {
            RoleState::Unavailable(r) => Some(r),
            _ => None,
        }
    }

    /// 可以拿它當「改派」的依據嗎。`Unknown` 與 `NotConfigured` 都是 `false`：
    /// 前者沒有證據，後者本來就沒有東西要改派。
    pub fn is_unavailable(self) -> bool {
        matches!(self, RoleState::Unavailable(_))
    }
}

pub async fn snapshot(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Cfg + crate::hosts::HostsAccess + crate::supervisor::ports::HostProbes + crate::supervisor::ports::QuotaOps + crate::supervisor::ports::LocalAccountView + crate::supervisor::role_faults::RoleFaultTable + crate::supervisor::ports::DaemonConnection)) -> Result<Value, LcError> {
    let bots = crate::db::live_bots(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let active = crate::db::active_runs_by_bot(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    let mut running = 0usize;
    let mut busy = 0usize;
    let mut stopped = 0usize;
    let mut background_stuck = Vec::new();
    for bot in &bots {
        match active.get(&bot.id) {
            Some(run) => {
                running += 1;
                if run.agent_status == "working" || run.agent_status == "blocked" { busy += 1; }
                if let Some(v) = background_stuck_entry(app, bot, run) { background_stuck.push(v); }
            }
            None => stopped += 1,
        }
    }
    let supervisor = crate::supervisor::status_json(app).await?;
    let supervisor_status = supervisor.get("status").and_then(Value::as_str).unwrap_or("unknown");
    let manager_severity = if supervisor_status == "failed" || supervisor_status == "waiting_quota" {
        "critical"
    } else if supervisor_status == "not_configured" || supervisor_status == "stopped" || !app.daemon_connected() {
        "degraded"
    } else { "healthy" };
    let responder = supervisor.get("responder").cloned().unwrap_or(Value::Null);
    let responder_status = responder.get("status").and_then(Value::as_str).unwrap_or("not_configured");
    let responder_severity = responder_severity(&responder);
    let hosts = app.hosts().list().await;
    let disconnected_hosts = hosts.iter().filter(|h| !h.is_connected()).count();
    let system = crate::supervisor::incidents::system_health(app).await;
    let system_severity = system.get("status").and_then(Value::as_str).unwrap_or("unknown");
    let core = crate::supervisor::incidents::worst(manager_severity, system_severity);
    let severity = crate::supervisor::incidents::worst(&core, responder_severity);
    let api_bots_stuck_rows = am_base::db::api_bot::stuck_claude_api_bots_in_db(
        app.db(),
        &crate::db::iso_in(-1920),
        &crate::db::iso_in(-300),
    ).await.unwrap_or_default();
    let api_bots_stuck: Vec<Value> = api_bots_stuck_rows
        .into_iter()
        .map(|(bot_id, name, reason)| json!({
            "bot_id": bot_id,
            "name": name,
            "reason": reason,
        }))
        .collect();
    Ok(json!({
        "status": severity,
        "checked_at": crate::db::now(),
        "manager_health": {
            "status": manager_severity,
            "supervisor_status": supervisor_status,
            "daemon_connected": app.daemon_connected(),
        },
        "system_health": system,
        "responder_health": {
            "status": responder_severity,
            "responder_status": responder_status,
            "unavailable_reason": responder.get("unavailable_reason"),
            "inbox_open": responder.get("inbox_open"),
            "wake_pending": responder.get("wake_pending"),
            "retry_at": responder.pointer("/stats/notify_next_at"),
        },
        "daemon": {"connected": app.daemon_connected()},
        "supervisor": supervisor,
        "bots": {"total": bots.len(), "running": running, "busy": busy, "stopped": stopped},
        "background_stuck": background_stuck,
        "api_bots_stuck": api_bots_stuck,
        "quota": app.quota_snapshot_json().await,
        "due_actions": app.due_actions_snapshot().await,
        "pending_assignments": crate::supervisor::store::open_assignment_count(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "awaiting_review": crate::supervisor::store::awaiting_review_count(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "inbox_open": crate::supervisor::store::open_inbox_count(app.db()).await.map_err(|e| LcError::Upstream(e.to_string()))?,
        "hosts": {"total": hosts.len(), "disconnected": disconnected_hosts},
        "release_triage": release_triage_health(app).await,
        "timing": crate::supervisor::timing::snapshot(),
    }))
}

/// 一顆 bot 的背景工作標著「可能卡住」（`background_jobs::STUCK_AFTER_SECS`）時的一列；沒有就是 `None`。
fn background_stuck_entry(app: &impl crate::supervisor::ports::LocalAccountView, bot: &crate::db::Bot, run: &crate::db::Run) -> Option<Value> {
    let n = app.background_jobs_known(&run.id).filter(|n| *n > 0)?;
    let (since, secs, stuck) = app.background_jobs_duration(&run.id)?;
    stuck.then(|| {
        let since = chrono::DateTime::from_timestamp_millis(since).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        json!({"bot_id": bot.id, "name": bot.name, "kind": bot.kind, "run_id": run.id, "background_jobs": n, "since": since, "secs": secs})
    })
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
    let reason = responder.get("unavailable_reason").and_then(Value::as_str);
    match (status, reason) {
        (_, Some(r)) if r == REASON_NEEDS_LOGIN => "critical",
        ("needs_login", _) => "critical",
        (_, Some(r)) if r == REASON_NO_RUN => match () {
            _ if responder.pointer("/desired_running").and_then(Value::as_bool) == Some(true) => "degraded",
            _ if waiting => "degraded",
            _ => "healthy",
        },
        (_, Some(_)) => "degraded",
        ("not_configured" | "idle" | "busy" | "starting", _) => "healthy",
        ("waiting_quota", _) => "degraded",
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
    /// A state waiting for its durable inbox write. Keep its key stable across retries so an
    /// ambiguous commit cannot turn one transition into several events.
    pending: Option<PendingHealth>,
    /// Something changed while the manager was down; it gets one snapshot when it is back.
    suppressed: bool,
}

#[derive(Debug)]
struct PendingHealth {
    state: (String, String),
    event_key: String,
}

impl Debounce {
    /// `true` = queue this tick's snapshot.
    pub fn observe(&mut self, severity: &str, supervisor_status: &str) -> bool {
        let state = inbox_state(supervisor_status);
        let key = (severity.to_string(), state.to_string());
        let changed = self.last_pushed.as_ref() != Some(&key);
        if manager_down(state) {
            if changed || self.pending.is_some() {
                self.suppressed = true;
            }
            return false;
        }
        if self.pending.as_ref().is_some_and(|p| p.state == key) {
            return true;
        }
        if changed || self.suppressed {
            self.pending = Some(PendingHealth {
                event_key: format!("health:{}:{}:{}", key.0, key.1, crate::db::ulid()),
                state: key,
            });
            return true;
        }
        self.pending = None;
        false
    }

    fn pending_event_key(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.event_key.as_str())
    }

    /// 測試接縫（#1862 S1）：`pending_event_key` 有 production 呼叫，維持私有；daemon 的 `supervisor_tests` 只透過這個轉呼叫。`#[doc(hidden)]`，正式 build 不編。
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn pending_event_key_for_test(&self) -> Option<&str> {
        self.pending_event_key()
    }

    /// Only call after `push_inbox` confirms the event is durable (including an already-present key).
    fn acknowledge(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.last_pushed = Some(pending.state);
            self.suppressed = false;
        }
    }

    /// 測試接縫（#1862 S1）：`acknowledge` 有 production 呼叫，維持私有；daemon 的 `supervisor_tests` 只透過這個轉呼叫。`#[doc(hidden)]`，正式 build 不編。
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn acknowledge_for_test(&mut self) {
        self.acknowledge()
    }
}

pub async fn queue_health_change(db: &SqlitePool, debounce: &mut Debounce, snapshot: &Value, severity: &str, sup_status: &str) -> bool {
    if !debounce.observe(severity, sup_status) {
        return false;
    }
    let Some(key) = debounce.pending_event_key().map(str::to_owned) else { return false };
    let bot_id = snapshot.pointer("/supervisor/bot_id").and_then(Value::as_str);
    match crate::supervisor::store::push_inbox(
        db, &key, "health_changed", None, bot_id, None, snapshot,
    ).await {
        Ok(_) => {
            debounce.acknowledge();
            let status = snapshot.get("status").and_then(Value::as_str).unwrap_or("unknown");
            tracing::info!(status, supervisor = %sup_status, "supervisor health changed");
            true
        }
        Err(e) => {
            tracing::warn!(error = ?e, status = ?snapshot.get("status"), supervisor = %sup_status, "health_changed inbox write failed; retrying next tick");
            false
        }
    }
}

/// The polling loop is composed in `runners::supervisor::health`.
#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod probe_cache_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn a_slow_probe_does_not_block_other_readers() {
        let slot = Arc::new(std::sync::Mutex::new(ProbeSlot {
            at: None,
            value: Value::Null,
            refreshing: false,
        }));
        let s1 = Arc::clone(&slot);
        let h1 = tokio::spawn(async move {
            cached_probe(
                &s1,
                Duration::from_secs(60),
                Duration::from_secs(5),
                || async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    json!({"n": 1})
                },
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let s2 = Arc::clone(&slot);
        let v2 = tokio::time::timeout(Duration::from_millis(100), async move {
            cached_probe(
                &s2,
                Duration::from_secs(60),
                Duration::from_secs(5),
                || async {
                    panic!("不應呼叫 probe");
                },
            )
            .await
        })
        .await
        .expect("應在時限內返回");

        assert_eq!(v2, Value::Null, "探測中其他呼叫應拿到上一份（此處為 Null）");

        let v1 = h1.await.unwrap();
        assert_eq!(v1, json!({"n": 1}));

        let s3 = Arc::clone(&slot);
        let v3 = cached_probe(
            &s3,
            Duration::from_secs(60),
            Duration::from_secs(5),
            || async {
                panic!("快取中不應呼叫 probe");
            },
        )
        .await;
        assert_eq!(v3, json!({"n": 1}));
    }

    #[tokio::test]
    async fn a_probe_that_times_out_is_not_retried_within_the_ttl() {
        let slot = Arc::new(std::sync::Mutex::new(ProbeSlot {
            at: None,
            value: Value::Null,
            refreshing: false,
        }));
        let count = Arc::new(AtomicUsize::new(0));

        let s1 = Arc::clone(&slot);
        let c1 = Arc::clone(&count);
        let v1 = cached_probe(
            &s1,
            Duration::from_secs(60),
            Duration::from_millis(50),
            || {
                let c = Arc::clone(&c1);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    json!({"n": 42})
                }
            },
        )
        .await;
        assert_eq!(v1, Value::Null);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let s2 = Arc::clone(&slot);
        let c2 = Arc::clone(&count);
        let v2 = cached_probe(
            &s2,
            Duration::from_secs(60),
            Duration::from_millis(50),
            || {
                let c = Arc::clone(&c2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    json!({"n": 99})
                }
            },
        )
        .await;
        assert_eq!(v2, Value::Null);
        assert_eq!(count.load(Ordering::SeqCst), 1, "ttl 內不應再次嘗試探測");
    }

    #[tokio::test]
    async fn a_cancelled_refresh_releases_the_slot() {
        let slot = Arc::new(std::sync::Mutex::new(ProbeSlot {
            at: None,
            value: Value::Null,
            refreshing: false,
        }));
        let count = Arc::new(AtomicUsize::new(0));

        let s1 = Arc::clone(&slot);
        let h1 = tokio::spawn(async move {
            cached_probe(
                &s1,
                Duration::from_secs(60),
                Duration::from_secs(5),
                || async {
                    std::future::pending::<Value>().await
                },
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        h1.abort();
        let _ = h1.await;

        let s2 = Arc::clone(&slot);
        let c2 = Arc::clone(&count);
        let v2 = cached_probe(
            &s2,
            Duration::from_secs(60),
            Duration::from_secs(5),
            || {
                let c = Arc::clone(&c2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    json!({"recovered": true})
                }
            },
        )
        .await;
        assert_eq!(count.load(Ordering::SeqCst), 1, "被取消後 slot 應被釋放並執行新 probe");
        assert_eq!(v2, json!({"recovered": true}));
    }

    use crate::capabilities::{Cfg, DataDir};
    use crate::config::{ConfigFile, ConfigStore, ReleaseTriageCfg};
    use std::path::{Path, PathBuf};

    struct NoHooks;
    impl crate::config::ConfigChangeHooks for NoHooks {
        fn validate_projection(&self, _next: &ConfigFile) -> anyhow::Result<()> {
            Ok(())
        }
        fn audit_external_change(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
        fn audit_reload_unchanged(&self, _at: &'static std::panic::Location<'static>, _path: &Path) {}
        fn audit_write(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
    }

    type ReleaseTriageProbeFn = Arc<dyn Fn(&ReleaseTriageCfg) -> Option<Value> + Send + Sync>;

    struct MockApp {
        dir: PathBuf,
        store: ConfigStore,
        probe_fn: ReleaseTriageProbeFn,
    }

    impl DataDir for MockApp {
        fn data_dir(&self) -> &Path {
            &self.dir
        }
    }

    impl Cfg for MockApp {
        fn cfg(&self) -> &ConfigStore {
            &self.store
        }
    }

    impl crate::supervisor::ports::HostProbes for MockApp {
        async fn pane_shows_login_problem(&self, _run: &crate::db::Run) -> Option<bool> {
            None
        }
        async fn process_dump(&self, _host: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn identity_for_host(&self, _host: &str, _name: &str) -> Option<crate::config::IdentityCfg> {
            None
        }
        async fn deploy_behind(&self) -> Result<Value, String> {
            Ok(Value::Null)
        }
        async fn release_triage_health_probe(&self, cfg: &ReleaseTriageCfg) -> Option<Value> {
            (self.probe_fn)(cfg)
        }
        async fn due_actions_snapshot(&self) -> Value {
            Value::Null
        }
        fn primary_keep_warm_tick(&self) {}
        async fn restart_loop<F, Fut>(&self, _name: &'static str, _factory: F)
        where
            F: Fn() -> Fut + Send + Sync + 'static,
            Fut: std::future::Future<Output = ()> + Send + 'static,
        {
        }
        fn spawn_restartable<F, Fut>(&self, _name: &'static str, _factory: F)
        where
            F: Fn() -> Fut + Send + Sync + 'static,
            Fut: std::future::Future<Output = ()> + Send + 'static,
        {
        }
        async fn is_share_bot(&self, _bot_id: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    async fn create_mock_app(
        name: &str,
        repo: &str,
        probe: impl Fn(&ReleaseTriageCfg) -> Option<Value> + Send + Sync + 'static,
    ) -> (PathBuf, MockApp) {
        let base = crate::test_dirs::scratch_dir(&format!("am-test-{name}"));
        std::fs::create_dir_all(&base).unwrap();
        let cfg_path = base.join("config.toml");
        let mut file = ConfigFile::default();
        file.release_triage.publish = true;
        file.release_triage.repo = Some(repo.to_string());
        std::fs::write(&cfg_path, toml::to_string(&file).unwrap()).unwrap();
        let store = ConfigStore::load_with_hooks(cfg_path, Arc::new(NoHooks)).await.unwrap();
        let app = MockApp {
            dir: base.join("data"),
            store,
            probe_fn: Arc::new(probe),
        };
        (base, app)
    }

    #[tokio::test]
    async fn cross_app_health_isolation() {
        let (_d1, app_a) = create_mock_app("app_a", "owner/repo-a", |_| Some(json!({"app": "A"}))).await;
        let (_d2, app_b) = create_mock_app("app_b", "owner/repo-b", |_| Some(json!({"app": "B"}))).await;

        let va1 = release_triage_health(&app_a).await;
        assert_eq!(va1, json!({"app": "A"}));

        let vb1 = release_triage_health(&app_b).await;
        assert_eq!(vb1, json!({"app": "B"}), "App B 不得讀取 App A 的快取結果");

        let va2 = release_triage_health(&app_a).await;
        assert_eq!(va2, json!({"app": "A"}));
        let vb2 = release_triage_health(&app_b).await;
        assert_eq!(vb2, json!({"app": "B"}));
    }

    #[tokio::test]
    async fn config_change_forces_immediate_invalidation() {
        let (_d, app) = create_mock_app("app_inval", "owner/repo-v1", |cfg| {
            Some(json!({"repo": cfg.repo.clone()}))
        }).await;

        let v1 = release_triage_health(&app).await;
        assert_eq!(v1, json!({"repo": "owner/repo-v1"}));

        app.store.update(|c| {
            c.release_triage.repo = Some("owner/repo-v2".to_string());
            Ok(())
        }).await.unwrap();

        let v2 = release_triage_health(&app).await;
        assert_eq!(v2, json!({"repo": "owner/repo-v2"}), "設定變更後應立即失效並重新探測");
    }

    #[tokio::test]
    async fn inflight_probe_does_not_overwrite_after_config_reload() {
        let state = Arc::new(std::sync::Mutex::new("v1"));
        let state_clone = Arc::clone(&state);

        let (_d, app) = create_mock_app("app_inflight", "owner/repo-v1", move |_cfg| {
            let s = *state_clone.lock().unwrap();
            if s == "v1" {
                std::thread::sleep(Duration::from_millis(200));
                Some(json!({"version": "v1"}))
            } else {
                Some(json!({"version": "v2"}))
            }
        }).await;

        let app_ref = Arc::new(app);
        let a1 = Arc::clone(&app_ref);
        let h1 = tokio::spawn(async move {
            release_triage_health(&*a1).await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;

        *state.lock().unwrap() = "v2";
        app_ref.store.update(|c| {
            c.release_triage.repo = Some("owner/repo-v2".to_string());
            Ok(())
        }).await.unwrap();

        let v2 = release_triage_health(&*app_ref).await;
        assert_eq!(v2, json!({"version": "v2"}));

        let _ = h1.await;

        let cached = release_triage_health(&*app_ref).await;
        assert_eq!(cached, json!({"version": "v2"}), "舊的 inflight probe 不得覆寫新設定的快取");
    }

    /// #1324：每個資料目錄一格快取，但格子不能只進不出：很多個短命的執行個體（測試、臨時部署）用完，
    /// 這張 process 級的表不能跟著無限長大。
    #[tokio::test]
    async fn release_triage_slots_stay_bounded_across_short_lived_apps() {
        for i in 0..100 {
            let (_d, app) = create_mock_app(&format!("app_bound_{i}"), "owner/repo-bound", |_| Some(json!({"ok": true}))).await;
            assert_eq!(release_triage_health(&app).await, json!({"ok": true}));
        }
        let len = RELEASE_TRIAGE_SLOTS.lock().unwrap_or_else(|e| e.into_inner()).len();
        assert!(len <= 16, "快取格子只進不出：100 個執行個體用完後還有 {len} 格");
    }

    #[tokio::test]
    async fn same_key_hits_cache_within_ttl() {
        let probe_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = Arc::clone(&probe_count);
        let (_d, app) = create_mock_app("app_hit", "owner/repo-hit", move |_| {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(json!({"hit": true}))
        }).await;

        let v1 = release_triage_health(&app).await;
        assert_eq!(v1, json!({"hit": true}));
        assert_eq!(probe_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let v2 = release_triage_health(&app).await;
        assert_eq!(v2, json!({"hit": true}));
        assert_eq!(probe_count.load(std::sync::atomic::Ordering::SeqCst), 1, "TTL 內重複呼叫不應重複探測");
    }
}
