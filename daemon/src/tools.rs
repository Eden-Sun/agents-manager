//! Per-host CLI detection, per-identity login state and `ccN` alias discovery (see SPEC §16).
//! Executables are looked up through the user's *login* shell: the daemon / non-interactive ssh has a bare PATH.
//! Login is asked per host under the identity's env — the credential may live in the Keychain, not a file.

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

/// One identity as seen *from one host*.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IdentityInfo {
    pub name: String,
    pub kind: String,
    /// `None` = could not tell (CLI missing, probe failed, output not understood).
    pub logged_in: Option<bool>,
    /// Explicit so a failed probe cannot look like a successful (or empty) status in the UI.
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// `config` | `shell` (`ccN` alias); only a config one can be edited or deleted.
    pub source: &'static str,
    /// Display only — the real env goes through [`identities_for_host`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

pub const SOURCE_CONFIG: &str = "config";
pub const SOURCE_SHELL: &str = "shell";

impl IdentityInfo {
    pub fn shell(name: &str, kind: &str, config_dir: Option<String>) -> Self {
        Self::unknown(name, kind, SOURCE_SHELL, config_dir)
    }

    fn unknown(name: &str, kind: &str, source: &'static str, config_dir: Option<String>) -> Self {
        Self {
            name: name.to_string(),
            kind: kind.to_string(),
            logged_in: None,
            reason: None,
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
    pub identities: BTreeMap<String, IdentityInfo>,
    /// Re-read on every detection and never written back to `config.toml`.
    pub shell_identities: Vec<crate::config::IdentityCfg>,
    pub checked_at: String,
}

/// Separate so the poller can re-run just the alias part without a CLI round trip (SPEC §16.5).
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

/// Lines are `AM_<WHAT> <kind> <value>`. `security` without `-w` needs no unlock but a
/// non-interactive session can still be refused — reported as unknown, not "not logged in".
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
    // Keep a not-installed tool's login answer only when positive (credentials may survive an uninstall).
    for t in m.values_mut() {
        if !t.installed && t.logged_in == Some(false) {
            t.logged_in = None;
        }
    }
    m
}

pub const SHELL_IDENTITY_NAMES: [&str; 7] = ["cc0", "cc1", "cc2", "cc3", "cc4", "cc5", "cc6"];

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
/// Only a leading assignment counts (the only place a shell honours it); the flags are not ours.
fn config_dir_of(cmd: &str) -> Option<String> {
    const KEY: &str = "CLAUDE_CONFIG_DIR=";
    let at = cmd.find(KEY)?;
    // Only `VAR=value` before it; `env …` or a dir set after the binary is skipped, not guessed.
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

/// An alias without `CLAUDE_CONFIG_DIR` becomes an **empty env** identity — the default account
/// (folded onto the bare `claude` quota key).
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
        match config_dir_of(&cmd) {
            Some(dir) => {
                env.insert("CLAUDE_CONFIG_DIR".to_string(), dir);
            }
            // Picks a config dir in a shape we don't read: treating it as default would run bots
            // on the wrong login — skip rather than guess.
            None if cmd.contains("CLAUDE_CONFIG_DIR=") => continue,
            None => {}
        }
        // Later definitions win, like the shell.
        found.insert(
            name.to_string(),
            crate::config::IdentityCfg { name: name.to_string(), kind: "claude".into(), host: None, env, args: vec![] },
        );
    }
    SHELL_IDENTITY_NAMES.iter().filter_map(|n| found.remove(*n)).collect()
}

/// 一台主機看得到哪些身分，照優先序合併（SPEC §16.2）。**只有這一份規則**：啟動 bot 的
/// [`identities_for_host`] 與偵測登入狀態的 `detect_identities` 都走它。`shell` 是那台讀到的 `ccN`，`None`＝還沒偵測過。
///
/// 1. config 裡**明寫這一台**的。
/// 2. 本機：沒寫 host 的 config 身分——現行 config.toml 在本機的行為一個字都沒變。
/// 3. 那台自己的 shell `ccN`。
/// 4. 遠端：沒寫 host 的 config 身分，**讓位給**那台同名的身分。名字是 `ccN` 時還要等那台的 alias 讀過才給：
///    偵測前不知道那台有沒有自己的 `cc1`，先給就是注入一個那台不存在的設定目錄（26a14c2 要擋的遮蔽）。
///
/// 以前第 4 條不存在（沒寫 host＝只適用本機）：codex／grok 身分只可能寫在 config 裡，升級後遠端 bot 一啟動就 409
/// `identity is not known on this host`（review 2026-09-16 M6）。
pub fn merge_identities(
    config: &[crate::config::IdentityCfg],
    host: &str,
    shell: Option<&[crate::config::IdentityCfg]>,
) -> Vec<(crate::config::IdentityCfg, &'static str)> {
    let host = if host.is_empty() { LOCAL_HOST } else { host };
    let mut out: Vec<(crate::config::IdentityCfg, &'static str)> = Vec::new();
    let push = |out: &mut Vec<(crate::config::IdentityCfg, &'static str)>, i: &crate::config::IdentityCfg, src: &'static str| {
        if !out.iter().any(|(x, _)| x.name == i.name) {
            out.push((i.clone(), src));
        }
    };
    for i in config.iter().filter(|i| !i.is_hostless() && i.host_or_local() == host) {
        push(&mut out, i, SOURCE_CONFIG);
    }
    if host == LOCAL_HOST {
        for i in config.iter().filter(|i| i.is_hostless()) {
            push(&mut out, i, SOURCE_CONFIG);
        }
    }
    for i in shell.unwrap_or_default() {
        push(&mut out, i, SOURCE_SHELL);
    }
    if host != LOCAL_HOST {
        for i in config.iter().filter(|i| i.is_hostless()) {
            if shell.is_none() && SHELL_IDENTITY_NAMES.contains(&i.name.as_str()) {
                continue;
            }
            push(&mut out, i, SOURCE_CONFIG);
        }
    }
    out
}

/// Hand-written `[[identities]]` win over a colliding `ccN` alias on the host they are written for
/// (see [`merge_identities`]).
pub async fn identities_for_host(app: &Arc<App>, host: &str) -> Vec<crate::config::IdentityCfg> {
    let cfg = app.cfg.get().await;
    let tools = app.tools.lock().await;
    let shell = tools.get(host).map(|t| t.shell_identities.as_slice());
    merge_identities(&cfg.identities, host, shell).into_iter().map(|(i, _)| i).collect()
}

pub async fn identity_for_host(app: &Arc<App>, host: &str, name: &str) -> Option<crate::config::IdentityCfg> {
    identities_for_host(app, host).await.into_iter().find(|i| i.name == name)
}

/// Deliberately *not* a file check: a claude account can live in the Keychain with no `.credentials.json`.
/// claude is **remote-only excluded**: over non-login ssh it can't read the Keychain and says
/// `loggedIn: false`, so remote claude is still asked in a pane ([`crate::quota_claude`] →
/// [`record_identity_login`]); locally `auth status --json` is authoritative and per config dir.
/// grok has no `status` subcommand, so `models` is used.
pub fn login_status_args(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "codex" => Some(&["login", "status"]),
        "grok" => Some(&["models"]),
        _ => None,
    }
}

/// What the periodic pass may ask on this host. claude only locally (see [`login_status_args`]).
pub fn login_probe_args(kind: &str, host: &str) -> Option<&'static [&'static str]> {
    if kind == "claude" {
        return (host == LOCAL_HOST).then_some(CLAUDE_LOGIN_ARGS);
    }
    login_status_args(kind)
}

pub const CLAUDE_LOGIN_ARGS: &[&str] = &["auth", "status", "--json"];

/// Sent to a temporary host shell so the device code / URL stays terminal output visible only to
/// the UI. Identity env is quoted as shell data; never logged or persisted by this path.
pub fn identity_login_command(kind: &str, env: &BTreeMap<String, String>) -> Option<String> {
    let login = match kind {
        "claude" => "claude /login",
        "codex" => "codex login",
        "grok" => "grok login",
        _ => return None,
    };
    Some(with_identity_env(login, env))
}

/// 登出，跟 [`identity_login_command`] 對稱：claude 沒有 `logout` 子命令，用 REPL 的斜線指令
/// （`/login` 也是這樣帶），codex／grok 有自己的子命令。帳號的認證資料在那個身份的設定目錄裡，
/// 所以環境變數前綴跟登入完全一樣——少帶一個就會去登出**別的**帳號。
pub fn identity_logout_command(kind: &str, env: &BTreeMap<String, String>) -> Option<String> {
    let logout = match kind {
        "claude" => "claude /logout",
        "codex" => "codex logout",
        "grok" => "grok logout",
        _ => return None,
    };
    Some(with_identity_env(logout, env))
}

fn with_identity_env(cmd: &str, env: &BTreeMap<String, String>) -> String {
    let prefix = env
        .iter()
        .filter(|(k, _)| valid_env_name(k))
        .map(|(k, v)| format!("{k}={}", sh_quote(v)))
        .collect::<Vec<_>>();
    if prefix.is_empty() {
        cmd.to_string()
    } else {
        format!("env {} {cmd}", prefix.join(" "))
    }
}

/// Returns whether anything changed, so the caller only pushes `host_changed` when needed.
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

/// 這次重驗的答案要不要寫回快取。遠端讀到「未登入」只有 **claude** 不可信（ssh 沒有 GUI session、
/// 讀不到 Keychain）；codex／grok 的 `login status` 在遠端照樣準——一律丟掉的話，在遠端主機按「登出」
/// 之後列上還是「已登入」、登出鈕也還在，要等手動重新偵測或 daemon 重啟（review3 c5 L3）。
pub(crate) fn login_answer_to_cache(local: bool, kind: &str, logged_in: bool) -> Option<bool> {
    if !local && !logged_in && kind == "claude" {
        return None;
    }
    Some(logged_in)
}

/// 登出的 pane 收尾時要往快取寫什麼：重驗說還登著就照實（登出沒成功），問不出來就記未登入——
/// 剛剛才親手下過登出指令，寧可顯示未登入也不要留一個按不完的登出鈕（review3 c5 L3）。
pub(crate) fn logout_result(recheck: Option<bool>) -> Option<bool> {
    match recheck {
        Some(true) => Some(true),
        _ => Some(false),
    }
}

/// 記下「這個身分已登出」：`account`／`plan` 一起清掉，否則列上會是「未登入」配著上一個帳號。
pub async fn record_identity_logged_out(app: &Arc<App>, host: &str, name: &str, reason: &str) -> bool {
    let mut all = app.tools.lock().await;
    let Some(ht) = all.get_mut(host) else { return false };
    let Some(info) = ht.identities.get_mut(name) else { return false };
    let before = (info.logged_in, info.account.clone(), info.plan.clone());
    info.logged_in = Some(false);
    info.account = None;
    info.plan = None;
    info.reason = Some(reason.to_string());
    before != (info.logged_in, info.account.clone(), info.plan.clone())
}

/// The poller parks a logged-out identity for 30 min, so after a login the cache stays stale;
/// `start_bot` rechecks before warning.
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
    let to_cache = login_answer_to_cache(host == LOCAL_HOST, &idn.kind, logged_in)?;
    if record_identity_login(app, host, name, Some(to_cache), account, plan).await {
        app.emit("host_changed", serde_json::json!({"host": host})).await;
    }
    if logged_in && idn.kind == "claude" {
        crate::quota_claude::unpark_identity(host, name);
    }
    Some(logged_in)
}

/// Never copies the login pane's terminal output anywhere else.
///
/// `logout`＝這個 pane 下的是登出指令：CLI 跑完之後重驗問不出來（遠端 claude 一律問不出來）就直接記未登入，
/// 不然列上會一直顯示「已登入」、登出鈕也還按得下去（review3 c5 L3）。
pub fn spawn_identity_login_watch(
    app: Arc<App>,
    host: String,
    pane_id: String,
    name: String,
    kind: String,
    logout: bool,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15 * 60);
        let startup_deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        let mut saw_cli = false;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some((client, _)) = crate::api::shell::client_for(&app, &host).await.ok() else { return };
            let Ok(processes) = client.pane_process_info(&pane_id).await else {
                if tokio::time::Instant::now() >= deadline {
                    let _ = crate::api::shell::close(&app, &host, &pane_id).await;
                    return;
                }
                continue;
            };
            let cli_active = processes.iter().any(|p| {
                p.argv.iter().chain(p.argv0.iter()).any(|arg| {
                    std::path::Path::new(arg).file_name().and_then(|v| v.to_str()) == Some(kind.as_str())
                })
            });
            saw_cli |= cli_active;
            if (saw_cli && !cli_active) || (!saw_cli && tokio::time::Instant::now() >= startup_deadline) {
                let after = recheck_identity_login(&app, &host, &name).await;
                if logout && logout_result(after) == Some(false) {
                    let changed =
                        record_identity_logged_out(&app, &host, &name, "剛剛在這台主機登出（重驗問不出來時照登出算）").await;
                    if changed {
                        app.emit("host_changed", serde_json::json!({"host": host})).await;
                    }
                }
                let _ = crate::api::shell::close(&app, &host, &pane_id).await;
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = crate::api::shell::close(&app, &host, &pane_id).await;
                return;
            }
        }
    });
}

/// Headless `claude auth login` never marks onboarding done, so interactive `claude` opens on
/// 「Select login method」 despite being logged in (cc2, 2026-09-08). Local host only.
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

#[derive(Debug, Clone)]
pub struct IdentityProbe {
    pub name: String,
    pub kind: String,
    /// Absolute path: the probe runs in a bare PATH.
    pub bin: String,
    /// `$HOME` expanded against *this* host's home.
    pub env: BTreeMap<String, String>,
    /// What to ask this CLI; `None` = this kind's default ([`login_status_args`]).
    pub args: Option<Vec<&'static str>>,
}

/// Config is user-written and `export`ed into a script, so invalid names are dropped, not quoted.
pub(crate) fn valid_env_name(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Each call in its own subshell (env never leaks to the next) with stdin closed (no TTY wait).
pub fn identity_probe_sh(items: &[IdentityProbe]) -> String {
    let mut s = String::new();
    for it in items {
        let Some(args) = it.args.clone().or_else(|| login_status_args(&it.kind).map(<[&str]>::to_vec)) else { continue };
        let args = args.as_slice();
        s.push_str(&format!("printf 'AM_IDENT_BEGIN %s\\n' {}\n", sh_quote(&it.name)));
        s.push('(');
        for (k, v) in it.env.iter().filter(|(k, _)| valid_env_name(k)) {
            s.push_str(&format!(" {k}={}; export {k};", sh_quote(v)));
        }
        s.push_str(&format!(" {}", sh_quote(&it.bin)));
        for a in args {
            s.push(' ');
            s.push_str(&sh_quote(a));
        }
        s.push_str("; rc=$?; printf '\\nAM_IDENT_RC %s %s\\n' ");
        s.push_str(&format!("{} \"$rc\" ) </dev/null 2>/dev/null\n", sh_quote(&it.name)));
        // The leading newline keeps the fence on its own line when the CLI ends without one.
        s.push_str(&format!("printf '\\nAM_IDENT_END %s\\n' {}\n", sh_quote(&it.name)));
    }
    s
}

/// Anything unrecognised stays `None` ("could not tell"), never `Some(false)`.
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
            let mut answer_body = String::new();
            let mut rc = None;
            for answer_line in body.lines() {
                if let Some(rest) = answer_line.strip_prefix("AM_IDENT_RC ") {
                    let mut fields = rest.split_whitespace();
                    let marker_name = fields.next().unwrap_or_default();
                    if marker_name == open {
                        rc = fields.next().and_then(|v| v.parse::<i32>().ok());
                        continue;
                    }
                }
                answer_body.push_str(answer_line);
                answer_body.push('\n');
            }
            let (logged_in, account, plan) = read_login_answer(kind, &answer_body);
            let reason = if rc.is_some_and(|code| code != 0) {
                Some(format!("auth status 指令失敗（exit code {}）", rc.unwrap_or_default()))
            } else if rc.is_none() {
                Some("auth status probe 沒有完成".into())
            } else if logged_in.is_none() {
                Some("auth status 輸出無法解析".into())
            } else {
                None
            };
            // `source` / `config_dir` are filled in by the caller.
            m.insert(
                open.clone(),
                IdentityInfo {
                    name: open,
                    kind: kind.clone(),
                    logged_in,
                    reason,
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

pub(crate) async fn host_home(app: &Arc<App>, host: &str) -> String {
    if let Some(conn) = app.hosts.get(host).await {
        if let Ok(h) = conn.home().await {
            return h;
        }
    }
    dirs::home_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
}

/// Never fails: a missing CLI or failed probe is reported as unknown.
async fn detect_identities(
    app: &Arc<App>,
    host: &str,
    tools: &BTreeMap<String, ToolInfo>,
    shell: &[crate::config::IdentityCfg],
) -> BTreeMap<String, IdentityInfo> {
    let cfg = app.cfg.get().await;
    // Same precedence as [`identities_for_host`], which is what actually starts the bots. 以前這裡把 config 裡
    // **每一台**的身分都列進來（包括明寫給別台的），用這台的 env 去問登入狀態。
    let all = merge_identities(&cfg.identities, host, Some(shell));
    let home = host_home(app, host).await;
    let dir_of = |i: &crate::config::IdentityCfg| {
        i.env.get("CLAUDE_CONFIG_DIR").map(|v| crate::config::expand_home(v, &home))
    };
    let mut out: BTreeMap<String, IdentityInfo> = all
        .iter()
        .map(|(i, src)| {
            let mut info = IdentityInfo::unknown(&i.name, &i.kind, src, dir_of(i));
            info.reason = Some(match tools.get(&i.kind).and_then(|t| t.path.as_ref()) {
                Some(_) if login_probe_args(&i.kind, host).is_some() => "auth status 尚未取得結果".into(),
                // 遠端 claude 讀不到 Keychain，只能等 pane 探測（`quota_claude`）。
                Some(_) if i.kind == "claude" => "遠端 claude 的登入狀態要等 pane 探測".into(),
                Some(_) => "這個 kind 沒有可用的 auth status 探測".into(),
                None => format!("{} CLI 不在 PATH", i.kind),
            });
            (i.name.clone(), info)
        })
        .collect();
    if out.is_empty() {
        return out;
    }
    let mut items = Vec::new();
    for (i, _) in &all {
        let Some(args) = login_probe_args(&i.kind, host) else { continue };
        let Some(bin) = tools.get(&i.kind).and_then(|t| t.path.clone()) else { continue };
        let env = i
            .env
            .iter()
            .filter(|(k, _)| valid_env_name(k))
            .map(|(k, v)| (k.clone(), crate::config::expand_home(v, &home)))
            .collect();
        items.push(IdentityProbe { name: i.name.clone(), kind: i.kind.clone(), bin, env, args: Some(args.to_vec()) });
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
            // The 30 s default is too short for many identities, and a timeout marks *every* one unprobed.
            Some(conn) => conn.ssh_exec_path_timeout(&script, IDENTITY_PROBE_TIMEOUT).await,
            None => Err(anyhow::anyhow!("unknown host `{host}`")),
        }
    };
    // 重新偵測會整張表重建：這一輪問不到的，沿用上一輪知道的答案，否則每次重探都會把 claude 身分
    // 打回「未知」，UI 看起來就像帳號自己登出了（2026-09-16 使用者）。
    let known = app.tools.lock().await.get(host).map(|t| t.identities.clone()).unwrap_or_default();
    match res {
        Ok(o) => {
            for (name, mut info) in parse_identity_probe(&o, &kinds) {
                if let Some(prev) = out.get(&name) {
                    info.source = prev.source;
                    info.config_dir = prev.config_dir.clone();
                }
                carry_over(&mut info, known.get(&name));
                out.insert(name, info);
            }
        }
        Err(e) => {
            let reason = format!("auth status 探測失敗：{e}");
            for info in out.values_mut() {
                info.reason = Some(reason.clone());
            }
            tracing::warn!(host, error = %e, "identity login detection failed")
        }
    }
    for (name, info) in out.iter_mut() {
        carry_over(info, known.get(name));
    }
    out
}

/// Keep the last known answer when this pass could not tell. A fresh `Some(false)` still wins —
/// that is a real logout; only "問不到" falls back.
fn carry_over(info: &mut IdentityInfo, prev: Option<&IdentityInfo>) {
    let Some(prev) = prev else { return };
    if info.logged_in.is_none() && prev.logged_in.is_some() {
        info.logged_in = prev.logged_in;
        info.reason = prev.reason.clone().or_else(|| info.reason.clone());
    }
    if info.account.is_none() {
        info.account = prev.account.clone();
    }
    if info.plan.is_none() {
        info.plan = prev.plan.clone();
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(40);
const IDENTITY_PROBE_TIMEOUT: Duration = Duration::from_secs(90);

async fn run_local(script: &str, budget: Duration) -> Result<String> {
    let o = crate::hosts::sh_local(script, budget).await?.ok_or_else(|| anyhow::anyhow!("local probe timed out"))?;
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}

/// On error the cache is left untouched; the identity pass never fails the whole detection.
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
    install_host_tools(app, host, ht.clone()).await;
    tracing::info!(
        host,
        tools = ?ht.tools.iter().map(|(k, t)| (k.clone(), t.installed, t.logged_in)).collect::<Vec<_>>(),
        identities = ?ht.identities.values().map(|i| (i.name.clone(), i.source, i.logged_in)).collect::<Vec<_>>(),
        "tools detected"
    );
    Ok(ht)
}

/// 偵測結果寫進 `app.tools`，以及寫完之後一定要跟著做的事（抽出來，測試不必真的跑 shell 探測）。
pub(crate) async fn install_host_tools(app: &Arc<App>, host: &str, ht: HostTools) {
    app.tools.lock().await.insert(host.to_string(), ht);
    // 身分表剛更新：清掉 kind 不符的 identity 與它留下的 quota key（`identity_kind::cleanup_host`）。
    crate::identity_kind::cleanup_host(app, host).await;
    // 身分表齊了，重啟前停下的交辦這時才算得出正確的 quota key（每台主機每個行程只跑一次，review 2026-09-16 M3）。
    crate::supervisor::controller::backfill_quota_limits_once(app, host).await;
}

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

/// Cheap (one login shell, no CLI), so a new alias shows up within a minute, not at restart.
const ALIAS_POLL_EVERY: Duration = Duration::from_secs(60);

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

/// Full [`detect`] only when the alias set changed; uncached hosts wait for their on-connect detection.
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

pub async fn cached_path(app: &Arc<App>, host: &str, kind: &str) -> Option<String> {
    app.tools.lock().await.get(host).and_then(|h| h.tools.get(kind)).and_then(|t| t.path.clone())
}

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

/// `POST /api/hosts/:name/tools/install` — goes through the ordinary prompt path (lock, idempotency).
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
    /// 現行 config.toml 的形狀（`[[identities]]` 不寫 host）在**本機**的行為一個字都不能變，
    /// 但不能再遮蔽遠端同名的 `ccN`——本機 cc1 與 m4p 的 cc1 是不同帳號（SPEC §16.2、review 2026-09-16）。
    /// 也不能因此讓遠端用不到它：codex／grok 身分只可能寫在 config 裡（review 2026-09-16 M6）。
    #[tokio::test]
    async fn a_config_identity_without_a_host_applies_everywhere_but_yields_to_that_hosts_own() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let cfg = |name: &str, kind: &str, host: Option<&str>, var: &str, dir: &str| crate::config::IdentityCfg {
            name: name.into(),
            kind: kind.into(),
            host: host.map(String::from),
            env: [(var.to_string(), dir.to_string())].into(),
            args: vec![],
        };
        app.cfg
            .update(|c| {
                c.identities = vec![
                    cfg("cc1", "claude", None, "CLAUDE_CONFIG_DIR", "/home/me/.claude-cc1"), // 現行形狀
                    cfg("cx2", "codex", None, "CODEX_HOME", "$HOME/.codex-cx2"),
                ];
                Ok(())
            })
            .await
            .unwrap();
        let dir_of = |i: Option<crate::config::IdentityCfg>, var: &str| i.and_then(|i| i.env.get(var).cloned());

        // 本機：照舊拿得到，env 也照舊。
        let local = identity_for_host(app, crate::config::LOCAL_HOST, "cc1").await;
        assert_eq!(dir_of(local, "CLAUDE_CONFIG_DIR").as_deref(), Some("/home/me/.claude-cc1"));

        // 遠端還沒偵測：codex 身分馬上能用（那台不可能有同名 shell 身分）；`ccN` 要等那台的 alias 讀過。
        assert_eq!(dir_of(identity_for_host(app, "m4p", "cx2").await, "CODEX_HOME").as_deref(), Some("$HOME/.codex-cx2"), "codex 身分只能寫在 config 裡，遠端要用得到");
        assert!(identity_for_host(app, "m4p", "cc1").await.is_none(), "還不知道 m4p 有沒有自己的 cc1");

        // m4p 偵測到自己的 cc1：那台的說了算，不被本機那筆遮蔽。
        let shell = |dir: Option<&str>| crate::tools::HostTools {
            tools: Default::default(),
            identities: Default::default(),
            shell_identities: dir.map(|d| vec![cfg("cc1", "claude", None, "CLAUDE_CONFIG_DIR", d)]).unwrap_or_default(),
            checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert("m4p".into(), shell(Some("$HOME/.claude-ccompany")));
        assert_eq!(dir_of(identity_for_host(app, "m4p", "cc1").await, "CLAUDE_CONFIG_DIR").as_deref(), Some("$HOME/.claude-ccompany"));
        // m4p 偵測完、沒有自己的 cc1：沒寫 host 的那筆就適用。
        app.tools.lock().await.insert("m4p".into(), shell(None));
        assert_eq!(dir_of(identity_for_host(app, "m4p", "cc1").await, "CLAUDE_CONFIG_DIR").as_deref(), Some("/home/me/.claude-cc1"));

        // 明寫 host 的最優先，而且只給那一台；`host = "local"` 就是只要本機。
        app.cfg
            .update(|c| {
                c.identities.push(cfg("cc1", "claude", Some("m4p"), "CLAUDE_CONFIG_DIR", "/home/m4p/.claude-ccompany"));
                c.identities.push(cfg("solo", "claude", Some("local"), "CLAUDE_CONFIG_DIR", "/home/me/.claude-solo"));
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(dir_of(identity_for_host(app, "m4p", "cc1").await, "CLAUDE_CONFIG_DIR").as_deref(), Some("/home/m4p/.claude-ccompany"));
        assert_eq!(dir_of(identity_for_host(app, crate::config::LOCAL_HOST, "cc1").await, "CLAUDE_CONFIG_DIR").as_deref(), Some("/home/me/.claude-cc1"));
        assert!(identity_for_host(app, "m4p", "solo").await.is_none(), "寫了 host = local 就只給本機");
        assert!(identity_for_host(app, crate::config::LOCAL_HOST, "solo").await.is_some());
    }

    /// 本機：沒寫 host 的 config 身分仍然蓋過同名的 shell alias（現行行為），偵測登入狀態用的清單跟啟動用的是同一份。
    #[test]
    fn detection_and_start_share_one_precedence() {
        let c = |name: &str, host: Option<&str>| crate::config::IdentityCfg { name: name.into(), kind: "claude".into(), host: host.map(String::from), env: Default::default(), args: vec![] };
        let config = vec![c("cc1", None), c("far", Some("m4p"))];
        let shell = vec![c("cc1", None), c("cc2", None)];
        let local = merge_identities(&config, crate::config::LOCAL_HOST, Some(&shell));
        assert_eq!(local.iter().map(|(i, s)| (i.name.as_str(), *s)).collect::<Vec<_>>(), [("cc1", SOURCE_CONFIG), ("cc2", SOURCE_SHELL)], "明寫給 m4p 的不出現在本機");
        let remote = merge_identities(&config, "m4p", Some(&shell));
        assert_eq!(remote.iter().map(|(i, s)| (i.name.as_str(), *s)).collect::<Vec<_>>(), [("far", SOURCE_CONFIG), ("cc1", SOURCE_SHELL), ("cc2", SOURCE_SHELL)]);
    }

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

    /// Real `alias` output from both machines (2026-09-06); m4p keys cc1 to `~/.claude-ccompany`.
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
            "AM_ALIAS cc2='env CLAUDE_CONFIG_DIR=$HOME/.x claude'",
        ] {
            let got = parse_shell_identities(line);
            // Never an empty-env identity that would run on the default account.
            assert!(got.is_empty(), "should not have made an identity from `{line}`: {got:?}");
        }
        // No config dir at all *is* the default account, and still counts.
        let ids = parse_shell_identities("AM_ALIAS cc0='claude --dangerously-skip-permissions'");
        assert_eq!(ids.len(), 1);
        assert!(ids[0].env.is_empty());
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
            args: None,
        }
    }

    /// claude 只在本機問得到（ssh 讀不到 Keychain，會謊報 loggedIn:false）。
    #[test]
    fn claude_is_probed_locally_and_left_to_the_pane_when_remote() {
        assert_eq!(login_probe_args("claude", LOCAL_HOST), Some(CLAUDE_LOGIN_ARGS));
        assert_eq!(login_probe_args("claude", "m4p"), None);
        assert_eq!(login_probe_args("codex", "m4p"), Some(&["login", "status"][..]));
        // 腳本要用帶進來的 args，不是 kind 的預設（claude 的預設是 None，會整段被略過）。
        let mut it = probe("cc1", "claude", &[("CLAUDE_CONFIG_DIR", "/Users/m4p/.claude-ccompany")]);
        it.args = Some(CLAUDE_LOGIN_ARGS.to_vec());
        let sh = identity_probe_sh(&[it]);
        assert!(sh.contains("AM_IDENT_BEGIN %s\\n' 'cc1'"), "{sh}");
        assert!(sh.contains("'auth' 'status' '--json'"), "{sh}");
        assert!(sh.contains("CLAUDE_CONFIG_DIR='/Users/m4p/.claude-ccompany'"), "{sh}");
    }

    /// 這一輪問不到就沿用上一輪；真的登出（Some(false)）照樣覆蓋。
    #[test]
    fn a_pass_that_cannot_tell_keeps_the_last_known_answer() {
        let known = IdentityInfo {
            name: "cc1".into(),
            kind: "claude".into(),
            logged_in: Some(true),
            reason: Some("pane 探測".into()),
            account: Some("a@example.com".into()),
            plan: Some("team".into()),
            source: SOURCE_SHELL,
            config_dir: Some("/Users/m4p/.claude-ccompany".into()),
        };
        let mut unknown = IdentityInfo::unknown("cc1", "claude", SOURCE_SHELL, None);
        unknown.reason = Some("auth status 尚未取得結果".into());
        carry_over(&mut unknown, Some(&known));
        assert_eq!(unknown.logged_in, Some(true));
        assert_eq!(unknown.account.as_deref(), Some("a@example.com"));
        assert_eq!(unknown.plan.as_deref(), Some("team"));

        let mut logged_out = IdentityInfo::unknown("cc1", "claude", SOURCE_SHELL, None);
        logged_out.logged_in = Some(false);
        carry_over(&mut logged_out, Some(&known));
        assert_eq!(logged_out.logged_in, Some(false), "真的登出不能被舊答案蓋回去");
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
        assert!(gk0.contains(" '/opt/homebrew/bin/grok' 'models'"));
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
        let out = "AM_IDENT_BEGIN a\nAM_IDENT_END a\nAM_IDENT_BEGIN b\nzsh: command not found\nAM_IDENT_RC b 0\nAM_IDENT_END b\nAM_IDENT_BEGIN c\n{\"loggedIn\":true}\n";
        let m = parse_identity_probe(out, &kinds(&[("a", "claude"), ("b", "claude"), ("c", "claude")]));
        assert_eq!(m["a"].logged_in, None);
        assert_eq!(m["a"].reason.as_deref(), Some("auth status probe 沒有完成"));
        assert_eq!(m["b"].logged_in, None);
        assert_eq!(m["b"].reason.as_deref(), Some("auth status 輸出無法解析"));
        // `c` never closed, so it is not reported at all (the caller keeps its unknown row).
        assert!(!m.contains_key("c"));
    }

    #[test]
    fn failed_auth_status_is_unknown_with_a_reason() {
        let out = "AM_IDENT_BEGIN cx\npermission denied\nAM_IDENT_RC cx 127\nAM_IDENT_END cx\n";
        let m = parse_identity_probe(out, &kinds(&[("cx", "codex")]));
        assert_eq!(m["cx"].logged_in, None);
        assert_eq!(m["cx"].reason.as_deref(), Some("auth status 指令失敗（exit code 127）"));
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

    #[test]
    fn identity_login_commands_use_the_kind_and_identity_env() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".into(), "/tmp/cc one".into());
        env.insert("bad name".into(), "must not be emitted".into());
        assert_eq!(identity_login_command("claude", &env).as_deref(), Some("env CLAUDE_CONFIG_DIR='/tmp/cc one' claude /login"));
        assert_eq!(identity_login_command("codex", &BTreeMap::new()).as_deref(), Some("codex login"));
        assert_eq!(identity_login_command("grok", &BTreeMap::new()).as_deref(), Some("grok login"));
        assert!(identity_login_command("other", &BTreeMap::new()).is_none());
    }

    /// 遠端讀到「未登入」只有 claude 不可信（ssh 讀不到 Keychain）：codex／grok 照樣寫回快取，
    /// 否則在遠端按了登出，列上還是「已登入」、登出鈕也還在（review3 c5 L3）。
    #[test]
    fn a_remote_logged_out_answer_is_only_distrusted_for_claude() {
        for kind in ["codex", "grok"] {
            assert_eq!(login_answer_to_cache(false, kind, false), Some(false), "{kind}");
            assert_eq!(login_answer_to_cache(false, kind, true), Some(true), "{kind}");
        }
        assert_eq!(login_answer_to_cache(false, "claude", false), None, "遠端 claude 讀不到 Keychain");
        assert_eq!(login_answer_to_cache(false, "claude", true), Some(true));
        // 本機一律照實寫（包含 claude 的未登入）。
        assert_eq!(login_answer_to_cache(true, "claude", false), Some(false));
        assert_eq!(login_answer_to_cache(true, "codex", false), Some(false));
    }

    /// 登出的 pane 收尾：重驗說還登著就照實，問不出來（遠端 claude 一律問不出來）就記未登入。
    #[test]
    fn a_logout_pane_writes_logged_out_when_the_recheck_cannot_tell() {
        assert_eq!(logout_result(None), Some(false));
        assert_eq!(logout_result(Some(false)), Some(false));
        assert_eq!(logout_result(Some(true)), Some(true), "登出沒成功就照實，不要騙人說登出了");
    }

    /// 登出要帶跟登入一模一樣的環境前綴，否則按下 cc2 的登出會把 cc0 登掉。
    #[test]
    fn identity_logout_commands_carry_the_same_config_dir() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), "/tmp/cc one".to_string());
        assert_eq!(identity_logout_command("claude", &env).as_deref(), Some("env CLAUDE_CONFIG_DIR='/tmp/cc one' claude /logout"));
        assert_eq!(identity_logout_command("codex", &BTreeMap::new()).as_deref(), Some("codex logout"));
        assert_eq!(identity_logout_command("grok", &BTreeMap::new()).as_deref(), Some("grok logout"));
        assert!(identity_logout_command("other", &BTreeMap::new()).is_none());
        // 兩邊的前綴是同一段程式算出來的，不會有一邊漏掉。
        assert_eq!(
            identity_login_command("claude", &env).unwrap().rsplit_once(' ').unwrap().0,
            identity_logout_command("claude", &env).unwrap().rsplit_once(' ').unwrap().0
        );
    }
}
