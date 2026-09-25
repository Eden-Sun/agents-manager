//! Credentials for launchd maintenance jobs. Each service gets an owner-only token file and a
//! fixed HTTP scope; these identities are separate from users and bots.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

pub const DAEMON_SWAP: &str = "daemon-swap";
pub const HERDR_UPGRADE: &str = "herdr-upgrade";
pub const IDS: [&str; 2] = [DAEMON_SWAP, HERDR_UPGRADE];

/// Read-only snapshots and maintenance operations each service needs. `/api/state` is never in a
/// service scope: it carries bot `env`/`args`, which may hold secrets; services read the sanitized
/// `/api/supervisor/state` instead. Writes stay on dedicated service routes except restart-lease
/// operations, whose handlers still enforce approval, lease token and fencing.
pub fn allows(id: &str, method: &str, path: &str) -> bool {
    let exact = matches!(
        (id, method, path),
        (DAEMON_SWAP, "GET", "/api/supervisor" | "/api/supervisor/health" | "/api/supervisor/state" | "/api/supervisor/leases" | "/api/supervisor/maintenance/safety")
            | (DAEMON_SWAP, "POST", "/api/supervisor/leases/restart/acquire" | "/api/supervisor/leases/restart/renew" | "/api/supervisor/leases/restart/release")
            | (HERDR_UPGRADE, "GET", "/api/capabilities" | "/api/supervisor/state" | "/api/panes" | "/api/supervisor/health")
            | (HERDR_UPGRADE, "POST", "/api/services/herdr-upgrade/notify")
    );
    exact
        || id == DAEMON_SWAP && method == "POST" && one_id_under(path, "/api/services/daemon-swap/probe/")
        || id == HERDR_UPGRADE && method == "POST" && one_id_under(path, "/api/services/herdr-upgrade/resume/")
}

/// `<prefix><one bot id>` and nothing after it (no extra segments, no query smuggled into the path).
fn one_id_under(path: &str, prefix: &str) -> bool {
    let Some(id) = path.strip_prefix(prefix) else { return false };
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Files that launchd clients may read. Never log or return these token values through an API.
pub fn token_path(data_dir: &Path, id: &str) -> Option<std::path::PathBuf> {
    IDS.contains(&id).then(|| data_dir.join("service-tokens").join(format!("{id}.token")))
}

/// Load stable service tokens for this daemon instance, creating them owner-only on first start.
pub fn load_or_create(data_dir: &Path) -> Result<HashMap<String, String>> {
    let root = data_dir.join("service-tokens");
    crate::private_files::create_private_dir(&root).with_context(|| format!("create {}", root.display()))?;
    let mut tokens = HashMap::new();
    for id in IDS {
        let path = token_path(data_dir, id).expect("IDS entries always have a token path");
        let token = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {
                tighten_token(&path).with_context(|| format!("secure {}", path.display()))?;
                std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?
                    .trim()
                    .to_string()
            }
            Ok(_) => anyhow::bail!("{} is not a regular service token file", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
        };
        let token = if token.is_empty() {
            let token = crate::projection::new_token();
            crate::lifecycle::setup::write_private(&path, token.as_bytes()).with_context(|| format!("write {}", path.display()))?;
            token
        } else {
            token
        };
        tokens.insert(id.to_string(), token);
    }
    Ok(tokens)
}

#[cfg(unix)]
fn tighten_token(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten_token(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn service_tokens_are_stable_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("am-service-token-{}", crate::db::ulid()));
        let tokens = load_or_create(&root).unwrap();
        assert_eq!(tokens.len(), IDS.len());
        for id in IDS {
            let path = token_path(&root, id).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let again = load_or_create(&root).unwrap();
        assert_eq!(again, tokens, "service token must remain stable across daemon restarts");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn service_scopes_allow_only_their_documented_operations() {
        assert!(allows(HERDR_UPGRADE, "POST", "/api/services/herdr-upgrade/resume/01M123"));
        assert!(allows(DAEMON_SWAP, "POST", "/api/services/daemon-swap/probe/01M123"));
        assert!(allows(DAEMON_SWAP, "GET", "/api/supervisor/health"));
        assert!(allows(DAEMON_SWAP, "POST", "/api/supervisor/leases/restart/acquire"));
        // One service's route is not the other's.
        assert!(!allows(DAEMON_SWAP, "POST", "/api/services/herdr-upgrade/resume/01M123"));
        assert!(!allows(HERDR_UPGRADE, "POST", "/api/services/daemon-swap/probe/01M123"));
        // No generic bot control, no extra path segments, no other lease resource.
        assert!(!allows(HERDR_UPGRADE, "POST", "/api/bots/01M123/start"));
        assert!(!allows(DAEMON_SWAP, "POST", "/api/bots/01M123/prompt"));
        assert!(!allows(HERDR_UPGRADE, "POST", "/api/services/herdr-upgrade/resume/01M123/stop"));
        assert!(!allows(HERDR_UPGRADE, "POST", "/api/services/herdr-upgrade/resume/"));
        assert!(!allows(DAEMON_SWAP, "POST", "/api/supervisor/leases/deploy/acquire"));
        assert!(!allows(DAEMON_SWAP, "POST", "/api/supervisor/approvals/a1/decide"));
        // `/api/state` carries bot env/args: neither service reads it.
        assert!(!allows(DAEMON_SWAP, "GET", "/api/state"));
        assert!(!allows(HERDR_UPGRADE, "GET", "/api/state"));
        // Unknown service ids get nothing.
        assert!(!allows("user", "GET", "/api/supervisor/health"));
    }
}
