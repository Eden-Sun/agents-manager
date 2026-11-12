use super::*;
use crate::testing as tt;

#[tokio::test]
async fn withdrawing_an_awaits_idle_turn_returns_its_text_and_attachment_ids() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "withdraw-awaits-idle").await;
    let conversation_id = db::conversation_id(&app.db, &bot.id).await.unwrap();

    let turn_id = db::ulid();
    let message_id = db::ulid();
    let attachment_id = db::ulid();
    let text = "撤回時要放回這段文字";
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, awaits_idle)
             VALUES (?,?,'web','queued','pending',?,?,1)",
    )
    .bind(&turn_id)
    .bind(&conversation_id)
    .bind(text)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, attachments_json, created_at)
             VALUES (?,?,?,'user',?,'web',?,?)",
    )
    .bind(&message_id)
    .bind(&conversation_id)
    .bind(&turn_id)
    .bind(text)
    .bind(
        json!([{ "id": attachment_id, "name": "proof.png", "mime": "image/png", "size": 3, "path": "/tmp/proof.png" }])
            .to_string(),
    )
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, message_id, created_at)
             VALUES (?,?,'proof.png','image/png',3,'/tmp/proof.png','/tmp/proof.png','local',?,?)",
    )
    .bind(&attachment_id)
    .bind(&bot.id)
    .bind(&message_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();

    let response = withdraw_turn(State(app.clone()), Path(turn_id.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({ "text": text, "attachments": [attachment_id] }));
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(status, "failed");

    let claimed_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, awaits_idle)
             VALUES (?,?,'web','in_flight','pending',?,?,1)",
    )
    .bind(&claimed_id)
    .bind(&conversation_id)
    .bind(text)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let response = withdraw_turn(State(app.clone()), Path(claimed_id.clone()))
        .await
        .unwrap_err()
        .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
        .bind(&claimed_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(status, "in_flight");
}
