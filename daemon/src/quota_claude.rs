//! Claude Code quota + login via one throwaway herdr pane running `auth status --json` and
//! `-p "/usage"` (statusLine only updates while a bot chats). A pane, not ssh: non-login ssh
//! can't read the macOS Keychain and wrongly reports `loggedIn: false`.
//! Sonnet/Opus 週列忽略；[`parse_claude_usage`] 仍認舊 TUI 對話框版面（有人手動貼畫面來測）。
//! Each identity is probed separately (empty-env / `cc0` shares the bare `claude` key).

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

pub const CLAUDE_POLL: Duration = Duration::from_secs(60);

/// `-p` starts a real session; a cold start on a busy host takes seconds.
const PROBE_TIMEOUT: Duration = Duration::from_secs(40);

const PROBE_SESSION: &str = "am-quota";
const PROBE_LABEL_PREFIX: &str = "am-quota-claude";

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

/// 標記拆成 `printf` 參數，shell 回顯的指令才不會長得像標記。
const AUTH_BEGIN: &str = "AM_AUTH_BEGIN";
const AUTH_END: &str = "AM_AUTH_END";
const USAGE_DONE: &str = "AM_USAGE_DONE=";

#[derive(Debug, Default)]
pub(crate) struct ProbeOutcome {
    /// `None` = 判不出來，不是「沒登入」。
    pub logged_in: Option<bool>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub quota: Option<Quota>,
}

/// `env` is spelled out before each call too, so the line is self-contained when read off screen.
fn probe_command(bin: &str, env: &BTreeMap<String, String>, with_usage: bool) -> String {
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
        Some(Window { used_pct: row.get("percent")?.as_f64()?.clamp(0.0, 100.0), resets_at: iso(row) })
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

/// `Ok(None)` = claude not installed. Caller must hold [`crate::quota::probe_lock`] for that host.
async fn refresh_claude_account(
    app: &Arc<App>,
    host: &str,
    base_key: &str,
    account: Option<&str>,
    env: BTreeMap<String, String>,
    with_usage: bool,
) -> Result<Option<ProbeOutcome>> {
    if !app.tools.lock().await.contains_key(host) {
        crate::tools::detect(app, host).await?;
    }
    let Some(bin) = crate::tools::cached_path(app, host, "claude").await else {
        return Ok(None);
    };

    let client = client_for(app, host).await?;
    // A sane cwd keeps the session's project files out of `/`.
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
    let cmd = probe_command(&bin, &env, with_usage);

    // workspace.create can return before the shell takes input; a line typed too early is lost, so retype.
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
    if !with_usage {
        return Ok(Some(out));
    }
    let parsed = parse_claude_usage_report(&usage, account).or_else(|| parse_claude_usage(&usage, Local::now(), account));
    if let Some(mut q) = parsed {
        q.plan = plan;
        crate::quota::set(app, host, base_key, q.clone()).await;
        out.quota = Some(q);
    } else {
        tracing::debug!(host, account = ?account, logged_in = ?out.logged_in, usage = %usage.trim(), "claude `/usage` reported no plan lines");
    }
    Ok(Some(out))
}

/// Unparsable timestamps count as stale.
/// 狀態列補不上的那一份（Fable 週窗、方案名、登入答案）最久多久要再問一次 `/usage`。
///
/// 為什麼需要它：狀態列**永遠不含 Fable**（SPEC §12.4），而底下那條「狀態列很新就別開 pane」的
/// 捷徑會讓有 bot 在講話的帳號**每一輪都被跳過**——cc0 每 30 秒就有一次狀態列，於是 daemon 重啟後
/// 那一格的 `fable` 再也填不回來（2026-09-16 使用者：「怎麼不 show fable 用量了」）。
/// 重啟前看得到只是因為 `quota::set` 會沿用舊的 `fable`，探到過就黏著。
const USAGE_REFRESH: Duration = Duration::from_secs(10 * 60);

/// `quota_key` → 上一次**成功**讀到 `/usage` 的時刻。只在記憶體：重啟就當沒問過，多開一次 pane
/// 是安全的方向（`backoff_map` 同理）。
fn usage_seen() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn mark_usage_seen(key: &str) {
    usage_seen().lock().unwrap().insert(key.to_string(), std::time::Instant::now());
}

fn usage_fresh(key: &str) -> bool {
    usage_fresh_at(key, std::time::Instant::now())
}

/// `now` 由呼叫端給：測試往後推時間，不必從 `Instant::now()` 往回減（開機不到十分鐘時會 panic）。
fn usage_fresh_at(key: &str, now: std::time::Instant) -> bool {
    usage_seen().lock().unwrap().get(key).is_some_and(|t| now.saturating_duration_since(*t) < USAGE_REFRESH)
}

/// 這一輪要不要**跳過**這把 key 的 `/usage` pane。
///
/// 只有兩件事都成立才跳過：①狀態列剛送過同樣那幾個窗（開 pane 重讀是純成本），②`/usage` 自己
/// 的那一份還新。少了②，有 bot 在講話的帳號就永遠問不到 Fable 與方案——省下的成本是「那格資料
/// 再也不會出現」，不是省。
pub(crate) fn skip_probe_for_fresh_statusline(login_known: bool, statusline_fresh: bool, usage_fresh: bool) -> bool {
    login_known && statusline_fresh && usage_fresh
}

fn fresher_than(updated_at: &str, window: Duration) -> bool {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(updated_at) else { return false };
    match chrono::Utc::now().signed_duration_since(t.with_timezone(&chrono::Utc)).to_std() {
        Ok(age) => age < window,
        // Negative age (clock skew) — treat as fresh rather than probing on every cycle.
        Err(_) => true,
    }
}

/// Short enough that a transient failure (herdr busy, TUI slow) heals on its own.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5 * 60);
/// The CLI said logged out: only a login changes that, so back off harder.
const RETRY_WHEN_LOGGED_OUT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ProbeEvidence {
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
pub(crate) fn should_probe_identity(e: ProbeEvidence) -> bool {
    !e.cooling_down
}

pub(crate) fn failure_backoff(e: ProbeEvidence) -> Duration {
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
fn cooling_down_at(key: &str, reported_statusline: bool, has_live_run: bool, now: std::time::Instant) -> bool {
    let mut m = backoff_map().lock().unwrap();
    let Some(f) = m.get(key).copied() else { return false };
    let e = ProbeEvidence { cli_says_logged_out: f.cli_says_logged_out, reported_statusline, has_live_run, cooling_down: false };
    if now.saturating_duration_since(f.at) < failure_backoff(e) {
        return true;
    }
    m.remove(key);
    false
}

fn cooling_down(key: &str, reported_statusline: bool, has_live_run: bool) -> bool {
    cooling_down_at(key, reported_statusline, has_live_run, std::time::Instant::now())
}

fn park(key: &str, cli_says_logged_out: bool) {
    backoff_map().lock().unwrap().insert(key.to_string(), Failure { at: std::time::Instant::now(), cli_says_logged_out });
}

fn unpark(key: &str) {
    backoff_map().lock().unwrap().remove(key);
}

/// After a login recheck, so the popover isn't wrong for the 30-minute logged-out park.
pub fn unpark_identity(host: &str, name: &str) {
    unpark(&crate::quota::quota_key(host, &format!("claude:{name}")));
}

struct Target {
    key: String,
    account: Option<String>,
    env: BTreeMap<String, String>,
    /// Identity rows the `auth status` answer describes; the bare target speaks for every
    /// no-env identity (`cc0`), since that *is* the default account.
    names: Vec<String>,
    /// 裸的預設帳號也有：以前它「永不 park」，`/usage` 一壞就每 60 秒開一個 pane（review 2026-09-16 M4）。
    evidence: ProbeEvidence,
    /// 只問登入答案、不寫額度：env 不是空的、卻沒有自己 `CLAUDE_CONFIG_DIR` 的身分（`ANTHROPIC_API_KEY`、
    /// `ANTHROPIC_BASE_URL`…）。額度照規則記在裸 `claude`，但登入答案要帶**它自己的 env** 問——以前併進裸 target
    /// 用空 env 探，預設帳號的 email／方案被記到它名下（review 2026-09-16 L2）。`key` 是 `claude:<name>`，只給退避用。
    login_only: bool,
}

/// 停用又沒有 run 在跑的身份跳過探測。**還在跑的不跳**：那顆 bot 的額度使用者仍然需要看得到，
/// 停用只是「不要再挑它」，不是把正在用的東西弄瞎。
fn skip_disabled(disabled: &[String], live: &std::collections::BTreeSet<String>, name: &str) -> bool {
    disabled.iter().any(|d| d == name) && !live.contains(name)
}

/// 裸的預設帳號要不要跳過：它代表的每一個共用預設帳號的身分（`cc0`…）都停用了、都沒有 run，而且沒有任何
/// 不帶身分的 claude bot 在跑（那些也吃預設帳號）。沒有任何身分指到它時照探——那就是「沒命名的預設帳號」，
/// 沒有東西可以停用（SPEC §16.3b，review 2026-09-16 L4）。
fn skip_disabled_default(
    disabled: &[String],
    live: &std::collections::BTreeSet<String>,
    names: &[String],
    unnamed_bot_running: bool,
) -> bool {
    !names.is_empty() && !unnamed_bot_running && names.iter().all(|n| skip_disabled(disabled, live, n))
}

/// 這台主機上有沒有不帶身分、正在跑的 claude bot（它們用的就是預設帳號）。
async fn unnamed_claude_running(app: &Arc<App>, host: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        // run 實際的身分（issue #238）：記了就用它，沒記才用 bot 設定的。
        "SELECT COUNT(*) FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE r.state IN ('starting','running','stopping') AND p.host = ? AND b.deleted_at IS NULL
            AND b.kind = 'claude'
            AND TRIM(COALESCE(CASE WHEN r.runtime_identity IS NULL THEN b.identity ELSE r.runtime_identity END, '')) = ''",
    )
    .bind(host)
    .fetch_one(&app.db)
    .await
    .map(|n| n > 0)
    // 讀不到就當有：寧可多探一次，也不要把正在用的預設帳號弄瞎。
    .unwrap_or(true)
}

/// 這一輪要探哪些 target（純函式，好測）。裸的預設帳號排第一個。
struct PlanInput<'a> {
    host: &'a str,
    home: &'a str,
    identities: &'a [crate::config::IdentityCfg],
    logins: &'a BTreeMap<String, crate::tools::IdentityInfo>,
    live: &'a std::collections::BTreeSet<String>,
    off: &'a [String],
    /// 這台有沒有不帶身分、正在跑的 claude bot。
    unnamed_running: bool,
    /// 完整 key：最近一次寫進來的是 statusLine。
    statusline_keys: &'a std::collections::BTreeSet<String>,
}

fn plan_targets(i: &PlanInput, cooling: impl Fn(&str, bool, bool) -> bool) -> Vec<Target> {
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

pub async fn refresh_claude(app: &Arc<App>, host: &str) -> Result<bool> {
    let _guard = crate::quota::probe_lock(host).await;
    // `~` expands against the *probed* host's home, not the daemon's.
    let home = host_home(app, host).await;
    // `ccN` are per host (SPEC §16): cc1 on m4p is a different account than here.
    let identities = crate::tools::identities_for_host(app, host).await;
    let logins = app.tools.lock().await.get(host).map(|t| t.identities.clone()).unwrap_or_default();
    // DB-backed so a daemon restart doesn't lose the proof.
    let live = crate::db::live_identities_on_host(&app.db, host).await.unwrap_or_default();
    // 停用的身份不上額度條（使用者 2026-09-16），所以也不必再花探測去問它。
    let off = crate::mission::store::disabled_identities(&app.db, host, "claude").await.unwrap_or_default();
    let unnamed_running = unnamed_claude_running(app, host).await;
    let statusline_keys: std::collections::BTreeSet<String> =
        app.quotas.lock().await.iter().filter(|(_, q)| q.source == "statusline").map(|(k, _)| k.clone()).collect();
    let targets = plan_targets(
        &PlanInput { host, home: &home, identities: &identities, logins: &logins, live: &live, off: &off, unnamed_running, statusline_keys: &statusline_keys },
        cooling_down,
    );

    let mut any = false;
    let mut saw_missing = false;
    let mut touched = false;
    for t in targets {
        let full = crate::quota::quota_key(host, &t.key);
        // Fresh statusLine = skip (many `ccN` per host, SPEC §16, would queue probes), unless
        // we've never had a login answer for it — that rides on the probe.
        let login_known = t.names.iter().all(|n| logins.get(n).is_some_and(|i| i.logged_in.is_some()));
        if t.login_only && login_known {
            continue;
        }
        let statusline_fresh = app
            .quotas
            .lock()
            .await
            .get(&full)
            .is_some_and(|q| q.source == "statusline" && fresher_than(&q.updated_at, CLAUDE_POLL));
        if skip_probe_for_fresh_statusline(login_known, statusline_fresh, usage_fresh(&full)) {
            any = true;
            continue;
        }
        match refresh_claude_account(app, host, &t.key, t.account.as_deref(), t.env, !t.login_only).await {
            Ok(None) => saw_missing = true,
            Ok(Some(o)) => {
                for name in &t.names {
                    touched |= crate::tools::record_identity_login(app, host, name, o.logged_in, o.email.clone(), o.plan.clone()).await;
                }
                if t.login_only {
                    if o.logged_in.is_some() {
                        unpark(&full);
                    } else {
                        park(&full, false);
                    }
                } else if o.quota.is_some() {
                    any = true;
                    unpark(&full);
                    // 這把 key 的 `/usage` 剛答過：接下來十分鐘讓狀態列接手，不用再開 pane。
                    mark_usage_seen(&full);
                } else {
                    // No plan lines: park it using the login answer we just got.
                    let logged_out = o.logged_in == Some(false);
                    park(&full, logged_out);
                    let retry = failure_backoff(ProbeEvidence { cli_says_logged_out: logged_out, ..t.evidence });
                    tracing::info!(host, key = %t.key, logged_in = ?o.logged_in, retry_in_s = retry.as_secs(), "claude reported no plan lines; parking this account");
                }
            }
            Err(e) => {
                park(&full, t.evidence.cli_says_logged_out);
                let retry = failure_backoff(t.evidence);
                tracing::warn!(host, key = %t.key, error = %e, retry_in_s = retry.as_secs(), "claude quota probe failed; parking this account");
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

pub fn spawn_claude_poller(app: Arc<App>) {
    tokio::spawn(async move {
        sweep_stale(&app).await;
        loop {
            crate::quota::for_each_host(crate::quota::pollable_hosts(&app).await, |host| {
                let app = app.clone();
                async move {
                    match refresh_claude(&app, &host).await {
                        Ok(true) => {}
                        Ok(false) => tracing::info!(host = %host, "claude not installed; claude quota stays null"),
                        Err(e) => tracing::warn!(host = %host, error = %e, "claude quota refresh failed"),
                    }
                }
            })
            .await;
            tokio::time::sleep(CLAUDE_POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 停用的身份不再探測，但**還在跑的例外**：停用是「別再挑它」，不是把正在用的額度弄瞎。
    #[test]
    fn a_disabled_identity_is_only_skipped_while_nothing_is_running_on_it() {
        let off = vec!["cc2".to_string()];
        let idle: std::collections::BTreeSet<String> = Default::default();
        let busy: std::collections::BTreeSet<String> = ["cc2".to_string()].into_iter().collect();
        assert!(skip_disabled(&off, &idle, "cc2"));
        assert!(!skip_disabled(&off, &busy, "cc2"), "還有 run 在跑就照探");
        assert!(!skip_disabled(&off, &idle, "cc1"), "沒被停用的不受影響");
        assert!(!skip_disabled(&[], &idle, "cc2"), "沒人被停用時什麼都不跳");
    }

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

    /// claude 2.1.273 的 `/usage --output-format stream-json`：分桶看 `kind`、時間直接用 ISO，
    /// 不再解析「Sep 16 at 7:50am」。實機抓到的那一行（只留這段會用到的欄位）。
    #[test]
    fn the_structured_usage_report_fills_every_window() {
        let line = r#"{"type":"assistant","local_command_source":"<local-command-stdout>Current session: 30% used</local-command-stdout>","usage_report":{"session":{"total_cost_usd":0},"rate_limits":{"limits":[
          {"kind":"session","group":"session","percent":30,"resets_at":"2026-09-15T23:50:00.594923+00:00","scope":null,"severity":"normal","is_active":false},
          {"kind":"weekly_all","group":"weekly","percent":66,"resets_at":"2026-09-21T04:00:00.594948+00:00","scope":null,"severity":"normal","is_active":true},
          {"kind":"weekly_scoped","group":"weekly","percent":24,"resets_at":"2026-09-21T03:59:59.595145+00:00","scope":{"model":{"display_name":"Fable"},"surface":null},"severity":"normal","is_active":false},
          {"kind":"weekly_scoped","group":"weekly","percent":9,"resets_at":"2026-09-21T03:59:59.595145+00:00","scope":{"model":{"display_name":"Opus"},"surface":null},"severity":"normal","is_active":false}]}}}"#;
        let screen = format!("$ claude -p '/usage' --output-format stream-json
{}
AM_USAGE_DONE=0
", line.replace('\n', " "));
        let q = parse_claude_usage_report(&screen, Some("cc1")).expect("structured report");
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 30.0);
        // 毫秒不再被截掉：以前 `Secs` 會把 `.594` 無條件捨去（總是往**早**的方向偏）。
        assert_eq!(q.five_hour.as_ref().unwrap().resets_at.as_deref(), Some("2026-09-15T23:50:00.594Z"));
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 66.0);
        assert_eq!(q.fable.as_ref().unwrap().used_pct, 24.0, "weekly_scoped 的 Fable 列");
        assert_eq!(q.fable.as_ref().unwrap().resets_at.as_deref(), Some("2026-09-21T03:59:59.595Z"));
        assert_eq!(q.account.as_deref(), Some("cc1"));
        assert_eq!(q.source, "claude-usage");
    }

    /// Fable 列的 `display_name` 帶版號（`Fable 5.1`）也要認；別的模型（`Opus`）不能被當成 Fable。
    #[test]
    fn a_versioned_fable_name_still_fills_the_fable_window() {
        let row = |name: &str| format!(
            r#"{{"usage_report":{{"rate_limits":{{"limits":[{{"kind":"weekly_scoped","percent":41,"resets_at":"2026-09-21T03:59:59+00:00","scope":{{"model":{{"display_name":"{name}"}}}}}}]}}}}}}"#
        );
        assert_eq!(parse_claude_usage_report(&row("Fable 5.1"), None).and_then(|q| q.fable).map(|w| w.used_pct), Some(41.0));
        assert_eq!(parse_claude_usage_report(&row("fable"), None).and_then(|q| q.fable).map(|w| w.used_pct), Some(41.0));
        assert!(parse_claude_usage_report(&row("Opus 5"), None).is_none(), "別的模型的週列沒有對應的桶");
        assert!(parse_claude_usage_report(&row("Fabled"), None).is_none());
    }

    /// 舊 CLI 沒有這個欄位（`grep` 沒抓到 → 跑純文字版）：JSON 解析回 None，交給原本的文字解析。
    #[test]
    fn a_plain_text_usage_screen_is_left_to_the_text_parser() {
        let screen = "Current session: 30% used · resets Sep 16 at 7:50am
AM_USAGE_DONE=0
";
        assert!(parse_claude_usage_report(screen, None).is_none());
        assert!(parse_claude_usage(screen, at("2026-09-15T20:00:00+08:00"), None).is_some());
        // 有那一行但壞掉（截斷）也不能當成有資料。
        assert!(parse_claude_usage_report("{\"usage_report\":{\"rate_limits\":{\"limi", None).is_none());
    }

    #[test]
    fn session_and_week_all_models_map() {
        let q = parse_claude_usage(SCREEN, at("2026-09-06T10:00:00+08:00"), Some("cc0")).unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 78.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 31.0);
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

    /// 2026-09-16 使用者：「怎麼不 show fable 用量了」。cc0 底下一直有 bot 在講話，狀態列每 30 秒
    /// 就刷一次，於是「狀態列很新就別開 pane」那條捷徑每一輪都成立，`/usage` 永遠輪不到——而 Fable
    /// 週窗與方案名**只有** `/usage` 讀得到。重啟前看得到只是因為 `quota::set` 會沿用舊的 `fable`。
    #[test]
    fn a_chatty_account_still_gets_its_usage_probe() {
        // 從沒問過 `/usage`：狀態列再新也要開一次 pane，否則 Fable 那格永遠是空的。
        assert!(!skip_probe_for_fresh_statusline(true, true, false));
        // 問過而且還新：這一輪讓狀態列接手，不用再開 pane（原本省成本的用意保留）。
        assert!(skip_probe_for_fresh_statusline(true, true, true));
        // 狀態列不新（帳號安靜了）：照樣要問。
        assert!(!skip_probe_for_fresh_statusline(true, false, true));
        // 還不知道這個帳號登入了沒：登入答案跟 `/usage` 同一趟，值得一個 pane。
        assert!(!skip_probe_for_fresh_statusline(false, true, true));
    }

    /// 記帳本身：沒問過就是不新，問過之後才在十分鐘內算數。
    #[test]
    fn the_usage_bookkeeping_starts_empty_and_expires() {
        let key = format!("claude:test-{}", crate::db::ulid());
        assert!(!usage_fresh(&key), "沒問過的 key 不能算新");
        mark_usage_seen(&key);
        assert!(usage_fresh(&key));
        // 十分鐘是上限：往後推時間就該重新問一次。不從 `Instant::now()` 往回減——開機不到十分鐘時那會 panic。
        assert!(!usage_fresh_at(&key, std::time::Instant::now() + USAGE_REFRESH), "超過 USAGE_REFRESH 就要再問一次");
    }

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
        let r = parse_claude_reset("Resets 3:00pm (Asia/Taipei)", at("2026-09-06T16:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-09-07T"), "{r}");
        let r = parse_claude_reset("Resets 3:00pm (Asia/Taipei)", at("2026-09-06T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-09-06T"), "{r}");
    }

    #[test]
    fn month_day_reset_parses() {
        let r = parse_claude_reset("Resets Sep 11 at 2pm (Asia/Taipei)", at("2026-09-06T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-09-11T06:00:00") || r.starts_with("2026-09-11T14:00:00"), "{r}");
    }

    /// #59：橫幅標的時區（America/New_York，9 月是 EDT -04:00）跟本機時區（sandbox 是 Asia/Taipei
    /// +08:00）不一樣時，一定要照橫幅標的時區換算，不能悄悄套用本機時區——那會整整差 12 小時。
    #[test]
    fn reset_timezone_differs_from_host_local() {
        let r = parse_claude_reset("Resets 3:00pm (America/New_York)", at("2026-09-06T10:00:00+08:00")).unwrap();
        assert_eq!(r, "2026-09-06T19:00:00.000Z", "{r}");
    }

    /// 認不出來的時區要有明確的退路：回 `None`，不要拿本機時區頂上去湊一個看似合理、其實可能錯的時間。
    #[test]
    fn unrecognized_timezone_returns_none_instead_of_guessing() {
        assert!(parse_claude_reset("Resets 3:00pm (Mars/OlympusMons)", at("2026-09-06T10:00:00+08:00")).is_none());
    }

    /// 橫幅裡完全沒有時區括號（不是目前 CLI 會產生的格式，但要防呆）：同樣不猜，回 `None`。
    #[test]
    fn missing_timezone_parenthetical_returns_none() {
        assert!(parse_claude_reset("Resets 3:00pm", at("2026-09-06T10:00:00+08:00")).is_none());
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
        assert!(q.five_hour.as_ref().unwrap().resets_at.as_deref().unwrap().starts_with("2026-09-07"));
        assert!(q.seven_day.as_ref().unwrap().resets_at.as_deref().unwrap().starts_with("2026-09-14"));
        assert!(q.fable.as_ref().unwrap().resets_at.as_deref().unwrap().starts_with("2026-09-14"));
        assert_ne!(q.seven_day.as_ref().unwrap().used_pct, 83.0);
    }

    #[test]
    fn a_logged_out_run_is_not_a_quota() {
        const OUT: &str = "Total cost:            $0.0000\nTotal duration (API):  0s\nUsage: 0 input, 0 output\n";
        assert!(parse_claude_usage(OUT, Local::now(), None).is_none());
    }

    #[test]
    fn the_probe_command_carries_the_identity_env() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), "/home/u/.claude-cc1".to_string());
        env.insert("bad name".to_string(), "x".to_string());
        let cmd = probe_command("/opt/homebrew/bin/claude", &env, true);
        assert!(cmd.contains("CLAUDE_CONFIG_DIR='/home/u/.claude-cc1'"), "{cmd}");
        assert!(!cmd.contains("bad name"), "{cmd}");
        assert!(cmd.contains("auth status --json"), "{cmd}");
        assert!(cmd.contains("-p '/usage'"), "{cmd}");
        assert!(!cmd.contains(AUTH_BEGIN) && !cmd.contains(AUTH_END) && !cmd.contains(USAGE_DONE), "{cmd}");
    }

    /// #80：quota probe 只問登入狀態跟 `/usage`，從不用工具，MCP server 起得慢或掛掉不該拖慢探測、
    /// 甚至把探測拖到 timeout。claude 2.1.274 起 `CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0` 讓第一個
    /// non-interactive turn 跳過等 MCP 連線；三個會跑 `claude` 的地方（auth status、結構化
    /// `/usage`、退回的純文字 `/usage`）都要吃得到，因為它跟身分自己的 env 一樣放進共用的 `pfx`。
    #[test]
    fn the_probe_command_skips_mcp_startup_wait() {
        let cmd = probe_command("/bin/claude", &BTreeMap::new(), true);
        assert_eq!(cmd.matches("CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0").count(), 3, "{cmd}");

        // login-only target（不跑 /usage）：登入探測一樣不該被 MCP 拖住。
        let login_only = probe_command("/bin/claude", &BTreeMap::new(), false);
        assert_eq!(login_only.matches("CLAUDE_CODE_MCP_STARTUP_WAIT_MS=0").count(), 1, "{login_only}");
    }

    #[test]
    fn the_echoed_command_does_not_look_like_the_markers() {
        let cmd = probe_command("/bin/claude", &BTreeMap::new(), true);
        let screen = format!(
            "u@host ~ % {cmd}\n\n{AUTH_BEGIN}\n{{\"loggedIn\":true,\"email\":\"a@b.c\"}}\n\n{AUTH_END}\n{PLAIN}\n{USAGE_DONE}0\n"
        );
        let (auth, usage) = split_probe_output(&screen).expect("markers found");
        assert!(auth.contains("\"loggedIn\":true"), "{auth}");
        assert!(usage.contains("Current week (Fable)"), "{usage}");
        assert!(!usage.contains("loggedIn"), "{usage}");
        assert_eq!(crate::tools::read_login_answer("claude", &auth).1.as_deref(), Some("a@b.c"));
    }

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

    /// 半截的 `/usage` 會少一條桶子。
    #[test]
    fn an_unfinished_run_has_no_answer_yet() {
        let screen = format!("{AUTH_BEGIN}\n{{\"loggedIn\":true}}\n{AUTH_END}\nCurrent session: 47% used\n");
        assert!(split_probe_output(&screen).is_none());
    }

    /// m4p's `cc1` right after a daemon restart: no statusLine yet, but a live run is proof enough.
    #[test]
    fn a_live_bot_run_beats_the_login_answer() {
        let e = ProbeEvidence { cli_says_logged_out: true, has_live_run: true, ..Default::default() };
        assert!(should_probe_identity(e));
        // 有 run 在跑，沒登入的 30 分鐘縮成 5 分鐘——但不是完全不退避（M4）。
        let k = format!("test/claude:{}", ulid::Ulid::new());
        let t0 = std::time::Instant::now();
        park(&k, true);
        let ten_min = t0 + Duration::from_secs(10 * 60);
        assert!(cooling_down_at(&k, false, false, ten_min), "沒有任何證據：沒登入的帳號等滿 30 分鐘");
        assert!(!cooling_down_at(&k, false, true, ten_min), "失敗之後開始有 bot 在用：5 分鐘就再問");
    }

    /// M4（review 2026-09-16）：`/usage` 壞掉（CLI 改格式、pane 卡住）時，一直有 bot 在講話的帳號以前無視退避，
    /// 每 60 秒開一個 pane、佔住 `probe_lock` 40 秒。現在一樣退避 5 分鐘。
    #[test]
    fn a_busy_account_still_backs_off_after_a_failed_probe() {
        let k = format!("test/claude:{}", ulid::Ulid::new());
        let t0 = std::time::Instant::now();
        park(&k, false);
        assert!(cooling_down_at(&k, true, true, t0 + Duration::from_secs(60)), "下一輪（60 秒後）不能又開 pane");
        let e = ProbeEvidence { reported_statusline: true, has_live_run: true, cooling_down: true, ..Default::default() };
        assert!(!should_probe_identity(e), "證據不再蓋過退避");
        assert!(!cooling_down_at(&k, true, true, t0 + RETRY_AFTER_FAILURE + Duration::from_secs(1)), "5 分鐘到了照樣再問");
    }

    fn ident(name: &str, env: &[(&str, &str)]) -> crate::config::IdentityCfg {
        crate::config::IdentityCfg {
            name: name.into(),
            kind: "claude".into(),
            host: None,
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            args: vec![],
        }
    }

    /// M4：裸的預設帳號以前「永不 park」——`/usage` 一壞，cc0 這種一直有 bot 在講話的帳號每 60 秒開一個 pane。
    /// 現在它跟其他身分一樣吃退避，證據只影響退避長度。
    #[test]
    fn the_default_account_target_backs_off_like_any_other() {
        let identities = vec![ident("cc0", &[]), ident("cc1", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc1")])];
        let logins = BTreeMap::new();
        let live: std::collections::BTreeSet<String> = ["cc0".to_string(), "cc1".to_string()].into_iter().collect();
        let statusline: std::collections::BTreeSet<String> = ["claude".to_string(), "claude:cc1".to_string()].into_iter().collect();
        let input = PlanInput {
            host: "local",
            home: "/home/me",
            identities: &identities,
            logins: &logins,
            live: &live,
            off: &[],
            unnamed_running: false,
            statusline_keys: &statusline,
        };
        let keys = |ts: Vec<Target>| ts.into_iter().map(|t| t.key).collect::<Vec<_>>();
        assert_eq!(keys(plan_targets(&input, |_, _, _| false)), ["claude", "claude:cc1"]);
        assert_eq!(keys(plan_targets(&input, |k, _, _| k == "claude")), ["claude:cc1"], "預設帳號剛失敗：這一輪不開 pane");
        let bare = plan_targets(&input, |_, _, _| false).into_iter().next().unwrap();
        assert_eq!(bare.names, ["cc0"]);
        assert!(bare.evidence.has_live_run && bare.evidence.reported_statusline);
    }

    /// L2（review 2026-09-16）：`api` 只帶 `ANTHROPIC_API_KEY`、沒有自己的 `CLAUDE_CONFIG_DIR`。額度規則上它跟預設帳號
    /// 同一格（裸 `claude`），但以前連探測也併進裸 target、用**空 env** 問，預設帳號的 email／方案被記到它名下。
    #[test]
    fn an_identity_with_env_but_no_config_dir_is_asked_about_its_login_with_its_own_env() {
        let identities = vec![ident("cc0", &[]), ident("api", &[("ANTHROPIC_API_KEY", "sk-test")])];
        let logins = BTreeMap::new();
        let live = Default::default();
        let statusline = Default::default();
        let input = PlanInput { host: "local", home: "/h", identities: &identities, logins: &logins, live: &live, off: &[], unnamed_running: false, statusline_keys: &statusline };
        let ts = plan_targets(&input, |_, _, _| false);
        assert_eq!(ts.len(), 2);
        assert_eq!((ts[0].key.as_str(), ts[0].names.clone(), ts[0].login_only), ("claude", vec!["cc0".to_string()], false), "裸 target 只代表 env 真的是空的身分");
        assert_eq!((ts[1].key.as_str(), ts[1].login_only), ("claude:api", true));
        assert_eq!(ts[1].env.get("ANTHROPIC_API_KEY").map(String::as_str), Some("sk-test"), "登入答案要帶它自己的 env 問");
        let cmd = probe_command("/bin/claude", &ts[1].env, false);
        assert!(cmd.contains("auth status --json") && !cmd.contains("/usage"), "只問登入，不跑 /usage：{cmd}");
        assert_eq!(crate::quota::quota_base_default_aware("claude", Some("api"), crate::quota::identity_shares_default("claude", &identities[1].env)), "claude", "額度落點規則不變");
    }

    /// L4（review 2026-09-16）：共用預設帳號的身分（cc0）停用了、沒有 run，也沒有不帶身分的 claude bot 在跑，
    /// 裸 `claude` 就不再探測（SPEC §16.3b）。沒有任何身分指到預設帳號時照探——沒有東西可以停用。
    #[test]
    fn the_default_account_is_skipped_only_when_everything_on_it_is_disabled_and_idle() {
        let off = vec!["cc0".to_string()];
        let idle: std::collections::BTreeSet<String> = Default::default();
        let busy: std::collections::BTreeSet<String> = ["cc0".to_string()].into_iter().collect();
        let names = vec!["cc0".to_string()];
        assert!(skip_disabled_default(&off, &idle, &names, false));
        assert!(!skip_disabled_default(&off, &busy, &names, false), "cc0 還有 run 在跑");
        assert!(!skip_disabled_default(&off, &idle, &names, true), "不帶身分的 claude bot 也吃預設帳號");
        assert!(!skip_disabled_default(&[], &idle, &names, false), "沒停用");
        assert!(!skip_disabled_default(&off, &idle, &[], false), "沒有身分指到預設帳號");
        let two = vec!["cc0".to_string(), "main".to_string()];
        assert!(!skip_disabled_default(&off, &idle, &two, false), "還有一個共用預設帳號的身分沒停用");
    }

    #[test]
    fn a_statusline_beats_the_login_answer() {
        let e = ProbeEvidence { cli_says_logged_out: true, reported_statusline: true, ..Default::default() };
        assert!(should_probe_identity(e));
    }

    /// Regression: a previous `loggedIn: false` must NOT park the identity forever.
    #[test]
    fn the_login_answer_alone_never_blocks_a_probe() {
        let e = ProbeEvidence { cli_says_logged_out: true, ..Default::default() };
        assert!(should_probe_identity(e));
    }

    #[test]
    fn a_failed_probe_parks_the_identity_until_its_cooldown_expires() {
        assert!(!should_probe_identity(ProbeEvidence { cooling_down: true, ..Default::default() }));
        assert!(should_probe_identity(ProbeEvidence { cooling_down: false, ..Default::default() }));
    }

    /// m4p's `cc2`: a genuinely logged-out account costs one probe per half hour.
    #[test]
    fn backoff_is_longer_when_the_cli_also_says_logged_out() {
        let out = ProbeEvidence { cli_says_logged_out: true, ..Default::default() };
        assert_eq!(failure_backoff(out), RETRY_WHEN_LOGGED_OUT);
        assert_eq!(failure_backoff(ProbeEvidence::default()), RETRY_AFTER_FAILURE);
        assert_eq!(failure_backoff(ProbeEvidence { has_live_run: true, ..out }), RETRY_AFTER_FAILURE);
        assert_eq!(failure_backoff(ProbeEvidence { reported_statusline: true, ..out }), RETRY_AFTER_FAILURE);
    }

    #[test]
    fn parking_expires_and_a_success_clears_it() {
        let k = format!("test/claude:{}", ulid::Ulid::new());
        assert!(!cooling_down(&k, false, false));
        park(&k, false);
        assert!(cooling_down(&k, false, false));
        unpark(&k);
        assert!(!cooling_down(&k, false, false));
        park(&k, false);
        assert!(!cooling_down_at(&k, false, false, std::time::Instant::now() + RETRY_AFTER_FAILURE), "a cool-down in the past is over");
    }
}
