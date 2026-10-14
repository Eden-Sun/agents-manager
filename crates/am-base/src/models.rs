//! `GET /api/models`: live model lists from the agent CLIs, per host, cached 10 min.
//!
//! * codex: `codex app-server` `model/list`. The server never exits on its own, so we kill it
//!   locally / let `awk` cut the remote stream at the answer.
//! * grok: `grok models` text + `~/.grok/models_cache.json` efforts (`config.toml` fallback).
//! * claude: static aliases (no list API); `default_effort` = vendor built-in (same on every account),
//!   `account_default_effort` = that identity's remembered `settings.json` value when it differs (#1679).

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
            let remapped_default = arr.iter()
                .find(|m| m.get("isDefault").and_then(Value::as_bool) == Some(true))
                .and_then(|m| remap_deprecated_model("codex", m.get("id")?.as_str()?));
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_string();
                    if remap_deprecated_model("codex", &id).is_some() {
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
                        "is_default": remapped_default.map(|target| target == id)
                            .unwrap_or_else(|| m.get("isDefault").and_then(Value::as_bool).unwrap_or(false)),
                        "default_effort": m.get("defaultReasoningEffort").and_then(|x| x.as_str()),
                        "efforts": efforts,
                        "service_tiers": tiers,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// agy 的預設模型（清單第一個）；不收的 agy 模型一律換成它。
pub const AGY_DEFAULT_MODEL: &str = "gemini-3.8-flash-medium";

/// agy 收的模型（2026-10-14 使用者：「agy 只收 Gemini，版本 3.8 含以上」）：slug 以 `gemini-` 開頭，版本號 `gemini-<主>[.<次>]-…` ≥ 3.8。
/// 例：`gemini-3.8-flash-high`、`gemini-4-argon`、`gemini-3.10-…` 收；`gemini-3.7-…`、`gemini-3-…`、`claude-*`、`gpt-oss-*` 不收。
/// 權威在這裡，web 的 `agyModelAllowed`（`web/src/api/types.ts`）是同一條判斷式，兩邊要一起改。
pub fn agy_model_allowed(slug: &str) -> bool {
    let Some(ver) = slug.strip_prefix("gemini-").and_then(|rest| rest.split('-').next()) else { return false };
    let mut parts = ver.split('.').map(|p| p.parse::<u32>());
    let Some(Ok(major)) = parts.next() else { return false };
    let minor = match parts.next() {
        None => 0,
        Some(Ok(m)) => m,
        Some(Err(_)) => return false,
    };
    parts.next().is_none() && (major, minor) >= (3, 8)
}

/// 解析 `agy models` 的輸出（每行 `slug<TAB>顯示名`，純文字）：只留 [`agy_model_allowed`] 的，預設排第一。
/// 未登入時的訊息沒有 tab、slug 也不是 `gemini-` 開頭，會被過濾掉，結果為空，呼叫端就退回 [`agy_static_models`]。
pub fn agy_models_from_text(text: &str) -> Vec<Value> {
    let mut models: Vec<Value> = text
        .lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let id = cols.next()?.trim();
            if !agy_model_allowed(id) {
                return None;
            }
            let name = cols.next().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(id);
            Some(json!({"id": id, "display_name": name, "description": "", "is_default": false, "default_effort": null, "efforts": [], "service_tiers": []}))
        })
        .collect();
    models.sort_by_key(|m| m["id"].as_str() != Some(AGY_DEFAULT_MODEL));
    if let Some(first) = models.first_mut() {
        first["is_default"] = json!(true);
    }
    models
}

/// 本機探測 `agy models` 的指令：拋棄式 cwd、不讀 stdin、關自動更新、跑完刪目錄（同 `quota_agy::probe_script`）。
pub fn agy_models_script(exe: &str) -> String {
    format!(
        "d=$(mktemp -d 2>/dev/null) || exit 1; cd \"$d\" || exit 1; \
         AGY_CLI_DISABLE_AUTO_UPDATE=true {exe} models </dev/null; rc=$?; cd / ; rm -rf \"$d\"; exit $rc",
        exe = sh_quote(exe)
    )
}

/// Map only explicitly retired names and aliases; other full model ids stay intact.
pub fn remap_deprecated_model(kind: &str, model: &str) -> Option<&'static str> {
    if kind == "agy" && !agy_model_allowed(model) {
        return Some(AGY_DEFAULT_MODEL);
    }
    match (kind, model) {
        ("codex", "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-6-sol") => Some("gpt-6.1-sol"),
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
            v["is_default"] = json!(v["id"].as_str() == Some(canonical_model("codex", &model)));
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

/// Unparsable is `(None, {})`, never an error. Overrides are keyed by real model id (`claude-opus-5-5`);
/// 別名對 id 看 [`claude_effort_override`]。
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

/// 非一般檔（FIFO、裝置）當成缺檔；一般檔最多讀 1 MiB（issue #1074：FIFO 沒有寫入端時 `cat` 永遠等，指到大檔就吃光記憶體）。
pub fn optional_cat_script(path_expr: &str) -> String {
    format!("[ -f {path_expr} ] && head -c 1048576 {path_expr} 2>/dev/null || true")
}

/// 本機讀設定檔的上限（同 `hosts::SSH_EXEC_TIMEOUT` 的量級）：逾時整個行程群組砍掉，回 Err（#268：讀不到是錯誤，不是空字串）。
const LOCAL_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 讀那台主機上的一個選用設定檔。**缺檔是答案（回空字串），讀不到是錯誤**：以前兩者都變成 `""`，
/// ssh 逾時／連不上就被當成「沒設定」，接下來拿內建預設值當成事實記下去（#268）。
/// 遠端 `ssh_exec` 看 exit code，所以缺檔那條要自己收成 0（`|| true`），不然缺檔也是 Err。
pub async fn read_optional_text(app: &impl crate::hosts::HostsAccess, host: &str, path_expr: &str) -> Result<String> {
    let script = optional_cat_script(path_expr);
    if host == LOCAL_HOST {
        // 本機也要有上限：走 hosts::sh_local_stdout（逾時整個行程群組砍掉），不是 Command::output() 外面包 timeout（子行程會留下）。
        crate::hosts::sh_local_stdout(&script, LOCAL_READ_TIMEOUT, &format!("read {path_expr}")).await
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

/// claude 的「廠推薦」一律 `medium`（使用者 2026-10-13 裁示：「廠推一律用 medium 就好，這樣比較簡單」）。
/// 不照模型分：Anthropic 文件（https://code.claude.com/docs/en/model-config ）裡 Opus／Sonnet／Haiku 5.5 本來就是 `medium`，
/// Fable 5.1、Opus 5 等文件寫 `high`——這裡刻意不跟，換來每個模型、每個帳號同一個答案。
const CLAUDE_BUILTIN_DEFAULT_EFFORT: &str = "medium";

/// 參數留著只為了呼叫端不用改；回傳值不看模型。
pub fn claude_builtin_default_effort(_alias: &str) -> &'static str {
    CLAUDE_BUILTIN_DEFAULT_EFFORT
}

/// 頂層 `effortLevel` 對這個模型算不算數。文件：「a top-level `effortLevel` in your user settings file doesn't count for
/// Opus 5.5 … it keeps applying where it applied before, on Opus 5, Fable 5.1, and earlier models」。文件點名的只有 Opus 5.5，
/// 其他照舊算數。
pub fn claude_global_effort_applies(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    !claude_current_model_id(&model).unwrap_or(&model).starts_with("claude-opus-5-5")
}

/// 裸別名現在對到的完整 model id（`modelSettings` 與對話紀錄用的是完整 id）。opus 跟 [`remap_deprecated_model`] 一致；
/// 其餘照 Claude Code 2.1.296 的別名解析。新版上線前這張表是舊的 → 查不到列，退回內建預設（不會拿舊版的列頂替）。
const CLAUDE_CURRENT_MODEL_IDS: [(&str, &str); 4] = [
    ("opus", "claude-opus-5-5"),
    ("sonnet", "claude-sonnet-5-5"),
    ("haiku", "claude-haiku-5-5"),
    ("fable", "claude-fable-5-1"),
];

/// 裸家族別名（opus／sonnet／haiku／fable，不分大小寫）→ 目前版本的完整 id；完整 id 或別的字串回 `None`。
pub fn claude_current_model_id(alias: &str) -> Option<&'static str> {
    let alias = alias.trim().to_ascii_lowercase();
    CLAUDE_CURRENT_MODEL_IDS.iter().find(|(a, _)| *a == alias).map(|(_, id)| *id)
}

/// `modelSettings` 裡該模型的 `effortLevel` 覆寫（issue #937、#1679）。
///
/// CLI 自己是照完整 model id 查 `modelSettings`，沒有那一列就用內建預設，所以這裡也只做完全相等（不分大小寫）：
/// 1. `model` 是完整 id → 只認 key 完全相等的那一列，不做子字串比對（#937：`claude-sonnet-5` 不會 `contains` 到 `claude-sonnet-5-5`）；
/// 2. `model` 是裸家族別名 → 先換成目前版本的完整 id（[`claude_current_model_id`]）再查。檔案裡只有**舊版**的列就是沒有，
///    不拿舊版的值頂替（#1679：cc1 只有 `claude-opus-5: xhigh`，opus-5-5 實際跑的是內建預設）。
pub fn claude_effort_override(per_model: &BTreeMap<String, String>, model: &str) -> Option<String> {
    let model = model.trim().to_ascii_lowercase();
    let key = claude_current_model_id(&model).unwrap_or(&model);
    per_model.iter().find(|(k, _)| k.to_ascii_lowercase() == key).map(|(_, v)| v.clone())
}

/// 帳號 settings.json 記住的強度：該模型目前版本那一列 > 全域 `effortLevel`（Opus 5.5 不吃全域那個，見 [`claude_global_effort_applies`]）。**不是廠推薦**，是 CLI 記住的「這個帳號上次選的」。
/// 沒有就 `None`。
pub fn claude_account_effort(global: Option<&str>, per_model: &BTreeMap<String, String>, model: &str) -> Option<String> {
    claude_effort_override(per_model, model).or_else(|| global.filter(|_| claude_global_effort_applies(model)).map(str::to_string))
}

/// What "不帶 `--effort`" resolves to; fills a spawned child's effort, since its argv never says.
/// 讀不到設定檔回 `Err`（不是內建預設）：呼叫端會把這個值記進 bot，記錯了沒有人會再來讀一次。
pub async fn claude_default_effort(app: &Arc<impl crate::hosts::HostsAccess + crate::tools::ToolsEnv + 'static>, host: &str, identity: Option<&str>, alias: &str) -> Result<String> {
    let dir = claude_config_dir(app, host, identity).await?;
    let (global, per_model, _) = read_claude_effort_settings(app, host, dir.as_deref()).await?;
    let alias = alias.to_ascii_lowercase();
    Ok(claude_account_effort(global.as_deref(), &per_model, &alias).unwrap_or_else(|| claude_builtin_default_effort(&alias).to_string()))
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
/// `default_effort` 是廠商內建預設（每個帳號都一樣）；帳號自己記住的值放 `account_default_effort`，跟內建相同或沒有就是 `null`（#1679）。
pub fn claude_static_models(global: Option<&str>, per_model: &BTreeMap<String, String>, default_model: Option<&str>) -> Vec<Value> {
    // claude 沒有 per-model effort 清單，每個 alias 都用 `--effort` 那五級。
    let efforts: Vec<Value> = crate::config::efforts_for_kind("claude").iter().map(|e| json!(e)).collect();
    let default_alias = default_model.and_then(claude_alias_of);
    ["opus", "sonnet", "haiku", "fable"]
        .iter()
        .map(|id| {
            let default_effort = claude_builtin_default_effort(id);
            let account_effort = claude_account_effort(global, per_model, id).filter(|e| e != default_effort);
            json!({
                "id": id, "display_name": id, "description": "", "is_default": default_alias == Some(*id),
                "default_effort": default_effort, "account_default_effort": account_effort, "efforts": efforts, "service_tiers": [],
            })
        })
        .collect()
}

/// agy 的後備模型清單：Gemini 3.8 Flash 三檔（2026-10-05 起的寫死清單）。`fetch` 能讀到 `agy models` 就用它的結果（[`agy_models_from_text`]），
/// 讀不到（未登入、逾時、遠端）才退回這一份。effort 變體已經在 slug 裡，所以 `efforts` 是空的。最前面那個是預設。
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
