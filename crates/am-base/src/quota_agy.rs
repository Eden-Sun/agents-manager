//! agy（Antigravity CLI）額度（SPEC §12a.7）。數字來自 `agy -p "/usage" --output-format json`：唯讀指令，**不開對話、不耗額度**，
//! 所以不必像 grok 那樣開一顆探測 pane——在拋棄式暫存目錄裡直接跑、有逾時、跑完刪目錄。
//!
//! 新版輸出在 `command.data.groups[].buckets[]`：只讀 Gemini 組的 weekly 與 five-hour 窗口，`remaining_fraction` 是剩餘比例，
//! `reset_time` 是 RFC3339 重置時刻。Claude/GPT 組不屬於 AG Man 的 agy 額度。舊版 tab 分隔 `response` 仍接受；
//! 探測失敗**不覆蓋**舊值（讀數自己會變陳舊，`quota::STALE_AFTER`）；格式讀不懂回 `None`，不編數字。

use crate::config::LOCAL_HOST;
use crate::quota::{Quota, Window};
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;

/// 每次探測要起一個約 200 MB 的執行檔。
pub const AGY_POLL: std::time::Duration = std::time::Duration::from_secs(300);
/// 測試縮短（探測逾時的測試不必真的等 40 秒）。daemon 的測試是以 `test-hooks`（dev-dependency）編 am-base，`cfg(test)` 對 am-base 本身不成立，所以兩個都要認；正式建置不開 `test-hooks`。
pub const PROBE_TIMEOUT: std::time::Duration = if cfg!(any(test, feature = "test-hooks")) { std::time::Duration::from_secs(10) } else { std::time::Duration::from_secs(40) };

/// 探測指令（本機與遠端同一份）：拋棄式 cwd、不讀 stdin、關自動更新、跑完刪目錄。`exe` 是偵測到的絕對路徑，沒有就用 PATH 上的 `agy`。
pub fn probe_script(exe: Option<&str>) -> String {
    probe_script_with_env(exe, &BTreeMap::new())
}

/// 同 [`probe_script`]，但 agy 前面帶身份的環境變數（`HOME=<身份目錄>`，已展開）。登入、登出、身份重驗、探測都用同一個 env 產生方式（[`env_assignments`]）。
pub fn probe_script_with_env(exe: Option<&str>, env: &BTreeMap<String, String>) -> String {
    let exe = crate::hosts::sh_quote(exe.unwrap_or("agy"));
    let assign = env_assignments(env);
    format!(
        "d=$(mktemp -d 2>/dev/null) || exit 1; cd \"$d\" || exit 1; \
         AGY_CLI_DISABLE_AUTO_UPDATE=true {assign}{exe} -p /usage --output-format json </dev/null; rc=$?; cd / ; rm -rf \"$d\"; exit $rc"
    )
}

/// 身份的 env 變成 shell 前綴：`K='v' K2='v2' `（名字不合法的略過，值用 sh_quote）。空 map＝空字串。
pub fn env_assignments(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .filter(|(k, _)| crate::tools::valid_env_name(k))
        .map(|(k, v)| format!("{k}={} ", crate::hosts::sh_quote(v)))
        .collect()
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

pub fn quota_row(key: &'static str, now: &str) -> (&'static str, Quota) {
    (
        key,
        Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            argon_weekly: None,
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

fn window_is_weekly(bucket: &Value) -> bool {
    matches!(bucket_window(bucket), Some(UsageWindow::SevenDay))
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

/// Gemini 4 Argon 的獨立週額度（`Gemini 4 Argon` 組，bucket `gemini-argon-wtu-weekly`，沒有 5h）：
/// 不扣 Gemini 額度，只認得出「是 Argon」的 bucket 才走這裡，不准蓋掉 Gemini 的 weekly。
fn is_argon(group_name: &str, bucket_name: &str) -> bool {
    group_name.to_ascii_lowercase().contains("argon") || bucket_name.to_ascii_lowercase().contains("argon")
}

/// `command.data.groups[].buckets[]`：Gemini 組（bucket id 精確為 `gemini-weekly`／`gemini-5h`）填 `agy` 的
/// five_hour／seven_day；Argon 組另存 `argon_weekly`（只有週窗）。Claude/GPT 組照舊略過。
fn parse_group_usage(v: &Value, now: &str) -> Option<Vec<(&'static str, Quota)>> {
    let groups = v.get("command")?.get("data")?.get("groups")?.as_array()?;
    let mut out: Vec<(&'static str, Quota)> = Vec::new();
    for group in groups {
        let group_name = text_field(group, &["name", "label", "title", "id", "model"]).unwrap_or_default();
        let Some(buckets) = group.get("buckets").and_then(Value::as_array) else { continue };
        for bucket in buckets {
            let bucket_name = text_field(bucket, &["id", "name", "label", "title", "model"]).unwrap_or_default();
            let argon = is_argon(group_name, bucket_name);
            let slot: Option<&str> = match (argon, bucket_name.to_ascii_lowercase().as_str()) {
                (true, _) => (window_is_weekly(bucket)).then_some("argon"),
                (false, "gemini-weekly" | "gemini-5h") => Some("gemini"),
                _ => None,
            };
            let Some(slot) = slot else { continue };
            let Some(window) = bucket_window(bucket) else { continue };
            let Some(left) = bucket.get("remaining_fraction").and_then(Value::as_f64).filter(|x| x.is_finite() && (0.0..=1.0).contains(x)) else {
                continue;
            };
            let w = Window { observed_at: None, used_pct: ((1.0 - left) * 100.0).clamp(0.0, 100.0), resets_at: bucket.get("reset_time").and_then(reset_time) };
            let row = if let Some(row) = out.iter_mut().find(|(k, _)| *k == "agy") {
                row
            } else {
                out.push(quota_row("agy", now));
                out.last_mut().expect("just pushed")
            };
            match (slot, window) {
                ("argon", _) => row.1.argon_weekly = Some(Box::new(w)),
                ("gemini", UsageWindow::FiveHour) => row.1.five_hour = Some(w),
                ("gemini", UsageWindow::SevenDay) => row.1.seven_day = Some(w),
                _ => {}
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
pub const PANE_BEGIN: &str = "AM_AGY_BEGIN";
pub const PANE_DONE: &str = "AM_AGY_DONE=";
/// pane 探測 workspace 的 label 前綴（claude 的是 `am-quota-claude`）：各自清各自的殘留，不互相把對方正在跑的收掉。
pub const PROBE_LABEL_PREFIX: &str = "am-quota-agy";

pub fn is_probe_label(l: &str) -> bool {
    l == PROBE_LABEL_PREFIX
}

/// 在 pane 的 shell 裡打的那一行：拋棄式 cwd、不讀 stdin、關自動更新，stdout 與 stderr 都留在畫面上（`Authentication required` 在那裡）。
pub fn pane_probe_command(exe: Option<&str>) -> String {
    pane_probe_command_with_env(exe, &BTreeMap::new())
}

/// 同 [`pane_probe_command`]，帶身份的 env 前綴（見 [`probe_script_with_env`]）。
pub fn pane_probe_command_with_env(exe: Option<&str>, env: &BTreeMap<String, String>) -> String {
    let exe = crate::hosts::sh_quote(exe.unwrap_or("agy"));
    let assign = env_assignments(env);
    format!(
        "d=$(mktemp -d 2>/dev/null) && cd \"$d\" && {{ printf '\\nAM_AGY_%s\\n' BEGIN; \
         AGY_CLI_DISABLE_AUTO_UPDATE=true {assign}{exe} -p /usage --output-format json </dev/null 2>&1; rc=$?; cd /; rm -rf \"$d\"; \
         printf '\\nAM_AGY_%s=%s\\n' DONE \"$rc\"; }} || printf '\\nAM_AGY_%s=%s\\n' DONE 1"
    )
}

/// 遠端在 pane 裡跑 `agy models` 的那一行（同 [`pane_probe_command_with_env`] 的包法與標記，只換子命令）：
/// 模型清單跟額度一樣要讀 Keychain，純 ssh 在 macOS 上拿到的是「Please sign in」。`env`＝身份的 `HOME`（已展開），預設帳號傳空的。
pub fn pane_models_command(exe: Option<&str>, env: &BTreeMap<String, String>) -> String {
    let exe = crate::hosts::sh_quote(exe.unwrap_or("agy"));
    let assign = env_assignments(env);
    format!(
        "d=$(mktemp -d 2>/dev/null) && cd \"$d\" && {{ printf '\\nAM_AGY_%s\\n' BEGIN; \
         AGY_CLI_DISABLE_AUTO_UPDATE=true {assign}{exe} models </dev/null 2>&1; rc=$?; cd /; rm -rf \"$d\"; \
         printf '\\nAM_AGY_%s=%s\\n' DONE \"$rc\"; }} || printf '\\nAM_AGY_%s=%s\\n' DONE 1"
    )
}

/// 沒登入時 `agy models` 印的字（2026-11-20 m4p 實測：`Error: Please sign in to view available models.`）；它會自己結束，不必等逾時。
pub fn models_sign_in_required(text: &str) -> bool {
    text.to_ascii_lowercase().contains("please sign in")
}

/// 畫面最後一次 `AM_AGY_BEGIN` 之後的部分（沒有就整個畫面）。
pub fn after_begin(screen: &str) -> &str {
    screen.rfind(PANE_BEGIN).map_or(screen, |i| &screen[i + PANE_BEGIN.len()..])
}

/// 指令跑完：`(結束碼, BEGIN 到 DONE 之間的輸出)`。從**尾端**找：重打過的指令會在捲動緩衝區留下兩輪，只有最後一輪是完整的。
pub fn pane_done(screen: &str) -> Option<(i32, String)> {
    let done = screen.rfind(PANE_DONE)?;
    let rc = screen[done + PANE_DONE.len()..].lines().next().and_then(|l| l.trim().parse::<i32>().ok()).unwrap_or(1);
    let head = &screen[..done];
    let body = head.rfind(PANE_BEGIN).map_or("", |i| &head[i + PANE_BEGIN.len()..]);
    Some((rc, body.trim().to_string()))
}

/// agy 沒有憑證可用時印的字（Keychain 讀不到、token 過期都一樣）；它接著會等人去開網址，不會自己結束。
pub fn auth_required(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("authentication required") || t.contains("please visit the url to log in")
}

/// 輸出裡的 JSON 那一行（`2>&1` 之後前後可能有警告）；`agy -p /usage --output-format json` 的 JSON 是單行。
pub fn json_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| l.starts_with('{') && l.ends_with('}'))
}

/// 探測失敗的分類：`AuthRequired` 是 agy 明說沒憑證（探測所在的環境讀得到 Keychain 時，這就是真的沒登入）；
/// 其餘是「已登入但額度這次拿不到」，帶一個固定的 `reason` 給網頁翻成人話。
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeFail {
    AuthRequired,
    Other { reason: &'static str, message: String },
}

impl ProbeFail {
    pub fn other(reason: &'static str, message: impl Into<String>) -> Self {
        Self::Other { reason, message: message.into() }
    }
}

pub fn tail(text: &str, n: usize) -> String {
    let t = text.trim();
    let skip = t.chars().count().saturating_sub(n);
    t.chars().skip(skip).collect()
}

/// 讀 agy 的 `/usage` 輸出（pane 的 BEGIN–DONE 之間，或本機的 stdout）：讀得到額度就回，否則說明為什麼沒有。
pub fn read_usage(host: &str, rc: Option<i32>, text: &str) -> Result<Vec<(&'static str, Quota)>, ProbeFail> {
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

/// 本機探測（`sh -c` 跑 [`probe_script`]）的結果。agy 沒憑證時會印 `Authentication required` 然後**卡住等人去開網址**，
/// 所以逾時也要看逾時前已經讀到的輸出：認得出就是 [`ProbeFail::AuthRequired`]，不是「逾時」（issue #870）。
/// stderr 也看（agy 的警告與登入提示不一定在 stdout）；正常結束時額度仍只從 stdout 讀。
pub fn read_local_run(host: &str, timed_out: bool, stdout: &str, stderr: &str, timeout_secs: u64) -> Result<Vec<(&'static str, Quota)>, ProbeFail> {
    if timed_out {
        if let Ok(found) = read_usage(host, None, stdout) {
            return Ok(found);
        }
        if auth_required(stdout) || auth_required(stderr) {
            return Err(ProbeFail::AuthRequired);
        }
        let seen = if stdout.trim().is_empty() { stderr } else { stdout };
        return Err(ProbeFail::other(
            "timeout",
            format!("`agy -p /usage` on {host} did not finish within {timeout_secs}s; output: {}", tail(seen, 200)),
        ));
    }
    match read_usage(host, None, stdout) {
        Err(ProbeFail::Other { .. }) if auth_required(stderr) => Err(ProbeFail::AuthRequired),
        other => other,
    }
}

/// 探測這一輪失敗的原因（每台主機一筆，成功或判定為未登入就清掉）：網頁在「已登入、但額度暫時拿不到」那一格講清楚，
/// 不再停在永遠的「背景查詢中」。只在記憶體：重啟就當沒失敗過，下一輪探測自然會再記。
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeError {
    pub reason: &'static str,
    pub message: String,
    pub at: String,
}

pub(crate) fn probe_errors() -> &'static std::sync::Mutex<std::collections::HashMap<String, ProbeError>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, ProbeError>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// 換掉那台主機記著的失敗（`None`＝清掉）；回傳內容有沒有變（沒變就不用再推 `host_changed`）。
pub fn set_probe_error(host: &str, err: Option<ProbeError>) -> bool {
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

/// agy 的登入憑證（Google OAuth token）：Linux（agm-host 實測）是這個檔（600）；**macOS 沒有檔，放 login Keychain**
/// （service `gemini`、account `antigravity`，agy log 寫 `authenticated via keyring`；2026-10-06 在 mac_builder 實測）。`agy` 沒有 `logout` 子命令，TUI 的 `/logout` 做的也是清憑證。
pub const TOKEN_FILE: &str = ".gemini/antigravity-cli/antigravity-oauth-token";

/// 遠端同一件事：只刪憑證（檔案、macOS 的 Keychain 項目）、不跑 agy。`AM_REMOVED`＝刪掉了，`AM_ABSENT`＝本來就沒有，`AM_FAILED`＝想刪沒刪成。
/// 測試行程共用一個假 HOME：碰憑證檔的測試（這裡與 `api::agy_logout_route_tests`）一個一個來。
#[cfg(any(test, feature = "test-hooks"))]
pub async fn token_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static L: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    L.lock().await
}

#[derive(Debug)]
pub enum LogoutError {
    UnknownHost,
    /// 排隊等探測鎖或刪憑證期間，這台主機換了主機或重連：什麼都沒刪，可重試。
    Superseded,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agy_1_5_14_fixture(credits: Option<serde_json::Value>) -> String {
        let mut data = json!({
            "groups": [
                {
                    "name": "Gemini Models",
                    "buckets": [
                        {
                            "id": "gemini-weekly",
                            "name": "Weekly Limit Remaining",
                            "window": "weekly",
                            "remaining_fraction": 0.8,
                            "reset_time": "2026-11-28T09:04:47Z"
                        },
                        {
                            "id": "gemini-5h",
                            "name": "Five Hour Limit Remaining",
                            "window": "5h",
                            "remaining_fraction": 0.6,
                            "reset_time": "2026-11-21T19:04:47Z"
                        }
                    ]
                },
                {
                    "name": "Claude and GPT models",
                    "buckets": [
                        {
                            "id": "3p-weekly",
                            "name": "Weekly Limit Remaining",
                            "window": "weekly",
                            "remaining_fraction": 1.0,
                            "reset_time": "2026-11-28T16:30:57Z"
                        },
                        {
                            "id": "3p-5h",
                            "name": "Five Hour Limit Remaining",
                            "window": "5h",
                            "remaining_fraction": 1.0,
                            "reset_time": "2026-11-21T21:30:57Z"
                        }
                    ]
                }
            ]
        });
        let mut root = json!({
            "conversation_id": "",
            "status": "SUCCESS",
            "command": {
                "name": "usage",
                "data": null
            }
        });
        if let Some(c) = credits {
            data.as_object_mut().unwrap().insert("credits".into(), c.clone());
            root.as_object_mut().unwrap().insert("credits".into(), c);
        }
        root.get_mut("command").unwrap().as_object_mut().unwrap().insert("data".into(), data);
        root.to_string()
    }

    #[test]
    fn agy_1_5_14_zero_credits_with_positive_remaining_fraction() {
        let raw = agy_1_5_14_fixture(Some(json!(0)));
        let parsed = parse_usage(&raw).expect("parsed usage");
        assert_eq!(parsed.len(), 1);
        let (key, quota) = &parsed[0];
        assert_eq!(*key, "agy");
        assert!(quota.limit_hit.is_none());
        let five = quota.five_hour.as_ref().expect("five hour window");
        assert!((five.used_pct - 40.0).abs() < 1e-6);
        assert_eq!(five.resets_at.as_deref(), Some("2026-11-21T19:04:47.000Z"));
        let seven = quota.seven_day.as_ref().expect("seven day window");
        assert!((seven.used_pct - 20.0).abs() < 1e-6);
        assert_eq!(seven.resets_at.as_deref(), Some("2026-11-28T09:04:47.000Z"));

        let res = read_usage("local", Some(0), &raw).expect("read usage ok");
        assert_eq!(res.len(), 1);
        assert!(res[0].1.limit_hit.is_none());
        assert!((res[0].1.five_hour.as_ref().unwrap().used_pct - 40.0).abs() < 1e-6);
        assert!((res[0].1.seven_day.as_ref().unwrap().used_pct - 20.0).abs() < 1e-6);
    }

    #[test]
    fn agy_1_5_14_positive_credits() {
        let raw = agy_1_5_14_fixture(Some(json!(100)));
        let parsed = parse_usage(&raw).expect("parsed usage");
        assert_eq!(parsed.len(), 1);
        let (key, quota) = &parsed[0];
        assert_eq!(*key, "agy");
        assert!(quota.limit_hit.is_none());
        let five = quota.five_hour.as_ref().expect("five hour window");
        assert!((five.used_pct - 40.0).abs() < 1e-6);
        assert_eq!(five.resets_at.as_deref(), Some("2026-11-21T19:04:47.000Z"));
        let seven = quota.seven_day.as_ref().expect("seven day window");
        assert!((seven.used_pct - 20.0).abs() < 1e-6);
        assert_eq!(seven.resets_at.as_deref(), Some("2026-11-28T09:04:47.000Z"));

        let res = read_usage("local", Some(0), &raw).expect("read usage ok");
        assert_eq!(res.len(), 1);
        assert!(res[0].1.limit_hit.is_none());
        assert!((res[0].1.five_hour.as_ref().unwrap().used_pct - 40.0).abs() < 1e-6);
        assert!((res[0].1.seven_day.as_ref().unwrap().used_pct - 20.0).abs() < 1e-6);
    }

    #[test]
    fn agy_1_5_14_no_credits_field() {
        let raw = agy_1_5_14_fixture(None);
        let parsed = parse_usage(&raw).expect("parsed usage");
        assert_eq!(parsed.len(), 1);
        let (key, quota) = &parsed[0];
        assert_eq!(*key, "agy");
        assert!(quota.limit_hit.is_none());
        let five = quota.five_hour.as_ref().expect("five hour window");
        assert!((five.used_pct - 40.0).abs() < 1e-6);
        assert_eq!(five.resets_at.as_deref(), Some("2026-11-21T19:04:47.000Z"));
        let seven = quota.seven_day.as_ref().expect("seven day window");
        assert!((seven.used_pct - 20.0).abs() < 1e-6);
        assert_eq!(seven.resets_at.as_deref(), Some("2026-11-28T09:04:47.000Z"));

        let res = read_usage("local", Some(0), &raw).expect("read usage ok");
        assert_eq!(res.len(), 1);
        assert!(res[0].1.limit_hit.is_none());
        assert!((res[0].1.five_hour.as_ref().unwrap().used_pct - 40.0).abs() < 1e-6);
        assert!((res[0].1.seven_day.as_ref().unwrap().used_pct - 20.0).abs() < 1e-6);
    }
}
