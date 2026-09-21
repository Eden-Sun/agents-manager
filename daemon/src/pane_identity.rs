//! 收編來的子 agent 到底跑在哪個帳號上（SPEC §16.6）。
//!
//! Adopt copies the parent's `identity`, but the parent may have split the child onto another
//! account. herdr gives pid but never env, so the account comes from `ps eww -p <pid>`.
//!
//! * **Only `managed_by='child'`.** For user bots `bots.identity` is configuration written back
//!   to `config.toml`; the daemon must not rewrite the user's file from a guess.
//! * **Never clears.** 抄來的值可能是對的，NULL 一定是錯的。

use crate::state::App;
use futures::future::BoxFuture;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

/// SPEC §16, §10.5.
pub fn config_dir_var(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        "grok" => Some("GROK_HOME"),
        _ => None,
    }
}

/// A trait so tests can exercise the reconcile path without a real process.
pub trait ProcEnv: Send + Sync {
    fn env_of<'a>(
        &'a self,
        app: &'a Arc<App>,
        host: &'a str,
        pid: i64,
    ) -> BoxFuture<'a, Option<BTreeMap<String, String>>>;
}

/// Works on macOS and Linux, and only for our own processes — exactly the scope we want.
fn ps_cmd(pid: i64) -> String {
    format!("ps eww -p {pid} 2>/dev/null")
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
            let env = parse_ps_env(&out);
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

    pub(crate) fn reader(&self) -> Arc<dyn ProcEnv> {
        self.0.get().cloned().unwrap_or_else(|| Arc::new(PsProcEnv))
    }
}

/// Only env-shaped keys count (argv can hold `--settings=x`). Values with spaces get cut and
/// then match no identity, so the child keeps what it inherited — the right way to fail.
pub fn parse_ps_env(out: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for tok in out.split_whitespace() {
        let Some((k, v)) = tok.split_once('=') else { continue };
        let is_env_key = k.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
            && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if is_env_key {
            env.insert(k.to_string(), v.to_string());
        }
    }
    env
}

/// Trailing slash and macOS `/private/tmp` vs `/tmp` must not make one directory look like two.
pub fn norm_dir(dir: &str) -> String {
    let d = dir.trim();
    let d = if d.starts_with("/private/") { &d["/private".len()..] } else { d };
    let d = d.trim_end_matches('/');
    if d.is_empty() {
        "/".to_string()
    } else {
        d.to_string()
    }
}

/// `home` is that host's `$HOME`. First match wins; input is config-first, so `[[identities]]`
/// beats a shell-read `ccN` (§16.2).
pub fn identity_named(
    identities: &[crate::config::IdentityCfg],
    kind: &str,
    var: &str,
    home: &str,
    dir: &str,
) -> Option<String> {
    let want = norm_dir(dir);
    identities
        .iter()
        .find(|i| {
            i.kind == kind
                && i.env.get(var).is_some_and(|d| norm_dir(&crate::config::expand_home(d, home)) == want)
        })
        .map(|i| i.name.clone())
}

fn default_dir(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" => Some("~/.claude"),
        "codex" => Some("~/.codex"),
        _ => None,
    }
}

/// Unset var or the CLI's default dir (`CLAUDE_CONFIG_DIR=~/.claude`) = the empty-env identity
/// (`cc0`, §16.1); otherwise such a child kept its parent's `cc1` forever.
pub fn child_identity(
    identities: &[crate::config::IdentityCfg],
    kind: &str,
    var: &str,
    home: &str,
    dir: Option<&str>,
) -> Option<String> {
    if let Some(d) = dir {
        if let Some(name) = identity_named(identities, kind, var, home, d) {
            return Some(name);
        }
        let is_default = default_dir(kind)
            .is_some_and(|def| norm_dir(&crate::config::expand_home(def, home)) == norm_dir(d));
        if !is_default {
            return None;
        }
    }
    identities.iter().find(|i| i.kind == kind && !i.env.contains_key(var)).map(|i| i.name.clone())
}

/// Ask once per pane: a reconnect's reconcile burst would otherwise be a `ps`/ssh storm
/// (2026-09-07), and a live process cannot change its account.
fn probed() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Checked before `pane.process_info`, so a settled child costs no herdr round trip.
pub fn probe_due(bot_id: &str, pane_id: &str) -> bool {
    !probed().lock().unwrap().contains(&probe_key(bot_id, pane_id))
}

/// A child's CLI kind can become known after the first reconcile pass.  An identity probe made
/// while the row still carried the parent's kind must not suppress the probe for the child's kind.
pub fn reset_probe(bot_id: &str, pane_id: &str) {
    probed().lock().unwrap().remove(&probe_key(bot_id, pane_id));
}

fn probe_key(bot_id: &str, pane_id: &str) -> String {
    format!("{bot_id}\u{1}{pane_id}")
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
    probed().lock().unwrap().insert(probe_key(&bot.id, pane_id));
    let home = crate::tools::host_home(app, host).await;
    let dir = env.get(var).map(String::as_str);
    let Some(name) = child_identity(&identities, &bot.kind, var, &home, dir) else {
        tracing::debug!(host, bot = %bot.name, ?dir, "no identity owns the child's account; keeping the inherited identity");
        return;
    };
    let dir = dir.unwrap_or("");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IdentityCfg;

    fn idn(name: &str, kind: &str, dir: Option<&str>) -> IdentityCfg {
        let mut env = BTreeMap::new();
        if let Some(d) = dir {
            env.insert(config_dir_var(kind).unwrap().to_string(), d.to_string());
        }
        IdentityCfg { name: name.into(), kind: kind.into(), host: None, env, args: vec![] }
    }

    #[test]
    fn the_environment_is_read_out_of_a_ps_line_and_the_command_line_is_not() {
        let out = "  PID   TT  STAT      TIME COMMAND\n 4924 s026  S+     0:10.72 claude --model opus \
                   --settings=NOPE=1 AM_KIND=claude CLAUDE_CONFIG_DIR=/Users/m4p/.claude-cc2 PATH=/usr/bin\n";
        let env = parse_ps_env(out);
        assert_eq!(env.get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("/Users/m4p/.claude-cc2"));
        assert_eq!(env.get("AM_KIND").map(String::as_str), Some("claude"));
        assert!(!env.contains_key("--settings"), "a flag that happens to hold `=` is not an environment variable");
    }

    #[test]
    fn a_trailing_slash_or_a_private_prefix_is_the_same_directory() {
        assert_eq!(norm_dir("/tmp/x/"), norm_dir("/private/tmp/x"));
        assert_eq!(norm_dir("/Users/m4p/.claude-cc2//"), "/Users/m4p/.claude-cc2");
    }

    #[test]
    fn the_account_directory_names_the_identity_it_belongs_to() {
        let ids = [idn("cc1", "claude", Some("$HOME/.claude-ccompany")), idn("cc2", "claude", Some("~/.claude-cc2"))];
        let var = "CLAUDE_CONFIG_DIR";
        assert_eq!(identity_named(&ids, "claude", var, "/Users/m4p", "/Users/m4p/.claude-cc2/"), Some("cc2".into()));
        assert_eq!(identity_named(&ids, "claude", var, "/Users/m4p", "/Users/m4p/.claude-ccompany"), Some("cc1".into()));
        // Unclaimed dir / wrong kind: no answer, caller keeps the inherited value.
        assert_eq!(identity_named(&ids, "claude", var, "/Users/m4p", "/Users/m4p/.claude-other"), None);
        assert_eq!(identity_named(&ids, "codex", "CODEX_HOME", "/Users/m4p", "/Users/m4p/.claude-cc2"), None);
    }

    /// m4p 2026-09-11: children of a pane exporting `CLAUDE_CONFIG_DIR=~/.claude` showed `cc1`, not `cc0`.
    #[test]
    fn the_default_account_directory_or_no_variable_is_the_empty_env_identity() {
        let var = "CLAUDE_CONFIG_DIR";
        let ids = [idn("cc0", "claude", None), idn("cc1", "claude", Some("$HOME/.claude-ccompany"))];
        let h = "/Users/m4p";
        assert_eq!(child_identity(&ids, "claude", var, h, Some("/Users/m4p/.claude")), Some("cc0".into()));
        assert_eq!(child_identity(&ids, "claude", var, h, Some("/Users/m4p/.claude/")), Some("cc0".into()));
        assert_eq!(child_identity(&ids, "claude", var, h, None), Some("cc0".into()));
        assert_eq!(child_identity(&ids, "claude", var, h, Some("/Users/m4p/.claude-ccompany")), Some("cc1".into()));
        assert_eq!(child_identity(&ids, "claude", var, h, Some("/Users/m4p/.claude-other")), None);
        assert_eq!(child_identity(&ids[1..], "claude", var, h, None), None);
    }
}
