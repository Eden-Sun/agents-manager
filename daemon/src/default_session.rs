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
        let mut last_error = None;
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if let Err(e) = sync(&app).await {
                let message = format!("{e:#}");
                if last_error.as_deref() != Some(message.as_str()) {
                    tracing::warn!(session = SESSION, error = %message, "default session poll failed");
                    last_error = Some(message);
                }
            } else {
                last_error = None;
            }
        }
    });
}

/// Observe the default session and import matching agents into the existing Project list.
///
/// This function intentionally does not create/close workspaces or panes. A default-session
/// agent is adopted as an already-running Run and its pane remains user-owned.
pub async fn sync(app: &Arc<App>) -> Result<()> {
    // 使用者的 default session 屬於正式那顆 daemon；隔離實例收編它等於兩顆搶同一批 pane。
    if app.isolated() {
        return Ok(());
    }
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
    // `agent.list` 空陣列不是失敗：herdr 重啟中清單還沒填好時，list 空、get 還找得到。
    // 沒再問就標 exited，等於把還在跑的 default-session bot 收掉（之後可能再 import 成另一顆）。
    for bot in db::live_bots(&app.db).await?.into_iter().filter(|b| b.herdr_session.as_deref() == Some(SESSION)) {
        if seen_bots.contains(&bot.id) {
            continue;
        }
        if let Some(run) = db::active_run(&app.db, &bot.id).await? {
            let target = run.pane_id.as_deref().filter(|p| !p.is_empty()).map(str::to_string).or_else(|| run.agent_name.clone());
            let still_there = match target.as_deref() {
                Some(t) => match client.agent_get(t).await {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(e) => {
                        tracing::warn!(bot = %bot.name, error = %e, "default session: could not ask herdr if the agent is still there; leaving the run this pass");
                        true
                    }
                },
                None => false,
            };
            if still_there {
                continue;
            }
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
    let name = crate::projection::update_and_project(&app.cfg, &app.db, |cfg| {
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
            instruction_files: None,
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
    let bot = db::bot(&app.db, &bot_id).await?.ok_or_else(|| anyhow::anyhow!("imported bot was not projected"))?;
    tracing::debug!(bot = %name, agent = agent_name, "default-session bot config created");
    Ok((bot, true))
}

#[cfg(test)]
mod tests {
    use super::{imported_name, same_workdir, sync, SESSION};
    use crate::db;
    use crate::testing as tt;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::atomic::Ordering;

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

    /// `agent.list` 暫時回空、但 `agent.get` 還找得到：不能把 default-session 的 run 標成 exited。
    #[tokio::test]
    async fn an_empty_agent_list_does_not_exit_a_default_session_run_that_is_still_there() {
        let env = tt::env().await;
        let app = env.app.clone();
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));
        let (ws, _root) = client.workspace_create("/tmp/p", "proj", json!({})).await.unwrap();
        let pane = client.tab_create(&ws.workspace_id, env.repo.to_str().unwrap(), "imported", json!({})).await.unwrap();
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, herdr_session, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind("imported")
        .bind(SESSION)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, tab_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,?,?,'default',?)",
        )
        .bind(&run)
        .bind(&bot)
        .bind(&ws.workspace_id)
        .bind(&pane.tab_id)
        .bind(&pane.pane_id)
        .bind("imported")
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        *env.herdr.agents.lock().unwrap() = vec![json!({
            "name": "imported", "agent": "claude", "agent_status": "idle",
            "workspace_id": ws.workspace_id, "tab_id": pane.tab_id, "pane_id": pane.pane_id,
            "cwd": env.repo.to_string_lossy(),
        })];
        env.herdr.hide_agent_list.store(true, Ordering::SeqCst);

        sync(&app).await.unwrap();

        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "running", "list 空、get 還在：不能當成 agent 消失");
    }

    /// list 空、get 也說沒有：這次是真的消失，run 才該收。
    #[tokio::test]
    async fn a_default_session_run_still_exits_when_herdr_confirms_the_agent_is_gone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, herdr_session, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok',?,?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind("gone")
        .bind(SESSION)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?,'default',?)",
        )
        .bind(&run)
        .bind(&bot)
        .bind("w1:pGone")
        .bind("gone")
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        sync(&app).await.unwrap();

        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "exited");
    }
}
