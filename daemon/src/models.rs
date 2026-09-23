//! `GET /api/models`: live model lists from the agent CLIs, per host, cached 10 min.
//!
//! * codex: `codex app-server` `model/list`. The server never exits on its own, so we kill it
//!   locally / let `awk` cut the remote stream at the answer.
//! * grok: `grok models` text + `~/.grok/models_cache.json` efforts (`config.toml` fallback).
//! * claude: static aliases (no list API); `default_effort` from that identity's `settings.json`.

use crate::config::{expand_home, LOCAL_HOST};
use crate::hosts::sh_quote;
use crate::state::App;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

pub const CACHE_TTL: Duration = Duration::from_secs(600);
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

pub fn codex_models_from_rpc(result: &Value) -> Vec<Value> {
    result
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_string();
                    if id == "gpt-5.6-luna" {
                        return None;
                    }
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

/// Map only explicitly retired aliases. Full versioned model ids are user choices and stay intact.
pub fn remap_deprecated_model(kind: &str, model: &str) -> Option<&'static str> {
    match (kind, model) {
        ("codex", "gpt-5.6-luna") => Some("gpt-6-luna"),
        ("claude", "opus") => Some("claude-opus-5-5"),
        _ => None,
    }
}

pub fn canonical_model<'a>(kind: &str, model: &'a str) -> &'a str {
    remap_deprecated_model(kind, model).unwrap_or(model)
}

/// Lines: `Default model: grok-4.5`, then `  - grok-4.6` or `  * grok-4.5 (default)`.
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
                // Fallback when models_cache.json is missing; enriched per-model below.
                "efforts": ["low", "medium", "high"],
                "service_tiers": [],
            })
        })
        .collect()
}

const GROK_EFFORT_ORDER: &[&str] = &["low", "medium", "high", "xhigh"];

/// Unknown ids go to the end in input order.
fn sort_grok_efforts(ids: Vec<String>) -> Vec<String> {
    let mut known: Vec<String> = GROK_EFFORT_ORDER
        .iter()
        .filter(|k| ids.iter().any(|id| id == *k))
        .map(|s| (*s).to_string())
        .collect();
    for id in ids {
        if !known.iter().any(|k| k == &id) {
            known.push(id);
        }
    }
    known
}

/// `models.<id>.info.reasoning_efforts[{id|value, default}]` → `(efforts ascending, default)`.
fn grok_efforts_from_cache(cache: &Value, model_id: &str) -> Option<(Vec<String>, Option<String>)> {
    let efforts = cache
        .pointer(&format!("/models/{model_id}/info/reasoning_efforts"))?
        .as_array()?;
    if efforts.is_empty() {
        return None;
    }
    let mut ids = Vec::new();
    let mut default = None;
    for e in efforts {
        let Some(id) = e
            .get("value")
            .or_else(|| e.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if e.get("default").and_then(|d| d.as_bool()) == Some(true) {
            default = Some(id.clone());
        }
        if !ids.iter().any(|x| x == &id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return None;
    }
    Some((sort_grok_efforts(ids), default))
}

fn enrich_grok_models(mut models: Vec<Value>, cache_text: &str, cfg_text: &str) -> Vec<Value> {
    let cache: Value = serde_json::from_str(cache_text).unwrap_or(Value::Null);
    let cfg_default = grok_default_effort_from_config(cfg_text);
    for v in &mut models {
        let Some(obj) = v.as_object_mut() else { continue };
        let id = obj.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if let Some((efforts, def)) = grok_efforts_from_cache(&cache, &id) {
            obj.insert("efforts".into(), json!(efforts));
            let default_effort = def.or_else(|| cfg_default.clone());
            obj.insert("default_effort".into(), json!(default_effort));
        } else if let Some(eff) = &cfg_default {
            obj.insert("default_effort".into(), json!(eff));
        }
    }
    models
}

/// Real TOML parse (comments, single quotes, table scoping); top-level key wins over `[models]`.
fn grok_default_effort_from_config(text: &str) -> Option<String> {
    let doc: toml::Value = toml::from_str(text).ok()?;
    let pick = |v: &toml::Value| {
        v.get("default_reasoning_effort")?
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    pick(&doc).or_else(|| doc.get("models").and_then(pick))
}

/// Unparsable is `(None, {})`, never an error. Overrides are keyed by real model id and matched
/// to aliases by substring (verified against local + m4p `settings.json`, 2026-09-07).
pub fn parse_claude_effort_settings(text: &str) -> (Option<String>, BTreeMap<String, String>) {
    let mut per_model = BTreeMap::new();
    let Ok(v) = serde_json::from_str::<Value>(text) else { return (None, per_model) };
    let global = v.get("effortLevel").and_then(|x| x.as_str()).map(str::to_string);
    if let Some(obj) = v.get("modelSettings").and_then(|x| x.as_object()) {
        for (model_id, settings) in obj {
            if let Some(e) = settings.get("effortLevel").and_then(|x| x.as_str()) {
                per_model.insert(model_id.clone(), e.to_string());
            }
        }
    }
    (global, per_model)
}

/// `None` = `~/.claude`; an unknown or non-claude identity silently falls back to that too.
async fn claude_config_dir(app: &Arc<App>, host: &str, identity: Option<&str>) -> Option<String> {
    let name = identity?;
    let idn = crate::tools::identity_for_host(app, host, name).await?;
    if idn.kind != "claude" {
        return None;
    }
    let dir = idn.env.get("CLAUDE_CONFIG_DIR")?;
    let home = crate::tools::host_home(app, host).await;
    Some(expand_home(dir, &home))
}

fn optional_cat_script(path_expr: &str) -> String {
    format!("cat {path_expr} 2>/dev/null || true")
}

/// 讀那台主機上的一個選用設定檔。**缺檔是答案（回空字串），讀不到是錯誤**：以前兩者都變成 `""`，
/// ssh 逾時／連不上就被當成「沒設定」，接下來拿內建預設值當成事實記下去（#268）。
/// 遠端 `ssh_exec` 看 exit code，所以缺檔那條要自己收成 0（`|| true`），不然缺檔也是 Err。
async fn read_optional_text(app: &Arc<App>, host: &str, path_expr: &str) -> Result<String> {
    let script = optional_cat_script(path_expr);
    if host == LOCAL_HOST {
        let o = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .with_context(|| format!("read {path_expr}"))?;
        Ok(String::from_utf8_lossy(&o.stdout).to_string())
    } else {
        let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
        conn.ssh_exec(&format!("{script}\n")).await.with_context(|| format!("read {path_expr} on {host}"))
    }
}

async fn read_claude_effort_settings(
    app: &Arc<App>,
    host: &str,
    config_dir: Option<&str>,
) -> Result<(Option<String>, BTreeMap<String, String>)> {
    let path = match config_dir {
        Some(dir) => format!("{}/settings.json", sh_quote(dir)),
        None => "\"$HOME/.claude/settings.json\"".to_string(),
    };
    Ok(parse_claude_effort_settings(&read_optional_text(app, host, &path).await?))
}

/// claude's built-in default with no `effortLevel` / override: docs and a fresh identity both say
/// `high` (2026-09-07); the documented exception (Opus 4.7) is none of our aliases.
const CLAUDE_BUILTIN_DEFAULT_EFFORT: &str = "high";

/// What "不帶 `--effort`" resolves to; fills a spawned child's effort, since its argv never says.
/// 讀不到設定檔回 `Err`（不是內建預設）：呼叫端會把這個值記進 bot，記錯了沒有人會再來讀一次。
pub async fn claude_default_effort(app: &Arc<App>, host: &str, identity: Option<&str>, alias: &str) -> Result<String> {
    let dir = claude_config_dir(app, host, identity).await;
    let (global, per_model) = read_claude_effort_settings(app, host, dir.as_deref()).await?;
    let alias = alias.to_ascii_lowercase();
    Ok(per_model
        .iter()
        .find(|(k, _)| k.to_ascii_lowercase().contains(&alias))
        .map(|(_, v)| v.clone())
        .or(global)
        .unwrap_or_else(|| CLAUDE_BUILTIN_DEFAULT_EFFORT.to_string()))
}

pub fn claude_static_models(global: Option<&str>, per_model: &BTreeMap<String, String>) -> Vec<Value> {
    // claude 沒有 per-model effort 清單，每個 alias 都用 `--effort` 那五級。
    let efforts: Vec<Value> = crate::config::efforts_for_kind("claude").iter().map(|e| json!(e)).collect();
    ["opus", "sonnet", "haiku", "fable"]
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let overridden = per_model.iter().find(|(k, _)| k.to_ascii_lowercase().contains(id)).map(|(_, v)| v.clone());
            let default_effort = overridden.or_else(|| global.map(str::to_string)).unwrap_or_else(|| CLAUDE_BUILTIN_DEFAULT_EFFORT.to_string());
            json!({
                "id": id, "display_name": id, "description": "", "is_default": i == 0,
                "default_effort": default_effort, "efforts": efforts, "service_tiers": [],
            })
        })
        .collect()
}

/// Uncached. `identity` (claude only) picks whose `settings.json` the "預設" hint comes from.
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
            let cfg_text = read_optional_text(app, host, "\"$HOME/.grok/config.toml\"").await?;
            let cache_text = read_optional_text(app, host, "\"$HOME/.grok/models_cache.json\"").await?;
            let m = enrich_grok_models(m, &cache_text, &cfg_text);
            ("grok-cli", m)
        }
        "claude" => {
            let config_dir = claude_config_dir(app, host, identity).await;
            let (global, per_model) = read_claude_effort_settings(app, host, config_dir.as_deref()).await?;
            ("static", claude_static_models(global.as_deref(), &per_model))
        }
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
    let v = fetch(app, host, kind, identity).await?;
    app.models_cache.lock().await.insert(key, (Instant::now(), v.clone()));
    Ok(v)
}

/// Inverse of `lifecycle::model_args`, for panes the daemon did not start (adopted /
/// `managed_by='child'`), which would otherwise read 「預設」 forever. Unrecognised → `None`:
/// an unset field is honest, a guessed one is not.
pub fn model_effort_from_argv(kind: &str, argv: &[String]) -> (Option<String>, Option<String>) {
    let mut model: Option<String> = None;
    let mut effort: Option<String> = None;
    // codex also takes both via `-c key=value`.
    let assignment = |s: &str, key: &str| -> Option<String> {
        let (k, v) = s.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').trim_matches('\'').to_string())
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v.to_string())),
            _ => (arg, None),
        };
        let next = |i: &mut usize| -> Option<String> {
            if let Some(v) = inline.clone() {
                return Some(v);
            }
            let v = argv.get(*i + 1)?.clone();
            // A missing value (`--model` last, or `--model --effort high`) is not a value.
            if v.starts_with('-') {
                return None;
            }
            *i += 1;
            Some(v)
        };
        match flag {
            "--model" | "-m" => model = next(&mut i).or(model),
            "--effort" | "--reasoning-effort" => effort = next(&mut i).or(effort),
            "-c" | "--config" => {
                if let Some(v) = next(&mut i) {
                    effort = assignment(&v, "model_reasoning_effort").or(effort);
                    model = assignment(&v, "model").or(model);
                }
            }
            _ => {}
        }
        i += 1;
    }
    let model = model.map(|m| crate::models::canonical_model(kind, m.trim()).to_string()).filter(|m| !m.is_empty());
    let effort = effort.and_then(|e| crate::config::normalize_effort(kind, Some(&e)).ok().flatten());
    (model, effort)
}

/// Read codex's Fast service tier from an adopted pane.  `service_tier=priority` is the argv
/// equivalent of `/fast`; an empty/default tier is explicitly not Fast.  Unknown values stay
/// unknown rather than turning a guessed value into persisted state.
pub fn fast_from_argv(kind: &str, argv: &[String]) -> Option<bool> {
    if kind != "codex" {
        return None;
    }
    let assignment = |s: &str| -> Option<String> {
        let (key, value) = s.split_once('=')?;
        (key.trim() == "service_tier").then(|| value.trim().trim_matches('"').trim_matches('\'').to_ascii_lowercase())
    };
    let mut i = 0;
    let mut tier = None;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v.to_string())),
            _ => (arg, None),
        };
        if matches!(flag, "-c" | "--config") {
            let value = inline.or_else(|| argv.get(i + 1).and_then(|v| {
                if v.starts_with('-') {
                    None
                } else {
                    i += 1;
                    Some(v.clone())
                }
            }));
            if let Some(value) = value {
                if let Some(value) = assignment(&value) {
                    tier = Some(value);
                }
            }
        }
        i += 1;
    }
    match tier.as_deref() {
        Some("priority") | Some("fast") => Some(true),
        Some("") | Some("default") | Some("standard") => Some(false),
        _ => None,
    }
}

/// grok's fallback when argv says nothing: before it renames itself to a task, the terminal
/// title is `Grok 4.6 (xhigh)`.
pub fn grok_title_model_effort(title: &str) -> (Option<String>, Option<String>) {
    let t = title.trim();
    let Some(rest) = t.strip_prefix("Grok ").or_else(|| t.strip_prefix("grok ")) else {
        return (None, None);
    };
    let mut it = rest.split_whitespace();
    // `4.6` -> the `grok-4.6` id the model list and `-m` both use.
    let model = it
        .next()
        .map(|v| v.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.'))
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map(|v| format!("grok-{v}"));
    let effort = it
        .next()
        .map(|v| v.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .and_then(|v| crate::config::normalize_effort("grok", Some(v)).ok().flatten());
    (model, effort)
}

/// grok TUI 把實際 effort 畫在框底 `╰── Grok 4.6 (high) · always-approve ─╯`（#215）。
/// `--reasoning-effort` 與 `default_reasoning_effort` 都被 TUI 忽略，啟動後要靠這行判斷要不要補 `/effort`。
pub fn grok_effort_from_screen(screen: &str) -> Option<String> {
    for line in screen.lines() {
        let Some(idx) = line.find("Grok ").or_else(|| line.find("grok ")) else { continue };
        if let (_, Some(e)) = grok_title_model_effort(&line[idx..]) {
            return Some(e);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一台設定了、但連不上的遠端主機（沒有人在聽那個 port，connection refused，快速失敗）。
    async fn unreachable_host(e: &crate::testing::Env) -> &'static str {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let cfg = crate::config::HostCfg {
            name: "unreachable-box".into(),
            ssh: "127.0.0.1".into(),
            ssh_port: port,
            ssh_opts: vec!["-o".into(), "ConnectTimeout=2".into()],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        e.app.hosts.apply_config(&e.app, &[cfg]).await;
        "unreachable-box"
    }

    /// #268：遠端 settings.json 讀不到（ssh 失敗）不是「沒設定」——以前讀成 `""`，預設 effort 變成內建的 `high`，
    /// 對帳把它記進子 bot，之後沒有人會再讀一次。
    #[tokio::test]
    async fn an_unreadable_remote_settings_file_is_an_error_not_the_builtin_default() {
        let e = crate::testing::env().await;
        let host = unreachable_host(&e).await;
        let r = tokio::time::timeout(Duration::from_secs(20), claude_default_effort(&e.app, host, None, "opus"))
            .await
            .expect("連不上要快速失敗");
        assert!(r.is_err(), "讀不到設定檔不能回內建預設：{r:?}");
        let listed = tokio::time::timeout(Duration::from_secs(20), list(&e.app, host, "claude", None, true)).await.unwrap();
        assert!(listed.is_err(), "模型清單不能拿內建預設充數、還快取十分鐘");
        assert!(e.app.models_cache.lock().await.is_empty(), "失敗的結果不能進快取");
        e.app.hosts.remove(&e.app, host).await;
    }

    /// 缺檔是答案不是錯誤：本機回 `""`，遠端腳本要 exit 0（`ssh_exec` 看 exit code）。
    #[tokio::test]
    async fn a_missing_settings_file_is_an_empty_answer_not_a_failure() {
        let e = crate::testing::env().await;
        let local = read_optional_text(&e.app, LOCAL_HOST, "/nonexistent-am-dir/settings.json").await.unwrap();
        assert_eq!(local, "");
        let st = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(optional_cat_script("/nonexistent-am-dir/settings.json"))
            .status()
            .await
            .unwrap();
        assert!(st.success(), "缺檔時腳本要 exit 0，不然遠端缺檔會被 ssh_exec 當成失敗");
    }

    /// The argv shapes `herdr pane process-info` actually reported on this machine,
    /// 2026-09-07 (a claude child pane and a grok one).
    #[test]
    fn model_and_effort_are_read_off_a_running_cli() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--dangerously-skip-permissions", "--model", "opus"])),
            (Some("claude-opus-5-5".into()), None)
        );
        assert_eq!(
            model_effort_from_argv("grok", &argv(&["grok", "--always-approve", "-m", "grok-4.6", "--reasoning-effort", "high"])),
            (Some("grok-4.6".into()), Some("high".into()))
        );
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--model=sonnet", "--effort=xhigh"])),
            (Some("sonnet".into()), Some("xhigh".into()))
        );
        assert_eq!(
            model_effort_from_argv(
                "codex",
                &argv(&["codex", "-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=\"max\"", "-c", "service_tier=\"priority\""])
            ),
            (Some("gpt-5.6-sol".into()), Some("max".into()))
        );
        assert_eq!(
            fast_from_argv("codex", &argv(&["codex", "-c", "service_tier=\"priority\""])),
            Some(true)
        );
        assert_eq!(fast_from_argv("codex", &argv(&["codex", "-c", "service_tier=\"\""])), Some(false));
        assert_eq!(fast_from_argv("claude", &argv(&["claude", "-c", "service_tier=\"priority\""])), None);
    }

    /// Nothing is guessed: a bare CLI stays unset, and an effort the kind does not accept
    /// (`max` is claude/codex only) is dropped rather than stored for grok.
    #[test]
    fn unparseable_argv_leaves_the_fields_unset() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(model_effort_from_argv("claude", &argv(&["claude", "--dangerously-skip-permissions"])), (None, None));
        assert_eq!(model_effort_from_argv("grok", &argv(&["grok", "--reasoning-effort", "max"])), (None, None));
        // A flag whose value is missing must not swallow the next flag.
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--model", "--effort", "high"])),
            (None, Some("high".into()))
        );
    }

    #[test]
    fn grok_falls_back_to_its_terminal_title() {
        assert_eq!(grok_title_model_effort("Grok 4.6 (xhigh)"), (Some("grok-4.6".into()), Some("xhigh".into())));
        assert_eq!(grok_title_model_effort("Grok 4.6"), (Some("grok-4.6".into()), None));
        // A title the agent has renamed to its task says nothing about the model.
        assert_eq!(grok_title_model_effort("遠端主機 gh 登入 API 與 UI - grok"), (None, None));
        assert_eq!(grok_title_model_effort("Grok Code Fast"), (None, None));
    }

    #[test]
    fn grok_effort_is_read_off_the_tui_footer() {
        let high = "  │ ❯                                        │\n  ╰──────────────── Grok 4.6 (high) · always-approve ─╯\n";
        assert_eq!(grok_effort_from_screen(high).as_deref(), Some("high"));
        let medium = "  ╰──────────────────────────────── Grok 4.6 (medium) · always-approve ─╯\n";
        assert_eq!(grok_effort_from_screen(medium).as_deref(), Some("medium"));
        assert_eq!(grok_effort_from_screen("claude composer, no grok footer"), None);
    }

    /// Real `settings.json` shapes seen on this machine and on m4p, 2026-09-07: an account
    /// with a global default and one override, and a host with only an override (no global).
    #[test]
    fn claude_effort_hint_prefers_the_per_model_override() {
        let (global, per_model) = parse_claude_effort_settings(
            r#"{"effortLevel":"high","modelSettings":{"claude-opus-5":{"effortLevel":"low"}}}"#,
        );
        assert_eq!(global.as_deref(), Some("high"));
        assert_eq!(per_model.get("claude-opus-5").map(String::as_str), Some("low"));

        let models = claude_static_models(global.as_deref(), &per_model);
        let of = |id: &str| models.iter().find(|m| m["id"] == id).unwrap()["default_effort"].as_str().map(String::from);
        assert_eq!(of("opus"), Some("low".into()), "per-model override wins");
        assert_eq!(of("sonnet"), Some("high".into()), "falls back to the account default");

        // m4p: no top-level `effortLevel`, only a fable override.
        let (global2, per_model2) =
            parse_claude_effort_settings(r#"{"modelSettings":{"claude-fable-5-1":{"effortLevel":"low"}}}"#);
        assert_eq!(global2, None);
        let models2 = claude_static_models(global2.as_deref(), &per_model2);
        let of2 = |id: &str| models2.iter().find(|m| m["id"] == id).unwrap()["default_effort"].clone();
        assert_eq!(of2("fable"), json!("low"));
        // No override and no global: the CLI's own default (verified on a fresh cc2 identity).
        assert_eq!(of2("opus"), json!("high"));

        // Unreadable / not JSON: no override, no global — same built-in fallback.
        let (g3, m3) = parse_claude_effort_settings("");
        assert_eq!(g3, None);
        assert!(m3.is_empty());
        let models3 = claude_static_models(g3.as_deref(), &m3);
        assert_eq!(models3[0]["default_effort"], json!("high"));
    }

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
    fn grok_cache_enriches_per_model_efforts() {
        let t = "Default model: grok-4.5\n\nAvailable models:\n  - grok-4.6\n  * grok-4.5 (default)\n";
        let m = grok_models_from_text(t);
        let cache = json!({
            "models": {
                "grok-4.6": {"info": {"reasoning_efforts": [
                    {"id": "xhigh", "value": "xhigh", "default": false},
                    {"id": "high", "value": "high", "default": true},
                    {"id": "medium", "value": "medium", "default": false},
                    {"id": "low", "value": "low", "default": false}
                ]}},
                "grok-4.5": {"info": {"reasoning_efforts": [
                    {"id": "high", "value": "high", "default": true},
                    {"id": "medium", "value": "medium", "default": false},
                    {"id": "low", "value": "low", "default": false}
                ]}}
            }
        });
        let m = enrich_grok_models(m, &cache.to_string(), "default_reasoning_effort = \"medium\"\n");
        assert_eq!(m[0]["efforts"], json!(["low", "medium", "high", "xhigh"]));
        assert_eq!(m[0]["default_effort"], "high");
        assert_eq!(m[1]["efforts"], json!(["low", "medium", "high"]));
        assert_eq!(m[1]["default_effort"], "high");
    }

    #[test]
    fn grok_config_effort_top_level_with_trailing_comment() {
        let cfg = "default_reasoning_effort = \"high\"  # 2026-09\n";
        assert_eq!(grok_default_effort_from_config(cfg).as_deref(), Some("high"));
    }

    #[test]
    fn grok_config_effort_models_table_and_single_quotes() {
        let cfg = "[models]\ndefault_reasoning_effort = 'medium' # note\n";
        assert_eq!(grok_default_effort_from_config(cfg).as_deref(), Some("medium"));
    }

    #[test]
    fn grok_config_effort_top_level_wins_over_models_table() {
        let cfg = "default_reasoning_effort = \"high\"\n[models]\ndefault_reasoning_effort = \"low\"\n";
        assert_eq!(grok_default_effort_from_config(cfg).as_deref(), Some("high"));
    }

    #[test]
    fn grok_config_effort_ignores_other_tables_and_junk() {
        // A same-named key under an unrelated table is not the global default.
        let cfg = "[models.\"grok-4.5\"]\ndefault_reasoning_effort = \"low\"\n";
        assert_eq!(grok_default_effort_from_config(cfg), None);
        assert_eq!(grok_default_effort_from_config("default_reasoning_effort = \"\"\n"), None);
        assert_eq!(grok_default_effort_from_config("default_reasoning_effort = 3\n"), None);
        assert_eq!(grok_default_effort_from_config("not toml = = =\n"), None);
        assert_eq!(grok_default_effort_from_config(""), None);
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
    fn deprecated_models_are_remapped_only_on_exact_matches() {
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-luna"), Some("gpt-6-luna"));
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-luna-extra"), None);
        assert_eq!(remap_deprecated_model("claude", "opus"), Some("claude-opus-5-5"));
        assert_eq!(remap_deprecated_model("claude", "claude-opus-4-1"), None);
        assert_eq!(remap_deprecated_model("claude", "claude-opus-5-5"), None);
    }

    #[test]
    fn adopted_child_argv_model_is_canonicalized_only_for_retired_exact_names() {
        let argv = |model: &str| vec!["codex".into(), "-m".into(), model.into()];
        assert_eq!(model_effort_from_argv("codex", &argv("gpt-5.6-luna")).0.as_deref(), Some("gpt-6-luna"));
        let argv = vec!["claude".into(), "--model".into(), "opus".into()];
        assert_eq!(model_effort_from_argv("claude", &argv).0.as_deref(), Some("claude-opus-5-5"));
        let argv = vec!["claude".into(), "--model".into(), "claude-opus-4-1".into()];
        assert_eq!(model_effort_from_argv("claude", &argv).0.as_deref(), Some("claude-opus-4-1"));
    }

    #[test]
    fn codex_model_catalog_hides_the_deprecated_luna_exactly() {
        let r = json!({"data": [
            {"id": "gpt-5.6-luna", "displayName": "Old"},
            {"id": "gpt-5.6-luna-preview", "displayName": "Other"},
            {"id": "gpt-6-luna", "displayName": "New"}
        ]});
        let mapped = codex_models_from_rpc(&r);
        let ids: Vec<_> = mapped.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["gpt-5.6-luna-preview", "gpt-6-luna"]);
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
