use super::{progress_due, PROGRESS_MIN_GAP};
use std::time::{Duration, Instant};

#[test]
fn the_gap_is_the_documented_four_frames_a_second() {
    assert_eq!(PROGRESS_MIN_GAP * 4, Duration::from_secs(1));
}

#[test]
fn a_run_that_has_not_emitted_yet_goes_out_at_once() {
    assert!(progress_due(None, false));
}

#[test]
fn a_frame_inside_the_window_is_held_back() {
    assert!(!progress_due(Some(&Instant::now()), false));
}

#[test]
fn the_window_opens_again_once_the_gap_has_passed() {
    let last = Instant::now() - PROGRESS_MIN_GAP - Duration::from_millis(1);
    assert!(progress_due(Some(&last), false));
}

/// The poller's last frame must not stay in `pending` when the turn ends inside the window (`force`).
#[test]
fn force_ignores_the_budget() {
    assert!(progress_due(Some(&Instant::now()), true));
}
