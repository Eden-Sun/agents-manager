//! 沒有 hook 的 claude run（典型是 bot 用 `herdr agent start` 開的子 agent，本機或遠端）的對話紀錄（SPEC §12.5b，issue #878）。
//!
//! 以前只能刮畫面：回覆被窄 pane 折行、標「可能不完整」，剛開出來只回了開場白就被當成一個完成的回合。claude 自己有結構化的
//! `<CLAUDE_CONFIG_DIR>/projects/<cwd 編碼>/<session id>.jsonl`（**只讀**），所以對 `runs.adopted = 1`、`bots.inject_hooks = 0` 的 claude run，
//! 對話檔是主要來源、畫面只是備援；`grok_transcript` 把這裡讀到的問答記成回合（跟 grok／agy 同一條路）。
//!
//! * **找 session**：herdr 的 `agent.get` 回的 `agent_session`（`kind: "id"`）就是 claude 當下的 session id——herdr 自己從 claude 的
//!   SessionStart 綁的，不必掃行程，遠端也一樣。先用 agent 名字查，名字被 herdr 拿掉（`agent start` 逾時）時改用 pane id 查；
//!   還沒綁（claude 剛起來）就用 run 記過的，兩者都沒有＝照舊看畫面。找到的記在 `runs.native_session_id`／`transcript_path`。
//! * **讀檔**：`<root>/projects/*/<session id>.jsonl` 的尾巴（一般檔、非 symlink），本機與遠端都用 `sh`。root 依序是：bot 的 `CLAUDE_CONFIG_DIR`、
//!   這個身分在該主機的 config 目錄、`~/.claude`。session id 只收英數與 `-`／`_`。
//! * **一問一答**：`type: user` 且是人打的字（不是 tool_result、`isMeta`、sidechain、slash 指令的回音）算一問；`stop_reason` 不是 `tool_use` 的
//!   `assistant` 文字是這一問的最終回覆（帶 `tool_use` 的旁白不算）；下一問出現也算這一問結束（被打斷、沒有回覆）。鑰匙是 user 那行的 `uuid`。

use super::grok_transcript::Exchange;
use super::s6_ports::GrokTranscriptContext;
use super::*;
use serde_json::Value;

/// 一次最多讀檔尾多少（跟 grok 同一個上限；從一行中間切開時，解析時自然跳過）。
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
const LOG_MARK: &str = "---AM-CLAUDE-LOG---";

/// uuid → 穩定的 `prompt_index`（FNV-1a 64；不用 `DefaultHasher`，重啟才不會把同一問再記一次）。
fn index_of(uuid: &str) -> u64 {
    uuid.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn parts_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for p in parts {
                if p.get("type").and_then(Value::as_str) != Some("text") {
                    return None;
                }
                out.extend(p.get("text").and_then(Value::as_str));
            }
            Some(out.join("\n"))
        }
        _ => None,
    }
}

/// slash 指令的回音、本機指令的輸出、系統提醒：CLI 寫進 user 行，卻不是人打的一問。
fn is_cli_echo(text: &str) -> bool {
    let t = text.trim_start();
    ["<command-name>", "<command-message>", "<command-args>", "<local-command-", "<system-reminder>", "Caveat: The messages below"]
        .iter()
        .any(|p| t.starts_with(p))
}

fn is_interrupt_marker(text: &str) -> bool {
    text.trim_start().starts_with("[Request interrupted by user")
}

pub(crate) fn parse_exchanges(text: &str) -> Vec<Exchange> {
    let mut out: Vec<Exchange> = Vec::new();
    // `tail -c` 會從一行中間切開。切剩的半行解析失敗後，後面的 assistant 仍是那一問的，
    // 不能接到上一問把回覆蓋掉。下一個完整的 user 才重新接上。
    let mut gap = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            gap = true;
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(raw) = v.get("message").and_then(|m| m.get("content")).and_then(parts_text) else { continue };
                if is_interrupt_marker(&raw) {
                    gap = false;
                    if let Some(prev) = out.last_mut() {
                        prev.closed = true;
                    }
                    continue;
                }
                if raw.trim().is_empty() || is_cli_echo(&raw) {
                    continue;
                }
                gap = false;
                if let Some(prev) = out.last_mut() {
                    prev.closed = true;
                }
                let prompt = crate::pasted_content::original(&raw).trim().to_string();
                let uuid = v.get("uuid").and_then(Value::as_str).filter(|u| !u.is_empty());
                out.push(Exchange { prompt, prompt_index: uuid.map(index_of), reply: None, closed: false, at: v.get("timestamp").and_then(Value::as_str).map(str::to_string) });
            }
            Some("assistant") => {
                if gap {
                    continue;
                }
                let Some(cur) = out.last_mut() else { continue };
                let Some(msg) = v.get("message") else { continue };
                // 停在 `tool_use` 的是旁白（後面還有工具要跑）；沒有 stop_reason 的是還在串流的半行。
                let terminal = msg.get("stop_reason").and_then(Value::as_str).is_some_and(|s| s != "tool_use");
                if !terminal {
                    continue;
                }
                let reply = msg.get("content").and_then(parts_text_lossy).unwrap_or_default();
                if !reply.trim().is_empty() {
                    cur.reply = Some(reply.trim().to_string());
                    cur.closed = true;
                }
            }
            _ => {}
        }
    }
    out
}

/// assistant 的 content 有 thinking／tool_use 等其他區塊：只取 `text` 區塊。
fn parts_text_lossy(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

fn read_script(roots: &[String], sid: &str) -> String {
    let list = roots.iter().map(|r| sh_quote(r)).collect::<Vec<_>>().join(" ");
    format!(
        "for r in {list}; do for f in \"$r\"/projects/*/{sid}.jsonl; do if [ -f \"$f\" ] && [ ! -L \"$f\" ]; then printf '%s\\n%s\\n' '{mark}' \"$f\"; tail -c {max} \"$f\"; break 2; fi; done; done; :",
        mark = LOG_MARK,
        max = MAX_READ_BYTES,
    )
}

/// `(path, body)`；沒有這個檔＝`None`。
fn split_output(out: &str) -> Option<(String, String)> {
    let (_, rest) = out.split_once(&format!("{LOG_MARK}\n"))?;
    let (path, body) = rest.split_once('\n')?;
    Some((path.to_string(), body.to_string()))
}

/// `Some((session id, 對話檔尾巴))`；找不到 session、讀不到檔＝`None`（照舊看畫面）。
/// herdr 與身分目錄的事實由 `GrokTranscriptServices` 提供（`runners/claude_child_log.rs` 接 `App`）。
pub(crate) async fn load(app: &impl GrokTranscriptContext, bot: &db::Bot, run: &db::Run, host: &str) -> anyhow::Result<Option<(String, String)>> {
    // herdr 綁的優先（claude `/clear`、`--resume` 會換 session），沒有就用記過的。
    let known = run.native_session_id.clone().filter(|s| super::grok_transcript::valid_session_id(s));
    let Some(sid) = app.claude_herdr_session(run).await.or(known) else { return Ok(None) };
    let roots = app.claude_config_roots(bot, run, host).await;
    if roots.is_empty() {
        return Ok(None);
    }
    let out = app.host_shell(host, &read_script(&roots, &sid)).await?;
    let Some((path, body)) = split_output(&out) else { return Ok(None) };
    if run.native_session_id.as_deref() != Some(sid.as_str()) || run.transcript_path.as_deref() != Some(path.as_str()) {
        sqlx::query("UPDATE runs SET native_session_id = ?, transcript_path = ? WHERE id = ?").bind(&sid).bind(&path).bind(&run.id).execute(app.db()).await?;
        tracing::info!(run = %run.id, session = %sid, host, "claude transcript: bound the pane to its claude session");
    }
    Ok(Some((sid, body)))
}

#[cfg(test)]
#[path = "claude_child_log_tests.rs"]
mod tests;
