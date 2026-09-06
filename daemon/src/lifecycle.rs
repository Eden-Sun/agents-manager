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
        app.emit("turn_updated", json!({ "bot_id": bot_id, "turn": t })).await;
        // SPEC-team §3: the same transition on the internal bus. Every path that takes a
        // turn out of `in_flight` (hook match, terminal fallback, watchdog, stop, interrupt)
        // funnels through here, so team schedulers only need this one subscription.
        app.publish_turn(crate::state::TurnEvent {
            bot_id,
            turn_id: t.id.clone(),
            status: t.status.clone(),
            delivery: t.delivery.clone(),
            team_id: t.team_id.clone(),
            team_event_id: t.team_event_id.clone(),
        });
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

    let cfg = app.cfg.get().await;
    if let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if let Some(id) = cfg.identities.iter().find(|i| i.name == idn) {
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

/// v4.0: `bot.persona` appended to the agent's system prompt, per kind. Sits right after the
/// daemon's own flags (before model / effort). Nothing when empty.
fn persona_args(bot: &db::Bot) -> Vec<String> {
    let Some(p) = bot.persona.as_deref().filter(|s| !s.trim().is_empty()) else { return vec![] };
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
    // grok: `--reasoning-effort low|medium|high` (verified: `grok --help`, unknown value → error)
    // codex: `-c model_reasoning_effort="<x>"` (values from `model/list`); claude: nothing.
    if let Some(e) = bot.effort.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        match bot.kind.as_str() {
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

    #[test]
    fn claude_never_gets_effort_or_fast() {
        let a = model_args(&bot("claude", Some("opus"), Some("high"), true));
        assert_eq!(a, vec!["--model", "opus"]);
    }

    #[test]
    fn persona_per_kind() {
        use super::{persona_args, toml_basic_string};
        let mut b = bot("claude", None, None, false);
        b.persona = Some("回覆結尾一律加上 [PERSONA-OK]".into());
        assert_eq!(persona_args(&b), vec!["--append-system-prompt", "回覆結尾一律加上 [PERSONA-OK]"]);
        b.kind = "grok".into();
        assert_eq!(persona_args(&b), vec!["--rules", "回覆結尾一律加上 [PERSONA-OK]"]);
        b.kind = "codex".into();
        b.persona = Some("line1\nsay \"hi\" \\ done".into());
        assert_eq!(persona_args(&b), vec!["-c", "developer_instructions=\"line1\\nsay \\\"hi\\\" \\\\ done\""]);
        b.persona = Some("   ".into());
        assert!(persona_args(&b).is_empty());
        assert_eq!(toml_basic_string("a\tb\u{1}"), "\"a\\tb\\u0001\"");
    }
}

/// Extra CLI args contributed by the bot's identity.
async fn identity_args(app: &Arc<App>, bot: &db::Bot) -> Vec<String> {
    let Some(idn) = bot.identity.as_deref().filter(|s| !s.is_empty()) else { return vec![] };
    app.cfg
        .get()
        .await
        .identities
        .iter()
        .find(|i| i.name == idn)
        .map(|i| i.args.clone())
        .unwrap_or_default()
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
    let mut fresh_root: Option<String> = None;
    // `projects.workspace_id` belongs to the manager's configured session. An imported bot
    // lives in the user's default session, so it must not overwrite that mapping or cause the
    // next named-session reconcile to clear it.
    let workspace_id = match (session.as_str() != "default", project.workspace_id.as_deref()) {
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
            fresh_root = Some(root.pane_id.clone());
            ws.workspace_id
        }
    };

    // 3. pane
    //
    // SPEC-team §2.2: the pane's cwd is `bots.cwd` when the bot has one (a team member lives
    // in its own worktree), otherwise the project's path. The *workspace* is still created at
    // `project.path` — one workspace per project stays true.
    let cwd = bot_cwd(bot, project);
    let pane_id = match fresh_root {
        Some(p) => p,
        None => {
            let panes = client.pane_list(Some(&workspace_id)).await.map_err(up)?;
            let first = panes.first().map(|p| p.pane_id.clone()).ok_or_else(|| {
                LcError::Upstream(format!("workspace {workspace_id} has no panes to split"))
            })?;
            // Always splitting the *first* pane halves it every time: the sixth bot in a
            // workspace ended up 6 columns wide, at which the agent's TUI lays text out one
            // glyph per row and the terminal fallback cannot read anything. Split the pane with
            // the most room instead, along its longer axis, which grows a grid rather than a
            // cascade. `pane.layout` is best-effort — fall back to the old behaviour.
            let (target, dir) = match client.pane_rects(&workspace_id).await {
                Ok(rects) if !rects.is_empty() => {
                    let (id, w, h) = rects
                        .into_iter()
                        .max_by_key(|(_, w, h)| (*w as u64) * (*h as u64))
                        .expect("non-empty");
                    // A terminal cell is about twice as tall as it is wide, so compare the
                    // pane's *visual* proportions, not its cell counts.
                    (id, if w >= h * 2 { "right" } else { "down" })
                }
                _ => (first, "right"),
            };
            client.pane_split(&target, dir, cwd, env.clone()).await.map_err(up)?.pane_id
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
    let injected = injected_args(app, bot, project, &env).await.map_err(up)?;
    let mut args = injected;
    args.extend(persona_args(bot));
    args.extend(model_args(&effort_checked(app, bot, &project.host).await));
    args.extend(identity_args(app, bot).await);
    args.extend(bot.args());

    // 5. agent.start (async on the socket) — under `<project>-<bot>`, recorded on the run
    let agent = crate::config::agent_name(&project.label, &bot.id);
    sqlx::query("UPDATE runs SET agent_name = ? WHERE id = ?")
        .bind(&agent)
        .bind(run_id)
        .execute(&app.db)
        .await
        .map_err(up)?;
    if let Err(e) = client.agent_start(&agent, &bot.kind, &pane_id, &args, 60_000).await {
        let _ = client.pane_close(&pane_id).await;
        return Err(up(e));
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
                    let _ = client.pane_close(&pane_id).await;
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

/// Read and persist newly seen Codex usage-reset hints. The caller must hold the bot lock.
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
        insert_message(app, &conversation_id, None, "system", &notice, "system", false, None).await?;
        tracing::info!(bot = %bot.name, notice = %notice, "codex account notice captured");
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
    // we close it, otherwise a bare shell pane would linger until the next reconcile.
    if let Some(p) = run.pane_id.as_deref() {
        let _ = client.pane_close(p).await;
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

/// 有些設定不用重啟就能改：agent 的 TUI 自己有 slash 指令。
///
/// * grok `effort` → `/effort <level>`（grok 1.0.13 `04-slash-commands.md`）
/// * claude `model` → `/model <alias>`（alias 同 `claude --model`：opus / sonnet / haiku / fable…）
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
    let line = match (bot.kind.as_str(), field) {
        ("grok", "effort") => format!("/effort {}", value.to_ascii_lowercase()),
        ("claude", "model") => format!("/model {value}"),
        _ => return false,
    };
    let Ok(Some(run)) = db::active_run(&app.db, bot_id).await else { return false };
    if run.state != "running" || run.agent_status == "working" || run.agent_status == "blocked" {
        return false;
    }
    // 正在跑的回合會把這行吃成 prompt 的一部分。
    if !matches!(db::in_flight_turn(&app.db, &run.id).await, Ok(None)) {
        return false;
    }
    let Some(pane_id) = run.pane_id.clone() else { return false };
    let Ok(client) = client_for_run(app, &run).await else { return false };
    // 和 grok 額度探測同一套：先打字，等輸入列畫好，再送 Enter。
    if client.pane_send_text(&pane_id, &line).await.is_err() {
        return false;
    }
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    if client.pane_send_keys(&pane_id, &["Enter"]).await.is_err() {
        return false;
    }
    tracing::info!(bot_id, line, "applied live via slash command");
    true
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
        if let Err(e) = try_fallback(&app2, &run_id).await {
            tracing::warn!(error = ?e, "terminal fallback failed");
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
    let client = app
        .herdr_for_run(&run)
        .await
        .ok_or_else(|| anyhow::anyhow!("no Herdr session is available for run `{}`", run.id))?;
    let read = client.pane_read(&pane_id, "recent_unwrapped", 200).await?;
    let fresh = slice_after_cursor(&read.text, run.last_read_tail_hash.as_deref());
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
        "（終端太窄，輸出被切成單字元而無法辨識；把 herdr 的 pane 拉寬一點就會恢復）".to_string()
    } else {
        reply
    };

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

fn codex_usage_notice_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(notice) = codex_usage_notice_line(line) {
            if !out.iter().any(|seen| seen == &notice) {
                out.push(notice);
            }
        }
    }
    out
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
