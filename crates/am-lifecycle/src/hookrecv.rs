//! Hook receiver: `POST /hook/{provider}` plus Turn matching (SPEC §6.7) and spool replay (§4.4.6).

use crate::events::ports::{ApiPort, HandoffRepo, MessageTxOps, ProviderPort, QuotaCommands, TurnCommands, TurnConnOps, TurnFenceOps};
use crate::ask_answers;
use crate::db;
use crate::config::{valid_id, ID_RE};
use crate::lifecycle;
use crate::hosts::sh_quote;
// 測試模組（`use super::*`）還在用整個 daemon 的型別、以及搬到 `runners/hookrecv.rs` 的組裝函式；測試檔之後再改成自己 import。
#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::state::App;
#[cfg(all(test, feature = "daemon-test-harness"))]
use crate::runners::hookrecv::{receive, replay_host, spawn_spool_scanner, statusline_tasks_spawned};
#[cfg(all(test, feature = "daemon-test-harness"))]
use axum::extract::{Path, State};
#[cfg(all(test, feature = "daemon-test-harness"))]
use axum::http::{HeaderMap, StatusCode};
#[cfg(all(test, feature = "daemon-test-harness"))]
use axum::Json;
#[cfg(all(test, feature = "daemon-test-harness"))]
use std::sync::Arc;
use anyhow::Result;
use serde_json::{json, Value};

pub use crate::hook_body::HookBody;

/// 驗 per-bot token → **寫進耐久收件匣並 commit** → 才回 200（SPEC §3.1、issue #70）。
///
/// `200` 的意思是「這則事件已經寫進 `hook_events`」，不是「已經處理完」。寫不進去就回 503：
/// 送端（`hook_cmd::inner`）看到非 2xx 會把同一份 body 追加到 `hook-spool.jsonl`，replay 會補回來。
/// 回 200 再掉事件是無聲的資料遺失，回 503 只是讓那則事件多繞一趟 spool。
///
/// StatusLine 例外，照舊 fire-and-forget（理由見 [`crate::hook_inbox`]）。
/// hook 的 provider（URL 那一段或 body 的 `provider`）必須就是這顆 bot 的 kind。空的 kind（舊資料）不擋。
pub fn provider_matches_kind(provider: &str, kind: &str) -> bool {
    kind.is_empty() || provider.eq_ignore_ascii_case(kind)
}

/// 這句 prompt 回音是別的 agent 打進來的嗎？（見 SPEC §6.5d）
/// 認不出來就當使用者自己打的——寧可少標一次，也不要冤枉一句話。
/// 遠端 shim 寫進 spool 的報備事件名（`herdr_shim.rs` 的 `am_spool_relay`）。
pub const RELAY_ANNOUNCE_EVENT: &str = "AmRelayAnnounce";

/// 報備比收件方的回音晚到時補標：寄件者那台主機上、`agent_name` 是 `to_agent` 的在跑 run，它的對話裡
/// 五分鐘內、還沒標來源、內容對得上的最新一則使用者訊息。補上就用掉那筆報備（同一句不標兩次）。
async fn relay_backfill(app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands), from_bot: &str, from_turn: Option<&str>, to_agent: &str, text: &str) -> Result<()> {
    let host = db::bot_host(app.db(), from_bot).await?;
    let since = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT m.id, m.content, b.id FROM messages m
           JOIN conversations c ON c.id = m.conversation_id
           JOIN bots b ON b.id = c.bot_id
           JOIN projects p ON p.id = b.project_id
           JOIN runs r ON r.bot_id = b.id AND r.state = 'running' AND r.agent_name = ?
          WHERE p.host = ? AND b.id <> ? AND m.role = 'user' AND m.source IN ('hook','terminal_fallback')
            AND m.relay_from IS NULL AND m.created_at >= ?
          ORDER BY m.created_at DESC, m.rowid DESC LIMIT 20",
    )
    .bind(to_agent)
    .bind(&host)
    .bind(from_bot)
    .bind(&since)
    .fetch_all(app.db())
    .await?;
    let Some((msg_id, content, to_bot)) = rows.into_iter().find(|(_, c, _)| crate::agent_relay::same_prompt(text, c)) else {
        return Ok(());
    };
    sqlx::query("UPDATE messages SET relay_from = ?, relay_unverified = 0, relay_turn_id = ?, updated_at = ? WHERE id = ? AND relay_from IS NULL")
        .bind(from_bot)
        .bind(from_turn)
        .bind(db::now())
        .bind(&msg_id)
        .execute(app.db())
        .await?;
    let _ = crate::agent_relay::claim(&host, to_agent, &content);
    let message: db::Message = sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE id = ?").bind(&msg_id).fetch_one(app.db()).await?;
    tracing::info!(from = %from_bot, to = %to_agent, msg = %msg_id, "relay announce arrived after the echo; attributed it");
    app.emit_message_added(&to_bot, message).await;
    Ok(())
}

async fn relay_source(app: &impl crate::capabilities::Db, run: Option<&db::Run>, echo: &str) -> Option<crate::agent_relay::Relayed> {
    let run = run?;
    let agent = run.agent_name.as_deref()?;
    // 讀不到主機＝認不出來就不標（寧可少標，不要錯標）。
    let host = db::bot_host(app.db(), &run.bot_id).await.ok()?;
    crate::agent_relay::claim_relayed(&host, agent, echo)
}

pub enum HookKind {
    /// Never creates a Turn.
    Identity { session_id: Option<String>, transcript_path: Option<String> },
    TurnComplete {
        session_id: Option<String>,
        turn_id: Option<String>,
        transcript_path: Option<String>,
        assistant: Option<String>,
        user: Option<String>,
    },
    /// claude 的 `StopFailure`：這一回合是**失敗**收尾的（API／auth／額度…），不是答完（issue #79）。
    TurnFailed {
        session_id: Option<String>,
        turn_id: Option<String>,
        transcript_path: Option<String>,
        reason: FailureReason,
        /// 原文，寫進系統訊息讓人看得出來為什麼失敗。
        detail: Option<String>,
    },
    /// Claude Code statusLine input — never a Turn.
    StatusLine,
    /// claude 原生 `SubagentStart`／`SubagentStop`（issue #82）：純可見性快照，從不建立或動 Turn，
    /// 也從不影響 §6.5a 的血緣認領——只覆蓋這顆 run 的 `subagent_json`。`event` 是 `"start"` 或
    /// `"stop"`；`SubagentStart` 沒有 transcript path，`SubagentStop` 才有。
    SubagentEvent {
        event: &'static str,
        agent_id: Option<String>,
        agent_type: Option<String>,
        transcript_path: Option<String>,
    },
    /// issue #94: `PostToolUse` on the Bash tool whose stdout contained herdr's own `pane:split` /
    /// `agent:start` responses — this bot's own tool call just created these pane IDs. Recorded as
    /// spawn hints for `reconcile::adopt_child`; never touches a Turn.
    SpawnHint { pane_ids: Vec<String> },
    /// `PostToolUse` on `AskUserQuestion` with a recognisable answer: record the Q&A in the conversation
    /// (`ask_answers`). Never touches a Turn.
    AskAnswered(crate::ask_answers::AskRecord),
    /// 遠端 bot 的 herdr shim 在 `agent prompt` 之前寫進自己 spool 的報備（SPEC §6.5d）：遠端沒有 `AM_PORT`，
    /// 打不到 `/relay/announce`，只能跟 hook 走同一條 spool。寄件者就是這則 body 的 bot（spool 在它自己的目錄）。
    RelayAnnounce { to_agent: String, text: String },
    Ignore(String),
}

impl std::fmt::Debug for HookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            HookKind::Identity { .. } => "Identity",
            HookKind::TurnComplete { .. } => "TurnComplete",
            HookKind::TurnFailed { .. } => "TurnFailed",
            HookKind::StatusLine => "StatusLine",
            HookKind::SubagentEvent { .. } => "SubagentEvent",
            HookKind::SpawnHint { .. } => "SpawnHint",
            HookKind::AskAnswered(_) => "AskAnswered",
            HookKind::RelayAnnounce { .. } => "RelayAnnounce",
            HookKind::Ignore(reason) => {
                let _ = reason;
                "Ignore"
            }
        };
        f.write_str(name)
    }
}

/// `StopFailure` 說的是哪一種失敗。分得出來就留著（`rate limit` 跟其他錯誤要分得開），
/// 分不出來是 `Unknown`——「回合失敗了」本身就是一級訊號，不必等分類到位才收回合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    RateLimit,
    Auth,
    Api,
    /// 使用者自己中斷。**不是** provider 失敗，一個字都不動：claude 把 Esc 也走 `StopFailure` 時，
    /// 記成失敗會讓「我自己按停的」看起來像系統壞了，也會蓋掉 `interrupted by user` 那條既有路徑。
    Interrupted,
    Unknown,
}

impl FailureReason {
    fn label(self) -> &'static str {
        match self {
            FailureReason::RateLimit => "額度或速率限制",
            FailureReason::Auth => "帳號或授權",
            FailureReason::Api => "API 錯誤",
            FailureReason::Interrupted => "使用者中斷",
            FailureReason::Unknown => "未分類",
        }
    }
}

/// 從 `StopFailure` 的 payload 裡撈出講得出口的原因。鍵名故意給一整排：這個 hook 的欄位名還在動，
/// 撈不到就退回 `None`（照樣收回合，只是說不出原因），絕不因為欄位對不上就當作沒發生。
fn failure_detail(p: &Value) -> Option<String> {
    const KEYS: [&str; 9] =
        ["reason", "failure_reason", "stop_reason", "error_type", "errorType", "subtype", "error", "message", "detail"];
    for k in KEYS {
        match p.get(k) {
            Some(Value::String(s)) if !s.trim().is_empty() => return Some(s.trim().to_string()),
            // `error: {type, message}` 之類的巢狀形狀。
            Some(Value::Object(o)) => {
                for inner in ["message", "type", "code"] {
                    if let Some(s) = o.get(inner).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                        return Some(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// 原因文字歸到哪一類。中斷排在最前面：寧可把一次真的失敗說成「使用者中斷」而少收一次，
/// 也不要把使用者自己按的停說成系統失敗（AGM 交辦 2026-09-18）。
pub fn classify_failure(detail: Option<&str>) -> FailureReason {
    let t = detail.unwrap_or("").to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    // `esc` 太短，直接 `contains` 會連「unescaped」「description」這種普通英文字都算中——那樣一來
    // 真正的失敗（API／額度）會被這條擋在最前面的分支攔走，回合被當成使用者中斷、一個字都不動，
    // #79／#108 想防的「卡在 in_flight、額度沒記下來」又繞了回來。只認被非英數字元包住的獨立字。
    let esc_word = t.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|w| w == "esc");
    if esc_word || has(&["interrupt", "cancel", "abort", "user_stop", "user stop"]) {
        FailureReason::Interrupted
    } else if crate::turn_error::is_quota_exhaustion(&t)
        || has(&["rate limit", "rate_limit", "ratelimit", "usage limit", "usage_limit", "quota", "429", "overloaded"])
    {
        // 第一支是 claude 撞額度的橫幅（`You've hit your session limit` 之類，沒有 rate/usage 字樣，issue #108）。
        FailureReason::RateLimit
    } else if has(&["auth", "401", "403", "credential", "unauthorized", "forbidden", "login", "api key", "api_key"]) {
        FailureReason::Auth
    } else if t.trim().is_empty() {
        FailureReason::Unknown
    } else {
        FailureReason::Api
    }
}

const DEFAULT_CLAUDE_IDENTITY: &str = "cc0";

/// `(email, warning)` from the identity's login state **on the bot's host**, not the daemon's
/// `.claude.json` (issue #4: wrong machine, and a logged-out cc1 silently ran as cc0).
fn claude_account_from_tools(
    tools: &std::collections::HashMap<String, crate::tools::HostTools>,
    host: &str,
    identity: Option<&str>,
) -> (Option<String>, Option<String>) {
    let info = tools.get(host).and_then(|t| t.identities.get(identity.unwrap_or(DEFAULT_CLAUDE_IDENTITY)));
    match (identity, info) {
        (Some(name), Some(i)) if i.logged_in == Some(false) => (
            None,
            Some(format!("身份 {name} 在 {host} 沒有登入：claude 會退回這台機器預設（cc0）的帳號執行（macOS 存在 Keychain、Linux 存在 ~/.claude/.credentials.json）。請在這個 Bot 按「登入 / 切換帳號」。")),
        ),
        (_, Some(i)) if i.logged_in != Some(false) && i.account.is_some() => (i.account.clone(), None),
        _ => (None, None),
    }
}

async fn claude_account(app: &impl crate::tools::ToolsTable, host: &str, identity: Option<&str>) -> (Option<String>, Option<String>) {
    let tools = app.tools().lock().await;
    claude_account_from_tools(&tools, host, identity)
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod account_tests {
    use super::*;

    fn identity(name: &str, account: &str) -> crate::tools::IdentityInfo {
        crate::tools::IdentityInfo {
            name: name.to_string(),
            kind: "claude".to_string(),
            logged_in: Some(true),
            reason: None,
            account: Some(account.to_string()),
            plan: None,
            source: crate::tools::SOURCE_CONFIG,
            config_dir: None,
        }
    }

    fn host_tools(identity: crate::tools::IdentityInfo) -> crate::tools::HostTools {
        crate::tools::HostTools {
            tools: std::collections::BTreeMap::new(),
            identities: [(identity.name.clone(), identity)].into_iter().collect(),
            shell_identities: Vec::new(),
            utc_offset_secs: None, herdr_cli: None, checked_at: String::new(),
        }
    }

    #[test]
    fn remote_bot_uses_remote_identity_account_not_local_account() {
        let mut tools = std::collections::HashMap::new();
        tools.insert("local".to_string(), host_tools(identity("cc1", "local@example.com")));
        tools.insert("remote".to_string(), host_tools(identity("cc1", "remote@example.com")));

        let (account, warning) = claude_account_from_tools(&tools, "remote", Some("cc1"));

        assert_eq!(account.as_deref(), Some("remote@example.com"));
        assert_eq!(warning, None);
    }

    #[test]
    fn local_identity_probe_miss_does_not_fall_back_to_default_account() {
        let mut tools = std::collections::HashMap::new();
        tools.insert("local".to_string(), host_tools(identity("cc0", "stale@example.com")));

        let (account, warning) = claude_account_from_tools(&tools, "local", Some("cc1"));

        assert_eq!(account, None);
        assert_eq!(warning, None);
    }

    #[test]
    fn default_bot_does_not_show_stale_account_when_host_says_logged_out() {
        let mut default_identity = identity("cc0", "stale@example.com");
        default_identity.logged_in = Some(false);
        let mut tools = std::collections::HashMap::new();
        tools.insert("local".to_string(), host_tools(default_identity));

        let (account, warning) = claude_account_from_tools(&tools, "local", None);

        assert_eq!(account, None);
        assert_eq!(warning, None);
    }
}

/// 這一則事件的 native session id，三家 provider 的鍵名都認（世代圍籬用它證明歸屬）。
fn hook_session_id(p: &Value) -> Option<&str> {
    ["session_id", "sessionId", "thread-id", "conversationId", "conversation_id"]
        .iter()
        .find_map(|k| p.get(*k).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub fn classify(provider: &str, p: &Value) -> HookKind {
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).map(String::from);
    // shim 自己的事件，不分 claude／codex（寄件的 bot 可以是任何一種）。
    if p.get("hook_event_name").and_then(Value::as_str) == Some(RELAY_ANNOUNCE_EVENT) {
        return match (s("to_agent"), s("text")) {
            (Some(to_agent), Some(text)) if !to_agent.trim().is_empty() && !text.trim().is_empty() => HookKind::RelayAnnounce { to_agent, text },
            _ => HookKind::Ignore("relay announce without to_agent/text".into()),
        };
    }
    match provider {
        "claude" => {
            // 解不開的 payload（hook 子行程包成 `{"raw": …}`：超過 1 MiB 被截斷、stdin 逾時）沒有事件名：不猜成 SessionStart（#1007）。
            if p.get("hook_event_name").is_none() && p.get("prompt_id").is_none() && p.get("raw").is_some() {
                return HookKind::Ignore("unparseable payload (raw)".into());
            }
            let ev = p
                .get("hook_event_name")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| if p.get("prompt_id").is_some() { "Stop".into() } else { "SessionStart".into() });
            match ev.as_str() {
                "StatusLine" => HookKind::StatusLine,
                "SessionStart" => HookKind::Identity { session_id: s("session_id"), transcript_path: s("transcript_path") },
                // 回合失敗收尾的原生訊號（issue #79）：以前只有 `Stop`，失敗的回合要等 §4.3 備援或
                // stuck watchdog 才被發現，中間一直掛在 in_flight。
                "StopFailure" => {
                    let detail = failure_detail(p);
                    HookKind::TurnFailed {
                        session_id: s("session_id"),
                        turn_id: s("prompt_id"),
                        transcript_path: s("transcript_path"),
                        reason: classify_failure(detail.as_deref()),
                        detail,
                    }
                }
                "Stop" => {
                    if p.get("stop_hook_active").and_then(|v| v.as_bool()).unwrap_or(false) {
                        return HookKind::Ignore("stop_hook_active".into());
                    }
                    HookKind::TurnComplete {
                        session_id: s("session_id"),
                        turn_id: s("prompt_id"),
                        transcript_path: s("transcript_path"),
                        assistant: s("last_assistant_message"),
                        user: None,
                    }
                }
                // issue #82：純可見性，見 `HookKind::SubagentEvent` 的說明——這裡不建立、不動任何
                // Turn，`agent_id`／`agent_type` 兩個事件都有，`agent_transcript_path` 只有 Stop 帶。
                "SubagentStart" => HookKind::SubagentEvent {
                    event: "start",
                    agent_id: s("agent_id"),
                    agent_type: s("agent_type"),
                    transcript_path: None,
                },
                "SubagentStop" => HookKind::SubagentEvent {
                    event: "stop",
                    agent_id: s("agent_id"),
                    agent_type: s("agent_type"),
                    transcript_path: s("agent_transcript_path"),
                },
                // issue #94：`matcher: "Bash"` already scopes this to shell commands; the actual
                // "was this herdr creating a pane" decision is `crate::spawn_hints::extract_pane_ids`.
                "PostToolUse" if p.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") => {
                    match crate::ask_answers::from_post_tool_use(p) {
                        Some(rec) => HookKind::AskAnswered(rec),
                        None => HookKind::Ignore("PostToolUse (AskUserQuestion without a recognisable answer)".into()),
                    }
                }
                "PostToolUse" => {
                    let pane_ids = crate::spawn_hints::extract_pane_ids(p);
                    if pane_ids.is_empty() {
                        HookKind::Ignore("PostToolUse (not a herdr spawn)".into())
                    } else {
                        HookKind::SpawnHint { pane_ids }
                    }
                }
                other => HookKind::Ignore(other.to_string()),
            }
        }
        "codex" => {
            let ty = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ty != "agent-turn-complete" {
                return HookKind::Ignore(ty.to_string());
            }
            let user = p
                .get("input-messages")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("\n"))
                .filter(|s| !s.is_empty());
            // Codex runs a hidden title-generation turn after each reply; not user-visible.
            let assistant = s("last-assistant-message");
            // 有 input 就以它為準（標題回合的 input 是標題指令）；真使用者回合的回覆剛好是 `{"title":…}` 不能被吞。
            // 取不到 input 時，回覆長相才是唯一線索。
            let is_title_turn = match user.as_deref() {
                Some(u) => u.contains("single-line task title"),
                None => assistant
                    .as_deref()
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .map(|v| v.as_object().map(|o| o.len() == 1 && o.contains_key("title")).unwrap_or(false))
                    .unwrap_or(false),
            };
            if is_title_turn {
                return HookKind::Ignore("codex title-generation turn".into());
            }
            HookKind::TurnComplete {
                session_id: s("thread-id"),
                turn_id: s("turn-id"),
                transcript_path: None,
                assistant,
                user,
            }
        }
        // SPEC §12 / appendix F: camelCase keys, grok 1.0.13 also sends some snake_case copies.
        "grok" => {
            let either = |camel: &str, snake: &str| s(camel).or_else(|| s(snake));
            let ev = either("hookEventName", "hook_event_name").unwrap_or_default().to_lowercase();
            match ev.as_str() {
                "session_start" | "sessionstart" => HookKind::Identity {
                    session_id: either("sessionId", "session_id"),
                    transcript_path: either("transcriptPath", "transcript_path"),
                },
                "stop" => {
                    // A second, observe-only Stop fires at session end (`reason: shutdown`).
                    let reason = s("reason").unwrap_or_else(|| "end_turn".into());
                    if reason != "end_turn" {
                        return HookKind::Ignore(format!("stop reason {reason}"));
                    }
                    let active = p
                        .get("stopHookActive")
                        .or_else(|| p.get("stop_hook_active"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if active {
                        return HookKind::Ignore("stop_hook_active".into());
                    }
                    HookKind::TurnComplete {
                        session_id: either("sessionId", "session_id"),
                        turn_id: either("promptId", "prompt_id"),
                        transcript_path: either("transcriptPath", "transcript_path"),
                        assistant: either("lastAssistantMessage", "last_assistant_message"),
                        user: None,
                    }
                }
                other => HookKind::Ignore(other.to_string()),
            }
        }
        // agy（設計 A.6／§2 #6）：payload 沒有事件名，hook 子行程把 dispatcher 的參數放在 `hookEventName`（`hook_cmd::enrich_agy_payload`）；
        // `Stop` 另帶 `lastAssistantMessage`／`lastUserMessage`（從 transcript 讀的）。camelCase 是 hook、snake_case 是 statusLine。
        "agy" => {
            let either = |camel: &str, snake: &str| s(camel).or_else(|| s(snake)).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let conv = either("conversationId", "conversation_id");
            let path = either("transcriptPath", "transcript_path");
            match s("hookEventName").unwrap_or_default().to_ascii_lowercase().as_str() {
                // 對話是第一則 prompt 才建立：`SessionStart` 沒文件、不一定來，第一個 `PreInvocation` 補身分。
                "sessionstart" | "preinvocation" => HookKind::Identity { session_id: conv, transcript_path: path },
                // statusLine：每次 agent 狀態改變一則，單槽、最新的贏（不進收件匣）。
                "state" => HookKind::StatusLine,
                "stop" => {
                    let reason = s("terminationReason").unwrap_or_default();
                    let error = s("error").map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
                    // 值域不可假設（官方 `model_stop`／`max_steps_exceeded`／`error`，實測 `ERROR`，第三方 `NO_TOOL_CALL`）：
                    // 有錯誤文字、或原因是 error 才算失敗；其他一律當正常結束。
                    if error.is_some() || reason.eq_ignore_ascii_case("error") {
                        let detail = error.or_else(|| Some(reason.clone()).filter(|r| !r.is_empty()));
                        return HookKind::TurnFailed {
                            session_id: conv,
                            // `executionNum` 是不是跨回合唯一沒有驗過：當去重鑰匙會把之後的回合吃掉，所以不給。
                            turn_id: None,
                            transcript_path: path,
                            reason: classify_failure(detail.as_deref()),
                            detail,
                        };
                    }
                    // 背景工作還在跑（`fullyIdle:false`）＝回合還沒真的結束，等最後那一個 Stop。
                    if p.get("fullyIdle").and_then(Value::as_bool) == Some(false) {
                        return HookKind::Ignore("agy stop with background work still running".into());
                    }
                    HookKind::TurnComplete { session_id: conv, turn_id: None, transcript_path: path, assistant: s("lastAssistantMessage"), user: s("lastUserMessage") }
                }
                other => HookKind::Ignore(format!("agy event `{other}`")),
            }
        }
        other => HookKind::Ignore(format!("unknown provider {other}")),
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod classify_tests {
    use super::*;

    /// Captured from grok 1.0.13 (appendix F), trimmed.
    const GROK_STOP: &str = r#"{"hookEventName":"stop","sessionId":"01a072c2-9098-7d50-b3d1-f1750320ae28",
      "cwd":"/x","workspaceRoot":"/x","timestamp":"2026-09-05T18:09:13.833011+00:00",
      "transcriptPath":"/Users/me/.grok/sessions/%2Fx/01a072c2/updates.jsonl",
      "promptId":"089f03f9-594f-4e92-bfb0-5180eefaa250","permissionMode":"bypassPermissions",
      "reason":"end_turn","stopHookActive":false,"lastAssistantMessage":"GROK-OK",
      "backgroundTasks":[],"sessionCrons":[],"hook_event_name":"stop",
      "session_id":"01a072c2-9098-7d50-b3d1-f1750320ae28"}"#;

    /// #1007：解不開的 claude payload（`{"raw": …}`）沒有事件名，不能猜成 SessionStart；舊形狀（只有 session_id、prompt_id）照舊猜。
    #[test]
    fn an_unparseable_claude_payload_is_ignored_not_guessed_as_session_start() {
        assert!(matches!(classify("claude", &json!({"raw": "{\"hook_event_name\":\"Stop\",\"last_assistant_mess"})), HookKind::Ignore(_)));
        assert!(matches!(classify("claude", &json!({"raw": ""})), HookKind::Ignore(_)));
        assert!(matches!(classify("claude", &json!({"session_id": "s1"})), HookKind::Identity { .. }), "沒有事件名、沒有 raw 的舊形狀照舊猜");
        assert!(matches!(classify("claude", &json!({"prompt_id": "p1"})), HookKind::TurnComplete { .. }));
    }

    #[test]
    fn grok_stop_is_a_turn() {
        let v: Value = serde_json::from_str(GROK_STOP).unwrap();
        match classify("grok", &v) {
            HookKind::TurnComplete { session_id, turn_id, transcript_path, assistant, user } => {
                assert_eq!(session_id.as_deref(), Some("01a072c2-9098-7d50-b3d1-f1750320ae28"));
                assert_eq!(turn_id.as_deref(), Some("089f03f9-594f-4e92-bfb0-5180eefaa250"));
                assert!(transcript_path.unwrap().ends_with("updates.jsonl"));
                assert_eq!(assistant.as_deref(), Some("GROK-OK"));
                assert!(user.is_none());
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
    }

    #[test]
    fn grok_session_end_stop_is_ignored() {
        let v = json!({"hookEventName":"stop","sessionId":"s","reason":"shutdown","stopHookActive":false});
        assert!(matches!(classify("grok", &v), HookKind::Ignore(_)));
        let v = json!({"hookEventName":"stop","sessionId":"s","reason":"end_turn","stopHookActive":true});
        assert!(matches!(classify("grok", &v), HookKind::Ignore(_)));
        let v = json!({"hookEventName":"session_end","sessionId":"s"});
        assert!(matches!(classify("grok", &v), HookKind::Ignore(_)));
    }

    #[test]
    fn grok_session_start_is_identity() {
        let v = json!({"hookEventName":"session_start","sessionId":"s1","source":"new"});
        match classify("grok", &v) {
            HookKind::Identity { session_id, transcript_path } => {
                assert_eq!(session_id.as_deref(), Some("s1"));
                assert!(transcript_path.is_none());
            }
            other => panic!("expected Identity, got {other:?}"),
        }
    }
}

/// SPEC §6.7, executed under the per-bot lock.
#[allow(dead_code)]
pub async fn process<H: HookHost>(app: &H, body: &HookBody) -> Result<()> {
    process_for(app, body, None).await
}

/// 同 [`process`]，多帶這則 hook 在收件匣的 `hook_events.id`（只有 `hook_inbox` 的 worker 有）。
///
/// 收件匣是 at-least-once：回合與訊息 commit 之後、worker 標 `processed_at` 之前 daemon 重啟，同一列會再處理一次。
/// 帶得到 native turn id 的 hook 靠 `(native_session_id, native_turn_id)` 去重；沒有的（遠端、舊版、手寫 body）以前沒有任何
/// 穩定的鑰匙，重播就再開一個外部回合與訊息。這把 id 跟回合同一個交易寫進 `turns.source_event_id`（唯一索引），
/// 重播先查它：命中就只把 commit 後才發的通知補發一次（`message_added` 靠 id 去重、回合事件本來就是 at-least-once）。
pub async fn process_for<H: HookHost>(app: &H, body: &HookBody, event_id: Option<&str>) -> Result<()> {
    let lock = app.bot_lock(&body.bot_id).await;
    let _g = lock.lock().await;
    process_locked_for(app, body, event_id).await?;
    // 回合收完（含終端打字開的外部回合）之後才補：這時候「這一回合」才一定存在。
    app.after_turn_end(body).await;
    Ok(())
}

/// A pane-wrapped echo must still compare equal to the hook's single-line copy.
fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalize layout whitespace without erasing word boundaries. A literal Tab may also be lost
/// by the Claude TUI, so only text that originally contained one gets a tab-removed candidate.
/// Containment handles clipped echoes, but short overlaps are too ambiguous to establish identity.
const MIN_CLIPPED_PROMPT_CHARS: usize = 8;

fn prompt_identity_candidates(s: &str) -> Vec<String> {
    let mut candidates = vec![squash_ws(s)];
    if s.contains('\t') {
        candidates.push(squash_ws(&s.replace('\t', "")));
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

/// Equality after layout normalization is strong evidence; clipped containment needs a useful
/// amount of text so a shared short prefix cannot assign a reply to the wrong turn.
fn prompt_texts_match(left: &str, right: &str) -> bool {
    if left.trim().is_empty() || right.trim().is_empty() {
        return false;
    }
    let left_candidates = prompt_identity_candidates(left);
    let right_candidates = prompt_identity_candidates(right);
    left_candidates.iter().any(|l| {
        right_candidates.iter().any(|r| {
            if l.is_empty() || r.is_empty() {
                return false;
            }
            if l == r {
                return true;
            }
            let (short, long) = if l.chars().count() <= r.chars().count() {
                (l, r)
            } else {
                (r, l)
            };
            short.chars().count() >= MIN_CLIPPED_PROMPT_CHARS && long.contains(short)
        })
    })
}

/// 遠端 `hook.sh` 塞進 Stop payload 的鍵：本機 transcript 最後一則使用者訊息（`setup.rs` 的 `REMOTE_HOOK_SH_TEMPLATE`）。
const CARRIED_USER_TEXT: &str = "agm_user_text";

/// 這一回合的使用者訊息原文：codex 的 hook 直接帶；claude 的 Stop 沒帶，從 transcript 尾巴找最後一則。
/// 讀不到就是 `None`（沒有證據，呼叫端照舊認領）。
///
/// 遠端 bot 的 transcript 在那台機器上、這裡讀不到（issue #753）：遠端 `hook.sh` 在 spool 前從**本機** transcript 讀出同一則，
/// 放在 payload 的 [`CARRIED_USER_TEXT`]。優先於讀檔：帶著的是 hook 當下讀的，檔案路徑在這台機器上可能指到別的東西。
async fn hook_user_text(from_hook: Option<&str>, payload: &Value, transcript_path: Option<&str>) -> Option<String> {
    if let Some(u) = from_hook.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(u.to_string());
    }
    if let Some(u) = payload.get(CARRIED_USER_TEXT).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        return Some(crate::lifecycle::pasted_content::original(u).into_owned());
    }
    let path = std::path::PathBuf::from(transcript_path?);
    tokio::task::spawn_blocking(move || last_transcript_user_text(&path)).await.ok().flatten()
}

/// 遠端 `hook.sh` 塞進 Stop payload 的另一個鍵：這一回合的起點（transcript 的 `origin.kind`，`human`／`task-notification`…）。
const CARRIED_ORIGIN_KIND: &str = "agm_origin_kind";

/// 這一回合**確定是人打的**時，那一句使用者訊息原文；起點不是 `human`、或讀不到起點（舊 CLI、讀不到 transcript、
/// 舊版 hook.sh）＝ `None`（issue #754）。
///
/// 背景工作完成喚醒 claude 的那一輪沒有新 prompt、transcript 最後一則使用者訊息是上一句，所以不能只憑「讀得到最後一句」
/// 就存：要起點是 `human`。遠端 transcript 在 agm-host 讀不到，起點跟文字一起由 `hook.sh` 從本機 transcript 帶來；
/// 本機就直接讀檔。Stop 到的時候這一輪的起點一定已經寫進 transcript，不像回合剛開（`-> working` 邊）時可能還讀到上一輪的。
async fn human_started_prompt(payload: &Value, transcript_path: Option<&str>) -> Option<String> {
    if let Some(kind) = payload.get(CARRIED_ORIGIN_KIND).and_then(Value::as_str) {
        return if kind == "human" { hook_user_text(None, payload, None).await } else { None };
    }
    let path = std::path::PathBuf::from(transcript_path?);
    let kind = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || lifecycle::starter_origin_kind_at(&path)).await.ok().flatten()
    };
    if kind.as_deref() != Some("human") {
        return None;
    }
    tokio::task::spawn_blocking(move || last_transcript_user_text(&path)).await.ok().flatten()
}

/// 外部回合沒有任何使用者訊息、而 Stop 證明起點是人打的：補記那一句。`begin_external_turn` 在回音跟上一句已答完的 prompt
/// 一樣時不存（為了擋背景工作喚醒那一輪，見 [`repeats_answered_prompt`]）；使用者真的重送同一句時這裡補回來。
/// 回合上已有使用者訊息就不動。
async fn store_resent_prompt_tx(
    app: &impl crate::capabilities::Db,
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    conv: &str,
    turn: &db::Turn,
    run: Option<&db::Run>,
    text: &str,
) -> Result<Option<db::Message>> {
    if turn.origin != "external" || text.trim().is_empty() {
        return Ok(None);
    }
    let have: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id = ? AND role = 'user'")
        .bind(&turn.id)
        .fetch_one(&mut **tx)
        .await?;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("resent_prompt_after_count_read", &turn.id).await;
    if have > 0 {
        return Ok(None);
    }
    tracing::info!(turn = %turn.id, "external turn: the Stop proves a person typed this prompt again; storing it");
    let from = relay_source(app, run, text).await;
    let msg = tx.insert_message_relayed_tx(conv, Some(&turn.id), "user", text, "hook", false, None, from.as_ref().map(|r| r.from_bot.as_str())).await?;
    crate::lifecycle::messages::mark_relay_turn(tx, &msg.id, from.as_ref().and_then(|r| r.from_turn.as_deref())).await?;
    Ok(Some(msg))
}

/// [`store_resent_prompt_tx`] 給「回合已被備援收掉、遲到的 Stop 才到」的路徑：自己開交易、補完就發事件。
async fn store_resent_prompt(app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands), bot_id: &str, conv: &str, turn: &db::Turn, run: Option<&db::Run>, payload: &Value, transcript_path: Option<&str>) -> Result<()> {
    if turn.origin != "external" {
        return Ok(());
    }
    let Some(text) = human_started_prompt(payload, transcript_path).await else { return Ok(()) };
    // 先數有沒有使用者訊息再寫：deferred 的話數完之後別的 writer 一 commit，INSERT 就 517，那一句補不上（#831）。
    let mut tx = db::begin_write(app.db()).await?;
    let added = store_resent_prompt_tx(app, &mut tx, conv, turn, run, &text).await?;
    tx.commit().await?;
    if let Some(m) = added {
        app.emit_message_added(bot_id, m).await;
    }
    Ok(())
}

/// transcript 最後一則使用者訊息。只讀尾巴：回合結束時它一定在最後幾百 KB 裡。
fn last_transcript_user_text(path: &std::path::Path) -> Option<String> {
    const TAIL: u64 = 512 * 1024;
    crate::transcript_read::read_tail(path, TAIL)?
        .lines()
        .rev()
        .find_map(crate::lifecycle::transcript_user_text)
        // CLI 把貼上的 prompt 包成 `<pasted_content>`（#218）：存進對話、比對的都是原文。
        .map(|t| crate::lifecycle::pasted_content::original(&t).into_owned())
}

/// 有兩邊原文而且保守比對仍對不上，才算「hook 回答的是另一句」。折疊排版空白後保留詞界；
/// 只有原文含字面 Tab 才另試去 Tab 候選。長度至少 8 字元的互含可辨認截斷回音；任一邊沒有就不下判斷。
fn answers_another_prompt(prompt: Option<&str>, hook_user: Option<&str>) -> bool {
    let (Some(p), Some(u)) = (prompt, hook_user) else { return false };
    if p.trim().is_empty() || u.trim().is_empty() {
        return false;
    }
    !prompt_texts_match(p, u)
}

/// [`answers_another_prompt`]，對一整組這一回合送出去的字：一句都對不上才算「回答的是別句」。
/// `None`＝這一回合沒有 prompt 可比（沒有證據，不下判斷）。
fn answers_none_of(prompts: Option<&[String]>, hook_user: Option<&str>) -> bool {
    let Some(ps) = prompts.filter(|ps| !ps.is_empty()) else { return false };
    ps.iter().all(|p| answers_another_prompt(Some(p), hook_user))
}

/// 這一回合送出去的字：`prompt` 再加上回合中補充進去的每一句（使用者 2026-09-28）。補充之後 transcript 最後一則
/// 使用者訊息是補充的那句，只拿 prompt 比就會被當成「回答的是別句」、開外部回合。沒有 prompt 就是 `None`，照舊不下判斷。
async fn with_supplements(app: &impl crate::capabilities::Db, turn_id: &str, prompt: Option<&str>) -> Result<Option<Vec<String>>> {
    let Some(p) = prompt.filter(|p| !p.trim().is_empty()) else { return Ok(None) };
    let mut all = vec![p.to_string()];
    all.extend(db::turn_supplements(app.db(), turn_id).await?);
    Ok(Some(all))
}

fn hook_user_is_new(existing: &[String], incoming: &str) -> bool {
    if incoming.trim().is_empty() {
        return false;
    }
    !existing.iter().any(|e| prompt_texts_match(e, incoming))
}

/// 外部回合的 hook 帶的「使用者訊息」其實是 transcript 裡最後一則 prompt：claude 自己接著做（背景 shell 跑完、
/// 排程叫醒）的那一輪沒有新的 prompt，hook 還是回報上一則（2026-10-01 cf-ox-2：「ui 審查你自己做」多存一則）。
/// 跟這一輪以外最近一則使用者訊息一樣、而且那一則的回合已經收掉，就是舊的那句，不再存。
pub async fn repeats_answered_prompt(conn: &mut sqlx::SqliteConnection, conv: &str, turn_id: &str, incoming: &str) -> Result<bool> {
    let last: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT m.content, t.status FROM messages m LEFT JOIN turns t ON t.id = m.turn_id
          WHERE m.conversation_id = ? AND m.role = 'user' AND (m.turn_id IS NULL OR m.turn_id <> ?)
          ORDER BY m.created_at DESC, m.rowid DESC LIMIT 1",
    )
    .bind(conv)
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(matches!(last, Some((content, Some(status))) if matches!(status.as_str(), "completed" | "completed_fallback" | "failed") && prompt_texts_match(&content, incoming)))
}

/// 只在既有那則是原文的（去空白）前綴且較短時才覆蓋；不是前綴就是另一句話，不能動。
/// 交易內：跟回合的收尾寫在同一個交易裡（#115）。
/// 把折行截斷的使用者回音補成 hook 的原文。回傳被補完的那一則（呼叫端要再推一次同 id，網頁才會換掉內容，#1006）；
/// 沒有任何一則被補完回 `None`。
async fn upgrade_clipped_user_message(conn: &mut sqlx::SqliteConnection, turn_id: &str, full: &str) -> Result<Option<db::Message>> {
    let full_sq = squash_ws(full);
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, content FROM messages WHERE turn_id = ? AND role = 'user' ORDER BY id")
            .bind(turn_id)
            .fetch_all(&mut *conn)
            .await?;
    for (id, content) in rows {
        let have = squash_ws(&content);
        if have.is_empty() || have.len() >= full_sq.len() || !full_sq.starts_with(&have) {
            continue;
        }
        sqlx::query("UPDATE messages SET content = ?, source = 'hook', updated_at = ? WHERE id = ?")
            .bind(full)
            .bind(db::now())
            .bind(&id)
            .execute(&mut *conn)
            .await?;
        tracing::info!(turn = %turn_id, msg = %id, "prompt 回音被截斷，用 hook 的原文補完");
        let message: db::Message = sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE id = ?").bind(&id).fetch_one(&mut *conn).await?;
        return Ok(Some(message));
    }
    Ok(None)
}

/// 把 hook 帶來的 native id（去重鑰匙）記到回合上：只填空的、只動備援關掉或剛由它升級的回合。
/// 沒有這兩道保護時，晚到 hook 手上的是舊快照，回合若已被別的路收好並蓋了自己的鑰匙，這裡會整個蓋掉。
async fn stamp_native_ids(
    conn: &mut sqlx::SqliteConnection,
    turn_id: &str,
    session_id: &Option<String>,
    native_turn_id: &Option<String>,
) -> Result<()> {
    sqlx::query(
        "UPDATE turns SET native_session_id=COALESCE(native_session_id, ?),
                          native_turn_id=COALESCE(native_turn_id, ?)
          WHERE id=? AND status IN ('completed_fallback','completed')",
    )
    .bind(session_id)
    .bind(native_turn_id)
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 遲到的 hook 撞上備援關掉的回合：沒有 assistant 訊息就用 hook 的回覆補上並改 `completed`，
/// 已有回覆才丟（防一回合兩則）。2026-09-13 GROK 備援 15 秒就關回合、36 秒後的真回覆被丟。
///
/// `same_turn`＝有正面證據這個 hook 就是這一回合的（hook 看得到的使用者訊息對上這回合的 prompt，或 CAS 輸給備援的
/// 那筆本來就是它）：已有的回覆**全是備援抓的**時，用 hook 的原文原地蓋掉最新那則（id 不變、`source=hook`、
/// 不再標可能不完整）並升 `completed`。2026-09-29 使用者：遠端 hook 走 spool 晚 25 秒到，畫面備援只抓到最後一段、
/// 還夾著 `✻ Crunched …` 狀態列，真回覆卻被丟掉。沒有證據（看不到使用者訊息）時照舊丟，防跨回合錯配。
/// 不會重開 c1526f7 的洞：`try_fallback` 認領與寫回覆同一交易、同一把 bot lock，讀到零則就真的是零則。
///
/// native id、升級、回覆寫在同一個交易裡（#115）：native id 是去重的鑰匙，先寫它再寫回覆的話，
/// 回覆那句失敗時收件匣的重試會被去重擋掉，回覆就永遠補不上了。
async fn fill_or_drop_late_hook(
    app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands),
    turn: &db::Turn,
    body_text: &str,
    session_id: &Option<String>,
    native_turn_id: &Option<String>,
    same_turn: bool,
) -> Result<()> {
    // 先數有沒有回覆再寫：deferred 的話數完之後別的 writer（對帳每一輪都在寫）一 commit 就 517，遲到的回覆補不上（#831）。
    let mut tx = db::begin_write(app.db()).await?;
    let has_reply: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn.id)
            .fetch_one(&mut *tx)
            .await?;
    #[cfg(all(test, feature = "daemon-test-harness"))]
    crate::lifecycle::race_point::hit("late_hook_after_reply_count", &turn.id).await;
    if !body_text.trim().is_empty() && has_reply > 0 && same_turn {
        return replace_fallback_reply(app, tx, turn, body_text, session_id, native_turn_id).await;
    }
    if body_text.trim().is_empty() || has_reply > 0 {
        // 丟掉的 hook：只有「有證據是這一回合的」才記它的鑰匙。沒有證據的（`same_turn = false`）可能屬於別的回合，
        // 記上去之後 `recent_fallback_turn`（只找 native id 為空的）就找不到這一回合，它真正的晚到 hook 沒地方收。
        if same_turn {
            stamp_native_ids(&mut tx, &turn.id, session_id, native_turn_id).await?;
        }
        tx.commit().await?;
        tracing::info!(turn = %turn.id, has_reply, "late hook dropped; turn already completed via terminal fallback");
        return Ok(());
    }
    // Keep the owner read in this transaction and ahead of durable message/status writes.
    // On failure, the inbox delivery can be retried without leaving a misrouted event behind.
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
        .bind(&turn.conversation_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("conversation {} has no owner", turn.conversation_id))?;
    // 這一筆是備援關掉的（`completed_fallback`），遲到的 hook 把回覆補上才升級成 `completed`。
    // 以前這句沒有 guard（`WHERE id=?`）：中間若有別的路徑動過它，這裡會無聲蓋過去（issue #68）。
    if tx.set_status_on(&turn.id, "completed_fallback", "completed", "遲到的 hook 補上回覆").await?
        != lifecycle::turn_controller::Outcome::Applied
    {
        tx.commit().await?;
        return Ok(());
    }
    stamp_native_ids(&mut tx, &turn.id, session_id, native_turn_id).await?;
    let message =
        tx.insert_message_tx(&turn.conversation_id, Some(&turn.id), "assistant", body_text, "hook", false, None).await?;
    tx.commit().await?;
    app.emit_message_added(&bot_id, message).await;
    tracing::info!(turn = %turn.id, "late hook filled a fallback-closed turn that had no reply");
    app.emit_turn(&turn.id).await;
    Ok(())
}

/// `fill_or_drop_late_hook` 的 `same_turn` 分支：這回合的 assistant 訊息全是 `terminal_fallback` 才蓋（有 hook 寫的就不動），
/// 蓋最新那則、其餘備援那幾則留著（通常只有一則）。回合 `completed_fallback → completed`（合法邊）；CAS 沒過就不蓋。
async fn replace_fallback_reply(
    app: &impl crate::events::ports::TurnCommands,
    mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    turn: &db::Turn,
    body_text: &str,
    session_id: &Option<String>,
    native_turn_id: &Option<String>,
) -> Result<()> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, source FROM messages WHERE turn_id=? AND role='assistant' ORDER BY created_at DESC, rowid DESC")
            .bind(&turn.id)
            .fetch_all(&mut *tx)
            .await?;
    let Some((latest, _)) = rows.first().filter(|_| rows.iter().all(|(_, src)| src == "terminal_fallback")) else {
        // 有證據是這一回合的 hook（呼叫端才會走到這裡），只是已經有 hook 寫的回覆：鑰匙照記。
        stamp_native_ids(&mut tx, &turn.id, session_id, native_turn_id).await?;
        tx.commit().await?;
        tracing::info!(turn = %turn.id, "late hook dropped; the turn already has a hook reply");
        return Ok(());
    };
    let latest = latest.clone();
    if tx.set_status_on(&turn.id, "completed_fallback", "completed", "遲到的 hook 以原文取代備援回覆").await?
        != lifecycle::turn_controller::Outcome::Applied
    {
        tx.commit().await?;
        return Ok(());
    }
    stamp_native_ids(&mut tx, &turn.id, session_id, native_turn_id).await?;
    sqlx::query("UPDATE messages SET content=?, source='hook', incomplete=0, updated_at=? WHERE id=?")
        .bind(body_text)
        .bind(db::now())
        .bind(&latest)
        .execute(&mut *tx)
        .await?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
        .bind(&turn.conversation_id)
        .fetch_optional(&mut *tx)
        .await?;
    let message: db::Message = sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE id=?").bind(&latest).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    tracing::info!(turn = %turn.id, msg = %latest, "late hook replaced the terminal-fallback reply with the transcript's");
    if let Some(bot_id) = bot_id {
        // 同一個 id 再推一次：前端遇到內容不同的同 id 訊息會換掉（`store/lists.ts` 的 `upsertSorted`）。
        app.emit_message_added(&bot_id, message).await;
    }
    app.emit_turn(&turn.id).await;
    Ok(())
}

/// Consume the one-shot `resume_native` request. Clearing the column before recording a mismatch
/// makes retries idempotent.
pub async fn consume_resume_session(
    app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands),
    bot: &db::Bot,
    run: &db::Run,
    reported_session_id: Option<&str>,
) -> Result<()> {
    let Some(expected) = run.resume_session_id.as_deref() else { return Ok(()) };
    // No session id proves nothing (codex notify may lack `thread-id`); leave the marker for the next hook.
    let Some(reported) = reported_session_id else { return Ok(()) };
    let mismatch = reported != expected;
    // 結論跟清掉標記寫在同一句：`resume_gate` 看 `resume_outcome` 放行（issue #92）。先前等到期寫了
    // `unverified` 的，這時候才到的回報照樣改成真正的結論。回報的 session 也在同一句記下：不拿鎖的讀者
    // （`api::started_json`，issue #107）看到 `verified` 時一定讀得到是哪一段。
    let consumed = sqlx::query(
        "UPDATE runs SET resume_session_id = NULL, resume_outcome = ?, native_session_id = COALESCE(native_session_id, ?)
          WHERE id = ? AND resume_session_id IS NOT NULL",
    )
    .bind(if mismatch { "mismatch" } else { "verified" })
    .bind(reported)
    .bind(&run.id)
    .execute(app.db())
    .await?;
    // A second hook may hold a stale `Run` snapshot; only the one that cleared the marker records a mismatch.
    if consumed.rows_affected() == 0 {
        return Ok(());
    }
    if mismatch {
        app.context_lost(bot, "resume_mismatch", Some(expected))
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    if !mismatch {
        app.retire_context_lost(&bot.id, expected).await;
    }
    // 閘門在等的就是這一則：排著的 prompt 現在可以送了（對不上的話，上面那則說明已經先進聊天室）。
    app.schedule_flush_queued(&bot.id);
    // 續行提示從接回驗過、畫面閒置起算 10 秒；對不上的就地取消（#424）。
    app.poke_resume_nudge(&bot.id);
    Ok(())
}

/// 120 秒內被終端備援關掉、還沒收到過 native id 的最近一筆回合（遲到的 hook 可能是它的真回覆，§4.3）。
async fn recent_fallback_turn(app: &impl crate::capabilities::Db, run_id: &str) -> Result<Option<db::Turn>> {
    // Fixed-width RFC3339 UTC, so lexicographic comparison is chronological.
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(sqlx::query_as::<_, db::Turn>(
        // 第二鍵同 issue #461 的其餘三處：`created_at` 只到毫秒，並列時 SQLite 回哪一列是未定義的，
        // 挑錯就把這次的回覆掛到另一回合底下。（同一個 run 通常一次只有一筆在飛，
        // 要兩筆 `completed_fallback` 撞同一毫秒才會中，機率比接回 session 那三處低很多；
        // 一起改是因為修法完全相同。）
        "SELECT * FROM turns WHERE run_id=? AND status='completed_fallback' AND native_turn_id IS NULL
         AND completed_at > ? ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(run_id)
    .bind(&cutoff)
    .fetch_optional(app.db())
    .await?)
}

/// hook 在 agent 那台觸發的時間（`received_at`，遠端 `hook.sh` 記的 UTC 秒）落在 `t` 開始之後，而且同一個對話在這段時間
/// 沒開過別的回合：這則 hook 收的就是 `t`。時間讀不懂就是沒有證據。兩台機器的時鐘只比到秒。
async fn fired_within(app: &impl crate::capabilities::Db, t: &db::Turn, received_at: Option<&str>) -> Result<bool> {
    let Some(fired) = received_at.and_then(|r| chrono::DateTime::parse_from_rfc3339(r).ok()) else { return Ok(false) };
    let Ok(started) = chrono::DateTime::parse_from_rfc3339(&t.created_at) else { return Ok(false) };
    if fired.timestamp() < started.timestamp() {
        return Ok(false);
    }
    let fired_at = fired.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let between: i64 = sqlx::query_scalar(
        // 「`t` 之後」同毫秒的也算：`created_at` 只到毫秒，同一毫秒開的另一個回合（寫入順序在 `t` 後面）一樣是在 `t` 之後開的。
        "SELECT COUNT(*) FROM turns WHERE conversation_id = ? AND id <> ?
            AND (created_at > ? OR (created_at = ? AND rowid > (SELECT rowid FROM turns WHERE id = ?)))
            AND created_at <= ?",
    )
    .bind(&t.conversation_id)
    .bind(&t.id)
    .bind(&t.created_at)
    .bind(&t.created_at)
    .bind(&t.id)
    .bind(&fired_at)
    .fetch_one(app.db())
    .await?;
    Ok(between == 0)
}

/// 那一回合被問了什麼：`prompt_text`，沒有就取第一則使用者訊息。
async fn turn_prompt(app: &impl crate::capabilities::Db, t: &db::Turn) -> Result<Option<String>> {
    match t.prompt_text.clone().filter(|p| !p.trim().is_empty()) {
        Some(p) => Ok(Some(p)),
        None => Ok(db::turn_user_messages(app.db(), &t.id).await?.into_iter().next()),
    }
}

/// 寫進回合的 native 證據。`(session, turn)` 是全域唯一的去重鑰匙（`turns_native`），而 hook 的 id 是送端自己報的：
/// 這組 id 若已經記在**別顆 bot** 的回合上（別顆 bot 先送了同樣的 id、或兩顆 bot 接回同一段 session），照寫會撞唯一索引，
/// 這顆 bot 的收尾就一直失敗、卡在收件匣重試，回合掛著等備援。撞到別人的就不寫 turn id（回合照常收尾），只留 session。
async fn native_evidence<'a>(
    app: &impl crate::capabilities::Db,
    bot_id: &str,
    session_id: Option<&'a str>,
    turn_id: Option<&'a str>,
) -> Result<lifecycle::turn_controller::NativeEvidence<'a>> {
    if let (Some(sid), Some(tid)) = (session_id, turn_id) {
        let taken: Option<String> = sqlx::query_scalar(
            "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id
              WHERE c.bot_id <> ? AND t.native_session_id = ? AND t.native_turn_id = ? LIMIT 1",
        )
        .bind(bot_id)
        .bind(sid)
        .bind(tid)
        .fetch_optional(app.db())
        .await?;
        if taken.is_some() {
            tracing::warn!(bot = bot_id, session = sid, turn = tid, "native turn id already belongs to another bot's turn; not recorded on this one");
            return Ok(lifecycle::turn_controller::NativeEvidence { session_id: Some(sid), turn_id: None });
        }
    }
    Ok(lifecycle::turn_controller::NativeEvidence { session_id, turn_id })
}

/// hook 是 pane 裡的行程送進來的：`transcript_path` 要在這顆 bot 自己的 `projects/`（codex：`sessions/`）底下才收進 `runs`，
/// 不然 daemon 之後每次輪詢都會去讀一個任意的檔（別顆 bot 的對話、`/etc/passwd`…）。不合就當沒帶（沿用原本的值）。
pub async fn vetted_transcript<H: HookHost>(app: &H, bot: &db::Bot, path: Option<&str>) -> Option<String> {
    let path = path.filter(|p| !p.trim().is_empty())?;
    if app.transcript_allowed(bot, path).await {
        Some(path.to_string())
    } else {
        tracing::warn!(bot = %bot.name, path, "ignoring a hook transcript_path outside the bot's own transcript directory");
        None
    }
}

pub async fn process_locked<H: HookHost>(app: &H, body: &HookBody) -> Result<()> {
    process_locked_for(app, body, None).await
}

pub async fn process_locked_for<H: HookHost>(app: &H, body: &HookBody, event_id: Option<&str>) -> Result<()> {
    let Some(bot) = db::bot(app.db(), &body.bot_id).await? else { return Ok(()) };
    // A3: also guards the spool-replay path, where nothing checked the token.
    if bot.deleted_at.is_some() {
        tracing::info!(bot = %bot.name, "hook for a deleted bot; ignored");
        return Ok(());
    }
    // #708：移交出去的專案，hook 的事歸接手的 daemon；記一行就丟（spool 重播、spawn hint 也一樣）。
    // 讀不到＝`projects` 那一列讀不到：往下走，後面讀主機的那幾步照它們自己的規則 fail closed（欠著的撞限、收件匣重試）。
    match app.db().bot_handed_off_to(&bot.id).await {
        Ok(None) => {}
        Ok(Some(to)) => {
            tracing::info!(bot = %bot.name, provider = %body.provider, handed_off_to = %to, "hook for a handed-off project; ignored");
            return Ok(());
        }
        Err(e) => tracing::warn!(bot = %bot.name, error = %e, "could not tell whether the hook's project was handed off; handling it as ours"),
    }
    // spool 重播的那條路也要擋（`receive` 已經擋過一次，但舊 spool 裡可能還躺著別種 provider 的）。
    if !provider_matches_kind(&body.provider, &bot.kind) {
        tracing::warn!(bot = %bot.name, kind = %bot.kind, provider = %body.provider, "hook from another provider; ignored");
        return Ok(());
    }
    // Provider matching is case-insensitive for compatibility; dispatch must use that same
    // canonical spelling or an accepted `Claude` event is consumed as an unknown event.
    let provider = body.provider.to_ascii_lowercase();
    let conv = db::conversation_id(app.db(), &bot.id).await?;
    let run = db::active_run(app.db(), &bot.id).await?;
    let kind = classify(&provider, &body.payload);
    // 被截斷或解不開的事件等於掉了一則：log 要說得出來，不然只看到一筆 `Identity`／`Ignore`（#1007）。只記旗標，不記 payload 內容。
    if body.truncated || matches!(&kind, HookKind::Ignore(why) if why.starts_with("unparseable payload")) {
        tracing::warn!(
            bot = %bot.name, provider = %body.provider, truncated = body.truncated,
            "hook payload was truncated or unparseable; the event itself is lost and the turn will close via the terminal fallback",
        );
    }
    if matches!(kind, HookKind::StatusLine) {
        tracing::debug!(bot = %bot.name, "statusline received");
    } else {
        tracing::info!(bot = %bot.name, provider = %body.provider, ?kind, "hook received");
    }

    // 世代圍籬（issue #69）：這一則屬於哪一代。只在這裡問一次，`fence` 是唯一的規則所在地——
    // 散在 hook／reconcile／fallback 各判一次，遲早會漂成三套。舊世代的事件只記錄，一個欄位都不改。
    // 放行的證明（`admitted`）是 hook 改 Turn 的前提：`turn_controller` 那兩支沒有它就不能呼叫（issue #125）。
    let mut admitted = None;
    // 放行的那一代主機世代（#1024）：agy 的授權失敗只改這一代，換代了就不改。
    let mut admitted_fence = None;
    if let Some(r) = &run {
        // 主機世代（#1035）：這一run 啟動時所在的遠端主機已經換代（`?confirm=repoint` 換指、改設定）＝這一則是上一代主機的，
        // 跟上面的 run 世代一樣：只記錄、不改任何欄位。
        let run_host_generation = db::run_host_generation(app.db(), &r.id).await.ok().flatten();
        let host_moved = match run_host_generation {
            Some(was) => app.admitted_host_fence(&bot).await.is_some_and(|f| f.generation() as i64 != was),
            None => false,
        };
        if host_moved {
            tracing::warn!(
                bot = %bot.name, provider = %body.provider, ?kind, run = %r.id,
                "run 啟動時的遠端主機已換代（repoint）：丟棄，不讓上一代主機的事件改到這一代的狀態（issue #1035）",
            );
            app.emit(
                "hook_fenced",
                json!({"bot_id": bot.id, "provider": body.provider, "run_id": r.id,
                       "prior_run_id": "", "session_id": hook_session_id(&body.payload), "why": "host_generation"}),
            )
            .await;
            return Ok(());
        }
        let ev = crate::lifecycle::fence::EventIdentity { run_id: body.run_id.as_deref(), session_id: hook_session_id(&body.payload) };
        let owner = app.db().classify_event_owner(&bot.id, r, ev).await;
        if !owner.may_mutate() {
            let (prior_run_id, why) = match &owner {
                crate::lifecycle::fence::Ownership::Stale { prior_run_id, why } => (prior_run_id.as_str(), *why),
                // `may_mutate()` 只有 `Stale` 會是 false；留著這一支是為了日後多一種歸屬時編譯器會提醒。
                _ => ("", "unknown"),
            };
            tracing::warn!(
                bot = %bot.name, provider = %body.provider, ?kind, run = %r.id, prior_run = %prior_run_id,
                session = hook_session_id(&body.payload), why,
                "上一代的 hook：丟棄，不讓它改到這一代的狀態（issue #69）",
            );
            // 看得見：丟掉一則事件不可以只活在這個函式裡。重跑同一則會走到同一個分支，仍然什麼都不改。
            app.emit(
                "hook_fenced",
                json!({"bot_id": bot.id, "provider": body.provider, "run_id": r.id,
                       "prior_run_id": prior_run_id, "session_id": hook_session_id(&body.payload), "why": why}),
            )
            .await;
            return Ok(());
        }
        if let crate::lifecycle::fence::Ownership::Unproven(why) = &owner {
            tracing::debug!(bot = %bot.name, run = %r.id, why, "這一則證不出世代歸屬：照既有規則處理");
        }
        admitted = owner.admit(&r.id);
        if admitted.is_some() {
            admitted_fence = app.admitted_host_fence(&bot).await;
        }
    }

    // claude ≥ 2.1.287 的 Stop 自己報背景工作（`background_tasks`）：以它為準，不用等畫面巡邏（`background_hook.rs`）。
    // 放在世代圍籬之後：上一代的 Stop 不能改這一代的帳。
    if provider == "claude" && matches!(&kind, HookKind::TurnComplete { .. }) {
        if let Some(r) = run.as_ref() {
            app.background_stop(r, &body.payload).await;
        }
    }

    // 登入失效的主動提示（#838，`login_prompt.rs`）：沒登入時回合只回 `Not logged in · Please run /login`（走 `Stop`，不是 `StopFailure`）
    // ＝授權失敗；綁著身分的 bot 正常答完一回合＝那個身分是通的。
    if provider == "claude" && admitted.is_some() {
        if let HookKind::TurnComplete { assistant: Some(a), .. } = &kind {
            if crate::login_prompt::is_not_logged_in_line(a) {
                app.login_on_auth_failure(&bot, admitted_fence.as_ref()).await;
            } else if !a.trim().is_empty() {
                app.login_on_turn_ok(&bot, admitted_fence.as_ref()).await;
            }
        }
    }

    // Codex's usage-reset hint is a TUI row, not in the payload; give the pane a moment to render it.
    if provider == "codex" && matches!(&kind, HookKind::TurnComplete { .. }) {
        if let Some(r) = run.as_ref() {
            app.schedule_codex_notice_capture(&bot.id, &r.id);
        }
        // 真的答完一回合＝帳號又能跑了，不必等橫幅寫的重置時間。
        if matches!(&kind, HookKind::TurnComplete { assistant: Some(a), .. } if !a.trim().is_empty()) {
            app.clear_limit_hit_for_bot(&bot).await;
        }
    }

    // 上一次打斷欠著的收尾先補（#147）：鍵已經生效、DB 那一半沒寫成的那一筆，要在這一則被對到任何回合之前收掉。
    // 寫不進去就讓這一則失敗、由收件匣重試，順序不亂。
    if matches!(kind, HookKind::TurnComplete { .. } | HookKind::TurnFailed { .. }) {
        app.settle_interruption(&bot.id, lifecycle::InterruptEvidence::Nothing).await?;
        // 送達結果欠著的同理（#149）：herdr 拒收、還沒收成 failed 的那一筆不能被這一則的回覆認領。
        app.settle_owed_deliveries(&bot.id).await?;
    }

    match kind {
        HookKind::Ignore(_) => {
            tracing::debug!("hook ignored");
            Ok(())
        }
        // issue #79：回合失敗是一級訊號。收掉那一筆 in-flight turn 並把原因寫進對話，
        // 而不是讓它掛著等 §4.3 備援（兩分鐘）或 stuck watchdog（五分鐘）來猜。
        HookKind::TurnFailed { session_id, turn_id, transcript_path, reason, detail } => {
            // 使用者自己中斷不是失敗。兩道都要：payload 說得出是中斷時照它說的；說不出來時看
            // daemon 自己的紀錄——`interrupt_bot` 先記下**被中斷的是哪一回合**再收 in-flight turn，
            // 對得上那一回合的 StopFailure 才是那一次 Esc 的回聲（AGM 交辦 2026-09-18）。只看「剛按過停」
            // 的話，Esc 之後馬上開的新回合撞額度也會被吞掉（#117）。
            if reason == FailureReason::Interrupted {
                // 回聲證明打斷的鍵生效了：還在等證據的那一筆在這裡收（#147）。
                app.settle_interruption(&bot.id, lifecycle::InterruptEvidence::Echo).await?;
                tracing::info!(bot = %bot.name, ?reason, "StopFailure 是使用者中斷的回聲：不算失敗");
                return Ok(());
            }
            // 登入失效：記下這個身分要重新登入、立刻重探它（網頁會跳提示，`login_prompt.rs`）。同一則重送只記一次。
            // agy 沒有身分：改把那台主機的 `tools.agy.logged_in` 翻成未登入（`agy_auth.rs`，issue #870）。
            if reason == FailureReason::Auth && (provider == "claude" || provider == "agy") && admitted.is_some() {
                app.login_on_auth_failure(&bot, admitted_fence.as_ref()).await;
            }
            // 同一筆送兩次（重試、spool 重播）：已經收過的那一回合。
            let seen = match (&session_id, &turn_id) {
                (Some(sid), Some(tid)) => sqlx::query_scalar::<_, String>(
                    "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id
                      WHERE c.bot_id=? AND t.native_session_id=? AND t.native_turn_id=?",
                )
                .bind(&bot.id)
                    .bind(sid)
                    .bind(tid)
                    .fetch_optional(app.db())
                    .await?,
                _ => None,
            };
            // 撞的是帳號額度（#108、#150）：先記撞限，**再**推回合結束——那個事件會叫醒 queue flush，排著的派工要看得到
            // 這個身分沒額度。撞額度是帳號的事實，跟這一則還有沒有回合可收無關：Esc 收掉回合之後才到、對上中斷的回聲、
            // 回合已被別的路收掉，都一樣要記，不然下一件派工會被送進沒額度的身分。只記這一代 run 送來的（`admitted`：
            // 上一代的在圍籬就丟了；沒有 run 時說不準是哪個身分）；已經收過的同一則不再記，免得把撞限時刻往後推。
            // 記不進去（讀不到主機、身分表還沒進來、憑據寫不進去）就讓這一則失敗、由收件匣重試（#108 重開）：回合先不收、
            // 不推回合結束，排著的派工照欠著的那一筆擋（`turn_error::owed_limit_hit`），不能當作沒撞。
            if admitted.is_some() && seen.is_none() {
                if let Some(d) = detail.as_deref().filter(|d| crate::turn_error::is_quota_exhaustion(d)) {
                    match bot.kind.as_str() {
                        "claude" => app.mark_claude_limit_hit(&bot, d).await?,
                        "agy" => app.mark_agy_limit_hit(&bot, d).await?,
                        _ => {}
                    }
                }
            }
            if let Some(r) = &run {
                let in_flight = db::in_flight_turn(app.db(), &r.id).await?;
                let ev = lifecycle::InterruptFailureEvidence {
                    session_id: session_id.as_deref(),
                    prompt_id: turn_id.as_deref(),
                    stamped_at: body
                        .received_at
                        .as_deref()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| t.with_timezone(&chrono::Utc)),
                };
                if app.settle_interrupt_echo(&bot.id, &r.id, &ev, in_flight.as_ref()).await? {
                    tracing::info!(bot = %bot.name, ?reason, "StopFailure 是被中斷那一回合的回聲：不算失敗");
                    return Ok(());
                }
            }
            if let Some(r) = &run {
                let transcript_path = vetted_transcript(app, &bot, transcript_path.as_deref()).await;
                sqlx::query(
                    "UPDATE runs SET native_session_id = COALESCE(native_session_id, ?),
                     transcript_path = COALESCE(?, transcript_path) WHERE id = ?",
                )
                .bind(&session_id)
                .bind(&transcript_path)
                .bind(&r.id)
                .execute(app.db())
                .await?;
            }
            // 同一筆送兩次（重試、spool 重播）不再收第二次——跟 `TurnComplete` 同一把鎖。
            if let Some(existing) = seen {
                tracing::info!(turn = %existing, "duplicate StopFailure ignored");
                return Ok(());
            }
            let (Some(r), Some(admitted)) = (&run, &admitted) else {
                tracing::info!(bot = %bot.name, ?reason, "StopFailure 但這顆 bot 沒有活著的 run：沒有回合可收");
                return Ok(());
            };
            let Some(t) = db::in_flight_turn(app.db(), &r.id).await? else {
                tracing::info!(bot = %bot.name, ?reason, "StopFailure 但沒有 in-flight turn：後到的訊號，不開新回合");
                return Ok(());
            };
            // CAS 在 `status='in_flight'` 上：後到的 Stop／§4.3 備援若已經把它收掉，這裡就什麼都不做，
            // 不會變成第二次收尾（issue #79 驗收第二條）。`delivery` 不動——字是送出去了，失敗的是回合。
            // 收尾（連同當作去重鑰匙的 native id）與說明同一個交易（#115）：說明寫不進去時整筆回滾，
            // 收件匣的重試才不會被去重擋掉、留下一筆沒有原因的失敗回合。
            let mut tx = app.db().begin().await?;
            let native = native_evidence(app, &bot.id, session_id.as_deref(), turn_id.as_deref()).await?;
            let claimed = tx.fail_with_native_evidence(&t.id, admitted, native).await?;
            if claimed != lifecycle::turn_controller::Outcome::Applied {
                tracing::info!(turn = %t.id, ?claimed, "StopFailure 來晚了：這一筆已經被別的路徑收掉，不重複收尾");
                return Ok(());
            }
            let note = match detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                Some(d) => format!("這一回合失敗收尾（{}）：{d}", reason.label()),
                None => format!("這一回合失敗收尾（{}）：agent 沒有給原因。", reason.label()),
            };
            let message = tx.insert_message_tx(&conv, Some(&t.id), "system", &note, "hook", false, None).await?;
            tx.commit().await?;
            // 撞限在上面（進這一支之前）就記好了：畫面那條路（`turn_error::capture`）要等讀 pane 才記得到，常常比 flush 晚。
            app.emit_message_added(&bot.id, message).await;
            app.emit_turn(&t.id).await;
            tracing::warn!(bot = %bot.name, turn = %t.id, ?reason, "StopFailure：回合收成失敗");
            Ok(())
        }
        // agy 的 statusLine 只用來補身分（對話一建立就有 `conversation_id`／`transcript_path`）。它的 `agent_state`／
        // `tool_confirmation_pending`／`quota` 之後（第二階段）才接；下面那段是 claude 的 rate_limits 與帳號，不能套在它身上。
        HookKind::StatusLine if provider == "agy" => {
            let s = |a: &str, b: &str| body.payload.get(a).or_else(|| body.payload.get(b)).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
            // 只記 conversation id（沒有的話什麼都不記）。**不記它的 `transcript_path`**：真機（1.2.16）statusLine 給的是
            // `~/.gemini/antigravity/brain/<id>/.system_generated/logs/transcript.jsonl`（摘要版、另一個目錄），
            // 完整的 `transcript_full.jsonl` 只有 hook 的 `transcriptPath` 帶得出來。
            if let (Some(r), Some(conv)) = (&run, s("conversationId", "conversation_id")) {
                let result = sqlx::query("UPDATE runs SET native_session_id = ? WHERE id = ? AND native_session_id IS NOT ?")
                    .bind(conv)
                    .bind(&r.id)
                    .bind(conv)
                    .execute(app.db())
                    .await?;
                if result.rows_affected() > 0 {
                    app.emit_bot_status(&bot.id).await;
                }
            }
            Ok(())
        }
        HookKind::StatusLine => {
            // 讀不到主機就丟掉這一份，下一次重繪會再來（#108 重開）：退回 local 會把遠端的讀數寫進本機那一格，
            // 還會拿它去校正、作廢本機身分真的撞限（`quota::set`）。
            let host = db::bot_host(app.db(), &bot.id).await?;
            // 讀數是這個 run 的 pane 送來的：帳號是它起來時的身分（issue #238），不是剛改、還沒重啟生效的設定。
            let identity = crate::quota::identity_for_run(&bot, run.as_ref());
            let identity = identity.as_deref();
            // Written only when changed — claude refreshes often and every write wakes every client.
            if let Some(r) = &run {
                let text = body.payload.get("status_line").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty());
                let mut rich = body.payload.clone();
                if let Some(o) = rich.as_object_mut() {
                    o.remove("status_line");
                    o.remove("hook_event_name");
                    let (email, warning) = claude_account(app, &host, identity).await;
                    if let Some(email) = email {
                        o.insert("account_email".into(), serde_json::json!(email));
                    }
                    if let Some(w) = warning {
                        o.insert("account_warning".into(), serde_json::json!(w));
                    }
                }
                let rich = serde_json::to_string(&rich).ok();
                let changed = text != r.status_line.as_deref() || rich != r.status_json;
                if changed {
                    // 快取倒數（`cache_clock`）：API 指紋真的動了才算一次 API 活動，閒置重繪不算。
                    crate::cache_clock::on_statusline(&r.id, r.status_json.as_deref(), rich.as_deref(), &db::now());
                    let _ = sqlx::query("UPDATE runs SET status_line = COALESCE(?, status_line), status_json = ? WHERE id = ?")
                        .bind(text)
                        .bind(&rich)
                        .bind(&r.id)
                        .execute(app.db())
                        .await;
                    app.emit_bot_status(&bot.id).await;
                }
                // #750：server fallback 後實際在跑的模型（statusLine 是權威），校正 runtime_model、不碰 bots.model。
                if provider == "claude" {
                    app.adopt_statusline_model(r, &body.payload).await;
                }
            }
            // Always keyed under the bot's **host**: remote limits must not land on the local row (SPEC §14).
            if let Some(idn) = identity {
                if let Some(q) = crate::quota::quota_from_statusline(&body.payload, Some(idn)) {
                    // 對 claude 共用預設帳號的身分（沒設 CLAUDE_CONFIG_DIR）寫裸 `claude`：另開 `claude:cc0` 會少掉
                    // `/usage` 探測的 Fable 週窗。規則跟 codex 同一支（`quota::quota_base_for_host`）。
                    let key = app.quota_base_for_host(&host, "claude", Some(idn)).await;
                    app.set_quota(&host, &key, q).await;
                }
            } else if let Some(q) = crate::quota::quota_from_statusline(&body.payload, None) {
                app.set_quota(&host, "claude", q).await;
            }
            if let (Some(r), Some(sid)) = (&run, body.payload.get("session_id").and_then(|v| v.as_str())) {
                let _ = sqlx::query("UPDATE runs SET native_session_id = COALESCE(native_session_id, ?) WHERE id = ?")
                    .bind(sid)
                    .bind(&r.id)
                    .execute(app.db())
                    .await;
            }
            Ok(())
        }
        // issue #82：純可見性快照，整筆覆蓋 `runs.subagent_json`，不建立、不動任何 Turn，不查重複
        // （跟 `StatusLine` 一樣，最新的贏），也不碰 `bots`／`parent_bot_id`——那是 §6.5a 血緣認領的事，
        // 這裡的 `agent_id` 是同一行程內 Task 工具呼叫的 id，不是哪個 pane 的身分。沒有活著的 run
        // 就沒地方寫，直接丟（跟 `HookKind::Identity` 一樣）。
        HookKind::SubagentEvent { event, agent_id, agent_type, transcript_path } => {
            if let Some(r) = &run {
                let snapshot = json!({
                    "event": event,
                    "agent_id": agent_id,
                    "agent_type": agent_type,
                    "transcript_path": transcript_path,
                    "at": db::now(),
                });
                sqlx::query("UPDATE runs SET subagent_json = ? WHERE id = ?")
                    .bind(snapshot.to_string())
                    .bind(&r.id)
                    .execute(app.db())
                    .await?;
            }
            Ok(())
        }
        // issue #94：這顆 bot 自己剛剛用 `herdr pane split`／`agent start` 開出 `pane_id`——記下來給
        // `reconcile::adopt_child` 當比同 tab 更早、更精確的線索。純粹記錄一個事實，不查任何 bot／pane
        // 現在的狀態，也不需要活著的 run（這是這顆 bot 自己的行程剛做的事，不是它的 Turn 的事）。
        HookKind::SpawnHint { pane_ids } => {
            for pane_id in pane_ids {
                crate::spawn_hints::record(app, &bot.id, &pane_id).await?;
            }
            Ok(())
        }
        // 2026-10-02 使用者：claude 問的題目與使用者的答案要留在對話裡（`ask_answers`）。
        HookKind::AskAnswered(rec) => {
            ask_answers::record_in_flight(app, &bot.id, &conv, vec![rec]).await?;
            Ok(())
        }
        // 2026-09-30 使用者：console-rpa（m4p）直送給 cicd 的一句被存成使用者訊息。spool 每 30 秒左右才收一次，
        // 收件方的回音常常比這則報備先到，所以除了照常記下，也回頭補標已經存下的那一則。
        HookKind::RelayAnnounce { to_agent, text } => {
            // 讀不到寄件者的主機就不記（記了也沒人認得出是哪台）；補標那邊同樣要讀主機，會一起報錯。
            let host = db::bot_host(app.db(), &bot.id).await?;
            // 寄件 bot 送這句當下正在跑的回合：用 shim 寫進報備的 `received_at`（送出時間；spool 可能晚好幾秒才收到）。
            let sent_at = body.received_at.clone().unwrap_or_else(db::now);
            let from_turn = crate::lifecycle::relay_watch::sender_turn_at(app, &bot.id, &sent_at).await?;
            crate::agent_relay::announce(&host, &bot.id, &to_agent, &text, from_turn.as_deref());
            relay_backfill(app, &bot.id, from_turn.as_deref(), &to_agent, &text).await
        }
        HookKind::Identity { session_id, transcript_path } => {
            if let Some(r) = &run {
                // Codex/Grok are checked on their first completed turn; don't consume the request early.
                // agy 的對話是第一則 prompt 才建立，`SessionStart`／`PreInvocation` 帶的 conversationId 就是真正在用的那段：
                // `--conversation=<不存在的 id>` 只警告、開新對話，靠這裡對不上才抓得到。
                if provider == "claude" || provider == "agy" {
                    consume_resume_session(app, &bot, r, session_id.as_deref()).await?;
                }
                let transcript_path = vetted_transcript(app, &bot, transcript_path.as_deref()).await;
                sqlx::query(
                    "UPDATE runs SET native_session_id = COALESCE(?, native_session_id),
                     transcript_path = COALESCE(?, transcript_path) WHERE id = ?",
                )
                .bind(&session_id)
                .bind(&transcript_path)
                .bind(&r.id)
                .execute(app.db())
                .await?;
                app.emit_bot_status(&bot.id).await;
            }
            Ok(())
        }
        HookKind::TurnComplete { session_id, turn_id, transcript_path, assistant, user } => {
            // Stop 的 transcript_path 不只寫進 `runs`：下面還會直接讀它、把內容當成使用者訊息記進對話，所以一進來就驗。
            let transcript_path = vetted_transcript(app, &bot, transcript_path.as_deref()).await;
            // Remote paths are retained for SSH-side transcript transfer, never opened on this host.
            let transcript_read_path = if let Some(path) = transcript_path.as_deref() {
                if app.local_transcript_allowed(&bot, path).await { Some(path.to_string()) } else { None }
            } else {
                None
            };
            if provider == "codex" || provider == "grok" {
                if let Some(r) = &run {
                    consume_resume_session(app, &bot, r, session_id.as_deref()).await?;
                }
            }
            // agy 沒有 claude 那種 statusLine 數字：模型與 context 的 token 數（hook 子行程讀 transcript 帶來的）記成網頁讀得懂的精簡 `status_json`。
            if provider == "agy" {
                if let Some(r) = &run {
                    let model = r.runtime_model.as_deref().or(bot.model.as_deref());
                    if let Some(json) = crate::agy_support::status_json(model, body.payload.get("lastInputTokens").and_then(Value::as_i64)) {
                        if r.status_json.as_deref() != Some(json.as_str()) {
                            sqlx::query("UPDATE runs SET status_json = ? WHERE id = ?").bind(&json).bind(&r.id).execute(app.db()).await?;
                            app.emit_bot_status(&bot.id).await;
                        }
                    }
                }
            }
            // Codex has no SessionStart; grok's carries no transcript path.
            if let Some(r) = &run {
                sqlx::query(
                    "UPDATE runs SET native_session_id = COALESCE(native_session_id, ?),
                     transcript_path = COALESCE(?, transcript_path) WHERE id = ?",
                )
                .bind(&session_id)
                .bind(&transcript_path)
                .bind(&r.id)
                .execute(app.db())
                .await?;
            }

            // 3a. 收件匣重播：這一則已經收成回合了（commit 之後、標 processed_at 之前重啟）。不再開第二個，只補發通知。
            if let Some(event) = event_id {
                if let Some(turn) = turn_of_event(app.db(), event).await? {
                    tracing::info!(turn = %turn, event, "inbox replay of an already-applied hook ignored; re-announcing");
                    reannounce_turn(app, &bot.id, &turn).await;
                    return Ok(());
                }
            }

            // 3. dedup on (native_session_id, native_turn_id)
            if let (Some(sid), Some(tid)) = (&session_id, &turn_id) {
                let dup: Option<String> =
                    sqlx::query_scalar(
                        "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id
                          WHERE c.bot_id=? AND t.native_session_id=? AND t.native_turn_id=?",
                    )
                    .bind(&bot.id)
                        .bind(sid)
                        .bind(tid)
                        .fetch_optional(app.db())
                        .await?;
                if let Some(existing) = dup {
                    tracing::info!(turn = %existing, "duplicate hook ignored");
                    return Ok(());
                }
            }

            // 4. the run's single in-flight Turn
            let target = match &run {
                Some(r) => db::in_flight_turn(app.db(), &r.id).await?,
                None => None,
            };
            // `unknown`＝打字進去但沒有證據。這時 hook 若看得到**別句**使用者訊息，那是使用者在終端手打的另一句：
            // 答案不能掛到原本那則，更不能順手把它標成「已送達」（第二輪 review 送達線 #3）。看不到使用者訊息時照舊認領。
            let (target, user) = match target {
                Some(t) if t.delivery == "unknown" => {
                    let seen = hook_user_text(user.as_deref(), &body.payload, transcript_read_path.as_deref()).await;
                    let sent = with_supplements(app, &t.id, t.prompt_text.as_deref()).await?;
                    if answers_none_of(sent.as_deref(), seen.as_deref()) {
                        tracing::info!(turn = %t.id, bot = %bot.id, "hook 的使用者訊息不是這一筆 unknown 的 prompt：不認領，記成外部回合");
                        (None, user.or(seen))
                    } else {
                        (Some(t), user)
                    }
                }
                // 備援關掉 T1、使用者接著送了 T2（已送達、in-flight），T1 的真回覆這時才到（#297）：
                // hook 看得到的使用者訊息不是 T2 的、而是那筆備援回合的，答案屬於 T1，不能收掉 T2。
                // 兩邊都對不上（或看不到使用者訊息）時照舊認領——沒有證據不改判。
                Some(t) => {
                    if let Some(r) = &run {
                        if let Some(late) = recent_fallback_turn(app, &r.id).await? {
                            let seen = hook_user_text(user.as_deref(), &body.payload, transcript_read_path.as_deref()).await;
                            let ours = with_supplements(app, &t.id, t.prompt_text.as_deref()).await?;
                            let late_prompt = with_supplements(app, &late.id, turn_prompt(app, &late).await?.as_deref()).await?;
                            if answers_none_of(ours.as_deref(), seen.as_deref())
                                && seen.is_some()
                                && late_prompt.is_some()
                                && !answers_none_of(late_prompt.as_deref(), seen.as_deref())
                            {
                                tracing::info!(turn = %t.id, late = %late.id, bot = %bot.id, "遲到 hook 回答的是備援關掉的那一回合，不是現在 in-flight 的：補回那一回合");
                                fill_or_drop_late_hook(app, &late, &assistant.clone().unwrap_or_default(), &session_id, &turn_id, true).await?;
                                return Ok(());
                            }
                        }
                    }
                    (Some(t), user)
                }
                other => (other, user),
            };
            let body_text = assistant.clone().unwrap_or_default();

            if let (Some(t), Some(admitted)) = (target, &admitted) {
                // 收尾（連同當作去重鑰匙的 native id）與訊息同一個交易（#115）：訊息寫不進去時整筆回滾，
                // 收件匣重試時才不會被去重擋掉、留下一筆沒有回覆的 completed 回合。
                let mut tx = app.db().begin().await?;
                let native = native_evidence(app, &bot.id, session_id.as_deref(), turn_id.as_deref()).await?;
                let claimed = tx.complete_with_native_evidence(&t.id, admitted, native).await?;
                match &claimed {
                    lifecycle::turn_controller::Outcome::Applied => {
                        stamp_source_event(&mut tx, &t.id, event_id).await?;
                    }
                    // 不是圍籬放行那一代的回合：一個字都不掛上去。
                    lifecycle::turn_controller::Outcome::Fenced(why) => {
                        tracing::warn!(turn = %t.id, why, "hook 收尾：回合不屬於放行的那一代，不動");
                        return Ok(());
                    }
                    // Lost the CAS to the §4.3 fallback: adding the hook's copy made two replies
                    // (review 2026-09-12 #5). Stopped/failed turns still keep the reply.
                    lifecycle::turn_controller::Outcome::Raced { now } if now == "completed_fallback" => {
                        // 這個交易沒寫到東西；先結束它，`fill_or_drop_late_hook` 自己開一個。
                        tx.commit().await?;
                        // 這筆就是 hook 要收的 in-flight 回合，只是 CAS 輸給備援：同一回合。
                        fill_or_drop_late_hook(app, &t, &body_text, &session_id, &turn_id, true).await?;
                        store_resent_prompt(app, &bot.id, &conv, &t, run.as_ref(), &body.payload, transcript_read_path.as_deref()).await?;
                        return Ok(());
                    }
                    _ => {}
                }
                let mut added = Vec::new();
                // `begin_external_turn` already stored the scraped echo; add the hook's copy only if different.
                if t.origin == "external" {
                    if let Some(u) = user.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        let have: Vec<String> =
                            sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'user' ORDER BY created_at")
                                .bind(&t.id)
                                .fetch_all(&mut *tx)
                                .await?;
                        if hook_user_is_new(&have, u) && !repeats_answered_prompt(&mut tx, &conv, &t.id, u).await? {
                            let from = relay_source(app, run.as_ref(), u).await;
                            let msg = tx
                                .insert_message_relayed_tx(&conv, Some(&t.id), "user", u, "hook", false, None, from.as_ref().map(|r| r.from_bot.as_str()))
                                .await?;
                            crate::lifecycle::messages::mark_relay_turn(&mut tx, &msg.id, from.as_ref().and_then(|r| r.from_turn.as_deref())).await?;
                            added.push(msg);
                        } else {
                            // 刮下來的回音可能被折行截斷；hook 的原文較可信，補完下半截。
                            // 同一個 id 在 commit 後再推一次：前端遇到內容不同的同 id 訊息會換掉（#1006）。
                            added.extend(upgrade_clipped_user_message(&mut tx, &t.id, u).await?);
                        }
                    } else if let Some(text) = human_started_prompt(&body.payload, transcript_read_path.as_deref()).await {
                        added.extend(store_resent_prompt_tx(app, &mut tx, &conv, &t, run.as_ref(), &text).await?);
                    }
                }
                if !body_text.is_empty() {
                    added.push(tx.insert_message_tx(&conv, Some(&t.id), "assistant", &body_text, "hook", false, None).await?);
                }
                tx.commit().await?;
                for m in added {
                    app.emit_message_added(&bot.id, m).await;
                }
                app.emit_turn(&t.id).await;
                return Ok(());
            }

            // §4.3: a late hook must not overwrite a fallback-claimed turn.
            let mut user = user;
            if let Some(r) = &run {
                let late = recent_fallback_turn(app, &r.id).await?;
                if let Some(t) = late {
                    // 跟上面 unknown 那條同一個判斷（review3 c1 L13）：hook 帶來的使用者訊息若是**別句**
                    // （使用者改到 pane 裡直接打、herdr 卡 working 沒開外部回合），答案屬於那一句，不能補進 T1、
                    // 更不能把 T1 標成 completed。對不上就往下記成外部回合；看不到使用者訊息時照舊補。
                    let seen = hook_user_text(user.as_deref(), &body.payload, transcript_read_path.as_deref()).await;
                    let prompt = with_supplements(app, &t.id, turn_prompt(app, &t).await?.as_deref()).await?;
                    if answers_none_of(prompt.as_deref(), seen.as_deref()) {
                        tracing::info!(turn = %t.id, bot = %bot.id, "遲到 hook 的使用者訊息不是這一筆備援回合的 prompt：不補，記成外部回合");
                        user = user.or(seen);
                    } else {
                        // 看得到使用者訊息而且對上＝同一回合。遠端 bot 讀不到 transcript（在那台）、看不到使用者訊息時，
                        // 退而看 hook 在那台觸發的時間：落在這一回合開始之後、中間沒開過別的回合，也是同一回合（2026-10-02
                        // wits-ops-web：備援抓到一份工具輸出當回覆，真回覆晚 25 秒到卻被丟）。都不成立只能補空的，不蓋已有的。
                        let same = seen.is_some() || fired_within(app, &t, body.received_at.as_deref()).await?;
                        fill_or_drop_late_hook(app, &t, &body_text, &session_id, &turn_id, same).await?;
                        store_resent_prompt(app, &bot.id, &conv, &t, run.as_ref(), &body.payload, transcript_read_path.as_deref()).await?;
                        return Ok(());
                    }
                }
            }

            // 5. external turn：回合（帶去重用的 native id）與它的訊息同一個交易（#115）。
            let tid = db::ulid();
            let native = native_evidence(app, &bot.id, session_id.as_deref(), turn_id.as_deref()).await?;
            let mut tx = app.db().begin().await?;
            sqlx::query(
                "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, native_session_id, native_turn_id, created_at, completed_at, source_event_id)
                 VALUES (?,?,?,'external','completed','ok',?,?,?,?,?)",
            )
            .bind(&tid)
            .bind(&conv)
            .bind(run.as_ref().map(|r| r.id.clone()))
            .bind(native.session_id)
            .bind(native.turn_id)
            .bind(db::now())
            .bind(db::now())
            .bind(event_id)
            .execute(&mut *tx)
            .await?;
            let mut added = Vec::new();
            if let Some(u) = user.filter(|s| !s.is_empty()) {
                let from = relay_source(app, run.as_ref(), &u).await;
                let msg = tx.insert_message_relayed_tx(&conv, Some(&tid), "user", &u, "hook", false, None, from.as_ref().map(|r| r.from_bot.as_str())).await?;
                crate::lifecycle::messages::mark_relay_turn(&mut tx, &msg.id, from.as_ref().and_then(|r| r.from_turn.as_deref())).await?;
                added.push(msg);
            }
            if !body_text.is_empty() {
                added.push(tx.insert_message_tx(&conv, Some(&tid), "assistant", &body_text, "hook", false, None).await?);
            }
            tx.commit().await?;
            for m in added {
                app.emit_message_added(&bot.id, m).await;
            }
            app.emit_turn(&tid).await;
            Ok(())
        }
    }
}

/// 這則收件匣事件已經收成哪個回合（`turns.source_event_id`）；沒有＝還沒處理過。
async fn turn_of_event(pool: &sqlx::SqlitePool, event_id: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM turns WHERE source_event_id = ?").bind(event_id).fetch_optional(pool).await?)
}

/// 收掉回合的那個交易裡順手記下是哪則 hook 收的（跟回合同生共死）。沒有收件匣 id（直接 `process`）就什麼都不寫。
async fn stamp_source_event(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, turn_id: &str, event_id: Option<&str>) -> Result<()> {
    if let Some(event) = event_id {
        sqlx::query("UPDATE turns SET source_event_id = ? WHERE id = ? AND source_event_id IS NULL")
            .bind(event)
            .bind(turn_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// 重播命中既有回合：把原本 commit 之後才發的通知補發一次。`message_added` 前端靠訊息 id 去重；回合事件有重試與冪等的消費者。
async fn reannounce_turn(app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands), bot_id: &str, turn_id: &str) {
    match sqlx::query_as::<_, db::Message>("SELECT * FROM messages WHERE turn_id = ? AND source = 'hook' ORDER BY created_at, rowid")
        .bind(turn_id)
        .fetch_all(app.db())
        .await
    {
        Ok(messages) => {
            for m in messages {
                app.emit_message_added(bot_id, m).await;
            }
        }
        Err(e) => tracing::warn!(turn = turn_id, error = ?e, "could not re-announce hook messages of a replayed event"),
    }
    app.emit_turn(turn_id).await;
}

// Remote drain: see SPEC §11.4.3–§11.4.5.

const STATUS_MARKER: &str = "---AM-STATUS---";

/// #500 複看：`am_fold` 跑完 `.claim` 還在＝這台的 spool 停收了，而且從外面看不出來——腳本 exit 0、
/// `n = 0`、`drain_remote` 兩個 `info!` 都被 `n > 0` 擋著，一行 log 都不會有。遠端自己判斷得出來，
/// 所以就地印一行（後面接 `.claim` 的位元組數），不必多一趟 ssh。
const FOLD_STUCK_MARKER: &str = "AM_FOLD_STUCK";

pub const DRAIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// The event can beat the spool write; retry once, still ahead of the 5s terminal fallback.
pub const DRAIN_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Catches status events that never arrived (§11.4.4).
pub const SCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Default)]
struct Drained<'a> {
    lines: Vec<&'a str>,
    status: Option<String>,
    /// `Some(位元組數)`＝`.claim` 併不進 `.replaying`，這台的 spool 停收了（#500 複看）。
    stuck_bytes: Option<i64>,
}

fn parse_drain_output(text: &str) -> Drained<'_> {
    let mut out = Drained::default();
    let mut status: Option<String> = None;
    let mut it = text.lines();
    for line in it.by_ref() {
        if line.trim() == STATUS_MARKER {
            status = Some(it.collect::<Vec<_>>().join("\n"));
            break;
        }
        let l = line.trim();
        if let Some(n) = l.strip_prefix(FOLD_STUCK_MARKER) {
            // 數字讀不出來不影響判斷「卡住了」——標記本身才是訊號。
            out.stuck_bytes = Some(n.trim().parse().unwrap_or(-1));
            continue;
        }
        if !l.is_empty() {
            out.lines.push(l);
        }
    }
    out.status = status.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    out
}

fn spool_line_log_context(line: &str) -> String {
    format!("malformed spool record: {} bytes (payload omitted)", line.len())
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod spool_log_privacy_tests {
    use super::{spool_line_log_context, FailureReason, HookKind};

    #[test]
    fn malformed_spool_log_context_does_not_repeat_payload_text() {
        let secret = r#"{"payload":{"prompt":"PRIVATE-HOOK-PROMPT-641"}}"#;
        let context = spool_line_log_context(secret);
        assert!(!context.contains("PRIVATE-HOOK-PROMPT-641"));
        assert!(context.contains(&secret.len().to_string()));
    }

    #[test]
    fn hook_kind_debug_does_not_repeat_payload_values() {
        let secret = "PRIVATE-HOOK-PROMPT-982";
        let kind = HookKind::TurnComplete {
            session_id: Some("session".into()),
            turn_id: Some("turn".into()),
            transcript_path: Some("/private/transcript.jsonl".into()),
            assistant: Some(secret.into()),
            user: Some(secret.into()),
        };
        assert!(!format!("{kind:?}").contains(secret));

        let failed = HookKind::TurnFailed {
            session_id: None,
            turn_id: None,
            transcript_path: None,
            reason: FailureReason::Unknown,
            detail: Some(secret.into()),
        };
        assert!(!format!("{failed:?}").contains(secret));
        assert!(!format!("{:?}", HookKind::Ignore(secret.into())).contains(secret));
    }
}

/// SPEC §11.4.3.
pub async fn drain_remote<H: HookHost>(app: &H, host: &str, bot_id: &str) -> Result<usize> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let Some(conn) = app.hosts().get(host).await else { return Ok(0) };
    if !conn.is_connected() {
        return Ok(0);
    }
    let root = crate::hosts::remote_root_for(app.instance().as_deref());
    // 第一趟：claim（`mv` 成 `.replaying` 再 `cat`），**不刪**。遠端那份是唯一的副本。
    let text = conn.ssh_exec(&claim_script(bot_id, &root)?).await?;
    let drained = parse_drain_output(&text);
    let (lines, status) = (drained.lines, drained.status);
    note_fold_stuck(app, host, bot_id, drained.stuck_bytes).await;
    let mut n = 0usize;
    for line in lines {
        match serde_json::from_str::<HookBody>(line) {
            Ok(b) => {
                if b.bot_id != bot_id {
                    tracing::warn!("remote spool line for a different bot; skipped");
                    continue;
                }
                // 收不下就整個放棄這一輪：`.replaying` 留在遠端，下一次 claim 會再讀到它。
                // 重讀不會變成兩筆——`hook_inbox` 的 dedupe_key 擋掉（issue #70 的去重規則）。
                crate::hook_inbox::accept(app.db(), &b, crate::hook_inbox::Source::Remote).await?;
                n += 1;
            }
            // A4: 解不開的行再 claim 幾次也一樣，留著只會擋住 ack；記一筆丟掉。
            Err(_) => tracing::warn!(record = %spool_line_log_context(line), "unparseable remote spool line; dropped"),
        }
    }
    // 第二趟：本機已經 commit 了，才准刪遠端那份。ack 失敗＝`.replaying` 還在，下一輪重來。
    conn.ssh_exec(&ack_script(bot_id, &root)?).await?;
    if n > 0 {
        app.wake_hook_inbox();
    }
    if let Some(raw) = status {
        match status_body(bot_id, &raw) {
            Some(b) => {
                if let Err(e) = process_locked(app, &b).await {
                    tracing::warn!(error = ?e, "remote statusline replay failed");
                }
            }
            None => tracing::warn!(bot_id, host, "unparseable remote hook-status.json; dropped"),
        }
    }
    if n > 0 {
        tracing::info!(bot_id, host, replayed = n, "remote hook spool replayed");
    }
    Ok(n)
}

/// #500 複看：`.claim` 卡住要有人知道。每輪更新一顆 bot 的連續次數，`supervisor::incidents` 的探針拿它開票。
/// 計數在記憶體、重啟重算（SPEC §18.9）——重啟之後第一輪 drain 就會重新看到標記。
async fn note_fold_stuck(app: &impl crate::hookrecv::SpoolFoldStuck, host: &str, bot_id: &str, bytes: Option<i64>) {
    let key = format!("{host}/{bot_id}");
    let mut g = app.spool_fold_stuck().lock().await;
    match bytes {
        Some(bytes) => {
            let e = g.entry(key).or_insert((0, bytes));
            e.0 += 1;
            e.1 = bytes;
            tracing::warn!(
                bot_id,
                host,
                claim_bytes = bytes,
                rounds = e.0,
                "remote spool is stuck: the claimed copy cannot be folded into .replaying, so nothing new is being taken"
            );
        }
        // 併進去了：這一輪是好的，重新算。
        None => {
            g.remove(&key);
        }
    }
}

fn bots_dir(bot_id: &str, root: &str) -> Result<String> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    Ok(format!("\"$HOME/{root}/bots/\"{}", sh_quote(bot_id)))
}

/// §11.4.5 第一趟：把 spool 收攏成 `.replaying` 並讀出來，**不刪**（issue #70）。
///
/// 以前這裡是 `cat` 完就 `rm`：遠端那份唯一的副本在位元組寫進本機任何地方之前就沒了，
/// 中間掉線或 daemon 掛掉，事件就永遠不見。刪的動作移到 [`ack_script`]，在本機 commit 之後。
///
/// #493：摘下來的動作是 `mv "$f" "$f.claim"`（rename，原子），不是「`cat` 完再 `rm -f "$f"`」。
/// 後者的 `cat` 與 `rm` 是兩支各自 fork／exec 的外部指令，中間 hook 附加進來的行會被 `rm` 連檔刪掉——
/// 而這條路徑正好只在「上一輪 ack 沒成功」時走到，也就是剛斷線重連、hook 正在補寫的那一刻。
/// rename 之後 append 落在新的 inode 上，這一輪碰不到它。`.claim` 是「摘下來、還沒併進 `.replaying`」
/// 的中繼：崩在中間的話下一輪的 `am_fold` 會接著併，不會留在那裡沒人管。
///
/// #500：`am_fold` 沒把 `.claim` 收掉（`.replaying` 寫不進去、磁碟滿）時**不准摘新的 spool**——
/// 這支腳本沒有 `set -e`、`am_fold` 的回傳值也沒人看，直接 `mv` 會把那份唯一的副本蓋掉，而且是無聲的。
/// 這一輪就只讀 `.replaying`，spool 留到下一輪，跟本機 `fold_spool(...)?` 的 `?` 行為對齊。
/// `hook-spool.d/.tmp.*` 只有先寫完再 rename 才會成為事件；清掉超過一天的暫存檔可回收被強制終止的寫入殘留，保留正在寫的檔。
///
/// `hook-status.json` 是例外，照舊讀完就刪：它是單槽、最新的贏的訊號（不是佇列），
/// 掉一格只是晚一次重繪——理由與本機 StatusLine 不進收件匣是同一個（[`crate::hook_inbox`]）。
fn claim_script(bot_id: &str, root: &str) -> Result<String> {
    let dir = bots_dir(bot_id, root)?;
    Ok(format!(
        // `.replaying` 是這支腳本自己建的，裡面裝完整 payload：0600，不交給那台機器的 login umask（#501）。
        "umask 077\n\
         d={dir}\n\
         f=\"$d/hook-spool.jsonl\"\n\
         am_fold() {{ [ -f \"$f.claim\" ] || return 0; \
         if [ -s \"$f.replaying\" ] && [ -n \"$(tail -c 1 \"$f.replaying\")\" ]; then printf '\\n' >> \"$f.replaying\"; fi; \
         cat \"$f.claim\" >> \"$f.replaying\" && rm -f \"$f.claim\"; }}\n\
         am_fold\n\
         if [ -f \"$f\" ] && [ ! -f \"$f.claim\" ]; then mv \"$f\" \"$f.claim\" && am_fold; fi\n\
         if [ -f \"$f.claim\" ]; then printf '{stuck} %s\\n' \"$(wc -c < \"$f.claim\" | tr -d ' ')\"; fi\n\
         if [ -f \"$f.replaying\" ]; then cat \"$f.replaying\"; fi\n\
         sd=\"$d/hook-spool.d\"\n\
         rd=\"$d/hook-spool.replaying\"\n\
         mkdir -p \"$rd\" 2>/dev/null || printf '{stuck} %s\\n' 0\n\
         if [ -d \"$sd\" ]; then\n\
         find \"$sd\" -type f -name '.tmp.*' -mtime +0 -exec rm -f {{}} \\; 2>/dev/null\n\
         for g in \"$sd\"/*.json; do\n\
         [ -f \"$g\" ] || continue\n\
         mv \"$g\" \"$rd/\" || printf '{stuck} %s\\n' \"$(wc -c < \"$g\" | tr -d ' ')\";\n\
         done\n\
         fi\n\
         if [ -d \"$rd\" ]; then\n\
         for g in \"$rd\"/*.json; do\n\
         [ -f \"$g\" ] || continue\n\
         cat \"$g\"\n\
         done\n\
         fi\n\
         s=\"$d/hook-status.json\"\n\
         if [ -f \"$s\" ]; then printf '\\n{marker}\\n'; cat \"$s\"; rm -f \"$s\"; fi\n",
        marker = STATUS_MARKER,
        stuck = FOLD_STUCK_MARKER,
    ))
}

/// 第二趟：本機已經把那些行 commit 進 `hook_events` 了，這時候才准刪遠端那份。
fn ack_script(bot_id: &str, root: &str) -> Result<String> {
    let dir = bots_dir(bot_id, root)?;
    Ok(format!(
        "d={dir}\n\
         rm -f \"$d/hook-spool.jsonl.replaying\"\n\
         if [ -d \"$d/hook-spool.replaying\" ]; then\n\
         for g in \"$d/hook-spool.replaying\"/*.json; do\n\
         [ -f \"$g\" ] || continue\n\
         rm -f \"$g\"\n\
         done\n\
         fi\n"
    ))
}

fn status_body(bot_id: &str, raw: &str) -> Option<HookBody> {
    let mut payload: Value = serde_json::from_str(raw).ok()?;
    // hook.sh already stamps it; a hand-written or older file may not.
    if let Some(o) = payload.as_object_mut() {
        o.insert("hook_event_name".into(), json!("StatusLine"));
    } else {
        return None;
    }
    Some(HookBody {
        bot_id: bot_id.to_string(),
        provider: "claude".into(),
        payload,
        received_at: Some(db::now()),
        truncated: false,
        run_id: None,
    })
}

#[derive(Default)]
pub struct DrainGate {
    last: Option<std::time::Instant>,
    again: bool,
}

pub fn drain_gates() -> &'static std::sync::Mutex<std::collections::HashMap<String, DrainGate>> {
    static G: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, DrainGate>>> =
        std::sync::OnceLock::new();
    G.get_or_init(Default::default)
}

/// `true` when this trigger owns the next ssh; `false` when it was merged into the drain that
/// is still inside the window (which then runs once more on its way out).
pub fn gate_admit(g: &mut DrainGate, now: std::time::Instant) -> bool {
    if let Some(last) = g.last {
        if now.duration_since(last) < DRAIN_WINDOW {
            g.again = true;
            return false;
        }
    }
    g.last = Some(now);
    g.again = false;
    true
}

/// 閘門只在 [`DRAIN_WINDOW`] 內有意義；key 是 bot id，每顆遠端 bot（含 child）一格，只記不清的話只增不減。
/// 過了這麼久沒再用、也沒有欠著的補跑，就把格子帶走（下次來是全新的格子，照樣放行）。
const DRAIN_GATE_KEEP: std::time::Duration = std::time::Duration::from_secs(600);

pub fn prune_gates(g: &mut std::collections::HashMap<String, DrainGate>, now: std::time::Instant) {
    g.retain(|_, e| e.again || e.last.is_some_and(|t| now.duration_since(t) < DRAIN_GATE_KEEP));
}

pub fn gate_take_again(g: &mut DrainGate) -> bool {
    std::mem::take(&mut g.again)
}

/// 掃遠端還有誰欠著 spool。根目錄跟著實例走（`HostInstance::instance`）。
pub fn scan_script(root: &str) -> String {
    format!(
        "am_pending() {{\n\
         [ -f \"$1hook-spool.jsonl\" ] && return 0\n\
         [ -f \"$1hook-spool.jsonl.replaying\" ] && return 0\n\
         [ -f \"$1hook-status.json\" ] && return 0\n\
         for g in \"$1hook-spool.d\"/*.json \"$1hook-spool.replaying\"/*.json; do\n\
         [ -f \"$g\" ] && return 0\n\
         done\n\
         return 1\n\
         }}\n\
         for d in \"$HOME/{root}/bots\"/*/; do\n\
         [ -d \"$d\" ] || continue\n\
         b=$(basename \"$d\")\n\
         if am_pending \"$d\"; then echo \"$b\"; fi\n\
         done\n",
    )
}

/// 把 `src` 併到 `dst` 尾端並刪掉 `src`（`dst` 不存在就直接 rename）。兩邊都是已經摘下來的副本，
/// 沒有別人會往裡面寫，所以這裡的 read／write／remove 沒有 #493 的窗口。
///
/// 位元組層合併，不經 UTF-8：崩在半個多位元組字元上的 `.replaying` 以前 `read_to_string` 失敗、
/// `unwrap_or_default` 成空字串，接著整份被新的 spool 覆蓋——舊事件就此消失（#302）。
fn fold_spool(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write as _;
    if !src.exists() {
        return Ok(());
    }
    if !dst.exists() {
        return std::fs::rename(src, dst);
    }
    // 追加寫入，不重寫 `dst`（#912）：以前 read(dst)→write(整份) 是先截斷再寫，崩在寫入途中就清空了這份唯一的副本。
    // 先讀 `src`：讀不到（壞檔、是目錄）就在碰 `dst` 之前回錯。
    let data = std::fs::read(src)?;
    let mut f = std::fs::OpenOptions::new().read(true).append(true).create(true).open(dst)?;
    // 崩在一行寫到一半時尾巴沒有換行：直接接上去，會跟下一份的第一行黏成一行、兩則一起解不開。
    let len = f.metadata()?.len();
    let mut buf = Vec::with_capacity(data.len() + 1);
    if len > 0 {
        use std::os::unix::fs::FileExt as _;
        let mut last = [0u8; 1];
        f.read_exact_at(&mut last, len - 1)?;
        if last[0] != b'\n' {
            buf.push(b'\n');
        }
    }
    buf.extend(data);
    // 寫完、sync 完才刪 `src`；中途崩掉的話兩份都在，重複與半行交給 `dedupe_key` 與 unparseable spool line 的既有保護。
    f.write_all(&buf)?;
    f.sync_all()?;
    std::fs::remove_file(src)
}

/// SPEC §4.4.6.
pub async fn replay_spool<H: HookHost>(app: &H, bot_id: &str) -> Result<usize> {
    // 讀不到 host 不等於本機（#243）：退回 local 會去讀本機 spool、遠端那份留著沒人排。回錯讓呼叫端重試。
    let host = db::bot_host(app.db(), bot_id).await?;
    if host != crate::config::LOCAL_HOST {
        return drain_remote(app, &host, bot_id).await;
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let dir = app.hook_bot_dir(bot_id)?;
    let spool = dir.join("hook-spool.jsonl");
    let claim = dir.join("hook-spool.jsonl.claim");
    let staging = dir.join("hook-spool.jsonl.replaying");
    // 上一輪崩在「收了一半」留下的 `.replaying`／`.claim` 就是唯一的副本：沒有新的 spool 也要處理它（#302）。
    // 以前只看 spool 在不在，兩邊都沒新事件時它就一直躺在那裡，直到下一則 hook 失敗寫進 spool 才被併回來。
    if !spool.exists() && !claim.exists() && !staging.exists() {
        return Ok(0);
    }
    // 上一輪摘下來、還沒併進 `.replaying` 就崩掉的。
    fold_spool(&claim, &staging)?;
    if spool.exists() {
        // #493：**先 rename 再讀**。以前是 `read(spool)` → 整份寫回 `.replaying` → `remove_file(spool)`，
        // 中間那一大段（寫一份完整的 staging）裡 hook 附加進來的行，會在最後那個 remove 被連檔刪掉。
        // rename 是原子的：之後 append 的行落在新的 inode 上，不在這一輪手上，也就刪不到。
        std::fs::rename(&spool, &claim)?;
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("spool_claimed", bot_id).await;
        fold_spool(&claim, &staging)?;
    }
    if !staging.exists() {
        return Ok(0);
    }
    let text = String::from_utf8_lossy(&std::fs::read(&staging)?).into_owned();
    let mut n = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<HookBody>(line) {
            Ok(b) => {
                if b.bot_id != bot_id {
                    tracing::warn!("spool line for a different bot; skipped");
                    continue;
                }
                // 同遠端：收進收件匣、commit 成功才算數。失敗就整個放棄這一輪，`.replaying`
                // 留在檔案系統上（上面那段會把它併回來），下一次重放再讀一次；dedupe 擋重複。
                crate::hook_inbox::accept(app.db(), &b, crate::hook_inbox::Source::Spool).await?;
                n += 1;
            }
            Err(_) => tracing::warn!(record = %spool_line_log_context(line), "unparseable spool line"),
        }
    }
    // 只有在上面每一行都 commit 進 hook_events 之後，才刪掉這份唯一的副本。
    std::fs::remove_file(&staging).ok();
    if n > 0 {
        app.wake_hook_inbox();
        tracing::info!(bot_id, accepted = n, "hook spool accepted into the inbox");
    }
    Ok(n)
}


#[cfg(all(test, feature = "daemon-test-harness"))]
mod drain_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn spool_lines_and_the_status_slot_come_apart() {
        let out = "{\"bot_id\":\"b\"}\n{\"bot_id\":\"b\",\"provider\":\"claude\"}\n\n---AM-STATUS---\n{\n  \"session_id\": \"s\"\n}\n";
        let d = parse_drain_output(out);
        assert_eq!(d.lines.len(), 2);
        assert_eq!(d.status.as_deref(), Some("{\n  \"session_id\": \"s\"\n}"));
        assert!(d.stuck_bytes.is_none());
    }

    #[test]
    fn output_without_the_marker_is_all_spool() {
        let d = parse_drain_output("{\"bot_id\":\"b\"}\n");
        assert_eq!(d.lines, vec!["{\"bot_id\":\"b\"}"]);
        assert!(d.status.is_none());
        let d = parse_drain_output("");
        assert!(d.lines.is_empty() && d.status.is_none());
    }

    #[test]
    fn an_empty_status_slot_is_no_status() {
        assert!(parse_drain_output("---AM-STATUS---\n\n").status.is_none());
    }

    #[tokio::test]
    async fn remote_spool_scanner_exits_on_shutdown_instead_of_waiting_for_its_next_poll() {
        let env = crate::testing::env().await;
        spawn_spool_scanner(env.app.clone());
        env.app.shutdown.cancel();
        env.app.background_tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(1), env.app.background_tasks.wait())
            .await
            .expect("scanner should join promptly on daemon shutdown");
    }

    #[tokio::test]
    async fn remote_spool_retry_joins_on_shutdown_while_waiting_to_retry() {
        let env = crate::testing::env().await;
        sqlx::query("DROP TABLE bots").execute(&env.app.db).await.unwrap();
        replay_host(&env.app, "remote").await;
        assert_eq!(env.app.background_tasks.len(), 1, "the failed pass should schedule one retry task");

        env.app.shutdown.cancel();
        env.app.background_tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(1), env.app.background_tasks.wait())
            .await
            .expect("spool retry must leave its delay on shutdown");
    }

    #[test]
    fn the_status_slot_becomes_a_claude_statusline_hook() {
        let b = status_body("bot1", "{\"session_id\":\"s\"}").expect("body");
        assert_eq!(b.provider, "claude");
        assert_eq!(b.bot_id, "bot1");
        assert!(matches!(classify(&b.provider, &b.payload), HookKind::StatusLine));
        assert!(status_body("bot1", "not json").is_none());
        // A JSON scalar is not a payload we can stamp.
        assert!(status_body("bot1", "3").is_none());
    }

    /// §11.4.4.
    #[test]
    fn a_second_trigger_inside_the_window_is_merged() {
        let mut g = DrainGate::default();
        let t0 = Instant::now();
        assert!(gate_admit(&mut g, t0));
        assert!(!gate_admit(&mut g, t0 + Duration::from_millis(200)));
        assert!(!gate_admit(&mut g, t0 + Duration::from_millis(900)));
        assert!(gate_take_again(&mut g));
        assert!(!gate_take_again(&mut g), "the flag is consumed once");
        // Past the window the next trigger opens its own ssh again.
        assert!(gate_admit(&mut g, t0 + DRAIN_WINDOW + Duration::from_millis(1)));
        assert!(!g.again);
    }

    #[test]
    fn the_claim_script_takes_the_spool_and_the_status_slot() {
        let s = claim_script("botX", crate::startup::REMOTE_ROOT).unwrap();
        assert!(s.contains("bots/\"'botX'"));
        // 遠端根目錄跟著實例走：兩顆 daemon 管同一台遠端時 spool 不能共用。
        assert!(s.contains("\"$HOME/.config/agents-manager/bots/\""), "正式實例路徑不變：{s}");
        let iso = crate::startup::remote_root_for(Some("a1b2"));
        assert!(claim_script("botX", &iso).unwrap().contains("\"$HOME/.config/agents-manager/instances/a1b2/bots/\""));
        assert!(scan_script(crate::startup::REMOTE_ROOT).contains("\"$HOME/.config/agents-manager/bots\"/*/"));
        assert!(scan_script(&iso).contains("\"$HOME/.config/agents-manager/instances/a1b2/bots\"/*/"));
        assert_eq!(crate::startup::remote_root_for(None), crate::startup::REMOTE_ROOT);
        assert_eq!(crate::startup::remote_root_for(Some("a1b2")), ".config/agents-manager/instances/a1b2");
        // #493：摘下來走 rename，不是 `cat` 完再刪 spool。
        assert!(s.contains("mv \"$f\" \"$f.claim\""), "要先 rename 再讀：{s}");
        assert!(!s.contains("rm -f \"$f\";"), "claim 階段不准刪 live spool：{s}");
        assert!(s.contains("hook-status.json"));
        assert!(s.contains(STATUS_MARKER));
    }

    /// issue #70 的核心不變式，釘在腳本這一層：**claim 不准刪 spool**。
    /// 遠端那份是唯一的副本，刪它的唯一時機是本機 commit 之後（`ack_script`）。
    #[test]
    fn the_claim_script_never_deletes_the_spool_it_just_read() {
        let s = claim_script("botX", crate::startup::REMOTE_ROOT).unwrap();
        assert!(s.contains("cat \"$f.replaying\""), "要讀出來：{s}");
        assert!(!s.contains("rm -f \"$f.replaying\""), "claim 階段不可以刪 .replaying：{s}");
        // 單槽的 status 是例外（最新的贏，不是佇列），照舊讀完就刪。
        assert!(s.contains("rm -f \"$s\""), "status 仍是讀完就刪：{s}");

        let ack = ack_script("botX", crate::startup::REMOTE_ROOT).unwrap();
        assert!(ack.contains("rm -f \"$d/hook-spool.jsonl.replaying\""), "ack 才刪：{ack}");
        assert!(!ack.contains("cat "), "ack 不再讀任何東西：{ack}");
    }

    #[test]
    fn the_drain_scripts_reject_unsafe_ids() {
        for id in ["../..", "x/y", r"..\..", "", "x\";id"] {
            assert!(claim_script(id, crate::startup::REMOTE_ROOT).is_err(), "unsafe id was accepted: {id:?}");
            assert!(ack_script(id, crate::startup::REMOTE_ROOT).is_err(), "unsafe id was accepted: {id:?}");
        }
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod external_claim_tests {
    use super::*;
    use crate::testing as tt;
    use std::time::Duration;

    #[test]
    fn hook_user_dedups_against_the_scraped_echo() {
        let echo = vec!["Reply with exactly MERGED-OK".to_string()];
        assert!(!hook_user_is_new(&echo, "Reply with exactly MERGED-OK"));
        // The pane wrapped the echo across two columns; the hook sends one line.
        assert!(!hook_user_is_new(&vec!["Reply with\n  exactly MERGED-OK".into()], "Reply with exactly MERGED-OK"));
        // The pane clipped the echo at the column width.
        assert!(!hook_user_is_new(&vec!["Reply with exactly MER".into()], "Reply with exactly MERGED-OK"));
    }

    #[test]
    fn hook_prompt_identity_keeps_word_boundaries_and_rejects_weak_overlap() {
        let separated = "echo a b";
        let joined = "echo ab";
        assert!(answers_another_prompt(Some(separated), Some(joined)));
        assert!(answers_another_prompt(Some(joined), Some(separated)));
        assert!(hook_user_is_new(&[separated.into()], joined));
        assert!(hook_user_is_new(&[joined.into()], separated));

        // A short common prefix is not enough evidence to assign a hook to a prompt.
        assert!(answers_another_prompt(Some("echo a much longer instruction"), Some("echo a")));
        assert!(hook_user_is_new(&["echo a much longer instruction".into()], "echo a"));
    }

    #[test]
    fn hook_prompt_identity_accepts_wrapping_and_repeated_whitespace() {
        assert!(!answers_another_prompt(
            Some("Reply with exactly MERGED-OK"),
            Some("Reply with\n  exactly   MERGED-OK")
        ));
        assert!(!hook_user_is_new(
            &["Reply with\n  exactly   MERGED-OK".into()],
            "Reply with exactly MERGED-OK"
        ));
        // The existing clipped echo remains attributable when enough prompt text survived.
        assert!(!answers_another_prompt(
            Some("Reply with exactly MERGED-OK"),
            Some("Reply with exactly MER")
        ));
    }

    /// #218：claude Stop 沒帶使用者訊息，從 transcript 尾巴補。CLI 把貼上的 prompt 包成 `<pasted_content id=…>`
    /// （真 transcript，2026-09-19 實測）：要拆回原文——不然記成外部回合時標籤會顯示給使用者，`agent_relay::claim` 也對不上。
    #[test]
    fn the_transcript_user_text_drops_the_cli_pasted_content_wrapper() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-hookrecv-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let log: Vec<&str> = include_str!("lifecycle/fixtures/claude_2.1.278_pasted_content.jsonl").lines().collect();
        std::fs::write(&path, log[..2].join("\n") + "\n").unwrap();
        assert_eq!(last_transcript_user_text(&path).as_deref(), Some("請只回覆 OK 兩個字母，不要多說任何其他的話，也不要使用任何工具。"));
        std::fs::write(&path, log[8..].join("\n") + "\n").unwrap();
        assert_eq!(
            last_transcript_user_text(&path).as_deref(),
            Some("這段文字裡有字面的 <pasted_content id=\"1234\">x</pasted_content id=\"1234\"> 標籤，請只回覆 OK 兩個字母。"),
            "CLI 跳脫的字面標籤還原"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_user_is_stored_when_it_is_not_the_echo() {
        assert!(hook_user_is_new(&[], "Reply with exactly MERGED-OK"));
        assert!(hook_user_is_new(&vec!["echo 1".into()], "echo 2"));
        // Nothing to store.
        assert!(!hook_user_is_new(&[], "   "));
    }

    /// `db::open` needs a path; the directory removes itself on drop.
    struct TmpDb(std::path::PathBuf);
    impl Drop for TmpDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn fixture() -> (TmpDb, sqlx::SqlitePool, String, String) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-hookrecv-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::app_ports_p1::open(&dir.join("t.db")).await.unwrap();
        let now = db::now();
        for q in [
            "INSERT INTO projects (id,path,label,created_at) VALUES ('p','/tmp/p','p',?)",
            "INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('b','p','b','claude','tok',?)",
            "INSERT INTO conversations (id,bot_id,created_at) VALUES ('c','b',?)",
            "INSERT INTO runs (id,bot_id,state,agent_status,pane_id,started_at) VALUES ('r','b','running','working','%1',?)",
        ] {
            sqlx::query(q).bind(&now).execute(&pool).await.unwrap();
        }
        (TmpDb(dir), pool, "r".to_string(), "c".to_string())
    }

    /// 2026-10-02 wits-ops-web：遠端讀不到 transcript，晚到的 hook 拿不出使用者訊息當證據。改看它在那台觸發的時間：
    /// 落在備援回合開始之後、中間沒開過別的回合，才算同一回合。
    #[tokio::test]
    async fn a_remote_late_hook_is_matched_by_when_it_fired() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "remote").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let mk = |at: &'static str| {
            let app = app.clone();
            let conv = conv.clone();
            async move {
                let id = db::ulid();
                sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'external','in_flight','ok',?)")
                    .bind(&id).bind(&conv).bind(at).execute(&app.db).await.unwrap();
                sqlx::query("UPDATE turns SET status='completed_fallback', completed_at=? WHERE id=?").bind(at).bind(&id).execute(&app.db).await.unwrap();
                sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&id).fetch_one(&app.db).await.unwrap()
            }
        };
        let t = mk("2026-10-01T16:54:43.000Z").await;
        assert!(fired_within(&app, &t, Some("2026-10-01T17:05:39Z")).await.unwrap(), "回合開始後觸發、中間沒別的回合");
        assert!(!fired_within(&app, &t, Some("2026-10-01T16:50:00Z")).await.unwrap(), "比回合還早觸發：是上一回合的");
        assert!(!fired_within(&app, &t, None).await.unwrap(), "沒有時間＝沒有證據");
        let _later = mk("2026-10-01T17:00:00.000Z").await;
        assert!(!fired_within(&app, &t, Some("2026-10-01T17:05:39Z")).await.unwrap(), "中間開過別的回合：不能確定是哪一回合的");
    }

    /// 2026-09-29（遠端 claude）：hook 走 spool 晚 25 秒到，備援已經存了只有最後一段、還帶狀態列的回覆。
    /// 有證據是同一回合就用 hook 的原文原地蓋掉（id 不變）；已經有 hook 寫的回覆、或沒有證據時不動。
    #[tokio::test]
    async fn a_late_hook_with_evidence_replaces_the_fallback_reply_in_place() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'late-replace','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web','completed_fallback','ok',?,?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let fb = lifecycle::insert_message(&app, &conv, Some(&turn_id), "assistant", "最後一段\n\n✻ Crunched for 40s · done 17:02", "terminal_fallback", true, None)
            .await
            .unwrap();
        let turn = || async {
            sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap()
        };

        fill_or_drop_late_hook(&app, &turn().await, "hook 原文", &Some("s1".into()), &Some("n1".into()), false).await.unwrap();
        let (content, source): (String, String) =
            sqlx::query_as("SELECT content, source FROM messages WHERE id=?").bind(&fb.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(source, "terminal_fallback", "沒有證據不蓋：{content}");

        fill_or_drop_late_hook(&app, &turn().await, "第一段\n\n最後一段", &Some("s1".into()), &Some("n1".into()), true).await.unwrap();
        let (content, source, incomplete): (String, String, i64) =
            sqlx::query_as("SELECT content, source, incomplete FROM messages WHERE id=?").bind(&fb.id).fetch_one(&app.db).await.unwrap();
        assert_eq!((content.as_str(), source.as_str(), incomplete), ("第一段\n\n最後一段", "hook", 0), "同一個 id 換成 hook 原文");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1, "不會變成兩則");
        assert_eq!(turn().await.status, "completed");

        // 已經是 hook 寫的（回合也不再是 completed_fallback）：再來一份不動。
        fill_or_drop_late_hook(&app, &turn().await, "又一份", &Some("s1".into()), &Some("n1".into()), true).await.unwrap();
        let content: String = sqlx::query_scalar("SELECT content FROM messages WHERE id=?").bind(&fb.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(content, "第一段\n\n最後一段");
    }

    #[test]
    fn idle_drain_gates_are_dropped() {
        let t0 = std::time::Instant::now();
        let mut g = std::collections::HashMap::new();
        g.insert("old".to_string(), DrainGate { last: Some(t0), again: false });
        g.insert("recent".to_string(), DrainGate { last: Some(t0 + DRAIN_GATE_KEEP), again: false });
        g.insert("owes".to_string(), DrainGate { last: Some(t0), again: true });
        prune_gates(&mut g, t0 + DRAIN_GATE_KEEP + std::time::Duration::from_secs(1));
        assert!(!g.contains_key("old"), "久沒用又沒欠補跑：帶走");
        assert!(g.contains_key("recent") && g.contains_key("owes"));
    }

    async fn late_hook_fixture(env: &tt::Env, name: &str, status: &str) -> (String, String) {
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(name)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&env.app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at) VALUES (?,?,'web',?,'ok',?,?)")
            .bind(&turn_id)
            .bind(&conv)
            .bind(status)
            .bind(db::now())
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        (conv, turn_id)
    }

    async fn native_ids(app: &Arc<App>, turn_id: &str) -> (Option<String>, Option<String>) {
        sqlx::query_as("SELECT native_session_id, native_turn_id FROM turns WHERE id=?").bind(turn_id).fetch_one(&app.db).await.unwrap()
    }

    /// 晚到的 hook 手上的回合快照是舊的：回合已經被別的路收成 `completed`、蓋好去重鑰匙（session, turn）。
    /// 補回覆之前那句「寫 native id」沒有狀態保護，會把別人的鑰匙蓋掉——該回合 hook 的重送就不再被去重擋住。
    #[tokio::test]
    async fn a_late_hook_never_overwrites_the_dedup_key_of_a_turn_already_closed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (_conv, turn_id) = late_hook_fixture(&env, "late-keys", "completed_fallback").await;
        let stale = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        // 快照之後，另一則 hook 把它收成 completed、蓋上自己的鑰匙。
        sqlx::query("UPDATE turns SET status='completed', native_session_id='s0', native_turn_id='n0' WHERE id=?").bind(&turn_id).execute(&app.db).await.unwrap();

        fill_or_drop_late_hook(&app, &stale, "晚到的另一則", &Some("s1".into()), &Some("n1".into()), true).await.unwrap();
        assert_eq!(native_ids(&app, &turn_id).await, (Some("s0".into()), Some("n0".into())), "已經收好的回合的去重鑰匙不能被蓋掉");
    }

    /// 沒有證據證明這則 hook 屬於這一回合（`same_turn = false`）而且丟掉它：它的 native id 不能記到這一回合上——
    /// 記上去之後 `recent_fallback_turn`（只找 native id 為空的）就再也找不到這一回合，它真正的晚到 hook 沒地方收。
    #[tokio::test]
    async fn a_dropped_late_hook_that_may_belong_elsewhere_does_not_stamp_its_ids_on_the_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (conv, turn_id) = late_hook_fixture(&env, "late-foreign", "completed_fallback").await;
        lifecycle::insert_message(&app, &conv, Some(&turn_id), "assistant", "備援抓的回覆", "terminal_fallback", true, None).await.unwrap();
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();

        fill_or_drop_late_hook(&app, &turn, "別回合的回覆", &Some("s-other".into()), &Some("n-other".into()), false).await.unwrap();
        assert_eq!(native_ids(&app, &turn_id).await, (None, None), "丟掉的 hook 不留鑰匙");
        let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(replies, 1);
    }

    /// 2026-09-13（GROK）：備援關掉的回合沒存回覆，遲到 hook 的答案被丟、使用者看到「沒回應」。
    #[tokio::test]
    async fn a_late_hook_fills_a_fallback_turn_that_has_no_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'late-hook','grok','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web','completed_fallback','ok',?,?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();

        fill_or_drop_late_hook(&app, &turn, "側欄那組徽章已收齊，cdcf165 已推", &Some("s1".into()), &Some("n1".into()), false)
            .await
            .unwrap();

        let (status, native): (String, Option<String>) =
            sqlx::query_as("SELECT status, native_turn_id FROM turns WHERE id=?")
                .bind(&turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(status, "completed", "有 hook 的證據就不再是「只看到畫面」");
        assert_eq!(native.as_deref(), Some("n1"), "native id 照舊蓋上去，重送才去得掉重");
        let replies: Vec<String> =
            sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
                .bind(&turn_id)
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(replies, vec!["側欄那組徽章已收齊，cdcf165 已推".to_string()]);

        // 已有回覆的照舊丟，防一回合兩則。
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        fill_or_drop_late_hook(&app, &turn, "第二份回覆", &Some("s1".into()), &Some("n1".into()), false).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1, "不會變成兩則");
    }

    /// #831：遲到的 hook 數完回合上的回覆、還沒寫的那一瞬，一個不相干的 writer commit 了一筆。deferred 交易這時升級寫鎖
    /// 直接 517，hook 的回覆補不上；寫鎖從讀之前就拿著，插進來的那一筆等，回覆補上一次、回合升 `completed`。
    #[tokio::test]
    async fn an_unrelated_writer_between_the_reply_count_and_the_fill_does_not_lose_the_late_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "late-hook-831").await;
        let conversation_id = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web','completed_fallback','ok',?,?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        let other = tt::arm_app_foreign_writer(&app, "late_hook_after_reply_count", &turn_id);

        fill_or_drop_late_hook(&app, &turn, "late reply", &Some("s1".into()), &Some("n1".into()), false)
            .await
            .expect("an unrelated writer must not make the late hook fail");

        assert_eq!(*other.lock().unwrap(), Some(false), "the fill holds the write lock from its read on; the other writer waits");
        assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
        let replies: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, vec!["late reply".to_string()]);
    }

    #[tokio::test]
    async fn unreadable_late_hook_message_owner_rolls_back_until_retry() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'late-hook-owner-retry','grok','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web','completed_fallback','ok',?,?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        let mut events = app.subscribe();

        tt::make_table_unreadable(&app, "conversations").await;
        let first = fill_or_drop_late_hook(&app, &turn, "late reply", &Some("s1".into()), &Some("n1".into()), false).await;
        tt::make_table_readable(&app, "conversations").await;

        assert!(first.is_err(), "an unreadable owner must return a retryable error: {first:?}");
        let mut failed_message_events = Vec::new();
        while let Ok(event) = events.try_recv() {
            if event.kind == "message_added" {
                failed_message_events.push(event);
            }
        }
        assert!(failed_message_events.is_empty(), "the failed attempt must not publish a message with an unknown owner");
        let (status, native_turn_id): (String, Option<String>) =
            sqlx::query_as("SELECT status, native_turn_id FROM turns WHERE id=?")
                .bind(&turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(status, "completed_fallback", "failed owner lookup rolls back the status transition");
        assert_eq!(native_turn_id, None, "failed owner lookup rolls back native ids too");
        let failed_replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(failed_replies, 0, "failed owner lookup cannot leave a durable reply without its event");

        fill_or_drop_late_hook(&app, &turn, "late reply", &Some("s1".into()), &Some("n1".into()), false).await.unwrap();
        let message_events: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| event.kind == "message_added")
            .collect();
        assert_eq!(message_events.len(), 1, "the replay emits exactly one message_added event");
        assert_eq!(message_events[0].data["bot_id"], bot_id);
        assert_eq!(message_events[0].data["message"]["turn_id"], turn_id);
    }

    /// 空的 hook 不能把回合改成 completed——那等於宣稱有答案。
    #[tokio::test]
    async fn an_empty_late_hook_changes_nothing_but_the_native_ids() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'late-hook-empty','grok','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at, completed_at)
             VALUES (?,?,'web','completed_fallback','ok',?,?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        fill_or_drop_late_hook(&app, &turn, "   ", &Some("s2".into()), &Some("n2".into()), false).await.unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(status, "completed_fallback");
    }

    /// 2026-09-12 使用者實機：折行讓刮下來的訊息斷在一半，既有那則是原文前綴時要補完。
    #[tokio::test]
    async fn a_clipped_scraped_prompt_is_upgraded_to_the_hooks_full_text() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'clipped-echo','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at)
             VALUES (?,?,'external','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let clipped = "請問我兩題，第二題 header『功能』請設 multiSelect:";
        let full = "請問我兩題，第二題 header『功能』請設 multiSelect: true，四個選項。問完就停著等我回答。";
        crate::lifecycle::insert_message(&app, &conv, Some(&turn_id), "user", clipped, "terminal_fallback", false, None)
            .await
            .unwrap();

        let upgraded = upgrade_clipped_user_message(&mut app.db.acquire().await.unwrap(), &turn_id, full).await.unwrap();
        let upgraded = upgraded.expect("補完了一則");
        assert_eq!((upgraded.content.as_str(), upgraded.source.as_str()), (full, "hook"));

        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT content, source FROM messages WHERE turn_id=? AND role='user'")
                .bind(&turn_id)
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1, "補完，不是多開一則");
        assert_eq!(rows[0].0, full);
        assert_eq!(rows[0].1, "hook");

        // 不是前綴的就別動：那是另一句話。
        let none = upgrade_clipped_user_message(&mut app.db.acquire().await.unwrap(), &turn_id, "完全不同的一句").await.unwrap();
        assert!(none.is_none(), "不是前綴就不補");
        let after: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='user'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(after, full);
    }

    /// #1006：外部回合的回音被 hook 原文補完後，同一則要再推一次（同 id），網頁才不會停在折行截斷的那一句。
    #[tokio::test]
    async fn a_clipped_prompt_upgrade_is_pushed_to_the_web() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'clipped-push','codex','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','working','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'external','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let full = "請問我兩題，第二題 header『功能』請設 multiSelect: true，四個選項。問完就停著等我回答。";
        let clipped = &full[..full.char_indices().nth(12).unwrap().0];
        let echo = crate::lifecycle::insert_message(&app, &conv, Some(&turn_id), "user", clipped, "terminal_fallback", false, None)
            .await
            .unwrap();

        let mut events = app.subscribe();
        process(&app, &codex_done(&bot_id, full)).await.unwrap();

        let pushed: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| e.kind == "message_added")
            .filter(|e| e.data["message"]["id"] == echo.id)
            .collect();
        assert_eq!(pushed.len(), 1, "被補完的那一則要再推一次");
        assert_eq!(pushed[0].data["message"]["content"], full);
    }

    async fn unknown_turn(app: &Arc<App>, project_id: &str, kind: &str, prompt: &str) -> (String, String, String) {
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(project_id)
        .bind(format!("hook-{}", &bot_id[bot_id.len() - 6..]))
        .bind(kind)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','working','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,?,'web','in_flight','unknown',?,?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(prompt)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        (bot_id, conversation_id, turn_id)
    }

    fn codex_done(bot_id: &str, user: &str) -> HookBody {
        HookBody {
            bot_id: bot_id.to_string(),
            provider: "codex".into(),
            payload: json!({
                "type": "agent-turn-complete",
                "thread-id": format!("thread-{bot_id}"),
                "turn-id": format!("turn-{}", db::ulid()),
                "input-messages": [user],
                "last-assistant-message": "hook reply",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        }
    }

    /// 第二輪 review 送達線 #3：`unknown` 期間使用者在終端手打另一句，答案不能掛到原本那則、也不能把它標成已送達。
    #[tokio::test]
    async fn a_hook_answering_another_prompt_does_not_claim_the_unknown_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "codex", "echo a b").await;

        process(&app, &codex_done(&bot_id, "echo ab")).await.unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("in_flight", "unknown"), "原本那則原封不動");
        let external: Vec<(String, String)> = sqlx::query_as(
            "SELECT t.id, m.content FROM turns t JOIN messages m ON m.turn_id=t.id
              WHERE t.conversation_id=? AND t.origin='external' AND m.role='user'",
        )
        .bind(&conv)
        .fetch_all(&app.db)
        .await
        .unwrap();
        assert_eq!(external.len(), 1, "答案記在一筆外部回合上");
        assert_eq!(external[0].1, "echo ab");
    }

    /// 沒有任何 native id 的 claude Stop（遠端、舊版 hook、手寫 body）：回合沒有去重用的 `(session, turn)` 鑰匙。
    fn stop_without_native_ids(bot_id: &str, reply: &str) -> HookBody {
        HookBody {
            bot_id: bot_id.to_string(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "last_assistant_message": reply}),
            received_at: None,
            truncated: false,
            run_id: None,
        }
    }

    async fn turn_count(app: &Arc<App>, conv: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(conv).fetch_one(&app.db).await.unwrap()
    }

    async fn hook_message_count(app: &Arc<App>, conv: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND source='hook'").bind(conv).fetch_one(&app.db).await.unwrap()
    }

    /// 發現 2（l8 審查）：外部回合 commit 了、daemon 在收件匣標 `processed_at` 之前重啟，重播同一列不能再開第二個回合與訊息——
    /// 沒有 native turn id 時，過去沒有任何穩定的鑰匙擋它。重播只補發 commit 後才發的通知。
    #[tokio::test]
    async fn an_inbox_row_replayed_after_the_external_turn_committed_opens_no_second_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "inbox-replay").await;
        tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let body = stop_without_native_ids(&bot.id, "外部回合的回覆");
        assert!(crate::hook_inbox::accept(&app.db, &body, crate::hook_inbox::Source::Http).await.unwrap().is_new());
        let row = crate::hook_inbox::pending(&app.db, &db::now(), 10).await.unwrap().remove(0);

        // 第一輪：回合與訊息已 commit，但 worker 還沒標 processed_at 就「重啟」了。
        process_for(&app, &body, Some(&row.id)).await.unwrap();
        assert_eq!(turn_count(&app, &conv).await, 1);
        assert_eq!(hook_message_count(&app, &conv).await, 1);
        let stamped: Option<String> = sqlx::query_scalar("SELECT source_event_id FROM turns WHERE conversation_id=?").bind(&conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(stamped.as_deref(), Some(row.id.as_str()), "回合記著是哪則收件匣事件收的");
        let unprocessed: Option<String> = sqlx::query_scalar("SELECT processed_at FROM hook_events WHERE id=?").bind(&row.id).fetch_one(&app.db).await.unwrap();
        assert!(unprocessed.is_none(), "還沒標處理完：模擬重啟在 commit 與 mark_done 之間");

        // 重啟後 worker 把這列再處理一次。
        let mut events = app.subscribe();
        assert_eq!(crate::runners::hook_inbox::drain_once(&app).await.unwrap(), 1);

        assert_eq!(turn_count(&app, &conv).await, 1, "沒有第二個回合");
        assert_eq!(hook_message_count(&app, &conv).await, 1, "沒有第二則訊息");
        let done: Option<String> = sqlx::query_scalar("SELECT processed_at FROM hook_events WHERE id=?").bind(&row.id).fetch_one(&app.db).await.unwrap();
        assert!(done.is_some(), "重播後這列標成處理完");
        // commit 後才發的通知補發了一次（前端靠訊息 id 去重）。
        let mut kinds = Vec::new();
        while let Ok(ev) = events.try_recv() {
            kinds.push(ev.kind);
        }
        assert!(kinds.iter().any(|k| k == "message_added"), "補發 message_added：{kinds:?}");
    }

    /// 同一個洞的另一條路：Stop 收掉的是 in-flight 的網頁回合，沒有 native id 時重播也不能多開一個外部回合。
    #[tokio::test]
    async fn an_inbox_row_replayed_after_an_in_flight_turn_was_completed_opens_no_external_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "跑一次測試").await;
        let body = stop_without_native_ids(&bot_id, "跑完了");
        assert!(crate::hook_inbox::accept(&app.db, &body, crate::hook_inbox::Source::Http).await.unwrap().is_new());
        let row = crate::hook_inbox::pending(&app.db, &db::now(), 10).await.unwrap().remove(0);

        process_for(&app, &body, Some(&row.id)).await.unwrap();
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turn.status, "completed");
        let before = turn_count(&app, &conv).await;
        let messages_before = hook_message_count(&app, &conv).await;

        assert_eq!(crate::runners::hook_inbox::drain_once(&app).await.unwrap(), 1);

        assert_eq!(turn_count(&app, &conv).await, before, "重播不開外部回合");
        assert_eq!(hook_message_count(&app, &conv).await, messages_before, "也不多一則訊息");
    }

    /// 沒有收件匣 id（直接 `process`）時行為跟以前一樣：兩次獨立的、沒有身分的事件各是一個回合。
    #[tokio::test]
    async fn without_an_inbox_event_id_nothing_is_deduplicated_by_the_new_key() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "no-event-id").await;
        tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        process(&app, &stop_without_native_ids(&bot.id, "一")).await.unwrap();
        process(&app, &stop_without_native_ids(&bot.id, "二")).await.unwrap();
        assert_eq!(turn_count(&app, &conv).await, 2);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE source_event_id IS NOT NULL").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 0);
    }

    /// 同一句（折疊排版空白後相同）就照舊認領並把 unknown 升成 ok。
    #[tokio::test]
    async fn a_hook_answering_the_same_prompt_still_resolves_the_unknown_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "codex", "跑一次   測試").await;

        process(&app, &codex_done(&bot_id, "跑一次 測試")).await.unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("completed", "ok"));
    }

    /// issue #753：遠端 bot 的 Stop 比備援晚 0.1–0.3 秒到，備援已把畫面抓到的字存成回覆。遠端 transcript 在 agm-host 讀不到，
    /// Stop 本身又不帶使用者訊息，以前 `seen=None` → 不算同一回合 → 真回覆被丟。遠端 `hook.sh` 現在在 spool 前從本機 transcript
    /// 把最後一則使用者訊息塞進 payload（`agm_user_text`）：對得上才蓋；對不上（別句）或沒帶都不蓋。
    #[tokio::test]
    async fn a_remote_stop_that_carries_the_users_text_replaces_the_fallback_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "現在部 demo").await;
        sqlx::query("UPDATE turns SET status='completed_fallback', delivery='ok', completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        let fb = db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'assistant','Bash(herdr agent get …)','terminal_fallback',?)")
            .bind(&fb)
            .bind(&conv)
            .bind(&turn_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let stop = |sid: &str, carried: Option<&str>| {
            let mut payload = json!({
                "hook_event_name": "Stop",
                "session_id": sid,
                "prompt_id": db::ulid(),
                // m4p 的路徑：agm-host 讀不到。
                "transcript_path": "/Users/m4p/.claude/projects/x/s.jsonl",
                "last_assistant_message": "已經交給 memleak 排查，先看這幾點",
            });
            if let Some(t) = carried {
                payload["agm_user_text"] = json!(t);
            }
            HookBody { bot_id: bot_id.clone(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }
        };
        let reply = || async {
            sqlx::query_as::<_, (String, String)>("SELECT content, source FROM messages WHERE id=?").bind(&fb).fetch_one(&app.db).await.unwrap()
        };

        // 沒帶證據（舊版 hook.sh、沒有 python3）：照舊不蓋。
        process(&app, &stop("s1", None)).await.unwrap();
        assert_eq!(reply().await.1, "terminal_fallback", "沒有證據不蓋");
        assert_eq!(turn_row(&app, &turn_id).await.status, "completed_fallback");

        // 每個事件各自對上同一筆備援回合：清掉前一則記下的 native id（它是「這一回合已有 hook 來過」的去重鑰匙）。
        let forget_native = || async { sqlx::query("UPDATE turns SET native_session_id=NULL, native_turn_id=NULL WHERE id=?").bind(&turn_id).execute(&app.db).await.unwrap() };
        forget_native().await;
        // 帶的是別句：答案不是這一回合的，不蓋。
        process(&app, &stop("s2", Some("完全不同的另一句話"))).await.unwrap();
        assert_eq!(reply().await.1, "terminal_fallback", "別句不蓋");

        forget_native().await;
        // 帶的就是這一回合的 prompt：蓋成 hook 原文。
        process(&app, &stop("s3", Some("現在部 demo"))).await.unwrap();
        assert_eq!(reply().await, ("已經交給 memleak 排查，先看這幾點".to_string(), "hook".to_string()));
        assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
        let on_turn = replies(&app, &conv).await.into_iter().filter(|(t, _)| t.as_deref() == Some(turn_id.as_str())).count();
        assert_eq!(on_turn, 1, "這一回合不會變成兩則（別句那則在它自己的外部回合上）");
    }

    /// Remote transcript paths are only shape-checked because the real file lives on the remote
    /// host. A same-shaped local file must not be read as evidence for a remote bot.
    #[tokio::test]
    async fn a_remote_stop_never_reads_a_local_file_at_its_transcript_path() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "預期 prompt").await;
        sqlx::query("UPDATE projects SET host='remote-test' WHERE id=?")
            .bind(&env.project_id)
            .execute(&app.db)
            .await
            .unwrap();

        let scratch = tt::track(std::env::temp_dir().join(format!("am-remote-transcript-{}", db::ulid())));
        std::fs::create_dir_all(&scratch).unwrap();
        let local_collision = scratch.join("remote.jsonl");
        let private_text = "LOCAL-ONLY-TRANSCRIPT-9274";
        std::fs::write(
            &local_collision,
            format!("{}\n", json!({"type":"user","message":{"role":"user","content":private_text}})),
        )
        .unwrap();

        process(&app, &HookBody {
            bot_id,
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "remote-session",
                "prompt_id": "remote-turn",
                "transcript_path": local_collision.to_string_lossy(),
                "last_assistant_message": "remote reply",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        }).await.unwrap();

        let leaked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE conversation_id=? AND content LIKE '%LOCAL-ONLY-TRANSCRIPT-9274%'",
        )
        .bind(&conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(leaked, 0, "daemon host 的同路徑檔案不可以當成 remote transcript 讀取");
        let target = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((target.status.as_str(), target.delivery.as_str()), ("completed", "ok"), "看不到 remote transcript 時照原本規則認領回合");
    }

    /// Provider matching is deliberately case-insensitive; accepted casing must still dispatch
    /// the event instead of durably consuming it as an unknown provider event.
    #[tokio::test]
    async fn accepted_provider_case_variants_still_process_stop_events() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _, turn_id) = unknown_turn(&app, &env.project_id, "claude", "prompt").await;

        process(&app, &HookBody {
            bot_id,
            provider: "Claude".into(),
            payload: json!({"hook_event_name":"Stop", "session_id":"case-session", "prompt_id":"case-turn",
                "last_assistant_message":"case reply"}),
            received_at: None,
            truncated: false,
            run_id: None,
        }).await.unwrap();

        let target = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((target.status.as_str(), target.delivery.as_str()), ("completed", "ok"));
    }

    /// The hook's custom tail reader must share transcript_read's regular-file guard; a FIFO in
    /// the bot's own projects directory must not strand a spawn_blocking worker waiting for a writer.
    #[tokio::test]
    async fn a_fifo_hook_transcript_does_not_block_stop_processing() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _, _) = unknown_turn(&app, &env.project_id, "claude", "prompt").await;
        let scratch = tt::track(std::env::temp_dir().join(format!("am-hook-fifo-{}", db::ulid())));
        let path = own_projects_file(&app, &bot_id, &scratch, "blocked.jsonl").await;
        assert!(std::process::Command::new("mkfifo").arg(&path).status().unwrap().success());
        let body = HookBody {
            bot_id,
            provider: "claude".into(),
            payload: json!({"hook_event_name":"Stop", "session_id":"fifo-session", "prompt_id":"fifo-turn",
                "transcript_path":path.to_string_lossy(), "last_assistant_message":"done"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        let app_for_task = app.clone();
        let mut task = tokio::spawn(async move { process(&app_for_task, &body).await });
        let quickly_finished = tokio::time::timeout(Duration::from_secs(3), &mut task).await;
        if quickly_finished.is_err() {
            // Release a buggy blocking open before asserting, so the red test never leaks a thread.
            let fifo = path.clone();
            std::thread::spawn(move || drop(std::fs::OpenOptions::new().write(true).open(fifo).unwrap())).join().unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(3), &mut task).await;
        }
        assert!(quickly_finished.is_ok(), "FIFO path held Stop processing for over three seconds");
        quickly_finished.unwrap().unwrap().unwrap();
    }

    /// #910：transcript 尾巴最後一列是 auto-compact 的摘要（`type:"user"`、`isCompactSummary`，內容是一大段
    /// `This session is being continued…`）時，Stop 讀到的「最後一則使用者訊息」不能是它：`unknown` 的 in-flight 回合照樣被認領，
    /// 不開外部回合，對話裡也不會多一則以摘要開頭的 user 訊息。
    #[tokio::test]
    async fn a_compact_summary_at_the_transcript_tail_does_not_hide_the_prompt_from_the_stop() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "做完那份長任務").await;
        let scratch = tt::track(std::env::temp_dir().join(format!("am-hook-compact-{}", db::ulid())));
        let path = own_projects_file(&app, &bot_id, &scratch, "compact.jsonl").await;
        let rows = [
            json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": "做完那份長任務"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "做完了"}], "stop_reason": "end_turn"}}),
            json!({"type": "user", "uuid": "s1", "isCompactSummary": true, "isVisibleInTranscriptOnly": true,
                "message": {"role": "user", "content": "This session is being continued from a previous conversation that ran out of context. Summary: ..."}}),
        ];
        std::fs::write(&path, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        assert_eq!(last_transcript_user_text(&path).as_deref(), Some("做完那份長任務"), "摘要列不是使用者訊息");

        process(
            &app,
            &HookBody {
                bot_id,
                provider: "claude".into(),
                payload: json!({"hook_event_name":"Stop", "session_id":"compact-session", "prompt_id":"compact-turn",
                    "transcript_path": path.to_string_lossy(), "last_assistant_message":"做完了"}),
                received_at: None,
                truncated: false,
                run_id: None,
            },
        )
        .await
        .unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turn.status, "completed", "原本的回合被認領");
        let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND origin='external'").bind(&conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(external, 0, "沒有外部回合");
        let summaries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='user' AND content LIKE 'This session is being continued%'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(summaries, 0, "對話裡沒有摘要當 user 訊息");
    }

    /// 備援剛把回合關掉、沒存回覆（這個 run 已經沒有 in-flight 回合）。
    async fn fallback_closed_turn(app: &Arc<App>, project_id: &str, prompt: &str) -> (String, String, String) {
        let (bot_id, conv, turn_id) = unknown_turn(app, project_id, "codex", prompt).await;
        sqlx::query("UPDATE turns SET status='completed_fallback', delivery='ok', completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&turn_id)
            .execute(&app.db)
            .await
            .unwrap();
        (bot_id, conv, turn_id)
    }

    /// issue #1035：run 在主機 A 上起來（記下 A 的世代），`?confirm=repoint` 把同名主機改指到 B（世代換了），
    /// 舊 run 的 Stop 這時才到。它是上一代主機的事件：一個欄位都不准動，也要留下 `hook_fenced`。
    #[tokio::test]
    async fn a_run_started_before_a_repoint_cannot_touch_the_repointed_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        let host = format!("repoint-{}", db::ulid());
        let host_cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
        };
        app.hosts
            .insert_remote_with_client_for_test(host_cfg("target-a"), crate::herdr::HerdrClient::new(env.dir.join("a.sock")))
            .await;
        sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(&host).bind(&env.project_id).execute(&app.db).await.unwrap();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'repointed','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();

        // 起在 A 上的 run：啟動當下記下 A 的世代（start 會這樣寫，這裡直接種）。
        let gen_a = app.hosts.fence(&host).await.unwrap().generation() as i64;
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at, host, host_generation)
             VALUES (?,?,'running','working','pane-a','s-a',?,?,?)",
        )
        .bind(&run)
        .bind(&bot_id)
        .bind(db::now())
        .bind(&host)
        .bind(gen_a)
        .execute(&app.db)
        .await
        .unwrap();
        let turn = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,?,'web','in_flight','ok','改指前問的',?)",
        )
        .bind(&turn)
        .bind(&conv)
        .bind(&run)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert_eq!(db::run_host_generation(&app.db, &run).await.unwrap(), Some(gen_a));

        // 同名主機改指到 B：世代換了。
        app.hosts
            .replace_remote_with_client_for_test(&app, host_cfg("target-b"), crate::herdr::HerdrClient::new(env.dir.join("b.sock")))
            .await;

        let mut events = app.subscribe();
        process(
            &app,
            &HookBody {
                bot_id: bot_id.clone(),
                provider: "claude".into(),
                payload: json!({"hook_event_name": "Stop", "session_id": "s-a", "prompt_id": "p-a",
                                "last_assistant_message": "改指前的回覆"}),
                received_at: None,
                truncated: false,
                run_id: None,
            },
        )
        .await
        .unwrap();

        let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.status, "in_flight", "改指之前的 run 的 Stop 不准收回合");
        let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='assistant'").fetch_one(&app.db).await.unwrap();
        assert_eq!(replies, 0, "上一代主機的回覆不能貼進對話");
        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await.unwrap().unwrap();
        assert_eq!(ev.kind, "hook_fenced");
        assert_eq!(ev.data["why"], "host_generation");
    }

    /// issue #69 的 race，整條走一遍：使用者 interrupt → bot 重啟（舊 run 收掉、新 run 起來並開了新回合）
    /// → 舊 CLI session 的 Stop 這時候才到。它是上一代的，一個欄位都不准動新回合。
    #[tokio::test]
    async fn a_late_stop_from_the_previous_run_cannot_touch_the_new_runs_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'fenced','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();

        // 上一代：跑在 `s-old` 上，使用者 interrupt 之後收掉（回合標 failed），run 結束。
        let old_run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at, ended_at)
             VALUES (?,?,'exited','unknown','pane-old','s-old',?,?)",
        )
        .bind(&old_run)
        .bind(&bot_id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let old_turn = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, completed_at, created_at)
             VALUES (?,?,?,'web','failed','ok','上一代問的',?,?)",
        )
        .bind(&old_turn)
        .bind(&conv)
        .bind(&old_run)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        // 這一代：重啟起來的新 run，已經開了新回合，還在跑。世代圍籬（`lifecycle::fence`）認的是
        // **寫入順序**（rowid），不是這裡的 ULID 字典序（issue #98：兩者不保證一致，這行以前斷言
        // `new_run > old_run` 會在同一毫秒巧合下偶爾紅——跟這支測試實際要驗的東西無關，拿掉）。
        let new_run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, started_at)
             VALUES (?,?,'running','working','pane-new',?)",
        )
        .bind(&new_run)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let new_turn = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,?,'web','in_flight','ok','這一代問的',?)",
        )
        .bind(&new_turn)
        .bind(&conv)
        .bind(&new_run)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let mut events = app.subscribe();
        // 舊 session 的 Stop 現在才抵達。
        process(
            &app,
            &HookBody {
                bot_id: bot_id.clone(),
                provider: "claude".into(),
                payload: json!({"hook_event_name": "Stop", "session_id": "s-old", "prompt_id": "p-old",
                                "last_assistant_message": "上一代的回覆"}),
                received_at: None,
                truncated: false,
                run_id: None,
            },
        )
        .await
        .unwrap();

        let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&new_turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.status, "in_flight", "上一代的 Stop 不准收這一代的回合");
        assert_eq!(t.native_session_id, None, "也不准把舊 session 蓋到新回合上");
        let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='assistant'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, 0, "上一代的回覆不能貼進這一代的對話");
        let run_session: Option<String> =
            sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&new_run).fetch_one(&app.db).await.unwrap();
        assert_eq!(run_session, None, "新 run 的 session 不能被舊事件寫進去");

        // 丟掉一則事件要看得見，不是只活在函式裡。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await.unwrap().unwrap();
        assert_eq!(ev.kind, "hook_fenced");
        assert_eq!(ev.data["prior_run_id"], serde_json::json!(old_run));
        assert_eq!(ev.data["session_id"], "s-old");

        // 重播同一則：還是什麼都不改（idempotent）。
        process(
            &app,
            &HookBody {
                bot_id: bot_id.clone(),
                provider: "claude".into(),
                payload: json!({"hook_event_name": "StopFailure", "session_id": "s-old", "prompt_id": "p-old",
                                "reason": "API Error: 500"}),
                received_at: None,
                truncated: false,
                run_id: None,
            },
        )
        .await
        .unwrap();
        let t = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&new_turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.status, "in_flight", "上一代的 StopFailure 也不准把這一代標成失敗");
    }

    /// 反過來的那一半：**這一代自己**的遲到 hook 照舊生效（`4fac036` 特意保留的行為）。
    /// 圍籬只擋證明得出來是舊世代的，不是「遲到就丟」。
    #[tokio::test]
    async fn a_late_hook_from_this_same_run_still_fills_its_fallback_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次測試").await;
        // 這個 run 已經回報過自己的 session：hook 帶同一個，就是這一代自己的。
        sqlx::query("UPDATE runs SET native_session_id='s-live' WHERE bot_id=?")
            .bind(&bot_id)
            .execute(&app.db)
            .await
            .unwrap();

        let mut body = codex_done(&bot_id, "跑一次測試");
        body.payload["thread-id"] = json!("s-live");
        process(&app, &body).await.unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turn.status, "completed", "同一代的遲到 hook 照舊補得進去");
        let replies: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, vec!["hook reply".to_string()]);
    }

    /// issue #92：撞額度 → 換身分 → `--resume` 接回**同一個** session。換身分前那個行程的 `StopFailure`
    /// （撞額度那一則）這時候才到：它帶的 session 跟新 run 一模一樣，只看 session 會被認成這一代的，
    /// 把換身分之後剛送出去的那一回合收成失敗。它自己帶的 run id 才分得開。
    #[tokio::test]
    async fn a_late_hook_from_the_process_before_a_resume_cannot_touch_the_resumed_runs_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "resumed").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        // 身分 A 的那一代：session `s-same`，撞額度之後被換身分重啟收掉。
        let old_run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at, ended_at)
             VALUES (?,?,'stopped','unknown','pane-a','s-same',?,?)",
        )
        .bind(&old_run)
        .bind(&bot.id)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        // 身分 B 的這一代：`--resume s-same` 起來、SessionStart 已經對上（`native_session_id` 同一個），
        // 並且已經送出一則新的回合。
        let new_run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, native_session_id, started_at)
             VALUES (?,?,'running','working','pane-b','s-same',?)",
        )
        .bind(&new_run)
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let new_turn = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,?,'web','in_flight','ok','換身分之後問的',?)",
        )
        .bind(&new_turn)
        .bind(&conv)
        .bind(&new_run)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_now = |id: String| {
            let app = app.clone();
            async move { sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap() }
        };
        let from = |run: &str, payload: Value| HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload,
            received_at: None,
            truncated: false,
            run_id: Some(run.to_string()),
        };

        let mut events = app.subscribe();
        let limit = json!({"hook_event_name": "StopFailure", "session_id": "s-same", "prompt_id": "p-a",
                           "reason": "You've hit your usage limit · resets 5pm"});
        process(&app, &from(&old_run, limit)).await.unwrap();
        assert_eq!(turn_now(new_turn.clone()).await.status, "in_flight", "身分 A 遲到的撞額度不准收掉身分 B 的回合");
        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await.unwrap().unwrap();
        assert_eq!((ev.kind.as_str(), ev.data["prior_run_id"].as_str()), ("hook_fenced", Some(old_run.as_str())));

        let reply = json!({"hook_event_name": "Stop", "session_id": "s-same", "prompt_id": "p-a",
                           "last_assistant_message": "身分 A 那一回合的半截回覆"});
        process(&app, &from(&old_run, reply)).await.unwrap();
        let t = turn_now(new_turn.clone()).await;
        assert_eq!((t.status.as_str(), t.native_turn_id.as_deref()), ("in_flight", None), "也不准拿舊行程的 Stop 收尾");
        let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='assistant'")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, 0, "舊行程的回覆不能貼進來，也不能變成一筆外部回合");

        // 對照：同一個 session、這一代自己的行程送的——照常收（證明擋下前兩則的是 run id，不是別的條件）。
        let own = json!({"hook_event_name": "StopFailure", "session_id": "s-same", "prompt_id": "p-b",
                         "reason": "You've hit your usage limit · resets 5pm"});
        process(&app, &from(&new_run, own)).await.unwrap();
        assert_eq!(turn_now(new_turn).await.status, "failed");
    }

    /// review3 c1 L13：沒有 in-flight 回合時，遲到 hook 帶來的是**別句**的答案（使用者改到 pane 裡直接打）——
    /// 不能補進 120 秒內那筆備援回合、更不能把它標成 completed，答案記在外部回合上。
    #[tokio::test]
    async fn a_late_hook_answering_another_prompt_does_not_fill_the_fallback_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次測試").await;

        process(&app, &codex_done(&bot_id, "順便看一下 lint")).await.unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turn.status, "completed_fallback", "T1 原封不動");
        let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, 0, "別句的答案不能寫成 T1 的回覆");
        let external: Vec<String> = sqlx::query_scalar(
            "SELECT m.content FROM turns t JOIN messages m ON m.turn_id=t.id
              WHERE t.conversation_id=? AND t.origin='external' ORDER BY m.role DESC",
        )
        .bind(&conv)
        .fetch_all(&app.db)
        .await
        .unwrap();
        assert_eq!(external, vec!["順便看一下 lint".to_string(), "hook reply".to_string()], "答案跟它的問題一起記在外部回合");
    }

    /// #297：備援關掉 T1、使用者接著送 T2（已送達、in-flight），T1 的真回覆這時才到——答案屬於 T1，
    /// 不能把 T2 收成 completed 還貼上 T1 的回覆（以前只有 `delivery = unknown` 才比對使用者訊息）。
    #[tokio::test]
    async fn a_late_hook_for_the_fallback_turn_does_not_complete_the_next_in_flight_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, t1) = fallback_closed_turn(&app, &env.project_id, "第一句").await;
        let run_id: String = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&t1).fetch_one(&app.db).await.unwrap();
        let t2 = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,?,'web','in_flight','ok','第二句',?)",
        )
        .bind(&t2)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        process(&app, &codex_done(&bot_id, "第一句")).await.unwrap();

        let turn = |id: String| {
            let app = app.clone();
            async move { sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap() }
        };
        assert_eq!(turn(t2.clone()).await.status, "in_flight", "T2 還在跑，T1 的回覆不能收掉它");
        let on_t2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&t2)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(on_t2, 0, "T1 的回覆不能貼在 T2 上");
        assert_eq!(turn(t1).await.status, "completed", "回覆補回它真正的回合");
    }

    /// 同一句就照舊補進去（2026-09-13 GROK 那種）。
    #[tokio::test]
    async fn a_late_hook_answering_the_same_prompt_still_fills_the_fallback_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次   測試").await;

        process(&app, &codex_done(&bot_id, "跑一次 測試")).await.unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turn.status, "completed");
        let replies: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, vec!["hook reply".to_string()]);
    }

    /// review3 c1 M3：交辦先帶著「沒有回覆」結算（備援關掉回合），遲到 hook 補上回覆之後，回覆要寫回交辦並通知驗收者。
    #[tokio::test]
    async fn a_late_hook_reply_reaches_the_assignment_it_answers() {
        use crate::supervisor::{controller, store};
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次測試").await;
        store::get_or_init(&app.db).await.unwrap();
        let a = store::insert_assignment(&app.db, None, &bot_id, "crid-late", "跑一次測試", &[], None, true).await.unwrap();
        store::mark_delivered(&app.db, &a.id, &turn_id, "ok").await.unwrap();
        // controller 在備援收掉回合時的結算：沒有回覆、證據不完整。
        store::settle_and_notify(&app.db, &a.id, "completed_fallback", false, None, None, "k-fallback", "assignment_completed", &json!({}))
            .await
            .unwrap();

        process(&app, &codex_done(&bot_id, "跑一次測試")).await.unwrap();
        // 補寫會推 turn_updated（completed）；controller 的事件迴圈收到後呼叫這一支。
        controller::late_reply_for_turn(&app, &turn_id, "completed").await;

        let row = store::assignment(&app.db, &a.id).await.unwrap().unwrap();
        assert_eq!(row.status, "awaiting_review", "還是等驗收，只是結果到了");
        assert_eq!(row.result.as_deref(), Some("hook reply"));
        assert_eq!(row.turn_status.as_deref(), Some("completed"));
        assert_eq!(row.evidence_complete, Some(1));
        let events: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, payload_json FROM supervisor_inbox WHERE assignment_id=? AND event_key LIKE 'assignment_late_reply:%'")
                .bind(&a.id)
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        let p: Value = serde_json::from_str(&events[0].1).unwrap();
        assert_eq!(events[0].0, "assignment_completed");
        assert_eq!((p["result"].as_str(), p["late_reply"].as_bool(), p["needs_review"].as_bool()), (Some("hook reply"), Some(true), Some(true)));

        // 同一個事件再來一次（重播、reconcile）：不再推第二則。
        controller::late_reply_for_turn(&app, &turn_id, "completed").await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE assignment_id=?").bind(&a.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 2, "結算那則＋結果到了那則");
    }

    /// 一顆 claude bot＋一筆已經送達（`delivery='ok'`）、還在飛的回合。StopFailure 要收的就是這種。
    async fn delivered_turn(app: &Arc<App>, project_id: &str) -> (String, String, String) {
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(project_id)
        // ULID 的前段是時間戳：同一毫秒建的兩顆 bot 前六碼一樣，撞 `bots.project_id, bots.name`。取尾段。
        .bind(format!("hook-{}", &bot_id[bot_id.len() - 8..]))
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','working','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,?,'web','in_flight','ok','跑一次測試',?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        (bot_id, conversation_id, turn_id)
    }

    fn stop_failure(bot_id: &str, payload: Value) -> HookBody {
        HookBody { bot_id: bot_id.to_string(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }
    }

    async fn turn_row(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    async fn system_notes(app: &Arc<App>, turn_id: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// issue #79：`StopFailure` 一到就把回合收成失敗，而且看得出為什麼——以前要等 §4.3 備援或
    /// stuck watchdog 才發現，中間一直掛在 in_flight。`delivery` 不動：字是送出去了，失敗的是回合。
    #[tokio::test]
    async fn a_stop_failure_closes_the_turn_as_failed_with_a_readable_reason() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;

        process(
            &app,
            &stop_failure(
                &bot_id,
                json!({"hook_event_name": "StopFailure", "session_id": "s1", "prompt_id": "p1",
                       "reason": "API Error: 429 rate_limit_error"}),
            ),
        )
        .await
        .unwrap();

        let t = turn_row(&app, &turn_id).await;
        assert_eq!(t.status, "failed", "回合當場收成失敗，不必等 watchdog");
        assert_eq!(t.delivery, "ok", "送達與回合成敗是兩件事");
        assert!(t.completed_at.is_some());
        assert_eq!((t.native_session_id.as_deref(), t.native_turn_id.as_deref()), (Some("s1"), Some("p1")), "認得出是哪一回合");
        let notes = system_notes(&app, &turn_id).await;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("額度或速率限制"), "分類要看得出來：{notes:?}");
        assert!(notes[0].contains("429 rate_limit_error"), "原文也要留著：{notes:?}");
    }

    /// 使用者自己按停不是 provider 失敗。兩條路都要擋：payload 說得出是中斷時照它說的；
    /// 說不出來時看 daemon 自己的紀錄（`interrupt_bot` 先記下被中斷的是哪一回合才收 turn）。
    #[tokio::test]
    async fn a_user_interrupt_is_never_recorded_as_a_provider_failure() {
        let env = tt::env().await;
        let app = env.app.clone();

        // (a) payload 自己說是中斷。
        let (bot_a, _c, turn_a) = delivered_turn(&app, &env.project_id).await;
        process(
            &app,
            &stop_failure(
                &bot_a,
                json!({"hook_event_name": "StopFailure", "session_id": "sa", "prompt_id": "pa",
                       "reason": "[Request interrupted by user]"}),
            ),
        )
        .await
        .unwrap();
        let t = turn_row(&app, &turn_a).await;
        assert_eq!(t.status, "in_flight", "使用者中斷不由這條路收尾");
        assert!(system_notes(&app, &turn_a).await.is_empty(), "也不寫「失敗」的說明");

        // (b) payload 說不出原因，但 daemon 記得使用者剛按停的就是這一筆（`fail_in_flight` 還沒收掉它）。
        let (bot_b, _c, turn_b) = delivered_turn(&app, &env.project_id).await;
        let run_b = turn_row(&app, &turn_b).await.run_id.unwrap();
        lifecycle::expect_interrupt_echo(
            &bot_b,
            lifecycle::InterruptedTurn {
                run_id: run_b,
                turn_id: Some(turn_b.clone()),
                session_id: None,
                prompt_id: None,
                at: chrono::Utc::now(),
            },
        );
        process(&app, &stop_failure(&bot_b, json!({"hook_event_name": "StopFailure", "session_id": "sb", "prompt_id": "pb"})))
            .await
            .unwrap();
        let t = turn_row(&app, &turn_b).await;
        assert_eq!(t.status, "in_flight", "剛按過停：這則 StopFailure 是那次 Esc 的回聲");
        assert!(system_notes(&app, &turn_b).await.is_empty());
    }

    /// 後到的 StopFailure 不會把已經收好的回合再收一次，重播的同一則也不會（issue #79 驗收第二條）。
    #[tokio::test]
    async fn a_late_or_replayed_stop_failure_does_not_close_a_turn_twice() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
        // Stop 先到，回合已經答完。
        sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&turn_id)
            .execute(&app.db)
            .await
            .unwrap();

        let ev = stop_failure(
            &bot_id,
            json!({"hook_event_name": "StopFailure", "session_id": "s9", "prompt_id": "p9", "reason": "API Error: 500"}),
        );
        process(&app, &ev).await.unwrap();

        let t = turn_row(&app, &turn_id).await;
        assert_eq!(t.status, "completed", "已經收好的不再被改成 failed");
        assert!(system_notes(&app, &turn_id).await.is_empty(), "也不補一則失敗說明");

        // 同一則重播（spool 重送）：照樣什麼都不動。
        process(&app, &ev).await.unwrap();
        assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
        assert!(system_notes(&app, &turn_id).await.is_empty());
    }

    /// 去重是「這顆 bot 自己」的事：另一顆 bot 的 token 驗得過，但它不能靠先送一則帶著別人 session／turn id 的 hook，
    /// 把那組 id 先佔走，讓真正屬於那顆 bot 的完成事件被當成「重複」吞掉（回合卡 in_flight 到備援才收）。
    #[tokio::test]
    async fn another_bots_hook_cannot_pre_claim_my_session_and_turn_ids() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (attacker, _, attacker_turn) = delivered_turn(&app, &env.project_id).await;
        let (victim, _, victim_turn) = delivered_turn(&app, &env.project_id).await;
        let stop = |bot: &str, msg: &str| {
            stop_failure(bot, json!({"hook_event_name": "Stop", "session_id": "s-shared", "prompt_id": "p-shared", "last_assistant_message": msg}))
        };
        process(&app, &stop(&attacker, "我先到")).await.unwrap();
        assert_eq!(turn_row(&app, &attacker_turn).await.status, "completed");

        process(&app, &stop(&victim, "我才是被問的那顆")).await.unwrap();
        assert_eq!(turn_row(&app, &victim_turn).await.status, "completed", "別顆 bot 先用過同一組 id，不能讓我的完成事件被當成重複");

        // 失敗事件同一道理。
        let (attacker2, _, _) = delivered_turn(&app, &env.project_id).await;
        let (victim2, _, victim2_turn) = delivered_turn(&app, &env.project_id).await;
        let fail = |bot: &str| stop_failure(bot, json!({"hook_event_name": "StopFailure", "session_id": "s-f", "prompt_id": "p-f", "reason": "API Error: 500"}));
        process(&app, &fail(&attacker2)).await.unwrap();
        process(&app, &fail(&victim2)).await.unwrap();
        assert_eq!(turn_row(&app, &victim2_turn).await.status, "failed");
    }

    /// 外部回合（終端手打、沒有 in-flight 的回合）那條路一樣：別顆 bot 佔過的 id 不能讓這一筆 insert 撞唯一索引、一直重試。
    #[tokio::test]
    async fn an_external_turn_survives_native_ids_another_bot_already_used() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (attacker, _, attacker_turn) = delivered_turn(&app, &env.project_id).await;
        let (victim, victim_conv, victim_turn) = delivered_turn(&app, &env.project_id).await;
        // 受害者那顆沒有 in-flight 的回合（使用者在終端手打的）。
        sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?").bind(db::now()).bind(&victim_turn).execute(&app.db).await.unwrap();
        let stop = |bot: &str| stop_failure(bot, json!({"hook_event_name": "Stop", "session_id": "s-ext", "prompt_id": "p-ext", "last_assistant_message": "終端那句的回答"}));
        process(&app, &stop(&attacker)).await.unwrap();
        assert_eq!(turn_row(&app, &attacker_turn).await.status, "completed");
        process(&app, &stop(&victim)).await.expect("撞到別顆 bot 的 native id 也要收得下");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='assistant' AND content='終端那句的回答'")
            .bind(&victim_conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1, "這顆 bot 的外部回合與回覆都記下來了");
    }

    /// StatusLine 是「最新的贏」的單槽訊號（每次重繪一則）：bot 鎖被長時間握著時，不能每一則各丟一個背景 task 排隊等鎖
    /// （每個都抱著一份 body，高頻時 task 與記憶體一路疊上去）。同一顆 bot 最多一個在等，後到的取代先到的。
    #[tokio::test]
    async fn statusline_hooks_queued_behind_a_busy_bot_lock_are_coalesced() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "status-flood").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let lock = app.bot_lock(&bot.id).await;
        let held = lock.lock().await;
        let spawned_before = statusline_tasks_spawned();
        for i in 0..50 {
            let mut headers = HeaderMap::new();
            headers.insert("X-AM-Bot-Token", bot.hook_token.parse().unwrap());
            let body = HookBody {
                bot_id: bot.id.clone(),
                provider: "claude".into(),
                payload: json!({"hook_event_name": "StatusLine", "session_id": "s-flood", "model": {"id": format!("claude-model-{i}")}}),
                received_at: None,
                truncated: false,
                run_id: None,
            };
            let (status, _) = receive(State(app.clone()), Path("claude".to_string()), headers, Json(body)).await;
            assert_eq!(status, StatusCode::OK);
        }
        let spawned = statusline_tasks_spawned() - spawned_before;
        assert!(spawned <= 2, "50 則 StatusLine 排在忙碌的 bot 鎖後面，背景 task 只該有 1～2 個，不是 {spawned}");
        drop(held);
        // 放鎖之後最後一則（最新的）贏。
        assert!(
            crate::testing::eventually!(db::run(&app.db, &run_id).await.unwrap().unwrap().runtime_model.as_deref() == Some("claude-model-49")),
            "最新的那一則要生效"
        );
    }

    /// 型別怪的 payload（字串、數字、陣列、null、很深的巢狀）不能 panic，也不能讓整批卡住：處理完就是處理完。
    #[tokio::test]
    async fn payloads_of_the_wrong_shape_are_handled_without_panicking() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
        let mut deep = json!("x");
        for _ in 0..200 {
            deep = json!({ "a": deep });
        }
        for payload in [
            json!("a string"),
            json!(5),
            json!([1, 2, 3]),
            json!(null),
            json!({"hook_event_name": 5}),
            json!({"hook_event_name": "PostToolUse", "tool_use_id": null, "tool_response": deep}),
            json!({"hook_event_name": "UserPromptSubmit", "prompt": 12}),
        ] {
            let ev = stop_failure(&bot_id, payload.clone());
            process(&app, &ev).await.unwrap_or_else(|e| panic!("{payload}: {e:#}"));
        }
        // 這一串怪事件沒有誤收掉那顆還在跑的回合。
        assert_eq!(turn_row(&app, &turn_id).await.status, "in_flight");
        // Stop 的欄位型別全錯：照樣只是「這一回合結束了」，不 panic、也沒有把垃圾當成回覆。
        let weird_stop = stop_failure(
            &bot_id,
            json!({"hook_event_name": "Stop", "session_id": 7, "prompt_id": ["x"], "last_assistant_message": {"not": "text"}}),
        );
        process(&app, &weird_stop).await.unwrap();
        assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
    }

    /// 收件匣的重試（issue #70）要能補回第一次只做了一半的處理。第一次認領回合、寫下 native id 之後，
    /// 回覆那句 insert 失敗（這裡用 trigger 模擬）：重試時同一組 native id 會被去重擋掉——認領跟回覆
    /// 不在同一個交易裡的話，這則回覆就永遠不見了，回合卻已經是 completed。
    async fn fail_next_insert(app: &Arc<App>, role: &str) {
        sqlx::query(&format!(
            "CREATE TRIGGER flaky_insert BEFORE INSERT ON messages WHEN NEW.role='{role}'
             BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
    }

    async fn heal(app: &Arc<App>) {
        sqlx::query("DROP TRIGGER flaky_insert").execute(&app.db).await.unwrap();
    }

    async fn replies(app: &Arc<App>, conv: &str) -> Vec<(Option<String>, String)> {
        sqlx::query_as("SELECT turn_id, content FROM messages WHERE conversation_id=? AND role='assistant'")
            .bind(conv)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_reply_whose_first_attempt_failed_halfway_survives_the_retry() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = delivered_turn(&app, &env.project_id).await;
        let ev = stop_failure(
            &bot_id,
            json!({"hook_event_name": "Stop", "session_id": "s-r", "prompt_id": "p-r", "last_assistant_message": "測試全綠"}),
        );

        fail_next_insert(&app, "assistant").await;
        assert!(process(&app, &ev).await.is_err(), "第一次寫不進回覆：錯誤要往上傳，收件匣才會重試");
        heal(&app).await;
        process(&app, &ev).await.expect("重試");

        assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
        assert_eq!(replies(&app, &conv).await, vec![(Some(turn_id), "測試全綠".to_string())], "回覆不能在重試時被去重吃掉");
    }

    /// 同一件事，外部回合那條路（沒有 in-flight 的回合，hook 自己開一筆 completed 的）。
    #[tokio::test]
    async fn an_external_reply_whose_first_attempt_failed_halfway_survives_the_retry() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = delivered_turn(&app, &env.project_id).await;
        sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?").bind(db::now()).bind(&turn_id).execute(&app.db).await.unwrap();
        let ev = stop_failure(
            &bot_id,
            json!({"hook_event_name": "Stop", "session_id": "s-x", "prompt_id": "p-x", "last_assistant_message": "終端手打那句的回答"}),
        );

        fail_next_insert(&app, "assistant").await;
        assert!(process(&app, &ev).await.is_err());
        heal(&app).await;
        process(&app, &ev).await.expect("重試");

        let got = replies(&app, &conv).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].1, "終端手打那句的回答");
    }

    /// 同一件事，`StopFailure`：回合收成 failed 之後說明寫不進去，重試不能只剩一筆沒有原因的失敗回合。
    #[tokio::test]
    async fn a_stop_failure_note_whose_first_attempt_failed_survives_the_retry() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
        let ev = stop_failure(
            &bot_id,
            json!({"hook_event_name": "StopFailure", "session_id": "s-f", "prompt_id": "p-f", "reason": "API Error: 500"}),
        );

        fail_next_insert(&app, "system").await;
        assert!(process(&app, &ev).await.is_err());
        heal(&app).await;
        process(&app, &ev).await.expect("重試");

        assert_eq!(turn_row(&app, &turn_id).await.status, "failed");
        let notes = system_notes(&app, &turn_id).await;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("API Error: 500"), "{notes:?}");
    }

    /// 同一件事，遲到的 hook 補備援關掉的回合（`fill_or_drop_late_hook`）：先寫 native id 再補回覆的話，
    /// 回覆失敗後重試被去重擋掉，回合還被升級成 completed、卻沒有回覆。
    #[tokio::test]
    async fn a_late_fill_whose_first_attempt_failed_survives_the_retry() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = fallback_closed_turn(&app, &env.project_id, "跑一次 測試").await;
        let ev = codex_done(&bot_id, "跑一次 測試");

        fail_next_insert(&app, "assistant").await;
        assert!(process(&app, &ev).await.is_err());
        heal(&app).await;
        process(&app, &ev).await.expect("重試");

        assert_eq!(turn_row(&app, &turn_id).await.status, "completed");
        assert_eq!(replies(&app, &conv).await, vec![(Some(turn_id), "hook reply".to_string())]);
    }

    /// issue #108：撞額度的 StopFailure 在推回合結束**之前**就把撞限記下來——那個事件會叫醒 queue flush，
    /// flush 要看得到這個身分沒額度。一下就好的限流（overloaded）不記：記了會把派工壓上好幾個小時。
    #[tokio::test]
    async fn a_quota_stop_failure_records_the_limit_before_the_turn_ends() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
        let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
        process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "StopFailure", "session_id": "s1", "prompt_id": "p1",
                                                    "reason": "API Error: 529 overloaded_error"})))
            .await
            .unwrap();
        assert_eq!(turn_row(&app, &turn_id).await.status, "failed");
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "過載不是額度用完");

        let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
        let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
        process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "StopFailure", "session_id": "s2", "prompt_id": "p2",
                                                    "reason": "You've hit your session limit · resets 5pm"})))
            .await
            .unwrap();
        assert_eq!(turn_row(&app, &turn_id).await.status, "failed");
        let hit = crate::runners::quota::limit_hit_for_bot(&app, &bot).await.expect("撞限記下來了");
        assert_eq!(hit.bucket.as_deref(), Some("five_hour"));
        assert!(hit.until.is_some(), "有期限，不會永遠擋著");
    }

    /// #108 重開：撞額度的 `StopFailure` 到的時候讀不到這顆（遠端）bot 在哪台主機。以前退回 `local`：撞限寫進本機帳號
    /// 那一格，回合照收、推回合結束，flush 查遠端那把 key 看不到撞限，排著的派工當場送進用盡的身分。現在這一則失敗、
    /// 由收件匣重試：哪一格都不寫、回合不收（在飛的回合本身擋著 flush），撞限記成欠著——就算回合被別的路收掉，排著的
    /// 照欠著的那一筆擋。讀得到之後重試：撞限進 `remote1/claude`，回合照常收成失敗。讀不到 bot 本身也一樣是這一則失敗。
    #[tokio::test]
    async fn a_quota_stop_failure_whose_host_cannot_be_read_fails_and_holds_the_queue() {
        let env = tt::env().await;
        let app = env.app.clone();
        let remote = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'remote1', ?)")
            .bind(&remote)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let (bot_id, conv, turn_id) = delivered_turn(&app, &remote).await;
        let queued = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','下一件派工',?)")
            .bind(&queued)
            .bind(&conv)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let ev = stop_failure(&bot_id, json!({"hook_event_name": "StopFailure", "session_id": "s-far", "prompt_id": "p-far",
                                              "reason": "You've hit your session limit · resets 5pm"}));
        let limit_hits = || {
            let app = app.clone();
            async move { app.quotas.lock().await.iter().filter(|(_, q)| q.limit_hit.is_some()).map(|(k, _)| k.clone()).collect::<Vec<_>>() }
        };

        sqlx::query("ALTER TABLE bots RENAME TO bots_unreadable").execute(&app.db).await.unwrap();
        assert!(process(&app, &ev).await.is_err(), "讀不到 bot：這一則失敗、收件匣重試");
        sqlx::query("ALTER TABLE bots_unreadable RENAME TO bots").execute(&app.db).await.unwrap();
        assert_eq!(turn_row(&app, &turn_id).await.status, "in_flight");

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        assert!(process(&app, &ev).await.is_err(), "讀不到主機：不當作沒撞限");
        assert_eq!(limit_hits().await, Vec::<String>::new(), "哪一格都沒寫，尤其不是本機的 `claude`");
        assert!(crate::turn_error::owes_limit_hit(&app, &bot_id), "撞限記成欠著");
        let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
        let owed_at = crate::runners::quota::limit_hit_for_bot(&app, &bot).await.expect("派送前照欠著的那一筆擋").at;
        assert_eq!(turn_row(&app, &turn_id).await.status, "in_flight", "回合不收、不推回合結束");
        // 回合被別的路收掉（卡住的回合被看門狗收了）：沒有在飛的回合擋著，排著的照欠著的那一筆擋。
        let run: String = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='failed', completed_at=? WHERE id=?").bind(db::now()).bind(&turn_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        lifecycle::forget_queue_retry_timer(&bot_id);
        lifecycle::flush_queued_locked(&app, &bot_id).await.unwrap();
        let q = turn_row(&app, &queued).await;
        assert_eq!((q.status.as_str(), q.flush_retries, q.run_id.as_deref()), ("queued", 0, None), "派工沒有送進用盡的身分");
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();

        // 收件匣重試：讀得到了。
        process(&app, &ev).await.expect("重試");
        assert_eq!(limit_hits().await, vec!["remote1/claude".to_string()], "記在遠端那一格");
        assert_eq!(app.quotas.lock().await["remote1/claude"].limit_hit.as_ref().map(|h| h.at.clone()), Some(owed_at), "撞限時刻是當初那一刻，不是重試的時刻");
        assert!(!crate::turn_error::owes_limit_hit(&app, &bot_id));
        lifecycle::forget_queue_retry_timer(&bot_id);
        lifecycle::flush_queued_locked(&app, &bot_id).await.unwrap();
        let q = turn_row(&app, &queued).await;
        assert_eq!((q.status.as_str(), q.flush_retries), ("queued", 0), "照撞限擋");
        let hold: Option<String> = sqlx::query_scalar("SELECT quota_hold FROM turns WHERE id=?").bind(&queued).fetch_one(&app.db).await.unwrap();
        assert!(hold.is_some(), "憑據落地");
        lifecycle::forget_queue_retry_timer(&bot_id);
        crate::lifecycle::quota_hold::forget_held(&bot_id);
    }

    /// #108 重開：遠端 bot 的 statusLine 到的時候讀不到它在哪台主機。以前退回 `local`：遠端帳號的讀數寫進本機那一格，
    /// 還拿它去校正本機帳號的撞限——那個窗是撞限之後才開的，本機真的撞限就被作廢了。現在丟掉這一份，下一次重繪再來。
    #[tokio::test]
    async fn a_status_line_whose_host_cannot_be_read_is_dropped_not_written_to_the_local_key() {
        let env = tt::env().await;
        let app = env.app.clone();
        let remote = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'remote1', ?)")
            .bind(&remote)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let (bot_id, _conv, _turn) = delivered_turn(&app, &remote).await;
        let until = db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2));
        assert!(crate::quota::seed_limit_hit(&app, crate::config::LOCAL_HOST, "claude", &until, "You've hit your session limit", Some("five_hour".into())).await);
        let reopened = (chrono::Utc::now() + chrono::Duration::hours(5) + chrono::Duration::seconds(30)).timestamp();
        let line = HookBody {
            bot_id: bot_id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "StatusLine", "rate_limits": {"five_hour": {"used_percentage": 3.0, "resets_at": reopened}}}),
            received_at: None,
            truncated: false,
            run_id: None,
        };

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(&app.db).await.unwrap();
        assert!(process(&app, &line).await.is_err(), "讀不到主機：丟掉這一份");
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(&app.db).await.unwrap();
        {
            let q = app.quotas.lock().await;
            assert!(q["claude"].limit_hit.is_some(), "本機帳號的撞限沒被遠端的讀數作廢");
            assert!(q["claude"].five_hour.is_none(), "遠端的讀數沒寫進本機那一格");
        }
        process(&app, &line).await.unwrap();
        let q = app.quotas.lock().await;
        assert_eq!(q.get("remote1/claude").and_then(|x| x.five_hour.as_ref()).map(|w| w.used_pct), Some(3.0), "讀得到：寫進遠端那一格");
        assert!(q["claude"].limit_hit.is_some());
    }

    /// issue #150：撞額度是帳號的事實，跟這一則還有沒有回合可收無關。Esc 把回合收掉之後，同一個身分晚到的真撞額度
    /// （對不上中斷的那一則、或就是被中斷那一則的回聲）都要記下撞限，下一件派工才不會送進沒額度的身分；
    /// 回合本身一個欄位都不改。沒有 run（說不準是哪個身分）與已經收過的同一則（重播）不記。
    #[tokio::test]
    async fn a_quota_stop_failure_after_an_esc_still_records_the_limit() {
        let env = tt::env().await;
        let app = env.app.clone();
        const LIMIT: &str = "You've hit your session limit · resets 5pm";

        // (a) Esc 已經把回合收掉（`fail_in_flight`），晚到的撞額度對不上被中斷的那一則：沒有 in-flight 回合可掛。
        let (bot_a, conv_a, turn_a) = delivered_turn(&app, &env.project_id).await;
        let run_a = turn_row(&app, &turn_a).await.run_id.unwrap();
        lifecycle::expect_interrupt_echo(
            &bot_a,
            lifecycle::InterruptedTurn { run_id: run_a.clone(), turn_id: Some(turn_a.clone()), session_id: None, prompt_id: Some("p-esc".into()), at: chrono::Utc::now() },
        );
        lifecycle::fail_in_flight(&app, &run_a, "user interrupt").await.unwrap();
        let closed = turn_row(&app, &turn_a).await;
        process(&app, &stop_failure(&bot_a, json!({"hook_event_name": "StopFailure", "session_id": "sa", "prompt_id": "p-late", "reason": LIMIT})))
            .await
            .unwrap();
        let bot = db::bot(&app.db, &bot_a).await.unwrap().unwrap();
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "沒有回合可掛，撞限照樣記下");
        let after = turn_row(&app, &turn_a).await;
        assert_eq!((after.status, after.completed_at), (closed.status, closed.completed_at), "已經收掉的回合不改");
        // 下一件派工：排在佇列的那一則不會被送進 A。
        let queued = db::ulid();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run_a).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','下一件派工',?)")
            .bind(&queued)
            .bind(&conv_a)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        lifecycle::forget_queue_retry_timer(&bot_a);
        lifecycle::flush_queued_locked(&app, &bot_a).await.unwrap();
        let q = turn_row(&app, &queued).await;
        assert_eq!((q.status.as_str(), q.flush_retries, q.run_id.as_deref()), ("queued", 0, None), "派工沒有送進沒額度的 A");
        lifecycle::forget_queue_retry_timer(&bot_a);

        // (b) 就是被中斷那一則的回聲（同一個 prompt id）：回合照舊一個欄位都不動，撞限一樣記。
        let (bot_b, _c, turn_b) = delivered_turn(&app, &env.project_id).await;
        let run_b = turn_row(&app, &turn_b).await.run_id.unwrap();
        lifecycle::expect_interrupt_echo(
            &bot_b,
            lifecycle::InterruptedTurn { run_id: run_b, turn_id: Some(turn_b.clone()), session_id: None, prompt_id: Some("p-b".into()), at: chrono::Utc::now() },
        );
        process(&app, &stop_failure(&bot_b, json!({"hook_event_name": "StopFailure", "session_id": "sb", "prompt_id": "p-b", "reason": LIMIT})))
            .await
            .unwrap();
        assert_eq!(turn_row(&app, &turn_b).await.status, "in_flight", "回聲：回合不動");
        let bot = db::bot(&app.db, &bot_b).await.unwrap().unwrap();
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "回聲裡的撞額度也是真的");

        // (c) 沒有 run：說不準是哪個身分送的（停機後可能換過身分），不記。
        let (bot_c, _c, turn_c) = delivered_turn(&app, &env.project_id).await;
        let run_c = turn_row(&app, &turn_c).await.run_id.unwrap();
        sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&run_c).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET identity='cc9' WHERE id=?").bind(&bot_c).execute(&app.db).await.unwrap();
        process(&app, &stop_failure(&bot_c, json!({"hook_event_name": "StopFailure", "session_id": "sc", "prompt_id": "p-c", "reason": LIMIT})))
            .await
            .unwrap();
        let bot = db::bot(&app.db, &bot_c).await.unwrap().unwrap();
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "沒有 run 的不記到現在的身分上");

        // (d) 已經收過的同一則重播：不再記一次（不把撞限時刻往後推）。
        let (bot_d, _c, turn_d) = delivered_turn(&app, &env.project_id).await;
        let ev = stop_failure(&bot_d, json!({"hook_event_name": "StopFailure", "session_id": "sd", "prompt_id": "p-d", "reason": LIMIT}));
        process(&app, &ev).await.unwrap();
        assert_eq!(turn_row(&app, &turn_d).await.status, "failed");
        let bot = db::bot(&app.db, &bot_d).await.unwrap().unwrap();
        let first = crate::runners::quota::limit_hit_for_bot(&app, &bot).await.expect("第一次記下");
        crate::runners::quota::clear_limit_hit_for_bot(&app, &bot).await;
        process(&app, &ev).await.unwrap();
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "重播不再記：{first:?}");
    }

    /// 分類：rate limit 跟其他錯誤要分得開，中斷排在最前面（寧可少收一次也不要把使用者按的停說成失敗）。
    #[test]
    fn the_failure_classifier_keeps_rate_limit_auth_and_interrupts_apart() {
        use FailureReason::*;
        assert_eq!(classify_failure(Some("API Error: 429 rate_limit_error")), RateLimit);
        assert_eq!(classify_failure(Some("You've hit your usage limit")), RateLimit);
        // claude 真正的撞額度橫幅沒有 rate／usage 字樣（issue #108）：以前被分成「API 錯誤」。
        assert_eq!(classify_failure(Some("You've hit your session limit · resets 5pm")), RateLimit);
        assert_eq!(classify_failure(Some("API Error: You've hit your weekly limit")), RateLimit);
        assert_eq!(classify_failure(Some("overloaded_error")), RateLimit);
        assert_eq!(classify_failure(Some("401 Unauthorized: invalid api key")), Auth);
        assert_eq!(classify_failure(Some("OAuth token expired; please login")), Auth);
        assert_eq!(classify_failure(Some("API Error: 500 Internal Server Error")), Api);
        assert_eq!(classify_failure(Some("[Request interrupted by user]")), Interrupted);
        assert_eq!(classify_failure(Some("cancelled")), Interrupted);
        assert_eq!(classify_failure(None), Unknown);
        assert_eq!(classify_failure(Some("   ")), Unknown);
    }

    /// `esc` 這個 needle 是用純字串 `contains` 比對的：`unescaped`／`description`／`prescribed`
    /// 這種一般英文字都含著 `esc` 三個字母，不該被誤判成使用者按了 Esc——那樣一來真正的失敗（API／
    /// 額度）會走進「中斷」那條回傳早退，回合永遠卡在 in_flight，額度也不會被記下來（issue #79／#108
    /// 想防的正是這件事，「esc」子字串比對把它繞回去了）。
    #[test]
    fn a_generic_word_that_merely_contains_esc_is_not_a_user_interrupt() {
        use FailureReason::*;
        assert_eq!(
            classify_failure(Some("invalid_request_error: unescaped control character in string")),
            Api,
            "「unescaped」含 esc，但這是一個 API 驗證錯誤，不是使用者按了 Esc"
        );
        assert_eq!(
            classify_failure(Some("A description of the rate limit reached for this account")),
            RateLimit,
            "「description」含 esc，不該蓋掉後面真正的 rate limit 分類"
        );
        // 真的是 Esc 觸發的中斷（獨立一個字，前後被非英數字元包住）照樣要抓到。
        assert_eq!(classify_failure(Some("Cancelled (Esc)")), Interrupted);
        assert_eq!(classify_failure(Some("esc")), Interrupted);
    }

    /// payload 的欄位名還在動：撈得到就留原文，撈不到也照樣收回合（只是說不出原因）。
    #[test]
    fn the_failure_detail_is_read_from_whichever_field_carries_it() {
        assert_eq!(failure_detail(&json!({"reason": "rate_limit_error"})).as_deref(), Some("rate_limit_error"));
        assert_eq!(failure_detail(&json!({"error": {"type": "overloaded_error"}})).as_deref(), Some("overloaded_error"));
        assert_eq!(
            failure_detail(&json!({"error": {"message": "Internal server error", "type": "api_error"}})).as_deref(),
            Some("Internal server error"),
        );
        assert_eq!(failure_detail(&json!({"hook_event_name": "StopFailure", "session_id": "s"})), None);
        assert_eq!(failure_detail(&json!({"reason": "  "})), None, "空白不算原因");
    }

    /// 分類本身：`StopFailure` 走 `TurnFailed`，一般 `Stop` 不受影響。
    #[test]
    fn stop_failure_classifies_as_a_failed_turn_and_plain_stop_is_untouched() {
        let v = json!({"hook_event_name": "StopFailure", "session_id": "s1", "prompt_id": "p1", "reason": "429 rate_limit"});
        match classify("claude", &v) {
            HookKind::TurnFailed { session_id, turn_id, reason, detail, .. } => {
                assert_eq!(session_id.as_deref(), Some("s1"));
                assert_eq!(turn_id.as_deref(), Some("p1"));
                assert_eq!(reason, FailureReason::RateLimit);
                assert_eq!(detail.as_deref(), Some("429 rate_limit"));
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
        let v = json!({"hook_event_name": "Stop", "session_id": "s1", "prompt_id": "p1"});
        assert!(matches!(classify("claude", &v), HookKind::TurnComplete { .. }), "一般 Stop 照舊");
    }

    /// issue #82：`SubagentStart`／`SubagentStop` 分類成 `SubagentEvent`，帶得到 `agent_id`／
    /// `agent_type`；只有 `SubagentStop` 帶 `agent_transcript_path`（`SubagentStart` 的原生 payload
    /// 本來就沒有這個欄位）。不認得的事件名字（例如 claude 才有的 `TeammateIdle`）照舊被 `Ignore`，
    /// 不會意外冒出一種新的 Turn 副作用。
    #[test]
    fn subagent_start_and_stop_classify_as_a_pure_snapshot() {
        let start = json!({"hook_event_name": "SubagentStart", "session_id": "s1", "agent_id": "a1", "agent_type": "general-purpose"});
        match classify("claude", &start) {
            HookKind::SubagentEvent { event, agent_id, agent_type, transcript_path } => {
                assert_eq!(event, "start");
                assert_eq!(agent_id.as_deref(), Some("a1"));
                assert_eq!(agent_type.as_deref(), Some("general-purpose"));
                assert_eq!(transcript_path, None);
            }
            other => panic!("expected SubagentEvent, got {other:?}"),
        }
        let stop = json!({"hook_event_name": "SubagentStop", "agent_id": "a1", "agent_type": "general-purpose",
                           "agent_transcript_path": "/tmp/a1.jsonl"});
        match classify("claude", &stop) {
            HookKind::SubagentEvent { event, transcript_path, .. } => {
                assert_eq!(event, "stop");
                assert_eq!(transcript_path.as_deref(), Some("/tmp/a1.jsonl"));
            }
            other => panic!("expected SubagentEvent, got {other:?}"),
        }
        assert!(matches!(classify("claude", &json!({"hook_event_name": "TeammateIdle"})), HookKind::Ignore(_)));
    }

    /// issue #82：`process` 把 `SubagentStart`／`SubagentStop` 整筆寫進這顆 run 的 `subagent_json`，
    /// 不建立、不動任何 Turn（跟 turn 完全脫鉤）；沒有活著的 run 就安靜丟掉，不報錯。
    #[tokio::test]
    async fn subagent_events_update_the_runs_snapshot_without_touching_any_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;

        process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "SubagentStart", "agent_id": "a1", "agent_type": "Explore"})))
            .await
            .unwrap();
        let snap: Option<String> = sqlx::query_scalar("SELECT subagent_json FROM runs WHERE bot_id = ?").bind(&bot_id).fetch_one(&app.db).await.unwrap();
        let snap: Value = serde_json::from_str(&snap.expect("snapshot written")).unwrap();
        assert_eq!(snap["event"], json!("start"));
        assert_eq!(snap["agent_id"], json!("a1"));
        assert_eq!(snap["agent_type"], json!("Explore"));
        assert_eq!(snap["transcript_path"], Value::Null);

        process(
            &app,
            &stop_failure(
                &bot_id,
                json!({"hook_event_name": "SubagentStop", "agent_id": "a1", "agent_type": "Explore", "agent_transcript_path": "/tmp/a1.jsonl"}),
            ),
        )
        .await
        .unwrap();
        let snap: Option<String> = sqlx::query_scalar("SELECT subagent_json FROM runs WHERE bot_id = ?").bind(&bot_id).fetch_one(&app.db).await.unwrap();
        let snap: Value = serde_json::from_str(&snap.unwrap()).unwrap();
        assert_eq!(snap["event"], json!("stop"), "後到的 stop 蓋掉 start，只留最新的快照");
        assert_eq!(snap["transcript_path"], json!("/tmp/a1.jsonl"));

        // 完全脫鉤：這顆 run 底下的 turn 沒有被碰過。
        let t = turn_row(&app, &turn_id).await;
        assert_eq!(t.status, "in_flight", "subagent 事件不該動到任何 turn");
    }

    /// 沒有活著的 run（例如 bot 剛好在重啟中間）：安靜丟掉，不報錯、也不憑空造一列。
    #[tokio::test]
    async fn a_subagent_event_with_no_active_run_is_a_quiet_no_op() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        process(&env.app, &stop_failure(&bot.id, json!({"hook_event_name": "SubagentStart", "agent_id": "a1"}))).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ?").bind(&bot.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(n, 0, "沒有 run 就沒有地方寫，也不該無中生有");
    }

    /// issue #94：`PostToolUse` 的 Bash 輸出剛好是 `herdr agent start` 的 JSON 回應時分類成
    /// `SpawnHint`；一般指令的輸出（不是 herdr 的 JSON 信封）照舊被 `Ignore`。
    #[test]
    fn post_tool_use_classifies_a_herdr_spawn_as_a_hint() {
        let v = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
            "tool_response": {"stdout": r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":"w1:p2"}}}"#, "stderr": ""},
        });
        match classify("claude", &v) {
            HookKind::SpawnHint { pane_ids } => assert_eq!(pane_ids, ["w1:p2"]),
            other => panic!("expected SpawnHint, got {other:?}"),
        }

        let ls = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
            "tool_response": {"stdout": "total 0\n", "stderr": ""},
        });
        assert!(matches!(classify("claude", &ls), HookKind::Ignore(_)));
    }

    /// issue #94：`process` 把偵測到的 `pane_id` 記進 `spawn_hints`，指到送這則 hook 的那顆 bot；
    /// 不需要活著的 run（這是這顆 bot 自己剛做的事，不是它的 Turn 的事），也不動任何 Turn。
    #[tokio::test]
    async fn a_spawn_hint_is_recorded_against_the_bot_that_sent_it() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;

        process(
            &env.app,
            &stop_failure(
                &bot.id,
                json!({
                    "hook_event_name": "PostToolUse",
                    "tool_name": "Bash",
                    "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
                    "tool_response": {"stdout": r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":"w1:p2"}}}"#, "stderr": ""},
                }),
            ),
        )
        .await
        .unwrap();

        let recorded: Option<String> =
            sqlx::query_scalar("SELECT bot_id FROM spawn_hints WHERE pane_id = 'w1:p2'").fetch_optional(&env.app.db).await.unwrap();
        assert_eq!(recorded.as_deref(), Some(bot.id.as_str()));
    }

    /// 2026-09-30：遠端 bot 直送的一句，回音先被存成使用者訊息，報備之後才從寄件者的 spool 收到——補標 `relay_from`。
    /// 同一句不標兩次；對不上的、別人的對話都不動。
    #[tokio::test]
    async fn a_late_relay_announce_attributes_the_echo_already_stored() {
        let env = tt::env().await;
        let from = tt::claude_bot(&env.app, &env.project_id, "rpa").await;
        let to = tt::claude_bot(&env.app, &env.project_id, "cicd").await;
        let run = tt::fake_run(&env.app, &to.id).await;
        sqlx::query("UPDATE runs SET agent_name = 'robins-hub-3b84sb' WHERE id = ?").bind(&run).execute(&env.app.db).await.unwrap();
        let conv = db::conversation_id(&env.app.db, &to.id).await.unwrap();
        let text = "我是 robins-hub-bf3xq3。console PR #95 的 test／lint 卡在排隊";
        let echo = lifecycle::insert_message(&env.app, &conv, None, "user", text, "hook", false, None).await.unwrap();
        let web = lifecycle::insert_message(&env.app, &conv, None, "user", text, "web", false, None).await.unwrap();
        sqlx::query("UPDATE messages SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ','now','+1 second') WHERE id = ?")
            .bind(&web.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let announce = |t: &str| HookBody {
            bot_id: from.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_agent": "robins-hub-3b84sb", "text": t}),
            received_at: None,
            truncated: false,
            run_id: None,
        };

        process(&env.app, &announce(text)).await.unwrap();
        let relay = |id: String| {
            let db = env.app.db.clone();
            async move { sqlx::query_scalar::<_, Option<String>>("SELECT relay_from FROM messages WHERE id = ?").bind(id).fetch_one(&db).await.unwrap() }
        };
        assert_eq!(relay(echo.id.clone()).await.as_deref(), Some(from.id.as_str()), "回音補標寄件者");
        assert_eq!(relay(web.id.clone()).await, None, "完全相同的 web 使用者訊息不動");
        assert_eq!(crate::agent_relay::claim(crate::config::LOCAL_HOST, "robins-hub-3b84sb", text), None, "補標後報備已用掉，不會再標一次");
    }

    /// 同一毫秒的兩則備援回覆：遲到的 hook 要蓋「最新」那則（寫入順序），不是 id 字典序大的那則。
    #[tokio::test]
    async fn a_late_hook_replaces_the_fallback_reply_written_last_when_two_share_a_millisecond() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "late-hook-tie").await;
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES ('t-late', ?, 'web','completed_fallback','ok','2026-10-01T00:00:00.000Z')")
            .bind(&conv)
            .execute(&env.app.db)
            .await
            .unwrap();
        for (id, content) in [("m-zzz-first", "備援一"), ("m-aaa-second", "備援二")] {
            sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?, 't-late', 'assistant', ?, 'terminal_fallback', '2026-10-01T00:00:01.000Z')")
                .bind(id)
                .bind(&conv)
                .bind(content)
                .execute(&env.app.db)
                .await
                .unwrap();
        }
        let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id='t-late'").fetch_one(&env.app.db).await.unwrap();
        let tx = env.app.db.begin().await.unwrap();
        replace_fallback_reply(&env.app, tx, &turn, "hook 原文", &None, &None).await.unwrap();
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, content FROM messages WHERE turn_id='t-late' ORDER BY rowid").fetch_all(&env.app.db).await.unwrap();
        assert_eq!(rows, vec![("m-zzz-first".to_string(), "備援一".to_string()), ("m-aaa-second".to_string(), "hook 原文".to_string())]);
    }

    /// 同一毫秒、寫在 `t` 後面的另一個回合，也算「`t` 之後有開過別的回合」：這則 hook 不能認領 `t`。
    #[tokio::test]
    async fn a_turn_opened_in_the_same_millisecond_after_this_one_counts_as_in_between() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "fired-tie").await;
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        for id in ["t-zzz-first", "t-aaa-second"] {
            sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?, 'web','in_flight','ok','2026-10-01T00:00:00.000Z')")
                .bind(id)
                .bind(&conv)
                .execute(&env.app.db)
                .await
                .unwrap();
        }
        let first: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id='t-zzz-first'").fetch_one(&env.app.db).await.unwrap();
        let second: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id='t-aaa-second'").fetch_one(&env.app.db).await.unwrap();
        let fired = Some("2026-10-01T00:00:05Z");
        assert!(!fired_within(&env.app, &first, fired).await.unwrap(), "同毫秒、後寫的那個回合在它之後開");
        assert!(fired_within(&env.app, &second, fired).await.unwrap(), "最後一個回合之後沒有別的");
    }

    /// 稽核：報備表只用 agent 名字當 key。名字是 `<專案>-<bot>`，兩台主機各有同名 agent 並不稀奇；
    /// 寄件者那台的報備不能被另一台同名 agent 的同一句回音認領（錯標＋用掉真收件方的那一筆）。
    #[tokio::test]
    async fn a_relay_announce_is_not_claimed_by_a_same_named_agent_on_another_host() {
        let env = tt::env().await;
        let from = tt::claude_bot(&env.app, &env.project_id, "relay-host-from").await;
        let local = tt::claude_bot(&env.app, &env.project_id, "relay-host-local").await;
        let local_run = tt::fake_run(&env.app, &local.id).await;
        let remote_project = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'remote', 'mac2', ?)")
            .bind(&remote_project)
            .bind(format!("{}/remote", env.dir.display()))
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let remote = tt::claude_bot(&env.app, &remote_project, "relay-host-remote").await;
        let remote_run = tt::fake_run(&env.app, &remote.id).await;
        for run in [&local_run, &remote_run] {
            sqlx::query("UPDATE runs SET agent_name = 'proj-same-name-host' WHERE id = ?").bind(run).execute(&env.app.db).await.unwrap();
        }
        let text = "請幫我看一下這個同名 agent 的跨主機報備是不是會被認錯";
        let announce = HookBody {
            bot_id: from.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_agent": "proj-same-name-host", "text": text}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        process(&env.app, &announce).await.unwrap();

        let remote_run = db::run(&env.app.db, &remote_run).await.unwrap().unwrap();
        assert_eq!(relay_source(&env.app, Some(&remote_run), text).await, None, "另一台主機的同名 agent 不能認領");
        let local_run = db::run(&env.app.db, &local_run).await.unwrap().unwrap();
        assert_eq!(relay_source(&env.app, Some(&local_run), text).await.map(|r| r.from_bot).as_deref(), Some(from.id.as_str()), "真正的收件方照常認領");
    }

    /// #927：報備記下寄件 bot 送它當下正在跑的回合，收件方的回音認領時一起帶出來（child_done 靠它認「這一回合已經自己回報」）。
    #[tokio::test]
    async fn a_relay_announce_carries_the_sender_turn_into_the_echo_it_attributes() {
        let env = tt::env().await;
        let from = tt::claude_bot(&env.app, &env.project_id, "relay-turn-from").await;
        let local = tt::claude_bot(&env.app, &env.project_id, "relay-turn-local").await;
        let local_run = tt::fake_run(&env.app, &local.id).await;
        sqlx::query("UPDATE runs SET agent_name = 'proj-relay-turn' WHERE id = ?").bind(&local_run).execute(&env.app.db).await.unwrap();
        let sender_conv = db::conversation_id(&env.app.db, &from.id).await.unwrap();
        let sender_turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','in_flight','ok',?)")
            .bind(&sender_turn)
            .bind(&sender_conv)
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let text = "請核准這一則，寄件的時候我還在跑這一回合";
        let announce = HookBody {
            bot_id: from.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_agent": "proj-relay-turn", "text": text}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        process(&env.app, &announce).await.unwrap();

        let local_run = db::run(&env.app.db, &local_run).await.unwrap().unwrap();
        let relayed = relay_source(&env.app, Some(&local_run), text).await.expect("收件方的回音認得出來");
        assert_eq!(relayed.from_bot, from.id);
        assert_eq!(relayed.from_turn.as_deref(), Some(sender_turn.as_str()));
    }

    #[tokio::test]
    async fn a_late_relay_announce_attributes_a_terminal_fallback_echo() {
        let env = tt::env().await;
        let from = tt::claude_bot(&env.app, &env.project_id, "rpa-terminal").await;
        let to = tt::claude_bot(&env.app, &env.project_id, "cicd-terminal").await;
        let run = tt::fake_run(&env.app, &to.id).await;
        sqlx::query("UPDATE runs SET agent_name = 'robins-hub-terminal-734' WHERE id = ?").bind(&run).execute(&env.app.db).await.unwrap();
        let conv = db::conversation_id(&env.app.db, &to.id).await.unwrap();
        let text = "請幫我檢查 PR #95，終端截取的 prompt echo";
        let echo = lifecycle::insert_message(&env.app, &conv, None, "user", text, "terminal_fallback", false, None).await.unwrap();
        let announce = HookBody {
            bot_id: from.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_agent": "robins-hub-terminal-734", "text": text}),
            received_at: None,
            truncated: false,
            run_id: None,
        };

        process(&env.app, &announce).await.unwrap();

        let relay: Option<String> = sqlx::query_scalar("SELECT relay_from FROM messages WHERE id = ?")
            .bind(&echo.id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(relay.as_deref(), Some(from.id.as_str()), "terminal_fallback echo 仍可由 late announce 補標");
    }

    #[tokio::test]
    async fn a_late_relay_announce_does_not_attribute_a_matching_web_prefix() {
        let env = tt::env().await;
        let from = tt::claude_bot(&env.app, &env.project_id, "rpa-prefix").await;
        let to = tt::claude_bot(&env.app, &env.project_id, "cicd-prefix").await;
        let run = tt::fake_run(&env.app, &to.id).await;
        sqlx::query("UPDATE runs SET agent_name = 'robins-hub-prefix-734' WHERE id = ?").bind(&run).execute(&env.app.db).await.unwrap();
        let conv = db::conversation_id(&env.app.db, &to.id).await.unwrap();
        let text = "請幫我檢查 PR #95，先跑 test 再回報，附上失敗摘要";
        let web_prefix = "請幫我檢查 PR #95，先跑 test";
        let web = lifecycle::insert_message(&env.app, &conv, None, "user", web_prefix, "web", false, None).await.unwrap();
        sqlx::query("UPDATE messages SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ','now','+1 second') WHERE id = ?")
            .bind(&web.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let announce = HookBody {
            bot_id: from.id,
            provider: "claude".into(),
            payload: json!({"hook_event_name": RELAY_ANNOUNCE_EVENT, "to_agent": "robins-hub-prefix-734", "text": text}),
            received_at: None,
            truncated: false,
            run_id: None,
        };

        process(&env.app, &announce).await.unwrap();

        let relay: Option<String> = sqlx::query_scalar("SELECT relay_from FROM messages WHERE id = ?")
            .bind(&web.id)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(relay, None, ">=12 字元的相符前綴也不能把 web 訊息標成 relay");
    }

    /// 2026-10-01 cf-ox-2：claude 自己接著做的那一輪（背景 shell 跑完），hook 回報的「使用者訊息」是上一則已經回答過的 prompt，
    /// 不能再存一次；真的是新的一句照存。
    #[tokio::test]
    async fn an_external_turn_does_not_restore_the_prompt_that_was_already_answered() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "cfox").await;
        let conv = db::conversation_id(&env.app.db, &bot.id).await.unwrap();
        let answered = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','in_flight','ok',?)")
            .bind(&answered).bind(&conv).bind(db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed' WHERE id=?").bind(&answered).execute(&env.app.db).await.unwrap();
        lifecycle::insert_message(&env.app, &conv, Some(&answered), "user", "ui 審查你自己做", "web", false, None).await.unwrap();
        let external = db::ulid();
        let mut conn = env.app.db.acquire().await.unwrap();
        assert!(repeats_answered_prompt(&mut conn, &conv, &external, "ui 審查你自己做").await.unwrap(), "同一句、那一回合已收掉：是舊的");
        assert!(!repeats_answered_prompt(&mut conn, &conv, &external, "另一句新的話").await.unwrap(), "新的一句照存");
    }

    /// issue #754 的場景：使用者送過「現在部 demo」（已答完），之後 claude 又開了一輪沒有新 prompt 的外部回合
    /// （`begin_external_turn` 因為回音跟上一句一樣沒存）。回傳 (bot, conv, 外部回合)。
    async fn an_external_turn_after_an_answered_prompt(app: &Arc<App>, project_id: &str, fallback_closed: bool) -> (String, String, String) {
        let (bot_id, conv, answered) = unknown_turn(app, project_id, "claude", "現在部 demo").await;
        lifecycle::insert_message(app, &conv, Some(&answered), "user", "現在部 demo", "web", false, None).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed', delivery='ok', completed_at=? WHERE id=?")
            .bind(db::now()).bind(&answered).execute(&app.db).await.unwrap();
        let run_id: String = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&answered).fetch_one(&app.db).await.unwrap();
        let external = db::ulid();
        let status = if fallback_closed { "completed_fallback" } else { "in_flight" };
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'external',?,'ok',?)",
        )
        .bind(&external).bind(&conv).bind(&run_id).bind(status).bind(db::now()).execute(&app.db).await.unwrap();
        if fallback_closed {
            sqlx::query("UPDATE turns SET completed_at=? WHERE id=?").bind(db::now()).bind(&external).execute(&app.db).await.unwrap();
            sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'assistant','⏺ 畫面備援','terminal_fallback',?)")
                .bind(db::ulid()).bind(&conv).bind(&external).bind(db::now()).execute(&app.db).await.unwrap();
        }
        (bot_id, conv, external)
    }

    fn claude_stop_for(bot_id: &str, extra: Value) -> HookBody {
        let mut payload = json!({
            "hook_event_name": "Stop",
            "session_id": format!("s-{}", db::ulid()),
            "prompt_id": db::ulid(),
            "transcript_path": "/Users/m4p/.claude/projects/x/s.jsonl",
            "last_assistant_message": "背景工作做完了",
        });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            payload[k] = v;
        }
        HookBody { bot_id: bot_id.to_string(), provider: "claude".into(), payload, received_at: None, truncated: false, run_id: None }
    }

    async fn user_texts_on(app: &Arc<App>, turn_id: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='user' ORDER BY created_at")
            .bind(turn_id).fetch_all(&app.db).await.unwrap()
    }

    /// issue #754：背景工作完成喚醒的外部回合不能掛上一句使用者訊息；但使用者真的在 pane 裡重送同一句時要照記。
    /// 遠端 transcript 在 agm-host 讀不到，起點由遠端 hook.sh 從本機 transcript 讀出、跟 Stop 一起帶來（`agm_origin_kind`）：
    /// `human` 才補記（`begin_external_turn` 開回合時為了擋喚醒那一輪而沒存的回音），其他起點或沒證據都不存。
    #[tokio::test]
    async fn a_resent_prompt_is_stored_but_a_background_wake_up_is_not() {
        let env = tt::env().await;
        let app = env.app.clone();
        for (origin, expect_stored) in [(Some("human"), true), (Some("task-notification"), false), (None, false)] {
            let (bot_id, _conv, external) = an_external_turn_after_an_answered_prompt(&app, &env.project_id, false).await;
            let mut extra = json!({"agm_user_text": "現在部 demo"});
            if let Some(k) = origin {
                extra["agm_origin_kind"] = json!(k);
            }
            process(&app, &claude_stop_for(&bot_id, extra)).await.unwrap();
            let stored = user_texts_on(&app, &external).await;
            assert_eq!(stored, if expect_stored { vec!["現在部 demo".to_string()] } else { vec![] }, "origin={origin:?}");
            assert_eq!(turn_row(&app, &external).await.status, "completed", "origin={origin:?}");
        }
    }

    /// 同一題，Stop 比終端備援晚到（#753 的競態）：備援已把外部回合收掉，使用者那一句一樣要補上。
    #[tokio::test]
    async fn a_late_stop_after_the_fallback_still_stores_a_resent_prompt() {
        let env = tt::env().await;
        let app = env.app.clone();
        for (origin, expect_stored) in [("human", true), ("task-notification", false)] {
            let (bot_id, _conv, external) = an_external_turn_after_an_answered_prompt(&app, &env.project_id, true).await;
            let extra = json!({"agm_user_text": "現在部 demo", "agm_origin_kind": origin});
            process(&app, &claude_stop_for(&bot_id, extra)).await.unwrap();
            let stored = user_texts_on(&app, &external).await;
            assert_eq!(stored, if expect_stored { vec!["現在部 demo".to_string()] } else { vec![] }, "origin={origin}");
        }
    }

    /// #831：Stop 比備援晚到、補記使用者那一句時，數完回合上的使用者訊息、還沒寫的那一瞬，一個不相干的 writer commit 了一筆。
    /// deferred 交易這時升級寫鎖直接 517，那一句補不上、整個 hook 回錯；寫鎖從讀之前就拿著，照樣補上一次。
    #[tokio::test]
    async fn an_unrelated_writer_between_the_user_count_and_the_insert_does_not_lose_the_resent_prompt() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, external) = an_external_turn_after_an_answered_prompt(&app, &env.project_id, true).await;
        let other = tt::arm_app_foreign_writer(&app, "resent_prompt_after_count_read", &external);

        let extra = json!({"agm_user_text": "現在部 demo", "agm_origin_kind": "human"});
        process(&app, &claude_stop_for(&bot_id, extra)).await.expect("an unrelated writer must not make the Stop fail");

        assert_eq!(*other.lock().unwrap(), Some(false), "the insert holds the write lock from its read on; the other writer waits");
        assert_eq!(user_texts_on(&app, &external).await, vec!["現在部 demo".to_string()]);
    }

    /// hook 送來的 transcript 路徑要在 bot 自己身分的 `projects/` 底下才算數（`transcript_read`）：把這顆 bot 的 `CLAUDE_CONFIG_DIR` 指到 `root`，
    /// 回傳 `root/projects/-t/<name>`（目錄已建好）。
    async fn own_projects_file(app: &Arc<App>, bot_id: &str, root: &std::path::Path, name: &str) -> std::path::PathBuf {
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CLAUDE_CONFIG_DIR": root.to_string_lossy()}).to_string())
            .bind(bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        let dir = root.join("projects/-t");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// 本機 bot：transcript 在這台，daemon 自己讀 `origin.kind`，不需要 hook.sh 帶。
    #[tokio::test]
    async fn a_local_transcript_decides_whether_the_stop_stores_a_resent_prompt() {
        let env = tt::env().await;
        let app = env.app.clone();
        let line = |origin: Option<&str>, text: &str| {
            let mut v = json!({"type": "user", "message": {"role": "user", "content": text}});
            if let Some(k) = origin {
                v["origin"] = json!({"kind": k});
            }
            format!("{v}\n")
        };
        for (origin, expect_stored) in [("human", true), ("task-notification", false)] {
            let (bot_id, _conv, external) = an_external_turn_after_an_answered_prompt(&app, &env.project_id, false).await;
            let path = own_projects_file(&app, &bot_id, &env.dir, &format!("t-{origin}.jsonl")).await;
            std::fs::write(&path, [line(Some("human"), "現在部 demo"), line(Some(origin), if origin == "human" { "現在部 demo" } else { "<task-notification/>" })].concat()).unwrap();
            process(&app, &claude_stop_for(&bot_id, json!({"transcript_path": path.to_string_lossy()}))).await.unwrap();
            let stored = user_texts_on(&app, &external).await;
            if expect_stored {
                assert_eq!(stored, vec!["現在部 demo".to_string()], "origin={origin}");
            } else {
                assert!(stored.is_empty(), "喚醒那一輪不掛上一句：{stored:?}");
            }
        }
    }

    /// issue #634: a single Bash `PostToolUse` hook records every herdr response against its caller.
    #[tokio::test]
    async fn a_bash_loop_spawn_records_every_hint() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let stdout = format!(
            "{}\n{}\n{}\n",
            json!({"id": "cli:agent:start", "result": {"agent": {"pane_id": "w1:p2"}}}),
            json!({"id": "cli:pane:split", "result": {"pane": {"pane_id": "w1:p3"}}}),
            json!({"id": "cli:agent:start", "result": {"agent": {"pane_id": "w1:p4"}}}),
        );

        process(
            &env.app,
            &stop_failure(
                &bot.id,
                json!({
                    "hook_event_name": "PostToolUse",
                    "tool_name": "Bash",
                    "tool_input": {"command": "for n in a b c; do herdr agent start $n --kind claude; done"},
                    "tool_response": {"stdout": stdout, "stderr": ""},
                }),
            ),
        )
        .await
        .unwrap();

        let recorded: Vec<String> = sqlx::query_scalar("SELECT pane_id FROM spawn_hints WHERE bot_id = ? ORDER BY pane_id")
            .bind(&bot.id)
            .fetch_all(&env.app.db)
            .await
            .unwrap();
        assert_eq!(recorded, ["w1:p2", "w1:p3", "w1:p4"]);
    }

    fn ask_messages_sql() -> &'static str {
        "SELECT content FROM messages WHERE id LIKE 'ask:%' ORDER BY created_at, rowid"
    }

    /// 2026-10-02 wits-pro：問了三題、答完，網頁看不到。回合結束時從 transcript 補進對話；同一筆 Stop 重送（hook 重送、spool replay）
    /// 與 PostToolUse 同一個 `tool_use_id` 之後到，都不能多一則。
    #[tokio::test]
    async fn a_finished_ask_user_question_lands_in_the_conversation_exactly_once() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "做 WA 流程").await;
        let at = |secs: i64| (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let qs = json!([
            {"question": "拋單怎麼處理？", "header": "拋單倉庫", "options": [{"label": "保留拋單"}, {"label": "拿掉拋單"}]},
            {"question": "go API 放哪？", "header": "go API", "options": [{"label": "stock-server"}]},
        ]);
        let ask = |id: &str| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion", "input": {"questions": qs}}]}});
        let result = |id: &str, secs: i64, body: Value| {
            let mut v = json!({"type": "user", "timestamp": at(secs), "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "…"}]}});
            v["toolUseResult"] = body;
            v
        };
        let path = own_projects_file(&app, &bot_id, &env.dir, "asks.jsonl").await;
        let lines = [
            // 這一回合開始之前就答完的（fork／resume 帶來的舊提問）：不記。
            ask("old"),
            result("old", -3600, json!({"questions": qs, "answers": {"拋單怎麼處理？": "舊的"}})),
            ask("t1"),
            result("t1", 1, json!({"questions": qs, "answers": {"拋單怎麼處理？": "拿掉拋單", "go API 放哪？": "自訂：gateway"}})),
            ask("t2"),
            // 取消（Cancel／Esc）：content 是固定那句。
            json!({"type": "user", "timestamp": at(2), "message": {"content": [{"type": "tool_result", "tool_use_id": "t2", "content": "The user did not answer the questions."}]}, "toolUseResult": {"questions": qs}}),
        ];
        std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();

        let stop = claude_stop_for(&bot_id, json!({"transcript_path": path.to_string_lossy()}));
        process(&app, &stop).await.unwrap();
        process(&app, &stop).await.unwrap(); // 重送
        let rows: Vec<String> = sqlx::query_scalar(ask_messages_sql()).fetch_all(&app.db).await.unwrap();
        assert_eq!(rows.len(), 2, "t1 與 t2 各一則，舊的不補：{rows:?}");
        let first: Value = serde_json::from_str(&rows[0]).unwrap();
        assert_eq!((first["type"].as_str(), first["tool_use_id"].as_str(), first["answered"].as_bool()), (Some("ask_answers"), Some("t1"), Some(true)));
        assert_eq!(first["items"][0], json!({"header": "拋單倉庫", "question": "拋單怎麼處理？", "answer": "拿掉拋單"}));
        assert_eq!(first["items"][1]["answer"], "自訂：gateway");
        let second: Value = serde_json::from_str(&rows[1]).unwrap();
        assert_eq!(second["answered"], json!(false), "取消＝沒有回答");
        assert!(second["items"].as_array().unwrap().iter().all(|i| i["answer"].is_null()));
        // 掛在這一回合、是系統訊息，不是使用者打的字，也沒有新開任何回合。
        let (role, source, turn): (String, String, Option<String>) =
            sqlx::query_as("SELECT role, source, turn_id FROM messages WHERE id LIKE 'ask:%' ORDER BY created_at LIMIT 1").fetch_one(&app.db).await.unwrap();
        assert_eq!((role.as_str(), source.as_str(), turn.as_deref()), ("system", "system", Some(turn_id.as_str())));
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns WHERE conversation_id = ?").bind(&_conv).fetch_one(&app.db).await.unwrap(), 1);

        // 同一個 tool_use_id 的 PostToolUse 晚到：一樣只有一列。
        let post = stop_failure(&bot_id, json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
            "tool_input": {"questions": qs}, "tool_response": {"questions": qs, "answers": {"拋單怎麼處理？": "拿掉拋單"}}}));
        process(&app, &post).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM messages WHERE id LIKE 'ask:%'").fetch_one(&app.db).await.unwrap(), 2);
    }

    /// 使用者在終端打字開的回合（外部回合）要等 Stop 才建立：問答要掛在那一個新回合上，不是前一個已經收掉的。
    #[tokio::test]
    async fn an_ask_answered_in_an_external_turn_belongs_to_that_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, old_turn) = unknown_turn(&app, &env.project_id, "claude", "舊的一句").await;
        let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET status='completed', delivery='ok', created_at=?, completed_at=? WHERE id=?")
            .bind(&long_ago).bind(&long_ago).bind(&old_turn).execute(&app.db).await.unwrap();
        let qs = json!([{"question": "要不要？", "header": "確認", "options": []}]);
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let path = own_projects_file(&app, &bot_id, &env.dir, "ext.jsonl").await;
        std::fs::write(&path, format!("{}\n{}\n",
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "AskUserQuestion", "input": {"questions": qs}}]}}),
            json!({"type": "user", "timestamp": now, "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "x"}]}, "toolUseResult": {"questions": qs, "answers": {"要不要？": "要"}}}),
        )).unwrap();
        process(&app, &claude_stop_for(&bot_id, json!({"transcript_path": path.to_string_lossy()}))).await.unwrap();
        let on: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id LIKE 'ask:%'").fetch_optional(&app.db).await.unwrap();
        let newest: String = sqlx::query_scalar("SELECT id FROM turns WHERE conversation_id=? ORDER BY created_at DESC, rowid DESC LIMIT 1").bind(&conv).fetch_one(&app.db).await.unwrap();
        assert_ne!(newest, old_turn, "Stop 開了一個外部回合");
        assert_eq!(on.as_deref(), Some(newest.as_str()), "問答掛在新的外部回合上");
    }

    #[tokio::test]
    async fn post_tool_use_does_not_pin_an_external_question_to_the_previous_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, old_turn) = unknown_turn(&app, &env.project_id, "claude", "舊的一句").await;
        let long_ago = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET status='completed', delivery='ok', created_at=?, completed_at=? WHERE id=?")
            .bind(&long_ago)
            .bind(&long_ago)
            .bind(&old_turn)
            .execute(&app.db)
            .await
            .unwrap();

        let qs = json!([{"question": "要不要？", "header": "確認", "options": []}]);
        let post = stop_failure(&bot_id, json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "AskUserQuestion",
            "tool_use_id": "external-tool-use",
            "tool_input": {"questions": qs},
            "tool_response": {"answers": {"要不要？": "要"}}
        }));
        process(&app, &post).await.unwrap();
        let early: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id LIKE 'ask:%'")
            .fetch_optional(&app.db)
            .await
            .unwrap();
        assert!(early.is_none(), "沒有當前 in-flight turn 時先不要掛到舊回合：{early:?}");

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let path = own_projects_file(&app, &bot_id, &env.dir, "external-ask.jsonl").await;
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "external-tool-use", "name": "AskUserQuestion", "input": {"questions": qs}}]}}),
                json!({"type": "user", "timestamp": now, "message": {"content": [{"type": "tool_result", "tool_use_id": "external-tool-use", "content": "answered"}]}, "toolUseResult": {"questions": qs, "answers": {"要不要？": "要"}}}),
            ),
        )
        .unwrap();
        process(&app, &claude_stop_for(&bot_id, json!({"transcript_path": path.to_string_lossy()})))
            .await
            .unwrap();

        let on: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id LIKE 'ask:%'")
            .fetch_optional(&app.db)
            .await
            .unwrap();
        let newest: String = sqlx::query_scalar("SELECT id FROM turns WHERE conversation_id=? ORDER BY created_at DESC, rowid DESC LIMIT 1")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_ne!(newest, old_turn, "Stop 建立了外部回合");
        assert_eq!(on.as_deref(), Some(newest.as_str()), "PostToolUse 與 Stop 冪等合併時要掛在外部回合");
    }

    /// 被使用者中斷的回合沒有 Stop：PostToolUse 當下就記；遠端 bot 的 Stop 帶 `agm_asks`（`hook.sh` 從本機 transcript 讀的），
    /// 讀不到檔案也照記。認不出答案的 PostToolUse 不記（不能先記一筆「沒有回答」把正確的擋掉）。
    #[tokio::test]
    async fn post_tool_use_and_the_carried_asks_record_without_reading_a_transcript() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, _turn) = unknown_turn(&app, &env.project_id, "claude", "做 WA 流程").await;
        let qs = json!([{"question": "拋單怎麼處理？", "header": "拋單倉庫", "options": []}]);

        let unreadable = stop_failure(&bot_id, json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion", "tool_use_id": "t0",
            "tool_input": {"questions": qs}, "tool_response": "something claude changed"}));
        process(&app, &unreadable).await.unwrap();
        assert!(sqlx::query_scalar::<_, String>(ask_messages_sql()).fetch_all(&app.db).await.unwrap().is_empty());

        let live = stop_failure(&bot_id, json!({"hook_event_name": "PostToolUse", "tool_name": "AskUserQuestion", "tool_use_id": "t1",
            "tool_input": {"questions": qs}, "tool_response": {"answers": {"拋單怎麼處理？": "拿掉拋單"}}}));
        process(&app, &live).await.unwrap();
        process(&app, &live).await.unwrap();
        let rows: Vec<String> = sqlx::query_scalar(ask_messages_sql()).fetch_all(&app.db).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(serde_json::from_str::<Value>(&rows[0]).unwrap()["items"][0]["answer"], "拿掉拋單");

        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let carried = claude_stop_for(&bot_id, json!({"transcript_path": "/Users/m4p/none.jsonl", "agm_asks": [
            {"id": "t1", "at": at, "items": [{"header": "拋單倉庫", "question": "拋單怎麼處理？", "answer": "拿掉拋單"}]},
            {"id": "t9", "at": at, "items": [{"question": "要不要？", "answer": null}]},
        ]}));
        process(&app, &carried).await.unwrap();
        let rows: Vec<String> = sqlx::query_scalar(ask_messages_sql()).fetch_all(&app.db).await.unwrap();
        assert_eq!(rows.len(), 2, "t1 已記過不重複，t9 是新的：{rows:?}");
    }

    /// claude 的 Stop 沒帶使用者訊息：從 transcript 尾巴讀。對不上一樣不認領；讀不到（沒有 transcript）就照舊認領。
    #[tokio::test]
    async fn claude_reads_the_prompt_from_the_transcript_before_claiming_an_unknown_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "跑一次測試").await;
        let transcript = own_projects_file(&app, &bot_id, &app.data_dir, &format!("t-{}.jsonl", db::ulid())).await;
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                json!({"type": "user", "message": {"role": "user", "content": "跑一次測試"}}),
                json!({"type": "user", "message": {"role": "user", "content": "算了，先幫我看 README"}}),
            ),
        )
        .unwrap();
        let stop = |sid: &str| HookBody {
            bot_id: bot_id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": sid,
                "prompt_id": db::ulid(),
                "transcript_path": transcript.to_string_lossy(),
                "last_assistant_message": "hook reply",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        process(&app, &stop("s1")).await.unwrap();
        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turn.status, "in_flight", "transcript 最後一則是別句：不認領");

        assert!(!answers_another_prompt(Some("跑一次測試"), None), "讀不到就不下判斷");
        assert!(!answers_another_prompt(None, Some("x")));
        assert!(answers_another_prompt(Some("跑一次測試"), Some("算了")));
    }

    /// 使用者 2026-09-28：回合中「補充」的那句被 claude 記成 transcript 最後一則使用者訊息。記下的補充要算成這一回合的 prompt：
    /// 照常認領（unknown 升 ok）、不開外部回合、也不多一則重複的使用者訊息。
    #[tokio::test]
    async fn a_supplement_typed_mid_turn_does_not_steal_the_turns_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "跑一次測試").await;
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user','跑一次測試','web',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&turn_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.live_pane("pane-1", tt::LivePane::default());
        let supp = crate::lifecycle::send_text_recorded(&app, &bot_id, "順便把 log 也貼上來", true, None, true)
            .await
            .unwrap()
            .expect("記成這一回合的訊息");
        assert_eq!(supp.turn_id.as_deref(), Some(turn_id.as_str()));

        let transcript = own_projects_file(&app, &bot_id, &app.data_dir, &format!("t-{}.jsonl", db::ulid())).await;
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                json!({"type": "user", "message": {"role": "user", "content": "跑一次測試"}}),
                json!({"type": "user", "message": {"role": "user", "content": "順便把 log 也貼上來"}}),
            ),
        )
        .unwrap();
        let stop = HookBody {
            bot_id: bot_id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "s-supp",
                "prompt_id": db::ulid(),
                "transcript_path": transcript.to_string_lossy(),
                "last_assistant_message": "測試過了，log 在這",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        process(&app, &stop).await.unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("completed", "ok"), "補充之後照常認領");
        let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND origin='external'")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(external, 0, "不開外部回合");
        let rows: Vec<(String, String, Option<String>)> =
            sqlx::query_as("SELECT role, content, turn_id FROM messages WHERE conversation_id=? ORDER BY created_at, rowid")
                .bind(&conv)
                .fetch_all(&app.db)
                .await
                .unwrap();
        let t = Some(turn_id.clone());
        assert_eq!(
            rows,
            vec![
                ("user".to_string(), "跑一次測試".to_string(), t.clone()),
                ("user".to_string(), "順便把 log 也貼上來".to_string(), t.clone()),
                ("assistant".to_string(), "測試過了，log 在這".to_string(), t),
            ],
            "兩則使用者訊息各一則、回覆掛在原回合"
        );
    }

    /// 沒記下的一句（使用者在終端手打）照舊分得出來：補充只放寬「這一回合記過的字」。
    #[tokio::test]
    async fn only_recorded_supplements_count_as_the_turns_prompt() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (_bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "跑一次測試").await;
        let sent = with_supplements(&app, &turn_id, Some("跑一次測試")).await.unwrap();
        assert!(answers_none_of(sent.as_deref(), Some("順便把 log 也貼上來")));
        assert!(!answers_none_of(sent.as_deref(), Some("跑一次測試")));
        assert!(!answers_none_of(None, Some("x")), "沒有 prompt 不下判斷");
        assert!(!answers_none_of(sent.as_deref(), None), "讀不到使用者訊息不下判斷");
    }

    /// 2026-09-27 wits-pro：貼上的 prompt 帶 Tab，送進 TUI 時 Tab 被吃掉；只在原文有 Tab 時試去 Tab 候選。
    #[test]
    fn a_prompt_whose_tab_the_tui_swallowed_is_still_the_same_prompt() {
        let sent = "條碼列印・進貨單\t每張 INBSHIP 一個批號 => 這樣一批最多能夠幾個item";
        let seen = "條碼列印・進貨單每張 INBSHIP 一個批號 => 這樣一批最多能夠幾個item";
        assert!(!answers_another_prompt(Some(sent), Some(seen)));
        assert!(!hook_user_is_new(&[sent.to_string()], seen), "同一句不再多一則使用者訊息");
        assert!(answers_another_prompt(Some(sent), Some("另一件事")), "真的是另一句照樣分得出來");
    }

    /// #217：`unknown` 那一則在 transcript 裡被 CLI 包成 `<pasted_content>`（真 transcript，2026-09-19 實測）還是同一則，照樣認領。
    /// prompt 本身寫著字面的 `<pasted_content …>` 時 CLI 還把它跳脫成 `<\…`，兩邊去空白後互不包含——以前被當成「回答的是別句」：
    /// 答案記到一筆外部回合、那一則一直掛在 unknown。
    #[tokio::test]
    async fn an_unknown_turn_whose_prompt_the_cli_wrapped_as_pasted_content_is_still_claimed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let prompt = "這段文字裡有字面的 <pasted_content id=\"1234\">x</pasted_content id=\"1234\"> 標籤，請只回覆 OK 兩個字母。";
        let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", prompt).await;
        let transcript = own_projects_file(&app, &bot_id, &app.data_dir, &format!("t-{}.jsonl", db::ulid())).await;
        let log: Vec<&str> = include_str!("lifecycle/fixtures/claude_2.1.278_pasted_content.jsonl").lines().collect();
        assert!(log[8].contains(r#"<\\pasted_content id=\"1234\">"#), "fixture 這一列是 CLI 跳脫過又包起來的");
        std::fs::write(&transcript, log[8..].join("\n") + "\n").unwrap();
        let stop = HookBody {
            bot_id: bot_id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "2c0bdae3-b75f-4dad-a24c-ed9b751cb14d",
                "prompt_id": db::ulid(),
                "transcript_path": transcript.to_string_lossy(),
                "last_assistant_message": "OK",
            }),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        process(&app, &stop).await.unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("completed", "ok"), "同一則：認領並升成 ok");
        let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND origin='external'")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(external, 0, "沒有多一筆外部回合");
    }

    #[tokio::test]
    async fn stop_hook_resolves_unknown_delivery_when_it_completes_a_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'hook-unknown','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conversation_id = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','working','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','unknown',?)",
        )
        .bind(&turn_id)
        .bind(&conversation_id)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        process(
            &app,
            &HookBody {
                bot_id,
                provider: "claude".into(),
                payload: json!({
                    "hook_event_name": "Stop",
                    "session_id": "native-session",
                    "prompt_id": "native-turn",
                    "last_assistant_message": "hook reply",
                }),
                received_at: None,
                truncated: false,
                run_id: None,
            },
        )
        .await
        .unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turn.status, "completed");
        assert_eq!(turn.delivery, "ok");
        let assistant_messages: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'",
        )
        .bind(&turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(assistant_messages, 1);
    }

    /// Hook vs. fallback race (review 2026-09-12 #5). The BEFORE UPDATE trigger stands in for the
    /// fallback winning the CAS. Losing the CAS is evidence the hook belongs to that very turn (8e7ef5fa),
    /// so its transcript text replaces the fallback's reply in place — still one answer, not two.
    #[tokio::test]
    async fn a_hook_that_loses_the_cas_to_the_fallback_adds_no_second_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'hook-race','claude','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let fallback = crate::lifecycle::insert_message(&app, &conv, Some(&turn_id), "assistant", "from the pane", "terminal_fallback", true, None)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER fallback_wins BEFORE UPDATE OF status ON turns
             WHEN OLD.status='in_flight' AND NEW.status='completed'
             BEGIN
               UPDATE turns SET status='completed_fallback', completed_at=NEW.completed_at WHERE id=OLD.id;
               SELECT RAISE(IGNORE);
             END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        process(
            &app,
            &HookBody {
                bot_id,
                provider: "claude".into(),
                payload: json!({
                    "hook_event_name": "Stop",
                    "session_id": "native-session",
                    "prompt_id": "native-turn",
                    "last_assistant_message": "from the hook",
                }),
                received_at: None,
                truncated: false,
                run_id: None,
            },
        )
        .await
        .unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turn.status, "completed", "the hook's reply supersedes the fallback's claim");
        assert_eq!(turn.native_turn_id.as_deref(), Some("native-turn"), "the ids land on that turn, so a retry dedups");
        assert_eq!(turn.native_session_id.as_deref(), Some("native-session"));
        let replies: Vec<(String, String, String, i64)> =
            sqlx::query_as("SELECT id, source, content, incomplete FROM messages WHERE turn_id=? AND role='assistant'")
                .bind(&turn_id)
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(
            replies,
            [(fallback.id.clone(), "hook".to_string(), "from the hook".to_string(), 0)],
            "one answer, not two: the fallback's row is overwritten in place"
        );
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 1, "and no external turn was opened for the dropped payload");
    }

    /// #76 item 3/8：使用者 interrupt 跟 reconcile 判定 run 已消失，最終都靠 `fail_in_flight`
    /// 把 in-flight 回合標 failed——這是兩條呼叫端共用的那支函式。這支測的是標完之後的窄窗：
    /// 舊 session 的 Stop hook 才姍姍來遲。`in_flight_turn` 只認 `status='in_flight'`，所以已經
    /// failed 的回合救不回來；遲到的回覆改記成一筆新的 external turn，不會憑空消失、也不會接到
    /// 已經結案的回合上（跟 review 2026-09-12 #5 同一個 CAS 精神，這裡換一個把回合收掉的呼叫端）。
    #[tokio::test]
    async fn a_late_stop_hook_does_not_resurrect_a_turn_that_interrupt_already_failed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "interrupt-race").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        // `interrupt_bot` 本身要碰 herdr pane；直接呼叫它跟 `mark_run_exited` 共用的那支收尾函式，
        // 單獨驗證 DB 這一段的不變量（herdr 那半邊 `interrupt_bot`/`abort_turns` 自己的測試已經蓋到）。
        lifecycle::fail_in_flight(&app, &run_id, "interrupted by user").await.unwrap();
        let before = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(before.status, "failed");

        process(
            &app,
            &HookBody {
                bot_id: bot.id.clone(),
                provider: "claude".into(),
                payload: json!({
                    "hook_event_name": "Stop",
                    "session_id": "native-session",
                    "prompt_id": "native-turn",
                    "last_assistant_message": "遲到的回覆",
                }),
                received_at: None,
                truncated: false,
                run_id: None,
            },
        )
        .await
        .unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turn.status, "failed", "已經被 fail_in_flight 收掉的回合不能被遲到的 hook 救回來");
        let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, 0, "遲到的回覆不會接到已經結案的回合上");
        let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND origin='external'")
            .bind(&conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(external, 1, "回覆不會憑空消失，改記成一筆外部回合");
    }

    /// 同一個不變量，換 reconcile 判定 run 已消失那條路（`mark_run_exited`）：run 整個轉成
    /// `exited`，之後 `db::active_run` 找不到它，process_locked 的 `run` 變 `None`——跟上一支
    /// 是不同的程式分支，要分開測。
    #[tokio::test]
    async fn a_late_stop_hook_does_not_resurrect_a_turn_whose_run_reconcile_already_exited() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "reconcile-race").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        lifecycle::mark_run_exited(&app, &run_id, "agent not found during reconcile").await;
        let run = sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(run.state, "exited");
        let before = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(before.status, "failed");

        process(
            &app,
            &HookBody {
                bot_id: bot.id.clone(),
                provider: "claude".into(),
                payload: json!({
                    "hook_event_name": "Stop",
                    "session_id": "native-session",
                    "prompt_id": "native-turn",
                    "last_assistant_message": "遲到的回覆",
                }),
                received_at: None,
                truncated: false,
                run_id: None,
            },
        )
        .await
        .unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turn.status, "failed", "run 已經 exited，舊 session 的 hook 不能把它的回合救回來");
        let replies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, 0, "遲到的回覆不會接到已經結案的回合上");
    }

    /// Step 4 must find the `external` turn, or step 5 inserts a duplicate.
    #[tokio::test]
    async fn stop_hook_finds_the_open_external_turn() {
        let (_tmp, pool, run, conv) = fixture().await;
        let now = db::now();
        sqlx::query(
            "INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at)
             VALUES ('t',?,?,'external','in_flight','ok',?)",
        )
        .bind(&conv)
        .bind(&run)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        let found = db::in_flight_turn(&pool, &run).await.unwrap().expect("step 4 must claim the external turn");
        assert_eq!(found.id, "t");
        assert_eq!(found.origin, "external");
        // `delivery` must be `ok` or the §4.3 terminal fallback refuses to close the turn.
        assert_eq!(found.delivery, "ok");

        // Afterwards nothing is in flight, so a retried hook dedups.
        sqlx::query("UPDATE turns SET status='completed', completed_at=?, native_turn_id='u' WHERE id=? AND status='in_flight'")
            .bind(&now)
            .bind("t")
            .execute(&pool)
            .await
            .unwrap();
        assert!(db::in_flight_turn(&pool, &run).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn turn_user_messages_returns_the_scraped_echo() {
        let (_tmp, pool, run, conv) = fixture().await;
        let now = db::now();
        sqlx::query(
            "INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,created_at)
             VALUES ('t',?,?,'external','in_flight','ok',?)",
        )
        .bind(&conv)
        .bind(&run)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at)
             VALUES ('m',?, 't','user','echo 1','hook',?)",
        )
        .bind(&conv)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        let have = db::turn_user_messages(&pool, "t").await.unwrap();
        assert_eq!(have, vec!["echo 1".to_string()]);
        assert!(!hook_user_is_new(&have, "echo 1"));
        assert!(hook_user_is_new(&have, "echo 2"));
    }
}

/// issue #70：`200` 必須代表「已經耐久收下」。
#[cfg(all(test, feature = "daemon-test-harness"))]
mod durable_handoff_tests {
    use super::*;
    use crate::testing::{claude_bot, env, fake_run, restart_app, Env};

    fn stop_body(bot_id: &str, prompt_id: &str) -> HookBody {
        HookBody {
            bot_id: bot_id.into(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "Stop",
                "session_id": "sess-1",
                "prompt_id": prompt_id,
                "last_assistant_message": "做完了",
            }),
            received_at: Some("2026-09-17T12:00:00.000Z".into()),
            truncated: false,
            run_id: None,
        }
    }

    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Token", token.parse().unwrap());
        h
    }

    async fn inbox_rows(e: &Env) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM hook_events").fetch_one(&e.app.db).await.unwrap()
    }

    /// 寫不進收件匣就**不准回 200**。回 200 再掉事件是無聲的資料遺失；回 503 送端會把同一份 body
    /// 寫進 hook-spool.jsonl，replay 補得回來。
    #[tokio::test]
    async fn a_hook_that_cannot_be_persisted_is_not_answered_with_200() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
        // 收件匣壞掉（磁碟滿、schema 沒跟上……都是同一種下場）。
        sqlx::query("DROP TABLE hook_events").execute(&e.app.db).await.unwrap();

        let (code, _) = receive(
            State(e.app.clone()),
            Path("claude".into()),
            headers("tok"),
            Json(stop_body(&bot.id, "p1")),
        )
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "寫不進去就不是 200");
        assert!(!code.is_success(), "送端要看得出來該 spool");
    }

    /// #304：沒帶對 token 的人分不出「有過這顆 bot」與「沒有」——不存在、已刪、token 不對都是同一個 401；
    /// 410 只給 token 正確者（被刪 bot 還活著的 agent 要知道自己該停）。
    #[tokio::test]
    async fn the_hook_endpoint_does_not_reveal_whether_a_bot_id_exists() {
        let e = env().await;
        let live = claude_bot(&e.app, &e.project_id, "live").await;
        let gone = claude_bot(&e.app, &e.project_id, "gone").await;
        sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&gone.id).execute(&e.app.db).await.unwrap();
        let mut seen = Vec::new();
        for (id, token) in [(&live.id, "wrong"), (&gone.id, "wrong"), (&gone.id, ""), (&"no-such-bot".to_string(), "wrong")] {
            let (code, Json(body)) =
                receive(State(e.app.clone()), Path("claude".into()), headers(token), Json(stop_body(id, "p1"))).await;
            seen.push((code, body));
        }
        assert!(seen.iter().all(|s| *s == seen[0]), "沒有有效 token：存在、已刪、不存在的回應要一模一樣：{seen:?}");
        assert_eq!(seen[0].0, StatusCode::UNAUTHORIZED);
        let (code, _) =
            receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(stop_body(&gone.id, "p1"))).await;
        assert_eq!(code, StatusCode::GONE, "token 正確的已刪 bot 仍是 410");
        assert_eq!(inbox_rows(&e).await, 0, "任何一種都不能寫進收件匣");
    }

    /// 收下了才 200；而且 200 的當下那一列已經在 DB 裡（不是「待會背景寫」）。
    #[tokio::test]
    async fn a_persisted_hook_is_in_the_database_before_the_200_comes_back() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
        let (code, _) =
            receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(stop_body(&bot.id, "p1"))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(inbox_rows(&e).await, 1, "200 回來的時候列已經在了");
        let pending = crate::hook_inbox::pending(&e.app.db, &db::now(), 10).await.unwrap();
        assert_eq!(pending.len(), 1, "還沒處理，但已經耐久收下");
    }

    /// 重送同一則 hook 不會變成兩列、也不會變成第二次完成。
    #[tokio::test]
    async fn the_same_hook_sent_twice_does_not_become_two_events() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
        for _ in 0..3 {
            let (code, _) = receive(
                State(e.app.clone()),
                Path("claude".into()),
                headers("tok"),
                Json(stop_body(&bot.id, "p1")),
            )
            .await;
            assert_eq!(code, StatusCode::OK, "重送照樣是 200：那則事件的確已經收下了");
        }
        assert_eq!(inbox_rows(&e).await, 1, "三次重送只有一列");

        // 處理過一輪之後再重送，也不會多出一列（去重看的是事件身分，不是有沒有處理過）。
        crate::runners::hook_inbox::drain_once(&e.app).await.unwrap();
        let (code, _) =
            receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(stop_body(&bot.id, "p1"))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(inbox_rows(&e).await, 1, "處理完之後重送還是同一列");
    }

    /// 兩則**不同**的 hook 不會被去重吃掉。
    #[tokio::test]
    async fn two_different_hooks_are_both_accepted() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
        for pid in ["p1", "p2"] {
            let (code, _) = receive(
                State(e.app.clone()),
                Path("claude".into()),
                headers("tok"),
                Json(stop_body(&bot.id, pid)),
            )
            .await;
            assert_eq!(code, StatusCode::OK);
        }
        assert_eq!(inbox_rows(&e).await, 2);
    }

    /// ACK 之後、處理之前 daemon 掛掉：重啟仍然處理得到那則事件（驗收第 2、4 條）。
    ///
    /// 「重啟」在這裡就是 `restart_app`（同一個資料目錄開一顆新的 App）＋ worker 開工做的第一件事
    /// （`drain_once`）——正式路徑上 `spawn_worker` 也是先 drain 再等。
    #[tokio::test]
    async fn an_accepted_hook_survives_a_restart_before_it_is_processed() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
        let run = fake_run(&e.app, &bot.id).await;

        let (code, _) =
            receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(stop_body(&bot.id, "p1"))).await;
        assert_eq!(code, StatusCode::OK);
        // 沒有人處理它就「掛掉」了。
        assert_eq!(crate::hook_inbox::pending(&e.app.db, &db::now(), 10).await.unwrap().len(), 1);

        let restarted = restart_app(&e).await;
        let done = crate::runners::hook_inbox::drain_once(&restarted).await.unwrap();
        assert_eq!(done, 1, "重啟後補處理");
        assert!(
            crate::hook_inbox::pending(&restarted.db, &db::now(), 10).await.unwrap().is_empty(),
            "處理完就不在待辦裡了"
        );
        // 事件真的走到了 §6.7（session id 回填到 run 上）。
        let sid: Option<String> =
            sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run).fetch_one(&restarted.db).await.unwrap();
        assert_eq!(sid.as_deref(), Some("sess-1"), "hook 真的被處理了，不是只被標成完成");
    }

    /// StatusLine 是單槽訊號，刻意不進收件匣（掉一格只是晚一次重繪）。
    #[tokio::test]
    async fn statusline_stays_fire_and_forget() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
        let body = HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "StatusLine", "status_line": "…"}),
            received_at: Some("2026-09-17T12:00:00.000Z".into()),
            truncated: false,
            run_id: None,
        };
        let (code, _) = receive(State(e.app.clone()), Path("claude".into()), headers("tok"), Json(body)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(inbox_rows(&e).await, 0, "重繪訊號不佔佇列");
    }

    /// 壞 token / 已刪除的 bot 在收下之前就被擋掉：收件匣不會被沒有身分的事件灌爆。
    #[tokio::test]
    async fn a_rejected_hook_never_reaches_the_inbox() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "hooky").await;
        let (code, _) = receive(
            State(e.app.clone()),
            Path("claude".into()),
            headers("wrong"),
            Json(stop_body(&bot.id, "p1")),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(inbox_rows(&e).await, 0);
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod resume_tests {
    use super::*;
    use crate::testing::{claude_bot, env, Env};

    async fn fixture() -> (Env, db::Bot, db::Run) {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "resumer").await;
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, started_at, resume_session_id)
             VALUES (?,?,'running','idle','ws-1',?,?,?)",
        )
        .bind(&run_id)
        .bind(&bot.id)
        .bind(format!("pane-{}", bot.id))
        .bind(db::now())
        .bind("native-expected")
        .execute(&e.app.db)
        .await
        .unwrap();
        let run = db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap();
        (e, bot, run)
    }

    async fn remaining(e: &Env, run: &db::Run) -> Option<String> {
        sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap()
    }

    /// claude bot 的 pane 裡跑的 `codex exec` 帶著繼承來的 AM_BOT_ID／token／run id 送 notify：thread-id 不是這顆
    /// bot 的 session，不能拿去對 `resume_session_id`、更不能寫進 native_session_id（2026-09-22 AM-issuers-XH）。
    #[tokio::test]
    async fn a_hook_from_another_provider_cannot_claim_this_bots_session() {
        let (e, bot, run) = fixture().await;
        let body = HookBody {
            bot_id: bot.id.clone(),
            provider: "codex".into(),
            payload: json!({"type": "agent-turn-complete", "thread-id": "01a0b8c6-e80a-7360-87e7-405fd448490f", "turn-id": "t1", "last-assistant-message": "done"}),
            ..serde_json::from_value(json!({"bot_id": ""})).unwrap()
        };
        process_locked(&e.app, &body).await.unwrap();
        assert_eq!(remaining(&e, &run).await.as_deref(), Some("native-expected"), "標記被別種 provider 消耗掉了");
        let (native, outcome): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT native_session_id, resume_outcome FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!((native, outcome), (None, None));
        // 同一顆的 claude hook 照常。
        assert!(provider_matches_kind("claude", "claude") && provider_matches_kind("Claude", "claude") && provider_matches_kind("codex", ""));
        assert!(!provider_matches_kind("codex", "claude") && !provider_matches_kind("grok", "codex"));
    }

    #[tokio::test]
    async fn a_reported_session_id_consumes_the_marker() {
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, Some("native-expected")).await.unwrap();
        assert_eq!(remaining(&e, &run).await, None);
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, Some("native-other")).await.unwrap();
        consume_resume_session(&e.app, &bot, &run, Some("native-other")).await.unwrap();
        assert_eq!(remaining(&e, &run).await, None);
    }

    /// hook 回報的 session 跟預期的不是同一個：CLI 自己默默開了新對話，`--resume` 沒有報任何錯——
    /// 這是最容易被忽略的一種（issue #92）。以前 `context_lost` 只寫 log，這裡要看得到系統訊息。
    #[tokio::test]
    async fn a_session_mismatch_leaves_a_visible_note_not_just_a_log_line() {
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, Some("native-other")).await.unwrap();
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        let note: String = sqlx::query_scalar(
            "SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&conv)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
        assert!(note.contains("不是同一個"), "{note}");
    }

    /// 結論與回報的 session 同一句寫下（issue #107）：不拿 bot 鎖的讀者（`api::started_json`）看到
    /// `verified` 時，一定讀得到是哪一段——不必等 Identity 分支下一句 UPDATE。
    #[tokio::test]
    async fn the_verdict_and_the_reported_session_are_written_in_one_statement() {
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, Some("native-expected")).await.unwrap();
        let (outcome, native): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT resume_outcome, native_session_id FROM runs WHERE id=?").bind(&run.id).fetch_one(&e.app.db).await.unwrap();
        assert_eq!((outcome.as_deref(), native.as_deref()), (Some("verified"), Some("native-expected")));
    }

    #[tokio::test]
    async fn a_hook_without_a_session_id_leaves_the_request_pending() {
        let (e, bot, run) = fixture().await;
        consume_resume_session(&e.app, &bot, &run, None).await.unwrap();
        assert_eq!(remaining(&e, &run).await.as_deref(), Some("native-expected"));
    }
}

/// codex 答完一回合清撞限：清的要是**這顆 bot 自己那把 key**（跟寫入端同一支 `quota_base_for_host`）。
#[cfg(all(test, feature = "daemon-test-harness"))]
mod codex_limit_clear_tests {
    use super::*;
    use crate::quota::{quota_key, LimitHit, Quota};

    fn hit() -> Quota {
        Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: Some(LimitHit { message: "You've hit your usage limit.".into(), until: None, at: db::now(), bucket: None }),
            plan: None,
            updated_at: db::now(),
            source: "codex-limit-hit".into(),
            account: None,
            host: "local".into(),
        }
    }

    /// H1（review 2026-09-16）：`cx2` 有自己的 `CODEX_HOME`，撞限寫在 `codex:cx2`；以前成功回合一律清裸
    /// `codex`，於是 cx2 的撞限（沒寫時間＝永不過期）卡到重啟，反而把預設帳號**真的**撞限清掉。
    #[tokio::test]
    async fn a_codex_turn_clears_the_limit_on_its_own_account_only() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        e.app
            .cfg
            .update(|cfg| {
                cfg.identities = vec![crate::config::IdentityCfg {
                    name: "cx2".into(),
                    kind: "codex".into(),
                    host: None,
                    env: [("CODEX_HOME".to_string(), "$HOME/.codex-cx2".to_string())].into(),
                    args: vec![],
                }];
                Ok(())
            })
            .await
            .unwrap();
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, identity, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,'cx2-bot','codex','cx2','[]',0,1,'tok','user',?)",
        )
        .bind(&bot)
        .bind(&e.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        {
            let mut q = app.quotas.lock().await;
            q.insert(quota_key("local", "codex:cx2"), hit());
            q.insert(quota_key("local", "codex"), hit());
        }
        let body = HookBody {
            bot_id: bot.clone(),
            provider: "codex".into(),
            payload: serde_json::json!({"type": "agent-turn-complete", "thread-id": "t", "turn-id": "u",
                                        "input-messages": ["hi"], "last-assistant-message": "done"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        let _ = process_locked(&app, &body).await;

        let q = app.quotas.lock().await;
        assert!(q.get("codex:cx2").unwrap().limit_hit.is_none(), "cx2 自己的撞限要被清掉");
        assert!(q.get("codex").unwrap().limit_hit.is_some(), "預設帳號的撞限不是 cx2 的回合能證明解除的");
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod codex_title_tests {
    use super::*;

    #[test]
    fn codex_title_turn_is_ignored() {
        let p = serde_json::json!({
            "type": "agent-turn-complete", "thread-id": "t", "turn-id": "u",
            "input-messages": ["Generate a concise, single-line task title of at most 36 characters …"],
            "last-assistant-message": "{\"title\":\"Reply with MERGED-OK\"}"
        });
        assert!(matches!(classify("codex", &p), HookKind::Ignore(_)));
        let real = serde_json::json!({
            "type": "agent-turn-complete", "thread-id": "t", "turn-id": "v",
            "input-messages": ["Reply with exactly MERGED-OK"], "last-assistant-message": "MERGED-OK"
        });
        assert!(matches!(classify("codex", &real), HookKind::TurnComplete { .. }));
    }

    /// 稽核：使用者真的要求「只回一個 title 的 JSON」時，助理回覆剛好就是 `{"title":…}`。以前只看回覆長相就當成標題回合丟掉——
    /// 這個真回合的 Stop 永遠不認領，只剩終端備援（`completed_fallback`）收尾。標題回合的 input 是標題指令，不是使用者的話。
    #[test]
    fn a_real_turn_whose_reply_is_a_title_object_is_not_a_title_turn() {
        let real = serde_json::json!({
            "type": "agent-turn-complete", "thread-id": "t", "turn-id": "v",
            "input-messages": ["把這篇文章的結論用 JSON 回我，只要一個欄位"],
            "last-assistant-message": "{\"title\":\"結論\"}"
        });
        assert!(matches!(classify("codex", &real), HookKind::TurnComplete { .. }), "{:?}", classify("codex", &real));
        // 沒有 input-messages（取不到）時，回覆長相仍是唯一線索：照舊當標題回合。
        let no_input = serde_json::json!({
            "type": "agent-turn-complete", "thread-id": "t", "turn-id": "w",
            "last-assistant-message": "{\"title\":\"x\"}"
        });
        assert!(matches!(classify("codex", &no_input), HookKind::Ignore(_)));
    }
}

/// #493：spool 的收攏是「先 rename 再讀」——claim 與 ack 之間 hook 附加進來的行不准被刪掉。
#[cfg(all(test, feature = "daemon-test-harness"))]
mod spool_claim_window_tests {
    use super::*;
    use crate::testing as tt;

    fn line(bot_id: &str, session: &str) -> String {
        let v = serde_json::json!({
            "bot_id": bot_id,
            "provider": "claude",
            "payload": {"hook_event_name": "Stop", "session_id": session},
        });
        format!("{v}\n")
    }

    async fn inbox_rows(env: &tt::Env) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM hook_events").fetch_one(&env.app.db).await.unwrap()
    }

    fn append(path: &std::path::Path, text: &str) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).create(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    /// 修正前：`read(spool)` → 整份寫回 `.replaying` → `remove_file(spool)`，中間那一段裡 hook 附加的行
    /// 被最後那個 remove 連檔刪掉。注入點放在「摘下來之後、這一輪的副本還沒刪掉」那一瞬。
    #[tokio::test]
    async fn a_line_appended_between_claim_and_ack_survives() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let dir = env.app.bot_dir(&bot.id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let spool = dir.join("hook-spool.jsonl");
        let staging = dir.join("hook-spool.jsonl.replaying");
        // 上一輪 ack 沒成功留下的 `.replaying`：修正前只有這條合併路徑會走到「讀完再刪」。
        std::fs::write(&staging, line(&bot.id, "s0")).unwrap();
        std::fs::write(&spool, line(&bot.id, "s1")).unwrap();

        let late_path = spool.clone();
        let late = line(&bot.id, "s2");
        crate::lifecycle::race_point::arm("spool_claimed", &bot.id, move || async move {
            append(&late_path, &late);
        });

        assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 2, "這一輪收的是 s0 與 s1");
        assert!(spool.exists(), "claim 之後附加的那一行必須還在 spool 上，不能被這一輪刪掉");
        assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 1, "下一輪把 s2 收進來");
        assert_eq!(inbox_rows(&env).await, 3, "三則都要進收件匣");
    }

    /// 摘下來（rename 成 `.claim`）之後、併進 `.replaying` 之前崩掉：`.claim` 是唯一的副本，
    /// 下一輪要接著收，不能留在那裡沒人管。
    #[tokio::test]
    async fn a_leftover_claim_file_is_folded_in_on_the_next_round() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "alfa").await;
        let dir = env.app.bot_dir(&bot.id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let claim = dir.join("hook-spool.jsonl.claim");
        std::fs::write(&claim, line(&bot.id, "s0")).unwrap();
        assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 1, "只剩 .claim 也要收");
        assert!(!claim.exists(), "收完就不該留著");
        assert_eq!(inbox_rows(&env).await, 1);
    }

    fn fold_dir() -> std::path::PathBuf {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-fold-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// #912：併進既有的 `.replaying` 是追加，不截斷；尾巴沒換行的半行補上換行再接；`src` 併完才刪。
    #[test]
    fn fold_spool_appends_without_truncating() {
        let dir = fold_dir();
        let (src, dst) = (dir.join("hook-spool.jsonl.claim"), dir.join("hook-spool.jsonl.replaying"));
        std::fs::write(&dst, "OLD1\nOLD2-half").unwrap();
        std::fs::write(&src, "NEW1\nNEW2\n").unwrap();
        super::fold_spool(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "OLD1\nOLD2-half\nNEW1\nNEW2\n");
        assert!(!src.exists(), "併完才刪 src");

        // dst 已經以換行結尾：不多補空行。
        std::fs::write(&src, "NEW3\n").unwrap();
        super::fold_spool(&src, &dst).unwrap();
        assert!(std::fs::read_to_string(&dst).unwrap().ends_with("NEW2\nNEW3\n"));

        // dst 不存在：直接搬過去。
        let fresh = dir.join("fresh.replaying");
        std::fs::write(&src, "ONLY\n").unwrap();
        super::fold_spool(&src, &fresh).unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "ONLY\n");
        assert!(!src.exists());
    }

    /// #912：`src` 讀不到（這裡是目錄）就回錯，`dst` 一個位元組都不動。
    #[test]
    fn fold_spool_keeps_dst_when_src_is_unreadable() {
        let dir = fold_dir();
        let (src, dst) = (dir.join("hook-spool.jsonl.claim"), dir.join("hook-spool.jsonl.replaying"));
        std::fs::create_dir(&src).unwrap();
        std::fs::write(&dst, "KEEP-ME\n").unwrap();
        assert!(super::fold_spool(&src, &dst).is_err());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "KEEP-ME\n");
        assert!(src.exists());
    }

    // ---- 遠端那一半：真的把產出的腳本跑起來（不連任何主機，`$HOME` 指到暫存目錄）

    struct Remote {
        home: std::path::PathBuf,
        bin: std::path::PathBuf,
        spool: std::path::PathBuf,
    }

    impl Remote {
        fn new() -> Self {
            let home = crate::testing::track(std::env::temp_dir().join(format!("am-claim-{}", db::ulid())));
            let d = home.join(crate::startup::REMOTE_ROOT).join("bots").join("botX");
            std::fs::create_dir_all(&d).unwrap();
            let bin = home.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            Self { spool: d.join("hook-spool.jsonl"), home, bin }
        }

        /// `rm` 的替身：第一次被呼叫時先往 spool 附加一行，再真的刪。修正前的腳本第一個 `rm` 是
        /// `rm -f "$f"`（刪 live spool），那一行就此消失；修正後第一個 `rm` 是 `rm -f "$f.claim"`，
        /// 附加的行落在 `mv` 之後新建的 spool 上，刪不到。只處理 `$HOME` 底下的路徑。
        fn arm_append_inside_the_window(&self, text: &str) {
            let stub = format!(
                "#!/bin/sh\n\
                 if [ ! -f \"$HOME/.armed\" ]; then : > \"$HOME/.armed\"; printf '%s' {line} >> {spool}; fi\n\
                 for a in \"$@\"; do case \"$a\" in -*) ;; \"$HOME\"/*) ;; *) echo \"refusing $a\" >&2; exit 1;; esac; done\n\
                 exec /bin/rm \"$@\"\n",
                line = crate::hosts::sh_quote(text),
                spool = crate::hosts::sh_quote(&self.spool.to_string_lossy()),
            );
            crate::testing::write_exec(self.bin.join("rm"), stub);
        }

        fn sh(&self, script: &str) -> String {
            let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap_or_default());
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .env("HOME", &self.home)
                .env("PATH", path)
                .output()
                .unwrap();
            assert!(out.status.success(), "腳本失敗：{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
    }

    impl Drop for Remote {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    /// 遠端同款：claim 的 `cat` 與 `rm` 是兩支各自 fork／exec 的外部指令，中間附加的行不准被刪。
    #[test]
    fn the_remote_claim_script_keeps_a_line_appended_inside_the_window() {
        let r = Remote::new();
        let root = crate::startup::REMOTE_ROOT;
        // 上一輪 ack 沒成功留下的 `.replaying`：修正前只有這條路徑會 `cat` 完再刪 live spool。
        std::fs::write(r.spool.with_extension("jsonl.replaying"), "OLD\n").unwrap();
        std::fs::write(&r.spool, "A\n").unwrap();
        r.arm_append_inside_the_window("LATE\n");

        let out = r.sh(&claim_script("botX", root).unwrap());
        assert!(out.contains("OLD") && out.contains("A"), "兩則都要讀出來：{out}");
        r.sh(&ack_script("botX", root).unwrap());

        let left = std::fs::read_to_string(&r.spool).unwrap_or_default();
        assert!(left.contains("LATE"), "窗口裡附加的那一行要留在 spool 上：{left:?}");
        let again = r.sh(&claim_script("botX", root).unwrap());
        assert!(again.contains("LATE"), "下一輪要讀得到它：{again}");
        assert!(!again.contains("OLD"), "已經 ack 過的不能再出現：{again}");
    }

    /// #500：`am_fold` 沒收掉 `.claim`（`.replaying` 寫不進去）時，這一輪不准摘新的 spool——
    /// 直接 `mv` 會把那份唯一的副本蓋掉，而且腳本 exit 0、stdout 有東西，daemon 會照常 ack。
    /// 用「`.replaying` 是目錄」製造寫入失敗：跟權限無關，root 底下跑也一樣失敗。
    #[test]
    fn a_claim_that_could_not_be_folded_is_never_overwritten_by_the_next_spool() {
        let r = Remote::new();
        let root = crate::startup::REMOTE_ROOT;
        let claim = r.spool.with_extension("jsonl.claim");
        std::fs::write(&claim, "OLD-CLAIM\n").unwrap();
        std::fs::create_dir(r.spool.with_extension("jsonl.replaying")).unwrap();
        std::fs::write(&r.spool, "NEW\n").unwrap();

        r.sh(&claim_script("botX", root).unwrap());

        assert_eq!(std::fs::read_to_string(&claim).unwrap(), "OLD-CLAIM\n", "唯一的副本不准被蓋掉");
        assert_eq!(std::fs::read_to_string(&r.spool).unwrap(), "NEW\n", "新的 spool 留到下一輪");
    }

    /// #500 複看：`.claim` 併不進去時，卡住這件事要從遠端自己回報——腳本 exit 0、`n = 0`、
    /// `drain_remote` 兩個 info 都被 `n > 0` 擋著，沒有這行標記就是一行 log 都沒有。
    #[test]
    fn a_claim_that_cannot_be_folded_reports_itself_as_stuck() {
        let r = Remote::new();
        let root = crate::startup::REMOTE_ROOT;
        std::fs::write(r.spool.with_extension("jsonl.claim"), "OLD-CLAIM\n").unwrap();
        std::fs::create_dir(r.spool.with_extension("jsonl.replaying")).unwrap();

        let out = r.sh(&claim_script("botX", root).unwrap());
        let d = parse_drain_output(&out);
        assert_eq!(d.stuck_bytes, Some(10), ".claim 的位元組數要帶回來：{out:?}");
        assert!(d.lines.is_empty(), "標記不可以被當成 spool 的一行：{:?}", d.lines);

        // 併得進去的那一輪不准報卡住（不然每次 drain 都在喊狼來了）。
        std::fs::remove_dir(r.spool.with_extension("jsonl.replaying")).unwrap();
        let out = r.sh(&claim_script("botX", root).unwrap());
        assert_eq!(parse_drain_output(&out).stuck_bytes, None, "{out:?}");
    }

    /// 連續看到才算數：併回去的那一輪要把計數清掉，不然一次卡住會永遠掛在那裡。
    #[tokio::test]
    async fn the_stuck_counter_only_counts_consecutive_rounds() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let rounds = |app: &Arc<App>| {
            let app = app.clone();
            async move { app.spool_fold_stuck.lock().await.get("mac2/b1").map(|e| e.0) }
        };

        note_fold_stuck(app, "mac2", "b1", Some(8)).await;
        assert_eq!(rounds(app).await, Some(1));
        note_fold_stuck(app, "mac2", "b1", Some(16)).await;
        assert_eq!(rounds(app).await, Some(2));
        assert_eq!(app.spool_fold_stuck.lock().await.get("mac2/b1").map(|e| e.1), Some(16), "大小跟著最新一輪");

        note_fold_stuck(app, "mac2", "b1", None).await;
        assert_eq!(rounds(app).await, None, "收成了就重新算");
        note_fold_stuck(app, "mac2", "b1", Some(8)).await;
        assert_eq!(rounds(app).await, Some(1), "不是接著上次算");
    }

    /// #501：`.replaying` 是 claim 腳本自己建的，裡面裝完整 payload——不能交給那台機器的 umask。
    /// 明確用寬鬆的 umask 跑，runner 剛好是 077 時才不會變成同義反覆。
    #[test]
    fn the_replaying_file_the_claim_script_creates_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let r = Remote::new();
        let root = crate::startup::REMOTE_ROOT;
        std::fs::write(&r.spool, "A\n").unwrap();
        r.sh(&format!("umask 022\n{}", claim_script("botX", root).unwrap()));
        let staging = r.spool.with_extension("jsonl.replaying");
        let mode = std::fs::metadata(&staging).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600, ".replaying 只給自己讀");
    }

    /// An interrupted remote writer can leave a private `.tmp.*` file that replay never sees.
    /// Drain should remove only old abandoned temp files and preserve a fresh writer's file.
    #[test]
    fn macos_local_remote_claim_cleans_stale_temp_spool_files() {
        let r = Remote::new();
        let root = crate::startup::REMOTE_ROOT;
        let sd = r.spool.parent().unwrap().join("hook-spool.d");
        std::fs::create_dir_all(&sd).unwrap();
        let stale = sd.join(".tmp.abandoned");
        let fresh = sd.join(".tmp.active");
        std::fs::write(&stale, "partial secret payload").unwrap();
        std::fs::write(&fresh, "active writer payload").unwrap();
        let status = std::process::Command::new("touch")
            .args(["-t", "200001010000"])
            .arg(&stale)
            .status()
            .unwrap();
        assert!(status.success());

        r.sh(&claim_script("botX", root).unwrap());

        assert!(!stale.exists(), "abandoned private temp files should be cleaned");
        assert!(fresh.exists(), "recent temp files may still belong to an active writer");
    }

    /// 摘下來還沒併進 `.replaying` 就斷線：`.claim` 下一輪要被收回來，不是留在遠端沒人管。
    #[test]
    fn the_remote_claim_script_folds_a_leftover_claim_file() {
        let r = Remote::new();
        let root = crate::startup::REMOTE_ROOT;
        std::fs::write(r.spool.with_extension("jsonl.claim"), "STRANDED\n").unwrap();
        let out = r.sh(&claim_script("botX", root).unwrap());
        assert!(out.contains("STRANDED"), "上一輪留下的 .claim 要收進來：{out}");
        assert!(!r.spool.with_extension("jsonl.claim").exists(), "併完就不留");
    }

    /// #652：一則一檔。claim 把已經寫好的 `*.json` 搬進 replaying 再讀；ack 才刪。
    /// claim 展開 glob 之後才出現的檔留在 `hook-spool.d`，下一輪才收。
    #[test]
    fn macos_local_per_file_spool_is_claimed_then_acked() {
        let r = Remote::new();
        let root = crate::startup::REMOTE_ROOT;
        let dir = r.spool.parent().unwrap();
        let sd = dir.join("hook-spool.d");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("100-1.json"), "ONE\n").unwrap();
        std::fs::write(sd.join("200-2.json"), "TWO\n").unwrap();
        let out = r.sh(&claim_script("botX", root).unwrap());
        assert!(out.contains("ONE") && out.contains("TWO"), "{out}");
        assert!(!sd.join("100-1.json").exists(), "摘下來的不留在 live 目錄");
        let rd = dir.join("hook-spool.replaying");
        assert!(rd.join("100-1.json").exists() && rd.join("200-2.json").exists());
        std::fs::write(sd.join("300-3.json"), "LATE\n").unwrap();
        r.sh(&ack_script("botX", root).unwrap());
        assert!(!rd.join("100-1.json").exists(), "ack 才刪已收下的");
        assert_eq!(std::fs::read_to_string(sd.join("300-3.json")).unwrap(), "LATE\n");
        let again = r.sh(&claim_script("botX", root).unwrap());
        assert!(again.contains("LATE"), "{again}");
        assert!(!again.contains("ONE"), "ack 過的不能再出現：{again}");
    }

    /// 掃描要看得到只有一則一檔、還沒 claim 的 bot（舊 jsonl 不在時也要）。
    #[test]
    fn macos_local_scan_sees_a_per_file_spool() {
        let r = Remote::new();
        let root = crate::startup::REMOTE_ROOT;
        let dir = r.spool.parent().unwrap();
        let sd = dir.join("hook-spool.d");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("1-1.json"), "{}\n").unwrap();
        let out = r.sh(&scan_script(root));
        assert!(out.lines().any(|l| l.trim() == "botX"), "有檔就要被掃到：{out:?}");
        std::fs::remove_file(sd.join("1-1.json")).unwrap();
        let empty = r.sh(&scan_script(root));
        assert!(!empty.lines().any(|l| l.trim() == "botX"), "空目錄不是欠著：{empty:?}");
    }

    /// `.replaying` 尾巴沒有換行（崩在一行寫到一半）時，併進來的第一行不能跟它黏成一行（#302 的遠端版）。
    #[test]
    fn the_remote_fold_does_not_glue_lines_together() {
        let r = Remote::new();
        let root = crate::startup::REMOTE_ROOT;
        std::fs::write(r.spool.with_extension("jsonl.replaying"), "TORN").unwrap();
        std::fs::write(&r.spool, "NEXT\n").unwrap();
        let out = r.sh(&claim_script("botX", root).unwrap());
        assert!(out.lines().any(|l| l == "NEXT"), "新的一行要自己一行：{out:?}");
    }
}

/// #243：讀不到 bot 的 host 不能當成本機——spool 重放要回錯、事件留著等下一輪，DB 好了要補回來。
#[cfg(all(test, feature = "daemon-test-harness"))]
mod host_unreadable_replay_tests {
    use super::*;
    use crate::testing as tt;

    async fn spooled(env: &tt::Env, name: &str) -> (db::Bot, std::path::PathBuf) {
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        let dir = env.app.bot_dir(&bot.id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let line = serde_json::json!({"bot_id": bot.id, "provider": "claude", "payload": {"hook_event_name": "Stop", "session_id": "s1"}});
        let spool = dir.join("hook-spool.jsonl");
        std::fs::write(&spool, format!("{line}\n")).unwrap();
        (bot, spool)
    }

    async fn inbox_rows(env: &tt::Env) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM hook_events").fetch_one(&env.app.db).await.unwrap()
    }

    /// #302：上一輪崩在收一半，留下 `.replaying`、沒有新的 spool——它是唯一的副本，這一輪要收進來。
    #[tokio::test]
    async fn a_leftover_replaying_file_with_no_new_spool_is_still_replayed() {
        let env = tt::env().await;
        let (bot, spool) = spooled(&env, "alfa").await;
        let staging = spool.with_extension("jsonl.replaying");
        std::fs::rename(&spool, &staging).unwrap();
        assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 1, "只剩 .replaying 也要收");
        assert!(!staging.exists());
        assert_eq!(inbox_rows(&env).await, 1);
    }

    /// 合併走位元組：`.replaying` 尾巴是崩掉時寫到一半的多位元組字元，舊事件不能因此被新的 spool 蓋掉。
    #[tokio::test]
    async fn a_leftover_replaying_with_a_torn_utf8_tail_is_merged_not_overwritten() {
        let env = tt::env().await;
        let (bot, spool) = spooled(&env, "alfa").await;
        let staging = spool.with_extension("jsonl.replaying");
        let mut old = std::fs::read(&spool).unwrap();
        old.extend_from_slice(&"{\"bot_id\":\"x\",\"note\":\"中".as_bytes()[..24]);
        std::fs::write(&staging, old).unwrap();
        let line = serde_json::json!({"bot_id": bot.id, "provider": "claude", "payload": {"hook_event_name": "Stop", "session_id": "s2"}});
        std::fs::write(&spool, format!("{line}\n")).unwrap();
        assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 2, "舊的一則加新的一則都要收");
        assert_eq!(inbox_rows(&env).await, 2);
    }

    /// 每顆 bot 的 host 讀不到：修前退回 local，把（遠端 bot 的）本機 spool 當成它的收下；修後回錯、檔案原封不動。
    #[tokio::test]
    async fn a_bot_whose_host_cannot_be_read_is_not_replayed_as_local() {
        let env = tt::env().await;
        let (bot, spool) = spooled(&env, "alfa").await;
        tt::make_table_unreadable(&env.app, "projects").await;
        let r = replay_spool(&env.app, &bot.id).await;
        tt::make_table_readable(&env.app, "projects").await;
        assert!(r.is_err(), "host 讀不到要回錯：{r:?}");
        assert!(spool.exists(), "spool 不能被當成本機的吃掉");
        assert_eq!(inbox_rows(&env).await, 0);
        assert_eq!(replay_spool(&env.app, &bot.id).await.unwrap(), 1, "DB 好了補回來");
    }

    /// 列舉那一層：`live_bots_on_host` 讀不到，以前變成「沒有 bot」；現在背景重試，DB 好了不需要任何新事件就補齊。
    #[tokio::test]
    async fn a_host_pass_that_cannot_enumerate_bots_retries_until_the_spool_is_drained() {
        let env = tt::env().await;
        let (_, spool) = spooled(&env, "alfa").await;
        tt::make_table_unreadable(&env.app, "projects").await;
        replay_host(&env.app, crate::config::LOCAL_HOST).await;
        assert!(spool.exists(), "讀不到時什麼都不能動");
        tt::make_table_readable(&env.app, "projects").await;
        // 等的是「收完」，不是「spool 不見了」（#255）：`replay_spool` 先把 spool rename 成
        // `.replaying`、逐行 commit 進 hook_events，**全部 commit 之後**才刪 `.replaying`。只等
        // spool 消失，會在 rename 之後、commit 之前就去數列數，慢的 runner 上數到 0。
        let staging = spool.with_extension("jsonl.replaying");
        for _ in 0..200 {
            if !spool.exists() && !staging.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(!spool.exists(), "DB 恢復後背景重試要把 spool 收進來");
        assert!(!staging.exists(), "收完才刪 .replaying：{staging:?}");
        assert_eq!(inbox_rows(&env).await, 1, "恰好收一次");
    }
}

/// agy（Antigravity CLI）：payload 沒有事件名（hook 子行程放進 `hookEventName`），`Stop` 也沒有助理文字（`lastAssistantMessage` 是子行程讀 transcript 補的）。
#[cfg(all(test, feature = "daemon-test-harness"))]
mod agy_tests {
    use super::*;
    use crate::testing as tt;

    /// 實測 1.2.16 的 Stop（trimmed）加上 hook 子行程補的兩個欄位。
    fn stop(extra: Value) -> Value {
        let mut v = json!({"hookEventName": "Stop", "conversationId": "c-1", "modelName": "gemini-3.1-pro-low",
            "transcriptPath": "/h/.gemini/antigravity-cli/brain/c-1/.system_generated/logs/transcript_full.jsonl",
            "workspacePaths": ["/w"], "executionNum": 0, "terminationReason": "NO_TOOL_CALL", "fullyIdle": true});
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }

    #[test]
    fn a_normal_stop_is_a_turn_with_the_reply_the_hook_process_read_and_no_dedup_key() {
        match classify("agy", &stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "say OK"}))) {
            HookKind::TurnComplete { session_id, turn_id, transcript_path, assistant, user } => {
                assert_eq!(session_id.as_deref(), Some("c-1"));
                assert_eq!(turn_id, None, "`executionNum` 跨回合是不是唯一沒驗過：當去重鑰匙會吃掉之後的回合");
                assert!(transcript_path.unwrap().ends_with("transcript_full.jsonl"));
                assert_eq!((assistant.as_deref(), user.as_deref()), (Some("OK"), Some("say OK")));
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
        // terminationReason 的值域不可假設：不認得的值照樣是正常結束。
        assert!(matches!(classify("agy", &stop(json!({"terminationReason": "model_stop"}))), HookKind::TurnComplete { .. }));
        assert!(matches!(classify("agy", &stop(json!({"terminationReason": "SOMETHING_NEW"}))), HookKind::TurnComplete { .. }));
    }

    #[test]
    fn a_stop_with_background_work_still_running_is_not_the_end_of_the_turn() {
        assert!(matches!(classify("agy", &stop(json!({"fullyIdle": false}))), HookKind::Ignore(_)));
    }

    #[test]
    fn an_error_stop_fails_the_turn_and_the_error_text_is_classified() {
        match classify("agy", &stop(json!({"terminationReason": "ERROR", "error": "429 RESOURCE_EXHAUSTED: quota exceeded"}))) {
            HookKind::TurnFailed { session_id, reason, detail, .. } => {
                assert_eq!(session_id.as_deref(), Some("c-1"));
                assert_eq!(reason, FailureReason::RateLimit);
                assert!(detail.unwrap().contains("quota exceeded"));
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
        assert!(matches!(classify("agy", &stop(json!({"terminationReason": "ERROR"}))), HookKind::TurnFailed { .. }), "原因是 ERROR、沒有錯誤文字也算失敗");
    }

    #[test]
    fn session_start_and_the_first_pre_invocation_both_give_the_identity_and_state_is_a_status_line() {
        for ev in ["SessionStart", "PreInvocation"] {
            let v = json!({"hookEventName": ev, "conversationId": "c-9", "transcriptPath": "/t/transcript_full.jsonl", "invocationNum": 0});
            match classify("agy", &v) {
                HookKind::Identity { session_id, transcript_path } => {
                    assert_eq!(session_id.as_deref(), Some("c-9"), "{ev}");
                    assert_eq!(transcript_path.as_deref(), Some("/t/transcript_full.jsonl"));
                }
                other => panic!("{ev}: expected Identity, got {other:?}"),
            }
        }
        assert!(matches!(classify("agy", &json!({"hookEventName": "state", "conversation_id": "c-9"})), HookKind::StatusLine));
        // 沒有事件名（手動跑、舊 dispatcher）：不猜。
        assert!(matches!(classify("agy", &json!({"conversationId": "c-9"})), HookKind::Ignore(_)));
        assert!(matches!(classify("agy", &json!({"hookEventName": "PostInvocation"})), HookKind::Ignore(_)));
    }

    async fn agy_turn(app: &Arc<App>, project_id: &str) -> (String, String, String) {
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'agy','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(project_id)
        .bind(format!("agy-{}", &bot_id[bot_id.len() - 8..]))
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','working','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,?,'web','in_flight','ok','say OK',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        (bot_id, run_id, turn_id)
    }

    fn body(bot_id: &str, payload: Value) -> HookBody {
        HookBody { bot_id: bot_id.into(), provider: "agy".into(), payload, received_at: None, truncated: false, run_id: None }
    }

    async fn replies(app: &Arc<App>, turn_id: &str) -> Vec<(String, String)> {
        sqlx::query_as("SELECT role, content FROM messages WHERE turn_id=? AND role IN ('assistant','system') ORDER BY created_at")
            .bind(turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_stop_completes_the_in_flight_turn_with_the_transcript_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _run, turn_id) = agy_turn(&app, &env.project_id).await;
        process(&app, &body(&bot_id, stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "say OK"})))).await.unwrap();
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.status, "completed");
        assert_eq!(replies(&app, &turn_id).await, [("assistant".to_string(), "OK".to_string())]);
        // 同一則重送（spool 重播）：回合已收，不會長出第二個回覆。
        process(&app, &body(&bot_id, stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "say OK"})))).await.unwrap();
        assert_eq!(replies(&app, &turn_id).await.len(), 1);
    }

    #[tokio::test]
    async fn a_stop_records_the_model_and_context_tokens_for_the_status_card() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, _turn) = agy_turn(&app, &env.project_id).await;
        sqlx::query("UPDATE bots SET model = 'gemini-3.8-flash-medium' WHERE id = ?").bind(&bot_id).execute(&app.db).await.unwrap();
        process(&app, &body(&bot_id, stop(json!({"lastAssistantMessage": "OK", "lastUserMessage": "say OK", "lastInputTokens": 11824})))).await.unwrap();
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        let st: Value = serde_json::from_str(r.status_json.as_deref().expect("status_json")).unwrap();
        assert_eq!(st["model"]["id"], "gemini-3.8-flash-medium");
        assert_eq!(st["context_window"]["total_input_tokens"], 11824);
    }

    /// `--conversation=<id>` 接回：第一個 `PreInvocation` 的 conversationId 對得上＝verified；agy 對不存在的 id 只警告、
    /// 開新對話（不報錯），回報的 id 對不上＝`resume_mismatch`，聊天室要看得見。
    #[tokio::test]
    async fn a_resumed_agy_conversation_is_verified_or_reported_as_a_mismatch() {
        for (reported, outcome) in [("conv-expected", "verified"), ("conv-fresh", "mismatch")] {
            let env = tt::env().await;
            let app = env.app.clone();
            let (bot_id, run_id, _turn) = agy_turn(&app, &env.project_id).await;
            sqlx::query("UPDATE runs SET resume_session_id='conv-expected' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
            let pre = json!({"hookEventName": "PreInvocation", "conversationId": reported, "transcriptPath": "/x/transcript_full.jsonl"});
            process(&app, &body(&bot_id, pre)).await.unwrap();
            let (got, native, requested): (Option<String>, Option<String>, Option<String>) =
                sqlx::query_as("SELECT resume_outcome, native_session_id, resume_session_id FROM runs WHERE id=?").bind(&run_id).fetch_one(&app.db).await.unwrap();
            assert_eq!(got.as_deref(), Some(outcome), "{reported}");
            assert_eq!(native.as_deref(), Some(reported));
            assert_eq!(requested, None, "一次性的要求標記清掉了");
            let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
            let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'").bind(&conv).fetch_all(&app.db).await.unwrap();
            assert_eq!(notes.iter().any(|n| n.contains("不是同一個")), outcome == "mismatch", "{notes:?}");
        }
    }

    #[tokio::test]
    async fn an_error_stop_closes_the_turn_as_failed_with_the_agy_error_in_the_note() {
        // "API key not valid" 被歸為授權失敗，會走 `agy_auth::on_turn_auth_failure`：寫行程全域的 agy 登入冷卻（key 是本機）。
        // 跟 `quota_agy` 的登入／登出測試共用同一把鎖，不然冷卻會在別條測試中途冒出來（login watcher 測試因此偶發紅）。
        let _agy_globals = crate::quota_agy::token_test_lock().await;
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _run, turn_id) = agy_turn(&app, &env.project_id).await;
        process(&app, &body(&bot_id, stop(json!({"terminationReason": "ERROR", "error": "API key not valid"})))).await.unwrap();
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.status, "failed");
        let notes = replies(&app, &turn_id).await;
        assert!(notes.iter().any(|(role, c)| role == "system" && c.contains("API key not valid")), "{notes:?}");
    }

    #[tokio::test]
    async fn the_first_pre_invocation_records_the_conversation_and_a_transcript_under_the_agy_brain_dir() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, _turn) = agy_turn(&app, &env.project_id).await;
        let conv = format!("c-{}", db::ulid());
        let logs = crate::home::dir().unwrap().join(format!(".gemini/antigravity-cli/brain/{conv}/.system_generated/logs"));
        std::fs::create_dir_all(&logs).unwrap();
        let tp = logs.join("transcript_full.jsonl");
        std::fs::write(&tp, "").unwrap();
        process(&app, &body(&bot_id, json!({"hookEventName": "PreInvocation", "conversationId": conv, "transcriptPath": tp, "invocationNum": 0}))).await.unwrap();
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.native_session_id.as_deref(), Some(conv.as_str()));
        assert_eq!(r.transcript_path.as_deref(), Some(tp.to_str().unwrap()));
        // statusLine 也補對話 id（沒有對話之前什麼都不記）。
        let (bot2, run2, _t2) = agy_turn(&app, &env.project_id).await;
        process(&app, &body(&bot2, json!({"hookEventName": "state", "conversation_id": "", "transcript_path": "/placeholder/transcript_full.jsonl"}))).await.unwrap();
        let r2 = db::run(&app.db, &run2).await.unwrap().unwrap();
        assert_eq!((r2.native_session_id, r2.transcript_path), (None, None), "對話還沒建立：不記");
        process(&app, &body(&bot2, json!({"hookEventName": "state", "conversation_id": conv, "transcript_path": "/h/.gemini/antigravity/brain/x/.system_generated/logs/transcript.jsonl"}))).await.unwrap();
        let r2 = db::run(&app.db, &run2).await.unwrap().unwrap();
        assert_eq!(r2.native_session_id.as_deref(), Some(conv.as_str()));
        assert_eq!(r2.transcript_path, None, "statusLine 的 transcript_path 是摘要版、另一個目錄：不記，只有 hook 的 transcriptPath（transcript_full.jsonl）算");
    }

    #[tokio::test]
    async fn a_transcript_path_outside_the_agy_brain_dir_is_not_recorded() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, run_id, _turn) = agy_turn(&app, &env.project_id).await;
        process(&app, &body(&bot_id, json!({"hookEventName": "SessionStart", "conversationId": "c-evil", "transcriptPath": "/etc/passwd.jsonl"}))).await.unwrap();
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.native_session_id.as_deref(), Some("c-evil"));
        assert_eq!(r.transcript_path, None, "bot 自己的 hook token 不能指定任意檔案當 transcript");
    }

    #[tokio::test]
    async fn a_hook_from_another_provider_is_refused_for_an_agy_bot() {
        assert!(provider_matches_kind("agy", "agy"));
        assert!(!provider_matches_kind("claude", "agy") && !provider_matches_kind("agy", "claude"));
    }
}

/// 遠端 spool 折疊卡住的帳。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait SpoolFoldStuck: Send + Sync {
    fn spool_fold_stuck(&self) -> &tokio::sync::Mutex<std::collections::HashMap<String, (u32, i64)>>;
}

/// hook 失敗分類的計數。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait ClassifyFailures: Send + Sync {
    fn classify_failures(&self) -> &std::sync::atomic::AtomicU32;
}

/// hook 收件／重放核心需要的宿主能力：一組窄 trait（`capabilities`、`events::ports`…）加上少數 `App` 才有的動作。
/// 核心（`process*`、`drain_remote`、`replay_spool`）對它泛型，不知道 `App`；`impl HookHost for Arc<App>` 在 `runners/hookrecv.rs`。
pub trait HookHost:
    crate::capabilities::Db
    + crate::capabilities::Emit
    + crate::capabilities::BotLocks
    + crate::capabilities::BotStatusEmit
    + crate::hosts::HostsAccess
    + crate::hosts::HostInstance
    + crate::tools::ToolsTable
    + SpoolFoldStuck
    + ClassifyFailures
    + TurnCommands
    + QuotaCommands
    + ProviderPort
    + ApiPort
    + Send
    + Sync
{
    /// 叫醒 durable hook 收件匣的 worker（commit 之後才叫）。
    fn wake_hook_inbox(&self);
    /// 本機這顆 bot 的資料夾（spool 在裡面）。
    fn hook_bot_dir(&self, bot_id: &str) -> Result<std::path::PathBuf>;
    /// 回合結束 hook 處理完之後補讀 AskUserQuestion 的答案。
    fn after_turn_end<'a>(&'a self, body: &'a HookBody) -> impl std::future::Future<Output = ()> + Send + 'a;
    /// claude Stop hook 帶來的背景工作數字。
    fn background_stop<'a>(&'a self, run: &'a db::Run, payload: &'a Value) -> impl std::future::Future<Output = ()> + Send + 'a;
    /// transcript 路徑是不是這顆 bot 讀得的（遠端／本機分別把關）。
    fn transcript_allowed<'a>(&'a self, bot: &'a db::Bot, path: &'a str) -> impl std::future::Future<Output = bool> + Send + 'a;
    fn local_transcript_allowed<'a>(&'a self, bot: &'a db::Bot, path: &'a str) -> impl std::future::Future<Output = bool> + Send + 'a;
}
