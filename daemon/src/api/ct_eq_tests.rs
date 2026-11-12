use super::ct_eq;

#[test]
fn compares_whole_tokens_exactly() {
    assert!(ct_eq("0123abcd", "0123abcd"));
    assert!(!ct_eq("0123abcd", "0123abce"), "只差最後一位");
    assert!(!ct_eq("0123abcd", "1123abcd"), "只差第一位");
    assert!(!ct_eq("0123abcd", "0123abc"), "長度不同");
    assert!(!ct_eq("", "x"));
    assert!(ct_eq("", ""), "空對空只是函式本身的性質；呼叫端仍要先擋空 token");
}

#[tokio::test]
async fn post_order_with_primary_writes_positions_and_state_reports_them() {
    use super::*;
    let e = crate::testing::env().await;
    let a = crate::testing::claude_bot(&e.app, &e.project_id, "a").await.id;
    let b = crate::testing::claude_bot(&e.app, &e.project_id, "b").await.id;
    let order = |ids: &[&str]| SetOrder { projects: None, bots: None, primary: Some(ids.iter().map(|s| s.to_string()).collect()) };
    let resp = set_order(State(e.app.clone()), Json(order(&[&b, &a]))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let state = state_json(&e.app).await.unwrap();
    let bots = state["projects"][0]["bots"].as_array().unwrap().clone();
    let pos = |id: &str| bots.iter().find(|x| x["id"] == id).unwrap()["primary_position"].as_i64().unwrap();
    assert_eq!((pos(&b), pos(&a)), (1, 2));
    // 未知 bot → 400，而且什麼都沒寫。
    let err = set_order(State(e.app.clone()), Json(order(&[&a, "ghost"]))).await.unwrap_err();
    assert!(matches!(err, LcError::Bad(_)));
    let state = state_json(&e.app).await.unwrap();
    let bots = state["projects"][0]["bots"].as_array().unwrap().clone();
    assert_eq!(bots.iter().find(|x| x["id"] == a.as_str()).unwrap()["primary_position"], 2);
    // 三個欄位都沒有仍是 400。
    let none = SetOrder { projects: None, bots: None, primary: None };
    assert!(matches!(set_order(State(e.app.clone()), Json(none)).await.unwrap_err(), LcError::Bad(_)));
}

#[tokio::test]
async fn post_order_rolls_back_if_a_named_bot_disappears_during_the_write() {
    use super::*;
    let e = crate::testing::env().await;
    let a = crate::testing::claude_bot(&e.app, &e.project_id, "a").await.id;
    let b = crate::testing::claude_bot(&e.app, &e.project_id, "b").await.id;
    // 模擬 validate 之後、第二筆 UPDATE 之前另一個請求刪掉 b。
    let trigger = format!(
        "CREATE TRIGGER delete_order_bot_after_first_update AFTER UPDATE OF primary_position ON bots WHEN NEW.id = '{a}' BEGIN UPDATE bots SET deleted_at = 'triggered' WHERE id = '{b}'; END"
    );
    sqlx::query(&trigger).execute(&e.app.db).await.unwrap();
    let body = SetOrder { projects: None, bots: None, primary: Some(vec![a.clone(), b.clone()]) };

    let err = set_order(State(e.app.clone()), Json(body)).await;
    assert!(err.is_err(), "bot 在寫入途中消失時不能回成功");
    let a_row: (i64, Option<String>) = sqlx::query_as("SELECT primary_position, deleted_at FROM bots WHERE id = ?")
        .bind(&a)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    let b_row: (i64, Option<String>) = sqlx::query_as("SELECT primary_position, deleted_at FROM bots WHERE id = ?")
        .bind(&b)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(a_row, (0, None), "前一筆位置也要跟著回滾");
    assert_eq!(b_row, (0, None), "被刪掉的 bot 變更也要跟著回滾");
}
