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
pub fn ps_cmd(pid: i64) -> String {
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
pub fn probed() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
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

pub fn probe_key(bot_id: &str, pane_id: &str) -> String {
    format!("{bot_id}\u{1}{pane_id}")
}

pub fn mark_probed(bot_id: &str, pane_id: &str) {
    probed().lock().unwrap().insert(probe_key(bot_id, pane_id));
}
