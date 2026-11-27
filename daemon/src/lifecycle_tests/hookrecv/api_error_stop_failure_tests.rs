use super::*;
use crate::lifecycle::api_error_nudge;
use crate::testing as tt;
use std::time::Duration;

const SCREEN: &str = "\
❯ 跑一次測試
⏺ 我先看一下現在的實作。

⏺ API Error: Connection lost mid-response. The response above may be incomplete.

✻ Baked for 5m 21s · done 12:56 AM
────────────────────────────────────────────
❯
────────────────────────────────────────────
";

/// 有 hook 的 claude bot＋已送達、還在飛的回合；pane 用 bot 自己的名字，畫面互不干擾。回 (bot, run, turn, pane)。
async fn delivered(app: &Arc<App>, project_id: &str) -> (String, String, String, String) {
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(project_id)
    .bind(format!("aesf-{}", &bot_id[bot_id.len() - 8..]))
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let (run_id, turn_id, pane) = (db::ulid(), db::ulid(), format!("pane-{bot_id}"));
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','working','ws-1',?,'agent','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(&pane)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','跑一次測試',?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    (bot_id, run_id, turn_id, pane)
}

fn stop_failure(bot_id: &str, reason: &str) -> HookBody {
    HookBody {
        bot_id: bot_id.to_string(),
        provider: "claude".into(),
        payload: json!({"hook_event_name": "StopFailure", "session_id": format!("s-{bot_id}"), "prompt_id": format!("p-{bot_id}"), "reason": reason}),
        received_at: None,
        truncated: false,
        run_id: None,
    }
}

async fn turn_status(app: &Arc<App>, id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
}

async fn settle_idle(app: &Arc<App>, run_id: &str) {
    sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(run_id).execute(&app.db).await.unwrap();
}

async fn nudge_crids(app: &Arc<App>, bot_id: &str) -> Vec<String> {
    let conv = db::conversation_id(&app.db, bot_id).await.unwrap();
    sqlx::query_scalar("SELECT client_request_id FROM turns WHERE conversation_id=? AND status='queued'")
        .bind(&conv)
        .fetch_all(&app.db)
        .await
        .unwrap()
}

/// 2026-11-27 m4p robins-hub lead：有 hook 的 claude 回合被「Connection lost mid-response」截斷，`StopFailure` 把回合收成 failed，
/// 但從來沒有人 capture 畫面、arm 續行提示。現在 StopFailure 之後背景補讀畫面：讀到錯誤列就釘 `turn_error`、[`api_error_nudge::WAIT`]
/// 後條件都成立就排一則 `api-error-nudge:<turn>` 回合。
#[tokio::test]
async fn an_api_stop_failure_with_a_connection_lost_line_queues_a_continue_nudge() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, run_id, turn_id, pane) = delivered(&app, &env.project_id).await;

    process(&app, &stop_failure(&bot_id, "API Error: Connection lost mid-response")).await.unwrap();
    assert_eq!(turn_status(&app, &turn_id).await, "failed");
    assert_eq!(api_error_nudge::pending_capture(&bot_id).as_deref(), Some(run_id.as_str()), "StopFailure 之後要排畫面補讀");
    env.herdr.set_screen(&pane, SCREEN);
    assert!(api_error_nudge::capture_after_stop_failure(&app, &bot_id, &run_id, &[Duration::ZERO]).await);
    let row = db::run(&app.db, &run_id).await.unwrap().unwrap();
    assert!(row.turn_error.as_deref().is_some_and(|e| e.contains("Connection lost")), "{:?}", row.turn_error);
    assert!(api_error_nudge::pending_capture(&bot_id).is_none());

    settle_idle(&app, &run_id).await;
    assert!(api_error_nudge::check(&app, &bot_id).await, "WAIT 之後 idle、沒有新回合：排續行提示");
    assert_eq!(nudge_crids(&app, &bot_id).await, vec![format!("{}{turn_id}", api_error_nudge::CRID_PREFIX)]);
}

/// 錯誤列一直沒畫出來（重試用完）：不補，也不留著等。
#[tokio::test]
async fn a_stop_failure_without_an_error_line_is_not_nudged() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, run_id, _turn, pane) = delivered(&app, &env.project_id).await;
    env.herdr.know_pane(&pane);
    process(&app, &stop_failure(&bot_id, "API Error: Connection lost mid-response")).await.unwrap();
    assert!(!api_error_nudge::capture_after_stop_failure(&app, &bot_id, &run_id, &[Duration::ZERO, Duration::ZERO]).await);
    assert!(api_error_nudge::pending_capture(&bot_id).is_none());
    settle_idle(&app, &run_id).await;
    assert!(!api_error_nudge::check(&app, &bot_id).await);
}

/// 撞額度的 StopFailure 不排畫面補讀，也就不會補續行提示。
#[tokio::test]
async fn a_quota_stop_failure_is_not_nudged() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _run, turn_id, _pane) = delivered(&app, &env.project_id).await;
    process(&app, &stop_failure(&bot_id, "You've hit your session limit · resets 5pm")).await.unwrap();
    assert_eq!(turn_status(&app, &turn_id).await, "failed");
    assert!(api_error_nudge::pending_capture(&bot_id).is_none());

}

/// 錯誤列讀到、arm 了，但 WAIT 期間使用者自己送了新回合：不補。
#[tokio::test]
async fn a_new_user_turn_in_the_window_cancels_the_nudge() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, run_id, _turn, pane) = delivered(&app, &env.project_id).await;
    process(&app, &stop_failure(&bot_id, "API Error: Connection lost mid-response")).await.unwrap();
    env.herdr.set_screen(&pane, SCREEN);
    assert!(api_error_nudge::capture_after_stop_failure(&app, &bot_id, &run_id, &[Duration::ZERO]).await);
    settle_idle(&app, &run_id).await;
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web','completed','ok','我自己接著做',?)")
        .bind(db::ulid())
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    assert!(!api_error_nudge::check(&app, &bot_id).await);
    assert!(nudge_crids(&app, &bot_id).await.is_empty());
}
