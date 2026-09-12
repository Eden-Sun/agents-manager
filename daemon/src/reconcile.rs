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
        // …and the other direction (2026-09-10 23:02, AGM down 5.5 h). The agent list is from
        // before this bot's lock was ours; when a stop or a restart was holding it, what the list
        // says about this bot can already be history — the agent exited, its pane closed, maybe
        // a fresh run started on another pane. Acting on that stale entry either adopts a dying
        // agent as a new run (the restart waiting for the lock then refuses with `active run
        // already exists` and nobody starts the bot again) or moves the fresh run back onto the
        // closed pane. So in exactly those two cases, ask herdr again before acting. The name
        // stays in `claimed` either way: it is this bot's own agent, never a stranger to adopt
        // as someone's child below.
        if let Some(listed) = found.clone() {
            let stale_possible = match &active {
                None => true,
                Some(r) => r.pane_id.as_deref() != Some(listed.pane_id.as_str()),
            };
            if stale_possible {
                let name = found_name.clone().unwrap_or_default();
                let got = client.agent_get(&name).await;
                let got_pane = got.as_ref().ok().and_then(|a| a.as_ref().map(|a| a.pane_id.clone()));
                let current = match got {
                    Ok(Some(a)) => match client.pane_get(&a.pane_id).await {
                        Ok(None) => None,
                        _ => Some(a),
                    },
                    Ok(None) => None,
                    // Cannot tell: keep what the list said, exactly as before this check existed.
                    Err(_) => Some(listed.clone()),
                };
                if current.as_ref().map(|a| a.pane_id.as_str()) != Some(listed.pane_id.as_str()) {
                    tracing::info!(host, bot = %bot.name, agent = %name, listed_pane = %listed.pane_id,
                        current_pane = ?current.as_ref().map(|a| a.pane_id.clone()), agent_get = ?got_pane,
                        run_pane = ?active.as_ref().and_then(|r| r.pane_id.clone()),
                        "reconcile: agent list went stale while waiting for the bot's lock; using herdr's current answer");
                }
                match (&active, current) {
                    // Nothing to protect: adopt only what herdr confirms, on the pane it is on now.
                    (None, current) => {
                        if current.is_none() {
                            found_name = None;
                        }
                        found = current;
                    }
                    // herdr confirms the agent on some pane: the run follows it (a pane move).
                    (Some(_), Some(a)) => found = Some(a),
                    // A run on another pane than the stale list said, and herdr cannot confirm
                    // the agent by name — the case right after a same-named restart. Not "gone"
                    // on that evidence alone: the run's own pane decides below.
                    (Some(_), None) => {
                        found = None;
                        found_name = None;
                    }
                }
            }
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
                //
                // The *status* is asked for again, now, under the lock. The list it came from
                // predates the lock — by up to a minute when an earlier bot's `start` held its
                // lock through `agent.wait` — and writing a stale `idle` over a run that has
                // since gone `working` eats the next `working -> idle` edge: the event handler
                // sees `prev == idle`, so no fallback is armed, no queued prompt is flushed and
                // no turn-error scan runs (review 2026-09-12 a). One RPC per live run; if it
                // fails, the list's answer stands as before.
                let status = match client.agent_get(&agent.pane_id).await {
                    Ok(Some(fresh)) => fresh.agent_status.normalized().as_str().to_string(),
                    _ => agent.agent_status.normalized().as_str().to_string(),
                };
                // `stopping` is healed too: a stop or an in-pane restart that gave up on an
                // agent which would not exit used to leave the run there for good (review
                // 2026-09-12 #1). herdr still lists the agent, so it is running.
                sqlx::query("UPDATE runs SET pane_id=?, workspace_id=?, tab_id=?, agent_status=?, agent_name=COALESCE(?, agent_name), herdr_session=COALESCE(herdr_session, ?), state=CASE WHEN state IN ('starting','stopping') THEN 'running' ELSE state END WHERE id=?")
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
                // herdr does not know the run's agent by name — but is the agent still sitting in
                // the run's pane? After a same-named restart it is: the old agent's late exit
                // cleared the name the new one had just taken (herdr keeps one registry keyed by
                // name), while the pane itself still reports `agent: claude`. Marking that run
                // exited (2026-09-11, the first fix for the 23:02 race) took every freshly
                // restarted bot down and let the orphan sweep close its new pane. Keep the run
                // and put the name back, so `agent.prompt`/`send_keys` by name work again.
                if let Some(p) = run.pane_id.as_deref() {
                    // `agent.get` takes a pane id too, and answers with whoever is in it —
                    // name and all, or `name: null` once herdr has cleared it.
                    let occupant = client.agent_get(p).await;
                    tracing::info!(host, bot = %bot.name, run = %run.id, pane = %p, candidates = ?candidates,
                        occupant = ?occupant.as_ref().map(|o| o.as_ref().map(|a| (a.name.clone(), a.agent.clone(), a.pane_id.clone()))).map_err(|e| e.to_string()),
                        "reconcile: run's agent not listed by name; asking its pane");
                    match occupant {
                        Ok(Some(occupant)) if occupant.name.as_deref().map_or(true, |n| candidates.iter().any(|c| c == n)) => {
                            let name = run.agent_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| computed.clone());
                            if occupant.name.is_none() {
                                match client.agent_rename(p, &name).await {
                                    Ok(_) => tracing::info!(host, bot = %bot.name, run = %run.id, pane = %p, agent = %name,
                                        "reconcile: herdr had lost the agent's name but its pane still hosts it; name re-applied, run kept"),
                                    Err(e) => tracing::warn!(host, bot = %bot.name, run = %run.id, pane = %p, agent = %name, error = ?e,
                                        "reconcile: herdr had lost the agent's name and would not take it back; run kept anyway"),
                                }
                            }
                            let status = occupant.agent_status.normalized().as_str().to_string();
                            sqlx::query("UPDATE runs SET agent_status=?, state=CASE WHEN state IN ('starting','stopping') THEN 'running' ELSE state END WHERE id=?")
                                .bind(&status)
                                .bind(&run.id)
                                .execute(&app.db)
                                .await?;
                            claimed.insert(name);
                            app.emit_bot_status(&bot.id).await;
                            continue;
                        }
                        // Someone else's agent, or nobody: the run's agent really is gone.
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(host, bot = %bot.name, run = %run.id, pane = %p, error = ?e,
                                "reconcile: could not ask herdr what is in the run's pane; leaving the run alone this pass");
                            continue;
                        }
                    }
                }
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
            (None, None) => {
                // #60: a spawned child whose pane was closed. `herdr pane close` reports
                // `pane_closed` first and `events::end_runs_for_pane` ends the run; by the time a
                // reconcile gets here the child has no run and herdr no longer lists its agent —
                // the "agent gone" branch above never sees it, and the child used to stay in the
                // sidebar for good. A child cannot be started from here (`start_bot` refuses), so
                // with its last run over and its agent gone it is done: retire it the same way.
                // An agent herdr still lists is not this case — it lands in `(None, Some)` and is
                // adopted again.
                if bot.managed_by == "child" {
                    let ended: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM runs WHERE bot_id = ? AND state NOT IN ('starting','running','stopping')",
                    )
                    .bind(&bot.id)
                    .fetch_one(&app.db)
                    .await?;
                    if ended > 0 {
                        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL")
                            .bind(db::now())
                            .bind(&bot.id)
                            .execute(&app.db)
                            .await?;
                        app.emit("project_changed", json!({"project_id": bot.project_id})).await;
                        tracing::info!(host, bot = %bot.name, "reconcile: spawned child retired — its run had already ended and herdr no longer lists its agent");
                    }
                }
            }
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
        // Everything from here to the run row is one child's business. A `?` used to end the
        // whole host's reconcile — every later agent unadopted, orphan panes kept, teams and
        // codex runtime never checked, and `reconcile failed` in the log every two seconds
        // (review 2026-09-12 #2). One child that cannot be adopted is logged and skipped.
        match adopt_child(app, host, &client, &session, agent, name, parent, &child_name, &kind).await {
            Ok(bot_id) => {
                claimed.insert(name.to_string());
                app.emit("bot_changed", json!({"bot_id": bot_id})).await;
                app.emit_bot_status(&bot_id).await;
                new_children += 1;
                tracing::info!(host, parent = %parent.name, child = %child_name, agent = %name, pane = %agent.pane_id,
                               "reconcile: adopted a spawned child agent");
            }
            Err(e) => {
                tracing::warn!(host, parent = %parent.name, agent = %name, pane = %agent.pane_id, error = ?e,
                               "reconcile: could not adopt a spawned child agent this pass");
            }
        }
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
    // SPEC §4.4a: a run the daemon did not start has NULL `runtime_*`, so the UI falls back to
    // `bots` and quietly claims the CLI is on whatever was configured. codex prints all three
    // in its own status line, so read it instead of guessing.
    fill_codex_runtime(app, host, &client).await;
    Ok(())
}

/// One spawned child: find or create its `managed_by='child'` bot row under `parent` and give
/// it an adopted run on `agent`'s pane. Returns the bot id.
///
/// **Same parent, same name = same child.** The same child coming back (its pane was closed
/// and reopened, or its run was ended by something other than the reconcile) is the same bot,
/// so it is reused with a new run. The lookup is keyed on the parent as well as the name:
/// keyed on the name alone it also hit *another* parent's child of the same name (two mother
/// bots in one project each spawning `<self>-ui`), re-parented it and then tripped
/// `runs_one_active` on its live run (review 2026-09-12 #2).
///
/// **A name that is taken is not ours.** `bots_name_project_live` allows one live `ui` per
/// project. When the suffix is already someone else's — the other mother's child, or a bot the
/// user named `review` while a parent spawned `<parent>-review` — the child is stored under its
/// full herdr agent name instead, which herdr keeps unique. Both spellings are looked up on the
/// way back in, so the child found under the long name is recognised next pass.
#[allow(clippy::too_many_arguments)]
async fn adopt_child(
    app: &Arc<App>,
    host: &str,
    client: &crate::herdr::HerdrClient,
    session: &str,
    agent: &crate::herdr::AgentInfo,
    name: &str,
    parent: &db::Bot,
    child_name: &str,
    kind: &str,
) -> anyhow::Result<String> {
    let now = db::now();
    let full_name = child_name_from_agent(name);
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM bots WHERE project_id = ? AND parent_bot_id = ? AND managed_by = 'child' AND deleted_at IS NULL
         AND name IN (?, ?) ORDER BY CASE WHEN name = ? THEN 0 ELSE 1 END LIMIT 1",
    )
    .bind(&parent.project_id)
    .bind(&parent.id)
    .bind(child_name)
    .bind(&full_name)
    .bind(child_name)
    .fetch_optional(&app.db)
    .await?;
    let bot_id = match existing {
        Some(id) => {
            // Its last run ended some other way and its agent is not the one we are looking at
            // now: the per-bot loop will sort that run out first. Adopting on top of it would
            // only trip `runs_one_active`.
            if let Some(r) = db::active_run(&app.db, &id).await? {
                anyhow::bail!("child `{child_name}` still has active run `{}` under agent `{:?}`", r.id, r.agent_name);
            }
            sqlx::query("UPDATE bots SET cwd = COALESCE(?, cwd), kind = ? WHERE id = ?")
                .bind(agent.cwd.clone())
                .bind(kind)
                .bind(&id)
                .execute(&app.db)
                .await?;
            id
        }
        None => {
            let taken = |n: String| {
                let db = app.db.clone();
                let project = parent.project_id.clone();
                async move {
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM bots WHERE project_id = ? AND name = ? AND deleted_at IS NULL")
                        .bind(&project)
                        .bind(&n)
                        .fetch_one(&db)
                        .await
                        .map(|c| c > 0)
                }
            };
            let use_name = if !taken(child_name.to_string()).await? {
                child_name.to_string()
            } else if full_name != child_name && !taken(full_name.clone()).await? {
                tracing::info!(host, parent = %parent.name, agent = %name, taken = %child_name, using = %full_name,
                               "reconcile: child's short name is already a bot in this project; using its full agent name");
                full_name.clone()
            } else {
                anyhow::bail!("both `{child_name}` and `{full_name}` are already live bots in this project");
            };
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
            .bind(&use_name)
            .bind(kind)
            .bind(&parent.identity)
            .bind(agent.cwd.clone())
            .bind(session)
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
    .bind(session)
    .bind(&now)
    .execute(&app.db)
    .await?;
    crate::events::watch_pane_on_session(app, host, session, &agent.pane_id).await;
    // The child's pane is the only source for both halves of its identity: what it is
    // saying (no hooks were injected, so §4.3's snapshot is all there is) and what it is
    // running on (we did not choose its model, its own argv did).
    crate::lifecycle::spawn_adopted_capture(app, &run_id, &bot_id);
    if let Ok(Some(child)) = db::bot(&app.db, &bot_id).await {
        sync_pane_model(app, host, client, &child, agent).await;
    }
    Ok(bot_id)
}

/// Learn `runs.runtime_model` / `runtime_effort` / `runtime_fast` for codex runs the daemon did
/// not start (adopted panes, SPEC §4.4a).
///
/// Without this the UI shows `bots.fast = false` while the terminal's status line says `fast`,
/// which is exactly the "靜靜顯示一個還沒生效的值" §4.4a forbids — and `/fast` is a toggle, so a
/// tier nobody knows is a tier nobody can flip. Only fills what is still NULL: a value written
/// by a start or by a live apply is already the truth, and re-reading would just race with it.
async fn fill_codex_runtime(app: &Arc<App>, host: &str, client: &crate::herdr::HerdrClient) {
    let rows: Vec<(String, String)> = match sqlx::query_as(
        "SELECT r.id, r.pane_id FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
         WHERE p.host = ? AND b.kind = 'codex' AND r.state = 'running' AND r.pane_id IS NOT NULL
         AND r.runtime_model IS NULL AND r.runtime_effort IS NULL AND r.runtime_fast IS NULL",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(host, error = ?e, "codex runtime probe: query failed");
            return;
        }
    };
    for (run_id, pane_id) in rows {
        let Ok(read) = client.pane_read(&pane_id, "visible", 60).await else { continue };
        let Some(seen) = crate::codex_live::parse_status_line(&read.text) else { continue };
        let _ = sqlx::query("UPDATE runs SET runtime_model = ?, runtime_effort = ?, runtime_fast = ? WHERE id = ?")
            .bind(&seen.model)
            .bind(&seen.effort)
            .bind(i64::from(seen.fast))
            .bind(&run_id)
            .execute(&app.db)
            .await;
        if let Ok(Some(run)) = crate::db::run(&app.db, &run_id).await {
            app.emit_bot_status(&run.bot_id).await;
        }
        tracing::info!(host, run = %run_id, model = %seen.model, effort = ?seen.effort, fast = seen.fast,
                       "codex runtime read off an adopted pane's status line");
    }
}

/// Fill in a child bot's `model` / `effort` / `identity` from what its CLI is actually running.
///
/// A spawned child was launched by another agent, so nothing in our database says what it is
/// on and the sidebar badge reads 「預設」 for a pane that is quite deliberately on `opus`.
/// `pane.process_info` reports the pane's foreground argv — the same flags `model_args` would
/// have produced — and grok additionally spells both out in its terminal title
/// (`Grok 4.6 (xhigh)`) until it renames itself after its task. The same call's **pid** is what
/// [`crate::pane_identity`] then reads the child's account off, the one thing argv cannot show.
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
///
/// `identity` is the one field that also *corrects*, and only because it is not in the same
/// category: nobody ever set a child's account, the adopt copied it off the parent. See
/// [`crate::pane_identity`] for where that line is drawn.
async fn sync_pane_model(app: &Arc<App>, host: &str, client: &crate::herdr::HerdrClient, bot: &db::Bot, agent: &crate::herdr::AgentInfo) {
    if bot.managed_by != "child" {
        return;
    }
    let want_model = bot.model.is_none() || bot.effort.is_none();
    let want_identity = crate::pane_identity::probe_due(&bot.id, &agent.pane_id);
    if !want_model && !want_identity {
        return;
    }
    let procs = match client.pane_process_info(&agent.pane_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(bot = %bot.name, pane = %agent.pane_id, error = %e, "pane.process_info unavailable");
            return;
        }
    };
    // The agent CLI is the front process; anything it shells out to (`git`, a pager, the
    // `caffeinate` a pane may be wrapped in) has an argv of its own, so pick the one that looks
    // like the CLI and fall back to the first. Both halves below want that same process: its
    // argv names the model, its pid names the account.
    let cli = procs
        .iter()
        .find(|p| p.argv.first().map(|a| a.contains(bot.kind.as_str())).unwrap_or(false))
        .or_else(|| procs.iter().find(|p| !p.argv.is_empty()));
    if want_identity {
        crate::pane_identity::sync_child_identity(app, host, bot, &agent.pane_id, cli.and_then(|p| p.pid)).await;
    }
    if !want_model {
        return;
    }
    let argv: &[String] = cli.map(|p| p.argv.as_slice()).unwrap_or(&[]);
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

    /// A run left in `stopping` — a stop or in-pane restart that gave up on an agent which would
    /// not exit — is healed back to `running` while herdr still lists the agent (review
    /// 2026-09-12 #1). `starting` was already healed this way; `stopping` was not, and the bot
    /// stayed yellow with every prompt refused.
    #[tokio::test]
    async fn a_run_stuck_in_stopping_is_healed_while_its_agent_is_listed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let bot = a_bot(&env, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot);
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'stopping','idle',?,?,?,?,'test',?)",
        )
        .bind(&run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(&agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": agent, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id,
            "cwd": "/tmp/p"})];

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let r = run_of(&app, &bot).await.unwrap();
        assert_eq!(r.id, run);
        assert_eq!(r.state, "running");
    }

    /// **Name collisions must not end the reconcile** (review 2026-09-12 #2). Two mother bots in
    /// one project each spawn `<self>-ui`, and a parent spawns `<parent>-review` while the user
    /// already has a bot called `review`. Keyed on the bare suffix, the second `ui` used to hit
    /// the first one's row (re-parented, then `runs_one_active`), and `review` hit
    /// `bots_name_project_live`; either `?` aborted `reconcile_host` — nothing after it ran,
    /// and the log said `reconcile failed` every two seconds until the agent went away.
    #[tokio::test]
    async fn colliding_child_names_are_stored_under_their_agent_name_and_never_abort_the_pass() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let bravo_pane = client.tab_create(&ws.workspace_id, "/tmp/p", "bravo", json!({})).await.unwrap();
        let alfa_ui = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let alfa_review = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let bravo_ui = client.pane_split(&bravo_pane.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let alfa = a_bot(&env, "alfa").await;
        let bravo = a_bot(&env, "bravo").await;
        let review = a_bot(&env, "review").await;
        let alfa_agent = crate::config::agent_name("proj", &alfa);
        let bravo_agent = crate::config::agent_name("proj", &bravo);
        for (bot, agent, pane) in [(&alfa, &alfa_agent, &root), (&bravo, &bravo_agent, &bravo_pane)] {
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
                 VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
            )
            .bind(db::ulid())
            .bind(bot)
            .bind(&ws.workspace_id)
            .bind(&pane.tab_id)
            .bind(&pane.pane_id)
            .bind(agent)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        }
        let entry = |name: String, pane: &crate::herdr::PaneInfo| {
            json!({"name": name, "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})
        };
        *env.herdr.agents.lock().unwrap() = vec![
            entry(alfa_agent.clone(), &root),
            entry(bravo_agent.clone(), &bravo_pane),
            entry(format!("{alfa_agent}-ui"), &alfa_ui),
            entry(format!("{alfa_agent}-review"), &alfa_review),
            entry(format!("{bravo_agent}-ui"), &bravo_ui),
        ];

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.expect("one bad name must not end the pass");

        let kids = |parent: String| {
            let db = app.db.clone();
            async move {
                sqlx::query_as::<_, db::Bot>(
                    "SELECT * FROM bots WHERE parent_bot_id = ? AND managed_by = 'child' AND deleted_at IS NULL ORDER BY name",
                )
                .bind(&parent)
                .fetch_all(&db)
                .await
                .unwrap()
            }
        };
        let alfa_kids = kids(alfa.clone()).await;
        let bravo_kids = kids(bravo.clone()).await;
        assert_eq!(
            alfa_kids.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            [format!("{alfa_agent}-review").as_str(), "ui"],
            "alfa's `ui` keeps the short name; its `review` yields to the user's bot of that name"
        );
        assert_eq!(
            bravo_kids.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            [format!("{bravo_agent}-ui").as_str()],
            "bravo's `ui` is a different child, stored under its full agent name"
        );
        let user_review = db::bot(&app.db, &review).await.unwrap().unwrap();
        assert_eq!(user_review.managed_by, "user");
        assert!(user_review.parent_bot_id.is_none(), "the user's bot was not touched");
        for k in alfa_kids.iter().chain(bravo_kids.iter()) {
            let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id = ?")
                .bind(&k.id)
                .fetch_one(&app.db)
                .await
                .unwrap();
            assert_eq!(runs, 1, "{}: one adopted run", k.name);
        }

        // The next pass finds every child again — under whichever spelling it was stored — and
        // neither re-parents nor duplicates anything.
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(kids(alfa.clone()).await.len(), 2);
        assert_eq!(kids(bravo.clone()).await.len(), 1);
        let all_runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs").fetch_one(&app.db).await.unwrap();
        assert_eq!(all_runs, 5, "two parents + three children, one run each");
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

    /// Waits until the mock herdr has been asked `method` — i.e. a reconcile running on another
    /// task has taken its snapshot and is now on its way to (or blocked on) a bot's lock.
    async fn wait_for_call(env: &tt::Env, method: &str) {
        for _ in 0..200 {
            if env.herdr.methods().iter().any(|m| m == method) {
                // One beat more so it is actually parked on the lock, not still between calls.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("reconcile never called {method}");
    }

    /// **2026-09-10 23:02 (AGM 停 5.5 小時).** `restart-idle` stopped AGM while a reconcile was
    /// already running: the reconcile had listed agents — AGM's still among them — and was
    /// parked on AGM's lock behind `stop_bot`. `stop_bot` closed the pane, ended the run and
    /// let go; the reconcile got the lock *before* `start_bot` did, found no active run, and
    /// adopted the agent from its stale list as a brand-new run. `start_bot` then refused with
    /// `active run already exists`, the pane-closed event ended the adopted run, and nobody
    /// ever started AGM again.
    ///
    /// A bot's agent that is gone by the time its lock is ours must not be adopted.
    #[tokio::test]
    async fn an_agent_that_left_while_reconcile_waited_for_the_lock_is_not_adopted() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let bot = a_bot(&env, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot);
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(&run_id)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(&agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": agent, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];

        // `stop_bot` is holding the bot's lock…
        let guard = app.bot_lock(&bot).await.lock_owned().await;
        let app2 = app.clone();
        let rec = tokio::spawn(async move { super::reconcile_host(&app2, crate::config::LOCAL_HOST).await });
        wait_for_call(&env, "agent.list").await;

        // …and finishes the stop while the reconcile waits: agent gone, pane closed, run ended.
        env.herdr.agents.lock().unwrap().clear();
        client.pane_close(&pane.pane_id).await.unwrap();
        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
            .bind(db::now())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        drop(guard);
        rec.await.unwrap().unwrap();

        // What `start_bot` finds next is the only thing that matters: nothing in its way.
        assert!(
            db::active_run(&app.db, &bot).await.unwrap().is_none(),
            "reconcile adopted the agent that had just been stopped, so the restart will refuse with `active run already exists`"
        );
    }

    /// A stale list must not roll a run's status back (review 2026-09-12 a). The list said
    /// `idle`; while the reconcile waited for the lock the agent went `working`. Writing `idle`
    /// from the list would make the real `working -> idle` event a no-op (`prev == idle`):
    /// no fallback armed, no queued prompt flushed.
    #[tokio::test]
    async fn a_stale_agent_list_does_not_roll_a_runs_status_back() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let bot = a_bot(&env, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(db::ulid())
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(&agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": agent, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];

        let guard = app.bot_lock(&bot).await.lock_owned().await;
        let app2 = app.clone();
        let rec = tokio::spawn(async move { super::reconcile_host(&app2, crate::config::LOCAL_HOST).await });
        wait_for_call(&env, "agent.list").await;

        // The agent starts working while the reconcile is parked on the lock; the event
        // handler records that in the DB.
        env.herdr.agents.lock().unwrap()[0]["agent_status"] = json!("working");
        sqlx::query("UPDATE runs SET agent_status='working' WHERE bot_id=?")
            .bind(&bot)
            .execute(&app.db)
            .await
            .unwrap();
        drop(guard);
        rec.await.unwrap().unwrap();

        let r = run_of(&app, &bot).await.unwrap();
        assert_eq!(r.agent_status, "working", "the reconcile asked herdr again instead of trusting its stale list");
    }

    /// The other half of the same race: the restart *won* the lock, so by the time the
    /// reconcile gets it there is a fresh run on a fresh pane — but the reconcile's agent list
    /// is from before, and still says the agent lives on the old pane. Taking that at face
    /// value moves the new run onto a pane that no longer exists.
    #[tokio::test]
    async fn a_stale_agent_list_does_not_move_a_fresh_run_back_to_the_old_pane() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let old = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let bot = a_bot(&env, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot);
        let old_run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(&old_run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&old.tab_id)
        .bind(&old.pane_id)
        .bind(&agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": agent, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": old.tab_id, "pane_id": old.pane_id, "cwd": "/tmp/p"})];

        let guard = app.bot_lock(&bot).await.lock_owned().await;
        let app2 = app.clone();
        let rec = tokio::spawn(async move { super::reconcile_host(&app2, crate::config::LOCAL_HOST).await });
        wait_for_call(&env, "agent.list").await;

        // The restart, under the lock: old pane closed and its run ended, a new pane and a new
        // run for the same agent name.
        client.pane_close(&old.pane_id).await.unwrap();
        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
            .bind(db::now())
            .bind(&old_run)
            .execute(&app.db)
            .await
            .unwrap();
        let new = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let new_run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(&new_run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&new.tab_id)
        .bind(&new.pane_id)
        .bind(&agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": agent, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": new.tab_id, "pane_id": new.pane_id, "cwd": "/tmp/p"})];
        drop(guard);
        rec.await.unwrap().unwrap();

        let r = db::active_run(&app.db, &bot).await.unwrap().expect("the new run survives");
        assert_eq!(r.id, new_run);
        assert_eq!(r.pane_id.as_deref(), Some(new.pane_id.as_str()), "the new run was moved onto the closed pane");
    }

    /// 2026-09-11, the fix for the race above failing the same way: the restart won the lock
    /// and left a fresh run on a fresh pane, the reconcile's list still names the old pane —
    /// and when the reconcile asks herdr again, herdr cannot confirm the agent at all (for a
    /// moment after a same-named agent is restarted, `agent.get` answers not-found, or the
    /// entry still points at the closed pane). That must not read as "agent gone": every bot
    /// in the batch was marked exited that way and the orphan sweep closed the new panes.
    async fn a_fresh_run_survives_when_herdr_cannot_confirm_its_agent(herdr_still_lists_the_old_pane: bool) {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let old = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let bot = a_bot(&env, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot);
        let old_run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(&old_run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&old.tab_id)
        .bind(&old.pane_id)
        .bind(&agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let old_entry = json!({
            "name": agent, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": old.tab_id, "pane_id": old.pane_id, "cwd": "/tmp/p"});
        *env.herdr.agents.lock().unwrap() = vec![old_entry.clone()];

        let guard = app.bot_lock(&bot).await.lock_owned().await;
        let app2 = app.clone();
        let rec = tokio::spawn(async move { super::reconcile_host(&app2, crate::config::LOCAL_HOST).await });
        wait_for_call(&env, "agent.list").await;

        // The restart, under the lock: old pane closed, its run ended, a new pane and a new
        // run for the same agent name — which herdr does not confirm yet.
        client.pane_close(&old.pane_id).await.unwrap();
        sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
            .bind(db::now())
            .bind(&old_run)
            .execute(&app.db)
            .await
            .unwrap();
        let new = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let new_run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'test',?)",
        )
        .bind(&new_run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&new.tab_id)
        .bind(&new.pane_id)
        .bind(&agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        // What herdr really shows then: the new pane's occupant with its name cleared — and,
        // in one variant, the old entry still lingering on the closed pane.
        let nameless = json!({
            "name": null, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": new.tab_id, "pane_id": new.pane_id, "cwd": "/tmp/p"});
        *env.herdr.agents.lock().unwrap() = if herdr_still_lists_the_old_pane { vec![old_entry, nameless] } else { vec![nameless] };
        drop(guard);
        rec.await.unwrap().unwrap();

        let r = db::active_run(&app.db, &bot).await.unwrap().expect("the fresh run was marked exited");
        assert_eq!(r.id, new_run);
        assert_eq!(r.state, "running");
        assert_eq!(r.pane_id.as_deref(), Some(new.pane_id.as_str()), "the fresh run was moved off its pane");
        assert!(
            client.pane_get(&new.pane_id).await.unwrap().is_some(),
            "the orphan sweep closed the pane the restart had just opened"
        );
        let rename = env.herdr.first_call("agent.rename").expect("the cleared name is put back");
        assert_eq!(rename["target"], new.pane_id);
        assert_eq!(rename["name"], agent);
    }

    /// The real shape of 2026-09-11 on herdr 0.8.2: after the same-named restart the new
    /// agent is in its pane (`pane.get` says `agent: claude`) but herdr has cleared its name —
    /// `agent.list` carries it with `name: null`, `agent.get <name>` is not-found. The run is
    /// kept and the name is put back with `agent.rename <pane> <name>`.
    #[tokio::test]
    async fn a_run_whose_agent_lost_its_name_is_kept_and_renamed() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "alfa", json!({})).await.unwrap();
        let bot = a_bot(&env, "alfa").await;
        let agent = crate::config::agent_name("proj", &bot);
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'starting','unknown',?,?,?,?,'test',?)",
        )
        .bind(&run_id)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind(&agent)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": null, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let r = db::active_run(&app.db, &bot).await.unwrap().expect("the run was marked exited over a lost name");
        assert_eq!(r.id, run_id);
        assert_eq!(r.state, "running");
        assert_eq!(r.agent_status, "idle", "status is read off the pane");
        assert_eq!(r.pane_id.as_deref(), Some(pane.pane_id.as_str()));
        let rename = env.herdr.first_call("agent.rename").expect("the name is put back");
        assert_eq!(rename["target"], pane.pane_id);
        assert_eq!(rename["name"], agent);
        assert_eq!(env.herdr.agents.lock().unwrap()[0]["name"], agent, "herdr knows the agent by name again");
        assert!(client.pane_get(&pane.pane_id).await.unwrap().is_some(), "the pane was not swept as an orphan");
    }

    #[tokio::test]
    async fn a_fresh_run_survives_when_agent_get_says_not_found() {
        a_fresh_run_survives_when_herdr_cannot_confirm_its_agent(false).await;
    }

    #[tokio::test]
    async fn a_fresh_run_survives_when_agent_get_still_points_at_the_closed_pane() {
        a_fresh_run_survives_when_herdr_cannot_confirm_its_agent(true).await;
    }

    /// A child row with a run on `pane`, the way reconcile adopts one (#60 tests).
    async fn a_child(env: &tt::Env, parent: &str, name: &str, agent: &str, ws: &str, tab: &str, pane: &str) -> (String, String) {
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, auto_approve, env_json, managed_by, parent_bot_id, hook_token, created_at)
             VALUES (?,?,?,'claude','[]',0,0,1,'{}','child',?,'tok',?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind(name)
        .bind(parent)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, adopted, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,1,?,'test',?)",
        )
        .bind(&run)
        .bind(&bot)
        .bind(ws)
        .bind(tab)
        .bind(pane)
        .bind(agent)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        (bot, run)
    }

    /// **#60.** `herdr pane close` on a child: herdr's `pane_closed` event arrives first and
    /// `events::end_runs_for_pane` ends the run; only *then* does a reconcile run. It used to
    /// find "no active run, no agent" and do nothing (`(None, None) => {}`), so the child stayed
    /// in the sidebar forever — 19 of them on 2026-09-11 (C1-部署console alone had 10).
    #[tokio::test]
    async fn a_child_whose_pane_closed_before_the_reconcile_is_retired() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
        let parent = a_bot(&env, "alfa").await;
        let kid_agent = format!("{}-kid", crate::config::agent_name("proj", &parent));
        let (kid, run) = a_child(&env, &parent, "kid", &kid_agent, &ws.workspace_id, &pane.tab_id, &pane.pane_id).await;

        // `pane_closed` first: the pane and its agent are gone, and the run is ended the way
        // `end_runs_for_pane` ends it…
        client.pane_close(&pane.pane_id).await.unwrap();
        env.herdr.agents.lock().unwrap().clear();
        crate::lifecycle::mark_run_exited(&app, &run, "pane exited").await;
        // …then the reconcile.
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let b = db::bot(&app.db, &kid).await.unwrap().unwrap();
        assert!(b.deleted_at.is_some(), "a child whose pane was closed must leave the sidebar");
    }

    /// The limit of the rule above: an ended run is not enough. If herdr still lists the
    /// child's agent (its pane was moved, say, and the old pane id reported closed), the child
    /// is alive and gets its run back instead of being retired.
    #[tokio::test]
    async fn a_child_whose_run_ended_but_whose_agent_is_still_listed_is_kept() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, "/tmp/p", "kid", json!({})).await.unwrap();
        let parent = a_bot(&env, "alfa").await;
        let kid_agent = format!("{}-kid", crate::config::agent_name("proj", &parent));
        let (kid, run) = a_child(&env, &parent, "kid", &kid_agent, &ws.workspace_id, &pane.tab_id, &pane.pane_id).await;
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": kid_agent, "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id, "cwd": "/tmp/p"})];
        crate::lifecycle::mark_run_exited(&app, &run, "pane exited").await;

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let b = db::bot(&app.db, &kid).await.unwrap().unwrap();
        assert!(b.deleted_at.is_none(), "the agent is still there, so the child is not retired");
        let r = run_of(&app, &kid).await.unwrap();
        assert_eq!(r.state, "running", "and it gets a run again");
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

    /// A stand-in for `ps eww -p <pid>`: what each pid is running with, plus every pid it was
    /// asked about — one `ps` (one *ssh*, on a remote host) per child per reconnect burst is
    /// exactly the storm this must not make.
    struct FakeProcEnv {
        envs: std::collections::BTreeMap<i64, std::collections::BTreeMap<String, String>>,
        asked: std::sync::Mutex<Vec<i64>>,
    }

    impl crate::pane_identity::ProcEnv for FakeProcEnv {
        fn env_of<'a>(
            &'a self,
            _app: &'a Arc<App>,
            _host: &'a str,
            pid: i64,
        ) -> futures::future::BoxFuture<'a, Option<std::collections::BTreeMap<String, String>>> {
            Box::pin(async move {
                self.asked.lock().unwrap().push(pid);
                self.envs.get(&pid).cloned()
            })
        }
    }

    fn claude_env(dir: &str) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), dir.to_string())])
    }

    fn claude_identity(name: &str, dir: &str) -> crate::config::IdentityCfg {
        crate::config::IdentityCfg {
            name: name.into(),
            kind: "claude".into(),
            env: claude_env(dir),
            args: vec![],
        }
    }

    /// A child's **account** is not its parent's (SPEC §16.6). `herdr pane split --env
    /// CLAUDE_CONFIG_DIR=…` is how one bot puts a helper on another login, and the adopt has
    /// nothing but the parent's row to copy — so the pane's own process is what has to be
    /// asked, or every token the child burns is billed to the wrong account in the sidebar and
    /// in `/api/quota`.
    #[tokio::test]
    async fn a_spawned_childs_identity_is_the_account_its_own_pane_runs_on() {
        let env = tt::env().await;
        let app = env.app.clone();
        let home = dirs::home_dir().unwrap().to_string_lossy().to_string();
        // Two accounts, spelled the two ways an identity may spell one.
        app.cfg
            .update(|c| {
                c.identities =
                    vec![claude_identity("cc1", "$HOME/.claude-ccompany"), claude_identity("cc2", "~/.claude-cc2")];
                Ok(())
            })
            .await
            .unwrap();

        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let head = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();
        let lost = client.pane_split(&root.pane_id, "right", "/tmp/p", json!({})).await.unwrap();

        let parent = a_bot(&env, "alfa").await;
        sqlx::query("UPDATE bots SET identity = 'cc1' WHERE id = ?")
            .bind(&parent)
            .execute(&app.db)
            .await
            .unwrap();
        let parent_agent = crate::config::agent_name("proj", &parent);
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
            json!({"name": format!("{parent_agent}-head"), "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": head.tab_id, "pane_id": head.pane_id, "cwd": "/tmp/p"}),
            json!({"name": format!("{parent_agent}-lost"), "agent": "claude", "agent_status": "idle",
                   "workspace_id": ws.workspace_id, "tab_id": lost.tab_id, "pane_id": lost.pane_id, "cwd": "/tmp/p"}),
        ];
        for pane in [&root.pane_id, &head.pane_id, &lost.pane_id] {
            env.herdr.set_argv(pane, &["claude", "--dangerously-skip-permissions", "--model", "opus"]);
        }
        env.herdr.set_pid(&head.pane_id, 4924);
        env.herdr.set_pid(&lost.pane_id, 4925);
        // What each pane is really running under. The parent's pane says `cc2` as well — and
        // must never be read, because a user bot's identity is the user's setting, projected
        // back into `config.toml`.
        let fake = Arc::new(FakeProcEnv {
            envs: std::collections::BTreeMap::from([
                (1, claude_env(&format!("{home}/.claude-cc2"))),
                (4924, claude_env(&format!("{home}/.claude-cc2/"))),
                (4925, claude_env("/tmp/an-account-nobody-configured")),
            ]),
            asked: std::sync::Mutex::new(Vec::new()),
        });
        app.proc_env.set(fake.clone());

        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();

        let kid = |name: &str| {
            let db = app.db.clone();
            let name = name.to_string();
            async move {
                sqlx::query_as::<_, db::Bot>("SELECT * FROM bots WHERE name = ?")
                    .bind(&name)
                    .fetch_one(&db)
                    .await
                    .expect("the child was adopted")
            }
        };
        let head_bot = kid("head").await;
        assert_eq!(head_bot.managed_by, "child");
        assert_eq!(head_bot.identity.as_deref(), Some("cc2"), "read off its own pane's CLAUDE_CONFIG_DIR");
        assert_eq!(head_bot.model.as_deref(), Some("opus"), "the same process_info still fills the model");

        // A directory no identity claims is not an answer: the inherited value stays, because
        // a value copied from the parent may be right and NULL is certainly wrong.
        assert_eq!(kid("lost").await.identity.as_deref(), Some("cc1"));

        let p = db::bot(&app.db, &parent).await.unwrap().unwrap();
        assert_eq!(p.identity.as_deref(), Some("cc1"), "a user bot's account is configuration, never scraped");

        // Asked once per child, and never about the parent — a reconnect replays a burst of
        // reconciles, and a `ps` per child per pass is an ssh storm on a remote host.
        super::reconcile_host(&app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(*fake.asked.lock().unwrap(), vec![4924, 4925]);
        assert_eq!(kid("head").await.identity.as_deref(), Some("cc2"));
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
