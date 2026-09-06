//! Discovery and adoption of agents already running in the user's local Herdr `default`
//! session.
//!
//! The manager owns its configured named session, but it can safely observe the user's default
//! session. Only recognised agent panes whose current working directory exactly matches an
//! existing local Project are imported. Ordinary shell panes and panes from other directories
//! remain entirely outside the manager's ownership.

use crate::config::{self, LOCAL_HOST};
use crate::db;
use crate::herdr::AgentInfo;
use crate::state::App;
use anyhow::Result;
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const SESSION: &str = "default";
const POLL_INTERVAL: Duration = Duration::from_secs(8);

/// Poll as a backstop for agents started after the daemon and for Herdr versions that do not
/// emit `pane.agent_detected` for every launch.
pub fn spawn_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if let Err(e) = sync(&app).await {
                tracing::debug!(session = SESSION, error = ?e, "default session poll failed");
            }
        }
    });
}

/// Observe the default session and import matching agents into the existing Project list.
///
/// This function intentionally does not create/close workspaces or panes. A default-session
/// agent is adopted as an already-running Run and its pane remains user-owned.
pub async fn sync(app: &Arc<App>) -> Result<()> {
    let _guard = app.default_sync_lock.lock().await;
    let client = app.default_herdr.clone();

    if let Err(e) = client.ping().await {
        set_connected(app, false).await;
        return Err(e);
    }
    set_connected(app, true).await;

    let agents = client
        .agent_list()
        .await
        .map_err(|e| anyhow::anyhow!("default session agent.list failed: {e}"))?;
    let projects: Vec<db::Project> = db::live_projects(&app.db)
        .await?
        .into_iter()
        .filter(|p| p.host == LOCAL_HOST)
        .collect();
    let mut seen_bots = HashSet::new();

    for agent in agents {
        let Some(kind) = agent_kind(&agent) else { continue };
        let Some(agent_name) = agent.name.as_deref().filter(|s| !s.trim().is_empty()) else { continue };
        let Some(cwd) = agent_cwd(&agent) else { continue };
        let Some(project) = projects.iter().find(|p| same_workdir(cwd, &p.path)) else { continue };

        let (bot, created) = ensure_imported_bot(app, project, agent_name, kind).await?;
        let bot_id = bot.id.clone();
        seen_bots.insert(bot_id.clone());
        let lock = app.bot_lock(&bot_id).await;
        let _bot_guard = lock.lock().await;
        let status = agent.agent_status.normalized().as_str().to_string();
        let active = db::active_run(&app.db, &bot_id).await?;

        if let Some(run) = active {
            // An imported bot can only have a default-session run. Keep an unexpected active
            // run in another session untouched rather than moving it under this agent.
            if run.herdr_session.as_deref().map(|s| s != SESSION).unwrap_or(false) {
                continue;
            }
            sqlx::query(
                "UPDATE runs SET pane_id=?, workspace_id=?, tab_id=?, agent_status=?, agent_name=?,
                 herdr_session=?, adopted=1, state=CASE WHEN state='starting' THEN 'running' ELSE state END,
                 ended_at=NULL WHERE id=?",
            )
            .bind(&agent.pane_id)
            .bind(&agent.workspace_id)
            .bind(&agent.tab_id)
            .bind(&status)
            .bind(agent_name)
            .bind(SESSION)
            .bind(&run.id)
            .execute(&app.db)
            .await?;
            if run.pane_id.as_deref() != Some(agent.pane_id.as_str()) {
                if let Some(old) = run.pane_id.as_deref() {
                    crate::events::unwatch_pane_on_session(app, LOCAL_HOST, SESSION, old).await;
                }
            }
            crate::events::watch_pane_on_session(app, LOCAL_HOST, SESSION, &agent.pane_id).await;
            if bot.kind == "codex" {
                crate::lifecycle::schedule_codex_notice_capture(app, &bot.id, &run.id);
            }
        } else {
            let run_id = db::ulid();
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted,
                 agent_name, herdr_session, started_at) VALUES (?,?,'running',?,?,?,?,1,?,?,?)",
            )
            .bind(&run_id)
            .bind(&bot.id)
            .bind(&status)
            .bind(&agent.workspace_id)
            .bind(&agent.pane_id)
            .bind(&agent.tab_id)
            .bind(agent_name)
            .bind(SESSION)
            .bind(db::now())
            .execute(&app.db)
            .await?;
            crate::events::watch_pane_on_session(app, LOCAL_HOST, SESSION, &agent.pane_id).await;
            if bot.kind == "codex" {
                crate::lifecycle::schedule_codex_notice_capture(app, &bot.id, &run_id);
            }
        }
        if created {
            app.emit("bot_changed", json!({"bot_id": bot.id})).await;
            app.emit("project_changed", json!({"project_id": project.id})).await;
            tracing::info!(bot = %bot.name, kind = %bot.kind, project = %project.label, agent = agent_name, "imported default-session agent");
        }
        app.emit_bot_status(&bot.id).await;
    }

    // A vanished default-session agent ends its Run, but its external pane is deliberately not
    // closed. The next sync can still observe a newly detected agent in that pane.
    for bot in db::live_bots(&app.db).await?.into_iter().filter(|b| b.herdr_session.as_deref() == Some(SESSION)) {
        if seen_bots.contains(&bot.id) {
            continue;
        }
        if let Some(run) = db::active_run(&app.db, &bot.id).await? {
            crate::lifecycle::mark_run_exited(app, &run.id, "agent not found in default session").await;
        }
    }
    Ok(())
}

async fn set_connected(app: &Arc<App>, connected: bool) {
    crate::state::set_default_connected(app, connected).await;
}

fn agent_kind(agent: &AgentInfo) -> Option<&'static str> {
    match agent.agent.as_deref()?.to_ascii_lowercase().as_str() {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "grok" => Some("grok"),
        _ => None,
    }
}

fn agent_cwd(agent: &AgentInfo) -> Option<&str> {
    agent.foreground_cwd.as_deref().or(agent.cwd.as_deref()).filter(|s| !s.trim().is_empty())
}

fn canonical_or_original(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Exact directory matching. A child directory is intentionally not considered the same
/// Project; this prevents importing an unrelated agent from a nested checkout.
pub fn same_workdir(cwd: &str, project_path: &str) -> bool {
    canonical_or_original(cwd) == canonical_or_original(project_path)
}

fn imported_name(raw: &str, used: &HashSet<String>) -> String {
    let raw = raw.trim();
    let mut base: String = raw
        .chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '@' | ',' | ':' | ';'))
        .take(32)
        .collect();
    if !config::valid_bot_name(&base) {
        base = "default-bot".to_string();
    }
    if !used.contains(&base) {
        return base;
    }
    for n in 2..=9999 {
        let suffix = format!("-{n}");
        let room = 32usize.saturating_sub(suffix.chars().count());
        let prefix: String = base.chars().take(room).collect();
        let candidate = format!("{prefix}{suffix}");
        if config::valid_bot_name(&candidate) && !used.contains(&candidate) {
            return candidate;
        }
    }
    format!("default-{}", db::ulid().to_ascii_lowercase().chars().take(8).collect::<String>())
}

async fn ensure_imported_bot(
    app: &Arc<App>,
    project: &db::Project,
    agent_name: &str,
    kind: &str,
) -> Result<(db::Bot, bool)> {
    // A renamed imported bot is found through its historical default-session Run. A bot that
    // has never had a Run is found by its original nickname.
    let existing_id: Option<String> = sqlx::query_scalar(
        "SELECT b.id FROM bots b
         WHERE b.project_id=? AND b.deleted_at IS NULL AND b.herdr_session=?
           AND (b.name=? OR EXISTS (
             SELECT 1 FROM runs r WHERE r.bot_id=b.id AND r.agent_name=? AND r.herdr_session=?
           ))
         ORDER BY CASE WHEN b.name=? THEN 0 ELSE 1 END, b.created_at LIMIT 1",
    )
    .bind(&project.id)
    .bind(SESSION)
    .bind(agent_name)
    .bind(agent_name)
    .bind(SESSION)
    .bind(agent_name)
    .fetch_optional(&app.db)
    .await?;
    if let Some(id) = existing_id {
        let bot = db::bot(&app.db, &id).await?.ok_or_else(|| anyhow::anyhow!("imported bot disappeared"))?;
        return Ok((bot, false));
    }

    let bot_id = db::ulid();
    let name = app
        .cfg
        .update(|cfg| {
            let p = cfg
                .projects
                .iter_mut()
                .find(|p| p.id.as_deref() == Some(project.id.as_str()))
                .ok_or_else(|| anyhow::anyhow!("project disappeared from config.toml"))?;
            let used: HashSet<String> = p.bots.iter().map(|b| b.name.clone()).collect();
            let name = imported_name(agent_name, &used);
            p.bots.push(config::BotCfg {
                id: Some(bot_id.clone()),
                name: name.clone(),
                kind: kind.to_string(),
                model: None,
                effort: None,
                fast: false,
                persona: None,
                args: vec![],
                autostart: false,
                // The agent already owns its process/config; do not rewrite its hooks when a
                // default-session pane is merely adopted.
                inject_hooks: false,
                auto_approve: false,
                identity: None,
                env: Default::default(),
                herdr_session: Some(SESSION.to_string()),
            });
            Ok(name)
        })
        .await?;
    crate::projection::project_config(&app.cfg, &app.db).await?;
    let bot = db::bot(&app.db, &bot_id).await?.ok_or_else(|| anyhow::anyhow!("imported bot was not projected"))?;
    tracing::debug!(bot = %name, agent = agent_name, "default-session bot config created");
    Ok((bot, true))
}

#[cfg(test)]
mod tests {
    use super::{imported_name, same_workdir};
    use std::collections::HashSet;

    #[test]
    fn matches_only_the_same_directory() {
        assert!(same_workdir("/tmp", "/tmp"));
        assert!(!same_workdir("/tmp/child", "/tmp"));
    }

    #[test]
    fn imported_names_are_unique_and_safe() {
        let used = HashSet::from(["agent".to_string()]);
        assert_eq!(imported_name("agent", &used), "agent-2");
        assert_eq!(imported_name("agent bad", &HashSet::new()), "agentbad");
        assert_eq!(imported_name("bad name", &HashSet::from(["badname".to_string()])), "badname-2");
    }
}
