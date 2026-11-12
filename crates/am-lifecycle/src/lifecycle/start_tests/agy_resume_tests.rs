use super::*;
use crate::capabilities::Db;
use crate::testing::{claude_bot, env, Env};

const SID: &str = "5e69519d-5998-496d-aa7a-b7df4383520a";

fn last_args(e: &Env) -> Vec<String> {
    let calls = e.herdr.calls_to("agent.start");
    calls.last().unwrap()["args"].as_array().unwrap().iter().filter_map(|a| a.as_str().map(String::from)).collect()
}

async fn system_notes(app: &Arc<App>, bot_id: &str) -> Vec<String> {
    let conv = db::conversation_id(app.db(), bot_id).await.unwrap();
    sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at")
        .bind(&conv)
        .fetch_all(app.db())
        .await
        .unwrap()
}

async fn ended_run(app: &Arc<App>, bot_id: &str, native: Option<&str>) {
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at) VALUES (?,?,'stopped','idle',?,?,?)",
    )
    .bind(db::ulid())
    .bind(bot_id)
    .bind(native)
    .bind("2026-10-05T00:00:00Z")
    .bind("2026-10-05T00:01:00Z")
    .execute(app.db())
    .await
    .unwrap();
}

#[tokio::test]
async fn a_resume_native_start_reopens_the_recorded_conversation_and_never_uses_dash_c() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "agy-resume").await;
    sqlx::query("UPDATE bots SET kind='agy', model='gemini-3.8-flash-low', auto_approve=1 WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    ended_run(&e.app, &bot.id, Some(SID)).await;

    start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();

    let args = last_args(&e);
    assert!(args.contains(&format!("--conversation={SID}")), "{args:?}");
    assert!(!args.iter().any(|a| a == "-c" || a == "--continue"), "-c 只認 cwd 最近一段，同 cwd 的 bot 會互搶：{args:?}");
    let requested: Option<String> = sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE bot_id=? AND state IN ('starting','running') ")
        .bind(&bot.id)
        .fetch_one(e.app.db())
        .await
        .unwrap();
    assert_eq!(requested.as_deref(), Some(SID), "要接的 id 記進 run，hook 回報的才有東西可對");
}

#[tokio::test]
async fn without_a_recorded_conversation_agy_starts_fresh_with_a_visible_note() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "agy-fresh").await;
    sqlx::query("UPDATE bots SET kind='agy', model='gemini-3.8-flash-low', auto_approve=1 WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    ended_run(&e.app, &bot.id, None).await;

    start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();

    let args = last_args(&e);
    assert!(!args.iter().any(|a| a == "-c" || a == "--continue" || a.starts_with("--conversation")), "沒有 id 就開新對話，不退到 -c：{args:?}");
    assert!(system_notes(&e.app, &bot.id).await.iter().any(|n| n.contains("接不回")));
}

/// #1001：全新 bot 第一次啟動（沒有 run、對話裡沒有 user／assistant 訊息）沒有「原本的對話」可接，不是接丟了，不寫警告。
#[tokio::test]
async fn a_brand_new_bot_first_start_does_not_warn_context_lost() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "rpa-new").await;

    start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();

    assert!(!system_notes(&e.app, &bot.id).await.iter().any(|n| n.contains(CONTEXT_LOST_PREFIX)), "全新 bot 第一次啟動不該說接不回");
    let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM context_lost_notices WHERE bot_id = ?").bind(&bot.id).fetch_one(e.app.db()).await.unwrap();
    assert_eq!(notices, 0);
}

/// #1001：有過 run 但沒記到 session id，仍然要警告。
#[tokio::test]
async fn a_bot_with_history_but_no_session_still_warns() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "rpa-old").await;
    ended_run(&e.app, &bot.id, None).await;

    start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();

    assert!(system_notes(&e.app, &bot.id).await.iter().any(|n| n.contains(CONTEXT_LOST_PREFIX)), "有過 run 的 bot 接不回就要說");
}

/// #1001：沒有 run 但對話裡已經有使用者的訊息（排在第一次啟動之前），也算有過對話，仍然要警告。
#[tokio::test]
async fn a_bot_that_already_spoke_but_never_ran_still_warns() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "rpa-spoke").await;
    let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'user','hi','web',?)")
        .bind(db::ulid())
        .bind(&conv)
        .bind(db::now())
        .execute(e.app.db())
        .await
        .unwrap();

    start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();

    assert!(system_notes(&e.app, &bot.id).await.iter().any(|n| n.contains(CONTEXT_LOST_PREFIX)), "已有使用者訊息的對話接不回就要說");
}

/// 在原 pane 裡重啟的 agy 子 agent：接得回就帶 `--conversation=<id>`；記到的 id 形狀不對則開新對話、聊天室留說明（不只 tracing）。
async fn restart_agy_child(recorded: &str) -> (Env, String, Vec<String>) {
    let e = env().await;
    let app = e.app.clone();
    let client = crate::herdr::HerdrClient::new(e.dir.join("data/herdr.sock"));
    let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
    let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
    let parent = claude_bot(&app, &e.project_id, "alfa").await;
    let kid = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, model, args_json, autostart, inject_hooks, hook_token, auto_approve, managed_by, parent_bot_id, created_at)
         VALUES (?,?,'ui','agy','gemini-3.8-flash-low','[]',0,0,'tok',1,'child',?,?)",
    )
    .bind(&kid)
    .bind(&e.project_id)
    .bind(&parent.id)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    ended_run(&app, &kid, Some(recorded)).await;
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
         VALUES (?,?,'running','idle',?,?,?,'proj-alfa-ui','test',1,?)",
    )
    .bind(db::ulid())
    .bind(&kid)
    .bind(&ws.workspace_id)
    .bind(&kid_pane.tab_id)
    .bind(&kid_pane.pane_id)
    .bind(db::now())
    .execute(app.db())
    .await
    .unwrap();
    *e.herdr.agents.lock().unwrap() = vec![json!({
        "name": "proj-alfa-ui", "agent": "agy", "agent_status": "idle",
        "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id, "cwd": "/tmp/p"})];
    restart_child_in_pane(&app, &kid).await.unwrap();
    let args = last_args(&e);
    (e, kid, args)
}

#[tokio::test]
async fn an_agy_child_restarted_in_its_pane_reopens_its_conversation() {
    let (e, kid, args) = restart_agy_child(SID).await;
    assert!(args.contains(&format!("--conversation={SID}")), "{args:?}");
    assert!(!args.iter().any(|a| a == "-c"), "{args:?}");
    assert!(system_notes(&e.app, &kid).await.is_empty(), "接回了就沒有說明");
}

#[tokio::test]
async fn an_agy_child_with_an_unusable_recorded_id_starts_fresh_and_says_so() {
    let (e, kid, args) = restart_agy_child("not a valid id; rm -rf").await;
    assert!(!args.iter().any(|a| a == "-c" || a.starts_with("--conversation")), "{args:?}");
    assert!(system_notes(&e.app, &kid).await.iter().any(|n| n.contains("接不回")), "{:?}", system_notes(&e.app, &kid).await);
}
