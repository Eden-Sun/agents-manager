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
pub const PERMIT_TTL: Duration = Duration::from_secs(60);

/// `herdr agent start --timeout` 最長 300 秒，加上收尾的餘裕。
pub const MAX_PERMIT_TTL: Duration = Duration::from_secs(330);

/// 這次 spawn 的 permit 有效多久：至少 [`PERMIT_TTL`]；shim 帶了 `agent start` 的 `--timeout`（毫秒）就涵蓋它再加 30 秒餘裕，
/// 上限 [`MAX_PERMIT_TTL`]。看不懂的值當沒帶——寧可短（過期只是不再擋輪替）也不要讓亂填的值把 permit 撐成永久。
pub fn permit_ttl(timeout_ms: &str) -> Duration {
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
pub enum FinishClaimError {
    Missing,
    PaneMismatch,
    InProgress,
}

#[derive(Default)]
pub struct Gate {
    pub rotating: HashSet<String>,
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

    pub fn begin_rotation(&mut self, bot_id: &str) -> Result<(), FenceError> {
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

    pub fn reserve(&mut self, bot_id: &str, permit_id: &str, ttl: Duration) -> Result<(), ()> {
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

    #[cfg(any(test, feature = "test-hooks"))]
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn permit_active(&self, bot_id: &str, permit_id: &str) -> bool {
        self.permits.get(bot_id).is_some_and(|permits| permits.get(permit_id).is_some_and(|p| p.alive(Instant::now())))
    }

    /// Bind a permit before any asynchronous registration. A replay with a different pane must
    /// never get as far as the DB write, even when two finish requests arrive together.
    pub fn claim_finish(&mut self, bot_id: &str, permit_id: &str, pane_id: &str) -> Result<(), FinishClaimError> {
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
    pub fn release(&mut self, bot_id: &str, permit_id: &str) -> bool {
        if self.permits.get(bot_id).and_then(|permits| permits.get(permit_id)).is_some_and(|p| p.finishing) {
            return false;
        }
        self.remove(bot_id, permit_id)
    }

    pub fn complete_finish(&mut self, bot_id: &str, permit_id: &str, pane_id: &str) -> bool {
        let valid = self
            .permits
            .get(bot_id)
            .and_then(|permits| permits.get(permit_id))
            .is_some_and(|p| p.finishing && p.pane_id.as_deref() == Some(pane_id));
        valid && self.remove(bot_id, permit_id)
    }

    pub fn unclaim_finish(&mut self, bot_id: &str, permit_id: &str, pane_id: &str) {
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
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn age_permit(&mut self, bot_id: &str, permit_id: &str, age: Duration) {
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



/// 開登入殼的閘門。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
#[allow(dead_code)]
pub trait CredentialSpawnGate: Send + Sync {
    fn credential_spawn_gate(&self) -> &std::sync::Mutex<crate::credential_spawn::Gate>;
}
