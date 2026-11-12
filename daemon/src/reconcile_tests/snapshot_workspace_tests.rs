use crate::testing as tt;
use serde_json::json;

#[tokio::test]
async fn an_empty_workspaces_array_does_not_clear_a_mapping_still_referenced_by_panes() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    sqlx::query("UPDATE projects SET workspace_id=? WHERE id=?")
        .bind(&ws.workspace_id)
        .bind(&env.project_id)
        .execute(&app.db)
        .await
        .unwrap();
    env.herdr.workspaces.lock().unwrap().clear();

    super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

    let mapped: Option<String> =
        sqlx::query_scalar("SELECT workspace_id FROM projects WHERE id=?").bind(&env.project_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(mapped.as_deref(), Some(ws.workspace_id.as_str()), "pane 還在那個 workspace，映射不能清掉");
}
