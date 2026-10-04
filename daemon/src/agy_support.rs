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

/// 一行 transcript 若是給使用者看的模型文字，回那段文字。**未實測**（設計 §6：登入後才取得樣本）：binary 內的步驟型別有
/// `PLANNER_RESPONSE`（模型回覆）與 `NOTIFY_USER`（agent 主動通知），兩者都收；內容是字串就直接用，是物件就試常見的文字欄位。
/// 認不出來就略過——寧可少讀，不要把工具輸出當成回覆。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_workspaces_are_appended_once_and_everything_else_is_kept() {
        let existing = r#"{"colorScheme":"dark","trustedWorkspaces":["/a"],"permissions":{"allow":["command(git)"]}}"#;
        let out = trusted_workspaces_merge(existing, &["/a".into(), "/b".into()]).unwrap().expect("/b is new");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["trustedWorkspaces"], json!(["/a", "/b"]));
        assert_eq!(v["colorScheme"], "dark");
        assert_eq!(v["permissions"]["allow"], json!(["command(git)"]));
        assert_eq!(trusted_workspaces_merge(&out, &["/b".into()]).unwrap(), None, "already trusted: untouched");
        let fresh: Value = serde_json::from_str(&trusted_workspaces_merge("", &["/x".into()]).unwrap().unwrap()).unwrap();
        assert_eq!(fresh["trustedWorkspaces"], json!(["/x"]));
    }

    #[test]
    fn unreadable_or_wrong_shaped_settings_are_never_overwritten() {
        assert!(trusted_workspaces_merge("{not json", &["/a".into()]).is_err());
        assert!(trusted_workspaces_merge("[1,2]", &["/a".into()]).is_err());
        assert!(trusted_workspaces_merge(r#"{"trustedWorkspaces":"/a"}"#, &["/a".into()]).is_err());
        assert!(statusline_merge("{not json", "x").is_err());
        assert!(hooks_merge("[]", "agents-manager", "/d/agy-hook.sh").is_err());
    }

    #[test]
    fn statusline_is_ours_to_set_only_when_unset_or_already_ours() {
        let cmd = "/data/agy-hook.sh state";
        let out = statusline_merge(r#"{"colorScheme":"dark"}"#, cmd).unwrap().expect("unset: written");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["statusLine"], json!({"type": "command", "command": cmd, "stack_with_default": true}));
        assert_eq!(v["colorScheme"], "dark");
        assert_eq!(statusline_merge(&out, cmd).unwrap(), None, "idempotent");
        // 資料目錄換了：我們的舊指令對齊成新的。
        let moved = statusline_merge(&out, "/new/agy-hook.sh state").unwrap().expect("ours, stale path");
        assert!(moved.contains("/new/agy-hook.sh"), "{moved}");
        // 使用者自己的 statusLine 不碰。
        let mine = r#"{"statusLine":{"type":"command","command":"/home/me/bar.sh"}}"#;
        assert_eq!(statusline_merge(mine, cmd).unwrap(), None);
    }

    #[test]
    fn our_named_hook_is_flat_for_the_three_events_and_other_hooks_survive() {
        let existing = r#"{"other-tool":{"enabled":true,"Stop":[{"type":"command","command":"x"}]},"version":1}"#;
        let out = hooks_merge(existing, "agents-manager", "/my data/agy-hook.sh").unwrap().expect("new entry");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["other-tool"], serde_json::from_str::<Value>(existing).unwrap()["other-tool"]);
        assert_eq!(v["version"], 1);
        let ours = &v["agents-manager"];
        assert_eq!(ours["enabled"], true);
        for event in HOOK_EVENTS {
            // 扁平：陣列元素直接是 handler，不是 `{matcher, hooks:[…]}`——形狀錯了 agy 會整個丟掉這個檔。
            let h = &ours[event][0];
            assert_eq!(h["type"], "command", "{event}");
            assert_eq!(h["timeout"], 5);
            assert_eq!(h["command"], format!("'/my data/agy-hook.sh' {event}"));
            assert!(h.get("hooks").is_none() && h.get("matcher").is_none(), "{event}: flat handler");
        }
        assert_eq!(ours.as_object().unwrap().len(), 1 + HOOK_EVENTS.len(), "nothing but enabled + the three events");
        assert_eq!(hooks_merge(&out, "agents-manager", "/my data/agy-hook.sh").unwrap(), None, "idempotent");
        let repointed = hooks_merge(&out, "agents-manager", "/elsewhere/agy-hook.sh").unwrap().expect("dispatcher moved");
        assert!(repointed.contains("/elsewhere/agy-hook.sh") && !repointed.contains("/my data/"));
    }

    #[test]
    fn instances_get_their_own_hook_name() {
        assert_eq!(hook_name(None), "agents-manager");
        assert_eq!(hook_name(Some("dev")), "agents-manager-dev");
    }

    const USER: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-04T00:00:00Z","content":"<USER_REQUEST>\nreply with OK\n</USER_REQUEST>\n<ADDITIONAL_METADATA>cwd=/x</ADDITIONAL_METADATA>"}"#;

    #[test]
    fn user_text_strips_the_request_tags_and_the_metadata() {
        assert_eq!(user_text(USER).as_deref(), Some("reply with OK"));
        assert_eq!(user_text(r#"{"type":"RUN_COMMAND","content":"<USER_REQUEST>x</USER_REQUEST>"}"#), None, "only USER_INPUT");
        assert_eq!(user_text(r#"{"type":"USER_INPUT","content":"no tags"}"#), None);
        assert_eq!(user_text("not json"), None);
    }

    #[test]
    fn the_last_exchange_is_the_last_user_input_and_the_last_model_text_after_it() {
        let old = r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"old answer"}"#;
        let tool = r#"{"step_index":3,"type":"RUN_COMMAND","content":"ls"}"#;
        let mid = r#"{"step_index":2,"type":"PLANNER_RESPONSE","content":"looking"}"#;
        let fin = r#"{"step_index":4,"type":"PLANNER_RESPONSE","content":"  OK  "}"#;
        let text = [USER, old, USER.replace("reply with OK", "again").as_str(), mid, tool, fin, "garbage line"].join("\n");
        assert_eq!(last_exchange(&text), Exchange { user: Some("again".into()), assistant: Some("OK".into()) });
        assert_eq!(last_exchange(USER), Exchange { user: Some("reply with OK".into()), assistant: None });
        assert_eq!(last_exchange(""), Exchange::default());
    }

    #[test]
    fn model_text_is_read_from_a_string_or_a_text_field_and_tool_steps_are_skipped() {
        assert_eq!(assistant_text(r#"{"type":"CORTEX_STEP_TYPE_NOTIFY_USER","content":{"message":"done"}}"#).as_deref(), Some("done"));
        assert_eq!(assistant_text(r#"{"type":"PLANNER_RESPONSE","content":""}"#), None);
        assert_eq!(assistant_text(r#"{"type":"RUN_COMMAND","content":"cat file"}"#), None);
        assert_eq!(assistant_text(r#"{"type":"PLANNER_RESPONSE","content":[1]}"#), None);
    }

    #[test]
    fn the_tail_read_keeps_only_the_end_of_a_big_file() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-agy-tail-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.jsonl");
        std::fs::write(&f, format!("{}\n{USER}\n", "x".repeat(5000))).unwrap();
        let tail = read_tail(&f, 1000).unwrap();
        assert!(tail.len() <= 1000);
        assert_eq!(last_exchange(&tail).user.as_deref(), Some("reply with OK"));
        assert!(read_tail(&dir.join("missing"), 10).is_err());
    }
}
