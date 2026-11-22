//! 卡在不認識的畫面（issue #240 第二個場景，2026-09-23）：有 prompt 排在 `queued`、pane 判 idle、卻幾分鐘都送不出去——
//! 2.1.278 的「Auto mode」推銷框就是這個形狀，daemon 什麼都沒說，等 30 分鐘把交辦撤回。
//!
//! 這裡問 Jev 一題是非題：畫面最底部是不是有個介面自己畫的選單／確認框在等使用者選。**不按任何鍵**：regex 認得的框
//! （switch model／trust／auto mode／登入）各有自己的處理，這一條只管「daemon 不認得的」——Jev 說是就推一則
//! `judge_stuck_screen` 進 AGM 收件匣（帶遮罩後的畫面尾段），讓人一分鐘內看到，而不是半小時後才發現。
//! 同一個 run 同一個畫面只問一次；答案照樣寫進 `judge_shadow`（`regex_verdict='stuck_queued'`）。

use std::time::Instant;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// SQL 撈候選的上限。
const MAX_CANDIDATES: i64 = 200;

/// 排隊多久沒送出去才算卡住。比 `assignment_queue_wait_secs`（撤回門檻）短得多，否則問到的時候交辦已經被撤了。
pub const STUCK_AFTER_SECS: i64 = 180;
/// Jev 的機率過這個值才推通知；低於的只記帳本。
pub const ALERT_THRESHOLD: f64 = 0.7;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Stuck {
    pub bot_id: String,
    pub run_id: String,
    pub pane_id: String,
    pub turn_id: String,
    pub waited_secs: i64,
}

/// 排了超過 `after_secs` 的 queued turn，且它的 bot 有一個 `running`／`idle` 的 run（有 pane）。
pub async fn candidates(app: &impl crate::capabilities::Db, after_secs: i64) -> Result<Vec<Stuck>> {
    let cutoff = crate::db::iso_in(-after_secs);
    Ok(sqlx::query_as::<_, Stuck>(
        "SELECT b.id AS bot_id, r.id AS run_id, r.pane_id AS pane_id, t.id AS turn_id,
                CAST((julianday('now') - julianday(t.created_at)) * 86400 AS INTEGER) AS waited_secs
           FROM turns t JOIN conversations c ON c.id = t.conversation_id
           JOIN bots b ON b.id = c.bot_id AND b.deleted_at IS NULL
           JOIN runs r ON r.bot_id = b.id AND r.state = 'running' AND r.agent_status = 'idle' AND r.pane_id IS NOT NULL
          WHERE t.status = 'queued' AND t.created_at <= ?
          ORDER BY t.created_at ASC, t.rowid ASC
          LIMIT ?",
    )
    .bind(cutoff)
    .bind(MAX_CANDIDATES)
    .fetch_all(app.db())
    .await?)
}

pub trait StuckPaneReader: Send + Sync {
    fn read_pane_plain_text(&self, client: &crate::herdr::HerdrClient, pane_id: &str, kind: &str) -> impl std::future::Future<Output = Result<String>> + Send;
}

/// 讀畫面、問 Jev、記帳本、必要時推通知。回 `Ok(Some(p))`＝問了，`Ok(None)`＝這一輪沒問（開關、保險絲、同畫面問過）。
pub async fn inspect(app: &(impl crate::capabilities::Cfg + crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::HerdrRoutes + crate::judge::JudgeFuse + StuckPaneReader), c: &Stuck) -> Result<Option<f64>> {
    let cfg = app.cfg().get().await.judge;
    let Some(bot) = crate::db::bot(app.db(), &c.bot_id).await? else { return Ok(None) };
    let label = crate::db::project(app.db(), &bot.project_id).await?.map(|p| p.label).unwrap_or_default();
    if let Some(skip @ (super::Skip::Disabled | super::Skip::ProjectNotListed)) = super::gate(&cfg, &bot.project_id, &label, 0) {
        return Err(anyhow!("{skip:?}"));
    }
    let Some(run) = crate::db::active_run(app.db(), &c.bot_id).await? else { return Ok(None) };
    if run.id != c.run_id {
        return Ok(None);
    }
    let Some(client) = app.herdr_for_run(&run).await else { return Ok(None) };
    let text = app.read_pane_plain_text(&client, &c.pane_id, &bot.kind).await?;
    let lines: Vec<&str> = text.lines().collect();
    if crate::tui_prompts::composer_is_idle(&lines) {
        return Ok(None);
    }
    let tail = super::tail(&text);
    let digest = fingerprint(&tail);
    let seen: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE run_id = ? AND regex_verdict = 'stuck_queued' AND matched_line = ?")
        .bind(&c.run_id)
        .bind(&digest)
        .fetch_one(app.db())
        .await?;
    if seen > 0 {
        return Ok(None);
    }
    let slot = super::reserve_slot(app, &cfg, &bot.project_id, &label, &c.bot_id, &c.run_id, &bot.kind, &digest, false, "stuck_queued").await?;
    let body = request_body(&cfg.model, &bot.kind, c.waited_secs, &tail);
    let started = Instant::now();
    let answer = match super::read_key(&cfg.key_file) {
        Ok(key) => super::ask_noul(&cfg, &key, &body, "blocked_by_dialog").await,
        Err(e) => Err(e),
    };
    let ms = started.elapsed().as_millis() as i64;
    let (p, model, tokens, error) = match &answer {
        Ok(a) => (Some(a.value), a.model.clone(), a.input_tokens, None),
        Err(e) => (None, None, None, Some(e.to_string())),
    };
    super::settle_slot(app, &slot, p, model, ms, tokens, error).await?;
    let p = answer?.value;
    if p >= ALERT_THRESHOLD {
        let key = format!("judge_stuck_screen:{}:{digest}", c.run_id);
        let payload = super::mark(
            json!({
                "bot_id": c.bot_id, "bot_name": bot.name, "kind": bot.kind, "run_id": c.run_id, "turn_id": c.turn_id,
                "waited_secs": c.waited_secs, "probability": p,
                "screen_tail": tail,
                "action": format!(
                    "{}：這顆 bot 有 prompt 排著送不出去，畫面底部看起來有一個 daemon 不認得的選單／確認框在等人選。到「終端」分頁看一眼、替它選完；認得的框請開票讓 daemon 學會。daemon 沒有按任何鍵。",
                    super::badge(Some(p))
                ),
            }),
            Some(p),
        );
        let id = crate::supervisor_inbox::push_inbox(app.db(), &key, "judge_stuck_screen", None, Some(&c.bot_id), Some(&c.turn_id), &payload).await?;
        if id.is_some() {
            tracing::warn!(bot = %bot.name, run = %c.run_id, p, "judge: a queued prompt looks blocked by a dialog the daemon does not recognise");
            app.emit("supervisor_changed", json!({"judge_stuck_screen": key})).await;
        }
    }
    Ok(Some(p))
}

/// 畫面尾段的短指紋（去重用；不存全文）。
fn fingerprint(tail: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in tail.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn request_body(model: &str, kind: &str, waited_secs: i64, screen: &str) -> Value {
    json!({
        "model": model,
        "state": {"agent": kind, "queued_prompt_waiting_secs": waited_secs, "screen": screen},
        "questions": {"blocked_by_dialog": {
            "type": "noul",
            "instructions": {
                "question": "At the bottom of `screen`, is the agent program's own interface currently showing a menu, selection list, or confirmation dialog that is waiting for the user to choose or confirm something?",
                "focus": "Only the live state at the bottom of the screen counts. Options drawn by the interface look like numbered or bulleted choices with a cursor marker, Yes/No or Enter/Esc hints. Ignore menus or dialogs that are merely quoted inside the agent's prose, code, diffs or logs above. A plain empty input box with a prompt cursor, or a spinner showing the agent is working, is not a dialog."
            },
            "criteria": {
                "true": "A live menu or confirmation dialog drawn by the interface is waiting for input right now",
                "false": "No dialog: the interface shows an input box, streaming output, a spinner, or only quoted text"
            }
        }}
    })
}

