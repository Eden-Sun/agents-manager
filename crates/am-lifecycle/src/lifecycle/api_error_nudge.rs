//! claude 回合被「暫時性」API 錯誤截斷（`Connection lost mid-response`、529、5xx、逾時…）時，daemon 自動補一句續行提示。
//!
//! `turn_error::capture` 把回合收成 failed、釘上 `runs.turn_error` 之後就停在那裡，要人手動打「繼續」。這裡在 capture 之後
//! [`arm`] 一個記憶體裡的等待（排程與防重沿用 `resume_nudge` 的做法），等 [`WAIT`] 之後全部成立才排一則 queued 回合：
//! - 仍是同一個 run、`running`、畫面 `idle`（不是 working／blocked）、`turn_error` 還在；
//! - 沒有回合在飛、佇列是空的、被截斷的那一回合仍是這個 run 的最後一回合（這段期間沒有任何新回合）；
//! - 使用者沒有從網頁中斷過（`interrupt_grace`）。
//!
//! 只認暫時性（[`is_transient`]）：撞額度、模型滿載、資安過濾、4xx 一律不補。同一個被截斷的回合只補一次
//! （`client_request_id = api-error-nudge:<turn_id>`）；補出去的那一輪又被截斷，不再補（看被截斷回合的 crid 前綴）。
//! 只有 claude；本機遠端都行（只靠畫面與 DB）。等待在這個行程的記憶體裡，daemon 重啟就不補。

use super::s6_ports::ResumeNudgeContext;
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// `client_request_id` 前綴，後面接被截斷的回合 id。
pub const CRID_PREFIX: &str = "api-error-nudge:";

/// 錯誤被記下之後等這麼久才確認、才補。
pub const WAIT: Duration = Duration::from_secs(30);

fn nudge_text(reason: &str) -> String {
    format!(
        "[來自 AG Man daemon] 上一輪回應被 API 斷線截斷（{reason}）。請先確認最後一個工具的實際結果，再接著把原本的工作做完；如果已經做完，簡短回報即可。"
    )
}

/// 第一個 `code` 之前、之後都不是數字的位置存在嗎（`503` 不吃 `15032`）。
fn has_code(s: &str, code: &str) -> bool {
    s.match_indices(code).any(|(i, _)| {
        let before = s[..i].chars().next_back().is_none_or(|c| !c.is_ascii_digit());
        let after = s[i + code.len()..].chars().next().is_none_or(|c| !c.is_ascii_digit());
        before && after
    })
}

/// 純規則：`runs.turn_error` 這一行是不是暫時性的 API 錯誤（重試或接著做有機會成功）。
pub fn is_transient(line: &str) -> bool {
    let l = line.to_lowercase();
    if crate::turn_error::kind_of(line) != "api_error" || crate::turn_error::is_quota_limit(line) {
        return false; // 額度撞限、模型滿載
    }
    if l.contains("資安過濾") || l.contains("can’t be shown") || l.contains("can't be shown") {
        return false;
    }
    if ["invalid_request", "authentication", "permission", "unauthorized", "forbidden", "not_found", "rate limit", "rate_limit", "quota", "credit balance"]
        .iter()
        .any(|k| l.contains(k))
    {
        return false;
    }
    if (400..500).any(|c| has_code(&l, &c.to_string())) {
        return false;
    }
    ["connection lost", "connection error", "overloaded", "econnreset", "timeout", "timed out", "socket hang up", "etimedout"]
        .iter()
        .any(|k| l.contains(k))
        || ["529", "500", "502", "503", "504"].iter().any(|c| has_code(&l, c))
}

/// 原因摘要：去掉前導記號、截到 80 字。
fn summary(line: &str) -> String {
    let s = line.trim().trim_start_matches(['⏺', '●', '•', '■']).trim();
    let s: String = s.chars().take(80).collect();
    s.trim().to_string()
}

struct Armed {
    run_id: String,
    turn_id: String,
    reason: String,
    at: chrono::DateTime<chrono::Utc>,
}

#[derive(Default)]
struct State {
    armed: HashMap<String, Armed>,
    /// 每顆 bot 最後處理過（排過、取消、或判定不補）的被截斷回合：capture 在同一個畫面上會重複呼叫，不重複 arm。
    handled: HashMap<String, String>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 不在 `live` 裡的 bot 不留記憶體帳。
pub fn retain_bots(live: &[String]) {
    let mut g = lock();
    g.armed.retain(|id, _| live.contains(id));
    g.handled.retain(|id, _| live.contains(id));
}

/// `capture` 之後叫（呼叫端持 bot 鎖）：這個 run 現在釘著暫時性錯誤、最後一回合不是 nudge 自己的，就記成候選並排 [`WAIT`] 後的確認。
pub async fn arm(app: &impl ResumeNudgeContext, bot_id: &str, run_id: &str) {
    let Ok(Some(bot)) = db::bot(app.db(), bot_id).await else { return };
    if bot.kind != "claude" {
        return;
    }
    let Ok(Some(run)) = db::run(app.db(), run_id).await else { return };
    let Some(line) = run.turn_error.as_deref().filter(|l| is_transient(l)) else { return };
    let turn: Option<db::Turn> = sqlx::query_as("SELECT * FROM turns WHERE run_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1")
        .bind(run_id)
        .fetch_optional(app.db())
        .await
        .ok()
        .flatten();
    let Some(turn) = turn else { return };
    if lock().handled.get(bot_id) == Some(&turn.id) {
        return;
    }
    if turn.client_request_id.as_deref().is_some_and(|c| c.starts_with(CRID_PREFIX)) {
        lock().handled.insert(bot_id.to_string(), turn.id.clone());
        tracing::info!(bot = %bot.name, run = run_id, turn = %turn.id, "the nudge itself was cut short by an API error; not nudging again");
        return;
    }
    {
        let mut g = lock();
        g.handled.insert(bot_id.to_string(), turn.id.clone());
        g.armed.insert(
            bot_id.to_string(),
            Armed { run_id: run_id.to_string(), turn_id: turn.id.clone(), reason: summary(line), at: chrono::Utc::now() },
        );
    }
    tracing::info!(bot = %bot.name, run = run_id, turn = %turn.id, error = line, "transient API error; a continue nudge follows if the bot sits idle");
    schedule(app, bot_id, &turn.id);
}

/// 測試裡不排：測試直接叫 [`check`]。
fn schedule<C: ResumeNudgeContext>(app: &C, bot_id: &str, turn_id: &str) {
    if cfg!(test) {
        return;
    }
    let app = (*app).clone();
    let bot_id = bot_id.to_string();
    let turn_id = turn_id.to_string();
    tokio::spawn(Box::pin(async move {
        tokio::time::sleep(WAIT).await;
        let bot_lock = app.bot_lock(&bot_id).await;
        let _g = bot_lock.lock_owned().await;
        let still_armed = lock().armed.get(&bot_id).is_some_and(|a| a.turn_id == turn_id);
        if still_armed {
            check(&app, &bot_id).await;
        }
    }));
}

async fn verdict(app: &impl crate::capabilities::Db, bot_id: &str, a: &Armed) -> Result<(), &'static str> {
    let Ok(Some(run)) = db::active_run(app.db(), bot_id).await else { return Err("no active run") };
    if run.id != a.run_id || run.state != "running" {
        return Err("the run changed");
    }
    if run.agent_status != "idle" {
        return Err("the bot is not idle");
    }
    if run.turn_error.as_deref().is_none_or(|e| e.trim().is_empty()) {
        return Err("the error was cleared (a new turn started)");
    }
    if super::interrupt_grace::hold_of(bot_id).is_some_and(|at| at >= a.at) {
        return Err("the user interrupted");
    }
    if db::in_flight_turn(app.db(), &run.id).await.map_err(|_| "db error")?.is_some() {
        return Err("a turn is in flight");
    }
    let conv = db::conversation_id(app.db(), bot_id).await.map_err(|_| "db error")?;
    let (queued, last): (i64, Option<String>) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM turns WHERE conversation_id = ? AND status = 'queued'),
                (SELECT id FROM turns WHERE run_id = ? ORDER BY created_at DESC, rowid DESC LIMIT 1)",
    )
    .bind(&conv)
    .bind(&run.id)
    .fetch_one(app.db())
    .await
    .map_err(|_| "db error")?;
    if queued > 0 {
        return Err("a prompt is queued");
    }
    if last.as_deref() != Some(a.turn_id.as_str()) {
        return Err("a new turn was sent meanwhile");
    }
    Ok(())
}

/// 使用者中斷時立刻取消，避免共用的 interrupt hold 被其他流程清除。
pub fn cancel(bot_id: &str) {
    lock().armed.remove(bot_id);
}

/// [`WAIT`] 到了：條件都還成立就排續行提示。回 `true`＝排進去了（呼叫端持 bot 鎖）。
pub async fn check(app: &impl ResumeNudgeContext, bot_id: &str) -> bool {
    let Some(a) = lock().armed.remove(bot_id) else { return false };
    if let Err(why) = verdict(app, bot_id, &a).await {
        tracing::info!(bot = %bot_id, run = %a.run_id, why, "api error nudge cancelled");
        return false;
    }
    let Ok(conv) = db::conversation_id(app.db(), bot_id).await else { return false };
    let crid = format!("{CRID_PREFIX}{}", a.turn_id);
    match app.queue_nudge(&conv, bot_id, &nudge_text(&a.reason), &crid, crate::agent_relay::DAEMON_SENDER).await {
        Ok(out) => {
            tracing::info!(bot = %bot_id, run = %a.run_id, turn = %out.turn_id, "queued a continue nudge after a transient API error");
            app.schedule_queue_flush(bot_id);
            true
        }
        Err(e) => {
            tracing::info!(bot = %bot_id, run = %a.run_id, error = ?e, "api error nudge not queued");
            false
        }
    }
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    #[test]
    fn only_transient_errors_count() {
        for l in [
            "API Error: Connection lost mid-response. Please try again.",
            "API Error: Connection error.",
            "API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}",
            "API Error: Overloaded",
            "API Error: 500 Internal server error",
            "API Error: 502 Bad Gateway",
            "API Error: 503 Service Unavailable",
            "API Error: 504 Gateway Timeout",
            "API Error: Request timed out.",
            "API Error: read ECONNRESET",
            "API Error: timeout of 600000ms exceeded",
        ] {
            assert!(is_transient(l), "{l}");
        }
        for l in [
            "You've reached your Fable limit. Run /usage-credits to continue.",
            "You've hit your session limit · resets 3pm",
            "模型目前滿載，請換模型或稍後再試（Selected model is at capacity. Please try a different model.）",
            "OpenAI 資安過濾擋掉了這次回覆，請改寫請求、避開觸發的內容後重送（This content can’t be shown）",
            "API Error: 400 {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad\"}}",
            "API Error: 401 authentication_error",
            "API Error: 403 Forbidden",
            "API Error: 429 rate limit",
            "API Error: 404 not_found_error",
            "API Error: 15032 something",
            "",
        ] {
            assert!(!is_transient(l), "{l}");
        }
    }

    #[test]
    fn summary_is_trimmed() {
        assert_eq!(summary("⏺ API Error: Connection lost"), "API Error: Connection lost");
        assert_eq!(summary(&"x".repeat(200)).chars().count(), 80);
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;
    use crate::capabilities::Db;
    use crate::testing::{claude_bot, env, Env};

    const ERR: &str = "API Error: Connection lost mid-response. Please try again.";

    /// 一個 running／idle 的 run，釘著 `err`，最後一回合是 `crid`（failed）。回 (run, turn)。
    async fn cut(e: &Env, bot_id: &str, err: &str, crid: Option<&str>) -> (String, String) {
        let (run, turn, conv) = (db::ulid(), db::ulid(), db::conversation_id(e.app.db(), bot_id).await.unwrap());
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, started_at, turn_error) VALUES (?,?,'running','idle','2026-11-04T00:00:00Z',?)")
            .bind(&run)
            .bind(bot_id)
            .bind(err)
            .execute(e.app.db())
            .await
            .unwrap();
        add_turn(e, &conv, &run, &turn, "failed", crid).await;
        (run, turn)
    }

    async fn add_turn(e: &Env, conv: &str, run: &str, id: &str, status: &str, crid: Option<&str>) {
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at) VALUES (?,?,?,'web',?,'ok',?,?)")
            .bind(id)
            .bind(conv)
            .bind(run)
            .bind(status)
            .bind(crid)
            .bind(db::now())
            .execute(e.app.db())
            .await
            .unwrap();
    }

    async fn queued(e: &Env, bot_id: &str) -> Vec<(String, Option<String>)> {
        let conv = db::conversation_id(e.app.db(), bot_id).await.unwrap();
        sqlx::query_as(
            "SELECT m.content, m.relay_from FROM turns t JOIN messages m ON m.turn_id = t.id WHERE t.conversation_id = ? AND t.status = 'queued'",
        )
        .bind(&conv)
        .fetch_all(e.app.db())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_transient_cut_is_nudged_once_with_the_daemon_as_sender() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "ae-ok").await;
        let (run, turn) = cut(&e, &bot.id, ERR, None).await;
        arm(&e.app, &bot.id, &run).await;
        assert!(check(&e.app, &bot.id).await);
        let q = queued(&e, &bot.id).await;
        assert_eq!(q.len(), 1);
        assert!(q[0].0.contains("API 斷線截斷") && q[0].0.contains("Connection lost"), "{}", q[0].0);
        assert_eq!(q[0].1.as_deref(), Some("daemon"));
        let crid: String = sqlx::query_scalar("SELECT client_request_id FROM turns WHERE status='queued'")
            .fetch_one(e.app.db())
            .await
            .unwrap();
        assert_eq!(crid, format!("{CRID_PREFIX}{turn}"));
        // 同一個回合不再 arm。
        arm(&e.app, &bot.id, &run).await;
        assert!(!check(&e.app, &bot.id).await);
    }

    #[tokio::test]
    async fn non_transient_errors_are_not_armed() {
        let e = env().await;
        for (i, err) in ["API Error: 400 invalid_request_error", "You've reached your Fable limit. Run /usage-credits", "API Error: 403 Forbidden"].iter().enumerate() {
            let bot = claude_bot(&e.app, &e.project_id, &format!("ae-no-{i}")).await;
            let (run, _) = cut(&e, &bot.id, err, None).await;
            arm(&e.app, &bot.id, &run).await;
            assert!(!check(&e.app, &bot.id).await, "{err}");
            assert!(queued(&e, &bot.id).await.is_empty());
        }
    }

    #[tokio::test]
    async fn a_cut_nudge_is_not_nudged_again() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "ae-loop").await;
        let (run, _) = cut(&e, &bot.id, ERR, Some("api-error-nudge:prev")).await;
        arm(&e.app, &bot.id, &run).await;
        assert!(!check(&e.app, &bot.id).await);
        assert!(queued(&e, &bot.id).await.is_empty());
    }

    #[tokio::test]
    async fn activity_in_the_window_cancels() {
        let e = env().await;
        for case in ["working", "blocked", "new_turn", "queued", "in_flight", "interrupt", "cleared", "other_run"] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("ae-{case}")).await;
            let (run, _) = cut(&e, &bot.id, ERR, None).await;
            arm(&e.app, &bot.id, &run).await;
            let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
            match case {
                "working" | "blocked" => {
                    sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(case).bind(&run).execute(e.app.db()).await.unwrap();
                }
                "new_turn" => add_turn(&e, &conv, &run, &db::ulid(), "completed", None).await,
                "queued" => add_turn(&e, &conv, &run, &db::ulid(), "queued", None).await,
                "in_flight" => add_turn(&e, &conv, &run, &db::ulid(), "in_flight", None).await,
                "interrupt" => crate::lifecycle::interrupt_grace::note_user_interrupt(&bot.id),
                "cleared" => {
                    sqlx::query("UPDATE runs SET turn_error=NULL WHERE id=?").bind(&run).execute(e.app.db()).await.unwrap();
                }
                _ => {
                    sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run).execute(e.app.db()).await.unwrap();
                }
            }
            let before = queued(&e, &bot.id).await.len();
            assert!(!check(&e.app, &bot.id).await, "{case}");
            assert_eq!(queued(&e, &bot.id).await.len(), before, "{case}");
        }
    }
}
