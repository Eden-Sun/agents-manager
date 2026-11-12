use super::{db, sync_pane_model};
use crate::testing;
use serde_json::json;

const CODEX_DEFAULT: &str = "› Ask Codex to do anything\n\n  GPT-6-Astra default · ~/project/agents-manager · Context 3% used\n  ? for shortcuts\n";
const PANE: &str = "pane-sync-effort";

/// 收編的 codex 子 agent：啟動 argv 是 `high`；run 的 `runtime_effort` 由測試指定（`None`＝未知）。
async fn codex_child(env: &testing::Env, effort: Option<&str>, runtime_effort: Option<&str>) -> (String, String) {
    let bot = testing::claude_bot(&env.app, &env.project_id, "sync-effort").await;
    sqlx::query("UPDATE bots SET kind='codex', managed_by='child', model='gpt-6-astra', effort=? WHERE id=?")
        .bind(effort)
        .bind(&bot.id)
        .execute(&env.app.db)
        .await
        .unwrap();
    let run = testing::fake_run(&env.app, &bot.id).await;
    sqlx::query("UPDATE runs SET pane_id=?, runtime_model='gpt-6-astra', runtime_effort=?, runtime_fast=0 WHERE id=?")
        .bind(PANE)
        .bind(runtime_effort)
        .bind(&run)
        .execute(&env.app.db)
        .await
        .unwrap();
    env.herdr.set_argv(PANE, &["codex", "--model", "gpt-6-astra", "-c", "model_reasoning_effort=\"high\""]);
    (bot.id, run)
}

/// 跟 daemon 對帳同一條路徑：拿 bots 當下的列，用 herdr 的 agent 資訊跑一輪 `sync_pane_model`。
async fn sync_round(env: &testing::Env, bot_id: &str) {
    let bot = db::bot(&env.app.db, bot_id).await.unwrap().unwrap();
    let agent: crate::herdr::AgentInfo = serde_json::from_value(json!({
        "name": "sync-effort", "agent": "codex", "agent_status": "idle",
        "workspace_id": "w", "tab_id": "t", "pane_id": PANE, "cwd": null,
    }))
    .expect("agent info");
    let client = crate::herdr::HerdrClient::new(env.dir.join("data").join("herdr.sock"));
    sync_pane_model(&env.app, crate::config::LOCAL_HOST, &client, &bot, &agent).await;
}

async fn bot_effort(env: &testing::Env, bot_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT effort FROM bots WHERE id=?").bind(bot_id).fetch_one(&env.app.db).await.unwrap()
}

async fn run_effort(env: &testing::Env, run: &str) -> Option<String> {
    db::run(&env.app.db, run).await.unwrap().unwrap().runtime_effort
}

#[tokio::test]
async fn fast_argv_fills_only_unknown_runtime_and_never_refills_after_a_tui_switch() {
    let env = testing::env().await;
    let (bot, run) = codex_child(&env, Some("high"), Some("high")).await;
    env.herdr.set_argv(PANE, &["codex", "--model", "gpt-6-astra", "-c", "service_tier=priority"]);
    sqlx::query("UPDATE runs SET runtime_fast=NULL WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
    sync_round(&env, &bot).await;
    assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1, "未知仍從啟動 argv 補 fast");
    sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
    assert!(crate::codex_live::correct_runtime_from_screen(&env.app, &run, CODEX_DEFAULT).await);
    assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 0);
    // model 設為未知以強迫走到 argv UPDATE；SQL 也必須保護 fast。
    sqlx::query("UPDATE bots SET model=NULL WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
    sync_round(&env, &bot).await;
    sync_round(&env, &bot).await;
    assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 0, "收編窗口內也不可從舊 priority argv 補回");
    assert_eq!(db::run(&env.app.db, &run).await.unwrap().unwrap().runtime_fast, Some(0));
}

/// 兩輪：第一輪畫面切到 default 並清掉子 agent 的 high；之後每一輪對帳都不能把 argv 的 high 補回來。
#[tokio::test]
async fn sync_pane_model_does_not_refill_a_cleared_effort_after_the_screen_shows_default() {
    let env = testing::env().await;
    let (bot, run) = codex_child(&env, Some("high"), Some("high")).await;

    // 第一輪：畫面切到明確 default → 子 agent 設定清成 NULL、run 記 `default`。
    assert!(crate::codex_live::correct_runtime_from_screen(&env.app, &run, CODEX_DEFAULT).await);
    assert_eq!(bot_effort(&env, &bot).await, None);
    assert_eq!(run_effort(&env, &run).await.as_deref(), Some("default"));

    // 第二輪：對帳讀到 argv high，但 run 已有畫面記錄 → 不補。
    sync_round(&env, &bot).await;
    assert_eq!(bot_effort(&env, &bot).await, None, "argv 的舊 high 不能覆寫清掉的 default");

    // 第三輪：畫面一樣、對帳又跑一次 → 仍然不動。
    assert!(!crate::codex_live::correct_runtime_from_screen(&env.app, &run, CODEX_DEFAULT).await);
    sync_round(&env, &bot).await;
    assert_eq!(bot_effort(&env, &bot).await, None);
    assert_eq!(run_effort(&env, &run).await.as_deref(), Some("default"));
}

/// 未知（run 的 runtime_effort 是 NULL）仍照舊從 argv 補 high：收編後還沒讀過畫面的那段。
#[tokio::test]
async fn sync_pane_model_still_fills_an_unknown_effort_from_argv() {
    let env = testing::env().await;
    let (bot, _run) = codex_child(&env, None, None).await;
    sync_round(&env, &bot).await;
    assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"));
}

#[tokio::test]
async fn sync_pane_model_does_not_import_options_from_the_literal_prompt() {
    let env = testing::env().await;
    let (bot_id, _run) = codex_child(&env, None, None).await;
    sqlx::query("UPDATE bots SET model=NULL, fast=0 WHERE id=?").bind(&bot_id).execute(&env.app.db).await.unwrap();
    env.herdr.set_argv(
        PANE,
        &[
            "codex",
            "--model",
            "gpt-6.1-sol",
            "--effort",
            "high",
            "-c",
            "service_tier=\"standard\"",
            "--",
            "--model=haiku",
            "--effort=low",
            "--config=service_tier=priority",
        ],
    );

    sync_round(&env, &bot_id).await;
    let bot = db::bot(&env.app.db, &bot_id).await.unwrap().unwrap();
    assert_eq!(bot.model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(bot.effort.as_deref(), Some("high"));
    assert_eq!(bot.fast, 0, "literal prompt 的 priority 不可將 Fast 寫回 child 設定");
}

/// 設定是 high、畫面是 default（真落差）：對帳不動設定，警示要保留。
#[tokio::test]
async fn sync_pane_model_leaves_a_real_high_versus_default_difference_alone() {
    let env = testing::env().await;
    let (bot, run) = codex_child(&env, Some("high"), Some("default")).await;
    sync_round(&env, &bot).await;
    assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"));
    assert_eq!(run_effort(&env, &run).await.as_deref(), Some("default"));
}
