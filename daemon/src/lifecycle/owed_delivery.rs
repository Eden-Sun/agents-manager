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

use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 503 body 的 `error`。
pub(crate) const UNCOMMITTED: &str = "delivery_state_uncommitted";

/// 定時重試的間隔（秒），同 `interruption`：DB 一時寫不進去多半幾秒內就好，總共約四分鐘，之後交給 hook 與下一則 prompt。
const RETRY_DELAYS_SECS: [u64; 8] = [1, 2, 4, 8, 15, 30, 60, 120];

/// 欠著要寫的那一句。
#[derive(Debug, Clone)]
enum Write {
    /// 字送出去了（或送出去但證不明）：寫回送達結果。`at` 是送出的那一刻——`delivered_at` 記它，不記補寫的時間。
    Delivered { rec: DeliveryRecord, at: String },
    /// herdr 明確拒收：一個字都沒進去，收成 failed＋說明（同一個交易）。
    Refused { note: String },
}

impl Write {
    /// 回給呼叫端的那個字。
    fn answer(&self) -> &'static str {
        match self {
            Write::Delivered { rec, .. } if rec.stored == "ok" && !rec.verified => "unverified",
            Write::Delivered { rec, .. } => rec.stored,
            Write::Refused { .. } => "failed",
        }
    }
}

#[derive(Debug, Clone)]
struct Owed {
    turn_id: String,
    write: Write,
}

/// bot → 這顆 bot 欠著的送達結果，一筆回合一條。
fn ledger() -> &'static Mutex<HashMap<String, Vec<Owed>>> {
    static M: OnceLock<Mutex<HashMap<String, Vec<Owed>>>> = OnceLock::new();
    M.get_or_init(Default::default)
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
pub(crate) async fn delivered(app: &Arc<App>, bot_id: &str, turn_id: &str, rec: DeliveryRecord) -> anyhow::Result<()> {
    owe(app, bot_id, Owed { turn_id: turn_id.to_string(), write: Write::Delivered { rec, at: db::now() } }).await
}

/// herdr 明確拒收（`agent_blocked`）：收成 failed＋說明。寫不進去同 [`delivered`]。
pub(crate) async fn refused(app: &Arc<App>, bot_id: &str, turn_id: &str, note: &str) -> anyhow::Result<()> {
    owe(app, bot_id, Owed { turn_id: turn_id.to_string(), write: Write::Refused { note: note.to_string() } }).await
}

async fn owe(app: &Arc<App>, bot_id: &str, o: Owed) -> anyhow::Result<()> {
    match write(app, bot_id, &o).await {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!(bot = bot_id, turn = %o.turn_id, error = %e, "prompt 已經送出（或被拒收），結果卻寫不進 DB：記成欠著，之後補");
            record(bot_id, o);
            schedule_retry(app, bot_id);
            Err(e)
        }
    }
}

/// 結清這顆 bot 欠著的送達結果。呼叫端握著 bot 鎖。`Err`：有一筆還是寫不進去，那一筆的帳留著（其他的照樣結清）。
pub(crate) async fn settle_locked(app: &Arc<App>, bot_id: &str) -> anyhow::Result<()> {
    let mut failed = None;
    for o in owed(bot_id) {
        match write(app, bot_id, &o).await {
            Ok(()) => {
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
async fn settle(app: &Arc<App>, bot_id: &str) -> anyhow::Result<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    settle_locked(app, bot_id).await
}

/// 寫那一句。那一筆已經被別的路收掉（run 結束、watchdog）時，收成 failed 那句 CAS 不到，就不補說明；送達結果照寫
/// （送達與回合成敗是兩件事）。
async fn write(app: &Arc<App>, bot_id: &str, o: &Owed) -> anyhow::Result<()> {
    match &o.write {
        Write::Delivered { rec, at } => mark_delivery(app, &o.turn_id, *rec, at).await?,
        Write::Refused { note } => {
            let mut tx = app.db.begin().await?;
            let out = super::turn_controller::fail_on(&mut tx, &o.turn_id, super::turn_controller::DeliveryOnFail::Failed, "agent_blocked").await?;
            let m = if out == super::turn_controller::Outcome::Applied {
                let conv: String = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?").bind(&o.turn_id).fetch_one(&mut *tx).await?;
                Some(insert_message_tx(&mut tx, &conv, Some(&o.turn_id), "system", note, "system", false, None).await?)
            } else {
                None
            };
            tx.commit().await?;
            if let Some(m) = m {
                emit_message_added(app, bot_id, m).await;
            }
        }
    }
    emit_turn(app, &o.turn_id).await;
    Ok(())
}

/// 欠著的帳沒有 hook、也沒有下一則 prompt 時也要補上。
fn schedule_retry(app: &Arc<App>, bot_id: &str) {
    if cfg!(test) {
        return;
    }
    let app = app.clone();
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        for secs in RETRY_DELAYS_SECS {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            if owed(&bot_id).is_empty() {
                return;
            }
            if let Err(e) = settle(&app, &bot_id).await {
                tracing::warn!(bot = %bot_id, error = %e, "欠著的送達結果還是寫不進去；稍後再試");
            }
        }
    });
}

/// 503：這一則的副作用已經發生（字送出去了，或 herdr 已經拒收），結果還沒寫進 DB。`delivery` 是看到的結果，
/// `sent` 講字有沒有進去（`unknown` 的話不知道，是 `null`）。重試帶同一個 `client_request_id`，拿到的就是這一則。
pub(crate) fn uncommitted(run_id: Option<&str>, turn_id: &str, message_id: &str, delivery: &str, cause: Option<&anyhow::Error>) -> LcError {
    let sent = match delivery {
        "ok" | "unverified" => json!(true),
        "failed" => json!(false),
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
pub(crate) fn uncommitted_answer(t: &db::Turn, message_id: &str) -> Option<LcError> {
    (t.status == "in_flight" && t.delivery == "pending")
        .then(|| uncommitted(t.run_id.as_deref(), &t.id, message_id, owed_answer(&t.id).unwrap_or("unknown"), None))
}

/// 只需要「這一則綁在哪一筆回合上、別再送一份」的呼叫端（AGM 收件匣的通知）：送達結果還欠著時照 §6.3 當 `unknown`
/// ——字很可能進去了、DB 還沒說；herdr 明確拒收的照樣是 `failed`（沒送出去，下一次換新的一則）。其他錯誤原樣回。
pub(crate) fn owed_as_unknown(res: LcResult<PromptOut>) -> LcResult<PromptOut> {
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
mod tests {
    use super::*;
    use crate::testing as tt;

    /// 一顆閒著、打字進 pane 的 bot（同 `delivery::api_tests::idle_bot`）。
    async fn idle_bot(env: &tt::Env, kind: &str) -> (String, String, String) {
        let app = &env.app;
        let bot = tt::claude_bot(app, &env.project_id, &format!("owed-{}", db::ulid())).await;
        sqlx::query("UPDATE bots SET kind=? WHERE id=?").bind(kind).bind(&bot.id).execute(&app.db).await.unwrap();
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-api','api-bot','test',1,?)",
        )
        .bind(&run)
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        (bot.id, conv, run)
    }

    /// SQLite 這一刻寫不進 `turns.delivery`（磁碟、鎖、I/O 錯誤）：送達結果寫回、收成 `failed`（它也寫 delivery）都寫不進去。
    async fn lose_delivery_writes(app: &Arc<App>) {
        sqlx::query("CREATE TRIGGER lost_delivery_write BEFORE UPDATE OF delivery ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();
    }

    async fn heal_delivery_writes(app: &Arc<App>) {
        sqlx::query("DROP TRIGGER lost_delivery_write").execute(&app.db).await.unwrap();
    }

    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    fn typed(env: &tt::Env) -> usize {
        env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count()
    }

    fn uncommitted(res: LcResult<PromptOut>) -> Value {
        match res {
            Err(LcError::Uncommitted(body)) => body,
            Ok(out) => panic!("送達結果沒寫成，卻回了普通的 {:?}", out.delivery),
            Err(e) => panic!("要 503 delivery_state_uncommitted，拿到 {e:?}"),
        }
    }

    /// daemon 重啟：記憶體裡的帳跟著上一個行程走了。
    fn process_restarts(bot_id: &str) {
        ledger().lock().unwrap().remove(bot_id);
    }

    /// #149 驗收一：字真的打進去、證據也有（`Submitted`），送達結果卻寫不進 DB——API 不能回普通的 `ok`。
    /// DB 那一筆停在 in_flight＋pending，而且「不自動重送」在打字之前就寫死了（驗收二）。
    #[tokio::test]
    async fn a_typed_prompt_whose_delivery_write_is_lost_is_not_reported_as_delivered() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;

        let body = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-lost").await);
        assert_eq!(body["error"], "delivery_state_uncommitted", "{body}");
        assert_eq!(body["delivery"], "ok", "看到的是送到了：{body}");
        assert_eq!(body["sent"], true, "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        let t = turn(&app, body["turn_id"].as_str().unwrap()).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"), "寫不進去就是還沒寫：不假裝寫了");
        assert_eq!(t.auto_resend, 0, "打字之前就寫死「不自動重送」：送達結果丟了也不會變成可以重送");
        assert_eq!(typed(&env), 1);
    }

    /// #149 驗收二：打過字、證不明（grok 多行）的那條路，正確紀錄是 `auto_resend=0`——寫回失敗時 DB 也不能留著預設的 1。
    /// DB 好了之後同一個 request id 重問，拿到的是寫好的 `unverified`，重送額度也照樣用掉。
    #[tokio::test]
    async fn a_typed_but_unprovable_prompt_keeps_its_no_resend_evidence_through_a_lost_write() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "grok").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), boxed: true, ..Default::default() });
        lose_delivery_writes(&app).await;

        let body = uncommitted(prompt(&app, &bot, "第一行\n第二行", "crid-grok").await);
        assert_eq!(body["delivery"], "unverified", "{body}");
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        assert_eq!(turn(&app, &turn_id).await.auto_resend, 0, "打過字的那一則絕不可自動重送");

        heal_delivery_writes(&app).await;
        let out = prompt(&app, &bot, "第一行\n第二行", "crid-grok").await.expect("DB 好了：同一個 request id 拿到寫好的結果");
        assert_eq!((out.turn_id.as_str(), out.delivery.as_str()), (turn_id.as_str(), "unverified"));
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.delivery.as_str(), t.delivery_verified, t.auto_resend), ("ok", 0, 0));
        let resends: i64 = sqlx::query_scalar("SELECT resend_count FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(resends, MAX_PROMPT_RESENDS, "回滾到舊 binary 也不會重打");
        assert_eq!(typed(&env), 1, "從頭到尾只打了一次");
    }

    /// 修法方向：同一個 request id 重試不能再送一次——那一筆已經存在、字可能已經進去了。DB 還壞著時重試照樣不是成功；
    /// 好了之後拿到寫好的結果，`delivered_at` 是當初送出的那一刻，不是補寫的那一刻。
    #[tokio::test]
    async fn retrying_the_same_request_never_types_it_again_and_answers_once_the_write_lands() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;

        let body = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-retry").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        let sent_by = db::now();
        let again = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-retry").await);
        assert_eq!(again["turn_id"], turn_id.as_str(), "{again}");
        assert_eq!(typed(&env), 1, "DB 還壞著：重試不再打字");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        heal_delivery_writes(&app).await;
        let out = prompt(&app, &bot, "Reply with PONG please", "crid-retry").await.expect("DB 好了：重試拿到寫好的結果");
        assert_eq!((out.turn_id.as_str(), out.delivery.as_str()), (turn_id.as_str(), "ok"));
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str(), t.delivery_verified), ("in_flight", "ok", 1));
        let at = t.delivered_at.expect("送達時間補上了");
        assert!(at <= sent_by, "送達時間是送出的那一刻（{at}），不是補寫的時候（{sent_by} 之後）");
        assert_eq!(typed(&env), 1);
    }

    /// #149 驗收三：停在這個半套狀態時 daemon 馬上重啟（帳只在記憶體，沒了）——不重送，收斂成看得見的 `unknown`，
    /// 不是永遠 pending（那一筆會一直佔著維護窗口的送達臨界區）。「不自動重送」與送達時間都還在。
    #[tokio::test]
    async fn a_restart_in_the_partial_state_converges_to_unknown_without_a_second_send() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;
        let body = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-restart").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();

        process_restarts(&bot);
        heal_delivery_writes(&app).await;
        let fresh = tt::restart_app(&env).await;
        crate::reconcile::rearm_progress(&fresh).await;

        let t = turn(&fresh, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "unknown"), "不是永遠 pending");
        assert_eq!(t.auto_resend, 0, "重啟之後也保住「不自動重送」");
        assert!(t.delivered_at.is_some(), "送達時間不退回建立時間：閒置 watchdog 不會把剛送出的當成老的");
        let out = prompt(&fresh, &bot, "Reply with PONG please", "crid-restart").await.expect("同一個 request id 重問");
        assert_eq!((out.turn_id.as_str(), out.delivery.as_str()), (turn_id.as_str(), "unknown"));
        assert_eq!(typed(&env), 1, "重啟之後也不重送");
    }

    /// herdr 明確回 `agent_blocked`（一個字都沒進去），收成 failed 那一句卻寫不進去：不能回普通的 `delivery=failed`
    /// （DB 還是 in_flight＋pending）。DB 好了就收成 failed＋說明，同一個 request id 重問拿到 `failed`，不會再送。
    #[tokio::test]
    async fn an_agent_blocked_refusal_whose_fail_write_is_lost_is_not_a_plain_failed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, run) = idle_bot(&env, "codex").await;
        sqlx::query("UPDATE runs SET pane_typed=0 WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        env.herdr.set_agent("api-bot", "pane-api", true);
        env.herdr.fail_next("agent.prompt", tt::Fault::RefuseWith("agent_blocked"));
        lose_delivery_writes(&app).await;

        let body = uncommitted(prompt(&app, &bot, "跑一下測試", "crid-blocked").await);
        assert_eq!(body["delivery"], "failed", "{body}");
        assert_eq!(body["sent"], false, "herdr 拒收：一個字都沒進去：{body}");
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        assert_eq!(turn(&app, &turn_id).await.status, "in_flight", "寫不進去就是還沒收");

        heal_delivery_writes(&app).await;
        let out = prompt(&app, &bot, "跑一下測試", "crid-blocked").await.expect("DB 好了");
        assert_eq!((out.turn_id.as_str(), out.delivery.as_str()), (turn_id.as_str(), "failed"));
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert!(notes.len() == 1 && notes[0].contains("agent_blocked"), "{notes:?}");
        assert_eq!(env.herdr.calls_to("agent.prompt").len(), 1, "不再送");
    }

    /// 一顆閒著的 bot 與一筆排著的 prompt（掛著 AGM 的交辦，delivered/queued）。
    async fn queued(env: &tt::Env, kind: &str, text: &str) -> (String, String, String) {
        let app = &env.app;
        let (bot, conv, _run) = idle_bot(env, kind).await;
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, client_request_id, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending','crid-q',?,?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(text)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot, "crid-q", text, &[], None, true).await.unwrap();
        crate::supervisor::store::mark_delivered(&app.db, &a.id, &turn_id, "queued").await.unwrap();
        (bot, turn_id, a.id)
    }

    /// #149 驗收四（排隊那條）：flush 打過字、證不明，送達結果寫不進去——那一筆不能停在 in_flight＋pending 沒人管，
    /// 也不能留著可以重送的預設。交辦照舊是 delivered/queued（回合真的在跑），不被當成別的狀態；DB 好了就補上，
    /// 送達時間是 flush 送出的那一刻。
    #[tokio::test]
    async fn a_queued_flush_whose_delivery_write_is_lost_is_settled_not_forgotten() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, turn_id, a) = queued(&env, "grok", "第一行\n第二行").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), boxed: true, ..Default::default() });
        lose_delivery_writes(&app).await;

        let flushed = flush_queued_locked(&app, &bot).await;
        let sent_by = db::now();
        assert!(flushed.is_err(), "送達結果沒寫成：flush 不回普通的成功");
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str(), t.auto_resend), ("in_flight", "pending", 0));
        assert_eq!(typed(&env), 1);
        let row = crate::supervisor::store::assignment(&app.db, &a).await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.delivery.as_deref()), ("delivered", Some("queued")), "交辦不因為這一下變成別的狀態");

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        heal_delivery_writes(&app).await;
        // 下一則 prompt 先把欠著的補上（回合真的還在跑，所以它自己照舊 409）。
        let _ = prompt(&app, &bot, "下一句", "crid-next").await;
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.delivery.as_str(), t.delivery_verified, t.auto_resend), ("ok", 0, 0));
        assert!(t.delivered_at.as_deref().is_some_and(|at| at <= sent_by.as_str()), "送達時間是 flush 送出的那一刻：{:?}", t.delivered_at);
        assert_eq!(typed(&env), 1, "不重送");
    }

    /// 排隊那條的 `agent_blocked`：herdr 拒收、收成 failed 那一句寫不進去——以前那一筆就永遠 in_flight＋pending
    /// （擋住佇列、佔住維護窗口的送達臨界區、交辦停在 delivered）。DB 好了要收成 failed。
    #[tokio::test]
    async fn a_queued_prompt_refused_as_blocked_while_the_db_is_down_does_not_stay_in_flight() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, turn_id, _a) = queued(&env, "claude", "跑一下測試").await;
        let run = db::active_run(&app.db, &bot).await.unwrap().unwrap().id;
        sqlx::query("UPDATE runs SET pane_typed=0 WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        env.herdr.set_agent("api-bot", "pane-api", true);
        env.herdr.fail_next("agent.prompt", tt::Fault::RefuseWith("agent_blocked"));
        lose_delivery_writes(&app).await;

        assert!(flush_queued_locked(&app, &bot).await.is_err(), "收不成 failed：flush 不回普通的成功");
        assert_eq!(turn(&app, &turn_id).await.status, "in_flight");

        heal_delivery_writes(&app).await;
        let _ = prompt(&app, &bot, "下一句", "crid-next").await;
        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "DB 好了就收掉，不留永久 in_flight");
        let resent = env.herdr.calls_to("agent.prompt").iter().filter(|p| p["text"] == "跑一下測試").count();
        assert_eq!(resent, 1, "被拒收的那一則不再送（「下一句」照常送出）");
    }

    /// #149 驗收四（AGM 派工）：送達結果沒寫成的 503 不是「沒送出」——交辦不能被記成送達（DB 還是 pending），
    /// 也不能花掉重試、最後被判 dispatch_failed（工作其實在跑）。留在 queued 等一下再問同一個 crid，拿到寫好的結果才記。
    #[tokio::test]
    async fn an_assignment_whose_delivery_write_was_lost_is_held_not_failed() {
        use crate::supervisor::store;
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        let mgr = tt::claude_bot(&app, &env.project_id, "owed-mgr").await;
        store::get_or_init(&app.db).await.unwrap();
        store::set_env(&app.db, &mgr.id, &env.project_id, "/tmp").await.unwrap();
        let a = store::insert_assignment(&app.db, None, &bot, "crid-assign", "Reply with PONG please", &[], None, true).await.unwrap();
        lose_delivery_writes(&app).await;

        crate::supervisor::controller::dispatch(&app, &a.id).await;
        let row = store::assignment(&app.db, &a.id).await.unwrap().unwrap();
        assert_eq!(row.status, "queued", "DB 還是 pending：不記成送達 {:?}", row.delivery);
        assert_eq!(row.attempts, 0, "不花重試額度");
        assert!(row.error.as_deref().is_some_and(|e| e.contains("delivery_state_uncommitted")), "{:?}", row.error);

        // 重試額度早就用完的那種也一樣：不判 dispatch_failed。
        sqlx::query("UPDATE supervisor_assignments SET attempts=5, next_attempt_at=NULL WHERE id=?").bind(&a.id).execute(&app.db).await.unwrap();
        crate::supervisor::controller::dispatch(&app, &a.id).await;
        assert_eq!(store::assignment(&app.db, &a.id).await.unwrap().unwrap().status, "queued", "工作可能在跑：不判失敗");
        assert_eq!(typed(&env), 1);

        heal_delivery_writes(&app).await;
        sqlx::query("UPDATE supervisor_assignments SET next_attempt_at=NULL WHERE id=?").bind(&a.id).execute(&app.db).await.unwrap();
        crate::supervisor::controller::dispatch(&app, &a.id).await;
        let row = store::assignment(&app.db, &a.id).await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.delivery.as_deref()), ("delivered", Some("ok")));
        assert_eq!(turn(&app, row.turn_id.as_deref().unwrap()).await.delivery, "ok", "交辦與 DB 說的是同一件事");
        assert_eq!(typed(&env), 1, "從頭到尾只送一次");
    }
    /// 沒有 hook、也沒有下一則 prompt：定時重試自己把欠著的補上（DB 還壞著的那一輪帳留著），送達時間是送出的那一刻。
    #[tokio::test]
    async fn the_timed_retry_alone_settles_an_owed_delivery_with_its_original_time() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        lose_delivery_writes(&app).await;
        let body = uncommitted(prompt(&app, &bot, "Reply with PONG please", "crid-timer").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        let sent_by = db::now();

        settle(&app, &bot).await.expect_err("DB 還沒好：這一輪補不上");
        assert_eq!(owed_answer(&turn_id), Some("ok"), "帳留著");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        heal_delivery_writes(&app).await;
        settle(&app, &bot).await.expect("定時重試的下一輪");

        let t = turn(&app, &turn_id).await;
        assert_eq!((t.delivery.as_str(), t.delivery_verified, t.auto_resend), ("ok", 1, 0));
        assert!(t.delivered_at.as_deref().is_some_and(|at| at <= sent_by.as_str()), "{:?} 晚於 {sent_by}", t.delivered_at);
        assert_eq!(owed_answer(&turn_id), None, "帳結清了");
        assert_eq!(typed(&env), 1, "補寫從不再送");
    }

    /// 回合 hook 一進來先補帳（同 #147）：herdr 拒收、還沒收成 failed 的那一筆，不能被下一則回覆（使用者在終端自己打的）
    /// 認領成「答完了」。
    #[tokio::test]
    async fn a_reply_hook_settles_an_owed_refusal_before_matching_turns() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, _conv, run) = idle_bot(&env, "claude").await;
        sqlx::query("UPDATE runs SET pane_typed=0 WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        env.herdr.set_agent("api-bot", "pane-api", true);
        env.herdr.fail_next("agent.prompt", tt::Fault::RefuseWith("agent_blocked"));
        lose_delivery_writes(&app).await;
        let body = uncommitted(prompt(&app, &bot, "跑一下測試", "crid-hook").await);
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        heal_delivery_writes(&app).await;

        let stop = crate::hookrecv::HookBody {
            bot_id: bot.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "s-typed", "prompt_id": "p-typed", "last_assistant_message": "終端裡那一句的回覆"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();

        let t = turn(&app, &turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "被拒收的就是沒送出去");
        let pinned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(pinned, 0, "別句的回覆沒有掛到它身上");
    }

    /// AGM 收件匣的通知只要「綁在哪一筆、別再送一份」：欠著的當 `unknown`，拒收的照樣 `failed`，其他錯誤原樣回。
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
