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


/// Where our typed prompt is on the screen after typing / after Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypedState {
    /// Still in the input box (typed, not submitted).
    InBox,
    /// Out of the box and visible above it: submitted (or queued by the TUI).
    LeftBox,
    /// Nowhere on screen: the keystrokes never landed, or the TUI threw them away.
    Missing,
}

/// Pure: classify one screen for `text`. Head match on whitespace-squashed text, same rule as the
/// Enter nudge, so wrapping and `[Image #n]` prefixes do not matter.
pub(crate) fn typed_state(kind: &str, screen: &str, text: &str) -> TypedState {
    if composer_holds_prompt(kind, screen, text) {
        return TypedState::InBox;
    }
    if prompt_never_reached_screen(kind, screen, &[text.to_string()]) {
        return TypedState::Missing;
    }
    TypedState::LeftBox
}

/// Pause after typing before the box is read (the TUI has to draw the paste).
const TYPE_SETTLE_MS: u64 = 700;
/// Pause after Enter before checking the prompt left the box.
const SUBMIT_SETTLE_MS: u64 = 1500;

/// Deliver a prompt to the agent.
///
/// Normal runs: herdr `agent.prompt` (bracketed paste + Enter, `agent_blocked` refusal).
/// Runs whose pane the daemon has typed a slash command into ([`pane_typed`]): herdr's
/// `agent.prompt` returned ok twice on such a pane without the text ever appearing
/// (2026-09-14 wits-c1-op-xh, 14:24 and 15:33 — the second two minutes after `/effort`), while
/// `pane.send_text` + Enter into the same pane always worked. Those runs, and every re-delivery
/// after a stall (`force_pane`), type into the pane and **look**: the text must reach the box, and
/// must leave it after Enter. Each step is retried once; what still cannot be verified is logged
/// and left to the stall watchdog, never typed a third time.
pub(crate) async fn deliver_prompt(client: &HerdrClient, run: &db::Run, bot: &db::Bot, text: &str, force_pane: bool) -> anyhow::Result<()> {
    let pane = run.pane_id.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let Some(pane) = pane.filter(|_| force_pane || pane_typed(&run.id)) else {
        client
            .call_timeout("agent.prompt", json!({"target": db::run_target(run, bot), "text": text}), Duration::from_secs(10))
            .await?;
        return Ok(());
    };
    let read = |client: &HerdrClient| {
        let client = client.clone();
        let pane = pane.to_string();
        async move { client.pane_read(&pane, "visible", 80).await.map(|r| r.text).unwrap_or_default() }
    };

    // 1. Type, and see it in the box. Missing → type once more.
    client.pane_send_text(pane, text).await?;
    tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
    let mut state = typed_state(&bot.kind, &read(client).await, text);
    if state == TypedState::Missing {
        tracing::warn!(run = %run.id, bot = %bot.name, "typed prompt did not show up in the composer; typing it again");
        client.pane_send_text(pane, text).await?;
        tokio::time::sleep(Duration::from_millis(TYPE_SETTLE_MS)).await;
        state = typed_state(&bot.kind, &read(client).await, text);
    }
    if state == TypedState::LeftBox {
        return Ok(());
    }
    if state == TypedState::Missing {
        tracing::warn!(run = %run.id, bot = %bot.name, "prompt still not in the composer after typing twice; leaving it to the stall watchdog");
        return Ok(());
    }

    // 2. Submit, and see it leave the box. Still there → one more Enter.
    client.pane_send_keys(pane, &["Enter"]).await?;
    tokio::time::sleep(Duration::from_millis(SUBMIT_SETTLE_MS)).await;
    match typed_state(&bot.kind, &read(client).await, text) {
        TypedState::LeftBox => {
            tracing::info!(run = %run.id, bot = %bot.name, "prompt typed into the pane and verified on screen");
        }
        TypedState::InBox => {
            tracing::warn!(run = %run.id, bot = %bot.name, "prompt still in the composer after Enter; pressing Enter again");
            client.pane_send_keys(pane, &["Enter"]).await?;
        }
        TypedState::Missing => {
            tracing::warn!(run = %run.id, bot = %bot.name, "prompt vanished on Enter without an echo; leaving it to the stall watchdog");
        }
    }
    Ok(())
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
    let res = deliver_prompt(&client, &run, &bot, &deliver, false).await;
    let delivery = match res {
        Ok(_) => "ok",
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

