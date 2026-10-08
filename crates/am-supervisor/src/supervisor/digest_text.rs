//! 喚醒摘要裡「一則事件寫什麼」：巡檢與協調者的 digest 共用。
//!
//! 以前巡檢的 digest 一律只讀 `payload.result`（交辦回報的欄位），協調者交接過來的 `bot_request`
//! （正文在 `text`）、`responder_watchdog_gave_up`（`why`／`action`）在摘要裡全變成「（沒有留下回覆）」，
//! 跑 fable-low 的巡檢多半直接 ack，要使用者裁示的事就這樣沒人問（review 2026-09-16 c3 M2）。

use serde_json::Value;

use super::store::InboxEvent;

/// hook 晚到補上的回覆（`late_reply:true`）：同一張交辦的**第二則**回報，帶的是真正的回覆。
/// 摘要只印 kind 與 result 的話，AGM 只會看到同一張交辦又完成了一次，看不出這是補上來的
/// （deliv3 2026-09-17 轉來的一條）。
pub fn late_reply_mark(p: &Value) -> String {
    if p.get("late_reply").and_then(Value::as_bool) != Some(true) {
        return String::new();
    }
    match p.get("assignment_status").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => format!("（回覆晚到：先前那則「沒有留下回覆」的補件，交辦現在是 {s}）"),
        None => "（回覆晚到：先前那則「沒有留下回覆」的補件）".to_string(),
    }
}

/// 交辦的**回合結果**那一型：payload 帶 `result`（回合的最後一句），由各自的 digest 照舊呈現。
///
/// 只有這三種。其他 `assignment_*`（送不進去、等額度、排隊中）不是回合結果、沒有 `result`：以前前綴一律算進來，
/// `assignment_undeliverable` 要 AGM 讀的 `reason`／`hint`（改用 followup、不要拿同一個 request id 再 assign）
/// 在摘要裡變成「（沒有留下回覆）」（issue #142）。它們走 [`detail`] 的一般寫法。
pub fn is_assignment_report(e: &InboxEvent) -> bool {
    matches!(e.kind.as_str(), "assignment_completed" | "assignment_failed" | "assignment_noticed")
}

/// 非交辦回報的事件，依種類寫出讀的人需要的欄位。每一行已經帶縮排與換行。
pub fn detail(e: &InboxEvent, p: &Value) -> String {
    let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    match e.kind.as_str() {
        "bot_request" => {
            let from = s("from_name").unwrap_or("");
            let from_id = s("from_bot_id").unwrap_or("");
            let verified = if p.get("sender_verified").and_then(Value::as_bool) == Some(true) { "" } else { "（來源未以 bot token 驗證）" };
            let reply = match (p.get("quiet_reason").and_then(Value::as_str), s("reply_to")) {
                (Some(_), Some(r)) => format!("（回覆 {r}）"),
                (Some("ack"), None) => "（寄件端標 ack）".to_string(),
                _ => String::new(),
            };
            format!(" from={from}（{from_id}）{verified}{reply}\n  內容：{}\n", snippet(s("text").unwrap_or(""), 1500))
        }
        "approval_requested" => format!(
            " approval={} requester={} purpose={} commit={}\n  範圍：{}\n",
            s("id").unwrap_or(""),
            s("requester").unwrap_or(""),
            s("purpose").unwrap_or(""),
            s("target_commit").unwrap_or(""),
            snippet(s("scope").unwrap_or(""), 600),
        ),
        "incident_opened" | "incident_resolved" => {
            let i = p.get("incident").cloned().unwrap_or(Value::Null);
            let f = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let d = i.get("detail").map(Value::to_string).unwrap_or_default();
            format!(" incident={} {}／{} severity={}\n  內容：{}\n", f("id"), f("kind"), f("resource"), f("severity"), snippet(&d, 600))
        }
        "health_changed" => {
            let at = |ptr: &str| p.pointer(ptr).and_then(Value::as_str).unwrap_or("?").to_string();
            format!(
                " 總體={} 巡檢={} 協調者={} 系統={}\n",
                at("/status"),
                at("/manager_health/status"),
                at("/responder_health/status"),
                at("/system_health/status"),
            )
        }
        _ => {
            // 故障類（`watchdog_gave_up`、`responder_*`、`bot_restart_failed`、`ops_alert`…）的欄位名不一，
            // 挑人看得懂的那幾個照順序印；一個都沒有就整份 payload 節錄，不要印成「空」。
            let mut out = String::new();
            for (k, label) in [("text", "內容"), ("why", "原因"), ("reason", "原因"), ("message", "說明"), ("detail", "細節"), ("action", "處理"), ("hint", "下一步")] {
                if let Some(v) = s(k) {
                    out.push_str(&format!("\n  {label}：{}", snippet(v, 600)));
                }
            }
            if out.is_empty() {
                out = format!("\n  內容：{}", snippet(&p.to_string(), 600));
            }
            // 掛在交辦上的事件（送不進去、等額度…）要說得出是哪一件：下一步就是對它下 `review`。
            let assignment = e.assignment_id.as_deref().map(|a| format!(" assignment={a}")).unwrap_or_default();
            let bot = e.bot_id.as_deref().map(|b| format!(" bot={b}")).unwrap_or_default();
            format!("{assignment}{bot}{out}\n")
        }
    }
}

pub fn snippet(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.is_empty() || t == "null" || t == "{}" {
        return "（空）".into();
    }
    let cut: String = t.chars().take(max).collect();
    if cut.chars().count() < t.chars().count() { format!("{cut}…") } else { cut }
}
