//! Console Billing 頁的實際餘額（使用者 2026-11-04 核准）。
//!
//! Anthropic 沒有查預付餘額的 API，真正的餘額只在 Console Billing 頁。這裡定期用 agent-browser 帶使用者匯出的登入態
//! （`[claude_api] console_state_file`，agent-browser `state save` 格式，**是憑證**）讀那一頁的文字，解析出
//! purchased／promotional 餘額與每筆促銷的剩餘、總額、到期日，放在記憶體裡給 `GET /api/claude-api/balance` 的 `console` 物件。
//!
//! 規矩：
//! - 固定 session `agm-console-balance`，讀完（含失敗路徑）一定 `close`；整個過程有總逾時，用 tokio 子行程，不擋 runtime。
//! - 登入態檔不存在就不跑；內容、頁面文字以外的任何輸出都不進 log（只記錯誤類別與短訊息）。
//! - 讀成功後用 `state save` 把更新過的 cookie 寫回同一檔（暫存檔 0600 → 驗證 → rename），延長登入態壽命。
//! - 純解析（[`parse_console_billing`]）不碰 IO，中英文介面、未登入頁、格式變了都有單元測試。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDate, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::state::App;

pub const SESSION: &str = "agm-console-balance";
const BILLING_URL: &str = "https://platform.claude.com/settings/billing";
/// 整個讀取流程（open → 等頁面 → get text → state save → close）的總逾時。
const TOTAL_BUDGET: Duration = Duration::from_secs(60);
const CLOSE_BUDGET: Duration = Duration::from_secs(15);
const POLL_EVERY: Duration = Duration::from_secs(3);
const MAX_POLLS: usize = 8;

/// 一筆促銷額度。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Promotion {
    pub remaining_usd: f64,
    pub total_usd: f64,
    /// `YYYY-MM-DD`；解析不到日期就是 `None`。
    pub expires_on: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleBalanceData {
    pub purchased_usd: f64,
    pub promotional_usd: f64,
    pub promotions: Vec<Promotion>,
}

impl ConsoleBalanceData {
    pub fn total_usd(&self) -> f64 {
        self.purchased_usd + self.promotional_usd
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseOutcome {
    Ok(ConsoleBalanceData),
    /// 頁面是登入畫面。
    LoggedOut,
    /// 不是登入畫面、也找不到餘額（頁面格式變了／還沒載完）。
    ParseFailed,
}

// ───────────────────────── 純解析 ─────────────────────────

/// `$1,234.56` → 1234.56；整段字串必須剛好是一個金額。
fn parse_money(s: &str) -> Option<f64> {
    let t = s.trim().strip_prefix("US").unwrap_or(s.trim());
    let t = t.strip_prefix('$')?;
    parse_amount_digits(t)
}

fn parse_amount_digits(t: &str) -> Option<f64> {
    let t = t.trim();
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.') || !t.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    t.replace(',', "").parse::<f64>().ok().filter(|v| v.is_finite())
}

/// 一行裡所有 `$金額`（依出現順序）。
fn money_tokens(line: &str) -> Vec<f64> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b',' || b[j] == b'.') {
                j += 1;
            }
            if let Some(v) = parse_amount_digits(&line[i + 1..j].trim_end_matches([',', '.'])) {
                out.push(v);
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// 把一行裡的 `$金額` 拿掉，剩下的字（去空白、小寫）；用來判斷「$A of $B」這種只剩連接詞的行。
fn residual_without_money(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b',' || b[j] == b'.') {
                j += 1;
            }
            i = j;
        } else {
            let ch = line[i..].chars().next().unwrap();
            if !ch.is_whitespace() {
                out.extend(ch.to_lowercase());
            }
            i += ch.len_utf8();
        }
    }
    out
}

fn is_balance_label(line: &str) -> bool {
    let l = line.trim().to_lowercase();
    if l.contains("credit balance") {
        return true;
    }
    ["額度餘額", "信用餘額", "點數餘額", "额度余额", "信用余额", "点数余额"].iter().any(|k| l.contains(k)) || l == "餘額" || l == "余额"
}

fn is_promo_line(line: &str) -> bool {
    let l = line.to_lowercase();
    ["promotional", "promo credit", "促銷", "促销", "優惠", "优惠", "推廣", "推广"].iter().any(|k| l.contains(k))
}

fn is_logged_out(text: &str) -> bool {
    let l = text.to_lowercase();
    let compact: String = l.chars().filter(|c| !c.is_whitespace()).collect();
    ["continuewithgoogle", "continuewithemail", "使用google繼續", "使用google继续", "使用google帳戶繼續", "使用google账号继续", "使用google帳號繼續", "使用電子郵件繼續", "使用电子邮件继续"].iter().any(|k| compact.contains(k))
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn ymd(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(y, m, d)
}

/// 一行裡所有日期（依出現順序）。支援 `MM/DD/YYYY`（第一段 > 12 當 DD/MM）、`YYYY-MM-DD`／`YYYY/MM/DD`、
/// `YYYY年M月D日`、`Nov 15, 2026`。
fn find_dates(line: &str) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    // 1) 數字加分隔符：以非「數字 / - . 年 月 日」切開的片段。
    for tok in line.split(|c: char| !(c.is_ascii_digit() || matches!(c, '/' | '-' | '.' | '年' | '月' | '日'))) {
        if tok.is_empty() {
            continue;
        }
        if let Some(d) = parse_numeric_date(tok) {
            out.push(d);
        }
    }
    // 2) 英文月份：Mon D, YYYY（或 Month D YYYY）。
    let words: Vec<&str> = line.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()).collect();
    for w in words.windows(3) {
        let m = w[0].trim_end_matches('.').to_lowercase();
        if m.len() < 3 {
            continue;
        }
        if let Some(mi) = MONTHS.iter().position(|p| m.starts_with(p)) {
            if let (Ok(d), Ok(y)) = (w[1].trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<u32>(), w[2].parse::<i32>()) {
                if let Some(date) = ymd(y, mi as u32 + 1, d) {
                    out.push(date);
                }
            }
        }
    }
    out
}

fn parse_numeric_date(tok: &str) -> Option<NaiveDate> {
    if tok.contains('年') {
        let t = tok.replace(['年', '月'], "-").replace('日', "");
        return parse_numeric_date(&t);
    }
    let parts: Vec<&str> = tok.split(['/', '-', '.']).collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let n: Vec<u32> = parts.iter().map(|p| p.parse().unwrap_or(0)).collect();
    if parts[0].len() == 4 {
        return ymd(n[0] as i32, n[1], n[2]);
    }
    if parts[2].len() == 4 {
        let y = n[2] as i32;
        // 英文介面是 MM/DD/YYYY；第一段不可能是月份才當 DD/MM。
        return if n[0] > 12 { ymd(y, n[1], n[0]) } else { ymd(y, n[0], n[1]) };
    }
    None
}

/// 解析 `get text body` 的頁面文字。
pub fn parse_console_billing(text: &str) -> ParseOutcome {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();

    // purchased：標籤行之後 8 行內第一個「整行就是金額」的行。
    let mut purchased: Option<f64> = None;
    for (i, l) in lines.iter().enumerate() {
        if !is_balance_label(l) {
            continue;
        }
        // 標籤跟金額同一行（例如「Credit balance $5.00」）也算。
        let same = money_tokens(l);
        if same.len() == 1 && !is_promo_line(l) {
            purchased = Some(same[0]);
            break;
        }
        if let Some(v) = lines.iter().skip(i + 1).take(8).find_map(|n| parse_money(n)) {
            purchased = Some(v);
            break;
        }
    }
    let Some(purchased_usd) = purchased else {
        return if is_logged_out(text) { ParseOutcome::LoggedOut } else { ParseOutcome::ParseFailed };
    };

    // promotions：「$剩 of $總」行，後面幾行（到下一筆之前）的日期。
    let mut promotions: Vec<Promotion> = Vec::new();
    let mut idx = 0;
    while idx < lines.len() {
        let l = lines[idx];
        let m = money_tokens(l);
        let is_pair = m.len() == 2 && {
            let r = residual_without_money(l);
            r.chars().count() <= 6 && !r.chars().any(|c| c.is_ascii_digit())
        };
        if !is_pair {
            idx += 1;
            continue;
        }
        let (remaining_usd, total_usd) = if m[0] <= m[1] { (m[0], m[1]) } else { (m[1], m[0]) };
        let mut dates: Vec<NaiveDate> = Vec::new();
        let mut j = idx + 1;
        while j < lines.len() && j <= idx + 6 {
            let n = lines[j];
            let nm = money_tokens(n);
            if nm.len() == 2 && residual_without_money(n).chars().count() <= 6 {
                break;
            }
            dates.extend(find_dates(n));
            if dates.len() >= 2 {
                break;
            }
            j += 1;
        }
        // 欄位順序是「到期日、發放日」；介面語言不同時保險起見取較晚的當到期日。
        let expires_on = dates.iter().max().map(|d| format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()));
        promotions.push(Promotion { remaining_usd, total_usd, expires_on });
        idx = j.max(idx + 1);
    }

    // promotional 可用額度：優先用「$X promotional credit available」那一行；沒有就加總各筆剩餘。
    let promotional_usd = lines
        .iter()
        .filter(|l| is_promo_line(l))
        .find_map(|l| {
            let m = money_tokens(l);
            (m.len() == 1).then(|| m[0])
        })
        .unwrap_or_else(|| promotions.iter().map(|p| p.remaining_usd).sum());

    ParseOutcome::Ok(ConsoleBalanceData { purchased_usd, promotional_usd, promotions })
}

// ───────────────────────── 快照與 JSON ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    LoggedOut,
    ParseFailed,
    Error,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::LoggedOut => "logged_out",
            Status::ParseFailed => "parse_failed",
            Status::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConsoleSnapshot {
    pub status: Status,
    pub data: Option<ConsoleBalanceData>,
    pub fetched_at: String,
    pub error: Option<String>,
}

/// `GET /api/claude-api/balance` 的 `console` 物件。`snapshot` 為 `None` 且有在跑＝還沒讀到第一次。
pub fn console_json(snapshot: Option<&ConsoleSnapshot>, configured: bool) -> Value {
    if !configured {
        return json!({"status": "not_configured"});
    }
    let Some(s) = snapshot else {
        return json!({"status": "error", "error": "pending"});
    };
    let mut v = json!({"status": s.status.as_str(), "fetched_at": s.fetched_at});
    if let (Status::Ok, Some(d)) = (s.status, s.data.as_ref()) {
        v["purchased_usd"] = json!(d.purchased_usd);
        v["promotional_usd"] = json!(d.promotional_usd);
        v["total_usd"] = json!(d.total_usd());
        v["promotions"] = json!(d.promotions);
    }
    if let Some(e) = &s.error {
        v["error"] = json!(e);
    }
    v
}

// ───────────────────────── 抓取 ─────────────────────────

/// 跑一條 agent-browser 指令；回 stdout。可替換成假的做測試。
pub trait BrowserRunner: Send + Sync {
    fn run(&self, args: Vec<String>, timeout: Duration) -> impl Future<Output = Result<String, String>> + Send;
}

pub struct RealRunner {
    exe: PathBuf,
}

impl RealRunner {
    pub fn new() -> Self {
        // daemon 的 PATH 不一定有 ~/.local/bin。
        let home = std::env::var("HOME").unwrap_or_default();
        let local = Path::new(&home).join(".local/bin/agent-browser");
        Self { exe: if local.exists() { local } else { PathBuf::from("agent-browser") } }
    }
}

impl BrowserRunner for RealRunner {
    fn run(&self, args: Vec<String>, timeout: Duration) -> impl Future<Output = Result<String, String>> + Send {
        let exe = self.exe.clone();
        async move {
            let mut cmd = tokio::process::Command::new(exe);
            cmd.args(&args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
            let out = tokio::time::timeout(timeout, cmd.output()).await.map_err(|_| "agent-browser timed out".to_string())?.map_err(|e| format!("agent-browser spawn failed: {e}"))?;
            if !out.status.success() {
                // stderr 只留第一行短訊息，不帶其他輸出。
                let err = String::from_utf8_lossy(&out.stderr);
                let first = err.lines().next().unwrap_or("").chars().take(160).collect::<String>();
                return Err(format!("agent-browser exited {}: {first}", out.status.code().unwrap_or(-1)));
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now()).max(Duration::from_secs(1))
}

/// 讀一次 Billing 頁。**一定** `close`，成功才把更新過的 cookie 寫回 `state_path`。
pub async fn fetch_once<R: BrowserRunner>(r: &R, state_path: &Path) -> ConsoleSnapshot {
    let deadline = Instant::now() + TOTAL_BUDGET;
    // 前一輪若殘留 session（daemon 被砍），先收掉，不然 --state 不會重新載入。
    let _ = r.run(args(&["--session", SESSION, "close"]), CLOSE_BUDGET).await;
    let result = fetch_inner(r, state_path, deadline).await;
    if let Err(e) = r.run(args(&["--session", SESSION, "close"]), CLOSE_BUDGET).await {
        tracing::warn!("claude console balance: close failed: {e}");
    }
    let fetched_at = am_base::db::iso_at(Utc::now());
    match result {
        Ok(ParseOutcome::Ok(d)) => ConsoleSnapshot { status: Status::Ok, data: Some(d), fetched_at, error: None },
        Ok(ParseOutcome::LoggedOut) => ConsoleSnapshot { status: Status::LoggedOut, data: None, fetched_at, error: None },
        Ok(ParseOutcome::ParseFailed) => ConsoleSnapshot { status: Status::ParseFailed, data: None, fetched_at, error: None },
        Err(e) => ConsoleSnapshot { status: Status::Error, data: None, fetched_at, error: Some(e) },
    }
}

async fn fetch_inner<R: BrowserRunner>(r: &R, state_path: &Path, deadline: Instant) -> Result<ParseOutcome, String> {
    let state = state_path.to_string_lossy().into_owned();
    r.run(args(&["--session", SESSION, "--state", &state, "open", BILLING_URL]), remaining(deadline)).await?;
    let mut outcome = ParseOutcome::ParseFailed;
    for _ in 0..MAX_POLLS {
        if Instant::now() >= deadline {
            return Err("timed out waiting for billing page".into());
        }
        // 頁面是 SPA，open 回來時餘額不一定已經畫好。
        tokio::time::sleep(POLL_EVERY.min(remaining(deadline))).await;
        let text = r.run(args(&["--session", SESSION, "get", "text", "body"]), remaining(deadline)).await?;
        outcome = parse_console_billing(&text);
        if outcome != ParseOutcome::ParseFailed {
            break;
        }
    }
    if matches!(outcome, ParseOutcome::Ok(_)) {
        if let Err(e) = save_state_back(r, state_path, deadline).await {
            // 寫回失敗不影響這次讀到的餘額。
            tracing::warn!("claude console balance: state write-back skipped: {e}");
        }
    }
    Ok(outcome)
}

/// `state save` 到同目錄的 0600 暫存檔 → 驗證有 cookie → chmod 600 → rename 蓋回原檔。
async fn save_state_back<R: BrowserRunner>(r: &R, state_path: &Path, deadline: Instant) -> Result<(), String> {
    let dir = state_path.parent().ok_or("state path has no parent")?;
    let name = state_path.file_name().ok_or("state path has no file name")?.to_string_lossy().into_owned();
    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));
    let result = async {
        create_private_file(&tmp).map_err(|e| format!("create tmp: {e}"))?;
        r.run(args(&["--session", SESSION, "state", "save", &tmp.to_string_lossy()]), remaining(deadline)).await?;
        validate_state_file(&tmp)?;
        set_mode_600(&tmp).map_err(|e| format!("chmod: {e}"))?;
        std::fs::rename(&tmp, state_path).map_err(|e| format!("rename: {e}"))
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn create_private_file(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(p);
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(p).map(|_| ())
}

fn set_mode_600(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
}

/// 寫回前確認新檔是 JSON 且至少有一個 cookie，免得把好的登入態換成壞檔。內容不外洩。
fn validate_state_file(p: &Path) -> Result<(), String> {
    let s = std::fs::read_to_string(p).map_err(|e| format!("read saved state: {e}"))?;
    let v: Value = serde_json::from_str(&s).map_err(|_| "saved state is not JSON".to_string())?;
    match v.get("cookies").and_then(|c| c.as_array()) {
        Some(c) if !c.is_empty() => Ok(()),
        _ => Err("saved state has no cookies".into()),
    }
}

// ───────────────────────── daemon 整合 ─────────────────────────

/// 設定現在要不要跑、登入態檔在哪；不跑回 `None`（含檔案不存在）。
pub async fn active_state_path(app: &Arc<App>) -> Option<(PathBuf, Duration)> {
    let cfg = app.cfg.get().await;
    let c = &cfg.claude_api;
    if !c.console_balance_enabled {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let path = PathBuf::from(crate::config::expand_home(&c.console_state_file, &home));
    if !path.is_file() {
        return None;
    }
    let mins = c.console_balance_interval_minutes.max(5);
    Some((path, Duration::from_secs(mins * 60)))
}

/// 讀一次並放進快照；不該跑就清掉快照。
pub async fn refresh(app: &Arc<App>) -> Option<Duration> {
    let Some((path, every)) = active_state_path(app).await else {
        *app.claude_api.console_balance.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return None;
    };
    let snap = fetch_once(&RealRunner::new(), &path).await;
    if snap.status != Status::Ok {
        tracing::warn!(status = snap.status.as_str(), error = snap.error.as_deref().unwrap_or(""), "claude console balance: fetch did not succeed");
    }
    *app.claude_api.console_balance.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap);
    Some(every)
}

/// 開機後先讀一次，之後每 `console_balance_interval_minutes` 讀一次。測試不起。
pub fn spawn_ticker(app: &Arc<App>) {
    if cfg!(test) {
        return;
    }
    let app = app.clone();
    let tasks = app.background_tasks.clone();
    am_base::background_loop::spawn_named(&tasks, "claude console balance", async move {
        loop {
            let wait = refresh(&app).await.unwrap_or(Duration::from_secs(300));
            tokio::select! {
                _ = app.shutdown.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    });
}

/// `GET /api/claude-api/balance` 用。
pub async fn current_json(app: &Arc<App>) -> Value {
    let configured = active_state_path(app).await.is_some();
    let snap = app.claude_api.console_balance.lock().unwrap_or_else(|e| e.into_inner()).clone();
    console_json(snap.as_ref(), configured)
}

#[cfg(test)]
mod tests;
