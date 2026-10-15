//! Claude 壓縮對話（issue #1759）：閒置壓縮、context 快滿的自動壓縮、手動 `/compact`。
//!
//! 壓縮不送 Stop，只送 SubagentStop（摘要子代理）與 `SessionStart`（`source: "compact"`）；pane 轉 working 時
//! [`crate::lifecycle::begin_external_turn`] 已經開了一筆沒有 prompt 的 external 回合。以前這筆回合由終端備援收，
//! 把畫面上的「Compacted while idle…」存成一則終端擷取泡泡；備援沒趕上就由 stuck sweep 收、留一則「沒有偵測到結束」（#1758）。
//!
//! 現在 `SessionStart` source=compact 一到：讀 transcript 尾端最後一個 `compact_boundary`（本機自己讀；遠端由 hook.sh
//! 在那台讀好、帶成 `agm_compact`），寫一則系統訊息說明是哪種壓縮與 tokens 前後，並把壓縮開出來的空回合收掉。
//! daemon 自己的熱壓已經有「主力熱壓」說明，不再重寫。

use crate::db;
use crate::events::ports::{MessageTxOps, TurnConnOps};
use crate::hookrecv::HookHost;
use anyhow::Result;
use serde_json::Value;

/// transcript 只讀尾端這麼多：壓縮剛做完，boundary 後面只有摘要那一則與幾行提示。
const TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// 熱壓（`primary_cache_actions`）在這段時間內：這次壓縮是 daemon 送的 `/compact`，「主力熱壓」說明已經有了。
const WARM_COMPACT_WINDOW_SECS: i64 = 15 * 60;
/// 收件匣重播或兩個來源各報一次：這段時間內已有壓縮說明就不再寫。
const DEDUPE_SECS: i64 = 120;

const IDLE_NOTE: &str = "Claude 閒置壓縮";
const AUTO_NOTE: &str = "Claude 自動壓縮（context 快滿）";
const MANUAL_NOTE: &str = "手動 /compact";
const UNKNOWN_NOTE: &str = "Claude 壓縮了對話";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactInfo {
    /// `compactMetadata.trigger`：`auto`／`manual`。
    pub trigger: Option<String>,
    /// boundary 後面跟著 `Compacted while idle…`：閒置時壓的。
    pub idle: bool,
    pub pre_tokens: Option<i64>,
    pub post_tokens: Option<i64>,
}

/// 遠端 hook.sh 帶來的 `agm_compact`（欄位同 [`CompactInfo`]，snake_case）。
pub fn carried(payload: &Value) -> Option<CompactInfo> {
    let c = payload.get("agm_compact")?.as_object()?;
    Some(CompactInfo {
        trigger: c.get("trigger").and_then(Value::as_str).map(str::to_string),
        idle: c.get("idle").and_then(Value::as_bool).unwrap_or(false),
        pre_tokens: c.get("pre_tokens").and_then(Value::as_i64),
        post_tokens: c.get("post_tokens").and_then(Value::as_i64),
    })
}

/// transcript 尾端最後一個 `compact_boundary`；它後面有 `Compacted while idle` 的 informational 就是閒置壓縮。
pub fn from_transcript_tail(text: &str) -> Option<CompactInfo> {
    let mut found: Option<CompactInfo> = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("system") {
            continue;
        }
        match v.get("subtype").and_then(Value::as_str) {
            Some("compact_boundary") => {
                let m = v.get("compactMetadata");
                let num = |k: &str| m.and_then(|m| m.get(k)).and_then(Value::as_i64);
                found = Some(CompactInfo {
                    trigger: m.and_then(|m| m.get("trigger")).and_then(Value::as_str).map(str::to_string),
                    idle: false,
                    pre_tokens: num("preTokens"),
                    post_tokens: num("postTokens"),
                });
            }
            _ => {
                if let Some(info) = found.as_mut() {
                    if v.get("content").and_then(Value::as_str).is_some_and(|c| c.starts_with("Compacted while idle")) {
                        info.idle = true;
                    }
                }
            }
        }
    }
    found
}

fn tokens(n: i64) -> String {
    if n >= 1000 { format!("{}K", (n + 500) / 1000) } else { n.to_string() }
}

pub fn note_text(info: Option<&CompactInfo>) -> String {
    let Some(info) = info else { return format!("{UNKNOWN_NOTE}。") };
    let head = match (info.idle, info.trigger.as_deref()) {
        (true, _) => IDLE_NOTE,
        (false, Some("manual")) => MANUAL_NOTE,
        (false, Some("auto")) => AUTO_NOTE,
        _ => UNKNOWN_NOTE,
    };
    match (info.pre_tokens, info.post_tokens) {
        (Some(pre), Some(post)) => format!("{head}：{} tokens → 摘要 {} tokens", tokens(pre), tokens(post)),
        (Some(pre), None) => format!("{head}：{} tokens → 摘要", tokens(pre)),
        _ => format!("{head}。"),
    }
}

fn is_note(content: &str) -> bool {
    [IDLE_NOTE, AUTO_NOTE, MANUAL_NOTE, UNKNOWN_NOTE].iter().any(|p| content.starts_with(p))
}

/// 終端備援讀到的畫面只剩壓縮提示（`Compacted while idle…`、`Conversation compacted · ctrl+o for history`）：
/// 不是 agent 的回覆，不存成終端擷取泡泡。
pub fn is_screen_notice(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    // `/compact` 做完 claude 會列出摘要裡帶回來的檔案（`Referenced file …`、`Read … (N lines)`，#1762）。
    let restored = |l: &str| {
        l.starts_with("Referenced file ") || (l.starts_with("Read ") && l.ends_with(" lines)")) || l.starts_with("Loaded ")
    };
    text.chars().count() <= 2000
        && lines.iter().any(|l| l.to_lowercase().contains("compacted"))
        && lines.iter().all(|l| l.to_lowercase().contains("compacted") || restored(l))
}

fn is_compact_command(prompt: &str) -> bool {
    prompt == "/compact" || prompt.starts_with("/compact ")
}

/// `SessionStart` source=compact：寫說明、收掉壓縮開出來的空回合。呼叫端握著 bot 鎖。
pub async fn record<H: HookHost>(app: &H, bot: &db::Bot, run: &db::Run, payload: &Value, transcript: Option<&str>) -> Result<()> {
    let mut info = carried(payload);
    if info.is_none() {
        if let Some(path) = transcript {
            if app.local_transcript_allowed(bot, path).await {
                info = am_base::transcript_read::read_tail(std::path::Path::new(path), TAIL_BYTES).as_deref().and_then(from_transcript_tail);
            }
        }
    }
    let conv = db::conversation_id(app.db(), &bot.id).await?;
    let now = chrono::Utc::now();
    let warm = info.as_ref().is_none_or(|i| i.trigger.as_deref() == Some("manual") && !i.idle)
        && am_base::primary_keep_warm::last_warm_compact_at(app.db(), &bot.id)
            .await?
            .is_some_and(|at| (now - at).num_seconds() <= WARM_COMPACT_WINDOW_SECS);
    let since = db::iso_in(-DEDUPE_SECS);
    let recent: Vec<String> =
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'system' AND created_at >= ?")
            .bind(&conv)
            .bind(&since)
            .fetch_all(app.db())
            .await?;
    let write_note = !warm && !recent.iter().any(|c| is_note(c));

    // 這次壓縮自己的回合：(a) pane 轉 working 開出來的 external 空回合（沒有 prompt、沒有任何訊息）；
    // (b) 使用者打的 `/compact`（終端直接打的 UserPromptSubmit 或網頁送的，#1762）——只有 `/compact` 這則 user 訊息、
    // 還沒有回覆。兩種都不會有 Stop，這裡收掉；終端備援就不會再把畫面上的壓縮提示存成回覆。
    // 其他有 prompt 的回合（自動壓縮發生在使用者回合中間）照常等 Stop。
    let own_turn = match db::in_flight_turn(app.db(), &run.id).await? {
        Some(t) => {
            let (users, others): (i64, i64) = sqlx::query_as(
                "SELECT COUNT(*) FILTER (WHERE role = 'user'), COUNT(*) FILTER (WHERE role <> 'user') FROM messages WHERE turn_id = ?",
            )
            .bind(&t.id)
            .fetch_one(app.db())
            .await?;
            let compact_users: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM messages WHERE turn_id = ? AND role = 'user' AND (trim(content) = '/compact' OR trim(content) LIKE '/compact %')",
            )
            .bind(&t.id)
            .fetch_one(app.db())
            .await?;
            let prompt = t.prompt_text.as_deref().map(str::trim).unwrap_or("");
            let empty = t.origin == "external" && prompt.is_empty() && users == 0 && others == 0;
            let command = others == 0
                && (is_compact_command(prompt) || (prompt.is_empty() && users > 0 && users == compact_users));
            (empty || command).then_some((t, command))
        }
        None => None,
    };
    let manual_command = own_turn.as_ref().is_some_and(|(_, command)| *command);
    // 遠端沒帶 `agm_compact`（hook.sh 還是舊版）時，至少知道是手動 `/compact`。
    if manual_command && info.is_none() {
        info = Some(CompactInfo { trigger: Some("manual".into()), ..Default::default() });
    }
    let empty_turn = own_turn.map(|(t, _)| t);

    let mut tx = app.db().begin().await?;
    let mut closed = None;
    if let Some(t) = &empty_turn {
        if tx.set_status_on(&t.id, "in_flight", "completed", "Claude 壓縮對話開出的空回合").await?
            == crate::lifecycle::turn_controller::Outcome::Applied
        {
            closed = Some(t.id.clone());
        }
    }
    let message = if write_note {
        Some(tx.insert_message_tx(&conv, closed.as_deref(), "system", &note_text(info.as_ref()), "system", false, None).await?)
    } else {
        None
    };
    tx.commit().await?;
    tracing::info!(bot = %bot.name, ?info, warm, closed = ?closed, noted = message.is_some(), "claude compacted its conversation");
    if let Some(m) = message {
        app.emit_message_added(&bot.id, m).await;
    }
    if let Some(turn) = closed {
        app.emit_turn(&turn).await;
        app.schedule_flush_queued(&bot.id);
    }
    Ok(())
}

#[cfg(all(test, not(feature = "daemon-test-harness")))]
mod tests {
    use super::*;

    const TAIL: &str = r#"{"type":"assistant","message":{"content":[]}}
{"type":"system","subtype":"compact_boundary","content":"Conversation compacted","compactMetadata":{"trigger":"auto","preTokens":286047,"postTokens":11362,"durationMs":37577}}
{"type":"user","isCompactSummary":true,"message":{"content":"summary"}}
{"type":"system","subtype":"informational","content":"Compacted while idle, before the prompt cache expired"}
"#;

    #[test]
    fn the_last_boundary_and_its_idle_notice_are_read_from_the_transcript() {
        let info = from_transcript_tail(TAIL).unwrap();
        assert_eq!(info, CompactInfo { trigger: Some("auto".into()), idle: true, pre_tokens: Some(286047), post_tokens: Some(11362) });
        assert_eq!(note_text(Some(&info)), "Claude 閒置壓縮：286K tokens → 摘要 11K tokens");
        let not_idle = TAIL.lines().take(3).collect::<Vec<_>>().join("\n");
        assert_eq!(note_text(from_transcript_tail(&not_idle).as_ref()), "Claude 自動壓縮（context 快滿）：286K tokens → 摘要 11K tokens");
        assert!(from_transcript_tail("{\"type\":\"user\"}").is_none());
    }

    #[test]
    fn an_idle_notice_before_the_last_boundary_does_not_count() {
        let text = format!(
            "{}\n{}",
            TAIL,
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual","preTokens":900}}"#
        );
        assert_eq!(note_text(from_transcript_tail(&text).as_ref()), "手動 /compact：900 tokens → 摘要");
    }

    #[test]
    fn the_carried_form_from_a_remote_host_reads_the_same() {
        let p = serde_json::json!({"agm_compact": {"trigger": "auto", "idle": true, "pre_tokens": 286047}});
        assert_eq!(note_text(carried(&p).as_ref()), "Claude 閒置壓縮：286K tokens → 摘要");
        assert_eq!(carried(&serde_json::json!({})), None);
        assert_eq!(note_text(None), "Claude 壓縮了對話。");
    }

    #[test]
    fn only_a_screen_of_compaction_notices_is_not_a_reply() {
        assert!(is_screen_notice("Compacted while idle, before the prompt cache expired"));
        assert!(is_screen_notice("✻ Conversation compacted · ctrl+o for history\n\nCompacted while idle, before the prompt cache expired"));
        assert!(!is_screen_notice("我把對話 compacted 了，接著修 #12\n修好了"));
        assert!(!is_screen_notice(""));
        assert!(is_screen_notice("Compacted\nReferenced file scripts/net-pin.sh\nRead scripts/rollback.sh (145 lines)\nReferenced file scripts/package-ota.sh"));
        assert!(!is_screen_notice("Referenced file a.sh"), "沒有壓縮提示就不是");
        assert!(!is_screen_notice("Compacted\n修好了，接著跑測試"));
    }
}
