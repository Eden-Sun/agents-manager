//! 回合結束沒被偵測到、永遠停在 `in_flight` 的 turn，由這裡收尾（AGM 2026-09-16 交辦）。
//!
//! 正常收尾有兩條路：Stop hook（`hookrecv`）與 working→idle 那一邊的終端備援（[`super::arm_fallback`]）。
//! 兩條都會漏——hook 沒來；備援只收 `delivery='ok'` 的 turn、畫面上殘留 spinner 就放手、事件漏掉那一邊就根本沒排。
//! 漏掉的 turn 佔著 in_flight：同一對話的 queued 送不出去、還佔住「每對話一筆 queued」的名額，
//! 掛在上面的交辦停在 `delivered`。案例：k8bw2f `01M2MR95RXVHADA4TS8M0YHVH7`（09:20 起、09:59 已 idle、
//! 10:22 daemon 重啟才清，期間 6b84fa5 的部署交辦 409 七次被保險絲標 blocked）、
//! R-部署console-fork `01M2MG74HFY3PYD8FMBJKEJX8J`（AskUserQuestion 被中斷）、AGM-responder 09-15 08:49（卡 6.5 小時）。
//!
//! 規則：
//! - run 的 `agent_status` **持續** `idle` 超過門檻（預設 [`IDLE_MINS_DEFAULT`] 分鐘，環境變數 [`IDLE_MINS_ENV`]，
//!   看不懂／0／負數回預設）仍有 in_flight turn，才收。`working`／`blocked`（等人回答）一律不收；
//!   中間閃一下 working 就重算——herdr 的每個狀態事件都經過 [`observe`]，每輪掃描也拿 DB 的現況再對一次。
//! - 先讀 transcript（claude）／rollout（codex）：證得出「我們送的 prompt 之後已經有完整回覆」→ `completed` 並回填回覆；
//!   證不出 → `completed_fallback`，在對話裡寫明理由。**不標 failed**：回合多半是做完了，只是結束沒被看見。
//! - 收尾走 [`emit_turn`]（交辦照一般回合結束流程），並在同一把 bot 鎖裡立刻 flush 這顆 bot 的 queued。
//!
//! idle 計時放記憶體：daemon 重啟後從第一次看到 idle 重新算，寧可晚收，不要誤收。

use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// idle 多久還沒收尾才算卡住。
pub(crate) const IDLE_MINS_DEFAULT: u64 = 5;
pub(crate) const IDLE_MINS_ENV: &str = "AM_STUCK_TURN_IDLE_MINS";
/// 定時掃描的間隔：reconcile 只在連線、agent 出現、子 pane 關掉時才跑，一顆靜靜 idle 的 bot 不會觸發它。
const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// transcript／rollout 只讀尾端這麼多：那個 prompt 如果不在這裡面，就當證不出來。
const LOG_TAIL_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn parse_idle_mins(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok()).filter(|m| *m > 0).map(|m| m as u64).unwrap_or(IDLE_MINS_DEFAULT)
}

pub(crate) fn idle_threshold() -> Duration {
    Duration::from_secs(parse_idle_mins(std::env::var(IDLE_MINS_ENV).ok().as_deref()) * 60)
}

fn idle_since() -> &'static Mutex<HashMap<String, Instant>> {
    static IDLE_SINCE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    IDLE_SINCE.get_or_init(Default::default)
}

/// 記下 run 此刻的狀態。`idle` 只在第一次看到時起算；任何非 idle（working、blocked、unknown）把計時清掉。
pub(crate) fn observe(run_id: &str, agent_status: &str) {
    observe_at(run_id, agent_status, Instant::now());
}

pub(crate) fn observe_at(run_id: &str, agent_status: &str, at: Instant) {
    let mut m = idle_since().lock().unwrap_or_else(|e| e.into_inner());
    if agent_status == "idle" {
        m.entry(run_id.to_string()).or_insert(at);
    } else {
        m.remove(run_id);
    }
}

/// 從第一次看到 idle 到 `now` 過了多久；目前不是 idle（或還沒看過）是 `None`。
pub(crate) fn idle_for(run_id: &str, now: Instant) -> Option<Duration> {
    let m = idle_since().lock().unwrap_or_else(|e| e.into_inner());
    m.get(run_id).map(|since| now.saturating_duration_since(*since))
}

/// 每 [`SWEEP_EVERY`] 掃一次所有主機。
pub fn spawn_stuck_turn_sweeper(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            sweep(&app, None).await;
        }
    });
}

/// 收掉卡住的 turn。`host` 給了只看那一台（reconcile 那一輪用），`None` 全部。回傳收掉的 turn id。
pub async fn sweep(app: &Arc<App>, host: Option<&str>) -> Vec<String> {
    sweep_at(app, host, Instant::now(), idle_threshold()).await
}

pub(crate) async fn sweep_at(app: &Arc<App>, host: Option<&str>, now: Instant, threshold: Duration) -> Vec<String> {
    let rows: Vec<(String, String, String, String)> = match sqlx::query_as(
        "SELECT t.id, r.id, r.bot_id, r.agent_status
           FROM turns t
           JOIN runs r ON r.id = t.run_id
           JOIN bots b ON b.id = r.bot_id
           JOIN projects p ON p.id = b.project_id
          WHERE t.status = 'in_flight' AND r.state = 'running' AND (? IS NULL OR p.host = ?)",
    )
    .bind(host)
    .bind(host)
    .fetch_all(&app.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = ?e, "stuck turn sweep: could not list in-flight turns");
            return Vec::new();
        }
    };
    let mut closed = Vec::new();
    for (turn_id, run_id, bot_id, status) in rows {
        // 每輪拿 DB 的現況再對一次：事件漏掉時，至少不會把 working 當成還在 idle。
        observe_at(&run_id, &status, now);
        if !idle_for(&run_id, now).is_some_and(|d| d >= threshold) {
            continue;
        }
        let lock = app.bot_lock(&bot_id).await;
        let _g = lock.lock().await;
        // 等鎖的時候狀態可能變了：鎖內重讀，run 不是 idle、turn 已經不是那一筆就不動。
        let Ok(Some(run)) = db::run(&app.db, &run_id).await else { continue };
        observe_at(&run_id, &run.agent_status, now);
        if run.state != "running" || run.agent_status != "idle" {
            continue;
        }
        let Some(idle) = idle_for(&run_id, now).filter(|d| *d >= threshold) else { continue };
        let Ok(Some(turn)) = db::in_flight_turn(&app.db, &run_id).await else { continue };
        if turn.id != turn_id {
            continue;
        }
        match close_locked(app, &run, &turn, idle).await {
            Ok(true) => {
                closed.push(turn.id.clone());
                // 卡住的就是排在後面的那一筆：同一把鎖裡馬上送，不等下一次喚醒。
                if let Err(e) = flush_queued_locked(app, &bot_id).await {
                    tracing::warn!(bot = %bot_id, error = ?e, "queued prompt flush after closing a stuck turn failed");
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(turn = %turn.id, error = ?e, "could not close a stuck turn"),
        }
    }
    closed
}

/// 收一筆。`Ok(false)`：CAS 沒搶到（別的路剛好收掉了）。
async fn close_locked(app: &Arc<App>, run: &db::Run, turn: &db::Turn, idle: Duration) -> anyhow::Result<bool> {
    let Some(bot) = db::bot(&app.db, &run.bot_id).await? else { return Ok(false) };
    let reply = proven_reply(app, &bot, run, turn).await;
    let already_answered: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE turn_id = ? AND role = 'assistant')")
            .bind(&turn.id)
            .fetch_one(&app.db)
            .await?;
    let mins = idle.as_secs() / 60;
    let status = if reply.is_some() { "completed" } else { "completed_fallback" };

    let mut tx = app.db.begin().await?;
    let claimed = sqlx::query("UPDATE turns SET status = ?, completed_at = ? WHERE id = ? AND status = 'in_flight'")
        .bind(status)
        .bind(db::now())
        .bind(&turn.id)
        .execute(&mut *tx)
        .await?;
    if claimed.rows_affected() == 0 {
        return Ok(false);
    }
    let message = match reply.as_deref() {
        // 回覆已經有一份（例如 hook 寫了訊息卻沒收掉 turn）：不再寫第二份。
        Some(_) if already_answered => None,
        Some(text) => Some(insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "assistant", text, "transcript", false, None).await?),
        None => {
            let why = format!(
                "這個回合沒有偵測到結束：agent 已經閒置 {mins} 分鐘，turn 仍是 in_flight，由 reconcile 收尾。\
                 transcript 證不出這則 prompt 之後有完整回覆，所以標成 completed_fallback（不是失敗）。"
            );
            Some(insert_message_tx(&mut tx, &turn.conversation_id, Some(&turn.id), "system", &why, "system", false, None).await?)
        }
    };
    tx.commit().await?;
    tracing::warn!(turn = %turn.id, bot = %bot.name, status, idle_mins = mins, "closed a turn stuck in flight after the agent went idle");
    if let Some(m) = message {
        emit_message_added(app, &bot.id, m).await;
    }
    emit_turn(app, &turn.id).await;
    Ok(true)
}

/// 讀 transcript／rollout 尾端：我們送的 prompt 之後，agent 有沒有完整回完。本機 claude／codex 才讀得到。
async fn proven_reply(app: &Arc<App>, bot: &db::Bot, run: &db::Run, turn: &db::Turn) -> Option<String> {
    if db::bot_host(&app.db, &bot.id).await.ok()? != LOCAL_HOST {
        return None;
    }
    let mut sent = turn_echo_texts(app, &turn.id).await;
    if let Some(p) = turn.prompt_text.as_deref().filter(|p| !p.trim().is_empty()) {
        sent.push(p.to_string());
    }
    if sent.is_empty() {
        return None;
    }
    match bot.kind.as_str() {
        "claude" => {
            let path = std::path::PathBuf::from(run.transcript_path.as_deref().filter(|p| !p.trim().is_empty())?);
            let log = tokio::task::spawn_blocking(move || read_tail(&path, LOG_TAIL_BYTES)).await.ok()??;
            claude_reply_after(&log, &sent)
        }
        "codex" => {
            let home = codex_home(app, bot).await?;
            let session = run.native_session_id.clone().filter(|s| !s.trim().is_empty())?;
            let log = tokio::task::spawn_blocking(move || {
                let path = codex_session_log(&home, &session)?;
                read_tail(&path, LOG_TAIL_BYTES)
            })
            .await
            .ok()??;
            codex_reply_after(&log, &sent)
        }
        _ => None,
    }
}

/// 檔案最後 `max` 個位元組（第一行可能被切到一半，解析時自然會跳過）。
fn read_tail(path: &std::path::Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// claude transcript：最後一次出現我們送的 prompt 之後，有 `stop_reason: end_turn` 的 assistant 訊息＝回合真的結束，
/// 回它的文字。之間又出現別的 user prompt（下一個回合）、或只有 tool_use（還在做）、或被中斷沒有 end_turn，都是 `None`。
pub(crate) fn claude_reply_after(log: &str, sent: &[String]) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let start = lines.iter().rposition(|l| transcript_user_text(l).is_some_and(|t| sent.iter().any(|s| s == &t)))?;
    for line in &lines[start + 1..] {
        if transcript_user_text(line).is_some() {
            return None;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let msg = v.get("message")?;
        if msg.get("stop_reason").and_then(Value::as_str) != Some("end_turn") {
            continue;
        }
        let text = msg
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

/// codex rollout：最後一次出現我們送的 prompt 之後，有 `event_msg`／`task_complete` 且帶 `last_agent_message`＝回合結束，
/// 回那句話。`last_agent_message` 是空的（例如撞額度，只有 `error`）不算完整回覆。
pub(crate) fn codex_reply_after(log: &str, sent: &[String]) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let start = lines.iter().rposition(|l| codex_user_text(l).is_some_and(|t| sent.iter().any(|s| s == &t)))?;
    for line in &lines[start + 1..] {
        if codex_user_text(line).is_some() {
            return None;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let p = v.get("payload");
        if v.get("type").and_then(Value::as_str) != Some("event_msg") || p.and_then(|p| p.get("type")).and_then(Value::as_str) != Some("task_complete") {
            continue;
        }
        return p
            .and_then(|p| p.get("last_agent_message"))
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    #[test]
    fn a_bad_threshold_falls_back_to_five_minutes() {
        assert_eq!(parse_idle_mins(None), 5);
        assert_eq!(parse_idle_mins(Some("12")), 12);
        assert_eq!(parse_idle_mins(Some(" 3 ")), 3);
        for bad in ["0", "-4", "five", "", "1.5"] {
            assert_eq!(parse_idle_mins(Some(bad)), 5, "{bad:?}");
        }
    }

    fn claude_user(text: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": text}}).to_string()
    }
    fn claude_assistant(stop: &str, parts: Value) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "stop_reason": stop, "content": parts}}).to_string()
    }

    #[test]
    fn a_claude_turn_is_proven_only_by_an_end_turn_after_our_prompt() {
        let sent = vec!["部署 6b84fa5".to_string()];
        let done = [
            claude_user("舊的問題"),
            claude_assistant("end_turn", json!([{"type": "text", "text": "舊的回答"}])),
            claude_user("部署 6b84fa5"),
            claude_assistant("tool_use", json!([{"type": "text", "text": "我先看一下"}])),
            claude_assistant("tool_use", json!([{"type": "tool_use", "name": "Bash"}])),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}}).to_string(),
            claude_assistant("end_turn", json!([{"type": "text", "text": "部署完成：pid 42894"}])),
            json!({"type": "system", "subtype": "stop_hook_summary"}).to_string(),
        ]
        .join("\n");
        assert_eq!(claude_reply_after(&done, &sent).as_deref(), Some("部署完成：pid 42894"), "最後那句，不是中途的旁白");

        // 被中斷（AskUserQuestion 按掉）：只有 tool_use，沒有 end_turn。
        let interrupted = [claude_user("部署 6b84fa5"), claude_assistant("tool_use", json!([{"type": "tool_use", "name": "AskUserQuestion"}]))].join("\n");
        assert_eq!(claude_reply_after(&interrupted, &sent), None);
        // 舊回合的 end_turn 不能冒充這一回合的。
        let only_old = [claude_user("舊的問題"), claude_assistant("end_turn", json!([{"type": "text", "text": "舊的回答"}])), claude_user("部署 6b84fa5")].join("\n");
        assert_eq!(claude_reply_after(&only_old, &sent), None);
        // 我們的 prompt 之後已經是下一個人的 prompt：那個 end_turn 是別人的。
        let next = [claude_user("部署 6b84fa5"), claude_user("另一件事"), claude_assistant("end_turn", json!([{"type": "text", "text": "另一件事的回答"}]))].join("\n");
        assert_eq!(claude_reply_after(&next, &sent), None);
        // transcript 裡根本沒有這個 prompt。
        assert_eq!(claude_reply_after(&done, &["沒送過".to_string()]), None);
    }

    #[test]
    fn a_codex_turn_is_proven_by_task_complete_with_a_message() {
        let user = |t: &str| json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": t}]}}).to_string();
        let complete = |m: Value| json!({"type": "event_msg", "payload": {"type": "task_complete", "last_agent_message": m}}).to_string();
        let sent = vec!["跑測試".to_string()];
        let log = [user("跑測試"), json!({"type": "event_msg", "payload": {"type": "token_count"}}).to_string(), complete(json!("測試全過"))].join("\n");
        assert_eq!(codex_reply_after(&log, &sent).as_deref(), Some("測試全過"));
        let limit = [user("跑測試"), complete(Value::Null)].join("\n");
        assert_eq!(codex_reply_after(&limit, &sent), None, "撞額度只有 error，不是回覆");
        assert_eq!(codex_reply_after(&user("跑測試"), &sent), None, "還沒 task_complete");
    }

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        run_id: String,
        turn_id: String,
    }

    /// 本機 claude bot、一顆 idle 的 run、一筆 in_flight turn（我們送的是 `prompt`）。
    async fn stuck(prompt: &str) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "k8").await;
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent','test',?)",
        )
        .bind(&run_id)
        .bind(&bot.id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','unknown',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(&run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        insert_message(&app, &conv, Some(&turn_id), "user", prompt, "web", false, None).await.unwrap();
        Fixture { env, bot_id: bot.id, run_id, turn_id }
    }

    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    async fn messages(app: &Arc<App>, turn_id: &str) -> Vec<(String, String, String)> {
        sqlx::query_as("SELECT role, source, content FROM messages WHERE turn_id = ? ORDER BY created_at, rowid")
            .bind(turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    const MIN: Duration = Duration::from_secs(60);

    /// 門檻之前不收；過了門檻、transcript 證不出來 → completed_fallback＋寫明理由，不是 failed；
    /// 交辦照一般流程走到 awaiting_review。
    #[tokio::test]
    async fn an_idle_run_past_the_threshold_closes_its_turn_as_fallback_with_a_reason() {
        let f = stuck("部署 6b84fa5").await;
        let app = f.env.app.clone();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &f.bot_id, "crid-stuck", "部署 6b84fa5", &[], None, true)
            .await
            .unwrap()
            .id;
        crate::supervisor::store::mark_delivered(&app.db, &a, &f.turn_id, "ok").await.unwrap();
        let mut events = app.subscribe_turns();

        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert!(sweep_at(&app, None, t0 + 4 * MIN, 5 * MIN).await.is_empty(), "四分鐘還不收");
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");

        assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await, vec![f.turn_id.clone()]);
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "completed_fallback", "證不出來是 fallback，不是 failed");
        let msgs = messages(&app, &f.turn_id).await;
        let why = msgs.iter().find(|m| m.0 == "system").expect("理由寫進對話");
        assert!(why.2.contains("閒置 6 分鐘") && why.2.contains("reconcile"), "{why:?}");

        let ev = events.try_recv().expect("收尾有推 turn 事件");
        assert_eq!((ev.turn_id.as_str(), ev.status.as_str()), (f.turn_id.as_str(), "completed_fallback"));
        crate::supervisor::controller::reconcile(&app).await;
        let row = crate::supervisor::store::assignment(&app.db, &a).await.unwrap().unwrap();
        assert_eq!(row.status, "awaiting_review", "交辦照一般回合結束流程，不留在 delivered");

        // 已經收掉的不會再收一次。
        assert!(sweep_at(&app, None, t0 + 20 * MIN, 5 * MIN).await.is_empty());
    }

    /// transcript 證得出完整回覆 → completed，回覆補進對話。
    #[tokio::test]
    async fn a_reply_proven_by_the_transcript_completes_the_turn_and_is_backfilled() {
        let f = stuck("部署 6b84fa5").await;
        let app = f.env.app.clone();
        let path = f.env.dir.join("transcript.jsonl");
        let log = [
            claude_user("部署 6b84fa5"),
            claude_assistant("end_turn", json!([{"type": "text", "text": "部署完成：pid 42894"}])),
        ]
        .join("\n");
        std::fs::write(&path, log + "\n").unwrap();
        sqlx::query("UPDATE runs SET transcript_path = ? WHERE id = ?").bind(path.to_string_lossy()).bind(&f.run_id).execute(&app.db).await.unwrap();

        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await.len(), 1);
        assert_eq!(turn(&app, &f.turn_id).await.status, "completed");
        let msgs = messages(&app, &f.turn_id).await;
        assert!(msgs.contains(&("assistant".into(), "transcript".into(), "部署完成：pid 42894".into())), "{msgs:?}");
        assert!(!msgs.iter().any(|m| m.0 == "system"), "證得出來就不用解釋");
    }

    /// working 或 blocked（等人回答）一律不收，就算計時早就超過；閃一下 working 就重算。
    #[tokio::test]
    async fn working_or_blocked_is_never_collected_and_a_blip_of_work_restarts_the_clock() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);

        for status in ["working", "blocked"] {
            sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?").bind(status).bind(&f.run_id).execute(&app.db).await.unwrap();
            assert!(sweep_at(&app, None, t0 + 30 * MIN, 5 * MIN).await.is_empty(), "{status} 不收");
            assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
        }

        // 回到 idle：計時從這一刻重新算，不接著之前那 30 分鐘。
        sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let back = t0 + 30 * MIN;
        observe_at(&f.run_id, "idle", back);
        assert!(sweep_at(&app, None, back + 4 * MIN, 5 * MIN).await.is_empty(), "重算後才四分鐘");
        // 中途又閃一下 working（事件進來），再回 idle。
        observe_at(&f.run_id, "working", back + 4 * MIN);
        observe_at(&f.run_id, "idle", back + 4 * MIN + Duration::from_secs(10));
        assert!(sweep_at(&app, None, back + 6 * MIN, 5 * MIN).await.is_empty(), "閃過 working，從 4 分 10 秒重算");
        assert_eq!(sweep_at(&app, None, back + 10 * MIN, 5 * MIN).await.len(), 1);
    }

    /// 事件漏掉、只有 DB 的狀態變了（reconcile 寫的）：每輪取樣也要重算，不能接著舊的計時。
    #[tokio::test]
    async fn a_status_change_seen_only_in_the_database_restarts_the_clock_too() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        let set = |status: &'static str| {
            let app = app.clone();
            let run = f.run_id.clone();
            async move {
                sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?").bind(status).bind(&run).execute(&app.db).await.unwrap();
            }
        };
        set("working").await;
        assert!(sweep_at(&app, None, t0 + 30 * MIN, 5 * MIN).await.is_empty());
        set("idle").await;
        assert!(sweep_at(&app, None, t0 + 31 * MIN, 5 * MIN).await.is_empty(), "這一輪看到 idle，從這裡起算");
        assert!(sweep_at(&app, None, t0 + 34 * MIN, 5 * MIN).await.is_empty(), "才三分鐘");
        assert_eq!(sweep_at(&app, None, t0 + 37 * MIN, 5 * MIN).await.len(), 1, "沒有事件也收得到");
    }

    /// 等 bot 鎖的時候那顆 bot 開始等人回答（blocked）：拿到鎖要重看一次，不收。
    #[tokio::test]
    async fn a_bot_that_starts_waiting_for_the_user_while_we_wait_for_its_lock_is_left_alone() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        let lock = app.bot_lock(&f.bot_id).await;
        let held = lock.lock().await;
        let app2 = app.clone();
        let sweeping = tokio::spawn(async move { sweep_at(&app2, None, t0 + 6 * MIN, 5 * MIN).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        sqlx::query("UPDATE runs SET agent_status = 'blocked' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        drop(held);
        assert!(sweeping.await.unwrap().is_empty());
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    }

    /// 還沒到門檻的 bot 不去排它的鎖：一顆正在忙著送 prompt 的 bot 不能把整輪掃描卡住。
    #[tokio::test]
    async fn a_bot_below_the_threshold_is_not_waited_on() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        let lock = app.bot_lock(&f.bot_id).await;
        let _held = lock.lock().await;
        let done = tokio::time::timeout(Duration::from_secs(2), sweep_at(&app, None, t0 + MIN, 5 * MIN)).await;
        assert_eq!(done.ok(), Some(vec![]), "沒到門檻就直接略過，不等鎖");
    }

    /// 收尾後，被它擋住的 queued 馬上送出去。
    #[tokio::test]
    async fn closing_a_stuck_turn_flushes_the_prompt_queued_behind_it() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", tt::LivePane { width: Some(120), ..Default::default() });
        let conv = db::conversation_id(&app.db, &f.bot_id).await.unwrap();
        let queued = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending','排在後面的那一則',?)",
        )
        .bind(&queued)
        .bind(&conv)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &queued).await.status, "queued", "卡住的 in_flight 擋著它");

        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert_eq!(sweep_at(&app, None, t0 + 6 * MIN, 5 * MIN).await.len(), 1);
        let q = turn(&app, &queued).await;
        assert_eq!(q.status, "in_flight", "收尾後同一把鎖裡就送出去了");
        assert_eq!(f.env.herdr.pane("pane-1").unwrap().transcript.iter().filter(|l| l.contains("排在後面的那一則")).count(), 1);
    }

    /// 只看指定的主機；遠端的 run 就算卡住，本機那一輪也不碰。
    #[tokio::test]
    async fn a_host_scoped_sweep_leaves_other_hosts_alone() {
        let f = stuck("部署").await;
        let app = f.env.app.clone();
        let t0 = Instant::now();
        observe_at(&f.run_id, "idle", t0);
        assert!(sweep_at(&app, Some("box"), t0 + 6 * MIN, 5 * MIN).await.is_empty());
        assert_eq!(sweep_at(&app, Some(LOCAL_HOST), t0 + 6 * MIN, 5 * MIN).await.len(), 1);
    }
}
