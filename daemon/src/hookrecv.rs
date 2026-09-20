//! Hook receiver: `POST /hook/{provider}` plus Turn matching (SPEC §6.7) and spool replay (§4.4.6).

use crate::db;
use crate::config::{valid_id, ID_RE};
use crate::lifecycle;
use crate::hosts::sh_quote;
use crate::state::App;
use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

// `Serialize` 是給 `hook_inbox` 用的：整個 body 要原封不動存進收件匣再讀回來走 §6.7。
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct HookBody {
    pub bot_id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub received_at: Option<String>,
    #[serde(default)]
    pub truncated: bool,
    /// 送出這則 hook 的 CLI 行程是替哪個 run 起的（pane env 的 `AM_RUN_ID`，issue #92）。
    /// 世代圍籬用它分辨「同一個 session、不同行程」：`--resume` 接回時新舊行程的 session id 一樣。
    /// 舊行程、遠端舊版 `hook.sh`、手寫的 body 沒有這一欄——那就照舊只看 session。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// 驗 per-bot token → **寫進耐久收件匣並 commit** → 才回 200（SPEC §3.1、issue #70）。
///
/// `200` 的意思是「這則事件已經寫進 `hook_events`」，不是「已經處理完」。寫不進去就回 503：
/// 送端（`hook_cmd::inner`）看到非 2xx 會把同一份 body 追加到 `hook-spool.jsonl`，replay 會補回來。
/// 回 200 再掉事件是無聲的資料遺失，回 503 只是讓那則事件多繞一趟 spool。
///
/// StatusLine 例外，照舊 fire-and-forget（理由見 [`crate::hook_inbox`]）。
pub async fn receive(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Json(body): Json<HookBody>,
) -> (StatusCode, Json<Value>) {
    let token = headers.get("X-AM-Bot-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let expected = match db::bot(&app.db, &body.bot_id).await {
        // A3: a deleted bot's surviving agent must not be able to create turns / messages.
        Ok(Some(b)) if b.deleted_at.is_some() => {
            return (StatusCode::GONE, Json(json!({"error": "bot deleted"})));
        }
        Ok(Some(b)) => b.hook_token,
        _ => {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unknown bot"})));
        }
    };
    if token.is_empty() || !crate::api::ct_eq(token, &expected) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "bad token"})));
    }
    let provider = if body.provider.is_empty() { provider } else { body.provider.clone() };
    let mut b = body;
    b.provider = provider;

    // 單槽、最新的贏的訊號：不進佇列，掉一格只是晚一次重繪（`hook_inbox` 模組說明）。
    if matches!(classify(&b.provider, &b.payload), HookKind::StatusLine) {
        let app2 = app.clone();
        tokio::spawn(async move {
            if let Err(e) = process(&app2, &b).await {
                tracing::error!(error = ?e, "statusline processing failed");
            }
        });
        return (StatusCode::OK, Json(json!({})));
    }

    match crate::hook_inbox::accept(&app.db, &b, crate::hook_inbox::Source::Http).await {
        Ok(accepted) => {
            // commit 之後才叫醒 worker：醒來一定看得到那一列。
            app.hook_inbox_wake.notify_one();
            (StatusCode::OK, Json(json!({"stored": accepted.is_new()})))
        }
        Err(e) => {
            tracing::error!(error = ?e, bot = %b.bot_id, "hook not persisted; telling the sender to spool it");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "not persisted"})))
        }
    }
}

/// 這句 prompt 回音是別的 agent 打進來的嗎？（見 SPEC §6.5d）
/// 認不出來就當使用者自己打的——寧可少標一次，也不要冤枉一句話。
fn relay_source(run: Option<&db::Run>, echo: &str) -> Option<String> {
    let agent = run?.agent_name.as_deref()?;
    crate::agent_relay::claim(agent, echo)
}

#[derive(Debug)]
enum HookKind {
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
    /// issue #94：`PostToolUse` on the Bash tool whose stdout was herdr's own `pane:split` /
    /// `agent:start` response — this bot's own tool call just created `pane_id`. Recorded as a
    /// spawn hint for `reconcile::adopt_child`; never touches a Turn.
    SpawnHint { pane_id: String },
    Ignore(String),
}

/// `StopFailure` 說的是哪一種失敗。分得出來就留著（`rate limit` 跟其他錯誤要分得開），
/// 分不出來是 `Unknown`——「回合失敗了」本身就是一級訊號，不必等分類到位才收回合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureReason {
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
pub(crate) fn classify_failure(detail: Option<&str>) -> FailureReason {
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
            Some(format!("身份 {name} 在 {host} 沒有登入：claude 會退回這台機器 Keychain 裡預設（cc0）的帳號執行。請在這個 Bot 按「登入 / 切換帳號」。")),
        ),
        (_, Some(i)) if i.logged_in != Some(false) && i.account.is_some() => (i.account.clone(), None),
        _ => (None, None),
    }
}

async fn claude_account(app: &Arc<App>, host: &str, identity: Option<&str>) -> (Option<String>, Option<String>) {
    let tools = app.tools.lock().await;
    claude_account_from_tools(&tools, host, identity)
}

#[cfg(test)]
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
    ["session_id", "sessionId", "thread-id"]
        .iter()
        .find_map(|k| p.get(*k).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn classify(provider: &str, p: &Value) -> HookKind {
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).map(String::from);
    match provider {
        "claude" => {
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
                // "was this herdr creating a pane" decision is `crate::spawn_hints::extract_pane_id`.
                "PostToolUse" => match crate::spawn_hints::extract_pane_id(p) {
                    Some(pane_id) => HookKind::SpawnHint { pane_id },
                    None => HookKind::Ignore("PostToolUse (not a herdr spawn)".into()),
                },
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
            let is_title_turn = user.as_deref().map(|u| u.contains("single-line task title")).unwrap_or(false)
                || assistant
                    .as_deref()
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .map(|v| v.as_object().map(|o| o.len() == 1 && o.contains_key("title")).unwrap_or(false))
                    .unwrap_or(false);
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
        other => HookKind::Ignore(format!("unknown provider {other}")),
    }
}

#[cfg(test)]
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
pub async fn process(app: &Arc<App>, body: &HookBody) -> Result<()> {
    let lock = app.bot_lock(&body.bot_id).await;
    let _g = lock.lock().await;
    process_locked(app, body).await
}

/// A pane-wrapped echo must still compare equal to the hook's single-line copy.
fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The scraped echo may be wrapped or clipped at the column width, so containment either way
/// counts as the same message (equality alone would add a second bubble).
/// 這一回合的使用者訊息原文：codex 的 hook 直接帶；claude 的 Stop 沒帶，從 transcript 尾巴找最後一則。
/// 讀不到就是 `None`（沒有證據，呼叫端照舊認領）。
async fn hook_user_text(from_hook: Option<&str>, transcript_path: Option<&str>) -> Option<String> {
    if let Some(u) = from_hook.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(u.to_string());
    }
    let path = std::path::PathBuf::from(transcript_path?);
    tokio::task::spawn_blocking(move || last_transcript_user_text(&path)).await.ok().flatten()
}

/// transcript 最後一則使用者訊息。只讀尾巴：回合結束時它一定在最後幾百 KB 裡。
fn last_transcript_user_text(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 512 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    String::from_utf8_lossy(&buf)
        .lines()
        .rev()
        .find_map(crate::lifecycle::transcript_user_text)
        // CLI 把貼上的 prompt 包成 `<pasted_content>`（#218）：存進對話、比對的都是原文。
        .map(|t| crate::lifecycle::pasted_content::original(&t).into_owned())
}

/// 有兩邊的原文、而且怎麼比都對不上，才算「hook 回答的是另一句」。去空白後互相包含就算同一句
/// （刮下來的回音、transcript 的折行都可能截斷一邊）。任一邊沒有就不下判斷。
fn answers_another_prompt(prompt: Option<&str>, hook_user: Option<&str>) -> bool {
    let (Some(p), Some(u)) = (prompt, hook_user) else { return false };
    let (p, u) = (squash_ws(p), squash_ws(u));
    if p.is_empty() || u.is_empty() {
        return false;
    }
    !(p.contains(&u) || u.contains(&p))
}

fn hook_user_is_new(existing: &[String], incoming: &str) -> bool {
    let inc = squash_ws(incoming);
    if inc.is_empty() {
        return false;
    }
    !existing.iter().any(|e| {
        let e = squash_ws(e);
        !e.is_empty() && (e.contains(&inc) || inc.contains(&e))
    })
}

/// 只在既有那則是原文的（去空白）前綴且較短時才覆蓋；不是前綴就是另一句話，不能動。
/// 交易內：跟回合的收尾寫在同一個交易裡（#115）。
async fn upgrade_clipped_user_message(conn: &mut sqlx::SqliteConnection, turn_id: &str, full: &str) -> Result<()> {
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
        return Ok(());
    }
    Ok(())
}

/// 遲到的 hook 撞上備援關掉的回合：沒有 assistant 訊息就用 hook 的回覆補上並改 `completed`，
/// 已有回覆才丟（防一回合兩則）。2026-09-13 GROK 備援 15 秒就關回合、36 秒後的真回覆被丟。
/// 不會重開 c1526f7 的洞：`try_fallback` 認領與寫回覆同一交易、同一把 bot lock，讀到零則就真的是零則。
///
/// native id、升級、回覆寫在同一個交易裡（#115）：native id 是去重的鑰匙，先寫它再寫回覆的話，
/// 回覆那句失敗時收件匣的重試會被去重擋掉，回覆就永遠補不上了。
async fn fill_or_drop_late_hook(
    app: &Arc<App>,
    turn: &db::Turn,
    body_text: &str,
    session_id: &Option<String>,
    native_turn_id: &Option<String>,
) -> Result<()> {
    let mut tx = app.db.begin().await?;
    sqlx::query(
        "UPDATE turns SET native_session_id=COALESCE(?, native_session_id),
                          native_turn_id=COALESCE(?, native_turn_id) WHERE id=?",
    )
    .bind(session_id)
    .bind(native_turn_id)
    .bind(&turn.id)
    .execute(&mut *tx)
    .await?;
    let has_reply: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn.id)
            .fetch_one(&mut *tx)
            .await?;
    if body_text.trim().is_empty() || has_reply > 0 {
        tx.commit().await?;
        tracing::info!(turn = %turn.id, has_reply, "late hook dropped; turn already completed via terminal fallback");
        return Ok(());
    }
    // 這一筆是備援關掉的（`completed_fallback`），遲到的 hook 把回覆補上才升級成 `completed`。
    // 以前這句沒有 guard（`WHERE id=?`）：中間若有別的路徑動過它，這裡會無聲蓋過去（issue #68）。
    if lifecycle::turn_controller::set_status_on(&mut tx, &turn.id, "completed_fallback", "completed", "遲到的 hook 補上回覆").await?
        != lifecycle::turn_controller::Outcome::Applied
    {
        tx.commit().await?;
        return Ok(());
    }
    let message =
        lifecycle::insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "assistant", body_text, "hook", false, None).await?;
    tx.commit().await?;
    let bot_id = sqlx::query_scalar::<_, String>("SELECT bot_id FROM conversations WHERE id = ?")
        .bind(&turn.conversation_id)
        .fetch_one(&app.db)
        .await
        .unwrap_or_default();
    lifecycle::emit_message_added(app, &bot_id, message).await;
    tracing::info!(turn = %turn.id, "late hook filled a fallback-closed turn that had no reply");
    lifecycle::emit_turn(app, &turn.id).await;
    Ok(())
}

/// Consume the one-shot `resume_native` request. Clearing the column before recording a mismatch
/// makes retries idempotent.
async fn consume_resume_session(
    app: &Arc<App>,
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
    .execute(&app.db)
    .await?;
    // A second hook may hold a stale `Run` snapshot; only the one that cleared the marker records a mismatch.
    if consumed.rows_affected() == 0 {
        return Ok(());
    }
    if mismatch {
        lifecycle::context_lost(app, bot, "resume_mismatch")
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    // 閘門在等的就是這一則：排著的 prompt 現在可以送了（對不上的話，上面那則說明已經先進聊天室）。
    lifecycle::schedule_flush_queued(app, &bot.id);
    Ok(())
}

/// 120 秒內被終端備援關掉、還沒收到過 native id 的最近一筆回合（遲到的 hook 可能是它的真回覆，§4.3）。
async fn recent_fallback_turn(app: &Arc<App>, run_id: &str) -> Result<Option<db::Turn>> {
    // Fixed-width RFC3339 UTC, so lexicographic comparison is chronological.
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(sqlx::query_as::<_, db::Turn>(
        "SELECT * FROM turns WHERE run_id=? AND status='completed_fallback' AND native_turn_id IS NULL
         AND completed_at > ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(run_id)
    .bind(&cutoff)
    .fetch_optional(&app.db)
    .await?)
}

/// 那一回合被問了什麼：`prompt_text`，沒有就取第一則使用者訊息。
async fn turn_prompt(app: &Arc<App>, t: &db::Turn) -> Result<Option<String>> {
    match t.prompt_text.clone().filter(|p| !p.trim().is_empty()) {
        Some(p) => Ok(Some(p)),
        None => Ok(db::turn_user_messages(&app.db, &t.id).await?.into_iter().next()),
    }
}

pub async fn process_locked(app: &Arc<App>, body: &HookBody) -> Result<()> {
    let Some(bot) = db::bot(&app.db, &body.bot_id).await? else { return Ok(()) };
    // A3: also guards the spool-replay path, where nothing checked the token.
    if bot.deleted_at.is_some() {
        tracing::info!(bot = %bot.name, "hook for a deleted bot; ignored");
        return Ok(());
    }
    let conv = db::conversation_id(&app.db, &bot.id).await?;
    let run = db::active_run(&app.db, &bot.id).await?;
    let kind = classify(&body.provider, &body.payload);
    if matches!(kind, HookKind::StatusLine) {
        tracing::debug!(bot = %bot.name, "statusline received");
    } else {
        tracing::info!(bot = %bot.name, provider = %body.provider, ?kind, "hook received");
    }

    // 世代圍籬（issue #69）：這一則屬於哪一代。只在這裡問一次，`fence` 是唯一的規則所在地——
    // 散在 hook／reconcile／fallback 各判一次，遲早會漂成三套。舊世代的事件只記錄，一個欄位都不改。
    // 放行的證明（`admitted`）是 hook 改 Turn 的前提：`turn_controller` 那兩支沒有它就不能呼叫（issue #125）。
    let mut admitted = None;
    if let Some(r) = &run {
        let ev = crate::lifecycle::fence::EventIdentity { run_id: body.run_id.as_deref(), session_id: hook_session_id(&body.payload) };
        let owner = crate::lifecycle::fence::classify(&app.db, &bot.id, r, ev).await;
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
    }

    // Codex's usage-reset hint is a TUI row, not in the payload; give the pane a moment to render it.
    if body.provider == "codex" && matches!(&kind, HookKind::TurnComplete { .. }) {
        if let Some(r) = run.as_ref() {
            lifecycle::schedule_codex_notice_capture(app, &bot.id, &r.id);
        }
        // 真的答完一回合＝帳號又能跑了，不必等橫幅寫的重置時間。
        if matches!(&kind, HookKind::TurnComplete { assistant: Some(a), .. } if !a.trim().is_empty()) {
            crate::quota::clear_limit_hit_for_bot(app, &bot).await;
        }
    }

    // 上一次打斷欠著的收尾先補（#147）：鍵已經生效、DB 那一半沒寫成的那一筆，要在這一則被對到任何回合之前收掉。
    // 寫不進去就讓這一則失敗、由收件匣重試，順序不亂。
    if matches!(kind, HookKind::TurnComplete { .. } | HookKind::TurnFailed { .. }) {
        lifecycle::settle_interruption(app, &bot.id, lifecycle::InterruptEvidence::Nothing).await?;
        // 送達結果欠著的同理（#149）：herdr 拒收、還沒收成 failed 的那一筆不能被這一則的回覆認領。
        lifecycle::settle_owed_deliveries(app, &bot.id).await?;
    }

    match kind {
        HookKind::Ignore(reason) => {
            tracing::debug!(reason, "hook ignored");
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
                lifecycle::settle_interruption(app, &bot.id, lifecycle::InterruptEvidence::Echo).await?;
                tracing::info!(bot = %bot.name, ?reason, detail, "StopFailure 是使用者中斷的回聲：不算失敗");
                return Ok(());
            }
            // 同一筆送兩次（重試、spool 重播）：已經收過的那一回合。
            let seen = match (&session_id, &turn_id) {
                (Some(sid), Some(tid)) => sqlx::query_scalar::<_, String>("SELECT id FROM turns WHERE native_session_id=? AND native_turn_id=?")
                    .bind(sid)
                    .bind(tid)
                    .fetch_optional(&app.db)
                    .await?,
                _ => None,
            };
            // 撞的是帳號額度（#108、#150）：先記撞限，**再**推回合結束——那個事件會叫醒 queue flush，排著的派工要看得到
            // 這個身分沒額度。撞額度是帳號的事實，跟這一則還有沒有回合可收無關：Esc 收掉回合之後才到、對上中斷的回聲、
            // 回合已被別的路收掉，都一樣要記，不然下一件派工會被送進沒額度的身分。只記這一代 run 送來的（`admitted`：
            // 上一代的在圍籬就丟了；沒有 run 時說不準是哪個身分）；已經收過的同一則不再記，免得把撞限時刻往後推。
            // 記不進去（讀不到主機、身分表還沒進來、憑據寫不進去）就讓這一則失敗、由收件匣重試（#108 重開）：回合先不收、
            // 不推回合結束，排著的派工照欠著的那一筆擋（`turn_error::owed_limit_hit`），不能當作沒撞。
            if bot.kind == "claude" && admitted.is_some() && seen.is_none() {
                if let Some(d) = detail.as_deref().filter(|d| crate::turn_error::is_quota_exhaustion(d)) {
                    crate::turn_error::mark_claude_limit_hit(app, &bot, d).await?;
                }
            }
            if let Some(r) = &run {
                let in_flight = db::in_flight_turn(&app.db, &r.id).await?;
                let ev = lifecycle::InterruptFailureEvidence {
                    session_id: session_id.as_deref(),
                    prompt_id: turn_id.as_deref(),
                    stamped_at: body
                        .received_at
                        .as_deref()
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| t.with_timezone(&chrono::Utc)),
                };
                if lifecycle::settle_interrupt_echo(app, &bot.id, &r.id, &ev, in_flight.as_ref()).await? {
                    tracing::info!(bot = %bot.name, ?reason, detail, "StopFailure 是被中斷那一回合的回聲：不算失敗");
                    return Ok(());
                }
            }
            if let Some(r) = &run {
                sqlx::query(
                    "UPDATE runs SET native_session_id = COALESCE(native_session_id, ?),
                     transcript_path = COALESCE(?, transcript_path) WHERE id = ?",
                )
                .bind(&session_id)
                .bind(&transcript_path)
                .bind(&r.id)
                .execute(&app.db)
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
            let Some(t) = db::in_flight_turn(&app.db, &r.id).await? else {
                tracing::info!(bot = %bot.name, ?reason, detail, "StopFailure 但沒有 in-flight turn：後到的訊號，不開新回合");
                return Ok(());
            };
            // CAS 在 `status='in_flight'` 上：後到的 Stop／§4.3 備援若已經把它收掉，這裡就什麼都不做，
            // 不會變成第二次收尾（issue #79 驗收第二條）。`delivery` 不動——字是送出去了，失敗的是回合。
            // 收尾（連同當作去重鑰匙的 native id）與說明同一個交易（#115）：說明寫不進去時整筆回滾，
            // 收件匣的重試才不會被去重擋掉、留下一筆沒有原因的失敗回合。
            let mut tx = app.db.begin().await?;
            let native = lifecycle::turn_controller::NativeEvidence { session_id: session_id.as_deref(), turn_id: turn_id.as_deref() };
            let claimed = lifecycle::turn_controller::fail_with_native_evidence(&mut tx, &t.id, admitted, native).await?;
            if claimed != lifecycle::turn_controller::Outcome::Applied {
                tracing::info!(turn = %t.id, ?claimed, "StopFailure 來晚了：這一筆已經被別的路徑收掉，不重複收尾");
                return Ok(());
            }
            let note = match detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                Some(d) => format!("這一回合失敗收尾（{}）：{d}", reason.label()),
                None => format!("這一回合失敗收尾（{}）：agent 沒有給原因。", reason.label()),
            };
            let message = lifecycle::insert_message_tx(&mut tx, &conv, Some(&t.id), "system", &note, "hook", false, None).await?;
            tx.commit().await?;
            // 撞限在上面（進這一支之前）就記好了：畫面那條路（`turn_error::capture`）要等讀 pane 才記得到，常常比 flush 晚。
            lifecycle::emit_message_added(app, &bot.id, message).await;
            lifecycle::emit_turn(app, &t.id).await;
            tracing::warn!(bot = %bot.name, turn = %t.id, ?reason, detail, "StopFailure：回合收成失敗");
            Ok(())
        }
        HookKind::StatusLine => {
            // 讀不到主機就丟掉這一份，下一次重繪會再來（#108 重開）：退回 local 會把遠端的讀數寫進本機那一格，
            // 還會拿它去校正、作廢本機身分真的撞限（`quota::set`）。
            let host = db::bot_host(&app.db, &bot.id).await?;
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
                    let _ = sqlx::query("UPDATE runs SET status_line = COALESCE(?, status_line), status_json = ? WHERE id = ?")
                        .bind(text)
                        .bind(&rich)
                        .bind(&r.id)
                        .execute(&app.db)
                        .await;
                    app.emit_bot_status(&bot.id).await;
                }
            }
            // Always keyed under the bot's **host**: remote limits must not land on the local row (SPEC §14).
            if let Some(idn) = identity {
                if let Some(q) = crate::quota::quota_from_statusline(&body.payload, Some(idn)) {
                    // 對 claude 共用預設帳號的身分（沒設 CLAUDE_CONFIG_DIR）寫裸 `claude`：另開 `claude:cc0` 會少掉
                    // `/usage` 探測的 Fable 週窗。規則跟 codex 同一支（`quota::quota_base_for_host`）。
                    let key = crate::quota::quota_base_for_host(app, &host, "claude", Some(idn)).await;
                    crate::quota::set(app, &host, &key, q).await;
                }
            } else if let Some(q) = crate::quota::quota_from_statusline(&body.payload, None) {
                crate::quota::set(app, &host, "claude", q).await;
            }
            if let (Some(r), Some(sid)) = (&run, body.payload.get("session_id").and_then(|v| v.as_str())) {
                let _ = sqlx::query("UPDATE runs SET native_session_id = COALESCE(native_session_id, ?) WHERE id = ?")
                    .bind(sid)
                    .bind(&r.id)
                    .execute(&app.db)
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
                    .execute(&app.db)
                    .await?;
            }
            Ok(())
        }
        // issue #94：這顆 bot 自己剛剛用 `herdr pane split`／`agent start` 開出 `pane_id`——記下來給
        // `reconcile::adopt_child` 當比同 tab 更早、更精確的線索。純粹記錄一個事實，不查任何 bot／pane
        // 現在的狀態，也不需要活著的 run（這是這顆 bot 自己的行程剛做的事，不是它的 Turn 的事）。
        HookKind::SpawnHint { pane_id } => {
            crate::spawn_hints::record(app, &bot.id, &pane_id).await?;
            Ok(())
        }
        HookKind::Identity { session_id, transcript_path } => {
            if let Some(r) = &run {
                // Codex/Grok are checked on their first completed turn; don't consume the request early.
                if body.provider == "claude" {
                    consume_resume_session(app, &bot, r, session_id.as_deref()).await?;
                }
                sqlx::query(
                    "UPDATE runs SET native_session_id = COALESCE(?, native_session_id),
                     transcript_path = COALESCE(?, transcript_path) WHERE id = ?",
                )
                .bind(&session_id)
                .bind(&transcript_path)
                .bind(&r.id)
                .execute(&app.db)
                .await?;
                app.emit_bot_status(&bot.id).await;
            }
            Ok(())
        }
        HookKind::TurnComplete { session_id, turn_id, transcript_path, assistant, user } => {
            if body.provider == "codex" || body.provider == "grok" {
                if let Some(r) = &run {
                    consume_resume_session(app, &bot, r, session_id.as_deref()).await?;
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
                .execute(&app.db)
                .await?;
            }

            // 3. dedup on (native_session_id, native_turn_id)
            if let (Some(sid), Some(tid)) = (&session_id, &turn_id) {
                let dup: Option<String> =
                    sqlx::query_scalar("SELECT id FROM turns WHERE native_session_id=? AND native_turn_id=?")
                        .bind(sid)
                        .bind(tid)
                        .fetch_optional(&app.db)
                        .await?;
                if let Some(existing) = dup {
                    tracing::info!(turn = %existing, "duplicate hook ignored");
                    return Ok(());
                }
            }

            // 4. the run's single in-flight Turn
            let target = match &run {
                Some(r) => db::in_flight_turn(&app.db, &r.id).await?,
                None => None,
            };
            // `unknown`＝打字進去但沒有證據。這時 hook 若看得到**別句**使用者訊息，那是使用者在終端手打的另一句：
            // 答案不能掛到原本那則，更不能順手把它標成「已送達」（第二輪 review 送達線 #3）。看不到使用者訊息時照舊認領。
            let (target, user) = match target {
                Some(t) if t.delivery == "unknown" => {
                    let seen = hook_user_text(user.as_deref(), transcript_path.as_deref()).await;
                    if answers_another_prompt(t.prompt_text.as_deref(), seen.as_deref()) {
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
                            let seen = hook_user_text(user.as_deref(), transcript_path.as_deref()).await;
                            let late_prompt = turn_prompt(app, &late).await?;
                            if answers_another_prompt(t.prompt_text.as_deref(), seen.as_deref())
                                && seen.is_some()
                                && late_prompt.is_some()
                                && !answers_another_prompt(late_prompt.as_deref(), seen.as_deref())
                            {
                                tracing::info!(turn = %t.id, late = %late.id, bot = %bot.id, "遲到 hook 回答的是備援關掉的那一回合，不是現在 in-flight 的：補回那一回合");
                                fill_or_drop_late_hook(app, &late, &assistant.clone().unwrap_or_default(), &session_id, &turn_id).await?;
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
                let mut tx = app.db.begin().await?;
                let native = lifecycle::turn_controller::NativeEvidence { session_id: session_id.as_deref(), turn_id: turn_id.as_deref() };
                let claimed = lifecycle::turn_controller::complete_with_native_evidence(&mut tx, &t.id, admitted, native).await?;
                match &claimed {
                    lifecycle::turn_controller::Outcome::Applied => {}
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
                        fill_or_drop_late_hook(app, &t, &body_text, &session_id, &turn_id).await?;
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
                        if hook_user_is_new(&have, u) {
                            let from = relay_source(run.as_ref(), u);
                            added.push(
                                lifecycle::insert_message_relayed_tx(&mut tx, &conv, Some(&t.id), "user", u, "hook", false, None, from.as_deref())
                                    .await?,
                            );
                        } else {
                            // 刮下來的回音可能被折行截斷；hook 的原文較可信，補完下半截。
                            upgrade_clipped_user_message(&mut tx, &t.id, u).await?;
                        }
                    }
                }
                if !body_text.is_empty() {
                    added.push(lifecycle::insert_message_tx(&mut tx, &conv, Some(&t.id), "assistant", &body_text, "hook", false, None).await?);
                }
                tx.commit().await?;
                for m in added {
                    lifecycle::emit_message_added(app, &bot.id, m).await;
                }
                lifecycle::emit_turn(app, &t.id).await;
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
                    let seen = hook_user_text(user.as_deref(), transcript_path.as_deref()).await;
                    let prompt = turn_prompt(app, &t).await?;
                    if answers_another_prompt(prompt.as_deref(), seen.as_deref()) {
                        tracing::info!(turn = %t.id, bot = %bot.id, "遲到 hook 的使用者訊息不是這一筆備援回合的 prompt：不補，記成外部回合");
                        user = user.or(seen);
                    } else {
                        fill_or_drop_late_hook(app, &t, &body_text, &session_id, &turn_id).await?;
                        return Ok(());
                    }
                }
            }

            // 5. external turn：回合（帶去重用的 native id）與它的訊息同一個交易（#115）。
            let tid = db::ulid();
            let mut tx = app.db.begin().await?;
            sqlx::query(
                "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, native_session_id, native_turn_id, created_at, completed_at)
                 VALUES (?,?,?,'external','completed','ok',?,?,?,?)",
            )
            .bind(&tid)
            .bind(&conv)
            .bind(run.as_ref().map(|r| r.id.clone()))
            .bind(&session_id)
            .bind(&turn_id)
            .bind(db::now())
            .bind(db::now())
            .execute(&mut *tx)
            .await?;
            let mut added = Vec::new();
            if let Some(u) = user.filter(|s| !s.is_empty()) {
                let from = relay_source(run.as_ref(), &u);
                added.push(
                    lifecycle::insert_message_relayed_tx(&mut tx, &conv, Some(&tid), "user", &u, "hook", false, None, from.as_deref()).await?,
                );
            }
            if !body_text.is_empty() {
                added.push(lifecycle::insert_message_tx(&mut tx, &conv, Some(&tid), "assistant", &body_text, "hook", false, None).await?);
            }
            tx.commit().await?;
            for m in added {
                lifecycle::emit_message_added(app, &bot.id, m).await;
            }
            lifecycle::emit_turn(app, &tid).await;
            Ok(())
        }
    }
}

// Remote drain: see SPEC §11.4.3–§11.4.5.

const STATUS_MARKER: &str = "---AM-STATUS---";

const DRAIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(1);

/// The event can beat the spool write; retry once, still ahead of the 5s terminal fallback.
const DRAIN_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Catches status events that never arrived (§11.4.4).
const SCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

fn parse_drain_output(text: &str) -> (Vec<&str>, Option<String>) {
    let mut lines = Vec::new();
    let mut status: Option<String> = None;
    let mut it = text.lines();
    for line in it.by_ref() {
        if line.trim() == STATUS_MARKER {
            status = Some(it.collect::<Vec<_>>().join("\n"));
            break;
        }
        let l = line.trim();
        if !l.is_empty() {
            lines.push(l);
        }
    }
    (lines, status.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

/// SPEC §11.4.3.
pub async fn drain_remote(app: &Arc<App>, host: &str, bot_id: &str) -> Result<usize> {
    if !valid_id(bot_id) {
        anyhow::bail!("invalid bot id `{bot_id}` (must match {ID_RE})");
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let Some(conn) = app.hosts.get(host).await else { return Ok(0) };
    if !conn.is_connected() {
        return Ok(0);
    }
    let root = crate::startup::remote_root_for(app.instance().as_deref());
    // 第一趟：claim（`mv` 成 `.replaying` 再 `cat`），**不刪**。遠端那份是唯一的副本。
    let text = conn.ssh_exec(&claim_script(bot_id, &root)?).await?;
    let (lines, status) = parse_drain_output(&text);
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
                crate::hook_inbox::accept(&app.db, &b, crate::hook_inbox::Source::Remote).await?;
                n += 1;
            }
            // A4: 解不開的行再 claim 幾次也一樣，留著只會擋住 ack；記一筆丟掉。
            Err(e) => tracing::warn!(error = %e, line, "unparseable remote spool line; dropped"),
        }
    }
    // 第二趟：本機已經 commit 了，才准刪遠端那份。ack 失敗＝`.replaying` 還在，下一輪重來。
    conn.ssh_exec(&ack_script(bot_id, &root)?).await?;
    if n > 0 {
        app.hook_inbox_wake.notify_one();
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
/// `hook-status.json` 是例外，照舊讀完就刪：它是單槽、最新的贏的訊號（不是佇列），
/// 掉一格只是晚一次重繪——理由與本機 StatusLine 不進收件匣是同一個（[`crate::hook_inbox`]）。
fn claim_script(bot_id: &str, root: &str) -> Result<String> {
    let dir = bots_dir(bot_id, root)?;
    Ok(format!(
        "d={dir}\n\
         f=\"$d/hook-spool.jsonl\"\n\
         if [ -f \"$f.replaying\" ]; then cat \"$f\" >> \"$f.replaying\" 2>/dev/null; rm -f \"$f\"; \
         elif [ -f \"$f\" ]; then mv \"$f\" \"$f.replaying\"; fi\n\
         if [ -f \"$f.replaying\" ]; then cat \"$f.replaying\"; fi\n\
         s=\"$d/hook-status.json\"\n\
         if [ -f \"$s\" ]; then printf '\\n{marker}\\n'; cat \"$s\"; rm -f \"$s\"; fi\n",
        marker = STATUS_MARKER
    ))
}

/// 第二趟：本機已經把那些行 commit 進 `hook_events` 了，這時候才准刪遠端那份。
fn ack_script(bot_id: &str, root: &str) -> Result<String> {
    let dir = bots_dir(bot_id, root)?;
    Ok(format!("d={dir}\nrm -f \"$d/hook-spool.jsonl.replaying\"\n"))
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
struct DrainGate {
    last: Option<std::time::Instant>,
    again: bool,
}

fn drain_gates() -> &'static std::sync::Mutex<std::collections::HashMap<String, DrainGate>> {
    static G: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, DrainGate>>> =
        std::sync::OnceLock::new();
    G.get_or_init(Default::default)
}

/// `true` when this trigger owns the next ssh; `false` when it was merged into the drain that
/// is still inside the window (which then runs once more on its way out).
fn gate_admit(g: &mut DrainGate, now: std::time::Instant) -> bool {
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

fn gate_take_again(g: &mut DrainGate) -> bool {
    std::mem::take(&mut g.again)
}

/// SPEC §11.4.4.
pub async fn drain_remote_coalesced(app: &Arc<App>, host: &str, bot_id: &str) -> Result<usize> {
    {
        let mut g = drain_gates().lock().unwrap();
        let e = g.entry(bot_id.to_string()).or_default();
        if !gate_admit(e, std::time::Instant::now()) {
            tracing::debug!(bot_id, host, "drain merged into the one in the window");
            return Ok(0);
        }
    }
    let n = drain_remote(app, host, bot_id).await?;
    let again = {
        let mut g = drain_gates().lock().unwrap();
        gate_take_again(g.entry(bot_id.to_string()).or_default())
    };
    // An empty drain may mean the hook is still writing its line (§11.4.4).
    let delay = if again {
        Some(DRAIN_WINDOW)
    } else if n == 0 {
        Some(DRAIN_RETRY)
    } else {
        None
    };
    if let Some(d) = delay {
        let (app2, host2, bot2) = (app.clone(), host.to_string(), bot_id.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(d).await;
            if let Err(e) = drain_remote(&app2, &host2, &bot2).await {
                tracing::debug!(bot_id = %bot2, host = %host2, error = ?e, "follow-up drain failed");
            }
        });
    }
    Ok(n)
}

/// SPEC §11.4.4.
pub fn spawn_spool_scanner(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SCAN_EVERY).await;
            for conn in app.hosts.list().await {
                if conn.is_local() || !conn.is_connected() {
                    continue;
                }
                let root = crate::startup::remote_root_for(app.instance().as_deref());
                let pending = match conn.ssh_exec(&scan_script(&root)).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(host = %conn.name, error = ?e, "spool scan failed");
                        continue;
                    }
                };
                let ids: std::collections::HashSet<&str> =
                    pending.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
                if ids.is_empty() {
                    continue;
                }
                for b in db::live_bots_on_host(&app.db, &conn.name).await.unwrap_or_default() {
                    if !ids.contains(b.id.as_str()) {
                        continue;
                    }
                    if let Err(e) = drain_remote_coalesced(&app, &conn.name, &b.id).await {
                        tracing::debug!(bot = %b.name, host = %conn.name, error = ?e, "scanned drain failed");
                    }
                }
            }
        }
    });
}

/// 掃遠端還有誰欠著 spool。根目錄跟著實例走（`App::instance`）。
fn scan_script(root: &str) -> String {
    format!(
        "for d in \"$HOME/{root}/bots\"/*/; do \
     [ -d \"$d\" ] || continue; b=$(basename \"$d\"); \
     if [ -f \"$d/hook-spool.jsonl\" ] || [ -f \"$d/hook-spool.jsonl.replaying\" ] || [ -f \"$d/hook-status.json\" ]; \
     then echo \"$b\"; fi; done\n",
    )
}

/// SPEC §4.4.6.
pub async fn replay_spool(app: &Arc<App>, bot_id: &str) -> Result<usize> {
    // 讀不到 host 不等於本機（#243）：退回 local 會去讀本機 spool、遠端那份留著沒人排。回錯讓呼叫端重試。
    let host = db::bot_host(&app.db, bot_id).await?;
    if host != crate::config::LOCAL_HOST {
        return drain_remote(app, &host, bot_id).await;
    }
    let lock = app.bot_lock(bot_id).await;
    let _g = lock.lock().await;
    let dir = app.bot_dir(bot_id)?;
    let spool = dir.join("hook-spool.jsonl");
    let staging = dir.join("hook-spool.jsonl.replaying");
    // 上一輪崩在「收了一半」留下的 `.replaying` 就是唯一的副本：沒有新的 spool 也要處理它（#302）。
    // 以前只看 spool 在不在，兩邊都沒新事件時它就一直躺在那裡，直到下一則 hook 失敗寫進 spool 才被併回來。
    if !spool.exists() && !staging.exists() {
        return Ok(0);
    }
    if !spool.exists() {
        // 只剩 `.replaying`：直接讀它。
    } else if staging.exists() {
        // 位元組層合併，不經 UTF-8：崩在半個多位元組字元上的 `.replaying` 以前 `read_to_string` 失敗、
        // `unwrap_or_default` 成空字串，接著整份被新的 spool 覆蓋——舊事件就此消失。
        let mut prev = std::fs::read(&staging)?;
        // 崩在一行寫到一半時尾巴沒有換行：直接接上去，會跟新 spool 的第一行黏成一行、兩則一起解不開。
        if prev.last().is_some_and(|b| *b != b'\n') {
            prev.push(b'\n');
        }
        prev.extend(std::fs::read(&spool)?);
        std::fs::write(&staging, prev)?;
        std::fs::remove_file(&spool)?;
    } else {
        std::fs::rename(&spool, &staging)?;
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
                crate::hook_inbox::accept(&app.db, &b, crate::hook_inbox::Source::Spool).await?;
                n += 1;
            }
            Err(e) => tracing::warn!(error = %e, line, "unparseable spool line"),
        }
    }
    // 只有在上面每一行都 commit 進 hook_events 之後，才刪掉這份唯一的副本。
    std::fs::remove_file(&staging).ok();
    if n > 0 {
        app.hook_inbox_wake.notify_one();
        tracing::info!(bot_id, accepted = n, "hook spool accepted into the inbox");
    }
    Ok(n)
}

#[allow(dead_code)]
pub async fn replay_all(app: &Arc<App>) {
    for host in app.hosts.names().await {
        replay_host(app, &host).await;
    }
}

/// 重放一台 host 的 spool。列舉 bot 讀不到、或某顆 bot 的重放失敗，都不能當成「沒有 spool」：
/// 記下來、背景重試到補齊為止（#243），沒有新的 reconnect／status 事件也會補。
pub async fn replay_host(app: &Arc<App>, host: &str) {
    if replay_host_pass(app, host).await {
        return;
    }
    let (app, host) = (app.clone(), host.to_string());
    tokio::spawn(async move {
        for _ in 0..REPLAY_RETRIES {
            tokio::time::sleep(REPLAY_RETRY_EVERY).await;
            if replay_host_pass(&app, &host).await {
                return;
            }
        }
        tracing::error!(host, "spool replay still failing after retries; spools stay on the host until the next reconnect");
    });
}

#[cfg(not(test))]
const REPLAY_RETRIES: u32 = 40;
// 測試的間隔是 50ms，40 次只撐 2 秒：runner 負載高時，測試執行緒從 `replay_host` 到把表改回
// 可讀之間就可能超過，背景重試先放棄，spool 永遠收不進來（#255 的另一個偶發來源）。
#[cfg(test)]
const REPLAY_RETRIES: u32 = 400;
#[cfg(not(test))]
const REPLAY_RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(test)]
const REPLAY_RETRY_EVERY: std::time::Duration = std::time::Duration::from_millis(50);

/// 一輪；全部成功才回 true。
async fn replay_host_pass(app: &Arc<App>, host: &str) -> bool {
    let bots = match db::live_bots_on_host(&app.db, host).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(host, error = ?e, "spool replay: could not list the host's bots; will retry");
            return false;
        }
    };
    let mut ok = true;
    for b in bots {
        if let Err(e) = replay_spool(app, &b.id).await {
            tracing::warn!(bot = %b.name, host, error = ?e, "spool replay failed; will retry");
            ok = false;
        }
    }
    ok
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn spool_lines_and_the_status_slot_come_apart() {
        let out = "{\"bot_id\":\"b\"}\n{\"bot_id\":\"b\",\"provider\":\"claude\"}\n\n---AM-STATUS---\n{\n  \"session_id\": \"s\"\n}\n";
        let (lines, status) = parse_drain_output(out);
        assert_eq!(lines.len(), 2);
        assert_eq!(status.as_deref(), Some("{\n  \"session_id\": \"s\"\n}"));
    }

    #[test]
    fn output_without_the_marker_is_all_spool() {
        let (lines, status) = parse_drain_output("{\"bot_id\":\"b\"}\n");
        assert_eq!(lines, vec!["{\"bot_id\":\"b\"}"]);
        assert!(status.is_none());
        let (lines, status) = parse_drain_output("");
        assert!(lines.is_empty() && status.is_none());
    }

    #[test]
    fn an_empty_status_slot_is_no_status() {
        let (_, status) = parse_drain_output("---AM-STATUS---\n\n");
        assert!(status.is_none());
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
        assert!(s.contains("mv \"$f\" \"$f.replaying\""));
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

#[cfg(test)]
mod external_claim_tests {
    use super::*;
    use crate::testing as tt;

    #[test]
    fn hook_user_dedups_against_the_scraped_echo() {
        let echo = vec!["Reply with exactly MERGED-OK".to_string()];
        assert!(!hook_user_is_new(&echo, "Reply with exactly MERGED-OK"));
        // The pane wrapped the echo across two columns; the hook sends one line.
        assert!(!hook_user_is_new(&vec!["Reply with\n  exactly MERGED-OK".into()], "Reply with exactly MERGED-OK"));
        // The pane clipped the echo at the column width.
        assert!(!hook_user_is_new(&vec!["Reply with exactly MER".into()], "Reply with exactly MERGED-OK"));
    }

    /// #218：claude Stop 沒帶使用者訊息，從 transcript 尾巴補。CLI 把貼上的 prompt 包成 `<pasted_content id=…>`
    /// （真 transcript，2026-09-19 實測）：要拆回原文——不然記成外部回合時標籤會顯示給使用者，`agent_relay::claim` 也對不上。
    #[test]
    fn the_transcript_user_text_drops_the_cli_pasted_content_wrapper() {
        let dir = std::env::temp_dir().join(format!("am-hookrecv-{}", db::ulid()));
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
        let dir = std::env::temp_dir().join(format!("am-hookrecv-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::open(&dir.join("t.db")).await.unwrap();
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

        fill_or_drop_late_hook(&app, &turn, "側欄那組徽章已收齊，cdcf165 已推", &Some("s1".into()), &Some("n1".into()))
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
        fill_or_drop_late_hook(&app, &turn, "第二份回覆", &Some("s1".into()), &Some("n1".into())).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1, "不會變成兩則");
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
        fill_or_drop_late_hook(&app, &turn, "   ", &Some("s2".into()), &Some("n2".into())).await.unwrap();
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

        upgrade_clipped_user_message(&mut app.db.acquire().await.unwrap(), &turn_id, full).await.unwrap();

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
        upgrade_clipped_user_message(&mut app.db.acquire().await.unwrap(), &turn_id, "完全不同的一句").await.unwrap();
        let after: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='user'")
            .bind(&turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(after, full);
    }

    async fn unknown_turn(app: &Arc<App>, project_id: &str, kind: &str, prompt: &str) -> (String, String, String) {
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(project_id)
        .bind(format!("hook-{}", &bot_id[..6]))
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
        let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "codex", "跑一次測試").await;

        process(&app, &codex_done(&bot_id, "順便看一下 lint")).await.unwrap();

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
        assert_eq!(external[0].1, "順便看一下 lint");
    }

    /// 同一句（去空白後互相包含）就照舊認領並把 unknown 升成 ok。
    #[tokio::test]
    async fn a_hook_answering_the_same_prompt_still_resolves_the_unknown_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "codex", "跑一次   測試").await;

        process(&app, &codex_done(&bot_id, "跑一次 測試")).await.unwrap();

        let turn = sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("completed", "ok"));
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
        assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "過載不是額度用完");

        let (bot_id, _conv, turn_id) = delivered_turn(&app, &env.project_id).await;
        let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
        process(&app, &stop_failure(&bot_id, json!({"hook_event_name": "StopFailure", "session_id": "s2", "prompt_id": "p2",
                                                    "reason": "You've hit your session limit · resets 5pm"})))
            .await
            .unwrap();
        assert_eq!(turn_row(&app, &turn_id).await.status, "failed");
        let hit = crate::quota::limit_hit_for_bot(&app, &bot).await.expect("撞限記下來了");
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
        let owed_at = crate::quota::limit_hit_for_bot(&app, &bot).await.expect("派送前照欠著的那一筆擋").at;
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
        assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "沒有回合可掛，撞限照樣記下");
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
        assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "回聲裡的撞額度也是真的");

        // (c) 沒有 run：說不準是哪個身分送的（停機後可能換過身分），不記。
        let (bot_c, _c, turn_c) = delivered_turn(&app, &env.project_id).await;
        let run_c = turn_row(&app, &turn_c).await.run_id.unwrap();
        sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&run_c).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET identity='cc9' WHERE id=?").bind(&bot_c).execute(&app.db).await.unwrap();
        process(&app, &stop_failure(&bot_c, json!({"hook_event_name": "StopFailure", "session_id": "sc", "prompt_id": "p-c", "reason": LIMIT})))
            .await
            .unwrap();
        let bot = db::bot(&app.db, &bot_c).await.unwrap().unwrap();
        assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "沒有 run 的不記到現在的身分上");

        // (d) 已經收過的同一則重播：不再記一次（不把撞限時刻往後推）。
        let (bot_d, _c, turn_d) = delivered_turn(&app, &env.project_id).await;
        let ev = stop_failure(&bot_d, json!({"hook_event_name": "StopFailure", "session_id": "sd", "prompt_id": "p-d", "reason": LIMIT}));
        process(&app, &ev).await.unwrap();
        assert_eq!(turn_row(&app, &turn_d).await.status, "failed");
        let bot = db::bot(&app.db, &bot_d).await.unwrap().unwrap();
        let first = crate::quota::limit_hit_for_bot(&app, &bot).await.expect("第一次記下");
        crate::quota::clear_limit_hit_for_bot(&app, &bot).await;
        process(&app, &ev).await.unwrap();
        assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_none(), "重播不再記：{first:?}");
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
            HookKind::SpawnHint { pane_id } => assert_eq!(pane_id, "w1:p2"),
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

    /// claude 的 Stop 沒帶使用者訊息：從 transcript 尾巴讀。對不上一樣不認領；讀不到（沒有 transcript）就照舊認領。
    #[tokio::test]
    async fn claude_reads_the_prompt_from_the_transcript_before_claiming_an_unknown_turn() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", "跑一次測試").await;
        let transcript = app.data_dir.join(format!("t-{}.jsonl", db::ulid()));
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

    /// #217：`unknown` 那一則在 transcript 裡被 CLI 包成 `<pasted_content>`（真 transcript，2026-09-19 實測）還是同一則，照樣認領。
    /// prompt 本身寫著字面的 `<pasted_content …>` 時 CLI 還把它跳脫成 `<\…`，兩邊去空白後互不包含——以前被當成「回答的是別句」：
    /// 答案記到一筆外部回合、那一則一直掛在 unknown。
    #[tokio::test]
    async fn an_unknown_turn_whose_prompt_the_cli_wrapped_as_pasted_content_is_still_claimed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let prompt = "這段文字裡有字面的 <pasted_content id=\"1234\">x</pasted_content id=\"1234\"> 標籤，請只回覆 OK 兩個字母。";
        let (bot_id, conv, turn_id) = unknown_turn(&app, &env.project_id, "claude", prompt).await;
        let transcript = app.data_dir.join(format!("t-{}.jsonl", db::ulid()));
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
    /// fallback winning the CAS.
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
        crate::lifecycle::insert_message(&app, &conv, Some(&turn_id), "assistant", "from the pane", "terminal_fallback", true, None)
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
        assert_eq!(turn.status, "completed_fallback", "the fallback's claim stands");
        assert_eq!(turn.native_turn_id.as_deref(), Some("native-turn"), "the ids land on that turn, so a retry dedups");
        assert_eq!(turn.native_session_id.as_deref(), Some("native-session"));
        let replies: Vec<String> = sqlx::query_scalar("SELECT source FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(replies, ["terminal_fallback"], "one answer, not two");
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
#[cfg(test)]
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
        crate::hook_inbox::drain_once(&e.app).await.unwrap();
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
        let done = crate::hook_inbox::drain_once(&restarted).await.unwrap();
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

#[cfg(test)]
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
#[cfg(test)]
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

#[cfg(test)]
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
}

/// #243：讀不到 bot 的 host 不能當成本機——spool 重放要回錯、事件留著等下一輪，DB 好了要補回來。
#[cfg(test)]
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
