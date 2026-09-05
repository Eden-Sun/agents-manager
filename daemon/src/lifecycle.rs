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
        let host = db::bot_host(&app.db, &run.bot_id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
        crate::events::unwatch_pane(app, &host, p).await;
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
if [ "$PROVIDER" = "codex" ]; then PAYLOAD="$1"; else PAYLOAD=$(head -c 1048576); fi
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
BODY=$(printf '{"bot_id":"%s","provider":"%s","payload":%s,"received_at":"%s","truncated":false}' "$BOT" "$PROVIDER" "$PAYLOAD" "$NOW")
DIR="$HOME/.config/agents-manager/bots/$BOT"
mkdir -p "$DIR"
OUT=$(NO_PROXY=127.0.0.1 curl -s -m 2 --connect-timeout 0.3 -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$PORT/hook/$PROVIDER" \
  -H 'Content-Type: application/json' -H "X-AM-Bot-Token: $TOKEN" --data-binary "$BODY" 2>>"$DIR/hook.log") || OUT=fail
case "$OUT" in 2*) ;; *) printf '%s\n' "$BODY" >> "$DIR/hook-spool.jsonl";; esac
exit 0
"#;

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
    let settings = json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": cmd}]}],
            "Stop": [{"hooks": [{"type": "command", "command": cmd}]}]
        }
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
async fn injected_args(app: &App, bot: &db::Bot, project: &db::Project) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    if bot.auto_approve != 0 {
        match bot.kind.as_str() {
            "claude" => out.push("--dangerously-skip-permissions".into()),
            "codex" => out.push("--yolo".into()),
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
            // v3: no Notification hook; Stop with stop_hook_active=true is ignored daemon-side.
            let settings = json!({
                "hooks": {
                    "SessionStart": [{"hooks": [{"type": "command", "command": cmd}]}],
                    "Stop": [{"hooks": [{"type": "command", "command": cmd}]}]
                }
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

/// Resolve the herdr client for a bot through its project's host.
async fn client_for_bot(app: &Arc<App>, bot_id: &str) -> LcResult<HerdrClient> {
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    app.herdr_for(&host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{host}` is not configured")))
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
    let host = project.host.clone();
    let client = app
        .herdr_for(&host)
        .await
        .ok_or_else(|| LcError::Upstream(format!("host `{host}` is not configured")))?;
    if !app.host_connected(&host).await {
        return Err(LcError::Upstream(format!("host `{host}` is not connected")));
    }
    let hook_port = match app.hosts.get(&host).await {
        Some(c) => c.hook_port(app.port),
        None => app.port,
    };
    let env = pane_env(app, bot, &host, run_id, hook_port).await;

    // 2. workspace
    let mut fresh_root: Option<String> = None;
    let workspace_id = match project.workspace_id.as_deref() {
        Some(ws) if client.workspace_get(ws).await.map_err(up)?.is_some() => ws.to_string(),
        _ => {
            let (ws, root) =
                client.workspace_create(&project.path, &project.label, env.clone()).await.map_err(up)?;
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
            let panes = client.pane_list(Some(&workspace_id)).await.map_err(up)?;
            let target = panes.first().map(|p| p.pane_id.clone()).ok_or_else(|| {
                LcError::Upstream(format!("workspace {workspace_id} has no panes to split"))
            })?;
            client
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
    let injected = injected_args(app, bot, project).await.map_err(up)?;
    let mut args = injected;
    args.extend(identity_args(app, bot).await);
    args.extend(bot.args());

    // 5. agent.start (async on the socket)
    if let Err(e) = client.agent_start(&bot.name, &bot.kind, &pane_id, &args, 60_000).await {
        let _ = client.pane_close(&pane_id).await;
        return Err(up(e));
    }

    // 6. per-run status subscription
    crate::events::watch_pane(app, &host, &pane_id).await;

    // 7. wait for readiness
    let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
    match client.agent_wait(&bot.name, &until, 60_000).await {
        Ok(info) => {
            let st = info.agent_status.normalized();
            set_run(app, run_id, "running", st.as_str()).await;
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "agent.wait did not settle");
            // Do NOT close the pane on timeout (SPEC §6.2.7).
            match client.agent_get(&bot.name).await {
                Ok(Some(info)) => set_run(app, run_id, "running", info.agent_status.normalized().as_str()).await,
                _ => {
                    let _ = client.pane_close(&pane_id).await;
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
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    let client = client_for_bot(app, bot_id).await?;

    let _ = sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run.id).execute(&app.db).await;
    app.emit_bot_status(bot_id).await;
    fail_in_flight(app, &run.id, "run stopped by user").await;

    for _ in 0..2 {
        let _ = client.agent_send_keys(&bot.name, &["ctrl+c".to_string()]).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut gone = false;
    for _ in 0..20 {
        let agent = client.agent_get(&bot.name).await;
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
        crate::events::unwatch_pane(app, &host, p).await;
    }
    app.emit_bot_status(bot_id).await;
    Ok(true)
}

pub async fn interrupt_bot(app: &Arc<App>, bot_id: &str) -> LcResult<()> {
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    client_for_bot(app, bot_id).await?.agent_send_keys(&bot.name, &["esc".to_string()]).await.map_err(up)?;
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
    client_for_bot(app, bot_id).await?.agent_send_keys(&bot.name, &keys).await.map_err(up)?;
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
    let res = client_for_bot(app, bot_id)
        .await?
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
    let host = db::bot_host(&app.db, &run.bot_id).await?;
    let client = app
        .herdr_for(&host)
        .await
        .ok_or_else(|| anyhow::anyhow!("host `{host}` is not configured"))?;
    let read = client.pane_read(&pane_id, "recent_unwrapped", 200).await?;
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
