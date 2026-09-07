//! Reconciliation (SPEC §6.5): runs at daemon start, after every event-stream reconnect,
//! and after a remote host's ssh master comes back (SPEC §11.3.4).
//!
//! Reconciliation is always scoped to **one host** — pane / workspace / agent ids are only
//! unique within a host's herdr session.

use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;
use anyhow::Result;
use serde_json::json;
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

/// A bot that can own children in this pass: the herdr name it is running under and — one
/// bot, one tab — the tab its live agent is sitting in. The tab is what makes descent
/// observable: whatever a spawned agent called itself, it is in its parent's tab.
struct Parent {
    agent_name: String,
    tab_id: Option<String>,
    bot: db::Bot,
}

/// How long `name` matches `parent` as a `<parent>-<suffix>` prefix; 0 when it does not.
fn prefix_score(parent: &str, name: &str) -> usize {
    if name.len() > parent.len() + 1 && name.starts_with(parent) && name.as_bytes()[parent.len()] == b'-' {
        parent.len()
    } else {
        0
    }
}

/// A child claimed by descent has no prefix to strip, so its bot name is herdr's agent name
/// with the characters `valid_bot_name` forbids dropped and the rest cut to 32.
fn child_name_from_agent(name: &str) -> String {
    let cleaned: String =
        name.chars().filter(|c| !c.is_whitespace() && !matches!(c, '@' | ',' | ':' | ';')).take(32).collect();
    if cleaned.is_empty() {
        "child".to_string()
    } else {
        cleaned
    }
}

/// Run ids whose remote hook material was already rewritten by this daemon process.
/// Returns `true` the first time a run id is seen.
fn mark_hook_refreshed(run_id: &str) -> bool {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    let set = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    set.lock().unwrap().insert(run_id.to_string())
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
    // Every herdr agent name a bot claimed in this pass; what is left over is a stranger,
    // and a stranger named `<some bot's agent name>-<suffix>` is that bot's child (below).
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Every bot that could be a parent this pass, for the descent and prefix matches.
    let mut parents: Vec<Parent> = Vec::new();
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
        } else if bot.managed_by == "child" {
            // No live run: the herdr name its last run carried is still how to find it.
            if let Some(n) = sqlx::query_scalar::<_, Option<String>>(
                "SELECT agent_name FROM runs WHERE bot_id = ? ORDER BY started_at DESC LIMIT 1",
            )
            .bind(&bot.id)
            .fetch_optional(&app.db)
            .await?
            .flatten()
            .filter(|s| !s.is_empty())
            {
                candidates.push(n);
            }
        }
        let label: String = sqlx::query_scalar("SELECT label FROM projects WHERE id = ?")
            .bind(&bot.project_id)
            .fetch_optional(&app.db)
            .await?
            .unwrap_or_default();
        let legacy = crate::config::agent_name_legacy(&label, &bot.name);
        // A spawned child is only ever the herdr agent it was adopted from: its run's name.
        // The computed `<project>-<hash>` name would match a pane the daemon started for it
        // by mistake, and the bare name is whatever suffix the parent picked.
        if bot.managed_by != "child" {
            for n in [computed.clone(), legacy, bot.name.clone()] {
                if !candidates.contains(&n) {
                    candidates.push(n);
                }
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
        if let Some(n) = &found_name {
            claimed.insert(n.clone());
        }
        // A bot only owns a tab while herdr still lists its agent; a bot whose run is about to
        // be exited below adopts nobody.
        parents.push(Parent {
            // The name it is *actually* running under, which for a spawned child is the one
            // its parent picked, not the `<project>-<hash>` we would have computed.
            agent_name: found_name.clone().unwrap_or_else(|| computed.clone()),
            tab_id: found.as_ref().map(|a| a.tab_id.clone()),
            bot: bot.clone(),
        });
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
                // A run with no hooks (a spawned child, above all) has nothing but its pane to
                // build a conversation from, and the daemon was not watching while it was down.
                crate::lifecycle::spawn_adopted_capture(app, &run.id, &bot.id);
                sync_pane_model(app, host, &client, &bot, agent).await;
                // SPEC §11.4.7: a run we keep may have been started by a pre-v4.3 daemon, whose
                // `hook.sh` still curls a port that no longer exists. Rewriting the material is
                // one ssh per bot, so it runs off-path — reconcile must not wait on the network.
                // Once per run per daemon lifetime: the herdr event stream replays a burst of
                // `pane.agent_detected` on (re)connect, each scheduling a reconcile, and one ssh
                // per bot per reconcile turned that into a storm sshd refused (2026-09-07).
                if host != LOCAL_HOST && bot.inject_hooks != 0 && mark_hook_refreshed(&run.id) {
                    let app2 = app.clone();
                    let bot2 = bot.clone();
                    tokio::spawn(async move {
                        if let Err(e) = crate::lifecycle::refresh_remote_hook(&app2, &bot2).await {
                            tracing::warn!(bot = %bot2.name, error = ?e, "could not refresh the remote hook");
                        }
                    });
                }
                tracing::info!(host, bot = %bot.name, run = %run.id, pane = %agent.pane_id, "reconcile: kept active run");
            }
            (Some(run), None) => {
                tracing::info!(host, bot = %bot.name, run = %run.id, "reconcile: agent gone, marking run exited");
                crate::lifecycle::mark_run_exited(app, &run.id, "agent not found during reconcile").await;
                // A spawned child exists only as long as its pane: retire it with the run,
                // keeping its conversation the way any deleted bot keeps it.
                if bot.managed_by == "child" {
                    sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
                        .bind(db::now())
                        .bind(&bot.id)
                        .execute(&app.db)
                        .await?;
                    app.emit("project_changed", json!({"project_id": bot.project_id})).await;
                    tracing::info!(host, bot = %bot.name, "reconcile: spawned child retired with its pane");
                }
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
                crate::lifecycle::spawn_adopted_capture(app, &run_id, &bot.id);
                sync_pane_model(app, host, &client, &bot, agent).await;
                tracing::info!(host, bot = %bot.name, run = %run_id, pane = %agent.pane_id, "reconcile: adopted existing agent");
            }
            (None, None) => {}
        }
        app.emit_bot_status(&bot.id).await;
    }

    // Spawned children: an agent nobody claimed becomes a `managed_by='child'` bot under the
    // parent it descends from, with an adopted run.
    //
    // **Descent first.** One bot, one tab: an unclaimed agent sitting in a bot's tab was
    // spawned from that bot's pane, whatever it named itself. That is a mechanism, where the
    // `<parent agent name>-<suffix>` naming the persona asks for (`lifecycle::child_agent_rules`) is
    // only a request — an agent that forgets it, or a codex/grok that never read it, used to
    // vanish into an untracked sub-task.
    //
    // The prefix match is kept for the cases descent cannot see: a child in another tab, or a
    // team workspace. Within the matching tab the longest prefix still wins, so a grandchild
    // lands under the child rather than the grandparent; with no prefix at all the tab's own
    // bot (not a child adopted into it) takes it.
    let mut new_children = 0usize;
    for agent in agents.iter() {
        let Some(name) = agent.name.as_deref() else { continue };
        if claimed.contains(name) {
            continue;
        }
        let by_tab = parents
            .iter()
            .filter(|p| p.tab_id.as_deref() == Some(agent.tab_id.as_str()))
            .max_by_key(|p| (prefix_score(&p.agent_name, name), u8::from(p.bot.managed_by != "child")));
        let by_prefix =
            parents.iter().filter(|p| prefix_score(&p.agent_name, name) > 0).max_by_key(|p| p.agent_name.len());
        let Some(entry) = by_tab.or(by_prefix) else { continue };
        let (parent_name, parent) = (&entry.agent_name, &entry.bot);
        let child_name = match prefix_score(parent_name, name) {
            0 => child_name_from_agent(name),
            n => {
                let suffix = &name[n + 1..];
                if crate::config::valid_bot_name(suffix) { suffix.to_string() } else { child_name_from_agent(name) }
            }
        };
        let kind = agent
            .agent
            .as_deref()
            .filter(|k| crate::config::valid_kind(k))
            .unwrap_or(parent.kind.as_str())
            .to_string();
        let now = db::now();
        // The same child coming back (its pane was closed and reopened, or its run was ended
        // by something other than the reconcile) is the same bot: `bots_name_project_live`
        // would refuse a second live row under that name anyway. Reuse it, new run.
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT id FROM bots WHERE project_id = ? AND name = ? AND managed_by = 'child' AND deleted_at IS NULL",
        )
        .bind(&parent.project_id)
        .bind(&child_name)
        .fetch_optional(&app.db)
        .await?;
        let bot_id = match existing {
            Some(id) => {
                sqlx::query("UPDATE bots SET parent_bot_id = ?, cwd = COALESCE(?, cwd), kind = ? WHERE id = ?")
                    .bind(&parent.id)
                    .bind(agent.cwd.clone())
                    .bind(&kind)
                    .bind(&id)
                    .execute(&app.db)
                    .await?;
                id
            }
            None => {
                let bot_id = db::ulid();
                // Hooks are never injected here (the pane was started by the parent, not by
                // us), so the child's replies come from the terminal fallback.
                sqlx::query(
                    "INSERT INTO bots (id, project_id, name, kind, model, effort, fast, persona, args_json, autostart, inject_hooks, auto_approve,
                       identity, env_json, managed_by, team_id, team_role, cwd, herdr_session, parent_bot_id, hook_token, created_at)
                     VALUES (?,?,?,?,NULL,NULL,0,NULL,'[]',0,0,1,?,'{}','child',NULL,NULL,?,?,?,?,?)",
                )
                .bind(&bot_id)
                .bind(&parent.project_id)
                .bind(&child_name)
                .bind(&kind)
                .bind(&parent.identity)
                .bind(agent.cwd.clone())
                .bind(&session)
                .bind(&parent.id)
                .bind(db::ulid())
                .bind(&now)
                .execute(&app.db)
                .await?;
                bot_id
            }
        };
        let run_id = db::ulid();
        let status = agent.agent_status.normalized().as_str().to_string();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted, agent_name, herdr_session, started_at)
             VALUES (?,?,'running',?,?,?,?,1,?,?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(&status)
        .bind(&agent.workspace_id)
        .bind(&agent.pane_id)
        .bind(&agent.tab_id)
        .bind(name)
        .bind(&session)
        .bind(&now)
        .execute(&app.db)
        .await?;
        claimed.insert(name.to_string());
        crate::events::watch_pane_on_session(app, host, &session, &agent.pane_id).await;
        // The child's pane is the only source for both halves of its identity: what it is
        // saying (no hooks were injected, so §4.3's snapshot is all there is) and what it is
        // running on (we did not choose its model, its own argv did).
        crate::lifecycle::spawn_adopted_capture(app, &run_id, &bot_id);
        if let Ok(Some(child)) = db::bot(&app.db, &bot_id).await {
            sync_pane_model(app, host, &client, &child, agent).await;
        }
        app.emit("bot_changed", json!({"bot_id": bot_id})).await;
        app.emit_bot_status(&bot_id).await;
        new_children += 1;
        tracing::info!(host, parent = %parent.name, child = %child_name, agent = %name, pane = %agent.pane_id,
                       "reconcile: adopted a spawned child agent");
    }
    if new_children > 0 {
        app.emit("project_changed", json!({})).await;
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

/// Fill in a child bot's `model` / `effort` from what its CLI is actually running.
///
/// A spawned child was launched by another agent, so nothing in our database says what it is
/// on and the sidebar badge reads 「預設」 for a pane that is quite deliberately on `opus`.
/// `pane.process_info` reports the pane's foreground argv — the same flags `model_args` would
/// have produced — and grok additionally spells both out in its terminal title
/// (`Grok 4.6 (xhigh)`) until it renames itself after its task.
///
/// Two deliberate limits:
/// * **children only.** For any other bot `bots.model` is the user's own setting, and for a
///   `managed_by='user'` bot the TOML projection writes it back to `config.toml` — reading a
///   model off a process and storing it there would be the daemon overwriting configuration
///   with a guess.
/// * **fills, never corrects.** argv cannot see a later `/model` typed into the TUI (which is
///   exactly what `apply_live_setting` sends when the model is changed from the UI), so a
///   value that is already recorded is left alone. A field we could not parse stays NULL —
///   「預設」 is honest, a guess is not.
async fn sync_pane_model(app: &Arc<App>, host: &str, client: &crate::herdr::HerdrClient, bot: &db::Bot, agent: &crate::herdr::AgentInfo) {
    if bot.managed_by != "child" || (bot.model.is_some() && bot.effort.is_some()) {
        return;
    }
    let procs = match client.pane_process_info(&agent.pane_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(bot = %bot.name, pane = %agent.pane_id, error = %e, "pane.process_info unavailable");
            return;
        }
    };
    // The agent CLI is the front process; anything it shells out to (`git`, a pager) has an
    // argv of its own, so pick the one that looks like the CLI and fall back to the first.
    let argv: &[String] = procs
        .iter()
        .find(|p| p.argv.first().map(|a| a.contains(bot.kind.as_str())).unwrap_or(false))
        .or_else(|| procs.iter().find(|p| !p.argv.is_empty()))
        .map(|p| p.argv.as_slice())
        .unwrap_or(&[]);
    let (mut model, mut effort) = crate::models::model_effort_from_argv(&bot.kind, argv);
    if bot.kind == "grok" && (model.is_none() || effort.is_none()) {
        let (tm, te) = crate::models::grok_title_model_effort(agent.terminal_title_stripped.as_deref().unwrap_or(""));
        model = model.or(tm);
        effort = effort.or(te);
    }
    // claude's argv rarely carries `--effort` (`herdr agent start … -- --model opus` is the usual
    // shape), yet the CLI still runs at that account's default level; resolve it the same way
    // the model list's "預設" hint does, so the sidebar chip reads `opus · High` and not just `opus`.
    if bot.kind == "claude" && effort.is_none() && bot.effort.is_none() {
        if let Some(alias) = model.as_deref().or(bot.model.as_deref()) {
            effort = Some(crate::models::claude_default_effort(app, host, bot.identity.as_deref(), alias).await);
        }
    }
    let model = model.filter(|_| bot.model.is_none());
    let effort = effort.filter(|_| bot.effort.is_none());
    if model.is_none() && effort.is_none() {
        return;
    }
    // COALESCE, so a field the argv did not mention keeps whatever it had.
    if let Err(e) = sqlx::query("UPDATE bots SET model = COALESCE(?, model), effort = COALESCE(?, effort) WHERE id = ?")
        .bind(&model)
        .bind(&effort)
        .bind(&bot.id)
        .execute(&app.db)
        .await
    {
        tracing::warn!(bot = %bot.name, error = ?e, "cannot record the model a child agent is running");
        return;
    }
    tracing::info!(bot = %bot.name, pane = %agent.pane_id, ?model, ?effort, "reconcile: read the child's model off its argv");
    app.emit("bot_changed", json!({"bot_id": bot.id})).await;
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

    /// A spawned child is adopted with the model and effort its CLI is actually running:
    /// nothing in the database chose them (its parent started the pane), so the only evidence
    /// is the pane's own argv. Without this the sidebar badge says 「預設」 for a child quite
    /// deliberately launched on `opus`.
    #[tokio::test]
    async fn a_spawned_child_is_adopted_with_the_model_its_argv_names() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let parent = a_bot(&env, "alfa").await;
        let parent_agent = crate::config::agent_name("proj", &parent);
        let kid_agent = format!("{parent_agent}-lastq");
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(&parent)
        .bind(&ws.workspace_id)
        .bind(&root.pane_id)
        .bind(&parent_agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![
            json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
            json!({"name": kid_agent, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id, "cwd": "/tmp/p"}),
        ];
        // What each pane is really running. The parent's says `haiku` — and must be ignored,
        // because a user bot's model is the user's setting, not something we read back.
        env.herdr.set_argv(&root.pane_id, &["claude", "--dangerously-skip-permissions", "--model", "haiku"]);
        env.herdr.set_argv(&kid_pane.pane_id, &["claude", "--dangerously-skip-permissions", "--model", "opus", "--effort", "high"]);

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let kid = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
            .bind(&parent)
            .fetch_one(&app.db)
            .await
            .expect("the child was adopted");
        assert_eq!(kid.name, "lastq");
        assert_eq!(kid.model.as_deref(), Some("opus"), "read off `--model`");
        assert_eq!(kid.effort.as_deref(), Some("high"), "read off `--effort`");
        assert_eq!(kid.inject_hooks, 0, "still no hooks: 對話 comes from the terminal");

        let p = db::bot(&app.db, &parent).await.unwrap().unwrap();
        assert_eq!(p.model, None, "a user bot's model is configuration, never scraped from its process");

        // A second reconcile does not undo a model the user has since changed from the UI
        // (`/model` inside the TUI leaves argv untouched, so argv must not win a rematch).
        sqlx::query("UPDATE bots SET model='sonnet' WHERE id=?").bind(&kid.id).execute(&app.db).await.unwrap();
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(db::bot(&app.db, &kid.id).await.unwrap().unwrap().model.as_deref(), Some("sonnet"));
    }

    /// **Descent, not naming.** A child that ignored the naming rule — `helper`, no parent
    /// prefix — is still claimed, because it is sitting in its parent's tab. This is the whole
    /// point: the prefix was a request to the agent, the tab is a fact about the pane.
    #[tokio::test]
    async fn a_stranger_in_a_bots_tab_is_claimed_as_its_child() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        // The child's pane is a split of the parent's, so it shares the parent's tab.
        let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        assert_eq!(kid_pane.tab_id, root.tab_id, "precondition: one bot, one tab");

        let parent = a_bot(&env, "alfa").await;
        let parent_agent = crate::config::agent_name("proj", &parent);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(&parent)
        .bind(&ws.workspace_id)
        .bind(&root.pane_id)
        .bind(&root.tab_id)
        .bind(&parent_agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![
            json!({"name": parent_agent, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": root.tab_id, "pane_id": root.pane_id, "cwd": "/tmp/p"}),
            json!({"name": "helper", "agent": "codex", "agent_status": "working",
                   "workspace_id": ws.workspace_id, "tab_id": kid_pane.tab_id, "pane_id": kid_pane.pane_id, "cwd": "/tmp/p"}),
        ];

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let kid = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
            .bind(&parent)
            .fetch_one(&app.db)
            .await
            .expect("the stranger in the tab was adopted as a child");
        assert_eq!(kid.name, "helper", "no prefix to strip: herdr's own agent name");
        assert_eq!(kid.kind, "codex", "a child need not be the parent's CLI");
        assert_eq!(kid.managed_by, "child");
        let r = run_of(&app, &kid.id).await.unwrap();
        assert_eq!(r.adopted, 1);
        assert_eq!(r.pane_id.as_deref(), Some(kid_pane.pane_id.as_str()));

        // Idempotent: a second pass reuses the bot rather than making a twin.
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bots WHERE parent_bot_id = ? AND deleted_at IS NULL")
            .bind(&parent)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    /// Descent decides *which* bot, and inside the tab the longest prefix still decides the
    /// depth: a grandchild named after the child lands under the child, not the top bot.
    #[tokio::test]
    async fn a_grandchild_in_the_same_tab_lands_under_the_child() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let kid_pane = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let grand_pane = client.pane_split(&kid_pane.pane_id, "down", "/tmp/p", json!({})).await.unwrap();

        let parent = a_bot(&env, "alfa").await;
        let parent_agent = crate::config::agent_name("proj", &parent);
        let kid_agent = format!("{parent_agent}-lastq");
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(&parent)
        .bind(&ws.workspace_id)
        .bind(&root.pane_id)
        .bind(&root.tab_id)
        .bind(&parent_agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let agent_json = |name: &str, pane: &str, tab: &str| {
            json!({"name": name, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": tab, "pane_id": pane, "cwd": "/tmp/p"})
        };
        *env.herdr.agents.lock().unwrap() = vec![
            agent_json(&parent_agent, &root.pane_id, &root.tab_id),
            agent_json(&kid_agent, &kid_pane.pane_id, &kid_pane.tab_id),
        ];
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        let kid = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE parent_bot_id = ?")
            .bind(&parent)
            .fetch_one(&app.db)
            .await
            .unwrap();

        // Now a grandchild appears, in the very same tab, named after the child.
        env.herdr.agents.lock().unwrap().push(agent_json(
            &format!("{kid_agent}-deep"),
            &grand_pane.pane_id,
            &grand_pane.tab_id,
        ));
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let grand = sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE name = 'deep'")
            .fetch_one(&app.db)
            .await
            .expect("the grandchild was adopted");
        assert_eq!(grand.parent_bot_id.as_deref(), Some(kid.id.as_str()), "under the child, not the top bot");
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
