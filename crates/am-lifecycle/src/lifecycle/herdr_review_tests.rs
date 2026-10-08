//! herdr 0.9.3 下的 start／restart 對帳審查：同一顆 bot 同時兩個 start、herdr 在 agent.start／停機當下斷線。
use super::*;
use crate::testing as tt;

async fn active_runs(app: &Arc<App>, bot_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ? AND state IN ('starting','running','stopping')")
        .bind(bot_id)
        .fetch_one(&app.db)
        .await
        .unwrap()
}

/// 同一顆 bot 同時兩個 start：只開一個 agent、一個 run，另一個回「已有 active run」。
#[tokio::test]
async fn two_concurrent_starts_of_one_bot_open_exactly_one_agent() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "dup").await;
    let (a, b) = tokio::join!(start_bot(&e.app, &bot.id), start_bot(&e.app, &bot.id));
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1, "一個成功一個被擋：{a:?} / {b:?}");
    assert_eq!(e.herdr.calls_to("agent.start").len(), 1, "只開一個 agent");
    assert_eq!(active_runs(&e.app, &bot.id).await, 1);
}

/// herdr 在 agent.start 當下斷線：不留 starting／running 的殭屍 run，下一次 start 照常成功。
#[tokio::test]
async fn a_herdr_that_drops_agent_start_leaves_no_zombie_run() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "drop").await;
    e.herdr.fail_next("agent.start", tt::Fault::DropBefore);
    let first = start_bot(&e.app, &bot.id).await;
    assert!(first.is_err(), "start 要回錯：{first:?}");
    assert_eq!(active_runs(&e.app, &bot.id).await, 0, "沒有殭屍 run：{:?}", db::active_run(&e.app.db, &bot.id).await.unwrap().map(|r| r.state));
    start_bot(&e.app, &bot.id).await.expect("herdr 回來之後可以再開");
    assert_eq!(active_runs(&e.app, &bot.id).await, 1);
}

/// herdr server 重啟（整個 session 清空、但回的是一份有效的空清單）：autostart 的 top-level bot 的 run 收成 `exited`
/// （不是 `stopped`＝使用者要它停），`bot_stopped` incident 探針才認得出它該被報（`supervisor/incidents.rs`）。
#[tokio::test]
async fn a_herdr_restart_that_empties_the_session_ends_the_run_as_exited_not_stopped() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "auto").await;
    sqlx::query("UPDATE bots SET autostart = 1 WHERE id = ?").bind(&bot.id).execute(&e.app.db).await.unwrap();
    start_bot(&e.app, &bot.id).await.unwrap();
    e.herdr.agents.lock().unwrap().clear();
    e.herdr.workspaces.lock().unwrap().clear();
    e.herdr.tabs.lock().unwrap().clear();
    crate::reconcile::reconcile_host(&e.app, crate::config::LOCAL_HOST).await.unwrap();
    let (state, why): (String, Option<String>) = sqlx::query_as("SELECT state, exit_reason FROM runs WHERE bot_id = ? ORDER BY started_at DESC LIMIT 1")
        .bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!((state.as_str(), why.as_deref()), ("exited", Some("agent not found during reconcile")), "不是使用者停的（不是 stopped）");
}

/// 同一顆 bot 同時兩個 restart（連點兩下）：兩次都在 bot 鎖裡依序做完，結束後恰好一個 active run、舊的都收乾淨，不留殭屍。
#[tokio::test]
async fn two_concurrent_restarts_leave_exactly_one_active_run() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "rr").await;
    start_bot(&e.app, &bot.id).await.unwrap();
    let (a, b) = tokio::join!(restart_bot(&e.app, &bot.id), restart_bot(&e.app, &bot.id));
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM runs WHERE bot_id = ? ORDER BY started_at").bind(&bot.id).fetch_all(&e.app.db).await.unwrap();
    assert!(a.is_ok() && b.is_ok(), "兩次重啟都依序做完：{a:?} / {b:?}");
    assert_eq!(states.iter().filter(|s| s.as_str() == "running").count(), 1, "只有最後一個在跑：{states:?}");
    assert_eq!(active_runs(&e.app, &bot.id).await, 1);
}

/// start 與 stop 同時打（依鎖序做完）：不能留下兩個 active run。
#[tokio::test]
async fn a_start_racing_a_stop_never_leaves_two_runs_or_a_live_agent_under_a_stopped_run() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "ss").await;
    start_bot(&e.app, &bot.id).await.unwrap();
    let (a, b) = tokio::join!(stop_bot(&e.app, &bot.id), start_bot(&e.app, &bot.id));
    let n = active_runs(&e.app, &bot.id).await;
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM runs WHERE bot_id = ? ORDER BY started_at").bind(&bot.id).fetch_all(&e.app.db).await.unwrap();
    assert!(a.is_ok() && b.is_ok(), "stop 與 start 都依鎖序做完：{a:?} / {b:?}");
    assert_eq!(n, 1, "start 排在 stop 後面：恰好一個 run 在跑：{states:?}");
}
