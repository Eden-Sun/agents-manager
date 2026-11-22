use crate::lifecycle_tests::prelude::*;
use crate::lifecycle::*;
use crate::lifecycle::poller::*;
use crate::lifecycle::poller::test_seams::*;
use crate::lifecycle::run_state::{a_turn, turn_status};
use crate::testing as tt;
use serde_json::json;

const RULE: &str = "────────────────────────────────────────────────────────";
fn idle_screen() -> String {
    format!("● 做完了 ALPHA\n{RULE}\n>\n{RULE}\n  Gemini 3.8 Flash · high\n")
}

struct F {
    env: tt::Env,
    bot_id: String,
    run_id: String,
}

async fn fixture() -> F {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "agy-activity").await;
    sqlx::query("UPDATE bots SET kind='agy' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    let run_id = tt::fake_run(&env.app, &bot.id).await;
    env.herdr.set_screen(&format!("pane-{}", bot.id), &idle_screen());
    F { env, bot_id: bot.id, run_id }
}

async fn hook_at(f: &F, at: chrono::DateTime<chrono::Utc>) {
    sqlx::query("INSERT INTO hook_events (id, bot_id, provider, source, body_json, received_at) VALUES (?,?,'agy','http','{}',?)")
        .bind(db::ulid())
        .bind(&f.bot_id)
        .bind(at.to_rfc3339())
        .execute(&f.env.app.db)
        .await
        .unwrap();
}

fn rec(i: u64, ty: &str, at: chrono::DateTime<chrono::Utc>, body: serde_json::Value) -> String {
    let mut v = json!({"step_index": i, "source": if ty == "USER_INPUT" { "USER_EXPLICIT" } else { "MODEL" }, "type": ty, "status": "DONE", "created_at": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)});
    for (k, val) in body.as_object().unwrap() {
        v[k] = val.clone();
    }
    v.to_string()
}

fn user(i: u64, at: chrono::DateTime<chrono::Utc>, text: &str) -> String {
    rec(i, "USER_INPUT", at, json!({"content": format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>")}))
}

fn tool_step(i: u64, at: chrono::DateTime<chrono::Utc>) -> String {
    rec(i, "PLANNER_RESPONSE", at, json!({"tool_calls": [{"name": "run_command", "args": {"CommandLine": "git status", "toolSummary": "status"}}], "input_tokens": 100, "cache_read_tokens": 900}))
}

fn answer(i: u64, at: chrono::DateTime<chrono::Utc>, text: &str) -> String {
    rec(i, "PLANNER_RESPONSE", at, json!({"content": text, "input_tokens": 100, "cache_read_tokens": 900}))
}

async fn bind_transcript(f: &F, transcript: &str) -> std::path::PathBuf {
    let sid = format!("c-{}", db::ulid());
    let file = crate::agy_support::transcript_path(&crate::home::dir().unwrap(), &sid);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, transcript).unwrap();
    sqlx::query("UPDATE runs SET native_session_id = ?, transcript_path = ? WHERE id = ?")
        .bind(&sid)
        .bind(file.to_str().unwrap())
        .bind(&f.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    file
}

#[tokio::test]
async fn recent_agy_hooks_keep_the_fallback_from_closing_the_turn_and_a_long_silence_lets_it() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;

    hook_at(&f, chrono::Utc::now() - chrono::Duration::seconds(30)).await;
    assert!(!try_fallback(&app, &f.run_id, Some(&a)).await.unwrap(), "30 秒前才有 hook：agy 還在做事，不收");
    assert_eq!(turn_status(&app, &a).await, "in_flight");

    sqlx::query("DELETE FROM hook_events").execute(&app.db).await.unwrap();
    hook_at(&f, chrono::Utc::now() - chrono::Duration::seconds(4 * 60)).await;
    assert!(try_fallback(&app, &f.run_id, Some(&a)).await.unwrap(), "4 分鐘沒有任何 hook：才動用終端備援");
    assert_eq!(turn_status(&app, &a).await, "completed_fallback");
}

#[tokio::test]
async fn the_fallback_closes_an_agy_turn_with_the_transcript_reply_not_the_screen() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    let now = chrono::Utc::now();
    let text = [user(0, now, "派工"), tool_step(1, now), answer(2, now, "最後的答案 ALPHA")].join("\n");
    let file = bind_transcript(&f, &text).await;
    hook_at(&f, now).await;

    assert!(try_fallback(&app, &f.run_id, Some(&a)).await.unwrap());
    assert_eq!(turn_status(&app, &a).await, "completed");
    let (content, source, incomplete): (String, String, i64) =
        sqlx::query_as("SELECT content, source, incomplete FROM messages WHERE turn_id = ? AND role = 'assistant'")
            .bind(&a)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!((content.as_str(), source.as_str(), incomplete), ("最後的答案 ALPHA", "transcript", 0));
    std::fs::remove_file(file).unwrap();
}

#[tokio::test]
async fn after_a_restart_with_no_hook_memory_a_working_transcript_still_holds_and_a_finished_one_closes() {
    let f = fixture().await;
    let app = f.env.app.clone();
    let a = a_turn(&app, &f.bot_id, Some(&f.run_id), "in_flight").await;
    let now = chrono::Utc::now();
    let working = [user(0, now, "派工"), answer(1, now, "我先看一下"), tool_step(2, now)].join("\n");
    let file = bind_transcript(&f, &working).await;

    assert!(!try_fallback(&app, &f.run_id, Some(&a)).await.unwrap(), "transcript 最後一步還是工具呼叫：不收");
    assert_eq!(turn_status(&app, &a).await, "in_flight");

    let finished = format!("{working}\n{}\n{}", rec(3, "GENERIC", now, json!({"content": "tool output"})), answer(4, now, "做完了"));
    std::fs::write(&file, finished).unwrap();
    assert!(try_fallback(&app, &f.run_id, Some(&a)).await.unwrap());
    assert_eq!(turn_status(&app, &a).await, "completed");
    let source: String = sqlx::query_scalar("SELECT source FROM messages WHERE turn_id = ? AND role = 'assistant'").bind(&a).fetch_one(&app.db).await.unwrap();
    assert_eq!(source, "transcript");
    std::fs::remove_file(file).unwrap();
}

#[test]
fn the_transcript_verdict_needs_a_matching_prompt_and_a_fresh_last_record() {
    let now = chrono::Utc::now();
    let old = now - chrono::Duration::minutes(10);
    let sent = vec!["go on".to_string()];

    let done = [user(0, now, "go on"), tool_step(1, now), answer(2, now, "ok")].join("\n");
    assert_eq!(agy_native_from_transcript(&done, &sent, now), AgyNative::Reply("ok".into()));
    assert_eq!(agy_native_from_transcript(&done, &[], now), AgyNative::Reply("ok".into()), "外部回合沒有送出的文字也能對上");

    let other = [user(0, now, "something else"), answer(1, now, "ok")].join("\n");
    assert_eq!(agy_native_from_transcript(&other, &sent, now), AgyNative::Unknown, "最後一問不是這一回合的 prompt");

    let working = [user(0, now, "go on"), tool_step(1, now)].join("\n");
    assert_eq!(agy_native_from_transcript(&working, &sent, now), AgyNative::Ongoing);

    let stale = [user(0, old, "go on"), tool_step(1, old)].join("\n");
    assert_eq!(agy_native_from_transcript(&stale, &sent, now), AgyNative::Unknown, "很久沒動的未完成回合不擋備援");

    assert_eq!(agy_native_from_transcript("", &sent, now), AgyNative::Unknown);
}
