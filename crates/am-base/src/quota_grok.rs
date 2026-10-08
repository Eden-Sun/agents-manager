//! grok quota (SPEC §12.6). The numbers only exist in the TUI's `/usage` dialog, so we drive a
//! throwaway grok pane. It runs in its own unattached `am-quota` session: a narrow user pane cuts
//! the percentage off, and probes every 30 s must not flash through the user's workspace.
//!
//!
//! ```text
//! Context usage  Usage limit  Session info
//! Weekly limit (SuperGrok)
//! ████░░░░░░░░░░░░░░░░░░░░░░░░░░  14%
//! Resets: September 12, 16:28
//! ```

use crate::config::LOCAL_HOST;
use crate::herdr::{AgentStatus, HerdrClient};
use crate::quota::{Quota, Window};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use std::time::Duration;

/// User-specified cadence ("30s is ok"), even though a probe holds grok for ~10 s.
pub const GROK_POLL: Duration = Duration::from_secs(30);

pub const DIALOG_TIMEOUT: Duration = Duration::from_secs(25);

fn clean(line: &str) -> String {
    let s: String = line
        .chars()
        .map(|c| match c {
            '│' | '┌' | '┐' | '└' | '┘' | '─' | '├' | '┤' | '┬' | '┴' | '┼' => ' ',
            c => c,
        })
        .collect();
    s.trim().to_string()
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

/// grok omits the year: assume this one, rolled forward if that puts the reset in the past.
/// The time is the wall clock of the host grok runs on (#629): `now` carries that host's zone.
pub fn parse_reset<Tz: TimeZone>(rest: &str, now: DateTime<Tz>) -> Option<String> {
    let mut month = None;
    let mut day = None;
    let mut year = None;
    let mut hm = None;
    for tok in rest.split(|c: char| c.is_whitespace() || c == ',').filter(|t| !t.is_empty()) {
        if month.is_none() {
            if let Some(m) = month_num(tok) {
                month = Some(m);
                continue;
            }
        }
        if let Some((h, m)) = tok.split_once(':') {
            if let (Ok(h), Ok(m)) = (h.parse::<u32>(), m.trim_matches(|c: char| !c.is_ascii_digit()).parse::<u32>())
            {
                hm = Some((h, m));
                continue;
            }
        }
        let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        match digits.len() {
            4 => year = digits.parse::<i32>().ok(),
            _ if day.is_none() => day = digits.parse::<u32>().ok(),
            _ => {}
        }
    }
    let (month, day) = (month?, day?);
    let (h, m) = hm.unwrap_or((0, 0));
    let tz = now.timezone();
    let build = |y: i32| -> Option<DateTime<Tz>> {
        let d = NaiveDate::from_ymd_opt(y, month, day)?.and_hms_opt(h, m, 0)?;
        tz.from_local_datetime(&d).earliest()
    };
    let dt = match year {
        Some(y) => build(y)?,
        None => {
            let this = build(now.year())?;
            // A reset more than a day behind us is last year's rendering of the same date.
            if this < now.clone() - chrono::Duration::days(1) {
                build(now.year() + 1)?
            } else {
                this
            }
        }
    };
    Some(dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// Bar rows only, so the context-usage percentage is never mistaken for a rate limit.
fn parse_pct(line: &str) -> Option<f64> {
    if !line.contains('█') && !line.contains('░') {
        return None;
    }
    let (head, _) = line.split_once('%')?;
    let num: String = head.chars().rev().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    if num.is_empty() {
        return None;
    }
    num.chars().rev().collect::<String>().parse().ok()
}

fn parse_header(line: &str) -> Option<(String, Option<String>)> {
    let low = line.to_ascii_lowercase();
    let at = low.find(" limit")?;
    let window = line[..at].trim().to_ascii_lowercase();
    if window.is_empty() || window.contains(' ') {
        return None;
    }
    let plan = line[at..].split_once('(').and_then(|(_, r)| r.split_once(')')).map(|(p, _)| p.trim().to_string());
    Some((window, plan.filter(|p| !p.is_empty())))
}

pub fn parse_grok_usage<Tz: TimeZone>(screen: &str, now: DateTime<Tz>) -> Option<Quota> {
    let lines: Vec<String> = screen.lines().map(clean).collect();
    let mut five: Option<Window> = None;
    let mut seven: Option<Window> = None;
    let mut plan: Option<String> = None;
    let mut i = 0;
    while i < lines.len() {
        let Some((window, p)) = parse_header(&lines[i]) else {
            i += 1;
            continue;
        };
        let mut pct = None;
        let mut resets = None;
        let mut j = i + 1;
        while j < lines.len() && j < i + 8 {
            if parse_header(&lines[j]).is_some() {
                break;
            }
            if pct.is_none() {
                pct = parse_pct(&lines[j]);
            }
            if let Some(rest) = lines[j].strip_prefix("Resets:") {
                resets = parse_reset(rest, now.clone());
            }
            j += 1;
        }
        if let Some(used_pct) = pct {
            let w = Window { observed_at: None, used_pct, resets_at: resets };
            if window.contains("week") {
                seven = Some(w);
            } else if window.contains("hour") {
                five = Some(w);
            }
            if plan.is_none() {
                plan = p;
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
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan,
        updated_at: crate::db::now(),
        source: "grok-usage".into(),
        account: None,
        host: LOCAL_HOST.into(),
    })
}

/// A remote grok prints the remote host's wall clock (#629), so read it with that host's detected UTC
/// offset. No offset = don't guess (same as #59): keep the percentages, drop the reset time.
pub fn parse_grok_usage_remote(screen: &str, now: DateTime<chrono::Utc>, offset_secs: Option<i32>) -> Option<Quota> {
    match offset_secs.and_then(chrono::FixedOffset::east_opt) {
        Some(tz) => parse_grok_usage(screen, now.with_timezone(&tz)),
        None => {
            let mut q = parse_grok_usage(screen, now)?;
            for w in [&mut q.five_hour, &mut q.seven_day].into_iter().flatten() {
                w.resets_at = None;
            }
            Some(q)
        }
    }
}

/// Local grok = the daemon's own zone; a remote one = that host's detected offset.
pub async fn parse_probe_screen(app: &impl crate::tools::ToolsTable, host: &str, screen: &str, now: DateTime<chrono::Utc>) -> Option<Quota> {
    if host == LOCAL_HOST {
        return parse_grok_usage(screen, now.with_timezone(&Local));
    }
    let offset = app.tools().lock().await.get(host).and_then(|t| t.utc_offset_secs);
    parse_grok_usage_remote(screen, now, offset)
}

const PROBE_SESSION: &str = "am-quota";
/// Lets stale probe workspaces be recognised after a daemon restart.
const PROBE_LABEL: &str = "am-quota-grok";

async fn probe_client() -> Result<HerdrClient> {
    let client = HerdrClient::new(HerdrClient::session_socket(PROBE_SESSION));
    if client.ping().await.is_ok() {
        return Ok(client);
    }
    // `herdr --session … server` stays in the foreground; detach it and wait for the socket.
    let child = std::process::Command::new("/bin/sh")
        .arg("-lc")
        .arg(format!("exec herdr --session {PROBE_SESSION} server"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("could not start the `{PROBE_SESSION}` herdr session: {e}"))?;
    reap_in_background(child);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if client.ping().await.is_ok() {
            return Ok(client);
        }
    }
    Err(anyhow!("the `{PROBE_SESSION}` herdr session did not come up"))
}

fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Probe workspaces orphaned by a daemon that died mid-probe still hold a live grok process.
pub async fn sweep_stale(app: &(impl crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess + crate::shared_host::SharedHostEnv)) {
    if let Ok(c) = probe_client().await {
        sweep_probes(&c, None).await;
    }
    sweep_stale_on_hosts(app).await;
}

/// Remote probes live in the remote session by design — see [`client_for`]. 共用 session 的主機只清帶本 daemon 標記的（#709）。
pub async fn sweep_stale_on_hosts(app: &(impl crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess + crate::shared_host::SharedHostEnv)) {
    for host in crate::quota::pollable_hosts(app).await {
        if let Some(c) = app.herdr_for(&host).await {
            sweep_probes(&c, crate::shared_host::probe_tag(app, &host).await.as_deref()).await;
        }
    }
}

async fn sweep_probes(c: &HerdrClient, own_tag: Option<&str>) {
    let Ok(list) = c.workspace_list().await else { return };
    for ws in list.iter().filter(|w| w.label.as_deref().is_some_and(|l| crate::shared_host::sweepable(l, own_tag, |l| l == PROBE_LABEL))) {
        match c.workspace_close(&ws.workspace_id).await {
            Ok(()) => tracing::info!(workspace = %ws.workspace_id, "closed a stale grok quota probe"),
            Err(e) => tracing::warn!(workspace = %ws.workspace_id, error = %e, "stale probe not closed"),
        }
    }
}

/// 共用 session 的主機上 label 帶本 daemon 的標記（#709）。
pub async fn probe_label(app: &impl crate::shared_host::SharedHostEnv, host: &str) -> String {
    match crate::shared_host::probe_tag(app, host).await {
        Some(tag) => crate::shared_host::tagged_label(PROBE_LABEL, &tag),
        None => PROBE_LABEL.to_string(),
    }
}

/// Closes the probe workspace on every exit path, including the error ones.
pub struct Probe {
    client: HerdrClient,
    workspace_id: String,
    /// 登記期間 `pane.agent_detected` 不觸發整台主機的對帳（[`crate::probe_ws`]）。
    _registered: crate::probe_ws::ProbeWorkspace,
}

impl Probe {
    pub fn new(client: HerdrClient, workspace_id: String) -> Self {
        Self {
            _registered: crate::probe_ws::ProbeWorkspace::register(client.socket_path(), &workspace_id),
            client,
            workspace_id,
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let (client, ws) = (self.client.clone(), self.workspace_id.clone());
        tokio::spawn(async move {
            if let Err(e) = client.workspace_close(&ws).await {
                tracing::warn!(workspace = %ws, error = %e, "grok quota probe workspace not closed");
            }
        });
    }
}

/// Remote hosts borrow the forwarded session — see [`crate::quota_claude::client_for`].
pub async fn client_for_fence(app: &impl crate::capabilities::HerdrRoutes, fence: &crate::hosts::HostFence) -> Result<HerdrClient> {
    if fence.conn().is_local() {
        return probe_client().await;
    }
    if !fence.conn().is_connected() {
        return Err(anyhow!("host `{}` is not connected", fence.conn().name));
    }
    let session = fence.conn().cfg.as_ref().map(|cfg| cfg.herdr_session.as_str())
        .ok_or_else(|| anyhow!("host `{}` has no configured herdr session", fence.conn().name))?;
    app.herdr_for_host_fence(fence, session).await
        .ok_or_else(|| anyhow!("host `{}` changed or has no herdr client", fence.conn().name))
}

/// `workspace.create` 回來時 pane 的 shell 常常還沒就緒（claude 探測也因此先睡 700 ms 才打字）：這時 `agent.start`
/// 回 `agent_pane_busy`（`agent target pane … is not an available shell`），以前整輪額度探測就此失敗、停放五分鐘——
/// 本機的 grok 額度從 09-28 起每天幾十到一百多次這樣失敗。這個錯誤只代表 shell 還沒好：等一下重試，有上限。
pub const SHELL_READY_ATTEMPTS: u32 = 12;
const SHELL_READY_WAIT: Duration = if cfg!(test) { Duration::from_millis(5) } else { Duration::from_millis(500) };

pub fn pane_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::herdr::HerdrError>().is_some_and(|h| h.code == "agent_pane_busy")
}

pub async fn start_when_shell_ready(client: &HerdrClient, name: &str, pane_id: &str) -> Result<crate::herdr::AgentInfo> {
    let mut attempt = 1;
    loop {
        match client.agent_start(name, "grok", pane_id, &[], 60_000).await {
            Err(e) if pane_busy(&e) && attempt < SHELL_READY_ATTEMPTS => {
                tracing::debug!(pane = %pane_id, attempt, "grok probe: the pane's shell is not ready yet; retrying agent.start");
                attempt += 1;
                tokio::time::sleep(SHELL_READY_WAIT).await;
            }
            other => return other,
        }
    }
}

/// herdr 回的是「名字已經不在那個 pane 上」——agent 本身可能好好的。
pub fn name_lost(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::herdr::HerdrError>().is_some_and(|h| h.code == "agent_name_not_found")
}

/// pane 上是 grok、而且已經在 idle／working／blocked（TUI 起來了）。
pub fn grok_pane_ready(info: Option<&crate::herdr::PaneInfo>) -> bool {
    info.is_some_and(|p| {
        p.agent.as_deref() == Some("grok")
            && matches!(p.agent_status, Some(AgentStatus::Idle | AgentStatus::Working | AgentStatus::Blocked))
    })
}

pub async fn wait_for_grok_pane(client: &HerdrClient, pane_id: &str, limit: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if grok_pane_ready(client.pane_get(pane_id).await?.as_ref()) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("grok did not come up in the probe pane {pane_id} within {limit:?}"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Backoff (SPEC §16.4): a grok that can't draw `/usage` otherwise costs a full probe every 30 s forever.
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);

/// In memory only: a restart costs one extra probe, the safe direction to err in.
fn backoff_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

pub fn cooling_down(key: &str) -> bool {
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

pub fn park(key: &str, how_long: Duration) {
    backoff_map().lock().unwrap().insert(key.to_string(), std::time::Instant::now() + how_long);
}

/// 主機被移除或改指到別台時，丟掉那台的探測退避（#892）：遠端 key 是 `<host>/…`，本機的不帶 `/`。
pub fn forget_host(host: &str) {
    backoff_map().lock().unwrap().retain(|k, _| {
        if host == LOCAL_HOST {
            k.contains('/')
        } else {
            !k.strip_prefix(host).is_some_and(|rest| rest.starts_with('/'))
        }
    });
}

/// §16.4 skip rules: logged out, or our own probe failed recently.
pub fn should_probe_grok(logged_in: Option<bool>, cooling: bool) -> bool {
    logged_in != Some(false) && !cooling
}
