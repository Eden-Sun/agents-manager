use super::explicit_retirement;

#[tokio::test]
async fn fresh_proven_unexpired_session_is_retired() {
    let env = super::retention_tests::fixture().await;
    sqlx::query("UPDATE api_bot_sessions SET expires_at=? WHERE id='r2-session'").bind(crate::db::iso_in(3600)).execute(&env.app.db).await.unwrap();
    let target = crate::lifecycle::api_bot_port::ApiFreshTarget { session_id: "r2-session".into(), generation: 1 };
    assert!(matches!(explicit_retirement::retire(&env.app, "r2-bot", &target).await, Ok(crate::lifecycle::api_bot_port::ApiFreshResult::Cleared)));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='r2-session'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 0, "a proven exact target must be deleted even before expiry");
}

fn target() -> crate::lifecycle::api_bot_port::ApiFreshTarget {
    crate::lifecycle::api_bot_port::ApiFreshTarget { session_id: "r2-session".into(), generation: 1 }
}

#[tokio::test]
async fn fresh_preserves_cost_and_public_messages() {
    let env = super::retention_tests::fixture().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,generation,revision,checkpoint_json,created_at,updated_at,expires_at,retired_at) VALUES('fresh-other-session','r2-bot',1,1,'{}',?,?,?,?)").bind(&now).bind(&now).bind(crate::db::iso_in(3600)).bind(&now).execute(&env.app.db).await.unwrap();
    for (id, session, state, estimate, hold) in [
        ("fresh-reserved", Some("r2-session"), "reserved", None, 123),
        ("fresh-recorded", Some("r2-session"), "recorded", Some(44_i64), 124),
        ("fresh-failed", Some("r2-session"), "failed", None, 125),
        ("fresh-unknown", Some("r2-session"), "unknown", None, 126),
        ("fresh-null", None, "reserved", None, 127),
        ("fresh-other", Some("fresh-other-session"), "reserved", None, 128),
    ] {
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,estimated_microusd,pricing_version,created_at) VALUES(?,?,'r2-bot',1,0,'fixture','standard',?,?,?,'fixture',?)").bind(id).bind(session).bind(state).bind(hold).bind(estimate).bind(&now).execute(&env.app.db).await.unwrap();
    }
    assert!(matches!(explicit_retirement::retire(&env.app, "r2-bot", &target()).await, Ok(crate::lifecycle::api_bot_port::ApiFreshResult::Cleared)));
    let rows: Vec<(String,String,Option<i64>,i64)> = sqlx::query_as("SELECT id,state,estimated_microusd,reserved_microusd FROM api_bot_attempts ORDER BY id").fetch_all(&env.app.db).await.unwrap();
    assert_eq!(rows, vec![("fresh-failed".into(),"failed".into(),None,125),("fresh-null".into(),"reserved".into(),None,127),("fresh-other".into(),"reserved".into(),None,128),("fresh-recorded".into(),"recorded".into(),Some(44),124),("fresh-reserved".into(),"unknown".into(),None,123),("fresh-unknown".into(),"unknown".into(),None,126)]);
    let receipt: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_join_receipts").fetch_one(&env.app.db).await.unwrap();
    let message: String = sqlx::query_scalar("SELECT content FROM messages WHERE id='r2-message'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(receipt, 0); assert_eq!(message, "fixture");
}

#[tokio::test]
async fn fresh_unresolved_and_wrong_target_never_deletes() {
    for mutation in ["pending", "fenced", "no_receipt", "wrong_revision", "wrong_text", "read_error", "wrong_generation", "missing_target"] {
        let env = super::retention_tests::fixture().await;
        match mutation {
            "pending" => { sqlx::query("UPDATE api_bot_join_receipts SET state='pending',processed_at=NULL").execute(&env.app.db).await.unwrap(); }
            "fenced" => { sqlx::query("UPDATE api_bot_join_receipts SET state='fenced'").execute(&env.app.db).await.unwrap(); }
            "no_receipt" => { sqlx::query("DELETE FROM api_bot_join_receipts").execute(&env.app.db).await.unwrap(); }
            "wrong_revision" => { sqlx::query("UPDATE api_bot_join_receipts SET revision=4999").execute(&env.app.db).await.unwrap(); }
            "wrong_text" => { sqlx::query("UPDATE messages SET content='different'").execute(&env.app.db).await.unwrap(); }
            "read_error" => { sqlx::query("ALTER TABLE messages RENAME TO fresh_missing_messages").execute(&env.app.db).await.unwrap(); }
            _ => {}
        }
        let mut t = target();
        if mutation == "wrong_generation" { t.generation = 2; }
        if mutation == "missing_target" { t.session_id = "missing".into(); }
        assert!(explicit_retirement::retire(&env.app, "r2-bot", &t).await.is_err(), "{mutation}");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='r2-session'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, 1, "{mutation}");
    }
}

#[tokio::test]
async fn fresh_commit_fault_is_unconfirmed_and_does_not_choose_successor() {
    let env = super::retention_tests::fixture().await;
    sqlx::query("CREATE TABLE fresh_parent(id INTEGER PRIMARY KEY)").execute(&env.app.db).await.unwrap();
    sqlx::query("CREATE TABLE fresh_child(parent INTEGER REFERENCES fresh_parent(id) DEFERRABLE INITIALLY DEFERRED)").execute(&env.app.db).await.unwrap();
    sqlx::query("CREATE TRIGGER fresh_commit_fault AFTER DELETE ON api_bot_sessions BEGIN INSERT INTO fresh_child(parent) VALUES(7); END").execute(&env.app.db).await.unwrap();
    assert!(matches!(explicit_retirement::retire(&env.app, "r2-bot", &target()).await, Err(crate::lifecycle::api_bot_port::ApiFreshError::Unconfirmed)));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='r2-session'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn fresh_dropped_caller_does_not_cancel_postcommit_owner() {
    let env = super::retention_tests::fixture().await;
    sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE api_bot_sessions SET id='fresh-drop' WHERE id='r2-session'").execute(&env.app.db).await.unwrap();
    let now = crate::db::now();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES('fresh-drop','r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    let entered = std::sync::Arc::new(tokio::sync::Notify::new()); let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let enter_hook = entered.clone(); let release_hook = release.clone();
    crate::lifecycle::race_point::arm("api_fresh_after_commit", "fresh-drop", move || async move { enter_hook.notify_one(); release_hook.notified().await; });
    let app = env.app.clone();
    let caller = tokio::spawn(async move {
        let target = crate::lifecycle::api_bot_port::ApiFreshTarget { session_id: "fresh-drop".into(), generation: 1 };
        explicit_retirement::retire(&app, "r2-bot", &target).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified()).await.unwrap();
    caller.abort(); let _ = caller.await;
    release.notify_one();
    env.app.background_tasks.close();
    tokio::time::timeout(std::time::Duration::from_secs(10), env.app.background_tasks.wait()).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='fresh-drop'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn fresh_dropped_caller_after_commit_poll_owner_finishes() {
    let env = super::retention_tests::fixture().await;
    sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE api_bot_sessions SET id='fresh-commit-drop' WHERE id='r2-session'").execute(&env.app.db).await.unwrap();
    let now = crate::db::now();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES('fresh-commit-drop','r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    let entered = std::sync::Arc::new(tokio::sync::Notify::new()); let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let enter_hook = entered.clone(); let release_hook = release.clone();
    crate::lifecycle::race_point::arm("api_fresh_commit_polled", "fresh-commit-drop", move || async move { enter_hook.notify_one(); release_hook.notified().await; });
    let app = env.app.clone();
    let caller = tokio::spawn(async move {
        let target = crate::lifecycle::api_bot_port::ApiFreshTarget { session_id: "fresh-commit-drop".into(), generation: 1 };
        explicit_retirement::retire(&app, "r2-bot", &target).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified()).await.unwrap();
    caller.abort(); let _ = caller.await;
    release.notify_one();
    env.app.background_tasks.close();
    tokio::time::timeout(std::time::Duration::from_secs(10), env.app.background_tasks.wait()).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='fresh-commit-drop'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn fresh_real_commit_synthetic_lost_ack_reads_exact_absence() {
    let env = super::retention_tests::fixture().await;
    sqlx::query("DELETE FROM api_bot_join_receipts WHERE session_id='r2-session'").execute(&env.app.db).await.unwrap();
    let session = crate::db::ulid();
    sqlx::query("UPDATE api_bot_sessions SET id=? WHERE id='r2-session'").bind(&session).execute(&env.app.db).await.unwrap();
    let now = crate::db::now();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES(?,'r2-bot','r2-run','r2-turn',1,5000,'completed','fixture','applied',?,?)").bind(&session).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('fresh-lost-hold',?,'r2-bot',1,0,'fixture','standard','reserved',123,'fixture',?)").bind(&session).bind(&now).execute(&env.app.db).await.unwrap();
    explicit_retirement::arm_lost_ack_for_test(&session);
    let target = crate::lifecycle::api_bot_port::ApiFreshTarget { session_id: session.clone(), generation: 1 };
    assert!(matches!(explicit_retirement::retire(&env.app, "r2-bot", &target).await, Ok(crate::lifecycle::api_bot_port::ApiFreshResult::Cleared)));
    let state: (i64, i64, String, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM api_bot_sessions WHERE id=?),(SELECT count(*) FROM api_bot_join_receipts WHERE session_id=?),(SELECT state FROM api_bot_attempts WHERE id='fresh-lost-hold'),(SELECT reserved_microusd FROM api_bot_attempts WHERE id='fresh-lost-hold')").bind(&session).bind(&session).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(state, (0, 0, "unknown".into(), 123));
    let content: String = sqlx::query_scalar("SELECT content FROM messages WHERE id='r2-message'").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(content, "fixture");
    assert!(matches!(explicit_retirement::retire(&env.app, "r2-bot", &target).await, Err(crate::lifecycle::api_bot_port::ApiFreshError::Unconfirmed)));
}

#[tokio::test]
async fn fresh_same_target_can_retry_after_definite_rollback() {
    let env = super::retention_tests::fixture().await;
    sqlx::query("CREATE TRIGGER fresh_reject_delete BEFORE DELETE ON api_bot_sessions BEGIN SELECT RAISE(ABORT,'fixture'); END").execute(&env.app.db).await.unwrap();
    assert!(matches!(explicit_retirement::retire(&env.app, "r2-bot", &target()).await, Err(crate::lifecycle::api_bot_port::ApiFreshError::Storage)));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='r2-session'").fetch_one(&env.app.db).await.unwrap(); assert_eq!(n, 1);
    sqlx::query("DROP TRIGGER fresh_reject_delete").execute(&env.app.db).await.unwrap();
    assert!(matches!(explicit_retirement::retire(&env.app, "r2-bot", &target()).await, Ok(crate::lifecycle::api_bot_port::ApiFreshResult::Cleared)));
}

#[tokio::test]
async fn fresh_receipt_evidence_bound_is_enforced_for_exact_target() {
    for history in [4095_i64, 4096] {
        let env = super::retention_tests::fixture().await;
        let now = crate::db::now();
        sqlx::query("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x+1<?) INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) SELECT printf('h%04d',x),'r2-conv','r2-run','web','completed','ok',? FROM n").bind(history).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) SELECT 'r2-session','r2-bot','r2-run',t.id,1,CAST(substr(t.id,2) AS INTEGER),'completed','fixture','applied',?,? FROM turns t WHERE t.id LIKE 'h%'").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) SELECT 'm-'||t.id,'r2-conv',t.id,'assistant','fixture','transcript',? FROM turns t WHERE t.id LIKE 'h%'").bind(&now).execute(&env.app.db).await.unwrap();
        let result = explicit_retirement::retire(&env.app, "r2-bot", &target()).await;
        if history == 4095 { assert!(matches!(result, Ok(crate::lifecycle::api_bot_port::ApiFreshResult::Cleared))); }
        else { assert!(matches!(result, Err(crate::lifecycle::api_bot_port::ApiFreshError::Unresolved))); }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_sessions WHERE id='r2-session'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, i64::from(history == 4096));
    }
}
