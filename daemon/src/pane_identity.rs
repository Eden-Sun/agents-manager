//! 收編來的子 agent 到底跑在哪個帳號上（SPEC §16.6）。
//!
//! A spawned child's pane was opened by its parent, not by us — `herdr pane split --env
//! CLAUDE_CONFIG_DIR=…` can put it on a completely different account — and the adopt in
//! [`crate::reconcile`] has nothing to go on but the parent's row, so it copies the parent's
//! `identity` verbatim. When the parent picked another account for its child, every token that
//! child burns is then billed to the wrong one, in the sidebar and in `/api/quota`.
//!
//! herdr's `pane.process_info` answers with argv, cwd and **pid**, never env, so the account
//! has to come from the operating system: `ps eww -p <pid>` prints a process's environment,
//! and its `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `GROK_HOME` maps back to one of the host's
//! identities (§16.2's `identities_for_host`).
//!
//! The same line the model / effort backfill next door draws, one notch further along:
//!
//! * **fills *and* corrects — but only `managed_by='child'`.** A child's `identity` was never
//!   the user's setting; it is a value our own adopt copied off the parent, so replacing it
//!   with what the pane is really running under fixes a mistake of ours. For every other bot
//!   `bots.identity` *is* configuration — the TOML projection writes it back into
//!   `config.toml` — and reading it off a process would be the daemon rewriting the user's
//!   file from a guess.
//! * **never clears.** Unreadable env, a directory no identity claims, or a CLI that names no
//!   account at all leaves the inherited value standing: 抄來的值可能是對的，NULL 一定是錯的。

use crate::state::App;
use futures::future::BoxFuture;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The environment variable that picks the account, per CLI kind (SPEC §16, §10.5).
pub fn config_dir_var(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        "grok" => Some("GROK_HOME"),
        _ => None,
    }
}

/// The one step that has to touch the machine: "which environment is pid `pid` on `host`
/// running with?". Behind a trait so the reconcile path can be exercised without a real
/// process — `ps` is this daemon's implementation of the question, not the question itself.
pub trait ProcEnv: Send + Sync {
    fn env_of<'a>(
        &'a self,
        app: &'a Arc<App>,
        host: &'a str,
        pid: i64,
    ) -> BoxFuture<'a, Option<BTreeMap<String, String>>>;
}

/// `ps eww -p <pid>`: the command line followed by the whole environment, one line, on both
/// macOS and Linux — and only for our own processes, which is exactly the scope we want.
fn ps_cmd(pid: i64) -> String {
    format!("ps eww -p {pid} 2>/dev/null")
}

/// The real reader: `ps` here, the same `ps` over ssh on a remote host (SPEC §11.2).
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
                let o = tokio::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(ps_cmd(pid))
                    .stdin(std::process::Stdio::null())
                    .output()
                    .await
                    .ok()?;
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

/// Which [`ProcEnv`] the daemon reads through. Unset — the only state a real daemon is ever in
/// — means [`PsProcEnv`]; a test installs its own before the reconcile runs.
#[derive(Default)]
pub struct ProcEnvHook(std::sync::OnceLock<Arc<dyn ProcEnv>>);

impl ProcEnvHook {
    /// Only a test ever calls this — a real daemon leaves the hook empty and reads `ps`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set(&self, reader: Arc<dyn ProcEnv>) {
        let _ = self.0.set(reader);
    }

    fn reader(&self) -> Arc<dyn ProcEnv> {
        self.0.get().cloned().unwrap_or_else(|| Arc::new(PsProcEnv))
    }
}

/// Every `KEY=value` in a `ps eww` dump. The command line is printed *before* the environment
/// and can carry `=` of its own (`--settings=x`), so only tokens whose key is shaped like an
/// environment variable count; a later assignment wins, which is the order `ps` prints in.
///
/// A value containing whitespace is cut at the first space — `ps` gives us no way to tell that
/// apart from the next variable. Such a directory then matches no identity and the child keeps
/// what it inherited, which is the right way for this to fail.
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

/// One directory, one spelling. A trailing slash, or macOS answering `/private/tmp/x` where the
/// config says `/tmp/x`, must not make one directory look like two.
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

/// The identity that owns `dir`, out of the ones a host knows. `home` is *that host's* `$HOME`,
/// because `~` / `$HOME` in an identity's env means the far side's home (§16.2).
///
/// The first match wins, and `identities_for_host` hands them over config-first, so a
/// hand-written `[[identities]]` beats a `ccN` read off the shell exactly as §16.2 says.
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

/// Panes this daemon process already went to the operating system about.
///
/// Bounded on purpose: a reconnect replays a burst of `pane.agent_detected`, each scheduling a
/// reconcile, and one `ps` — one *ssh* on a remote host — per child per pass is the storm the
/// hook refresh next door already had to be taught not to make (2026-09-07). A live process
/// cannot change the account it was started with, so asking once is enough.
fn probed() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Whether [`sync_child_identity`] still has anything to do for this pane — asked *before* the
/// `pane.process_info` call it would need, so a settled child costs no herdr round trip.
pub fn probe_due(bot_id: &str, pane_id: &str) -> bool {
    !probed().lock().unwrap().contains(&probe_key(bot_id, pane_id))
}

fn probe_key(bot_id: &str, pane_id: &str) -> String {
    format!("{bot_id}\u{1}{pane_id}")
}

/// Correct one adopted child's `bots.identity` to the account its own pane is running under.
///
/// `pid` is what `pane.process_info` reported for the pane's CLI process; `None` (a herdr too
/// old to report it, or a pane with no foreground process) leaves the row exactly as it is.
pub async fn sync_child_identity(
    app: &Arc<App>,
    host: &str,
    bot: &crate::db::Bot,
    pane_id: &str,
    pid: Option<i64>,
) {
    // Children only. For a user bot this column is the user's setting and the projection
    // writes it back to `config.toml`; a process is not allowed to have an opinion about it.
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
        // Nothing came back (the process is gone, ssh is down). Not marked as probed: a
        // transient failure must not pin the child to its parent's account for the rest of
        // this daemon's life.
        tracing::debug!(host, bot = %bot.name, pid, "cannot read the child pane's environment");
        return;
    };
    let identities = crate::tools::identities_for_host(app, host).await;
    if !identities.iter().any(|i| i.kind == bot.kind) {
        // §16 reads the `ccN` identities off the host's shell during tool detection, which a
        // reconcile at boot can easily beat. Answering "no identity owns this directory" from
        // an empty list would be wrong *and* final — leave it for the next pass.
        tracing::debug!(host, bot = %bot.name, "no identities known for this kind yet; re-checking next pass");
        return;
    }
    probed().lock().unwrap().insert(probe_key(&bot.id, pane_id));
    // No account variable at all is the *default* account, which several identities may spell
    // (§16.1: `cc0` is simply an identity with an empty env). Nothing unambiguous to write, so
    // the inherited value stays — see the module header.
    let Some(dir) = env.get(var) else {
        tracing::debug!(host, bot = %bot.name, var, "the child's pane names no account; keeping the inherited identity");
        return;
    };
    let home = crate::tools::host_home(app, host).await;
    let Some(name) = identity_named(&identities, &bot.kind, var, &home, dir) else {
        tracing::debug!(host, bot = %bot.name, dir, "no identity owns the child's account directory");
        return;
    };
    if bot.identity.as_deref() == Some(name.as_str()) {
        return;
    }
    // `managed_by` is in the WHERE clause too: this is the one column a race with the TOML
    // projection must never let us write on a user bot.
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
        IdentityCfg { name: name.into(), kind: kind.into(), env, args: vec![] }
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
        // A directory nobody claims, and the right directory under the wrong kind: both are
        // "no answer", which the caller turns into "keep what was inherited".
        assert_eq!(identity_named(&ids, "claude", var, "/Users/m4p", "/Users/m4p/.claude-other"), None);
        assert_eq!(identity_named(&ids, "codex", "CODEX_HOME", "/Users/m4p", "/Users/m4p/.claude-cc2"), None);
    }
}
