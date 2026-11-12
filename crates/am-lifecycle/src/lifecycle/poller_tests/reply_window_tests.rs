use super::*;
use crate::testing as tt;

async fn codex_empty_hook_case(has_error: bool, hook_first: bool, has_partial: bool) {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cx-native-error").await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let run = tt::fake_run(&app, &bot.id).await;
    let session = "01a18888-0000-7000-8000-000000000002";
    sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?").bind(session).bind(&run).execute(&app.db).await.unwrap();
    let home = app.data_dir.join("codex_error_home");
    let dir = home.join("sessions/2026/10/25");
    std::fs::create_dir_all(&dir).unwrap();
    let error = if has_error { serde_json::json!({"message":"Access to this deployment ended.","codex_error_info":"model_not_found"}) } else { serde_json::Value::Null };
    let partial = has_partial.then_some("Access to this deployment ended.");
    let rows = [
        serde_json::json!({"type":"event_msg","timestamp":"2026-10-25T01:00:00Z","payload":{"type":"task_started","turn_id":"native-error"}}),
        serde_json::json!({"type":"response_item","timestamp":"2026-10-25T01:00:01Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"test prompt"}]}}),
        serde_json::json!({"type":"event_msg","timestamp":"2026-10-25T01:00:05Z","payload":{"type":"task_complete","turn_id":"native-error","last_agent_message":partial,"error":error}}),
    ];
    std::fs::write(dir.join(format!("rollout-2026-10-25T01-00-00-{session}.jsonl")), rows.iter().map(|v| v.to_string() + "\n").collect::<String>()).unwrap();
    sqlx::query("UPDATE bots SET env_json=? WHERE id=?").bind(serde_json::json!({"CODEX_HOME":home.to_string_lossy()}).to_string()).bind(&bot.id).execute(&app.db).await.unwrap();
    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    sqlx::query("UPDATE turns SET prompt_text='test prompt', delivery='ok', native_turn_id='native-error', created_at='2026-10-25T00:59:50Z', delivered_at='2026-10-25T00:59:50Z' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
    env.herdr.set_screen(&format!("pane-{}", bot.id), "› test prompt\n\n› \n");
    let hook = crate::hookrecv::HookBody { bot_id: bot.id.clone(), provider: "codex".into(), payload: serde_json::json!({"type":"agent-turn-complete","thread-id":session,"turn-id":"native-error","input-messages":["test prompt"],"last-assistant-message":""}), received_at: None, truncated: false, run_id: None };
    if hook_first {
        crate::hookrecv::process(&app, &hook).await.unwrap();
        assert!(app.progress_pollers.lock().await.contains_key(&run), "empty hook arms the progress poller");
    }
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
    let error: Option<String> = sqlx::query_scalar("SELECT turn_error FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
    let messages: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&turn).fetch_all(&app.db).await.unwrap();
    if has_error {
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("Access to this deployment ended."));
        assert_eq!(messages, ["Access to this deployment ended."]);
        let replies: Vec<(String, i64)> = sqlx::query_as("SELECT content, incomplete FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn).fetch_all(&app.db).await.unwrap();
        assert_eq!(replies, if has_partial { vec![("Access to this deployment ended.".into(), 1)] } else { vec![] });
    } else {
        assert_eq!(status, "completed_fallback");
        assert_eq!(error, None);
        assert!(messages.is_empty());
    }
    assert!(!try_fallback(&app, &run, Some(&turn)).await.unwrap());
    if !hook_first {
        crate::hookrecv::process(&app, &hook).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        let errors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!((count, errors), (1, i64::from(has_error)), "late hook must not duplicate the turn or error");
    }
}

#[tokio::test]
async fn codex_empty_hook_then_native_error_fails_once() { codex_empty_hook_case(true, true, false).await; }

#[tokio::test]
async fn codex_empty_hook_without_error_completes_fallback() { codex_empty_hook_case(false, true, false).await; }

#[tokio::test]
async fn codex_empty_hook_late_after_native_error_is_deduplicated() { codex_empty_hook_case(true, false, false).await; }

#[tokio::test]
async fn codex_empty_hook_with_partial_reply_and_error_preserves_both() { codex_empty_hook_case(true, true, true).await; }

/// 多節 markdown 的長回覆（#1977）：標題、粗體、列表、空行都有，總長遠超過游標切得開的範圍。
fn long_reply() -> String {
    "⏺ ## 1977 調查報告\n\n**結論**\n- 游標切頭\n- 備援截尾\n\n## 修法\n\n".to_string() + &"**細節** 說明文字很長很長很長。\n".repeat(120) + "\n## 驗證\n- 全綠"
}

/// 回合結束時的 claude 畫面：使用者那句在最上面，回覆，狀態列，輸入框。
fn screen_with(reply: &str) -> String {
    format!("❯ 做個調查\n\n{reply}\n\n✻ Worked for 3s · done 5:08 PM\n\n───────────\n❯ \n───────────\n")
}

/// 沒有 hook 的 claude run：送出 `prompt`，畫面是 `screen`，游標是 `cursor`。
async fn claude_fallback(screen: &str, prompt: &str, cursor: Option<String>) -> (tt::Env, String, String) {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cl-window").await;
    let run = tt::fake_run(&app, &bot.id).await;
    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    sqlx::query("UPDATE turns SET prompt_text=? WHERE id=?").bind(prompt).bind(&turn).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE runs SET last_read_tail_hash=? WHERE id=?").bind(cursor).bind(&run).execute(&app.db).await.unwrap();
    env.herdr.set_screen(&format!("pane-{}", bot.id), screen);
    (env, run, turn)
}

async fn stored_replies(app: &Arc<App>, turn: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'").bind(turn).fetch_all(&app.db).await.unwrap()
}

#[tokio::test]
async fn a_long_multi_section_reply_is_stored_whole_when_the_cursor_sits_mid_reply() {
    let screen = screen_with(&long_reply());
    // 游標＝讀到「## 修法」之前為止的畫面（回覆寫到一半時記下的）。
    let cursor = tail_hash(&screen[..screen.find("## 修法").unwrap()]);
    let (env, run, turn) = claude_fallback(&screen, "做個調查", Some(cursor)).await;
    let app = env.app.clone();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let got = stored_replies(&app, &turn).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("## 1977 調查報告"), "頭被切掉了：{}", &got[0][..40.min(got[0].len())]);
    assert!(got[0].contains("**結論**") && got[0].ends_with("- 全綠"));
}

/// ② chrome：狀態列、輸入框、模式列不因為多取了內容就被存進去。
#[tokio::test]
async fn chrome_below_the_reply_is_not_stored_with_it() {
    let screen = "❯ 做個調查\n\n⏺ ## 結論\n\n內容一行。\n\n✻ Worked for 3s · done 5:08 PM\n\n───────────\n❯ \n───────────\n  ⏵⏵ accept edits on\n";
    let (env, run, turn) = claude_fallback(screen, "做個調查", None).await;
    let app = env.app.clone();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let got = stored_replies(&app, &turn).await;
    assert_eq!(got, vec!["## 結論\n\n內容一行。".to_string()]);
}

/// ① 回覆正文引用了使用者那句話（縮排的引用）：切點仍在真正的回音，引用行留在正文裡。
#[tokio::test]
async fn an_indented_quote_of_the_prompt_in_the_reply_does_not_move_the_cut() {
    let screen = format!("❯ 做個調查\n\n⏺ 結論如下。\n  ❯ 做個調查 是你那句\n\n後續說明。\n\n───────────\n❯ \n───────────\n");
    let (env, run, turn) = claude_fallback(&screen, "做個調查", None).await;
    let app = env.app.clone();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let got = stored_replies(&app, &turn).await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("結論如下。"), "{}", got[0]);
    assert!(got[0].contains("  ❯ 做個調查 是你那句") && got[0].ends_with("後續說明。"), "{}", got[0]);
}

/// ① 送出的句子對不上畫面上的回音：退回游標切片（行為照舊，沒有游標就是整個畫面）。
#[tokio::test]
async fn an_echo_that_does_not_match_what_was_sent_falls_back_to_the_cursor() {
    let screen = screen_with(&long_reply());
    let (env, run, turn) = claude_fallback(&screen, "別句話", None).await;
    let app = env.app.clone();
    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let got = stored_replies(&app, &turn).await;
    assert!(got[0].starts_with("## 1977 調查報告"), "{}", &got[0][..40.min(got[0].len())]);
}

/// ④ 上限照舊：超過 6000 字從頭保留，結尾標示截斷。
#[test]
fn a_reply_over_the_cap_keeps_its_head_and_says_so() {
    let out = truncate_hookless_reply("甲".repeat(HOOKLESS_REPLY_MAX + 500));
    assert!(out.starts_with("甲"));
    assert_eq!(out.matches('甲').count(), HOOKLESS_REPLY_MAX);
    assert!(out.ends_with("（終端擷取到此截斷）"));
}

/// ② 同樣的 chrome 規則：多取了內容，reply 抽取仍然不收狀態列與輸入框。
#[test]
fn screen_reply_keeps_status_rows_out_of_a_wider_window() {
    let text = "❯ 做個調查\n⏺ ## 結論\n\n內容。\n\n✻ Worked for 3s · done 5:08 PM\n\n───────────\n❯ \n───────────\n  ⏵⏵ accept edits on\n";
    let window = slice_after_sent_echo("claude", text, &["做個調查".to_string()]).unwrap();
    assert_eq!(screen_reply("claude", &window, &["做個調查".to_string()]).as_deref(), Some("## 結論\n\n內容。"));
}

/// #1988: Codex fallback 優先自 rollout 取得精確回覆（source="transcript", incomplete=false）
#[tokio::test]
async fn codex_fallback_uses_rollout_reply_when_available() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cx-rollout").await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let run = tt::fake_run(&app, &bot.id).await;
    let sess_id = "01a18888-0000-7000-8000-000000000001";
    sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?").bind(sess_id).bind(&run).execute(&app.db).await.unwrap();

    // 建立假 rollout 檔案
    let home = app.data_dir.join("codex_home");
    let sess_dir = home.join("sessions/2026/10/25");
    std::fs::create_dir_all(&sess_dir).unwrap();
    let rollout_file = sess_dir.join(format!("rollout-2026-10-25T01-00-00-{sess_id}.jsonl"));
    let log = format!(
        "{}\n{}",
        serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-10-25T01:00:00Z",
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "跑測試"}]}
        }),
        serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-10-25T01:00:05Z",
            "payload": {"type": "task_complete", "last_agent_message": "測試全過"}
        })
    );
    std::fs::write(&rollout_file, log).unwrap();

    let env_json = serde_json::json!({"CODEX_HOME": home.to_string_lossy()}).to_string();
    sqlx::query("UPDATE bots SET env_json=? WHERE id=?").bind(env_json).bind(&bot.id).execute(&app.db).await.unwrap();

    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    sqlx::query("UPDATE turns SET prompt_text='跑測試', created_at='2026-10-25T00:59:50Z', delivered_at='2026-10-25T00:59:50Z' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
    env.herdr.set_screen(&format!("pane-{}", bot.id), "› 跑測試\n› 混雜畫面舊內容\n• idle");

    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let msgs: Vec<(String, String, i64)> = sqlx::query_as("SELECT content, source, incomplete FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].0, "測試全過");
    assert_eq!(msgs[0].1, "transcript");
    assert_eq!(msgs[0].2, 0); // incomplete = false
}

/// #1988: 只有上一 turn 的 task_complete、本 turn 尚未完成時不得誤用上一 turn 的回覆，應退回終端快照
#[tokio::test]
async fn codex_fallback_rejects_previous_turn_reply_when_current_turn_incomplete() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cx-prev").await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let run = tt::fake_run(&app, &bot.id).await;
    let sess_id = "01a18888-0000-7000-8000-000000000002";
    sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?").bind(sess_id).bind(&run).execute(&app.db).await.unwrap();

    let home = app.data_dir.join("codex_home_prev");
    let sess_dir = home.join("sessions/2026/10/25");
    std::fs::create_dir_all(&sess_dir).unwrap();
    let rollout_file = sess_dir.join(format!("rollout-2026-10-25T01-00-00-{sess_id}.jsonl"));
    // 只有上一回合的 task_complete，接著是本回合 user prompt，但本回合沒有 task_complete
    let log = format!(
        "{}\n{}\n{}",
        serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-10-25T01:00:00Z",
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "上一回合"}]}
        }),
        serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-10-25T01:00:05Z",
            "payload": {"type": "task_complete", "last_agent_message": "上一回合回覆"}
        }),
        serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-10-25T01:05:00Z",
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "本回合"}]}
        })
    );
    std::fs::write(&rollout_file, log).unwrap();

    let env_json = serde_json::json!({"CODEX_HOME": home.to_string_lossy()}).to_string();
    sqlx::query("UPDATE bots SET env_json=? WHERE id=?").bind(env_json).bind(&bot.id).execute(&app.db).await.unwrap();

    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    sqlx::query("UPDATE turns SET prompt_text='本回合', created_at='2026-10-25T01:04:50Z', delivered_at='2026-10-25T01:04:50Z' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
    env.herdr.set_screen(&format!("pane-{}", bot.id), "› 本回合\n終端快照內容\n");

    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let msgs: Vec<(String, String, i64)> = sqlx::query_as("SELECT content, source, incomplete FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(msgs.len(), 1);
    // 不得誤拿上一回合回覆，退回終端快照
    assert_eq!(msgs[0].1, "terminal_fallback");
    assert_eq!(msgs[0].2, 1); // incomplete = true
}

/// #1988: rollout 缺失時退回終端快照
#[tokio::test]
async fn codex_fallback_falls_back_to_screen_when_rollout_missing() {
    let env = tt::env().await;
    let app = env.app.clone();
    let bot = tt::claude_bot(&app, &env.project_id, "cx-missing").await;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
    let run = tt::fake_run(&app, &bot.id).await;

    let turn = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
    sqlx::query("UPDATE turns SET prompt_text='查狀態', created_at='2026-10-25T01:00:00Z', delivered_at='2026-10-25T01:00:00Z' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
    env.herdr.set_screen(&format!("pane-{}", bot.id), "› 查狀態\n終端快照回覆\n");

    assert!(try_fallback(&app, &run, Some(&turn)).await.unwrap());
    let msgs: Vec<(String, String, i64)> = sqlx::query_as("SELECT content, source, incomplete FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&turn)
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].1, "terminal_fallback");
    assert_eq!(msgs[0].2, 1);
}
