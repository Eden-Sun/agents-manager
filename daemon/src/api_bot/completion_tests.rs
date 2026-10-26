use super::{
    completion::{self, FinalText, Outcome},
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

#[tokio::test]
async fn completion_storage_commits_text_before_turn_events() {
    let f = fixture(true).await;
    let response = response();
    let projected = runner::final_text(&response);
    let mut events = f.env.app.subscribe();

    let committed = match completion::commit_after_join(
        &f.env.app,
        &f.key,
        &f.identity,
        &FinalText::from_response(&response),
    )
    .await
    .unwrap()
    {
        Outcome::Committed(committed) => committed,
        Outcome::Fenced => panic!("exact fixture owner must commit"),
    };
    completion::emit_after_commit(&f.env.app, &f.key, committed).await;

    let turn_status: String =
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
    assert_eq!(turn_status, "completed");
    assert_eq!(
        messages(&f.env.app, &f.key.turn_id).await,
        vec![("assistant".into(), projected, "transcript".into())]
    );
    let attempt: (String, Option<i64>, i64) =
        sqlx::query_as("SELECT state,estimated_microusd,reserved_microusd FROM api_bot_attempts WHERE id='completion-reservation'")
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
    assert_eq!(attempt, ("unknown".into(), None, 140));

    let mut event_kinds = Vec::new();
    while let Ok(event) = events.try_recv() {
        event_kinds.push(event.kind);
    }
    let message_added = event_kinds
        .iter()
        .position(|kind| kind == "message_added")
        .unwrap();
    let turn_updated = event_kinds
        .iter()
        .position(|kind| kind == "turn_updated")
        .unwrap();
    assert!(message_added < turn_updated, "{event_kinds:?}");

    let before_duplicate = f.env.app.current_seq();
    assert!(matches!(
        completion::commit_after_join(
            &f.env.app,
            &f.key,
            &f.identity,
            &FinalText::from_response(&response)
        )
        .await
        .unwrap(),
        Outcome::Fenced
    ));
    assert_eq!(f.env.app.current_seq(), before_duplicate);
    assert_eq!(messages(&f.env.app, &f.key.turn_id).await.len(), 1);
}

#[tokio::test]
async fn exact_key_mismatches_fence_without_closeout_message_or_event() {
    let f = fixture(true).await;
    let before_seq = f.env.app.current_seq();
    for wrong in [
        key_with("wrong-bot", &f.key.run_id, &f.key.turn_id),
        key_with(&f.key.bot_id, "wrong-run", &f.key.turn_id),
        key_with(&f.key.bot_id, &f.key.run_id, "wrong-turn"),
    ] {
        assert!(matches!(
            completion::commit_after_join(
                &f.env.app,
                &wrong,
                &f.identity,
                &FinalText::from_response(&response())
            )
            .await
            .unwrap(),
            Outcome::Fenced
        ));
    }
    let status: String =
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
    let attempt_state: String =
        sqlx::query_scalar("SELECT state FROM api_bot_attempts WHERE id='completion-reservation'")
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
    assert_eq!(status, "in_flight");
    assert_eq!(attempt_state, "reserved");
    assert!(messages(&f.env.app, &f.key.turn_id).await.is_empty());
    assert_eq!(f.env.app.current_seq(), before_seq);
}

#[tokio::test]
async fn stopping_run_fences_completion_before_cost_or_assistant_write() {
    let f = fixture(true).await;
    sqlx::query("UPDATE runs SET state='stopping' WHERE id=?")
        .bind(&f.key.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let before_seq = f.env.app.current_seq();

    assert!(matches!(
        completion::commit_after_join(
            &f.env.app,
            &f.key,
            &f.identity,
            &FinalText::from_response(&response())
        )
        .await
        .unwrap(),
        Outcome::Fenced
    ));
    let attempt_state: String =
        sqlx::query_scalar("SELECT state FROM api_bot_attempts WHERE id='completion-reservation'")
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
    assert_eq!(attempt_state, "reserved");
    assert!(messages(&f.env.app, &f.key.turn_id).await.is_empty());
    assert_eq!(f.env.app.current_seq(), before_seq);
}

#[tokio::test]
async fn no_reservation_is_valid_when_exact_live_owner_and_turn_cas_succeed() {
    let f = fixture(false).await;
    assert!(matches!(
        completion::commit_after_join(
            &f.env.app,
            &f.key,
            &f.identity,
            &FinalText::from_response(&response())
        )
        .await
        .unwrap(),
        Outcome::Committed(_)
    ));
    assert_eq!(messages(&f.env.app, &f.key.turn_id).await.len(), 1);
}

#[tokio::test]
async fn completion_closes_reserved_attempts_without_changing_recorded_attempts() {
    let f = fixture(true).await;
    let recorded_checkpoint = checkpoint(false);
    let recorded_usage = json!({"input_tokens":3,"output_tokens":2,"speed":"fast"});
    let recorded_receipt = api_bot::Receipt {
        state: "recorded",
        actual_speed: Some("fast"),
        usage: Some(&recorded_usage),
        estimated_microusd: Some(42),
    };
    let first_fence = Fence {
        session: &f.identity.session_id,
        bot: &f.key.bot_id,
        run: &f.key.run_id,
        turn: &f.key.turn_id,
        generation: f.identity.generation,
        revision: 1,
    };
    assert_eq!(
        api_bot::record(
            &f.env.app.db,
            &first_fence,
            "completion-reservation",
            &recorded_receipt,
            &recorded_checkpoint,
        )
        .await
        .unwrap(),
        2
    );
    let second_fence = Fence { revision: 2, ..first_fence };
    assert_eq!(
        api_bot::reserve(
            &f.env.app.db,
            &second_fence,
            &Attempt {
                id: "completion-second-reservation",
                ordinal: 1,
                request_hash: "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                speed: "standard",
                reserved_microusd: 90,
                pricing_version: "fixture-v1",
            },
            &checkpoint(true),
            1000,
            10,
        )
        .await
        .unwrap(),
        api_bot::Reservation::Fresh(3)
    );

    let recorded_before: String = sqlx::query_scalar(
        "SELECT json_object('id',id,'session_id',session_id,'bot_id',bot_id,'run_id',run_id,'turn_id',turn_id,'generation',generation,'ordinal',ordinal,'request_hash',request_hash,'requested_speed',requested_speed,'actual_speed',actual_speed,'state',state,'usage_json',usage_json,'reserved_microusd',reserved_microusd,'estimated_microusd',estimated_microusd,'pricing_version',pricing_version,'created_at',created_at,'finished_at',finished_at) FROM api_bot_attempts WHERE id='completion-reservation'",
    )
    .fetch_one(&f.env.app.db)
    .await
    .unwrap();
    assert!(matches!(
        completion::commit_after_join(
            &f.env.app,
            &f.key,
            &f.identity,
            &FinalText::from_response(&response())
        )
        .await
        .unwrap(),
        Outcome::Committed(_)
    ));
    let recorded_after: String = sqlx::query_scalar(
        "SELECT json_object('id',id,'session_id',session_id,'bot_id',bot_id,'run_id',run_id,'turn_id',turn_id,'generation',generation,'ordinal',ordinal,'request_hash',request_hash,'requested_speed',requested_speed,'actual_speed',actual_speed,'state',state,'usage_json',usage_json,'reserved_microusd',reserved_microusd,'estimated_microusd',estimated_microusd,'pricing_version',pricing_version,'created_at',created_at,'finished_at',finished_at) FROM api_bot_attempts WHERE id='completion-reservation'",
    )
    .fetch_one(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(recorded_after, recorded_before);
    let reserved_state: (String, Option<i64>, i64) = sqlx::query_as(
        "SELECT state,estimated_microusd,reserved_microusd FROM api_bot_attempts WHERE id='completion-second-reservation'",
    )
    .fetch_one(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(reserved_state, ("unknown".into(), None, 90));
}

#[tokio::test]
async fn detached_attempt_cannot_claim_owner_or_fallback_to_another_key() {
    let f = fixture(true).await;
    sqlx::query(
        "UPDATE api_bot_attempts SET session_id=NULL,bot_id=NULL,run_id=NULL,turn_id=NULL WHERE id='completion-reservation'",
    )
    .execute(&f.env.app.db)
    .await
    .unwrap();

    assert!(matches!(
        completion::commit_after_join(
            &f.env.app,
            &f.key,
            &f.identity,
            &FinalText::from_response(&response())
        )
        .await
        .unwrap(),
        Outcome::Committed(_)
    ));
    type DetachedAttempt = (Option<String>, Option<String>, Option<String>, Option<String>, String, Option<i64>, i64);
    let detached: DetachedAttempt = sqlx::query_as(
        "SELECT session_id,bot_id,run_id,turn_id,state,estimated_microusd,reserved_microusd FROM api_bot_attempts WHERE id='completion-reservation'",
    )
    .fetch_one(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(detached, (None, None, None, None, "reserved".into(), None, 140));
    assert_eq!(messages(&f.env.app, &f.key.turn_id).await.len(), 1);
}

#[tokio::test]
async fn assistant_insert_error_rolls_back_cost_and_turn_and_emits_nothing() {
    let f = fixture(true).await;
    sqlx::query(&format!(
        "CREATE TRIGGER completion_insert_failure BEFORE INSERT ON messages WHEN NEW.turn_id='{}' BEGIN SELECT RAISE(ABORT, 'injected completion insert failure'); END",
        f.key.turn_id
    ))
    .execute(&f.env.app.db)
    .await
    .unwrap();
    let before_seq = f.env.app.current_seq();

    assert!(completion::commit_after_join(
        &f.env.app,
        &f.key,
        &f.identity,
        &FinalText::from_response(&response())
    )
    .await
    .is_err());
    let status: String =
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&f.key.turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
    let attempt: (String, Option<i64>, Option<String>) =
        sqlx::query_as("SELECT state,estimated_microusd,finished_at FROM api_bot_attempts WHERE id='completion-reservation'")
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
    assert_eq!(status, "in_flight");
    assert_eq!(attempt, ("reserved".into(), None, None));
    assert!(messages(&f.env.app, &f.key.turn_id).await.is_empty());
    assert_eq!(f.env.app.current_seq(), before_seq);
}

#[tokio::test]
async fn joined_failure_settles_reserved_and_zero_cost_rows_do_not_block_owner_cas() {
    let f = fixture(true).await;
    let committed = match completion::fail_after_join(&f.env.app, &f.key, &f.identity)
        .await
        .unwrap()
    {
        Outcome::Committed(committed) => committed,
        Outcome::Fenced => panic!("exact live owner must settle failed turn"),
    };
    assert!(committed.message.is_none());
    completion::emit_after_commit(&f.env.app, &f.key, committed).await;
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
        .bind(&f.key.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    let attempt: (String, Option<i64>, i64) = sqlx::query_as(
        "SELECT state,estimated_microusd,reserved_microusd FROM api_bot_attempts WHERE id='completion-reservation'",
    )
    .fetch_one(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(attempt, ("unknown".into(), None, 140));
    assert!(messages(&f.env.app, &f.key.turn_id).await.is_empty());

    let no_cost = fixture(false).await;
    assert!(matches!(
        completion::fail_after_join(&no_cost.env.app, &no_cost.key, &no_cost.identity)
            .await
            .unwrap(),
        Outcome::Committed(_)
    ));
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
        .bind(&no_cost.key.turn_id)
        .fetch_one(&no_cost.env.app.db)
        .await
        .unwrap();
    assert_eq!(status, "failed");
    assert!(messages(&no_cost.env.app, &no_cost.key.turn_id).await.is_empty());
}

#[tokio::test]
async fn failed_turn_cas_error_rolls_back_reserved_closeout() {
    let f = fixture(true).await;
    sqlx::query(&format!(
        "CREATE TRIGGER completion_fail_cas_error BEFORE UPDATE OF status ON turns WHEN NEW.id='{}' AND NEW.status='failed' BEGIN SELECT RAISE(ABORT, 'injected failed CAS error'); END",
        f.key.turn_id
    ))
    .execute(&f.env.app.db)
    .await
    .unwrap();
    let before_seq = f.env.app.current_seq();

    assert!(completion::fail_after_join(&f.env.app, &f.key, &f.identity)
        .await
        .is_err());
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
        .bind(&f.key.turn_id)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
    let attempt: (String, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT state,estimated_microusd,finished_at FROM api_bot_attempts WHERE id='completion-reservation'",
    )
    .fetch_one(&f.env.app.db)
    .await
    .unwrap();
    assert_eq!(status, "in_flight");
    assert_eq!(attempt, ("reserved".into(), None, None));
    assert!(messages(&f.env.app, &f.key.turn_id).await.is_empty());
    assert_eq!(f.env.app.current_seq(), before_seq);
}

#[tokio::test]
async fn ambiguous_commit_retry_requires_exact_inflight_turn_without_assistant() {
    let f = fixture(true).await;
    assert!(completion::exact_turn_is_uncommitted(&f.env.app, &f.key).await);
    assert!(!completion::exact_turn_is_uncommitted(
        &f.env.app,
        &key_with("wrong-bot", &f.key.run_id, &f.key.turn_id)
    )
    .await);

    let mut tx = crate::db::begin_write(&f.env.app.db).await.unwrap();
    crate::lifecycle::insert_message_tx(
        &mut tx,
        &f.conversation_id,
        Some(&f.key.turn_id),
        "assistant",
        "already committed",
        "transcript",
        false,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(!completion::exact_turn_is_uncommitted(&f.env.app, &f.key).await);

    sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
        .bind(crate::db::now())
        .bind(&f.key.turn_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    assert!(!completion::exact_turn_is_uncommitted(&f.env.app, &f.key).await);
}

