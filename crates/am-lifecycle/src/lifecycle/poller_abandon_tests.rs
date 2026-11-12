use super::*;
use crate::testing as tt;

async fn completed_turn(status: &str) -> (tt::Env, String, String) {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'abandon-test','claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web',?,'unknown',?,?)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(status)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    insert_message(&app, &conversation_id, Some(&turn_id), "assistant", "already complete", "hook", false, None)
        .await
        .unwrap();
    (env, turn_id, conversation_id)
}

#[tokio::test]
async fn abandon_of_a_completed_unknown_delivery_turn_is_a_noop_conflict() {
    for status in ["completed", "completed_fallback"] {
        let (env, turn_id, conversation_id) = completed_turn(status).await;
        let app = env.app.clone();
        let before = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        let before_messages = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT id, role, content, source FROM messages WHERE conversation_id=? ORDER BY id",
        )
        .bind(&conversation_id)
        .fetch_all(&app.db)
        .await
        .unwrap();

        let err = abandon_turn(&app, &turn_id).await.expect_err("completed turns cannot be abandoned");
        match err {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "turn is neither in-flight nor of unknown delivery");
                assert_eq!(body["turn_id"], turn_id);
            }
            other => panic!("expected conflict, got {other:?}"),
        }

        let after = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(after.status, before.status);
        assert_eq!(after.delivery, before.delivery);
        assert_eq!(after.completed_at, before.completed_at);
        let after_messages = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT id, role, content, source FROM messages WHERE conversation_id=? ORDER BY id",
        )
        .bind(&conversation_id)
        .fetch_all(&app.db)
        .await
        .unwrap();
        assert_eq!(after_messages, before_messages);
    }
}

#[tokio::test]
async fn a_completed_unknown_delivery_turn_does_not_block_a_new_prompt() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'prompt-unknown','codex','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,?,'web','completed','unknown',?,?)",
    )
    .bind(db::ulid())
    .bind(&conversation_id)
    .bind(&run_id)
    .bind(db::now())
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    env.herdr.set_agent("agent", "pane-1", true);
    let out = prompt_with_attachments_and_relay(&app, &bot_id, "next", "request-1", &[], None)
        .await
        .expect("a completed unknown-delivery row must not block the next prompt");
    assert_eq!(out.delivery, "unknown", "the mock RPC was reached and failed delivery, rather than the stale row blocking it");
}
