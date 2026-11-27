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
use anyhow::Result;
use serde_json::json;
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

pub const SESSION: &str = "default";

/// Host operation that applies one default-session import through the daemon's guarded
/// config-to-database projection path. Keeping the mutation closure here preserves the
/// projection's serialized read/guard/write/project transaction without coupling lifecycle to
/// the daemon composition module.
pub trait DefaultSessionProjection: Send + Sync {
    fn update_and_project<'a, T, F>(&'a self, update: F) -> Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>
    where
        F: FnOnce(&mut config::ConfigFile) -> Result<T> + Send + 'a,
        T: Send + 'a;
}

/// Observe the default session and import matching agents into the existing Project list.
///
/// This function intentionally does not create/close workspaces or panes. A default-session
/// agent is adopted as an already-running Run and its pane remains user-owned.
pub async fn sync(app: &(impl crate::capabilities::BotLocks + crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::capabilities::Emit + crate::capabilities::Isolation + crate::default_session::DefaultSyncLock + DefaultSessionProjection + crate::events::ports::HostSidePort + crate::events::ports::IngressCommands + crate::events::ports::TurnCommands + crate::herdr::LocalHerdr)) -> Result<()> {
    if app.isolated() {
        return Ok(());
    }
    let _guard = app.default_sync_lock().lock().await;
    let client = app.default_herdr().clone();

    if let Err(e) = client.ping().await {
        set_connected(app, false).await;
        return Err(e);
    }
    set_connected(app, true).await;

    let agents = client
        .agent_list()
        .await
        .map_err(|e| anyhow::anyhow!("default session agent.list failed: {e}"))?;
    let projects: Vec<db::Project> = db::live_projects(app.db())
        .await?
        .into_iter()
        .filter(|p| p.host == LOCAL_HOST)
        .filter(|p| p.handed_off_to.is_none())
        .collect();
    let mut seen_bots = HashSet::new();
    let mut native_ids = HashSet::new();
    let mut ambiguous_native_ids = HashSet::new();
    for agent in &agents {
        if let Some(native) = crate::lifecycle::dead_panes::herdr_session_id(agent) {
            let key = (agent_kind(agent), native);
            if !native_ids.insert(key.clone()) { ambiguous_native_ids.insert(key); }
        }
    }

    for agent in agents {
        if agent.pane_id.is_empty() { continue; }
        if crate::lifecycle::dead_panes::herdr_session_id(&agent)
            .is_some_and(|native| ambiguous_native_ids.contains(&(agent_kind(&agent), native))) { continue; }
        let Some(kind) = agent_kind(&agent) else { continue };
        let agent_name = agent.name.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or("default-bot");
        let Some(cwd) = agent_cwd(&agent) else { continue };
        let Some(project) = projects.iter().find(|p| same_workdir(cwd, &p.path)) else { continue };

        let occupied: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT r.bot_id, r.native_session_id FROM runs r JOIN bots b ON b.id=r.bot_id
             WHERE b.project_id=? AND b.deleted_at IS NULL AND r.herdr_session=? AND r.pane_id=?
             AND r.state IN ('starting','running','stopping') LIMIT 1",
        ).bind(&project.id).bind(SESSION).bind(&agent.pane_id).fetch_optional(app.db()).await?;
        if let Some((bot_id, Some(native))) = occupied {
            if crate::lifecycle::dead_panes::herdr_session_id(&agent).as_deref() != Some(native.as_str()) {
                seen_bots.insert(bot_id);
                continue;
            }
        }
        let (bot, created) = ensure_imported_bot(app, project, &agent, agent_name, kind).await?;
        let bot_id = bot.id.clone();
        seen_bots.insert(bot_id.clone());
        let lock = app.bot_lock(&bot_id).await;
        let _bot_guard = lock.lock().await;
        let status = agent.agent_status.normalized().as_str().to_string();
        let active = db::active_run(app.db(), &bot_id).await?;

        if let Some(run) = active {
            if run.herdr_session.as_deref().map(|s| s != SESSION).unwrap_or(false) {
                continue;
            }
            let native = crate::lifecycle::dead_panes::herdr_session_id(&agent);
            if run.native_session_id.as_ref().is_some_and(|sid| Some(sid.as_str()) != native.as_deref())
                || (run.native_session_id.is_none() && run.pane_id.as_deref() != Some(agent.pane_id.as_str()))
            {
                continue;
            }
            sqlx::query(
                "UPDATE runs SET pane_id=?, workspace_id=?, tab_id=?, agent_status=?, agent_name=?,
                 herdr_session=?, native_session_id=COALESCE(native_session_id, ?), adopted=1, state=CASE WHEN state='starting' THEN 'running' ELSE state END,
                 ended_at=NULL WHERE id=?",
            )
            .bind(&agent.pane_id)
            .bind(&agent.workspace_id)
            .bind(&agent.tab_id)
            .bind(&status)
            .bind(agent_name)
            .bind(SESSION)
            .bind(crate::lifecycle::dead_panes::herdr_session_id(&agent))
            .bind(&run.id)
            .execute(app.db())
            .await?;
            if run.pane_id.as_deref() != Some(agent.pane_id.as_str()) {
                if let Some(old) = run.pane_id.as_deref() {
                    app.unwatch_pane_on_session(LOCAL_HOST, SESSION, old).await;
                }
            }
            app.watch_pane_on_session(LOCAL_HOST, SESSION, &agent.pane_id).await;
            if bot.kind == "codex" {
                app.schedule_codex_notice_capture(&bot.id, &run.id);
            }
        } else {
            let run_id = db::ulid();
            sqlx::query(
                "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, tab_id, adopted,
                 agent_name, herdr_session, native_session_id, started_at) VALUES (?,?,'running',?,?,?,?,1,?,?,?,?)",
            )
            .bind(&run_id)
            .bind(&bot.id)
            .bind(&status)
            .bind(&agent.workspace_id)
            .bind(&agent.pane_id)
            .bind(&agent.tab_id)
            .bind(agent_name)
            .bind(SESSION)
            .bind(crate::lifecycle::dead_panes::herdr_session_id(&agent))
            .bind(db::now())
            .execute(app.db())
            .await?;
            app.watch_pane_on_session(LOCAL_HOST, SESSION, &agent.pane_id).await;
            if bot.kind == "codex" {
                app.schedule_codex_notice_capture(&bot.id, &run_id);
            }
        }
        if created {
            app.emit("bot_changed", json!({"bot_id": bot.id})).await;
            app.emit("project_changed", json!({"project_id": project.id})).await;
            tracing::info!(bot = %bot.name, kind = %bot.kind, project = %project.label, agent = agent_name, "imported default-session agent");
        }
        app.emit_bot_status(&bot.id).await;
    }

    for bot in db::live_bots(app.db()).await?.into_iter().filter(|b| b.herdr_session.as_deref() == Some(SESSION)) {
        if seen_bots.contains(&bot.id) {
            continue;
        }
        if let Some(run) = db::active_run(app.db(), &bot.id).await? {
            let target = run.pane_id.as_deref().filter(|p| !p.is_empty());
            let still_there = match target {
                Some(t) => match client.agent_get(t).await {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(e) => {
                        tracing::warn!(bot = %bot.name, error = %e, "default session: could not ask herdr if the agent is still there; leaving the run this pass");
                        true
                    }
                },
                None => true,
            };
            if still_there {
                continue;
            }
            app.mark_run_exited(&run.id, "agent not found in default session").await;
        }
    }
    Ok(())
}

/// True when `e` only says the default session is not running: its socket file is missing or
/// nothing listens on it. The user opening and closing that session is normal, so callers log
/// this at debug instead of warning.
pub fn session_absent(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<crate::herdr::HerdrUnreachable>())
        .any(|u| matches!(u.source.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused))
}

async fn set_connected(app: &impl crate::events::ports::HostSidePort, connected: bool) {
    app.set_default_connected(connected).await;
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
    app: &(impl crate::capabilities::Db + DefaultSessionProjection),
    project: &db::Project,
    agent: &AgentInfo,
    agent_name: &str,
    kind: &str,
) -> Result<(db::Bot, bool)> {
    let native = crate::lifecycle::dead_panes::herdr_session_id(agent);
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT b.id FROM bots b JOIN runs r ON r.bot_id=b.id
         WHERE b.project_id=? AND b.deleted_at IS NULL AND b.herdr_session=? AND b.kind=?
           AND r.herdr_session=? AND (
             (? IS NOT NULL AND r.native_session_id=?) OR
             (r.state IN ('starting','running','stopping') AND r.pane_id=?
              AND r.native_session_id IS NULL))",
    ).bind(&project.id).bind(SESSION).bind(kind).bind(SESSION)
        .bind(&native).bind(&native).bind(&agent.pane_id).fetch_all(app.db()).await?;
    anyhow::ensure!(ids.len() <= 1, "ambiguous default-session identity");
    let existing_id = ids.into_iter().next();
    if let Some(id) = existing_id {
        let bot = db::bot(app.db(), &id).await?.ok_or_else(|| anyhow::anyhow!("imported bot disappeared"))?;
        return Ok((bot, false));
    }

    let bot_id = db::ulid();
    let name = app.update_and_project(|cfg| {
        let p = cfg
            .projects
            .iter_mut()
            .find(|p| p.id.as_deref() == Some(project.id.as_str()))
            .ok_or_else(|| anyhow::anyhow!("project disappeared from config.toml"))?;
        let used: HashSet<String> = p.bots.iter().map(|b| b.name.clone()).collect();
        let name = imported_name(agent_name, &used);
        p.bots.push(config::BotCfg { role: None, manager_use: true, track: None, runtime_override: None, fork_from: None,
            id: Some(bot_id.clone()),
            name: name.clone(),
            kind: kind.to_string(),
            model: None,
            effort: None,
            fast: false,
            persona: None,
            args: vec![],
            autostart: false,
            inject_hooks: false,
            auto_approve: false,
            identity: None,
            env: Default::default(),
            herdr_session: Some(SESSION.to_string()),
            create_request_id: None,
            create_fingerprint: None,
            ..Default::default()
        });
        Ok(name)
    })
    .await?;
    let bot = db::bot(app.db(), &bot_id).await?.ok_or_else(|| anyhow::anyhow!("imported bot was not projected"))?;
    tracing::debug!(bot = %name, agent = agent_name, "default-session bot config created");
    Ok((bot, true))
}

#[cfg(test)]
mod tests {
    use super::{imported_name, same_workdir, session_absent, sync, SESSION};
    use crate::herdr::{HerdrClient, HerdrUnreachable};
    use std::collections::HashSet;
    use std::io::{Error, ErrorKind};
    fn unreachable(kind: ErrorKind) -> anyhow::Error {
        HerdrUnreachable { socket: "/x/herdr.sock".into(), source: Error::from(kind) }.into()
    }
    #[test]
    fn a_missing_or_refused_socket_is_an_absent_session_and_other_errors_are_not() {
        assert!(session_absent(&unreachable(ErrorKind::NotFound)));
        assert!(session_absent(&unreachable(ErrorKind::ConnectionRefused)));
        assert!(session_absent(&unreachable(ErrorKind::NotFound).context("ping")));
        assert!(!session_absent(&unreachable(ErrorKind::PermissionDenied)));
        assert!(!session_absent(&anyhow::anyhow!("No such file or directory (os error 2)")));
        assert!(!session_absent(&anyhow::Error::from(Error::from(ErrorKind::NotFound))));
    }
    #[tokio::test]
    async fn pinging_a_socket_that_does_not_exist_reads_as_an_absent_session() {
        let err = HerdrClient::new(std::path::PathBuf::from("/nonexistent-am-default-session/herdr.sock")).ping().await.unwrap_err();
        assert!(session_absent(&err), "{err:#}");
    }
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

/// default session 同步的互斥鎖。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait DefaultSyncLock: Send + Sync {
    fn default_sync_lock(&self) -> &tokio::sync::Mutex<()>;
}

/// SPEC §6.5.1: a bot from the user's `default` session is observed, never driven — start
/// would create a workspace in their session, restart would close their pane (review 2026-09-12 #4).
pub fn refuse_default_session(bot: &db::Bot) -> crate::lc_error::LcResult<()> {
    if bot.herdr_session.as_deref() == Some(SESSION) {
        return Err(crate::lc_error::LcError::conflict(
            "default_session",
            json!({"bot_id": bot.id,
                   "message": "這顆是從你自己的 herdr default session 匯入的，daemon 只觀察、不替它開或關 pane：要重啟請在那個終端裡自己做。"}),
        ));
    }
    Ok(())
}
