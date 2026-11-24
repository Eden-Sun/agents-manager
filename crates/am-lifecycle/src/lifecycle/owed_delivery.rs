//! 送達結果寫不回 DB（#149）。prompt 的副作用做完之後——字打進去送出了、交給了 `agent.prompt`、或 herdr 明確拒收
//! （`agent_blocked`）——把結果寫回那一筆回合是唯一的一步。以前那一句被 `let _ =` 吞掉：寫不進去時 API 照樣回
//! `ok`／`unverified`／`failed`，DB 卻停在 in_flight＋pending，`auto_resend`／`delivery_verified`／`delivered_at` 全丟，
//! 重啟時還被當成崩潰現場另外收。
//!
//! 跟 #147／#156 欠著的收尾（[`super::interruption`]）同一套：
//! - 外面已經發生了，所以**不重送**、也**不假裝成功**：寫不進去就記成欠著、排定時重試，直接送的回 503
//!   `delivery_state_uncommitted`（[`LcError::Uncommitted`]），排隊的 flush 回 `Err`。
//! - 會去結清的路：這顆 bot 的下一則 prompt（同一個 request id 的重試也是它，冪等那條路因此拿到寫好的結果）、
//!   下一則回合 hook（先補再對回合）、定時重試。只寫記下的那一筆，從不再送。
//! - 帳只在記憶體：daemon 在補上之前重啟就沒了。那一筆由 `rearm_progress` 收成 `unknown`（鍵按過、證不出來）；
//!   「不自動重送」在打字之前就寫死了（`auto_resend=0`），不會因為帳丟了變成可以重送。
//!
//! 「確定一個字都沒送出」的那一頭同一套（#158）：回合已經 commit（直接送）或已經從佇列認領（flush）之後才發現送不了——
//! 附件綁不上、撤不回來、插隊送出的鍵沒生效、畫面沒準備好、herdr client 拿不到、框被佔住——收回來（收成 failed、放回佇列）
//! 那一句寫不進去時，不回普通的 `failed`／`Ok`，也不讓那一筆停在 in_flight＋pending 沒人收（擋住之後每一則、佔住維護窗口的
//! 送達臨界區）。記成欠著、照上面同一套補；補的時候同樣從不送。

#[cfg(feature = "test-hooks")]
use crate::capabilities::Db;
use super::s6_ports::OwedDeliveryContext;
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 503 body 的 `error`。
pub const UNCOMMITTED: &str = "delivery_state_uncommitted";

/// 定時重試的間隔（秒），同 `interruption`：DB 一時寫不進去多半幾秒內就好，總共約四分鐘，之後交給 hook 與下一則 prompt。
const RETRY_DELAYS_SECS: [u64; 8] = [1, 2, 4, 8, 15, 30, 60, 120];

/// 欠著要寫的那一句。
#[derive(Debug, Clone)]
enum Write {
    /// 字送出去了（或送出去但證不明）：寫回送達結果。`at` 是送出的那一刻——`delivered_at` 記它，不記補寫的時間。
    Delivered { rec: DeliveryRecord, at: String },
    /// 收成 failed＋說明（同一個交易），`delivery` 記看到的：`failed`＝一個字都沒進去（herdr 拒收、本地準備失敗、
    /// 送出鍵沒生效），`unknown`＝不知道進去了沒有（插隊送出的鍵沒回）。
    Closed { delivery: &'static str, note: String },
    /// 從佇列認領了、一個字都沒打：放回佇列（照退避與重試上限，[`super::queue::put_back`]）。
    PutBack { conv: String, reason: String, wait_key: String },
    /// 認領了、一個字都沒打，而且是窗口握著（或讀不到）：放回佇列、不花送達重試，`until` 之後再試（#1683）。
    Release { until: String },
}

impl Write {
    /// 回給呼叫端的那個字。
    fn answer(&self) -> &'static str {
        match self {
            Write::Delivered { rec, .. } if rec.stored == "ok" && !rec.verified => "unverified",
            Write::Delivered { rec, .. } => rec.stored,
            Write::Closed { delivery, .. } => delivery,
            Write::PutBack { .. } | Write::Release { .. } => "queued",
        }
    }
}

macro_rules! owed_type {
    ($vis:vis,) => {
        #[derive(Debug, Clone)]
        $vis struct Owed {
            turn_id: String,
            write: Write,
        }
    };
}
#[cfg(any(test, feature = "test-hooks"))]
owed_type!(pub,);
#[cfg(not(any(test, feature = "test-hooks")))]
owed_type!(,);

/// bot → 這顆 bot 欠著的送達結果，一筆回合一條。
fn ledger() -> &'static Mutex<HashMap<String, Vec<Owed>>> {
    static M: OnceLock<Mutex<HashMap<String, Vec<Owed>>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 不在 `live` 裡的 bot 不留欠著的送達結果：bot 沒了就沒有回合可以補寫。
pub fn retain_bots(live: &[String]) {
    ledger().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| live.contains(id));
}

fn owed(bot_id: &str) -> Vec<Owed> {
    ledger().lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).cloned().unwrap_or_default()
}

/// 同一筆回合只留最新的一條。
fn record(bot_id: &str, o: Owed) {
    let mut m = ledger().lock().unwrap_or_else(|e| e.into_inner());
    let list = m.entry(bot_id.to_string()).or_default();
    list.retain(|x| x.turn_id != o.turn_id);
    list.push(o);
}

fn forget(bot_id: &str, turn_id: &str) {
    let mut m = ledger().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(list) = m.get_mut(bot_id) {
        list.retain(|x| x.turn_id != turn_id);
        if list.is_empty() {
            m.remove(bot_id);
        }
    }
}

/// `turn_id` 的送達結果還欠著嗎？欠著就是回給呼叫端的那個字。
fn owed_answer(turn_id: &str) -> Option<&'static str> {
    let m = ledger().lock().unwrap_or_else(|e| e.into_inner());
    m.values().flatten().find(|o| o.turn_id == turn_id).map(|o| o.write.answer())
}

/// 送達之後寫回結果（`at` 取此刻）。寫不進去就記成欠著、排定時重試，回 `Err`——呼叫端**不能**回普通的成功，也不能重送。
pub async fn delivered<C: OwedDeliveryContext>(app: &C, bot_id: &str, turn_id: &str, rec: DeliveryRecord) -> anyhow::Result<()> {
    delivered_at(app, bot_id, turn_id, rec, &db::now()).await
}

/// [`delivered`]，但送出的那一刻早就過了：插隊送出欠著的那一則，補收尾時才寫（#157）。
pub async fn delivered_at<C: OwedDeliveryContext>(app: &C, bot_id: &str, turn_id: &str, rec: DeliveryRecord, at: &str) -> anyhow::Result<()> {
    owe(app, bot_id, Owed { turn_id: turn_id.to_string(), write: Write::Delivered { rec, at: at.to_string() } }).await.map(drop)
}

/// 收掉一則沒送出（或不知道送出沒有）的回合：failed＋說明，`delivery` 記 `failed`／`unknown`（[`Write::Closed`]）。
/// 寫不進去同 [`delivered`]。`Ok(false)`：別的路先收掉了——回合怎麼收歸它，送達照樣補成這一次看到的。
pub async fn closed<C: OwedDeliveryContext>(app: &C, bot_id: &str, turn_id: &str, delivery: &'static str, note: &str) -> anyhow::Result<bool> {
    owe(app, bot_id, Owed { turn_id: turn_id.to_string(), write: Write::Closed { delivery, note: note.to_string() } }).await
}

/// 認領了排隊的那一筆、一個字都沒打：放回佇列並掛 retry timer。寫不進去同 [`delivered`]（#158）。
pub async fn put_back<C: OwedDeliveryContext>(app: &C, bot_id: &str, conv: &str, turn_id: &str, reason: &str, wait_key: &str) -> anyhow::Result<()> {
    let write = Write::PutBack { conv: conv.to_string(), reason: reason.to_string(), wait_key: wait_key.to_string() };
    owe(app, bot_id, Owed { turn_id: turn_id.to_string(), write }).await.map(drop)
}

/// 認領了排隊的那一筆、窗口握著（或讀不到）、一個字都沒打：放回佇列、不花送達重試，`until` 之後再試（#1683）。
/// 寫不進去同 [`put_back`]。
pub async fn release_claim<C: OwedDeliveryContext>(app: &C, bot_id: &str, turn_id: &str, until: &str) -> anyhow::Result<()> {
    owe(app, bot_id, Owed { turn_id: turn_id.to_string(), write: Write::Release { until: until.to_string() } }).await.map(drop)
}

/// 同一筆回合**任何一次寫成**都是它最新的送達結果：之前欠著的那一條跟著作廢（#149 重開）。留著的話定時重試會把舊的
/// 那一份再寫一次，蓋掉這之間別的路寫進去的較新結果。送達時間取帳上與這一次較早的那一刻——`delivered_at` 只記第一次，
/// 先寫成的若是較晚的那一刻，之後就改不回來了。寫帳的路握著 bot 鎖；唯一不拿鎖的（run 結束時補插隊送出的收尾）寫的是
/// 帳上記著的同一份結果。
async fn owe<C: OwedDeliveryContext>(app: &C, bot_id: &str, mut o: Owed) -> anyhow::Result<bool> {
    if let Write::Delivered { at, .. } = &mut o.write {
        let earlier = owed(bot_id).into_iter().filter(|x| x.turn_id == o.turn_id).find_map(|x| match x.write {
            Write::Delivered { at: first, .. } if first < *at => Some(first),
            _ => None,
        });
        if let Some(first) = earlier {
            *at = first;
        }
    }
    match write(app, bot_id, &o).await {
        Ok(applied) => {
            forget(bot_id, &o.turn_id);
            Ok(applied)
        }
        Err(e) => {
            tracing::warn!(bot = bot_id, turn = %o.turn_id, error = %e, "prompt 送出（或確定沒送出）之後，結果卻寫不進 DB：記成欠著，之後補");
            record(bot_id, o);
            schedule_retry(app, bot_id);
            #[cfg(feature = "test-hooks")]
            super::race_point::hit("owed_delivery_recorded", bot_id).await;
            Err(e)
        }
    }
}

/// 結清這顆 bot 欠著的送達結果。呼叫端握著 bot 鎖。`Err`：有一筆還是寫不進去，那一筆的帳留著（其他的照樣結清）。
pub async fn settle_locked<C: OwedDeliveryContext>(app: &C, bot_id: &str) -> anyhow::Result<()> {
    let mut failed = None;
    for o in owed(bot_id) {
        match write(app, bot_id, &o).await {
            Ok(_) => {
                tracing::info!(bot = bot_id, turn = %o.turn_id, "補上了欠著的送達結果");
                forget(bot_id, &o.turn_id);
            }
            Err(e) => {
                tracing::warn!(bot = bot_id, turn = %o.turn_id, error = %e, "欠著的送達結果還是寫不進去");
                failed.get_or_insert(e);
            }
        }
    }
    failed.map_or(Ok(()), Err)
}

/// [`settle_locked`]，自己拿 bot 鎖（定時重試用）。
async fn settle<C: OwedDeliveryContext>(app: &C, bot_id: &str) -> anyhow::Result<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock_owned().await;
    settle_locked(app, bot_id).await
}

/// 寫那一句。那一筆已經被別的路收掉（run 結束、watchdog）時，收成 failed 那句 CAS 不到，就不補說明；送達結果照寫
/// （送達與回合成敗是兩件事）——收掉的那條路若沒動 `delivery`，它不能停在 `pending`。`Ok(false)`＝別的路先收掉了。
async fn write<C: OwedDeliveryContext>(app: &C, bot_id: &str, o: &Owed) -> anyhow::Result<bool> {
    let applied = match &o.write {
        Write::Delivered { rec, at } => {
            let run_id = if rec.stored == "unknown" {
                sqlx::query_scalar::<_, Option<String>>("SELECT run_id FROM turns WHERE id=?")
                    .bind(&o.turn_id)
                    .fetch_optional(app.db())
                    .await?
                    .flatten()
            } else {
                None
            };
            app.mark_delivery(&o.turn_id, *rec, at).await?;
            if let Some(run_id) = run_id {
                app.watch_late_proof_after_owed(&run_id, bot_id, &o.turn_id);
            }
            true
        }
        Write::Closed { delivery, note } => {
            let mut tx = app.db().begin().await?;
            let out = super::turn_controller::fail_on(&mut tx, &o.turn_id, super::turn_controller::DeliveryOnFail::Keep, note).await?;
            let applied = out == super::turn_controller::Outcome::Applied;
            let only_pending = if applied { "" } else { " AND delivery='pending'" };
            sqlx::query(&format!("UPDATE turns SET delivery=? WHERE id=?{only_pending}")).bind(*delivery).bind(&o.turn_id).execute(&mut *tx).await?;
            let m = if applied {
                let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?").bind(&o.turn_id).fetch_one(&mut *tx).await?;
                Some(super::messages::insert_message_tx(&mut tx, &conv, Some(&o.turn_id), "system", note, "system", false, None).await?)
            } else {
                None
            };
            tx.commit().await?;
            if let Some(m) = m {
                app.message_added(bot_id, m).await;
            }
            applied
        }
        Write::PutBack { conv, reason, wait_key } => match app.queue_put_back(bot_id, conv, &o.turn_id, reason, wait_key).await {
            Ok(()) => true,
            Err(e) if is_unique_violation(&e) => return slot_taken(app, bot_id, o, &e).await,
            Err(e) => return Err(e),
        },
        Write::Release { until } => match app.queue_release_claim(bot_id, &o.turn_id, until).await {
            Ok(()) => true,
            Err(e) if is_unique_violation(&e) => return slot_taken(app, bot_id, o, &e).await,
            Err(e) => return Err(e),
        },
    };
    app.turn_changed(&o.turn_id).await;
    Ok(applied)
}

/// 補寫放回佇列的時候，這段對話的唯一佇列名額（`turns_one_queued`）已經被後來的那則佔走：這一則放不回去，
/// 再補也永遠一樣（#903）。它一個字都沒送出，收成 failed＋說明，別讓帳永遠欠著、佇列被一直卡住。
async fn slot_taken<C: OwedDeliveryContext>(app: &C, bot_id: &str, o: &Owed, e: &anyhow::Error) -> anyhow::Result<bool> {
    tracing::warn!(bot = bot_id, turn = %o.turn_id, error = %e, "放回佇列時名額已被後來的那則佔走：改收成 failed");
    let closed = Owed { turn_id: o.turn_id.clone(), write: Write::Closed { delivery: "failed", note: SLOT_TAKEN_NOTE.to_string() } };
    Box::pin(write(app, bot_id, &closed)).await
}

/// 放回佇列時名額已被佔走、收成 failed 的說明（#903）。
const SLOT_TAKEN_NOTE: &str = "放回佇列時名額已被後來的那則佔走，這一則一個字都沒送出，不再自動重送；請重送。";

/// `anyhow` 鏈裡有沒有 SQLite 的唯一鍵衝突（可能被 `context` 包過，所以走整條鏈）。
fn is_unique_violation(e: &anyhow::Error) -> bool {
    e.chain().filter_map(|c| c.downcast_ref::<sqlx::Error>()).any(|e| e.as_database_error().is_some_and(|d| d.is_unique_violation()))
}

/// 欠著的帳沒有 hook、也沒有下一則 prompt 時也要補上。
fn schedule_retry<C: OwedDeliveryContext>(app: &C, bot_id: &str) {
    if cfg!(any(test, feature = "test-hooks")) {
        return;
    }
    let app = (*app).clone();
    let bot_id = bot_id.to_string();
    tokio::spawn(Box::pin(async move {
        for secs in RETRY_DELAYS_SECS {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            if owed(&bot_id).is_empty() {
                return;
            }
            if let Err(e) = settle(&app, &bot_id).await {
                tracing::warn!(bot = %bot_id, error = %e, "欠著的送達結果還是寫不進去；稍後再試");
            }
        }
    }));
}

/// 503：這一則的副作用已經發生（字送出去了，或 herdr 已經拒收），或確定沒送出（#158），結果還沒寫進 DB。`delivery` 是看到的
/// 結果，`sent` 講字有沒有進去（`failed`／放回佇列的 `queued` 是 `false`，`unknown` 的話不知道，是 `null`）。重試帶同一個 `client_request_id`，拿到的就是這一則。
pub fn uncommitted(run_id: Option<&str>, turn_id: &str, message_id: &str, delivery: &str, cause: Option<&anyhow::Error>) -> LcError {
    let sent = match delivery {
        "ok" | "unverified" => json!(true),
        "failed" | "queued" => json!(false),
        _ => Value::Null,
    };
    LcError::Uncommitted(json!({
        "error": UNCOMMITTED, "run_id": run_id, "turn_id": turn_id, "message_id": message_id,
        "delivery": delivery, "sent": sent, "retryable": true,
        "message": "這一則的送達結果還沒寫進 DB（delivery 是看到的結果）；daemon 會自己補上。用同一個 client_request_id 重問會拿到這一則的結果，不會再送一次。",
        "detail": cause.map(|e| format!("{e:#}")),
    }))
}

/// 冪等那條路看到一筆 in_flight＋pending（#149）：同一把 bot 鎖裡不會有人正在送它，所以是送出去了、結果沒寫成
/// （欠著，或重啟後還沒收成 `unknown`）。不回 `pending`，也不假裝知道結果。
pub fn uncommitted_answer(t: &db::Turn, message_id: &str) -> Option<LcError> {
    (t.status == "in_flight" && t.delivery == "pending")
        .then(|| uncommitted(t.run_id.as_deref(), &t.id, message_id, owed_answer(&t.id).unwrap_or("unknown"), None))
}

/// 只需要「這一則綁在哪一筆回合上、別再送一份」的呼叫端（AGM 收件匣的通知）：送達結果還欠著時照 §6.3 當 `unknown`
/// ——字很可能進去了、DB 還沒說；herdr 明確拒收的照樣是 `failed`（沒送出去，下一次換新的一則）。其他錯誤原樣回。
pub fn owed_as_unknown(res: LcResult<PromptOut>) -> LcResult<PromptOut> {
    match res {
        Err(LcError::Uncommitted(v)) if v["error"] == UNCOMMITTED => {
            let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
            let delivery = if v["delivery"] == "failed" { "failed" } else { "unknown" };
            Ok(PromptOut { turn_id: s("turn_id"), message_id: s("message_id"), delivery: delivery.into(), send_now: None })
        }
        other => other,
    }
}

#[cfg(test)]
mod retain_tests {
    use super::*;
    #[test]
    fn a_deleted_bots_owed_delivery_is_dropped() {
        for bot in ["owed-gone", "owed-kept"] {
            record(bot, Owed { turn_id: format!("t-{bot}"), write: Write::Closed { delivery: "failed", note: "n".into() } });
        }
        retain_bots(&["owed-kept".to_string()]);
        assert!(owed("owed-gone").is_empty() && !owed("owed-kept").is_empty());
        retain_bots(&[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn uncommitted(res: LcResult<PromptOut>) -> Value {
        match res {
            Err(LcError::Uncommitted(body)) => body,
            Ok(out) => panic!("送達結果沒寫成，卻回了普通的 {:?}", out.delivery),
            Err(e) => panic!("要 503 delivery_state_uncommitted，拿到 {e:?}"),
        }
    }
    #[test]
    fn an_owed_delivery_reads_as_unknown_to_callers_that_only_need_the_turn() {
        let owed = |delivery: &str| Err(super::uncommitted(Some("r"), "t", "m", delivery, None));
        for (seen, want) in [("ok", "unknown"), ("unverified", "unknown"), ("unknown", "unknown"), ("failed", "failed")] {
            let out = owed_as_unknown(owed(seen)).expect("綁在那一筆上");
            assert_eq!((out.turn_id.as_str(), out.message_id.as_str(), out.delivery.as_str()), ("t", "m", want), "{seen}");
        }
        let other = owed_as_unknown(Err(LcError::uncommitted("send_now_state_uncommitted", "r", "x", "y")));
        assert!(matches!(other, Err(LcError::Uncommitted(_))), "別種的 503 不動");
        assert!(matches!(owed_as_unknown(Err(LcError::Bad("x".into()))), Err(LcError::Bad(_))));
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    //! #1862 L2：daemon 測試要碰的私有項，窄轉呼叫（只在測試建置存在）。
    use super::*;
    pub fn ledger() -> &'static Mutex<HashMap<String, Vec<Owed>>> { super::ledger() }
    pub fn owed(bot_id: &str) -> Vec<Owed> { super::owed(bot_id) }
    pub fn record(bot_id: &str, o: Owed)  { super::record(bot_id, o) }
    pub fn owed_answer(turn_id: &str) -> Option<&'static str> { super::owed_answer(turn_id) }
    pub async fn settle<C: OwedDeliveryContext>(app: &C, bot_id: &str) -> anyhow::Result<()> { super::settle(app, bot_id).await }
}
