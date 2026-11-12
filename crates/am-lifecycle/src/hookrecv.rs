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
/// 舊版遠端 shim 寫進 spool 的報備事件名，保留對既有事件的對帳。
pub const RELAY_ANNOUNCE_EVENT: &str = "AmRelayAnnounce";
/// 遠端 shim 開完子 agent 後寫進自己 bot spool 的登記（issue #1790，`herdr_shim.rs` 的 `am_spawn_claim`）。
pub const SPAWN_CLAIM_EVENT: &str = "AmSpawnClaim";
/// 遠端 agm 認領 issue 後寫入自己 bot spool 的撞題檢查（issue #1468）。
pub const CLAIM_COLLISION_EVENT: &str = "AmClaimCollisionCheck";
/// 這一則是不是撞題檢查（#1626：收件匣的撞題 lane 與 [`process_for`] 都用它分流；`classify` 也是同一個判斷）。
pub fn is_claim_collision(body: &HookBody) -> bool {
    body.payload.get("hook_event_name").and_then(Value::as_str) == Some(CLAIM_COLLISION_EVENT)
}
const MAX_CLAIM_COLLISION_TEXT_CHARS: usize = 500;

/// 遲到報備只補標已綁定指定 run 的回合訊息；conversation／名字不足以證明世代。
async fn relay_backfill(app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands), from_bot: &str, from_turn: Option<&str>, to_run_id: &str, text: &str) -> Result<()> {
    let host = db::bot_host(app.db(), from_bot).await?;
    let since = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT m.id, m.content, b.id FROM messages m
           JOIN conversations c ON c.id = m.conversation_id
           JOIN bots b ON b.id = c.bot_id
           JOIN projects p ON p.id = b.project_id
           JOIN runs r ON r.bot_id = b.id AND r.state = 'running' AND r.id = ?
          WHERE p.host = ? AND b.id <> ? AND m.role = 'user' AND m.source IN ('hook','terminal_fallback')
            AND m.relay_from IS NULL AND m.created_at >= ?
            AND EXISTS (SELECT 1 FROM turns t WHERE t.id = m.turn_id AND t.run_id = r.id)
          ORDER BY m.created_at DESC, m.rowid DESC LIMIT 20",
    )
    .bind(to_run_id)
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
    let _ = crate::agent_relay::claim(&host, to_run_id, &content);
    let message: db::Message = sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE id = ?").bind(&msg_id).fetch_one(app.db()).await?;
    tracing::info!(from = %from_bot, to = %to_run_id, msg = %msg_id, "relay announce arrived after the echo; attributed it");
    app.emit_message_added(&to_bot, message).await;
    Ok(())
}

// Accept the caller's transaction as well as a pool; hook writes must not borrow
// another pool slot while holding SQLite's write lock.
async fn relay_source<'e>(conn: impl sqlx::Executor<'e, Database = sqlx::Sqlite>, run: Option<&db::Run>, echo: &str) -> Option<crate::agent_relay::Relayed> {
    let run = run?;
    let host: Option<String> = sqlx::query_scalar("SELECT p.host FROM bots b JOIN projects p ON p.id=b.project_id WHERE b.id=?")
        .bind(&run.bot_id).fetch_optional(conn).await.ok()?;
    let host = host.unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    crate::agent_relay::claim_relayed(&host, &run.id, echo)
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
    /// claude 的 `PreCompact`（issue #1770）：這顆 run 開始壓縮對話（手動、閒置、自動都會送）。只標記「壓縮中」
    /// （`compacting.rs`），不建立、不動 Turn——閒置壓縮的空回合是 pane 轉 working 時開出來的。
    CompactStarting,
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
    RelayAnnounce { to_bot_id: String, to_run_id: String, delivery_id: String, text: String },
    /// 遠端 bot 的 herdr shim 在 `agent start` 成功後寫進自己 spool 的登記（issue #1790）：這顆 pane 是這顆 bot 開的。
    /// 本機走 `/relay/spawn/claim`，遠端沒有 `AM_PORT`，跟 `RelayAnnounce` 同一條 spool。
    SpawnClaim { pane_id: String, parent_run_id: String },
    /// 遠端 agm 認領票後交給 daemon 的撞題檢查（issue #1468）；耐久 hook worker 等檢查完成才 ACK。
    ClaimCollisionCheck { issue: Option<u64>, title: String, child: String, child_bot_id: Option<String>, claim_id: Option<String>, branch: Option<String> },
    Ignore(String),
}

impl std::fmt::Debug for HookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            HookKind::Identity { .. } => "Identity",
            HookKind::TurnComplete { .. } => "TurnComplete",
            HookKind::TurnFailed { .. } => "TurnFailed",
            HookKind::StatusLine => "StatusLine",
            HookKind::CompactStarting => "CompactStarting",
            HookKind::SubagentEvent { .. } => "SubagentEvent",
            HookKind::SpawnHint { .. } => "SpawnHint",
            HookKind::AskAnswered(_) => "AskAnswered",
            HookKind::RelayAnnounce { .. } => "RelayAnnounce",
            HookKind::SpawnClaim { .. } => "SpawnClaim",
            HookKind::ClaimCollisionCheck { .. } => "ClaimCollisionCheck",
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
#[path = "hookrecv_tests/account_tests.rs"]
mod account_tests;

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
    if p.get("hook_event_name").and_then(Value::as_str) == Some(CLAIM_COLLISION_EVENT) {
        let issue = match p.get("issue") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n),
                None => return HookKind::Ignore("claim collision without valid issue".into()),
            },
        };
        let Some(title) = s("title").map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
            return HookKind::Ignore("claim collision without title".into());
        };
        let Some(child) = s("child").map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
            return HookKind::Ignore("claim collision without child".into());
        };
        let branch = s("branch").map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let child_bot_id = s("child_bot_id").map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let claim_id = s("claim_id").map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        if title.chars().count() > MAX_CLAIM_COLLISION_TEXT_CHARS
            || child.chars().count() > MAX_CLAIM_COLLISION_TEXT_CHARS
            || child_bot_id.as_ref().is_some_and(|v| v.chars().count() > MAX_CLAIM_COLLISION_TEXT_CHARS)
            || claim_id.as_ref().is_some_and(|v| v.chars().count() > MAX_CLAIM_COLLISION_TEXT_CHARS)
            || branch.as_ref().is_some_and(|v| v.chars().count() > MAX_CLAIM_COLLISION_TEXT_CHARS)
        {
            return HookKind::Ignore("claim collision text too long".into());
        }
        return HookKind::ClaimCollisionCheck { issue, title, child, child_bot_id, claim_id, branch };
    }
    // shim 自己的事件，不分 claude／codex（寄件的 bot 可以是任何一種）。
    if p.get("hook_event_name").and_then(Value::as_str) == Some(RELAY_ANNOUNCE_EVENT) {
        return match (s("to_bot_id"), s("to_run_id"), s("delivery_id"), s("text")) {
            (Some(to_bot_id), Some(to_run_id), Some(delivery_id), Some(text))
                if [&to_bot_id, &to_run_id, &delivery_id, &text].iter().all(|s| !s.trim().is_empty()) =>
                HookKind::RelayAnnounce { to_bot_id, to_run_id, delivery_id, text },
            _ => HookKind::Ignore("relay announce without stable recipient/delivery ids".into()),
        };
    }
    if p.get("hook_event_name").and_then(Value::as_str) == Some(SPAWN_CLAIM_EVENT) {
        return match s("pane_id").map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            Some(pane_id) => HookKind::SpawnClaim { pane_id, parent_run_id: s("parent_run_id").unwrap_or_default() },
            None => HookKind::Ignore("spawn claim without pane_id".into()),
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
                "PreCompact" => HookKind::CompactStarting,
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
        // `Stop` 另帶 `lastAssistantMessage`／`lastUserMessage`（從 transcript 讀的）；agy 1.4.7 起 stdin 自帶 `lastUserInput`（issue #1950）。
        // camelCase 是 hook、snake_case 是 statusLine。
        "agy" => {
            let either = |camel: &str, snake: &str| s(camel).or_else(|| s(snake)).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let conv = either("conversationId", "conversation_id");
            let path = either("transcriptPath", "transcript_path");
            match s("hookEventName").unwrap_or_default().to_ascii_lowercase().as_str() {
                // 對話是第一則 prompt 才建立：`SessionStart` 沒文件、不一定來，第一個 `PreInvocation` 補身份。
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
                    // issue #1950: agy 1.4.7 hook stdin 新增 lastUserInput 當額外證據；
                    // lastUserMessage 優先，缺席或空白才退回 lastUserInput
                    let user = either("lastUserMessage", "last_user_message")
                        .or_else(|| either("lastUserInput", "last_user_input"));
                    HookKind::TurnComplete { session_id: conv, turn_id: None, transcript_path: path, assistant: s("lastAssistantMessage"), user }
                }
                other => HookKind::Ignore(format!("agy event `{other}`")),
            }
        }
        other => HookKind::Ignore(format!("unknown provider {other}")),
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/classify_tests.rs"]
mod classify_tests;

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
    // #1626：撞題檢查等 Jev（可能很慢），而它只寫撞題帳本與提示、不改這顆 bot 的回合狀態，所以不取 bot 鎖；
    // 占著鎖的話同一顆 bot 之後的 Stop 與其他要這把鎖的操作都得排在後面。移交、世代、provider 等檢查照常走 process_locked。
    if is_claim_collision(body) {
        process_locked_for_with_outcome(app, body, event_id, &mut None).await?;
        return Ok(());
    }
    let lock = app.bot_lock(&body.bot_id).await;
    let _g = lock.lock().await;
    let mut accepted_turn_id = None;
    process_locked_for_with_outcome(app, body, event_id, &mut accepted_turn_id).await?;
    // Only a Stop that the core actually applied can contribute its post-Stop records.
    if let Some(turn_id) = accepted_turn_id {
        let run_id = sqlx::query_scalar::<_, Option<String>>("SELECT run_id FROM turns WHERE id = ?")
            .bind(&turn_id)
            .fetch_optional(app.db())
            .await?
            .flatten();
        app.after_turn_end(body, run_id.as_deref(), &turn_id).await?;
    }
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
    let from = relay_source(&mut **tx, run, text).await;
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
    let added = store_resent_prompt_tx(&mut tx, conv, turn, run, &text).await?;
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
///
/// 綁上 native turn id（這一句之後它非空）就是 hook 的事後證據：`delivery_verified` 一併寫成 1（issue #1471）。
/// 只有 session id、沒有 turn id 的不算；備援收尾又沒有 hook 證據的回合維持原判。
async fn stamp_native_ids(
    conn: &mut sqlx::SqliteConnection,
    turn_id: &str,
    session_id: &Option<String>,
    native_turn_id: &Option<String>,
) -> Result<()> {
    sqlx::query(
        "UPDATE turns SET native_session_id=COALESCE(native_session_id, ?),
                          native_turn_id=COALESCE(native_turn_id, ?),
                          delivery_verified=CASE WHEN COALESCE(native_turn_id, ?) IS NOT NULL THEN 1 ELSE delivery_verified END
          WHERE id=? AND status IN ('completed_fallback','completed')",
    )
    .bind(session_id)
    .bind(native_turn_id)
    .bind(native_turn_id)
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 遲到 hook 的去重鑰匙：native id 與收件匣事件 id 一起記（同一個交易、同一個回合）。
///
/// 只記 native id 的話，這一回合的核心交易 commit 之後 `after_turn_end` 失敗、收件匣重試同一則事件時：
/// 3a 的 `turn_of_event` 認不得它，第 3 步又被 native id 去重吞掉並回 Ok——事件標成 processed，
/// Stop 裡的 AskUserQuestion 答案永遠補不上（issue #1469，#1319 只涵蓋了正常收尾那條路）。
/// 所以 `event_id` 跟 native id 同進同出：native id 記了，事件 id 就一定記。
async fn stamp_late_hook_keys(
    conn: &mut sqlx::SqliteConnection,
    turn_id: &str,
    session_id: &Option<String>,
    native_turn_id: &Option<String>,
    event_id: Option<&str>,
) -> Result<()> {
    stamp_native_ids(conn, turn_id, session_id, native_turn_id).await?;
    stamp_source_event(conn, turn_id, event_id).await
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
/// 收件匣事件 id（`event_id`）跟 native id 同一句寫（[`stamp_late_hook_keys`]，issue #1469）：之後 `after_turn_end` 失敗、
/// 收件匣重試同一則時，3a 的重播判斷才認得「這一則已經收進這一回合」，補做後置處理而不是被 native id 去重吞掉。
async fn fill_or_drop_late_hook(
    app: &(impl crate::capabilities::Db + crate::events::ports::TurnCommands),
    turn: &db::Turn,
    body_text: &str,
    session_id: &Option<String>,
    native_turn_id: &Option<String>,
    same_turn: bool,
    event_id: Option<&str>,
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
        return replace_fallback_reply(app, tx, turn, body_text, session_id, native_turn_id, event_id).await;
    }
    if body_text.trim().is_empty() || has_reply > 0 {
        // 丟掉的 hook：只有「有證據是這一回合的」才記它的鑰匙。沒有證據的（`same_turn = false`）可能屬於別的回合，
        // 記上去之後 `recent_fallback_turn`（只找 native id 為空的）就找不到這一回合，它真正的晚到 hook 沒地方收。
        if same_turn {
            stamp_late_hook_keys(&mut tx, &turn.id, session_id, native_turn_id, event_id).await?;
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
    stamp_late_hook_keys(&mut tx, &turn.id, session_id, native_turn_id, event_id).await?;
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
    event_id: Option<&str>,
) -> Result<()> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, source FROM messages WHERE turn_id=? AND role='assistant' ORDER BY created_at DESC, rowid DESC")
            .bind(&turn.id)
            .fetch_all(&mut *tx)
            .await?;
    let Some((latest, _)) = rows.first().filter(|_| rows.iter().all(|(_, src)| src == "terminal_fallback")) else {
        // 有證據是這一回合的 hook（呼叫端才會走到這裡），只是已經有 hook 寫的回覆：鑰匙照記。
        stamp_late_hook_keys(&mut tx, &turn.id, session_id, native_turn_id, event_id).await?;
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
    stamp_late_hook_keys(&mut tx, &turn.id, session_id, native_turn_id, event_id).await?;
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

/// #1760：這個 run 在飛、已打字送出（`delivered_at`）卻沒有逐字證據的回合，在送出之後收到 PostToolUse 就補 `delivery_verified=1`。
/// `at` 是 hook 在 bot 那台機器上的時間（遠端 spool 晚到也不會被當成送出前的活動）；沒有就用現在。重送閘門看 `auto_resend`，不動。
async fn confirm_delivery_by_tool_use<H: HookHost>(app: &H, run: &db::Run, at: Option<&str>) -> Result<()> {
    let at = at.map(str::to_string).unwrap_or_else(db::now);
    let verified: Option<String> = sqlx::query_scalar(
        "UPDATE turns SET delivery_verified = 1
          WHERE run_id = ? AND status = 'in_flight' AND delivery = 'ok' AND delivery_verified = 0
            AND delivered_at IS NOT NULL AND delivered_at <= ?
          RETURNING id",
    )
    .bind(&run.id)
    .bind(&at)
    .fetch_optional(app.db())
    .await?;
    if let Some(turn) = verified {
        tracing::info!(turn = %turn, run = %run.id, "tool activity after delivery proves the prompt arrived");
        app.emit_turn(&turn).await;
    }
    Ok(())
}

/// #1969：agy 1.4.7 起 PreInvocation hook 帶 `lastUserInput`。
/// 當 run 處於本機、hook 通過世代圍籬（admitted 是 Some）、非 truncated，且為 PreInvocation 時，
/// 若帶有非空 `lastUserInput`，在嚴格 CRLF/LF 正規化與 trim 全等、時間解析且 delivered_at <= fired <= now + 5s、
/// 且在同 run 內存在唯一匹配的 in_flight / delivery='ok' / delivery_verified=0 / 非 external turn 時，
/// 將其 `delivery_verified` 更新為 1 並 emit turn。
/// 僅補事後 verified 證據，不作為同步唯一 proof；auto_resend 與重送額度完全不動。
async fn confirm_agy_pre_invocation_delivery<H: HookHost>(
    app: &H,
    run: &db::Run,
    body: &HookBody,
    is_local: bool,
    admitted: bool,
) -> Result<()> {
    if !is_local || !admitted || body.truncated {
        return Ok(());
    }
    let event = body.payload.get("hookEventName")
        .or_else(|| body.payload.get("hook_event_name"))
        .and_then(Value::as_str);
    if !event.is_some_and(|e| e.eq_ignore_ascii_case("preinvocation")) {
        return Ok(());
    }
    let last_user_input = body.payload.get("lastUserInput")
        .or_else(|| body.payload.get("last_user_input"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(input) = last_user_input else { return Ok(()) };

    let fired_at = match body.received_at.as_deref().and_then(|r| chrono::DateTime::parse_from_rfc3339(r).ok()) {
        Some(dt) => dt.with_timezone(&chrono::Utc),
        None => return Ok(()),
    };
    let now_utc = chrono::Utc::now();
    if fired_at > now_utc + chrono::Duration::seconds(5) {
        return Ok(());
    }

    let rows: Vec<(String, Option<String>, Option<String>, i64)> = sqlx::query_as(
        "SELECT id, prompt_text, delivered_at, delivery_verified FROM turns
          WHERE run_id = ? AND status = 'in_flight' AND delivery = 'ok'
            AND origin != 'external'",
    )
    .bind(&run.id)
    .fetch_all(app.db())
    .await?;

    let normalize = |s: &str| s.trim().replace("\r\n", "\n");
    let norm_input = normalize(input);

    let mut matches = Vec::new();
    for (id, prompt_opt, delivered_opt, verified) in rows {
        let Some(prompt) = prompt_opt else { continue };
        let Some(deliv_str) = delivered_opt else { continue };
        let Ok(deliv_dt) = chrono::DateTime::parse_from_rfc3339(&deliv_str) else { continue };
        let deliv_utc = deliv_dt.with_timezone(&chrono::Utc);

        if deliv_utc <= fired_at && normalize(&prompt) == norm_input {
            matches.push((id, deliv_str, verified));
        }
    }

    // 唯一性守衛：候選唯一性先看同文 in_flight 集合；若有兩筆以上相同候選（不論是否已 verified），一筆都不補
    if matches.len() != 1 {
        return Ok(());
    }
    let (target_id, target_deliv_str, verified) = &matches[0];
    if *verified != 0 {
        return Ok(());
    }

    // CAS 條件防更新競態：比對 id、run_id、status、delivery、delivery_verified、delivered_at
    let affected = sqlx::query(
        "UPDATE turns SET delivery_verified = 1
          WHERE id = ? AND run_id = ? AND status = 'in_flight' AND delivery = 'ok'
            AND delivery_verified = 0 AND delivered_at = ?",
    )
    .bind(target_id)
    .bind(&run.id)
    .bind(target_deliv_str)
    .execute(app.db())
    .await?
    .rows_affected();

    // 只有 UPDATE 真正 0 -> 1 才 emit
    if affected > 0 {
        tracing::info!(turn = %target_id, run = %run.id, "agy PreInvocation lastUserInput matched prompt; delivery verified");
        app.emit_turn(target_id).await;
    }

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
        lifecycle::resume_context::invalidate(app.db(), &run.id, reported).await?;
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

/// Grok Stop 當成「帳號又能跑了」的證據時，要比對撞限時刻的那一回合的送出時間（`turns.delivered_at`）；不夠格回 `None`（不清）。
///
/// 這一則 Stop 收的是同一個 run 唯一的 in-flight 回合（`turns_one_in_flight`），而且必須 `hit.at < delivered_at <= received_at`：
/// 回合是撞限之後才送出的，Stop 又是送出之後才觸發的。`clear_limit_hit_if_older_than` 負責 `hit.at < success_at` 那一半，
/// 這裡傳 `delivered_at`；先送出、撞限發生在回合中間（別的 bot 同帳號撞的）的回合，Stop 再晚也不清。
/// - 回合要是 daemon 送出的（`delivery='ok'`、有 `delivered_at`）；`delivered_at`、`received_at` 缺或解不開、比現在晚都不清。
/// - 被比對的只有這一個 `in_flight` 回合；被備援收成 `completed_fallback` 的那一筆不算（不是 in-flight，晚到的 Stop 補回它時不清）。
///   不看前序回合、不看整個 run：已知殘餘風險是舊回合（被備援收掉）的晚到 Stop 被當成新回合的 Stop，AGM 已接受（SPEC §12.7）。
/// - `hit.at`、`delivered_at` 是 daemon 的時鐘，`received_at` 是 hook 所在機器的：只認本機，遠端一律不清。
/// - 回覆要非空，而且不是撞限／API 錯誤的字（`is_quota_exhaustion`、畫面的撞限行、`Turn failed:`／`API error` 開頭）。
async fn grok_success_at(app: &impl HookHost, bot: &db::Bot, run: &db::Run, assistant: &str, received_at: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    if bot.kind != "grok" || assistant.trim().is_empty() || grok_reply_is_failure(assistant) {
        return None;
    }
    let parse = |t: &str| chrono::DateTime::parse_from_rfc3339(t).ok().map(|t| t.with_timezone(&chrono::Utc));
    let fired = parse(received_at?)?;
    if fired > chrono::Utc::now() + chrono::Duration::seconds(5) {
        return None;
    }
    if !matches!(db::bot_host(app.db(), &bot.id).await, Ok(h) if h == crate::config::LOCAL_HOST) {
        return None;
    }
    let turn = db::in_flight_turn(app.db(), &run.id).await.ok().flatten()?;
    let delivered_text = turn.delivered_at.as_deref()?;
    let delivered = parse(delivered_text)?;
    if turn.delivery != "ok" || turn.origin == "external" || delivered > fired {
        return None;
    }
    Some(delivered)
}

fn grok_reply_is_failure(reply: &str) -> bool {
    if crate::turn_error::is_quota_exhaustion(reply) {
        return true;
    }
    reply.lines().any(|l| {
        let low = l.trim().to_ascii_lowercase();
        crate::lifecycle::grok_limit_hit_line(l).is_some() || low.starts_with("turn failed") || low.starts_with("api error")
    })
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

/// 同一筆送兩次（重試、spool 重播）：已經收過的那一回合。
async fn seen_turn<H: HookHost>(app: &H, bot: &db::Bot, session_id: Option<&str>, turn_id: Option<&str>) -> Result<Option<String>> {
    let (Some(sid), Some(tid)) = (session_id, turn_id) else { return Ok(None) };
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT t.id FROM turns t JOIN conversations c ON c.id = t.conversation_id
          WHERE c.bot_id=? AND t.native_session_id=? AND t.native_turn_id=?",
    )
    .bind(&bot.id)
    .bind(sid)
    .bind(tid)
    .fetch_optional(app.db())
    .await?)
}

/// #1259：移交查詢讀不出來時，這一則唯一會做的寫入：撞額度的記帳（#108）。規則與正常路徑相同：只記這一代 run 送來的、
/// 主機已換代的不記、同一則已經收過的不再記。讀不到主機或寫不進去就是錯誤，由呼叫端回給收件匣重試。
async fn record_limit_hit_only<H: HookHost>(app: &H, body: &HookBody, bot: &db::Bot, run: Option<&db::Run>, kind: &HookKind) -> Result<()> {
    let HookKind::TurnFailed { session_id, turn_id, reason, detail, .. } = kind else { return Ok(()) };
    if *reason == FailureReason::Interrupted {
        return Ok(());
    }
    let Some(d) = detail.as_deref().filter(|d| crate::turn_error::is_quota_exhaustion(d)) else { return Ok(()) };
    let Some(r) = run else { return Ok(()) };
    let host_moved = match db::run_host_generation(app.db(), &r.id).await.ok().flatten() {
        Some(was) => app.admitted_host_fence(bot).await.is_some_and(|f| f.target_epoch().is_some_and(|now| now != was)),
        None => false,
    };
    if host_moved {
        return Ok(());
    }
    let ev = crate::lifecycle::fence::EventIdentity { run_id: body.run_id.as_deref(), session_id: hook_session_id(&body.payload) };
    if app.db().classify_event_owner(&bot.id, r, ev).await.admit(&r.id).is_none() {
        return Ok(());
    }
    if seen_turn(app, bot, session_id.as_deref(), turn_id.as_deref()).await?.is_some() {
        return Ok(());
    }
    match bot.kind.as_str() {
        "claude" => app.mark_claude_limit_hit(bot, d).await?,
        "agy" => app.mark_agy_limit_hit(bot, d).await?,
        _ => {}
    }
    Ok(())
}

pub async fn process_locked_for<H: HookHost>(app: &H, body: &HookBody, event_id: Option<&str>) -> Result<()> {
    let mut accepted_turn_id = None;
    process_locked_for_with_outcome(app, body, event_id, &mut accepted_turn_id).await
}

async fn process_locked_for_with_outcome<H: HookHost>(
    app: &H,
    body: &HookBody,
    event_id: Option<&str>,
    accepted_turn_id: &mut Option<String>,
) -> Result<()> {
    let Some(bot) = db::bot(app.db(), &body.bot_id).await? else { return Ok(()) };
    // A3: also guards the spool-replay path, where nothing checked the token.
    if bot.deleted_at.is_some() {
        tracing::info!(bot = %bot.name, "hook for a deleted bot; ignored");
        return Ok(());
    }
    // #708：移交出去的專案，hook 的事歸接手的 daemon；記一行就丟（spool 重播、spawn hint 也一樣）。
    // #1259：讀不到＝不知道專案歸誰。不當成「我的」去處理，也不當成「移交了」去丟：下面會只做撞額度的記帳，然後回錯讓收件匣重試。
    let handoff_unknown = match app.db().bot_handed_off_to(&bot.id).await {
        Ok(None) => None,
        Ok(Some(to)) => {
            tracing::info!(bot = %bot.name, provider = %body.provider, handed_off_to = %to, "hook for a handed-off project; ignored");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "could not tell whether the hook's project was handed off; will retry");
            Some(e)
        }
    };
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
    // #1760：送出當下沒有逐字證據的回合（遠端 claude 不送 UserPromptSubmit、補送過一次），送出之後 agent 跑了工具就是收到了；
    // 以前要等 Stop 收尾（#1471）才補，回合進行中一直掛著「未驗證送達」。
    if provider == "claude" && body.payload.get("hook_event_name").and_then(Value::as_str) == Some("PostToolUse") {
        if let Some(r) = &run {
            confirm_delivery_by_tool_use(app, r, body.received_at.as_deref()).await?;
        }
    }
    if matches!(kind, HookKind::StatusLine) {
        tracing::debug!(bot = %bot.name, "statusline received");
    } else {
        tracing::info!(bot = %bot.name, provider = %body.provider, ?kind, "hook received");
    }

    // #1259：移交查詢讀不出來時，這一則不做任何 hook 副作用（不認領、不收回合、不寫訊息、不送子通知、不清旗標）。
    // 唯一的例外是撞額度的記帳（#108）：撞限是帳號的事實，記下來排著的派工才擋得住。記完回錯，收件匣退避重試。
    if let Some(e) = handoff_unknown {
        record_limit_hit_only(app, body, &bot, run.as_ref(), &kind).await?;
        return Err(e.context("could not tell whether the hook's project was handed off; retrying"));
    }
    // 壓縮中的標記（`compacting.rs`，#1770）收掉的時機：壓縮做完的 SessionStart（source=compact）、回合收尾的 Stop／StopFailure。
    let compact_ends = match &kind {
        HookKind::TurnComplete { .. } | HookKind::TurnFailed { .. } => true,
        HookKind::Identity { .. } => body.payload.get("source").and_then(Value::as_str) == Some("compact"),
        _ => false,
    };
    if let (true, Some(r)) = (compact_ends, &run) {
        if am_base::compacting::end(&r.id) {
            app.emit_bot_status(&bot.id).await;
        }
    }

    // 世代圍籬（issue #69）：這一則屬於哪一代。只在這裡問一次，`fence` 是唯一的規則所在地——
    // 散在 hook／reconcile／fallback 各判一次，遲早會漂成三套。舊世代的事件只記錄，一個欄位都不改。
    // 放行的證明（`admitted`）是 hook 改 Turn 的前提：`turn_controller` 那兩支沒有它就不能呼叫（issue #125）。
    let mut admitted = None;
    // 放行的那一代主機世代（#1024）：agy 的授權失敗只改這一代，換代了就不改。
    let mut admitted_fence = None;
    if let Some(r) = &run {
        // 主機換機（#1035）：這一run 啟動時所在的遠端主機名已經指到別台（`?confirm=repoint` 換了 ssh 目標）＝這一則是上一代主機的，
        // 跟上面的 run 世代一樣：只記錄、不改任何欄位。比的是主機目標指紋（`target_epoch`），不是連線世代：
        // 同一台機器改 remote_path、重連都不算換機器（#1773）。
        let run_host_generation = db::run_host_generation(app.db(), &r.id).await.ok().flatten();
        let host_moved = match run_host_generation {
            Some(was) => app.admitted_host_fence(&bot).await.is_some_and(|f| f.target_epoch().is_some_and(|now| now != was)),
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
        // Spawn claims describe a bot/pane fact, not a turn in the parent's current run.
        if !owner.may_mutate() && !matches!(&kind, HookKind::SpawnClaim { .. }) {
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
    // ＝授權失敗；綁著身份的 bot 正常答完一回合＝那個身份是通的。
    if (provider == "claude" || provider == "codex") && admitted.is_some() {
        if let HookKind::TurnComplete { assistant: Some(a), .. } = &kind {
            if crate::login_prompt::is_auth_failure_line(&provider, a) {
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

    // 撞限的 Grok 回合不以 `end_turn` 收尾，所以一則通過世代圍籬的 end_turn Stop 代表這個帳號答得了話。但只有「撞限之後才送出」的那一回合才能當證據，
    // 週限誤清會影響同身份的所有 bot：`grok_success_at` 取 daemon 記的 `turns.delivered_at` 當 `success_at`，不用 Stop 的時間（#1872）。
    if provider == "grok" && admitted.is_some() {
        if let (HookKind::TurnComplete { assistant: Some(a), .. }, Some(r)) = (&kind, run.as_ref()) {
            if let Some(success_at) = grok_success_at(app, &bot, r, a, body.received_at.as_deref()).await {
                app.clear_limit_hit_for_bot_if_older_than(&bot, success_at).await;
            }
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
            // 登入失效：記下這個身份要重新登入、立刻重探它（網頁會跳提示，`login_prompt.rs`）。同一則重送只記一次。
            // agy 沒有身份：改把那台主機的 `tools.agy.logged_in` 翻成未登入（`agy_auth.rs`，issue #870）。
            if reason == FailureReason::Auth && (provider == "claude" || provider == "agy") && admitted.is_some() {
                app.login_on_auth_failure(&bot, admitted_fence.as_ref()).await;
            }
            // 同一筆送兩次（重試、spool 重播）：已經收過的那一回合。
            let seen = seen_turn(app, &bot, session_id.as_deref(), turn_id.as_deref()).await?;
            // 撞的是帳號額度（#108、#150）：先記撞限，**再**推回合結束——那個事件會叫醒 queue flush，排著的派工要看得到
            // 這個身份沒額度。撞額度是帳號的事實，跟這一則還有沒有回合可收無關：Esc 收掉回合之後才到、對上中斷的回聲、
            // 回合已被別的路收掉，都一樣要記，不然下一件派工會被送進沒額度的身份。只記這一代 run 送來的（`admitted`：
            // 上一代的在圍籬就丟了；沒有 run 時說不準是哪個身份）；已經收過的同一則不再記，免得把撞限時刻往後推。
            // 記不進去（讀不到主機、身份表還沒進來、憑據寫不進去）就讓這一則失敗、由收件匣重試（#108 重開）：回合先不收、
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
            let native = native_evidence(app, &bot.id, session_id.as_deref(), turn_id.as_deref()).await?;
            let mut tx = app.db().begin().await?;
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
            *accepted_turn_id = Some(t.id.clone());
            // 撞限在上面（進這一支之前）就記好了：畫面那條路（`turn_error::capture`）要等讀 pane 才記得到，常常比 flush 晚。
            app.emit_message_added(&bot.id, message).await;
            app.emit_turn(&t.id).await;
            tracing::warn!(bot = %bot.name, turn = %t.id, ?reason, "StopFailure：回合收成失敗");
            Ok(())
        }
        // agy 的 statusLine 只用來補身份（對話一建立就有 `conversation_id`／`transcript_path`）。它的 `agent_state`／
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
            // 還會拿它去校正、作廢本機身份真的撞限（`quota::set`）。
            let host = db::bot_host(app.db(), &bot.id).await?;
            // 讀數是這個 run 的 pane 送來的：帳號是它起來時的身份（issue #238），不是剛改、還沒重啟生效的設定。
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
                if provider == "claude" {
                    lifecycle::resume_context::retain_until_usage(&mut rich, r.status_json.as_deref());
                }
                let rich = serde_json::to_string(&rich).ok();
                let changed = text != r.status_line.as_deref() || rich != r.status_json;
                if changed {
                    // 快取倒數（`cache_clock`）：API 指紋真的動了才算一次 API 活動，閒置重繪不算。
                    // A restored measurement is not an API baseline: startup redraws must not
                    // make the old session's expired prompt cache look warm again (#1869).
                    if !lifecycle::resume_context::is_restored(r.status_json.as_deref())
                        && !lifecycle::resume_context::is_restored(rich.as_deref())
                    {
                        crate::cache_clock::on_statusline(&r.id, r.status_json.as_deref(), rich.as_deref(), &db::now());
                    }
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
                    // 對 claude 共用預設帳號的身份（沒設 CLAUDE_CONFIG_DIR）寫裸 `claude`：另開 `claude:cc0` 會少掉
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
        // issue #1770：壓縮開始。只標記這顆 run「壓縮中」，不建立、不動任何 Turn；沒有活著的 run 就沒地方標。
        HookKind::CompactStarting => {
            if let Some(r) = &run {
                if am_base::compacting::begin(&r.id, chrono::Utc::now()) {
                    app.emit_bot_status(&bot.id).await;
                }
            }
            Ok(())
        }
        // issue #82：純可見性快照，整筆覆蓋 `runs.subagent_json`，不建立、不動任何 Turn，不查重複
        // （跟 `StatusLine` 一樣，最新的贏），也不碰 `bots`／`parent_bot_id`——那是 §6.5a 血緣認領的事，
        // 這裡的 `agent_id` 是同一行程內 Task 工具呼叫的 id，不是哪個 pane 的身份。沒有活著的 run
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
            reconcile_after_spawn_hint(app, &bot.id).await;
            Ok(())
        }
        // 2026-10-02 使用者：claude 問的題目與使用者的答案要留在對話裡（`ask_answers`）。
        HookKind::AskAnswered(rec) => {
            ask_answers::record_in_flight(app, &bot.id, &conv, vec![rec]).await?;
            Ok(())
        }
        // 2026-09-30 使用者：console-rpa（mac_builder）直送給 cicd 的一句被存成使用者訊息。spool 每 30 秒左右才收一次，
        // 收件方的回音常常比這則報備先到，所以除了照常記下，也回頭補標已經存下的那一則。
        HookKind::RelayAnnounce { to_bot_id, to_run_id, delivery_id, text } => {
            // 讀不到寄件者的主機就不記（記了也沒人認得出是哪台）；補標那邊同樣要讀主機，會一起報錯。
            let host = db::bot_host(app.db(), &bot.id).await?;
            // 寄件 bot 送這句當下正在跑的回合：用 shim 寫進報備的 `received_at`（送出時間；spool 可能晚好幾秒才收到）。
            let sent_at = body.received_at.clone().unwrap_or_else(db::now);
            let from_turn = crate::lifecycle::relay_watch::sender_turn_at(app, &bot.id, &sent_at).await?;
            let Some(run) = db::run(app.db(), &to_run_id).await? else { return Ok(()) };
            if run.bot_id != to_bot_id || run.state != "running" || db::bot_host(app.db(), &to_bot_id).await? != host {
                return Ok(());
            }
            let Some(recipient) = db::bot(app.db(), &to_bot_id).await? else { return Ok(()) };
            let handed_off: Option<String> = sqlx::query_scalar("SELECT handed_off_to FROM projects WHERE id=?")
                .bind(&recipient.project_id).fetch_one(app.db()).await?;
            if recipient.deleted_at.is_some() || handed_off.is_some() { return Ok(()); }
            if crate::agent_relay::announce_delivery(&host, &bot.id, &to_run_id, &delivery_id, &text, from_turn.as_deref()) {
                relay_backfill(app, &bot.id, from_turn.as_deref(), &to_run_id, &text).await?;
            }
            Ok(())
        }
        // issue #1790：遠端 shim 開完子 agent 後的登記（本機走 `/relay/spawn/claim`）；只記「這顆 pane 是這顆 bot 開的」。
        HookKind::SpawnClaim { pane_id, parent_run_id } => {
            // The parent may have slept or restarted while this event waited in the spool.
            crate::spawn_hints::claim_for_run(app, &bot.id, &parent_run_id, &pane_id).await?;
            reconcile_after_spawn_hint(app, &bot.id).await;
            Ok(())
        }
        HookKind::ClaimCollisionCheck { issue, title, child, child_bot_id, claim_id, branch } => {
            app.check_claim_collision(crate::judge::collision::ClaimAsk {
                project_id: bot.project_id.clone(),
                caller: bot.id.clone(),
                issue,
                title,
                child,
                child_bot_id,
                claim_id,
                branch,
            })
            .await?;
            Ok(())
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
                if provider == "claude" {
                    lifecycle::resume_context::on_identity(app.db(), r, session_id.as_deref(),
                        body.payload.get("source").and_then(Value::as_str) == Some("compact")).await?;
                }
                app.emit_bot_status(&bot.id).await;
                // #1759：壓縮做完的 SessionStart。說明寫不進去不擋身份更新。
                if provider == "claude" && body.payload.get("source").and_then(Value::as_str) == Some("compact") {
                    if let Err(e) = crate::compaction::record(app, &bot, r, &body.payload, transcript_path.as_deref()).await {
                        tracing::warn!(bot = %bot.name, error = ?e, "could not record a claude compaction");
                    }
                }
                // #1969：agy 1.4.7 起 PreInvocation hook 帶 `lastUserInput`，在嚴格條件下補記送達證據
                if provider == "agy" {
                    let is_local = matches!(db::bot_host(app.db(), &bot.id).await, Ok(h) if h == crate::config::LOCAL_HOST);
                    confirm_agy_pre_invocation_delivery(app, r, body, is_local, admitted.is_some()).await?;
                }
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
                    *accepted_turn_id = Some(turn);
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
                    if bot.kind == "codex" && assistant.as_deref().unwrap_or("").is_empty() && admitted.is_some() {
                        if let Some(r) = &run {
                            if db::in_flight_turn(app.db(), &r.id).await?.is_some_and(|t| t.id == existing) {
                                let mut tx = app.db().begin().await?;
                                stamp_source_event(&mut tx, &existing, event_id).await?;
                                tx.commit().await?;
                                *accepted_turn_id = Some(existing.clone());
                                app.arm_progress(&r.id, &bot.id, &existing).await;
                                return Ok(());
                            }
                        }
                    }
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
                                fill_or_drop_late_hook(app, &late, &assistant.clone().unwrap_or_default(), &session_id, &turn_id, true, event_id).await?;
                                *accepted_turn_id = Some(late.id.clone());
                                return Ok(());
                            }
                        }
                    }
                    (Some(t), user)
                }
                other => (other, user),
            };
            let body_text = assistant.clone().unwrap_or_default();

            // An empty Codex notify can hide a typed native error. Keep the exact
            // matching turn in flight so the existing rollout fallback can decide.
            if bot.kind == "codex" && body_text.is_empty() {
                if let (Some(t), Some(r), Some(native)) = (&target, &run, turn_id.as_deref()) {
                    if t.native_turn_id.as_deref() == Some(native) && admitted.is_some() {
                        let mut tx = app.db().begin().await?;
                        stamp_source_event(&mut tx, &t.id, event_id).await?;
                        tx.commit().await?;
                        *accepted_turn_id = Some(t.id.clone());
                        app.arm_progress(&r.id, &bot.id, &t.id).await;
                        return Ok(());
                    }
                }
            }

            if let (Some(t), Some(admitted)) = (target, &admitted) {
                // 收尾（連同當作去重鑰匙的 native id）與訊息同一個交易（#115）：訊息寫不進去時整筆回滾，
                // 收件匣重試時才不會被去重擋掉、留下一筆沒有回覆的 completed 回合。
                // Transcript reads can wait on filesystem I/O / the blocking pool. Resolve
                // them before borrowing a connection or taking SQLite's write lock.
                let resent_prompt = if t.origin == "external" && user.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_none() {
                    human_started_prompt(&body.payload, transcript_read_path.as_deref()).await
                } else { None };
                let native = native_evidence(app, &bot.id, session_id.as_deref(), turn_id.as_deref()).await?;
                let mut tx = app.db().begin().await?;
                #[cfg(all(test, feature = "daemon-test-harness"))]
                crate::lifecycle::race_point::hit("hook_before_complete_cas", &t.id).await;
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
                        fill_or_drop_late_hook(app, &t, &body_text, &session_id, &turn_id, true, event_id).await?;
                        store_resent_prompt(app, &bot.id, &conv, &t, run.as_ref(), &body.payload, transcript_read_path.as_deref()).await?;
                        *accepted_turn_id = Some(t.id.clone());
                        return Ok(());
                    }
                    // This hook still writes its reply after losing to another terminal status;
                    // make the inbox replay key atomic with that accepted reply as well.
                    lifecycle::turn_controller::Outcome::Raced { .. } => {
                        stamp_source_event(&mut tx, &t.id, event_id).await?;
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
                            let from = relay_source(&mut *tx, run.as_ref(), u).await;
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
                    } else if let Some(text) = resent_prompt {
                        added.extend(store_resent_prompt_tx(&mut tx, &conv, &t, run.as_ref(), &text).await?);
                    }
                }
                if !body_text.is_empty() {
                    added.push(tx.insert_message_tx(&conv, Some(&t.id), "assistant", &body_text, "hook", false, None).await?);
                }
                tx.commit().await?;
                *accepted_turn_id = Some(t.id.clone());
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
                        fill_or_drop_late_hook(app, &t, &body_text, &session_id, &turn_id, same, event_id).await?;
                        store_resent_prompt(app, &bot.id, &conv, &t, run.as_ref(), &body.payload, transcript_read_path.as_deref()).await?;
                        *accepted_turn_id = Some(t.id.clone());
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
                let from = relay_source(&mut *tx, run.as_ref(), &u).await;
                let msg = tx.insert_message_relayed_tx(&conv, Some(&tid), "user", &u, "hook", false, None, from.as_ref().map(|r| r.from_bot.as_str())).await?;
                crate::lifecycle::messages::mark_relay_turn(&mut tx, &msg.id, from.as_ref().and_then(|r| r.from_turn.as_deref())).await?;
                added.push(msg);
            }
            if !body_text.is_empty() {
                added.push(tx.insert_message_tx(&conv, Some(&tid), "assistant", &body_text, "hook", false, None).await?);
            }
            tx.commit().await?;
            *accepted_turn_id = Some(tid.clone());
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
async fn stamp_source_event(conn: &mut sqlx::SqliteConnection, turn_id: &str, event_id: Option<&str>) -> Result<()> {
    if let Some(event) = event_id {
        sqlx::query("UPDATE turns SET source_event_id = ? WHERE id = ? AND source_event_id IS NULL")
            .bind(event)
            .bind(turn_id)
            .execute(&mut *conn)
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
#[path = "hookrecv_tests/spool_log_privacy_tests.rs"]
mod spool_log_privacy_tests;

/// SPEC §11.4.3.
pub async fn drain_remote<H: HookHost>(app: &H, host: &str, bot_id: &str) -> Result<usize> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    drain_remote_locked(app, host, bot_id).await
}

/// 同 [`drain_remote`]，但**呼叫端已經握著這顆 bot 的鎖**（#1417：定時掃描先 try-lock，鎖被佔就跳過、不排隊）。
pub async fn drain_remote_locked<H: HookHost>(app: &H, host: &str, bot_id: &str) -> Result<usize> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
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
    // 先開 `src`、確認是一般檔：讀不到（壞檔、是目錄）就在碰 `dst` 之前回錯。
    let mut from = std::fs::File::open(src)?;
    if !from.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "spool source is not a regular file"));
    }
    let mut f = std::fs::OpenOptions::new().read(true).append(true).create(true).open(dst)?;
    // 崩在一行寫到一半時尾巴沒有換行：直接接上去，會跟下一份的第一行黏成一行、兩則一起解不開。
    let len = f.metadata()?.len();
    if len > 0 {
        use std::os::unix::fs::FileExt as _;
        let mut last = [0u8; 1];
        f.read_exact_at(&mut last, len - 1)?;
        if last[0] != b'\n' {
            f.write_all(b"\n")?;
        }
    }
    // #1068：串流複製，不把整份 `src` 讀進記憶體（spool 在 daemon 離線時可能長到 GiB）。
    std::io::copy(&mut from, &mut f)?;
    // 寫完、sync 完才刪 `src`；中途崩掉的話兩份都在，重複與半行交給 `dedupe_key` 與 unparseable spool line 的既有保護。
    f.sync_all()?;
    std::fs::remove_file(src)
}

/// 一行 spool 的上限（#1068）。hook_cmd 把 payload 截在 1 MiB，包成 JSON 時跳脫最多放大到 6 倍，這個上限留足空間。
/// 超過的行不保留在記憶體裡，整行丟掉並記 warn：它不可能是合法的 hook 行，也不能讓單一一行把 daemon 撐爆。
const MAX_SPOOL_LINE: usize = 8 * 1024 * 1024;

/// [`read_spool_line`] 讀到的一行。`oversized` 為真時 `bytes` 是空的（超過上限的內容已丟掉）。
struct SpoolLine {
    bytes: Vec<u8>,
    oversized: bool,
}

/// 讀下一行（不含換行），記憶體上界是 [`MAX_SPOOL_LINE`]。EOF 回 `None`；最後一行沒有換行也照樣回。
async fn read_spool_line<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> std::io::Result<Option<SpoolLine>> {
    use tokio::io::AsyncBufReadExt as _;
    let mut out = SpoolLine { bytes: Vec::new(), oversized: false };
    let mut seen = false;
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Ok(seen.then_some(out));
        }
        seen = true;
        let (take, done) = match buf.iter().position(|&c| c == b'\n') {
            Some(i) => (i, true),
            None => (buf.len(), false),
        };
        if !out.oversized {
            if out.bytes.len() + take > MAX_SPOOL_LINE {
                out.oversized = true;
                out.bytes = Vec::new();
            } else {
                out.bytes.extend_from_slice(&buf[..take]);
            }
        }
        r.consume(if done { take + 1 } else { take });
        if done {
            return Ok(Some(out));
        }
    }
}

/// [`fold_spool`] 是同步的檔案 I/O，丟到 blocking 執行緒去跑，不佔 Tokio 的 worker（#1068）。
async fn fold_spool_blocking(src: std::path::PathBuf, dst: std::path::PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || fold_spool(&src, &dst)).await??;
    Ok(())
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
    replay_spool_locked(app, bot_id).await
}

/// 同 [`replay_spool`]，但本機 bot 的鎖被別人握著就回 `Ok(None)`，不排隊等（#1067）：定時掃描是全域共用的一輪，
/// 一顆 bot 握著鎖（啟動等就緒、批次重啟）不能拖住別顆本機 bot 與遠端主機。spool 還在，下一輪再收。只給本機 bot。
pub async fn replay_spool_if_idle<H: HookHost>(app: &H, bot_id: &str) -> Result<Option<usize>> {
    let lock = app.bot_lock(bot_id).await;
    let Ok(_g) = lock.try_lock() else { return Ok(None) };
    replay_spool_locked(app, bot_id).await.map(Some)
}

/// 已經握著 bot 鎖之後的本機重放本體；呼叫端負責鎖。
async fn replay_spool_locked<H: HookHost>(app: &H, bot_id: &str) -> Result<usize> {
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
    fold_spool_blocking(claim.clone(), staging.clone()).await?;
    if spool.exists() {
        // #493：**先 rename 再讀**。以前是 `read(spool)` → 整份寫回 `.replaying` → `remove_file(spool)`，
        // 中間那一大段（寫一份完整的 staging）裡 hook 附加進來的行，會在最後那個 remove 被連檔刪掉。
        // rename 是原子的：之後 append 的行落在新的 inode 上，不在這一輪手上，也就刪不到。
        // #1256：認領也拿 spool 目錄的鎖，跟 hook 行程的超量丟棄互斥（`am_base::hook_cmd::lock_spool_dir`）。
        let (s, c, d) = (spool.clone(), claim.clone(), dir.clone());
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let _lock = am_base::hook_cmd::lock_spool_dir(&d)?;
            std::fs::rename(&s, &c)
        })
        .await??;
        #[cfg(all(test, feature = "daemon-test-harness"))]
        crate::lifecycle::race_point::hit("spool_claimed", bot_id).await;
        fold_spool_blocking(claim.clone(), staging.clone()).await?;
    }
    if !staging.exists() {
        return Ok(0);
    }
    // #1068：逐行讀，不把整份 `.replaying` 讀進記憶體；超過上限的行整行丟掉（不可能是合法的 hook）。
    let mut reader = tokio::io::BufReader::new(tokio::fs::File::open(&staging).await?);
    let mut n = 0usize;
    while let Some(raw) = read_spool_line(&mut reader).await? {
        if raw.oversized {
            tracing::warn!(limit = MAX_SPOOL_LINE, "oversized spool line dropped");
            continue;
        }
        let text = String::from_utf8_lossy(&raw.bytes);
        let line = text.trim();
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
#[path = "hookrecv_tests/drain_tests.rs"]
mod drain_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/external_claim_tests.rs"]
mod external_claim_tests;

/// issue #70：`200` 必須代表「已經耐久收下」。
#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/durable_handoff_tests.rs"]
mod durable_handoff_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/resume_tests.rs"]
mod resume_tests;

/// codex 答完一回合清撞限：清的要是**這顆 bot 自己那把 key**（跟寫入端同一支 `quota_base_for_host`）。
#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/codex_limit_clear_tests.rs"]
mod codex_limit_clear_tests;

/// Grok 撞限解除：只有「撞限之後才送出」的 in-flight 回合收到的 end_turn Stop 才清，`hit.at < turn.delivered_at <= received_at`（#1872）。
/// 全部走 `process_locked`（真的 hook 入口：圍籬、分類、清除一條路），不只測 helper。
#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/grok_limit_hit_success_tests.rs"]
mod grok_limit_hit_success_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/codex_title_tests.rs"]
mod codex_title_tests;

/// #493：spool 的收攏是「先 rename 再讀」——claim 與 ack 之間 hook 附加進來的行不准被刪掉。
#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/spool_claim_window_tests.rs"]
mod spool_claim_window_tests;

/// #243：讀不到 bot 的 host 不能當成本機——spool 重放要回錯、事件留著等下一輪，DB 好了要補回來。
#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/host_unreadable_replay_tests.rs"]
mod host_unreadable_replay_tests;

/// agy（Antigravity CLI）：payload 沒有事件名（hook 子行程放進 `hookEventName`），`Stop` 也沒有助理文字（`lastAssistantMessage` 是子行程讀 transcript 補的）。
#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/agy_tests.rs"]
mod agy_tests;

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
    + crate::capabilities::HerdrRoutes
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
    + crate::events::ports::ReconcileCommands
    + Send
    + Sync
{
    /// 叫醒 durable hook 收件匣的 worker（commit 之後才叫）。
    fn wake_hook_inbox(&self);
    /// 處理遠端 issue claim 的撞題查詢；hook inbox 只有在 durable reservation／結果已落帳後才 ACK。
    fn check_claim_collision<'a>(
        &'a self,
        ask: crate::judge::collision::ClaimAsk,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'a;
    /// 本機這顆 bot 的資料夾（spool 在裡面）。
    fn hook_bot_dir(&self, bot_id: &str) -> Result<std::path::PathBuf>;
    /// 回合結束 hook 處理完之後補讀 AskUserQuestion 的答案。
    fn after_turn_end<'a>(
        &'a self,
        body: &'a HookBody,
        run_id: Option<&'a str>,
        turn_id: &'a str,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'a;
    /// claude Stop hook 帶來的背景工作數字。
    fn background_stop<'a>(&'a self, run: &'a db::Run, payload: &'a Value) -> impl std::future::Future<Output = ()> + Send + 'a;
    /// transcript 路徑是不是這顆 bot 讀得的（遠端／本機分別把關）。
    fn transcript_allowed<'a>(&'a self, bot: &'a db::Bot, path: &'a str) -> impl std::future::Future<Output = bool> + Send + 'a;
    fn local_transcript_allowed<'a>(&'a self, bot: &'a db::Bot, path: &'a str) -> impl std::future::Future<Output = bool> + Send + 'a;
}

/// 線索晚到（2026-10-19 mac_builder `hub-5j3cjh-draft`）：遠端 hook 走 spool，比 `pane_agent_detected` 觸發的那輪對帳晚 20～50 秒，
/// 那輪沒有線索就不認領，之後要等不相干的事件才再對帳——子 agent 在側欄消失了 9 分鐘。線索一記下就排一輪延後對帳。
async fn reconcile_after_spawn_hint<H: HookHost>(app: &H, bot_id: &str) {
    match db::bot_host(app.db(), bot_id).await {
        Ok(host) => app.schedule_deferred_pass(&host),
        Err(e) => tracing::debug!(bot = bot_id, error = ?e, "spawn hint: cannot read the bot's host; adoption waits for the next pass"),
    }
}

/// issue #1259：移交查詢讀不出來＝不知道專案歸誰，不准當成「我的」去處理。
#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/handoff_lookup_tests.rs"]
mod handoff_lookup_tests;

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "hookrecv_tests/agy_pre_invocation_delivery_tests.rs"]
mod agy_pre_invocation_delivery_tests;
