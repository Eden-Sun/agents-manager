use super::*;
#[tokio::test]
async fn active_remote_run_without_daemon_url_is_pending_until_restarted() {
    let env = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&env.app, &env.project_id, "remote-pending").await;
    sqlx::query("UPDATE projects SET host='remote' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let run = crate::testing::fake_run(&env.app, &bot.id).await;
    let state = state_json(&env.app).await.unwrap();
    assert_eq!(state["projects"][0]["bots"][0]["daemon_url_pending"], true);
    assert_eq!(state["projects"][0]["bots"][0]["role_pending"], true);
    sqlx::query("UPDATE runs SET daemon_url_injected=1 WHERE id=?").bind(run).execute(&env.app.db).await.unwrap();
    let state = state_json(&env.app).await.unwrap();
    assert_eq!(state["projects"][0]["bots"][0]["daemon_url_pending"], false);
}

#[test]
fn pre_expert_runs_in_plain_projects_are_not_role_pending() {
    assert!(!role_policy_drifted(None, "p", "local", "parent_child", None));
    assert!(role_policy_drifted(None, "p", "local", "expert", None));
    assert!(role_policy_drifted(None, "p", "local", "parent_child", Some("executor")));
    assert!(!role_policy_drifted(Some("p:local:expert:executor"), "p", "local", "expert", Some("executor")));
    assert!(role_policy_drifted(Some("p:local:parent_child:"), "p", "local", "expert", None));
}
