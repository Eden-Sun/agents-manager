use super::{
    completion::{self, Outcome},
    protocol::Response,
    runner,
};
use super::adapter::AdapterIdentity;
use crate::lifecycle::api_bot_port::ApiTurnKey;
use am_base::db::api_bot::{self, Attempt, Checkpoint, Fence};
use serde_json::json;

fn response() -> Response {
    Response {
        id: "completion-fixture".into(),
        model: "claude-opus-5-5".into(),
        content: vec![
            json!({"type":"text","text":"final"}),
            json!({"type":"thinking","thinking":"private reasoning","signature":"private signature"}),
            json!({"type":"text","text":" answer"}),
            json!({"type":"tool_use","id":"private-tool","input":{"command":"private payload"}}),
            json!({"type":"unknown","payload":"private extension"}),
        ],
        stop_reason: "end_turn".into(),
        usage: json!({"input_tokens":10,"output_tokens":4}),
    }
}

fn checkpoint(pending: bool) -> Checkpoint {
    Checkpoint::new(&json!({
        "messages": [{"role":"user","content":"fixture"}],
        "responses": [],
        "api_pending": pending,
        "tool_pending": null,
        "completed_tool_ids": []
    }))
    .unwrap()
}

struct Fixture {
    env: crate::testing::Env,
    key: ApiTurnKey,
    identity: AdapterIdentity,
    conversation_id: String,
}

async fn fixture(with_reservation: bool) -> Fixture {
    let env = crate::testing::env().await;
    let bot_id = crate::db::ulid();
    let run_id = crate::db::ulid();
    let turn_id = crate::db::ulid();
    let conversation_id = crate::db::ulid();
    let session_id = crate::db::ulid();
    let now = crate::db::now();

    sqlx::query("INSERT INTO bots(id,project_id,name,kind,effort,fast,hook_token,created_at) VALUES(?,?,?,'claude-api','high',0,'fixture',?)")
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(format!("completion-{bot_id}"))
        .bind(&now)
        .execute(&env.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES(?,?,?)")
        .bind(&conversation_id)
        .bind(&bot_id)
        .bind(&now)
        .execute(&env.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
        .bind(&run_id)
        .bind(&bot_id)
        .bind(&now)
        .execute(&env.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES(?,?,?,'web','in_flight','ok','fixture',?)")
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(&now)
        .execute(&env.app.db)
        .await
        .unwrap();

    let fence = Fence {
        session: &session_id,
        bot: &bot_id,
        run: &run_id,
        turn: &turn_id,
        generation: 1,
        revision: 0,
    };
    api_bot::create(&env.app.db, &fence, &checkpoint(false))
        .await
        .unwrap();
    if with_reservation {
        api_bot::reserve(
            &env.app.db,
            &fence,
            &Attempt {
                id: "completion-reservation",
                ordinal: 0,
                request_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                speed: "fast",
                reserved_microusd: 140,
                pricing_version: "fixture-v1",
            },
            &checkpoint(true),
            1000,
            10,
        )
        .await
        .unwrap();
    }

    Fixture {
        env,
        conversation_id: conversation_id.clone(),
        key: ApiTurnKey {
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        },
        identity: AdapterIdentity {
            session_id,
            bot_id: bot_id.clone(),
            run_id: run_id.clone(),
            turn_id,
            generation: 1,
        },
    }
}

fn key_with(bot_id: &str, run_id: &str, turn_id: &str) -> ApiTurnKey {
    ApiTurnKey {
        bot_id: bot_id.into(),
        run_id: run_id.into(),
        turn_id: turn_id.into(),
    }
}

async fn messages(app: &crate::state::App, turn_id: &str) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT role,content,source FROM messages WHERE turn_id=? ORDER BY rowid",
    )
    .bind(turn_id)
    .fetch_all(&app.db)
    .await
    .unwrap()
}

#[test]
fn final_text_projection_excludes_non_text_provider_blocks() {
    assert_eq!(runner::final_text(&response()), "final answer");
}

async fn durable_item(f: &Fixture) -> super::receipt::Pending {
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM api_bot_sessions WHERE id=?")
        .bind(&f.identity.session_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    let payload = super::receipt::Payload::visible(&super::runner::final_text(&response())).unwrap();
    super::receipt::save(&f.env.app.db, &f.identity, revision, &payload)
        .await
        .unwrap();
    super::receipt::load(&f.env.app.db, &f.identity)
        .await
        .unwrap()
        .unwrap()
        .item
}

#[tokio::test]
async fn durable_receipt_and_completion_rollback_together_then_apply_once() {
    let f = fixture(true).await;
    let item = durable_item(&f).await;
    sqlx::query("CREATE TRIGGER reject_receipt_apply BEFORE UPDATE OF state ON api_bot_join_receipts WHEN NEW.state='applied' BEGIN SELECT RAISE(ABORT,'synthetic receipt fault'); END")
        .execute(&f.env.app.db).await.unwrap();
    assert!(matches!(
        completion::consume_receipt(&f.env.app, &f.key, &item).await,
        Err(completion::StoreError::Retryable)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
        "in_flight"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM api_bot_attempts WHERE id='completion-reservation'"
        )
        .fetch_one(&f.env.app.db)
        .await
        .unwrap(),
        "reserved"
    );
    assert!(messages(&f.env.app, &f.key.turn_id).await.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM api_bot_join_receipts")
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
        "pending"
    );
    sqlx::query("DROP TRIGGER reject_receipt_apply")
        .execute(&f.env.app.db)
        .await
        .unwrap();
    assert!(matches!(
        completion::consume_receipt(&f.env.app, &f.key, &item)
            .await
            .unwrap(),
        Outcome::Committed(_)
    ));
    assert!(matches!(
        completion::probe_after_join(&f.env.app, &f.identity, &item.payload)
            .await
            .unwrap(),
        completion::Probe::Applied(_)
    ));
    assert!(matches!(
        completion::store_durable_after_join(&f.env.app, &f.key, &f.identity, &item.payload)
            .await
            .unwrap(),
        Outcome::Committed(_)
    ));
    assert_eq!(messages(&f.env.app, &f.key.turn_id).await.len(), 1);
    let cost:(String,Option<i64>,i64)=sqlx::query_as("SELECT state,estimated_microusd,reserved_microusd FROM api_bot_attempts WHERE id='completion-reservation'").fetch_one(&f.env.app.db).await.unwrap();
    assert_eq!(cost, ("unknown".into(), None, 140));
}

#[tokio::test]
async fn durable_receipt_stop_expiry_and_owner_change_only_fence_old_receipt() {
    for mutation in [
        "UPDATE runs SET state='stopping'",
        "UPDATE api_bot_sessions SET revision=revision+1",
        "UPDATE api_bot_sessions SET generation=generation+1",
        "UPDATE api_bot_sessions SET expires_at='2000-01-01T00:00:00Z'",
        "UPDATE api_bot_sessions SET owner_turn_id=NULL",
    ] {
        let f = fixture(true).await;
        let item = durable_item(&f).await;
        sqlx::query(mutation).execute(&f.env.app.db).await.unwrap();
        let before:(String,String,String,i64)=sqlx::query_as("SELECT r.state,t.status,a.state,a.reserved_microusd FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_attempts a ON a.turn_id=t.id WHERE t.id=?").bind(&f.key.turn_id).fetch_one(&f.env.app.db).await.unwrap();
        assert!(
            matches!(
                completion::consume_receipt(&f.env.app, &f.key, &item)
                    .await
                    .unwrap(),
                Outcome::Fenced
            ),
            "{mutation}"
        );
        let after:(String,String,String,i64)=sqlx::query_as("SELECT r.state,t.status,a.state,a.reserved_microusd FROM runs r JOIN turns t ON t.run_id=r.id JOIN api_bot_attempts a ON a.turn_id=t.id WHERE t.id=?").bind(&f.key.turn_id).fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(after, before, "{mutation}");
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM api_bot_join_receipts")
                .fetch_one(&f.env.app.db)
                .await
                .unwrap(),
            "fenced"
        );
        assert!(messages(&f.env.app, &f.key.turn_id).await.is_empty());
    }
}

#[tokio::test]
async fn durable_receipt_mismatched_identity_revision_outcome_or_text_cannot_settle() {
    for field in 0..6 {
        let f = fixture(true).await;
        let mut item = durable_item(&f).await;
        match field {
            0 => item.identity.bot_id = "other".into(),
            1 => item.identity.run_id = "other".into(),
            2 => item.identity.turn_id = "other".into(),
            3 => item.revision += 1,
            4 => {
                item.payload =
                    super::receipt::Payload::ended(super::receipt::JoinOutcome::Failed).unwrap()
            }
            _ => item.payload = super::receipt::Payload::visible("wrong visible text").unwrap(),
        };
        assert!(!matches!(
            completion::consume_receipt(&f.env.app, &f.key, &item).await,
            Ok(Outcome::Committed(_))
        ));
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
                .bind(&f.key.turn_id)
                .fetch_one(&f.env.app.db)
                .await
                .unwrap(),
            "in_flight"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT state FROM api_bot_attempts WHERE id='completion-reservation'"
            )
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
            "reserved"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM api_bot_join_receipts")
                .fetch_one(&f.env.app.db)
                .await
                .unwrap(),
            "pending"
        );
        assert!(messages(&f.env.app, &f.key.turn_id).await.is_empty());
    }
}

#[tokio::test]
async fn applied_receipt_proof_requires_exact_visible_message_and_live_owner() {
    let f = fixture(true).await;
    let item = durable_item(&f).await;
    completion::consume_receipt(&f.env.app, &f.key, &item)
        .await
        .unwrap();
    assert!(matches!(
        completion::probe_after_join(&f.env.app, &f.identity, &item.payload)
            .await
            .unwrap(),
        completion::Probe::Applied(_)
    ));
    sqlx::query("UPDATE messages SET content='mismatched' WHERE turn_id=?")
        .bind(&f.key.turn_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    assert!(matches!(
        completion::probe_after_join(&f.env.app, &f.identity, &item.payload)
            .await
            .unwrap(),
        completion::Probe::Unconfirmed
    ));
    sqlx::query("UPDATE messages SET content='final answer' WHERE turn_id=?")
        .bind(&f.key.turn_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET state='stopping' WHERE id=?")
        .bind(&f.key.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    assert!(matches!(
        completion::probe_after_join(&f.env.app, &f.identity, &item.payload)
            .await
            .unwrap(),
        completion::Probe::Unconfirmed
    ));
    assert_eq!(messages(&f.env.app, &f.key.turn_id).await.len(), 1);
}

#[tokio::test]
async fn ambiguous_read_error_keeps_durable_pending_and_reserved_cost() {
    let f = fixture(true).await;
    let item = durable_item(&f).await;
    // A read failure must not authorize another write or infer zero charge.
    sqlx::query("ALTER TABLE api_bot_join_receipts RENAME TO unavailable_receipts")
        .execute(&f.env.app.db)
        .await
        .unwrap();
    assert!(
        completion::probe_after_join(&f.env.app, &f.identity, &item.payload)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM unavailable_receipts")
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
        "pending"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM api_bot_attempts WHERE id='completion-reservation'"
        )
        .fetch_one(&f.env.app.db)
        .await
        .unwrap(),
        "reserved"
    );
    assert!(messages(&f.env.app, &f.key.turn_id).await.is_empty());
}

#[tokio::test]
async fn pending_receipt_with_preexisting_assistant_cannot_insert_a_second_message() {
    let f = fixture(true).await;
    let item = durable_item(&f).await;
    sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES('preexisting',?,?,'assistant','existing reply','transcript',?)")
        .bind(&f.conversation_id).bind(&f.key.turn_id).bind(crate::db::now()).execute(&f.env.app.db).await.unwrap();
    assert!(matches!(
        completion::consume_receipt(&f.env.app, &f.key, &item).await,
        Err(completion::StoreError::Ambiguous)
    ));
    assert!(matches!(
        completion::probe_after_join(&f.env.app, &f.identity, &item.payload)
            .await
            .unwrap(),
        completion::Probe::Unconfirmed
    ));
    assert_eq!(messages(&f.env.app, &f.key.turn_id).await.len(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
        "in_flight"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT state FROM api_bot_attempts WHERE id='completion-reservation'"
        )
        .fetch_one(&f.env.app.db)
        .await
        .unwrap(),
        "reserved"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM api_bot_join_receipts")
            .fetch_one(&f.env.app.db)
            .await
            .unwrap(),
        "pending"
    );
}
