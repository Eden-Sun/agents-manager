use super::{spool_line_log_context, FailureReason, HookKind};

#[test]
fn malformed_spool_log_context_does_not_repeat_payload_text() {
    let secret = r#"{"payload":{"prompt":"PRIVATE-HOOK-PROMPT-641"}}"#;
    let context = spool_line_log_context(secret);
    assert!(!context.contains("PRIVATE-HOOK-PROMPT-641"));
    assert!(context.contains(&secret.len().to_string()));
}

#[test]
fn hook_kind_debug_does_not_repeat_payload_values() {
    let secret = "PRIVATE-HOOK-PROMPT-982";
    let kind = HookKind::TurnComplete {
        session_id: Some("session".into()),
        turn_id: Some("turn".into()),
        transcript_path: Some("/private/transcript.jsonl".into()),
        assistant: Some(secret.into()),
        user: Some(secret.into()),
    };
    assert!(!format!("{kind:?}").contains(secret));

    let failed = HookKind::TurnFailed {
        session_id: None,
        turn_id: None,
        transcript_path: None,
        reason: FailureReason::Unknown,
        detail: Some(secret.into()),
    };
    assert!(!format!("{failed:?}").contains(secret));
    assert!(!format!("{:?}", HookKind::Ignore(secret.into())).contains(secret));
}
