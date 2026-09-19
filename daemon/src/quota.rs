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

/// [`quota_base_for_host`]，但**算不準就回錯**：寫撞限要落在查詢端之後會讀的那一把 key（#108 重開）。
/// 那台主機的身分表還沒偵測完（重啟後、`tools::detect` 之前）又不是手寫的 `[[identities]]` 時，共用預設帳號的
/// `cc0` 會被算成 `claude:cc0`：偵測完之後查詢端讀裸 `claude`，那一格的撞限就沒人看得到。
pub async fn resolve_quota_base(app: &Arc<App>, host: &str, kind: &str, identity: Option<&str>) -> Result<String> {
    if let Some(idn) = identity.map(str::trim).filter(|s| !s.is_empty()) {
        if crate::tools::identity_for_host(app, host, idn).await.is_none() && !app.tools.lock().await.contains_key(host) {
            anyhow::bail!("`{host}` 的身分表還沒偵測完，算不出身分 `{idn}` 的額度 key");
        }
    }
    Ok(quota_base_for_host(app, host, kind, identity).await)
}

/// 額度記在哪個身分上（issue #238）：pane 裡實際的帳號＝這個 run 起來時的身分。PATCH 改了身分、還沒重啟時，
/// `bots.identity` 已經是新的、pane 還是舊帳號——讀數、撞限、閘門都要跟著 run 走，不然舊帳號用盡記到新帳號名下
/// （新帳號被誤擋、舊帳號的其他 bot 照樣被派工）。沒有 run、或 run 沒記身分（不是 daemon 起的、升級前的舊列）才用設定的。
pub fn identity_for_run(bot: &crate::db::Bot, run: Option<&crate::db::Run>) -> Option<String> {
    match run.and_then(|r| r.started_identity()) {
        Some(started) => started,
        None => bot.identity.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
    }
}

/// [`identity_for_run`]，run 用這顆 bot 現在的 active run。讀不到 run 回錯：不能拿設定的身分猜（那正是會記錯格的那一個）。
pub async fn billing_identity(app: &Arc<App>, bot: &crate::db::Bot) -> Result<Option<String>> {
    let run = crate::db::active_run(&app.db, &bot.id).await?;
    Ok(identity_for_run(bot, run.as_ref()))
}

/// 查詢端：這顆 bot 的讀數在哪幾把 key。先查它自己那一把（收斂規則同寫入端），**只有**收斂到裸 kind
/// 的身分才會落在裸 key——有自己 home 的身分（cc2 帶 `CODEX_HOME`）不借預設帳號的數字。身分是 [`billing_identity`]。
async fn keys_for_bot(app: &Arc<App>, host: &str, bot: &crate::db::Bot, identity: Option<&str>) -> Vec<String> {
    let base = quota_base_for_host(app, host, &bot.kind, identity).await;
    vec![quota_key(host, &base)]
}

/// CLI 的模型字屬於哪一家（`fable`、`claude-fable-5-1`、`Fable 5.1`、`opus[1m]` 都認得）。認不出來回 `None`。
pub fn model_family(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    ["fable", "opus", "sonnet", "haiku"].into_iter().find(|f| m.contains(f))
}

/// 這一桶是某個模型專屬的（撞了只擋跑那個模型的 bot）就回那個模型；5h、7d、沒有桶名的是整個帳號共用，回 `None`。
///
/// claude 的模型週桶：`Fable limit`（`seven_day_overage_included`）、`Opus limit`（`seven_day_opus`）、
/// `Sonnet limit`（`seven_day_sonnet`）——CLI 2.1.273 的字串表把它們跟 `weekly limit` 分開列（review3 c4 M1）。
fn model_of_bucket(bucket: &str) -> Option<&'static str> {
    match bucket {
        "fable" => Some("fable"),
        "opus" => Some("opus"),
        "sonnet" => Some("sonnet"),
        _ => None,
    }
}

/// 撞了 `bucket` 那一桶，擋不擋一顆跑 `model` 的 bot（`None` = 不知道它在跑什麼）。
///
/// 5h／7d 與沒有桶名的撞限（codex、開機回填）是整個帳號的，照舊擋所有 bot。模型專屬的桶只擋跑那個模型的
/// bot：以前不分桶，巡檢（cc0、fable）一撞 Fable 上限，同是 cc0、跑 opus 的協調者與交辦就一路被擋到
/// Fable 週窗重置（review3 c3 H2）。不知道 bot 在跑什麼模型時保守地照舊擋。
/// `limit_hit_for_bot`（派工、回合結束、重送）與協調者的 `quota_state` 都走這一條。
pub fn bucket_blocks_model(bucket: Option<&str>, model: Option<&str>) -> bool {
    let Some(only) = bucket.and_then(model_of_bucket) else { return true };
    match model.and_then(model_family) {
        Some(family) => family == only,
        None => true,
    }
}

/// [`bucket_blocks_model`]，吃整筆撞限。
pub fn limit_hit_blocks_model(hit: &LimitHit, model: Option<&str>) -> bool {
    bucket_blocks_model(hit.bucket.as_deref(), model)
}

/// 這顆 bot 現在實際在跑的模型：run 的 `runtime_model`（啟動 argv 與 `/model` 會更新它），沒有就用設定值。
///
/// 讀不到 run 就是「不知道」（`None`，[`bucket_blocks_model`] 照舊擋），不退回設定值：`/model` 換過的話設定值是錯的，
/// 撞的是模型專屬的桶時會把正在跑那個模型的 bot 放行（#108 重開）。
pub async fn running_model(app: &Arc<App>, bot: &crate::db::Bot) -> Option<String> {
    let runtime = match crate::db::active_run(&app.db, &bot.id).await {
        Ok(run) => run.and_then(|r| r.runtime_model),
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the running model; treating it as unknown");
            return None;
        }
    };
    runtime.or_else(|| bot.model.clone()).filter(|m| !m.trim().is_empty())
}

/// 擋住這顆 bot 的撞限：沒過期、而且撞的那一桶管得到它在跑的模型（[`bucket_blocks_model`]）。
///
/// 讀不到這顆 bot 在哪台主機就回錯（#108 重開）：以前退回 `local`，查錯 key、回「沒撞限」，遠端那個已經用盡的身分
/// 就被放行。撞限記不進正確那把 key 而欠著的那一筆（`turn_error::owed_limit_hit`）先算。
pub async fn try_limit_hit_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Result<Option<LimitHit>> {
    let identity = billing_identity(app, bot).await?;
    if let Some(hit) = crate::turn_error::owed_limit_hit(app, bot, identity.as_deref()).await {
        return Ok(Some(hit));
    }
    let host = crate::db::bot_host(&app.db, &bot.id).await?;
    let keys = keys_for_bot(app, &host, bot, identity.as_deref()).await;
    let model = running_model(app, bot).await;
    let q = app.quotas.lock().await;
    for k in keys {
        if let Some(hit) = q.get(&k).and_then(|x| x.limit_hit.clone()) {
            if !limit_hit_expired(Some(&hit)) && limit_hit_blocks_model(&hit, model.as_deref()) {
                return Ok(Some(hit));
            }
        }
    }
    Ok(None)
}

/// [`try_limit_hit_for_bot`]，讀不到時回 `None`（記 warn）。只剩 supervisor 的派送／重送在用：那邊拿到撞限會 park、
/// 群組任務還會換身分，不能拿假的撞限去擋；改用 `try_` 版、讀不到就延後，見 #108 重開時開的 supervisor 票。
pub async fn limit_hit_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Option<LimitHit> {
    match try_limit_hit_for_bot(app, bot).await {
        Ok(hit) => hit,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot tell whether this bot's identity has hit its limit");
            None
        }
    }
}

/// 只回未來的重置時間；CLI 橫幅時間會舊，supervisor 要兩邊都看（2026-09-13：橫幅 22:15、app-server 22:20）。
/// 讀不到主機回 `None`（沒有這份證據，只看橫幅的時間）：退回 `local` 會拿到本機帳號的重置時間，把重送提早。
pub async fn next_reset_for_bot(app: &Arc<App>, bot: &crate::db::Bot) -> Option<String> {
    let host = match crate::db::bot_host(&app.db, &bot.id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the host of a bot; no quota reset time from its readings");
            return None;
        }
    };
    let identity = match billing_identity(app, bot).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the run of a bot; no quota reset time from its readings");
            return None;
        }
    };
    let keys = keys_for_bot(app, &host, bot, identity.as_deref()).await;
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
            return Some(crate::db::iso_at(t));
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

/// 每台主機各跑一份、**同時**跑，全部跑完才回來（SPEC §14.3）。以前三個 poller 都是 `for host in …` 一台一台 await：
/// `probe_lock` 是 per host，本意就是「一台慢的 ssh 主機不拖住本機」，串列迴圈下這層保護等於沒有（review 2026-09-16）。
pub async fn for_each_host<F, Fut>(hosts: Vec<String>, f: F)
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for host in hosts {
        set.spawn(f(host));
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            tracing::warn!(error = %e, "a per-host quota poll task failed");
        }
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
/// 身分偵測完之前寫進分開那一格、現在已經收斂到裸 `kind` 的 key（daemon 重啟那一秒最常見：第一筆 statusline
/// 比身分偵測先到，[`quota_base_for_host`] 查不到身分就寧可分開）。之後的讀數都寫裸 key，那一格停在啟動當下、
/// 沒有 Fable，留著就會被讀到（2026-09-16 使用者：「怎麼又看不見 Fable 的剩餘」）。查不到的身分照舊保留。
async fn stale_split_keys(app: &Arc<App>, host: &str, kind: &str) -> Vec<String> {
    let prefix = quota_key(host, &format!("{kind}:"));
    let candidates: Vec<String> = app.quotas.lock().await.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
    let mut stale = Vec::new();
    for key in candidates {
        if quota_base_for_host(app, host, kind, Some(&key[prefix.len()..])).await == kind {
            stale.push(key);
        }
    }
    stale
}

pub async fn set(app: &Arc<App>, host: &str, base: &str, mut q: Quota) {
    q.host = host.to_string();
    let key = quota_key(host, base);
    let stale = if base.contains(':') { Vec::new() } else { stale_split_keys(app, host, base).await };
    // 撞限校正只看**這份讀數自己帶來的**窗；下面沿用的舊窗不是新證據。
    let brings_its_own_hit = q.limit_hit.is_some();
    let fresh = q.clone();
    let mut quotas = app.quotas.lock().await;
    for k in &stale {
        if quotas.remove(k).is_some() {
            tracing::info!(host, stale = %k, bare = %key, "dropped a split quota key that now resolves to the bare key");
        }
    }
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
    if !brings_its_own_hit {
        q.limit_hit = q.limit_hit.take().and_then(|h| recalibrate_limit_hit(h, &fresh));
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

fn parse_utc(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s.trim()).ok().map(|t| t.with_timezone(&chrono::Utc))
}

/// 知道桶名的撞限（claude 橫幅）遇到**那一桶的新讀數**時校正，不然保底時間（Fable 7 天）與橫幅當下讀到的
/// `resets_at` 之後永遠不會被真讀數改寫——claude 沒有 `clear_limit_hit`，帳號恢復後照樣擋到 7 天（review 2026-09-16 M2）。
///
/// - 讀數的窗是撞限**之後**才開的（`resets_at − 窗長 ≥ hit.at`）：那一桶已經重置過了，撞限作廢。
///   用窗的起點判斷而不是看百分比：撞限前就開始、撞限後才回來的 `/usage` 探測，百分比可能還沒到頂。
/// - 否則撞限最晚到這個窗重置為止：`until = min(until, resets_at)`。
/// - 讀數的窗在撞限**之前**就結束了（`resets_at ≤ hit.at`）：那是上一個窗的讀數（閒置的 5h 窗，`/usage` 照樣回上一個
///   重置時間），說不出這次撞限的事，不動——拿它取 `min`，撞限會被拉到過去、當場作廢（#236）。
///
/// 沒有桶名（codex 的 credits 用完、開機回填的格子）一律不動：那種撞限只有橫幅說得準。
fn recalibrate_limit_hit(mut hit: LimitHit, fresh: &Quota) -> Option<LimitHit> {
    let (window, len) = match hit.bucket.as_deref() {
        Some("five_hour") => (fresh.five_hour.as_ref(), chrono::Duration::hours(5)),
        Some("seven_day") => (fresh.seven_day.as_ref(), chrono::Duration::days(7)),
        Some("fable") => (fresh.fable.as_ref(), chrono::Duration::days(7)),
        // Opus／Sonnet 的週桶沒有自己的量表，但跟 7d 同一個週期重置（`/usage` 的 `weekly_scoped` 列與
        // `weekly_all` 同一個 `resets_at`）：拿 7d 的窗來校正時間，不看它的百分比。
        Some("opus") | Some("sonnet") => (fresh.seven_day.as_ref(), chrono::Duration::days(7)),
        _ => return Some(hit),
    };
    let Some(resets) = window.and_then(|w| w.resets_at.as_deref()).and_then(parse_utc) else { return Some(hit) };
    let Some(at) = parse_utc(&hit.at) else { return Some(hit) };
    if resets <= at {
        return Some(hit);
    }
    if resets - len >= at {
        return None;
    }
    if hit.until.as_deref().and_then(parse_utc).map_or(true, |u| resets < u) {
        hit.until = Some(crate::db::iso_at(resets));
    }
    Some(hit)
}

/// 沒寫時間的一律**不**過期，只能靠 [`clear_limit_hit`]。
pub fn limit_hit_expired(hit: Option<&LimitHit>) -> bool {
    let Some(until) = hit.and_then(|h| h.until.as_deref()) else { return false };
    match chrono::DateTime::parse_from_rfc3339(until) {
        Ok(t) => chrono::Utc::now() >= t.with_timezone(&chrono::Utc),
        Err(_) => false,
    }
}

/// 這顆 bot 真的答完一回合：清掉**它自己那把 key** 的撞限。key 跟寫入端（`apply_codex_limit_hit_quota`）
/// 與查詢端（[`limit_hit_for_bot`]）走同一支 [`quota_base_for_host`]——以前寫死裸 `codex`，有自己
/// `CODEX_HOME` 的 `cx2` 一撞限就永遠清不掉，反而把預設帳號真的撞限清掉（review 2026-09-16 H1）。
///
/// 讀不到主機就不清（#108 重開）：退回 `local` 會把**本機**那個身分真的撞限清掉。少清一次只是多擋到到期。
pub async fn clear_limit_hit_for_bot(app: &Arc<App>, bot: &crate::db::Bot) {
    let host = match crate::db::bot_host(&app.db, &bot.id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the host of a bot; its limit hit is left in place");
            return;
        }
    };
    // 清的是答完這一回合的那個帳號（issue #238）：run 起來時的身分，不是剛改、還沒生效的設定。
    let identity = match billing_identity(app, bot).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the run of a bot; its limit hit is left in place");
            return;
        }
    };
    let base = quota_base_for_host(app, &host, &bot.kind, identity.as_deref()).await;
    clear_limit_hit(app, &host, &base).await;
}

/// 每把 key 上一次被成功回合清撞限（[`clear_limit_hit`]）的時刻。只在記憶體：重啟後是空的，意思就是
/// 「這個行程還沒看過任何成功回合」，parked 的交辦照舊等 `resume_at`（AGM 裁示：記憶體空了不等於額度回來）。
/// 鍵帶 `data_dir`，一個行程裡的多個 `App`（測試）不會互相看到。
fn cleared_at() -> &'static std::sync::Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>> {
    static M: std::sync::OnceLock<std::sync::Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn cleared_at_key(app: &App, key: &str) -> String {
    format!("{}\u{0}{key}", app.data_dir.display())
}

/// 這顆 bot 的帳號在 `since` 之後有沒有被成功回合清過撞限。`resume_quota_blocked` 用它分辨
/// 「記憶體裡沒有撞限是因為真的被清掉了」與「只是重啟後什麼都不記得」（review 2026-09-16 M1）。
/// 讀不到主機回 `false`（沒有證據說額度回來了）：退回 `local` 會拿本機帳號的成功回合當作這顆的放行證據。
pub async fn limit_cleared_since(app: &Arc<App>, bot: &crate::db::Bot, since: chrono::DateTime<chrono::Utc>) -> bool {
    let host = match crate::db::bot_host(&app.db, &bot.id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the host of a bot; no evidence its limit was cleared");
            return false;
        }
    };
    let identity = match billing_identity(app, bot).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the run of a bot; no evidence its limit was cleared");
            return false;
        }
    };
    let keys = keys_for_bot(app, &host, bot, identity.as_deref()).await;
    let m = cleared_at().lock().unwrap();
    keys.iter().any(|k| m.get(&cleared_at_key(app, k)).is_some_and(|t| *t > since))
}

/// 一回合真的跑完就拿掉「撞上限」，不必等它自己寫的時間。
///
/// 不管這一格當下有沒有撞限都記下時刻：成功回合本身就是「這個帳號收得下工作」的證據，
/// 撞限可能在那之前已經自己過期、或是重啟後根本沒被回填。
pub async fn clear_limit_hit(app: &Arc<App>, host: &str, base: &str) {
    let key = quota_key(host, base);
    cleared_at().lock().unwrap().insert(cleared_at_key(app, &key), chrono::Utc::now());
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

/// 重啟後的回填來源：這一格的撞限是從「還在等額度的交辦」推回來的，不是誰真的看到橫幅。
pub const PARKED_SOURCE: &str = "parked-assignment";
/// 同上，但來源是排著的 prompt 自己記下的撞限（[`restore_limit_hit`]）。
pub const HELD_SOURCE: &str = "queued-prompt-hold";

/// 重啟後把「還在等額度」這件事補回記憶體。
///
/// `app.quotas` 只活在記憶體裡（SPEC §12.4）：daemon 一重啟就全空，而撞限橫幅要等下一次真的
/// 跑回合才會再出現。少了這一格，supervisor 會把「沒有讀數」誤讀成「額度回來了」，一開機就把
/// 整批 parked 的交辦重送出去——對方帳號其實還在擋（review 2026-09-16）。
///
/// 只寫 `limit_hit`，不動任何量表或重置時間（那是橫幅／app-server／statusLine 的事）。已經過期的
/// 時間不寫；同一格已經有**更晚**（或沒寫時間＝黏著）的撞限時也不覆蓋，所以呼叫端可以照順序把
/// 每一張交辦餵進來，最晚的那個自然會留下。回傳有沒有真的寫進去。
pub async fn seed_limit_hit(app: &Arc<App>, host: &str, base: &str, until: &str, message: &str, bucket: Option<String>) -> bool {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&chrono::Utc));
    let Some(t) = parse(until) else { return false };
    if t <= chrono::Utc::now() {
        return false;
    }
    let key = quota_key(host, base);
    let mut quotas = app.quotas.lock().await;
    if let Some(hit) = quotas.get(&key).and_then(|q| q.limit_hit.as_ref()) {
        if !limit_hit_expired(Some(hit)) {
            // 沒寫時間的撞限永不過期（`limit_hit_expired`），一定比任何時間都「晚」。
            let keep = match hit.until.as_deref().and_then(parse) {
                None => true,
                Some(prev) => prev >= t,
            };
            if keep {
                return false;
            }
        }
    }
    let now = crate::db::now();
    let mut q = quotas.get(&key).cloned().unwrap_or_else(|| Quota {
        five_hour: None,
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: now.clone(),
        source: PARKED_SOURCE.into(),
        account: None,
        host: host.to_string(),
    });
    q.limit_hit =
        Some(LimitHit { message: message.to_string(), until: Some(until.to_string()), at: now.clone(), bucket });
    q.updated_at = now;
    let out = q.clone();
    quotas.insert(key.clone(), q);
    drop(quotas);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": out})).await;
    true
}

/// [`seed_limit_hit`] 的另一個來源：排著的 prompt 被擋下時自己記下的那筆撞限（`lifecycle::quota_hold`），
/// 重啟後**原樣**種回來——`at` 是當初撞限的時刻、帶著桶名，沒寫時間的（codex credits）照樣黏著。
///
/// `at` 要是原本那一刻，讀數才校正得準（[`recalibrate_limit_hit`] 靠它判斷窗是不是撞限之後才開的）；
/// 重啟之後、回填之前已經有新讀數進來的話，當場校正一次，不必等下一份。已經過期、被校正作廢、或這一格
/// 已有更晚（或黏著）的撞限時不寫。回傳有沒有真的寫進去。
pub async fn restore_limit_hit(app: &Arc<App>, host: &str, base: &str, hit: LimitHit) -> bool {
    if limit_hit_expired(Some(&hit)) {
        return false;
    }
    let key = quota_key(host, base);
    let mut quotas = app.quotas.lock().await;
    let hit = match quotas.get(&key) {
        Some(q) => match recalibrate_limit_hit(hit, q) {
            Some(h) => h,
            None => return false,
        },
        None => hit,
    };
    if let Some(prev) = quotas.get(&key).and_then(|q| q.limit_hit.as_ref()).filter(|h| !limit_hit_expired(Some(h))) {
        let keep = match (prev.until.as_deref().and_then(parse_utc), hit.until.as_deref().and_then(parse_utc)) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(p), Some(n)) => p >= n,
        };
        if keep {
            return false;
        }
    }
    let now = crate::db::now();
    let mut q = quotas.get(&key).cloned().unwrap_or_else(|| Quota {
        five_hour: None,
        seven_day: None,
        fable: None,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: now.clone(),
        source: HELD_SOURCE.into(),
        account: None,
        host: host.to_string(),
    });
    q.limit_hit = Some(hit);
    q.updated_at = now;
    let out = q.clone();
    quotas.insert(key.clone(), q);
    drop(quotas);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": out})).await;
    true
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

/// 這張狀態列畫面在這個行程裡是第一次看到、跟上次不一樣、還是同一張。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sighting {
    New,
    Changed,
    Same,
}

/// 記在行程裡就夠：daemon 重啟後第一次看到的畫面算 [`Sighting::New`]，年紀另外用回合時間判斷。
async fn status_line_sighting(host: &str, pane_id: &str, line: &str) -> Sighting {
    static SEEN: std::sync::OnceLock<tokio::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    let mut map = SEEN.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new())).lock().await;
    let key = format!("{host}:{pane_id}");
    match map.insert(key, line.to_string()) {
        None => Sighting::New,
        Some(prev) if prev == line => Sighting::Same,
        Some(_) => Sighting::Changed,
    }
}

/// pane 狀態列上的某一格窗（已用 %）要不要寫進去。
///
/// pane 讀的是**畫面**，不是感測器：數字停在那顆 pane 最後一回合的時候。兩個方向都出過事——
/// - 閒著三小時的 pane 每 60 秒被重新解析、蓋上 now()：app-server 剛寫進去的「視窗重置了」被蓋回見底（review 2026-09-16）。
/// - 改成「畫面沒變就不寫」之後，app-server 落後的數字（CLI 說 90% left、app-server 還是 100%）在 pane 閒著時
///   再也沒人蓋回去（2026-09-15 使用者回報的症狀回來了，review 2026-09-16 M5）。
///
/// 所以看的是「這張畫面屬於哪一個窗」：
/// - 這個行程裡看著它**變了**（剛跑完一回合）：CLI 當下的說法，照寫。
/// - 其他（同一張、或重啟後第一次看到）用那顆 pane 最後一回合的時間 `screen_at` 對 app-server 給的重置時間：
///   畫面比現在這個窗的起點（`resets_at − 窗長`）還舊 → 不採用；在同一個窗裡 → 用量只增不減，比現有的大才寫
///   （app-server 落後時補上，別的 pane 已經用更多時不倒退）。記著的窗已經過了重置時間 → 畫面要晚於那次重置才採用。
/// - 沒有重置時間（app-server 還沒答過）：只採用這個行程第一次看到的畫面，同一張不重寫。
fn pane_window_used(
    pane_used: Option<f64>,
    stored: Option<&Window>,
    window_len: chrono::Duration,
    screen_at: Option<chrono::DateTime<chrono::Utc>>,
    sighting: Sighting,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<f64> {
    let p = pane_used?;
    if sighting == Sighting::Changed {
        return Some(p);
    }
    let reset = stored.and_then(|w| w.resets_at.as_deref()).and_then(parse_utc);
    match (reset, screen_at) {
        (None, _) => (sighting == Sighting::New).then_some(p),
        (Some(_), None) => None,
        (Some(r), Some(s)) if r <= now => (s >= r).then_some(p),
        (Some(r), Some(s)) if s < r - window_len => None,
        (Some(_), Some(_)) => match stored {
            Some(w) if w.used_pct >= p => None,
            _ => Some(p),
        },
    }
}

/// app-server 讀數會落後 CLI 一整輪（2026-09-13 使用者截圖：量表 5h 100、pane 90% left），CLI 狀態列才是它當下擋你的
/// 依據。與 app-server 共用同一格；什麼時候採用 pane 上的數字見 [`pane_window_used`]。
pub async fn refresh_codex_from_panes(app: &Arc<App>, host: &str) -> usize {
    let rows: Vec<(String, Option<String>, Option<String>)> = match sqlx::query_as(
        // 最近有動靜的 pane 排前面：它的狀態列最新。閒著的 pane 也會刷新，但剛跑完回合的那顆最準。
        // 第三欄是那顆 pane 畫面的年紀：最後一回合結束（或開始）的時間，沒有回合就是 run 起來的時間。
        // run 實際的身分（issue #238）：記了就用它，沒記才用 bot 設定的。
        "SELECT r.pane_id, CASE WHEN r.runtime_identity IS NULL THEN b.identity ELSE NULLIF(TRIM(r.runtime_identity), '') END,
                COALESCE((SELECT MAX(COALESCE(t.completed_at, t.created_at)) FROM turns t WHERE t.run_id = r.id), r.started_at)
           FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id
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
    for (pane_id, identity, screen_at) in rows {
        let base = quota_base_for_host(app, host, "codex", identity.as_deref()).await;
        // 同一個身分讀到一次就夠——但要「讀到」才算：那顆 pane 正在壓縮對話、捲動中讀不到狀態列時，
        // 換同帳號的下一顆，而不是整個帳號這輪都停在 app-server 落後的數字（2026-09-15）。
        if seen.contains(&base) {
            continue;
        }
        let Some(client) = app.herdr_for(host).await else { continue };
        let Ok(read) = client.pane_read(&pane_id, "visible", 60).await else { continue };
        let Some(parsed) = crate::codex_live::parse_status_quota(&read.text) else { continue };
        seen.insert(base.clone());
        let reading = format!("{:?}/{:?}", parsed.five_hour_left, parsed.weekly_left);
        let sighting = status_line_sighting(host, &pane_id, &reading).await;
        let stored = app.quotas.lock().await.get(&quota_key(host, &base)).cloned();
        let screen_at = screen_at.as_deref().and_then(parse_utc);
        let now = chrono::Utc::now();
        let used = |left: Option<f64>| left.map(|l| (100.0 - l).clamp(0.0, 100.0));
        let five = pane_window_used(used(parsed.five_hour_left), stored.as_ref().and_then(|q| q.five_hour.as_ref()), chrono::Duration::hours(5), screen_at, sighting, now);
        let weekly = pane_window_used(used(parsed.weekly_left), stored.as_ref().and_then(|q| q.seven_day.as_ref()), chrono::Duration::days(7), screen_at, sighting, now);
        let status = crate::codex_live::CodexStatusQuota { five_hour_left: five.map(|u| 100.0 - u), weekly_left: weekly.map(|u| 100.0 - u) };
        let Some(q) = quota_from_codex_status(&status, identity.as_deref()) else { continue };
        set(app, host, &base, q).await;
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
            for_each_host(pollable_hosts(&app).await, |host| {
                let app = app.clone();
                async move {
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
            })
            .await;
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

    /// M5（review 2026-09-16）：pane 剛跑完回合說 5h 90% left，之後 app-server 寫進落後的「0% 已用」。pane 閒著、畫面沒變，
    /// 以前就再也沒人蓋回去；現在畫面屬於同一個窗，就把用量補回來（只增不減）。
    /// 反過來，畫面比現在這個窗還舊（重置之前那一回合）就不採用——那是 2026-09-16 量表在滿與見底之間跳的原因。
    #[tokio::test]
    async fn an_idle_codex_pane_corrects_a_lagging_server_but_not_a_newer_window() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES (?,?,'cx','codex','[]',0,1,'tok','user',?)",
        )
        .bind(&bot)
        .bind(&env.project_id)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run = crate::testing::fake_run(&app, &bot).await;
        let now = chrono::Utc::now();
        let turn_done = |ago: chrono::Duration| {
            let app = app.clone();
            let (run, bot) = (run.clone(), bot.clone());
            async move {
                sqlx::query("DELETE FROM turns WHERE run_id=?").bind(&run).execute(&app.db).await.unwrap();
                let conv = crate::db::conversation_id(&app.db, &bot).await.unwrap();
                let t = iso(now - ago);
                sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, created_at, completed_at) VALUES (?,?,?,'web','completed',?,?)")
                    .bind(crate::db::ulid())
                    .bind(conv)
                    .bind(&run)
                    .bind(&t)
                    .bind(&t)
                    .execute(&app.db)
                    .await
                    .unwrap();
            }
        };
        let pane = crate::db::run(&app.db, &run).await.unwrap().unwrap().pane_id.unwrap();
        let screen = |left: u32| format!("\n› Ask Codex\n  gpt-6-astra low · /tmp · Context 20% used · 5h {left}% left · weekly 65% left\n");
        let server = |used: f64, resets: chrono::DateTime<chrono::Utc>| {
            let mut q = codex_q("codex-app-server", None);
            q.five_hour = Some(Window { used_pct: used, resets_at: Some(iso(resets)) });
            q.seven_day = Some(Window { used_pct: 35.0, resets_at: Some(iso(now + chrono::Duration::days(3))) });
            q
        };
        let five_used = |app: Arc<App>| async move { app.quotas.lock().await.get("codex").unwrap().five_hour.clone().unwrap().used_pct };

        // 窗 1 小時前開始；回合 10 分鐘前跑完，pane 說 90% left。
        turn_done(chrono::Duration::minutes(10)).await;
        set(&app, LOCAL_HOST, "codex", server(0.0, now + chrono::Duration::hours(4))).await;
        env.herdr.screens.lock().unwrap().insert(pane.clone(), screen(90));
        refresh_codex_from_panes(&app, LOCAL_HOST).await;
        assert_eq!(five_used(app.clone()).await, 10.0);
        // app-server 落後，又寫回 0%；pane 閒著、畫面沒變——同一個窗，照樣補回來。
        set(&app, LOCAL_HOST, "codex", server(0.0, now + chrono::Duration::hours(4))).await;
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 1);
        assert_eq!(five_used(app.clone()).await, 10.0, "落後的 app-server 不能讓量表停在偏滿");
        // 已經是一樣的數字就不重寫（`updated_at` 不被刷成「剛剛」）。
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 0);

        // 同一張畫面，但回合是 3 小時前跑的，而 app-server 說窗 1 小時前才重置：畫面屬於上一個窗，不採用。
        turn_done(chrono::Duration::hours(3)).await;
        set(&app, LOCAL_HOST, "codex", server(5.0, now + chrono::Duration::hours(4))).await;
        assert_eq!(refresh_codex_from_panes(&app, LOCAL_HOST).await, 0);
        assert_eq!(five_used(app.clone()).await, 5.0, "重置之前的畫面不能把量表蓋回去");
    }

    #[test]
    fn a_pane_window_is_used_only_when_it_belongs_to_the_current_window() {
        let now = chrono::Utc::now();
        let h = chrono::Duration::hours;
        let len = h(5);
        let w = |used: f64, resets: chrono::DateTime<chrono::Utc>| Window { used_pct: used, resets_at: Some(iso(resets)) };
        let cur = w(20.0, now + h(4)); // 窗從 1 小時前開始
        // 剛看著它變：照寫，連比較小的數字也寫（CLI 當下的說法，例如用了重置券）。
        assert_eq!(pane_window_used(Some(5.0), Some(&cur), len, Some(now), Sighting::Changed, now), Some(5.0));
        // 同一個窗：只增不減。
        assert_eq!(pane_window_used(Some(30.0), Some(&cur), len, Some(now - h(0)), Sighting::Same, now), Some(30.0));
        assert_eq!(pane_window_used(Some(10.0), Some(&cur), len, Some(now), Sighting::Same, now), None);
        // 畫面比這個窗還舊：不管 New 還是 Same 都不採用。
        assert_eq!(pane_window_used(Some(90.0), Some(&cur), len, Some(now - h(2)), Sighting::New, now), None);
        // 記著的窗已經過了重置：畫面要晚於那次重置才採用。
        let ended = w(90.0, now - h(1));
        assert_eq!(pane_window_used(Some(3.0), Some(&ended), len, Some(now - h(2)), Sighting::Same, now), None);
        assert_eq!(pane_window_used(Some(3.0), Some(&ended), len, Some(now - chrono::Duration::minutes(30)), Sighting::Same, now), Some(3.0));
        // 沒有重置時間：只收這個行程第一次看到的畫面。
        let bare = Window { used_pct: 20.0, resets_at: None };
        assert_eq!(pane_window_used(Some(7.0), Some(&bare), len, Some(now), Sighting::New, now), Some(7.0));
        assert_eq!(pane_window_used(Some(7.0), Some(&bare), len, Some(now), Sighting::Same, now), None);
        assert_eq!(pane_window_used(None, Some(&cur), len, Some(now), Sighting::Changed, now), None);
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
    /// 回歸（2026-09-16）：重啟那一秒身分還沒偵測完，cc0 的讀數先落在 `claude:cc0`；偵測完之後寫裸 `claude` 時，
    /// 那一格要清掉。有自己帳號目錄的 cc1、查不到的身分都照舊保留，遠端主機的同名 key 不受本機影響。
    #[tokio::test]
    async fn a_split_key_left_from_before_identities_were_known_is_dropped_once_they_are() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let reading = |pct: f64| {
            let mut q = codex_q("statusline", None);
            q.five_hour = Some(Window { used_pct: pct, resets_at: None });
            q
        };
        // 身分還沒偵測到：cc0 寧可分開。
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "claude", Some("cc0")).await, "claude:cc0");
        set(&app, LOCAL_HOST, "claude:cc0", reading(1.0)).await;
        set(&app, LOCAL_HOST, "claude:cc1", reading(40.0)).await;
        set(&app, LOCAL_HOST, "claude:nobody", reading(50.0)).await;
        set(&app, "m4p", "claude:cc0", reading(60.0)).await;

        let ident = |name: &str, pairs: &[(&str, &str)]| crate::config::IdentityCfg {
            name: name.into(),
            kind: "claude".into(),
            host: None,
            env: env(pairs),
            args: vec![],
        };
        app.tools.lock().await.insert(
            LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![ident("cc0", &[]), ident("cc1", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc1")])],
                checked_at: crate::db::now(),
            },
        );
        // 偵測完：cc0 收斂到裸 key。下一筆讀數寫裸 key 的同時把殘留的那一格清掉。
        assert_eq!(quota_base_for_host(&app, LOCAL_HOST, "claude", Some("cc0")).await, "claude");
        set(&app, LOCAL_HOST, "claude", reading(25.0)).await;
        let q = app.quotas.lock().await;
        assert!(q.get("claude:cc0").is_none(), "殘留的分開那格清掉");
        assert_eq!(q.get("claude").and_then(|x| x.five_hour.as_ref()).map(|w| w.used_pct), Some(25.0));
        assert!(q.get("claude:cc1").is_some(), "有自己帳號目錄的照舊分開");
        assert!(q.get("claude:nobody").is_some(), "查不到的身分寧可保留");
        assert!(q.get(&quota_key("m4p", "claude:cc0")).is_some(), "別台主機的不受影響");
    }

    #[tokio::test]
    async fn codex_bots_on_cc1_share_the_bare_key_and_cc2_keeps_its_own() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let ident = |name: &str, pairs: &[(&str, &str)]| crate::config::IdentityCfg {
            name: name.into(),
            kind: "claude".into(),
            host: None,
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
                        host: None,
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
            instruction_files: None,
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

    /// SPEC §14.3：同輪各主機併發。兩台都要等對方到齊才放行——串列跑就會卡到逾時。
    #[tokio::test]
    async fn hosts_are_polled_at_the_same_time() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let run = for_each_host(vec!["local".into(), "m4p".into()], |_host| {
            let (barrier, done) = (barrier.clone(), done.clone());
            async move {
                barrier.wait().await;
                done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), run).await.expect("一台一台跑會卡在 barrier");
        assert_eq!(done.load(std::sync::atomic::Ordering::SeqCst), 2, "全部跑完才回來");
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

    /// review3 c3 H2：Fable 桶只擋跑 Fable 的 bot；5h／7d／沒有桶名的擋整個帳號；不知道 bot 在跑什麼模型時保守地擋。
    #[test]
    fn a_model_bucket_only_blocks_bots_on_that_model() {
        for (bucket, model, blocks) in [
            (Some("fable"), Some("fable"), true),
            (Some("fable"), Some("claude-fable-5-1"), true),
            (Some("fable"), Some("Fable 5.1"), true),
            (Some("fable"), Some("opus"), false),
            (Some("fable"), Some("opus[1m]"), false),
            (Some("fable"), Some("sonnet"), false),
            (Some("fable"), None, true),
            (Some("fable"), Some("default"), true),
            // Opus／Sonnet 的週桶同理（review3 c4 M1）。
            (Some("opus"), Some("opus"), true),
            (Some("opus"), Some("claude-opus-5"), true),
            (Some("opus"), Some("fable"), false),
            (Some("opus"), Some("sonnet"), false),
            (Some("sonnet"), Some("sonnet"), true),
            (Some("sonnet"), Some("opus"), false),
            (Some("opus"), None, true),
            (Some("five_hour"), Some("opus"), true),
            (Some("seven_day"), Some("opus"), true),
            (None, Some("opus"), true),
            (None, Some("gpt-5.6-sol"), true),
        ] {
            assert_eq!(bucket_blocks_model(bucket, model), blocks, "{bucket:?} × {model:?}");
        }
    }

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

    fn iso(t: chrono::DateTime<chrono::Utc>) -> String {
        // 跟生產端同一支：格式只有一種（issue #101）。
        crate::db::iso_at(t)
    }

    /// M2（review 2026-09-16）：Fable 撞限時那一桶還沒讀數，保底 7 天；之後 `/usage` 的真讀數要能把它縮短，
    /// 窗重置之後的讀數要能把它清掉。claude 沒有成功回合清撞限這條路，`until` 是唯一出口。
    #[tokio::test]
    async fn a_claude_hit_is_corrected_by_later_readings_of_its_own_bucket() {
        let app = crate::testing::env().await.app.clone();
        let now = chrono::Utc::now();
        let at = now - chrono::Duration::hours(1);
        let hit = LimitHit {
            message: "You've reached your Fable limit".into(),
            until: Some(iso(at + chrono::Duration::days(7))),
            at: iso(at),
            bucket: Some("fable".into()),
        };
        let mut banner = codex_q("claude-limit-hit", Some(hit));
        banner.five_hour = None;
        banner.seven_day = None;
        set(&app, LOCAL_HOST, "claude:cc1", banner).await;
        let until = |app: Arc<App>| async move {
            app.quotas.lock().await.get("claude:cc1").unwrap().limit_hit.as_ref().map(|h| h.until.clone().unwrap())
        };

        // 撞限後讀到的 Fable 窗：還見底，明天 08:00 重置 → 撞限最晚到那時，不是下週。
        let tomorrow = now + chrono::Duration::hours(20);
        let mut usage = codex_q("claude-usage", None);
        usage.fable = Some(Window { used_pct: 100.0, resets_at: Some(iso(tomorrow)) });
        set(&app, LOCAL_HOST, "claude:cc1", usage.clone()).await;
        assert_eq!(until(app.clone()).await, Some(iso(tomorrow)), "保底的 7 天要被那一桶自己的重置時間截短");

        // 不相干的桶（statusLine 只有 5h／7d）不算那一桶的讀數。
        let mut status = codex_q("statusline", None);
        status.five_hour = Some(Window { used_pct: 3.0, resets_at: Some(iso(now + chrono::Duration::hours(4))) });
        set(&app, LOCAL_HOST, "claude:cc1", status).await;
        assert_eq!(until(app.clone()).await, Some(iso(tomorrow)));

        // 重置之後的讀數：窗的起點在撞限之後 → 撞限作廢。
        let mut after = codex_q("claude-usage", None);
        after.fable = Some(Window { used_pct: 0.0, resets_at: Some(iso(at + chrono::Duration::days(7) + chrono::Duration::minutes(1))) });
        set(&app, LOCAL_HOST, "claude:cc1", after).await;
        assert_eq!(until(app.clone()).await, None, "那一桶重置過了，撞限不能再擋");
    }

    /// 撞限前就開始、撞限後才回來的讀數（百分比可能還沒到頂）不能把撞限清掉，只能截短時間。
    /// 沒有桶名的撞限（codex credits 用完、開機回填）完全不動。
    #[test]
    fn a_reading_from_before_the_hit_only_shortens_it_and_a_bucketless_hit_is_left_alone() {
        let now = chrono::Utc::now();
        let at = now - chrono::Duration::minutes(10);
        let hit = |bucket: Option<&str>| LimitHit {
            message: "You've hit your session limit".into(),
            until: Some(iso(at + chrono::Duration::hours(5))),
            at: iso(at),
            bucket: bucket.map(String::from),
        };
        let mut reading = codex_q("statusline", None);
        reading.five_hour = Some(Window { used_pct: 94.0, resets_at: Some(iso(now + chrono::Duration::minutes(20))) });
        let got = recalibrate_limit_hit(hit(Some("five_hour")), &reading).expect("窗在撞限之前就開了：還在擋");
        assert_eq!(got.until, Some(iso(now + chrono::Duration::minutes(20))));
        assert_eq!(recalibrate_limit_hit(hit(None), &reading), Some(hit(None)), "沒有桶名就不猜");
        let mut later = reading.clone();
        later.five_hour.as_mut().unwrap().resets_at = Some(iso(at + chrono::Duration::hours(6)));
        assert_eq!(recalibrate_limit_hit(hit(Some("five_hour")), &later), None);
    }

    /// #236：窗在撞限**之前**就結束的讀數（閒置的 5h 窗：`/usage` 照樣回上一個重置時間）說不出這次撞限的事——
    /// 拿它取 `min` 會把 `until` 拉到過去、撞限當場作廢，派工照送。撞限原樣留著，經過 `set` 也一樣。
    #[tokio::test]
    async fn a_reading_of_a_window_that_ended_before_the_hit_leaves_it_alone() {
        let now = chrono::Utc::now();
        let at = now - chrono::Duration::minutes(10);
        let hit = LimitHit {
            message: "You've hit your session limit".into(),
            until: Some(iso(at + chrono::Duration::hours(5))),
            at: iso(at),
            bucket: Some("five_hour".into()),
        };
        let mut idle = codex_q("claude-usage", None);
        idle.five_hour = Some(Window { used_pct: 0.0, resets_at: Some(iso(now - chrono::Duration::hours(1))) });
        assert_eq!(recalibrate_limit_hit(hit.clone(), &idle), Some(hit.clone()), "上一個窗的讀數不動它");

        let app = crate::testing::env().await.app.clone();
        let mut banner = codex_q("claude-limit-hit", Some(hit.clone()));
        banner.five_hour = None;
        banner.seven_day = None;
        set(&app, LOCAL_HOST, "claude:cc1", banner).await;
        set(&app, LOCAL_HOST, "claude:cc1", idle).await;
        let got = app.quotas.lock().await.get("claude:cc1").unwrap().limit_hit.clone();
        assert_eq!(got, Some(hit), "經過 set 也還擋著");
    }

    /// #238：run 在跑的時候改身分（PATCH 回 `needs_restart`）：按「重啟」之前 pane 裡還是**起來時的帳號**。額度讀數、撞限、
    /// 閘門、成功回合清撞限都要記在那個身分上；以前一律看 `bots.identity`（已經是新的）——舊帳號用盡記到新帳號名下，
    /// 新帳號被誤擋、舊帳號的其他 bot 照樣被派工。重啟之後才換成新身分。
    #[tokio::test]
    async fn a_run_bills_the_identity_it_started_with_until_it_is_restarted() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        for name in ["cc1", "cc2"] {
            let dir = env.dir.join(format!("claude-{name}")).to_string_lossy().into_owned();
            app.cfg
                .update(move |c| {
                    c.identities.push(crate::config::IdentityCfg {
                        name: name.into(),
                        kind: "claude".into(),
                        host: None,
                        env: [("CLAUDE_CONFIG_DIR".to_string(), dir)].into(),
                        args: vec![],
                    });
                    Ok(())
                })
                .await
                .unwrap();
        }
        let bot = crate::testing::claude_bot(&app, &env.project_id, "switcher").await;
        sqlx::query("UPDATE bots SET identity='cc1' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        crate::lifecycle::start_bot(&app, &bot.id).await.unwrap();
        let run = crate::db::active_run(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(run.started_identity(), Some(Some("cc1".to_string())), "起來時的身分蓋在 run 上");

        // 使用者把身分改成 cc2、還沒重啟：pane 還是 cc1 的帳號。
        sqlx::query("UPDATE bots SET identity='cc2' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        let status = crate::hookrecv::HookBody {
            bot_id: bot.id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "StatusLine", "rate_limits": {"five_hour": {"used_percentage": 42.0, "resets_at": (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp()}}}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &status).await.unwrap();
        {
            let q = app.quotas.lock().await;
            let keys: Vec<String> = q.keys().cloned().collect();
            assert_eq!(q.get("claude:cc1").and_then(|x| x.five_hour.as_ref()).map(|w| w.used_pct), Some(42.0), "讀數記在 cc1：{keys:?}");
            assert!(q.get("claude:cc2").is_none(), "新身分那一格沒被寫：{keys:?}");
        }

        crate::turn_error::mark_claude_limit_hit(&app, &bot, "You've hit your session limit · resets 5pm").await.unwrap();
        let hits: Vec<String> = app.quotas.lock().await.iter().filter(|(_, q)| q.limit_hit.is_some()).map(|(k, _)| k.clone()).collect();
        assert_eq!(hits, vec!["claude:cc1".to_string()], "撞限記在實際撞到的帳號");
        assert!(try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "閘門照 cc1 擋：排著的會送進 cc1 的行程");
        clear_limit_hit_for_bot(&app, &bot).await;
        assert!(app.quotas.lock().await["claude:cc1"].limit_hit.is_none(), "成功回合清的是 cc1");

        // 重啟之後才是 cc2：cc1 的撞限不再擋它，cc2 的才擋。
        crate::turn_error::mark_claude_limit_hit(&app, &bot, "You've hit your session limit · resets 5pm").await.unwrap();
        crate::lifecycle::stop_bot(&app, &bot.id).await.unwrap();
        crate::lifecycle::start_bot(&app, &bot.id).await.unwrap();
        let run = crate::db::active_run(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(run.started_identity(), Some(Some("cc2".to_string())));
        assert!(try_limit_hit_for_bot(&app, &bot).await.unwrap().is_none(), "重啟成 cc2：cc1 的撞限不擋它");
        assert_eq!(billing_identity(&app, &bot).await.unwrap().as_deref(), Some("cc2"));
    }

    /// run 沒記身分（不是 daemon 起的、升級前的舊列）照 bot 設定的；記了空字串＝起來時沒有身分（預設帳號），就算之後設了身分也一樣。
    #[tokio::test]
    async fn a_run_without_a_recorded_identity_falls_back_to_the_bots() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "fallback").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let case = |bot_identity: Option<&'static str>, recorded: Option<&'static str>| {
            let (app, bot_id, run_id) = (app.clone(), bot.id.clone(), run_id.clone());
            async move {
                sqlx::query("UPDATE bots SET identity=? WHERE id=?").bind(bot_identity).bind(&bot_id).execute(&app.db).await.unwrap();
                sqlx::query("UPDATE runs SET runtime_identity=? WHERE id=?").bind(recorded).bind(&run_id).execute(&app.db).await.unwrap();
                let bot = crate::db::bot(&app.db, &bot_id).await.unwrap().unwrap();
                billing_identity(&app, &bot).await.unwrap()
            }
        };
        assert_eq!(case(Some("cc2"), None).await.as_deref(), Some("cc2"), "run 沒記：照 bot 設定的");
        assert_eq!(case(Some("cc2"), Some("cc1")).await.as_deref(), Some("cc1"), "run 記了就用它");
        assert_eq!(case(Some("cc2"), Some("")).await, None, "起來時沒有身分（預設帳號）");
        assert_eq!(case(None, Some(" cc1 ")).await.as_deref(), Some("cc1"));
        assert_eq!(case(Some("  "), None).await, None);
        crate::lifecycle::stop_bot(&app, &bot.id).await.ok();
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET identity='cc2' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(billing_identity(&app, &bot).await.unwrap().as_deref(), Some("cc2"), "沒有 run：照 bot 設定的");
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

    /// issue #108：排著的 prompt 記下的撞限重啟後原樣種回——撞限時刻、桶名、沒寫時間的黏著都留著；
    /// 回填之前已經進來的讀數當場校正；過期的、這一格已有更晚（或黏著）的不寫。
    #[tokio::test]
    async fn a_restored_limit_hit_keeps_its_moment_and_bucket_and_meets_the_reading_already_in() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let t = |mins: i64| crate::db::iso_at(chrono::Utc::now() + chrono::Duration::minutes(mins));
        let hit = |at: &str, until: Option<String>, bucket: Option<&str>| LimitHit {
            message: "You've hit your session limit".into(),
            until,
            at: at.into(),
            bucket: bucket.map(String::from),
        };
        let got = |key: &'static str| {
            let app = app.clone();
            async move { app.quotas.lock().await.get(key).and_then(|q| q.limit_hit.clone()) }
        };

        let original = hit(&t(-90), Some(t(120)), Some("five_hour"));
        assert!(restore_limit_hit(&app, LOCAL_HOST, "claude:r1", original.clone()).await);
        assert_eq!(got("claude:r1").await, Some(original.clone()), "原樣，不是「現在」撞的");
        assert_eq!(app.quotas.lock().await["claude:r1"].source, HELD_SOURCE);
        assert!(!restore_limit_hit(&app, LOCAL_HOST, "claude:r1", hit(&t(-90), Some(t(60)), None)).await, "已有更晚的：不蓋");
        assert!(restore_limit_hit(&app, LOCAL_HOST, "claude:r1", hit(&t(-90), None, None)).await, "黏著的比任何時間都晚");
        assert!(!restore_limit_hit(&app, LOCAL_HOST, "claude:r1", hit(&t(-90), Some(t(600)), None)).await, "已經黏著：不蓋");
        assert!(!restore_limit_hit(&app, LOCAL_HOST, "claude:r2", hit(&t(-90), Some(t(-1)), None)).await, "過期的不種");

        // 重啟後、回填前就進來的讀數：5 小時窗是撞限之後才開的——撞限作廢，不種。
        let reading = |resets: String| Quota {
            five_hour: Some(Window { used_pct: 3.0, resets_at: Some(resets) }),
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "statusline".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:r3", reading(t(5 * 60 - 1))).await;
        assert!(!restore_limit_hit(&app, LOCAL_HOST, "claude:r3", hit(&t(-90), Some(t(120)), Some("five_hour"))).await);
        assert_eq!(got("claude:r3").await, None);
        // 窗在撞限之前就開了：照種，但到期時間收斂到那個窗的重置。
        let resets = t(30);
        set(&app, LOCAL_HOST, "claude:r4", reading(resets.clone())).await;
        assert!(restore_limit_hit(&app, LOCAL_HOST, "claude:r4", hit(&t(-90), Some(t(120)), Some("five_hour"))).await);
        assert_eq!(got("claude:r4").await.and_then(|h| h.until), Some(resets));
        assert_eq!(app.quotas.lock().await["claude:r4"].five_hour.as_ref().map(|w| w.used_pct), Some(3.0), "讀數不動");
    }

    /// 一顆遠端 bot（`remote1`），它自己的 key 沒有撞限；本機 `claude` 是另一個帳號，有撞限、有讀數。
    async fn remote_bot_beside_a_local_hit(app: &Arc<App>) -> crate::db::Bot {
        let pid = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'remote1', ?)")
            .bind(&pid)
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let bot = crate::testing::claude_bot(app, &pid, "far").await;
        let later = |h: i64| crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(h));
        let mut local = codex_q("statusline", None);
        local.five_hour = Some(Window { used_pct: 40.0, resets_at: Some(later(1)) });
        set(app, LOCAL_HOST, "claude", local).await;
        assert!(seed_limit_hit(app, LOCAL_HOST, "claude", &later(2), "You've hit your session limit", Some("five_hour".into())).await);
        bot
    }

    async fn projects_unreadable(app: &Arc<App>, unreadable: bool) {
        let sql = if unreadable { "ALTER TABLE projects RENAME TO projects_unreadable" } else { "ALTER TABLE projects_unreadable RENAME TO projects" };
        sqlx::query(sql).execute(&app.db).await.unwrap();
    }

    /// #108 重開：讀不到 bot 在哪台主機不等於它在本機。以前四支都退回 `local`：查本機帳號的撞限回「沒撞限」、拿本機的
    /// 重置時間把重送提早、把**本機**帳號真的撞限清掉、拿本機的成功回合當放行證據。
    #[tokio::test]
    async fn a_bot_whose_host_cannot_be_read_is_never_read_or_cleared_on_the_local_key() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let bot = remote_bot_beside_a_local_hit(&app).await;
        let since = chrono::Utc::now() - chrono::Duration::seconds(1);

        projects_unreadable(&app, true).await;
        assert!(try_limit_hit_for_bot(&app, &bot).await.is_err(), "讀不到主機是錯，不是「沒撞限」");
        assert!(limit_hit_for_bot(&app, &bot).await.is_none(), "舊介面（supervisor 用）讀不到記 warn，不拿本機的撞限頂替");
        assert_eq!(next_reset_for_bot(&app, &bot).await, None, "不拿本機帳號的重置時間");
        clear_limit_hit_for_bot(&app, &bot).await;
        assert!(app.quotas.lock().await["claude"].limit_hit.is_some(), "本機帳號的撞限沒被別台 bot 的成功回合清掉");
        clear_limit_hit(&app, LOCAL_HOST, "claude").await; // 本機帳號自己答完一回合
        assert!(!limit_cleared_since(&app, &bot, since).await, "本機的成功回合不是遠端這顆的放行證據");

        projects_unreadable(&app, false).await;
        assert_eq!(try_limit_hit_for_bot(&app, &bot).await.unwrap(), None, "讀得到：看自己那把 `remote1/claude`");
        assert!(!limit_cleared_since(&app, &bot, since).await);
    }

    /// 讀不到 run 就不知道它在跑什麼模型：照擋（`None`），不退回設定值——`/model` 換成 fable 的 bot 撞 Fable 桶，
    /// 設定值還寫 opus，退回設定值就放行了。
    #[tokio::test]
    async fn a_model_bucket_hit_holds_a_bot_whose_running_model_cannot_be_read() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        let bot = crate::testing::claude_bot(&app, &env_.project_id, "switched").await;
        sqlx::query("UPDATE bots SET model='opus' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        let run = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET runtime_model='fable' WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        let until = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(2));
        assert!(seed_limit_hit(&app, LOCAL_HOST, "claude", &until, "You've hit your Fable limit", Some("fable".into())).await);
        assert!(try_limit_hit_for_bot(&app, &bot).await.unwrap().is_some(), "前提：它實際在跑 fable");

        sqlx::query("ALTER TABLE runs RENAME TO runs_unreadable").execute(&app.db).await.unwrap();
        assert_eq!(running_model(&app, &bot).await, None, "不知道，不是設定值的 opus");
        // 讀不到 run 連它用哪個身分起來都不知道（#238）：回錯，呼叫端照擋（hold）。無論如何不能是「沒撞限」。
        let got = try_limit_hit_for_bot(&app, &bot).await;
        assert!(!matches!(got, Ok(None)), "不知道在跑什麼：照擋，不是沒撞限：{got:?}");
        sqlx::query("ALTER TABLE runs_unreadable RENAME TO runs").execute(&app.db).await.unwrap();
    }

    /// 寫撞限要落在查詢端之後讀的那把 key：身分表還沒偵測完、又不是手寫的身分時算不準，回錯（不猜 `claude:cc0`）。
    #[tokio::test]
    async fn the_limit_key_is_not_guessed_before_the_identities_are_known() {
        let env_ = crate::testing::env().await;
        let app = env_.app.clone();
        assert!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("cc0")).await.is_err(), "偵測之前：cc0 是不是預設帳號還不知道");
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", None).await.unwrap(), "claude", "沒有身分就是裸 kind");
        app.cfg
            .update(|c| {
                c.identities.push(crate::config::IdentityCfg { name: "hand".into(), kind: "claude".into(), host: None, env: env(&[("CLAUDE_CONFIG_DIR", "/x")]), args: vec![] });
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("hand")).await.unwrap(), "claude:hand", "手寫的身分不必等偵測");
        let cc0 = crate::config::IdentityCfg { name: "cc0".into(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] };
        app.tools.lock().await.insert(
            LOCAL_HOST.to_string(),
            crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![cc0], checked_at: crate::db::now() },
        );
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("cc0")).await.unwrap(), "claude", "偵測完：cc0 就是預設帳號");
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("nobody")).await.unwrap(), "claude:nobody", "偵測完還查不到：照舊分開");
    }
}
