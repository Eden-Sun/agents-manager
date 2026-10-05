//! P9 Quota seam adapters for agents-managerd.
//!
//! Implements `am_ports::QuotaAccess` for `App` and `AppQuotaAccess`,
//! providing a decoupled port for quota state and operations.

use crate::state::App;
use am_core::{LimitHit as CoreLimitHit, PortError, QuotaKey, QuotaSnapshot as CoreQuotaSnapshot};
use am_ports::QuotaAccess;
use std::future::Future;

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

impl QuotaAccess for App {
    fn resolve_key<'a>(
        &'a self,
        host: &'a str,
        provider: &'a str,
        identity: Option<&'a str>,
    ) -> impl Future<Output = Result<QuotaKey, PortError>> + Send + 'a {
        async move {
            let base = crate::quota::resolve_quota_base(self, host, provider, identity)
                .await
                .map_err(|error| PortError::Unavailable(error.to_string()))?;
            Ok(crate::quota::quota_key(host, &base))
        }
    }

    fn snapshot<'a>(
        &'a self,
        key: &'a QuotaKey,
    ) -> impl Future<Output = Result<Option<CoreQuotaSnapshot>, PortError>> + Send + 'a {
        async move {
            let quotas = self.quotas.lock().await;
            Ok(quotas.get(key).cloned().map(Into::into))
        }
    }

    fn store_snapshot<'a>(
        &'a self,
        key: &'a QuotaKey,
        snapshot: CoreQuotaSnapshot,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            let (host, base) = match key.split_once('/') {
                Some((host, base)) if !host.is_empty() && !base.is_empty() => (host, base),
                Some(_) => return Err(PortError::InvalidInput(format!("invalid quota key `{key}`"))),
                None if !key.is_empty() => (crate::config::LOCAL_HOST, key.as_str()),
                None => return Err(PortError::InvalidInput("quota key cannot be empty".into())),
            };
            crate::quota::set(self, host, base, snapshot.into()).await;
            Ok(())
        }
    }

    fn record_limit_hit<'a>(
        &'a self,
        key: &'a QuotaKey,
        hit: CoreLimitHit,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            let (host, base) = match key.split_once('/') {
                Some((h, b)) => (h, b),
                None => (crate::config::LOCAL_HOST, key.as_str()),
            };
            let hit_local: crate::quota::LimitHit = hit.into();
            crate::quota::restore_limit_hit(self, host, base, hit_local).await;
            Ok(())
        }
    }

    fn clear_limit<'a>(
        &'a self,
        key: &'a QuotaKey,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a {
        async move {
            let (host, base) = match key.split_once('/') {
                Some((h, b)) => (h, b),
                None => (crate::config::LOCAL_HOST, key.as_str()),
            };
            crate::quota::clear_limit_hit(self, host, base).await;
            Ok(())
        }
    }

    fn should_probe<'a>(
        &'a self,
        key: &'a QuotaKey,
    ) -> impl Future<Output = Result<bool, PortError>> + Send + 'a {
        async move {
            if crate::quota::is_retired_agy_quota_key(key) {
                return Ok(false);
            }
            let (host, base) = match key.split_once('/') {
                Some((h, b)) => (h, b),
                None => (crate::config::LOCAL_HOST, key.as_str()),
            };
            let (kind, _) = match base.split_once(':') {
                Some((k, id)) => (k, Some(id)),
                None => (base, None),
            };
            match kind {
                "grok" => {
                    let grok_key = crate::quota::quota_key(host, "grok");
                    let logged_in = self
                        .tools
                        .lock()
                        .await
                        .get(host)
                        .and_then(|t| t.tools.get("grok"))
                        .and_then(|t| t.logged_in);
                    let cooling = crate::quota_grok::cooling_down(&grok_key);
                    Ok(crate::quota_grok::should_probe_grok(logged_in, cooling))
                }
                "claude" => {
                    let full_key = crate::quota::quota_key(host, base);
                    let cooling = crate::quota_claude::cooling_down(&full_key, false, false);
                    Ok(!cooling)
                }
                _ => Ok(true),
            }
        }
    }
}

/// A borrowed adapter struct for `App` that implements `QuotaAccess`.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct AppQuotaAccess<'a>(pub &'a App);

impl<'a> QuotaAccess for AppQuotaAccess<'a> {
    fn resolve_key<'b>(
        &'b self,
        host: &'b str,
        provider: &'b str,
        identity: Option<&'b str>,
    ) -> impl Future<Output = Result<QuotaKey, PortError>> + Send + 'b {
        self.0.resolve_key(host, provider, identity)
    }

    fn snapshot<'b>(
        &'b self,
        key: &'b QuotaKey,
    ) -> impl Future<Output = Result<Option<CoreQuotaSnapshot>, PortError>> + Send + 'b {
        self.0.snapshot(key)
    }

    fn store_snapshot<'b>(
        &'b self,
        key: &'b QuotaKey,
        snapshot: CoreQuotaSnapshot,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'b {
        self.0.store_snapshot(key, snapshot)
    }

    fn record_limit_hit<'b>(
        &'b self,
        key: &'b QuotaKey,
        hit: CoreLimitHit,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'b {
        self.0.record_limit_hit(key, hit)
    }

    fn clear_limit<'b>(
        &'b self,
        key: &'b QuotaKey,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'b {
        self.0.clear_limit(key)
    }

    fn should_probe<'b>(
        &'b self,
        key: &'b QuotaKey,
    ) -> impl Future<Output = Result<bool, PortError>> + Send + 'b {
        self.0.should_probe(key)
    }
}

#[allow(dead_code)]
pub fn app_quota_access(app: &App) -> AppQuotaAccess<'_> {
    AppQuotaAccess(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn app_implements_quota_access_contract() {
        let env = crate::testing::env().await;
        let app = env.app.clone();

        let key = "claude:cc1".to_string();
        assert_eq!(
            app.resolve_key(crate::config::LOCAL_HOST, "claude", None)
                .await
                .unwrap(),
            "claude"
        );
        // Initially no snapshot exists
        let snap = app.snapshot(&key).await.unwrap();
        assert_eq!(snap, None);

        // Record a limit hit through the port
        let hit = CoreLimitHit {
            message: "Usage limit reached".to_string(),
            until: Some(crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2))),
            at: crate::db::now(),
            bucket: Some("five_hour".to_string()),
        };
        app.record_limit_hit(&key, hit.clone()).await.unwrap();

        // Snapshot now exists and contains the limit hit
        let snap = app.snapshot(&key).await.unwrap();
        assert!(snap.is_some());
        let q = snap.unwrap();
        assert_eq!(q.limit_hit, Some(hit));

        // Clear limit through the port
        app.clear_limit(&key).await.unwrap();
        let snap = app.snapshot(&key).await.unwrap().unwrap();
        assert_eq!(snap.limit_hit, None);

        // Should probe check
        let probe = app.should_probe(&key).await.unwrap();
        assert!(probe);

        // Retired agy key returns false
        let retired = "agy:claude-gpt".to_string();
        assert!(!app.should_probe(&retired).await.unwrap());

        // Test with AppQuotaAccess adapter
        let access = app_quota_access(&app);
        let snap2 = access.snapshot(&key).await.unwrap().unwrap();
        assert_eq!(snap2.limit_hit, None);
        assert_eq!(
            access
                .resolve_key(crate::config::LOCAL_HOST, "claude", None)
                .await
                .unwrap(),
            "claude"
        );

        let snapshot = CoreQuotaSnapshot {
            five_hour: Some(am_core::Window {
                used_pct: 23.0,
                resets_at: None,
                observed_at: Some(crate::db::now()),
            }),
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: Some("test-plan".into()),
            updated_at: crate::db::now(),
            source: "quota-port-test".into(),
            account: None,
            host: crate::config::LOCAL_HOST.into(),
        };
        app.store_snapshot(&key, snapshot.clone()).await.unwrap();
        assert_eq!(app.snapshot(&key).await.unwrap(), Some(snapshot.clone()));
        access.store_snapshot(&key, snapshot.clone()).await.unwrap();
        assert_eq!(access.snapshot(&key).await.unwrap(), Some(snapshot));
    }
}
