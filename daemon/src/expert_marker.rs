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

pub async fn alert(app: &Arc<App>, host: &str, message: &str) {
    let payload = serde_json::json!({"host":host,"reason":"expert_marker_reconciliation","message":message});
    if let Err(e) = crate::supervisor_inbox::push_inbox(&app.db, &format!("expert-marker:{host}:{}", db::ulid()), "ops_alert", None, None, None, &payload).await {
        tracing::error!(host, error = %e, "could not record expert marker alert");
    }
}

/// Reconcile under the same host lock as activation and start. On uncertainty retain markers.
pub async fn reconcile_host(app: &Arc<App>, host: &str) -> anyhow::Result<()> {
    let gate = db::expert_host_gate(host);
    let _guard = gate.lock().await;
    app.expert_marker_blocked.lock().await.insert(host.to_string());
    let result = reconcile_host_inner(app, host).await;
    match &result {
        Ok(()) => { app.expert_marker_blocked.lock().await.remove(host); }
        Err(e) => { alert(app, host, &format!("專家標記對帳失敗；保留保守狀態：{e:#}")).await; }
    }
    result
}

pub(crate) async fn reconcile_host_inner(app: &Arc<App>, host: &str) -> anyhow::Result<()> {
    use std::collections::{BTreeSet, HashSet};
    let _projection = crate::projection::marker_snapshot_lock().await;
    // Read both the persisted file and the in-memory snapshot: a failed rollback may leave
    // either one ahead of SQLite. Never clear a marker while either still says expert.
    let text = std::fs::read_to_string(&app.cfg.path)?;
    let disk: crate::config::ConfigFile = toml::from_str(&text)?;
    let memory = app.cfg.get().await;
    let projects = db::live_projects(&app.db).await?;
    let mut expert_ids: HashSet<String> = projects.iter().filter(|p| p.mode == "expert").map(|p| p.id.clone()).collect();
    for p in disk.projects.iter().chain(&memory.projects).filter(|p| p.mode == "expert") {
        expert_ids.insert(p.id.clone().ok_or_else(|| anyhow::anyhow!("expert project has no id"))?);
    }
    let mut desired = BTreeSet::new();
    for project in projects.iter().filter(|p| p.host == host && expert_ids.contains(&p.id)) {
        for session in sessions(app, project).await? { desired.insert((project.id.clone(), session)); }
    }
    for project in disk.projects.iter().chain(&memory.projects).filter(|p| p.host == host && p.id.as_ref().is_some_and(|id| expert_ids.contains(id))) {
        let id = project.id.as_ref().expect("filtered id");
        let default_session = app.session_for_host(host).await.ok_or_else(|| anyhow::anyhow!("host session unavailable"))?;
        if project.bots.is_empty() { desired.insert((id.clone(), default_session.clone())); }
        for bot in &project.bots {
            desired.insert((id.clone(), bot.herdr_session.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| default_session.clone())));
        }
    }
    // Complete every write before removing anything, so a partial failure cannot unguard a pane.
    for (project, session) in &desired { set(app, host, project, std::slice::from_ref(session), true).await?; }
    for project in projects.iter().filter(|p| p.host == host) {
        for cfg in [&disk, &memory] {
            if let Some(saved) = cfg.projects.iter().find(|p| p.id.as_deref() == Some(project.id.as_str())) {
                anyhow::ensure!(saved.mode == project.mode && saved.host == project.host, "project {} config and database disagree", project.id);
            }
        }
    }
    let keep: HashSet<(String, String)> = desired.iter().map(|(project, session)| (am_base::expert_marker::encode(session), am_base::expert_marker::encode(project))).collect();
    for (session, project) in list_markers(app, host).await? {
        if !keep.contains(&(session.clone(), project.clone())) { remove_encoded(app, host, &session, &project).await?; }
    }
    Ok(())
}

fn hex_name(s: &str) -> bool { !s.is_empty() && s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) }

async fn list_markers(app: &Arc<App>, host: &str) -> anyhow::Result<Vec<(String,String)>> {
    let mut out = Vec::new();
    if host == crate::config::LOCAL_HOST {
        let root = app.data_dir.join("expert-sessions");
        let entries = match std::fs::read_dir(root) { Ok(v) => v, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out), Err(e) => return Err(e.into()) };
        for entry in entries {
            let entry = entry?;
            let session = entry.file_name().to_string_lossy().into_owned();
            if !hex_name(&session) || !entry.file_type()?.is_dir() { continue; }
            for item in std::fs::read_dir(entry.path())? {
                let item = item?; let project = item.file_name().to_string_lossy().into_owned();
                if hex_name(&project) && item.file_type()?.is_file() { out.push((session.clone(), project)); }
            }
        }
    } else {
        let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host {host}"))?;
        let script = "set -eu\nroot=\"$HOME/.config/agents-manager/expert-sessions\"\nfor dir in \"$root\"/*; do [ -d \"$dir\" ] || continue; for file in \"$dir\"/*; do [ -f \"$file\" ] || continue; printf '%s/%s\\n' \"${dir##*/}\" \"${file##*/}\"; done; done\n";
        for line in conn.ssh_exec(script).await?.lines() {
            if let Some((session, project)) = line.split_once('/') { if hex_name(session) && hex_name(project) { out.push((session.to_string(), project.to_string())); } }
        }
    }
    Ok(out)
}

async fn remove_encoded(app: &Arc<App>, host: &str, session: &str, project: &str) -> anyhow::Result<()> {
    anyhow::ensure!(hex_name(session) && hex_name(project), "invalid marker filename");
    if host == crate::config::LOCAL_HOST {
        let file = app.data_dir.join("expert-sessions").join(session).join(project);
        match std::fs::remove_file(file) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
    } else {
        let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host {host}"))?;
        conn.ssh_exec(&format!("set -eu\nrm -f \"$HOME/.config/agents-manager/expert-sessions/{session}/{project}\"\n")).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restart_reconciliation_removes_only_obsolete_project_markers() {
        let e = crate::testing::env().await;
        let other = db::ulid();
        sqlx::query("INSERT INTO projects(id,path,label,host,mode,created_at) VALUES(?,?,'expert','local','expert',?)")
            .bind(&other).bind(format!("/tmp/{other}")).bind(db::now()).execute(&e.app.db).await.unwrap();
        let sessions = vec!["test".to_string()];
        set(&e.app, "local", &e.project_id, &sessions, true).await.unwrap();
        set(&e.app, "local", &other, &sessions, true).await.unwrap();
        reconcile_host(&e.app, "local").await.unwrap();
        let root = e.app.data_dir.join("expert-sessions").join(am_base::expert_marker::encode("test"));
        assert!(!root.join(am_base::expert_marker::encode(&e.project_id)).exists());
        assert!(root.join(am_base::expert_marker::encode(&other)).exists());
        // A missing expert marker is repaired when a host reconnects.
        std::fs::remove_file(root.join(am_base::expert_marker::encode(&other))).unwrap();
        reconcile_host(&e.app, "local").await.unwrap();
        assert!(root.join(am_base::expert_marker::encode(&other)).exists());
    }

    #[tokio::test]
    async fn remote_reconnect_repairs_expert_marker_and_fails_closed_on_ssh_error() {
        let e = crate::testing::env().await;
        let host = format!("host-{}", db::ulid());
        let cfg: crate::config::HostCfg = serde_json::from_value(serde_json::json!({"name":host,"ssh":"test","herdr_session":"remote-session"})).unwrap();
        e.app.hosts.insert_remote_for_test(cfg).await;
        sqlx::query("UPDATE projects SET host=?,mode='expert' WHERE id=?").bind(&host).bind(&e.project_id).execute(&e.app.db).await.unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new())); let seen2 = seen.clone();
        crate::hosts::set_ssh_fake(&host, move |script| { seen2.lock().unwrap().push(script.to_string()); Ok(String::new()) });
        reconcile_host(&e.app, &host).await.unwrap();
        assert!(seen.lock().unwrap().iter().any(|s| s.contains(&am_base::expert_marker::encode("remote-session")) && s.contains(&am_base::expert_marker::encode(&e.project_id))));
        crate::hosts::set_ssh_fake(&host, |_| anyhow::bail!("ssh unavailable"));
        assert!(reconcile_host(&e.app, &host).await.is_err());
        assert!(e.app.expert_marker_blocked.lock().await.contains(&host));
    }

    #[tokio::test]
    async fn failed_marker_repair_blocks_expert_relay_until_recovered() {
        let e = crate::testing::env().await;
        let a = crate::testing::claude_bot(&e.app, &e.project_id, "marker-repair-a").await;
        let b = crate::testing::claude_bot(&e.app, &e.project_id, "marker-repair-b").await;
        sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&e.project_id).execute(&e.app.db).await.unwrap();
        let root = e.app.data_dir.join("expert-sessions");
        std::fs::write(&root, "unwritable").unwrap();
        assert!(reconcile_host(&e.app, "local").await.is_err());
        assert!(matches!(crate::relay_policy::relay_prompt_allowed(&e.app, &a.id, &b.id, crate::relay_policy::Admission::Deliver).await, Err(crate::relay_policy::RelayDenied::Unavailable(_))));
        std::fs::remove_file(root).unwrap();
        reconcile_host(&e.app, "local").await.unwrap();
        assert!(!e.app.expert_marker_blocked.lock().await.contains("local"));
    }
}

/// Parent-child hosts can continue normal reconciliation while a stale-marker cleanup retries.
/// Any unreadable source is treated as possibly expert.
pub async fn host_may_be_expert(app: &Arc<App>, host: &str) -> bool {
    let projects = match db::live_projects(&app.db).await { Ok(p) => p, Err(_) => return true };
    if projects.iter().any(|p| p.host == host && p.mode == "expert") { return true; }
    let disk: crate::config::ConfigFile = match std::fs::read_to_string(&app.cfg.path).ok().and_then(|text| toml::from_str(&text).ok()) { Some(v) => v, None => return true };
    let memory = app.cfg.get().await;
    disk.projects.iter().chain(&memory.projects).any(|p| p.host == host && p.mode == "expert")
}
