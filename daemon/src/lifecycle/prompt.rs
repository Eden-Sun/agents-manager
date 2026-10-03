//! Delivering a prompt: the turn row, the pane write, and the queued path.

use super::*;

pub(super) async fn emit_prompt_message(app: &Arc<App>, bot_id: &str, message_id: &str) {
    if let Ok(Some(m)) = sqlx::query_as::<_, db::Message>("SELECT *, rowid AS seq FROM messages WHERE id=?")
        .bind(message_id)
        .fetch_optional(&app.db)
        .await
    {
        app.emit("message_added", json!({"bot_id": bot_id, "message": m})).await;
    }
}


/// 記下一則送達：`delivery`（CHECK 只認四種）＋ `delivery_verified`（有沒有證據）＋ `auto_resend`
/// （能不能自動重送）。後兩者是兩件事，見 [`crate::lifecycle::delivery::DeliveryRecord`]。`at` 是送出的那一刻。
/// 寫不進去回 `Err`，不吞（#149）：送出之後的呼叫端走 [`super::owed_delivery`]，記成欠著、不回普通的成功。
pub(crate) async fn mark_delivery(app: &Arc<App>, turn_id: &str, rec: DeliveryRecord, at: &str) -> anyhow::Result<()> {
    let verified = i64::from(rec.verified);
    let auto = i64::from(rec.auto_resend);
    #[cfg(test)]
    super::race_point::hit("mark_delivery_before_claim", turn_id).await;

    let mut tx = app.db.begin().await?;
    // 只有將 delivered_at 從 NULL 原子改寫成功的呼叫者建立 restamp debt；回合內保存的時間是重試唯一依據。
    let first_delivery = sqlx::query("UPDATE turns SET delivered_at=?, restamp_pending=1 WHERE id=? AND delivered_at IS NULL")
    .bind(at)
    .bind(turn_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    // 不重送的那條路照樣把重送額度用掉：回滾到不認得 `auto_resend` 的舊 binary 時，
    // 它仍然不會把同一則再打一次。
    // 這筆 UPDATE 對每份 evidence 都執行，包括沒有取得 first-delivery claim 的呼叫者。
    sqlx::query(
        "UPDATE turns SET delivery=?, delivery_verified=?, auto_resend=?,
                resend_count = CASE WHEN ? = 0 THEN MAX(resend_count, ?) ELSE resend_count END
          WHERE id=?",
    )
    .bind(rec.stored)
    .bind(verified)
    .bind(auto)
    .bind(auto)
    .bind(crate::lifecycle::MAX_PROMPT_RESENDS)
    .bind(turn_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if first_delivery {
        if let Err(e) = restamp_queued_prompt(app, turn_id).await {
            tracing::warn!(turn = turn_id, error = %e, "queued prompt restamp is durable and will be retried");
            schedule_restamp_retry(app, turn_id);
        }
    }
    Ok(())
}

/// 排過隊的一則（2026-09-30 使用者：cf-ox-2 的 daemon 通知 13:28 排進佇列、13:38:55 才送進去，畫面卻排在 13:31 那則補充上面，
/// 13:40 的回覆看起來在回錯的那一則）：第一次真的送出、而且在佇列裡等了超過 [`QUEUE_RESTAMP_SECS`] 秒時，
/// 這個回合的使用者訊息改成送出的時間，再推一次同 id 的 `message_added`（前端同 id、時間變了就重排）。
/// 回合的 `created_at` 不動（它是排進佇列的時間，別處拿它算等了多久）。補充（`sent_via`）不是這個回合的 prompt，不動。
/// Finish the durable side effect for a first-delivered prompt. All required reads and timestamp
/// writes stay in one transaction, so any SQLite failure leaves `restamp_pending` set. The event
/// carries the loaded row directly; there is no fallible post-commit lookup that can lose it.
async fn restamp_queued_prompt(app: &Arc<App>, turn_id: &str) -> anyhow::Result<()> {
    // `BEGIN IMMEDIATE`：這個交易先讀後寫，一般（deferred）交易讀完才升級成寫入時，別的寫入者正拿著鎖，SQLite 會直接回
    // `database is locked`（code 5／517），完全不等 busy_timeout——daemon 剛起來撞上前一顆還沒收乾淨的寫入就是這樣（正式環境
    // 一天 10 次）。一開始就要寫入鎖，才會等。
    let mut tx = db::begin_write(&app.db).await?;
    let target: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT created_at, delivered_at FROM turns WHERE id=? AND restamp_pending=1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((queued_at, first_at)) = target else {
        tx.rollback().await?;
        return Ok(());
    };
    let Some(first_at) = first_at else {
        anyhow::bail!("queued prompt restamp debt has no first delivered_at: {turn_id}");
    };
    let waited = match (chrono::DateTime::parse_from_rfc3339(&queued_at), chrono::DateTime::parse_from_rfc3339(&first_at)) {
        (Ok(q), Ok(a)) => (a - q).num_seconds(),
        _ => 0,
    };
    if waited <= QUEUE_RESTAMP_SECS {
        sqlx::query("UPDATE turns SET restamp_pending=0 WHERE id=? AND restamp_pending=1")
            .bind(turn_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }

    let pending_messages: Vec<db::Message> = sqlx::query_as(
        "SELECT *, rowid AS seq FROM messages WHERE turn_id=? AND role='user' AND sent_via IS NULL AND created_at<=?",
    )
    .bind(turn_id)
    .bind(&first_at)
    .fetch_all(&mut *tx)
    .await?;
    if pending_messages.is_empty() {
        sqlx::query("UPDATE turns SET restamp_pending=0 WHERE id=? AND restamp_pending=1")
            .bind(turn_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }

    // `fetch_one` preserves lookup failures (including a missing conversation) as retryable debt.
    let bot_id: String = sqlx::query_scalar(
        "SELECT c.bot_id FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE t.id=?",
    )
    .bind(turn_id)
    .fetch_one(&mut *tx)
    .await?;
    for message in pending_messages {
        if message.created_at < first_at {
            sqlx::query("UPDATE messages SET created_at=? WHERE id=? AND created_at<?")
                .bind(&first_at)
                .bind(&message.id)
                .bind(&first_at)
                .execute(&mut *tx)
                .await?;
        }
    }
    // Re-load in the same transaction, so the emitted message reflects the committed timestamp.
    let messages: Vec<db::Message> = sqlx::query_as(
        "SELECT *, rowid AS seq FROM messages WHERE turn_id=? AND role='user' AND sent_via IS NULL AND created_at<=?",
    )
    .bind(turn_id)
    .bind(&first_at)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    for message in messages {
        app.emit("message_added", json!({"bot_id": bot_id, "message": message})).await;
    }
    // Keep the marker through emission. A crash before this write replays the same id and first
    // timestamp on recovery; clients upsert by message id.
    sqlx::query("UPDATE turns SET restamp_pending=0 WHERE id=? AND delivered_at=? AND restamp_pending=1")
        .bind(turn_id)
        .bind(first_at)
        .execute(&app.db)
        .await?;
    Ok(())
}

pub(crate) async fn rearm_queued_prompt_restamps(app: &Arc<App>) -> anyhow::Result<()> {
    let turns: Vec<String> = sqlx::query_scalar("SELECT id FROM turns WHERE restamp_pending=1 ORDER BY created_at, id")
        .fetch_all(&app.db)
        .await?;
    let mut failed = None;
    for turn_id in turns {
        if let Err(e) = restamp_queued_prompt(app, &turn_id).await {
            tracing::warn!(turn = %turn_id, error = %e, "cannot finish queued prompt restamp yet");
            failed.get_or_insert(e);
        }
    }
    failed.map_or(Ok(()), Err)
}

fn schedule_restamp_retry(app: &Arc<App>, turn_id: &str) {
    let app = app.clone();
    let turn_id = turn_id.to_string();
    tokio::spawn(async move {
        let mut attempt = 0;
        loop {
            tokio::time::sleep(restamp_retry_delay(attempt)).await;
            match restamp_queued_prompt(&app, &turn_id).await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!(turn = %turn_id, error = %e, "queued prompt restamp retry failed");
                    attempt += 1;
                }
            }
        }
    });
}

fn restamp_retry_delay(attempt: usize) -> std::time::Duration {
    if cfg!(test) {
        return std::time::Duration::from_millis(20);
    }
    const SECS: [u64; 5] = [2, 5, 15, 30, 60];
    std::time::Duration::from_secs(SECS[attempt.min(SECS.len() - 1)])
}

/// 在佇列裡等超過這麼久才重標時間：一般送出（沒排隊）的那幾百毫秒不必動。
const QUEUE_RESTAMP_SECS: i64 = 5;

/// The API answer for a prompt that was not sent. `retry` → 409 (temporary: a busy box, a
/// transcript not reported yet; callers retry and assignments stay queued). Otherwise 422: this
/// prompt can never be proven on this run, and saying so beats holding it forever.
pub(crate) fn not_attempted_error(run_id: &str, not: Delivered) -> LcError {
    let Delivered::NotAttempted { reason, retry } = not else {
        return LcError::Upstream("not_attempted_error called with a delivered outcome".into());
    };
    let detail = json!({"run_id": run_id, "reason": reason, "retryable": retry, "sent": false});
    if retry {
        LcError::conflict(reason, detail)
    } else {
        LcError::Unprocessable(json!({"error": "delivery_unprovable", "reason": reason, "run_id": run_id, "sent": false}))
    }
}

/// 撤不回來（DB 出錯）：改收成 failed 了，或那一句也寫不進去、記成欠著（`owed`，#158）。
pub(super) struct Unretracted {
    cause: anyhow::Error,
    owed: Option<anyhow::Error>,
}

impl Unretracted {
    /// 收成 failed 了：這個 request id 之後只會拿到那筆失敗的回合，回 5xx（最終答案，不是可重試的 409）。還欠著：跟送達結果
    /// 寫不回去同一種 503（`sent:false`）——不回普通的錯誤讓人以為收掉了，daemon 自己補，同一個 request id 重問拿得到結果。
    pub(super) fn answer(self, run_id: &str, turn_id: &str, msg_id: &str, what: &str) -> LcError {
        match self.owed {
            Some(e) => super::owed_delivery::uncommitted(Some(run_id), turn_id, msg_id, "failed", Some(&e)),
            None => LcError::Upstream(format!("{what}: {}", self.cause)),
        }
    }
}

/// 撤不回來、改收成 failed 時寫進對話的說明。
const UNRETRACTED_NOTE: &str = "沒有送出：一個字都沒打，但這一則撤不回來，改收成 failed。";

/// 撤回一筆沒送出的 turn 的結果。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Retraction {
    /// turn 與它的訊息都撤掉了：同一個 `client_request_id` 可以原樣重送。
    Withdrawn,
    /// turn 已經不是 `in_flight`——窄窗裡別的路徑（`mark_run_exited` → `fail_in_flight`）先把它收掉了。
    /// 一個字都沒刪，照 turn 現在的樣子回答呼叫端。
    AlreadySettled,
}

/// Remove a turn and its user message that never reached the agent. They were committed before the
/// delivery so an early hook could match; nothing was typed, so nothing can match them now.
///
/// 刪之前先確認 turn 還是 `in_flight`，而且跟刪訊息在同一個交易裡：`fail_in_flight` 不拿 per-bot 鎖，
/// 會在「turn 已 commit、第一個字還沒打」這個窄窗裡把它標成 failed 並插一則「run ended」說明。訊息那句
/// 原本不帶條件，於是使用者那顆泡泡跟那則說明被一起刪掉，只留下一個空的 failed 回合（review3 L4）。
pub(super) async fn retract_unsent_turn(app: &Arc<App>, bot_id: &str, turn_id: &str, msg_id: &str) -> Result<Retraction, Unretracted> {
    let res = async {
        let mut tx = app.db.begin().await?;
        // 訊息要先刪（`messages.turn_id` 指著 turns，反過來會踩到外鍵），turn 那句才是把關的：
        // 刪不到就 rollback，連訊息那句一起退掉——效果就是「turn 不是 in_flight 時一個字都不刪」。
        // 附件的 message_id 指著這則訊息、沒有 ON DELETE。先解開再刪，不然帶圖的撤回會外鍵失敗，
        // 可重試的 409 變成 502，回合被收成假的 failed、crid 也燒掉（#646）。
        crate::attach::unbind_message(&mut tx, msg_id, turn_id).await?;
        sqlx::query("DELETE FROM messages WHERE id = ? OR turn_id = ?").bind(msg_id).bind(turn_id).execute(&mut *tx).await?;
        let gone = sqlx::query("DELETE FROM turns WHERE id = ? AND status = 'in_flight'").bind(turn_id).execute(&mut *tx).await?;
        if gone.rows_affected() == 0 {
            tx.rollback().await?;
            return anyhow::Ok(Retraction::AlreadySettled);
        }
        tx.commit().await?;
        anyhow::Ok(Retraction::Withdrawn)
    }
    .await;
    let out = match res {
        Ok(Retraction::Withdrawn) => {
            tracing::info!(turn = turn_id, "retracted a prompt that was never typed into the pane");
            Ok(Retraction::Withdrawn)
        }
        Ok(Retraction::AlreadySettled) => {
            tracing::warn!(turn = turn_id, "an unsent turn had already been closed by another path; left as it is");
            Ok(Retraction::AlreadySettled)
        }
        Err(e) => {
            tracing::error!(turn = turn_id, error = %e, "could not retract an unsent turn; failing it instead");
            // 收成 failed 也寫不進去就記成欠著、之後補（#158）：不留一筆沒人收的 in_flight＋pending。
            let owed = super::owed_delivery::closed(app, bot_id, turn_id, "failed", UNRETRACTED_NOTE).await.err();
            Err(Unretracted { cause: e, owed })
        }
    };
    emit_turn(app, turn_id).await;
    if out.as_ref().map(|r| *r == Retraction::Withdrawn).unwrap_or(false) {
        // 事件模型只有「新增／更新」，沒有「刪除」：`message_added` 與 `turn_updated` 已經廣播出去了，
        // 每個客戶端都收下了那顆泡泡與那筆進行中的回合，而 `emit_turn` 對已刪除的列是 no-op。
        // 不補一次重讀的話，畫面會留著一顆送不出去的幽靈泡泡與一個永遠不會結束的回合（review 2026-09-16）。
        // 沒刪成的兩條路沒有幽靈列要收，`emit_turn` 就夠了。
        app.emit("resync", json!({"reason": "turn_retracted", "turn_id": turn_id})).await;
    }
    out
}

/// 這筆 turn 現在的樣子，換成給呼叫端的答覆。冪等重送（同一個 `client_request_id`）與「撤回時發現
/// turn 已經被別的路徑收掉」都走這裡，同一筆 turn 才不會因為問法不同而拿到兩種答案（review3 L4）。
pub(super) async fn answer_for_turn(app: &Arc<App>, t: &db::Turn) -> LcResult<PromptOut> {
    // 第一則才是這一回合的 prompt；之後的可能是回合中補充的（`sent_via = 'supplement'`）。
    let message_id = sqlx::query_scalar::<_, String>("SELECT id FROM messages WHERE turn_id=? AND role='user' ORDER BY created_at, rowid LIMIT 1")
        .bind(&t.id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .unwrap_or_default();
    if let Some(e) = super::owed_delivery::uncommitted_answer(t, &message_id) {
        return Err(e);
    }
    // The same request asked again reports the same outcome, including "unverified".
    // 還在排隊的那筆照第一次的回答說 `queued`：它的 `delivery` 欄位是 `pending`，原樣回的話
    // 重派的交辦會被記成 delivered/pending，從此不歸排隊保險絲管（review2 deliv L1）。
    let delivery = if t.status == "queued" {
        "queued".to_string()
    } else if t.delivery == "ok" && t.delivery_verified == 0 {
        "unverified".to_string()
    } else {
        t.delivery.clone()
    };
    Ok(PromptOut { turn_id: t.id.clone(), message_id, delivery, send_now: None })
}

/// [`answer_for_turn`]，但 turn 要先從 id 讀回來（撤回撤不掉時，手上只有 id）。
pub(super) async fn answer_for_turn_id(app: &Arc<App>, turn_id: &str) -> LcResult<PromptOut> {
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::Upstream("the prompt was not sent and its turn is gone".into()))?;
    answer_for_turn(app, &t).await
}

#[derive(Debug, serde::Serialize)]
pub struct PromptOut {
    pub turn_id: String,
    pub message_id: String,
    pub delivery: String,
    /// 只有請求帶 `send_now` 時才有（issue #103）。`"interrupted"`＝真的打斷了一個進行中的回合並按了
    /// send-now 鍵；`"steered"`＝codex（#748）把字打進忙碌的 TUI、併進同一個進行中的回合（沒有打斷，`turn_id` 是被 steer 的那一回合）；`"idle"`＝當下沒有回合在飛，照一般 Enter 送出，不需要插隊；其他值是
    /// [`send_now::Refusal::code`]，也就是**沒有**插隊的原因。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_now: Option<&'static str>,
}

/// 插隊送出成功時，被打斷的那一回合會收到的系統說明。
pub(crate) const SEND_NOW_NOTE: &str = "被插隊送出打斷（claude send-now）";

/// 這一則 prompt 在維護窗口（`restart` 租約）前面算哪一類（issue #86）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// 一般送入（使用者、web、群組、工具、AGM 派工）：窗口持有期間擋下，回 409 `maintenance_window`。
    Gated,
    /// 控制面自己的送入：AGM／協調者啟動時那一則握手。**不擋**——窗口就是為了「把東西停下來再起來」
    /// 而開的，連起來時的握手都擋掉就會把控制面鎖死在自己的閘門外（issue #86 驗收第三條）。
    /// 只有 daemon 自己呼叫得到，沒有從 HTTP 進得來的路。
    ControlPlane,
}

/// 窗口握著就回 `Some(那個 409)`；**讀不到窗口狀態也擋**，回 503 `maintenance_state_unavailable`（issue #127：
/// 觀測不到租約不等於沒有租約）。`ControlPlane` 一律放行。
pub(super) async fn maintenance_refusal(app: &Arc<App>, admission: Admission) -> Option<LcError> {
    if admission == Admission::ControlPlane {
        return None;
    }
    match crate::supervisor::maintenance::window_held(app).await {
        Ok(None) => None,
        Ok(Some(w)) => Some(w.refusal()),
        Err(unreadable) => Some(unreadable.refusal()),
    }
}

pub async fn prompt(app: &Arc<App>, bot_id: &str, text: &str, client_request_id: &str) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, &[], None).await
}

/// `prompt` with images (`attach.rs`): the agent gets paths on its host; the timeline renders
/// thumbnails from `messages.attachments_json`.
#[cfg(test)]
pub async fn prompt_with(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, attachment_ids, None).await
}

/// 訊息上記的來源（`messages.relay_from`／`relay_unverified`）。daemon 自己寫的來源一律可信；
/// 只有 `POST /bots/{id}/prompt` 的相容期（沒帶 bot token，`relay_auth`）會是未驗證（issue #339）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RelaySrc<'a> {
    pub from: Option<&'a str>,
    pub unverified: bool,
}

impl<'a> RelaySrc<'a> {
    pub fn trusted(from: Option<&'a str>) -> Self {
        Self { from, unverified: false }
    }
}

/// 同 `prompt_with`，記下 `relay_from`（bot id 或哨符 `daemon`）；UI 靠它把泡泡畫在左邊。
pub async fn prompt_relayed(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, attachment_ids, relay_from).await
}

/// `POST /bots/{id}/prompt`：一般送出（`send_now=false`）或插隊送出，來源已由 `relay_auth` 驗過（issue #339）。
#[allow(clippy::too_many_arguments)]
pub async fn prompt_from_api(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
    send_now: bool,
    // 「清掉再送我這則」：先清掉框裡使用者確認過的那段（`composer_draft::clear`），框空了才打字。
    clear_draft: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, None, None, attachment_ids, relay, false, false, Admission::Gated, send_now, clear_draft, None).await
}

/// `POST /prompt` with `queue_if_busy:true`: queue a user prompt only while the active run is busy.
#[allow(clippy::too_many_arguments)]
pub async fn prompt_from_api_queue_if_busy(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
    send_now: bool,
    clear_draft: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(
        app,
        bot_id,
        text,
        client_request_id,
        None,
        None,
        attachment_ids,
        relay,
        true,
        true,
        Admission::Gated,
        send_now,
        clear_draft,
        None,
    )
    .await
}

/// Share-portal prompt route. The capability is checked inside `prompt_inner` after taking the bot lock.
pub async fn prompt_from_share_with_token(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
    queue_if_busy: bool,
    token: &str,
) -> LcResult<PromptOut> {
    prompt_inner(
        app,
        bot_id,
        text,
        client_request_id,
        None,
        None,
        attachment_ids,
        relay,
        queue_if_busy,
        queue_if_busy,
        Admission::Gated,
        false,
        None,
        Some(token),
    )
    .await
}

/// AGM 派工專用：對方正在回合中時**排隊**而不是 409（AGM 2026-09-16 裁示）。
///
/// 使用者與 web 的 `/prompt` 不走這條——那條路的語意變更要單獨評估。排進 `queued` 之後由既有的
/// `queue::flush_queued_locked` 在回合結束時送出，送達判定與證據記錄完全沿用（`Handed`／`auto_resend` 不變）。
/// 回傳 `delivery = "queued"` 代表「已排隊、還沒送」。
pub async fn prompt_relayed_queueable(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, None, None, &[], RelaySrc::trusted(relay_from), true, false, Admission::Gated, false, None, None).await
}

/// 排一筆 `queued` turn 等下一回合（只有 AGM 派工走這裡）。
///
/// 界線都靠 DB：`turns_one_queued`（每個對話最多一筆 queued）擋住「同一顆 bot 疊第二筆」，
/// `turns_client_req` 擋住「同一筆交辦重試疊出第二筆」——撞到就回原本的 409，讓呼叫端照舊退避。
/// `prompt_text` 存要送的內容（含附件路徑展開後的樣子），與 queue flush 用的是同一欄。
#[allow(clippy::too_many_arguments)]
/// 排隊用的 INSERT 失敗怎麼回（#341）：只有唯一約束衝突（每個對話最多一筆 queued、同一個 client_request_id）才是「已經有一筆在排」的 409；
/// 其他（SQLITE_BUSY、磁碟滿、schema 錯）是真的寫入失敗，照實回 502，不能偽裝成「已排隊」讓呼叫端以為送出去了、一直退避。
pub(crate) fn queue_insert_error(bot_id: &str, conv: &str, e: sqlx::Error) -> LcError {
    if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
        tracing::info!(bot = %bot_id, error = %e, "另一筆 prompt 已經在排隊，這次回 409");
        LcError::conflict("a turn is already queued for this bot", json!({"conversation_id": conv}))
    } else {
        tracing::error!(bot = %bot_id, error = %e, "排隊的 turn 寫不進去");
        up(e)
    }
}

pub(super) async fn queue_for_next_turn(
    app: &Arc<App>,
    conv: &str,
    bot_id: &str,
    text: &str,
    deliver: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    relay: RelaySrc<'_>,
) -> LcResult<PromptOut> {
    let turn_id = db::ulid();
    let msg_id = db::ulid();
    let mut tx = app.db.begin().await.map_err(up)?;
    let queued = sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, prompt_text)
         VALUES (?,?,NULL,'web','queued','pending',?,?,?)",
    )
    .bind(&turn_id)
    .bind(conv)
    .bind(client_request_id)
    .bind(db::now())
    .bind(deliver)
    .execute(&mut *tx)
    .await;
    if let Err(e) = queued {
        return Err(queue_insert_error(bot_id, conv, e));
    }
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, relay_from, relay_unverified, created_at) VALUES (?,?,?,'user',?,'web',?,?,?,?)",
    )
    .bind(&msg_id)
    .bind(conv)
    .bind(&turn_id)
    .bind(text)
    .bind(group_id)
    .bind(relay.from)
    .bind(relay.unverified)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    emit_prompt_message(app, bot_id, &msg_id).await;
    emit_turn(app, &turn_id).await;
    tracing::info!(bot = %bot_id, turn = %turn_id, "對方回合中：prompt 排進佇列，等回合結束再送");
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: "queued".into(), send_now: None })
}

/// The screen checks every prompt passes before text enters the pane — shared with the queue
/// flush (review 2026-09-12 #6: the flush skipped them and typed into codex's `/model` menu).
/// Refusals insert a system hint and 409 with `needs_login` / `dialog_open` / `picker_open`.
fn pane_guard_retry(run: &db::Run, reason: &'static str) -> LcError {
    LcError::conflict(reason, json!({"run_id": run.id, "retryable": true}))
}

async fn read_guard_screen(client: &HerdrClient, pane: &str, run: &db::Run, reason: &'static str) -> LcResult<String> {
    client.pane_read(pane, "visible", 80).await.map(|r| r.text).map_err(|e| {
        tracing::warn!(run = %run.id, error = %e, "cannot inspect the pane before prompt delivery");
        pane_guard_retry(run, reason)
    })
}

fn agent_prompt_dialog_reason(kind: &str, screen: &str) -> Option<&'static str> {
    match kind {
        "claude" if crate::tui_prompts::stuck_at_login(screen) => Some("needs_login"),
        "claude" if crate::tui_prompts::dangerous_rm_prompt(screen).is_some() => Some("dangerous_rm_pending"),
        "claude" if crate::tui_prompts::is_auto_mode_offer(screen)
            || crate::tui_prompts::is_switch_model_dialog(screen) => Some("dialog_open"),
        "grok" if crate::tui_prompts::is_grok_trust_dialog(screen) => Some("dialog_open"),
        "codex" if crate::codex_live::picker_open(screen) => Some("picker_open"),
        // #782：帳號安全提醒橫幅開著時 agent.prompt 打進去的開頭數字一樣會被當成選項吃掉。
        "codex" if super::codex_banner::blocks_typing(screen) => Some(super::codex_banner::REASON),
        _ => None,
    }
}

/// Last read-only fence for the `agent.prompt` route, which otherwise bypasses the pane composer.
pub(crate) async fn final_agent_prompt_guard(
    app: &Arc<App>,
    client: &super::RunClient,
    run: &db::Run,
    bot: &db::Bot,
    target: &str,
) -> Result<(), &'static str> {
    // Read authority before any final run/pane inspection; these reads and the eventual RPC use the
    // client captured from this exact HostConn, never a fresh lookup by host name.
    if !app.hosts.is_current(&client.fence).await {
        return Err("host_changed");
    }
    let current = match db::active_run(&app.db, &run.bot_id).await {
        Ok(Some(current)) => current,
        _ => return Err("run_unreadable"),
    };
    if current.id != run.id
        || current.state != "running"
        || current.pane_id != run.pane_id
        || current.native_session_id != run.native_session_id
    {
        return Err("run_changed");
    }
    let Some(agent) = client.agent_get(target).await.map_err(|_| "agent_unreadable")? else {
        return Err("agent_unreadable");
    };
    if agent.agent_session.is_none() {
        return Err("agent_session_missing");
    }
    let pane = run
        .pane_id
        .as_deref()
        .map(str::trim)
        .filter(|pane| !pane.is_empty())
        .unwrap_or(agent.pane_id.as_str());
    if pane.is_empty()
        || run
            .pane_id
            .as_deref()
            .is_some_and(|expected| expected.trim() != agent.pane_id)
    {
        return Err("pane_changed");
    }
    let read = client
        .pane_read(pane, "visible", 80)
        .await
        .map_err(|_| "pane_unreadable")?;
    if bot.kind == "codex" {
        super::codex_banner::observe(app, run, super::codex_banner::blocks_typing(&read.text)).await;
    }
    if let Some(reason) = agent_prompt_dialog_reason(&bot.kind, &read.text) {
        return Err(reason);
    }
    Ok(())
}

pub(crate) async fn pane_ready_for_prompt(app: &Arc<App>, bot: &db::Bot, run: &db::Run, conv: &str) -> LcResult<()> {
    let pane = match run.pane_id.as_deref().map(str::trim) {
        Some("") => return Err(pane_guard_retry(run, "pane_unreadable")),
        Some(pane) => Some(pane),
        None => None,
    };
    // A missing pane is handled by the delivery planner. Once a pane id exists, a missing client or
    // failed read is uncertainty: do not treat it as an empty screen and let prompt delivery pass.
    let client = match pane {
        Some(_) => Some(client_for_run(app, run).await.map_err(|_| pane_guard_retry(run, "pane_unreadable"))?),
        None => None,
    };
    let screen = match (pane, client.as_ref()) {
        (Some(pane), Some(client)) => read_guard_screen(client, pane, run, "composer_unreadable").await?,
        _ => String::new(),
    };

    // An unlogged claude opens on "Select login method" and looks idle; a prompt would type into the menu.
    if bot.kind == "claude" && crate::tui_prompts::stuck_at_login(&screen) {
        let identity = bot.identity.clone().unwrap_or_default();
        let hint = if identity.is_empty() {
            "這個 claude 還沒登入：到「終端」分頁選 1 完成登入，或在額度那格按「登入」。".to_string()
        } else {
            format!("身份 `{identity}` 還沒登入：到「終端」分頁選 1 完成登入，或在額度那格按「登入」。")
        };
        let _ = insert_message(app, conv, None, "system", &hint, "system", false, None).await;
        return Err(LcError::conflict("needs_login", json!({"run_id": run.id, "identity": identity, "message": hint})));
    }
    if bot.kind == "claude" {
        if let (Some(pane), Some(client)) = (pane, client.as_ref()) {
            // 2.1.281 的防誤刪框只由使用者核准。不按任何鍵、不打字進去，通知後回 409。
            if let Some(rm) = crate::tui_prompts::dangerous_rm_prompt(&screen) {
                crate::dangerous_rm::notify_once(app, run, &rm).await;
                return Err(LcError::conflict(
                    "dangerous_rm_pending",
                    json!({"run_id": run.id, "target": rm.target, "warning": rm.warning,
                           "message": "claude 停在防誤刪確認框（Dangerous rm），要使用者本人到終端回答；框關掉之後再送。"}),
                ));
            }
            // 2.1.278 首次啟動的 Auto mode 框：只替它選原本 bypass 模式的第二項。
            if crate::tui_prompts::is_auto_mode_offer(&screen) {
                #[cfg(test)]
                super::race_point::hit("claude_auto_mode_before_answer", &bot.id).await;
                let latest = read_guard_screen(client, pane, run, "pane_unreadable").await?;
                if crate::tui_prompts::is_auto_mode_offer(&latest) {
                    client.pane_send_keys(pane, &["Down", "Enter"]).await.map_err(|e| {
                        tracing::warn!(run = %run.id, error = %e, "could not answer claude's Auto mode dialog");
                        pane_guard_retry(run, "pane_guard_failed")
                    })?;
                    tokio::time::sleep(Duration::from_millis(900)).await;
                    let still = read_guard_screen(client, pane, run, "pane_unreadable").await?;
                    if crate::tui_prompts::is_auto_mode_offer(&still) {
                        let hint = "claude 2.1.278 的「Auto mode」框擋在輸入列前面，自動選「No, keep bypass permissions」沒有關掉。請到「終端」分頁選第 2 項再送一次。";
                        let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                        return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
                    }
                    tracing::info!(run = %run.id, "answered claude's auto-mode offer (keep bypass) before delivering a prompt");
                }
            }
            if crate::tui_prompts::is_switch_model_dialog(&screen) {
                let latest = read_guard_screen(client, pane, run, "pane_unreadable").await?;
                if crate::tui_prompts::is_switch_model_dialog(&latest) {
                    client.pane_send_keys(pane, &["Escape"]).await.map_err(|e| {
                        tracing::warn!(run = %run.id, error = %e, "could not close claude's model-switch dialog");
                        pane_guard_retry(run, "pane_guard_failed")
                    })?;
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    let still = read_guard_screen(client, pane, run, "pane_unreadable").await?;
                    if crate::tui_prompts::is_switch_model_dialog(&still) {
                        let hint = "claude 的「Switch model?」確認框擋在輸入列前面，關不掉。請到「終端」分頁選 1 或 2 再送一次。";
                        let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                        return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
                    }
                    tracing::info!(run = %run.id, "closed a leftover claude model-switch confirmation before delivering a prompt");
                }
            }
        }
    }
    // grok's folder-trust dialog is not recognized by herdr, so trust the bot's actual host/cwd before answering `y`.
    if bot.kind == "grok" && crate::tui_prompts::is_grok_trust_dialog(&screen) {
        if let (Some(pane), Some(client)) = (pane, client.as_ref()) {
            #[cfg(test)]
            super::race_point::hit("grok_trust_before_host", &bot.id).await;
            let authority = async {
                let host = db::bot_host(&app.db, &bot.id).await?;
                let cwd = match bot.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(c) => c.to_string(),
                    None => db::project(&app.db, &bot.project_id).await?.map(|p| p.path).unwrap_or_default(),
                };
                anyhow::Ok((host, cwd))
            };
            let (host, cwd) = match authority.await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(run = %run.id, error = %e, "cannot read the host/cwd of a grok bot; its trust dialog is left alone");
                    let hint = "grok 在問「要不要信任這個目錄」，但讀不到這顆 bot 所在的主機或目錄，沒有自動按 y。稍後會自動再試；急的話請到「終端」分頁確認後按 y。";
                    return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
                }
            };
            if cwd.is_empty() {
                tracing::warn!(bot = %bot.name, host, "grok 的信任目錄查不出來，只能按 y");
            } else {
                for warning in crate::trust::pretrust_for_start(app, bot, &host, &cwd).await {
                    tracing::warn!(bot = %bot.name, host, cwd, warning = %warning, "could not record grok folder trust");
                }
            }
            #[cfg(test)]
            super::race_point::hit("grok_trust_before_answer", &bot.id).await;
            let latest = read_guard_screen(client, pane, run, "pane_unreadable").await?;
            if crate::tui_prompts::is_grok_trust_dialog(&latest) {
                client.pane_send_keys(pane, &["y"]).await.map_err(|e| {
                    tracing::warn!(run = %run.id, error = %e, "could not answer grok's folder-trust dialog");
                    pane_guard_retry(run, "pane_guard_failed")
                })?;
                tokio::time::sleep(Duration::from_millis(1200)).await;
                #[cfg(test)]
                super::race_point::hit("grok_trust_after_answer", &bot.id).await;
                let still = read_guard_screen(client, pane, run, "pane_unreadable").await?;
                if crate::tui_prompts::is_grok_trust_dialog(&still) {
                    let hint = "grok 在問「要不要信任這個目錄」，自動按 y 沒有關掉。請到「終端」分頁按 y 再送一次。";
                    let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                    return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
                }
                tracing::info!(run = %run.id, "answered grok's folder-trust dialog before delivering a prompt");
            }
        }
    }
    // codex migration/rate-limit questions belong to the user. Ordinary pickers may be Escaped,
    // but any unreadable step in `close_picker` is returned as a refusal.
    if bot.kind == "codex" {
        if let (Some(pane), Some(client)) = (pane, client.as_ref()) {
            if crate::tui_prompts::is_codex_model_migration_prompt(&screen) {
                crate::codex_model_migration::observe_screen(app, run, &screen).await;
                return Err(LcError::conflict(
                    "dialog_open",
                    json!({"run_id": run.id, "message": crate::codex_model_migration::WAITING_HINT}),
                ));
            }
            if crate::codex_live::rate_limit_switch_prompt_open(&screen) {
                crate::codex_model_migration::observe_screen(app, run, &screen).await;
                let hint = "codex 正在問要不要為了降低額度消耗切換模型；請到「終端」選擇，daemon 不會替你選，也不會把訊息打進選單。";
                return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
            }
            // 啟動時的更新選單：不替使用者選更新、不把 prompt 打進去（Enter 會選到預設的 Update now）。
            // 補標 blocked 並在對話裡講一次原因（`observe_screen` 每個選單只講一次）；這裡的 409 不另寫訊息，排隊的 prompt 重試也不洗版。
            if crate::codex_update::update_menu_open(&screen) {
                crate::codex_model_migration::observe_screen(app, run, &screen).await;
                return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": crate::codex_update::MENU_HINT})));
            }
            if !crate::codex_live::close_picker(client, pane).await {
                let hint = "codex 的 /model 選單擋在輸入列前面或畫面讀取失敗。請到「終端」分頁確認後再送一次。";
                let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                return Err(LcError::conflict("picker_open", json!({"run_id": run.id, "message": hint})));
            }
        }
    }
    Ok(())
}

pub async fn prompt_grouped(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    deliver: Option<&str>,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, group_id, deliver, attachment_ids, RelaySrc::trusted(relay_from), false, false, Admission::Gated, false, None, None).await
}

/// 插隊送出（issue #103）：照舊寫 turn／訊息進資料庫，但**不等 idle**——對 pane 送 claude 2.1.275 的
/// send-now 鍵（`ctrl+x ctrl+s`），由 CLI 自己收掉當下那一回合，daemon 這一側把被打斷的那一筆收成
/// `failed`（`fail_in_flight`），不留一個永遠 `in_flight` 的回合。
///
/// 不合資格（不是 claude、CLI 比 2.1.275 舊、版本還不知道）時**不插隊**：照原本的路走，忙的話一樣
/// 回 409，只是 body 多帶 `send_now_refused` 說清楚為什麼沒插隊。API 走 [`prompt_from_api`]；這支是測試用的捷徑。
#[cfg(test)]
pub async fn prompt_send_now(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_from_api(app, bot_id, text, client_request_id, attachment_ids, RelaySrc::trusted(relay_from), true, None).await
}

/// daemon 自己的控制面 prompt（AGM／協調者啟動時的握手）：不受維護窗口的入場閘門管（issue #86）。
/// 只有 `supervisor` 那兩條啟動路徑用得到；一般送入一律走 [`prompt_relayed`]。
pub async fn prompt_control_plane(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, None, None, &[], RelaySrc::trusted(relay_from), false, false, Admission::ControlPlane, false, None, None).await
}

#[allow(clippy::too_many_arguments)]
async fn prompt_inner(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    deliver: Option<&str>,
    attachment_ids: &[String],
    // `from: None` = 使用者自己在畫面上打的。
    relay: RelaySrc<'_>,
    // 對方回合中時排隊而不是 409。只有 AGM 派工會給 true。
    queue_if_busy: bool,
    // User /prompt 的 `queue_if_busy:true`：只在 run 正在跑但 turn／agent 忙時排入 awaits_idle。
    queue_awaits_idle: bool,
    // 維護窗口的入場閘門要不要管這一則（issue #86）。
    admission: Admission,
    // 插隊送出（issue #103）：對方回合中時打斷它，而不是 409。只有使用者按「立刻送出」會給 true。
    want_send_now: bool,
    // 先清掉框裡的這段草稿（使用者在網頁上按「清掉再送我這則」）。
    clear_draft: Option<&str>,
    // 分享 capability 必須在 bot 鎖裡重驗，避免撤銷前通過的請求之後才送達。
    share_token: Option<&str>,
) -> LcResult<PromptOut> {
    let deliver = deliver.unwrap_or(text);
    #[cfg(test)]
    super::race_point::hit("prompt_before_bot_lock", bot_id).await;
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    if let Some(token) = share_token {
        let valid = crate::share::store::resolve(&app.db, token).await.map_err(up)?;
        if valid.as_deref() != Some(bot_id) {
            return Err(LcError::NotFound("bot".into()));
        }
        crate::share::store::touch(&app.db, bot_id).await;
    }
    // #708：移交出去的專案不收 prompt（也不排隊）：送到了也是另一顆 daemon 的回合。
    crate::handoff::refuse(&app.db, bot_id).await?;
    // 這顆如果是 AGM 因為閒置收起來的（§6.11），先用 `--resume` 把它叫醒再送——「下次要用再叫醒」
    // 的那個「下次要用」就是這裡。**在鎖裡**做（issue #123）：巡邏收機器也在同一把鎖裡判斷＋寫標記＋停機，
    // 叫醒放在鎖外的話，會夾在「叫醒檢查過、沒睡」與「拿到鎖」之間被收掉，接著只會看到 409 沒有 active run。
    // 不是睡著的 bot 只多一次索引查詢，路徑照舊。
    if let Err(e) = crate::supervisor::idle_sleep::wake_locked(app, bot_id, "有新的訊息要送進來").await {
        tracing::warn!(bot = %bot_id, error = %e, "could not wake a sleeping bot for a prompt");
    }
    // 上一次打斷欠著的收尾先補（#147）：不然那筆已經被 Esc 停掉、只是狀態沒寫成的回合，會把這一則擋成
    // 「a turn is already in flight」直到 watchdog。
    if let Err(e) = super::interruption::settle_locked(app, bot_id, super::interruption::Evidence::Nothing).await {
        tracing::warn!(bot = %bot_id, error = %e, "上一次打斷欠著的收尾還是寫不進去");
    }
    // 送達結果欠著的也先補（#149）：同一個 request id 的重試因此拿到寫好的結果，herdr 拒收、還沒收成 failed 的那一筆
    // 也不會把這一則擋成「a turn is already in flight」。
    if let Err(e) = super::owed_delivery::settle_locked(app, bot_id).await {
        tracing::warn!(bot = %bot_id, error = %e, "欠著的送達結果還是寫不進去");
    }

    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    // 已軟刪的 bot 不收 prompt（#338）：pane 可能還活著（停機中、停機失敗保留 run），打進去是打進使用者已經刪掉的東西。
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    // 專案刪除時的 child 清理可能因 DB 暫時故障而重試：那段期間 child row 可能仍是 live，但
    // 專案已定案刪除。檢查放在 per-bot 鎖內，避免驗過之後剛好被 delete_project 軟刪仍送進舊 pane。
    db::project(&app.db, &bot.project_id)
        .await
        .map_err(up)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;
    // Resolve first so an unknown id is a plain 400, not an undelivered turn.
    let files = crate::attach::resolve(app, bot_id, attachment_ids)
        .await
        .map_err(|e| LcError::Bad(e.to_string()))?;
    // 打進 pane 的字先清成 CLI 會原樣收下的樣子（#205，`pane_text`）：`prompt_text` 存清過的字，泡泡（`text`）留原文。
    let deliver = super::pane_text::for_pane(&bot.kind, &crate::attach::deliver_text(deliver, &files));
    if super::pane_text::nothing_left(text, &deliver) {
        return Err(LcError::Bad("the prompt is empty once invisible characters and terminal escapes are removed".into()));
    }

    // 2. idempotency
    if let Some(t) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
        .bind(&conv)
        .bind(client_request_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
    {
        check_same_text(app, &t, text).await?;
        return answer_for_turn(app, &t).await;
    }

    if queue_awaits_idle {
        // A queued turn can remain pending briefly after its predecessor ends; do not let a direct
        // prompt overtake that occupied user slot during the queue worker's wake-up window.
        super::busy_send::refuse_if_slot_taken(app, &conv).await?;
    }
    // 維護窗口的入場閘門（issue #86）：`restart` 租約握著的時候，daemon 這一側不再開新回合。
    // 擋在規劃與 turn 之前，所以連一列都不會建，同一個 `client_request_id` 之後原樣重送是乾淨的。
    // 冪等那一段在上面：已經送出去的那一筆照舊回它自己的結果，不會因為窗口開著就改口。
    if let Some(refusal) = maintenance_refusal(app, admission).await {
        return Err(refusal);
    }

    // 1. preconditions
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| {
        LcError::conflict("bot has no active run", json!({}))
    })?;
    if run.state != "running" {
        return Err(LcError::conflict("run is not running", json!({"run_id": run.id, "state": run.state})));
    }
    if queue_awaits_idle {
        let in_flight = db::in_flight_turn(&app.db, &run.id).await.map_err(up)?;
        if in_flight.is_some() || run.agent_status != "idle" {
            return super::busy_send::queue_awaiting_idle(app, &conv, bot_id, text, &deliver, client_request_id, group_id, relay, &files).await;
        }
    }
    if run.agent_status == "blocked" {
        return Err(LcError::conflict("agent is blocked; answer the prompt first", json!({"run_id": run.id})));
    }
    // issue #103：插隊送出。先問這顆 run 認不認得 send-now 鍵，**再**決定要不要打斷——不合資格時
    // 一個鍵都不按，照原本的路回 409，只是把原因一起講出來。
    // issue #748：codex 另有一條 steer（`[codex] instant_interrupt` 旗標＋跑著的版本 >= 0.159.0），旗標預設關。
    let codex_on = want_send_now && bot.kind == "codex" && app.cfg.get().await.codex.instant_interrupt;
    let gate = want_send_now.then(|| {
        let codex_running = crate::codex_update::running_version_of(&run.id);
        send_now::gate(&bot.kind, run.status_json.as_deref(), codex_running.as_deref(), codex_on)
    });
    let (refused, send_now_mode) = match gate {
        Some(Err(r)) => (Some(r), None),
        Some(Ok(m)) => (None, Some(m)),
        None => (None, None),
    };
    let send_now_ok = want_send_now && refused.is_none();
    // 打斷哪一筆。**現在不收**：計畫失敗（框裡有字、證據讀不到）時一個鍵都還沒按，不該先把人家的回合收掉。
    let interrupted = match db::in_flight_turn(&app.db, &run.id).await.map_err(up)? {
        Some(t) if send_now_ok => Some(t),
        Some(t) => {
            if queue_if_busy {
                return queue_for_next_turn(app, &conv, bot_id, text, &deliver, client_request_id, group_id, relay).await;
            }
            let mut detail = json!({"turn_id": t.id});
            if let Some(r) = refused {
                detail["send_now_refused"] = json!(r.code());
                detail["send_now_message"] = json!(r.message());
            }
            return Err(LcError::conflict("a turn is already in flight", detail));
        }
        None => None,
    };
    // 真的要按那顆鍵，只有在**有東西可以打斷**的時候。閒著的 bot 照一般 Enter 送出：按 send-now
    // 什麼回合都沒打斷，卻把這條路多綁一個版本前提上去。
    let send_now_active = send_now_ok && interrupted.is_some();
    // 回給呼叫端的那個字：真的插了隊、當下本來就閒著、或沒插隊的原因。
    let send_now = match (want_send_now, refused, interrupted.is_some()) {
        (false, _, _) => None,
        (true, Some(r), _) => Some(r.code()),
        (true, None, true) => Some("interrupted"),
        (true, None, false) => Some("idle"),
    };
    // `--resume` 接回之後還沒證明接回的是原本那段對話（issue #92，`resume_gate`）：一個字都不打。
    // AGM 派工排進佇列（驗證完或到期由 flush 送）；其他送入跟「對方回合中」一樣回可重試的 409。
    if let super::resume_gate::Gate::Waiting { expected, left } = super::resume_gate::check(app, &bot, &run, &conv).await {
        if queue_awaits_idle {
            let out = super::busy_send::queue_awaiting_idle(app, &conv, bot_id, text, &deliver, client_request_id, group_id, relay, &files).await?;
            schedule_flush_retry(app, bot_id, left);
            return Ok(out);
        }
        if queue_if_busy {
            let out = queue_for_next_turn(app, &conv, bot_id, text, &deliver, client_request_id, group_id, relay).await?;
            schedule_flush_retry(app, bot_id, left);
            return Ok(out);
        }
        return Err(LcError::conflict(
            "resume_unverified",
            json!({"run_id": run.id, "session_id": expected, "retry_after_s": left.as_secs().max(1)}),
        ));
    }
    // AGM 派工遇到「使用者剛按 Esc」：一樣先讓使用者拿回輸入框——排進佇列，寬限到了才送（§4.4a）。
    // 使用者自己在網頁送的（User /prompt：`queue_awaits_idle`，沒有轉寄來源）不擋：那就是他接管後要打的那一句，
    // 擋下來等 60 秒是反了（2026-10-02 prorosal：Esc 後 7 秒送的「用cc1 sonnets」卡到寬限結束才送）。
    let users_own = queue_awaits_idle && relay.from.is_none();
    if queue_if_busy && !users_own {
        if let Some(wait) = super::interrupt_grace::hold(app, &bot, &run, &conv).await {
            if queue_awaits_idle {
                let out = super::busy_send::queue_awaiting_idle(app, &conv, bot_id, text, &deliver, client_request_id, group_id, relay, &files).await?;
                schedule_flush_retry(app, bot_id, wait);
                return Ok(out);
            }
            let out = queue_for_next_turn(app, &conv, bot_id, text, &deliver, client_request_id, group_id, relay).await?;
            schedule_flush_retry(app, bot_id, wait);
            return Ok(out);
        }
    }
    pane_ready_for_prompt(app, &bot, &run, &conv).await?;
    if let Some(t) = sqlx::query_as::<_, db::Turn>(
        "SELECT * FROM turns WHERE conversation_id=? AND delivery='unknown' AND status='in_flight' LIMIT 1",
    )
    .bind(&conv)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?
    {
        return Err(LcError::conflict("a previous turn has unknown delivery; abandon it first", json!({"turn_id": t.id})));
    }
    // Resolve the client before committing: must stay a retryable 502, not a stuck `pending` turn.
    let client = client_for_run(app, &run).await?;
    if let Some(expect) = clear_draft {
        super::composer_draft::clear(app, &client, &run, &bot, expect, interrupted.is_some()).await?;
    }
    // codex steer（#748）：字照一般方式打進忙碌的 pane，併進**同一個**進行中的回合——不收舊回合、不建新回合。
    // 閒著（沒有進行中的回合）時不走這裡，照一般送出（`send_now: "idle"`）。
    if let (Some(send_now::Mode::CodexSteer), Some(turn)) = (send_now_mode, interrupted.as_ref()) {
        return super::codex_steer::steer(app, &client, &run, &bot, turn, text, &deliver, client_request_id).await;
    }
    // Decide how it will be delivered before a turn exists: a prompt that cannot be sent right now
    // (box busy, no way to prove it) must never become an in-flight turn nobody can release
    // (sol review round seven #2). A 409 keeps a supervisor assignment queued with backoff.
    // A direct prompt never waits inside the request: a missing codex rollout answers 409 and the
    // caller (or AGM's backoff) asks again.
    // 插隊送出一定要走打字那條路（`force_pane`）：herdr 的 `agent.prompt` 沒有鍵可以按，
    // 按不到 send-now 就只是把字排進 CLI 自己的佇列，等於沒插隊。
    let plan = match plan_delivery(app, &client, &run, &bot, &deliver, send_now_active, false).await.map_err(up)? {
        Ok(plan) if send_now_active => plan.submitting_with(Submit::SendNow),
        Ok(plan) => plan,
        Err(not) => return Err(super::composer_draft::with_draft(&client, &run, &bot, not_attempted_error(&run.id, not)).await),
    };

    #[cfg(test)]
    super::race_point::hit("prompt_before_turn_commit", bot_id).await;

    // 插隊送出：被打斷的那一筆**現在不收**，要等送出鍵確定生效（#120，`send_now::deliver`）。新的那一則照樣先寫進 DB
    // （維護窗口的閘門、冪等都靠它），但 `run_id` 先留空：舊的那一筆還佔著 run 的 in-flight 名額
    // （`turns_one_in_flight`），送出鍵生效時跟收掉舊的同一個交易掛上去。連續兩次插隊送出也因此不會有兩個 in_flight：
    // 兩次都在同一顆 per-bot 鎖裡排隊，第二次打斷的是第一次掛上去的那一則。

    // 3. turn + user message committed BEFORE the RPC, so an early hook can match.
    // `prompt_text` 存**實際送出**的字（群組去掉 @mention、附件路徑展開後），跟排隊那條同一欄：
    // stall 重送、畫面比對、hook 對 prompt 都讀它，不讀泡泡原文（review3 c3 M4）。
    // `auto_resend=0` 在打字之前寫死：送出之後結果寫不回來（#149），這一筆也不會變成可以自動重送；寫回時照證據打開。
    let turn_id = db::ulid();
    let mut tx = app.db.begin().await.map_err(up)?;
    let inserted = sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, prompt_text, auto_resend)
         SELECT ?, ?, ?, 'web', 'in_flight', 'pending', ?, ?, ?, 0
          WHERE EXISTS (
              SELECT 1 FROM runs
               WHERE id = ? AND bot_id = ? AND state = 'running'
                 AND agent_name IS ? AND pane_id IS ? AND herdr_session IS ?
          )",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(interrupted.is_none().then_some(run.id.as_str()))
    .bind(client_request_id)
    .bind(db::now())
    .bind(&deliver)
    .bind(&run.id)
    .bind(bot_id)
    .bind(run.agent_name.as_deref())
    .bind(run.pane_id.as_deref())
    .bind(run.herdr_session.as_deref())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    if inserted.rows_affected() == 0 {
        tx.rollback().await.map_err(up)?;
        return Err(LcError::conflict(
            "run_ended",
            json!({ "run_id": run.id, "retryable": true, "sent": false }),
        ));
    }
    let msg_id = db::ulid();
    // 真的要打斷一個進行中的回合才標「插隊」；閒著時按插隊就是一般送出。
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, relay_from, relay_unverified, sent_via, created_at) VALUES (?,?,?,'user',?,'web',?,?,?,?,?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(text)
    .bind(group_id)
    .bind(relay.from)
    .bind(relay.unverified)
    .bind(send_now_active.then_some("send_now"))
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    if let Err(e) = crate::attach::bind(app, &msg_id, &files).await {
        emit_prompt_message(app, bot_id, &msg_id).await;
        // 一個字都沒送：收成 failed＋說明（同一個交易），前端最後看到的不能是 `pending`。收不成就記成欠著、回 503（#158）——
        // DB 還是 in_flight＋pending 時不回普通的 `failed`。
        let note = format!("delivery failed: attachment binding failed: {e}");
        return match super::owed_delivery::closed(app, bot_id, &turn_id, "failed", &note).await {
            Ok(true) => Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into(), send_now }),
            Ok(false) => answer_for_turn_id(app, &turn_id).await,
            Err(err) => Err(super::owed_delivery::uncommitted(Some(&run.id), &turn_id, &msg_id, "failed", Some(&err))),
        };
    }
    emit_prompt_message(app, bot_id, &msg_id).await;
    emit_turn(app, &turn_id).await;

    // 窄窗：閘門讀完之後、這一筆 commit 之前，窗口才被拿走。一個字都還沒打，所以撤回這一筆再回 409。
    // 另一邊（`store::acquire_lease`）的條件式寫入保證它看不到已經 commit 的 turn，兩句都是單句寫入、
    // 由 SQLite 排序，所以先 commit 的那個贏：這裡贏 → acquire 拿不到；acquire 贏 → 這裡撤回。
    // 兩邊加起來才是「acquire 回 Ok 之後不會有任何一個字進 pane」（issue #86）。
    if let Some(refusal) = maintenance_refusal(app, admission).await {
        match retract_unsent_turn(app, bot_id, &turn_id, &msg_id).await {
            Ok(Retraction::Withdrawn) => return Err(refusal),
            // 這筆已經被別的路徑收掉了：照它現在的樣子回，跟同一個 request id 重送一致（review3 L4）。
            Ok(Retraction::AlreadySettled) => return answer_for_turn_id(app, &turn_id).await,
            Err(u) => return Err(u.answer(&run.id, &turn_id, &msg_id, "a maintenance window opened and the turn could not be withdrawn")),
        }
    }

    // 打第一個字之前的那一瞬：人在終端裡打字、CLI 跳出新框，都不受 bot 鎖管（#120 測試插在這裡）。
    #[cfg(test)]
    super::race_point::hit("prompt_before_typing", bot_id).await;

    // 4. deliver
    let res = match interrupted.as_ref() {
        None => execute_delivery(app, &client, &run, &bot, &deliver, plan).await,
        Some(old) => match super::send_now::deliver(app, &client, &run, &bot, &deliver, plan, old, &turn_id).await {
            super::send_now::Outcome::Interrupted(res) => res,
            super::send_now::Outcome::NotAttempted(not) => Ok(not),
            super::send_now::Outcome::NotSent(why) => return send_now_fell_through(app, bot_id, &run.id, &turn_id, &msg_id, why, false).await,
            super::send_now::Outcome::Unknown(why) => return send_now_fell_through(app, bot_id, &run.id, &turn_id, &msg_id, why, true).await,
            // 送出鍵生效了、狀態沒寫成：503，跟 interrupt／start／stop 的 `*_state_uncommitted` 同一種（#147）。
            super::send_now::Outcome::Uncommitted(e) => {
                return Err(LcError::Uncommitted(json!({
                    "error": "send_now_state_uncommitted", "run_id": run.id, "turn_id": turn_id, "interrupted_turn_id": old.id,
                    "sent": true, "retryable": true, "detail": format!("{e:#}"),
                    "message": "送出鍵已經生效了，但回合的狀態還沒寫成；daemon 會自己補上，用同一個 client_request_id 重送會拿到這一則、不會再打一次。",
                })));
            }
        },
    };
    // `delivery` 是回給呼叫端／UI 的字；`rec` 是要寫進 DB 的兩個欄位（證據、能不能重送）。
    let mut rec = DeliveryRecord { stored: "unknown", verified: false, auto_resend: true };
    let delivery = match res {
        // 打字＋無損證據。`Handed`（agent.prompt）沒有證據，API 也照證據說「unverified」，
        // 但它照舊可以自動重送（AGM 2026-09-16：證據與重送分開）。
        Ok(d @ (Delivered::Submitted | Delivered::Handed)) => {
            rec = d.record().expect("delivered outcome records");
            if matches!(d, Delivered::Submitted) { "ok" } else { "unverified" }
        }
        // Typed and submitted on a run with no lossless evidence (grok, remote, codex before its
        // session is known): delivered as far as anyone can tell, marked for a human, never re-sent.
        Ok(d @ Delivered::Unverified) => {
            rec = d.record().expect("delivered outcome records");
            "unverified"
        }
        // The box filled between the plan and the first keystroke: nothing was sent. Take the turn
        // back out so the same request id can be sent again, and answer 409 like the plan would.
        Ok(not @ Delivered::NotAttempted { .. }) => {
            match retract_unsent_turn(app, bot_id, &turn_id, &msg_id).await {
                // 撤掉了：同一個 request id 原樣重送會重來一次，照計畫階段的答案回 409／422。
                Ok(Retraction::Withdrawn) => return Err(super::composer_draft::with_draft(&client, &run, &bot, not_attempted_error(&run.id, not)).await),
                // 窄窗裡別的路徑（`mark_run_exited` → `fail_in_flight`）已經把這筆收掉並插了說明：
                // 一個字都沒刪，照 turn 現況回。回可重試的 409 只會叫呼叫端用同一個 request id 重送，
                // 而重送從冪等分支拿到的就是這一筆收掉的回合——兩次答案不一致，交辦還被記成送達失敗
                // （review3 L4）。
                Ok(Retraction::AlreadySettled) => return answer_for_turn_id(app, &turn_id).await,
                // If the turn cannot be taken back, the same request id would only ever find a failed
                // turn: answer 5xx (final for this id), not a retryable 409.
                Err(u) => return Err(u.answer(&run.id, &turn_id, &msg_id, "prompt was not sent and its turn could not be withdrawn")),
            }
        }
        // Keys were sent and the result cannot be proven: that is what `unknown` means (§6.3).
        Ok(Delivered::Unproven(why)) => {
            tracing::warn!(bot = %bot_id, reason = why, "prompt delivery could not be proven");
            "unknown"
        }
        Err(e) => {
            let blocked = e.downcast_ref::<HerdrError>().map(|h| h.code == "agent_blocked").unwrap_or(false);
            if blocked {
                // 收成 failed 寫不進去就記成欠著（#149）：DB 還是 in_flight＋pending 時不回普通的 `failed`。
                if let Err(err) = super::owed_delivery::closed(app, bot_id, &turn_id, "failed", &format!("delivery failed: {e}")).await {
                    return Err(super::owed_delivery::uncommitted(Some(&run.id), &turn_id, &msg_id, "failed", Some(&err)));
                }
                return Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into(), send_now });
            }
            tracing::warn!(error = %e, "agent.prompt delivery unknown");
            "unknown"
        }
    };
    // 寫回（寫成才推 `turn_updated`）；寫不進去就記成欠著、回 503（#149）。watchdog 照樣掛——字真的送出去了，它們每一步都重讀 DB。
    let written = super::owed_delivery::delivered(app, bot_id, &turn_id, rec).await;
    if delivery == "ok" || delivery == "unverified" {
        arm_stall(app, &run.id, bot_id, &turn_id).await;
        arm_progress(app, &run.id, bot_id, &turn_id).await;
    }
    if let Err(e) = written {
        return Err(super::owed_delivery::uncommitted(Some(&run.id), &turn_id, &msg_id, delivery, Some(&e)));
    }
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: delivery.into(), send_now })
}

/// 插隊送出沒有打斷任何東西（#120）：送出鍵沒生效（`unknown = false`）或不知道生效了沒有（`true`）。
/// 正在跑的那一回合原封不動；新的那一則收成 failed（不佔 run 的 in-flight 名額），送達記成 `failed`／`unknown`，
/// 並說清楚字可能還留在輸入框裡。收不成就記成欠著、回 503（#158）：不留一筆 run_id 為空的 in_flight 沒人收。
async fn send_now_fell_through(
    app: &Arc<App>,
    bot_id: &str,
    run_id: &str,
    turn_id: &str,
    msg_id: &str,
    why: &'static str,
    unknown: bool,
) -> LcResult<PromptOut> {
    let note = if unknown {
        format!(
            "插隊送出的結果不明（{why}）：送出鍵送出去了但 herdr 沒有回，不知道有沒有生效。正在跑的那一回合先不收；\
             若這一句其實送出去了，它的回覆會以外部回合出現。這一句也可能還留在終端的輸入框裡。"
        )
    } else {
        format!("插隊送出沒有送出（{why}）：送出鍵沒有生效，正在跑的那一回合照常進行。這一句可能還留在終端的輸入框裡。")
    };
    let delivery = if unknown { "unknown" } else { "failed" };
    match super::owed_delivery::closed(app, bot_id, turn_id, delivery, &note).await {
        Ok(true) => Ok(PromptOut {
            turn_id: turn_id.to_string(),
            message_id: msg_id.to_string(),
            delivery: delivery.into(),
            send_now: Some(if unknown { "unknown" } else { "not_sent" }),
        }),
        // 別的路徑先收掉了：照它現在的樣子回。
        Ok(false) => answer_for_turn_id(app, turn_id).await,
        Err(e) => Err(super::owed_delivery::uncommitted(Some(run_id), turn_id, msg_id, delivery, Some(&e))),
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use crate::testing as tt;

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
    }

    #[tokio::test]
    async fn xreview_prompt_refuses_a_live_bot_after_its_project_is_deleted() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE projects SET deleted_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(&f.env.project_id)
            .execute(&app.db)
            .await
            .unwrap();

        let err = prompt(&app, &f.bot_id, "must not reach the old pane", "xreview-project-deleted")
            .await
            .expect_err("a live child row under a deleted project must not accept prompt delivery");
        assert!(matches!(err, LcError::NotFound(ref what) if what == "project"), "unexpected error: {err:?}");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 0, "the rejected prompt must not create a turn");
    }

    /// #341：排隊用的 turn INSERT 真的寫不進去（不是唯一約束衝突）不能被說成「已經有一筆在排」的 409；真的重複才是 409。
    #[tokio::test]
    async fn a_queue_insert_that_really_fails_is_not_reported_as_already_queued() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("CREATE TRIGGER am_test_no_turns BEFORE INSERT ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();
        let err = queue_for_next_turn(&app, &f.conv, &f.bot_id, "x", "x", "c-fail", None, RelaySrc::default()).await.unwrap_err();
        assert!(matches!(err, LcError::Upstream(_)), "真的寫入失敗要照實回，不是 409：{err:?}");
        sqlx::query("DROP TRIGGER am_test_no_turns").execute(&app.db).await.unwrap();

        queue_for_next_turn(&app, &f.conv, &f.bot_id, "one", "one", "c-1", None, RelaySrc::default()).await.unwrap();
        let err = queue_for_next_turn(&app, &f.conv, &f.bot_id, "two", "two", "c-2", None, RelaySrc::default()).await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)), "真的已經有一筆在排才是 409：{err:?}");
    }

    async fn fixture(kind: &str, session: &str) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'prompt-test',?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(kind)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(session)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        // These exercise the agent.prompt path, which needs an agent herdr has a session bound to.
        env.herdr.set_agent("prompt-test", "pane-prompt-test", true);
        Fixture { env, bot_id, conv, run_id }
    }

    /// 帶附件的回合一個字都還沒打就撤回：要先解開 attachments.message_id，不能踩外鍵（#646）。
    #[tokio::test]
    async fn retracting_an_unsent_turn_with_an_attachment_withdraws_it() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let turn_id = db::ulid();
        let msg_id = db::ulid();
        let file_id = attachment(&app, &f.bot_id).await;
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at)
             VALUES (?,?,?,'web','in_flight','pending','crid-attach',?)",
        )
        .bind(&turn_id)
        .bind(&f.conv)
        .bind(&f.run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at)
             VALUES (?,?,?,'user','看這張圖','web',?)",
        )
        .bind(&msg_id)
        .bind(&f.conv)
        .bind(&turn_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query("UPDATE attachments SET message_id=? WHERE id=?")
            .bind(&msg_id)
            .bind(&file_id)
            .execute(&app.db)
            .await
            .unwrap();

        let out = retract_unsent_turn(&app, &f.bot_id, &turn_id, &msg_id).await;
        assert!(matches!(out, Ok(Retraction::Withdrawn)), "帶附件也要撤得掉");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        let msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id=?").bind(&msg_id).fetch_one(&app.db).await.unwrap();
        let bound: Option<String> = sqlx::query_scalar("SELECT message_id FROM attachments WHERE id=?").bind(&file_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 0);
        assert_eq!(msgs, 0);
        assert!(bound.is_none(), "附件列留著，但不再指著已刪的訊息：{bound:?}");
    }

    #[tokio::test]
    async fn a_run_exit_before_direct_turn_admission_sends_nothing_and_allows_a_clean_retry() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let app_for_exit = app.clone();
        let exiting_run = f.run_id.clone();
        super::super::race_point::arm("prompt_before_turn_commit", &f.bot_id, move || async move {
            assert_eq!(mark_run_exited(&app_for_exit, &exiting_run, "pane exited").await, RunExit::Recorded);
        });

        let out = prompt(&app, &f.bot_id, "這句不能送進已結束的 run", "race-before-turn").await;
        let Err(LcError::Conflict(body)) = out else { panic!("losing the exit race must be retryable without creating a turn: {out:?}") };
        assert_eq!(body["reason"], "run_ended", "{body}");
        assert_eq!(body["run_id"], f.run_id, "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(count, 0, "the lost admission leaves no in-flight or idempotency row");
        for method in ["agent.prompt", "pane.send_text", "pane.send_keys"] {
            assert!(f.env.herdr.calls_to(method).is_empty(), "the losing request has no delivery side effect: {method}");
        }

        let new_run = db::ulid();
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at) VALUES (?, ?, 'running', 'idle', 'test', ?)")
            .bind(&new_run).bind(&f.bot_id).bind(db::now()).execute(&app.db).await.unwrap();
        let retried = prompt(&app, &f.bot_id, "這句不能送進已結束的 run", "race-before-turn").await.unwrap();
        assert_eq!(retried.delivery, "unknown", "the mock accepts the new run's RPC path");
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND client_request_id='race-before-turn'")
            .bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(stored, 1, "the same client_request_id is reusable after the losing admission");
    }

    #[tokio::test]
    async fn a_run_exit_after_turn_admission_closes_the_new_turn() {
        let f = fixture("claude", "test").await;
        let app_for_exit = f.env.app.clone();
        let exiting_run = f.run_id.clone();
        let conversation = f.conv.clone();
        super::super::race_point::arm("prompt_before_typing", &f.bot_id, move || async move {
            let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE conversation_id=? AND client_request_id='exit-after-admission'")
                .bind(&conversation).fetch_one(&app_for_exit.db).await.unwrap();
            assert_eq!(status, "in_flight", "the prompt transaction committed before run exit");
            assert_eq!(mark_run_exited(&app_for_exit, &exiting_run, "pane exited").await, RunExit::Recorded);
        });

        let _ = prompt(&f.env.app, &f.bot_id, "在退出之前建立的回合", "exit-after-admission").await;
        let (status, notes): (String, i64) = sqlx::query_as(
            "SELECT t.status, (SELECT COUNT(*) FROM messages m WHERE m.turn_id=t.id AND m.role='system' AND m.content='run ended: pane exited')
               FROM turns t WHERE t.conversation_id=? AND t.client_request_id='exit-after-admission'",
        )
        .bind(&f.conv)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
        assert_eq!(status, "failed", "run-exit cleanup sees and closes the admitted turn");
        assert_eq!(notes, 1, "run-exit cleanup leaves its normal explanation");
    }

    async fn attachment(app: &Arc<App>, bot_id: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at)
             VALUES (?,?,'image.png','image/png',1,'/tmp/image.png','/tmp/image.png','local',?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        id
    }

    /// 在對方回合中派工：AGM 那條路排隊（queued turn），使用者的 `/prompt` 仍是 409（AGM 2026-09-16 裁示）。
    #[tokio::test]
    async fn an_agm_dispatch_queues_behind_an_in_flight_turn_but_a_user_prompt_still_gets_409() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        // 先佔住一個回合。
        let busy = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&busy)
        .bind(&f.conv)
        .bind(&f.run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        // 使用者：照舊 409，不排隊。
        let user = prompt(&app, &f.bot_id, "使用者自己打的", "user-1").await;
        assert!(matches!(user, Err(LcError::Conflict(_))), "使用者的 /prompt 仍是 409");

        // AGM 派工：排一筆 queued，內容存在 prompt_text 等 flush。
        let out = prompt_relayed_queueable(&app, &f.bot_id, "AGM 派的工作", "agm-1", Some("agm-bot")).await.unwrap();
        assert_eq!(out.delivery, "queued");
        let (status, run_id, text): (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT status, run_id, prompt_text FROM turns WHERE id=?")
                .bind(&out.turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!((status.as_str(), run_id, text.as_deref()), ("queued", None, Some("AGM 派的工作")));

        // 同一筆交辦重試：回原本那一筆，不會疊第二筆。
        let again = prompt_relayed_queueable(&app, &f.bot_id, "AGM 派的工作", "agm-1", Some("agm-bot")).await.unwrap();
        assert_eq!(again.turn_id, out.turn_id);
        assert_eq!(again.delivery, "queued", "重問一筆還在排隊的，回答跟第一次一樣（不是 turn 欄位上的 pending）");
        // 另一筆交辦想排第二筆：擋下來，照舊 409（每個對話只留一筆 queued）。
        let other = prompt_relayed_queueable(&app, &f.bot_id, "另一件事", "agm-2", Some("agm-bot")).await;
        assert!(matches!(other, Err(LcError::Conflict(_))), "每個對話只留一筆 queued");
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status='queued'")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(queued, 1);
    }

    /// 群組訊息：泡泡留原文（含 @mention），turn 上記實際送出的字，stall 重送與畫面比對讀它（review3 c3 M4）。
    #[tokio::test]
    async fn a_direct_prompt_records_the_text_actually_delivered_on_its_turn() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let out = prompt_grouped(&app, &f.bot_id, "@prompt-test 跑一次測試", "group-1:b", Some("group-1"), Some("跑一次測試"), &[], None)
            .await
            .unwrap();
        let (bubble, delivered): (String, Option<String>) = sqlx::query_as(
            "SELECT m.content, t.prompt_text FROM turns t JOIN messages m ON m.turn_id = t.id AND m.role = 'user' WHERE t.id = ?",
        )
        .bind(&out.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(bubble, "@prompt-test 跑一次測試", "泡泡照使用者打的");
        assert_eq!(delivered.as_deref(), Some("跑一次測試"), "turn 記的是送給 bot 的字");
    }

    /// 打字前 `runs.pane_typed` 寫不進去（SQLite 鎖逾時、磁碟滿）：一個字都沒打，所以是 409＋撤回 turn，
    /// 不能留一筆 `unknown` 的 in_flight 擋住之後每一則、5 分鐘後又被收成 completed_fallback（review3 c4 L5）。
    #[tokio::test]
    async fn a_pane_typed_marker_that_cannot_be_written_is_a_retryable_409_with_no_turn_left() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?")
            .bind(&f.run_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), ..Default::default() });
        sqlx::query(
            "CREATE TRIGGER pane_typed_unwritable BEFORE UPDATE OF pane_typed ON runs
             BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-pane-typed").await;
        let Err(LcError::Conflict(body)) = out else { panic!("expected a retryable 409, got {:?}", out.map(|o| o.delivery)) };
        assert_eq!(body["reason"], "pane_typed_unwritable", "{body}");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 0, "沒送出的 turn 被撤回，不是留成 unknown");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");
    }

    /// 窄窗：turn 已經 commit、第一個字還沒打的時候 pane 死掉，事件路徑 `mark_run_exited` →
    /// `fail_in_flight`（不拿 per-bot 鎖）把這筆標 failed 並插了「run ended」說明。撤回這時候
    /// 一個字都不能刪——訊息那句原本不帶條件，會把使用者的泡泡跟那則說明一起刪掉，只留一個空的
    /// failed 回合，還回可重試的 409 叫呼叫端重送，重送又從冪等分支拿到那筆失敗的回合（review3 L4）。
    ///
    /// `BEFORE UPDATE OF pane_typed` 的 trigger 正好落在那個窗裡：`RAISE(FAIL)` 只中止外層那句
    /// UPDATE，trigger 自己先做的兩筆寫入留著，等於另一條路徑插進來的效果。
    #[tokio::test]
    async fn a_turn_the_run_end_already_failed_is_left_alone_and_answered_like_a_resend() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?")
            .bind(&f.run_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), ..Default::default() });
        sqlx::query(
            "CREATE TRIGGER the_run_ends_mid_delivery BEFORE UPDATE OF pane_typed ON runs
             BEGIN
               INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at)
                 SELECT 'msg-run-ended', t.conversation_id, t.id, 'system', 'run ended: pane died', 'system', datetime('now')
                   FROM turns t WHERE t.run_id = NEW.id AND t.status = 'in_flight';
               UPDATE turns SET status = 'failed', completed_at = datetime('now')
                 WHERE run_id = NEW.id AND status = 'in_flight';
               SELECT RAISE(FAIL, 'run ended mid-delivery');
             END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-raced").await.expect("不是可重試的 409");

        // 那一筆回合留著，使用者的訊息與「run ended」說明都在。
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id = ?").bind(&out.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.status, "failed", "收掉它的是 run 結束那條路，不是撤回");
        let msgs: Vec<(String, String)> = sqlx::query_as("SELECT role, content FROM messages WHERE turn_id = ? ORDER BY role")
            .bind(&out.turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(
            msgs,
            vec![("system".to_string(), "run ended: pane died".to_string()), ("user".to_string(), "Reply with PONG please".to_string())],
            "使用者的泡泡與那則說明都不該被刪掉",
        );
        assert!(!out.message_id.is_empty(), "回的是那顆還在的泡泡");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");

        // 同一個 request id 重送：走冪等分支，答案必須跟剛剛那次一模一樣。
        let again = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-raced").await.unwrap();
        assert_eq!(
            (again.turn_id, again.message_id, again.delivery),
            (out.turn_id, out.message_id, out.delivery),
            "撤不掉時的回答要跟同一個 request id 重送拿到的一致",
        );
    }

    /// A missing run session is rejected before writing, so a retry reports the same upstream problem.
    #[tokio::test]
    async fn an_unavailable_run_session_does_not_create_a_turn() {
        let f = fixture("codex", "no-such-session").await;
        let app = f.env.app.clone();

        assert!(matches!(prompt(&app, &f.bot_id, "first", "prompt-1").await, Err(LcError::Upstream(_))));
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 0, "the unavailable client was checked before INSERT");

        assert!(matches!(prompt(&app, &f.bot_id, "second", "prompt-2").await, Err(LcError::Upstream(_))));
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
    }

    /// 別的 bot／排程送進來的 prompt 要留 `relay_from`，UI 才分得出來源（2026-09-12 使用者）。
    #[tokio::test]
    async fn a_relayed_prompt_records_who_sent_it() {
        // 一顆 bot 同時只有一個回合在飛：各用一個 fixture。
        let user = fixture("codex", "test").await;
        let user_app = user.env.app.clone();
        let mine = prompt_with(&user_app, &user.bot_id, "使用者自己打的", "prompt-user", &[]).await.unwrap();

        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let relayed = prompt_relayed(&app, &f.bot_id, "排程派的", "prompt-daemon", &[], Some(crate::agent_relay::DAEMON_SENDER))
            .await
            .unwrap();

        let from = |db: sqlx::SqlitePool, id: &str| {
            let db = db.clone();
            let id = id.to_string();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT relay_from FROM messages WHERE id = ?")
                    .bind(&id)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(from(user_app.db.clone(), &mine.message_id).await, None, "使用者自己打的不該有來源標");
        assert_eq!(
            from(app.db.clone(), &relayed.message_id).await,
            Some(crate::agent_relay::DAEMON_SENDER.to_string())
        );
    }

    /// 握住一個 `restart` 維護窗口（`mins` 為負＝已經過期）。
    async fn hold_restart_window(app: &Arc<App>, owner: &str, mins: i64) {
        let until = (chrono::Utc::now() + chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        crate::supervisor::store::acquire_lease(&app.db, "restart", owner, None, None, &until, false, None, &json!({}))
            .await
            .unwrap()
            .expect("沒有人握著，一定拿得到");
    }

    async fn turn_count(app: &Arc<App>, conv: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(conv).fetch_one(&app.db).await.unwrap()
    }

    /// issue #86：`restart` 租約握著的期間，daemon 這一側不再開新回合。擋下來要講清楚**誰**握著、
    /// 到**什麼時候**——只說「送不出去」的話呼叫端只會一直重送。連一列都不建，所以同一個
    /// `client_request_id` 之後原樣重送是乾淨的。
    ///
    /// 這一支跟 `maintenance` 那邊的 `a_window_is_not_taken_while_a_prompt_is_in_the_delivery_critical_section`
    /// 是一對：租約先到就擋 prompt，prompt 先到就擋租約，兩個不會同時進送達臨界區。
    #[tokio::test]
    async fn a_held_restart_window_refuses_new_prompts_and_says_who_holds_it() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", 5).await;
        let mut events = app.subscribe();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-in-window").await;

        let Err(LcError::Conflict(body)) = out else { panic!("expected 409, got {:?}", out.map(|o| o.delivery)) };
        assert_eq!(body["reason"], "maintenance_window", "{body}");
        assert_eq!(body["held_by"], "k8bw2f", "{body}");
        assert_eq!(body["resource"], "restart", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert!(body["expires_at"].as_str().is_some_and(|s| !s.is_empty()), "要講到什麼時候：{body}");
        assert!(body["retry_after_secs"].as_i64().unwrap_or(0) > 0, "要講還要等多久：{body}");
        assert_eq!(turn_count(&app, &f.conv).await, 0, "連 turn 都沒建");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");
        // 擋在 turn 之前，不是建完再撤：撤回那條路會先廣播 `message_added` 再補一次 `resync`，
        // 使用者看到的是一顆泡泡冒出來又消失。窗口開著時每一則 prompt 都這樣閃一下是不行的。
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await.is_err(),
            "一則都不該廣播：沒有泡泡冒出來又消失",
        );
    }

    /// 窗口 release 之後自動恢復：同一個 `client_request_id` 原樣重送就真的送出去（前一次沒留下任何列）。
    #[tokio::test]
    async fn a_released_window_lets_the_same_request_id_through_again() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", 5).await;
        assert!(prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-retry").await.is_err());

        let l = crate::supervisor::store::lease(&app.db, "restart").await.unwrap().unwrap();
        assert!(crate::supervisor::store::release_lease(&app.db, "restart", "k8bw2f", l.fence).await.unwrap());

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-retry").await.expect("窗口關了就照常送");
        assert_ne!(out.delivery, "failed", "{out:?}", out = out.delivery);
        assert_eq!(turn_count(&app, &f.conv).await, 1);
    }

    /// 過期沒 release 的租約**不會**把 daemon 鎖死：閘門看 `held_at`，`expires_at` 一到就自動不再擋，
    /// 不需要任何人來收尾（租約 TTL 上限一小時）。
    #[tokio::test]
    async fn an_expired_window_fences_nothing() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", -1).await;
        let l = crate::supervisor::store::lease(&app.db, "restart").await.unwrap().unwrap();
        assert!(l.released_at.is_none(), "沒有人 release，它只是過期了");

        prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-after-expiry").await.expect("過期的窗口不擋");
    }

    /// 控制面自己的送入（AGM／協調者啟動時的握手）不受閘門管：窗口就是為了「停下來再起來」而開的，
    /// 連起來時的握手都擋掉，控制面就被自己的閘門鎖在門外（issue #86 驗收第三條）。
    #[tokio::test]
    async fn the_control_plane_handshake_is_not_fenced() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", 5).await;

        assert!(prompt(&app, &f.bot_id, "使用者的", "prompt-user").await.is_err(), "一般送入照擋");
        prompt_control_plane(&app, &f.bot_id, "AGM 啟動握手", "agm-bootstrap-v1", Some(crate::agent_relay::DAEMON_SENDER))
            .await
            .expect("控制面的握手照樣進得去");
    }

    /// 窄窗：閘門讀完之後、這一筆 turn commit 之前，窗口才被拿走。一個字都還沒打，所以這一筆要被
    /// 撤回再回 409——留著的話它就是一筆 `in_flight`＋`pending`，正好是窗口承諾「裡面沒有」的那種。
    /// 用 `AFTER INSERT ON turns` 的 trigger 精準插進那個時點。
    #[tokio::test]
    async fn a_window_that_opens_while_the_turn_commits_takes_the_turn_back_out() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        // 這一刻還沒有人握著（`released_at` 有值）：閘門的第一次讀會放行。
        sqlx::query("INSERT INTO supervisor_leases (resource, owner, fence, released_at) VALUES ('restart','k8bw2f',7,?)")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER a_window_opens_mid_commit AFTER INSERT ON turns
             BEGIN
               UPDATE supervisor_leases SET expires_at='2099-01-01T00:00:00Z', acquired_at='2026-01-01T00:00:00Z',
                      released_at=NULL WHERE resource='restart';
             END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-raced-window").await;

        let Err(LcError::Conflict(body)) = out else { panic!("expected 409, got {:?}", out.map(|o| o.delivery)) };
        assert_eq!(body["reason"], "maintenance_window", "{body}");
        assert_eq!(body["held_by"], "k8bw2f", "{body}");
        assert_eq!(turn_count(&app, &f.conv).await, 0, "搶進來的那一筆被撤回，不是留成 in_flight+pending");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");
    }

    /// issue #127：讀不到 `restart` 租約 **不等於** 沒有窗口。窗口明明握著、只是那一刻 SELECT 失敗——
    /// 以前 `window_held` 把錯誤當成「沒有窗口」，新的 prompt 就在維護期間進了 pane。現在照窗口處理：
    /// 一個字都不送、連 turn 都不建；讀取恢復之後重新判斷，窗口還在就是「真的有窗口」的 409，
    /// 窗口收了就照常送（不會永久卡住）。
    #[tokio::test]
    async fn a_restart_lease_that_cannot_be_read_refuses_the_prompt_instead_of_letting_it_through() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", 5).await;
        crate::supervisor::maintenance::fault::break_lease_reads(&app.db).await;

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unreadable").await;

        assert!(out.is_err(), "讀不到窗口狀態就不能放行：{:?}", out.as_ref().map(|o| &o.delivery));
        assert_eq!(turn_count(&app, &f.conv).await, 0, "連 turn 都沒建");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text" || *m == "agent.prompt").count(), 0, "一個字都沒送");

        // 讀取恢復、窗口還握著：現在是「真的有窗口」的 409，跟上面「讀不到」分得開。
        crate::supervisor::maintenance::fault::restore_lease_reads(&app.db).await;
        let Err(LcError::Conflict(body)) = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unreadable").await else {
            panic!("窗口還握著，要是 409")
        };
        assert_eq!(body["reason"], "maintenance_window", "{body}");

        // 窗口收了：同一個 request id 原樣重送就送得進去。
        let l = crate::supervisor::store::lease(&app.db, "restart").await.unwrap().unwrap();
        assert!(crate::supervisor::store::release_lease(&app.db, "restart", "k8bw2f", l.fence).await.unwrap());
        prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unreadable").await.expect("恢復＋窗口收了：照常送");
    }

    /// 同一條 fence 的第二道：第一次讀時窗口不在，turn commit 之後複查的那一刻才讀壞。這一筆已經 commit、
    /// 一個字都還沒打，所以要被撤回（不是留成 `in_flight`＋`pending`）再回錯誤——跟窗口在複查時
    /// 剛好被拿走是同一條路（`a_window_that_opens_while_the_turn_commits_takes_the_turn_back_out`）。
    #[tokio::test]
    async fn a_lease_that_becomes_unreadable_while_the_turn_commits_takes_the_turn_back_out() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("INSERT INTO supervisor_leases (resource, owner, fence, released_at) VALUES ('restart','k8bw2f',7,?)")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        // turn 一 commit，那一列就變成解不開的樣子（第一次讀已經過了）。
        sqlx::query(
            "CREATE TRIGGER the_lease_goes_bad_mid_commit AFTER INSERT ON turns
             BEGIN
               UPDATE supervisor_leases SET fence='corrupt' WHERE resource='restart';
             END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-lease-goes-bad").await;

        assert!(out.is_err(), "複查讀不到就不能放行：{:?}", out.as_ref().map(|o| &o.delivery));
        assert_eq!(turn_count(&app, &f.conv).await, 0, "已經 commit 的那一筆要撤回，不是留成 in_flight+pending");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text" || *m == "agent.prompt").count(), 0, "一個字都沒送");
    }

    /// #158 驗收一：附件綁不上（一個字都沒送），收成 failed 那一句又寫不進去——以前 helper 只記一行 error，照樣回普通的
    /// `delivery=failed`，DB 卻停在 in_flight＋pending。現在記成欠著、回 503（`sent:false`）；DB 好了同一個 request id
    /// 拿到 failed＋說明，一個字都沒送。
    #[tokio::test]
    async fn an_attachment_bind_failure_whose_close_cannot_be_written_is_not_a_plain_failed() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let attachment_id = attachment(&app, &f.bot_id).await;
        sqlx::query("CREATE TRIGGER fail_prompt_attachment_bind BEFORE UPDATE OF message_id ON attachments BEGIN SELECT RAISE(ABORT, 'bind failed'); END")
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN NEW.status = 'failed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let res = prompt_with(&app, &f.bot_id, "look", "prompt-attach-lost", std::slice::from_ref(&attachment_id)).await;
        let Err(LcError::Uncommitted(body)) = res else { panic!("收尾沒寫成，卻回了 {:?}", res.map(|o| o.delivery)) };
        assert_eq!((body["error"].as_str(), body["delivery"].as_str(), &body["sent"]), (Some("delivery_state_uncommitted"), Some("failed"), &json!(false)), "{body}");
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"), "寫不進去就是還沒收");

        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        let again = prompt_with(&app, &f.bot_id, "look", "prompt-attach-lost", &[attachment_id]).await.expect("DB 好了");
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (turn_id.as_str(), "failed"));
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&turn_id).fetch_all(&app.db).await.unwrap();
        assert!(notes.len() == 1 && notes[0].contains("attachment binding failed"), "{notes:?}");
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none(), "不擋住下一則");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt" || m == "pane.send_text"), "一個字都沒送");
    }

    /// #158（同一類）：打字前才發現送不了（`runs.pane_typed` 寫不進去 → `NotAttempted`），撤回那一句寫不進去、改收成 failed
    /// 那一句也寫不進去——以前第二句被 `let _ =` 吞掉，那一筆停在 in_flight＋pending、沒人會收（擋住之後每一則）。
    /// 現在記成欠著、回 503（`sent:false`）；DB 好了同一個 request id 拿到 failed＋說明，一個字都沒打。
    #[tokio::test]
    async fn an_unsent_turn_that_can_neither_be_withdrawn_nor_failed_is_owed_not_left_in_flight() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), ..Default::default() });
        for ddl in [
            "CREATE TRIGGER pane_typed_unwritable BEFORE UPDATE OF pane_typed ON runs BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
            "CREATE TRIGGER lost_retract BEFORE DELETE ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN NEW.status = 'failed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
        ] {
            sqlx::query(ddl).execute(&app.db).await.unwrap();
        }

        let res = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unretractable").await;
        let Err(LcError::Uncommitted(body)) = res else { panic!("撤不回來也收不成，卻回了 {:?}", res.map(|o| o.delivery)) };
        assert_eq!((body["delivery"].as_str(), &body["sent"]), (Some("failed"), &json!(false)), "{body}");
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"));

        sqlx::query("DROP TRIGGER lost_retract").execute(&app.db).await.unwrap();
        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        let again = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unretractable").await.expect("DB 好了");
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (turn_id.as_str(), "failed"));
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "不留永久 in_flight");
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&turn_id).fetch_all(&app.db).await.unwrap();
        assert_eq!(notes, vec![UNRETRACTED_NOTE.to_string()]);
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");
    }

    /// Binding can fail after the turn commits; the UI must still get a terminal turn event.
    #[tokio::test]
    async fn an_attachment_bind_failure_closes_the_pending_turn() {
        let success = fixture("codex", "test").await;
        let success_app = success.env.app.clone();
        let success_attachment = attachment(&success_app, &success.bot_id).await;
        let mut success_events = success_app.subscribe();
        let success_out = prompt_with(&success_app, &success.bot_id, "look", "prompt-attachments-ok", &[success_attachment])
            .await
            .unwrap();
        let success_event = tokio::time::timeout(std::time::Duration::from_secs(1), success_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(success_event.kind, "message_added");
        assert_eq!(success_event.data["message"]["id"], success_out.message_id);
        assert!(!success_event.data["message"]["attachments_json"].is_null());

        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let attachment_id = attachment(&app, &f.bot_id).await;
        sqlx::query(
            "CREATE TRIGGER fail_prompt_attachment_bind
             BEFORE UPDATE OF message_id ON attachments
             BEGIN SELECT RAISE(ABORT, 'bind failed'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        let mut ws_events = app.subscribe();
        let mut turn_events = app.subscribe_turns();

        let out = prompt_with(&app, &f.bot_id, "look", "prompt-attachments", &[attachment_id]).await.unwrap();
        assert_eq!(out.delivery, "failed");
        let user_event = tokio::time::timeout(std::time::Duration::from_secs(1), ws_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user_event.kind, "message_added");
        assert_eq!(user_event.data["message"]["id"], out.message_id);
        assert_eq!(user_event.data["message"]["role"], "user");
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
        let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("failed", "failed"));
        let system: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(system.contains("attachment binding failed"));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), turn_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.turn_id, out.turn_id);
        assert_eq!((event.status.as_str(), event.delivery.as_str()), ("failed", "failed"));
    }

    /// 2.1.278 首次啟動的「Auto mode」框（2026-09-22）：送 Down＋Enter 選 keep bypass；mock 的畫面不會因為按鍵改變，
    /// 所以這裡驗的是「按了對的鍵、還在就 409 dialog_open、沒把 prompt 打進框裡」。
    #[tokio::test]
    async fn the_auto_mode_offer_is_answered_with_keep_bypass_and_still_open_is_a_409() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::AUTO_MODE);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        match err {
            LcError::Conflict(v) => assert_eq!(v["reason"], "dialog_open", "{v}"),
            other => panic!("{other:?}"),
        }
        let keys: Vec<String> = f.env.herdr.calls_to("pane.send_keys").iter().filter_map(|p| p.get("keys").and_then(|k| serde_json::to_string(k).ok())).collect();
        assert_eq!(keys, vec![r#"["Down","Enter"]"#.to_string()], "先替它選第二項 keep bypass");
    }

    #[tokio::test]
    async fn an_unreadable_auto_mode_recheck_refuses_to_continue() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::AUTO_MODE);
        let fail = f.env.herdr.fail_later();
        super::super::race_point::arm("claude_auto_mode_before_answer", &f.bot_id, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        match err {
            LcError::Conflict(body) => assert_eq!(body["reason"], "pane_unreadable", "{body}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "unknown screen means no Auto answer");
    }

    #[tokio::test]
    async fn codex_model_migration_is_blocked_without_choosing_or_typing() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", include_str!("fixtures/codex-0.157-model-migration.txt"));
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "dialog_open", "{body}");
                assert!(body["message"].as_str().unwrap().contains("不替你選"), "{body}");
            }
            other => panic!("{other:?}"),
        }
        let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&f.run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "blocked", "herdr 判 idle 時也要顯示 blocked");
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty());
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }

    /// 啟動時的更新選單：預設選的是 `› 1. Update now`，畫面底下是 `Press enter to continue`。daemon 把 prompt 打進去、
    /// 最後那個 Enter 就等於替使用者按下「現在更新」（在 pane 裡跑 `npm install -g`），而且 prompt 本身被選單吃掉
    /// （2026-10-02 實際發生：Update available 吃掉 prompt）。要像其他 codex 對話框一樣擋下、不打字不按鍵、告訴使用者。
    #[tokio::test]
    async fn codex_update_menu_is_never_answered_by_typing_a_prompt_into_it() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", include_str!("fixtures/codex-0.155-update-menu.txt"));
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "dialog_open", "{body}");
                let msg = body["message"].as_str().unwrap();
                assert!(msg.contains("更新") && msg.contains("Skip"), "要講清楚是更新選單、選 Skip：{msg}");
            }
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "不按任何鍵（Enter 會選到 Update now）");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty(), "不打字進選單");
        let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&f.run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "blocked", "herdr 判 idle 時也要顯示 blocked，網頁才看得到要去處理");
    }

    #[tokio::test]
    async fn a_scrolled_codex_update_menu_is_never_answered_by_a_prompt() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let menu = include_str!("fixtures/codex-0.155-update-menu.txt");
        let partial = menu.lines().skip_while(|line| !line.trim_start().starts_with("› 1. Update now")).collect::<Vec<_>>().join("\n");
        f.env.herdr.set_screen("pane-prompt-test", &partial);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(body) => assert_eq!(body["reason"], "dialog_open", "{body}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "更新選單上絕不按鍵");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty(), "更新選單上絕不送 prompt");
    }

    /// 非互動的更新方框（底下就是正常的輸入列）與對話裡引用選單原文，都不是擋路的對話框。
    #[tokio::test]
    async fn codex_update_banner_or_a_quoted_menu_does_not_block_delivery() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        let boxed = "╭──────╮\n│ >_ OpenAI Codex (v0.154.0) │\n╰──────╯\n\n  ✨ Update available! 0.154.0 -> 0.155.1\n  Run sh -c 'curl x/install.sh | sh' to update.\n\n› Explain this codebase\n";
        let quoted = format!("• 上次畫面長這樣：\n{}\n\n› ", include_str!("fixtures/codex-0.155-update-menu.txt").lines().map(|l| format!("  > {l}")).collect::<Vec<_>>().join("\n"));
        for screen in [boxed.to_string(), quoted] {
            f.env.herdr.set_screen("pane-prompt-test", &screen);
            pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_or_else(|e| panic!("不該擋：{e:?}\n{screen}"));
        }
    }

    #[tokio::test]
    async fn codex_rate_limit_suggestion_waits_for_a_user_choice() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", include_str!("fixtures/codex-0.157-rate-limit-switch.txt"));
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "dialog_open", "{body}");
                assert!(body["message"].as_str().unwrap().contains("切換模型"), "{body}");
            }
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty());
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }

    #[tokio::test]
    async fn a_closed_auto_mode_offer_does_not_get_a_stale_down_or_enter() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::AUTO_MODE);
        let screens = f.env.herdr.screens.clone();
        super::super::race_point::arm("claude_auto_mode_before_answer", &f.bot_id, move || async move {
            screens.lock().unwrap().insert("pane-prompt-test".into(), "Claude Code\n❯\n".into());
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap();
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "畫面已變，不能送 Down 或 Enter");
    }

    /// Trust setup can take time (especially on a remote host); if the prompt vanishes during it,
    /// the stale `y` must not hit the next screen.
    #[tokio::test]
    async fn a_grok_trust_prompt_that_closes_before_the_answer_gets_no_y() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home");
        std::fs::create_dir_all(&grok_home).unwrap();
        let cwd = f.env.repo.to_string_lossy().into_owned();
        sqlx::query("UPDATE bots SET cwd=?, env_json=? WHERE id=?")
            .bind(&cwd)
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let screens = f.env.herdr.screens.clone();
        super::super::race_point::arm("grok_trust_before_answer", &f.bot_id, move || async move {
            screens.lock().unwrap().insert("pane-prompt-test".into(), "Grok Build 1.0.34\n❯\n".into());
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap();
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "畫面已變，不能送 y");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }

    /// The screen can change from a trust dialog to printed lookalike text while host trust is prepared.
    /// A second read without the live `y` / `n` controls must never authorize the key.
    #[tokio::test]
    async fn a_grok_trust_lookalike_on_the_second_read_gets_no_y() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home-lookalike");
        std::fs::create_dir_all(&grok_home).unwrap();
        let cwd = f.env.repo.to_string_lossy().into_owned();
        sqlx::query("UPDATE bots SET cwd=?, env_json=? WHERE id=?")
            .bind(&cwd)
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let screens = f.env.herdr.screens.clone();
        super::super::race_point::arm("grok_trust_before_answer", &f.bot_id, move || async move {
            screens.lock().unwrap().insert(
                "pane-prompt-test".into(),
                "⏺ Printed example:\nDo you trust the contents of this directory?\nYes, proceed\nNo, quit\nGrok Build 1.0.46\n".into(),
            );
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        let _ = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await;
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "printed text is not live confirmation UI");
    }

    #[tokio::test]
    async fn an_unreadable_screen_after_grok_trust_answer_stops_prompt_delivery() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home-after-y");
        std::fs::create_dir_all(&grok_home).unwrap();
        let cwd = f.env.repo.to_string_lossy().into_owned();
        sqlx::query("UPDATE bots SET cwd=?, env_json=? WHERE id=?")
            .bind(&cwd)
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let fail = f.env.herdr.fail_later();
        super::super::race_point::arm("grok_trust_after_answer", &f.bot_id, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        match err {
            LcError::Conflict(body) => assert_eq!(body["reason"], "pane_unreadable", "{body}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            f.env.herdr.calls_to("pane.send_keys").iter().map(|p| p["keys"].clone()).collect::<Vec<_>>(),
            vec![json!(["y"])],
            "after the trust answer the unknown screen blocks further prompt effects"
        );
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }

    /// #243 重開：信任框要寫到**哪台**的信任紀錄，取決於 bot 在哪台主機。讀不到主機不能當成本機——
    /// 遠端 grok 會被寫進 daemon 這台的 `~/.grok`，再替它按 `y`。讀不到就一個鍵都不按、不寫信任，
    /// 回 409 讓排隊的 prompt 重試；讀得回來之後同一個 prompt 照常寫信任、按 `y`。
    #[tokio::test]
    async fn an_unreadable_host_leaves_the_grok_trust_dialog_alone_until_it_is_readable() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home");
        std::fs::create_dir_all(&grok_home).unwrap();
        let trust_file = grok_home.join("trusted_folders.toml");
        let cwd = f.env.repo.to_string_lossy().into_owned();
        sqlx::query("UPDATE bots SET cwd=?, env_json=? WHERE id=?")
            .bind(&cwd)
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        // pane 的 client 已經照 run 拿到了，之後才讀不到主機——正是現場的窗口。
        let app2 = app.clone();
        super::super::race_point::arm("grok_trust_before_host", &f.bot_id, move || async move {
            crate::testing::make_table_unreadable(&app2, "projects").await;
        });
        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        crate::testing::make_table_readable(&app, "projects").await;
        match err {
            LcError::Conflict(v) => assert_eq!(v["reason"], "dialog_open", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "主機讀不到：不能按 y");
        assert!(!trust_file.exists(), "主機讀不到：不能把信任寫到 daemon 這台");

        // 讀得回來：同一個 prompt 再試一次就照常信任並按 y（mock 畫面不會消失，所以仍是 409）。
        assert!(pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.is_err());
        let keys: Vec<serde_json::Value> = f.env.herdr.calls_to("pane.send_keys").iter().filter_map(|p| p.get("keys").cloned()).collect();
        assert_eq!(keys, vec![json!(["y"])]);
        assert!(trust_file.exists(), "讀得到之後才寫信任");
    }

    /// 同一條的 cwd 那一半：bot 沒有自己的 cwd 時信任的是專案目錄。專案**讀不到**不等於「沒有目錄」——
    /// 後者只能按 y，前者要等讀得到再來（不然信任沒寫、y 卻按了）。用 BLOB 塞進 `path` 讓只有這一列解不開，
    /// 主機（同一張表的 `host`）照樣讀得到。
    #[tokio::test]
    async fn an_unreadable_project_dir_leaves_the_grok_trust_dialog_alone() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home");
        std::fs::create_dir_all(&grok_home).unwrap();
        let trust_file = grok_home.join("trusted_folders.toml");
        sqlx::query("UPDATE bots SET cwd=NULL, env_json=? WHERE id=?")
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        let path: String = sqlx::query_scalar("SELECT path FROM projects WHERE id=?").bind(&f.env.project_id).fetch_one(&app.db).await.unwrap();
        sqlx::query("UPDATE projects SET path=X'FF00' WHERE id=?").bind(&f.env.project_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        assert!(db::project(&app.db, &f.env.project_id).await.is_err(), "前提：專案那一列解不開");

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(v) => assert_eq!(v["reason"], "dialog_open", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "目錄讀不到：不能按 y");

        sqlx::query("UPDATE projects SET path=? WHERE id=?").bind(&path).bind(&f.env.project_id).execute(&app.db).await.unwrap();
        assert!(pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.is_err());
        let keys: Vec<serde_json::Value> = f.env.herdr.calls_to("pane.send_keys").iter().filter_map(|p| p.get("keys").cloned()).collect();
        assert_eq!(keys, vec![json!(["y"])]);
        assert!(trust_file.exists(), "讀得到之後信任的是專案目錄");
    }

    /// 2.1.281 的防誤刪框（herdr 判成 idle 也一樣）：409 `dangerous_rm_pending` 帶目標，一個鍵都不按、一個字都不打；
    /// 通知寫在對話裡，同一個框被擋幾次都只寫一次（排隊的 flush 會一再撞這道閘門）。
    #[tokio::test]
    async fn a_dangerous_rm_prompt_is_never_typed_into_or_answered() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::DANGEROUS_RM);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        for _ in 0..2 {
            match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
                LcError::Conflict(v) => {
                    assert_eq!(v["reason"], "dangerous_rm_pending", "{v}");
                    assert_eq!(v["target"], "command substitution output", "{v}");
                }
                other => panic!("{other:?}"),
            }
        }
        for m in ["pane.send_keys", "pane.send_text", "agent.prompt"] {
            assert!(f.env.herdr.calls_to(m).is_empty(), "{m}");
        }
        let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='system' AND content LIKE '%防誤刪確認框等你本人核准%'")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(notices, 1);
    }

    /// onboarding 的主題頁跟登入選單同一類：交給人（needs_login），一個鍵都不按。
    #[tokio::test]
    async fn the_onboarding_theme_page_is_needs_login_and_nothing_is_pressed() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::ONBOARDING_THEME);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(v) => assert_eq!(v["reason"], "needs_login", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_pane_guard_does_not_authorize_a_prompt() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::DANGEROUS_RM);
        f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);
        f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)), "unreadable guard must be retryable: {err:?}");
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty());
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }

    #[tokio::test]
    async fn agent_prompt_pane_guard_rechecks_after_preflight() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Claude Code\n❯\n");
        let screens = f.env.herdr.screens.clone();
        super::super::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            screens.lock().unwrap().insert(
                "pane-prompt-test".into(),
                "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch to Claude Opus 5.5\n  2. No, go back\n".into(),
            );
        });

        let err = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-dialog-race").await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)), "dialog race must be retryable: {err:?}");
        assert_eq!(turn_count(&app, &f.conv).await, 0, "an unsent direct turn is withdrawn");
        assert!(f.env.herdr.calls_to("agent.prompt").is_empty(), "the final fence must precede agent.prompt");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }

    /// #782：agent.prompt 那條路也會把字打進 pane。帳號安全提醒橫幅在前置檢查之後才出現：最後一道圍籬擋下、可重試，
    /// 一個鍵都不按（不按 Esc），對話留一則通知。
    #[tokio::test]
    async fn agent_prompt_is_fenced_by_a_codex_security_banner() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "• PONG\n\n› \n\n  gpt-6.1-sol default · /tmp/x\n");
        let screens = f.env.herdr.screens.clone();
        super::super::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            screens.lock().unwrap().insert(
                "pane-prompt-test".into(),
                include_str!("fixtures/codex-0.159.3-security-setup-banner.txt").into(),
            );
        });

        let result = prompt(&app, &f.bot_id, "1. 先看 issue 782", "codex-banner-fence").await;
        let Err(LcError::Conflict(body)) = result else { panic!("banner must be a retryable 409: {result:?}") };
        assert_eq!(body["reason"], "codex_security_banner", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert!(f.env.herdr.calls_to("agent.prompt").is_empty());
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty() && f.env.herdr.calls_to("pane.send_keys").is_empty());
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&f.conv)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert!(notes.iter().any(|n| n.contains("安全提醒橫幅")), "{notes:?}");
    }

    #[tokio::test]
    async fn agent_prompt_is_refused_when_host_generation_changes_after_client_capture() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let conn = app.hosts.get(crate::config::LOCAL_HOST).await.unwrap();
        let calls = f.env.herdr.calls.clone();
        let guard_reads_before = Arc::new(std::sync::Mutex::new(None));
        let reads_before = guard_reads_before.clone();
        super::super::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            let counts = {
                let calls = calls.lock().unwrap();
                (
                    calls
                        .iter()
                        .filter(|(method, _)| method == "agent.get")
                        .count(),
                    calls
                        .iter()
                        .filter(|(method, _)| method == "pane.read")
                        .count(),
                )
            };
            *reads_before.lock().unwrap() = Some(counts);
            conn.bump_generation_for_test();
        });

        let result = prompt(
            &app,
            &f.bot_id,
            "這句不能送到失效世代",
            "stale-host-generation",
        )
        .await;
        let Err(LcError::Conflict(body)) = result else {
            panic!("a prompt bound to a superseded host generation must be retryable: {result:?}");
        };
        assert_eq!(body["reason"], "host_changed", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(
            turn_count(&app, &f.conv).await,
            0,
            "an unsent direct turn is withdrawn"
        );
        assert!(
            f.env.herdr.calls_to("agent.prompt").is_empty(),
            "the stale client must not receive the prompt"
        );
        let (gets_before, reads_before) = guard_reads_before
            .lock()
            .unwrap()
            .expect("race point captured baseline");
        let calls = f.env.herdr.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "agent.get")
                .count(),
            gets_before,
            "a stale fence blocks final-guard RPCs"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "pane.read")
                .count(),
            reads_before,
            "a stale fence blocks final-guard RPCs"
        );
    }

    /// A same-name remote repoint must leave the captured RPC bound to A, but reject it before
    /// either A or B sees a request. Once reconcile publishes a run on the new authority, retry works.
    #[tokio::test]
    async fn a_repointed_remote_host_receives_no_prompt_from_the_superseded_generation() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let host = format!("prompt-repoint-{}", db::ulid());
        let host_cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
        };
        let socket_a = f.env.dir.join("herdr-a.sock");
        let socket_b = f.env.dir.join("herdr-b.sock");
        let herdr_a = tt::MockHerdr::start(socket_a.clone());
        let herdr_b = tt::MockHerdr::start(socket_b.clone());
        for herdr in [&herdr_a, &herdr_b] {
            herdr.set_agent("prompt-test", "pane-prompt-test", true);
            herdr.set_screen("pane-prompt-test", "Claude Code\n❯\n");
        }
        let client_b = crate::herdr::HerdrClient::new(socket_b);
        app.hosts
            .insert_remote_with_client_for_test(
                host_cfg("target-a"),
                crate::herdr::HerdrClient::new(socket_a),
            )
            .await;
        sqlx::query("UPDATE projects SET host=? WHERE id=?")
            .bind(&host)
            .bind(&f.env.project_id)
            .execute(&app.db)
            .await
            .unwrap();

        let calls_at_repoint = Arc::new(std::sync::Mutex::new(None));
        let captured_calls = calls_at_repoint.clone();
        let app_to_repoint = app.clone();
        let replacement = host_cfg("target-b");
        let run_id = f.run_id.clone();
        let a_calls = herdr_a.calls.clone();
        let b_calls = herdr_b.calls.clone();
        super::super::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            assert!(
                db::in_flight_turn(&app_to_repoint.db, &run_id).await.unwrap().is_some(),
                "the direct turn already exists"
            );
            *captured_calls.lock().unwrap() = Some((a_calls.lock().unwrap().len(), b_calls.lock().unwrap().len()));
            app_to_repoint
                .hosts
                .replace_remote_with_client_for_test(&app_to_repoint, replacement, client_b)
                .await;
        });

        let result = prompt(&app, &f.bot_id, "同名主機改指後不能沿用舊 client", "remote-host-repoint").await;
        let Err(LcError::Conflict(body)) = result else {
            panic!("a repointed host generation must refuse delivery: {result:?}");
        };
        assert_eq!(body["reason"], "host_changed", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(turn_count(&app, &f.conv).await, 0, "the unsent direct turn is retracted");
        let (a_before, b_before) = calls_at_repoint
            .lock()
            .unwrap()
            .expect("captured the A/B baseline");
        assert_eq!(herdr_a.methods().len(), a_before, "the superseded operation sends no more RPCs to A");
        assert_eq!(herdr_b.methods().len(), b_before, "the superseded operation never switches over to B");
        assert!(herdr_a.calls_to("agent.prompt").is_empty());
        assert!(herdr_b.calls_to("agent.prompt").is_empty());

        // Model reconcile publishing a fresh active run against the now-authoritative B connection.
        assert_eq!(mark_run_exited(&app, &f.run_id, "host repointed").await, RunExit::Recorded);
        let new_run = db::ulid();
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at) VALUES (?, ?, 'running', 'idle', 'test', ?)")
            .bind(&new_run)
            .bind(&f.bot_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let retry = prompt(&app, &f.bot_id, "同名主機改指後不能沿用舊 client", "remote-host-repoint").await.unwrap();
        // The test mock deliberately does not implement agent.prompt, so a received call is
        // recorded as unknown. What matters here is that the explicit retry reaches only B.
        assert_eq!(retry.delivery, "unknown", "the retry reaches B after the new run exists");
        assert!(herdr_a.calls_to("agent.prompt").is_empty(), "the old physical host remains untouched");
        assert_eq!(herdr_b.calls_to("agent.prompt").len(), 1, "only the newly authoritative host gets the retry");
    }

    #[tokio::test]
    async fn agent_prompt_rechecks_host_generation_after_final_guard() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let conn = app.hosts.get(crate::config::LOCAL_HOST).await.unwrap();
        super::super::race_point::arm("agent_prompt_after_guard", &f.bot_id, move || async move {
            conn.bump_generation_for_test();
        });

        let result = prompt(
            &app,
            &f.bot_id,
            "這句不能送到失效世代",
            "stale-host-after-guard",
        )
        .await;
        let Err(LcError::Conflict(body)) = result else {
            panic!("a generation superseded during the final reads must be retryable: {result:?}");
        };
        assert_eq!(body["reason"], "host_changed", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(
            turn_count(&app, &f.conv).await,
            0,
            "an unsent direct turn is withdrawn"
        );
        assert!(
            f.env.herdr.calls_to("agent.prompt").is_empty(),
            "the stale client must not receive the prompt"
        );
    }
}

#[cfg(test)]
mod send_now_tests {
    //! 插隊送出（issue #103）：daemon 這一側的記帳。端到端（pane 真的跑 2.1.275、CLI 真的收到）要等
    //! §6.9 批次升級之後才能驗，這裡釘的是「哪些 run 可以插隊」與「被打斷的回合怎麼收」。
    use super::*;
    use crate::testing as tt;

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
    }

    /// 一顆會回話的 claude pane：`status_json` 帶 statusLine 回報的版本，transcript 檔就是送達證據。
    async fn fixture(kind: &str, version: Option<&str>) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'send-now-test',?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(kind)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        let transcript = if kind == "claude" {
            // Claude transcript evidence is trusted only inside this bot's configured project root.
            let config_dir = env.dir.join("claude-config");
            sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
                .bind(serde_json::json!({ "CLAUDE_CONFIG_DIR": config_dir.to_string_lossy() }).to_string())
                .bind(&bot_id)
                .execute(&app.db)
                .await
                .unwrap();
            let transcript_dir = config_dir.join("projects/-t");
            std::fs::create_dir_all(&transcript_dir).unwrap();
            transcript_dir.join(format!("session-{}.jsonl", db::ulid()))
        } else {
            env.dir.join(format!("session-{}.jsonl", db::ulid()))
        };
        std::fs::write(&transcript, "").unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, native_session_id, transcript_path, status_json, started_at)
             VALUES (?,?,'running','working','pane-sn','test','sess-1',?,?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(transcript.to_str().unwrap())
        .bind(version.map(|v| format!(r#"{{"version":"{v} (Claude Code)","model_name":"Opus"}}"#)))
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        env.herdr.live_pane(
            "pane-sn",
            tt::LivePane { width: Some(120), transcript_file: Some(transcript), ..Default::default() },
        );
        Fixture { env, bot_id, conv, run_id }
    }

    /// 佔住一個回合，回傳那筆 turn 的 id。
    async fn busy(f: &Fixture) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&id)
        .bind(&f.conv)
        .bind(&f.run_id)
        .bind(db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
        id
    }

    async fn status_of(f: &Fixture, turn_id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
            .bind(turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap()
    }

    async fn in_flight_count(f: &Fixture) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status='in_flight'")
            .bind(&f.conv)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap()
    }

    fn keys_sent(f: &Fixture) -> Vec<String> {
        f.env
            .herdr
            .calls_to("pane.send_keys")
            .iter()
            .filter_map(|p| p.get("keys").and_then(|k| serde_json::to_string(k).ok()))
            .collect()
    }

    /// 驗收一：bot 正在跑時插隊送出——那句話進了對話，鍵真的按下去了，被打斷的回合收成 `failed`
    /// 並留下一則說明，不是卡在進行中。
    #[tokio::test]
    async fn a_send_now_closes_the_turn_it_interrupted_and_delivers_the_new_one() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let interrupted = busy(&f).await;

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-1", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"));
        assert_eq!(out.delivery, "ok", "打字進 pane 並由 transcript 證明送出");

        assert_eq!(status_of(&f, &interrupted).await, "failed", "被打斷的回合不能留在 in_flight");
        assert_eq!(status_of(&f, &out.turn_id).await, "in_flight", "新的那一筆才是在飛的");
        assert_eq!(in_flight_count(&f).await, 1);
        let note: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system' AND content=?")
            .bind(&interrupted)
            .bind(SEND_NOW_NOTE)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(note, 1, "對話裡看得到它是被插隊打斷的");
        assert_eq!(keys_sent(&f), vec![r#"["ctrl+x","ctrl+s"]"#.to_string()], "按的是 send-now 鍵，不是 Enter");
        assert_eq!(sent_via(&f, &out.message_id).await.as_deref(), Some("send_now"), "泡泡標「插隊」（使用者 2026-09-28）");
    }

    async fn sent_via(f: &Fixture, message_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT sent_via FROM messages WHERE id=? AND role='user'").bind(message_id).fetch_one(&f.env.app.db).await.unwrap()
    }

    /// issue #748：一顆 codex 的 run。`version` 是巡邏從畫面讀到的跑著的版本，`flag` 是 `[codex] instant_interrupt`。
    /// 沒有 native session：送達證據走一列回音，不去翻真的 ~/.codex。
    async fn codex_fixture(version: Option<&str>, flag: bool) -> Fixture {
        let f = fixture("codex", None).await;
        sqlx::query("UPDATE runs SET native_session_id = NULL, transcript_path = NULL WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
        f.env.herdr.live_pane("pane-sn", tt::LivePane { width: Some(120), codex: true, ..Default::default() });
        if let Some(v) = version {
            crate::codex_update::remember_running(&f.run_id, &format!("│ >_ OpenAI Codex (v{v})   │"), None);
        }
        f.env.app.cfg.update(|c| { c.codex.instant_interrupt = flag; Ok(()) }).await.unwrap();
        f
    }

    async fn turn_count(f: &Fixture) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&f.conv).fetch_one(&f.env.app.db).await.unwrap()
    }

    fn typed(f: &Fixture) -> usize {
        f.env.herdr.calls_to("pane.send_text").len()
    }

    /// 旗標關（預設）：codex 的 send_now 跟以前一模一樣——409 說「只有 claude 有」，一個字都不打。
    #[tokio::test]
    async fn codex_send_now_is_refused_as_before_while_the_canary_is_off() {
        let f = codex_fixture(Some("0.159.0"), false).await;
        let running = busy(&f).await;
        let err = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-off", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["send_now_refused"], "send_now_unsupported_kind", "{body}");
        assert_eq!(typed(&f), 0, "沒插隊就一個字都不打");
        assert_eq!(status_of(&f, &running).await, "in_flight");
    }

    /// 旗標開、版本夠：字打進忙碌的 pane，併進同一個回合——舊回合不收、不建新回合，泡泡掛在被 steer 的回合上，
    /// 回應是 `steered` 而不是 claude 式的 `interrupted`，按的是一般 Enter 不是 send-now 鍵。
    #[tokio::test]
    async fn a_codex_steer_joins_the_running_turn_without_interrupting_it() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        let running = busy(&f).await;
        let out = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-steer", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("steered"));
        assert_eq!(out.turn_id, running, "回的是被 steer 的那一回合");
        assert!(matches!(out.delivery.as_str(), "ok" | "unverified"), "{}", out.delivery);
        assert_eq!(status_of(&f, &running).await, "in_flight", "舊回合不收");
        assert_eq!(turn_count(&f).await, 1, "不建新回合");
        assert_eq!(in_flight_count(&f).await, 1);
        assert_eq!(sent_via(&f, &out.message_id).await.as_deref(), Some("supplement"));
        let msg_turn: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id=?").bind(&out.message_id).fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(msg_turn.as_deref(), Some(running.as_str()));
        assert!(notes_on(&f, &running).await.is_empty(), "不留「被插隊打斷」的說明");
        assert_eq!(typed(&f), 1);
        assert_eq!(keys_sent(&f), vec![r#"["Enter"]"#.to_string()], "一般 Enter，沒有 send-now 鍵");
    }

    /// 同一個 request id 重送（逾時重試）：字已經在 pane 裡，不能再打一次，也不能多一則訊息。
    #[tokio::test]
    async fn retrying_a_codex_steer_does_not_type_it_twice() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        busy(&f).await;
        let first = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-retry", &[], None).await.unwrap();
        let again = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-retry", &[], None).await.unwrap();
        assert_eq!((again.message_id.as_str(), again.send_now), (first.message_id.as_str(), Some("steered")));
        assert_eq!(typed(&f), 1, "只打一次");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='user' AND sent_via='supplement'").fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(n, 1);
    }

    /// 字打進去了但訊息寫不進 DB：回 503（sent:true），重送只補記訊息，不重打。
    #[tokio::test]
    async fn a_codex_steer_whose_message_could_not_be_saved_is_not_typed_again_on_retry() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        busy(&f).await;
        sqlx::query("CREATE TRIGGER no_user_msg BEFORE INSERT ON messages WHEN NEW.sent_via = 'supplement' BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END")
            .execute(&f.env.app.db).await.unwrap();
        let err = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-db", &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("{err:?}") };
        assert_eq!((body["error"].as_str(), body["sent"].as_bool()), (Some("send_now_state_uncommitted"), Some(true)), "{body}");
        sqlx::query("DROP TRIGGER no_user_msg").execute(&f.env.app.db).await.unwrap();
        let out = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-db", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("steered"));
        assert!(!out.message_id.is_empty());
        assert_eq!(typed(&f), 1, "重送只補記，不重打");
    }

    /// 版本不夠或不知道：不賭，409 帶原因，一個字都不打。
    #[tokio::test]
    async fn a_codex_steer_needs_a_known_new_enough_version() {
        for (version, want) in [(Some("0.158.9"), "send_now_codex_too_old"), (None, "send_now_codex_version_unknown")] {
            let f = codex_fixture(version, true).await;
            busy(&f).await;
            let err = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-ver", &[], None).await.unwrap_err();
            let LcError::Conflict(body) = err else { panic!("{err:?}") };
            assert_eq!(body["send_now_refused"], want, "{body}");
            assert_eq!(typed(&f), 0);
        }
    }

    /// 普通 prompt／排隊不因這個功能變成插隊：旗標開著，沒帶 send_now 的 codex prompt 照舊 409。
    #[tokio::test]
    async fn a_plain_codex_prompt_still_waits_while_the_canary_is_on() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        busy(&f).await;
        let err = prompt_relayed(&f.env.app, &f.bot_id, "一般一句", "cx-plain", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "a turn is already in flight", "{body}");
        assert_eq!(typed(&f), 0);
    }

    /// codex 閒著時按插隊＝一般送出（`idle`），建新回合，不走 steer。
    #[tokio::test]
    async fn a_codex_send_now_while_idle_is_a_plain_send() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        let out = prompt_send_now(&f.env.app, &f.bot_id, "現在做這個", "cx-idle", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("idle"));
        assert_eq!(turn_count(&f).await, 1);
        assert_ne!(sent_via(&f, &out.message_id).await.as_deref(), Some("supplement"));
    }

    /// #120：插隊送出在**按任何鍵之前**就被擋下（這裡讓 `set_pane_typed` 寫不進去，`NotAttempted`）時，
    /// 什麼都沒打斷——正在跑的回合不能被收成「被插隊打斷」的 failed。以前計畫一成立就先收掉它，
    /// claude 其實還在跑，回覆之後變成一筆外部回合，重送還會退化成一般 Enter 打進忙碌的 pane。
    #[tokio::test]
    async fn a_send_now_refused_before_any_key_leaves_the_running_turn_alone() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        sqlx::query("CREATE TRIGGER no_pane_typed BEFORE UPDATE OF pane_typed ON runs BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-x", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("按鍵前被擋是可重試的 409：{err:?}") };
        assert_eq!(body["sent"], false, "{body}");
        assert!(keys_sent(&f).is_empty(), "一個鍵都沒按");

        assert_eq!(status_of(&f, &running).await, "in_flight", "沒按鍵就沒有打斷：舊回合還在跑");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'")
            .bind(&running)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(notes, 0, "也不留「被插隊打斷」的說明");
        assert_eq!(in_flight_count(&f).await, 1);
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 1, "新的那一則沒有留下任何列，同一個 request id 可以原樣重送");
    }

    async fn transcript_of(f: &Fixture) -> String {
        let path: String =
            sqlx::query_scalar("SELECT transcript_path FROM runs WHERE id=?").bind(&f.run_id).fetch_one(&f.env.app.db).await.unwrap();
        std::fs::read_to_string(path).unwrap_or_default()
    }

    async fn delivery_of(f: &Fixture, turn_id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT delivery FROM turns WHERE id=?").bind(turn_id).fetch_one(&f.env.app.db).await.unwrap()
    }

    async fn notes_on(f: &Fixture, turn_id: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system' ORDER BY created_at")
            .bind(turn_id)
            .fetch_all(&f.env.app.db)
            .await
            .unwrap()
    }

    fn send_now_presses(f: &Fixture) -> usize {
        keys_sent(f).iter().filter(|k| k.contains("ctrl+s")).count()
    }

    /// #120 重開 regression 1：準備全過、第一個 `pane.send_text` 被 herdr **明確拒絕**（side effect 沒發生）。
    /// 一個字都沒進 pane：舊回合還在跑，不能被記成「被插隊打斷」；新的那一則撤掉，同一個 request id 可以重送。
    #[tokio::test]
    async fn a_send_now_whose_text_herdr_refused_leaves_the_running_turn_alone() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_text", tt::Fault::Refuse);

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-refused", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("herdr 拒收＝什麼都沒發生，是可重試的 409：{err:?}") };
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert!(keys_sent(&f).is_empty(), "一個鍵都沒按");

        assert_eq!(status_of(&f, &running).await, "in_flight", "字沒進去就沒有打斷：舊回合還在跑");
        assert!(notes_on(&f, &running).await.is_empty(), "不留「被插隊打斷」的說明");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 1, "新的那一則沒有留下任何列");
    }

    /// #647：長文字拆段，第 1 段已進框、第 2 段被拒。不能回「一個字都沒進去」的可重試 409。
    #[tokio::test]
    async fn a_send_now_whose_later_paste_chunk_was_refused_is_not_told_that_nothing_landed() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_after("pane.send_text", 1, tt::Fault::Refuse);
        let text = "x".repeat(1500);

        let out = prompt_send_now(&app, &f.bot_id, &text, "sn-partial", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("not_sent"), "{out:?}");
        assert_eq!(out.delivery, "failed", "{out:?}");
        assert_eq!(send_now_presses(&f), 0, "半段留在框裡就不按送出鍵");
        assert_eq!(f.env.herdr.calls_to("pane.send_text").len(), 2, "第 1 段送出、第 2 段被拒");
        let box_rows = f.env.herdr.pane("pane-sn").unwrap().composer;
        assert_eq!(box_rows, vec!["x".repeat(1000)], "前 1000 字留在框裡：{box_rows:?}");
        assert_eq!(status_of(&f, &running).await, "in_flight");
        assert_eq!(status_of(&f, &out.turn_id).await, "failed");
        assert!(notes_on(&f, &out.turn_id).await.iter().any(|n| n.contains("輸入框")));
    }

    /// 同一條，但這次 `pane.send_text` 的結果**不知道**（送出去之後連線斷了、沒回）。打字從來不會打斷 claude，
    /// 送出鍵也沒有按——舊回合照樣還在跑；新的那一則沒送出（字可能還留在框裡），說清楚、不按鍵。
    #[tokio::test]
    async fn a_send_now_whose_typing_went_unanswered_does_not_press_the_key_or_close_the_running_turn() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_text", tt::Fault::DropAfter);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-typing-unknown", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("not_sent"), "{out:?}");
        assert_eq!(out.delivery, "failed", "{out:?}");
        assert_eq!(send_now_presses(&f), 0, "不知道框裡是什麼就不按送出鍵");

        assert_eq!(status_of(&f, &running).await, "in_flight", "沒按送出鍵就沒有打斷");
        assert!(notes_on(&f, &running).await.is_empty());
        assert_eq!(status_of(&f, &out.turn_id).await, "failed");
        assert!(notes_on(&f, &out.turn_id).await.iter().any(|n| n.contains("輸入框")), "告訴人字可能還留在輸入框");
        assert_eq!(in_flight_count(&f).await, 1);
    }

    /// #120 重開 regression 2：準備做完之後、打第一個字之前，有人在終端裡打了字（不受 bot 鎖管）。
    /// 不能把兩段字接在一起送出——再看一次框、看到有字就放手。
    #[tokio::test]
    async fn someone_typing_after_the_send_now_was_prepared_never_gets_merged_into_it() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        let herdr = f.env.herdr.live.clone();
        super::super::race_point::arm("prompt_before_typing", &f.bot_id, move || async move {
            herdr.lock().unwrap().get_mut("pane-sn").unwrap().composer.push("我自己的草稿".into());
        });

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-draft", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("框裡有別人的字：不送，可重試：{err:?}") };
        assert_eq!(body["reason"], "composer_busy", "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(send_now_presses(&f), 0, "沒有按送出鍵");
        assert!(!transcript_of(&f).await.contains("先看這句"), "兩段字沒有被接在一起送出去：{}", transcript_of(&f).await);
        let box_rows = f.env.herdr.pane("pane-sn").unwrap().composer;
        assert_eq!(box_rows, vec!["我自己的草稿".to_string()], "別人的草稿原封不動，沒被接上我們的字");
        assert_eq!(status_of(&f, &running).await, "in_flight");
    }

    /// 送出鍵被 herdr **明確拒絕**：字在框裡、鍵沒按下去，claude 還在跑舊回合。不能記成被打斷。
    #[tokio::test]
    async fn a_send_now_key_that_herdr_refused_does_not_count_as_an_interruption() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::Refuse);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-key-refused", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("not_sent"), "{out:?}");
        assert_eq!(out.delivery, "failed", "{out:?}");
        assert_eq!(status_of(&f, &running).await, "in_flight", "鍵沒按下去：舊回合照常");
        assert!(notes_on(&f, &running).await.is_empty());
        assert!(!transcript_of(&f).await.contains("先看這句"), "沒送出");
        assert_eq!(in_flight_count(&f).await, 1);
    }

    /// #158（同一類）：插隊送出的鍵被 herdr 拒收（沒生效），新的那一則收成 failed 那一句卻寫不進去——以前回 502，那一則停在
    /// run_id 為空的 in_flight＋pending（佔住維護窗口的送達臨界區、同一個 request id 重問拿不到結果）直到 daemon 重啟。
    /// 現在記成欠著、回 503；DB 好了同一個 request id 拿到 failed。正在跑的那一回合從頭到尾不動。
    #[tokio::test]
    async fn a_send_now_that_fell_through_and_could_not_be_closed_is_owed() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::Refuse);
        sqlx::query("CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.run_id IS NULL BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-fell-lost", &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("收不成，卻回了 {err:?}") };
        assert_eq!((body["error"].as_str(), body["delivery"].as_str(), &body["sent"]), (Some("delivery_state_uncommitted"), Some("failed"), &json!(false)), "{body}");
        let new_turn = body["turn_id"].as_str().unwrap().to_string();
        assert_eq!(status_of(&f, &new_turn).await, "in_flight", "寫不進去就是還沒收");

        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        let again = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-fell-lost", &[], None).await.expect("DB 好了");
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (new_turn.as_str(), "failed"));
        assert_eq!(status_of(&f, &new_turn).await, "failed");
        assert!(notes_on(&f, &new_turn).await.iter().any(|n| n.contains("插隊送出沒有送出")));
        assert_eq!(status_of(&f, &running).await, "in_flight", "鍵沒生效：舊回合照常");
        assert_eq!(in_flight_count(&f).await, 1);
    }

    /// #120 重開 regression 3：送出鍵**確定被 herdr 收下**之後，後面才失敗（這裡是 TUI 吃掉了那顆鍵、字一直留在框裡）。
    /// 第一個會打斷的 side effect 已經成立：舊回合照被插隊打斷收掉，新的那一則是「送出狀態不明」。
    #[tokio::test]
    async fn a_failure_after_the_send_now_key_was_accepted_still_closes_the_interrupted_turn() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.live.lock().unwrap().get_mut("pane-sn").unwrap().swallow_enter = true;

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-after-key", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"), "{out:?}");
        assert_eq!(out.delivery, "unknown", "{out:?}");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
        assert_eq!(status_of(&f, &out.turn_id).await, "in_flight");
        assert_eq!(in_flight_count(&f).await, 1);
    }

    /// 送出鍵的 RPC 沒有回（herdr 其實按下去了）：看證據——transcript 裡有這一則，就是送出去了、舊回合被打斷。
    #[tokio::test]
    async fn an_unanswered_send_now_key_that_did_land_is_proven_by_the_transcript() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropAfter);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-key-landed", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"), "{out:?}");
        assert_eq!(out.delivery, "ok", "transcript 證明送出了：{out:?}");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
        assert_eq!(send_now_presses(&f), 1, "證據已經說送出了，不再按");
    }

    /// 送出鍵的 RPC 沒有回、字還整個在框裡（鍵沒按下去）：再按一次，這次 herdr 收下了——照常插隊送出。
    #[tokio::test]
    async fn an_unanswered_send_now_key_that_did_not_land_is_pressed_once_more() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropBefore);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-key-again", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"), "{out:?}");
        assert_eq!(out.delivery, "ok", "{out:?}");
        assert_eq!(send_now_presses(&f), 2, "第一次沒進去，再按一次");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
    }

    /// 送出鍵的 RPC 沒有回、而且真的沒按下去（兩次都是）：字一直在框裡、transcript 沒有這一則——鍵沒生效，
    /// 舊回合照常。不假定打斷了。
    #[tokio::test]
    async fn an_unanswered_send_now_key_that_never_landed_leaves_the_running_turn_alone() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropBefore);
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropBefore);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-key-lost", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("not_sent"), "{out:?}");
        assert_eq!(status_of(&f, &running).await, "in_flight", "證據說沒送出：舊回合照常");
        assert!(notes_on(&f, &running).await.is_empty());
        assert_eq!(status_of(&f, &out.turn_id).await, "failed");
        assert!(!transcript_of(&f).await.contains("先看這句"));
    }

    /// 送出鍵的結果**真的不知道**：RPC 沒回、之後連 pane 都讀不到。不能假定打斷了——舊回合留在 in_flight，
    /// 新的那一則標成「結果不明」。之後的證據（transcript 裡確實有這一則）到的時候，hook 先把「被插隊打斷」補上，
    /// 回覆才不會掛到舊回合上。
    #[tokio::test]
    async fn an_unknown_send_now_is_settled_by_the_evidence_that_arrives_later() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropAfter);
        let transcript = f.env.herdr.pane("pane-sn").and_then(|p| p.transcript_file).unwrap();
        let hidden = transcript.with_extension("hidden");
        let (herdr_live, herdr_screens) = (f.env.herdr.live.clone(), f.env.herdr.screens.clone());
        let (from, to) = (transcript.clone(), hidden.clone());
        super::super::race_point::arm("send_now_after_key", &f.bot_id, move || async move {
            // 鍵送出去之後什麼都看不到了：pane 讀不到、transcript 也暫時讀不到。
            herdr_live.lock().unwrap().remove("pane-sn");
            herdr_screens.lock().unwrap().insert("pane-sn".into(), "__READ_ERROR__".into());
            std::fs::rename(&from, &to).unwrap();
        });

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-unknown", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("unknown"), "{out:?}");
        assert_eq!(status_of(&f, &running).await, "in_flight", "結果不明＝不假定打斷");
        assert!(notes_on(&f, &running).await.is_empty());
        assert_eq!(status_of(&f, &out.turn_id).await, "failed", "新的那一則不佔 in_flight");
        assert_eq!(delivery_of(&f, &out.turn_id).await, "unknown");

        // transcript 又讀得到了，而新那一則的 user entry 確實在裡面；它的 Stop 到了：先補「被插隊打斷」，
        // 回覆才不會接到舊回合上。
        std::fs::rename(&hidden, &transcript).unwrap();
        let stop = crate::hookrecv::HookBody {
            bot_id: f.bot_id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "sess-1", "prompt_id": "p-new",
                            "last_assistant_message": "看到了"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();
        assert_eq!(status_of(&f, &running).await, "failed", "證據到了：舊回合確實被插隊打斷");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
        let replies_on_old: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&running)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(replies_on_old, 0, "新那一則的回覆不掛到被打斷的回合上");
        let answered: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages m JOIN turns t ON t.id = m.turn_id
              WHERE t.conversation_id=? AND m.role='assistant' AND m.content='看到了'",
        )
        .bind(&f.conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(answered, 1, "回覆有地方放");
    }

    /// 送出鍵確定生效，但 DB 那一半（收舊回合、把新的那一則掛上 run）寫不進去：跟 #147 同一個模型——
    /// 不回普通成功，欠著的收尾之後補上，而且不再按鍵。
    #[tokio::test]
    async fn a_send_now_whose_bookkeeping_could_not_be_written_is_settled_later_without_pressing_again() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-owed", &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("不能是普通的成功或 502：{err:?}") };
        assert_eq!(body["error"], "send_now_state_uncommitted", "{body}");
        assert_eq!(body["sent"], true, "{body}");
        assert_eq!(body["interrupted_turn_id"], running.as_str(), "{body}");
        assert_eq!(send_now_presses(&f), 1);
        assert!(transcript_of(&f).await.contains("先看這句"), "鍵確實生效了");

        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        // 同一個 request id 重送：先補欠著的那一半，回的是那一則，不重打。
        let again = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-owed", &[], None).await.unwrap();
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
        assert_eq!(status_of(&f, &again.turn_id).await, "in_flight");
        assert_eq!(in_flight_count(&f).await, 1);
        assert_eq!(send_now_presses(&f), 1, "補收尾不按鍵");
        let bound: Option<String> = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&again.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(bound.as_deref(), Some(f.run_id.as_str()), "新的那一則掛回 run 上");
    }

    /// 同一條，但 DB 只是一時寫不進去（送達證據等完時已經好了）：當場補上，回普通的成功，不必等重試。
    #[tokio::test]
    async fn a_send_now_whose_bookkeeping_failed_only_for_a_moment_is_settled_on_the_spot() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'database is locked'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let db = app.db.clone();
        super::super::race_point::arm("send_now_owed", &f.bot_id, move || async move {
            sqlx::query("DROP TRIGGER lost_close").execute(&db).await.unwrap();
        });

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-owed-briefly", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"), "{out:?}");
        assert_eq!(out.delivery, "ok", "{out:?}");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(status_of(&f, &out.turn_id).await, "in_flight");
        let bound: Option<String> = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&out.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(bound.as_deref(), Some(f.run_id.as_str()));
        assert_eq!(send_now_presses(&f), 1);
    }

    /// #149 重開的時序：插隊送出收舊回合那一半一時寫不進去（欠著），當場補上時交易 commit 了，緊接著寫新那一則的送達結果
    /// （送出的那一刻）又失敗——記成送達欠帳；回到 `prompt_inner` 再寫一次（此刻）時 DB 好了。舊帳要跟著清掉，送達時間
    /// 要是最早送出的那一刻；之後別的路寫進較新的證據，結清時不能被舊帳蓋回去。
    #[tokio::test]
    async fn a_send_now_whose_owed_delivery_write_lands_on_the_second_try_leaves_no_stale_debt() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'database is locked'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let sent_by = Arc::new(std::sync::Mutex::new(String::new()));
        let (db, bot, seen) = (app.db.clone(), f.bot_id.clone(), sent_by.clone());
        super::super::race_point::arm("send_now_owed", &f.bot_id, move || async move {
            // 收舊回合那一半好了，寫送達結果的那一句（只有它寫 `delivery_verified`）換成寫不進去。
            sqlx::query("DROP TRIGGER lost_close").execute(&db).await.unwrap();
            sqlx::query("CREATE TRIGGER lost_delivery BEFORE UPDATE OF delivery_verified ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
                .execute(&db)
                .await
                .unwrap();
            super::super::race_point::arm("owed_delivery_recorded", &bot, move || async move {
                *seen.lock().unwrap() = db::now();
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                sqlx::query("DROP TRIGGER lost_delivery").execute(&db).await.unwrap();
            });
        });

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-stale-debt", &[], None).await.expect("第二次寫成了：普通的成功");
        assert_eq!((out.send_now, out.delivery.as_str()), (Some("interrupted"), "ok"));
        let sent_by = sent_by.lock().unwrap().clone();
        assert!(!sent_by.is_empty(), "送達結果的第一次寫入確實失敗、記成欠著");
        let at: Option<String> = sqlx::query_scalar("SELECT delivered_at FROM turns WHERE id=?").bind(&out.turn_id).fetch_one(&app.db).await.unwrap();
        assert!(at.as_deref().is_some_and(|at| at <= sent_by.as_str()), "送達時間是送出的那一刻（{sent_by} 之前），拿到 {at:?}");

        sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?").bind(&out.turn_id).execute(&app.db).await.unwrap();
        super::super::owed_delivery::settle_locked(&app, &f.bot_id).await.expect("沒有欠著的");
        assert_eq!(delivery_of(&f, &out.turn_id).await, "unknown", "舊帳不會把較新的結果蓋回去");
        assert_eq!(send_now_presses(&f), 1);
    }

    /// 驗收二：連續兩次插隊送出不會產生兩個 `in_flight`——第二次同樣先收掉第一次那筆。
    #[tokio::test]
    async fn two_send_nows_in_a_row_never_leave_two_in_flight_turns() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        busy(&f).await;

        let first = prompt_send_now(&app, &f.bot_id, "第一句", "sn-1", &[], None).await.unwrap();
        let second = prompt_send_now(&app, &f.bot_id, "第二句", "sn-2", &[], None).await.unwrap();

        assert_eq!(second.send_now, Some("interrupted"));
        assert_eq!(status_of(&f, &first.turn_id).await, "failed");
        assert_eq!(status_of(&f, &second.turn_id).await, "in_flight");
        assert_eq!(in_flight_count(&f).await, 1);
    }

    /// 驗收三：舊版 claude 與其他 kind 走原本的路——照樣 409、一個鍵都不按、在飛的那筆原封不動，
    /// 只是 body 多說一句為什麼沒插隊。
    #[tokio::test]
    async fn an_old_claude_or_another_kind_keeps_the_existing_behaviour() {
        for (kind, version, code) in [
            ("claude", Some("2.1.274"), "send_now_cli_too_old"),
            ("claude", None, "send_now_version_unknown"),
            ("codex", Some("2.1.275"), "send_now_unsupported_kind"),
            ("grok", Some("2.1.275"), "send_now_unsupported_kind"),
        ] {
            let f = fixture(kind, version).await;
            let app = f.env.app.clone();
            let interrupted = busy(&f).await;

            let err = prompt_send_now(&app, &f.bot_id, "插不進去", "sn-1", &[], None).await.unwrap_err();
            let LcError::Conflict(body) = err else { panic!("{kind}/{version:?} 應該照舊回 409") };
            assert_eq!(body["reason"], "a turn is already in flight", "{kind}/{version:?}");
            assert_eq!(body["send_now_refused"], code, "{kind}/{version:?}");
            assert!(body["send_now_message"].as_str().is_some_and(|m| !m.is_empty()), "{kind}/{version:?}");

            assert_eq!(status_of(&f, &interrupted).await, "in_flight", "{kind}/{version:?}：沒插隊就不准動人家的回合");
            assert_eq!(in_flight_count(&f).await, 1, "{kind}/{version:?}");
            assert!(keys_sent(&f).is_empty(), "{kind}/{version:?}：一個鍵都不該按");
        }
    }

    /// 閒著的 bot 帶 `send_now` 送出：沒有回合可以打斷，就照一般 Enter 送，不多綁一個版本前提。
    #[tokio::test]
    async fn a_send_now_with_nothing_in_flight_is_an_ordinary_send() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();

        let out = prompt_send_now(&app, &f.bot_id, "現在有空嗎", "sn-1", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("idle"));
        assert_eq!(in_flight_count(&f).await, 1);
        assert!(!keys_sent(&f).iter().any(|k| k.contains("ctrl+s")), "沒有回合可以打斷就不按那顆鍵");
        assert_eq!(sent_via(&f, &out.message_id).await, None, "什麼都沒打斷就不標插隊");
    }
    /// #157 的前半：送出鍵生效了，收舊回合那一半寫不進去（欠著），確認送出（`confirm_submitted`）又出錯——這裡讓它第一次
    /// 讀畫面就被 herdr 拒絕。回的是 `send_now_state_uncommitted`；帳上要記著新的那一則「鍵按過、證不出來」。
    async fn a_send_now_owed_while_its_confirmation_failed(f: &Fixture, crid: &str) -> (String, String, String) {
        let app = f.env.app.clone();
        let running = busy(f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let fail = f.env.herdr.fail_later();
        super::super::race_point::arm("send_now_after_key", &f.bot_id, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", crid, &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("鍵生效、收尾沒寫成：{err:?}") };
        assert_eq!(body["error"], "send_now_state_uncommitted", "{body}");
        let sent_by = db::now();
        assert_eq!(send_now_presses(f), 1);
        assert!(transcript_of(f).await.contains("先看這句"), "鍵確實生效了");
        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        (running, body["turn_id"].as_str().unwrap().to_string(), sent_by)
    }

    /// #157：確認送出出錯時，帳上以前記的是「沒有送達結果」——補收尾把新的那一則掛上 run 之後沒東西可寫，它停在
    /// in_flight＋pending 直到重啟，同一個 request id 重問只拿得到 503（#149）。要記成 `unknown`（鍵按過、證不出來），
    /// 送達時間是送出的那一刻；補的時候絕不再按鍵、不再打字。
    #[tokio::test]
    async fn a_send_now_whose_confirmation_failed_while_owed_is_settled_as_unknown_without_resending() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let (running, new_turn, sent_by) = a_send_now_owed_while_its_confirmation_failed(&f, "sn-unconfirmed").await;

        let again = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-unconfirmed", &[], None).await.expect("補上了：同一個 request id 拿到這一則");
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (new_turn.as_str(), "unknown"), "鍵按過、證不出來");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(status_of(&f, &new_turn).await, "in_flight");
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&new_turn).fetch_one(&app.db).await.unwrap();
        assert!(t.delivered_at.as_deref().is_some_and(|at| at <= sent_by.as_str()), "送出的那一刻，不是補寫的時候：{:?}", t.delivered_at);
        assert_eq!(send_now_presses(&f), 1, "補的時候不再按鍵");
        assert_eq!(f.env.herdr.calls_to("pane.send_text").len(), 1, "也不再打字");
        assert_eq!(transcript_of(&f).await.matches("先看這句").count(), 1, "只送了一次");
    }

    /// #157 同一個洞走 hook 那條：回覆到了，hook 先把欠著的收尾補上、新的那一則掛上 run，再認領它答完——
    /// 送達要跟著收成 `ok`（有回覆就是送到了），不是一筆已經完成、送達卻永遠 pending 的回合。
    #[tokio::test]
    async fn a_send_now_whose_confirmation_failed_while_owed_is_settled_by_the_reply_hook() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let (running, new_turn, _) = a_send_now_owed_while_its_confirmation_failed(&f, "sn-unconfirmed-hook").await;

        let stop = crate::hookrecv::HookBody {
            bot_id: f.bot_id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "sess-1", "prompt_id": "p-sn", "last_assistant_message": "看到了"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();

        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(status_of(&f, &new_turn).await, "completed");
        assert_eq!(delivery_of(&f, &new_turn).await, "ok", "答完了：送達不留 pending");
        assert_eq!(send_now_presses(&f), 1);
        assert_eq!(f.env.herdr.calls_to("pane.send_text").len(), 1);
    }

    /// 送出鍵生效了（transcript 證明送到），收掉被插隊那一筆寫不進去：回 503、帳上欠著（新的那一則還沒掛上 run）。
    /// `healed`：回來之前 DB 就好了；否則收被插隊那一筆的寫入還壞著，由呼叫端之後自己拿掉。
    async fn an_owed_send_now(f: &Fixture, crid: &str, healed: bool) -> (String, String) {
        let app = f.env.app.clone();
        let running = busy(f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let err = prompt_send_now(&app, &f.bot_id, "先看這句", crid, &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("鍵生效、收尾沒寫成：{err:?}") };
        assert!(transcript_of(f).await.contains("先看這句"), "鍵確實生效了");
        if healed {
            sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        }
        (running, body["turn_id"].as_str().unwrap().to_string())
    }

    /// 插隊送出欠著收尾時 bot 被停掉：補收尾時新的那一則掛不上 run（run 已經不在），收成 failed——但送出鍵生效過、
    /// 送達結果也在帳上，不能丟掉只留 `pending`：503 叫呼叫端用同一個 request id 重問，拿到的要是送達結果（API 說不回 `pending`）。
    #[tokio::test]
    async fn a_send_now_owed_when_its_run_stops_keeps_its_delivery_result() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let (running, new_turn) = an_owed_send_now(&f, "sn-stopped", true).await;
        stop_bot(&app, &f.bot_id).await.expect("停得掉");
        assert_eq!(status_of(&f, &running).await, "failed");

        let again = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-stopped", &[], None).await.expect("補上了：同一個 request id 拿到這一則");
        assert_eq!(again.turn_id, new_turn);
        assert_eq!(status_of(&f, &new_turn).await, "failed", "run 已經不在：這一則接不上去");
        assert_eq!(again.delivery, "ok", "送出鍵生效過、有送達證據：不是 pending");
        assert_eq!(delivery_of(&f, &new_turn).await, "ok");
        assert_eq!(send_now_presses(&f), 1, "補的時候不再按鍵");
    }

    /// 插隊送出欠著收尾時 run 結束了（pane 關了）：run 結束那一路收的是被插隊的那一筆，帳上跟它綁在一起的新那一則不能跟著
    /// 被忘掉或蓋掉——不然它永遠是 `run_id` 為空的 in_flight＋pending：同一個 request id 重問永遠 503、維護窗口的送達臨界區
    /// 永遠有它，直到 daemon 重啟。run 結束的當下 DB 好了（收得掉）、還沒好（又記成欠著）各一條。
    async fn a_run_that_ends_while_a_send_now_is_owed(healed: bool) {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let crid = format!("sn-run-ended-{healed}");
        let (running, new_turn) = an_owed_send_now(&f, &crid, healed).await;
        mark_run_exited(&app, &f.run_id, "pane exited").await;
        if !healed {
            sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        }

        let again = prompt_send_now(&app, &f.bot_id, "先看這句", &crid, &[], None)
            .await
            .unwrap_or_else(|e| panic!("healed={healed}：DB 好了，同一個 request id 要拿到這一則，拿到 {e:?}"));
        assert_eq!(again.turn_id, new_turn);
        assert_eq!(status_of(&f, &running).await, "failed", "healed={healed}");
        assert_eq!(status_of(&f, &new_turn).await, "failed", "healed={healed}：run 不在了，這一則接不上去，也不能一直在飛");
        assert_eq!(again.delivery, "ok", "healed={healed}");
        assert!(!crate::supervisor::store::delivery_critical_anywhere(&app.db).await.unwrap(), "healed={healed}：不再佔著送達臨界區");
        assert_eq!(send_now_presses(&f), 1, "healed={healed}");
    }

    #[tokio::test]
    async fn a_run_that_ends_while_a_send_now_is_owed_does_not_orphan_the_new_message() {
        a_run_that_ends_while_a_send_now_is_owed(true).await;
    }

    #[tokio::test]
    async fn a_run_that_ends_while_a_send_now_is_owed_and_the_db_is_still_down_does_not_orphan_the_new_message() {
        a_run_that_ends_while_a_send_now_is_owed(false).await;
    }

    /// 插隊送出欠著收尾（503）之後，使用者看 claude 還在忙、按 Esc（網頁不帶 turn_id）：補帳把新的那一則掛上 run，claude
    /// 正在處理的就是它——這一次 Esc 要打斷它，不是「上一次中斷的重試」：不能回 200 卻一個鍵都沒按。
    #[tokio::test]
    async fn an_esc_after_an_owed_send_now_interrupts_the_message_claude_is_working_on() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let (running, new_turn) = an_owed_send_now(&f, "sn-then-esc", true).await;

        interrupt_turn(&app, &f.bot_id, None).await.expect("Esc 進去了");
        let escs = f
            .env
            .herdr
            .calls_to("agent.send_keys")
            .iter()
            .filter(|p| p.get("keys").and_then(|k| k.as_array()).is_some_and(|k| k.iter().any(|k| k == "esc")))
            .count();
        assert_eq!(escs, 1, "使用者要停下 claude 正在做的：Esc 要按下去");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(status_of(&f, &new_turn).await, "failed", "被這次 Esc 打斷的是新的那一則");
        assert_eq!(notes_on(&f, &new_turn).await, vec![super::super::interruption::INTERRUPT_NOTE.to_string()]);
    }
}

/// 同一個 client_request_id 換了內容不能回第一則的結果（#337）：呼叫端會以為新的那則送達了，其實一個字都沒打。
/// 只有內容完全一樣的重送才是冪等；讀不到原本的內容也不敢說「一樣」，回錯讓呼叫端重試。
pub(crate) async fn check_same_text(app: &Arc<App>, turn: &db::Turn, text: &str) -> LcResult<()> {
    let stored: Option<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'user' ORDER BY created_at LIMIT 1")
        .bind(&turn.id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?;
    match stored {
        Some(prev) if prev != text => Err(LcError::conflict(
            "client_request_id already used with different text",
            json!({"reason": "text_mismatch", "turn_id": turn.id}),
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod restamp_tests {
    use super::*;
    use crate::lifecycle::delivery::DeliveryRecord;

    async fn turn_with_prompt(app: &Arc<App>, conv: &str, queued_at: &str, text: &str) -> (String, String) {
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','queued','pending',?)")
            .bind(&turn)
            .bind(conv)
            .bind(queued_at)
            .execute(&app.db)
            .await
            .unwrap();
        let msg = db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user',?,'web',?)")
            .bind(&msg)
            .bind(conv)
            .bind(&turn)
            .bind(text)
            .bind(queued_at)
            .execute(&app.db)
            .await
            .unwrap();
        (turn, msg)
    }

    async fn created(app: &Arc<App>, id: &str) -> String {
        sqlx::query_scalar("SELECT created_at FROM messages WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    /// 2026-09-30（cf-ox-2）：13:28 排進佇列、13:38:55 才送出的那一則，要排到送出的時間，回覆才接在它後面。
    #[tokio::test]
    async fn a_prompt_that_waited_in_the_queue_moves_to_when_it_was_sent() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let ok = DeliveryRecord { stored: "ok", verified: true, auto_resend: false };

        let (turn, msg) = turn_with_prompt(&app, &conv, "2026-09-30T13:28:31.333Z", "daemon 通知").await;
        mark_delivery(&app, &turn, ok, "2026-09-30T13:38:55.000Z").await.unwrap();
        assert_eq!(created(&app, &msg).await, "2026-09-30T13:38:55.000Z");
        // 之後補證據再記一次：delivered_at 不變，訊息也不再動。
        mark_delivery(&app, &turn, ok, "2026-09-30T13:50:00.000Z").await.unwrap();
        assert_eq!(created(&app, &msg).await, "2026-09-30T13:38:55.000Z");

        sqlx::query("UPDATE turns SET status='failed' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        // 沒排隊（幾百毫秒內就送出）的不動。
        let (turn, msg) = turn_with_prompt(&app, &conv, "2026-09-30T14:00:00.000Z", "一般送出").await;
        mark_delivery(&app, &turn, ok, "2026-09-30T14:00:00.600Z").await.unwrap();
        assert_eq!(created(&app, &msg).await, "2026-09-30T14:00:00.000Z");
    }

    /// SQLite may fail the post-delivery message update after the delivery transaction has committed.
    /// A later delivery observation must settle that debt with the original first-delivery time.
    #[tokio::test]
    async fn queued_prompt_restamp_retries_after_a_transient_message_update_failure() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q-restamp-retry").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let (turn, msg) = turn_with_prompt(&app, &conv, "2026-09-30T13:28:31.333Z", "daemon 通知").await;
        let ok = DeliveryRecord { stored: "ok", verified: true, auto_resend: false };

        sqlx::query("CREATE TABLE issue_738_restamp_block (turn_id TEXT PRIMARY KEY)").execute(&app.db).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER issue_738_fail_restamp BEFORE UPDATE OF created_at ON messages
             WHEN EXISTS (SELECT 1 FROM issue_738_restamp_block WHERE turn_id = OLD.turn_id)
             BEGIN SELECT RAISE(ABORT, 'issue 738 injected restamp failure'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO issue_738_restamp_block (turn_id) VALUES (?)")
            .bind(&turn)
            .execute(&app.db)
            .await
            .unwrap();

        let first_at = "2026-09-30T13:38:55.000Z";
        mark_delivery(&app, &turn, ok, first_at).await.unwrap();
        let delivered_at: String = sqlx::query_scalar("SELECT delivered_at FROM turns WHERE id=?")
            .bind(&turn)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(delivered_at, first_at, "delivery stays durable even when restamping fails");
        assert_eq!(created(&app, &msg).await, "2026-09-30T13:28:31.333Z", "the injected update failed once");

        sqlx::query("DELETE FROM issue_738_restamp_block WHERE turn_id=?").bind(&turn).execute(&app.db).await.unwrap();
        assert!(crate::testing::eventually!(created(&app, &msg).await == first_at), "background retry uses first delivered_at");
        mark_delivery(&app, &turn, ok, "2026-09-30T14:00:00.000Z").await.unwrap();
        assert_eq!(created(&app, &msg).await, first_at, "repeated retries are idempotent");
    }

    /// 別的寫入者正拿著寫入鎖時，欠著的 restamp 要等它、不是讀完 SELECT 才在升級成寫入時直接 `database is locked`。
    #[tokio::test]
    async fn queued_prompt_restamp_waits_for_a_busy_writer_instead_of_failing_on_upgrade() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q-restamp-busy").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let queued_at = "2026-09-30T13:28:31.333Z";
        let first_at = "2026-09-30T13:38:55.000Z";
        let (turn, msg) = turn_with_prompt(&app, &conv, queued_at, "daemon 通知").await;
        let ok = DeliveryRecord { stored: "ok", verified: true, auto_resend: false };
        mark_delivery(&app, &turn, ok, first_at).await.unwrap();
        sqlx::query("UPDATE messages SET created_at=? WHERE id=?").bind(queued_at).bind(&msg).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET restamp_pending=1 WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();

        let mut writer = app.db.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *writer).await.unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            sqlx::query("COMMIT").execute(&mut *writer).await.unwrap();
        });
        super::rearm_queued_prompt_restamps(&app).await.expect("must wait out the other writer, not fail with database is locked");
        release.await.unwrap();
        assert_eq!(created(&app, &msg).await, first_at);
    }

    #[tokio::test]
    async fn queued_prompt_restamp_keeps_debt_when_bot_lookup_fails() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q-restamp-bot-read").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let queued_at = "2026-09-30T13:28:31.333Z";
        let first_at = "2026-09-30T13:38:55.000Z";
        let (turn, msg) = turn_with_prompt(&app, &conv, queued_at, "daemon 通知").await;
        let ok = DeliveryRecord { stored: "ok", verified: true, auto_resend: false };
        mark_delivery(&app, &turn, ok, first_at).await.unwrap();

        // Recreate an uncompleted durable debt, then make the bot lookup fail once.
        sqlx::query("UPDATE messages SET created_at=? WHERE id=?").bind(queued_at).bind(&msg).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET restamp_pending=1 WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        let mut events = app.subscribe();
        crate::testing::make_table_unreadable(&app, "conversations").await;
        assert!(super::rearm_queued_prompt_restamps(&app).await.is_err(), "bot lookup failure must remain an error");
        assert_eq!(created(&app, &msg).await, queued_at, "failed lookup rolls back the timestamp update");
        let pending: i64 = sqlx::query_scalar("SELECT restamp_pending FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(pending, 1, "the durable debt remains set");

        crate::testing::make_table_readable(&app, "conversations").await;
        super::rearm_queued_prompt_restamps(&app).await.unwrap();
        assert_eq!(created(&app, &msg).await, first_at, "recovery uses first delivered_at");
        let pending: i64 = sqlx::query_scalar("SELECT restamp_pending FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(pending, 0);
        let event = events.recv().await.unwrap();
        assert_eq!(event.kind, "message_added");
        assert_eq!(event.data["bot_id"], bot.id);
        assert_eq!(event.data["message"]["id"], msg);
    }

    /// 在 first-delivery claim 前插入第二份 evidence，讓它先 claim；後到的 evidence 不能再移動訊息時間（#735）。
    #[tokio::test]
    async fn concurrent_delivery_evidence_only_restamps_for_the_first_claim() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q-race").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();

        let (turn, msg) = turn_with_prompt(&app, &conv, "2026-09-30T13:28:31.333Z", "daemon 通知").await;
        let turn2 = turn.clone();
        let app2 = app.clone();
        super::super::race_point::arm("mark_delivery_before_claim", &turn, move || async move {
            // 這筆在第一筆 claim 前進來，並先完成原子 claim。
            mark_delivery(
                &app2,
                &turn2,
                Delivered::Unverified.record().unwrap(),
                "2026-09-30T13:38:55.000Z",
            )
            .await
            .unwrap();
        });

        // 外層呼叫先抵達 claim 點後暫停；注入的 evidence 先 claim 13:38:55，外層稍後以另一份 evidence 寫入。
        mark_delivery(
            &app,
            &turn,
            Delivered::Submitted.record().unwrap(),
            "2026-09-30T13:39:02.000Z",
        )
        .await
        .unwrap();

        let delivered_at: String = sqlx::query_scalar("SELECT delivered_at FROM turns WHERE id=?")
            .bind(&turn)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(delivered_at, "2026-09-30T13:38:55.000Z");
        assert_eq!(created(&app, &msg).await, delivered_at, "訊息時間跟著第一個成功 claim，不跟後到的 evidence 移動");

        let (delivery, verified, auto_resend): (String, i64, i64) = sqlx::query_as(
            "SELECT delivery, delivery_verified, auto_resend FROM turns WHERE id=?",
        )
        .bind(&turn)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!((delivery.as_str(), verified, auto_resend), ("ok", 1, 0), "第二條 evidence 仍更新送達欄位");
    }
}
