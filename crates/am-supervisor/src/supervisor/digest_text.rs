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
        "approval_requested" => {
            // 申請理由（沒附時印「（空）」）與未驗證身分一起印：裁示的人才看得到理由，也看得出是不是冒名。
            let unverified = if p.get("requester_unverified").and_then(Value::as_bool) == Some(true) { "（申請者未以 bot token 驗證）" } else { "" };
            format!(
                " approval={} requester={}{unverified} purpose={} commit={}\n  範圍：{}\n  理由：{}\n",
                s("id").unwrap_or(""),
                s("requester").unwrap_or(""),
                s("purpose").unwrap_or(""),
                s("target_commit").unwrap_or(""),
                snippet(s("scope").unwrap_or(""), 600),
                snippet(s("request_reason").unwrap_or(""), 600),
            )
        }
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
            for (k, label) in [
                ("text", "內容"),
                ("question", "問題"),
                ("answer", "回答"),
                ("why", "原因"),
                ("reason", "原因"),
                ("message", "說明"),
                ("detail", "細節"),
                ("action", "處理"),
                ("hint", "下一步"),
                ("note", "備註"),
            ] {
                if let Some(v) = s(k) {
                    out.push_str(&format!("\n  {label}：{}", snippet(v, 600)));
                }
            }
            // mission 的「下一步」物件（放行後的下一步，`mission_answered`／`mission_resumed`）：沒有字串的 `action` 才補，
            // `mission_next` 自己已經有 `action`，不重複印。
            if s("action").is_none() {
                if let Some(n) = p.get("next").filter(|n| n.is_object()) {
                    let action = n.get("action").and_then(Value::as_str).unwrap_or("?");
                    let role = n.get("role").and_then(Value::as_str).map(|r| format!("（{r}）")).unwrap_or_default();
                    let hint = n.get("hint").and_then(Value::as_str).unwrap_or("");
                    out.push_str(&format!("\n  下一步：{action}{role}：{}", snippet(hint, 300)));
                }
            }
            if out.is_empty() {
                out = format!("\n  內容：{}", snippet(&p.to_string(), 600));
            }
            // 掛在交辦上的事件（送不進去、等額度…）要說得出是哪一件：下一步就是對它下 `review`。
            let assignment = e.assignment_id.as_deref().map(|a| format!(" assignment={a}")).unwrap_or_default();
            let bot = e.bot_id.as_deref().map(|b| format!(" bot={b}")).unwrap_or_default();
            // mission 事件要說得出是哪一筆任務：AGM 接著每一步（`mission pick／assign --mission <id>`）都要這個 id。
            let mission = s("mission_id").map(|m| format!(" mission={m}")).unwrap_or_default();
            format!("{assignment}{bot}{mission}{out}\n")
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn approval(payload: Value) -> InboxEvent {
        event("approval_requested", payload)
    }

    fn event(kind: &str, payload: Value) -> InboxEvent {
        InboxEvent {
            id: "e-ev1".into(),
            event_key: "k-ev1".into(),
            assignment_id: None,
            bot_id: None,
            turn_id: None,
            kind: kind.into(),
            payload_json: payload.to_string(),
            state: "pending".into(),
            notify_turn_id: None,
            notify_delivery: None,
            notify_attempts: 0,
            notify_next_at: None,
            notify_error: None,
            delivered_at: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            role: None,
            wake: None,
            claimed_by: None,
            acked_by: None,
            merged_into: None,
        }
    }

    #[test]
    fn approval_digest_shows_the_request_reason() {
        let p = json!({"id": "ap1", "requester": "bot-a", "purpose": "restart", "scope": "daemon", "request_reason": "修 #900 要重啟", "requester_unverified": false});
        let e = approval(p.clone());
        let out = detail(&e, &p);
        assert!(out.contains("理由：修 #900 要重啟"), "{out}");
        assert!(!out.contains("未以 bot token 驗證"), "{out}");
    }

    #[test]
    fn approval_digest_marks_missing_reason_and_unverified_requester() {
        let p = json!({"id": "ap2", "requester": "bot-b", "purpose": "restart", "scope": "daemon", "requester_unverified": true});
        let e = approval(p.clone());
        let out = detail(&e, &p);
        assert!(out.contains("理由：（空）"), "{out}");
        assert!(out.contains("（申請者未以 bot token 驗證）"), "{out}");
    }

    /// #1116：mission_paused 的摘要要說得出是哪一筆任務，也要帶 `note`（「不要再派新交辦」）。
    #[test]
    fn mission_paused_digest_names_the_mission() {
        let p = json!({"mission_id": "01MISSION", "project_id": "p", "reason": "clarify", "detail": "等使用者確認", "note": "任務被暫停：不要再派新交辦"});
        let out = detail(&event("mission_paused", p.clone()), &p);
        assert!(out.contains(" mission=01MISSION"), "{out}");
        assert!(out.contains("原因：clarify"), "{out}");
        assert!(out.contains("備註：任務被暫停"), "{out}");
    }

    /// #1116：任務本文很長時，問題仍要印得出來（以前整份 JSON 節錄會被鍵序切掉）。
    #[test]
    fn mission_question_digest_shows_the_question_past_a_long_mission_text() {
        let p = json!({"mission_id": "m1", "mission_text": "字".repeat(2000), "question": "標題要不要改？", "asked_by": "executor", "expects": "answer", "status": "open"});
        let out = detail(&event("mission_question", p.clone()), &p);
        assert!(out.contains("問題：標題要不要改？"), "{out}");
        assert!(out.contains(" mission=m1"), "{out}");
    }

    /// #1116：放行後的下一步（`next` 物件）要印出來。
    #[test]
    fn mission_answered_digest_keeps_the_next_step() {
        let p = json!({"mission_id": "m1", "answer": "好", "next": {"action": "assign", "role": "executor", "hint": "派執行者"}});
        let out = detail(&event("mission_answered", p.clone()), &p);
        assert!(out.contains("回答：好"), "{out}");
        assert!(out.contains("下一步：assign（executor）：派執行者"), "{out}");
    }

    /// #1116：`mission_next` 自己已經有字串的 `action`，`next` 物件不再重複印一次。
    #[test]
    fn mission_next_digest_does_not_repeat_the_next_step() {
        let p = json!({"mission_id": "m1", "action": "next=assign executor", "next": {"action": "assign", "role": "executor", "hint": "派執行者"}});
        let out = detail(&event("mission_next", p.clone()), &p);
        assert!(!out.contains("下一步：assign（"), "{out}");
    }

    /// #1116：沒有 mission 欄位的事件，輸出逐字不變。
    #[test]
    fn events_without_mission_fields_are_unchanged() {
        let p = json!({"text": "x"});
        assert_eq!(detail(&event("ops_alert", p.clone()), &p), "\n  內容：x\n");
    }
}
