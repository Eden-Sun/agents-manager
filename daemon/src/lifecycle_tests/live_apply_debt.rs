#![allow(unused_imports)]

use crate::lifecycle_tests::prelude::*;
use crate::db;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::state::App;
use crate::lifecycle::*;
use crate::lifecycle::start::{restart_bot_with, StartOpts};
use crate::lifecycle::live_apply_debt::*;
use crate::lifecycle::live_apply_debt::test_seams::*;

mod tests {
    use super::*;
    use super::*;
    use crate::launch_rev;
    use crate::testing as tt;

    /// #723：補寫帳時另一個 writer 正拿著寫入鎖（背景重試與手動重試同時跑就是這樣）。先讀後寫的 deferred 交易升級寫鎖時
    /// SQLite 不跑 busy handler，直接回 `database is locked`（對方剛 commit 則是 517 BUSY_SNAPSHOT）；要等對方放手再寫。
    #[tokio::test]
    async fn a_retry_waits_for_a_concurrent_writer_instead_of_failing_as_locked() {
        let e = tt::env().await;
        let bot = tt::claude_bot(&e.app, &e.project_id, "debt-lock").await;
        let run_id = tt::fake_run(&e.app, &bot.id).await;
        let rev = launch_rev::of(&bot);
        let debt = RuntimeDebt {
            run_id: run_id.clone(),
            bot_id: bot.id.clone(),
            baseline_rev: rev.clone(),
            target_rev: rev,
            runtime_model: Some("claude-opus-5-5".into()),
            runtime_effort: None,
            runtime_fast: None,
            created_at: crate::db::now(),
        };
        store_debt(e.app.db(), &debt).await.unwrap();

        let mut writer = e.app.db().begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?")
            .bind(&run_id)
            .execute(&mut *writer)
            .await
            .unwrap();
        let (app, id) = (e.app.clone(), run_id.clone());
        let retry = tokio::spawn(async move { retry_once(&app, &id).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        writer.commit().await.unwrap();
        retry.await.unwrap().expect("a busy writer only delays the retry; it is not a failure");

        let runtime: Option<String> = sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(e.app.db())
            .await
            .unwrap();
        assert_eq!(runtime.as_deref(), Some("claude-opus-5-5"));
    }
}
