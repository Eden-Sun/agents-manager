//! Reconciliation (SPEC §6.5): runs at daemon start, after every event-stream reconnect,
//! and after a remote host's ssh master comes back (SPEC §11.3.4).
//!
//! Reconciliation is always scoped to **one host** — pane / workspace / agent ids are only
//! unique within a host's herdr session.

use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

/// Reconcile every known host.
#[allow(dead_code)]
pub async fn reconcile(app: &Arc<App>) -> Result<()> {
    let mut first_err = None;
    for host in app.hosts.names().await {
        if let Err(e) = reconcile_host(app, &host).await {
            tracing::error!(host = %host, error = ?e, "reconcile failed");
            if host == LOCAL_HOST && first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A progress poller only ever starts when we deliver a prompt or notice a CLI-side turn, so a
/// Turn that outlives a daemon restart is left with none: no live bubble, and — since the
/// poller is also what notices an agent sitting at an empty composer — nothing to complete it
/// if its hook never arrives. Re-arm every in-flight Turn once the runs are adopted.
pub async fn rearm_progress(app: &Arc<App>) {
    let runs = match crate::db::all_active_runs(&app.db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = ?e, "cannot re-arm progress pollers");
            return;
        }
    };
    for run in runs {
        if let Ok(Some(turn)) = crate::db::in_flight_turn(&app.db, &run.id).await {
            tracing::info!(run = %run.id, turn = %turn.id, "re-arming progress poller after restart");
            crate::lifecycle::arm_progress(app, &run.id, &run.bot_id, &turn.id).await;
        }
    }
}

pub async fn reconcile_host(app: &Arc<App>, host: &str) -> Result<()> {
    let Some(session) = app.session_for_host(host).await else {
        anyhow::bail!("unknown host `{host}`");
    };
    let Some(client) = app.herdr_for_session(host, &session).await else {
        anyhow::bail!("unknown host `{host}`");
    };
    // v4.0: refresh the GitHub-origin cache for this host's projects (off-path).
    crate::github::spawn_detect_host(app.clone(), host.to_string());
    let snapshot = client.snapshot().await?;
    // A1: never reconcile against an empty list. A transient RPC failure would otherwise look
    // like "no agents on this host" and mark every active Run `exited` (failing their turns).
    let agents = client.agent_list().await.map_err(|e| {
        anyhow::anyhow!("agent.list failed on host `{host}`: {e}; skipping reconcile so runs are not falsely exited")
    })?;
    let by_name: HashMap<String, &crate::herdr::AgentInfo> =
        agents.iter().filter_map(|a| a.name.clone().map(|n| (n, a))).collect();

    let live_ws: Vec<String> = snapshot
        .get("workspaces")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|w| w.get("workspace_id").and_then(|s| s.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    let live_panes: Vec<String> = snapshot
        .get("panes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|p| p.get("pane_id").and_then(|s| s.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    let panes_with_agent: Vec<String> = snapshot
        .get("panes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|p| p.get("agent").map(|g| !g.is_null()).unwrap_or(false))
                .filter_map(|p| p.get("pane_id").and_then(|s| s.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Drop workspace mappings that no longer exist.
    for p in db::live_projects(&app.db).await?.into_iter().filter(|p| p.host == host) {
        if let Some(ws) = p.workspace_id.as_deref() {
            if !live_ws.contains(&ws.to_string()) {
                sqlx::query("UPDATE projects SET workspace_id=NULL WHERE id=?").bind(&p.id).execute(&app.db).await?;
                tracing::info!(host, project = %p.label, "workspace disappeared; mapping cleared");
            }
        }
    }

    let bots = db::live_bots_on_host(&app.db, host).await?;
    for bot in bots {
        // A local bot imported from the user's default session is reconciled by
        // default_session::sync; it must not be marked exited because it is absent from the
        // manager's named-session agent.list.
        if bot.herdr_session.as_deref().map(|s| s != session).unwrap_or(false) {
            continue;
        }
        let lock = app.bot_lock(&bot.id).await;
        let _g = lock.lock().await;
        let active = db::active_run(&app.db, &bot.id).await?;
        if active
            .as_ref()
            .and_then(|r| r.herdr_session.as_deref())
            .map(|s| s != session)
            .unwrap_or(false)
        {
            continue;
        }
        // Names this bot may be running under: the run's recorded name, the current
        // `<project>-<bot>` scheme, and the legacy bare bot name (runs from before v3.5).
        let computed = db::agent_name_for_bot(&app.db, &bot).await?;
        let mut candidates: Vec<String> = Vec::new();
        if let Some(r) = &active {
            if let Some(n) = r.agent_name.clone().filter(|s| !s.is_empty()) {
                candidates.push(n);
            }
        }
        let label: String = sqlx::query_scalar("SELECT label FROM projects WHERE id = ?")
            .bind(&bot.project_id)
            .fetch_optional(&app.db)
            .await?
            .unwrap_or_default();
        let legacy = crate::config::agent_name_legacy(&label, &bot.name);
        for n in [computed.clone(), legacy, bot.name.clone()] {
            if !candidates.contains(&n) {
                candidates.push(n);
            }
        }
        let mut found: Option<crate::herdr::AgentInfo> = None;
        let mut found_name: Option<String> = None;
        for n in &candidates {
            if let Some(a) = by_name.get(n) {
                found = Some((*a).clone());
                found_name = Some(n.clone());
                break;
            }
        }
        // The snapshot above was taken *before* this bot's lock was acquired, so a Run that
        // started in the meantime would look dead. Re-check that one agent under the lock.
        if found.is_none() && active.is_some() {
            for n in &candidates {
                if let Some(a) = client.agent_get(n).await.ok().flatten() {
                    tracing::debug!(host, bot = %bot.name, agent = %n, "reconcile: agent appeared after the snapshot");
                    found = Some(a);
                    found_name = Some(n.clone());
                    break;
                }
            }
        }
        match (active, found.as_ref()) {
            (Some(run), Some(agent)) => {
                let agent: &crate::herdr::AgentInfo = agent;
                // Still alive: refresh pane_id (pane move changes it) and status.
                //
                // `tab_id` is refreshed the same way, from herdr rather than from what we
                // recorded. That is what makes old and new runs coexist: liveness is decided
                // by the agent's *name*, never by whether the run has a tab of its own, so a
                // bot started the old way — `pane.split` into a shared tab, `tab_id` NULL —
                // is kept exactly as before and simply learns where it is sitting.
                let status = agent.agent_status.normalized().as_str().to_string();
                sqlx::query("UPDATE runs SET pane_id=?, workspace_id=?, tab_id=?, agent_status=?, agent_name=COALESCE(?, agent_name), herdr_session=COALESCE(herdr_session, ?), state=CASE WHEN state='starting' THEN 'running' ELSE state END WHERE id=?")
                    .bind(&agent.pane_id)
                    .bind(&agent.workspace_id)
                    .bind(&agent.tab_id)
                    .bind(&status)
                    .bind(&found_name)
                    .bind(&session)
                    .bind(&run.id)
                    .execute(&app.db)
                    .await?;
                if run.pane_id.as_deref() != Some(agent.pane_id.as_str()) {
                    if let Some(old) = run.pane_id.as_deref() {
                        crate::events::unwatch_pane_on_session(app, host, &session, old).await;
                    }
                }
                crate::events::watch_pane_on_session(app, host, &session, &agent.pane_id).await;
                if bot.kind == "codex" {
                    crate::lifecycle::schedule_codex_notice_capture(app, &bot.id, &run.id);
                }
                tracing::info!(host, bot = %bot.name, run = %run.id, pane = %agent.pane_id, "reconcile: kept active run");
            }
            (Some(run), None) => {
                tracing::info!(host, bot = %bot.name, run = %run.id, "reconcile: agent gone, marking run exited");
                crate::lifecycle::mark_run_exited(app, &run.id, "agent not found during reconcile").await;
            }
            (None, Some(agent)) => {
                let run_id = db::ulid();
                let status = agent.agent_status.normalized().as_str().to_string();
                sqlx::query(
                    "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, started_at)
                     VALUES (?,?,'running',?,?,?,?,1,?,?,?)",
                )
                .bind(&run_id)
                .bind(&bot.id)
                .bind(&status)
                .bind(&agent.workspace_id)
                .bind(&agent.pane_id)
                .bind(&agent.tab_id)
                .bind(&found_name)
                .bind(&session)
                .bind(db::now())
                .execute(&app.db)
                .await?;
                // Make sure the project keeps pointing at the workspace the agent lives in.
                sqlx::query("UPDATE projects SET workspace_id=? WHERE id=? AND workspace_id IS NULL")
                    .bind(&agent.workspace_id)
                    .bind(&bot.project_id)
                    .execute(&app.db)
                    .await?;
                crate::events::watch_pane_on_session(app, host, &session, &agent.pane_id).await;
                if bot.kind == "codex" {
                    crate::lifecycle::schedule_codex_notice_capture(app, &bot.id, &run_id);
                }
                tracing::info!(host, bot = %bot.name, run = %run_id, pane = %agent.pane_id, "reconcile: adopted existing agent");
            }
            (None, None) => {}
        }
        app.emit_bot_status(&bot.id).await;
    }

    // Orphan pane recovery: panes we created for finished runs that no longer host an agent.
    // Carries the run's tab along, so a tab left empty by the reclaimed pane goes with it.
    let dead: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT r.pane_id, r.tab_id, r.workspace_id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
         WHERE r.pane_id IS NOT NULL AND p.host = ? AND r.state IN ('exited','stopped')
         AND COALESCE(r.herdr_session, ?) = ?
         AND r.pane_id NOT IN (
           SELECT r2.pane_id FROM runs r2 JOIN bots b2 ON b2.id = r2.bot_id JOIN projects p2 ON p2.id = b2.project_id
           WHERE r2.pane_id IS NOT NULL AND p2.host = ? AND r2.state IN ('starting','running','stopping')
           AND COALESCE(r2.herdr_session, ?) = ?)",
    )
    .bind(host)
    .bind(&session)
    .bind(&session)
    .bind(host)
    .bind(&session)
    .bind(&session)
    .fetch_all(&app.db)
    .await?;
    for (pane, tab, ws) in dead {
        if live_panes.contains(&pane) && !panes_with_agent.contains(&pane) {
            tracing::info!(host, pane_id = %pane, "reconcile: closing orphan pane");
            crate::lifecycle::close_pane_and_tab(&client, ws.as_deref(), tab.as_deref(), &pane).await;
        }
    }
    // SPEC-team §6.5 / §6.4a: a team's worktrees and its workspace are checked on *every*
    // reconcile pass, not only at boot — a worktree the user deleted by hand, or a closed
    // workspace, pauses the team where they can see it. Scoped to this host's teams, since
    // reconcile is always per-host (pane / workspace ids are only unique within a session).
    crate::team::reconcile_teams_on_host(app, host).await;
    Ok(())
}

#[cfg(test)]
mod compat_tests {
    //! Old and new bots side by side (SPEC §6.5).
    //!
    //! Every bot that was running when one-bot-one-tab landed was `pane.split` into a shared
    //! tab and has `runs.tab_id` NULL. The reconcile must go on adopting them exactly as
    //! before: an agent is alive because herdr still lists it under one of its names, never
    //! because the run has a tab of its own.
    use crate::db;
    use crate::state::App;
    use crate::team::testing as tt;
    use serde_json::json;
    use std::sync::Arc;

    async fn a_bot(env: &tt::Env, name: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?)",
        )
        .bind(&id)
        .bind(&env.project_id)
        .bind(name)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        id
    }

    async fn run_of(app: &Arc<App>, bot_id: &str) -> Option<db::Run> {
        sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE bot_id = ? ORDER BY started_at DESC LIMIT 1")
            .bind(bot_id)
            .fetch_optional(&app.db)
            .await
            .unwrap()
    }

    /// **The compatibility guarantee.** A run recorded the old way — a pane split into a tab
    /// it shares, `tab_id` NULL — is kept running and simply learns which tab it is sitting
    /// in. It is emphatically not marked `exited` for lacking a tab of its own.
    #[tokio::test]
    async fn a_split_pane_from_before_tabs_is_kept_and_learns_its_tab() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        // Two bots sharing one tab, the way every bot started before this change was.
        let old_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot);
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'test',?)",
        )
        .bind(&run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&old_pane.pane_id)
        .bind(&agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        assert!(run_of(&app, &bot).await.unwrap().tab_id.is_none(), "precondition: no tab of its own");

        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": agent, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": old_pane.tab_id, "pane_id": old_pane.pane_id,
            "cwd": "/tmp/p"})];

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let r = run_of(&app, &bot).await.unwrap();
        assert_eq!(r.id, run, "the same run, not a replacement");
        assert_eq!(r.state, "running", "an old split pane is still a live bot");
        assert!(r.ended_at.is_none());
        assert_eq!(r.pane_id.as_deref(), Some(old_pane.pane_id.as_str()));
        assert_eq!(r.tab_id.as_deref(), Some(old_pane.tab_id.as_str()), "tab_id is backfilled from herdr");
        // And the shared tab is untouched — the reconcile closes orphan *panes*, and this
        // pane is not orphaned.
        assert!(env.herdr.tab(&old_pane.tab_id).unwrap().panes.contains(&old_pane.pane_id));
    }

    /// An agent herdr knows about that the database has no run for is adopted, tab and all —
    /// so a bot the daemon lost track of comes back with its tab recorded, not NULL.
    #[tokio::test]
    async fn an_adopted_agent_records_the_tab_it_is_in() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot);
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": agent, "agent": "claude", "agent_status": "working",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id,
            "cwd": "/tmp/p"})];

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let r = run_of(&app, &bot).await.expect("the agent was adopted");
        assert_eq!(r.state, "running");
        assert_eq!(r.adopted, 1);
        assert_eq!(r.pane_id.as_deref(), Some(pane.pane_id.as_str()));
        assert_eq!(r.tab_id.as_deref(), Some(pane.tab_id.as_str()));
    }

    /// The unhappy path is unchanged: a run whose agent herdr no longer lists is exited,
    /// whether or not it had a tab. (The guard here is that "old style" must not become a
    /// second, silent reason to kill a run.)
    #[tokio::test]
    async fn a_run_whose_agent_is_gone_still_exits() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();

        let bot = a_bot(&env, "alfa").await;
        let ghost = a_bot(&env, "bravo").await;
        let live_agent = crate::config::agent_name("proj", &bot);
        for (b, pane) in [(&bot, &root.pane_id), (&ghost, &root.pane_id)] {
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
                 VALUES (?,?,'running','idle',?,?,?,'test',?)",
            )
            .bind(db::ulid())
            .bind(b)
            .bind(&ws.workspace_id)
            .bind(pane)
            .bind(crate::config::agent_name("proj", b))
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        }
        // herdr lists only one of the two.
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": live_agent, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id,
            "cwd": "/tmp/p"})];

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        assert_eq!(run_of(&app, &bot).await.unwrap().state, "running");
        assert_eq!(run_of(&app, &ghost).await.unwrap().state, "exited");
    }
}
