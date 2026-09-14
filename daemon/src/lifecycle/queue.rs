//! The queued-prompt flush and the run/turn bookkeeping that frees a slot.

use super::*;

/// Hand the oldest queued prompt to the agent, if it can take one now. Caller holds the bot lock
/// (the one-in-flight / one-queued unique indexes make a lost race an error). Early returns leave
/// the turn queued; after the claim, any give-up must `requeue_turn` — `in_flight` +
/// `delivery='pending'` has no other way out.
async fn flush_queued_locked(app: &Arc<App>, bot_id: &str) -> anyhow::Result<()> {
    let conv = match db::conversation_id(&app.db, bot_id).await {
        Ok(conv) => conv,
        Err(error) => {
            tracing::warn!(error = ?error, bot = %bot_id, "could not get conversation for queued prompt flush");
            return Ok(());
        }
    };
    let Some(turn) = db::queued_turn(&app.db, &conv).await? else { return Ok(()) };
    // Put back with a backoff: other wake-ups must not spend its retries early. Its timer brings it back.
    if let Some(at) = turn.next_flush_at.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        if at > chrono::Utc::now() {
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
    let text = turn.prompt_text.clone().unwrap_or_default();
    if text.trim().is_empty() {
        // Nothing deliverable: drop it rather than leave the queue permanently blocked.
        let _ = sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=? AND status='queued'")
            .bind(db::now())
            .bind(&turn.id)
            .execute(&app.db)
            .await;
        emit_turn(app, &turn.id).await;
        return Ok(());
    }

    // Claim it first. If the CAS loses, another flush got there and this one has nothing to do.
    let claimed = sqlx::query("UPDATE turns SET status='in_flight', run_id=? WHERE id=? AND status='queued'")
        .bind(&run.id)
        .bind(&turn.id)
        .execute(&app.db)
        .await?;
    if claimed.rows_affected() == 0 {
        return Ok(());
    }
    emit_turn(app, &turn.id).await;

    // Refused by a screen check → back on the queue; the next `working -> idle` edge retries.
    if let Err(e) = pane_ready_for_prompt(app, &bot, &run, &conv).await {
        let why = match &e {
            LcError::Conflict(v) => v.get("reason").and_then(|r| r.as_str()).unwrap_or("conflict").to_string(),
            other => format!("{other:?}"),
        };
        requeue_turn(app, &turn.id, bot_id, &format!("pane not ready for a prompt: {why}")).await;
        return Ok(());
    }

    // From the claim to the RPC, giving up must requeue: `arm_stall` / `arm_progress` /
    // `try_fallback` all require `delivery == "ok"`, so an abandoned turn would 409 every
    // later prompt until the run ended, invisibly (background task).
    let client = match client_for_run(app, &run).await {
        Ok(c) => c,
        Err(e) => {
            requeue_turn(app, &turn.id, bot_id, &format!("no herdr client: {e:?}")).await;
            return Ok(());
        }
    };
    // A queued prompt waits for a codex rollout that is on its way for a few put-backs (15+30+60 s),
    // then stops waiting and goes out with what is available.
    let waited_for_log = turn.flush_retries >= CODEX_LOG_WAIT_RETRIES;
    let res = deliver_prompt(app, &client, &run, &bot, &text, false, waited_for_log).await;
    let delivery = match res {
        Ok(Delivered::Submitted) => "ok",
        // Typed and submitted on a run with no lossless evidence (grok, remote, codex before its
        // session is known): delivered as far as anyone can tell, marked for a human, never re-sent.
        Ok(Delivered::Unverified) => "unverified",
        // Nothing was typed. A temporary reason goes back on the queue with a timed retry — a busy
        // box produces no `working -> idle` edge to wake the flush (sol review round seven #2).
        Ok(Delivered::NotAttempted { reason, retry: true }) => {
            match defer_queued_turn(app, &conv, &turn.id, reason).await {
                Ok(Some(delay)) => schedule_flush_retry(app, bot_id, delay),
                Ok(None) => tracing::warn!(bot = %bot_id, turn = %turn.id, reason, "queued prompt gave up after its retry limit"),
                Err(e) => tracing::error!(bot = %bot_id, turn = %turn.id, error = %e, "could not put a queued prompt back"),
            }
            emit_turn(app, &turn.id).await;
            return Ok(());
        }
        // A prompt that can never be sent as asked on this run: fail it visibly, in one transaction
        // with its explanation, instead of retrying forever.
        Ok(Delivered::NotAttempted { reason, retry: false }) => {
            let hint = format!("沒有送出（{reason}）：這一則在這個 bot 上沒有辦法照原樣送出，所以一個字都沒打。");
            if let Err(e) = fail_queued_turn(app, &conv, &turn.id, &hint).await {
                tracing::error!(bot = %bot_id, turn = %turn.id, error = %e, "could not fail an unsendable queued prompt");
            }
            emit_turn(app, &turn.id).await;
            return Ok(());
        }
        Ok(Delivered::Unproven(why)) => {
            tracing::warn!(bot = %bot_id, reason = why, "queued prompt delivery could not be proven");
            "unknown"
        }
        Err(e) => {
            let blocked = e.downcast_ref::<HerdrError>().map(|h| h.code == "agent_blocked").unwrap_or(false);
            if blocked {
                let _ = sqlx::query("UPDATE turns SET delivery='failed', status='failed', completed_at=? WHERE id=?")
                    .bind(db::now())
                    .bind(&turn.id)
                    .execute(&app.db)
                    .await;
                let _ = insert_message(app, &conv, Some(&turn.id), "system", &format!("delivery failed: {e}"), "system", false, None).await;
                emit_turn(app, &turn.id).await;
                return Ok(());
            }
            // Not requeued: the agent may have taken it, so a retry could deliver twice.
            // `delivery='unknown'` is the designed user-visible parking state (§6.3).
            tracing::warn!(bot = %bot_id, error = %e, "queued prompt delivery unknown");
            "unknown"
        }
    };
    mark_delivery(app, &turn.id, delivery).await;
    emit_turn(app, &turn.id).await;
    if delivery == "ok" || delivery == "unverified" {
        arm_stall(app, &run.id, bot_id, &turn.id).await;
        arm_progress(app, &run.id, bot_id, &turn.id).await;
    }
    Ok(())
}

/// Undo a `queued -> in_flight` claim that never became a delivery. Bot lock held and only for
/// a turn this flush claimed, so `turns_one_queued` cannot be violated.
async fn requeue_turn(app: &Arc<App>, turn_id: &str, bot_id: &str, reason: &str) {
    match sqlx::query("UPDATE turns SET status='queued', run_id=NULL WHERE id=? AND status='in_flight'")
        .bind(turn_id)
        .execute(&app.db)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::warn!(bot = %bot_id, turn = %turn_id, %reason, "queued prompt put back on the queue");
        }
        Ok(_) => return,
        Err(e) => {
            tracing::error!(bot = %bot_id, turn = %turn_id, %reason, error = %e,
                            "could not put a claimed prompt back on the queue");
            return;
        }
    }
    emit_turn(app, turn_id).await;
}

/// Put-backs a queued prompt spends waiting for a codex rollout that has not been written yet.
pub(crate) const CODEX_LOG_WAIT_RETRIES: i64 = 3;

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

/// Put a claimed turn back on the queue with its retry count and next attempt time, or — past the
/// limit — fail it with an explanation. One transaction either way. `Some(delay)` = requeued.
async fn defer_queued_turn(app: &Arc<App>, conv: &str, turn_id: &str, reason: &str) -> anyhow::Result<Option<std::time::Duration>> {
    let mut tx = app.db.begin().await?;
    let retries: i64 = sqlx::query_scalar("SELECT flush_retries FROM turns WHERE id = ?").bind(turn_id).fetch_one(&mut *tx).await?;
    if retries >= QUEUE_RETRY_LIMIT {
        sqlx::query("UPDATE turns SET status='failed', delivery='failed', completed_at=? WHERE id=? AND status='in_flight'")
            .bind(db::now())
            .bind(turn_id)
            .execute(&mut *tx)
            .await?;
        let hint = format!("沒有送出：試了 {QUEUE_RETRY_LIMIT} 次都沒辦法打字（最後一次是 {reason}），已停止自動重試。請清空輸入框後重送。");
        insert_message_tx(&mut tx, conv, Some(turn_id), "system", &hint, "system", false, None).await?;
        tx.commit().await?;
        return Ok(None);
    }
    let delay = queue_retry_delay(retries);
    let next = (chrono::Utc::now() + chrono::Duration::from_std(delay)?).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query(
        "UPDATE turns SET status='queued', run_id=NULL, flush_retries=flush_retries+1, next_flush_at=?
          WHERE id=? AND status='in_flight'",
    )
    .bind(&next)
    .bind(turn_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(turn = turn_id, reason, attempt = retries + 1, retry_in_s = delay.as_secs(), "queued prompt not sent yet; put back");
    Ok(Some(delay))
}

/// Fail a claimed turn together with the system message that explains it.
async fn fail_queued_turn(app: &Arc<App>, conv: &str, turn_id: &str, hint: &str) -> anyhow::Result<()> {
    let mut tx = app.db.begin().await?;
    sqlx::query("UPDATE turns SET delivery='failed', status='failed', completed_at=? WHERE id=? AND status='in_flight'")
        .bind(db::now())
        .bind(turn_id)
        .execute(&mut *tx)
        .await?;
    insert_message_tx(&mut tx, conv, Some(turn_id), "system", hint, "system", false, None).await?;
    tx.commit().await?;
    Ok(())
}

/// One pending retry timer per bot. The value is the timer's generation, so only the timer that is
/// still registered fires.
static QUEUE_RETRY_TIMERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> = std::sync::OnceLock::new();
static QUEUE_RETRY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Arm `fire` after `delay`, unless this bot already has a retry timer waiting — extra wake-ups
/// must not pile up timers. `false` = one was already armed.
pub(crate) fn arm_queue_retry<F>(bot_id: &str, delay: std::time::Duration, fire: F) -> bool
where
    F: FnOnce() + Send + 'static,
{
    let timers = QUEUE_RETRY_TIMERS.get_or_init(Default::default);
    let generation = {
        let Ok(mut map) = timers.lock() else { return false };
        if map.contains_key(bot_id) {
            return false;
        }
        let g = QUEUE_RETRY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        map.insert(bot_id.to_string(), g);
        g
    };
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let mine = timers.lock().map(|mut map| {
            if map.get(&bot_id) == Some(&generation) {
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
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.bot_id, t.next_flush_at FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE t.status = 'queued' AND t.next_flush_at IS NOT NULL",
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
pub(crate) async fn rearm_queue_retries_with<F>(app: &Arc<App>, fire: F) -> usize
where
    F: Fn(String) + Clone + Send + 'static,
{
    let pending = match pending_queue_retries(app).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "cannot re-arm queued prompt retries");
            return 0;
        }
    };
    let mut armed = 0;
    for (bot, delay) in pending {
        let fire = fire.clone();
        let id = bot.clone();
        if arm_queue_retry(&bot, delay, move || fire(id)) {
            armed += 1;
            tracing::info!(bot = %bot, retry_in_s = delay.as_secs(), "re-armed a queued prompt retry after restart");
        }
    }
    armed
}

pub async fn rearm_queue_retries(app: &Arc<App>) -> usize {
    let a = app.clone();
    rearm_queue_retries_with(app, move |bot| schedule_flush_queued(&a, &bot)).await
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


/// Terminate a run: state `exited`, fail its in-flight turn, drop the pane watcher.
pub async fn mark_run_exited(app: &Arc<App>, run_id: &str, reason: &str) {
    let Ok(Some(run)) = db::run(&app.db, run_id).await else { return };
    if !matches!(run.state.as_str(), "starting" | "running" | "stopping") {
        return;
    }
    let _ = sqlx::query("UPDATE runs SET state = 'exited', ended_at = ? WHERE id = ?")
        .bind(db::now())
        .bind(run_id)
        .execute(&app.db)
        .await;
    fail_in_flight(app, run_id, &format!("run ended: {reason}")).await;
    if let Some(p) = run.pane_id.as_deref() {
        let host = db::bot_host(&app.db, &run.bot_id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
        if let Some(session) = app.session_for_run(&run).await {
            crate::events::unwatch_pane_on_session(app, &host, &session, p).await;
        }
    }
    app.emit_bot_status(&run.bot_id).await;
}

pub async fn fail_in_flight(app: &Arc<App>, run_id: &str, note: &str) {
    if let Ok(Some(t)) = db::in_flight_turn(&app.db, run_id).await {
        let _ = sqlx::query("UPDATE turns SET status = 'failed', completed_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(&t.id)
            .execute(&app.db)
            .await;
        let _ = insert_message(app, &t.conversation_id, Some(&t.id), "system", note, "system", false, None).await;
        emit_turn(app, &t.id).await;
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
        apply_codex_limit_hit_quota(&app, LOCAL_HOST, Some("astra"), notice).await;

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
        apply_codex_limit_hit_quota(&app, LOCAL_HOST, None, notice).await;
        let first = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().unwrap();

        // 中間 app-server 清橫幅是另一條規則；這裡只測重掃。
        apply_codex_limit_hit_quota(&app, LOCAL_HOST, None, notice).await;
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

    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?")
            .bind(id)
            .fetch_one(&app.db)
            .await
            .unwrap()
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
        assert!(!arm_queue_retry(&bot, std::time::Duration::from_secs(1), hit(&fired)), "not even a sooner one");
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
        // 「重啟」：舊行程的 timer 全沒了；框也清空了。退避剩 150 ms。
        forget_queue_retry_timer(&f.bot_id);
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        let soon = (chrono::Utc::now() + chrono::Duration::milliseconds(150)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET next_flush_at = ? WHERE id = ?").bind(&soon).bind(&f.turn_id).execute(&app.db).await.unwrap();

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
        assert_eq!(rearm_queue_retries_with(&app, fire.clone()).await, 1, "one timer for the bot");
        assert_eq!(rearm_queue_retries_with(&app, fire).await, 0, "a second pass does not add another");
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "not before it is due");

        // The timer fires after ~150 ms; the typed delivery itself then takes about two seconds.
        let mut t = turn(&app, &f.turn_id).await;
        for _ in 0..80 {
            if t.delivery != "pending" && t.status != "queued" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            t = turn(&app, &f.turn_id).await;
        }
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"));
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1, "exactly once");
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

        // Still retryable: requeueing didn't poison `turns_one_queued` or the CAS.
        f.env.herdr.set_agent("agent", "pane-1", true);
        sqlx::query("UPDATE runs SET herdr_session = 'test' WHERE id = ?")
            .bind(&f.run_id)
            .execute(&app.db)
            .await
            .unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "in_flight", "the retry got to claim it");
        assert_eq!(t.run_id.as_deref(), Some(f.run_id.as_str()));
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
        assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt"));
        let hints: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(hints, 1);
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

