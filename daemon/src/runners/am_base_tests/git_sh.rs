#![allow(unused_imports)]
use crate::git_sh::*;
use crate::config::LOCAL_HOST;
use crate::hosts::sh_quote;
use crate::hosts::HostsAccess;
use anyhow::{anyhow, Result};
use std::time::Duration;

#[path = "../../../../crates/am-base/src/git_sh.daemon-tests-tests.rs"]
mod tests;
