//! Current-state admission for bot relay prompts, shared by the API and durable retries.
use std::sync::Arc;
use crate::state::App;

#[derive(Debug)]
pub enum RelayDenied { Denied(String), MemberDisabled(String), Unavailable(String) }

/// Keep the machine-readable reason stable while explaining how the user can re-enable the member.
pub fn member_disabled_body(message: &str) -> serde_json::Value {
    serde_json::json!({"error":"forbidden", "reason":"member_disabled", "message":message})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission { NewRequest, Deliver, ResultNotice }

pub async fn relay_prompt_allowed(app: &Arc<App>, from_bot_id: &str, to_bot_id: &str, kind: Admission) -> Result<(), RelayDenied> {
    relay_prompt_basis(app, from_bot_id, to_bot_id, kind).await.map(|_| ())
}

/// 同 [`relay_prompt_allowed`]，另回「這次只靠什麼放行」的可信依據，給建 turn 時記在 `messages.relay_auth`：
/// `specialist:<專案 id>`＝因專案的專家派工開關放行；`deputy:<專案 id>`＝因代班中的副管理者放行；兩者都成立就都記（逗號分隔）；其他原本就放行的組合
/// （manager、advisor 目標、直屬 parent、AGM…）回 `None`。最終寫入（`lifecycle::relay_grant`）依這個依據正向確認仍成立。
pub async fn relay_prompt_basis(app: &Arc<App>, from_bot_id: &str, to_bot_id: &str, kind: Admission) -> Result<Option<String>, RelayDenied> {
    if kind == Admission::ResultNotice {
        let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots WHERE id=? AND deleted_at IS NULL)")
            .bind(from_bot_id).fetch_one(&app.db).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?;
        return if live { Ok(None) } else { Err(RelayDenied::Denied("requester unavailable".into())) };
    }
    if from_bot_id == to_bot_id { return Err(RelayDenied::Denied("relay_self".into())); }
    let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots WHERE id=? AND deleted_at IS NULL) AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id WHERE b.id=? AND b.deleted_at IS NULL AND p.deleted_at IS NULL AND p.handed_off_to IS NULL)")
        .bind(from_bot_id).bind(to_bot_id).fetch_one(&app.db).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?;
    if !live { return Err(RelayDenied::Denied("relay participant or project unavailable".into())); }
    // 改派給副管理者的請求：原本送管理者時已通過 NewRequest，送達時不重驗副管理者（跨專案送件者本來進不了它）。
    if kind == Admission::Deliver && crate::runners::relay_deputy::redirected_open(app, from_bot_id, to_bot_id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))? { return Ok(None); }
    let from = crate::db::bot(&app.db, from_bot_id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.ok_or_else(|| RelayDenied::Denied("source_missing".into()))?;
    let to = crate::db::bot(&app.db, to_bot_id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.ok_or_else(|| RelayDenied::Denied("target_missing".into()))?;
    let project = crate::db::project(&app.db, &from.project_id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.ok_or_else(|| RelayDenied::Denied("project_missing".into()))?;
    let target_project = crate::db::project(&app.db, &to.project_id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.ok_or_else(|| RelayDenied::Denied("project_missing".into()))?;
    // AGM 角色（巡檢／協調者）是跨專案的協調者：不受 expert 專案的跨專案與角色限制（使用者 2026-10-31）。
    let is_agm = crate::supervisor::roles::role_of_bot(&app.db, &from.id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?.is_some();
    if !is_agm && matches!(to.role.as_deref(), Some("executor" | "advisor" | "specialist")) && to.manager_use == 0 {
        return Err(RelayDenied::MemberDisabled(format!("{} 已被使用者停用（側欄『停用』鈕／manager_use=false），管理者無法派工；請使用者在 AG Man 側欄重新啟用", to.name)));
    }
    if is_agm { return Ok(None); }
    if to.role.as_deref() == Some("scheduler") { return Err(RelayDenied::Denied("schedule_bot_not_dispatchable".into())); }
    // 跨專案（使用者 2026-10-31／11-02）：目標在 expert 專案 → 只准送到它的頂層 manager（對外窗口），送件者若在 expert 專案也必須是 manager；
    // 目標在一般專案 → 寄件者是一般專案的 bot，或 expert 專案的頂層 manager（前台要能回覆外部 bot）。expert 的非管理者不能出專案。
    let from_is_front = from.role.as_deref() == Some("manager") && from.parent_bot_id.is_none();
    if from.project_id != to.project_id && (project.mode == "expert" || target_project.mode == "expert") {
        let to_front_desk = to.role.as_deref() == Some("manager") && to.parent_bot_id.is_none();
        let allowed = if target_project.mode == "expert" { to_front_desk && (project.mode != "expert" || from_is_front) } else { from_is_front };
        if !allowed { return Err(RelayDenied::Denied("expert_cross_project".into())); }
    }
    if project.mode == "expert" {
        let direct_family = from.parent_bot_id.as_deref() == Some(to.id.as_str());
        if !direct_family && kind == Admission::NewRequest {
            let run = crate::db::active_run(&app.db, &from.id).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?;
            let expected = format!("{}:{}:{}:{}", project.id, project.host, project.mode, from.role.as_deref().unwrap_or(""));
            if run.as_ref().and_then(|r| r.runtime_role_policy.as_deref()) != Some(expected.as_str()) { return Err(RelayDenied::Denied("role_not_applied".into())); }
        }
        let base_allowed = direct_family || matches!(to.role.as_deref(), Some("manager" | "advisor")) || from.role.as_deref() == Some("manager");
        // 記下**所有**成立的依據（逗號分隔）：送達時任一仍成立就放行。同時靠開關又在代班的專家，關掉開關仍可代班派工（Astra review5 #1）。
        if !base_allowed {
            let mut bases = Vec::new();
            if specialist_dispatch_allows(&project, &from, &to) { bases.push(format!("specialist:{}", project.id)); }
            if crate::runners::relay_deputy::acting(app, &from.id).await { bases.push(format!("deputy:{}", project.id)); }
            if !bases.is_empty() { return Ok(Some(bases.join(","))); }
        }
        if !base_allowed {
            let manager: Option<String> = sqlx::query_scalar("SELECT name FROM bots WHERE project_id=? AND role='manager' AND managed_by='user' AND deleted_at IS NULL LIMIT 1").bind(&project.id).fetch_optional(&app.db).await.map_err(|e| RelayDenied::Unavailable(e.to_string()))?;
            let manager = manager.as_deref().unwrap_or("（未指定）");
            let target_role = to.role.as_deref().unwrap_or("沒有角色的成員");
            // 開關只讓專家派工給執行者：其他組合提示開關沒有意義。
            let hint = if from.role.as_deref() == Some("specialist") && to.role.as_deref() == Some("executor") {
                format!("本專案的「專家可直接派工給執行者」開關已關閉，請改送給管理者 {manager}")
            } else {
                format!("請改送給管理者 {manager}")
            };
            return Err(RelayDenied::Denied(format!("{target_role} 只接受 manager 派工，{hint}")));
        }
    }
    Ok(None)
}

/// 專家派工總開關（`projects.specialist_dispatch`）：專案是 expert 且開關開著、送件者是同專案存活頂層 specialist、
/// 目標是同專案存活頂層 executor。child、刪除與角色都看 DB 當下；送達時 `lifecycle::relay_grant` 再以同樣條件正向確認。
pub(crate) fn specialist_dispatch_allows(project: &crate::db::Project, from: &crate::db::Bot, to: &crate::db::Bot) -> bool {
    project.mode == "expert" && project.specialist_dispatch
        && project.deleted_at.is_none() && project.handed_off_to.is_none()
        && from.project_id == project.id && to.project_id == project.id
        && from.parent_bot_id.is_none() && to.parent_bot_id.is_none()
        && from.deleted_at.is_none() && to.deleted_at.is_none()
        && from.role.as_deref() == Some("specialist") && to.role.as_deref() == Some("executor")
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
        let run = crate::testing::fake_run(&env.app, &manager.id).await;
        sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{other}:local:expert:manager")).bind(run).execute(&env.app.db).await.unwrap();
        assert!(relay_prompt_allowed(&env.app, &manager.id, &plain.id, Admission::NewRequest).await.is_ok(), "expert front-desk manager may contact plain-project bots");
        assert!(matches!(relay_prompt_allowed(&env.app, &plain.id, &worker.id, Admission::NewRequest).await, Err(RelayDenied::Denied(r)) if r == "expert_cross_project"), "plain bots still cannot reach expert executors");
        crate::supervisor::store::get_or_init(&env.app.db).await.unwrap();
        crate::supervisor::store::set_env(&env.app.db, &agm.id, &env.project_id, "/tmp").await.unwrap();
        assert!(relay_prompt_allowed(&env.app, &agm.id, &worker.id, Admission::NewRequest).await.is_ok());
        assert!(relay_prompt_allowed(&env.app, &agm.id, &worker.id, Admission::Deliver).await.is_ok());

        // manager_use=0 (member_disabled)
        sqlx::query("UPDATE bots SET manager_use=0 WHERE id=?").bind(&worker.id).execute(&env.app.db).await.unwrap();
        // Plain bot / manager to disabled worker -> member_disabled
        for kind in [Admission::NewRequest, Admission::Deliver] {
            let Err(RelayDenied::MemberDisabled(message)) = relay_prompt_allowed(&env.app, &manager.id, &worker.id, kind).await else { panic!("disabled member must be denied") };
            let body = member_disabled_body(&message);
            assert_eq!(body["reason"], "member_disabled");
            assert_eq!(body["message"], format!("{} 已被使用者停用（側欄『停用』鈕／manager_use=false），管理者無法派工；請使用者在 AG Man 側欄重新啟用", worker.name));
        }
        // ResultNotice is unaffected
        assert!(relay_prompt_allowed(&env.app, &manager.id, &worker.id, Admission::ResultNotice).await.is_ok());
        // AGM is unaffected
        assert!(relay_prompt_allowed(&env.app, &agm.id, &worker.id, Admission::NewRequest).await.is_ok());
    }
}
