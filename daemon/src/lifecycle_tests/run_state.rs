#![allow(unused_imports)]

use crate::lifecycle_tests::prelude::*;
use crate::db;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::state::App;
use crate::lifecycle::*;
use crate::lifecycle::start::{restart_bot_with, StartOpts};
use crate::lifecycle::run_state::*;
use crate::lifecycle::run_state::test_seams::*;

mod tests {
    use super::*;
    use super::*;
    use crate::testing as tt;

    async fn state(app: &Arc<App>, run: &str) -> String {
        sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(run).fetch_one(app.db()).await.unwrap()
    }

    /// 三種結果分得開：轉過去、CAS 輸了、DB 寫不進去。
    #[tokio::test]
    async fn a_transition_tells_applied_lost_and_a_failed_write_apart() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "cas").await;
        let run = tt::fake_run(&app, &bot.id).await;

        assert_eq!(transition(app.db(), &run, &["starting"], "running", None).await.unwrap(), Moved::Lost, "它是 running，不是 starting");
        assert_eq!(state(&app, &run).await, "running");

        refuse_run_state(&app, "stopping").await;
        assert!(transition(app.db(), &run, LIVE, "stopping", None).await.is_err());
        assert_eq!(state(&app, &run).await, "running", "寫不進去就是沒轉");
        accept_run_state(&app, "stopping").await;

        assert_eq!(transition(app.db(), &run, LIVE, "stopping", None).await.unwrap(), Moved::Applied);
        assert_eq!(transition(app.db(), &run, &["stopping"], "stopped", None).await.unwrap(), Moved::Applied);
        let ended: Option<String> = sqlx::query_scalar("SELECT ended_at FROM runs WHERE id=?").bind(&run).fetch_one(app.db()).await.unwrap();
        assert!(ended.is_some(), "終態補上 ended_at");
        assert_eq!(transition(app.db(), &run, LIVE, "running", None).await.unwrap(), Moved::Lost, "終態不會被拉回 active");
    }

    /// 終態改標跟轉移同一套三種結果（#146 重開）：寫不進去是錯、不是「輸了」；這顆 bot 已經有新的 active run 時不動舊的。
    #[tokio::test]
    async fn a_relabel_tells_applied_lost_and_a_failed_write_apart() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "relabel").await;
        let old = tt::fake_run(&app, &bot.id).await;
        assert_eq!(transition(app.db(), &old, LIVE, "exited", None).await.unwrap(), Moved::Applied);

        refuse_run_state(&app, "stopped").await;
        assert!(relabel(app.db(), &old, "exited", "stopped").await.is_err(), "寫不進去就是錯");
        assert_eq!(state(&app, &old).await, "exited");
        accept_run_state(&app, "stopped").await;
        assert_eq!(relabel(app.db(), &old, "stopped", "exited").await.unwrap(), Moved::Lost, "它是 exited，不是 stopped");
        assert_eq!(relabel(app.db(), &old, "exited", "stopped").await.unwrap(), Moved::Applied);

        let new = tt::fake_run(&app, &bot.id).await;
        assert_eq!(relabel(app.db(), &old, "stopped", "exited").await.unwrap(), Moved::Lost, "新的 run 起來了：舊 run 的標籤不再代表 bot");
        assert_eq!((state(&app, &old).await, state(&app, &new).await), ("stopped".to_string(), "running".to_string()));
    }
}
