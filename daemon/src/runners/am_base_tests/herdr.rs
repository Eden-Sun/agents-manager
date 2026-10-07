#![allow(unused_imports)]
use crate::herdr::*;
use anyhow::{anyhow, bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

#[path = "../../../../crates/am-base/src/herdr.daemon-tests-arg_tests.rs"]
mod arg_tests;

#[path = "../../../../crates/am-base/src/herdr.daemon-tests-nbsp_tests.rs"]
mod nbsp_tests;

#[path = "../../../../crates/am-base/src/herdr.daemon-tests-rpc_tests.rs"]
mod rpc_tests;

#[path = "../../../../crates/am-base/src/herdr.daemon-tests-split_paste_tests.rs"]
mod split_paste_tests;
