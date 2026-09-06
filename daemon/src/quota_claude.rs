//! Claude Code quota via a throwaway `/usage` pane (complements the statusLine push).
//!
//! StatusLine only updates while a bot is running and chatting. The user also wants a
//! background probe — same idea as [`crate::quota_grok`]: open a pane in the dedicated
//! `am-quota` herdr session, start claude, type `/usage`, parse the dialog, close the pane.
//!
//! The dialog (Claude Code 2.1.x) looks like:
//!
//! ```text
//! Current session
//! ███████████████████████████████████████            78% used
//! Resets 1:20pm (Asia/Taipei)
//!
//! Current week (all models)
//! ███████████████▌                                   31% used
//! Resets Sep 11 at 2pm (Asia/Taipei)
//!
//! Current week (Fable)
//! ███████████████████▌                               39% used
//! Resets Sep 11 at 2pm (Asia/Taipei)
//! ```
//!
//! `Current session` → `five_hour`; `Current week (all models)` → `seven_day`. Model-specific
//! weekly rows (Fable / Sonnet / Opus) are ignored so the strip keeps one weekly bar.
//!
//! Each configured Claude identity is probed separately (empty-env / `cc0` shares the bare
//! `claude` key with the default account).

use crate::config::{expand_home, LOCAL_HOST};
use crate::herdr::{AgentStatus, HerdrClient};
use crate::quota::{Quota, Window};
use crate::state::App;
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Claude TUI start + `/usage` is heavier than grok; one cycle per minute is enough for the strip.
pub const CLAUDE_POLL: Duration = Duration::from_secs(60);

const DIALOG_TIMEOUT: Duration = Duration::from_secs(25);

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

fn is_session_header(line: &str) -> bool {
    let t = line.trim().to_ascii_lowercase();
    t == "current session" || t.starts_with("current session ")
}

fn is_week_all_models_header(line: &str) -> bool {
    let t = line.trim().to_ascii_lowercase();
    t.starts_with("current week") && (t.contains("all models") || t == "current week")
}

/// The whole `/usage` screen → Quota, or `None` when the plan bars are not on screen yet.
pub fn parse_claude_usage(screen: &str, now: DateTime<Local>, account: Option<&str>) -> Option<Quota> {
    let lines: Vec<String> = screen.lines().map(clean).filter(|l| !l.is_empty()).collect();
    let mut five: Option<Window> = None;
    let mut seven: Option<Window> = None;
    let mut i = 0;
    while i < lines.len() {
        let header = &lines[i];
        let kind = if is_session_header(header) {
            Some("five")
        } else if is_week_all_models_header(header) {
            Some("seven")
        } else {
            None
        };
        if kind.is_none() {
            i += 1;
            continue;
        }
        let mut pct = None;
        let mut resets = None;
        let mut j = i + 1;
        while j < lines.len() && j < i + 6 {
            if is_session_header(&lines[j]) || lines[j].to_ascii_lowercase().starts_with("current week") {
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
                Some("five") => five = Some(w),
                Some("seven") => seven = Some(w),
                _ => {}
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
        plan: None,
        updated_at: crate::db::now(),
        source: "claude-usage".into(),
        account: account.map(String::from),
    })
}

// ------------------------------------------------------------------ probe

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
    if let Some(c) = app.herdr_for(LOCAL_HOST).await {
        clients.push(c);
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

/// One account probe. `env` is the pane env (e.g. `CLAUDE_CONFIG_DIR` for cc1).
/// Caller must hold [`crate::quota::probe_lock`].
async fn refresh_claude_account(
    app: &Arc<App>,
    kind_key: &str,
    account: Option<&str>,
    env: BTreeMap<String, String>,
) -> Result<bool> {
    if !app.tools.lock().await.contains_key(LOCAL_HOST) {
        crate::tools::detect(app, LOCAL_HOST).await?;
    }
    if crate::tools::cached_path(app, LOCAL_HOST, "claude").await.is_none() {
        return Ok(false);
    }

    let client = probe_client().await?;
    // Prefer a cwd Claude already trusts; $HOME can trip the first-run workspace trust dialog
    // (agent stays `blocked`) for a fresh CLAUDE_CONFIG_DIR like cc1.
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .or_else(|| dirs::home_dir().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "/tmp".into());
    let label = match account {
        Some(a) if !a.is_empty() => format!("{PROBE_LABEL_PREFIX}-{a}"),
        _ => PROBE_LABEL_PREFIX.to_string(),
    };
    let env_json = Value::Object(env.into_iter().map(|(k, v)| (k, json!(v))).collect());
    let (ws, pane) = client.workspace_create(&cwd, &label, env_json).await?;
    let mut probe = Probe { client: client.clone(), workspace_id: ws.workspace_id.clone(), closed: false };
    let pane_id = pane.pane_id.clone();

    // workspace.create returns before the interactive shell is always ready for agent.start.
    let mut started = None;
    let name = format!("amquota{}", ulid::Ulid::new().to_string()[20..].to_ascii_lowercase());
    for attempt in 0..10u32 {
        match client.agent_start(&name, "claude", &pane_id, &[], 90_000).await {
            Ok(a) => {
                started = Some(a);
                break;
            }
            Err(e) if e.to_string().contains("agent_pane_busy") || e.to_string().contains("not an available shell") => {
                tracing::debug!(attempt, error = %e, "claude probe pane not ready yet");
                tokio::time::sleep(Duration::from_millis(500 + 250 * u64::from(attempt))).await;
            }
            Err(e) => {
                probe.close().await;
                return Err(e);
            }
        }
    }
    if started.is_none() {
        probe.close().await;
        return Err(anyhow!("claude probe pane never became an available shell"));
    }
    // Idle only — `blocked` is usually the workspace-trust / login dialog, not a usable TUI.
    match client.agent_wait(&name, &[AgentStatus::Idle], 90_000).await {
        Ok(_) => {}
        Err(e) => {
            // Try dismissing a trust prompt, then wait for idle once more.
            tracing::debug!(error = %e, "claude probe not idle; trying to clear a dialog");
            let _ = client.pane_send_keys(&pane_id, &["Enter"]).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
            if let Err(e2) = client.agent_wait(&name, &[AgentStatus::Idle], 30_000).await {
                probe.close().await;
                return Err(e2);
            }
        }
    }
    // Let the TUI finish drawing its input line before the slash command.
    tokio::time::sleep(Duration::from_secs(3)).await;
    client.pane_send_text(&pane_id, "/usage").await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    client.pane_send_keys(&pane_id, &["Enter"]).await?;

    let deadline = tokio::time::Instant::now() + DIALOG_TIMEOUT;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(900)).await;
        last = client.pane_read(&pane_id, "visible", 120).await.map(|r| r.text).unwrap_or_default();
        if let Some(q) = parse_claude_usage(&last, Local::now(), account) {
            crate::quota::set(app, kind_key, q).await;
            probe.close().await;
            return Ok(true);
        }
        // Dialog sometimes needs a second Enter if the slash menu only highlighted Usage.
        if last.to_ascii_lowercase().contains("usage") && !last.to_ascii_lowercase().contains("% used") {
            let _ = client.pane_send_keys(&pane_id, &["Enter"]).await;
        }
    }
    probe.close().await;
    tracing::debug!(account = ?account, screen = %last, "claude /usage did not render plan bars");
    Err(anyhow!("claude `/usage` did not report plan limits within {DIALOG_TIMEOUT:?}"))
}

/// Default account + every Claude identity with a distinct config dir.
pub async fn refresh_claude(app: &Arc<App>) -> Result<bool> {
    let _guard = crate::quota::probe_lock().await;
    let home = dirs::home_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "/tmp".into());
    let cfg = app.cfg.get().await;
    let mut targets: Vec<(String, Option<String>, BTreeMap<String, String>)> = Vec::new();

    // Bare default account (also covers empty-env identities like cc0).
    targets.push(("claude".into(), None, BTreeMap::new()));

    for id in cfg.identities.iter().filter(|i| i.kind == "claude") {
        let mut env = BTreeMap::new();
        for (k, v) in &id.env {
            env.insert(k.clone(), expand_home(v, &home));
        }
        if env.is_empty() {
            // Same credentials as the default account — strip already maps cc0 → `claude`.
            continue;
        }
        targets.push((format!("claude:{}", id.name), Some(id.name.clone()), env));
    }

    let mut any = false;
    let mut saw_missing = false;
    for (key, account, env) in targets {
        match refresh_claude_account(app, &key, account.as_deref(), env).await {
            Ok(true) => any = true,
            Ok(false) => saw_missing = true,
            Err(e) => tracing::warn!(key = %key, error = %e, "claude quota refresh failed"),
        }
    }
    if saw_missing && !any {
        return Ok(false);
    }
    Ok(any || !saw_missing)
}

pub fn spawn_claude_poller(app: Arc<App>) {
    tokio::spawn(async move {
        sweep_stale(&app).await;
        loop {
            match refresh_claude(&app).await {
                Ok(true) => {}
                Ok(false) => tracing::info!("claude not installed locally; claude quota stays null"),
                Err(e) => tracing::warn!(error = %e, "claude quota refresh failed"),
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

    fn at(s: &str) -> DateTime<Local> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Local)
    }

    #[test]
    fn session_and_week_all_models_map() {
        let q = parse_claude_usage(SCREEN, at("2026-09-06T10:00:00+08:00"), Some("cc0")).unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 78.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 31.0);
        // Fable-only weekly must not overwrite all-models.
        assert_ne!(q.seven_day.as_ref().unwrap().used_pct, 39.0);
        assert_eq!(q.source, "claude-usage");
        assert_eq!(q.account.as_deref(), Some("cc0"));
        let five_reset = q.five_hour.as_ref().unwrap().resets_at.as_deref().unwrap();
        assert!(five_reset.contains("T05:20:00") || five_reset.contains("T13:20:00"), "{five_reset}");
        let week_reset = q.seven_day.as_ref().unwrap().resets_at.as_deref().unwrap();
        assert!(week_reset.starts_with("2026-09-11"), "{week_reset}");
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
}
