//! v4.0 — `GET /api/models`: live model lists from the agent CLIs, per host, cached 10 min.
//!
//! * codex: `codex app-server` (stdio JSON-RPC) `model/list`. The server never exits on its
//!   own; locally we drive it with a tokio child (write three lines, read stdout until the
//!   matching id, kill), remotely with a `sh` pipeline that feeds stdin, waits a few seconds
//!   and lets `awk` cut the stream at the answer.
//! * grok: `grok models` text output.
//! * claude: static `opus / sonnet / haiku` (no list API).

use crate::config::LOCAL_HOST;
use crate::hosts::sh_quote;
use crate::state::App;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

pub const CACHE_TTL: Duration = Duration::from_secs(600);
/// How long a local `codex app-server` round trip may take (initialize is instant,
/// `model/list` ≈ 1 s locally).
const LOCAL_RPC_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds the remote pipeline keeps stdin open before the server is allowed to exit.
const REMOTE_RPC_HOLD_SECS: u32 = 6;

const CLIENT_INFO: &str = r#"{"name":"agents-manager","title":"agents-manager","version":"0.1"}"#;

fn rpc_lines(id: u64, method: &str, params: &Value) -> Vec<String> {
    vec![
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"clientInfo":{CLIENT_INFO}}}}}"#),
        r#"{"jsonrpc":"2.0","method":"initialized"}"#.to_string(),
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
    ]
}

/// Pick the response with `id` out of a stream of JSON-RPC lines.
fn find_response(text: &str, id: u64) -> Option<Result<Value>> {
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if v.get("id").and_then(|x| x.as_u64()) == Some(id) {
            if let Some(err) = v.get("error") {
                return Some(Err(anyhow!("codex app-server error: {err}")));
            }
            return Some(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
        }
    }
    None
}

/// One `codex app-server` JSON-RPC call on `host`. `codex_path` overrides the executable
/// (from the tools cache); otherwise the login shell's `codex` is used.
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

async fn codex_rpc_local(exe: &str, lines: &[String], id: u64) -> Result<Value> {
    // The daemon's own PATH may lack codex (launchd); resolve through the login shell first.
    let exe = if exe.contains('/') {
        exe.to_string()
    } else {
        let probe = format!("( \"${{SHELL:-/bin/sh}}\" -lic 'command -v {exe}' 2>/dev/null || command -v {exe} 2>/dev/null ) | tail -1");
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

/// `model/list` → the API shape.
pub fn codex_models_from_rpc(result: &Value) -> Vec<Value> {
    result
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_string();
                    let efforts: Vec<String> = m
                        .get("supportedReasoningEfforts")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|e| e.get("reasoningEffort")?.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    let tiers: Vec<Value> = m
                        .get("serviceTiers")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .map(|t| {
                                    json!({
                                        "id": t.get("id").and_then(|x| x.as_str()).unwrap_or(""),
                                        "name": t.get("name").and_then(|x| x.as_str()).unwrap_or(""),
                                        "description": t.get("description").and_then(|x| x.as_str()).unwrap_or(""),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(json!({
                        "id": id,
                        "display_name": m.get("displayName").and_then(|x| x.as_str()).unwrap_or(""),
                        "description": m.get("description").and_then(|x| x.as_str()).unwrap_or(""),
                        "is_default": m.get("isDefault").and_then(|x| x.as_bool()).unwrap_or(false),
                        "default_effort": m.get("defaultReasoningEffort").and_then(|x| x.as_str()),
                        "efforts": efforts,
                        "service_tiers": tiers,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `grok models` text → the API shape. Lines: `Default model: grok-4.5`, then under
/// `Available models:` either `  - grok-4.6` or `  * grok-4.5 (default)`.
pub fn grok_models_from_text(text: &str) -> Vec<Value> {
    let mut default: Option<String> = None;
    let mut ids: Vec<(String, bool)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(d) = line.strip_prefix("Default model:") {
            default = Some(d.trim().to_string());
            continue;
        }
        let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) else { continue };
        let is_default_mark = line.starts_with('*') || rest.contains("(default)");
        let id = rest.split_whitespace().next().unwrap_or("").to_string();
        if id.is_empty() || ids.iter().any(|(x, _)| *x == id) {
            continue;
        }
        ids.push((id, is_default_mark));
    }
    ids.into_iter()
        .map(|(id, mark)| {
            let is_default = mark || default.as_deref() == Some(id.as_str());
            json!({
                "id": id,
                "display_name": id,
                "description": "",
                "is_default": is_default,
                "default_effort": Value::Null,
                "efforts": ["low", "medium", "high"],
                "service_tiers": [],
            })
        })
        .collect()
}

pub fn claude_static_models() -> Vec<Value> {
    ["opus", "sonnet", "haiku"]
        .iter()
        .enumerate()
        .map(|(i, id)| {
            json!({
                "id": id, "display_name": id, "description": "", "is_default": i == 0,
                "default_effort": Value::Null, "efforts": [], "service_tiers": [],
            })
        })
        .collect()
}

/// Uncached fetch.
pub async fn fetch(app: &Arc<App>, host: &str, kind: &str) -> Result<Value> {
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
                let o = tokio::time::timeout(
                    Duration::from_secs(30),
                    tokio::process::Command::new("/bin/sh").arg("-c").arg(&script).stdin(std::process::Stdio::null()).output(),
                )
                .await
                .map_err(|_| anyhow!("`grok models` timed out"))??;
                String::from_utf8_lossy(&o.stdout).to_string()
            } else {
                let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
                conn.ssh_exec_path(&format!("{} models 2>/dev/null </dev/null\n", sh_quote(&exe))).await?
            };
            let m = grok_models_from_text(&text);
            if m.is_empty() {
                bail!("could not parse `grok models` output:\n{}", text.trim());
            }
            ("grok-cli", m)
        }
        "claude" => ("static", claude_static_models()),
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

/// Cached fetch (10 min per host+kind); `refresh` bypasses the cache.
pub async fn list(app: &Arc<App>, host: &str, kind: &str, refresh: bool) -> Result<Value> {
    let key = format!("{host}/{kind}");
    if !refresh {
        if let Some((at, v)) = app.models_cache.lock().await.get(&key) {
            if at.elapsed() < CACHE_TTL {
                return Ok(v.clone());
            }
        }
    }
    let v = fetch(app, host, kind).await?;
    app.models_cache.lock().await.insert(key, (Instant::now(), v.clone()));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_text_parses() {
        let t = "You are logged in with grok.com.\n\nDefault model: grok-4.5\n\nAvailable models:\n  - grok-4.6\n  * grok-4.5 (default)\n";
        let m = grok_models_from_text(t);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["id"], "grok-4.6");
        assert_eq!(m[0]["is_default"], false);
        assert_eq!(m[1]["id"], "grok-4.5");
        assert_eq!(m[1]["is_default"], true);
        assert_eq!(m[1]["efforts"][2], "high");
    }

    #[test]
    fn codex_rpc_result_maps() {
        let r = json!({"data": [{
            "id": "gpt-6-astra", "displayName": "GPT-6-Astra", "description": "d", "isDefault": true,
            "defaultReasoningEffort": "low",
            "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": ""}, {"reasoningEffort": "ultra", "description": ""}],
            "serviceTiers": [{"id": "priority", "name": "Fast", "description": "2x speed, increased usage"}]
        }]});
        let m = codex_models_from_rpc(&r);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["id"], "gpt-6-astra");
        assert_eq!(m[0]["default_effort"], "low");
        assert_eq!(m[0]["efforts"], json!(["low", "ultra"]));
        assert_eq!(m[0]["service_tiers"][0]["id"], "priority");
    }

    #[test]
    fn find_response_skips_notifications() {
        let text = "{\"method\":\"remoteControl/status/changed\",\"params\":{}}\n{\"id\":1,\"result\":{}}\n{\"id\":2,\"result\":{\"data\":[]}}\n";
        let r = find_response(text, 2).unwrap().unwrap();
        assert_eq!(r["data"], json!([]));
        assert!(find_response(text, 9).is_none());
        let err = "{\"id\":2,\"error\":{\"code\":-1,\"message\":\"nope\"}}";
        assert!(find_response(err, 2).unwrap().is_err());
    }
}
