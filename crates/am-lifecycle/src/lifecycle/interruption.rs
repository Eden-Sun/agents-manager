//! 「打斷一個回合」是一個分散式操作（#147、#120）：外面的一顆鍵（網頁 Esc、強制中止的 `esc`、插隊送出的
//! `ctrl+x ctrl+s`），加上 DB 裡把被打斷的回合收成 `failed`。兩半不在同一個交易裡，所以：
//!
//! - **鍵的結果分三種**（[`KeyFate`]）：herdr 回錯誤或根本連不上＝**沒做**；回 ok＝**做了**；送出去之後逾時、
//!   連線斷了沒回＝**不知道**（[`crate::herdr::never_applied`]）。
//! - **沒做**：什麼都不動，那一筆照舊 in_flight。
//! - **做了**：DB 那一半跟著寫（收成 failed 與說明同一個交易）。寫不進去就**記成欠著**，呼叫端不回普通的成功。
//! - **不知道**：不假定打斷——那一筆留在 in_flight，**記成待證**；之後的證據（那次 Esc 的 `StopFailure` 回聲；
//!   按鍵之後 log 裡出現的使用者中斷紀錄；插隊那一句出現在 transcript 裡）說鍵生效了才收。那一筆自己正常收尾了
//!   （Stop hook）就作廢。claude 2.1.276～2.1.278 按 Esc 不送 Stop／StopFailure（#223），Esc 實際上只能靠 log 證明。
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

#[cfg(feature = "test-hooks")]
use crate::capabilities::Db;
use super::s6_ports::InterruptionContext;
use super::*;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 網頁 Esc 收掉回合時寫進對話的說明。
pub const INTERRUPT_NOTE: &str = "interrupted by user";

/// 定時重試的間隔（秒）：DB 一時寫不進去（鎖住、I/O 錯誤）多半幾秒內就好；最後幾次拉長，總共約四分鐘，
/// 之後交給 hook 與 `stuck_turns`。
const RETRY_DELAYS_SECS: [u64; 8] = [1, 2, 4, 8, 15, 30, 60, 120];

/// 一顆鍵送出去之後的結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFate {
    /// herdr 回 ok：鍵進了 pane。
    Applied,
    /// herdr 回錯誤或連不上：鍵沒有進 pane。
    NotApplied,
    /// 送出去了，但不知道 herdr 有沒有做（逾時、連線斷了沒回、回應讀不懂）。
    Unknown,
}

pub fn key_fate(res: &anyhow::Result<()>) -> KeyFate {
    match res {
        Ok(()) => KeyFate::Applied,
        Err(e) if crate::herdr::never_applied(e) => KeyFate::NotApplied,
        Err(_) => KeyFate::Unknown,
    }
}

/// 結清的時候手上多了什麼證據。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
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
    /// 待證的 Esc：按鍵**之前**的時刻。log 裡在這之後出現使用者中斷的紀錄，就是 Esc 生效了（#223）。
    esc_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct NewTurn {
    id: String,
    /// 送達結果與送出的那一刻（已經知道的話）。
    delivery: Option<(DeliveryRecord, String)>,
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

/// 不在 `live` 裡的 bot 不留欠著／待證的收尾：待證的（鍵不知道進了沒有）要等證據才結清，bot 被刪掉就永遠等不到。
pub fn retain_bots(live: &[String]) {
    ledger().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| live.contains(id));
}

/// 這顆 bot 對 `turn_id` 還欠著（鍵已經生效、DB 還沒寫成）嗎？欠著的時候不能再按一次鍵。
pub fn owes(bot_id: &str, turn_id: &str) -> bool {
    owed_turn(bot_id).as_deref() == Some(turn_id)
}

/// 這顆 bot 最近一筆欠著收尾的回合（鍵已經生效的那種；待證的不算）。
pub fn owed_turn(bot_id: &str) -> Option<String> {
    pending(bot_id).into_iter().rev().find(|p| p.stage == Stage::Owed).map(|p| p.turn_id)
}

/// 鍵做了：把 `turn_id` 收成被打斷的 failed（跟說明同一個交易）。寫不進去就記成欠著、排定時重試，回 `Err`——
/// 呼叫端**不能**回普通的成功。
pub async fn interrupted<C: InterruptionContext>(app: &C, bot_id: &str, run_id: &str, turn_id: &str, note: &str) -> anyhow::Result<()> {
    let p = Pending {
        run_id: run_id.to_string(),
        turn_id: turn_id.to_string(),
        note: note.to_string(),
        stage: Stage::Owed,
        new_turn: None,
        proof: None,
        esc_at: None,
    };
    owe(app, bot_id, p).await
}

/// 插隊送出的送出鍵生效了：收掉被插隊的那一筆，同一個交易把新的那一則掛上 run（#120）。其餘同 [`interrupted`]。
pub async fn send_now_interrupted<C: InterruptionContext>(app: &C, bot_id: &str, run_id: &str, turn_id: &str, new_turn: &str) -> anyhow::Result<()> {
    let p = Pending {
        run_id: run_id.to_string(),
        turn_id: turn_id.to_string(),
        note: SEND_NOW_NOTE.to_string(),
        stage: Stage::Owed,
        new_turn: Some(NewTurn { id: new_turn.to_string(), delivery: None }),
        proof: None,
        esc_at: None,
    };
    owe(app, bot_id, p).await
}

/// 欠著的插隊送出，新那一則的送達結果後來才知道：記在帳上（連同送出的那一刻），補的時候一起寫。
pub fn owe_delivery(bot_id: &str, new_turn: &str, delivery: Option<DeliveryRecord>, at: String) {
    let mut m = ledger().lock().unwrap_or_else(|e| e.into_inner());
    let list = m.get_mut(bot_id).into_iter().flatten();
    if let Some(n) = list.filter_map(|p| p.new_turn.as_mut()).find(|n| n.id == new_turn) {
        n.delivery = delivery.map(|rec| (rec, at));
    }
}

async fn owe<C: InterruptionContext>(app: &C, bot_id: &str, mut p: Pending) -> anyhow::Result<()> {
    if p.new_turn.is_none() {
        p.new_turn = pending(bot_id).into_iter().find(|x| x.turn_id == p.turn_id).and_then(|x| x.new_turn);
    }
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

/// 從 log 認 Esc 生效的證據時，按鍵時刻往前放寬這麼多：log 與 daemon 是同一台的時鐘，只差毫秒的捨入。
const ESC_LOG_SLACK: Duration = Duration::from_secs(1);

/// Esc 不知道進了沒有（herdr 沒回）：先看 log（#223）。claude 2.1.276～2.1.278 按 Esc 不送 Stop／StopFailure，回聲等不到；
/// Esc 生效當下 transcript 就寫了中斷標記（codex 是 rollout 的 `turn_aborted`）。看到了就照「做了」收：`Ok(true)`，
/// DB 寫不進去就記成欠著、回 `Err`。還看不到就記成待證、排定時重試再看，那一筆留在 in_flight（不假定打斷）：`Ok(false)`。
///
/// 呼叫端握著 bot 鎖，**放鎖之前**呼叫：§4.3 備援計時器等的是同一把鎖，放了鎖它就先把回合收成 `completed_fallback`、
/// 串流到一半的字當回覆。
pub async fn unconfirmed(
    app: &impl InterruptionContext,
    bot_id: &str,
    run_id: &str,
    turn_id: &str,
    note: &str,
    esc_at: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let mut p = Pending {
        run_id: run_id.to_string(),
        turn_id: turn_id.to_string(),
        note: note.to_string(),
        stage: Stage::Unconfirmed,
        new_turn: None,
        proof: None,
        esc_at: Some(esc_at),
    };
    if esc_landed(app, bot_id, &p).await {
        tracing::info!(bot = bot_id, turn = turn_id, "herdr 沒回 Esc，但 log 裡已經有這次的中斷紀錄：照打斷收");
        p.stage = Stage::Owed;
        return owe(app, bot_id, p).await.map(|()| true);
    }
    tracing::warn!(bot = bot_id, turn = turn_id, "不知道打斷的鍵有沒有進 pane：回合留在 in_flight，等證據");
    record(bot_id, p);
    schedule_retry(app, bot_id);
    Ok(false)
}

/// 待證的 Esc：log 裡在按鍵之後（而且不早於這一回合開始）出現了使用者中斷的紀錄嗎？讀不到＝沒有證據。
/// 不早於回合開始：上一回合被按停留下的標記不能拿來證明這一次。
async fn esc_landed(app: &impl InterruptionContext, bot_id: &str, p: &Pending) -> bool {
    let Some(esc_at) = p.esc_at else { return false };
    let (Ok(Some(bot)), Ok(Some(run))) = (db::bot(app.db(), bot_id).await, db::run(app.db(), &p.run_id).await) else { return false };
    let began = sqlx::query_scalar::<_, String>("SELECT created_at FROM turns WHERE id=?")
        .bind(&p.turn_id)
        .fetch_optional(app.db())
        .await
        .ok()
        .flatten()
        .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
        .map(|t| t.with_timezone(&Utc));
    let since = esc_at - chrono::Duration::from_std(ESC_LOG_SLACK).unwrap_or_default();
    let since = began.map_or(since, |b| b.max(since));
    app.log_interrupted_since(&bot, &run, since).await
}

/// 插隊送出的送出鍵不知道生效了沒有（#120）：被插隊的那一筆留在 in_flight，記成待證。`proof` 在的話，之後
/// transcript 裡出現那一則就是證據；Esc 式的回聲（claude 說那一回合被使用者中斷）也算。
pub fn unconfirmed_send_now(bot_id: &str, run_id: &str, turn_id: &str, proof: Option<SentProof>) {
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
            esc_at: None,
        },
    );
}

/// 結清這顆 bot 欠著／待證的收尾。呼叫端握著 bot 鎖。`Err`：有一筆 DB 還是寫不進去，那一筆的帳留著（其他的照樣結清）。
pub async fn settle_locked<C: InterruptionContext>(app: &C, bot_id: &str, evidence: Evidence) -> anyhow::Result<()> {
    let mut failed = None;
    for p in pending(bot_id) {
        if let Err(e) = settle_one(app, bot_id, &p, evidence).await {
            tracing::warn!(bot = bot_id, turn = %p.turn_id, error = %e, "欠著的收尾還是寫不進去");
            failed.get_or_insert(e);
        }
    }
    failed.map_or(Ok(()), Err)
}

async fn settle_one<C: InterruptionContext>(app: &C, bot_id: &str, p: &Pending, evidence: Evidence) -> anyhow::Result<()> {
    let now: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT status, run_id FROM turns WHERE id=?").bind(&p.turn_id).fetch_optional(app.db()).await?;
    let old_in_flight = matches!(&now, Some((status, run)) if status == "in_flight" && run.as_deref() == Some(p.run_id.as_str()));
    let new_unbound = match &p.new_turn {
        Some(n) => sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns WHERE id=? AND status='in_flight' AND run_id IS NULL")
            .bind(&n.id)
            .fetch_one(app.db())
            .await?
            > 0,
        None => false,
    };
    if !old_in_flight && !new_unbound {
        tracing::info!(bot = bot_id, turn = %p.turn_id, ?now, "打斷的帳作廢：那一筆已經不在飛了");
        forget(bot_id, &p.turn_id);
        return Ok(());
    }
    let proven = match p.stage {
        Stage::Owed => true,
        Stage::Unconfirmed => evidence == Evidence::Echo || shows(p.proof.clone()).await || esc_landed(app, bot_id, p).await,
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
async fn settle<C: InterruptionContext>(app: &C, bot_id: &str) -> anyhow::Result<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock_owned().await;
    settle_locked(app, bot_id, Evidence::Nothing).await
}

/// 收掉一筆回合（failed＋說明同一個交易，CAS 在 in_flight），**不記帳**：寫不進去就回 `Err`，由呼叫端決定——
/// 還沒動外面的（stop、重啟）就不動，已經發生的（run 結束）改用 [`interrupted`] 記成欠著（#156）。
pub async fn close_turn<C: InterruptionContext>(app: &C, bot_id: &str, run_id: &str, turn_id: &str, note: &str) -> anyhow::Result<()> {
    let p = Pending {
        run_id: run_id.to_string(),
        turn_id: turn_id.to_string(),
        note: note.to_string(),
        stage: Stage::Owed,
        new_turn: None,
        proof: None,
        esc_at: None,
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
async fn close<C: InterruptionContext>(app: &C, bot_id: &str, p: &Pending) -> anyhow::Result<()> {
    let partial = match codex_partial_for_interruption(app, bot_id, p).await {
        Ok(partial) => partial,
        Err(e) => {
            tracing::debug!(bot = bot_id, turn = %p.turn_id, error = ?e, "partial Codex reply could not be captured during interruption");
            None
        }
    };
    let mut tx = app.db().begin().await?;
    let outcome = super::turn_controller::fail_on(&mut tx, &p.turn_id, super::turn_controller::DeliveryOnFail::Keep, &p.note).await?;
    let mut notes = Vec::new();
    if outcome == super::turn_controller::Outcome::Applied {
        let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?").bind(&p.turn_id).fetch_one(&mut *tx).await?;
        if let Some((reply, snapshot)) = partial.as_ref() {
            let already_answered: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE turn_id = ? AND role = 'assistant')")
                    .bind(&p.turn_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if !already_answered {
                notes.push(
                    super::messages::insert_message_tx(
                        &mut tx,
                        &conv,
                        Some(&p.turn_id),
                        "assistant",
                        reply,
                        "terminal_fallback",
                        true,
                        Some(snapshot),
                    )
                    .await?,
                );
            }
        }
        notes.push(super::messages::insert_message_tx(&mut tx, &conv, Some(&p.turn_id), "system", &p.note, "system", false, None).await?);
    }
    if let Some(n) = &p.new_turn {
        let bound = sqlx::query(
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
                notes.push(super::messages::insert_message_tx(&mut tx, &conv, Some(&n.id), "system", why, "system", false, None).await?);
            }
        }
    }
    tx.commit().await?;
    for m in notes {
        super::messages::emit_message_added(app, bot_id, m).await;
    }
    if let Some(n) = &p.new_turn {
        if let Some((rec, at)) = &n.delivery {
            let _ = super::owed_delivery::delivered_at(app, bot_id, &n.id, *rec, at).await;
        }
        app.turn_changed(&n.id).await;
    }
    app.turn_changed(&p.turn_id).await;
    Ok(())
}

/// Read Codex's finalized TUI stream after a confirmed interruption. The captured message carries
/// `incomplete = 1`; failure to read a pane must never keep an already-sent Esc from closing the turn.
async fn codex_partial_for_interruption(app: &(impl crate::capabilities::Db + crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess), bot_id: &str, p: &Pending) -> anyhow::Result<Option<(String, String)>> {
    let Some(bot) = db::bot(app.db(), bot_id).await? else { return Ok(None) };
    if bot.kind != "codex" {
        return Ok(None);
    }
    let Some(run) = db::run(app.db(), &p.run_id).await? else { return Ok(None) };
    let Some(pane_id) = run.pane_id.as_deref() else { return Ok(None) };
    let Ok(client) = super::client_for_run(app, &run).await else { return Ok(None) };
    let client = client.client;
    let read = client.pane_read(pane_id, "recent_unwrapped", 200).await?;
    let sent = super::turn_echo_texts(app, &p.turn_id).await?;
    Ok(super::codex_partial_reply(&read.text, &sent).map(|reply| (reply, read.text)))
}

/// 重啟時從 session log 補收「重啟前就被按停」的那一筆，寫進對話的說明。
const RESTART_INTERRUPT_NOTE: &str = "interrupted by user（在 daemon 重啟之前；重啟時照 session log 補收）";

/// 重啟接回在飛的那一筆之前（#235）：它是不是在重啟前就被按停了。打斷的帳（欠著、待證）只在記憶體，使用者也可能在 daemon
/// 停著的時候直接在 pane 裡按 Esc——claude 2.1.276+ 按 Esc 不送任何 hook（#223），接回 poller 之後它會被 §4.3 備援當成答完了
/// 收掉，下一句的 Stop 還會認領它。session log 裡我們那一則之後、答完之前有中斷紀錄，就照 Esc 收。
/// `Ok(true)`＝收掉了，不必接回；`Ok(false)`＝照舊接回（沒有這個證據、讀不到 log、找不到我們那一則）。
pub async fn adopt_interrupted_on_restart(app: &impl InterruptionContext, run: &db::Run, turn: &db::Turn) -> anyhow::Result<bool> {
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(false) };
    let sent = turn_echo_texts(app, &turn.id).await?;
    if sent.is_empty() || !app.log_interrupted_after(&bot, run, &sent).await {
        return Ok(false);
    }
    close_turn(app, &run.bot_id, &run.id, &turn.id, RESTART_INTERRUPT_NOTE).await?;
    tracing::warn!(run = %run.id, turn = %turn.id, "this turn was interrupted before the restart; closed it from the session log");
    Ok(true)
}

/// run 已經結束（不是 starting／running／stopping）、回合卻還 in_flight（#156）：欠著的收尾只記在記憶體，daemon 在補上
/// 之前重啟就沒人會收——那個 run 不會再有 hook，閒置 watchdog 也只看活著的 run。重啟時補收成 failed，寫明原因。
///
/// 只收 `boot`（開機那一刻）之前就結束的 run：開機恢復讀不到時會在背景重試（#75），那時才結束的 run 的收尾在這個行程
/// 自己的記憶體帳上，不歸這裡。回 `true`＝都收完了；讀不到或有一筆收不成回 `false`，下一輪再來。
pub async fn adopt_turns_of_ended_runs(app: &impl InterruptionContext, boot: &str) -> bool {
    let rows: Vec<(String, String, String, String)> = match sqlx::query_as(
        "SELECT t.id, t.run_id, c.bot_id, r.state FROM turns t
           JOIN runs r ON r.id = t.run_id
           JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'in_flight' AND r.state NOT IN ('starting','running','stopping')
            AND (r.ended_at IS NULL OR r.ended_at <= ?)",
    )
    .bind(boot)
    .fetch_all(app.db())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not look for in-flight turns left on ended runs");
            return false;
        }
    };
    let mut done = true;
    for (turn, run, bot, state) in rows {
        match db::bot(app.db(), &bot).await {
            Ok(Some(b)) if crate::config::is_api_kind(&b.kind) => continue,
            Ok(Some(_)) => {},
            Ok(None) | Err(_) => { done = false; continue; }
        }
        let lock = app.bot_lock(&bot).await;
        let _g = lock.lock_owned().await;
        let note = format!("這一回合的 run 已經結束（{state}），當時沒能把回合收掉；daemon 重啟時補收。");
        match close_turn(app, &bot, &run, &turn, &note).await {
            Ok(()) => tracing::warn!(turn = %turn, run = %run, %state, "closed a turn left in flight on an ended run"),
            Err(e) => {
                tracing::warn!(turn = %turn, error = %e, "could not close a turn left in flight on an ended run");
                done = false;
            }
        }
    }
    done
}

/// 重啟前正在插隊送出、還沒掛上 run 的那一則（`in_flight`、`run_id IS NULL`）：送出鍵有沒有生效沒有人知道，
/// 收它的那個 async 任務也跟著上一個行程走了。收成 failed、送達記成 unknown，不留一筆沒有 run 的 in_flight
/// （它會一直佔著維護窗口的閘門）。被插隊的那一筆照舊在它的 run 上，由 hook 或 watchdog 收。
///
/// 只收 `boot`（開機那一刻）之前建立的：開機恢復讀不到時會在背景重試（#75），那時這個行程自己正在插隊送出的那一則
/// 同樣是 in_flight、還沒掛上 run，不能被當成孤兒收掉。回 `true`＝都收完了；讀不到或有一筆收不成回 `false`。
pub async fn adopt_unbound_send_nows(app: &impl InterruptionContext, boot: &str) -> bool {
    let rows: Vec<(String, String, String)> = match sqlx::query_as(
        "SELECT t.id, t.conversation_id, c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'in_flight' AND t.run_id IS NULL AND t.created_at <= ?",
    )
    .bind(boot)
    .fetch_all(app.db())
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not look for send-now turns left unbound by a restart");
            return false;
        }
    };
    let mut done = true;
    let why = "插隊送出途中 daemon 停了：送出鍵有沒有生效不知道。正在跑的那一回合沒有被收掉；這一句若真的送出去了，回覆會以外部回合出現。";
    for (turn, conv, bot) in rows {
        match db::bot(app.db(), &bot).await {
            Ok(Some(b)) if crate::config::is_api_kind(&b.kind) => continue,
            Ok(Some(_)) => {},
            Ok(None) | Err(_) => { done = false; continue; }
        }
        let lock = app.bot_lock(&bot).await;
        let _g = lock.lock_owned().await;
        match adopt_landed_send_now(app, &bot, &turn).await {
            Ok(true) => {
                tracing::warn!(turn = %turn, bot = %bot, "a send-now had landed before the daemon stopped; finished closing the turn it interrupted");
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(turn = %turn, error = %e, "a send-now landed before the restart but its bookkeeping cannot be written yet");
                done = false;
                continue;
            }
        }
        let res: anyhow::Result<Option<db::Message>> = async {
            let mut tx = app.db().begin().await?;
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
                super::messages::emit_message_added(app, &bot, m).await;
                app.turn_changed(&turn).await;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(turn = %turn, error = %e, "could not close a send-now left unbound by a restart");
                done = false;
            }
        }
    }
    done
}

/// 重啟前還沒掛上 run 的那一則插隊送出，送出鍵其實已經生效了嗎（#229）？證據跟平常待證時一樣：這顆 bot 的 session log
/// 在它建立之後出現了這一句。生效了就照平常的收尾做（[`close`]）：run 上在飛的那一筆收成「被插隊送出打斷」、這一則掛上 run、
/// 送達記 ok＋有證據——之後 claude 替它送的 Stop 才歸它，不會認領被插隊的那一筆。
/// `Ok(true)`＝收好了；`Ok(false)`＝看不到證據、讀不到（遠端、grok），或 run 上已經沒有在飛的那一筆：照舊當成不知道。
async fn adopt_landed_send_now(app: &impl InterruptionContext, bot_id: &str, turn_id: &str) -> anyhow::Result<bool> {
    let (Some(bot), Some(run)) = (db::bot(app.db(), bot_id).await?, db::active_run(app.db(), bot_id).await?) else { return Ok(false) };
    let row: Option<(Option<String>, String)> =
        sqlx::query_as("SELECT prompt_text, created_at FROM turns WHERE id=?").bind(turn_id).fetch_optional(app.db()).await?;
    let Some((Some(text), created)) = row else { return Ok(false) };
    let Ok(since) = DateTime::parse_from_rfc3339(&created) else { return Ok(false) };
    if !app.log_shows_prompt_since(&bot, &run, &text, since.with_timezone(&Utc)).await {
        return Ok(false);
    }
    let Some(interrupted) = db::in_flight_turn(app.db(), &run.id).await? else { return Ok(false) };
    let p = Pending {
        run_id: run.id.clone(),
        turn_id: interrupted.id,
        note: SEND_NOW_NOTE.to_string(),
        stage: Stage::Owed,
        new_turn: Some(NewTurn { id: turn_id.to_string(), delivery: Delivered::Submitted.record().map(|rec| (rec, db::now())) }),
        proof: None,
        esc_at: None,
    };
    close(app, bot_id, &p).await?;
    Ok(true)
}

/// 欠著的帳沒有 hook 也要補上：DB 一時寫不進去多半很快就好。只補欠著的（待證的要證據，不是時間）。
pub fn schedule_retry<C: InterruptionContext>(app: &C, bot_id: &str) {
    if cfg!(any(test, feature = "test-hooks")) {
        return;
    }
    let app = (*app).clone();
    let bot_id = bot_id.to_string();
    tokio::spawn(Box::pin(async move {
        for secs in RETRY_DELAYS_SECS {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            if !pending(&bot_id).iter().any(|p| p.stage == Stage::Owed || p.esc_at.is_some()) {
                return;
            }
            if let Err(e) = settle(&app, &bot_id).await {
                tracing::warn!(bot = %bot_id, error = %e, "打斷的收尾還是寫不進去；稍後再試");
            }
        }
    }));
}

/// 鍵生效了、回合狀態沒寫成（#147）：503，跟 start／stop 的 `*_state_uncommitted` 同一種——外面的副作用已經做了、
/// DB 那一半還欠著（[`LcError::Uncommitted`]）。帶著那一筆的身份：重試時帶 `turn_id`，那一筆已經不在飛就不會誤按到下一回合。
pub fn uncommitted(run_id: &str, turn_id: &str, cause: Option<&anyhow::Error>) -> LcError {
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
mod retain_tests {
    use super::*;

    #[test]
    fn a_deleted_bots_unsettled_interruptions_are_dropped() {
        for bot in ["ledger-gone", "ledger-kept"] {
            record(bot, Pending { run_id: "r".into(), turn_id: format!("t-{bot}"), note: "n".into(), stage: Stage::Unconfirmed, new_turn: None, proof: None, esc_at: None });
        }
        retain_bots(&["ledger-kept".to_string()]);
        assert!(pending("ledger-gone").is_empty() && !pending("ledger-kept").is_empty());
        retain_bots(&[]);
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    use super::*;
    pub fn pending(bot_id: &str) -> Vec<()> { super::pending(bot_id).into_iter().map(|_| ()).collect() }
    pub async fn settle<C: InterruptionContext>(app: &C, bot_id: &str) -> anyhow::Result<()> { super::settle(app, bot_id).await }
}
