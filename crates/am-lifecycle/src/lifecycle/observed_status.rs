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
        || !super::pane_awaits_input("claude", &screen.text)
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

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;
    use crate::capabilities::Db;
    use crate::testing as tt;

    const BUSY: &str = "❯ Work\n✻ Drizzling… (1m 24s · ↓ 6.2k tokens)\n──────\n❯\n──────\n";
    const IDLE: &str = "❯ Work\n⏺ Done\n✻ Cooked for 2s\n──────\n❯\n──────\n";

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
