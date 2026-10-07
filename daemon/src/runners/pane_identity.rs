use std::collections::BTreeMap;
use std::sync::Arc;
use futures::future::BoxFuture;
use serde_json::json;
use crate::pane_identity::{
    child_identity, config_dir_var, mark_probed, probe_due, ps_cmd,
};
use crate::state::App;

/// A trait so tests can exercise the reconcile path without a real process.
pub trait ProcEnv: Send + Sync {
    fn env_of<'a>(
        &'a self,
        app: &'a Arc<App>,
        host: &'a str,
        pid: i64,
    ) -> BoxFuture<'a, Option<BTreeMap<String, String>>>;
}

/// Over ssh on a remote host (SPEC §11.2).
pub struct PsProcEnv;

impl ProcEnv for PsProcEnv {
    fn env_of<'a>(
        &'a self,
        app: &'a Arc<App>,
        host: &'a str,
        pid: i64,
    ) -> BoxFuture<'a, Option<BTreeMap<String, String>>> {
        Box::pin(async move {
            let conn = app.hosts.get(host).await?;
            let out = if conn.is_local() {
                let o = crate::local_sh::output(&ps_cmd(pid)).await.ok()?;
                if !o.status.success() {
                    return None;
                }
                String::from_utf8_lossy(&o.stdout).into_owned()
            } else {
                if !conn.is_connected() {
                    return None;
                }
                conn.ssh_exec(&ps_cmd(pid)).await.ok()?
            };
            let env = crate::pane_identity::parse_ps_env(&out);
            (!env.is_empty()).then_some(env)
        })
    }
}

/// Unset (always, in a real daemon) = [`PsProcEnv`]; tests install their own.
#[derive(Default)]
pub struct ProcEnvHook(std::sync::OnceLock<Arc<dyn ProcEnv>>);

impl ProcEnvHook {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set(&self, reader: Arc<dyn ProcEnv>) {
        let _ = self.0.set(reader);
    }

    pub fn reader(&self) -> Arc<dyn ProcEnv> {
        self.0.get().cloned().unwrap_or_else(|| Arc::new(PsProcEnv))
    }
}

/// `pid: None` leaves the row as it is.
pub async fn sync_child_identity(
    app: &Arc<App>,
    host: &str,
    bot: &crate::db::Bot,
    pane_id: &str,
    pid: Option<i64>,
) {
    // Children only: for a user bot this is the user's setting, projected back to `config.toml`.
    if bot.managed_by != "child" {
        return;
    }
    let Some(var) = config_dir_var(&bot.kind) else { return };
    let Some(pid) = pid.filter(|p| *p > 0) else { return };
    if !probe_due(&bot.id, pane_id) {
        return;
    }
    let Some(fence) = app.hosts.fence(host).await else {
        tracing::debug!(host, bot = %bot.name, "host disappeared before child identity detection");
        return;
    };
    let home = match crate::hosts::home_for_fence(&fence).await {
        Ok(home) => home,
        Err(e) => {
            tracing::warn!(host, bot = %bot.name, error = %e, "child identity detection deferred because the host HOME is unreadable");
            return;
        }
    };
    if !app.hosts.is_current(&fence).await {
        return;
    }
    let reader = app.proc_env.reader();
    let Some(env) = reader.env_of(app, host, pid).await else {
        // Not marked probed: a transient failure must not pin the child to the parent's account.
        tracing::debug!(host, bot = %bot.name, pid, "cannot read the child pane's environment");
        return;
    };
    let identities = crate::tools::identities_for_host(app, host).await;
    if !identities.iter().any(|i| i.kind == bot.kind) {
        // Boot reconcile can beat tool detection (§16); an empty list must not become a final answer.
        tracing::debug!(host, bot = %bot.name, "no identities known for this kind yet; re-checking next pass");
        return;
    }
    mark_probed(&bot.id, pane_id);
    let dir = env.get(var).map(String::as_str);
    let Some(name) = child_identity(&identities, &bot.kind, var, &home, dir) else {
        tracing::debug!(host, bot = %bot.name, ?dir, "no identity owns the child's account; keeping the inherited identity");
        return;
    };
    let dir = dir.unwrap_or("");
    if !app.hosts.is_current(&fence).await {
        return;
    }
    // The run remembers the account it was actually read off (`runs.runtime_identity`), even when
    // it matches the inherited value: NULL there means "nobody knows", and the UI says so.
    stamp_run_identity(app.as_ref(), bot, pane_id, &name).await;
    if bot.identity.as_deref() == Some(name.as_str()) {
        return;
    }
    // `managed_by` in WHERE too: a race with the TOML projection must never write a user bot.
    if let Err(e) = sqlx::query("UPDATE bots SET identity = ? WHERE id = ? AND managed_by = 'child'")
        .bind(&name)
        .bind(&bot.id)
        .execute(&app.db)
        .await
    {
        tracing::warn!(bot = %bot.name, error = ?e, "cannot record the account a child agent is running on");
        return;
    }
    tracing::info!(host, bot = %bot.name, pane = %pane_id, was = ?bot.identity, now = %name, %dir,
                   "reconcile: the child is on its own account, not the one it was adopted with");
    app.emit("bot_changed", json!({"bot_id": bot.id})).await;
}

/// Fills only an unrecorded run: a run the daemon started already carries the identity it was
/// launched with (`lifecycle::start`). A kind change clears it (`reconcile::refresh_child_kind`).
async fn stamp_run_identity(app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db), bot: &crate::db::Bot, pane_id: &str, name: &str) {
    let stamped = sqlx::query(
        "UPDATE runs SET runtime_identity = ?
          WHERE bot_id = ? AND pane_id = ? AND state IN ('starting','running','stopping') AND runtime_identity IS NULL",
    )
    .bind(name)
    .bind(&bot.id)
    .bind(pane_id)
    .execute(app.db())
    .await;
    match stamped {
        Ok(r) if r.rows_affected() > 0 => app.emit_bot_status(&bot.id).await,
        Ok(_) => {}
        Err(e) => tracing::warn!(bot = %bot.name, error = ?e, "cannot record the account on the child's run"),
    }
}
