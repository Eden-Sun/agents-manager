//! 強制重啟（使用者 2026-11-18）：`POST /api/bots/{id}/restart` 成功後清掉「閒下來自動重啟」的旗標；child 照舊 409。
use super::*;

async fn running_bot(e: &crate::testing::Env, name: &str) -> db::Bot {
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
    lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
    bot
}

fn no_query() -> Query<StartQuery> {
    Query(serde_json::from_value(json!({})).unwrap())
}

#[tokio::test]
async fn a_forced_restart_clears_the_idle_restart_flag() {
    let e = crate::testing::env().await;
    let bot = running_bot(&e, "forced").await;
    let other = running_bot(&e, "other").await;
    db::update_restart::schedule(&e.app.db, &[bot.id.clone(), other.id.clone()]).await.unwrap();
    let mut events = e.app.subscribe();

    let res = restart_bot(State(e.app.clone()), Path(bot.id.clone()), no_query()).await;
    assert!(res.is_ok(), "{:?}", res.err());

    assert_eq!(db::update_restart::list(&e.app.db).await.unwrap(), vec![other.id.clone()], "只清被強制重啟的那顆，別顆的旗標照舊");
    let mut seen = None;
    while let Ok(ev) = events.try_recv() {
        if ev.kind == "update_restart_changed" {
            seen = Some(ev.data);
        }
    }
    assert_eq!(seen.expect("清旗標要通知網頁")["bot_ids"], json!([other.id]));
}

#[tokio::test]
async fn a_forced_restart_that_fails_keeps_the_flag() {
    let e = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "broken").await;
    sqlx::query("UPDATE bots SET identity='nope' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
    let run = crate::testing::fake_run(&e.app, &bot.id).await;
    let _ = run;
    db::update_restart::schedule(&e.app.db, std::slice::from_ref(&bot.id)).await.unwrap();
    let res = restart_bot(State(e.app.clone()), Path(bot.id.clone()), no_query()).await;
    assert!(res.is_err(), "夾具的身份讓 start 失敗");
    assert!(db::update_restart::is_scheduled(&e.app.db, &bot.id).await.unwrap(), "失敗＝更新沒套上，旗標留著");
}

#[tokio::test]
async fn a_child_still_cannot_be_force_restarted_and_keeps_nothing_scheduled() {
    let e = crate::testing::env().await;
    let parent = running_bot(&e, "parent").await;
    let kid = crate::testing::claude_bot(&e.app, &e.project_id, "kid").await;
    sqlx::query("UPDATE bots SET managed_by='child', parent_bot_id=? WHERE id=?").bind(&parent.id).bind(&kid.id).execute(&e.app.db).await.unwrap();
    let res = restart_bot(State(e.app.clone()), Path(kid.id.clone()), no_query()).await;
    assert!(matches!(res, Err(LcError::Conflict(_))), "{:?}", res.err());
}
