#![allow(unused_imports)]
use crate::capture::{Capture, ACTIVITY_MAX};
use crate::capture::claude::*;
use crate::capture::*;

#[path = "../../../../crates/am-base/src/capture/claude.daemon-tests-linux_2_1_287_screen_tests.rs"]
mod linux_2_1_287_screen_tests;

#[path = "../../../../crates/am-base/src/capture/claude.daemon-tests-loose_noise_tests.rs"]
mod loose_noise_tests;

#[path = "../../../../crates/am-base/src/capture/claude.daemon-tests-reply_boundary_tests.rs"]
mod reply_boundary_tests;
