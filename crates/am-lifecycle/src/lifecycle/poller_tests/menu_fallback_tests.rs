//! 2026-09-25 cf-ox-fork-fork：claude 2.1.281 停在「Session paused」選單、herdr 判 idle，備援把
//! `2. Edit prompt and retry with …` 當成回覆存下來、回合收掉。停在等人選的選單上不能收。
use super::*;
use crate::testing as tt;
use crate::tui_prompts::screens::SESSION_PAUSED;

#[tokio::test]
async fn a_turn_paused_at_a_numbered_menu_is_not_closed_and_the_menu_is_not_stored() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "paused").await;
    let run = tt::fake_run(&app, &bot.id).await;
    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    sqlx::query("UPDATE turns SET prompt_text='派工' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
    let pane = format!("pane-{}", bot.id);
    env.herdr.set_screen(&pane, &format!("❯ 派工\n\n{SESSION_PAUSED}"));

    assert!(!try_fallback(&app, &run, Some(&turn)).await.unwrap(), "停在選單上：不收");
    assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "in_flight");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
    assert_eq!(stored, 0, "選單那幾行不是回覆");

    // 使用者選完、claude 答完回到輸入列：照常收，存的是回覆本身。
    env.herdr.set_screen(&pane, "❯ 派工\n\n⏺ 做完了。\n\n✻ Cooked for 3s\n────────\n❯ \n────────\n");
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let reply: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&app.db).await.unwrap();
    assert!(reply.contains("做完了") && !reply.contains("Edit prompt"), "{reply}");
}
