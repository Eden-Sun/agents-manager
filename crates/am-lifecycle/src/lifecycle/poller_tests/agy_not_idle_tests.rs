//! #2146：agy 的 Stop 帶 `fullyIdle:false`、herdr 報 idle 時，5 秒終端備援不能把還在工作的回合收掉
//! （2026-11-17 agi1：前景 `○ Running (wait-log.py …)`＋背景 task 還在跑，被收成 completed_fallback）。
use super::*;
use crate::hookrecv::{process, HookBody};
use crate::lifecycle::run_state::{a_turn, turn_status};
use crate::testing as tt;
use serde_json::json;

const RULE: &str = "────────────────────────────────────────────────────────";
fn idle_screen() -> String {
    format!("● 做完了 ALPHA\n{RULE}\n>\n{RULE}\n  Gemini 3.8 Flash · high\n")
}
fn busy_screen() -> String {
    format!("○ Running (wait-log.py --timeout 540 · 12s)\n{RULE}\n>\n{RULE}\n● [09:18:37] wait-log.py running\nesc to cancel   Gemini 3.8 Flash · high · 1 task(s) · /tasks\n")
}

struct F {
    env: tt::Env,
    bot_id: String,
    run_id: String,
}

async fn fixture() -> F {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "agy-nonidle").await;
    sqlx::query("UPDATE bots SET kind='agy' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    let run_id = tt::fake_run(&env.app, &bot.id).await;
    env.herdr.set_screen(&format!("pane-{}", bot.id), &idle_screen());
    F { env, bot_id: bot.id, run_id }
}

async fn stop(f: &F, fully_idle: bool) {
    let payload = json!({"hookEventName": "Stop", "conversationId": "c-1", "terminationReason": "NO_TOOL_CALL", "fullyIdle": fully_idle,
        "lastAssistantMessage": "ALPHA", "lastUserMessage": "go"});
    let body = HookBody { bot_id: f.bot_id.clone(), provider: "agy".into(), payload, received_at: None, truncated: false, run_id: None };
    process(&f.env.app, &body).await.unwrap();
}

#[tokio::test]
async fn fully_idle_false_stop_keeps_the_turn_in_flight_through_the_fallback_then_fully_idle_closes_it_once() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

    stop(&f, false).await;
    assert_eq!(turn_status(&app, &a).await, "in_flight", "fullyIdle:false 不收回合");
    assert!(!try_fallback(&app, &f.run_id, Some(&a)).await.unwrap(), "畫面看起來閒置，但證據還在：備援不收");
    assert_eq!(turn_status(&app, &a).await, "in_flight");

    stop(&f, true).await;
    assert_eq!(turn_status(&app, &a).await, "completed", "之後的 fullyIdle:true 正常收");
    stop(&f, true).await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'").bind(&a).fetch_one(&app.db).await.unwrap();
    assert_eq!(n, 1, "只收一次");
}

#[tokio::test]
async fn evidence_from_an_old_turn_does_not_affect_a_new_turn() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    stop(&f, false).await;
    assert!(crate::lifecycle::agy_busy::holds(&f.run_id, &a));
    crate::lifecycle::turn_controller::set_status(&app.db, &a, "in_flight", "completed", "test").await.unwrap();
    let b = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    assert!(!crate::lifecycle::agy_busy::holds(&f.run_id, &b));
    assert!(try_fallback(&app, &f.run_id, Some(&b)).await.unwrap());
    assert_eq!(turn_status(&app, &b).await, "completed_fallback");
}

#[tokio::test]
async fn a_new_idle_after_working_releases_the_evidence() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    stop(&f, false).await;
    crate::lifecycle::stuck_turns::observe_at(&f.run_id, "working", std::time::Instant::now());
    crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
    assert!(try_fallback(&app, &f.run_id, Some(&a)).await.unwrap());
    assert_eq!(turn_status(&app, &a).await, "completed_fallback");
}

#[tokio::test]
async fn a_busy_agy_screen_blocks_the_fallback_even_without_hook_evidence() {
    let f = fixture().await;
    let app = f.env.app.clone();
    f.env.herdr.set_screen(&format!("pane-{}", f.bot_id), &busy_screen());
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    assert!(!try_fallback(&app, &f.run_id, Some(&a)).await.unwrap());
    assert_eq!(turn_status(&app, &a).await, "in_flight");
}
