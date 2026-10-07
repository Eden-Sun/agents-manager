//! Claude Code quota + login via one throwaway herdr pane running `auth status --json` and
//! `-p "/usage"` (statusLine only updates while a bot chats). A pane, not ssh: non-login ssh
//! can't read the macOS Keychain and wrongly reports `loggedIn: false`.
//! Sonnet/Opus 週列忽略；[`parse_claude_usage`] 仍認舊 TUI 對話框版面（有人手動貼畫面來測）。
//! Each identity is probed separately (empty-env / `cc0` shares the bare `claude` key).

use crate::config::{expand_home, LOCAL_HOST};
use crate::herdr::HerdrClient;
use crate::quota::{Quota, Window};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub const CLAUDE_POLL: Duration = Duration::from_secs(60);

/// `-p` starts a real session; a cold start on a busy host takes seconds.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(40);

const PROBE_SESSION: &str = "am-quota";
pub const PROBE_LABEL_PREFIX: &str = "am-quota-claude";

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

fn parse_used_pct(line: &str) -> Option<f64> {
    let low = line.to_ascii_lowercase();
    if let Some(at) = low.find("% used") {
        let head = &line[..at];
        let num: String = head.chars().rev().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if !num.is_empty() {
            return num.chars().rev().collect::<String>().parse().ok();
        }
    }
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
/// 括號裡的時區標註一定要看得懂（IANA 名稱），才據此換算；daemon host 常常跟橫幅標的時區不同
/// （代管別台機器上的 bot），認不出來就回 `None`，不要靜靜套用本機時區猜一個可能錯的時間（issue #59）。
pub fn parse_claude_reset(line: &str, now: DateTime<Local>) -> Option<String> {
    let rest = line
        .trim()
        .strip_prefix("Resets")
        .or_else(|| line.trim().strip_prefix("resets"))?
        .trim()
        .trim_start_matches(':')
        .trim();
    let (rest, tz_name) = rest.split_once('(')?;
    let tz_name = tz_name.trim().trim_end_matches(')').trim();
    let tz: chrono_tz::Tz = tz_name.parse().ok()?;
    let rest = rest.trim();
    let now = now.with_timezone(&tz);

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
            let build = |y: i32| -> Option<DateTime<chrono_tz::Tz>> {
                let d = NaiveDate::from_ymd_opt(y, month, day)?.and_hms_opt(h, m, 0)?;
                tz.from_local_datetime(&d).earliest()
            };
            let this = build(now.year())?;
            if this < now - chrono::Duration::days(1) {
                build(now.year() + 1)?
            } else {
                this
            }
        }
        _ => {
            // Time-only: today, or tomorrow if that time already passed (both in `tz`, not host local).
            let today = now.date_naive().and_hms_opt(h, m, 0)?;
            let mut dt = tz.from_local_datetime(&today).earliest()?;
            if dt <= now {
                dt += chrono::Duration::days(1);
            }
            dt
        }
    };
    Some(dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// 切在第一個 `:` 前，純文字與對話框兩種版面都認得（`9:59pm` 在後面切不到）。
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

fn resets_tail(line: &str) -> Option<&str> {
    let at = line.to_ascii_lowercase().find("resets")?;
    Some(&line[at..])
}

/// `None` when the plan lines are not there (logged out, or not drawn yet).
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
            let w = Window { observed_at: None, used_pct, resets_at };
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
            let w = Window { observed_at: None, used_pct, resets_at: resets };
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
        limit_hit: None,
        plan: None,
        updated_at: crate::db::now(),
        source: "claude-usage".into(),
        account: account.map(String::from),
        host: LOCAL_HOST.into(),
    })
}

/// Local uses the dedicated `am-quota` session (SPEC §12.6); a remote host has only one
/// forwarded socket (the daemon's session), and the probe workspace isn't in the DB so reconcile ignores it.
pub async fn client_for_fence(host: &str, fence: &crate::hosts::HostFence) -> Result<HerdrClient> {
    if host == LOCAL_HOST {
        return probe_client().await;
    }
    let conn = fence.conn();
    if !conn.is_connected() {
        return Err(anyhow!("host `{host}` is not connected"));
    }
    Ok(conn.client.clone())
}

async fn probe_client() -> Result<HerdrClient> {
    let client = HerdrClient::new(HerdrClient::session_socket(PROBE_SESSION));
    if client.ping().await.is_ok() {
        return Ok(client);
    }
    let child = std::process::Command::new("/bin/sh")
        .arg("-lc")
        .arg(format!("exec herdr --session {PROBE_SESSION} server"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("could not start the `{PROBE_SESSION}` herdr session: {e}"))?;
    crate::local_sh::reap_in_background(child);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if client.ping().await.is_ok() {
            return Ok(client);
        }
    }
    Err(anyhow!("the `{PROBE_SESSION}` herdr session did not come up"))
}

pub async fn sweep_stale(app: &(impl crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess + crate::shared_host::SharedHostEnv)) {
    if let Ok(c) = probe_client().await {
        sweep_probes(&c, None, "stale").await;
    }
    sweep_stale_on_hosts(app).await;
}

/// 主 session 上的探測殘留（遠端主機的探測借它開）。共用 session 的主機只清帶本 daemon 標記的（#709）。
pub async fn sweep_stale_on_hosts(app: &(impl crate::capabilities::HerdrRoutes + crate::hosts::HostsAccess + crate::shared_host::SharedHostEnv)) {
    for host in crate::quota::pollable_hosts(app).await {
        if let Some(c) = app.herdr_for(&host).await {
            let tag = crate::shared_host::probe_tag(app, &host).await;
            sweep_probes(&c, tag.as_deref(), "stale").await;
            sweep_probes_with(&c, tag.as_deref(), "stale", crate::quota_agy::is_probe_label).await;
        }
    }
}

struct Probe {
    client: HerdrClient,
    workspace_id: String,
    closed: bool,
    /// 登記期間 `pane.agent_detected` 不觸發整台主機的對帳（[`crate::probe_ws`]）。
    _registered: crate::probe_ws::ProbeWorkspace,
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

/// 標記拆成 `printf` 參數，shell 回顯的指令才不會長得像標記。
pub const AUTH_BEGIN: &str = "AM_AUTH_BEGIN";
pub const AUTH_END: &str = "AM_AUTH_END";
pub const USAGE_DONE: &str = "AM_USAGE_DONE=";

#[derive(Debug, Default)]
pub struct ProbeOutcome {
    /// `None` = 判不出來，不是「沒登入」。
    pub logged_in: Option<bool>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub quota: Option<Quota>,
}

pub async fn record_probe_identity(
    app: &Arc<impl crate::tools::ToolsEnv + 'static>,
    host: &str,
    name: &str,
    outcome: &ProbeOutcome,
    fence: &crate::hosts::HostFence,
) -> bool {
    crate::tools::record_identity_login_fenced(app, host, name, fence, outcome.logged_in, outcome.email.clone(), outcome.plan.clone()).await
}

/// `env` is spelled out before each call too, so the line is self-contained when read off screen.
pub fn probe_command(bin: &str, env: &BTreeMap<String, String>, with_usage: bool) -> String {
    let mut pfx = String::new();
    for (k, v) in env.iter().filter(|(k, _)| crate::tools::valid_env_name(k)) {
        pfx.push_str(&format!("{k}={} ", crate::hosts::sh_quote(v)));
    }
    // claude 2.1.274 起認得這個變數：`0` 讓第一個 non-interactive turn 不等 MCP server 連線。
    // 這支只問登入狀態跟 `/usage`，從不用到工具，MCP 起得慢或掛掉不該拖慢額度探測、甚至把探測
    // 拖到 timeout（issue #80）。只在這支 throwaway probe 命令裡加；一般 managed bot 的啟動指令
    // 是完全分開的路徑（`lifecycle/start.rs`），不會被這裡影響到，工具可用性維持原樣。放在
    // identity 自己的 env 之後：就算某個身分自己也設了同名變數，probe 要的 `0` 一律蓋過去。
    // 舊版 CLI（2.1.274 之前）不認得這個變數，當成一般環境變數忽略，行為跟現在一樣，安全。
    pfx.push_str("CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0 ");
    let b = crate::hosts::sh_quote(bin);
    let auth = crate::tools::CLAUDE_LOGIN_ARGS.join(" ");
    // `/usage` 先要結構化版本（claude 2.1.273 起：`usage_report` 帶 kind／percent／ISO resets_at／scope）。
    // `grep -m1` 沒抓到（舊 CLI 不認 `--output-format`、或還沒有這個欄位）才跑純文字版給 [`parse_claude_usage`]。
    // 只要登入答案的 target（[`Target::login_only`]）不跑 `/usage`：那一格的額度由裸 target 負責，多跑一次是純成本。
    let usage = if with_usage {
        format!(
            "{pfx}{b} -p '/usage' --output-format stream-json --verbose </dev/null 2>/dev/null | grep -m1 usage_report \
               || {pfx}{b} -p '/usage' </dev/null 2>&1; rc=$?; "
        )
    } else {
        "rc=0; ".to_string()
    };
    format!(
        "printf '\\nAM_AUTH_%s\\n' BEGIN; {pfx}{b} {auth} </dev/null 2>&1; \
         printf '\\nAM_AUTH_%s\\n' END; \
         {usage}printf '\\nAM_USAGE_%s=%s\\n' DONE \"$rc\""
    )
}

/// `/usage` 的結構化版本（claude 2.1.273 起）。`limits[]` 的 `kind` 才是分桶依據，不看顯示字串：
/// `session`→5h、`weekly_all`→7d、`weekly_scoped` 且 scope 是 Fable→Fable 桶。其餘 scoped 列（別的模型）
/// 沒有對應的桶，先忽略。時間直接用 `resets_at`，不必再解析「Sep 16 at 7:50am」這種跟語系綁在一起的字。
pub fn parse_claude_usage_report(segment: &str, account: Option<&str>) -> Option<Quota> {
    let line = segment.lines().find(|l| l.contains("\"usage_report\""))?;
    let start = line.find('{')?;
    let v: Value = serde_json::from_str(line[start..].trim_end()).ok()?;
    let limits = v.pointer("/usage_report/rate_limits/limits")?.as_array()?;
    let iso = |row: &Value| -> Option<String> {
        let s = row.get("resets_at")?.as_str()?;
        DateTime::parse_from_rfc3339(s).ok().map(|t| crate::db::iso_at(t.to_utc()))
    };
    let win = |row: &Value| -> Option<Window> {
        Some(Window { observed_at: None, used_pct: row.get("percent")?.as_f64()?.clamp(0.0, 100.0), resets_at: iso(row) })
    };
    let (mut five, mut seven, mut fable) = (None, None, None);
    for row in limits {
        match row.get("kind").and_then(Value::as_str) {
            Some("session") => five = win(row),
            Some("weekly_all") => seven = win(row),
            Some("weekly_scoped") => {
                let model = row.pointer("/scope/model/display_name").and_then(Value::as_str).unwrap_or_default();
                // 2.1.273 實機是 `Fable`；帶版號的 `Fable 5.1` 也要認，不然 `fable` 窗永遠是 None，
                // 撞限的保底時間與 `mission::pick` 的桶判斷都會退回猜測（review 2026-09-16「沒把握」一節）。
                if model.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case("fable")) {
                    fable = win(row);
                }
            }
            _ => {}
        }
    }
    if five.is_none() && seven.is_none() && fable.is_none() {
        return None;
    }
    Some(Quota {
        five_hour: five,
        seven_day: seven,
        fable,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: crate::db::now(),
        source: "claude-usage".into(),
        account: account.map(String::from),
        host: LOCAL_HOST.into(),
    })
}

/// Searched from the **end**: a retyped command leaves two runs in scrollback; only the last finished.
pub fn split_probe_output(out: &str) -> Option<(String, String)> {
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

/// 上一輪 `workspace.close` 失敗（ssh 斷一下）留下的 claude 探測 workspace：開新的之前先收，
/// 不然要等 daemon 重啟的 [`sweep_stale`] 才收，中間每一輪都可能再多留一個（#408）。
/// 呼叫端握著這台的 `probe_lock`，同一台同時只有一個 claude 探測在跑，所以此刻同前綴的一定是殘留。
async fn sweep_leftovers(client: &HerdrClient, label: &str, is_label: fn(&str) -> bool) {
    // #709：這一輪的 label 帶 `@<標記>`＝共用 session 的主機，只清同一個標記的（別顆 daemon 的探測可能正在跑）。
    sweep_probes_with(client, label.rsplit_once('@').map(|(_, tag)| tag), "leftover", is_label).await
}

async fn sweep_probes(client: &HerdrClient, own_tag: Option<&str>, what: &str) {
    sweep_probes_with(client, own_tag, what, is_probe_label).await
}

/// `is_label` 認「哪一種探測的 label」：claude 與 agy 的探測各自一個前綴、各自的鎖，不能互相把對方正在跑的 workspace 收掉。
pub(crate) async fn sweep_probes_with(client: &HerdrClient, own_tag: Option<&str>, what: &str, is_label: fn(&str) -> bool) {
    let list = match client.workspace_list().await {
        Ok(l) => l,
        Err(e) => {
            tracing::debug!(error = %e, "claude probe: could not list workspaces to sweep leftovers");
            return;
        }
    };
    for ws in list.iter().filter(|w| w.label.as_deref().is_some_and(|l| crate::shared_host::sweepable(l, own_tag, is_label))) {
        match client.workspace_close(&ws.workspace_id).await {
            Ok(()) => tracing::info!(workspace = %ws.workspace_id, what, "closed a claude quota probe left behind"),
            Err(e) => tracing::warn!(workspace = %ws.workspace_id, what, error = %e, "claude probe left behind not closed"),
        }
    }
}

/// 探測 workspace 的 label；共用 session 的主機上帶本 daemon 的標記（#709），清殘留時才分得出是誰的。
pub async fn probe_label(app: &impl crate::shared_host::SharedHostEnv, host: &str, account: Option<&str>) -> String {
    let base = match account {
        Some(a) if !a.is_empty() => format!("{PROBE_LABEL_PREFIX}-{a}"),
        _ => PROBE_LABEL_PREFIX.to_string(),
    };
    tagged_probe_label(app, host, base).await
}

/// 同上，agy 的探測（`quota_agy`）用自己的前綴。
pub async fn tagged_probe_label(app: &impl crate::shared_host::SharedHostEnv, host: &str, base: String) -> String {
    match crate::shared_host::probe_tag(app, host).await {
        Some(tag) => crate::shared_host::tagged_label(&base, &tag),
        None => base,
    }
}

fn is_probe_label(l: &str) -> bool {
    l == PROBE_LABEL_PREFIX || l.starts_with(&format!("{PROBE_LABEL_PREFIX}-"))
}

/// [`run_probe_pane`] 的結果：打完指令後讀到兩段輸出，或時間到了（帶最後一次讀到的畫面）。
#[derive(Debug)]
pub enum PaneRun {
    Done(String, String),
    TimedOut(String),
}

/// [`run_marked_pane`] 的結果：`done` 認得輸出、`early` 提早判定失敗（帶當時的畫面），或時間到了（帶最後一次讀到的畫面）。
#[derive(Debug)]
pub enum MarkedRun<T> {
    Done(T),
    Early(String),
    TimedOut(String),
}

/// 在 `client` 上開一個探測 workspace、打 `cmd`、等到 `done` 認得畫面、`early` 說可以提早放棄、或 `timeout`。不論成功、RPC 失敗、
/// 逾時，或整個 future 被丟棄（[`Probe`] 的 `Drop`），這個 workspace 都會關掉（#408）。`begin` 是指令印出的開頭標記：
/// 打完指令一直看不到它，就當 shell 吞掉了指令、重打（最多三次）。claude 與 agy 的探測共用這一段。
#[allow(clippy::too_many_arguments)]
pub async fn run_marked_pane<T>(
    client: &HerdrClient,
    cwd: &str,
    label: &str,
    env_json: Value,
    cmd: &str,
    timeout: Duration,
    begin: &str,
    is_label: fn(&str) -> bool,
    done: impl Fn(&str) -> Option<T>,
    early: impl Fn(&str) -> bool,
) -> Result<MarkedRun<T>> {
    sweep_leftovers(client, label, is_label).await;
    let (ws, pane) = client.workspace_create(cwd, label, env_json).await?;
    let probe = Probe {
        client: client.clone(),
        workspace_id: ws.workspace_id.clone(),
        closed: false,
        _registered: crate::probe_ws::ProbeWorkspace::register(client.socket_path(), &ws.workspace_id),
    };
    let pane_id = pane.pane_id.clone();

    // workspace.create can return before the shell takes input; a line typed too early is lost, so retype.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let mut sends = 1u32;
    let mut sent_at = tokio::time::Instant::now();
    send_line(client, &pane_id, cmd).await?;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(700)).await;
        last = client.pane_read(&pane_id, "recent_unwrapped", 400).await?.text;
        if let Some(found) = done(&last) {
            probe.close().await;
            return Ok(MarkedRun::Done(found));
        }
        if early(&last) {
            probe.close().await;
            return Ok(MarkedRun::Early(last));
        }
        if sends < 3 && !last.contains(begin) && sent_at.elapsed() > Duration::from_secs(5) {
            tracing::debug!(label, sends, "quota probe pane swallowed the command; retyping");
            let _ = send_line(client, &pane_id, cmd).await;
            sends += 1;
            sent_at = tokio::time::Instant::now();
        }
    }
    probe.close().await;
    Ok(MarkedRun::TimedOut(last))
}

/// claude 的探測：[`run_marked_pane`] 配上 `AM_AUTH_*`／`AM_USAGE_DONE` 標記。
pub async fn run_probe_pane(
    client: &HerdrClient,
    cwd: &str,
    label: &str,
    env_json: Value,
    cmd: &str,
    timeout: Duration,
) -> Result<PaneRun> {
    Ok(match run_marked_pane(client, cwd, label, env_json, cmd, timeout, AUTH_BEGIN, is_probe_label, split_probe_output, |_| false).await? {
        MarkedRun::Done((auth, usage)) => PaneRun::Done(auth, usage),
        MarkedRun::TimedOut(last) | MarkedRun::Early(last) => PaneRun::TimedOut(last),
    })
}

/// Unparsable timestamps count as stale.
/// 狀態列補不上的那一份（Fable 週窗、方案名、登入答案）最久多久要再問一次 `/usage`。
///
/// 為什麼需要它：狀態列**永遠不含 Fable**（SPEC §12.4），而底下那條「狀態列很新就別開 pane」的
/// 捷徑會讓有 bot 在講話的帳號**每一輪都被跳過**——cc0 每 30 秒就有一次狀態列，於是 daemon 重啟後
/// 那一格的 `fable` 再也填不回來（2026-09-16 使用者：「怎麼不 show fable 用量了」）。
/// 重啟前看得到只是因為 `quota::set` 會沿用舊的 `fable`，探到過就黏著。
pub const USAGE_REFRESH: Duration = Duration::from_secs(10 * 60);

/// `quota_key` → 上一次**成功**讀到 `/usage` 的時刻。只在記憶體：重啟就當沒問過，多開一次 pane
/// 是安全的方向（`backoff_map` 同理）。
fn usage_seen() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

pub fn mark_usage_seen(key: &str) {
    usage_seen().lock().unwrap().insert(key.to_string(), std::time::Instant::now());
}

pub fn usage_fresh(key: &str) -> bool {
    usage_fresh_at(key, std::time::Instant::now())
}

/// `now` 由呼叫端給：測試往後推時間，不必從 `Instant::now()` 往回減（開機不到十分鐘時會 panic）。
pub fn usage_fresh_at(key: &str, now: std::time::Instant) -> bool {
    usage_seen().lock().unwrap().get(key).is_some_and(|t| now.saturating_duration_since(*t) < USAGE_REFRESH)
}

/// 這一輪要不要**跳過**這把 key 的 `/usage` pane。
///
/// 登入答案已知、而且 `/usage` 自己那一份還新（[`USAGE_REFRESH`]）就跳過，**不看狀態列新不新**。
/// 有 bot 在講話的帳號，5h/7d 由狀態列即時補；安靜的帳號額度本來就不太動，十分鐘一次夠用。
/// 以前安靜帳號每 [`CLAUDE_POLL`]（60 秒）就開一個 `claude -p`：每一個都是完整 session，會跟同一個
/// config dir 裡的 bot 搶著換 OAuth token（refresh token 只能用一次，慢的那個會被登出），`/usage`
/// 背後的端點也很快就 429（2026-09-25 使用者：「m4p 的 cc1 一直被登出」）。
pub fn skip_usage_probe(login_known: bool, usage_fresh: bool) -> bool {
    login_known && usage_fresh
}

/// Short enough that a transient failure (herdr busy, TUI slow) heals on its own.
pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);
/// The CLI said logged out: only a login changes that, so back off harder.
pub const RETRY_WHEN_LOGGED_OUT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProbeEvidence {
    pub cli_says_logged_out: bool,
    /// Seen by this process only.
    pub reported_statusline: bool,
    /// Survives a daemon restart.
    pub has_live_run: bool,
    pub cooling_down: bool,
}

/// 只有我們自己最近的失敗能擋住一次探測；退避多久由 [`failure_backoff`] 照**現在**的證據算。
///
/// 以前「狀態列有在更新／有 run 在跑」直接無視退避：`/usage` 一壞（CLI 改格式、pane 卡住），一直有 bot
/// 在講話的帳號每 60 秒就開一個 pane、佔住 `probe_lock` 40 秒，grok 輪詢與 `?refresh=1` 一直排隊
/// （review 2026-09-16 M4）。證據的用處改成「把沒登入的 30 分鐘縮成 5 分鐘」，不是完全不退避——
/// 過時的「沒登入」一樣不會把帳號永遠停掉（m4p 的 cc1／cc2 當年就是這樣丟了額度）。
pub fn should_probe_identity(e: ProbeEvidence) -> bool {
    !e.cooling_down
}

pub fn failure_backoff(e: ProbeEvidence) -> Duration {
    if e.cli_says_logged_out && !e.reported_statusline && !e.has_live_run {
        RETRY_WHEN_LOGGED_OUT
    } else {
        RETRY_AFTER_FAILURE
    }
}

/// 一次失敗的探測：什麼時候，以及那時 CLI 說不說沒登入。
#[derive(Clone, Copy, Debug)]
struct Failure {
    at: std::time::Instant,
    cli_says_logged_out: bool,
}

/// In memory only: a restart costs one extra probe, the safe direction to err in.
fn backoff_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, Failure>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Failure>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// 退避長度照失敗那時的登入答案、加上**現在**的證據算（[`failure_backoff`]）：失敗之後才開始有 bot 在用的帳號，
/// 不必等滿沒登入的 30 分鐘。
pub fn cooling_down_at(key: &str, reported_statusline: bool, has_live_run: bool, now: std::time::Instant) -> bool {
    let mut m = backoff_map().lock().unwrap();
    let Some(f) = m.get(key).copied() else { return false };
    let e = ProbeEvidence { cli_says_logged_out: f.cli_says_logged_out, reported_statusline, has_live_run, cooling_down: false };
    if now.saturating_duration_since(f.at) < failure_backoff(e) {
        return true;
    }
    m.remove(key);
    false
}

pub fn cooling_down(key: &str, reported_statusline: bool, has_live_run: bool) -> bool {
    cooling_down_at(key, reported_statusline, has_live_run, std::time::Instant::now())
}

pub fn park(key: &str, cli_says_logged_out: bool) {
    backoff_map().lock().unwrap().insert(key.to_string(), Failure { at: std::time::Instant::now(), cli_says_logged_out });
}

pub fn unpark(key: &str) {
    backoff_map().lock().unwrap().remove(key);
}

/// After a login recheck, so the popover isn't wrong for the 30-minute logged-out park.
/// `shares_default`：這個身分用的是預設帳號（[`crate::quota::identity_shares_default`]），它的探測退避記在裸 `claude` 那把 key。
pub fn unpark_identity(host: &str, name: &str, shares_default: bool) {
    unpark(&crate::quota::quota_key(host, &format!("claude:{name}")));
    if shares_default {
        unpark(&crate::quota::quota_key(host, "claude"));
    }
}

pub struct Target {
    pub key: String,
    pub account: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Identity rows the `auth status` answer describes; the bare target speaks for every
    /// no-env identity (`cc0`), since that *is* the default account.
    pub names: Vec<String>,
    /// 裸的預設帳號也有：以前它「永不 park」，`/usage` 一壞就每 60 秒開一個 pane（review 2026-09-16 M4）。
    pub evidence: ProbeEvidence,
    /// 只問登入答案、不寫額度：env 不是空的、卻沒有自己 `CLAUDE_CONFIG_DIR` 的身分（`ANTHROPIC_API_KEY`、
    /// `ANTHROPIC_BASE_URL`…）。額度照規則記在裸 `claude`，但登入答案要帶**它自己的 env** 問——以前併進裸 target
    /// 用空 env 探，預設帳號的 email／方案被記到它名下（review 2026-09-16 L2）。`key` 是 `claude:<name>`，只給退避用。
    pub login_only: bool,
}

/// 停用又沒有 run 在跑的身份跳過探測。**還在跑的不跳**：那顆 bot 的額度使用者仍然需要看得到，
/// 停用只是「不要再挑它」，不是把正在用的東西弄瞎。
pub fn skip_disabled(disabled: &[String], live: &std::collections::BTreeSet<String>, name: &str) -> bool {
    disabled.iter().any(|d| d == name) && !live.contains(name)
}

/// 裸的預設帳號要不要跳過：它代表的每一個共用預設帳號的身分（`cc0`…）都停用了、都沒有 run，而且沒有任何
/// 不帶身分的 claude bot 在跑（那些也吃預設帳號）。沒有任何身分指到它時照探——那就是「沒命名的預設帳號」，
/// 沒有東西可以停用（SPEC §16.3b，review 2026-09-16 L4）。
pub fn skip_disabled_default(
    disabled: &[String],
    live: &std::collections::BTreeSet<String>,
    names: &[String],
    unnamed_bot_running: bool,
) -> bool {
    !names.is_empty() && !unnamed_bot_running && names.iter().all(|n| skip_disabled(disabled, live, n))
}

/// 這台主機上有沒有不帶身分、正在跑的 claude bot（它們用的就是預設帳號）。
pub async fn unnamed_claude_running(app: &impl crate::capabilities::Db, host: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        // run 實際的身分（issue #238）：記了就用它，沒記才用 bot 設定的。
        "SELECT COUNT(*) FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE r.state IN ('starting','running','stopping') AND p.host = ? AND b.deleted_at IS NULL
            AND b.kind = 'claude'
            AND TRIM(COALESCE(CASE WHEN r.runtime_identity IS NULL THEN b.identity ELSE r.runtime_identity END, '')) = ''",
    )
    .bind(host)
    .fetch_one(app.db())
    .await
    .map(|n| n > 0)
    // 讀不到就當有：寧可多探一次，也不要把正在用的預設帳號弄瞎。
    .unwrap_or(true)
}

/// 這一輪要探哪些 target（純函式，好測）。裸的預設帳號排第一個。
pub struct PlanInput<'a> {
    pub host: &'a str,
    pub home: &'a str,
    pub identities: &'a [crate::config::IdentityCfg],
    pub logins: &'a BTreeMap<String, crate::tools::IdentityInfo>,
    pub live: &'a std::collections::BTreeSet<String>,
    pub off: &'a [String],
    /// 這台有沒有不帶身分、正在跑的 claude bot。
    pub unnamed_running: bool,
    /// 完整 key：最近一次寫進來的是 statusLine。
    pub statusline_keys: &'a std::collections::BTreeSet<String>,
}

pub fn plan_targets(i: &PlanInput, cooling: impl Fn(&str, bool, bool) -> bool) -> Vec<Target> {
    let host = i.host;
    let mut targets: Vec<Target> = Vec::new();
    let mut bare_names = Vec::new();
    let mut rest: Vec<Target> = Vec::new();
    for id in i.identities.iter().filter(|x| x.kind == "claude") {
        let mut env = BTreeMap::new();
        for (k, v) in &id.env {
            env.insert(k.clone(), expand_home(v, i.home));
        }
        // 「這個身分就是預設帳號嗎」只能有一份規則：`env.is_empty()` 會把只帶
        // `ANTHROPIC_BASE_URL`（沒有 CLAUDE_CONFIG_DIR）的身分算成獨立帳號，跟 statusline 那邊
        // 的落點不一致，同一個帳號的數字會分裂在兩格（review 2026-09-16）。
        let shares_default = crate::quota::identity_shares_default("claude", &id.env);
        if shares_default && id.env.is_empty() {
            bare_names.push(id.name.clone());
            continue;
        }
        if skip_disabled(i.off, i.live, &id.name) {
            tracing::debug!(host, identity = %id.name, "identity is disabled and idle; skipping its quota probe");
            continue;
        }
        let key = format!("claude:{}", id.name);
        let full = crate::quota::quota_key(host, &key);
        let reported_statusline = i.statusline_keys.contains(&full);
        let has_live_run = i.live.contains(&id.name);
        let evidence = ProbeEvidence {
            cli_says_logged_out: i.logins.get(&id.name).map(|x| x.logged_in) == Some(Some(false)),
            reported_statusline,
            has_live_run,
            cooling_down: cooling(&full, reported_statusline, has_live_run),
        };
        if !should_probe_identity(evidence) {
            tracing::debug!(host, identity = %id.name, "identity probe is cooling down after a failure; skipping");
            continue;
        }
        rest.push(Target { key, account: Some(id.name.clone()), env, names: vec![id.name.clone()], evidence, login_only: shares_default });
    }
    if skip_disabled_default(i.off, i.live, &bare_names, i.unnamed_running) {
        tracing::debug!(host, identities = ?bare_names, "the default account's identities are all disabled and idle; skipping its quota probe");
    } else {
        let bare_full = crate::quota::quota_key(host, "claude");
        let reported_statusline = i.statusline_keys.contains(&bare_full);
        let has_live_run = i.unnamed_running || bare_names.iter().any(|n| i.live.contains(n));
        let evidence = ProbeEvidence {
            cli_says_logged_out: bare_names.iter().any(|n| i.logins.get(n).map(|x| x.logged_in) == Some(Some(false))),
            reported_statusline,
            has_live_run,
            cooling_down: cooling(&bare_full, reported_statusline, has_live_run),
        };
        if should_probe_identity(evidence) {
            targets.push(Target { key: "claude".into(), account: bare_names.first().cloned(), env: BTreeMap::new(), names: bare_names, evidence, login_only: false });
        } else {
            tracing::debug!(host, "default-account probe is cooling down after a failure; skipping");
        }
    }
    targets.append(&mut rest);
    targets
}

/// [`force_probe`] 失敗的種類：API 要分得出「沒這個身分」「沒裝 claude」「探了但沒讀到」。
#[derive(Debug)]
pub enum ForceProbeError {
    UnknownAccount,
    NotInstalled,
    Failed(String),
}

/// 強制探哪一個 target：`account` 省略＝裸的預設帳號；共用預設帳號的身分（含只問登入的那種）額度都記在裸 `claude`，
/// 所以也探裸的那一趟。
pub fn forced_target(targets: Vec<Target>, account: Option<&str>) -> Option<Target> {
    let bare = |t: &Target| t.key == "claude";
    let Some(name) = account else { return targets.into_iter().find(bare) };
    let own = targets.iter().position(|t| t.names.iter().any(|n| n == name))?;
    if targets[own].login_only {
        return targets.into_iter().find(bare);
    }
    targets.into_iter().nth(own)
}
