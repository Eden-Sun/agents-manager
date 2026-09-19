//! What a pane needs before an agent starts: hooks, shims, skills, persona and argv.

use super::*;

/// The hook / statusLine command line for a *local* bot. The token is deliberately **not**
/// on the argv (issue #43: `ps` shows every user the full command line, and the statusLine
/// runs on every redraw); the subcommands read `AM_HOOK_TOKEN` from the pane env instead.
fn hook_cmd_parts(app: &App, bot: &db::Bot, provider: &str) -> Vec<String> {
    hook_cmd_parts_for(&app.exe.to_string_lossy(), app.port, &bot.id, provider, &app.data_dir.to_string_lossy())
}

/// `--data-dir` 寫死在 argv 裡：pane env 只保護這顆 daemon 新開的 pane，舊 pane 的 env 換不掉，
/// 但 hook.sh／設定檔每次啟動都重寫，spool 才不會跑去別顆 daemon 的目錄（sol 複審二輪）。
fn hook_cmd_parts_for(exe: &str, port: u16, bot_id: &str, provider: &str, data_dir: &str) -> Vec<String> {
    vec![
        exe.into(),
        "hook".into(),
        provider.into(),
        "--bot".into(),
        bot_id.into(),
        "--port".into(),
        port.to_string(),
        "--data-dir".into(),
        data_dir.into(),
    ]
}

/// `agents-managerd hook claude …` → `agents-managerd statusline …`（同一套旗標）。
fn statusline_parts(mut hook: Vec<String>) -> Vec<String> {
    hook[1] = "statusline".into();
    hook.remove(2);
    hook
}

/// What goes in `hook.sh`'s third argv slot. The script ignores it; the real `hook_token` is
/// never put on a remote command line (review 2026-09-12 #8).
pub const REMOTE_TOKEN_SLOT: &str = "-";

/// SPEC §11.4 — the POSIX sh hook for remote hosts. Payload goes to the bot's spool; state goes
/// to this machine's herdr (`pane report-agent`), whose event makes the daemon drain the spool (§11.4.3).
///
/// 根目錄是參數（`remote_hook_sh`）：隔離實例的事件要寫進自己的 `instances/<slug>`，否則會落進正式實例的
/// spool，而它自己的 scanner 永遠看不到（sol 三輪）。正式實例（`REMOTE_ROOT`）的 spool 路徑與行為不變。
pub fn remote_hook_sh(root: &str) -> String {
    REMOTE_HOOK_SH_TEMPLATE.replace("__AM_REMOTE_ROOT__", root)
}

const REMOTE_HOOK_SH_TEMPLATE: &str = r#"#!/bin/sh
PROVIDER="$1"; BOT="$2"; TOKEN="$3"; shift 3
LIMIT=1048576
DIR="$HOME/__AM_REMOTE_ROOT__/bots/$BOT"
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
# The run this CLI process was started for: `--resume` makes an old and a new process report the
# same session, so this is how the daemon tells their late hooks apart (issue #92). Only id-safe
# characters survive, so an odd value can never break the JSON line.
RUN=$(printf '%s' "${AM_RUN_ID:-}" | tr -cd 'A-Za-z0-9_-')
BODY=$(printf '{"bot_id":"%s","provider":"%s","payload":%s,"received_at":"%s","truncated":%s,"run_id":"%s"}' "$BOT" "$PROVIDER" "$PAYLOAD" "$NOW" "$TRUNC" "$RUN")
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
    # 失敗收尾的回合一樣不再是 working（issue #79）。`"Stop"` 那個 pattern 帶了收尾的引號，配不到 StopFailure。
    case "$PAYLOAD" in *'"hook_event_name":"StopFailure"'*|*'"hook_event_name": "StopFailure"'*) STATE=idle ;; esac
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

/// grok 會合併 `hooks/*.json` 全部檔案，所以每個實例一份檔、各自指向自己的 dispatcher，
/// 不會互相覆寫。正式實例沿用原檔名。
pub fn grok_hooks_file(instance: Option<&str>) -> String {
    match instance {
        Some(slug) => format!("agents-manager-{slug}.json"),
        None => GROK_HOOKS_FILE.to_string(),
    }
}

/// 每個實例的 dispatcher 都會被 grok 叫到；只處理自己實例的 pane（pane env 的 `AM_INSTANCE`）。
/// 正式實例的 pane 沒有這個變數，所以舊 pane 照舊歸正式實例。
fn instance_gate(instance: Option<&str>) -> String {
    match instance {
        Some(slug) => format!("[ \"${{AM_INSTANCE:-}}\" = {} ] || exit 0\n", sh_quote(slug)),
        None => "[ -z \"${AM_INSTANCE:-}\" ] || exit 0\n".to_string(),
    }
}

/// Dispatcher installed on remote hosts: forwards to the per-bot `hook.sh` (SPEC §11.4).
/// 根目錄與實例閘門都跟著實例走：兩顆 daemon 管同一台遠端、bot id 又一樣時，不能共用同一份 spool。
pub fn remote_grok_dispatch_sh(root: &str, instance: Option<&str>) -> String {
    format!(
        "#!/bin/sh\n# agents-manager grok dispatcher (SPEC §12). Installed by the daemon; no-op outside daemon panes.\n[ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0\n{gate}H=\"$HOME/{root}/bots/$AM_BOT_ID/hook.sh\"\n[ -x \"$H\" ] || exit 0\nexec \"$H\" grok \"$AM_BOT_ID\" \"$AM_HOOK_TOKEN\"\n",
        gate = instance_gate(instance),
    )
}

fn local_grok_dispatch_sh(exe: &str, data_dir: &str, instance: Option<&str>) -> String {
    format!(
        "#!/bin/sh\n# agents-manager grok dispatcher (SPEC §12). Rewritten by the daemon on every grok bot start; no-op outside daemon panes.\n[ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0\n{gate}exec {exe} hook grok --bot \"$AM_BOT_ID\" --token \"$AM_HOOK_TOKEN\" --port \"${{AM_PORT:-7788}}\" --data-dir {data_dir}\n",
        gate = instance_gate(instance),
        exe = sh_quote(exe),
        data_dir = sh_quote(data_dir)
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
    let instance = app.instance();
    let a = write_if_changed(
        &dispatcher,
        &local_grok_dispatch_sh(&exe, &app.data_dir.to_string_lossy(), instance.as_deref()),
        true,
    )?;
    let hooks_path =
        std::path::PathBuf::from(grok_home(env, &home)).join("hooks").join(grok_hooks_file(instance.as_deref()));
    let b = write_if_changed(&hooks_path, &grok_hooks_json(&dispatcher.to_string_lossy()), false)?;
    if a || b {
        tracing::info!(dispatcher = %dispatcher.display(), hooks = %hooks_path.display(), "grok hook installed");
    }
    Ok(())
}

/// Remote grok bot: the same two files, written over ssh after `install_remote_hook`.
async fn install_remote_grok_hook(conn: &HostConn, env: &Value, instance: Option<&str>) -> anyhow::Result<()> {
    let home = conn.home().await?;
    let root = crate::startup::remote_root_for(instance);
    // 按實例分址：共用一個 dispatcher 的話，兩顆 daemon 會互相把它改寫成指向自己（sol 三輪）。
    let dispatcher = format!("{home}/{root}/{GROK_DISPATCH_SH}");
    let hooks_dir = format!("{}/hooks", grok_home(env, &home));
    let script = format!(
        "set -e\nW={w}\nmkdir -p \"$(dirname \"$W\")\"\ncat > \"$W\" <<'AM_WRAP_EOF'\n{wrap}AM_WRAP_EOF\nchmod +x \"$W\"\nG={g}\nmkdir -p \"$G\"\ncat > \"$G/{file}\" <<'AM_JSON_EOF'\n{json}\nAM_JSON_EOF\nprintf 'AM_GROK_INSTALLED\\n'\n",
        w = sh_quote(&dispatcher),
        wrap = remote_grok_dispatch_sh(&root, instance),
        g = sh_quote(&hooks_dir),
        file = grok_hooks_file(instance),
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

/// 沒有 `App` 在手的呼叫端（`stop.rs` 的清目錄）用行程層級的實例名；`App::instance` 也是從它初始化的。
pub async fn remote_bot_dir(conn: &HostConn, bot_id: &str) -> anyhow::Result<RemoteHookPaths> {
    remote_bot_dir_for(conn, bot_id, crate::startup::instance().as_deref()).await
}

pub async fn remote_bot_dir_for(conn: &HostConn, bot_id: &str, instance: Option<&str>) -> anyhow::Result<RemoteHookPaths> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    let home = conn.home().await?;
    // 隔離實例在遠端也要有自己的根（`instances/<slug>`），否則同 id 的 bot 會共用 spool。
    let dir = format!("{home}/{}/bots/{bot_id}", crate::startup::remote_root_for(instance));
    Ok(RemoteHookPaths { hook_sh: format!("{dir}/hook.sh"), settings: format!("{dir}/claude-settings.json"), dir })
}

/// SPEC §11.4 — push `hook.sh` (+ `claude-settings.json`) to the remote before `agent.start`.
async fn install_remote_hook(conn: &HostConn, bot: &db::Bot, instance: Option<&str>) -> anyhow::Result<RemoteHookPaths> {
    let p = remote_bot_dir_for(conn, &bot.id, instance).await?;
    // Token slot is `-` (review 2026-09-12 #8): `hook.sh` never reads it and the real key opens
    // `/relay/announce` + `/hook/*`. Kept positional for older agents.
    let cmd = shell_join(&[p.hook_sh.clone(), "claude".into(), bot.id.clone(), REMOTE_TOKEN_SLOT.into()]);
    // `hook.sh statusline` writes `hook-status.json` (§11.4.5), then execs the user's own statusLine.
    let statusline = shell_join(&[p.hook_sh.clone(), "statusline".into(), bot.id.clone(), REMOTE_TOKEN_SLOT.into()]);
    // 遠端也用同一支：`remoteControlAtStartup` 要明講，否則那台機器帳號的全域設定會替每顆 bot
    // 決定要不要開手機入口（見 `claude_settings`）。
    let settings = claude_settings(&cmd, &statusline, bot.args().iter().any(|a| a == "--remote-control"), instruction_files_of(bot));
    let settings_text = serde_json::to_string_pretty(&settings)?;
    let script = format!(
        "set -e\nD={dir}\nmkdir -p \"$D\"\ncat > \"$D/hook.sh\" <<'AM_HOOK_EOF'\n{hook}AM_HOOK_EOF\nchmod +x \"$D/hook.sh\"\ncat > \"$D/claude-settings.json\" <<'AM_SETTINGS_EOF'\n{settings}\nAM_SETTINGS_EOF\nprintf 'AM_INSTALLED\\n'\n",
        dir = sh_quote(&p.dir),
        // 腳本裡寫的根目錄與上面的安裝位置同一個：事件才會進這個實例自己的 spool。
        hook = remote_hook_sh(&crate::startup::remote_root_for(instance)),
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
///
/// The `cargo` build-scheduler shim (issue #90) rides along into the **same** dir — one PATH
/// prepend covers both. Its own install is best-effort and never blocks the bot on failure: a bot
/// that cannot get a scheduled `cargo` still gets a working `herdr`, which is what actually gates
/// starting at all.
pub(crate) async fn install_shim(app: &Arc<App>, bot: &db::Bot, project: &db::Project) -> Option<String> {
    let installed = if project.host == LOCAL_HOST {
        app.bot_dir(&bot.id).and_then(|dir| {
            let bin = crate::herdr_shim::install_local(&dir)?;
            if let Err(e) = crate::cargo_shim::install_local(&dir) {
                tracing::warn!(bot = %bot.name, error = ?e, "could not install the cargo build-slot shim");
            }
            Ok::<_, anyhow::Error>(bin.to_string_lossy().into_owned())
        })
    } else {
        match app.hosts.get(&project.host).await {
            Some(conn) => match remote_bot_dir_for(&conn, &bot.id, app.instance().as_deref()).await {
                Ok(p) => {
                    let dir = crate::herdr_shim::install_remote(&conn, &p.dir).await;
                    if dir.is_ok() {
                        if let Err(e) = crate::cargo_shim::install_remote(&conn, &p.dir).await {
                            tracing::warn!(bot = %bot.name, host = %project.host, error = ?e, "could not install the remote cargo build-slot shim");
                        }
                    }
                    dir
                }
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
        let paths = install_remote_hook(&conn, bot, app.instance().as_deref()).await?;
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
                install_remote_grok_hook(&conn, env, app.instance().as_deref()).await?;
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
            let statusline = shell_join(&statusline_parts(hook_cmd_parts(app, bot, "claude")));
            // Remote Control 明講，不要靠帳號的全域 settings 決定（見 `claude_settings`）。
            let wants_remote = bot.args().iter().any(|a| a == "--remote-control");
            let settings = claude_settings(&cmd, &statusline, wants_remote, instruction_files_of(bot));
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
/// 每顆 claude bot 自己的 `claude-settings.json`。
///
/// `remoteControlAtStartup` 明講而不是省略：使用者帳號的全域 `settings.json` 只要開了它，
/// **每一顆** bot 起來都會多開一個手機入口（SPEC §18.15 說入口只有巡檢一個，背景 worker 一律
/// rc off）。判準是這顆 bot 自己有沒有要求——argv 帶了 `--remote-control` 才是 true。
///
/// `instruction_files` 是 `agents-md` plugin 的選項值，只能是 `config::INSTRUCTION_FILES` 裡的一個
/// （由 [`instruction_files_of`] 給；CLI 遇到選項以外的值會退回它自己的預設，等於沒釘）。
fn claude_settings(hook_cmd: &str, statusline: &str, wants_remote: bool, instruction_files: &str) -> Value {
    json!({
        "remoteControlAtStartup": wants_remote,
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": hook_cmd}]}],
            "Stop": [{"hooks": [{"type": "command", "command": hook_cmd}]}],
            // 回合**失敗**收尾（API／auth／額度…）也是一級訊號，不是只有答完才算結束（issue #79）。
            // 沒有它的話，失敗的回合要等 §4.3 備援或 stuck watchdog 才被發現，中間一直掛在 in_flight。
            // 舊版 claude 不認得這個鍵就忽略它，不影響既有兩個 hook。
            "StopFailure": [{"hooks": [{"type": "command", "command": hook_cmd}]}],
            // issue #82：claude 原生的 in-process Task 工具子代理的第二路訊號（`agent_id`／
            // `agent_type`／`agent_transcript_path`），純可見性，寫進 `runs.subagent_json`——**不是**
            // §6.5a 血緣認領要的那種子 agent：這裡的「子代理」是同一個行程裡的 Task 工具呼叫，沒有自己
            // 的 pane；AGM 的 child bot（`herdr pane split` 開出來的獨立 pane）本來就沒有 hook（§4.3），
            // 這兩個鍵永遠不會替 child bot 觸發，也就不可能影響哪個 pane 歸誰。
            "SubagentStart": [{"hooks": [{"type": "command", "command": hook_cmd}]}],
            "SubagentStop": [{"hooks": [{"type": "command", "command": hook_cmd}]}],
            // issue #94：這顆 bot 自己的 Bash 工具跑 `herdr pane split`／`agent start` 時，那條指令的
            // stdout 就是 herdr 自己回的 JSON（`{"id":"cli:pane:split"/"cli:agent:start",
            // "result":{...,"pane_id":...}}`），daemon 讀得到「這個 pane 是我剛剛開的」這個事實，
            // 比 §6.5a 的同 tab／名字前綴推斷更早、更精確（`spawn_hints.rs`）。`matcher: "Bash"` 只在
            // 跑 shell 指令時觸發，不是每個工具呼叫都送一次。
            "PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": hook_cmd}]}]
        },
        "statusLine": {"type": "command", "command": statusline},
        // Trial: shorter replies scrape cleaner from the terminal (§4.3) and read better in 對話.
        "outputStyle": "Concise",
        // `--dangerously-skip-permissions` still asks 「Bypass Permissions mode … Yes, I accept」
        // once per config dir; this is the record accepting it writes (2026-09-08).
        "skipDangerousModePermissionPrompt": true,
        // 使用者 2026-09-15：每顆 bot 的 CLI 時間統一台北時間 24 小時制（claude 2.1.257 起的設定；本機、遠端都一樣）。
        "timeFormat": "24-hour",
        "timeZone": "Asia/Taipei",
        // issue #78：撞到用量上限時 claude 自己排一個「continuing automatically at HH:MM」，daemon 完全
        // 不知道，而且那個自動續跑被取消時（畫面變 `Automatic continue cancelled`）沒有人接手，整顆卡死
        // 到有人手動 `/rate-limit-options` 重新掛上。managed pane 的 Turn 該由誰接回去是 daemon 的
        // resend／排隊機制（`stuck_turns.rs`、queue timer）決定，不該讓 CLI 自己另開一條線。
        //
        // 這不是 `CLAUDE_CODE_RESUME_INTERRUPTED_TURN`（那是完全不同的子系統：cloud/remote worker
        // epoch 之間搬 session 用的環境變數，字串表裡緊跟著 `host_draining`／`container_recreated`／
        // `checkpoint_restore`，跟本機 pane 的用量上限自動續跑無關，關了也不會影響這裡）。真正管這個
        // 行為的是 settings.json 的 `autoContinueAtUsageLimit`（`/config` 裡的「Continue automatically
        // at usage limit」，claude 2.1.234 起存在），關掉之後撞到上限會停下來、把「等」變成使用者自己選
        // 的選項，不會自己續跑。
        "autoContinueAtUsageLimit": false,
        // issue #102：claude 2.1.275 起會把「你 claude.ai 帳號上開啟的 skills／plugins」同步進用同一個帳號
        // 登入的終端 session。managed pane 的工具集必須由 daemon 決定，理由跟上面那條同一條：
        //   1. 同步進來的東西 daemon 不知情，同一顆 bot 在不同時間會跑出不同行為，出事無從重現；
        //   2. skills 會吃 context，而 §4.4a 的 context／額度判斷都假設環境是 daemon 決定的；
        //   3. 帳號是共用的（cc0／cc1／cc2…），一個人在 claude.ai 上開一個 skill 會同時改掉所有用那個帳號的 bot。
        // 只寫進 daemon 注入的 `--settings` 檔，使用者自己終端的 `~/.claude*/settings.json` 不受影響；
        // 2.1.275 以前的 claude 不認得這兩個鍵，一律靜靜忽略。
        "syncClaudeAiSkills": false,
        "syncClaudeAiPlugins": false,
        // issue #206：claude 2.1.277 起，專案沒有 CLAUDE.md 時改讀 AGENTS.md——內建 plugin `agents-md` 的 `instructionFiles`
        // （`/config` 裡的「Project instructions」），預設 `claude-md-or-agents-md`，開不開由伺服器端旗標 `tengu_agents_md_mod`
        // 放量（同一台機器上 2.1.276 開著、2.1.278 關著）。同一個 project 常同時有 codex bot，`AGENTS.md` 是寫給 codex 的：
        // bot 讀哪份指示檔要由 daemon 決定，不因 CLI 升級或放量悄悄換檔——釘在 `claude-md`（跟 2.1.277 以前一樣只讀 CLAUDE.md）。
        // plugin 的選項只從 user／`--settings`／managed settings 讀（專案層的 settings 不讀），鍵認 `agents-md` 與 `agents-md@builtin`。
        // 2.1.276 的舊選項 `projectInstructions` 預設本來就是只讀 CLAUDE.md，不另外寫（新版兩個都寫會印一行提示）；更舊的
        // 版本沒有這個 plugin，這一格沒人讀、不影響啟動。
        //
        // issue #213：值由 bot 決定（`bots.instruction_files`，沒設＝`claude-md`），不是全域開關——要讓某顆 claude 跟同 project 的
        // codex 共用 AGENTS.md 就在那顆 bot 上設 `claude-md-and-agents-md`（或 `claude-md-or-agents-md`）。
        "pluginConfigs": {"agents-md@builtin": {"options": {"instructionFiles": instruction_files}}}
    })
}

/// 這顆 bot 的 `instructionFiles`：`bots.instruction_files`，沒設或不在 CLI 選項裡就是釘住的 `claude-md`。
fn instruction_files_of(bot: &db::Bot) -> &'static str {
    crate::config::effective_instruction_files(bot.instruction_files.as_deref())
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
    // §6.5e：bot 開的 pane 要落在自己 project 的 workspace。shim 在 `tab create` 沒指定時先問 herdr 母 pane 現在的
    // workspace，問不到才補這個值——它是 workspace 決定**之前**的舊映射，第一次啟動沒有、映射失效時是死的（review core 7）。
    // `pane split` 以母 pane 為基準，本來就同 workspace。
    env.insert("AM_PROJECT_ID".into(), json!(bot.project_id));
    if let Ok(Some(p)) = crate::db::project(&app.db, &bot.project_id).await {
        if let Some(ws) = p.workspace_id.filter(|w| !w.trim().is_empty()) {
            env.insert("AM_WORKSPACE_ID".into(), json!(ws));
        }
    }
    if let Some(m) = bot.model.as_deref().filter(|m| !m.trim().is_empty()) {
        env.insert("AM_MODEL".into(), json!(m));
    }
    if let Some(e) = bot.effort.as_deref().filter(|e| !e.trim().is_empty()) {
        env.insert("AM_EFFORT".into(), json!(e));
    }
    if let Some(dir) = shim_dir {
    // Best effort only: the login shell's profile (`path_helper`, `brew shellenv`) pushes us back;
    // `start_inner` re-prepends in the pane's shell, which is what actually wins.
        // 別顆 bot 的 shim 目錄先清掉再接自己的（`shim_path`）：daemon 常常是從某顆 bot 的 pane
        // 裡啟動的，它的 `PATH` 前面就掛著那顆 bot 的 shim，照抄進來就是 2026-09-18 的巢狀死鎖。
        let path = match std::env::var("PATH") {
            Ok(p) if host == LOCAL_HOST => crate::shim_path::prepend_own_shim_dir(&p, dir),
            _ => format!("{dir}:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"),
        };
        env.insert("PATH".into(), json!(path));
    }
    env.insert("AM_RUN_ID".into(), json!(run_id));
    // Only local hook commands call home over HTTP; remote panes have no port since v4.3 (§11.4.6).
    if host == LOCAL_HOST {
        env.insert("AM_PORT".into(), json!(app.port.to_string()));
        // issue #104：cargo shim 只拿到 helper/config 的位置與是否啟用；SSH 密碼永遠不進 pane env。
        env.insert("AM_DAEMON_EXE".into(), json!(app.exe.to_string_lossy()));
        env.insert("AM_CONFIG_PATH".into(), json!(app.cfg.path.to_string_lossy()));
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
    let local_data_dir = (host == LOCAL_HOST).then(|| app.data_dir.to_string_lossy().into_owned());
    reserve_instance_env(&mut env, app.instance().as_deref(), local_data_dir.as_deref());
    // §6.5f：給使用者的檔案放這裡（不是 scratchpad）。跟 AM_DATA_DIR 一樣在自訂 env 合併之後才由 daemon 蓋回去：
    // 被改掉的話 bot 寫到別處，使用者在網頁上看不到。遠端主機不給——那台的檔案這台 daemon 拿不到。
    match (host == LOCAL_HOST).then(|| crate::outbox::ensure(&app.data_dir, &bot.id)).flatten() {
        Some(dir) => env.insert("AM_OUTBOX".into(), json!(dir.to_string_lossy())),
        None => env.remove("AM_OUTBOX"),
    };
    Value::Object(env)
}

/// 決定「這個 pane 屬於哪顆 daemon」的兩個變數是保留的：identity.env、bot.env 合併**之後**才由 daemon
/// 蓋回去，自訂 env 寫了也不算（sol 四輪）。否則正式 bot 可以偽造隔離 slug、隔離 bot 可以清掉 slug，
/// grok dispatcher 就把事件送錯實例；`AM_DATA_DIR` 被改掉則 hook 的 spool 落到別顆 daemon 的目錄。
/// - `AM_INSTANCE`：隔離實例＝它的 slug；正式實例＝移除（dispatcher 以「沒有這個變數」認正式實例）。
/// - `AM_DATA_DIR`：本機＝這顆 daemon 的資料目錄；遠端＝移除（遠端 bot 目錄在遠端家目錄，§11.4）。
fn reserve_instance_env(
    env: &mut serde_json::Map<String, Value>,
    instance: Option<&str>,
    local_data_dir: Option<&str>,
) {
    match instance {
        Some(slug) => env.insert("AM_INSTANCE".into(), json!(slug)),
        None => env.remove("AM_INSTANCE"),
    };
    match local_data_dir {
        Some(dir) => env.insert("AM_DATA_DIR".into(), json!(dir)),
        None => env.remove("AM_DATA_DIR"),
    };
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
分頁留著不關，RAM 就是這樣被吃光的。\n\
\n\
輸出檔案（同樣是硬規則，使用者 2026-09-16 裁示）：\n\
\n\
- **scratchpad 只放中間產物**（腳本、log、暫存資料）。scratchpad **不是**給使用者的地方，**禁止**把要交給使用者的檔案放在那裡。\n\
- **要交給使用者的檔案一律放 `$AM_OUTBOX`**（`~/.config/agents-manager/outbox/<AM_BOT_ID>/`）。寫之前**必須先** `mkdir -p \"$AM_OUTBOX\"`（空目錄會被清掉）。\
放進去 **1 小時後由 AGM 自動刪除**；要長期保留的放 repo 或 `reports/`。`$AM_OUTBOX` 沒有值（遠端主機）時**禁止**改放 scratchpad，直接在對話裡講清楚檔案在哪台機器的哪個路徑。\n\
- **私鑰、憑證、DB 一律禁止放進 scratchpad 或 `$AM_OUTBOX`**（`.pem`、`.key`、`.p12`、`.env`、`*.sqlite*`、`*.db`、DB 複本、瀏覽器 profile）。\
驗證要用 DB 複本時，做完**必須當下刪掉**。"
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
    use super::{claude_settings, hook_cmd_parts_for, write_private};

    /// issue #79：claude 的 `--settings` 要訂 `StopFailure`，daemon 才收得到「回合失敗收尾」的原生訊號；
    /// 既有的兩個 hook 一個都不能掉，三個都指向同一支 hook 指令（分類在 daemon 裡做）。
    #[test]
    fn a_claude_bot_subscribes_to_stop_failure_as_well_as_stop() {
        let v = claude_settings("/usr/bin/agents-managerd hook claude --bot b1", "/usr/bin/agents-managerd statusline", false, "claude-md");
        let hooks = v.get("hooks").and_then(|h| h.as_object()).expect("hooks");
        let mut names: Vec<&String> = hooks.keys().collect();
        names.sort();
        assert_eq!(
            names,
            vec!["PostToolUse", "SessionStart", "Stop", "StopFailure", "SubagentStart", "SubagentStop"],
            "{hooks:?}"
        );
        for name in ["SessionStart", "Stop", "StopFailure", "SubagentStart", "SubagentStop", "PostToolUse"] {
            let cmd = hooks[name][0]["hooks"][0]["command"].as_str().unwrap_or_default();
            assert!(cmd.contains("hook claude"), "{name} 要指向同一支 hook 指令：{cmd}");
        }
    }

    /// 遠端走 `hook.sh`：失敗收尾的回合一樣不再是 working，要向 herdr 報 idle（`"Stop"` 那個
    /// pattern 帶了收尾的引號，配不到 `StopFailure`）。
    #[test]
    fn the_remote_dispatcher_reports_idle_for_a_failed_turn_too() {
        assert!(
            super::REMOTE_HOOK_SH_TEMPLATE.contains(r#"*'"hook_event_name":"StopFailure"'*"#),
            "hook.sh 少了 StopFailure 那一條",
        );
    }

    /// Issue #43: the hook / statusLine command line must not carry the token.
    #[test]
    fn hook_cmd_parts_has_no_token() {
        let parts = hook_cmd_parts_for("/usr/bin/agents-managerd", 7788, "b1", "claude", "/tmp/am-iso");
        assert_eq!(
            parts,
            vec!["/usr/bin/agents-managerd", "hook", "claude", "--bot", "b1", "--port", "7788", "--data-dir", "/tmp/am-iso"]
        );
        assert!(!parts.iter().any(|p| p == "--token"));
    }

    /// 產生出來的 hook 與 statusLine 指令，真的丟給 daemon 自己的 CLI parser 要過（2026-09-15 回歸：hook 加了
    /// `--data-dir`，statusline 子命令不認，claude 的狀態列整個不見）。只比對字串抓不到這種事。
    #[test]
    fn the_generated_hook_and_statusline_commands_parse() {
        use clap::Parser as _;
        let hook = hook_cmd_parts_for("/usr/bin/agents-managerd", 7788, "b1", "claude", "/tmp/am-iso");
        crate::Cli::try_parse_from(&hook).unwrap_or_else(|e| panic!("hook argv 不合法：{e}\n{hook:?}"));
        let statusline = super::statusline_parts(hook);
        assert_eq!(statusline[1], "statusline");
        crate::Cli::try_parse_from(&statusline).unwrap_or_else(|e| panic!("statusline argv 不合法：{e}\n{statusline:?}"));
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
            instruction_files: None,
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

    /// §6.5f（使用者 2026-09-16 裁示）：每顆 bot 都讀得到輸出規則——claude 的 skill 與三種 kind 的 persona
    /// 都用這份文字，所以不在 agents-manager 專案裡的 bot 也拿得到。
    #[test]
    fn every_bot_is_told_where_user_files_go_and_what_never_goes_there() {
        let rule = super::child_agent_rules("proj-abc123");
        assert!(rule.contains("scratchpad 只放中間產物"), "{rule}");
        assert!(rule.contains("$AM_OUTBOX"));
        assert!(rule.contains("mkdir -p \"$AM_OUTBOX\""), "空目錄會被清掉，寫之前要自己建");
        assert!(rule.contains("1 小時"));
        assert!(rule.contains("reports/"));
        for banned in [".pem", ".key", ".sqlite", ".db", "DB 複本"] {
            assert!(rule.contains(banned), "禁放清單要列出 {banned}");
        }
        // 真的送到 bot 手上：三種 kind 的 persona 參數裡有，claude 的 skill 文件裡也有。
        let mut b = bot("claude", None, None, false);
        for kind in ["claude", "grok", "codex"] {
            b.kind = kind.into();
            let args = super::persona_args(&b, "proj-abc123").join(" ");
            assert!(args.contains("$AM_OUTBOX"), "{kind}: {args}");
        }
        let doc = super::herdr_skill_doc("---\nname: herdr\ndescription: x\n---\n# herdr\n", "proj-abc123");
        assert!(doc.contains("$AM_OUTBOX"), "{doc}");
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
            Self::with_root(with_herdr, crate::startup::REMOTE_ROOT)
        }

        /// 用哪個遠端根產生腳本（正式實例＝`REMOTE_ROOT`，隔離實例＝`instances/<slug>`）。
        fn with_root(with_herdr: bool, root: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("am-hook-{}", crate::db::ulid()));
            std::fs::create_dir_all(&dir).unwrap();
            write_exec(&dir.join("hook.sh"), &super::remote_hook_sh(root));
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
            self.run_with_env(argv, stdin, &[])
        }

        /// Same, with extra pane env on top of the fixed test environment.
        fn run_with_env(&self, argv: &[&str], stdin: &str, extra: &[(&str, &str)]) -> (String, bool) {
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
            for (k, v) in extra {
                cmd.env(k, v);
            }
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut ch = cmd.spawn().unwrap();
            ch.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
            let out = ch.wait_with_output().unwrap();
            (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
        }
    }

    const STOP: &str = r#"{"hook_event_name":"Stop","session_id":"s-1","transcript_path":"/tmp/t.jsonl","stop_hook_active":false}"#;

    /// 真的執行產生出來的腳本：spool 要落在**這個實例**的根底下（sol 三輪）。正式實例的既有路徑不變。
    #[test]
    fn the_generated_hook_spools_under_its_own_instance_root() {
        let default = super::remote_hook_sh(crate::startup::REMOTE_ROOT);
        assert!(default.contains("DIR=\"$HOME/.config/agents-manager/bots/$BOT\""), "正式實例的腳本路徑不能變");
        assert!(!default.contains("__AM_REMOTE_ROOT__"));

        for slug in [None, Some("a1b2c3d4")] {
            let root = crate::startup::remote_root_for(slug);
            let sb = Sandbox::with_root(false, &root);
            let (_, ok) = sb.run(&["claude", &sb.bot, "-"], STOP);
            assert!(ok);
            let spool = sb.dir.join(&root).join("bots").join(&sb.bot).join("hook-spool.jsonl");
            let line = std::fs::read_to_string(&spool).unwrap_or_else(|_| panic!("{slug:?}: 沒寫到 {}", spool.display()));
            assert!(line.contains("\"Stop\""), "{line}");
            if slug.is_some() {
                let prod = sb.dir.join(".config/agents-manager/bots").join(&sb.bot).join("hook-spool.jsonl");
                assert!(!prod.exists(), "隔離實例的事件跑進了正式 spool");
            }
        }
    }

    /// grok 會把每個實例的 dispatcher 都叫一次：各自只接自己實例的 pane（`AM_INSTANCE`），
    /// 同 id 的 bot 才不會被兩邊各記一次。正式實例的舊 pane 沒有這個變數，照舊歸正式。
    #[test]
    fn each_grok_dispatcher_only_serves_its_own_instance() {
        let home = std::env::temp_dir().join(format!("am-grok-{}", crate::db::ulid()));
        let run = |slug: Option<&str>, pane_instance: Option<&str>| -> bool {
            let root = crate::startup::remote_root_for(slug);
            let bot_dir = home.join(&root).join("bots/b1");
            std::fs::create_dir_all(&bot_dir).unwrap();
            let marker = bot_dir.join("called");
            let _ = std::fs::remove_file(&marker);
            write_exec(&bot_dir.join("hook.sh"), &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
            let disp = home.join(format!("{}-dispatch.sh", slug.unwrap_or("default")));
            write_exec(&disp, &super::remote_grok_dispatch_sh(&root, slug));
            let mut cmd = Command::new("/bin/sh");
            cmd.arg(&disp).env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &home);
            cmd.env("AM_BOT_ID", "b1").env("AM_HOOK_TOKEN", "t");
            if let Some(i) = pane_instance {
                cmd.env("AM_INSTANCE", i);
            }
            assert!(cmd.status().unwrap().success());
            marker.exists()
        };
        assert!(run(None, None), "正式實例接沒有 AM_INSTANCE 的 pane（含升級前開的舊 pane）");
        assert!(!run(None, Some("a1b2")), "正式實例不碰隔離實例的 pane");
        assert!(run(Some("a1b2"), Some("a1b2")));
        assert!(!run(Some("a1b2"), None), "隔離實例不碰正式實例的 pane");
        assert!(!run(Some("a1b2"), Some("zzzz")));

        assert_eq!(super::grok_hooks_file(None), super::GROK_HOOKS_FILE, "正式實例檔名不變");
        assert_eq!(super::grok_hooks_file(Some("a1b2")), "agents-manager-a1b2.json");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// issue #92：遠端的 hook 也要帶這個行程自己的 run id（pane env 的 `AM_RUN_ID`），daemon 才分得出
    /// `--resume` 前後兩個行程送的同一個 session。怪字元一律濾掉：spool 裡一行壞掉的 JSON 會永遠卡在那裡。
    #[test]
    fn the_remote_hook_names_the_run_its_process_was_started_for() {
        let parse = |line: &str| -> crate::hookrecv::HookBody { serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")) };
        let sb = Sandbox::new(false);
        let (_, ok) = sb.run_with_env(&["claude", &sb.bot, "tok"], STOP, &[("AM_RUN_ID", "01RUNREMOTE")]);
        assert!(ok);
        let (_, ok) = sb.run_with_env(&["claude", &sb.bot, "tok"], STOP, &[("AM_RUN_ID", "01RUN\"$(touch pwned)\\x")]);
        assert!(ok);
        let (_, ok) = sb.run(&["claude", &sb.bot, "tok"], STOP);
        assert!(ok);
        let spool = sb.read("hook-spool.jsonl");
        let lines: Vec<&str> = spool.lines().collect();
        assert_eq!(lines.len(), 3, "{spool}");
        assert_eq!(parse(lines[0]).run_id.as_deref(), Some("01RUNREMOTE"));
        assert_eq!(parse(lines[1]).run_id.as_deref(), Some("01RUNtouchpwnedx"), "引號、反斜線、括號都被濾掉，JSON 還是好的");
        assert_eq!(parse(lines[2]).run_id.as_deref().map(str::trim), Some(""), "沒有 AM_RUN_ID：空字串，圍籬當成沒帶");
    }

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

#[cfg(test)]
mod pane_env_tests {
    use super::*;
    use crate::testing as tt;

    /// 正式／隔離 × 本機／遠端 × 自訂 env 帶了偽造值、清空值、完全沒帶：結果只看 daemon 自己。
    #[test]
    fn reserved_instance_keys_ignore_whatever_custom_env_says() {
        for instance in [None, Some("a1b2")] {
            for local in [Some("/data/iso"), None] {
                for custom in [Some("forged"), Some(""), None] {
                    let mut env = serde_json::Map::new();
                    if let Some(v) = custom {
                        env.insert("AM_INSTANCE".into(), json!(v));
                        env.insert("AM_DATA_DIR".into(), json!(v));
                    }
                    reserve_instance_env(&mut env, instance, local);
                    let case = format!("instance={instance:?} local={local:?} custom={custom:?}");
                    assert_eq!(env.get("AM_INSTANCE"), instance.map(|s| json!(s)).as_ref(), "{case}");
                    assert_eq!(env.get("AM_DATA_DIR"), local.map(|s| json!(s)).as_ref(), "{case}");
                }
            }
        }
    }

    /// §6.5f：本機 pane 拿到自己的 outbox（啟動時就建好），自訂 env 搬不走；遠端沒有。
    #[tokio::test]
    async fn a_local_pane_gets_its_own_outbox_that_custom_env_cannot_move() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let want = env.app.data_dir.join("outbox").join(&bot.id);
        let e = pane_env(&env.app, &bot, LOCAL_HOST, "run-1", "proj-alfa", None).await;
        assert_eq!(e["AM_OUTBOX"], json!(want.to_string_lossy()));
        assert!(want.is_dir(), "啟動時就建好");
        assert!(pane_env(&env.app, &bot, "box", "run-1", "proj-alfa", None).await.get("AM_OUTBOX").is_none(), "遠端沒有");

        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?")
            .bind(r#"{"AM_OUTBOX":"/elsewhere","FOO":"kept"}"#)
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let e = pane_env(&env.app, &bot, LOCAL_HOST, "run-1", "proj-alfa", None).await;
        assert_eq!(e["AM_OUTBOX"], json!(want.to_string_lossy()), "bot.env 蓋不過去");
        assert_eq!(e["FOO"], json!("kept"));
        assert!(pane_env(&env.app, &bot, "box", "run-1", "proj-alfa", None).await.get("AM_OUTBOX").is_none(), "遠端也不留自訂的假路徑");
    }

    /// hook 打不通時會 spool 到 `AM_DATA_DIR`；沒注入的話隔離跑的 bot 會把檔案丟進正式資料目錄，
    /// 換成正式 daemon 去重播它（sol 複審 2026-09-14）。
    #[tokio::test]
    async fn a_local_pane_learns_the_daemons_data_dir() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let e = pane_env(&env.app, &bot, LOCAL_HOST, "run-1", "proj-alfa", None).await;
        assert_eq!(e["AM_DATA_DIR"], json!(env.app.data_dir.to_string_lossy()));
        assert_ne!(e["AM_DATA_DIR"], json!(""));

        // 遠端 pane 的 bot 目錄在遠端家目錄，注入本機路徑只會誤導（§11.4）。
        let remote = pane_env(&env.app, &bot, "box", "run-1", "proj-alfa", None).await;
        assert!(remote.get("AM_DATA_DIR").is_none());

        // 正式實例不設 AM_INSTANCE（舊 pane 也沒有，兩者一致）；隔離實例本機、遠端都要帶。
        assert!(e.get("AM_INSTANCE").is_none());
        env.app.set_instance(Some("a1b2".into()));
        for host in [LOCAL_HOST, "box"] {
            let iso = pane_env(&env.app, &bot, host, "run-1", "proj-alfa", None).await;
            assert_eq!(iso["AM_INSTANCE"], json!("a1b2"), "{host}");
        }

        // identity.env、bot.env 各自寫了偽造值都蓋不過去：真的走 pane_env 的合併順序。
        env.app
            .cfg
            .update(|cfg| {
                // 這個測試測的是 pane_env 的合併順序，不是身分的 host 範圍：兩台各放一份同名的，
                // 本機那份不寫 host（＝現行 config.toml 的形狀），遠端那份明寫 host（SPEC §16.2）。
                let forged = || -> std::collections::BTreeMap<String, String> {
                    [("AM_INSTANCE", "forged-by-identity"), ("AM_DATA_DIR", "/identity/dir"), ("ID_ONLY", "kept")]
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .into()
                };
                cfg.identities = vec![
                    crate::config::IdentityCfg {
                        name: "cc9".into(),
                        kind: "claude".into(),
                        host: None,
                        env: forged(),
                        args: vec![],
                    },
                    crate::config::IdentityCfg {
                        name: "cc9".into(),
                        kind: "claude".into(),
                        host: Some("box".into()),
                        env: forged(),
                        args: vec![],
                    },
                ];
                Ok(())
            })
            .await
            .unwrap();
        for layer in ["identity", "bot"] {
            let (identity, env_json) = match layer {
                "identity" => (Some("cc9"), r#"{"FOO":"kept"}"#),
                _ => (None, r#"{"AM_INSTANCE":"forged-by-bot","AM_DATA_DIR":"/bot/dir","FOO":"kept"}"#),
            };
            sqlx::query("UPDATE bots SET identity = ?, env_json = ? WHERE id = ?")
                .bind(identity)
                .bind(env_json)
                .bind(&bot.id)
                .execute(&env.app.db)
                .await
                .unwrap();
            let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
            for instance in [None, Some("a1b2".to_string())] {
                env.app.set_instance(instance.clone());
                for host in [LOCAL_HOST, "box"] {
                    let got = pane_env(&env.app, &bot, host, "run-1", "proj-alfa", None).await;
                    let case = format!("layer={layer} instance={instance:?} host={host}");
                    assert_eq!(got.get("AM_INSTANCE"), instance.as_ref().map(|s| json!(s)).as_ref(), "{case}");
                    let dir = (host == LOCAL_HOST).then(|| json!(env.app.data_dir.to_string_lossy()));
                    assert_eq!(got.get("AM_DATA_DIR"), dir.as_ref(), "{case}");
                    assert_eq!(got["FOO"], json!("kept"), "其他自訂 env 照舊生效：{case}");
                    if layer == "identity" {
                        assert_eq!(got["ID_ONLY"], json!("kept"), "{case}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod claude_settings_tests {
    use super::*;

    /// 2026-09-14 review：協調者的 rc off 只靠 `args=[]`，但帳號的全域設定可以把它打開。
    /// 現在每顆 bot 的設定檔都明講，且只有自己 argv 要求過才是 true。
    #[test]
    fn remote_control_is_stated_per_bot_not_inherited_from_the_account() {
        let off = claude_settings("hook", "sl", false, "claude-md");
        assert_eq!(off["remoteControlAtStartup"], json!(false));
        let on = claude_settings("hook", "sl", true, "claude-md");
        assert_eq!(on["remoteControlAtStartup"], json!(true));
        for v in [&off, &on] {
            assert_eq!(v["statusLine"]["command"], "sl");
            assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "hook");
            assert_eq!(v["outputStyle"], "Concise");
            assert_eq!(v["skipDangerousModePermissionPrompt"], json!(true));
            assert_eq!(v["timeFormat"], "24-hour");
            assert_eq!(v["timeZone"], "Asia/Taipei");
        }
    }

    /// issue #78：managed pane 不讓 claude 自己排「撞到用量上限就自動續跑」——那條線改由 daemon 的
    /// resend／排隊機制接手（`stuck_turns.rs`）。這一個鍵是 `/config` 裡「Continue automatically at
    /// usage limit」的設定檔對應（`autoContinueAtUsageLimit`，claude 2.1.234 起存在），不是
    /// `CLAUDE_CODE_RESUME_INTERRUPTED_TURN`（那是不相干的 cloud worker 續傳環境變數）。
    #[test]
    fn managed_panes_do_not_let_claude_auto_continue_past_a_usage_limit() {
        for wants_remote in [false, true] {
            let v = claude_settings("hook", "sl", wants_remote, "claude-md");
            assert_eq!(v["autoContinueAtUsageLimit"], json!(false), "wants_remote={wants_remote}");
        }
    }

    /// issue #102：claude 2.1.275 把 claude.ai 帳號上啟用的 skills／plugins 同步進終端 session。managed
    /// pane 的工具集由 daemon 決定，不讓 CLI 接一條我們看不到的線——帳號是共用的，一個人在網站上開一個
    /// skill 會同時改掉所有用那個帳號的 bot，而且同步進來的 skills 會吃掉 §4.4a 在算的 context。
    #[test]
    fn managed_panes_do_not_sync_skills_or_plugins_from_the_claude_ai_account() {
        for wants_remote in [false, true] {
            let v = claude_settings("hook", "sl", wants_remote, "claude-md");
            assert_eq!(v["syncClaudeAiSkills"], json!(false), "wants_remote={wants_remote}");
            assert_eq!(v["syncClaudeAiPlugins"], json!(false), "wants_remote={wants_remote}");
        }
    }

    /// issue #206／#213：claude 2.1.277 起，沒有 CLAUDE.md 的專案會改讀 AGENTS.md（寫給同一個 project 裡 codex bot 的那份）。
    /// managed pane 讀哪份指示檔由 daemon 決定：`agents-md` plugin 的 `instructionFiles` 寫這顆 bot 的值，值必須是 CLI 認得的
    /// 那幾個之一——認不得的值 CLI 會退回預設（`claude-md-or-agents-md`），等於沒釘。沒設的 bot 是 `claude-md`
    /// （bot 這一層的接線與預設值由 `api.rs` 的 `instruction_files_tests` 從建 bot 一路驗到 `--settings` 檔）。
    #[test]
    fn managed_panes_write_the_instruction_files_they_are_given() {
        // 2.1.277／2.1.278 binary 裡 `instructionFiles` 的 options。
        const KNOWN: [&str; 4] = ["claude-md", "claude-md-or-agents-md", "claude-md-and-agents-md", "managed-only"];
        for wants_remote in [false, true] {
            for want in KNOWN {
                let v = claude_settings("hook", "sl", wants_remote, want);
                let options = &v["pluginConfigs"]["agents-md@builtin"]["options"];
                assert_eq!(options["instructionFiles"], json!(want), "wants_remote={wants_remote}");
                assert!(KNOWN.contains(&options["instructionFiles"].as_str().unwrap()));
                assert!(options.get("projectInstructions").is_none(), "舊選項不寫：新版兩個都有時會在畫面上提示一行 (wants_remote={wants_remote})");
            }
        }
    }

    /// issue #82：native `SubagentStart`／`SubagentStop` 走同一支 hook 指令，跟 `SessionStart`／`Stop`
    /// 一樣——`hook_cmd.rs` 是通用轉發，不分事件名字（`payload comes from stdin`），不需要另外的旗標。
    #[test]
    fn subagent_lifecycle_hooks_are_registered_on_the_same_command() {
        let v = claude_settings("hook", "sl", false, "claude-md");
        for event in ["SubagentStart", "SubagentStop"] {
            assert_eq!(v["hooks"][event][0]["hooks"][0]["command"], "hook", "{event}");
        }
    }

    /// issue #94：`PostToolUse` 只在 Bash 工具觸發（`matcher`），不是每個工具呼叫都送一次——那樣會把
    /// Read／Edit／Grep 這些跟子 pane 完全無關的呼叫也送進 daemon，白白增加流量。
    #[test]
    fn post_tool_use_only_matches_the_bash_tool() {
        let v = claude_settings("hook", "sl", false, "claude-md");
        assert_eq!(v["hooks"]["PostToolUse"][0]["matcher"], json!("Bash"));
        assert_eq!(v["hooks"]["PostToolUse"][0]["hooks"][0]["command"], "hook");
    }
}
