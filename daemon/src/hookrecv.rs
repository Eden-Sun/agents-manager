//! Hook receiver: `POST /hook/{provider}` plus Turn matching (SPEC §6.7) and spool replay (§4.4.6).

use crate::db;
use crate::config::{valid_id, ID_RE};
use crate::lifecycle;
use crate::hosts::sh_quote;
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

/// 這句 prompt 回音是別的 agent 打進來的嗎？（見 SPEC §6.5d）
/// 認不出來就當使用者自己打的——寧可少標一次，也不要冤枉一句話。
fn relay_source(run: Option<&db::Run>, echo: &str) -> Option<String> {
    let agent = run?.agent_name.as_deref()?;
    crate::agent_relay::claim(agent, echo)
}

#[derive(Debug)]
enum HookKind {
    /// Never creates a Turn.
    Identity { session_id: Option<String>, transcript_path: Option<String> },
    TurnComplete {
        session_id: Option<String>,
        turn_id: Option<String>,
        transcript_path: Option<String>,
        assistant: Option<String>,
        user: Option<String>,
    },
    /// Claude Code statusLine input — never a Turn.
    StatusLine,
    Ignore(String),
}

const DEFAULT_CLAUDE_IDENTITY: &str = "cc0";

/// `(email, warning)` from the identity's login state **on the bot's host**, not the daemon's
/// `.claude.json` (issue #4: wrong machine, and a logged-out cc1 silently ran as cc0).
fn claude_account_from_tools(
    tools: &std::collections::HashMap<String, crate::tools::HostTools>,
    host: &str,
    identity: Option<&str>,
) -> (Option<String>, Option<String>) {
    let info = tools.get(host).and_then(|t| t.identities.get(identity.unwrap_or(DEFAULT_CLAUDE_IDENTITY)));
    match (identity, info) {
        (Some(name), Some(i)) if i.logged_in == Some(false) => (
            None,
            Some(format!("身份 {name} 在 {host} 沒有登入：claude 會退回這台機器 Keychain 裡預設（cc0）的帳號執行。請在這個 Bot 按「登入 / 切換帳號」。")),
        ),
        (_, Some(i)) if i.logged_in != Some(false) && i.account.is_some() => (i.account.clone(), None),
        _ => (None, None),
    }
}

async fn claude_account(app: &Arc<App>, host: &str, identity: Option<&str>) -> (Option<String>, Option<String>) {
    let tools = app.tools.lock().await;
    claude_account_from_tools(&tools, host, identity)
}

#[cfg(test)]
mod account_tests {
    use super::*;

    fn identity(name: &str, account: &str) -> crate::tools::IdentityInfo {
        crate::tools::IdentityInfo {
            name: name.to_string(),
            kind: "claude".to_string(),
            logged_in: Some(true),
            reason: None,
            account: Some(account.to_string()),
            plan: None,
            source: crate::tools::SOURCE_CONFIG,
            config_dir: None,
        }
    }

    fn host_tools(identity: crate::tools::IdentityInfo) -> crate::tools::HostTools {
        crate::tools::HostTools {
            tools: std::collections::BTreeMap::new(),
            identities: [(identity.name.clone(), identity)].into_iter().collect(),
            shell_identities: Vec::new(),
            checked_at: String::new(),
        }
    }

    #[test]
    fn remote_bot_uses_remote_identity_account_not_local_account() {
        let mut tools = std::collections::HashMap::new();
        tools.insert("local".to_string(), host_tools(identity("cc1", "local@example.com")));
        tools.insert("remote".to_string(), host_tools(identity("cc1", "remote@example.com")));

        let (account, warning) = claude_account_from_tools(&tools, "remote", Some("cc1"));

        assert_eq!(account.as_deref(), Some("remote@example.com"));
        assert_eq!(warning, None);
    }

    #[test]
    fn local_identity_probe_miss_does_not_fall_back_to_default_account() {
        let mut tools = std::collections::HashMap::new();
        tools.insert("local".to_string(), host_tools(identity("cc0", "stale@example.com")));

        let (account, warning) = claude_account_from_tools(&tools, "local", Some("cc1"));

        assert_eq!(account, None);
        assert_eq!(warning, None);
    }

    #[test]
    fn default_bot_does_not_show_stale_account_when_host_says_logged_out() {
        let mut default_identity = identity("cc0", "stale@example.com");
        default_identity.logged_in = Some(false);
        let mut tools = std::collections::HashMap::new();
        tools.insert("local".to_string(), host_tools(default_identity));

        let (account, warning) = claude_account_from_tools(&tools, "local", None);

        assert_eq!(account, None);
        assert_eq!(warning, None);
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
            // Codex runs a hidden title-generation turn after each reply; not user-visible.
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
        // SPEC §12 / appendix F: camelCase keys, grok 1.0.13 also sends some snake_case copies.
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

/// A pane-wrapped echo must still compare equal to the hook's single-line copy.
fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The scraped echo may be wrapped or clipped at the column width, so containment either way
/// counts as the same message (equality alone would add a second bubble).
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

/// 只在既有那則是原文的（去空白）前綴且較短時才覆蓋；不是前綴就是另一句話，不能動。
async fn upgrade_clipped_user_message(app: &Arc<App>, turn_id: &str, full: &str) -> Result<()> {
    let full_sq = squash_ws(full);
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, content FROM messages WHERE turn_id = ? AND role = 'user' ORDER BY id")
            .bind(turn_id)
            .fetch_all(&app.db)
            .await?;
    for (id, content) in rows {
        let have = squash_ws(&content);
        if have.is_empty() || have.len() >= full_sq.len() || !full_sq.starts_with(&have) {
            continue;
        }
        sqlx::query("UPDATE messages SET content = ?, source = 'hook', updated_at = ? WHERE id = ?")
            .bind(full)
            .bind(db::now())
            .bind(&id)
            .execute(&app.db)
            .await?;
        tracing::info!(turn = %turn_id, msg = %id, "prompt 回音被截斷，用 hook 的原文補完");
        return Ok(());
    }
    Ok(())
}

/// 遲到的 hook 撞上備援關掉的回合：沒有 assistant 訊息就用 hook 的回覆補上並改 `completed`，
/// 已有回覆才丟（防一回合兩則）。2026-09-13 GROK 備援 15 秒就關回合、36 秒後的真回覆被丟。
/// 不會重開 c1526f7 的洞：`try_fallback` 認領與寫回覆同一交易、同一把 bot lock，讀到零則就真的是零則。
async fn fill_or_drop_late_hook(
    app: &Arc<App>,
    turn: &db::Turn,
    body_text: &str,
    session_id: &Option<String>,
    native_turn_id: &Option<String>,
) -> Result<()> {
    sqlx::query(
        "UPDATE turns SET native_session_id=COALESCE(?, native_session_id),
                          native_turn_id=COALESCE(?, native_turn_id) WHERE id=?",
    )
    .bind(session_id)
    .bind(native_turn_id)
    .bind(&turn.id)
    .execute(&app.db)
    .await?;
    let has_reply: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn.id)
            .fetch_one(&app.db)
            .await?;
    if body_text.trim().is_empty() || has_reply > 0 {
        tracing::info!(turn = %turn.id, has_reply, "late hook dropped; turn already completed via terminal fallback");
        return Ok(());
    }
    sqlx::query("UPDATE turns SET status='completed', completed_at=COALESCE(completed_at, ?) WHERE id=?")
        .bind(db::now())
        .bind(&turn.id)
        .execute(&app.db)
        .await?;
    lifecycle::insert_message(app, &turn.conversation_id, Some(&turn.id), "assistant", body_text, "hook", false, None)
        .await?;
    tracing::info!(turn = %turn.id, "late hook filled a fallback-closed turn that had no reply");
    lifecycle::emit_turn(app, &turn.id).await;
    Ok(())
}

/// Consume the one-shot `resume_native` request. Clearing the column before recording a mismatch
/// makes retries idempotent.
async fn consume_resume_session(
    app: &Arc<App>,
    bot: &db::Bot,
    run: &db::Run,
    reported_session_id: Option<&str>,
) -> Result<()> {
    let Some(expected) = run.resume_session_id.as_deref() else { return Ok(()) };
    // No session id proves nothing (codex notify may lack `thread-id`); leave the marker for the next hook.
    let Some(reported) = reported_session_id else { return Ok(()) };
    let mismatch = reported != expected;
    let consumed = sqlx::query("UPDATE runs SET resume_session_id = NULL WHERE id = ? AND resume_session_id IS NOT NULL")
        .bind(&run.id)
        .execute(&app.db)
        .await?;
    // A second hook may hold a stale `Run` snapshot; only the one that cleared the marker records a mismatch.
    if consumed.rows_affected() == 0 {
        return Ok(());
    }
    if mismatch {
        lifecycle::context_lost(app, bot, "resume_mismatch")
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

    // Codex's usage-reset hint is a TUI row, not in the payload; give the pane a moment to render it.
    if body.provider == "codex" && matches!(&kind, HookKind::TurnComplete { .. }) {
        if let Some(r) = run.as_ref() {
            lifecycle::schedule_codex_notice_capture(app, &bot.id, &r.id);
        }
        // 真的答完一回合＝帳號又能跑了，不必等橫幅寫的重置時間。
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
            let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
            // Written only when changed — claude refreshes often and every write wakes every client.
            if let Some(r) = &run {
                let text = body.payload.get("status_line").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty());
                let mut rich = body.payload.clone();
                if let Some(o) = rich.as_object_mut() {
                    o.remove("status_line");
                    o.remove("hook_event_name");
                    let identity = bot.identity.as_deref().filter(|s| !s.is_empty());
                    let (email, warning) = claude_account(app, &host, identity).await;
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
            // Always keyed under the bot's **host**: remote limits must not land on the local row (SPEC §14).
            let identity = bot.identity.as_deref().filter(|s| !s.is_empty());
            if let Some(idn) = identity {
                if let Some(q) = crate::quota::quota_from_statusline(&body.payload, Some(idn)) {
                    // 空 env 身份（cc0）就是預設帳號：另開 `claude:cc0` 會少掉 `/usage` 探測的 Fable 週窗。
                    let default_account = crate::tools::identity_for_host(app, &host, idn)
                        .await
                        .is_some_and(|i| i.env.is_empty());
                    let key = if default_account { "claude".to_string() } else { format!("claude:{idn}") };
                    crate::quota::set(app, &host, &key, q).await;
                }
            } else if let Some(q) = crate::quota::quota_from_statusline(&body.payload, None) {
                crate::quota::set(app, &host, "claude", q).await;
            }
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
                // Codex/Grok are checked on their first completed turn; don't consume the request early.
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
            if body.provider == "codex" || body.provider == "grok" {
                if let Some(r) = &run {
                    consume_resume_session(app, &bot, r, session_id.as_deref()).await?;
                }
            }
            // Codex has no SessionStart; grok's carries no transcript path.
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
                let claimed = sqlx::query(
                    "UPDATE turns SET status='completed', delivery=CASE WHEN delivery='unknown' THEN 'ok' ELSE delivery END,
                     completed_at=?, native_session_id=?, native_turn_id=? WHERE id=? AND status='in_flight'",
                )
                .bind(db::now())
                .bind(&session_id)
                .bind(&turn_id)
                .bind(&t.id)
                .execute(&app.db)
                .await?;
                // Lost the CAS to the §4.3 fallback: adding the hook's copy made two replies
                // (review 2026-09-12 #5). Stopped/failed turns still keep the reply.
                if claimed.rows_affected() == 0 {
                    let now_t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
                        .bind(&t.id)
                        .fetch_optional(&app.db)
                        .await?;
                    if let Some(closed) = now_t.filter(|c| c.status == "completed_fallback") {
                        fill_or_drop_late_hook(app, &closed, &body_text, &session_id, &turn_id).await?;
                        return Ok(());
                    }
                }
                // `begin_external_turn` already stored the scraped echo; add the hook's copy only if different.
                if t.origin == "external" {
                    if let Some(u) = user.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        let have = db::turn_user_messages(&app.db, &t.id).await?;
                        if hook_user_is_new(&have, u) {
                            let from = relay_source(run.as_ref(), u);
                            lifecycle::insert_message_full(app, &conv, Some(&t.id), "user", u, "hook", false, None, None, from.as_deref()).await?;
                        } else {
                            // 刮下來的回音可能被折行截斷；hook 的原文較可信，補完下半截。
                            upgrade_clipped_user_message(app, &t.id, u).await?;
                        }
                    }
                }
                if !body_text.is_empty() {
                    lifecycle::insert_message(app, &conv, Some(&t.id), "assistant", &body_text, "hook", false, None).await?;
                }
                lifecycle::emit_turn(app, &t.id).await;
                return Ok(());
            }

            // §4.3: a late hook must not overwrite a fallback-claimed turn.
            if let Some(r) = &run {
                // Fixed-width RFC3339 UTC, so lexicographic comparison is chronological.
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
                    fill_or_drop_late_hook(app, &t, &body_text, &session_id, &turn_id).await?;
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

// Remote drain: see SPEC §11.4.3–§11.4.5.

const STATUS_MARKER: &str = "---AM-STATUS---";

const DRAIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// The event can beat the spool write; retry once, still ahead of the 5s terminal fallback.
const DRAIN_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Catches status events that never arrived (§11.4.4).
const SCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

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

/// SPEC §11.4.3.
pub async fn drain_remote(app: &Arc<App>, host: &str, bot_id: &str) -> Result<usize> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let Some(conn) = app.hosts.get(host).await else { return Ok(0) };
    if !conn.is_connected() {
        return Ok(0);
    }
    let text = conn.ssh_exec(&drain_script(bot_id)?).await?;
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
            // A4: already removed from the remote spool, so this is a drop, not a retry.
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

/// §11.4.5.
fn drain_script(bot_id: &str) -> Result<String> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    let dir = format!("\"$HOME/.config/agents-manager/bots/\"{}", sh_quote(bot_id));
    Ok(format!(
        "d={dir}\n\
         f=\"$d/hook-spool.jsonl\"\n\
         if [ -f \"$f.replaying\" ]; then cat \"$f\" >> \"$f.replaying\" 2>/dev/null; rm -f \"$f\"; \
         elif [ -f \"$f\" ]; then mv \"$f\" \"$f.replaying\"; fi\n\
         if [ -f \"$f.replaying\" ]; then cat \"$f.replaying\"; rm -f \"$f.replaying\"; fi\n\
         s=\"$d/hook-status.json\"\n\
         if [ -f \"$s\" ]; then printf '\\n{marker}\\n'; cat \"$s\"; rm -f \"$s\"; fi\n",
        marker = STATUS_MARKER
    ))
}

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

fn gate_take_again(g: &mut DrainGate) -> bool {
    std::mem::take(&mut g.again)
}

/// SPEC §11.4.4.
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
    // An empty drain may mean the hook is still writing its line (§11.4.4).
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

/// SPEC §11.4.4.
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

const SCAN_SCRIPT: &str = "for d in \"$HOME/.config/agents-manager/bots\"/*/; do \
     [ -d \"$d\" ] || continue; b=$(basename \"$d\"); \
     if [ -f \"$d/hook-spool.jsonl\" ] || [ -f \"$d/hook-spool.jsonl.replaying\" ] || [ -f \"$d/hook-status.json\" ]; \
     then echo \"$b\"; fi; done\n";

/// SPEC §4.4.6.
pub async fn replay_spool(app: &Arc<App>, bot_id: &str) -> Result<usize> {
    let host = db::bot_host(&app.db, bot_id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
    if host != crate::config::LOCAL_HOST {
        return drain_remote(app, &host, bot_id).await;
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let dir = app.bot_dir(bot_id)?;
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

    #[test]
    fn output_without_the_marker_is_all_spool() {
        let (lines, status) = parse_drain_output("{\"bot_id\":\"b\"}\n");
        assert_eq!(lines, vec!["{\"bot_id\":\"b\"}"]);
        assert!(status.is_none());
        let (lines, status) = parse_drain_output("");
        assert!(lines.is_empty() && status.is_none());
    }

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

    /// §11.4.4.
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
        let s = drain_script("botX").unwrap();
        assert!(s.contains("bots/\"'botX'"));
        assert!(s.contains("mv \"$f\" \"$f.replaying\""));
        assert!(s.contains("hook-status.json"));
        assert!(s.contains(STATUS_MARKER));
    }

    #[test]
    fn the_drain_script_rejects_unsafe_ids() {
        for id in ["../..", "x/y", r"..\..", "", "x\";id"] {
            assert!(drain_script(id).is_err(), "unsafe id was accepted: {id:?}");
        }
    }
}

#[cfg(test)]
mod external_claim_tests {
    use super::*;
    use crate::testing as tt;

    #[test]
    fn hook_user_dedups_against_the_scraped_echo() {
        let echo = vec!["Reply with exactly MERGED-OK".to_string()];
        assert!(!hook_user_is_new(&echo, "Reply with exactly MERGED-OK"));
        // The pane wrapped the echo across two columns; the hook sends one line.
        assert!(!hook_user_is_new(&vec!["Reply with\n  exactly MERGED-OK".into()], "Reply with exactly MERGED-OK"));
        // The pane clipped the echo at the column width.
        assert!(!hook_user_is_new(&vec!["Reply with exactly MER".into()], "Reply with exactly MERGED-OK"));
    }

    #[test]
    fn hook_user_is_stored_when_it_is_not_the_echo() {
        assert!(hook_user_is_new(&[], "Reply with exactly MERGED-OK"));
        assert!(hook_user_is_new(&vec!["echo 1".into()], "echo 2"));
        // Nothing to store.
        assert!(!hook_user_is_new(&[], "   "));
    }

    /// `db::open` needs a path; the directory removes itself on drop.
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

    /// 2026-09-13（GROK）：備援關掉的回合沒存回覆，遲到 hook 的答案被丟、使用者看到「沒回應」。
    #[tokio::test]
    async fn a_late_hook_fills_a_fallback_turn_that_has_no_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'late-hook','grok','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web','completed_fallback','ok',?,?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();

        fill_or_drop_late_hook(&app, &turn, "側欄那組徽章已收齊，cdcf165 已推", &Some("s1".into()), &Some("n1".into()))
            .await
            .unwrap();

        let (status, native): (String, Option<String>) =
            sqlx::query_as("SELECT status, native_turn_id FROM turns WHERE id=?")
                .bind(&turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(status, "completed", "有 hook 的證據就不再是「只看到畫面」");
        assert_eq!(native.as_deref(), Some("n1"), "native id 照舊蓋上去，重送才去得掉重");
        let replies: Vec<String> =
            sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
                .bind(&turn_id)
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(replies, vec!["側欄那組徽章已收齊，cdcf165 已推".to_string()]);

        // 已有回覆的照舊丟，防一回合兩則。
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        fill_or_drop_late_hook(&app, &turn, "第二份回覆", &Some("s1".into()), &Some("n1".into())).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1, "不會變成兩則");
    }

    /// 空的 hook 不能把回合改成 completed——那等於宣稱有答案。
    #[tokio::test]
    async fn an_empty_late_hook_changes_nothing_but_the_native_ids() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'late-hook-empty','grok','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web','completed_fallback','ok',?,?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        fill_or_drop_late_hook(&app, &turn, "   ", &Some("s2".into()), &Some("n2".into())).await.unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(status, "completed_fallback");
    }

    /// 2026-09-12 使用者實機：折行讓刮下來的訊息斷在一半，既有那則是原文前綴時要補完。
    #[tokio::test]
    async fn a_clipped_scraped_prompt_is_upgraded_to_the_hooks_full_text() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'clipped-echo','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at)
             VALUES (?,?,'external','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let clipped = "請問我兩題，第二題 header『功能』請設 multiSelect:";
        let full = "請問我兩題，第二題 header『功能』請設 multiSelect: true，四個選項。問完就停著等我回答。";
        crate::lifecycle::insert_message(&app, &conv, Some(&turn_id), "user", clipped, "terminal_fallback", false, None)
            .await
            .unwrap();

        upgrade_clipped_user_message(&app, &turn_id, full).await.unwrap();

        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT content, source FROM messages WHERE turn_id=? AND role='user'")
                .bind(&turn_id)
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1, "補完，不是多開一則");
        assert_eq!(rows[0].0, full);
        assert_eq!(rows[0].1, "hook");

        // 不是前綴的就別動：那是另一句話。
        upgrade_clipped_user_message(&app, &turn_id, "完全不同的一句").await.unwrap();
        let after: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='user'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(after, full);
    }

    #[tokio::test]
    async fn stop_hook_resolves_unknown_delivery_when_it_completes_a_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'hook-unknown','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','working','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','unknown',?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        process(
            &app,
            &HookBody {
                bot_id,
                provider: "claude".into(),
                payload: json!({
                    "hook_event_name": "Stop",
                    "session_id": "native-session",
                    "prompt_id": "native-turn",
                    "last_assistant_message": "hook reply",
                }),
                received_at: None,
                truncated: false,
            },
        )
        .await
        .unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turn.status, "completed");
        assert_eq!(turn.delivery, "ok");
        let assistant_messages: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'",
        )
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(assistant_messages, 1);
    }

    /// Hook vs. fallback race (review 2026-09-12 #5). The BEFORE UPDATE trigger stands in for the
    /// fallback winning the CAS.
    #[tokio::test]
    async fn a_hook_that_loses_the_cas_to_the_fallback_adds_no_second_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'hook-race','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        crate::lifecycle::insert_message(&app, &conv, Some(&turn_id), "assistant", "from the pane", "terminal_fallback", true, None)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER fallback_wins BEFORE UPDATE OF status ON turns
             WHEN OLD.status='in_flight' AND NEW.status='completed'
             BEGIN
               UPDATE turns SET status='completed_fallback', completed_at=NEW.completed_at WHERE id=OLD.id;
               SELECT RAISE(IGNORE);
             END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        process(
            &app,
            &HookBody {
                bot_id,
                provider: "claude".into(),
                payload: json!({
                    "hook_event_name": "Stop",
                    "session_id": "native-session",
                    "prompt_id": "native-turn",
                    "last_assistant_message": "from the hook",
                }),
                received_at: None,
                truncated: false,
            },
        )
        .await
        .unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turn.status, "completed_fallback", "the fallback's claim stands");
        assert_eq!(turn.native_turn_id.as_deref(), Some("native-turn"), "the ids land on that turn, so a retry dedups");
        assert_eq!(turn.native_session_id.as_deref(), Some("native-session"));
        let replies: Vec<String> = sqlx::query_scalar("SELECT source FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, ["terminal_fallback"], "one answer, not two");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 1, "and no external turn was opened for the dropped payload");
    }

    /// Step 4 must find the `external` turn, or step 5 inserts a duplicate.
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

        // Afterwards nothing is in flight, so a retried hook dedups.
        sqlx::query("UPDATE turns SET status='completed', completed_at=?, native_turn_id='u' WHERE id=? AND status='in_flight'")
            .bind(&now)
            .bind("t")
            .execute(&pool)
            .await
            .unwrap();
        assert!(db::in_flight_turn(&pool, &run).await.unwrap().is_none());
    }

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
    use crate::testing::{claude_bot, env, Env};

    async fn fixture() -> (Env, db::Bot, db::Run) {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "resumer").await;
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

    async fn remaining(e: &Env, run: &db::Run) -> Option<String> {
        sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap()
    }

    #[tokio::test]
    async fn a_reported_session_id_consumes_the_marker() {
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, Some("native-expected")).await.unwrap();
        assert_eq!(remaining(&e, &run).await, None);
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, Some("native-other")).await.unwrap();
        consume_resume_session(&e.app, &bot, &run, Some("native-other")).await.unwrap();
        assert_eq!(remaining(&e, &run).await, None);
    }

    #[tokio::test]
    async fn a_hook_without_a_session_id_leaves_the_request_pending() {
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, None).await.unwrap();
        assert_eq!(remaining(&e, &run).await.as_deref(), Some("native-expected"));
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
