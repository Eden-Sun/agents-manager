use am_core::{LimitHit, PortError, QuotaKey, QuotaSnapshot};
use std::future::Future;

pub trait QuotaAccess: Send + Sync {
    fn resolve_key<'a>(
        &'a self,
        host: &'a str,
        provider: &'a str,
        identity: Option<&'a str>,
    ) -> impl Future<Output = Result<QuotaKey, PortError>> + Send + 'a;

    fn snapshot<'a>(
        &'a self,
        key: &'a QuotaKey,
    ) -> impl Future<Output = Result<Option<QuotaSnapshot>, PortError>> + Send + 'a;

    fn record_limit_hit<'a>(
        &'a self,
        key: &'a QuotaKey,
        hit: LimitHit,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a;

    fn clear_limit<'a>(
        &'a self,
        key: &'a QuotaKey,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a;

    fn should_probe<'a>(
        &'a self,
        key: &'a QuotaKey,
    ) -> impl Future<Output = Result<bool, PortError>> + Send + 'a;
}
