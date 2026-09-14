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
    let res = deliver_prompt(app, &client, &run, &bot, &text, false).await;
    let delivery = match res {
        Ok(Delivered::Submitted) => "ok",
        // Nothing was typed. A temporary reason goes back on the queue with a timed retry — a busy
        // box produces no `working -> idle` edge to wake the flush (sol review round seven #2).
        Ok(Delivered::NotAttempted { reason, retry: true }) => {
            requeue_turn(app, &turn.id, bot_id, &format!("not sent yet: {reason}")).await;
            schedule_flush_retry(app, bot_id, QUEUE_RETRY_SECS);
            return Ok(());
        }
        // A prompt that can never be proven on this run: fail it visibly instead of retrying forever.
        Ok(Delivered::NotAttempted { reason, retry: false }) => {
            let _ = sqlx::query("UPDATE turns SET delivery='failed', status='failed', completed_at=? WHERE id=? AND status='in_flight'")
                .bind(db::now())
                .bind(&turn.id)
                .execute(&app.db)
                .await;
            let hint = format!("沒有送出（{reason}）：這一則在這個 bot 上沒有辦法確認送達，所以一個字都沒打。");
            let _ = insert_message(app, &conv, Some(&turn.id), "system", &hint, "system", false, None).await;
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
    let _ = sqlx::query("UPDATE turns SET delivery=? WHERE id=?").bind(delivery).bind(&turn.id).execute(&app.db).await;
    emit_turn(app, &turn.id).await;
    if delivery == "ok" {
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

/// Seconds before a queued prompt that could not be typed yet (busy box, transcript not reported)
/// is tried again.
const QUEUE_RETRY_SECS: u64 = 15;

/// Try the queue again after `secs`, for conditions no lifecycle edge will announce.
pub fn schedule_flush_retry(app: &Arc<App>, bot_id: &str, secs: u64) {
    if cfg!(test) {
        return;
    }
    let app = app.clone();
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        schedule_flush_queued(&app, &bot_id);
    });
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

        // 使用者把草稿清掉了。
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"));
        let pane = f.env.herdr.pane("pane-1").unwrap();
        assert_eq!(pane.transcript.iter().filter(|l| l.contains("Reply with PONG please")).count(), 1);
    }

    /// 這個 bot 上永遠證明不了的 prompt：不打字、不卡在 in-flight，直接失敗並說明。
    #[tokio::test]
    async fn a_queued_prompt_that_can_never_be_proven_fails_visibly_instead_of_hanging() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(&app.db, &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = '第一行\n第二行' WHERE id = ?").bind(&f.turn_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
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

