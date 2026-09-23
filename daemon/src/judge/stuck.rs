//! 卡在不認識的畫面（issue #240 第二個場景，2026-09-23）：有 prompt 排在 `queued`、pane 判 idle、卻幾分鐘都送不出去——
//! 2.1.278 的「Auto mode」推銷框就是這個形狀，daemon 什麼都沒說，等 30 分鐘把交辦撤回。
//!
//! 這裡問 Jev 一題是非題：畫面最底部是不是有個介面自己畫的選單／確認框在等使用者選。**不按任何鍵**：regex 認得的框
//! （switch model／trust／auto mode／登入）各有自己的處理，這一條只管「daemon 不認得的」——Jev 說是就推一則
//! `judge_stuck_screen` 進 AGM 收件匣（帶遮罩後的畫面尾段），讓人一分鐘內看到，而不是半小時後才發現。
//! 同一個 run 同一個畫面只問一次；答案照樣寫進 `judge_shadow`（`regex_verdict='stuck_queued'`）。

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::state::App;

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
pub async fn candidates(app: &Arc<App>, after_secs: i64) -> Result<Vec<Stuck>> {
    let cutoff = crate::db::iso_in(-after_secs);
    Ok(sqlx::query_as::<_, Stuck>(
        "SELECT b.id AS bot_id, r.id AS run_id, r.pane_id AS pane_id, t.id AS turn_id,
                CAST((julianday('now') - julianday(t.created_at)) * 86400 AS INTEGER) AS waited_secs
           FROM turns t JOIN conversations c ON c.id = t.conversation_id
           JOIN bots b ON b.id = c.bot_id AND b.deleted_at IS NULL
           JOIN runs r ON r.bot_id = b.id AND r.state = 'running' AND r.agent_status = 'idle' AND r.pane_id IS NOT NULL
          WHERE t.status = 'queued' AND t.created_at <= ?",
    )
    .bind(cutoff)
    .fetch_all(&app.db)
    .await?)
}

/// 控制迴圈每拍呼叫：關著就零成本。每一顆各自成敗，失敗只記 log。
pub async fn sweep(app: &Arc<App>) {
    let cfg = app.cfg.get().await.judge;
    if !cfg.enabled {
        return;
    }
    let cands = match candidates(app, STUCK_AFTER_SECS).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "judge stuck sweep: cannot list queued turns");
            return;
        }
    };
    for c in cands {
        if let Err(e) = inspect(app, &c).await {
            tracing::debug!(bot = %c.bot_id, error = %e, "judge stuck sweep skipped");
        }
    }
}

/// 讀畫面、問 Jev、記帳本、必要時推通知。回 `Ok(Some(p))`＝問了，`Ok(None)`＝這一輪沒問（開關、保險絲、同畫面問過）。
pub async fn inspect(app: &Arc<App>, c: &Stuck) -> Result<Option<f64>> {
    let cfg = app.cfg.get().await.judge;
    let Some(bot) = crate::db::bot(&app.db, &c.bot_id).await? else { return Ok(None) };
    let label = crate::db::project(&app.db, &bot.project_id).await?.map(|p| p.label).unwrap_or_default();
    let asked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE at >= ?")
        .bind(crate::db::iso_in(-3600))
        .fetch_one(&app.db)
        .await?;
    if let Some(skip) = super::gate(&cfg, &bot.project_id, &label, asked) {
        return Err(anyhow!("{skip:?}"));
    }
    let Some(run) = crate::db::active_run(&app.db, &c.bot_id).await? else { return Ok(None) };
    if run.id != c.run_id {
        return Ok(None);
    }
    let Some(client) = app.herdr_for_run(&run).await else { return Ok(None) };
    // 樣式讀：輸入框裡只有 claude 的「建議下一句」（dim）時是空輸入列，不是框在擋（`plain_without_hints`）。
    let styled = crate::lifecycle::read_styled(&client, &c.pane_id, "visible", 60).await?;
    let text = crate::lifecycle::plain_without_hints(&bot.kind, &styled);
    let lines: Vec<&str> = text.lines().collect();
    // 輸入列空著＝沒有框在擋；那是別的問題（例如 flush 沒被叫醒），不是這裡要看的。
    if crate::tui_prompts::composer_is_idle(&lines) {
        return Ok(None);
    }
    let tail = super::tail(&text);
    let digest = fingerprint(&tail);
    // 同一個 run、同一個畫面只問一次。
    let seen: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE run_id = ? AND regex_verdict = 'stuck_queued' AND matched_line = ?")
        .bind(&c.run_id)
        .bind(&digest)
        .fetch_one(&app.db)
        .await?;
    if seen > 0 {
        return Ok(None);
    }
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
    sqlx::query(
        "INSERT INTO judge_shadow (id, at, bot_id, run_id, kind, matched_line, composer_idle, regex_verdict, jev_is_live_ui, model, ms, input_tokens, error)
         VALUES (?, ?, ?, ?, ?, ?, 0, 'stuck_queued', ?, ?, ?, ?, ?)",
    )
    .bind(crate::db::ulid())
    .bind(crate::db::now())
    .bind(&c.bot_id)
    .bind(&c.run_id)
    .bind(&bot.kind)
    .bind(&digest)
    .bind(p)
    .bind(model)
    .bind(ms)
    .bind(tokens)
    .bind(error)
    .execute(&app.db)
    .await?;
    let p = answer?.value;
    if p >= ALERT_THRESHOLD {
        let key = format!("judge_stuck_screen:{}:{digest}", c.run_id);
        let payload = json!({
            "bot_id": c.bot_id, "bot_name": bot.name, "kind": bot.kind, "run_id": c.run_id, "turn_id": c.turn_id,
            "waited_secs": c.waited_secs, "probability": p,
            "screen_tail": tail,
            "action": "這顆 bot 有 prompt 排著送不出去，畫面底部看起來有一個 daemon 不認得的選單／確認框在等人選。到「終端」分頁看一眼、替它選完；認得的框請開票讓 daemon 學會。daemon 沒有按任何鍵。",
        });
        let id = crate::supervisor::store::push_inbox(&app.db, &key, "judge_stuck_screen", None, Some(&c.bot_id), Some(&c.turn_id), &payload).await?;
        if id.is_some() {
            tracing::warn!(bot = %bot.name, run = %c.run_id, p, "judge: a queued prompt looks blocked by a dialog the daemon does not recognise");
            app.emit("supervisor_changed", json!({"judge_stuck_screen": key})).await;
        }
    }
    Ok(Some(p))
}

/// 畫面尾段的短指紋（去重用；不存全文）。
fn fingerprint(tail: &str) -> String {
    // FNV-1a 64：跟 `bin/agm` 備份檔名同一種做法，不引新 crate。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::testing as tt;

    /// 假 Jev：回 `blocked_by_dialog` 的機率，記下收到的 body。
    async fn fake_jev(p: f64) -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::http::StatusCode;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(body);
                (StatusCode::OK, axum::Json(json!({"model": "jev-1.13.0", "answers": {"blocked_by_dialog": {"type": "noul", "noul": p}}, "usage": {"input_tokens": 700}})))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen)
    }

    async fn setup(p: f64, enabled: bool) -> (tt::Env, Stuck, Arc<std::sync::Mutex<Vec<Value>>>, std::path::PathBuf) {
        let env = tt::env().await;
        let app = env.app.clone();
        let (url, seen) = fake_jev(p).await;
        let dir = std::env::temp_dir().join(format!("am-judge-stuck-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, "k-test\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let pid = env.project_id.clone();
        app.cfg
            .update(move |c| {
                c.judge.enabled = enabled;
                c.judge.projects = vec![pid];
                c.judge.key_file = key.to_string_lossy().into_owned();
                c.judge.endpoint = url;
                Ok(())
            })
            .await
            .unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "stuck").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工','2026-09-22T15:00:00.000Z')")
            .bind(&turn_id)
            .bind(&conv)
            .execute(&app.db)
            .await
            .unwrap();
        let stuck = Stuck { bot_id: bot.id.clone(), run_id, pane_id: format!("pane-{}", bot.id), turn_id, waited_secs: 1800 };
        (env, stuck, seen, dir)
    }

    async fn inbox_kinds(app: &Arc<App>) -> Vec<String> {
        crate::supervisor::store::pending_inbox(&app.db).await.unwrap().into_iter().map(|e| e.kind).collect()
    }

    /// 2.1.278 的 Auto mode 框：排了半小時的 prompt、pane idle、輸入列被框占著 → 問 Jev、記帳本、推通知；不按鍵。
    #[tokio::test]
    async fn a_queued_prompt_behind_an_unknown_dialog_raises_one_inbox_event_and_presses_nothing() {
        let (env, s, seen, dir) = setup(0.93, true).await;
        let app = env.app.clone();
        env.herdr.set_screen(&s.pane_id, crate::tui_prompts::screens::AUTO_MODE);
        let cands = candidates(&app, STUCK_AFTER_SECS).await.unwrap();
        assert_eq!(cands.iter().map(|c| c.turn_id.as_str()).collect::<Vec<_>>(), [s.turn_id.as_str()], "候選就是這一筆");
        assert_eq!(inspect(&app, &s).await.unwrap(), Some(0.93));
        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0]["state"]["screen"].as_str().unwrap().contains("keep bypass permissions"));
        assert!(!sent[0].to_string().contains(&s.bot_id), "不送 bot 識別");
        assert_eq!(inbox_kinds(&app).await, ["judge_stuck_screen"]);
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "只通知，不按鍵");
        // 同一個畫面再掃一次：不再問、不再推。
        assert_eq!(inspect(&app, &s).await.unwrap(), None);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(inbox_kinds(&app).await.len(), 1);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE regex_verdict='stuck_queued' AND jev_is_live_ui=0.93").fetch_one(&app.db).await.unwrap();
        assert_eq!(rows, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 低機率只記帳本；輸入列空著（沒有框）連問都不問；關著什麼都不做。
    #[tokio::test]
    async fn low_probability_idle_composer_and_disabled_are_all_quiet() {
        let (env, s, seen, dir) = setup(0.2, true).await;
        let app = env.app.clone();
        env.herdr.set_screen(&s.pane_id, crate::tui_prompts::screens::AUTO_MODE);
        assert_eq!(inspect(&app, &s).await.unwrap(), Some(0.2));
        assert!(inbox_kinds(&app).await.is_empty(), "低於門檻不推通知");
        let idle = "⏺ 做完了。\n────────\n❯\n────────\n  x | agents-manager | Opus 5 | 5h:96%\n";
        env.herdr.set_screen(&s.pane_id, idle);
        assert_eq!(inspect(&app, &s).await.unwrap(), None);
        assert_eq!(seen.lock().unwrap().len(), 1, "輸入列空著不問");
        std::fs::remove_dir_all(&dir).ok();

        let (env, s, seen, dir) = setup(0.99, false).await;
        env.herdr.set_screen(&s.pane_id, crate::tui_prompts::screens::AUTO_MODE);
        sweep(&env.app).await;
        assert!(seen.lock().unwrap().is_empty() && inbox_kinds(&env.app).await.is_empty(), "關著零呼叫");
        std::fs::remove_dir_all(&dir).ok();
    }
}
