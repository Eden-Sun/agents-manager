//! #1897：送達證不出來、字還留在框裡的回合，idle 收尾之前先補按一次 Enter；沒用才收成 failed＋ops_alert。
//! 全部走真的 `stuck_turns::sweep_at` 與 mock herdr 的 codex pane（`pane.read` 畫出框、`Enter` 會把框裡的字送進對話）。

use super::*;
use crate::capabilities::Db;
use crate::lifecycle::stuck_turns::{observe_at, sweep_at};
use crate::testing as tt;
use std::time::{Duration, Instant};

const MIN: Duration = Duration::from_secs(60);
const PROMPT: &str = "請照 #1872 的裁示把 CI 的快取鍵改掉";

struct Fixture {
    env: tt::Env,
    bot_id: String,
    run_id: String,
    pane: String,
    turn_id: String,
}

/// 一顆 codex bot、idle 的 run、一筆 `delivery='unknown'`／`delivery_verified=0` 的 in-flight（送達的那條路寫下的樣子），
/// pane 的框裡是 `composer` 那幾列。
async fn fixture(composer: &[&str], swallow_enter: bool) -> Fixture {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "a-ci").await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(app.db()).await.unwrap();
    let run_id = tt::fake_run(&app, &bot.id).await;
    let pane = format!("pane-{}", bot.id);
    env.herdr.live_pane(&pane, tt::LivePane {
        codex: true,
        width: Some(120),
        composer: composer.iter().map(|s| s.to_string()).collect(),
        swallow_enter,
        ..Default::default()
    });
    let conv = db::conversation_id(app.db(), &bot.id).await.unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, delivery_verified, auto_resend, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','unknown',0,1,?,?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run_id)
    .bind(PROMPT)
    .bind(db::iso_in(-3600))
    .execute(app.db())
    .await
    .unwrap();
    insert_message(&app, &conv, Some(&turn_id), "user", PROMPT, "web", false, None).await.unwrap();
    Fixture { env, bot_id: bot.id, run_id, pane, turn_id }
}

async fn turn(f: &Fixture) -> db::Turn {
    sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?").bind(&f.turn_id).fetch_one(f.env.app.db()).await.unwrap()
}

async fn resends(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT resend_count FROM turns WHERE id=?").bind(&f.turn_id).fetch_one(f.env.app.db()).await.unwrap()
}

fn enters(f: &Fixture) -> usize {
    f.env.herdr.calls_to("pane.send_keys").iter().filter(|c| c["keys"] == serde_json::json!(["Enter"])).count()
}

async fn alerts(f: &Fixture) -> Vec<String> {
    sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE kind='ops_alert' ORDER BY rowid")
        .fetch_all(f.env.app.db())
        .await
        .unwrap()
}

/// 一輪：從 `t0` 起算的第 `mins` 分鐘掃一次（門檻 5 分鐘）。
async fn sweep(f: &Fixture, t0: Instant, mins: u32) -> Vec<String> {
    sweep_at(&f.env.app, None, t0 + mins * MIN, 5 * MIN).await
}

/// 字還在框裡：只按一次 Enter、這一輪不收、額度記在 DB；Enter 生效（字進了對話）之後就交回一般流程。
#[tokio::test]
async fn a_prompt_left_in_the_composer_gets_one_enter_instead_of_a_fallback_close() {
    let f = fixture(&[PROMPT], false).await;
    let t0 = Instant::now();
    observe_at(&f.run_id, "idle", t0);

    assert!(sweep(&f, t0, 6).await.is_empty(), "補按 Enter 的這一輪不收");
    assert_eq!(enters(&f), 1);
    assert_eq!(turn(&f).await.status, "in_flight");
    assert_eq!(resends(&f).await, 1, "額度用掉、寫在 DB");
    assert!(f.env.herdr.pane(&f.pane).unwrap().composer.is_empty(), "Enter 把字送出去了");
    assert!(alerts(&f).await.is_empty());
}

/// 補按過、字還在：下一輪收成 failed（不是 completed_fallback）＋系統訊息＋ops_alert，不按第二次、不清框。
#[tokio::test]
async fn a_prompt_still_in_the_composer_after_the_one_enter_fails_with_an_alert() {
    let f = fixture(&[PROMPT], true).await;
    let t0 = Instant::now();
    observe_at(&f.run_id, "idle", t0);
    assert!(sweep(&f, t0, 6).await.is_empty());
    assert_eq!(enters(&f), 1);

    // 補按之後閒置重新計時：下一輪看到 idle 才起算，再過門檻才輪到它。
    assert!(sweep(&f, t0, 7).await.is_empty(), "閒置剛重新起算，不收");
    assert_eq!(sweep(&f, t0, 13).await, vec![f.turn_id.clone()]);
    let t = turn(&f).await;
    assert_eq!(t.status, "failed", "不是 completed_fallback");
    assert_eq!(t.delivery, "unknown", "送達欄位不動");
    assert_eq!(enters(&f), 1, "不按第二次");
    assert_eq!(f.env.herdr.pane(&f.pane).unwrap().composer, vec![PROMPT.to_string()], "不清框");
    assert!(f.env.herdr.calls_to("pane.send_keys").iter().all(|c| c["keys"] == serde_json::json!(["Enter"])), "沒有 ctrl+c 之類的清框鍵");
    assert_eq!(alerts(&f).await, vec![format!("undelivered_draft:{}", f.turn_id)]);
    let note: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system' AND content LIKE '%Enter 沒有生效%'")
        .bind(&f.turn_id)
        .fetch_one(f.env.app.db())
        .await
        .unwrap();
    assert_eq!(note, 1);
}

/// 額度已經用掉（例如 daemon 重啟前補按過）：不再按，字還在就直接收成 failed。
#[tokio::test]
async fn a_spent_resend_budget_is_not_pressed_again_after_a_restart() {
    let f = fixture(&[PROMPT], true).await;
    sqlx::query("UPDATE turns SET resend_count=1 WHERE id=?").bind(&f.turn_id).execute(f.env.app.db()).await.unwrap();
    let t0 = Instant::now();
    observe_at(&f.run_id, "idle", t0);

    assert_eq!(sweep(&f, t0, 6).await, vec![f.turn_id.clone()]);
    assert_eq!(enters(&f), 0);
    assert_eq!(turn(&f).await.status, "failed");
    assert_eq!(alerts(&f).await, vec![format!("undelivered_draft:{}", f.turn_id)]);
}

/// 框是空的：照原本收成 completed_fallback，一個鍵都不按、不吵巡檢。
#[tokio::test]
async fn an_empty_composer_keeps_the_fallback_close() {
    let f = fixture(&[], false).await;
    let t0 = Instant::now();
    observe_at(&f.run_id, "idle", t0);

    assert_eq!(sweep(&f, t0, 6).await, vec![f.turn_id.clone()]);
    assert_eq!(turn(&f).await.status, "completed_fallback");
    assert_eq!(enters(&f), 0);
    assert!(alerts(&f).await.is_empty());
}

/// 框裡是使用者改過的字（對不上這一則）：不按任何鍵，suspect alert 只發一次，照原本收尾。
#[tokio::test]
async fn an_edited_draft_is_never_submitted_and_raises_one_suspect_alert() {
    let f = fixture(&["請照 #1872 的裁示把 CI 的快取鍵改成別的名字"], false).await;
    let t0 = Instant::now();
    observe_at(&f.run_id, "idle", t0);

    assert_eq!(sweep(&f, t0, 6).await, vec![f.turn_id.clone()]);
    assert_eq!(enters(&f), 0, "不是我們那一則就不按");
    assert_eq!(turn(&f).await.status, "completed_fallback");
    let key = format!("undelivered_draft_suspect:{}", f.turn_id);
    assert_eq!(alerts(&f).await, vec![key.clone()]);

    // 同一筆再走一次判斷（例如收尾失敗下一輪重來）：alert 不重複。
    let run = db::run(f.env.app.db(), &f.run_id).await.unwrap().unwrap();
    let mut t = turn(&f).await;
    t.status = "in_flight".into();
    assert_eq!(before_idle_close(&f.env.app, &run, &t).await.unwrap(), Recovery::Proceed);
    assert_eq!(alerts(&f).await, vec![key], "同一個 key 只寫一次");
    assert_eq!(enters(&f), 0);
}

/// 讀畫面到按鍵之間 run 換了 pane（fence 內重讀）：不按，額度退回，這一輪不收。
#[tokio::test]
async fn a_run_that_changed_before_the_key_is_not_pressed() {
    let f = fixture(&[PROMPT], false).await;
    let app = f.env.app.clone();
    let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
    let t = turn(&f).await;
    sqlx::query("UPDATE runs SET pane_id='pane-moved' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();

    assert_eq!(before_idle_close(&app, &run, &t).await.unwrap(), Recovery::Wait);
    assert_eq!(enters(&f), 0, "pane 換了就不按");
    assert_eq!(resends(&f).await, 0, "沒按就退還額度");
    assert_eq!(turn(&f).await.status, "in_flight");
}

/// 外面讀畫面時字還在，fence 內送鍵前框裡的字被改掉了：不按，額度退回，這一輪不收。
#[tokio::test]
async fn a_composer_edited_after_the_first_read_is_not_pressed() {
    let f = fixture(&[PROMPT], false).await;
    let app = f.env.app.clone();
    let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
    let t = turn(&f).await;
    let sent = vec![PROMPT.to_string()];
    assert!(db::claim_resend(app.db(), &t.id, crate::lifecycle::poller::MAX_PROMPT_RESENDS).await.unwrap());
    f.env.herdr.live_pane(&f.pane, tt::LivePane {
        codex: true,
        width: Some(120),
        composer: vec!["請照 #1872 的裁示把 CI 的快取鍵改成別的名字".to_string()],
        ..Default::default()
    });

    assert_eq!(press_enter_once(&app, &run, "codex", &t, &sent).await.unwrap(), Recovery::Wait);
    assert_eq!(enters(&f), 0, "框裡的字已經不是這一則就不按");
    assert_eq!(resends(&f).await, 0, "沒按就退還額度");
    assert_eq!(turn(&f).await.status, "in_flight");
}

/// fence 內重讀畫面失敗：鍵還沒送，不按，額度退回，這一輪不收。
#[tokio::test]
async fn an_unreadable_screen_inside_the_fence_refunds_the_resend_without_pressing() {
    let f = fixture(&[PROMPT], false).await;
    let app = f.env.app.clone();
    let run = db::run(app.db(), &f.run_id).await.unwrap().unwrap();
    let t = turn(&f).await;
    let sent = vec![PROMPT.to_string()];
    assert!(db::claim_resend(app.db(), &t.id, crate::lifecycle::poller::MAX_PROMPT_RESENDS).await.unwrap());
    assert_eq!(resends(&f).await, 1);
    f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);

    assert_eq!(press_enter_once(&app, &run, "codex", &t, &sent).await.unwrap(), Recovery::Wait);
    assert_eq!(enters(&f), 0, "讀不到畫面就不按");
    assert_eq!(resends(&f).await, 0, "鍵沒送就退還額度");
    assert_eq!(turn(&f).await.status, "in_flight");
}

/// 讀不到的計數表滿了：新的 turn 先淘汰最舊的一筆，仍然可以等滿 UNREADABLE_ROUNDS 輪。
#[test]
fn a_full_unreadable_table_evicts_the_oldest_so_a_new_turn_still_waits() {
    let mut table = Unreadable::default();
    for i in 0..4 {
        assert!(table.again(&format!("stale-{i}"), 4));
    }
    assert_eq!(table.rounds.len(), 4);
    for round in 1..=UNREADABLE_ROUNDS {
        let wait = table.again("fresh", 4);
        assert_eq!(wait, round < UNREADABLE_ROUNDS, "第 {round} 輪");
    }
    assert!(!table.rounds.contains_key("stale-0"), "最舊的先被淘汰");
    assert!(table.rounds.contains_key("stale-3"), "其餘不動");
    assert!(table.rounds.len() <= 4);
}

/// 讀不到畫面：前兩輪不收；第三輪還讀不到就照原本收尾（記一次 WARN），不按任何鍵。
#[tokio::test]
async fn an_unreadable_screen_waits_three_sweeps_then_closes_as_before() {
    let f = fixture(&[PROMPT], false).await;
    let (logs, _guard) = crate::config_audit::capture::start();
    let t0 = Instant::now();
    observe_at(&f.run_id, "idle", t0);
    for round in 0..UNREADABLE_ROUNDS {
        f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);
        let closed = sweep(&f, t0, 6 + round).await;
        if round + 1 < UNREADABLE_ROUNDS {
            assert!(closed.is_empty(), "第 {} 輪讀不到：不收", round + 1);
            assert_eq!(turn(&f).await.status, "in_flight");
        } else {
            assert_eq!(closed, vec![f.turn_id.clone()], "第 {UNREADABLE_ROUNDS} 輪還讀不到：照原本收尾");
        }
    }
    assert_eq!(turn(&f).await.status, "completed_fallback");
    assert_eq!(enters(&f), 0);
    assert!(alerts(&f).await.is_empty());
    let warns: Vec<String> = logs.text().lines()
        .filter(|l| l.contains("WARN") && l.contains("screen_unreadable") && l.contains(&f.turn_id))
        .map(String::from)
        .collect();
    assert_eq!(warns.len(), 1, "第三輪記一次 WARN（turn、reason=screen_unreadable）：{}", logs.text());
}

/// 送達有證據（`delivery_verified=1`）的回合不在這條路上：照原本收尾，連畫面都不讀。
#[tokio::test]
async fn a_verified_turn_is_closed_as_before_without_reading_the_screen() {
    let f = fixture(&[PROMPT], false).await;
    sqlx::query("UPDATE turns SET delivery_verified=1 WHERE id=?").bind(&f.turn_id).execute(f.env.app.db()).await.unwrap();
    let t0 = Instant::now();
    observe_at(&f.run_id, "idle", t0);

    assert_eq!(sweep(&f, t0, 6).await, vec![f.turn_id.clone()]);
    assert_eq!(turn(&f).await.status, "completed_fallback");
    assert_eq!(enters(&f), 0);
    let _ = &f.bot_id;
}
