#![allow(unused_imports)]

use crate::lifecycle_tests::prelude::*;
use crate::db;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::state::App;
use crate::lifecycle::*;
use crate::lifecycle::start::{restart_bot_with, StartOpts};

use super::*;

#[test]
fn an_ended_runs_pane_typed_memo_is_dropped() {
    remember_pane_typed("memo-gone");
    remember_pane_typed("memo-kept");
    retain_pane_typed(&["memo-kept".to_string()]);
    assert!(!pane_typed_memo("memo-gone"), "結束的 run 不留帳");
    assert!(pane_typed_memo("memo-kept"));
    retain_pane_typed(&[]);
}
