//! Rate-limit quota per host + kind (`GET /api/quota`, WS `quota_updated`); sources: codex
//! app-server, claude statusLine + [`crate::quota_claude`] probe, grok [`crate::quota_grok`].
//! Keys are host-scoped (SPEC §14): bare on local, `<host>/…` remote — a remote bot's statusLine
//! must never land on the local row.

use crate::config::LOCAL_HOST;
use crate::state::App;
use anyhow::Result;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const CODEX_POLL: Duration = Duration::from_secs(300);
/// 讀 pane 狀態列只是一個 `pane.read`，比 app-server RPC 便宜得多：這個頻率跟上 CLI 自己的數字
/// （2026-09-15 使用者：pane 寫 5h 93% left、header 還是 100）。
pub const CODEX_PANE_POLL: Duration = Duration::from_secs(60);

/// Requirement: the low/critical decision is the daemon's; the UI only reads [`Window::low`].
pub const LOW_REMAINING_PCT: f64 = 30.0;

pub const CRITICAL_REMAINING_PCT: f64 = 5.0;

/// Per-host probe lock: `?refresh=1` and pollers must not fight over a pane, and a slow ssh
/// host must not hold up the local one.
pub async fn probe_lock(host: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::OnceLock::new();
    let map = LOCKS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
    let lock = map.lock().await.entry(host.to_string()).or_default().clone();
    lock.lock_owned().await
}

pub fn quota_key(host: &str, base: &str) -> String {
    if host == LOCAL_HOST {
        base.to_string()
    } else {
        format!("{host}/{base}")
    }
}

/// 拼 key 的最底層；要不要收斂到裸 kind 由 [`quota_base_for_host`] 決定（寫入端與查詢端共用）。
pub fn quota_base(kind: &str, identity: Option<&str>) -> String {
    match identity.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => format!("{kind}:{id}"),
        None => kind.to_string(),
    }
}

/// 這個身分**對這個 kind** 是不是就是預設帳號？
///
/// 看的是**該 kind 的 home 變數**（codex＝`CODEX_HOME`、claude＝`CLAUDE_CONFIG_DIR`、grok＝`GROK_HOME`，
/// 見 `pane_identity::config_dir_var`），不是「env 空不空」。
///
/// 2026-09-14 第二次冒出兩個 codex：中午所有 cc0 bot 改用 cc1，而 cc1 的 alias 只設
/// `CLAUDE_CONFIG_DIR`、沒有 `CODEX_HOME`——對 codex 來說 cc1 仍是同一個帳號，但 708a81a 只把
/// 「env 整個空的 cc0」當預設，於是 codex 又寫出一把 `codex:cc1`。env 裡沒有該 kind 的 home
/// 變數，那個 CLI 就會用它自己的預設目錄，也就是預設帳號。
pub fn identity_shares_default(kind: &str, env: &std::collections::BTreeMap<String, String>) -> bool {
    match crate::pane_identity::config_dir_var(kind) {
        Some(var) => !env.contains_key(var),
        // 不認得的 kind 沒有「home 變數」可看，只好退回「env 整個是空的才算」。
        None => env.is_empty(),
    }
}

/// 同 [`quota_base`]，但身分對這個 kind 就是預設帳號時（見 [`identity_shares_default`]）寫回裸 kind。
pub fn quota_base_default_aware(kind: &str, identity: Option<&str>, shares_default: bool) -> String {
    match identity.map(str::trim).filter(|s| !s.is_empty()) {
        Some(_) if shares_default => kind.to_string(),
        other => quota_base(kind, other),
    }
}

/// [`quota_base_default_aware`]，身分的 env 從那台主機的身分表查（§16.2）。**寫入端與查詢端都走這支**：
/// codex statusline、撞限橫幅、claude statusLine、`limit_hit_for_bot`／`next_reset_for_bot`、
/// supervisor 的額度判讀、mission 挑身分。查不到那個身分就當它有自己的帳號——寧可多開一格，
/// 也不要把兩個帳號的數字疊在一起。
pub async fn quota_base_for_host(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>) -> String {
    let Some(idn) = identity.map(str::trim).filter(|s| !s.is_empty()) else { return kind.to_string() };
    let found = crate::tools::identity_for_host(app, host, idn).await;
    // 身分有 kind（`identity_kind`）：別的 kind 的身分（codex bot 身上的 claude `cc1`）根本不是這個 CLI 的
    // 帳號代號，一律寫裸 kind——codex 不該有任何 `codex:ccN`（2026-09-14 使用者指正）。
    // 同 kind 才看 home 變數那條保險；查不到那個身分（主機的身分還沒偵測完）維持分開，免得把兩個
    // claude 帳號的數字疊進同一格。
    let shares = match &found {
        Some(i) if i.kind != kind => true,
        Some(i) => identity_shares_default(kind, &i.env),
        None => false,
    };
    quota_base_default_aware(kind, Some(idn), shares)
}

/// 查詢端：這顆 bot 的讀數在哪幾把 key。先查它自己那一把（收斂規則同寫入端），**只有**收斂到裸 kind
/// 的身分才會落在裸 key——有自己 home 的身分（cc2 帶 `CODEX_HOME`）不借預設帳號的數字。
async fn keys_for_bot(app: &Arc<App>, host: &str, bot: &crate::db::Bot) -> Vec<String> {
    let base = quota_base_for_host(app, host, &bot.kind, bot.identity.as_deref()).await;
    vec![quota_key(host, &base)]
}

pub async fn limit_hit_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Option<LimitHit> {
    let host = crate::db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
    let keys = keys_for_bot(app, &host, bot).await;
    let q = app.quotas.lock().await;
    for k in keys {
        if let Some(hit) = q.get(&k).and_then(|x| x.limit_hit.clone()) {
            if !limit_hit_expired(Some(&hit)) {
                return Some(hit);
            }
        }
    }
    None
}

/// 只回未來的重置時間；CLI 橫幅時間會舊，supervisor 要兩邊都看（2026-09-13：橫幅 22:15、app-server 22:20）。
pub async fn next_reset_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Option<String> {
    let host = crate::db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
    let keys = keys_for_bot(app, &host, bot).await;
    let now = chrono::Utc::now();
    let future = |t: &Option<String>| {
        t.as_deref()
            .and_then(|x| chrono::DateTime::parse_from_rfc3339(x).ok())
            .map(|x| x.with_timezone(&chrono::Utc))
            .filter(|x| *x > now)
    };
    let q = app.quotas.lock().await;
    for k in keys {
        let Some(entry) = q.get(&k) else { continue };
        let candidates = [
            entry.five_hour.as_ref().and_then(|w| future(&w.resets_at)),
            entry.seven_day.as_ref().and_then(|w| future(&w.resets_at)),
        ];
        if let Some(t) = candidates.into_iter().flatten().min() {
            return Some(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        }
    }
    None
}

/// Unknown host prefixes fall back to `local`.
pub fn host_of_key<'a>(key: &'a str, hosts: &[String]) -> (&'a str, &'a str) {
    match key.split_once('/') {
        Some((h, base)) if hosts.iter().any(|n| n == h) => (h, base),
        _ => (LOCAL_HOST, key),
    }
}

pub async fn pollable_hosts(app: &Arc<App>) -> Vec<String> {
    app.hosts
        .list()
        .await
        .into_iter()
        .filter(|c| c.is_local() || c.is_connected())
        .map(|c| c.name.clone())
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub used_pct: f64,
    pub resets_at: Option<String>,
}

impl Window {
    fn remaining_pct(&self) -> f64 {
        (100.0 - self.used_pct).max(0.0)
    }

    pub fn low(&self) -> bool {
        self.remaining_pct() < LOW_REMAINING_PCT
    }

    pub fn critical(&self) -> bool {
        self.remaining_pct() < CRITICAL_REMAINING_PCT
    }
}

/// Manual impl so `low` / `critical` go over the wire as computed fields.
impl Serialize for Window {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Window", 4)?;
        st.serialize_field("used_pct", &self.used_pct)?;
        st.serialize_field("resets_at", &self.resets_at)?;
        st.serialize_field("low", &self.low())?;
        st.serialize_field("critical", &self.critical())?;
        st.end()
    }
}

/// Codex 的額度重置券（`rateLimitResetCredits`）：額度用完時使用者唯一能做的事，所以要看得到
/// （2026-09-10 使用者）。daemon 只讀不用。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResetCredits {
    pub available: i64,
    pub title: Option<String>,
    pub expires_at: Option<String>,
}

/// CLI 印的上限橫幅。credits 用完時 5h／7d 速率窗可以是滿的（2026-09-12 使用者：量表全滿卻一直
/// hit limit），所以單獨記且**黏住**：[`set`] 沿用舊值，直到 `until` 過了或下一回合跑成功。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LimitHit {
    pub message: String,
    /// 沒寫時間就 `None`，只能等下一次成功的回合清掉。
    pub until: Option<String>,
    pub at: String,
    /// 橫幅說的是哪一桶（`five_hour`／`seven_day`／`fable`）。解析時就知道了，**不要讓下游再猜一次**：
    /// `mission::pick` 以前是用「當下哪個桶見底」倒推，撞 Fable 上限而 5h 剛好也快滿時會把
    /// Fable 的下週重置時間當成「5 小時窗什麼時候回來」（review 2026-09-16）。舊資料是 `None`。
    #[serde(default)]
    pub bucket: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Quota {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Max 方案才有的 Fable 週額度；`None` 時 UI 完全不畫。
    pub fable: Option<Window>,
    pub reset_credits: Option<ResetCredits>,
    pub limit_hit: Option<LimitHit>,
    pub plan: Option<String>,
    pub updated_at: String,
    pub source: String,
    pub account: Option<String>,
    /// Parsers build `local`; [`set`] stamps the real host so no caller can forget it.
    pub host: String,
}

fn unix_to_rfc3339(v: Option<&Value>) -> Option<String> {
    let secs = match v? {
        Value::Number(n) => n.as_f64()? as i64,
        Value::String(s) => {
            if let Ok(n) = s.parse::<i64>() {
                n
            } else {
                // Already a timestamp string? Keep it.
                return Some(s.clone());
            }
        }
        _ => return None,
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// `credits[]` 可能含已用／過期的，標題與到期只取第一張 `available`。
fn reset_credits(result: &Value) -> Option<ResetCredits> {
    let rc = result.get("rateLimitResetCredits")?;
    let available = rc.get("availableCount").and_then(|x| x.as_i64()).unwrap_or(0);
    let first = rc
        .get("credits")
        .and_then(|x| x.as_array())
        .and_then(|a| a.iter().find(|c| c.get("status").and_then(|s| s.as_str()) == Some("available")));
    Some(ResetCredits {
        available,
        title: first.and_then(|c| c.get("title")).and_then(|x| x.as_str()).map(String::from),
        expires_at: first.and_then(|c| unix_to_rfc3339(c.get("expiresAt"))),
    })
}

/// Windows matched by `windowDurationMins`, falling back to primary/secondary order.
pub fn quota_from_codex(result: &Value) -> Option<Quota> {
    let rl = result.get("rateLimits")?;
    let window = |v: Option<&Value>| -> Option<Window> {
        let v = v?;
        Some(Window { used_pct: v.get("usedPercent")?.as_f64()?, resets_at: unix_to_rfc3339(v.get("resetsAt")) })
    };
    let mins = |v: Option<&Value>| v.and_then(|x| x.get("windowDurationMins")).and_then(|m| m.as_i64());
    let (p, s) = (rl.get("primary"), rl.get("secondary"));
    let mut five = None;
    let mut seven = None;
    for w in [p, s] {
        match mins(w) {
            Some(300) => five = window(w),
            Some(10080) => seven = window(w),
            _ => {}
        }
    }
    if five.is_none() && seven.is_none() {
        five = window(p);
        seven = window(s);
    }
    Some(Quota {
        five_hour: five,
        seven_day: seven,
        fable: None,
        reset_credits: reset_credits(result),
        limit_hit: None,
        plan: rl.get("planType").and_then(|x| x.as_str()).map(String::from),
        updated_at: crate::db::now(),
        source: "codex-app-server".into(),
        account: None,
        host: LOCAL_HOST.into(),
    })
}

pub fn quota_from_statusline(payload: &Value, account: Option<&str>) -> Option<Quota> {
    let rl = payload.get("rate_limits")?;
    let window = |v: Option<&Value>| -> Option<Window> {
        let v = v?;
        Some(Window {
            used_pct: v.get("used_percentage")?.as_f64()?,
            resets_at: unix_to_rfc3339(v.get("resets_at")),
        })
    };
    let five = window(rl.get("five_hour"));
    let seven = window(rl.get("seven_day"));
    // 實測（2026-09-07）statusLine 沒有 fable 桶，這裡只是有就收。
    let fable = window(rl.get("fable")).or_else(|| window(rl.get("seven_day_fable")));
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
        source: "statusline".into(),
        account: account.map(String::from),
        host: LOCAL_HOST.into(),
    })
}

/// `base` is the host-less key; stored key and `quota.host` derive from `host`.
pub async fn set(app: &Arc<App>, host: &str, base: &str, mut q: Quota) {
    q.host = host.to_string();
    let key = quota_key(host, base);
    let mut quotas = app.quotas.lock().await;
    // A window the new reading lacks keeps the previous value: statusLine has no Fable and
    // otherwise wipes the probe's F bar every few seconds.
    if q.fable.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.fable = prev.fable.clone();
        }
    }
    // 同理 5h／7d：狀態列被截斷只讀到 5h 時不可洗掉 7d（2026-09-13 實機、使用者回報）。
    if q.five_hour.is_none() || q.seven_day.is_none() {
        if let Some(prev) = quotas.get(&key) {
            if q.five_hour.is_none() {
                q.five_hour = prev.five_hour.clone();
            }
            if q.seven_day.is_none() {
                q.seven_day = prev.seven_day.clone();
            }
        }
    }
    // 重置券只有 app-server 讀得到，別的來源不該抹掉。
    if q.reset_credits.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.reset_credits = prev.reset_credits.clone();
        }
    }
    // 撞上限只有 CLI 橫幅看得到（§12.4），不沿用會被 app-server 輪詢洗回滿格（2026-09-12 使用者）。
    if q.limit_hit.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.limit_hit = prev.limit_hit.clone();
        }
    }
    if limit_hit_expired(q.limit_hit.as_ref()) {
        q.limit_hit = None;
    }
    // CLI 狀態列沒有重置時間，沿用上一份，否則量表的「N 小時後重置」會消失。
    if let Some(prev) = quotas.get(&key) {
        for (now, old) in [(&mut q.five_hour, &prev.five_hour), (&mut q.seven_day, &prev.seven_day), (&mut q.fable, &prev.fable)] {
            if let (Some(w), Some(p)) = (now.as_mut(), old.as_ref()) {
                if w.resets_at.is_none() {
                    w.resets_at = p.resets_at.clone();
                }
            }
        }
    }

    quotas.insert(key.clone(), q.clone());
    drop(quotas);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": q})).await;
}

/// 沒寫時間的一律**不**過期，只能靠 [`clear_limit_hit`]。
pub fn limit_hit_expired(hit: Option<&LimitHit>) -> bool {
    let Some(until) = hit.and_then(|h| h.until.as_deref()) else { return false };
    match chrono::DateTime::parse_from_rfc3339(until) {
        Ok(t) => chrono::Utc::now() >= t.with_timezone(&chrono::Utc),
        Err(_) => false,
    }
}

/// 一回合真的跑完就拿掉「撞上限」，不必等它自己寫的時間。
pub async fn clear_limit_hit(app: &Arc<App>, host: &str, base: &str) {
    let key = quota_key(host, base);
    let mut quotas = app.quotas.lock().await;
    let Some(q) = quotas.get_mut(&key) else { return };
    if q.limit_hit.is_none() {
        return;
    }
    q.limit_hit = None;
    let out = q.clone();
    drop(quotas);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": out})).await;
}

/// Base kinds always present per host (empty bars before first report); orphan-host keys dropped.
pub async fn snapshot(app: &Arc<App>) -> Value {
    let hosts = app.hosts.names().await;
    let q = app.quotas.lock().await;
    let mut m = serde_json::Map::new();
    for h in &hosts {
        for k in crate::config::KINDS {
            let key = quota_key(h, k);
            m.insert(key.clone(), q.get(&key).map(|x| json!(x)).unwrap_or(Value::Null));
        }
    }
    for (k, v) in q.iter() {
        // Otherwise read as a local key downstream.
        let orphan = k.contains('/') && host_of_key(k, &hosts).0 == LOCAL_HOST;
        if !orphan {
            m.insert(k.clone(), json!(v));
        }
    }
    json!({"kinds": Value::Object(m)})
}

/// codex 狀態列的剩餘量；沒有 `resets_at`，交給 [`set`] 沿用。
pub fn quota_from_codex_status(q: &crate::codex_live::CodexStatusQuota, account: Option<&str>) -> Option<Quota> {
    let win = |left: Option<f64>| left.map(|l| Window { used_pct: (100.0 - l).clamp(0.0, 100.0), resets_at: None });
    let (five, seven) = (win(q.five_hour_left), win(q.weekly_left));
    if five.is_none() && seven.is_none() {
        return None;
    }
    Some(Quota {
        five_hour: five,
        seven_day: seven,
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: crate::db::now(),
        source: "codex-statusline".into(),
        account: account.map(String::from),
        host: LOCAL_HOST.into(),
    })
}

/// app-server 讀數會落後 CLI 一整輪（2026-09-13 使用者截圖：量表 5h 100、pane 90% left），
/// CLI 狀態列才是它當下擋你的依據。與 app-server 共用同一格，後到覆蓋先到。
/// 這個 pane 的狀態列讀數這一輪有沒有變過。記在行程裡就夠：daemon 重啟後第一輪本來就該重讀一次。
/// 數字一樣就是同一張畫面（真的沒變時，跳過也只是少寫一次一模一樣的值——但 `updated_at` 不會被
/// 刷新成「剛剛讀到的」，那正是重點）。
async fn status_line_changed(host: &str, pane_id: &str, line: &str) -> bool {
    static SEEN: std::sync::OnceLock<tokio::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    let mut map = SEEN.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new())).lock().await;
    let key = format!("{host}:{pane_id}");
    match map.get(&key) {
        Some(prev) if prev == line => false,
        _ => {
            map.insert(key, line.to_string());
            true
        }
    }
}

pub async fn refresh_codex_from_panes(app: &Arc<App>, host: &str) -> usize {
    let rows: Vec<(String, Option<String>)> = match sqlx::query_as(
        // 最近有動靜的 pane 排前面：它的狀態列最新。閒著的 pane 也會刷新，但剛跑完回合的那顆最準。
        "SELECT r.pane_id, b.identity FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE p.host = ? AND b.kind = 'codex' AND r.state = 'running' AND r.pane_id IS NOT NULL
            AND b.deleted_at IS NULL
          ORDER BY COALESCE((SELECT MAX(t.created_at) FROM turns t WHERE t.run_id = r.id), r.started_at) DESC",
    )
    .bind(host)
    .fetch_all(&app.db)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(host, error = ?e, "codex statusline quota: query failed");
            return 0;
        }
    };
    let mut wrote = 0;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (pane_id, identity) in rows {
        let base = quota_base_for_host(app, host, "codex", identity.as_deref()).await;
        // 同一個身分讀到一次就夠——但要「讀到」才算：那顆 pane 正在壓縮對話、捲動中讀不到狀態列時，
        // 換同帳號的下一顆，而不是整個帳號這輪都停在 app-server 落後的數字（2026-09-15）。
        if seen.contains(&base) {
            continue;
        }
        let Some(client) = app.herdr_for(host).await else { continue };
        let Ok(read) = client.pane_read(&pane_id, "visible", 60).await else { continue };
        let Some(parsed) = crate::codex_live::parse_status_quota(&read.text) else { continue };
        // pane 讀的是**畫面**，不是感測器：一顆閒著三小時的 pane，狀態列上的數字就是三小時前那一回合的，
        // 而這裡每 60 秒把同一張沒變過的畫面重新解析一次、蓋上 now() 當新讀數——app-server 剛寫進去的
        // 「視窗重置了」會被它蓋回見底，量表在滿與見底之間跳（review 2026-09-16）。
        // 同一段字就是同一張畫面：沒變就不是新讀數。（`limit_banner` 用同樣的想法分辨重播與新撞限。）
        let reading = format!("{:?}/{:?}", parsed.five_hour_left, parsed.weekly_left);
        if !status_line_changed(host, &pane_id, &reading).await {
            seen.insert(base);
            continue;
        }
        let Some(q) = quota_from_codex_status(&parsed, identity.as_deref()) else { continue };
        set(app, host, &base, q).await;
        seen.insert(base);
        wrote += 1;
    }
    wrote
}

/// `Ok(false)` = codex not installed there (quota stays null).
pub async fn refresh_codex(app: &Arc<App>, host: &str) -> Result<bool> {
    let r = crate::models::codex_rpc(app, host, "account/rateLimits/read", json!({})).await;
    let r = match r {
        Ok(v) => v,
        Err(e) if e.to_string().contains("is not installed") => return Ok(false),
        Err(e) => return Err(e),
    };
    match quota_from_codex(&r) {
        Some(q) => {
            set(app, host, "codex", q).await;
            Ok(true)
        }
        None => anyhow::bail!("unexpected rateLimits shape: {r}"),
    }
}

pub fn spawn_codex_poller(app: Arc<App>) {
    tokio::spawn(async move {
        let mut last_server: Option<std::time::Instant> = None;
        loop {
            // app-server 每 CODEX_POLL 問一次；狀態列每 CODEX_PANE_POLL 讀一次。同一輪兩個都做時先問 app-server，
            // 狀態列後到蓋前（CLI 狀態列較即時且分得出身分）。
            let ask_server = last_server.map_or(true, |t| t.elapsed() >= CODEX_POLL);
            if ask_server {
                last_server = Some(std::time::Instant::now());
            }
            for host in pollable_hosts(&app).await {
                if ask_server {
                    match refresh_codex(&app, &host).await {
                        Ok(true) => {}
                        Ok(false) => tracing::info!(host = %host, "codex not installed; codex quota stays null"),
                        Err(e) => tracing::warn!(host = %host, error = %e, "codex quota refresh failed"),
                    }
                }
                let n = refresh_codex_from_panes(&app, &host).await;
                if n > 0 {
                    tracing::debug!(host = %host, panes = n, "codex quota read off the status line");
                }
            }
            tokio::time::sleep(CODEX_PANE_POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    /// 狀態列是剩餘、存的是已用；不可洗掉 `resets_at`（2026-09-13 使用者：量表停在舊數字）。
    #[tokio::test]
    async fn the_status_line_updates_the_numbers_without_losing_the_reset_time() {
        let app = crate::testing::env().await.app.clone();
        let from_server = Quota {
            five_hour: Some(Window { used_pct: 0.0, resets_at: Some("2026-09-13T12:00:00Z".into()) }),
            seven_day: Some(Window { used_pct: 50.0, resets_at: Some("2026-09-18T00:00:00Z".into()) }),
            fable: None,
            reset_credits: Some(ResetCredits { available: 1, title: None, expires_at: None }),
            limit_hit: None,
            plan: Some("plus".into()),
            updated_at: crate::db::now(),
            source: "codex-app-server".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "codex", from_server).await;

        let seen = crate::codex_live::parse_status_quota(
            "gpt-6-astra high · /tmp · Context 28% used · 5h 90% left · weekly 48% …",
        )
        .unwrap();
        set(&app, LOCAL_HOST, "codex", quota_from_codex_status(&seen, None).unwrap()).await;

        let q = app.quotas.lock().await.get("codex").cloned().unwrap();
        assert_eq!(q.source, "codex-statusline");
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 10.0, "90% left = 10% used");
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 52.0);
        assert_eq!(q.five_hour.as_ref().unwrap().resets_at.as_deref(), Some("2026-09-13T12:00:00Z"), "重置時間沿用");
        assert_eq!(q.seven_day.as_ref().unwrap().resets_at.as_deref(), Some("2026-09-18T00:00:00Z"));
        assert!(q.reset_credits.is_some(), "重置券只有 app-server 讀得到，不能被洗掉");
    }

    /// 同帳號有好幾顆 codex pane：先讀最近有動靜的那顆；它讀不到狀態列（壓縮對話中）就換下一顆，
    /// 不是整個帳號停在舊數字（2026-09-15 使用者：pane 寫 93% left、header 還是 100）。
    #[tokio::test]
    async fn the_freshest_readable_codex_pane_sets_the_numbers() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let codex = |name: &'static str| {
            let app = app.clone();
            let pid = env.project_id.clone();
            async move {
                let id = crate::db::ulid();
                sqlx::query(
                    "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
                     VALUES (?,?,?,'codex','[]',0,1,'tok','user',?)",
                )
                .bind(&id)
                .bind(&pid)
                .bind(name)
                .bind(crate::db::now())
                .execute(&app.db)
                .await
                .unwrap();
                let run = crate::testing::fake_run(&app, &id).await;
                (id, run)
            }
        };
        let (_old_bot, old_run) = codex("idle-old").await;
        let (fresh_bot, fresh_run) = codex("busy-fresh").await;
        let turn = |run: String, bot: String, at: &'static str| {
            let app = app.clone();
            async move {
                let conv = crate::db::conversation_id(&app.db, &bot).await.unwrap();
                sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, created_at) VALUES (?,?,?,'web','completed',?)")
                    .bind(crate::db::ulid())
                    .bind(conv)
                    .bind(run)
                    .bind(at)
                    .execute(&app.db)
                    .await
                    .unwrap();
            }
        };
        turn(old_run.clone(), _old_bot.clone(), "2026-09-15T03:00:00Z").await;
        turn(fresh_run.clone(), fresh_bot.clone(), "2026-09-15T09:00:00Z").await;
        let pane = |run: &str| futures::executor::block_on(crate::db::run(&app.db, run)).unwrap().unwrap().pane_id.unwrap();
        let (old_pane, fresh_pane) = (pane(&old_run), pane(&fresh_run));
        let line = |five: u32| format!("\n› Ask Codex\n  gpt-6-astra low · /tmp · Context 20% used · 5h {five}% left · weekly 65% left\n");

        // 兩顆都讀得到：最近有動靜的那顆說了算。
        env.herdr.screens.lock().unwrap().insert(old_pane.clone(), line(100));
        env.herdr.screens.lock().unwrap().insert(fresh_pane.clone(), line(93));
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 1);
        let used = |app: Arc<App>| async move { app.quotas.lock().await.get("codex").unwrap().five_hour.clone().unwrap().used_pct };
        assert_eq!(used(app.clone()).await, 7.0, "93% left 那顆較新");

        // 最新那顆正在壓縮、讀不到狀態列：換同帳號的下一顆，不是這輪整個跳過。
        env.herdr.screens.lock().unwrap().insert(fresh_pane.clone(), "• Compacting context (1m 17s • esc to interrupt)\n".into());
        env.herdr.screens.lock().unwrap().insert(old_pane.clone(), line(88));
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 1);
        assert_eq!(used(app.clone()).await, 12.0);

        // 同一張沒變過的畫面不是新讀數：再讀一次不該把 `updated_at` 刷新成「剛剛」，
        // 否則閒著的 pane 每 60 秒就把 app-server 剛寫進去的「視窗重置了」蓋回見底（review 2026-09-16）。
        let before = app.quotas.lock().await.get("codex").unwrap().updated_at.clone();
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 0, "畫面沒變就不算一次讀數");
        assert_eq!(app.quotas.lock().await.get("codex").unwrap().updated_at, before);

        // 畫面真的變了才是新讀數。
        env.herdr.screens.lock().unwrap().insert(old_pane, line(70));
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 1);
        assert_eq!(used(app.clone()).await, 30.0);
    }

    /// 截斷只讀到 5h 時不可洗掉 7d（2026-09-13 使用者：header 的 codex 只剩一條）。
    #[tokio::test]
    async fn a_partial_reading_keeps_the_window_it_could_not_see() {
        let app = crate::testing::env().await.app.clone();
        let mut full = codex_q("codex-app-server", None);
        full.five_hour = Some(Window { used_pct: 30.0, resets_at: Some("2026-09-13T19:22:00.000Z".into()) });
        full.seven_day = Some(Window { used_pct: 76.0, resets_at: Some("2026-09-18T00:00:00.000Z".into()) });
        set(&app, LOCAL_HOST, "codex", full).await;

        let mut partial = codex_q("codex-statusline", None);
        partial.five_hour = Some(Window { used_pct: 64.0, resets_at: None });
        partial.seven_day = None;
        set(&app, LOCAL_HOST, "codex", partial).await;

        let q = app.quotas.lock().await.get("codex").cloned().unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 64.0, "看得到的那條要更新");
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 76.0, "看不到的那條沿用，不是清空");
        assert_eq!(q.seven_day.as_ref().unwrap().resets_at.as_deref(), Some("2026-09-18T00:00:00.000Z"));
    }

    fn env(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// 2026-09-14 第二次冒出兩個 codex（AGM 交辦）：cc1 只設 `CLAUDE_CONFIG_DIR`，對 codex 它仍是預設
    /// 帳號；要看的是**該 kind 的 home 變數**，不是 env 空不空。
    #[test]
    fn an_identity_shares_the_default_account_unless_it_sets_that_kinds_home() {
        let cc0 = env(&[]);
        let cc1 = env(&[("CLAUDE_CONFIG_DIR", "$HOME/.claude-ccompany")]);
        let cc2 = env(&[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc2"), ("CODEX_HOME", "$HOME/.codex-cc2")]);
        assert!(identity_shares_default("codex", &cc0));
        assert!(identity_shares_default("codex", &cc1), "cc1 沒有 CODEX_HOME：對 codex 就是預設帳號");
        assert!(!identity_shares_default("codex", &cc2), "cc2 有自己的 CODEX_HOME 才分開");
        assert!(identity_shares_default("claude", &cc0));
        assert!(!identity_shares_default("claude", &cc1), "對 claude，cc1 有自己的 config dir");
        assert!(identity_shares_default("grok", &cc2), "沒有 GROK_HOME 的身分對 grok 是預設帳號");
    }

    /// 寫入端與查詢端走同一支：帶 claude 身分（cc1）的 codex bot 寫裸 `codex`、`limit_hit_for_bot` 也從裸
    /// `codex` 讀到；只有 codex 自己的身分（cx2）才寫 `codex:cx2`，而且**不借**裸 `codex` 的數字。
    #[tokio::test]
    async fn codex_bots_on_cc1_share_the_bare_key_and_cc2_keeps_its_own() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let ident = |name: &str, pairs: &[(&str, &str)]| crate::config::IdentityCfg {
            name: name.into(),
            kind: "claude".into(),
            env: env(pairs),
            args: vec![],
        };
        app.tools.lock().await.insert(
            LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![
                    ident("cc0", &[]),
                    ident("cc1", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-ccompany")]),
                    ident("cc2", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc2"), ("CODEX_HOME", "$HOME/.codex-cc2")]),
                    // codex 自己的身分（kind = codex）才可能分開成 `codex:<name>`。
                    crate::config::IdentityCfg {
                        name: "cx2".into(),
                        kind: "codex".into(),
                        env: env(&[("CODEX_HOME", "$HOME/.codex-cx2")]),
                        args: vec![],
                    },
                ],
                checked_at: crate::db::now(),
            },
        );
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("cc1")).await, "codex");
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("cc0")).await, "codex");
        // 2026-09-14 使用者指正：ccN 是 Claude Code 的帳號代號，就算 cc2 設了 CODEX_HOME，它仍是 claude 的身分，
        // codex 不該有 `codex:cc2`。
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("cc2")).await, "codex");
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("cx2")).await, "codex:cx2", "codex 自己的身分才分開");
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "claude", Some("cc1")).await, "claude:cc1");
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "codex", Some("nobody")).await, "codex:nobody", "查不到的身分寧可分開");

        // 裸 codex 撞限：cc1 的 codex bot 讀得到，cc2 的讀不到（它有自己的帳號）。
        let hit = LimitHit { message: "You've hit your usage limit.".into(), until: Some("2999-01-01T00:00:00Z".into()), at: crate::db::now(), bucket: None };
        let mut q = codex_q("codex-limit-hit", Some(hit));
        q.five_hour = Some(Window { used_pct: 100.0, resets_at: None });
        set(&app, LOCAL_HOST, "codex", q).await;
        let bot = |identity: &str| crate::db::Bot {
            id: format!("b-{identity}"),
            project_id: "p".into(),
            name: identity.into(),
            kind: "codex".into(),
            model: None,
            effort: None,
            fast: 0,
            persona: None,
            args_json: "[]".into(),
            autostart: 0,
            inject_hooks: 1,
            auto_approve: 1,
            identity: Some(identity.into()),
            env_json: "{}".into(),
            managed_by: "user".into(),
            cwd: None,
            herdr_session: None,
            parent_bot_id: None,
            is_primary: 0,
            hook_token: "t".into(),
            deleted_at: None,
            created_at: crate::db::now(),
        };
        assert!(limit_hit_for_bot(&app, &bot("cc1")).await.is_some(), "cc1 的 codex bot 讀的是裸 codex");
        assert!(limit_hit_for_bot(&app, &bot("cx2")).await.is_none(), "cx2 是 codex 自己的另一個帳號，不借預設帳號的撞限");
    }

    /// AGM 的條件：寫入 key 要跟 `limit_hit_for_bot` 查法對得起來，且不能洗掉「撞上限」。
    #[tokio::test]
    async fn the_status_line_writes_where_the_lookup_reads_and_keeps_the_limit_hit() {
        let app = crate::testing::env().await.app.clone();
        let base = quota_base("codex", Some("astra"));
        assert_eq!(base, "codex:astra");
        assert_eq!(quota_base("codex", None), "codex");
        // 2026-09-14 使用者：額度列冒出第二個 codex。對 codex 共用預設帳號的身分寫裸 key。
        assert_eq!(quota_base_default_aware("codex", Some("cc0"), true), "codex");
        assert_eq!(quota_base_default_aware("codex", Some("cc2"), false), "codex:cc2");
        assert_eq!(quota_base_default_aware("codex", None, true), "codex");
        assert_eq!(quota_base_default_aware("claude", Some("cc1"), false), "claude:cc1");
        assert_eq!(quota_base("codex", Some("  ")), "codex", "空白身分就是沒指定");

        // 清掉的話 assignment 會立刻又派工過去（718d025 的 quota_blocked 靠這一格）。
        let hit = LimitHit {
            message: "You've hit your usage limit.".into(),
            until: Some("2999-01-01T00:00:00Z".into()),
            at: crate::db::now(),
            bucket: None,
        };
        let mut server = quota_from_codex_status(
            &crate::codex_live::CodexStatusQuota { five_hour_left: Some(50.0), weekly_left: Some(50.0) },
            Some("astra"),
        )
        .unwrap();
        server.limit_hit = Some(hit);
        server.source = "codex-app-server".into();
        set(&app, LOCAL_HOST, &base, server).await;

        let fresh = quota_from_codex_status(
            &crate::codex_live::CodexStatusQuota { five_hour_left: Some(90.0), weekly_left: Some(48.0) },
            Some("astra"),
        )
        .unwrap();
        assert!(fresh.limit_hit.is_none(), "狀態列本來就讀不到這一格");
        set(&app, LOCAL_HOST, &base, fresh).await;

        let q = app.quotas.lock().await.get(&quota_key(LOCAL_HOST, &base)).cloned().unwrap();
        assert_eq!(q.source, "codex-statusline", "來源分得出來");
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 10.0);
        assert!(q.limit_hit.is_some(), "撞上限那一格要留著");
    }

    #[test]
    fn a_status_line_without_numbers_is_not_a_reading() {
        let empty = crate::codex_live::CodexStatusQuota { five_hour_left: None, weekly_left: None };
        assert!(quota_from_codex_status(&empty, None).is_none());
    }

    #[tokio::test]
    async fn a_statusline_reading_keeps_the_probes_fable_window() {
        let app = crate::testing::env().await.app.clone();
        let probe = Quota {
            five_hour: Some(Window { used_pct: 10.0, resets_at: None }),
            seven_day: Some(Window { used_pct: 20.0, resets_at: None }),
            fable: Some(Window { used_pct: 66.0, resets_at: None }),
            reset_credits: None,
            limit_hit: None,
            plan: None, updated_at: crate::db::now(), source: "claude-usage".into(), account: Some("cc1".into()), host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:cc1", probe).await;
        let status = Quota {
            five_hour: Some(Window { used_pct: 11.0, resets_at: None }),
            seven_day: Some(Window { used_pct: 21.0, resets_at: None }),
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None, updated_at: crate::db::now(), source: "statusline".into(), account: Some("cc1".into()), host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:cc1", status).await;
        let got = app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "claude:cc1")).cloned().unwrap();
        assert_eq!(got.five_hour.unwrap().used_pct, 11.0, "the fresher 5h wins");
        assert_eq!(got.fable.unwrap().used_pct, 66.0, "the Fable window the statusLine cannot see survives");
    }

    use super::*;

    fn codex_q(source: &str, limit_hit: Option<LimitHit>) -> Quota {
        Quota {
            five_hour: Some(Window { used_pct: 0.0, resets_at: None }),
            seven_day: Some(Window { used_pct: 0.0, resets_at: None }),
            fable: None,
            reset_credits: None,
            limit_hit,
            plan: None,
            updated_at: crate::db::now(),
            source: source.into(),
            account: None,
            host: LOCAL_HOST.into(),
        }
    }

    /// 2026-09-12 使用者：量表全滿卻一直 hit limit；橫幅要黏過 app-server 輪詢。
    #[tokio::test]
    async fn a_codex_limit_hit_outlives_the_app_server_poll() {
        let app = crate::testing::env().await.app.clone();
        // 時間寫死：2026-09-13 用 `db::now()` 時 6 跑 2 敗（跨毫秒變成另一情境）。
        let hit = LimitHit {
            message: "ERROR: You've hit your usage limit.".into(),
            until: Some("2999-01-01T00:00:00.000Z".into()),
            at: "2026-09-13T14:15:30.000Z".into(),
            bucket: None,
        };
        let mut blocked = codex_q("codex-limit-hit", Some(hit));
        blocked.updated_at = "2026-09-13T14:15:30.000Z".into();
        set(&app, LOCAL_HOST, "codex", blocked).await;
        let mut poll = codex_q("codex-app-server", None);
        poll.updated_at = "2026-09-13T14:21:00.000Z".into();
        set(&app, LOCAL_HOST, "codex", poll).await;
        let got = |app: &std::sync::Arc<crate::state::App>| {
            let app = app.clone();
            async move { app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "codex")).cloned().unwrap() }
        };
        // 2026-09-12 不變量：只有 `until` 到了或 `clear_limit_hit` 才能清掉。
        assert!(got(&app).await.limit_hit.is_some(), "量表滿了不代表 CLI 收得下一句話");
        clear_limit_hit(&app, LOCAL_HOST, "codex").await;
        assert!(got(&app).await.limit_hit.is_none());
    }

    #[tokio::test]
    async fn a_limit_hit_past_its_reset_time_is_dropped() {
        let app = crate::testing::env().await.app.clone();
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let hit = LimitHit { message: "ERROR: You've hit your usage limit.".into(), until: Some(past), at: crate::db::now(), bucket: None };
        set(&app, LOCAL_HOST, "codex", codex_q("codex-limit-hit", Some(hit))).await;
        let got = app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "codex")).cloned().unwrap();
        assert!(got.limit_hit.is_none(), "過了恢復時間的橫幅不該再擋著畫面");
    }

    /// codex 當天只寫 `try again at 5:07 AM`，解析不出來寧可留著等下一回合成功再清。
    #[test]
    fn a_limit_hit_without_a_time_never_expires_on_its_own() {
        let hit = LimitHit { message: "ERROR: usage limit".into(), until: None, at: crate::db::now(), bucket: None };
        assert!(!limit_hit_expired(Some(&hit)));
        assert!(!limit_hit_expired(None));
    }

    #[test]
    fn codex_rate_limits_map_by_window() {
        let r = json!({"rateLimits": {
            "primary": {"usedPercent": 0, "windowDurationMins": 300, "resetsAt": 1788650185},
            "secondary": {"usedPercent": 18, "windowDurationMins": 10080, "resetsAt": 1789179340},
            "planType": "plus"
        }});
        let q = quota_from_codex(&r).unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 0.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 18.0);
        assert!(q.seven_day.unwrap().resets_at.unwrap().starts_with("2026-"));
        assert_eq!(q.plan.as_deref(), Some("plus"));
        assert_eq!(q.source, "codex-app-server");
    }

    /// 2026-09-10 使用者：額度用完時的重置券。
    #[test]
    fn codex_reset_credits_are_read_with_the_windows() {
        let r = json!({
            "rateLimits": {
                "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1789074446},
                "secondary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 1789450308},
                "planType": "plus"
            },
            "rateLimitResetCredits": {
                "availableCount": 1,
                "credits": [
                    {"status": "used", "title": "已經用掉的那張", "expiresAt": 1791173488},
                    {"status": "available", "title": "Full reset (Weekly + 5 hr)", "expiresAt": 1791173488}
                ]
            }
        });
        let c = quota_from_codex(&r).unwrap().reset_credits.unwrap();
        assert_eq!(c.available, 1);
        assert_eq!(c.title.as_deref(), Some("Full reset (Weekly + 5 hr)"));
        assert!(c.expires_at.unwrap().starts_with("2026-"));
    }

    #[test]
    fn no_reset_credits_field_means_none() {
        let r = json!({"rateLimits": {"primary": {"usedPercent": 3, "windowDurationMins": 300}}});
        assert!(quota_from_codex(&r).unwrap().reset_credits.is_none());
    }

    #[test]
    fn keys_are_host_scoped() {
        assert_eq!(quota_key("local", "claude"), "claude");
        assert_eq!(quota_key("local", "claude:cc1"), "claude:cc1");
        assert_eq!(quota_key("m4p", "claude:cc1"), "m4p/claude:cc1");
        let hosts = vec!["local".to_string(), "m4p".to_string()];
        assert_eq!(host_of_key("claude", &hosts), ("local", "claude"));
        assert_eq!(host_of_key("m4p/claude:cc1", &hosts), ("m4p", "claude:cc1"));
        assert_eq!(host_of_key("gone/claude", &hosts), ("local", "gone/claude"));
    }

    #[tokio::test]
    async fn snapshot_covers_live_hosts_only() {
        let dir = std::env::temp_dir().join(format!("am-quota-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
        let app = App::new(
            pool,
            client.clone(),
            client,
            cfg,
            dir.clone(),
            dir.join("agents-managerd"),
            7799,
            "t".into(),
            "test".into(),
            false,
        );
        let q = Quota {
            five_hour: Some(Window { used_pct: 10.0, resets_at: None }),
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "test".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:cc1", q.clone()).await;
        app.quotas.lock().await.insert("gone/claude".into(), q);

        let snap = snapshot(&app).await;
        let kinds = snap["kinds"].as_object().unwrap().clone();
        for k in crate::config::KINDS {
            assert!(kinds.contains_key(k), "missing base kind {k}");
        }
        assert_eq!(kinds["claude:cc1"]["host"], "local");
        assert!(!kinds.contains_key("gone/claude"), "orphan host key was kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn statusline_maps() {
        let p = json!({"hook_event_name":"StatusLine","rate_limits":{
            "five_hour":{"used_percentage":3.5,"resets_at":1788650185},
            "seven_day":{"used_percentage":22,"resets_at":1789179340}}});
        let q = quota_from_statusline(&p, Some("cc1")).unwrap();
        assert_eq!(q.five_hour.unwrap().used_pct, 3.5);
        assert!(q.fable.is_none());
        assert_eq!(q.account.as_deref(), Some("cc1"));
        assert_eq!(q.source, "statusline");
        assert!(quota_from_statusline(&json!({"model": {}}), None).is_none());
    }

    #[test]
    fn statusline_picks_up_a_fable_bucket_if_it_appears() {
        let p = json!({"rate_limits":{
            "five_hour":{"used_percentage":3.5,"resets_at":1788650185},
            "seven_day":{"used_percentage":22,"resets_at":1789179340},
            "fable":{"used_percentage":61,"resets_at":1789179340}}});
        let q = quota_from_statusline(&p, None).unwrap();
        assert_eq!(q.seven_day.unwrap().used_pct, 22.0);
        assert_eq!(q.fable.unwrap().used_pct, 61.0);
    }
}
