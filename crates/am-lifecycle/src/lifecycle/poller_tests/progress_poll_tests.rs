//! #193：終端側的安全網讀 DB 一時失敗，不能當成「回合沒了」就收手。故障用改表名／trigger 注入；poller 的輪詢只對
//! 這幾個 run 調快（[`fast`]），每一輪讀 DB 之前的 `progress_poll` 注入點讓故障確定落在某一輪、下一輪恢復。
use super::*;
use crate::lifecycle::race_point;
use crate::testing as tt;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn intervals() -> &'static Mutex<HashMap<String, Duration>> {
    static M: OnceLock<Mutex<HashMap<String, Duration>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

pub(super) fn interval(run_id: &str) -> Option<Duration> {
    intervals().lock().ok()?.get(run_id).copied()
}

fn fast(run_id: &str) {
    intervals().lock().unwrap().insert(run_id.to_string(), Duration::from_millis(10));
}

const PONG: &str = "❯ Reply with PONG\n⏺ PONG\n✻ Worked for 5s · done 1:07 AM\n──────\n❯\n";
const BUSY: &str = "❯ Reply with PONG\n✢ Baking… (3s · esc to interrupt)\n──────\n❯\n";

struct F {
    env: tt::Env,
    bot_id: String,
    run_id: String,
    turn_id: String,
}

/// 一顆在跑的 claude（`pane-p`），一筆在飛、送達 ok 的回合，對話裡是 `prompt`。
async fn in_flight(screen: &str, prompt: &str) -> F {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "polled").await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at)
             VALUES (?,?,'running','idle','pane-p','test',?)",
    )
    .bind(&run_id)
    .bind(&bot.id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user',?,'web',?)")
        .bind(db::ulid())
        .bind(&conv)
        .bind(&turn_id)
        .bind(prompt)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    env.herdr.set_screen("pane-p", screen);
    F { env, bot_id: bot.id, run_id, turn_id }
}

async fn unreadable(app: &Arc<App>, table: &str, yes: bool) {
    let sql = if yes { format!("ALTER TABLE {table} RENAME TO {table}_unreadable") } else { format!("ALTER TABLE {table}_unreadable RENAME TO {table}") };
    sqlx::query(&sql).execute(&app.db).await.unwrap();
}

/// 還沒恢復才恢復（注入點可能走到、也可能沒走到）。
async fn readable_again(app: &Arc<App>, table: &str) {
    let _ = sqlx::query(&format!("ALTER TABLE {table}_unreadable RENAME TO {table}")).execute(&app.db).await;
}

/// 這一輪（`progress_poll`）先跑 `now`，下一輪跑 `next` 並通知測試。poller 在這一輪之後就退出的話，永遠等不到通知。
fn this_poll_then_next<A, AF, B, BF>(run_id: &str, now: A, next: B) -> tokio::sync::oneshot::Receiver<()>
where
    A: FnOnce() -> AF + Send + 'static,
    AF: std::future::Future<Output = ()> + Send + 'static,
    B: FnOnce() -> BF + Send + 'static,
    BF: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let run = run_id.to_string();
    race_point::arm("progress_poll", run_id, move || async move {
        now().await;
        race_point::arm("progress_poll", &run, move || async move {
            next().await;
            let _ = tx.send(());
        });
    });
    rx
}

async fn still_polled_after(rx: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("讀錯之後 poller 應該還在：下一輪沒有來，它在讀錯那一輪就退出了")
        .unwrap();
}

async fn status(app: &Arc<App>, turn_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn_id).fetch_one(&app.db).await.unwrap()
}

async fn pollers(app: &Arc<App>, run_id: &str) -> Option<bool> {
    app.progress_pollers.lock().await.get(run_id).map(|h| h.is_finished())
}

async fn wait_for_status(app: &Arc<App>, turn_id: &str, want: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while status(app, turn_id).await != want {
        assert!(std::time::Instant::now() < deadline, "回合沒有變成 {want}（pane 閒著、沒有 Stop hook，閒置備援應該收掉它）");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_until_poller_gone(app: &Arc<App>, run_id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while pollers(app, run_id).await.is_some() {
        assert!(std::time::Instant::now() < deadline, "回合收掉了，poller 應該退出");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// am-lead 2026-09-25：herdr 抖一下開了沒有 prompt 的 external 回合，游標對不上（狀態列額度數字變了），備援把上一段回覆又存一次。
/// 沒有 prompt 的 external 回合讀到跟上一則一樣的回覆：收掉回合、不存第二次；有人送 prompt 的回合回一樣的話照存。
#[tokio::test]
async fn a_prompt_less_external_turn_does_not_store_the_previous_reply_again() {
    let f = in_flight(PONG, "Reply with PONG").await;
    let app = f.env.app.clone();
    assert!(try_fallback(&app, &f.run_id, Some(&f.turn_id)).await.unwrap());
    let conv = db::conversation_id(&app.db, &f.bot_id).await.unwrap();
    let replies = |app: Arc<App>, conv: String| async move {
        sqlx::query_scalar::<_, String>("SELECT content FROM messages WHERE conversation_id=? AND role='assistant'").bind(conv).fetch_all(&app.db).await.unwrap()
    };
    assert_eq!(replies(app.clone(), conv.clone()).await, vec!["PONG"]);

    let open = |origin: &'static str| {
        let (app, conv, run) = (app.clone(), conv.clone(), f.run_id.clone());
        async move {
            // 游標對不上＝整個畫面都算新的。
            sqlx::query("UPDATE runs SET last_read_tail_hash='nope' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
            let tid = db::ulid();
            sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,?,'in_flight','ok',?)")
                .bind(&tid).bind(&conv).bind(&run).bind(origin).bind(db::now()).execute(&app.db).await.unwrap();
            tid
        }
    };
    let ext = open("external").await;
    assert!(try_fallback(&app, &f.run_id, Some(&ext)).await.unwrap(), "回合照樣收掉");
    assert_eq!(status(&app, &ext).await, "completed_fallback");
    assert_eq!(replies(app.clone(), conv.clone()).await, vec!["PONG"], "同一段回覆不能存第二次");

    let web = open("web").await;
    sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user','Reply with PONG','web',?)")
        .bind(db::ulid()).bind(&conv).bind(&web).bind(db::now()).execute(&app.db).await.unwrap();
    assert!(try_fallback(&app, &f.run_id, Some(&web)).await.unwrap());
    assert_eq!(replies(app.clone(), conv).await, vec!["PONG", "PONG"], "有人送 prompt 的回合回一樣的話要照存");
}

/// 驗收 1、2：某一輪讀 `in_flight_turn` 失敗（SQLite busy／I/O error），一輪之後 DB 恢復——poller 還活著；之後 pane
/// 閒著、沒有 Stop hook，閒置備援照樣把回合收掉。以前讀錯＝`still=false`＝退出，那一回合從此沒人收。
#[tokio::test]
async fn a_transient_in_flight_read_error_does_not_end_the_poller() {
    let f = in_flight(PONG, "Reply with PONG").await;
    let app = f.env.app.clone();
    fast(&f.run_id);
    let (a, b) = (app.clone(), app.clone());
    let rx = this_poll_then_next(&f.run_id, move || async move { unreadable(&a, "turns", true).await }, move || async move {
        unreadable(&b, "turns", false).await
    });
    arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
    still_polled_after(rx).await;

    wait_for_status(&app, &f.turn_id, "completed_fallback").await;
    let reply: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&f.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(reply, "PONG");
    wait_until_poller_gone(&app, &f.run_id).await;
}

/// 驗收 3：讀到了、確定不是這一回合（`Ok(None)`）才停。
#[tokio::test]
async fn the_poller_stops_once_the_turn_is_really_closed() {
    let f = in_flight(BUSY, "Reply with PONG").await;
    let app = f.env.app.clone();
    // A healthy Herdr working report has no synthetic receipt to restore. The stale-cache
    // case (Stop before the idle redraw) is covered by observed_status's real-poller test.
    sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?")
        .bind(&f.run_id).execute(&app.db).await.unwrap();
    fast(&f.run_id);
    let rx = this_poll_then_next(&f.run_id, || async {}, || async {});
    arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
    still_polled_after(rx).await;
    assert_eq!(pollers(&app, &f.run_id).await, Some(false), "回合還在飛：poller 在跑");

    sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?").bind(db::now()).bind(&f.turn_id).execute(&app.db).await.unwrap();
    wait_until_poller_gone(&app, &f.run_id).await;
}

/// 驗收 4：讀 `runs` 一時失敗一樣重讀，不退出；恢復之後還是同一個 poller。
#[tokio::test]
async fn a_transient_run_read_error_does_not_end_the_poller() {
    let f = in_flight(BUSY, "Reply with PONG").await;
    let app = f.env.app.clone();
    fast(&f.run_id);
    let (a, b) = (app.clone(), app.clone());
    let rx = this_poll_then_next(&f.run_id, move || async move { unreadable(&a, "runs", true).await }, move || async move {
        unreadable(&b, "runs", false).await
    });
    arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
    still_polled_after(rx).await;
    assert_eq!(pollers(&app, &f.run_id).await, Some(false));
    assert_eq!(app.progress_pollers.lock().await.len(), 1, "沒有多出第二個 poller");
}

/// 驗收 4：掛上的那一刻讀不到 bot，以前 poller 直接不開始；現在下一輪再讀。
#[tokio::test]
async fn a_bot_that_cannot_be_read_when_the_poller_starts_is_read_again() {
    let f = in_flight(BUSY, "Reply with PONG").await;
    let app = f.env.app.clone();
    fast(&f.run_id);
    unreadable(&app, "bots", true).await;
    let b = app.clone();
    // 第一輪 bot 還讀不到，第二輪之前恢復。
    let rx = this_poll_then_next(&f.run_id, || async {}, move || async move { unreadable(&b, "bots", false).await });
    arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
    still_polled_after(rx).await;
    let again = this_poll_then_next(&f.run_id, || async {}, || async {});
    still_polled_after(again).await;
    assert_eq!(pollers(&app, &f.run_id).await, Some(false));
    assert_eq!(app.progress_pollers.lock().await.len(), 1);
}

/// 開始新回合時上一回合的錯誤標記清不掉（寫入失敗）：poller 之後再清。留著的話，這一回合斷在同一句錯誤上會被
/// `turn_error::capture` 的「同一則只記一次」吞掉。
#[tokio::test]
async fn the_previous_turns_error_is_cleared_once_the_write_goes_through() {
    let f = in_flight(BUSY, "Reply with PONG").await;
    let app = f.env.app.clone();
    fast(&f.run_id);
    sqlx::query("UPDATE runs SET turn_error='API Error: Connection lost mid-response.' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_clear BEFORE UPDATE OF turn_error ON runs WHEN NEW.turn_error IS NULL
             BEGIN SELECT RAISE(ABORT, 'injected: cannot clear runs.turn_error'); END",
    )
    .execute(&app.db)
    .await
    .unwrap();
    let a = app.clone();
    let rx = this_poll_then_next(&f.run_id, move || async move { sqlx::query("DROP TRIGGER refuse_clear").execute(&a.db).await.unwrap(); }, || async {});
    arm_progress(&app, &f.run_id, &f.bot_id, &f.turn_id).await;
    still_polled_after(rx).await;
    let left: Option<String> = sqlx::query_scalar("SELECT turn_error FROM runs WHERE id=?").bind(&f.run_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(left, None, "寫得進去之後就清掉了");
}

/// 備援收回合之前讀不到送了什麼：不收（回合留在飛），不把我們自己的 prompt 當成 agent 的回覆存下來。
#[tokio::test]
async fn a_fallback_that_cannot_read_what_was_sent_closes_nothing() {
    let f = in_flight(PONG, "Reply with PONG").await;
    let app = f.env.app.clone();
    unreadable(&app, "messages", true).await;
    // 讀完「送了什麼」之後 DB 就恢復：接著寫得進去的話，錯的回覆就真的存下來了。
    let a = app.clone();
    race_point::arm("fallback_after_echo", &f.turn_id, move || async move { readable_again(&a, "messages").await });

    let first = {
        let lock = app.bot_lock(&f.bot_id).await;
        let _g = lock.lock().await;
        try_fallback(&app, &f.run_id, Some(&f.turn_id)).await
    };
    assert!(first.is_err(), "讀不到送了什麼是錯，不是「什麼都沒送」：{first:?}");
    assert_eq!(status(&app, &f.turn_id).await, "in_flight");
    readable_again(&app, "messages").await;

    assert!(try_fallback(&app, &f.run_id, Some(&f.turn_id)).await.unwrap(), "讀得到了：照常收");
    let reply: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&f.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(reply, "PONG");
}
