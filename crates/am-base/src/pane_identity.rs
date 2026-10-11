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

/// A probe in progress suppresses concurrent reconcile passes without making a failed
/// database write permanent.
fn probing() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    static IN_FLIGHT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> =
        std::sync::OnceLock::new();
    IN_FLIGHT.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 不在 `live` 裡的 bot（刪掉、退役的 child）不留探測記錄：key 是 `host\u{1}bot\u{1}pane`。
pub fn retain_bots(live: &[String]) {
    probed().lock().unwrap().retain(|k| k.split('\u{1}').nth(1).is_some_and(|bot| live.iter().any(|l| l == bot)));
    probing().lock().unwrap().retain(|k, _| k.split('\u{1}').nth(1).is_some_and(|bot| live.iter().any(|l| l == bot)));
}

/// Checked before `pane.process_info`, so a settled child costs no herdr round trip.
pub fn probe_due(host: &str, bot_id: &str, pane_id: &str) -> bool {
    let key = probe_key(host, bot_id, pane_id);
    !probed().lock().unwrap().contains(&key) && !probing().lock().unwrap().contains_key(&key)
}

/// Claims a probe atomically so concurrent reconcile passes do not repeat the process
/// environment round trip. A failed attempt releases this claim for the next pass.
pub fn begin_probe(host: &str, bot_id: &str, pane_id: &str) -> Option<u64> {
    let key = probe_key(host, bot_id, pane_id);
    let done = probed().lock().unwrap();
    if done.contains(&key) {
        return None;
    }
    static NEXT_PROBE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut in_flight = probing().lock().unwrap();
    if in_flight.contains_key(&key) {
        return None;
    }
    let token = NEXT_PROBE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    in_flight.insert(key, token);
    Some(token)
}

/// A child's CLI kind can become known after the first reconcile pass.  An identity probe made
/// while the row still carried the parent's kind must not suppress the probe for the child's kind.
pub fn reset_probe(host: &str, bot_id: &str, pane_id: &str) {
    let key = probe_key(host, bot_id, pane_id);
    probed().lock().unwrap().remove(&key);
    probing().lock().unwrap().remove(&key);
}

/// Host replacement/repoint retires every child observation tied to its old authority.
pub fn reset_host_probes(host: &str) {
    let prefix = format!("{host}\u{1}");
    probed().lock().unwrap().retain(|key| !key.starts_with(&prefix));
    probing().lock().unwrap().retain(|key, _| !key.starts_with(&prefix));
}

/// Releases a probe claim after a transient failure or when its future is cancelled.
pub fn release_probe(host: &str, bot_id: &str, pane_id: &str, token: u64) {
    let key = probe_key(host, bot_id, pane_id);
    let mut in_flight = probing().lock().unwrap();
    if in_flight.get(&key) == Some(&token) {
        in_flight.remove(&key);
    }
}

pub fn probe_key(host: &str, bot_id: &str, pane_id: &str) -> String {
    format!("{host}\u{1}{bot_id}\u{1}{pane_id}")
}

pub fn mark_probed(host: &str, bot_id: &str, pane_id: &str, token: u64) -> bool {
    let key = probe_key(host, bot_id, pane_id);
    let mut done = probed().lock().unwrap();
    let mut in_flight = probing().lock().unwrap();
    if in_flight.get(&key) != Some(&token) {
        return false;
    }
    in_flight.remove(&key);
    done.insert(key);
    true
}
