#![allow(unused_imports)]
use crate::changelog;
use crate::release_triage::*;
use std::path::Path;
use sqlx::SqlitePool;

#[path = "../../../../crates/am-base/src/release_triage/tests.rs"]
mod tests;

#[path = "../../../../crates/am-base/src/release_triage/tests_publish.rs"]
mod tests_publish;

#[path = "../../../../crates/am-base/src/release_triage/tests_feed.rs"]
mod tests_feed;
