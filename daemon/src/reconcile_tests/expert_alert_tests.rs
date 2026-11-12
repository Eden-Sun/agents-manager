use super::*;
#[tokio::test]
async fn non_manager_child_is_reported_once_to_manager() {
    let env = crate::testing::env().await;
    let manager = crate::testing::claude_bot(&env.app, &env.project_id, "manager").await;
    let worker = crate::testing::claude_bot(&env.app, &env.project_id, "worker").await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
    alert_expert_child(&*env.app, &worker, "kid", "child-id").await.unwrap();
    alert_expert_child(&*env.app, &worker, "kid", "child-id").await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM supervisor_inbox WHERE kind='ops_alert' AND event_key='expert-unauthorized-child:child-id'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 1);
    let conv = db::conversation_id(&env.app.db, &manager.id).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id=? AND content LIKE '%worker%'").bind(conv).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 1);
}
