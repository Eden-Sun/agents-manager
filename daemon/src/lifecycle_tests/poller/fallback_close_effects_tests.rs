//! 備援收尾（`try_fallback`、`capture_hookless_turn_locked`）也做 hook 收尾的副作用（`turn_close`）：
//! 收掉 `PreCompact` 的壓縮中標記、依回覆標／清「要登入」、codex 原生回覆清撞限、外部回合的字標 `relay_from`。
use crate::lifecycle_tests::prelude::*;
use crate::lifecycle::*;
use crate::lifecycle::poller::test_seams::*;

use crate::testing as tt;

const HOST: &str = crate::config::LOCAL_HOST;

fn claude_screen(reply: &str) -> String {
    format!("❯ 派工\n⏺ {reply}\n\n✻ Worked for 3s\n────────────────\n❯ \n────────────────\n")
}

async fn in_flight(app: &Arc<App>, bot: &db::Bot) -> (String, String) {
    let run = tt::fake_run(app, &bot.id).await;
    let turn = crate::lifecycle::run_state::a_turn(app, &bot.id, Some(&run), "in_flight").await;
    (run, turn)
}

async fn with_identity(app: &Arc<App>, kind: &str, name: &str) {
    let info = crate::tools::IdentityInfo {
        name: name.into(),
        kind: kind.into(),
        logged_in: Some(true),
        reason: None,
        account: Some("fake@example.test".into()),
        plan: None,
        source: crate::tools::SOURCE_CONFIG,
        config_dir: None,
        checked_at: None,
        basis: None,
    };
    app.tools.lock().await.insert(
        HOST.to_string(),
        crate::tools::HostTools {
            tools: Default::default(),
            identities: [(name.to_string(), info)].into_iter().collect(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: db::now(),
        },
    );
}

#[tokio::test]
async fn a_fallback_close_ends_the_compacting_mark_the_hook_opened() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "compactor").await;
    let (run, turn) = in_flight(&app, &bot).await;
    am_base::compacting::begin(&run, chrono::Utc::now());
    env.herdr.set_screen(&format!("pane-{}", bot.id), &claude_screen("壓縮完繼續做完了"));

    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    assert_eq!(am_base::compacting::since_at(&run, chrono::Utc::now()), None, "PreCompact 開的標記跟著備援收掉");
}

#[tokio::test]
async fn a_fallback_reply_marks_and_then_clears_login_needed() {
    let env = tt::env().await;
    let app = env.app.clone();
    with_identity(&app, "claude", "cc1").await;
    let bot = tt::claude_bot(&app, &env.project_id, "logout-fb").await;
    sqlx::query("UPDATE bots SET identity='cc1' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let pane = format!("pane-{}", bot.id);

    let (run, turn) = in_flight(&app, &bot).await;
    env.herdr.set_screen(&pane, &claude_screen("Not logged in · Please run /login"));
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    assert!(crate::login_prompt::get(&app, HOST, "cc1").is_some(), "備援收到的授權失敗一樣標要登入");

    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    env.herdr.set_screen(&pane, &claude_screen("登入之後正常答完"));
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    assert!(crate::login_prompt::get(&app, HOST, "cc1").is_none(), "正常答完清掉標記");
}

async fn codex_with_rollout(env: &tt::Env, answer: &str) -> (db::Bot, String, String) {
    let app = env.app.clone();
    let session = "01a18888-0000-7000-8000-000000002233";
    let bot = tt::claude_bot(&app, &env.project_id, "cx-limit").await;
    let home = app.data_dir.join("fallback-codex-home");
    let dir = home.join("sessions/2026/11/27");
    std::fs::create_dir_all(&dir).unwrap();
    let log = format!(
        "{}\n{}\n",
        json!({"type":"response_item","timestamp":"2026-11-27T01:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"派工"}]}}),
        json!({"type":"event_msg","timestamp":"2026-11-27T01:00:05Z","payload":{"type":"task_complete","last_agent_message":answer}}),
    );
    std::fs::write(dir.join(format!("rollout-2026-11-27T01-00-00-{session}.jsonl")), log).unwrap();
    sqlx::query("UPDATE bots SET kind='codex', env_json=? WHERE id=?")
        .bind(json!({"CODEX_HOME": home.to_string_lossy()}).to_string()).bind(&bot.id).execute(&app.db).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    let (run, turn) = in_flight(&app, &bot).await;
    sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?").bind(session).bind(&run).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE turns SET created_at='2026-11-27T00:59:50Z', delivered_at='2026-11-27T00:59:50Z' WHERE id=?")
        .bind(&turn).execute(&app.db).await.unwrap();
    env.herdr.set_screen(&format!("pane-{}", bot.id), &format!("› 派工\n• {answer}\n› "));
    (bot, run, turn)
}

#[tokio::test]
async fn a_codex_rollout_reply_from_the_fallback_clears_the_limit_hit() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot, run, turn) = codex_with_rollout(&env, "撞限過後正常答完").await;
    let until = db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2));
    assert!(crate::quota::seed_limit_hit(&app, HOST, "codex", &until, "You've hit your usage limit", None).await);
    assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_some());

    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let source: String = sqlx::query_scalar("SELECT source FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&app.db).await.unwrap();
    assert_eq!(source, "transcript");
    assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "rollout 的原生回覆＝帳號又能跑了");
}

#[tokio::test]
async fn a_scraped_codex_reply_does_not_clear_the_limit_hit() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cx-scraped").await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
    let (run, turn) = in_flight(&app, &bot).await;
    env.herdr.set_screen(&format!("pane-{}", bot.id), "› 派工\n• 畫面上的回覆\n› ");
    let until = db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2));
    assert!(crate::quota::seed_limit_hit(&app, HOST, "codex", &until, "You've hit your usage limit", None).await);

    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "畫面刮來的可能是舊畫面：不清");
}

#[tokio::test]
async fn a_hookless_capture_marks_the_echo_another_bot_typed() {
    let env = tt::env().await;
    let app = env.app.clone();
    let from = tt::claude_bot(&app, &env.project_id, "relay-sender").await;
    let bot = tt::claude_bot(&app, &env.project_id, "relay-hookless").await;
    sqlx::query("UPDATE bots SET inject_hooks=0, kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let run = tt::fake_run(&app, &bot.id).await;
    sqlx::query("UPDATE runs SET adopted=1 WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
    let text = "別的 bot 打進來的派工";
    env.herdr.set_screen(&format!("pane-{}", bot.id), &format!("› {text}\n• 收到，做完了\n› "));
    crate::agent_relay::announce(HOST, &from.id, &run, text, Some("from-turn-1"));

    assert!(capture_hookless_turn_locked(&app, &run, false).await.unwrap());
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let row: (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT content, relay_from, relay_turn_id FROM messages WHERE conversation_id=? AND role='user'")
            .bind(&conv).fetch_one(&app.db).await.unwrap();
    assert_eq!(row, (text.to_string(), Some(from.id.clone()), Some("from-turn-1".to_string())));
}

#[tokio::test]
async fn an_external_turn_opened_from_the_pane_marks_the_relayed_echo() {
    let env = tt::env().await;
    let app = env.app.clone();
    let from = tt::claude_bot(&app, &env.project_id, "relay-sender-2").await;
    let bot = tt::claude_bot(&app, &env.project_id, "relay-target-2").await;
    let run_id = tt::fake_run(&app, &bot.id).await;
    let text = "AGM 直接打進 pane 的一句";
    env.herdr.set_screen(&format!("pane-{}", bot.id), &format!("❯ {text}\n\n✻ Thinking…\n"));
    crate::agent_relay::announce(HOST, &from.id, &run_id, text, None);

    let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
    crate::lifecycle::poller::begin_external_turn(&app, &run).await;
    let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
    let row: (String, Option<String>) = sqlx::query_as("SELECT content, relay_from FROM messages WHERE conversation_id=? AND role='user'")
        .bind(&conv).fetch_one(&app.db).await.unwrap();
    assert_eq!(row, (text.to_string(), Some(from.id.clone())));
}
