use super::ports::{MissionGateRules, SupervisorRepo};
use super::store;
use crate::lifecycle::LcError;
use crate::supervisor::store as supervisor_store;
use serde_json::Value;
use sqlx::SqlitePool;

impl SupervisorRepo for SqlitePool {
    const SUPERVISOR_ID: &'static str = supervisor_store::SUPERVISOR_ID;

    async fn mission_assignments(&self, mission_id: &str) -> anyhow::Result<Vec<supervisor_store::Assignment>> {
        supervisor_store::mission_assignments(self, mission_id).await
    }

    async fn assignment(&self, id: &str) -> anyhow::Result<Option<supervisor_store::Assignment>> {
        supervisor_store::assignment(self, id).await
    }

    async fn push_inbox(
        &self,
        event_key: &str,
        kind: &str,
        assignment_id: Option<&str>,
        bot_id: Option<&str>,
        turn_id: Option<&str>,
        payload: &Value,
    ) -> anyhow::Result<Option<String>> {
        supervisor_store::push_inbox(self, event_key, kind, assignment_id, bot_id, turn_id, payload).await
    }

    async fn review_assignment(
        &self,
        id: &str,
        decision: &str,
        actor: &str,
        source: &str,
        reason: Option<&str>,
        evidence: Option<&str>,
        followup_assignment_id: Option<&str>,
    ) -> anyhow::Result<Option<supervisor_store::Assignment>> {
        supervisor_store::review(self, id, decision, actor, source, reason, evidence, followup_assignment_id).await
    }
}

impl MissionGateRules for SqlitePool {
    fn user_pause_reason(paused_reason: Option<&str>) -> Option<&str> {
        crate::supervisor::api::user_pause_reason(paused_reason)
    }

    fn mission_gate(mission: &store::Mission) -> Result<(), LcError> {
        crate::supervisor::api::mission_gate(mission)
    }
}

impl<T: super::ports::CallerOps + Send + Sync> crate::relay_auth::RelayTokenVerify for T {
    fn ct_eq(&self, a: &str, b: &str) -> bool {
        super::ports::CallerOps::ct_eq(self, a, b)
    }
}

impl<T: super::ports::CallerOps + Send + Sync> crate::relay_auth::RelayAuthOps for T {
    async fn is_agm_role(&self, headers: &axum::http::HeaderMap) -> Result<bool, LcError> {
        Ok(super::ports::CallerOps::actor_role(self, headers).await?.is_some())
    }
}
