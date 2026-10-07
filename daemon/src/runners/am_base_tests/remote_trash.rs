#![allow(unused_imports)]
use crate::remote_trash::*;
use crate::hosts::{sh_quote, HostConn};
use anyhow::{bail, Result};
use std::time::Duration;

#[path = "../../../../crates/am-base/src/remote_trash.daemon-tests-tests.rs"]
mod tests;
