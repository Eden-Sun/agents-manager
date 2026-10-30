//! Per-session markers for inherited shell panes with old environment variables.
use std::{path::Path, sync::Arc};
use crate::hosts::HostConn;

/// All daemon instances on a host share this directory; tests use their isolated data root.
pub fn local_root(data_dir: &Path) -> std::path::PathBuf {
    if cfg!(any(test, feature = "test-hooks")) { return data_dir.join("expert-sessions"); }
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config/agents-manager/expert-sessions"))
        .unwrap_or_else(|| data_dir.join("expert-sessions"))
}

pub fn encode(s: &str) -> String { s.as_bytes().iter().map(|b| format!("{b:02x}")).collect() }

pub fn marker_name(instance: Option<&str>, project: &str) -> String { format!("i{}-p{}", encode(instance.unwrap_or("default")), encode(project)) }

pub async fn set(data_dir: &Path, remote: Option<&Arc<HostConn>>, instance: Option<&str>, project: &str, session: &str, enabled: bool) -> anyhow::Result<()> {
    let project_hex = marker_name(instance, project);
    let session_hex = encode(session);
    if let Some(conn) = remote {
        let dir = format!("$HOME/.config/agents-manager/expert-sessions/{session_hex}");
        let quoted = crate::hosts::sh_quote(project);
        let script = if enabled {
            format!("set -eu\numask 077\nmkdir -p \"{dir}\"\nprintf '%s' {quoted} > \"{dir}/{project_hex}\"\ntest \"$(cat \"{dir}/{project_hex}\")\" = {quoted}\n")
        } else { format!("set -eu\nrm -f \"{dir}/{project_hex}\"\n") };
        conn.ssh_exec(&script).await?;
    } else {
        let dir = local_root(data_dir).join(&session_hex);
        let file = dir.join(&project_hex);
        if enabled {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&file, project.as_bytes())?;
            anyhow::ensure!(std::fs::read(&file)? == project.as_bytes(), "marker readback mismatch");
        } else {
            match std::fs::remove_file(file) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
        }
    }
    Ok(())
}
