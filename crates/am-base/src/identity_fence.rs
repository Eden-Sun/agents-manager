//! Admission fence between identity deletion and identity-reference writers (#2002).
//!
//! `shell_identity_delete` checks references, edits the shell rc and re-checks. Without this
//! fence a Bot could be assigned the identity between those steps. Deleters take [`exclusive`]
//! for `(host, kind, identity)`; every path that validates an identity and then commits a
//! reference (DB or config.toml) takes [`admit`] before validating and holds it until the write
//! is committed. Lock order is fixed: host lock → identity exclusive.
//!
//! Scope: only in-daemon writers are serialized. Hand edits of config.toml are not protected;
//! they are only caught by the deleter's post-check and later validation failures. The default
//! account (no identity) never enters the fence.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

type Key = (String, String, String);
static FENCES: OnceLock<Mutex<BTreeMap<Key, Arc<RwLock<()>>>>> = OnceLock::new();

fn key(host: &str, kind: &str, identity: &str) -> Key {
    let host = host.trim();
    let host = if host.is_empty() { am_config::LOCAL_HOST } else { host };
    (host.to_string(), kind.to_string(), identity.trim().to_string())
}

fn lock_for(host: &str, kind: &str, identity: &str) -> Arc<RwLock<()>> {
    FENCES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key(host, kind, identity))
        .or_default()
        .clone()
}

/// Referencing side. `None`/blank identity = default account: no fence, returns `None`.
pub async fn admit(host: &str, kind: &str, identity: Option<&str>) -> Option<OwnedRwLockReadGuard<()>> {
    let identity = identity.map(str::trim).filter(|s| !s.is_empty())?;
    Some(lock_for(host, kind, identity).read_owned().await)
}

/// Deleting side: waits for in-flight admitted writers and blocks new ones.
pub async fn exclusive(host: &str, kind: &str, identity: &str) -> OwnedRwLockWriteGuard<()> {
    lock_for(host, kind, identity).write_owned().await
}
