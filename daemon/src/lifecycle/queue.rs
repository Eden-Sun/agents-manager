//! The queued-prompt flush and the run/turn bookkeeping that frees a slot.

use super::*;

/// Hand the oldest queued prompt to the agent, if it can take one now. Caller holds the bot lock
/// (the one-in-flight / one-queued unique indexes make a lost race an error). Early returns leave
/// the turn queued; after the claim, any give-up must put it back or fail it — `in_flight` +
/// `delivery='pending'` has no other way out. 那一句寫不進去就記成欠著（`owed_delivery`），回 `Err`（#158）。
pub(crate) async fn flush_queued_locked(app: &Arc<App>, bot_id: &str) -> anyhow::Result<()> {
    let conv = match db::conversation_id(&app.db, bot_id).await {
        Ok(conv) => conv,
        Err(error) => {
            tracing::warn!(error = ?error, bot = %bot_id, "could not get conversation for queued prompt flush");
            return Ok(());
        }
    };
    let Some(turn) = db::queued_turn(&app.db, &conv).await? else { return Ok(()) };
    // 交辦已經不要了（cancel／superseded／failed）：撤銷，不送。API 做決定的當下已經撤過一次，這裡是保險——
    // 繞過 API 改了狀態、或決定 commit 之後還沒撤就重啟，都不能讓一則已取消的指令在錯的時機送到（AGM 2026-09-16）。
    // 讀不到交辦的狀態不等於還要、撤不掉也不等於撤完了（#159）：都留在佇列（不認領、不花重試、一個字都不送），
    // 短 timer 再判斷一次。
    match assignment_withdrawal(app, &turn.id).await {
        Ok(None) => {}
        Ok(Some(why)) => {
            if let Err(e) = revoke_queued_turn(app, &turn.id, &why).await {
                schedule_flush_retry(app, bot_id, WITHDRAWAL_RECHECK);
                return Err(e.context("交辦已經不要了，排著的這一則卻撤不掉：留在佇列，稍後再撤"));
            }
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(bot = %bot_id, turn = %turn.id, error = %e, "讀不到交辦是否已撤回：排著的 prompt 留在佇列，稍後重新判斷");
            schedule_flush_retry(app, bot_id, WITHDRAWAL_RECHECK);
            return Err(e.context("讀不到交辦是否已撤回：排著的 prompt 留在佇列"));
        }
    }
    // Put back with a backoff: other wake-ups must not spend its retries early. Its timer brings it back.
    if let Some(at) = turn.next_flush_at.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        let left = at.with_timezone(&chrono::Utc) - chrono::Utc::now();
        if left > chrono::Duration::zero() {
            // 這次叫醒比退避早（例如中斷寬限的 timer 比 `next_flush_at` 先燒）：不動這一筆，但要留下一個
            // 到期才燒的 timer。燒掉的那個已經把自己從表上拿掉了，不補的話這顆 bot 一個 timer 都沒有，
            // 閒著也不會再有 `working -> idle` 邊，排隊的派工要等 30 分鐘保險絲才被撤掉（review3 L1）。
            // `arm_queue_retry` 本來就會去重，所以重複叫醒不會疊 timer。
            schedule_flush_retry(app, bot_id, left.to_std().unwrap_or_default());
            return Ok(());
        }
    }
    // One turn at a time, per SPEC §2: a queued prompt waits for the previous one to finish.
    let Some(run) = db::active_run(&app.db, bot_id).await? else { return Ok(()) };
    // `working` holds the queue too: a prompt pasted while claude is still drawing loses its
    // Enter and stalls (2026-09-07 11:21). The `working -> idle` edge re-schedules this flush.
    if run.state != "running" || run.agent_status == "blocked" || run.agent_status == "working" {
        return Ok(());
    }
    if db::in_flight_turn(&app.db, &run.id).await?.is_some() {
        return Ok(());
    }
    let Some(bot) = db::bot(&app.db, bot_id).await? else { return Ok(()) };
    // 維護窗口握著：排隊的這一筆**留在佇列**，不要送進一個正要被重啟的 session（issue #86）。
    // 不算重試、不動 `flush_retries`——擋住它的不是 bot 的狀態，是我們自己開的窗口，不該花掉它的額度。
    // 掛一個到窗口到期為止的 timer：窗口提早 release 時 `drain_queue` 會叫醒 flush，沒有人來收的話
    // 這個 timer 就是底線（租約到期即自動失效，不會鎖死）。
    // 讀不到窗口狀態也一樣留在佇列（issue #127）：觀測不到租約不等於沒有租約，這一筆不 claim、不花重試，
    // 過幾秒再判斷一次；DB 一恢復就照常往下走，不會永久卡住。
    match crate::supervisor::maintenance::window_held(app).await {
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
            schedule_flush_retry(app, bot_id, std::time::Duration::from_secs(crate::supervisor::maintenance::UNREADABLE_RETRY_SECS as u64));
            return Ok(());
        }
    }
    // `--resume` 接回之後還沒證明接回的是原本那段對話（issue #92）：留在佇列，不算重試，掛 timer 到期再來。
    // `SessionStart` 一到，`hookrecv` 那邊會叫醒這裡；到期沒來就由閘門自己走刻意的退路（`resume_gate`）。
    if let super::resume_gate::Gate::Waiting { expected, left } = super::resume_gate::check(app, &bot, &run, &conv).await {
        tracing::info!(bot = %bot_id, turn = %turn.id, session = %expected, wait_s = left.as_secs(),
                       "resume 還沒驗證：排隊的 prompt 等 claude 回報 session 再送");
        schedule_flush_retry(app, bot_id, left);
        return Ok(());
    }
    // 續行提示只送進驗證過的接回（#430，`resume_nudge`）：接回失敗、驗證不了就撤掉，不送進一段全新的對話。
    match super::resume_nudge::withdraw_unless_resumed(app, &turn, &run).await {
        Ok(false) => {}
        Ok(true) => return Ok(()),
        Err(e) => {
            schedule_flush_retry(app, bot_id, WITHDRAWAL_RECHECK);
            return Err(e.context("續行提示該撤卻撤不掉：留在佇列，稍後再撤"));
        }
    }
    // 目標身分還沒有額度（issue #108，`quota_hold`）：送進去只會再撞一次、派工白白燒掉。留在佇列、不花重試，
    // 掛 timer 到撞限到期（最多五分鐘再看一次）；換身分重啟（#106）會叫醒這裡。撞限記在這一列上，重啟後照樣擋。
    if let Some(hit) = super::quota_hold::blocking_hit(app, &bot, &turn.id).await {
        let wait = super::quota_hold::recheck_in(hit.until.as_deref(), chrono::Utc::now());
        super::quota_hold::note_held(bot_id);
        tracing::info!(bot = %bot_id, turn = %turn.id, until = ?hit.until, wait_s = wait.as_secs(),
                       "目標身分還沒有額度：排隊的 prompt 等額度回來或換身分再送");
        schedule_flush_retry(app, bot_id, wait);
        return Ok(());
    }
    // 使用者剛按了 interrupt：讓他先拿回輸入框（§4.4a）。判斷放在這裡（而不是觸發端）是因為 flush 不只一個呼叫端
    // （`stuck_turns` 在同一把鎖裡直接呼叫）；排在撤銷檢查之後，不要的派工照樣當場撤。閒著的 bot 不會再有
    // `working -> idle` 邊叫醒它，所以要掛 timer 到寬限結束。
    if let Some(left) = super::interrupt_grace::hold(app, &bot, &run, &conv).await {
        tracing::info!(bot = %bot_id, turn = %turn.id, wait_s = left.as_secs(), "使用者剛 interrupt：排隊的派工等寬限結束再送");
        schedule_flush_retry(app, bot_id, left);
        return Ok(());
    }
    let text = turn.prompt_text.clone().unwrap_or_default();
    if text.trim().is_empty() {
        // Nothing deliverable: drop it rather than leave the queue permanently blocked. 還沒認領：收不成就留在佇列、
        // 掛 timer 稍後再收，不當成丟掉了（#158）。
        if let Err(e) = super::turn_controller::set_status(&app.db, &turn.id, "queued", "failed", "prompt 是空的，送不出去").await {
            schedule_flush_retry(app, bot_id, std::time::Duration::from_secs(QUEUE_RETRY_BASE_SECS));
            return Err(e.context("空的 prompt 收不成 failed：留在佇列，稍後再收"));
        }
        emit_turn(app, &turn.id).await;
        return Ok(());
    }

    // 讀完 run、還沒認領的那一瞬（測試在這裡插進不拿 bot 鎖的 run 結束，issue #125）。
    #[cfg(test)]
    {
        super::race_point::hit("flush_before_claim", bot_id).await;
    }
    // Claim it first. If the CAS loses, another flush got there and this one has nothing to do.
    // 認領只認領到此刻還在跑的 run（issue #125）：讀完 run 之後它被收掉的話，這一筆留在佇列給下一個 run。
    // 掛的交辦也要此刻還要（#159）：上面讀完到這裡之間才被取消的，不被領走；下一輪照它的狀態撤。
    match super::turn_controller::claim_queued(&mut *app.db.acquire().await?, &turn.id, &run.id).await? {
        super::turn_controller::Outcome::Applied => {}
        other => {
            tracing::info!(bot = %bot_id, turn = %turn.id, run = %run.id, ?other, "排隊的 prompt 沒認領到：留在佇列");
            if matches!(other, super::turn_controller::Outcome::Fenced(_)) {
                schedule_flush_retry(app, bot_id, WITHDRAWAL_RECHECK);
            }
            return Ok(());
        }
    }
    emit_turn(app, &turn.id).await;

    let wait_key = rollout_wait_key(&run);
    // Refused by a screen check → back on the queue **with a timed retry**: a bot left idle on a
    // menu it cannot close produces no `working -> idle` edge, so waiting for one parked the prompt
    // until somebody happened to use the bot (review 2 L2).
    if let Err(e) = pane_ready_for_prompt(app, &bot, &run, &conv).await {
        let why = match &e {
            LcError::Conflict(v) => v.get("reason").and_then(|r| r.as_str()).unwrap_or("conflict").to_string(),
            other => format!("{other:?}"),
        };
        return put_back_or_owe(app, bot_id, &conv, &turn.id, &format!("pane not ready for a prompt: {why}"), &wait_key).await;
    }

    // From the claim to the RPC, giving up must requeue: `arm_stall` / `arm_progress` /
    // `try_fallback` all require `delivery == "ok"`, so an abandoned turn would 409 every
    // later prompt until the run ended, invisibly (background task).
    let client = match client_for_run(app, &run).await {
        Ok(c) => c,
        Err(e) => return put_back_or_owe(app, bot_id, &conv, &turn.id, &format!("no herdr client: {e:?}"), &wait_key).await,
    };
    // A queued prompt waits a few put-backs for a codex rollout that is on its way, then goes out
    // with what is available. Counted only for that reason and only for this run and session.
    let waited_for_log = turn.rollout_wait_key.as_deref() == Some(wait_key.as_str()) && turn.rollout_waits >= CODEX_LOG_WAIT_RETRIES;
    let res = deliver_prompt(app, &client, &run, &bot, &text, false, waited_for_log).await;
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
        // Nothing was typed. A temporary reason goes back on the queue with a timed retry — a busy
        // box produces no `working -> idle` edge to wake the flush (sol review round seven #2).
        Ok(Delivered::NotAttempted { reason, retry: true }) => {
            return put_back_or_owe(app, bot_id, &conv, &turn.id, reason, &wait_key).await;
        }
        // A prompt that can never be sent as asked on this run: fail it visibly, in one transaction
        // with its explanation, instead of retrying forever. 收不成就記成欠著（#158），不回普通的成功。
        Ok(Delivered::NotAttempted { reason, retry: false }) => {
            let hint = format!("沒有送出（{reason}）：這一則在這個 bot 上沒有辦法照原樣送出，所以一個字都沒打。");
            return super::owed_delivery::closed(app, bot_id, &turn.id, "failed", &hint)
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
                // 收成 failed 寫不進去就記成欠著、之後補（#149），不留一筆永久 in_flight＋pending。
                return super::owed_delivery::closed(app, bot_id, &turn.id, "failed", &format!("delivery failed: {e}"))
                    .await
                    .map(drop)
                    .map_err(|e| e.context("herdr 拒收了排隊的 prompt，回合卻收不成 failed（記成欠著）"));
            }
            // Not requeued: the agent may have taken it, so a retry could deliver twice.
            // `delivery='unknown'` is the designed user-visible parking state (§6.3).
            tracing::warn!(bot = %bot_id, error = %e, "queued prompt delivery unknown");
            "unknown"
        }
    };
    // 寫回（寫成才推 `turn_updated`）；寫不進去就記成欠著、之後補（#149），不回普通的成功。watchdog 照樣掛（字真的送出去了）。
    let written = super::owed_delivery::delivered(app, bot_id, &turn.id, rec).await;
    if delivery == "ok" || delivery == "unverified" {
        arm_stall(app, &run.id, bot_id, &turn.id).await;
        arm_progress(app, &run.id, bot_id, &turn.id).await;
    }
    written.map_err(|e| e.context("排隊的 prompt 送出去了，送達結果卻寫不進去（記成欠著）"))
}

/// Undo a `queued -> in_flight` claim that never became a delivery, and arm a retry timer for it —
/// the same backoff and retry limit as any other put-back (`defer_queued_turn`). Bot lock held and
/// only for a turn this flush claimed, so `turns_one_queued` cannot be violated.
/// 寫不進去回 `Err`，不當成放回去了（#158）：flush 經 [`put_back_or_owe`] 記成欠著，由 `owed_delivery` 補。
pub(super) async fn put_back(app: &Arc<App>, bot_id: &str, conv: &str, turn_id: &str, reason: &str, wait_key: &str) -> anyhow::Result<()> {
    match defer_queued_turn(app, conv, turn_id, reason, wait_key).await? {
        Deferred::Requeued(delay) => {
            tracing::warn!(bot = %bot_id, turn = %turn_id, %reason, retry_in_s = delay.as_secs(), "queued prompt put back on the queue");
            schedule_flush_retry(app, bot_id, delay);
        }
        Deferred::GaveUp => tracing::warn!(bot = %bot_id, turn = %turn_id, %reason, "queued prompt gave up after its retry limit"),
        Deferred::Settled(now) => tracing::info!(bot = %bot_id, turn = %turn_id, %reason, ?now, "要放回佇列時已經被別的路徑收掉：不重試"),
    }
    Ok(())
}

/// 認領之後一個字都沒打：放回佇列；那一句寫不進去就記成欠著、之後補（`owed_delivery`），flush 回 `Err`——
/// 不回普通的成功，那一筆也不會停在 in_flight＋pending 沒人收（#158）。
async fn put_back_or_owe(app: &Arc<App>, bot_id: &str, conv: &str, turn_id: &str, reason: &str, wait_key: &str) -> anyhow::Result<()> {
    super::owed_delivery::put_back(app, bot_id, conv, turn_id, reason, wait_key)
        .await
        .map_err(|e| e.context("認領了排隊的 prompt、一個字都沒打，卻放不回佇列（記成欠著）"))
}

/// Put-backs a queued prompt spends waiting for a codex rollout that has not been written yet.
pub(crate) const CODEX_LOG_WAIT_RETRIES: i64 = 3;

/// Which run and session a rollout wait belongs to. A restarted bot or a new session is a new key.
pub(crate) fn rollout_wait_key(run: &db::Run) -> String {
    format!("{}:{}", run.id, run.native_session_id.as_deref().unwrap_or(""))
}

/// Backoff for a queued prompt that could not be typed yet: 15 s, doubling, capped at 5 min.
const QUEUE_RETRY_BASE_SECS: u64 = 15;
const QUEUE_RETRY_MAX_SECS: u64 = 300;
/// After this many put-backs the prompt is failed with an explanation (about 40 minutes of a box
/// that never emptied), so it cannot sit in the queue forever.
pub(crate) const QUEUE_RETRY_LIMIT: i64 = 12;

pub(crate) fn queue_retry_delay(retries: i64) -> std::time::Duration {
    let shift = retries.clamp(0, 16) as u32;
    std::time::Duration::from_secs(QUEUE_RETRY_BASE_SECS.saturating_mul(1u64 << shift).min(QUEUE_RETRY_MAX_SECS))
}

/// [`defer_queued_turn`] 做了什麼。
enum Deferred {
    /// 放回佇列了，這麼久之後再試。
    Requeued(std::time::Duration),
    /// 重試用完，收成 failed 並寫了說明。
    GaveUp,
    /// 放回之前已經被別的路徑收掉（不拿 bot 鎖的 run 結束之類）：什麼都沒寫，不重試（issue #125）。
    Settled(super::turn_controller::Outcome),
}

/// Put a claimed turn back on the queue with its retry count and next attempt time, or — past the
/// limit — fail it with an explanation. One transaction either way.
async fn defer_queued_turn(
    app: &Arc<App>,
    conv: &str,
    turn_id: &str,
    reason: &str,
    wait_key: &str,
) -> anyhow::Result<Deferred> {
    // 認領之後、放回之前的那一瞬（測試在這裡插進不拿 bot 鎖的 run 結束，issue #125）。
    #[cfg(test)]
    {
        super::race_point::hit("defer_before_return", turn_id).await;
    }
    let mut tx = app.db.begin().await?;
    let retries: i64 = sqlx::query_scalar("SELECT flush_retries FROM turns WHERE id = ?").bind(turn_id).fetch_one(&mut *tx).await?;
    if retries >= QUEUE_RETRY_LIMIT {
        let out = super::turn_controller::fail_on(&mut tx, turn_id, super::turn_controller::DeliveryOnFail::Failed, "退避次數用完").await?;
        // 沒收到的（別的路徑先收掉了）不補「試了 N 次」的說明：那不是它結束的原因。
        if out != super::turn_controller::Outcome::Applied {
            return Ok(Deferred::Settled(out));
        }
        let hint = format!("沒有送出：試了 {QUEUE_RETRY_LIMIT} 次都沒辦法打字（最後一次是 {reason}），已停止自動重試。請清空輸入框後重送。");
        insert_message_tx(&mut tx, conv, Some(turn_id), "system", &hint, "system", false, None).await?;
        tx.commit().await?;
        return Ok(Deferred::GaveUp);
    }
    let delay = queue_retry_delay(retries);
    let next = (chrono::Utc::now() + chrono::Duration::from_std(delay)?).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let out = super::turn_controller::return_to_queue(&mut tx, turn_id, &next).await?;
    if out != super::turn_controller::Outcome::Applied {
        return Ok(Deferred::Settled(out));
    }
    if reason == "codex_log_not_ready" {
        // Only this reason spends the rollout wait, and a different run or session starts over.
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
const WITHDRAWAL_RECHECK: std::time::Duration = std::time::Duration::from_secs(crate::supervisor::maintenance::UNREADABLE_RETRY_SECS as u64);

/// 掛著這些狀態的交辦，它排著的那一則不送（[`assignment_withdrawal`]；`turn_controller::claim_queued` 認領時也看這張表）。
pub(crate) const WITHDRAWN_ASSIGNMENT: [&str; 5] = ["cancelled", "superseded", "failed", "blocked", "quota_blocked"];

/// 這筆 queued turn 掛的交辦已經不要了（cancelled／superseded／failed，或被保險絲停在 blocked）→ 寫進對話的撤銷理由；
/// 還要、或沒掛交辦 → `Ok(None)`。blocked 的交辦不在執行中，就算送出去，結果也沒有地方收（AGM 會以為沒送出而重派）。
/// **讀不到是錯誤，不是「還要」**（#159）：不知道交辦還要不要，就不能送。
pub(crate) async fn assignment_withdrawal(app: &Arc<App>, turn_id: &str) -> anyhow::Result<Option<String>> {
    let Some(a) = crate::supervisor::store::assignment_by_turn(&app.db, turn_id).await? else { return Ok(None) };
    let (what, detail) = match a.status.as_str() {
        "cancelled" => ("已取消", a.review_reason.as_deref()),
        "superseded" => ("已被後續交辦取代", a.review_reason.as_deref()),
        "failed" => ("已判定失敗", a.review_reason.as_deref()),
        "blocked" => ("已停在 blocked", a.error.as_deref()),
        // 額度回來後 controller 會用下一個 `#r<n>` 另開一則重送；這一則送出去就是做兩次。
        "quota_blocked" => ("在等額度回來，屆時會用新的一則重送", None),
        _ => return Ok(None),
    };
    let why = detail.map(str::trim).filter(|r| !r.is_empty()).map(|r| format!("（{r}）")).unwrap_or_default();
    Ok(Some(format!("排隊中的這則沒有送出：交辦 {} {what}{why}，一併撤銷，不會再送。", a.id)))
}


/// 撤銷一筆還在排隊的 turn：標成 failed、寫明理由、釋放這個對話的 queued 名額，**不送**。
/// 只動 `queued`——已經 in_flight 或送出的撤不回來，不假裝撤回。`Ok(true)`＝這次真的撤掉了。
pub(crate) async fn revoke_queued_turn(app: &Arc<App>, turn_id: &str, why: &str) -> anyhow::Result<bool> {
    let mut tx = app.db.begin().await?;
    let Some(revoked) = revoke_queued_turn_tx(&mut tx, turn_id, why).await? else { return Ok(false) };
    tx.commit().await?;
    announce_revoked(app, turn_id, revoked).await;
    Ok(true)
}

/// 撤銷已經 commit 之後要推的東西（訊息、turn 事件）。
pub(crate) struct Revoked {
    bot_id: String,
    message: db::Message,
}

/// [`revoke_queued_turn`] 的交易內版本：要跟別的寫入綁在一起時用（例如保險絲「撤成功才標 blocked」）。
/// `None`＝這筆已經不是 queued（被 flush 領走、或早就撤過），什麼都沒寫。commit 之後呼叫 [`announce_revoked`]。
pub(crate) async fn revoke_queued_turn_tx(
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

pub(crate) async fn announce_revoked(app: &Arc<App>, turn_id: &str, revoked: Revoked) {
    tracing::info!(turn = turn_id, bot = %revoked.bot_id, "revoked a queued prompt that will not be sent");
    emit_message_added(app, &revoked.bot_id, revoked.message).await;
    emit_turn(app, turn_id).await;
}

/// 這顆 bot 已經沒有活著的 run：它排著的 queued turn 沒有人會送，收掉（AGM 2026-09-16）。
///
/// 排隊只會發生在「有 running run、正在回合中」的時候（`prompt_inner`），所以沒有 run 的 queued 一定是遺留的；
/// 留著的話會一直佔 queued 名額，還讓 restart safety 的 `delivery_critical` 永遠判成臨界區。
/// 還有活著的 run（例如重啟時新的已經起來）就不動——flush 會送。回傳撤掉的 turn id。
pub(crate) async fn revoke_orphaned_queued_turns(app: &Arc<App>, bot_id: &str, why: &str) -> Vec<String> {
    if !matches!(db::active_run(&app.db, bot_id).await, Ok(None)) {
        return Vec::new();
    }
    // 重啟的 stop 與 start 之間（issue #106）：沒有 active run 是暫時的，排著的派工留給新的 run 送。
    // 重啟沒能把 bot 開回來時，`restart_bot_with` 結束重啟之後會自己再呼叫這支。
    if super::restart_hold::in_progress(bot_id) {
        tracing::info!(bot = %bot_id, why, "重啟中：排著的 prompt 留給新的 run，不當孤兒撤掉");
        return Vec::new();
    }
    // bot 沒在跑時使用者送的（issue #122，`start_send`）不撤：留在佇列等重新啟動或取消，只記下原因。
    super::start_send::note_run_gone(app, bot_id, why).await;
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE c.bot_id = ? AND t.status = 'queued' AND t.awaits_start = 0",
    )
    .bind(bot_id)
    .fetch_all(&app.db)
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
pub(crate) async fn revoke_all_orphaned_queued_turns(app: &Arc<App>) -> Vec<String> {
    let bots: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'queued'
            AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = c.bot_id AND r.state IN ('starting','running','stopping'))",
    )
    .fetch_all(&app.db)
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
pub(crate) fn arm_queue_retry<F>(bot_id: &str, delay: std::time::Duration, fire: F) -> bool
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
        // The timer being replaced stays asleep; its generation no longer matches, so it does nothing.
        map.insert(bot_id.to_string(), (g, deadline));
        g
    };
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
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
    });
    true
}

/// Queued prompts waiting out a backoff, one entry per bot (its soonest), with the time left.
pub(crate) async fn pending_queue_retries(app: &Arc<App>) -> anyhow::Result<Vec<(String, std::time::Duration)>> {
    // `next_flush_at IS NULL` 的也要收：AGM 派工排進來的 queued turn 沒有退避時間，靠回合結束的事件送出；
    // daemon 重啟時那個事件早就過去了，不補一次就變成永遠不會送的孤兒（AGM 2026-09-16）。
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.bot_id, COALESCE(t.next_flush_at, '1970-01-01T00:00:00Z')
           FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'queued'",
    )
    .fetch_all(&app.db)
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
pub(crate) async fn rearm_queue_retries_with<F>(app: &Arc<App>, fire: F) -> anyhow::Result<usize>
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

pub async fn rearm_queue_retries(app: &Arc<App>) -> anyhow::Result<usize> {
    let a = app.clone();
    rearm_queue_retries_with(app, move |bot| schedule_flush_queued(&a, &bot)).await
}

/// Test hook: is a retry timer waiting for this bot?
#[cfg(test)]
pub(crate) fn queue_retry_timer_armed(bot_id: &str) -> bool {
    QUEUE_RETRY_TIMERS.get_or_init(Default::default).lock().map(|m| m.contains_key(bot_id)).unwrap_or(false)
}

/// Test hook: how long this bot's retry timer still has to run.
#[cfg(test)]
pub(crate) fn queue_retry_timer_left(bot_id: &str) -> Option<std::time::Duration> {
    let now = tokio::time::Instant::now();
    let (_, at) = *QUEUE_RETRY_TIMERS.get_or_init(Default::default).lock().ok()?.get(bot_id)?;
    Some(at.saturating_duration_since(now))
}

/// Test hook: what a process restart does to the in-memory timers.
#[cfg(test)]
pub(crate) fn forget_queue_retry_timer(bot_id: &str) {
    if let Ok(mut map) = QUEUE_RETRY_TIMERS.get_or_init(Default::default).lock() {
        map.remove(bot_id);
    }
}

/// Try the queue again after `delay`, for conditions no lifecycle edge will announce.
pub fn schedule_flush_retry(app: &Arc<App>, bot_id: &str, delay: std::time::Duration) {
    let app = app.clone();
    let id = bot_id.to_string();
    arm_queue_retry(bot_id, delay, move || schedule_flush_queued(&app, &id));
}

/// Wake the durable prompt queue after a turn / Run transition. No-op in tests so a background
/// RPC cannot race the DB state machine they drive.
pub fn schedule_flush_queued(app: &Arc<App>, bot_id: &str) {
    if cfg!(test) {
        return;
    }
    let app = app.clone();
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        // Let the caller finish its current event / status write before taking the same lock.
        tokio::task::yield_now().await;
        let lock = app.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if let Err(e) = flush_queued_locked(&app, &bot_id).await {
            tracing::warn!(bot = %bot_id, error = ?e, "queued prompt flush failed");
        }
    });
}


/// [`mark_run_exited`] 做成了什麼。呼叫端多半不看，但「寫不進去」與「別的路徑先收了」要分得開（#135）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    /// 記成 `exited`，收尾（in-flight、孤兒佇列、watcher）做完了。
    Recorded,
    /// 這顆 run 已經不是 active（讀到時就不是，或 CAS 輸給先收掉它的路徑）：收尾歸那條路，這裡一樣都不做。
    AlreadyEnded,
    /// DB 讀寫失敗：什麼都沒動，run 照舊是 active；寫入失敗的排了對帳重試。
    NotRecorded,
    /// 記成 `exited`、孤兒佇列與 watcher 收了，但 in-flight 那一筆寫不進 failed（#156）：記成欠著的收尾，之後補上
    /// （`interruption` 的帳：定時重試、這顆 bot 的下一則 hook／prompt；daemon 重啟則由 `rearm_progress` 補收）。
    TurnOwed,
}

/// Terminate a run: state `exited`, fail its in-flight turn, drop the pane watcher.
///
/// `exited` 的 durable commit 是後面每一步的前提（#135）：寫不進去就不能當它已經結束——
/// 收 in-flight、撤佇列、拆 watcher 都留到寫進去之後（重試或下一個 pane 事件）。
pub async fn mark_run_exited(app: &Arc<App>, run_id: &str, reason: &str) -> RunExit {
    let run = match db::run(&app.db, run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return RunExit::AlreadyEnded,
        Err(e) => {
            tracing::warn!(run = run_id, reason, error = %e, "run exit not recorded: could not read the run; nothing torn down");
            return RunExit::NotRecorded;
        }
    };
    if !super::run_state::LIVE.contains(&run.state.as_str()) {
        return RunExit::AlreadyEnded;
    }
    // 讀完狀態、還沒寫 exited 的那一瞬（測試在這裡插進使用者 stop 的收尾寫入）。
    #[cfg(test)]
    {
        super::race_point::hit("mark_run_exited_after_read", run_id).await;
    }
    // CAS 在 UPDATE 自己身上（#131）：這支不拿 bot 鎖，上面讀完之後使用者的 stop 可能剛寫下 `stopped`，
    // 無條件寫 `exited` 會把「使用者要它停」蓋掉。沒寫到就是別的路徑先收掉了，後續由那條路負責。
    match super::run_state::transition(&app.db, run_id, super::run_state::LIVE, "exited", None).await {
        Ok(super::run_state::Moved::Applied) => {}
        Ok(super::run_state::Moved::Lost) => {
            tracing::info!(run = run_id, reason, "run exit: another path ended the run first (CAS lost); its cleanup is that path's");
            return RunExit::AlreadyEnded;
        }
        Err(e) => {
            tracing::warn!(run = run_id, reason, error = %e,
                "run exit not recorded (DB write failed); in-flight turn, queue and watcher left as they are");
            super::run_state::schedule_settle(app, run_id, super::run_state::Settle::Reconcile { stuck: run.state.clone() });
            return RunExit::NotRecorded;
        }
    }
    // 只有 CAS 真的寫下 exited 的這一條路記原因（每個 run 只會成功一次，後到的在上面就回 AlreadyEnded）。#554：
    // child 退役紀錄靠它分辨 pane 是 herdr 報關掉的還是對帳才發現不見。
    // 記不下來只是少一個證據：退役紀錄讀到 NULL 就當成沒親眼看到，偏向「不算刻意」。
    if let Err(e) = sqlx::query("UPDATE runs SET exit_reason = ? WHERE id = ?").bind(reason).bind(run_id).execute(&app.db).await {
        tracing::warn!(run = run_id, reason, error = %e, "run exited but its exit reason could not be recorded");
    }
    // pane 已經沒了，這不是我們能不做的事：回合收不成就記成欠著、之後補（#156），不當成已經收掉。
    // 撤佇列與拆 watcher 是 run 結束的事，跟回合收不收得成無關，照做。
    let turn_owed = match fail_in_flight_or_owe(app, run_id, &format!("run ended: {reason}")).await {
        Ok(()) => false,
        Err(e) => {
            tracing::warn!(run = run_id, reason, error = %e, "run exited but its in-flight turn could not be closed; owed, will be settled later");
            true
        }
    };
    revoke_orphaned_queued_turns(app, &run.bot_id, &format!("run 已結束（{reason}）")).await;
    if let Some(p) = run.pane_id.as_deref() {
        // watcher 的 key 是 (host, session, pane)。讀不到主機就不拆（#198）：以前退回 `local`，本機剛好同名 session、同 pane id
        // 的那顆 bot 的 watcher 被拆掉，它的狀態事件從此收不到。留下一個死 pane 的 watcher 無害。
        match db::bot_host(&app.db, &run.bot_id).await {
            Ok(host) => {
                if let Some(session) = app.session_for_run(&run).await {
                    crate::events::unwatch_pane_on_session(app, &host, &session, p).await;
                }
            }
            Err(e) => tracing::warn!(run = run_id, error = %e, "cannot read the host of an exited run; its pane watcher is left in place"),
        }
    }
    app.emit_bot_status(&run.bot_id).await;
    if turn_owed {
        RunExit::TurnOwed
    } else {
        RunExit::Recorded
    }
}

/// 收掉這個 run 還在飛的那一筆（failed＋說明同一個交易，CAS 在 `status='in_flight'`，輸了就不寫說明）。
///
/// **寫不進去回 `Err`，不假裝收掉了**（#156）：以前只記一行 warning，stop／重啟／run 結束接著照「回合已經收掉」做下去，
/// DB 裡那一筆卻還在飛。呼叫端各自決定：還沒動外面的（stop、子 agent 重啟）就不動、放回 running；
/// 外面已經發生的（run 結束）改用 [`fail_in_flight_or_owe`] 記成欠著、之後補。
///
/// CAS 在 UPDATE 自己身上（issue #68）：`mark_run_exited` 不拿 per-bot 鎖，SELECT 與 UPDATE 之間 hook 或 §4.3 備援
/// 把那一筆收掉的話，這裡什麼都不寫（`the_guard_refuses_to_resurrect_a_finished_turn` 與 turn_controller 的測試釘住）。
pub async fn fail_in_flight(app: &Arc<App>, run_id: &str, note: &str) -> anyhow::Result<()> {
    let Some((turn, bot)) = in_flight_of(app, run_id).await? else { return Ok(()) };
    super::interruption::close_turn(app, &bot, run_id, &turn, note).await
}

/// [`fail_in_flight`]，寫不進去時記成欠著的收尾（`interruption` 的帳），之後由定時重試、這顆 bot 的下一則 hook／prompt
/// 補上；`Err` 照樣回給呼叫端，讓它不回「收尾做完了」。只給外面已經發生、不能不做的那種（run 結束）。
pub async fn fail_in_flight_or_owe(app: &Arc<App>, run_id: &str, note: &str) -> anyhow::Result<()> {
    let Some((turn, bot)) = in_flight_of(app, run_id).await? else { return Ok(()) };
    super::interruption::interrupted(app, &bot, run_id, &turn, note).await
}

/// 這個 run 在飛的那一筆與它的 bot。讀不到是錯誤，不是「沒有」。
async fn in_flight_of(app: &Arc<App>, run_id: &str) -> anyhow::Result<Option<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT t.id, c.bot_id FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.run_id = ? AND t.status = 'in_flight'",
    )
    .bind(run_id)
    .fetch_optional(&app.db)
    .await?)
}


#[cfg(test)]
mod run_exit_race_tests {
    use super::*;
    use super::super::run_state as rs;
    use crate::testing as tt;

    /// 使用者 stop 一顆 bot：`stop_bot_locked` 自己關 pane，pane-exit 事件的 `mark_run_exited` 讀到的 run 還是
    /// `stopping`；stop 這時寫下 `stopped`，事件那邊接著寫 `exited` 的話就把「使用者要它停」蓋掉了——
    /// autostart 的 bot 會被 incident 探針（看最後一個 run 是不是 `stopped`）報成 `bot_stopped`。
    #[tokio::test]
    async fn a_pane_exit_that_read_the_run_before_the_user_stop_finished_does_not_overwrite_it() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "stopped-on-purpose").await;
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();

        let (app2, run2) = (app.clone(), run.clone());
        super::super::race_point::arm("mark_run_exited_after_read", &run, move || async move {
            // `stop_bot_locked` 的最後一步。
            sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?").bind(db::now()).bind(&run2).execute(&app2.db).await.unwrap();
        });
        mark_run_exited(&app, &run, "pane exited").await;

        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "stopped", "使用者停的就是停的，不是 pane 自己死掉");
    }

    /// 沒有人搶的時候照舊：還活著的 run 碰到 pane-exit 就是 `exited`。
    #[tokio::test]
    async fn a_pane_exit_on_a_live_run_still_ends_it_as_exited() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "died").await;
        let run = tt::fake_run(&app, &bot.id).await;
        mark_run_exited(&app, &run, "pane exited").await;
        let (state, ended): (String, Option<String>) =
            sqlx::query_as("SELECT state, ended_at FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "exited");
        assert!(ended.is_some());
    }

    async fn run_state(app: &Arc<App>, run: &str) -> String {
        db::run(&app.db, run).await.unwrap().unwrap().state
    }

    /// #135：`exited` 寫不進去（SQLite I/O／busy）就還不是 exited——依附這顆 run 的東西一樣都不能先動：
    /// in-flight 不收、排著的不撤、watcher 不拆。DB 恢復後再收一次，收尾照常、只做一次。
    #[tokio::test]
    async fn a_run_exit_that_cannot_be_recorded_leaves_the_run_and_its_work_alone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "db-hiccup").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let in_flight = rs::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot.id, None, "queued").await;
        let watcher = rs::watch_run_pane(&app, &run).await;

        rs::refuse_run_state(&app, "exited").await;
        assert_eq!(mark_run_exited(&app, &run, "pane exited").await, RunExit::NotRecorded, "跟「別的路徑先收了」分得開");
        assert_eq!(run_state(&app, &run).await, "running", "寫不進去就還是 running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight", "run 還沒結束，in-flight 不能先被收成 failed");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "run 還沒結束，排著的不能先被撤");
        assert!(rs::watched(&app, &watcher).await, "watcher 不能先拆");
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::Reconcile { stuck: "running".into() }], "排了對帳重試，不是只留一行 warning");

        rs::accept_run_state(&app, "exited").await;
        assert_eq!(mark_run_exited(&app, &run, "pane exited").await, RunExit::Recorded);
        assert_eq!(run_state(&app, &run).await, "exited");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "failed");
        assert_eq!(rs::turn_status(&app, &queued).await, "failed");
        assert_eq!((rs::system_notes(&app, &in_flight).await, rs::system_notes(&app, &queued).await), (1, 1), "各收一次");
        assert!(!rs::watched(&app, &watcher).await);
    }

    /// #198 同類：run 結束時讀不到它的主機，不拿 `local` 頂替去拆 watcher——本機剛好同名 session、同 pane id 的那顆
    /// bot 的 watcher 會被拆掉，它的狀態事件從此收不到。
    #[tokio::test]
    async fn a_run_exit_whose_host_cannot_be_read_leaves_other_hosts_watchers_alone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let far = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'far', ?)").bind(&far).bind(db::now()).execute(&app.db).await.unwrap();
        let bot = tt::claude_bot(&app, &far, "remote").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let local = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        app.pane_watchers.lock().await.insert(local.clone(), tokio::spawn(std::future::pending::<()>()));

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        mark_run_exited(&app, &run, "pane exited").await;
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
        assert!(rs::watched(&app, &local).await, "本機那顆的 watcher 不是這個 run 的");
    }

    /// #156：`exited` 記下了，in-flight 那一筆卻收不成。pane 已經沒了（這不是我們能不做的事），所以不回
    /// 「收尾做完了」：記成欠著的收尾、之後補上；佇列與 watcher 照樣收（那是 run 結束的事，跟回合收不收得成無關）。
    #[tokio::test]
    async fn a_run_exit_whose_in_flight_turn_cannot_be_closed_owes_it_and_settles_it_later() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "exit-turn-owed").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let in_flight = rs::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot.id, None, "queued").await;
        let watcher = rs::watch_run_pane(&app, &run).await;
        rs::refuse_turn_close(&app, &in_flight).await;

        let exit = mark_run_exited(&app, &run, "pane exited").await;
        assert_ne!(exit, RunExit::Recorded, "回合沒收掉，不能說收尾做完了");
        assert_eq!(exit, RunExit::TurnOwed);
        assert_eq!(run_state(&app, &run).await, "exited");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight", "寫不進去就是還沒收");
        assert_eq!(rs::turn_status(&app, &queued).await, "failed", "run 結束了，排著的照撤");
        assert!(!rs::watched(&app, &watcher).await);

        rs::accept_turn_close(&app).await;
        // 定時重試那一輪（沒有 hook、沒有 prompt）。
        super::super::interruption::settle_locked(&app, &bot.id, super::super::interruption::Evidence::Nothing).await.unwrap();
        assert_eq!(rs::turn_status(&app, &in_flight).await, "failed", "欠著的收尾補上了");
        assert_eq!(rs::system_notes(&app, &in_flight).await, 1);
        let note: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&in_flight).fetch_one(&app.db).await.unwrap();
        assert_eq!(note, "run ended: pane exited");
    }

    /// #135 驗收第三條：CAS 輸給使用者的 stop 時照 #131——不覆寫 `stopped`，也不做第二份收尾（那是 stop 的）。
    #[tokio::test]
    async fn a_pane_exit_that_lost_the_race_to_a_user_stop_leaves_the_cleanup_to_the_stop() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "stopping").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let in_flight = rs::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot.id, None, "queued").await;
        let watcher = rs::watch_run_pane(&app, &run).await;
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let (app2, run2) = (app.clone(), run.clone());
        super::super::race_point::arm("mark_run_exited_after_read", &run, move || async move {
            sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?").bind(db::now()).bind(&run2).execute(&app2.db).await.unwrap();
        });

        assert_eq!(mark_run_exited(&app, &run, "pane exited").await, RunExit::AlreadyEnded, "CAS 輸了");
        assert_eq!(run_state(&app, &run).await, "stopped");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight", "不是這條路的收尾");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued");
        assert!(rs::watched(&app, &watcher).await);
        assert!(rs::scheduled(&run).is_empty(), "CAS 輸了不是寫入失敗，不排重試");
    }
}

#[cfg(test)]
mod flush_queue_tests {
    //! Durable prompt queue claim step. `schedule_flush_queued` is a no-op in tests, so these call
    //! `flush_queued_locked` directly.
    use super::*;
    use crate::testing as tt;

    /// 上限橫幅寫進這顆 bot 身分的 key，且不把 5h `resets_at` 蓋成橫幅時間（2026-09-13）。
    #[tokio::test]
    async fn a_limit_hit_banner_lands_on_the_bots_own_quota_key() {
        let env = tt::env().await;
        let app = env.app.clone();
        let notice = "ERROR: You've hit your usage limit, or try again at 10:15 PM.";
        apply_codex_limit_hit_quota(&app, LOCAL_HOST, "codex:astra", codex_limit_hit(notice, db::now())).await;

        let q = app.quotas.lock().await;
        let mine = q.get("codex:astra").expect("寫進帶身分的那把");
        assert!(mine.limit_hit.is_some());
        assert_eq!(mine.five_hour.as_ref().unwrap().used_pct, 100.0, "量表標成用完");
        assert!(mine.five_hour.as_ref().unwrap().resets_at.is_none(), "橫幅的時間只進 limit_hit.until");
        assert!(mine.limit_hit.as_ref().unwrap().until.is_some());
        assert!(q.get("codex").is_none(), "沒有身分的那把不該被動到");
    }

    /// 同一張橫幅重掃不是新證據，不可把 `at` 蓋成現在（2026-09-13：22:21 掃到 22:15 的舊橫幅）。
    #[tokio::test]
    async fn the_same_banner_seen_again_is_not_new_evidence() {
        let env = tt::env().await;
        let app = env.app.clone();
        let notice = "ERROR: You've hit your usage limit, or try again at 10:15 PM.";
        apply_codex_limit_hit_quota(&app, LOCAL_HOST, "codex", codex_limit_hit(notice, db::now())).await;
        let first = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().unwrap();

        // 中間 app-server 清橫幅是另一條規則；這裡只測重掃。
        apply_codex_limit_hit_quota(&app, LOCAL_HOST, "codex", codex_limit_hit(notice, db::now())).await;
        let again = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().unwrap();
        assert_eq!(first.at, again.at, "同一張橫幅不會把時間戳往前推");
        assert_eq!(first.until, again.until);
    }

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
        turn_id: String,
    }

    /// Running bot with one queued prompt. `session` other than `"test"` makes `client_for_run` fail
    /// (a host that dropped out between queueing and flush).
    async fn queued(session: &str) -> Fixture {
        queued_kind("claude", session).await
    }

    async fn queued_kind(kind: &str, session: &str) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'q',?,'[]',0,1,'tok',?)",
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
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent',?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(session)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending','ping',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        Fixture { env, bot_id, conv, run_id, turn_id }
    }

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
        // 沒有接管：不用等。
        assert_eq!(left(None, false, s(0), now, grace), None);
        // 20 秒前按 Esc、之後一直閒著：還要 40 秒。
        assert_eq!(left(Some(at("2026-09-16T11:59:40Z")), false, s(20), now, grace), Some(s(40)));
        // 按 Esc 之後使用者自己送了一則（規格 3）：不再擋，那一回合結束後照一般規則送。
        assert_eq!(left(Some(at("2026-09-16T11:58:00Z")), true, s(10), now, grace), None);
        // 兩分鐘前按 Esc，但十秒前才又閒下來（中間 working 過，規格 4）：從閒下來算，還要 50 秒。
        assert_eq!(left(Some(at("2026-09-16T11:58:00Z")), false, s(10), now, grace), Some(s(50)));
        // idle 計時比中斷還早（Esc 在閒著的時候按）：從中斷算。
        assert_eq!(left(Some(at("2026-09-16T11:59:40Z")), false, s(600), now, grace), Some(s(40)));
        // 連續閒置滿寬限：送。
        assert_eq!(left(Some(at("2026-09-16T11:58:00Z")), false, s(120), now, grace), None);
        // 接管最久 30 分鐘，之後回到一般排隊（不讓一次 Esc 永遠壓著）。
        assert_eq!(left(Some(at("2026-09-16T11:29:00Z")), false, s(1), now, grace), None);
    }

    /// 真的走 flush：剛 interrupt 的 bot 排著的派工不動（不 claim、不算重試、掛好 timer）；
    /// 寬限過了就照常往下送。
    #[tokio::test]
    async fn a_queued_dispatch_waits_out_the_grace_after_the_user_interrupts() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        note_user_interrupt(&f.bot_id);
        forget_queue_retry_timer(&f.bot_id);

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued");
        assert_eq!((t.flush_retries, t.next_flush_at.clone()), (0, None), "寬限內連試都不試");
        assert!(queue_retry_timer_armed(&f.bot_id), "閒著的 bot 沒有邊會叫醒它，要掛 timer");

        // 寬限過了：標記往回撥到 2 分鐘前，flush 就照常往下走（這裡沒有 herdr，會被放回佇列並算一次重試）。
        // （規格 4：看連續 idle，所以 idle 計時也要撥回兩分鐘前。）
        super::super::interrupt_grace::note_user_interrupt_at(&f.bot_id, chrono::Utc::now() - chrono::Duration::seconds(120));
        super::super::stuck_turns::observe_at(&f.run_id, "working", ago(121));
        super::super::stuck_turns::observe_at(&f.run_id, "idle", ago(120));
        forget_queue_retry_timer(&f.bot_id);
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert!(t.flush_retries > 0 || t.status != "queued", "寬限過了要真的去送：{} retries={}", t.status, t.flush_retries);
        assert_eq!(super::super::interrupt_grace::hold_of(&f.bot_id), None, "等完就收掉標記");
    }

    /// issue #86：維護窗口握著的時候，排隊的 prompt **留在佇列**，不進 pane。擋住它的是我們自己開的
    /// 窗口、不是 bot 的狀態，所以不算一次重試；窗口到期為止掛一個 timer，窗口提早關掉時
    /// `drain_queue` 會叫醒 flush，這個 timer 只是底線。
    #[tokio::test]
    async fn a_queued_prompt_waits_out_a_maintenance_window_instead_of_going_into_the_pane() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        crate::supervisor::store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, false, None, &json!({}))
            .await
            .unwrap()
            .unwrap();
        forget_queue_retry_timer(&f.bot_id);

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 0), "留在佇列，而且不花它的重試額度");
        assert_eq!(t.run_id, None, "沒有被 claim");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_text"), "一個字都沒打");
        assert!(queue_retry_timer_armed(&f.bot_id), "掛了 timer，窗口關掉之後有人會回來送");

        // 窗口過期（沒有人 release）就自動恢復：閘門看 `held_at`，不會鎖死。
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='restart'").bind(&past).execute(&app.db).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert!(t.status != "queued" || t.flush_retries > 0, "窗口過期就照常往下送：{} retries={}", t.status, t.flush_retries);
    }

    /// issue #127：窗口握著、但讀不到它——排隊的這一筆要**留在佇列**，不 claim、不花重試、一個字都不送，
    /// 掛 timer 等下一輪重新判斷。跟窗口明確握著是同一個結果；讀取恢復、窗口過期之後才往下送。
    #[tokio::test]
    async fn a_queued_prompt_stays_queued_while_the_restart_lease_cannot_be_read() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        crate::supervisor::store::acquire_lease(&app.db, "restart", "k8bw2f", None, None, &until, false, None, &json!({}))
            .await
            .unwrap()
            .unwrap();
        crate::supervisor::maintenance::fault::break_lease_reads(&app.db).await;
        forget_queue_retry_timer(&f.bot_id);

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 0), "留在佇列，而且不花它的重試額度");
        assert_eq!(t.run_id, None, "沒有被 claim");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒送");
        assert!(queue_retry_timer_armed(&f.bot_id), "掛了 timer，下一輪重新判斷；不然這筆會一直躺著");

        // 讀取恢復、窗口過期（沒有人 release）：照常往下送，不會永久卡住。
        crate::supervisor::maintenance::fault::restore_lease_reads(&app.db).await;
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='restart'").bind(&past).execute(&app.db).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert!(t.status != "queued" || t.flush_retries > 0, "恢復後照常往下送：{} retries={}", t.status, t.flush_retries);
    }

    /// 中斷寬限的 timer 比 `next_flush_at` 早燒：flush 看到退避還沒到就早退，這一刻起沒有任何 timer。
    /// 早退時一定要補一個到期才燒的 timer，否則閒著的 bot 再也不會有人來送，排隊的派工要等 30 分鐘
    /// 保險絲才被撤掉（review3 L1）。
    #[tokio::test]
    async fn an_early_wake_up_leaves_a_timer_for_when_the_backoff_is_actually_up() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        // 這一筆已經被放回來過，下一次嘗試在 5 秒後。
        let next = (chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET next_flush_at = ?, flush_retries = 1 WHERE id = ?")
            .bind(&next)
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        // 寬限的 timer 剛剛燒掉並把自己從表上收走：現在這顆 bot 一個 timer 都沒有。
        forget_queue_retry_timer(&f.bot_id);

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 1), "退避還沒到：不動它、不花重試");
        assert!(queue_retry_timer_armed(&f.bot_id), "早退也要留下一個 timer，否則沒有人會再來送");
        let left = queue_retry_timer_left(&f.bot_id).expect("timer 掛著");
        assert!(left <= std::time::Duration::from_secs(5), "掛的是剩下的退避時間，不是整輪重來：{left:?}");
    }

    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?")
            .bind(id)
            .fetch_one(&app.db)
            .await
            .unwrap()
    }

    /// 交辦掛上這筆 queued turn（AGM 派工遇到回合中就是這個形狀）。
    async fn queued_assignment(f: &Fixture) -> String {
        crate::supervisor::store::get_or_init(&f.env.app.db).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&f.env.app.db, None, &f.bot_id, "crid-fence", "請釋放 fence 21", &[], None, true)
            .await
            .unwrap();
        crate::supervisor::store::mark_delivered(&f.env.app.db, &a.id, &f.turn_id, "queued").await.unwrap();
        a.id
    }

    async fn decide(app: &Arc<App>, id: &str, decision: &str) -> Value {
        let input: crate::supervisor::api::ReviewIn =
            serde_json::from_value(json!({"decision": decision, "reason": "改主意了"})).unwrap();
        crate::supervisor::api::post_review(axum::extract::State(app.clone()), axum::extract::Path(id.to_string()), axum::http::HeaderMap::new(), axum::Json(input))
            .await
            .map(|j| j.0)
            .unwrap()
    }

    /// cancel 當下就撤掉它排著的 queued turn：終態、寫明理由、名額立刻釋放，之後的 flush 也不會送出去。
    #[tokio::test]
    async fn cancelling_an_assignment_revokes_its_queued_turn_and_frees_the_slot() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        let a = queued_assignment(&f).await;

        let out = decide(&app, &a, "cancel").await;
        assert_eq!(out["status"], "cancelled");
        assert_eq!(out["revoked_turn_id"], json!(f.turn_id));
        assert!(out.get("warning").is_none() && out.get("may_still_be_running").is_none(), "撤掉的是還沒送出的那則，不能再說它還在跑：{out}");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "終態，不佔名額");
        let why: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(why.contains(&a) && why.contains("已取消") && why.contains("改主意了"), "{why}");

        // 名額釋放：同一個對話馬上排得進下一筆。
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','下一則',?)")
            .bind(db::ulid())
            .bind(&f.conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .expect("queued 名額已經釋放");
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let typed = f.env.herdr.pane("pane-1").unwrap().transcript;
        assert!(!typed.iter().any(|l| l.contains("fence 21")), "已取消的那則一個字都沒送：{typed:?}");
        assert!(typed.iter().any(|l| l.contains("下一則")), "排在後面的照常送：{typed:?}");
    }

    /// #200：cancel 的那一刻撤不掉排著的那一則——讀不到交辦（以前舊介面把讀取錯誤吞成「沒有要撤的」），或撤銷那一句寫不進去
    /// （以前只記一行）——回應看起來都跟「沒有要撤的」一樣。現在回應講明撤銷待補（`revoke_pending_turn_id`）、叫醒 flush；
    /// DB 好了 flush 把它撤掉，一個字都不送。
    #[tokio::test]
    async fn a_cancel_that_cannot_revoke_the_queued_prompt_right_now_says_it_is_pending() {
        for why in ["讀不到交辦", "撤銷寫不進去"] {
            let f = queued("test").await;
            let app = f.env.app.clone();
            f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
            sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
            let a = queued_assignment(&f).await;
            forget_queue_retry_timer(&f.bot_id);
            if why == "讀不到交辦" {
                let app2 = app.clone();
                super::super::race_point::arm("review_before_revoke", &a, move || async move { break_assignment_reads(&app2).await });
            } else {
                sqlx::query("CREATE TRIGGER lost_revoke BEFORE UPDATE OF status ON turns WHEN NEW.status = 'failed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
                    .execute(&app.db)
                    .await
                    .unwrap();
            }

            let out = decide(&app, &a, "cancel").await;
            if why == "讀不到交辦" {
                restore_assignment_reads(&app).await;
            } else {
                sqlx::query("DROP TRIGGER lost_revoke").execute(&app.db).await.unwrap();
            }
            assert_eq!(out["status"], "cancelled", "{why}");
            assert!(out.get("revoked_turn_id").is_none(), "{why}：沒撤成：{out}");
            assert_eq!(out["revoke_pending_turn_id"], json!(f.turn_id), "{why}：講明撤銷待補：{out}");
            assert!(queue_retry_timer_armed(&f.bot_id), "{why}：叫醒 flush 再撤");
            assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "{why}");

            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "{why}：flush 撤掉");
            assert_eq!(typed(&f, "fence 21"), 0, "{why}：一個字都沒送");
        }
    }

    /// 已經 in_flight（送出去了）的撤不回來：cancel 不動 turn，也不說撤掉了。
    #[tokio::test]
    async fn an_assignment_whose_prompt_already_went_out_is_not_pretended_away() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let a = queued_assignment(&f).await;
        sqlx::query("UPDATE turns SET status = 'in_flight', delivery = 'ok', run_id = ? WHERE id = ?")
            .bind(&f.run_id)
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        let out = decide(&app, &a, "cancel").await;
        assert_eq!(out["status"], "cancelled");
        assert!(out.get("revoked_turn_id").is_none(), "{out}");
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    }

    /// 繞過 API 改成 superseded／failed（或決定 commit 之後還沒撤就重啟）：flush 送出前也會撤掉，不送。
    #[tokio::test]
    async fn the_flush_never_sends_a_prompt_whose_assignment_was_withdrawn() {
        for status in ["superseded", "failed", "cancelled", "blocked"] {
            let f = queued("test").await;
            let app = f.env.app.clone();
            f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
            let a = queued_assignment(&f).await;
            sqlx::query("UPDATE supervisor_assignments SET status = ? WHERE id = ?").bind(status).bind(&a).execute(&app.db).await.unwrap();
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "{status}");
            assert!(f.env.herdr.pane("pane-1").map_or(true, |p| p.transcript.is_empty()), "{status}：一個字都沒打");
        }
        // 對照：交辦還在（delivered）的照常送。
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        queued_assignment(&f).await;
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    }

    /// 之後每一句讀 `supervisor_assignments` 的都失敗（`no such table`）——DB 出錯的樣子（同 `maintenance::fault`）。
    async fn break_assignment_reads(app: &Arc<App>) {
        sqlx::query("ALTER TABLE supervisor_assignments RENAME TO supervisor_assignments_unreadable").execute(&app.db).await.unwrap();
    }

    async fn restore_assignment_reads(app: &Arc<App>) {
        sqlx::query("ALTER TABLE supervisor_assignments_unreadable RENAME TO supervisor_assignments").execute(&app.db).await.unwrap();
    }

    /// #159 驗收一、二、五：交辦已經不要了（cancelled／superseded／failed／blocked／quota_blocked），flush 卻讀不到它——
    /// 以前 `.ok()??` 把讀取錯誤當成「沒有撤銷理由」，照樣認領、打進 pane，被取消的工作真的開始跑。讀不到不等於還要：
    /// 留在佇列、不認領、不花重試、一個字都不送，掛短 timer；讀得到之後照它的狀態撤掉，永遠不送。
    #[tokio::test]
    async fn a_queued_prompt_whose_assignment_cannot_be_read_is_never_sent() {
        for status in WITHDRAWN_ASSIGNMENT {
            let f = queued("test").await;
            let app = f.env.app.clone();
            f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
            sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
            let a = queued_assignment(&f).await;
            sqlx::query("UPDATE supervisor_assignments SET status = ? WHERE id = ?").bind(status).bind(&a).execute(&app.db).await.unwrap();
            forget_queue_retry_timer(&f.bot_id);
            break_assignment_reads(&app).await;

            assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "{status}：讀不到，不回普通的成功");
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.run_id.as_deref(), t.flush_retries), ("queued", None, 0), "{status}：留在佇列、不認領、不花重試");
            assert_eq!(typed(&f, "fence 21"), 0, "{status}：一個字都沒送");
            assert!(queue_retry_timer_armed(&f.bot_id), "{status}：掛了 timer，稍後重新判斷");

            restore_assignment_reads(&app).await;
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "{status}：讀得到了就撤");
            assert_eq!(typed(&f, "fence 21"), 0, "{status}：永遠不送");
        }
    }

    /// #159：交辦已經取消、要撤排著的那一則時寫不進去——以前只記一行 error、回 Ok、不掛 timer：沒撤成卻當撤完了，那一筆
    /// 佔著佇列，直到下一次剛好有人叫醒 flush。現在留在佇列、回錯誤、掛短 timer；DB 好了就撤，一個字都不送。
    #[tokio::test]
    async fn a_withdrawn_prompt_that_cannot_be_revoked_stays_queued_and_is_retried() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        let a = queued_assignment(&f).await;
        sqlx::query("UPDATE supervisor_assignments SET status = 'cancelled' WHERE id = ?").bind(&a).execute(&app.db).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        sqlx::query("CREATE TRIGGER lost_revoke BEFORE UPDATE OF status ON turns WHEN NEW.status = 'failed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "撤不掉：不回普通的成功");
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        assert!(queue_retry_timer_armed(&f.bot_id), "掛了 timer，稍後再撤");
        assert_eq!(typed(&f, "fence 21"), 0);

        sqlx::query("DROP TRIGGER lost_revoke").execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        assert_eq!(typed(&f, "fence 21"), 0, "永遠不送");
    }

    /// #159（同一類）：flush 讀完交辦（還要）、還沒認領的那一瞬，交辦被取消（review API 不拿 bot 鎖）。認領那一句本身帶上
    /// 「掛的交辦此刻還要」：已經取消的那一則不被領走、不打進 pane；下一輪照它的狀態撤掉。
    #[tokio::test]
    async fn an_assignment_cancelled_between_the_check_and_the_claim_is_not_claimed() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        let a = queued_assignment(&f).await;
        let (app2, a2) = (app.clone(), a.clone());
        super::super::race_point::arm("flush_before_claim", &f.bot_id, move || async move {
            sqlx::query("UPDATE supervisor_assignments SET status = 'cancelled' WHERE id = ?").bind(&a2).execute(&app2.db).await.unwrap();
        });

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref()), ("queued", None), "取消了就不領走");
        assert_eq!(typed(&f, "fence 21"), 0, "一個字都沒打");

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "下一輪撤掉");
        assert_eq!(typed(&f, "fence 21"), 0);
    }

    /// run 結束（pane 不見、agent 退出）：它排著的 queued 收掉、名額釋放，交辦照一般流程變成失敗讓 AGM 看得到。
    #[tokio::test]
    async fn a_run_that_ends_takes_its_queued_prompt_with_it() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let a = queued_assignment(&f).await;
        let mut events = app.subscribe_turns();
        mark_run_exited(&app, &f.run_id, "pane exited").await;
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let why: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'")
            .bind(&f.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(why.contains("pane exited") && why.contains("沒有在跑"), "{why}");
        let ev = events.try_recv().expect("撤銷有推 turn 事件，交辦才會結案");
        assert_eq!((ev.turn_id.as_str(), ev.status.as_str()), (f.turn_id.as_str(), "failed"));
        crate::supervisor::controller::reconcile(&app).await;
        let row = crate::supervisor::store::assignment(&app.db, &a).await.unwrap().unwrap();
        assert_ne!(row.status, "delivered", "不留在 delivered 假裝還在路上");
    }

    /// bot 被停掉：同樣收掉。
    #[tokio::test]
    async fn stopping_a_bot_revokes_its_queued_prompt() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        stop_bot(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        assert!(db::active_run(&app.db, &f.bot_id).await.unwrap().is_none());
    }

    /// 還有活著的 run 就不動（flush 會送）；定時掃描收掉 run 早就不在的遺留（含上線前留下來的）。
    #[tokio::test]
    async fn only_a_queued_prompt_with_no_live_run_is_an_orphan() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        assert!(revoke_orphaned_queued_turns(&app, &f.bot_id, "測試").await.is_empty(), "run 還在");
        assert!(revoke_all_orphaned_queued_turns(&app).await.is_empty());
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");

        // 模擬舊版本：run 已經結束，但當時沒有撤銷。
        sqlx::query("UPDATE runs SET state = 'exited', ended_at = ? WHERE id = ?").bind(db::now()).bind(&f.run_id).execute(&app.db).await.unwrap();
        assert_eq!(revoke_all_orphaned_queued_turns(&app).await, vec![f.turn_id.clone()]);
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        assert!(revoke_all_orphaned_queued_turns(&app).await.is_empty(), "撤過的不再撤");
    }

    /// 在等額度的交辦：排著的那則不送（額度回來會用新的一則重送，送了就是做兩次）。
    #[tokio::test]
    async fn a_prompt_whose_assignment_waits_for_quota_is_not_sent() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        let a = queued_assignment(&f).await;
        sqlx::query("UPDATE supervisor_assignments SET status = 'quota_blocked' WHERE id = ?").bind(&a).execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        assert!(f.env.herdr.pane("pane-1").map_or(true, |p| p.transcript.is_empty()));
    }

    fn ago(secs: u64) -> std::time::Instant {
        std::time::Instant::now().checked_sub(std::time::Duration::from_secs(secs)).unwrap_or_else(std::time::Instant::now)
    }

    fn typed(f: &Fixture, text: &str) -> usize {
        f.env.herdr.pane("pane-1").map_or(0, |p| p.transcript.iter().filter(|l| l.contains(text)).count())
    }

    /// 使用者剛按 Esc：排著的派工不立刻打進去，timer 掛好等寬限。
    #[tokio::test]
    async fn after_the_user_interrupts_the_queued_prompt_waits_for_the_grace() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        note_user_interrupt(&f.bot_id);
        super::super::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "寬限內不送");
        assert_eq!(typed(&f, "AGM 的派工"), 0, "一個字都沒打");
        assert!(queue_retry_timer_armed(&f.bot_id), "寬限到了自己再來一次");
    }

    /// 連續 idle 滿寬限、期間沒有使用者新輸入才送；中斷很久但剛剛才又閒下來（有動靜）還是要等。
    #[tokio::test]
    async fn once_the_bot_has_been_quiet_for_the_whole_grace_the_queued_prompt_goes_out() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        super::super::interrupt_grace::note_user_interrupt_at(&f.bot_id, chrono::Utc::now() - chrono::Duration::seconds(300));

        // 中斷是五分鐘前，但 bot 十秒前才又閒下來（中間 working 過）：從閒下來那刻算，不送。
        super::super::stuck_turns::observe_at(&f.run_id, "working", ago(11));
        super::super::stuck_turns::observe_at(&f.run_id, "idle", ago(10));
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");

        // 連續閒了超過 60 秒：送。
        super::super::stuck_turns::observe_at(&f.run_id, "working", ago(62));
        super::super::stuck_turns::observe_at(&f.run_id, "idle", ago(61));
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
        assert_eq!(typed(&f, "AGM 的派工"), 1);
    }

    /// 寬限內使用者自己送了一則：在跑的時候派工排在後面；使用者那一回合結束後照一般規則馬上送，不再等寬限。
    #[tokio::test]
    async fn a_prompt_the_user_sends_inside_the_grace_goes_first() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        super::super::interrupt_grace::note_user_interrupt_at(&f.bot_id, chrono::Utc::now() - chrono::Duration::seconds(5));
        super::super::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());

        let mine = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&mine).bind(&f.conv).bind(&f.run_id).bind(db::now()).execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "使用者那則在跑，派工排在後面");

        sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?").bind(&mine).execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "使用者那一回合結束：照一般規則，不等寬限");
    }

    /// 一般回合結束（沒有中斷）照舊立刻送。
    #[tokio::test]
    async fn an_ordinary_turn_end_still_flushes_at_once() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        super::super::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    }

    /// 使用者直接在 pane 裡按 Esc（daemon 沒有事件）：從 transcript 認出來，一樣先等。
    #[tokio::test]
    async fn an_escape_pressed_in_the_pane_itself_is_read_from_the_transcript() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        let path = f.env.dir.join("t.jsonl");
        let write = |at: chrono::DateTime<chrono::Utc>| {
            let marker = json!({"type": "user", "timestamp": at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "interruptedMessageId": "m",
                                "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}});
            let prompt = json!({"type": "user", "message": {"role": "user", "content": "長回合"}});
            std::fs::write(&path, format!("{prompt}\n{marker}\n")).unwrap();
        };
        sqlx::query("UPDATE runs SET transcript_path = ? WHERE id = ?").bind(path.to_string_lossy()).bind(&f.run_id).execute(&app.db).await.unwrap();

        write(chrono::Utc::now());
        super::super::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "transcript 說剛被中斷");

        write(chrono::Utc::now() - chrono::Duration::seconds(120));
        super::super::stuck_turns::observe_at(&f.run_id, "working", ago(121));
        super::super::stuck_turns::observe_at(&f.run_id, "idle", ago(120));
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "兩分鐘前中斷、之後一直閒著：送");
    }

    /// 寬限內 AGM 直接派新的一件（沒有 in_flight，本來會直接打字）：一樣排進佇列等寬限。
    #[tokio::test]
    async fn a_dispatch_arriving_inside_the_grace_is_queued_instead_of_typed() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("DELETE FROM turns WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        note_user_interrupt(&f.bot_id);
        super::super::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
        let out = prompt_relayed_queueable(&app, &f.bot_id, "新的派工", "crid-grace", None).await.unwrap();
        assert_eq!(out.delivery, "queued");
        assert_eq!(typed(&f, "新的派工"), 0);
        assert!(queue_retry_timer_armed(&f.bot_id));
        // AGM 自己排進去的那筆不是「使用者新輸入」：下一次 flush 照樣等寬限。
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(typed(&f, "新的派工"), 0);
    }

    /// 網頁按 Esc（interrupt_bot）端到端：收掉回合觸發的 flush 不會把排著的派工打進去。
    #[tokio::test]
    async fn the_web_escape_button_holds_the_queue_behind_it() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        f.env.herdr.set_agent("agent", "pane-1", true);
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid()).bind(&f.conv).bind(&f.run_id).bind(db::now()).execute(&app.db).await.unwrap();
        super::super::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());

        interrupt_bot(&app, &f.bot_id).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        assert_eq!(typed(&f, "AGM 的派工"), 0);
    }

    /// 強制中止（abort_turns）也是使用者要接手：不撤排著的派工（裁示不變），只是先等寬限。
    #[tokio::test]
    async fn the_abort_button_holds_the_queue_behind_it_without_revoking_it() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        f.env.herdr.set_agent("agent", "pane-1", true);
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        super::super::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());

        abort_turns(&app, &f.bot_id).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "不撤，也不立刻送");
        assert_eq!(typed(&f, "AGM 的派工"), 0);
    }

    /// 框裡有字時排隊的 prompt 一個字都不打、放回隊列；框清空後再 flush 就送出去（第七輪 #2）。
    #[tokio::test]
    async fn a_queued_prompt_waits_for_a_busy_box_and_goes_out_once_it_clears() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = 'Reply with PONG please' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane(
            "pane-1",
            crate::testing::LivePane { composer: vec!["我自己在打的草稿".into()], width: Some(120), ..Default::default() },
        );

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued", "沒送出就回隊列，不是 in-flight unknown");
        let writes = |f: &Fixture| f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text" || *m == "pane.send_keys").count();
        assert_eq!(writes(&f), 0);

        // 使用者把草稿清掉了；在退避時間到之前的喚醒不會搶先送。
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "still inside its backoff");
        // 退避時間到了（重試 timer 觸發）。
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"));
        let pane = f.env.herdr.pane("pane-1").unwrap();
        assert_eq!(pane.transcript.iter().filter(|l| l.contains("Reply with PONG please")).count(), 1);
    }

    /// 排隊中的 grok 多行 prompt：沒有無損證據也照樣送出，turn 標成 unverified（delivery ok＋delivery_verified 0）。
    #[tokio::test]
    async fn a_queued_prompt_without_lossless_proof_goes_out_marked_unverified() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = '第一行\n第二行' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str(), t.delivery_verified), ("in_flight", "ok", 0));
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1);
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
        // 中斷寬限掛的 60 秒 timer，接著排隊的派工被放回來，只要等 15 秒。
        assert!(arm_queue_retry(&bot, std::time::Duration::from_secs(60), mark(&fired, 60)));
        assert!(arm_queue_retry(&bot, std::time::Duration::from_secs(15), mark(&fired, 15)), "更早的換掉舊的");
        assert_eq!(queue_retry_timer_left(&bot), Some(std::time::Duration::from_secs(15)), "掛著的是新的那個");

        tokio::time::sleep(std::time::Duration::from_secs(16)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fired.lock().unwrap().as_slice(), &[15], "換上去的那個照自己的時間燒");
        assert!(!queue_retry_timer_armed(&bot), "燒完就把自己收掉");

        // 被換掉的那個還睡著：醒來發現 generation 不是自己的，什麼都不做。
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fired.lock().unwrap().as_slice(), &[15], "被換掉的舊 timer 不會再燒一次");
        assert!(arm_queue_retry(&bot, std::time::Duration::from_secs(30), mark(&fired, 30)), "之後照樣可以再掛");
    }

    /// 框永遠有字：每次放回都記次數與下次時間，額度用完就明確失敗並留說明（同一個 transaction）。
    #[tokio::test]
    async fn a_box_that_never_empties_ends_in_a_failed_turn_with_an_explanation() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        f.env.herdr.live_pane(
            "pane-1",
            crate::testing::LivePane { width: Some(120), boxed: true, composer: vec!["草稿".into()], ..Default::default() },
        );
        for n in 1..=QUEUE_RETRY_LIMIT {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            let t = turn(&app, &f.turn_id).await;
            assert_eq!(t.status, "queued", "put back #{n}");
            assert_eq!(t.flush_retries, n);
            assert!(t.next_flush_at.is_some());
            // 另一次喚醒在 next_flush_at 之前：不動它、不花額度。
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.flush_retries, n, "early wake-ups do not spend retries");
            sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        }
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let hints: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'")
            .bind(&f.turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("已停止自動重試"));
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0, "從頭到尾沒打過字");
    }

    /// codex 已知 session、rollout 還沒寫出來：先放回隊列等，等滿 CODEX_LOG_WAIT_RETRIES 次才退回 unverified 送出。
    #[tokio::test]
    async fn a_queued_codex_prompt_waits_for_its_rollout_then_falls_back() {
        let f = queued_kind("codex", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let home = f.env.dir.join("codex-home-q");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?")
            .bind(json!({"CODEX_HOME": home.to_str().unwrap()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-q' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = '第一行\n第二行' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), codex: true, ..Default::default() });

        for n in 1..=CODEX_LOG_WAIT_RETRIES {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.flush_retries), ("queued", n), "waiting for the rollout #{n}");
            sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        }
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0, "等的時候一個字都沒打");
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str(), t.delivery_verified), ("in_flight", "ok", 0), "等過了就照樣送、標 unverified");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1);
    }

    /// 退避中重啟：記憶體裡的 timer 沒了，啟動時依 next_flush_at 重建每 bot 唯一的 timer，到期恰好送一次（sol 第九輪 #3）。
    ///
    /// 兩段都不跟真實時間賽跑（#203）。以前退避只剩 150 ms：負載一高，兩次重建之間 timer 就先燒掉、把自己從表上拿掉，
    /// 第二次重建又掛一個，「不疊 timer」被判成錯的（偶發紅）。先把到期設在一小時後，驗「每顆 bot 一個、照 next_flush_at 掛」；
    /// 再把到期改成現在、重建一次（較早的取代較晚的），驗到期送出、只送一次。
    #[tokio::test]
    async fn a_backoff_survives_a_restart_and_the_prompt_goes_out_exactly_once() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        f.env.herdr.live_pane(
            "pane-1",
            crate::testing::LivePane { width: Some(120), boxed: true, composer: vec!["草稿".into()], ..Default::default() },
        );
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        // 「重啟」：舊行程的 timer 全沒了；框也清空了。
        forget_queue_retry_timer(&f.bot_id);
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        let due_in = |d: chrono::Duration| (chrono::Utc::now() + d).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET next_flush_at = ? WHERE id = ?")
            .bind(due_in(chrono::Duration::hours(1)))
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();

        let fire = {
            let app = app.clone();
            move |bot: String| {
                let app = app.clone();
                tokio::spawn(async move {
                    let lock = app.bot_lock(&bot).await;
                    let _g = lock.lock().await;
                    flush_queued_locked(&app, &bot).await.unwrap();
                });
            }
        };
        assert_eq!(rearm_queue_retries_with(&app, fire.clone()).await.unwrap(), 1, "one timer for the bot");
        assert_eq!(rearm_queue_retries_with(&app, fire.clone()).await.unwrap(), 0, "a second pass does not add another");
        let left = queue_retry_timer_left(&f.bot_id).expect("armed");
        assert!(left > std::time::Duration::from_secs(55 * 60), "armed for next_flush_at, not sooner: {left:?}");
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "not before it is due");

        // 到期了：到期時間改成現在、再重建一次——較早的取代還在睡的那一個（它醒來時世代對不上，什麼都不做）。
        sqlx::query("UPDATE turns SET next_flush_at = ? WHERE id = ?")
            .bind(due_in(chrono::Duration::zero()))
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        assert_eq!(rearm_queue_retries_with(&app, fire).await.unwrap(), 1, "the due one replaces the later timer");

        // The typed delivery itself takes about two seconds.
        let _ = crate::testing::eventually!({
            let t = turn(&app, &f.turn_id).await;
            t.delivery != "pending" && t.status != "queued"
        });
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"));
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1, "exactly once");
    }

    /// 排隊送：herdr 拒絕 `format: ansi` 照樣送出；讀不到畫面就放回隊列（不是 delivery=unknown 卡住後面的訊息）。
    #[tokio::test]
    async fn queued_prompts_survive_a_herdr_without_styled_reads_and_unreadable_panes() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = 'Reply with PONG please' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        f.env.herdr.reject_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"));

        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = 'Reply with PONG please' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        // 讀畫面失敗（兩種讀法都失敗）。
        f.env.herdr.live.lock().unwrap().remove("pane-1");
        f.env.herdr.set_screen("pane-1", "__READ_ERROR__");
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("queued", "pending"), "put back, not parked as unknown");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }

    /// 證據檔在打字前讀不到：排隊中的 prompt 放回隊列，不停在 unknown，pane 零寫入（sol 第十一輪）。
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_evidence_file_puts_the_queued_prompt_back() {
        use std::os::unix::fs::PermissionsExt;
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        let t = f.env.dir.join("locked-q.jsonl");
        std::fs::write(&t, "").unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 's-q', transcript_path = ? WHERE id = ?")
            .bind(t.to_str().unwrap())
            .bind(&f.run_id)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE turns SET prompt_text = '第一行\n第二行' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o000)).unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
        let tr = turn(&app, &f.turn_id).await;
        assert_eq!((tr.status.as_str(), tr.delivery.as_str()), ("queued", "pending"));
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }

    fn codex_queue_env<'a>(f: &'a Fixture) -> impl std::future::Future<Output = ()> + 'a {
        async move {
            let app = &f.env.app;
            db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
            let home = f.env.dir.join("codex-home-w");
            std::fs::create_dir_all(home.join("sessions")).unwrap();
            sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?")
                .bind(json!({"CODEX_HOME": home.to_str().unwrap()}).to_string())
                .bind(&f.bot_id)
                .execute(&app.db)
                .await
                .unwrap();
            sqlx::query("UPDATE turns SET prompt_text = '第一行\n第二行' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        }
    }

    async fn clear_backoff(f: &Fixture) {
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(&f.env.app.db).await.unwrap();
    }

    /// 先因為框忙放回好幾次，之後 session 已知但 rollout 未寫：rollout 的等待額度不被框忙吃掉，還是要等滿 3 次（sol 第十輪 #2）。
    #[tokio::test]
    async fn busy_put_backs_do_not_spend_the_rollout_wait() {
        let f = queued_kind("codex", "test").await;
        codex_queue_env(&f).await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), codex: true, composer: vec!["草稿".into()], ..Default::default() });
        for _ in 0..3 {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            clear_backoff(&f).await;
        }
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.flush_retries, t.rollout_waits), (3, 0), "busy put-backs are not rollout waits");

        // 框清空了；codex 回報了 session，但 rollout 還沒寫。
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), codex: true, ..Default::default() });
        sqlx::query("UPDATE runs SET native_session_id = 'sess-busy' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        for n in 1..=CODEX_LOG_WAIT_RETRIES {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.rollout_waits), ("queued", n), "still waiting for the rollout #{n}");
            clear_backoff(&f).await;
        }
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "waited its three, then went out");
    }

    /// 等到一半 bot 重啟／換了 session：等待次數跟著新的 run／session 重新算。
    #[tokio::test]
    async fn a_new_run_or_session_starts_the_rollout_wait_over() {
        let f = queued_kind("codex", "test").await;
        codex_queue_env(&f).await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), codex: true, ..Default::default() });
        sqlx::query("UPDATE runs SET native_session_id = 'sess-a' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        for _ in 0..2 {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            clear_backoff(&f).await;
        }
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.rollout_waits, 2);
        assert_eq!(t.rollout_wait_key.as_deref(), Some(format!("{}:sess-a", f.run_id).as_str()));

        // 新 session：前一個 session 等過的兩次不算數。
        sqlx::query("UPDATE runs SET native_session_id = 'sess-b' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.rollout_waits), ("queued", 1), "counted again from one");
        assert_eq!(t.rollout_wait_key.as_deref(), Some(format!("{}:sess-b", f.run_id).as_str()));
        clear_backoff(&f).await;
        // 已經等滿三次也不能讓換了 session 的 turn 直接送：key 不同就不算等過。
        sqlx::query("UPDATE turns SET rollout_waits = 9 WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-c' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }

    /// AGM 派工排進來的 queued turn 沒有 `next_flush_at`（靠回合結束的事件送）。daemon 重啟後那個事件不會再來，
    /// 所以重啟掃描也要收它，不然就是永遠不會送的孤兒（AGM 2026-09-16）。
    #[tokio::test]
    async fn a_queued_turn_without_a_backoff_is_still_rearmed_after_a_restart() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET status='queued', run_id=NULL, next_flush_at=NULL, prompt_text='AGM 派的工作' WHERE id=?")
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        forget_queue_retry_timer(&f.bot_id);
        let fired = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = fired.clone();
        let armed = rearm_queue_retries_with(&app, move |bot: String| seen.lock().unwrap().push(bot)).await.unwrap();
        assert_eq!(armed, 1, "沒有 next_flush_at 的 queued turn 也要重新掛上");
        let _ = crate::testing::eventually!(!fired.lock().unwrap().is_empty());
        assert_eq!(fired.lock().unwrap().as_slice(), &[f.bot_id.clone()], "立刻補一次 flush");
    }

    /// 真的「重啟」：同一個 DB 開一個新的 App，走啟動時真正呼叫的 `reconcile::rearm_progress`，timer 被接回來。
    #[tokio::test]
    async fn the_startup_path_rearms_a_backoff_on_a_fresh_app() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        f.env.herdr.live_pane(
            "pane-1",
            crate::testing::LivePane { width: Some(120), boxed: true, composer: vec!["草稿".into()], ..Default::default() },
        );
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert!(queue_retry_timer_armed(&f.bot_id), "the first process armed it");

        // 程序結束：記憶體裡的 timer 沒了。新的 App 開在同一個資料庫上。
        forget_queue_retry_timer(&f.bot_id);
        drop(app);
        let fresh = crate::testing::restart_app(&f.env).await;
        assert!(!queue_retry_timer_armed(&f.bot_id));
        crate::reconcile::rearm_progress(&fresh).await;
        assert!(queue_retry_timer_armed(&f.bot_id), "startup re-armed it from next_flush_at");
        let again = rearm_queue_retries(&fresh).await.unwrap();
        assert_eq!(again, 0, "a second startup pass does not add another timer");
    }

    /// Regression: a client lookup failing after the claim abandoned the turn `in_flight` +
    /// `delivery='pending'` (nothing finishes that), 409-ing every later prompt. It must be requeued.
    #[tokio::test]
    async fn a_claim_that_cannot_be_delivered_goes_back_on_the_queue() {
        let f = queued("no-such-session").await;
        let app = f.env.app.clone();

        flush_queued_locked(&app, &f.bot_id).await.expect("the flush itself does not error");

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued", "the claim was undone, not left in flight");
        assert_eq!(t.delivery, "pending");
        assert_eq!(t.run_id, None, "an undelivered turn does not belong to that run");
        assert!(t.completed_at.is_none(), "it was requeued, not failed");
        assert!(
            db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none(),
            "nothing is in flight, so the next prompt is not refused with 409",
        );
        assert_eq!(
            db::queued_turn(&app.db, &f.conv).await.unwrap().map(|q| q.id),
            Some(f.turn_id.clone()),
            "the durable queue still holds it, so a later transition retries the delivery",
        );
        // review 2 L2：放回去要自己掛 timer——閒著的 bot 不會再有 working→idle 邊來叫醒它。
        assert_eq!(t.flush_retries, 1, "算進重試次數，有上限");
        assert!(t.next_flush_at.is_some(), "有下一次嘗試的時間");
        assert!(queue_retry_timer_armed(&f.bot_id), "timer 已經掛上");

        // Still retryable: requeueing didn't poison `turns_one_queued` or the CAS.
        f.env.herdr.set_agent("agent", "pane-1", true);
        sqlx::query("UPDATE runs SET herdr_session = 'test' WHERE id = ?")
            .bind(&f.run_id)
            .execute(&app.db)
            .await
            .unwrap();
        // 退避時間到之前的其他喚醒不搶先；timer 到點（清掉 next_flush_at）才送。
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "still inside its backoff");
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "in_flight", "the retry got to claim it");
        assert_eq!(t.run_id.as_deref(), Some(f.run_id.as_str()));
    }

    /// issue #125：flush 讀完 run 到認領之間，run 被不拿 bot 鎖的路徑收掉（`mark_run_exited`：pane 死掉、
    /// reconcile）。重啟中那一段孤兒撤銷刻意不撤排著的派工；這時認領下去，回合掛在死掉的 run 上、
    /// 字打進不存在的 pane。認領只認領到此刻還在跑的 run，沒認領到的留在佇列給下一個 run。
    #[tokio::test]
    async fn a_queued_prompt_is_not_claimed_onto_a_run_that_ended_mid_flush() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '給舊 run 的派工' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        let (app2, run2) = (app.clone(), f.run_id.clone());
        super::super::race_point::arm("flush_before_claim", &f.bot_id, move || async move {
            // `mark_run_exited` 的第一句寫入。
            sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE id=?").bind(db::now()).bind(&run2).execute(&app2.db).await.unwrap();
        });

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref()), ("queued", None), "留在佇列，不掛到死掉的 run 上");
        assert_eq!(typed(&f, "給舊 run 的派工"), 0, "一個字都沒打進舊 pane");
        assert_eq!(t.flush_retries, 0, "不是這一筆送不出去，不花它的重試");
    }

    /// issue #125：放回佇列之前，這一筆已經被不拿 bot 鎖的路徑收掉（run 結束的 `fail_in_flight`）。
    /// 以前 `defer_queued_turn` 不看 CAS 有沒有打中，照樣回報「放回去了」並掛 retry timer、記一行 put back。
    #[tokio::test]
    async fn a_put_back_that_loses_to_a_run_exit_does_not_pretend_it_requeued() {
        let f = queued("no-such-session").await;
        let app = f.env.app.clone();
        forget_queue_retry_timer(&f.bot_id);
        let (app2, turn2) = (app.clone(), f.turn_id.clone());
        super::super::race_point::arm("defer_before_return", &f.turn_id, move || async move {
            super::super::turn_controller::fail(&app2.db, &turn2, super::super::turn_controller::DeliveryOnFail::Keep, "run ended")
                .await
                .unwrap();
        });

        flush_queued_locked(&app, &f.bot_id).await.expect("the flush itself does not error");

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "failed", "別的路徑收掉的就是收掉了");
        assert_eq!((t.flush_retries, t.next_flush_at.clone()), (0, None), "沒有放回去，就不記一次重試");
        assert!(!queue_retry_timer_armed(&f.bot_id), "沒有東西在排隊，不掛 retry timer");
    }

    /// Once the RPC went out, a failure is not requeued (could deliver twice); `delivery='unknown'`
    /// parks it. The mock's `unsupported` answer to `agent.prompt` is exactly that case.
    #[tokio::test]
    async fn a_failure_after_the_rpc_is_parked_as_unknown_not_requeued() {
        let f = queued("test").await;
        // The agent.prompt path needs an agent herdr has a session bound to.
        f.env.herdr.set_agent("agent", "pane-1", true);
        let app = f.env.app.clone();

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "in_flight");
        assert_eq!(t.delivery, "unknown");
        assert!(db::queued_turn(&app.db, &f.conv).await.unwrap().is_none(), "not put back on the queue");
    }

    /// The queue gets the live prompt's screen checks (review 2026-09-12 #6): codex on its `/model`
    /// picker (mock can't Esc) → back on the queue with the hint.
    #[tokio::test]
    async fn a_queued_prompt_waits_while_codex_shows_its_model_picker() {
        let f = queued_kind("codex", "test").await;
        let app = f.env.app.clone();
        f.env.herdr.set_screen("pane-1", "Select Model and Effort\n› 1. gpt-5 (current)\n  2. gpt-5-mini\n\nPress enter to confirm or esc to go back\n");

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued", "put back, not delivered into the menu");
        assert_eq!(t.run_id, None);
        assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt"), "nothing was typed");
        let hints: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&f.conv)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(hints.len(), 1, "{hints:?}");
        assert!(hints[0].contains("/model"), "{hints:?}");
    }

    /// claude parked on its login menu is the same story: `needs_login`, back on the queue.
    #[tokio::test]
    async fn a_queued_prompt_waits_while_claude_shows_its_login_menu() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.set_screen("pane-1", "Select login method:\n❯ 1. Claude account with subscription\n  2. Anthropic Console account\n");

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued");
        assert!(t.next_flush_at.is_some() && queue_retry_timer_armed(&f.bot_id), "畫面擋住也要自己掛 timer（L2）");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt"));
        let hints: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(hints, 1);
    }

    /// 之後讓放回佇列（寫 `flush_retries`）或收成 failed 的寫入失敗（SQLite 這一刻寫不進去）；認領那一句照常。
    async fn lose(app: &Arc<App>, name: &str, what: &str) {
        let on = if what == "failed" { "status ON turns WHEN NEW.status = 'failed'" } else { "flush_retries ON turns" };
        sqlx::query(&format!("CREATE TRIGGER {name} BEFORE UPDATE OF {on} BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"))
            .execute(&app.db)
            .await
            .unwrap();
    }

    async fn heal(app: &Arc<App>, name: &str) {
        sqlx::query(&format!("DROP TRIGGER {name}")).execute(&app.db).await.unwrap();
    }

    /// #158 驗收二：認領之後一個字都沒打就放棄（claude 停在登入選單、herdr client 拿不到），放回佇列那一句卻寫不進去——
    /// 以前只記一行 error、flush 回 Ok，那一筆停在 in_flight＋pending：之後每一則都被擋，交辦看起來已經離開佇列，其實沒送。
    /// 現在 flush 回錯誤、記成欠著；DB 好了補回佇列（照退避、算一次重試），擋住它的東西沒了就照常送，從頭到尾只送一次。
    #[tokio::test]
    async fn a_claim_whose_put_back_cannot_be_written_is_owed_not_left_in_flight() {
        for (why, session) in [("pane not ready", "test"), ("no herdr client", "no-such-session")] {
            let f = queued(session).await;
            let app = f.env.app.clone();
            if session == "test" {
                f.env.herdr.set_screen("pane-1", "Select login method:\n❯ 1. Claude account with subscription\n  2. Anthropic Console account\n");
            }
            forget_queue_retry_timer(&f.bot_id);
            lose(&app, "lost_put_back", "put_back").await;

            assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "{why}：放不回去，flush 不回普通的成功");
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.delivery.as_str(), t.flush_retries), ("in_flight", "pending", 0), "{why}：寫不進去就是還沒放回去");

            heal(&app, "lost_put_back").await;
            super::super::owed_delivery::settle_locked(&app, &f.bot_id).await.expect("DB 好了：補上");
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.run_id.as_deref(), t.flush_retries), ("queued", None, 1), "{why}：補回佇列，算一次重試");
            assert!(t.next_flush_at.is_some() && queue_retry_timer_armed(&f.bot_id), "{why}：掛了下一次的 timer");
            assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none(), "{why}：不再擋住下一則");

            // 擋住它的東西沒了、退避到了：照常送出，只送一次。
            f.env.herdr.set_screen("pane-1", "");
            f.env.herdr.set_agent("agent", "pane-1", true);
            sqlx::query("UPDATE runs SET herdr_session = 'test' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
            sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "{why}");
            assert_eq!(f.env.herdr.calls_to("agent.prompt").len(), 1, "{why}：只送一次");
        }
    }

    /// #158 驗收三：排隊的那一則在這個 bot 上永遠送不出去（太長、證明不了，`NotAttempted { retry: false }`），收成 failed
    /// 那一句卻寫不進去——以前只記一行 error、flush 回 Ok，那一筆停在 in_flight＋pending。現在記成欠著、flush 回錯誤；
    /// DB 好了收成 failed＋一則說明，一個字都沒打。
    #[tokio::test]
    async fn an_unsendable_queued_prompt_whose_failure_cannot_be_written_is_owed() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let huge = "x".repeat(super::super::delivery::MAX_PROVABLE_CHARS + 1);
        sqlx::query("UPDATE turns SET prompt_text = ? WHERE id = ?").bind(&huge).bind(&f.turn_id).execute(&app.db).await.unwrap();
        lose(&app, "lost_close", "failed").await;

        assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "收不成 failed：flush 不回普通的成功");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"));

        heal(&app, "lost_close").await;
        super::super::owed_delivery::settle_locked(&app, &f.bot_id).await.expect("DB 好了：補上");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "不留永久 in_flight");
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'")
            .bind(&f.turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert!(notes.len() == 1 && notes[0].contains("prompt_too_long_to_prove"), "{notes:?}");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
    }

    /// #158（同一類，還沒認領的那一段）：空的 prompt 收成 failed 那一句寫不進去——以前 `let _ =` 吞掉、回 Ok、不掛 timer，
    /// 那一筆佔著這個對話唯一的 queued 名額，直到有人剛好叫醒 flush。現在回錯誤、留在佇列、掛 timer；DB 好了就收掉。
    #[tokio::test]
    async fn an_empty_queued_prompt_that_cannot_be_dropped_stays_queued_and_is_retried() {
        let f = queued("no-such-session").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET prompt_text = '   ' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        lose(&app, "lost_drop", "failed").await;

        assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "收不掉：不回普通的成功");
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        assert!(queue_retry_timer_armed(&f.bot_id), "掛了 timer，稍後再收");

        heal(&app, "lost_drop").await;
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
    }

    /// An empty queued prompt is dropped, not requeued ("always put it back" would loop forever).
    #[tokio::test]
    async fn an_empty_queued_prompt_is_still_failed_not_requeued() {
        let f = queued("no-such-session").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET prompt_text = '   ' WHERE id = ?")
            .bind(&f.turn_id)
            .execute(&app.db)
            .await
            .unwrap();

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "failed");
        assert!(db::queued_turn(&app.db, &f.conv).await.unwrap().is_none());
    }
}

