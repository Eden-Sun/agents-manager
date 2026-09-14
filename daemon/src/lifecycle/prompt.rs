//! Delivering a prompt: the turn row, the pane write, and the queued path.

use super::*;

/// Close a prompt whose local setup failed after its turn was committed; `pending` must never
/// be the last state the frontend sees.
async fn fail_prompt_delivery(app: &Arc<App>, conversation_id: &str, turn_id: &str, reason: &str) {
    let updated = match sqlx::query(
        "UPDATE turns SET delivery='failed', status='failed', completed_at=? WHERE id=? AND status='in_flight'",
    )
    .bind(db::now())
    .bind(turn_id)
    .execute(&app.db)
    .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(turn = %turn_id, error = %e, "could not fail prompt delivery");
            return;
        }
    };
    if updated.rows_affected() == 0 {
        return;
    }
    let _ = insert_message(
        app,
        conversation_id,
        Some(turn_id),
        "system",
        &format!("delivery failed: {reason}"),
        "system",
        false,
        None,
    )
    .await;
    emit_turn(app, turn_id).await;
}

async fn emit_prompt_message(app: &Arc<App>, bot_id: &str, message_id: &str) {
    if let Ok(Some(m)) = sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE id=?")
        .bind(message_id)
        .fetch_optional(&app.db)
        .await
    {
        app.emit("message_added", json!({"bot_id": bot_id, "message": m})).await;
    }
}


/// What the composer holds right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoxState {
    /// The bare marker row: nothing typed (a Tip/spinner line elsewhere does not count).
    Empty,
    /// Our text is in there (matched on its head **or** its tail, so wrapping and a long
    /// multi-line paste whose first line scrolled out of the box still count).
    Holds,
    /// Something else is in there, or the screen does not say — never type into this.
    Other,
}

/// Chars of the prompt used as a needle, at each end. Long enough not to match everything,
/// short enough to survive the TUI's own wrapping.
const NEEDLE_LEN: usize = 24;
/// Below this a needle proves nothing (a two-character prompt is in every screen).
const NEEDLE_MIN: usize = 8;

fn squash_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Head and tail needles, or `None` when the text is too short to prove anything with.
pub(crate) fn needles(text: &str) -> Option<(String, String)> {
    let flat = squash_ws(text);
    if flat.chars().count() < NEEDLE_MIN {
        return None;
    }
    let head: String = flat.chars().take(NEEDLE_LEN).collect();
    let tail: String = {
        let all: Vec<char> = flat.chars().collect();
        all[all.len().saturating_sub(NEEDLE_LEN)..].iter().collect()
    };
    Some((head, tail))
}

/// How many times the prompt shows on this screen. Counted on the whitespace-squashed screen so
/// wrapped Chinese and re-indented rows still match; the head is counted, falling back to the
/// tail (a very long paste can have its head scrolled away while its tail is on screen).
pub(crate) fn screen_hits(screen: &str, text: &str) -> usize {
    let Some((head, tail)) = needles(text) else { return 0 };
    let flat = squash_ws(screen);
    let n = flat.matches(head.as_str()).count();
    if n > 0 {
        n
    } else {
        flat.matches(tail.as_str()).count()
    }
}

/// Pure: what is in the composer on this screen.
pub(crate) fn box_state(kind: &str, screen: &str, text: &str) -> BoxState {
    let box_text = composer_text(kind, screen);
    match box_text {
        None if pane_awaits_input(kind, screen) => BoxState::Empty,
        None => BoxState::Other,
        Some(inside) => {
            let flat = squash_ws(&inside);
            let whole = squash_ws(text);
            if flat == whole {
                return BoxState::Holds;
            }
            // 框裡看得到的可能只是整段的一截（長訊息把開頭捲出框，或框只畫得下前幾行）：
            // 那一截是我們這段文字的子字串就算數，反過來框裡含頭或含尾也算。
            let fragment = flat.chars().count() >= NEEDLE_MIN && whole.contains(&flat);
            match needles(text) {
                _ if fragment => BoxState::Holds,
                Some((head, tail)) if flat.contains(&head) || flat.contains(&tail) => BoxState::Holds,
                _ => BoxState::Other,
            }
        }
    }
}

/// The outcome of one delivery attempt. Anything short of `Submitted` is **not** success:
/// sol review 2026-09-14 #1——「證明不了遺失」不等於「已經送出」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivered {
    /// Seen: our text was in the box, and after Enter the box is empty and the screen gained it.
    Submitted,
    /// Could not be proven either way. Caller must park the turn, never re-type on this.
    Unknown(&'static str),
}

/// Pause after typing before the box is read (the TUI has to draw the paste).
const TYPE_SETTLE_MS: u64 = 700;
/// Pause after Enter before checking the prompt left the box.
const SUBMIT_SETTLE_MS: u64 = 1500;
/// Scrollback read for the before/after comparison. Unwrapped so a wrapped line is one row.
const DELIVER_SCAN_LINES: u32 = 400;

/// Decide the outcome from the two screens around Enter. Pure, so every case is a test:
/// `before_enter` is the screen with our text in the box, `after` the one after Enter.
pub(crate) fn submit_outcome(kind: &str, hits_before: usize, before_enter: &str, after: &str, text: &str) -> Delivered {
    match box_state(kind, after, text) {
        BoxState::Holds => Delivered::Unknown("still_in_box"),
        BoxState::Other => Delivered::Unknown("composer_unreadable"),
        BoxState::Empty => {
            // The box held our text and is now empty: the Enter was taken. Confirm the screen
            // gained an occurrence, so a TUI that silently threw the text away is not counted.
            if screen_hits(after, text) > hits_before {
                Delivered::Submitted
            } else if needles(text).is_none() && after != before_enter {
                // Too short to count occurrences; the box emptying plus a changed screen is all
                // the evidence there is.
                Delivered::Submitted
            } else {
                Delivered::Unknown("no_new_echo")
            }
        }
    }
}

/// `agent.prompt` on an agent herdr has no session bound to answered ok twice while the text never
/// reached the pane (2026-09-14 wits-c1-op-xh). Treat that as "cannot deliver through the agent".
async fn agent_prompt_usable(client: &HerdrClient, target: &str) -> bool {
    match client.agent_get(target).await {
        Ok(Some(info)) => info.agent_session.is_some(),
        _ => false,
    }
}

/// Deliver a prompt to the agent, and know whether it landed.
///
/// `agent.prompt` (herdr types with bracketed paste and refuses at a dialog with `agent_blocked`)
/// is still the normal path — but only for a run whose pane the daemon has **not** typed into and
/// whose agent has a session bound. Everything else types into the pane and watches the screen:
///
/// 1. the box must start empty (residue is cleared once with `ctrl+c`, then re-checked);
/// 2. after `pane.send_text` our text must be **in the box** — if nothing landed at all it is
///    typed once more, and only then; anything unreadable stops here;
/// 3. after Enter the box must be empty **and** the screen must have gained the text.
///
/// Every read failure is an error, never an empty screen (sol review #2). Anything unproven comes
/// back as `Unknown`, which the caller parks — it must never look like a delivery.
pub(crate) async fn deliver_prompt(
    app: &Arc<App>,
    client: &HerdrClient,
    run: &db::Run,
    bot: &db::Bot,
    text: &str,
    force_pane: bool,
) -> anyhow::Result<Delivered> {
    let pane = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(str::to_string);
    let target = db::run_target(run, bot);
    let must_type = force_pane || db::pane_typed(&app.db, &run.id).await || !agent_prompt_usable(client, &target).await;
    let Some(pane) = pane.filter(|_| must_type) else {
        client
            .call_timeout("agent.prompt", json!({"target": target, "text": text}), Duration::from_secs(10))
            .await?;
        return Ok(Delivered::Submitted);
    };
    // Typing into the pane is itself a reason to keep typing into it from now on.
    let _ = db::set_pane_typed(&app.db, &run.id).await;
    let read = || async { client.pane_read(&pane, "recent-unwrapped", DELIVER_SCAN_LINES).await.map(|r| r.text) };

    // 1. Start from an empty box.
    let before = read().await?;
    let hits_before = screen_hits(&before, text);
    if box_state(&bot.kind, &before, text) != BoxState::Empty {
        tracing::warn!(run = %run.id, bot = %bot.name, "composer was not empty before typing; clearing it");
        client.pane_send_keys(&pane, &["ctrl+c"]).await?;
        tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
        if box_state(&bot.kind, &read().await?, text) != BoxState::Empty {
            return Ok(Delivered::Unknown("composer_not_empty"));
        }
    }

    // 2. Type, and see it in the box.
    client.pane_send_text(&pane, text).await?;
    tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
    let mut typed = read().await?;
    if box_state(&bot.kind, &typed, text) == BoxState::Empty && screen_hits(&typed, text) == hits_before {
        // Nothing landed anywhere: the box is provably empty, so re-typing cannot duplicate.
        tracing::warn!(run = %run.id, bot = %bot.name, "typed prompt did not reach the composer; typing it once more");
        client.pane_send_text(&pane, text).await?;
        tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
        typed = read().await?;
    }
    match box_state(&bot.kind, &typed, text) {
        BoxState::Holds => {}
        BoxState::Empty if screen_hits(&typed, text) > hits_before => {
            // The TUI submitted it as it was pasted (bracketed paste with a trailing newline).
            tracing::info!(run = %run.id, bot = %bot.name, "the pasted prompt was submitted without an Enter");
            return Ok(Delivered::Submitted);
        }
        BoxState::Empty => return Ok(Delivered::Unknown("nothing_typed")),
        BoxState::Other => return Ok(Delivered::Unknown("composer_has_other_text")),
    }

    // 3. Enter, and see it leave the box for the transcript.
    client.pane_send_keys(&pane, &["Enter"]).await?;
    tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
    let after = read().await?;
    let outcome = match submit_outcome(&bot.kind, hits_before, &typed, &after, text) {
        Delivered::Unknown("still_in_box") => {
            // One more Enter, and it is checked too (sol review #2: the second Enter was blind).
            tracing::warn!(run = %run.id, bot = %bot.name, "prompt still in the composer after Enter; pressing Enter again");
            client.pane_send_keys(&pane, &["Enter"]).await?;
            tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
            submit_outcome(&bot.kind, hits_before, &after, &read().await?, text)
        }
        other => other,
    };
    match outcome {
        Delivered::Submitted => tracing::info!(run = %run.id, bot = %bot.name, "prompt typed into the pane and seen submitted"),
        Delivered::Unknown(why) => tracing::warn!(run = %run.id, bot = %bot.name, reason = why, "could not confirm the prompt was submitted"),
    }
    Ok(outcome)
}

#[derive(serde::Serialize)]
pub struct PromptOut {
    pub turn_id: String,
    pub message_id: String,
    pub delivery: String,
}

pub async fn prompt(app: &Arc<App>, bot_id: &str, text: &str, client_request_id: &str) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, &[], None).await
}

/// `prompt` with images (`attach.rs`): the agent gets paths on its host; the timeline renders
/// thumbnails from `messages.attachments_json`.
pub async fn prompt_with(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, attachment_ids, None).await
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

/// The screen checks every prompt passes before text enters the pane — shared with the queue
/// flush (review 2026-09-12 #6: the flush skipped them and typed into codex's `/model` menu).
/// Refusals insert a system hint and 409 with `needs_login` / `dialog_open` / `picker_open`.
pub(crate) async fn pane_ready_for_prompt(app: &Arc<App>, bot: &db::Bot, run: &db::Run, conv: &str) -> LcResult<()> {
    // An unlogged claude opens on "Select login method" and looks idle; a prompt would type into the menu.
    if bot.kind == "claude" && crate::tui_prompts::stuck_at_login(app, run).await {
        let identity = bot.identity.clone().unwrap_or_default();
        let hint = if identity.is_empty() {
            "這個 claude 還沒登入：到「終端」分頁選 1 完成登入，或在額度那格按「登入」。".to_string()
        } else {
            format!("身份 `{identity}` 還沒登入：到「終端」分頁選 1 完成登入，或在額度那格按「登入」。")
        };
        let _ = insert_message(app, conv, None, "system", &hint, "system", false, None).await;
        return Err(LcError::conflict("needs_login", json!({"run_id": run.id, "identity": identity, "message": hint})));
    }
    // claude「Switch model?」框被 herdr 判成 idle，prompt 打進去會被吃、Enter 按了 Yes（2026-09-11
    // AGM 實測）。還看得到框＝有人在終端手動 `/model`；使用者要送訊息，按 Esc 退掉再送，退不掉就講清楚。
    if bot.kind == "claude" {
        if let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            if let Ok(client) = client_for_run(app, run).await {
                if let Ok(r) = client.pane_read(pane, "visible", 60).await {
                    if crate::tui_prompts::is_switch_model_dialog(&r.text) {
                        let _ = client.pane_send_keys(pane, &["Escape"]).await;
                        tokio::time::sleep(Duration::from_millis(700)).await;
                        let still = matches!(client.pane_read(pane, "visible", 60).await,
                            Ok(r2) if crate::tui_prompts::is_switch_model_dialog(&r2.text));
                        if still {
                            let hint = "claude 的「Switch model?」確認框擋在輸入列前面，關不掉。請到「終端」分頁選 1 或 2 再送一次。";
                            let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                            return Err(LcError::conflict("dialog_open", json!({"run_id": run.id, "message": hint})));
                        }
                        tracing::info!(run = %run.id, "closed a leftover claude model-switch confirmation before delivering a prompt");
                    }
                }
            }
        }
    }
    // codex `/model` 選單開著時，prompt 會變成選單操作、Enter 換掉模型（2026-09-10 實測）。先關掉，關不掉就講清楚。
    if bot.kind == "codex" {
        if let Some(pane) = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            if let Ok(client) = client_for_run(app, run).await {
                if !crate::codex_live::close_picker(&client, pane).await {
                    let hint = "codex 的 /model 選單擋在輸入列前面，關不掉。請到「終端」分頁按 Esc 回到輸入列再送一次。";
                    let _ = insert_message(app, conv, None, "system", hint, "system", false, None).await;
                    return Err(LcError::conflict("picker_open", json!({"run_id": run.id, "message": hint})));
                }
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
    // `None` = 使用者自己在畫面上打的。
    relay_from: Option<&str>,
) -> LcResult<PromptOut> {
    let deliver = deliver.unwrap_or(text);
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;

    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;
    // Resolve first so an unknown id is a plain 400, not an undelivered turn.
    let files = crate::attach::resolve(app, bot_id, attachment_ids)
        .await
        .map_err(|e| LcError::Bad(e.to_string()))?;
    let deliver = crate::attach::deliver_text(deliver, &files);

    // 2. idempotency
    if let Some(t) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
        .bind(&conv)
        .bind(client_request_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
    {
        let mid = sqlx::query_scalar::<_, String>("SELECT id FROM messages WHERE turn_id=? AND role='user' LIMIT 1")
            .bind(&t.id)
            .fetch_optional(&app.db)
            .await
            .map_err(up)?
            .unwrap_or_default();
        return Ok(PromptOut { turn_id: t.id, message_id: mid, delivery: t.delivery });
    }

    // 1. preconditions
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| {
        LcError::conflict("bot has no active run", json!({}))
    })?;
    if run.state != "running" {
        return Err(LcError::conflict("run is not running", json!({"run_id": run.id, "state": run.state})));
    }
    if run.agent_status == "blocked" {
        return Err(LcError::conflict("agent is blocked; answer the prompt first", json!({"run_id": run.id})));
    }
    if let Some(t) = db::in_flight_turn(&app.db, &run.id).await.map_err(up)? {
        return Err(LcError::conflict("a turn is already in flight", json!({"turn_id": t.id})));
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

    // 3. turn + user message committed BEFORE the RPC, so an early hook can match.
    let turn_id = db::ulid();
    let mut tx = app.db.begin().await.map_err(up)?;
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at)
         VALUES (?,?,?,'web','in_flight','pending',?,?)",
    )
    .bind(&turn_id)
    .bind(&conv)
    .bind(&run.id)
    .bind(client_request_id)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    let msg_id = db::ulid();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, relay_from, created_at) VALUES (?,?,?,'user',?,'web',?,?,?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(text)
    .bind(group_id)
    .bind(relay_from)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    if let Err(e) = crate::attach::bind(app, &msg_id, &files).await {
        emit_prompt_message(app, bot_id, &msg_id).await;
        fail_prompt_delivery(app, &conv, &turn_id, &format!("attachment binding failed: {e}")).await;
        return Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into() });
    }
    emit_prompt_message(app, bot_id, &msg_id).await;
    emit_turn(app, &turn_id).await;

    // 4. deliver
    let res = deliver_prompt(app, &client, &run, &bot, &deliver, false).await;
    let delivery = match res {
        Ok(Delivered::Submitted) => "ok",
        // Unproven is not delivered: park the turn (§6.3) instead of arming a watchdog for a
        // prompt that may never have reached the agent.
        Ok(Delivered::Unknown(why)) => {
            tracing::warn!(bot = %bot_id, reason = why, "prompt delivery could not be confirmed");
            "unknown"
        }
        Err(e) => {
            let blocked = e.downcast_ref::<HerdrError>().map(|h| h.code == "agent_blocked").unwrap_or(false);
            if blocked {
                let _ = sqlx::query("UPDATE turns SET delivery='failed', status='failed', completed_at=? WHERE id=?")
                    .bind(db::now())
                    .bind(&turn_id)
                    .execute(&app.db)
                    .await;
                let _ = insert_message(app, &conv, Some(&turn_id), "system", &format!("delivery failed: {e}"), "system", false, None).await;
                emit_turn(app, &turn_id).await;
                return Ok(PromptOut { turn_id, message_id: msg_id, delivery: "failed".into() });
            }
            tracing::warn!(error = %e, "agent.prompt delivery unknown");
            "unknown"
        }
    };
    let _ = sqlx::query("UPDATE turns SET delivery=? WHERE id=?").bind(delivery).bind(&turn_id).execute(&app.db).await;
    emit_turn(app, &turn_id).await;
    if delivery == "ok" {
        arm_stall(app, &run.id, bot_id, &turn_id).await;
        arm_progress(app, &run.id, bot_id, &turn_id).await;
    }
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: delivery.into() })
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
        Fixture { env, bot_id, conv, run_id }
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
}


#[cfg(test)]
mod delivery_tests {
    use super::*;

    /// 真實畫面骨架（2026-09-14 w1HJ:pM 那類 claude）：對話區 + Tip/spinner 行 + 空輸入框 + 狀態列。
    fn screen(transcript: &str, box_rows: &[&str]) -> String {
        let mut s = String::new();
        s.push_str(transcript);
        s.push_str("\n✻ Sautéed for 15m 9s · done 2:18 PM\n");
        s.push_str("─────────────────────────────────────────────\n");
        if box_rows.is_empty() {
            s.push_str("❯\n");
        } else {
            s.push_str(&format!("❯ {}\n", box_rows[0]));
            for r in &box_rows[1..] {
                s.push_str(&format!("  {r}\n"));
            }
        }
        s.push_str("─────────────────────────────────────────────\n");
        s.push_str("  user. | web | OP5 61% | 5h:53% | 7d:95%\n");
        s.push_str("  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n");
        s
    }

    const TEXT: &str = "加一個功能除了按 SKU 之外，也要能用品名批次置換";
    const HISTORY: &str = "❯ 加一個功能除了按 SKU 之外，也要能用品名批次置換\n⏺ 好，我看一下。\n";

    #[test]
    fn an_empty_box_a_full_box_and_someone_elses_text_are_told_apart() {
        assert_eq!(box_state("claude", &screen("⏺ 先前的回覆\n", &[]), TEXT), BoxState::Empty);
        assert_eq!(box_state("claude", &screen("⏺ 先前的回覆\n", &[TEXT]), TEXT), BoxState::Holds);
        assert_eq!(box_state("claude", &screen("⏺ 先前的回覆\n", &["/effort high"]), TEXT), BoxState::Other);
    }

    /// 中文折行、多行貼上：開頭在第一列、結尾在最後一列，兩邊都認得。
    #[test]
    fn a_wrapped_multi_line_chinese_paste_is_still_seen_in_the_box() {
        let wrapped = screen("⏺ 先前的回覆\n", &["加一個功能除了按 SKU 之外，", "也要能用品名批次置換"]);
        assert_eq!(box_state("claude", &wrapped, TEXT), BoxState::Holds);
        // 長訊息的開頭被捲出框、只剩尾巴看得到，也算（長度要超過 needle，否則頭尾是同一段）。
        let long = "先把離線報價匯入的效能問題整理成一份報告，接著加一個功能：除了按 SKU 之外，也要能用品名批次置換";
        let tail_only = screen("⏺ 先前的回覆\n", &["也要能用品名批次置換"]);
        assert_eq!(box_state("claude", &tail_only, long), BoxState::Holds);
    }

    /// 送前→框內→送出：三張畫面的轉換就是唯一的成功條件。
    #[test]
    fn the_before_in_box_after_transition_is_what_counts_as_submitted() {
        let before = screen("⏺ 先前的回覆\n", &[]);
        let in_box = screen("⏺ 先前的回覆\n", &[TEXT]);
        let after = screen(&format!("⏺ 先前的回覆\n❯ {TEXT}\n"), &[]);
        let hits_before = screen_hits(&before, TEXT);
        assert_eq!(hits_before, 0);
        assert_eq!(submit_outcome("claude", hits_before, &in_box, &after, TEXT), Delivered::Submitted);
    }

    /// 對話裡早就有同樣一句（使用者重送）：只有「又多一次」才算送出。
    #[test]
    fn an_identical_prompt_already_in_the_history_is_not_mistaken_for_this_one() {
        let before = screen(HISTORY, &[]);
        let in_box = screen(HISTORY, &[TEXT]);
        let hits_before = screen_hits(&before, TEXT);
        assert_eq!(hits_before, 1, "舊的那一句本來就在畫面上");
        // 框空了但畫面沒有新增一次：證明不了，回 Unknown。
        assert_eq!(
            submit_outcome("claude", hits_before, &in_box, &before, TEXT),
            Delivered::Unknown("no_new_echo"),
        );
        let after = screen(&format!("{HISTORY}❯ {TEXT}\n"), &[]);
        assert_eq!(submit_outcome("claude", hits_before, &in_box, &after, TEXT), Delivered::Submitted);
    }

    /// Enter 之後字還在框裡、或框變成別人的字：都不算送出。
    #[test]
    fn text_left_in_the_box_or_replaced_is_never_counted_as_submitted() {
        let before = screen("⏺ 先前的回覆\n", &[]);
        let in_box = screen("⏺ 先前的回覆\n", &[TEXT]);
        assert_eq!(submit_outcome("claude", 0, &in_box, &in_box, TEXT), Delivered::Unknown("still_in_box"));
        let other = screen("⏺ 先前的回覆\n", &["/effort high"]);
        assert_eq!(submit_outcome("claude", 0, &in_box, &other, TEXT), Delivered::Unknown("composer_unreadable"));
        assert_eq!(submit_outcome("claude", 0, &in_box, &before, TEXT), Delivered::Unknown("no_new_echo"));
    }

    /// Tip／spinner 行不會被當成輸入框裡的字。
    #[test]
    fn a_tip_or_spinner_row_is_not_composer_text() {
        let spinner = screen("⏺ 先前的回覆\n✻ Crunching… (12s · esc to interrupt)\n", &[]);
        assert_eq!(box_state("claude", &spinner, TEXT), BoxState::Empty);
        assert_eq!(submit_outcome("claude", 0, &spinner, &spinner, TEXT), Delivered::Unknown("no_new_echo"));
    }

    /// 太短的 prompt 沒有可信的 needle：靠「框空了而且畫面變了」，畫面沒變就不算。
    #[test]
    fn a_prompt_too_short_to_count_needs_the_screen_to_change() {
        let short = "go";
        assert!(needles(short).is_none());
        let in_box = screen("⏺ 先前的回覆\n", &[short]);
        let after = screen("⏺ 先前的回覆\n❯ go\n⏺ 好\n", &[]);
        assert_eq!(submit_outcome("claude", 0, &in_box, &after, short), Delivered::Submitted);
        let unchanged = screen("⏺ 先前的回覆\n", &[]);
        assert_eq!(submit_outcome("claude", 0, &unchanged, &unchanged, short), Delivered::Unknown("no_new_echo"));
    }
}
