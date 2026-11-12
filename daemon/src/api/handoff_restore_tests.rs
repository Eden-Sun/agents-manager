use super::*;
use crate::testing as tt;

#[tokio::test]
async fn restore_bot_refuses_handed_off_ownership_before_mutating() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "restore-handed").await;
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&bot.id).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE projects SET handed_off_to='mac' WHERE id=?").bind(&e.project_id).execute(&e.app.db).await.unwrap();

    let err = restore_bot(State(e.app.clone()), Path(bot.id.clone())).await.unwrap_err();

    assert!(matches!(err, LcError::Conflict(ref body) if body["reason"] == "handed_off"), "expected handoff refusal, got {err:?}");
    assert!(db::bot(&e.app.db, &bot.id).await.unwrap().unwrap().deleted_at.is_some(), "handoff refusal leaves bot deleted");
}
