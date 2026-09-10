//! Claude Code quota + login, read from a throwaway herdr pane (complements the statusLine push).
//!
//! StatusLine only updates while a bot is running and chatting. The background probe opens one
//! pane in the dedicated `am-quota` herdr session (the daemon's own named session on a remote
//! host) and runs **one** shell line there:
//!
//! ```text
//! claude auth status --json      → loggedIn / email / subscriptionType
//! claude -p "/usage"             → the three plan lines
//! ```
//!
//! It is a pane rather than ssh on purpose: a non-login ssh session cannot read the macOS
//! Keychain, where some accounts keep their OAuth credentials, so it answers `loggedIn: false`
//! for accounts that work fine here. A pane runs under the user's own login session and sees
//! what the user sees — local and remote alike.
//!
//! `claude -p "/usage"` prints plain text (no TUI dialog to drive, no first-run trust prompt):
//!
//! ```text
//! Current session: 47% used · resets Sep 7 at 9:59pm (Asia/Taipei)
//! Current week (all models): 15% used · resets Sep 14 at 11:59am (Asia/Taipei)
//! Current week (Fable): 23% used · resets Sep 14 at 11:59am (Asia/Taipei)
//! ```
//!
//! `Current session` → `five_hour`；`Current week (all models)` → `seven_day`；Max 方案才有的
//! `Current week (Fable)` → `fable`（一樣是週窗，只算 Fable 那一份）。其餘 model-specific 的
//! 週列（Sonnet / Opus）仍舊忽略，額度條上只多 Fable 這一條。沒登入時 `/usage` 只印一段
//! 成本摘要、沒有那三條，於是解析回 `None`，而同一次探測的 `auth status` 已經說了為什麼。
//!
//! [`parse_claude_usage`] 仍認得舊的 TUI 對話框版面（標題／長條／`Resets` 各一行），因為
//! statusLine 之外還有人手動貼那個畫面進來測。
//!
//! Each configured Claude identity is probed separately (empty-env / `cc0` shares the bare
//! `claude` key with the default account).

use crate::config::{expand_home, LOCAL_HOST};
use crate::herdr::HerdrClient;
use crate::quota::{Quota, Window};
use crate::state::App;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// One pane + two `claude` invocations is cheap now, but still not free; once a minute is
/// plenty for the strip.
pub const CLAUDE_POLL: Duration = Duration::from_secs(60);

/// How long the whole `auth status` + `-p "/usage"` line may take in the pane. `-p` starts a
/// real session, so a cold start on a busy host is measured in seconds, not milliseconds.
const PROBE_TIMEOUT: Duration = Duration::from_secs(40);

const PROBE_SESSION: &str = "am-quota";
const PROBE_LABEL_PREFIX: &str = "am-quota-claude";

// ------------------------------------------------------------------ parsing

fn clean(line: &str) -> String {
    let s: String = line
        .chars()
        .map(|c| match c {
            // Keep bar glyphs for the fallback pct parser; drop frame / chrome.
            '│' | '┌' | '┐' | '└' | '┘' | '─' | '├' | '┤' | '┬' | '┴' | '┼' | '▎' | '▔' | '↓' => ' ',
            c => c,
        })
        .collect();
    s.trim().to_string()
}

/// `78% used` or a bar row ending in `78%` / `78% used`.
fn parse_used_pct(line: &str) -> Option<f64> {
    let low = line.to_ascii_lowercase();
    // Prefer the explicit "N% used" form from the Usage tab.
    if let Some(at) = low.find("% used") {
        let head = &line[..at];
        let num: String = head.chars().rev().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if !num.is_empty() {
            return num.chars().rev().collect::<String>().parse().ok();
        }
    }
    // Bar row fallback (same shape as grok): glyphs then a percentage.
    if line.contains('█') || line.contains('░') || line.contains('▌') {
        let (head, _) = line.split_once('%')?;
        let num: String = head.chars().rev().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if !num.is_empty() {
            return num.chars().rev().collect::<String>().parse().ok();
        }
    }
    None
}

fn month_num(tok: &str) -> Option<u32> {
    const M: [&str; 12] =
        ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let t = tok.trim_matches(|c: char| !c.is_alphabetic()).to_ascii_lowercase();
    if t.len() < 3 {
        return None;
    }
    M.iter().position(|m| t.starts_with(m)).map(|i| i as u32 + 1)
}

fn parse_ampm_time(tok: &str) -> Option<(u32, u32)> {
    let t = tok.trim().to_ascii_lowercase();
    let (hm, pm) = if let Some(h) = t.strip_suffix("pm") {
        (h, true)
    } else if let Some(h) = t.strip_suffix("am") {
        (h, false)
    } else {
        return None;
    };
    let (h, m) = if let Some((h, m)) = hm.split_once(':') {
        (h.parse::<u32>().ok()?, m.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?)
    } else {
        (hm.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?, 0)
    };
    let h = match (h, pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, true) => h + 12,
        (h, false) => h,
    };
    if h < 24 && m < 60 {
        Some((h, m))
    } else {
        None
    }
}

/// `Resets 1:20pm (Asia/Taipei)` / `Resets Sep 11 at 2pm (Asia/Taipei)` → RFC3339 UTC.
pub fn parse_claude_reset(line: &str, now: DateTime<Local>) -> Option<String> {
    let rest = line
        .trim()
        .strip_prefix("Resets")
        .or_else(|| line.trim().strip_prefix("resets"))?
        .trim()
        .trim_start_matches(':')
        .trim();
    // Drop the timezone parenthetical — we interpret wall time in *local* (daemon host).
    let rest = rest.split('(').next()?.trim();

    let mut month = None;
    let mut day = None;
    let mut hm = None;
    for tok in rest.split(|c: char| c.is_whitespace() || c == ',').filter(|t| !t.is_empty()) {
        if tok.eq_ignore_ascii_case("at") {
            continue;
        }
        if hm.is_none() {
            if let Some(t) = parse_ampm_time(tok) {
                hm = Some(t);
                continue;
            }
        }
        if month.is_none() {
            if let Some(m) = month_num(tok) {
                month = Some(m);
                continue;
            }
        }
        let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && digits.len() <= 2 && day.is_none() {
            day = digits.parse().ok();
        }
    }
    let (h, m) = hm?;
    let dt = match (month, day) {
        (Some(month), Some(day)) => {
            let build = |y: i32| -> Option<DateTime<Local>> {
                let d = NaiveDate::from_ymd_opt(y, month, day)?.and_hms_opt(h, m, 0)?;
                Local.from_local_datetime(&d).earliest()
            };
            let this = build(now.year())?;
            if this < now - chrono::Duration::days(1) {
                build(now.year() + 1)?
            } else {
                this
            }
        }
        _ => {
            // Time-only: today, or tomorrow if that time already passed.
            let today = now.date_naive().and_hms_opt(h, m, 0)?;
            let mut dt = Local.from_local_datetime(&today).earliest()?;
            if dt <= now {
                dt += chrono::Duration::days(1);
            }
            dt
        }
    };
    Some(dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// 一行的標題部分屬於哪個桶子。純文字版是 `Current week (Fable): 23% used · …`，對話框版是
/// 單獨一行的 `Current week (Fable)`——切在第一個 `:` 之前兩種都認得（時間裡的 `9:59pm` 在後面，
/// 切不到）。
fn header_kind(line: &str) -> Option<&'static str> {
    let head = line.split(':').next().unwrap_or(line).trim().to_ascii_lowercase();
    if head == "current session" || head.starts_with("current session ") {
        return Some("five");
    }
    if head.starts_with("current week") {
        if head.contains("fable") {
            return Some("fable");
        }
        if head.contains("all models") || head == "current week" {
            return Some("seven");
        }
    }
    None
}

/// `… · resets Sep 7 at 9:59pm (Asia/Taipei)` → 從 `resets` 起的那一段（大小寫不敏感）。
fn resets_tail(line: &str) -> Option<&str> {
    let at = line.to_ascii_lowercase().find("resets")?;
    Some(&line[at..])
}

/// A `/usage` answer → Quota, or `None` when the plan lines are not there (logged out, or the
/// dialog has not drawn them yet).
pub fn parse_claude_usage(screen: &str, now: DateTime<Local>, account: Option<&str>) -> Option<Quota> {
    let lines: Vec<String> = screen.lines().map(clean).filter(|l| !l.is_empty()).collect();
    let mut five: Option<Window> = None;
    let mut seven: Option<Window> = None;
    let mut fable: Option<Window> = None;
    let mut i = 0;
    while i < lines.len() {
        let Some(kind) = header_kind(&lines[i]) else {
            i += 1;
            continue;
        };
        // 純文字（`claude -p "/usage"`）：百分比與重置時間都在同一行。
        if let Some(used_pct) = parse_used_pct(&lines[i]) {
            let resets_at = resets_tail(&lines[i]).and_then(|t| parse_claude_reset(t, now));
            let w = Window { used_pct, resets_at };
            match kind {
                "five" => five = Some(w),
                "seven" => seven = Some(w),
                _ => fable = Some(w),
            }
            i += 1;
            continue;
        }
        // 舊的 TUI 對話框：標題、長條、`Resets` 各一行。
        let mut pct = None;
        let mut resets = None;
        let mut j = i + 1;
        while j < lines.len() && j < i + 6 {
            if header_kind(&lines[j]).is_some() {
                break;
            }
            if pct.is_none() {
                pct = parse_used_pct(&lines[j]);
            }
            if resets.is_none() && lines[j].to_ascii_lowercase().starts_with("resets") {
                resets = parse_claude_reset(&lines[j], now);
            }
            j += 1;
        }
        if let Some(used_pct) = pct {
            let w = Window { used_pct, resets_at: resets };
            match kind {
                "five" => five = Some(w),
                "seven" => seven = Some(w),
                _ => fable = Some(w),
            }
        }
        i = j;
    }
    if five.is_none() && seven.is_none() {
        return None;
    }
    Some(Quota {
        five_hour: five,
        seven_day: seven,
        fable,
        reset_credits: None,
        plan: None,
        updated_at: crate::db::now(),
        source: "claude-usage".into(),
        account: account.map(String::from),
        host: LOCAL_HOST.into(),
    })
}

// ------------------------------------------------------------------ probe

/// Where the probe runs for `host`.
///
/// Local keeps its dedicated `am-quota` session (SPEC §12.6) so nothing shows up in the
/// user's own herdr. A remote host has exactly **one** forwarded socket — the daemon's own
/// named session — so the probe borrows that one: it is the daemon's session, not the user's,
/// and the throwaway workspace (label `am-quota-claude*`, agent `amquota…`) is not in the DB,
/// so reconcile never mistakes it for a bot.
async fn client_for(app: &Arc<App>, host: &str) -> Result<HerdrClient> {
    if host == LOCAL_HOST {
        return probe_client().await;
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    if !conn.is_connected() {
        return Err(anyhow!("host `{host}` is not connected"));
    }
    app.herdr_for(host).await.ok_or_else(|| anyhow!("no herdr client for `{host}`"))
}

/// `$HOME` on `host` (the local home for `local`), used for the probe cwd and `~` expansion.
async fn host_home(app: &Arc<App>, host: &str) -> String {
    if let Some(conn) = app.hosts.get(host).await {
        if let Ok(h) = conn.home().await {
            return h;
        }
    }
    dirs::home_dir().map(|p| p.display().to_string()).unwrap_or_else(|| "/tmp".into())
}

async fn probe_client() -> Result<HerdrClient> {
    let client = HerdrClient::new(HerdrClient::session_socket(PROBE_SESSION));
    if client.ping().await.is_ok() {
        return Ok(client);
    }
    std::process::Command::new("/bin/sh")
        .arg("-lc")
        .arg(format!("exec herdr --session {PROBE_SESSION} server"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("could not start the `{PROBE_SESSION}` herdr session: {e}"))?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if client.ping().await.is_ok() {
            return Ok(client);
        }
    }
    Err(anyhow!("the `{PROBE_SESSION}` herdr session did not come up"))
}

pub async fn sweep_stale(app: &Arc<App>) {
    let mut clients = Vec::new();
    if let Ok(c) = probe_client().await {
        clients.push(c);
    }
    // Local session (older builds probed there) plus every connected remote session, where
    // the probe lives by design.
    for host in crate::quota::pollable_hosts(app).await {
        if let Some(c) = app.herdr_for(&host).await {
            clients.push(c);
        }
    }
    for c in clients {
        let Ok(list) = c.workspace_list().await else { continue };
        for ws in list.iter().filter(|w| {
            w.label
                .as_deref()
                .map(|l| l == PROBE_LABEL_PREFIX || l.starts_with(&format!("{PROBE_LABEL_PREFIX}-")))
                .unwrap_or(false)
        }) {
            match c.workspace_close(&ws.workspace_id).await {
                Ok(()) => tracing::info!(workspace = %ws.workspace_id, "closed a stale claude quota probe"),
                Err(e) => tracing::warn!(workspace = %ws.workspace_id, error = %e, "stale claude probe not closed"),
            }
        }
    }
}

struct Probe {
    client: HerdrClient,
    workspace_id: String,
    closed: bool,
}

impl Probe {
    async fn close(mut self) {
        self.closed = true;
        if let Err(e) = self.client.workspace_close(&self.workspace_id).await {
            tracing::warn!(workspace = %self.workspace_id, error = %e, "claude quota probe workspace not closed");
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let (client, ws) = (self.client.clone(), self.workspace_id.clone());
        tokio::spawn(async move {
            if let Err(e) = client.workspace_close(&ws).await {
                tracing::warn!(workspace = %ws, error = %e, "claude quota probe workspace not closed");
            }
        });
    }
}

/// 標記名稱拆成 `printf` 的參數，shell 回顯的指令本身才不會也長得像標記
/// （回顯的是 `AM_AUTH_%s`，輸出的才是 `AM_AUTH_BEGIN`）。
const AUTH_BEGIN: &str = "AM_AUTH_BEGIN";
const AUTH_END: &str = "AM_AUTH_END";
const USAGE_DONE: &str = "AM_USAGE_DONE=";

/// 一次 pane 探測拿到的東西：登入狀態與（有登入才有的）額度。
#[derive(Debug, Default)]
pub(crate) struct ProbeOutcome {
    /// `auth status --json` 的 `loggedIn`；讀不懂就是 `None`（「判不出來」，不是「沒登入」）。
    pub logged_in: Option<bool>,
    pub email: Option<String>,
    /// `subscriptionType`（`max`、`pro`…）。
    pub plan: Option<String>,
    pub quota: Option<Quota>,
}

/// The one shell line the pane runs. `env` (an identity's `CLAUDE_CONFIG_DIR`) is spelled out
/// in front of each call as well as being the pane's env, so the command is self-contained if
/// anyone reads it off the screen.
fn probe_command(bin: &str, env: &BTreeMap<String, String>) -> String {
    let mut pfx = String::new();
    for (k, v) in env.iter().filter(|(k, _)| crate::tools::valid_env_name(k)) {
        pfx.push_str(&format!("{k}={} ", crate::hosts::sh_quote(v)));
    }
    let b = crate::hosts::sh_quote(bin);
    let auth = crate::tools::CLAUDE_LOGIN_ARGS.join(" ");
    format!(
        "printf '\\nAM_AUTH_%s\\n' BEGIN; {pfx}{b} {auth} </dev/null 2>&1; \
         printf '\\nAM_AUTH_%s\\n' END; {pfx}{b} -p '/usage' </dev/null 2>&1; rc=$?; \
         printf '\\nAM_USAGE_%s=%s\\n' DONE \"$rc\""
    )
}

/// `(auth json, /usage text)` once the trailing `AM_USAGE_DONE=` marker is on screen.
///
/// Everything is searched from the **end**: a pane that swallowed the first line and had it
/// retyped has two runs in its scrollback, and only the last one finished.
fn split_probe_output(out: &str) -> Option<(String, String)> {
    let done = out.rfind(USAGE_DONE)?;
    let head = &out[..done];
    let e = head.rfind(AUTH_END)?;
    let b = head[..e].rfind(AUTH_BEGIN)? + AUTH_BEGIN.len();
    Some((head[b..e].to_string(), head[e + AUTH_END.len()..].to_string()))
}

async fn send_line(client: &HerdrClient, pane_id: &str, line: &str) -> Result<()> {
    client.pane_send_text(pane_id, line).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    client.pane_send_keys(pane_id, &["Enter"]).await
}

/// One account probe on `host`. `env` is the pane env (e.g. `CLAUDE_CONFIG_DIR` for cc1).
/// `base_key` is the host-less quota key (`claude` / `claude:cc1`).
/// `Ok(None)` means claude is not installed there. Caller must hold
/// [`crate::quota::probe_lock`] for that host.
async fn refresh_claude_account(
    app: &Arc<App>,
    host: &str,
    base_key: &str,
    account: Option<&str>,
    env: BTreeMap<String, String>,
) -> Result<Option<ProbeOutcome>> {
    if !app.tools.lock().await.contains_key(host) {
        crate::tools::detect(app, host).await?;
    }
    let Some(bin) = crate::tools::cached_path(app, host, "claude").await else {
        return Ok(None);
    };

    let client = client_for(app, host).await?;
    // Prefer a cwd Claude already trusts; `-p` does not ask about workspace trust, but a
    // sane cwd still keeps the session's project files out of `/`.
    let home = host_home(app, host).await;
    let cwd = if host == LOCAL_HOST {
        std::env::current_dir().ok().map(|p| p.display().to_string()).unwrap_or(home)
    } else {
        home
    };
    let label = match account {
        Some(a) if !a.is_empty() => format!("{PROBE_LABEL_PREFIX}-{a}"),
        _ => PROBE_LABEL_PREFIX.to_string(),
    };
    let env_json = Value::Object(env.iter().map(|(k, v)| (k.clone(), json!(v))).collect());
    let (ws, pane) = client.workspace_create(&cwd, &label, env_json).await?;
    let probe = Probe { client: client.clone(), workspace_id: ws.workspace_id.clone(), closed: false };
    let pane_id = pane.pane_id.clone();
    let cmd = probe_command(&bin, &env);

    // workspace.create returns before the interactive shell is always ready to take input, and
    // a line typed too early is simply lost — so if no marker shows up at all, type it again.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let mut sends = 1u32;
    let mut sent_at = tokio::time::Instant::now();
    send_line(&client, &pane_id, &cmd).await?;

    let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
    let mut last = String::new();
    let mut got = None;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(700)).await;
        last = client.pane_read(&pane_id, "recent_unwrapped", 400).await.map(|r| r.text).unwrap_or_default();
        if let Some(v) = split_probe_output(&last) {
            got = Some(v);
            break;
        }
        if sends < 3 && !last.contains(AUTH_BEGIN) && sent_at.elapsed() > Duration::from_secs(5) {
            tracing::debug!(host, sends, "claude probe pane swallowed the command; retyping");
            let _ = send_line(&client, &pane_id, &cmd).await;
            sends += 1;
            sent_at = tokio::time::Instant::now();
        }
    }
    probe.close().await;

    let Some((auth, usage)) = got else {
        return Err(anyhow!("claude probe on {host} did not finish within {PROBE_TIMEOUT:?}; screen:\n{}", last.trim()));
    };
    let (logged_in, email, plan) = crate::tools::read_login_answer("claude", &auth);
    let mut out = ProbeOutcome { logged_in, email, plan: plan.clone(), quota: None };
    if let Some(mut q) = parse_claude_usage(&usage, Local::now(), account) {
        q.plan = plan;
        crate::quota::set(app, host, base_key, q.clone()).await;
        out.quota = Some(q);
    } else {
        tracing::debug!(host, account = ?account, logged_in = ?out.logged_in, usage = %usage.trim(), "claude `/usage` reported no plan lines");
    }
    Ok(Some(out))
}

/// Was `updated_at` written within `window`? Unparsable timestamps count as stale.
fn fresher_than(updated_at: &str, window: Duration) -> bool {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(updated_at) else { return false };
    match chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc)).to_std() {
        Ok(age) => age < window,
        // Negative age (clock skew) — treat as fresh rather than probing on every cycle.
        Err(_) => true,
    }
}

// ------------------------------------------------ which identities are worth probing

/// How long a failed probe parks an identity when the login state is merely unknown — long
/// enough that a genuinely broken account is not retried every cycle, short enough that a
/// transient failure (herdr busy, TUI slow) heals on its own.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);
/// Same, for an identity the CLI itself reported as logged out in the probe pane: nothing but
/// a login will change that, so back off much harder.
const RETRY_WHEN_LOGGED_OUT: Duration = Duration::from_secs(30 * 60);

/// Everything known about one identity when deciding whether to open a probe pane for it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ProbeEvidence {
    /// The last probe's `claude auth status --json` said `loggedIn: false`. Since that now runs
    /// in the same herdr pane as `/usage` — which *can* read the macOS Keychain, unlike a
    /// non-login ssh session — it is a real answer, not the guess ssh used to give.
    pub cli_says_logged_out: bool,
    /// A statusLine from a real bot under this account has reached the daemon (this process).
    pub reported_statusline: bool,
    /// A bot with this identity has a live run on this host — survives a daemon restart.
    pub has_live_run: bool,
    /// A probe failed recently and its cool-down has not expired.
    pub cooling_down: bool,
}

/// A stale "logged out" must never park an identity forever (that is exactly how m4p's `cc1`
/// and `cc2` ended up with no quota at all) — the user may have just logged in. Positive
/// evidence — a statusLine, or a live bot run under this account — always wins; otherwise the
/// only thing that holds a probe back is a probe of our own that just failed.
pub(crate) fn should_probe_identity(e: ProbeEvidence) -> bool {
    if e.reported_statusline || e.has_live_run {
        return true;
    }
    !e.cooling_down
}

/// How long to park an identity after its probe failed.
pub(crate) fn failure_backoff(e: ProbeEvidence) -> Duration {
    if e.cli_says_logged_out && !e.reported_statusline && !e.has_live_run {
        RETRY_WHEN_LOGGED_OUT
    } else {
        RETRY_AFTER_FAILURE
    }
}

/// `quota_key` → when the identity may be probed again. In memory only: a restart just means
/// one extra probe, which is the safe direction to err in.
fn backoff_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn cooling_down(key: &str) -> bool {
    let mut m = backoff_map().lock().unwrap();
    match m.get(key) {
        Some(t) if *t > std::time::Instant::now() => true,
        Some(_) => {
            m.remove(key);
            false
        }
        None => false,
    }
}

fn park(key: &str, how_long: Duration) {
    backoff_map().lock().unwrap().insert(key.to_string(), std::time::Instant::now() + how_long);
}

fn unpark(key: &str) {
    backoff_map().lock().unwrap().remove(key);
}

/// Forget the failure backoff for one identity on one host, so the next poll cycle probes it
/// again right away. Called after a login recheck says the account is back: a logged-out
/// identity is parked for 30 minutes, which is exactly how long the popover used to stay
/// wrong after the user logged in.
pub fn unpark_identity(host: &str, name: &str) {
    unpark(&crate::quota::quota_key(host, &format!("claude:{name}")));
}

/// One account to probe this cycle.
struct Target {
    /// Host-less quota key (`claude`, `claude:cc1`, …).
    key: String,
    account: Option<String>,
    env: BTreeMap<String, String>,
    /// Which identity rows this probe's `auth status` answer describes. The bare target speaks
    /// for every claude identity with no env of its own (`cc0`), because that *is* the default
    /// account; an identity target speaks only for itself.
    names: Vec<String>,
    /// `None` for the bare default account, which is never parked.
    evidence: Option<ProbeEvidence>,
}

/// Default account + every Claude identity with a distinct config dir, on `host`.
pub async fn refresh_claude(app: &Arc<App>, host: &str) -> Result<bool> {
    let _guard = crate::quota::probe_lock(host).await;
    // `~` in an identity's env expands against the *probed* host's home, not the daemon's.
    let home = host_home(app, host).await;
    // `[[identities]]` plus the `ccN` aliases discovered on *this* host (SPEC §16): cc1 is a
    // different account on m4p than it is here, and each gets its own probe there.
    let identities = crate::tools::identities_for_host(app, host).await;
    let mut targets: Vec<Target> = Vec::new();

    // Login state as we last knew it — the previous probe's answer, see `ProbeEvidence`.
    let logins = app.tools.lock().await.get(host).map(|t| t.identities.clone()).unwrap_or_default();
    // Which accounts are demonstrably usable here right now (DB-backed, so a daemon restart
    // does not throw the proof away the way the in-memory statusLine trace does).
    let live = crate::db::live_identities_on_host(&app.db, host).await.unwrap_or_default();

    let mut bare_names = Vec::new();
    let mut rest: Vec<Target> = Vec::new();
    for id in identities.iter().filter(|i| i.kind == "claude") {
        let mut env = BTreeMap::new();
        for (k, v) in &id.env {
            env.insert(k.clone(), expand_home(v, &home));
        }
        if env.is_empty() {
            // Same credentials as the default account — strip already maps cc0 → `claude`.
            bare_names.push(id.name.clone());
            continue;
        }
        let key = format!("claude:{}", id.name);
        let full = crate::quota::quota_key(host, &key);
        let evidence = ProbeEvidence {
            cli_says_logged_out: logins.get(&id.name).map(|i| i.logged_in) == Some(Some(false)),
            reported_statusline: app.quotas.lock().await.get(&full).is_some_and(|q| q.source == "statusline"),
            has_live_run: live.contains(&id.name),
            cooling_down: cooling_down(&full),
        };
        if !should_probe_identity(evidence) {
            tracing::debug!(host, identity = %id.name, "identity probe is cooling down after a failure; skipping");
            continue;
        }
        rest.push(Target { key, account: Some(id.name.clone()), env, names: vec![id.name.clone()], evidence: Some(evidence) });
    }
    // Bare default account (also covers empty-env identities like cc0), first.
    targets.push(Target {
        key: "claude".into(),
        account: bare_names.first().cloned(),
        env: BTreeMap::new(),
        names: bare_names,
        evidence: None,
    });
    targets.append(&mut rest);

    let mut any = false;
    let mut saw_missing = false;
    let mut touched = false;
    for t in targets {
        let full = crate::quota::quota_key(host, &t.key);
        // A bot chatting under this account already pushes its limits through the statusLine
        // hook. Opening a pane to re-read what arrived seconds ago is pure cost — and with
        // `cc0`…`cc6` discovered from the shell (SPEC §16) there can be several accounts per
        // host, so this is what keeps one 60 s cycle from turning into a queue of probes.
        // The login answer rides along on the same probe, though, so an identity we have never
        // had an answer for is still worth one pane even while its statusLine is fresh.
        let login_known = t.names.iter().all(|n| logins.get(n).is_some_and(|i| i.logged_in.is_some()));
        if login_known {
            if let Some(q) = app.quotas.lock().await.get(&full) {
                if q.source == "statusline" && fresher_than(&q.updated_at, CLAUDE_POLL) {
                    any = true;
                    continue;
                }
            }
        }
        match refresh_claude_account(app, host, &t.key, t.account.as_deref(), t.env).await {
            Ok(None) => saw_missing = true,
            Ok(Some(o)) => {
                for name in &t.names {
                    touched |= crate::tools::record_identity_login(app, host, name, o.logged_in, o.email.clone(), o.plan.clone()).await;
                }
                if o.quota.is_some() {
                    any = true;
                    unpark(&full);
                } else if let Some(ev) = t.evidence {
                    // Ran fine, but there were no plan lines — logged out, or an account the
                    // CLI will not answer for. Park it with the answer we just got.
                    let how_long = failure_backoff(ProbeEvidence { cli_says_logged_out: o.logged_in == Some(false), ..ev });
                    park(&full, how_long);
                    tracing::info!(host, key = %t.key, logged_in = ?o.logged_in, retry_in_s = how_long.as_secs(), "claude reported no plan lines; parking this identity");
                }
            }
            Err(e) => {
                if let Some(ev) = t.evidence {
                    let how_long = failure_backoff(ev);
                    park(&full, how_long);
                    tracing::warn!(host, key = %t.key, error = %e, retry_in_s = how_long.as_secs(), "claude quota probe failed; parking this identity");
                } else {
                    tracing::warn!(host, key = %t.key, error = %e, "claude quota refresh failed");
                }
            }
        }
    }
    if touched {
        if let Some(conn) = app.hosts.get(host).await {
            crate::state::emit_host_changed(app, &conn).await;
        }
    }
    if saw_missing && !any {
        return Ok(false);
    }
    Ok(any || !saw_missing)
}

/// Start-up + every 60 s, for `local` and every connected remote host (one host at a time:
/// each probe opens a real TUI, and the hosts share nothing but this loop).
pub fn spawn_claude_poller(app: Arc<App>) {
    tokio::spawn(async move {
        sweep_stale(&app).await;
        loop {
            for host in crate::quota::pollable_hosts(&app).await {
                match refresh_claude(&app, &host).await {
                    Ok(true) => {}
                    Ok(false) => tracing::info!(host = %host, "claude not installed; claude quota stays null"),
                    Err(e) => tracing::warn!(host = %host, error = %e, "claude quota refresh failed"),
                }
            }
            tokio::time::sleep(CLAUDE_POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: &str = r#"
▎ Using Opus 5 (1M context) (from .claude/settings.json) · /model
   Settings  Status   Config   Usage   Stats

   Session

   Total cost:            $0.0000
   Usage:                 0 input, 0 output, 0 cache read, 0 cache write

   Current session
   ███████████████████████████████████████            78% used
   Resets 1:20pm (Asia/Taipei)

   Current week (all models)
   ███████████████▌                                   31% used
   Resets Sep 11 at 2pm (Asia/Taipei)
   +50% weekly limits promo through Sep 13 · clau.de/cc-50-promo

   Current week (Fable)
   ███████████████████▌                               39% used
   Resets Sep 11 at 2pm (Asia/Taipei)

   What's contributing to your limits usage?
"#;

    /// `claude -p "/usage"` 的純文字版（本機實測 2026-09-07）。
    const PLAIN: &str = r#"
You are currently using your subscription to power your Claude Code usage

Current session: 47% used · resets Sep 7 at 9:59pm (Asia/Taipei)
Current week (all models): 15% used · resets Sep 14 at 11:59am (Asia/Taipei)
Current week (Fable): 23% used · resets Sep 14 at 11:59am (Asia/Taipei)

What's contributing to your limits usage?
Last 24h · 2950 requests · 43 sessions
  83% of your usage was at >150k context
"#;

    fn at(s: &str) -> DateTime<Local> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Local)
    }

    #[test]
    fn session_and_week_all_models_map() {
        let q = parse_claude_usage(SCREEN, at("2026-09-06T10:00:00+08:00"), Some("cc0")).unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 78.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 31.0);
        // Fable-only weekly must not overwrite all-models — it gets its own window.
        assert_ne!(q.seven_day.as_ref().unwrap().used_pct, 39.0);
        assert_eq!(q.fable.as_ref().unwrap().used_pct, 39.0);
        let fable_reset = q.fable.as_ref().unwrap().resets_at.as_deref().unwrap();
        assert!(fable_reset.starts_with("2026-09-11"), "{fable_reset}");
        assert_eq!(q.source, "claude-usage");
        assert_eq!(q.account.as_deref(), Some("cc0"));
        let five_reset = q.five_hour.as_ref().unwrap().resets_at.as_deref().unwrap();
        assert!(five_reset.contains("T05:20:00") || five_reset.contains("T13:20:00"), "{five_reset}");
        let week_reset = q.seven_day.as_ref().unwrap().resets_at.as_deref().unwrap();
        assert!(week_reset.starts_with("2026-09-11"), "{week_reset}");
    }

    /// 沒有 Fable 那條（非 Max 方案）時 `fable` 要留空，UI 才不會多畫一條空的。
    #[test]
    fn no_fable_row_leaves_the_window_empty() {
        const NO_FABLE: &str = r#"
   Current session
   ███████████████████████████████████████            78% used
   Resets 1:20pm (Asia/Taipei)

   Current week (all models)
   ███████████████▌                                   31% used
   Resets Sep 11 at 2pm (Asia/Taipei)
"#;
        let q = parse_claude_usage(NO_FABLE, at("2026-09-06T10:00:00+08:00"), None).unwrap();
        assert_eq!(q.seven_day.unwrap().used_pct, 31.0);
        assert!(q.fable.is_none());
    }

    /// 標題大小寫不固定（`FABLE` / `fable`），一律要認得。
    #[test]
    fn fable_header_is_case_insensitive() {
        const LOUD: &str = r#"
   Current session
   ███████████████████████████████████████            78% used
   Resets 1:20pm (Asia/Taipei)

   CURRENT WEEK (FABLE)
   ███████████████████▌                               39% used
   Resets Sep 11 at 2pm (Asia/Taipei)
"#;
        let q = parse_claude_usage(LOUD, at("2026-09-06T10:00:00+08:00"), None).unwrap();
        assert_eq!(q.fable.unwrap().used_pct, 39.0);
    }

    #[test]
    fn time_only_reset_rolls_to_tomorrow_when_past() {
        // 3pm asked after 3pm → tomorrow.
        let r = parse_claude_reset("Resets 3:00pm (Asia/Taipei)", at("2026-09-06T16:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-09-07T"), "{r}");
        // 3pm asked before 3pm → today.
        let r = parse_claude_reset("Resets 3:00pm (Asia/Taipei)", at("2026-09-06T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-09-06T"), "{r}");
    }

    #[test]
    fn month_day_reset_parses() {
        let r = parse_claude_reset("Resets Sep 11 at 2pm (Asia/Taipei)", at("2026-09-06T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-09-11T06:00:00") || r.starts_with("2026-09-11T14:00:00"), "{r}");
    }

    #[test]
    fn loading_screen_is_not_a_quota() {
        assert!(parse_claude_usage("Settings  Status   Config   Usage\n  Loading…", Local::now(), None).is_none());
    }

    #[test]
    fn plain_text_usage_parses() {
        let q = parse_claude_usage(PLAIN, at("2026-09-07T12:00:00+08:00"), Some("cc1")).unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 47.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 15.0);
        assert_eq!(q.fable.as_ref().unwrap().used_pct, 23.0);
        assert_eq!(q.account.as_deref(), Some("cc1"));
        assert_eq!(q.source, "claude-usage");
        // `resets Sep 7 at 9:59pm` → 今天稍晚；`Sep 14 …` → 一週後。
        assert!(q.five_hour.as_ref().unwrap().resets_at.as_deref().unwrap().starts_with("2026-09-07"));
        assert!(q.seven_day.as_ref().unwrap().resets_at.as_deref().unwrap().starts_with("2026-09-14"));
        assert!(q.fable.as_ref().unwrap().resets_at.as_deref().unwrap().starts_with("2026-09-14"));
        // 底下的 `83% of your usage …` 不是桶子，不能被當成資料。
        assert_ne!(q.seven_day.as_ref().unwrap().used_pct, 83.0);
    }

    /// 沒登入時 `-p "/usage"` 只印成本摘要，一條桶子都沒有 → 不能生出假的 Quota。
    #[test]
    fn a_logged_out_run_is_not_a_quota() {
        const OUT: &str = "Total cost:            $0.0000\nTotal duration (API):  0s\nUsage: 0 input, 0 output\n";
        assert!(parse_claude_usage(OUT, Local::now(), None).is_none());
    }

    #[test]
    fn the_probe_command_carries_the_identity_env() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), "/home/u/.claude-cc1".to_string());
        // 空字串鍵之類的壞名字不能被 export 進去。
        env.insert("bad name".to_string(), "x".to_string());
        let cmd = probe_command("/opt/homebrew/bin/claude", &env);
        assert!(cmd.contains("CLAUDE_CONFIG_DIR='/home/u/.claude-cc1'"), "{cmd}");
        assert!(!cmd.contains("bad name"), "{cmd}");
        assert!(cmd.contains("auth status --json"), "{cmd}");
        assert!(cmd.contains("-p '/usage'"), "{cmd}");
        // 標記本身不能出現在指令裡，否則 shell 回顯就會被當成輸出。
        assert!(!cmd.contains(AUTH_BEGIN) && !cmd.contains(AUTH_END) && !cmd.contains(USAGE_DONE), "{cmd}");
    }

    #[test]
    fn the_echoed_command_does_not_look_like_the_markers() {
        let cmd = probe_command("/bin/claude", &BTreeMap::new());
        // 真實畫面：先是 shell 回顯整行指令，接著才是輸出。
        let screen = format!(
            "u@host ~ % {cmd}\n\n{AUTH_BEGIN}\n{{\"loggedIn\":true,\"email\":\"a@b.c\"}}\n\n{AUTH_END}\n{PLAIN}\n{USAGE_DONE}0\n"
        );
        let (auth, usage) = split_probe_output(&screen).expect("markers found");
        assert!(auth.contains("\"loggedIn\":true"), "{auth}");
        assert!(usage.contains("Current week (Fable)"), "{usage}");
        assert!(!usage.contains("loggedIn"), "{usage}");
        assert_eq!(crate::tools::read_login_answer("claude", &auth).1.as_deref(), Some("a@b.c"));
    }

    /// 第一行被 shell 吃掉、重打一次時，畫面上會有兩輪；只有最後一輪算數。
    #[test]
    fn only_the_last_run_on_screen_counts() {
        let screen = format!(
            "{AUTH_BEGIN}\nzsh: no such file or directory\n{AUTH_END}\nzsh: no such file or directory\n{USAGE_DONE}127\n\
             {AUTH_BEGIN}\n{{\"loggedIn\":true}}\n{AUTH_END}\n{PLAIN}\n{USAGE_DONE}0\n"
        );
        let (auth, usage) = split_probe_output(&screen).expect("markers found");
        assert!(auth.contains("loggedIn"), "{auth}");
        assert!(!auth.contains("no such file"), "{auth}");
        assert_eq!(parse_claude_usage(&usage, at("2026-09-07T12:00:00+08:00"), None).unwrap().fable.unwrap().used_pct, 23.0);
    }

    /// 指令還沒跑完（沒有 `AM_USAGE_DONE=`）就不能收工——半截的 `/usage` 會少一條桶子。
    #[test]
    fn an_unfinished_run_has_no_answer_yet() {
        let screen = format!("{AUTH_BEGIN}\n{{\"loggedIn\":true}}\n{AUTH_END}\nCurrent session: 47% used\n");
        assert!(split_probe_output(&screen).is_none());
    }

    // ---------------------------------------------------------- probe gating

    /// m4p's `cc1`: the last probe answered `loggedIn: false` and the daemon has just
    /// restarted, so no statusLine has arrived yet. The bot running under `cc1` on that host
    /// is proof enough that it is worth asking again.
    #[test]
    fn a_live_bot_run_beats_the_login_answer() {
        let e = ProbeEvidence { cli_says_logged_out: true, has_live_run: true, ..Default::default() };
        assert!(should_probe_identity(e));
        // …and it keeps beating it even while a failed probe is cooling down.
        assert!(should_probe_identity(ProbeEvidence { cooling_down: true, ..e }));
    }

    /// A statusLine seen this process contradicts a stale "logged out". Still true.
    #[test]
    fn a_statusline_beats_the_login_answer() {
        let e = ProbeEvidence { cli_says_logged_out: true, reported_statusline: true, ..Default::default() };
        assert!(should_probe_identity(e));
    }

    /// The regression this whole gate caused: with no positive evidence yet, a previous
    /// `loggedIn: false` must NOT park the identity forever — the user may have logged in
    /// since, and only another probe can find out.
    #[test]
    fn the_login_answer_alone_never_blocks_a_probe() {
        let e = ProbeEvidence { cli_says_logged_out: true, ..Default::default() };
        assert!(should_probe_identity(e));
    }

    /// Only our own failed probe holds one back, and only until it expires.
    #[test]
    fn a_failed_probe_parks_the_identity_until_its_cooldown_expires() {
        assert!(!should_probe_identity(ProbeEvidence { cooling_down: true, ..Default::default() }));
        assert!(should_probe_identity(ProbeEvidence { cooling_down: false, ..Default::default() }));
    }

    /// A genuinely logged-out account (m4p's `cc2`) costs one probe per half hour, not one per
    /// minute; anything else retries soon in case it was just a busy herdr.
    #[test]
    fn backoff_is_longer_when_the_cli_also_says_logged_out() {
        let out = ProbeEvidence { cli_says_logged_out: true, ..Default::default() };
        assert_eq!(failure_backoff(out), RETRY_WHEN_LOGGED_OUT);
        assert_eq!(failure_backoff(ProbeEvidence::default()), RETRY_AFTER_FAILURE);
        // Positive evidence means the account does work here: a failure is transient.
        assert_eq!(failure_backoff(ProbeEvidence { has_live_run: true, ..out }), RETRY_AFTER_FAILURE);
        assert_eq!(failure_backoff(ProbeEvidence { reported_statusline: true, ..out }), RETRY_AFTER_FAILURE);
    }

    #[test]
    fn parking_expires_and_a_success_clears_it() {
        let k = format!("test/claude:{}", ulid::Ulid::new());
        assert!(!cooling_down(&k));
        park(&k, Duration::from_secs(60));
        assert!(cooling_down(&k));
        unpark(&k);
        assert!(!cooling_down(&k));
        park(&k, Duration::from_millis(0));
        assert!(!cooling_down(&k), "a cool-down in the past is over");
    }
}
