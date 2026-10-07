//! agy（Antigravity CLI）額度（SPEC §12a.7）。數字來自 `agy -p "/usage" --output-format json`：唯讀指令，**不開對話、不耗額度**，
//! 所以不必像 grok 那樣開一顆探測 pane——在拋棄式暫存目錄裡直接跑、有逾時、跑完刪目錄。
//!
//! 新版輸出在 `command.data.groups[].buckets[]`：只讀 Gemini 組的 weekly 與 five-hour 窗口，`remaining_fraction` 是剩餘比例，
//! `reset_time` 是 RFC3339 重置時刻。Claude/GPT 組不屬於 AG Man 的 agy 額度。舊版 tab 分隔 `response` 仍接受；
//! 探測失敗**不覆蓋**舊值（讀數自己會變陳舊，`quota::STALE_AFTER`）；格式讀不懂回 `None`，不編數字。

use crate::config::LOCAL_HOST;
use crate::quota::{Quota, Window};
use crate::state::App;
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// 每次探測要起一個約 200 MB 的執行檔。
pub const AGY_POLL: Duration = Duration::from_secs(300);
/// 測試縮短（探測逾時的測試不必真的等 40 秒）。
const PROBE_TIMEOUT: Duration = if cfg!(test) { Duration::from_secs(3) } else { Duration::from_secs(40) };
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(15 * 60);

/// 探測指令（本機與遠端同一份）：拋棄式 cwd、不讀 stdin、關自動更新、跑完刪目錄。`exe` 是偵測到的絕對路徑，沒有就用 PATH 上的 `agy`。
pub fn probe_script(exe: Option<&str>) -> String {
    let exe = crate::hosts::sh_quote(exe.unwrap_or("agy"));
    format!(
        "d=$(mktemp -d 2>/dev/null) || exit 1; cd \"$d\" || exit 1; \
         AGY_CLI_DISABLE_AUTO_UPDATE=true {exe} -p /usage --output-format json </dev/null; rc=$?; cd / ; rm -rf \"$d\"; exit $rc"
    )
}

fn bucket_key(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if n.contains("gemini") {
        Some("agy")
    } else {
        None
    }
}

fn parse_remaining(field: &str) -> Option<f64> {
    let t = field.trim().strip_suffix('%')?.trim();
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn reset_time(value: &Value) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn quota_row(key: &'static str, now: &str) -> (&'static str, Quota) {
    (
        key,
        Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: now.to_string(),
            source: "agy-usage".into(),
            account: None,
            host: LOCAL_HOST.into(),
        },
    )
}

fn text_field<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields.iter().find_map(|field| value.get(*field).and_then(Value::as_str))
}

#[derive(Clone, Copy)]
enum UsageWindow {
    FiveHour,
    SevenDay,
}

fn bucket_window(bucket: &Value) -> Option<UsageWindow> {
    let window = text_field(bucket, &["window", "type"]).unwrap_or_default().to_ascii_lowercase();
    let id = text_field(bucket, &["id", "name", "label", "title"]).unwrap_or_default().to_ascii_lowercase();
    let description = format!("{window} {id}");
    if description.contains("weekly") || description.contains("week") {
        Some(UsageWindow::SevenDay)
    } else if description.contains("five_hour")
        || description.contains("five-hour")
        || description.contains("five hour")
        || description.contains("5h")
    {
        Some(UsageWindow::FiveHour)
    } else {
        None
    }
}

/// `command.data.groups[].buckets[]`：只依 Gemini 組的窗口填入 `agy`。
fn parse_group_usage(v: &Value, now: &str) -> Option<Vec<(&'static str, Quota)>> {
    let groups = v.get("command")?.get("data")?.get("groups")?.as_array()?;
    let mut out: Vec<(&'static str, Quota)> = Vec::new();
    for group in groups {
        let group_name = text_field(group, &["name", "label", "title", "id", "model"]).unwrap_or_default();
        let Some(buckets) = group.get("buckets").and_then(Value::as_array) else { continue };
        for bucket in buckets {
            let bucket_name = text_field(bucket, &["id", "name", "label", "title", "model"]).unwrap_or_default();
            let Some(key) = bucket_key(group_name).or_else(|| bucket_key(bucket_name)) else { continue };
            let Some(window) = bucket_window(bucket) else { continue };
            let Some(left) = bucket.get("remaining_fraction").and_then(Value::as_f64).filter(|x| x.is_finite() && (0.0..=1.0).contains(x)) else {
                continue;
            };
            let w = Window { observed_at: None, used_pct: ((1.0 - left) * 100.0).clamp(0.0, 100.0), resets_at: bucket.get("reset_time").and_then(reset_time) };
            let row = if let Some(row) = out.iter_mut().find(|(k, _)| *k == key) {
                row
            } else {
                out.push(quota_row(key, now));
                out.last_mut().expect("just pushed")
            };
            match window {
                UsageWindow::FiveHour => row.1.five_hour = Some(w),
                UsageWindow::SevenDay => row.1.seven_day = Some(w),
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// 舊版 `response` 是 tab 分隔列；只保留 Gemini weekly 百分比，沒有 5h 資料。
fn parse_legacy_usage(v: &Value, now: &str) -> Option<Vec<(&'static str, Quota)>> {
    let response = v.get("response")?.as_str()?;
    let mut out: Vec<(&'static str, Quota)> = Vec::new();
    for line in response.lines() {
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        if f.len() < 3 {
            continue;
        }
        let Some(key) = bucket_key(f[0]) else { continue };
        if !f[1].to_ascii_lowercase().contains("week") {
            continue;
        }
        let Some(left) = f[2..].iter().find_map(|x| parse_remaining(x)) else { continue };
        if out.iter().any(|(k, _)| *k == key) {
            continue;
        }
        let mut row = quota_row(key, now);
        row.1.seven_day = Some(Window {
            observed_at: None,
            used_pct: (100.0 - left).clamp(0.0, 100.0),
            resets_at: f[2..].iter().find_map(|x| chrono::DateTime::parse_from_rfc3339(x).ok())
                .map(|d| d.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        });
        out.push(row);
    }
    (!out.is_empty()).then_some(out)
}

/// `[(key, Quota)]`，窗口或模型名認不得就略過；完全讀不到回 `None`。
pub fn parse_usage(stdout: &str) -> Option<Vec<(&'static str, Quota)>> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let now = crate::db::now();
    parse_group_usage(&v, &now).or_else(|| parse_legacy_usage(&v, &now))
}

/// 遠端探測的 pane：macOS 的 agy 憑證在 login Keychain，純 ssh 讀不到（`security` 回 `User interaction is not allowed`，
/// `agy -p /usage` 印 `Authentication required` 然後卡住），所以跟 claude 的額度探測一樣，在那台 herdr 的 pane 裡跑
/// （herdr server 由 launchd 起，pane 在 GUI session 底下、讀得到 Keychain）。標記拆成 `printf` 參數，shell 回顯的指令才不會長得像標記。
const PANE_BEGIN: &str = "AM_AGY_BEGIN";
const PANE_DONE: &str = "AM_AGY_DONE=";
/// pane 探測 workspace 的 label 前綴（claude 的是 `am-quota-claude`）：各自清各自的殘留，不互相把對方正在跑的收掉。
pub(crate) const PROBE_LABEL_PREFIX: &str = "am-quota-agy";

pub(crate) fn is_probe_label(l: &str) -> bool {
    l == PROBE_LABEL_PREFIX
}

/// 在 pane 的 shell 裡打的那一行：拋棄式 cwd、不讀 stdin、關自動更新，stdout 與 stderr 都留在畫面上（`Authentication required` 在那裡）。
pub fn pane_probe_command(exe: Option<&str>) -> String {
    let exe = crate::hosts::sh_quote(exe.unwrap_or("agy"));
    format!(
        "d=$(mktemp -d 2>/dev/null) && cd \"$d\" && {{ printf '\\nAM_AGY_%s\\n' BEGIN; \
         AGY_CLI_DISABLE_AUTO_UPDATE=true {exe} -p /usage --output-format json </dev/null 2>&1; rc=$?; cd /; rm -rf \"$d\"; \
         printf '\\nAM_AGY_%s=%s\\n' DONE \"$rc\"; }} || printf '\\nAM_AGY_%s=%s\\n' DONE 1"
    )
}

/// 畫面最後一次 `AM_AGY_BEGIN` 之後的部分（沒有就整個畫面）。
fn after_begin(screen: &str) -> &str {
    screen.rfind(PANE_BEGIN).map_or(screen, |i| &screen[i + PANE_BEGIN.len()..])
}

/// 指令跑完：`(結束碼, BEGIN 到 DONE 之間的輸出)`。從**尾端**找：重打過的指令會在捲動緩衝區留下兩輪，只有最後一輪是完整的。
fn pane_done(screen: &str) -> Option<(i32, String)> {
    let done = screen.rfind(PANE_DONE)?;
    let rc = screen[done + PANE_DONE.len()..].lines().next().and_then(|l| l.trim().parse::<i32>().ok()).unwrap_or(1);
    let head = &screen[..done];
    let body = head.rfind(PANE_BEGIN).map_or("", |i| &head[i + PANE_BEGIN.len()..]);
    Some((rc, body.trim().to_string()))
}

/// agy 沒有憑證可用時印的字（Keychain 讀不到、token 過期都一樣）；它接著會等人去開網址，不會自己結束。
fn auth_required(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("authentication required") || t.contains("please visit the url to log in")
}

/// 輸出裡的 JSON 那一行（`2>&1` 之後前後可能有警告）；`agy -p /usage --output-format json` 的 JSON 是單行。
fn json_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| l.starts_with('{') && l.ends_with('}'))
}

/// 探測失敗的分類：`AuthRequired` 是 agy 明說沒憑證（探測所在的環境讀得到 Keychain 時，這就是真的沒登入）；
/// 其餘是「已登入但額度這次拿不到」，帶一個固定的 `reason` 給網頁翻成人話。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ProbeFail {
    AuthRequired,
    Other { reason: &'static str, message: String },
}

impl ProbeFail {
    fn other(reason: &'static str, message: impl Into<String>) -> Self {
        Self::Other { reason, message: message.into() }
    }
}

fn tail(text: &str, n: usize) -> String {
    let t = text.trim();
    let skip = t.chars().count().saturating_sub(n);
    t.chars().skip(skip).collect()
}

/// 讀 agy 的 `/usage` 輸出（pane 的 BEGIN–DONE 之間，或本機的 stdout）：讀得到額度就回，否則說明為什麼沒有。
fn read_usage(host: &str, rc: Option<i32>, text: &str) -> Result<Vec<(&'static str, Quota)>, ProbeFail> {
    if let Some(found) = json_line(text).and_then(parse_usage).or_else(|| parse_usage(text)) {
        return Ok(found);
    }
    if auth_required(text) {
        return Err(ProbeFail::AuthRequired);
    }
    match rc {
        Some(rc) if rc != 0 => Err(ProbeFail::other("exit", format!("`agy -p /usage` on {host} exited with {rc}: {}", tail(text, 200)))),
        _ => Err(ProbeFail::other("unreadable", format!("`agy -p /usage` on {host} printed no readable quota window: {}", tail(text, 200)))),
    }
}

async fn pane_probe(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence, exe: &str) -> Result<Vec<(&'static str, Quota)>, ProbeFail> {
    let client = crate::quota_claude::client_for_fence(host, fence).await.map_err(|e| ProbeFail::other("not_connected", format!("{e:#}")))?;
    let home = crate::hosts::home_for_fence(fence).await.map_err(|e| ProbeFail::other("pane", format!("{e:#}")))?;
    if !app.hosts.is_current(fence).await {
        return Err(ProbeFail::other("superseded", format!("host `{host}` changed before its agy quota probe")));
    }
    let label = crate::quota_claude::tagged_probe_label(app, host, PROBE_LABEL_PREFIX.to_string()).await;
    let cmd = pane_probe_command(Some(exe));
    let run = crate::quota_claude::run_marked_pane(
        &client,
        &home,
        &label,
        serde_json::json!({}),
        &cmd,
        PROBE_TIMEOUT,
        PANE_BEGIN,
        is_probe_label,
        pane_done,
        |screen| auth_required(after_begin(screen)),
    )
    .await
    .map_err(|e| ProbeFail::other("pane", format!("could not run the agy quota probe pane on {host}: {e:#}")))?;
    if !app.hosts.is_current(fence).await {
        return Err(ProbeFail::other("superseded", format!("host `{host}` was reconnected/reconfigured during the agy quota probe; stale result discarded")));
    }
    use crate::quota_claude::MarkedRun;
    match run {
        MarkedRun::Done((rc, text)) => read_usage(host, Some(rc), &text),
        MarkedRun::Early(_) => Err(ProbeFail::AuthRequired),
        MarkedRun::TimedOut(screen) => Err(ProbeFail::other(
            "timeout",
            format!("`agy -p /usage` on {host} did not finish within {}s; screen: {}", PROBE_TIMEOUT.as_secs(), tail(after_begin(&screen), 200)),
        )),
    }
}

/// 探測這一輪失敗的原因（每台主機一筆，成功或判定為未登入就清掉）：網頁在「已登入、但額度暫時拿不到」那一格講清楚，
/// 不再停在永遠的「背景查詢中」。只在記憶體：重啟就當沒失敗過，下一輪探測自然會再記。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProbeError {
    pub reason: &'static str,
    pub message: String,
    pub at: String,
}

fn probe_errors() -> &'static std::sync::Mutex<std::collections::HashMap<String, ProbeError>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, ProbeError>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// 換掉那台主機記著的失敗（`None`＝清掉）；回傳內容有沒有變（沒變就不用再推 `host_changed`）。
fn set_probe_error(host: &str, err: Option<ProbeError>) -> bool {
    let mut m = probe_errors().lock().unwrap();
    let same = match (&err, m.get(host)) {
        (None, None) => true,
        (Some(a), Some(b)) => a.reason == b.reason && a.message == b.message,
        _ => false,
    };
    if same {
        return false;
    }
    match err {
        Some(e) => m.insert(host.to_string(), e),
        None => m.remove(host),
    };
    true
}

/// `tools` 的 JSON，`agy` 那筆帶上 `quota_error`（`{reason, message, at}`）；沒有失敗記錄就跟 `json!(tools)` 一模一樣。
/// 偵測結果（`app.tools`）會被整份換掉，失敗記錄放在這裡，序列化時才合進去。
pub fn tools_json(host: &str, tools: &std::collections::BTreeMap<String, crate::tools::ToolInfo>) -> serde_json::Value {
    let mut v = serde_json::json!(tools);
    if let Some(e) = probe_errors().lock().unwrap().get(host) {
        if let Some(agy) = v.get_mut("agy").and_then(|a| a.as_object_mut()) {
            agy.insert("quota_error".into(), serde_json::json!({"reason": e.reason, "message": e.message, "at": e.at}));
        }
    }
    v
}

/// agy 明說沒憑證之後，登入偵測（[`login_watch_once`]）暫時不要又因為 Keychain 裡有項目而把旗標翻回已登入、再開一次 pane。
const AUTH_DENIED_COOLDOWN: Duration = Duration::from_secs(5 * 60);

fn auth_denied() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn auth_denied_active(key: &str) -> bool {
    auth_denied().lock().unwrap().get(key).is_some_and(|t| *t > std::time::Instant::now())
}

/// 把這一輪的結果記下來：成功清掉失敗、未登入翻旗標、其他失敗留下原因；有變就推 `host_changed`。
async fn record_probe_result(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence, result: &Result<(), ProbeFail>) {
    let key = crate::quota::quota_key(host, "agy");
    let mut changed = false;
    match result {
        Ok(()) => {
            auth_denied().lock().unwrap().remove(&key);
            changed |= set_probe_error(host, None);
            // 探測讀得到額度＝這台已登入（登入後第一次探測成功就把「未登入」翻回來）。
            changed |= set_logged_in_quiet(app, host, fence, true).await;
        }
        Err(ProbeFail::AuthRequired) => {
            auth_denied().lock().unwrap().insert(key, std::time::Instant::now() + AUTH_DENIED_COOLDOWN);
            changed |= set_probe_error(host, None);
            changed |= set_logged_in_quiet(app, host, fence, false).await;
        }
        Err(ProbeFail::Other { reason, message }) => {
            changed |= set_probe_error(host, Some(ProbeError { reason, message: message.clone(), at: crate::db::now() }));
        }
    }
    if changed {
        crate::state::emit_host_changed(app, fence).await;
    }
}

/// `Ok(false)` ＝這台主機沒裝 agy（不探測、不報錯）。失敗時錯誤訊息就是原因（同時記在 [`tools_json`] 的 `quota_error`）。
pub async fn refresh_agy(app: &Arc<App>, host: &str) -> Result<bool> {
    let _guard = crate::quota::probe_lock(&format!("{host}#agy")).await;
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    if !app.tools.lock().await.contains_key(host) {
        crate::tools::detect(app, host).await?;
    }
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` changed before its agy quota probe");
    }
    let Some(exe) = crate::tools::cached_path(app, host, "agy").await else { return Ok(false) };
    let parsed = if fence.conn().is_local() {
        match crate::hosts::sh_local_stdout(&probe_script(Some(&exe)), PROBE_TIMEOUT, "`agy -p /usage`").await {
            Ok(stdout) => read_usage(host, None, &stdout),
            Err(e) => Err(ProbeFail::other("timeout", format!("{e:#}"))),
        }
    } else {
        pane_probe(app, host, &fence, &exe).await
    };
    let buckets = match parsed {
        Ok(b) => b,
        Err(fail) => {
            // 主機換掉了：這個結果不屬於現在這台，別記。
            if !matches!(&fail, ProbeFail::Other { reason: "superseded", .. }) {
                record_probe_result(app, host, &fence, &Err(fail.clone())).await;
            }
            return Err(match fail {
                ProbeFail::AuthRequired => anyhow!("agy on {host} is not logged in (`agy -p /usage` printed `Authentication required`)"),
                ProbeFail::Other { message, .. } => anyhow!(message),
            });
        }
    };
    for (key, q) in buckets {
        crate::quota::set_fenced(app, host, key, q, &fence).await?;
    }
    record_probe_result(app, host, &fence, &Ok(())).await;
    Ok(true)
}

/// agy 的登入憑證（Google OAuth token）：Linux（agm-host 實測）是這個檔（600）；**macOS 沒有檔，放 login Keychain**
/// （service `gemini`、account `antigravity`，agy log 寫 `authenticated via keyring`；2026-10-06 在 m4p 實測）。`agy` 沒有 `logout` 子命令，TUI 的 `/logout` 做的也是清憑證。
pub const TOKEN_FILE: &str = ".gemini/antigravity-cli/antigravity-oauth-token";

/// 遠端同一件事：只刪憑證（檔案、macOS 的 Keychain 項目）、不跑 agy。`AM_REMOVED`＝刪掉了，`AM_ABSENT`＝本來就沒有，`AM_FAILED`＝想刪沒刪成。
const REMOTE_LOGOUT_SCRIPT: &str = "f=\"$HOME/.gemini/antigravity-cli/antigravity-oauth-token\"; r=0; \
if [ -e \"$f\" ] || [ -L \"$f\" ]; then rm -f \"$f\" && r=1 || { echo AM_FAILED; exit 0; }; fi; \
if command -v security >/dev/null 2>&1 && security find-generic-password -s gemini -a antigravity >/dev/null 2>&1; then \
security delete-generic-password -s gemini -a antigravity >/dev/null 2>&1 && r=1 || { echo AM_FAILED; exit 0; }; fi; \
[ $r = 1 ] && echo AM_REMOVED || echo AM_ABSENT";

/// 本機 macOS 的 Keychain 項目在不在／刪掉。測試的假 HOME 不會換 Keychain，所以 `cfg(test)` 一律當不存在、不碰——不然測試會讀到（甚至刪掉）真的登入。
#[cfg(all(target_os = "macos", not(test)))]
async fn local_keychain(args: &[&str]) -> bool {
    let run = tokio::process::Command::new("security").args(args).args(["-s", "gemini", "-a", "antigravity"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    matches!(tokio::time::timeout(Duration::from_secs(10), run).await, Ok(Ok(s)) if s.success())
}
#[cfg(not(all(target_os = "macos", not(test))))]
async fn local_keychain(_args: &[&str]) -> bool {
    false
}

/// 測試行程共用一個假 HOME：碰憑證檔的測試（這裡與 `api::agy_logout_route_tests`）一個一個來。
#[cfg(test)]
pub(crate) async fn token_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static L: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    L.lock().await
}

/// 憑證在不在：檔案（非空）或 macOS 的 Keychain 項目。遠端跑一小段 sh，本機直接 stat（macOS 再問 `security`）；問不出來是 `None`（不改現有的判斷）。
async fn token_present(fence: &crate::hosts::HostFence) -> Option<bool> {
    if fence.conn().is_local() {
        let home = crate::home::dir()?;
        if std::fs::metadata(home.join(TOKEN_FILE)).map(|m| m.len() > 0).unwrap_or(false) {
            return Some(true);
        }
        return Some(local_keychain(&["find-generic-password"]).await);
    }
    if !fence.conn().is_connected() {
        return None;
    }
    let out = fence.conn().ssh_exec_path_timeout(REMOTE_PRESENT_SCRIPT, Duration::from_secs(10)).await.ok()?;
    match out.trim() {
        "AM_YES" => Some(true),
        "AM_NO" => Some(false),
        _ => None,
    }
}

const REMOTE_PRESENT_SCRIPT: &str = "if [ -s \"$HOME/.gemini/antigravity-cli/antigravity-oauth-token\" ] || { command -v security >/dev/null 2>&1 && security find-generic-password -s gemini -a antigravity >/dev/null 2>&1; }; then echo AM_YES; else echo AM_NO; fi";

/// 把 `tools.agy.logged_in` 寫成 `logged_in`（已知有裝 agy 才寫），有變就推 `host_changed`。額度那格的「未登入」與登入鈕吃這個旗標（網頁 `useLoggedOut`）。
async fn set_logged_in(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence, logged_in: bool) -> bool {
    let changed = set_logged_in_quiet(app, host, fence, logged_in).await;
    if changed {
        crate::state::emit_host_changed(app, fence).await;
    }
    changed
}

/// 同上但不推：呼叫端還有別的東西要一起變（[`record_probe_result`] 一輪只推一次）。
async fn set_logged_in_quiet(app: &Arc<App>, host: &str, fence: &crate::hosts::HostFence, logged_in: bool) -> bool {
    let mut all = app.tools.lock().await;
    if !app.hosts.is_current(fence).await {
        return false;
    }
    match all.get_mut(host).and_then(|h| h.tools.get_mut("agy")) {
        Some(t) if t.installed && t.logged_in != Some(logged_in) => {
            t.logged_in = Some(logged_in);
            true
        }
        _ => false,
    }
}

async fn agy_state(app: &impl crate::tools::ToolsTable, host: &str) -> Option<(bool, Option<bool>)> {
    app.tools().lock().await.get(host).and_then(|h| h.tools.get("agy")).map(|t| (t.installed, t.logged_in))
}

#[derive(Debug)]
pub enum LogoutError {
    UnknownHost,
    Failed(String),
}

/// `POST /api/hosts/{name}/agy/logout`：刪掉那台主機的 agy 憑證檔，再清掉那台 agy 的額度快照（`agy`）並廣播，
/// 那一格立刻變成沒有讀數。回「檔案原本在不在」（不在不算錯）。不跑 agy、不動別的檔、不停正在跑的 agy bot（它們下次重啟才會停在登入畫面）。
/// 拿探測的鎖：正在跑的探測不能在清掉之後又把讀數寫回來。
pub async fn logout(app: &Arc<App>, host: &str) -> Result<bool, LogoutError> {
    let fence = app.hosts.fence(host).await.ok_or(LogoutError::UnknownHost)?;
    let _guard = crate::quota::probe_lock(&format!("{host}#agy")).await;
    let removed = if fence.conn().is_local() {
        let home = crate::home::dir().ok_or_else(|| LogoutError::Failed("no home directory".into()))?;
        let file = match std::fs::remove_file(home.join(TOKEN_FILE)) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(LogoutError::Failed(format!("cannot remove the agy credentials: {e}"))),
        };
        let keychain = local_keychain(&["find-generic-password"]).await;
        if keychain && !local_keychain(&["delete-generic-password"]).await {
            return Err(LogoutError::Failed("cannot remove the agy credentials from the Keychain".into()));
        }
        file || keychain
    } else {
        if !fence.conn().is_connected() {
            return Err(LogoutError::Failed(format!("host `{host}` is not connected")));
        }
        let out = fence.conn().ssh_exec_path_timeout(REMOTE_LOGOUT_SCRIPT, PROBE_TIMEOUT).await.map_err(|e| LogoutError::Failed(format!("{e:#}")))?;
        match out.trim() {
            "AM_REMOVED" => true,
            "AM_ABSENT" => false,
            other => return Err(LogoutError::Failed(format!("remote agy logout did not confirm: {other}"))),
        }
    };
    if !app.hosts.is_current(&fence).await {
        return Err(LogoutError::Failed(format!("host `{host}` changed during the agy logout")));
    }
    let key = crate::quota::quota_key(host, "agy");
    let had = app.quotas.lock().await.contains_key(&key);
    crate::quota::forget(app, &key).await;
    if had {
        app.emit("quota_updated", serde_json::json!({"kind": key, "host": host, "quota": null})).await;
    }
    // 額度那格要立刻變成「未登入」並出現登入鈕，不等下一輪偵測。
    set_logged_in(app, host, &fence, false).await;
    tracing::info!(host, removed, "agy logged out: credentials removed and quota snapshots cleared");
    Ok(removed)
}

fn backoff() -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// `None` ＝這一輪跳過（上次失敗還在冷卻）。`GET /api/quota?refresh=1` 不走這裡，一律真的探測。
pub async fn refresh_agy_if_due(app: &Arc<App>, host: &str) -> Result<Option<bool>> {
    // 已知未登入：`agy -p /usage` 會停在登入畫面等到逾時，不探測；登入後由 [`spawn_agy_login_watcher`] 發現憑證檔再探。
    if matches!(agy_state(app, host).await, Some((true, Some(false)))) {
        return Ok(None);
    }
    let key = crate::quota::quota_key(host, "agy");
    if backoff().lock().unwrap().get(&key).is_some_and(|t| *t > std::time::Instant::now()) {
        return Ok(None);
    }
    match refresh_agy(app, host).await {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            backoff().lock().unwrap().insert(key, std::time::Instant::now() + RETRY_AFTER_FAILURE);
            Err(e)
        }
    }
}

pub fn spawn_agy_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            crate::quota::for_each_host(crate::quota::pollable_hosts(&app).await, |host| {
                let app = app.clone();
                async move {
                    match refresh_agy_if_due(&app, &host).await {
                        Ok(Some(true) | None) => {}
                        Ok(Some(false)) => tracing::debug!(host = %host, "agy not installed; agy quota stays null"),
                        Err(e) => tracing::warn!(host = %host, error = %e, retry_in_s = RETRY_AFTER_FAILURE.as_secs(), "agy quota refresh failed; keeping the last reading"),
                    }
                }
            })
            .await;
            tokio::time::sleep(AGY_POLL).await;
        }
    });
}

/// 登入偵測的間隔：只對「裝了 agy、目前記成未登入」的主機做一次 stat（遠端是一個很小的 ssh），或補一次還沒有讀數的探測。
pub const LOGIN_WATCH: Duration = Duration::from_secs(20);

/// 使用者在 shell 裡把 agy 登好之後，不重啟、不等 5 分鐘輪詢：憑證檔一出現就把 `tools.agy.logged_in` 翻成已登入、
/// 清掉探測冷卻並馬上探測一次額度，兩條額度就回到那一格。另外，已登入但還沒有任何讀數（例如手動「重新偵測」才翻成已登入）也補探一次。
pub async fn login_watch_once(app: &Arc<App>, host: &str) {
    // 沒裝、或這輪偵測沒問出登入與否（`None`）：什麼都不猜。
    let Some((true, Some(logged_in))) = agy_state(app, host).await else { return };
    let Some(fence) = app.hosts.fence(host).await else { return };
    let key = crate::quota::quota_key(host, "agy");
    if !logged_in {
        // agy 剛明說沒憑證：Keychain 裡有項目（可能是過期的 token）不算登好了，冷卻期內不翻回去、不再開 pane。
        if auth_denied_active(&key) || token_present(&fence).await != Some(true) {
            return;
        }
        set_logged_in(app, host, &fence, true).await;
        backoff().lock().unwrap().remove(&key);
    } else if app.quotas.lock().await.contains_key(&key) {
        return;
    }
    if let Err(e) = refresh_agy_if_due(app, host).await {
        tracing::warn!(host, error = %e, "agy quota probe after login failed");
    }
}

pub fn spawn_agy_login_watcher(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LOGIN_WATCH).await;
            for host in crate::quota::pollable_hosts(&app).await {
                login_watch_once(&app, &host).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 新版 command.data fixture：Gemini 與 Claude/GPT 組都有 weekly 與 5h 桶；只使用 Gemini。
    fn real() -> String {
        json!({"conversation_id": "", "status": "SUCCESS", "command": {"data": {"groups": [
            {"name": "Gemini Models", "buckets": [
                {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.98, "reset_time": "2026-10-11T15:39:29Z"},
                {"id": "gemini-5h", "name": "Five Hour Limit Remaining", "window": "five_hour", "remaining_fraction": 0.23, "reset_time": "2026-10-05T15:00:00Z"}
            ]},
            {"name": "Claude and GPT models", "buckets": [
                {"id": "claude-gpt-weekly", "window": "weekly", "remaining_fraction": 1.0, "reset_time": "2026-10-11T15:56:55Z"},
                {"id": "claude-gpt-5h", "name": "Five Hour Limit Remaining", "window": "five_hour", "remaining_fraction": 0.67, "reset_time": "2026-10-05T15:30:00Z"}
            ]}
        ]}}})
        .to_string()
    }

    #[test]
    fn the_real_output_keeps_both_gemini_windows_and_ignores_claude_gpt() {
        let got = parse_usage(&real()).expect("parsed");
        assert_eq!(got.iter().map(|(k, _)| *k).collect::<Vec<_>>(), ["agy"]);
        let (_, gemini) = &got[0];
        let week = gemini.seven_day.as_ref().unwrap();
        assert!((week.used_pct - 2.0).abs() < 1e-9, "98% remaining = 2% used");
        assert_eq!(week.resets_at.as_deref(), Some("2026-10-11T15:39:29.000Z"));
        let five = gemini.five_hour.as_ref().unwrap();
        assert!((five.used_pct - 77.0).abs() < 1e-9, "remaining_fraction .23 = 77% used");
        assert_eq!(five.resets_at.as_deref(), Some("2026-10-05T15:00:00.000Z"));
        assert!(gemini.fable.is_none());
        assert_eq!(gemini.source, "agy-usage");
    }

    #[test]
    fn the_legacy_response_format_still_populates_its_weekly_window() {
        let legacy = json!({"response": "Gemini Models\tWeekly Limit Remaining\t98%\t2026-10-11T15:39:29Z\nClaude and GPT models\tWeekly Limit Remaining\t100%\t2026-10-11T15:56:55Z\n"}).to_string();
        let got = parse_usage(&legacy).expect("legacy output remains readable");
        assert_eq!(got.len(), 1, "legacy Claude/GPT row is ignored");
        assert_eq!(got[0].1.seven_day.as_ref().unwrap().used_pct, 2.0);
        assert!(got[0].1.five_hour.is_none());
    }

    #[test]
    fn a_format_change_is_none_never_a_made_up_number() {
        for bad in [
            "",
            "not json",
            r#"{"response": 5}"#,
            r#"{"status":"SUCCESS"}"#,
            r#"{"response": "nothing useful here\n"}"#,
            r#"{"response": "Gemini Models\tWeekly Limit Remaining\tunknown\t2026-10-11T15:39:29Z"}"#,
            r#"{"response": "Gemini Models\tDaily Limit Remaining\t90%\t2026-10-11T15:39:29Z"}"#,
            r#"{"command":{"data":{"groups":[{"name":"Gemini Models","buckets":[{"id":"gemini-5h","window":"five_hour","remaining_fraction":1.4}]}]}}}"#,
            r#"{"command":{"data":{"groups":[{"name":"Claude and GPT models","buckets":[{"id":"claude-gpt-5h","window":"five_hour","remaining_fraction":0.5}]}]}}}"#,
            r#"{"response": "Some Other Models\tWeekly Limit Remaining\t90%\t2026-10-11T15:39:29Z"}"#,
        ] {
            assert!(parse_usage(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn extra_columns_a_missing_reset_and_out_of_range_percentages_are_tolerated() {
        let r = json!({"response": "Gemini Models\tWeekly Limit Remaining\t120%\nClaude and GPT models\tWeekly Limit Remaining\t40%\tnote\t2026-10-11T00:00:00+00:00\n"}).to_string();
        let got = parse_usage(&r).unwrap();
        let g = got[0].1.seven_day.as_ref().unwrap();
        assert_eq!((g.used_pct, g.resets_at.as_deref()), (0.0, None), "超過 100% 夾進範圍、沒有重置時間就是 None");
        assert_eq!(got.len(), 1, "Claude/GPT weekly row is ignored");
        // 只有一桶也算（另一桶之後再說）。
        let one = json!({"response": "Gemini Models\tWeekly Limit Remaining\t50%\t2026-10-11T00:00:00Z"}).to_string();
        assert_eq!(parse_usage(&one).unwrap().len(), 1);
    }

    #[test]
    fn the_probe_runs_in_a_throwaway_dir_without_updates_or_stdin_and_cleans_up() {
        let s = probe_script(Some("/home/u/.local/bin/agy"));
        assert!(s.contains("mktemp -d") && s.contains("rm -rf \"$d\""), "{s}");
        assert!(s.contains("AGY_CLI_DISABLE_AUTO_UPDATE=true") && s.contains("'/home/u/.local/bin/agy' -p /usage --output-format json </dev/null"), "{s}");
        assert!(probe_script(Some("/tmp/a b/agy")).contains("'/tmp/a b/agy'"), "路徑要 quote");
        assert!(probe_script(None).contains(" agy -p /usage") || probe_script(None).contains("'agy' -p /usage") || probe_script(None).contains("agy -p /usage"));
    }

    /// 真的跑一次那段 shell：用假 `agy`（輸出真機的 JSON），確認在別的目錄跑、目錄用完就刪、環境變數有帶、exit code 傳得出來。
    #[test]
    fn the_script_runs_a_fake_agy_in_a_temp_dir_that_is_gone_afterwards() {
        let dir = crate::testing::scratch_dir("am-agy-quota");
        let fake = dir.join("agy");
        let seen = dir.join("seen.txt");
        crate::testing::write_exec(
            &fake,
            format!("#!/bin/sh\npwd > {s}\necho \"$AGY_CLI_DISABLE_AUTO_UPDATE $*\" >> {s}\nprintf '%s' '{j}'\n", s = seen.display(), j = real()),
        );
        let out = crate::exec_retry::output(std::process::Command::new("/bin/sh").arg("-c").arg(probe_script(fake.to_str()))).unwrap();
        assert!(out.status.success());
        assert!(parse_usage(&String::from_utf8_lossy(&out.stdout)).is_some());
        let seen = std::fs::read_to_string(&seen).unwrap();
        let mut lines = seen.lines();
        let cwd = lines.next().unwrap();
        assert!(!cwd.starts_with(dir.to_str().unwrap()) && !std::path::Path::new(cwd).exists(), "拋棄式 cwd 用完刪掉：{cwd}");
        assert_eq!(lines.next().unwrap(), "true -p /usage --output-format json");
    }

    /// macOS 的 agy 憑證在 Keychain、沒有檔案（m4p 實測）：用假 `security`（PATH 前面）跑真的 sh，確認「有 Keychain 項目」算已登入、
    /// 登出會刪它；沒有 `security`（Linux）時只看檔案。假 HOME 底下沒有憑證檔。
    #[test]
    fn a_keychain_item_counts_as_logged_in_and_logout_deletes_it() {
        let dir = crate::testing::scratch_dir("am-agy-keychain");
        let home = dir.join("home");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let state = dir.join("item");
        crate::testing::write_exec(
            &bin.join("security"),
            format!(
                "#!/bin/sh\ncase \"$1 $2 $3 $4 $5\" in\n\"find-generic-password -s gemini -a antigravity\"*) [ -e {s} ] ;;\n\"delete-generic-password -s gemini -a antigravity\"*) rm -f {s} ;;\n*) exit 9 ;;\nesac\n",
                s = state.display()
            ),
        );
        // macOS 的 /usr/bin 有真的 `security`（會去問真的 Keychain）：沒有 security 的情境 PATH 只放一個空目錄，sh 內建的 `[`、`command` 夠用。
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let run = |script: &str, with_security: bool| {
            let path = if with_security { format!("{}:/usr/bin:/bin", bin.display()) } else { empty.display().to_string() };
            let out = crate::exec_retry::output(std::process::Command::new("/bin/sh").arg("-c").arg(script).env("HOME", &home).env("PATH", path)).unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(run(REMOTE_PRESENT_SCRIPT, true), "AM_NO", "沒檔也沒 Keychain 項目");
        std::fs::write(&state, "x").unwrap();
        assert_eq!(run(REMOTE_PRESENT_SCRIPT, true), "AM_YES", "只有 Keychain 項目也算已登入");
        assert_eq!(run(REMOTE_PRESENT_SCRIPT, false), "AM_NO", "沒有 security（Linux）時不猜");
        assert_eq!(run(REMOTE_LOGOUT_SCRIPT, true), "AM_REMOVED");
        assert!(!state.exists(), "Keychain 項目被刪");
        assert_eq!(run(REMOTE_LOGOUT_SCRIPT, true), "AM_ABSENT");
        // 檔案與 Keychain 都在：兩個都清。
        let tok = home.join(TOKEN_FILE);
        std::fs::create_dir_all(tok.parent().unwrap()).unwrap();
        std::fs::write(&tok, "t").unwrap();
        std::fs::write(&state, "x").unwrap();
        assert_eq!(run(REMOTE_PRESENT_SCRIPT, false), "AM_YES", "檔案照舊算");
        assert_eq!(run(REMOTE_LOGOUT_SCRIPT, true), "AM_REMOVED");
        assert!(!tok.exists() && !state.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_host_without_agy_is_not_probed_and_not_an_error() {
        let e = crate::testing::env().await;
        let tools = |agy: bool| crate::tools::HostTools {
            tools: agy
                .then(|| ("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/opt/agy".into()), version: None, logged_in: None }))
                .into_iter()
                .collect(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        e.app.tools.lock().await.insert(LOCAL_HOST.into(), tools(false));
        assert!(!refresh_agy(&e.app, LOCAL_HOST).await.expect("no agy: Ok(false), no error"));
        assert!(e.app.quotas.lock().await.get("agy").is_none());
    }

    #[tokio::test]
    async fn gemini_five_hour_and_weekly_windows_share_one_key_and_failed_probe_keeps_the_old_reading() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        for (key, q) in parse_usage(&real()).unwrap() {
            crate::quota::set(&app, LOCAL_HOST, key, q).await;
        }
        // 兩個窗口都落在唯一的 Gemini key 上。
        let again = parse_usage(&real()).unwrap().remove(0);
        crate::quota::set(&app, LOCAL_HOST, again.0, again.1).await;
        let q = app.quotas.lock().await.clone();
        assert!(q.contains_key("agy"), "{:?}", q.keys().collect::<Vec<_>>());
        assert!(!q.contains_key("agy:claude-gpt"), "不建立舊的 Claude/GPT key");
        let before = q["agy"].seven_day.clone();
        // 探測壞掉（沒有 agy 輸出）：解析回 None，呼叫端不寫任何東西。
        assert!(parse_usage("garbage").is_none());
        assert_eq!(app.quotas.lock().await["agy"].seven_day, before);
        let snap = crate::quota::snapshot(&app).await;
        assert!(snap["kinds"]["agy"]["five_hour"]["used_pct"].is_number() && snap["kinds"]["agy"]["seven_day"]["used_pct"].is_number(), "{snap}");
        assert!(snap["kinds"].get("agy:claude-gpt").is_none(), "{snap}");
    }

    #[tokio::test]
    async fn retired_agy_claude_gpt_cache_is_purged_on_load_and_cannot_be_saved_again() {
        let e = crate::testing::env().await;
        let app = e.app.clone();
        let q = quota_row("agy", &crate::db::now()).1;
        let raw = serde_json::to_string(&q).unwrap();
        for key in ["agy:claude-gpt", "agy-remote/agy:claude-gpt"] {
            sqlx::query("INSERT INTO quota_cache (key, quota_json, updated_at) VALUES (?, ?, ?)")
                .bind(key)
                .bind(&raw)
                .bind(&q.updated_at)
                .execute(&app.db)
                .await
                .unwrap();
        }

        assert_eq!(crate::quota::load_cache(&app).await.unwrap(), 0);
        assert!(!app.quotas.lock().await.contains_key("agy:claude-gpt"));
        let old_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quota_cache WHERE key LIKE '%agy:claude-gpt'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(old_rows, 0, "startup purges local and remote old snapshots");

        crate::quota::set(&app, LOCAL_HOST, "agy:claude-gpt", q).await;
        assert!(!app.quotas.lock().await.contains_key("agy:claude-gpt"));
        let old_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quota_cache WHERE key LIKE '%agy:claude-gpt'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(old_rows, 0, "legacy callers cannot restore it");
    }
}

#[cfg(test)]
mod logout_tests {
    use super::*;
    use crate::testing as tt;

    fn token_path() -> std::path::PathBuf {
        crate::home::dir().unwrap().join(TOKEN_FILE)
    }

    async fn seed(app: &Arc<App>, host: &str) {
        for (key, q) in parse_usage(&json!({"response": "Gemini Models\tWeekly Limit Remaining\t98%\t2026-10-11T15:39:29Z\nClaude and GPT models\tWeekly Limit Remaining\t100%\t2026-10-11T15:56:55Z\n"}).to_string()).unwrap() {
            crate::quota::set(app, host, key, q).await;
        }
    }

    use serde_json::json;

    /// 本機：刪假 HOME 底下的憑證檔、清 Gemini 額度 key（連重啟快取）並廣播 `quota:null`；別的檔不動。
    #[tokio::test]
    async fn logging_out_removes_only_the_token_clears_gemini_quota_and_broadcasts() {
        let _lock = token_test_lock().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let tok = token_path();
        let other = tok.with_file_name("settings.json");
        std::fs::create_dir_all(tok.parent().unwrap()).unwrap();
        std::fs::write(&tok, "secret").unwrap();
        std::fs::write(&other, "{}").unwrap();
        seed(&app, LOCAL_HOST).await;
        let mut rx = app.subscribe();

        assert!(logout(&app, LOCAL_HOST).await.unwrap(), "檔案在 → removed");
        assert!(!tok.exists(), "憑證檔被刪");
        assert!(other.exists(), "不動別的檔");
        let q = app.quotas.lock().await;
        assert!(!q.contains_key("agy"), "Gemini 額度快照清掉");
        drop(q);
        let cached: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quota_cache WHERE key = 'agy'").fetch_one(&app.db).await.unwrap();
        assert_eq!(cached, 0, "重啟快取也清掉，不然重開機又顯示舊數字");
        let mut cleared = std::collections::BTreeSet::new();
        while let Ok(ev) = rx.try_recv() {
            if ev.kind == "quota_updated" && ev.data["quota"].is_null() {
                cleared.insert(ev.data["kind"].as_str().unwrap().to_string());
            }
        }
        assert_eq!(cleared, ["agy".to_string()].into(), "推一則 quota:null");

        // 檔案原本不在：false，不算錯。
        assert!(!logout(&app, LOCAL_HOST).await.unwrap());
        std::fs::remove_file(&other).unwrap();
    }

    #[tokio::test]
    async fn an_unknown_host_is_unknown_and_other_hosts_quota_is_untouched() {
        let _lock = token_test_lock().await;
        let e = tt::env().await;
        let app = e.app.clone();
        assert!(matches!(logout(&app, "no-such-host").await, Err(LogoutError::UnknownHost)));
        // 遠端那台的 agy 額度不受本機登出影響。
        let host = format!("agy-lo-{}", crate::db::ulid().to_ascii_lowercase());
        app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        seed(&app, &host).await;
        seed(&app, LOCAL_HOST).await;
        logout(&app, LOCAL_HOST).await.unwrap();
        assert!(app.quotas.lock().await.contains_key(&format!("{host}/agy")), "別台的快照還在");
    }

    /// 遠端：走 ssh（同探測那條），只送一段刪檔的 script；回 `AM_REMOVED`／`AM_ABSENT`／其他（失敗、502）。
    #[tokio::test]
    async fn a_remote_logout_runs_one_rm_script_over_ssh_and_clears_that_hosts_keys() {
        let e = tt::env().await;
        let app = e.app.clone();
        let host = format!("agy-lo-r-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        let replies = Arc::new(std::sync::Mutex::new(vec!["AM_REMOVED\n".to_string(), "AM_ABSENT\n".to_string(), "boom\n".to_string()]));
        let scripts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let (r2, s2) = (replies.clone(), scripts.clone());
        crate::hosts::set_ssh_fake(&host, move |script| {
            s2.lock().unwrap().push(script.to_string());
            Ok(r2.lock().unwrap().remove(0))
        });
        // 沒連線：不送 ssh，回錯。
        conn.connected.store(false, std::sync::atomic::Ordering::SeqCst);
        seed(&app, &host).await;
        assert!(matches!(logout(&app, &host).await, Err(LogoutError::Failed(m)) if m.contains("not connected")));
        assert!(scripts.lock().unwrap().is_empty());
        assert!(app.quotas.lock().await.contains_key(&format!("{host}/agy")), "失敗不清快照");

        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(logout(&app, &host).await.unwrap());
        let sent = scripts.lock().unwrap()[0].clone();
        assert!(sent.contains(".gemini/antigravity-cli/antigravity-oauth-token") && sent.contains("rm -f") && !sent.contains("agy -p"), "{sent}");
        assert!(!app.quotas.lock().await.contains_key(&format!("{host}/agy")));

        assert!(!logout(&app, &host).await.unwrap(), "AM_ABSENT → false");
        seed(&app, &host).await;
        assert!(matches!(logout(&app, &host).await, Err(LogoutError::Failed(m)) if m.contains("did not confirm")), "認不得的回覆＝失敗，快照留著");
        assert!(app.quotas.lock().await.contains_key(&format!("{host}/agy")));
    }
}

#[cfg(test)]
mod login_tests {
    use super::*;
    use crate::testing as tt;

    fn token_path() -> std::path::PathBuf {
        crate::home::dir().unwrap().join(TOKEN_FILE)
    }

    async fn install_agy(app: &Arc<App>, logged_in: Option<bool>) {
        let ht = crate::tools::HostTools {
            tools: [("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/nonexistent/agy".into()), version: None, logged_in })].into(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert(LOCAL_HOST.into(), ht);
    }

    async fn agy_logged_in(app: &Arc<App>) -> Option<bool> {
        app.tools.lock().await[LOCAL_HOST].tools["agy"].logged_in
    }

    /// 登出：憑證檔刪了、`tools.agy.logged_in` 立刻是 false 並推 `host_changed`（那一格馬上變「未登入」＋出現登入鈕）。
    #[tokio::test]
    async fn logging_out_flips_the_login_flag_at_once_and_pushes_host_changed() {
        let _lock = token_test_lock().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let tok = token_path();
        std::fs::create_dir_all(tok.parent().unwrap()).unwrap();
        std::fs::write(&tok, "secret").unwrap();
        install_agy(&app, Some(true)).await;
        let mut rx = app.subscribe();
        logout(&app, LOCAL_HOST).await.unwrap();
        assert_eq!(agy_logged_in(&app).await, Some(false));
        let mut pushed = false;
        while let Ok(ev) = rx.try_recv() {
            pushed |= ev.kind == "host_changed" && ev.data["tools"]["agy"]["logged_in"] == false;
        }
        assert!(pushed, "要推 host_changed，網頁才會翻成未登入");
    }

    /// 登入之後不用重啟：憑證檔出現，下一輪 watcher 就把旗標翻成已登入並探測一次額度（假 agy 路徑讀不到輸出＝探測失敗，旗標仍已翻）。
    /// 憑證檔還沒出現就什麼都不動；沒問出登入與否（`None`）也不猜。
    #[tokio::test]
    async fn the_watcher_flips_to_logged_in_once_the_token_file_appears() {
        let _lock = token_test_lock().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let tok = token_path();
        let _ = std::fs::remove_file(&tok);
        install_agy(&app, Some(false)).await;

        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, Some(false), "檔案還沒出現：不動");

        install_agy(&app, None).await;
        std::fs::create_dir_all(tok.parent().unwrap()).unwrap();
        std::fs::write(&tok, "secret").unwrap();
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, None, "沒問出登入與否：不猜");

        install_agy(&app, Some(false)).await;
        let mut rx = app.subscribe();
        login_watch_once(&app, LOCAL_HOST).await;
        assert_eq!(agy_logged_in(&app).await, Some(true), "憑證檔出現：翻成已登入");
        let mut pushed = false;
        while let Ok(ev) = rx.try_recv() {
            pushed |= ev.kind == "host_changed" && ev.data["tools"]["agy"]["logged_in"] == true;
        }
        assert!(pushed, "要推 host_changed");
        std::fs::remove_file(&tok).unwrap();
    }

    /// 已知未登入的主機，定期輪詢不去跑 `agy -p /usage`（那會停在登入畫面等到逾時）。
    #[tokio::test]
    async fn a_logged_out_host_is_not_probed_by_the_poller() {
        let e = tt::env().await;
        install_agy(&e.app, Some(false)).await;
        assert!(matches!(refresh_agy_if_due(&e.app, LOCAL_HOST).await, Ok(None)));
    }
}

/// 遠端額度探測走那台 herdr 的 pane（macOS 的 Keychain 純 ssh 讀不到）：用假 herdr 驗開 pane、讀畫面、失敗原因與登入旗標。
#[cfg(test)]
mod pane_tests {
    use super::*;
    use crate::herdr::HerdrClient;
    use crate::testing::MockHerdr;
    use serde_json::json;

    struct Remote {
        host: String,
        herdr: MockHerdr,
        ssh_calls: Arc<std::sync::Mutex<Vec<String>>>,
        _dir: std::path::PathBuf,
    }

    async fn remote(app: &Arc<App>, logged_in: Option<bool>) -> Remote {
        let host = format!("agy-pane-{}", crate::db::ulid().to_ascii_lowercase());
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-agy-pane-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let herdr = MockHerdr::start(sock.clone());
        let conn = app
            .hosts
            .insert_remote_with_client_for_test(
                crate::config::HostCfg {
                    shared_session: false,
                    name: host.clone(),
                    ssh: "unused".into(),
                    ssh_port: 22,
                    ssh_opts: vec![],
                    herdr_session: "agents-manager".into(),
                    remote_path: String::new(),
                },
                HerdrClient::new(sock),
            )
            .await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        *conn.remote_home.lock().await = Some("/Users/m4p".into());
        // 純 ssh 讀不到 Keychain：探測**不能**走 ssh（記下任何 ssh 呼叫）。
        let ssh_calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls = ssh_calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls.lock().unwrap().push(script.to_string());
            Err(anyhow!("ssh must not be used for the agy quota probe"))
        });
        let ht = crate::tools::HostTools {
            tools: [("agy".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/Users/m4p/.local/bin/agy".into()), version: None, logged_in })].into(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert(host.clone(), ht);
        Remote { host, herdr, ssh_calls, _dir: dir }
    }

    fn screen(body: &str, rc: i32) -> String {
        format!("{PANE_BEGIN}\n{body}\n{PANE_DONE}{rc}\n")
    }

    fn usage_json() -> String {
        json!({"status": "SUCCESS", "command": {"data": {"groups": [{"name": "Gemini Models", "buckets": [
            {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.98, "reset_time": "2026-10-11T15:39:29Z"},
            {"id": "gemini-5h", "window": "five_hour", "remaining_fraction": 0.23, "reset_time": "2026-10-05T15:00:00Z"}
        ]}]}}})
        .to_string()
    }

    async fn agy_flag(app: &Arc<App>, host: &str) -> Option<bool> {
        app.tools.lock().await[host].tools["agy"].logged_in
    }

    async fn open_workspaces(h: &MockHerdr) -> usize {
        for _ in 0..40 {
            if h.workspaces.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        h.workspaces.lock().unwrap().len()
    }

    fn quota_error_of(app_tools: &crate::tools::HostTools, host: &str) -> serde_json::Value {
        tools_json(host, &app_tools.tools)["agy"]["quota_error"].clone()
    }

    #[tokio::test]
    async fn a_remote_probe_runs_in_a_pane_not_over_ssh_and_stores_both_gemini_windows() {
        let app = crate::testing::env().await.app.clone();
        let r = remote(&app, Some(true)).await;
        r.herdr.set_screen("*", &screen(&usage_json(), 0));
        assert!(refresh_agy(&app, &r.host).await.unwrap());
        let q = app.quotas.lock().await;
        let got = &q[&format!("{}/agy", r.host)];
        assert!(got.five_hour.is_some() && got.seven_day.is_some(), "{got:?}");
        assert!(!q.contains_key("agy"), "遠端的讀數不能落在本機那格");
        drop(q);
        assert_eq!(r.ssh_calls.lock().unwrap().len(), 0, "不走 ssh（macOS 的 Keychain 純 ssh 讀不到）");
        let creates = r.herdr.calls_to("workspace.create");
        assert_eq!(creates.len(), 1, "{creates:?}");
        assert_eq!(creates[0]["cwd"], "/Users/m4p", "在遠端 home 開 pane");
        let typed: String = r.herdr.calls_to("pane.send_text").iter().filter_map(|c| c["text"].as_str().map(String::from)).collect();
        assert!(typed.contains("'/Users/m4p/.local/bin/agy' -p /usage --output-format json"), "{typed}");
        assert_eq!(open_workspaces(&r.herdr).await, 0, "探測完 workspace 關掉");
        assert_eq!(agy_flag(&app, &r.host).await, Some(true));
        assert!(quota_error_of(&app.tools.lock().await[&r.host], &r.host).is_null());
    }

    /// agy 在 pane 裡明說沒憑證：不等到逾時，旗標翻成未登入（網頁出現登入鈕），不留「額度暫時拿不到」的錯誤，
    /// 而且登入偵測冷卻期內不把旗標又翻回去。
    #[tokio::test]
    async fn authentication_required_in_the_pane_marks_logged_out_at_once() {
        let app = crate::testing::env().await.app.clone();
        let r = remote(&app, Some(true)).await;
        r.herdr.set_screen("*", &format!("{PANE_BEGIN}\nAuthentication required. Please visit the URL to log in\nhttps://accounts.example/x\n"));
        let started = std::time::Instant::now();
        let err = refresh_agy(&app, &r.host).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2), "不必等逾時：{:?}", started.elapsed());
        assert!(err.to_string().contains("not logged in"), "{err:#}");
        assert_eq!(agy_flag(&app, &r.host).await, Some(false));
        assert!(quota_error_of(&app.tools.lock().await[&r.host], &r.host).is_null(), "未登入由旗標表達，不另記額度錯誤");
        assert_eq!(open_workspaces(&r.herdr).await, 0, "卡在登入畫面的 pane 也關掉");
        assert_eq!(r.ssh_calls.lock().unwrap().len(), 0);

        // 冷卻期內：Keychain 裡有項目也不翻回去、不再開 pane。
        let before = r.herdr.calls_to("workspace.create").len();
        login_watch_once(&app, &r.host).await;
        assert_eq!(agy_flag(&app, &r.host).await, Some(false));
        assert_eq!(r.herdr.calls_to("workspace.create").len(), before);
    }

    /// 已登入但這次拿不到額度：原因記起來、跟 `tools.agy` 一起送出去（網頁寫「已登入，額度暫時拿不到」），旗標不動；下一次成功就清掉。
    #[tokio::test]
    async fn a_probe_that_times_out_keeps_logged_in_and_reports_why_until_it_recovers() {
        let app = crate::testing::env().await.app.clone();
        let r = remote(&app, Some(true)).await;
        let mut rx = app.subscribe();
        r.herdr.set_screen("*", &format!("{PANE_BEGIN}\n"));
        let err = refresh_agy(&app, &r.host).await.unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err:#}");
        assert_eq!(agy_flag(&app, &r.host).await, Some(true), "逾時不等於未登入");
        let e = quota_error_of(&app.tools.lock().await[&r.host], &r.host);
        assert_eq!(e["reason"], "timeout", "{e}");
        assert!(e["message"].as_str().unwrap().contains(&r.host) && e["at"].is_string(), "{e}");
        let mut pushed = false;
        while let Ok(ev) = rx.try_recv() {
            pushed |= ev.kind == "host_changed" && ev.data["name"] == r.host.as_str() && ev.data["tools"]["agy"]["quota_error"]["reason"] == "timeout";
        }
        assert!(pushed, "失敗原因要推 host_changed，網頁才換掉「背景查詢中」");

        // 恢復：下一次讀得到，錯誤清掉並再推一次。
        r.herdr.set_screen("*", &screen(&usage_json(), 0));
        assert!(refresh_agy(&app, &r.host).await.unwrap());
        assert!(quota_error_of(&app.tools.lock().await[&r.host], &r.host).is_null());
        let mut cleared = false;
        while let Ok(ev) = rx.try_recv() {
            cleared |= ev.kind == "host_changed" && ev.data["tools"]["agy"].get("quota_error").is_none();
        }
        assert!(cleared, "成功後清掉並推出去");
    }

    #[tokio::test]
    async fn an_unreadable_or_failing_agy_names_its_reason() {
        let app = crate::testing::env().await.app.clone();
        let r = remote(&app, Some(true)).await;
        r.herdr.set_screen("*", &screen("{\"status\":\"SUCCESS\"}", 0));
        refresh_agy(&app, &r.host).await.unwrap_err();
        assert_eq!(quota_error_of(&app.tools.lock().await[&r.host], &r.host)["reason"], "unreadable");
        r.herdr.set_screen("*", &screen("agy: command not found", 127));
        let err = refresh_agy(&app, &r.host).await.unwrap_err();
        assert!(err.to_string().contains("exited with 127"), "{err:#}");
        assert_eq!(quota_error_of(&app.tools.lock().await[&r.host], &r.host)["reason"], "exit");
        assert_eq!(agy_flag(&app, &r.host).await, Some(true));
    }

    #[test]
    fn the_failure_record_does_not_touch_other_hosts_or_other_tools() {
        let tools: std::collections::BTreeMap<String, crate::tools::ToolInfo> =
            [("agy".to_string(), Default::default()), ("claude".to_string(), Default::default())].into();
        let host = format!("agy-json-{}", crate::db::ulid().to_ascii_lowercase());
        assert_eq!(tools_json(&host, &tools), json!(tools), "沒有失敗記錄：跟原本的序列化一模一樣");
        assert!(set_probe_error(&host, Some(ProbeError { reason: "timeout", message: "m".into(), at: "t".into() })));
        assert!(!set_probe_error(&host, Some(ProbeError { reason: "timeout", message: "m".into(), at: "later".into() })), "同樣的原因不重複推");
        let v = tools_json(&host, &tools);
        assert_eq!(v["agy"]["quota_error"]["reason"], "timeout");
        assert!(v["claude"].get("quota_error").is_none());
        assert_eq!(tools_json("other-host", &tools), json!(tools));
        assert!(set_probe_error(&host, None));
        assert_eq!(tools_json(&host, &tools), json!(tools));
    }

    #[test]
    fn the_pane_output_parsers_read_the_last_complete_run_and_spot_the_login_wall() {
        // 指令重打過：捲動緩衝區有兩輪，只認最後那輪完整的。
        let two = format!("{PANE_BEGIN}\nold\n{PANE_BEGIN}\n{{\"a\":1}}\n{PANE_DONE}0\n");
        assert_eq!(pane_done(&two), Some((0, "{\"a\":1}".to_string())));
        assert_eq!(pane_done(&format!("{PANE_BEGIN}\nstill running")), None, "沒有 DONE＝還沒結束");
        assert_eq!(pane_done(&format!("{PANE_DONE}1\n")), Some((1, String::new())), "mktemp 失敗：只有 DONE");
        assert_eq!(pane_done(&format!("{PANE_BEGIN}\nx\n{PANE_DONE}zz\n")).map(|x| x.0), Some(1), "結束碼讀不懂當失敗");
        assert!(auth_required("Authentication required. Please visit the URL to log in"));
        assert!(auth_required("AUTHENTICATION REQUIRED"));
        assert!(!auth_required("{\"status\":\"SUCCESS\"}"));
        assert_eq!(json_line("warning: x\n  {\"a\":1}  \ntrailer"), Some("{\"a\":1}"));
        assert_eq!(json_line("no json here"), None);
        // 登入牆只看最後一次 BEGIN 之後：之前畫面上的字不算。
        assert!(!auth_required(after_begin(&format!("Authentication required\n{PANE_BEGIN}\nok\n"))));
    }

    /// 真的把 pane 那一行丟給 sh：假 agy 在別的目錄跑、目錄用完就刪、標記與結束碼印得出來；壞掉的 agy 結束碼傳得出來。
    #[test]
    fn the_pane_command_runs_in_a_throwaway_dir_and_prints_the_markers() {
        let dir = crate::testing::scratch_dir("am-agy-pane-cmd");
        let seen = dir.join("seen.txt");
        let fake = dir.join("agy");
        crate::testing::write_exec(&fake, format!("#!/bin/sh\npwd > {s}\necho \"$AGY_CLI_DISABLE_AUTO_UPDATE $*\" >> {s}\nprintf '%s\\n' '{j}'\nexit 3\n", s = seen.display(), j = usage_json()));
        let out = crate::exec_retry::output(std::process::Command::new("/bin/sh").arg("-c").arg(pane_probe_command(fake.to_str()))).unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let (rc, body) = pane_done(&text).expect(&text);
        assert_eq!(rc, 3, "agy 的結束碼傳得出來");
        assert!(read_usage("h", Some(rc), &body).is_ok(), "JSON 那行照樣讀得到");
        let seen = std::fs::read_to_string(&seen).unwrap();
        let mut lines = seen.lines();
        let cwd = lines.next().unwrap();
        assert!(!cwd.starts_with(dir.to_str().unwrap()) && !std::path::Path::new(cwd).exists(), "拋棄式 cwd 用完刪掉：{cwd}");
        assert_eq!(lines.next().unwrap(), "true -p /usage --output-format json");
        assert!(!pane_probe_command(None).contains(PANE_BEGIN) && !pane_probe_command(None).contains(PANE_DONE), "回顯的指令不能長得像標記");
    }

    /// claude 與 agy 的探測 workspace 各清各的殘留：同台主機同時跑時，不能互相收掉對方正在跑的。
    #[test]
    fn claude_and_agy_probe_labels_do_not_sweep_each_other() {
        assert!(is_probe_label("am-quota-agy") && !is_probe_label("am-quota-claude") && !is_probe_label("am-quota-claude-cc1") && !is_probe_label("am-quota-grok"));
        assert!(crate::shared_host::sweepable("am-quota-agy@t", Some("t"), is_probe_label));
        assert!(!crate::shared_host::sweepable("am-quota-agy@other", Some("t"), is_probe_label));
    }
}
