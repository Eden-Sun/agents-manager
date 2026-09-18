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
//!
//! 同一個時刻另記一筆**被中斷的是哪一回合**（[`InterruptedTurn`]），給 `StopFailure` 認 Esc 的回聲用
//! （#117）。兩筆各自清：回聲到了只結清回聲那筆，排隊寬限照舊。

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

/// 排隊寬限用的接管標記（[`hold_at`] 讀它）。跟下面「等 Esc 的回聲」是兩件事，各自清。
pub(crate) fn hold_of(bot_id: &str) -> Option<DateTime<Utc>> {
    interrupt_holds().lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).copied()
}

/// 網頁 Esc／強制中止那一刻**被中斷的是哪一回合**（#117）。`StopFailure` 用它分辨「那次 Esc 的回聲」
/// 與「之後的新回合真的失敗了」（issue #79）。
///
/// 以前只記「這顆 bot 什麼時候按過停」，之後在窗口裡到的 `StopFailure` 一律當回聲：下一個新回合
/// 在窗口內撞額度也被吞掉（回合不收、#108 的撞限不記）。現在綁的是那一回合自己的證據，收到對應的
/// 回聲就結清；新回合的失敗對不上這些證據，照真的失敗收。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterruptedTurn {
    /// 被中斷的那一代。bot 換了 run 之後這筆就作廢——舊世代的事件由 [`super::fence`] 擋。
    pub run_id: String,
    /// 當下在飛的那一筆（沒有就是 `None`：閒著時按的 Esc）。
    pub turn_id: Option<String>,
    /// 當下這一代回報的 native session（`SessionStart` 會更新，`/clear` 之後也是新的那個）。
    pub session_id: Option<String>,
    /// claude 替那一則 prompt 編的 id：transcript 每一列的 `promptId`，跟 hook 的 `prompt_id` 是同一個值。
    /// 只有本機讀得到 transcript 時才有。
    pub prompt_id: Option<String>,
    /// daemon 記下這筆的時間（Esc 已經送出）。
    pub at: DateTime<Utc>,
}

/// 回聲最晚在 Esc 之後多久被**擷取**：Esc 落到 CLI、CLI 放棄請求、hook 當下蓋時間。遠端 `hook.sh`
/// 只蓋到秒（無條件捨去）、兩台時鐘也可能差一點，所以留幾秒。
///
/// 這**不是**上一版那種收到時間的窗口：比的是送端蓋在事件上的時間，spool 在遠端放多久都不影響它；
/// 而且只在 run／session／prompt id／回合都證不出歸屬時才輪到它。
pub(crate) const ECHO_CAPTURE_SLACK: Duration = Duration::from_secs(3);

/// bot → 還在等回聲的那一次中斷。同一顆 bot 只記最新的一次。
fn pending_echoes() -> &'static Mutex<HashMap<String, InterruptedTurn>> {
    static M: OnceLock<Mutex<HashMap<String, InterruptedTurn>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

pub(crate) fn expect_interrupt_echo(bot_id: &str, interrupted: InterruptedTurn) {
    pending_echoes().lock().unwrap_or_else(|e| e.into_inner()).insert(bot_id.to_string(), interrupted);
}

#[cfg(test)]
pub(crate) fn pending_echo(bot_id: &str) -> Option<InterruptedTurn> {
    pending_echoes().lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).cloned()
}

/// `interrupt_bot`／`abort_turns` 送完 Esc、收 in-flight **之前**呼叫：記排隊寬限，也記下被中斷的是哪一回合。
pub(crate) async fn note_user_interrupt_of(app: &Arc<App>, bot: &db::Bot, run: &db::Run, in_flight: Option<&db::Turn>) {
    note_user_interrupt(&bot.id);
    let prompt_id = claude_prompt_id(app, bot, run).await;
    expect_interrupt_echo(
        &bot.id,
        InterruptedTurn {
            run_id: run.id.clone(),
            turn_id: in_flight.map(|t| t.id.clone()),
            session_id: run.native_session_id.clone().filter(|s| !s.trim().is_empty()),
            prompt_id,
            at: Utc::now(),
        },
    );
}

/// 一則 `StopFailure` 自己說得出來的身分。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FailureEvidence<'a> {
    pub session_id: Option<&'a str>,
    pub prompt_id: Option<&'a str>,
    /// 送端蓋的時間（hook body 的 `received_at`）。
    pub stamped_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EchoVerdict {
    /// 是那次中斷的回聲：什麼都不動，並結清這筆。
    Echo(&'static str),
    /// 不是：照真的失敗處理，這筆留著（回聲可能還在路上）。
    NotEcho(&'static str),
    /// 這筆屬於已經換掉的 run：作廢。
    Superseded,
}

/// 純函式：這則 `StopFailure` 是不是那次中斷的回聲。證據依強弱排：
/// 1. run：不是被中斷的那一代就不是（bot 重啟過）。
/// 2. session：兩邊都有且不同就不是。
/// 3. prompt id：兩邊都有時一錘定音——同一則 prompt 才是回聲，新回合的 prompt id 一定不同。
/// 4. 回合：此刻在飛的就是被中斷的那一筆（`fail_in_flight` 沒收掉它）→ 這則只可能在講它。
/// 5. 擷取時間：前面都證不出來時，只有**在 Esc 當下被擷取**、而且早於此刻在飛的新回合開始的，才算回聲。
///    沒有時間的一律不算——舊回合的標記不可以蓋掉新回合的失敗。
pub(crate) fn echo_verdict(
    pending: &InterruptedTurn,
    run_id: &str,
    ev: &FailureEvidence,
    in_flight: Option<(&str, Option<DateTime<Utc>>)>,
) -> EchoVerdict {
    fn given(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|v| !v.is_empty())
    }
    if pending.run_id != run_id {
        return EchoVerdict::Superseded;
    }
    if let (Some(a), Some(b)) = (given(pending.session_id.as_deref()), given(ev.session_id)) {
        if a != b {
            return EchoVerdict::NotEcho("session 不是被中斷的那一段");
        }
    }
    if let (Some(a), Some(b)) = (given(pending.prompt_id.as_deref()), given(ev.prompt_id)) {
        return if a == b { EchoVerdict::Echo("同一則 prompt") } else { EchoVerdict::NotEcho("prompt id 不是被中斷的那一則") };
    }
    if let (Some((current, _)), Some(interrupted)) = (in_flight, pending.turn_id.as_deref()) {
        if current == interrupted {
            return EchoVerdict::Echo("在飛的就是被中斷的那一筆");
        }
    }
    let Some(stamped) = ev.stamped_at else {
        return EchoVerdict::NotEcho("證不出是被中斷的那一回合");
    };
    let slack = chrono::Duration::from_std(ECHO_CAPTURE_SLACK).unwrap_or_default();
    if stamped > pending.at + slack {
        return EchoVerdict::NotEcho("在 Esc 之後才擷取");
    }
    // 此刻在飛的是 Esc 之後才開的回合：擷取時間不早於它開始，就是它自己的事。
    if let Some((_, Some(began))) = in_flight {
        if began > pending.at && stamped >= began {
            return EchoVerdict::NotEcho("在新回合開始之後才擷取");
        }
    }
    EchoVerdict::Echo("在 Esc 當下擷取")
}

/// `StopFailure` 問這一支：是那次中斷的回聲就結清標記並回 `true`（呼叫端什麼都不動）。
pub(crate) fn settle_interrupt_echo(bot_id: &str, run_id: &str, ev: &FailureEvidence, in_flight: Option<&db::Turn>) -> bool {
    let mut m = pending_echoes().lock().unwrap_or_else(|e| e.into_inner());
    let Some(pending) = m.get(bot_id) else { return false };
    let began = in_flight.and_then(|t| DateTime::parse_from_rfc3339(&t.created_at).ok()).map(|t| t.with_timezone(&Utc));
    let verdict = echo_verdict(pending, run_id, ev, in_flight.map(|t| (t.id.as_str(), began)));
    tracing::info!(bot = bot_id, ?verdict, interrupted = ?pending.turn_id, "StopFailure 對上 Esc 標記");
    match verdict {
        EchoVerdict::Echo(_) => {
            m.remove(bot_id);
            true
        }
        EchoVerdict::Superseded => {
            m.remove(bot_id);
            false
        }
        EchoVerdict::NotEcho(_) => false,
    }
}

/// claude transcript 尾端最後一個 `promptId`：Esc 當下那就是被中斷的那一則 prompt（中斷標記那一列
/// 也帶同一個 id）。subagent 的 sidechain 列不算。
pub(crate) fn claude_last_prompt_id(log: &str) -> Option<String> {
    log.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v.get("isSidechain").and_then(Value::as_bool) != Some(true))
        .find_map(|v| v.get("promptId").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty()).map(String::from))
}

async fn claude_prompt_id(app: &Arc<App>, bot: &db::Bot, run: &db::Run) -> Option<String> {
    if bot.kind != "claude" || db::bot_host(&app.db, &bot.id).await.ok()? != LOCAL_HOST {
        return None;
    }
    let path = std::path::PathBuf::from(run.transcript_path.as_deref().filter(|p| !p.trim().is_empty())?);
    let log = tokio::task::spawn_blocking(move || read_tail(&path)).await.ok()??;
    claude_last_prompt_id(&log)
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

    /// 接管標記是「剛按了停」，不是「這顆 bot 按過停」。以前標記只有在排隊的派工問 `hold()` 時才清：
    /// 使用者按一次 Esc、之後自己繼續用這顆（沒有派工），標記就一直留著，之後真的 `StopFailure`（撞額度、
    /// API 錯誤）全被當成那次 Esc 的回聲丟掉——回合不收、撞限也不記（#79／#108 的收尾都失效）。
    #[tokio::test]
    async fn a_stop_failure_long_after_an_interrupt_is_a_real_failure() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "esc-then-work").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        // 十分鐘前閒著的時候按過一次 Esc（回聲一直沒來）；之後使用者自己送了一則，現在在跑。
        let at = Utc::now() - chrono::Duration::minutes(10);
        note_user_interrupt_at(&bot.id, at);
        expect_interrupt_echo(&bot.id, InterruptedTurn { run_id: run.clone(), turn_id: None, session_id: None, prompt_id: None, at });
        let turn = open_turn(&app, &conv, &run).await;

        let mut ev = stop_failure(&bot.id, "p-esc", Some(QUOTA), Utc::now());
        // 沒有擷取時間的（手寫、舊版送端）一樣證不出是那次 Esc 的回聲。
        ev.received_at = None;
        crate::hookrecv::process(&app, &ev).await.unwrap();

        assert_eq!(status_of(&app, &turn).await, "failed", "十分鐘前的 Esc 不是這則 StopFailure 的來源");
    }

    /// 回聲的判準是**被中斷的那一回合**的證據，不是「這顆 bot 最近按過停」（#117 重開）。
    #[test]
    fn the_echo_is_recognised_by_the_interrupted_turn_not_by_the_clock() {
        use EchoVerdict::*;
        let t0 = Utc::now();
        let s = chrono::Duration::seconds;
        let pending = |prompt: Option<&str>, turn: Option<&str>| InterruptedTurn {
            run_id: "run-1".into(),
            turn_id: turn.map(String::from),
            session_id: Some("s-1".into()),
            prompt_id: prompt.map(String::from),
            at: t0,
        };
        let ev = |session: Option<&'static str>, prompt: Option<&'static str>, stamped: Option<DateTime<Utc>>| FailureEvidence {
            session_id: session,
            prompt_id: prompt,
            stamped_at: stamped,
        };
        let is_echo = |v: EchoVerdict| matches!(v, Echo(_));

        // run：bot 換了 run，舊的那筆作廢。
        assert_eq!(echo_verdict(&pending(None, None), "run-2", &ev(None, None, Some(t0)), None), Superseded);
        // session 對不上就不是，時間再近都一樣。
        assert!(!is_echo(echo_verdict(&pending(None, None), "run-1", &ev(Some("s-2"), None, Some(t0)), None)));
        // prompt id 兩邊都有時一錘定音：同一則就是回聲，擷取得再晚都一樣（遠端 spool 放多久都不影響）；
        // 不同則就不是，即使擷取時間落在 Esc 當下。
        assert!(is_echo(echo_verdict(&pending(Some("p-a"), None), "run-1", &ev(Some("s-1"), Some("p-a"), Some(t0 + s(600))), Some(("b", Some(t0 + s(20)))))));
        assert!(!is_echo(echo_verdict(&pending(Some("p-a"), None), "run-1", &ev(Some("s-1"), Some("p-b"), Some(t0)), None)));
        // 此刻在飛的就是被中斷的那一筆：只可能在講它。
        assert!(is_echo(echo_verdict(&pending(None, Some("a")), "run-1", &ev(None, Some("p-x"), None), Some(("a", Some(t0 - s(30)))))));
        // 證不出來時看擷取時間：Esc 當下擷取的才是回聲（遠端只蓋到秒、往下捨去也算）。
        assert!(is_echo(echo_verdict(&pending(None, Some("a")), "run-1", &ev(None, Some("p-a"), Some(t0 - s(1))), None)));
        assert!(is_echo(echo_verdict(&pending(None, Some("a")), "run-1", &ev(None, Some("p-a"), Some(t0 + s(1))), Some(("b", Some(t0 + s(20)))))));
        // 新回合 B 在 Esc 之後 40 秒撞額度——以前在 120 秒窗口內會被吞掉。
        assert!(!is_echo(echo_verdict(&pending(None, Some("a")), "run-1", &ev(None, Some("p-b"), Some(t0 + s(40))), Some(("b", Some(t0 + s(20)))))));
        // B 是 Esc 之前就排好的派工（`created_at` 比 Esc 早）：沒有「新回合開始」可比，Esc 之後 40 秒才擷取的就不是回聲。
        assert!(!is_echo(echo_verdict(&pending(None, Some("a")), "run-1", &ev(None, Some("p-b"), Some(t0 + s(40))), Some(("b", Some(t0 - s(60)))))));
        // 就算 B 快到在寬容的那幾秒內開始又失敗：擷取在 B 開始之後，就是 B 自己的事。
        assert!(!is_echo(echo_verdict(&pending(None, Some("a")), "run-1", &ev(None, Some("p-b"), Some(t0 + s(2))), Some(("b", Some(t0 + s(1)))))));
        // 沒有擷取時間：證不出來，不算。
        assert!(!is_echo(echo_verdict(&pending(None, Some("a")), "run-1", &ev(None, Some("p-b"), None), Some(("b", Some(t0 + s(1)))))));
    }

    async fn open_turn(app: &Arc<App>, conv: &str, run: &str) -> String {
        open_turn_at(app, conv, run, Utc::now()).await
    }

    async fn open_turn_at(app: &Arc<App>, conv: &str, run: &str, at: DateTime<Utc>) -> String {
        let id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&id)
            .bind(conv)
            .bind(run)
            .bind(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    async fn status_of(app: &Arc<App>, turn: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap()
    }

    /// 一則 claude `StopFailure`，`stamped` 是送端蓋的時間（`hook_cmd`／`hook.sh` 在事件發生當下蓋）。
    fn stop_failure(bot_id: &str, prompt_id: &str, error: Option<&str>, stamped: DateTime<Utc>) -> crate::hookrecv::HookBody {
        let mut payload = json!({"hook_event_name": "StopFailure", "session_id": "s-esc", "prompt_id": prompt_id});
        if let Some(e) = error {
            payload["error"] = json!(e);
        }
        crate::hookrecv::HookBody {
            bot_id: bot_id.to_string(),
            provider: "claude".into(),
            payload,
            received_at: Some(stamped.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            truncated: false,
            run_id: None,
        }
    }

    const QUOTA: &str = "You've hit your session limit · resets 3pm (Asia/Taipei)";

    /// #117 重開留言的時序：A 被網頁 Esc 中斷 → A 的回聲到了 → 馬上開 B → B 在 120 秒內真的撞額度。
    /// Esc 的標記只屬於 A：B 必須收成 failed，撞限也要記下來（#108 的閘靠它擋住下一則派工）。
    #[tokio::test]
    async fn a_new_turn_that_hits_the_limit_right_after_an_escaped_one_is_a_real_failure() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "esc-then-limit").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let a = open_turn(&app, &conv, &run).await;
        let t0 = Utc::now();

        interrupt_bot(&app, &bot.id).await.unwrap();
        assert_eq!(status_of(&app, &a).await, "failed", "A 由 Esc 收掉");
        // A 的回聲：說不出原因，Esc 那一刻就蓋了時間。
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-a", None, t0)).await.unwrap();

        // 馬上開 B；B 在 Esc 之後 40 秒撞額度（還在舊的 120 秒窗口內）。
        let b = open_turn(&app, &conv, &run).await;
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-b", Some(QUOTA), t0 + chrono::Duration::seconds(40))).await.unwrap();

        assert_eq!(status_of(&app, &b).await, "failed", "A 的 Esc 不是 B 這則 StopFailure 的來源");
        assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "B 撞的額度要記下來");
    }

    /// 遠端 hook 走 spool，回聲可能在 B 已經開始之後才到（30 秒以上）。認它靠的是它**在 Esc 當下被擷取**，
    /// 不是放大收到時間的窗口：B 不被它收掉，之後 B 自己的撞額度照樣收。回聲結清的是回聲那筆，
    /// 排隊寬限的接管標記不受影響（那是讓使用者拿回輸入框，兩件事）。
    #[tokio::test]
    async fn an_echo_delayed_past_the_start_of_the_next_turn_is_still_recognised_by_when_it_was_captured() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "esc-remote-echo").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let a = open_turn(&app, &conv, &run).await;

        interrupt_bot(&app, &bot.id).await.unwrap();
        let pending = pending_echo(&bot.id).expect("記下了被中斷的那一回合");
        assert_eq!((pending.run_id.as_str(), pending.turn_id.as_deref()), (run.as_str(), Some(a.as_str())));
        let t0 = pending.at;
        let b = open_turn_at(&app, &conv, &run, t0 + chrono::Duration::seconds(20)).await;

        // Esc 當下擷取、45 秒後才被撈回來的回聲（處理時間不參與判斷）。
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-a", None, t0 + chrono::Duration::milliseconds(300))).await.unwrap();
        assert_eq!(status_of(&app, &b).await, "in_flight", "A 的回聲不收 B");
        assert_eq!(pending_echo(&bot.id), None, "回聲到了就結清");
        assert!(hold_of(&bot.id).is_some(), "排隊寬限的標記另外算，不被回聲收掉");

        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-b", Some(QUOTA), t0 + chrono::Duration::seconds(40))).await.unwrap();
        assert_eq!(status_of(&app, &b).await, "failed");
        assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_some());
    }

    /// 本機 claude 讀得到 transcript：記下被中斷那一則 prompt 的 id（hook 的 `prompt_id` 就是它），
    /// 之後認回聲只看它——擷取時間再晚也是回聲，擷取時間再早、prompt 不同就不是。
    #[tokio::test]
    async fn the_interrupted_prompt_id_decides_what_is_an_echo_when_the_transcript_is_readable() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "esc-prompt-id").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let transcript = app.data_dir.join(format!("{}.jsonl", bot.id));
        let lines = [
            json!({"type": "user", "promptId": "p-old", "message": {"role": "user", "content": "上一則"}}),
            json!({"type": "user", "promptId": "p-a", "message": {"role": "user", "content": "跑測試"}}),
            json!({"type": "assistant", "promptId": "p-a", "message": {"content": [{"type": "text", "text": "跑…"}]}}),
            json!({"type": "user", "isSidechain": true, "promptId": "p-sub", "message": {"role": "user", "content": "subagent"}}),
        ];
        std::fs::write(&transcript, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?").bind(transcript.to_string_lossy().to_string()).bind(&run).execute(&app.db).await.unwrap();
        open_turn(&app, &conv, &run).await;

        interrupt_bot(&app, &bot.id).await.unwrap();
        let t0 = pending_echo(&bot.id).expect("記下了").at;
        assert_eq!(pending_echo(&bot.id).unwrap().prompt_id.as_deref(), Some("p-a"), "subagent 的 sidechain 不算");
        let b = open_turn_at(&app, &conv, &run, t0 + chrono::Duration::seconds(20)).await;

        // B 的失敗擷取時間落在 Esc 那幾秒內（時鐘歪了之類），但 prompt 不是被中斷的那一則。
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-b", Some("API Error: 500"), t0 + chrono::Duration::seconds(1))).await.unwrap();
        assert_eq!(status_of(&app, &b).await, "failed", "prompt id 對不上就不是回聲");
        assert!(pending_echo(&bot.id).is_some(), "不是回聲的不結清：A 的回聲可能還在路上");

        let c = open_turn_at(&app, &conv, &run, t0 + chrono::Duration::seconds(60)).await;
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-a", None, t0 + chrono::Duration::seconds(600))).await.unwrap();
        assert_eq!(status_of(&app, &c).await, "in_flight", "同一則 prompt 的回聲，擷取再晚也不收新回合");
        assert_eq!(pending_echo(&bot.id), None);
    }

    /// 回聲一直沒來（claude 對 Esc 不一定送 `StopFailure`），使用者馬上開了 B、B 立刻失敗：
    /// 就算落在擷取時間寬容的那幾秒內，擷取在 B 開始之後就是 B 的。強制中止走的是同一條。
    #[tokio::test]
    async fn an_old_interrupt_never_swallows_the_failure_of_a_turn_started_after_it() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "abort-then-b").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let a = open_turn(&app, &conv, &run).await;

        abort_turns(&app, &bot.id).await.unwrap();
        assert_eq!(status_of(&app, &a).await, "failed");
        let t0 = pending_echo(&bot.id).expect("強制中止也記下被中斷的那一回合").at;
        let b = open_turn_at(&app, &conv, &run, t0 + chrono::Duration::milliseconds(800)).await;
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-b", Some(QUOTA), t0 + chrono::Duration::milliseconds(1500))).await.unwrap();

        assert_eq!(status_of(&app, &b).await, "failed");
        assert!(crate::quota::limit_hit_for_bot(&app, &bot).await.is_some());
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
