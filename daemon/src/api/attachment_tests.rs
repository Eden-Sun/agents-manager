use super::*;
use axum::http::HeaderValue;

fn image_headers() -> HeaderMap {
    HeaderMap::from_iter([(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))])
}

async fn status(result: Result<Response, LcError>) -> StatusCode {
    match result {
        Ok(response) => response.status(),
        Err(error) => error.into_response().status(),
    }
}

#[tokio::test]
async fn upload_rejects_empty_oversized_unknown_and_deleted_bots() {
    let e = crate::testing::env().await;
    let query = Query(HashMap::new());

    assert_eq!(
        status(upload_attachment(
            State(e.app.clone()),
            Path("missing".into()),
            query.clone(),
            image_headers(),
            Bytes::new(),
        )
        .await)
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status(upload_attachment(
            State(e.app.clone()),
            Path("missing".into()),
            query.clone(),
            image_headers(),
            Bytes::from(vec![0; crate::attach::MAX_BYTES + 1]),
        )
        .await)
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status(upload_attachment(
            State(e.app.clone()),
            Path("missing".into()),
            query.clone(),
            image_headers(),
            Bytes::from_static(b"png"),
        )
        .await)
        .await,
        StatusCode::NOT_FOUND
    );

    let deleted_id = crate::db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, hook_token, deleted_at, created_at)
             VALUES (?, ?, ?, 'claude', ?, ?, ?)",
    )
    .bind(&deleted_id)
    .bind(&e.project_id)
    .bind("deleted-bot")
    .bind("test-token")
    .bind(crate::db::now())
    .bind(crate::db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    assert_eq!(
        status(upload_attachment(
            State(e.app.clone()),
            Path(deleted_id),
            query,
            image_headers(),
            Bytes::from_static(b"png"),
        )
        .await)
        .await,
        StatusCode::NOT_FOUND
    );
}

/// 2026-09-14 使用者：暫存區要收任意檔。上傳端不再看 mime，沒帶 Content-Type 也要收。
#[tokio::test]
async fn upload_takes_any_file_not_just_images() {
    let e = crate::testing::env().await;
    let bot_id = crate::testing::claude_bot(&e.app, &e.project_id, "attach-bot").await.id;
    let query = Query(HashMap::from([("name".to_string(), "run.log".to_string())]));
    let headers = HeaderMap::from_iter([(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))]);
    assert_eq!(
        status(upload_attachment(State(e.app.clone()), Path(bot_id.clone()), query, headers, Bytes::from_static(b"boom\n")).await).await,
        StatusCode::OK
    );
    // 完全沒有 Content-Type 的上傳（有些瀏覽器貼上就是這樣）也不能被擋。
    let query = Query(HashMap::from([("name".to_string(), "notes.md".to_string())]));
    assert_eq!(
        status(upload_attachment(State(e.app.clone()), Path(bot_id), query, HeaderMap::new(), Bytes::from_static(b"# hi")).await).await,
        StatusCode::OK
    );
}
