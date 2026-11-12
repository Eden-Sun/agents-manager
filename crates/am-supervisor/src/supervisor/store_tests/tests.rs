use super::*;

#[tokio::test]
async fn native_api_turn_blocks_quiet_lease_including_own_requester() {
    let env = crate::testing::env().await;
    let bot = crate::db::ulid(); let run = crate::db::ulid(); let conv = crate::db::ulid(); let turn = crate::db::ulid();
    let now = crate::db::now(); let until = crate::db::iso_in(300);
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api','fixture',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)").bind(&run).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)").bind(&conv).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','pending',?)").bind(&turn).bind(&conv).bind(&run).bind(&now).execute(&env.app.db).await.unwrap();
    for delivery in ["pending", "ok"] {
        sqlx::query("UPDATE turns SET delivery=? WHERE id=?").bind(delivery).bind(&turn).execute(&env.app.db).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), acquire_lease(&env.app.db, &crate::db::ulid(), "owner", None, None, &until, true, Some(&bot), &serde_json::json!({}))).await.unwrap().unwrap();
        assert!(result.is_none(), "API requester cannot exclude its own live turn");
    }
    sqlx::query("UPDATE turns SET status='completed' WHERE id=?").bind(&turn).execute(&env.app.db).await.unwrap();
    assert!(acquire_lease(&env.app.db, &crate::db::ulid(), "owner", None, None, &until, true, Some(&bot), &serde_json::json!({})).await.unwrap().is_some());
    sqlx::query("UPDATE bots SET kind='claude' WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
    let cli_turn = crate::db::ulid();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','pending',?)").bind(&cli_turn).bind(&conv).bind(&run).bind(&now).execute(&env.app.db).await.unwrap();
    assert!(acquire_lease(&env.app.db, &crate::db::ulid(), "owner", None, None, &until, true, Some(&bot), &serde_json::json!({})).await.unwrap().is_some(), "CLI self exclusion remains intact");
}

/// #831：重申請讀完「要取代哪一筆 pending」、還沒寫的那一瞬，一個不相干的 writer commit 了一筆。deferred 交易這時升級寫鎖
/// 直接 517，申請整個被拒；寫鎖從讀之前就拿著，插進來的那一筆等，新申請照建、舊的照樣被取代。
#[tokio::test]
async fn an_unrelated_writer_between_the_supersede_read_and_the_write_does_not_refuse_the_approval() {
    let e = crate::testing::env().await;
    let p = &e.app.db;
    let old = create_approval_notifying(p, "agm-kick-831", "rebuild", "release rebuild", Some("aaa111"), None, None, None, None, true).await.unwrap();
    let other = crate::testing::arm_app_foreign_writer(&e.app, "approval_after_supersede_read", "agm-kick-831");

    let new = create_approval_notifying(p, "agm-kick-831", "rebuild", "release rebuild", Some("bbb222"), None, None, None, None, true)
        .await
        .expect("an unrelated writer must not refuse the approval request");
    assert_eq!(*other.lock().unwrap(), Some(false), "the request holds the write lock from its read on; the other writer waits");
    assert!(new.created);
    assert_eq!(new.superseded.as_deref(), Some(old.approval.id.as_str()));
    let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_approvals WHERE requester='agm-kick-831' AND status='pending'").fetch_one(p).await.unwrap();
    assert_eq!(pending, 1);
}

/// #831：收 incident 讀完開著的那一筆、還沒改的那一瞬，一個不相干的 writer commit 了一筆。deferred 交易這時 517，
/// incident 收不掉、這一輪的 `incident_resolved` 通知也沒了；寫鎖從讀之前就拿著，照樣收掉並通知一次。
#[tokio::test]
async fn an_unrelated_writer_between_the_incident_read_and_the_resolve_does_not_fail_it() {
    let e = crate::testing::env().await;
    let p = &e.app.db;
    open_incident(p, "host_disconnected", "mac-831", "warn", &serde_json::json!({})).await.unwrap();
    let other = crate::testing::arm_app_foreign_writer(&e.app, "incident_resolve_after_read", "mac-831");

    let resolved = resolve_incident_notifying(p, "host_disconnected", "mac-831", true)
        .await
        .expect("an unrelated writer must not make the resolve fail")
        .expect("the open incident is resolved");
    assert_eq!(*other.lock().unwrap(), Some(false), "the resolve holds the write lock from its read on; the other writer waits");
    assert_eq!(resolved.status, "resolved");
    let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='incident_resolved' AND event_key=?")
        .bind(format!("incident:{}:resolved", resolved.id))
        .fetch_one(p)
        .await
        .unwrap();
    assert_eq!(notices, 1);
}
