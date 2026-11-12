/// review 2026-09-16 core 5：遠端每次重連都走一次「連上」分支。使用者停掉的 autostart bot（`stop` 不改 `autostart`）
/// 不能在筆電睡醒重連後被重開；對帳失敗的那一次也不能跑（不知道哪些 agent 其實還活著）。
#[tokio::test]
async fn autostart_runs_once_per_host_and_only_after_a_successful_reconcile() {
    let env = crate::testing::env().await;
    let app = &env.app;
    assert!(!super::autostart_after_reconcile(app, "mac_builder", false).await, "對帳失敗不跑");
    assert!(super::autostart_after_reconcile(app, "mac_builder", true).await, "失敗那次不算數：下次連上照跑");
    assert!(!super::autostart_after_reconcile(app, "mac_builder", true).await, "重連不再跑");
    assert!(super::autostart_after_reconcile(app, "local", true).await, "每台主機各算各的");
}

#[tokio::test]
async fn autostart_retries_bots_skipped_while_the_host_is_disconnected() {
    let env = tt::env().await;
    let app = &env.app;
    let first = autostart_bot(&env, &env.project_id, "first").await;
    let second = autostart_bot(&env, &env.project_id, "second").await;

    app.connected.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(super::autostart_after_reconcile(app, "local", true).await);
    assert!(!running(app, &first).await && !running(app, &second).await);

    app.connected.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(eventually_running(app, &first).await, "重連後補起第一顆");
    assert!(eventually_running(app, &second).await, "重連後補起尚未判斷的第二顆");
    for bot in [&first, &second] {
        let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=?")
            .bind(bot)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(runs, 1, "每顆只起一次");
    }
}

#[tokio::test]
async fn dropping_an_interrupted_autostart_claim_allows_a_later_pass() {
    let env = tt::env().await;
    let app = &env.app;
    let claim = super::AutostartClaim::begin(app, "local").expect("claim absent host");
    let since = claim.since.clone();
    assert_eq!(
        app.autostart_hosts.lock().unwrap().get("local"),
        Some(&crate::state::AutostartHostStatus::InProgress)
    );

    drop(claim);
    assert!(!app.autostart_hosts.lock().unwrap().contains_key("local"), "cancelled pass clears its claim");

    let retry = super::AutostartClaim::begin(app, "local").expect("retry after cancellation");
    assert_eq!(retry.since, since, "retry preserves the first attempt time");
    retry.complete();
    assert_eq!(
        app.autostart_hosts.lock().unwrap().get("local"),
        Some(&crate::state::AutostartHostStatus::Done)
    );
    assert!(!app.autostart_since.lock().unwrap().contains_key("local"), "completed pass releases its retry time");
    assert!(super::AutostartClaim::begin(app, "local").is_none(), "completed pass runs once");
}

use crate::db;
use crate::state::App;
use crate::testing as tt;
use std::sync::Arc;

async fn autostart_bot(env: &tt::Env, project: &str, name: &str) -> String {
    let bot = tt::claude_bot(&env.app, project, name).await;
    sqlx::query("UPDATE bots SET autostart=1 WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    bot.id
}

async fn running(app: &Arc<App>, bot: &str) -> bool {
    db::active_run(&app.db, bot).await.unwrap().is_some()
}

async fn eventually_running(app: &Arc<App>, bot: &str) -> bool {
    crate::testing::eventually!(running(app, bot).await)
}

/// **#209.** 開機那一刻 bot 清單讀不到：以前變成空清單、一顆都不起，主機卻已經標成「跑過」——本機不會有下一次連上，
/// AGM 在內的 autostart bot 要到下次重啟才起。現在這一輪不起，DB 恢復後背景自己補起，而且主機仍只算跑過一次。
#[tokio::test]
async fn an_autostart_that_cannot_list_bots_starts_them_once_the_db_can() {
    let env = tt::env().await;
    let app = &env.app;
    let bot = autostart_bot(&env, &env.project_id, "auto").await;

    sqlx::query("ALTER TABLE bots RENAME TO bots_unreadable").execute(&app.db).await.unwrap();
    assert!(super::autostart_after_reconcile(app, "local", true).await);
    sqlx::query("ALTER TABLE bots_unreadable RENAME TO bots").execute(&app.db).await.unwrap();
    assert!(eventually_running(app, &bot).await, "DB 恢復之後，沒有任何重連也要起來");
    assert!(!super::autostart_after_reconcile(app, "local", true).await, "主機照樣只算跑過一次");
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&bot).fetch_one(&app.db).await.unwrap();
    assert_eq!(runs, 1, "只起一次");
}

/// #209 同一支：讀不到 active run 也不當成沒在跑——這一輪不起、記成欠著，讀得到之後再判斷（以前落進 `start_bot`、
/// 在鎖裡再讀一次失敗，之後就再也沒人管）。這一列讀不出來（欄位型別錯，跟 I/O 錯誤一樣是 `Err`）；修好時它已經結束。
#[tokio::test]
async fn an_autostart_that_cannot_read_active_runs_retries_that_bot() {
    let env = tt::env().await;
    let app = &env.app;
    let bot = autostart_bot(&env, &env.project_id, "auto").await;
    let run = db::ulid();
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,agent_name,started_at) VALUES (?,?,'running','idle',X'6B6964','2020-01-01T00:00:00.000Z')")
        .bind(&run)
        .bind(&bot)
        .execute(&app.db)
        .await
        .unwrap();

    assert!(super::autostart_after_reconcile(app, "local", true).await);
    let runs = || async { sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&bot).fetch_one(&app.db).await.unwrap() };
    assert_eq!(runs().await, 1, "讀不到它在不在跑：不起");
    sqlx::query("UPDATE runs SET agent_name=NULL, state='exited', ended_at=? WHERE id=?").bind(db::now()).bind(&run).execute(&app.db).await.unwrap();
    assert!(eventually_running(app, &bot).await, "讀得到之後再判斷：確定沒在跑就起");
}

/// #209 同一支：某顆讀不到主機不能當成本機（那會對別台的 bot 下 start），只跳過那顆、其他照起；讀得到之後補起。
/// 補的時候，使用者在這之間自己起過又停掉的那顆不再替它起——停掉的就是要它停。
#[tokio::test]
async fn an_autostart_bot_whose_host_is_unreadable_is_skipped_and_retried_alone() {
    let env = tt::env().await;
    let app = &env.app;
    let good = autostart_bot(&env, &env.project_id, "good").await;
    // 主機欄讀不出字串（型別錯，跟 I/O 錯誤一樣是 `Err`）：只有這個專案的 bot 讀不到主機。
    let broken = db::ulid();
    let repo = env.dir.join("broken");
    tt::git::init_repo(&repo);
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'broken', X'6C6F63616C', ?)")
        .bind(&broken)
        .bind(repo.to_string_lossy().to_string())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let later = autostart_bot(&env, &broken, "later").await;
    let stopped = autostart_bot(&env, &broken, "stopped").await;

    assert!(super::autostart_after_reconcile(app, "local", true).await);
    assert!(running(app, &good).await, "讀得到的照起");
    assert!(!running(app, &later).await && !running(app, &stopped).await, "讀不到主機：不當成本機起");

    // 這之間使用者自己起過 `stopped` 又停掉它。
    sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at,ended_at) VALUES (?,?,'stopped','idle',?,?)")
        .bind(db::ulid())
        .bind(&stopped)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE projects SET host='local' WHERE id=?").bind(&broken).execute(&app.db).await.unwrap();
    assert!(eventually_running(app, &later).await, "讀得到之後補起");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!running(app, &stopped).await, "使用者停掉的不再替它起");
    let good_runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=?").bind(&good).fetch_one(&app.db).await.unwrap();
    assert_eq!(good_runs, 1, "已經判斷過的不再碰");
}
