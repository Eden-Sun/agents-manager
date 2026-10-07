//! Conversions between daemon quota values and the stable port snapshots.

impl From<crate::quota::Window> for am_core::Window {
    fn from(w: crate::quota::Window) -> Self {
        Self {
            used_pct: w.used_pct,
            resets_at: w.resets_at,
            observed_at: w.observed_at,
        }
    }
}

impl From<am_core::Window> for crate::quota::Window {
    fn from(w: am_core::Window) -> Self {
        Self {
            used_pct: w.used_pct,
            resets_at: w.resets_at,
            observed_at: w.observed_at,
        }
    }
}

impl From<crate::quota::ResetCredits> for am_core::ResetCredits {
    fn from(rc: crate::quota::ResetCredits) -> Self {
        Self {
            available: rc.available,
            title: rc.title,
            expires_at: rc.expires_at,
        }
    }
}

impl From<am_core::ResetCredits> for crate::quota::ResetCredits {
    fn from(rc: am_core::ResetCredits) -> Self {
        Self {
            available: rc.available,
            title: rc.title,
            expires_at: rc.expires_at,
        }
    }
}

impl From<crate::quota::LimitHit> for am_core::LimitHit {
    fn from(lh: crate::quota::LimitHit) -> Self {
        Self {
            message: lh.message,
            until: lh.until,
            at: lh.at,
            bucket: lh.bucket,
        }
    }
}

impl From<am_core::LimitHit> for crate::quota::LimitHit {
    fn from(lh: am_core::LimitHit) -> Self {
        Self {
            message: lh.message,
            until: lh.until,
            at: lh.at,
            bucket: lh.bucket,
        }
    }
}

impl From<crate::quota::Quota> for am_core::Quota {
    fn from(q: crate::quota::Quota) -> Self {
        Self {
            five_hour: q.five_hour.map(Into::into),
            seven_day: q.seven_day.map(Into::into),
            fable: q.fable.map(Into::into),
            reset_credits: q.reset_credits.map(Into::into),
            limit_hit: q.limit_hit.map(Into::into),
            plan: q.plan,
            updated_at: q.updated_at,
            source: q.source,
            account: q.account,
            host: q.host,
        }
    }
}

impl From<am_core::Quota> for crate::quota::Quota {
    fn from(q: am_core::Quota) -> Self {
        Self {
            five_hour: q.five_hour.map(Into::into),
            seven_day: q.seven_day.map(Into::into),
            fable: q.fable.map(Into::into),
            reset_credits: q.reset_credits.map(Into::into),
            limit_hit: q.limit_hit.map(Into::into),
            plan: q.plan,
            updated_at: q.updated_at,
            source: q.source,
            account: q.account,
            host: q.host,
        }
    }
}
