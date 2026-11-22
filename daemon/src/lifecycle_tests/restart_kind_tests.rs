#![allow(unused_imports)]

use crate::lifecycle_tests::prelude::*;
use crate::db;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::state::App;
use crate::lifecycle::*;
use crate::lifecycle::start::{restart_bot_with, StartOpts};
use crate::lifecycle::restart_kind_tests::*;
use crate::testing as tt;

/// 驗收三：直接誤呼一般的 `restart_bot_with(child)` 要在停任何東西之前就被拒絕，不靠呼叫端先分類的約定。
#[tokio::test]
async fn a_regular_restart_of_a_child_is_refused_before_anything_is_stopped() {
    let env = tt::env().await;
    let app = env.app.clone();
    let kid = live_child(&env, "ui").await;

    for opts in [StartOpts::default(), StartOpts { resume_native: true, require_idle: true, ..Default::default() }] {
        match restart_bot_with(&app, &kid.id, opts).await {
            Err(LcError::Conflict(v)) => assert_eq!(v["reason"], "child_restarts_in_pane", "{v}"),
            other => panic!("child 走一般 stop + start 必須被拒絕：{other:?}"),
        }
        assert_child_untouched(&env, &kid).await;
    }
}
