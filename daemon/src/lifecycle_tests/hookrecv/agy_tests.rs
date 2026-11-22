#![allow(dead_code, unused_imports)]
use super::*;
use super::*;
use crate::testing as tt;

/// 實測 1.2.16 的 Stop（trimmed）加上 hook 子行程補的兩個欄位。
fn stop(extra: Value) -> Value {
    let mut v = json!({"hookEventName": "Stop", "conversationId": "c-1", "modelName": "gemini-3.1-pro-low",
        "transcriptPath": "/h/.gemini/antigravity-cli/brain/c-1/.system_generated/logs/transcript_full.jsonl",
        "workspacePaths": ["/w"], "executionNum": 0, "terminationReason": "NO_TOOL_CALL", "fullyIdle": true});
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v
}





async fn agy_turn(app: &Arc<App>, project_id: &str) -> (String, String, String) {
    let bot_id = db::ulid();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
         VALUES (?,?,?,'agy','[]',0,1,'tok',?)",
    )
    .bind(&bot_id)
    .bind(project_id)
    .bind(format!("agy-{}", &bot_id[bot_id.len() - 8..]))
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
         VALUES (?,?,'running','working','ws-1','pane-1','agent','test',?)",
    )
    .bind(&run_id)
    .bind(&bot_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    let turn_id = db::ulid();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
         VALUES (?,?,?,'web','in_flight','ok','say OK',?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run_id)
    .bind(db::now())
    .execute(&app.db)
    .await
    .unwrap();
    (bot_id, run_id, turn_id)
}

fn body(bot_id: &str, payload: Value) -> HookBody {
    HookBody { bot_id: bot_id.into(), provider: "agy".into(), payload, received_at: None, truncated: false, run_id: None }
}

async fn replies(app: &Arc<App>, turn_id: &str) -> Vec<(String, String)> {
    sqlx::query_as("SELECT role, content FROM messages WHERE turn_id=? AND role IN ('assistant','system') ORDER BY created_at")
        .bind(turn_id)
        .fetch_all(&app.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_stop_completes_the_in_flight_turn_with_the_transcript_reply() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _run, turn_id) = agy_turn(&app, &env.project_id).await;
    process(&app, &body(&bot_id, stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "say OK"})))).await.unwrap();
    let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(t.status, "completed");
    assert_eq!(replies(&app, &turn_id).await, [("assistant".to_string(), "OK".to_string())]);
    process(&app, &body(&bot_id, stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "say OK"})))).await.unwrap();
    assert_eq!(replies(&app, &turn_id).await.len(), 1);
}

#[tokio::test]
async fn a_stop_records_the_model_and_context_tokens_for_the_status_card() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, run_id, _turn) = agy_turn(&app, &env.project_id).await;
    sqlx::query("UPDATE bots SET model = 'gemini-3.8-flash-medium' WHERE id = ?").bind(&bot_id).execute(&app.db).await.unwrap();
    process(&app, &body(&bot_id, stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "say OK", "lastInputTokens": 11824})))).await.unwrap();
    let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
    let st: Value = serde_json::from_str(r.status_json.as_deref().expect("status_json")).unwrap();
    assert_eq!(st["model"]["id"], "gemini-3.8-flash-medium");
    assert_eq!(st["context_window"]["total_input_tokens"], 11824);
}

/// `--conversation=<id>` 接回：第一個 `PreInvocation` 的 conversationId 對得上＝verified；agy 對不存在的 id 只警告、
/// 開新對話（不報錯），回報的 id 對不上＝`resume_mismatch`，聊天室要看得見。
#[tokio::test]
async fn a_resumed_agy_conversation_is_verified_or_reported_as_a_mismatch() {
    for (reported, outcome) in [("conv-expected", "verified"), ("conv-fresh", "mismatch")] {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, _turn) = agy_turn(&app, &env.project_id).await;
        sqlx::query("UPDATE runs SET resume_session_id='conv-expected' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        let pre = json!({"hookEventName": "PreInvocation", "conversationId": reported, "transcriptPath": "/x/transcript_full.jsonl"});
        process(&app, &body(&bot_id, pre)).await.unwrap();
        let (got, native, requested): (Option<String>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT resume_outcome, native_session_id, resume_session_id FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(got.as_deref(), Some(outcome), "{reported}");
        assert_eq!(native.as_deref(), Some(reported));
        assert_eq!(requested, None, "一次性的要求標記清掉了");
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'").bind(&conv).fetch_all(&app.db).await.unwrap();
        assert_eq!(notes.iter().any(|n| n.contains("不是同一個")), outcome == "mismatch", "{notes:?}");
    }
}

#[tokio::test]
async fn an_error_stop_closes_the_turn_as_failed_with_the_agy_error_in_the_note() {
    let _agy_globals = crate::quota_agy::token_test_lock().await;
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, _run, turn_id) = agy_turn(&app, &env.project_id).await;
    process(&app, &body(&bot_id, stop(json!({"terminationReason": "ERROR", "error": "API key not valid"})))).await.unwrap();
    let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
    assert_eq!(t.status, "failed");
    let notes = replies(&app, &turn_id).await;
    assert!(notes.iter().any(|(role, c)| role == "system" && c.contains("API key not valid")), "{notes:?}");
}

#[tokio::test]
async fn the_first_pre_invocation_records_the_conversation_and_a_transcript_under_the_agy_brain_dir() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, run_id, _turn) = agy_turn(&app, &env.project_id).await;
    let conv = format!("c-{}", db::ulid());
    let logs = crate::home::dir().unwrap().join(format!(".gemini/antigravity-cli/brain/{conv}/.system_generated/logs"));
    std::fs::create_dir_all(&logs).unwrap();
    let tp = logs.join("transcript_full.jsonl");
    std::fs::write(&tp, "").unwrap();
    process(&app, &body(&bot_id, json!({"hookEventName": "PreInvocation", "conversationId": conv, "transcriptPath": tp, "invocationNum": 0}))).await.unwrap();
    let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
    assert_eq!(r.native_session_id.as_deref(), Some(conv.as_str()));
    assert_eq!(r.transcript_path.as_deref(), Some(tp.to_str().unwrap()));
    let (bot2, run2, _t2) = agy_turn(&app, &env.project_id).await;
    process(&app, &body(&bot2, json!({"hookEventName": "state", "conversation_id": "", "transcript_path": "/placeholder/transcript_full.jsonl"}))).await.unwrap();
    let r2 = db::run(&app.db, &run2).await.unwrap().unwrap();
    assert_eq!((r2.native_session_id, r2.transcript_path), (None, None), "對話還沒建立：不記");
    process(&app, &body(&bot2, json!({"hookEventName": "state", "conversation_id": conv, "transcript_path": "/h/.gemini/antigravity/brain/x/.system_generated/logs/transcript.jsonl"}))).await.unwrap();
    let r2 = db::run(&app.db, &run2).await.unwrap().unwrap();
    assert_eq!(r2.native_session_id.as_deref(), Some(conv.as_str()));
    assert_eq!(r2.transcript_path, None, "statusLine 的 transcript_path 是摘要版、另一個目錄：不記，只有 hook 的 transcriptPath（transcript_full.jsonl）算");
}

#[tokio::test]
async fn a_transcript_path_outside_the_agy_brain_dir_is_not_recorded() {
    let env = tt::env().await;
    let app = env.app.clone();
    let (bot_id, run_id, _turn) = agy_turn(&app, &env.project_id).await;
    process(&app, &body(&bot_id, json!({"hookEventName": "SessionStart", "conversationId": "c-evil", "transcriptPath": "/etc/passwd.jsonl"}))).await.unwrap();
    let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
    assert_eq!(r.native_session_id.as_deref(), Some("c-evil"));
    assert_eq!(r.transcript_path, None, "bot 自己的 hook token 不能指定任意檔案當 transcript");
}

#[tokio::test]
async fn a_hook_from_another_provider_is_refused_for_an_agy_bot() {
    assert!(provider_matches_kind("agy", "agy"));
    assert!(!provider_matches_kind("claude", "agy") && !provider_matches_kind("agy", "claude"));
}

/// 官方 statusLine JSON fixture：完整解析 context_window（含 cache_read_input_tokens、真实 context_window_size）、
/// model.{id,effort}、agent_state、tool_confirmation_pending、task_count、pending_input_count、quota；
/// 有 statusLine 時優先於 Stop hook／transcript 估算與 `[agy] context_window_tokens`；
/// `tool_confirmation_pending` 觸發與解除 blocked；`quota` 只記進 status_json 不覆蓋 `/usage` 額度列；
/// 遠端（m4p）經 dispatcher → hook.sh spool → drain_remote_locked 一樣完整接取。
#[tokio::test]
async fn statusline_official_payload_takes_precedence_over_transcript_fallback_and_drives_blocked_and_remote_spool() {
    let env = tt::env().await;
    let app = env.app.clone();
    app.cfg.update(|c| {
        c.agy.context_window_tokens = 400_000;
        Ok(())
    }).await.unwrap();
    let (bot_id, run_id, _turn) = agy_turn(&app, &env.project_id).await;
    sqlx::query("UPDATE bots SET model = 'gemini-3.8-flash-medium' WHERE id = ?").bind(&bot_id).execute(&app.db).await.unwrap();
    sqlx::query("UPDATE runs SET runtime_model = 'gemini-3.8-flash-medium', agent_status = 'idle' WHERE id = ?").bind(&run_id).execute(&app.db).await.unwrap();

    let state_payload = json!({
        "hookEventName": "state",
        "conversation_id": "conv-sl-1",
        "transcript_path": "/h/.gemini/antigravity/brain/conv-sl-1/.system_generated/logs/transcript.jsonl",
        "model": {
            "id": "gemini-4-argon",
            "display_name": "Gemini 4 Argon (High)",
            "effort": "high"
        },
        "context_window": {
            "total_input_tokens": 150_000,
            "total_output_tokens": 3_000,
            "context_window_size": 1_048_576,
            "used_percentage": 14.3,
            "remaining_percentage": 85.7,
            "current_usage": {
                "input_tokens": 10_000,
                "output_tokens": 600,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 140_000
            }
        },
        "agent_state": "tool_use",
        "tool_confirmation_pending": true,
        "task_count": 2,
        "pending_input_count": 1,
        "quota": {"gemini": {"remaining_fraction": 0.75}}
    });
    process(&app, &body(&bot_id, state_payload.clone())).await.unwrap();

    let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
    assert_eq!(r.native_session_id.as_deref(), Some("conv-sl-1"));
    assert_eq!(r.transcript_path, None, "statusLine 的摘要版 transcript_path 絕不寫入");
    assert_eq!(r.runtime_model.as_deref(), Some("gemini-4-argon-high"), "model.id + effort 校正 runtime_model");
    assert_eq!(r.agent_status, "blocked", "tool_confirmation_pending:true 補標 blocked");
    let reason = crate::blocked_reason::of(&run_id).expect("blocked_reason");
    assert_eq!(reason.code, "agy_dialog");
    assert!(reason.text.contains("權限"), "{}", reason.text);

    let st: Value = serde_json::from_str(r.status_json.as_deref().expect("status_json")).unwrap();
    assert_eq!(st["context_window"]["context_window_size"], 1_048_576, "官方視窗大小優先於 [agy] context_window_tokens (400K)");
    assert_eq!(st["context_window"]["total_input_tokens"], 150_000);
    assert_eq!(st["context_window"]["current_usage"]["cache_read_input_tokens"], 140_000);
    assert_eq!(st["model"]["id"], "gemini-4-argon-high");
    assert_eq!(st["effort"]["level"], "high");
    assert_eq!(st["agent_state"], "tool_use");
    assert_eq!(st["tool_confirmation_pending"], true);
    assert_eq!(st["task_count"], 2);
    assert_eq!(st["pending_input_count"], 1);
    assert_eq!(st["quota"]["gemini"]["remaining_fraction"], 0.75);
    assert!(!app.quotas.lock().await.contains_key("agy"), "quota 只記在 status_json，不取代 /usage 探測");

    process(&app, &body(&bot_id, stop(json!({"conversationId": "conv-sl-1", "lastAssistantMessage": "DONE", "lastUserMessage": "say OK", "lastInputTokens": 11824})))).await.unwrap();
    let r_after_stop = db::run(&app.db, &run_id).await.unwrap().unwrap();
    let st_after: Value = serde_json::from_str(r_after_stop.status_json.as_deref().unwrap()).unwrap();
    assert_eq!(st_after["context_window"]["context_window_size"], 1_048_576, "Stop hook 不覆蓋 statusLine 的視窗");
    assert_eq!(st_after["context_window"]["total_input_tokens"], 150_000, "Stop hook 不覆蓋 statusLine 的 token 數");

    let mut cleared_payload = state_payload.clone();
    cleared_payload["tool_confirmation_pending"] = json!(false);
    cleared_payload["agent_state"] = json!("idle");
    cleared_payload["task_count"] = json!(0);
    cleared_payload["pending_input_count"] = json!(0);
    process(&app, &body(&bot_id, cleared_payload)).await.unwrap();
    let r_cleared = db::run(&app.db, &run_id).await.unwrap().unwrap();
    assert_eq!(r_cleared.agent_status, "idle", "待確認解除後還原成補標前的 idle");
    assert!(crate::blocked_reason::of(&run_id).is_none());

    let host = "m4p";
    let conn = app
        .hosts
        .insert_remote_for_test(crate::config::HostCfg {
            daemon_url: None,
            shared_session: false,
            name: host.into(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "am-test".into(),
            remote_path: String::new(),
        })
        .await;
    conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
    let rem_proj = db::ulid();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/remote/m4p', 'm4p', ?, ?)")
        .bind(&rem_proj)
        .bind(host)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let (rem_bot, rem_run, _rem_turn) = agy_turn(&app, &rem_proj).await;
    let remote_line = json!({
        "bot_id": rem_bot,
        "provider": "agy",
        "payload": state_payload,
        "received_at": "2026-11-21T05:00:00Z",
        "truncated": false,
        "run_id": rem_run,
    })
    .to_string();
    crate::hosts::set_ssh_fake(host, move |script| {
        if script.contains("umask 077") {
            return Ok(format!("{remote_line}\n"));
        }
        Ok(String::new())
    });
    assert_eq!(drain_remote(&app, host, &rem_bot).await.unwrap(), 1);
    crate::runners::hook_inbox::drain_once(&app).await.unwrap();
    let rr = db::run(&app.db, &rem_run).await.unwrap().unwrap();
    assert_eq!(rr.native_session_id.as_deref(), Some("conv-sl-1"));
    assert_eq!(rr.transcript_path, None);
    let rst: Value = serde_json::from_str(rr.status_json.as_deref().expect("remote status_json")).unwrap();
    assert_eq!(rst["context_window"]["context_window_size"], 1_048_576);
    assert_eq!(rst["context_window"]["current_usage"]["cache_read_input_tokens"], 140_000);
    assert_eq!(rst["task_count"], 2);
}

#[test]
fn a_normal_stop_is_a_turn_with_the_reply_the_hook_process_read_and_no_dedup_key() {
    match classify("agy", &stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "say OK"}))) {
        HookKind::TurnComplete { session_id, turn_id, transcript_path, assistant, user } => {
            assert_eq!(session_id.as_deref(), Some("c-1"));
            assert_eq!(turn_id, None, "`executionNum` 跨回合是不是唯一沒驗過：當去重鑰匙會吃掉之後的回合");
            assert!(transcript_path.unwrap().ends_with("transcript_full.jsonl"));
            assert_eq!((assistant.as_deref(), user.as_deref()), (Some("OK"), Some("say OK")));
        }
        other => panic!("expected TurnComplete, got {other:?}"),
    }
    let stop_raw_fixture = r#"{"artifactDirectoryPath":"/tmp/d/.gemini/antigravity-cli/brain/29ce4fac-aba5-4325-a11c-b5d664ff7a5e","conversationId":"29ce4fac-aba5-4325-a11c-b5d664ff7a5e","error":"","executionNum":0,"fullyIdle":true,"lastUserInput":"test_stop_input","modelName":"gemini-3.8-flash-low","terminationReason":"NO_TOOL_CALL","transcriptPath":"/tmp/d/.gemini/antigravity-cli/brain/29ce4fac-aba5-4325-a11c-b5d664ff7a5e/.system_generated/logs/transcript_full.jsonl","workspacePaths":["/tmp/d"]}"#;
    let mut from_stop_fixture: Value = serde_json::from_str(stop_raw_fixture).unwrap();
    from_stop_fixture["hookEventName"] = json!("Stop");
    from_stop_fixture["lastAssistantMessage"] = json!("OK");
    from_stop_fixture["lastUserMessage"] = json!("from transcript");
    match classify("agy", &from_stop_fixture) {
        HookKind::TurnComplete { user, .. } => assert_eq!(user.as_deref(), Some("from transcript"), "兩者皆有時 lastUserMessage 優先"),
        other => panic!("expected TurnComplete, got {other:?}"),
    }
    match classify("agy", &stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "from transcript", "lastUserInput": "from stdin"}))) {
        HookKind::TurnComplete { user, .. } => assert_eq!(user.as_deref(), Some("from transcript")),
        other => panic!("expected TurnComplete, got {other:?}"),
    }
    match classify("agy", &stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "", "lastUserInput": "from stdin"}))) {
        HookKind::TurnComplete { user, .. } => assert_eq!(user.as_deref(), Some("from stdin"), "lastUserMessage 空白退回 lastUserInput"),
        other => panic!("expected TurnComplete, got {other:?}"),
    }
    match classify("agy", &stop(json!({"lastAssistantMessage": "OK", "lastUserInput": "only input"}))) {
        HookKind::TurnComplete { user, .. } => assert_eq!(user.as_deref(), Some("only input")),
        other => panic!("expected TurnComplete, got {other:?}"),
    }
    match classify("agy", &stop(json!({"lastAssistantMessage": "OK", "last_user_input": "snake case input"}))) {
        HookKind::TurnComplete { user, .. } => assert_eq!(user.as_deref(), Some("snake case input")),
        other => panic!("expected TurnComplete, got {other:?}"),
    }
    match classify("agy", &stop(json!({"lastAssistantMessage": "OK", "lastUserInput": 12345}))) {
        HookKind::TurnComplete { user, .. } => assert_eq!(user, None, "只有 lastUserInput 但型別錯時忽略"),
        other => panic!("expected TurnComplete, got {other:?}"),
    }
    assert!(matches!(classify("agy", &stop(json!({"terminationReason": "model_stop"}))), HookKind::TurnComplete { .. }));
    assert!(matches!(classify("agy", &stop(json!({"terminationReason": "SOMETHING_NEW"}))), HookKind::TurnComplete { .. }));
}

#[test]
fn a_stop_with_background_work_still_running_is_not_the_end_of_the_turn() {
    assert!(matches!(classify("agy", &stop(json!({"fullyIdle": false}))), HookKind::Ignore(_)));
}

#[test]
fn an_error_stop_fails_the_turn_and_the_error_text_is_classified() {
    match classify("agy", &stop(json!({"terminationReason": "ERROR", "error": "429 RESOURCE_EXHAUSTED: quota exceeded"}))) {
        HookKind::TurnFailed { session_id, reason, detail, .. } => {
            assert_eq!(session_id.as_deref(), Some("c-1"));
            assert_eq!(reason, FailureReason::RateLimit);
            assert!(detail.unwrap().contains("quota exceeded"));
        }
        other => panic!("expected TurnFailed, got {other:?}"),
    }
    assert!(matches!(classify("agy", &stop(json!({"terminationReason": "ERROR"}))), HookKind::TurnFailed { .. }), "原因是 ERROR、沒有錯誤文字也算失敗");
}

#[test]
fn session_start_and_the_first_pre_invocation_both_give_the_identity_and_state_is_a_status_line() {
    for ev in ["SessionStart", "PreInvocation"] {
        let v = json!({"hookEventName": ev, "conversationId": "c-9", "transcriptPath": "/t/transcript_full.jsonl", "invocationNum": 0});
        match classify("agy", &v) {
            HookKind::Identity { session_id, transcript_path, .. } => {
                assert_eq!(session_id.as_deref(), Some("c-9"), "{ev}");
                assert_eq!(transcript_path.as_deref(), Some("/t/transcript_full.jsonl"));
            }
            other => panic!("{ev}: expected Identity, got {other:?}"),
        }
    }
    assert!(matches!(classify("agy", &json!({"hookEventName": "state", "conversation_id": "c-9"})), HookKind::StatusLine));
    assert!(matches!(classify("agy", &json!({"conversationId": "c-9"})), HookKind::Ignore(_)));
    assert!(matches!(classify("agy", &json!({"hookEventName": "PostInvocation"})), HookKind::Ignore(_)));
}
