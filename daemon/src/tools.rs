//! v4.0 — per-host CLI detection (`hosts[].tools`) and "install / log in through an existing
//! agent" (`POST /api/hosts/:name/tools/install`).
//!
//! Detection runs one POSIX `sh` script on the host (locally through `/bin/sh`, remotely
//! through `HostConn::ssh_exec_path`). Executables are looked up the way a pane sees them —
//! through the user's *login* shell — so a tool installed by e.g. Homebrew or `~/.local/bin`
//! is found even though the daemon / a non-interactive ssh shell has a bare PATH.
//!
//! A second pass answers the *per-identity* question (`hosts[].identities`): an
//! `[[identities]]` entry is global config, but whether the account it points at is usable is
//! a property of each host — `CLAUDE_CONFIG_DIR = "$HOME/.claude-ccompany"` resolves on every
//! machine and is logged in on only some of them. That pass runs the CLI's own
//! "am I logged in" question **under the identity's env** (`$HOME` expanded against *that*
//! host's home), because the credential may live in the macOS Keychain or the environment
//! rather than in a file next to the config dir.
//!
//! The same pass also *discovers* identities (SPEC §16): people who run several Claude
//! accounts already keep them as shell aliases —
//!
//! ```sh
//! alias cc1='CLAUDE_CONFIG_DIR=$HOME/.claude-cc1 claude --dangerously-skip-permissions'
//! ```
//!
//! — so `cc0`…`cc6` are read off the host's own login shell (`$SHELL -lic alias`, falling back
//! to `~/.zshrc`) and offered as identities without anyone writing `[[identities]]` by hand.
//! They are **per host**: `cc1` is `~/.claude-cc1` here and `~/.claude-ccompany` on m4p, and
//! that is exactly right — the alias is the account, and the account lives on the machine.
//! Only `CLAUDE_CONFIG_DIR` is taken from the alias; the flags after `claude` are left alone
//! (the daemon decides those, e.g. `auto_approve`).

use crate::config::{valid_kind, LOCAL_HOST};
use crate::hosts::sh_quote;
use crate::state::App;
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolInfo {
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    /// `None` = could not tell.
    pub logged_in: Option<bool>,
}

impl Default for ToolInfo {
    fn default() -> Self {
        Self { installed: false, path: None, version: None, logged_in: None }
    }
}

/// One identity as seen *from one host*. Same shape as `ToolInfo`'s login half, so the UI can
/// read both the same way.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IdentityInfo {
    pub name: String,
    pub kind: String,
    /// `None` = could not tell (CLI missing, probe failed, output not understood).
    pub logged_in: Option<bool>,
    /// Who is logged in, when the CLI says so (claude: e-mail, codex: `ChatGPT`, grok: `grok.com`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// claude's `subscriptionType` (`max`, `team`, …) when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// `config` = an `[[identities]]` entry; `shell` = discovered from a `ccN` alias on this
    /// host. The UI needs the difference: only a config one can be edited or deleted.
    pub source: &'static str,
    /// The config dir this identity points at *on this host*, when it has one (`cc0` and other
    /// default-account identities have none). Display only — the real env goes through
    /// [`identities_for_host`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

pub const SOURCE_CONFIG: &str = "config";
pub const SOURCE_SHELL: &str = "shell";

impl IdentityInfo {
    /// A discovered identity whose login state is not known yet (the next detection fills it in).
    pub fn shell(name: &str, kind: &str, config_dir: Option<String>) -> Self {
        Self::unknown(name, kind, SOURCE_SHELL, config_dir)
    }

    fn unknown(name: &str, kind: &str, source: &'static str, config_dir: Option<String>) -> Self {
        Self {
            name: name.to_string(),
            kind: kind.to_string(),
            logged_in: None,
            account: None,
            plan: None,
            source,
            config_dir,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HostTools {
    pub tools: BTreeMap<String, ToolInfo>,
    /// Login state on this host for every identity it knows — config ones and the `ccN`
    /// aliases discovered here — keyed by identity name.
    pub identities: BTreeMap<String, IdentityInfo>,
    /// The `ccN` aliases found on this host, in `cc0`…`cc6` order. Not config: they are
    /// re-read on every detection and never written back to `config.toml`.
    pub shell_identities: Vec<crate::config::IdentityCfg>,
    pub checked_at: String,
}

/// The `ccN` alias half of the probe, on its own so the poller can re-run just this part
/// every minute without a CLI round trip (SPEC §16.5).
macro_rules! alias_sh {
    () => {
        r#"
al=$( "${SHELL:-/bin/sh}" -lic 'alias' 2>/dev/null )
[ -n "$al" ] || al=$(cat "$HOME/.zshrc" 2>/dev/null)
printf '%s\n' "$al" | grep -E "(^|[[:space:]])(alias[[:space:]]+)?cc[0-6]=" | while IFS= read -r line; do
  printf 'AM_ALIAS %s\n' "$line"
done
"#
    };
}
pub const ALIAS_SH: &str = alias_sh!();

/// The probe. Every line is `AM_<WHAT> <kind> <value>`; a missing value means unknown.
/// claude's login on macOS lives in the Keychain: `security find-generic-password` without
/// `-w` needs no unlock, but a non-interactive session can still be refused — that case is
/// reported as unknown rather than "not logged in".
pub const PROBE_SH: &str = concat!(r#"
for k in claude codex grok; do
  p=$( "${SHELL:-/bin/sh}" -lic "command -v $k" 2>/dev/null | tail -1 )
  [ -n "$p" ] || p=$(command -v "$k" 2>/dev/null)
  case "$p" in /*) ;; *) p="" ;; esac
  printf 'AM_PATH %s %s\n' "$k" "$p"
  if [ -n "$p" ]; then
    v=$( "$p" --version 2>/dev/null </dev/null | head -1 | tr -d '\r' )
    printf 'AM_VER %s %s\n' "$k" "$v"
  fi
done
CD="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
if [ -f "$CD/.credentials.json" ]; then
  printf 'AM_LOGIN claude 1\n'
elif command -v security >/dev/null 2>&1; then
  out=$(security find-generic-password -s "Claude Code-credentials" 2>&1); rc=$?
  if [ $rc -eq 0 ]; then printf 'AM_LOGIN claude 1\n'
  elif printf '%s' "$out" | grep -qi "could not be found"; then printf 'AM_LOGIN claude 0\n'
  else printf 'AM_LOGIN claude ?\n'; fi
else
  printf 'AM_LOGIN claude 0\n'
fi
if [ -f "${CODEX_HOME:-$HOME/.codex}/auth.json" ]; then printf 'AM_LOGIN codex 1\n'; else printf 'AM_LOGIN codex 0\n'; fi
GH="${GROK_HOME:-$HOME/.grok}"
if [ -f "$GH/auth.json" ] || ls "$GH"/auth* >/dev/null 2>&1; then printf 'AM_LOGIN grok 1\n'; else printf 'AM_LOGIN grok 0\n'; fi
"#, alias_sh!());

pub fn parse_probe(out: &str) -> BTreeMap<String, ToolInfo> {
    let mut m: BTreeMap<String, ToolInfo> = crate::config::KINDS.iter().map(|k| (k.to_string(), ToolInfo::default())).collect();
    for line in out.lines() {
        let mut it = line.trim_end().splitn(3, ' ');
        let (Some(tag), Some(kind)) = (it.next(), it.next()) else { continue };
        let val = it.next().unwrap_or("").trim();
        let Some(t) = m.get_mut(kind) else { continue };
        match tag {
            "AM_PATH" => {
                if !val.is_empty() {
                    t.installed = true;
                    t.path = Some(val.to_string());
                }
            }
            "AM_VER" => {
                if !val.is_empty() {
                    t.version = Some(val.to_string());
                }
            }
            "AM_LOGIN" => {
                t.logged_in = match val {
                    "1" => Some(true),
                    "0" => Some(false),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    // A tool that is not installed cannot be "logged in"; keep the file-based answer only
    // when it is positive (credentials may survive an uninstall).
    for t in m.values_mut() {
        if !t.installed && t.logged_in == Some(false) {
            t.logged_in = None;
        }
    }
    m
}

// ------------------------------------------------------------------ ccN aliases

/// The alias names looked for, in the order they are offered.
pub const SHELL_IDENTITY_NAMES: [&str; 7] = ["cc0", "cc1", "cc2", "cc3", "cc4", "cc5", "cc6"];

/// Strip one layer of matching quotes.
fn unquote(s: &str) -> &str {
    let t = s.trim();
    for q in ['\'', '"'] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return &t[1..t.len() - 1];
        }
    }
    t
}

/// `CLAUDE_CONFIG_DIR=$HOME/.claude-cc1 claude …` → `$HOME/.claude-cc1`.
///
/// Only a leading assignment counts (that is the only place a shell would honour it), and the
/// value ends at the first unquoted space — the flags after `claude` are not ours to take.
fn config_dir_of(cmd: &str) -> Option<String> {
    const KEY: &str = "CLAUDE_CONFIG_DIR=";
    let at = cmd.find(KEY)?;
    // Anything before it must be other `VAR=value` assignments, never a word like `env` or a
    // second command — an alias whose config dir is set *after* the binary is not one we
    // understand, so it is skipped rather than guessed at.
    if cmd[..at].split_whitespace().any(|w| !w.contains('=')) {
        return None;
    }
    let rest = &cmd[at + KEY.len()..];
    let val = match rest.chars().next() {
        Some(q @ ('\'' | '"')) => rest[1..].split(q).next().unwrap_or(""),
        _ => rest.split_whitespace().next().unwrap_or(""),
    };
    let val = val.trim();
    (!val.is_empty()).then(|| val.to_string())
}

/// `AM_ALIAS` lines → identities, in [`SHELL_IDENTITY_NAMES`] order.
///
/// A `ccN` alias only counts when it actually runs `claude`; `cc0`-style aliases with no
/// `CLAUDE_CONFIG_DIR` become an identity with an **empty env** — the default account, which
/// is what the strip already folds onto the bare `claude` quota key.
pub fn parse_shell_identities(out: &str) -> Vec<crate::config::IdentityCfg> {
    let mut found: BTreeMap<String, crate::config::IdentityCfg> = BTreeMap::new();
    for line in out.lines() {
        let Some(rest) = line.trim().strip_prefix("AM_ALIAS ") else { continue };
        let rest = rest.trim().strip_prefix("alias ").unwrap_or(rest.trim());
        let Some((name, body)) = rest.split_once('=') else { continue };
        let name = name.trim();
        if !SHELL_IDENTITY_NAMES.contains(&name) {
            continue;
        }
        let cmd = unquote(body).trim().to_string();
        if !cmd.split_whitespace().any(|w| w == "claude" || w.ends_with("/claude")) {
            continue;
        }
        let mut env = BTreeMap::new();
        if let Some(dir) = config_dir_of(&cmd) {
            env.insert("CLAUDE_CONFIG_DIR".to_string(), dir);
        }
        // Later definitions win, the way the shell itself resolves a redefined alias.
        found.insert(
            name.to_string(),
            crate::config::IdentityCfg { name: name.to_string(), kind: "claude".into(), env, args: vec![] },
        );
    }
    SHELL_IDENTITY_NAMES.iter().filter_map(|n| found.remove(*n)).collect()
}

/// Every identity usable on `host`: the global `[[identities]]` first, then this host's `ccN`
/// aliases that do not collide with one (hand-written config always wins).
pub async fn identities_for_host(app: &Arc<App>, host: &str) -> Vec<crate::config::IdentityCfg> {
    let mut out = app.cfg.get().await.identities.clone();
    if let Some(ht) = app.tools.lock().await.get(host) {
        for i in &ht.shell_identities {
            if !out.iter().any(|x| x.name == i.name) {
                out.push(i.clone());
            }
        }
    }
    out
}

/// One identity by name on `host` — [`identities_for_host`] plus a lookup.
pub async fn identity_for_host(app: &Arc<App>, host: &str, name: &str) -> Option<crate::config::IdentityCfg> {
    identities_for_host(app, host).await.into_iter().find(|i| i.name == name)
}

// ------------------------------------------------------------------ per-identity login

/// The CLI's own "am I logged in" question, per kind. Deliberately *not* a file check: a
/// claude account can live in the macOS Keychain with nothing in `$CLAUDE_CONFIG_DIR`, so
/// `.credentials.json` being absent proves nothing. Every one of these is read-only and
/// answers for whatever env it is given.
///
/// * claude — **not here**: `auth status --json` over a non-login ssh session cannot read the
///   macOS Keychain, so it answers `loggedIn: false` for accounts that work fine. It runs in a
///   herdr pane instead, alongside `/usage` ([`crate::quota_claude`]), and the answer lands
///   back here through [`record_identity_login`].
/// * codex  — `login status` (`Logged in using …` / `Not logged in`)
/// * grok   — `models` (`You are logged in with …` / `You are not authenticated.`); grok has
///   no `status` subcommand, and `models` is already how `models.rs` talks to it.
pub fn login_status_args(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "codex" => Some(&["login", "status"]),
        "grok" => Some(&["models"]),
        _ => None,
    }
}

/// The claude arguments the pane probe uses. Kept next to [`login_status_args`] so the two
/// stay in step even though claude deliberately no longer goes through the ssh pass.
pub const CLAUDE_LOGIN_ARGS: &[&str] = &["auth", "status", "--json"];

/// Write one identity's login answer into the cached `HostTools`, creating the row if the
/// tools pass has not seen this identity yet. Returns whether anything actually changed, so
/// the caller only pushes `host_changed` when the UI would see something new.
pub async fn record_identity_login(
    app: &Arc<App>,
    host: &str,
    name: &str,
    logged_in: Option<bool>,
    account: Option<String>,
    plan: Option<String>,
) -> bool {
    let mut all = app.tools.lock().await;
    let Some(ht) = all.get_mut(host) else { return false };
    let Some(info) = ht.identities.get_mut(name) else { return false };
    // A pane answer of "could not tell" must not erase what we already knew.
    if logged_in.is_none() && account.is_none() && plan.is_none() {
        return false;
    }
    let before = (info.logged_in, info.account.clone(), info.plan.clone());
    if logged_in.is_some() {
        info.logged_in = logged_in;
    }
    if account.is_some() {
        info.account = account;
    }
    if plan.is_some() {
        info.plan = plan;
    }
    before != (info.logged_in, info.account.clone(), info.plan.clone())
}

/// Ask the CLI *right now* whether `name` is logged in on `host`, and update the cache.
///
/// The tools pass and the quota poller keep the login state, but the poller parks a
/// logged-out identity for 30 minutes — so after the user logs in (via the popover's shell
/// button, or on their own) the cache said "not logged in" until 「重新偵測」. `start_bot`
/// calls this when the cache says logged-out, before it warns.
///
/// Locally this is `claude auth status --json` with the identity's env. Over ssh the answer
/// can be a false negative (no Keychain), so a remote `false` is **not** written back — only
/// a positive answer updates the cache there. Returns the fresh answer when there is one.
pub async fn recheck_identity_login(app: &Arc<App>, host: &str, name: &str) -> Option<bool> {
    let idn = identity_for_host(app, host, name).await?;
    let args: Vec<&str> = match idn.kind.as_str() {
        "claude" => CLAUDE_LOGIN_ARGS.to_vec(),
        k => login_status_args(k)?.to_vec(),
    };
    let bin = cached_path(app, host, &idn.kind).await.unwrap_or_else(|| idn.kind.clone());
    let home = host_home(app, host).await;
    let mut script = String::new();
    for (k, v) in &idn.env {
        if valid_env_name(k) {
            script.push_str(&format!("export {k}={}\n", sh_quote(&crate::config::expand_home(v, &home))));
        }
    }
    script.push_str(&format!("{} {} 2>/dev/null </dev/null", sh_quote(&bin), args.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ")));
    let out = if host == LOCAL_HOST {
        run_local(&script, Duration::from_secs(20)).await.ok()?
    } else {
        app.hosts.get(host).await?.ssh_exec_path(&script).await.ok()?
    };
    let (logged_in, account, plan) = read_login_answer(&idn.kind, &out);
    let logged_in = logged_in?;
    if host != LOCAL_HOST && !logged_in {
        return None;
    }
    if record_identity_login(app, host, name, Some(logged_in), account, plan).await {
        app.emit("host_changed", serde_json::json!({"host": host})).await;
    }
    if logged_in && idn.kind == "claude" {
        crate::quota_claude::unpark_identity(host, name);
    }
    Some(logged_in)
}

/// A headless `claude auth login` (what the popover's 「開 shell 登入」 runs) writes the
/// credentials but never marks onboarding done, so the next *interactive* `claude` in that
/// config dir opens on 「Select login method」 even though `auth status` says logged in
/// (observed on cc2, 2026-09-08). Set `hasCompletedOnboarding` when the account is there and
/// the flag is not. Local host only; returns whether the file was changed.
pub fn ensure_claude_onboarded(config_dir: &std::path::Path) -> bool {
    let path = config_dir.join(".claude.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    let Some(obj) = v.as_object_mut() else { return false };
    if obj.get("hasCompletedOnboarding").and_then(|x| x.as_bool()) == Some(true) {
        return false;
    }
    if obj.get("oauthAccount").map(|a| a.is_object()).unwrap_or(false) == false {
        return false;
    }
    obj.insert("hasCompletedOnboarding".into(), serde_json::Value::Bool(true));
    let Ok(out) = serde_json::to_string_pretty(&v) else { return false };
    let tmp = path.with_extension("json.am-tmp");
    if std::fs::write(&tmp, out).is_err() {
        return false;
    }
    std::fs::rename(&tmp, &path).is_ok()
}

/// One identity to ask about, already resolved for a specific host.
#[derive(Debug, Clone)]
pub struct IdentityProbe {
    pub name: String,
    pub kind: String,
    /// Absolute path from the tools pass (never a bare name: the probe runs in a bare PATH).
    pub bin: String,
    /// The identity's env with `$HOME` expanded against *this* host's home.
    pub env: BTreeMap<String, String>,
}

/// A shell-assignable variable name. Config is user-written, and these values are `export`ed
/// into a script, so anything else is dropped rather than quoted around.
pub(crate) fn valid_env_name(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The identity pass: each CLI call is fenced by `AM_IDENT_BEGIN`/`AM_IDENT_END <name>` so the
/// output can be handed to the per-kind reader verbatim. Each runs in its own subshell, so one
/// identity's env never leaks into the next, and with stdin closed so nothing waits for a TTY.
pub fn identity_probe_sh(items: &[IdentityProbe]) -> String {
    let mut s = String::new();
    for it in items {
        let Some(args) = login_status_args(&it.kind) else { continue };
        s.push_str(&format!("printf 'AM_IDENT_BEGIN %s\\n' {}\n", sh_quote(&it.name)));
        s.push('(');
        for (k, v) in it.env.iter().filter(|(k, _)| valid_env_name(k)) {
            s.push_str(&format!(" {k}={}; export {k};", sh_quote(v)));
        }
        s.push_str(&format!(" exec {}", sh_quote(&it.bin)));
        for a in args {
            s.push(' ');
            s.push_str(&sh_quote(a));
        }
        s.push_str(" ) </dev/null 2>/dev/null\n");
        // The leading newline keeps the fence on its own line when the CLI ends without one.
        s.push_str(&format!("printf '\\nAM_IDENT_END %s\\n' {}\n", sh_quote(&it.name)));
    }
    s
}

/// Read one CLI's answer → `(logged_in, account, plan)`. Anything unrecognised stays `None`
/// ("could not tell"), never `Some(false)`.
pub(crate) fn read_login_answer(kind: &str, body: &str) -> (Option<bool>, Option<String>, Option<String>) {
    let text = body.trim();
    if text.is_empty() {
        return (None, None, None);
    }
    match kind {
        "claude" => {
            let (Some(i), Some(j)) = (text.find('{'), text.rfind('}')) else { return (None, None, None) };
            if j < i {
                return (None, None, None);
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[i..=j]) else { return (None, None, None) };
            let li = v.get("loggedIn").or_else(|| v.get("logged_in")).and_then(|x| x.as_bool());
            let account = v.get("email").and_then(|x| x.as_str()).map(str::to_string);
            let plan = v.get("subscriptionType").and_then(|x| x.as_str()).map(str::to_string);
            (li, account, plan)
        }
        "codex" => {
            let low = text.to_ascii_lowercase();
            if low.contains("not logged in") {
                return (Some(false), None, None);
            }
            if let Some(line) = text.lines().find(|l| l.to_ascii_lowercase().contains("logged in")) {
                let account = line.split_once(" using ").map(|(_, r)| r.trim().trim_end_matches('.').to_string());
                return (Some(true), account.filter(|s| !s.is_empty()), None);
            }
            (None, None, None)
        }
        "grok" => {
            let low = text.to_ascii_lowercase();
            if low.contains("not authenticated") || low.contains("not logged in") {
                return (Some(false), None, None);
            }
            if let Some(line) = text.lines().find(|l| l.to_ascii_lowercase().contains("logged in")) {
                let account = line.split_once(" with ").map(|(_, r)| r.trim().trim_end_matches('.').to_string());
                return (Some(true), account.filter(|s| !s.is_empty()), None);
            }
            (None, None, None)
        }
        _ => (None, None, None),
    }
}

/// Split the fenced identity output and read each block. `kinds` maps identity name → kind;
/// blocks for names that are not in it are ignored.
pub fn parse_identity_probe(out: &str, kinds: &BTreeMap<String, String>) -> BTreeMap<String, IdentityInfo> {
    let mut m = BTreeMap::new();
    let mut cur: Option<(String, String)> = None;
    for line in out.lines() {
        let l = line.trim_end();
        if let Some(name) = l.strip_prefix("AM_IDENT_BEGIN ") {
            cur = Some((name.trim().to_string(), String::new()));
            continue;
        }
        if let Some(name) = l.strip_prefix("AM_IDENT_END ") {
            let Some((open, body)) = cur.take() else { continue };
            if open != name.trim() {
                continue;
            }
            let Some(kind) = kinds.get(&open) else { continue };
            let (logged_in, account, plan) = read_login_answer(kind, &body);
            // `source` / `config_dir` belong to the identity, not to its answer — the caller
            // fills them in from the entry it asked about.
            m.insert(
                open.clone(),
                IdentityInfo {
                    name: open,
                    kind: kind.clone(),
                    logged_in,
                    account,
                    plan,
                    source: SOURCE_CONFIG,
                    config_dir: None,
                },
            );
            continue;
        }
        if let Some((_, body)) = cur.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    m
}

/// `$HOME` on `host` (the daemon's own home for `local`), for expanding identity env values.
pub(crate) async fn host_home(app: &Arc<App>, host: &str) -> String {
    if let Some(conn) = app.hosts.get(host).await {
        if let Ok(h) = conn.home().await {
            return h;
        }
    }
    dirs::home_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
}

/// Ask every identity known on this host — config ones plus the `ccN` aliases just read off
/// its shell — whether it is logged in *here*. Never fails: an identity whose CLI is missing,
/// or whose probe could not run, is reported as unknown.
async fn detect_identities(
    app: &Arc<App>,
    host: &str,
    tools: &BTreeMap<String, ToolInfo>,
    shell: &[crate::config::IdentityCfg],
) -> BTreeMap<String, IdentityInfo> {
    let cfg = app.cfg.get().await;
    // Config first, then the discovered aliases that do not collide — same precedence as
    // [`identities_for_host`], which is what actually starts the bots.
    let mut all: Vec<(&crate::config::IdentityCfg, &'static str)> =
        cfg.identities.iter().map(|i| (i, SOURCE_CONFIG)).collect();
    for i in shell {
        if !all.iter().any(|(x, _)| x.name == i.name) {
            all.push((i, SOURCE_SHELL));
        }
    }
    let home = host_home(app, host).await;
    let dir_of = |i: &crate::config::IdentityCfg| {
        i.env.get("CLAUDE_CONFIG_DIR").map(|v| crate::config::expand_home(v, &home))
    };
    let mut out: BTreeMap<String, IdentityInfo> = all
        .iter()
        .map(|(i, src)| (i.name.clone(), IdentityInfo::unknown(&i.name, &i.kind, src, dir_of(i))))
        .collect();
    if out.is_empty() {
        return out;
    }
    let mut items = Vec::new();
    for (i, _) in &all {
        if login_status_args(&i.kind).is_none() {
            continue;
        }
        // Without an installed CLI there is nothing to ask; `logged_in` stays unknown and the
        // UI falls back to the host's `tools` row ("not installed").
        let Some(bin) = tools.get(&i.kind).and_then(|t| t.path.clone()) else { continue };
        let env = i
            .env
            .iter()
            .filter(|(k, _)| valid_env_name(k))
            .map(|(k, v)| (k.clone(), crate::config::expand_home(v, &home)))
            .collect();
        items.push(IdentityProbe { name: i.name.clone(), kind: i.kind.clone(), bin, env });
    }
    if items.is_empty() {
        return out;
    }
    let kinds: BTreeMap<String, String> = items.iter().map(|i| (i.name.clone(), i.kind.clone())).collect();
    let script = identity_probe_sh(&items);
    let res = if host == LOCAL_HOST {
        run_local(&script, IDENTITY_PROBE_TIMEOUT).await
    } else {
        match app.hosts.get(host).await {
            Some(conn) => conn.ssh_exec_path(&script).await,
            None => Err(anyhow::anyhow!("unknown host `{host}`")),
        }
    };
    match res {
        Ok(o) => {
            for (name, mut info) in parse_identity_probe(&o, &kinds) {
                if let Some(prev) = out.get(&name) {
                    info.source = prev.source;
                    info.config_dir = prev.config_dir.clone();
                }
                out.insert(name, info);
            }
        }
        Err(e) => tracing::warn!(host, error = %e, "identity login detection failed"),
    }
    out
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(40);
/// One CLI round trip per identity, so this gets more room than the single tools probe.
const IDENTITY_PROBE_TIMEOUT: Duration = Duration::from_secs(90);

async fn run_local(script: &str, budget: Duration) -> Result<String> {
    let o = tokio::time::timeout(
        budget,
        tokio::process::Command::new("/bin/sh").arg("-c").arg(script).stdin(std::process::Stdio::null()).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("local probe timed out"))??;
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}

/// Run the probe on `host` and cache the result. Errors are returned (remote ssh failures);
/// the cache is left untouched then. The identity pass runs afterwards and never fails the
/// whole detection — a missing login answer is "unknown", not "no tools".
pub async fn detect(app: &Arc<App>, host: &str) -> Result<HostTools> {
    let out = if host == LOCAL_HOST {
        run_local(PROBE_SH, PROBE_TIMEOUT).await?
    } else {
        let conn = app.hosts.get(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
        conn.ssh_exec_path(PROBE_SH).await?
    };
    let tools = parse_probe(&out);
    let shell_identities = parse_shell_identities(&out);
    let identities = detect_identities(app, host, &tools, &shell_identities).await;
    let ht = HostTools { tools, identities, shell_identities, checked_at: crate::db::now() };
    app.tools.lock().await.insert(host.to_string(), ht.clone());
    tracing::info!(
        host,
        tools = ?ht.tools.iter().map(|(k, t)| (k.clone(), t.installed, t.logged_in)).collect::<Vec<_>>(),
        identities = ?ht.identities.values().map(|i| (i.name.clone(), i.source, i.logged_in)).collect::<Vec<_>>(),
        "tools detected"
    );
    Ok(ht)
}

/// Fire-and-forget detection (on connect); pushes `host_changed` when done so the UI refetches.
pub fn spawn_detect(app: Arc<App>, host: String) {
    tokio::spawn(async move {
        match detect(&app, &host).await {
            Ok(_) => {
                if let Some(conn) = app.hosts.get(&host).await {
                    crate::state::emit_host_changed(&app, &conn).await;
                }
            }
            Err(e) => tracing::warn!(host, error = %e, "tool detection failed"),
        }
    });
}

/// How often the `ccN` aliases are re-read. Cheap (one login shell, no CLI), so a new
/// alias in `~/.zshrc` shows up within a minute instead of at the next daemon restart.
const ALIAS_POLL_EVERY: Duration = Duration::from_secs(60);

/// Read only the aliases off `host`; `None` when the host is unreachable.
async fn poll_aliases(app: &Arc<App>, host: &str) -> Option<Vec<crate::config::IdentityCfg>> {
    let out = if host == LOCAL_HOST {
        run_local(ALIAS_SH, PROBE_TIMEOUT).await
    } else {
        let conn = app.hosts.get(host).await?;
        if !conn.connected.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        conn.ssh_exec_path(ALIAS_SH).await
    };
    match out {
        Ok(o) => Some(parse_shell_identities(&o)),
        Err(e) => {
            tracing::debug!(host, error = %e, "alias poll failed");
            None
        }
    }
}

/// Every [`ALIAS_POLL_EVERY`] compare each host's `ccN` aliases with the cached detection
/// and run a full [`detect`] only when the set changed (a name added, removed, or pointed
/// at a different `CLAUDE_CONFIG_DIR`). Hosts with no cache yet are left to their own
/// first detection, which runs on connect.
pub fn spawn_alias_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(ALIAS_POLL_EVERY).await;
            for name in app.hosts.names().await {
                let Some(cached) = app.tools.lock().await.get(&name).map(|t| t.shell_identities.clone()) else { continue };
                let Some(now) = poll_aliases(&app, &name).await else { continue };
                if now == cached {
                    continue;
                }
                tracing::info!(host = %name, before = ?cached.iter().map(|i| &i.name).collect::<Vec<_>>(),
                    after = ?now.iter().map(|i| &i.name).collect::<Vec<_>>(), "ccN aliases changed; re-detecting");
                spawn_detect(app.clone(), name);
            }
        }
    });
}

/// The resolved executable path for a kind on a host, from the detection cache.
pub async fn cached_path(app: &Arc<App>, host: &str, kind: &str) -> Option<String> {
    app.tools.lock().await.get(host).and_then(|h| h.tools.get(kind)).and_then(|t| t.path.clone())
}

/// The prompt sent to an existing agent to install + log in `kind` (official installers).
pub fn install_prompt(kind: &str) -> Option<String> {
    let (name, install, login) = match kind {
        "claude" => (
            "Claude Code",
            "curl -fsSL https://claude.ai/install.sh | bash   （官方安裝腳本；若失敗可改用 npm i -g @anthropic-ai/claude-code）",
            "claude   （首次啟動會進入登入流程）",
        ),
        "codex" => ("OpenAI Codex CLI", "npm i -g @openai/codex   （官方安裝方式）", "codex login"),
        "grok" => ("xAI Grok CLI", "curl -fsSL https://x.ai/cli/install.sh | bash   （官方安裝腳本）", "grok login"),
        _ => return None,
    };
    Some(format!(
        "請在這台機器上安裝並登入 {name}，步驟如下，逐步執行並回報每一步的輸出：\n\
         1. 安裝：`{install}`。\n\
         2. 確認安裝成功：執行 `{kind} --version` 並印出結果（若找不到指令，檢查安裝腳本輸出的安裝路徑並加入 PATH，例如 ~/.local/bin 或 ~/.{kind}/bin）。\n\
         3. 登入：執行 `{login}`。這是互動式流程，會顯示一個登入 URL（或裝置代碼）；請把該 URL 原封不動、完整地印出來給我，然後停在那裡等待我在瀏覽器完成登入，不要自行中斷或略過。\n\
         4. 登入完成後再執行一次 `{kind} --version` 確認，並回報「{kind} 已安裝並登入」。",
    ))
}

/// `POST /api/hosts/:name/tools/install` — validate, then send the prompt through the
/// ordinary prompt path (per-bot lock, idempotency, delivery states).
pub async fn install_via_bot(
    app: &Arc<App>,
    host: &str,
    kind: &str,
    via_bot_id: &str,
) -> crate::lifecycle::LcResult<crate::lifecycle::PromptOut> {
    use crate::lifecycle::LcError;
    if !valid_kind(kind) {
        return Err(LcError::Bad(format!("kind must be {}", crate::config::kinds_list())));
    }
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let bot = crate::db::bot(&app.db, via_bot_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|b| b.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("bot".into()))?;
    let bot_host = crate::db::bot_host(&app.db, &bot.id).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if bot_host != host {
        return Err(LcError::Bad(format!("bot `{}` lives on host `{bot_host}`, not `{host}`", bot.name)));
    }
    let text = install_prompt(kind).ok_or_else(|| LcError::Bad("unknown kind".into()))?;
    let crid = format!("tools-install:{kind}:{}", crate::db::ulid());
    crate::lifecycle::prompt(app, &bot.id, &text, &crid).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn onboarding_flag_is_set_only_when_logged_in_and_missing() {
        let dir = std::env::temp_dir().join(format!("am-onboard-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".claude.json");
        // No account: leave it alone (the TUI has to log in anyway).
        std::fs::write(&f, r#"{"theme":"dark"}"#).unwrap();
        assert!(!super::ensure_claude_onboarded(&dir));
        // Account but no flag: set it.
        std::fs::write(&f, r#"{"oauthAccount":{"emailAddress":"x@y"},"theme":"dark"}"#).unwrap();
        assert!(super::ensure_claude_onboarded(&dir));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["hasCompletedOnboarding"], true);
        assert_eq!(v["oauthAccount"]["emailAddress"], "x@y");
        // Already set: no rewrite.
        assert!(!super::ensure_claude_onboarded(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    use super::*;

    /// Real `alias` output from both machines (2026-09-06): the local box keys cc1/cc2 to
    /// `~/.claude-cc1` / `-cc2`, m4p keys cc1 to `~/.claude-ccompany`.
    #[test]
    fn reads_ccn_aliases_off_the_shell() {
        let out = r#"
AM_PATH claude /opt/homebrew/bin/claude
AM_ALIAS cc='claude'
AM_ALIAS cc0='claude --dangerously-skip-permissions'
AM_ALIAS cc1='CLAUDE_CONFIG_DIR=$HOME/.claude-cc1 claude --dangerously-skip-permissions'
AM_ALIAS cc2='CLAUDE_CONFIG_DIR=$HOME/.claude-cc2 claude --dangerously-skip-permissions'
"#;
        let ids = parse_shell_identities(out);
        assert_eq!(ids.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["cc0", "cc1", "cc2"]);
        assert!(ids[0].env.is_empty(), "cc0 is the default account: no config dir");
        assert_eq!(ids[1].env["CLAUDE_CONFIG_DIR"], "$HOME/.claude-cc1");
        assert_eq!(ids[2].env["CLAUDE_CONFIG_DIR"], "$HOME/.claude-cc2");
        // The alias's own flags are never taken (the daemon owns those).
        assert!(ids.iter().all(|i| i.args.is_empty() && i.kind == "claude"));

        let remote = "AM_ALIAS cc1='CLAUDE_CONFIG_DIR=$HOME/.claude-ccompany claude --dangerously-skip-permissions'";
        assert_eq!(parse_shell_identities(remote)[0].env["CLAUDE_CONFIG_DIR"], "$HOME/.claude-ccompany");
    }

    #[test]
    fn ignores_aliases_that_are_not_ours() {
        // bash prints a leading `alias `; zsh does not. Both are read.
        let ids = parse_shell_identities("AM_ALIAS alias cc3=\"CLAUDE_CONFIG_DIR=~/.c3 claude\"");
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].env["CLAUDE_CONFIG_DIR"], "~/.c3");
        // Not claude, out of range, or the config dir set after the binary → skipped.
        for line in [
            "AM_ALIAS cc1='CLAUDE_CONFIG_DIR=$HOME/.x codex'",
            "AM_ALIAS cc7='CLAUDE_CONFIG_DIR=$HOME/.x claude'",
            "AM_ALIAS ccx='CLAUDE_CONFIG_DIR=$HOME/.x claude'",
            "AM_ALIAS cc1='claude --settings CLAUDE_CONFIG_DIR=$HOME/.x'",
        ] {
            let got = parse_shell_identities(line);
            assert!(
                got.is_empty() || got[0].env.is_empty(),
                "should not have taken a config dir from `{line}`: {got:?}"
            );
        }
        // A later definition of the same name wins, the way the shell resolves it.
        let ids = parse_shell_identities(
            "AM_ALIAS cc1='CLAUDE_CONFIG_DIR=/a claude'\nAM_ALIAS cc1='CLAUDE_CONFIG_DIR=/b claude'",
        );
        assert_eq!(ids[0].env["CLAUDE_CONFIG_DIR"], "/b");
    }

    #[test]
    fn parses_probe_output() {
        let out = "AM_PATH claude /opt/homebrew/bin/claude\nAM_VER claude 2.1.0 (Claude Code)\nAM_PATH codex /usr/local/bin/codex\nAM_VER codex codex-cli 0.120.0\nAM_PATH grok \nAM_LOGIN claude ?\nAM_LOGIN codex 1\nAM_LOGIN grok 0\n";
        let m = parse_probe(out);
        assert!(m["claude"].installed);
        assert_eq!(m["claude"].path.as_deref(), Some("/opt/homebrew/bin/claude"));
        assert_eq!(m["claude"].version.as_deref(), Some("2.1.0 (Claude Code)"));
        assert_eq!(m["claude"].logged_in, None);
        assert_eq!(m["codex"].logged_in, Some(true));
        assert!(!m["grok"].installed);
        assert!(m["grok"].path.is_none());
        // not installed + "no auth file" → unknown, not false
        assert_eq!(m["grok"].logged_in, None);
    }

    fn probe(name: &str, kind: &str, env: &[(&str, &str)]) -> IdentityProbe {
        IdentityProbe {
            name: name.into(),
            kind: kind.into(),
            bin: format!("/opt/homebrew/bin/{kind}"),
            env: env.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
        }
    }

    fn kinds(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(n, k)| ((*n).to_string(), (*k).to_string())).collect()
    }

    #[test]
    fn identity_script_exports_env_per_subshell() {
        let sh = identity_probe_sh(&[
            probe("gk0", "grok", &[]),
            probe("cx1", "codex", &[("CODEX_HOME", "/Users/m4p/.codex-alt")]),
        ]);
        assert!(sh.contains("AM_IDENT_BEGIN %s\\n' 'gk0'"));
        assert!(sh.contains("AM_IDENT_END %s\\n' 'cx1'"));
        // gk0 has no env of its own, so its subshell must not carry cx1's.
        let gk0 = sh.split("AM_IDENT_BEGIN %s\\n' 'gk0'").nth(1).unwrap().split("AM_IDENT_END").next().unwrap();
        assert!(!gk0.contains("CODEX_HOME"));
        assert!(gk0.contains("exec '/opt/homebrew/bin/grok' 'models'"));
        assert!(sh.contains("CODEX_HOME='/Users/m4p/.codex-alt'; export CODEX_HOME;"));
        // stdin closed: none of these CLIs may wait for a TTY.
        assert!(sh.contains("</dev/null"));
    }

    /// claude 不走這條 ssh 路（憑證可能在 Keychain 裡，非登入 shell 看不到），
    /// 它的登入答案由 [`crate::quota_claude`] 的 pane 探測帶回來。
    #[test]
    fn claude_is_not_asked_over_ssh() {
        assert!(login_status_args("claude").is_none());
        let sh = identity_probe_sh(&[probe("cc1", "claude", &[("CLAUDE_CONFIG_DIR", "/Users/m4p/.claude-ccompany")])]);
        assert_eq!(sh, "");
    }

    #[test]
    fn identity_script_drops_env_names_that_are_not_shell_identifiers() {
        let sh = identity_probe_sh(&[probe("cx1", "codex", &[("OK_VAR", "1"), ("bad name", "2"), ("2BAD", "3")])]);
        assert!(sh.contains("OK_VAR='1'"));
        assert!(!sh.contains("bad name"));
        assert!(!sh.contains("2BAD"));
    }

    #[test]
    fn reads_claude_auth_status_json() {
        let out = "AM_IDENT_BEGIN cc0\n{\n  \"loggedIn\": true,\n  \"email\": \"a@b.c\",\n  \"subscriptionType\": \"max\"\n}\nAM_IDENT_END cc0\nAM_IDENT_BEGIN cc1\n{\"loggedIn\": false, \"authMethod\": \"none\"}\nAM_IDENT_END cc1\n";
        let m = parse_identity_probe(out, &kinds(&[("cc0", "claude"), ("cc1", "claude")]));
        assert_eq!(m["cc0"].logged_in, Some(true));
        assert_eq!(m["cc0"].account.as_deref(), Some("a@b.c"));
        assert_eq!(m["cc0"].plan.as_deref(), Some("max"));
        assert_eq!(m["cc1"].logged_in, Some(false));
        assert_eq!(m["cc1"].account, None);
    }

    #[test]
    fn reads_codex_and_grok_answers() {
        let out = "AM_IDENT_BEGIN cx\nLogged in using ChatGPT\nAM_IDENT_END cx\nAM_IDENT_BEGIN gk\nYou are not authenticated.\n\nDefault model: grok-4.6\nAM_IDENT_END gk\nAM_IDENT_BEGIN gk2\nYou are logged in with grok.com.\nAM_IDENT_END gk2\n";
        let m = parse_identity_probe(out, &kinds(&[("cx", "codex"), ("gk", "grok"), ("gk2", "grok")]));
        assert_eq!(m["cx"].logged_in, Some(true));
        assert_eq!(m["cx"].account.as_deref(), Some("ChatGPT"));
        assert_eq!(m["gk"].logged_in, Some(false));
        assert_eq!(m["gk2"].logged_in, Some(true));
        assert_eq!(m["gk2"].account.as_deref(), Some("grok.com"));
    }

    #[test]
    fn unreadable_answers_stay_unknown_not_logged_out() {
        // Empty (the CLI died), garbage, and a truncated block are all "could not tell".
        let out = "AM_IDENT_BEGIN a\nAM_IDENT_END a\nAM_IDENT_BEGIN b\nzsh: command not found\nAM_IDENT_END b\nAM_IDENT_BEGIN c\n{\"loggedIn\":true}\n";
        let m = parse_identity_probe(out, &kinds(&[("a", "claude"), ("b", "claude"), ("c", "claude")]));
        assert_eq!(m["a"].logged_in, None);
        assert_eq!(m["b"].logged_in, None);
        // `c` never closed, so it is not reported at all (the caller keeps its unknown row).
        assert!(!m.contains_key("c"));
    }

    /// claude 例外（見 [`claude_is_not_asked_over_ssh`]）；其餘每種 CLI 都要有一條 ssh 問法。
    #[test]
    fn every_other_kind_has_a_login_question() {
        for k in crate::config::KINDS.iter().filter(|k| **k != "claude") {
            assert!(login_status_args(k).is_some(), "{k} has no login status command");
        }
        assert!(login_status_args("nope").is_none());
    }

    #[test]
    fn install_prompts_use_official_installers() {
        assert!(install_prompt("grok").unwrap().contains("https://x.ai/cli/install.sh"));
        assert!(install_prompt("claude").unwrap().contains("https://claude.ai/install.sh"));
        assert!(install_prompt("codex").unwrap().contains("npm i -g @openai/codex"));
        assert!(install_prompt("codex").unwrap().contains("codex login"));
        assert!(install_prompt("nope").is_none());
    }
}
