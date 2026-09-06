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
        // A3: a deleted bot's surviving agent must not be able to create turns / messages.
        Ok(Some(b)) if b.deleted_at.is_some() => {
            return (StatusCode::GONE, Json(json!({"error": "bot deleted"})));
        }
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
    /// v4.0: Claude Code statusLine input — rate limits only, never a Turn.
    StatusLine,
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
                "StatusLine" => HookKind::StatusLine,
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
            // Codex runs a hidden follow-up turn after each reply to name the thread
            // ("Generate a concise, single-line task title …" → `{"title": …}`). It is not a
            // user-visible turn, so it must not become an external Turn in the timeline.
            let assistant = s("last-assistant-message");
            let is_title_turn = user.as_deref().map(|u| u.contains("single-line task title")).unwrap_or(false)
                || assistant
                    .as_deref()
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .map(|v| v.as_object().map(|o| o.len() == 1 && o.contains_key("title")).unwrap_or(false))
                    .unwrap_or(false);
            if is_title_turn {
                return HookKind::Ignore("codex title-generation turn".into());
            }
            HookKind::TurnComplete {
                session_id: s("thread-id"),
                turn_id: s("turn-id"),
                transcript_path: None,
                assistant,
                user,
            }
        }
        // SPEC §12 / appendix F: grok's stdin envelope. Keys come in camelCase (and, on 1.0.13,
        // a snake_case copy of some of them); we read the camelCase ones and fall back to snake.
        "grok" => {
            let either = |camel: &str, snake: &str| s(camel).or_else(|| s(snake));
            let ev = either("hookEventName", "hook_event_name").unwrap_or_default().to_lowercase();
            match ev.as_str() {
                "session_start" | "sessionstart" => HookKind::Identity {
                    session_id: either("sessionId", "session_id"),
                    transcript_path: either("transcriptPath", "transcript_path"),
                },
                "stop" => {
                    // A second, observe-only Stop fires at session end (`reason: shutdown`).
                    let reason = s("reason").unwrap_or_else(|| "end_turn".into());
                    if reason != "end_turn" {
                        return HookKind::Ignore(format!("stop reason {reason}"));
                    }
                    let active = p
                        .get("stopHookActive")
                        .or_else(|| p.get("stop_hook_active"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if active {
                        return HookKind::Ignore("stop_hook_active".into());
                    }
                    HookKind::TurnComplete {
                        session_id: either("sessionId", "session_id"),
                        turn_id: either("promptId", "prompt_id"),
                        transcript_path: either("transcriptPath", "transcript_path"),
                        assistant: either("lastAssistantMessage", "last_assistant_message"),
                        user: None,
                    }
                }
                other => HookKind::Ignore(other.to_string()),
            }
        }
        other => HookKind::Ignore(format!("unknown provider {other}")),
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    /// Captured from grok 1.0.13 (appendix F), trimmed.
    const GROK_STOP: &str = r#"{"hookEventName":"stop","sessionId":"01a072c2-9098-7d50-b3d1-f1750320ae28",
      "cwd":"/x","workspaceRoot":"/x","timestamp":"2026-09-05T18:09:13.833011+00:00",
      "transcriptPath":"/Users/me/.grok/sessions/%2Fx/01a072c2/updates.jsonl",
      "promptId":"089f03f9-594f-4e92-bfb0-5180eefaa250","permissionMode":"bypassPermissions",
      "reason":"end_turn","stopHookActive":false,"lastAssistantMessage":"GROK-OK",
      "backgroundTasks":[],"sessionCrons":[],"hook_event_name":"stop",
      "session_id":"01a072c2-9098-7d50-b3d1-f1750320ae28"}"#;

    #[test]
    fn grok_stop_is_a_turn() {
        let v: Value = serde_json::from_str(GROK_STOP).unwrap();
        match classify("grok", &v) {
            HookKind::TurnComplete { session_id, turn_id, transcript_path, assistant, user } => {
                assert_eq!(session_id.as_deref(), Some("01a072c2-9098-7d50-b3d1-f1750320ae28"));
                assert_eq!(turn_id.as_deref(), Some("089f03f9-594f-4e92-bfb0-5180eefaa250"));
                assert!(transcript_path.unwrap().ends_with("updates.jsonl"));
                assert_eq!(assistant.as_deref(), Some("GROK-OK"));
                assert!(user.is_none());
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
    }

    #[test]
    fn grok_session_end_stop_is_ignored() {
        let v = json!({"hookEventName":"stop","sessionId":"s","reason":"shutdown","stopHookActive":false});
        assert!(matches!(classify("grok", &v), HookKind::Ignore(_)));
        let v = json!({"hookEventName":"stop","sessionId":"s","reason":"end_turn","stopHookActive":true});
        assert!(matches!(classify("grok", &v), HookKind::Ignore(_)));
        let v = json!({"hookEventName":"session_end","sessionId":"s"});
        assert!(matches!(classify("grok", &v), HookKind::Ignore(_)));
    }

    #[test]
    fn grok_session_start_is_identity() {
        let v = json!({"hookEventName":"session_start","sessionId":"s1","source":"new"});
        match classify("grok", &v) {
            HookKind::Identity { session_id, transcript_path } => {
                assert_eq!(session_id.as_deref(), Some("s1"));
                assert!(transcript_path.is_none());
            }
            other => panic!("expected Identity, got {other:?}"),
        }
    }
}

/// SPEC §6.7, executed under the per-bot lock.
pub async fn process(app: &Arc<App>, body: &HookBody) -> Result<()> {
    let lock = app.bot_lock(&body.bot_id).await;
    let _g = lock.lock().await;
    process_locked(app, body).await
}

/// Whitespace collapsed, so a prompt echo the pane wrapped across columns still compares equal
/// to the hook's single-line copy of the same text.
fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Should the Stop hook's `user` payload be stored on a turn that already carries `existing`
/// user messages?
///
/// A turn opened by `lifecycle::begin_external_turn` already holds the prompt echo scraped off
/// the pane — the *same* text the hook reports, except the pane can wrap or clip it at the
/// column width. Comparing for equality alone would let a clipped echo through as a second
/// bubble, so a containment either way counts as the same message.
fn hook_user_is_new(existing: &[String], incoming: &str) -> bool {
    let inc = squash_ws(incoming);
    if inc.is_empty() {
        return false;
    }
    !existing.iter().any(|e| {
        let e = squash_ws(e);
        !e.is_empty() && (e.contains(&inc) || inc.contains(&e))
    })
}

pub async fn process_locked(app: &Arc<App>, body: &HookBody) -> Result<()> {
    let Some(bot) = db::bot(&app.db, &body.bot_id).await? else { return Ok(()) };
    // A3: also guards the spool-replay path, where nothing checked the token.
    if bot.deleted_at.is_some() {
        tracing::info!(bot = %bot.name, "hook for a deleted bot; ignored");
        return Ok(());
    }
    let conv = db::conversation_id(&app.db, &bot.id).await?;
    let run = db::active_run(&app.db, &bot.id).await?;
    let kind = classify(&body.provider, &body.payload);
    if matches!(kind, HookKind::StatusLine) {
        tracing::debug!(bot = %bot.name, "statusline received");
    } else {
        tracing::info!(bot = %bot.name, provider = %body.provider, ?kind, "hook received");
    }

    // Codex's usage-reset hint is a standalone TUI row, not `last-assistant-message`. Give the
    // pane a moment to render it after the notify hook, then capture it under the same bot lock.
    if body.provider == "codex" && matches!(&kind, HookKind::TurnComplete { .. }) {
        if let Some(r) = run.as_ref() {
            lifecycle::schedule_codex_notice_capture(app, &bot.id, &r.id);
        }
    }

    match kind {
        HookKind::Ignore(reason) => {
            tracing::debug!(reason, "hook ignored");
            Ok(())
        }
        HookKind::StatusLine => {
            // Quota only. Bots with an identity write `claude:<identity>` alone so they do not
            // overwrite the default-account `claude` row (cc0 / no-identity bots keep that key).
            let identity = bot.identity.as_deref().filter(|s| !s.is_empty());
            if let Some(idn) = identity {
                if let Some(q) = crate::quota::quota_from_statusline(&body.payload, Some(idn)) {
                    crate::quota::set(app, &format!("claude:{idn}"), q).await;
                }
            } else if let Some(q) = crate::quota::quota_from_statusline(&body.payload, None) {
                crate::quota::set(app, "claude", q).await;
            }
            // The statusLine also carries the session id — backfill it like SessionStart does.
            if let (Some(r), Some(sid)) = (&run, body.payload.get("session_id").and_then(|v| v.as_str())) {
                let _ = sqlx::query("UPDATE runs SET native_session_id = COALESCE(native_session_id, ?) WHERE id = ?")
                    .bind(sid)
                    .bind(&r.id)
                    .execute(&app.db)
                    .await;
            }
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
            // Backfill run identity opportunistically (Codex has no SessionStart equivalent;
            // grok's SessionStart carries no transcript path).
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
                // This turn may be one `begin_external_turn` opened when the user typed into the
                // pane — claim it instead of opening a second one at step 5. Its user message
                // was scraped off the prompt echo, so only store the hook's copy (codex sends
                // `input-messages`) when it is not the same text we already have. A `web` turn
                // always has its user message from the composer, so it never takes this branch.
                if t.origin == "external" {
                    if let Some(u) = user.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        let have = db::turn_user_messages(&app.db, &t.id).await?;
                        if hook_user_is_new(&have, u) {
                            lifecycle::insert_message(app, &conv, Some(&t.id), "user", u, "hook", false, None).await?;
                        }
                    }
                }
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
            // A4: the line is already removed from the remote spool, so this is a drop, not a
            // retry — say so once and move on.
            Err(e) => tracing::warn!(error = %e, line, "unparseable remote spool line; dropped"),
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


#[cfg(test)]
mod external_claim_tests {
    use super::*;

    /// The prompt echo scraped off the pane and the hook's own copy are the same message,
    /// however the pane wrapped or clipped it.
    #[test]
    fn hook_user_dedups_against_the_scraped_echo() {
        let echo = vec!["Reply with exactly MERGED-OK".to_string()];
        assert!(!hook_user_is_new(&echo, "Reply with exactly MERGED-OK"));
        // The pane wrapped the echo across two columns; the hook sends one line.
        assert!(!hook_user_is_new(&vec!["Reply with\n  exactly MERGED-OK".into()], "Reply with exactly MERGED-OK"));
        // The pane clipped the echo at the column width.
        assert!(!hook_user_is_new(&vec!["Reply with exactly MER".into()], "Reply with exactly MERGED-OK"));
    }

    /// …but a genuinely different prompt, or a turn that has no user message at all (the echo
    /// was off screen), must still be stored.
    #[test]
    fn hook_user_is_stored_when_it_is_not_the_echo() {
        assert!(hook_user_is_new(&[], "Reply with exactly MERGED-OK"));
        assert!(hook_user_is_new(&vec!["echo 1".into()], "echo 2"));
        // Nothing to store.
        assert!(!hook_user_is_new(&[], "   "));
    }

    /// A throwaway on-disk database (`db::open` needs a path) seeded with one running bot.
    /// The directory removes itself when the returned guard drops.
    struct TmpDb(std::path::PathBuf);
    impl Drop for TmpDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn fixture() -> (TmpDb, sqlx::SqlitePool, String, String) {
        let dir = std::env::temp_dir().join(format!("am-hookrecv-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::open(&dir.join("t.db")).await.unwrap();
        let now = db::now();
        for q in [
            "INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp/p','p',?)",
            "INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','tok',?)",
            "INSERT INTO conversations (id,bot_id,created_at) VALUES ('c','b',?)",
            "INSERT INTO runs (id,bot_id,state,agent_status,pane_id,started_at) VALUES ('r','b','running','working','%1',?)",
        ] {
            sqlx::query(q).bind(&now).execute(&pool).await.unwrap();
        }
        (TmpDb(dir), pool, "r".to_string(), "c".to_string())
    }

    /// The Stop hook must *claim* the in-flight turn `begin_external_turn` opened when the user
    /// typed into the pane, not open a second one. Step 4 of `process_locked` looks the target
    /// up with exactly this query, so an `external` / `in_flight` turn has to come back from it
    /// — otherwise the handler falls through to step 5 and inserts a duplicate.
    #[tokio::test]
    async fn stop_hook_finds_the_open_external_turn() {
        let (_tmp, pool, run, conv) = fixture().await;
        let now = db::now();
        sqlx::query(
            "INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at)
             VALUES ('t',?,?,'external','in_flight','ok',?)",
        )
        .bind(&conv)
        .bind(&run)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        let found = db::in_flight_turn(&pool, &run).await.unwrap().expect("step 4 must claim the external turn");
        assert_eq!(found.id, "t");
        assert_eq!(found.origin, "external");
        // `delivery` must be `ok` or the §4.3 terminal fallback refuses to close the turn.
        assert_eq!(found.delivery, "ok");

        // Claiming it is the same UPDATE a web turn gets; afterwards nothing is in flight, so a
        // retried hook dedups instead of opening a second turn.
        sqlx::query("UPDATE turns SET status='completed', completed_at=?, native_turn_id='u' WHERE id=? AND status='in_flight'")
            .bind(&now)
            .bind("t")
            .execute(&pool)
            .await
            .unwrap();
        assert!(db::in_flight_turn(&pool, &run).await.unwrap().is_none());
    }

    /// The scraped echo lives on the turn as a `hook`-sourced user message; that is what the
    /// dedup in step 4 compares the hook's `input-messages` against.
    #[tokio::test]
    async fn turn_user_messages_returns_the_scraped_echo() {
        let (_tmp, pool, run, conv) = fixture().await;
        let now = db::now();
        sqlx::query(
            "INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at)
             VALUES ('t',?,?,'external','in_flight','ok',?)",
        )
        .bind(&conv)
        .bind(&run)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at)
             VALUES ('m',?, 't','user','echo 1','hook',?)",
        )
        .bind(&conv)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        let have = db::turn_user_messages(&pool, "t").await.unwrap();
        assert_eq!(have, vec!["echo 1".to_string()]);
        assert!(!hook_user_is_new(&have, "echo 1"));
        assert!(hook_user_is_new(&have, "echo 2"));
    }
}

#[cfg(test)]
mod codex_title_tests {
    use super::*;

    #[test]
    fn codex_title_turn_is_ignored() {
        let p = serde_json::json!({
            "type": "agent-turn-complete", "thread-id": "t", "turn-id": "u",
            "input-messages": ["Generate a concise, single-line task title of at most 36 characters …"],
            "last-assistant-message": "{\"title\":\"Reply with MERGED-OK\"}"
        });
        assert!(matches!(classify("codex", &p), HookKind::Ignore(_)));
        let real = serde_json::json!({
            "type": "agent-turn-complete", "thread-id": "t", "turn-id": "v",
            "input-messages": ["Reply with exactly MERGED-OK"], "last-assistant-message": "MERGED-OK"
        });
        assert!(matches!(classify("codex", &real), HookKind::TurnComplete { .. }));
    }
}
