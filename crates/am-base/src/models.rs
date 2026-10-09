//! `GET /api/models`: live model lists from the agent CLIs, per host, cached 10 min.
//!
//! * codex: `codex app-server` `model/list`. The server never exits on its own, so we kill it
//!   locally / let `awk` cut the remote stream at the answer.
//! * grok: `grok models` text + `~/.grok/models_cache.json` efforts (`config.toml` fallback).
//! * claude: static aliases (no list API); `default_effort` from that identity's `settings.json`.

use crate::config::{expand_home, LOCAL_HOST};
use crate::hosts::sh_quote;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub const CACHE_TTL: Duration = Duration::from_secs(600);

pub(crate) const CLIENT_INFO: &str = r#"{"name":"agents-manager","title":"agents-manager","version":"0.1"}"#;

/// Read a fresh snapshot without turning an ordinary Bot request into a CLI probe.
pub async fn cached(app: &impl crate::models::ModelsCache, host: &str, kind: &str, identity: Option<&str>) -> Option<Value> {
    let key = format!("{host}/{kind}/{}", identity.unwrap_or(""));
    app.models_cache()
        .lock()
        .await
        .get(&key)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, value)| value.clone())
}

pub fn rpc_lines(id: u64, method: &str, params: &Value) -> Vec<String> {
    vec![
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"clientInfo":{CLIENT_INFO}}}}}"#),
        r#"{"jsonrpc":"2.0","method":"initialized"}"#.to_string(),
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
    ]
}

pub fn find_response(text: &str, id: u64) -> Option<Result<Value>> {
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


pub fn codex_models_from_rpc(result: &Value) -> Vec<Value> {
    result
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_string();
                    if matches!(id.as_str(), "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna") {
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

/// agy 的預設模型（清單第一個）；被拿掉的 agy 模型一律換成它。
pub const AGY_DEFAULT_MODEL: &str = "gemini-3.8-flash-medium";

/// agy 已經拿掉的模型（2026-10-05 使用者：「agy 的 model 只限 3.8」）：3.7／3.6 Flash（agy 公告即將下架）、3.1 Pro、
/// claude-sonnet-4-6、claude-opus-4-6-thinking、gpt-oss-120b-medium。已存在的 bot 設了其中之一，啟動時與寫入設定時都換成 [`AGY_DEFAULT_MODEL`]。
pub const AGY_RETIRED_MODELS: &[&str] = &[
    "gemini-3.7-flash-high",
    "gemini-3.7-flash-medium",
    "gemini-3.7-flash-low",
    "gemini-3.6-flash-high",
    "gemini-3.6-flash-medium",
    "gemini-3.6-flash-low",
    "gemini-3.1-pro-high",
    "gemini-3.1-pro-low",
    "claude-sonnet-4-6",
    "claude-opus-4-6-thinking",
    "gpt-oss-120b-medium",
];

/// Map only explicitly retired aliases. Full versioned model ids are user choices and stay intact.
pub fn remap_deprecated_model(kind: &str, model: &str) -> Option<&'static str> {
    if kind == "agy" && AGY_RETIRED_MODELS.contains(&model) {
        return Some(AGY_DEFAULT_MODEL);
    }
    match (kind, model) {
        ("codex", "gpt-5.6-sol" | "gpt-5.6-terra") => Some("gpt-6-sol"),
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

pub fn enrich_grok_models(mut models: Vec<Value>, cache_text: &str, cfg_text: &str) -> Vec<Value> {
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
pub fn grok_default_effort_from_config(text: &str) -> Option<String> {
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

/// codex 身分 `config.toml` 頂層的 `model`／`model_reasoning_effort`＝這個身分實際會跑的模型與強度（`-c` 或 `/model` 寫進去的也是這兩行）。
/// 模型有設就以它為 `is_default`（不在清單裡的就一顆都不標，不猜）；強度有設就覆寫那顆預設模型的 `default_effort`。
/// 兩個都沒設＝照 app-server 回的。讀不懂的 TOML 原樣回傳。只看頂層，`[profiles.*]` 裡的不算。
pub fn codex_apply_identity_config(mut models: Vec<Value>, cfg_text: &str) -> Vec<Value> {
    let Ok(doc) = toml::from_str::<toml::Value>(cfg_text) else { return models };
    let top = |key: &str| doc.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    if let Some(model) = top("model") {
        for v in &mut models {
            v["is_default"] = json!(v["id"].as_str() == Some(model.as_str()));
        }
    }
    if let Some(effort) = top("model_reasoning_effort") {
        for v in &mut models {
            if v["is_default"] == json!(true) {
                v["default_effort"] = json!(effort);
            }
        }
    }
    models
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

/// 身分 `env` 裡的家目錄變數（`CLAUDE_CONFIG_DIR`／`CODEX_HOME`／`GROK_HOME`）展開成這台的絕對路徑。
/// `None` = 沒有身分、身分不是這個 kind、或沒設這個變數：照 CLI 預設家目錄。
pub async fn identity_home_dir(app: &Arc<impl crate::tools::ToolsEnv + 'static>, host: &str, identity: Option<&str>, kind: &str, var: &str) -> Result<Option<String>> {
    let Some(name) = identity else { return Ok(None) };
    let Some(idn) = crate::tools::identity_for_host(app, host, name).await else { return Ok(None) };
    if idn.kind != kind {
        return Ok(None);
    }
    let Some(dir) = idn.env.get(var) else { return Ok(None) };
    let home = crate::tools::host_home(app, host).await?;
    Ok(Some(expand_home(dir, &home)))
}

/// `None` = `~/.claude`; an unknown or non-claude identity silently falls back to that too.
pub async fn claude_config_dir(app: &Arc<impl crate::tools::ToolsEnv + 'static>, host: &str, identity: Option<&str>) -> Result<Option<String>> {
    identity_home_dir(app, host, identity, "claude", "CLAUDE_CONFIG_DIR").await
}

/// 在 shell 腳本前面把家目錄變數 export 成這台的絕對路徑（`CODEX_HOME='…'; export CODEX_HOME; `）。
pub fn home_env_prefix(var: &str, dir: &str) -> String {
    format!("{var}={}; export {var}; ", sh_quote(dir))
}

/// `<家目錄>/<file>` 的 shell 讀檔運算式。有解出絕對路徑就用它；沒有就讀 CLI 預設的 `$HOME/<default_dir>`。
pub fn home_file_expr(home_dir: Option<&str>, default_dir: &str, file: &str) -> String {
    match home_dir {
        Some(dir) => format!("{}/{file}", sh_quote(dir)),
        None => format!("\"$HOME/{default_dir}/{file}\""),
    }
}

pub fn optional_cat_script(path_expr: &str) -> String {
    format!("cat {path_expr} 2>/dev/null || true")
}

/// 讀那台主機上的一個選用設定檔。**缺檔是答案（回空字串），讀不到是錯誤**：以前兩者都變成 `""`，
/// ssh 逾時／連不上就被當成「沒設定」，接下來拿內建預設值當成事實記下去（#268）。
/// 遠端 `ssh_exec` 看 exit code，所以缺檔那條要自己收成 0（`|| true`），不然缺檔也是 Err。
pub async fn read_optional_text(app: &impl crate::hosts::HostsAccess, host: &str, path_expr: &str) -> Result<String> {
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
        let conn = app.hosts().get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
        conn.ssh_exec(&format!("{script}\n")).await.with_context(|| format!("read {path_expr} on {host}"))
    }
}

/// settings.json 頂層的 `model`：claude 的 `/model` 寫進去的那個預設（`sonnet`、`claude-sonnet-5-5`…）。沒有＝`None`，不猜。
pub fn parse_claude_default_model(text: &str) -> Option<String> {
    let v = serde_json::from_str::<Value>(text).ok()?;
    v.get("model").and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// 回傳 `(全域 effortLevel, per-model 覆寫, 頂層 model)`，三個都從同一份 settings.json 讀。
pub async fn read_claude_effort_settings(
    app: &impl crate::hosts::HostsAccess,
    host: &str,
    config_dir: Option<&str>,
) -> Result<(Option<String>, BTreeMap<String, String>, Option<String>)> {
    let text = read_optional_text(app, host, &home_file_expr(config_dir, ".claude", "settings.json")).await?;
    let (global, per_model) = parse_claude_effort_settings(&text);
    Ok((global, per_model, parse_claude_default_model(&text)))
}

/// claude's built-in default with no `effortLevel` / override: docs and a fresh identity both say
/// `high` (2026-09-07); the documented exception (Opus 4.7) is none of our aliases.
const CLAUDE_BUILTIN_DEFAULT_EFFORT: &str = "high";

/// haiku 的內建預設是 `medium`（2026-10-08 實測 Claude Code 2.1.293：沒帶 `--effort`、settings 沒有 `effortLevel`，
/// 對話紀錄每則 assistant 都是 `"effort":"medium"`；sonnet 是 high，#880）。`alias` 可以是 `haiku` 或完整 id。
pub fn claude_builtin_default_effort(alias: &str) -> &'static str {
    if alias.to_ascii_lowercase().contains("haiku") {
        "medium"
    } else {
        CLAUDE_BUILTIN_DEFAULT_EFFORT
    }
}

/// `modelSettings` 裡該模型的 `effortLevel` 覆寫（issue #937）。
///
/// 以前用 `key.contains(alias)` 掃字典序的 `BTreeMap`，一律先撞到**舊版**那一列（`claude-sonnet-5` 排在 `claude-sonnet-5-5` 前面，
/// 完整 id `claude-sonnet-5` 還會 `contains` 到 `claude-sonnet-5-5`），預設強度因此記錯又標錯。現在：
/// 1. key 與 `model` 不分大小寫完全相等 → 用它；
/// 2. `model` 是裸家族別名（opus／sonnet／haiku／fable）→ 只看 `claude-<別名>-` 開頭的 key，取版本最高的（別名指到該家族最新版）；
///    版本＝`-` 分段的數字段逐段比數值，`[1m]` 這類非數字尾巴剝掉；
/// 3. 其他（完整 id 但沒有完全相等的 key）→ `None`，不做子字串比對。
pub fn claude_effort_override(per_model: &BTreeMap<String, String>, model: &str) -> Option<String> {
    let model = model.trim().to_ascii_lowercase();
    if let Some((_, v)) = per_model.iter().find(|(k, _)| k.to_ascii_lowercase() == model) {
        return Some(v.clone());
    }
    if !["opus", "sonnet", "haiku", "fable"].contains(&model.as_str()) {
        return None;
    }
    let prefix = format!("claude-{model}-");
    let version = |rest: &str| -> Vec<u64> {
        rest.split('-').map_while(|seg| seg.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u64>().ok()).collect()
    };
    per_model
        .iter()
        .filter_map(|(k, v)| k.to_ascii_lowercase().strip_prefix(&prefix).map(|rest| (version(rest), k.clone(), v.clone())))
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, _, v)| v)
}

/// What "不帶 `--effort`" resolves to; fills a spawned child's effort, since its argv never says.
/// 讀不到設定檔回 `Err`（不是內建預設）：呼叫端會把這個值記進 bot，記錯了沒有人會再來讀一次。
pub async fn claude_default_effort(app: &Arc<impl crate::hosts::HostsAccess + crate::tools::ToolsEnv + 'static>, host: &str, identity: Option<&str>, alias: &str) -> Result<String> {
    let dir = claude_config_dir(app, host, identity).await?;
    let (global, per_model, _) = read_claude_effort_settings(app, host, dir.as_deref()).await?;
    let alias = alias.to_ascii_lowercase();
    Ok(claude_effort_override(&per_model, &alias)
        .or(global)
        .unwrap_or_else(|| claude_builtin_default_effort(&alias).to_string()))
}

/// settings.json 的 `model` 對到哪個系列別名（`sonnet`、`opus[1m]`、`claude-sonnet-5-5`…）；對不到就 `None`。
/// 只認整段相等或 `claude-<alias>-…`，不做子字串（否則 `claude-opus-…` 會被當成 sonnet 之類的誤判）。
pub fn claude_alias_of(model: &str) -> Option<&'static str> {
    let base = model.split('[').next().unwrap_or("").trim().to_ascii_lowercase();
    ["opus", "sonnet", "haiku", "fable"]
        .into_iter()
        .find(|a| base == *a || base == format!("claude-{a}") || base.starts_with(&format!("claude-{a}-")))
}

/// `default_model` 是帳號 settings.json 的頂層 `model`：`is_default` 只標它對到的那一顆；沒有就一顆都不標（不猜 opus）。
pub fn claude_static_models(global: Option<&str>, per_model: &BTreeMap<String, String>, default_model: Option<&str>) -> Vec<Value> {
    // claude 沒有 per-model effort 清單，每個 alias 都用 `--effort` 那五級。
    let efforts: Vec<Value> = crate::config::efforts_for_kind("claude").iter().map(|e| json!(e)).collect();
    let default_alias = default_model.and_then(claude_alias_of);
    ["opus", "sonnet", "haiku", "fable"]
        .iter()
        .map(|id| {
            let overridden = claude_effort_override(per_model, id);
            let default_effort = overridden.or_else(|| global.map(str::to_string)).unwrap_or_else(|| claude_builtin_default_effort(id).to_string());
            json!({
                "id": id, "display_name": id, "description": "", "is_default": default_alias == Some(*id),
                "default_effort": default_effort, "efforts": efforts, "service_tiers": [],
            })
        })
        .collect()
}

/// agy 的模型 slug：只留 Gemini 3.8 Flash 三檔（2026-10-05 使用者：「agy 的 model 只限 3.8」；`agy models` 還列的 3.7／3.6 Flash 即將下架，
/// 其餘 3.1 Pro、claude、gpt-oss 也不要，見 [`AGY_RETIRED_MODELS`]）。effort 變體已經在 slug 裡，所以 `efforts` 是空的。
/// 寫死的原因：`agy models` 要登入才答、清單隨帳號不同；第二階段改成讀 `agy models --output-format json`。最前面那個是預設。
pub fn agy_static_models() -> Vec<Value> {
    [
        (AGY_DEFAULT_MODEL, "Gemini 3.8 Flash (Medium)"),
        ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
        ("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
    ]
    .iter()
    .enumerate()
    .map(|(i, (id, name))| {
        json!({"id": id, "display_name": name, "description": "", "is_default": i == 0, "default_effort": null, "efforts": [], "service_tiers": []})
    })
    .collect()
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

/// 視窗底部最後一個 `╰` 接到 `╯`（窄 pane 會拆成最多再兩行）。再往上的 `╰` 或
/// `Switched to Grok …` 是對話裡的舊值。
pub fn grok_composer_fragment(screen: &str) -> Option<String> {
    const TAIL_LINES: usize = 8;
    let lines: Vec<&str> = screen.lines().collect();
    let tail = &lines[lines.len().saturating_sub(TAIL_LINES)..];
    let rel = tail.iter().rposition(|line| line.trim().starts_with('╰'))?;
    let mut joined = tail[rel].trim().to_string();
    if !joined.contains('╯') {
        for extra in tail.iter().skip(rel + 1).take(2) {
            joined.push(' ');
            joined.push_str(extra.trim());
            if extra.contains('╯') {
                break;
            }
        }
    }
    let idx = joined.find("Grok ").or_else(|| joined.find("grok "))?;
    Some(joined[idx..].to_string())
}

/// grok TUI 把實際 effort 畫在框底 `╰── Grok 4.6 (high) · always-approve ─╯`。
/// 讀這行只做觀察。啟動不再因為對不上就送 `/effort`（該 slash 會寫進 config.toml）。
pub fn grok_effort_from_screen(screen: &str) -> Option<String> {
    grok_title_model_effort(&grok_composer_fragment(screen)?).1
}



/// 模型清單快取。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait ModelsCache: Send + Sync {
    fn models_cache(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, serde_json::Value)>>;
}
