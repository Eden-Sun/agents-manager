use super::*;
use crate::capabilities::Db;
use crate::testing as tt;

async fn bot_with_run(env: &tt::Env, managed_by: &str, agent_status: &str) -> (String, String) {
    let app = &env.app;
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
         VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
    )
    .bind(&bot_id)
    .bind(&env.project_id)
    .bind(format!("busy-{bot_id}"))
    .bind(managed_by)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running',?,'pane-busy','busy','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(agent_status)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    (bot_id, run_id)
}

fn reason(e: LcError) -> (String, String) {
    match e {
        LcError::Conflict(v) => (v["reason"].as_str().unwrap_or_default().into(), v["busy"].as_str().unwrap_or_default().into()),
        other => panic!("expected 409, got {other:?}"),
    }
}

/// 重啟時 stop 成功、start 在前置檢查就失敗（這裡是身份在這台主機不存在）：剛停掉的 run 不能留成
/// `stopped`——那代表「使用者要它停」，incident 探針就永遠不替這顆 autostart bot 開 `bot_stopped`
/// （review 2026-09-16 c1 L3）。使用者自己 stop 的照舊是 `stopped`。
#[tokio::test]
async fn a_restart_that_cannot_start_again_is_not_recorded_as_a_user_stop() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, run_id) = bot_with_run(&env, "user", "idle").await;
    sqlx::query("UPDATE bots SET identity='nope-not-on-this-host', autostart=1 WHERE id=?").bind(&bot_id).execute(app.db()).await.unwrap();
    let err = restart_bot(&app, &bot_id).await.expect_err("身份不在這台主機：start 一定失敗");
    assert!(matches!(err, LcError::Conflict(_)), "{err:?}");
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run_id).fetch_one(app.db()).await.unwrap();
    assert_eq!(state, "exited", "重啟沒開回來，不是使用者停的");
    assert!(db::active_run(app.db(), &bot_id).await.unwrap().is_none());

    // 對照：使用者自己 stop 的 run 留在 `stopped`。
    let (other, other_run) = bot_with_run(&env, "user", "idle").await;
    stop_bot(&app, &other).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&other_run).fetch_one(app.db()).await.unwrap();
    assert_eq!(state, "stopped");
}

/// #146 重開 B：重啟停掉了、start 在前置檢查就失敗，舊 run 改記 `exited` 卻寫不進去。以前只看 `Ok(rows>0)`，DB 錯誤
/// 直接吞掉，留下 `stopped`（「使用者要它停」）：autostart 的 bot 從此沒在跑，incident 探針卻永遠不報。現在回 503
/// `restart_state_uncommitted`（帶著 start 的錯）、排 `Relabel` 重試；DB 恢復後成 `exited`，探針報 `bot_stopped`。
#[tokio::test]
async fn a_restart_that_cannot_relabel_its_old_run_says_so_and_retries() {
    use super::super::run_state as rs;
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, run_id) = bot_with_run(&env, "user", "idle").await;
    sqlx::query("UPDATE bots SET identity='nope-not-on-this-host', autostart=1 WHERE id=?").bind(&bot_id).execute(app.db()).await.unwrap();
    rs::refuse_run_state(&app, "exited").await;
    match restart_bot(&app, &bot_id).await {
        Err(LcError::Uncommitted(v)) => {
            assert_eq!((v["error"].as_str(), v["run_id"].as_str()), (Some("restart_state_uncommitted"), Some(run_id.as_str())));
            assert!(v["start_error"].as_str().is_some_and(|s| s.contains("Conflict")), "start 的錯一起帶回：{v}");
        }
        other => panic!("舊 run 還記著使用者要它停，不能只回 start 的錯：{other:?}"),
    }
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run_id).fetch_one(app.db()).await.unwrap();
    assert_eq!(state, "stopped");
    assert_eq!(rs::scheduled(&run_id), vec![rs::Settle::Relabel { from: "stopped", to: "exited" }], "排了改標的重試");
    assert!(!rs::bot_stopped_reported(&app, &bot_id).await, "前提：留著 stopped，探針以為是故意停的");

    rs::accept_run_state(&app, "exited").await;
    assert!(rs::settle_once(&app, &run_id, &rs::Settle::Relabel { from: "stopped", to: "exited" }).await);
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run_id).fetch_one(app.db()).await.unwrap();
    assert_eq!(state, "exited", "重啟沒開回來，不是使用者停的");
    assert!(rs::bot_stopped_reported(&app, &bot_id).await, "autostart 的 bot 沒在跑：探針報出來");
}

/// 排到它的那一刻還閒著、拿到鎖時已經在跑：不送 ctrl+c、run 原封不動。
#[tokio::test]
async fn a_bot_that_got_busy_before_the_lock_is_not_restarted() {
    let env = tt::env().await;
    let app = env.app.clone();
    for (status, want) in [("working", "working"), ("blocked", "blocked")] {
        let (bot_id, run_id) = bot_with_run(&env, "user", status).await;
        let opts = StartOpts { resume_native: true, require_idle: true, ..Default::default() };
        assert_eq!(reason(restart_bot_with(&app, &bot_id, opts).await.unwrap_err()), ("not_idle".into(), want.into()));
        assert_eq!(db::active_run(app.db(), &bot_id).await.unwrap().map(|r| (r.id, r.state)), Some((run_id, "running".into())));
    }
    // 閒著但還有一回合沒收掉，也不動。
    let (bot_id, run_id) = bot_with_run(&env, "user", "idle").await;
    let conv = db::conversation_id(app.db(), &bot_id).await.unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
        .bind(db::ulid())
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
    let opts = StartOpts { resume_native: true, require_idle: true, ..Default::default() };
    assert_eq!(reason(restart_bot_with(&app, &bot_id, opts).await.unwrap_err()).1, "turn_in_flight");
    // 子 agent 的原地重啟也一樣。
    let (kid, _) = bot_with_run(&env, "child", "working").await;
    assert_eq!(reason(restart_child_in_pane_with(&app, &kid, true).await.unwrap_err()).1, "working");
    let methods = env.herdr.methods();
    assert!(!methods.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "什麼都沒動：{methods:?}");
}

/// 子 agent 的原地重啟（一鍵重啟也會走）同一個競態：鎖裡看過閒置之後才開始工作，不能送 ctrl+c。
#[tokio::test]
async fn a_child_that_starts_working_right_before_the_in_pane_restart_is_not_interrupted() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (kid, run_id) = bot_with_run(&env, "child", "idle").await;
    let (a, r) = (app.clone(), run_id.clone());
    crate::lifecycle::race_point::arm("child_restart_before_stopping", &kid, move || async move {
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&r).execute(a.db()).await.unwrap();
    });
    let err = restart_child_in_pane_with(&app, &kid, true).await.expect_err("剛開始工作的子 agent 不能被重啟");
    assert_eq!(reason(err).0, "not_idle");
    assert_eq!(db::active_run(app.db(), &kid).await.unwrap().map(|r| (r.id, r.state)), Some((run_id, "running".into())), "run 原封不動");
    assert!(!env.herdr.methods().iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "沒有 ctrl+c");
}

/// The in-pane restart has its own stop path, so a late hook report must be checked after its
/// idle inspection just like a bulk restart through `stop_locked`.
#[tokio::test]
async fn a_child_background_hook_report_before_stopping_blocks_the_in_pane_restart() {
    let env = tt::env().await;
    let app = env.app.clone();
    let kid = crate::lifecycle::restart_kind_tests::live_child(&env, "late-hook").await;
    let run = db::run(app.db(), &kid.run_id).await.unwrap().unwrap();
    let hook_app = app.clone();
    crate::lifecycle::race_point::arm("child_restart_before_stopping", &kid.id, move || async move {
        crate::runners::background_hook::on_stop(
            &hook_app,
            &run,
            &json!({"background_tasks": [{"id": "agent-1", "type": "subagent", "status": "running", "description": "review"}]}),
        )
        .await;
    });

    let err = restart_child_in_pane_with(&app, &kid.id, true).await.expect_err("new background work blocks the in-pane restart");
    assert_eq!(reason(err).1, "background_jobs");
    assert_eq!(db::active_run(app.db(), &kid.id).await.unwrap().map(|r| (r.id, r.state)), Some((kid.run_id, "running".into())));
    assert!(!env.herdr.methods().iter().any(|m| m == "agent.send_keys" || m == "agent.start"), "background work must not be interrupted");
}

/// 一鍵重啟鎖裡看過閒置、還沒記 `stopping` 的那一瞬，使用者直接在 pane 裡打字：`events::handle_status` 不拿 bot 鎖就寫
/// `agent_status=working`。閒置回收（#144）對這一瞬有「還是 idle 才准停」的許可，重啟停機那一步以前沒有：照樣記 `stopping`、
/// 送 ctrl+c，使用者剛開始的那一回合被砍。要跟那一句寫入互相排序：不閒了就不停，什麼都不動。
#[tokio::test]
async fn a_bot_that_starts_working_right_before_the_restart_stop_is_not_interrupted() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, run_id) = bot_with_run(&env, "user", "idle").await;
    let (a, r) = (app.clone(), run_id.clone());
    crate::lifecycle::race_point::arm("stop_before_stopping", &bot_id, move || async move {
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&r).execute(a.db()).await.unwrap();
    });
    let opts = StartOpts { resume_native: true, require_idle: true, ..Default::default() };
    let err = restart_bot_with(&app, &bot_id, opts).await.expect_err("剛開始工作的 bot 不能被重啟");
    assert_eq!(reason(err).0, "not_idle");
    assert_eq!(db::active_run(app.db(), &bot_id).await.unwrap().map(|r| (r.id, r.state)), Some((run_id, "running".into())), "run 原封不動");
    let methods = env.herdr.methods();
    assert!(!methods.iter().any(|m| m == "agent.send_keys" || m == "pane.close"), "沒有 ctrl+c、沒有關 pane：{methods:?}");
}
