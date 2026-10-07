use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use crate::config::LOCAL_HOST;
use crate::hosts::sh_quote;
use crate::models::{
    agy_static_models, claude_config_dir, claude_static_models, codex_models_from_rpc,
    enrich_grok_models, find_response, grok_models_from_text, read_claude_effort_settings,
    read_optional_text, rpc_lines, CACHE_TTL,
};
use crate::state::App;

const LOCAL_RPC_TIMEOUT: Duration = Duration::from_secs(20);
const REMOTE_RPC_HOLD_SECS: u32 = 6;

async fn codex_rpc_local(exe: &str, lines: &[String], id: u64) -> Result<Value> {
    let exe = if exe.contains('/') {
        exe.to_string()
    } else {
        let probe = format!("{}; printf '%s\\n' \"$p\"", crate::tools::login_abs_sh(exe));
        let o = tokio::process::Command::new("/bin/sh").arg("-c").arg(&probe).output().await?;
        let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if p.is_empty() {
            bail!("`{exe}` is not installed on this machine");
        }
        p
    };
    let mut child = tokio::process::Command::new(&exe)
        .arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {exe} app-server"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    for l in lines {
        stdin.write_all(l.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
    }
    stdin.flush().await?;
    let mut reader = tokio::io::BufReader::new(stdout).lines();
    let result = loop {
        match reader.next_line().await? {
            None => break Err(anyhow!("codex app-server exited before answering")),
            Some(line) => {
                if let Some(r) = find_response(&line, id) {
                    break r;
                }
            }
        }
    };
    let _ = child.kill().await;
    result
}

pub async fn codex_rpc(app: &Arc<App>, host: &str, method: &str, params: Value) -> Result<Value> {
    const ID: u64 = 2;
    let lines = rpc_lines(ID, method, &params);
    let exe = crate::tools::cached_path(app, host, "codex").await.unwrap_or_else(|| "codex".into());

    if host == LOCAL_HOST {
        return tokio::time::timeout(LOCAL_RPC_TIMEOUT, codex_rpc_local(&exe, &lines, ID))
            .await
            .map_err(|_| anyhow!("codex app-server `{method}` timed out"))?;
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let printf_args = lines.iter().map(|l| sh_quote(l)).collect::<Vec<_>>().join(" ");
    let script = format!(
        "{{ printf '%s\\n' {printf_args}; sleep {REMOTE_RPC_HOLD_SECS}; }} | {exe} app-server 2>/dev/null | awk '{{print}} /\"id\":{ID}[,}}]/ {{exit}}'\n",
        exe = sh_quote(&exe),
    );
    let out = conn.ssh_exec_path(&script).await.with_context(|| format!("codex app-server on {host}"))?;
    find_response(&out, ID).ok_or_else(|| anyhow!("codex app-server on {host}: no response to `{method}`:\n{}", out.trim()))?
}

pub async fn fetch(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>) -> Result<Value> {
    let (source, models) = match kind {
        "codex" => {
            let r = codex_rpc(app, host, "model/list", json!({"includeHidden": false})).await?;
            let m = codex_models_from_rpc(&r);
            if m.is_empty() {
                bail!("codex app-server returned no models");
            }
            ("codex-app-server", m)
        }
        "grok" => {
            let exe = crate::tools::cached_path(app, host, "grok").await.unwrap_or_else(|| "grok".into());
            let text = if host == LOCAL_HOST {
                let script = format!(
                    "( \"${{SHELL:-/bin/sh}}\" -lic {q} 2>/dev/null || {exe} models 2>/dev/null )",
                    q = sh_quote(&format!("{exe} models")),
                    exe = sh_quote(&exe)
                );
                crate::hosts::sh_local_stdout(&script, Duration::from_secs(30), "`grok models`").await?
            } else {
                let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
                conn.ssh_exec_path(&format!("{} models 2>/dev/null </dev/null\n", sh_quote(&exe))).await?
            };
            let m = grok_models_from_text(&text);
            if m.is_empty() {
                bail!("could not parse `grok models` output:\n{}", text.trim());
            }
            let cfg_text = read_optional_text(app.as_ref(), host, "\"$HOME/.grok/config.toml\"").await?;
            let cache_text = read_optional_text(app.as_ref(), host, "\"$HOME/.grok/models_cache.json\"").await?;
            let m = enrich_grok_models(m, &cache_text, &cfg_text);
            ("grok-cli", m)
        }
        "claude" => {
            let config_dir = claude_config_dir(app, host, identity).await?;
            let (global, per_model) = read_claude_effort_settings(app.as_ref(), host, config_dir.as_deref()).await?;
            ("static", claude_static_models(global.as_deref(), &per_model))
        }
        "agy" => ("static", agy_static_models()),
        other => bail!("unknown kind `{other}`"),
    };
    Ok(json!({
        "kind": kind,
        "host": host,
        "source": source,
        "fetched_at": crate::db::now(),
        "models": models,
    }))
}

pub async fn list(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>, refresh: bool) -> Result<Value> {
    let key = format!("{host}/{kind}/{}", identity.unwrap_or(""));
    if !refresh {
        if let Some((at, v)) = app.models_cache.lock().await.get(&key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(v.clone());
            }
        }
    }
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let v = fetch(app, host, kind, identity).await?;
    let mut cache = app.models_cache.lock().await;
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` was reconnected/reconfigured while listing {kind} models; stale result discarded");
    }
    cache.insert(key, (Instant::now(), v.clone()));
    Ok(v)
}
