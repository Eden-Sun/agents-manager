//! issue #216：§4.3 備援計時器綁定排定當下的回合。全部依序真的呼叫（`arm_fallback_after` 排計時、真的開新回合、
//! 真的等計時到點走完 `try_fallback`），不靠 trigger 或注入點插窄窗——現況（計時到點抓「當下在飛的」）第一條就紅。
use super::*;
use crate::lifecycle::run_state::{a_turn, turn_status};
use crate::testing as tt;

/// 計時縮短到 100ms；別的測試的 5 秒不動。
const SHORT: Duration = Duration::from_millis(100);
/// 上一則（A）的回覆還在畫面上、輸入框是空的：沒有 spinner、不是工具進度——備援收得下去。
const A_REPLY: &str = "❯ 派工\n⏺ 上一則的回覆 ALPHA\n✻ Worked for 5s · done 1:07 AM\n──────\n❯\n";

struct F {
    env: tt::Env,
    bot_id: String,
    run_id: String,
}

/// 一顆在跑的 claude，pane 上是 A 的回覆。
async fn fixture() -> F {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "armed").await;
    let run_id = tt::fake_run(&env.app, &bot.id).await;
    env.herdr.set_screen(&format!("pane-{}", bot.id), A_REPLY);
    F { env, bot_id: bot.id, run_id }
}

/// Stop hook 收掉一回合（`hookrecv` 做的事：in_flight -> completed）。
async fn stop_hook_closes(f: &F, turn: &str) {
    let done = super::super::turn_controller::set_status(&f.env.app.db, turn, "in_flight", "completed", "test：Stop hook").await.unwrap();
    assert_eq!(done, super::super::turn_controller::Outcome::Applied);
}

async fn assistant_messages(f: &F) -> Vec<(Option<String>, String)> {
    let conv = db::conversation_id(&f.env.app.db, &f.bot_id).await.unwrap();
    sqlx::query_as("SELECT turn_id, content FROM messages WHERE conversation_id=? AND role='assistant' ORDER BY created_at")
        .bind(conv)
        .fetch_all(&f.env.app.db)
        .await
        .unwrap()
}

/// 排了計時、而且等它真的走完（計時任務最後會把自己這一代從表裡拿掉）。走不完＝這條測試沒測到東西，不算過。
async fn arm_and_wait(f: &F, delay: Duration) {
    let app = &f.env.app;
    arm_fallback_after(app, &f.run_id, &f.bot_id, delay).await;
    wait_for_timer(f).await;
}

async fn armed(f: &F) -> bool {
    f.env.app.fallback_timers.lock().await.contains_key(&f.run_id)
}

async fn wait_for_timer(f: &F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while armed(f).await {
        assert!(std::time::Instant::now() < deadline, "備援計時器沒有走完");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 事故（2026-09-19 真機）：A 的 Stop hook 收掉 A → 畫面 idle，排備援 → 計時內使用者送出 B，CLI 還沒畫 spinner →
/// 到點時「在飛的」是 B，畫面上最後一段回覆是 A 的。B 不能被收成 `completed_fallback`、也不能存 A 的回覆。
#[tokio::test]
async fn a_turn_sent_after_the_idle_edge_is_not_closed_with_the_previous_reply() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    stop_hook_closes(&f, &a).await;

    arm_fallback_after(&app, &f.run_id, &f.bot_id, SHORT).await;
    assert!(armed(&f).await, "排定了計時");
    let b = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    wait_for_timer(&f).await;

    assert_eq!(turn_status(&app, &b).await, "in_flight", "B 留給自己的 hook／下一次 edge／poller");
    assert_eq!(assistant_messages(&f).await, vec![], "沒有把 A 的回覆存成任何人的回覆");
    assert_eq!(turn_status(&app, &a).await, "completed");
}

/// 同一件事、Stop hook 晚一步：edge 進來時 A 還在飛（排定綁 A），A 被 hook 收掉、B 送出，到點在飛的是 B。
#[tokio::test]
async fn the_timer_stays_bound_to_the_turn_that_was_in_flight_when_it_was_armed() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

    arm_fallback_after(&app, &f.run_id, &f.bot_id, SHORT).await;
    stop_hook_closes(&f, &a).await;
    let b = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    wait_for_timer(&f).await;

    assert_eq!(turn_status(&app, &b).await, "in_flight");
    assert_eq!(assistant_messages(&f).await, vec![]);
}

/// 對照組：hook 一直沒來、到點在飛的就是排定當下那一回合——備援照舊把它收掉（功能沒被綁壞）。
#[tokio::test]
async fn the_turn_the_timer_was_armed_for_is_still_closed_from_the_pane() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

    arm_and_wait(&f, SHORT).await;

    assert_eq!(turn_status(&app, &a).await, "completed_fallback");
    assert_eq!(assistant_messages(&f).await, vec![(Some(a), "上一則的回覆 ALPHA".to_string())]);
}

/// 新回合自己的 working -> idle 會排一個新的計時（換掉舊的一代）：那一個綁 B，B 的回覆照樣收得到。
#[tokio::test]
async fn a_later_idle_edge_arms_the_timer_for_the_new_turn() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    stop_hook_closes(&f, &a).await;
    arm_fallback_after(&app, &f.run_id, &f.bot_id, Duration::from_millis(400)).await;

    let b = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    f.env.herdr.set_screen(&format!("pane-{}", f.bot_id), "❯ 派工\n⏺ B 的回覆 BRAVO\n✻ Worked for 5s · done 1:08 AM\n──────\n❯\n");
    arm_fallback_after(&app, &f.run_id, &f.bot_id, SHORT).await;
    wait_for_timer(&f).await;

    assert_eq!(turn_status(&app, &b).await, "completed_fallback");
    assert_eq!(assistant_messages(&f).await, vec![(Some(b), "B 的回覆 BRAVO".to_string())]);
    // 舊的那一代（400ms）到點時已經被換掉，什麼都不動。
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(assistant_messages(&f).await.len(), 1);
}

/// 讀不到「現在哪一回合在飛」就不排：排了也不知道要收哪一回合，收錯比不收糟。回合留在飛，由 poller 的閒置備援與 stuck watchdog 兜著。
#[tokio::test]
async fn no_timer_is_armed_when_it_cannot_tell_which_turn_is_in_flight() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

    tt::make_table_unreadable(&app, "turns").await;
    arm_fallback_after(&app, &f.run_id, &f.bot_id, SHORT).await;
    tt::make_table_readable(&app, "turns").await;

    assert!(!armed(&f).await, "讀不到就沒有排計時");
    assert_eq!(turn_status(&app, &a).await, "in_flight");
}

/// poller 的閒置備援（`arm_progress`）同一條規則：它問的是自己那一回合，在飛的若是別的就不收。
#[tokio::test]
async fn a_fallback_only_closes_the_turn_it_was_asked_about() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    stop_hook_closes(&f, &a).await;
    let b = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

    assert!(!try_fallback(&app, &f.run_id, Some(&a)).await.unwrap(), "問的是 A，在飛的是 B：不收");
    assert!(!try_fallback(&app, &f.run_id, None).await.unwrap(), "排定當下沒有回合在飛，在飛的是後來才開的 B：不收");
    assert_eq!(turn_status(&app, &b).await, "in_flight");
    assert_eq!(assistant_messages(&f).await, vec![]);

    assert!(try_fallback(&app, &f.run_id, Some(&b)).await.unwrap(), "問的就是 B：照舊收");
    assert_eq!(turn_status(&app, &b).await, "completed_fallback");
}
