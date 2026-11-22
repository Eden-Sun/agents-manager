use super::*;
use std::time::{Duration, Instant};

#[test]
fn spool_lines_and_the_status_slot_come_apart() {
    let out = "{\"bot_id\":\"b\"}\n{\"bot_id\":\"b\",\"provider\":\"claude\"}\n\n---AM-STATUS---\n{\n  \"session_id\": \"s\"\n}\n";
    let d = parse_drain_output(out);
    assert_eq!(d.lines.len(), 2);
    assert_eq!(d.status.as_deref(), Some("{\n  \"session_id\": \"s\"\n}"));
    assert!(d.stuck_bytes.is_none());
}

#[test]
fn output_without_the_marker_is_all_spool() {
    let d = parse_drain_output("{\"bot_id\":\"b\"}\n");
    assert_eq!(d.lines, vec!["{\"bot_id\":\"b\"}"]);
    assert!(d.status.is_none());
    let d = parse_drain_output("");
    assert!(d.lines.is_empty() && d.status.is_none());
}

#[test]
fn an_empty_status_slot_is_no_status() {
    assert!(parse_drain_output("---AM-STATUS---\n\n").status.is_none());
}



#[test]
fn the_status_slot_becomes_a_claude_statusline_hook() {
    let b = status_body("bot1", "{\"session_id\":\"s\"}").expect("body");
    assert_eq!(b.provider, "claude");
    assert_eq!(b.bot_id, "bot1");
    assert!(matches!(classify(&b.provider, &b.payload), HookKind::StatusLine));
    assert!(status_body("bot1", "not json").is_none());
    assert!(status_body("bot1", "3").is_none());
}

/// §11.4.4.
#[test]
fn a_second_trigger_inside_the_window_is_merged() {
    let mut g = DrainGate::default();
    let t0 = Instant::now();
    assert!(gate_admit(&mut g, t0));
    assert!(!gate_admit(&mut g, t0 + Duration::from_millis(200)));
    assert!(!gate_admit(&mut g, t0 + Duration::from_millis(900)));
    assert!(gate_take_again(&mut g));
    assert!(!gate_take_again(&mut g), "the flag is consumed once");
    assert!(gate_admit(&mut g, t0 + DRAIN_WINDOW + Duration::from_millis(1)));
    assert!(!g.again);
}



