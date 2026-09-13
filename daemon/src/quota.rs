//! v4.0 — rate-limit quota per host + kind (`GET /api/quota`, WS `quota_updated`).
//!
//! * codex: polled from that host's `codex app-server` (`account/rateLimits/read`) at start-up,
//!   every 5 min and on `?refresh=1`.
//! * claude: statusLine push while a bot is chatting, **plus** a background `/usage` pane probe
//!   every 60 s — see [`crate::quota_claude`]. Keyed `claude` for the default account, or
//!   `claude:<identity>` when probing / receiving under an identity (identity rows do not
//!   overwrite the default-account `claude` row).
//! * grok: scraped from the TUI's `/usage` dialog in a throwaway pane every 30 s — see
//!   [`crate::quota_grok`]; grok exposes no CLI or RPC surface for it.
//!
//! Every quota belongs to the host it was read on (SPEC §14): the local host keeps the bare
//! keys (`claude`, `claude:cc1`, `codex`, `grok`) and a remote host prefixes them with its name
//! (`m4p/claude`, `m4p/claude:cc1`, …), the same shape `GET /api/models` caches under. The
//! header strip shows one host at a time — the host of the bot / project being viewed — so a
//! remote bot's statusLine must never land on the local row.

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

/// Remaining % below which the strip shows the number, not just the bar (requirement: this
/// decision is the daemon's, not the UI's — the UI only reads [`Window::low`] off the wire).
pub const LOW_REMAINING_PCT: f64 = 30.0;

/// Remaining % below which the sidebar bot row surfaces a warning — see [`Window::critical`].
pub const CRITICAL_REMAINING_PCT: f64 = 5.0;

/// Serialises throwaway `/usage` probes **per host** so a `?refresh=1` and the background
/// pollers (claude + grok) never fight over the same pane. One lock per host: a slow ssh
/// probe on `m4p` must not hold up the local one.
pub async fn probe_lock(host: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::OnceLock::new();
    let map = LOCKS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
    let lock = map.lock().await.entry(host.to_string()).or_default().clone();
    lock.lock_owned().await
}

/// Quota map key for a host: bare on `local`, `<host>/…` everywhere else.
pub fn quota_key(host: &str, base: &str) -> String {
    if host == LOCAL_HOST {
        base.to_string()
    } else {
        format!("{host}/{base}")
    }
}

/// 這顆 bot 用的是哪一把額度 key（`claude:cc1`、`codex`…），含主機前綴。
///
/// 規則跟 UI 的額度條同一套：有 identity 就是 `<kind>:<identity>`，沒有就是裸的 kind；
/// `cc0` 這種預設身份可能沒有自己的那一把，所以呼叫端要連裸 kind 一起看。
pub fn bot_quota_keys(host: &str, kind: &str, identity: Option<&str>) -> Vec<String> {
    let base = quota_base(kind, identity);
    let mut out = Vec::new();
    if base != kind {
        out.push(quota_key(host, &base));
    }
    out.push(quota_key(host, kind));
    out
}

/// 這顆 bot 的讀數要寫進哪一把（還沒加主機前綴的）key：`<kind>:<identity>`，沒有身分就是裸 kind。
///
/// 寫入端（`refresh_codex_from_panes`、claude 的 statusLine）與查詢端（[`bot_quota_keys`]、
/// [`limit_hit_for_bot`]）共用這一支，兩邊才不會各自拼字串然後對不起來。
pub fn quota_base(kind: &str, identity: Option<&str>) -> String {
    match identity.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => format!("{kind}:{id}"),
        None => kind.to_string(),
    }
}

/// 這顆 bot 的帳號現在是不是被 CLI 擋著（[`LimitHit`]），還沒過期的才算。
pub async fn limit_hit_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Option<LimitHit> {
    let host = crate::db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
    let keys = bot_quota_keys(&host, &bot.kind, bot.identity.as_deref());
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

/// 這顆 bot 的額度**下一次重置**是什麼時候（5h 優先，沒有就看週窗）。
///
/// 給 `supervisor::controller` 判斷「被額度擋下之後什麼時候再試」用：CLI 橫幅上的時間會舊
/// （2026-09-13：橫幅還寫著剛過去的 22:15，app-server 已經說 22:20 重置），所以兩邊都要看。
/// 只回**還在未來**的時間。
pub async fn next_reset_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Option<String> {
    let host = crate::db::bot_host(&app.db, &bot.id).await.unwrap_or_else(|_| LOCAL_HOST.to_string());
    let keys = bot_quota_keys(&host, &bot.kind, bot.identity.as_deref());
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

/// The host a quota map key belongs to, given the hosts that exist (`local` for anything else).
pub fn host_of_key<'a>(key: &'a str, hosts: &[String]) -> (&'a str, &'a str) {
    match key.split_once('/') {
        Some((h, base)) if hosts.iter().any(|n| n == h) => (h, base),
        _ => (LOCAL_HOST, key),
    }
}

/// `local` plus every remote host that is currently connected — the hosts worth polling.
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

    /// Remaining < [`LOW_REMAINING_PCT`] — the strip should show the number.
    pub fn low(&self) -> bool {
        self.remaining_pct() < LOW_REMAINING_PCT
    }

    /// Remaining < [`CRITICAL_REMAINING_PCT`] — the sidebar bot row should warn.
    pub fn critical(&self) -> bool {
        self.remaining_pct() < CRITICAL_REMAINING_PCT
    }
}

/// Manual impl (rather than `#[derive(Serialize)]`) so `low` / `critical` go over the wire as
/// computed fields — every call site still just builds a plain `{used_pct, resets_at}` struct.
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

/// Codex 的「額度重置券」（`account/rateLimits/read` 的 `rateLimitResetCredits`）。
///
/// 2026-09-10 使用者：codex 用完額度時，OpenAI 會送一張「立刻重置」的券，TUI 上寫成
/// `Reset usage`。它跟 5h／7d 兩條桶子是不同的事——桶子說「還剩多少、什麼時候回血」，
/// 這張券說「你可以現在就把它清掉，還有幾張」。額度用完的當下，這是使用者唯一還能做的動作，
/// 所以要看得到。daemon 只讀不用（按下去仍然在 codex 那邊做）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResetCredits {
    /// 現在可用的張數（`availableCount`）。0 = 有這個欄位但沒券可用。
    pub available: i64,
    /// 第一張可用券的名稱，例如 `Full reset (Weekly + 5 hr)`；沒有就 `None`。
    pub title: Option<String>,
    /// 第一張可用券的到期時間（RFC3339）。券會過期，所以這是「什麼時候用掉它」的依據。
    pub expires_at: Option<String>,
}

/// 「這個帳號現在被擋住了」——CLI 自己印出來的上限橫幅（codex：`You've hit your usage limit …`）。
///
/// 為什麼不能只靠 5h／7d 兩條桶子：那兩條是**速率**視窗，codex 的 credits 用完時它們可以是滿的，
/// app-server 也照樣回報 0% 已用。2026-09-12 使用者看到的就是這個矛盾——量表全滿，送出去卻一直
/// 回「hit your usage limit」。所以把 CLI 講的話單獨記一格，並且**黏住**：每 5 分鐘一次的
/// app-server 輪詢不帶這個欄位，[`set`] 會沿用舊值，直到 `until` 過了或下一回合真的跑成功。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LimitHit {
    /// CLI 印的那一句（已經接好換行的完整橫幅）。
    pub message: String,
    /// 橫幅裡的 `try again at …`（RFC3339）；沒寫時間就 `None`，那就只能等下一次成功的回合清掉。
    pub until: Option<String>,
    /// 什麼時候撞到的（RFC3339）。
    pub at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Quota {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Max 方案才有的 Fable 週額度（`Current week (Fable)`）。跟 [`Quota::seven_day`] 同型
    /// 也同樣是週窗，只是只算 Fable 那一份；沒有這條桶子的方案／來源就是 `None`，UI 完全不畫。
    pub fable: Option<Window>,
    /// Codex 的額度重置券；只有 codex 這個來源會有，其餘一律 `None`。
    pub reset_credits: Option<ResetCredits>,
    /// CLI 說這個帳號現在被擋住了。見 [`LimitHit`]；沒撞到就是 `None`。
    pub limit_hit: Option<LimitHit>,
    pub plan: Option<String>,
    pub updated_at: String,
    pub source: String,
    pub account: Option<String>,
    /// Host this was read on. Parsers build `local`; [`set`] stamps the real one, so no call
    /// site can store a remote reading under a local key by forgetting a field.
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

/// `rateLimitResetCredits` → [`ResetCredits`]。`availableCount` 才是張數；`credits[]` 裡可能
/// 還有已經用掉／過期的，所以標題與到期時間只取第一張 `status == "available"` 的。
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

/// `account/rateLimits/read` result → Quota. Windows are matched by `windowDurationMins`
/// (300 → 5 h, 10080 → 7 d), falling back to primary/secondary order.
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

/// Claude Code statusLine JSON (`rate_limits.five_hour / seven_day`) → Quota.
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
    // Max 方案的 Fable 週桶。實測（2026-09-07）真的 statusLine payload 裡只有 `five_hour` /
    // `seven_day` 兩個 key，所以這裡只是「有就收」——桶名兩種寫法都認，沒有就維持 `None`。
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

/// Store + push `quota_updated`. `base` is the host-less key (`claude`, `claude:cc1`, …);
/// the stored key and `quota.host` are both derived from `host` here.
pub async fn set(app: &Arc<App>, host: &str, base: &str, mut q: Quota) {
    q.host = host.to_string();
    let key = quota_key(host, base);
    let mut quotas = app.quotas.lock().await;
    // Sources do not all know the same windows. Claude's statusLine reports 5h / 7d only and
    // fires every few seconds while a bot is chatting, so without this it wiped the Fable
    // reading the `/usage` probe had just made (cc1 showed 5h / 7d and no F bar while the idle
    // accounts kept theirs). A window the new reading does not carry keeps the previous value.
    if q.fable.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.fable = prev.fable.clone();
        }
    }
    // 同一條規則給兩條桶子：新讀數看不到的窗口，沿用上一份。狀態列被行寬截斷時只讀得到 5h
    // （2026-09-13 實機：`weekly 24%…` 解析失敗），整份寫進去會把 app-server 的 7d 洗成「不知道」，
    // header 上那一格就少一條（使用者回報）。少一個數字是「這次沒看到」，不是「沒有了」。
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
    // 同理：重置券只有 codex 的 app-server 讀得到，別的來源（statusLine）寫進同一把 key 時
    // 不該把它抹掉。
    if q.reset_credits.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.reset_credits = prev.reset_credits.clone();
        }
    }
    // 「撞上限」只有 CLI 的橫幅看得到（§12.4）。app-server 的輪詢每 5 分鐘把同一把 key 蓋掉一次，
    // 沿用之前額度會在 CLI 還在拒絕的時候變回滿格——2026-09-12 使用者看到的就是這個。
    if q.limit_hit.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.limit_hit = prev.limit_hit.clone();
        }
    }
    if limit_hit_expired(q.limit_hit.as_ref()) {
        q.limit_hit = None;
    }
    // 重置時間只有 app-server／statusLine 那種結構化來源帶得出來。CLI 自己的狀態列只寫
    // 「還剩幾 %」，沒有時間——那不代表重置時間變成未知，所以沿用上一份讀數的。少了這一條，
    // pane 讀數一寫進來，量表上的「N 小時後重置」就會消失。
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

/// 這張「撞上限」已經過了它自己寫的恢復時間了嗎？沒寫時間的一律**不**過期——只能靠下一次
/// 成功的回合（[`clear_limit_hit`]）把它清掉。
fn limit_hit_expired(hit: Option<&LimitHit>) -> bool {
    let Some(until) = hit.and_then(|h| h.until.as_deref()) else { return false };
    match chrono::DateTime::parse_from_rfc3339(until) {
        Ok(t) => chrono::Utc::now() >= t.with_timezone(&chrono::Utc),
        Err(_) => false,
    }
}

/// 這個帳號又能跑了：一回合真的跑完就把「撞上限」拿掉，不必等它自己寫的時間。
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

/// `GET /api/quota` body: every known key, with the three base kinds always present **per
/// host** (so the strip can show empty bars for a host that has not reported yet). Keys for
/// hosts that no longer exist are dropped rather than shown under the wrong host.
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
        // `foo/claude` with no `foo` host left in the config would otherwise be read as a
        // local key by everything downstream — drop it instead.
        let orphan = k.contains('/') && host_of_key(k, &hosts).0 == LOCAL_HOST;
        if !orphan {
            m.insert(k.clone(), json!(v));
        }
    }
    json!({"kinds": Value::Object(m)})
}

/// codex 自己狀態列上的剩餘量 → Quota。
///
/// 只帶 5h / 7d 兩條（CLI 就只寫這兩個）；`resets_at` 交給 [`set`] 沿用上一份讀數的。
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

/// 掃一遍這台主機上跑著的 codex pane，把它們狀態列上的額度寫進去。
///
/// 為什麼需要：`account/rateLimits/read` 每 5 分鐘一次，而且它跟 CLI 自己知道的數字會差一整輪
/// （2026-09-13 使用者截圖：量表停在 5h 100、pane 上寫 5h 90% left）。CLI 的那一行是它**當下**
/// 拿來擋你的依據，所以它更接近真的。寫進該 bot 身分的那把 key（`codex` / `codex:<identity>`），
/// 跟 app-server 的讀數共用同一格——後到的覆蓋先到的，`resets_at`、重置券與 limit_hit 由
/// [`set`] 沿用。
pub async fn refresh_codex_from_panes(app: &Arc<App>, host: &str) -> usize {
    let rows: Vec<(String, Option<String>)> = match sqlx::query_as(
        "SELECT r.pane_id, b.identity FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
          WHERE p.host = ? AND b.kind = 'codex' AND r.state = 'running' AND r.pane_id IS NOT NULL
            AND b.deleted_at IS NULL",
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
        // 查詢端（`limit_hit_for_bot` / `bot_quota_keys`）用的是同一支，兩邊不會對不起來。
        let base = quota_base("codex", identity.as_deref());
        // 同一個身分讀一次就夠：兩顆 bot 共用帳號時第二顆只是再寫一次同樣的數字。
        if !seen.insert(base.clone()) {
            continue;
        }
        let Some(client) = app.herdr_for(host).await else { continue };
        let Ok(read) = client.pane_read(&pane_id, "visible", 60).await else { continue };
        let Some(parsed) = crate::codex_live::parse_status_quota(&read.text) else { continue };
        let Some(q) = quota_from_codex_status(&parsed, identity.as_deref()) else { continue };
        set(app, host, &base, q).await;
        wrote += 1;
    }
    wrote
}

/// One codex refresh on `host`. `Ok(false)` = codex not installed there (quota stays null).
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

/// Start-up + every 5 min, for `local` and every connected remote host.
pub fn spawn_codex_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            for host in pollable_hosts(&app).await {
                match refresh_codex(&app, &host).await {
                    Ok(true) => {}
                    Ok(false) => tracing::info!(host = %host, "codex not installed; codex quota stays null"),
                    Err(e) => tracing::warn!(host = %host, error = %e, "codex quota refresh failed"),
                }
                // app-server 那份是帳號層級、每 5 分鐘一次；CLI 狀態列是**這個 pane 現在**看到的
                // 數字，而且分得出身分。兩個都收，後到的蓋前面的。
                let n = refresh_codex_from_panes(&app, &host).await;
                if n > 0 {
                    tracing::debug!(host = %host, panes = n, "codex quota read off the status line");
                }
            }
            tokio::time::sleep(CODEX_POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    /// CLI 狀態列寫的是**剩餘**，我們存的是**已用**；而且它沒有重置時間，不能因此把
    /// app-server 帶來的 `resets_at` 洗掉（2026-09-13 使用者：量表停在舊數字）。
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

    /// 狀態列被行寬截斷時只讀得到 5h；那一份寫進去不可以把 app-server 的 7d 洗掉
    /// （2026-09-13 使用者：header 的 codex 只剩一條）。
    #[tokio::test]
    async fn a_partial_reading_keeps_the_window_it_could_not_see() {
        let app = crate::testing::env().await.app.clone();
        let mut full = codex_q("codex-app-server", None);
        full.five_hour = Some(Window { used_pct: 30.0, resets_at: Some("2026-09-13T19:22:00.000Z".into()) });
        full.seven_day = Some(Window { used_pct: 76.0, resets_at: Some("2026-09-18T00:00:00.000Z".into()) });
        set(&app, LOCAL_HOST, "codex", full).await;

        // 只讀到 5h 的那種（`weekly …` 被截斷）。
        let mut partial = codex_q("codex-statusline", None);
        partial.five_hour = Some(Window { used_pct: 64.0, resets_at: None });
        partial.seven_day = None;
        set(&app, LOCAL_HOST, "codex", partial).await;

        let q = app.quotas.lock().await.get("codex").cloned().unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 64.0, "看得到的那條要更新");
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 76.0, "看不到的那條沿用，不是清空");
        assert_eq!(q.seven_day.as_ref().unwrap().resets_at.as_deref(), Some("2026-09-18T00:00:00.000Z"));
    }

    /// AGM 的條件：寫進去的 key 要跟 `limit_hit_for_bot` 的查法（`<kind>:<identity>` → 裸 kind）
    /// 對得起來，而且狀態列的讀數不能把 CLI 說的「撞上限」洗掉。
    #[tokio::test]
    async fn the_status_line_writes_where_the_lookup_reads_and_keeps_the_limit_hit() {
        let app = crate::testing::env().await.app.clone();
        // 帶身分的 codex bot：寫進 `codex:astra`，查也先查它。
        let base = quota_base("codex", Some("astra"));
        assert_eq!(base, "codex:astra");
        assert_eq!(bot_quota_keys(LOCAL_HOST, "codex", Some("astra"))[0], quota_key(LOCAL_HOST, &base));
        // 沒有身分的寫裸 kind，查的第一把也是它。
        assert_eq!(quota_base("codex", None), "codex");
        assert_eq!(bot_quota_keys(LOCAL_HOST, "codex", Some("  "))[0], quota_key(LOCAL_HOST, "codex"));

        // CLI 說撞上限之後，狀態列再寫一次讀數不能把它清掉——清掉的話 assignment 那邊會
        // 立刻又把工作派過去（718d025 的 quota_blocked 就是靠這一格）。
        let hit = LimitHit {
            message: "You've hit your usage limit.".into(),
            until: Some("2999-01-01T00:00:00Z".into()),
            at: crate::db::now(),
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

    /// 狀態列讀不到額度時不要寫一筆空的——那會把 app-server 的數字蓋成「不知道」。
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

    /// 2026-09-12 使用者：codex 的量表全滿、送出去卻一直回「hit your usage limit」。
    /// CLI 的橫幅是唯一看得到這件事的地方（credits 用完時 5h／7d 兩條桶子可以是 0%），
    /// 所以它要黏過五分鐘一次的 app-server 輪詢，直到恢復時間到、或下一回合真的跑成功。
    #[tokio::test]
    async fn a_codex_limit_hit_outlives_the_app_server_poll() {
        let app = crate::testing::env().await.app.clone();
        // 時間全部寫死：這個測試講的是「兩份讀數誰蓋掉誰」，不該跟著現在幾點變答案
        // （2026-09-13 它 6 跑 2 敗——`db::now()` 有時候跨過一毫秒，就變成另一個情境）。
        let hit = LimitHit {
            message: "ERROR: You've hit your usage limit.".into(),
            until: Some("2999-01-01T00:00:00.000Z".into()),
            at: "2026-09-13T14:15:30.000Z".into(),
        };
        let mut blocked = codex_q("codex-limit-hit", Some(hit));
        blocked.updated_at = "2026-09-13T14:15:30.000Z".into();
        set(&app, LOCAL_HOST, "codex", blocked).await;
        // 輪詢回來（比橫幅晚六分鐘）：桶子空的、而且它根本不知道有這回事。
        let mut poll = codex_q("codex-app-server", None);
        poll.updated_at = "2026-09-13T14:21:00.000Z".into();
        set(&app, LOCAL_HOST, "codex", poll).await;
        let got = |app: &std::sync::Arc<crate::state::App>| {
            let app = app.clone();
            async move { app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "codex")).cloned().unwrap() }
        };
        // 這是 2026-09-12 立下的那條不變量：codex 的 credits 用完時，5h／7d 兩條**速率**視窗可以
        // 是滿的，app-server 也照實回報 0% 已用——唯一講出「現在收不下工作」的是 CLI 的橫幅。
        // 所以後到的輪詢再怎麼說還有額度，都不會把它蓋掉；只有 `until` 到了、或下一回合真的跑完
        // （`clear_limit_hit`）才算數。
        assert!(got(&app).await.limit_hit.is_some(), "量表滿了不代表 CLI 收得下一句話");
        // 一回合真的跑完就清掉。
        clear_limit_hit(&app, LOCAL_HOST, "codex").await;
        assert!(got(&app).await.limit_hit.is_none());
    }

    /// 恢復時間過了就自己消失——不必等下一回合，也不必使用者手動清。
    #[tokio::test]
    async fn a_limit_hit_past_its_reset_time_is_dropped() {
        let app = crate::testing::env().await.app.clone();
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let hit = LimitHit { message: "ERROR: You've hit your usage limit.".into(), until: Some(past), at: crate::db::now() };
        set(&app, LOCAL_HOST, "codex", codex_q("codex-limit-hit", Some(hit))).await;
        let got = app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "codex")).cloned().unwrap();
        assert!(got.limit_hit.is_none(), "過了恢復時間的橫幅不該再擋著畫面");
    }

    /// 沒寫恢復時間的橫幅不會自己過期：codex 當天撞上限時只寫 `try again at 5:07 AM`，
    /// 解析不出來時寧可留著，等下一回合成功再清。
    #[test]
    fn a_limit_hit_without_a_time_never_expires_on_its_own() {
        let hit = LimitHit { message: "ERROR: usage limit".into(), until: None, at: crate::db::now() };
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

    /// 2026-09-10 使用者：codex 額度用完時 OpenAI 會給「立刻重置」的券。桶子只說什麼時候
    /// 回血，這張券說「現在就能清掉，還有幾張」——額度歸零的當下那是唯一還能做的事。
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
        // 標題取的是**還能用**的那一張，不是陣列的第一個。
        assert_eq!(c.title.as_deref(), Some("Full reset (Weekly + 5 hr)"));
        assert!(c.expires_at.unwrap().starts_with("2026-"));
    }

    /// 沒有這個欄位的舊 codex（或別的 kind）＝ `None`，UI 什麼都不畫。
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
        // A key for a host that is gone must not be read as a local one.
        assert_eq!(host_of_key("gone/claude", &hosts), ("local", "gone/claude"));
    }

    /// The snapshot always carries the three base kinds for every live host, and drops rows
    /// belonging to hosts that no longer exist (rather than folding them into `local`).
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

    /// 若哪天 statusLine 真的多一個 fable 桶，它要落在 `fable`，不能蓋掉週窗。
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
