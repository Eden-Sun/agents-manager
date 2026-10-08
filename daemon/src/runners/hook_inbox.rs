//! `hook_inbox` 背景工作與 drain_once runner。

use crate::app_ports_r2a9::HookProcessor;
use crate::db;
use crate::hook_body::HookBody;
use crate::hook_inbox::{
    mark_dead, mark_done, mark_failed, pending, prune, unparseable_body_reason, Pending,
};
use crate::state::App;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::task::JoinSet;

const POLL_EVERY: Duration = Duration::from_secs(5);
const BATCH: i64 = 64;

/// 唯一的消費者。`receive` / drain 只負責 commit 之後叫醒它，不自己處理——單一消費者才不必為
/// 「同一列被兩邊同時處理」另外加 claim 欄位。
pub fn spawn_worker(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            // 先做一輪再等：daemon 重啟後把上一輪沒處理完的補回來，就是這一行。
            match drain_once(&app).await {
                Ok(n) if n > 0 => tracing::info!(processed = n, "hook inbox drained"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = ?e, "hook inbox drain failed"),
            }
            if let Err(e) = prune(&app.db).await {
                tracing::debug!(error = ?e, "hook inbox prune failed");
            }
            tokio::select! {
                _ = app.hook_inbox_wake.notified() => {}
                _ = tokio::time::sleep(POLL_EVERY) => {}
            }
        }
    });
}

/// 同時處理幾顆 bot 的列（每顆 bot 一個 task）。
const MAX_BOT_TASKS: usize = 8;

/// 正在被某個 task 處理的 bot：同一顆 bot 不會有兩個 task 同時吃它的列（組內要照 rowid 順序）。
fn busy_bots() -> &'static std::sync::Mutex<HashSet<String>> {
    static BUSY: OnceLock<std::sync::Mutex<HashSet<String>>> = OnceLock::new();
    BUSY.get_or_init(Default::default)
}

struct BotClaim(String);

impl BotClaim {
    fn try_new(bot_id: &str) -> Option<Self> {
        let mut busy = busy_bots().lock().unwrap_or_else(|e| e.into_inner());
        busy.insert(bot_id.to_string()).then(|| Self(bot_id.to_string()))
    }
}

impl Drop for BotClaim {
    fn drop(&mut self) {
        busy_bots().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// 一顆 bot 的列，照 rowid 順序一列一列處理。有一列失敗（`mark_failed`）就停住這顆 bot 這一輪：
/// 它後面的列不能跳過去先跑（順序），等退避到了下一輪再從這列開始。
async fn drain_bot(app: Arc<App>, rows: Vec<Pending>, _claim: BotClaim) -> Result<usize> {
    let mut done = 0usize;
    for row in rows {
        match serde_json::from_str::<HookBody>(&row.body_json) {
            Ok(body) => match app.process_hook(&body, Some(&row.id)).await {
                Ok(()) => {
                    mark_done(&app.db, &row.id).await?;
                    done += 1;
                }
                Err(e) => {
                    let attempts = row.attempts + 1;
                    tracing::warn!(id = %row.id, attempts, error = ?e, "hook event failed; will retry");
                    mark_failed(&app.db, &row.id, attempts, &format!("{e:#}")).await?;
                    break;
                }
            },
            Err(_) => {
                tracing::error!(id = %row.id, "hook event body unparseable; dropped");
                mark_dead(&app.db, &row.id, unparseable_body_reason()).await?;
            }
        }
    }
    Ok(done)
}

/// 處理一批。回傳這一輪真的處理完幾列。
///
/// 一批按 `bot_id` 分組，每顆 bot 一個 task（同時最多 [`MAX_BOT_TASKS`] 顆），組內照寫入順序（#911）：
/// 一顆 bot 握著自己的鎖（啟動等就緒 60 秒、批次重啟）時，別顆 bot 的 Stop hook 不再排在它後面。
pub async fn drain_once(app: &Arc<App>) -> Result<usize> {
    let mut done = 0usize;
    loop {
        let rows = pending(&app.db, &db::now(), BATCH).await?;
        if rows.is_empty() {
            return Ok(done);
        }
        let batch = rows.len();
        let mut groups: Vec<(String, Vec<Pending>)> = Vec::new();
        for row in rows {
            match groups.iter_mut().find(|(bot, _)| *bot == row.bot_id) {
                Some((_, g)) => g.push(row),
                None => groups.push((row.bot_id.clone(), vec![row])),
            }
        }
        let mut tasks: JoinSet<Result<usize>> = JoinSet::new();
        let mut first_err = None;
        let mut started = 0usize;
        let mut collect = |res: Result<Result<usize>, tokio::task::JoinError>, done: &mut usize| match res {
            Ok(Ok(n)) => *done += n,
            Ok(Err(e)) => {
                first_err.get_or_insert(e);
            }
            Err(e) => {
                first_err.get_or_insert(anyhow::anyhow!("hook inbox task failed: {e}"));
            }
        };
        for (bot_id, rows) in groups {
            // 別的 drain 正在處理這顆 bot：這一輪讓它做完，不搶。
            let Some(claim) = BotClaim::try_new(&bot_id) else { continue };
            while tasks.len() >= MAX_BOT_TASKS {
                if let Some(res) = tasks.join_next().await {
                    collect(res, &mut done);
                }
            }
            started += 1;
            tasks.spawn(drain_bot(app.clone(), rows, claim));
        }
        while let Some(res) = tasks.join_next().await {
            collect(res, &mut done);
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        // 這一批沒滿就沒有下一批了；滿了就繼續，避免一次喚醒只吃 BATCH 列。
        // 所有組都被別人佔著（一個都沒起）時再撈也是同一批，直接收工。
        if batch < BATCH as usize || started == 0 {
            return Ok(done);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;
    use serde_json::json;

    fn stop(bot_id: &str, prompt_id: &str) -> HookBody {
        HookBody {
            bot_id: bot_id.into(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "sess-1", "prompt_id": prompt_id, "last_assistant_message": "done"}),
            received_at: Some(format!("2026-09-17T12:00:00.000Z-{prompt_id}")),
            truncated: false,
            run_id: None,
        }
    }

    async fn processed(app: &Arc<App>, bot_id: &str) -> bool {
        let at: Option<String> =
            sqlx::query_scalar("SELECT processed_at FROM hook_events WHERE bot_id = ?").bind(bot_id).fetch_one(&app.db).await.unwrap();
        at.is_some()
    }

    /// #911：A 握著自己的鎖（像啟動等就緒、批次重啟），A 的 Stop 卡在鎖上；B 的 Stop 照常處理完，不排在 A 後面。
    #[tokio::test]
    async fn a_bot_holding_its_lock_does_not_delay_other_bots_hooks() {
        let env = tt::env().await;
        let app = env.app.clone();
        let a = tt::claude_bot(&app, &env.project_id, "inbox-a").await;
        let b = tt::claude_bot(&app, &env.project_id, "inbox-b").await;
        tt::fake_run(&app, &a.id).await;
        tt::fake_run(&app, &b.id).await;
        // A 的列先進來（在 B 前面）：舊的單一消費者會先卡在它身上。
        for body in [stop(&a.id, "pa"), stop(&b.id, "pb")] {
            assert!(crate::hook_inbox::accept(&app.db, &body, crate::hook_inbox::Source::Http).await.unwrap().is_new());
        }
        let lock = app.bot_lock(&a.id).await;
        let held = lock.lock().await;

        let drain_app = app.clone();
        let drain = tokio::spawn(async move { drain_once(&drain_app).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !processed(&app, &b.id).await {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("A 握著鎖時 B 的 hook 仍要在 2 秒內處理完");
        assert!(!processed(&app, &a.id).await, "A 的還卡在它的鎖上");
        assert!(!drain.is_finished());

        drop(held);
        let n = tokio::time::timeout(Duration::from_secs(5), drain).await.expect("放鎖後 A 的也處理完").unwrap().unwrap();
        assert_eq!(n, 2);
        assert!(processed(&app, &a.id).await);
    }
}
