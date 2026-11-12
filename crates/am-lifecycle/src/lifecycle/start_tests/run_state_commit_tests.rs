//! #145：跨過 `agent.start` 之後，run 狀態寫不進去就不能回「啟動成功」，也不能留下一顆永遠卡住的 run。
use super::super::run_state as rs;
use super::*;
use crate::capabilities::Db;
use crate::testing as tt;

fn starts(env: &tt::Env) -> usize {
    env.herdr.methods().iter().filter(|m| *m == "agent.start").count()
}

async fn live_runs(app: &Arc<App>, bot_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=? AND state IN ('starting','running','stopping')")
        .bind(bot_id)
        .fetch_one(app.db())
        .await
        .unwrap()
}

/// 驗收 1、2：agent 真的起來了，`running` 卻寫不進去——不回一般的成功、不把活著的 agent 殺掉，
/// DB 恢復後照 herdr 的證據收成 `running`，不再開第二顆。
#[tokio::test]
async fn a_start_whose_running_state_cannot_be_recorded_is_not_reported_as_started() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
    rs::refuse_run_state(&app, "running").await;

    let err = start_bot(&app, &bot.id).await.expect_err("DB 沒記下 running，不能回啟動成功");
    // 先看錯誤是哪一種：沒走到 agent.start 就失敗（例如這台沒裝 claude）的話，訊息直接寫在這裡。
    let LcError::Uncommitted(body) = &err else { panic!("要 503 start_state_uncommitted，拿到 {err:?}") };
    let run = db::active_run(app.db(), &bot.id).await.unwrap().expect("agent 在跑，run 不能被收成 exited");
    assert_eq!((body["error"].as_str(), body["run_id"].as_str()), (Some("start_state_uncommitted"), Some(run.id.as_str())));
    assert_eq!(run.state, "starting");
    let methods = env.herdr.methods();
    assert!(!methods.iter().any(|m| m == "pane.close"), "活著的 agent 不收：{methods:?}");
    assert_eq!(starts(&env), 1);
    assert_eq!(rs::scheduled(&run.id), vec![rs::Settle::Reconcile { stuck: "starting".into() }]);

    rs::accept_run_state(&app, "running").await;
    assert!(rs::settle_once(&app, &run.id, &rs::Settle::Reconcile { stuck: "starting".into() }).await);
    assert_eq!(db::run(app.db(), &run.id).await.unwrap().unwrap().state, "running", "照 herdr 的證據收斂");
    assert_eq!(starts(&env), 1, "沒有再開第二顆");
    assert_eq!(live_runs(&app, &bot.id).await, 1);
}

/// 同驗收 1，走重啟（換身份、`?resume=native`、一鍵重啟）而且 AGM 有一則排著的派工：重啟中不撤（#106），但對帳把新 run
/// 收成 `running` 不會叫 flush（#152 在 start_send 補過同一個洞），它起來時的 idle 邊又早在 `starting` 就過了——要自己等它
/// 收斂再叫，不然那一則要等 30 分鐘的排隊保險絲把它撤掉、交辦停在 blocked。
#[tokio::test]
async fn a_restart_whose_new_run_cannot_be_recorded_running_still_flushes_the_queue_once_it_settles() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
    start_bot(&app, &bot.id).await.expect("先起來");
    let queued = rs::a_turn(&app, &bot.id, None, "queued").await;
    rs::refuse_run_state(&app, "running").await;

    let err = restart_bot(&app, &bot.id).await.expect_err("新 run 沒記下 running");
    let LcError::Uncommitted(body) = &err else { panic!("要 503 start_state_uncommitted，拿到 {err:?}") };
    let run = db::active_run(app.db(), &bot.id).await.unwrap().expect("新 run 停在 starting");
    assert_eq!((body["run_id"].as_str(), run.state.as_str()), (Some(run.id.as_str()), "starting"));
    assert_eq!(rs::turn_status(&app, &queued).await, "queued", "重啟中：不當孤兒撤");
    assert!(super::super::start_send::watching_for_running(&run.id), "排了「收成 running 就叫 flush」");
}

/// 驗收 3：start 失敗、pane 收掉了，`exited` 卻也寫不進去——不能留一顆永遠擋住下一次 start 的 `starting`。
#[tokio::test]
async fn a_failed_start_that_could_not_record_the_exit_does_not_leave_a_zombie() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
    sqlx::query(
        "CREATE TRIGGER fail_run_mapping BEFORE UPDATE OF workspace_id, pane_id, tab_id ON runs
         BEGIN SELECT RAISE(ABORT, 'forced run mapping failure'); END",
    )
    .execute(app.db())
    .await
    .unwrap();
    rs::refuse_run_state(&app, "exited").await;

    let err = start_bot(&app, &bot.id).await.expect_err("mapping 寫不進去");
    assert!(matches!(&err, LcError::Upstream(m) if m.contains("forced run mapping failure")), "要在 mapping 那一步失敗，拿到 {err:?}");
    let run = db::active_run(app.db(), &bot.id).await.unwrap().expect("exited 寫不進去，DB 上還是 starting");
    assert!(env.herdr.methods().iter().any(|m| m == "pane.close"), "pane 照樣收掉");
    assert_eq!(rs::scheduled(&run.id), vec![rs::Settle::Reconcile { stuck: "starting".into() }], "排了重試，不是只吞掉");

    sqlx::query("DROP TRIGGER fail_run_mapping").execute(app.db()).await.unwrap();
    rs::accept_run_state(&app, "exited").await;
    assert!(rs::settle_once(&app, &run.id, &rs::Settle::Reconcile { stuck: "starting".into() }).await);
    assert_eq!(db::run(app.db(), &run.id).await.unwrap().unwrap().state, "exited");
    start_bot(&app, &bot.id).await.expect("zombie 清掉之後可以再啟動");
}

/// 驗收 4：agent 起來之後、`running` 記下之前，不拿 bot 鎖的 pane-exit 事件先把 run 收成 `exited`——
/// 不能把一顆已經結束的 run 拉回 `running`，也不能回啟動成功。
#[tokio::test]
async fn a_start_does_not_bring_back_a_run_another_path_already_ended() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "alfa").await;
    let (app2, bot2) = (app.clone(), bot.id.clone());
    super::super::race_point::arm("start_before_running", &bot.id, move || async move {
        let run = db::active_run(app2.db(), &bot2).await.unwrap().unwrap();
        mark_run_exited(&app2, &run.id, "pane exited").await;
    });

    let err = start_bot(&app, &bot.id).await.expect_err("run 已經結束了，不是啟動成功");
    assert!(matches!(&err, LcError::Upstream(m) if m.contains("ended while it was starting")), "要輸在 running 的 CAS，拿到 {err:?}");
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE bot_id=?").bind(&bot.id).fetch_one(app.db()).await.unwrap();
    assert_eq!(state, "exited", "終態不會被拉回 running");
}
