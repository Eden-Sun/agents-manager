//! 「打斷一個回合」是一個分散式操作（#147）：外面的一顆鍵（網頁 Esc、強制中止的 `esc`），加上 DB 裡把被打斷的
//! 回合收成 `failed`。兩半不在同一個交易裡，所以：
//!
//! - **鍵的結果分三種**（[`KeyFate`]）：herdr 回錯誤或根本連不上＝**沒做**；回 ok＝**做了**；送出去之後逾時、
//!   連線斷了沒回＝**不知道**（[`crate::herdr::never_applied`]）。
//! - **沒做**：什麼都不動，那一筆照舊 in_flight。
//! - **做了**：DB 那一半跟著寫（收成 failed 與說明同一個交易）。寫不進去就**記成欠著**，呼叫端不回普通的成功。
//! - **不知道**：不假定打斷——那一筆留在 in_flight，**記成待證**；之後的證據（那次 Esc 的 `StopFailure` 回聲）
//!   說鍵生效了才收。那一筆自己正常收尾了（Stop hook）就作廢。
//!
//! 欠著／待證的帳由 [`settle_locked`] 結清：只認記下的那一筆（CAS 在 `id` 與 `status='in_flight'` 上），
//! **從不再按鍵**。會去結清的路：這顆 bot 的下一則回合 hook（Esc 的回聲就是其中一則）、下一則 prompt、同一次中斷的
//! 重試、強制中止、定時重試（[`schedule_retry`]）。那一筆已經被別的路收掉了（hook、run 結束、watchdog）就作廢。
//!
//! 帳只在記憶體：daemon 重啟就沒了。重啟後還 in_flight 的那一筆照舊由 hook 或閒置 watchdog（`stuck_turns`）收，
//! 跟記帳之前一樣——不會永遠卡住，只是慢。

use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 網頁 Esc 收掉回合時寫進對話的說明。
pub(crate) const INTERRUPT_NOTE: &str = "interrupted by user";

/// 定時重試的間隔（秒）：DB 一時寫不進去（鎖住、I/O 錯誤）多半幾秒內就好；最後幾次拉長，總共約四分鐘，
/// 之後交給 hook 與 `stuck_turns`。
const RETRY_DELAYS_SECS: [u64; 8] = [1, 2, 4, 8, 15, 30, 60, 120];

/// 一顆鍵送出去之後的結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyFate {
    /// herdr 回 ok：鍵進了 pane。
    Applied,
    /// herdr 回錯誤或連不上：鍵沒有進 pane。
    NotApplied,
    /// 送出去了，但不知道 herdr 有沒有做（逾時、連線斷了沒回、回應讀不懂）。
    Unknown,
}

pub(crate) fn key_fate(res: &anyhow::Result<()>) -> KeyFate {
    match res {
        Ok(()) => KeyFate::Applied,
        Err(e) if crate::herdr::never_applied(e) => KeyFate::NotApplied,
        Err(_) => KeyFate::Unknown,
    }
}

/// 結清的時候手上多了什麼證據。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// 沒有新的證據：只補欠著的；待證的照舊等。
    Nothing,
    /// 那次打斷的 `StopFailure` 回聲到了（payload 說是使用者中斷，或 #117 認得出是那一回合的回聲）：鍵確實生效了。
    Echo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// 鍵做了，DB 那一半還沒寫成。
    Owed,
    /// 不知道鍵做了沒有，等證據。
    Unconfirmed,
}

#[derive(Debug, Clone)]
struct Pending {
    run_id: String,
    /// 被打斷的那一筆。
    turn_id: String,
    /// 收掉時寫進對話的說明。
    note: String,
    stage: Stage,
}

/// bot → 這顆 bot 欠著／待證的那一次打斷。同一顆 bot 同時最多一筆在飛，所以一次也最多一筆。
fn ledger() -> &'static Mutex<HashMap<String, Pending>> {
    static M: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn pending(bot_id: &str) -> Option<Pending> {
    ledger().lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).cloned()
}

fn record(bot_id: &str, p: Pending) {
    ledger().lock().unwrap_or_else(|e| e.into_inner()).insert(bot_id.to_string(), p);
}

/// 只結清同一筆：等的時候有人記了新的一筆（下一次打斷），留給它。
fn forget(bot_id: &str, turn_id: &str) {
    let mut m = ledger().lock().unwrap_or_else(|e| e.into_inner());
    if m.get(bot_id).is_some_and(|p| p.turn_id == turn_id) {
        m.remove(bot_id);
    }
}

/// 這顆 bot 對 `turn_id` 還欠著（鍵已經生效、DB 還沒寫成）嗎？欠著的時候不能再按一次鍵。
pub(crate) fn owes(bot_id: &str, turn_id: &str) -> bool {
    owed_turn(bot_id).as_deref() == Some(turn_id)
}

/// 這顆 bot 欠著收尾的是哪一筆（鍵已經生效的那種；待證的不算）。
pub(crate) fn owed_turn(bot_id: &str) -> Option<String> {
    pending(bot_id).filter(|p| p.stage == Stage::Owed).map(|p| p.turn_id)
}

/// 鍵做了：把 `turn_id` 收成被打斷的 failed（跟說明同一個交易）。寫不進去就記成欠著、排定時重試，回 `Err`——
/// 呼叫端**不能**回普通的成功。
pub(crate) async fn interrupted(app: &Arc<App>, bot_id: &str, run_id: &str, turn_id: &str, note: &str) -> anyhow::Result<()> {
    let p = Pending { run_id: run_id.to_string(), turn_id: turn_id.to_string(), note: note.to_string(), stage: Stage::Owed };
    match close(app, bot_id, &p).await {
        Ok(()) => {
            forget(bot_id, turn_id);
            Ok(())
        }
        Err(e) => {
            tracing::warn!(bot = bot_id, turn = turn_id, error = %e, "鍵已經生效，回合卻沒收成：記成欠著，之後補");
            record(bot_id, p);
            schedule_retry(app, bot_id);
            Err(e)
        }
    }
}

/// 鍵不知道做了沒有：那一筆留在 in_flight（不假定打斷），記成待證。
pub(crate) fn unconfirmed(bot_id: &str, run_id: &str, turn_id: &str, note: &str) {
    tracing::warn!(bot = bot_id, turn = turn_id, "不知道打斷的鍵有沒有進 pane：回合留在 in_flight，等證據");
    record(
        bot_id,
        Pending { run_id: run_id.to_string(), turn_id: turn_id.to_string(), note: note.to_string(), stage: Stage::Unconfirmed },
    );
}

/// 結清這顆 bot 欠著／待證的那一筆。呼叫端握著 bot 鎖。`Err`：DB 還是寫不進去，帳留著。
pub(crate) async fn settle_locked(app: &Arc<App>, bot_id: &str, evidence: Evidence) -> anyhow::Result<()> {
    let Some(p) = pending(bot_id) else { return Ok(()) };
    let now: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT status, run_id FROM turns WHERE id=?").bind(&p.turn_id).fetch_optional(&app.db).await?;
    // 那一筆已經被別的路收掉（或不是這一代的了）：這筆帳作廢。
    if !matches!(&now, Some((status, run)) if status == "in_flight" && run.as_deref() == Some(p.run_id.as_str())) {
        tracing::info!(bot = bot_id, turn = %p.turn_id, ?now, "打斷的帳作廢：那一筆已經不在飛了");
        forget(bot_id, &p.turn_id);
        return Ok(());
    }
    let proven = match p.stage {
        Stage::Owed => true,
        Stage::Unconfirmed => evidence == Evidence::Echo,
    };
    if !proven {
        return Ok(());
    }
    close(app, bot_id, &p).await?;
    tracing::info!(bot = bot_id, turn = %p.turn_id, stage = ?p.stage, "補上了打斷的收尾");
    forget(bot_id, &p.turn_id);
    Ok(())
}

/// [`settle_locked`]，自己拿 bot 鎖（定時重試用）。
async fn settle(app: &Arc<App>, bot_id: &str) -> anyhow::Result<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    settle_locked(app, bot_id, Evidence::Nothing).await
}

/// 收掉那一筆：CAS 在 `status='in_flight'`；贏了才寫說明，兩句同一個交易（說明寫不進去就整筆回滾，下次再試）。
async fn close(app: &Arc<App>, bot_id: &str, p: &Pending) -> anyhow::Result<()> {
    let mut tx = app.db.begin().await?;
    let outcome = super::turn_controller::fail_on(&mut tx, &p.turn_id, super::turn_controller::DeliveryOnFail::Keep, &p.note).await?;
    let note = if outcome == super::turn_controller::Outcome::Applied {
        let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?").bind(&p.turn_id).fetch_one(&mut *tx).await?;
        Some(insert_message_tx(&mut tx, &conv, Some(&p.turn_id), "system", &p.note, "system", false, None).await?)
    } else {
        None
    };
    tx.commit().await?;
    if let Some(m) = note {
        emit_message_added(app, bot_id, m).await;
    }
    // 推回合結束：排著的派工由它叫醒（`fail_in_flight` 一樣靠這個）。
    emit_turn(app, &p.turn_id).await;
    Ok(())
}

/// 欠著的帳沒有 hook 也要補上：DB 一時寫不進去多半很快就好。只補欠著的（待證的要證據，不是時間）。
pub(crate) fn schedule_retry(app: &Arc<App>, bot_id: &str) {
    if cfg!(test) {
        return;
    }
    let app = app.clone();
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        for secs in RETRY_DELAYS_SECS {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            if !pending(&bot_id).is_some_and(|p| p.stage == Stage::Owed) {
                return;
            }
            if let Err(e) = settle(&app, &bot_id).await {
                tracing::warn!(bot = %bot_id, error = %e, "打斷的收尾還是寫不進去；稍後再試");
            }
        }
    });
}

/// 鍵生效了、回合狀態沒寫成（#147）：503，跟 start／stop 的 `*_state_uncommitted` 同一種——外面的副作用已經做了、
/// DB 那一半還欠著（[`LcError::Uncommitted`]）。帶著那一筆的身分：重試時帶 `turn_id`，那一筆已經不在飛就不會誤按到下一回合。
pub(crate) fn uncommitted(run_id: &str, turn_id: &str, cause: Option<&anyhow::Error>) -> LcError {
    LcError::Uncommitted(json!({
        "error": "interrupt_state_uncommitted",
        "run_id": run_id,
        "turn_id": turn_id,
        "esc_sent": true,
        "retryable": true,
        "message": "Esc 已經送進去了，回合的狀態還沒寫成；daemon 會自己補上，也可以帶同一個 turn_id 重試（不會再按一次 Esc）。",
        "detail": cause.map(|e| format!("{e:#}")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    struct Busy {
        env: tt::Env,
        bot: db::Bot,
        run: String,
        turn: String,
    }

    /// 一顆正在跑回合的 claude：herdr 有這個 agent（Esc 送得進去），run 上有一筆 in-flight。
    async fn busy(name: &str) -> Busy {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, name).await;
        let run = tt::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, prompt_text)
             VALUES (?,?,?,'web','in_flight','ok',?,'跑一下測試')",
        )
        .bind(&turn)
        .bind(&conv)
        .bind(&run)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        Busy { env, bot, run, turn }
    }

    /// SQLite 這一刻寫不進 `turns.status`（磁碟、鎖、I/O 錯誤）。
    async fn lose_turn_writes(app: &Arc<App>) {
        sqlx::query("CREATE TRIGGER lost_turn_write BEFORE UPDATE OF status ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();
    }

    async fn heal_turn_writes(app: &Arc<App>) {
        sqlx::query("DROP TRIGGER lost_turn_write").execute(&app.db).await.unwrap();
    }

    async fn status_of(app: &Arc<App>, turn: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap()
    }

    async fn system_notes(app: &Arc<App>, turn: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system' ORDER BY created_at")
            .bind(turn)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    fn escs(env: &tt::Env) -> usize {
        env.herdr
            .calls_to("agent.send_keys")
            .iter()
            .filter(|p| p.get("keys").and_then(|k| k.as_array()).is_some_and(|k| k.iter().any(|k| k == "esc")))
            .count()
    }

    fn echo(bot_id: &str, detail: Option<&str>) -> crate::hookrecv::HookBody {
        let mut payload = json!({"hook_event_name": "StopFailure", "session_id": "s-esc", "prompt_id": "p-esc"});
        if let Some(d) = detail {
            payload["error"] = json!(d);
        }
        crate::hookrecv::HookBody {
            bot_id: bot_id.to_string(),
            provider: "claude".into(),
            payload,
            received_at: Some(db::now()),
            truncated: false,
            run_id: None,
        }
    }

    /// #147 驗收一：Esc 真的送進去了、回合卻沒收掉（`set_status` 失敗）——API 不能回普通的成功。
    /// 以前 `fail_in_flight` 把錯吞掉只記 warning，interrupt 照樣 200，回合永遠 in_flight。
    #[tokio::test]
    async fn an_esc_that_landed_while_the_turn_could_not_be_closed_is_not_a_plain_success() {
        let b = busy("esc-lost-write").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("Esc 送進去、回合沒收掉：不能回普通的成功");
        let LcError::Uncommitted(body) = err else { panic!("要是可重試的結構化錯誤（503）：{err:?}") };
        assert_eq!(body["error"], "interrupt_state_uncommitted", "{body}");
        assert_eq!(body["turn_id"], b.turn.as_str(), "{body}");
        assert_eq!(body["run_id"], b.run.as_str(), "{body}");
        assert_eq!(body["esc_sent"], true, "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(escs(&b.env), 1);
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "寫不進去就是還沒收：不假裝收掉了");
    }

    /// #147 驗收二：寫失敗之後，那次 Esc 的 `StopFailure` 回聲到了——回合要在這裡收掉（照 Esc 的說明），
    /// 不能因為「回聲一律什麼都不動」而永遠卡在 in_flight；也不能被算成 provider／額度失敗。
    #[tokio::test]
    async fn the_esc_echo_finishes_the_turn_that_the_lost_write_left_in_flight() {
        let b = busy("esc-echo-recovers").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        heal_turn_writes(&app).await;

        // 回聲的字面看起來像撞額度也一樣：這一回合是被 Esc 停掉的，不是撞額度失敗收尾。（帳號撞限記不記由 #150
        // 決定——那是帳號的事實，跟回合怎麼收無關，這裡不管。）
        let quota = "You've hit your session limit · resets 3pm (Asia/Taipei)";
        crate::hookrecv::process(&app, &echo(&b.bot.id, Some(quota))).await.unwrap();

        assert_eq!(status_of(&app, &b.turn).await, "failed", "回聲是 Esc 已經生效的證據：回合收掉");
        assert_eq!(system_notes(&app, &b.turn).await, vec!["interrupted by user".to_string()], "說明是使用者中斷，不是失敗收尾");
        assert_eq!(escs(&b.env), 1, "補收尾不再按鍵");
    }

    /// #147 驗收三：同一次中斷重試（使用者再按一次、呼叫端照 409 重送）——Esc 已經生效，不能再按一次
    /// （claude 閒著時連按兩次 Esc 會跳 rewind 選單）。只把欠著的那一半補上。
    #[tokio::test]
    async fn retrying_the_same_interrupt_never_presses_esc_twice() {
        let b = busy("esc-retry").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");

        let again = interrupt_bot(&app, &b.bot.id).await.expect_err("DB 還是寫不進去：照樣不是成功");
        let LcError::Uncommitted(body) = again else { panic!("{again:?}") };
        assert_eq!(body["error"], "interrupt_state_uncommitted", "{body}");
        assert_eq!(escs(&b.env), 1, "Esc 已經生效：重試不再按");

        heal_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect("DB 好了：重試把欠著的收尾補上就成功");
        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert_eq!(system_notes(&app, &b.turn).await, vec!["interrupted by user".to_string()]);
        assert_eq!(escs(&b.env), 1, "從頭到尾只按了一次");
    }

    /// #147 驗收三之二：重試綁著那一筆的身分。它已經收掉、下一回合已經開始時，帶著它的 `turn_id` 重試
    /// **不按 Esc**，回 409——不會誤傷下一回合。定時重試同樣只認記下的那一筆、從不按鍵。
    #[tokio::test]
    async fn a_retry_bound_to_the_old_turn_never_escapes_the_next_one() {
        let b = busy("esc-retry-bound").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        heal_turn_writes(&app).await;
        settle(&app, &b.bot.id).await.expect("定時重試補上了");
        assert_eq!(status_of(&app, &b.turn).await, "failed");

        // 下一回合開始了。
        let conv = db::conversation_id(&app.db, &b.bot.id).await.unwrap();
        let next = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&next)
            .bind(&conv)
            .bind(&b.run)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        settle(&app, &b.bot.id).await.unwrap();

        let err = interrupt_turn(&app, &b.bot.id, Some(&b.turn)).await.expect_err("那一筆已經不在飛");
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "turn_not_in_flight", "{body}");
        assert_eq!(body["in_flight_turn_id"], next.as_str(), "{body}");
        assert_eq!(escs(&b.env), 1, "沒有對下一回合按 Esc");
        assert_eq!(status_of(&app, &next).await, "in_flight");
    }

    /// herdr 明確拒收 Esc：什麼都沒發生——502、回合照舊在飛、不記任何帳，之後的回聲式 hook 也不會把它收掉。
    #[tokio::test]
    async fn an_esc_herdr_refused_changes_nothing() {
        let b = busy("esc-refused").await;
        let app = b.env.app.clone();
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::Refuse);

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("Esc 沒進去");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        assert_eq!(status_of(&app, &b.turn).await, "in_flight");
        assert!(pending(&b.bot.id).is_none(), "沒有欠著、也沒有待證");
        assert!(crate::lifecycle::interrupt_grace::pending_echo(&b.bot.id).is_none(), "也不等回聲");
    }

    /// Esc 送出去了但 herdr 沒回（不知道進了沒有）：不假定打斷——回合留在 in_flight、回 409 說清楚。
    /// 之後那次 Esc 的回聲到了才收（照 Esc 的說明）；回合自己正常答完的話就照答完收，帳作廢。
    #[tokio::test]
    async fn an_unanswered_esc_waits_for_its_echo_before_closing_anything() {
        let b = busy("esc-unknown").await;
        let app = b.env.app.clone();
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::DropAfter);

        let err = interrupt_bot(&app, &b.bot.id).await.expect_err("不知道 Esc 進了沒有");
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "interrupt_unconfirmed", "{body}");
        assert_eq!(body["turn_id"], b.turn.as_str(), "{body}");
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "不假定打斷");
        settle(&app, &b.bot.id).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "時間過去不是證據");

        crate::hookrecv::process(&app, &echo(&b.bot.id, None)).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "failed", "回聲證明 Esc 進去了");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
        assert!(pending(&b.bot.id).is_none());

        // 另一種結局：Esc 其實沒進去，回合自己答完了。
        let c = busy("esc-unknown-finished").await;
        let app = c.env.app.clone();
        c.env.herdr.fail_next("agent.send_keys", tt::Fault::DropBefore);
        interrupt_bot(&app, &c.bot.id).await.expect_err("不知道");
        let stop = crate::hookrecv::HookBody {
            bot_id: c.bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-done", "prompt_id": "p-done", "last_assistant_message": "測完了"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();
        assert_eq!(status_of(&app, &c.turn).await, "completed", "Esc 沒進去：照答完收");
        assert!(system_notes(&app, &c.turn).await.is_empty(), "不補「被中斷」");
        settle(&app, &c.bot.id).await.unwrap();
        assert!(pending(&c.bot.id).is_none(), "那一筆不在飛了：帳作廢");
    }

    /// 寫失敗之後，使用者直接在 pane 裡打了下一句、它的 Stop 先到：被 Esc 停掉的那一筆要先收掉（hook 一進來先補帳），
    /// 下一句的回覆才不會掛到它身上、把它記成「答完了」。
    #[tokio::test]
    async fn the_next_answer_after_a_lost_interrupt_write_is_not_pinned_on_the_interrupted_turn() {
        let b = busy("esc-then-typed").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        heal_turn_writes(&app).await;

        let stop = crate::hookrecv::HookBody {
            bot_id: b.bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-next", "prompt_id": "p-next", "last_assistant_message": "下一句的回覆"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();

        assert_eq!(status_of(&app, &b.turn).await, "failed", "被 Esc 停掉的就是被停掉的");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
        let pinned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&b.turn)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(pinned, 0, "下一句的回覆沒有掛到被中斷的回合上");
    }

    /// 不知道 Esc 進了沒有，然後 claude 自己說「使用者中斷」（payload 就講得出來的那種回聲）：一樣是證據。
    #[tokio::test]
    async fn an_unanswered_esc_is_also_proven_by_an_echo_that_says_so_itself() {
        let b = busy("esc-unknown-says-so").await;
        let app = b.env.app.clone();
        b.env.herdr.fail_next("agent.send_keys", tt::Fault::DropBefore);
        interrupt_bot(&app, &b.bot.id).await.expect_err("不知道");

        crate::hookrecv::process(&app, &echo(&b.bot.id, Some("[Request interrupted by user]"))).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
    }

    /// #147 驗收五（沒有 hook、也沒有下一則 prompt）：定時重試自己把欠著的補上——排隊 flush 看的是「這個 run
    /// 有沒有 in-flight」，那筆幽靈不在了，排著的派工只剩 Esc 本來就有的寬限（§4.4a），不會永久卡住。
    #[tokio::test]
    async fn the_retry_alone_unblocks_the_queue_after_a_lost_interrupt_write() {
        let b = busy("esc-queue").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        assert!(db::in_flight_turn(&app.db, &b.run).await.unwrap().is_some(), "還擋著");

        settle(&app, &b.bot.id).await.expect_err("DB 還沒好：這一輪補不上，帳留著");
        assert!(owes(&b.bot.id, &b.turn));
        heal_turn_writes(&app).await;
        settle(&app, &b.bot.id).await.expect("定時重試的下一輪");
        assert!(db::in_flight_turn(&app.db, &b.run).await.unwrap().is_none(), "不再擋著排隊的那一則");
        assert!(!owes(&b.bot.id, &b.turn), "帳結清了");
        assert_eq!(escs(&b.env), 1, "重試從不按鍵");
    }

    /// 強制中止同一個洞：以前 `fail_in_flight` 吞錯，迴圈又因為它在 `aborted` 裡跳過它——回 200、它卻還在飛。
    #[tokio::test]
    async fn an_abort_whose_write_was_lost_is_not_reported_as_done() {
        let b = busy("abort-lost-write").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;

        let err = abort_turns(&app, &b.bot.id).await.expect_err("回合沒收掉：不能回 aborted");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        assert_eq!(status_of(&app, &b.turn).await, "in_flight");

        heal_turn_writes(&app).await;
        settle(&app, &b.bot.id).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert_eq!(system_notes(&app, &b.turn).await, vec!["回合已由使用者強制中止".to_string()]);
    }

    /// #147 驗收四：DB 正常時跟以前一模一樣——一次 Esc、回合收成 failed、一則說明、回 Ok、不留帳。
    #[tokio::test]
    async fn with_a_healthy_db_an_interrupt_is_exactly_what_it_was() {
        let b = busy("esc-healthy").await;
        let app = b.env.app.clone();
        interrupt_bot(&app, &b.bot.id).await.expect("照舊成功");
        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert_eq!(system_notes(&app, &b.turn).await, vec![INTERRUPT_NOTE.to_string()]);
        assert_eq!(escs(&b.env), 1);
        assert!(pending(&b.bot.id).is_none());
        assert!(crate::lifecycle::interrupt_grace::pending_echo(&b.bot.id).is_some(), "照舊等回聲（#117）");
    }

    /// #147 驗收五：寫失敗之後 hook 一直沒來（claude 對 Esc 不一定送 `StopFailure`）——下一則 prompt 不能
    /// 被那筆幽靈 in-flight 擋成「a turn is already in flight」直到五分鐘後的 watchdog。
    #[tokio::test]
    async fn a_lost_interrupt_write_does_not_wedge_the_next_prompt() {
        let b = busy("esc-no-wedge").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        interrupt_bot(&app, &b.bot.id).await.expect_err("寫不進去");
        heal_turn_writes(&app).await;

        match prompt(&app, &b.bot.id, "下一句", "after-esc").await {
            Ok(_) => {}
            Err(LcError::Conflict(body)) => {
                assert_ne!(body["reason"], "a turn is already in flight", "被已經中斷的回合擋住了：{body}");
            }
            Err(e) => panic!("{e:?}"),
        }
        assert_eq!(status_of(&app, &b.turn).await, "failed", "欠著的收尾在下一則 prompt 之前補上");
    }
}
