#![allow(unused_imports)]
use crate::attach::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;
use crate::db;
use crate::config::{valid_id, ID_RE};
use crate::hosts::{sh_quote, HostConn};

#[path = "../../../../crates/am-base/src/attach.daemon-tests-tests.rs"]
mod tests;
