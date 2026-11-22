//! Antigravity CLI（`agy`）的設定檔與對話紀錄格式（設計：`docs/design/agy-cli-support.md`，SPEC §agy）。
//!
//! 這裡只放**純函式**：怎麼把 AG Man 的條目 merge 進 agy 的 JSON 設定、怎麼從 `transcript_full.jsonl` 讀出一回合的問與答。
//! 讀寫檔案（原子寫、鎖）在 `trust::update_file` 與 `lifecycle::agy_hook`；兩邊都走 `crate::home::dir()`，測試只會碰假 HOME。
//!
//! 檔案全是 agy 與使用者的：我們**只動自己那個鍵**（`trustedWorkspaces` 的元素、`statusLine`（沒被使用者換掉時）、hooks.json 的
//! 具名 hook），其他一個字不改；JSON 讀不懂就回錯、不覆寫（agy 自己遇到解析失敗也是拒絕覆寫）。

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 設定目錄只認 `$HOME`（設計 A.3：`XDG_*` 都無效）。
pub fn settings_path(home: &Path) -> PathBuf {
    home.join(".gemini").join("antigravity-cli").join("settings.json")
}

/// 全域 customization 根目錄下的 hooks 檔（設計 A.6）。
pub fn hooks_path(home: &Path) -> PathBuf {
    home.join(".gemini").join("config").join("hooks.json")
}

/// dispatcher 腳本的檔名；`statusLine` 指令是不是我們的，就靠這個名字認。
pub const DISPATCH_SH: &str = "agy-hook.sh";

/// hooks.json 裡我們那個具名 hook 的名字；每個 daemon 實例各一個（跟 grok 的 `agents-manager-<slug>.json` 同一個道理）。
pub fn hook_name(instance: Option<&str>) -> String {
    match instance {
        Some(slug) => format!("agents-manager-{slug}"),
        None => "agents-manager".to_string(),
    }
}

/// agy 會在第一則 prompt 才建立對話，所以只訂這三個（`SessionStart` 沒有文件但 1.2.16 會觸發，`PreInvocation` 補身份）。
/// **形狀**：這三個事件是**扁平**的 handler 陣列；`PreToolUse`／`PostToolUse` 才要包 `{matcher, hooks}`——形狀錯了整個檔會被 agy 丟掉。
pub const HOOK_EVENTS: [&str; 3] = ["SessionStart", "PreInvocation", "Stop"];

fn parse_object(existing: &str, what: &str) -> Result<serde_json::Map<String, Value>> {
    let clean = existing.trim_start_matches('\u{feff}').trim();
    if clean.is_empty() {
        return Ok(Default::default());
    }
    match serde_json::from_str::<Value>(clean).with_context(|| format!("{what} is not valid JSON"))? {
        Value::Object(o) => Ok(o),
        _ => bail!("{what} is not a JSON object"),
    }
}

fn render(root: serde_json::Map<String, Value>) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(&Value::Object(root))?))
}

/// `settings.json` 的 `trustedWorkspaces`（信任框接受後 agy 自己寫的鍵，絕對路徑陣列；設計 A.5）。`None`＝都已經在裡面。
pub fn trusted_workspaces_merge(existing: &str, paths: &[String]) -> Result<Option<String>> {
    let mut root = parse_object(existing, "agy `settings.json`")?;
    let list = root.entry("trustedWorkspaces").or_insert_with(|| json!([]));
    let Some(list) = list.as_array_mut() else { bail!("`trustedWorkspaces` in agy `settings.json` is not an array") };
    let mut changed = false;
    for p in paths {
        if !list.iter().any(|v| v.as_str() == Some(p.as_str())) {
            list.push(json!(p));
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    render(root).map(Some)
}

/// 這個 `statusLine` 指令是不是我們寫的（以 dispatcher 檔名認，資料目錄換過也認得出來）。
fn is_our_status_command(command: &str) -> bool {
    command.contains(DISPATCH_SH)
}

/// `statusLine`：沒設就寫成我們的（疊在 agy 內建的下面，`stack_with_default`）；已經是我們的就對齊成這顆 daemon 的 dispatcher；
/// **使用者自己設的不碰**（`None`，狀態列信標就沒有，畫面判讀與 hook 照常）。
pub fn statusline_merge(existing: &str, command: &str) -> Result<Option<String>> {
    let mut root = parse_object(existing, "agy `settings.json`")?;
    let want = json!({"type": "command", "command": command, "stack_with_default": true});
    match root.get("statusLine") {
        None | Some(Value::Null) => {}
        Some(cur) => {
            let ours = cur.get("command").and_then(Value::as_str).is_some_and(is_our_status_command);
            if !ours || *cur == want {
                return Ok(None);
            }
        }
    }
    root.insert("statusLine".into(), want);
    render(root).map(Some)
}

/// `hooks.json` 的具名 hook：整個 `<name>` 鍵是我們的（換成目前的 dispatcher），其他具名 hook、其他頂層鍵原樣保留。`None`＝已經對了。
pub fn hooks_merge(existing: &str, name: &str, dispatcher: &str) -> Result<Option<String>> {
    hooks_merge_with(existing, name, dispatcher, false)
}

/// 本機版：另外訂 `PreToolUse`（廣域 kill 守門，#1884）。**包 `{matcher, hooks}`**，不是扁平陣列。遠端用 [`hooks_merge`]：
/// 遠端的 dispatcher 只會 spool、沒有守門邏輯，訂了只是每個工具呼叫多寫一筆。
pub fn hooks_merge_guarded(existing: &str, name: &str, dispatcher: &str) -> Result<Option<String>> {
    hooks_merge_with(existing, name, dispatcher, true)
}

fn hooks_merge_with(existing: &str, name: &str, dispatcher: &str, kill_guard: bool) -> Result<Option<String>> {
    let mut root = parse_object(existing, "agy `hooks.json`")?;
    let quoted = crate::hosts::sh_quote(dispatcher);
    let mut entry = serde_json::Map::new();
    entry.insert("enabled".into(), json!(true));
    for event in HOOK_EVENTS {
        entry.insert(event.into(), json!([{"type": "command", "command": format!("{quoted} {event}"), "timeout": 5}]));
    }
    if kill_guard {
        entry.insert("PreToolUse".into(), json!([{"matcher": "*", "hooks": [{"type": "command", "command": format!("{quoted} PreToolUse"), "timeout": 5}]}]));
    }
    let entry = Value::Object(entry);
    if root.get(name) == Some(&entry) {
        return Ok(None);
    }
    root.insert(name.to_string(), entry);
    render(root).map(Some)
}

/// `<USER_REQUEST>\n…\n</USER_REQUEST>` 裡的使用者原文（後面附的 `<ADDITIONAL_METADATA>` 等不要）。沒有標籤＝`None`。
pub fn user_request(content: &str) -> Option<String> {
    let start = content.find("<USER_REQUEST>")? + "<USER_REQUEST>".len();
    let end = content[start..].find("</USER_REQUEST>")? + start;
    Some(content[start..end].trim().to_string())
}

/// 一行 transcript 若是使用者輸入，回原文。格式（實測 1.2.16）：
/// `{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","content":"<USER_REQUEST>…"}`。
pub fn user_text(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("USER_INPUT") {
        return None;
    }
    user_request(v.get("content")?.as_str()?)
}

/// 一行 transcript 若是給使用者看的模型文字，回那段文字。真機（1.2.16，2026-10-04）：`{"source":"MODEL","type":"PLANNER_RESPONSE",
/// "status":"DONE","content":"PONG","input_tokens":…}`，內容是字串，工具呼叫夾在中間時最後一則才是給使用者的答案。`NOTIFY_USER`
/// （agent 主動通知）**沒見過**，照 binary 內的型別名一併收；內容是物件就試常見的文字欄位。認不出來就略過——寧可少讀，不要把工具輸出當成回覆。
pub fn assistant_text(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    let ty = v.get("type").and_then(Value::as_str)?.to_ascii_uppercase();
    if !(ty.contains("PLANNER_RESPONSE") || ty.contains("NOTIFY_USER")) {
        return None;
    }
    let text = match v.get("content")? {
        Value::String(s) => s.clone(),
        Value::Object(o) => ["response", "text", "message", "content", "notification"].iter().find_map(|k| o.get(*k).and_then(Value::as_str))?.to_string(),
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// 最近一回合：最後一則使用者輸入，與它之後最後一段模型文字。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Exchange {
    pub user: Option<String>,
    pub assistant: Option<String>,
}

pub fn last_exchange(jsonl: &str) -> Exchange {
    let mut out = Exchange::default();
    for line in jsonl.lines() {
        if let Some(u) = user_text(line) {
            out = Exchange { user: Some(u), assistant: None };
        } else if let Some(a) = assistant_text(line) {
            out.assistant = Some(a);
        }
    }
    out
}

/// 一問一答：`USER_INPUT` 與它之後最後一段 `PLANNER_RESPONSE`（`lifecycle::grok_transcript` 把它記成回合，沒有 hook 的 agy child 靠這個）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// `USER_INPUT` 的 `step_index`：同一段對話內單調遞增、不重複，拿來當回合鑰匙。
    pub step_index: u64,
    pub prompt: String,
    pub reply: Option<String>,
    /// 結束了：後面接了下一問（被打斷也算），或最後一則紀錄就是給使用者的回覆（沒有工具步驟接在後面＝還在跑）。
    pub closed: bool,
}

pub fn parse_turns(jsonl: &str) -> Vec<Turn> {
    struct Cur {
        turn: Turn,
        reply_is_last: bool,
    }
    let mut done: Vec<Turn> = Vec::new();
    let mut cur: Option<Cur> = None;
    let finish = |c: Cur, by_next: bool, done: &mut Vec<Turn>| {
        let mut t = c.turn;
        t.closed = by_next || (t.reply.is_some() && c.reply_is_last);
        done.push(t);
    };
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(prompt) = user_text(line) {
            if let Some(c) = cur.take() {
                finish(c, true, &mut done);
            }
            let step_index = v.get("step_index").and_then(Value::as_u64).unwrap_or(done.len() as u64);
            cur = Some(Cur { turn: Turn { step_index, prompt, reply: None, closed: false }, reply_is_last: false });
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        let finished = v.get("status").and_then(Value::as_str).is_none_or(|s| s.eq_ignore_ascii_case("DONE"));
        match assistant_text(line).filter(|_| finished) {
            Some(text) => {
                c.turn.reply = Some(text);
                c.reply_is_last = true;
            }
            None => c.reply_is_last = false,
        }
    }
    if let Some(c) = cur.take() {
        finish(c, false, &mut done);
    }
    done
}

/// 最後一筆模型回覆的整段 context＝`input_tokens`（未命中快取的新 token）＋`cache_read_tokens`（命中快取讀進來的 token）。
/// agy 的 `input_tokens` 只算未命中快取的部分，只取它會把 context 低估（真機：3415＋140185＝143600，只顯示 3.4k）。
/// `cache_read_tokens` 缺就當 0；新對話第一問沒有快取，只有 `input_tokens`（真機 11824）。
pub fn last_input_tokens(jsonl: &str) -> Option<i64> {
    jsonl.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        assistant_text(l)?;
        let input = v.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
        let cached = v.get("cache_read_tokens").and_then(Value::as_i64).unwrap_or(0);
        Some(input + cached).filter(|n| *n > 0)
    })
}

/// 最後一筆 assistant 回覆的 `created_at`（UTC ISO）：agy 的「最後一次 API 活動」（`cache_clock` 用它算快取倒數）。
pub fn last_assistant_at(jsonl: &str) -> Option<String> {
    jsonl.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        assistant_text(l)?;
        v.get("created_at").and_then(Value::as_str).map(str::to_owned)
    })
}

/// `runs.status_json` 給網頁讀的精簡版（claude statusLine 的形狀，`normalize` 認得）：模型，與 context 的 token 數、視窗大小、用量百分比。
/// 沒有 statusLine（舊版 agy、沒有 hook 的子 agent）時，視窗大小來自 `[agy] context_window_tokens`（預設 1,048,576，可調到 2,000,000），百分比＝token 數／視窗大小×100（取一位小數，上限 100）。
pub fn status_json(model: Option<&str>, context_tokens: Option<i64>, context_window_tokens: u64) -> Option<String> {
    if model.is_none() && context_tokens.is_none() {
        return None;
    }
    let mut v = json!({});
    if let Some(m) = model {
        v["model"] = json!({"id": m, "display_name": m});
    }
    if let Some(n) = context_tokens {
        let size = context_window_tokens.max(1);
        v["context_window"] = json!({
            "total_input_tokens": n,
            "context_window_size": size,
            "used_percentage": used_percentage(n, size),
        });
    }
    Some(v.to_string())
}

/// 這份 `runs.status_json` 是不是由 agy 的 `statusLine`（`state` 事件）寫的。
/// 有 statusLine 時（帶 `_agm_statusline: true` 或它的真實 `context_window_size`／`agent_state`），優先於 Stop hook／transcript 估算與 `[agy] context_window_tokens`。
pub fn has_statusline(status_json: Option<&str>) -> bool {
    let Some(raw) = status_json else { return false };
    let Ok(v) = serde_json::from_str::<Value>(raw) else { return false };
    v.get("_agm_statusline").and_then(Value::as_bool) == Some(true)
        || v.get("agent_state").and_then(Value::as_str).is_some()
}

/// 從 agy `statusLine` 的 `model.{id, display_name, effort}` 組出 AG Man 使用的模型 slug（如 `gemini-4-argon-medium`）。
///
/// agy 的模型清單與 `--model` 把 effort 嵌在 slug 尾端（`gemini-4-argon-medium`），但 statusLine 可能回：
/// 1. `id` 已含 effort（如 `gemini-4-argon-medium`）；
/// 2. `id` 不含 effort（如 `gemini-4-argon`）＋ `effort: "medium"` → 拼成 `gemini-4-argon-medium`（若拼出來過 [`crate::models::agy_model_allowed`] 就取拼好的，否則像 `gemini-pro-orchestrator-neon` 這種固定強度的模型維持原 `id`）。
/// 不過 `agy_model_allowed` 的 id（例如未核准的第三方模型）回 `None`。
pub fn model_slug_from_statusline(payload: &Value) -> Option<String> {
    let model = payload.get("model")?.as_object()?;
    let id = model.get("id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())?;
    let id_lower = id.to_ascii_lowercase();
    if crate::models::agy_is_orchestrator(&id_lower) {
        return Some(id_lower);
    }
    let effort = model
        .get("effort")
        .and_then(Value::as_str)
        .or_else(|| payload.pointer("/effort/level").and_then(Value::as_str))
        .or_else(|| payload.get("effort").and_then(Value::as_str))
        .map(str::trim)
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(str::to_ascii_lowercase);
    if let Some(eff) = &effort {
        let suffix = format!("-{eff}");
        if !id_lower.ends_with(&suffix) {
            let combined = format!("{id_lower}-{eff}");
            if crate::models::agy_model_allowed(&combined) {
                return Some(combined);
            }
        }
    }
    crate::models::agy_model_allowed(&id_lower).then_some(id_lower)
}

/// 把 agy 官方 `statusLine` JSON（`https://antigravity.google/docs/cli/statusline`）轉成 `runs.status_json`（形狀照現有 claude statusLine／[`status_json`]，網頁不用大改）。
///
/// * `context_window`：取官方 `context_window_size`（優先於 `fallback_window` 即 `[agy] context_window_tokens`）；
///   token 數優先取 `current_usage`（`input_tokens` + `cache_creation_input_tokens` + `cache_read_input_tokens`，缺項當 0；相容 `cache_read_tokens`／`cache_creation_tokens`），
///   沒有正的 `current_usage` 才取 `total_input_tokens`；`used_percentage` 有官方有限數且 ≥0 就用（夾在 0..=100、一位小數），否則由 token 數 ÷ 視窗大小算。
///   保留 `total_output_tokens`、`remaining_percentage` 與正規化後的 `current_usage`（含 `cache_read_input_tokens`）。
///   若當前 payload 的 `context_window` 還沒帶真讀數（剛啟動重繪全 0 或缺欄位），沿用 `prev_status_json` 已有的 `context_window`。
/// * `model`：保留 `id`（若能與 `effort` 合成合規 slug 則用合成 slug）、`display_name`、`effort`，並同步寫入頂層 `effort: {"level": effort}`（供 `toStatusInfo` 讀取）。
/// * 狀態欄位：寫入 `agent_state`、`tool_confirmation_pending`（bool）、`task_count`（u32，缺省 0）、`pending_input_count`（u32，缺省 0）。
/// * `quota`：原樣記在 `status_json.quota`（只記不取代 `/usage` 探測）。
/// * **絕不寫入 `transcript_path`／`transcriptPath`**（statusLine 給的是摘要版 `transcript.jsonl`，既有防護保留）。
pub fn status_json_from_statusline(
    payload: &Value,
    prev_status_json: Option<&str>,
    fallback_model: Option<&str>,
    fallback_window: u64,
) -> Option<String> {
    let obj = payload.as_object()?;
    let prev: Value = prev_status_json.and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
    let mut out = serde_json::Map::new();
    out.insert("_agm_statusline".into(), json!(true));

    let raw_model = obj.get("model").and_then(Value::as_object);
    let slug = model_slug_from_statusline(payload)
        .or_else(|| raw_model.and_then(|m| m.get("id")).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
        .or_else(|| prev.pointer("/model/id").and_then(Value::as_str).map(str::to_string))
        .or_else(|| fallback_model.map(str::to_string));
    let display = raw_model
        .and_then(|m| m.get("display_name").or_else(|| m.get("displayName")))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| prev.pointer("/model/display_name").and_then(Value::as_str).map(str::to_string))
        .or_else(|| slug.clone());
    let effort = raw_model
        .and_then(|m| m.get("effort"))
        .and_then(Value::as_str)
        .or_else(|| payload.pointer("/effort/level").and_then(Value::as_str))
        .or_else(|| obj.get("effort").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .or_else(|| prev.pointer("/model/effort").and_then(Value::as_str).map(str::to_string))
        .or_else(|| prev.pointer("/effort/level").and_then(Value::as_str).map(str::to_string));
    if let Some(id) = slug {
        let mut m = serde_json::Map::new();
        m.insert("id".into(), json!(id));
        m.insert("display_name".into(), json!(display.unwrap_or_else(|| id.clone())));
        if let Some(eff) = &effort {
            m.insert("effort".into(), json!(eff));
        }
        out.insert("model".into(), Value::Object(m));
    }
    if let Some(eff) = effort {
        out.insert("effort".into(), json!({"level": eff}));
    }

    if let Some(cw) = obj.get("context_window").or_else(|| obj.get("contextWindow")).and_then(Value::as_object) {
        let size = cw
            .get("context_window_size")
            .or_else(|| cw.get("contextWindowSize"))
            .and_then(Value::as_u64)
            .filter(|&n| n > 0)
            .or_else(|| prev.pointer("/context_window/context_window_size").and_then(Value::as_u64).filter(|&n| n > 0))
            .unwrap_or_else(|| fallback_window.max(1));
        let cu = cw.get("current_usage").or_else(|| cw.get("currentUsage")).and_then(Value::as_object);
        let i64_of = |m: &serde_json::Map<String, Value>, k1: &str, k2: &str| -> Option<i64> {
            m.get(k1).or_else(|| m.get(k2)).and_then(Value::as_i64).filter(|&n| n >= 0)
        };
        let (usage_obj, usage_tokens) = if let Some(u) = cu {
            let inp = i64_of(u, "input_tokens", "inputTokens").unwrap_or(0);
            let out_t = i64_of(u, "output_tokens", "outputTokens").unwrap_or(0);
            let cc = i64_of(u, "cache_creation_input_tokens", "cacheCreationInputTokens")
                .or_else(|| i64_of(u, "cache_creation_tokens", "cacheCreationTokens"))
                .unwrap_or(0);
            let cr = i64_of(u, "cache_read_input_tokens", "cacheReadInputTokens")
                .or_else(|| i64_of(u, "cache_read_tokens", "cacheReadTokens"))
                .unwrap_or(0);
            let sum = inp + cc + cr;
            (
                Some(json!({
                    "input_tokens": inp,
                    "output_tokens": out_t,
                    "cache_creation_input_tokens": cc,
                    "cache_read_input_tokens": cr,
                })),
                (sum > 0).then_some(sum),
            )
        } else {
            (None, None)
        };
        let total_in = cw
            .get("total_input_tokens")
            .or_else(|| cw.get("totalInputTokens"))
            .and_then(Value::as_i64)
            .filter(|&n| n >= 0);
        let context_tokens = usage_tokens.or(total_in);
        let pct_from_payload = cw
            .get("used_percentage")
            .or_else(|| cw.get("usedPercentage"))
            .and_then(Value::as_f64)
            .filter(|p| p.is_finite() && *p >= 0.0)
            .map(|p| ((p * 10.0).round() / 10.0).min(100.0));
        let has_reading = context_tokens.is_some_and(|n| n > 0)
            || pct_from_payload.is_some_and(|p| p > 0.0)
            || cw.get("context_window_size").or_else(|| cw.get("contextWindowSize")).and_then(Value::as_u64).is_some_and(|n| n > 0);
        if has_reading {
            let tokens = context_tokens.unwrap_or(0);
            let pct = match pct_from_payload {
                Some(p) if p > 0.0 || tokens == 0 => p,
                _ => used_percentage(tokens, size),
            };
            let mut c = serde_json::Map::new();
            c.insert("total_input_tokens".into(), json!(tokens));
            if let Some(tot_out) = cw.get("total_output_tokens").or_else(|| cw.get("totalOutputTokens")).and_then(Value::as_i64) {
                c.insert("total_output_tokens".into(), json!(tot_out));
            }
            c.insert("context_window_size".into(), json!(size));
            c.insert("used_percentage".into(), json!(pct));
            if let Some(rem) = cw
                .get("remaining_percentage")
                .or_else(|| cw.get("remainingPercentage"))
                .and_then(Value::as_f64)
                .filter(|p| p.is_finite() && *p >= 0.0)
            {
                c.insert("remaining_percentage".into(), json!(((rem * 10.0).round() / 10.0).min(100.0)));
            }
            if let Some(u) = usage_obj {
                c.insert("current_usage".into(), u);
            }
            out.insert("context_window".into(), Value::Object(c));
        } else if let Some(prev_cw) = prev.get("context_window").filter(|v| v.is_object()) {
            out.insert("context_window".into(), prev_cw.clone());
        }
    } else if let Some(prev_cw) = prev.get("context_window").filter(|v| v.is_object()) {
        out.insert("context_window".into(), prev_cw.clone());
    }

    if let Some(state) = obj
        .get("agent_state")
        .or_else(|| obj.get("agentState"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.insert("agent_state".into(), json!(state));
    }
    let confirm = obj
        .get("tool_confirmation_pending")
        .or_else(|| obj.get("toolConfirmationPending"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    out.insert("tool_confirmation_pending".into(), json!(confirm));
    let tasks = obj
        .get("task_count")
        .or_else(|| obj.get("taskCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    out.insert("task_count".into(), json!(tasks));
    let pending_in = obj
        .get("pending_input_count")
        .or_else(|| obj.get("pendingInputCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    out.insert("pending_input_count".into(), json!(pending_in));

    if let Some(q) = obj.get("quota").filter(|q| !q.is_null()) {
        out.insert("quota".into(), q.clone());
    } else if let Some(prev_q) = prev.get("quota").filter(|q| !q.is_null()) {
        out.insert("quota".into(), prev_q.clone());
    }

    Some(Value::Object(out).to_string())
}

/// `tokens／size×100`，四捨五入到一位小數、上限 100，負數當 0。
fn used_percentage(tokens: i64, size: u64) -> f64 {
    let raw = tokens.max(0) as f64 / size as f64 * 100.0;
    ((raw * 10.0).round() / 10.0).min(100.0)
}

/// 一條 `/proc/<pid>/fd/*` 的連結目標若是這個 agy 開著的對話資料庫（`…/antigravity-cli/conversations/<id>.db`，含 `-wal`／`-shm`），回對話 id。
pub fn conversation_from_link(link: &str) -> Option<String> {
    let name = link.rsplit_once("/antigravity-cli/conversations/")?.1;
    let id = name.strip_suffix(".db").or_else(|| name.strip_suffix(".db-wal")).or_else(|| name.strip_suffix(".db-shm"))?;
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| id.to_string())
}

/// 這些行程開著的對話（本機；`proc_root` 是 `/proc`，測試給假的）。同一段對話可能被好幾個 fd 指到，去重。
pub fn open_conversations(proc_root: &Path, pids: &[i32]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for pid in pids {
        let Ok(rd) = std::fs::read_dir(proc_root.join(pid.to_string()).join("fd")) else { continue };
        for e in rd.flatten() {
            let Ok(target) = std::fs::read_link(e.path()) else { continue };
            if let Some(id) = conversation_from_link(&target.to_string_lossy()) {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// 完整對話紀錄的位置（hook 的 `transcriptPath` 同一個檔）。
pub fn transcript_path(home: &Path, conversation_id: &str) -> PathBuf {
    home.join(".gemini").join("antigravity-cli").join("brain").join(conversation_id).join(".system_generated").join("logs").join("transcript_full.jsonl")
}

/// 讀檔尾端最多 `max` 個位元組（對話紀錄會長大；第一行可能被截斷，解析不了就會被略過）。
pub fn read_tail(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// `lsof -Fn` 的輸出（每個開著的檔一行 `n<路徑>`；遠端 `Site::open_conversations` 用）裡的對話 id，去重。
pub fn parse_lsof_conversations(out: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for id in out.lines().filter_map(|l| l.strip_prefix('n')).filter_map(conversation_from_link) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// 遠端 `date -r <紀錄檔>` 的輸出（每行 `<對話 id> <epoch 秒>`，沒有紀錄檔的不印）→ 紀錄檔的修改時間。
pub fn parse_mtimes(out: &str) -> std::collections::HashMap<String, std::time::SystemTime> {
    out.lines()
        .filter_map(|l| {
            let (id, secs) = l.trim().split_once(' ')?;
            let secs: u64 = secs.trim().parse().ok()?;
            Some((id.to_string(), std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)))
        })
        .collect()
}

#[cfg(test)]
mod lsof_and_mtime_tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    /// 遠端 lsof 的輸出：對話 id 去重、`-wal`／`-shm` 認得、非對話檔與 `p`／`f` 行略過。
    #[test]
    fn lsof_lines_name_the_open_conversations_once() {
        let dir = "/Users/me/.gemini/antigravity-cli/conversations";
        let out = format!("p901\nn/Users/me/.zshrc\nn{dir}/abc-1.db\nn{dir}/abc-1.db-wal\nn{dir}/abc-1.db-shm\nn{dir}/xyz.db\nn{dir}/bad.name.db\n/tmp/x\n\n");
        assert_eq!(parse_lsof_conversations(&out), ["abc-1", "xyz"]);
        assert!(parse_lsof_conversations("").is_empty(), "空輸出");
        assert!(parse_lsof_conversations("p901\nn/tmp/y.db\n").is_empty(), "不在 conversations 底下");
    }

    /// 遠端 `date -r` 的輸出：每行 `id epoch`；壞行、空行略過。
    #[test]
    fn date_lines_become_transcript_times() {
        let m = parse_mtimes("abc-1 1700000000\nxyz  1700000100 \nbroken\n\nq notanumber\n");
        assert_eq!(m.len(), 2);
        assert_eq!(m["abc-1"], UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(m["xyz"], UNIX_EPOCH + Duration::from_secs(1_700_000_100));
        assert!(parse_mtimes("").is_empty(), "空輸出");
    }
}
