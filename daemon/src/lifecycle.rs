//! Bot / Run lifecycle (SPEC §6.2–§6.4, §4.3). Every public entry point takes the per-bot lock.

use crate::capture::Capture;
use crate::config::{valid_id, ID_RE, LOCAL_HOST};
use crate::db;
use crate::herdr::{AgentStatus, HerdrClient, HerdrError};
use crate::hosts::{sh_quote, HostConn};
use crate::state::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug)]
pub enum LcError {
    NotFound(String),
    Conflict(Value),
    Upstream(String),
    Bad(String),
    /// A 400 whose body is machine-readable rather than a message, e.g.
    /// `{"error":"remote_not_supported","host":"m4p"}`.
    BadValue(Value),
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

/// herdr's "pane exists but its shell is not ready yet" — a timing answer, not a failure.
/// Message fallback: some call sites only see the error flattened into a string.
fn pane_not_ready(e: &anyhow::Error) -> bool {
    if let Some(h) = e.downcast_ref::<HerdrError>() {
        if h.code == "agent_pane_busy" || h.message.contains("not an available shell") {
            return true;
        }
    }
    let s = e.to_string();
    s.contains("agent_pane_busy") || s.contains("not an available shell")
}


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
    insert_message_grouped(app, conversation_id, turn_id, role, content, source, incomplete, snapshot, None).await
}

#[allow(clippy::too_many_arguments)]
async fn insert_message_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
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
    .execute(&mut **tx)
    .await?;
    Ok(sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE id = ?")
        .bind(&id)
        .fetch_one(&mut **tx)
        .await?)
}

async fn emit_message_added(app: &Arc<App>, bot_id: &str, message: db::Message) {
    app.emit("message_added", json!({ "bot_id": bot_id, "message": message })).await;
}

/// `insert_message` with a SPEC §13 `group_id` (project group chat).
#[allow(clippy::too_many_arguments)]
pub async fn insert_message_grouped(
    app: &Arc<App>,
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
    group_id: Option<&str>,
) -> anyhow::Result<db::Message> {
    insert_message_full(app, conversation_id, turn_id, role, content, source, incomplete, snapshot, group_id, None).await
}

/// 同上，外加 `relay_from`（別的 bot 送進來的，SPEC §6.5d）。INSERT 時就寫：`message_added`
/// 當下就推出去，事後 UPDATE 的話泡泡要重新載入才會變「AGM →」。
#[allow(clippy::too_many_arguments)]
pub async fn insert_message_full(
    app: &Arc<App>,
    conversation_id: &str,
    turn_id: Option<&str>,
    role: &str,
    content: &str,
    source: &str,
    incomplete: bool,
    snapshot: Option<&str>,
    group_id: Option<&str>,
    relay_from: Option<&str>,
) -> anyhow::Result<db::Message> {
    let id = db::ulid();

    let now = db::now();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, incomplete, terminal_snapshot, group_id, relay_from, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(&id)
    .bind(conversation_id)
    .bind(turn_id)
    .bind(role)
    .bind(content)
    .bind(source)
    .bind(incomplete as i64)
    .bind(snapshot)
    .bind(group_id)
    .bind(relay_from)
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
        let should_flush_queue = t.status != "in_flight" && t.status != "queued";
        app.emit("turn_updated", json!({ "bot_id": bot_id, "turn": t })).await;
        // Every path that takes a turn out of `in_flight` funnels through here: one subscription suffices.
        app.publish_turn(crate::state::TurnEvent {
            bot_id: bot_id.clone(),
            turn_id: t.id.clone(),
            status: t.status.clone(),
            delivery: t.delivery.clone(),
        });
        // Schedule after publishing so the next prompt cannot race the completion event.
        if should_flush_queue {
            schedule_flush_queued(app, &bot_id);
        }
    }
}

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
    let res = client
        .call_timeout("agent.prompt", json!({"target": db::run_target(&run, &bot), "text": &text}), Duration::from_secs(10))
        .await;
    let delivery = match res {
        Ok(_) => "ok",
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


/// The hook / statusLine command line for a *local* bot. The token is deliberately **not**
/// on the argv (issue #43: `ps` shows every user the full command line, and the statusLine
/// runs on every redraw); the subcommands read `AM_HOOK_TOKEN` from the pane env instead.
fn hook_cmd_parts(app: &App, bot: &db::Bot, provider: &str) -> Vec<String> {
    hook_cmd_parts_for(&app.exe.to_string_lossy(), app.port, &bot.id, provider)
}

fn hook_cmd_parts_for(exe: &str, port: u16, bot_id: &str, provider: &str) -> Vec<String> {
    vec![exe.into(), "hook".into(), provider.into(), "--bot".into(), bot_id.into(), "--port".into(), port.to_string()]
}

/// What goes in `hook.sh`'s third argv slot. The script ignores it; the real `hook_token` is
/// never put on a remote command line (review 2026-09-12 #8).
pub const REMOTE_TOKEN_SLOT: &str = "-";

/// SPEC §11.4 — the POSIX sh hook for remote hosts. Payload goes to the bot's spool; state goes
/// to this machine's herdr (`pane report-agent`), whose event makes the daemon drain the spool (§11.4.3).
pub const REMOTE_HOOK_SH: &str = r#"#!/bin/sh
PROVIDER="$1"; BOT="$2"; TOKEN="$3"; shift 3
LIMIT=1048576
DIR="$HOME/.config/agents-manager/bots/$BOT"
mkdir -p "$DIR" 2>/dev/null
# Argument 3 is the token slot. It is never used here (the spool file is already only ours to
# read), and the daemon passes `-` in it: a real token on codex's argv would be visible to
# every user of the host through `ps`.
: "$TOKEN"

# v4.0 statusLine mode: the rate limits arrive on every repaint, so they go to a single-slot
# file the daemon picks up with the next drain (§11.4.5) — never the spool, which is a queue.
# Then run the user's own statusLine command on the same input so the pane looks unchanged.
if [ "$PROVIDER" = "statusline" ]; then
  INPUT=$(head -c $LIMIT)
  case "$INPUT" in
    '{}'|'') ;;
    '{'*)
      printf '{"hook_event_name":"StatusLine",%s' "${INPUT#\{}" > "$DIR/hook-status.json.tmp" 2>/dev/null \
        && mv -f "$DIR/hook-status.json.tmp" "$DIR/hook-status.json" 2>/dev/null ;;
  esac
  CFG="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json"
  CMD=""
  if [ -f "$CFG" ]; then
    if command -v jq >/dev/null 2>&1; then
      CMD=$(jq -r 'if (.statusLine.type // "command") == "command" then (.statusLine.command // empty) else empty end' "$CFG" 2>/dev/null)
    elif command -v python3 >/dev/null 2>&1; then
      CMD=$(python3 -c 'import json,sys
s=(json.load(open(sys.argv[1])).get("statusLine") or {})
print(s.get("command","") if s.get("type","command")=="command" else "")' "$CFG" 2>/dev/null)
    fi
  fi
  case "$CMD" in *"hook.sh statusline"*|*"agents-managerd statusline"*) CMD="" ;; esac
  if [ -n "$CMD" ]; then printf '%s' "$INPUT" | sh -c "$CMD" 2>/dev/null; fi
  exit 0
fi
if [ "$PROVIDER" = "codex" ]; then PAYLOAD="$1"; else PAYLOAD=$(head -c $LIMIT); fi
LEN=$(printf '%s' "$PAYLOAD" | wc -c | tr -d ' ')
if [ "$PROVIDER" != "codex" ] && [ "$LEN" -ge "$LIMIT" ]; then TRUNC=true; else TRUNC=false; fi
# The payload is spliced into JSON verbatim, so it must BE valid JSON: empty stdin becomes
# null and anything that is not an object is wrapped as a string, otherwise the daemon would
# reject the body and the line would sit in the spool forever.
case "$PAYLOAD" in
  '{'*) ;;
  '') PAYLOAD=null ;;
  *) ESC=$(printf '%s' "$PAYLOAD" | tr -d '\015' | tr '\011' ' ' | tr -d '\000-\010\013\014\016-\037' \
       | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | awk '{printf "%s\\n", $0}')
     PAYLOAD=$(printf '{"raw":"%s"}' "$ESC") ;;
esac
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
BODY=$(printf '{"bot_id":"%s","provider":"%s","payload":%s,"received_at":"%s","truncated":%s}' "$BOT" "$PROVIDER" "$PAYLOAD" "$NOW" "$TRUNC")
# Spool FIRST: the daemon reads this file the moment it sees the state change below, so the
# line has to be there before herdr is told anything.
printf '%s\n' "$BODY" >> "$DIR/hook-spool.jsonl"

# ---- tell this host's herdr what the agent is doing (SPEC §11.4.2)
[ -n "${HERDR_PANE_ID:-}" ] || exit 0
HERDR="${AM_REAL_HERDR:-}"
if [ -z "$HERDR" ] || [ ! -x "$HERDR" ]; then HERDR=$(command -v herdr 2>/dev/null); fi
if [ -z "$HERDR" ]; then printf '%s herdr not found; spooled only\n' "$NOW" >> "$DIR/hook.log"; exit 0; fi
am_herdr() {
  if [ -n "${HERDR_SESSION:-}" ]; then "$HERDR" --session "$HERDR_SESSION" "$@"; else "$HERDR" "$@"; fi
}
# `--seq` is monotonic per (pane, source) and anything <= the last one is dropped, so this has
# to keep climbing: nanoseconds where python3 exists (herdr's own integration does the same),
# else <epoch seconds><3-digit counter>, which is still seconds*1000 + n.
am_seq() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import time; print(time.time_ns())' 2>/dev/null && return 0
  fi
  _n=0
  [ -f "$DIR/hook-seq" ] && _n=$(cat "$DIR/hook-seq" 2>/dev/null)
  case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
  _n=$(( (_n + 1) % 1000 ))
  printf '%s\n' "$_n" > "$DIR/hook-seq" 2>/dev/null
  printf '%s%03d\n' "$(date +%s)" "$_n"
}
# One string field out of a single-line JSON object. Coarse on purpose (see below).
am_str() {
  printf '%s' "$PAYLOAD" | sed -n 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1
}
# The classification that matters lives in the daemon (`hookrecv::classify`). All this needs
# to decide is "did a turn just end" — a wrong guess here would only cost a late sweep, while
# a wrong guess about *content* would swallow a reply.
STATE=""
START=0
case "$PROVIDER" in
  claude)
    case "$PAYLOAD" in *'"hook_event_name":"Stop"'*|*'"hook_event_name": "Stop"'*) STATE=idle ;; esac
    case "$PAYLOAD" in *'"stop_hook_active":true'*|*'"stop_hook_active": true'*) STATE="" ;; esac
    case "$PAYLOAD" in *'"hook_event_name":"SessionStart"'*|*'"hook_event_name": "SessionStart"'*) START=1 ;; esac
    ;;
  codex)
    case "$PAYLOAD" in *'"type":"agent-turn-complete"'*|*'"type": "agent-turn-complete"'*) STATE=idle ;; esac
    ;;
  grok)
    case "$PAYLOAD" in *'"hookEventName":"stop"'*|*'"hook_event_name":"stop"'*) STATE=idle ;; esac
    case "$PAYLOAD" in *'"reason":"shutdown"'*|*'"stopHookActive":true'*|*'"stop_hook_active":true'*) STATE="" ;; esac
    case "$PAYLOAD" in *'"hookEventName":"session_start"'*|*'"hook_event_name":"session_start"'*) START=1 ;; esac
    ;;
esac
[ "$START" = 1 ] || [ -n "$STATE" ] || exit 0
SID=$(am_str session_id)
[ -n "$SID" ] || SID=$(am_str sessionId)
[ -n "$SID" ] || SID=$(am_str thread-id)
TP=$(am_str transcript_path)
[ -n "$TP" ] || TP=$(am_str transcriptPath)
SEQ=$(am_seq)
if [ "$START" = 1 ]; then
  # herdr 0.8.2 emits no event for this and does not surface it; best effort, for herdr's own
  # bookkeeping. The daemon still learns the session id from the spooled payload.
  set -- pane report-agent-session "$HERDR_PANE_ID" --source "agents-manager:$BOT" --agent "$PROVIDER" --seq "$SEQ"
else
  # Only ever `idle`: there is no "the agent started" hook, and herdr's own terminal detection
  # already reports `working` / `blocked`. Reporting those here would just fight it.
  set -- pane report-agent "$HERDR_PANE_ID" --source "agents-manager:$BOT" --agent "$PROVIDER" --state "$STATE" --seq "$SEQ"
fi
[ -n "$SID" ] && set -- "$@" --agent-session-id "$SID"
[ -n "$TP" ] && set -- "$@" --agent-session-path "$TP"
am_herdr "$@" >/dev/null 2>>"$DIR/hook.log"
exit 0
"#;

// grok (SPEC §12): 1.0.13 has no per-launch hook flag, so one global hooks file runs a static
// dispatcher that reads `AM_BOT_ID` / `AM_HOOK_TOKEN` / `AM_PORT` from the pane env and exits 0
// when unset — the user's own grok sessions are unaffected.

pub const GROK_HOOKS_FILE: &str = "agents-manager.json";
pub const GROK_DISPATCH_SH: &str = "grok-hook.sh";

/// Dispatcher installed on remote hosts: forwards to the per-bot `hook.sh` (SPEC §11.4).
pub const REMOTE_GROK_DISPATCH_SH: &str = r#"#!/bin/sh
# agents-manager grok dispatcher (SPEC §12). Installed by the daemon; no-op outside daemon panes.
[ -n "$AM_BOT_ID" ] && [ -n "$AM_HOOK_TOKEN" ] || exit 0
H="$HOME/.config/agents-manager/bots/$AM_BOT_ID/hook.sh"
[ -x "$H" ] || exit 0
exec "$H" grok "$AM_BOT_ID" "$AM_HOOK_TOKEN"
"#;

fn local_grok_dispatch_sh(exe: &str) -> String {
    format!(
        "#!/bin/sh\n# agents-manager grok dispatcher (SPEC §12). Rewritten by the daemon on every grok bot start; no-op outside daemon panes.\n[ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0\nexec {exe} hook grok --bot \"$AM_BOT_ID\" --token \"$AM_HOOK_TOKEN\" --port \"${{AM_PORT:-7788}}\"\n",
        exe = sh_quote(exe)
    )
}

fn grok_hooks_json(dispatcher: &str) -> String {
    let entry = json!([{"hooks": [{"type": "command", "command": dispatcher, "timeout": 5}]}]);
    serde_json::to_string_pretty(&json!({"hooks": {"SessionStart": entry, "Stop": entry}})).unwrap_or_default()
}

fn grok_home(env: &Value, home: &str) -> String {
    env.get("GROK_HOME")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| format!("{home}/.grok"))
}

/// Write only when the content differs, so grok's hook loader does not see spurious changes.
fn write_if_changed(path: &std::path::Path, content: &str, executable: bool) -> anyhow::Result<bool> {
    if std::fs::read_to_string(path).map(|cur| cur == content).unwrap_or(false) {
        return Ok(false);
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, content)?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(true)
}

fn install_local_grok_hook(app: &App, env: &Value) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?.to_string_lossy().to_string();
    let dispatcher = app.data_dir.join(GROK_DISPATCH_SH);
    let exe = app.exe.to_string_lossy().to_string();
    let a = write_if_changed(&dispatcher, &local_grok_dispatch_sh(&exe), true)?;
    let hooks_path = std::path::PathBuf::from(grok_home(env, &home)).join("hooks").join(GROK_HOOKS_FILE);
    let b = write_if_changed(&hooks_path, &grok_hooks_json(&dispatcher.to_string_lossy()), false)?;
    if a || b {
        tracing::info!(dispatcher = %dispatcher.display(), hooks = %hooks_path.display(), "grok hook installed");
    }
    Ok(())
}

/// Remote grok bot: the same two files, written over ssh after `install_remote_hook`.
async fn install_remote_grok_hook(conn: &HostConn, env: &Value) -> anyhow::Result<()> {
    let home = conn.home().await?;
    let dispatcher = format!("{home}/.config/agents-manager/{GROK_DISPATCH_SH}");
    let hooks_dir = format!("{}/hooks", grok_home(env, &home));
    let script = format!(
        "set -e\nW={w}\nmkdir -p \"$(dirname \"$W\")\"\ncat > \"$W\" <<'AM_WRAP_EOF'\n{wrap}AM_WRAP_EOF\nchmod +x \"$W\"\nG={g}\nmkdir -p \"$G\"\ncat > \"$G/{file}\" <<'AM_JSON_EOF'\n{json}\nAM_JSON_EOF\nprintf 'AM_GROK_INSTALLED\\n'\n",
        w = sh_quote(&dispatcher),
        wrap = REMOTE_GROK_DISPATCH_SH,
        g = sh_quote(&hooks_dir),
        file = GROK_HOOKS_FILE,
        json = grok_hooks_json(&dispatcher),
    );
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_GROK_INSTALLED") {
        anyhow::bail!("remote grok hook install did not confirm:\n{}", out.trim());
    }
    tracing::info!(host = %conn.name, hooks_dir, "remote grok hook installed");
    Ok(())
}

pub struct RemoteHookPaths {
    pub dir: String,
    pub hook_sh: String,
    pub settings: String,
}

pub async fn remote_bot_dir(conn: &HostConn, bot_id: &str) -> anyhow::Result<RemoteHookPaths> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    let home = conn.home().await?;
    let dir = format!("{home}/.config/agents-manager/bots/{bot_id}");
    Ok(RemoteHookPaths { hook_sh: format!("{dir}/hook.sh"), settings: format!("{dir}/claude-settings.json"), dir })
}

/// SPEC §11.4 — push `hook.sh` (+ `claude-settings.json`) to the remote before `agent.start`.
async fn install_remote_hook(conn: &HostConn, bot: &db::Bot) -> anyhow::Result<RemoteHookPaths> {
    let p = remote_bot_dir(conn, &bot.id).await?;
    // Token slot is `-` (review 2026-09-12 #8): `hook.sh` never reads it and the real key opens
    // `/relay/announce` + `/hook/*`. Kept positional for older agents.
    let cmd = shell_join(&[p.hook_sh.clone(), "claude".into(), bot.id.clone(), REMOTE_TOKEN_SLOT.into()]);
    // `hook.sh statusline` writes `hook-status.json` (§11.4.5), then execs the user's own statusLine.
    let statusline = shell_join(&[p.hook_sh.clone(), "statusline".into(), bot.id.clone(), REMOTE_TOKEN_SLOT.into()]);
    let settings = json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": cmd}]}],
            "Stop": [{"hooks": [{"type": "command", "command": cmd}]}]
        },
        "statusLine": {"type": "command", "command": statusline},
            // Trial: shorter replies scrape cleaner from the terminal (§4.3) and read better in 對話.
            "outputStyle": "Concise",
            // `--dangerously-skip-permissions` still asks 「Bypass Permissions mode … Yes, I accept」
            // once per config dir; this is the record accepting it writes (2026-09-08).
            "skipDangerousModePermissionPrompt": true
    });
    let settings_text = serde_json::to_string_pretty(&settings)?;
    let script = format!(
        "set -e\nD={dir}\nmkdir -p \"$D\"\ncat > \"$D/hook.sh\" <<'AM_HOOK_EOF'\n{hook}AM_HOOK_EOF\nchmod +x \"$D/hook.sh\"\ncat > \"$D/claude-settings.json\" <<'AM_SETTINGS_EOF'\n{settings}\nAM_SETTINGS_EOF\nprintf 'AM_INSTALLED\\n'\n",
        dir = sh_quote(&p.dir),
        hook = REMOTE_HOOK_SH,
        settings = settings_text,
    );
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_INSTALLED") {
        anyhow::bail!("remote hook install did not confirm:\n{}", out.trim());
    }
    tracing::info!(host = %conn.name, bot = %bot.name, dir = %p.dir, "remote hook installed");
    Ok(p)
}

/// Put the `herdr` shim (SPEC §6.5b) where this bot's pane can reach it; returns the PATH dir.
/// An unwritable host does not block the start: reconcile's descent match still tracks children.
async fn install_shim(app: &Arc<App>, bot: &db::Bot, project: &db::Project) -> Option<String> {
    let installed = if project.host == LOCAL_HOST {
        app.bot_dir(&bot.id).and_then(|dir| {
            crate::herdr_shim::install_local(&dir)
                .map(|d| d.to_string_lossy().into_owned())
                .map_err(anyhow::Error::from)
        })
    } else {
        match app.hosts.get(&project.host).await {
            Some(conn) => match remote_bot_dir(&conn, &bot.id).await {
                Ok(p) => crate::herdr_shim::install_remote(&conn, &p.dir).await,
                Err(e) => Err(e),
            },
            None => Err(anyhow::anyhow!("unknown host `{}`", project.host)),
        }
    };
    match installed {
        Ok(dir) => Some(dir),
        Err(e) => {
            tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not install the herdr shim");
            None
        }
    }
}

/// Daemon-injected CLI args that go *before* the bot's own; remote projects also upload the hook (§11.4).
async fn injected_args(app: &App, bot: &db::Bot, project: &db::Project, env: &Value) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    if bot.auto_approve != 0 {
        match bot.kind.as_str() {
            "claude" => out.push("--dangerously-skip-permissions".into()),
            "codex" => out.push("--yolo".into()),
            // = `--permission-mode bypassPermissions` (grok 1.0.13 `--help`).
            "grok" => out.push("--always-approve".into()),
            other => anyhow::bail!("unknown bot kind {other}"),
        }
    }
    if bot.inject_hooks == 0 {
        return Ok(out);
    }

    // remote project: POSIX sh hook via that host's own herdr (§11.4)
    if project.host != LOCAL_HOST {
        let conn = app
            .hosts
            .get(&project.host)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown host `{}`", project.host))?;
        let paths = install_remote_hook(&conn, bot).await?;
        let hook_args: Vec<String> = match bot.kind.as_str() {
            // Trial: `--verbose` expands tool output in the pane so the 終端 preview shows what ran.
            "claude" => vec!["--settings".into(), paths.settings, "--verbose".into()],
            // The notify argv is visible in `ps` for the whole run: placeholder token slot (#8, issue #43).
            "codex" => {
                let parts = vec![paths.hook_sh, "codex".to_string(), bot.id.clone(), REMOTE_TOKEN_SLOT.to_string()];
                vec!["-c".into(), format!("notify={}", serde_json::to_string(&parts)?)]
            }
            // SPEC §12: global hooks file + dispatcher on the remote; nothing on the argv.
            "grok" => {
                install_remote_grok_hook(&conn, env).await?;
                vec![]
            }
            other => anyhow::bail!("unknown bot kind {other}"),
        };
        out.extend(hook_args);
        return Ok(out);
    }

    let dir = app.bot_dir(&bot.id)?;
    std::fs::create_dir_all(&dir)?;
    let hook_args: Vec<String> = match bot.kind.as_str() {
        "claude" => {
            let cmd = shell_join(&hook_cmd_parts(app, bot, "claude"));
            // Status line reports rate limits, then runs the user's own statusLine command.
            let mut sl = hook_cmd_parts(app, bot, "claude");
            sl[1] = "statusline".into();
            sl.remove(2);
            let statusline = shell_join(&sl);
            let settings = json!({
                "hooks": {
                    "SessionStart": [{"hooks": [{"type": "command", "command": cmd}]}],
                    "Stop": [{"hooks": [{"type": "command", "command": cmd}]}]
                },
                "statusLine": {"type": "command", "command": statusline},
            // Trial: shorter replies scrape cleaner from the terminal (§4.3) and read better in 對話.
            "outputStyle": "Concise",
            // `--dangerously-skip-permissions` still asks 「Bypass Permissions mode … Yes, I accept」
            // once per config dir; this is the record accepting it writes (2026-09-08).
            "skipDangerousModePermissionPrompt": true
            });
            let path = dir.join("claude-settings.json");
            write_private(&path, &serde_json::to_vec_pretty(&settings)?)?;
            vec!["--settings".into(), path.to_string_lossy().to_string(), "--verbose".into()]
        }
        "codex" => {
            let parts = hook_cmd_parts(app, bot, "codex");
            let arr = serde_json::to_string(&parts)?;
            vec!["-c".into(), format!("notify={arr}")]
        }
        // SPEC §12: the hook is global (dispatched through the pane env), not an argv flag.
        "grok" => {
            install_local_grok_hook(app, env)?;
            vec![]
        }
        other => anyhow::bail!("unknown bot kind {other}"),
    };
    out.extend(hook_args);
    Ok(out)
}

/// Write a file only its owner can read (0600): bot settings may carry secrets.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(bytes)?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        return Ok(());
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes)
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| if p.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=".contains(c)) { p.clone() } else { format!("'{}'", p.replace('\'', "'\\''")) })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pane env = daemon-injected ∪ identity.env ∪ bot.env (later wins); `$HOME` / `~` expand against that host's home.
async fn pane_env(
    app: &Arc<App>,
    bot: &db::Bot,
    host: &str,
    run_id: &str,
    agent_name: &str,
    shim_dir: Option<&str>,
) -> Value {
    let mut env = serde_json::Map::new();
    env.insert("AM_BOT_ID".into(), json!(bot.id));
    // Name the herdr shim prefixes children with (SPEC §6.5b); the persona quotes it too.
    env.insert("AM_AGENT_NAME".into(), json!(agent_name));
    // 母 bot 的 kind／模型／強度：子 agent 沒指定 `--model` 時 shim 拿來補（SPEC §6.5b）。
    env.insert("AM_KIND".into(), json!(bot.kind));
    if let Some(m) = bot.model.as_deref().filter(|m| !m.trim().is_empty()) {
        env.insert("AM_MODEL".into(), json!(m));
    }
    if let Some(e) = bot.effort.as_deref().filter(|e| !e.trim().is_empty()) {
        env.insert("AM_EFFORT".into(), json!(e));
    }
    if let Some(dir) = shim_dir {
    // Best effort only: the login shell's profile (`path_helper`, `brew shellenv`) pushes us back;
    // `start_inner` re-prepends in the pane's shell, which is what actually wins.
        let path = match std::env::var("PATH") {
            Ok(p) if host == LOCAL_HOST => format!("{dir}:{p}"),
            _ => format!("{dir}:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"),
        };
        env.insert("PATH".into(), json!(path));
    }
    env.insert("AM_RUN_ID".into(), json!(run_id));
    // Only local hook commands call home over HTTP; remote panes have no port since v4.3 (§11.4.6).
    if host == LOCAL_HOST {
        env.insert("AM_PORT".into(), json!(app.port.to_string()));
    }
    // Hook token rides in the pane env for every kind: grok's dispatcher learns the bot only here
    // (SPEC §12), and local hooks read it so it never shows in `ps` (issue #43).
    // Omitting it is how `inject_hooks = false` is honoured for grok.
    if bot.inject_hooks != 0 {
        env.insert("AM_HOOK_TOKEN".into(), json!(bot.hook_token));
    }
    env.insert("CLAUDE_CODE_CHILD_SESSION".into(), json!(""));
    env.insert("CLAUDECODE".into(), json!(""));

    let home = match app.hosts.get(host).await {
        Some(c) => c.home().await.unwrap_or_else(|e| {
            tracing::warn!(host, error = %e, "could not resolve the host's home; leaving $HOME unexpanded");
            "$HOME".to_string()
        }),
        None => dirs::home_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
    };

    // Identities are per host (SPEC §16): `cc1` means that machine's config dir.
    if let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if let Some(id) = crate::tools::identity_for_host(app, host, idn).await {
            for (k, v) in &id.env {
                env.insert(k.clone(), json!(crate::config::expand_home(v, &home)));
            }
        }
    }
    for (k, v) in bot.env() {
        env.insert(k, json!(crate::config::expand_home(&v, &home)));
    }
    Value::Object(env)
}

/// Quote `s` as a TOML basic string (for codex `-c key="…"`).
pub fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The one text about opening sub-agents, used by the persona (all kinds) and the claude herdr
/// skill so they never drift. Naming isn't load-bearing (§6.5a descent, §6.5b shim) but keeps
/// the agent's mental model matching the sidebar.
pub fn child_agent_rules(agent_name: &str) -> String {
    format!(
        "你在 agents-manager（AG Man）裡的 agent 名稱是 `{agent_name}`。\
以下是**硬規則，不是建議**：一律照做，不要自行判斷要不要遵守，也不要事後才補。違反就是錯誤。\n\
\n\
需要開子任務或平行工作時，一律用 herdr 開子 agent：\n\
\n\
- **先找閒置的 child**：開新的子 agent 之前，**必須先跑** `herdr agent list`，看自己底下有沒有 `idle` / `done` 的子 agent。\
有就用 `herdr agent prompt <名稱> \"…\"` 把下一份工作交下去。**禁止**每件事都開一顆新的；只有使用者明確要求新開時才可以。\n\
- **命名**：`herdr agent start <名稱> …` 的名稱**必須**以 `{agent_name}-` 為前綴（例：`{agent_name}-review`、`{agent_name}-ui`）。\
PATH 上的 herdr 會幫你補，但你自己要寫對。\n\
- **開 pane**：一律 `herdr pane split --pane \"$HERDR_PANE_ID\"`（或 `--current`）。\
**禁止省略目標**——省略時 herdr 會去拆使用者正在看的那個 pane。\n\
- **環境變數**：帳號與 hook 會自動帶進子 pane（`CLAUDE_CONFIG_DIR`、`AM_*`）。**禁止覆蓋**這些變數。\n\
- **禁止 `git stash`、禁止 `--autostash`**：同一個工作樹上有別的 agent 還沒提交的改動，弄丟了算你的。\n\
- 你開的子 agent 會被 AG Man 掛在**你底下**追蹤（側欄縮排顯示）。做完**必須**自己把它的 pane 收掉，不准留著。\n\
\n\
瀏覽器（同樣是硬規則）：\n\
\n\
- **一律用 ego lite**（`ego-browser` skill）。**禁止**開 Chrome、**禁止**用其他 headless 或內建的瀏覽器工具。\n\
- **一個 bot 最多一個分頁**（你和你的子 agent 各自算一個）：用 `openOrReuseTab` 在同一個分頁裡換頁，\
**禁止**一個網址開一個新分頁；task space 一律重用（`useOrCreateTaskSpace(\"{agent_name}\")`）。\n\
- **結束就關分頁**：工作做完、或收掉子 agent 之前，**必須先** `closeTab` / `completeTaskSpace(…, {{ keep: false }})`。\
分頁留著不關，RAM 就是這樣被吃光的。"
    )
}

/// Replaces `herdr --skill`'s description, which says 「只有使用者明確提到 Herdr 才用」 —
/// backwards for a bot inside AG Man.
const HERDR_SKILL_DESC: &str = "在 agents-manager（AG Man）裡控制 herdr 的 pane、tab、workspace 與子 agent。\
需要開子任務、平行工作、或把工作分給另一個 agent 時，一律用這個 skill，並**必須**照裡面的 AG Man 規則命名與開 pane。";

/// Turn `herdr --skill`'s output into the copy this bot should read: our own description in
/// the front matter, and `child_agent_rules` as the first section of the body.
///
/// Only the description line inside the front matter is touched; the rest of herdr's document
/// is the CLI's own authority on its commands and is passed through untouched, so a herdr
/// upgrade brings its new text along.
fn herdr_skill_doc(raw: &str, agent_name: &str) -> String {
    let rules = format!("## AG Man 規則（硬規則，優先於本文件其餘內容）\n\n{}\n", child_agent_rules(agent_name));
    let lines: Vec<&str> = raw.lines().collect();
    // Front matter is `---` … `---`; without one, put ours in front and leave the rest alone.
    let end = if lines.first().map(|l| l.trim_end()) == Some("---") {
        lines.iter().skip(1).position(|l| l.trim_end() == "---").map(|i| i + 1)
    } else {
        None
    };
    let Some(end) = end else {
        return format!("{rules}\n{raw}");
    };
    let mut out = String::new();
    let mut replaced = false;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 && i < end && line.starts_with("description:") {
            out.push_str("description: \"");
            out.push_str(HERDR_SKILL_DESC);
            out.push_str("\"\n");
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if i == end {
            if !replaced {
                // No description to replace: the front matter is not what we expected, so add
                // ours to the body rather than editing a document we do not understand.
                tracing::debug!("herdr --skill has no description line; leaving its front matter alone");
            }
            out.push('\n');
            out.push_str(&rules);
        }
    }
    out
}

/// Install herdr's skill (with `child_agent_rules` on top) into `$CLAUDE_CONFIG_DIR/skills/`.
/// Per identity; idempotent because the file is in the user's own claude config (backup noise).
async fn install_herdr_skill(app: &Arc<App>, bot: &db::Bot, project: &db::Project, env: &Value, agent_name: &str) {
    if bot.kind != "claude" {
        return;
    }
    // Never write into the developer's own `~/.claude`; the rewrite is covered by `herdr_skill_doc` tests.
    if cfg!(test) {
        return;
    }
    let cfg_dir = env.get("CLAUDE_CONFIG_DIR").and_then(|v| v.as_str()).map(str::to_string);
    let result = if project.host == LOCAL_HOST {
        install_herdr_skill_local(cfg_dir, agent_name).await
    } else {
        match app.hosts.get(&project.host).await {
            Some(conn) => install_herdr_skill_remote(&conn, cfg_dir, agent_name).await,
            None => Err(anyhow::anyhow!("unknown host `{}`", project.host)),
        }
    };
    if let Err(e) = result {
        // Never fatal: the skill is a convenience, and claude works without it.
        tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not install the herdr skill");
    }
}

async fn install_herdr_skill_local(cfg_dir: Option<String>, agent_name: &str) -> anyhow::Result<()> {
    let out = tokio::process::Command::new("herdr").arg("--skill").output().await?;
    if !out.status.success() {
        anyhow::bail!("`herdr --skill` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let doc = herdr_skill_doc(&String::from_utf8_lossy(&out.stdout), agent_name);
    let base = match cfg_dir {
        Some(d) if !d.trim().is_empty() => std::path::PathBuf::from(d),
        _ => dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?.join(".claude"),
    };
    let dir = base.join("skills").join("herdr");
    let path = dir.join("SKILL.md");
    if std::fs::read_to_string(&path).map(|c| c == doc).unwrap_or(false) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, doc)?;
    tracing::info!(path = %path.display(), "herdr skill installed for claude");
    Ok(())
}

async fn install_herdr_skill_remote(
    conn: &HostConn,
    cfg_dir: Option<String>,
    agent_name: &str,
) -> anyhow::Result<()> {
    let raw = conn.ssh_exec("herdr --skill").await?;
    if !raw.contains("name:") {
        anyhow::bail!("`herdr --skill` on `{}` did not look like a skill file", conn.name);
    }
    let doc = herdr_skill_doc(&raw, agent_name);
    let base = match cfg_dir {
        Some(d) if !d.trim().is_empty() => d,
        _ => format!("{}/.claude", conn.home().await?),
    };
    let dir = format!("{base}/skills/herdr");
    // Temp file + `cmp` so an unchanged skill keeps its mtime, like the local path.
    let script = format!(
        "set -e\nD={dir}\nmkdir -p \"$D\"\ncat > \"$D/.SKILL.md.new\" <<'AM_SKILL_EOF'\n{doc}\nAM_SKILL_EOF\nif cmp -s \"$D/.SKILL.md.new\" \"$D/SKILL.md\" 2>/dev/null; then rm -f \"$D/.SKILL.md.new\"; else mv \"$D/.SKILL.md.new\" \"$D/SKILL.md\"; fi\nprintf 'AM_SKILL_OK\\n'\n",
        dir = sh_quote(&dir),
        doc = doc.trim_end(),
    );
    let out = conn.ssh_exec(&script).await?;
    if !out.contains("AM_SKILL_OK") {
        anyhow::bail!("remote herdr skill install did not confirm:\n{}", out.trim());
    }
    tracing::info!(host = %conn.name, dir, "herdr skill installed for claude");
    Ok(())
}

/// `bot.persona` appended to the system prompt per kind; `child_agent_rules` comes first.
fn persona_args(bot: &db::Bot, agent_name: &str) -> Vec<String> {
    let user = bot.persona.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let p = match user {
        Some(u) => format!("{}\n\n{u}", child_agent_rules(agent_name)),
        None => child_agent_rules(agent_name),
    };
    let p = p.as_str();
    match bot.kind.as_str() {
        "claude" => vec!["--append-system-prompt".into(), p.to_string()],
        // `grok --help`: "Extra rules to append to the system prompt".
        "grok" => vec!["--rules".into(), p.to_string()],
        // app-server / config schema key `developer_instructions`; the value is TOML.
        "codex" => vec!["-c".into(), format!("developer_instructions={}", toml_basic_string(p))],
        _ => vec![],
    }
}

/// `bot.model` / `bot.effort` as CLI args. Efforts are per model: a stale one gets
/// `400 unsupported_value` on every turn (codex `max` on `gpt-5.5`), so it is dropped — but only
/// when the model's list is readable and lacks it; uncertain paths keep it (dropping a valid one
/// silently downgrades the agent).
async fn effort_checked(app: &Arc<App>, bot: &db::Bot, host: &str) -> db::Bot {
    let Some(effort) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { return bot.clone() };
    let Some(model) = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { return bot.clone() };
    // `efforts` does not depend on identity, so no identity is passed.
    let Ok(list) = crate::models::list(app, host, &bot.kind, None, false).await else { return bot.clone() };
    let Some(entry) = list
        .get("models")
        .and_then(|m| m.as_array())
        .and_then(|a| a.iter().find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model)))
    else {
        return bot.clone();
    };
    let Some(efforts) = entry.get("efforts").and_then(|e| e.as_array()).filter(|a| !a.is_empty()) else { return bot.clone() };
    if efforts.iter().any(|e| e.as_str() == Some(effort)) {
        return bot.clone();
    }
    tracing::warn!(bot = %bot.name, model, effort, "model does not accept this reasoning effort; starting without it");
    let mut out = bot.clone();
    out.effort = None;
    out
}

fn model_args(bot: &db::Bot) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(m) = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        match bot.kind.as_str() {
            "claude" => out.extend(["--model".to_string(), m.to_string()]),
            "codex" | "grok" => out.extend(["-m".to_string(), m.to_string()]),
            _ => {}
        }
    }
    // grok: `--reasoning-effort low|medium|high|xhigh` (per model; grok-4.5 rejects xhigh)
    // codex: `-c model_reasoning_effort="<x>"` (values from `model/list`)
    // claude: `--effort low|medium|high|xhigh|max` (2.1+; an unknown value is only a warning)
    if let Some(e) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        match bot.kind.as_str() {
            "claude" => out.extend(["--effort".to_string(), e.to_lowercase()]),
            "grok" => out.extend(["--reasoning-effort".to_string(), e.to_lowercase()]),
            "codex" => out.extend(["-c".to_string(), format!("model_reasoning_effort=\"{}\"", e.to_lowercase())]),
            _ => {}
        }
    }
    // codex Fast tier must be sent both ways (2026-09-09): omitted means `~/.codex/config.toml`
    // decides, often `service_tier = "fast"`, so an unchecked bot ran fast. `service_tier=""` is
    // codex's "no tier" (verified 0.153.4); `priority` is what the TUI shows as `fast`.
    if bot.kind == "codex" {
        let tier = if bot.fast != 0 { "priority" } else { "" };
        out.extend(["-c".to_string(), format!("service_tier=\"{tier}\"")]);
    }
    out
}

#[cfg(test)]
mod hook_cmd_parts_tests {
    use super::{hook_cmd_parts_for, write_private};

    /// Issue #43: the hook / statusLine command line must not carry the token.
    #[test]
    fn hook_cmd_parts_has_no_token() {
        let parts = hook_cmd_parts_for("/usr/bin/agents-managerd", 7788, "b1", "claude");
        assert_eq!(parts, vec!["/usr/bin/agents-managerd", "hook", "claude", "--bot", "b1", "--port", "7788"]);
        assert!(!parts.iter().any(|p| p == "--token"));
    }

    #[cfg(unix)]
    #[test]
    fn write_private_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("am-write-private-{}.json", std::process::id()));
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"{}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod model_args_tests {
    use super::model_args;
    use crate::db::Bot;

    fn bot(kind: &str, model: Option<&str>, effort: Option<&str>, fast: bool) -> Bot {
        Bot {
            id: "b".into(),
            project_id: "p".into(),
            name: "n".into(),
            kind: kind.into(),
            model: model.map(String::from),
            effort: effort.map(String::from),
            fast: fast as i64,
            persona: None,
            args_json: "[]".into(),
            autostart: 0,
            inject_hooks: 1,
            auto_approve: 1,
            identity: None,
            env_json: "{}".into(),
            managed_by: "user".into(),
            cwd: None,
            herdr_session: None,
            parent_bot_id: None,
            is_primary: 0,
            hook_token: "t".into(),
            deleted_at: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn codex_effort_and_fast() {
        let a = model_args(&bot("codex", Some("gpt-5.6-sol"), Some("high"), true));
        assert_eq!(
            a,
            vec!["-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=\"high\"", "-c", "service_tier=\"priority\""]
        );
        // Fast off ≠ no opinion: without the flag `~/.codex/config.toml` decides.
        let a = model_args(&bot("codex", None, None, false));
        assert_eq!(a, vec!["-c", "service_tier=\"\""]);
    }

    /// SPEC §4.4a: `model_args` and its inverse must agree — the run stamp is read back through it.
    #[test]
    fn the_runtime_stamp_reads_back_what_we_passed() {
        let b = bot("codex", Some("gpt-5.6-luna"), Some("xhigh"), true);
        let args = model_args(&b);
        let (model, effort) = crate::models::model_effort_from_argv("codex", &args);
        assert_eq!(model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(effort.as_deref(), Some("xhigh"));
        assert!(args.iter().any(|a| a.contains("service_tier=\"priority\"")), "fast is a flag we can read back");
        // CLI defaults read back as neither (not 「設定與實際不符」); an empty tier reads as not-fast.
        let bare = model_args(&bot("codex", None, None, false));
        assert_eq!(crate::models::model_effort_from_argv("codex", &bare), (None, None));
        assert!(!bare.iter().any(|a| a.contains("service_tier=\"priority\"")));
    }

    #[test]
    fn grok_keeps_reasoning_effort_and_ignores_fast() {
        let a = model_args(&bot("grok", Some("grok-4.6"), Some("low"), true));
        assert_eq!(a, vec!["-m", "grok-4.6", "--reasoning-effort", "low"]);
    }

    /// claude takes `--effort` (2.1+) but never the codex Fast tier.
    #[test]
    fn claude_gets_effort_but_not_fast() {
        let a = model_args(&bot("claude", Some("opus"), Some("high"), true));
        assert_eq!(a, vec!["--model", "opus", "--effort", "high"]);
        let a = model_args(&bot("claude", None, Some("MAX"), false));
        assert_eq!(a, vec!["--effort", "max"], "the level goes out lowercase");
        assert!(model_args(&bot("claude", None, None, true)).is_empty());
    }

    /// Injected herdr skill: our description replaces herdr's, our rules go first, herdr's CLI text survives.
    #[test]
    fn the_herdr_skill_is_rewritten_for_ag_man() {
        let raw = "---\nname: herdr\ndescription: \"Control Herdr… Use only when the user explicitly mentions Herdr.\"\n---\n\n# Herdr\n\nherdr organizes terminals.\n";
        let doc = super::herdr_skill_doc(raw, "proj-abc123");
        assert!(doc.starts_with("---\nname: herdr\ndescription: \""), "front matter is kept, description replaced:\n{doc}");
        assert!(!doc.contains("Use only when the user explicitly mentions Herdr"), "herdr's own description is gone");
        assert!(doc.contains("需要開子任務"), "ours says to use it for sub-tasks");
        assert!(doc.contains("## AG Man 規則"), "the rules lead the body");
        assert!(doc.contains("`proj-abc123-`"), "the naming rule quotes this agent's name");
        assert!(doc.contains("git stash"), "the no-stash rule is carried");
        assert!(doc.contains("herdr agent list"), "reuse an idle child before opening a new one");
        assert!(doc.contains("$HERDR_PANE_ID"), "how to split its own pane");
        assert!(doc.contains("herdr organizes terminals."), "herdr's own body survives");
        // The rules must come before herdr's text, not after it.
        assert!(doc.find("AG Man 規則").unwrap() < doc.find("herdr organizes").unwrap());
    }

    /// A document without front matter is not edited — our rules simply go in front of it.
    #[test]
    fn a_skill_without_front_matter_is_not_rewritten() {
        let doc = super::herdr_skill_doc("# Herdr\n\nbody\n", "p-1");
        assert!(doc.starts_with("## AG Man 規則"));
        assert!(doc.ends_with("# Herdr\n\nbody\n"));
    }

    #[test]
    fn persona_per_kind() {
        use super::{child_agent_rules, persona_args, toml_basic_string};
        let mut b = bot("claude", None, None, false);
        let rule = child_agent_rules("proj-abc123");
        b.persona = Some("回覆結尾一律加上 [PERSONA-OK]".into());
        let want = format!("{rule}\n\n回覆結尾一律加上 [PERSONA-OK]");
        assert_eq!(persona_args(&b, "proj-abc123"), vec!["--append-system-prompt".to_string(), want.clone()]);
        b.kind = "grok".into();
        assert_eq!(persona_args(&b, "proj-abc123"), vec!["--rules".to_string(), want.clone()]);
        b.kind = "codex".into();
        b.persona = Some("line1\nsay \"hi\" \\ done".into());
        let want = format!("{rule}\n\nline1\nsay \"hi\" \\ done");
        assert_eq!(persona_args(&b, "proj-abc123"), vec!["-c".to_string(), format!("developer_instructions={}", toml_basic_string(&want))]);
        // No user persona: the daemon's rule alone, never nothing.
        b.kind = "claude".into();
        b.persona = None;
        assert_eq!(persona_args(&b, "proj-abc123"), vec!["--append-system-prompt".to_string(), rule.clone()]);
        assert!(rule.contains("`proj-abc123-`"));
        // 瀏覽器規則：只用 ego lite、一個 bot 一個分頁、bot 結束就關分頁，task space 用自己的名字。
        assert!(rule.contains("ego lite"));
        assert!(rule.contains("useOrCreateTaskSpace(\"proj-abc123\")"));
        assert!(rule.contains("{ keep: false }"));
    }

    /// 2026-09-13 使用者要求：人設與注入提示一律用命令語氣。客氣的寫法（「請…」「不要…比較好」）
    /// agent 會當成建議，實際上不照做；硬規則要寫成命令才會被執行。
    #[test]
    fn the_rules_read_as_orders_not_suggestions() {
        let rule = super::child_agent_rules("proj-abc123");
        assert!(rule.contains("硬規則，不是建議"), "the register is stated up front");
        assert!(rule.contains("必須"));
        assert!(rule.contains("禁止"));
        assert!(!rule.contains("請"), "no polite softeners in an injected rule");
    }
}

/// Identity CLI args on `host`. Discovered `ccN` identities carry none: alias flags are the user's shell habit.
async fn identity_args(app: &Arc<App>, bot: &db::Bot, host: &str) -> Vec<String> {
    let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) else { return vec![] };
    crate::tools::identity_for_host(app, host, idn).await.map(|i| i.args).unwrap_or_default()
}

async fn client_for_run(app: &Arc<App>, run: &db::Run) -> LcResult<HerdrClient> {
    app.herdr_for_run(run)
        .await
        .ok_or_else(|| LcError::Upstream(format!("no Herdr session is available for run `{}`", run.id)))
}

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


/// `resume_native` continues the bot's last native session (batch update restart).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StartOpts {
    pub resume_native: bool,
}

pub async fn start_bot(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    start_bot_with(app, bot_id, StartOpts::default()).await
}

pub async fn start_bot_with(app: &Arc<App>, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    start_bot_locked_with(app, bot_id, opts).await
}

pub async fn start_bot_locked_with(app: &Arc<App>, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    // A child only exists as the pane its parent opened; starting here would open an unrelated pane.
    if bot.managed_by == "child" {
        return Err(LcError::conflict(
            "a spawned child is started by its parent agent, not from here",
            json!({"parent_bot_id": bot.parent_bot_id}),
        ));
    }
    refuse_default_session(&bot)?;
    if let Some(existing) = db::active_run(&app.db, bot_id).await.map_err(up)? {
        return Err(LcError::conflict("active run already exists", json!({"run_id": existing.id})));
    }
    let project = db::project(&app.db, &bot.project_id)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let session = app
        .session_for_bot(&bot, &project.host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{}` is not configured", project.host)))?;
    // An unknown identity would silently run as the host's default login (m4p: `cc1` bot answered
    // as cc0). Refuse; not-logged-in only gets a system message so the user can `/login` inside.
    if let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if crate::tools::identity_for_host(app, &project.host, idn).await.is_none() {
            return Err(LcError::conflict(
                "identity is not known on this host",
                json!({"identity": idn, "host": project.host,
                       "hint": format!("主機 {} 沒有 `{idn}` 這個身份（config.toml 的 [[identities]] 或該機 zshrc 的 ccN alias）；先在那台建好，或到主機設定按「重新偵測」", project.host)}),
            ));
        }
        let mut not_logged_in = app
            .tools
            .lock()
            .await
            .get(&project.host)
            .and_then(|t| t.identities.get(idn))
            .map(|i| i.logged_in == Some(false))
            .unwrap_or(false);
    // The cache can be ~30 min stale after a login; ask the CLI before saying not logged in.
        if not_logged_in {
            if let Some(fresh) = crate::tools::recheck_identity_login(app, &project.host, idn).await {
                not_logged_in = !fresh;
            }
        }
        // Logged in headlessly but never onboarded: the TUI would open on the login menu.
        if !not_logged_in && bot.kind == "claude" && project.host == crate::config::LOCAL_HOST {
            if let Some(i) = crate::tools::identity_for_host(app, &project.host, idn).await {
                let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                let dir = i
                    .env
                    .get("CLAUDE_CONFIG_DIR")
                    .map(|d| crate::config::expand_home(d, &home))
                    .unwrap_or_else(|| format!("{home}/.claude"));
                if crate::tools::ensure_claude_onboarded(std::path::Path::new(&dir)) {
                    tracing::info!(bot = %bot.name, identity = idn, dir, "marked claude onboarding complete so the TUI skips the login menu");
                }
            }
        }
        if not_logged_in {
            tracing::warn!(bot = %bot.name, identity = idn, host = %project.host, "identity not logged in on host; the CLI will use the machine's default login");
            match db::conversation_id(&app.db, bot_id).await {
                Ok(conv) => {
                    let _ = insert_message(
                        app,
                        &conv,
                        None,
                        "system",
                        &format!("身份 `{idn}` 在 {} 沒有登入：claude 會退回這台機器預設（cc0）的帳號執行。啟動後請按「登入 / 切換帳號」登入 `{idn}`。", project.host),
                        "system",
                        false,
                        None,
                    )
                    .await;
                }
                Err(error) => {
                    tracing::warn!(error = ?error, bot = %bot_id, "could not get conversation for identity warning");
                }
            }
        }
    }

    // 1. INSERT Run before touching herdr (SPEC §6.2.1).
    let run_id = db::ulid();
    let ins = sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at) VALUES (?,?,'starting','unknown',?,?)")
        .bind(&run_id)
        .bind(bot_id)
        .bind(&session)
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

    match start_inner(app, &bot, &project, &run_id, opts).await {
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

/// Native-session continuation args. codex `resume` is a subcommand (must come first); grok has none.
fn resume_args_by_kind(kind: &str, session_id: &str) -> Result<Vec<String>, &'static str> {
    if session_id.trim().is_empty() {
        return Err("no_session_id");
    }
    match kind {
        "claude" => Ok(vec!["--resume".into(), session_id.into()]),
        "codex" => Ok(vec!["resume".into(), session_id.into()]),
        "grok" => Err("unsupported_kind"),
        _ => Err("unsupported_kind"),
    }
}

/// The last native session could not be continued, so this start opens a new conversation.
pub(crate) async fn context_lost(_app: &Arc<App>, bot: &db::Bot, why: &str) -> LcResult<()> {
    tracing::info!(bot = %bot.name, why, "native session continuation unavailable; starting a new conversation");
    Ok(())
}

/// A bot pane's start dir: `bots.cwd` (adopted child) or the project path.
pub fn bot_cwd<'a>(bot: &'a db::Bot, project: &'a db::Project) -> &'a str {
    match bot.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => c,
        None => project.path.as_str(),
    }
}

/// The bot's herdr tab label: its nickname, instead of herdr's `1`, `2`, `3`.
pub fn tab_label(bot: &db::Bot) -> String {
    let n = bot.name.trim();
    if n.is_empty() {
        "bot".to_string()
    } else {
        n.to_string()
    }
}

/// Close `tab_id` when empty — the single decider for every caller removing a pane from a tab.
/// Quiet and idempotent: herdr often reaps the tab itself. A tab still holding panes (pre
/// one-bot-one-tab runs, user layouts) is left alone.
async fn close_tab_if_empty(client: &crate::herdr::HerdrClient, workspace_id: &str, tab_id: &str) {
    let tabs = match client.tab_list(workspace_id).await {
        Ok(t) => t,
        // Never guess: closing a tab we cannot see could take the user's pane with it.
        Err(e) => {
            tracing::debug!(workspace_id, tab_id, error = %e, "tab.list failed; leaving the tab alone");
            return;
        }
    };
    match tabs.iter().find(|t| t.tab_id == tab_id) {
        None => {} // herdr already reaped it
        Some(t) if t.pane_count == 0 => {
            if let Err(e) = client.tab_close(tab_id).await {
                tracing::debug!(tab_id, error = %e, "tab.close failed");
            }
        }
        Some(_) => {}
    }
}

/// Close a run's pane and its tab if now empty; `tab_id` is `None` for pre one-bot-one-tab runs.
pub(crate) async fn close_pane_and_tab(
    client: &crate::herdr::HerdrClient,
    workspace_id: Option<&str>,
    tab_id: Option<&str>,
    pane_id: &str,
) {
    let _ = client.pane_close(pane_id).await;
    let ws = workspace_id.filter(|w| !w.trim().is_empty());
    if let (Some(ws), Some(tab)) = (ws, tab_id.filter(|t| !t.trim().is_empty())) {
        close_tab_if_empty(client, ws, tab).await;
    }
}

/// One bot, one tab. Splitting one tab shrank panes below ~31 columns, where TUIs reflow without
/// spaces (`is_shredded`); tabs don't share width. `focus` is false so starting a bot doesn't
/// yank the user's tab; `fresh_root` (a just-created workspace's pane) is used as-is.
async fn acquire_run_pane(
    client: &crate::herdr::HerdrClient,
    workspace_id: &str,
    cwd: &str,
    label: &str,
    env: &Value,
    fresh_root: Option<crate::herdr::PaneInfo>,
) -> anyhow::Result<crate::herdr::PaneInfo> {
    match fresh_root {
        Some(p) => Ok(p),
        None => client.tab_create(workspace_id, cwd, label, env.clone()).await,
    }
}

/// Pane cleanup for startup after a pane exists. Async, not `Drop`: `pane.close` and the tab
/// tidy-up must finish before the start error returns.
struct StartPaneGuard<'a> {
    client: &'a crate::herdr::HerdrClient,
    workspace_id: &'a str,
    tab_id: &'a str,
    pane_id: &'a str,
    armed: bool,
}

impl<'a> StartPaneGuard<'a> {
    fn new(client: &'a crate::herdr::HerdrClient, workspace_id: &'a str, tab_id: &'a str, pane_id: &'a str) -> Self {
        Self { client, workspace_id, tab_id, pane_id, armed: true }
    }

    async fn protect<T>(&mut self, result: LcResult<T>) -> LcResult<T> {
        if result.is_err() {
            self.cleanup().await;
        }
        result
    }

    async fn cleanup(&mut self) {
        if self.armed {
            close_pane_and_tab(self.client, Some(self.workspace_id), Some(self.tab_id), self.pane_id).await;
            self.armed = false;
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

async fn start_inner(
    app: &Arc<App>,
    bot: &db::Bot,
    project: &db::Project,
    run_id: &str,
    opts: StartOpts,
) -> LcResult<()> {
    let host = project.host.clone();
    let session = app
        .session_for_bot(bot, &host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{host}` is not configured")))?;
    let client = app
        .herdr_for_session(&host, &session)
        .await
        .ok_or_else(|| LcError::Upstream(format!("Herdr session `{session}` for host `{host}` is not configured")))?;
    if !app.session_connected(&host, &session).await {
        return Err(LcError::Upstream(format!("host `{host}` is not connected")));
    }
    // 1b. preflight: a missing CLI would sit in `launch_pending` for the full 60 s silently.
    if let Err(reason) = ensure_kind_installed(app, &host, &bot.kind).await {
        let conv = db::conversation_id(&app.db, &bot.id).await.map_err(up)?;
        let _ = insert_message(app, &conv, None, "system", &reason, "system", false, None).await;
        return Err(LcError::Bad(reason));
    }
    let agent = crate::config::agent_name(&project.label, &bot.id);
    let shim_dir = install_shim(app, bot, project).await;
    let env = pane_env(app, bot, &host, run_id, &agent, shim_dir.as_deref()).await;
    // SPEC §6.5c: claude learns herdr from a skill (the CLI's own doc), not the persona.
    install_herdr_skill(app, bot, project, &env, &agent).await;

    // Remote hook injection may ssh-upload, so it must happen before workspace/tab creation.
    let injected = injected_args(app, bot, project, &env).await.map_err(up)?;
    let mut args = injected;
    args.extend(persona_args(bot, &agent));
    args.extend(model_args(&effort_checked(app, bot, &project.host).await));
    args.extend(identity_args(app, bot, &project.host).await);
    args.extend(bot.args());

    // Reopen only: resolve the previous native session after preflight. The requested id is
    // persisted before `agent.start`; hookrecv uses it to detect a provider that ignored resume.
    let resume = if opts.resume_native {
        match db::last_native_session(&app.db, &bot.id).await.map_err(up)? {
            // No transcript = cannot resume: `--resume` prints "No conversation found" and exits
            // right after a "successful" restart (2026-09-11 restart-idle repro). Start fresh.
            Some((session_id, Some(transcript))) if host == LOCAL_HOST && !transcript.trim().is_empty() && !std::path::Path::new(&transcript).exists() => {
                tracing::info!(bot = %bot.name, session = %session_id, transcript, "native session has no transcript on disk; not resuming it");
                context_lost(app, bot, "transcript_missing").await?;
                None
            }
            Some((session_id, _)) if !session_id.trim().is_empty() => match resume_args_by_kind(&bot.kind, &session_id) {
                Ok(resume_args) => Some((session_id, resume_args)),
                Err(why) => {
                    context_lost(app, bot, why).await?;
                    None
                }
            },
            _ => {
                context_lost(app, bot, "no_session_id").await?;
                None
            }
        }
    } else {
        None
    };
    if let Some((session_id, resume_args)) = resume {
        if bot.kind == "codex" {
            let mut resumed = resume_args;
            resumed.extend(args);
            args = resumed;
        } else {
            args.extend(resume_args);
        }
        sqlx::query("UPDATE runs SET resume_session_id = ? WHERE id = ?")
            .bind(&session_id)
            .bind(run_id)
            .execute(&app.db)
            .await
            .map_err(up)?;
    }

    let cwd = bot_cwd(bot, project);
    // A new dir opens on "trust this project?" with the cursor on *No*: claude quits, codex eats
    // the first message. Record trust first (local, only when not yet trusted).
    if project.host == LOCAL_HOST {
        let mut b = bot.clone();
        b.cwd = Some(cwd.to_string());
        for w in crate::trust::pretrust_bots(app, std::slice::from_ref(&b)).await {
            tracing::warn!(bot = %bot.name, cwd, warning = %w, "could not pre-trust the working directory");
        }
    }

    // 2. workspace
    let mut fresh_root: Option<crate::herdr::PaneInfo> = None;
    // `projects.workspace_id` is the configured session's; an imported bot in `default` must not
    // overwrite it (the next reconcile would clear it).
    let workspace_id = match (session.as_str() != "default", project.workspace_id.as_deref()) {
        (true, Some(ws)) if client.workspace_get(ws).await.map_err(up)?.is_some() => ws.to_string(),
        _ => {
            let (ws, root) = client.workspace_create(&project.path, &project.label, env.clone()).await.map_err(up)?;
            if session != "default" {
                sqlx::query("UPDATE projects SET workspace_id = ? WHERE id = ?")
                    .bind(&ws.workspace_id)
                    .bind(&project.id)
                    .execute(&app.db)
                    .await
                    .map_err(up)?;
            }
            fresh_root = Some(root);
            ws.workspace_id
        }
    };

    // 3. pane
    let root = acquire_run_pane(&client, &workspace_id, cwd, &tab_label(bot), &env, fresh_root).await.map_err(up)?;
    let pane_id = root.pane_id;
    let tab_id = root.tab_id;
    let mut pane_guard = StartPaneGuard::new(&client, &workspace_id, &tab_id, &pane_id);

    // 4. persist mapping. Until `agent.start` succeeds, every error must close the pane and tab.
    pane_guard
        .protect(
            sqlx::query("UPDATE runs SET workspace_id = ?, pane_id = ?, tab_id = ? WHERE id = ?")
                .bind(&workspace_id)
                .bind(&pane_id)
                .bind(&tab_id)
                .bind(run_id)
                .execute(&app.db)
                .await
                .map_err(up),
        )
        .await?;

    // 5. agent.start under `<project>-<bot>`
    pane_guard
        .protect(
            sqlx::query("UPDATE runs SET agent_name = ? WHERE id = ?")
                .bind(&agent)
                .bind(run_id)
                .execute(&app.db)
                .await
                .map_err(up),
        )
        .await?;
    // SPEC §4.4a: stamp model/effort read back off the argv, not `bots` — `effort_checked` and
    // `bot.args` can differ, and `bots` changes under a live run.
    let (rt_model, rt_effort) = crate::models::model_effort_from_argv(&bot.kind, &args);
    let rt_fast = i64::from(args.iter().any(|a| a.contains("service_tier=\"priority\"")));
    pane_guard
        .protect(
            sqlx::query("UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ? WHERE id = ?")
                .bind(&rt_model)
                .bind(&rt_effort)
                .bind(rt_fast)
                .bind(run_id)
                .execute(&app.db)
                .await
                .map_err(up),
        )
        .await?;
    // A fresh pane answers `agent_pane_busy: … is not an available shell` until its shell settles
    // (2026-09-06: one of six back-to-back starts lost, 300 ms in); hence the retry.
    // SPEC §6.5b: the login shell's profile rebuilds PATH after the pane env (macOS 2026-09-07:
    // `path_helper` + `brew shellenv`), so the shim prepend is typed into the shell before
    // `agent.start`; the pty buffers it if the shell isn't up yet.
    if let Some(dir) = shim_dir.as_deref() {
        let line = format!(" export PATH={}:\"$PATH\"\n", sh_quote(dir));
        if let Err(e) = client.pane_send_text(&pane_id, &line).await {
            tracing::warn!(bot = %bot.name, pane = %pane_id, error = %e, "could not prepend the herdr shim to the pane PATH");
        }
    }
    let mut started = false;
    for attempt in 0..10u32 {
        match client.agent_start(&agent, &bot.kind, &pane_id, &args, 60_000).await {
            Ok(_) => {
                started = true;
                break;
            }
            Err(e) if pane_not_ready(&e) => {
                tracing::debug!(bot = %bot.name, attempt, error = %e, "pane is not an available shell yet");
                tokio::time::sleep(Duration::from_millis(300 + 200 * u64::from(attempt))).await;
            }
            Err(e) => {
                pane_guard.cleanup().await;
                return Err(up(e));
            }
        }
    }
    if !started {
        pane_guard.cleanup().await;
        return Err(up(format!("pane {pane_id} never became an available shell")));
    }
    pane_guard.disarm();

    // 6. per-run status subscription
    crate::events::watch_pane_on_session(app, &host, &session, &pane_id).await;

    // 7. wait for readiness
    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    match client.agent_wait(&agent, &until, 60_000).await {
        Ok(info) => {
            let st = info.agent_status.normalized();
            set_run(app, run_id, "running", st.as_str()).await;
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "agent.wait did not settle");
            // Do NOT close the pane on timeout (SPEC §6.2.7).
            match client.agent_get(&agent).await {
                Ok(Some(info)) => set_run(app, run_id, "running", info.agent_status.normalized().as_str()).await,
                _ => {
                    close_pane_and_tab(&client, Some(&workspace_id), Some(&tab_id), &pane_id).await;
                    return Err(up(e));
                }
            }
        }
    }
    // Codex account notices are TUI history rows, not in `notify`'s last message: snapshot the pane later.
    if bot.kind == "codex" {
        schedule_codex_notice_capture(app, &bot.id, run_id);
    }
    app.emit_bot_status(&bot.id).await;
    Ok(())
}

/// Find the kind's executable via the user's login shell (`$SHELL -lic`), else PATH. Returns a
/// user-facing reason when missing; a lookup that itself fails passes.
async fn ensure_kind_installed(app: &Arc<App>, host: &str, kind: &str) -> Result<(), String> {
    if !crate::config::valid_kind(kind) {
        return Err(format!("未知的 bot kind `{kind}`"));
    }
    let probe = format!(
        "( \"${{SHELL:-/bin/sh}}\" -lic 'command -v {kind}' 2>/dev/null || command -v {kind} 2>/dev/null ) | tail -1"
    );
    let found: Option<String> = if host == LOCAL_HOST {
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("/bin/sh").arg("-c").arg(&probe).output(),
        )
        .await;
        match out {
            Ok(Ok(o)) => Some(String::from_utf8_lossy(&o.stdout).trim().to_string()),
            _ => None, // could not probe → do not block the start
        }
    } else {
        match app.hosts.get(host).await {
            Some(conn) => match conn.ssh_exec_path(&probe).await {
                Ok(o) => Some(o.trim().to_string()),
                Err(e) => {
                    tracing::warn!(host, kind, error = %e, "kind preflight could not run; continuing");
                    None
                }
            },
            None => None,
        }
    };
    match found {
        Some(path) if path.is_empty() => {
            let where_ = if host == LOCAL_HOST { "本機".to_string() } else { format!("主機 {host}") };
            Err(format!(
                "{where_}上找不到 `{kind}` 執行檔（用登入 shell 檢查 `command -v {kind}` 沒有結果）。請先在該主機安裝 {kind}，或確認它在登入 shell 的 PATH 中；遠端主機也可在主機設定的 remote_path 補上路徑。"
            ))
        }
        Some(path) => {
            tracing::debug!(host, kind, %path, "kind preflight ok");
            Ok(())
        }
        None => Ok(()),
    }
}

async fn set_run(app: &Arc<App>, run_id: &str, state: &str, agent_status: &str) {
    let _ = sqlx::query("UPDATE runs SET state = ?, agent_status = ? WHERE id = ?")
        .bind(state)
        .bind(agent_status)
        .bind(run_id)
        .execute(&app.db)
        .await;
}


/// Codex's account hint is outside any turn (not in `agent-turn-complete`); the terminal is the only source.
const CODEX_NOTICE_DELAY: Duration = Duration::from_millis(500);

/// Best-effort delayed read of Codex's hint; re-checks the run id so an old read can't land on a new run.
pub fn schedule_codex_notice_capture(app: &Arc<App>, bot_id: &str, run_id: &str) {
    let app = app.clone();
    let bot_id = bot_id.to_string();
    let run_id = run_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(CODEX_NOTICE_DELAY).await;
        let lock = app.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if let Err(e) = capture_codex_usage_notices(&app, &bot_id, &run_id).await {
            tracing::debug!(bot = %bot_id, run = %run_id, error = ?e, "codex notice capture failed");
        }
    });
}

/// Persist newly seen Codex account notices (reset available or hard limit). Caller holds the bot lock.
pub async fn capture_codex_usage_notices(app: &Arc<App>, bot_id: &str, expected_run_id: &str) -> anyhow::Result<()> {
    let Some(run) = db::active_run(&app.db, bot_id).await? else { return Ok(()) };
    if run.id != expected_run_id {
        return Ok(());
    }
    let Some(bot) = db::bot(&app.db, bot_id).await? else { return Ok(()) };
    if bot.kind != "codex" {
        return Ok(());
    }
    let Some(pane_id) = run.pane_id.as_deref() else { return Ok(()) };
    let Some(client) = app.herdr_for_run(&run).await else { return Ok(()) };
    let read = client.pane_read(pane_id, "recent_unwrapped", 200).await?;
    let conversation_id = db::conversation_id(&app.db, bot_id).await?;

    for notice in codex_usage_notice_lines(&read.text) {
        // 去重只看這個 run 開始之後：比對整段對話時，兩天前一樣的上限橫幅讓這次被跳過，
        // 額度沒標、回合沒解開（2026-09-12 使用者）。
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM messages
             WHERE conversation_id=? AND role='system' AND source='system' AND content=? AND created_at >= ?)",
        )
        .bind(&conversation_id)
        .bind(&notice)
        .bind(&run.started_at)
        .fetch_one(&app.db)
        .await?;
        let fresh = exists == 0;
        if fresh {
            // No pane snapshot: the idle splash is a boxed TUI, not a failed cut of a reply.
            insert_message(app, &conversation_id, None, "system", &notice, "system", false, None).await?;
            tracing::info!(bot = %bot.name, notice = %notice, "codex account notice captured");
        }
        if codex_limit_hit_line(&notice).is_some() {
            // 有回合在飛＝橫幅就是那句的答案，照樣處理；否則只認這個 run 內第一次看到的，
            // 舊橫幅才不會反覆把額度打回 100%。
            let in_flight = db::in_flight_turn(&app.db, &run.id).await?;
            if !fresh && in_flight.is_none() {
                continue;
            }
            let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
            apply_codex_limit_hit_quota(app, &host, bot.identity.as_deref(), &notice).await;
            // Unlock the composer: a limit hit is a failed turn, not a silent idle.
            if let Some(turn) = in_flight {
                let res = sqlx::query(
                    "UPDATE turns SET status='failed', completed_at=? WHERE id=? AND status='in_flight'",
                )
                .bind(db::now())
                .bind(&turn.id)
                .execute(&app.db)
                .await?;
                if res.rows_affected() > 0 {
                    emit_turn(app, &turn.id).await;
                }
            }
        }
    }
    Ok(())
}


pub async fn stop_bot(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    stop_bot_locked(app, bot_id).await
}

/// Is this run sitting in the user's own herdr `default` session (SPEC §6.5.1)?
pub fn in_default_session(run: &db::Run) -> bool {
    run.herdr_session.as_deref() == Some("default")
}

/// SPEC §6.5.1: a bot from the user's `default` session is observed, never driven — start
/// would create a workspace in their session, restart would close their pane (review 2026-09-12 #4).
fn refuse_default_session(bot: &db::Bot) -> LcResult<()> {
    if bot.herdr_session.as_deref() == Some("default") {
        return Err(LcError::conflict(
            "default_session",
            json!({"bot_id": bot.id,
                   "message": "這顆是從你自己的 herdr default session 匯入的，daemon 只觀察、不替它開或關 pane：要重啟請在那個終端裡自己做。"}),
        ));
    }
    Ok(())
}

/// [`stop_bot`] with the lock already held, so a restart stops and starts under one guard.
pub async fn stop_bot_locked(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? else { return Ok(false) };
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    let client = client_for_run(app, &run).await?;

    let _ = sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run.id).execute(&app.db).await;
    app.emit_bot_status(bot_id).await;
    fail_in_flight(app, &run.id, "run stopped by user").await;

    let target = db::run_target(&run, &bot);
    for _ in 0..2 {
        let _ = client.agent_send_keys(&target, &["ctrl+c".to_string()]).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut gone = false;
    for _ in 0..20 {
        let agent = client.agent_get(&target).await;
        let pane = match run.pane_id.as_deref() {
            Some(p) => client.pane_get(p).await.ok().flatten(),
            None => None,
        };
        if matches!(agent, Ok(None)) || pane.is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Close the Run's pane (and its tab if owned) so no bare shell lingers — except in the user's
    // `default` session (SPEC §6.5.1): only ctrl+c; closing took their terminal (review 2026-09-12 #4).
    if in_default_session(&run) {
        if !gone {
            tracing::warn!(bot = %bot.name, "agent did not exit within 10s; its pane is the user's own and is left open");
        }
    } else {
        if let Some(p) = run.pane_id.as_deref() {
            close_pane_and_tab(&client, run.workspace_id.as_deref(), run.tab_id.as_deref(), p).await;
        }
        if !gone {
            tracing::warn!(bot = %bot.name, "agent did not exit within 10s; pane closed forcibly");
        }
    }
    let _ = sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(db::now())
        .bind(&run.id)
        .execute(&app.db)
        .await;
    if let Some(p) = run.pane_id.as_deref() {
        if let Some(session) = app.session_for_run(&run).await {
            crate::events::unwatch_pane_on_session(app, &host, &session, p).await;
        }
    }
    app.emit_bot_status(bot_id).await;
    Ok(true)
}

/// stop (if running) + start. Used to make edited `model` / `args` / `identity` / `env` take effect.
pub async fn restart_bot(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    restart_bot_with(app, bot_id, StartOpts::default()).await
}

/// Stop + start under one hold of the bot's lock. Two holds let a reconcile adopt the just-stopped
/// agent in between (2026-09-10 23:02, `restart-idle`: AGM + three bots down 5.5 h).
/// If a run whose pane is gone still blocks the start, it is ended and the start retried once.
pub async fn restart_bot_with(app: &Arc<App>, bot_id: &str, opts: StartOpts) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    // Checked before the stop, or the user's agent gets ctrl+c for nothing.
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    refuse_default_session(&bot)?;
    stop_bot_locked(app, bot_id).await?;
    match start_bot_locked_with(app, bot_id, opts).await {
        Err(LcError::Conflict(v)) if v.get("reason").and_then(|r| r.as_str()) == Some("active run already exists") => {
            let Some(run) = db::active_run(&app.db, bot_id).await.map_err(up)? else {
                return start_bot_locked_with(app, bot_id, opts).await;
            };
            let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
            if run_alive(app, &run, &bot).await {
                return Err(LcError::Conflict(v));
            }
            tracing::warn!(bot = %bot.name, run = %run.id, pane = ?run.pane_id, "restart found a run with no live pane in its way; ending it and starting again");
            mark_run_exited(app, &run.id, "its pane was gone when the bot restarted").await;
            start_bot_locked_with(app, bot_id, opts).await
        }
        other => other,
    }
}

/// 子 agent 原地重啟（SPEC §6.5a / §6.9）：在它自己的 pane 裡 `ctrl+c` 收掉、**不關 pane**，
/// 同名 `agent.start --resume <上一個 session>`。pane 是父 agent 開的，且 shell 裡的環境
/// （`CLAUDE_CONFIG_DIR`、shim）daemon 重建不了。沒注入 hook，回覆照舊走終端快照。
/// 過程中 pane 不見了就不重開。
pub async fn restart_child_in_pane(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.managed_by != "child" {
        return Err(LcError::Bad("這不是 agent spawn 出來的子 agent".into()));
    }
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let Some(pane_id) = run.pane_id.clone() else {
        return Err(LcError::Bad("這個子 agent 的 run 沒有記到 pane".into()));
    };
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    let client = client_for_run(app, &run).await?;
    let agent = run.agent_name.clone().unwrap_or_else(|| bot.name.clone());

    let _ = sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run.id).execute(&app.db).await;
    app.emit_bot_status(bot_id).await;
    fail_in_flight(app, &run.id, "restarted to apply the CLI update").await;

    let target = db::run_target(&run, &bot);
    for _ in 0..2 {
        let _ = client.agent_send_keys(&target, &["ctrl+c".to_string()]).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut empty = false;
    for _ in 0..20 {
        match client.pane_get(&pane_id).await {
            // pane 沒了：子 agent 結束，run 收掉，不在別處重開。
            Ok(None) => {
                mark_run_exited(app, &run.id, "子 agent 的 pane 在重啟過程中被關掉").await;
                app.emit_bot_status(bot_id).await;
                return Err(LcError::Bad("這個子 agent 的 pane 已經被關掉了".into()));
            }
            Ok(Some(p)) if p.agent.is_none() => {
                empty = true;
                break;
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !empty {
        // agent 還在：run 轉回 running。留在 `stopping` 會讓 prompt 409、start 拒絕、reconcile
        // 也不救——永遠黃燈（2026-09-12 review #1）。
        let _ = sqlx::query("UPDATE runs SET state='running' WHERE id=? AND state='stopping'")
            .bind(&run.id)
            .execute(&app.db)
            .await;
        app.emit_bot_status(bot_id).await;
        return Err(LcError::Upstream("子 agent 十秒內沒有退出，沒有動它的 pane".into()));
    }
    let _ = sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(db::now())
        .bind(&run.id)
        .execute(&app.db)
        .await;

    let mut args: Vec<String> = Vec::new();
    if bot.auto_approve != 0 {
        match bot.kind.as_str() {
            "claude" => args.push("--dangerously-skip-permissions".into()),
            "codex" => args.push("--yolo".into()),
            "grok" => args.push("--always-approve".into()),
            _ => {}
        }
    }
    // 模型／強度用 `bots` 上 §4.4a 從子 agent argv 讀回的；讀不到就讓 CLI 用預設。
    args.extend(model_args(&effort_checked(app, &bot, &host).await));
    args.extend(bot.args());
    let resume = match db::last_native_session(&app.db, bot_id).await.map_err(up)? {
        Some((sid, _)) => resume_args_by_kind(&bot.kind, &sid).ok().map(|a| (sid, a)),
        None => None,
    };
    if let Some((_, resume_args)) = resume.clone() {
        if bot.kind == "codex" {
            let mut resumed = resume_args;
            resumed.extend(args);
            args = resumed;
        } else {
            args.extend(resume_args);
        }
    } else {
        tracing::info!(bot = %bot.name, "子 agent 沒有可接續的 session，重啟後從新的對話開始");
    }

    let run_id = db::ulid();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, resume_session_id, started_at)
         VALUES (?,?,'starting','unknown',?,?,?,1,?,?,?,?)",
    )
    .bind(&run_id)
    .bind(bot_id)
    .bind(&run.workspace_id)
    .bind(&pane_id)
    .bind(&run.tab_id)
    .bind(&agent)
    .bind(&run.herdr_session)
    .bind(resume.as_ref().map(|(sid, _)| sid.clone()))
    .bind(db::now())
    .execute(&app.db)
    .await
    .map_err(up)?;

    let mut started = false;
    for attempt in 0..10u32 {
        match client.agent_start(&agent, &bot.kind, &pane_id, &args, 60_000).await {
            Ok(_) => {
                started = true;
                break;
            }
            Err(e) if pane_not_ready(&e) => {
                tracing::debug!(bot = %bot.name, attempt, error = %e, "子 agent 的 pane 還沒回到可用的 shell");
                tokio::time::sleep(Duration::from_millis(300 + 200 * u64::from(attempt))).await;
            }
            Err(e) => {
                mark_run_exited(app, &run_id, "子 agent 重啟時 agent.start 失敗").await;
                app.emit_bot_status(bot_id).await;
                return Err(up(e));
            }
        }
    }
    if !started {
        mark_run_exited(app, &run_id, "子 agent 的 pane 一直不是可用的 shell").await;
        app.emit_bot_status(bot_id).await;
        return Err(LcError::Upstream(format!("pane {pane_id} never became an available shell")));
    }
    let _ = sqlx::query("UPDATE runs SET state='running' WHERE id=?").bind(&run_id).execute(&app.db).await;
    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    if let Err(e) = client.agent_wait(&agent, &until, 60_000).await {
        tracing::warn!(bot = %bot.name, error = %e, "子 agent 重啟後沒等到 ready，run 留著讓對帳接手");
    }
    // 沒有 hook 的 run，畫面是唯一來源（同收編）。
    spawn_adopted_capture(app, &run_id, bot_id);
    app.emit_bot_status(bot_id).await;
    app.emit("bot_changed", json!({"bot_id": bot_id})).await;
    tracing::info!(bot = %bot.name, pane = %pane_id, run = %run_id, "子 agent 在原本的 pane 裡重啟完成");
    Ok(run_id)
}

/// Is this run's pane open and its agent listed? A failed RPC counts as alive: ending a live bot
/// over a hiccup is the worse mistake.
pub async fn run_alive(app: &Arc<App>, run: &db::Run, bot: &db::Bot) -> bool {
    let Some(pane) = run.pane_id.as_deref() else { return false };
    let Ok(client) = client_for_run(app, run).await else { return true };
    match client.pane_get(pane).await {
        Ok(None) => return false,
        // The pane says an agent is in it: `agent.get` by name can briefly miss after a same-named
        // restart on a new pane (2026-09-11).
        Ok(Some(p)) if p.agent.is_some() => return true,
        _ => {}
    }
    !matches!(client.agent_get(&db::run_target(run, bot)).await, Ok(None))
}

/// #61: one-time purge of `bots/<id>/` for soft-deleted bots with no live run. Dirs no bot row
/// claims are left alone (rt-87's `bots-orphan-backup-2026-09-10/` is outside `bots/`).
pub async fn purge_deleted_bot_dirs(app: &Arc<App>) -> usize {
    let root = app.data_dir.join("bots");
    let Ok(entries) = std::fs::read_dir(&root) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else { continue };
        let deleted: Option<Option<String>> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id = ?")
            .bind(&id)
            .fetch_optional(&app.db)
            .await
            .ok()
            .flatten();
        if !matches!(deleted, Some(Some(_))) {
            continue;
        }
        if matches!(db::active_run(&app.db, &id).await, Ok(Some(_))) {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(dir = %entry.path().display(), error = %e, "could not remove a deleted bot's directory"),
        }
    }
    if removed > 0 {
        tracing::info!(removed, "removed bots/<id>/ directories left behind by deleted bots");
    }
    removed
}

pub async fn purge_bot_dir(app: &Arc<App>, bot_id: &str, host: &str) {
    if !valid_id(bot_id) {
        tracing::warn!(host, bot = %bot_id, "invalid bot id; bot config dir left in place");
        return;
    }
    if host == LOCAL_HOST {
        let Ok(dir) = app.bot_dir(bot_id) else { return };
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!(dir = %dir.display(), "removed bot config dir"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "could not remove bot config dir"),
        }
        return;
    }
    let Some(conn) = app.hosts.get(host).await else {
        tracing::warn!(host, bot = %bot_id, "unknown host; remote bot dir left in place");
        return;
    };
    let res = async {
        let p = remote_bot_dir(&conn, bot_id).await?;
        conn.ssh_exec(&format!("rm -rf {}\n", sh_quote(&p.dir))).await?;
        Ok::<_, anyhow::Error>(p.dir)
    }
    .await;
    match res {
        Ok(dir) => tracing::info!(host, %dir, "removed remote bot config dir"),
        Err(e) => tracing::warn!(host, bot = %bot_id, error = %format!("{e:#}"), "could not remove remote bot config dir"),
    }
}

#[cfg(test)]
mod bot_dir_safety_tests {
    use super::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn invalid_ids_do_not_access_or_remove_local_bot_dirs() {
        let env = tt::env().await;
        let protected = env.app.data_dir.join("bots").join("keep");
        std::fs::create_dir_all(&protected).unwrap();

        for id in ["../..", "foo/bar", r"..\..", ""] {
            assert!(env.app.bot_dir(id).is_err(), "invalid id was accepted: {id:?}");
            purge_bot_dir(&env.app, id, LOCAL_HOST).await;
            assert!(protected.exists(), "purge touched the protected directory for {id:?}");
        }
    }
}

pub async fn interrupt_bot(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let target = db::run_target(&run, &bot);
    let client = client_for_run(app, &run).await?;
    client.agent_send_keys(&target, &["esc".to_string()]).await.map_err(up)?;
    fail_in_flight(app, &run.id, "interrupted by user").await;
    clear_restored_prompt(&client, &run, &bot).await;
    Ok(())
}

/// claude 在吐出第一個字前被 `esc` 打斷，會把 prompt 放回輸入框（2026-09-08 實測），下一則
/// 貼上會黏在後面。中斷後 composer 有字就 `ctrl+c` 清掉（有字時只清不退出）。只做 claude，失敗不報錯。
async fn clear_restored_prompt(client: &HerdrClient, run: &db::Run, bot: &db::Bot) {
    if bot.kind != "claude" {
        return;
    }
    let Some(pane) = run.pane_id.as_deref() else { return };
    tokio::time::sleep(Duration::from_millis(400)).await;
    let Ok(read) = client.pane_read(pane, "visible", 80).await else { return };
    if composer_text(&bot.kind, &read.text).is_none() {
        return;
    }
    match client.pane_send_keys(pane, &["ctrl+c"]).await {
        Ok(()) => tracing::info!(run = %run.id, "interrupt put the prompt back into the composer; cleared it"),
        Err(e) => tracing::warn!(run = %run.id, error = ?e, "could not clear the restored prompt from the composer"),
    }
}

/// 強制結束目前回合（`POST /api/bots/:id/abort`）。和 [`interrupt_bot`] 相反，先保證 DB 解開、
/// 送 `esc` 只是盡力（`keys_sent`）：in-flight 標 failed、`delivery = unknown` 也一併收（§6.3，
/// 同樣鎖輸入框）。沒有 active run 不算錯——那正是最需要這支的情況。
pub async fn abort_turns(app: &Arc<App>, bot_id: &str) -> LcResult<Value> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?;

    let mut keys_sent = false;
    let mut key_error: Option<String> = None;
    let mut client: Option<HerdrClient> = None;
    if let Some(r) = run.as_ref() {
        let target = db::run_target(r, &bot);
        match client_for_run(app, r).await {
            Ok(c) => match c.agent_send_keys(&target, &["esc".to_string()]).await {
                Ok(()) => {
                    keys_sent = true;
                    client = Some(c);
                }
                Err(e) => key_error = Some(format!("{e:#}")),
            },
            Err(e) => key_error = Some(format!("{e:?}")),
        }
    }

    let mut aborted: Vec<String> = Vec::new();
    if let Some(r) = run.as_ref() {
        if let Ok(Some(t)) = db::in_flight_turn(&app.db, &r.id).await {
            aborted.push(t.id.clone());
        }
        fail_in_flight(app, &r.id, "回合已由使用者強制中止").await;
        if let Some(c) = client.as_ref() {
            clear_restored_prompt(c, r, &bot).await;
        }
    }
    // Unknown-delivery turns live on the bot, not a run: a stopped run can leave one behind.
    let unknown = sqlx::query_as::<_, db::Turn>(
        "SELECT t.* FROM turns t JOIN conversations c ON c.id = t.conversation_id
         WHERE c.bot_id = ? AND (t.status = 'in_flight' OR t.delivery = 'unknown')",
    )
    .bind(bot_id)
    .fetch_all(&app.db)
    .await
    .map_err(up)?;
    for t in unknown {
        if aborted.contains(&t.id) && t.delivery != "unknown" {
            continue;
        }
        sqlx::query(
            "UPDATE turns SET status='failed',
             delivery = CASE WHEN delivery='unknown' THEN 'failed' ELSE delivery END,
             completed_at=? WHERE id=?",
        )
        .bind(db::now())
        .bind(&t.id)
        .execute(&app.db)
        .await
        .map_err(up)?;
        if !aborted.contains(&t.id) {
            let _ = insert_message(app, &t.conversation_id, Some(&t.id), "system", "回合已由使用者強制中止", "system", false, None).await;
            aborted.push(t.id.clone());
        }
        emit_turn(app, &t.id).await;
    }

    tracing::info!(bot = %bot.name, keys_sent, aborted = aborted.len(), "turn(s) force-aborted by user");
    Ok(json!({"aborted": aborted, "keys_sent": keys_sent, "key_error": key_error}))
}

/// Give a running pre one-bot-one-tab bot its own tab. A move, not a restart: herdr keeps
/// `pane_id` across `pane.move` (0.8.2), so mapping, poller and in-flight turn carry on.
/// Idempotent: re-moving a solo pane would rebuild a tab and renumber the user's tab bar.
pub async fn move_pane_to_own_tab(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let pane_id = run
        .pane_id
        .clone()
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| LcError::NotFound("pane".into()))?;
    let client = client_for_run(app, &run).await?;

    // `runs.tab_id` is NULL for old runs and stale if the user dragged the pane.
    let pane = client.pane_get(&pane_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("pane".into()))?;
    let workspace_id = pane.workspace_id.clone();
    let current_tab = pane.tab_id.clone();

    let solo = client
        .tab_list(&workspace_id)
        .await
        .map_err(up)?
        .into_iter()
        .find(|t| t.tab_id == current_tab)
        .map(|t| t.pane_count <= 1)
        .unwrap_or(false);
    let tab_id = if solo {
        current_tab
    } else {
        let (new_tab, previous) = client.pane_move_to_new_tab(&pane_id, &tab_label(&bot)).await.map_err(up)?;
        // The shared tidy-up is idempotent and the one place that decides a tab may go.
        if !previous.is_empty() && previous != new_tab {
            close_tab_if_empty(&client, &workspace_id, &previous).await;
        }
        new_tab
    };

    sqlx::query("UPDATE runs SET workspace_id = ?, tab_id = ? WHERE id = ?")
        .bind(&workspace_id)
        .bind(&tab_id)
        .bind(&run.id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    app.emit_bot_status(bot_id).await;
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
    let target = db::run_target(&run, &bot);
    client_for_run(app, &run).await?.agent_send_keys(&target, &keys).await.map_err(up)?;
    Ok(())
}

/// `POST /api/bots/:id/text` — 把（多行）文字打進 pane，選擇性按 Enter。`\n` 不是鍵名，所以不走
/// `send_keys`；Enter 另用 `pane.send_keys`（`send_text` 裡的 `\n` 是貼上換行）。不擋
/// `agent_status`：用途就是回合中「併送」。
pub async fn send_text(app: &Arc<App>, bot_id: &str, text: &str, enter: bool, expect_run_id: Option<String>) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    if let Some(exp) = expect_run_id {
        if exp != run.id {
            return Err(LcError::conflict("run mismatch", json!({"run_id": run.id})));
        }
    }
    let pane_id = run
        .pane_id
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| LcError::NotFound("pane".into()))?
        .to_string();
    let client = client_for_run(app, &run).await?;
    if !text.is_empty() {
        client.pane_send_text(&pane_id, text).await.map_err(up)?;
    }
    if enter {
        client.pane_send_keys(&pane_id, &["enter"]).await.map_err(up)?;
    }
    Ok(())
}

/// TUI slash command for a live setting, or `None` (caller reports `needs_restart`).
fn live_slash_command(kind: &str, field: &str, value: &str, effort: Option<&str>) -> Option<String> {
    match (kind, field) {
        ("grok", "effort") | ("claude", "effort") => Some(format!("/effort {}", value.to_ascii_lowercase())),
        ("grok", "model") => {
            let mut line = format!("/model {value}");
            if let Some(e) = effort.map(str::trim).filter(|s| !s.is_empty()) {
                line.push(' ');
                line.push_str(&e.to_ascii_lowercase());
            }
            Some(line)
        }
        ("claude", "model") => Some(format!("/model {value}")),
        _ => None,
    }
}

/// 不重啟就套用設定（SPEC §4.4a）。grok `effort`/`model`、claude `model`/`effort` 走一行 slash
/// 指令；codex 的 `/model` 是不吃參數的選單、`/fast` 是開關，走 [`crate::codex_live`]
/// 送鍵讀畫面再回讀狀態列（2026-09-09 實測）。副作用：claude（2.1.263 實測）與 codex 都會把
/// 選擇存成帳號之後新 session 的預設。
/// 回傳 `None` = 已套用；`Some(理由)` = 退回重啟。理由一路帶回 `live_apply` 並寫 log——
/// 2026-09-13 使用者問 codex 改 effort 為何重啟，當時每個失敗出口都是靜默的。
pub async fn apply_live_setting(app: &Arc<App>, bot_id: &str, fields: &[&str]) -> Option<String> {
    let reason = apply_live_setting_inner(app, bot_id, fields).await;
    match &reason {
        Some(why) => tracing::info!(bot_id, ?fields, reason = %why, "設定沒能當場套用，改用重啟"),
        None => tracing::info!(bot_id, ?fields, "設定已當場套用，不需要重啟"),
    }
    reason
}

async fn apply_live_setting_inner(app: &Arc<App>, bot_id: &str, fields: &[&str]) -> Option<String> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let Ok(Some(bot)) = db::bot(&app.db, bot_id).await else { return Some("bot_missing".into()) };
    let Ok(Some(run)) = db::active_run(&app.db, bot_id).await else { return Some("no_active_run".into()) };
    let in_flight = !matches!(db::in_flight_turn(&app.db, &run.id).await, Ok(None));
    let pane_id = match slash_gate(&run, in_flight) {
        Ok(p) => p,
        Err(why) => return Some(format!("slash_gate: {}", why.reason())),
    };
    let Ok(client) = client_for_run(app, &run).await else { return Some("no_herdr_client".into()) };

    if bot.kind == "codex" {
        // `/fast` 是開關：不知道現在狀態就不能按。
        let was_fast = run.runtime_fast.map(|v| v != 0);
        let seen = match crate::codex_live::apply(&client, &pane_id, &bot, was_fast, fields).await {
            Ok(seen) => seen,
            Err(why) => return Some(format!("codex: {why}")),
        };
        // 回讀的狀態列才是 runtime 的定義（SPEC §4.4a）。
        let _ = sqlx::query("UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ? WHERE id = ?")
            .bind(&seen.model)
            .bind(&seen.effort)
            .bind(i64::from(seen.fast))
            .bind(&run.id)
            .execute(&app.db)
            .await;
        app.emit_bot_status(bot_id).await;
        tracing::info!(bot_id, model = %seen.model, effort = ?seen.effort, fast = seen.fast, "codex applied live");
        return None;
    }

    let [field] = fields else { return Some("not_a_single_field".into()) };
    let value = match *field {
        "effort" => bot.effort.as_deref(),
        "model" => bot.model.as_deref(),
        _ => None,
    };
    // `/effort`、`/model` 都一定要帶值，清成「不指定」沒有 slash 指令。
    let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Some(format!("{field}_cleared_to_default"));
    };
    let Some(line) = live_slash_command(&bot.kind, field, value, bot.effort.as_deref()) else {
        return Some(format!("no_slash_command_for_{field}"));
    };
    if send_slash_line(&client, &pane_id, &line).await.is_err() {
        return Some("slash_send_failed".into());
    }
    // SPEC §4.4a: clear the drift marker only for the field sent (`/model` doesn't touch effort).
    let col = match *field {
        "effort" => "runtime_effort",
        _ => "runtime_model",
    };
    let _ = sqlx::query(&format!("UPDATE runs SET {col} = ? WHERE id = ?"))
        .bind(value)
        .bind(&run.id)
        .execute(&app.db)
        .await;
    if *field == "model" && bot.kind == "grok" {
        if let Some(e) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let _ = sqlx::query("UPDATE runs SET runtime_effort = ? WHERE id = ?")
                .bind(e.to_ascii_lowercase())
                .bind(&run.id)
                .execute(&app.db)
                .await;
        }
    }
    app.emit_bot_status(bot_id).await;
    tracing::info!(bot_id, line, "applied live via slash command");
    None
}

/// 不能送 slash 指令的理由。`apply_live_setting` 只需要「不行」，但使用者按「登入」時靜默失敗是 bug。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashBlocked {
    NotRunning,
    /// `working` / `blocked`：打的字會被吃掉，或掉進權限提示。
    AgentBusy,
    /// 這一行會變成在飛 prompt 的一部分。
    TurnInFlight,
    NoPane,
}

impl SlashBlocked {
    /// 機器可讀理由；文案由前端翻。
    pub fn reason(self) -> &'static str {
        match self {
            SlashBlocked::NotRunning => "not_running",
            SlashBlocked::AgentBusy => "agent_busy",
            SlashBlocked::TurnInFlight => "turn_in_flight",
            SlashBlocked::NoPane => "no_pane",
        }
    }
}

/// 能打字就回 pane id。純函式：`apply_live_setting` 與 `login` 共用，每條擋下的理由都測得到。
fn slash_gate(run: &db::Run, turn_in_flight: bool) -> Result<String, SlashBlocked> {
    if run.state != "running" {
        return Err(SlashBlocked::NotRunning);
    }
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Err(SlashBlocked::AgentBusy);
    }
    if turn_in_flight {
        return Err(SlashBlocked::TurnInFlight);
    }
    run.pane_id
        .clone()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .ok_or(SlashBlocked::NoPane)
}

/// 打一行 slash 指令並送出：先打字、等輸入列畫好、再 Enter（`"/login\n"` 會被當多行貼上）。
/// 送出不等於套用（2026-09-11 AGM）：claude 有對話時 `/model` 會跳「Switch model?」框，留著會吃掉
/// 下一則 prompt、Enter 替人按 Yes、回合 stall。所以回頭看畫面：是那個框 → 按 `1`（使用者已在
/// AG Man 選過）；還在 → Esc 並回 `Err`。絕不把框留在畫面上。
async fn send_slash_line(client: &HerdrClient, pane_id: &str, line: &str) -> LcResult<()> {
    client.pane_send_text(pane_id, line).await.map_err(up)?;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    client.pane_send_keys(pane_id, &["Enter"]).await.map_err(up)?;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let screen = client.pane_read(pane_id, "visible", 60).await.map(|r| r.text).unwrap_or_default();
    if !crate::tui_prompts::is_switch_model_dialog(&screen) {
        return Ok(());
    }
    tracing::info!(pane_id, line, "claude asked to confirm the model switch; answering Yes");
    client.pane_send_keys(pane_id, &["1"]).await.map_err(up)?;
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    let screen = client.pane_read(pane_id, "visible", 60).await.map(|r| r.text).unwrap_or_default();
    if !crate::tui_prompts::is_switch_model_dialog(&screen) {
        return Ok(());
    }
    let _ = client.pane_send_keys(pane_id, &["Escape"]).await;
    tracing::warn!(pane_id, line, "model-switch confirmation would not close; backed out with Esc");
    Err(up(anyhow::anyhow!("claude 的換模型確認框沒有關掉，已按 Esc 退出")))
}


/// 登入／換帳號的 slash 指令（拋棄式 herdr session 實測）：claude 2.1.263 與 grok 1.0.13 是
/// `/login`；codex 0.153.4 沒有（只有 `/logout`），送進去會被當一般 prompt 丟給模型，所以 `None`。
pub fn login_slash_command(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" | "grok" => Some("/login"),
        _ => None,
    }
}

#[derive(serde::Serialize)]
pub struct LoginOut {
    pub run_id: String,
    pub kind: String,
    pub command: String,
}

/// 對正在跑的 bot 送登入指令。同 `apply_live_setting` 的 gate 與打字節奏，但使用者明確按了按鈕，
/// 送不出去要說明理由。daemon 不等登入完成；由 `POST /hosts/:name/tools/refresh` 重新偵測。
pub async fn login(app: &Arc<App>, bot_id: &str) -> LcResult<LoginOut> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let Some(line) = login_slash_command(&bot.kind) else {
        return Err(LcError::BadValue(json!({
            "error": "login_unsupported",
            "kind": bot.kind,
            "message": format!("{} has no in-session login command", bot.kind),
        })));
    };
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| {
        LcError::conflict(SlashBlocked::NotRunning.reason(), json!({ "bot_id": bot_id }))
    })?;
    let in_flight = db::in_flight_turn(&app.db, &run.id).await.map_err(up)?.is_some();
    let pane_id = slash_gate(&run, in_flight)
        .map_err(|b| LcError::conflict(b.reason(), json!({"bot_id": bot_id, "run_id": run.id})))?;
    let client = client_for_run(app, &run).await?;
    send_slash_line(&client, &pane_id, line).await?;
    tracing::info!(bot_id, kind = %bot.kind, line, "sent login slash command");
    Ok(LoginOut { run_id: run.id, kind: bot.kind, command: line.to_string() })
}

#[cfg(test)]
mod resume_args_tests {
    use super::{resume_args_by_kind, start_bot, start_bot_with, stop_bot, StartOpts};
    use crate::db;
    use crate::testing::{claude_bot, env, Env};
    use serde_json::Value;

    fn started_args(e: &Env) -> Vec<Vec<String>> {
        e.herdr
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "agent.start")
            .filter_map(|(_, params)| {
                params
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|args| args.iter().filter_map(Value::as_str).map(String::from).collect())
            })
            .collect()
    }

    #[test]
    fn provider_resume_arguments_are_exact() {
        assert_eq!(resume_args_by_kind("claude", "sid-1").unwrap(), vec!["--resume", "sid-1"]);
        assert_eq!(resume_args_by_kind("codex", "sid-1").unwrap(), vec!["resume", "sid-1"]);
        assert_eq!(resume_args_by_kind("grok", "sid-1"), Err("unsupported_kind"));
        assert_eq!(resume_args_by_kind("claude", ""), Err("no_session_id"));
    }

    /// Continuation is opt-in: only `resume_native` gets `--resume <id>`.
    #[tokio::test]
    async fn native_resume_is_opt_in() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
             VALUES (?,?,'stopped','idle',?,?,?)",
        )
        .bind(db::ulid())
        .bind(&pm.id)
        .bind("native-previous")
        .bind("2026-09-07T00:00:00Z")
        .bind("2026-09-07T00:01:00Z")
        .execute(&e.app.db)
        .await
        .unwrap();

        start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true }).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        assert!(args.windows(2).any(|w| w == ["--resume", "native-previous"]));
        stop_bot(&e.app, &pm.id).await.unwrap();

        start_bot(&e.app, &pm.id).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        assert!(!args.contains(&"--resume".into()));
        stop_bot(&e.app, &pm.id).await.unwrap();
    }

    /// No transcript → not resumed: `claude --resume` would exit with "No conversation found".
    #[tokio::test]
    async fn a_session_without_a_transcript_on_disk_is_not_resumed() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        let ended = |sid: &str, transcript: &str, at: &str| {
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
                 VALUES (?,?,'stopped','idle',?,?,?,?)",
            )
            .bind(db::ulid())
            .bind(pm.id.clone())
            .bind(sid.to_string())
            .bind(transcript.to_string())
            .bind(at.to_string())
            .bind(at.to_string())
        };
        let missing = e.dir.join("never-written.jsonl");
        ended("sid-unwritten", missing.to_str().unwrap(), "2026-09-11T00:00:00Z").execute(&e.app.db).await.unwrap();
        start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true }).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        assert!(!args.contains(&"--resume".into()), "resumed a session that has no transcript: {args:?}");
        stop_bot(&e.app, &pm.id).await.unwrap();

        let written = e.dir.join("written.jsonl");
        std::fs::write(&written, "{}\n").unwrap();
        ended("sid-written", written.to_str().unwrap(), "2026-09-11T00:10:00Z").execute(&e.app.db).await.unwrap();
        start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true }).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        assert!(args.windows(2).any(|w| w == ["--resume", "sid-written"]), "{args:?}");
        stop_bot(&e.app, &pm.id).await.unwrap();
    }

    /// 2026-09-10 23:02: restart repeatedly beside a tight reconcile loop; every restart must
    /// return the run it started, alive (no adoption in a stop/start gap).
    #[tokio::test]
    async fn restarts_racing_a_reconcile_loop_always_come_back() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        start_bot(&e.app, &pm.id).await.unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (app2, stop2) = (e.app.clone(), stop.clone());
        let looper = tokio::spawn(async move {
            let mut rounds = 0u32;
            while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = crate::reconcile::reconcile_host(&app2, crate::config::LOCAL_HOST).await;
                rounds += 1;
                tokio::task::yield_now().await;
            }
            rounds
        });

        for i in 0..15 {
            let started = match crate::lifecycle::restart_bot_with(&e.app, &pm.id, StartOpts::default()).await {
                Ok(id) => id,
                Err(_) => panic!("restart {i} was refused while a reconcile was running"),
            };
            let active = db::active_run(&e.app.db, &pm.id).await.unwrap().expect("a run after the restart");
            assert_eq!(active.id, started, "restart {i}: the active run is the one the restart started, not an adopted leftover");
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let rounds = looper.await.unwrap();
        assert!(rounds > 0, "the reconcile loop actually ran alongside the restarts");

        let run = db::active_run(&e.app.db, &pm.id).await.unwrap().unwrap();
        let bot = db::bot(&e.app.db, &pm.id).await.unwrap().unwrap();
        assert!(crate::lifecycle::run_alive(&e.app, &run, &bot).await, "the surviving run has a live pane and agent");
        let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=? AND state IN ('starting','running','stopping')")
            .bind(&pm.id)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(live, 1, "exactly one live run");
    }
}

#[cfg(test)]
mod abort_tests {
    use super::*;

    /// `abort_turns` must work when `esc` cannot be delivered (no herdr here): the turn still leaves `in_flight`.
    #[tokio::test]
    async fn unlocks_even_when_the_keys_cannot_be_sent() {
        let dir = std::env::temp_dir().join(format!("am-abort-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
        let app = crate::state::App::new(
            pool,
            client.clone(),
            client,
            cfg,
            dir.clone(),
            dir.join("agents-managerd"),
            7799,
            "t".into(),
            "test".into(),
            false,
        );

        let (pid, bid, rid, cid) = (db::ulid(), db::ulid(), db::ulid(), db::ulid());
        let now = db::now();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,'local',?)")
            .bind(&pid).bind("/tmp/p").bind("p").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?,?,?,?)")
            .bind(&bid).bind(&pid).bind("b").bind("claude").bind("tok").bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO runs (id, bot_id, state, pane_id, started_at) VALUES (?,?,'running','w1:p1',?)")
            .bind(&rid).bind(&bid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations (id, bot_id, created_at) VALUES (?,?,?)")
            .bind(&cid).bind(&bid).bind(&now).execute(&app.db).await.unwrap();
        // Both an in-flight turn and an unknown-delivery one block the next prompt.
        let (t_flight, t_unknown) = (db::ulid(), db::ulid());
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&t_flight).bind(&cid).bind(&rid).bind(&now).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','failed','unknown',?)")
            .bind(&t_unknown).bind(&cid).bind(&rid).bind(&now).execute(&app.db).await.unwrap();

        let out = abort_turns(&app, &bid).await.expect("abort must not fail just because the keys did");
        assert_eq!(out["keys_sent"], false, "no herdr behind the socket");
        let aborted = out["aborted"].as_array().unwrap();
        assert_eq!(aborted.len(), 2, "both the in-flight and the unknown-delivery turn: {out}");

        let flight: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_flight).fetch_one(&app.db).await.unwrap();
        assert_eq!(flight, ("failed".into(), "ok".into()), "in-flight turn is closed, delivery untouched");
        let unknown: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?")
            .bind(&t_unknown).fetch_one(&app.db).await.unwrap();
        assert_eq!(unknown, ("failed".into(), "failed".into()), "unknown delivery is resolved, not left to block");
        assert!(db::in_flight_turn(&app.db, &rid).await.unwrap().is_none(), "nothing left in flight");

        // Idempotent: nothing to abort the second time round.
        let again = abort_turns(&app, &bid).await.unwrap();
        assert_eq!(again["aborted"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod live_slash_tests {
    use super::live_slash_command;

    #[test]
    fn grok_effort_and_model() {
        assert_eq!(
            live_slash_command("grok", "effort", "HIGH", None).as_deref(),
            Some("/effort high")
        );
        assert_eq!(
            live_slash_command("grok", "model", "grok-4.6", Some("high")).as_deref(),
            Some("/model grok-4.6 high")
        );
        assert_eq!(
            live_slash_command("grok", "model", "grok-4.5", None).as_deref(),
            Some("/model grok-4.5")
        );
        assert_eq!(
            live_slash_command("grok", "model", "grok-4.5", Some("  ")).as_deref(),
            Some("/model grok-4.5")
        );
    }

    /// claude takes both, but its `/model` has no second parameter the way grok's does.
    #[test]
    fn claude_model_and_effort() {
        assert_eq!(
            live_slash_command("claude", "model", "opus", Some("high")).as_deref(),
            Some("/model opus")
        );
        assert_eq!(
            live_slash_command("claude", "effort", "MAX", None).as_deref(),
            Some("/effort max")
        );
        // codex has no slash for either (0.153.4).
        assert_eq!(live_slash_command("codex", "model", "gpt-5.5", None), None);
        assert_eq!(live_slash_command("codex", "effort", "high", None), None);
    }
}

#[cfg(test)]
mod login_slash_tests {
    use super::{login_slash_command, slash_gate, SlashBlocked};
    use crate::db;

    fn run(state: &str, agent_status: &str, pane_id: Option<&str>) -> db::Run {
        db::Run {
            id: "r1".into(),
            bot_id: "b1".into(),
            state: state.into(),
            agent_status: agent_status.into(),
            workspace_id: Some("w1".into()),
            pane_id: pane_id.map(str::to_string),
            tab_id: Some("w1:t1".into()),
            adopted: 0,
            agent_name: None,
            herdr_session: None,
            agent_title: None,
            status_line: None,
            status_json: None,
            runtime_model: None,
            runtime_effort: None,
            runtime_fast: None,
            update_notice: None,
            turn_error: None,
            native_session_id: None,
            transcript_path: None,
            last_read_revision: None,
            last_read_tail_hash: None,
            started_at: "2026-01-01T00:00:00Z".into(),
            ended_at: None,
            resume_session_id: None,
        }
    }

    /// codex 沒有 `/login`（只有 `/logout`），回「不支援」好過送不存在的指令。
    #[test]
    fn only_claude_and_grok_can_log_in_from_the_tui() {
        assert_eq!(login_slash_command("claude"), Some("/login"));
        assert_eq!(login_slash_command("grok"), Some("/login"));
        assert_eq!(login_slash_command("codex"), None);
        assert_eq!(login_slash_command(""), None);
        assert_eq!(login_slash_command("Claude"), None);
    }

    #[test]
    fn an_idle_running_pane_is_typeable() {
        assert_eq!(slash_gate(&run("running", "idle", Some("w1:p1")), false), Ok("w1:p1".into()));
        // 「不知道」不是「在忙」：擋掉它只會讓按鈕在正常狀態下也按不動。
        assert_eq!(slash_gate(&run("running", "unknown", Some("w1:p1")), false), Ok("w1:p1".into()));
    }

    #[test]
    fn each_reason_is_reported_separately() {
        assert_eq!(slash_gate(&run("stopped", "idle", Some("w1:p1")), false), Err(SlashBlocked::NotRunning));
        assert_eq!(slash_gate(&run("exited", "idle", Some("w1:p1")), false), Err(SlashBlocked::NotRunning));
        assert_eq!(slash_gate(&run("running", "working", Some("w1:p1")), false), Err(SlashBlocked::AgentBusy));
        assert_eq!(slash_gate(&run("running", "blocked", Some("w1:p1")), false), Err(SlashBlocked::AgentBusy));
        assert_eq!(slash_gate(&run("running", "idle", Some("w1:p1")), true), Err(SlashBlocked::TurnInFlight));
        assert_eq!(slash_gate(&run("running", "idle", None), false), Err(SlashBlocked::NoPane));
        // 空字串的 pane id 和沒有 pane 是同一件事。
        assert_eq!(slash_gate(&run("running", "idle", Some("  ")), false), Err(SlashBlocked::NoPane));
    }

    /// 停掉的 run 就算同時在忙也先報「沒在跑」：那是使用者要先處理的那一件事。
    #[test]
    fn the_reasons_are_checked_in_the_order_the_user_would_fix_them() {
        assert_eq!(slash_gate(&run("stopped", "working", None), true), Err(SlashBlocked::NotRunning));
        assert_eq!(slash_gate(&run("running", "working", None), true), Err(SlashBlocked::AgentBusy));
        assert_eq!(slash_gate(&run("running", "idle", None), true), Err(SlashBlocked::TurnInFlight));
    }

    #[test]
    fn reasons_are_stable_wire_keys() {
        assert_eq!(SlashBlocked::NotRunning.reason(), "not_running");
        assert_eq!(SlashBlocked::AgentBusy.reason(), "agent_busy");
        assert_eq!(SlashBlocked::TurnInFlight.reason(), "turn_in_flight");
        assert_eq!(SlashBlocked::NoPane.reason(), "no_pane");
    }
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
async fn pane_ready_for_prompt(app: &Arc<App>, bot: &db::Bot, run: &db::Run, conv: &str) -> LcResult<()> {
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
    let res = client
        .call_timeout("agent.prompt", json!({"target": db::run_target(&run, &bot), "text": &deliver}), Duration::from_secs(10))
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
    if delivery == "ok" {
        arm_stall(app, &run.id, bot_id, &turn_id).await;
        arm_progress(app, &run.id, bot_id, &turn_id).await;
    }
    Ok(PromptOut { turn_id, message_id: msg_id, delivery: delivery.into() })
}


/// Unchanged polls at an empty composer before we complete the Turn ourselves (~14s; a slow hook still wins).
const IDLE_POLLS: u32 = 20;

/// 這回合畫面從沒出現過任何東西時的門檻（~63s）。2026-09-13（GROK、w168:pN）：grok 還沒印第一個字，
/// 空 `❯` 被當成等輸入，備援關掉回合，36 秒後 Stop hook 撞上已關的回合。等久只是備援晚接手；
/// 等不夠會吃掉使用者的問題。
const IDLE_POLLS_SILENT: u32 = 90;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(700);
const PROGRESS_MAX: Duration = Duration::from_secs(40 * 60);

/// Min gap between `turn_progress` frames per run (4/s, docs/API.md). The poll interval isn't a
/// rate limit: `arm_progress` re-arms per turn, so turn churn could burst.
const PROGRESS_MIN_GAP: Duration = Duration::from_millis(250);

const PROGRESS_STALE: Duration = Duration::from_secs(60);

/// Split out of `flush_progress` so the budget rule is testable without an `App`.
fn progress_due(last: Option<&std::time::Instant>, force: bool) -> bool {
    force || last.is_none_or(|t| t.elapsed() >= PROGRESS_MIN_GAP)
}

/// Ship the held frame if the 4/s budget allows. Frames merge (newest wins); `force` flushes the
/// final state once the poller is done.
async fn flush_progress(app: &Arc<App>, run_id: &str, pending: &mut Option<Value>, force: bool) {
    let Some(frame) = pending.take() else { return };
    let mut emitted = app.progress_emitted.lock().await;
    if !progress_due(emitted.get(run_id), force) {
        *pending = Some(frame);
        return;
    }
    emitted.insert(run_id.to_string(), std::time::Instant::now());
    // The map outlives its poller (a new turn must not get a fresh budget); drop stale entries.
    emitted.retain(|_, t| t.elapsed() < PROGRESS_STALE);
    drop(emitted);
    app.emit("turn_progress", frame).await;
}

/// While a turn is in flight, poll the pane and push partial replies as `turn_progress`. Also the
/// idle-prompt safety net that calls `try_fallback` (see below). Stops once the turn leaves `in_flight`.
pub async fn arm_progress(app: &Arc<App>, run_id: &str, bot_id: &str, turn_id: &str) {
    // A new turn is starting: whatever cut the *previous* one short is history (§4.3a).
    crate::turn_error::clear(app, run_id, bot_id).await;
    let mut pollers = app.progress_pollers.lock().await;
    if let Some(h) = pollers.remove(run_id) {
        h.abort();
    }
    let app2 = app.clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    let turn_id = turn_id.to_string();
    let key = run_id.clone();
    let h = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let Ok(Some(bot)) = db::bot(&app2.db, &bot_id).await else { return };
        // What we sent, to strip the pane's echo off every frame.
        let sent = turn_echo_texts(&app2, &turn_id).await;
        let mut last = (String::new(), String::new(), String::new());
        let mut quiet = 0u32;
        let mut pending: Option<Value> = None;
        loop {
            tokio::time::sleep(PROGRESS_INTERVAL).await;
            if started.elapsed() > PROGRESS_MAX {
                break;
            }
            let still = matches!(db::in_flight_turn(&app2.db, &run_id).await, Ok(Some(t)) if t.id == turn_id);
            if !still {
                break;
            }
            let Ok(Some(run)) = db::run(&app2.db, &run_id).await else { break };
            let Some(pane) = run.pane_id.clone() else { continue };
            let Ok(client) = client_for_run(&app2, &run).await else { continue };
            let Ok(read) = client.pane_read(&pane, "recent_unwrapped", 160).await else { continue };
            let live = live_reply(&bot.kind, &read.text).unwrap_or_default();
            let live = sent.iter().fold(live, |acc, p| strip_echoed_prompt(&acc, p));
            // `clean_screen` drops the spinner row, so a thinking / tool phase needs its own `activity` field.
            let activity = live_activity(&bot.kind, &read.text).unwrap_or_default();
            let alert = live_alert(&bot.kind, &read.text).unwrap_or_default();
            if live != last.0 || activity != last.1 || alert != last.2 {
                last = (live.clone(), activity.clone(), alert.clone());
                quiet = 0;
                pending = Some(
                    json!({"bot_id": bot_id, "run_id": run_id, "turn_id": turn_id, "text": live, "activity": activity, "alert": alert, "revision": read.revision}),
                );
                flush_progress(&app2, &run_id, &mut pending, false).await;
                continue;
            }
            flush_progress(&app2, &run_id, &mut pending, false).await;
            // §4.3's fallback is armed by herdr's `working -> idle`; when that sticks (2026-09-06: grok
            // idle at an empty composer, herdr still `working`) nothing closes the turn. So trust the
            // pane too: empty composer + no change = wants input. `blocked` excluded (a modal isn't an end).
            let agent_status = match db::run(&app2.db, &run_id).await {
                Ok(Some(r)) => r.agent_status,
                _ => String::new(),
            };
            if agent_status == "blocked" || !pane_awaits_input(&bot.kind, &read.text) {
                quiet = 0;
                continue;
            }
            quiet += 1;
            // 這回合畫面上出現過任何東西嗎（回覆、spinner、警示都算）。
            let said_something = !(last.0.is_empty() && last.1.is_empty() && last.2.is_empty());
            if quiet >= idle_threshold(said_something, &agent_status) {
                tracing::info!(turn = %turn_id, "pane idle at an empty prompt; completing via fallback");
                // Under the bot lock, like the Stop hook: outside it the two interleaved into two
                // assistant messages for one turn (review 2026-09-12 #5).
                let done = {
                    let lock = app2.bot_lock(&bot_id).await;
                    let _g = lock.lock().await;
                    try_fallback(&app2, &run_id).await
                };
                match done {
                    Ok(true) => break,
                    // Nothing claimed (spinner, tool, or hook won). Keep watching: this is the net for a
                    // status that never flips (grok 2026-09-06); the `still` check ends it once closed.
                    Ok(false) => quiet = 0,
                    Err(e) => {
                        tracing::debug!(turn = %turn_id, error = ?e, "idle-prompt fallback failed; keeping the poller");
                        quiet = 0;
                    }
                }
            }
        }
        flush_progress(&app2, &run_id, &mut pending, true).await;
        app2.progress_pollers.lock().await.remove(&run_id);
    });
    pollers.insert(key, h);
}


/// The user typed straight into the pane: open the `external` turn on the `-> working` edge so it
/// gets the same live bubble / progress as a web prompt. `delivery='ok'` so the §4.3 fallback
/// (which ignores other values) can still close it if the Stop hook never comes.
pub async fn begin_external_turn(app: &Arc<App>, run: &db::Run) {
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    // Re-read under the lock: `prompt()` may have opened a turn, and the watcher is armed before
    // the run is `running`, so a boot-time `-> working` blip is not the user typing.
    let Ok(Some(run)) = db::run(&app.db, &run.id).await else { return };
    if run.state != "running" {
        return;
    }
    if !matches!(db::in_flight_turn(&app.db, &run.id).await, Ok(None)) {
        return;
    }
    let conv = match db::conversation_id(&app.db, &run.bot_id).await {
        Ok(conv) => conv,
        Err(error) => {
            tracing::warn!(error = ?error, bot = %run.bot_id, "could not get conversation for external turn");
            return;
        }
    };
    let tid = db::ulid();
    if let Err(e) = sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
         VALUES (?,?,?,'external','in_flight','ok',?)",
    )
    .bind(&tid)
    .bind(&conv)
    .bind(&run.id)
    .bind(db::now())
    .execute(&app.db)
    .await
    {
        // Losing the `turns_one_in_flight` race means someone else opened it — fine.
        tracing::debug!(run = %run.id, error = %e, "external turn not opened");
        return;
    }
    tracing::info!(run = %run.id, turn = %tid, "external turn opened from pane activity");
    // No echo on screen: open the turn anyway rather than invent a user message.
    if let Some(text) = pane_prompt_echo(app, &run).await {
        if let Err(e) = insert_message(app, &conv, Some(&tid), "user", &text, "hook", false, None).await {
            tracing::warn!(turn = %tid, error = ?e, "external prompt echo not stored");
        }
    }
    emit_turn(app, &tid).await;
    arm_progress(app, &run.id, &run.bot_id, &tid).await;
}

async fn pane_prompt_echo(app: &Arc<App>, run: &db::Run) -> Option<String> {
    let bot = db::bot(&app.db, &run.bot_id).await.ok().flatten()?;
    let pane = run.pane_id.clone()?;
    let client = client_for_run(app, run).await.ok()?;
    let read = client.pane_read(&pane, "recent_unwrapped", 160).await.ok()?;
    last_prompt_echo_text(&bot.kind, &read.text)
}

/// Everything printed since the prompt echo, chrome and reply markers (`⏺ ` / `• `) removed.
fn live_reply(kind: &str, text: &str) -> Option<String> {
    let cleaned = clean_screen(kind, text)?;
    let out: Vec<String> = cleaned
        .lines()
        .filter(|l| !matches!(l.trim(), "⏺" | "•"))
        .map(|l| {
            let t = l.trim_start();
            t.strip_prefix("⏺ ").or_else(|| t.strip_prefix("• ")).unwrap_or(l).to_string()
        })
        .collect();
    let joined = out.join("\n").trim().to_string();
    if joined.is_empty() { None } else { Some(joined) }
}

const ACTIVITY_MAX: usize = crate::capture::ACTIVITY_MAX;

/// Empty composer = waiting for input? The bare marker row after stripping box frames (grok's
/// `│ ❯ │` never matches `clean_screen`'s test). Also true while claude works under a spinner, so
/// only meaningful with "nothing changed" (the progress poller's idle net → `try_fallback`).
fn pane_awaits_input(kind: &str, text: &str) -> bool {
    if kind == "claude" {
        return crate::capture::claude::PARSER.awaits_input(text);
    }
    let Some(marker) = prompt_echo_prefix(kind).and_then(|p| p.trim_end().chars().next()) else { return false };
    text.lines().rev().take(12).any(|l| {
        let mut chars = l.chars().filter(|c| !"│┃╭╮╰╯─━ \t".contains(*c));
        chars.next() == Some(marker) && chars.next().is_none()
    })
}

/// 空 composer 靜止幾輪才算等輸入。herdr 說閒著且畫面印過東西 → 14 秒（2026-09-06 grok 卡 working
/// 的安全網）；herdr 說 working 或這回合什麼都沒印過 → 63 秒（2026-09-13 GROK 還在想就被關）。
fn idle_threshold(said_something: bool, agent_status: &str) -> u32 {
    if said_something && agent_status != "working" {
        IDLE_POLLS
    } else {
        IDLE_POLLS_SILENT
    }
}

/// Every form of what we sent, for echo matching: image prompts were delivered with
/// `attach::deliver_text`'s appendix, which came back stored as an answer
/// (`01M1XSVME9SKEG1NZXG51HFP73`, 2026-09-07). Delivered form first so the longer text strips first.
async fn turn_echo_texts(app: &Arc<App>, turn_id: &str) -> Vec<String> {
    let rows = db::turn_user_messages_with_attachments(&app.db, turn_id).await.unwrap_or_default();
    let mut out = Vec::new();
    for (content, attachments) in rows {
        if let Some(json) = attachments.as_deref() {
            if let Ok(items) = serde_json::from_str::<Vec<crate::attach::Attachment>>(json) {
                let delivered = crate::attach::deliver_text(&content, &items);
                if delivered != content {
                    out.push(delivered);
                }
            }
        }
        out.push(content);
    }
    out
}

/// The reply in this snapshot with our prompt echo removed, or `None`. The three strippers only
/// mean "the agent said something" together; our own prompt alone is not an answer.
fn screen_reply(kind: &str, text: &str, sent: &[String]) -> Option<String> {
    let raw = extract_reply(kind, text).or_else(|| clean_screen(kind, text))?;
    let stripped = sent.iter().fold(raw, |acc, p| strip_echoed_prompt(&acc, p));
    let stripped = stripped.trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Trailing-ellipsis marker a TUI leaves where it clipped the echo of a long prompt.
const ELLIPSES: [&str; 2] = ["…", "..."];

fn without_ellipsis(s: &str) -> Option<&str> {
    ELLIPSES.iter().find_map(|e| s.strip_suffix(e)).map(str::trim_end)
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Is this line "the rest of the prompt, clipped"? grok squeezes lines 2..n onto one row ending in
/// `…`. Requiring a prefix of the remaining prompt (≥ [`SQUASH_MIN`] chars) keeps real replies ending in `…` safe.
fn is_clipped_echo(line: &str, rest: &[&str]) -> bool {
    let Some(body) = without_ellipsis(line.trim()) else { return false };
    let body = squash(body);
    if body.chars().count() < SQUASH_MIN {
        return false;
    }
    squash(&rest.join("")).starts_with(&body)
}

/// Drop lines 2..n of a multi-line prompt echo: `after_last_prompt_echo` only skips the `❯` row.
/// Matching against what we sent is exact, unlike guessing from indentation.
fn strip_echoed_prompt(text: &str, prompt: &str) -> String {
    let want: Vec<&str> = prompt.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if want.len() < 2 {
        // A one-line prompt can still be shredded across rows in a narrow pane.
        return strip_echoed_prompt_squashed(text, prompt).unwrap_or_else(|| text.to_string());
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    // The `❯ <first line>` marker row is already gone, so resume at the prompt's second line.
    let mut w = 1;
    while i < lines.len() && w < want.len() {
        let l = lines[i].trim();
        if l.is_empty() {
            i += 1;
            continue;
        }
        if l != want[w] {
            // The TUI may have clipped the whole remaining echo onto this row.
            if is_clipped_echo(l, &want[w..]) {
                i += 1;
                w = want.len();
            }
            break;
        }
        i += 1;
        w += 1;
    }
    // Only strip on a full match; eating half a real reply is worse than an echo.
    if w < want.len() {
        return strip_echoed_prompt_squashed(text, prompt).unwrap_or_else(|| text.to_string());
    }
    lines[i..].join("\n").trim().to_string()
}

/// Whitespace-insensitive fallback for [`strip_echoed_prompt`]: a very narrow pane lays the echo
/// out one char per line, so it was stored as a vertical-column reply. The candidate must open
/// with ≥ [`SQUASH_MIN`] chars of the prompt tail to keep real replies safe.
const SQUASH_MIN: usize = 8;

fn strip_echoed_prompt_squashed(text: &str, prompt: &str) -> Option<String> {
    let ps: Vec<char> = prompt.chars().filter(|c| !c.is_whitespace()).collect();
    let ts: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if ps.len() < SQUASH_MIN || ts.is_empty() {
        return None;
    }
    // Longest prompt tail the candidate opens with (the head went with the `❯ ` row).
    let mut matched = 0;
    for k in 0..ps.len() {
        let suf = &ps[k..];
        if suf.len() < SQUASH_MIN {
            break;
        }
        if ts.len() >= suf.len() {
            if ts[..suf.len()] == *suf {
                matched = suf.len();
                break;
            }
        } else if suf[..ts.len()] == *ts {
            // The candidate ran out inside the echo: all of it is echo.
            return Some(String::new());
        }
    }
    if matched == 0 {
        return None;
    }
    let mut n = 0;
    let mut cut = text.len();
    for (i, c) in text.char_indices() {
        if n == matched {
            cut = i;
            break;
        }
        if !c.is_whitespace() {
            n += 1;
        }
    }
    if n < matched {
        return Some(String::new());
    }
    Some(text[cut..].trim().to_string())
}

/// Pane width for the "too narrow" message; best effort, failure only means less detail.
async fn pane_columns(app: &Arc<App>, run: &db::Run) -> Option<u32> {
    let pane = run.pane_id.clone()?;
    let ws = run.workspace_id.clone()?;
    let client = client_for_run(app, run).await.ok()?;
    let rects = client.pane_rects(&ws).await.ok()?;
    rects.into_iter().find(|(id, _, _)| *id == pane).map(|(_, w, _)| w)
}

/// Output shredded into single characters? herdr unwraps terminal wrapping, not the TUI's own
/// one-glyph-per-row layout, whose spaces are lost — say so rather than store a column.
fn is_shredded(text: &str) -> bool {
    crate::capture::is_shredded(text)
}

/// Spinner-row shape `<Verb>… (3m 18s · ↓ 11.0k tokens)`. Verb is random and glyphs change
/// across releases; only the bracketed time / token counter is stable.
pub(crate) fn is_activity_shape(s: &str) -> bool {
    crate::capture::is_activity_shape(s)
}

/// The spinner row of this turn, glyph stripped, for `turn_progress.activity` only — never stored,
/// so `clean_screen` / `extract_reply` can keep dropping it as chrome.
fn live_activity(kind: &str, text: &str) -> Option<String> {
    if kind == "claude" {
        return crate::capture::claude::PARSER.activity(text);
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = after_last_prompt_echo(kind, &lines);
    let grok = kind == "grok";
    // Fast path only; the glyph set grows between releases, `is_activity_shape` catches the rest.
    let glyphs: &[char] = if grok { &['◆'] } else { &['✻', '✽', '✶', '✳', '✢', '·'] };
    let mut found: Option<String> = None;
    for line in &lines[start..] {
        let stripped;
        let s = if grok {
            stripped = strip_grok_decor(line);
            stripped.trim()
        } else {
            line.trim()
        };
        let Some(first) = s.chars().next() else { continue };
        let rest = if glyphs.contains(&first) {
            s[first.len_utf8()..].trim()
        } else if is_activity_shape(s) {
            // Unknown glyph (or none at all): drop a leading symbol if there is one.
            if first.is_alphanumeric() { s } else { s[first.len_utf8()..].trim() }
        } else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        // The last activity row on screen is the current one.
        found = Some(rest.to_string());
    }
    let s = found?;
    if s.chars().count() <= ACTIVITY_MAX {
        return Some(s);
    }
    let mut cut: String = s.chars().take(ACTIVITY_MAX).collect::<String>().trim_end().to_string();
    cut.push('…');
    Some(cut)
}

/// Retry / API-error banner (`API error · Retrying in 3s · attempt 1/10`, codex `stream error: …;
/// retrying 2/5`) as `turn_progress.alert`, since the spinner makes a stuck turn look healthy.
/// Shape, not wording: short + *error* + retry/attempt token, or opens with `API error`.
fn live_alert(kind: &str, text: &str) -> Option<String> {
    const RETRY_TOKENS: [&str; 6] = ["retry", "retrying", "attempt", "reconnect", "重試", "retries"];
    // Codex hard limit may wrap across narrow-pane rows: multi-line scanner.
    if kind == "codex" {
        if let Some(hit) = codex_usage_notice_lines(text)
            .into_iter()
            .find(|n| codex_limit_hit_line(n).is_some())
        {
            if hit.chars().count() <= ACTIVITY_MAX {
                return Some(hit);
            }
            let mut cut: String = hit.chars().take(ACTIVITY_MAX).collect::<String>().trim_end().to_string();
            cut.push('…');
            return Some(cut);
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = after_last_prompt_echo(kind, &lines);
    let mut found: Option<String> = None;
    for line in &lines[start..] {
        let stripped;
        let s = if kind == "grok" {
            stripped = strip_grok_decor(line);
            stripped.trim()
        } else {
            line.trim()
        };
        // Drop a leading spinner / bullet glyph so `✻ API error …` matches too.
        let s = match s.chars().next() {
            Some(c) if !c.is_alphanumeric() => s[c.len_utf8()..].trim(),
            _ => s,
        };
        if s.is_empty() || s.chars().count() > ACTIVITY_MAX * 2 {
            continue;
        }
        let low = s.to_ascii_lowercase();
        let says_error = low.contains("error") || low.contains("錯誤") || low.contains("overloaded");
        if !says_error {
            continue;
        }
        // Data, not a banner (2026-09-08): JSON or `|` rows from e.g. a sqlite dump.
        if s.contains('{') || s.contains('}') || s.matches('|').count() >= 2 {
            continue;
        }
        let retrying = RETRY_TOKENS.iter().any(|t| low.contains(t));
        if !retrying && !low.starts_with("api error") {
            continue;
        }
        // The newest banner is the one still true.
        found = Some(s.to_string());
    }
    let s = found?;
    if s.chars().count() <= ACTIVITY_MAX {
        return Some(s);
    }
    let mut cut: String = s.chars().take(ACTIVITY_MAX).collect::<String>().trim_end().to_string();
    cut.push('…');
    Some(cut)
}


const STALL_SECS: u64 = 12;

/// Rows above the bottom the composer can start (it grows with its text; claude adds a hint row).
const COMPOSER_TAIL: usize = 24;

/// Chars of our prompt head that must be visible in the composer. `contains`, not prefix: claude
/// puts `[Image #6]` in front of pasted text.
const COMPOSER_HEAD: usize = 12;

/// Never match on a fragment this short — a two-character prompt is in every screen.
const COMPOSER_HEAD_MIN: usize = 4;

fn undecorate_row(line: &str) -> String {
    let s = strip_grok_decor(line);
    s.trim().trim_start_matches('│').trim_end_matches('│').trim().to_string()
}

fn is_rule_row(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| "─━-=_╭╮╰╯".contains(c))
}

/// Text in the input box, or `None` when empty / no known marker. Searched from the bottom; an
/// empty box is `pane_awaits_input`'s job, else we'd walk back to an accepted prompt's echo.
fn composer_text(kind: &str, screen: &str) -> Option<String> {
    let marker = prompt_echo_prefix(kind)?.trim_end();
    if pane_awaits_input(kind, screen) {
        return None;
    }
    let lines: Vec<&str> = screen.lines().collect();
    let from = lines.len().saturating_sub(COMPOSER_TAIL);
    let tail = &lines[from..];
    let idx = tail.iter().rposition(|l| {
        let t = undecorate_row(l);
        t.starts_with(marker) && !t[marker.len()..].trim().is_empty()
    })?;
    let mut out: Vec<String> = Vec::new();
    for (n, line) in tail[idx..].iter().enumerate() {
        let row = undecorate_row(line);
        let body = if n == 0 { row[marker.len()..].trim().to_string() } else { row };
        if n > 0 && (body.is_empty() || is_rule_row(&body)) {
            break;
        }
        out.push(body);
    }
    let joined = out.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Is our prompt still unsent in the box? 2026-09-07 11:21: claude swallowed the Enter while
/// compacting; the turn failed as a stall with the message one keystroke from sent.
fn composer_holds_prompt(kind: &str, screen: &str, sent: &str) -> bool {
    let Some(box_text) = composer_text(kind, screen) else { return false };
    let needle: String = squash(sent).chars().take(COMPOSER_HEAD).collect();
    if needle.chars().count() < COMPOSER_HEAD_MIN {
        return false;
    }
    squash(&box_text).contains(&needle)
}

/// Early check after delivery: enough for the box to draw, short of the full stall deadline.
const NUDGE_EARLY_SECS: u64 = 3;

/// After re-sending Enter, how long the agent gets to react before the turn is failed.
const NUDGE_GRACE_SECS: u64 = 8;

/// Press Enter if our prompt is still in the box and the agent idle. Best effort: every reason to
/// do nothing is `false`, never an error.
async fn nudge_unsent_prompt(app: &Arc<App>, run_id: &str, turn_id: &str, sent: &[String]) -> bool {
    let Ok(Some(run)) = db::run(&app.db, run_id).await else { return false };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return false;
    }
    if !matches!(db::in_flight_turn(&app.db, run_id).await, Ok(Some(t)) if t.id == turn_id && t.delivery == "ok") {
        return false;
    }
    let Ok(Some(bot)) = db::bot(&app.db, &run.bot_id).await else { return false };
    let Some(pane) = run.pane_id.clone() else { return false };
    let Ok(client) = client_for_run(app, &run).await else { return false };
    let Ok(read) = client.pane_read(&pane, "visible", 80).await else { return false };
    if !sent.iter().any(|p| composer_holds_prompt(&bot.kind, &read.text, p)) {
        return false;
    }
    if let Err(e) = client.pane_send_keys(&pane, &["Enter"]).await {
        tracing::warn!(error = ?e, run_id, "could not re-send Enter for an unsent prompt");
        return false;
    }
    tracing::warn!(run_id, turn = %turn_id, "prompt was still in the composer; re-sent Enter");
    true
}

/// The agent must leave `idle` within `STALL_SECS` after delivery, or the Turn sits `in_flight`
/// forever (not logged in, invisible modal). Cancelled by the first `working` / `blocked` event.
pub async fn arm_stall(app: &Arc<App>, run_id: &str, bot_id: &str, turn_id: &str) {
    let mut timers = app.stall_timers.lock().await;
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    timers.insert(run_id.to_string(), generation);
    let app2 = app.clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    let turn_id = turn_id.to_string();
    tokio::spawn(async move {
        let sent = turn_echo_texts(&app2, &turn_id).await;
        let mut nudged = false;
    // Every prompt gets the early look: a single line pasted into a busy TUI loses its Enter too.
        tokio::time::sleep(Duration::from_secs(NUDGE_EARLY_SECS)).await;
        {
            let lock = app2.bot_lock(&bot_id).await;
            let _g = lock.lock().await;
            if app2.stall_timers.lock().await.get(&run_id) != Some(&generation) {
                return;
            }
            nudged = nudge_unsent_prompt(&app2, &run_id, &turn_id, &sent).await;
        }
        tokio::time::sleep(Duration::from_secs(STALL_SECS - NUDGE_EARLY_SECS)).await;
    // Deadline: if the text is still there, press Enter and grant a grace period first.
        {
            let lock = app2.bot_lock(&bot_id).await;
            let _g = lock.lock().await;
            if app2.stall_timers.lock().await.get(&run_id) != Some(&generation) {
                return;
            }
            nudged |= nudge_unsent_prompt(&app2, &run_id, &turn_id, &sent).await;
        }
        if nudged {
            tokio::time::sleep(Duration::from_secs(NUDGE_GRACE_SECS)).await;
        }
        let lock = app2.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if app2.stall_timers.lock().await.get(&run_id) != Some(&generation) {
            return;
        }
        if let Err(e) = fail_stalled_turn(&app2, &run_id, &bot_id, &turn_id, nudged).await {
            tracing::warn!(error = ?e, "stall watchdog failed");
        }
        let mut timers = app2.stall_timers.lock().await;
        if timers.get(&run_id) == Some(&generation) {
            timers.remove(&run_id);
        }
    });
}

pub async fn cancel_stall(app: &Arc<App>, run_id: &str) {
    app.stall_timers.lock().await.remove(run_id);
}

async fn fail_stalled_turn(
    app: &Arc<App>,
    run_id: &str,
    bot_id: &str,
    turn_id: &str,
    nudged: bool,
) -> anyhow::Result<()> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(()) };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Ok(());
    }
    let Some(turn) = db::in_flight_turn(&app.db, run_id).await? else { return Ok(()) };
    if turn.id != turn_id || turn.delivery != "ok" {
        return Ok(());
    }
    // Quote the screen, never assert a cause (the host may well be logged in).
    let mut snapshot: Option<String> = None;
    let mut hints: Vec<String> = Vec::new();
    if let Ok(client) = client_for_run(app, &run).await {
        if let Some(pane) = run.pane_id.as_deref() {
            if let Ok(read) = client.pane_read(pane, "visible", 60).await {
                hints = stall_hint_lines(&read.text);
                snapshot = Some(read.text);
            }
        }
    }
    let reason = stall_reason(&hints, nudged);
    let mut tx = app.db.begin().await?;
    let res = sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=? AND status='in_flight'")
        .bind(db::now())
        .bind(turn_id)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(());
    }
    let message = insert_message_tx(
        &mut tx,
        &turn.conversation_id,
        Some(turn_id),
        "system",
        &reason,
        "system",
        false,
        snapshot.as_deref(),
    )
    .await?;
    tx.commit().await?;
    tracing::warn!(turn = %turn_id, bot = %bot_id, %reason, "prompt stalled; turn failed");
    emit_message_added(app, bot_id, message).await;
    emit_turn(app, turn_id).await;
    Ok(())
}

/// Screen lines worth quoting back to the user when a prompt stalls.
fn stall_hint_lines(screen: &str) -> Vec<String> {
    const NEEDLES: [&str; 5] = ["not logged in", "/login", "unlock-keychain", "usage limit", "limit"];
    let mut out: Vec<String> = Vec::new();
    for line in screen.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let low = t.to_lowercase();
        if NEEDLES.iter().any(|n| low.contains(n)) && !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Neutral wording: symptom + quoted screen + keychain caveat; never claim "not logged in".
fn stall_reason(hints: &[String], nudged: bool) -> String {
    let head = format!("agent 在 {STALL_SECS} 秒內沒有對訊息作出反應。");
    // `nudged`: tell the user we already pressed Enter for text left in the box.
    let head = if nudged {
        format!("{head}訊息還留在輸入框沒送出（TUI 忙碌時會把多行文字當成貼上，吞掉最後的 Enter），已嘗試補送一次 Enter，等 {NUDGE_GRACE_SECS} 秒仍沒有反應。")
    } else {
        head
    };
    if hints.is_empty() {
        return format!("{head}請查看終端分頁。");
    }
    format!(
        "{head}終端畫面：\n{}\n若該身份使用 macOS Keychain 儲存憑證，透過 ssh 啟動的 herdr 可能讀不到（畫面提示 `security unlock-keychain`）。",
        hints.join("\n")
    )
}

pub async fn abandon_turn(app: &Arc<App>, turn_id: &str) -> LcResult<()> {
    let conversation_id = sqlx::query_scalar::<_, String>("SELECT conversation_id FROM turns WHERE id=?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id=?")
        .bind(&conversation_id)
        .fetch_one(&app.db)
        .await
        .map_err(up)?;
    let lock = app.bot_lock(&bot_id).await;
    let _g = lock.lock().await;
    let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .map_err(up)?
        .ok_or_else(|| LcError::NotFound("turn".into()))?;
    if t.status != "in_flight" {
        return Err(LcError::conflict("turn is neither in-flight nor of unknown delivery", json!({"turn_id": t.id})));
    }
    let res = sqlx::query(
        "UPDATE turns SET status='failed', delivery = CASE WHEN delivery='unknown' THEN 'failed' ELSE delivery END, completed_at=?
         WHERE id=? AND status='in_flight'",
    )
        .bind(db::now())
        .bind(turn_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    if res.rows_affected() == 0 {
        return Err(LcError::conflict("turn is neither in-flight nor of unknown delivery", json!({"turn_id": t.id})));
    }
    let _ = insert_message(app, &t.conversation_id, Some(turn_id), "system", "turn abandoned by user", "system", false, None).await;
    emit_turn(app, turn_id).await;
    Ok(())
}


/// Arm the 5s terminal-fallback timer after a working -> idle transition.
pub async fn arm_fallback(app: &Arc<App>, run_id: &str, bot_id: &str) {
    let mut timers = app.fallback_timers.lock().await;
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    timers.insert(run_id.to_string(), generation);
    let app2 = app.clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let lock = app2.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if app2.fallback_timers.lock().await.get(&run_id) != Some(&generation) {
            return;
        }
        match try_fallback(&app2, &run_id).await {
            // No turn, but for a hookless run the pane is the only record of the answer.
            Ok(false) => {
                if let Err(e) = capture_hookless_turn_locked(&app2, &run_id, false).await {
                    tracing::warn!(error = ?e, "hookless terminal capture failed");
                }
            }
            Ok(true) => {}
            Err(e) => tracing::warn!(error = ?e, "terminal fallback failed"),
        }
        // A Codex usage hint may render after notify already completed the turn: capture regardless.
        if let Err(e) = capture_codex_usage_notices(&app2, &bot_id, &run_id).await {
            tracing::debug!(error = ?e, "codex notice capture failed");
        }
        // §4.3a: also classify ended vs cut off by the API, even when the Stop hook already completed it.
        if let Err(e) = crate::turn_error::capture(&app2, &bot_id, &run_id).await {
            tracing::debug!(error = ?e, "turn error capture failed");
        }
        let mut timers = app2.fallback_timers.lock().await;
        if timers.get(&run_id) == Some(&generation) {
            timers.remove(&run_id);
        }
    });
}

/// Close the in-flight turn from the pane (§4.3). `Ok(false)`: nothing to fall back on (no turn,
/// or untrusted delivery). A turn closed with zero reply can still be filled by hookrecv's late hook.
async fn try_fallback(app: &Arc<App>, run_id: &str) -> anyhow::Result<bool> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(false) };
    let Some(turn) = db::in_flight_turn(&app.db, run_id).await? else { return Ok(false) };
    if turn.delivery != "ok" {
        return Ok(false);
    }
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(false) };

    // Read the pane before claiming: a failure after claiming left `completed_fallback` with no
    // message, no `turn_updated`, no queue flush. Failing here keeps it in flight for the next edge.
    let pane_id = run.pane_id.clone().unwrap_or_default();
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| anyhow::anyhow!("no Herdr session is available for run `{}`", run.id))?;
    let read = client.pane_read(&pane_id, "recent_unwrapped", 200).await?;

    // A spinner still turning means mid-thought whatever herdr says: claiming stored chrome as the
    // answer (2026-09-07 11:17). Left in flight, the next edge or the idle poller re-arms this.
    if pane_still_busy(&read.text) {
        tracing::debug!(run_id, "pane still shows a spinner; not completing the turn from it");
        return Ok(false);
    }
    let fresh_probe = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());
    let probe = extract_reply(&bot.kind, &fresh_probe).or_else(|| clean_screen(&bot.kind, &fresh_probe)).unwrap_or_default();
    if is_tool_progress(&probe) {
        tracing::debug!(run_id, "pane is still mid-tool-call; not completing the turn from it");
        return Ok(false);
    }

    let fresh = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());

    // Codex hard limit: system notice + failed turn, not a fake reply. Banner may sit above the cursor.
    let codex_hit = if bot.kind == "codex" {
        codex_usage_notice_lines(&fresh)
            .into_iter()
            .chain(codex_usage_notice_lines(&read.text))
            .find(|n| codex_limit_hit_line(n).is_some())
    } else {
        None
    };

    // Derive the reply before SQLite's write lock (`pane_columns` RPC, `turn_echo_texts` other conn).
    let reply = if codex_hit.is_some() {
        None
    } else {
    // Only the `❯` row counts as echo; lines 2..n would be stored as the answer.
        let sent = turn_echo_texts(app, &turn.id).await;
        match screen_reply(&bot.kind, &fresh, &sent) {
            None => None,
            Some(reply) => Some(if is_shredded(&reply) {
                // Shredded = too narrow to read; name the pane and width so it's actionable.
                let how_wide = match pane_columns(app, &run).await {
                    Some(w) => format!("目前 {w} 欄，"),
                    None => String::new(),
                };
                format!(
                    "（pane {pane_id} 太窄，{how_wide}輸出在終端就被切成單字元，無法還原。\
                     把它拉寬一點就會恢復；這只影響終端備援，hook 取得的回覆不受影響。）"
                )
            } else {
                reply
            }),
        }
    };

    // CAS claim + reply in one transaction, so a completed turn never lacks its reply.
    let mut tx = app.db.begin().await?;
    let res = sqlx::query("UPDATE turns SET status='completed_fallback', completed_at=? WHERE id=? AND status='in_flight'")
        .bind(db::now())
        .bind(&turn.id)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(false);
    }
    tracing::info!(turn = %turn.id, "terminal fallback engaged");

    if let Some(hit) = codex_hit {
        sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&turn.id)
            .execute(&mut *tx)
            .await?;
        let message = insert_message_tx(
            &mut tx,
            &turn.conversation_id,
            Some(&turn.id),
            "system",
            &hit,
            "system",
            false,
            Some(&read.text),
        )
        .await?;
        remember_pane_cursor_tx(&mut tx, run_id, &read).await?;
        tx.commit().await?;
        emit_message_added(app, &bot.id, message).await;
        let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
        apply_codex_limit_hit_quota(app, &host, bot.identity.as_deref(), &hit).await;
        emit_turn(app, &turn.id).await;
        return Ok(true);
    }

    let Some(reply) = reply else {
        // Only our own prompt: the turn is closed (composer unlocks); storing the echo would put words in the agent's mouth.
        tracing::info!(turn = %turn.id, "terminal fallback saw only our own prompt; storing no reply");
        tx.commit().await?;
        remember_pane_cursor(app, run_id, &read).await?;
        emit_turn(app, &turn.id).await;
        return Ok(true);
    };

    let message = insert_message_tx(
        &mut tx,
        &turn.conversation_id,
        Some(&turn.id),
        "assistant",
        &reply,
        "terminal_fallback",
        true,
        Some(&read.text),
    )
    .await?;
    remember_pane_cursor_tx(&mut tx, run_id, &read).await?;
    tx.commit().await?;
    emit_message_added(app, &bot.id, message).await;
    emit_turn(app, &turn.id).await;
    Ok(true)
}

/// Spinner glyphs before an in-progress verb (`✢ Baking…`, `⠦ Thinking… 52s`); braille is codex/grok.
fn is_spinner_glyph(c: char) -> bool {
    crate::capture::claude::is_spinner_glyph(c)
}

/// Mid-turn: a spinner still on screen. codex's `• Working (4s • esc to interrupt)` has no ellipsis,
/// so it's matched separately (2026-09-08).
fn pane_still_busy(screen: &str) -> bool {
    crate::capture::claude::PARSER.still_busy(screen) || screen.lines().any(is_codex_working_line)
}

fn is_codex_working_line(s: &str) -> bool {
    let s = s.trim();
    s.starts_with('•') && s.contains("Working (") && s.contains("esc to interrupt")
}

fn is_tool_progress(reply: &str) -> bool {
    crate::capture::claude::is_tool_progress(reply)
}


/// Adopted run without hooks: no `user_prompt` / `stop` payload will ever come, so the terminal
/// snapshot is the only source (e.g. `managed_by='child'` bots whose parent started the pane).
fn is_hookless(bot: &db::Bot, run: &db::Run) -> bool {
    run.adopted != 0 && bot.inject_hooks == 0
}

/// Cap on a hookless scraped reply: near 200 lines of scrollback it's a screen, not a message.
const HOOKLESS_REPLY_MAX: usize = 6000;

/// Delay before reading an adopted pane: its banner and `agent_status` are still settling.
const ADOPTED_CAPTURE_DELAY: Duration = Duration::from_secs(2);

/// Store a finished hookless exchange as its own `external` turn. Adopted panes are often picked
/// up mid-answer, so `working -> idle` arrives with no turn in flight and the reply was dropped.
/// `seed` (adoption-time capture) only writes into an empty conversation, since re-adoption
/// happens on every restart / reconnect. Caller holds the bot lock; returns whether stored.
async fn capture_hookless_turn_locked(app: &Arc<App>, run_id: &str, seed: bool) -> anyhow::Result<bool> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(false) };
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(false) };
    if !is_hookless(&bot, &run) || run.state != "running" {
        return Ok(false);
    }
    // Like §4.3: a modal waiting for an answer is not an ended turn.
    if run.agent_status == "blocked" {
        return Ok(false);
    }
    // An in-flight turn belongs to `try_fallback`; capturing too would store it twice.
    if db::in_flight_turn(&app.db, run_id).await?.is_some() {
        return Ok(false);
    }
    let Some(pane_id) = run.pane_id.clone() else { return Ok(false) };
    let conv = db::conversation_id(&app.db, &run.bot_id).await?;
    if seed && conversation_message_count(app, &conv).await? > 0 {
        return Ok(false);
    }
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| anyhow::anyhow!("no Herdr session is available for run `{}`", run.id))?;
    let read = client.pane_read(&pane_id, "recent_unwrapped", 200).await?;
    let fresh = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());
    // The pane echo is the only record of what was typed.
    let echo = last_prompt_echo_text(&bot.kind, &fresh);
    // Without a cursor, only the echo marks where the last turn began; with neither, just
    // remember the cursor — storing older turns as one message is worse than nothing.
    if run.last_read_tail_hash.is_none() && echo.is_none() {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    let scraped = extract_reply(&bot.kind, &fresh).or_else(|| clean_screen(&bot.kind, &fresh));
    let reply = match (&echo, scraped) {
        (Some(p), Some(r)) => strip_echoed_prompt(&r, p),
        (None, Some(r)) => r,
        (_, None) => String::new(),
    };
    // Unlike `try_fallback`, no turn waits to be closed, so no 「（終端沒有可辨識的回覆）」 bubble.
    if reply.trim().is_empty() || is_shredded(&reply) {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    // herdr reports `working -> idle` more than once per answer: an identical reply is the same reply.
    if last_assistant_content(app, &conv).await.as_deref().map(str::trim) == Some(reply.trim()) {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    let reply = truncate_hookless_reply(reply);

    let tid = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, completed_at)
         VALUES (?,?,?,'external','completed_fallback','ok',?,?)",
    )
    .bind(&tid)
    .bind(&conv)
    .bind(run_id)
    .bind(&now)
    .bind(&now)
    .execute(&app.db)
    .await?;
    if let Some(text) = echo.as_deref() {
        insert_message(app, &conv, Some(&tid), "user", text, "terminal_fallback", false, None).await?;
        // ULIDs only order by millisecond; wait so the reply doesn't sort above its prompt.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    insert_message(app, &conv, Some(&tid), "assistant", &reply, "terminal_fallback", true, Some(&read.text)).await?;
    remember_pane_cursor(app, run_id, &read).await?;
    emit_turn(app, &tid).await;
    tracing::info!(run = %run_id, turn = %tid, bot = %bot.name, seed, "hookless turn captured from the pane");
    Ok(true)
}

/// [`capture_hookless_turn_locked`] for a caller that does not already hold the bot lock.
async fn capture_hookless_turn(app: &Arc<App>, run_id: &str, bot_id: &str, seed: bool) -> anyhow::Result<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    capture_hookless_turn_locked(app, run_id, seed).await
}

/// Adopted hookless run (called by `reconcile`, off its task since both take the bot lock):
/// still `working` → open the `external` turn now; already `idle` → store the exchange once (`seed`).
pub fn spawn_adopted_capture(app: &Arc<App>, run_id: &str, bot_id: &str) {
    let (app, run_id, bot_id) = (app.clone(), run_id.to_string(), bot_id.to_string());
    tokio::spawn(async move {
        tokio::time::sleep(ADOPTED_CAPTURE_DELAY).await;
        let (Ok(Some(run)), Ok(Some(bot))) = (db::run(&app.db, &run_id).await, db::bot(&app.db, &bot_id).await) else {
            return;
        };
        if !is_hookless(&bot, &run) {
            return;
        }
        if run.agent_status == "working" {
            begin_external_turn(&app, &run).await;
            return;
        }
        // `blocked` / `unknown` are refused inside the capture: neither is a finished turn.
        if let Err(e) = capture_hookless_turn(&app, &run_id, &bot_id, true).await {
            tracing::debug!(run = %run_id, error = ?e, "adopted pane capture failed");
        }
    });
}

fn truncate_hookless_reply(reply: String) -> String {
    if reply.chars().count() <= HOOKLESS_REPLY_MAX {
        return reply;
    }
    let mut cut: String = reply.chars().take(HOOKLESS_REPLY_MAX).collect::<String>().trim_end().to_string();
    cut.push_str("\n（終端擷取到此截斷）");
    cut
}

async fn conversation_message_count(app: &Arc<App>, conversation_id: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_one(&app.db)
        .await?)
}

/// Newest assistant message: duplicate guard for re-reading an unchanged screen.
async fn last_assistant_content(app: &Arc<App>, conversation_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT content FROM messages WHERE conversation_id = ? AND role = 'assistant'
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(&app.db)
    .await
    .ok()
    .flatten()
}

/// Record how far into the pane we have read, so the next capture starts after it.
async fn remember_pane_cursor(app: &Arc<App>, run_id: &str, read: &crate::herdr::PaneRead) -> anyhow::Result<()> {
    sqlx::query("UPDATE runs SET last_read_revision=?, last_read_tail_hash=? WHERE id=?")
        .bind(read.revision as i64)
        .bind(tail_hash(&read.text))
        .bind(run_id)
        .execute(&app.db)
        .await?;
    Ok(())
}

async fn remember_pane_cursor_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run_id: &str,
    read: &crate::herdr::PaneRead,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE runs SET last_read_revision=?, last_read_tail_hash=? WHERE id=?")
        .bind(read.revision as i64)
        .bind(tail_hash(&read.text))
        .bind(run_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn tail_hash(text: &str) -> String {
    let tail: String = text.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{:x}", md5ish(&tail))
}

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

/// Prompt-echo prefix per CLI; single source for `after_last_prompt_echo` / `last_prompt_echo_text`.
fn prompt_echo_prefix(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" | "grok" => Some("❯ "),
        "codex" => Some("› "),
        _ => None,
    }
}

/// Index after the last prompt echo (`❯ …` / `› …`), or 0 when not on screen.
fn after_last_prompt_echo(kind: &str, lines: &[&str]) -> usize {
    let Some(echo) = prompt_echo_prefix(kind) else { return 0 };
    lines
        .iter()
        .rposition(|l| {
            let t = l.trim_start();
            t.starts_with(echo) && t.len() > echo.len() && !is_codex_idle_prompt(t)
        })
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// What the user typed on the last prompt echo, prefix stripped — the only source of the user
/// message for a turn typed straight into the pane (no hook payload until it ends).
pub fn last_prompt_echo_text(kind: &str, text: &str) -> Option<String> {
    let echo = prompt_echo_prefix(kind)?;
    let lines: Vec<&str> = text.lines().collect();
    let idx = after_last_prompt_echo(kind, &lines);
    if idx == 0 {
        return None;
    }
    let raw = lines[idx - 1];
    // grok right-aligns a clock and a scrollbar glyph onto the prompt row (see `clean_screen`).
    let stripped;
    let line = if kind == "grok" {
        stripped = strip_grok_decor(raw);
        stripped.as_str()
    } else {
        raw
    };
    let body = line.trim_start().strip_prefix(echo)?.trim();
    if body.is_empty() {
        return None;
    }
    // 折行的續行也是同一句話（2026-09-12 使用者回報）：只讀 `❯` 那行的話訊息被截斷，
    // 剩下半句被 `extract_reply` 當成 agent 的回覆。
    let mut out = vec![body.to_string()];
    // 只有排到行尾的回音才可能有續行；否則 grok 回覆、codex `thinking…` 這類縮排行會被誤收成使用者的話。
    if echo_row_is_full(raw) {
        for l in lines.iter().skip(idx) {
            match echo_continuation(kind, l) {
                Some(rest) => out.push(rest.to_string()),
                None => break,
            }
        }
    }
    Some(out.join("\n"))
}

/// 這行有沒有排到行尾（下一行可能是折下來的）。快照不知 pane 寬度，用顯示寬度（CJK 算兩欄）估，
/// 門檻 60 欄：實機折行都在 100 欄以上。
fn echo_row_is_full(raw: &str) -> bool {
    const WRAP_MIN_COLS: usize = 60;
    raw.trim_end().chars().map(|c| if (c as u32) > 0x1100 { 2 } else { 1 }).sum::<usize>() >= WRAP_MIN_COLS
}

/// 上一行回音的續行？續行＝有縮排且不是別的東西（`⎿`、`⏺`、`●`、`✻`、框線要先排除）。
/// 寧可少收（留一句回音）也不多收（吃掉 agent 的回覆）。
fn echo_continuation<'a>(kind: &str, line: &'a str) -> Option<&'a str> {
    // 續行一定有縮排；沒縮排的是下一塊內容。
    let rest = line.strip_prefix("  ")?;
    let t = rest.trim();
    if t.is_empty() || is_noise(line) {
        return None;
    }
    // 這些開頭代表另一塊東西開始了。
    const MARKERS: [&str; 10] = ["⎿", "⏺", "●", "✻", "✳", "│", "└", "├", "╭", "╰"];
    if MARKERS.iter().any(|m| t.starts_with(m)) {
        return None;
    }
    // 下一個回音行（使用者連送兩句）也不是續行。
    if let Some(echo) = prompt_echo_prefix(kind) {
        if t.starts_with(echo) {
            return None;
        }
    }
    Some(t)
}

/// Is this line TUI chrome (banner, boxes, rules, status bar, spinner) rather than content?
pub(crate) fn is_noise(s: &str) -> bool {
    crate::capture::claude::PARSER.noise_line(s)
}

/// Codex empty-composer placeholder `› Ask Codex to do anything` — looks like an echo, isn't one.
fn is_codex_idle_prompt(s: &str) -> bool {
    let t = s.trim_start();
    let body = t.strip_prefix("› ").unwrap_or(t).trim();
    body.to_ascii_lowercase().starts_with("ask codex to do")
}

/// grok 1.0.13 TUI chrome (appendix F): `◆` rows, "Worked for" footer, telemetry banner, shortcut
/// footer, `<cwd>   15K / 500K` header, `[stable]`.
fn is_grok_noise(s: &str) -> bool {
    if s.starts_with('◆') || s.starts_with("Worked for ") || s.contains("[hooks:") {
        return true;
    }
    if s.starts_with("Help improve Grok")
        || s.starts_with("Off by default.")
        || s == "settings."
        || s.starts_with("Read Terms and Privacy")
        || s == "[stable]"
        || s.starts_with("Grok Build ")
    {
        return true;
    }
    if s.contains("Ctrl+.:shortcuts") || s.contains("Shift+Tab:mode") || s.contains("Esc:cancel") {
        return true;
    }
    // "<cwd>                       15K / 500K"
    if let Some((_, tail)) = s.rsplit_once("  ") {
        let t = tail.trim();
        if t.ends_with('K') && t.contains(" / ") && t.chars().all(|c| c.is_ascii_digit() || c == 'K' || c == ' ' || c == '/' || c == '.') {
            return true;
        }
    }
    false
}

/// Strip grok's right-edge scrollbar `█` and right-aligned `h:mm AM|PM` clock.
fn strip_grok_decor(line: &str) -> String {
    let mut s = line.trim_end().trim_end_matches('█').trim_end().to_string();
    if let Some(rest) = s.strip_suffix(" AM").or_else(|| s.strip_suffix(" PM")) {
        if let Some((head, clock)) = rest.rsplit_once(' ') {
            let ok = clock.len() >= 4
                && clock.len() <= 5
                && clock.chars().filter(|c| *c == ':').count() == 1
                && clock.chars().all(|c| c.is_ascii_digit() || c == ':');
            // Two or more spaces before the clock = right-aligned column, not prose.
            if ok && head.ends_with(' ') {
                s = head.trim_end().to_string();
            }
        }
    }
    s
}

/// Codex usage-reset hint line; its glyph (`•` / `■`, varies by release) is not stored.
fn codex_usage_notice_line(line: &str) -> Option<String> {
    let body = line
        .trim()
        .strip_prefix('•')
        .or_else(|| line.trim().strip_prefix('■'))
        .map(str::trim_start)
        .unwrap_or_else(|| line.trim());
    let lower = body.to_ascii_lowercase();
    if lower.starts_with("you have ")
        && lower.contains("usage limit reset")
        && lower.contains("available")
        && lower.contains("run /usage")
    {
        Some(body.to_string())
    } else {
        None
    }
}

/// `ERROR: You've hit your usage limit. Upgrade to Pro …, or try again at Aug 8th, 2025 1:47 PM.`
fn codex_limit_hit_line(line: &str) -> Option<String> {
    let raw = line.trim();
    if raw.is_empty() {
        return None;
    }
    let s = match raw.chars().next() {
        Some(c) if !c.is_alphanumeric() => raw[c.len_utf8()..].trim(),
        _ => raw,
    };
    let body = s
        .strip_prefix("ERROR:")
        .or_else(|| s.strip_prefix("Error:"))
        .or_else(|| s.strip_prefix("error:"))
        .map(str::trim_start)
        .unwrap_or(s);
    let lower = body.to_ascii_lowercase();
    let hit = lower.contains("hit your usage limit")
        || (lower.contains("usage limit") && (lower.contains("try again") || lower.contains("upgrade to")));
    if !hit {
        return None;
    }
    // Stable ERROR: prefix so the chat styles it as a hard failure.
    if s.to_ascii_lowercase().starts_with("error:") {
        Some(s.to_string())
    } else {
        Some(format!("ERROR: {body}"))
    }
}

fn strip_codex_bullet(line: &str) -> &str {
    line.trim()
        .strip_prefix('•')
        .or_else(|| line.trim().strip_prefix('■'))
        .map(str::trim_start)
        .unwrap_or_else(|| line.trim())
}

/// Rejoin the limit banner that narrow panes wrap across rows, from row `i`; returns where it ends.
fn join_wrapped_limit_hit(lines: &[&str], i: usize) -> (Option<String>, usize) {
    let mut parts: Vec<&str> = vec![strip_codex_bullet(lines[i])];
    let mut j = i + 1;
    while j < lines.len() && parts.len() < 16 {
        let t = lines[j].trim();
        if t.is_empty() || t.starts_with('›') || t.starts_with('❯') || t.starts_with('╭') || t.starts_with('╰') {
            break;
        }
        // Model-picker chrome under the input box — stop.
        if t.to_ascii_lowercase().starts_with("gpt-") || t.contains("max fas") {
            break;
        }
        parts.push(t);
        j += 1;
    }
    (codex_limit_hit_line(&parts.join(" ")), j)
}

fn codex_usage_notice_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<String>, notice: String| {
        if !out.iter().any(|seen| seen == &notice) {
            out.push(notice);
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(notice) = codex_limit_hit_line(line) {
            // When wrapped, `try again at …` (the only reset time) is on the next row; the single row
            // stored `…, visit` with no reset (2026-09-10, codex-astra). Prefer the join only if it adds that.
            let (joined, next) = join_wrapped_limit_hit(&lines, i);
            if !has_try_again(&notice) {
                if let Some(joined) = joined.filter(|j| has_try_again(j)) {
                    push(&mut out, joined);
                    i = next;
                    continue;
                }
            }
            push(&mut out, notice);
            i += 1;
            continue;
        }
        if let Some(notice) = codex_usage_notice_line(line) {
            push(&mut out, notice);
            i += 1;
            continue;
        }
        let head_low = strip_codex_bullet(line).to_ascii_lowercase();
        let looks_hit = head_low.contains("hit your usage")
            || (head_low.contains("error") && head_low.contains("usage"))
            || head_low.contains("you've hit");
        if looks_hit {
            let (joined, next) = join_wrapped_limit_hit(&lines, i);
            if let Some(notice) = joined {
                push(&mut out, notice);
                i = next;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn has_try_again(notice: &str) -> bool {
    notice.to_ascii_lowercase().contains("try again at")
}

fn month_num_token(tok: &str) -> Option<u32> {
    const M: [&str; 12] =
        ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let t = tok.trim_matches(|c: char| !c.is_alphabetic()).to_ascii_lowercase();
    if t.len() < 3 {
        return None;
    }
    M.iter().position(|m| t.starts_with(m)).map(|i| i as u32 + 1)
}

/// `try again at Aug 8th, 2025 1:47 PM` → RFC3339 UTC. The date is optional: same-day resets are
/// a bare `5:07 AM.` (2026-09-10, codex-astra), read as the next time that clock comes round.
fn parse_codex_try_again(notice: &str) -> Option<String> {
    parse_codex_try_again_at(notice, chrono::Local::now())
}

/// 橫幅時間剛過去幾分鐘＝舊橫幅，不要滾到明天。2026-09-13：22:15:22 派工時橫幅還是 `10:15 PM`，
/// 滾成隔天讓兩筆交辦等 24 小時，而 app-server 說 22:20 就重置。
const STALE_BANNER_GRACE_MINS: i64 = 15;
const STALE_BANNER_RETRY_MINS: i64 = 5;

/// 可測版本：`now` 由呼叫端給。
fn parse_codex_try_again_at(notice: &str, now: chrono::DateTime<chrono::Local>) -> Option<String> {
    use chrono::{Datelike, Local, NaiveDate, TimeZone};
    let low = notice.to_ascii_lowercase();
    let rest = low.split("try again at").nth(1)?.trim();
    let mut month = None;
    let mut day = None;
    let mut year = None;
    let mut hour = None;
    let mut minute = 0u32;
    let mut pm = false;
    for tok in rest.split(|c: char| c.is_whitespace() || c == ',').filter(|t| !t.is_empty()) {
        if month.is_none() {
            if let Some(m) = month_num_token(tok) {
                month = Some(m);
                continue;
            }
        }
        let digits: String = tok.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            // The last token is `pm.`: an exact match loses the meridiem (reset 12 h early).
            match tok.trim_matches(|c: char| !c.is_ascii_alphanumeric()) {
                "pm" => pm = true,
                "am" => pm = false,
                _ => {}
            }
            continue;
        }
        if let Some((h, m)) = tok.split_once(':') {
            if let (Ok(h), Ok(m)) = (
                h.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>(),
                m.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>(),
            ) {
                hour = Some(h);
                minute = m;
                let tail = tok.to_ascii_lowercase();
                if tail.contains("pm") {
                    pm = true;
                } else if tail.contains("am") {
                    pm = false;
                }
                continue;
            }
        }
        match digits.len() {
            4 => year = digits.parse().ok(),
            _ if day.is_none() => day = digits.parse().ok(),
            _ if hour.is_none() => hour = digits.parse().ok(),
            _ => {}
        }
        if tok.to_ascii_lowercase().ends_with("pm") {
            pm = true;
        } else if tok.to_ascii_lowercase().ends_with("am") {
            pm = false;
        }
    }
    let mut hour = hour?;
    if pm && hour < 12 {
        hour += 12;
    }
    if !pm && hour == 12 {
        hour = 0;
    }
    // A reset is always ahead: a passed clock time means tomorrow, a passed month/day next year.
    let today = now.date_naive();
    let (date, roll) = match (month, day) {
        (Some(m), Some(d)) => (NaiveDate::from_ymd_opt(year.unwrap_or_else(|| today.year()), m, d)?, year.is_none()),
        _ => (today, true),
    };
    let mut naive = date.and_hms_opt(hour, minute, 0)?;
    if roll && naive <= now.naive_local() {
        // 剛過去幾分鐘＝舊橫幅：晚點再問，不等一整輪。
        let behind = now.naive_local().signed_duration_since(naive);
        if behind <= chrono::Duration::minutes(STALE_BANNER_GRACE_MINS) {
            naive = now.naive_local() + chrono::Duration::minutes(STALE_BANNER_RETRY_MINS);
        } else {
            naive = match (month, day) {
                (Some(_), Some(_)) => naive.with_year(naive.year() + 1)?,
                _ => naive + chrono::Duration::days(1),
            };
        }
    }
    let dt = Local.from_local_datetime(&naive).earliest()?;
    Some(dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// Mirror a Codex hard limit-hit onto that host's quota row immediately (the rate-limits RPC lags).
async fn apply_codex_limit_hit_quota(app: &Arc<App>, host: &str, identity: Option<&str>, notice: &str) {
    let resets = parse_codex_try_again(notice);
    // 寫進這顆 bot 身分的 key：查詢端先查 `codex:<identity>`，以前寫裸 `codex` 對不上（2026-09-13 AGM）。
    let base = crate::quota::quota_base("codex", identity);
    let key = crate::quota::quota_key(host, &base);
    let mut q = app
        .quotas
        .lock()
        .await
        .get(&key)
        .cloned()
        .unwrap_or_else(|| crate::quota::Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "codex-limit-hit".into(),
            account: None,
            host: host.to_string(),
        });
    // 同一張橫幅再看到不是新證據（2026-09-13：掃到 22:15 的舊橫幅卻把 `at` 蓋成現在、量表打回 100%）。
    if q.limit_hit.as_ref().is_some_and(|h| h.message == notice) {
        return;
    }
    // 量表標成用完，但重置時間不從橫幅寫：橫幅時間會舊會歪（同日解析成隔天，交辦等 24 小時）。
    // 只記在 `limit_hit.until`；`resets_at` 留給 app-server／statusLine。
    let win = crate::quota::Window { used_pct: 100.0, resets_at: None };
    if let Some(existing) = q.five_hour.as_mut() {
        existing.used_pct = 100.0;
    } else if let Some(existing) = q.seven_day.as_mut() {
        existing.used_pct = 100.0;
    } else {
        q.five_hour = Some(win);
    }
    // 黏著走，直到 `until` 過了、下一回合成功、或更新的結構化讀數說還有額度（`quota::set`）。
    q.limit_hit = Some(crate::quota::LimitHit {
        message: notice.to_string(),
        until: resets.clone(),
        at: crate::db::now(),
    });
    q.updated_at = crate::db::now();
    q.source = "codex-limit-hit".into();
    crate::quota::set(app, host, &base, q).await;
}

/// No reply marker: keep what follows the last prompt echo minus chrome. `⎿` lines stay — they
/// usually carry the actual error ("Not logged in · Please run /login").
fn clean_screen(kind: &str, text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = after_last_prompt_echo(kind, &lines);
    let mut out: Vec<String> = Vec::new();
    let grok = kind == "grok";
    // grok's telemetry banner wraps at pane width: skip "Help improve Grok" … "Privacy Policy." as a block.
    let mut in_banner = false;
    for line in &lines[start..] {
        let stripped;
        let s = if grok {
            stripped = strip_grok_decor(line);
            stripped.trim()
        } else {
            line.trim()
        };
        if grok {
            if s.starts_with("Help improve Grok") {
                in_banner = true;
            }
            if in_banner {
                if s.starts_with("Read Terms and Privacy") {
                    in_banner = false;
                }
                continue;
            }
        }
        // `is_activity_shape` catches spinner glyphs `is_noise` doesn't know.
        if is_noise(s) || is_activity_shape(s) || (grok && is_grok_noise(s)) {
            continue;
        }
        // An empty prompt box means the transcript ended.
        if s == "❯" || s == "›" {
            break;
        }
        let s = s.strip_prefix("⎿ ").or_else(|| s.strip_prefix("⎿")).unwrap_or(s).trim();
        if s.is_empty() {
            if !out.last().map(|l: &String| l.is_empty()).unwrap_or(true) {
                out.push(String::new());
            }
            continue;
        }
        out.push(s.to_string());
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    let joined = out.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Provider-specific reply extraction. grok has no reply marker (appendix F): always `clean_screen`.
fn extract_reply(kind: &str, text: &str) -> Option<String> {
    if kind == "claude" {
        return crate::capture::claude::PARSER.extract_reply(text);
    }
    let marker = match kind {
        "codex" => "• ",
        _ => return None,
    };
    let lines: Vec<&str> = text.lines().collect();
    // A2: only this turn's output — else the previous turn's `⏺` line is returned as the answer.
    let after_echo = after_last_prompt_echo(kind, &lines);
    let start = after_echo
        + lines[after_echo..].iter().rposition(|l| {
            let s = l.trim_start();
            s.starts_with(marker) && (kind != "codex" || codex_usage_notice_line(s).is_none())
        })?;
    let mut out: Vec<String> = Vec::new();
    for line in &lines[start..] {
        let t = line.trim_end();
        let s = t.trim_start();
        // Stop at the input box / horizontal rule drawn below the transcript.
        if s.starts_with('╭') || s.starts_with('│') || s.starts_with('╰') || s.starts_with('▔') {
            break;
        }
        if !s.is_empty() && s.chars().all(|c| c == '─' || c == '━' || c == '-' || c == '=' || c == '_') {
            break;
        }
        // Skip the spinner / status line and its neighbours (`Tip:`, `✗ Auto-update failed`).
        if s.chars().next().map(is_spinner_glyph).unwrap_or(false) || is_noise(s) {
            continue;
        }
        let cleaned = s.strip_prefix(marker).unwrap_or(t).to_string();
        out.push(cleaned);
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    let joined = out.join("\n").trim().to_string();
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

#[cfg(test)]
mod extract_tests {
    use super::*;

    const NOT_LOGGED_IN: &str = "\
 ▐▛███▛█   Claude Code v2.1.261
▝▜██████▀  Haiku 4.5 · API Usage Billing
  ▝▝ ▝▝    ~/project/hermes-agents/projects/pt

 ⚠ AGENTS.md is over the 40.0k-char limit (57.0k chars) · /memory to free up context

❯ echo 1
  ⎿  Not logged in · Please run /login
   · Run in another terminal: security unlock-keychain

✻ Worked for 0s · done 1:07 AM
────────────────────────────────────────────
❯
────────────────────────────────────────────
  tony. | pt | HAI4.5 | 5h:- | 7d:-
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";

    const CODEX_STARTUP: &str = "\
╭────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.153.4)                  │
╰────────────────────────────────────────────╯

  Tip: Use /init to create an AGENTS.md.

• You have 1 usage limit reset available. Run /usage to use one.

› Write tests for @filename
";

    #[test]
    fn codex_usage_reset_hint_is_captured_without_the_bullet() {
        assert_eq!(
            codex_usage_notice_lines(CODEX_STARTUP),
            vec!["You have 1 usage limit reset available. Run /usage to use one."],
        );
        // Older Codex builds used a square marker and pluralised the noun.
        assert_eq!(
            codex_usage_notice_line(" ■ You have 2 usage limit resets available. Run /usage to use one."),
            Some("You have 2 usage limit resets available. Run /usage to use one.".into()),
        );
        assert!(codex_usage_notice_line("• ordinary assistant text").is_none());
    }

    /// Codex 0.153 idle splash (2026-09-08): must not become a user prompt or a reply.
    const CODEX_IDLE_SPLASH: &str = "\
╭────────────────────────────────────────────╮
│ >_ OpenAI Codex (v0.153.4)                  │
│                                            │
│ model:        gpt-5.6-luna max  fast   /model to change
│ directory:    ~/project/hermes-agents/projects/pt
│ permissions:  YOLO mode
╰────────────────────────────────────────────╯

  Tip: Type / to open the command popup; Tab autocompletes slash commands.

• You have 1 usage limit reset available. Run /usage to use one.

› Ask Codex to do anything

gpt-5.6-luna max fast · ~/project/hermes-agents/projects/pt · Context 0% used · 5h 100% left
";

    #[test]
    fn codex_idle_splash_is_not_a_reply_or_a_user_prompt() {
        assert_eq!(clean_screen("codex", CODEX_IDLE_SPLASH), None, "{:?}", clean_screen("codex", CODEX_IDLE_SPLASH));
        assert_eq!(extract_reply("codex", CODEX_IDLE_SPLASH), None);
        assert_eq!(last_prompt_echo_text("codex", CODEX_IDLE_SPLASH), None);
        assert!(codex_usage_notice_lines(CODEX_IDLE_SPLASH).iter().any(|n| n.contains("usage limit reset")));
    }

    const CODEX_LIMIT_HIT: &str = "\
ERROR: You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), \
or try again at Aug 8th, 2025 1:47 PM.
";

    #[test]
    fn codex_limit_hit_is_captured_as_notice() {
        let lines = codex_usage_notice_lines(CODEX_LIMIT_HIT);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_ascii_lowercase().contains("hit your usage limit"));
        assert!(lines[0].starts_with("ERROR:"));
        assert_eq!(
            codex_limit_hit_line(
                "ERROR: You've hit your usage limit. Upgrade to Pro, or try again at Sep 6th, 2026 3:00 PM."
            )
            .map(|s| s.contains("hit your usage limit")),
            Some(true),
        );
        assert!(codex_limit_hit_line("• ordinary assistant text").is_none());
    }

    /// Real pane when Codex wraps the banner at ~28 columns.
    const CODEX_LIMIT_HIT_WRAPPED: &str = "\
› ping

■ You've hit your usage
limit. Upgrade to Pro
(https://chatgpt.com/ex
plore/pro),
visit
https://chatgpt.com/cod
ex/settings/usage
to purchase more
credits or try again at
1:32 PM.

› Ask Codex to do anyt

  gpt-5.6-luna max fas…
";

    #[test]
    fn codex_limit_hit_survives_narrow_pane_wrap() {
        let lines = codex_usage_notice_lines(CODEX_LIMIT_HIT_WRAPPED);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let n = lines[0].to_ascii_lowercase();
        assert!(n.contains("hit your usage"));
        assert!(n.contains("limit"));
        assert!(n.contains("try again"));
    }

    /// codex-astra 2026-09-10: even a wide pane wraps this banner; the reset half was dropped (`…, visit`).
    const CODEX_LIMIT_HIT_TWO_ROWS: &str = "\
› AGM 交辦：…

■ You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit
https://chatgpt.com/codex/settings/usage to purchase more credits or try again at 5:07 AM.

  1 background terminal running · /ps to view · /stop to close
";

    #[test]
    fn codex_limit_hit_keeps_the_reset_half_off_the_next_row() {
        let lines = codex_usage_notice_lines(CODEX_LIMIT_HIT_TWO_ROWS);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let n = lines[0].to_ascii_lowercase();
        assert!(n.contains("hit your usage limit"));
        assert!(n.contains("try again at 5:07 am"), "the reset half must survive: {n}");
        assert!(parse_codex_try_again(&lines[0]).is_some());
        // A banner that already carries its own reset is not extended by whatever follows it.
        assert_eq!(codex_usage_notice_lines(CODEX_LIMIT_HIT).len(), 1);
    }
    /// 2026-09-13：派工時橫幅剛過去 22 秒是舊字，不能滾到隔天壓 24 小時。
    #[test]
    fn a_banner_that_just_went_stale_does_not_roll_to_tomorrow() {
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 9, 13, 22, 15, 22).unwrap();
        let got = parse_codex_try_again_at("ERROR: You've hit your usage limit, or try again at 10:15 PM.", now)
            .expect("讀得到時間");
        let t = chrono::DateTime::parse_from_rfc3339(&got).unwrap();
        let mins = (t.timestamp() - now.timestamp()) / 60;
        assert!((4..=6).contains(&mins), "應該是幾分鐘後再問，不是隔天：{got}（{mins} 分）");
    }

    /// 真的過去很久的（超過 15 分鐘）才當成「明天的那個時刻」。
    #[test]
    fn a_clock_time_long_past_still_means_tomorrow() {
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 9, 13, 22, 15, 22).unwrap();
        let got = parse_codex_try_again_at("or try again at 9:00 PM.", now).expect("讀得到時間");
        let t = chrono::DateTime::parse_from_rfc3339(&got).unwrap();
        let hours = (t.timestamp() - now.timestamp()) / 3600;
        assert!((22..=23).contains(&hours), "隔天的 21:00：{got}（{hours} 小時後）");
    }


    /// Codex writes a bare clock time when the reset is later today, with no date at all.
    #[test]
    fn codex_try_again_without_a_date_is_the_next_time_that_clock_comes_round() {
        use chrono::{Datelike, Local, TimeZone, Timelike};
        let now = Local::now();
        let at = |h: u32| {
            format!("ERROR: You've hit your usage limit. Upgrade to Pro, or try again at {}:07 {}.",
                if h % 12 == 0 { 12 } else { h % 12 },
                if h < 12 { "AM" } else { "PM" })
        };
        for h in 0..24u32 {
            // 15 分鐘寬限內的鐘點另有規則（見 `a_banner_that_just_went_stale_does_not_roll_to_tomorrow`）。
            let candidate = now.date_naive().and_hms_opt(h, 7, 0).unwrap();
            let behind = now.naive_local().signed_duration_since(candidate);
            if behind >= chrono::Duration::zero() && behind <= chrono::Duration::minutes(STALE_BANNER_GRACE_MINS) {
                continue;
            }
            // 同一個 `now` 解析：各讀各的時鐘時跨過邊界會偶發紅燈（2026-09-13）。
            let parsed = parse_codex_try_again_at(&at(h), now).unwrap_or_else(|| panic!("hour {h} did not parse"));
            let dt = chrono::DateTime::parse_from_rfc3339(&parsed).unwrap().with_timezone(&Local);
            assert_eq!(dt.hour(), h, "{parsed}");
            assert_eq!(dt.minute(), 7);
            assert!(dt > now, "a reset is always ahead of us: {parsed}");
            assert!(dt.signed_duration_since(now).num_hours() < 25, "and never more than a day out: {parsed}");
            // Today or tomorrow, never some other date.
            let day = dt.date_naive();
            assert!(day == now.date_naive() || day == now.date_naive() + chrono::Duration::days(1));
        }
        // A month/day with no year still lands on a real date.
        let r = parse_codex_try_again_at("try again at Aug 8th 1:47 PM.", now).unwrap();
        let dt = chrono::DateTime::parse_from_rfc3339(&r).unwrap().with_timezone(&Local);
        assert_eq!((dt.month(), dt.day()), (8, 8));
        assert!(dt > now);
        let _ = Local.timestamp_opt(0, 0);
        let _ = now.year();
    }

    #[test]
    fn codex_try_again_at_parses_reset() {
        let r = parse_codex_try_again(
            "ERROR: You've hit your usage limit. Upgrade to Pro, or try again at Aug 8th, 2025 1:47 PM.",
        )
        .unwrap();
        assert!(r.starts_with("2025-08-08T"), "{r}");
    }

    #[test]
    fn live_alert_surfaces_codex_limit_hit() {
        let alert = live_alert("codex", CODEX_LIMIT_HIT).unwrap();
        assert!(alert.to_ascii_lowercase().contains("hit your usage limit"));
    }

    #[test]
    fn clean_screen_keeps_only_tool_result_lines() {
        assert!(extract_reply("claude", NOT_LOGGED_IN).is_none());
        let got = clean_screen("claude", NOT_LOGGED_IN).unwrap();
        assert_eq!(got, "Not logged in · Please run /login");
    }

    /// Second turn printed only tool output; the previous `⏺ FIRST-ANSWER` must not be its answer (review A2).
    const TWO_TURNS: &str = "\
❯ echo 1
⏺ FIRST-ANSWER

❯ echo 2
  ⎿  Not logged in · Please run /login

✻ Worked for 0s
────────────────────────────────────────────
❯
";

    #[test]
    fn live_alert_catches_a_retry_banner() {
        let screen = "\
❯ do the thing
● Running 3 shell commands…
  ⎿ $ ls
✻ API error · Retrying in 0s · attempt 1/10";
        assert_eq!(
            live_alert("claude", screen).as_deref(),
            Some("API error · Retrying in 0s · attempt 1/10")
        );
        // codex words it differently; the shape is what matches.
        assert_eq!(
            live_alert("codex", "stream error: 503 upstream; retrying 2/5 in 1s").as_deref(),
            Some("stream error: 503 upstream; retrying 2/5 in 1s")
        );
        // The newest banner wins.
        let two = "API error · Retrying in 0s · attempt 1/10\nAPI error · Retrying in 4s · attempt 2/10";
        assert!(live_alert("claude", two).unwrap().ends_with("attempt 2/10"));
    }

    #[test]
    fn live_alert_ignores_the_agent_talking_about_errors() {
        // Prose that merely mentions an error is not a banner: no retry token.
        assert!(live_alert("claude", "I fixed the error in the parser.").is_none());
        // A retry token with no error is not one either.
        assert!(live_alert("claude", "Retrying the test suite now").is_none());
        // Long prose that happens to contain both stays out.
        let prose = format!("The {} error means we should retry the request later on.", "x".repeat(300));
        assert!(live_alert("claude", &prose).is_none());
        // Printed data saying error + attempt (sqlite row / JSON, 2026-09-08).
        let row = r#"62|note||{"action":"protocol_error","attempt":3,"bot":"t1-dev-2","error":"report.status 必須是 done 或 blocked"}|06:12"#;
        assert!(live_alert("claude", row).is_none());
        assert!(live_alert("claude", r#"{"error":"timeout","retry":true}"#).is_none());
        // …but a real banner still gets through.
        assert!(live_alert("claude", "API error · Retrying in 2s · attempt 2/10").is_some());
    }

    #[test]
    fn extract_reply_ignores_the_previous_turn() {
        assert_eq!(extract_reply("claude", TWO_TURNS), None);
        assert_eq!(clean_screen("claude", TWO_TURNS).unwrap(), "Not logged in · Please run /login");
    }

    #[test]
    fn extract_reply_prefers_marker() {
        let screen = "❯ Reply with PONG\n⏺ PONG\n✻ Cooked for 5s\n──────\n❯\n";
        assert_eq!(extract_reply("claude", screen).unwrap(), "PONG");
    }

    /// grok 1.0.13 `agent.read {source: visible}` (appendix F), columns narrowed.
    const GROK_SCREEN: &str = "\

  /private/tmp/scratch/grok-ws                                       15K / 500K


     ❯ Reply with exactly GROK-OK                                        2:09 AM
                                                                                █
     ◆ user_prompt_submit  [hooks: 1]                                           █
     ◆ Thought for 0.1s                                                         █
                                                                                █
     GROK-OK                                                             2:09 AM   █
                                                                                █
     Worked for 3.6s                                        stop  [hooks: 2]   █
                                                                                █

  Help improve Grok                                       [Opt out] [Opt in]
  Off by default. Opt-in to allow SpaceXAI to retain coding data. Change anytime via
  settings.
  Read Terms and Privacy Policy.

  ╭──────────────────────────────────────────────────────────────────────────╮
  │ ❯                                                                        │
  ╰──────────────────────────────── Grok 4.6 (low) · always-approve ─╯

  Shift+Tab:mode  │  Ctrl+.:shortcuts
";

    #[test]
    fn grok_reply_comes_from_clean_screen() {
        assert_eq!(extract_reply("grok", GROK_SCREEN), None);
        assert_eq!(clean_screen("grok", GROK_SCREEN).unwrap(), "GROK-OK");
    }

    /// Narrower pane: the banner wraps differently and the header carries a git branch.
    const GROK_SCREEN_NARROW: &str = "\
   main ~/project/agents-manager                                15K / 500K
     ❯ Reply with exactly GROK-FALLBACK                           2:20 AM
                                                                            █
     ◆ user_prompt_submit  [hooks: 1]                                       █
     ◆ Thought for 0.3s                                                     █
     GROK-FALLBACK                                                2:20 AM   █
     Worked for 3.3s                                     stop  [hooks: 2]   █
  Help improve Grok                                      [Opt out] [Opt in]
  Off by default. Opt-in to allow SpaceXAI to retain coding data, e.g.,
  prompts, traces, & metrics, for training and debugging purposes.
  Change anytime via settings.
  Read Terms and Privacy Policy.
  ╭───────────────────────────────────────────────────────────────────────╮
  │ ❯                                                                     │
  ╰──────────────────────────────────── Grok 4.5 (high) · always-approve ─╯
  Shift+Tab:mode  │  Ctrl+.:shortcuts
";

    /// grok echo of an attachment prompt (`01M1XSVME9SKEG1NZXG51HFP73`, 2026-09-07): lines 2..n
    /// squeezed onto one `…` row, which was stored as the answer.
    const GROK_ATTACHMENT_ECHO: &str = "\

   main ~/project/agents-manager                                          250K / 500K


     ❯ 是否能有更好的ui表示法                                                7:25 PM

       附加圖片（請讀取這個檔案來查看）： …


     ◆ user_prompt_submit  [hooks: 1]

                                                                                       █

    ⠴ Waiting for response… 16s                                       16s ⇣250k [stop]

  Help improve Grok                                                 [Opt out] [Opt in]
  Off by default. Opt-in to allow SpaceXAI to retain coding data,
  e.g., prompts, traces, & metrics, for training and debugging
  purposes. Change anytime via settings.
  Read Terms and Privacy Policy.

  ╭──────────────────────────────────────────────────────────────────────────────────╮
  │ ❯                                                                                │
  ╰─────────────────────────────────────────────── Grok 4.6 (high) · always-approve ─╯

  Shift+Tab:mode  │  Esc:cancel  │  Ctrl+.:shortcuts
";

    /// What the agent was handed: typed text plus `attach::deliver_text`'s block.
    const SENT_WITH_ATTACHMENT: &str = "是否能有更好的ui表示法\n\n附加圖片（請讀取這個檔案來查看）：\n/Users/m1pro/project/agents-manager/.agents-manager/attachments/01M1XSTPGPMTZ3HYENTBP36125-2026-09-07---7-25-01.png";

    /// Reads as an answer until the echo is removed; the clipped row is all that remains.
    #[test]
    fn a_clipped_attachment_echo_is_not_an_answer() {
        assert_eq!(clean_screen("grok", GROK_ATTACHMENT_ECHO).unwrap(), "附加圖片（請讀取這個檔案來查看）： …");
        let sent = vec![SENT_WITH_ATTACHMENT.to_string()];
        assert_eq!(screen_reply("grok", GROK_ATTACHMENT_ECHO, &sent), None);
    }

    /// Only the delivered text carries the attachment block; matching the typed text let the echo through.
    #[test]
    fn the_typed_line_alone_does_not_cover_the_echo() {
        let typed = vec!["是否能有更好的ui表示法".to_string()];
        assert!(screen_reply("grok", GROK_ATTACHMENT_ECHO, &typed).is_some());
    }

    /// A real reply after the clipped echo survives — only the echo is taken off.
    #[test]
    fn a_reply_after_a_clipped_echo_survives() {
        let screen = GROK_ATTACHMENT_ECHO.replace(
            "附加圖片（請讀取這個檔案來查看）： …",
            "附加圖片（請讀取這個檔案來查看）： …\n\n     好的，我看過圖了。",
        );
        let sent = vec![SENT_WITH_ATTACHMENT.to_string()];
        assert_eq!(screen_reply("grok", &screen, &sent).unwrap(), "好的，我看過圖了。");
    }

    /// The clipped-echo rule keys on content, not `…`: a reply that trails off survives.
    #[test]
    fn a_reply_that_merely_ends_in_an_ellipsis_is_kept() {
        let text = "不太確定，讓我先看看那個檔案…";
        assert_eq!(strip_echoed_prompt(text, SENT_WITH_ATTACHMENT), text);
    }

    /// ...and neither is a fragment too short to be sure about.
    #[test]
    fn a_short_clipped_line_is_not_treated_as_an_echo() {
        assert!(!is_clipped_echo("附加…", &["附加圖片（請讀取這個檔案來查看）："]));
        assert!(is_clipped_echo("附加圖片（請讀取這個檔案來查看）： …", &["附加圖片（請讀取這個檔案來查看）：", "/tmp/a.png"]));
    }

    // composer check behind the stall watchdog's Enter nudge

    /// claude 2.1.263 compacting a 673k session (2026-09-07 11:21): Enter swallowed, text left in the box.
    const CLAUDE_UNSENT_PROMPT: &str = "\
❯ 直接做，且要確保claude裝有herdr 的skill

  Ran 3 shell commands

⏺ 已派出 agents-manager-6verqr-track（pane w8:p25），正在讀 config / state 開始做。

✻ Worked for 2m 14s · done 3:34 PM
                                                new task? /clear to save 673k tokens
──────────────────────────────────────────────────────────────────────────────────
❯ [Image #6]試著對claude max方案增加 fable用量的讀取
  附加圖片（請讀取這個檔案來查看）：
──────────────────────────────────────────────────────────────────────────────────
  tony. | agents-manager | Fable 5.1 67% | 5h:- | 7d:92%(rst 6d 16h) | F5:85%   /rc
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

    const SENT_UNSENT_PROMPT: &str = "試著對claude max方案增加 fable用量的讀取\n\n附加圖片（請讀取這個檔案來查看）：\n/Users/m1pro/project/agents-manager/.agents-manager/attachments/x.png";

    #[test]
    fn an_unsent_prompt_is_seen_in_the_composer() {
        // The box, not the transcript echo; `[Image #6]` in front doesn't hide it.
        assert_eq!(
            composer_text("claude", CLAUDE_UNSENT_PROMPT).unwrap(),
            "[Image #6]試著對claude max方案增加 fable用量的讀取\n附加圖片（請讀取這個檔案來查看）："
        );
        assert!(composer_holds_prompt("claude", CLAUDE_UNSENT_PROMPT, SENT_UNSENT_PROMPT));
    }

    /// claude draws U+00A0 after `❯` (pane read 2026-09-07); the marker must accept any blank.
    #[test]
    fn a_no_break_space_after_the_marker_still_reads_as_the_composer() {
        let screen = CLAUDE_UNSENT_PROMPT.replace("❯ [Image #6]", "❯\u{a0}[Image #6]");
        assert!(composer_text("claude", &screen).is_some());
        assert!(composer_holds_prompt("claude", &screen, SENT_UNSENT_PROMPT));
        assert!(!pane_awaits_input("claude", &screen));
    }

    /// Prompt taken: box empty, echo only in the transcript. Enter here would submit an empty prompt.
    #[test]
    fn an_accepted_prompt_leaves_the_composer_empty() {
        let screen = CLAUDE_UNSENT_PROMPT.replace(
            "❯ [Image #6]試著對claude max方案增加 fable用量的讀取\n  附加圖片（請讀取這個檔案來查看）：",
            "❯",
        );
        assert_eq!(composer_text("claude", &screen), None);
        assert!(!composer_holds_prompt("claude", &screen, SENT_UNSENT_PROMPT));
    }

    /// Someone else's text in the box is not ours.
    #[test]
    fn other_text_in_the_composer_is_not_our_prompt() {
        assert!(!composer_holds_prompt("claude", CLAUDE_UNSENT_PROMPT, "完全不一樣的另一個問題"));
        // grok's empty box (`│ ❯ │`) reads as empty through its border glyphs too.
        assert_eq!(composer_text("grok", GROK_SCREEN), None);
    }

    /// A prompt too short to identify is never matched — every screen contains "hi".
    #[test]
    fn a_very_short_prompt_is_not_matched() {
        assert!(!composer_holds_prompt("claude", CLAUDE_UNSENT_PROMPT, "試"));
    }

    #[test]
    fn the_stall_message_says_an_enter_was_re_sent() {
        assert!(stall_reason(&[], true).contains("補送"));
        assert!(!stall_reason(&[], false).contains("補送"));
    }

    #[test]
    fn grok_banner_is_skipped_as_a_block() {
        assert_eq!(clean_screen("grok", GROK_SCREEN_NARROW).unwrap(), "GROK-FALLBACK");
    }

    #[test]
    fn a_mid_tool_call_screen_is_not_an_answer() {
        // The exact shape that closed a turn with the wrong content (2026-09-07).
        assert!(is_tool_progress("Running 1 shell command…\n\nTip: Run /install-slack-app to use it"));
        assert!(is_tool_progress("Running 4 shell commands…"));
        assert!(is_tool_progress("  Running 1 shell command…  \n  esc to interrupt  "));
    }

    #[test]
    fn a_turning_spinner_means_the_turn_is_not_over() {
        // The screen that closed a turn with chrome as its answer (2026-09-07 11:17).
        let screen = "⏺ 上一句回覆\n\n✢ Baking…\n  ⎿  Tip: Use /memory to view and manage Claude memory\n✗ Auto-update failed · Run claude doctor\n❯ ";
        assert!(pane_still_busy(screen));
        assert!(pane_still_busy("· Philosophising… (33m 33s · ↓ 94.9k tokens)"));
        assert!(pane_still_busy("⠦ Thinking… 52s"));
        // Finished-spinner lines are not "busy".
        assert!(!pane_still_busy("✻ Crunched for 9s · done 11:35 PM\n❯ "));
        assert!(!pane_still_busy("✻ Baked for 25m 50s · done 11:01 AM"));
        assert!(!pane_still_busy("⏺ 做完了。\n❯ "));
        // And none of that chrome survives into a stored reply.
        let screen = "❯ hi\n✢ Baking…\nTip: Use /memory\n✗ Auto-update failed · Run claude doctor\n⏺ 真正的回覆\n╭───╮\n│ ❯ │\n╰───╯";
        assert_eq!(extract_reply("claude", screen).unwrap(), "真正的回覆");
        assert_eq!(clean_screen("claude", screen).unwrap(), "⏺ 真正的回覆");
        // Chrome alone is nothing at all — never a stored "reply".
        assert!(clean_screen("claude", "❯ hi\n✢ Baking…\nTip: Use /memory\n✗ Auto-update failed · Run claude doctor\n❯ ").is_none());
    }

    #[test]
    fn a_real_reply_is_still_stored() {
        // Finished tools (`Ran`, no ellipsis) come with the answer; do not throw that away.
        assert!(!is_tool_progress("Ran 4 shell commands\n\n已追加給同一個 agent 一起做。"));
        // A reply that merely talks about running commands is a reply.
        assert!(!is_tool_progress("我會 Running 1 shell command… 之後再回報結果"));
        // A lone Tip line is a different symptom and must not be swallowed here.
        assert!(!is_tool_progress("Tip: Run /ultrareview for a cloud-based review"));
        assert!(!is_tool_progress(""));
    }

    /// claude mid-turn, nothing printed yet: all chrome, but the user still needs to see it thinking.
    const THINKING_ONLY: &str = "\
❯ 幫我看一下這個 bug
✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)
╭────────────────────────────────────────────╮
│ ❯                                          │
╰────────────────────────────────────────────╯
  tony. | pt | HAI4.5 | 5h:- | 7d:-
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";



    /// 2026-09-06: lines 2..n of a multi-line prompt echo were stored as the agent's answer.
    const ECHOED_BACK: &str = "\
❯ 併行
1 沒事 bot 不會需要停止的動作
2 執行中 能夠show session name agent取的名字

✛ Generating… (4s · thinking)
";
    const SENT: &str = "併行\n1 沒事 bot 不會需要停止的動作\n2 執行中 能夠show session name agent取的名字";

    #[test]
    fn the_users_own_prompt_does_not_come_back_as_the_reply() {
        let scraped = clean_screen("claude", ECHOED_BACK).unwrap_or_default();
        assert_eq!(strip_echoed_prompt(&scraped, SENT), "");
    }

    #[test]
    fn an_unknown_spinner_glyph_never_reaches_a_message() {
        // `✛` is in no glyph list, so only `is_activity_shape` keeps it out of `clean_screen`.
        assert_eq!(clean_screen("claude", "❯ go\n✛ Generating… (4s · thinking)\n"), None);
    }

    #[test]
    fn strip_echoed_prompt_keeps_a_real_reply() {
        let text = "1 沒事 bot 不會需要停止的動作\n2 執行中 能夠show session name agent取的名字\n好的，我來處理。";
        assert_eq!(strip_echoed_prompt(text, SENT), "好的，我來處理。");
    }

    /// 2026-09-06: a pane a couple of columns wide echoed the prompt one glyph per line; it was stored as the reply.
    #[test]
    fn a_prompt_shredded_one_glyph_per_line_is_still_recognised_as_the_echo() {
        let sent = "不要依剩餘量重排 固定 cc0 cc1 codex grok";
        let shredded = "要\n依\n剩\n餘\n量\n重\n排\n固\n定\nc\nc\n0\nc\nc\n1\nc\no\nd\ne\nx\ng\nr\no\nk";
        assert_eq!(strip_echoed_prompt(shredded, sent), "");
        // …and with a real answer after it, only the echo goes.
        let with_reply = format!("{shredded}\n好\n的");
        assert_eq!(strip_echoed_prompt(&with_reply, sent), "好\n的");
    }

    #[test]
    fn the_squashed_strip_does_not_eat_a_reply_that_merely_starts_alike() {
        // Shares only three characters with the prompt: nowhere near SQUASH_MIN.
        assert_eq!(strip_echoed_prompt("不要這樣做，我改用別的方法。", SENT), "不要這樣做，我改用別的方法。");
        // No prompt at all to match against.
        assert_eq!(strip_echoed_prompt("PONG", "hi"), "PONG");
    }

    #[test]
    fn shredded_output_is_recognised() {
        assert!(is_shredded("要\n依\n剩\n餘\n量\n重\n排\n固\n定"));
        // A normal reply is not shredded, however many short lines it happens to have.
        assert!(!is_shredded("好的，我來處理。\n第一步：讀設定。\n第二步：改程式。"));
        // Too few lines to judge.
        assert!(!is_shredded("要\n依\n剩"));
    }

    #[test]
    fn strip_echoed_prompt_leaves_a_partial_match_alone() {
        // The pane wrapped line 2 away: dropping half a real reply is worse than a duplicate.
        let text = "1 沒事 bot 不會需要停止的動作\n好的，我來處理。";
        assert_eq!(strip_echoed_prompt(text, SENT), text);
        // A single-line prompt has no tail to strip.
        assert_eq!(strip_echoed_prompt("PONG", "ping"), "PONG");
    }

    /// grok pane `w8:pK` (2026-09-06): finished at an empty boxed composer while herdr said `working`.
    const GROK_AWAITING: &str = "\
     一
     次
     。
             █

  Help impro
  Off by
  default.

  ╭────────╮
  │ ❯      │
  ╰─ Grok ─╯

  Shift+Tab:
";

    /// `w8:pK` (2026-09-06): every CJK char on its own row; `strip_echoed_prompt` rightly refuses
    /// partial matches, so shredding must be detected.
    #[test]
    fn a_pane_too_narrow_to_read_is_recognised() {
        let shredded = "要\n依\n剩\n餘\n量\n重\n排\n固\n定\nc\nc\n0\nHelp impro\n";
        assert!(is_shredded(shredded));
        // A normal reply must never be mistaken for one, however short its lines are.
        assert!(!is_shredded("好的，我來處理。\n改了三個檔案：\n- a.rs\n- b.rs\n- c.rs\n都跑過測試了。\n"));
        // Too little to judge: a two-line answer is not evidence of a broken pane.
        assert!(!is_shredded("好\n的\n"));

        // Verbatim `w8:pK` (2026-09-06): short-line count alone let it through; widest row = 4 chars gives it away.
        let real = "381K\n❯\n█\n█\n▼\nHel\nOff\nby\ndef\nau…\n";
        assert!(is_shredded(real));

        // The width rule must not fire on a narrow *but legible* reply.
        assert!(!is_shredded("已修好。\n改了 db.rs。\n測試全過。\n沒有其他影響。\n重啟後生效。\n請確認。\n"));
    }

    #[test]
    fn an_empty_composer_is_recognised_through_the_box_frame() {
        assert!(pane_awaits_input("grok", GROK_AWAITING));
        assert!(pane_awaits_input("claude", "⏺ done\n╭────╮\n│ ❯  │\n╰────╯\n"));
        assert!(pane_awaits_input("codex", "• done\n╭────╮\n│ ›  │\n╰────╯\n"));
        // True while claude works too — why the caller pairs it with "nothing changed for N polls".
        assert!(pane_awaits_input("claude", THINKING_ONLY));
    }

    #[test]
    fn a_composer_with_text_in_it_is_not_awaiting_input() {
        assert!(!pane_awaits_input("grok", "╭────────╮\n│ ❯ hi   │\n╰─ Grok ─╯\n"));
        assert!(!pane_awaits_input("claude", "⏺ still writing the answer\n"));
        // Only the tail is searched: an old empty prompt scrolled far up must not count.
        let mut s = String::from("╭──╮\n│ ❯ │\n╰──╯\n");
        for _ in 0..20 {
            s.push_str("output line\n");
        }
        assert!(!pane_awaits_input("claude", &s));
    }

    #[test]
    fn live_activity_surfaces_the_spinner_when_there_is_no_text() {
        assert_eq!(live_reply("claude", THINKING_ONLY), None);
        assert_eq!(live_activity("claude", THINKING_ONLY).unwrap(), "Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)");
    }

    /// Once text prints, `live_reply` works as before and the activity row is still reported.
    #[test]
    fn live_reply_still_wins_once_there_is_output() {
        let screen = "❯ Reply with PONG\n⏺ PONG\n✻ Cooked for 5s\n──────\n❯\n";
        assert_eq!(live_reply("claude", screen).unwrap(), "PONG");
        assert_eq!(live_activity("claude", screen).unwrap(), "Cooked for 5s");
    }

    #[test]
    fn live_activity_takes_the_last_row_and_is_capped() {
        let screen = format!("❯ go\n✻ Thinking…\n✻ {}\n", "x".repeat(200));
        let got = live_activity("claude", &screen).unwrap();
        assert_eq!(got.chars().count(), ACTIVITY_MAX + 1);
        assert!(got.ends_with('…'));
    }

    /// grok `◆` rows carry the scrollbar glyph: `strip_grok_decor` first.
    #[test]
    fn live_activity_reads_grok_event_rows() {
        assert_eq!(live_activity("grok", GROK_SCREEN).unwrap(), "Thought for 0.1s");
    }

    /// Real capture: three minutes in, only this row, UI still 「等待回覆（hook）…」.
    const BOOGIEING: &str = "\
❯ 幫我重構一下
✻ Boogieing… (3m 18s · ↓ 11.0k tokens)
╭────────────────────────────────────────────╮
│ ❯                                          │
╰────────────────────────────────────────────╯
  tony. | pt | HAI4.5 | 5h:- | 7d:-
";

    #[test]
    fn live_activity_reports_the_real_stuck_frame() {
        assert_eq!(live_reply("claude", BOOGIEING), None);
        assert_eq!(live_activity("claude", BOOGIEING).unwrap(), "Boogieing… (3m 18s · ↓ 11.0k tokens)");
    }

    /// Unknown glyph, or none, still recognised by shape.
    #[test]
    fn live_activity_falls_back_to_shape_for_unknown_glyphs() {
        let unknown = "❯ go\n⣾ Puttering… (12s · ↑ 1.2k tokens)\n";
        assert_eq!(live_activity("claude", unknown).unwrap(), "Puttering… (12s · ↑ 1.2k tokens)");
        let bare = "❯ go\nSimmering… (1m 4s)\n";
        assert_eq!(live_activity("claude", bare).unwrap(), "Simmering… (1m 4s)");
    }

    /// …but the shape test must not swallow ordinary prose that happens to use an ellipsis.
    #[test]
    fn activity_shape_ignores_prose() {
        assert!(!is_activity_shape("等一下… (我先看看)"));
        assert!(!is_activity_shape("好的… (see the note below)"));
        assert!(is_activity_shape("Boogieing… (3m 18s · ↓ 11.0k tokens)"));
        assert!(is_activity_shape("✢ Improvising… (5s)"));
    }

    /// Before the prompt echo nothing is reported (no previous turn's spinner).
    #[test]
    fn live_activity_ignores_the_previous_turn() {
        let screen = "❯ echo 1\n✻ Worked for 9s\n❯ echo 2\n";
        assert_eq!(live_activity("claude", screen), None);
    }

    #[test]
    fn grok_decor_strip_keeps_prose_times() {
        assert_eq!(strip_grok_decor("     GROK-OK                 2:09 AM   █"), "     GROK-OK");
        assert_eq!(strip_grok_decor("meet at 2:09 PM"), "meet at 2:09 PM");
        assert_eq!(strip_grok_decor("plain line █"), "plain line");
    }

    /// CLI-typed prompt read off the pane echo; the last echo wins.
    #[test]
    fn last_prompt_echo_text_reads_what_the_user_typed() {
        assert_eq!(last_prompt_echo_text("claude", TWO_TURNS).as_deref(), Some("echo 2"));
        assert_eq!(last_prompt_echo_text("claude", NOT_LOGGED_IN).as_deref(), Some("echo 1"));
        assert_eq!(last_prompt_echo_text("codex", "› 幫我看一下這個 bug\n  thinking…\n").as_deref(), Some("幫我看一下這個 bug"));
    }

    /// grok 常駐的 telemetry 橫幅不算「畫面有東西」，否則 [`idle_threshold`] 誤判講完了（2026-09-13 GROK）。
    #[test]
    fn the_grok_opt_in_banner_is_not_content() {
        let screen = "❯ fix ui\n\n  Help improve Grok                                    [Opt out] [Opt in]\n  Off by default. Opt-in to allow SpaceXAI to retain coding data, e.g.,\n  prompts, traces, & metrics, for training and debugging purposes.\n  Change anytime via settings.\n  Read Terms and Privacy Policy.\n\n  ╭──────────────────────────────╮\n  │ ❯                            │\n  ╰──────── Grok 4.6 (low) ──────╯\n\n  Shift+Tab:mode  │  Ctrl+.:shortcuts\n";
        assert!(live_reply("grok", screen).unwrap_or_default().trim().is_empty(), "橫幅不是回覆");
        assert!(pane_awaits_input("grok", screen));
    }

    /// 2026-09-13（GROK／w168:pN）：15 秒還是空 `❯` 被當成等輸入，回覆 36 秒後才到。沒印過東西要多等。
    #[test]
    fn a_pane_that_never_rendered_anything_gets_a_longer_grace() {
        // 只有「herdr 說閒著」+「印過東西然後停住」才走短的那條。
        assert_eq!(idle_threshold(true, "idle"), IDLE_POLLS);
        assert_eq!(idle_threshold(false, "idle"), IDLE_POLLS_SILENT);
        // herdr 說還在跑：可能真的在做事（2026-09-13 GROK）。
        assert_eq!(idle_threshold(true, "working"), IDLE_POLLS_SILENT);
        assert_eq!(idle_threshold(false, "working"), IDLE_POLLS_SILENT);
        // 讀不到狀態時不要比原本更急。
        assert_eq!(idle_threshold(true, ""), IDLE_POLLS);
        assert!(IDLE_POLLS_SILENT > IDLE_POLLS * 3, "要明顯長過那 14 秒，不然等於沒改");
    }

    /// 2026-09-12 使用者實機：長 prompt 折成兩行，下半句被當成回覆。續行要算進回音。
    #[test]
    fn last_prompt_echo_text_takes_the_wrapped_continuation() {
        let screen = "❯ 請直接呼叫 AskUserQuestion 工具問我兩題：第二題 header『功能』請設 multiSelect:\n  true，四個選項：『站內搜尋』『SEO 是主要目的』。問完就停著等我回答。\n  ⎿  You've reached your Fable limit. Run /usage-credits to continue.\n\n✻ Worked for 0s · done 5:08 PM\n";
        let got = last_prompt_echo_text("claude", screen).expect("有回音");
        assert!(got.starts_with("請直接呼叫 AskUserQuestion"), "第一行還在：{got}");
        assert!(got.contains("問完就停著等我回答。"), "折行的下半句要收進來：{got}");
        // 工具結果那行不是使用者說的話。
        assert!(!got.contains("Fable limit"), "`⎿` 開頭的是 claude 的輸出：{got}");
    }

    /// 續行只吃「縮排且不是別的東西」，寧可保守；回音沒排到行尾就沒有續行。
    #[test]
    fn a_short_echo_row_has_no_continuation() {
        assert!(!echo_row_is_full("❯ echo 2"));
        assert!(!echo_row_is_full("› 幫我看一下這個 bug"));
        assert!(echo_row_is_full("❯ 請直接呼叫 AskUserQuestion 工具問我兩題：第二題 header『功能』請設 multiSelect:"));
    }

    #[test]
    fn echo_continuation_stops_at_anything_that_is_not_the_same_sentence() {
        // 沒縮排 = 下一塊內容
        assert_eq!(echo_continuation("claude", "⏺ 我看了一下"), None);
        assert_eq!(echo_continuation("claude", "done"), None);
        // 縮排但是輸出標記／狀態列／框線
        for l in ["  ⎿  結果", "  ⏺ 回覆", "  ✻ Worked for 0s", "  │ box", "  ╭─────"] {
            assert_eq!(echo_continuation("claude", l), None, "{l} 不是續行");
        }
        // 空行
        assert_eq!(echo_continuation("claude", "   "), None);
        // 下一個回音（使用者連送兩句）
        assert_eq!(echo_continuation("claude", "  ❯ 第二句"), None);
        // 真的續行
        assert_eq!(echo_continuation("claude", "  第二半句"), Some("第二半句"));
    }

    /// 收進續行後 `strip_echoed_prompt` 才吃得到整段回音——症狀真正修掉的地方。
    #[test]
    fn the_wrapped_half_no_longer_looks_like_a_reply() {
        let screen = "❯ 第一半句很長很長，長到排滿整行才會折到下一行去，這是折行的前提\n  第二半句也不短\n  ⎿  真正的回覆\n";
        let prompt = last_prompt_echo_text("claude", screen).expect("有回音");
        let left = strip_echoed_prompt("第二半句也不短\n⎿  真正的回覆", &prompt);
        assert!(!left.contains("第二半句"), "回音要被剝掉：{left}");
        assert!(left.contains("真正的回覆"), "回覆要留著：{left}");
    }

    /// grok's clock and scrollbar glyph on the echo row are not part of the prompt.
    #[test]
    fn last_prompt_echo_text_strips_grok_decor() {
        let screen = "❯ Reply with GROK-OK                    2:09 AM   █\n     GROK-OK\n";
        assert_eq!(last_prompt_echo_text("grok", screen).as_deref(), Some("Reply with GROK-OK"));
    }

    /// No echo / empty box / unknown CLI: report nothing (`begin_external_turn` opens with no user message).
    #[test]
    fn last_prompt_echo_text_is_none_without_an_echo() {
        assert_eq!(last_prompt_echo_text("claude", "⏺ orphaned reply\n"), None);
        assert_eq!(last_prompt_echo_text("claude", "❯\n"), None);
        assert_eq!(last_prompt_echo_text("claude", "❯    \n"), None);
        assert_eq!(last_prompt_echo_text("unknown", "❯ hello\n"), None);
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
mod abandon_tests {
    use super::*;
    use crate::testing as tt;

    async fn completed_turn(status: &str) -> (tt::Env, String, String) {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'abandon-test','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web',?,'unknown',?,?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(status)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        insert_message(&app, &conversation_id, Some(&turn_id), "assistant", "already complete", "hook", false, None)
            .await
            .unwrap();
        (env, turn_id, conversation_id)
    }

    #[tokio::test]
    async fn abandon_of_a_completed_unknown_delivery_turn_is_a_noop_conflict() {
        for status in ["completed", "completed_fallback"] {
            let (env, turn_id, conversation_id) = completed_turn(status).await;
            let app = env.app.clone();
            let before = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
                .bind(&turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
            let before_messages = sqlx::query_as::<_, (String, String, String, String)>(
                "SELECT id, role, content, source FROM messages WHERE conversation_id=? ORDER BY id",
            )
            .bind(&conversation_id)
            .fetch_all(&app.db)
            .await
            .unwrap();

            let err = abandon_turn(&app, &turn_id).await.expect_err("completed turns cannot be abandoned");
            match err {
                LcError::Conflict(body) => {
                    assert_eq!(body["reason"], "turn is neither in-flight nor of unknown delivery");
                    assert_eq!(body["turn_id"], turn_id);
                }
                other => panic!("expected conflict, got {other:?}"),
            }

            let after = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
                .bind(&turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
            assert_eq!(after.status, before.status);
            assert_eq!(after.delivery, before.delivery);
            assert_eq!(after.completed_at, before.completed_at);
            let after_messages = sqlx::query_as::<_, (String, String, String, String)>(
                "SELECT id, role, content, source FROM messages WHERE conversation_id=? ORDER BY id",
            )
            .bind(&conversation_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
            assert_eq!(after_messages, before_messages);
        }
    }

    #[tokio::test]
    async fn a_completed_unknown_delivery_turn_does_not_block_a_new_prompt() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'prompt-unknown','codex','[]',0,1,'tok',?)",
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
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,?,'web','completed','unknown',?,?)",
        )
        .bind(db::ulid())
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt_grouped(&app, &bot_id, "next", "request-1", None, None, &[], None)
            .await
            .expect("a completed unknown-delivery row must not block the next prompt");
        assert_eq!(out.delivery, "unknown", "the mock RPC was reached and failed delivery, rather than the stale row blocking it");
    }
}

#[cfg(test)]
mod child_restart_tests {
    //! `restart_child_in_pane` when the agent will not leave (review 2026-09-12 #1).
    use super::*;
    use crate::testing as tt;

    /// Agent ignores ctrl+c: after 20 polls the `stopping` run must return to `running` (else 409s
    /// and a permanently yellow bot).
    #[tokio::test]
    async fn a_child_that_will_not_exit_gets_its_run_back() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let parent = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'alfa','claude','[]',0,1,'tok',?)",
        )
        .bind(&parent)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let kid = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
             VALUES (?,?,'ui','claude','[]',0,0,'tok','child',?,?)",
        )
        .bind(&kid)
        .bind(&env.project_id)
        .bind(&parent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,'proj-alfa-ui','test',1,?)",
        )
        .bind(&run_id)
        .bind(&kid)
        .bind(&ws.workspace_id)
        .bind(&kid_pane.tab_id)
        .bind(&kid_pane.pane_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        // The mock drops an agent on ctrl+c only by name; `name: null` (herdr 0.8.2 after a same-named
        // restart) stays put — a stand-in for one ignoring ctrl+c.
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": null, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id,
            "cwd": "/tmp/p"})];

        let err = restart_child_in_pane(&app, &kid).await.expect_err("the agent never left");
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");

        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, "running", "the agent is still in its pane, so the run is still live");
        assert!(run.ended_at.is_none());
        assert_eq!(db::active_run(&app.db, &kid).await.unwrap().map(|r| r.id), Some(run_id));
        // Nothing touched the pane.
        assert!(env.herdr.tab(&kid_pane.tab_id).unwrap().panes.contains(&kid_pane.pane_id));
        assert!(!env.herdr.methods().iter().any(|m| m == "pane.close"));
    }
}

#[cfg(test)]
mod default_session_tests {
    //! SPEC §6.5.1: a run in the user's `default` session is observed; its pane is never closed
    //! or re-created (review 2026-09-12 #4).
    use super::*;
    use crate::testing as tt;

    async fn imported_bot(env: &tt::Env) -> (String, String, crate::herdr::PaneInfo) {
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "mine", json!({})).await.unwrap();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, herdr_session, created_at)
             VALUES (?,?,'mine','claude','[]',0,0,'tok','default',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, adopted, started_at)
             VALUES (?,?,'running','idle',?,?,?,'mine','default',1,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "mine", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];
        (bot_id, run_id, pane)
    }

    /// Stop sends ctrl+c and ends the run, but the user's pane and tab stay exactly as they were.
    #[tokio::test]
    async fn stop_never_closes_the_users_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, pane) = imported_bot(&env).await;

        assert!(stop_bot(&app, &bot_id).await.unwrap());

        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, "stopped");
        let methods = env.herdr.methods();
        assert!(methods.iter().any(|m| m == "agent.send_keys"), "the agent was asked to exit");
        assert!(!methods.iter().any(|m| m == "pane.close" || m == "tab.close"), "{methods:?}");
        assert!(env.herdr.tab(&pane.tab_id).unwrap().panes.contains(&pane.pane_id), "the pane is still there");
    }

    /// Start / restart refused with a UI reason, before any ctrl+c.
    #[tokio::test]
    async fn start_and_restart_are_refused_before_touching_the_agent() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, _pane) = imported_bot(&env).await;

        let reason = |e: LcError| match e {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
            other => panic!("expected 409, got {other:?}"),
        };
        assert_eq!(reason(restart_bot(&app, &bot_id).await.unwrap_err()), "default_session");
        assert_eq!(reason(restart_bot_with(&app, &bot_id, StartOpts { resume_native: true }).await.unwrap_err()), "default_session");
        assert_eq!(db::active_run(&app.db, &bot_id).await.unwrap().map(|r| r.id), Some(run_id.clone()), "still running");
        assert!(!env.herdr.methods().iter().any(|m| m == "agent.send_keys"), "no ctrl+c was sent");

        // With no run at all, `start` is what the sidebar button would call.
        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
            .bind(db::now())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        assert_eq!(reason(start_bot(&app, &bot_id).await.unwrap_err()), "default_session");
        let creates = env.herdr.methods().iter().filter(|m| *m == "workspace.create").count();
        assert_eq!(creates, 1, "only the fixture's own workspace.create; the daemon made none in the user's session");
    }
}

#[cfg(test)]
mod tab_tests {
    //! One bot, one tab (and the retrofit). The mock herdr keeps real tab/pane bookkeeping but,
    //! unlike herdr 0.8.2, does not reap empty tabs — so these see whether the daemon tidies up itself.
    use super::*;
    use crate::testing as tt;

    async fn a_bot(env: &tt::Env, name: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
        )
        .bind(&id)
        .bind(&env.project_id)
        .bind(name)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        id
    }

    /// A DB failure after `workspace.create` must still remove the root pane and its tab (trigger-injected).
    #[tokio::test]
    async fn a_run_mapping_failure_closes_the_new_pane_and_tab() {
        let env = tt::env().await;
        let bot_id = a_bot(&env, "alfa").await;
        sqlx::query(
            "CREATE TRIGGER fail_run_mapping BEFORE UPDATE OF workspace_id, pane_id, tab_id ON runs
             BEGIN SELECT RAISE(ABORT, 'forced run mapping failure'); END",
        )
        .execute(&env.app.db)
        .await
        .unwrap();

        assert!(start_bot(&env.app, &bot_id).await.is_err());

        let workspace_id = db::project(&env.app.db, &env.project_id)
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .expect("workspace.create ran before the mapping failure");
        assert!(env.herdr.tabs_in(&workspace_id).is_empty(), "the failed start left a tab behind");
        let methods = env.herdr.methods();
        assert!(methods.contains(&"pane.close".into()), "cleanup did not close the pane: {methods:?}");
        assert!(methods.contains(&"tab.close".into()), "cleanup did not close the empty tab: {methods:?}");

        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE bot_id = ? ORDER BY started_at DESC LIMIT 1")
            .bind(&bot_id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(state, "exited");
    }

    async fn run_row(app: &Arc<App>, id: &str) -> db::Run {
        sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id = ?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    /// A run that is `running` on `pane_id`, with whatever `tab_id` the caller says.
    async fn running_on(app: &Arc<App>, bot_id: &str, ws: &str, pane: &str, tab: Option<&str>) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'agent','test',?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(ws)
        .bind(pane)
        .bind(tab)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        id
    }

    /// Every start pre-trusts its cwd, or claude's "trust this project?" prompt (cursor on *No*) fails it.
    #[tokio::test]
    async fn a_fresh_working_directory_is_trusted_before_the_agent_starts() {
        let dir = std::env::temp_dir().join(format!("am-trust-start-{}", crate::db::ulid()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let store = dir.join(".claude.json");
        std::fs::write(&store, "{\"numStartups\":7}").unwrap();

        let cwd = repo.to_string_lossy().to_string();
        let wrote = crate::trust::mark_trusted("claude", &store, &[crate::trust::canonical(&cwd)]).unwrap();
        assert!(wrote, "a directory the CLI has not seen is recorded");

        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 7, "the CLI's own state survives the merge");
        let key = crate::trust::canonical(&cwd);
        assert_eq!(v["projects"][&key]["hasTrustDialogAccepted"], true);
        // Resolved path: macOS `/tmp` is a symlink and the CLI compares its `getcwd()`.
        assert!(!key.starts_with("/tmp/"), "the recorded path is canonical, got {key}");

        assert!(!crate::trust::mark_trusted("claude", &store, &[key]).unwrap(), "already trusted: left alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Starting a bot creates a tab (never `pane.split`): tabs don't divide a fixed width.
    #[tokio::test]
    async fn a_starting_bot_gets_a_tab_of_its_own() {
        let env = tt::env().await;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

        let e = json!({"AM_BOT_ID": "b1", "AM_HOOK_TOKEN": "tok"});
        let first = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &e, None).await.unwrap();
        let second = acquire_run_pane(&client, &ws.workspace_id, "/tmp/worktree", "bravo", &e, None).await.unwrap();

        assert!(!env.herdr.methods().iter().any(|m| m == "pane.split"), "a bot is never split into a shared tab");
        assert_ne!(first.tab_id, second.tab_id, "two bots, two tabs — they do not share a width");
        assert_ne!(first.tab_id, root.tab_id, "and neither lands in the workspace's own tab");
        for t in [&first.tab_id, &second.tab_id] {
            assert_eq!(env.herdr.tab(t).unwrap().panes.len(), 1, "each tab holds exactly the one bot's pane");
        }

        // Same cwd/env contract as `pane.split`, nickname on the tab bar, never steals focus.
        let p = env.herdr.first_call("tab.create").expect("tab.create was called");
        assert_eq!(p["workspace_id"], json!(ws.workspace_id));
        assert_eq!(p["cwd"], json!("/tmp/p"));
        assert_eq!(p["label"], json!("alfa"));
        assert_eq!(p["focus"], json!(false), "starting a bot must not yank the user's focus");
        assert_eq!(p["env"]["AM_BOT_ID"], json!("b1"));
        assert_eq!(p["env"]["AM_HOOK_TOKEN"], json!("tok"));
    }

    /// A fresh workspace is already one tab / one pane: the first bot uses the root pane.
    #[tokio::test]
    async fn the_first_bot_in_a_fresh_workspace_reuses_its_root_pane() {
        let env = tt::env().await;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

        let got =
            acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), Some(root.clone())).await.unwrap();

        assert_eq!(got.pane_id, root.pane_id);
        assert_eq!(got.tab_id, root.tab_id);
        assert!(env.herdr.first_call("tab.create").is_none(), "no second tab for a workspace that is one already");
        assert_eq!(env.herdr.tabs_in(&ws.workspace_id).len(), 1);
    }

    /// Stopping a bot takes its tab too, so no row of empty tabs builds up.
    #[tokio::test]
    async fn stopping_a_bot_closes_the_tab_it_owned() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), None).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        running_on(&app, &bot, &ws.workspace_id, &pane.pane_id, Some(&pane.tab_id)).await;

        assert!(stop_bot(&app, &bot).await.unwrap());

        assert!(env.herdr.tab(&pane.tab_id).is_none(), "the bot's own tab went with it");
        assert_eq!(env.herdr.tabs_in(&ws.workspace_id).len(), 1, "only the workspace's own tab is left");
    }

    /// A pane sharing its tab only loses the pane; closing the tab would kill a neighbour's agent.
    #[tokio::test]
    async fn stopping_a_bot_in_a_shared_tab_leaves_the_tab_alone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        // The old world: two bots split into the workspace's single tab.
        let neighbour = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let mine = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        assert_eq!(mine.tab_id, neighbour.tab_id);

        let bot = a_bot(&env, "alfa").await;
        // Reconcile fills `tab_id` for old runs too, so "has a tab id" can't decide closing.
        running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, Some(&mine.tab_id)).await;

        assert!(stop_bot(&app, &bot).await.unwrap());

        let tab = env.herdr.tab(&mine.tab_id).expect("the shared tab survives");
        assert!(!tab.panes.contains(&mine.pane_id), "our pane is gone");
        assert!(tab.panes.contains(&neighbour.pane_id), "the neighbour's agent is untouched");
    }

    /// Retrofit: a bot in a shared tab gets its own via a move; `pane_id` must survive.
    #[tokio::test]
    async fn moving_a_running_bot_gives_it_a_tab_without_changing_its_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let neighbour = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let mine = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        // NULL tab_id: a run from before the column existed.
        let run = running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, None).await;

        move_pane_to_own_tab(&app, &bot).await.unwrap();

        let r = run_row(&app, &run).await;
        assert_eq!(r.pane_id.as_deref(), Some(mine.pane_id.as_str()), "a move, not a restart: same pane");
        assert_eq!(r.state, "running");
        let tab = r.tab_id.expect("the new tab was recorded on the run");
        assert_ne!(tab, mine.tab_id);
        assert_eq!(env.herdr.tab(&tab).unwrap().panes, vec![mine.pane_id.clone()], "it has the tab to itself");
        assert_eq!(env.herdr.first_call("pane.move").unwrap()["destination"]["label"], json!("alfa"));
        assert_eq!(env.herdr.first_call("pane.move").unwrap()["focus"], json!(false));
        // The tab it left still holds the neighbour, so it is not closed.
        assert!(env.herdr.tab(&neighbour.tab_id).unwrap().panes.contains(&neighbour.pane_id));
    }

    /// The shared tidy-up decides from herdr's pane count, not our records. "Not found" is normal in
    /// production (herdr 0.8.2 reaps tabs); only the non-reaping mock reaches `tab.close`.
    #[tokio::test]
    async fn the_tidy_up_closes_an_empty_tab_and_only_an_empty_one() {
        let env = tt::env().await;
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let alone = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), None).await.unwrap();
        let busy = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "bravo", &json!({}), None).await.unwrap();

        // Occupied: left alone.
        close_tab_if_empty(&client, &ws.workspace_id, &busy.tab_id).await;
        assert!(env.herdr.tab(&busy.tab_id).is_some());

        // Emptied: closed.
        client.pane_close(&alone.pane_id).await.unwrap();
        close_tab_if_empty(&client, &ws.workspace_id, &alone.tab_id).await;
        assert!(env.herdr.tab(&alone.tab_id).is_none());

        // Already gone: not an error, and nothing else is touched.
        close_tab_if_empty(&client, &ws.workspace_id, &alone.tab_id).await;
        assert!(env.herdr.tab(&busy.tab_id).is_some());
    }

    /// Idempotence: on herdr a second move rebuilds the tab and renumbers the tab bar.
    #[tokio::test]
    async fn moving_a_bot_that_already_owns_its_tab_changes_nothing() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let mine = acquire_run_pane(&client, &ws.workspace_id, "/tmp/p", "alfa", &json!({}), None).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        let run = running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, None).await;

        move_pane_to_own_tab(&app, &bot).await.unwrap();

        assert!(env.herdr.first_call("pane.move").is_none(), "nothing to move");
        let r = run_row(&app, &run).await;
        assert_eq!(r.tab_id.as_deref(), Some(mine.tab_id.as_str()), "the tab it already had is recorded");
        assert_eq!(env.herdr.tab(&mine.tab_id).unwrap().panes, vec![mine.pane_id]);
    }

    /// No active run, or a run with no pane behind it, is a 404 — not a 502 and not a panic.
    #[tokio::test]
    async fn moving_a_bot_that_is_not_running_is_a_not_found() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = a_bot(&env, "alfa").await;

        assert!(matches!(move_pane_to_own_tab(&app, &bot).await, Err(LcError::NotFound(_))));

        let id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at)
             VALUES (?,?,'running','idle','test',?)",
        )
        .bind(&id)
        .bind(&bot)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert!(matches!(move_pane_to_own_tab(&app, &bot).await, Err(LcError::NotFound(_))));
    }
}

#[cfg(test)]
mod hookless_capture_tests {
    //! 對話 for a hookless child run (`managed_by='child'`): scraped off a mock herdr pane via
    //! `capture_hookless_turn_locked`.
    use super::*;
    use crate::testing as tt;

    /// A finished claude exchange as `recent_unwrapped` renders it.
    const EXCHANGE: &str = "\
❯ 幫我看一下 lifecycle.rs
  ⎿  Read lifecycle.rs (4308 lines)
⏺ 看完了：try_fallback 只認 in-flight turn。

✻ Worked for 9s · done 11:35 PM
────────────────────────────────────────────
❯
────────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

    struct Child {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
    }

    /// An adopted hookless bot on `pane-1`; `hooks` / `adopted` decide whether the terminal is the source.
    async fn child(status: &str, screen: &str, hooks: i64, adopted: i64) -> Child {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, managed_by, hook_token, created_at)
             VALUES (?,?,'lastq','claude','[]',0,?,'child','tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(hooks)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, started_at)
             VALUES (?,?,'running',?,'ws-1','pane-1','tab-1',?,'parent-lastq','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(status)
        .bind(adopted)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        env.herdr.set_screen("pane-1", screen);
        Child { env, bot_id, conv, run_id }
    }

    async fn messages(app: &Arc<App>, conv: &str) -> Vec<(String, String, String)> {
        sqlx::query_as::<_, (String, String, String)>(
            // Same order as `GET /api/bots/{id}/messages`.
            "SELECT role, content, source FROM messages WHERE conversation_id = ? ORDER BY id",
        )
        .bind(conv)
        .fetch_all(&app.db)
        .await
        .unwrap()
    }

    /// The bug: a child adopted mid-answer hits `working -> idle` with no turn in flight, and
    /// `try_fallback` bails — 對話 stayed empty. The edge now becomes a turn of its own.
    #[tokio::test]
    async fn a_finished_exchange_on_the_pane_becomes_a_turn() {
        let c = child("idle", EXCHANGE, 0, 1).await;
        let app = c.env.app.clone();

        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap(), "the screen holds an exchange");

        let msgs = messages(&app, &c.conv).await;
        assert_eq!(msgs.len(), 2, "one prompt, one reply: {msgs:?}");
        assert_eq!(msgs[0].0, "user");
        assert_eq!(msgs[0].1, "幫我看一下 lifecycle.rs", "what was typed straight into the pane");
        assert_eq!(msgs[0].2, "terminal_fallback");
        assert_eq!(msgs[1].0, "assistant");
        assert_eq!(
            msgs[1].1, "看完了：try_fallback 只認 in-flight turn。",
            "the `⏺` reply, with the status line and the composer chrome dropped",
        );
        assert_eq!(msgs[1].2, "terminal_fallback");
        let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE conversation_id = ?")
            .bind(&c.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((t.origin.as_str(), t.status.as_str()), ("external", "completed_fallback"));
        assert_eq!(t.run_id.as_deref(), Some(c.run_id.as_str()));
        assert!(t.completed_at.is_some(), "nothing is left in flight, so the composer is not locked");
    }

    /// Repeated `working -> idle` edges and re-adoption must not grow the conversation.
    #[tokio::test]
    async fn the_same_screen_is_never_stored_twice() {
        let c = child("idle", EXCHANGE, 0, 1).await;
        let app = c.env.app.clone();

        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
        assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap(), "nothing new on the pane");
        assert!(!capture_hookless_turn_locked(&app, &c.run_id, true).await.unwrap(), "and the adoption seed is one-shot");

        assert_eq!(messages(&app, &c.conv).await.len(), 2);
    }

    /// The next prompt typed into the pane is a second turn, not an addition to the first.
    #[tokio::test]
    async fn the_next_exchange_is_its_own_turn() {
        let c = child("idle", EXCHANGE, 0, 1).await;
        let app = c.env.app.clone();
        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());

        c.env.herdr.set_screen("pane-1", &format!("{EXCHANGE}❯ 再看一次\n⏺ 修好了。\n"));
        assert!(capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());

        let msgs = messages(&app, &c.conv).await;
        assert_eq!(msgs.len(), 4, "{msgs:?}");
        assert_eq!(msgs[2].1, "再看一次");
        assert_eq!(msgs[3].1, "修好了。");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?")
            .bind(&c.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 2);
    }

    /// A pane we started keeps hooks as source of truth (SPEC §4.3: snapshot is the 備援); this path must not touch it.
    #[tokio::test]
    async fn a_run_with_hooks_is_left_to_its_hooks() {
        let c = child("idle", EXCHANGE, 1, 1).await;
        let app = c.env.app.clone();
        assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
        assert!(messages(&app, &c.conv).await.is_empty());

        // Same for a run the daemon started itself, hooks or not.
        let own = child("idle", EXCHANGE, 0, 0).await;
        let app = own.env.app.clone();
        assert!(!capture_hookless_turn_locked(&app, &own.run_id, false).await.unwrap());
        assert!(messages(&app, &own.conv).await.is_empty());
    }

    /// No cursor and no echo: storing the whole scrollback would merge turns, so store nothing and remember the cursor.
    #[tokio::test]
    async fn a_screen_with_no_prompt_echo_is_not_guessed_at() {
        let c = child("idle", "⏺ 一段沒有頭的舊輸出\n", 0, 1).await;
        let app = c.env.app.clone();

        assert!(!capture_hookless_turn_locked(&app, &c.run_id, false).await.unwrap());
        assert!(messages(&app, &c.conv).await.is_empty());
        assert!(
            db::run(&app.db, &c.run_id).await.unwrap().unwrap().last_read_tail_hash.is_some(),
            "the cursor moved, so the next real exchange is read from here",
        );
    }

    /// The adoption seed only fires into an empty conversation (re-adoption on every restart / reconnect).
    #[tokio::test]
    async fn the_adoption_seed_refuses_a_conversation_that_already_has_messages() {
        let c = child("idle", EXCHANGE, 0, 1).await;
        let app = c.env.app.clone();
        insert_message(&app, &c.conv, None, "user", "早先的訊息", "web", false, None).await.unwrap();

        assert!(!capture_hookless_turn_locked(&app, &c.run_id, true).await.unwrap());
        assert_eq!(messages(&app, &c.conv).await.len(), 1);
        let _ = &c.bot_id;
    }
}

#[cfg(test)]
mod issue_17_tests {
    use super::*;
    use crate::testing as tt;

    const FALLBACK_SCREEN: &str = "❯ Reply with PONG\n⏺ PONG\n✻ Worked for 5s · done 1:07 AM\n──────\n❯\n";

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conversation_id: String,
        run_id: String,
        turn_id: String,
    }

    async fn fixture(kind: &str, screen: &str) -> Fixture {
        let env = tt::env().await;
        let bot_id = db::ulid();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, hook_token, created_at) VALUES (?,?,?, ?, 'tok', ?)")
            .bind(&bot_id)
            .bind(&env.project_id)
            .bind("issue-17")
            .bind(kind)
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let conversation_id = db::conversation_id(&env.app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at)
             VALUES (?,?,'running','idle','pane-17','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at)
             VALUES (?,?,?,'user','Reply with PONG','web',?)",
        )
        .bind(db::ulid())
        .bind(&conversation_id)
        .bind(&turn_id)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        env.herdr.set_screen("pane-17", screen);
        Fixture { env, bot_id, conversation_id, run_id, turn_id }
    }

    async fn event_kinds(mut rx: tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> Vec<String> {
        let mut kinds = Vec::new();
        while kinds.len() < 2 {
            kinds.push(tokio::time::timeout(Duration::from_secs(1), rx.recv()).await.unwrap().unwrap().kind);
        }
        kinds
    }

    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(id)
            .fetch_one(&app.db)
            .await
            .unwrap()
    }

    /// Re-arming invalidates the old task; at the lock it returns without touching the newer registration.
    #[tokio::test]
    async fn an_old_fallback_timer_gives_up_inside_the_bot_lock() {
        let env = tt::env().await;
        let app = env.app.clone();
        let lock = app.bot_lock("bot-17").await;
        let guard = lock.lock().await;

        arm_fallback(&app, "run-17", "bot-17").await;
        let old = *app.fallback_timers.lock().await.get("run-17").unwrap();
        arm_fallback(&app, "run-17", "bot-17").await;
        let current = *app.fallback_timers.lock().await.get("run-17").unwrap();
        assert_ne!(old, current, "re-arming advances the generation");

        tokio::time::sleep(Duration::from_secs(5) + Duration::from_millis(50)).await;
        assert_eq!(*app.fallback_timers.lock().await.get("run-17").unwrap(), current);
        drop(guard);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(app.fallback_timers.lock().await.get("run-17").is_none());
    }

    #[tokio::test]
    async fn fallback_commits_assistant_message_before_turn_updated() {
        let f = fixture("claude", FALLBACK_SCREEN).await;
        let app = f.env.app.clone();
        let rx = app.subscribe();

        assert!(try_fallback(&app, &f.run_id).await.unwrap());
        let messages: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, content FROM messages WHERE conversation_id=? ORDER BY created_at, id",
        )
        .bind(&f.conversation_id)
        .fetch_all(&app.db)
        .await
        .unwrap();
        assert_eq!(messages, vec![("user".into(), "Reply with PONG".into()), ("assistant".into(), "PONG".into())]);
        assert_eq!(turn(&app, &f.turn_id).await.status, "completed_fallback");
        let kinds = event_kinds(rx).await;
        assert_eq!(kinds, vec!["message_added", "turn_updated"]);
    }

    #[tokio::test]
    async fn stall_commits_system_message_before_turn_updated() {
        let f = fixture("claude", "not logged in\n").await;
        let app = f.env.app.clone();
        let rx = app.subscribe();

        fail_stalled_turn(&app, &f.run_id, &f.bot_id, &f.turn_id, false).await.unwrap();
        let message: (String, String, String) = sqlx::query_as(
            "SELECT role, content, source FROM messages WHERE conversation_id=? AND turn_id=? AND role='system'",
        )
        .bind(&f.conversation_id)
        .bind(&f.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(message.0, "system");
        assert!(message.1.contains("not logged in"));
        assert_eq!(message.2, "system");
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        let kinds = event_kinds(rx).await;
        assert_eq!(kinds, vec!["message_added", "turn_updated"]);
    }
}

#[cfg(test)]
mod progress_rate_tests {
    use super::{progress_due, PROGRESS_MIN_GAP};
    use std::time::{Duration, Instant};

    #[test]
    fn the_gap_is_the_documented_four_frames_a_second() {
        assert_eq!(PROGRESS_MIN_GAP * 4, Duration::from_secs(1));
    }

    #[test]
    fn a_run_that_has_not_emitted_yet_goes_out_at_once() {
        assert!(progress_due(None, false));
    }

    #[test]
    fn a_frame_inside_the_window_is_held_back() {
        assert!(!progress_due(Some(&Instant::now()), false));
    }

    #[test]
    fn the_window_opens_again_once_the_gap_has_passed() {
        let last = Instant::now() - PROGRESS_MIN_GAP - Duration::from_millis(1);
        assert!(progress_due(Some(&last), false));
    }

    /// The poller's last frame must not stay in `pending` when the turn ends inside the window (`force`).
    #[test]
    fn force_ignores_the_budget() {
        assert!(progress_due(Some(&Instant::now()), true));
    }
}

#[cfg(test)]
mod remote_hook_tests {
    //! SPEC §11.4.2 — runs `REMOTE_HOOK_SH` with `/bin/sh` against a fake `$HOME` and `herdr`.
    //! Classification stays coarse; the real one is `hookrecv::classify`.
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    struct Sandbox {
        dir: std::path::PathBuf,
        bot: String,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn write_exec(path: &std::path::Path, body: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        drop(f);
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    impl Sandbox {
        /// `with_herdr = false` is the §11.4.2 "no herdr on this host" path (acceptance H5).
        fn new(with_herdr: bool) -> Self {
            let dir = std::env::temp_dir().join(format!("am-hook-{}", crate::db::ulid()));
            std::fs::create_dir_all(&dir).unwrap();
            write_exec(&dir.join("hook.sh"), super::REMOTE_HOOK_SH);
            if with_herdr {
                // Records one invocation per line so `--seq` ordering stays observable.
                write_exec(
                    &dir.join("herdr"),
                    "#!/bin/sh\n{ for a in \"$@\"; do printf '%s ' \"$a\"; done; printf '\\n'; } >> \"$AM_TEST_LOG\"\n",
                );
            }
            Sandbox { dir, bot: "b-test".into() }
        }

        fn bot_dir(&self) -> std::path::PathBuf {
            self.dir.join(".config/agents-manager/bots").join(&self.bot)
        }

        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.bot_dir().join(name)).unwrap_or_default()
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("herdr.log"))
                .unwrap_or_default()
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        }

        /// Runs `hook.sh <argv…>` with `stdin`, answering `(stdout, exit_ok)`.
        fn run(&self, argv: &[&str], stdin: &str) -> (String, bool) {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg(self.dir.join("hook.sh"));
            cmd.args(argv);
            cmd.env_clear();
            cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            cmd.env("HOME", &self.dir);
            cmd.env("AM_TEST_LOG", self.dir.join("herdr.log"));
            cmd.env("HERDR_PANE_ID", "p1");
            cmd.env("HERDR_SESSION", "am-test");
            let herdr = self.dir.join("herdr");
            if herdr.exists() {
                cmd.env("AM_REAL_HERDR", &herdr);
            }
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut ch = cmd.spawn().unwrap();
            ch.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
            let out = ch.wait_with_output().unwrap();
            (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
        }
    }

    const STOP: &str = r#"{"hook_event_name":"Stop","session_id":"s-1","transcript_path":"/tmp/t.jsonl","stop_hook_active":false}"#;

    #[test]
    fn claude_stop_spools_then_reports_idle() {
        let sb = Sandbox::new(true);
        let (out, ok) = sb.run(&["claude", &sb.bot, "tok"], STOP);
        assert!(ok, "the hook must always exit 0");
        assert_eq!(out, "", "the hook must always keep stdout empty (§4.4)");
        let spool = sb.read("hook-spool.jsonl");
        assert_eq!(spool.lines().count(), 1);
        assert!(spool.contains(r#""bot_id":"b-test""#) && spool.contains(r#""provider":"claude""#));
        assert!(spool.contains(r#""hook_event_name":"Stop""#));
        let calls = sb.calls();
        assert_eq!(calls.len(), 1, "one report per turn end: {calls:?}");
        let c = &calls[0];
        assert!(c.contains("--session am-test "), "{c}");
        assert!(c.contains("pane report-agent p1 "), "{c}");
        assert!(c.contains("--source agents-manager:b-test "), "{c}");
        assert!(c.contains("--agent claude "), "{c}");
        assert!(c.contains("--state idle "), "{c}");
        assert!(c.contains("--agent-session-id s-1"), "{c}");
        assert!(c.contains("--agent-session-path /tmp/t.jsonl"), "{c}");
        // `--message` costs a fork and never reaches the subscriber (§11.4.1).
        assert!(!c.contains("--message"), "{c}");
    }

    #[test]
    fn nested_stop_and_grok_shutdown_spool_without_reporting() {
        let sb = Sandbox::new(true);
        sb.run(&["claude", &sb.bot, "tok"], r#"{"hook_event_name":"Stop","stop_hook_active":true}"#);
        sb.run(&["grok", &sb.bot, "tok"], r#"{"hookEventName":"stop","reason":"shutdown"}"#);
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 2, "both still spool");
        assert!(sb.calls().is_empty(), "neither is a turn ending: {:?}", sb.calls());
    }

    #[test]
    fn grok_end_turn_reports_idle() {
        let sb = Sandbox::new(true);
        sb.run(&["grok", &sb.bot, "tok"], r#"{"hookEventName":"stop","reason":"end_turn","sessionId":"g-9"}"#);
        let calls = sb.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("--agent grok ") && calls[0].contains("--state idle "), "{:?}", calls);
        assert!(calls[0].contains("--agent-session-id g-9"), "{:?}", calls);
    }

    /// Token slot `-` (review 2026-09-12 #8) is treated like a real token: spool, then report.
    #[test]
    fn a_placeholder_token_slot_changes_nothing() {
        let sb = Sandbox::new(true);
        let (out, ok) = sb.run(&["claude", &sb.bot, super::REMOTE_TOKEN_SLOT], STOP);
        assert!(ok && out.is_empty());
        let spool = sb.read("hook-spool.jsonl");
        assert_eq!(spool.lines().count(), 1);
        assert!(spool.contains(r#""bot_id":"b-test""#), "{spool}");
        assert!(!spool.contains("tok"), "no token, real or placeholder, is written anywhere: {spool}");
        assert_eq!(sb.calls().len(), 1);
        let payload = r#"{"type":"agent-turn-complete","thread-id":"c-4"}"#;
        sb.run(&["codex", &sb.bot, super::REMOTE_TOKEN_SLOT, payload], "");
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 2);
        assert!(sb.calls()[1].contains("--agent-session-id c-4"), "{:?}", sb.calls());
    }

    #[test]
    fn codex_takes_the_payload_from_argv() {
        let sb = Sandbox::new(true);
        let payload = r#"{"type":"agent-turn-complete","thread-id":"c-3"}"#;
        sb.run(&["codex", &sb.bot, "tok", payload], "");
        assert!(sb.read("hook-spool.jsonl").contains("agent-turn-complete"));
        let calls = sb.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("--state idle ") && calls[0].contains("--agent-session-id c-3"), "{calls:?}");
    }

    #[test]
    fn session_start_only_reports_the_session() {
        let sb = Sandbox::new(true);
        sb.run(&["claude", &sb.bot, "tok"], r#"{"hook_event_name":"SessionStart","session_id":"s-2"}"#);
        let calls = sb.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("pane report-agent-session p1 "), "{calls:?}");
        assert!(!calls[0].contains("--state"), "a session start says nothing about the state: {calls:?}");
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 1);
    }

    #[test]
    fn without_herdr_it_spools_and_says_so() {
        let sb = Sandbox::new(false);
        let (out, ok) = sb.run(&["claude", &sb.bot, "tok"], STOP);
        assert!(ok);
        assert_eq!(out, "");
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 1);
        assert!(sb.read("hook.log").contains("herdr not found; spooled only"));
    }

    #[test]
    fn statusline_overwrites_a_single_slot_and_runs_the_user_command() {
        let sb = Sandbox::new(true);
        let cfg = sb.dir.join(".claude");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("settings.json"),
            r#"{"statusLine":{"type":"command","command":"printf USERLINE"}}"#,
        )
        .unwrap();
        sb.run(&["statusline", &sb.bot, "tok"], r#"{"cost":{"a":1}}"#);
        let (out, ok) = sb.run(&["statusline", &sb.bot, "tok"], r#"{"cost":{"b":2}}"#);
        assert!(ok);
        assert_eq!(out, "USERLINE", "stdout is the user's own status line, nothing else");
        let slot = sb.read("hook-status.json");
        assert_eq!(slot.lines().count(), 1, "single slot, not a queue: {slot}");
        assert!(slot.starts_with(r#"{"hook_event_name":"StatusLine","cost":{"b":2}}"#), "{slot}");
        assert_eq!(sb.read("hook-spool.jsonl"), "", "the status line never enters the spool (§11.4.5)");
        assert!(sb.calls().is_empty(), "the status line reports no state");
    }

    #[test]
    fn seq_is_strictly_increasing() {
        let sb = Sandbox::new(true);
        sb.run(&["claude", &sb.bot, "tok"], STOP);
        sb.run(&["claude", &sb.bot, "tok"], STOP);
        let seqs: Vec<u128> = sb
            .calls()
            .iter()
            .map(|c| {
                c.split(" --seq ").nth(1).unwrap().split_whitespace().next().unwrap().parse::<u128>().unwrap()
            })
            .collect();
        assert_eq!(seqs.len(), 2, "{:?}", sb.calls());
        assert!(seqs[1] > seqs[0], "herdr drops a seq that did not grow: {seqs:?}");
    }

    #[test]
    fn without_a_pane_id_it_only_spools() {
        let sb = Sandbox::new(true);
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(sb.dir.join("hook.sh")).args(["claude", &sb.bot, "tok"]);
        cmd.env_clear();
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.env("HOME", &sb.dir);
        cmd.env("AM_TEST_LOG", sb.dir.join("herdr.log"));
        cmd.env("AM_REAL_HERDR", sb.dir.join("herdr"));
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut ch = cmd.spawn().unwrap();
        ch.stdin.take().unwrap().write_all(STOP.as_bytes()).unwrap();
        assert!(ch.wait().unwrap().success());
        assert_eq!(sb.read("hook-spool.jsonl").lines().count(), 1);
        assert!(sb.calls().is_empty(), "no pane to report against");
    }
}
