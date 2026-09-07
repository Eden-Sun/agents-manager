//! Bot / Run lifecycle: start, stop, interrupt, prompt, keys, terminal fallback (SPEC §6.2–§6.4, §4.3).
//!
//! Every public entry point takes the per-bot lock.

use crate::config::LOCAL_HOST;
use crate::db;
use crate::herdr::{AgentStatus, HerdrClient, HerdrError};
use crate::hosts::{sh_quote, HostConn};
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
    /// A 400 whose body is machine-readable rather than a message, e.g. SPEC-team §10.1's
    /// `{"error":"quota_low","kind":"claude","used_pct":93}`.
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

/// herdr's "the pane exists but its shell is not ready for an agent yet" answer.
///
/// It is a timing answer, not a failure: the shell settles a few hundred milliseconds later.
/// Matched on the error code first, with the message as a fallback because the same condition
/// reaches some call sites already flattened into a string.
fn pane_not_ready(e: &anyhow::Error) -> bool {
    if let Some(h) = e.downcast_ref::<HerdrError>() {
        if h.code == "agent_pane_busy" || h.message.contains("not an available shell") {
            return true;
        }
    }
    let s = e.to_string();
    s.contains("agent_pane_busy") || s.contains("not an available shell")
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
    insert_message_grouped(app, conversation_id, turn_id, role, content, source, incomplete, snapshot, None).await
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
    let id = db::ulid();
    let now = db::now();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, incomplete, terminal_snapshot, group_id, created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
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
        // SPEC-team §3: the same transition on the internal bus. Every path that takes a
        // turn out of `in_flight` (hook match, terminal fallback, watchdog, stop, interrupt)
        // funnels through here, so team schedulers only need this one subscription.
        app.publish_turn(crate::state::TurnEvent {
            bot_id: bot_id.clone(),
            turn_id: t.id.clone(),
            status: t.status.clone(),
            delivery: t.delivery.clone(),
            team_id: t.team_id.clone(),
            team_event_id: t.team_event_id.clone(),
        });
        // The queue is daemon-owned. Schedule after publishing so the next prompt cannot race
        // the completion event, and let the per-bot lock serialize it with hooks / status events.
        if should_flush_queue {
            schedule_flush_queued(app, &bot_id);
        }
    }
}

/// Hand the oldest queued prompt to the agent, if it can take one right now.
///
/// The caller already holds the bot lock, which is what makes the `queued -> in_flight`
/// promotion safe: `turns_one_in_flight` and `turns_one_queued` are both unique indexes, so
/// losing a race here would be an error rather than a no-op. Every early return leaves the
/// turn queued for the next transition to retry — the queue is durable, so "not now" is
/// always a safe answer. That holds *after* the claim as well: anything that gives up
/// between `queued -> in_flight` and the `agent.prompt` RPC calls `requeue_turn`, because a
/// turn left `in_flight` with `delivery='pending'` has no other way out.
async fn flush_queued_locked(app: &Arc<App>, bot_id: &str) -> anyhow::Result<()> {
    let Ok(conv) = db::conversation_id(&app.db, bot_id).await else { return Ok(()) };
    let Some(turn) = db::queued_turn(&app.db, &conv).await? else { return Ok(()) };
    // One turn at a time, per SPEC §2: a queued prompt waits for the previous one to finish.
    let Some(run) = db::active_run(&app.db, bot_id).await? else { return Ok(()) };
    if run.state != "running" || run.agent_status == "blocked" {
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

    // Everything between the claim and the RPC must put the turn *back* on the queue if it
    // gives up. Nothing else in the daemon finishes a turn that is `in_flight` with
    // `delivery='pending'`: `arm_stall`, `arm_progress` and `try_fallback` all require
    // `delivery == "ok"`, so a turn abandoned here would sit in flight until the run ended
    // — blocking every later `prompt()` with 409 "a turn is already in flight" — and this is
    // a background task, so the user would never see why.
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
            // Deliberately *not* requeued: the RPC was sent and we do not know whether the
            // agent took it, so putting the same text back on the queue could deliver it
            // twice. `delivery='unknown'` is the designed, user-visible parking state —
            // `prompt()` refuses the next prompt with "abandon it first" and names this turn.
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

/// Undo a `queued -> in_flight` claim that never became a delivery.
///
/// Only ever called while the bot lock is held and only for a turn this flush claimed
/// itself, so the `turns_one_queued` unique index cannot be violated: the row going back is
/// the very one that was taken out of the queue a moment ago, and nothing else can have
/// queued behind it in between. `run_id` goes back to NULL too — a turn that was never
/// delivered does not belong to that run.
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

/// Wake the durable prompt queue after a turn or Run transition. The task deliberately does
/// nothing in tests; tests drive the DB state machine directly and must not race a background
/// RPC attempt.
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

/// SPEC appendix E — the POSIX sh + curl hook installed on remote hosts (no daemon binary there).
pub const REMOTE_HOOK_SH: &str = r#"#!/bin/sh
PROVIDER="$1"; BOT="$2"; TOKEN="$3"; PORT="$4"; shift 4
LIMIT=1048576
# v4.0 statusLine mode: POST the rate limits as a `StatusLine` claude event (fire-and-forget,
# never spooled), then run the user's own statusLine command on the same input so the pane
# shows exactly what it would without the daemon. Budget: ~2 s, always exit 0.
if [ "$PROVIDER" = "statusline" ]; then
  INPUT=$(head -c $LIMIT)
  case "$INPUT" in
    '{}'|'') ;;
    '{'*)
      NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
      PAYLOAD='{"hook_event_name":"StatusLine",'"${INPUT#\{}"
      BODY=$(printf '{"bot_id":"%s","provider":"claude","payload":%s,"received_at":"%s","truncated":false}' "$BOT" "$PAYLOAD" "$NOW")
      ( NO_PROXY=127.0.0.1 curl -s -m 2 --connect-timeout 0.3 -o /dev/null -X POST "http://127.0.0.1:$PORT/hook/claude" \
          -H 'Content-Type: application/json' -H "X-AM-Bot-Token: $TOKEN" --data-binary "$BODY" >/dev/null 2>&1 & ) ;;
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
DIR="$HOME/.config/agents-manager/bots/$BOT"
mkdir -p "$DIR"
OUT=$(NO_PROXY=127.0.0.1 curl -s -m 2 --connect-timeout 0.3 -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/hook/$PROVIDER" \
  -H 'Content-Type: application/json' -H "X-AM-Bot-Token: $TOKEN" --data-binary "$BODY" 2>>"$DIR/hook.log") || OUT=fail
# 4xx means the daemon will never accept this line (deleted bot / bad token): do not spool it.
case "$OUT" in 2*) ;; 4*) printf '%s rejected %s\n' "$NOW" "$OUT" >> "$DIR/hook.log";; *) printf '%s\n' "$BODY" >> "$DIR/hook-spool.jsonl";; esac
exit 0
"#;

// ---------------------------------------------------------------- grok (SPEC §12)
//
// grok 1.0.13 has no per-launch hook flag (`--settings` / `--hooks` / `--plugin-dir` are all
// rejected by the TUI), so the daemon installs ONE global, always-trusted hook file
// `<GROK_HOME>/hooks/agents-manager.json` whose Stop / SessionStart entries run a static
// dispatcher `~/.config/agents-manager/grok-hook.sh`. The dispatcher reads the pane env
// (`AM_BOT_ID`, `AM_HOOK_TOKEN`, `AM_PORT`) to decide which bot to report to, and exits 0
// immediately when those are unset, so the user's own grok sessions are unaffected.

/// File name inside `<GROK_HOME>/hooks/`.
pub const GROK_HOOKS_FILE: &str = "agents-manager.json";
/// Dispatcher file name inside `~/.config/agents-manager/`.
pub const GROK_DISPATCH_SH: &str = "grok-hook.sh";

/// Dispatcher installed on remote hosts: forwards to the per-bot `hook.sh` (SPEC §11.4).
pub const REMOTE_GROK_DISPATCH_SH: &str = r#"#!/bin/sh
# agents-manager grok dispatcher (SPEC §12). Installed by the daemon; no-op outside daemon panes.
[ -n "$AM_BOT_ID" ] && [ -n "$AM_HOOK_TOKEN" ] || exit 0
H="$HOME/.config/agents-manager/bots/$AM_BOT_ID/hook.sh"
[ -x "$H" ] || exit 0
exec "$H" grok "$AM_BOT_ID" "$AM_HOOK_TOKEN" "${AM_PORT:-7788}"
"#;

/// Local dispatcher: runs the daemon binary's `hook grok` subcommand.
fn local_grok_dispatch_sh(exe: &str) -> String {
    format!(
        "#!/bin/sh\n# agents-manager grok dispatcher (SPEC §12). Rewritten by the daemon on every grok bot start; no-op outside daemon panes.\n[ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0\nexec {exe} hook grok --bot \"$AM_BOT_ID\" --token \"$AM_HOOK_TOKEN\" --port \"${{AM_PORT:-7788}}\"\n",
        exe = sh_quote(exe)
    )
}

/// The hooks file (grok's JSON hook-file schema, same shape as Claude's `hooks` object).
fn grok_hooks_json(dispatcher: &str) -> String {
    let entry = json!([{"hooks": [{"type": "command", "command": dispatcher, "timeout": 5}]}]);
    serde_json::to_string_pretty(&json!({"hooks": {"SessionStart": entry, "Stop": entry}})).unwrap_or_default()
}

/// `GROK_HOME` from the resolved pane env, else `<home>/.grok`.
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

/// Local grok bot: install the dispatcher + the global hooks file. Idempotent.
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

/// Absolute remote paths for a bot's hook material.
pub struct RemoteHookPaths {
    pub dir: String,
    pub hook_sh: String,
    pub settings: String,
}

pub async fn remote_bot_dir(conn: &HostConn, bot_id: &str) -> anyhow::Result<RemoteHookPaths> {
    let home = conn.home().await?;
    let dir = format!("{home}/.config/agents-manager/bots/{bot_id}");
    Ok(RemoteHookPaths { hook_sh: format!("{dir}/hook.sh"), settings: format!("{dir}/claude-settings.json"), dir })
}

/// SPEC §11.4 — push `hook.sh` (+ `claude-settings.json`) to the remote before `agent.start`.
async fn install_remote_hook(
    conn: &HostConn,
    bot: &db::Bot,
    hook_port: u16,
) -> anyhow::Result<RemoteHookPaths> {
    let p = remote_bot_dir(conn, &bot.id).await?;
    let cmd = shell_join(&[
        p.hook_sh.clone(),
        "claude".into(),
        bot.id.clone(),
        bot.hook_token.clone(),
        hook_port.to_string(),
    ]);
    // v4.0: `hook.sh statusline <bot> <token> <port>` POSTs the rate limits, then execs the
    // user's own statusLine command (read from the remote ~/.claude/settings.json).
    let statusline = shell_join(&[
        p.hook_sh.clone(),
        "statusline".into(),
        bot.id.clone(),
        bot.hook_token.clone(),
        hook_port.to_string(),
    ]);
    let settings = json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": cmd}]}],
            "Stop": [{"hooks": [{"type": "command", "command": cmd}]}]
        },
        "statusLine": {"type": "command", "command": statusline}
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

/// Returns the daemon-injected CLI args that go *before* the bot's own args.
/// For a remote project this also uploads the hook script over ssh (SPEC §11.4).
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

    // ---- remote project: POSIX sh hook over the reverse tunnel
    if project.host != LOCAL_HOST {
        let conn = app
            .hosts
            .get(&project.host)
            .await
            .ok_or_else(|| anyhow::anyhow!("unknown host `{}`", project.host))?;
        let hook_port = conn.hook_port(app.port);
        let paths = install_remote_hook(&conn, bot, hook_port).await?;
        let hook_args: Vec<String> = match bot.kind.as_str() {
            "claude" => vec!["--settings".into(), paths.settings],
            "codex" => {
                let parts = vec![
                    paths.hook_sh,
                    "codex".to_string(),
                    bot.id.clone(),
                    bot.hook_token.clone(),
                    hook_port.to_string(),
                ];
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

    // ---- local project: the daemon binary is right here
    let dir = app.bot_dir(&bot.id);
    std::fs::create_dir_all(&dir)?;
    let hook_args: Vec<String> = match bot.kind.as_str() {
        "claude" => {
            let cmd = shell_join(&hook_cmd_parts(app, bot, "claude"));
            // v4.0: the status line reports rate limits (`StatusLine` hook event) and then
            // runs the user's own statusLine command so the pane looks unchanged.
            let mut sl = hook_cmd_parts(app, bot, "claude");
            sl[1] = "statusline".into();
            sl.remove(2);
            let statusline = shell_join(&sl);
            // v3: no Notification hook; Stop with stop_hook_active=true is ignored daemon-side.
            let settings = json!({
                "hooks": {
                    "SessionStart": [{"hooks": [{"type": "command", "command": cmd}]}],
                    "Stop": [{"hooks": [{"type": "command", "command": cmd}]}]
                },
                "statusLine": {"type": "command", "command": statusline}
            });
            let path = dir.join("claude-settings.json");
            std::fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
            vec!["--settings".into(), path.to_string_lossy().to_string()]
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

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| if p.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=".contains(c)) { p.clone() } else { format!("'{}'", p.replace('\'', "'\\''")) })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pane env = daemon-injected ∪ identity.env ∪ bot.env (later wins). `$HOME` / `~` in the
/// identity's and bot's values expand against *that host's* home.
async fn pane_env(app: &Arc<App>, bot: &db::Bot, host: &str, run_id: &str, hook_port: u16) -> Value {
    let mut env = serde_json::Map::new();
    env.insert("AM_BOT_ID".into(), json!(bot.id));
    // diagnostics only; hook identity is per-bot
    env.insert("AM_RUN_ID".into(), json!(run_id));
    // On a remote host this is the reverse-forwarded port, not the daemon's own.
    env.insert("AM_PORT".into(), json!(hook_port.to_string()));
    // SPEC §12: grok's hook is a global dispatcher that can only learn the bot from the pane
    // env, so the token rides along too (claude / codex still get it on the command line).
    // Omitting it is how `inject_hooks = false` is honoured for grok: the dispatcher exits 0.
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

    // Identities are per host (SPEC §16): `[[identities]]` plus the `ccN` aliases discovered
    // on *this* machine, so `cc1` picks up the config dir that machine's shell means by it.
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

/// Quote `s` as a TOML basic string (for codex `-c key="…"` overrides): `\`, `"`, newlines,
/// tabs and other control characters are escaped.
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

/// The one rule every daemon-started agent carries, ahead of `bot.persona`: a child pane it
/// opens through herdr must be named `<its own agent name>-<suffix>`, which is what lets the
/// reconcile file that child under this bot (`managed_by='child'`) instead of losing it.
pub fn spawn_rule(agent_name: &str) -> String {
    format!(
        "你在 agents-manager 裡的 agent 名稱是 `{agent_name}`。\
         若要用 herdr 開子 pane / 子 agent（`herdr agent start <名稱> …`），名稱**必須**以 `{agent_name}-` 為前綴\
         （例：`{agent_name}-review`、`{agent_name}-ui`），管理器才會把它掛在你底下追蹤；不照做的子 agent 不會被管理。"
    )
}

/// v4.0: `bot.persona` appended to the agent's system prompt, per kind. Sits right after the
/// daemon's own flags (before model / effort). The daemon's own rule (`spawn_rule`) comes
/// first; the user's text follows it.
fn persona_args(bot: &db::Bot, agent_name: &str) -> Vec<String> {
    let user = bot.persona.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let p = match user {
        Some(u) => format!("{}\n\n{u}", spawn_rule(agent_name)),
        None => spawn_rule(agent_name),
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

/// `bot.model` as CLI args, inserted between the daemon's own flags and `identity.args`.
/// Drop `bot.effort` when the chosen model does not accept it.
///
/// The levels are **per model** (`model/list` reports `supportedReasoningEfforts`), so an effort
/// left over from a previous model is rejected outright: codex answers `-c
/// model_reasoning_effort="max"` on `gpt-5.5` with `400 unsupported_value … Supported values are
/// 'none', 'low', 'medium', 'high', and 'xhigh'`, and every turn of that Run fails. The UI
/// already prevents the pairing, but the stored value can predate that, be edited by hand, or
/// come from `config.toml`.
///
/// Silence is deliberate on every uncertain path — an unreachable CLI, an unknown model, an
/// empty list — because dropping a *valid* effort would quietly downgrade the agent. Only a
/// model we can see, whose list we can read, and which does not contain this value, is filtered.
async fn effort_checked(app: &Arc<App>, bot: &db::Bot, host: &str) -> db::Bot {
    let Some(effort) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { return bot.clone() };
    let Some(model) = bot.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { return bot.clone() };
    let Ok(list) = crate::models::list(app, host, &bot.kind, false).await else { return bot.clone() };
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
    // v4.0: codex Fast tier, same key the user's config.toml uses.
    if bot.fast != 0 && bot.kind == "codex" {
        out.extend(["-c".to_string(), "service_tier=\"priority\"".to_string()]);
    }
    out
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
            team_id: None,
            team_role: None,
            cwd: None,
            herdr_session: None,
            parent_bot_id: None,
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
        let a = model_args(&bot("codex", None, None, false));
        assert!(a.is_empty());
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

    #[test]
    fn persona_per_kind() {
        use super::{persona_args, spawn_rule, toml_basic_string};
        let mut b = bot("claude", None, None, false);
        let rule = spawn_rule("proj-abc123");
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
    }
}

/// Extra CLI args contributed by the bot's identity on `host`. Discovered `ccN` identities
/// carry no args by design — the flags in the alias are the user's shell habit, not ours.
async fn identity_args(app: &Arc<App>, bot: &db::Bot, host: &str) -> Vec<String> {
    let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) else { return vec![] };
    crate::tools::identity_for_host(app, host, idn).await.map(|i| i.args).unwrap_or_default()
}

async fn client_for_run(app: &Arc<App>, run: &db::Run) -> LcResult<HerdrClient> {
    app.herdr_for_run(run)
        .await
        .ok_or_else(|| LcError::Upstream(format!("no Herdr session is available for run `{}`", run.id)))
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
    let session = app
        .session_for_bot(&bot, &project.host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{}` is not configured", project.host)))?;

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

/// SPEC-team §2.2: the directory a bot's pane starts in. `bots.cwd` when set (a team member
/// lives in its own worktree), otherwise the project's path — which is what every ordinary
/// bot has, so this is a no-op for them.
pub fn bot_cwd<'a>(bot: &'a db::Bot, project: &'a db::Project) -> &'a str {
    match bot.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => c,
        None => project.path.as_str(),
    }
}

/// What the bot's tab is called on herdr's tab bar. The bot's own nickname, so the user can
/// tell which tab is which — a label herdr would otherwise number `1`, `2`, `3`.
pub fn tab_label(bot: &db::Bot) -> String {
    let n = bot.name.trim();
    if n.is_empty() {
        "bot".to_string()
    } else {
        n.to_string()
    }
}

/// Close `tab_id` when nothing is left in it.
///
/// The one place that decides "is this tab now empty?", shared by every caller that takes a
/// pane out of a tab: stopping a run, cleaning up after a failed start, and moving a pane to
/// a tab of its own. Deliberately quiet and idempotent — herdr reaps a tab whose last pane
/// closes, and `pane.move` reports the emptied tab it closed itself, so finding the tab
/// already gone is the *expected* outcome, not a failure worth surfacing.
///
/// A tab that still holds panes is left alone: that is a run from before the one-bot-one-tab
/// change (split into a shared tab), or panes the user arranged by hand.
async fn close_tab_if_empty(client: &crate::herdr::HerdrClient, workspace_id: &str, tab_id: &str) {
    let tabs = match client.tab_list(workspace_id).await {
        Ok(t) => t,
        // Never guess when herdr cannot be asked: closing a tab we cannot see the contents
        // of could take a pane the user is working in with it.
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

/// Close a run's pane and, when that leaves its tab empty, the tab as well.
///
/// `tab_id` is `None` for runs started before one-bot-one-tab: their pane was split into a
/// tab it shares, so only the pane goes.
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

/// The pane a starting run gets: **one bot, one tab**.
///
/// This used to `pane.split` inside the project's single tab, so N bots meant N panes
/// carving up one screen width. Seven of them in a 185-column workspace left the narrowest
/// at 18 columns, and below roughly 31 an agent's TUI reflows to a few glyphs a row *without
/// writing the spaces* — the user cannot read it and the terminal fallback recovers only
/// fragments (`is_shredded`). Splitting the roomiest pane rather than the first slowed that
/// down without fixing it, because the width being divided never grew.
///
/// Tabs of one workspace do **not** share width, so `tab.create` is what actually scales:
/// five bots become five tabs of the full 185 columns each. It takes the same arguments as
/// `pane.split` and hands back the new tab's root pane, so everything downstream —
/// `agent.start`, the pane subscription, the run mapping — is unchanged.
///
/// `focus` is always false: starting a bot must not yank the user out of the tab they are
/// reading. `fresh_root` is the root pane of a workspace we *just* created, which is already
/// a tab of its own holding exactly one pane, so it is used as-is rather than doubled.
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
        // A team workspace (SPEC-team §6.4a) keeps its root pane: it is a plain shell sitting
        // at the team root, a useful place for the user to stand while watching, and
        // reclaiming it would mean re-homing a pane that is already running a shell.
        None => client.tab_create(workspace_id, cwd, label, env.clone()).await,
    }
}

async fn start_inner(app: &Arc<App>, bot: &db::Bot, project: &db::Project, run_id: &str) -> LcResult<()> {
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
    // 1b. preflight: the agent CLI must exist on that host, otherwise herdr would sit in
    //     `launch_pending` for the whole 60 s timeout with nothing to tell the user.
    if let Err(reason) = ensure_kind_installed(app, &host, &bot.kind).await {
        let conv = db::conversation_id(&app.db, &bot.id).await.map_err(up)?;
        let _ = insert_message(app, &conv, None, "system", &reason, "system", false, None).await;
        return Err(LcError::Bad(reason));
    }
    let hook_port = match app.hosts.get(&host).await {
        Some(c) => c.hook_port(app.port),
        None => app.port,
    };
    let env = pane_env(app, bot, &host, run_id, hook_port).await;

    // 2. workspace
    let mut fresh_root: Option<crate::herdr::PaneInfo> = None;
    // SPEC-team §6.4a: a team member's panes belong to the **team's** workspace, not the
    // project's. This is an extension of SPEC §2's "one workspace per project", not a breach
    // of it: the project's workspace is untouched, the team simply owns another one. The
    // team's workspace is created up front by `team::create`, so a member that cannot find
    // it fails to start rather than quietly filling the user's own workspace with four
    // throw-away panes — the reconcile then reports `workspace_missing`.
    let team_ws: Option<String> = match bot.team_id.as_deref() {
        Some(tid) if session.as_str() != "default" => {
            let t = db::team(&app.db, tid).await.map_err(up)?;
            let ws = t
                .and_then(|t| t.workspace_id)
                .filter(|w| !w.trim().is_empty())
                .ok_or_else(|| LcError::Upstream("this team has no workspace".into()))?;
            if client.workspace_get(&ws).await.map_err(up)?.is_none() {
                return Err(LcError::Upstream(format!("the team's workspace {ws} is gone")));
            }
            Some(ws)
        }
        _ => None,
    };
    // `projects.workspace_id` belongs to the manager's configured session. An imported bot
    // lives in the user's default session, so it must not overwrite that mapping or cause the
    // next named-session reconcile to clear it.
    let workspace_id = match team_ws {
        Some(ws) => ws,
        None => match (session.as_str() != "default", project.workspace_id.as_deref()) {
            (true, Some(ws)) if client.workspace_get(ws).await.map_err(up)?.is_some() => ws.to_string(),
            _ => {
                let (ws, root) =
                    client.workspace_create(&project.path, &project.label, env.clone()).await.map_err(up)?;
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
        },
    };

    // 3. pane
    //
    // SPEC-team §2.2: the pane's cwd is `bots.cwd` when the bot has one (a team member lives
    // in its own worktree), otherwise the project's path.
    //
    // For a **team member** that fallback is forbidden (§6.1 #1): the project path is the
    // user's own checkout, and an agent started there can commit whatever the user had in
    // progress. So a team member's cwd goes through `team::checked_member_cwd`, which
    // demands it be inside the team's worktree root, and a member that fails it never gets
    // a pane at all rather than getting the wrong one.
    let checked;
    let cwd = match bot.team_id.as_deref() {
        Some(tid) => {
            let t = db::team(&app.db, tid)
                .await
                .map_err(up)?
                .ok_or_else(|| LcError::Upstream("this bot's team is gone".into()))?;
            checked = crate::team::checked_member_cwd(&t, project, bot).map_err(LcError::Upstream)?;
            checked.as_str()
        }
        None => bot_cwd(bot, project),
    };
    // A directory the CLI has not seen before opens with "Is this a project you trust?", and
    // the cursor starts on *No, exit* — claude then quits and the start fails, while codex
    // sits at the prompt looking `idle` and silently eats the first message. Record the trust
    // first. Only local hosts, and only when the path is not already trusted, so in practice
    // this touches the user's config once per new directory (SPEC-team §7.4 does the same for
    // team worktrees, which are new by construction).
    if project.host == LOCAL_HOST {
        let mut b = bot.clone();
        b.cwd = Some(cwd.to_string());
        for w in crate::trust::pretrust_members(app, std::slice::from_ref(&b)).await {
            tracing::warn!(bot = %bot.name, cwd, warning = %w, "could not pre-trust the working directory");
        }
    }
    let root = acquire_run_pane(&client, &workspace_id, cwd, &tab_label(bot), &env, fresh_root).await.map_err(up)?;
    let pane_id = root.pane_id;
    let tab_id = root.tab_id;

    // 4. persist mapping + generate hook injection
    sqlx::query("UPDATE runs SET workspace_id = ?, pane_id = ?, tab_id = ? WHERE id = ?")
        .bind(&workspace_id)
        .bind(&pane_id)
        .bind(&tab_id)
        .bind(run_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    let injected = injected_args(app, bot, project, &env).await.map_err(up)?;
    // The agent name is decided here because the persona quotes it (see `spawn_rule`).
    let agent = crate::config::agent_name(&project.label, &bot.id);
    let mut args = injected;
    args.extend(persona_args(bot, &agent));
    args.extend(model_args(&effort_checked(app, bot, &project.host).await));
    args.extend(identity_args(app, bot, &project.host).await);
    args.extend(bot.args());

    // 5. agent.start (async on the socket) — under `<project>-<bot>`, recorded on the run
    sqlx::query("UPDATE runs SET agent_name = ? WHERE id = ?")
        .bind(&agent)
        .bind(run_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    // A freshly created pane is not an available shell the instant `tab.create` /
    // `pane.split` returns — herdr answers `agent_pane_busy: … is not an available shell`
    // until the interactive shell has settled. Observed 2026-09-06: starting a six-member
    // team started five agents and lost `dev-1` to exactly that, 300 ms in; the team then
    // paused on `member_lost` with nothing on screen to explain it. `quota_claude` already
    // retries this same herdr answer — the bot start path is the one that did not.
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
                close_pane_and_tab(&client, Some(&workspace_id), Some(&tab_id), &pane_id).await;
                return Err(up(e));
            }
        }
    }
    if !started {
        close_pane_and_tab(&client, Some(&workspace_id), Some(&tab_id), &pane_id).await;
        return Err(up(format!("pane {pane_id} never became an available shell")));
    }

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
    // Codex renders account notices as standalone TUI history rows rather than part of an
    // agent turn. They are not included in `notify`'s `last-assistant-message`, so take a
    // delayed pane snapshot once the startup screen has had time to render.
    if bot.kind == "codex" {
        schedule_codex_notice_capture(app, &bot.id, run_id);
    }
    app.emit_bot_status(&bot.id).await;
    Ok(())
}

/// Look the kind's executable up the way the pane will see it: through the user's *login*
/// shell (`$SHELL -lic`), falling back to the plain PATH. Returns a user-facing reason when
/// it is missing. Best effort — a lookup that itself fails (timeout, odd shell) passes.
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

// ---------------------------------------------------------------- Codex account notices

/// Codex draws this hint in the startup / idle transcript, outside any agent turn. It is not
/// part of `agent-turn-complete`, so terminal inspection is the only source available to us.
const CODEX_NOTICE_DELAY: Duration = Duration::from_millis(500);

/// Schedule a best-effort read after Codex has time to paint its startup or post-turn hint.
/// The task re-checks the run id so a delayed read from an old run cannot land on a new one.
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

/// Read and persist newly seen Codex account notices (reset available **or** hard limit hit).
/// The caller must hold the bot lock.
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
        let exists: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM messages
             WHERE conversation_id=? AND role='system' AND source='system' AND content=?)",
        )
        .bind(&conversation_id)
        .bind(&notice)
        .fetch_one(&app.db)
        .await?;
        if exists != 0 {
            continue;
        }
        insert_message(app, &conversation_id, None, "system", &notice, "system", false, Some(&read.text)).await?;
        tracing::info!(bot = %bot.name, notice = %notice, "codex account notice captured");
        if codex_limit_hit_line(&notice).is_some() {
            let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
            apply_codex_limit_hit_quota(app, &host, &notice).await;
            // Unlock the composer: a limit hit is a failed turn, not a silent idle.
            if let Some(turn) = db::in_flight_turn(&app.db, &run.id).await? {
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

// ---------------------------------------------------------------- stop / interrupt

pub async fn stop_bot(app: &Arc<App>, bot_id: &str) -> LcResult<bool> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
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
    // The pane belongs to this Run either way: once the agent is gone (or refused to go)
    // we close it, otherwise a bare shell pane would linger until the next reconcile. And
    // when the run owned its tab (started with `tab.create`, or moved into one) the tab goes
    // with it, so stopping bots does not leave a row of empty tabs behind.
    if let Some(p) = run.pane_id.as_deref() {
        close_pane_and_tab(&client, run.workspace_id.as_deref(), run.tab_id.as_deref(), p).await;
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
        if let Some(session) = app.session_for_run(&run).await {
            crate::events::unwatch_pane_on_session(app, &host, &session, p).await;
        }
    }
    app.emit_bot_status(bot_id).await;
    Ok(true)
}

/// stop (if running) + start. Used to make edited `model` / `args` / `identity` / `env` take effect.
pub async fn restart_bot(app: &Arc<App>, bot_id: &str) -> LcResult<String> {
    stop_bot(app, bot_id).await?;
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    start_bot_locked(app, bot_id).await
}

/// Remove a deleted bot's hook material (`~/.config/agents-manager/bots/<id>/`). Best effort:
/// a remote host that is down only gets a log line — the bot is gone either way.
pub async fn purge_bot_dir(app: &Arc<App>, bot_id: &str, host: &str) {
    if host == LOCAL_HOST {
        let dir = app.bot_dir(bot_id);
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

pub async fn interrupt_bot(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let target = db::run_target(&run, &bot);
    client_for_run(app, &run).await?.agent_send_keys(&target, &["esc".to_string()]).await.map_err(up)?;
    fail_in_flight(app, &run.id, "interrupted by user").await;
    Ok(())
}

/// Give a *running* bot a tab of its own — the retrofit for every bot started before
/// one-bot-one-tab, which is sitting in a pane split off a shared tab.
///
/// This is a move, not a restart. herdr keeps the `pane_id` across `pane.move` (verified
/// against 0.8.2), so the run's mapping, its pane subscription, the progress poller and any
/// turn in flight all carry on untouched; only `tab_id` changes. Nothing is sent to the
/// agent, so a bot mid-answer does not notice.
///
/// Idempotent: a pane that already owns its tab is left exactly where it is. Moving it again
/// would not be a no-op on herdr's side — it builds a *new* tab and closes the old one, which
/// renumbers the user's tab bar for nothing.
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

    // Where the pane actually is right now — `runs.tab_id` is NULL for every run started the
    // old way, and stale if the user dragged the pane about themselves.
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
        // Belt to two braces: the `solo` check above means we only ever move out of a tab
        // that still holds something, and herdr closes a tab its move emptied anyway. The
        // shared tidy-up runs regardless because it is idempotent, and because it is the one
        // place that decides a tab may go — a second opinion here would be a second bug.
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

/// Build the TUI slash command for a live setting, or `None` if this kind/field
/// has no in-session command (caller then reports `needs_restart`).
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

/// 有些設定不用重啟就能改：agent 的 TUI 自己有 slash 指令。
///
/// * grok `effort` → `/effort <level>`（grok 1.0.13 `04-slash-commands.md`）
/// * grok `model` → `/model <id>`；bot 同時有 effort 時帶第二參數（`/model grok-4.6 high`）
/// * claude `model` → `/model <alias>`（alias 同 `claude --model`：opus / sonnet / haiku / fable…）
/// * claude `effort` → `/effort <level>`（2.1.263 實測：`/effort low` 直接套用並回
///   `Set effort level to low (saved as your default for new sessions)`。不帶參數的 `/effort`
///   才是那條拉桿。**注意副作用**：claude 會把它存成該帳號之後新 session 的預設值，這是 CLI
///   的行為，只有 TUI 上按 `s` 才是「只有這次」——daemon 沒有那個選項。）
///
/// 回傳 `true` = 已經送進去（呼叫端就不用回 `needs_restart`）。做不到的一律 `false`
/// （kind 不符、沒在跑、正在忙、或清成「CLI 預設」——那個沒有對應的 slash 指令），
/// 讓呼叫端退回原本的「重啟才生效」。
pub async fn apply_live_setting(app: &Arc<App>, bot_id: &str, field: &str) -> bool {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let Ok(Some(bot)) = db::bot(&app.db, bot_id).await else { return false };
    let value = match field {
        "effort" => bot.effort.as_deref(),
        "model" => bot.model.as_deref(),
        _ => None,
    };
    // 清成「不指定」沒有 slash 指令可用（`/effort` 與 `/model` 都一定要帶值）。
    let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) else { return false };
    let Some(line) = live_slash_command(&bot.kind, field, value, bot.effort.as_deref()) else {
        return false;
    };
    let Ok(Some(run)) = db::active_run(&app.db, bot_id).await else { return false };
    let in_flight = !matches!(db::in_flight_turn(&app.db, &run.id).await, Ok(None));
    let Ok(pane_id) = slash_gate(&run, in_flight) else { return false };
    let Ok(client) = client_for_run(app, &run).await else { return false };
    if send_slash_line(&client, &pane_id, &line).await.is_err() {
        return false;
    }
    tracing::info!(bot_id, line, "applied live via slash command");
    true
}

/// 現在不能把一行 slash 指令送進 agent 輸入列的理由。
///
/// `apply_live_setting` 只需要知道「不行」（它會退回「重啟才生效」），但使用者自己按下
/// 「登入」時，靜默失敗就是個 bug ——所以理由要拿得出來。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashBlocked {
    /// 沒有正在跑的 run，或 run 已經不是 `running`。
    NotRunning,
    /// agent 自己在忙（`working` / `blocked`）：這時打的字會被吃掉，或掉進權限提示裡。
    AgentBusy,
    /// 有回合正在飛，這一行會變成那個 prompt 的一部分。
    TurnInFlight,
    /// run 沒有 pane 可以打字（舊資料，或 herdr 那邊已經收掉了）。
    NoPane,
}

impl SlashBlocked {
    /// 給 API body 用的機器可讀理由；文案由前端依這個 key 翻。
    pub fn reason(self) -> &'static str {
        match self {
            SlashBlocked::NotRunning => "not_running",
            SlashBlocked::AgentBusy => "agent_busy",
            SlashBlocked::TurnInFlight => "turn_in_flight",
            SlashBlocked::NoPane => "no_pane",
        }
    }
}

/// 這個 run 現在能不能被打字？能的話回它的 pane id。
///
/// 純函式，好讓每一條擋下來的理由都測得到——`apply_live_setting` 與 `login` 共用同一組
/// 判斷，兩邊就不會各自漂走。
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

/// 把一行 slash 指令打進 agent 的輸入列並送出。
///
/// 和 grok 額度探測同一套：先打字，等輸入列畫好，再送 Enter。`"/login\n"` 一次送會被
/// TUI 當成貼上多行，不會送出。
async fn send_slash_line(client: &HerdrClient, pane_id: &str, line: &str) -> LcResult<()> {
    client.pane_send_text(pane_id, line).await.map_err(up)?;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    client.pane_send_keys(pane_id, &["Enter"]).await.map_err(up)?;
    Ok(())
}

// ---------------------------------------------------------------- login

/// 這個 kind 在 TUI 裡登入 / 換帳號的 slash 指令，沒有就是 `None`。
///
/// 這是實際問過 CLI 的結果，不是猜的（每個都在拋棄式 herdr session 裡開起 TUI，打前綴看
/// 補完選單）：
///
/// * `claude` 2.1.263 → `/login`，選單說明「Sign in with your Anthropic account」。
/// * `grok` 1.0.13 → `/login`，選單說明「Log in or re-authenticate with your account」；
///   另見 `~/.grok/docs/user-guide/04-slash-commands.md` 的 “Account and Billing”。
/// * `codex` 0.153.4 → **沒有**。它的 slash 選單只有 `/logout`（`/logi` 完全沒有補完項），
///   登入得在 TUI 外面跑 `codex login`。所以這裡回 `None`：送一個不存在的 slash 指令進去，
///   codex 只會把 `/login` 當成一般 prompt 丟給模型，比報錯還糟。
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
    /// 實際送進 TUI 的那一行，讓前端／log 講得出送了什麼。
    pub command: String,
}

/// 對一個**正在跑的** bot 送登入指令，讓它的 TUI 進入登入 / 切換帳號流程。
///
/// 走的路和 `apply_live_setting` 一模一樣（同一個 gate、同一套打字節奏），差別只在錯誤語意：
/// 這裡是使用者明確按了按鈕，送不出去就要說明白為什麼，而不是靜悄悄地不做。
///
/// 送出後 agent 會停在登入畫面（通常還會開瀏覽器），在使用者完成之前它不能工作——daemon
/// 不去等、也不去替使用者完成，登入完成與否由 `POST /hosts/:name/tools/refresh` 重新偵測。
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

    /// 一個「可以打字」的 run；每個測試只動它想證明的那一格。
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
            native_session_id: None,
            transcript_path: None,
            last_read_revision: None,
            last_read_tail_hash: None,
            started_at: "2026-01-01T00:00:00Z".into(),
            ended_at: None,
        }
    }

    /// claude 與 grok 的 TUI 都有 `/login`；codex 沒有（只有 `/logout`），所以它得是 `None`
    /// ——回一個「不支援」比送一個不存在的指令進去好。
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

// ---------------------------------------------------------------- prompt

#[derive(serde::Serialize)]
pub struct PromptOut {
    pub turn_id: String,
    pub message_id: String,
    pub delivery: String,
}

pub async fn prompt(app: &Arc<App>, bot_id: &str, text: &str, client_request_id: &str) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, &[]).await
}

/// `prompt` carrying images dropped into the composer (`attach.rs`). The agent gets the
/// text plus their paths on its own host; the timeline keeps the text and renders the
/// thumbnails from `messages.attachments_json`.
pub async fn prompt_with(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    attachment_ids: &[String],
) -> LcResult<PromptOut> {
    prompt_grouped(app, bot_id, text, client_request_id, None, None, attachment_ids).await
}

/// `prompt` whose user message carries a SPEC §13 `group_id` (project group chat).
/// `deliver` is what the agent actually receives; `text` is what the timeline shows. The
/// group chat passes the mention-stripped variant so a bot never sees `@all`.
pub async fn prompt_grouped(
    app: &Arc<App>,
    bot_id: &str,
    text: &str,
    client_request_id: &str,
    group_id: Option<&str>,
    deliver: Option<&str>,
    attachment_ids: &[String],
) -> LcResult<PromptOut> {
    let deliver = deliver.unwrap_or(text);
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;

    if client_request_id.trim().is_empty() {
        return Err(LcError::Bad("client_request_id must not be empty".into()));
    }
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let conv = db::conversation_id(&app.db, bot_id).await.map_err(up)?;
    // Resolved before anything is written, so an unknown id is a plain 400 rather than a
    // turn that exists but was never delivered.
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
        "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, group_id, created_at) VALUES (?,?,?,'user',?,'web',?,?)",
    )
    .bind(&msg_id)
    .bind(&conv)
    .bind(&turn_id)
    .bind(text)
    .bind(group_id)
    .bind(db::now())
    .execute(&mut *tx)
    .await
    .map_err(up)?;
    tx.commit().await.map_err(up)?;
    crate::attach::bind(app, &msg_id, &files).await.map_err(up)?;

    if let Ok(Some(m)) = sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE id=?")
        .bind(&msg_id)
        .fetch_optional(&app.db)
        .await
    {
        app.emit("message_added", json!({"bot_id": bot_id, "message": m})).await;
    }
    emit_turn(app, &turn_id).await;

    // 4. deliver
    let res = client_for_run(app, &run)
        .await?
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

// ---------------------------------------------------------------- live progress (v3.9)

/// Consecutive unchanged polls at an empty composer before we stop trusting `agent_status`
/// and complete the Turn ourselves. ~14s: long enough that a merely slow hook still wins.
const IDLE_POLLS: u32 = 20;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(700);
const PROGRESS_MAX: Duration = Duration::from_secs(40 * 60);

/// While a turn is in flight, poll the pane and push the partial reply as `turn_progress`
/// so the UI can render it as it is being written. Stops by itself once the turn is no
/// longer `in_flight` (hook / fallback / watchdog / stop all end it).
pub async fn arm_progress(app: &Arc<App>, run_id: &str, bot_id: &str, turn_id: &str) {
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
        // What we sent, so the pane's echo of it can be stripped back off every frame.
        let sent = db::turn_user_messages(&app2.db, &turn_id).await.unwrap_or_default();
        let mut last = (String::new(), String::new(), String::new());
        let mut quiet = 0u32;
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
            // Same multi-line echo problem as the fallback path: strip our own prompt back off.
            let live = sent.iter().fold(live, |acc, p| strip_echoed_prompt(&acc, p));
            // The spinner row is dropped by `clean_screen`, so a pure thinking / tool phase
            // produces no frame at all and the UI sits on "waiting". Ship it separately.
            let activity = live_activity(&bot.kind, &read.text).unwrap_or_default();
            let alert = live_alert(&bot.kind, &read.text).unwrap_or_default();
            if live != last.0 || activity != last.1 || alert != last.2 {
                last = (live.clone(), activity.clone(), alert.clone());
                quiet = 0;
                app2.emit(
                    "turn_progress",
                    json!({"bot_id": bot_id, "run_id": run_id, "turn_id": turn_id, "text": live, "activity": activity, "alert": alert, "revision": read.revision}),
                )
                .await;
                continue;
            }
            // §4.3's fallback is armed by `working -> idle`, which is herdr's reading of the
            // pane. When that reading sticks (observed 2026-09-06: grok finished and sat at an
            // empty composer while herdr still reported `working`, its title spinner never
            // cleared) no hook and no fallback ever completes the Turn, and the UI waits for
            // ever. So also believe the pane directly: an empty composer plus nothing changing
            // is the agent telling us it wants input. `blocked` is excluded — SPEC §4.3 keeps
            // it out of the fallback because a modal is not an ended turn.
            let blocked = matches!(db::run(&app2.db, &run_id).await, Ok(Some(r)) if r.agent_status == "blocked");
            if blocked || !pane_awaits_input(&bot.kind, &read.text) {
                quiet = 0;
                continue;
            }
            quiet += 1;
            if quiet >= IDLE_POLLS {
                tracing::info!(turn = %turn_id, "pane idle at an empty prompt; completing via fallback");
                let _ = try_fallback(&app2, &run_id).await;
                break;
            }
        }
        app2.progress_pollers.lock().await.remove(&run_id);
    });
    pollers.insert(key, h);
}

// ------------------------------------------------- CLI-side (external) turn, live

/// The user typed straight into the tmux pane: open the `external` turn **now**.
///
/// Until this existed an external turn was only ever born at Stop-hook time, already
/// `completed` — so for the whole time the agent was working the web UI had no in-flight turn,
/// no `turn_progress` poller and therefore nothing to show; the exchange appeared in one lump
/// at the end. Opening the turn on the `-> working` edge puts a CLI prompt on exactly the same
/// footing as one the web sent: live bubble, activity row, streaming reply.
///
/// `delivery` is `ok` deliberately — the §4.3 terminal fallback ignores any other value, and
/// this turn needs that safety net just as much as a web one (it is what closes the turn if the
/// Stop hook never arrives).
pub async fn begin_external_turn(app: &Arc<App>, run: &db::Run) {
    let lock = app.bot_lock(&run.bot_id).await;
    let _g = lock.lock().await;
    // Re-read under the lock: `prompt()` may have opened its own turn since the status event,
    // and the pane watcher is armed *before* `start_bot` flips the run to `running` (step 6 vs
    // step 7), so a boot-time `-> working` blip must not be mistaken for the user typing.
    let Ok(Some(run)) = db::run(&app.db, &run.id).await else { return };
    if run.state != "running" {
        return;
    }
    if !matches!(db::in_flight_turn(&app.db, &run.id).await, Ok(None)) {
        return;
    }
    let Ok(conv) = db::conversation_id(&app.db, &run.bot_id).await else { return };
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
        // `turns_one_in_flight` is a unique index; losing that race just means someone else
        // opened the turn first, which is the outcome we wanted anyway.
        tracing::debug!(run = %run.id, error = %e, "external turn not opened");
        return;
    }
    tracing::info!(run = %run.id, turn = %tid, "external turn opened from pane activity");
    // The prompt echo on the pane is the text the user typed. If it is not on screen we open
    // the turn anyway (the live reply is still worth showing) rather than invent a message.
    if let Some(text) = pane_prompt_echo(app, &run).await {
        if let Err(e) = insert_message(app, &conv, Some(&tid), "user", &text, "hook", false, None).await {
            tracing::warn!(turn = %tid, error = ?e, "external prompt echo not stored");
        }
    }
    emit_turn(app, &tid).await;
    arm_progress(app, &run.id, &run.bot_id, &tid).await;
}

/// Read the pane and pull the last prompt echo off it. Same read as `arm_progress`.
async fn pane_prompt_echo(app: &Arc<App>, run: &db::Run) -> Option<String> {
    let bot = db::bot(&app.db, &run.bot_id).await.ok().flatten()?;
    let pane = run.pane_id.clone()?;
    let client = client_for_run(app, run).await.ok()?;
    let read = client.pane_read(&pane, "recent_unwrapped", 160).await.ok()?;
    last_prompt_echo_text(&bot.kind, &read.text)
}

/// Everything the agent has printed since the prompt echo, cleaned of TUI chrome, with the
/// reply markers (`⏺ ` / `• `) dropped so it reads like the final message will.
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

/// Longest activity string we forward; the pane can carry a whole wrapped status bar.
const ACTIVITY_MAX: usize = 120;

/// Is `s` an elapsed-time token — `18s`, `3m`, `1h`, `0.1s`?
fn is_elapsed_token(s: &str) -> bool {
    let Some(num) = s.strip_suffix('s').or_else(|| s.strip_suffix('m')).or_else(|| s.strip_suffix('h')) else {
        return false;
    };
    !num.is_empty() && num.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Is the agent sitting at an **empty** composer, i.e. waiting for input?
///
/// The marker alone on a row, once the box frame is stripped — grok draws its composer as
/// `│ ❯      │` inside `╭──╮ / ╰─ Grok ─╯`, so the bare `s == "❯"` test `clean_screen` uses
/// never matches there. Only the tail is searched because the composer is always at the bottom.
///
/// Note this is true while the agent is *working* too (claude keeps the empty composer on
/// screen under the spinner), so it is only meaningful together with "nothing changed".
fn pane_awaits_input(kind: &str, text: &str) -> bool {
    let Some(marker) = prompt_echo_prefix(kind).and_then(|p| p.trim_end().chars().next()) else { return false };
    text.lines().rev().take(12).any(|l| {
        let mut chars = l.chars().filter(|c| !"│┃╭╮╰╯─━ \t".contains(*c));
        chars.next() == Some(marker) && chars.next().is_none()
    })
}

/// Drop the tail of the user's own prompt from the head of what we scraped.
///
/// A multi-line prompt echoes as **one** `❯ <first line>` marker row followed by its remaining
/// lines verbatim, and `after_last_prompt_echo` only skips that marker row — so lines 2..n of
/// the user's own message come back as if the agent had written them. Matching them against
/// what we actually sent is exact, unlike guessing from indentation.
fn strip_echoed_prompt(text: &str, prompt: &str) -> String {
    let want: Vec<&str> = prompt.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if want.len() < 2 {
        // A one-line prompt leaves no tail for the line-wise match — but a narrow pane can
        // still have shredded that one line across many rows, which the squashed match sees.
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
            break;
        }
        i += 1;
        w += 1;
    }
    // Only strip once the whole tail matched. A partial match means the pane wrapped the text
    // (or the agent opened by quoting us), and eating half a real reply is worse than a echo.
    if w < want.len() {
        return strip_echoed_prompt_squashed(text, prompt).unwrap_or_else(|| text.to_string());
    }
    lines[i..].join("\n").trim().to_string()
}

/// Whitespace-insensitive fallback for [`strip_echoed_prompt`].
///
/// A very narrow pane lays the echo out **one character per line**, so no line of it can ever
/// equal a line of what we sent and the line-wise match above always gives up — which is how a
/// whole prompt came back stored as the agent's reply, rendered as a vertical column of glyphs.
///
/// Compare with every whitespace character removed instead. To keep a real reply safe this
/// still demands that the candidate *opens* with a tail of the prompt, at least
/// [`SQUASH_MIN`] characters of it; a candidate that is itself only a fragment of that tail is
/// pure echo and leaves nothing behind.
const SQUASH_MIN: usize = 8;

fn strip_echoed_prompt_squashed(text: &str, prompt: &str) -> Option<String> {
    let ps: Vec<char> = prompt.chars().filter(|c| !c.is_whitespace()).collect();
    let ts: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if ps.len() < SQUASH_MIN || ts.is_empty() {
        return None;
    }
    // Longest tail of the prompt that the candidate opens with. The head is what the `❯ ` row
    // took away, so the tail is what survives on screen.
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
    // Drop that many non-whitespace characters off the front of the original text.
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

/// How many columns wide this run's pane currently is, for the "too narrow to read" message.
/// Best effort: it only ever decorates a diagnostic, so any failure just means less detail.
async fn pane_columns(app: &Arc<App>, run: &db::Run) -> Option<u32> {
    let pane = run.pane_id.clone()?;
    let ws = run.workspace_id.clone()?;
    let client = client_for_run(app, run).await.ok()?;
    let rects = client.pane_rects(&ws).await.ok()?;
    rects.into_iter().find(|(id, _, _)| *id == pane).map(|(_, w, _)| w)
}

/// Has the pane shredded its output into a column of single characters?
///
/// herdr's `recent_unwrapped` undoes the *terminal's* soft wrapping, not the TUI's own layout:
/// in a pane a few columns wide the agent itself lays text out one glyph per row, and the
/// spaces between words fall off the ends of those rows entirely. Nothing can reconstruct that,
/// so the honest move is to say so rather than store a vertical column as the reply.
fn is_shredded(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() < 6 {
        return false;
    }
    // The widest row is the decisive one: whatever the mix of glyphs and word fragments, no
    // pane whose longest line is a few characters is holding a readable answer. Counting short
    // lines alone missed the real case (`w8:pK`, 31 columns, grok's borders eating most of
    // them) — its rows were `381K` / `Hel` / `Off` / `by`, so only half were ≤ 2 chars while
    // the *widest* was 4.
    if lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) <= 6 {
        return true;
    }
    let narrow = lines.iter().filter(|l| l.chars().count() <= 2).count();
    narrow * 10 >= lines.len() * 7
}

/// Does this row have the *shape* of a spinner frame — `<Verb>… (3m 18s · ↓ 11.0k tokens)` —
/// whatever glyph, if any, precedes it?
///
/// Neither half of the row can be matched literally: the verb is picked at random per frame
/// (`Thinking`, `Boogieing`, `Improvising`, `Puttering`, `Simmering`, …) and the leading glyph
/// set is a moving target across CLI releases. The bracketed elapsed time / token counter is
/// the part that has stayed stable, so that is what this matches — a single word, `… (`, then
/// a parenthesised status carrying `tokens` or a duration.
fn is_activity_shape(s: &str) -> bool {
    // A leading decoration glyph is ignored here; the caller strips it from what it reports.
    let body = match s.chars().next() {
        Some(c) if !c.is_alphanumeric() => s[c.len_utf8()..].trim_start(),
        _ => s,
    };
    let Some((verb, tail)) = body.split_once("… (") else { return false };
    if verb.is_empty() || verb.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((inner, _)) = tail.rsplit_once(')') else { return false };
    inner.contains("tokens") || inner.split(|c: char| c.is_whitespace() || c == '·').any(|t| is_elapsed_token(t.trim()))
}

/// The agent's *current activity* — the spinner row of this turn (`✻ Thinking… (12s · ↑ 1.2k
/// tokens)`, grok's `◆ Thought for 0.1s`), with the glyph stripped.
///
/// `clean_screen` (and therefore `live_reply`) throws these lines away as TUI chrome, which is
/// right for the message body but leaves a purely-thinking turn with nothing at all to show.
/// This is a read-only side channel for `turn_progress.activity`: it never reaches a stored
/// message, so `clean_screen` / `extract_reply` stay untouched.
fn live_activity(kind: &str, text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = after_last_prompt_echo(kind, &lines);
    let grok = kind == "grok";
    // grok marks event / thinking rows with `◆` (see `is_grok_noise`); claude / codex cycle
    // through this spinner set. It grows between CLI releases, so it is a fast path, not the
    // whole test — `is_activity_shape` below catches frames printed with a glyph we don't know.
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
        // The verb is randomised per frame (`Thinking…`, `Boogieing…`, `Improvising…`), so this
        // can only ever match on shape — never on the word itself.
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
        // Keep scanning: the last activity row on screen is the current one.
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

/// A retry / API-error banner on the pane, e.g. claude's `API error · Retrying in 3s ·
/// attempt 1/10` or codex's `stream error: …; retrying 2/5`.
///
/// The turn is still technically in flight when one of these is on screen, so `activity` shows
/// the spinner and the UI looks healthy while the agent is actually stuck retrying an upstream
/// failure. Surfaced separately as `turn_progress.alert` so the UI can say so.
///
/// Shape, never wording: a short line that says *error* **and** carries a retry / attempt token
/// — or that opens with `API error`. Requiring both halves keeps the agent's own prose about
/// errors (which is neither short nor retry-shaped, and rarely both) out of the banner.
fn live_alert(kind: &str, text: &str) -> Option<String> {
    const RETRY_TOKENS: [&str; 6] = ["retry", "retrying", "attempt", "reconnect", "重試", "retries"];
    // Codex hard limit may be wrapped across many narrow-pane rows — use the multi-line scanner.
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
        let retrying = RETRY_TOKENS.iter().any(|t| low.contains(t));
        if !retrying && !low.starts_with("api error") {
            continue;
        }
        // Keep scanning: the newest banner is the one that is still true.
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

// ---------------------------------------------------------------- prompt-stall watchdog

const STALL_SECS: u64 = 12;

/// After a delivered prompt the agent must leave `idle` within `STALL_SECS`; otherwise the
/// Turn would sit `in_flight` forever (e.g. Claude "Not logged in", or a modal we cannot see)
/// and the composer would stay locked. Cancelled by the first `working` / `blocked` event.
pub async fn arm_stall(app: &Arc<App>, run_id: &str, bot_id: &str, turn_id: &str) {
    let mut timers = app.stall_timers.lock().await;
    if let Some(h) = timers.remove(run_id) {
        h.abort();
    }
    let app2 = app.clone();
    let run_id = run_id.to_string();
    let bot_id = bot_id.to_string();
    let turn_id = turn_id.to_string();
    let key = run_id.clone();
    let h = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(STALL_SECS)).await;
        let lock = app2.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        if let Err(e) = fail_stalled_turn(&app2, &run_id, &bot_id, &turn_id).await {
            tracing::warn!(error = ?e, "stall watchdog failed");
        }
        app2.stall_timers.lock().await.remove(&run_id);
    });
    timers.insert(key, h);
}

pub async fn cancel_stall(app: &Arc<App>, run_id: &str) {
    if let Some(h) = app.stall_timers.lock().await.remove(run_id) {
        h.abort();
    }
}

async fn fail_stalled_turn(app: &Arc<App>, run_id: &str, bot_id: &str, turn_id: &str) -> anyhow::Result<()> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(()) };
    if run.agent_status == "working" || run.agent_status == "blocked" {
        return Ok(());
    }
    let Some(turn) = db::in_flight_turn(&app.db, run_id).await? else { return Ok(()) };
    if turn.id != turn_id || turn.delivery != "ok" {
        return Ok(());
    }
    // Peek at the screen and quote the lines that usually explain it. We do NOT assert a cause
    // (the host may well be logged in) — the user reads the quoted lines and decides.
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
    let reason = stall_reason(&hints);
    let res = sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=? AND status='in_flight'")
        .bind(db::now())
        .bind(turn_id)
        .execute(&app.db)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(());
    }
    tracing::warn!(turn = %turn_id, bot = %bot_id, %reason, "prompt stalled; turn failed");
    insert_message(app, &turn.conversation_id, Some(turn_id), "system", &reason, "system", false, snapshot.as_deref()).await?;
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

/// Neutral wording: state the symptom, quote the screen, and mention the keychain caveat.
/// Never claim the agent "is not logged in" — the host may be logged in and stuck for other reasons.
fn stall_reason(hints: &[String]) -> String {
    let head = format!("agent 在 {STALL_SECS} 秒內沒有對訊息作出反應。");
    if hints.is_empty() {
        return format!("{head}請查看終端分頁。");
    }
    format!(
        "{head}終端畫面：\n{}\n若該身份使用 macOS Keychain 儲存憑證，透過 ssh 啟動的 herdr 可能讀不到（畫面提示 `security unlock-keychain`）。",
        hints.join("\n")
    )
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
        match try_fallback(&app2, &run_id).await {
            // No turn to fall back on. For a run with no hooks that is not "nothing happened":
            // the agent just finished answering and the only record of it is on the pane.
            Ok(false) => {
                if let Err(e) = capture_hookless_turn_locked(&app2, &run_id, false).await {
                    tracing::warn!(error = ?e, "hookless terminal capture failed");
                }
            }
            Ok(true) => {}
            Err(e) => tracing::warn!(error = ?e, "terminal fallback failed"),
        }
        // A Codex usage-reset hint can be rendered after the turn's notify hook has already
        // completed it. Capture it even when there is no longer an in-flight turn to fall back.
        if let Err(e) = capture_codex_usage_notices(&app2, &bot_id, &run_id).await {
            tracing::debug!(error = ?e, "codex notice capture failed");
        }
        app2.fallback_timers.lock().await.remove(&run_id);
    });
    timers.insert(key, h);
}

/// `Ok(true)` when a turn was actually completed from the pane, `Ok(false)` when there was
/// nothing to fall back on (no in-flight turn, or one whose delivery we do not trust).
async fn try_fallback(app: &Arc<App>, run_id: &str) -> anyhow::Result<bool> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(false) };
    let Some(turn) = db::in_flight_turn(&app.db, run_id).await? else { return Ok(false) };
    if turn.delivery != "ok" {
        return Ok(false);
    }
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(false) };

    // Read the pane *before* the turn is claimed. Marking it complete first and then failing
    // on the session or the read left the turn `completed_fallback` with no assistant message,
    // no `turn_updated` on the wire (only `emit_turn` sends one) and no `schedule_flush_queued`
    // — a locked composer and a queued prompt that never went out. Failing here instead leaves
    // the turn in flight, which the next `working -> idle` edge re-arms.
    let pane_id = run.pane_id.clone().unwrap_or_default();
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| anyhow::anyhow!("no Herdr session is available for run `{}`", run.id))?;
    let read = client.pane_read(&pane_id, "recent_unwrapped", 200).await?;

    // CAS: only one writer wins the turn.
    let res = sqlx::query("UPDATE turns SET status='completed_fallback', completed_at=? WHERE id=? AND status='in_flight'")
        .bind(db::now())
        .bind(&turn.id)
        .execute(&app.db)
        .await?;
    if res.rows_affected() == 0 {
        return Ok(false);
    }
    tracing::info!(turn = %turn.id, "terminal fallback engaged");

    let fresh = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());

    // Codex hard limit: prefer a system notice + failed turn over a fake assistant reply.
    // Scan both the fresh slice and the full snapshot — the banner may sit above the cursor.
    if bot.kind == "codex" {
        if let Some(hit) = codex_usage_notice_lines(&fresh)
            .into_iter()
            .chain(codex_usage_notice_lines(&read.text))
            .find(|n| codex_limit_hit_line(n).is_some())
        {
            sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=?")
                .bind(db::now())
                .bind(&turn.id)
                .execute(&app.db)
                .await?;
            insert_message(
                app,
                &turn.conversation_id,
                Some(&turn.id),
                "system",
                &hit,
                "system",
                false,
                Some(&read.text),
            )
            .await?;
            let host = db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
            apply_codex_limit_hit_quota(app, &host, &hit).await;
            remember_pane_cursor(app, run_id, &read).await?;
            emit_turn(app, &turn.id).await;
            return Ok(true);
        }
    }

    let reply = extract_reply(&bot.kind, &fresh)
        .or_else(|| clean_screen(&bot.kind, &fresh))
        .unwrap_or_else(|| "（終端沒有可辨識的回覆）".to_string());
    // Only the `❯ <first line>` row counts as the echo, so a multi-line prompt leaves lines
    // 2..n on screen and they would be stored as the agent's answer.
    let sent = db::turn_user_messages(&app.db, &turn.id).await.unwrap_or_default();
    let reply = sent.iter().fold(reply, |acc, p| strip_echoed_prompt(&acc, p));
    // What is left has to be worth storing. An empty string means the capture was nothing but
    // our own prompt coming back; a shredded one means the pane is too narrow to read at all.
    let reply = if reply.trim().is_empty() {
        "（終端沒有可辨識的回覆）".to_string()
    } else if is_shredded(&reply) {
        // Name the pane and its width: "too narrow" is not actionable when the user has a
        // dozen panes open and no idea which one, or how much wider it needs to be.
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
    };

    remember_pane_cursor(app, run_id, &read).await?;

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
    Ok(true)
}

// ------------------------------------------------- hookless runs: the terminal is the source

/// An adopted run whose bot has no hooks injected.
///
/// Nothing will ever POST a `user_prompt` / `stop` payload for such a run, so §4.3's terminal
/// snapshot is not a *fallback* here — it is the only place its conversation can come from.
/// The `managed_by='child'` bots `reconcile` adopts (one agent spawning another) are all like
/// this: their pane was started by their parent, so the 終端 tab showed what they were saying
/// while 對話 stayed empty.
fn is_hookless(bot: &db::Bot, run: &db::Run) -> bool {
    run.adopted != 0 && bot.inject_hooks == 0
}

/// Longest terminal-scraped reply stored for a hookless turn. The snapshot is 200 lines of
/// scrollback, so a capture anywhere near this is a screen rather than a message; cutting it
/// keeps one adoption from writing a novel into the conversation.
const HOOKLESS_REPLY_MAX: usize = 6000;

/// How long after adoption the pane is read. An agent herdr has only just detected is often
/// still painting its banner, and its `agent_status` settles a moment after the pane appears.
const ADOPTED_CAPTURE_DELAY: Duration = Duration::from_secs(2);

/// Store the exchange a hookless run has just finished as a turn of its own.
///
/// The ordinary path needs the daemon to have watched the whole turn: `begin_external_turn`
/// opens it on the `-> working` edge and `try_fallback` closes it on the way back to `idle`.
/// An adopted pane is routinely picked up *mid-answer* — reconcile runs when herdr detects the
/// child agent, by which time its parent has already prompted it — so `working -> idle` arrives
/// with no turn in flight and the reply used to be dropped on the floor. This writes it as a
/// finished `external` turn instead, from the same snapshot, through the same noise filters.
///
/// `seed` is the one-shot capture done at adoption, when the exchange on screen is already
/// over. It is refused unless the conversation is still empty, so re-adopting a bot (which
/// happens on every daemon restart and every event-stream reconnect) cannot copy one screen in
/// twice.
///
/// The caller must hold the bot lock. Returns whether anything was stored.
async fn capture_hookless_turn_locked(app: &Arc<App>, run_id: &str, seed: bool) -> anyhow::Result<bool> {
    let Some(run) = db::run(&app.db, run_id).await? else { return Ok(false) };
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(false) };
    if !is_hookless(&bot, &run) || run.state != "running" {
        return Ok(false);
    }
    // §4.3 keeps `blocked` out of the fallback, and for the same reason: a modal waiting for an
    // answer is not an ended turn, and the question on screen is not a reply.
    if run.agent_status == "blocked" {
        return Ok(false);
    }
    // An in-flight turn belongs to `try_fallback`; capturing here as well would store the
    // reply twice.
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
    // What the user (or the parent agent) typed. The pane echo is the only record of it —
    // there is no hook payload to read it from, ever.
    let echo = last_prompt_echo_text(&bot.kind, &fresh);
    // Where does this exchange begin? With a cursor, at the cursor. Without one the snapshot
    // is the whole scrollback and only the prompt echo says where the last turn started, so
    // with neither we just remember the cursor and wait for the next edge — storing screens of
    // older turns as one message is worse than storing nothing.
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
    // Nothing readable on screen: move the cursor on and say nothing. Unlike `try_fallback`
    // there is no turn waiting to be closed, so a 「（終端沒有可辨識的回覆）」 bubble here would
    // be noise nobody asked for.
    if reply.trim().is_empty() || is_shredded(&reply) {
        remember_pane_cursor(app, run_id, &read).await?;
        return Ok(false);
    }
    // The screen need not have moved since the last edge (herdr reports `working -> idle` more
    // than once for a single answer, and the cursor only matches when the tail is still on
    // screen), so an identical reply is the same reply.
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
        // 對話 is ordered by message id, and a ULID is only ordered by its millisecond — two
        // messages written in the same one sort at random, which here would put the reply
        // above the prompt that caused it. This is the only place the daemon writes both
        // halves of an exchange back to back, so it is the only place that has to wait.
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

/// Give a freshly adopted hookless run whatever its pane can tell us.
///
/// Called by `reconcile` for every adopted agent, off the reconcile's own task because that
/// holds the bot lock and both paths here take it.
///
/// * still `working`: open the `external` turn now, exactly as if we had watched the user type
///   — the answer then streams into 對話 live and `try_fallback` closes the turn as usual.
/// * already `idle`: the exchange is over, so store it once (`seed`), otherwise the
///   conversation stays empty until somebody prompts the agent again.
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

/// The newest `assistant` message of a conversation — the duplicate guard for terminal
/// capture, which re-reads a screen that may not have changed since the previous edge.
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

/// The prompt-echo prefix each CLI prints in front of what the user typed. Single source of
/// truth for `after_last_prompt_echo` and `last_prompt_echo_text`.
fn prompt_echo_prefix(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" | "grok" => Some("❯ "),
        "codex" => Some("› "),
        _ => None,
    }
}

/// Index of the first line *after* the last prompt echo (`❯ …` / `› …`), or 0 when the echo
/// is not on screen. Everything before it belongs to earlier turns.
fn after_last_prompt_echo(kind: &str, lines: &[&str]) -> usize {
    let Some(echo) = prompt_echo_prefix(kind) else { return 0 };
    lines
        .iter()
        .rposition(|l| {
            let t = l.trim_start();
            t.starts_with(echo) && t.len() > echo.len()
        })
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// What the user typed on the *last* prompt echo — the echo line itself, prefix stripped.
///
/// `after_last_prompt_echo` reports the index of the line **after** the echo, so the echo is
/// at `idx - 1` and `idx == 0` means no echo is on screen. This is how a turn the user started
/// by typing straight into the tmux pane gets its user message: there is no hook payload to
/// read it from until the turn ends.
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
    if body.is_empty() { None } else { Some(body.to_string()) }
}

/// Is this line TUI chrome (banner, boxes, rules, status bar, spinner) rather than content?
fn is_noise(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.chars().next().unwrap_or(' ');
    if "▐▝▛▜█╭╮╰╯│▔✻✽✶✳⏵⚠·".contains(first) {
        return true;
    }
    if s.chars().all(|c| c == '─' || c == '━' || c == '-' || c == '=' || c == '_' || c == ' ') {
        return true;
    }
    // Claude Code status bar: "user | project | model | 5h:- | 7d:-"; codex: "gpt-… · ~/dir · 5h 60% left"
    if (s.contains(" | ") && (s.contains("5h:") || s.contains("7d:"))) || (s.contains(" · ") && s.contains("left")) {
        return true;
    }
    s.starts_with("Claude Code v") || s.starts_with("Tip:") || s.starts_with("Ask Codex") || s.contains("shift+tab to cycle")
}

/// grok 1.0.13 TUI chrome (appendix F): `◆ …` event / thinking lines, the "Worked for" footer
/// with its `[hooks: N]` chip, the telemetry opt-in banner, the shortcut footer, the
/// `<cwd>   15K / 500K` header, and the `[stable]` tag.
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

/// Strip grok's per-line decoration: the scrollbar glyph `█` at the right edge and the
/// right-aligned `h:mm AM|PM` timestamp on prompt / reply lines.
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

/// Extract the standalone Codex usage-reset hint from a pane snapshot. Codex has used both
/// `•` and `■` for this kind of account notice across CLI releases; neither glyph belongs in
/// the stored system message.
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
    // Drop a leading spinner / bullet so `• ERROR: …` still matches.
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
    // Keep a stable ERROR: prefix so the chat styles it as a hard failure.
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
        if let Some(notice) = codex_limit_hit_line(line).or_else(|| codex_usage_notice_line(line)) {
            push(&mut out, notice);
            i += 1;
            continue;
        }
        // Narrow panes wrap the hard-limit banner across many rows:
        //   ■ You've hit your usage
        //   limit. Upgrade to Pro
        //   …
        //   try again at 1:32 PM.
        let head = strip_codex_bullet(line);
        let head_low = head.to_ascii_lowercase();
        let looks_hit = head_low.contains("hit your usage")
            || (head_low.contains("error") && head_low.contains("usage"))
            || head_low.contains("you've hit");
        if looks_hit {
            let mut parts: Vec<&str> = vec![head];
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
            let joined = parts.join(" ");
            if let Some(notice) = codex_limit_hit_line(&joined) {
                push(&mut out, notice);
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
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

/// `try again at Aug 8th, 2025 1:47 PM` → RFC3339 UTC, if present.
fn parse_codex_try_again(notice: &str) -> Option<String> {
    use chrono::{Local, NaiveDate, TimeZone};
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
            // The notice ends in a full stop, so the last token is `pm.` — an exact match here
            // silently loses the meridiem and reports a reset twelve hours early.
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
    let (month, day, year, mut hour) = (month?, day?, year?, hour?);
    if pm && hour < 12 {
        hour += 12;
    }
    if !pm && hour == 12 {
        hour = 0;
    }
    let naive = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
    let dt = Local.from_local_datetime(&naive).earliest()?;
    Some(dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// When Codex prints a hard limit-hit, mirror it onto that host's `codex` quota row so the
/// strip shows empty immediately (the rate-limits RPC can lag a turn behind the TUI).
async fn apply_codex_limit_hit_quota(app: &Arc<App>, host: &str, notice: &str) {
    let resets = parse_codex_try_again(notice);
    let key = crate::quota::quota_key(host, "codex");
    let mut q = app
        .quotas
        .lock()
        .await
        .get(&key)
        .cloned()
        .unwrap_or_else(|| crate::quota::Quota {
            five_hour: None,
            seven_day: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "codex-limit-hit".into(),
            account: None,
            host: host.to_string(),
        });
    // Prefer marking the 5h window (the burst limit); fall back to 7d if that is all we have.
    let win = crate::quota::Window { used_pct: 100.0, resets_at: resets.clone() };
    if q.five_hour.is_some() || q.seven_day.is_none() {
        q.five_hour = Some(win);
    } else if let Some(existing) = q.seven_day.as_mut() {
        existing.used_pct = 100.0;
        if resets.is_some() {
            existing.resets_at = resets;
        }
    } else {
        q.seven_day = Some(win);
    }
    q.updated_at = crate::db::now();
    q.source = "codex-limit-hit".into();
    crate::quota::set(app, host, "codex", q).await;
}

/// No reply marker found: keep whatever the agent printed after the last prompt echo,
/// minus TUI chrome. Tool-result lines (`⎿ …`) are kept because they usually carry the
/// actual error ("Not logged in · Please run /login").
fn clean_screen(kind: &str, text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = after_last_prompt_echo(kind, &lines);
    let mut out: Vec<String> = Vec::new();
    let grok = kind == "grok";
    // grok's telemetry opt-in banner wraps at the pane width, so it is skipped as a block:
    // from "Help improve Grok" through "Read Terms and Privacy Policy." inclusive.
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
        // `is_noise` only knows the spinner glyphs we have seen; `is_activity_shape` catches the
        // rest by shape, so a frame like `✛ Generating… (4s · thinking)` cannot reach a message.
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

/// Provider-specific reply extraction from a terminal snapshot.
///
/// grok prints the reply as plain indented text with no marker (appendix F), so it has no
/// entry here and always goes through `clean_screen`.
fn extract_reply(kind: &str, text: &str) -> Option<String> {
    let marker = match kind {
        "claude" => "⏺ ",
        "codex" => "• ",
        _ => return None,
    };
    let lines: Vec<&str> = text.lines().collect();
    // A2: only this turn's output counts. Without this the last `⏺` line of the *previous*
    // turn would be handed back as the answer whenever the current turn printed no marker.
    let after_echo = after_last_prompt_echo(kind, &lines);
    let start = after_echo + lines[after_echo..].iter().rposition(|l| l.trim_start().starts_with(marker))?;
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
        // Skip the spinner / status line ("✻ Crunched for 9s · done 11:35 PM").
        if s.starts_with('✻') || s.starts_with('✽') || s.starts_with('✶') || s.starts_with('·') {
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

    /// Two turns; the second produced only tool output. The previous turn's `⏺ FIRST-ANSWER`
    /// must not be reported as the answer to "echo 2" (review A2).
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

    /// grok 1.0.13 `agent.read {source: visible}` after "Reply with exactly GROK-OK"
    /// (appendix F), columns narrowed.
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

    #[test]
    fn grok_banner_is_skipped_as_a_block() {
        assert_eq!(clean_screen("grok", GROK_SCREEN_NARROW).unwrap(), "GROK-FALLBACK");
    }

    /// Claude mid-turn with nothing printed yet: spinner + input box + status bar only.
    /// Everything here is chrome, so `live_reply` has nothing — but the user still needs to
    /// see that the agent is thinking.
    const THINKING_ONLY: &str = "\
❯ 幫我看一下這個 bug
✻ Thinking… (12s · ↑ 1.2k tokens · esc to interrupt)
╭────────────────────────────────────────────╮
│ ❯                                          │
╰────────────────────────────────────────────╯
  tony. | pt | HAI4.5 | 5h:- | 7d:-
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

    /// Reported from a real stuck session (2026-09-06): the UI sat on 「等待回覆（hook）…」
    /// for 3m18s. The verb is randomised per frame and `✢` was missing from the glyph set,
    /// so this row has to be caught on shape alone.

    /// Same frame with a glyph that is in no whitelist at all — `is_activity_shape` is what
    /// keeps this working when the CLI adds a spinner character we have never seen.

    /// Reported 2026-09-06: the reply bubble came back holding the user's own message.
    /// A multi-line prompt echoes as one `❯ <first line>` row plus its remaining lines verbatim,
    /// and only the marker row is skipped — so lines 2..n were stored as the agent's answer.
    const ECHOED_BACK: &str = "\
❯ 併行
1 沒事 bot 不會需要停止的動作
2 執行中 能夠show session name agent取的名字

✛ Generating… (4s · thinking)
";
    const SENT: &str = "併行\n1 沒事 bot 不會需要停止的動作\n2 執行中 能夠show session name agent取的名字";

    #[test]
    fn the_users_own_prompt_does_not_come_back_as_the_reply() {
        // Before the fix this was the whole tail of the prompt plus the spinner row.
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

    /// Real capture (2026-09-06): the pane was a couple of columns wide, so the echo of
    /// 「不要依剩餘量重排 固定 cc0 cc1 codex grok」came back one glyph per line and the
    /// line-wise match never fired — the whole prompt got stored as the agent's reply.
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

    /// Real capture (2026-09-06) from the stuck grok pane `w8:pK`: grok had finished and was
    /// sitting at an empty composer, but herdr still reported `agent_status: working`, so
    /// nothing ever completed the Turn. The box frame is why `clean_screen`'s bare `s == "❯"`
    /// test does not see this.
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

    /// Real capture (2026-09-06): pane `w8:pK` was so narrow that every CJK character wrapped
    /// onto its own row, and the fallback stored the user's own prompt back as the reply.
    /// `strip_echoed_prompt` cannot help here — the wrap points do not line up with what we
    /// sent, so it correctly refuses to strip a partial match.
    #[test]
    fn a_pane_too_narrow_to_read_is_recognised() {
        let shredded = "要\n依\n剩\n餘\n量\n重\n排\n固\n定\nc\nc\n0\nHelp impro\n";
        assert!(is_shredded(shredded));
        // A normal reply must never be mistaken for one, however short its lines are.
        assert!(!is_shredded("好的，我來處理。\n改了三個檔案：\n- a.rs\n- b.rs\n- c.rs\n都跑過測試了。\n"));
        // Too little to judge: a two-line answer is not evidence of a broken pane.
        assert!(!is_shredded("好\n的\n"));

        // Verbatim from `w8:pK` (2026-09-06): 31 columns, most of them eaten by grok's borders.
        // Only half of these rows are ≤ 2 chars, so counting short lines alone let it through —
        // the widest row being 4 characters is what actually gives it away.
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
        // Claude keeps the empty composer on screen while working, which is exactly why the
        // caller pairs this with "nothing changed for N polls" rather than trusting it alone.
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

    /// Once the agent prints something, `live_reply` keeps working exactly as before and the
    /// activity row is still reported alongside it (the UI prefers the text).
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

    /// grok's thinking rows are `◆ …` and carry the scrollbar glyph, so they go through
    /// `strip_grok_decor` first (same as `clean_screen`).
    #[test]
    fn live_activity_reads_grok_event_rows() {
        assert_eq!(live_activity("grok", GROK_SCREEN).unwrap(), "Thought for 0.1s");
    }

    /// Real capture from the stuck session that started this fix: three minutes in, the pane
    /// carried nothing but this row and the UI still said 「等待回覆（hook）…」.
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

    /// The spinner verb is randomised and the glyph set grows between CLI releases, so an
    /// unknown glyph — or none at all — must still be recognised by shape alone.
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

    /// Before the prompt echo there is nothing to report (the previous turn's spinner must
    /// not leak into this turn).
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

    /// The CLI-typed prompt is read back off the pane's own echo — the line `after_last_prompt_echo`
    /// stops just past. The *last* echo wins, so an earlier turn's prompt is never reported.
    #[test]
    fn last_prompt_echo_text_reads_what_the_user_typed() {
        assert_eq!(last_prompt_echo_text("claude", TWO_TURNS).as_deref(), Some("echo 2"));
        assert_eq!(last_prompt_echo_text("claude", NOT_LOGGED_IN).as_deref(), Some("echo 1"));
        assert_eq!(last_prompt_echo_text("codex", "› 幫我看一下這個 bug\n  thinking…\n").as_deref(), Some("幫我看一下這個 bug"));
    }

    /// grok's echo row carries the right-aligned clock and the scrollbar glyph; neither is
    /// part of the prompt.
    #[test]
    fn last_prompt_echo_text_strips_grok_decor() {
        let screen = "❯ Reply with GROK-OK                    2:09 AM   █\n     GROK-OK\n";
        assert_eq!(last_prompt_echo_text("grok", screen).as_deref(), Some("Reply with GROK-OK"));
    }

    /// No echo on screen, an empty prompt box, or a CLI we have no prefix for: report nothing
    /// rather than a made-up prompt (`begin_external_turn` then opens the turn with no user
    /// message at all).
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
    //! The durable prompt queue's claim step, driven straight at the sqlite state machine.
    //! `schedule_flush_queued` is a no-op under `cfg!(test)`, so these call
    //! `flush_queued_locked` themselves — which is also the only way to observe what the
    //! background task would have done.
    use super::*;
    use crate::team::testing as tt;

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
        turn_id: String,
    }

    /// A bot with a running run and one queued prompt behind it. `session` is what the run
    /// claims to live on: `"test"` is the one the mock herdr answers, anything else makes
    /// `client_for_run` fail — which is exactly the shape of a host that dropped out between
    /// the prompt being queued and the flush trying to deliver it.
    async fn queued(session: &str) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'q','claude','[]',0,1,'tok',?)",
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

    /// **The regression.** The flush claims the turn (`queued -> in_flight`) *before* it has
    /// a herdr client. When that lookup then fails the turn used to be abandoned mid-claim:
    /// `in_flight` with `delivery='pending'`, which nothing finishes — `arm_stall`,
    /// `arm_progress` and `try_fallback` all require `delivery == "ok"` — so the composer
    /// said "回合進行中" for ever and every later `prompt()` was refused with 409 "a turn is
    /// already in flight". It must go back on the queue instead.
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
        // The two consequences of the bug, checked directly.
        assert!(
            db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none(),
            "nothing is in flight, so the next prompt is not refused with 409",
        );
        assert_eq!(
            db::queued_turn(&app.db, &f.conv).await.unwrap().map(|q| q.id),
            Some(f.turn_id.clone()),
            "the durable queue still holds it, so a later transition retries the delivery",
        );

        // And it really is retryable: the same call against a reachable session claims it
        // again, so requeueing did not poison `turns_one_queued` or the CAS.
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

    /// The other side of the same coin: once the RPC has actually gone out, a failure is
    /// **not** requeued. We do not know whether the agent took the text, so replaying it
    /// could deliver the same prompt twice; `delivery='unknown'` is the designed parking
    /// state and `prompt()` names that turn in its "abandon it first" conflict.
    ///
    /// (The mock herdr answers `agent.prompt` with `unsupported`, which is a delivery
    /// failure that is not `agent_blocked` — precisely this case.)
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

    /// An empty queued prompt is still dropped rather than requeued — an unchanged path,
    /// pinned here because "always put it back" would turn it into an infinite queue.
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
mod tab_tests {
    //! One bot, one tab (and the retrofit for the bots that predate it).
    //!
    //! These drive the real functions against the mock herdr in `team::testing`, which keeps
    //! genuine tab/pane bookkeeping — so "the tab was closed" is a fact about the server's
    //! state, not about which RPC we happened to send. The mock deliberately does **not**
    //! reap a tab when its last pane closes, though herdr 0.8.2 does: that is the only way to
    //! see whether the daemon tidies up on its own rather than leaning on the server.
    use super::*;
    use crate::team::testing as tt;

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

    /// Every start pre-trusts its own working directory, not just a team's worktree. A project
    /// pointed at a directory claude has never opened hits the same "Is this a project you
    /// trust?" prompt, whose cursor starts on *No, exit* — 2026-09-06 that killed every team,
    /// and an ordinary bot in a fresh checkout fails the same way.
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
        // The key has to be the resolved path: on macOS `/tmp` is a symlink and the CLI
        // compares against its own `getcwd()`, so an unresolved key silently does nothing.
        assert!(!key.starts_with("/tmp/"), "the recorded path is canonical, got {key}");

        assert!(!crate::trust::mark_trusted("claude", &store, &[key]).unwrap(), "already trusted: left alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The change.** Starting a bot asks for a *tab*, not a split of somebody else's pane,
    /// and hands back that tab's root pane. `pane.split` is never sent — which is the whole
    /// point: panes of one tab divide a fixed width between them, tabs do not.
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

        // The call itself: same cwd/env contract as the old `pane.split`, the bot's nickname
        // on the tab bar, and never stealing focus from whatever the user is reading.
        let p = env.herdr.first_call("tab.create").expect("tab.create was called");
        assert_eq!(p["workspace_id"], json!(ws.workspace_id));
        assert_eq!(p["cwd"], json!("/tmp/p"));
        assert_eq!(p["label"], json!("alfa"));
        assert_eq!(p["focus"], json!(false), "starting a bot must not yank the user's focus");
        assert_eq!(p["env"]["AM_BOT_ID"], json!("b1"));
        assert_eq!(p["env"]["AM_HOOK_TOKEN"], json!("tok"));
    }

    /// A workspace we just created is already one tab with one pane in it, so the first bot
    /// in a fresh project sits in the root pane rather than opening a second, empty tab.
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

    /// Stopping a bot takes its tab with it, so a day of starting and stopping does not leave
    /// a row of empty tabs across the top of the workspace.
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

    /// The other half of the same rule: a pane that *shares* its tab — every bot started
    /// before this change, and anything the user split by hand — only loses the pane. Closing
    /// the tab there would take a neighbour's agent down with it.
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
        // `tab_id` recorded even though the tab is shared — reconcile fills it in from herdr
        // for old runs too, so "has a tab id" must not be what decides to close the tab.
        running_on(&app, &bot, &ws.workspace_id, &mine.pane_id, Some(&mine.tab_id)).await;

        assert!(stop_bot(&app, &bot).await.unwrap());

        let tab = env.herdr.tab(&mine.tab_id).expect("the shared tab survives");
        assert!(!tab.panes.contains(&mine.pane_id), "our pane is gone");
        assert!(tab.panes.contains(&neighbour.pane_id), "the neighbour's agent is untouched");
    }

    /// The retrofit endpoint: a bot already running in a shared tab is given one of its own.
    /// It is a move, so the `pane_id` — everything the run, its subscription and any turn in
    /// flight are keyed by — must survive unchanged.
    #[tokio::test]
    async fn moving_a_running_bot_gives_it_a_tab_without_changing_its_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let neighbour = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let mine = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        // NULL tab_id: exactly what a run started before the column existed looks like.
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

    /// The tidy-up both paths share, on its own. It is the *only* thing that decides a tab
    /// may go, and it decides it from herdr's pane count rather than from anything we
    /// recorded — a tab still holding a pane is somebody's live agent.
    ///
    /// It also has to be quiet about a tab that is already gone: herdr 0.8.2 reaps a tab when
    /// its last pane closes and `pane.move` closes the tab it emptied, so "not found" is the
    /// ordinary outcome in production, and only the mock (which does not reap) ever reaches
    /// the `tab.close` below.
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

    /// Idempotence. A pane that already owns its tab is left exactly where it is: on herdr a
    /// second move is *not* a no-op — it builds a new tab and closes the old one, renumbering
    /// the user's tab bar for nothing.
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
    //! 對話 for a run the daemon did not start and never injected hooks into: a spawned child
    //! agent (`managed_by='child'`). Everything it says has to be scraped off its pane, so
    //! these drive `capture_hookless_turn_locked` at a mock herdr holding a real screen.
    use super::*;
    use crate::team::testing as tt;

    /// A finished claude exchange, as `recent_unwrapped` renders it: the prompt echo, the
    /// reply, the status line, and the empty composer below the rule.
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

    /// An adopted, hook-less bot sitting in `pane-1`, with `screen` on that pane.
    /// `hooks` / `adopted` are the two flags that decide whether the terminal is the source.
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
            // Ordered the way `GET /api/bots/{id}/messages` orders it, so a test sees what 對話 shows.
            "SELECT role, content, source FROM messages WHERE conversation_id = ? ORDER BY id",
        )
        .bind(conv)
        .fetch_all(&app.db)
        .await
        .unwrap()
    }

    /// **The bug.** A child agent's `working -> idle` edge arrives with no turn in flight —
    /// the daemon adopted the pane mid-answer, so it never saw the `-> working` edge that
    /// opens one — and `try_fallback` bails on that, which left 對話 empty for a bot whose
    /// 終端 tab was full of text. The edge now becomes a turn of its own.
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

    /// herdr reports `working -> idle` more than once for one answer, and a re-adoption reads
    /// the same screen again. Neither may grow the conversation.
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

    /// A bot whose pane *we* started keeps hooks as its source of truth (SPEC §4.3: the
    /// snapshot is the備援, not the record), so this path must not touch it — a hook reply is
    /// complete, a scrape is not.
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

    /// Without a cursor the snapshot is the whole scrollback, and the prompt echo is the only
    /// thing marking where the last turn began. With neither, storing "the screen" would drop
    /// several turns into one bubble — so it stores nothing and just remembers the cursor.
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

    /// The adoption seed only ever fires into an empty conversation: a bot is re-adopted on
    /// every daemon restart and every event-stream reconnect, and its last exchange is still
    /// on screen each time.
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
