//! Composition-side adapter for r2a9 seam cuts.
//!
//! Provides narrow port implementations on `App` for lower modules
//! (`quota`, `hook_inbox`), severing reverse dependencies to upper groups.

use std::sync::Arc;
use crate::state::App;

/// 查詢這顆 bot 是否有欠著未記進配額的 limit hit。
pub trait OwedLimitHitProbe {
    fn owed_limit_hit<'a>(
        &'a self,
        bot: &'a crate::db::Bot,
        identity: Option<&'a str>,
    ) -> impl std::future::Future<Output = Option<crate::quota::LimitHit>> + Send + 'a;
}

impl OwedLimitHitProbe for Arc<App> {
    async fn owed_limit_hit(&self, bot: &crate::db::Bot, identity: Option<&str>) -> Option<crate::quota::LimitHit> {
        crate::turn_error::owed_limit_hit(self, bot, identity).await
    }
}

/// Durable hook inbox 事件處理器。
pub trait HookProcessor: Send + Sync {
    fn process_hook<'a>(
        &'a self,
        body: &'a crate::hook_body::HookBody,
        inbox_event_id: Option<&'a str>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'a;
}

impl HookProcessor for Arc<App> {
    async fn process_hook(&self, body: &crate::hook_body::HookBody, inbox_event_id: Option<&str>) -> anyhow::Result<()> {
        crate::hookrecv::process_for(self, body, inbox_event_id).await
    }
}
