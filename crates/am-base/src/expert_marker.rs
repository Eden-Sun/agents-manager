//! Per-session markers for inherited shell panes with old environment variables.
use std::{path::Path, sync::Arc};
use crate::hosts::HostConn;

pub fn encode(s: &str) -> String { s.as_bytes().iter().map(|b| format!("{b:02x}")).collect() }

pub async fn set(data_dir: &Path, remote: Option<&Arc<HostConn>>, project: &str, session: &str, enabled: bool) -> anyhow::Result<()> {
    let project_hex = encode(project);
    let session_hex = encode(session);
    if let Some(conn) = remote {
        let dir = format!("$HOME/.config/agents-manager/expert-sessions/{session_hex}");
        let quoted = crate::hosts::sh_quote(project);
        let script = if enabled {
            format!("set -eu\numask 077\nmkdir -p \"{dir}\"\nprintf '%s' {quoted} > \"{dir}/{project_hex}\"\ntest \"$(cat \"{dir}/{project_hex}\")\" = {quoted}\n")
        } else { format!("set -eu\nrm -f \"{dir}/{project_hex}\"\n") };
        conn.ssh_exec(&script).await?;
    } else {
        let dir = data_dir.join("expert-sessions").join(&session_hex);
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
