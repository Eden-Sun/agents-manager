use super::*;

#[tokio::test]
async fn patrol_may_restart_and_start_top_level_bots_only() {
    let env = crate::testing::env().await;
    let patrol = crate::testing::claude_bot(&env.app, &env.project_id, "scope-patrol").await;
    let other = crate::testing::claude_bot(&env.app, &env.project_id, "scope-other").await;
    let child = crate::testing::claude_bot(&env.app, &env.project_id, "scope-child").await;
    sqlx::query("UPDATE bots SET managed_by='child',parent_bot_id=? WHERE id=?").bind(&other.id).bind(&child.id).execute(&env.app.db).await.unwrap();
    let plain = crate::testing::claude_bot(&env.app, &env.project_id, "scope-plain").await;
    let denied = |r: Result<(), LcError>| matches!(r, Err(LcError::Forbidden(_)));
    let restart = format!("/api/bots/{}/restart", other.id);
    let start = format!("/api/bots/{}/start", other.id);
    assert!(denied(authorize_bot_path(&env.app, &patrol.id, "POST", &restart).await), "not patrol yet");
    crate::supervisor::store::get_or_init(&env.app.db).await.unwrap();
    crate::supervisor::store::set_env(&env.app.db, &patrol.id, &env.project_id, "/tmp").await.unwrap();
    assert!(authorize_bot_path(&env.app, &patrol.id, "POST", &restart).await.is_ok());
    assert!(authorize_bot_path(&env.app, &patrol.id, "POST", &start).await.is_ok());
    assert!(denied(authorize_bot_path(&env.app, &patrol.id, "POST", &format!("/api/bots/{}/restart", child.id)).await), "child stays in its parent's tree");
    assert!(denied(authorize_bot_path(&env.app, &patrol.id, "POST", &format!("/api/bots/{}/stop", other.id)).await), "only restart/start are widened");
    assert!(denied(authorize_bot_path(&env.app, &patrol.id, "DELETE", &format!("/api/bots/{}", other.id)).await));
    assert!(denied(authorize_bot_path(&env.app, &plain.id, "POST", &restart).await), "non-patrol bots unchanged");
}
