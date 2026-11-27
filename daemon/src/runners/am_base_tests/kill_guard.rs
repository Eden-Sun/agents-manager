#![allow(unused_imports)]
use crate::kill_guard::*;
use serde_json::Value;

#[path = "../../../../crates/am-base/src/kill_guard.daemon-tests-tests.rs"]
mod tests;

#[path = "../../../../crates/am-base/src/kill_guard.parity.daemon-tests-tests.rs"]
mod parity;
