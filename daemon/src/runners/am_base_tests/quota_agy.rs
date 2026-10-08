#![allow(unused_imports)]
use std::time::Duration;
use crate::quota_agy::*;
use crate::config::LOCAL_HOST;
use crate::quota::{Quota, Window};
use anyhow::{anyhow, Result};

#[path = "../../../../crates/am-base/src/quota_agy.daemon-tests-backoff_tests.rs"]
mod backoff_tests;

#[path = "../../../../crates/am-base/src/quota_agy.daemon-tests-login_tests.rs"]
mod login_tests;

#[path = "../../../../crates/am-base/src/quota_agy.daemon-tests-logout_tests.rs"]
mod logout_tests;

#[path = "../../../../crates/am-base/src/quota_agy.daemon-tests-pane_tests.rs"]
mod pane_tests;

#[path = "../../../../crates/am-base/src/quota_agy.daemon-tests-tests.rs"]
mod tests;
