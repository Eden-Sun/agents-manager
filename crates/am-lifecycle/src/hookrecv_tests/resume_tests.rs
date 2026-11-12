use super::*;
use crate::testing::{claude_bot, env, Env};

async fn fixture() -> (Env, db::Bot, db::Run) {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "resumer").await;
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, started_at, resume_session_id)
         VALUES (?,?,'running','idle','ws-1',?,?,?)",
    )
    .bind(&run_id)
    .bind(&bot.id)
    .bind(format!("pane-{}", bot.id))
    .bind(db::now())
    .bind("native-expected")
    .execute(&e.app.db)
    .await
    .unwrap();
    let run = db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap();
    (e, bot, run)
}

async fn remaining(e: &Env, run: &db::Run) -> Option<String> {
    sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap()
}

/// claude bot 的 pane 裡跑的 `codex exec` 帶著繼承來的 AM_BOT_ID／token／run id 送 notify：thread-id 不是這顆
/// bot 的 session，不能拿去對 `resume_session_id`、更不能寫進 native_session_id（2026-09-22 AM-issuers-XH）。
#[tokio::test]
async fn a_hook_from_another_provider_cannot_claim_this_bots_session() {
    let (e, bot, run) = fixture().await;
    let body = HookBody {
        bot_id: bot.id.clone(),
        provider: "codex".into(),
        payload: json!({"type": "agent-turn-complete", "thread-id": "01a0b8c6-e80a-7360-87e7-405fd448490f", "turn-id": "t1", "last-assistant-message": "done"}),
        ..serde_json::from_value(json!({"bot_id": ""})).unwrap()
    };
    process_locked(&e.app, &body).await.unwrap();
    assert_eq!(remaining(&e, &run).await.as_deref(), Some("native-expected"), "標記被別種 provider 消耗掉了");
    let (native, outcome): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT native_session_id, resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!((native, outcome), (None, None));
    // 同一顆的 claude hook 照常。
    assert!(provider_matches_kind("claude", "claude") && provider_matches_kind("Claude", "claude") && provider_matches_kind("codex", ""));
    assert!(!provider_matches_kind("codex", "claude") && !provider_matches_kind("grok", "codex"));
}

#[tokio::test]
async fn a_reported_session_id_consumes_the_marker() {
    let (e, bot, run) = fixture().await;
    consume_resume_session(&e.app, &bot, &run, Some("native-expected")).await.unwrap();
    assert_eq!(remaining(&e, &run).await, None);
    let (e, bot, run) = fixture().await;
    consume_resume_session(&e.app, &bot, &run, Some("native-other")).await.unwrap();
    consume_resume_session(&e.app, &bot, &run, Some("native-other")).await.unwrap();
    assert_eq!(remaining(&e, &run).await, None);
}

/// hook 回報的 session 跟預期的不是同一個：CLI 自己默默開了新對話，`--resume` 沒有報任何錯——
/// 這是最容易被忽略的一種（issue #92）。以前 `context_lost` 只寫 log，這裡要看得到系統訊息。
#[tokio::test]
async fn a_session_mismatch_leaves_a_visible_note_not_just_a_log_line() {
    let (e, bot, run) = fixture().await;
    sqlx::query("UPDATE runs SET status_json = ? WHERE id = ?")
        .bind(json!({"context_window": {"used_percentage": 42}, "_agm_resumed_context": "native-expected"}).to_string())
        .bind(&run.id).execute(&e.app.db).await.unwrap();
    consume_resume_session(&e.app, &bot, &run, Some("native-other")).await.unwrap();
    let status = db::run(&e.app.db, &run.id).await.unwrap().unwrap().status_json.unwrap();
    assert!(serde_json::from_str::<Value>(&status).unwrap().get("context_window").is_none(), "a mismatched resume must not keep old context");
    let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
    let note: String = sqlx::query_scalar(
        "SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&conv)
    .fetch_one(&e.app.db)
    .await
    .unwrap();
    assert!(note.contains("不是同一個"), "{note}");
}

/// 結論與回報的 session 同一句寫下（issue #107）：不拿 bot 鎖的讀者（`api::started_json`）看到
/// `verified` 時，一定讀得到是哪一段——不必等 Identity 分支下一句 UPDATE。
#[tokio::test]
async fn the_verdict_and_the_reported_session_are_written_in_one_statement() {
    let (e, bot, run) = fixture().await;
    consume_resume_session(&e.app, &bot, &run, Some("native-expected")).await.unwrap();
    let (outcome, native): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT resume_outcome, native_session_id FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!((outcome.as_deref(), native.as_deref()), (Some("verified"), Some("native-expected")));
}

#[tokio::test]
async fn a_hook_without_a_session_id_leaves_the_request_pending() {
    let (e, bot, run) = fixture().await;
    consume_resume_session(&e.app, &bot, &run, None).await.unwrap();
    assert_eq!(remaining(&e, &run).await.as_deref(), Some("native-expected"));
}
