//! 「打斷一個回合」是一個分散式操作（#147、#120）：外面的一顆鍵（網頁 Esc、強制中止的 `esc`、插隊送出的
//! `ctrl+x ctrl+s`），加上 DB 裡把被打斷的回合收成 `failed`。兩半不在同一個交易裡，所以：
//!
//! - **鍵的結果分三種**（[`KeyFate`]）：herdr 回錯誤或根本連不上＝**沒做**；回 ok＝**做了**；送出去之後逾時、
//!   連線斷了沒回＝**不知道**（[`crate::herdr::never_applied`]）。
//! - **沒做**：什麼都不動，那一筆照舊 in_flight。
//! - **做了**：DB 那一半跟著寫（收成 failed 與說明同一個交易）。寫不進去就**記成欠著**，呼叫端不回普通的成功。
//! - **不知道**：不假定打斷——那一筆留在 in_flight，**記成待證**；之後的證據（那次 Esc 的 `StopFailure` 回聲；
//!   插隊那一句出現在 transcript 裡）說鍵生效了才收。那一筆自己正常收尾了（Stop hook）就作廢。
//!
//! 插隊送出多一件事：新的那一則在送出鍵生效**之前**已經寫進 DB（維護窗口的閘門與冪等靠它），但還不能佔 run 的
//! in-flight 名額（`turns_one_in_flight`，舊的那一筆還在跑）——所以它先以 `run_id = NULL` 的 in_flight 存在，
//! 送出鍵生效時跟收掉舊的那一筆**同一個交易**掛上 run。鍵沒生效或不知道時，新的那一則收成 failed，不佔名額。
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
    /// 插隊送出（#120）：跟收掉舊的那一筆同一個交易掛上 run 的那一則，與它的送達結果（已經知道的話）。
    new_turn: Option<NewTurn>,
    /// 待證的插隊送出：之後看得到那一則進了 transcript，就是送出鍵生效了。
    proof: Option<SentProof>,
}

#[derive(Debug, Clone)]
struct NewTurn {
    id: String,
    delivery: Option<DeliveryRecord>,
}

/// bot → 這顆 bot 欠著／待證的收尾，一筆回合一條。同一個 run 同時最多一筆在飛，但 DB 一直寫不進去時，上一個 run
/// 欠著的那一筆還沒補上、新的 run 又欠一筆是可能的（#156）——後來的不能蓋掉先前的。
fn ledger() -> &'static Mutex<HashMap<String, Vec<Pending>>> {
    static M: OnceLock<Mutex<HashMap<String, Vec<Pending>>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn pending(bot_id: &str) -> Vec<Pending> {
    ledger().lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).cloned().unwrap_or_default()
}

/// 同一筆回合只留最新的一條。
fn record(bot_id: &str, p: Pending) {
    let mut m = ledger().lock().unwrap_or_else(|e| e.into_inner());
    let list = m.entry(bot_id.to_string()).or_default();
    list.retain(|x| x.turn_id != p.turn_id);
    list.push(p);
}

/// 只結清那一筆。
fn forget(bot_id: &str, turn_id: &str) {
    let mut m = ledger().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(list) = m.get_mut(bot_id) {
        list.retain(|x| x.turn_id != turn_id);
        if list.is_empty() {
            m.remove(bot_id);
        }
    }
}

/// 這顆 bot 對 `turn_id` 還欠著（鍵已經生效、DB 還沒寫成）嗎？欠著的時候不能再按一次鍵。
pub(crate) fn owes(bot_id: &str, turn_id: &str) -> bool {
    owed_turn(bot_id).as_deref() == Some(turn_id)
}

/// 這顆 bot 最近一筆欠著收尾的回合（鍵已經生效的那種；待證的不算）。
pub(crate) fn owed_turn(bot_id: &str) -> Option<String> {
    pending(bot_id).into_iter().rev().find(|p| p.stage == Stage::Owed).map(|p| p.turn_id)
}

/// 鍵做了：把 `turn_id` 收成被打斷的 failed（跟說明同一個交易）。寫不進去就記成欠著、排定時重試，回 `Err`——
/// 呼叫端**不能**回普通的成功。
pub(crate) async fn interrupted(app: &Arc<App>, bot_id: &str, run_id: &str, turn_id: &str, note: &str) -> anyhow::Result<()> {
    let p = Pending {
        run_id: run_id.to_string(),
        turn_id: turn_id.to_string(),
        note: note.to_string(),
        stage: Stage::Owed,
        new_turn: None,
        proof: None,
    };
    owe(app, bot_id, p).await
}

/// 插隊送出的送出鍵生效了：收掉被插隊的那一筆，同一個交易把新的那一則掛上 run（#120）。其餘同 [`interrupted`]。
pub(crate) async fn send_now_interrupted(app: &Arc<App>, bot_id: &str, run_id: &str, turn_id: &str, new_turn: &str) -> anyhow::Result<()> {
    let p = Pending {
        run_id: run_id.to_string(),
        turn_id: turn_id.to_string(),
        note: SEND_NOW_NOTE.to_string(),
        stage: Stage::Owed,
        new_turn: Some(NewTurn { id: new_turn.to_string(), delivery: None }),
        proof: None,
    };
    owe(app, bot_id, p).await
}

/// 欠著的插隊送出，新那一則的送達結果後來才知道：記在帳上，補的時候一起寫。
pub(crate) fn owe_delivery(bot_id: &str, new_turn: &str, delivery: Option<DeliveryRecord>) {
    let mut m = ledger().lock().unwrap_or_else(|e| e.into_inner());
    let list = m.get_mut(bot_id).into_iter().flatten();
    if let Some(n) = list.filter_map(|p| p.new_turn.as_mut()).find(|n| n.id == new_turn) {
        n.delivery = delivery;
    }
}

async fn owe(app: &Arc<App>, bot_id: &str, p: Pending) -> anyhow::Result<()> {
    let turn_id = p.turn_id.clone();
    match close(app, bot_id, &p).await {
        Ok(()) => {
            forget(bot_id, &turn_id);
            Ok(())
        }
        Err(e) => {
            tracing::warn!(bot = bot_id, turn = %turn_id, error = %e, "鍵已經生效，回合卻沒收成：記成欠著，之後補");
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
        Pending {
            run_id: run_id.to_string(),
            turn_id: turn_id.to_string(),
            note: note.to_string(),
            stage: Stage::Unconfirmed,
            new_turn: None,
            proof: None,
        },
    );
}

/// 插隊送出的送出鍵不知道生效了沒有（#120）：被插隊的那一筆留在 in_flight，記成待證。`proof` 在的話，之後
/// transcript 裡出現那一則就是證據；Esc 式的回聲（claude 說那一回合被使用者中斷）也算。
pub(crate) fn unconfirmed_send_now(bot_id: &str, run_id: &str, turn_id: &str, proof: Option<SentProof>) {
    tracing::warn!(bot = bot_id, turn = turn_id, "不知道插隊送出的鍵有沒有生效：被插隊的那一筆留在 in_flight，等證據");
    record(
        bot_id,
        Pending {
            run_id: run_id.to_string(),
            turn_id: turn_id.to_string(),
            note: SEND_NOW_NOTE.to_string(),
            stage: Stage::Unconfirmed,
            new_turn: None,
            proof,
        },
    );
}

/// 結清這顆 bot 欠著／待證的收尾。呼叫端握著 bot 鎖。`Err`：有一筆 DB 還是寫不進去，那一筆的帳留著（其他的照樣結清）。
pub(crate) async fn settle_locked(app: &Arc<App>, bot_id: &str, evidence: Evidence) -> anyhow::Result<()> {
    let mut failed = None;
    for p in pending(bot_id) {
        if let Err(e) = settle_one(app, bot_id, &p, evidence).await {
            tracing::warn!(bot = bot_id, turn = %p.turn_id, error = %e, "欠著的收尾還是寫不進去");
            failed.get_or_insert(e);
        }
    }
    failed.map_or(Ok(()), Err)
}

async fn settle_one(app: &Arc<App>, bot_id: &str, p: &Pending, evidence: Evidence) -> anyhow::Result<()> {
    let now: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT status, run_id FROM turns WHERE id=?").bind(&p.turn_id).fetch_optional(&app.db).await?;
    let old_in_flight = matches!(&now, Some((status, run)) if status == "in_flight" && run.as_deref() == Some(p.run_id.as_str()));
    // 插隊送出那一則還沒掛上 run：舊的那一筆就算已經被別的路收掉，它也還要有個結果。
    let new_unbound = match &p.new_turn {
        Some(n) => sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns WHERE id=? AND status='in_flight' AND run_id IS NULL")
            .bind(&n.id)
            .fetch_one(&app.db)
            .await?
            > 0,
        None => false,
    };
    // 那一筆已經被別的路收掉（或不是這一代的了）：這筆帳作廢。
    if !old_in_flight && !new_unbound {
        tracing::info!(bot = bot_id, turn = %p.turn_id, ?now, "打斷的帳作廢：那一筆已經不在飛了");
        forget(bot_id, &p.turn_id);
        return Ok(());
    }
    let proven = match p.stage {
        Stage::Owed => true,
        Stage::Unconfirmed => evidence == Evidence::Echo || shows(p.proof.clone()).await,
    };
    if !proven {
        return Ok(());
    }
    close(app, bot_id, p).await?;
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

/// 收掉一筆回合（failed＋說明同一個交易，CAS 在 in_flight），**不記帳**：寫不進去就回 `Err`，由呼叫端決定——
/// 還沒動外面的（stop、重啟）就不動，已經發生的（run 結束）改用 [`interrupted`] 記成欠著（#156）。
pub(crate) async fn close_turn(app: &Arc<App>, bot_id: &str, run_id: &str, turn_id: &str, note: &str) -> anyhow::Result<()> {
    let p = Pending {
        run_id: run_id.to_string(),
        turn_id: turn_id.to_string(),
        note: note.to_string(),
        stage: Stage::Owed,
        new_turn: None,
        proof: None,
    };
    close(app, bot_id, &p).await
}

/// 待證的插隊送出：那一則現在出現在 transcript 裡了嗎？（讀檔，放到 blocking 執行緒）
async fn shows(proof: Option<SentProof>) -> bool {
    let Some(proof) = proof else { return false };
    tokio::task::spawn_blocking(move || proof.shows()).await.unwrap_or(false)
}

/// 收掉那一筆：CAS 在 `status='in_flight'`；贏了才寫說明，兩句同一個交易（說明寫不進去就整筆回滾，下次再試）。
/// 插隊送出的話，同一個交易把新的那一則掛上 run；掛不上（run 已經不在、另有回合在飛）就把它收成 failed。
async fn close(app: &Arc<App>, bot_id: &str, p: &Pending) -> anyhow::Result<()> {
    let mut tx = app.db.begin().await?;
    let outcome = super::turn_controller::fail_on(&mut tx, &p.turn_id, super::turn_controller::DeliveryOnFail::Keep, &p.note).await?;
    let note = if outcome == super::turn_controller::Outcome::Applied {
        let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?").bind(&p.turn_id).fetch_one(&mut *tx).await?;
        Some(insert_message_tx(&mut tx, &conv, Some(&p.turn_id), "system", &p.note, "system", false, None).await?)
    } else {
        None
    };
    let mut notes = Vec::from_iter(note);
    let mut bound = false;
    if let Some(n) = &p.new_turn {
        bound = sqlx::query(
            "UPDATE turns SET run_id=? WHERE id=? AND run_id IS NULL AND status='in_flight'
                AND NOT EXISTS (SELECT 1 FROM turns WHERE run_id=? AND status='in_flight')
                AND EXISTS (SELECT 1 FROM runs WHERE id=? AND state='running')",
        )
        .bind(&p.run_id)
        .bind(&n.id)
        .bind(&p.run_id)
        .bind(&p.run_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !bound {
            let why = "插隊送出的鍵生效了，但這個 run 已經不在（或另有回合在飛），這一則接不上去。";
            if super::turn_controller::fail_on(&mut tx, &n.id, super::turn_controller::DeliveryOnFail::Keep, why).await?
                == super::turn_controller::Outcome::Applied
            {
                let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?").bind(&n.id).fetch_one(&mut *tx).await?;
                notes.push(insert_message_tx(&mut tx, &conv, Some(&n.id), "system", why, "system", false, None).await?);
            }
        }
    }
    tx.commit().await?;
    for m in notes {
        emit_message_added(app, bot_id, m).await;
    }
    if let Some(n) = &p.new_turn {
        if let (true, Some(rec)) = (bound, n.delivery) {
            mark_delivery(app, &n.id, rec).await;
        }
        emit_turn(app, &n.id).await;
    }
    // 推回合結束：排著的派工由它叫醒（`fail_in_flight` 一樣靠這個）。
    emit_turn(app, &p.turn_id).await;
    Ok(())
}

/// run 已經結束（不是 starting／running／stopping）、回合卻還 in_flight（#156）：欠著的收尾只記在記憶體，daemon 在補上
/// 之前重啟就沒人會收——那個 run 不會再有 hook，閒置 watchdog 也只看活著的 run。重啟時補收成 failed，寫明原因。
pub(crate) async fn adopt_turns_of_ended_runs(app: &Arc<App>) {
    let rows: Vec<(String, String, String, String)> = match sqlx::query_as(
        "SELECT t.id, t.run_id, c.bot_id, r.state FROM turns t
           JOIN runs r ON r.id = t.run_id
           JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'in_flight' AND r.state NOT IN ('starting','running','stopping')",
    )
    .fetch_all(&app.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not look for in-flight turns left on ended runs");
            return;
        }
    };
    for (turn, run, bot, state) in rows {
        let lock = app.bot_lock(&bot).await;
        let _g = lock.lock().await;
        let note = format!("這一回合的 run 已經結束（{state}），當時沒能把回合收掉；daemon 重啟時補收。");
        match close_turn(app, &bot, &run, &turn, &note).await {
            Ok(()) => tracing::warn!(turn = %turn, run = %run, %state, "closed a turn left in flight on an ended run"),
            Err(e) => tracing::warn!(turn = %turn, error = %e, "could not close a turn left in flight on an ended run"),
        }
    }
}

/// 重啟前正在插隊送出、還沒掛上 run 的那一則（`in_flight`、`run_id IS NULL`）：送出鍵有沒有生效沒有人知道，
/// 收它的那個 async 任務也跟著上一個行程走了。收成 failed、送達記成 unknown，不留一筆沒有 run 的 in_flight
/// （它會一直佔著維護窗口的閘門）。被插隊的那一筆照舊在它的 run 上，由 hook 或 watchdog 收。
pub(crate) async fn adopt_unbound_send_nows(app: &Arc<App>) {
    let rows: Vec<(String, String, String)> = match sqlx::query_as(
        "SELECT t.id, t.conversation_id, c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'in_flight' AND t.run_id IS NULL",
    )
    .fetch_all(&app.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not look for send-now turns left unbound by a restart");
            return;
        }
    };
    let why = "插隊送出途中 daemon 停了：送出鍵有沒有生效不知道。正在跑的那一回合沒有被收掉；這一句若真的送出去了，回覆會以外部回合出現。";
    for (turn, conv, bot) in rows {
        let lock = app.bot_lock(&bot).await;
        let _g = lock.lock().await;
        let res: anyhow::Result<Option<db::Message>> = async {
            let mut tx = app.db.begin().await?;
            if super::turn_controller::fail_on(&mut tx, &turn, super::turn_controller::DeliveryOnFail::Keep, why).await?
                != super::turn_controller::Outcome::Applied
            {
                return Ok(None);
            }
            sqlx::query("UPDATE turns SET delivery='unknown' WHERE id=?").bind(&turn).execute(&mut *tx).await?;
            let m = insert_message_tx(&mut tx, &conv, Some(&turn), "system", why, "system", false, None).await?;
            tx.commit().await?;
            Ok(Some(m))
        }
        .await;
        match res {
            Ok(Some(m)) => {
                tracing::warn!(turn = %turn, bot = %bot, "a send-now was mid-delivery when the daemon stopped; closed it as unknown");
                emit_message_added(app, &bot, m).await;
                emit_turn(app, &turn).await;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(turn = %turn, error = %e, "could not close a send-now left unbound by a restart"),
        }
    }
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
            if !pending(&bot_id).iter().any(|p| p.stage == Stage::Owed) {
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
        assert!(pending(&b.bot.id).is_empty(), "沒有欠著、也沒有待證");
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
        assert!(pending(&b.bot.id).is_empty());

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
        assert!(pending(&c.bot.id).is_empty(), "那一筆不在飛了：帳作廢");
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
        assert!(pending(&b.bot.id).is_empty());
        assert!(crate::lifecycle::interrupt_grace::pending_echo(&b.bot.id).is_some(), "照舊等回聲（#117）");
    }

    /// #120：插隊送出途中 daemon 停了——還沒掛上 run 的那一則（`run_id = NULL` 的 in_flight）沒有人會收，而且它會一直
    /// 擋住維護窗口的閘門。重啟時收成 failed、送達 unknown、寫明原因；被插隊的那一筆照舊在它的 run 上，由 hook／watchdog 收。
    #[tokio::test]
    async fn a_send_now_cut_off_by_a_restart_is_closed_as_unknown() {
        let b = busy("send-now-restart").await;
        let app = b.env.app.clone();
        let conv = db::conversation_id(&app.db, &b.bot.id).await.unwrap();
        let orphan = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, prompt_text)
             VALUES (?,?,NULL,'web','in_flight','pending',?,'先看這句')",
        )
        .bind(&orphan)
        .bind(&conv)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let fresh = tt::restart_app(&b.env).await;
        crate::reconcile::rearm_progress(&fresh).await;

        let (status, delivery): (String, String) =
            sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?").bind(&orphan).fetch_one(&app.db).await.unwrap();
        assert_eq!((status.as_str(), delivery.as_str()), ("failed", "unknown"));
        assert!(system_notes(&app, &orphan).await.iter().any(|n| n.contains("daemon 停了")));
        assert_eq!(status_of(&app, &b.turn).await, "in_flight", "被插隊的那一筆不動");
    }

    /// #156：欠著的帳只在記憶體。run 已經結束（`exited`／`stopped`）、那一筆卻還 in_flight，而 daemon 在補上之前重啟了——
    /// 沒有 hook 會來（run 不在了），閒置 watchdog 也只看活著的 run。重啟時補收成 failed，寫明原因。
    #[tokio::test]
    async fn a_turn_left_in_flight_on_an_ended_run_is_closed_after_a_restart() {
        let b = busy("ended-run-turn").await;
        let app = b.env.app.clone();
        sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE id=?").bind(db::now()).bind(&b.run).execute(&app.db).await.unwrap();

        let fresh = tt::restart_app(&b.env).await;
        crate::reconcile::rearm_progress(&fresh).await;

        assert_eq!(status_of(&app, &b.turn).await, "failed");
        assert!(system_notes(&app, &b.turn).await.iter().any(|n| n.contains("已經結束")), "{:?}", system_notes(&app, &b.turn).await);
    }

    /// #156：DB 一直寫不進去時，上一個 run 欠著的那一筆還沒補上、新的 run 又欠一筆——後來的不能蓋掉先前的，兩筆都要補上
    /// （舊 run 已經結束，那一筆不會再有 hook，也不歸閒置 watchdog 管，帳一丟就永遠在飛）。
    #[tokio::test]
    async fn two_owed_closes_for_one_bot_are_both_settled() {
        let b = busy("two-owed").await;
        let app = b.env.app.clone();
        lose_turn_writes(&app).await;
        assert_eq!(mark_run_exited(&app, &b.run, "pane exited").await, RunExit::TurnOwed);

        let second_run = tt::fake_run(&app, &b.bot.id).await;
        let conv = db::conversation_id(&app.db, &b.bot.id).await.unwrap();
        let second = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&second)
            .bind(&conv)
            .bind(&second_run)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        assert_eq!(mark_run_exited(&app, &second_run, "pane exited").await, RunExit::TurnOwed);

        heal_turn_writes(&app).await;
        settle(&app, &b.bot.id).await.unwrap();
        assert_eq!(status_of(&app, &b.turn).await, "failed", "先欠的那一筆沒被蓋掉");
        assert_eq!(status_of(&app, &second).await, "failed");
        assert!(pending(&b.bot.id).is_empty());
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
