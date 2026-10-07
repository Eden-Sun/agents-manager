#![allow(unused_imports)]
use crate::hosts::*;
use crate::config::{HostCfg, LOCAL_HOST};
use crate::herdr::HerdrClient;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[path = "../../../../crates/am-base/src/hosts.daemon-tests-linux_systemd_script_tests.rs"]
mod linux_systemd_script_tests;

#[path = "../../../../crates/am-base/src/hosts.daemon-tests-shared_session_script_tests.rs"]
mod shared_session_script_tests;

#[path = "../../../../crates/am-base/src/hosts.daemon-tests-tests.rs"]
mod tests;
