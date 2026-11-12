use super::*;

/// Captured from grok 1.0.13 (appendix F), trimmed.
const GROK_STOP: &str = r#"{"hookEventName":"stop","sessionId":"01a072c2-9098-7d50-b3d1-f1750320ae28",
  "cwd":"/x","workspaceRoot":"/x","timestamp":"2026-09-05T18:09:13.833011+00:00",
  "transcriptPath":"/Users/me/.grok/sessions/%2Fx/01a072c2/updates.jsonl",
  "promptId":"089f03f9-594f-4e92-bfb0-5180eefaa250","permissionMode":"bypassPermissions",
  "reason":"end_turn","stopHookActive":false,"lastAssistantMessage":"GROK-OK",
  "backgroundTasks":[],"sessionCrons":[],"hook_event_name":"stop",
  "session_id":"01a072c2-9098-7d50-b3d1-f1750320ae28"}"#;

/// #1007：解不開的 claude payload（`{"raw": …}`）沒有事件名，不能猜成 SessionStart；舊形狀（只有 session_id、prompt_id）照舊猜。
#[test]
fn an_unparseable_claude_payload_is_ignored_not_guessed_as_session_start() {
    assert!(matches!(classify("claude", &json!({"raw": "{\"hook_event_name\":\"Stop\",\"last_assistant_mess"})), HookKind::Ignore(_)));
    assert!(matches!(classify("claude", &json!({"raw": ""})), HookKind::Ignore(_)));
    assert!(matches!(classify("claude", &json!({"session_id": "s1"})), HookKind::Identity { .. }), "沒有事件名、沒有 raw 的舊形狀照舊猜");
    assert!(matches!(classify("claude", &json!({"prompt_id": "p1"})), HookKind::TurnComplete { .. }));
}

/// #1757：claude 2.1.299 實錄（`claude -p "/hicmd abc"`、`claude -p "/greet"`，路徑與 session_id 改過）。新版 `UserPromptSubmit`
/// 在 slash 指令／skill 展開前觸發，`prompt` 是原始的 `/指令`。daemon 不掛也不讀這個事件（一律 Ignore），
/// 回合起點與送達看 transcript、結束看 Stop 的 `prompt_id`——同一回合的兩個事件 `prompt_id` 相同，Stop 照舊配得上。
const CLAUDE_2_1_299_SLASH: [(&str, &str); 2] = [
    (
        r#"{"session_id":"9f0c51f6-61b5-4f4e-9d0b-0a4f39b7c1d2","transcript_path":"/x/t.jsonl","cwd":"/x","permission_mode":"default","prompt_id":"3041d9db-7bbc-4a44-a695-bc0ac9ceca3a","hook_event_name":"UserPromptSubmit","prompt":"/hicmd abc"}"#,
        r#"{"session_id":"9f0c51f6-61b5-4f4e-9d0b-0a4f39b7c1d2","transcript_path":"/x/t.jsonl","cwd":"/x","permission_mode":"default","prompt_id":"3041d9db-7bbc-4a44-a695-bc0ac9ceca3a","effort":{"level":"medium"},"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"HI-CMD abc","background_tasks":[],"session_crons":[]}"#,
    ),
    (
        r#"{"session_id":"5b2e8d07-3c1a-4e8f-a2d6-7e9f0c4b1a35","transcript_path":"/x/t2.jsonl","cwd":"/x","permission_mode":"default","prompt_id":"c4ef1704-0d91-4bd4-8722-b8c7c3052d66","hook_event_name":"UserPromptSubmit","prompt":"/greet"}"#,
        r#"{"session_id":"5b2e8d07-3c1a-4e8f-a2d6-7e9f0c4b1a35","transcript_path":"/x/t2.jsonl","cwd":"/x","permission_mode":"default","prompt_id":"c4ef1704-0d91-4bd4-8722-b8c7c3052d66","effort":{"level":"medium"},"hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"HI-SKILL","background_tasks":[],"session_crons":[]}"#,
    ),
];

#[test]
fn claude_2_1_299_slash_and_skill_prompt_submit_is_ignored_and_stop_still_pairs() {
    for (submit, stop) in CLAUDE_2_1_299_SLASH {
        let submit: Value = serde_json::from_str(submit).unwrap();
        assert!(submit["prompt"].as_str().unwrap().starts_with('/'), "新版給的是展開前的原始指令");
        assert!(matches!(classify("claude", &submit), HookKind::Ignore(_)), "{submit}");
        match classify("claude", &serde_json::from_str(stop).unwrap()) {
            HookKind::TurnComplete { turn_id, assistant, .. } => {
                assert_eq!(turn_id.as_deref(), submit["prompt_id"].as_str());
                assert!(assistant.unwrap().starts_with("HI-"));
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_claim_collision_spool_event_carries_the_child_bot_id_and_claim_id() {
    let payload = json!({
        "hook_event_name": "AmClaimCollisionCheck",
        "issue": 1797,
        "title": "遠端認領票",
        "child": "worker",
        "child_bot_id": " 01CHILD ",
        "claim_id": "abc123",
    });
    match classify("claude", &payload) {
        HookKind::ClaimCollisionCheck { child, child_bot_id, claim_id, .. } => {
            assert_eq!(child, "worker");
            assert_eq!(child_bot_id.as_deref(), Some("01CHILD"));
            assert_eq!(claim_id.as_deref(), Some("abc123"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_claim_collision_spool_event_is_not_treated_as_an_unknown_provider_event() {
    let payload = json!({
        "hook_event_name": "AmClaimCollisionCheck",
        "issue": 1468,
        "title": "遠端認領票",
        "child": "worker",
        "branch": "fix/lx1468",
    });
    assert!(
        !matches!(classify("claude", &payload), HookKind::Ignore(_)),
        "認領撞題 spool 事件應該被 hook receiver 識別，而不是靜默丟棄",
    );
}

#[test]
fn grok_stop_is_a_turn() {
    let v: Value = serde_json::from_str(GROK_STOP).unwrap();
    match classify("grok", &v) {
        HookKind::TurnComplete { session_id, turn_id, transcript_path, assistant, user } => {
            assert_eq!(session_id.as_deref(), Some("01a072c2-9098-7d50-b3d1-f1750320ae28"));
            assert_eq!(turn_id.as_deref(), Some("089f03f9-594f-4e92-bfb0-5180eefaa250"));
            assert!(transcript_path.unwrap().ends_with("updates.jsonl"));
            assert_eq!(assistant.as_deref(), Some("GROK-OK"));
            assert!(user.is_none());
        }
        other => panic!("expected TurnComplete, got {other:?}"),
    }
}

#[test]
fn grok_session_end_stop_is_ignored() {
    let v = json!({"hookEventName":"stop","sessionId":"s","reason":"shutdown","stopHookActive":false});
    assert!(matches!(classify("grok", &v), HookKind::Ignore(_)));
    let v = json!({"hookEventName":"stop","sessionId":"s","reason":"end_turn","stopHookActive":true});
    assert!(matches!(classify("grok", &v), HookKind::Ignore(_)));
    let v = json!({"hookEventName":"session_end","sessionId":"s"});
    assert!(matches!(classify("grok", &v), HookKind::Ignore(_)));
}

#[test]
fn grok_session_start_is_identity() {
    let v = json!({"hookEventName":"session_start","sessionId":"s1","source":"new"});
    match classify("grok", &v) {
        HookKind::Identity { session_id, transcript_path } => {
            assert_eq!(session_id.as_deref(), Some("s1"));
            assert!(transcript_path.is_none());
        }
        other => panic!("expected Identity, got {other:?}"),
    }
}
