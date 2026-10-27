use super::{adapter::AdapterIdentity, protocol::Response, receipt::*};
use am_base::db::api_bot::{self, Checkpoint, Fence};
use serde_json::json;

async fn fixture() -> (crate::testing::Env, AdapterIdentity) {
    let e = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at)VALUES('b',?,'receipt','claude-api','fake',?)")
        .bind(&e.project_id).bind(&now).execute(&e.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at)VALUES('r','b','running',?)")
        .bind(&now)
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at)VALUES('c','b',?)")
        .bind(&now)
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at)VALUES('t','c','r','web','in_flight','ok',?)").bind(&now).execute(&e.app.db).await.unwrap();
    let i = AdapterIdentity {
        session_id: "s".into(),
        bot_id: "b".into(),
        run_id: "r".into(),
        turn_id: "t".into(),
        generation: 1,
    };
    let cp = Checkpoint::new(&json!({"messages":[],"responses":[],"api_pending":false,"tool_pending":null,"completed_tool_ids":[]})).unwrap();
    api_bot::create(
        &e.app.db,
        &Fence {
            session: "s",
            bot: "b",
            run: "r",
            turn: "t",
            generation: 1,
            revision: 0,
        },
        &cp,
    )
    .await
    .unwrap();
    (e, i)
}
fn response(text: &str) -> Response {
    Response {
        id: "fixture".into(),
        model: "claude-opus-5-5".into(),
        content: vec![
            json!({"type":"text","text":text}),
            json!({"type":"thinking","thinking":"SECRET_THINKING","signature":"SECRET_SIGNATURE"}),
            json!({"type":"tool_use","input":{"command":"SECRET_TOOL"}}),
            json!({"type":"unknown","prompt":"SECRET_PROMPT","key":"SECRET_KEY"}),
        ],
        stop_reason: "end_turn".into(),
        usage: json!({}),
    }
}
#[tokio::test]
async fn receipt_visible_only_redacted_idempotent_and_conflict() {
    let (e, i) = fixture().await;
    let p = Payload::completed(&response("PRIVATE_VISIBLE_SENTINEL")).unwrap();
    let debug = format!("{p:?}");
    assert!(!debug.contains("PRIVATE_VISIBLE_SENTINEL"));
    assert!(debug.contains("redacted"));
    assert_eq!(save(&e.app.db, &i, 0, &p).await.unwrap(), Saved::Inserted);
    assert_eq!(save(&e.app.db, &i, 0, &p).await.unwrap(), Saved::Existing);
    assert_eq!(
        save(
            &e.app.db,
            &i,
            0,
            &Payload::completed(&response("different")).unwrap()
        )
        .await
        .unwrap_err(),
        Error::Conflict
    );
    let text: String = sqlx::query_scalar("SELECT final_text FROM api_bot_join_receipts")
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(text, "PRIVATE_VISIBLE_SENTINEL");
    for secret in [
        "SECRET_THINKING",
        "SECRET_SIGNATURE",
        "SECRET_TOOL",
        "SECRET_PROMPT",
        "SECRET_KEY",
    ] {
        assert!(!text.contains(secret));
        assert!(!debug.contains(secret));
    }
    assert!(!crate::api::state_json(&e.app)
        .await
        .unwrap()
        .to_string()
        .contains("PRIVATE_VISIBLE_SENTINEL"));
    assert_eq!(pending(&e.app.db, 1).await.unwrap().len(), 1);
}
#[tokio::test]
async fn receipt_bounds_and_terminal_payload_contract() {
    assert_eq!(
        Payload::completed(&response(&"é".repeat(MAX_TEXT_BYTES / 2 + 1))).unwrap_err(),
        Error::Bounds
    );
    assert!(Payload::completed(&response(&"x".repeat(MAX_TEXT_BYTES))).is_ok());
    assert_eq!(
        Payload::ended(JoinOutcome::Completed).unwrap_err(),
        Error::Invalid
    );
    let (e, i) = fixture().await;
    assert!(matches!(pending(&e.app.db, 0).await, Err(Error::Bounds)));
    assert!(matches!(
        pending(&e.app.db, MAX_BATCH + 1).await,
        Err(Error::Bounds)
    ));
    assert_eq!(
        save(
            &e.app.db,
            &i,
            0,
            &Payload::ended(JoinOutcome::Cancelled).unwrap()
        )
        .await
        .unwrap(),
        Saved::Inserted
    );
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>("SELECT final_text FROM api_bot_join_receipts")
            .fetch_one(&e.app.db)
            .await
            .unwrap(),
        None
    );
    assert!(
        sqlx::query("UPDATE api_bot_join_receipts SET final_text='invalid'")
            .execute(&e.app.db)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn receipt_owner_fences_reject_each_changed_dimension() {
    for sql in [
        "UPDATE api_bot_sessions SET generation=2",
        "UPDATE api_bot_sessions SET revision=1",
        "UPDATE api_bot_sessions SET retired_at='retired'",
        "UPDATE api_bot_sessions SET expires_at='2000'",
        "UPDATE api_bot_sessions SET owner_run_id=NULL",
        "UPDATE api_bot_sessions SET owner_turn_id=NULL",
        "UPDATE bots SET deleted_at='deleted'",
        "UPDATE bots SET kind='codex'",
        "UPDATE runs SET state='stopping'",
        "UPDATE turns SET status='completed'",
        "UPDATE turns SET delivery='pending'",
    ] {
        let (e, i) = fixture().await;
        sqlx::query(sql).execute(&e.app.db).await.unwrap();
        assert_eq!(
            save(
                &e.app.db,
                &i,
                0,
                &Payload::ended(JoinOutcome::Failed).unwrap()
            )
            .await
            .unwrap(),
            Saved::Fenced,
            "{sql}"
        );
    }
    let (e, i) = fixture().await;
    for field in 0..5 {
        let mut stale = i.clone();
        match field {
            0 => stale.session_id = "other".into(),
            1 => stale.bot_id = "other".into(),
            2 => stale.run_id = "other".into(),
            3 => stale.turn_id = "other".into(),
            _ => stale.generation = 2,
        };
        assert_eq!(
            save(
                &e.app.db,
                &stale,
                0,
                &Payload::ended(JoinOutcome::Failed).unwrap()
            )
            .await
            .unwrap(),
            Saved::Fenced
        );
    }
}
#[tokio::test]
async fn receipt_mark_cas_and_retention_preserve_ledger_and_owner() {
    let (e, i) = fixture().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at)VALUES('a','s','b','r','t',1,0,'hash','fast','unknown',123,'fixture',?)").bind(now).execute(&e.app.db).await.unwrap();
    save(
        &e.app.db,
        &i,
        0,
        &Payload::ended(JoinOutcome::Failed).unwrap(),
    )
    .await
    .unwrap();
    let mut items = pending(&e.app.db, MAX_BATCH).await.unwrap();
    let item = &mut items[0];
    item.revision = 1;
    let mut tx = crate::db::begin_write(&e.app.db).await.unwrap();
    assert!(!mark_on(&mut tx, item, Disposition::Applied).await.unwrap());
    item.revision = 0;
    assert!(mark_on(&mut tx, item, Disposition::Fenced).await.unwrap());
    assert!(!mark_on(&mut tx, item, Disposition::Applied).await.unwrap());
    tx.commit().await.unwrap();
    assert!(pending(&e.app.db, 1).await.unwrap().is_empty());
    sqlx::query("DELETE FROM api_bot_sessions WHERE id='s'")
        .execute(&e.app.db)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM api_bot_join_receipts")
            .fetch_one(&e.app.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT reserved_microusd FROM api_bot_attempts WHERE id='a'")
            .fetch_one(&e.app.db)
            .await
            .unwrap(),
        123
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM runs WHERE id='r'")
            .fetch_one(&e.app.db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn receipt_stop_after_save_cannot_overwrite_or_revalidate_old_receipt() {
    let (e, i) = fixture().await;
    let p = Payload::completed(&response("joined final")).unwrap();
    assert_eq!(save(&e.app.db, &i, 0, &p).await.unwrap(), Saved::Inserted);
    sqlx::query("UPDATE runs SET state='stopping' WHERE id='r'")
        .execute(&e.app.db)
        .await
        .unwrap();
    assert_eq!(save(&e.app.db, &i, 0, &p).await.unwrap(), Saved::Fenced);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT final_text FROM api_bot_join_receipts")
            .fetch_one(&e.app.db)
            .await
            .unwrap(),
        "joined final"
    );
}
