use super::*;

#[tokio::test]
async fn an_unreadable_bot_host_is_a_retryable_error_and_writes_no_pane_row() {
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let bot = crate::testing::claude_bot(&app, &e.project_id, "alfa").await;
    let report = || {
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Token", "tok".parse().unwrap());
        let body = RelayPane { bot_id: bot.id.clone(), pane_id: "w1-9".into(), purpose: "build".into() };
        relay_pane(State(app.clone()), h, axum::extract::Form(body))
    };
    let panes = || async { sqlx::query_scalar::<_, i64>("SELECT count(*) FROM panes").fetch_one(&app.db).await.unwrap() };

    crate::testing::make_table_unreadable(&app, "projects").await;
    let (code, _) = report().await;
    crate::testing::make_table_readable(&app, "projects").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(panes().await, 0, "不能寫出任何 pane 列");

    let (code, _) = report().await;
    assert_eq!(code, StatusCode::OK, "DB 好了重送就成功");
    let host: String = sqlx::query_scalar("SELECT host FROM panes WHERE pane_id='w1-9'").fetch_one(&app.db).await.unwrap();
    assert_eq!(host, "local");
}
