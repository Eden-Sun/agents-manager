//! Reattach top-level bots after an unexpected restart of the local herdr server.
//!
//! The event stream is the restart signal. Before the ordinary reconcile pass can end its runs,
//! verify that none of their old panes still hosts an agent and reuse the native-resume path from
//! `herdr_upgrade`. A rolling circuit breaker limits repeated recovery attempts in an OOM loop.

use crate::{herdr::HerdrClient, state::App};
use futures::future::BoxFuture;
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

const ATTEMPT_WINDOW: Duration = Duration::from_secs(30 * 60);
const MAX_ATTEMPTS: usize = 3;
const BACKOFF: [Duration; MAX_ATTEMPTS] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// Lives for the lifetime of the global event watcher, so restarting herdr cannot reset its fuse.
#[derive(Default)]
pub(crate) struct RecoveryGuard {
    attempts: VecDeque<Instant>,
}

impl RecoveryGuard {
    fn delay(&mut self, now: Instant) -> Option<Duration> {
        while self
            .attempts
            .front()
            .is_some_and(|at| now.duration_since(*at) >= ATTEMPT_WINDOW)
        {
            self.attempts.pop_front();
        }
        BACKOFF.get(self.attempts.len()).copied()
    }

    fn record(&mut self, now: Instant) {
        self.attempts.push_back(now);
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Resumed {
    pub bot_id: String,
    pub name: String,
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Failed {
    pub bot_id: String,
    pub name: String,
    pub error: String,
}

/// Result of a confirmed full pane loss. `limited` means the fuse is open and no bot was started.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RecoveryReport {
    pub recovery_id: String,
    pub resumed: Vec<Resumed>,
    pub failed: Vec<Failed>,
    pub children_lost: Vec<crate::herdr_upgrade::LostChild>,
    pub limited: bool,
    pub skipped: Option<String>,
}

impl RecoveryReport {
    fn empty(children_lost: Vec<crate::herdr_upgrade::LostChild>) -> Self {
        Self {
            recovery_id: crate::db::ulid(),
            resumed: Vec::new(),
            failed: Vec::new(),
            children_lost,
            limited: false,
            skipped: None,
        }
    }
}

/// Tell the browser and AGM about a confirmed unexpected local herdr restart.
pub(crate) async fn publish_recovery_report(app: &Arc<App>, report: &RecoveryReport) {
    let data = serde_json::to_value(report).unwrap_or_default();
    app.emit("herdr_recovery_done", data).await;

    let resumed = report
        .resumed
        .iter()
        .map(|bot| bot.name.as_str())
        .collect::<Vec<_>>();
    let failed = report
        .failed
        .iter()
        .map(|bot| format!("{}（{}）", bot.name, bot.error))
        .collect::<Vec<_>>();
    let children = report
        .children_lost
        .iter()
        .map(|child| child.name.as_str())
        .collect::<Vec<_>>();
    let describe = |names: &[&str]| {
        if names.is_empty() {
            "無".to_string()
        } else {
            names.join("、")
        }
    };
    let detail = format!(
        "herdr 本機非預期重啟：頂層 bot 接回 {} 顆（{}），失敗 {} 顆（{}），遺失 child pane {} 個（{}）。{}{}",
        resumed.len(),
        describe(&resumed),
        failed.len(),
        if failed.is_empty() { "無".to_string() } else { failed.join("、") },
        children.len(),
        describe(&children),
        if report.limited { "已達自動接回上限。" } else { "" },
        report.skipped.as_deref().map_or(String::new(), |reason| format!("接回略過：{reason}。")),
    );
    let event_key = format!(
        "ops_alert:daemon:herdr_unexpected_restart:{}",
        report.recovery_id
    );
    let payload = serde_json::json!({
        "source": "daemon",
        "reason": "herdr_unexpected_restart",
        "subject": "local herdr",
        "recovery_id": report.recovery_id,
        "resumed_count": report.resumed.len(),
        "failed_count": report.failed.len(),
        "children_lost_count": report.children_lost.len(),
        "limited": report.limited,
        "skipped": report.skipped,
        "detail": detail,
        "action": "請檢查 herdr 與 bot 狀態；遺失的 child pane 不會由 daemon 重建。",
    });
    match crate::supervisor::store::push_inbox(
        &app.db,
        &event_key,
        "ops_alert",
        None,
        None,
        None,
        &payload,
    )
    .await
    {
        Ok(Some(_)) => {
            app.emit(
                "supervisor_changed",
                serde_json::json!({"ops_alert": event_key}),
            )
            .await
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(recovery_id = %report.recovery_id, error = ?error, "could not queue the herdr recovery ops_alert")
        }
    }
}

/// Called only after the local main event stream was established, dropped, then re-established.
/// Returning `None` means there was no evidence of a full server restart or maintenance owns it.
pub(crate) fn recover_after_reconnect<'a>(
    app: &'a Arc<App>,
    client: &'a HerdrClient,
    guard: &'a mut RecoveryGuard,
) -> BoxFuture<'a, Option<RecoveryReport>> {
    Box::pin(recover_after_reconnect_inner(app, client, guard))
}

async fn recover_after_reconnect_inner(
    app: &Arc<App>,
    client: &HerdrClient,
    guard: &mut RecoveryGuard,
) -> Option<RecoveryReport> {
    if maintenance_owns_reconnect(app).await {
        tracing::info!(
            "herdr recovery skipped: a herdr update or maintenance window owns this reconnect"
        );
        return None;
    }

    // Recovery must win the reconnect race with reconcile. The global watcher calls us before its
    // own reconcile pass; sharing the pass lock also keeps a periodic/deferred pass from retiring
    // the old runs while the recovery backoff is waiting.
    let pass_lock = crate::reconcile::host_pass_lock(app.as_ref(), crate::config::LOCAL_HOST).await;
    let _pass = pass_lock.lock().await;

    let first_plan = match crate::herdr_upgrade::affected(app, crate::config::LOCAL_HOST).await {
        Ok(plan) if !plan.resume.is_empty() || !plan.children.is_empty() => plan,
        Ok(_) => return None,
        Err(error) => {
            tracing::warn!(error = ?error, "herdr recovery could not read active bot runs");
            return None;
        }
    };
    match all_run_panes_are_unoccupied(app, client, &first_plan).await {
        Ok(true) => {}
        Ok(false) => return None,
        Err(error) => {
            tracing::warn!(error = ?error, "herdr recovery could not inspect run panes after reconnect");
            return None;
        }
    }

    let mut report = RecoveryReport::empty(first_plan.children);
    if first_plan.resume.is_empty() {
        // There are no top-level bots to recreate; the normal reconcile pass retires the children.
        return Some(report);
    }

    let now = Instant::now();
    let Some(delay) = guard.delay(now) else {
        report.limited = true;
        tracing::error!("herdr recovery circuit open: already attempted {MAX_ATTEMPTS} recoveries in 30 minutes");
        return Some(report);
    };
    let delay = if cfg!(test) {
        Duration::from_millis(10)
    } else {
        delay
    };
    tokio::time::sleep(delay).await;

    // Recheck after backoff: an update may have opened its maintenance window or another actor
    // may have restored panes while we waited. Never race that work with a native resume.
    if maintenance_owns_reconnect(app).await {
        report.skipped = Some("maintenance_started_during_backoff".into());
        return Some(report);
    }
    let plan = match crate::herdr_upgrade::affected(app, crate::config::LOCAL_HOST).await {
        Ok(plan) => plan,
        Err(error) => {
            report.skipped = Some(format!("active_runs_unavailable:{error}"));
            return Some(report);
        }
    };
    report.children_lost = plan.children.clone();
    if plan.resume.is_empty() {
        report.skipped = Some("no_top_level_runs_remain".into());
        return Some(report);
    }
    match all_run_panes_are_unoccupied(app, client, &plan).await {
        Ok(true) => {}
        Ok(false) => {
            report.skipped = Some("active_run_panes_still_host_agents".into());
            return Some(report);
        }
        Err(error) => {
            report.skipped = Some(format!("pane_list_failed:{error}"));
            return Some(report);
        }
    }

    // Count the episode before attempting the first bot; a partial batch followed by another OOM
    // must consume a strike as well.
    guard.record(Instant::now());
    tracing::warn!(
        bots = plan.resume.len(),
        children = report.children_lost.len(),
        delay_secs = delay.as_secs(),
        "herdr server restarted unexpectedly; reattaching top-level bots"
    );
    for bot in plan.resume {
        match crate::app_ports_p12::restart_bot_resume_native(app, &bot.bot_id).await {
            Ok(run_id) => report.resumed.push(Resumed {
                bot_id: bot.bot_id,
                name: bot.name,
                run_id,
            }),
            Err(error) => report.failed.push(Failed {
                bot_id: bot.bot_id,
                name: bot.name,
                error,
            }),
        }
    }
    Some(report)
}

/// A restarted herdr server can restore the old pane ids as empty shells. Only recover when every
/// active run that the plan found has a known old pane and none of those panes currently hosts an
/// agent. A still occupied pane means this reconnect did not lose the whole bot set (or a pane was
/// reused), so restarting would risk interrupting a live agent.
async fn all_run_panes_are_unoccupied(
    app: &Arc<App>,
    client: &HerdrClient,
    plan: &crate::herdr_upgrade::Affected,
) -> anyhow::Result<bool> {
    let bot_ids = plan
        .resume
        .iter()
        .map(|bot| bot.bot_id.as_str())
        .chain(plan.children.iter().map(|child| child.bot_id.as_str()))
        .collect::<std::collections::HashSet<_>>();
    if bot_ids.is_empty() {
        return Ok(false);
    }

    let runs = crate::db::all_active_runs(&app.db).await?;
    let runs = runs
        .iter()
        .filter(|run| bot_ids.contains(run.bot_id.as_str()))
        .collect::<Vec<_>>();
    if runs.len() != bot_ids.len()
        || runs
            .iter()
            .any(|run| run.pane_id.as_deref().is_none_or(str::is_empty))
    {
        return Ok(false);
    }

    let panes = client.pane_list(None).await?;
    for run in runs {
        let pane_id = run.pane_id.as_deref().expect("checked above");
        if panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .is_some_and(|pane| pane.agent.is_some())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn maintenance_owns_reconnect(app: &Arc<App>) -> bool {
    if crate::herdr_upgrade::is_running(app, crate::config::LOCAL_HOST) {
        return true;
    }
    match crate::runners::herdr_maintenance::active(app).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(error = ?error, "herdr recovery skipped: could not read maintenance state");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    #[test]
    fn recovery_backoff_is_bounded_and_resets_after_the_window() {
        let start = Instant::now();
        let mut guard = RecoveryGuard::default();
        assert_eq!(guard.delay(start), Some(BACKOFF[0]));
        guard.record(start);
        assert_eq!(
            guard.delay(start + Duration::from_secs(1)),
            Some(BACKOFF[1])
        );
        guard.record(start + Duration::from_secs(1));
        assert_eq!(
            guard.delay(start + Duration::from_secs(2)),
            Some(BACKOFF[2])
        );
        guard.record(start + Duration::from_secs(2));
        assert_eq!(guard.delay(start + Duration::from_secs(3)), None);
        assert_eq!(
            guard.delay(start + ATTEMPT_WINDOW + Duration::from_secs(2)),
            Some(BACKOFF[0])
        );
    }

    #[tokio::test]
    async fn full_pane_loss_resumes_only_the_parent_and_notifies_the_browser_and_agm() {
        let env = tt::env().await;
        let parent = tt::claude_bot(&env.app, &env.project_id, "top-level").await;
        sqlx::query(
            "UPDATE bots SET kind='codex', model='gpt-6.1-sol', auto_approve=1 WHERE id = ?",
        )
        .bind(&parent.id)
        .execute(&env.app.db)
        .await
        .unwrap();
        let parent_run = tt::fake_run(&env.app, &parent.id).await;
        sqlx::query(
            "UPDATE runs SET native_session_id = 'codex-session-before-restart' WHERE id = ?",
        )
        .bind(&parent_run)
        .execute(&env.app.db)
        .await
        .unwrap();

        let child_id = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok','child',?,?)",
        )
        .bind(&child_id)
        .bind(&env.project_id)
        .bind("top-level-worker")
        .bind(&parent.id)
        .bind(crate::db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        let _child_run = tt::fake_run(&env.app, &child_id).await;

        let mut events = env.app.subscribe();
        let mut guard = RecoveryGuard::default();
        let report = recover_after_reconnect(&env.app, &env.app.herdr, &mut guard)
            .await
            .expect("confirmed full pane loss");

        assert_eq!(report.resumed.len(), 1);
        assert_eq!(report.resumed[0].bot_id, parent.id);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.children_lost.len(), 1);
        assert_eq!(report.children_lost[0].bot_id, child_id);
        let starts = env.herdr.calls_to("agent.start");
        assert_eq!(
            starts.len(),
            1,
            "child agent is never started independently: {starts:?}"
        );
        assert!(starts[0]["name"]
            .as_str()
            .is_some_and(|name| !name.is_empty()));
        let start_args = starts[0]["args"].as_array().unwrap();
        assert_eq!(
            &start_args[..2],
            ["resume", "codex-session-before-restart"],
            "the top-level bot reuses its prior native Codex session"
        );

        publish_recovery_report(&env.app, &report).await;
        let alerts: Vec<(String, String)> = sqlx::query_as(
            "SELECT event_key, payload_json FROM supervisor_inbox WHERE kind='ops_alert'",
        )
        .fetch_all(&env.app.db)
        .await
        .unwrap();
        assert_eq!(alerts.len(), 1);
        let (event_key, payload) = &alerts[0];
        let payload: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert!(event_key.contains(&report.recovery_id));
        assert_eq!(payload["reason"], "herdr_unexpected_restart");
        assert_eq!(payload["resumed_count"], 1);
        assert_eq!(payload["children_lost_count"], 1);
        assert!(payload["detail"].as_str().unwrap().contains(&parent.name));

        let mut saw_browser_notice = false;
        let mut saw_agm_refresh = false;
        while let Ok(event) = events.try_recv() {
            saw_browser_notice |= event.kind == "herdr_recovery_done"
                && event.data["recovery_id"] == report.recovery_id;
            saw_agm_refresh |= event.kind == "supervisor_changed";
        }
        assert!(
            saw_browser_notice,
            "browser event must carry the recovery report"
        );
        assert!(
            saw_agm_refresh,
            "AGM inbox insert invalidates the open AGM panel"
        );
    }

    #[tokio::test]
    async fn a_restarted_herdr_with_restored_empty_panes_resumes_and_closes_the_old_shell() {
        let env = tt::env().await;
        let parent = tt::claude_bot(&env.app, &env.project_id, "top-level").await;
        sqlx::query(
            "UPDATE bots SET kind='codex', model='gpt-6.1-sol', auto_approve=1 WHERE id = ?",
        )
        .bind(&parent.id)
        .execute(&env.app.db)
        .await
        .unwrap();
        let run_id = tt::fake_run(&env.app, &parent.id).await;
        sqlx::query(
            "UPDATE runs SET native_session_id = 'codex-session-before-restart' WHERE id = ?",
        )
        .bind(&run_id)
        .execute(&env.app.db)
        .await
        .unwrap();

        // Simulate the real restart shape: herdr restores its workspace/tab/pane ids, but the
        // agent that used to occupy the run's pane has died and the pane is now an empty shell.
        let (workspace, old_pane) = env
            .app
            .herdr
            .workspace_create("/tmp/p", "restart recovery", serde_json::json!({}))
            .await
            .unwrap();
        let project = crate::db::project(&env.app.db, &env.project_id)
            .await
            .unwrap()
            .unwrap();
        let agent_name = crate::config::agent_name(&project.label, &parent.id);
        sqlx::query("UPDATE projects SET workspace_id = ? WHERE id = ?")
            .bind(&workspace.workspace_id)
            .bind(&env.project_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET workspace_id = ?, pane_id = ?, tab_id = ?, agent_name = ? WHERE id = ?")
            .bind(&workspace.workspace_id)
            .bind(&old_pane.pane_id)
            .bind(&old_pane.tab_id)
            .bind(&agent_name)
            .bind(&run_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        env.herdr.set_agent(&agent_name, &old_pane.pane_id, false);
        let plan = crate::herdr_upgrade::affected(&env.app, crate::config::LOCAL_HOST)
            .await
            .unwrap();
        assert!(
            !all_run_panes_are_unoccupied(&env.app, &env.app.herdr, &plan)
                .await
                .unwrap(),
            "a pane that still hosts an agent is not restart evidence"
        );
        env.herdr
            .agents
            .lock()
            .unwrap()
            .retain(|agent| agent["pane_id"].as_str() != Some(&old_pane.pane_id));

        let panes_after_restart = env.app.herdr.pane_list(None).await.unwrap();
        assert!(panes_after_restart
            .iter()
            .any(|pane| pane.pane_id == old_pane.pane_id));
        assert!(panes_after_restart
            .iter()
            .find(|pane| pane.pane_id == old_pane.pane_id)
            .is_some_and(|pane| pane.agent.is_none()));

        let mut guard = RecoveryGuard::default();
        let report = recover_after_reconnect(&env.app, &env.app.herdr, &mut guard)
            .await
            .expect("restored empty pane ids prove the agent processes were lost");
        assert_eq!(report.resumed.len(), 1);
        assert_eq!(report.resumed[0].bot_id, parent.id);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let starts = env.herdr.calls_to("agent.start");
        assert_eq!(starts.len(), 1, "the original native session is resumed");
        assert_eq!(
            &starts[0]["args"].as_array().unwrap()[..2],
            ["resume", "codex-session-before-restart"]
        );

        let panes_after_recovery = env.app.herdr.pane_list(None).await.unwrap();
        assert!(
            panes_after_recovery
                .iter()
                .all(|pane| pane.pane_id != old_pane.pane_id),
            "restart stop closes the restored empty shell before opening a new pane"
        );
        assert!(env
            .herdr
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(method, args)| {
                method == "pane.close" && args["pane_id"] == old_pane.pane_id
            }));

        // This is the same order as the global event loop: recovery finishes before reconnect
        // reconcile. The subsequent pass sees the resumed agent and keeps its new run alive.
        crate::reconcile::reconcile_host(&env.app, crate::config::LOCAL_HOST)
            .await
            .unwrap();
        let active = crate::db::active_run(&env.app.db, &parent.id)
            .await
            .unwrap()
            .expect("reconcile must keep the resumed run");
        assert_eq!(active.state, "running");
        assert_ne!(active.pane_id.as_deref(), Some(old_pane.pane_id.as_str()));
    }
}
