//! The queued-prompt flush and the run/turn bookkeeping that frees a slot.

#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::capabilities::Db;
use super::*;
use super::s6_ports::QueueContext;
use super::send_now::ports::{MaintenancePort, SupervisorSendRepo};

/// Only linked schedule turns are fenced; ordinary prompts keep their existing admission rules.
/// Callers pass a SQL expression from source code, never request input. Keep soft-deleted rows
/// so cancelling a schedule cannot turn its queued prompt into an unlinked, eligible prompt.
pub(super) fn scheduled_turn_allowed_sql(turn_id: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM schedule_occurrences o LEFT JOIN schedules s ON s.id=o.schedule_id
          WHERE o.turn_id={turn_id} AND (o.status NOT IN ('pending','queued') OR s.id IS NULL
            OR s.enabled<>1 OR s.deleted_at IS NOT NULL OR o.generation<>s.generation))"
    )
}

/// Bot lock held. Retract a cancelled schedule and its queued turn in the same transaction.
async fn withdraw_invalid_schedule<C: QueueContext>(app: &C, turn_id: &str) -> anyhow::Result<bool> {
    let mut tx = db::begin_write(app.db()).await?;
    let allowed: bool = sqlx::query_scalar(&format!("SELECT {}", scheduled_turn_allowed_sql("?")))
        .bind(turn_id)
        .fetch_one(&mut *tx)
        .await?;
    if allowed {
        return Ok(false);
    }
    let why = "排程已停用、刪除或修改，尚未送出的任務已撤回";
    let revoked = revoke_queued_turn_tx(&mut tx, turn_id, why).await?;
    if revoked.is_some() {
        sqlx::query("UPDATE schedule_occurrences SET status='cancelled', error=?, updated_at=? WHERE turn_id=? AND status IN ('pending','queued')")
            .bind(why).bind(db::now()).bind(turn_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    if let Some(revoked) = revoked {
        announce_revoked(app, turn_id, revoked).await;
    }
    Ok(true)
}

/// Hand the oldest queued prompt to the agent, if it can take one now. Caller holds the bot lock
/// (the one-in-flight / one-queued unique indexes make a lost race an error). Early returns leave
/// the turn queued; after the claim, any give-up must put it back or fail it — `in_flight` +
/// `delivery='pending'` has no other way out. 那一句寫不進去就記成欠著（`owed_delivery`），回 `Err`（#158）。
pub async fn flush_queued_locked<C: QueueContext>(app: &C, bot_id: &str) -> anyhow::Result<()> {
    let failures_before = read_failure_count(bot_id);
    let res = flush_queued_inner(app, bot_id).await;
    if res.is_err() && read_failure_count(bot_id) == failures_before {
        schedule_read_failure_retry(app, bot_id, WITHDRAWAL_RECHECK);
    }
    if read_failure_count(bot_id) == failures_before {
        forget_read_failures(bot_id);
    }
    res
}

async fn flush_queued_inner<C: QueueContext>(app: &C, bot_id: &str) -> anyhow::Result<()> {
    match db::bot(app.db(), bot_id).await? {
        Some(bot) if crate::config::is_api_kind(&bot.kind) => return Ok(()),
        Some(_) => {},
        None => anyhow::bail!("queued bot is unavailable"),
    }
    match app.bot_handed_off_to(bot_id).await {
        Ok(None) => {}
        Ok(Some(to)) => {
            tracing::info!(bot = %bot_id, handed_off_to = %to, "queued prompts held: the project was handed off");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = %e, "讀不到專案是否已移交：排著的 prompt 留在佇列，稍後重新判斷");
            schedule_read_failure_retry(app, bot_id, std::time::Duration::from_secs(<C as MaintenancePort>::UNREADABLE_RETRY_SECS as u64));
            return Ok(());
        }
    }
    let conv = match db::conversation_id(app.db(), bot_id).await {
        Ok(conv) => conv,
        Err(error) => {
            tracing::warn!(error = ?error, bot = %bot_id, "could not get conversation for queued prompt flush");
            schedule_read_failure_retry(app, bot_id, WITHDRAWAL_RECHECK);
            return Ok(());
        }
    };
    let Some(turn) = db::queued_turn(app.db(), &conv).await? else { return Ok(()) };
    if db::bot(app.db(), bot_id).await?.is_some_and(|b| b.deleted_at.is_some()) {
        revoke_queued_turn(app, &turn.id, "bot 已被刪除，排著的 prompt 不再送出").await?;
        return Ok(());
    }
    match assignment_withdrawal(app, &turn.id).await {
        Ok(None) => {}
        Ok(Some(why)) => {
            if let Err(e) = revoke_queued_turn(app, &turn.id, &why).await {
                schedule_read_failure_retry(app, bot_id, WITHDRAWAL_RECHECK);
                return Err(e.context("交辦已經不要了，排著的這一則卻撤不掉：留在佇列，稍後再撤"));
            }
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(bot = %bot_id, turn = %turn.id, error = %e, "讀不到交辦是否已撤回：排著的 prompt 留在佇列，稍後重新判斷");
            schedule_read_failure_retry(app, bot_id, WITHDRAWAL_RECHECK);
            return Err(e.context("讀不到交辦是否已撤回：排著的 prompt 留在佇列"));
        }
    }
    if withdraw_invalid_schedule(app, &turn.id).await? {
        return Ok(());
    }
    if let Some(at) = turn.next_flush_at.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        let left = at.with_timezone(&chrono::Utc) - chrono::Utc::now();
        if left > chrono::Duration::zero() {
            schedule_flush_retry(app, bot_id, left.to_std().unwrap_or_default());
            return Ok(());
        }
    }
    let Some(mut run) = db::active_run(app.db(), bot_id).await? else { return Ok(()) };
    if run.state == "running" && matches!(run.agent_status.as_str(), "blocked" | "unknown")
        && super::claude_resume_menu::accept_if_shown(app, &run).await.unwrap_or(false)
    {
        let Some(current) = db::active_run(app.db(), bot_id).await? else { return Ok(()) };
        run = current;
    }
    if run.state != "running" || run.agent_status == "blocked" || run.agent_status == "working" {
        return Ok(());
    }
    let awaits_idle: i64 = sqlx::query_scalar("SELECT awaits_idle FROM turns WHERE id = ?")
        .bind(&turn.id)
        .fetch_one(app.db())
        .await?;
    if awaits_idle == 1 && run.agent_status != "idle" {
        return Ok(());
    }
    if db::in_flight_turn(app.db(), &run.id).await?.is_some() {
        return Ok(());
    }
    let Some(bot) = db::bot(app.db(), bot_id).await? else { return Ok(()) };
    match app.window_held().await {
        Ok(None) => {}
        Ok(Some(w)) => {
            let left = w.retry_after_secs(&db::now()).max(1) as u64;
            tracing::info!(bot = %bot_id, turn = %turn.id, until = %w.expires_at, holder = %w.owner,
                           "維護窗口開著：排隊的 prompt 等窗口關掉再送");
            schedule_flush_retry(app, bot_id, std::time::Duration::from_secs(left));
            return Ok(());
        }
        Err(_) => {
            tracing::warn!(bot = %bot_id, turn = %turn.id, "維護窗口狀態讀不到：排隊的 prompt 留在佇列，稍後重新判斷");
            schedule_read_failure_retry(app, bot_id, std::time::Duration::from_secs(<C as MaintenancePort>::UNREADABLE_RETRY_SECS as u64));
            return Ok(());
        }
    }
    if let super::resume_gate::Gate::Waiting { expected, left } = app.resume_gate(&bot, &run, &conv).await {
        tracing::info!(bot = %bot_id, turn = %turn.id, session = %expected, wait_s = left.as_secs(),
                       "resume 還沒驗證：排隊的 prompt 等 claude 回報 session 再送");
        schedule_flush_retry(app, bot_id, left);
        return Ok(());
    }
    match super::resume_nudge::withdraw_unless_resumed(app, &turn, &run).await {
        Ok(false) => {}
        Ok(true) => return Ok(()),
        Err(e) => {
            schedule_read_failure_retry(app, bot_id, WITHDRAWAL_RECHECK);
            return Err(e.context("續行提示該撤卻撤不掉：留在佇列，稍後再撤"));
        }
    }
    if let Some(hit) = super::quota_hold::blocking_hit(app, &bot, &turn.id).await {
        let wait = super::quota_hold::recheck_in(hit.until.as_deref(), chrono::Utc::now());
        super::quota_hold::note_held(bot_id);
        tracing::info!(bot = %bot_id, turn = %turn.id, until = ?hit.until, wait_s = wait.as_secs(),
                       "目標身份還沒有額度：排隊的 prompt 等額度回來或換身份再送");
        schedule_flush_retry(app, bot_id, wait);
        return Ok(());
    }
    if !queued_from_the_user(app, &turn).await {
        if let Some(left) = app.interrupt_grace_left(&bot, &run, &conv).await {
        tracing::info!(bot = %bot_id, turn = %turn.id, wait_s = left.as_secs(), "使用者剛 interrupt：排隊的派工等寬限結束再送");
        schedule_flush_retry(app, bot_id, left);
        return Ok(());
    }
    }
    let text = turn.prompt_text.clone().unwrap_or_default();
    if text.trim().is_empty() {
        if let Err(e) = super::turn_controller::set_status(app.db(), &turn.id, "queued", "failed", "prompt 是空的，送不出去").await {
            schedule_flush_retry(app, bot_id, std::time::Duration::from_secs(QUEUE_RETRY_BASE_SECS));
            return Err(e.context("空的 prompt 收不成 failed：留在佇列，稍後再收"));
        }
        app.turn_changed(&turn.id).await;
        return Ok(());
    }

    match app.relay_queue_denial(&turn.id).await {
        Ok(Some(why)) => {
            if let Err(e) = revoke_queued_turn(app, &turn.id, &why).await {
                schedule_read_failure_retry(app, bot_id, WITHDRAWAL_RECHECK);
                return Err(e.context("relay authorization denied; queued prompt could not be withdrawn"));
            }
            return Ok(());
        }
        Ok(None) => {},
        Err(e) => {
            schedule_read_failure_retry(app, bot_id, WITHDRAWAL_RECHECK);
            return Err(e.context("relay authorization unavailable; prompt remains queued"));
        }
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    {
        super::race_point::hit("flush_before_claim", bot_id).await;
    }
    match super::turn_controller::claim_queued(&mut *app.db().acquire().await?, &turn.id, &run.id).await? {
        super::turn_controller::Outcome::Applied => {}
        other => {
            tracing::info!(bot = %bot_id, turn = %turn.id, run = %run.id, ?other, "排隊的 prompt 沒認領到：留在佇列");
            if matches!(other, super::turn_controller::Outcome::Fenced(_)) {
                schedule_read_failure_retry(app, bot_id, WITHDRAWAL_RECHECK);
            }
            return Ok(());
        }
    }
    app.turn_changed(&turn.id).await;

    let held = app.window_held().await;
    if !matches!(held, Ok(None)) {
        let until = match &held {
            Ok(Some(w)) => chrono::Utc::now() + chrono::Duration::seconds(w.retry_after_secs(&db::now()).max(1)),
            _ => chrono::Utc::now() + chrono::Duration::seconds(<C as MaintenancePort>::UNREADABLE_RETRY_SECS),
        };
        tracing::info!(bot = %bot_id, turn = %turn.id, until = %until, "認領後發現維護窗口已經握著（或讀不到）：一個字都不打，放回佇列，不花重試");
        let until = until.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        return release_claim_or_owe(app, bot_id, &turn.id, &until).await;
    }

    let wait_key = rollout_wait_key(&run);
    if let Err(e) = app.pane_ready_for_prompt(&bot, &run, &conv).await {
        let why = match &e {
            LcError::Conflict(v) => v.get("reason").and_then(|r| r.as_str()).unwrap_or("conflict").to_string(),
            other => format!("{other:?}"),
        };
        return put_back_or_owe(app, bot_id, &conv, &turn.id, &format!("pane not ready for a prompt: {why}"), &wait_key).await;
    }

    let client = match client_for_run(app, &run).await {
        Ok(c) => c,
        Err(e) => return put_back_or_owe(app, bot_id, &conv, &turn.id, &format!("no herdr client: {e:?}"), &wait_key).await,
    };
    let waited_for_log = turn.rollout_wait_key.as_deref() == Some(wait_key.as_str()) && turn.rollout_waits >= CODEX_LOG_WAIT_RETRIES;
    let res = app.deliver_queued_prompt(&client, &run, &bot, &text, waited_for_log).await;
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
        Ok(Delivered::NotAttempted { reason, retry: true }) => {
            return put_back_or_owe(app, bot_id, &conv, &turn.id, reason, &wait_key).await;
        }
        Ok(Delivered::NotAttempted { reason, retry: false }) => {
            let hint = format!("沒有送出（{reason}）：這一則在這個 bot 上沒有辦法照原樣送出，所以一個字都沒打。");
            return app.close_owed(bot_id, &turn.id, "failed", &hint)
                .await
                .map(drop)
                .map_err(|e| e.context("排隊的 prompt 一個字都沒打、送不出去，回合卻收不成 failed（記成欠著）"));
        }
        Ok(Delivered::Unproven(why)) => {
            tracing::warn!(bot = %bot_id, reason = why, "queued prompt delivery could not be proven");
            "unknown"
        }
        Err(e) => {
            let blocked = e.downcast_ref::<HerdrError>().map(|h| h.code == "agent_blocked").unwrap_or(false);
            if blocked {
                return app.close_owed(bot_id, &turn.id, "failed", &format!("delivery failed: {e}"))
                    .await
                    .map(drop)
                    .map_err(|e| e.context("herdr 拒收了排隊的 prompt，回合卻收不成 failed（記成欠著）"));
            }
            tracing::warn!(bot = %bot_id, error = %e, "queued prompt delivery unknown");
            "unknown"
        }
    };
    let written = app.delivered_owed(bot_id, &turn.id, rec).await;
    if delivery == "ok" || delivery == "unverified" {
        app.arm_stall_and_progress(&run.id, bot_id, &turn.id).await;
    }
    if delivery == "unknown" && written.is_ok() {
        app.watch_late_proof(&run.id, bot_id, &turn.id);
    }
    written.map_err(|e| e.context("排隊的 prompt 送出去了，送達結果卻寫不進去（記成欠著）"))
}

/// 排著的這一則是使用者自己送的：User lane 的 `/prompt` 排的（`origin = web`，bot 忙碌時排的是 `awaits_idle = 1`，
/// bot 沒在跑時送的是 `awaits_start = 1`，#1362）。Relay 與 daemon 通知依 lane 仍受 interrupt grace 擋。
async fn queued_from_the_user(_app: &impl crate::capabilities::Db, turn: &db::Turn) -> bool {
    turn.origin == "web"
        && turn.queue_class == super::prompt::QueueClass::User.as_str()
        && (turn.awaits_idle == 1 || turn.awaits_start == 1)
}

/// Undo a `queued -> in_flight` claim that never became a delivery, and arm a retry timer for it —
/// the same backoff and retry limit as any other put-back (`defer_queued_turn`). Bot lock held and
/// only for a turn this flush claimed, so the queued-slot unique indexes (`turns_one_queued_user`/`_relay`/`_daemon`) cannot be violated.
/// 寫不進去回 `Err`，不當成放回去了（#158）：flush 經 [`put_back_or_owe`] 記成欠著，由 `owed_delivery` 補。
pub async fn put_back<C: QueueContext>(app: &C, bot_id: &str, conv: &str, turn_id: &str, reason: &str, wait_key: &str) -> anyhow::Result<()> {
    match defer_queued_turn(app, conv, turn_id, reason, wait_key).await? {
        Deferred::Requeued(delay) => {
            tracing::warn!(bot = %bot_id, turn = %turn_id, %reason, retry_in_s = delay.as_secs(), "queued prompt put back on the queue");
            schedule_flush_retry(app, bot_id, delay);
        }
        Deferred::GaveUp(message) => {
            tracing::warn!(bot = %bot_id, turn = %turn_id, %reason, "queued prompt gave up after its retry limit");
            app.message_added(bot_id, message).await;
        }
        Deferred::Settled(now) => tracing::info!(bot = %bot_id, turn = %turn_id, %reason, ?now, "要放回佇列時已經被別的路徑收掉：不重試"),
    }
    Ok(())
}

/// 認領之後一個字都沒打：放回佇列；那一句寫不進去就記成欠著、之後補（`owed_delivery`），flush 回 `Err`——
/// 不回普通的成功，那一筆也不會停在 in_flight＋pending 沒人收（#158）。
async fn put_back_or_owe<C: QueueContext>(app: &C, bot_id: &str, conv: &str, turn_id: &str, reason: &str, wait_key: &str) -> anyhow::Result<()> {
    app.put_back_owed(bot_id, conv, turn_id, reason, wait_key)
        .await
        .map_err(|e| e.context("認領了排隊的 prompt、一個字都沒打，卻放不回佇列（記成欠著）"))
}

/// 認領之後窗口握著（或讀不到）：放回佇列、不花送達重試（#1683）。寫不進去同 [`put_back_or_owe`]：記成欠著、之後補。
async fn release_claim_or_owe<C: QueueContext>(app: &C, bot_id: &str, turn_id: &str, until: &str) -> anyhow::Result<()> {
    app.release_claim_owed(bot_id, turn_id, until)
        .await
        .map_err(|e| e.context("認領了排隊的 prompt、一個字都沒打，卻放不回佇列（記成欠著）"))
}

/// 放回佇列、不花送達重試（#1683）：`in_flight -> queued`，`flush_retries` 不動，`until` 之後再試。
/// 別的路徑已經收掉的就不掛 timer（同 [`defer_queued_turn`] 的 `Settled`）。
pub async fn release_claim<C: QueueContext>(app: &C, bot_id: &str, turn_id: &str, until: &str) -> anyhow::Result<()> {
    let mut tx = db::begin_write(app.db()).await?;
    if super::turn_controller::return_to_queue_unspent(&mut tx, turn_id, until).await? != super::turn_controller::Outcome::Applied {
        tracing::info!(turn = turn_id, "要放回佇列時已經被別的路徑收掉：不掛 timer");
        return Ok(());
    }
    tx.commit().await?;
    let delay = chrono::DateTime::parse_from_rfc3339(until)
        .ok()
        .and_then(|t| (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().ok())
        .unwrap_or_default();
    tracing::info!(bot = %bot_id, turn = turn_id, retry_in_s = delay.as_secs(), "維護窗口握著：排隊的 prompt 放回佇列，不花送達重試");
    schedule_flush_retry(app, bot_id, delay);
    Ok(())
}

/// Put-backs a queued prompt spends waiting for a codex rollout that has not been written yet.
pub const CODEX_LOG_WAIT_RETRIES: i64 = 3;

/// Which run and session a rollout wait belongs to. A restarted bot or a new session is a new key.
pub fn rollout_wait_key(run: &db::Run) -> String {
    format!("{}:{}", run.id, run.native_session_id.as_deref().unwrap_or(""))
}

/// Backoff for a queued prompt that could not be typed yet: 15 s, doubling, capped at 5 min.
const QUEUE_RETRY_BASE_SECS: u64 = 15;
const QUEUE_RETRY_MAX_SECS: u64 = 300;
/// After this many put-backs the prompt is failed with an explanation (about 40 minutes of a box
/// that never emptied), so it cannot sit in the queue forever.
pub const QUEUE_RETRY_LIMIT: i64 = 12;

pub fn queue_retry_delay(retries: i64) -> std::time::Duration {
    let shift = retries.clamp(0, 16) as u32;
    std::time::Duration::from_secs(QUEUE_RETRY_BASE_SECS.saturating_mul(1u64 << shift).min(QUEUE_RETRY_MAX_SECS))
}

/// [`defer_queued_turn`] 做了什麼。
enum Deferred {
    /// 放回佇列了，這麼久之後再試。
    Requeued(std::time::Duration),
    /// 重試用完，收成 failed 並寫了說明。
    GaveUp(db::Message),
    /// 放回之前已經被別的路徑收掉（不拿 bot 鎖的 run 結束之類）：什麼都沒寫，不重試（issue #125）。
    Settled(super::turn_controller::Outcome),
}

/// Put a claimed turn back on the queue with its retry count and next attempt time, or — past the
/// limit — fail it with an explanation. One transaction either way.
async fn defer_queued_turn(
    app: &impl crate::capabilities::Db,
    conv: &str,
    turn_id: &str,
    reason: &str,
    wait_key: &str,
) -> anyhow::Result<Deferred> {
    #[cfg(all(test, feature = "daemon-test-harness"))]
    {
        super::race_point::hit("defer_before_return", turn_id).await;
    }
    let mut tx = db::begin_write(app.db()).await?;
    let (retries, queue_class): (i64, String) =
        sqlx::query_as("SELECT flush_retries, queue_class FROM turns WHERE id = ?").bind(turn_id).fetch_one(&mut *tx).await?;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::race_point::hit("defer_after_retries_read", turn_id).await;
    let notice = queue_class == super::prompt::QueueClass::DaemonNotice.as_str();
    if retries >= if notice { super::daemon_notice::RETRY_LIMIT } else { QUEUE_RETRY_LIMIT } {
        let out = super::turn_controller::fail_on(&mut tx, turn_id, super::turn_controller::DeliveryOnFail::Failed, "退避次數用完").await?;
        if out != super::turn_controller::Outcome::Applied {
            return Ok(Deferred::Settled(out));
        }
        let hint = if notice {
            super::daemon_notice::gave_up_hint(reason)
        } else {
            format!("沒有送出：試了 {QUEUE_RETRY_LIMIT} 次都沒辦法打字（最後一次是 {reason}），已停止自動重試。請清空輸入框後重送。")
        };
        let message = insert_message_tx(&mut tx, conv, Some(turn_id), "system", &hint, "system", false, None).await?;
        tx.commit().await?;
        return Ok(Deferred::GaveUp(message));
    }
    let delay = queue_retry_delay(retries);
    let next = (chrono::Utc::now() + chrono::Duration::from_std(delay)?).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let out = super::turn_controller::return_to_queue(&mut tx, turn_id, &next).await?;
    if out != super::turn_controller::Outcome::Applied {
        return Ok(Deferred::Settled(out));
    }
    if reason == "codex_log_not_ready" {
        sqlx::query(
            "UPDATE turns SET rollout_waits = CASE WHEN rollout_wait_key = ? THEN rollout_waits + 1 ELSE 1 END,
                              rollout_wait_key = ?
              WHERE id = ?",
        )
        .bind(wait_key)
        .bind(wait_key)
        .bind(turn_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    tracing::info!(turn = turn_id, reason, attempt = retries + 1, retry_in_s = delay.as_secs(), "queued prompt not sent yet; put back");
    Ok(Deferred::Requeued(delay))
}

/// 讀不到交辦、或排著的那一則撤不掉時，flush 多久之後再判斷一次（同維護窗口讀不到的那一條，#127）。
const WITHDRAWAL_RECHECK: std::time::Duration = std::time::Duration::from_secs(10);

/// 連續讀不到的次數（每顆 bot，只在記憶體）：重排的間隔跟著加倍（#1431）。flush 讀得到、沒再排重試就歸零。
static QUEUE_READ_FAILURES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u32>>> = std::sync::OnceLock::new();
/// 讀不到時重排的間隔上限（#1431）：5 分鐘。DB 真的壞很久，就每 5 分鐘試一次，不是每 10 秒。
const QUEUE_READ_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(300);

fn read_failure_count(bot_id: &str) -> u32 {
    QUEUE_READ_FAILURES.get_or_init(Default::default).lock().ok().and_then(|m| m.get(bot_id).copied()).unwrap_or(0)
}

fn forget_read_failures(bot_id: &str) {
    if let Ok(mut m) = QUEUE_READ_FAILURES.get_or_init(Default::default).lock() {
        m.remove(bot_id);
    }
}

/// 讀不到東西就排一次重試：`base` 起、每次加倍、上限 [`QUEUE_READ_RETRY_CAP`]（#1431）。
fn schedule_read_failure_retry<C: QueueContext>(app: &C, bot_id: &str, base: std::time::Duration) {
    let Ok(mut m) = QUEUE_READ_FAILURES.get_or_init(Default::default).lock() else {
        schedule_flush_retry(app, bot_id, base);
        return;
    };
    let c = m.entry(bot_id.to_string()).or_insert(0);
    *c = c.saturating_add(1);
    let shift = (*c - 1).min(16);
    drop(m);
    schedule_flush_retry(app, bot_id, base.saturating_mul(1u32 << shift).min(QUEUE_READ_RETRY_CAP));
}

/// 掛著這些狀態的交辦，它排著的那一則不送（[`assignment_withdrawal`]；`turn_controller::claim_queued` 認領時也看這張表）。
pub const WITHDRAWN_ASSIGNMENT: [&str; 5] = ["cancelled", "superseded", "failed", "blocked", "quota_blocked"];

/// 這筆 queued turn 掛的交辦已經不要了（cancelled／superseded／failed，或被保險絲停在 blocked）→ 寫進對話的撤銷理由；
/// 還要、或沒掛交辦 → `Ok(None)`。blocked 的交辦不在執行中，就算送出去，結果也沒有地方收（AGM 會以為沒送出而重派）。
/// **讀不到是錯誤，不是「還要」**（#159）：不知道交辦還要不要，就不能送。
pub async fn assignment_withdrawal(app: &(impl crate::capabilities::Db + SupervisorSendRepo), turn_id: &str) -> anyhow::Result<Option<String>> {
    let Some(a) = app.assignment_by_turn(turn_id).await? else { return Ok(None) };
    let (what, detail) = match a.status.as_str() {
        "cancelled" => ("已取消", a.review_reason.as_deref()),
        "superseded" => ("已被後續交辦取代", a.review_reason.as_deref()),
        "failed" => ("已判定失敗", a.review_reason.as_deref()),
        "blocked" => ("已停在 blocked", a.error.as_deref()),
        "quota_blocked" => ("在等額度回來，屆時會用新的一則重送", None),
        _ => return Ok(None),
    };
    let why = detail.map(str::trim).filter(|r| !r.is_empty()).map(|r| format!("（{r}）")).unwrap_or_default();
    Ok(Some(format!("排隊中的這則沒有送出：交辦 {} {what}{why}，一併撤銷，不會再送。", a.id)))
}

/// 撤銷一筆還在排隊的 turn：標成 failed、寫明理由、釋放這個對話的 queued 名額，**不送**。
/// 只動 `queued`——已經 in_flight 或送出的撤不回來，不假裝撤回。`Ok(true)`＝這次真的撤掉了。
pub async fn revoke_queued_turn<C: QueueContext>(app: &C, turn_id: &str, why: &str) -> anyhow::Result<bool> {
    let mut tx = app.db().begin().await?;
    let Some(revoked) = revoke_queued_turn_tx(&mut tx, turn_id, why).await? else { return Ok(false) };
    tx.commit().await?;
    announce_revoked(app, turn_id, revoked).await;
    Ok(true)
}

pub use crate::lc_error::Revoked;

/// [`revoke_queued_turn`] 的交易內版本：要跟別的寫入綁在一起時用（例如保險絲「撤成功才標 blocked」）。
/// `None`＝這筆已經不是 queued（被 flush 領走、或早就撤過），什麼都沒寫。commit 之後呼叫 [`announce_revoked`]。
pub async fn revoke_queued_turn_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    turn_id: &str,
    why: &str,
) -> anyhow::Result<Option<Revoked>> {
    if super::turn_controller::retract_queued(tx, turn_id).await? != super::turn_controller::Outcome::Applied {
        return Ok(None);
    }
    let (conv, bot_id): (String, String) = sqlx::query_as(
        "SELECT t.conversation_id, c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id = ?",
    )
    .bind(turn_id)
    .fetch_one(&mut **tx)
    .await?;
    let message = insert_message_tx(tx, &conv, Some(turn_id), "system", why, "system", false, None).await?;
    Ok(Some(Revoked { bot_id, message }))
}

pub async fn announce_revoked<C: QueueContext>(app: &C, turn_id: &str, revoked: Revoked) {
    tracing::info!(turn = turn_id, bot = %revoked.bot_id, "revoked a queued prompt that will not be sent");
    app.announce_revoked(turn_id, revoked).await;
}

/// 這顆 bot 已經沒有活著的 run：它排著的 queued turn 沒有人會送，收掉（AGM 2026-09-16）。
///
/// 排隊只會發生在「有 running run、正在回合中」的時候（`prompt_inner`），所以沒有 run 的 queued 一定是遺留的；
/// 留著的話會一直佔 queued 名額，還讓 restart safety 的 `delivery_critical` 永遠判成臨界區。
/// 還有活著的 run（例如重啟時新的已經起來）就不動——flush 會送。回傳撤掉的 turn id。
pub async fn revoke_orphaned_queued_turns<C: QueueContext>(app: &C, bot_id: &str, why: &str) -> Vec<String> {
    if !matches!(db::active_run(app.db(), bot_id).await, Ok(None)) {
        return Vec::new();
    }
    if super::restart_hold::in_progress(bot_id) {
        tracing::info!(bot = %bot_id, why, "重啟中：排著的 prompt 留給新的 run，不當孤兒撤掉");
        return Vec::new();
    }
    app.note_run_gone(bot_id, why).await;
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE c.bot_id = ? AND t.status = 'queued' AND t.awaits_start = 0",
    )
    .bind(bot_id)
    .fetch_all(app.db())
    .await
    .unwrap_or_default();
    let mut revoked = Vec::new();
    for id in ids {
        let text = format!("排隊中的這則沒有送出：{why}，這顆 bot 已經沒有在跑，一併撤銷，不會再送。");
        match revoke_queued_turn(app, &id, &text).await {
            Ok(true) => revoked.push(id),
            Ok(false) => {}
            Err(e) => tracing::error!(bot = %bot_id, turn = %id, error = %e, "could not revoke an orphaned queued prompt"),
        }
    }
    revoked
}

/// 全部 bot 掃一次遺留的 queued turn（定時掃描用；也收掉這個版本上線前就留下來的）。
pub async fn revoke_all_orphaned_queued_turns<C: QueueContext>(app: &C) -> Vec<String> {
    let bots: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'queued'
            AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = c.bot_id AND r.state IN ('starting','running','stopping'))",
    )
    .fetch_all(app.db())
    .await
    .unwrap_or_default();
    let mut revoked = Vec::new();
    for bot in bots {
        revoked.extend(revoke_orphaned_queued_turns(app, &bot, "它的 run 已經結束").await);
    }
    revoked
}

/// One pending retry timer per bot: its generation (only the timer still registered fires) and when
/// it goes off (so a sooner retry can take its place).
static QUEUE_RETRY_TIMERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (u64, tokio::time::Instant)>>> =
    std::sync::OnceLock::new();
static QUEUE_RETRY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
#[cfg(all(test, feature = "daemon-test-harness"))]
static TEST_QUEUE_FLUSH_REQUESTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
    std::sync::OnceLock::new();

/// How much sooner a new retry has to be before it replaces the timer already armed. Re-computing
/// the same `next_flush_at` (every startup re-arm does) lands a hair either side of the old
/// deadline; without the slack that would swap the timer out on every pass for nothing.
const QUEUE_RETRY_SOONER_SLACK: std::time::Duration = std::time::Duration::from_secs(1);

/// Arm `fire` after `delay`, unless this bot already has a retry timer that goes off by then —
/// extra wake-ups must not pile up timers. `false` = the one already armed is good enough.
///
/// A **sooner** retry does replace it. Refusing one used to mean the earlier timer (the interrupt
/// grace, say) burned first, found `next_flush_at` still in the future and left the bot with no
/// timer at all: an idle bot has no `working -> idle` edge left either, so the queued dispatch sat
/// there until the 30-minute fuse revoked it (review3 L1).
pub fn arm_queue_retry<F>(bot_id: &str, delay: std::time::Duration, fire: F) -> bool
where
    F: FnOnce() + Send + 'static,
{
    let timers = QUEUE_RETRY_TIMERS.get_or_init(Default::default);
    let deadline = tokio::time::Instant::now() + delay;
    let generation = {
        let Ok(mut map) = timers.lock() else { return false };
        if let Some((_, armed)) = map.get(bot_id) {
            if deadline + QUEUE_RETRY_SOONER_SLACK >= *armed {
                return false;
            }
        }
        let g = QUEUE_RETRY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        map.insert(bot_id.to_string(), (g, deadline));
        g
    };
    let bot_id = bot_id.to_string();
    tokio::spawn(Box::pin(async move {
        tokio::time::sleep(delay).await;
        let mine = timers.lock().map(|mut map| {
            if map.get(&bot_id).map(|(g, _)| *g) == Some(generation) {
                map.remove(&bot_id);
                true
            } else {
                false
            }
        });
        if mine.unwrap_or(false) {
            fire();
        }
    }));
    true
}

/// Queued prompts waiting out a backoff, one entry per bot (its soonest), with the time left.
pub async fn pending_queue_retries(app: &impl crate::capabilities::Db) -> anyhow::Result<Vec<(String, std::time::Duration)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        concat!("SELECT c.bot_id, COALESCE(t.next_flush_at, '1970-01-01T00:00:00Z')
           FROM turns t JOIN conversations c ON c.id = t.conversation_id
           LEFT JOIN bots b ON b.id=c.bot_id
          WHERE t.status = 'queued' AND (b.kind NOT IN ", crate::config::api_kinds_sql!(), " OR b.id IS NULL)"),
    )
    .fetch_all(app.db())
    .await?;
    let now = chrono::Utc::now();
    let mut soonest: std::collections::BTreeMap<String, std::time::Duration> = std::collections::BTreeMap::new();
    for (bot, at) in rows {
        let left = chrono::DateTime::parse_from_rfc3339(&at)
            .ok()
            .map(|t| (t.with_timezone(&chrono::Utc) - now).to_std().unwrap_or_default())
            .unwrap_or_default();
        soonest.entry(bot).and_modify(|d| *d = (*d).min(left)).or_insert(left);
    }
    Ok(soonest.into_iter().collect())
}

/// After a restart the in-memory retry timers are gone while `next_flush_at` survived: arm one per
/// bot again at `max(now, next_flush_at)` (sol review round nine #3). `fire` is what the timer does.
///
/// 讀不到回 `Err`，不是「沒有排著的」（#75 重開）：開機恢復把它記成欠著、之後再掃。
pub async fn rearm_queue_retries_with<F>(app: &impl crate::capabilities::Db, fire: F) -> anyhow::Result<usize>
where
    F: Fn(String) + Clone + Send + 'static,
{
    let pending = pending_queue_retries(app).await?;
    let mut armed = 0;
    for (bot, delay) in pending {
        let fire = fire.clone();
        let id = bot.clone();
        if arm_queue_retry(&bot, delay, move || fire(id)) {
            armed += 1;
            tracing::info!(bot = %bot, retry_in_s = delay.as_secs(), "re-armed a queued prompt retry after restart");
        }
    }
    Ok(armed)
}

pub async fn rearm_queue_retries<C: QueueContext>(app: &C) -> anyhow::Result<usize> {
        let a = (*app).clone();
    rearm_queue_retries_with(app, move |bot| schedule_flush_queued(&a, &bot)).await
}

/// Test hook: is a retry timer waiting for this bot?
#[cfg(test)]
pub fn queue_retry_timer_armed(bot_id: &str) -> bool {
    QUEUE_RETRY_TIMERS.get_or_init(Default::default).lock().map(|m| m.contains_key(bot_id)).unwrap_or(false)
}

/// Test hook: how long this bot's retry timer still has to run.
#[cfg(test)]
pub fn queue_retry_timer_left(bot_id: &str) -> Option<std::time::Duration> {
    let now = tokio::time::Instant::now();
    let (_, at) = *QUEUE_RETRY_TIMERS.get_or_init(Default::default).lock().ok()?.get(bot_id)?;
    Some(at.saturating_duration_since(now))
}

/// Test hook: what a process restart does to the in-memory timers.
#[cfg(test)]
pub fn forget_queue_retry_timer(bot_id: &str) {
    if let Ok(mut map) = QUEUE_RETRY_TIMERS.get_or_init(Default::default).lock() {
        map.remove(bot_id);
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
pub fn take_scheduled_flush_count(bot_id: &str) -> usize {
    TEST_QUEUE_FLUSH_REQUESTS
        .get_or_init(Default::default)
        .lock()
        .ok()
        .and_then(|mut requests| requests.remove(bot_id))
        .unwrap_or_default()
}

/// Try the queue again after `delay`, for conditions no lifecycle edge will announce.
pub fn schedule_flush_retry<C: QueueContext>(app: &C, bot_id: &str, delay: std::time::Duration) {
    let app = (*app).clone();
    let id = bot_id.to_string();
    arm_queue_retry(bot_id, delay, move || schedule_flush_queued(&app, &id));
}

/// Wake the durable prompt queue after a turn / Run transition. No-op in tests so a background
/// RPC cannot race the DB state machine they drive.
pub fn schedule_flush_queued<C: QueueContext>(app: &C, bot_id: &str) {
    #[cfg(all(test, feature = "daemon-test-harness"))]
    {
        let _ = app;
        if let Ok(mut requests) = TEST_QUEUE_FLUSH_REQUESTS.get_or_init(Default::default).lock() {
            *requests.entry(bot_id.to_string()).or_default() += 1;
        }
    }

    #[cfg(not(test))]
    {
        let app = (*app).clone();
        let bot_id = bot_id.to_string();
        tokio::spawn(Box::pin(async move {
            tokio::task::yield_now().await;
            let lock = crate::capabilities::BotLocks::bot_lock(&app, &bot_id).await;
            let _g = lock.lock_owned().await;
            if let Err(e) = flush_queued_locked(&app, &bot_id).await {
                tracing::warn!(bot = %bot_id, error = ?e, "queued prompt flush failed");
            }
        }));
    }
}

pub use crate::lc_error::RunExit;

/// Terminate a run: state `exited`, fail its in-flight turn, drop the pane watcher.
///
/// `exited` 的 durable commit 是後面每一步的前提（#135）：寫不進去就不能當它已經結束——
/// 收 in-flight、撤佇列、拆 watcher 都留到寫進去之後（重試或下一個 pane 事件）。
pub async fn mark_run_exited<C: QueueContext>(app: &C, run_id: &str, reason: &str) -> RunExit {
    mark_run_exited_proved(app, run_id, reason).await.0
}

/// 這一次 `exited` 轉移的證據（只有真的由這條路寫下 `exited` 才有；見 [`mark_run_exited_proved`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitProof {
    /// 轉移前是 `stopping`（使用者或 daemon 的 stop／restart 已經在做）。CAS 自己回報，不是事後推論。
    pub from_stopping: bool,
    /// 轉移當下的時間（寫完 `exited` 之後立刻取，格式同 [`db::now`]）。
    pub at: String,
}

/// [`mark_run_exited`] 加上轉移的證據。語意與原本的單一 CAS（`LIVE → exited`）完全等價，只是拆成兩步讓來源可知：
/// 先 `starting`／`running → exited`，輸了才 `stopping → exited`；兩步都輸才是 `AlreadyEnded`，任一步寫不進去照原路（不做第二步）。
pub async fn mark_run_exited_proved<C: QueueContext>(app: &C, run_id: &str, reason: &str) -> (RunExit, Option<ExitProof>) {
    let run = match db::run(app.db(), run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return (RunExit::AlreadyEnded, None),
        Err(e) => {
            tracing::warn!(run = run_id, reason, error = %e, "run exit not recorded: could not read the run; nothing torn down");
            return (RunExit::NotRecorded, None);
        }
    };
    if !super::run_state::LIVE.contains(&run.state.as_str()) {
        return (RunExit::AlreadyEnded, None);
    }
    match app.bot_handed_off_to(&run.bot_id).await {
        Ok(None) => {}
        Ok(Some(to)) => {
            tracing::info!(run = run_id, reason, handed_off_to = %to, "run exit ignored: the project was handed off");
            return (RunExit::AlreadyEnded, None);
        }
        Err(e) => {
            tracing::warn!(run = run_id, reason, error = %e, "run exit not recorded: could not tell whether the project was handed off");
            return (RunExit::NotRecorded, None);
        }
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    {
        super::race_point::hit("mark_run_exited_after_read", run_id).await;
    }
    let moved = match super::run_state::transition(app.db(), run_id, &["starting", "running"], "exited", None).await {
        Ok(super::run_state::Moved::Lost) => super::run_state::transition(app.db(), run_id, &["stopping"], "exited", None).await.map(|m| (m, true)),
        other => other.map(|m| (m, false)),
    };
    let from_stopping = match moved {
        Ok((super::run_state::Moved::Applied, from_stopping)) => from_stopping,
        Ok((super::run_state::Moved::Lost, _)) => {
            tracing::info!(run = run_id, reason, "run exit: another path ended the run first (CAS lost); its cleanup is that path's");
            return (RunExit::AlreadyEnded, None);
        }
        Err(e) => {
            tracing::warn!(run = run_id, reason, error = %e,
                "run exit not recorded (DB write failed); in-flight turn, queue and watcher left as they are");
            app.schedule_reconcile_settle(run_id, &run.state);
            return (RunExit::NotRecorded, None);
        }
    };
    let proof = ExitProof { from_stopping, at: db::now() };
    if let Err(e) = sqlx::query("UPDATE runs SET exit_reason = ? WHERE id = ?").bind(reason).bind(run_id).execute(app.db()).await {
        tracing::warn!(run = run_id, reason, error = %e, "run exited but its exit reason could not be recorded");
    }
    let turn_owed = match fail_in_flight_or_owe(app, run_id, &format!("run ended: {reason}")).await {
        Ok(()) => false,
        Err(e) => {
            tracing::warn!(run = run_id, reason, error = %e, "run exited but its in-flight turn could not be closed; owed, will be settled later");
            true
        }
    };
    revoke_orphaned_queued_turns(app, &run.bot_id, &format!("run 已結束（{reason}）")).await;
    if let Some(p) = run.pane_id.as_deref() {
        match db::bot_host(app.db(), &run.bot_id).await {
            Ok(host) => {
                if let Some(session) = crate::capabilities::HerdrRoutes::session_for_run(app, &run).await {
                    app.unwatch_pane_on_session(&host, &session, p).await;
                }
            }
            Err(e) => tracing::warn!(run = run_id, error = %e, "cannot read the host of an exited run; its pane watcher is left in place"),
        }
    }
    app.bot_status_changed(&run.bot_id).await;
    (if turn_owed { RunExit::TurnOwed } else { RunExit::Recorded }, Some(proof))
}

/// 收掉這個 run 還在飛的那一筆（failed＋說明同一個交易，CAS 在 `status='in_flight'`，輸了就不寫說明）。
///
/// **寫不進去回 `Err`，不假裝收掉了**（#156）：以前只記一行 warning，stop／重啟／run 結束接著照「回合已經收掉」做下去，
/// DB 裡那一筆卻還在飛。呼叫端各自決定：還沒動外面的（stop、子 agent 重啟）就不動、放回 running；
/// 外面已經發生的（run 結束）改用 [`fail_in_flight_or_owe`] 記成欠著、之後補。
///
/// CAS 在 UPDATE 自己身上（issue #68）：`mark_run_exited` 不拿 per-bot 鎖，SELECT 與 UPDATE 之間 hook 或 §4.3 備援
/// 把那一筆收掉的話，這裡什麼都不寫（`the_guard_refuses_to_resurrect_a_finished_turn` 與 turn_controller 的測試釘住）。
pub async fn fail_in_flight<C: QueueContext>(app: &C, run_id: &str, note: &str) -> anyhow::Result<()> {
    app.fail_in_flight(run_id, note).await
}

/// [`fail_in_flight`]，寫不進去時記成欠著的收尾（`interruption` 的帳），之後由定時重試、這顆 bot 的下一則 hook／prompt
/// 補上；`Err` 照樣回給呼叫端，讓它不回「收尾做完了」。只給外面已經發生、不能不做的那種（run 結束）。
pub async fn fail_in_flight_or_owe<C: QueueContext>(app: &C, run_id: &str, note: &str) -> anyhow::Result<()> {
    app.fail_in_flight_or_owe(run_id, note).await
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod flush_queue_tests {
    use super::*;

    fn at(iso: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&chrono::Utc)
    }

    /// AGM 2026-09-16：使用者 interrupt 之後，排隊的派工要等 bot **連續 idle** 滿寬限才送。
    /// 2b0fe98 原本的「使用者自己的回合跑完之後從那時重新計時」照規格 3 改掉：使用者有新輸入就不再擋，
    /// 那一回合結束後照一般規則馬上送；另照規格 4 改看 idle 狀態，中間 working 過就重算。
    /// （它原本的環境變數測試搬進 `interrupt_grace`，改成測解析函式。）
    #[test]
    fn the_interrupt_grace_counts_continuous_idle_and_yields_to_new_user_input() {
        use super::super::interrupt_grace::interrupt_grace_remaining as left;
        let s = std::time::Duration::from_secs;
        let grace = s(60);
        let now = at("2026-09-16T12:00:00Z");
        assert_eq!(left(None, false, s(0), now, grace), None);
        assert_eq!(left(Some(at("2026-09-16T11:59:40Z")), false, s(20), now, grace), Some(s(40)));
        assert_eq!(left(Some(at("2026-09-16T11:58:00Z")), true, s(10), now, grace), None);
        assert_eq!(left(Some(at("2026-09-16T11:58:00Z")), false, s(10), now, grace), Some(s(50)));
        assert_eq!(left(Some(at("2026-09-16T11:59:40Z")), false, s(600), now, grace), Some(s(40)));
        assert_eq!(left(Some(at("2026-09-16T11:58:00Z")), false, s(120), now, grace), None);
        assert_eq!(left(Some(at("2026-09-16T11:29:00Z")), false, s(1), now, grace), None);
    }


    #[test]
    fn the_retry_backoff_doubles_and_is_capped() {
        let secs: Vec<u64> = (0..8).map(|n| queue_retry_delay(n).as_secs()).collect();
        assert_eq!(secs, vec![15, 30, 60, 120, 240, 300, 300, 300]);
        assert_eq!(queue_retry_delay(-3).as_secs(), 15);
        assert_eq!(queue_retry_delay(i64::MAX).as_secs(), 300);
    }

    /// 每顆 bot 只會有一個重試 timer：多次喚醒不會疊出好幾條；觸發之後才可以再排（paused Tokio time）。
    #[tokio::test(start_paused = true)]
    async fn only_one_retry_timer_per_bot_and_it_fires_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let bot = format!("timer-bot-{}", db::ulid());
        let fired = Arc::new(AtomicUsize::new(0));
        let hit = |fired: &Arc<AtomicUsize>| {
            let fired = fired.clone();
            move || {
                fired.fetch_add(1, Ordering::SeqCst);
            }
        };
        assert!(arm_queue_retry(&bot, std::time::Duration::from_secs(15), hit(&fired)));
        assert!(!arm_queue_retry(&bot, std::time::Duration::from_secs(15), hit(&fired)), "second wake-up does not add a timer");
        assert!(!arm_queue_retry(&bot, std::time::Duration::from_secs(600), hit(&fired)), "nor does a later one");
        let other = format!("timer-bot-{}", db::ulid());
        assert!(arm_queue_retry(&other, std::time::Duration::from_secs(15), hit(&fired)), "another bot has its own");

        tokio::time::sleep(std::time::Duration::from_secs(14)).await;
        tokio::task::yield_now().await;
        assert_eq!(fired.load(Ordering::SeqCst), 0, "not before its delay");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fired.load(Ordering::SeqCst), 2, "each bot's single timer fired exactly once");

        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        tokio::task::yield_now().await;
        assert_eq!(fired.load(Ordering::SeqCst), 2, "and never again on its own");
        assert!(arm_queue_retry(&bot, std::time::Duration::from_secs(30), hit(&fired)), "after firing, the next retry can be armed");
    }

    /// 更早的重試要換掉已經掛著的 timer。不換的話（舊行為），早到的那個先燒、發現退避還沒到就走人，
    /// 這顆 bot 從此一個 timer 都沒有，排隊的派工卡到 30 分鐘保險絲才被撤掉（review3 L1）。
    #[tokio::test(start_paused = true)]
    async fn a_sooner_retry_replaces_the_timer_already_armed() {
        let bot = format!("timer-bot-{}", db::ulid());
        let fired: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mark = |fired: &Arc<std::sync::Mutex<Vec<u64>>>, secs: u64| {
            let fired = fired.clone();
            move || fired.lock().unwrap().push(secs)
        };
        assert!(arm_queue_retry(&bot, std::time::Duration::from_secs(60), mark(&fired, 60)));
        assert!(arm_queue_retry(&bot, std::time::Duration::from_secs(15), mark(&fired, 15)), "更早的換掉舊的");
        assert_eq!(queue_retry_timer_left(&bot), Some(std::time::Duration::from_secs(15)), "掛著的是新的那個");

        tokio::time::sleep(std::time::Duration::from_secs(16)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fired.lock().unwrap().as_slice(), &[15], "換上去的那個照自己的時間燒");
        assert!(!queue_retry_timer_armed(&bot), "燒完就把自己收掉");

        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fired.lock().unwrap().as_slice(), &[15], "被換掉的舊 timer 不會再燒一次");
        assert!(arm_queue_retry(&bot, std::time::Duration::from_secs(30), mark(&fired, 30)), "之後照樣可以再掛");
    }

}
