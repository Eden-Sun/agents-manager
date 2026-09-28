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
use crate::state::App;
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// User-specified cadence ("30s is ok"), even though a probe holds grok for ~10 s.
pub const GROK_POLL: Duration = Duration::from_secs(30);

const DIALOG_TIMEOUT: Duration = Duration::from_secs(25);

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
fn parse_grok_usage_remote(screen: &str, now: DateTime<chrono::Utc>, offset_secs: Option<i32>) -> Option<Quota> {
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
async fn parse_probe_screen(app: &Arc<App>, host: &str, screen: &str, now: DateTime<chrono::Utc>) -> Option<Quota> {
    if host == LOCAL_HOST {
        return parse_grok_usage(screen, now.with_timezone(&Local));
    }
    let offset = app.tools.lock().await.get(host).and_then(|t| t.utc_offset_secs);
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
    crate::state::reap_in_background(child);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if client.ping().await.is_ok() {
            return Ok(client);
        }
    }
    Err(anyhow!("the `{PROBE_SESSION}` herdr session did not come up"))
}

/// Probe workspaces orphaned by a daemon that died mid-probe still hold a live grok process.
pub async fn sweep_stale(app: &Arc<App>) {
    if let Ok(c) = probe_client().await {
        sweep_probes(&c, None).await;
    }
    sweep_stale_on_hosts(app).await;
}

/// Remote probes live in the remote session by design — see [`client_for`]. 共用 session 的主機只清帶本 daemon 標記的（#709）。
async fn sweep_stale_on_hosts(app: &Arc<App>) {
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
async fn probe_label(app: &Arc<App>, host: &str) -> String {
    match crate::shared_host::probe_tag(app, host).await {
        Some(tag) => crate::shared_host::tagged_label(PROBE_LABEL, &tag),
        None => PROBE_LABEL.to_string(),
    }
}

/// Closes the probe workspace on every exit path, including the error ones.
struct Probe {
    client: HerdrClient,
    workspace_id: String,
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
async fn client_for_fence(app: &Arc<App>, fence: &crate::hosts::HostFence) -> Result<HerdrClient> {
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

/// `Ok(false)` = grok is not installed there (quota stays null).
pub async fn refresh_grok(app: &Arc<App>, host: &str) -> Result<bool> {
    let _guard = crate::quota::probe_lock(host).await;
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let cwd = crate::hosts::home_for_fence(&fence).await?;
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` changed while resolving HOME");
    }
    // The start-up poller can beat detection; empty cache = unknown, not missing.
    if !app.tools.lock().await.contains_key(host) {
        crate::tools::detect(app, host).await?;
    }
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` changed before its Grok quota probe");
    }
    if crate::tools::cached_path(app, host, "grok").await.is_none() {
        return Ok(false);
    }
    let client = client_for_fence(app, &fence).await?;
    let (ws, pane) = client.workspace_create(&cwd, &probe_label(app, host).await, json!({})).await?;
    let probe = Probe { client: client.clone(), workspace_id: ws.workspace_id.clone() };
    let pane_id = pane.pane_id.clone();

    // A distinct name so reconcile and the bot list can never confuse it with a real bot.
    let name = format!("amquota{}", ulid::Ulid::new().to_string()[20..].to_ascii_lowercase());
    // grok 1.0.41 起 herdr 常在啟動當下就把名字從 pane 上拿掉（`agent_name_not_found`：named agent … no longer owns
    // the target terminal），之後用名字 `agent_wait` 回 `agent_not_running`——grok 其實好好開著，額度卻從 09-23 起
    // 大多數輪都讀不到（2026-09-28 使用者：「grok children 跑了一陣子，usage 沒更新」）。探測只需要 pane：
    // 名字掉了不算失敗，改看 pane 自己的 agent／狀態。
    if let Err(e) = client.agent_start(&name, "grok", &pane_id, &[], 60_000).await {
        if !name_lost(&e) {
            return Err(e);
        }
        tracing::debug!(host, pane = %pane_id, "grok probe: herdr dropped the agent name at start; waiting on the pane instead");
    }
    wait_for_grok_pane(&client, &pane_id, Duration::from_secs(60)).await?;
    // The TUI accepts a slash command only once its input line is drawn.
    tokio::time::sleep(Duration::from_secs(3)).await;
    client.pane_send_text(&pane_id, "/usage").await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    client.pane_send_keys(&pane_id, &["Enter"]).await?;

    let deadline = tokio::time::Instant::now() + DIALOG_TIMEOUT;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(900)).await;
        last = client.pane_read(&pane_id, "visible", 120).await.map(|r| r.text).unwrap_or_default();
        if let Some(q) = parse_probe_screen(app, host, &last, chrono::Utc::now()).await {
            let published = crate::quota::set_fenced(app, host, "grok", q, &fence).await;
            drop(probe);
            published?;
            return Ok(true);
        }
    }
    drop(probe);
    tracing::debug!(host, screen = %last, "grok /usage did not render a limit row");
    Err(anyhow!("grok `/usage` on {host} did not report a limit within {DIALOG_TIMEOUT:?}"))
}

/// herdr 回的是「名字已經不在那個 pane 上」——agent 本身可能好好的。
fn name_lost(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::herdr::HerdrError>().is_some_and(|h| h.code == "agent_name_not_found")
}

/// pane 上是 grok、而且已經在 idle／working／blocked（TUI 起來了）。
fn grok_pane_ready(info: Option<&crate::herdr::PaneInfo>) -> bool {
    info.is_some_and(|p| {
        p.agent.as_deref() == Some("grok")
            && matches!(p.agent_status, Some(AgentStatus::Idle | AgentStatus::Working | AgentStatus::Blocked))
    })
}

async fn wait_for_grok_pane(client: &HerdrClient, pane_id: &str, limit: Duration) -> Result<()> {
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
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);

/// In memory only: a restart costs one extra probe, the safe direction to err in.
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

/// §16.4 skip rules: logged out, or our own probe failed recently.
pub(crate) fn should_probe_grok(logged_in: Option<bool>, cooling: bool) -> bool {
    logged_in != Some(false) && !cooling
}

/// `None` = skipped this cycle. `GET /api/quota?refresh=1` bypasses this and always really probes.
pub async fn refresh_grok_if_due(app: &Arc<App>, host: &str) -> Result<Option<bool>> {
    let key = crate::quota::quota_key(host, "grok");
    let logged_in = app.tools.lock().await.get(host).and_then(|t| t.tools.get("grok")).and_then(|t| t.logged_in);
    if !should_probe_grok(logged_in, cooling_down(&key)) {
        return Ok(None);
    }
    match refresh_grok(app, host).await {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            park(&key, RETRY_AFTER_FAILURE);
            Err(e)
        }
    }
}

pub fn spawn_grok_poller(app: Arc<App>) {
    tokio::spawn(async move {
        sweep_stale(&app).await;
        loop {
            crate::quota::for_each_host(crate::quota::pollable_hosts(&app).await, |host| {
                let app = app.clone();
                async move {
                    match refresh_grok_if_due(&app, &host).await {
                        Ok(Some(true)) => {}
                        Ok(Some(false)) => tracing::info!(host = %host, "grok not installed; grok quota stays null"),
                        Ok(None) => tracing::debug!(host = %host, "grok probe skipped (logged out or cooling down)"),
                        Err(e) => tracing::warn!(host = %host, error = %e, retry_in_s = RETRY_AFTER_FAILURE.as_secs(), "grok quota refresh failed; parking this host"),
                    }
                }
            })
            .await;
            tokio::time::sleep(GROK_POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_logged_out_or_recently_failed_host_is_not_probed() {
        assert!(should_probe_grok(None, false));
        assert!(should_probe_grok(Some(true), false));
        assert!(!should_probe_grok(Some(false), false));
        assert!(!should_probe_grok(None, true));

        let key = format!("test-{}/grok", crate::db::ulid());
        assert!(!cooling_down(&key));
        park(&key, Duration::from_secs(60));
        assert!(cooling_down(&key));
        park(&key, Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(5));
        assert!(!cooling_down(&key), "an expired park clears itself");
    }

    #[tokio::test]
    async fn an_unreadable_remote_home_skips_grok_probe_and_recovers_next_poll() {
        use std::sync::atomic::Ordering;

        let app = crate::testing::env().await.app.clone();
        let host = format!("grok-home-618-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        conn.connected.store(true, Ordering::SeqCst);
        app.tools.lock().await.insert(host.clone(), crate::tools::HostTools {
            tools: [("grok".into(), crate::tools::ToolInfo { installed: true, path: Some("/usr/bin/grok".into()), version: None, logged_in: Some(true) })].into(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        });
        let ssh_calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let ssh_calls2 = ssh_calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            ssh_calls2.lock().unwrap().push(script.to_string());
            Err(anyhow!("injected remote HOME read failure"))
        });
        let socket = conn.client.socket_path().to_path_buf();
        let herdr = crate::testing::MockHerdr::start(socket.clone());

        let key = crate::quota::quota_key(&host, "grok");
        let err = refresh_grok(&app, &host).await.expect_err("unreadable remote HOME must stop the Grok probe");
        assert!(err.to_string().contains("HOME"), "retain the HOME failure reason: {err:#}");
        assert!(herdr.calls_to("workspace.create").is_empty(), "do not create a workspace at `/tmp` or daemon HOME");
        assert!(!app.quotas.lock().await.contains_key(&key), "no quota observation is published");
        assert_eq!(ssh_calls.lock().unwrap().len(), 1, "only the failed HOME read is allowed");

        *conn.remote_home.lock().await = Some("/home/remote-grok".into());
        herdr.set_screen("*", SCREEN);
        assert!(refresh_grok(&app, &host).await.unwrap(), "the next poll retries naturally");
        let creates = herdr.calls_to("workspace.create");
        assert_eq!(creates.len(), 1, "one recovered Grok workspace is created");
        assert_eq!(creates[0]["cwd"], "/home/remote-grok", "workspace uses the remote HOME");
        assert!(app.quotas.lock().await.contains_key(&key), "the recovered quota is published");

        drop(herdr);
        let _ = std::fs::remove_file(socket);
    }

    const SCREEN: &str = "\
  /private/tmp                                                        1.5K / 500K
     ◆ session_start  [hooks: 2]
        ┌──────────────────────────────────────────────────── [✗] ─┐
        │  Context usage  Usage limit  Session info                │
        │──────────────────────────────────────────────────────────│
        │  Weekly limit (SuperGrok)                                │
        │                                                          │
        │  ████░░░░░░░░░░░░░░░░░░░░░░░░░░  14%                     │
        │  Resets: September 12, 16:28                             │
        │                                                          │
        │           Tab switch  |  ↑/↓ scroll  |  Esc close        │
        └──────────────────────────────────────────────────────────┘";

    fn at(s: &str) -> DateTime<Local> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Local)
    }

    /// 2026-09-28：herdr 在啟動當下把名字拿掉只算「名字不見」，不算探測失敗；其他錯照舊是失敗。
    #[test]
    fn a_dropped_agent_name_is_not_a_failed_probe() {
        let lost: anyhow::Error = crate::herdr::HerdrError { code: "agent_name_not_found".into(), message: "named agent amquota no longer owns the target terminal".into() }.into();
        assert!(name_lost(&lost));
        let other: anyhow::Error = crate::herdr::HerdrError { code: "pane_not_found".into(), message: "x".into() }.into();
        assert!(!name_lost(&other));
        assert!(!name_lost(&anyhow::anyhow!("socket closed")));
    }

    #[test]
    fn the_probe_pane_is_ready_once_grok_is_up_on_it() {
        let pane = |agent: Option<&str>, st: Option<AgentStatus>| crate::herdr::PaneInfo {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            cwd: None,
            foreground_cwd: None,
            agent: agent.map(String::from),
            agent_status: st,
            revision: 0,
            scroll: None,
        };
        assert!(grok_pane_ready(Some(&pane(Some("grok"), Some(AgentStatus::Idle)))));
        assert!(grok_pane_ready(Some(&pane(Some("grok"), Some(AgentStatus::Blocked)))));
        assert!(!grok_pane_ready(Some(&pane(None, None))), "還在 shell");
        assert!(!grok_pane_ready(Some(&pane(Some("grok"), Some(AgentStatus::Unknown)))), "還沒畫完");
        assert!(!grok_pane_ready(Some(&pane(Some("claude"), Some(AgentStatus::Idle)))));
        assert!(!grok_pane_ready(None));
    }

    #[test]
    fn weekly_limit_lands_on_seven_day() {
        let q = parse_grok_usage(SCREEN, at("2026-09-06T12:00:00+08:00")).unwrap();
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 14.0);
        assert!(q.five_hour.is_none());
        assert_eq!(q.plan.as_deref(), Some("SuperGrok"));
        assert_eq!(q.source, "grok-usage");
        assert!(q.seven_day.unwrap().resets_at.unwrap().starts_with("2026-09-12"));
    }

    #[test]
    fn a_reset_already_past_belongs_to_next_year() {
        // Asked in late December about a reset rendered as "January 3".
        let r = parse_reset(" January 3, 09:00", at("2026-12-28T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2027-01-03"), "{r}");
        // Same date, asked in January: this year.
        let r = parse_reset(" January 3, 09:00", at("2026-01-02T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-01-03"), "{r}");
    }

    #[test]
    fn an_explicit_year_is_honoured() {
        let r = parse_reset(" September 12, 2027 16:28", at("2026-09-06T12:00:00+08:00")).unwrap();
        assert!(r.starts_with("2027-09-12"), "{r}");
    }

    const REMOTE_SCREEN: &str = "Weekly limit (SuperGrok)\n  ████░░  14%\n  Resets: September 12, 2027 16:28";

    #[test]
    fn a_remote_grok_reset_uses_the_host_offset_not_the_daemon_timezone() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-06T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        for (offset_secs, expected) in [(0, "2027-09-12T16:28:00.000Z"), (13 * 3600, "2027-09-12T03:28:00.000Z")] {
            let quota = parse_grok_usage_remote(REMOTE_SCREEN, now, Some(offset_secs)).unwrap();
            assert_eq!(quota.seven_day.unwrap().resets_at.as_deref(), Some(expected));
        }
    }

    #[test]
    fn a_remote_grok_without_a_detected_offset_keeps_usage_but_does_not_guess_reset_time() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-06T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let quota = parse_grok_usage_remote(REMOTE_SCREEN, now, None).unwrap();
        let weekly = quota.seven_day.unwrap();
        assert_eq!(weekly.used_pct, 14.0);
        assert_eq!(weekly.resets_at, None);
    }

    /// The probe reads the offset detected for that host (#629), not the daemon's zone.
    #[tokio::test]
    async fn the_probe_parses_a_remote_screen_with_that_hosts_offset() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let tools = |offset| crate::tools::HostTools {
            tools: Default::default(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: offset,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert("r13".into(), tools(Some(13 * 3600)));
        app.tools.lock().await.insert("rnone".into(), tools(None));
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-06T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let reset = |q: Option<Quota>| q.unwrap().seven_day.unwrap().resets_at;
        assert_eq!(reset(parse_probe_screen(&app, "r13", REMOTE_SCREEN, now).await).as_deref(), Some("2027-09-12T03:28:00.000Z"));
        assert_eq!(reset(parse_probe_screen(&app, "rnone", REMOTE_SCREEN, now).await), None);
        assert_eq!(reset(parse_probe_screen(&app, "unknown", REMOTE_SCREEN, now).await), None, "never detected = no offset");
        let local = Local.from_local_datetime(&NaiveDate::from_ymd_opt(2027, 9, 12).unwrap().and_hms_opt(16, 28, 0).unwrap()).earliest().unwrap();
        assert_eq!(reset(parse_probe_screen(&app, LOCAL_HOST, REMOTE_SCREEN, now).await), Some(crate::db::iso_at(local.with_timezone(&chrono::Utc))));
    }

    #[test]
    fn an_hourly_row_would_land_on_five_hour() {
        let s = "  2-hour limit (SuperGrok)\n  ██░░  7%\n  Resets: September 6, 18:00\n\
                 \n  Weekly limit (SuperGrok)\n  ████░░  40%\n  Resets: September 12, 16:28";
        let q = parse_grok_usage(s, at("2026-09-06T12:00:00+08:00")).unwrap();
        assert_eq!(q.five_hour.unwrap().used_pct, 7.0);
        assert_eq!(q.seven_day.unwrap().used_pct, 40.0);
    }

    #[test]
    fn a_screen_without_a_bar_row_is_not_a_quota() {
        assert!(parse_grok_usage("Context usage  Usage limit\n  Loading…", Local::now()).is_none());
        // The context-usage tab has a percentage but no limit header.
        assert!(parse_grok_usage("  Context: ████░░  62%", Local::now()).is_none());
    }
}

#[cfg(test)]
mod shared_session_tests {
    //! #709：同 `quota_claude` 那一條，grok 的探測。
    use crate::shared_host::tests::{set_shared, shared_host, HOST};
    use serde_json::json;

    async fn labels(c: &crate::herdr::HerdrClient) -> Vec<String> {
        let mut v: Vec<String> = c.workspace_list().await.unwrap().into_iter().filter_map(|w| w.label).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn a_shared_host_labels_and_sweeps_only_this_daemons_probes() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sh = shared_host(&env, true).await;
        let tag = crate::shared_host::probe_tag(&app, HOST).await.unwrap();
        assert_eq!(super::probe_label(&app, HOST).await, format!("am-quota-grok@{tag}"));
        for l in [format!("am-quota-grok@{tag}"), "am-quota-grok@other".into(), "am-quota-grok".into(), "proj".into()] {
            sh.client.workspace_create("/tmp", &l, json!({})).await.unwrap();
        }
        super::sweep_stale_on_hosts(&app).await;
        assert_eq!(labels(&sh.client).await, ["am-quota-grok", "am-quota-grok@other", "proj"]);

        set_shared(&app, false).await;
        assert_eq!(super::probe_label(&app, HOST).await, "am-quota-grok");
        super::sweep_stale_on_hosts(&app).await;
        assert_eq!(labels(&sh.client).await, ["proj"]);
    }
}
