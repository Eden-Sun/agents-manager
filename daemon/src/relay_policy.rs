//! Current-state admission for bot relay prompts, shared by the API and durable retries.
use std::sync::Arc;
use crate::state::App;

#[derive(Debug)]
pub enum RelayDenied { Denied(String), Unavailable(String) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    let from = crate::db::bot(&app.db, from_bot_id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.ok_or_else(|| RelayDenied::Denied("source_missing".into()))?;
    let to = crate::db::bot(&app.db, to_bot_id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.ok_or_else(|| RelayDenied::Denied("target_missing".into()))?;
    let project = crate::db::project(&app.db, &from.project_id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.ok_or_else(|| RelayDenied::Denied("project_missing".into()))?;
    let target_project = crate::db::project(&app.db, &to.project_id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.ok_or_else(|| RelayDenied::Denied("project_missing".into()))?;
    // AGM 角色（巡檢／協調者）是跨專案的協調者：不受 expert 專案的跨專案與角色限制（使用者 2026-10-31）。
    if crate::supervisor::roles::role_of_bot(&app.db, &from.id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.is_some() { return Ok(()); }
    // 跨專案只准送到 expert 專案的 manager（對外窗口，使用者 2026-10-31）；送件者若在 expert 專案也必須是它的 manager。
    let to_front_desk = target_project.mode == "expert" && to.role.as_deref() == Some("manager") && to.parent_bot_id.is_none();
    let from_may_leave = project.mode != "expert" || (from.role.as_deref() == Some("manager") && from.parent_bot_id.is_none());
    if from.project_id != to.project_id && (project.mode == "expert" || target_project.mode == "expert") && !(to_front_desk && from_may_leave) { return Err(RelayDenied::Denied("expert_cross_project".into())); }
    if project.mode == "expert" {
        let direct_family = from.parent_bot_id.as_deref() == Some(to.id.as_str());
        if !direct_family && kind == Admission::NewRequest {
            let run = crate::db::active_run(&app.db, &from.id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?;
            let expected = format!("{}:{}:{}:{}", project.id, project.host, project.mode, from.role.as_deref().unwrap_or(""));
            if run.as_ref().and_then(|r| r.runtime_role_policy.as_deref()) != Some(expected.as_str()) { return Err(RelayDenied::Denied("role_not_applied".into())); }
        }
        if !direct_family && !matches!(to.role.as_deref(), Some("manager" | "advisor")) && from.role.as_deref() != Some("manager") {
            let manager: Option<String> = sqlx::query_scalar("SELECT name FROM bots WHERE project_id=? AND role='manager' AND managed_by='user' AND deleted_at IS NULL LIMIT 1").bind(&project.id).fetch_optional(&app.db).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?;
            return Err(RelayDenied::Denied(format!("executor 只接受 manager 派工，請改送給管理者 {}", manager.as_deref().unwrap_or("（未指定）"))));
        }
    }
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

#[cfg(test)]
mod expert_tests {
    use super::*;
    #[tokio::test]
    async fn expert_policy_rechecks_role_project_and_run_snapshot() {
        let env = crate::testing::env().await;
        let manager = crate::testing::claude_bot(&env.app, &env.project_id, "manager").await;
        let advisor = crate::testing::claude_bot(&env.app, &env.project_id, "advisor").await;
        let worker = crate::testing::claude_bot(&env.app, &env.project_id, "worker").await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        for (bot, role) in [(&manager, "manager"), (&advisor, "advisor"), (&worker, "executor")] {
            sqlx::query("UPDATE bots SET role=? WHERE id=?").bind(role).bind(&bot.id).execute(&env.app.db).await.unwrap();
            let run = crate::testing::fake_run(&env.app, &bot.id).await;
            sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:{role}", env.project_id)).bind(run).execute(&env.app.db).await.unwrap();
        }
        assert!(relay_prompt_allowed(&env.app, &manager.id, &advisor.id, Admission::NewRequest).await.is_ok());
        assert!(relay_prompt_allowed(&env.app, &advisor.id, &manager.id, Admission::NewRequest).await.is_ok());
        assert!(relay_prompt_allowed(&env.app, &worker.id, &advisor.id, Admission::NewRequest).await.is_ok());
        assert!(matches!(relay_prompt_allowed(&env.app, &advisor.id, &worker.id, Admission::Deliver).await, Err(RelayDenied::Denied(_))));
        sqlx::query("UPDATE bots SET parent_bot_id=?, managed_by='child' WHERE id=?").bind(&advisor.id).bind(&worker.id).execute(&env.app.db).await.unwrap();
        assert!(matches!(relay_prompt_allowed(&env.app, &advisor.id, &worker.id, Admission::NewRequest).await, Err(RelayDenied::Denied(_))));
        assert!(matches!(relay_prompt_allowed(&env.app, &advisor.id, &worker.id, Admission::Deliver).await, Err(RelayDenied::Denied(_))));
        assert!(relay_prompt_allowed(&env.app, &worker.id, &advisor.id, Admission::Deliver).await.is_ok());
        sqlx::query("UPDATE bots SET parent_bot_id=NULL, managed_by='user' WHERE id=?").bind(&worker.id).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE bot_id=?").bind(crate::db::now()).bind(&advisor.id).execute(&env.app.db).await.unwrap();
        assert!(relay_prompt_allowed(&env.app, &advisor.id, &manager.id, Admission::Deliver).await.is_ok());
        assert!(relay_prompt_allowed(&env.app, &advisor.id, &manager.id, Admission::ResultNotice).await.is_ok());
        sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
        assert!(matches!(relay_prompt_allowed(&env.app, &manager.id, &advisor.id, Admission::NewRequest).await, Err(RelayDenied::Denied(_))));
        assert!(matches!(relay_prompt_allowed(&env.app, &manager.id, &worker.id, Admission::Deliver).await, Err(RelayDenied::Denied(_))));
        let other = crate::db::ulid();
        sqlx::query("INSERT INTO projects(id,path,label,created_at) VALUES (?,?,?,?)").bind(&other).bind(format!("/tmp/{other}")).bind("other").bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE bots SET project_id=? WHERE id=?").bind(&other).bind(&advisor.id).execute(&env.app.db).await.unwrap();
        assert!(matches!(relay_prompt_allowed(&env.app, &worker.id, &advisor.id, Admission::NewRequest).await, Err(RelayDenied::Denied(_))));
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(crate::db::now()).bind(&worker.id).execute(&env.app.db).await.unwrap();
        assert!(matches!(relay_prompt_allowed(&env.app, &worker.id, &manager.id, Admission::Deliver).await, Err(RelayDenied::Denied(_))));
    }
}

#[cfg(test)]
mod agm_cross_project_tests {
    use super::*;

    #[tokio::test]
    async fn agm_role_reaches_expert_projects_but_plain_bots_do_not() {
        let env = crate::testing::env().await;
        let agm = crate::testing::claude_bot(&env.app, &env.project_id, "agm-cross").await;
        let plain = crate::testing::claude_bot(&env.app, &env.project_id, "plain-cross").await;
        let other = crate::db::ulid();
        sqlx::query("INSERT INTO projects(id,path,label,host,created_at,mode) VALUES(?,?,'expert-x','local',?,'expert')")
            .bind(&other).bind(format!("/tmp/{other}")).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let worker = crate::testing::claude_bot(&env.app, &other, "expert-worker").await;
        sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&worker.id).execute(&env.app.db).await.unwrap();
        assert!(matches!(relay_prompt_allowed(&env.app, &plain.id, &worker.id, Admission::NewRequest).await, Err(RelayDenied::Denied(r)) if r == "expert_cross_project"));
        let manager = crate::testing::claude_bot(&env.app, &other, "expert-front").await;
        sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
        assert!(relay_prompt_allowed(&env.app, &plain.id, &manager.id, Admission::NewRequest).await.is_ok(), "other projects reach the expert manager");
        assert!(matches!(relay_prompt_allowed(&env.app, &worker.id, &plain.id, Admission::NewRequest).await, Err(RelayDenied::Denied(r)) if r == "expert_cross_project"), "expert executors do not leave the project");
        crate::supervisor::store::get_or_init(&env.app.db).await.unwrap();
        crate::supervisor::store::set_env(&env.app.db, &agm.id, &env.project_id, "/tmp").await.unwrap();
        assert!(relay_prompt_allowed(&env.app, &agm.id, &worker.id, Admission::NewRequest).await.is_ok());
        assert!(relay_prompt_allowed(&env.app, &agm.id, &worker.id, Admission::Deliver).await.is_ok());
    }
}
