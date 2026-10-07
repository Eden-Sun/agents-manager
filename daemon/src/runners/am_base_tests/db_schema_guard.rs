#![allow(unused_imports)]
use super::check_drift;
use crate::db::schema_guard::*;
use anyhow::{Context, Result};
use sqlx::SqlitePool;

#[path = "../../../../crates/am-base/src/db/schema_guard.daemon-tests-tests.rs"]
mod tests;
