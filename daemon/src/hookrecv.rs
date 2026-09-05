//! Hook receiver: `POST /hook/{provider}` plus Turn matching (SPEC §6.7) and spool replay (§4.4.6).

use crate::db;
use crate::lifecycle;
use crate::state::App;
use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize)]
pub struct HookBody {
    pub bot_id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub received_at: Option<String>,
    #[serde(default)]
    pub truncated: bool,
}

/// Validate the per-bot token, enqueue, answer 200 immediately (SPEC §3.1).
pub async fn receive(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Json(body): Json<HookBody>,
) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let expected = match db::bot(&app.db, &body.bot_id).await {
        Ok(Some(b)) => b.hook_token,
        _ => {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unknown bot"})));
        }
    };
    if token.is_empty() || token != expected {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "bad token"})));
    }
    let provider = if body.provider.is_empty() { provider } else { body.provider.clone() };
    let app2 = app.clone();
    let mut b = body;
    b.provider = provider;
    tokio::spawn(async move {
        if let Err(e) = process(&app2, &b).await {
            tracing::error!(error = ?e, "hook processing failed");
        }
    });
    (StatusCode::OK, Json(json!({})))
}

#[derive(Debug)]
enum HookKind {
    /// Session / thread identity only — never creates a Turn.
    Identity { session_id: Option<String>, transcript_path: Option<String> },
    /// A completed turn.
    TurnComplete {
        session_id: Option<String>,
        turn_id: Option<String>,
        transcript_path: Option<String>,
        assistant: Option<String>,
        user: Option<String>,
    },
    Ignore(String),
}

fn classify(provider: &str, p: &Value) -> HookKind {
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).map(String::from);
    match provider {
        "claude" => {
            let ev = p
                .get("hook_event_name")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| if p.get("prompt_id").is_some() { "Stop".into() } else { "SessionStart".into() });
            match ev.as_str() {
                "SessionStart" => HookKind::Identity { session_id: s("session_id"), transcript_path: s("transcript_path") },
                "Stop" => {
                    if p.get("stop_hook_active").and_then(|v| v.as_bool()).unwrap_or(false) {
                        return HookKind::Ignore("stop_hook_active".into());
                    }
                    HookKind::TurnComplete {
                        session_id: s("session_id"),
                        turn_id: s("prompt_id"),
                        transcript_path: s("transcript_path"),
                        assistant: s("last_assistant_message"),
                        user: None,
                    }
                }
                other => HookKind::Ignore(other.to_string()),
            }
        }
        "codex" => {
            let ty = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ty != "agent-turn-complete" {
                return HookKind::Ignore(ty.to_string());
            }
            let user = p
                .get("input-messages")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("\n"))
                .filter(|s| !s.is_empty());
            HookKind::TurnComplete {
                session_id: s("thread-id"),
                turn_id: s("turn-id"),
                transcript_path: None,
                assistant: s("last-assistant-message"),
                user,
            }
        }
        other => HookKind::Ignore(format!("unknown provider {other}")),
    }
}

/// SPEC §6.7, executed under the per-bot lock.
pub async fn process(app: &Arc<App>, body: &HookBody) -> Result<()> {
    let lock = app.bot_lock(&body.bot_id).await;
    let _g = lock.lock().await;
    process_locked(app, body).await
}

pub async fn process_locked(app: &Arc<App>, body: &HookBody) -> Result<()> {
    let Some(bot) = db::bot(&app.db, &body.bot_id).await? else { return Ok(()) };
    let conv = db::conversation_id(&app.db, &bot.id).await?;
    let run = db::active_run(&app.db, &bot.id).await?;
    let kind = classify(&body.provider, &body.payload);
    tracing::info!(bot = %bot.name, provider = %body.provider, ?kind, "hook received");

    match kind {
        HookKind::Ignore(reason) => {
            tracing::debug!(reason, "hook ignored");
            Ok(())
        }
        HookKind::Identity { session_id, transcript_path } => {
            if let Some(r) = run {
                sqlx::query(
                    "UPDATE runs SET native_session_id = COALESCE(?, native_session_id),
                     transcript_path = COALESCE(?, transcript_path) WHERE id = ?",
                )
                .bind(&session_id)
                .bind(&transcript_path)
                .bind(&r.id)
                .execute(&app.db)
                .await?;
                app.emit_bot_status(&bot.id).await;
            }
            Ok(())
        }
        HookKind::TurnComplete { session_id, turn_id, transcript_path, assistant, user } => {
            // Backfill run identity opportunistically (Codex has no SessionStart equivalent).
            if let Some(r) = &run {
                sqlx::query(
                    "UPDATE runs SET native_session_id = COALESCE(native_session_id, ?),
                     transcript_path = COALESCE(?, transcript_path) WHERE id = ?",
                )
                .bind(&session_id)
                .bind(&transcript_path)
                .bind(&r.id)
                .execute(&app.db)
                .await?;
            }

            // 3. dedup on (native_session_id, native_turn_id)
            if let (Some(sid), Some(tid)) = (&session_id, &turn_id) {
                let dup: Option<String> =
                    sqlx::query_scalar("SELECT id FROM turns WHERE native_session_id=? AND native_turn_id=?")
                        .bind(sid)
                        .bind(tid)
                        .fetch_optional(&app.db)
                        .await?;
                if let Some(existing) = dup {
                    tracing::info!(turn = %existing, "duplicate hook ignored");
                    return Ok(());
                }
            }

            // 4. the run's single in-flight Turn
            let target = match &run {
                Some(r) => db::in_flight_turn(&app.db, &r.id).await?,
                None => None,
            };
            let body_text = assistant.clone().unwrap_or_default();

            if let Some(t) = target {
                sqlx::query(
                    "UPDATE turns SET status='completed', completed_at=?, native_session_id=?, native_turn_id=? WHERE id=? AND status='in_flight'",
                )
                .bind(db::now())
                .bind(&session_id)
                .bind(&turn_id)
                .bind(&t.id)
                .execute(&app.db)
                .await?;
                if !body_text.is_empty() {
                    lifecycle::insert_message(app, &conv, Some(&t.id), "assistant", &body_text, "hook", false, None).await?;
                }
                lifecycle::emit_turn(app, &t.id).await;
                // A hook that lands after a fallback already claimed the turn is dropped.
                return Ok(());
            }

            // §4.3: a hook that arrives after the terminal fallback already claimed the
            // turn must not overwrite it. Stamp the native ids onto that turn (so a retry
            // dedups) and drop the payload.
            if let Some(r) = &run {
                // Only a *recent* fallback counts as "this hook's turn"; timestamps are
                // fixed-width RFC3339 UTC so lexicographic comparison is chronological.
                let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(120))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                let late = sqlx::query_as::<_, db::Turn>(
                    "SELECT * FROM turns WHERE run_id=? AND status='completed_fallback' AND native_turn_id IS NULL
                     AND completed_at > ? ORDER BY created_at DESC LIMIT 1",
                )
                .bind(&r.id)
                .bind(&cutoff)
                .fetch_optional(&app.db)
                .await?;
                if let Some(t) = late {
                    sqlx::query("UPDATE turns SET native_session_id=?, native_turn_id=? WHERE id=?")
                        .bind(&session_id)
                        .bind(&turn_id)
                        .bind(&t.id)
                        .execute(&app.db)
                        .await?;
                    tracing::info!(turn = %t.id, "late hook dropped; turn already completed via terminal fallback");
                    return Ok(());
                }
            }

            // 5. external turn
            let tid = db::ulid();
            sqlx::query(
                "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, native_session_id, native_turn_id, created_at, completed_at)
                 VALUES (?,?,?,'external','completed','ok',?,?,?,?)",
            )
            .bind(&tid)
            .bind(&conv)
            .bind(run.as_ref().map(|r| r.id.clone()))
            .bind(&session_id)
            .bind(&turn_id)
            .bind(db::now())
            .bind(db::now())
            .execute(&app.db)
            .await?;
            if let Some(u) = user.filter(|s| !s.is_empty()) {
                lifecycle::insert_message(app, &conv, Some(&tid), "user", &u, "hook", false, None).await?;
            }
            if !body_text.is_empty() {
                lifecycle::insert_message(app, &conv, Some(&tid), "assistant", &body_text, "hook", false, None).await?;
            }
            lifecycle::emit_turn(app, &tid).await;
            Ok(())
        }
    }
}

/// SPEC §11.4 — the same rename-and-drain dance, but on a remote host over ssh.
async fn replay_spool_remote(app: &Arc<App>, bot_id: &str, host: &str) -> Result<usize> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let Some(conn) = app.hosts.get(host).await else { return Ok(0) };
    if !conn.is_connected() {
        return Ok(0);
    }
    let script = format!(
        "d=\"$HOME/.config/agents-manager/bots/{id}\"\nf=\"$d/hook-spool.jsonl\"\n         if [ -f \"$f.replaying\" ]; then cat \"$f\" >> \"$f.replaying\" 2>/dev/null; rm -f \"$f\";          elif [ -f \"$f\" ]; then mv \"$f\" \"$f.replaying\"; fi\n         if [ -f \"$f.replaying\" ]; then cat \"$f.replaying\"; rm -f \"$f.replaying\"; fi\n",
        id = bot_id
    );
    let text = conn.ssh_exec(&script).await?;
    let mut n = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<HookBody>(line) {
            Ok(b) => {
                if b.bot_id != bot_id {
                    tracing::warn!("remote spool line for a different bot; skipped");
                    continue;
                }
                if let Err(e) = process_locked(app, &b).await {
                    tracing::error!(error = ?e, "remote spool replay line failed");
                } else {
                    n += 1;
                }
            }
            Err(e) => tracing::warn!(error = %e, line, "unparseable remote spool line"),
        }
    }
    if n > 0 {
        tracing::info!(bot_id, host, replayed = n, "remote hook spool replayed");
    }
    Ok(n)
}

/// SPEC §4.4.6: take the per-bot lock, rename the spool aside, replay each line, delete.
pub async fn replay_spool(app: &Arc<App>, bot_id: &str) -> Result<usize> {
    let host = db::bot_host(&app.db, bot_id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
    if host != crate::config::LOCAL_HOST {
        return replay_spool_remote(app, bot_id, &host).await;
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let dir = app.bot_dir(bot_id);
    let spool = dir.join("hook-spool.jsonl");
    if !spool.exists() {
        return Ok(0);
    }
    let staging = dir.join("hook-spool.jsonl.replaying");
    // A leftover .replaying from a crash is merged back in first.
    if staging.exists() {
        let mut prev = std::fs::read_to_string(&staging).unwrap_or_default();
        prev.push_str(&std::fs::read_to_string(&spool).unwrap_or_default());
        std::fs::write(&staging, prev)?;
        std::fs::remove_file(&spool)?;
    } else {
        std::fs::rename(&spool, &staging)?;
    }
    let text = std::fs::read_to_string(&staging)?;
    let mut n = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<HookBody>(line) {
            Ok(b) => {
                if b.bot_id != bot_id {
                    tracing::warn!("spool line for a different bot; skipped");
                    continue;
                }
                if let Err(e) = process_locked(app, &b).await {
                    tracing::error!(error = ?e, "spool replay line failed");
                } else {
                    n += 1;
                }
            }
            Err(e) => tracing::warn!(error = %e, line, "unparseable spool line"),
        }
    }
    std::fs::remove_file(&staging).ok();
    if n > 0 {
        tracing::info!(bot_id, replayed = n, "hook spool replayed");
    }
    Ok(n)
}

#[allow(dead_code)]
pub async fn replay_all(app: &Arc<App>) {
    for host in app.hosts.names().await {
        replay_host(app, &host).await;
    }
}

/// Replay every bot's spool on one host (called after a host (re)connects).
pub async fn replay_host(app: &Arc<App>, host: &str) {
    for b in db::live_bots_on_host(&app.db, host).await.unwrap_or_default() {
        if let Err(e) = replay_spool(app, &b.id).await {
            tracing::warn!(bot = %b.name, host, error = ?e, "spool replay failed");
        }
    }
}
