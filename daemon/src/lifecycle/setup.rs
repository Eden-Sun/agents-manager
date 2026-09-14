//! What a pane needs before an agent starts: hooks, shims, skills, persona and argv.

use super::*;

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
pub(crate) async fn install_shim(app: &Arc<App>, bot: &db::Bot, project: &db::Project) -> Option<String> {
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
pub(crate) async fn injected_args(app: &App, bot: &db::Bot, project: &db::Project, env: &Value) -> anyhow::Result<Vec<String>> {
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
pub(crate) async fn pane_env(
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
pub(crate) async fn install_herdr_skill(app: &Arc<App>, bot: &db::Bot, project: &db::Project, env: &Value, agent_name: &str) {
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
pub(crate) fn persona_args(bot: &db::Bot, agent_name: &str) -> Vec<String> {
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
pub(crate) async fn effort_checked(app: &Arc<App>, bot: &db::Bot, host: &str) -> db::Bot {
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

pub(crate) fn model_args(bot: &db::Bot) -> Vec<String> {
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
