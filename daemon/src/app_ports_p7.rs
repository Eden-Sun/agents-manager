//! P7 mission 的窄介面由 `App`／`SqlitePool` 實作的地方（crate 拆分第 3 步）。每個方法逐行委派給 mission 原本呼叫的函式，
//! 不加任何邏輯，所以行為與錯誤型別不變。介面本身在 `mission/ports.rs`；這個檔案是 mission 唯一還知道 supervisor／quota／
//! tools／lifecycle／api 實作細節的地方（composition root 側的 adapter，不屬於未來的 am-mission）。
//!
//! 模組掛在 `mission/mod.rs`（`#[path]`），不碰 `lib.rs`，免得跟其他包各自加 `mod` 行互相衝突。

use crate::db;
use crate::lifecycle::{self, LcError, LcResult, PromptOut};
use crate::mission::ports::{AgmRole, CallerOps, EventOps, GroupTurnOps, IdentityOps, KnownIdentity, SupervisorOps};
use crate::quota::{self, Quota};
use crate::state::App;
use crate::supervisor::roles::{self, Role};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::Value;
use std::future::Future;

fn agm_role(r: Role) -> AgmRole {
    match r {
        Role::Patrol => AgmRole::Patrol,
        Role::Responder => AgmRole::Responder,
    }
}

impl IdentityOps for App {
    async fn billing_identity(&self, bot: &db::Bot) -> anyhow::Result<Option<String>> {
        quota::billing_identity(&self.shared(), bot).await
    }
    async fn known_identities(&self, host: &str) -> Vec<KnownIdentity> {
        crate::tools::identities_for_host(&self.shared(), host)
            .await
            .into_iter()
            .map(|i| KnownIdentity { shares_default: quota::identity_shares_default(&i.kind, &i.env), kind: i.kind, name: i.name })
            .collect()
    }
    async fn quota_base_for_host(&self, host: &str, kind: &str, identity: Option<&str>) -> String {
        quota::quota_base_for_host(&self.shared(), host, kind, identity).await
    }
    async fn quota_readings(&self, host: &str, bases: &[String]) -> Vec<Option<Quota>> {
        let quotas = self.quotas.lock().await;
        bases.iter().map(|base| quotas.get(&quota::quota_key(host, base)).cloned()).collect()
    }
}

impl CallerOps for App {
    async fn verified_bot_id(&self, headers: &HeaderMap) -> Result<Option<String>, LcError> {
        crate::supervisor::bot_requests::verified_bot_id(&self.shared(), headers).await
    }
    async fn actor_role(&self, headers: &HeaderMap) -> Result<Option<AgmRole>, LcError> {
        Ok(crate::supervisor::bot_requests::actor_role(&self.shared(), headers).await?.map(agm_role))
    }
    async fn role_of_bot(&self, bot_id: &str) -> anyhow::Result<Option<AgmRole>> {
        Ok(roles::role_of_bot(&self.db, bot_id).await?.map(agm_role))
    }
    async fn responder_configured(&self) -> anyhow::Result<bool> {
        roles::responder_configured(&self.db).await
    }
    fn ct_eq(&self, a: &str, b: &str) -> bool {
        crate::api::ct_eq(a, b)
    }
}

impl SupervisorOps for App {
    type OpGuard = crate::supervisor::OpGuard;
    #[track_caller]
    fn supervisor_lock(&self) -> impl Future<Output = Self::OpGuard> {
        crate::supervisor::lock()
    }
    async fn cancel_assignment(&self, assignment_id: &str, headers: &HeaderMap, actor: &str, source: &str, reason: String) -> Result<Value, LcError> {
        let review = crate::runners::supervisor::api::ReviewIn {
            decision: "cancel".into(),
            actor: Some(actor.into()),
            source: Some(source.into()),
            reason: Some(reason),
            evidence: None,
            followup_text: None,
            followup_request_id: None,
            followup_bot_id: None,
            ownership: Vec::new(),
        };
        crate::runners::supervisor::api::post_review(State(self.shared()), Path(assignment_id.to_string()), headers.clone(), Json(review))
            .await
            .map(|Json(v)| v)
    }
    async fn delete_bot(&self, bot_id: &str) -> Result<(), String> {
        crate::api::delete_bot(State(self.shared()), Path(bot_id.to_string())).await.map(|_| ()).map_err(|e| format!("{e:?}"))
    }
}

impl EventOps for App {
    async fn emit_event(&self, kind: &str, data: Value) {
        self.emit(kind, data).await
    }
}

impl GroupTurnOps for App {
    async fn prompt_grouped(
        &self,
        bot_id: &str,
        text: &str,
        client_request_id: &str,
        group_id: Option<&str>,
        deliver: Option<&str>,
        attachment_ids: &[String],
        relay_from: Option<&str>,
    ) -> LcResult<PromptOut> {
        lifecycle::prompt_grouped(&self.shared(), bot_id, text, client_request_id, group_id, deliver, attachment_ids, relay_from).await
    }
    fn owed_as_unknown(&self, res: LcResult<PromptOut>) -> LcResult<PromptOut> {
        lifecycle::owed_as_unknown(res)
    }
    async fn insert_message_grouped(
        &self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
        group_id: Option<&str>,
    ) -> anyhow::Result<db::Message> {
        lifecycle::insert_message_grouped(&self.shared(), conversation_id, turn_id, role, content, source, incomplete, snapshot, group_id).await
    }
    fn cursor_not_found(&self, reason: &str, message_id: &str) -> LcError {
        crate::api::cursor_not_found(reason, message_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::ports::MissionGateRules;
    use crate::mission::ports::SupervisorRepo;
    use crate::supervisor::store as supervisor_store;
    use sqlx::SqlitePool;

    #[test]
    fn the_supervisor_id_the_mission_queries_bind_is_the_supervisors_own() {
        assert_eq!(<SqlitePool as SupervisorRepo>::SUPERVISOR_ID, supervisor_store::SUPERVISOR_ID);
    }

    #[test]
    fn the_gate_rules_are_the_supervisors_rules() {
        assert_eq!(<SqlitePool as MissionGateRules>::user_pause_reason(Some(" max_rounds ")), None);
        assert_eq!(<SqlitePool as MissionGateRules>::user_pause_reason(Some("要想一想")), Some("要想一想"));
        assert_eq!(<SqlitePool as MissionGateRules>::user_pause_reason(None), None);
    }
}
