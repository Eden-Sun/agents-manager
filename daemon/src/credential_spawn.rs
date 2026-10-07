//! Credential inheritance fences for herdr pane / child-agent creation.
//!
//! A bot's already-running CLI can create panes outside the daemon's SQLite transaction. The shim
//! therefore reserves a spawn permit before invoking herdr and reports the resulting pane before
//! releasing it. Rotation sets a short-lived fence in the same registry before checking descendants;
//! permits already in flight make rotation fail closed, and new permits are refused until the token
//! update commits or the rotation aborts.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// A permit the shim never finishes (herdr failed, curl timed out after reserve, Ctrl-C) must not
/// block credential rotation until the daemon restarts (#664).
const PERMIT_TTL: Duration = Duration::from_secs(60);

/// `herdr agent start --timeout` 最長 300 秒，加上收尾的餘裕。
const MAX_PERMIT_TTL: Duration = Duration::from_secs(330);

/// 這次 spawn 的 permit 有效多久：至少 [`PERMIT_TTL`]；shim 帶了 `agent start` 的 `--timeout`（毫秒）就涵蓋它再加 30 秒餘裕，
/// 上限 [`MAX_PERMIT_TTL`]。看不懂的值當沒帶——寧可短（過期只是不再擋輪替）也不要讓亂填的值把 permit 撐成永久。
pub(crate) fn permit_ttl(timeout_ms: &str) -> Duration {
    let Ok(ms) = timeout_ms.trim().parse::<u64>() else { return PERMIT_TTL };
    let wanted = Duration::from_millis(ms.min(MAX_PERMIT_TTL.as_millis() as u64)) + Duration::from_secs(30);
    wanted.clamp(PERMIT_TTL, MAX_PERMIT_TTL)
}

/// 一張 permit：何時開的、有效多久。
struct Permit {
    at: Instant,
    ttl: Duration,
    pane_id: Option<String>,
    finishing: bool,
}

impl Permit {
    fn alive(&self, now: Instant) -> bool {
        self.finishing || now.saturating_duration_since(self.at) < self.ttl
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FinishClaimError {
    Missing,
    PaneMismatch,
    InProgress,
}

#[derive(Default)]
pub struct Gate {
    pub(crate) rotating: HashSet<String>,
    permits: HashMap<String, HashMap<String, Permit>>,
}

impl Gate {
    fn sweep(&mut self, bot_id: &str, now: Instant) {
        let Some(permits) = self.permits.get_mut(bot_id) else { return };
        permits.retain(|_, p| p.alive(now));
        if permits.is_empty() {
            self.permits.remove(bot_id);
        }
    }

    pub(crate) fn begin_rotation(&mut self, bot_id: &str) -> Result<(), FenceError> {
        if !self.rotating.insert(bot_id.to_string()) {
            return Err(FenceError::AlreadyRotating);
        }
        self.sweep(bot_id, Instant::now());
        let in_flight = self.permits.get(bot_id).map_or(0, HashMap::len);
        if in_flight > 0 {
            self.rotating.remove(bot_id);
            return Err(FenceError::SpawnsInFlight(in_flight));
        }
        Ok(())
    }

    pub(crate) fn reserve(&mut self, bot_id: &str, permit_id: &str, ttl: Duration) -> Result<(), ()> {
        if self.rotating.contains(bot_id) {
            return Err(());
        }
        self.sweep(bot_id, Instant::now());
        self.permits.entry(bot_id.to_string()).or_default().insert(
            permit_id.to_string(),
            Permit { at: Instant::now(), ttl, pane_id: None, finishing: false },
        );
        Ok(())
    }

    #[cfg(test)]
    fn permit_active(&self, bot_id: &str, permit_id: &str) -> bool {
        self.permits.get(bot_id).is_some_and(|permits| permits.get(permit_id).is_some_and(|p| p.alive(Instant::now())))
    }

    /// Bind a permit before any asynchronous registration. A replay with a different pane must
    /// never get as far as the DB write, even when two finish requests arrive together.
    pub(crate) fn claim_finish(&mut self, bot_id: &str, permit_id: &str, pane_id: &str) -> Result<(), FinishClaimError> {
        self.sweep(bot_id, Instant::now());
        let permit = self
            .permits
            .get_mut(bot_id)
            .and_then(|permits| permits.get_mut(permit_id))
            .filter(|p| p.alive(Instant::now()))
            .ok_or(FinishClaimError::Missing)?;
        if permit.finishing {
            return Err(FinishClaimError::InProgress);
        }
        if permit.pane_id.as_deref().is_some_and(|bound| bound != pane_id) {
            return Err(FinishClaimError::PaneMismatch);
        }
        permit.pane_id.get_or_insert_with(|| pane_id.to_string());
        permit.finishing = true;
        Ok(())
    }

    /// An abort can only discard an untouched permit; once finish started, its registration must
    /// either complete or return an error and release the claim for a retry.
    pub(crate) fn release(&mut self, bot_id: &str, permit_id: &str) -> bool {
        if self.permits.get(bot_id).and_then(|permits| permits.get(permit_id)).is_some_and(|p| p.finishing) {
            return false;
        }
        self.remove(bot_id, permit_id)
    }

    pub(crate) fn complete_finish(&mut self, bot_id: &str, permit_id: &str, pane_id: &str) -> bool {
        let valid = self
            .permits
            .get(bot_id)
            .and_then(|permits| permits.get(permit_id))
            .is_some_and(|p| p.finishing && p.pane_id.as_deref() == Some(pane_id));
        valid && self.remove(bot_id, permit_id)
    }

    pub(crate) fn unclaim_finish(&mut self, bot_id: &str, permit_id: &str, pane_id: &str) {
        if let Some(p) = self
            .permits
            .get_mut(bot_id)
            .and_then(|permits| permits.get_mut(permit_id))
            .filter(|p| p.pane_id.as_deref() == Some(pane_id))
        {
            p.finishing = false;
        }
    }

    fn remove(&mut self, bot_id: &str, permit_id: &str) -> bool {
        let Some(permits) = self.permits.get_mut(bot_id) else { return false };
        let removed = permits.remove(permit_id).is_some();
        if permits.is_empty() {
            self.permits.remove(bot_id);
        }
        removed
    }

    /// Test-only: pretend `permit_id` was reserved `age` ago so TTL can be checked without sleeping.
    #[cfg(test)]
    pub(crate) fn age_permit(&mut self, bot_id: &str, permit_id: &str, age: Duration) {
        if let Some(p) = self.permits.get_mut(bot_id).and_then(|p| p.get_mut(permit_id)) {
            p.at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FenceError {
    AlreadyRotating,
    SpawnsInFlight(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::Json;
    use crate::runners::credential_spawn::{begin, finish, RotationFence, SpawnBegin, SpawnFinish};

    #[test]
    fn a_pending_rotation_refuses_new_spawns_and_an_existing_spawn_refuses_rotation() {
        let mut gate = Gate::default();
        let bot = "gate-test";
        gate.reserve(bot, "before-rotation", PERMIT_TTL).unwrap();
        assert_eq!(gate.begin_rotation(bot).unwrap_err(), FenceError::SpawnsInFlight(1));
        assert!(gate.reserve(bot, "during-failed-rotation", PERMIT_TTL).is_ok(), "failed fence setup must be cleared");
        gate.release(bot, "before-rotation");
        gate.release(bot, "during-failed-rotation");

        gate.begin_rotation(bot).unwrap();
        assert!(gate.reserve(bot, "during-rotation", PERMIT_TTL).is_err());
        gate.rotating.remove(bot);
        assert!(gate.reserve(bot, "after-aborted-rotation", PERMIT_TTL).is_ok());
    }

    /// #664：失敗的 agent start 留下的 permit 過了 TTL 就不再擋輪替。
    #[test]
    fn an_expired_spawn_permit_does_not_block_rotation() {
        let mut gate = Gate::default();
        let bot = "ttl";
        gate.reserve(bot, "stuck", PERMIT_TTL).unwrap();
        gate.age_permit(bot, "stuck", PERMIT_TTL + Duration::from_secs(1));
        gate.begin_rotation(bot).unwrap();
        assert!(!gate.permit_active(bot, "stuck"));
    }

    #[test]
    fn a_spawn_permit_is_bound_to_one_pane_and_abort_cannot_race_finish() {
        let mut gate = Gate::default();
        let bot = "single-pane";
        let permit = "one-use";
        gate.reserve(bot, permit, PERMIT_TTL).unwrap();

        gate.claim_finish(bot, permit, "w1:p1").unwrap();
        assert!(!gate.release(bot, permit), "abort must not remove a pane registration while finish is in progress");
        assert_eq!(gate.claim_finish(bot, permit, "w1:p2"), Err(FinishClaimError::InProgress));

        gate.unclaim_finish(bot, permit, "w1:p1");
        assert_eq!(gate.claim_finish(bot, permit, "w1:p2"), Err(FinishClaimError::PaneMismatch));
        gate.claim_finish(bot, permit, "w1:p1").unwrap();
        assert!(gate.complete_finish(bot, permit, "w1:p1"));
        assert_eq!(gate.claim_finish(bot, permit, "w1:p1"), Err(FinishClaimError::Missing));
    }

    fn bot_headers() -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("X-AM-Bot-Token", "tok".parse().unwrap());
        h
    }

    /// `herdr agent start --timeout` 最長 300 秒：子 agent 慢慢啟動時，permit 不能在 `finish` 之前就過期
    /// （以前固定 60 秒：pane 其實開好了，finish 回 409「permit missing」、shim 報尚未登記）。
    /// permit 的有效期涵蓋那次的 timeout，期間照樣擋輪替。
    #[tokio::test]
    async fn a_long_agent_start_timeout_keeps_its_permit_until_finish() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "slow-spawner").await;
        let begin_with = |timeout_ms: &str| {
            let (app, id, t) = (app.clone(), bot.id.clone(), timeout_ms.to_string());
            async move {
                let (st, Json(v)) = begin(State(app), bot_headers(), axum::extract::Form(SpawnBegin { bot_id: id, timeout_ms: t })).await;
                assert_eq!(st, StatusCode::OK, "{v}");
                v["permit_id"].as_str().unwrap().to_string()
            }
        };
        let permit = begin_with("120000").await;
        app.credential_spawn_gate.lock().unwrap().age_permit(&bot.id, &permit, Duration::from_secs(100));
        assert_eq!(RotationFence::begin(&app, &bot.id).err(), Some(FenceError::SpawnsInFlight(1)), "100 秒時 120 秒 timeout 的 spawn 還在飛，要擋輪替");
        let (st, Json(v)) = finish(
            State(app.clone()),
            bot_headers(),
            axum::extract::Form(SpawnFinish { bot_id: bot.id.clone(), permit_id: permit.clone(), pane_id: "w1:p9".into(), purpose: String::new() }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "pane 開好了、finish 要登記成功：{v}");

        // 沒帶 timeout（split／tab create、舊 shim）維持 60 秒。
        let quick = begin_with("").await;
        app.credential_spawn_gate.lock().unwrap().age_permit(&bot.id, &quick, PERMIT_TTL + Duration::from_secs(1));
        assert!(RotationFence::begin(&app, &bot.id).is_ok(), "沒帶 timeout 的 permit 過 60 秒就不擋輪替");
    }

    /// 有效期有上限（herdr 自己最多等 300 秒）：亂填的 timeout 不能讓 permit 永遠擋輪替。
    #[test]
    fn the_permit_lifetime_is_bounded() {
        assert_eq!(permit_ttl(""), PERMIT_TTL);
        assert_eq!(permit_ttl("abc"), PERMIT_TTL);
        assert_eq!(permit_ttl("30000"), PERMIT_TTL, "預設 30 秒的啟動：60 秒綽綽有餘");
        assert_eq!(permit_ttl("120000"), Duration::from_secs(150));
        assert_eq!(permit_ttl("999999999999"), MAX_PERMIT_TTL);
    }
}

/// 開登入殼的閘門。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
#[allow(dead_code)]
pub trait CredentialSpawnGate: Send + Sync {
    fn credential_spawn_gate(&self) -> &std::sync::Mutex<crate::credential_spawn::Gate>;
}
