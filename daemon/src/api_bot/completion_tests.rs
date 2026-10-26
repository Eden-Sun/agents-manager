use super::{
    completion::{self, FinalText, Outcome},
    protocol::Response,
    registry::{ApiBotRegistry, RunTurnKey, TurnOutcome},
    runner,
};
use crate::lifecycle::api_bot_port::ApiTurnKey;
use am_base::db::api_bot::{self, Attempt, Checkpoint, Fence};
use serde_json::json;
use std::sync::Arc;

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
        key: ApiTurnKey {
            bot_id,
            run_id,
            turn_id,
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

async fn prove_fake_registry_join(key: &ApiTurnKey) {
    let registry = Arc::new(ApiBotRegistry::new());
    let ticket = registry
        .spawn_tracked(
            RunTurnKey {
                run_id: key.run_id.clone(),
                turn_id: key.turn_id.clone(),
            },
            |_cancel| async { Ok(TurnOutcome::Completed) },
        )
        .unwrap();
    assert!(matches!(
        ticket.join().await,
        Ok(TurnOutcome::Completed)
    ));
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
async fn real_fake_registry_join_then_completion_commits_text_before_turn_events() {
    let f = fixture(true).await;
    prove_fake_registry_join(&f.key).await;
    let response = response();
    let projected = runner::final_text(&response);
    let mut events = f.env.app.subscribe();

    assert!(matches!(
        completion::commit_after_join(
            &f.env.app,
            &f.key,
            FinalText::from_response(&response)
        )
        .await
        .unwrap(),
        Outcome::Committed
    ));

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
            FinalText::from_response(&response)
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
        prove_fake_registry_join(&wrong).await;
        assert!(matches!(
            completion::commit_after_join(
                &f.env.app,
                &wrong,
                FinalText::from_response(&response())
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
    prove_fake_registry_join(&f.key).await;
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
            FinalText::from_response(&response())
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
    prove_fake_registry_join(&f.key).await;
    assert!(matches!(
        completion::commit_after_join(
            &f.env.app,
            &f.key,
            FinalText::from_response(&response())
        )
        .await
        .unwrap(),
        Outcome::Committed
    ));
    assert_eq!(messages(&f.env.app, &f.key.turn_id).await.len(), 1);
}

#[tokio::test]
async fn assistant_insert_error_rolls_back_cost_and_turn_and_emits_nothing() {
    let f = fixture(true).await;
    prove_fake_registry_join(&f.key).await;
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
        FinalText::from_response(&response())
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

