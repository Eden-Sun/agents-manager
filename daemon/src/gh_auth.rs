//! Remote (and local) `gh` login for hosts that issue-list / team need.
//!
//! See `docs/goals/remote-gh-auth-2026-09-07.md`. Token bytes never go on argv, into
//! tracing, or into API JSON. Device-flow `device_code` stays in memory only.

use crate::config::LOCAL_HOST;
use crate::github::PATH_FIX;
use crate::hosts::sh_quote;
use crate::lifecycle::LcError;
use crate::state::App;
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::task::JoinHandle;

const GH_TIMEOUT: Duration = Duration::from_secs(40);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(60);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);

/// Public GitHub CLI OAuth app (`cli/cli` `internal/authflow/flow.go`). The secret is
/// documented there as safe to embed — we only use it so the resulting token is one `gh`
/// will accept via `--with-token`.
const GH_OAUTH_CLIENT_ID: &str = "178c6fc778ccc68e1d6a";
const GH_OAUTH_CLIENT_SECRET: &str = "34ddeff2b558a23d38fba8a6de74f086ede1cc0b";
const GH_OAUTH_SCOPES: &str = "repo read:org gist";

pub struct DeviceSession {
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_at: Instant,
    pub error: Option<String>,
    device_code: String,
    poll: Option<JoinHandle<()>>,
}

impl DeviceSession {
    fn pending_json(&self) -> Value {
        let expires_in = self.expires_at.saturating_duration_since(Instant::now()).as_secs() as i64;
        json!({
            "user_code": self.user_code,
            "verification_uri": self.verification_uri,
            "verification_uri_complete": self.verification_uri_complete,
            "expires_in": expires_in,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhAccount {
    pub login: String,
    pub active: bool,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhStatus {
    pub installed: bool,
    pub path: Option<String>,
    pub accounts: Vec<GhAccount>,
}

impl GhStatus {
    pub fn logged_in(&self) -> bool {
        self.accounts.iter().any(|a| a.active && a.ok)
    }

    pub fn account(&self) -> Option<&str> {
        self.accounts
            .iter()
            .find(|a| a.active)
            .map(|a| a.login.as_str())
            .or_else(|| self.accounts.first().map(|a| a.login.as_str()))
    }

    /// A valid account that is not active — `gh auth switch` can make gh usable without a new token.
    pub fn switchable(&self) -> Option<&str> {
        if self.logged_in() {
            return None;
        }
        self.accounts.iter().find(|a| a.ok && !a.active).map(|a| a.login.as_str())
    }

    fn to_json(&self, name: &str, mode: Option<&str>, pending: Option<Value>, error: Option<&str>) -> Value {
        json!({
            "name": name,
            "installed": self.installed,
            "path": self.path,
            "logged_in": self.logged_in(),
            "account": self.account(),
            "accounts": self.accounts.iter().map(|a| json!({
                "login": a.login,
                "active": a.active,
                "ok": a.ok,
            })).collect::<Vec<_>>(),
            "mode": mode,
            "pending": pending,
            "error": error,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMode {
    Auto,
    Copy,
    Device,
    Switch,
}

impl LoginMode {
    fn parse(s: Option<&str>) -> Result<Self, LcError> {
        match s.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("auto") {
            "auto" => Ok(Self::Auto),
            "copy" => Ok(Self::Copy),
            "device" => Ok(Self::Device),
            "switch" => Ok(Self::Switch),
            other => Err(LcError::Bad(format!("mode must be auto, copy, device or switch (got {other})"))),
        }
    }
}

// ---------------------------------------------------------------- redact / parse

/// Wipe `ghp_` / `gho_` / `github_pat_` (etc.) payloads so they cannot leak into logs or 502 bodies.
pub fn redact_secrets(s: &str) -> String {
    const PREFIXES: [&str; 6] = ["github_pat_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        if let Some(p) = PREFIXES.iter().copied().find(|p| rest.starts_with(p)) {
            out.push_str(p);
            out.push_str("***");
            i += p.chars().count();
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn up(e: impl std::fmt::Display) -> LcError {
    LcError::Upstream(redact_secrets(&e.to_string()))
}

fn lc_msg(e: &LcError) -> String {
    match e {
        LcError::Upstream(m) | LcError::Bad(m) => redact_secrets(m),
        LcError::NotFound(w) => format!("not found: {w}"),
        LcError::Conflict(v) | LcError::BadValue(v) => redact_secrets(&v.to_string()),
    }
}

/// Pull `AM_PATH` / `AM_JSON_BEGIN…AM_JSON_END` out of the status script.
pub fn parse_probe(out: &str) -> GhStatus {
    let mut path = None;
    let mut json_buf = String::new();
    let mut in_json = false;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("AM_PATH ") {
            let p = rest.trim();
            if !p.is_empty() {
                path = Some(p.to_string());
            }
            continue;
        }
        if line.trim() == "AM_JSON_BEGIN" {
            in_json = true;
            json_buf.clear();
            continue;
        }
        if line.trim() == "AM_JSON_END" {
            in_json = false;
            continue;
        }
        if in_json {
            json_buf.push_str(line);
            json_buf.push('\n');
        }
    }
    let accounts = parse_accounts(&json_buf);
    GhStatus { installed: path.is_some(), path, accounts }
}

pub fn parse_accounts(raw: &str) -> Vec<GhAccount> {
    let v: Value = serde_json::from_str(raw.trim()).unwrap_or(json!({}));
    let Some(hosts) = v.get("hosts").and_then(|h| h.as_object()) else {
        return Vec::new();
    };
    let mut accounts = Vec::new();
    // Prefer github.com; if the key is missing, take the first host.
    let list = hosts
        .get("github.com")
        .or_else(|| hosts.values().next())
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    for a in list {
        let login = a.get("login").and_then(|x| x.as_str()).unwrap_or("").trim();
        if login.is_empty() {
            continue;
        }
        let state = a.get("state").and_then(|x| x.as_str()).unwrap_or("");
        let ok = state.eq_ignore_ascii_case("success");
        let active = a.get("active").and_then(|x| x.as_bool()).unwrap_or(false);
        accounts.push(GhAccount { login: login.to_string(), active, ok });
    }
    accounts
}

// ---------------------------------------------------------------- run on host

async fn run_script(app: &Arc<App>, host: &str, script: &str, timeout: Duration) -> Result<String> {
    if host == LOCAL_HOST {
        let o = crate::hosts::sh_local(script, timeout).await?.ok_or_else(|| anyhow!("command timed out"))?;
        if !o.status.success() {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
            anyhow::bail!("{}", if err.is_empty() { out } else { err });
        }
        return Ok(String::from_utf8_lossy(&o.stdout).to_string());
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    conn.ssh_exec_path_timeout(script, timeout).await
}

async fn run_script_stdin(app: &Arc<App>, host: &str, script: &str, data: &[u8], timeout: Duration) -> Result<String> {
    if host == LOCAL_HOST {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn()?;
        if let Some(mut sin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            sin.write_all(data).await.ok();
            sin.shutdown().await.ok();
        }
        let o = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow!("command timed out"))??;
        if !o.status.success() {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            anyhow::bail!("{}", if err.is_empty() { "gh auth login failed".into() } else { err });
        }
        return Ok(String::from_utf8_lossy(&o.stdout).to_string());
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    conn.ssh_exec_path_stdin(script, data, timeout).await
}

fn status_script() -> String {
    format!(
        "{PATH_FIX}\
         p=$(command -v gh 2>/dev/null)\n\
         printf 'AM_PATH %s\\n' \"$p\"\n\
         printf 'AM_JSON_BEGIN\\n'\n\
         if [ -n \"$p\" ]; then\n\
           gh auth status --hostname github.com --json hosts 2>/dev/null || true\n\
         fi\n\
         printf '\\nAM_JSON_END\\n'\n\
         exit 0\n"
    )
}

async fn read_status(app: &Arc<App>, host: &str) -> Result<GhStatus, LcError> {
    let out = run_script(app, host, &status_script(), GH_TIMEOUT).await.map_err(up)?;
    Ok(parse_probe(&out))
}

async fn switch_user(app: &Arc<App>, host: &str, user: &str) -> Result<(), LcError> {
    let script = format!(
        "{PATH_FIX}gh auth switch --hostname github.com --user {}",
        sh_quote(user)
    );
    run_script(app, host, &script, GH_TIMEOUT).await.map_err(up)?;
    Ok(())
}

async fn logout_user(app: &Arc<App>, host: &str, user: &str) -> Result<(), LcError> {
    let script = format!(
        "{PATH_FIX}gh auth logout --hostname github.com --user {}",
        sh_quote(user)
    );
    run_script(app, host, &script, GH_TIMEOUT).await.map_err(up)?;
    Ok(())
}

/// Keyring `default` vs `hosts.yml` on macOS: `gh auth switch` refuses, and the dead
/// active token keeps winning. Dropping only an *invalid* active account lets the
/// remaining valid one become active. Never logs out a working account.
async fn drop_invalid_active(app: &Arc<App>, host: &str, st: &GhStatus) -> Result<bool, LcError> {
    let bad = st.accounts.iter().find(|a| a.active && !a.ok).map(|a| a.login.clone());
    let good = st.accounts.iter().find(|a| a.ok).map(|a| a.login.clone());
    let (Some(bad), Some(good)) = (bad, good) else {
        return Ok(false);
    };
    logout_user(app, host, &bad).await?;
    let _ = switch_user(app, host, &good).await;
    Ok(true)
}

/// Feed a token to `gh auth login --with-token`. `data` is the only place the token exists
/// on the wire; the script does not interpolate it.
async fn apply_token(app: &Arc<App>, host: &str, token: &str) -> Result<(), LcError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(LcError::Bad("empty github token".into()));
    }
    let script = format!(
        "{PATH_FIX}gh auth login --hostname github.com --git-protocol ssh --with-token --insecure-storage --skip-ssh-key"
    );
    let mut payload = token.as_bytes().to_vec();
    if !payload.ends_with(&[b'\n']) {
        payload.push(b'\n');
    }
    run_script_stdin(app, host, &script, &payload, LOGIN_TIMEOUT).await.map_err(up)?;
    Ok(())
}

/// Local `gh auth token`. Stdout is the secret; callers must not log it.
async fn local_token(app: &Arc<App>) -> Result<String, LcError> {
    let script = format!("{PATH_FIX}gh auth token --hostname github.com");
    let out = run_script(app, LOCAL_HOST, &script, TOKEN_TIMEOUT).await.map_err(|_| {
        LcError::Conflict(json!({
            "error": "conflict",
            "reason": "local_gh_not_logged_in",
            "message": "本機 gh 尚未登入，無法轉發 token；改用裝置碼",
        }))
    })?;
    let token = out.trim().to_string();
    if token.is_empty() || token.contains(' ') || token.contains('\n') {
        return Err(LcError::Conflict(json!({
            "error": "conflict",
            "reason": "local_gh_not_logged_in",
            "message": "本機 gh 尚未登入，無法轉發 token；改用裝置碼",
        })));
    }
    Ok(token)
}

async fn overlay_json(app: &Arc<App>, host: &str, st: &GhStatus, mode: Option<&str>) -> Value {
    let (pending, error) = {
        let g = app.gh_device.lock().await;
        match g.get(host) {
            Some(s) if s.error.is_some() => (None, s.error.clone()),
            Some(s) if Instant::now() < s.expires_at && !st.logged_in() => (Some(s.pending_json()), None),
            _ => (None, None),
        }
    };
    st.to_json(host, mode, pending, error.as_deref())
}

pub async fn status(app: &Arc<App>, host: &str) -> Result<Value, LcError> {
    ensure_host(app, host).await?;
    let st = read_status(app, host).await?;
    if st.logged_in() {
        abort_device(app, host).await;
    }
    Ok(overlay_json(app, host, &st, None).await)
}

async fn ensure_host(app: &Arc<App>, host: &str) -> Result<(), LcError> {
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    Ok(())
}

async fn abort_device(app: &Arc<App>, host: &str) {
    let mut g = app.gh_device.lock().await;
    if let Some(mut s) = g.remove(host) {
        if let Some(h) = s.poll.take() {
            h.abort();
        }
    }
}

pub async fn cancel(app: &Arc<App>, host: &str) -> Result<Value, LcError> {
    ensure_host(app, host).await?;
    abort_device(app, host).await;
    let st = read_status(app, host).await?;
    Ok(st.to_json(host, Some("cancel"), None, None))
}

pub async fn login(app: &Arc<App>, host: &str, mode: Option<&str>, user: Option<&str>) -> Result<Value, LcError> {
    ensure_host(app, host).await?;
    let mode = LoginMode::parse(mode)?;
    match mode {
        LoginMode::Auto => login_auto(app, host).await,
        LoginMode::Copy => login_copy(app, host).await,
        LoginMode::Device => login_device(app, host).await,
        LoginMode::Switch => login_switch(app, host, user).await,
    }
}

async fn login_auto(app: &Arc<App>, host: &str) -> Result<Value, LcError> {
    let st = read_status(app, host).await?;
    if !st.installed {
        return Err(LcError::Upstream("gh 未安裝（brew install gh）".into()));
    }
    if st.logged_in() {
        abort_device(app, host).await;
        return Ok(st.to_json(host, Some("auto"), None, None));
    }
    if let Some(u) = st.switchable().map(str::to_string) {
        // `gh auth switch` fails when the active token is in a different store
        // (macOS keyring "default") than the valid one (`hosts.yml`). Fall through
        // to dropping the dead active account, then to copy.
        if switch_user(app, host, &u).await.is_ok() {
            let st = read_status(app, host).await?;
            if st.logged_in() {
                abort_device(app, host).await;
                return Ok(st.to_json(host, Some("switch"), None, None));
            }
        }
    }
    if drop_invalid_active(app, host, &st).await? {
        let st = read_status(app, host).await?;
        if st.logged_in() {
            abort_device(app, host).await;
            return Ok(st.to_json(host, Some("switch"), None, None));
        }
    }
    if host != LOCAL_HOST {
        if let Ok(local) = read_status(app, LOCAL_HOST).await {
            if local.logged_in() {
                return login_copy(app, host).await;
            }
        }
    }
    login_device(app, host).await
}

async fn login_switch(app: &Arc<App>, host: &str, user: Option<&str>) -> Result<Value, LcError> {
    let st = read_status(app, host).await?;
    let user = match user.map(str::trim).filter(|s| !s.is_empty()) {
        Some(u) => u.to_string(),
        None => st
            .switchable()
            .map(str::to_string)
            .ok_or_else(|| LcError::Bad("沒有可切換的有效 gh 帳號".into()))?,
    };
    switch_user(app, host, &user).await?;
    let st = read_status(app, host).await?;
    if st.logged_in() {
        abort_device(app, host).await;
    }
    Ok(st.to_json(host, Some("switch"), None, None))
}

async fn login_copy(app: &Arc<App>, host: &str) -> Result<Value, LcError> {
    if host == LOCAL_HOST {
        return Err(LcError::Bad("copy 只適用遠端主機（本機請用裝置碼）".into()));
    }
    let local = read_status(app, LOCAL_HOST).await?;
    if !local.logged_in() {
        return Err(LcError::Conflict(json!({
            "error": "conflict",
            "reason": "local_gh_not_logged_in",
            "message": "本機 gh 尚未登入，無法轉發 token；改用裝置碼",
        })));
    }
    let token = local_token(app).await?;
    let apply = apply_token(app, host, &token).await;
    drop(token);
    apply?;
    if let Some(acct) = local.account().map(str::to_string) {
        let _ = switch_user(app, host, &acct).await;
    }
    let mut st = read_status(app, host).await?;
    if !st.logged_in() {
        let _ = drop_invalid_active(app, host, &st).await;
        st = read_status(app, host).await?;
    }
    if st.logged_in() {
        abort_device(app, host).await;
    }
    if !st.logged_in() {
        return Err(LcError::Upstream("已把本機 token 寫入遠端，但 gh 作用中帳號仍無法使用".into()));
    }
    Ok(st.to_json(host, Some("copy"), None, None))
}

async fn login_device(app: &Arc<App>, host: &str) -> Result<Value, LcError> {
    let st = read_status(app, host).await?;
    if !st.installed {
        return Err(LcError::Upstream("gh 未安裝（brew install gh）".into()));
    }
    abort_device(app, host).await;
    let started = start_device().await.map_err(up)?;
    let expires_at = Instant::now() + Duration::from_secs(started.expires_in.max(30));
    let session = DeviceSession {
        user_code: started.user_code.clone(),
        verification_uri: started.verification_uri.clone(),
        verification_uri_complete: started.verification_uri_complete.clone(),
        expires_at,
        error: None,
        device_code: started.device_code.clone(),
        poll: None,
    };
    let pending = session.pending_json();
    {
        let mut g = app.gh_device.lock().await;
        g.insert(host.to_string(), session);
    }
    let poll_app = Arc::clone(app);
    let poll_host = host.to_string();
    let device_code = started.device_code;
    let interval = Duration::from_secs(started.interval.max(5));
    let handle = tokio::spawn(async move {
        poll_device(poll_app, poll_host, device_code, expires_at, interval).await;
    });
    if let Some(s) = app.gh_device.lock().await.get_mut(host) {
        s.poll = Some(handle);
    }
    Ok(st.to_json(host, Some("device"), Some(pending), None))
}

struct DeviceStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    verification_uri_complete: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("agents-manager")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| anyhow!("http client: {e}"))
}

async fn start_device() -> Result<DeviceStart> {
    let client = http_client()?;
    let resp = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("client_id={GH_OAUTH_CLIENT_ID}&scope={}", GH_OAUTH_SCOPES.replace(' ', "%20")))
        .send()
        .await
        .map_err(|e| anyhow!("github device code: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let parsed: DeviceCodeResponse = serde_json::from_str(&text).unwrap_or(DeviceCodeResponse {
        device_code: None,
        user_code: None,
        verification_uri: None,
        verification_uri_complete: None,
        expires_in: None,
        interval: None,
        error: None,
        error_description: None,
    });
    if let Some(err) = parsed.error {
        anyhow::bail!("{}", parsed.error_description.unwrap_or(err));
    }
    let device_code = parsed.device_code.filter(|s| !s.is_empty()).ok_or_else(|| {
        anyhow!("github device code http {}: missing device_code", status.as_u16())
    })?;
    let user_code = parsed.user_code.filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("github device code: missing user_code"))?;
    let verification_uri = parsed
        .verification_uri
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://github.com/login/device".into());
    Ok(DeviceStart {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete: parsed.verification_uri_complete.filter(|s| !s.is_empty()),
        expires_in: parsed.expires_in.unwrap_or(900),
        interval: parsed.interval.unwrap_or(5),
    })
}

enum TokenPoll {
    Pending,
    SlowDown,
    Denied,
    Expired,
    Token(String),
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn exchange_token(device_code: &str) -> Result<TokenPoll> {
    let client = http_client()?;
    let body = format!(
        "client_id={GH_OAUTH_CLIENT_ID}&client_secret={GH_OAUTH_CLIENT_SECRET}&device_code={}&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
        url_encode(device_code)
    );
    let resp = client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| anyhow!("github access_token: {e}"))?;
    let text = resp.text().await.unwrap_or_default();
    let parsed: TokenResponse = serde_json::from_str(&text).unwrap_or(TokenResponse {
        access_token: None,
        error: None,
        error_description: None,
    });
    if let Some(token) = parsed.access_token.filter(|s| !s.is_empty()) {
        return Ok(TokenPoll::Token(token));
    }
    match parsed.error.as_deref() {
        Some("authorization_pending") => Ok(TokenPoll::Pending),
        Some("slow_down") => Ok(TokenPoll::SlowDown),
        Some("access_denied") => Ok(TokenPoll::Denied),
        Some("expired_token") => Ok(TokenPoll::Expired),
        Some(other) => anyhow::bail!("{}", parsed.error_description.unwrap_or_else(|| other.to_string())),
        None => anyhow::bail!("github access_token: empty response"),
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn set_device_error(app: &Arc<App>, host: &str, device_code: &str, msg: String) {
    let mut g = app.gh_device.lock().await;
    if let Some(s) = g.get_mut(host) {
        if s.device_code == device_code {
            s.error = Some(redact_secrets(&msg));
            s.poll = None;
        }
    }
}

async fn poll_device(app: Arc<App>, host: String, device_code: String, expires_at: Instant, mut interval: Duration) {
    loop {
        if Instant::now() >= expires_at {
            set_device_error(&app, &host, &device_code, "裝置碼已過期，請再點一次登入".into()).await;
            return;
        }
        tokio::time::sleep(interval).await;
        {
            let g = app.gh_device.lock().await;
            match g.get(&host) {
                Some(s) if s.device_code == device_code && s.error.is_none() => {}
                _ => return,
            }
        }
        match exchange_token(&device_code).await {
            Ok(TokenPoll::Pending) => {}
            Ok(TokenPoll::SlowDown) => interval += Duration::from_secs(5),
            Ok(TokenPoll::Denied) => {
                set_device_error(&app, &host, &device_code, "GitHub 拒絕授權".into()).await;
                return;
            }
            Ok(TokenPoll::Expired) => {
                set_device_error(&app, &host, &device_code, "裝置碼已過期，請再點一次登入".into()).await;
                return;
            }
            Ok(TokenPoll::Token(token)) => {
                let apply = apply_token(&app, &host, &token).await;
                drop(token);
                match apply {
                    Ok(()) => {
                        // Drop the session without aborting this task — we *are* the poller.
                        let mut g = app.gh_device.lock().await;
                        if g.get(&host).is_some_and(|s| s.device_code == device_code) {
                            g.remove(&host);
                        }
                    }
                    Err(e) => {
                        set_device_error(&app, &host, &device_code, lc_msg(&e)).await;
                    }
                }
                return;
            }
            Err(e) => {
                set_device_error(&app, &host, &device_code, e.to_string()).await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_github_tokens() {
        let s = "boom gho_abcDEF123 and ghp_zzz and github_pat_11AAAA extra";
        let r = redact_secrets(s);
        assert!(!r.contains("abcDEF123"), "{r}");
        assert!(!r.contains("zzz"), "{r}");
        assert!(!r.contains("11AAAA"), "{r}");
        assert!(r.contains("gho_***"), "{r}");
        assert!(r.contains("ghp_***"), "{r}");
        assert!(r.contains("github_pat_***"), "{r}");
        assert!(r.contains(" extra"), "{r}");
    }

    #[test]
    fn parse_m4p_status_invalid_active_plus_valid_inactive() {
        let raw = r#"{
          "hosts": {
            "github.com": [
              {
                "state": "error",
                "error": "HTTP 401: Bad credentials",
                "active": true,
                "host": "github.com",
                "login": "eddysun-alt"
              },
              {
                "state": "success",
                "active": false,
                "host": "github.com",
                "login": "Eden-Sun"
              }
            ]
          }
        }"#;
        let probe = format!("AM_PATH /opt/homebrew/bin/gh\nAM_JSON_BEGIN\n{raw}\nAM_JSON_END\n");
        let st = parse_probe(&probe);
        assert!(st.installed);
        assert_eq!(st.path.as_deref(), Some("/opt/homebrew/bin/gh"));
        assert!(!st.logged_in(), "invalid active token is not logged in");
        assert_eq!(st.account(), Some("eddysun-alt"));
        assert_eq!(st.switchable(), Some("Eden-Sun"));
        assert_eq!(st.accounts.len(), 2);
        assert!(!st.accounts[0].ok && st.accounts[0].active);
        assert!(st.accounts[1].ok && !st.accounts[1].active);
    }

    #[test]
    fn parse_local_success() {
        let raw = r#"{"hosts":{"github.com":[{"state":"success","active":true,"login":"Eden-Sun"}]}}"#;
        let st = parse_probe(&format!("AM_PATH /opt/homebrew/bin/gh\nAM_JSON_BEGIN\n{raw}\nAM_JSON_END\n"));
        assert!(st.logged_in());
        assert_eq!(st.account(), Some("Eden-Sun"));
        assert!(st.switchable().is_none());
    }

    #[test]
    fn parse_missing_gh() {
        let st = parse_probe("AM_PATH \nAM_JSON_BEGIN\n\nAM_JSON_END\n");
        assert!(!st.installed);
        assert!(!st.logged_in());
        assert!(st.accounts.is_empty());
    }

    #[test]
    fn login_mode_parse() {
        assert_eq!(LoginMode::parse(None).unwrap(), LoginMode::Auto);
        assert_eq!(LoginMode::parse(Some(" copy ")).unwrap(), LoginMode::Copy);
        assert!(LoginMode::parse(Some("web")).is_err());
    }

    #[test]
    fn url_encode_device_code() {
        assert_eq!(url_encode("abc-XYZ_~."), "abc-XYZ_~.");
        assert_eq!(url_encode("a b"), "a%20b");
    }
}
