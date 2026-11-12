//! 對話 for a hookless child run (`managed_by='child'`): scraped off a mock herdr pane via
//! `capture_hookless_turn_locked`.
use super::*;
use crate::testing as tt;

/// A finished claude exchange as `recent_unwrapped` renders it.
const EXCHANGE: &str = "\
❯ 幫我看一下 lifecycle.rs
  ⎿  Read lifecycle.rs (4308 lines)
⏺ 看完了：try_fallback 只認 in-flight turn。

✻ Worked for 9s · done 11:35 PM
────────────────────────────────────────────
❯
────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

struct Child {
    env: tt::Env,
    bot_id: String,
    conv: String,
    run_id: String,
}

/// An adopted hookless bot on `pane-1`; `hooks` / `adopted` decide whether the terminal is the source.
async fn child(status: &str, screen: &str, hooks: i64, adopted: i64) -> Child {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, managed_by, hook_token, created_at)
             VALUES (?,?,'lastq','claude','[]',0,?,'child','tok',?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(hooks)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, started_at)
             VALUES (?,?,'running',?,'ws-1','pane-1','tab-1',?,'parent-lastq','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(status)
    .bind(adopted)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    env.herdr.set_screen("pane-1", screen);
    Child { env, bot_id, conv, run_id }
}

async fn messages(app: &Arc<App>, conv: &str) -> Vec<(String, String, String)> {
    sqlx::query_as::<_, (String, String, String)>(
        // Same order as `GET /api/bots/{id}/messages`.
        "SELECT role, content, source FROM messages WHERE conversation_id = ? ORDER BY id",
    )
    .bind(conv)
    .fetch_all(&app.db)
    .await
    .unwrap()
}

#[tokio::test]
async fn synthetic_hookless_codex_does_not_infer_compaction_from_answer_text() {
    let screen = "› Reply with the requested text\nWorked for 21s • 11:03 AM\n• Context compacted · 3s\nWorked for 3s • 11:57 AM\n› \n";
    let c = child("idle", screen, 0, 1).await;
    let app = c.env.app.clone();
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&c.bot_id).execute(&app.db).await.unwrap();
    assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
    let msgs = messages(&app, &c.conv).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1], ("assistant".into(), "Context compacted · 3s".into(), "terminal_fallback".into()));
    assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
}

/// The bug: a child adopted mid-answer hits `working -> idle` with no turn in flight, and
/// `try_fallback` bails — 對話 stayed empty. The edge now becomes a turn of its own.
#[tokio::test]
async fn a_finished_exchange_on_the_pane_becomes_a_turn() {
    let c = child("idle", EXCHANGE, 0, 1).await;
    let app = c.env.app.clone();

    assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap(), "the screen holds an exchange");

    let msgs = messages(&app, &c.conv).await;
    assert_eq!(msgs.len(), 2, "one prompt, one reply: {msgs:?}");
    assert_eq!(msgs[0].0, "user");
    assert_eq!(msgs[0].1, "幫我看一下 lifecycle.rs", "what was typed straight into the pane");
    assert_eq!(msgs[0].2, "terminal_fallback");
    assert_eq!(msgs[1].0, "assistant");
    assert_eq!(
        msgs[1].1, "看完了：try_fallback 只認 in-flight turn。",
        "the `⏺` reply, with the status line and the composer chrome dropped",
    );
    assert_eq!(msgs[1].2, "terminal_fallback");
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id = ?")
        .bind(&c.conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!((t.origin.as_str(), t.status.as_str()), ("external", "completed_fallback"));
    assert_eq!(t.run_id.as_deref(), Some(c.run_id.as_str()));
    assert!(t.completed_at.is_some(), "nothing is left in flight, so the composer is not locked");
}

/// Repeated `working -> idle` edges and re-adoption must not grow the conversation.
#[tokio::test]
async fn the_same_screen_is_never_stored_twice() {
    let c = child("idle", EXCHANGE, 0, 1).await;
    let app = c.env.app.clone();

    assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
    assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap(), "nothing new on the pane");
    assert!(!capture_hookless_turn_locked(&app, &c.run_id, true).await.unwrap(), "and the adoption seed is one-shot");

    assert_eq!(messages(&app, &c.conv).await.len(), 2);
}

/// #193：去重要看的上一則回覆讀不到，不當成「還沒有回覆」——以前照樣開一個外部回合，同一份回覆存第二次
/// （這裡訊息寫不進去，留下一個沒有訊息的回合）。
#[tokio::test]
async fn an_unreadable_last_reply_is_not_taken_as_no_reply() {
    let c = child("idle", EXCHANGE, 0, 1).await;
    let app = c.env.app.clone();
    assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
    // herdr 對同一份回答報了第二次 `working -> idle`，游標又不在（重讀同一個畫面）：只有去重擋得住。
    sqlx::query("UPDATE runs SET last_read_tail_hash=NULL WHERE id=?").bind(&c.run_id).execute(&app.db).await.unwrap();

    sqlx::query("ALTER TABLE messages RENAME TO messages_unreadable").execute(&app.db).await.unwrap();
    assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.is_err(), "讀不到上一則回覆是錯，不是「沒有」");
    sqlx::query("ALTER TABLE messages_unreadable RENAME TO messages").execute(&app.db).await.unwrap();
    assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap(), "讀得到了：同一份回覆");

    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&c.conv).fetch_one(&app.db).await.unwrap();
    assert_eq!(turns, 1, "沒有多開回合");
    assert_eq!(messages(&app, &c.conv).await.len(), 2);
}

/// The next prompt typed into the pane is a second turn, not an addition to the first.
#[tokio::test]
async fn the_next_exchange_is_its_own_turn() {
    let c = child("idle", EXCHANGE, 0, 1).await;
    let app = c.env.app.clone();
    assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());

    c.env.herdr.set_screen("pane-1", &format!("{EXCHANGE}❯ 再看一次\n⏺ 修好了。\n"));
    assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());

    let msgs = messages(&app, &c.conv).await;
    assert_eq!(msgs.len(), 4, "{msgs:?}");
    assert_eq!(msgs[2].1, "再看一次");
    assert_eq!(msgs[3].1, "修好了。");
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?")
        .bind(&c.conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(turns, 2);
}

/// A pane we started keeps hooks as source of truth (SPEC §4.3: snapshot is the 備援); this path must not touch it.
#[tokio::test]
async fn a_run_with_hooks_is_left_to_its_hooks() {
    let c = child("idle", EXCHANGE, 1, 1).await;
    let app = c.env.app.clone();
    assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
    assert!(messages(&app, &c.conv).await.is_empty());

    // Same for a run the daemon started itself, hooks or not.
    let own = child("idle", EXCHANGE, 0, 0).await;
    let app = own.env.app.clone();
    assert!(!capture_hookless_turn_locked(&app, &own.run_id, false).await.unwrap());
    assert!(messages(&app, &own.conv).await.is_empty());
}

/// No cursor and no echo: storing the whole scrollback would merge turns, so store nothing and remember the cursor.
#[tokio::test]
async fn a_screen_with_no_prompt_echo_is_not_guessed_at() {
    let c = child("idle", "⏺ 一段沒有頭的舊輸出\n", 0, 1).await;
    let app = c.env.app.clone();

    assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
    assert!(messages(&app, &c.conv).await.is_empty());
    assert!(
        db::run(&app.db, &c.run_id).await.unwrap().unwrap().last_read_tail_hash.is_some(),
        "the cursor moved, so the next real exchange is read from here",
    );
}

/// The adoption seed only fires into an empty conversation (re-adoption on every restart / reconnect).
#[tokio::test]
async fn the_adoption_seed_refuses_a_conversation_that_already_has_messages() {
    let c = child("idle", EXCHANGE, 0, 1).await;
    let app = c.env.app.clone();
    insert_message(&app, &c.conv, None, "user", "早先的訊息", "web", false, None).await.unwrap();

    assert!(!capture_hookless_turn_locked(&app, &c.run_id, true).await.unwrap());
    assert_eq!(messages(&app, &c.conv).await.len(), 1);
    let _ = &c.bot_id;
}
