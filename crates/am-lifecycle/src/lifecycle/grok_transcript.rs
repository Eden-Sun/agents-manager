//! 沒有 hook 的 grok run（典型是 bot 用 `herdr agent start --kind grok` 開的子 agent）的對話紀錄（SPEC §12.5）。
//!
//! 以前只能刮畫面：窄 pane 把回覆折得七零八落、`…` 截斷，歡迎畫面的選單被當成回覆，一個多小時的工作只留下幾則。
//! grok 自己有結構化的對話檔 `<GROK_HOME>/sessions/<cwd 的 URL 編碼>/<session id>/chat_history.jsonl`（**只讀**），
//! 這裡把它當主要來源，畫面只在找不到 session、讀不到檔時才當備援。
//!
//! - **session 對 pane**：`<GROK_HOME>/active_sessions.json` 列著每個開著的 grok 的 `pid`／`session_id`／`cwd`。
//!   pid 的環境 `HERDR_PANE_ID`（與 `HERDR_SESSION`）指回 pane（`memproc::pids_in_pane`，本機讀 `/proc`、遠端 `ps -E` 走 ssh），
//!   同一個 cwd 開好幾顆 grok 也對得準。環境讀不到時才退回「同 cwd、沒被別的 run 綁走、只有一個」；不只一個就不猜。
//!   找到的 session 記在 `runs.native_session_id`（續行、分叉也用得到），之後不再掃行程。
//! - **一問一答＝一個回合**：`type: user`、沒有 `synthetic_reason`、內容包在 `<user_query>` 裡的才是對話（`<user_info>`、
//!   system reminder、壓縮摘要都不是）；之後第一則沒有 `tool_calls` 的 `assistant` 是這一問的回覆。slash 指令不會進這個檔。
//!   回合的鑰匙是 `turns.(native_session_id, native_turn_id)`：`native_turn_id = p<prompt_index>`；壓縮後重寫進檔、
//!   沒有 `prompt_index` 的那一問用穩定的 FNV `q<hash>`（不用 `DefaultHasher`，重啟才不會再記一次）。
//!   同一句已經在這個 run 裡就不再記；`prompt_index` 被重用到另一句時改用內容鑰匙。已經記過的不再記。
//! - **先認既有回合**：派工開的 in-flight 回合、relay 先記下的使用者訊息、畫面備援收過的回合，prompt 對得上就補進去
//!   （備援抓的回覆換成原文），對不上才另開一筆 `external` 回合；那句是別的 agent 交辦的就照 §6.5d 標 `relay_from`。

use crate::lifecycle::s6_ports::GrokTranscriptContext;
use super::*;
use std::collections::HashSet;

/// 一次最多讀檔尾多少（壓縮會把檔案收小；讀尾巴時第一行可能被切到一半，解析時自然跳過）。
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
const HISTORY_MARK: &str = "---AM-GROK-HISTORY---";

/// `chat_history.jsonl` 裡的一問（與它的回覆）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    pub prompt: String,
    pub prompt_index: Option<u64>,
    pub reply: Option<String>,
    /// 這一問已經結束：有了最終回覆，或後面已經接了下一問（被打斷）。
    pub closed: bool,
    /// 這一問寫進對話檔的時間（RFC 3339，UTC；claude 的 `timestamp`）。其他來源沒有＝None。
    pub at: Option<String>,
}

impl Exchange {
    pub fn key(&self) -> String {
        match self.prompt_index {
            Some(i) => format!("p{i}"),
            None => format!("q{}", crate::supervisor_inbox::short_hash(self.prompt.trim().as_bytes())),
        }
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str).is_none_or(|t| t == "text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 真的對話才回字：`<user_query>…</user_query>` 裡面那段。
fn user_query(v: &Value) -> Option<String> {
    if v.get("synthetic_reason").is_some_and(|r| !r.is_null()) {
        return None;
    }
    let text = text_of(v.get("content")?);
    let start = text.find("<user_query>")? + "<user_query>".len();
    let end = text.rfind("</user_query>").filter(|e| *e >= start).unwrap_or(text.len());
    let q = text[start..end].trim();
    (!q.is_empty()).then(|| q.to_string())
}

pub fn parse_chat_history(text: &str) -> Vec<Exchange> {
    let mut out: Vec<Exchange> = Vec::new();
    let mut gap = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            gap = true;
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                gap = false;
                let Some(prompt) = user_query(&v) else { continue };
                if let Some(prev) = out.last_mut() {
                    prev.closed = true;
                }
                out.push(Exchange { prompt, prompt_index: v.get("prompt_index").and_then(Value::as_u64), reply: None, closed: false, at: None });
            }
            Some("assistant") => {
                if gap {
                    continue;
                }
                let Some(cur) = out.last_mut() else { continue };
                if v.get("tool_calls").and_then(Value::as_array).is_some_and(|c| !c.is_empty()) {
                    continue;
                }
                let reply = v.get("content").map(text_of).unwrap_or_default();
                if !reply.trim().is_empty() {
                    cur.reply = Some(reply.trim().to_string());
                }
                cur.closed = true;
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ActiveSession {
    pub session_id: String,
    #[serde(default)]
    pub pid: i64,
    #[serde(default)]
    pub cwd: String,
}

/// session id 會被拼進 shell glob：只收 grok 實際用的字元（UUID）。
pub fn valid_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn parse_active(text: &str) -> Vec<ActiveSession> {
    serde_json::from_str::<Vec<ActiveSession>>(text.trim())
        .unwrap_or_default()
        .into_iter()
        .filter(|s| valid_session_id(&s.session_id))
        .collect()
}

/// 這顆 pane 的 grok session。`pane_pids` 是環境指回這顆 pane 的行程；讀得到就只認它們，讀不到（空）才退回 cwd，
/// 而且要剛好一個、沒被別的 run 綁走。
pub fn pick_session(active: &[ActiveSession], pane_pids: &[i32], cwd: Option<&str>, taken: &HashSet<String>) -> Option<String> {
    if !pane_pids.is_empty() {
        let mut hits: Vec<&str> = active
            .iter()
            .filter(|s| pane_pids.iter().any(|p| i64::from(*p) == s.pid) && !taken.contains(&s.session_id))
            .map(|s| s.session_id.as_str())
            .collect();
        hits.dedup();
        return (hits.len() == 1).then(|| hits[0].to_string());
    }
    let norm = |p: &str| p.trim_end_matches('/').to_string();
    let cwd = norm(cwd?);
    let hits: Vec<&ActiveSession> = active.iter().filter(|s| norm(&s.cwd) == cwd && !taken.contains(&s.session_id)).collect();
    (hits.len() == 1).then(|| hits[0].session_id.clone())
}

/// 回音常被 TUI 截斷、尾巴帶 `…`：拿掉再比（§6.5d 的 `same_prompt`：忽略空白、短的一方是開頭）。
pub fn prompt_matches(seen: &str, prompt: &str) -> bool {
    let seen = seen.trim().trim_end_matches(['…', '.']).trim();
    !seen.is_empty() && crate::agent_relay::same_prompt(seen, prompt)
}

/// grok 啟動畫面的選單（`New worktree ctrl+w`／`Resume session ctrl+r`／`Quit ctrl+q`）不是回覆。
pub fn is_startup_screen(text: &str) -> bool {
    let rows: Vec<&str> = text.lines().map(|l| l.trim().trim_matches('│').trim()).collect();
    let has = |label: &str, key: &str| {
        rows.iter().any(|row| row.match_indices(label).any(|(i, _)| row[i + label.len()..].trim_start() == key))
    };
    has("New worktree", "ctrl+w") && has("Resume session", "ctrl+r") && has("Quit", "ctrl+q")
}

/// 單行的 `/指令 …`（`/effort high`、`/model …`）：送給 TUI 的設定，不是對話。`/home/x 看一下` 這種路徑開頭不算。
pub fn is_slash_command(text: &str) -> bool {
    let t = text.trim();
    !t.contains('\n')
        && t.strip_prefix('/').and_then(|r| r.split_whitespace().next()).is_some_and(|cmd| {
            !cmd.is_empty() && cmd.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
        })
}

async fn host_sh(app: &impl GrokTranscriptContext, host: &str, script: &str) -> anyhow::Result<String> {
    app.host_shell(host, script).await
}

async fn grok_home_for(app: &impl GrokTranscriptContext, bot: &db::Bot, host: &str) -> anyhow::Result<String> {
    app.grok_home_for(bot, host).await
}

async fn read_active(app: &impl GrokTranscriptContext, host: &str, grok_home: &str) -> anyhow::Result<Vec<ActiveSession>> {
    let out = host_sh(app, host, &format!("cat {}/active_sessions.json 2>/dev/null; :", sh_quote(grok_home))).await?;
    Ok(parse_active(&out))
}

/// `None`＝這個 session 沒有對話檔（還沒開始、被刪了、不是一般檔）。
async fn read_history(app: &impl GrokTranscriptContext, host: &str, grok_home: &str, session_id: &str) -> anyhow::Result<Option<String>> {
    if !valid_session_id(session_id) {
        return Ok(None);
    }
    let script = format!(
        "for d in {g}/sessions/*/{sid}; do f=\"$d/chat_history.jsonl\"; if [ -f \"$f\" ] && [ ! -L \"$f\" ]; then printf '%s\\n' '{mark}'; tail -c {max} \"$f\"; break; fi; done; :",
        g = sh_quote(grok_home),
        sid = session_id,
        mark = HISTORY_MARK,
        max = MAX_READ_BYTES,
    );
    let out = host_sh(app, host, &script).await?;
    Ok(out.split_once(&format!("{HISTORY_MARK}\n")).map(|(_, body)| body.to_string()))
}

/// 這個 run 的 grok session：記過的優先（還開著、或已經找不到更新的）；沒記過就從行程環境找 pane。
async fn session_for(app: &impl GrokTranscriptContext, bot: &db::Bot, run: &db::Run, host: &str, grok_home: &str) -> anyhow::Result<Option<String>> {
    let known = run.native_session_id.clone().filter(|s| valid_session_id(s));
    let active = read_active(app, host, grok_home).await?;
    if let Some(sid) = &known {
        if active.iter().any(|s| &s.session_id == sid) {
            return Ok(known);
        }
    }
    let Some(pane) = run.pane_id.as_deref() else { return Ok(known) };
    let pids = app.pids_in_pane(host, pane, run.herdr_session.as_deref()).await;
    let taken: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT native_session_id FROM runs WHERE state = 'running' AND id <> ? AND native_session_id IS NOT NULL",
    )
    .bind(&run.id)
    .fetch_all(app.db())
    .await?
    .into_iter()
    .collect();
    let cwd = match bot.cwd.clone().filter(|c| !c.trim().is_empty()) {
        Some(c) => Some(c),
        None => sqlx::query_scalar::<_, String>("SELECT path FROM projects WHERE id = ?").bind(&bot.project_id).fetch_optional(app.db()).await?,
    };
    let picked = pick_session(&active, &pids, cwd.as_deref(), &taken);
    if let Some(sid) = &picked {
        if known.as_deref() != Some(sid.as_str()) {
            sqlx::query("UPDATE runs SET native_session_id = ? WHERE id = ?").bind(sid).bind(&run.id).execute(app.db()).await?;
            tracing::info!(run = %run.id, session = %sid, "grok transcript: bound the pane to its grok session");
        }
    }
    Ok(picked.or(known))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Synced {
    /// 不是沒有 hook 的 grok run，或找不到 session／對話檔：照舊走畫面。
    Unavailable,
    Read {
        /// 這次新記下（或補上回覆）的回合數。
        imported: usize,
        /// 檔尾還沒結束的那一問。
        pending: Option<String>,
    },
}

fn is_transcript_run(bot: &db::Bot, run: &db::Run) -> bool {
    matches!(bot.kind.as_str(), "grok" | "agy" | "claude") && run.adopted != 0 && bot.inject_hooks == 0 && run.state == "running"
}

/// 讀這個 run 的 grok 對話檔，把結束了的每一問記成回合。呼叫端拿著 bot 鎖。
pub async fn sync_locked(app: &impl GrokTranscriptContext, run_id: &str) -> anyhow::Result<Synced> {
    let Some(run) = db::run(app.db(), run_id).await? else { return Ok(Synced::Unavailable) };
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else { return Ok(Synced::Unavailable) };
    if !is_transcript_run(&bot, &run) {
        return Ok(Synced::Unavailable);
    }
    let host = db::bot_host(app.db(), &bot.id).await?;
    let (sid, exchanges) = if bot.kind == "agy" {
        let Some((sid, text)) = app.agy_session_load(&run, &host).await? else { return Ok(Synced::Unavailable) };
        app.consume_resume_session(&bot, &run, Some(&sid)).await?;
        app.agy_session_record_status(&bot, &run, &text).await;
        let turns = crate::agy_support::parse_turns(&text)
            .into_iter()
            .map(|t| Exchange { prompt: t.prompt, prompt_index: Some(t.step_index), reply: t.reply, closed: t.closed, at: None })
            .collect();
        (sid, turns)
    } else if bot.kind == "claude" {
        let Some((sid, text)) = super::claude_child_log::load(app, &bot, &run, &host).await? else { return Ok(Synced::Unavailable) };
        super::claude_child_log::record_runtime(app, &bot, &run, &text).await;
        (sid, super::claude_child_log::parse_exchanges(&text))
    } else {
        let grok_home = grok_home_for(app, &bot, &host).await?;
        let Some(sid) = session_for(app, &bot, &run, &host, &grok_home).await? else { return Ok(Synced::Unavailable) };
        let Some(text) = read_history(app, &host, &grok_home, &sid).await? else { return Ok(Synced::Unavailable) };
        (sid, parse_chat_history(&text))
    };
    let pending = exchanges.last().filter(|e| !e.closed).map(|e| e.prompt.clone());
    let mut done: HashSet<String> =
        sqlx::query_scalar::<_, String>("SELECT native_turn_id FROM turns WHERE native_session_id = ? AND native_turn_id IS NOT NULL")
            .bind(&sid)
            .fetch_all(app.db())
            .await?
            .into_iter()
            .collect();
    let last_closed = exchanges.iter().rposition(|e| e.closed);
    let cutoff = if bot.kind == "claude" { baseline_at(app, &run.id).await? } else { None };
    let first = bot.kind == "claude" && cutoff.is_none();
    let mut imported = 0;
    for (i, ex) in exchanges.iter().enumerate() {
        if !ex.closed {
            continue;
        }
        if let (Some(c), Some(at)) = (cutoff.as_deref(), ex.at.as_deref()) {
            if at <= c {
                continue;
            }
        }
        let mut key = ex.key();
        if done.contains(&key) {
            if recorded_prompt_matches(app, &sid, &key, &ex.prompt).await? {
                continue;
            }
            key = format!("q{}", crate::supervisor_inbox::short_hash(ex.prompt.trim().as_bytes()));
            if done.contains(&key) {
                continue;
            }
        }
        let newest = Some(i) == last_closed && !(first && pending.is_some());
        if import(app, &bot, &run, &host, &sid, ex, &key, newest, first).await? {
            imported += 1;
            done.insert(key);
        }
    }
    if first {
        let at = exchanges.iter().filter(|e| e.closed).filter_map(|e| e.at.as_deref()).max().unwrap_or("");
        sqlx::query("UPDATE runs SET transcript_baseline_at = ? WHERE id = ? AND transcript_baseline_at IS NULL").bind(at).bind(&run.id).execute(app.db()).await?;
        tracing::info!(run = %run.id, session = %sid, baseline = %at, "claude transcript: baseline taken; existing exchanges are not turns");
    }
    Ok(Synced::Read { imported, pending })
}

/// `runs.transcript_baseline_at`：NULL＝這個 run 還沒讀過對話檔（下一次讀就是第一次）。
async fn baseline_at(app: &impl crate::capabilities::Db, run_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar::<_, Option<String>>("SELECT transcript_baseline_at FROM runs WHERE id = ?").bind(run_id).fetch_optional(app.db()).await?.flatten())
}

/// 這個 run 裡還沒綁 native id 的回合，依建立順序，帶著它的使用者訊息。
async fn unbound_turns(app: &impl crate::capabilities::Db, run_id: &str) -> anyhow::Result<Vec<(db::Turn, Vec<(String, String, String)>)>> {
    let turns: Vec<db::Turn> = sqlx::query_as(
        "SELECT * FROM turns WHERE run_id = ? AND native_turn_id IS NULL AND status IN ('in_flight','completed','completed_fallback')
          ORDER BY created_at, rowid",
    )
    .bind(run_id)
    .fetch_all(app.db())
    .await?;
    let mut out = Vec::new();
    for t in turns {
        let users: Vec<(String, String, String)> =
            sqlx::query_as("SELECT id, content, source FROM messages WHERE turn_id = ? AND role = 'user' ORDER BY created_at, rowid")
                .bind(&t.id)
                .fetch_all(app.db())
                .await?;
        out.push((t, users));
    }
    Ok(out)
}

/// 這把鑰匙底下已存的使用者原文。對不上代表 `prompt_index` 被重用。
async fn recorded_prompt_matches(app: &impl crate::capabilities::Db, sid: &str, key: &str, prompt: &str) -> anyhow::Result<bool> {
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT m.content FROM messages m JOIN turns t ON t.id = m.turn_id
          WHERE t.native_session_id = ? AND t.native_turn_id = ? AND m.role = 'user'
          ORDER BY m.created_at, m.rowid LIMIT 1",
    )
    .bind(sid)
    .bind(key)
    .fetch_optional(app.db())
    .await?;
    Ok(stored.as_deref().is_some_and(|s| s.trim() == prompt.trim()))
}

/// 這個 run 已記下的使用者訊息（回合 id、內容），依訊息時間。每次同步都跑：要先用 `turns_run_time` 找到這個 run 的
/// 幾個回合、再用 `messages_turn` 取訊息；沒有 run 的索引時 planner 反過來掃整張 messages（線上 3 萬則、1.4–2.7 秒，#1653）。
const RUN_USER_PROMPTS_SQL: &str = "SELECT t.id, m.content FROM turns t JOIN messages m ON m.turn_id = t.id
          WHERE t.run_id = ? AND m.role = 'user' ORDER BY m.created_at, m.rowid";

async fn import(app: &impl GrokTranscriptContext, bot: &db::Bot, run: &db::Run, host: &str, sid: &str, ex: &Exchange, key: &str, newest: bool, baseline: bool) -> anyhow::Result<bool> {
    let prior_users: Vec<(String, String)> = sqlx::query_as(RUN_USER_PROMPTS_SQL).bind(&run.id).fetch_all(app.db()).await?;
    if let Some((turn_id, _)) = prior_users.iter().find(|(_, c)| c.trim() == ex.prompt.trim()) {
        let bound: Option<String> = sqlx::query_scalar::<_, Option<String>>("SELECT native_turn_id FROM turns WHERE id = ?")
            .bind(turn_id)
            .fetch_optional(app.db())
            .await?
            .flatten()
            .filter(|s| !s.is_empty());
        let replies: Vec<String> =
            sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'assistant'").bind(turn_id).fetch_all(app.db()).await?;
        if bound.is_some() {
            if !replies.is_empty() || ex.reply.is_none() {
                return Ok(false);
            }
            let conv = db::conversation_id(app.db(), &bot.id).await?;
            let mut tx = db::begin_write(app.db()).await?;
            let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?").bind(turn_id).fetch_one(&mut *tx).await?;
            #[cfg(test)]
            super::race_point::hit("grok_transcript_after_replay_status_read", turn_id).await;
            if status == "in_flight" {
                turn_controller::set_status_on(&mut tx, turn_id, "in_flight", "completed", "grok chat_history.jsonl").await?;
            }
            let msg = insert_message_tx(&mut tx, &conv, Some(turn_id), "assistant", ex.reply.as_deref().unwrap(), "transcript", false, None).await?;
            tx.commit().await?;
            emit_message_added(app, &bot.id, msg).await;
            app.emit_turn(turn_id).await;
            return Ok(true);
        }
    }
    let candidates = unbound_turns(app, &run.id).await?;
    let matched = candidates
        .iter()
        .find(|(t, users)| {
            users.iter().any(|(_, c, _)| prompt_matches(c, &ex.prompt))
                || t.prompt_text.as_deref().is_some_and(|p| prompt_matches(p, &ex.prompt))
        })
        .or_else(|| {
            candidates.iter().find(|(t, users)| {
                newest && t.status == "in_flight" && t.origin == "external" && users.is_empty() && t.prompt_text.is_none()
            })
        });
    if baseline && matched.is_none() {
        return Ok(false);
    }
    let conv = db::conversation_id(app.db(), &bot.id).await?;
    let mut tx = db::begin_write(app.db()).await?;
    let mut added: Vec<db::Message> = Vec::new();
    let turn_id = match matched {
        Some((t, users)) => {
            if t.status == "in_flight" {
                let to = if ex.reply.is_some() { "completed" } else { "completed_fallback" };
                if turn_controller::set_status_on(&mut tx, &t.id, "in_flight", to, "grok chat_history.jsonl").await? != turn_controller::Outcome::Applied {
                    return Ok(false);
                }
            }
            for (id, content, source) in users {
                if source == "terminal_fallback" && content.trim() != ex.prompt.trim() {
                    sqlx::query("UPDATE messages SET content = ?, source = 'transcript', updated_at = ? WHERE id = ?")
                        .bind(&ex.prompt)
                        .bind(db::now())
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    added.push(sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE id = ?").bind(id).fetch_one(&mut *tx).await?);
                }
            }
            if users.is_empty() && t.prompt_text.is_none() {
                let from = relay_from(host, run, &ex.prompt);
                let msg = insert_message_relayed_tx(&mut tx, &conv, Some(&t.id), "user", &ex.prompt, "transcript", false, None, from.as_ref().map(|r| r.from_bot.as_str())).await?;
                super::messages::mark_relay_turn(&mut tx, &msg.id, from.as_ref().and_then(|r| r.from_turn.as_deref())).await?;
                added.push(msg);
            }
            if let Some(reply) = &ex.reply {
                let replies: Vec<(String, String)> =
                    sqlx::query_as("SELECT id, source FROM messages WHERE turn_id = ? AND role = 'assistant' ORDER BY created_at DESC, rowid DESC")
                        .bind(&t.id)
                        .fetch_all(&mut *tx)
                        .await?;
                #[cfg(test)]
                super::race_point::hit("grok_transcript_after_reply_read", &t.id).await;
                match replies.first() {
                    None => added.push(insert_message_tx(&mut tx, &conv, Some(&t.id), "assistant", reply, "transcript", false, None).await?),
                    Some((id, _)) if replies.iter().all(|(_, s)| s == "terminal_fallback") => {
                        if t.status == "completed_fallback" {
                            turn_controller::set_status_on(&mut tx, &t.id, "completed_fallback", "completed", "grok chat_history.jsonl 取代備援回覆").await?;
                        }
                        sqlx::query("UPDATE messages SET content = ?, source = 'transcript', incomplete = 0, terminal_snapshot = NULL, updated_at = ? WHERE id = ?")
                            .bind(reply)
                            .bind(db::now())
                            .bind(id)
                            .execute(&mut *tx)
                            .await?;
                        added.push(sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE id = ?").bind(id).fetch_one(&mut *tx).await?);
                    }
                    Some(_) => {}
                }
            }
            t.id.clone()
        }
        None => {
            let tid = db::ulid();
            let now = db::now();
            sqlx::query(
                "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at, completed_at)
                 VALUES (?,?,?,'external',?,'ok',?,?)",
            )
            .bind(&tid)
            .bind(&conv)
            .bind(&run.id)
            .bind(if ex.reply.is_some() { "completed" } else { "completed_fallback" })
            .bind(&now)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            let orphans: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, content FROM messages WHERE conversation_id = ? AND turn_id IS NULL AND role = 'user' AND created_at >= ?
                  ORDER BY created_at, rowid",
            )
            .bind(&conv)
            .bind(&run.started_at)
            .fetch_all(&mut *tx)
            .await?;
            match orphans.iter().find(|(_, c)| prompt_matches(c, &ex.prompt)) {
                Some((id, _)) => {
                    sqlx::query("UPDATE messages SET turn_id = ?, updated_at = ? WHERE id = ?").bind(&tid).bind(db::now()).bind(id).execute(&mut *tx).await?;
                    added.push(sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE id = ?").bind(id).fetch_one(&mut *tx).await?);
                }
                None => {
                    let from = relay_from(host, run, &ex.prompt);
                    let msg = insert_message_relayed_tx(&mut tx, &conv, Some(&tid), "user", &ex.prompt, "transcript", false, None, from.as_ref().map(|r| r.from_bot.as_str())).await?;
                    super::messages::mark_relay_turn(&mut tx, &msg.id, from.as_ref().and_then(|r| r.from_turn.as_deref())).await?;
                    added.push(msg);
                }
            }
            if let Some(reply) = &ex.reply {
                added.push(insert_message_tx(&mut tx, &conv, Some(&tid), "assistant", reply, "transcript", false, None).await?);
            }
            tid
        }
    };
    sqlx::query("UPDATE turns SET native_session_id = ?, native_turn_id = ? WHERE id = ?")
        .bind(sid)
        .bind(&key)
        .bind(&turn_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    for m in added {
        emit_message_added(app, &bot.id, m).await;
    }
    app.emit_turn(&turn_id).await;
    tracing::info!(run = %run.id, turn = %turn_id, key, "grok transcript: recorded an exchange from chat_history.jsonl");
    Ok(true)
}

/// §6.5d：別的 agent 用 `herdr agent prompt` 交辦的這句。
fn relay_from(host: &str, run: &db::Run, prompt: &str) -> Option<crate::agent_relay::Relayed> {
    crate::agent_relay::claim_relayed(host, &run.id, prompt)
}

/// `try_fallback` 先問這裡。`Some(true)`：回合已經由對話檔收掉；`Some(false)`：對話檔說這一問還沒結束，留在飛；
/// `None`：對話檔幫不上（讀不到、或這一問不在檔裡），照舊看畫面。
pub async fn settle_in_flight(app: &impl GrokTranscriptContext, bot: &db::Bot, run: &db::Run, turn: &db::Turn) -> Option<bool> {
    if !is_transcript_run(bot, run) {
        return None;
    }
    let pending = match sync_locked(app, &run.id).await {
        Ok(Synced::Read { pending, .. }) => pending,
        Ok(Synced::Unavailable) => return None,
        Err(e) => {
            tracing::warn!(run = %run.id, error = ?e, "grok transcript unreadable; falling back to the pane");
            return None;
        }
    };
    match db::in_flight_turn(app.db(), &run.id).await {
        Ok(Some(t)) if t.id == turn.id => {}
        Ok(_) => return Some(true),
        Err(_) => return None,
    }
    let sent = turn_echo_texts(app, &turn.id).await.ok()?;
    if sent.is_empty() {
        if pending.is_some() {
            return Some(false);
        }
        let mut tx = app.db().begin().await.ok()?;
        let out = turn_controller::set_status_on(&mut tx, &turn.id, "in_flight", "completed_fallback", "grok chat_history.jsonl 沒有新的一問").await.ok()?;
        tx.commit().await.ok()?;
        if out == turn_controller::Outcome::Applied {
            app.emit_turn(&turn.id).await;
        }
        return Some(true);
    }
    match pending {
        Some(p) if sent.iter().any(|s| prompt_matches(s, &p)) => Some(false),
        _ => None,
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "grok_transcript_tests.rs"]
mod tests;
