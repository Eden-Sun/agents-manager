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
/// 最多移焦兩次、按一次 Enter；畫面改變或權威失效時不盲按、不重貼訊息。
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
    let client = super::client_for_run(app, run)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    accept_on(app, run, &client).await
}

pub fn accept_on<'a>(
    app: &'a (impl Db + HerdrRoutes + HostsAccess),
    run: &'a db::Run,
    client: &'a RunClient,
) -> impl std::future::Future<Output = anyhow::Result<bool>> + Send + 'a {
    Box::pin(accept_on_body(app, run, client))
}

async fn accept_on_body(
    app: &(impl Db + HerdrRoutes + HostsAccess),
    run: &db::Run,
    client: &RunClient,
) -> anyhow::Result<bool> {
    let Some(bot) = db::bot(app.db(), &run.bot_id).await? else {
        return Ok(false);
    };
    if bot.kind != "claude" {
        return Ok(false);
    }
    let pane = run
        .pane_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!(PENDING))?;
    let mut answered = false;
    let mut moves = 0;
    let mut seen = false;
    for _ in 0..4 {
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
        if answered
            || read.pane_id != pane
            || read.source != "visible"
            || read.truncated
            || read.revision == 0
        {
            anyhow::bail!(PENDING);
        }
        let key = match choice {
            ResumeChoice::Resume => "enter",
            _ => "up",
        };
        if key == "up" {
            if moves >= 2 {
                anyhow::bail!(PENDING)
            }
            moves += 1;
        }
        #[cfg(all(test, feature = "daemon-test-harness"))]
        super::race_point::hit("cold_resume_before_key", &run.id).await;
        if !current(app, run, client).await? {
            anyhow::bail!(PENDING)
        }
        let latest = client.pane_read(pane, "visible", 80).await?;
        if latest.revision != read.revision
            || latest.text != read.text
            || latest.pane_id != read.pane_id
            || latest.source != "visible"
            || latest.truncated
            || !current(app, run, client).await?
        {
            anyhow::bail!(PENDING);
        }
        // Herdr 沒有按 revision 的 conditional send；同 survey 使用最後一讀＋同一連線世代。
        app.hosts()
            .run_if_current(&client.fence, client.pane_send_keys(pane, &[key]))
            .await
            .ok_or_else(|| anyhow::anyhow!(PENDING))??;
        answered = key == "enter";
        tokio::time::sleep(SETTLE).await;
    }
    anyhow::bail!(PENDING)
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
        tokio::time::sleep(SETTLE).await;
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
