//! Codex rollout 回合回覆解析輔助（#1988）。
//!
//! 當 Codex 的 legacy notify 因 payload 過大（>128 KiB E2BIG）送不出 hook 時，
//! 終端備援與 hookless 捕獲可優先自 rollout 提取結構化回覆，避免存成終端整面快照。

use serde_json::Value;

/// 自 rollout 取出嚴格屬於該 turn（turn 開始時間 `since_iso` 之後、且以 `sent` 為 prompt）的完整回覆。
///
/// 嚴格條件：
/// 1. `since_iso` 必須為合法 RFC3339 時間戳，解析失敗一律回傳 None；
/// 2. 該 prompt 必須在 `since_iso` 之後出現（若 prompt 時間早於 since 或沒有時間戳則排除）；
/// 3. 在該 prompt 之後，存在 `event_msg` / `task_complete`，且帶有非空的 `last_agent_message`；
/// 4. 在該 prompt 與 `task_complete` 之間沒有出現別的 user input；
/// 5. 該 `task_complete` 的完成時間必須晚於 `since_iso`；
/// 6. 該 `task_complete` 之後若已有新的 user input，表示該回合已過期。
pub fn codex_reply_for_turn(log: &str, sent: &[String], since_iso: &str) -> Option<String> {
    let since = chrono::DateTime::parse_from_rfc3339(since_iso).ok()?;
    let since_ts = since.timestamp();

    let lines: Vec<&str> = log.lines().collect();

    // 尋找最後一次出現 sent 其中一句的 user prompt 行
    let start = lines.iter().rposition(|l| {
        let is_match = crate::lifecycle::delivery::codex_user_text(l)
            .is_some_and(|t| sent.iter().any(|s| s == &t));
        if !is_match {
            return false;
        }
        let Ok(v) = serde_json::from_str::<Value>(l) else { return false };
        let Some(ts_str) = v.get("timestamp").and_then(Value::as_str) else { return false };
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(ts_str) else { return false };
        at >= since
    })?;

    // 從該 prompt 往後尋找 task_complete，期間不能有新的 user input
    let mut reply = None;
    let mut tc_offset = None;
    for (idx, line) in lines[start + 1..].iter().enumerate() {
        if crate::lifecycle::delivery::codex_user_text(line).is_some() {
            // 本 turn 之後又有別的 user input，代表已經不是同一個回合
            return None;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let p = v.get("payload");
        if v.get("type").and_then(Value::as_str) != Some("event_msg")
            || p.and_then(|p| p.get("type")).and_then(Value::as_str) != Some("task_complete")
        {
            continue;
        }

        // 檢查 task_complete 的時間戳
        if let Some(ts_str) = v.get("timestamp").and_then(Value::as_str) {
            if let Ok(at) = chrono::DateTime::parse_from_rfc3339(ts_str) {
                if at < since {
                    return None;
                }
            }
        }
        if let Some(completed_at) = p.and_then(|p| p.get("completed_at")).and_then(Value::as_i64) {
            if completed_at < since_ts {
                return None;
            }
        }

        reply = p
            .and_then(|p| p.get("last_agent_message"))
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string);
        tc_offset = Some(idx);
        break;
    }

    if let (Some(r), Some(idx)) = (reply, tc_offset) {
        for line in &lines[start + 1 + idx + 1..] {
            if crate::lifecycle::delivery::codex_user_text(line).is_some() {
                return None;
            }
        }
        return Some(r);
    }

    None
}

/// codex 寫入 rollout 與 daemon 確認送達（標 `delivered_at`）之間的時間差容許量。codex 先寫 prompt、
/// daemon 之後才確認：#1988 上線驗收實測 turn 01M5RYFAAFHSSBGGRGEPF577SB 的 prompt 在 07:12:50.788Z
/// 寫進 rollout、delivered_at 是 07:12:51.920Z（早 1.13 秒）。下限取 `max(created_at, delivered_at − 這個值)`：
/// 仍不早於 turn 建立，撿不到本 turn 建立前的舊 prompt；沒有 delivered_at 就只用 created_at。
pub const DELIVERY_SLACK_SECS: i64 = 5;

pub fn turn_since(created_at: &str, delivered_at: Option<&str>) -> String {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&chrono::Utc));
    let (Some(created), Some(delivered)) = (parse(created_at), delivered_at.and_then(parse)) else {
        return created_at.to_string();
    };
    crate::db::iso_at(created.max(delivered - chrono::Duration::seconds(DELIVERY_SLACK_SECS)))
}

/// 自 rollout 取出在 `since_iso`（ISO 8601 時刻）之後最新一筆 `task_complete` 的回覆（供無 prompt 的 hookless turn 使用）。
///
/// 嚴格邊界：
/// - `task_complete` 的 `timestamp` 或 `completed_at` 必須嚴格晚於 `since_iso`；
/// - 該 `task_complete` 之後若已有新的 `user` message，表示該回合已過期，不予採用；
/// - 找不到或時間比對失敗一律回傳 `None`，退回終端畫面備援。
pub fn codex_latest_reply_since(log: &str, since_iso: &str) -> Option<String> {
    let since = chrono::DateTime::parse_from_rfc3339(since_iso).ok()?;
    let since_ts = since.timestamp();
    let lines: Vec<&str> = log.lines().collect();

    // 尋找最後一筆 task_complete
    let pos = lines.iter().rposition(|l| {
        let Ok(v) = serde_json::from_str::<Value>(l) else { return false };
        let is_tc = v.get("type").and_then(Value::as_str) == Some("event_msg")
            && v.pointer("/payload/type").and_then(Value::as_str) == Some("task_complete");
        if !is_tc {
            return false;
        }
        // 驗證時間戳
        if let Some(ts_str) = v.get("timestamp").and_then(Value::as_str) {
            if let Ok(at) = chrono::DateTime::parse_from_rfc3339(ts_str) {
                return at > since;
            }
        }
        if let Some(completed_at) = v.pointer("/payload/completed_at").and_then(Value::as_i64) {
            return completed_at > since_ts;
        }
        false
    })?;

    // 檢查在該 task_complete 之後是否又有了新的 user message
    for line in &lines[pos + 1..] {
        if crate::lifecycle::delivery::codex_user_text(line).is_some() {
            return None;
        }
    }

    let Ok(v) = serde_json::from_str::<Value>(lines[pos]) else { return None };
    v.pointer("/payload/last_agent_message")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
}

/// 嘗試為 Codex 的 in-flight turn 自本地 rollout 取得精確回覆。
pub async fn codex_rollout_reply_for_in_flight(
    app: &impl super::lc_host::LcHost,
    bot: &crate::db::Bot,
    run: &crate::db::Run,
    sent: &[String],
    since_iso: &str,
) -> Option<String> {
    if !crate::db::project(app.db(), &bot.project_id).await.ok().flatten().is_some_and(|p| p.host == crate::config::LOCAL_HOST) {
        return None;
    }
    let home = super::s6_ports::StuckTurnServices::codex_home(app, bot).await?;
    let session = run.native_session_id.clone().filter(|s| !s.trim().is_empty())?;
    let sent = sent.to_vec();
    let since = since_iso.to_string();
    tokio::task::spawn_blocking(move || {
        let path = super::delivery::codex_session_log(&home, &session)?;
        let log = crate::transcript_read::read_tail(&path, 4 * 1024 * 1024)?;
        codex_reply_for_turn(&log, &sent, &since)
    })
    .await
    .ok()?
}

/// 嘗試為 Codex 的 hookless turn 自本地 rollout 取得最新一筆已完成回合回覆。
pub async fn codex_rollout_reply_for_hookless(
    app: &impl super::lc_host::LcHost,
    bot: &crate::db::Bot,
    run: &crate::db::Run,
    since_iso: &str,
) -> Option<String> {
    if !crate::db::project(app.db(), &bot.project_id).await.ok().flatten().is_some_and(|p| p.host == crate::config::LOCAL_HOST) {
        return None;
    }
    let home = super::s6_ports::StuckTurnServices::codex_home(app, bot).await?;
    let session = run.native_session_id.clone().filter(|s| !s.trim().is_empty())?;
    let since = since_iso.to_string();
    tokio::task::spawn_blocking(move || {
        let path = super::delivery::codex_session_log(&home, &session)?;
        let log = crate::transcript_read::read_tail(&path, 4 * 1024 * 1024)?;
        codex_latest_reply_since(&log, &since)
    })
    .await
    .ok()?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(at: &str, text: &str) -> String {
        serde_json::json!({
            "type": "response_item",
            "timestamp": at,
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}]
            }
        }).to_string()
    }

    fn task_complete(at: &str, reply: &str) -> String {
        serde_json::json!({
            "type": "event_msg",
            "timestamp": at,
            "payload": {
                "type": "task_complete",
                "last_agent_message": reply
            }
        }).to_string()
    }

    /// 正常完成：本 turn 有 user prompt 與 task_complete
    #[test]
    fn turn_reply_matches_completed_turn() {
        let log = format!(
            "{}\n{}",
            user_msg("2026-10-25T01:00:00Z", "hello"),
            task_complete("2026-10-25T01:00:05Z", "world")
        );
        assert_eq!(
            codex_reply_for_turn(&log, &["hello".into()], "2026-10-25T00:59:50Z"),
            Some("world".into())
        );
    }

    /// ① 上一 turn 有回覆，本 turn 沒有：不得拿上一 turn 的回覆充數
    #[test]
    fn turn_reply_rejects_previous_turn_reply_when_current_turn_missing() {
        let log = format!(
            "{}\n{}\n{}",
            user_msg("2026-10-25T01:00:00Z", "turn 1 prompt"),
            task_complete("2026-10-25T01:00:05Z", "turn 1 reply"),
            user_msg("2026-10-25T01:05:00Z", "turn 2 prompt")
        );
        // 本 turn 是 "turn 2 prompt"，開始於 01:05:00Z，但尚未有 task_complete
        assert_eq!(
            codex_reply_for_turn(&log, &["turn 2 prompt".into()], "2026-10-25T01:04:50Z"),
            None
        );
        // 若問的是 turn 1，但在 turn 1 之後已有新的 user prompt（turn 2），也不得回傳
        assert_eq!(
            codex_reply_for_turn(&log, &["turn 1 prompt".into()], "2026-10-25T00:59:50Z"),
            None
        );
    }

    /// ① rollout 裡前一回合有同一句 sent 且已 task_complete、本回合的 prompt 尚未出現（或在 since 之前）→ 必須回 None
    #[test]
    fn turn_reply_rejects_duplicate_sent_from_previous_turn_before_since() {
        let log = format!(
            "{}\n{}",
            user_msg("2026-10-25T01:00:00Z", "any updates"),
            task_complete("2026-10-25T01:00:05Z", "old update reply")
        );
        // 本回合在 01:05:00Z 送出 "any updates"，但目前 rollout 裡只有 01:00:00Z 的紀錄
        assert_eq!(
            codex_reply_for_turn(&log, &["any updates".into()], "2026-10-25T01:05:00Z"),
            None
        );
        // since 解析失敗一律回 None
        assert_eq!(
            codex_reply_for_turn(&log, &["any updates".into()], "invalid-time"),
            None
        );
    }

    /// #1988 上線驗收：prompt 比 delivered_at 早 1 秒寫進 rollout，仍屬本回合
    #[test]
    fn turn_since_accepts_a_prompt_logged_just_before_delivery() {
        let since = turn_since("2026-10-25T07:12:50.000Z", Some("2026-10-25T07:12:51.920Z"));
        let log = format!(
            "{}\n{}",
            user_msg("2026-10-25T07:12:50.788Z", "notice"),
            task_complete("2026-10-25T07:13:19.336Z", "done")
        );
        assert_eq!(codex_reply_for_turn(&log, &["notice".into()], &since), Some("done".into()));
    }

    /// 上一 turn 的同句 prompt 落在 delivered_at − 5 秒之內、但早於本 turn 的 created_at：不採用
    #[test]
    fn turn_since_rejects_a_previous_same_prompt_inside_the_slack_but_before_creation() {
        let since = turn_since("2026-10-25T01:05:00.000Z", Some("2026-10-25T01:05:01.000Z"));
        assert_eq!(since, "2026-10-25T01:05:00.000Z");
        let log = format!(
            "{}\n{}",
            user_msg("2026-10-25T01:04:58Z", "any updates"),
            task_complete("2026-10-25T01:04:59Z", "old reply")
        );
        assert_eq!(codex_reply_for_turn(&log, &["any updates".into()], &since), None);
    }

    /// delivered_at 是空值（或解析不了）時只用 created_at
    #[test]
    fn turn_since_falls_back_to_created_at_without_delivery() {
        assert_eq!(turn_since("2026-10-25T01:05:00.000Z", None), "2026-10-25T01:05:00.000Z");
        assert_eq!(turn_since("2026-10-25T01:05:00.000Z", Some("not-a-time")), "2026-10-25T01:05:00.000Z");
    }

    /// ① 本 turn 寫到一半（有 user prompt，但無 task_complete）：回傳 None 退回終端快照
    #[test]
    fn turn_reply_rejects_mid_turn_without_task_complete() {
        let log = user_msg("2026-10-25T01:00:00Z", "in progress prompt");
        assert_eq!(
            codex_reply_for_turn(&log, &["in progress prompt".into()], "2026-10-25T00:59:50Z"),
            None
        );
    }

    /// hookless 測試：since 邊界與後續 prompt 守門
    #[test]
    fn latest_reply_since_respects_time_and_interruption() {
        let log = format!(
            "{}\n{}\n{}",
            user_msg("2026-10-25T01:00:00Z", "q1"),
            task_complete("2026-10-25T01:00:05Z", "reply 1"),
            user_msg("2026-10-25T01:05:00Z", "q2")
        );
        // since 晚於 task_complete -> 忽略
        assert_eq!(codex_latest_reply_since(&log, "2026-10-25T01:01:00Z"), None);
        // task_complete 之後有新 user prompt -> 忽略
        assert_eq!(codex_latest_reply_since(&log, "2026-10-25T00:55:00Z"), None);
    }
}
