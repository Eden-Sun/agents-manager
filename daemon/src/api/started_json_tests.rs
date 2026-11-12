//! issue #107：`?resume=native` 的回應要照接回的**結論**說，不是看一個 SessionStart 一到就被清掉的暫存欄位。
use super::*;
use crate::testing as tt;

async fn resumed_run(app: &Arc<App>, project_id: &str, name: &str, resume: Option<&str>) -> (String, String) {
    let bot = tt::claude_bot(app, project_id, name).await;
    let run = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at, resume_session_id)
             VALUES (?,?,'running','idle','ws-1',?,'agent','test',?,?)",
    )
    .bind(&run)
    .bind(&bot.id)
    .bind(format!("pane-{}", bot.id))
    .bind(db::now())
    .bind(resume)
    .execute(&app.db)
    .await
    .unwrap();
    (bot.id, run)
}

async fn session_start(app: &Arc<App>, bot_id: &str, run_id: &str, session: &str) {
    crate::hookrecv::process(
        app,
        &crate::hookrecv::HookBody {
            bot_id: bot_id.to_string(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "SessionStart", "session_id": session, "source": "resume"}),
            received_at: None,
            truncated: false,
            run_id: Some(run_id.to_string()),
        },
    )
    .await
    .unwrap();
}

fn summary(v: &Value) -> (Value, Value, Value) {
    (v["resumed"].clone(), v["session_id"].clone(), v["resume_outcome"].clone())
}

#[tokio::test]
async fn resumed_reports_the_recorded_outcome_not_the_marker_session_start_clears() {
    let e = tt::env().await;
    let app = e.app.clone();
    let opts = lifecycle::StartOpts { resume_native: true, resume_required: true, ..Default::default() };

    // 帶了 --resume、還在等回報。
    let (bot, run) = resumed_run(&app, &e.project_id, "pending", Some("s-1")).await;
    assert_eq!(summary(&started_json(&app, &run, &opts).await.unwrap()), (json!(true), json!("s-1"), Value::Null));
    // SessionStart 在回應組好之前就到了：暫存欄位清掉，但接回了就是接回了。
    session_start(&app, &bot, &run, "s-1").await;
    let marker: Option<String> = sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
    assert_eq!(marker, None, "前提：暫存欄位真的被清掉了");
    assert_eq!(summary(&started_json(&app, &run, &opts).await.unwrap()), (json!(true), json!("s-1"), json!("verified")));

    // CLI 開了新對話：不是接回。
    let (bot, run) = resumed_run(&app, &e.project_id, "mismatch", Some("s-2")).await;
    session_start(&app, &bot, &run, "s-brand-new").await;
    assert_eq!(summary(&started_json(&app, &run, &opts).await.unwrap()), (json!(false), Value::Null, json!("mismatch")));

    // 這次根本沒帶 --resume（接不回、退回開新對話）。
    let (_bot, run) = resumed_run(&app, &e.project_id, "fresh", None).await;
    assert_eq!(summary(&started_json(&app, &run, &opts).await.unwrap()), (json!(false), Value::Null, Value::Null));

    // 沒要求 resume：只回 run_id，跟以前一樣。
    assert_eq!(started_json(&app, &run, &lifecycle::StartOpts::default()).await.unwrap(), json!({"run_id": run}));
}

/// 按「啟動」叫醒睡著的 bot：接不回原對話時 `wake` 會退回開新對話，回應不能照樣說 `resumed:true`。
#[tokio::test]
async fn waking_a_sleeping_bot_that_could_not_resume_says_so() {
    let e = tt::env().await;
    let app = e.app.clone();
    let bot = tt::claude_bot(&app, &e.project_id, "sleeper").await;
    // 睡著了，但沒有任何一段對話可以接（沒有 native session）。
    sqlx::query("INSERT INTO bot_sleeps (bot_id, native_session_id, idle_minutes, reason, slept_at) VALUES (?,NULL,90,'idle',?)")
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let resp = start_bot(State(app.clone()), Path(bot.id.clone()), Query(StartQuery { resume: None, session: None })).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
    let run = db::active_run(&app.db, &bot.id).await.unwrap().expect("叫醒了");
    assert_eq!(body["run_id"], json!(run.id));
    assert_eq!(body["resumed"], json!(false), "開的是新對話：{body}");
    lifecycle::stop_bot(&app, &bot.id).await.unwrap();
}

/// 睡著的 bot 也一樣：參數不合法就在叫醒之前 400，睡眠列與 run 都不能被動到。
#[tokio::test]
async fn starting_a_sleeping_bot_with_bad_resume_params_is_refused_before_waking() {
    let e = tt::env().await;
    let app = e.app.clone();
    let bot = tt::claude_bot(&app, &e.project_id, "sleeper2").await;
    sqlx::query("INSERT INTO bot_sleeps (bot_id, native_session_id, idle_minutes, reason, slept_at) VALUES (?,NULL,90,'idle',?)")
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let bad = [
        StartQuery { resume: Some("bogus".into()), session: None },
        StartQuery { resume: None, session: Some("abc".into()) },
        StartQuery { resume: Some("native".into()), session: Some("a b".into()) },
    ];
    for q in bad {
        let err = start_bot(State(app.clone()), Path(bot.id.clone()), Query(q)).await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "{err:?}");
    }
    assert!(db::active_run(&app.db, &bot.id).await.unwrap().is_none(), "不該被叫醒");
    let sleeps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bot_sleeps WHERE bot_id = ?").bind(&bot.id).fetch_one(&app.db).await.unwrap();
    assert_eq!(sleeps, 1);
}
