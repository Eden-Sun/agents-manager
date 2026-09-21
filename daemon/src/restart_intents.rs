//! 開機補完被打斷的重啟（#355 P2，設計見該票）。
//!
//! 重啟＝停舊 run（記 `stopping`→`stopped`）再起新 run。兩步之間 daemon 死掉，舊 run 留著 `stopped`（＝使用者要它停），沒有新 run，
//! 開機對帳看到的是一顆「使用者停掉的」bot，autostart／`bot_stopped` 探針都不會拉回（#354）。`restart_bot_with` 在**記 `stopping` 之前**
//! 先 commit 一件 `restart` intent；這裡在開機（與主機重連、對帳成功之後、autostart 判斷之前）讀還開著的 intent，檢查世界決定：
//!
//! | 世界 | 判斷 |
//! |---|---|
//! | 有新的 active run（不是 intent 記的那顆） | 已經回來了 → `done` |
//! | 舊 run 還 running／starting（`stopping` 從沒記過） | 承諾點之前 → `abandoned`，什麼都沒變 |
//! | 舊 run 在 `stopping` | 停到一半 → 補完停、改標 `exited`、起 → `done` |
//! | 沒有 active run（舊 run `stopped`／`exited`，或本來就沒有 run） | 改標 `exited`（**不是**使用者要它停）、起 → `done` |
//!
//! 使用者裁示（2026-09-20）：被打斷的都**往前補完**（`autostart=0` 的也補）；補不成最多試 [`crate::intents::MAX_ATTEMPTS`] 次，
//! 用完就 `failed` 並在同一個交易推 AGM inbox（`intent_failed`）。讀不到（DB／herdr）＝這次失敗、之後重試，不當成「不用做」。

use crate::db;
use crate::intents::{self, Intent};
use crate::lifecycle::{self, LcError, StartOpts};
use crate::state::App;
use std::sync::Arc;

/// 一次補做的結果。
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// intent 已經收尾（done／abandoned／被別人收了／已 failed）。
    Finished,
    /// 這次沒補成（原因在裡面），下一輪再試；已用完次數的話已經 failed＋通知。
    Retry(String),
}

/// 主機（本機開機，或遠端連上並對帳成功）之後：先收過期的，再把這台主機上還開著的 `restart` intent 各補一次（**內嵌**——
/// 呼叫端接著要做 autostart 判斷，必須先解決）；補不成的丟背景以 `recovery_retry_delay` 重試到用完次數。
pub async fn recover_host(app: &Arc<App>, host: &str) {
    match intents::expire_overdue(&app.db, &db::now()).await {
        Ok(expired) => {
            for i in expired {
                tracing::error!(intent = %i.id, kind = %i.kind, subject = %i.subject_id, "intent expired before it could be completed; reported to AGM");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not expire overdue intents"),
    }
    if let Err(e) = intents::sweep_finished(&app.db).await {
        tracing::warn!(error = %e, "could not sweep finished intents");
    }
    let open = match intents::open(&app.db).await {
        Ok(v) => {
            // 過期被收掉的、或別人收尾的：它們的開機 hold 不用再留（#378）。
            lifecycle::restart_hold::retain_open(app, &v.iter().map(|i| i.id.clone()).collect());
            v
        }
        Err(e) => {
            // 讀不到不等於沒有：稍後整輪重來。
            tracing::warn!(error = %e, host, "cannot list open intents yet; will retry");
            let (app, host) = (app.clone(), host.to_string());
            tokio::spawn(async move {
                for attempt in 0..intents::MAX_ATTEMPTS as usize {
                    tokio::time::sleep(crate::reconcile::recovery_retry_delay(attempt)).await;
                    if let Ok(v) = intents::open(&app.db).await {
                        for i in v.into_iter().filter(|i| i.kind == "restart" && i.host == host) {
                            drive(&app, &i.id).await;
                        }
                        return;
                    }
                }
                tracing::error!(host, "open intents stayed unreadable; giving up this recovery pass");
            });
            return;
        }
    };
    for i in open.into_iter().filter(|i| i.kind == "restart" && i.host == host) {
        if let Outcome::Retry(why) = drive_once(app, &i.id).await {
            tracing::warn!(intent = %i.id, bot = %i.subject_id, error = %why, "interrupted restart could not be completed yet; retrying in the background");
            let (app, id) = (app.clone(), i.id.clone());
            tokio::spawn(async move { retry_loop(&app, &id).await });
        } else {
            lifecycle::restart_hold::release_intent(app, &i.id);
        }
    }
}

async fn retry_loop(app: &Arc<App>, id: &str) {
    for attempt in 0.. {
        tokio::time::sleep(crate::reconcile::recovery_retry_delay(attempt)).await;
        if drive_once(app, id).await == Outcome::Finished {
            lifecycle::restart_hold::release_intent(app, id);
            return;
        }
    }
}

async fn drive(app: &Arc<App>, id: &str) {
    if let Outcome::Retry(_) = drive_once(app, id).await {
        retry_loop(app, id).await;
    } else {
        lifecycle::restart_hold::release_intent(app, id);
    }
}

/// 認領一次並補一輪。認領不到（別的行程／已收尾）＝ `Finished`。
pub async fn drive_once(app: &Arc<App>, id: &str) -> Outcome {
    match intents::claim(&app.db, id, &app.boot_id).await {
        Ok(true) => {}
        Ok(false) => return Outcome::Finished,
        Err(e) => return Outcome::Retry(format!("cannot claim the intent: {e:#}")),
    }
    let intent = match intents::get(&app.db, id).await {
        Ok(Some(i)) => i,
        Ok(None) => return Outcome::Finished,
        Err(e) => return fail_attempt(app, id, format!("cannot read the intent: {e:#}")).await,
    };
    match resume(app, &intent).await {
        Ok(()) => Outcome::Finished,
        Err(why) => fail_attempt(app, id, why).await,
    }
}

async fn fail_attempt(app: &Arc<App>, id: &str, why: String) -> Outcome {
    match intents::record_failure(&app.db, id, &why).await {
        Ok(true) => {
            tracing::error!(intent = id, error = %why, "interrupted restart could not be completed; gave up and told AGM");
            Outcome::Finished
        }
        Ok(false) => Outcome::Retry(why),
        Err(e) => Outcome::Retry(format!("{why}（且記不下失敗：{e:#}）")),
    }
}

/// 檢查世界、往前補完。`Ok`＝intent 已收尾；`Err(原因)`＝這次沒補成。
async fn resume(app: &Arc<App>, intent: &Intent) -> Result<(), String> {
    let bot_id = intent.subject_id.as_str();
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    // 拿到鎖之後再確認一次：這段等鎖的時間裡活著的 handler 可能已經把它收尾了（遠端重連時 recovery 與正在服務的重啟會碰到）——
    // 已經不是我們認領的那件就什麼都不做，不能在使用者已經拿到錯誤之後又替他補做。
    match intents::get(&app.db, &intent.id).await.map_err(|e| format!("db: {e:#}"))? {
        Some(cur) if cur.status == "running" => {}
        _ => return Ok(()),
    }
    let payload = intent.payload();
    let opts: StartOpts = serde_json::from_value(payload.get("opts").cloned().unwrap_or_default()).unwrap_or_default();
    let from_run = payload.get("from_run_id").and_then(|v| v.as_str());
    let s = |e: anyhow::Error| format!("db: {e:#}");

    let bot = db::bot(&app.db, bot_id).await.map_err(s)?;
    if bot.as_ref().is_none_or(|b| b.deleted_at.is_some()) {
        intents::abandon(&app.db, &intent.id, "bot is gone").await.map_err(|e| format!("{e:#}"))?;
        return Ok(());
    }
    match db::active_run(&app.db, bot_id).await.map_err(s)? {
        // 已經有新的 run（不是 intent 記的那顆）：bot 回來了。
        Some(run) if Some(run.id.as_str()) != from_run => {
            intents::complete(&app.db, &intent.id).await.map_err(|e| format!("{e:#}"))?;
            return Ok(());
        }
        // 舊 run 還好好的、`stopping` 從沒記過：承諾點之前就死了，世界沒變。
        Some(run) if run.state != "stopping" => {
            intents::abandon(&app.db, &intent.id, "the stop never began").await.map_err(|e| format!("{e:#}"))?;
            return Ok(());
        }
        _ => {}
    }
    match lifecycle::resume_restart_locked(app, bot_id, opts, from_run).await {
        Ok(_) => {}
        // 新 agent 起來了、只是 `running` 還沒記下：bot 回來了。
        Err(LcError::Uncommitted(v)) if v.get("start_error").is_none() => {}
        Err(e) => return Err(format!("{e:?}")),
    }
    intents::complete(&app.db, &intent.id).await.map_err(|e| format!("{e:#}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LOCAL_HOST;
    use crate::lifecycle::race_point;
    use crate::testing as tt;
    use serde_json::json;

    async fn runs_of(app: &Arc<App>, bot: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ?").bind(bot).fetch_one(&app.db).await.unwrap()
    }

    async fn run_state(app: &Arc<App>, run: &str) -> String {
        sqlx::query_scalar("SELECT state FROM runs WHERE id = ?").bind(run).fetch_one(&app.db).await.unwrap()
    }

    async fn intent_status(app: &Arc<App>, bot: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT status FROM intents WHERE subject_id = ? ORDER BY created_at").bind(bot).fetch_all(&app.db).await.unwrap()
    }

    /// 模擬行程死亡：重啟走到 `point` 就卡住，然後把整個 future abort 掉（之後不再有任何 await 執行）。
    async fn die_at(e: &tt::Env, bot_id: &str, point: &'static str) {
        let reached = Arc::new(tokio::sync::Notify::new());
        let r2 = reached.clone();
        race_point::arm(point, bot_id, move || async move {
            r2.notify_one();
            std::future::pending::<()>().await
        });
        let (app, id) = (e.app.clone(), bot_id.to_string());
        let h = tokio::spawn(async move { lifecycle::restart_bot_with(&app, &id, StartOpts::default()).await });
        reached.notified().await;
        h.abort();
        let _ = h.await;
    }

    async fn running_bot(e: &tt::Env, name: &str) -> (db::Bot, String) {
        let bot = tt::claude_bot(&e.app, &e.project_id, name).await;
        let run = lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
        (bot, run)
    }

    /// #354：stop 之後、start 之前行程死掉。以前開機看到一顆「使用者停掉的」bot，永遠不拉回；現在開機往前補完
    /// （不管 autostart 是 0 還是 1），舊 run 改標 exited（不是使用者要它停），而且補兩次不會起兩個。
    #[tokio::test]
    async fn a_restart_killed_between_stop_and_start_is_completed_on_boot_exactly_once() {
        let e = tt::env().await;
        let (bot, run1) = running_bot(&e, "victim").await;
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT autostart FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap(), 0, "autostart=0 也要補");

        die_at(&e, &bot.id, "restart_after_stop").await;
        assert!(db::active_run(&e.app.db, &bot.id).await.unwrap().is_none(), "死掉的當下沒有 active run");
        assert_eq!(run_state(&e.app, &run1).await, "stopped", "舊 run 記著 stopped——這就是 #354 的半套");
        assert_eq!(intent_status(&e.app, &bot.id).await, vec!["pending"], "持久的 intent 還開著");

        let app2 = tt::restart_app(&e).await;
        recover_host(&app2, LOCAL_HOST).await;
        let run2 = db::active_run(&app2.db, &bot.id).await.unwrap().expect("開機補完：bot 回來了");
        assert_ne!(run2.id, run1);
        assert_eq!(run_state(&app2, &run1).await, "exited", "不是使用者要它停");
        assert_eq!(intent_status(&app2, &bot.id).await, vec!["done"]);

        // 冪等：再補一次（或兩個一起補）不會多起。
        recover_host(&app2, LOCAL_HOST).await;
        tokio::join!(recover_host(&app2, LOCAL_HOST), recover_host(&app2, LOCAL_HOST));
        assert_eq!(runs_of(&app2, &bot.id).await, 2, "舊的＋補起來的那一顆，沒有第三個");
    }

    /// #378：stop 之後、start 之前死掉，開機時對帳／sweeper 在 recovery 補完之前先跑——排著的派工不能被當孤兒撤掉，
    /// 補完之後（新 run 起來、hold 放掉）它還在佇列。
    #[tokio::test]
    async fn a_queued_prompt_survives_the_gap_between_boot_and_recovery_of_an_interrupted_restart() {
        let e = tt::env().await;
        let (bot, run1) = running_bot(&e, "queued-victim").await;
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工',?)")
            .bind(&turn)
            .bind(&conv)
            .bind(db::now())
            .execute(&e.app.db)
            .await
            .unwrap();
        die_at(&e, &bot.id, "restart_after_stop").await;
        assert!(!lifecycle::restart_hold::in_progress(&bot.id), "行程死了，記憶體 hold 隨 future 一起消失");
        assert_eq!(run_state(&e.app, &run1).await, "stopped");

        let app2 = tt::restart_app(&e).await;
        lifecycle::restart_hold::adopt_open_intents(&app2).await;
        assert!(lifecycle::restart_hold::in_progress(&bot.id), "開機接回 hold");
        // recovery 之前：對帳收尾／回合結束事件／定時 sweeper 的撤孤兒路徑。
        assert!(lifecycle::revoke_orphaned_queued_turns(&app2, &bot.id, "它的 run 已經結束").await.is_empty());
        assert!(lifecycle::revoke_all_orphaned_queued_turns(&app2).await.is_empty());
        let st = |t: String| {
            let app = app2.clone();
            async move { sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?").bind(t).fetch_one(&app.db).await.unwrap() }
        };
        assert_eq!(st(turn.clone()).await, "queued", "recovery 之前沒被撤");

        recover_host(&app2, LOCAL_HOST).await;
        assert!(db::active_run(&app2.db, &bot.id).await.unwrap().is_some(), "recovery 把 bot 補起來");
        assert!(!lifecycle::restart_hold::in_progress(&bot.id), "intent 收尾後 hold 放掉");
        assert_ne!(st(turn.clone()).await, "failed", "排著的派工留給新 run，沒有被撤");
    }

    /// 另一個 DB 的 `recover_host`（測試共用行程；也就是「別人的 intent 表」）不能把我們接回的 hold 放掉：
    /// `retain_open` 只對同一個資料目錄的 intent 下判斷。以前它清掉所有不在「自己那份開著的名單」的 hold，
    /// 整樹平行時另一條測試的 `recover_host` 剛好夾在中間，`a_queued_prompt_survives…` 就偶發紅。
    #[tokio::test]
    async fn another_apps_recovery_never_releases_our_boot_holds() {
        let e = tt::env().await;
        let (bot, _run1) = running_bot(&e, "held").await;
        die_at(&e, &bot.id, "restart_after_stop").await;
        let app2 = tt::restart_app(&e).await;
        lifecycle::restart_hold::adopt_open_intents(&app2).await;
        assert!(lifecycle::restart_hold::in_progress(&bot.id));

        // 另一個完全獨立的 App（自己的 DB、沒有任何開著的 intent）跑一輪 recovery。
        let other = tt::env().await;
        recover_host(&other.app, LOCAL_HOST).await;
        assert!(lifecycle::restart_hold::in_progress(&bot.id), "別人的 recovery 不能放掉我們的 hold");

        // 自己的 recovery 收尾之後才放。
        recover_host(&app2, LOCAL_HOST).await;
        assert!(!lifecycle::restart_hold::in_progress(&bot.id));
    }

    /// 承諾點之前（intent 寫了、stop 還沒記）死掉：世界沒變，abandoned，bot 照舊在跑、不多起。
    #[tokio::test]
    async fn a_restart_killed_before_anything_was_stopped_is_abandoned() {
        let e = tt::env().await;
        let (bot, run1) = running_bot(&e, "untouched").await;
        die_at(&e, &bot.id, "restart_after_intent").await;
        assert_eq!(run_state(&e.app, &run1).await, "running");

        let app2 = tt::restart_app(&e).await;
        recover_host(&app2, LOCAL_HOST).await;
        assert_eq!(intent_status(&app2, &bot.id).await, vec!["abandoned"]);
        assert_eq!(run_state(&app2, &run1).await, "running", "什麼都沒動");
        assert_eq!(runs_of(&app2, &bot.id).await, 1);
    }

    /// stop 做到一半（run 記了 stopping）：補完停、再起。
    #[tokio::test]
    async fn a_restart_killed_in_the_middle_of_the_stop_finishes_the_stop_and_starts() {
        let e = tt::env().await;
        let (bot, run1) = running_bot(&e, "half-stopped").await;
        sqlx::query("UPDATE runs SET state = 'stopping' WHERE id = ?").bind(&run1).execute(&e.app.db).await.unwrap();
        crate::intents::insert(&e.app.db, "restart", &bot.id, LOCAL_HOST, &json!({"opts": {}, "from_run_id": run1}), 900).await.unwrap();

        let app2 = tt::restart_app(&e).await;
        recover_host(&app2, LOCAL_HOST).await;
        let run2 = db::active_run(&app2.db, &bot.id).await.unwrap().expect("補完之後 bot 回來");
        assert_ne!(run2.id, run1);
        assert_eq!(intent_status(&app2, &bot.id).await, vec!["done"]);
    }

    /// 補不成（start 一直失敗）：最多試 MAX_ATTEMPTS 次，然後 failed，並在同一個交易推 AGM inbox（不只寫 log）。
    #[tokio::test]
    async fn a_completion_that_keeps_failing_gives_up_after_a_few_tries_and_tells_agm() {
        let e = tt::env().await;
        let (bot, _run1) = running_bot(&e, "doomed").await;
        die_at(&e, &bot.id, "restart_after_stop").await;
        // 之後 start 一定失敗：身分不存在。
        sqlx::query("UPDATE bots SET identity = 'no-such-identity' WHERE id = ?").bind(&bot.id).execute(&e.app.db).await.unwrap();

        let app2 = tt::restart_app(&e).await;
        recover_host(&app2, LOCAL_HOST).await;
        let mut status = vec![];
        for _ in 0..200 {
            status = intent_status(&app2, &bot.id).await;
            if status == vec!["failed"] {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(status, vec!["failed"], "用完次數就放棄，不無限重試");
        let attempts: i64 = sqlx::query_scalar("SELECT attempts FROM intents WHERE subject_id = ?").bind(&bot.id).fetch_one(&app2.db).await.unwrap();
        assert_eq!(attempts, crate::intents::MAX_ATTEMPTS);
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'intent_failed' AND bot_id = ?")
            .bind(&bot.id)
            .fetch_one(&app2.db)
            .await
            .unwrap();
        assert_eq!(events, 1, "AGM inbox 有一則 intent_failed");
    }

    /// 寫不進 intent＝什麼都還沒動、不能開始重啟（fail closed）：bot 照舊在跑。
    #[tokio::test]
    async fn a_restart_that_cannot_record_its_intent_does_not_stop_anything() {
        let e = tt::env().await;
        let (bot, run1) = running_bot(&e, "guarded").await;
        sqlx::query("CREATE TRIGGER am_test_no_intents BEFORE INSERT ON intents BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&e.app.db)
            .await
            .unwrap();
        assert!(lifecycle::restart_bot_with(&e.app, &bot.id, StartOpts::default()).await.is_err());
        assert_eq!(run_state(&e.app, &run1).await, "running", "沒停任何東西");
    }

    /// handler 活著時的正常重啟：intent 走完就是 done，不留開著的。
    #[tokio::test]
    async fn a_normal_restart_leaves_no_open_intent() {
        let e = tt::env().await;
        let (bot, run1) = running_bot(&e, "normal").await;
        let run2 = lifecycle::restart_bot_with(&e.app, &bot.id, StartOpts::default()).await.unwrap();
        assert_ne!(run1, run2);
        assert_eq!(intent_status(&e.app, &bot.id).await, vec!["done"]);
    }
}
