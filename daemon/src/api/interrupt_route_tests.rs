//! The real interrupt handler must preserve the retryable response and avoid Esc when its
//! arrival snapshot cannot be read.
use super::*;

#[tokio::test]
async fn an_unreadable_arrival_snapshot_is_a_retryable_503_without_esc() {
    let e = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "interrupt-route").await;
    crate::testing::fake_run(&e.app, &bot.id).await;
    e.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);

    crate::testing::make_table_unreadable(&e.app, "runs").await;
    let restore_app = e.app.clone();
    let restore_bot = bot.id.clone();
    crate::lifecycle::race_point::arm("interrupt_after_arrival", &bot.id, move || async move {
        crate::testing::make_table_readable(&restore_app, "runs").await;
    });
    let result = interrupt_bot(State(e.app.clone()), Path(bot.id.clone()), None).await;
    // In the fixed path the handler returns before the race point; consume it here to restore
    // the test DB. The previous fail-open path consumes it before sending Esc.
    crate::lifecycle::race_point::hit("interrupt_after_arrival", &restore_bot).await;
    let response = match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"], "interrupt_state_unavailable", "{body}");
    assert_eq!((body["retryable"].as_bool(), body["sent"].as_bool()), (Some(true), Some(false)), "{body}");
    assert!(e.herdr.calls_to("agent.send_keys").is_empty(), "a failed arrival read must not send Esc");
}
