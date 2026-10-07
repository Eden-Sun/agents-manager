#![allow(unused_imports)]
use crate::tools::*;
use crate::config::LOCAL_HOST;
use crate::hosts::sh_quote;
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[path = "../../../../crates/am-base/src/tools.daemon-tests-alias_path_tests.rs"]
mod alias_path_tests;

#[path = "../../../../crates/am-base/src/tools.daemon-tests-tests.rs"]
mod tests;
