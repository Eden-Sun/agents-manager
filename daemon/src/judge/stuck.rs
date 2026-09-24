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
          WHERE t.status = 'queued' AND t.created_at <= ?
          ORDER BY t.created_at ASC, t.rowid ASC
          LIMIT ?",
    )
    .bind(cutoff)
    .bind(MAX_CANDIDATES)
    .fetch_all(&app.db)
    .await?)
}

/// 一次只准一個 sweep 在跑。`sweep` 是每拍呼叫的，而一輪最久可能要好幾十秒
/// （每顆候選一次 herdr 讀畫面＋一次外部 HTTP），不擋的話會越疊越多。
static SWEEPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 一輪最多**看**幾顆（讀畫面＋問 Jev）。原本沒有上限：排隊的 turn 有多少就做多少次，全塞在同一拍。
const MAX_PER_ROUND: usize = 10;

/// SQL 撈候選的上限。比 [`MAX_PER_ROUND`] 大，輪轉才有東西可以輪——
/// 只撈 10 筆的話，冷卻中的那 10 筆會讓這一輪什麼都不做。
const MAX_CANDIDATES: i64 = 200;

/// 同一筆 turn 看過之後多久內不再看。比 tick（10 秒）長很多，才真的省得下讀畫面的成本；
/// 比 `STUCK_AFTER_SECS`（180 秒）短，卡住的那顆不會太久沒人看。
const SEEN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(120);

/// 控制迴圈每拍呼叫。**丟背景跑**（issue #480）：一輪要做 herdr 讀畫面與外部 Jev 呼叫，
/// 同步 await 會把整個 tick 拖住——同模組的 [`crate::judge::shadow_limit_hit`] 早就是這樣做的，
/// 這裡跟上。`STUCK_AFTER_SECS` 是 180 秒，少跑幾拍完全沒差。
///
/// 同時只准一個在跑：上一輪還沒做完就直接跳過這一拍，不排隊、不疊加。
pub fn sweep(app: &Arc<App>) {
    let Some(guard) = SweepGuard::take() else {
        tracing::debug!("judge stuck sweep: 上一輪還在跑，這一拍跳過");
        return;
    };
    let app = app.clone();
    tokio::spawn(async move {
        // guard 在這個 task 結束時才放掉，**包含 panic**（i339 review #480）：
        // 用 `store(false)` 寫在最後一行的話，只要 `sweep_once` 裡任何一步 panic 就永遠放不掉——
        // tokio 不會因為 task panic 中止行程，所以 daemon 照常活著、sweep 從此靜靜不再跑，一行 log 都沒有。
        let _guard = guard;
        sweep_once(&app).await;
    });
}

/// 拿到就代表「這一輪歸我跑」，drop 的時候放掉（panic 也會 drop）。
struct SweepGuard;

impl SweepGuard {
    fn take() -> Option<Self> {
        (!SWEEPING.swap(true, std::sync::atomic::Ordering::SeqCst)).then_some(SweepGuard)
    }
}

impl Drop for SweepGuard {
    fn drop(&mut self) {
        SWEEPING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 一輪的本體。測試直接叫這支（不經過單一併發守衛，才不會跟平行跑的別條測試互相擋）。
pub(crate) async fn sweep_once(app: &Arc<App>) {
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
    // 冷卻 ＋ 輪轉（i339 review #480）：`LIMIT` ＋ `created_at ASC` 單獨用會**餓死第 11 顆**——
    // `inspect` 不會改 turn 的 `queued`，候選條件只會隨時間更成立，所以最舊那幾顆永遠佔滿名額。
    // 看過的記在記憶體裡，冷卻期內跳過；名額自然輪到後面的。
    // 這也順手省掉「已經問過的那幾顆每輪再付一次 herdr 讀畫面」——那個 dedupe 的 key 是畫面指紋，
    // 讀完畫面才算得出來，所以擋不住讀的成本，只有這一層擋得住。
    let now = std::time::Instant::now();
    let due: Vec<Stuck> = {
        let mut seen = app.judge_stuck_seen.lock().await;
        seen.retain(|_, t| now.duration_since(*t) < SEEN_COOLDOWN);
        let picked: Vec<Stuck> = cands.into_iter().filter(|c| !seen.contains_key(&c.turn_id)).take(MAX_PER_ROUND).collect();
        for c in &picked {
            seen.insert(c.turn_id.clone(), now);
        }
        picked
    };
    for c in due {
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
    // 便宜的早退：開關與專案名單在讀畫面之前就看得出來。真正的名額是等到要問之前才占
    // （`reserve_slot`），否則占了卻因為「輸入列空著」「同畫面問過」而沒問，名額就白燒了。
    if let Some(skip @ (super::Skip::Disabled | super::Skip::ProjectNotListed)) = super::gate(&cfg, &bot.project_id, &label, 0) {
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
    // 跟 `observe` 走**同一支**占位（i339 review #481）：只有一邊占位的話，另一邊照樣衝得過保險絲。
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
        sweep_once(&env.app).await;
        assert!(seen.lock().unwrap().is_empty() && inbox_kinds(&env.app).await.is_empty(), "關著零呼叫");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #480：一輪最多**看** `MAX_PER_ROUND` 顆，而且下一輪要輪到後面的（不能餓死第 11 顆）。
    ///
    /// 一個對話只能有一筆 queued（`turns.conversation_id` 上的唯一索引），所以候選數等於
    /// 「有 queued turn 的 bot 數」——要撐出上限就得真的多開幾顆 bot。
    #[tokio::test]
    async fn a_round_is_bounded_and_the_next_round_moves_on() {
        let (env, _s, _seen, dir) = setup(0.99, true).await;
        let app = env.app.clone();
        let total = MAX_PER_ROUND + 5;
        for i in 0..total {
            let bot = tt::claude_bot(&app, &env.project_id, &format!("stuck-{i}")).await;
            tt::fake_run(&app, &bot.id).await;
            let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
            sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工','2026-09-22T15:00:00.000Z')")
                .bind(db::ulid())
                .bind(&conv)
                .execute(&app.db)
                .await
                .unwrap();
        }
        // SQL 那一層撈得比每輪上限多，輪轉才有東西可以輪。
        let all = candidates(&app, STUCK_AFTER_SECS).await.unwrap();
        assert!(all.len() > MAX_PER_ROUND, "撈到的要比每輪上限多，否則輪不動：{}", all.len());

        // 第一輪挑的數量有上限；第二輪（冷卻還在）要換一批，不能又是同一批。
        let pick = |app: Arc<App>| async move {
            let now = std::time::Instant::now();
            let mut seen = app.judge_stuck_seen.lock().await;
            seen.retain(|_, t| now.duration_since(*t) < SEEN_COOLDOWN);
            let cands = candidates(&app, STUCK_AFTER_SECS).await.unwrap();
            let picked: Vec<String> =
                cands.into_iter().filter(|c| !seen.contains_key(&c.turn_id)).take(MAX_PER_ROUND).map(|c| c.turn_id).collect();
            for t in &picked {
                seen.insert(t.clone(), now);
            }
            picked
        };
        let first = pick(app.clone()).await;
        assert_eq!(first.len(), MAX_PER_ROUND, "一輪要有上限");
        let second = pick(app.clone()).await;
        assert!(!second.is_empty(), "第二輪要輪到後面的，不能被最舊那幾顆餓死");
        assert!(second.iter().all(|t| !first.contains(t)), "第二輪不該又是同一批");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #480：同時只准一輪在跑；而且 **panic 也要放掉**——用「最後一行 `store(false)`」寫法時，
    /// 一次 panic 就會讓 sweep 從此靜靜不再跑（tokio 不會因為 task panic 收掉行程）。
    #[test]
    fn the_sweep_guard_is_released_even_when_the_round_panics() {
        use std::sync::atomic::Ordering;
        assert!(!SWEEPING.load(Ordering::SeqCst), "起點是沒人在跑");
        {
            let _g = SweepGuard::take().expect("第一個搶得到");
            assert!(SweepGuard::take().is_none(), "第二個搶不到——那一拍就跳過");
        }
        assert!(!SWEEPING.load(Ordering::SeqCst), "正常結束要放掉");

        // panic 的那一輪：guard 在 unwind 時被 drop，下一輪照樣搶得到。
        let hit = std::panic::catch_unwind(|| {
            let _g = SweepGuard::take().expect("panic 那一輪也拿得到");
            panic!("boom");
        });
        assert!(hit.is_err(), "這裡就是要它 panic");
        assert!(!SWEEPING.load(Ordering::SeqCst), "panic 之後也要放掉，否則 sweep 從此不再跑");
        assert!(SweepGuard::take().is_some(), "下一輪拿得到");
        SWEEPING.store(false, Ordering::SeqCst);
    }
}
