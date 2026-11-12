//! #207：codex 0.155.1 真畫面（見 `screen.rs` 的 `codex_0155_screen_tests`）。
use super::*;
use crate::testing as tt;

const WORKING: &str = include_str!("fixtures/codex-0.155-working.txt");
const WORKING_SUMMARY: &str = include_str!("fixtures/codex-0.155-working-summary.txt");
const FINISHED: &str = include_str!("fixtures/codex-0.155-finished.txt");

async fn codex_turn(screen: &str, prompt: &str) -> (tt::Env, String, String) {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cx").await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let run = tt::fake_run(&app, &bot.id).await;
    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    sqlx::query("UPDATE turns SET prompt_text=? WHERE id=?").bind(prompt).bind(&turn).execute(&app.db).await.unwrap();
    env.herdr.set_screen(&format!("pane-{}", bot.id), screen);
    (env, run, turn)
}

// Synthetic fixture based on issue #1878's DB suffix, not a captured pane.
const SYNTHETIC_COMPACT: &str = "• 上一則已由 hook 落過的回覆。\n\n  Worked for 21s • 11:03 AM\n\n• Context compacted · 1m 14s\n\n  Worked for 1m 14s • 11:57 AM\n\n› \n";

#[tokio::test]
async fn synthetic_codex_manual_and_external_compact_store_only_a_system_note() {
    for (origin, prompt) in [("external", "/compact"), ("web", "/compact"), ("external", "/compact 保留問題脈絡")] {
        let (env, run, turn) = codex_turn(SYNTHETIC_COMPACT, prompt).await;
        let app = env.app.clone();
        let bot: String = sqlx::query_scalar("SELECT bot_id FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        let conv = db::conversation_id(&app.db, &bot).await.unwrap();
        sqlx::query("UPDATE turns SET origin=? WHERE id=?").bind(origin).bind(&turn).execute(&app.db).await.unwrap();
        insert_message(&app, &conv, None, "assistant", "上一則已由 hook 落過的回覆。", "hook", false, None).await.unwrap();

        assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
        assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "completed");
        let messages: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT role,content,source,incomplete FROM messages WHERE turn_id=?",
        ).bind(&turn).fetch_all(&app.db).await.unwrap();
        assert_eq!(messages, vec![("system".into(), "Codex 已壓縮 · 1m 14s".into(), "system".into(), 0)]);

        // A repeated external edge with a mismatched cursor must not replay the
        // old hook reply or duplicate the compaction note.
        let again = crate::lifecycle::run_state::a_turn(&app, &bot, Some(&run), "in_flight").await;
        sqlx::query("UPDATE turns SET origin='external',prompt_text='/compact' WHERE id=?").bind(&again).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET last_read_tail_hash='stale' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        assert!(try_fallback(&app, &run, Some(&again)).await.unwrap());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?").bind(&conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(count, 2);
    }
}

#[tokio::test]
async fn synthetic_codex_compact_uses_delivered_input_instead_of_routing_mentions() {
    let (env, run, turn) = codex_turn(SYNTHETIC_COMPACT, "/compact").await;
    let app = env.app.clone();
    let bot: String = sqlx::query_scalar("SELECT bot_id FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
    let conv = db::conversation_id(&app.db, &bot).await.unwrap();
    insert_message(&app, &conv, Some(&turn), "user", "@cx /compact", "web", false, None).await.unwrap();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "completed");
    let messages: Vec<(String,String)> = sqlx::query_as("SELECT role,content FROM messages WHERE turn_id=? AND role<>'user'")
        .bind(&turn).fetch_all(&app.db).await.unwrap();
    assert_eq!(messages, vec![("system".into(), "Codex 已壓縮 · 1m 14s".into())]);
}

#[tokio::test]
async fn synthetic_codex_compaction_words_are_stored_and_complete_normal_turns() {
    for echo in ["", "› Reply with the requested text\n"] {
        for answer in ["Context compacted", "Context compacted successfully; here is the summary.", "Context compacted · 3s"] {
            let screen = format!(
                "• Previous answer\nWorked for 21s • 11:03 AM\n{echo}• {answer}\nWorked for 3s • 11:58 AM\n› \n"
            );
            let (env, run, turn) = codex_turn(&screen, "Reply with the requested text").await;
            let app = env.app.clone();
            assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap(), "{answer}");
            assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "completed_fallback");
            let messages: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT role,content,source FROM messages WHERE turn_id=?",
            ).bind(&turn).fetch_all(&app.db).await.unwrap();
            assert_eq!(messages, vec![("assistant".into(), answer.into(), "terminal_fallback".into())]);
        }
    }
}

#[tokio::test]
async fn synthetic_codex_warm_compact_keeps_the_existing_system_note() {
    let (env, run, turn) = codex_turn(SYNTHETIC_COMPACT, "/compact").await;
    let app = env.app.clone();
    let bot: String = sqlx::query_scalar("SELECT bot_id FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
    let conv = db::conversation_id(&app.db, &bot).await.unwrap();
    sqlx::query("UPDATE turns SET origin='external' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
    assert!(am_base::primary_keep_warm::claim_warm_compact(&app.db, &bot, &db::now(), &run, 50).await.unwrap());
    insert_message(&app, &conv, None, "system", "主力熱壓", "system", false, None).await.unwrap();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let messages: Vec<(String, String)> = sqlx::query_as("SELECT role,content FROM messages WHERE conversation_id=?")
        .bind(&conv).fetch_all(&app.db).await.unwrap();
    assert_eq!(messages, vec![("system".into(), "主力熱壓".into())]);
}

#[tokio::test]
async fn synthetic_codex_auto_compact_without_an_answer_waits_then_records_one_reply() {
    let (env, run, turn) = codex_turn(SYNTHETIC_COMPACT, "繼續修正").await;
    let app = env.app.clone();
    let bot: String = sqlx::query_scalar("SELECT bot_id FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
    sqlx::query("UPDATE turns SET delivered_at=created_at WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
    let since: String = sqlx::query_scalar("SELECT delivered_at FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
    let home = env.dir.join("synthetic-codex-home");
    let day = home.join("sessions/2026/10/20");
    std::fs::create_dir_all(&day).unwrap();
    let log = day.join("rollout-2026-10-20T11-56-00-synthetic-compact.jsonl");
    let event = json!({"timestamp": since, "type":"compacted", "payload":{"replacement_history":[{"role":"assistant","content":"old answer"}]}}).to_string();
    std::fs::write(&log, format!("{event}\n")).unwrap();
    sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
        .bind(json!({"CODEX_HOME":home.to_str().unwrap()}).to_string()).bind(&bot).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE runs SET native_session_id='synthetic-compact' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();

    for _ in 0..2 {
        assert!(!try_fallback(&app, &run, Some(&turn)).await.unwrap());
        assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "in_flight");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(count, 0, "no chrome bubble or system message");
    }
    let screen = SYNTHETIC_COMPACT.replace("› \n", "• 修好了。\nWorked for 8s • 11:58 AM\n› \n");
    env.herdr.set_screen(&format!("pane-{bot}"), &screen);
    // The pane may show the answer before rollout appends the assistant item.
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    assert!(!try_fallback(&app, &run, Some(&turn)).await.unwrap());
    assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "completed_fallback");
    let messages: Vec<(String,String)> = sqlx::query_as("SELECT role,content FROM messages WHERE turn_id=?")
        .bind(&turn).fetch_all(&app.db).await.unwrap();
    assert_eq!(messages, vec![("assistant".into(), "修好了。".into())]);
}

#[tokio::test]
async fn synthetic_queued_codex_turn_does_not_adopt_compaction_before_delivery() {
    for delivered_at in [Some("2026-10-20T11:20:00Z"), None, Some("invalid")] {
        let screen = "Worked for 21s • 11:03 AM\n• Context compacted · 3s\nWorked for 3s • 11:58 AM\n› \n";
        let (env, run, turn) = codex_turn(screen, "Reply with the requested text").await;
        let app = env.app.clone();
        let bot: String = sqlx::query_scalar("SELECT bot_id FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        // Queued at 11:00; the preceding turn compacts at 11:10; this input
        // is delivered only at 11:20. There is no later assistant in rollout.
        sqlx::query("UPDATE turns SET created_at='2026-10-20T11:00:00Z',delivered_at=? WHERE id=?")
            .bind(delivered_at).bind(&turn).execute(&app.db).await.unwrap();
        let home = env.dir.join("synthetic-queued-codex-home");
        let day = home.join("sessions/2026/10/20");
        std::fs::create_dir_all(&day).unwrap();
        let log = day.join("rollout-2026-10-20T11-00-00-synthetic-queue.jsonl");
        let compact = json!({"timestamp":"2026-10-20T11:10:00Z","type":"compacted","payload":{}}).to_string();
        let input = json!({"timestamp":"2026-10-20T11:20:00Z","type":"response_item","payload":{
            "type":"message","role":"user","content":[{"type":"input_text","text":"Reply with the requested text"}]
        }}).to_string();
        std::fs::write(&log, format!("{compact}\n{input}\n")).unwrap();
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CODEX_HOME":home.to_str().unwrap()}).to_string()).bind(&bot).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET native_session_id='synthetic-queue' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();

        assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap(), "{delivered_at:?}");
        assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "completed_fallback");
        let messages: Vec<(String,String)> = sqlx::query_as("SELECT role,content FROM messages WHERE turn_id=?")
            .bind(&turn).fetch_all(&app.db).await.unwrap();
        assert_eq!(messages, vec![("assistant".into(), "Context compacted · 3s".into())]);
    }
}

#[tokio::test]
async fn synthetic_codex_auto_compact_keeps_the_answer_and_closes_the_turn() {
    let screen = SYNTHETIC_COMPACT.replace("› \n", "• 修好了。\n\n  Worked for 8s • 11:58 AM\n\n› \n");
    let (env, run, turn) = codex_turn(&screen, "繼續修正").await;
    let app = env.app.clone();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "completed_fallback");
    let messages: Vec<(String, String)> = sqlx::query_as("SELECT role,content FROM messages WHERE turn_id=?")
        .bind(&turn).fetch_all(&app.db).await.unwrap();
    assert_eq!(messages, vec![("assistant".into(), "修好了。".into())]);
}

#[tokio::test]
async fn synthetic_codex_completion_chrome_cannot_defeat_hook_reply_dedupe() {
    let screen = "• 已完成。\n  第二行。\n\nWorked for 21s • 11:03 AM\n\n› \n";
    let (env, run, turn) = codex_turn(screen, "").await;
    let app = env.app.clone();
    sqlx::query("UPDATE turns SET origin='external' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
    let bot: String = sqlx::query_scalar("SELECT bot_id FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
    let conv = db::conversation_id(&app.db, &bot).await.unwrap();
    insert_message(&app, &conv, None, "assistant", "已完成。\n第二行。", "hook", false, None).await.unwrap();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=?").bind(&conv).fetch_one(&app.db).await.unwrap();
    assert_eq!(count, 1);
}

/// 真畫面回合中：還在想，備援不收。summary 開著時字頭在撞限前仍是 `Working`；疊上 summary 字頭一樣不收。
#[tokio::test]
async fn a_codex_still_thinking_under_a_summary_header_is_not_closed() {
    for screen in [WORKING, WORKING_SUMMARY] {
        let (env, run, turn) = codex_turn(screen, "Reply with only the word PONG. Do not use tools.").await;
        let app = env.app.clone();
        assert!(!try_fallback(&app, &run, Some(&turn)).await.unwrap(), "還在想：不收");
        assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "in_flight");
    }
    let summary = WORKING.replace("Working (0s • esc to interrupt)", "Planning the fix for the poller (12s • esc to interrupt)");
    let (env, run, turn) = codex_turn(&summary, "Reply with only the word PONG. Do not use tools.").await;
    let app = env.app.clone();
    assert!(!try_fallback(&app, &run, Some(&turn)).await.unwrap(), "summary 字頭：不收");
    assert_eq!(crate::lifecycle::run_state::turn_status(&app, &turn).await, "in_flight");
}

/// 真畫面回合結束：存下來的回覆不帶完成時間那一行。
#[tokio::test]
async fn a_finished_codex_turn_is_stored_without_its_completion_line() {
    let (env, run, turn) = codex_turn(FINISHED, "Reply with exactly: CODEX OK").await;
    let app = env.app.clone();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let reply: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_one(&app.db).await.unwrap();
    assert_eq!(reply, "CODEX OK");
    assert!(!reply.contains("done"));
}
