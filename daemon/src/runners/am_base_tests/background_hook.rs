#![allow(unused_imports)]
use crate::background_hook::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[path = "../../../../crates/am-base/src/background_hook.daemon-tests-tests.rs"]
mod tests;
