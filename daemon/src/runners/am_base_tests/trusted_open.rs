#![allow(unused_imports)]
use crate::trusted_open::*;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::io::{AsRawFd, FromRawFd as _};
use std::path::{Component, Path};
use std::time::{Duration, SystemTime};

#[path = "../../../../crates/am-base/src/trusted_open.daemon-tests-tests.rs"]
mod tests;
