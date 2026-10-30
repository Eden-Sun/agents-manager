//! Host/session markers protect naturally inherited old shell environments.
use std::sync::Arc;
use crate::{db, state::App};

pub async fn sessions(app: &Arc<App>, project: &db::Project) -> anyhow::Result<Vec<String>> {
    let bots = db::live_bots(&app.db).await?;
    let mut sessions = std::collections::BTreeSet::new();
    for bot in bots.iter().filter(|b| b.project_id == project.id) {
        let session = app.session_for_bot(bot, &project.host).await.ok_or_else(|| anyhow::anyhow!("host session unavailable"))?;
        sessions.insert(session);
        if let Some(run_session) = db::active_run(&app.db, &bot.id).await?.and_then(|run| run.herdr_session) { sessions.insert(run_session); }
    }
    if sessions.is_empty() { sessions.insert(app.session_for_host(&project.host).await.ok_or_else(|| anyhow::anyhow!("host session unavailable"))?); }
    Ok(sessions.into_iter().collect())
}

pub async fn set(app: &Arc<App>, host: &str, project: &str, sessions: &[String], enabled: bool) -> anyhow::Result<()> {
    let conn = if host == crate::config::LOCAL_HOST { None } else {
        Some(app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host {host}"))?)
    };
    for session in sessions {
        am_base::expert_marker::set(&app.data_dir, conn.as_ref(), project, session, enabled).await?;
    }
    Ok(())
}

/// Remove only this project's markers, including old sessions after bots were moved.
pub async fn remove_project(app: &Arc<App>, host: &str, project: &str) -> anyhow::Result<()> {
    let name = am_base::expert_marker::encode(project);
    if host == crate::config::LOCAL_HOST {
        let root = app.data_dir.join("expert-sessions");
        let entries = match std::fs::read_dir(root) { Ok(v) => v, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), Err(e) => return Err(e.into()) };
        for entry in entries {
            let path = entry?.path().join(&name);
            match std::fs::remove_file(path) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
        }
    } else {
        let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host {host}"))?;
        let script = format!("set -eu\nroot=\"$HOME/.config/agents-manager/expert-sessions\"\nfor file in \"$root\"/*/{name}; do if [ -f \"$file\" ]; then rm -f \"$file\"; fi; done\n");
        conn.ssh_exec(&script).await?;
    }
    Ok(())
}
