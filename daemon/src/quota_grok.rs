//! grok quota (SPEC §12.6).
//!
//! grok's CLI has no `usage` subcommand and no rate-limit RPC — the numbers only exist inside
//! the TUI's `/usage` dialog. So the daemon opens a throwaway herdr workspace, starts a grok
//! agent in it, types `/usage`, reads the rendered dialog back through `pane.read`, parses it
//! and closes the workspace again. The probe pane is never focused and never adopted as a bot
//! (reconcile only matches agents named in the DB), so it is invisible to the user.
//!
//! The dialog looks like this (box drawing trimmed):
//!
//! ```text
//! Context usage  Usage limit  Session info
//! Weekly limit (SuperGrok)
//! ████░░░░░░░░░░░░░░░░░░░░░░░░░░  14%
//! Resets: September 12, 16:28
//! ```
//!
//! Only a weekly window is reported today, so it lands on `seven_day`; an hourly row would map
//! to `five_hour` if grok ever grows one.

use crate::config::LOCAL_HOST;
use crate::herdr::{AgentStatus, HerdrClient};
use crate::quota::{Quota, Window};
use crate::state::App;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// One probe opens a grok process for ~10 s, so the cycle is mostly probe. The user asked for
/// this cadence explicitly ("30s is ok"); the weekly number then tracks the TUI closely.
pub const GROK_POLL: Duration = Duration::from_secs(30);

/// How long to wait for the `/usage` dialog to render before giving up.
const DIALOG_TIMEOUT: Duration = Duration::from_secs(25);

// ------------------------------------------------------------------ parsing

/// Box drawing, bar glyphs and the dialog frame carry no information — drop them.
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

/// `Resets: September 12, 16:28` → RFC3339. The year is omitted by grok, so it is inferred:
/// the current one, rolled forward when that would put the reset in the past.
pub fn parse_reset(rest: &str, now: DateTime<Local>) -> Option<String> {
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
    let build = |y: i32| -> Option<DateTime<Local>> {
        let d = NaiveDate::from_ymd_opt(y, month, day)?.and_hms_opt(h, m, 0)?;
        Local.from_local_datetime(&d).earliest()
    };
    let dt = match year {
        Some(y) => build(y)?,
        None => {
            let this = build(now.year())?;
            // A reset more than a day behind us is last year's rendering of the same date.
            if this < now - chrono::Duration::days(1) {
                build(now.year() + 1)?
            } else {
                this
            }
        }
    };
    Some(dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// `████░░░░  14%` → `14.0`. Only bar rows are accepted, so the context-usage percentage in
/// another tab can never be mistaken for a rate limit.
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

/// `Weekly limit (SuperGrok)` → (`weekly`, `SuperGrok`).
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

/// The whole `/usage` screen → a Quota, or `None` when no limit row is on it yet.
pub fn parse_grok_usage(screen: &str, now: DateTime<Local>) -> Option<Quota> {
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
        // The bar and the reset line follow the header, with blank rows in between.
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
                resets = parse_reset(rest, now);
            }
            j += 1;
        }
        if let Some(used_pct) = pct {
            let w = Window { used_pct, resets_at: resets };
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
        plan,
        updated_at: crate::db::now(),
        source: "grok-usage".into(),
        account: None,
    })
}

// ------------------------------------------------------------------ the probe

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

/// One grok quota read. `Ok(false)` = grok is not installed locally (quota stays null).
pub async fn refresh_grok(app: &Arc<App>) -> Result<bool> {
    // The start-up poller can beat CLI detection to the cache; an empty cache is "unknown",
    // not "missing", so detect once rather than reporting grok as uninstalled for 30 minutes.
    if !app.tools.lock().await.contains_key(LOCAL_HOST) {
        crate::tools::detect(app, LOCAL_HOST).await?;
    }
    if crate::tools::cached_path(app, LOCAL_HOST, "grok").await.is_none() {
        return Ok(false);
    }
    let client = app.herdr_for(LOCAL_HOST).await.ok_or_else(|| anyhow!("local host is not configured"))?;
    if !app.host_connected(LOCAL_HOST).await {
        return Err(anyhow!("local herdr is not connected"));
    }
    let cwd = dirs::home_dir().map(|p| p.display().to_string()).unwrap_or_else(|| "/tmp".into());
    let (ws, pane) = client.workspace_create(&cwd, "am-quota-grok", json!({})).await?;
    let probe = Probe { client: client.clone(), workspace_id: ws.workspace_id.clone() };
    let pane_id = pane.pane_id.clone();

    // A distinct name so reconcile and the bot list can never confuse it with a real bot.
    let name = format!("amquota{}", ulid::Ulid::new().to_string()[20..].to_ascii_lowercase());
    client.agent_start(&name, "grok", &pane_id, &[], 60_000).await?;
    client
        .agent_wait(&name, &[AgentStatus::Idle, AgentStatus::Working, AgentStatus::Blocked], 60_000)
        .await?;
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
        if let Some(q) = parse_grok_usage(&last, Local::now()) {
            crate::quota::set(app, "grok", q).await;
            drop(probe);
            return Ok(true);
        }
    }
    drop(probe);
    tracing::debug!(screen = %last, "grok /usage did not render a limit row");
    Err(anyhow!("grok `/usage` did not report a limit within {DIALOG_TIMEOUT:?}"))
}

/// Start-up + every 30 min.
pub fn spawn_grok_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            match refresh_grok(&app).await {
                Ok(true) => {}
                Ok(false) => tracing::info!("grok not installed locally; grok quota stays null"),
                Err(e) => tracing::warn!(error = %e, "grok quota refresh failed"),
            }
            tokio::time::sleep(GROK_POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
