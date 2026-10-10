//! `hook_inbox` 背景工作與 drain_once runner。

use crate::app_ports_r2a9::HookProcessor;
use crate::db;
use crate::hook_body::HookBody;
use crate::hook_inbox::{
    mark_dead, mark_done, mark_failed, pending_excluding, prune, unparseable_body_reason, Pending,
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
    // panic 之後由 supervisor 退避重啟（#924）；睡覺時看 shutdown。
    crate::background_loop::spawn_restartable(&app, "hook inbox worker", {
        let app = app.clone();
        move || {
            let app = app.clone();
            async move {
        let shutdown = app.shutdown.clone();
        loop {
            #[cfg(test)]
            {
                if PANIC_NEXT_TICK.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    panic!("test-injected hook inbox tick panic");
                }
                TICKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
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
                _ = shutdown.cancelled() => return,
                _ = app.hook_inbox_wake.notified() => {}
                _ = tokio::time::sleep(POLL_EVERY) => {}
            }
        }
            }
        }
    });
}

/// 測試用：讓下一輪 tick 在最前面 panic 一次（驗證 supervisor 會重啟、後面的輪次照常），以及數 tick 次數。
#[cfg(test)]
pub(crate) static PANIC_NEXT_TICK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
pub(crate) static TICKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 同時處理幾顆 bot 的列（每顆 bot 一個 task）。
const MAX_BOT_TASKS: usize = 8;

/// 正在被某個 task 處理的 bot：同一顆 bot 不會有兩個 task 同時吃它的列（組內要照 rowid 順序）。
fn busy_bots() -> &'static std::sync::Mutex<HashSet<String>> {
    static BUSY: OnceLock<std::sync::Mutex<HashSet<String>>> = OnceLock::new();
    BUSY.get_or_init(Default::default)
}

/// 目前有 task 在處理的 bot。撈列時要從 SQL 排除：它的積壓會佔滿整批 [`BATCH`]，別顆 bot 的列就撈不到（#1066）。
fn busy_bot_ids() -> Vec<String> {
    busy_bots().lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
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

/// 沒有新事件、也沒有 task 做完時，每隔這段時間再撈一次（退避中的列要等 `next_attempt_at` 到）。
const RESCAN_EVERY: Duration = Duration::from_secs(1);

/// 處理到沒有可以做的列為止。回傳這一輪真的處理完幾列。
///
/// 每顆 bot 一個 task（同時最多 [`MAX_BOT_TASKS`] 顆），組內照寫入順序（#911）。#1002：task 跨輪保留，
/// 等的是「任一個 task 做完、有新事件、或 [`RESCAN_EVERY`]」，而不是等整批做完才回頭撈——一顆 bot 握著鎖
/// （啟動等就緒、批次重啟）時，別顆 bot 在那之後才到的 Stop hook 也不會排在它後面。
pub async fn drain_once(app: &Arc<App>) -> Result<usize> {
    let mut done = 0usize;
    let mut first_err: Option<anyhow::Error> = None;
    let mut tasks: JoinSet<Result<usize>> = JoinSet::new();
    loop {
        // 出錯之後不再起新的 task：手上的收完就回傳第一個錯誤（同舊行為）。
        if first_err.is_none() && tasks.len() < MAX_BOT_TASKS {
            match pending_excluding(&app.db, &db::now(), BATCH, &busy_bot_ids()).await {
                Ok(rows) => start_tasks(app, rows, &mut tasks),
                Err(e) => first_err = Some(e),
            }
        }
        if tasks.is_empty() {
            break;
        }
        tokio::select! {
            Some(res) = tasks.join_next() => settle(res, &mut done, &mut first_err),
            _ = app.hook_inbox_wake.notified() => {}
            _ = tokio::time::sleep(RESCAN_EVERY) => {}
        }
    }
    first_err.map_or(Ok(done), Err)
}

/// 替這一批裡沒人在處理的 bot 起 task（最多到 [`MAX_BOT_TASKS`] 顆）。被別人佔著的 bot 跳過，下一輪再看。
fn start_tasks(app: &Arc<App>, rows: Vec<Pending>, tasks: &mut JoinSet<Result<usize>>) {
    let mut groups: Vec<(String, Vec<Pending>)> = Vec::new();
    for row in rows {
        match groups.iter_mut().find(|(bot, _)| *bot == row.bot_id) {
            Some((_, g)) => g.push(row),
            None => groups.push((row.bot_id.clone(), vec![row])),
        }
    }
    for (bot_id, rows) in groups {
        if tasks.len() >= MAX_BOT_TASKS {
            break;
        }
        // 別的 task（或別的 drain）正在處理這顆 bot：不搶，等它做完。
        let Some(claim) = BotClaim::try_new(&bot_id) else { continue };
        tasks.spawn(drain_bot(app.clone(), rows, claim));
    }
}

fn settle(res: Result<Result<usize>, tokio::task::JoinError>, done: &mut usize, first_err: &mut Option<anyhow::Error>) {
    match res {
        Ok(Ok(n)) => *done += n,
        Ok(Err(e)) => {
            first_err.get_or_insert(e);
        }
        Err(e) => {
            first_err.get_or_insert(anyhow::anyhow!("hook inbox task failed: {e}"));
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
        // 上限 30 秒：被等的是一串 sqlx 查詢，整樹平行時 2 秒會假紅（#1154）。放寬不會變假綠：A 的鎖握到下面才放。
        assert!(tt::eventually!(processed(&app, &b.id).await), "A 握著鎖時 B 的 hook 仍要處理完（不是排在 A 後面）");
        assert!(!processed(&app, &a.id).await, "A 的還卡在它的鎖上");
        assert!(!drain.is_finished());

        // 前提：A 的鎖整段都還握著（不是因為等久了 A 也好了）。
        assert!(lock.try_lock().is_err(), "前提：A 的鎖整段都還握著");
        drop(held);
        let n = tokio::time::timeout(Duration::from_secs(30), drain).await.expect("放鎖後 A 的也處理完").unwrap().unwrap();
        assert_eq!(n, 2);
        assert!(processed(&app, &a.id).await);
    }
    /// #1002：drain 已經在等 A（A 握著鎖）之後，B 才到的 Stop hook 不能排在 A 後面；要靠 wake 或退避重掃撈到它。
    #[tokio::test]
    async fn a_hook_arriving_while_another_bot_is_stuck_is_not_delayed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let a = tt::claude_bot(&app, &env.project_id, "inbox-stuck-a").await;
        let b = tt::claude_bot(&app, &env.project_id, "inbox-stuck-b").await;
        tt::fake_run(&app, &a.id).await;
        tt::fake_run(&app, &b.id).await;
        assert!(crate::hook_inbox::accept(&app.db, &stop(&a.id, "pa"), crate::hook_inbox::Source::Http).await.unwrap().is_new());

        let lock = app.bot_lock(&a.id).await;
        let held = lock.lock().await;
        let drain_app = app.clone();
        let drain = tokio::spawn(async move { drain_once(&drain_app).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!drain.is_finished(), "A 握著鎖，drain 還在等它");

        // 這時才到的 B：照 receive 的順序寫進收件匣，再叫醒。
        assert!(crate::hook_inbox::accept(&app.db, &stop(&b.id, "pb"), crate::hook_inbox::Source::Http).await.unwrap().is_new());
        app.hook_inbox_wake.notify_one();
        assert!(tt::eventually!(processed(&app, &b.id).await), "A 還握著鎖時，後到的 B 仍要處理完");
        assert!(!processed(&app, &a.id).await, "A 還卡在它的鎖上");
        assert!(!drain.is_finished());

        // 前提：A 的鎖整段都還握著（不是因為等久了 A 也好了）。
        assert!(lock.try_lock().is_err(), "前提：A 的鎖整段都還握著");
        drop(held);
        let n = tokio::time::timeout(Duration::from_secs(30), drain).await.expect("放鎖後 A 也處理完").unwrap().unwrap();
        assert_eq!(n, 2);
        assert!(processed(&app, &a.id).await);
    }

    /// #1066：A 有 130 列（超過一批的 [`BATCH`]，全排在別人前面）、A 握著鎖；其餘 7 顆 bot 各一列在後面。
    /// 以前每次只撈前 64 列，全是 A 的，A 被佔著時撈不到別人的列，就永遠卡住；現在 A 已被佔用，就從 SQL 排除。
    #[tokio::test]
    async fn a_bot_with_a_backlog_past_the_batch_limit_does_not_hide_other_bots_hooks() {
        let env = tt::env().await;
        let app = env.app.clone();
        let a = tt::claude_bot(&app, &env.project_id, "inbox-backlog-a").await;
        tt::fake_run(&app, &a.id).await;
        let mut others = Vec::new();
        for i in 0..7 {
            let b = tt::claude_bot(&app, &env.project_id, &format!("inbox-backlog-b{i}")).await;
            tt::fake_run(&app, &b.id).await;
            others.push(b.id);
        }
        for i in 0..(2 * BATCH + 2) {
            assert!(crate::hook_inbox::accept(&app.db, &stop(&a.id, &format!("pa{i}")), crate::hook_inbox::Source::Http).await.unwrap().is_new());
        }
        for (i, b) in others.iter().enumerate() {
            assert!(crate::hook_inbox::accept(&app.db, &stop(b, &format!("pb{i}")), crate::hook_inbox::Source::Http).await.unwrap().is_new());
        }

        let lock = app.bot_lock(&a.id).await;
        let held = lock.lock().await;
        let drain_app = app.clone();
        let drain = tokio::spawn(async move { drain_once(&drain_app).await });
        for b in &others {
            assert!(tt::eventually!(processed(&app, b).await), "A 握著鎖、A 還有一整批以上的積壓時，其他 bot 的 hook 仍要處理完");
        }
        assert!(!processed(&app, &a.id).await, "A 還卡在它的鎖上");
        assert!(!drain.is_finished());

        drop(held);
        tokio::time::timeout(Duration::from_secs(30), drain).await.expect("放鎖後 A 也處理完").unwrap().unwrap();
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hook_events WHERE processed_at IS NULL").fetch_one(&app.db).await.unwrap();
        assert_eq!(left, 0, "A 的 130 列也要全部處理完");
    }
}
