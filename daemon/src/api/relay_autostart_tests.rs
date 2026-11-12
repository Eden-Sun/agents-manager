use super::*;

async fn fixture() -> (crate::testing::Env, String, String) {
    let env = crate::testing::env().await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let manager = crate::testing::claude_bot(&env.app, &env.project_id, "start-manager").await;
    let worker = crate::testing::claude_bot(&env.app, &env.project_id, "start-worker").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&worker.id).execute(&env.app.db).await.unwrap();
    let run = crate::testing::fake_run(&env.app, &manager.id).await;
    sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:manager", env.project_id)).bind(run).execute(&env.app.db).await.unwrap();
    (env, manager.id, worker.id)
}

#[tokio::test]
async fn stopped_disabled_member_has_explanatory_error() {
    let (env, manager, worker) = fixture().await;
    sqlx::query("UPDATE bots SET manager_use=0 WHERE id=?").bind(&worker).execute(&env.app.db).await.unwrap();
    let err = ensure_relay_member_ready(&env.app, &manager, &worker).await.unwrap_err();
    assert!(matches!(err, LcError::Forbidden(v) if v["error"] == "member_not_dispatchable" && v["message"].as_str().unwrap().contains("manager_use=false")));
}

/// 專家的 `specialist` 送件者：`switch_on` 控制專家派工開關。
async fn specialist_fixture(switch_on: bool) -> (crate::testing::Env, String, String) {
    let (env, manager, worker) = fixture().await;
    sqlx::query("UPDATE bots SET role='specialist' WHERE id=?").bind(&manager).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE bot_id=?").bind(format!("{}:local:expert:specialist", env.project_id)).bind(&manager).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE projects SET specialist_dispatch=? WHERE id=?").bind(switch_on).bind(&env.project_id).execute(&env.app.db).await.unwrap();
    (env, manager, worker)
}

#[tokio::test]
async fn stopped_member_is_started_for_a_specialist_when_dispatch_switch_is_on() {
    let (env, specialist, worker) = specialist_fixture(true).await;
    ensure_relay_member_ready(&env.app, &specialist, &worker).await.unwrap();
    let run = db::active_run(&env.app.db, &worker).await.unwrap().unwrap();
    assert_eq!(run.state, "running");
}

#[tokio::test]
async fn stopped_member_is_not_started_for_a_specialist_when_dispatch_switch_is_off() {
    let (env, specialist, worker) = specialist_fixture(false).await;
    let err = ensure_relay_member_ready(&env.app, &specialist, &worker).await.unwrap_err();
    assert!(matches!(err, LcError::Conflict(v) if v["error"] == "member_stopped" && v["message"].as_str().unwrap().contains("管理者")));
    assert!(db::active_run(&env.app.db, &worker).await.unwrap().is_none());
}

#[tokio::test]
async fn stopped_member_outside_specialist_scope_is_not_started() {
    let (env, specialist, _worker) = specialist_fixture(true).await;
    // 頂層 advisor 不在專家可直接派工的範圍（只有執行者），即使開關開著也不能替它啟動。
    let advisor = crate::testing::claude_bot(&env.app, &env.project_id, "scope-advisor").await;
    sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&advisor.id).execute(&env.app.db).await.unwrap();
    let err = ensure_relay_member_ready(&env.app, &specialist, &advisor.id).await.unwrap_err();
    assert!(matches!(err, LcError::Conflict(v) if v["error"] == "member_stopped"));
    assert!(db::active_run(&env.app.db, &advisor.id).await.unwrap().is_none());
}

#[tokio::test]
async fn existing_start_is_waited_until_ready() {
    let (env, manager, worker) = fixture().await;
    let run = crate::testing::fake_run(&env.app, &worker).await;
    sqlx::query("UPDATE runs SET state='starting', agent_status='unknown' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
    let app = env.app.clone();
    let run2 = run.clone();
    let update = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(run2).execute(&app.db).await.unwrap();
    });
    ensure_relay_member_ready(&env.app, &manager, &worker).await.unwrap();
    update.await.unwrap();
}

#[tokio::test]
async fn stopped_member_is_started_and_relay_request_is_persisted() {
    use tower::ServiceExt;
    let (env, manager, worker) = fixture().await;
    let req = axum::http::Request::builder().method("POST")
        .uri(format!("/api/bots/{worker}/prompt"))
        .header("host", "127.0.0.1:7799")
        .header("X-AM-Bot-Id", &manager).header("X-AM-Bot-Token", "tok")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"text":"autostart work","client_request_id":"autostart-test","expect_result":true}"#)).unwrap();
    let res = router(env.app.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert!(status.is_success(), "{status}: {value}");
    let run = db::active_run(&env.app.db, &worker).await.unwrap().unwrap();
    assert_eq!(run.state, "running");
    // The test herdr creates a pane but does not simulate a composer until explicitly attached.
    // Reconcile the durable request once that composer becomes available.
    let pane = run.pane_id.as_deref().unwrap();
    env.herdr.live_pane(pane, crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
    crate::runners::relay_requests::sweep(&env.app).await;
    let stored: (String, String, Option<String>) = sqlx::query_as("SELECT to_bot_id,prompt_text,to_turn_id FROM relay_requests WHERE id=?")
        .bind(value["request_id"].as_str().unwrap()).fetch_one(&env.app.db).await.unwrap();
    assert_eq!((stored.0, stored.1), (worker, "autostart work".to_string()));
    assert!(stored.2.is_some(), "reconcile 未建立投遞 turn：{value}");
    assert!(env.herdr.pane(pane).unwrap().transcript.iter().any(|line| line.contains("autostart work")));
}

#[tokio::test]
async fn start_failure_is_returned_to_caller() {
    let (env, manager, worker) = fixture().await;
    sqlx::query("UPDATE bots SET identity='missing-autostart-test-identity' WHERE id=?").bind(&worker).execute(&env.app.db).await.unwrap();
    let err = ensure_relay_member_ready(&env.app, &manager, &worker).await.unwrap_err();
    assert!(matches!(err, LcError::Unavailable(v) if v["error"] == "member_start_failed" && v["message"].as_str().unwrap().contains("start-worker")));
}
