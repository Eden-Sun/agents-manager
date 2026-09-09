//! TOML -> SQLite projection (SPEC §3.1).
//!
//! config.toml is the authority for the desired Project / Bot set. Rows removed from
//! the TOML are soft-deleted so their conversation history survives.

use crate::config::{canonical_path, valid_bot_name, valid_id, ConfigStore, ID_RE};
use crate::db;
use anyhow::{bail, Result};
use rand::Rng;
use sqlx::SqlitePool;
use std::collections::HashSet;

pub fn new_token() -> String {
    let mut rng = rand::thread_rng();
    (0..32).map(|_| std::char::from_digit(rng.gen_range(0..16), 16).unwrap()).collect()
}

/// Fill in missing ids (writing them back to the TOML) and upsert everything into SQLite.
pub async fn project_config(store: &ConfigStore, pool: &SqlitePool) -> Result<()> {
    // 1. fill in ids / canonicalize paths, write back only if something changed.
    let changed = store
        .update(|cfg| {
            let mut dirty = false;
            let identities = cfg.identities.clone();
            for i in &identities {
                if !crate::config::valid_identity_name(&i.name) {
                    bail!("invalid identity name `{}` (must match {})", i.name, crate::config::SLUG_NAME_RE);
                }
                if !crate::config::valid_kind(&i.kind) {
                    bail!("invalid identity kind `{}` (must be {})", i.kind, crate::config::kinds_list());
                }
            }
            for p in cfg.projects.iter_mut() {
                if p.id.is_none() {
                    p.id = Some(db::ulid());
                    dirty = true;
                }
                if let Some(id) = p.id.as_deref() {
                    if !valid_id(id) {
                        bail!("invalid project id `{id}` for project `{}` (must match {})", p.label, ID_RE);
                    }
                }
                // Only local paths can be canonicalized here; a remote path was already
                // canonicalized on its host when the project was created (SPEC §11.6).
                if p.host == crate::config::LOCAL_HOST {
                    if let Ok(c) = canonical_path(&p.path) {
                        if c != p.path {
                            p.path = c;
                            dirty = true;
                        }
                    }
                }
                for b in p.bots.iter_mut() {
                    if b.id.is_none() {
                        b.id = Some(db::ulid());
                        dirty = true;
                    }
                    if let Some(id) = b.id.as_deref() {
                        if !valid_id(id) {
                            bail!(
                                "invalid bot id `{id}` for bot `{}` in project `{}` (must match {})",
                                b.name,
                                p.label,
                                ID_RE
                            );
                        }
                    }
                    if !valid_bot_name(&b.name) {
                        bail!("invalid bot name `{}` ({})", b.name, crate::config::BOT_NAME_RE);
                    }
                    if !crate::config::valid_kind(&b.kind) {
                        bail!("invalid bot kind `{}` (must be {})", b.kind, crate::config::kinds_list());
                    }
                    if let Some(idn) = b.identity.as_deref().filter(|s| !s.is_empty()) {
                        // A host's shell `ccN` alias is a legal binding too (API.md §10.2: the
                        // identity list is `[[identities]]` ∪ that host's `ccN`); the config never
                        // owns those, so an unknown name is only an error outside that set.
                        match identities.iter().find(|i| i.name == idn) {
                            None if crate::tools::SHELL_IDENTITY_NAMES.contains(&idn) => {}
                            None => bail!("bot `{}` references unknown identity `{idn}`", b.name),
                            Some(i) if i.kind != b.kind => {
                                bail!("identity `{idn}` is for {} but bot `{}` is {}", i.kind, b.name, b.kind)
                            }
                            Some(_) => {}
                        }
                    }
                }
            }
            Ok(dirty)
        })
        .await?;
    if changed {
        tracing::info!("config.toml: filled in missing ids / canonical paths");
    }

    let cfg = store.get().await;
    let now = db::now();
    let mut live_projects: HashSet<String> = HashSet::new();
    let mut live_bots: HashSet<String> = HashSet::new();

    for p in &cfg.projects {
        let pid = p.id.clone().unwrap();
        live_projects.insert(pid.clone());
        sqlx::query(
            "INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET path=excluded.path, label=excluded.label,
               host=excluded.host, deleted_at=NULL",
        )
        .bind(&pid)
        .bind(&p.path)
        .bind(&p.label)
        .bind(&p.host)
        .bind(&now)
        .execute(pool)
        .await?;

        for b in &p.bots {
            let bid = b.id.clone().unwrap();
            live_bots.insert(bid.clone());
            let args_json = serde_json::to_string(&b.args)?;
            let env_json = serde_json::to_string(&b.env)?;
            let token = new_token();
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, model, effort, fast, persona, args_json, autostart, inject_hooks, auto_approve, identity, env_json, herdr_session, hook_token, created_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id, name=excluded.name, kind=excluded.kind,
                   model=excluded.model, effort=excluded.effort, fast=excluded.fast, persona=excluded.persona, args_json=excluded.args_json, autostart=excluded.autostart, inject_hooks=excluded.inject_hooks,
                   auto_approve=excluded.auto_approve, identity=excluded.identity, env_json=excluded.env_json, herdr_session=excluded.herdr_session, deleted_at=NULL",
            )
            .bind(&bid)
            .bind(&pid)
            .bind(&b.name)
            .bind(&b.kind)
            .bind(b.model.as_deref().filter(|s| !s.trim().is_empty()))
            // v4.0: effort is kind-dependent; a hand-edited value that the kind rejects is dropped.
            .bind(crate::config::normalize_effort(&b.kind, b.effort.as_deref()).unwrap_or(None))
            .bind(b.fast as i64)
            .bind(b.persona.as_deref().filter(|s| !s.trim().is_empty()))
            .bind(&args_json)
            .bind(b.autostart as i64)
            .bind(b.inject_hooks as i64)
            .bind(b.auto_approve as i64)
            .bind(&b.identity)
            .bind(&env_json)
            .bind(&b.herdr_session)
            .bind(&token)
            .bind(&now)
            .execute(pool)
            .await?;
            db::conversation_id(pool, &bid).await?;
        }
    }

    // 2. soft-delete rows no longer in the TOML.
    //
    // SPEC-team §5.3: team members are the one exception. They are daemon-owned runtime
    // objects (`managed_by='team'`) that deliberately never enter config.toml, so the
    // "not in the TOML ⇒ deleted" rule must not touch them.
    for b in db::live_bots(pool).await? {
        // Same for `child`: an agent the bot spawned, adopted by the reconcile.
        if b.managed_by != "user" {
            continue;
        }
        if !live_bots.contains(&b.id) {
            sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(&now).bind(&b.id).execute(pool).await?;
            tracing::info!(bot = %b.name, "bot removed from config.toml; soft-deleted");
        }
    }
    for p in db::live_projects(pool).await? {
        if !live_projects.contains(&p.id) {
            sqlx::query("UPDATE projects SET deleted_at=? WHERE id=?").bind(&now).bind(&p.id).execute(pool).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn projection_error(project_id: &str, bot_id: &str) -> String {
        let dir = std::env::temp_dir().join(format!("am-projection-id-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let text = format!(
            "[server]\nlisten = '127.0.0.1:7788'\n\n[[projects]]\nid = '{project_id}'\npath = '/tmp'\nlabel = 'demo'\nhost = 'remote'\n\n[[projects.bots]]\nid = '{bot_id}'\nname = 'worker'\nkind = 'claude'\n"
        );
        std::fs::write(&path, text).unwrap();
        let store = ConfigStore::load(path).await.unwrap();
        let pool = db::open(&dir.join("db.sqlite3")).await.unwrap();
        let error = project_config(&store, &pool).await.unwrap_err().to_string();
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
        error
    }

    #[tokio::test]
    async fn rejects_invalid_project_and_bot_ids() {
        for id in ["../..", "foo/bar", r"..\..", ""] {
            let error = projection_error(id, "bot-1").await;
            assert!(error.contains("invalid project id"), "{error}");
            assert!(error.contains(id), "{error}");
        }
        for id in ["../..", "foo/bar", r"..\..", ""] {
            let error = projection_error("project-1", id).await;
            assert!(error.contains("invalid bot id"), "{error}");
            assert!(error.contains("worker") && error.contains("demo"), "{error}");
        }
    }
}
