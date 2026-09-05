//! TOML -> SQLite projection (SPEC §3.1).
//!
//! config.toml is the authority for the desired Project / Bot set. Rows removed from
//! the TOML are soft-deleted so their conversation history survives.

use crate::config::{canonical_path, valid_bot_name, ConfigStore};
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
            for p in cfg.projects.iter_mut() {
                if p.id.is_none() {
                    p.id = Some(db::ulid());
                    dirty = true;
                }
                if let Ok(c) = canonical_path(&p.path) {
                    if c != p.path {
                        p.path = c;
                        dirty = true;
                    }
                }
                for b in p.bots.iter_mut() {
                    if b.id.is_none() {
                        b.id = Some(db::ulid());
                        dirty = true;
                    }
                    if !valid_bot_name(&b.name) {
                        bail!("invalid bot name `{}` (must match {})", b.name, crate::config::BOT_NAME_RE);
                    }
                    if b.kind != "claude" && b.kind != "codex" {
                        bail!("invalid bot kind `{}`", b.kind);
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
            "INSERT INTO projects (id, path, label, created_at) VALUES (?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET path=excluded.path, label=excluded.label, deleted_at=NULL",
        )
        .bind(&pid)
        .bind(&p.path)
        .bind(&p.label)
        .bind(&now)
        .execute(pool)
        .await?;

        for b in &p.bots {
            let bid = b.id.clone().unwrap();
            live_bots.insert(bid.clone());
            let args_json = serde_json::to_string(&b.args)?;
            let token = new_token();
            sqlx::query(
                "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
                 VALUES (?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(id) DO UPDATE SET project_id=excluded.project_id, name=excluded.name, kind=excluded.kind,
                   args_json=excluded.args_json, autostart=excluded.autostart, inject_hooks=excluded.inject_hooks, deleted_at=NULL",
            )
            .bind(&bid)
            .bind(&pid)
            .bind(&b.name)
            .bind(&b.kind)
            .bind(&args_json)
            .bind(b.autostart as i64)
            .bind(b.inject_hooks as i64)
            .bind(&token)
            .bind(&now)
            .execute(pool)
            .await?;
            db::conversation_id(pool, &bid).await?;
        }
    }

    // 2. soft-delete rows no longer in the TOML.
    for b in db::live_bots(pool).await? {
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
