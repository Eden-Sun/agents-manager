#![allow(unused_imports)]
use crate::primary_keep_warm::*;
use crate::cache_clock::{self, KEEP_WARM_CRID_PREFIX, WARM_COMPACT_NOTE_PREFIX};
use crate::db;
use am_core::PromptRequest;
use am_ports::{Clock, DbContext, EventSink, SystemMessageWriter, TurnControl};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[path = "../../../../crates/am-base/src/primary_keep_warm.daemon-tests-tests.rs"]
mod tests;
