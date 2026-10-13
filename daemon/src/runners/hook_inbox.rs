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

/// 測試用：每列處理完（`mark_done`）就發一則它的 id。測試等的是這個訊號，不是輪詢 DB 或牆鐘。
#[cfg(test)]
pub(crate) fn done_tx() -> &'static tokio::sync::broadcast::Sender<String> {
    static DONE: OnceLock<tokio::sync::broadcast::Sender<String>> = OnceLock::new();
    DONE.get_or_init(|| tokio::sync::broadcast::channel(4096).0)
}

#[cfg(test)]
fn signal_done(id: &str) {
    let _ = done_tx().send(id.to_string());
}

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
                    #[cfg(test)]
                    signal_done(&row.id);
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
    let mut claims: JoinSet<Result<usize>> = JoinSet::new();
    loop {
        // 出錯之後不再起新的 task：手上的收完就回傳第一個錯誤（同舊行為）。
        // 撈列每輪都做：撞題 lane 的列不占 bot task 名額，bot 名額滿了也要看得到撞題列以外的列。
        if first_err.is_none() {
            match pending_excluding(&app.db, &db::now(), BATCH, &busy_bot_ids(), &claims_in_flight_ids()).await {
                Ok(rows) => start_tasks(app, rows, &mut tasks, &mut claims),
                Err(e) => first_err = Some(e),
            }
        }
        if tasks.is_empty() && claims.is_empty() {
            break;
        }
        tokio::select! {
            Some(res) = tasks.join_next() => settle(res, &mut done, &mut first_err),
            Some(res) = claims.join_next() => settle(res, &mut done, &mut first_err),
            _ = app.hook_inbox_wake.notified() => {}
            _ = tokio::time::sleep(RESCAN_EVERY) => {}
        }
    }
    first_err.map_or(Ok(done), Err)
}

/// 替這一批裡沒人在處理的 bot 起 task（最多到 [`MAX_BOT_TASKS`] 顆）。被別人佔著的 bot 跳過，下一輪再看。
/// 撞題檢查不進 bot 的組，交給撞題 lane（#1626）。
fn start_tasks(app: &Arc<App>, rows: Vec<Pending>, tasks: &mut JoinSet<Result<usize>>, claims: &mut JoinSet<Result<usize>>) {
    let mut groups: Vec<(String, Vec<Pending>)> = Vec::new();
    for row in rows {
        if let Some(body) = claim_check_body(&row) {
            spawn_claim_check(app, claims, row, body);
            continue;
        }
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

/// 撞題檢查（#1626）：獨立 lane，同時最多 [`MAX_CLAIM_CHECKS`] 個在問 Jev。不占 bot 的組，也不占 [`MAX_BOT_TASKS`] 名額。
/// 列在收件匣裡維持未 processed，檢查結束才 `mark_done`（#1500）；換版中斷留給重播。
const MAX_CLAIM_CHECKS: usize = 4;

fn claim_checks() -> &'static tokio::sync::Semaphore {
    static SLOTS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(MAX_CLAIM_CHECKS))
}

/// 已經交給 lane、還沒 ACK 的撞題列。撈列時排除它們（否則它們會佔滿整批 [`BATCH`]，後面的列撈不到），也不重派。
fn claims_in_flight() -> &'static std::sync::Mutex<HashSet<String>> {
    static IN_FLIGHT: OnceLock<std::sync::Mutex<HashSet<String>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

fn claims_in_flight_ids() -> Vec<String> {
    claims_in_flight().lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
}

/// task 結束（成功、失敗或被取消）就放掉 in-flight 標記。
struct InFlight(String);

impl Drop for InFlight {
    fn drop(&mut self) {
        claims_in_flight().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// 撞題列才解：先用子字串預篩，免得每列都 parse。解不開或不是撞題的照常走 bot 的組。
fn claim_check_body(row: &Pending) -> Option<HookBody> {
    if !row.body_json.contains(crate::hookrecv::CLAIM_COLLISION_EVENT) {
        return None;
    }
    serde_json::from_str::<HookBody>(&row.body_json).ok().filter(crate::hookrecv::is_claim_collision)
}

/// 把一列撞題檢查交給撞題 lane；已經在跑的不重派。
/// 成功才 `mark_done`，失敗記退避（同 bot 的組規則）；DB 寫不進去就回錯，跟 bot 的列一樣由 drain 回報。
fn spawn_claim_check(app: &Arc<App>, claims: &mut JoinSet<Result<usize>>, row: Pending, body: HookBody) {
    if !claims_in_flight().lock().unwrap_or_else(|e| e.into_inner()).insert(row.id.clone()) {
        return;
    }
    let app = app.clone();
    claims.spawn(async move {
        let _in_flight = InFlight(row.id.clone());
        let _permit = claim_checks().acquire().await?;
        match app.process_hook(&body, Some(&row.id)).await {
            Ok(()) => {
                mark_done(&app.db, &row.id).await?;
                #[cfg(test)]
                signal_done(&row.id);
                Ok(1)
            }
            Err(e) => {
                let attempts = row.attempts + 1;
                tracing::warn!(id = %row.id, attempts, error = ?e, "claim check failed; will retry");
                mark_failed(&app.db, &row.id, attempts, &format!("{e:#}")).await?;
                Ok(0)
            }
        }
    });
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

    /// 遠端 claim 的 hook 必須等撞題檢查完成才可以 ACK。
    /// gate 卡在 Jev 回覆前，模擬 daemon 在工作中途被 swap；重啟先把 reservation 收成
    /// `interrupted`，再由 hook inbox 重播時保留這筆可追溯記錄。
    #[tokio::test]
    async fn claim_collision_hook_stays_pending_until_reserved_work_is_reconciled() {
        let env = tt::env().await;
        let app = env.app.clone();
        let caller = tt::claude_bot(&app, &env.project_id, "claim-inbox-caller").await;
        let existing = tt::claude_bot(&app, &env.project_id, "claim-inbox-existing").await;
        let run_id = tt::fake_run(&app, &existing.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?")
            .bind("正在修同一個 issue 1500 的撞題提醒")
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();

        let dir = tt::track(std::env::temp_dir().join(format!("am-hook-claim-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let key_file = dir.join("key");
        std::fs::write(&key_file, "test-key\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let request_received = Arc::new(tokio::sync::Notify::new());
        let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let response_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let route = {
            let request_received = request_received.clone();
            let request_count = request_count.clone();
            let response_gate = response_gate.clone();
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let request_received = request_received.clone();
                let request_count = request_count.clone();
                let response_gate = response_gate.clone();
                async move {
                    request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    request_received.notify_one();
                    response_gate.acquire().await.unwrap().forget();
                    let _ = body;
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({
                            "model": "jev-test",
                            "answers": {"same_work": {"type": "noul", "noul": 0.8}},
                            "usage": {"input_tokens": 1}
                        })),
                    )
                }
            })
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap();
        });
        let project_id = env.project_id.clone();
        let key_path = key_file.to_string_lossy().into_owned();
        app.cfg
            .update(move |cfg| {
                cfg.judge.enabled = true;
                cfg.judge.projects = vec![project_id];
                cfg.judge.key_file = key_path;
                cfg.judge.endpoint = endpoint;
                cfg.judge.timeout_ms = 60_000;
                Ok(())
            })
            .await
            .unwrap();

        let body = HookBody {
            bot_id: caller.id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "AmClaimCollisionCheck",
                "issue": 1500,
                "title": "修正遠端 claim 撞題提醒的 durable handoff",
                "child": caller.name,
                "branch": "fix/pm1500",
            }),
            received_at: Some("2026-10-12T00:00:00.000Z".into()),
            truncated: false,
            run_id: None,
        };
        let accepted = crate::hook_inbox::accept(&app.db, &body, crate::hook_inbox::Source::Remote).await.unwrap();
        assert!(accepted.is_new());
        let event_id: String = sqlx::query_scalar("SELECT id FROM hook_events WHERE bot_id=?")
            .bind(&caller.id)
            .fetch_one(&app.db)
            .await
            .unwrap();

        let request_wait = request_received.notified();
        let mut processing = Box::pin(crate::app_ports_r2a9::HookProcessor::process_hook(&app, &body, Some(&event_id)));
        tokio::select! {
            _ = request_wait => {}
            result = &mut processing => panic!("hook returned before the gated Jev request: {result:?}"),
        }

        let processed_at: Option<String> = sqlx::query_scalar("SELECT processed_at FROM hook_events WHERE id=?")
            .bind(&event_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(processed_at.is_none(), "Jev gate 尚未放行，hook 不得 ACK");
        let reservation: (Option<f64>, Option<String>) = sqlx::query_as(
            "SELECT jev_same_work, error FROM judge_shadow WHERE regex_verdict='same_work'",
        )
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(reservation, (None, Some("pending".into())), "先有可恢復的 reservation 才能進入 Jev");

        // 丟棄中的 hook future 等同 swap 取消正在處理的工作；收件匣列尚未 ACK。
        drop(processing);
        assert_eq!(crate::judge::settle_interrupted(&app).await.unwrap(), 1);
        let interrupted: Option<String> = sqlx::query_scalar("SELECT error FROM judge_shadow WHERE regex_verdict='same_work'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(interrupted.as_deref(), Some("interrupted"));

        // 讓被取消的假 Jev handler 收尾，再模擬新 daemon 的 inbox worker 重播。
        response_gate.add_permits(1);
        assert_eq!(drain_once(&app).await.unwrap(), 1);
        assert!(processed(&app, &caller.id).await, "重播完成後 ACK hook");
        assert_eq!(request_count.load(std::sync::atomic::Ordering::SeqCst), 1, "保留 interrupted 記錄，不重複消耗 Jev 名額");
        assert!(
            crate::supervisor::store::pending_inbox(&app.db).await.unwrap().iter().all(|e| e.kind != "judge_same_work"),
            "interrupted 前沒有答案，不推撞題提示"
        );
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

    /// 假 Jev：每個請求到了就送一則到 `arrivals`（測試據此知道「撞題查詢已經送到 Jev」，不靠 sleep），
    /// 再等 `gate` 放行才回答 same_work 0.8。
    struct GatedJev {
        arrivals: tokio::sync::mpsc::UnboundedReceiver<()>,
        gate: Arc<tokio::sync::Semaphore>,
    }

    /// 開啟撞題判斷、接到假 Jev。key 檔放在測試暫存目錄，不碰真的 HOME；每小時上限拉高，
    /// 免得超過預設的 60 次後查詢提早結束，就測不到「慢查詢」。
    async fn gated_jev(app: &Arc<App>, project_id: &str) -> GatedJev {
        let dir = tt::track(std::env::temp_dir().join(format!("am-hook-claim-gate-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let key_file = dir.join("key");
        std::fs::write(&key_file, "test-key\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let (arrivals_tx, arrivals) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let route = {
            let gate = gate.clone();
            axum::routing::post(move |axum::Json(_body): axum::Json<serde_json::Value>| {
                let arrivals_tx = arrivals_tx.clone();
                let gate = gate.clone();
                async move {
                    let _ = arrivals_tx.send(());
                    gate.acquire().await.unwrap().forget();
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({
                            "model": "jev-test",
                            "answers": {"same_work": {"type": "noul", "noul": 0.8}},
                            "usage": {"input_tokens": 1}
                        })),
                    )
                }
            })
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap();
        });
        let project_id = project_id.to_string();
        let key_path = key_file.to_string_lossy().into_owned();
        app.cfg
            .update(move |cfg| {
                cfg.judge.enabled = true;
                cfg.judge.projects = vec![project_id];
                cfg.judge.key_file = key_path;
                cfg.judge.endpoint = endpoint;
                cfg.judge.timeout_ms = 60_000;
                cfg.judge.max_per_hour = 1_000;
                Ok(())
            })
            .await
            .unwrap();
        GatedJev { arrivals, gate }
    }

    /// 等下一個請求送到假 Jev。上限只是失敗時不掛住（不用來決定先後）。
    async fn next_arrival(jev: &mut GatedJev) {
        let got = tokio::time::timeout(Duration::from_secs(30), jev.arrivals.recv()).await;
        assert!(matches!(got, Ok(Some(()))), "撞題查詢應該已經送到 Jev");
    }

    /// 等 `id` 那一列處理完（收件匣的完成訊號）。上限只是失敗時不掛住（不用來決定先後）。
    async fn wait_done(done: &mut tokio::sync::broadcast::Receiver<String>, id: &str) {
        let found = tokio::time::timeout(Duration::from_secs(30), async {
            while let Ok(got) = done.recv().await {
                if got == id {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(matches!(found, Ok(true)), "列 {id} 應該處理完");
    }

    /// 一顆正在跑的同專案 bot：撞題查詢的候選。
    async fn running_peer(app: &Arc<App>, project_id: &str, name: &str, title: &str) {
        let peer = tt::claude_bot(app, project_id, name).await;
        let run_id = tt::fake_run(app, &peer.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?").bind(title).bind(&run_id).execute(&app.db).await.unwrap();
    }

    /// 遠端 spool 來的撞題檢查。`received_at` 每票不同，才不會被當成重播。
    fn claim(bot_id: &str, child: &str, issue: u64) -> HookBody {
        HookBody {
            bot_id: bot_id.into(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "AmClaimCollisionCheck",
                "issue": issue,
                "title": format!("修正遠端 claim 撞題提醒 {issue}"),
                "child": child,
                "branch": format!("fix/pm{issue}"),
            }),
            received_at: Some(format!("2026-10-13T00:00:00.000Z-{issue}")),
            truncated: false,
            run_id: None,
        }
    }

    /// 同一顆 bot 的收件匣事件，照寫入順序（rowid）。
    async fn event_ids(app: &Arc<App>, bot_id: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT id FROM hook_events WHERE bot_id=? ORDER BY rowid").bind(bot_id).fetch_all(&app.db).await.unwrap()
    }

    async fn processed_at(app: &Arc<App>, event_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT processed_at FROM hook_events WHERE id=?").bind(event_id).fetch_one(&app.db).await.unwrap()
    }

    /// #1626：撞題查詢卡在 Jev 上（還沒回）時，不得占住 bot 鎖，也不得擋住同一顆 bot 之後到的 Stop；
    /// 檢查沒完成前，撞題那一列仍不 ACK（#1500）。
    #[tokio::test]
    async fn a_slow_claim_check_does_not_hold_the_bot_lock_or_delay_its_stop() {
        let env = tt::env().await;
        let app = env.app.clone();
        let caller = tt::claude_bot(&app, &env.project_id, "slow-claim-caller").await;
        tt::fake_run(&app, &caller.id).await;
        running_peer(&app, &env.project_id, "slow-claim-peer", "正在做別的事").await;
        let mut jev = gated_jev(&app, &env.project_id).await;
        let mut done = done_tx().subscribe();

        assert!(crate::hook_inbox::accept(&app.db, &claim(&caller.id, &caller.name, 1626), crate::hook_inbox::Source::Remote).await.unwrap().is_new());
        assert!(crate::hook_inbox::accept(&app.db, &stop(&caller.id, "pa"), crate::hook_inbox::Source::Http).await.unwrap().is_new());
        let ids = event_ids(&app, &caller.id).await;
        let (claim_id, stop_id) = (ids[0].clone(), ids[1].clone());

        let drain_app = app.clone();
        let drain = tokio::spawn(async move { drain_once(&drain_app).await });
        next_arrival(&mut jev).await;
        wait_done(&mut done, &stop_id).await;
        let lock = app.bot_lock(&caller.id).await;
        assert!(lock.try_lock().is_ok(), "撞題查詢不得占住 bot 鎖");
        drop(lock);
        assert!(processed_at(&app, &claim_id).await.is_none(), "檢查沒完成前不 ACK（#1500）");

        jev.gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(30), drain).await.expect("放行後 drain 結束").unwrap().unwrap();
        assert!(processed_at(&app, &claim_id).await.is_some(), "放行後檢查完成才 ACK");
    }

    /// #1626：八顆 bot 的撞題查詢都卡在 Jev 上（以前會占住八個 bot task 名額），第九顆 bot 的 Stop 仍要處理完。
    #[tokio::test]
    async fn slow_claim_checks_do_not_take_the_hook_workers_from_other_bots() {
        let env = tt::env().await;
        let app = env.app.clone();
        running_peer(&app, &env.project_id, "slow-claim-peer", "正在做別的事").await;
        let mut jev = gated_jev(&app, &env.project_id).await;
        let mut done = done_tx().subscribe();

        let mut claim_ids = Vec::new();
        for i in 0..8 {
            let bot = tt::claude_bot(&app, &env.project_id, &format!("slow-claim-{i}")).await;
            assert!(crate::hook_inbox::accept(&app.db, &claim(&bot.id, &bot.name, 1700 + i), crate::hook_inbox::Source::Remote).await.unwrap().is_new());
            claim_ids.push(event_ids(&app, &bot.id).await.remove(0));
        }
        let quiet = tt::claude_bot(&app, &env.project_id, "slow-claim-quiet").await;
        tt::fake_run(&app, &quiet.id).await;
        assert!(crate::hook_inbox::accept(&app.db, &stop(&quiet.id, "pq"), crate::hook_inbox::Source::Http).await.unwrap().is_new());
        let quiet_stop = event_ids(&app, &quiet.id).await.remove(0);

        let drain_app = app.clone();
        let drain = tokio::spawn(async move { drain_once(&drain_app).await });
        // 撞題 lane 同時最多 4 個在問 Jev：等其中四個到了，其餘的八顆 bot 都還卡著。
        for _ in 0..4 {
            next_arrival(&mut jev).await;
        }
        wait_done(&mut done, &quiet_stop).await;

        jev.gate.add_permits(1000);
        tokio::time::timeout(Duration::from_secs(30), drain).await.expect("放行後 drain 結束").unwrap().unwrap();
        for id in &claim_ids {
            assert!(processed_at(&app, id).await.is_some(), "放行後每顆 bot 的撞題查詢都要 ACK");
        }
    }

    /// #1626：撞題列一批就佔滿 [`BATCH`]，而且全是不同 bot 的慢查詢（每顆 bot 只有一列，互相沒有忙碌排除），
    /// 排在後面的別顆 bot 的 Stop 不能被擠出撈列的窗口。已經交給撞題 lane 的列要從撈列的 SQL 排除。
    /// 在修正前：八顆 bot 的組占滿 bot task，其餘的撞題列每輪都撈回同一批，Stop 永遠排不到。
    #[tokio::test]
    async fn slow_claim_checks_across_many_bots_do_not_hide_a_later_bots_hooks() {
        let env = tt::env().await;
        let app = env.app.clone();
        running_peer(&app, &env.project_id, "starve-claim-peer", "正在做別的事").await;
        let mut jev = gated_jev(&app, &env.project_id).await;
        let mut done = done_tx().subscribe();

        let mut claim_ids = Vec::new();
        for i in 0..(BATCH + 16) {
            let bot = tt::claude_bot(&app, &env.project_id, &format!("starve-claim-{i}")).await;
            assert!(crate::hook_inbox::accept(&app.db, &claim(&bot.id, &bot.name, 2000 + i as u64), crate::hook_inbox::Source::Remote).await.unwrap().is_new());
            claim_ids.push(event_ids(&app, &bot.id).await.remove(0));
        }
        let quiet = tt::claude_bot(&app, &env.project_id, "starve-claim-quiet").await;
        tt::fake_run(&app, &quiet.id).await;
        assert!(crate::hook_inbox::accept(&app.db, &stop(&quiet.id, "pq"), crate::hook_inbox::Source::Http).await.unwrap().is_new());
        let quiet_stop = event_ids(&app, &quiet.id).await.remove(0);

        let drain_app = app.clone();
        let drain = tokio::spawn(async move { drain_once(&drain_app).await });
        next_arrival(&mut jev).await;
        wait_done(&mut done, &quiet_stop).await;

        jev.gate.add_permits(1000);
        tokio::time::timeout(Duration::from_secs(30), drain).await.expect("放行後 drain 結束").unwrap().unwrap();
        for id in &claim_ids {
            assert!(processed_at(&app, id).await.is_some(), "放行後每列撞題都要 ACK");
        }
    }

    /// #1626：Jev 卡住期間刪掉 caller，放行後不得寫入撞題提示。撞題 lane 不再持有 bot 鎖，
    /// 所以刪除不會等它；答案回來時 caller 已經不在，結果要丟掉（帳本記下原因、不留答案，也不推提示）。
    #[tokio::test]
    async fn a_claim_whose_bot_is_deleted_during_the_jev_check_writes_no_hint() {
        let env = tt::env().await;
        let app = env.app.clone();
        let caller = tt::claude_bot(&app, &env.project_id, "deleted-claim-caller").await;
        // child 由母 agent 開：刪除走 SQL 軟刪，不查 config.toml（user 管理的那種刪除要比對設定檔）。
        sqlx::query("UPDATE bots SET managed_by='child' WHERE id=?").bind(&caller.id).execute(&app.db).await.unwrap();
        running_peer(&app, &env.project_id, "deleted-claim-peer", "正在做同一件事").await;
        let mut jev = gated_jev(&app, &env.project_id).await;
        let mut done = done_tx().subscribe();

        assert!(crate::hook_inbox::accept(&app.db, &claim(&caller.id, &caller.name, 1800), crate::hook_inbox::Source::Remote).await.unwrap().is_new());
        let claim_id = event_ids(&app, &caller.id).await.remove(0);

        let drain_app = app.clone();
        let drain = tokio::spawn(async move { drain_once(&drain_app).await });
        next_arrival(&mut jev).await;
        // Jev 還沒回：刪 caller。失敗時（例如鎖死）這裡會被上限擋下，不會掛住。
        tokio::time::timeout(Duration::from_secs(30), crate::api::delete_bot(axum::extract::State(app.clone()), axum::extract::Path(caller.id.clone())))
            .await
            .expect("Jev 等待中刪 bot 不得被撞題查詢擋住")
            .unwrap();

        jev.gate.add_permits(1);
        wait_done(&mut done, &claim_id).await;
        tokio::time::timeout(Duration::from_secs(30), drain).await.expect("放行後 drain 結束").unwrap().unwrap();

        let hints: Vec<String> = crate::supervisor::store::pending_inbox(&app.db)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "judge_same_work")
            .map(|e| e.event_key)
            .collect();
        assert!(hints.is_empty(), "caller 已刪除，不得寫入撞題提示：{hints:?}");
        let (answer, error): (Option<f64>, Option<String>) =
            sqlx::query_as("SELECT jev_same_work, error FROM judge_shadow WHERE regex_verdict='same_work'").fetch_one(&app.db).await.unwrap();
        assert_eq!(error.as_deref(), Some("bot_gone"), "帳本記下丟掉的原因");
        assert_eq!(answer, None, "丟掉的結果不留答案");
    }
}
