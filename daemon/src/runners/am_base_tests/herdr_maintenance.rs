#![allow(unused_imports)]
use crate::herdr_maintenance::*;
use anyhow::Result;
use serde::Deserialize;
use sqlx::SqlitePool;

#[path = "../../../../crates/am-base/src/herdr_maintenance.daemon-tests-tests.rs"]
mod tests;
