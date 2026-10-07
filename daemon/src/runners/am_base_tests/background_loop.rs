#![allow(unused_imports)]
use crate::background_loop::*;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[path = "../../../../crates/am-base/src/background_loop.daemon-tests-tests.rs"]
mod tests;
