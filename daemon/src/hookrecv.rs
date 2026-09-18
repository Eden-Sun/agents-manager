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

/// 這句 prompt 回音是別的 agent 打進來的嗎？（SPEC §6.5d）
///
/// hook 只帶回音本身，說不出是誰打的。PATH 上的 herdr shim 在 `agent prompt` 轉發前會先向
/// `/relay/announce` 報一聲，所以這裡拿 run 的 agent 名字去認領；認不出來就回 `None`，那則訊息
/// 維持「使用者自己打的」——寧可少標一次，也不要冤枉一句話。
fn relay_source(run: Option<&db::Run>, echo: &str) -> Option<String> {
    let agent = run?.agent_name.as_deref()?;
    crate::agent_relay::claim(agent, echo)
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

/// What the run's status bar should say about the account: `(email, warning)`.
///
/// The identity's login state **on the bot's host** is the authority (`tools` probes each
/// host with `claude auth status`). Reading `.claude.json` off the daemon's own disk was wrong
/// twice over for a remote bot (issue #4): the wrong machine, and a file whose `oauthAccount`
/// is only metadata — on m4p `cc1` carried tony.lin's e-mail while the CLI, with no login in
/// that config dir, quietly fell back to the machine's legacy Keychain entry and ran as cc0.
/// That case is exactly the warning: identity set, host says not logged in.
async fn claude_account(app: &Arc<App>, bot: &db::Bot) -> (Option<String>, Option<String>) {
    let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
    let idn = bot.identity.as_deref().filter(|s| !s.is_empty());
    let info = {
        let tools = app.tools.lock().await;
        tools.get(&host).and_then(|t| idn.and_then(|n| t.identities.get(n)).or_else(|| t.identities.get("cc0")).cloned())
    };
    match (idn, info) {
        (Some(name), Some(i)) if i.logged_in == Some(false) => (
            None,
            Some(format!("身份 {name} 在 {host} 沒有登入：claude 會退回這台機器 Keychain 裡預設（cc0）的帳號執行。請在這個 Bot 按「登入 / 切換帳號」。")),
        ),
        (_, Some(i)) if i.logged_in == Some(true) && i.account.is_some() => (i.account, None),
        // No probe result for this host yet: fall back to the file, local only.
        (_, _) if host == crate::config::LOCAL_HOST => (claude_account_email_file(bot), None),
        _ => (None, None),
    }
}

/// The email claude is logged in as for this bot, read the way the user's own statusline
/// script does (`oauthAccount.emailAddress` in `.claude.json`). The identity decides which
/// config directory that is; `None` when it cannot be read. Local disk only.
fn claude_account_email_file(bot: &db::Bot) -> Option<String> {
    let home = dirs::home_dir()?;
    // `env_json` may pin CLAUDE_CONFIG_DIR (that is how cc0 / cc1 are kept apart).
    let cfg_dir = serde_json::from_str::<Value>(&bot.env_json)
        .ok()
        .and_then(|e| e.get("CLAUDE_CONFIG_DIR").and_then(|v| v.as_str()).map(String::from))
        .map(|d| {
            let d = d.replace("$HOME", &home.to_string_lossy());
            std::path::PathBuf::from(d)
        });
    // Default config dir keeps its json at ~/.claude.json; a custom one keeps it inside.
    let path = match cfg_dir {
        Some(d) => d.join(".claude.json"),
        None => home.join(".claude.json"),
    };
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let email = v.get("oauthAccount")?.get("emailAddress")?.as_str()?.trim().to_string();
    if email.is_empty() {
        None
    } else {
        Some(email)
    }
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

/// Consume the one-shot native session request written for a reopened Team member. Claude
/// reports its identity in `SessionStart`; Codex and Grok have no equivalent hook, so callers
/// pass their first completed turn instead. Clearing the column before recording a mismatch
/// makes retries idempotent and prevents duplicate timeline notes.
async fn consume_resume_session(
    app: &Arc<App>,
    bot: &db::Bot,
    run: &db::Run,
    reported_session_id: Option<&str>,
) -> Result<()> {
    let Some(expected) = run.resume_session_id.as_deref() else { return Ok(()) };
    // A hook that carries no session id proves nothing either way (codex's notify payload does
    // not always have `thread-id`). Leave the marker for the next hook rather than reading the
    // silence as "the CLI opened a new conversation".
    let Some(reported) = reported_session_id else { return Ok(()) };
    let mismatch = reported != expected;
    let consumed = sqlx::query("UPDATE runs SET resume_session_id = NULL WHERE id = ? AND resume_session_id IS NOT NULL")
        .bind(&run.id)
        .execute(&app.db)
        .await?;
    // A second hook may arrive with a stale in-memory `Run` snapshot. Only the request that
    // actually cleared the marker is allowed to record the mismatch note.
    if consumed.rows_affected() == 0 {
        return Ok(());
    }
    if mismatch {
        lifecycle::member_context_lost(app, bot, "resume_mismatch")
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    Ok(())
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
        // 真的答完一回合＝這個帳號又能跑了，把「撞上限」拿掉，不必等橫幅寫的那個時間
        // （券兌換、方案升級、或它自己提早恢復都算）。
        if matches!(&kind, HookKind::TurnComplete { assistant: Some(a), .. } if !a.trim().is_empty()) {
            let host = crate::db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| "local".to_string());
            crate::quota::clear_limit_hit(app, &host, "codex").await;
        }
    }

    match kind {
        HookKind::Ignore(reason) => {
            tracing::debug!(reason, "hook ignored");
            Ok(())
        }
        HookKind::StatusLine => {
            // The rendered status bar, for the chat header. Written only when it changed —
            // claude refreshes this line often and every write would wake every client.
            if let Some(r) = &run {
                let text = body.payload.get("status_line").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty());
                // The rest of the payload (context window, model, cost, limits) so the web
                // bar can be fuller than the pane's single line. `status_line` itself is
                // stored separately, so drop it from the copy.
                let mut rich = body.payload.clone();
                if let Some(o) = rich.as_object_mut() {
                    o.remove("status_line");
                    o.remove("hook_event_name");
                    let (email, warning) = claude_account(app, &bot).await;
                    if let Some(email) = email {
                        o.insert("account_email".into(), serde_json::json!(email));
                    }
                    if let Some(w) = warning {
                        o.insert("account_warning".into(), serde_json::json!(w));
                    }
                }
                let rich = serde_json::to_string(&rich).ok();
                let changed = text != r.status_line.as_deref() || rich != r.status_json;
                if changed {
                    let _ = sqlx::query("UPDATE runs SET status_line = COALESCE(?, status_line), status_json = ? WHERE id = ?")
                        .bind(text)
                        .bind(&rich)
                        .bind(&r.id)
                        .execute(&app.db)
                        .await;
                    app.emit_bot_status(&bot.id).await;
                }
            }
            // Quota only. Bots with an identity write `claude:<identity>` alone so they do not
            // overwrite the default-account `claude` row (cc0 / no-identity bots keep that key).
            // It is always stored under the bot's **host**: a remote bot reports the remote
            // account's limits, which must not land on the local row (SPEC §14).
            let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
            let identity = bot.identity.as_deref().filter(|s| !s.is_empty());
            if let Some(idn) = identity {
                if let Some(q) = crate::quota::quota_from_statusline(&body.payload, Some(idn)) {
                    // 空 env 的身份（cc0）跟預設帳號是同一組憑證，`/usage` 探測也是寫進裸的
                    // `claude`。這裡若另開一列 `claude:cc0`，那一列永遠沒有探測才讀得到的
                    // Fable 週窗，頂端那條就只有 cc0 少一條 F。同一個帳號寫同一個 key。
                    let default_account = crate::tools::identity_for_host(app, &host, idn)
                        .await
                        .is_some_and(|i| i.env.is_empty());
                    let key = if default_account { "claude".to_string() } else { format!("claude:{idn}") };
                    crate::quota::set(app, &host, &key, q).await;
                }
            } else if let Some(q) = crate::quota::quota_from_statusline(&body.payload, None) {
                crate::quota::set(app, &host, "claude", q).await;
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
            if let Some(r) = &run {
                // Claude reports the native id in SessionStart. Codex and Grok are checked on
                // their first completed turn instead; an identity-shaped hook from either
                // provider must not consume the one-shot request early.
                if body.provider == "claude" {
                    consume_resume_session(app, &bot, r, session_id.as_deref()).await?;
                }
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
            // Claude's SessionStart is authoritative for its resume check. Codex and Grok
            // identify the continued session on the first completed turn instead.
            if body.provider == "codex" || body.provider == "grok" {
                if let Some(r) = &run {
                    consume_resume_session(app, &bot, r, session_id.as_deref()).await?;
                }
            }
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
                            let from = relay_source(run.as_ref(), u);
                            lifecycle::insert_message_full(app, &conv, Some(&t.id), "user", u, "hook", false, None, None, from.as_deref()).await?;
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
                let from = relay_source(run.as_ref(), &u);
                lifecycle::insert_message_full(app, &conv, Some(&tid), "user", &u, "hook", false, None, None, from.as_deref()).await?;
            }
            if !body_text.is_empty() {
                lifecycle::insert_message(app, &conv, Some(&tid), "assistant", &body_text, "hook", false, None).await?;
            }
            lifecycle::emit_turn(app, &tid).await;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- remote drain (§11.4.3)

/// Separates the spool lines from the single-slot `hook-status.json` in one drain's output.
const STATUS_MARKER: &str = "---AM-STATUS---";

/// How long one bot's drains are merged into one ssh (§11.4.4).
const DRAIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// The event can beat the spool write; retry once, still ahead of the 5s terminal fallback.
const DRAIN_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// One scan per connected host, every 30s, for the status events that never arrived (§11.4.4).
const SCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Split a drain's stdout into the spool lines and the optional statusLine JSON.
fn parse_drain_output(text: &str) -> (Vec<&str>, Option<String>) {
    let mut lines = Vec::new();
    let mut status: Option<String> = None;
    let mut it = text.lines();
    for line in it.by_ref() {
        if line.trim() == STATUS_MARKER {
            status = Some(it.collect::<Vec<_>>().join("\n"));
            break;
        }
        let l = line.trim();
        if !l.is_empty() {
            lines.push(l);
        }
    }
    (lines, status.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

/// SPEC §11.4.3 — rename the remote spool aside, replay every line, and pick up the
/// statusLine slot file in the same ssh. Returns how many spool lines were replayed.
pub async fn drain_remote(app: &Arc<App>, host: &str, bot_id: &str) -> Result<usize> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let Some(conn) = app.hosts.get(host).await else { return Ok(0) };
    if !conn.is_connected() {
        return Ok(0);
    }
    let text = conn.ssh_exec(&drain_script(bot_id)).await?;
    let (lines, status) = parse_drain_output(&text);
    let mut n = 0usize;
    for line in lines {
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
    if let Some(raw) = status {
        match status_body(bot_id, &raw) {
            Some(b) => {
                if let Err(e) = process_locked(app, &b).await {
                    tracing::warn!(error = ?e, "remote statusline replay failed");
                }
            }
            None => tracing::warn!(bot_id, host, "unparseable remote hook-status.json; dropped"),
        }
    }
    if n > 0 {
        tracing::info!(bot_id, host, replayed = n, "remote hook spool replayed");
    }
    Ok(n)
}

/// The remote sh: spool → `.replaying` → stdout → gone, then the statusLine slot (§11.4.5).
fn drain_script(bot_id: &str) -> String {
    format!(
        "d=\"$HOME/.config/agents-manager/bots/{id}\"\n\
         f=\"$d/hook-spool.jsonl\"\n\
         if [ -f \"$f.replaying\" ]; then cat \"$f\" >> \"$f.replaying\" 2>/dev/null; rm -f \"$f\"; \
         elif [ -f \"$f\" ]; then mv \"$f\" \"$f.replaying\"; fi\n\
         if [ -f \"$f.replaying\" ]; then cat \"$f.replaying\"; rm -f \"$f.replaying\"; fi\n\
         s=\"$d/hook-status.json\"\n\
         if [ -f \"$s\" ]; then printf '\\n{marker}\\n'; cat \"$s\"; rm -f \"$s\"; fi\n",
        id = bot_id,
        marker = STATUS_MARKER
    )
}

/// The statusLine slot file as a `HookBody` for the existing `HookKind::StatusLine` branch.
fn status_body(bot_id: &str, raw: &str) -> Option<HookBody> {
    let mut payload: Value = serde_json::from_str(raw).ok()?;
    // hook.sh already stamps it; a hand-written or older file may not.
    if let Some(o) = payload.as_object_mut() {
        o.insert("hook_event_name".into(), json!("StatusLine"));
    } else {
        return None;
    }
    Some(HookBody {
        bot_id: bot_id.to_string(),
        provider: "claude".into(),
        payload,
        received_at: Some(db::now()),
        truncated: false,
    })
}

#[derive(Default)]
struct DrainGate {
    last: Option<std::time::Instant>,
    again: bool,
}

fn drain_gates() -> &'static std::sync::Mutex<std::collections::HashMap<String, DrainGate>> {
    static G: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, DrainGate>>> =
        std::sync::OnceLock::new();
    G.get_or_init(Default::default)
}

/// `true` when this trigger owns the next ssh; `false` when it was merged into the drain that
/// is still inside the window (which then runs once more on its way out).
fn gate_admit(g: &mut DrainGate, now: std::time::Instant) -> bool {
    if let Some(last) = g.last {
        if now.duration_since(last) < DRAIN_WINDOW {
            g.again = true;
            return false;
        }
    }
    g.last = Some(now);
    g.again = false;
    true
}

/// Take the "somebody asked again while we were draining" flag.
fn gate_take_again(g: &mut DrainGate) -> bool {
    std::mem::take(&mut g.again)
}

/// SPEC §11.4.4 — `drain_remote` with the 1s merge window and the T+2s empty-handed retry.
/// Awaited by `events::handle_status`; the follow-up runs on its own task.
pub async fn drain_remote_coalesced(app: &Arc<App>, host: &str, bot_id: &str) -> Result<usize> {
    {
        let mut g = drain_gates().lock().unwrap();
        let e = g.entry(bot_id.to_string()).or_default();
        if !gate_admit(e, std::time::Instant::now()) {
            tracing::debug!(bot_id, host, "drain merged into the one in the window");
            return Ok(0);
        }
    }
    let n = drain_remote(app, host, bot_id).await?;
    let again = {
        let mut g = drain_gates().lock().unwrap();
        gate_take_again(g.entry(bot_id.to_string()).or_default())
    };
    // A merged trigger is served right after the window; an empty drain means the hook may
    // still be writing its line (§11.4.4), so look once more before the fallback takes over.
    let delay = if again {
        Some(DRAIN_WINDOW)
    } else if n == 0 {
        Some(DRAIN_RETRY)
    } else {
        None
    };
    if let Some(d) = delay {
        let (app2, host2, bot2) = (app.clone(), host.to_string(), bot_id.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(d).await;
            if let Err(e) = drain_remote(&app2, &host2, &bot2).await {
                tracing::debug!(bot_id = %bot2, host = %host2, error = ?e, "follow-up drain failed");
            }
        });
    }
    Ok(n)
}

/// SPEC §11.4.4 — one ssh per connected host every 30s, listing the bot dirs that have
/// something to drain (a lost status event, or a host that had no `herdr` on PATH).
pub fn spawn_spool_scanner(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SCAN_EVERY).await;
            for conn in app.hosts.list().await {
                if conn.is_local() || !conn.is_connected() {
                    continue;
                }
                let pending = match conn.ssh_exec(SCAN_SCRIPT).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(host = %conn.name, error = ?e, "spool scan failed");
                        continue;
                    }
                };
                let ids: std::collections::HashSet<&str> =
                    pending.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
                if ids.is_empty() {
                    continue;
                }
                for b in db::live_bots_on_host(&app.db, &conn.name).await.unwrap_or_default() {
                    if !ids.contains(b.id.as_str()) {
                        continue;
                    }
                    if let Err(e) = drain_remote_coalesced(&app, &conn.name, &b.id).await {
                        tracing::debug!(bot = %b.name, host = %conn.name, error = ?e, "scanned drain failed");
                    }
                }
            }
        }
    });
}

/// Loops the bot dirs on the host and prints the ids that have spool or statusLine material.
const SCAN_SCRIPT: &str = "for d in \"$HOME/.config/agents-manager/bots\"/*/; do \
     [ -d \"$d\" ] || continue; b=$(basename \"$d\"); \
     if [ -f \"$d/hook-spool.jsonl\" ] || [ -f \"$d/hook-spool.jsonl.replaying\" ] || [ -f \"$d/hook-status.json\" ]; \
     then echo \"$b\"; fi; done\n";

/// SPEC §4.4.6: take the per-bot lock, rename the spool aside, replay each line, delete.
pub async fn replay_spool(app: &Arc<App>, bot_id: &str) -> Result<usize> {
    let host = db::bot_host(&app.db, bot_id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
    if host != crate::config::LOCAL_HOST {
        return drain_remote(app, &host, bot_id).await;
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
mod drain_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn spool_lines_and_the_status_slot_come_apart() {
        let out = "{\"bot_id\":\"b\"}\n{\"bot_id\":\"b\",\"provider\":\"claude\"}\n\n---AM-STATUS---\n{\n  \"session_id\": \"s\"\n}\n";
        let (lines, status) = parse_drain_output(out);
        assert_eq!(lines.len(), 2);
        assert_eq!(status.as_deref(), Some("{\n  \"session_id\": \"s\"\n}"));
    }

    /// No statusLine file: everything is spool, and nothing is mistaken for a status blob.
    #[test]
    fn output_without_the_marker_is_all_spool() {
        let (lines, status) = parse_drain_output("{\"bot_id\":\"b\"}\n");
        assert_eq!(lines, vec!["{\"bot_id\":\"b\"}"]);
        assert!(status.is_none());
        let (lines, status) = parse_drain_output("");
        assert!(lines.is_empty() && status.is_none());
    }

    /// An empty slot file must not turn into an unparseable statusline warning.
    #[test]
    fn an_empty_status_slot_is_no_status() {
        let (_, status) = parse_drain_output("---AM-STATUS---\n\n");
        assert!(status.is_none());
    }

    #[test]
    fn the_status_slot_becomes_a_claude_statusline_hook() {
        let b = status_body("bot1", "{\"session_id\":\"s\"}").expect("body");
        assert_eq!(b.provider, "claude");
        assert_eq!(b.bot_id, "bot1");
        assert!(matches!(classify(&b.provider, &b.payload), HookKind::StatusLine));
        assert!(status_body("bot1", "not json").is_none());
        // A JSON scalar is not a payload we can stamp.
        assert!(status_body("bot1", "3").is_none());
    }

    /// §11.4.4: the second `working -> idle` of the same turn rides on the first drain's ssh,
    /// and the first drain notices it has to look once more on its way out.
    #[test]
    fn a_second_trigger_inside_the_window_is_merged() {
        let mut g = DrainGate::default();
        let t0 = Instant::now();
        assert!(gate_admit(&mut g, t0));
        assert!(!gate_admit(&mut g, t0 + Duration::from_millis(200)));
        assert!(!gate_admit(&mut g, t0 + Duration::from_millis(900)));
        assert!(gate_take_again(&mut g));
        assert!(!gate_take_again(&mut g), "the flag is consumed once");
        // Past the window the next trigger opens its own ssh again.
        assert!(gate_admit(&mut g, t0 + DRAIN_WINDOW + Duration::from_millis(1)));
        assert!(!g.again);
    }

    #[test]
    fn the_drain_script_takes_the_spool_and_the_status_slot() {
        let s = drain_script("botX");
        assert!(s.contains("bots/botX"));
        assert!(s.contains("mv \"$f\" \"$f.replaying\""));
        assert!(s.contains("hook-status.json"));
        assert!(s.contains(STATUS_MARKER));
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
mod resume_tests {
    use super::*;
    use crate::team::testing::{env, make_team, req, Env};

    async fn fixture() -> (Env, db::Bot, db::Run) {
        let e = env().await;
        let team_id = make_team(&e.app, &e.project_id, req(Some(1), false)).await;
        let bot = db::team_members(&e.app.db, &team_id)
            .await
            .unwrap()
            .into_iter()
            .find(|b| b.team_role.as_deref() == Some("pm"))
            .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, started_at, resume_session_id)
             VALUES (?,?,'running','idle','ws-1',?,?,?)",
        )
        .bind(&run_id)
        .bind(&bot.id)
        .bind(format!("pane-{}", bot.id))
        .bind(db::now())
        .bind("native-expected")
        .execute(&e.app.db)
        .await
        .unwrap();
        let run = db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap();
        (e, bot, run)
    }

    /// A provider that reports exactly the requested native id consumes the marker without
    /// creating a `member_context_lost` event.
    #[tokio::test]
    async fn matching_resume_session_id_is_accepted_once() {
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, Some("native-expected"))
            .await
            .unwrap();
        let remaining: Option<String> = sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?")
            .bind(&run.id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(remaining, None);
        let lost: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM team_events WHERE team_id=? AND json_extract(payload_json,'$.action')='member_context_lost'",
        )
        .bind(bot.team_id.as_deref().unwrap())
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert_eq!(lost, 0);
    }

    /// A hook with no session id is not evidence of a new conversation. The marker survives so
    /// the next hook — the one that does carry an id — gets to decide.
    #[tokio::test]
    async fn a_hook_without_a_session_id_leaves_the_request_pending() {
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, None).await.unwrap();
        let remaining: Option<String> = sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?")
            .bind(&run.id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(remaining.as_deref(), Some("native-expected"));
        let lost: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM team_events WHERE team_id=? AND json_extract(payload_json,'$.action')='member_context_lost'",
        )
        .bind(bot.team_id.as_deref().unwrap())
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert_eq!(lost, 0);
    }

    /// A provider that ignores or misroutes the requested id is recorded once, and the user
    /// sees the same explanation in the member conversation.
    #[tokio::test]
    async fn mismatching_resume_session_id_records_context_loss() {
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, Some("native-other"))
            .await
            .unwrap();
        // A duplicate hook can still hold the pre-consumption snapshot; the SQL guard keeps
        // the one-shot mismatch note from appearing twice.
        consume_resume_session(&e.app, &bot, &run, Some("native-other"))
            .await
            .unwrap();
        let payload: String = sqlx::query_scalar(
            "SELECT payload_json FROM team_events WHERE team_id=? AND json_extract(payload_json,'$.action')='member_context_lost'",
        )
        .bind(bot.team_id.as_deref().unwrap())
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["why"], "resume_mismatch");
        let content: String = sqlx::query_scalar(
            "SELECT m.content FROM messages m JOIN conversations c ON c.id=m.conversation_id
             WHERE c.bot_id=? AND m.role='system' AND m.content='沒能續接先前對話，這是新的一段'",
        )
        .bind(&bot.id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert_eq!(content, "沒能續接先前對話，這是新的一段");
        let remaining: Option<String> = sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?")
            .bind(&run.id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(remaining, None);
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
