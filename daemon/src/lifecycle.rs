//! Bot / Run lifecycle: start, stop, interrupt, prompt, keys, terminal fallback (SPEC §6.2–§6.4, §4.3).
//!
//! Every public entry point takes the per-bot lock.

use crate::db;
use crate::herdr::{AgentStatus, HerdrError};
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub enum LcError {
    NotFound(String),
    Conflict(Value),
    Upstream(String),
    Bad(String),
}

impl LcError {
    pub fn conflict(reason: &str, extra: Value) -> Self {
        let mut o = json!({ "error": "conflict", "reason": reason });
        if let (Some(a), Some(b)) = (o.as_object_mut(), extra.as_object()) {
            for (k, v) in b {
                a.insert(k.clone(), v.clone());
            }
        }
        LcError::Conflict(o)
    }
}

pub type LcResult<T> = std::result::Result<T, LcError>;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

// ---------------------------------------------------------------- messages

pub async fn insert_message(
    app: &Arc<App>,
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
) -> anyhow::Result<db::Message> {
    let id = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, incomplete, terminal_snapshot, created_at)
         VALUES (?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(conversation_id)
    .bind(turn_id)
    .bind(role)
    .bind(content)
    .bind(source)
    .bind(incomplete as i64)
    .bind(snapshot)
    .bind(&now)
    .execute(&app.db)
    .await?;
    let m = sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE id = ?")
        .bind(&id)
        .fetch_one(&app.db)
        .await?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
        .bind(conversation_id)
        .fetch_one(&app.db)
        .await
        .unwrap_or_default();
    app.emit("message_added", json!({ "bot_id": bot_id, "message": m })).await;
    Ok(m)
}

pub async fn emit_turn(app: &Arc<App>, turn_id: &str) {
    if let Ok(Some(t)) = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
    {
        let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
            .bind(&t.conversation_id)
            .fetch_one(&app.db)
            .await
            .unwrap_or_default();
        app.emit("turn_updated", json!({ "bot_id": bot_id, "turn": t })).await;
    }
}

// ---------------------------------------------------------------- run state

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
        crate::events::unwatch_pane(app, p).await;
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

// ---------------------------------------------------------------- hook injection

fn hook_cmd_parts(app: &App, bot: &db::Bot, provider: &str) -> Vec<String> {
    vec![
        app.exe.to_string_lossy().to_string(),
        "hook".into(),
        provider.into(),
        "--bot".into(),
        bot.id.clone(),
        "--token".into(),
        bot.hook_token.clone(),
        "--port".into(),
        app.port.to_string(),
    ]
}

/// Returns the daemon-injected CLI args that go *before* the bot's own args.
fn injected_args(app: &App, bot: &db::Bot) -> anyhow::Result<Vec<String>> {
    if bot.inject_hooks == 0 {
        return Ok(vec![]);
    }
    let dir = app.bot_dir(&bot.id);
    std::fs::create_dir_all(&dir)?;
    match bot.kind.as_str() {
        "claude" => {
            let cmd = shell_join(&hook_cmd_parts(app, bot, "claude"));
            // v3: no Notification hook; Stop with stop_hook_active=true is ignored daemon-side.
            let settings = json!({
                "hooks": {
                    "SessionStart": [{"hooks": [{"type": "command", "command": cmd}]}],
                    "Stop": [{"hooks": [{"type": "command", "command": cmd}]}]
                }
            });
            let path = dir.join("claude-settings.json");
            std::fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
            Ok(vec!["--settings".into(), path.to_string_lossy().to_string()])
        }
        "codex" => {
            let parts = hook_cmd_parts(app, bot, "codex");
            let arr = serde_json::to_string(&parts)?;
            Ok(vec!["-c".into(), format!("notify={arr}")])
        }
        other => anyhow::bail!("unknown bot kind {other}"),
    }
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| if p.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=".contains(c)) { p.clone() } else { format!("'{}'", p.replace('\'', "'\\''")) })
        .collect::<Vec<_>>()
        .join(" ")
}

fn pane_env(app: &App, bot_id: &str, run_id: &str) -> Value {
    json!({
        "AM_BOT_ID": bot_id,
        "AM_RUN_ID": run_id,   // diagnostics only; hook identity is per-bot
        "AM_PORT": app.port.to_string(),
        "CLAUDE_CODE_CHILD_SESSION": "",
        "CLAUDECODE": "",
    })
}

// ---------------------------------------------------------------- start

pub async fn start_bot(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    start_bot_locked(app, bot_id).await
}

pub async fn start_bot_locked(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    if let Some(existing) = db::active_run(&app.db, bot_id).await.map_err(up)? {
        return Err(LcError::conflict("active run already exists", json!({"run_id": existing.id})));
    }
    let project = db::project(&app.db, &bot.project_id)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("project".into()))?;

    // 1. INSERT Run before touching herdr (SPEC §6.2.1).
    let run_id = db::ulid();
    let ins = sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, started_at) VALUES (?,?,'starting','unknown',?)")
        .bind(&run_id)
        .bind(bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await;
    if let Err(e) = ins {
        if let Some(dbe) = e.as_database_error() {
            if dbe.is_unique_violation() {
                let existing = db::active_run(&app.db, bot_id).await.map_err(up)?.map(|r| r.id);
                return Err(LcError::conflict("active run already exists", json!({ "run_id": existing })));
            }
        }
        return Err(up(e));
    }
    app.emit_bot_status(bot_id).await;

    match start_inner(app, &bot, &project, &run_id).await {
        Ok(()) => Ok(run_id),
        Err(e) => {
            let _ = sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE id=?")
                .bind(db::now())
                .bind(&run_id)
                .execute(&app.db)
                .await;
            app.emit_bot_status(bot_id).await;
            Err(e)
        }
    }
}

async fn start_inner(app: &Arc<App>, bot: &db::Bot, project: &db::Project, run_id: &str) -> LcResult<()> {
    let env = pane_env(app, &bot.id, run_id);

    // 2. workspace
    let mut fresh_root: Option<String> = None;
    let workspace_id = match project.workspace_id.as_deref() {
        Some(ws) if app.herdr.workspace_get(ws).await.map_err(up)?.is_some() => ws.to_string(),
        _ => {
            let (ws, root) =
                app.herdr.workspace_create(&project.path, &project.label, env.clone()).await.map_err(up)?;
            sqlx::query("UPDATE projects SET workspace_id = ? WHERE id = ?")
                .bind(&ws.workspace_id)
                .bind(&project.id)
                .execute(&app.db)
                .await
                .map_err(up)?;
            fresh_root = Some(root.pane_id.clone());
            ws.workspace_id
        }
    };

    // 3. pane
    let pane_id = match fresh_root {
        Some(p) => p,
        None => {
            let panes = app.herdr.pane_list(Some(&workspace_id)).await.map_err(up)?;
            let target = panes.first().map(|p| p.pane_id.clone()).ok_or_else(|| {
                LcError::Upstream(format!("workspace {workspace_id} has no panes to split"))
            })?;
            app.herdr
                .pane_split(&target, "right", &project.path, env.clone())
                .await
                .map_err(up)?
                .pane_id
        }
    };

    // 4. persist mapping + generate hook injection
    sqlx::query("UPDATE runs SET workspace_id = ?, pane_id = ? WHERE id = ?")
        .bind(&workspace_id)
        .bind(&pane_id)
        .bind(run_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    let injected = injected_args(app, bot).map_err(up)?;
    let mut args = injected;
    args.extend(bot.args());

    // 5. agent.start (async on the socket)
    if let Err(e) = app.herdr.agent_start(&bot.name, &bot.kind, &pane_id, &args, 60_000).await {
        let _ = app.herdr.pane_close(&pane_id).await;
        return Err(up(e));
    }

    // 6. per-run status subscription
    crate::events::watch_pane(app, &pane_id).await;

    // 7. wait for readiness
    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    match app.herdr.agent_wait(&bot.name, &until, 60_000).await {
        Ok(info) => {
            let st = info.agent_status.normalized();
            set_run(app, run_id, "running", st.as_str()).await;
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "agent.wait did not settle");
            // Do NOT close the pane on timeout (SPEC §6.2.7).
            match app.herdr.agent_get(&bot.name).await {
                Ok(Some(info)) => set_run(app, run_id, "running", info.agent_status.normalized().as_str()).await,
                _ => {
                    let _ = app.herdr.pane_close(&pane_id).await;
                    return Err(up(e));
                }
            }
        }
    }
    app.emit_bot_status(&bot.id).await;
    Ok(())
}

async fn set_run(app: &Arc<App>, run_id: &str, state: &str, agent_status: &str) {
    let _ = sqlx::query("UPDATE runs SET state = ?, agent_status = ? WHERE id = ?")
        .bind(state)
        .bind(agent_status)
        .bind(run_id)
        .execute(&app.db)
        .await;
}

// ---------------------------------------------------------------- stop / interrupt

pub async fn stop_bot(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? else { return Ok(false) };

    let _ = sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run.id).execute(&app.db).await;
    app.emit_bot_status(bot_id).await;
    fail_in_flight(app, &run.id, "run stopped by user").await;

    for _ in 0..2 {
        let _ = app.herdr.agent_send_keys(&bot.name, &["ctrl+c".to_string()]).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut gone = false;
    for _ in 0..20 {
        let agent = app.herdr.agent_get(&bot.name).await;
        let pane = match run.pane_id.as_deref() {
            Some(p) => app.herdr.pane_get(p).await.ok().flatten(),
            None => None,
        };
        if matches!(agent, Ok(None)) || pane.is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // The pane belongs to this Run either way: once the agent is gone (or refused to go)
    // we close it, otherwise a bare shell pane would linger until the next reconcile.
    if let Some(p) = run.pane_id.as_deref() {
        let _ = app.herdr.pane_close(p).await;
    }
    if !gone {
        tracing::warn!(bot = %bot.name, "agent did not exit within 10s; pane closed forcibly");
    }
    let _ = sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(db::now())
        .bind(&run.id)
        .execute(&app.db)
        .await;
    if let Some(p) = run.pane_id.as_deref() {
        crate::events::unwatch_pane(app, p).await;
    }
    app.emit_bot_status(bot_id).await;
    Ok(true)
}

pub async fn interrupt_bot(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    app.herdr.agent_send_keys(&bot.name, &["esc".to_string()]).await.map_err(up)?;
    fail_in_flight(app, &run.id, "interrupted by user").await;
    Ok(())
}

pub async fn send_keys(app: &Arc<App>, bot_id: &str, keys: Vec<String>, expect_run_id: Option<String>) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    if let Some(exp) = expect_run_id {
        if exp != run.id {
            return Err(LcError::conflict("run mismatch", json!({"run_id": run.id})));
        }
    }
    app.herdr.agent_send_keys(&bot.name, &keys).await.map_err(up)?;
    Ok(())
}

// ---------------------------------------------------------------- prompt

#[derive(serde::Serialize)]
pub struct PromptOut {
    pub turn_id: String,
    pub message_id: String,
    pub delivery: String,
}

pub async fn prompt(app: &Arc<App>, bot_id: &str, text: &str, client_request_id: &str) -> LcResult<PromptOut> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;

    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;

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
    if let Some(t) = sqlx::query_as::<_, db::Turn>(
        "SELECT * FROM turns WHERE conversation_id=? AND delivery='unknown' AND status IN ('in_flight','completed','completed_fallback') LIMIT 1",
    )
    .bind(&conv)
    .fetch_optional(&app.db)
    .await
    .map_err(up)?
    {
        return Err(LcError::conflict("a previous turn has unknown delivery; abandon it first", json!({"turn_id": t.id})));
    }

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
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user',?,'web',?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(text)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;

    if let Ok(Some(m)) = sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE id=?")
        .bind(&msg_id)
        .fetch_optional(&app.db)
        .await
    {
        app.emit("message_added", json!({"bot_id": bot_id, "message": m})).await;
    }
    emit_turn(app, &turn_id).await;

    // 4. deliver
    let res = app
        .herdr
        .call_timeout("agent.prompt", json!({"target": bot.name, "text": text}), Duration::from_secs(10))
        .await;
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
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: delivery.into() })
}

pub async fn abandon_turn(app: &Arc<App>, turn_id: &str) -> LcResult<()> {
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id=?")
        .bind(&t.conversation_id)
        .fetch_one(&app.db)
        .await
        .map_err(up)?;
    let lock = app.bot_lock(&bot_id).await;
    let _g = lock.lock().await;
    if t.status != "in_flight" && t.delivery != "unknown" {
        return Err(LcError::conflict("turn is neither in-flight nor of unknown delivery", json!({"turn_id": t.id})));
    }
    sqlx::query("UPDATE turns SET status='failed', delivery = CASE WHEN delivery='unknown' THEN 'failed' ELSE delivery END, completed_at=? WHERE id=?")
        .bind(db::now())
        .bind(turn_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    let _ = insert_message(app, &t.conversation_id, Some(turn_id), "system", "turn abandoned by user", "system", false, None).await;
    emit_turn(app, turn_id).await;
    Ok(())
}

// ---------------------------------------------------------------- terminal fallback (§4.3)

/// Arm the 5s terminal-fallback timer after a working -> idle transition.
pub async fn arm_fallback(app: &Arc<App>, run_id: &str, bot_id: &str) {
    let mut timers = app.fallback_timers.lock().await;
    if let Some(h) = timers.remove(run_id) {
        h.abort();
    }
    let app2 = app.clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    let key = run_id.clone();
    let h = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let lock = app2.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if let Err(e) = try_fallback(&app2, &run_id).await {
            tracing::warn!(error = ?e, "terminal fallback failed");
        }
        app2.fallback_timers.lock().await.remove(&run_id);
    });
    timers.insert(key, h);
}

async fn try_fallback(app: &Arc<App>, run_id: &str) -> anyhow::Result<()> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(()) };
    let Some(turn) = db::in_flight_turn(&app.db, run_id).await? else { return Ok(()) };
    if turn.delivery != "ok" {
        return Ok(());
    }
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(()) };

    // CAS: only one writer wins the turn.
    let res = sqlx::query("UPDATE turns SET status='completed_fallback', completed_at=? WHERE id=? AND status='in_flight'")
        .bind(db::now())
        .bind(&turn.id)
        .execute(&app.db)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(());
    }
    tracing::info!(turn = %turn.id, "terminal fallback engaged");

    let pane_id = run.pane_id.clone().unwrap_or_default();
    let read = app.herdr.pane_read(&pane_id, "recent_unwrapped", 200).await?;
    let fresh = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());
    let reply = extract_reply(&bot.kind, &fresh).unwrap_or_else(|| fresh.trim().to_string());

    sqlx::query("UPDATE runs SET last_read_revision=?, last_read_tail_hash=? WHERE id=?")
        .bind(read.revision as i64)
        .bind(tail_hash(&read.text))
        .bind(run_id)
        .execute(&app.db)
        .await?;

    insert_message(
        app,
        &turn.conversation_id,
        Some(&turn.id),
        "assistant",
        &reply,
        "terminal_fallback",
        true,
        Some(&read.text),
    )
    .await?;
    emit_turn(app, &turn.id).await;
    Ok(())
}

fn tail_hash(text: &str) -> String {
    let tail: String = text.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{:x}", md5ish(&tail))
}

/// Tiny non-cryptographic digest — enough to detect "have I already seen this tail".
fn md5ish(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn slice_after_cursor(text: &str, prev_tail_hash: Option<&str>) -> String {
    let Some(prev) = prev_tail_hash else { return text.to_string() };
    // Walk suffix boundaries looking for the previously recorded tail.
    let chars: Vec<char> = text.chars().collect();
    for end in (0..=chars.len()).rev() {
        let start = end.saturating_sub(400);
        let window: String = chars[start..end].iter().collect();
        if format!("{:x}", md5ish(&window)) == prev {
            return chars[end..].iter().collect();
        }
    }
    text.to_string()
}

/// Provider-specific reply extraction from a terminal snapshot.
fn extract_reply(kind: &str, text: &str) -> Option<String> {
    let marker = match kind {
        "claude" => "⏺ ",
        "codex" => "• ",
        _ => return None,
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().rposition(|l| l.trim_start().starts_with(marker))?;
    let mut out: Vec<String> = Vec::new();
    for line in &lines[start..] {
        let t = line.trim_end();
        // Stop at the input box / status bar drawn below the transcript.
        if t.trim_start().starts_with('╭') || t.trim_start().starts_with('│') || t.trim_start().starts_with('╰') {
            break;
        }
        let cleaned = t.trim_start().strip_prefix(marker).unwrap_or(t).to_string();
        out.push(cleaned);
    }
    let joined = out.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}
