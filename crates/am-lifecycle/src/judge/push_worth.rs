//! 用 Jev 判斷主力 bot 的完成回覆是否值得送出網頁推播（2026-11-28）。
//!
//! 像「已分配任務」「已派給 worker 處理，完成後回報」這類過渡性回執沒有實質結果、也不需要使用者操作，
//! 不必吵醒手機。門檻保守取 [`NOTIFY_THRESHOLD`]（0.35）：只有 Jev 明確判斷不需通知（`< 0.35`）才壓掉，
//! 其餘情況以及 Jev 關閉、專案不在名單、逾時、錯誤或保險絲滿一律 fail-open 照常推播。

use std::time::Instant;

use anyhow::Result;
use serde_json::{json, Value};

pub const VERDICT: &str = "push_worth";
pub const NOTIFY_THRESHOLD: f64 = 0.35;

const PROMPT_CHARS: usize = 1_000;
const REPLY_CHARS: usize = 4_000;
const MATCHED_LINE_CHARS: usize = 120;

pub struct Sample {
    pub bot_id: String,
    pub project_id: String,
    pub kind: String,
    pub turn_id: String,
    pub user_prompt: String,
    pub reply: String,
}

pub fn should_notify_prob(prob: f64) -> bool {
    !prob.is_finite() || prob >= NOTIFY_THRESHOLD
}

pub async fn evaluate(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::judge::JudgeFuse), s: &Sample) -> Result<Option<f64>> {
    let cfg = app.cfg().get().await.judge;
    let label = crate::db::project(app.db(), &s.project_id).await?.map(|p| p.label).unwrap_or_default();
    if super::gate(&cfg, &s.project_id, &label, 0).is_some() {
        return Ok(None);
    }
    let seen: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE run_id = ? AND regex_verdict = ?")
        .bind(&s.turn_id)
        .bind(VERDICT)
        .fetch_one(app.db())
        .await?;
    if seen > 0 {
        return Ok(None);
    }
    let prompt = clip(&super::mask(&s.user_prompt), PROMPT_CHARS);
    let reply = clip(&super::mask(&s.reply), REPLY_CHARS);
    if reply.trim().is_empty() {
        return Ok(None);
    }
    let preview = preview_line(&reply);
    let slot = match super::reserve_slot(app, &cfg, &s.project_id, &label, &s.bot_id, &s.turn_id, &s.kind, &preview, false, VERDICT).await {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let body = request_body(&cfg.model, &prompt, &reply);
    let started = Instant::now();
    let answer = match super::read_key(&cfg.key_file) {
        Ok(key) => super::ask_noul(&cfg, &key, &body, "should_notify").await,
        Err(e) => Err(e),
    };
    let ms = started.elapsed().as_millis() as i64;
    let (p, model, tokens, error) = match &answer {
        Ok(a) => (Some(a.value), a.model.clone(), a.input_tokens, None),
        Err(e) => (None, None, None, Some(e.to_string())),
    };
    super::settle_slot(app, &slot, p, model, ms, tokens, error).await?;
    Ok(answer.ok().map(|a| a.value))
}

fn clip(text: &str, max_chars: usize) -> String {
    let mut out: String = text.trim().chars().take(max_chars).collect();
    if text.trim().chars().count() > max_chars {
        out.push('…');
    }
    out
}

fn preview_line(reply: &str) -> String {
    let line = reply.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut out: String = line.chars().filter(|c| !c.is_control()).take(MATCHED_LINE_CHARS).collect();
    if line.chars().count() > MATCHED_LINE_CHARS {
        out.push('…');
    }
    out
}

pub(crate) fn request_body(model: &str, user_prompt: &str, reply: &str) -> Value {
    json!({
        "model": model,
        "state": {
            "user_prompt": user_prompt,
            "reply": reply,
        },
        "questions": {
            "should_notify": {
                "type": "noul",
                "instructions": {
                    "question": "Should this assistant `reply` trigger a push notification to the user?",
                    "focus": "Return false for brief transitional acknowledgments or task-dispatch receipts that do not yet deliver a substantive result or ask for user action (e.g., '已分配任務', '已派給 worker 處理，完成後回報', '收到，正在跑測試稍後回報', 'Dispatched to worker, will report back'). Return true when the reply reports completed work, concrete findings, errors or blockers, or asks the user a question or decision."
                },
                "criteria": {
                    "true": "The reply delivers a substantive result, answer, error/blocker, or asks the user for input or a decision",
                    "false": "The reply is only a brief acknowledgment, task-dispatch notice, or in-progress status update that does not need the user's attention yet"
                }
            }
        }
    })
}

#[cfg(all(test, not(feature = "test-hooks")))]
mod tests {
    use super::*;

    #[test]
    fn threshold_is_fail_open_and_only_suppresses_confident_low_scores() {
        assert!(!should_notify_prob(0.0));
        assert!(!should_notify_prob(0.12));
        assert!(!should_notify_prob(0.34));
        assert!(should_notify_prob(0.35));
        assert!(should_notify_prob(0.5));
        assert!(should_notify_prob(0.95));
        assert!(should_notify_prob(f64::NAN));
    }

    #[test]
    fn request_body_carries_prompt_and_reply_for_should_notify_noul() {
        let b = request_body("jev-1", "修一下首頁", "已分配任務給 cc0-op-1");
        assert_eq!(b["model"], "jev-1");
        assert_eq!(b["state"]["user_prompt"], "修一下首頁");
        assert_eq!(b["state"]["reply"], "已分配任務給 cc0-op-1");
        assert_eq!(b["questions"]["should_notify"]["type"], "noul");
    }

    #[test]
    fn preview_line_strips_controls_and_clips_long_lines() {
        let long = format!("\n  {}\nsecond", "字".repeat(150));
        let p = preview_line(&long);
        assert_eq!(p.chars().count(), MATCHED_LINE_CHARS + 1);
        assert!(p.ends_with('…'));
    }
}
