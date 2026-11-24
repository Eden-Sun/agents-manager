#![allow(unused_imports)]

use crate::lifecycle::late_proof::*;
use crate::lifecycle::late_proof::test_seams::*;
use crate::lifecycle::delivery::Proof;
use std::collections::HashSet;
use std::time::{Duration, Instant};

fn pending(noted: Instant) -> Pending {
    pending_at("claude", "pane-test", &Proof::EchoRow, 0, 0, "prompt", "2026-10-12T12:00:00.000Z".into(), noted)
}

#[test]
fn issue_1494_registry_caps_1000_runs_and_expires_without_waiting() {
    let base = Instant::now();
    let mut registry = PendingRegistry::default();
    for n in 0..1000u64 {
        let noted = base + Duration::from_millis(n);
        registry.insert_at(format!("run-{n}"), pending(noted), noted);
    }

    assert_eq!(registry.len(), MAX_PENDING, "容量上限應淘汰最舊的 prompt");
    assert!(!registry.contains("run-0"));
    assert!(registry.contains("run-999"));
    let active_runs = (0..1000).map(|n| format!("run-{n}")).collect();
    let expired_at = base + Duration::from_millis(999) + CLAIM_WITHIN + Duration::from_millis(1);
    assert_eq!(registry.sweep_at(expired_at, &active_runs), MAX_PENDING, "TTL 到期應清掉所有 prompt");
    assert_eq!(registry.len(), 0);

    let mut completed = PendingRegistry::default();
    completed.insert_at("run-completed".into(), pending(base), base);
    assert_eq!(completed.sweep_at(base, &HashSet::new()), 1, "run 已完成／消失就立即清掉 prompt");
    assert!(!completed.contains("run-completed"));
}

#[test]
fn owed_take_refuses_a_pending_bound_to_another_turn() {
    let base = Instant::now();
    let mut registry = PendingRegistry::default();
    registry.insert_at("run-1".into(), pending(base), base);
    registry.bind_turn("run-1", "turn-2");

    assert!(registry.take_for_turn("run-1", "turn-1").is_none(), "T2 的 Pending 不能升級 T1");
    assert!(registry.contains("run-1"), "屬於 T2 的項目留給 T2 自己認領");
    assert!(registry.take_for_turn("run-1", "turn-2").is_some(), "同一回合仍可認領");
}

#[test]
fn owed_take_refuses_an_unbound_pending() {
    let base = Instant::now();
    let mut registry = PendingRegistry::default();
    registry.insert_at("run-1".into(), pending(base), base);

    assert!(registry.take_for_turn("run-1", "turn-1").is_none());
}

#[test]
fn issue_1670_registry_is_private_to_each_test_thread() {
    std::thread::spawn(|| note("run-left-by-another-test", pending(Instant::now()))).join().unwrap();
    assert_eq!(pending_count(), 0, "別的測試留下的 prompt 不屬於這條測試的 registry");
}
