//! #1899：頂層主力 bot 的 `limit_hit`。`/api/state` 與 `bot_status` 必須給同一個值（都走 [`super::limit_hit_json`]）。

use super::*;
use crate::config::LOCAL_HOST;
use crate::quota::seed_limit_hit;

fn future() -> String {
    crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2))
}

/// `/api/state` 裡這顆 bot 的那一列。
async fn state_bot(app: &Arc<App>, bot_id: &str) -> Value {
    let state = crate::api::state_json(app).await.unwrap();
    state["projects"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["bots"].as_array().cloned().unwrap_or_default())
        .find(|b| b["id"] == bot_id)
        .expect("bot 在 /api/state 裡")
}

/// 這顆 bot 下一則 `bot_status` 的 `limit_hit`。
async fn status_limit_hit(app: &Arc<App>, bot_id: &str) -> Value {
    let mut rx = app.subscribe();
    app.emit_bot_status(bot_id).await;
    while let Ok(ev) = rx.try_recv() {
        if ev.kind == "bot_status" && ev.data["bot_id"] == bot_id {
            return ev.data["limit_hit"].clone();
        }
    }
    panic!("bot_status 沒有發出來");
}

#[tokio::test]
async fn a_top_level_bot_under_a_live_hit_gets_it_in_state_and_in_bot_status() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "hit").await;
    let until = future();
    assert!(seed_limit_hit(&app, LOCAL_HOST, "claude", &until, "You've hit your usage limit", None).await);

    let from_state = state_bot(&app, &bot.id).await["limit_hit"].clone();
    let from_status = status_limit_hit(&app, &bot.id).await;
    assert_eq!(from_state["until"], until.as_str());
    assert_eq!(from_state["message"], "You've hit your usage limit");
    assert_eq!(from_state, from_status, "/api/state 與 bot_status 給同一個值");
}

#[tokio::test]
async fn an_expired_hit_is_null_in_both_places() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "expired").await;
    assert!(seed_limit_hit(&app, LOCAL_HOST, "claude", &future(), "usage limit", None).await);
    let past = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(1));
    app.quotas.lock().await.get_mut("claude").unwrap().limit_hit.as_mut().unwrap().until = Some(past);

    assert_eq!(state_bot(&app, &bot.id).await["limit_hit"], Value::Null);
    assert_eq!(status_limit_hit(&app, &bot.id).await, Value::Null);
}

#[tokio::test]
async fn a_child_bot_is_always_null_even_when_its_key_is_hit() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let parent = crate::testing::claude_bot(&app, &env.project_id, "parent").await;
    let child = crate::testing::claude_bot(&app, &env.project_id, "child").await;
    sqlx::query("UPDATE bots SET parent_bot_id=?, managed_by='child' WHERE id=?")
        .bind(&parent.id)
        .bind(&child.id)
        .execute(&app.db)
        .await
        .unwrap();
    assert!(seed_limit_hit(&app, LOCAL_HOST, "claude", &future(), "usage limit", None).await);

    assert_eq!(state_bot(&app, &child.id).await["limit_hit"], Value::Null);
    assert_eq!(status_limit_hit(&app, &child.id).await, Value::Null);
}

#[tokio::test]
async fn a_model_bucket_that_does_not_block_this_model_is_null() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "opus-bot").await;
    let run = crate::testing::fake_run(&app, &bot.id).await;
    sqlx::query("UPDATE runs SET runtime_model='opus' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
    // 撞的是 Fable 週窗；這顆跑 opus，不被擋（`bucket_blocks_model`）。
    assert!(seed_limit_hit(&app, LOCAL_HOST, "claude", &future(), "You've hit your Fable limit", Some("fable".into())).await);

    assert_eq!(state_bot(&app, &bot.id).await["limit_hit"], Value::Null);
    assert_eq!(status_limit_hit(&app, &bot.id).await, Value::Null);
}

#[tokio::test]
async fn an_unreadable_lookup_is_null_and_does_not_fail_the_projection() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "unreadable").await;
    assert!(seed_limit_hit(&app, LOCAL_HOST, "claude", &future(), "usage limit", None).await);
    crate::testing::make_table_unreadable(&app, "projects").await;

    let limit_hit = limit_hit_json(&app, &bot).await;
    assert_eq!(limit_hit, Value::Null, "讀不到就 null，不是錯誤");
    crate::testing::make_table_readable(&app, "projects").await;
}

/// #1899：批次版（`/api/state`）逐顆等於單筆版（`try_limit_hit_for_bot`）。owed 那一支與單筆共用同一個
/// `owed_limit_hit`，這裡不另造欠帳（欠帳的寫入私有於 am-lifecycle）。
#[tokio::test]
async fn the_batch_gives_every_bot_what_the_single_bot_path_gives() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    // 撞限中（claude 的共用 key）。
    crate::testing::claude_bot(&app, &env.project_id, "hit").await;
    assert!(seed_limit_hit(&app, LOCAL_HOST, "claude", &future(), "usage limit", None).await);
    // 過期：撞限還在，until 已過。
    codex_bot(&app, &env.project_id, "expired").await;
    assert!(seed_limit_hit(&app, LOCAL_HOST, "codex", &future(), "usage limit", None).await);
    app.quotas.lock().await.get_mut("codex").unwrap().limit_hit.as_mut().unwrap().until =
        Some(crate::db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(1)));
    // bucket 不擋：自己的 key（claude:cc1）撞 Fable，這顆跑 opus。
    let opus = crate::testing::claude_bot(&app, &env.project_id, "opus").await;
    sqlx::query("UPDATE bots SET identity='cc1' WHERE id=?").bind(&opus.id).execute(&app.db).await.unwrap();
    let run = crate::testing::fake_run(&app, &opus.id).await;
    sqlx::query("UPDATE runs SET runtime_model='opus' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
    assert!(seed_limit_hit(&app, LOCAL_HOST, "claude:cc1", &future(), "Fable limit", Some("fable".into())).await);
    // child：key 上有撞限，但 child 的投影一律 null（這裡比的是撞限判斷本身，所以單筆與批次都要有值）。
    let parent = crate::testing::claude_bot(&app, &env.project_id, "parent").await;
    let child = crate::testing::claude_bot(&app, &env.project_id, "child").await;
    sqlx::query("UPDATE bots SET parent_bot_id=?, managed_by='child' WHERE id=?").bind(&parent.id).bind(&child.id).execute(&app.db).await.unwrap();
    codex_bot(&app, &env.project_id, "nohit").await;

    let bots = crate::db::live_bots(&app.db).await.unwrap();
    let batch = crate::runners::quota::limit_hits_for_bots(&app, &bots).await;
    assert_eq!(batch.len(), bots.len());
    for (bot, got) in bots.iter().zip(&batch) {
        let single = crate::runners::quota::try_limit_hit_for_bot(&app, bot).await.unwrap();
        assert_eq!(got, &single, "bot {} 的批次與單筆不一致", bot.name);
    }
    let by = |name: &str| bots.iter().zip(&batch).find(|(b, _)| b.name == name).map(|(_, h)| h.is_some()).unwrap();
    assert!(by("hit"), "撞限中");
    assert!(!by("expired"), "過期");
    assert!(!by("opus"), "bucket 不擋這個模型");
    assert!(by("child"), "child 的單筆判斷也有值（投影才會變 null）");
    assert!(!by("nohit"), "沒撞限");
}

/// 讀不到：單筆回 Err，批次對整批給 null（查詢是整張表失敗，沒有逐顆的部分結果）。
#[tokio::test]
async fn an_unreadable_table_is_an_error_for_one_bot_and_null_for_the_batch() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let bot = crate::testing::claude_bot(&app, &env.project_id, "unreadable").await;
    assert!(seed_limit_hit(&app, LOCAL_HOST, "claude", &future(), "usage limit", None).await);
    crate::testing::make_table_unreadable(&app, "projects").await;

    assert!(crate::runners::quota::try_limit_hit_for_bot(&app, &bot).await.is_err());
    let batch = crate::runners::quota::limit_hits_for_bots(&app, std::slice::from_ref(&bot)).await;
    assert_eq!(batch, vec![None]);
    crate::testing::make_table_readable(&app, "projects").await;
}

/// codex 的 bot（`testing` 只有 claude 的建立函式）。
async fn codex_bot(app: &Arc<App>, project_id: &str, name: &str) -> crate::db::Bot {
    let bot = crate::testing::claude_bot(app, project_id, name).await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap()
}
