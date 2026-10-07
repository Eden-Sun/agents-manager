//! 收編來的子 agent 到底跑在哪個帳號上（SPEC §16.6）。
//!
//! Adopt copies the parent's `identity`, but the parent may have split the child onto another
//! account. herdr gives pid but never env, so the account comes from `ps eww -p <pid>`.
//!
//! * **Only `managed_by='child'`.** For user bots `bots.identity` is configuration written back
//!   to `config.toml`; the daemon must not rewrite the user's file from a guess.
//! * **Never clears.** 抄來的值可能是對的，NULL 一定是錯的。

use std::collections::BTreeMap;

/// SPEC §16, §10.5.
pub fn config_dir_var(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        "grok" => Some("GROK_HOME"),
        _ => None,
    }
}

/// Works on macOS and Linux, and only for our own processes — exactly the scope we want.
pub(crate) fn ps_cmd(pid: i64) -> String {
    format!("ps eww -p {pid} 2>/dev/null")
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

/// 不在 `live` 裡的 bot（刪掉、退役的 child）不留探測記錄：key 是 `bot\u{1}pane`，每顆 child 每個 pane 一格，只記不清的話只增不減。
pub fn retain_bots(live: &[String]) {
    probed().lock().unwrap().retain(|k| k.split('\u{1}').next().is_some_and(|bot| live.iter().any(|l| l == bot)));
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

pub(crate) fn mark_probed(bot_id: &str, pane_id: &str) {
    probed().lock().unwrap().insert(probe_key(bot_id, pane_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runners::pane_identity::{ProcEnv, ProcEnvHook, PsProcEnv, sync_child_identity};
    use crate::state::App;
    use futures::future::BoxFuture;
    use std::sync::Arc;
    use crate::config::IdentityCfg;

    struct FixedProcEnv {
        reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        env: BTreeMap<String, String>,
    }

    impl ProcEnv for FixedProcEnv {
        fn env_of<'a>(

            &'a self,
            _app: &'a Arc<App>,
            _host: &'a str,
            _pid: i64,
        ) -> BoxFuture<'a, Option<BTreeMap<String, String>>> {
            Box::pin(async move {
                self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(self.env.clone())
            })
        }
    }

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

    #[tokio::test]
    async fn child_identity_waits_for_remote_home_and_retries_after_recovery() {
        use std::sync::atomic::Ordering;

        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = format!("child-home-616-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        let bot = crate::testing::claude_bot(&app, &env.project_id, "child-home").await;
        sqlx::query("UPDATE bots SET managed_by='child', identity='cc1' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        app.cfg.update(|cfg| {
            cfg.identities = vec![
                idn("cc1", "claude", Some("~/.claude-cc1")),
                idn("cc2", "claude", Some("~/.claude-cc2")),
            ].into_iter().map(|mut identity| { identity.host = Some(host.clone()); identity }).collect();
            Ok(())
        }).await.unwrap();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        app.proc_env.set(std::sync::Arc::new(FixedProcEnv {
            reads: reads.clone(),
            env: [("CLAUDE_CONFIG_DIR".into(), "/home/remote-child/.claude-cc2".into())].into(),
        }));
        crate::hosts::set_ssh_fake(&host, |_| Err(anyhow::anyhow!("injected remote HOME read failure")));

        sync_child_identity(&app, &host, &bot, "w1:p1", Some(901)).await;
        let unchanged = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(unchanged.identity.as_deref(), Some("cc1"), "unreadable HOME leaves the inherited identity alone");
        assert!(probe_due(&bot.id, "w1:p1"), "HOME failure is retryable, not marked as probed");
        assert_eq!(reads.load(Ordering::SeqCst), 0, "do not read or interpret the pane env without its host HOME");

        *conn.remote_home.lock().await = Some("/home/remote-child".into());
        sync_child_identity(&app, &host, &bot, "w1:p1", Some(901)).await;
        let recovered = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(recovered.identity.as_deref(), Some("cc2"), "later pass resolves the child's identity under remote HOME");
        assert!(!probe_due(&bot.id, "w1:p1"));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    /// Linux procps 的 `ps eww -p`（BSD 的 `e` 混 SysV 的 `-p`）也要吐出環境：child 的身分就靠它認
    /// （SPEC「Linux 主機」）。真的起一個行程，外部編譯主機會跑到。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_real_ps_eww_shows_the_environment_of_our_own_process() {
        let mut child = std::process::Command::new("sleep").arg("30").env("CLAUDE_CONFIG_DIR", "/home/u/.claude-cc2").spawn().unwrap();
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(ps_cmd(i64::from(child.id()))).output().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let env = parse_ps_env(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(env.get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("/home/u/.claude-cc2"), "{}", String::from_utf8_lossy(&out.stdout));
    }
}

#[cfg(test)]
mod retain_tests {
    use super::*;

    #[test]
    fn a_deleted_bots_probe_records_are_dropped() {
        for (bot, pane) in [("probe-gone", "w1:p1"), ("probe-gone", "w1:p2"), ("probe-kept", "w1:p3")] {
            probed().lock().unwrap().insert(probe_key(bot, pane));
        }
        retain_bots(&["probe-kept".to_string()]);
        assert!(probe_due("probe-gone", "w1:p1") && probe_due("probe-gone", "w1:p2"), "結束的 bot 不留記錄");
        assert!(!probe_due("probe-kept", "w1:p3"), "還在的 bot 的記錄不動");
        retain_bots(&[]);
    }
}
