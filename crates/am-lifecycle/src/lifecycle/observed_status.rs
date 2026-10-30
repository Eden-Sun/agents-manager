//! Repair a stale Herdr cache in the existing run status, using the poller's live pane.
use super::super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Clone)]
pub(super) struct Stamp {
    run: db::Run,
    host: Option<String>,
    generation: Option<i64>,
}

impl Stamp {
    pub(super) async fn capture(app: &impl LcHost, run: &db::Run) -> anyhow::Result<Self> {
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

// Ownership receipts only, not another status machine. A successor poller can inherit its
// predecessor's receipt; aborting the old task must not lose the eventual CAS restoration.
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
    // Scope #2003 to Claude. The parser's live footer check also excludes a quoted old spinner
    // in the reply/composer; is_screen_busy alone can find activity-shaped text elsewhere.
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
            // Read the trigger's timestamp in the same transaction. A failed read rolls the
            // status update back rather than leaving a working value without an owner.
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
    // A new in-flight turn owns the pane now; it can inherit the receipt on its first busy frame.
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
        // 輸入框裡可以有使用者還沒送出的草稿（2026-10-30 hub-8p5dpe）：只要求框在、沒有 spinner。
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
    // A newer turn may already have inherited the receipt: never erase that owner's entry.
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

// A distinct key in the existing receipt table keeps a late working poller from retiring
// an independently observed menu receipt. The suffix of `turn` is the consecutive-clear count.
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
        key.strip_prefix("menu:").is_none_or(|run| active.iter().any(|id| id == run))
    });
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
    let result = observe_menu_inner(app, run).await;
    if result.is_err() { reset_menu_clear_count(&menu_key(&run.id)); }
    result
}

async fn observe_menu_inner(app: &impl LcHost, run: &db::Run) -> anyhow::Result<bool> {
    let key = menu_key(&run.id);
    if run.state != "running" || !matches!(run.agent_status.as_str(), "idle" | "unknown" | "blocked") { return Ok(false) }
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
        // An existing dialog owner now controls this pane. Never restore its status later.
        if let Some(owner) = receipt(&key) { forget(&key, &owner); }
        return Ok(true);
    }
    let open = super::super::delivery::claude_open_choice_menu(&plain);
    let clear = !open && (super::pane_awaits_input("claude", &plain) || crate::capture::claude::PARSER.still_busy(&plain));
    if !open && !clear { reset_menu_clear_count(&key); return Ok(true); }

    let lock = app.bot_lock(&run.bot_id).await;
    let Ok(guard) = lock.try_lock() else { reset_menu_clear_count(&key); return Ok(true) };
    let changed = client.fence.run_current(Box::pin(async {
        if db::bot_host(app.db(), &run.bot_id).await? != client.fence.conn().name { reset_menu_clear_count(&key); return Ok::<_, anyhow::Error>(false) }
        let Some(fresh) = db::run(app.db(), &run.id).await? else { reset_menu_clear_count(&key); return Ok(false) };
        if !stamp.matches(app, &fresh).await? { reset_menu_clear_count(&key); return Ok(false) }
        let prior = receipt(&key);
        if let Some(ref owner) = prior {
            if fresh.agent_status != "blocked" || fresh.agent_status_since != owner.since
                || !owner.authority.matches(&client.fence) || !owner.stamp.matches(app, &fresh).await? {
                forget(&key, owner);
                return Ok(false);
            }
        }
        if open {
            if prior.is_some() { reset_menu_clear_count(&key); return Ok(false) }
            if !matches!(fresh.agent_status.as_str(), "idle" | "unknown") { return Ok(false) }
            let Some(turn) = eligible_menu_turn(app, &fresh).await? else { return Ok(false) };
            #[cfg(all(test, feature = "daemon-test-harness"))]
            super::super::race_point::hit("observed_menu_before_cas", &fresh.id).await;
            let mut tx = app.db().begin().await?;
            let applied = sqlx::query(
                "UPDATE runs SET agent_status='blocked' WHERE id=? AND state='running' AND ended_at IS NULL
                 AND agent_status=? AND agent_status_since IS ? AND pane_id IS ? AND herdr_session IS ?
                 AND native_session_id IS ? AND terminal_id IS ? AND host IS ? AND host_generation IS ?
                 AND EXISTS(SELECT 1 FROM bots b JOIN projects p ON p.id=b.project_id
                            WHERE b.id=runs.bot_id AND b.kind='claude' AND b.deleted_at IS NULL AND p.host=?)
                 AND ?=(SELECT id FROM turns WHERE run_id=runs.id ORDER BY created_at DESC,id DESC LIMIT 1)
                 AND EXISTS(SELECT 1 FROM turns t WHERE t.id=? AND t.run_id=runs.id AND (
                       (t.status='in_flight' AND t.delivery='ok') OR
                       (NOT EXISTS(SELECT 1 FROM turns live WHERE live.run_id=runs.id AND live.status='in_flight')
                        AND (t.status='completed_fallback' OR (t.status IN ('completed','failed') AND t.completed_at>=?)))))")
                .bind(&fresh.id).bind(&fresh.agent_status).bind(&fresh.agent_status_since)
                .bind(&stamp.run.pane_id).bind(&stamp.run.herdr_session).bind(&stamp.run.native_session_id)
                .bind(&stamp.run.terminal_id).bind(&stamp.host).bind(stamp.generation)
                .bind(&client.fence.conn().name).bind(&turn).bind(&turn).bind(db::iso_in(-1800)).execute(&mut *tx).await?.rows_affected() == 1;
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

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;
    use crate::capabilities::Db;
    use crate::testing as tt;

    const BUSY: &str = "❯ Work\n✻ Drizzling… (1m 24s · ↓ 6.2k tokens)\n──────\n❯\n──────\n";
    const IDLE: &str = "❯ Work\n⏺ Done\n✻ Cooked for 2s\n──────\n❯\n──────\n";
    const DRAFT: &str = "❯ Work\n⏺ Done\n✻ Cooked for 2s\n──────\n❯ ship it to prod now\n──────\n";

    struct Fixture {
        env: tt::Env,
        run: db::Run,
        turn: String,
        client: RunClient,
    }

    async fn fixture() -> Fixture {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "observed").await;
        let id = tt::fake_run(&env.app, &bot.id).await;
        let run = db::run(env.app.db(), &id).await.unwrap().unwrap();
        let conv = db::conversation_id(env.app.db(), &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(&turn).bind(conv).bind(&id).bind(db::now()).execute(env.app.db()).await.unwrap();
        let client = client_for_run(&env.app, &run).await.unwrap();
        env.herdr.agents.lock().unwrap().push(json!({
            "name": "agent", "agent": "claude", "pane_id": run.pane_id,
            "agent_status": "done", "revision": 1, "completion_seq": 30865,
        }));
        env.herdr.set_screen(run.pane_id.as_deref().unwrap(), BUSY);
        Fixture {
            env,
            run,
            turn,
            client,
        }
    }

    async fn status(f: &Fixture) -> String {
        db::run(f.env.app.db(), &f.run.id)
            .await
            .unwrap()
            .unwrap()
            .agent_status
    }

    async fn sample(f: &Fixture, text: &str) {
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        let stamp = Stamp::capture(&f.env.app, &run).await.unwrap();
        observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", text)
            .await
            .unwrap();
    }

    async fn complete(f: &Fixture) {
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&f.turn)
            .execute(f.env.app.db())
            .await
            .unwrap();
        f.env
            .herdr
            .set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
    }

    #[tokio::test]
    async fn p63_synthetic_in_flight_menu_is_blocked() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked", "in-flight menu should be visible");
    }

    #[tokio::test]
    async fn in_flight_menu_blocks_only_delivered_turn_and_clears_after_two_reads() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        sqlx::query("UPDATE turns SET delivery='pending' WHERE id=?").bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "idle");
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id=?").bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        let now = std::time::Instant::now();
        crate::lifecycle::stuck_turns::observe_at(&f.run.id, "idle", now - std::time::Duration::from_secs(600));
        let closed = crate::lifecycle::stuck_turns::sweep_at(&f.env.app, None, now, std::time::Duration::from_secs(300)).await;
        assert!(!closed.contains(&f.turn));
        assert!(db::in_flight_turn(f.env.app.db(), &f.run.id).await.unwrap().is_some());
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "__READ_ERROR__");
        observe_menu(&f.env.app, &run).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "unknown");
        assert_eq!(crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id), 0);
    }

    #[tokio::test]
    async fn in_flight_menu_receipt_cannot_restore_a_new_turn() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        complete(&f).await;
        let conversation = db::conversation_id(f.env.app.db(), &f.run.bot_id).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid()).bind(conversation).bind(&f.run.id).bind(db::iso_in(1))
            .execute(f.env.app.db()).await.unwrap();
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        assert!(!menu_owned(&f.run.id));
    }

    #[tokio::test]
    async fn delayed_in_flight_menu_read_cannot_block_a_new_turn() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let app = f.env.app.clone(); let turn = f.turn.clone(); let run_id = f.run.id.clone();
        let bot_id = f.run.bot_id.clone();
        crate::lifecycle::race_point::arm("observed_menu_before_cas", &f.run.id, move || async move {
            sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
                .bind(db::now()).bind(&turn).execute(app.db()).await.unwrap();
            let conversation = db::conversation_id(app.db(), &bot_id).await.unwrap();
            sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
                .bind(db::ulid()).bind(conversation).bind(&run_id).bind(db::iso_in(1))
                .execute(app.db()).await.unwrap();
        });
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "idle");
        assert!(!menu_owned(&f.run.id));
    }

    #[tokio::test]
    async fn in_flight_menu_allows_normal_turn_completion() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.live_pane(f.run.pane_id.as_deref().unwrap(), crate::testing::LivePane { width: Some(120), ..Default::default() });
        let queued = crate::lifecycle::prompt_from_api_queue_if_busy(
            &f.env.app, &f.run.bot_id, "Synthetic next message", "p63-queue", &[],
            crate::lifecycle::RelaySrc::trusted(None), false, None,
        ).await.unwrap();
        assert_eq!(queued.delivery, "queued");
        let hook = crate::hookrecv::HookBody {
            bot_id: f.run.bot_id.clone(), provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "last_assistant_message": "Synthetic completed answer"}),
            received_at: None, truncated: false, run_id: Some(f.run.id.clone()),
        };
        crate::hookrecv::process(&f.env.app, &hook).await.unwrap();
        assert!(db::in_flight_turn(f.env.app.db(), &f.run.id).await.unwrap().is_none());
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "unknown", "the queued turn must not invalidate the menu owner");
        // The daemon-test harness records queue wakeups rather than spawning them.
        crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id);
        let ev = crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"agent": "claude", "pane_id": f.run.pane_id, "agent_status": "idle"}),
        };
        crate::runners::events::handle_status(&f.env.app, "local", "test", &ev).await;
        assert_eq!(status(&f).await, "idle");
        assert!(crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id) > 0,
            "the observed idle transition must request a queue flush");
        crate::lifecycle::queue::flush_queued_locked(&f.env.app, &f.run.bot_id).await.unwrap();
        let delivered = tt::eventually!(db::in_flight_turn(f.env.app.db(), &f.run.id).await.unwrap()
            .is_some_and(|turn| turn.id == queued.turn_id && turn.delivery == "ok"));
        let row: (String, String, Option<String>, Option<String>, i64) = sqlx::query_as(
            "SELECT status,delivery,run_id,start_error,flush_retries FROM turns WHERE id=?")
            .bind(&queued.turn_id).fetch_one(f.env.app.db()).await.unwrap();
        eprintln!("queue metadata: {:?}, prompt calls: {}, pane send calls: {}", row,
            f.env.herdr.calls_to("agent.prompt").len(), f.env.herdr.calls_to("pane.send_text").len());
        assert!(delivered, "the exact queued turn must be delivered after the observed idle event");
    }

    #[tokio::test]
    async fn a_completed_fallback_with_a_live_menu_is_blocked_without_an_in_flight_turn() {
        let f = fixture().await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked", "a live menu must be visible after the turn is closed");
    }

    #[tokio::test]
    async fn menu_requires_two_fresh_clear_reads_and_releases_to_unknown() {
        let f = fixture().await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "__READ_ERROR__");
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked", "failed reads reset the consecutive count");
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "unknown");
        assert_eq!(crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id), 0);
    }

    #[tokio::test]
    async fn late_working_finish_does_not_remove_menu_receipt() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?")
            .bind(&f.run.id).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert!(menu_owned(&f.run.id));
        assert_eq!(status(&f).await, "blocked");
    }

    #[tokio::test]
    async fn menu_followed_by_working_also_releases_only_after_two_reads() {
        let f = fixture().await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), BUSY);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "unknown");
        assert_eq!(crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id), 0);
    }

    #[tokio::test]
    async fn a_new_turn_fences_the_old_menu_receipt() {
        let f = fixture().await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        let conversation = db::conversation_id(f.env.app.db(), &f.run.bot_id).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid()).bind(conversation).bind(&f.run.id).bind(db::iso_in(1))
            .execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        assert!(!menu_owned(&f.run.id));
    }

    #[tokio::test]
    async fn cached_idle_with_live_busy_is_working_then_completed_is_idle() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        assert_eq!(
            status(&f).await,
            "working",
            "Herdr's stale done must not hide a real busy pane"
        );
        complete(&f).await;
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "idle");
        assert_eq!(
            crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id),
            1,
            "restoring idle must wake the queue even without a Herdr idle edge"
        );
    }

    #[tokio::test]
    async fn an_unsent_draft_in_the_composer_does_not_block_the_restore() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        assert_eq!(status(&f).await, "working");
        complete(&f).await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), DRAFT);
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "idle", "composer draft must not keep the run working");
    }

    async fn stale_working(f: &Fixture, minutes_ago: i64, screen: &str) -> db::Run {
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&f.run.id).execute(f.env.app.db()).await.unwrap();
        let old = db::iso_in(-minutes_ago * 60);
        sqlx::query("UPDATE runs SET agent_status_since=? WHERE id=?").bind(&old).bind(&f.run.id).execute(f.env.app.db()).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?").bind(&old).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), screen);
        db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_run_stuck_at_working_after_its_turn_ended_is_settled_by_the_sweep() {
        let f = fixture().await;
        let run = stale_working(&f, 10, DRAFT).await;
        assert!(settle_stale_working(&f.env.app, &run).await.unwrap());
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn the_sweep_leaves_fresh_busy_or_in_flight_runs_alone() {
        let f = fixture().await;
        let run = stale_working(&f, 1, IDLE).await;
        assert!(!settle_stale_working(&f.env.app, &run).await.unwrap(), "not stale long enough");
        let run = stale_working(&f, 10, BUSY).await;
        assert!(!settle_stale_working(&f.env.app, &run).await.unwrap(), "spinner still on screen");
        let run = stale_working(&f, 10, IDLE).await;
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) SELECT ?,conversation_id,run_id,'web','in_flight','ok',? FROM turns WHERE id=?")
            .bind(db::ulid()).bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        assert!(!settle_stale_working(&f.env.app, &run).await.unwrap(), "turn still in flight");
        assert_eq!(status(&f).await, "working");
    }

    #[tokio::test]
    async fn unknown_can_be_repaired_but_a_new_working_status_is_not_ours_to_restore() {
        let f = fixture().await;
        sqlx::query("UPDATE runs SET agent_status='unknown' WHERE id=?")
            .bind(&f.run.id)
            .execute(f.env.app.db())
            .await
            .unwrap();
        sample(&f, BUSY).await;
        assert_eq!(status(&f).await, "working");
        assert_eq!(
            f.env.herdr.agents.lock().unwrap()[0]["agent_status"],
            "done"
        );
        sqlx::query("UPDATE runs SET agent_status_since='2099-01-01T00:00:00Z' WHERE id=?")
            .bind(&f.run.id)
            .execute(f.env.app.db())
            .await
            .unwrap();
        complete(&f).await;
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(
            status(&f).await,
            "working",
            "a fresher writer owns this status now"
        );
    }

    #[tokio::test]
    async fn the_real_progress_poller_repairs_and_restores_the_run() {
        let f = fixture().await;
        super::super::arm_progress(&f.env.app, &f.run.id, &f.run.bot_id, &f.turn).await;
        assert!(tt::eventually!(status(&f).await == "working"));
        let reads = f.env.herdr.calls_to("pane.read").len();
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&f.turn)
            .execute(f.env.app.db())
            .await
            .unwrap();
        assert!(tt::eventually!(
            f.env.herdr.calls_to("pane.read").len() >= reads + 2
        ));
        assert_eq!(
            status(&f).await,
            "working",
            "Stop alone cannot declare a still-busy pane idle"
        );
        f.env
            .herdr
            .set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        assert!(tt::eventually!(status(&f).await == "idle"));
        assert!(tt::eventually!(!f
            .env
            .app
            .progress_pollers
            .lock()
            .await
            .contains_key(&f.run.id)));
    }

    #[tokio::test]
    async fn blocked_unknown_delivery_and_menus_do_not_become_working() {
        for state in ["blocked", "idle"] {
            let f = fixture().await;
            sqlx::query("UPDATE runs SET agent_status=? WHERE id=?")
                .bind(state)
                .bind(&f.run.id)
                .execute(f.env.app.db())
                .await
                .unwrap();
            if state == "idle" {
                sqlx::query("UPDATE turns SET delivery='unknown' WHERE id=?")
                    .bind(&f.turn)
                    .execute(f.env.app.db())
                    .await
                    .unwrap();
            }
            sample(&f, BUSY).await;
            assert_eq!(status(&f).await, state);
        }
        let f = fixture().await;
        let dialog = format!(
            "✻ Drizzling… (1m 24s · ↓ 6.2k tokens)\n{}",
            include_str!("fixtures/claude-2.1.287-bash-permission.txt")
        );
        assert!(menu(&dialog));
        sample(&f, &dialog).await;
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn idle_text_and_quoted_activity_are_not_positive_evidence() {
        let f = fixture().await;
        let quoted = "❯ Work\n⏺ Example:\n  ✻ Drizzling… (1m 24s · ↓ 6.2k tokens)\n\n⏺ Done\n\n──────\n❯\n──────\n";
        for text in [IDLE, quoted] {
            sample(&f, text).await;
            assert_eq!(status(&f).await, "idle");
        }
        sample(&f, BUSY).await;
        complete(&f).await;
        f.env
            .herdr
            .set_screen(f.run.pane_id.as_deref().unwrap(), quoted);
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(
            status(&f).await,
            "idle",
            "quoted activity must not prevent restoration"
        );
    }

    #[tokio::test]
    async fn old_session_pane_host_generation_and_turn_snapshots_are_rejected() {
        for change in [
            "UPDATE runs SET native_session_id='new-session' WHERE id=?",
            "UPDATE runs SET herdr_session='other' WHERE id=?",
            "UPDATE runs SET pane_id='other-pane' WHERE id=?",
            "UPDATE runs SET host_generation=123 WHERE id=?",
        ] {
            let f = fixture().await;
            let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
            sqlx::query(change)
                .bind(&f.run.id)
                .execute(f.env.app.db())
                .await
                .unwrap();
            observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", BUSY)
                .await
                .unwrap();
            assert_eq!(status(&f).await, "idle", "{change}");
        }
        let f = fixture().await;
        let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
        f.env
            .app
            .hosts
            .bump_generation_for_test(&f.client.fence.conn().name)
            .await;
        observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", BUSY)
            .await
            .unwrap();
        assert_eq!(status(&f).await, "idle");

        let f = fixture().await;
        let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
        observe(
            &f.env.app,
            &f.client,
            &stamp,
            "another-turn",
            "claude",
            BUSY,
        )
        .await
        .unwrap();
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn stop_between_snapshot_and_status_write_cannot_resurrect_working() {
        let f = fixture().await;
        let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
        let (app, turn) = (f.env.app.clone(), f.turn.clone());
        crate::lifecycle::race_point::arm(
            "observed_status_before_write",
            &f.run.id,
            move || async move {
                sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
                    .bind(db::now())
                    .bind(turn)
                    .execute(app.db())
                    .await
                    .unwrap();
            },
        );
        observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", BUSY)
            .await
            .unwrap();
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn restoration_does_not_clobber_a_blocker_or_a_new_turn() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?")
            .bind(&f.run.id)
            .execute(f.env.app.db())
            .await
            .unwrap();
        complete(&f).await;
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "blocked");

        let f = fixture().await;
        sample(&f, BUSY).await;
        complete(&f).await;
        let conv = db::conversation_id(f.env.app.db(), &f.run.bot_id)
            .await
            .unwrap();
        let next = db::ulid();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(&next).bind(conv).bind(&f.run.id).bind(db::now()).execute(f.env.app.db()).await.unwrap();
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "working");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        let stamp = Stamp::capture(&f.env.app, &run).await.unwrap();
        observe(&f.env.app, &f.client, &stamp, &next, "claude", BUSY)
            .await
            .unwrap();
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&next)
            .execute(f.env.app.db())
            .await
            .unwrap();
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(
            status(&f).await,
            "working",
            "old poller cannot erase the successor's ownership"
        );
        finish(&f.env.app, &f.run.id, &next).await.unwrap();
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn pane_read_failure_does_not_publish_idle_and_database_failure_does_not_mark_busy() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        complete(&f).await;
        f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);
        assert!(finish(&f.env.app, &f.run.id, &f.turn).await.is_err());
        assert_eq!(status(&f).await, "working");
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "idle");

        let f = fixture().await;
        let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
        sqlx::query("ALTER TABLE turns RENAME TO turns_unreadable")
            .execute(f.env.app.db())
            .await
            .unwrap();
        assert!(
            observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", BUSY)
                .await
                .is_err()
        );
        sqlx::query("ALTER TABLE turns_unreadable RENAME TO turns")
            .execute(f.env.app.db())
            .await
            .unwrap();
        assert_eq!(status(&f).await, "idle");
    }
}
