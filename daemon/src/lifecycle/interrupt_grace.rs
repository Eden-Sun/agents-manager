//! 使用者按 Esc 中斷之後，排著的 AGM 派工先讓使用者拿回輸入框（AGM 裁示 2026-09-16；2b0fe98 起頭、本檔補齊到規格）。
//!
//! 使用者按 Esc 多半是要親手接管、馬上打字；這時排在後面的派工立刻 flush 打進去，等於跟使用者搶輸入框。
//! 規則（SPEC §4.4a）：
//! - 最近一個回合是**被使用者中斷**結束的：這顆 bot 的 queued 不立刻送，等它**連續 idle 滿寬限**
//!   （預設 [`INTERRUPT_GRACE_SECS`] 秒，[`INTERRUPT_GRACE_ENV`] 可調，看不懂／0／負數回預設）才送；中間 working 過就重算。
//! - 寬限內使用者有新輸入（網頁送 prompt、在 pane 裡打字送出，都會開一筆 turn）：不再替 AGM 擋——
//!   使用者那則在跑的時候 queued 本來就排在後面，那一回合結束後照一般規則馬上 flush。
//! - 一般回合結束不受影響。**不撤** queued（「abort 不動 queued」的裁示不變），只是晚一點送。
//!   接管最久算 [`INTERRUPT_HOLD_MAX`]，之後回到一般排隊（不讓一次 Esc 永遠壓著佇列）。
//!
//! 怎麼知道是中斷：網頁的 Esc（`interrupt_bot`）與強制中止（`abort_turns`）當下就記一筆（CLI 寫 transcript
//! 比收回合觸發的 flush 慢，只靠 log 會被搶先）；使用者直接在 pane 裡按 Esc，daemon 沒有任何事件，讀 transcript／rollout
//! 的最後一個回合邊界：claude 是帶 `interruptedMessageId` 的 `[Request interrupted by user]`，codex 是
//! `event_msg`／`turn_aborted`（`reason: interrupted`）。
//!
//! idle 計時跟 [`super::stuck_turns`] 共用同一份來源（每個狀態事件都記、閃一下 working 就重算），
//! 但語意分開：那邊是「收尾卡住的 in_flight」，這邊是「讓使用者先拿回輸入框」。

use super::stuck_turns::{idle_for, observe_at};
use super::*;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub const INTERRUPT_GRACE_SECS: u64 = 60;
pub const INTERRUPT_GRACE_ENV: &str = "AM_INTERRUPT_FLUSH_GRACE_SECS";
/// 接管標記（或 log 裡的中斷）最久算這麼久：之後就算一直有人在講話，也回到一般的排隊行為。
pub(crate) const INTERRUPT_HOLD_MAX: Duration = Duration::from_secs(30 * 60);
/// transcript／rollout 只讀尾端這麼多：中斷一定是最近的事。
const LOG_TAIL_BYTES: u64 = 1024 * 1024;

/// 看不懂、0、負數一律回預設——一個手滑的值不該讓 Esc 之後的派工立刻搶進去（或永遠不送）。
pub(crate) fn parse_grace_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok()).filter(|s| *s > 0).map(|s| s as u64).unwrap_or(INTERRUPT_GRACE_SECS)
}

pub fn interrupt_grace() -> Duration {
    Duration::from_secs(parse_grace_secs(std::env::var(INTERRUPT_GRACE_ENV).ok().as_deref()))
}

/// bot → 使用者接管的時刻。只在記憶體：daemon 重啟就當沒有接管（log 裡的中斷照樣認得）。
fn interrupt_holds() -> &'static Mutex<HashMap<String, DateTime<Utc>>> {
    static M: OnceLock<Mutex<HashMap<String, DateTime<Utc>>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 使用者剛從網頁按了 interrupt／強制中止：從現在起，這顆 bot 排著的派工要等它連續 idle 滿寬限才送。
pub fn note_user_interrupt(bot_id: &str) {
    note_user_interrupt_at(bot_id, Utc::now());
}

pub(crate) fn note_user_interrupt_at(bot_id: &str, at: DateTime<Utc>) {
    interrupt_holds().lock().unwrap_or_else(|e| e.into_inner()).insert(bot_id.to_string(), at);
}

/// 測試用：這顆 bot 現在還有沒有接管標記。
#[cfg(test)]
pub(crate) fn is_held(bot_id: &str) -> bool {
    hold_of(bot_id).is_some()
}

fn hold_of(bot_id: &str) -> Option<DateTime<Utc>> {
    interrupt_holds().lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).copied()
}

/// 只收掉同一次接管的標記：等的這段時間裡使用者又按了一次，就留給新的那次。
fn clear_hold(bot_id: &str, at: DateTime<Utc>) {
    let mut m = interrupt_holds().lock().unwrap_or_else(|e| e.into_inner());
    if m.get(bot_id).is_some_and(|x| *x <= at) {
        m.remove(bot_id);
    }
}

/// 還要再等多久（`None`＝不用等）。純函式：
/// - 沒有中斷、中斷超過 [`INTERRUPT_HOLD_MAX`]、或中斷之後使用者有新輸入 → 不等（使用者那一回合結束後照一般規則送）。
/// - 否則看**連續 idle** 多久，且不早於中斷本身；滿寬限才送。
pub(crate) fn interrupt_grace_remaining(
    interrupted_at: Option<DateTime<Utc>>,
    user_input_since: bool,
    idle: Duration,
    now: DateTime<Utc>,
    grace: Duration,
) -> Option<Duration> {
    let at = interrupted_at?;
    let since = (now - at).to_std().unwrap_or_default();
    if since >= INTERRUPT_HOLD_MAX || user_input_since {
        return None;
    }
    grace.checked_sub(idle.min(since)).filter(|left| !left.is_zero())
}

fn ts(v: &Value) -> Option<DateTime<Utc>> {
    v.get("timestamp").and_then(Value::as_str).and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc))
}

/// claude transcript 的最後一個回合邊界是不是使用者中斷；是的話回那一行的時間。
/// 邊界：使用者 prompt、`stop_reason: end_turn` 的 assistant、中斷標記。中斷之後又有 prompt 或回完一回合都不算。
pub(crate) fn claude_interrupted_at(log: &str) -> Option<DateTime<Utc>> {
    let mut last: Option<Option<DateTime<Utc>>> = None;
    for line in log.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                let marker = v.get("interruptedMessageId").is_some()
                    || v.pointer("/message/content").and_then(Value::as_array).is_some_and(|parts| {
                        parts.iter().any(|p| p.get("text").and_then(Value::as_str).is_some_and(|t| t.starts_with("[Request interrupted by user")))
                    });
                if marker {
                    last = Some(ts(&v));
                } else if transcript_user_text(line).is_some() {
                    last = Some(None);
                }
            }
            Some("assistant") if v.pointer("/message/stop_reason").and_then(Value::as_str) == Some("end_turn") => last = Some(None),
            _ => {}
        }
    }
    last.flatten()
}

/// codex rollout 的最後一個回合邊界是不是使用者中斷（`turn_aborted`／`interrupted`）。
pub(crate) fn codex_interrupted_at(log: &str) -> Option<DateTime<Utc>> {
    let mut last: Option<Option<DateTime<Utc>>> = None;
    for line in log.lines() {
        if codex_user_text(line).is_some() {
            last = Some(None);
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        match v.pointer("/payload/type").and_then(Value::as_str) {
            Some("task_complete") => last = Some(None),
            Some("turn_aborted") if v.pointer("/payload/reason").and_then(Value::as_str) == Some("interrupted") => last = Some(ts(&v)),
            _ => {}
        }
    }
    last.flatten()
}

fn read_tail(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// 使用者直接在 pane 裡按 Esc：讀這顆 bot 的 log。本機 claude／codex 才讀得到。
async fn log_interrupted_at(app: &Arc<App>, bot: &db::Bot, run: &db::Run) -> Option<DateTime<Utc>> {
    if db::bot_host(&app.db, &bot.id).await.ok()? != LOCAL_HOST {
        return None;
    }
    match bot.kind.as_str() {
        "claude" => {
            let path = std::path::PathBuf::from(run.transcript_path.as_deref().filter(|p| !p.trim().is_empty())?);
            let log = tokio::task::spawn_blocking(move || read_tail(&path)).await.ok()??;
            claude_interrupted_at(&log)
        }
        "codex" => {
            let home = codex_home(app, bot).await?;
            let session = run.native_session_id.clone().filter(|s| !s.trim().is_empty())?;
            let log = tokio::task::spawn_blocking(move || read_tail(&codex_session_log(&home, &session)?)).await.ok()??;
            codex_interrupted_at(&log)
        }
        _ => None,
    }
}

/// 送出前問一次：要不要先讓使用者拿回輸入框。`Some(還要等多久)`＝先不送；`None`＝照一般規則。
pub(crate) async fn hold(app: &Arc<App>, bot: &db::Bot, run: &db::Run, conv: &str) -> Option<Duration> {
    hold_at(app, bot, run, conv, Instant::now(), Utc::now(), interrupt_grace()).await
}

pub(crate) async fn hold_at(
    app: &Arc<App>,
    bot: &db::Bot,
    run: &db::Run,
    conv: &str,
    now: Instant,
    now_utc: DateTime<Utc>,
    grace: Duration,
) -> Option<Duration> {
    let noted = hold_of(&bot.id);
    let logged = log_interrupted_at(app, bot, run).await;
    let interrupted_at = match (noted, logged) {
        (Some(a), Some(b)) => a.max(b),
        (a, b) => a.or(b)?,
    };
    // 中斷之後使用者有新輸入（網頁送的、在 pane 裡打字送出的，都會開一筆 turn）＝使用者已經拿回輸入框。
    // AGM 自己排進去的 queued 不算。
    let newer: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM turns WHERE conversation_id = ? AND status <> 'queued' AND created_at > ?)",
    )
    .bind(conv)
    .bind(interrupted_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    .fetch_one(&app.db)
    .await
    .unwrap_or(false);
    // 連續 idle 多久：跟 stuck_turns 同一份計時（還沒記過就從現在起算）。
    observe_at(&run.id, &run.agent_status, now);
    let idle = idle_for(&run.id, now).unwrap_or_default();
    let left = interrupt_grace_remaining(Some(interrupted_at), newer, idle, now_utc, grace);
    if left.is_none() {
        if let Some(at) = noted {
            clear_hold(&bot.id, at);
        }
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2b0fe98 的 `a_broken_grace_env_falls_back_to_sixty_seconds` 改成測解析函式：原本在測試裡 `set_var`，
    /// 寬限現在有更多平行測試會讀，全域環境變數會互相干擾。
    #[test]
    fn a_broken_grace_env_falls_back_to_sixty_seconds() {
        assert_eq!(INTERRUPT_GRACE_ENV, "AM_INTERRUPT_FLUSH_GRACE_SECS", "規格指定的名字");
        assert_eq!(parse_grace_secs(None), 60);
        assert_eq!(parse_grace_secs(Some("90")), 90);
        assert_eq!(parse_grace_secs(Some(" 15 ")), 15);
        for bad in ["0", "-3", "-5", "soon", "abc", "", "1.5"] {
            assert_eq!(parse_grace_secs(Some(bad)), 60, "{bad:?}");
        }
    }

    fn user(text: &str) -> String {
        json!({"type": "user", "timestamp": "2026-09-16T12:00:00.000Z", "message": {"role": "user", "content": text}}).to_string()
    }
    fn interrupted(at: &str) -> String {
        json!({"type": "user", "timestamp": at, "interruptedMessageId": "msg_1",
               "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}})
        .to_string()
    }
    fn end_turn() -> String {
        json!({"type": "assistant", "message": {"stop_reason": "end_turn", "content": [{"type": "text", "text": "好了"}]}}).to_string()
    }

    #[test]
    fn a_claude_turn_ended_by_the_user_is_recognised_only_while_it_is_the_last_boundary() {
        let at = "2026-09-16T12:01:00.000Z";
        let log = [user("跑測試"), interrupted(at)].join("\n");
        assert_eq!(claude_interrupted_at(&log).map(|t| t.to_rfc3339()), Some("2026-09-16T12:01:00+00:00".into()));
        // 工具執行中被按掉的那種寫法（沒有 interruptedMessageId）。
        let tool = json!({"type": "user", "timestamp": at, "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]}}).to_string();
        assert!(claude_interrupted_at(&[user("跑測試"), tool].join("\n")).is_some());
        assert_eq!(claude_interrupted_at(&[user("跑測試"), interrupted(at), user("我自己來")].join("\n")), None, "中斷後使用者又送了一則");
        assert_eq!(claude_interrupted_at(&[user("跑測試"), interrupted(at), end_turn()].join("\n")), None, "之後回完了一回合");
        assert_eq!(claude_interrupted_at(&[user("跑測試"), end_turn()].join("\n")), None, "一般結束");
    }

    #[test]
    fn a_codex_turn_aborted_by_the_user_is_recognised() {
        let aborted = |reason: &str| json!({"timestamp": "2026-09-16T12:01:00.000Z", "type": "event_msg", "payload": {"type": "turn_aborted", "reason": reason}}).to_string();
        let prompt = json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "跑測試"}]}}).to_string();
        let done = json!({"type": "event_msg", "payload": {"type": "task_complete", "last_agent_message": "好了"}}).to_string();
        assert!(codex_interrupted_at(&[prompt.clone(), aborted("interrupted")].join("\n")).is_some());
        assert_eq!(codex_interrupted_at(&[prompt.clone(), aborted("replaced")].join("\n")), None, "不是使用者中斷");
        assert_eq!(codex_interrupted_at(&[aborted("interrupted"), prompt.clone()].join("\n")), None);
        assert_eq!(codex_interrupted_at(&[aborted("interrupted"), done].join("\n")), None);
    }
}
