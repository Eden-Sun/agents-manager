use super::*;
use crate::testing as tt;

fn handed_off_reason(err: LcError) -> String {
    match err {
        LcError::Conflict(body) => body["reason"].as_str().unwrap_or_default().to_owned(),
        other => panic!("expected handed_off conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn delete_project_and_bot_refuse_handed_off_ownership() {
    let project_env = tt::env().await;
    let project_bot = tt::claude_bot(&project_env.app, &project_env.project_id, "delete-project-handed").await;
    sqlx::query("UPDATE projects SET handed_off_to='mac' WHERE id=?")
        .bind(&project_env.project_id)
        .execute(&project_env.app.db)
        .await
        .unwrap();
    let project_result = delete_project(State(project_env.app.clone()), Path(project_env.project_id.clone())).await;

    let bot_env = tt::env().await;
    let bot = tt::claude_bot(&bot_env.app, &bot_env.project_id, "delete-bot-handed").await;
    sqlx::query("UPDATE projects SET handed_off_to='mac' WHERE id=?").bind(&bot_env.project_id).execute(&bot_env.app.db).await.unwrap();
    let bot_result = delete_bot(State(bot_env.app.clone()), Path(bot.id.clone())).await;

    assert_eq!(handed_off_reason(project_result.unwrap_err()), "handed_off");
    assert_eq!(handed_off_reason(bot_result.unwrap_err()), "handed_off");
    assert!(db::bot(&project_env.app.db, &project_bot.id).await.unwrap().unwrap().deleted_at.is_none(), "project refusal leaves bots intact");
    assert!(db::project(&project_env.app.db, &project_env.project_id).await.unwrap().unwrap().deleted_at.is_none(), "handoff refusal leaves project intact");
    assert!(db::bot(&bot_env.app.db, &bot.id).await.unwrap().unwrap().deleted_at.is_none(), "bot refusal leaves bot intact");
}
