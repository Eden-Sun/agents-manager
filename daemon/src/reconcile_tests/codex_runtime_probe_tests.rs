use crate::config::LOCAL_HOST;
use crate::db;
use crate::testing as tt;

/// 收編的 codex run：`runtime_*` 三欄是 NULL，pane 認得得到。回傳 (run id, pane id)。
async fn adopted_codex_run(env: &tt::Env) -> (String, String) {
    let bot = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'kid','codex','[]',0,1,'tok',?)",
    )
    .bind(&bot)
    .bind(&env.project_id)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    let run = db::ulid();
    let pane = "w9:pCodex".to_string();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','w9',?,'kid-agent','test',?)",
    )
    .bind(&run)
    .bind(&bot)
    .bind(&pane)
    .bind(db::now())
    .execute(&env.app.db)
    .await
    .unwrap();
    (run, pane)
}

/// #1652：收編的 codex run 讀不出狀態列時，巡邏不能每拍（10 秒）都重讀畫面。
/// 連續讀不出來的間隔倍增（讀完跳過 1、2、4、8…拍，封頂 64 拍）：40 拍內只該讀第 1、3、6、11、20、37 拍。
#[tokio::test]
async fn an_unparseable_codex_status_line_is_reread_with_growing_gaps_not_every_pass() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (_run, pane) = adopted_codex_run(&env).await;
    env.herdr.set_screen(&pane, "codex 啟動中，狀態列還沒畫出來");

    let mut read_on = Vec::new();
    for pass in 1..=40 {
        let before = env.herdr.calls_to("pane.read").len();
        super::fill_codex_runtime(app.as_ref(), LOCAL_HOST, &client).await;
        if env.herdr.calls_to("pane.read").len() > before {
            read_on.push(pass);
        }
    }
    assert_eq!(read_on, vec![1, 3, 6, 11, 20, 37]);
}

/// 退避不能把之後才出現的狀態列擋住：讀得出來後，最多跳過封頂的拍數就會寫進 `runtime_model`。
#[tokio::test]
async fn a_status_line_that_appears_later_is_picked_up_within_the_backoff_cap() {
    let env = tt::env().await;
    let app = env.app.clone();
    let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
    let (run, pane) = adopted_codex_run(&env).await;
    env.herdr.set_screen(&pane, "codex 啟動中");
    for _ in 0..10 {
        super::fill_codex_runtime(app.as_ref(), LOCAL_HOST, &client).await;
    }
    env.herdr.set_screen(&pane, "  gpt-6.1-sol high · /tmp · Context 3% used\n");

    let mut passes = 0;
    let model = loop {
        super::fill_codex_runtime(app.as_ref(), LOCAL_HOST, &client).await;
        passes += 1;
        let model: Option<String> = sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id = ?")
            .bind(&run)
            .fetch_one(&app.db)
            .await
            .unwrap();
        if model.is_some() || passes > 200 {
            break model;
        }
    };
    assert_eq!(model.as_deref(), Some("gpt-6.1-sol"));
    assert!(passes <= 65, "讀得出來後要在封頂的拍數內校正，實際第 {passes} 拍");
}
