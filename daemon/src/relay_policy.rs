//! Current-state admission for bot relay prompts, shared by the API and durable retries.
use std::sync::Arc;
use crate::state::App;

#[derive(Debug)]
pub enum RelayDenied { Denied(String), Unavailable(String) }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission { NewRequest, Deliver, ResultNotice }

pub async fn relay_prompt_allowed(app: &Arc<App>, from_bot_id: &str, to_bot_id: &str, kind: Admission) -> Result<(), RelayDenied> {
    if kind == Admission::ResultNotice {
        let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots WHERE id=? AND deleted_at IS NULL)")
            .bind(from_bot_id).fetch_one(&app.db).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?;
        return if live { Ok(()) } else { Err(RelayDenied::Denied("requester unavailable".into())) };
    }
    if from_bot_id == to_bot_id { return Err(RelayDenied::Denied("relay_self".into())); }
    let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots WHERE id=? AND deleted_at IS NULL) AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id WHERE b.id=? AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.handed_off_to IS NULL)")
        .bind(from_bot_id).bind(to_bot_id).fetch_one(&app.db).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?;
    if !live { return Err(RelayDenied::Denied("relay participant or project unavailable".into())); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivery_and_result_notice_allow_offline_requester() {
        let env = crate::testing::env().await;
        let a = crate::testing::claude_bot(&env.app, &env.project_id, "policy-offline-a").await;
        let b = crate::testing::claude_bot(&env.app, &env.project_id, "policy-offline-b").await;
        assert!(relay_prompt_allowed(&env.app, &a.id, &b.id, Admission::Deliver).await.is_ok());
        assert!(relay_prompt_allowed(&env.app, &a.id, &b.id, Admission::ResultNotice).await.is_ok());
    }
}
