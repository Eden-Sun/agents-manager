#![allow(unused_imports)]
use crate::upstream_update::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use anyhow::{anyhow, Result};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use crate::changelog::{self, cli_version_string, parse_version, version_string};

#[path = "../../../../crates/am-base/src/upstream_update.daemon-tests-tests.rs"]
mod tests;
