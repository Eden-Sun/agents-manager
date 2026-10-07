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

/// agy 會在第一則 prompt 才建立對話，所以只訂這三個（`SessionStart` 沒有文件但 1.2.16 會觸發，`PreInvocation` 補身分）。
/// **形狀**：這三個事件是**扁平**的 handler 陣列；`PreToolUse`／`PostToolUse` 才要包 `{matcher, hooks}`——形狀錯了整個檔會被 agy 丟掉。
pub const HOOK_EVENTS: [&str; 3] = ["SessionStart", "PreInvocation", "Stop"];

fn parse_object(existing: &str, what: &str) -> Result<serde_json::Map<String, Value>> {
    if existing.trim().is_empty() {
        return Ok(Default::default());
    }
    match serde_json::from_str::<Value>(existing).with_context(|| format!("{what} is not valid JSON"))? {
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
    let mut root = parse_object(existing, "agy `hooks.json`")?;
    let quoted = crate::hosts::sh_quote(dispatcher);
    let mut entry = serde_json::Map::new();
    entry.insert("enabled".into(), json!(true));
    for event in HOOK_EVENTS {
        entry.insert(event.into(), json!([{"type": "command", "command": format!("{quoted} {event}"), "timeout": 5}]));
    }
    let entry = Value::Object(entry);
    if root.get(name) == Some(&entry) {
        return Ok(None);
    }
    root.insert(name.to_string(), entry);
    render(root).map(Some)
}

// ───────────────────────── transcript_full.jsonl ─────────────────────────

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
        // 還在產生的步驟（`status` 不是 DONE）不是最終回覆。
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

/// 最後一筆模型回覆的 `input_tokens`＝那次請求讀進去的整段 context（真機：新對話第一問 11824）。
pub fn last_input_tokens(jsonl: &str) -> Option<i64> {
    jsonl.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        assistant_text(l)?;
        v.get("input_tokens").and_then(Value::as_i64).filter(|n| *n > 0)
    })
}

/// `runs.status_json` 給網頁讀的精簡版（claude statusLine 的形狀，`normalize` 認得）：模型，與 context 的 token 數。
/// **視窗大小沒有可靠的來源**（agy 不給），所以不填百分比，不編數字。
pub fn status_json(model: Option<&str>, context_tokens: Option<i64>) -> Option<String> {
    if model.is_none() && context_tokens.is_none() {
        return None;
    }
    let mut v = json!({});
    if let Some(m) = model {
        v["model"] = json!({"id": m, "display_name": m});
    }
    if let Some(n) = context_tokens {
        // 鍵名照 claude statusLine（網頁 `normalizeStatus` 讀 `total_input_tokens`）。
        v["context_window"] = json!({"total_input_tokens": n});
    }
    Some(v.to_string())
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
