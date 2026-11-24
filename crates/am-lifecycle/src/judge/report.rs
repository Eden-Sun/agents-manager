//! 交辦完成回報的證據旗標（issue #558，#263 的 shadow）。
//!
//! 回合收成、`result` 寫進交辦的那一刻，背景問 Jev 評估時定下的兩題 Noul：
//! `claims_verified`（回報有沒有宣稱測試／建置／CI 通過）與 `asks_parent_action`
//!（要不要收件者動手）。門檻 0.8 沿用那次離線重放。不過就推一則 inbox，
//! 父 bot 有 `parent_bot_id` 時再在它的聊天室留一則系統訊息。
//!
//! **不阻擋**：不改交辦狀態、不重派、不按鍵。Jev 關著、專案不在名單、或這次呼叫失敗，
//! 都不標旗。帳本列帶 `assignment_id`，之後拿 `supervisor_assignments.result` 對這兩個機率。
//!
//! 呼叫走 [`crate::judge::post_systemone`]，不另寫 HTTP 客戶端。題目原文是
//! 離線 spike 量過的那兩題（題目檔已移出 repo）；state 只放遮罩後的回報，
//! 不放交辦原文、bot id、專案 id。

use std::time::Instant;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// #263 在這個門檻上記的 precision：`claims_verified` 0.95、`asks_parent_action` 1.00。
pub const FLAG_THRESHOLD: f64 = 0.8;

/// 送出去的回報上限。評估樣本最長 2,864 字；再長的留開頭（回報的結論通常在前）。
const REPORT_CHARS: usize = 12_000;

/// `regex_verdict`。`cleared_at` 只蓋 `limit_hit`，這一類不蓋。
pub const VERDICT: &str = "report_evidence";

#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    pub missing_verification: bool,
    pub needs_parent: bool,
}

impl Flag {
    pub fn raise(&self) -> bool {
        self.missing_verification || self.needs_parent
    }

    pub fn reasons(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.missing_verification {
            out.push("missing_verification");
        }
        if self.needs_parent {
            out.push("needs_parent_action");
        }
        out
    }
}

/// 高機率那側才算「宣稱驗證通過」；低於門檻就是這則沒過證據檢查。
/// `asks_parent_action` 反過來：≥ 門檻才標「要你動手」（評估時只在很確定時閃）。
pub fn decide(claims_verified: f64, asks_parent_action: f64) -> Flag {
    Flag {
        missing_verification: claims_verified < FLAG_THRESHOLD,
        needs_parent: asks_parent_action >= FLAG_THRESHOLD,
    }
}

pub trait ReportNotifier: Send + Sync {
    fn insert_system_message(&self, conv_id: &str, note: &str) -> impl std::future::Future<Output = Result<crate::db::Message>> + Send;
}

pub struct Sample {
    pub assignment_id: String,
    pub bot_id: String,
    pub turn_id: Option<String>,
    pub report: String,
}

/// 問、記帳、必要時標旗。回 `Ok(None)`＝這次沒問（問過、沒有 bot）。失敗寫進帳本的 `error`，不標旗。
pub async fn observe(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::judge::JudgeFuse + ReportNotifier), s: &Sample) -> Result<Option<Flag>> {
    let cfg = app.cfg().get().await.judge;
    let Some(bot) = crate::db::bot(app.db(), &s.bot_id).await? else { return Ok(None) };
    let label = crate::db::project(app.db(), &bot.project_id).await?.map(|p| p.label).unwrap_or_default();
    if let Some(skip @ (super::Skip::Disabled | super::Skip::ProjectNotListed)) = super::gate(&cfg, &bot.project_id, &label, 0) {
        return Err(anyhow!("{skip:?}"));
    }
    let seen: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM judge_shadow WHERE assignment_id = ? AND regex_verdict = ?",
    )
    .bind(&s.assignment_id)
    .bind(VERDICT)
    .fetch_one(app.db())
    .await?;
    if seen > 0 {
        return Ok(None);
    }
    let report = clip(&super::mask(&s.report));
    let digest = fingerprint(&report);
    let run_id = run_of(app, s.turn_id.as_deref()).await.unwrap_or_else(|| s.assignment_id.clone());
    let slot = super::reserve_slot(
        app,
        &cfg,
        &bot.project_id,
        &label,
        &s.bot_id,
        &run_id,
        &bot.kind,
        &digest,
        false,
        VERDICT,
    )
    .await?;
    sqlx::query("UPDATE judge_shadow SET assignment_id = ? WHERE id = ?")
        .bind(&s.assignment_id)
        .bind(&slot)
        .execute(app.db())
        .await?;

    let body = request_body(&cfg.model, &report);
    let started = Instant::now();
    let answer = match super::read_key(&cfg.key_file) {
        Ok(key) => ask_pair(&cfg, &key, &body).await,
        Err(e) => Err(e),
    };
    let ms = started.elapsed().as_millis() as i64;
    let (pair, model, tokens, error) = match &answer {
        Ok(a) => (Some((a.claims_verified, a.asks_parent_action)), a.model.clone(), a.input_tokens, None),
        Err(e) => (None, None, None, Some(e.to_string())),
    };
    settle_pair(app, &slot, pair, model, ms, tokens, error).await?;
    let pair = match answer {
        Ok(a) => a,
        Err(_) => return Ok(None),
    };
    let flag = decide(pair.claims_verified, pair.asks_parent_action);
    if flag.raise() {
        raise(app, s, &bot, &flag, pair.claims_verified, pair.asks_parent_action).await?;
    }
    Ok(Some(flag))
}

struct Pair {
    claims_verified: f64,
    asks_parent_action: f64,
    model: Option<String>,
    input_tokens: Option<i64>,
}

async fn ask_pair(cfg: &crate::config::JudgeCfg, key: &str, body: &Value) -> Result<Pair> {
    let v = super::post_systemone(cfg, key, body).await?;
    let claims = v["answers"]["claims_verified"]["noul"].as_f64().ok_or_else(|| anyhow!("no noul in the answer"))?;
    let asks = v["answers"]["asks_parent_action"]["noul"].as_f64().ok_or_else(|| anyhow!("no noul in the answer"))?;
    Ok(Pair {
        claims_verified: claims,
        asks_parent_action: asks,
        model: v["model"].as_str().map(str::to_string),
        input_tokens: v["usage"]["input_tokens"].as_i64(),
    })
}

async fn settle_pair(
    app: &impl crate::capabilities::Db,
    id: &str,
    pair: Option<(f64, f64)>,
    model: Option<String>,
    ms: i64,
    tokens: Option<i64>,
    error: Option<String>,
) -> Result<()> {
    let (claims, asks) = match pair {
        Some((c, a)) => (Some(c), Some(a)),
        None => (None, None),
    };
    sqlx::query(
        "UPDATE judge_shadow SET claims_verified=?, asks_parent_action=?, model=?, ms=?, input_tokens=?, error=? WHERE id=?",
    )
    .bind(claims)
    .bind(asks)
    .bind(model)
    .bind(ms)
    .bind(tokens)
    .bind(error)
    .bind(id)
    .execute(app.db())
    .await?;
    Ok(())
}

async fn raise(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + ReportNotifier), s: &Sample, bot: &crate::db::Bot, flag: &Flag, claims: f64, asks: f64) -> Result<()> {
    let action = action_text(flag, claims, asks);
    let key = format!("judge_report_evidence:{}", s.assignment_id);
    let payload = super::mark(
        json!({
            "assignment_id": s.assignment_id,
            "bot_id": s.bot_id,
            "bot_name": bot.name,
            "parent_bot_id": bot.parent_bot_id,
            "turn_id": s.turn_id,
            "claims_verified": claims,
            "asks_parent_action": asks,
            "reasons": flag.reasons(),
            "action": action,
        }),
        None,
    );
    let id = crate::supervisor_inbox::push_inbox(
        app.db(),
        &key,
        "judge_report_evidence",
        Some(&s.assignment_id),
        Some(&s.bot_id),
        s.turn_id.as_deref(),
        &payload,
    )
    .await?;
    if id.is_some() {
        tracing::info!(assignment = %s.assignment_id, claims, asks, "judge: report evidence flag");
        app.emit("supervisor_changed", json!({"judge_report_evidence": key})).await;
    }
    if let Some(parent) = bot.parent_bot_id.as_deref() {
        if let Ok(conv) = crate::db::conversation_id(app.db(), parent).await {
            let note = format!("{action}（交辦 {}，{}）", s.assignment_id, bot.name);
            let _ = app.insert_system_message(&conv, &note).await;
        }
    }
    Ok(())
}

fn action_text(flag: &Flag, claims: f64, asks: f64) -> String {
    let mut parts = Vec::new();
    if flag.missing_verification {
        parts.push(format!("沒有把握說這則回報宣稱測試、建置或 CI 通過了（宣稱通過的機率 {}）", super::pct(claims)));
    }
    if flag.needs_parent {
        parts.push(format!("這則回報看起來要收件者動手或裁示（機率 {}）", super::pct(asks)));
    }
    format!(
        "{}：回報證據旗標（只標記、不阻擋，門檻 {FLAG_THRESHOLD:.1}）：{}。交辦狀態沒有改，也不會自動重派。",
        super::badge(None),
        parts.join("；")
    )
}

fn request_body(model: &str, report: &str) -> Value {
    json!({
        "model": model,
        "state": {"report": report},
        "questions": {
            "claims_verified": {
                "type": "noul",
                "instructions": {
                    "question": "Does `report` claim that tests, a build, a type check or CI passed?",
                    "focus": "A check that is described as still running, skipped or unconfirmed is not a claim that it passed."
                },
                "criteria": {
                    "true": "The report asserts at least one test run, build, lint/type check or CI run passed",
                    "false": "No such claim, or the report says the check is still running, was skipped, or is unconfirmed"
                }
            },
            "asks_parent_action": {
                "type": "noul",
                "instructions": {
                    "question": "Does `report` need its recipient — the dispatcher or the user — to do something or decide something?",
                    "focus": "Count an explicit request, an approval or deployment that is waiting on the recipient, a question left for them, or a remaining action handed to them. Do not count the reporter describing what it will do itself or what it told its own sub-agents to do. Do not count the bare line that this round did no release build and no restart."
                },
                "criteria": {
                    "true": "The report needs the recipient to act, approve, decide or answer",
                    "false": "The report is informational; nothing is required from the recipient"
                }
            }
        }
    })
}

fn clip(text: &str) -> String {
    if text.chars().count() <= REPORT_CHARS {
        return text.to_string();
    }
    text.chars().take(REPORT_CHARS).collect()
}

fn fingerprint(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

async fn run_of(app: &impl crate::capabilities::Db, turn_id: Option<&str>) -> Option<String> {
    let turn_id = turn_id?;
    sqlx::query_scalar::<_, Option<String>>("SELECT run_id FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(app.db())
        .await
        .ok()
        .flatten()
        .flatten()
}

#[cfg(all(test, not(feature = "test-hooks")))]
mod tests {
    use super::*;

    use std::time::Duration;

    #[test]
    fn the_threshold_flags_a_missing_claim_and_a_sure_ask() {
        assert_eq!(
            decide(0.80, 0.79),
            Flag { missing_verification: false, needs_parent: false },
            "0.8 剛好宣稱驗證通過，父動作還沒到門檻：不標"
        );
        assert!(decide(0.79, 0.10).missing_verification && !decide(0.79, 0.10).needs_parent);
        assert!(!decide(0.95, 0.80).missing_verification && decide(0.95, 0.80).needs_parent);
        let both = decide(0.11, 0.91);
        assert!(both.raise());
        assert_eq!(both.reasons(), ["missing_verification", "needs_parent_action"]);
    }

    #[test]
    fn the_request_is_the_two_measured_questions_and_nothing_else() {
        let body = request_body("jev-1.13.0", "cargo test 通過。commit abcdef1。");
        assert_eq!(body["model"], "jev-1.13.0");
        let state = body["state"].as_object().unwrap();
        assert_eq!(state.keys().cloned().collect::<Vec<_>>(), ["report"]);
        let mut qs: Vec<_> = body["questions"].as_object().unwrap().keys().cloned().collect();
        qs.sort();
        assert_eq!(qs, ["asks_parent_action", "claims_verified"]);
        assert_eq!(
            body["questions"]["claims_verified"]["instructions"]["question"],
            "Does `report` claim that tests, a build, a type check or CI passed?"
        );
        assert!(body["questions"]["asks_parent_action"]["instructions"]["focus"]
            .as_str()
            .unwrap()
            .contains("this round did no release build"));
        let sent = body.to_string();
        assert!(!sent.contains("assignment") && !sent.contains("bot_id"));
    }

}
