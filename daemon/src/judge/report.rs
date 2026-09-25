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
//! `reports/jev-spike/report-evidence/build_cases.py` 量過的那兩題；state 只放遮罩後的回報，
//! 不放交辦原文、bot id、專案 id。

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::state::App;

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

/// 結案寫進 `result` 之後呼叫。**先看開關再丟背景**（#480）：HTTP 不在 tick 裡等。
/// 失敗、沒回報、不是完成回合，都直接回來，結案那條路不受影響。
///
/// 只問 `completed`：`completed_fallback` 的 `result` 是從終端機刮下來的，會混進交辦原文與額度橫幅——
/// #263 的 F34 就是這種，兩題都判錯。真正的回覆遲到補上時（`late_reply`）會以 `completed` 再進來一次。
pub async fn shadow_settled(
    app: &Arc<App>,
    assignment_id: &str,
    bot_id: &str,
    turn_id: Option<&str>,
    turn_status: &str,
    result: Option<&str>,
) {
    if turn_status != "completed" {
        return;
    }
    let Some(report) = result.map(str::trim).filter(|s| !s.is_empty()) else { return };
    if !app.cfg.get().await.judge.enabled {
        return;
    }
    let sample = Sample {
        assignment_id: assignment_id.to_string(),
        bot_id: bot_id.to_string(),
        turn_id: turn_id.map(str::to_string),
        report: report.to_string(),
    };
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = observe(&app, &sample).await {
            tracing::debug!(error = %e, assignment = %sample.assignment_id, "judge report shadow skipped");
        }
    });
}

struct Sample {
    assignment_id: String,
    bot_id: String,
    turn_id: Option<String>,
    report: String,
}

/// 問、記帳、必要時標旗。回 `Ok(None)`＝這次沒問（問過、沒有 bot）。失敗寫進帳本的 `error`，不標旗。
async fn observe(app: &Arc<App>, s: &Sample) -> Result<Option<Flag>> {
    let cfg = app.cfg.get().await.judge;
    let Some(bot) = crate::db::bot(&app.db, &s.bot_id).await? else { return Ok(None) };
    let label = crate::db::project(&app.db, &bot.project_id).await?.map(|p| p.label).unwrap_or_default();
    if let Some(skip @ (super::Skip::Disabled | super::Skip::ProjectNotListed)) = super::gate(&cfg, &bot.project_id, &label, 0) {
        return Err(anyhow!("{skip:?}"));
    }
    let seen: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM judge_shadow WHERE assignment_id = ? AND regex_verdict = ?",
    )
    .bind(&s.assignment_id)
    .bind(VERDICT)
    .fetch_one(&app.db)
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
        .execute(&app.db)
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
    app: &Arc<App>,
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
    .execute(&app.db)
    .await?;
    Ok(())
}

async fn raise(app: &Arc<App>, s: &Sample, bot: &crate::db::Bot, flag: &Flag, claims: f64, asks: f64) -> Result<()> {
    let action = action_text(flag, claims, asks);
    let key = format!("judge_report_evidence:{}", s.assignment_id);
    let payload = json!({
        "assignment_id": s.assignment_id,
        "bot_id": s.bot_id,
        "bot_name": bot.name,
        "parent_bot_id": bot.parent_bot_id,
        "turn_id": s.turn_id,
        "claims_verified": claims,
        "asks_parent_action": asks,
        "reasons": flag.reasons(),
        "action": action,
    });
    let id = crate::supervisor::store::push_inbox(
        &app.db,
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
        if let Ok(conv) = crate::db::conversation_id(&app.db, parent).await {
            let note = format!("交辦 {}（{}）的{action}", s.assignment_id, bot.name);
            let _ = crate::lifecycle::insert_message(app, &conv, None, "system", &note, "system", false, None).await;
        }
    }
    Ok(())
}

fn action_text(flag: &Flag, claims: f64, asks: f64) -> String {
    let mut parts = Vec::new();
    if flag.missing_verification {
        parts.push(format!("沒有把握說這則回報宣稱測試、建置或 CI 通過了（{claims:.2}）"));
    }
    if flag.needs_parent {
        parts.push(format!("這則回報看起來要收件者動手或裁示（{asks:.2}）"));
    }
    format!(
        "回報證據旗標（只標記、不阻擋，門檻 {FLAG_THRESHOLD:.1}）：{}。交辦狀態沒有改，也不會自動重派。",
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

async fn run_of(app: &Arc<App>, turn_id: Option<&str>) -> Option<String> {
    let turn_id = turn_id?;
    sqlx::query_scalar::<_, Option<String>>("SELECT run_id FROM turns WHERE id = ?")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .flatten()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db;
    use crate::testing as tt;
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

    /// 假 Jev。`wait` 時先停住，讓測試證明呼叫端沒有在等答案。
    pub(crate) async fn fake_jev(status: u16, claims: f64, asks: f64, wait: bool) -> (String, Arc<std::sync::Mutex<Vec<Value>>>, Arc<tokio::sync::Notify>) {
        use axum::http::StatusCode;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let go = Arc::new(tokio::sync::Notify::new());
        let gate = go.clone();
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            let gate = gate.clone();
            async move {
                log.lock().unwrap().push(body);
                if wait {
                    gate.notified().await;
                }
                (
                    StatusCode::from_u16(status).unwrap(),
                    axum::Json(json!({
                        "model": "jev-1.13.0",
                        "answers": {
                            "claims_verified": {"type": "noul", "noul": claims},
                            "asks_parent_action": {"type": "noul", "noul": asks}
                        },
                        "usage": {"input_tokens": 1400}
                    })),
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen, go)
    }

    struct World {
        env: tt::Env,
        bot_id: String,
        parent_id: String,
        assignment_id: String,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl World {
        fn app(&self) -> &Arc<App> {
            &self.env.app
        }
    }

    pub(crate) async fn enable(app: &Arc<App>, project_id: &str, endpoint: &str, key: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let project_id = project_id.to_string();
        let endpoint = endpoint.to_string();
        let key_path = key.to_string_lossy().into_owned();
        app.cfg
            .update(move |c| {
                c.judge.enabled = true;
                c.judge.projects = vec![project_id];
                c.judge.key_file = key_path;
                c.judge.endpoint = endpoint;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn world(status: u16, claims: f64, asks: f64) -> World {
        let env = tt::env().await;
        let (url, seen, _go) = fake_jev(status, claims, asks, false).await;
        let key = env.dir.join("jev-key");
        std::fs::write(&key, "k-test\n").unwrap();
        enable(&env.app, &env.project_id, &url, &key).await;
        let parent = tt::claude_bot(&env.app, &env.project_id, "parent").await;
        let child = tt::claude_bot(&env.app, &env.project_id, "child").await;
        sqlx::query("UPDATE bots SET parent_bot_id = ?, managed_by = 'child' WHERE id = ?")
            .bind(&parent.id)
            .bind(&child.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let a = crate::supervisor::store::insert_assignment(&env.app.db, None, &child.id, "crid-report", "把功能做完", &[], None, true)
            .await
            .unwrap();
        let settled = crate::supervisor::store::settle_and_notify(
            &env.app.db,
            &a.id,
            "completed",
            true,
            Some("cargo test 12 通過。commit abcdef1。"),
            None,
            "assignment_completed:test",
            "assignment_completed",
            &json!({"bot_id": child.id}),
        )
        .await
        .unwrap();
        assert!(settled.moved, "測試前置：交辦要先收到 awaiting_review");
        World { env, bot_id: child.id, parent_id: parent.id, assignment_id: a.id, seen }
    }

    async fn status_of(app: &Arc<App>, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM supervisor_assignments WHERE id = ?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    async fn flags(app: &Arc<App>, id: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE kind = 'judge_report_evidence' AND assignment_id = ?")
            .bind(id)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    async fn ledger(app: &Arc<App>, id: &str) -> (Option<f64>, Option<f64>, Option<String>, String) {
        sqlx::query_as(
            "SELECT claims_verified, asks_parent_action, error, regex_verdict FROM judge_shadow WHERE assignment_id = ?",
        )
        .bind(id)
        .fetch_one(&app.db)
        .await
        .unwrap()
    }

    async fn parent_notes(app: &Arc<App>, parent_id: &str) -> Vec<String> {
        let conv = db::conversation_id(&app.db, parent_id).await.unwrap();
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'system' ORDER BY created_at, rowid")
            .bind(conv)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    fn sample(w: &World, report: &str) -> Sample {
        Sample { assignment_id: w.assignment_id.clone(), bot_id: w.bot_id.clone(), turn_id: None, report: report.into() }
    }

    #[tokio::test]
    async fn a_report_that_claims_a_passing_check_is_recorded_and_not_flagged() {
        let w = world(200, 0.96, 0.21).await;
        let before = status_of(w.app(), &w.assignment_id).await;
        let flag = observe(w.app(), &sample(&w, "cargo test 12 passed. commit abcdef1.")).await.unwrap();
        assert_eq!(flag, Some(Flag { missing_verification: false, needs_parent: false }));
        assert!(flags(w.app(), &w.assignment_id).await.is_empty(), "有證據、也不用人動手：不標旗");
        assert!(parent_notes(w.app(), &w.parent_id).await.is_empty());
        let (claims, asks, err, verdict) = ledger(w.app(), &w.assignment_id).await;
        assert_eq!((claims, asks, err.as_deref(), verdict.as_str()), (Some(0.96), Some(0.21), None, VERDICT));
        assert_eq!(status_of(w.app(), &w.assignment_id).await, before, "帳本寫了也不能改交辦狀態");
        let sent = w.seen.lock().unwrap()[0].to_string();
        assert!(!sent.contains("ghp_"));
        assert_eq!(w.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_report_without_a_verification_claim_flags_the_parent_and_does_not_block() {
        let w = world(200, 0.08, 0.30).await;
        let before = status_of(w.app(), &w.assignment_id).await;
        let report = "做完了，還沒跑測試。token ghp_abcdefghijklmnopqrstuvwxyz0123456789 不要送出去。";
        let flag = observe(w.app(), &sample(&w, report)).await.unwrap().unwrap();
        assert!(flag.missing_verification && !flag.needs_parent);
        assert_eq!(status_of(w.app(), &w.assignment_id).await, before);
        let inbox = flags(w.app(), &w.assignment_id).await;
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].contains("missing_verification"), "{}", inbox[0]);
        assert!(inbox[0].contains("不阻擋"), "{}", inbox[0]);
        assert!(!inbox[0].contains("ghp_"), "旗標內文不帶回報原文：{}", inbox[0]);
        let notes = parent_notes(w.app(), &w.parent_id).await;
        assert_eq!(notes.len(), 1, "父 bot 聊天室要有一則系統旗標");
        assert!(notes[0].contains(&w.assignment_id) && !notes[0].contains("ghp_"));
        let sent = w.seen.lock().unwrap()[0].to_string();
        assert!(!sent.contains("ghp_"), "送出去之前要遮罩：{sent}");
        assert!(sent.contains("claims_verified") && sent.contains("asks_parent_action"));
        // 同一則再問一次：不打第二次、不再標一面。
        assert!(observe(w.app(), &sample(&w, report)).await.unwrap().is_none());
        assert_eq!(w.seen.lock().unwrap().len(), 1);
        assert_eq!(flags(w.app(), &w.assignment_id).await.len(), 1);
    }

    #[tokio::test]
    async fn a_verified_report_that_asks_the_parent_to_act_is_flagged_too() {
        let w = world(200, 0.97, 0.91).await;
        let flag = observe(w.app(), &sample(&w, "tests passed. please restart the daemon.")).await.unwrap().unwrap();
        assert!(!flag.missing_verification && flag.needs_parent);
        let inbox = flags(w.app(), &w.assignment_id).await;
        assert!(inbox[0].contains("needs_parent_action") && !inbox[0].contains("missing_verification"));
        assert_eq!(status_of(w.app(), &w.assignment_id).await, "awaiting_review");
    }

    #[tokio::test]
    async fn a_failing_jev_writes_the_ledger_and_raises_nothing() {
        let w = world(500, 0.0, 0.0).await;
        let before = status_of(w.app(), &w.assignment_id).await;
        let flag = observe(w.app(), &sample(&w, "做完了。")).await.unwrap();
        assert!(flag.is_none(), "失敗不標旗");
        assert!(flags(w.app(), &w.assignment_id).await.is_empty());
        assert!(parent_notes(w.app(), &w.parent_id).await.is_empty());
        let (claims, asks, err, _) = ledger(w.app(), &w.assignment_id).await;
        assert!(claims.is_none() && asks.is_none());
        assert_eq!(err.as_deref(), Some("http 500"));
        assert_eq!(status_of(w.app(), &w.assignment_id).await, before);
        assert_eq!(w.seen.lock().unwrap().len(), 1, "不重試");
    }

    #[tokio::test]
    async fn shadow_settled_returns_before_jev_answers_and_a_disabled_judge_never_calls() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (url, seen, go) = fake_jev(200, 0.1, 0.1, true).await;
        let key = env.dir.join("jev-key");
        std::fs::write(&key, "k-test\n").unwrap();
        enable(&app, &env.project_id, &url, &key).await;
        app.cfg.update(|c| { c.judge.timeout_ms = 2_000; Ok(()) }).await.unwrap();
        let child = tt::claude_bot(&app, &env.project_id, "child").await;
        // 完成回合才問。失敗回合、從終端機刮下來的 fallback 回覆，連 task 都不起。
        shadow_settled(&app, "A-fail", &child.id, None, "failed", Some("做完了")).await;
        shadow_settled(&app, "A-scraped", &child.id, None, "completed_fallback", Some("把功能做完\n5-hour limit reached")).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(seen.lock().unwrap().is_empty(), "失敗回合與 completed_fallback 都不問");

        let started = Instant::now();
        shadow_settled(&app, "A-hang", &child.id, None, "completed", Some("還沒跑測試")).await;
        assert!(started.elapsed() < Duration::from_millis(400), "不能在 tick 裡等 Jev：{:?}", started.elapsed());
        // 放行之後帳本才會有。先確認此刻還沒有旗標（問都還沒回來）。
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'judge_report_evidence'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 0);
        go.notify_one();
        // 占位列先寫 pending；等它補成終態，才算這次 HTTP 真的結束。
        let got = tt::eventually!(sqlx::query_scalar::<_, Option<String>>("SELECT error FROM judge_shadow WHERE assignment_id = 'A-hang'")
            .fetch_optional(&app.db)
            .await
            .unwrap()
            .is_some_and(|e| e.as_deref() != Some("pending")));
        assert!(got, "背景任務問完要寫帳本");
        app.cfg.update(|c| { c.judge.enabled = false; Ok(()) }).await.unwrap();
        let calls = seen.lock().unwrap().len();
        shadow_settled(&app, "A-off", &child.id, None, "completed", Some("還沒跑測試")).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(seen.lock().unwrap().len(), calls, "關掉之後不再打 API");
    }

    #[tokio::test]
    async fn note_cleared_does_not_stamp_a_report_row() {
        let w = world(200, 0.96, 0.1).await;
        observe(w.app(), &sample(&w, "tests passed, 4 ok.")).await.unwrap();
        crate::judge::note_cleared(&w.app().db, &w.bot_id).await;
        let cleared: Option<String> = sqlx::query_scalar("SELECT cleared_at FROM judge_shadow WHERE assignment_id = ?")
            .bind(&w.assignment_id)
            .fetch_one(&w.app().db)
            .await
            .unwrap();
        assert!(cleared.is_none(), "report_evidence 的 cleared_at 沒有「被清掉」這件事");
    }
}
