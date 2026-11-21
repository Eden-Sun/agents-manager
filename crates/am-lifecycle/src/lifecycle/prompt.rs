//! Delivering a prompt: the turn row, the pane write, and the queued path.

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::state::App;
use super::*;
use super::send_now::ports::AttachConnPort;

pub async fn emit_prompt_message(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), bot_id: &str, message_id: &str) -> anyhow::Result<()> {
    if let Some(m) = sqlx::query_as::<_, db::Message>("SELECT *, rowid AS seq FROM messages WHERE id=?")
        .bind(message_id)
        .fetch_optional(app.db())
        .await?
    {
        crate::capabilities::Emit::emit(app, "message_added", json!({"bot_id": bot_id, "message": m})).await;
    }
    Ok(())
}

/// Initial publication failure requests a snapshot refresh; an idempotent retry calls the fallible emitter again.
pub async fn publish_prompt_message(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), bot_id: &str, message_id: &str) {
    if let Err(e) = emit_prompt_message(app, bot_id, message_id).await {
        tracing::warn!(message = %message_id, error = %e, "could not read a prompt message for publication");
        crate::capabilities::Emit::emit(app, "resync", json!({"reason": "prompt_message_unreadable"})).await;
    }
}

/// 記下一則送達：`delivery`（CHECK 只認四種）＋ `delivery_verified`（有沒有證據）＋ `auto_resend`
/// （能不能自動重送）。後兩者是兩件事，見 [`crate::lifecycle::delivery::DeliveryRecord`]。`at` 是送出的那一刻。
/// 寫不進去回 `Err`，不吞（#149）：送出之後的呼叫端走 [`super::owed_delivery`]，記成欠著、不回普通的成功。
pub async fn mark_delivery(app: &Arc<impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::Emit + crate::capabilities::BotStatusEmit + 'static>, turn_id: &str, rec: DeliveryRecord, at: &str) -> anyhow::Result<()> {
    let verified = i64::from(rec.verified);
    let auto = i64::from(rec.auto_resend);
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("mark_delivery_before_claim", turn_id).await;

    let mut tx = app.db().begin().await?;
    let first_delivery = sqlx::query("UPDATE turns SET delivered_at=?, restamp_pending=1 WHERE id=? AND delivered_at IS NULL")
    .bind(at)
    .bind(turn_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    sqlx::query(
        "UPDATE turns SET delivery=?,
                delivery_verified=CASE WHEN status='completed' AND native_turn_id IS NOT NULL THEN 1 ELSE ? END,
                auto_resend=?,
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
async fn restamp_queued_prompt(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit), turn_id: &str) -> anyhow::Result<()> {
    let mut tx = db::begin_write(app.db()).await?;
    let target: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT t.created_at, t.delivered_at, b.kind FROM turns t LEFT JOIN conversations c ON c.id=t.conversation_id LEFT JOIN bots b ON b.id=c.bot_id WHERE t.id=? AND t.restamp_pending=1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((queued_at, first_at, kind)) = target else {
        tx.rollback().await?;
        return Ok(());
    };
    match kind.as_deref() {
        Some(k) if crate::config::is_api_kind(k) => { tx.rollback().await?; return Ok(()); }
        Some(_) => {},
        None => anyhow::bail!("queued restamp bot unavailable"),
    }
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
    let messages: Vec<db::Message> = sqlx::query_as(
        "SELECT *, rowid AS seq FROM messages WHERE turn_id=? AND role='user' AND sent_via IS NULL AND created_at<=?",
    )
    .bind(turn_id)
    .bind(&first_at)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    for message in messages {
        crate::capabilities::Emit::emit(app, "message_added", json!({"bot_id": bot_id, "message": message})).await;
    }
    sqlx::query("UPDATE turns SET restamp_pending=0 WHERE id=? AND delivered_at=? AND restamp_pending=1")
        .bind(turn_id)
        .bind(first_at)
        .execute(app.db())
        .await?;
    Ok(())
}

pub async fn rearm_queued_prompt_restamps(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::Emit + crate::capabilities::BotStatusEmit)) -> anyhow::Result<()> {
    let turns: Vec<String> = sqlx::query_scalar(concat!("SELECT t.id FROM turns t LEFT JOIN conversations c ON c.id=t.conversation_id LEFT JOIN bots b ON b.id=c.bot_id WHERE t.restamp_pending=1 AND (b.kind NOT IN ", crate::config::api_kinds_sql!(), " OR b.id IS NULL) ORDER BY t.created_at, t.id"))
        .fetch_all(app.db())
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

fn schedule_restamp_retry(app: &Arc<impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::Emit + crate::capabilities::BotStatusEmit + 'static>, turn_id: &str) {
    let app = (*app).clone();
    let turn_id = turn_id.to_string();
    tokio::spawn(Box::pin(async move {
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
    }));
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
pub fn not_attempted_error(run_id: &str, not: Delivered) -> LcError {
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

macro_rules! unretracted_type {
    ($vis:vis,) => {
        /// 撤不回來（DB 出錯）：改收成 failed 了，或那一句也寫不進去、記成欠著（`owed`，#158）。
        $vis struct Unretracted {
            cause: anyhow::Error,
            owed: Option<anyhow::Error>,
        }
    };
}
#[cfg(any(test, feature = "test-hooks"))]
unretracted_type!(pub,);
#[cfg(not(any(test, feature = "test-hooks")))]
unretracted_type!(pub(super),);

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

macro_rules! retraction_type {
    ($vis:vis,) => {
        /// 撤回一筆沒送出的 turn 的結果。
        #[derive(Debug, PartialEq, Eq)]
        $vis enum Retraction {
            /// turn 與它的訊息都撤掉了：同一個 `client_request_id` 可以原樣重送。
            Withdrawn,
            /// turn 已經不是 `in_flight`——窄窗裡別的路徑（`mark_run_exited` → `fail_in_flight`）先把它收掉了。
            /// 一個字都沒刪，照 turn 現在的樣子回答呼叫端。
            AlreadySettled,
        }
    };
}
#[cfg(any(test, feature = "test-hooks"))]
retraction_type!(pub,);
#[cfg(not(any(test, feature = "test-hooks")))]
retraction_type!(pub(super),);

/// Remove a turn and its user message that never reached the agent. They were committed before the
/// delivery so an early hook could match; nothing was typed, so nothing can match them now.
///
/// 刪之前先確認 turn 還是 `in_flight`，而且跟刪訊息在同一個交易裡：`fail_in_flight` 不拿 per-bot 鎖，
/// 會在「turn 已 commit、第一個字還沒打」這個窄窗裡把它標成 failed 並插一則「run ended」說明。訊息那句
/// 原本不帶條件，於是使用者那顆泡泡跟那則說明被一起刪掉，只留下一個空的 failed 回合（review3 L4）。
pub(super) async fn retract_unsent_turn(app: &impl LcHost, bot_id: &str, turn_id: &str, msg_id: &str) -> Result<Retraction, Unretracted> {
    let res = async {
        let mut tx = app.db().begin().await?;
        tx.unbind_attachment_message(msg_id, turn_id).await?;
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
            let owed = super::owed_delivery::closed(app, bot_id, turn_id, "failed", UNRETRACTED_NOTE).await.err();
            Err(Unretracted { cause: e, owed })
        }
    };
    crate::lifecycle::s6_ports::QueueServices::turn_changed(app, turn_id).await;
    if out.as_ref().map(|r| *r == Retraction::Withdrawn).unwrap_or(false) {
        crate::capabilities::Emit::emit(app, "resync", json!({"reason": "turn_retracted", "turn_id": turn_id})).await;
    }
    out
}

/// 這筆 turn 現在的樣子，換成給呼叫端的答覆。冪等重送（同一個 `client_request_id`）與「撤回時發現
/// turn 已經被別的路徑收掉」都走這裡，同一筆 turn 才不會因為問法不同而拿到兩種答案（review3 L4）。
pub(super) async fn answer_for_turn(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), t: &db::Turn) -> LcResult<PromptOut> {
    let message_id = sqlx::query_scalar::<_, String>("SELECT id FROM messages WHERE turn_id=? AND role='user' ORDER BY created_at, rowid LIMIT 1")
        .bind(&t.id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?
        .unwrap_or_default();
    if let Some(e) = super::owed_delivery::uncommitted_answer(t, &message_id) {
        return Err(e);
    }
    if !message_id.is_empty() {
        let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id=?")
            .bind(&t.conversation_id)
            .fetch_optional(app.db())
            .await
            .map_err(up)?
            .ok_or_else(|| LcError::Upstream("the prompt conversation is gone".into()))?;
        emit_prompt_message(app, &bot_id, &message_id).await.map_err(up)?;
    }
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
pub(super) async fn answer_for_turn_id(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), turn_id: &str) -> LcResult<PromptOut> {
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::Upstream("the prompt was not sent and its turn is gone".into()))?;
    answer_for_turn(app, &t).await
}

pub use crate::lc_error::PromptOut;

/// 插隊送出成功時，被打斷的那一回合會收到的系統說明。
pub const SEND_NOW_NOTE: &str = "被插隊送出打斷（claude send-now）";

/// 這一則 prompt 在維護窗口（`restart` 租約）前面算哪一類（issue #86）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// 一般送入（使用者、web、工具、AGM 派工）：窗口持有期間擋下，回 409 `maintenance_window`。
    Gated,
    /// 控制面自己的送入：AGM／協調者啟動時那一則握手。**不擋**——窗口就是為了「把東西停下來再起來」
    /// 而開的，連起來時的握手都擋掉就會把控制面鎖死在自己的閘門外（issue #86 驗收第三條）。
    /// 只有 daemon 自己呼叫得到，沒有從 HTTP 進得來的路。
    ControlPlane,
}

/// 窗口握著就回 `Some(那個 409)`；**讀不到窗口狀態也擋**，回 503 `maintenance_state_unavailable`（issue #127：
/// 觀測不到租約不等於沒有租約）。`ControlPlane` 一律放行。
pub(super) async fn maintenance_refusal(app: &impl crate::lifecycle::send_now::ports::MaintenancePort, admission: Admission) -> Option<LcError> {
    if admission == Admission::ControlPlane {
        return None;
    }
    match app.window_held().await {
        Ok(None) => None,
        Ok(Some(w)) => Some(w.refusal()),
        Err(unreadable) => Some(unreadable.refusal()),
    }
}

pub async fn prompt(app: &impl LcHost, bot_id: &str, text: &str, client_request_id: &str) -> LcResult<PromptOut> {
    prompt_with_attachments_and_relay(app, bot_id, text, client_request_id, &[], None).await
}

/// `prompt` with images (`attach.rs`): the agent gets paths on its host; the timeline renders
/// thumbnails from `messages.attachments_json`.
#[cfg(all(test, feature = "daemon-test-harness"))]
pub async fn prompt_with(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
) -> LcResult<PromptOut> {
    prompt_with_attachments_and_relay(app, bot_id, text, client_request_id, attachment_ids, None).await
}

pub use crate::lc_error::QueueClass;

#[derive(Debug, Clone, Copy, Default)]
pub struct RelaySrc<'a> {
    pub from: Option<&'a str>,
    pub unverified: bool,
    pub queue_class: QueueClass,
    /// 這則派工的可信依據（daemon `relay_policy::relay_prompt_basis`：`specialist:<專案>`／`deputy:<專案>`），寫進
    /// `messages.relay_auth`；turn 變成可送出的那一句依它正向確認（`relay_grant`）。原本就不靠依據的派工是 `None`。
    pub auth: Option<&'a str>,
    /// claude-api 這一回合送出時選的快取保留秒數（300／3600）；`None`＝預設 5 分鐘。其他 kind 忽略。
    pub cache_ttl_secs: Option<i64>,
    /// daemon 自己產生的回合（保溫、控制面握手、提醒…）：不是人打的字，帳本收件一律不收（#1862 修正）。
    pub daemon: bool,
}

impl<'a> RelaySrc<'a> {
    pub fn trusted(from: Option<&'a str>) -> Self {
        Self { from, unverified: false, queue_class: if from.is_some() { QueueClass::Relay } else { QueueClass::User }, auth: None, cache_ttl_secs: None, daemon: false }
    }

    pub fn user(from: Option<&'a str>) -> Self {
        Self { from, unverified: false, queue_class: QueueClass::User, auth: None, cache_ttl_secs: None, daemon: false }
    }

    pub fn daemon_notice(from: Option<&'a str>) -> Self {
        Self { from, unverified: false, queue_class: QueueClass::DaemonNotice, auth: None, cache_ttl_secs: None, daemon: true }
    }

    /// daemon 自己排的、走 User lane 的回合（主力保溫送 `/compact` 前的探測、控制面握手）。lane 不變，只標記來源。
    pub fn daemon_system() -> Self {
        Self { from: None, unverified: false, queue_class: QueueClass::User, auth: None, cache_ttl_secs: None, daemon: true }
    }

    /// 帶上 claude-api 這一回合的快取保留秒數（見 [`RelaySrc::cache_ttl_secs`]）。
    pub fn with_cache_ttl(mut self, secs: Option<i64>) -> Self {
        self.cache_ttl_secs = secs;
        self
    }

    /// 帶上授權依據（見 [`RelaySrc::auth`]）。
    pub fn with_auth(mut self, auth: Option<&'a str>) -> Self {
        self.auth = auth;
        self
    }
}

/// 同 `prompt_with`，記下 `relay_from`（bot id 或哨符 `daemon`）；UI 靠它把泡泡畫在左邊。
pub async fn prompt_relayed(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_with_attachments_and_relay(app, bot_id, text, client_request_id, attachment_ids, relay_from).await
}

/// `POST /bots/{id}/prompt`：一般送出（`send_now=false`）或插隊送出，來源已由 `relay_auth` 驗過（issue #339）。
#[allow(clippy::too_many_arguments)]
pub async fn prompt_from_api(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
    send_now: bool,
    clear_draft: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, attachment_ids, relay, false, false, Admission::Gated, send_now, clear_draft, None, None).await
}

/// `POST /prompt` with `queue_if_busy:true`: queue a user prompt only while the active run is busy.
#[allow(clippy::too_many_arguments)]
pub async fn prompt_from_api_queue_if_busy(
    app: &impl LcHost,
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
        attachment_ids,
        relay,
        true,
        true,
        Admission::Gated,
        send_now,
        clear_draft,
        None,
        None,
    )
    .await
}

/// Share-portal prompt route. The capability is checked inside `prompt_inner` after taking the bot lock.
pub async fn prompt_from_share_with_token(
    app: &impl LcHost,
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
        attachment_ids,
        relay,
        queue_if_busy,
        queue_if_busy,
        Admission::Gated,
        false,
        None,
        Some(token),
        None,
    )
    .await
}

/// 給受信任的內部來源排隊：呼叫端明確選 user、relay 或 daemon notice lane；caller-controlled request id 不參與分類。
///
/// 使用者與 web 的 `/prompt` 不走這條——那條路的語意變更要單獨評估。排進 `queued` 之後由既有的
/// `queue::flush_queued_locked` 在回合結束時送出，送達判定與證據記錄完全沿用（`Handed`／`auto_resend` 不變）。
/// 回傳 `delivery = "queued"` 代表「已排隊、還沒送」。
pub async fn prompt_relayed_queueable(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    relay_from: Option<&str>,
    queue_class: QueueClass,
) -> LcResult<PromptOut> {
    prompt_inner(
        app,
        bot_id,
        text,
        client_request_id,
        &[],
        RelaySrc { from: relay_from, unverified: false, queue_class, auth: None, cache_ttl_secs: None, daemon: false },
        true,
        false,
        Admission::Gated,
        false,
        None,
        None,
        None,
    )
    .await
}

/// 排一筆 `queued` turn 等下一回合，lane 由受信任的 `RelaySrc` 決定。
///
/// 界線都靠 DB：`turns_one_queued_user`／`turns_one_queued_relay`／`turns_one_queued_daemon`（每條槽每個對話最多一筆 queued，#1685）擋住「同一顆 bot 疊第二筆」，
/// `turns_client_req` 擋住「同一筆交辦重試疊出第二筆」——撞到就回原本的 409，讓呼叫端照舊退避。
/// `prompt_text` 存要送的內容（含附件路徑展開後的樣子），與 queue flush 用的是同一欄。
#[allow(clippy::too_many_arguments)]
/// 排隊用的 INSERT 失敗怎麼回（#341）：只有唯一約束衝突（同 lane 每對話一筆 queued、同一個 client_request_id）才是「已經有一筆在排」的 409；
/// 其他（SQLITE_BUSY、磁碟滿、schema 錯）是真的寫入失敗，照實回 502，不能偽裝成「已排隊」讓呼叫端以為送出去了、一直退避。
pub fn queue_insert_error(bot_id: &str, conv: &str, e: sqlx::Error) -> LcError {
    if e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
        tracing::info!(bot = %bot_id, error = %e, "另一筆 prompt 已經在排隊，這次回 409");
        LcError::conflict("a turn is already queued for this bot", json!({"conversation_id": conv}))
    } else {
        tracing::error!(bot = %bot_id, error = %e, "排隊的 turn 寫不進去");
        up(e)
    }
}

pub async fn queue_for_next_turn(
    app: &impl LcHost,
    conv: &str,
    bot_id: &str,
    text: &str,
    deliver: &str,
    client_request_id: &str,
    relay: RelaySrc<'_>,
) -> LcResult<PromptOut> {
    let turn_id = db::ulid();
    let msg_id = db::ulid();
    let mut tx = app.db().begin().await.map_err(up)?;
    let queued = sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, queue_class, created_at, prompt_text)
         VALUES (?,?,NULL,'web','queued','pending',?,?,?,?)",
    )
    .bind(&turn_id)
    .bind(conv)
    .bind(client_request_id)
    .bind(relay.queue_class.as_str())
    .bind(db::now())
    .bind(deliver)
    .execute(&mut *tx)
    .await;
    if let Err(e) = queued {
        return Err(queue_insert_error(bot_id, conv, e));
    }
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, relay_unverified, relay_auth, created_at) VALUES (?,?,?,'user',?,'web',?,?,?,?)",
    )
    .bind(&msg_id)
    .bind(conv)
    .bind(&turn_id)
    .bind(text)
    .bind(relay.from)
    .bind(relay.unverified)
    .bind(relay.auth)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    publish_prompt_message(app, bot_id, &msg_id).await;
    crate::lifecycle::s6_ports::QueueServices::turn_changed(app, &turn_id).await;
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

pub(super) fn agent_prompt_dialog_reason(kind: &str, screen: &str) -> Option<&'static str> {
    let scoped = if kind == "codex" {
        crate::composer_parse::codex_main_conversation(screen)
    } else {
        std::borrow::Cow::Borrowed(screen)
    };
    let screen = scoped.as_ref();
    match kind {
        "claude" if crate::tui_prompts::stuck_at_login(screen) => Some("needs_login"),
        "claude" if crate::tui_prompts::cold_resume_choice(screen).is_some() => Some(super::claude_resume_menu::PENDING),
        "claude" if crate::tui_prompts::dangerous_rm_prompt(screen).is_some() => Some("dangerous_rm_pending"),
        "claude" if crate::tui_prompts::is_auto_mode_offer(screen)
            || crate::tui_prompts::is_switch_model_dialog(screen) => Some("dialog_open"),
        "claude" if super::delivery::claude_open_choice_menu(screen) => Some("dialog_open"),
        "grok" if crate::tui_prompts::is_grok_trust_dialog(screen) => Some("dialog_open"),
        "agy" => crate::agy_screen::blocking_dialog(screen).map(|d| d.reason()),
        "codex" if super::codex_hook_review::review_open(screen) => Some(super::codex_hook_review::REASON),
        "codex" if crate::codex_live::permission_approval_open(screen) => Some("dialog_open"),
        "codex" if crate::codex_live::picker_open(screen) => Some("picker_open"),
        "codex" if super::codex_banner::blocks_typing(screen) => Some(super::codex_banner::REASON),
        _ => None,
    }
}

/// Last read-only fence for the `agent.prompt` route, which otherwise bypasses the pane composer.
pub async fn final_agent_prompt_guard(
    app: &impl LcHost,
    client: &super::RunClient,
    run: &db::Run,
    bot: &db::Bot,
    target: &str,
) -> Result<(), &'static str> {
    if !app.hosts().is_current(&client.fence).await {
        return Err("host_changed");
    }
    let current = match db::active_run(app.db(), &run.bot_id).await {
        Ok(Some(current)) => current,
        _ => return Err("run_unreadable"),
    };
    if current.id != run.id
        || current.state != "running"
        || current.pane_id != run.pane_id
        || current.native_session_id != run.native_session_id
        || current.herdr_session != run.herdr_session
    {
        return Err("run_changed");
    }
    let Some(agent) = client.agent_get(target).await.map_err(|_| "agent_unreadable")? else {
        return Err("agent_unreadable");
    };
    if agent.agent_session.is_none() {
        return Err("agent_session_missing");
    }
    if let (Some(expected), Some(actual)) = (run.native_session_id.as_deref(), super::dead_panes::herdr_session_id(&agent)) {
        if expected.trim() != actual {
            return Err("pane_reused");
        }
    }
    let pane = run
        .pane_id
        .as_deref()
        .map(str::trim)
        .filter(|pane| !pane.is_empty())
        .ok_or("pane_missing")?;
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
        if super::codex_hook_review::review_open(&read.text) {
            app.observe_codex_hook_review(run, &read.text).await;
        }
        super::codex_banner::observe(app, run, super::codex_banner::blocks_typing(&read.text)).await;
    }
    if let Some(reason) = agent_prompt_dialog_reason(&bot.kind, &read.text) {
        return Err(reason);
    }
    if bot.kind == "codex" && (run.resume_session_id.is_some() || run.resume_outcome.is_some()) {
        if let Err(super::delivery::Delivered::NotAttempted { reason, .. }) =
            super::delivery::composer_ready(app, run, "codex", &read.text).await
        {
            return Err(reason);
        }
    }
    Ok(())
}

pub async fn pane_ready_for_prompt(app: &impl LcHost, bot: &db::Bot, run: &db::Run, conv: &str) -> LcResult<()> {
    let pane = match run.pane_id.as_deref().map(str::trim) {
        Some("") => return Err(pane_guard_retry(run, "pane_unreadable")),
        Some(pane) => Some(pane),
        None => None,
    };
    let client = match pane {
        Some(_) => Some(client_for_run(app, run).await.map_err(|_| pane_guard_retry(run, "pane_unreadable"))?),
        None => None,
    };
    let mut screen = match (pane, client.as_ref()) {
        (Some(pane), Some(client)) => read_guard_screen(client, pane, run, "composer_unreadable").await?,
        _ => String::new(),
    };

    if bot.kind == "claude" && crate::tui_prompts::cold_resume_choice(&screen).is_some() {
        super::claude_resume_menu::accept_if_shown(app, run).await.map_err(|_| pane_guard_retry(run, super::claude_resume_menu::PENDING))?;
        if let (Some(pane), Some(client)) = (pane, client.as_ref()) {
            screen = read_guard_screen(client, pane, run, "composer_unreadable").await?;
        }
    }
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
            if let Some(rm) = crate::tui_prompts::dangerous_rm_prompt(&screen) {
                app.dangerous_rm_notify_once(run, &rm).await;
                return Err(LcError::conflict(
                    "dangerous_rm_pending",
                    json!({"run_id": run.id, "target": rm.target, "warning": rm.warning,
                           "message": "claude 停在防誤刪確認框（Dangerous rm），要使用者本人到終端回答；框關掉之後再送。"}),
                ));
            }
            if crate::tui_prompts::is_auto_mode_offer(&screen) {
                #[cfg(all(test, feature = "daemon-test-harness"))]
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
        if !crate::tui_prompts::is_auto_mode_offer(&screen)
            && !crate::tui_prompts::is_switch_model_dialog(&screen)
            && super::delivery::claude_open_choice_menu(&screen)
        {
            return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": "claude 正在等待選單選擇，請到終端處理後再送。"})));
        }
    }
    if bot.kind == "agy" {
        let dialog = crate::agy_screen::blocking_dialog(&screen).or_else(|| {
            super::agy_busy::tool_confirmation_pending(&run.id).then_some(crate::agy_screen::AgyDialog::Permission)
        });
        if let Some(dialog) = dialog {
            let hint = dialog.label();
            let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
            return Err(LcError::conflict(dialog.reason(), json!({"run_id": run.id, "message": hint})));
        }
    }
    if bot.kind == "grok" && crate::tui_prompts::is_grok_trust_dialog(&screen) {
        if let (Some(pane), Some(client)) = (pane, client.as_ref()) {
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::race_point::hit("grok_trust_before_host", &bot.id).await;
            let authority = async {
                let host = db::bot_host(app.db(), &bot.id).await?;
                let cwd = match bot.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(c) => c.to_string(),
                    None => db::project(app.db(), &bot.project_id).await?.map(|p| p.path).unwrap_or_default(),
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
                for warning in crate::lifecycle::s6_ports::StartServices::pretrust_for_start(app, bot, &host, &cwd).await {
                    tracing::warn!(bot = %bot.name, host, cwd, warning = %warning, "could not record grok folder trust");
                }
            }
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::race_point::hit("grok_trust_before_answer", &bot.id).await;
            let latest = read_guard_screen(client, pane, run, "pane_unreadable").await?;
            if crate::tui_prompts::is_grok_trust_dialog(&latest) {
                client.pane_send_keys(pane, &["y"]).await.map_err(|e| {
                    tracing::warn!(run = %run.id, error = %e, "could not answer grok's folder-trust dialog");
                    pane_guard_retry(run, "pane_guard_failed")
                })?;
                tokio::time::sleep(Duration::from_millis(1200)).await;
                #[cfg(all(test, feature = "daemon-test-harness"))]
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
    if bot.kind == "codex" {
        if let (Some(pane), Some(client)) = (pane, client.as_ref()) {
            if super::codex_hook_review::review_open(&screen) {
                app.observe_codex_screen(run, &screen).await;
                return Err(LcError::conflict(
                    "dialog_open",
                    json!({"run_id": run.id, "message": crate::codex_model_migration::HOOK_REVIEW_WAITING_HINT}),
                ));
            }
            if crate::tui_prompts::is_codex_model_migration_prompt(&screen) {
                app.observe_codex_screen(run, &screen).await;
                return Err(LcError::conflict(
                    "dialog_open",
                    json!({"run_id": run.id, "message": crate::codex_model_migration::WAITING_HINT}),
                ));
            }
            if crate::codex_live::rate_limit_switch_prompt_open(&screen) {
                app.observe_codex_screen(run, &screen).await;
                let hint = "codex 正在問要不要為了降低額度消耗切換模型；請到「終端」選擇，daemon 不會替你選，也不會把訊息打進選單。";
                return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
            }
            if crate::codex_update::update_menu_open(&screen) {
                app.observe_codex_screen(run, &screen).await;
                return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": crate::codex_update::MENU_HINT})));
            }
            if !app.close_codex_picker(client, pane).await {
                let hint = "codex 的 /model 選單擋在輸入列前面或畫面讀取失敗。請到「終端」分頁確認後再送一次。";
                let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                return Err(LcError::conflict("picker_open", json!({"run_id": run.id, "message": hint})));
            }
        }
    }
    Ok(())
}

pub async fn prompt_with_attachments_and_relay(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, attachment_ids, RelaySrc::trusted(relay_from), false, false, Admission::Gated, false, None, None, None).await
}

/// 主力保溫專用（#872）：跟 [`prompt`] 同一條路，但在 bot 鎖內確認 active run 仍是計畫時的那顆、仍 idle、沒有排隊回合，
/// 否則回 409 `superseded_run`（一個鍵都不打、不建 turn），由呼叫端下一輪依新 run 重判。
pub async fn prompt_expect_run(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    expected_run_id: &str,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, &[], RelaySrc::daemon_system(), false, false, Admission::Gated, false, None, None, Some(expected_run_id)).await
}

/// 插隊送出（issue #103）：照舊寫 turn／訊息進資料庫，但**不等 idle**——對 pane 送 claude 2.1.275 的
/// send-now 鍵（`ctrl+x ctrl+s`），由 CLI 自己收掉當下那一回合，daemon 這一側把被打斷的那一筆收成
/// `failed`（`fail_in_flight`），不留一個永遠 `in_flight` 的回合。
///
/// 不合資格（不是 claude、CLI 比 2.1.275 舊、版本還不知道）時**不插隊**：照原本的路走，忙的話一樣
/// 回 409，只是 body 多帶 `send_now_refused` 說清楚為什麼沒插隊。API 走 [`prompt_from_api`]；這支是測試用的捷徑。
#[cfg(all(test, feature = "daemon-test-harness"))]
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
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    prompt_inner(app, bot_id, text, client_request_id, &[], RelaySrc { from: relay_from, daemon: true, ..RelaySrc::trusted(relay_from) }, false, false, Admission::ControlPlane, false, None, None, None).await
}

#[allow(clippy::too_many_arguments)]
async fn prompt_inner(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
    queue_if_busy: bool,
    queue_awaits_idle: bool,
    admission: Admission,
    want_send_now: bool,
    clear_draft: Option<&str>,
    share_token: Option<&str>,
    expected_run: Option<&str>,
) -> LcResult<PromptOut> {
    let bot_kind = db::bot(app.db(), bot_id).await.map_err(|_| LcError::Upstream("bot_kind_unconfirmed".into()))?.ok_or_else(|| LcError::NotFound("bot".into()))?.kind;
    if crate::config::is_api_kind(&bot_kind) {
        if relay.from.is_some() || relay.unverified || relay.queue_class != QueueClass::User || queue_if_busy || queue_awaits_idle || admission != Admission::Gated || want_send_now || clear_draft.is_some() || share_token.is_some() || expected_run.is_some() {
            return Err(LcError::Bad("api_native_prompt_unsupported".into()));
        }
        if text.trim().is_empty() || client_request_id.trim().is_empty() { return Err(LcError::Bad("api_native_prompt_invalid".into())); }
        let out = crate::lifecycle::api_bot_port::ApiBotPort::api_native_prompt_ttl(app, bot_id, text, client_request_id, relay.cache_ttl_secs.unwrap_or(300), attachment_ids)
            .await.map_err(|error| match error {
                crate::lifecycle::api_bot_port::ApiNativePromptError::Unconfirmed => LcError::Upstream("api_native_prompt_unconfirmed".into()),
                crate::lifecycle::api_bot_port::ApiNativePromptError::Refused(reason) => LcError::conflict(reason, json!({"reason": reason})),
                crate::lifecycle::api_bot_port::ApiNativePromptError::ModelChanged { from, to } => LcError::conflict("api_model_changed", json!({
                    "reason": "api_model_changed",
                    "checkpoint_model": from,
                    "current_model": to,
                })),
            })?;
        super::ledger_ingest::ingest_user_prompt(app, relay, bot_id, &out.message_id, &out.turn_id, text).await;
        return Ok(out);
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("prompt_before_bot_lock", bot_id).await;
    let _g = crate::lifecycle::s6_ports::StartServices::lock_bot_for_start(app, bot_id).await;
    if let Some(token) = share_token {
        let valid = app.resolve_share_token(token).await.map_err(up)?;
        if valid.as_deref() != Some(bot_id) {
            return Err(LcError::NotFound("bot".into()));
        }
        app.touch_share(bot_id).await;
    }
    super::send_now::ports::HandoffSendRepo::refuse_handed_off(app, bot_id).await?;
    if let Err(e) = app.idle_sleep_wake_locked(bot_id, "有新的訊息要送進來").await {
        tracing::warn!(bot = %bot_id, error = %e, "could not wake a sleeping bot for a prompt");
    }
    if let Err(e) = super::interruption::settle_locked(app, bot_id, super::interruption::Evidence::Nothing).await {
        tracing::warn!(bot = %bot_id, error = %e, "上一次打斷欠著的收尾還是寫不進去");
    }
    if let Err(e) = super::owed_delivery::settle_locked(app, bot_id).await {
        tracing::warn!(bot = %bot_id, error = %e, "欠著的送達結果還是寫不進去");
    }

    match prompt_gate(app, bot_id, text, client_request_id, attachment_ids, relay, queue_if_busy, queue_awaits_idle, admission, want_send_now, clear_draft, expected_run).await? {
        Gated::Done(out) => Ok(out),
        Gated::Go(ready) => prompt_commit_and_deliver(app, bot_id, text, client_request_id, relay, admission, *ready).await,
    }
}

macro_rules! gated_types {
    ($vis:vis,) => {
        /// 閘門階段的結果：已經有答案（排隊／冪等／steer），或可以建 turn 並送出。
        $vis enum Gated {
            Done(PromptOut),
            Go(Box<Ready>),
        }

        /// 閘門階段通過後，建 turn 與送出需要的東西（小型值；沒有任何中間 future）。
        $vis struct Ready {
            bot: db::Bot,
            conv: String,
            files: Vec<crate::attach::Attachment>,
            deliver: String,
            run: db::Run,
            client: crate::lifecycle::RunClient,
            plan: Plan,
            interrupted: Option<db::Turn>,
            send_now: Option<&'static str>,
            send_now_active: bool,
        }
    };
}
#[cfg(any(test, feature = "test-hooks"))]
gated_types!(pub,);
#[cfg(not(any(test, feature = "test-hooks")))]
gated_types!(,);

/// `prompt_inner` 分三段、各段被呼叫端裝箱（#1461／#1465 的 1 MiB stack 守門）：整段寫在一個 async fn 裡時，debug 建置下
/// 它的 poll 框有 500 KiB 以上（每個 await 的暫存都各佔一份），握手那一拍光進來就吃掉預算的大半。分開之後同一時間只有一段的框在棧上。
#[allow(clippy::too_many_arguments)]
fn prompt_gate<'a>(
    app: &'a impl LcHost,
    bot_id: &'a str,
    text: &'a str,
    client_request_id: &'a str,
    attachment_ids: &'a [String],
    relay: RelaySrc<'a>,
    queue_if_busy: bool,
    queue_awaits_idle: bool,
    admission: Admission,
    want_send_now: bool,
    clear_draft: Option<&'a str>,
    expected_run: Option<&'a str>,
) -> impl std::future::Future<Output = LcResult<Gated>> + 'a {
    Box::pin(prompt_gate_body(app, bot_id, text, client_request_id, attachment_ids, relay, queue_if_busy, queue_awaits_idle, admission, want_send_now, clear_draft, expected_run))
}

#[allow(clippy::too_many_arguments)]
async fn prompt_gate_body(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
    relay: RelaySrc<'_>,
    queue_if_busy: bool,
    queue_awaits_idle: bool,
    admission: Admission,
    want_send_now: bool,
    clear_draft: Option<&str>,
    expected_run: Option<&str>,
) -> LcResult<Gated> {
    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    let bot = db::bot(app.db(), bot_id).await.map_err(up)?.filter(|b| b.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("bot".into()))?;
    db::project(app.db(), &bot.project_id)
        .await
        .map_err(up)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let conv = db::conversation_id(app.db(), bot_id).await.map_err(up)?;
    let files = app.resolve_attachments(bot_id, attachment_ids)
        .await
        .map_err(|e| LcError::Bad(e.to_string()))?;
    let deliver = super::pane_text::for_pane(&bot.kind, &crate::attach::deliver_text(text, &files));
    if super::pane_text::nothing_left(text, &deliver) {
        return Err(LcError::Bad("the prompt is empty once invisible characters and terminal escapes are removed".into()));
    }

    if let Some(t) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
        .bind(&conv)
        .bind(client_request_id)
        .fetch_optional(app.db())
        .await
        .map_err(up)?
    {
        check_same_text(app, &t, text).await?;
        return answer_for_turn(app, &t).await.map(Gated::Done);
    }
    app.note_keep_warm_prompt(bot_id, client_request_id).await;

    if queue_awaits_idle {
        super::busy_send::refuse_if_slot_taken(app, &conv, relay.queue_class).await?;
    }
    if let Some(refusal) = maintenance_refusal(app, admission).await {
        return Err(refusal);
    }

    let mut run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| {
        LcError::conflict("bot has no active run", json!({}))
    })?;
    if run.state != "running" {
        return Err(LcError::conflict("run is not running", json!({"run_id": run.id, "state": run.state})));
    }
    if bot.kind == "claude" && run.agent_status == "blocked" {
        if super::claude_resume_menu::accept_if_shown(app, &run).await
            .map_err(|_| pane_guard_retry(&run, super::claude_resume_menu::PENDING))?
        {
            run = db::active_run(app.db(), bot_id).await.map_err(up)?.ok_or_else(|| pane_guard_retry(&run, "run_changed"))?;
        }
    }
    if let Some(want) = expected_run {
        if run.id != want
            || run.agent_status != "idle"
            || db::queued_turn_for_bot(app.db(), bot_id).await.map_err(up)?.is_some()
        {
            return Err(LcError::conflict(
                "superseded_run",
                json!({"expected_run_id": want, "run_id": run.id, "agent_status": run.agent_status}),
            ));
        }
    }
    if queue_awaits_idle {
        let in_flight = db::in_flight_turn(app.db(), &run.id).await.map_err(up)?;
        if in_flight.is_some() || run.agent_status != "idle" {
            return super::busy_send::queue_awaiting_idle(app, &conv, bot_id, text, &deliver, client_request_id, relay, &files).await.map(Gated::Done);
        }
    }
    if run.agent_status == "blocked" {
        return Err(LcError::conflict("agent is blocked; answer the prompt first", json!({"run_id": run.id})));
    }
    let codex_on = want_send_now && bot.kind == "codex" && app.cfg().get().await.codex.instant_interrupt;
    let gate = want_send_now.then(|| {
        let codex_running = app.codex_running_version(&run.id);
        send_now::gate(&bot.kind, run.status_json.as_deref(), codex_running.as_deref(), codex_on, run.codex_instant_interrupt == Some(1))
    });
    let (refused, send_now_mode) = match gate {
        Some(Err(r)) => (Some(r), None),
        Some(Ok(m)) => (None, Some(m)),
        None => (None, None),
    };
    let send_now_ok = want_send_now && refused.is_none();
    let interrupted = match db::in_flight_turn(app.db(), &run.id).await.map_err(up)? {
        Some(t) if send_now_ok => Some(t),
        Some(t) => {
            if queue_if_busy {
                return queue_for_next_turn(app, &conv, bot_id, text, &deliver, client_request_id, relay).await.map(Gated::Done);
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
    let send_now_active = send_now_ok && interrupted.is_some();
    let send_now = match (want_send_now, refused, interrupted.is_some()) {
        (false, _, _) => None,
        (true, Some(r), _) => Some(r.code()),
        (true, None, true) => Some("interrupted"),
        (true, None, false) => Some("idle"),
    };
    if let super::resume_gate::Gate::Waiting { expected, left } = super::resume_gate::check(app, &bot, &run, &conv).await {
        if queue_awaits_idle {
            let out = super::busy_send::queue_awaiting_idle(app, &conv, bot_id, text, &deliver, client_request_id, relay, &files).await?;
            schedule_flush_retry(app, bot_id, left);
            return Ok(Gated::Done(out));
        }
        if queue_if_busy {
            let out = queue_for_next_turn(app, &conv, bot_id, text, &deliver, client_request_id, relay).await?;
            schedule_flush_retry(app, bot_id, left);
            return Ok(Gated::Done(out));
        }
        return Err(LcError::conflict(
            "resume_unverified",
            json!({"run_id": run.id, "session_id": expected, "retry_after_s": left.as_secs().max(1)}),
        ));
    }
    let users_own = queue_awaits_idle && relay.from.is_none();
    if queue_if_busy && !users_own {
        if let Some(wait) = super::interrupt_grace::hold(app, &bot, &run, &conv).await {
            if queue_awaits_idle {
                let out = super::busy_send::queue_awaiting_idle(app, &conv, bot_id, text, &deliver, client_request_id, relay, &files).await?;
                schedule_flush_retry(app, bot_id, wait);
                return Ok(Gated::Done(out));
            }
            let out = queue_for_next_turn(app, &conv, bot_id, text, &deliver, client_request_id, relay).await?;
            schedule_flush_retry(app, bot_id, wait);
            return Ok(Gated::Done(out));
        }
    }
    pane_ready_for_prompt(app, &bot, &run, &conv).await?;
    if let Some(t) = sqlx::query_as::<_, db::Turn>(
        "SELECT * FROM turns WHERE conversation_id=? AND delivery='unknown' AND status='in_flight' LIMIT 1",
    )
    .bind(&conv)
    .fetch_optional(app.db())
    .await
    .map_err(up)?
    {
        return Err(LcError::conflict("a previous turn has unknown delivery; abandon it first", json!({"turn_id": t.id})));
    }
    let client = client_for_run(app, &run).await?;
    if let Some(expect) = clear_draft {
        super::composer_draft::clear(app, &client, &run, &bot, expect, interrupted.is_some()).await?;
    }
    if let (Some(send_now::Mode::CodexSteer), Some(turn)) = (send_now_mode, interrupted.as_ref()) {
        return super::codex_steer::steer(app, &client, &run, &bot, turn, text, &deliver, client_request_id).await.map(Gated::Done);
    }
    let plan = match plan_delivery(app, &client, &run, &bot, &deliver, send_now_active, false).await.map_err(up)? {
        Ok(plan) if send_now_active => plan.submitting_with(Submit::SendNow),
        Ok(plan) => plan,
        Err(not) => return Err(super::composer_draft::with_draft(&client, &run, &bot, not_attempted_error(&run.id, not)).await),
    };

    Ok(Gated::Go(Box::new(Ready { bot, conv, files, deliver, run, client, plan, interrupted, send_now, send_now_active })))
}

fn prompt_commit_and_deliver<'a>(
    app: &'a impl LcHost,
    bot_id: &'a str,
    text: &'a str,
    client_request_id: &'a str,
    relay: RelaySrc<'a>,
    admission: Admission,
    ready: Ready,
) -> impl std::future::Future<Output = LcResult<PromptOut>> + 'a {
    Box::pin(prompt_commit_and_deliver_body(app, bot_id, text, client_request_id, relay, admission, ready))
}

async fn prompt_commit_and_deliver_body(
    app: &impl LcHost,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    relay: RelaySrc<'_>,
    admission: Admission,
    ready: Ready,
) -> LcResult<PromptOut> {
    let Ready { bot, conv, files, deliver, run, client, plan, interrupted, send_now, send_now_active } = ready;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("prompt_before_turn_commit", bot_id).await;

    let turn_id = db::ulid();
    let mut tx = app.db().begin().await.map_err(up)?;
    let inserted = sqlx::query(&format!(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at, prompt_text, auto_resend)
         SELECT ?, ?, ?, 'web', 'in_flight', 'pending', ?, ?, ?, 0
          WHERE EXISTS (
              SELECT 1 FROM runs
               WHERE id = ? AND bot_id = ? AND state = 'running'
                 AND pane_id IS ? AND herdr_session IS ? AND native_session_id IS ?
          ) AND {}",
        super::relay_grant::relay_allowed_params_sql()
    ))
    .bind(&turn_id)
    .bind(&conv)
    .bind(interrupted.is_none().then_some(run.id.as_str()))
    .bind(client_request_id)
    .bind(db::now())
    .bind(&deliver)
    .bind(&run.id)
    .bind(bot_id)
    .bind(run.pane_id.as_deref())
    .bind(run.herdr_session.as_deref())
    .bind(run.native_session_id.as_deref())
    .bind(relay.from)
    .bind(bot_id)
    .bind(relay.auth)
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    if inserted.rows_affected() == 0 {
        let grant_ok: bool = sqlx::query_scalar(&format!("SELECT {}", super::relay_grant::relay_allowed_params_sql()))
            .bind(relay.from).bind(bot_id).bind(relay.auth).fetch_one(&mut *tx).await.map_err(up)?;
        tx.rollback().await.map_err(up)?;
        if !grant_ok {
            return Err(LcError::conflict(
                "relay_dispatch_changed",
                json!({ "retryable": true, "sent": false, "message": "專家派工的依據已失效（開關已關、角色或所屬專案已變更）：沒有送出；請改送管理者" }),
            ));
        }
        return Err(LcError::conflict(
            "run_ended",
            json!({ "run_id": run.id, "retryable": true, "sent": false }),
        ));
    }
    let msg_id = db::ulid();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, relay_unverified, relay_auth, sent_via, created_at) VALUES (?,?,?,'user',?,'web',?,?,?,?,?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(text)
    .bind(relay.from)
    .bind(relay.unverified)
    .bind(relay.auth)
    .bind(send_now_active.then_some("send_now"))
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    if let Err(e) = app.bind_attachments(&msg_id, &files).await {
        publish_prompt_message(app, bot_id, &msg_id).await;
        super::ledger_ingest::ingest_user_prompt(app, relay, bot_id, &msg_id, &turn_id, text).await;
        let note = format!("delivery failed: attachment binding failed: {e}");
        return match super::owed_delivery::closed(app, bot_id, &turn_id, "failed", &note).await {
            Ok(true) => Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into(), send_now }),
            Ok(false) => answer_for_turn_id(app, &turn_id).await,
            Err(err) => Err(super::owed_delivery::uncommitted(Some(&run.id), &turn_id, &msg_id, "failed", Some(&err))),
        };
    }
    publish_prompt_message(app, bot_id, &msg_id).await;
    crate::lifecycle::s6_ports::QueueServices::turn_changed(app, &turn_id).await;

    if let Some(refusal) = maintenance_refusal(app, admission).await {
        match retract_unsent_turn(app, bot_id, &turn_id, &msg_id).await {
            Ok(Retraction::Withdrawn) => return Err(refusal),
            Ok(Retraction::AlreadySettled) => return answer_for_turn_id(app, &turn_id).await,
            Err(u) => return Err(u.answer(&run.id, &turn_id, &msg_id, "a maintenance window opened and the turn could not be withdrawn")),
        }
    }

    super::ledger_ingest::ingest_user_prompt(app, relay, bot_id, &msg_id, &turn_id, text).await;

    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("prompt_before_typing", bot_id).await;

    let res = match interrupted.as_ref() {
        None => execute_delivery(app, &client, &run, &bot, &deliver, plan).await,
        Some(old) => match super::send_now::deliver(app, &client, &run, &bot, &deliver, plan, old, &turn_id).await {
            super::send_now::Outcome::Interrupted(res) => res,
            super::send_now::Outcome::NotAttempted(not) => Ok(not),
            super::send_now::Outcome::NotSent(why) => return send_now_fell_through(app, bot_id, &run.id, &turn_id, &msg_id, why, false).await,
            super::send_now::Outcome::Unknown(why) => return send_now_fell_through(app, bot_id, &run.id, &turn_id, &msg_id, why, true).await,
            super::send_now::Outcome::Uncommitted(e) => {
                return Err(LcError::Uncommitted(json!({
                    "error": "send_now_state_uncommitted", "run_id": run.id, "turn_id": turn_id, "interrupted_turn_id": old.id,
                    "sent": true, "retryable": true, "detail": format!("{e:#}"),
                    "message": "送出鍵已經生效了，但回合的狀態還沒寫成；daemon 會自己補上，用同一個 client_request_id 重送會拿到這一則、不會再打一次。",
                })));
            }
        },
    };
    let mut rec = DeliveryRecord { stored: "unknown", verified: false, auto_resend: true };
    let delivery = match res {
        Ok(d @ (Delivered::Submitted | Delivered::Handed)) => {
            rec = d.record().expect("delivered outcome records");
            if matches!(d, Delivered::Submitted) { "ok" } else { "unverified" }
        }
        Ok(d @ Delivered::Unverified) => {
            rec = d.record().expect("delivered outcome records");
            "unverified"
        }
        Ok(not @ Delivered::NotAttempted { .. }) => {
            match retract_unsent_turn(app, bot_id, &turn_id, &msg_id).await {
                Ok(Retraction::Withdrawn) => return Err(super::composer_draft::with_draft(&client, &run, &bot, not_attempted_error(&run.id, not)).await),
                Ok(Retraction::AlreadySettled) => return answer_for_turn_id(app, &turn_id).await,
                Err(u) => return Err(u.answer(&run.id, &turn_id, &msg_id, "prompt was not sent and its turn could not be withdrawn")),
            }
        }
        Ok(Delivered::Unproven(why)) => {
            tracing::warn!(bot = %bot_id, reason = why, "prompt delivery could not be proven");
            "unknown"
        }
        Err(e) => {
            let blocked = e.downcast_ref::<HerdrError>().map(|h| h.code == "agent_blocked").unwrap_or(false);
            if blocked {
                if let Err(err) = super::owed_delivery::closed(app, bot_id, &turn_id, "failed", &format!("delivery failed: {e}")).await {
                    return Err(super::owed_delivery::uncommitted(Some(&run.id), &turn_id, &msg_id, "failed", Some(&err)));
                }
                return Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into(), send_now });
            }
            tracing::warn!(error = %e, "agent.prompt delivery unknown");
            "unknown"
        }
    };
    let written = super::owed_delivery::delivered(app, bot_id, &turn_id, rec).await;
    if delivery == "ok" || delivery == "unverified" {
        arm_stall(app, &run.id, bot_id, &turn_id).await;
        arm_progress(app, &run.id, bot_id, &turn_id).await;
    }
    if let Err(e) = written {
        return Err(super::owed_delivery::uncommitted(Some(&run.id), &turn_id, &msg_id, delivery, Some(&e)));
    }
    if delivery == "unknown" {
        super::late_proof::arm(app, &run.id, bot_id, &turn_id);
    }
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: delivery.into(), send_now })
}

/// 插隊送出沒有打斷任何東西（#120）：送出鍵沒生效（`unknown = false`）或不知道生效了沒有（`true`）。
/// 正在跑的那一回合原封不動；新的那一則收成 failed（不佔 run 的 in-flight 名額），送達記成 `failed`／`unknown`，
/// 並說清楚字可能還留在輸入框裡。收不成就記成欠著、回 503（#158）：不留一筆 run_id 為空的 in_flight 沒人收。
async fn send_now_fell_through(
    app: &impl LcHost,
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
        Ok(false) => answer_for_turn_id(app, turn_id).await,
        Err(e) => Err(super::owed_delivery::uncommitted(Some(run_id), turn_id, msg_id, delivery, Some(&e))),
    }
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod prompt_tests {
    use super::*;
    /// 上游 0.169.1 的審核框：派送被擋下、回可重試的 `codex_hook_review_pending`，不按任何鍵（不會被當成 composer 的草稿）。
    #[test]
    fn codex_hook_review_screen_is_a_retryable_reason_not_composer_busy() {
        let screen = include_str!("fixtures/codex-0.169.1-hooks-review.txt");
        assert_eq!(agent_prompt_dialog_reason("codex", screen), Some(crate::lifecycle::codex_hook_review::REASON));
        assert_eq!(agent_prompt_dialog_reason("codex", "• Done.\n\n› \n  gpt-5 · ~/project\n"), None);
    }
}

/// 同一個 client_request_id 換了內容不能回第一則的結果（#337）：呼叫端會以為新的那則送達了，其實一個字都沒打。
/// 只有內容完全一樣的重送才是冪等；讀不到原本的內容也不敢說「一樣」，回錯讓呼叫端重試。
pub async fn check_same_text(app: &impl crate::capabilities::Db, turn: &db::Turn, text: &str) -> LcResult<()> {
    let stored: Option<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'user' ORDER BY created_at LIMIT 1")
        .bind(&turn.id)
        .fetch_optional(app.db())
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

/// #1461／#1465 守門：`prompt_inner` 拆段之後，段與段之間傳的 `Ready` 要維持小型值（只放資料列與 client，不放中間 future）。
/// 真正的棧餘裕由 daemon 的 `*_fits_in_a_one_mib_stack` 守：握手那一拍（`prompt_control_plane`）就在 `the_plain_bot_gate_path_…` 裡。
#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod stack_shape_tests {
    #[test]
    fn the_prompt_stage_hand_off_stays_small() {
        const LIMIT: usize = 16 * 1024;
        assert!(std::mem::size_of::<super::Ready>() <= LIMIT, "Ready 有 {} 位元組，超過 {LIMIT}", std::mem::size_of::<super::Ready>());
        assert!(std::mem::size_of::<super::Gated>() <= LIMIT);
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub mod test_seams {
    //! #1862 L2：daemon 測試要碰的私有項，窄轉呼叫（只在測試建置存在）。
    use super::*;
    pub async fn restamp_queued_prompt(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::BotStatusEmit), turn_id: &str) -> anyhow::Result<()> { super::restamp_queued_prompt(app, turn_id).await }
    pub const UNRETRACTED_NOTE: &str = super::UNRETRACTED_NOTE;
    pub async fn retract_unsent_turn(app: &impl LcHost, bot_id: &str, turn_id: &str, msg_id: &str) -> Result<Retraction, Unretracted> { super::retract_unsent_turn(app, bot_id, turn_id, msg_id).await }
    pub async fn answer_for_turn(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), t: &db::Turn) -> LcResult<PromptOut> { super::answer_for_turn(app, t).await }
    pub fn agent_prompt_dialog_reason(kind: &str, screen: &str) -> Option<&'static str> { super::agent_prompt_dialog_reason(kind, screen) }
}
