//! Repair a stale Herdr cache in the existing run status, using the poller's live pane.
use super::super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

macro_rules! stamp_type {
    ($vis:vis) => {
        #[derive(Clone)]
        $vis struct Stamp {
            run: db::Run,
            host: Option<String>,
            generation: Option<i64>,
        }
    };
}
#[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
stamp_type!(pub);
#[cfg(not(any(feature = "test-hooks", all(test, feature = "daemon-test-harness"))))]
stamp_type!(pub(super));

impl Stamp {
    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    pub async fn capture(app: &impl LcHost, run: &db::Run) -> anyhow::Result<Self> {
        Self::capture_inner(app, run).await
    }
    #[cfg(not(any(feature = "test-hooks", all(test, feature = "daemon-test-harness"))))]
    pub(super) async fn capture(app: &impl LcHost, run: &db::Run) -> anyhow::Result<Self> {
        Self::capture_inner(app, run).await
    }
    async fn capture_inner(app: &impl LcHost, run: &db::Run) -> anyhow::Result<Self> {
        let (host, generation) = sqlx::query_as("SELECT host,host_generation FROM runs WHERE id=?")
            .bind(&run.id)
            .fetch_one(app.db())
            .await?;
        Ok(Self {
            run: run.clone(),
            host,
            generation,
        })
    }

    async fn matches(&self, app: &impl LcHost, fresh: &db::Run) -> anyhow::Result<bool> {
        let now = Self::capture(app, fresh).await?;
        Ok(fresh.state == "running"
            && fresh.ended_at.is_none()
            && fresh.bot_id == self.run.bot_id
            && fresh.pane_id == self.run.pane_id
            && fresh.herdr_session == self.run.herdr_session
            && fresh.native_session_id == self.run.native_session_id
            && fresh.terminal_id == self.run.terminal_id
            && now.host == self.host
            && now.generation == self.generation)
    }
}

#[derive(Clone)]
struct Owned {
    stamp: Stamp,
    turn: String,
    since: Option<String>,
    authority: crate::hosts::HostAuthorityKey,
}

fn owned() -> &'static Mutex<HashMap<String, Owned>> {
    static OWNED: OnceLock<Mutex<HashMap<String, Owned>>> = OnceLock::new();
    OWNED.get_or_init(Default::default)
}

fn receipt(run: &str) -> Option<Owned> {
    owned()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(run)
        .cloned()
}

pub(super) fn needed(run: &db::Run, turn: &str) -> bool {
    matches!(run.agent_status.as_str(), "idle" | "unknown")
        || (run.agent_status == "working" && receipt(&run.id).is_some_and(|p| p.turn != turn))
}

fn forget(run: &str, prior: &Owned) {
    let mut map = owned().lock().unwrap_or_else(|e| e.into_inner());
    if map.get(run).is_some_and(|p| {
        p.turn == prior.turn && p.since == prior.since && p.authority == prior.authority
    }) {
        map.remove(run);
    }
}

fn menu(text: &str) -> bool {
    crate::tui_prompts::awaits_menu_choice(text)
        || crate::tui_prompts::permission_prompt(text).is_some()
        || crate::tui_prompts::is_login_menu(text)
        || crate::tui_prompts::is_switch_model_dialog(text)
        || crate::tui_prompts::is_auto_mode_offer(text)
        || crate::tui_prompts::is_onboarding_theme(text)
        || crate::tui_prompts::cold_resume_choice(text).is_some()
        || crate::tui_prompts::is_session_paused_menu(text)
}

pub(super) async fn observe(
    app: &impl LcHost,
    client: &RunClient,
    stamp: &Stamp,
    turn: &str,
    kind: &str,
    text: &str,
) -> anyhow::Result<()> {
    if kind != "claude"
        || menu(text)
        || !super::is_screen_busy(text)
        || !crate::capture::claude::PARSER.still_busy(text)
    {
        return Ok(());
    }
    #[cfg(all(test, feature = "daemon-test-harness"))]
    super::super::race_point::hit("observed_status_before_write", &stamp.run.id).await;
    let lock = app.bot_lock(&stamp.run.bot_id).await;
    let guard = lock.lock().await;
    let changed = client.fence.run_current(Box::pin(async {
        if db::bot_host(app.db(), &stamp.run.bot_id).await? != client.fence.conn().name {
            return Ok::<_, anyhow::Error>(false);
        }
        let Some(fresh) = db::run(app.db(), &stamp.run.id).await? else { return Ok(false) };
        if !stamp.matches(app, &fresh).await? { return Ok(false) }
        let Some(current) = db::in_flight_turn(app.db(), &fresh.id).await? else { return Ok(false) };
        if current.id != turn || current.delivery != "ok" { return Ok(false) }
        if fresh.agent_status == "working" {
            if let Some(mut prior) = receipt(&fresh.id) {
                if prior.authority.matches(&client.fence) && prior.since == fresh.agent_status_since
                    && prior.stamp.matches(app, &fresh).await? {
                    prior.turn = turn.to_owned();
                    owned().lock().unwrap_or_else(|e| e.into_inner()).insert(fresh.id.clone(), prior);
                } else { forget(&fresh.id, &prior); }
            }
            return Ok(false);
        }
        if !matches!(fresh.agent_status.as_str(), "idle" | "unknown") { return Ok(false) }
        let mut tx = app.db().begin().await?;
        let changed = sqlx::query(
            "UPDATE runs SET agent_status='working' WHERE id=? AND state='running' AND ended_at IS NULL
             AND agent_status=? AND agent_status_since IS ? AND pane_id IS ? AND herdr_session IS ?
             AND native_session_id IS ? AND terminal_id IS ? AND host IS ? AND host_generation IS ?
             AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                        WHERE b.id=runs.bot_id AND b.kind='claude' AND b.deleted_at IS NULL AND p.host=?)
             AND EXISTS(SELECT 1 FROM turns WHERE id=? AND run_id=runs.id AND status='in_flight' AND delivery='ok')")
            .bind(&fresh.id).bind(&fresh.agent_status).bind(&fresh.agent_status_since)
            .bind(&stamp.run.pane_id).bind(&stamp.run.herdr_session).bind(&stamp.run.native_session_id)
            .bind(&stamp.run.terminal_id).bind(&stamp.host).bind(stamp.generation).bind(&client.fence.conn().name).bind(turn)
            .execute(&mut *tx).await?.rows_affected() == 1;
        let since: Option<String> = if changed {
            sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id=?")
                .bind(&fresh.id).fetch_one(&mut *tx).await?
        } else { None };
        tx.commit().await?;
        if changed {
            owned().lock().unwrap_or_else(|e| e.into_inner()).insert(fresh.id.clone(), Owned {
                stamp: stamp.clone(), turn: turn.to_owned(), since,
                authority: client.fence.authority_key(),
            });
            tracing::info!(run = %fresh.id, turn, basis = "is_screen_busy/live_footer", "repaired stale cached agent status from live pane");
        }
        Ok(changed)
    })).await.transpose()?.unwrap_or(false);
    drop(guard);
    if changed {
        app.emit_bot_status(&stamp.run.bot_id).await;
    }
    Ok(())
}

pub(super) async fn finish(app: &impl LcHost, run: &str, turn: &str) -> anyhow::Result<bool> {
    let Some(prior) = receipt(run).filter(|p| p.turn == turn) else {
        return Ok(true);
    };
    let Some(fresh) = db::run(app.db(), run).await? else {
        forget(run, &prior);
        return Ok(true);
    };
    if !prior.stamp.matches(app, &fresh).await?
        || fresh.agent_status != "working"
        || fresh.agent_status_since != prior.since
    {
        forget(run, &prior);
        return Ok(true);
    }
    if db::in_flight_turn(app.db(), run).await?.is_some() {
        return Ok(true);
    }
    let client = client_for_run(app, &fresh)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    if !prior.authority.matches(&client.fence) {
        forget(run, &prior);
        return Ok(true);
    }
    let Some(pane) = fresh.pane_id.as_deref() else {
        return Ok(true);
    };
    let screen = client.pane_read(pane, "recent_unwrapped", 160).await?;
    if menu(&screen.text)
        || crate::capture::claude::PARSER.still_busy(&screen.text)
        || !(super::pane_awaits_input("claude", &screen.text) || crate::capture::claude::composer_visible(&screen.text))
    {
        return Ok(false);
    }
    let lock = app.bot_lock(&fresh.bot_id).await;
    let guard = lock.lock().await;
    let changed = client.fence.run_current(Box::pin(async {
        if db::bot_host(app.db(), &fresh.bot_id).await? != client.fence.conn().name { return Ok::<_, anyhow::Error>(false) }
        let result = sqlx::query(
            "UPDATE runs SET agent_status='idle' WHERE id=? AND state='running' AND ended_at IS NULL
             AND agent_status='working' AND agent_status_since IS ? AND pane_id IS ? AND herdr_session IS ?
             AND native_session_id IS ? AND terminal_id IS ? AND host IS ? AND host_generation IS ?
             AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                        WHERE b.id=runs.bot_id AND b.kind='claude' AND b.deleted_at IS NULL AND p.host=?)
             AND EXISTS(SELECT 1 FROM turns WHERE id=? AND run_id=runs.id AND status NOT IN('queued','in_flight'))
             AND NOT EXISTS(SELECT 1 FROM turns WHERE run_id=runs.id AND status='in_flight')")
            .bind(run).bind(&prior.since).bind(&prior.stamp.run.pane_id).bind(&prior.stamp.run.herdr_session)
            .bind(&prior.stamp.run.native_session_id).bind(&prior.stamp.run.terminal_id)
            .bind(&prior.stamp.host).bind(prior.stamp.generation).bind(&client.fence.conn().name).bind(turn).execute(app.db()).await?;
        Ok(result.rows_affected() == 1)
    })).await.transpose()?.unwrap_or(false);
    if changed {
        let mut map = owned().lock().unwrap_or_else(|e| e.into_inner());
        if map
            .get(run)
            .is_some_and(|p| p.turn == turn && p.since == prior.since)
        {
            map.remove(run);
        }
    }
    drop(guard);
    if changed {
        app.emit_bot_status(&fresh.bot_id).await;
        super::super::queue::schedule_flush_queued(app, &fresh.bot_id);
    }
    Ok(changed || receipt(run).is_none())
}

/// 一個 working 超過這麼久、回合早已收掉的 run 才進安全網。
const STALE_WORKING_AFTER_SECS: i64 = 120;

/// 防呆（2026-10-30 hub-8p5dpe）：`finish` 的收據在 daemon 重啟、poller 被取代或 40 分鐘上限時會遺失，
/// herdr 本身已經是 idle/done 就不會再有 `-> idle` 邊，DB 的 working 永遠不動。
/// 條件全部成立才校正：run 在跑、`working` 超過 [`STALE_WORKING_AFTER_SECS`]、沒有 in_flight 回合、
/// 最後一回合已收掉且收掉超過同樣時間、herdr 回報 idle/done、畫面沒有 spinner 或選單且輸入框在。
/// 用與 `finish` 相同的 CAS（`agent_status_since`）；回傳 true 表示有改。
pub(super) async fn settle_stale_working(app: &impl LcHost, run: &db::Run) -> anyhow::Result<bool> {
    if run.state != "running" || run.ended_at.is_some() || run.agent_status != "working" { return Ok(false) }
    let cutoff = db::iso_in(-STALE_WORKING_AFTER_SECS);
    let Some(since) = run.agent_status_since.clone().filter(|s| *s < cutoff) else { return Ok(false) };
    let Some(pane) = run.pane_id.clone().filter(|p| !p.trim().is_empty()) else { return Ok(false) };
    if !matches!(db::bot(app.db(), &run.bot_id).await?, Some(bot) if bot.kind == "claude") { return Ok(false) }
    if db::in_flight_turn(app.db(), &run.id).await?.is_some() { return Ok(false) }
    let latest: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id,status,completed_at FROM turns WHERE run_id=? ORDER BY created_at DESC,id DESC LIMIT 1")
        .bind(&run.id).fetch_optional(app.db()).await?;
    let Some((turn, status, completed)) = latest else { return Ok(false) };
    if !matches!(status.as_str(), "completed" | "completed_fallback" | "failed")
        || !completed.as_deref().is_some_and(|at| at < cutoff.as_str()) { return Ok(false) }
    let stamp = Stamp::capture(app, run).await?;
    let client = client_for_run(app, run).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let live = client.pane_get(&pane).await?;
    if !live.and_then(|p| p.agent_status).is_some_and(|s| s.normalized() == crate::herdr::AgentStatus::Idle) { return Ok(false) }
    let screen = client.pane_read(&pane, "recent_unwrapped", 160).await?;
    if menu(&screen.text)
        || crate::capture::claude::PARSER.still_busy(&screen.text)
        || !crate::capture::claude::composer_visible(&screen.text) { return Ok(false) }
    let lock = app.bot_lock(&run.bot_id).await;
    let Ok(guard) = lock.try_lock() else { return Ok(false) };
    let changed = client.fence.run_current(Box::pin(async {
        if db::bot_host(app.db(), &run.bot_id).await? != client.fence.conn().name { return Ok::<_, anyhow::Error>(false) }
        let Some(fresh) = db::run(app.db(), &run.id).await? else { return Ok(false) };
        if !stamp.matches(app, &fresh).await? { return Ok(false) }
        let result = sqlx::query(
            "UPDATE runs SET agent_status='idle' WHERE id=? AND state='running' AND ended_at IS NULL
             AND agent_status='working' AND agent_status_since IS ? AND pane_id IS ? AND herdr_session IS ?
             AND native_session_id IS ? AND terminal_id IS ? AND host IS ? AND host_generation IS ?
             AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                        WHERE b.id=runs.bot_id AND b.kind='claude' AND b.deleted_at IS NULL AND p.host=?)
             AND ?=(SELECT id FROM turns WHERE run_id=runs.id ORDER BY created_at DESC,id DESC LIMIT 1)
             AND NOT EXISTS(SELECT 1 FROM turns WHERE run_id=runs.id AND status IN('queued','in_flight'))")
            .bind(&run.id).bind(&since).bind(&stamp.run.pane_id).bind(&stamp.run.herdr_session)
            .bind(&stamp.run.native_session_id).bind(&stamp.run.terminal_id)
            .bind(&stamp.host).bind(stamp.generation).bind(&client.fence.conn().name).bind(&turn)
            .execute(app.db()).await?;
        Ok(result.rows_affected() == 1)
    })).await.transpose()?.unwrap_or(false);
    drop(guard);
    if changed {
        tracing::warn!(run = %run.id, turn = %turn, working_since = %since, "run stuck at working after its turn completed and the pane is idle; settled to idle");
        owned().lock().unwrap_or_else(|e| e.into_inner()).remove(&run.id);
        app.emit_bot_status(&run.bot_id).await;
        super::super::queue::schedule_flush_queued(app, &run.bot_id);
    }
    Ok(changed)
}

fn menu_key(run: &str) -> String { format!("menu:{run}") }

pub(super) fn menu_owned(run: &str) -> bool { receipt(&menu_key(run)).is_some() }

fn reset_menu_clear_count(key: &str) {
    if let Some(owner) = owned().lock().unwrap_or_else(|e| e.into_inner()).get_mut(key) {
        if let Some((turn, _)) = owner.turn.rsplit_once(':') {
            owner.turn = format!("{turn}:0");
        }
    }
}

pub(super) fn retain_menu_receipts(active: &[String]) {
    owned().lock().unwrap_or_else(|e| e.into_inner()).retain(|key, _| {
        key.strip_prefix("menu:").or_else(|| key.strip_prefix("fblocked:")).is_none_or(|run| active.iter().any(|id| id == run))
    });
    clears().lock().unwrap_or_else(|e| e.into_inner()).retain(|run, _| active.iter().any(|id| id == run));
}

fn other_menu_owner(screen: &str) -> bool {
    crate::tui_prompts::cold_resume_choice(screen).is_some()
        || crate::tui_prompts::is_auto_mode_offer(screen)
        || crate::tui_prompts::dangerous_rm_prompt(screen).is_some()
        || crate::tui_prompts::is_session_paused_menu(screen)
        || crate::tui_prompts::is_switch_model_dialog(screen)
        || crate::tui_prompts::is_feedback_survey(screen)
        || crate::tui_prompts::is_login_menu(screen)
}

async fn eligible_menu_turn(app: &impl LcHost, run: &db::Run) -> anyhow::Result<Option<String>> {
    let latest: Option<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT id,status,completed_at,delivery FROM turns WHERE run_id=? ORDER BY created_at DESC,id DESC LIMIT 1")
        .bind(&run.id).fetch_optional(app.db()).await?;
    let Some((turn, status, completed, delivery)) = latest else { return Ok(None) };
    let in_flight = db::in_flight_turn(app.db(), &run.id).await?;
    if status == "in_flight" {
        return Ok(in_flight.filter(|t| t.id == turn && delivery == "ok").map(|_| turn));
    }
    if in_flight.is_some() || !matches!(status.as_str(), "completed" | "completed_fallback" | "failed") { return Ok(None) }
    if status == "completed_fallback" || completed.as_deref().is_some_and(|at| at >= db::iso_in(-1800).as_str()) {
        Ok(Some(turn))
    } else { Ok(None) }
}

/// One fresh read only. No transcript or pane content is retained in the receipt.
pub(super) async fn observe_menu(app: &impl LcHost, run: &db::Run) -> anyhow::Result<bool> {
    let result = observe_menu_inner(app, run, None, None).await;
    if result.is_err() { reset_menu_clear_count(&menu_key(&run.id)); }
    result
}

pub(super) async fn observe_permission_notification(app: &impl LcHost, run: &db::Run, at: chrono::DateTime<chrono::Utc>) -> anyhow::Result<()> {
    let Some(turn) = db::in_flight_turn(app.db(), &run.id).await?.filter(|t| t.delivery == "ok") else { return Ok(()) };
    observe_menu_inner(app, run, Some(&turn.id), Some(at)).await?;
    Ok(())
}

async fn observe_menu_inner(app: &impl LcHost, run: &db::Run, expected_turn: Option<&str>, at: Option<chrono::DateTime<chrono::Utc>>) -> anyhow::Result<bool> {
    let key = menu_key(&run.id);
    if run.state != "running" || (!matches!(run.agent_status.as_str(), "idle" | "unknown" | "blocked") && !(expected_turn.is_some() && run.agent_status == "working")) { return Ok(false) }
    if run.agent_status == "blocked" && !menu_owned(&run.id) { return Ok(false) }
    if !matches!(db::bot(app.db(), &run.bot_id).await?, Some(bot) if bot.kind == "claude") { return Ok(false) }
    if run.agent_status != "blocked" && eligible_menu_turn(app, run).await?.is_none() { return Ok(false) }
    let stamp = Stamp::capture(app, run).await?;
    let Some(pane) = run.pane_id.as_deref().filter(|p| !p.trim().is_empty()) else {
        reset_menu_clear_count(&key);
        return Ok(false);
    };
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let client = client_for_run(app, run).await.ok()?;
        let read = client.pane_read(pane, "visible", 80).await.ok()?;
        Some((client, read))
    }).await;
    let (client, read) = match result {
        Ok(Some((client, read))) if read.pane_id == pane && read.source == "visible" && matches!(read.format.as_str(), "text" | "ansi") && !read.truncated => (client, read),
        _ => { reset_menu_clear_count(&key); return Ok(true); }
    };
    let plain = crate::composer_parse::strip_ansi(&read.text);
    if other_menu_owner(&plain) {
        if expected_turn.is_none() { if let Some(owner) = receipt(&key) { forget(&key, &owner); } }
        return Ok(true);
    }
    let open = if expected_turn.is_some() { crate::tui_prompts::permission_prompt(&plain).is_some() } else { super::super::delivery::claude_open_choice_menu(&plain) };
    if expected_turn.is_some() && !open { return Ok(true) }
    let clear = !open && (super::pane_awaits_input("claude", &plain) || crate::capture::claude::PARSER.still_busy(&plain));
    if !open && !clear { reset_menu_clear_count(&key); return Ok(true); }

    let lock = app.bot_lock(&run.bot_id).await;
    let Ok(guard) = lock.try_lock() else {
        if expected_turn.is_some() { return Err(anyhow::anyhow!("permission screen observation busy")) }
        reset_menu_clear_count(&key);
        return Ok(true);
    };
    let changed = client.fence.run_current(Box::pin(async {
        if db::bot_host(app.db(), &run.bot_id).await? != client.fence.conn().name { reset_menu_clear_count(&key); return Ok::<_, anyhow::Error>(false) }
        let Some(fresh) = db::run(app.db(), &run.id).await? else { reset_menu_clear_count(&key); return Ok(false) };
        if !stamp.matches(app, &fresh).await? { reset_menu_clear_count(&key); return Ok(false) }
        if at.is_some_and(|t| (chrono::Utc::now() - t).num_seconds().unsigned_abs() > 120) { return Ok(false) }
        if expected_turn.is_some() && stamp.generation.is_some_and(|was| client.fence.target_epoch().is_some_and(|now| now != was)) { return Ok(false) }
        let mut prior = receipt(&key);
        if let Some(ref owner) = prior {
            if fresh.agent_status != "blocked" || fresh.agent_status_since != owner.since
                || !owner.authority.matches(&client.fence) || !owner.stamp.matches(app, &fresh).await? {
                forget(&key, owner);
                if expected_turn.is_none() || fresh.agent_status == "blocked" { return Ok(false) }
                prior = None;
            }
        }
        if open {
            if prior.is_some() { reset_menu_clear_count(&key); return Ok(false) }
            if !matches!(fresh.agent_status.as_str(), "idle" | "unknown") && !(expected_turn.is_some() && fresh.agent_status == "working") { return Ok(false) }
            let Some(turn) = eligible_menu_turn(app, &fresh).await? else { return Ok(false) };
            if expected_turn.is_some_and(|expected| expected != turn) { return Ok(false) }
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::super::race_point::hit("observed_menu_before_cas", &fresh.id).await;
            let mut tx = app.db().begin().await?;
            let applied = sqlx::query(
                "UPDATE runs SET agent_status='blocked' WHERE id=? AND state='running' AND ended_at IS NULL
                 AND agent_status=? AND agent_status_since IS ? AND pane_id IS ? AND herdr_session IS ?
                 AND native_session_id IS ? AND terminal_id IS ? AND host IS ? AND host_generation IS ?
                 AND (? IS NULL OR ABS((julianday('now') - julianday(?)) * 86400) <= 120)
                 AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                            WHERE b.id=runs.bot_id AND b.kind='claude' AND b.deleted_at IS NULL AND p.host=?)
                 AND ?=(SELECT id FROM turns WHERE run_id=runs.id ORDER BY created_at DESC,id DESC LIMIT 1)
                 AND EXISTS(SELECT 1 FROM turns t WHERE t.id=? AND t.run_id=runs.id AND (?=0 OR (t.status='in_flight' AND t.delivery='ok')) AND (
                       (t.status='in_flight' AND t.delivery='ok') OR
                       (NOT EXISTS(SELECT 1 FROM turns live WHERE live.run_id=runs.id AND live.status='in_flight')
                        AND (t.status='completed_fallback' OR (t.status IN ('completed','failed') AND t.completed_at>=?)))))")
                .bind(&fresh.id).bind(&fresh.agent_status).bind(&fresh.agent_status_since)
                .bind(&stamp.run.pane_id).bind(&stamp.run.herdr_session).bind(&stamp.run.native_session_id)
                .bind(&stamp.run.terminal_id).bind(&stamp.host).bind(stamp.generation)
                .bind(at.map(|t| t.to_rfc3339())).bind(at.map(|t| t.to_rfc3339()))
                .bind(&client.fence.conn().name).bind(&turn).bind(&turn).bind(expected_turn.is_some()).bind(db::iso_in(-1800)).execute(&mut *tx).await?.rows_affected() == 1;
            let since: Option<String> = if applied {
                sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id=?").bind(&fresh.id).fetch_one(&mut *tx).await?
            } else { None };
            tx.commit().await?;
            if applied {
                owned().lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), Owned {
                    stamp: stamp.clone(), turn: format!("{turn}:0"), since, authority: client.fence.authority_key(),
                });
            }
            return Ok(applied);
        }
        let Some(owner) = prior else { return Ok(false) };
        let (turn, count) = owner.turn.rsplit_once(':').unwrap_or((&owner.turn, "0"));
        let latest: Option<String> = sqlx::query_scalar("SELECT id FROM turns WHERE run_id=? ORDER BY created_at DESC,id DESC LIMIT 1")
            .bind(&fresh.id).fetch_optional(app.db()).await?;
        if latest.as_deref() != Some(turn) { forget(&key, &owner); return Ok(false) }
        if count != "1" {
            let mut map = owned().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(current) = map.get_mut(&key) {
                if current.since == owner.since && current.turn == owner.turn { current.turn = format!("{turn}:1"); }
            }
            return Ok(false);
        }
        let applied = sqlx::query(
            "UPDATE runs SET agent_status='unknown' WHERE id=? AND state='running' AND ended_at IS NULL
             AND agent_status='blocked' AND agent_status_since IS ? AND pane_id IS ? AND herdr_session IS ?
             AND native_session_id IS ? AND terminal_id IS ? AND host IS ? AND host_generation IS ?
             AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                        WHERE b.id=runs.bot_id AND b.kind='claude' AND b.deleted_at IS NULL AND p.host=?)
             AND ?=(SELECT id FROM turns WHERE run_id=runs.id ORDER BY created_at DESC,id DESC LIMIT 1)")
            .bind(&fresh.id).bind(&owner.since).bind(&owner.stamp.run.pane_id).bind(&owner.stamp.run.herdr_session)
            .bind(&owner.stamp.run.native_session_id).bind(&owner.stamp.run.terminal_id).bind(&owner.stamp.host)
            .bind(owner.stamp.generation).bind(&client.fence.conn().name).bind(turn).execute(app.db()).await?.rows_affected() == 1;
        if applied { forget(&key, &owner); }
        Ok(applied)
    })).await.transpose()?.unwrap_or_else(|| { reset_menu_clear_count(&key); false });
    drop(guard);
    if changed { app.emit_bot_status(&run.bot_id).await; }
    Ok(true)
}

/// 畫面「沒有提示」的判斷要連續成立至少這麼久、至少兩次讀取：畫面剛換、框還沒畫出來的一瞬間不算。
const FALSE_BLOCK_MIN_SPAN: Duration = Duration::from_millis(1000);

struct Clear {
    since: Option<String>,
    first: std::time::Instant,
    reads: u32,
}

fn clears() -> &'static Mutex<HashMap<String, Clear>> {
    static CLEARS: OnceLock<Mutex<HashMap<String, Clear>>> = OnceLock::new();
    CLEARS.get_or_init(Default::default)
}

fn forget_clear(run: &str) {
    clears().lock().unwrap_or_else(|e| e.into_inner()).remove(run);
}

fn false_key(run: &str) -> String { format!("fblocked:{run}") }

/// 這個 run 的 blocked 是不是被我們判成誤報而改掉的（收據還在）。
#[allow(dead_code)]
pub(super) fn false_blocked_owned(run: &str) -> bool { receipt(&false_key(run)).is_some() }

/// 讀一次 pane 畫面（visible 80 列）；讀不到／不是這個 pane 的／被截斷都回 `None`（讀不到不等於沒有提示）。
async fn read_visible(app: &impl LcHost, run: &db::Run, pane: &str) -> Option<(RunClient, String)> {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let client = client_for_run(app, run).await.ok()?;
        let read = client.pane_read(pane, "visible", 80).await.ok()?;
        Some((client, read))
    }).await;
    match result {
        Ok(Some((client, read))) if read.pane_id == pane && read.source == "visible" && matches!(read.format.as_str(), "text" | "ansi") && !read.truncated => {
            Some((client, crate::composer_parse::strip_ansi(&read.text)))
        }
        _ => None,
    }
}

/// 一個 blocked 的 claude run：畫面上沒有可回答的提示就改掉狀態。回傳 `true`＝這一次改了（呼叫端不必再做什麼，已推 `bot_status`）。
/// 第一次看到沒有提示只記一筆 debug 日誌與計數，等下一次讀取確認。
pub(super) async fn settle_false_blocked(app: &impl LcHost, run: &db::Run) -> anyhow::Result<bool> {
    if run.state != "running" || run.ended_at.is_some() || run.agent_status != "blocked" {
        forget_clear(&run.id);
        return Ok(false);
    }
    if menu_owned(&run.id) || crate::blocked_reason::of(&run.id).is_some() {
        forget_clear(&run.id);
        return Ok(false);
    }
    if !matches!(db::bot(app.db(), &run.bot_id).await?, Some(bot) if bot.kind == "claude") { return Ok(false) }
    let Some(pane) = run.pane_id.as_deref().filter(|p| !p.trim().is_empty()) else { return Ok(false) };
    let stamp = Stamp::capture(app, run).await?;
    let Some((client, plain)) = read_visible(app, run, pane).await else {
        forget_clear(&run.id);
        return Ok(false);
    };
    if crate::tui_prompts::screen_has_answerable_prompt(&plain) {
        forget_clear(&run.id);
        return Ok(false);
    }
    {
        let mut map = clears().lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(run.id.clone()).or_insert_with(|| Clear { since: run.agent_status_since.clone(), first: std::time::Instant::now(), reads: 0 });
        if entry.since != run.agent_status_since {
            *entry = Clear { since: run.agent_status_since.clone(), first: std::time::Instant::now(), reads: 0 };
        }
        entry.reads += 1;
        if entry.reads == 1 {
            tracing::debug!(run = %run.id, pane, "herdr says blocked but the pane shows nothing to answer; confirming on the next read");
        }
        if entry.reads < 2 || entry.first.elapsed() < FALSE_BLOCK_MIN_SPAN {
            return Ok(false);
        }
    }
    let busy = super::is_screen_busy(&plain) && crate::capture::claude::PARSER.still_busy(&plain);
    let idle_ok = !busy
        && (super::pane_awaits_input("claude", &plain) || crate::capture::claude::composer_visible(&plain))
        && db::in_flight_turn(app.db(), &run.id).await?.is_none();
    let target = if busy { "working" } else if idle_ok { "idle" } else { "unknown" };
    let lock = app.bot_lock(&run.bot_id).await;
    let Ok(guard) = lock.try_lock() else { return Ok(false) };
    let applied = client.fence.run_current(Box::pin(async {
        if db::bot_host(app.db(), &run.bot_id).await? != client.fence.conn().name { return Ok::<_, anyhow::Error>(None) }
        let Some(fresh) = db::run(app.db(), &run.id).await? else { return Ok(None) };
        if !stamp.matches(app, &fresh).await? || fresh.agent_status != "blocked" { return Ok(None) }
        let mut tx = app.db().begin().await?;
        let changed = sqlx::query(
            "UPDATE runs SET agent_status=? WHERE id=? AND state='running' AND ended_at IS NULL
             AND agent_status='blocked' AND agent_status_since IS ? AND pane_id IS ? AND herdr_session IS ?
             AND native_session_id IS ? AND terminal_id IS ? AND host IS ? AND host_generation IS ?
             AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                        WHERE b.id=runs.bot_id AND b.kind='claude' AND b.deleted_at IS NULL AND p.host=?)
             AND (?='working' OR ?='unknown' OR NOT EXISTS(SELECT 1 FROM turns WHERE run_id=runs.id AND status='in_flight'))")
            .bind(target).bind(&fresh.id).bind(&fresh.agent_status_since).bind(&stamp.run.pane_id).bind(&stamp.run.herdr_session)
            .bind(&stamp.run.native_session_id).bind(&stamp.run.terminal_id).bind(&stamp.host).bind(stamp.generation)
            .bind(&client.fence.conn().name).bind(target).bind(target)
            .execute(&mut *tx).await?.rows_affected() == 1;
        let since: Option<String> = if changed {
            sqlx::query_scalar("SELECT agent_status_since FROM runs WHERE id=?").bind(&fresh.id).fetch_one(&mut *tx).await?
        } else { None };
        tx.commit().await?;
        if changed {
            owned().lock().unwrap_or_else(|e| e.into_inner()).insert(false_key(&fresh.id), Owned {
                stamp: stamp.clone(), turn: String::new(), since: since.clone(), authority: client.fence.authority_key(),
            });
        }
        Ok(changed.then_some(()))
    })).await.transpose()?.flatten().is_some();
    drop(guard);
    forget_clear(&run.id);
    if applied {
        tracing::info!(run = %run.id, pane, status = target, basis = "no answerable prompt on the pane", "herdr reported blocked but nothing on screen can be answered; settled the run status");
        crate::blocked_reason::forget(&run.id);
        crate::child_alerts::forget(&run.bot_id);
        app.emit_bot_status(&run.bot_id).await;
        if target == "idle" {
            super::super::queue::schedule_flush_queued(app, &run.bot_id);
        }
    }
    Ok(applied)
}

/// [`settle_false_blocked`] 改過狀態的 run：herdr 沒再報、畫面卻出現可回答的提示時，改回 blocked（不按鍵）。
/// 收據的 `agent_status_since` 不吻合＝herdr（或別人）已經改過狀態，收據作廢、不動。回傳 `true`＝這一次改回了。
pub(super) async fn reblock_if_prompt(app: &impl LcHost, run: &db::Run) -> anyhow::Result<bool> {
    let key = false_key(&run.id);
    let Some(owner) = receipt(&key) else { return Ok(false) };
    if run.state != "running" || run.ended_at.is_some() || run.agent_status == "blocked" || run.agent_status_since != owner.since {
        forget(&key, &owner);
        return Ok(false);
    }
    let Some(pane) = run.pane_id.as_deref().filter(|p| !p.trim().is_empty()) else { return Ok(false) };
    let Some((client, plain)) = read_visible(app, run, pane).await else { return Ok(false) };
    if !crate::tui_prompts::screen_has_answerable_prompt(&plain) { return Ok(false) }
    let lock = app.bot_lock(&run.bot_id).await;
    let Ok(guard) = lock.try_lock() else { return Ok(false) };
    let applied = client.fence.run_current(Box::pin(async {
        if db::bot_host(app.db(), &run.bot_id).await? != client.fence.conn().name { return Ok::<_, anyhow::Error>(false) }
        let Some(fresh) = db::run(app.db(), &run.id).await? else { return Ok(false) };
        if !owner.authority.matches(&client.fence) || !owner.stamp.matches(app, &fresh).await? || fresh.agent_status_since != owner.since {
            forget(&key, &owner);
            return Ok(false);
        }
        let result = sqlx::query(
            "UPDATE runs SET agent_status='blocked' WHERE id=? AND state='running' AND ended_at IS NULL
             AND agent_status IN ('working','idle','unknown') AND agent_status_since IS ? AND pane_id IS ? AND herdr_session IS ?
             AND native_session_id IS ? AND terminal_id IS ? AND host IS ? AND host_generation IS ?
             AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                        WHERE b.id=runs.bot_id AND b.kind='claude' AND b.deleted_at IS NULL AND p.host=?)")
            .bind(&fresh.id).bind(&owner.since).bind(&owner.stamp.run.pane_id).bind(&owner.stamp.run.herdr_session)
            .bind(&owner.stamp.run.native_session_id).bind(&owner.stamp.run.terminal_id).bind(&owner.stamp.host)
            .bind(owner.stamp.generation).bind(&client.fence.conn().name)
            .execute(app.db()).await?;
        if result.rows_affected() == 1 { forget(&key, &owner); }
        Ok(result.rows_affected() == 1)
    })).await.transpose()?.unwrap_or(false);
    drop(guard);
    if applied {
        tracing::info!(run = %run.id, pane, "a prompt appeared on a pane whose false blocked had been settled; restored blocked");
        app.emit_bot_status(&run.bot_id).await;
    }
    Ok(applied)
}


#[cfg(any(test, feature = "test-hooks"))]
pub mod test_seams {
    use super::*;

    #[cfg(any(feature = "test-hooks", all(test, feature = "daemon-test-harness")))]
    pub use super::Stamp;

    pub fn menu(text: &str) -> bool { super::menu(text) }
    pub fn menu_owned(run: &str) -> bool { super::menu_owned(run) }
    pub fn false_blocked_owned(run: &str) -> bool { super::false_blocked_owned(run) }
    pub async fn observe(
        app: &impl LcHost,
        client: &RunClient,
        stamp: &Stamp,
        turn: &str,
        kind: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        super::observe(app, client, stamp, turn, kind, text).await
    }
    pub async fn finish(app: &impl LcHost, run: &str, turn: &str) -> anyhow::Result<bool> {
        super::finish(app, run, turn).await
    }
    pub async fn observe_menu(app: &impl LcHost, run: &db::Run) -> anyhow::Result<bool> {
        super::observe_menu(app, run).await
    }
}
