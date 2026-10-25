//! 冷快取的額度警告只選 Resume，不建立新 session，也不代替送達證據。

use super::*;
use crate::capabilities::{Db, HerdrRoutes};
use crate::hosts::HostsAccess;
use crate::tui_prompts::{cold_resume_choice, ResumeChoice};

pub const PENDING: &str = "cold_resume_pending";
const SETTLE: Duration = Duration::from_millis(250);

/// 每顆鍵之前重查 run、host 世代、pane session 與完整畫面。呼叫端持 bot 鎖。
fn current<'a>(
    app: &'a (impl Db + HerdrRoutes + HostsAccess),
    run: &'a db::Run,
    client: &'a RunClient,
) -> impl std::future::Future<Output = anyhow::Result<bool>> + Send + 'a {
    Box::pin(current_body(app, run, client))
}

async fn current_body(
    app: &(impl Db + HerdrRoutes + HostsAccess),
    run: &db::Run,
    client: &RunClient,
) -> anyhow::Result<bool> {
    if !matches!(db::bot(app.db(), &run.bot_id).await?, Some(b) if b.kind == "claude") {
        return Ok(false);
    }
    if !app.hosts().is_current(&client.fence).await {
        return Ok(false);
    }
    let Some(now) = db::active_run(app.db(), &run.bot_id).await? else {
        return Ok(false);
    };
    if now.id != run.id
        || !matches!(now.state.as_str(), "starting" | "running")
        || now.pane_id != run.pane_id
        || now.herdr_session != run.herdr_session
        || db::bot_host(app.db(), &run.bot_id).await? != client.fence.conn().name
    {
        return Ok(false);
    }
    let expected = run
        .native_session_id
        .as_deref()
        .or(run.resume_session_id.as_deref());
    if let (Some(expected), Some(actual)) = (expected, now.native_session_id.as_deref()) {
        if expected != actual {
            return Ok(false);
        }
    }
    if let (Some(expected), Some(agent)) = (
        expected,
        client
            .agent_get(run.pane_id.as_deref().unwrap_or(""))
            .await?,
    ) {
        if let Some(actual) = super::dead_panes::herdr_session_id(&agent) {
            if expected != actual {
                return Ok(false);
            }
        }
    }
    if super::dead_panes::pane_target_stale(client, &now).await {
        return Ok(false);
    }
    Ok(app.hosts().is_current(&client.fence).await)
}

/// `true` 只代表警告已關閉；原訊息仍由 delivery 的 transcript/session fence 證明。
/// 只在游標已停在 Resume 時按一次 Enter；不移動游標。畫面改變或權威失效時不盲按、不重貼訊息。
pub fn accept_if_shown<'a>(
    app: &'a (impl Db + HerdrRoutes + HostsAccess),
    run: &'a db::Run,
) -> impl std::future::Future<Output = anyhow::Result<bool>> + Send + 'a {
    Box::pin(accept_if_shown_body(app, run))
}

async fn accept_if_shown_body(
    app: &(impl Db + HerdrRoutes + HostsAccess),
    run: &db::Run,
) -> anyhow::Result<bool> {
    let client = match super::client_for_run(app, run).await {
        Ok(client) => client,
        Err(e) => {
            pending_runs().lock().unwrap_or_else(|e| e.into_inner()).clear_run(&run.id);
            return Err(anyhow::anyhow!("{e:?}"));
        }
    };
    accept_on(app, run, &client).await
}

pub fn accept_on<'a>(
    app: &'a (impl Db + HerdrRoutes + HostsAccess),
    run: &'a db::Run,
    client: &'a RunClient,
) -> impl std::future::Future<Output = anyhow::Result<bool>> + Send + 'a {
    Box::pin(accept_on_body(app, run, client))
}

/// 放棄的原因（固定字串）。呼叫端把 Err 吞掉，這裡是唯一會留下紀錄的地方。
const NO_PANE: &str = "no_pane";
const READ_UNTRUSTED: &str = "read_not_visible_or_truncated";
const CURSOR_NOT_ON_RESUME: &str = "cursor_not_on_resume";
const AUTHORITY_CHANGED: &str = "run_host_session_or_pane_changed";
const SCREEN_CHANGED: &str = "screen_changed_between_reads";
const STILL_SHOWN: &str = "menu_still_shown_after_enter";

/// 每組 (run, 原因) 第一次警告的時間：同一組不重複寫 WARN（排隊的 prompt 每次重試都會走到這裡；原因交替 A→B→A 也只各寫一次）。
/// 上限 [`GIVE_UP_CAP`] 組，滿了淘汰插入時間最舊的一組；超過 [`GIVE_UP_TTL`] 的當作不存在並刪掉。
/// 不為了清理去查 DB：結束的 run 留在表裡無妨，會被 TTL 或上限擠掉。
const GIVE_UP_CAP: usize = 256;
const GIVE_UP_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// 60 秒約等於 watcher 每 10 秒一次、連續 6 次仍看到冷選單；避免短暫重繪或換 run 的 pending 噪音。
pub(crate) const PENDING_ALERT_AFTER: Duration = Duration::from_secs(60);
/// watcher 每 10 秒巡檢；缺席超過 3 個週期就視為觀測中斷，從下次看到選單重新計時。
pub(crate) const PENDING_OBSERVATION_GAP: Duration = Duration::from_secs(30);

struct PendingRun {
    bot_id: String,
    since: std::time::Instant,
    last_seen: std::time::Instant,
    alerted: bool,
}

#[derive(Default)]
pub(crate) struct PendingRuns(std::collections::HashMap<String, PendingRun>);

impl PendingRuns {
    /// 記下仍可見的冷選單；同一 bot 出現新 run 時清掉舊 run 的計時。
    pub(crate) fn observe(&mut self, run_id: &str, bot_id: &str, now: std::time::Instant) -> bool {
        self.0.retain(|_, entry| now.saturating_duration_since(entry.last_seen) <= GIVE_UP_TTL);
        self.0.retain(|id, entry| entry.bot_id != bot_id || id == run_id);
        if let Some(entry) = self.0.get_mut(run_id) {
            if now.saturating_duration_since(entry.last_seen) > PENDING_OBSERVATION_GAP {
                entry.since = now;
            }
            entry.last_seen = now;
            return !entry.alerted && now.saturating_duration_since(entry.since) >= PENDING_ALERT_AFTER;
        }
        if self.0.len() >= GIVE_UP_CAP {
            if let Some(oldest) = self.0.iter().min_by_key(|(_, entry)| entry.since).map(|(id, _)| id.clone()) {
                self.0.remove(&oldest);
            }
        }
        self.0.insert(
            run_id.to_string(),
            PendingRun { bot_id: bot_id.to_string(), since: now, last_seen: now, alerted: false },
        );
        false
    }

    pub(crate) fn mark_alerted(&mut self, run_id: &str) {
        if let Some(entry) = self.0.get_mut(run_id) {
            entry.alerted = true;
        }
    }

    pub(crate) fn clear_run(&mut self, run_id: &str) {
        self.0.remove(run_id);
    }

    pub(crate) fn clear_other_runs(&mut self, bot_id: &str, run_id: &str) {
        self.0.retain(|id, entry| entry.bot_id != bot_id || id == run_id);
    }
}

fn pending_runs() -> &'static std::sync::Mutex<PendingRuns> {
    static T: std::sync::OnceLock<std::sync::Mutex<PendingRuns>> = std::sync::OnceLock::new();
    T.get_or_init(Default::default)
}

#[derive(Default)]
pub(crate) struct GiveUps(std::collections::HashMap<(String, &'static str), std::time::Instant>);

impl GiveUps {
    /// 這個 run 的這個原因是不是第一次（順手記下來）。`now` 由呼叫端給，測試不必真的等。
    pub(crate) fn first_time(&mut self, run_id: &str, reason: &'static str, now: std::time::Instant) -> bool {
        let key = (run_id.to_string(), reason);
        if let Some(at) = self.0.get(&key).copied() {
            if now.saturating_duration_since(at) <= GIVE_UP_TTL {
                return false;
            }
            self.0.remove(&key);
        }
        if self.0.len() >= GIVE_UP_CAP {
            if let Some(oldest) = self.0.iter().min_by_key(|(_, at)| **at).map(|(k, _)| k.clone()) {
                self.0.remove(&oldest);
            }
        }
        self.0.insert(key, now);
        true
    }
}

pub(crate) fn gave_up() -> &'static std::sync::Mutex<GiveUps> {
    static T: std::sync::OnceLock<std::sync::Mutex<GiveUps>> = std::sync::OnceLock::new();
    T.get_or_init(Default::default)
}

/// 放棄自動確認：同一個原因每個 run 只寫一次 WARN，然後回 `PENDING`（呼叫端據此重試）。
fn give_up(run: &db::Run, reason: &'static str) -> anyhow::Error {
    if gave_up().lock().unwrap_or_else(|e| e.into_inner()).first_time(&run.id, reason, std::time::Instant::now()) {
        tracing::warn!(run = %run.id, bot = %run.bot_id, reason, "冷快取的 Resume 選單沒有自動確認，保持 cold_resume_pending");
    }
    anyhow::anyhow!(PENDING)
}

/// 只有可信畫面仍顯示冷選單、且 run 權限仍有效時才累積時間；這只通知，不改按鍵守門。
async fn give_up_after_observation(
    app: &(impl Db + HerdrRoutes + HostsAccess),
    run: &db::Run,
    bot: &db::Bot,
    client: &RunClient,
    reason: &'static str,
    observed_menu: bool,
    now: Option<std::time::Instant>,
    observed_menu_pending: &mut bool,
) -> anyhow::Error {
    let error = give_up(run, reason);
    if !observed_menu || !current(app, run, client).await.unwrap_or(false) {
        pending_runs().lock().unwrap_or_else(|e| e.into_inner()).clear_run(&run.id);
        return error;
    }

    *observed_menu_pending = true;
    let now = now.unwrap_or_else(std::time::Instant::now);
    let should_alert = pending_runs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .observe(&run.id, &run.bot_id, now);
    if should_alert {
        // `supervisors.bot_id` is the authoritative Patrol identity; names are not role identifiers.
        let patrol_id = sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM supervisors WHERE bot_id = ?)")
            .bind(&run.bot_id)
            .fetch_one(app.db())
            .await;
        let patrol_is_stuck = match patrol_id {
            Ok(found) => found != 0,
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "could not identify the patrol role for a cold resume alert");
                return error;
            }
        };

        let key = format!("ops_alert:daemon:claude_cold_resume_stuck:{}", run.id);
        let mut payload = serde_json::json!({
            "source": "daemon",
            "reason": "claude_cold_resume_stuck",
            "subject": bot.name,
            "bot_id": bot.id,
            "bot_name": bot.name,
            "run_id": run.id,
            "detail": "冷快取的 Resume 選單持續 60 秒未自動確認；daemon 保留待重試狀態，沒有重貼訊息。",
            "action": "請檢查這顆 bot 的冷快取 Resume 選單與 run 狀態；確認後讓選單自行消失或由人處理。",
        });
        if patrol_is_stuck {
            payload["to_role"] = serde_json::json!("responder");
        }
        match crate::supervisor_inbox::push_inbox(app.db(), &key, "ops_alert", None, Some(&bot.id), None, &payload).await {
            Ok(_) => pending_runs().lock().unwrap_or_else(|e| e.into_inner()).mark_alerted(&run.id),
            Err(e) => tracing::warn!(run = %run.id, error = %e, "could not queue the cold resume ops_alert"),
        }
    }
    error
}

/// 兩次讀要完全相同（pane、來源、截斷、文字）。herdr 的 `pane.read` 不提供可信 revision（實測恆為 0），
/// 不能用它當證據；文字相同加上 run／host／session／pane 的 fence 才是授權。
fn same_frame(a: &am_base::herdr::PaneRead, b: &am_base::herdr::PaneRead) -> bool {
    a.pane_id == b.pane_id
        && a.source == b.source
        && a.truncated == b.truncated
        && a.text == b.text
}

async fn accept_on_body(
    app: &(impl Db + HerdrRoutes + HostsAccess),
    run: &db::Run,
    client: &RunClient,
) -> anyhow::Result<bool> {
    accept_on_body_at(app, run, client, None).await
}

async fn accept_on_body_at(
    app: &(impl Db + HerdrRoutes + HostsAccess),
    run: &db::Run,
    client: &RunClient,
    now: Option<std::time::Instant>,
) -> anyhow::Result<bool> {
    let mut observed_menu_pending = false;
    let result = accept_on_body_inner(app, run, client, now, &mut observed_menu_pending).await;
    if !observed_menu_pending {
        pending_runs().lock().unwrap_or_else(|e| e.into_inner()).clear_run(&run.id);
    }
    result
}

async fn accept_on_body_inner(
    app: &(impl Db + HerdrRoutes + HostsAccess),
    run: &db::Run,
    client: &RunClient,
    now: Option<std::time::Instant>,
    observed_menu_pending: &mut bool,
) -> anyhow::Result<bool> {
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else {
        return Ok(false);
    };
    if bot.kind != "claude" {
        return Ok(false);
    }
    pending_runs().lock().unwrap_or_else(|e| e.into_inner()).clear_other_runs(&run.bot_id, &run.id);
    let Some(pane) = run.pane_id.as_deref() else {
        return Err(give_up_after_observation(app, run, &bot, client, NO_PANE, false, now, observed_menu_pending).await);
    };
    let mut answered = false;
    let mut seen = false;
    for _ in 0..2 {
        let read = client.pane_read(pane, "visible", 80).await?;
        let Some(choice) = cold_resume_choice(&read.text) else {
            if seen
                && current(app, run, client).await?
                && !crate::tui_prompts::awaits_menu_choice(&read.text)
                && crate::composer_parse::box_state("claude", &read.text)
                    == crate::composer_parse::BoxState::Empty
            {
                // herdr 曾把這個自動處理的框標成 blocked；真的回到空框才撤掉，不蓋掉 working 或別的問題。
                sqlx::query("UPDATE runs SET agent_status='idle', agent_status_since=? WHERE id=? AND state='running' AND agent_status IN ('blocked','unknown')")
                    .bind(db::now()).bind(&run.id).execute(app.db()).await?;
            }
            return Ok(seen);
        };
        seen = true;
        if read.pane_id != pane || read.source != "visible" || read.truncated {
            return Err(give_up_after_observation(app, run, &bot, client, READ_UNTRUSTED, false, now, observed_menu_pending).await);
        }
        if answered {
            return Err(give_up_after_observation(app, run, &bot, client, STILL_SHOWN, true, now, observed_menu_pending).await);
        }
        // 游標不在 Resume 上（含沒有游標）一律不按、也不移焦：那一鍵可能選到「Start a new conversation」。
        if choice != ResumeChoice::Resume {
            return Err(give_up_after_observation(app, run, &bot, client, CURSOR_NOT_ON_RESUME, true, now, observed_menu_pending).await);
        }
        #[cfg(all(test, feature = "daemon-test-harness"))]
        super::race_point::hit("cold_resume_before_key", &run.id).await;
        if !current(app, run, client).await? {
            return Err(give_up_after_observation(app, run, &bot, client, AUTHORITY_CHANGED, false, now, observed_menu_pending).await);
        }
        let latest = client.pane_read(pane, "visible", 80).await?;
        if !same_frame(&read, &latest) {
            let latest_is_menu = latest.pane_id == pane
                && latest.source == "visible"
                && !latest.truncated
                && cold_resume_choice(&latest.text).is_some();
            return Err(give_up_after_observation(app, run, &bot, client, SCREEN_CHANGED, latest_is_menu, now, observed_menu_pending).await);
        }
        if !current(app, run, client).await? {
            return Err(give_up_after_observation(app, run, &bot, client, AUTHORITY_CHANGED, false, now, observed_menu_pending).await);
        }
        // Herdr 沒有按 revision 的 conditional send；用最後一讀的文字＋同一連線世代縮小窗口。
        let sent = app.hosts()
            .run_if_current(&client.fence, client.pane_send_keys(pane, &["enter"]))
            .await;
        let Some(sent) = sent else {
            return Err(give_up_after_observation(app, run, &bot, client, AUTHORITY_CHANGED, false, now, observed_menu_pending).await);
        };
        sent?;
        answered = true;
        tokio::time::sleep(pane_settle(SETTLE)).await;
    }
    Err(give_up_after_observation(app, run, &bot, client, STILL_SHOWN, seen, now, observed_menu_pending).await)
}

/// herdr 的 idle／blocked 不能把此框當成已 ready 的輸入框。
pub fn startup_status(kind: &str, screen: &str, reported: AgentStatus) -> AgentStatus {
    if kind == "claude" && cold_resume_choice(screen).is_some() {
        AgentStatus::Unknown
    } else {
        reported
    }
}

/// `--resume` 先畫出一幀空框才畫警告。接回啟動須等空框穩定，期間看到警告先處理再等。
/// 失敗保留活著的 run、回 unknown，由巡邏／送達前守衛補處理，不能標成可打字或要人選。
pub fn after_start<'a>(
    app: &'a (impl Db + HerdrRoutes + HostsAccess),
    kind: &'a str,
    run_id: &'a str,
    reported: AgentStatus,
    expected: Option<&'a HostFence>,
) -> impl std::future::Future<Output = AgentStatus> + Send + 'a {
    Box::pin(after_start_body(app, kind, run_id, reported, expected))
}

async fn after_start_body(
    app: &(impl Db + HerdrRoutes + HostsAccess),
    kind: &str,
    run_id: &str,
    reported: AgentStatus,
    expected: Option<&HostFence>,
) -> AgentStatus {
    if kind != "claude" {
        return reported;
    }
    let Ok(Some(run)) = db::run(app.db(), run_id).await else {
        return reported;
    };
    // SessionStart 可能先於警告畫面抵達，consume_resume_session 已清掉 marker；仍須檢查這次接回。
    if run.resume_session_id.is_none() && run.resume_outcome.is_none() {
        return reported;
    }
    let Ok(client) = super::client_for_run(app, &run).await else {
        pending_runs().lock().unwrap_or_else(|e| e.into_inner()).clear_run(run_id);
        return AgentStatus::Unknown;
    };
    if expected.is_some_and(|f| !f.same_authority(&client.fence)) {
        return AgentStatus::Unknown;
    }
    let Some(pane) = run.pane_id.as_deref() else {
        return AgentStatus::Unknown;
    };
    let mut stable = 0;
    let mut resumed = false;
    for _ in 0..12 {
        tokio::time::sleep(pane_settle(SETTLE)).await;
        let Ok(read) = client.pane_read(pane, "visible", 80).await else {
            return AgentStatus::Unknown;
        };
        if read.pane_id != pane || read.source != "visible" || read.truncated {
            return AgentStatus::Unknown;
        }
        if startup_status(kind, &read.text, reported) == AgentStatus::Unknown
            && cold_resume_choice(&read.text).is_some()
        {
            if accept_on(app, &run, &client).await.is_err() {
                return AgentStatus::Unknown;
            }
            resumed = true;
            stable = 0;
            continue;
        }
        if crate::composer_parse::box_state(kind, &read.text)
            == crate::composer_parse::BoxState::Empty
        {
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::race_point::hit("cold_resume_start_empty_frame", &run.id).await;
            stable += 1;
            if stable >= 4 {
                if !current(app, &run, &client).await.unwrap_or(false) {
                    return AgentStatus::Unknown;
                }
                return if resumed { AgentStatus::Idle } else { reported };
            }
        } else {
            stable = 0;
            // 其他確認框仍交給原本的處理流程。
            if crate::tui_prompts::awaits_menu_choice(&read.text) {
                return reported;
            }
        }
    }
    AgentStatus::Unknown
}

#[cfg(all(test, feature = "daemon-test-harness"))]
#[path = "claude_resume_menu_tests.rs"]
mod tests;
