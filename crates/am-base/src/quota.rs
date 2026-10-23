//! Rate-limit quota per host + kind (`GET /api/quota`, WS `quota_updated`); sources: codex
//! app-server, claude statusLine + [`crate::quota_claude`] probe, grok [`crate::quota_grok`].
//! Keys are host-scoped (SPEC §14): bare on local, `<host>/…` remote — a remote bot's statusLine
//! must never land on the local row.

use crate::config::LOCAL_HOST;
use anyhow::Result;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Weak};
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

/// A quota snapshot and its cache write must follow the same per-key order as the in-memory
/// mutation. Keep the lock map weak so identities that disappear do not stay in memory.
async fn cache_write_lock(key: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        std::sync::OnceLock::new();
    let lock = {
        let mut locks = LOCKS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new())).lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        match locks.get(key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key.to_string(), Arc::downgrade(&lock));
                lock
            }
        }
    };
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
pub async fn quota_base_for_host(app: &impl HostIdentities, host: &str, kind: &str, identity: Option<&str>) -> String {
    let Some(idn) = identity.map(str::trim).filter(|s| !s.is_empty()) else { return kind.to_string() };
    let found = app.identity_for_host(host, idn).await;
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
pub async fn resolve_quota_base(app: &impl HostIdentities, host: &str, kind: &str, identity: Option<&str>) -> Result<String> {
    if let Some(idn) = identity.map(str::trim).filter(|s| !s.is_empty()) {
        if app.identity_for_host(host, idn).await.is_none() && !app.host_tools_detected(host).await {
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
pub async fn billing_identity(app: &impl crate::capabilities::Db, bot: &crate::db::Bot) -> Result<Option<String>> {
    let run = crate::db::active_run(app.db(), &bot.id).await?;
    Ok(identity_for_run(bot, run.as_ref()))
}

/// 查詢端：這顆 bot 的讀數在哪幾把 key。先查它自己那一把（收斂規則同寫入端），**只有**收斂到裸 kind
/// 的身分才會落在裸 key——有自己 home 的身分（cc2 帶 `CODEX_HOME`）不借預設帳號的數字。身分是 [`billing_identity`]。
pub async fn keys_for_bot(app: &impl HostIdentities, host: &str, bot: &crate::db::Bot, identity: Option<&str>) -> Vec<String> {
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
pub async fn running_model(app: &impl crate::capabilities::Db, bot: &crate::db::Bot) -> Option<String> {
    let runtime = match crate::db::active_run(app.db(), &bot.id).await {
        Ok(run) => run.and_then(|r| r.runtime_model),
        Err(e) => {
            tracing::warn!(bot = %bot.id, error = %e, "cannot read the running model; treating it as unknown");
            return None;
        }
    };
    runtime.or_else(|| bot.model.clone()).filter(|m| !m.trim().is_empty())
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

pub async fn pollable_hosts(app: &impl crate::hosts::HostsAccess) -> Vec<String> {
    app.hosts()
        .list()
        .await
        .into_iter()
        .filter(|c| c.is_local() || c.is_connected())
        .map(|c| c.name.clone())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Window {
    pub used_pct: f64,
    pub resets_at: Option<String>,
    /// 這一桶最後一次**真的出現在新讀數裡**的時間（issue #475，i267 review）。
    ///
    /// 不能用 `Quota::updated_at` 代替：那是**整筆**讀數最後寫入的時間，而 [`set`] 會刻意沿用新讀數
    /// 缺的窗（statusline 被截斷只剩 7d 時把舊的 5h 原樣搬過來），`updated_at` 卻蓋成現在。
    /// statusline 每幾秒進來一次，於是被沿用的那一桶年齡永遠是 0，「比窗長還舊」永遠不成立——
    /// #475 想修的「同一次 uptime 內永久排除」在那條路上等於沒修到。
    ///
    /// 沿用時原樣保留，只有那一桶真的在新讀數裡才更新。`seed_limit_hit`／`restore_limit_hit` 只改
    /// `limit_hit` 與 `updated_at`（它們繞過 [`set`]，直接改 map 裡那一筆），窗是原本那幾個，
    /// 所以 `observed_at` 自然留著——年齡不會被一次撞限記錄重設。
    ///
    /// `None` = 不知道（這個欄位出現以前寫的快取列），這時退回 `updated_at`。那種列在下一次真讀數
    /// 進來（走 [`set`]）就會補上。
    #[serde(default)]
    pub observed_at: Option<String>,
}

/// 各桶的窗長。`recalibrate_limit_hit` 與 pane 讀數也用同一組數字。
pub const FIVE_HOUR_LEN: chrono::Duration = chrono::Duration::hours(5);
pub const SEVEN_DAY_LEN: chrono::Duration = chrono::Duration::days(7);

/// 哪一桶。窗長跟著桶走——`Window` 自己不知道它是 5h 還是 7d，所以「這筆讀數有沒有比窗長還舊」
/// 只能由拿著 [`Quota`] 的那一端判（issue #475）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    FiveHour,
    SevenDay,
    Fable,
}

impl Bucket {
    pub fn len(self) -> chrono::Duration {
        match self {
            Bucket::FiveHour => FIVE_HOUR_LEN,
            // Fable 跟 7d 同一個週期重置。
            Bucket::SevenDay | Bucket::Fable => SEVEN_DAY_LEN,
        }
    }
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

    /// 網頁的「偏低」（黃燈）看速度，不是只看剩幾 %（使用者 2026-10-14：cc1 的 7d 剩 16% 就黃，其實 10 小時後就重置）。
    /// 剩餘 % 低於 [`LOW_REMAINING_PCT`] **而且**比「離重置還有的時間佔窗長的比例」還少，才算偏低——
    /// 照整個窗口平均的速度用到重置都用不完，就不必提醒。不知道重置時間、或這筆讀數已經跨過重置，退回只看門檻。
    /// 見底（[`Self::critical`]）不看速度：剩不到 5% 隨時會撞限，跟重置早晚無關。
    pub fn low_paced(&self, len: chrono::Duration, now: chrono::DateTime<chrono::Utc>) -> bool {
        if !self.low() {
            return false;
        }
        if self.critical() || self.reset_passed(now) {
            return true;
        }
        let Some(resets) = self.resets_at.as_deref().and_then(parse_utc) else { return true };
        let left = (resets - now).num_seconds().clamp(0, len.num_seconds());
        let time_left_pct = left as f64 / len.num_seconds() as f64 * 100.0;
        self.remaining_pct() < time_left_pct
    }

    /// **這筆讀數跨過了重置**嗎（＝它比重置還舊，所以它說的百分比已經沒有意義）。
    ///
    /// 兩個條件都要成立：
    /// 1. `resets_at` 已經過去；
    /// 2. 這一桶的 `observed_at`（那一桶最後一次真的出現在新讀數裡的時間）**不晚於** `resets_at`。
    ///
    /// 第 2 條是 issue #489 補的。只看第 1 條會把一筆**剛剛讀到而且真的見底**的讀數判成「已重置」：
    /// codex 狀態列的讀數建構時一定沒有 `resets_at`（`quota_from_codex_status`），而 [`set`] 會沿用上一份的
    /// 重置時間（原本純粹為了顯示）。app-server 探測壞掉、只剩狀態列在進來時，那個繼承來的時刻早就過去了，
    /// 於是 97% 用掉的新讀數被當成有額度——正好跟 #464 想修的方向相反。觀測時間比重置新，就表示這筆讀數
    /// 已經反映了重置後的狀態；它說見底就是真的見底。
    ///
    /// **解不開的 `resets_at` 當成已經過去**，沿用 `supervisor::policy::past` 的先例（那裡的註解：
    /// 「An unreadable timestamp must not park the manager forever」）：一筆壞資料不該把一個身分永久排除。
    ///
    /// ⚠️ **這裡的 `true` 與 [`already_past`] 的 `true` 安全方向相反**（i264 review，#489）：
    /// 四處對「解不開」的約定都是「當成已過去」，但後果不一樣——`already_past` 回 `true` 是**丟棄**那個值
    /// （嚴格、安全），這支回 `true` 卻是**放行**（`exhausted_at` 變 `false`，身分看起來有額度）。
    /// 兩者現在相容，只因為壞值在生產路徑上到不了這裡：寫 `Window.resets_at` 的入口
    /// （claude `/usage`、claude statusline 的 `unix_to_rfc3339`、grok 的 `parse_reset`、快取載入）
    /// 都自己算出來或驗過格式。**要是有人新增一個不驗格式的入口，這一行就會變成「壞值＝有額度」。**
    ///
    /// **沒有 `resets_at` 就不算重置過**：那是「不知道」，不是「已經重置」。那種讀數由
    /// [`Quota::usable_window`] 用窗長收掉（issue #475）。
    ///
    /// `observed_at` 是 `None` 時退回只看第 1 條（＝#464 的行為）。**刻意不拿 `Quota::updated_at` 當備援**：
    /// 那是整筆讀數最後**寫入**的時間，不是那一桶被觀測的時間，兩者在沿用的情況下差很多；而且這個欄位
    /// 出現以前的測試與快取列都只是順手把 `updated_at` 填成「現在」，拿它當觀測時間會把那些資料重新解讀成
    /// 「觀測於重置之後」，連帶改掉 `policy` 對 Fable 的既有決定（那不是這張票的範圍）。
    /// 沒有 `observed_at` 的只有兩種來源：這個欄位出現以前寫的快取列（本來就是舊讀數，照 #464 放行是對的），
    /// 以及測試自己建的 `Window`。生產路徑寫進來的窗都會在 [`set`] 蓋上 `observed_at`。
    pub fn reset_passed(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        let Some(raw) = self.resets_at.as_deref() else { return false };
        let Some(resets) = parse_utc(raw) else { return true };
        if resets > now {
            return false;
        }
        match self.observed_at.as_deref().and_then(parse_utc) {
            // 觀測比重置新 → 這筆讀數已經是重置後的狀態，不算跨過重置（#489）。
            Some(observed) => observed <= resets,
            None => true,
        }
    }

    /// 見底**而且還沒重置**才算用盡（issue #464）。
    ///
    /// 三處共用這一份：`mission::pick`（選身分）、`supervisor::policy`（換模型）、
    /// `supervisor::responder`（協調者能不能答）。以前各寫一份，其中 `mission::pick` 那份連
    /// 「重置過沒有」都不看，另外兩份對解不開的時間戳又跟它相反（i407 review，#464）。
    pub fn exhausted_at(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        !self.reset_passed(now) && self.critical()
    }
}

/// 放在 [`Quota`] 裡的窗知道自己是哪一桶：`low` 用 [`Window::low_paced`]（看離重置多久）。
fn ser_bucket<S: Serializer>(w: &Option<Window>, len: chrono::Duration, s: S) -> Result<S::Ok, S::Error> {
    let Some(w) = w else { return s.serialize_none() };
    let mut st = s.serialize_struct("Window", 5)?;
    st.serialize_field("used_pct", &w.used_pct)?;
    st.serialize_field("resets_at", &w.resets_at)?;
    st.serialize_field("observed_at", &w.observed_at)?;
    st.serialize_field("low", &w.low_paced(len, chrono::Utc::now()))?;
    st.serialize_field("critical", &w.critical())?;
    st.end()
}

fn ser_five_hour<S: Serializer>(w: &Option<Window>, s: S) -> Result<S::Ok, S::Error> {
    ser_bucket(w, FIVE_HOUR_LEN, s)
}

fn ser_weekly<S: Serializer>(w: &Option<Window>, s: S) -> Result<S::Ok, S::Error> {
    ser_bucket(w, SEVEN_DAY_LEN, s)
}

/// Manual impl so `low` / `critical` go over the wire as computed fields.
impl Serialize for Window {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Window", 5)?;
        st.serialize_field("used_pct", &self.used_pct)?;
        st.serialize_field("resets_at", &self.resets_at)?;
        st.serialize_field("observed_at", &self.observed_at)?;
        st.serialize_field("low", &self.low())?;
        st.serialize_field("critical", &self.critical())?;
        st.end()
    }
}

/// Codex 的額度重置券（`rateLimitResetCredits`）：額度用完時使用者唯一能做的事，所以要看得到
/// （2026-09-10 使用者）。daemon 只讀不用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResetCredits {
    pub available: i64,
    pub title: Option<String>,
    pub expires_at: Option<String>,
}

/// CLI 印的上限橫幅。credits 用完時 5h／7d 速率窗可以是滿的（2026-09-12 使用者：量表全滿卻一直
/// hit limit），所以單獨記且**黏住**：[`set`] 沿用舊值，直到 `until` 過了或下一回合跑成功。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Quota {
    #[serde(serialize_with = "ser_five_hour")]
    pub five_hour: Option<Window>,
    #[serde(serialize_with = "ser_weekly")]
    pub seven_day: Option<Window>,
    /// Max 方案才有的 Fable 週額度；`None` 時 UI 完全不畫。
    #[serde(serialize_with = "ser_weekly")]
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

impl Quota {
    fn window_of(&self, b: Bucket) -> Option<&Window> {
        match b {
            Bucket::FiveHour => self.five_hour.as_ref(),
            Bucket::SevenDay => self.seven_day.as_ref(),
            Bucket::Fable => self.fable.as_ref(),
        }
    }

    /// **那一桶的讀數**比它自己的窗長還舊嗎。
    ///
    /// 年齡看 [`Window::observed_at`]（那一桶最後一次真的出現在新讀數裡的時間），沒有才退回
    /// `Quota::updated_at`。不能只看 `updated_at`：[`set`] 會沿用新讀數缺的窗，而 `updated_at`
    /// 蓋成現在，被沿用的那一桶年齡永遠是 0（i267 review，#475）。解不開就回 `false`（不知道年齡，不亂判）。
    fn reading_older_than_window(&self, b: Bucket, now: chrono::DateTime<chrono::Utc>) -> bool {
        // `observed_at` 解不開時**退回 `updated_at`**：能用的時間戳優先於「不知道」。
        // 兩個都解不開才算說不出年齡。
        let observed = self.window_of(b).and_then(|w| w.observed_at.as_deref()).and_then(parse_utc);
        match observed.or_else(|| parse_utc(&self.updated_at)) {
            Some(at) => now - at >= b.len(),
            // **兩個時間戳都解不開＝當成已經過期**（i267 review，#518）：跟其他四處「解不開＝已過去」同向。
            //
            // 第一版回 `false`（＝「這筆讀數還很新」），方向剛好相反，於是 #475 那個「永久擋住一個身分」
            // 原封不動回來：`resets_at` 被丟成 `None`（#489 那顆）收不掉、`reset_passed` 回 false、
            // `exhausted` 永遠 true。這裡的嚴格方向是「寧可放掉，也不要永久擋住」——跟 #464／#475
            // 整條線的取捨一致，而且 [`load_cache`] 已經先把這兩個欄位驗過，壞值不該從那條路進來。
            None => true,
        }
    }

    /// 這一桶**還說得出話**的讀數，沒有就 `None`（issue #475）。
    ///
    /// 「見底、沒有 `resets_at`」的讀數在 [`Window::exhausted_at`] 眼裡永遠算用盡——沒有時間可以
    /// 讓它翻回來。這種讀數真的會產生（`quota_claude` 的 statusline 路徑解不出重置時間就是 `None`，
    /// 而百分比照樣可能見底），於是那個身分在**同一次 uptime 內**被永久排除；而這台 daemon 常連跑好幾天。
    ///
    /// 規則：`resets_at` 是 `None` 而且這筆讀數比那一桶的窗長還舊 → 當成**沒有讀數**。一筆比窗長
    /// 還舊的讀數必定跨過了一次重置，不管它當時是幾 %。有 `resets_at` 的**不套這條**——那時
    /// `reset_passed` 判得更準，而且一個 7d 窗的讀數本來就可能好幾天前才更新、窗卻還沒到。
    ///
    /// 回 `None` 而不是「不算用盡」，是為了讓「不知道」跟「有額度」分開：`responder` 要兩個共用窗
    /// **都有讀數**才敢說恢復，拿一筆過期讀數冒充「沒見底」會讓它宣稱一個沒有證據的恢復。
    ///
    /// issue #464 原本只在 `load_cache` 開機時做一次（i266 在 #475 指出來）：那只擋得住跨重啟，
    /// 同一次 uptime 內照樣永久卡住。現在只有這一份，而且在每次判斷時都跑（純計算，沒有 I/O）。
    pub fn usable_window(&self, b: Bucket, now: chrono::DateTime<chrono::Utc>) -> Option<&Window> {
        let w = self.window_of(b)?;
        if w.resets_at.is_none() && self.reading_older_than_window(b, now) {
            return None;
        }
        Some(w)
    }

    /// 這一桶現在算不算用盡：讀數還說得出話、窗還沒重置、而且見底。三處共用（`mission::pick`、
    /// `supervisor::policy`、`supervisor::responder`）。
    pub fn exhausted(&self, b: Bucket, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.usable_window(b, now).is_some_and(|w| w.exhausted_at(now))
    }
}

/// DB 裡的額度讀數不是派工判準；它只在開機時先填回畫面，第一次新的探測成功後才變成 fresh。
/// `Quota` 本身不帶 stale，避免把顯示用的狀態帶進 daemon 內部所有額度判斷。
pub fn is_retired_agy_quota_key(key: &str) -> bool {
    key == "agy:claude-gpt" || key.ends_with("/agy:claude-gpt")
}

pub async fn load_cache(app: &(impl crate::capabilities::Db + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables)) -> Result<usize> {
    let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT key, quota_json, updated_at FROM quota_cache")
        .fetch_all(app.db())
        .await?;
    let mut restored = Vec::new();
    let mut retired = Vec::new();
    for (key, raw, updated_at) in rows {
        // agy 現在只提供 Gemini 額度。清除舊版保存的 Claude/GPT 子 key，避免重啟後又出現在快照。
        if is_retired_agy_quota_key(&key) {
            retired.push(key);
            continue;
        }
        let mut q: Quota = match serde_json::from_str(&raw) {
            Ok(q) => q,
            Err(e) => {
                tracing::warn!(key, error = %e, "ignoring an invalid cached quota");
                continue;
            }
        };
        // Cache rows can outlive the reset. Keep the existing limit-hit expiry rule at boot too,
        // so an old "用完了" marker never blocks work while the first fresh probe is pending.
        if limit_hit_expired(q.limit_hit.as_ref()) {
            q.limit_hit = None;
        }
        q.updated_at = if updated_at.trim().is_empty() { q.updated_at } else { updated_at };
        // 快取列的 `resets_at` 也要驗一次（i264 review，#489）：`load_cache` 是裸的
        // `serde_json::from_str`，而 `Window` 只 derive `Deserialize`，所以解不開的值**不是被丟掉，
        // 是原樣載回記憶體**——接著 `reset_passed` 把它讀成「已經跨過重置」，那個身分一路看起來有額度，
        // 直到下一次探測成功才好，而 `unix_to_rfc3339`／`already_past` 的兩個 warn 都不在這條路上。
        drop_unparseable_resets(&mut q, &key);
        q.host = key.split_once('/').map_or(LOCAL_HOST, |(host, _)| host).to_string();
        restored.push((key, q));
    }

    let count = restored.len();
    let mut quotas = app.quotas().lock().await;
    let mut stale = app.quota_stale().lock().await;
    for key in &retired {
        quotas.remove(key);
        stale.remove(key);
    }
    for (key, q) in restored {
        quotas.insert(key.clone(), q);
        stale.insert(key);
    }
    drop(stale);
    drop(quotas);
    for key in &retired {
        delete_cache(app, key).await;
    }
    Ok(count)
}

pub async fn persist_cache(app: &impl crate::capabilities::Db, key: &str, q: &Quota) {
    let _cache_write = cache_write_lock(key).await;
    persist_cache_locked(app, key, q).await;
}

/// Caller holds [`cache_write_lock`] across the state mutation and this write.
async fn persist_cache_locked(app: &impl crate::capabilities::Db, key: &str, q: &Quota) {
    let raw = match serde_json::to_string(q) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(key, error = %e, "cannot serialize quota cache");
            return;
        }
    };
    // 兩邊都包 `ts_sql` 照時刻比：舊列可能是秒格式（`…:00Z`），同一秒內較新的毫秒讀數（`…:00.500Z`）不能被字串比較判成較舊（#101）。
    let sql = format!(
        "INSERT INTO quota_cache (key, quota_json, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET quota_json=excluded.quota_json, updated_at=excluded.updated_at
         WHERE {} >= {}",
        crate::db::ts_sql("excluded.updated_at"),
        crate::db::ts_sql("quota_cache.updated_at"),
    );
    if let Err(e) = sqlx::query(&sql)
    .bind(key)
    .bind(raw)
    .bind(&q.updated_at)
    .execute(app.db())
    .await
    {
        tracing::warn!(key, error = %e, "cannot persist quota cache");
    }
}

async fn delete_cache(app: &impl crate::capabilities::Db, key: &str) {
    if let Err(e) = sqlx::query("DELETE FROM quota_cache WHERE key = ?").bind(key).execute(app.db()).await {
        tracing::warn!(key, error = %e, "cannot delete quota cache");
    }
}

/// Remove a quota key that no longer belongs to a live identity/host, including its restart cache.
pub async fn forget(app: &(impl crate::capabilities::Db + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables), key: &str) {
    let _cache_write = cache_write_lock(key).await;
    app.quotas().lock().await.remove(key);
    app.quota_stale().lock().await.remove(key);
    delete_cache(app, key).await;
}

pub fn quota_value(q: &Quota, stale: bool) -> Value {
    let mut value = serde_json::to_value(q).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert("stale".into(), Value::Bool(stale));
    }
    value
}

/// 秒與毫秒都收（**加固，不是修 bug**：目前沒有任何來源送毫秒，issue #464）。
///
/// 沒有這道防線時，一個毫秒值會被當成秒算出西元五萬多年的 `resets_at`——`chrono` 收得下那個範圍，
/// 所以不會失敗，只會安靜地產生一個「永遠不會到」的重置時間：`future()` 那類過濾永遠成立、
/// `same_window` 永遠對不上、靠它判「窗是不是重置了」的地方（#464 的 `window_reset`）永遠說沒有。
/// 寧可在入口把單位判掉，也不要讓一個單位錯誤變成永久卡住的額度。
const MS_THRESHOLD: i64 = 1_000_000_000_000;

pub fn unix_to_rfc3339(v: Option<&Value>) -> Option<String> {
    let raw = match v? {
        Value::Number(n) => n.as_f64()? as i64,
        Value::String(s) => {
            if let Ok(n) = s.parse::<i64>() {
                n
            } else {
                // 已經是時間字串：**至少驗一次解得開**再放行（i204 review，#489）。原本是原樣回傳，
                // 所以格式漂移時一串亂碼會直接變成 `resets_at`，而解不開的重置時間會被
                // `Window::reset_passed` 讀成「已經跨過重置」——那個身分就永遠看起來有額度。
                // 解不開就丟掉：`resets_at: None` 是「不知道」，由 `usable_window` 的窗長規則收尾（#475），
                // 比一個會讓人誤判成「有額度」的壞值安全。
                // 只驗、不改寫：格式正規化不是這張票的事，而且下游一律 `parse_utc`／`cmp_ts` 比時刻。
                return match parse_utc(s) {
                    Some(_) => Some(s.clone()),
                    None => {
                        tracing::warn!(raw = %s, "額度讀數的重置時間不是 RFC3339，丟掉");
                        None
                    }
                };
            }
        }
        _ => return None,
    };
    // 1e12 秒 ＝ 西元 33658 年；1e12 毫秒 ＝ 2001 年。超過就只可能是毫秒。
    let (secs, millis) = if raw.abs() >= MS_THRESHOLD { (raw.div_euclid(1000), raw.rem_euclid(1000) as u32) } else { (raw, 0) };
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, millis * 1_000_000)
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
        Some(Window { observed_at: None, used_pct: v.get("usedPercent")?.as_f64()?, resets_at: unix_to_rfc3339(v.get("resetsAt")) })
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
    // 兩個窗都讀不出來（空的 rateLimits、malformed）不是完整帳號讀數：回 None，讓呼叫端當成探測失敗，
    // 不去清掉有效的窗與撞限（#1506）。只有週窗的方案照常是完整讀數（#1496）。
    if five.is_none() && seven.is_none() {
        return None;
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
            observed_at: None,
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
async fn stale_split_keys(app: &(impl crate::quota::HostIdentities + crate::quota::QuotaTables), host: &str, kind: &str) -> Vec<String> {
    let prefix = quota_key(host, &format!("{kind}:"));
    let candidates: Vec<String> = app.quotas().lock().await.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
    let mut stale = Vec::new();
    for key in candidates {
        if quota_base_for_host(app, host, kind, Some(&key[prefix.len()..])).await == kind {
            stale.push(key);
        }
    }
    stale
}

/// 外部探測的結果只在探測開始時記下的 `fence` 仍是這台主機的權威時才發布（#347）：探測途中同名主機被重連／改指到
/// 另一台、或被移除，舊機器的額度就不能寫進新機器（或已移除主機）的 key。不是權威就回 `Err`、什麼都不寫。
/// 檢查在 `app.quotas` 鎖裡做：主機換連線時會清掉 `<host>/…`（`hosts::forget_host_observations`），檢查放在鎖外的話，
/// 通過檢查到寫入之間清掉的那份會被舊讀數種回來。
pub async fn set_fenced(
    app: &(impl crate::capabilities::DataDir
        + crate::capabilities::Db
        + crate::capabilities::Emit
        + crate::hosts::HostsAccess
        + crate::quota::HostIdentities
        + crate::quota::QuotaStaleKeys
        + crate::quota::QuotaTables),
    host: &str,
    base: &str,
    q: Quota,
    fence: &crate::hosts::HostFence,
) -> Result<()> {
    if !set_inner(app, host, base, q, Some(fence)).await {
        anyhow::bail!("host `{host}` was reconnected/reconfigured during the quota probe; stale reading discarded");
    }
    Ok(())
}

/// 快取列載回來時把解不開的 `resets_at` 設成 `None`（i264 review，#489）。
///
/// `None` ＝「不知道」，由 [`Quota::usable_window`] 的窗長規則收尾（#475）；留著壞值會被
/// [`Window::reset_passed`] 讀成「已重置」而放行一個可能真的見底的身分。
fn drop_unparseable_resets(q: &mut Quota, key: &str) {
    if parse_utc(&q.updated_at).is_none() {
        // 不編一個時間出來（那等於謊報新鮮度）：只記下來。年齡判斷那一側對解不開的已經當成過期
        // （`reading_older_than_window`），所以結果是「這筆讀數不算數」，不是被當成很新。
        tracing::warn!(key, updated_at = %q.updated_at, "快取列的 updated_at 解不開，這筆讀數的年齡無從判斷");
    }
    for (bucket, w) in [("five_hour", &mut q.five_hour), ("seven_day", &mut q.seven_day), ("fable", &mut q.fable)] {
        let Some(w) = w.as_mut() else { continue };
        if let Some(raw) = w.resets_at.as_deref() {
            if parse_utc(raw).is_none() {
                tracing::warn!(key, bucket, resets_at = %raw, "快取裡的重置時間解不開，載回來時丟掉");
                w.resets_at = None;
            }
        }
        // `observed_at` 同樣要驗（i267 review，#518）：留著壞值會讓年齡判斷拿它當輸入。
        // 設成 `None` 就退回 `updated_at`，那個也壞的話由上面那條 warn ＋ 年齡側的「解不開＝過期」收尾。
        if let Some(raw) = w.observed_at.as_deref() {
            if parse_utc(raw).is_none() {
                tracing::warn!(key, bucket, observed_at = %raw, "快取裡的觀測時間解不開，載回來時丟掉");
                w.observed_at = None;
            }
        }
    }
}

/// 這個重置時刻已經過去了嗎。**解不開的也算「已過去」**，所以不會被沿用（i204 review，#489）。
///
/// 第一版這裡回 `false`（＝沿用看不懂的值，交給讀取端判），跟 [`Window::reset_passed`] 把解不開當成
/// 「已經跨過重置」**方向相反**，兩者接起來就重現了這張票要修的問題，而且更糟——它會一直黏著：
/// 解不開的值被 `set` 一路沿用，而 `reset_passed` 每次都說「重置過了」，於是那個身分從此永遠看起來
/// 有額度，即使 97% 用掉。過期的時刻至少會隨著新讀數自己好，解不開的不會。
///
/// 寫成具名函式而不是 `is_some_and(|t| t <= now)`：`timestamp_compat_tests` 那條原始碼 lint 認得
/// `|t| t <= …` 這個形狀（issue #101），而它分不出閉包參數是時間字串還是已經 parse 過的 `DateTime`。
/// 這裡比的是 `DateTime`，秒／毫秒混存不影響。
fn already_past(resets_at: Option<&str>) -> bool {
    let now = chrono::Utc::now();
    match resets_at {
        // 沒有重置時間：沒有東西可以沿用，也不必說它過去了。
        None => false,
        Some(raw) => match parse_utc(raw) {
            Some(at) => at <= now,
            None => {
                tracing::warn!(resets_at = %raw, "額度讀數的重置時間解不開，不沿用它");
                true
            }
        },
    }
}

/// 讀數出口的最後一道檢查（對抗式審查）：入口（statusLine、app-server、`/usage` 文字、grok 長條）各自 parse 出裸 `f64`，
/// 只有少數幾條有夾值。百分比一律夾進 0–100；不是有限數（`NaN`／`inf`）整格丟掉——留著的話
/// `(100 - NaN).max(0)` 是 0，那個身分會被判成見底。丟掉之後 [`set_inner`] 會沿用上一份的同一桶，不是填 0。
fn sanitize_percentages(q: &mut Quota, key: &str) {
    for (bucket, w) in [("five_hour", &mut q.five_hour), ("seven_day", &mut q.seven_day), ("fable", &mut q.fable)] {
        let Some(win) = w.as_mut() else { continue };
        if !win.used_pct.is_finite() {
            tracing::warn!(key, bucket, used_pct = win.used_pct, "額度讀數的百分比不是有限數，丟掉這一桶");
            *w = None;
        } else if !(0.0..=100.0).contains(&win.used_pct) {
            tracing::warn!(key, bucket, used_pct = win.used_pct, "額度讀數的百分比超出 0–100，夾進範圍");
            win.used_pct = win.used_pct.clamp(0.0, 100.0);
        }
    }
}

/// 執行中沒有人更新超過這麼久，就當成陳舊的讀數。最慢的健康節奏是 claude `/usage` 的 10 分鐘（`USAGE_REFRESH`），
/// codex 5 分鐘、grok 30 秒；留三倍，容得下兩次探測失敗。
pub const STALE_AFTER: chrono::Duration = chrono::Duration::minutes(30);

/// 這筆讀數該不該標「陳舊」：開機從快取回填、還沒被真探測換掉的（`flagged`），或執行中太久沒人更新。
/// 解不開的 `updated_at` 當成陳舊（同這個檔案其他地方「解不開＝過期」的方向）。
pub fn reading_is_stale(q: &Quota, flagged: bool, now: chrono::DateTime<chrono::Utc>) -> bool {
    flagged || parse_utc(&q.updated_at).map_or(true, |at| now - at > STALE_AFTER)
}

pub async fn set(
    app: &(impl crate::capabilities::DataDir
        + crate::capabilities::Db
        + crate::capabilities::Emit
        + crate::hosts::HostsAccess
        + crate::quota::HostIdentities
        + crate::quota::QuotaStaleKeys
        + crate::quota::QuotaTables),
    host: &str,
    base: &str,
    q: Quota,
) {
    set_inner(app, host, base, q, None).await;
}

/// 回 `false`＝`fence` 已經不是這台主機的權威，什麼都沒寫。
async fn set_inner(
    app: &(impl crate::capabilities::DataDir
        + crate::capabilities::Db
        + crate::capabilities::Emit
        + crate::hosts::HostsAccess
        + crate::quota::HostIdentities
        + crate::quota::QuotaStaleKeys
        + crate::quota::QuotaTables),
    host: &str,
    base: &str,
    mut q: Quota,
    fence: Option<&crate::hosts::HostFence>,
) -> bool {
    q.host = host.to_string();
    let key = quota_key(host, base);
    if is_retired_agy_quota_key(&key) {
        if let Some(f) = fence {
            if !app.hosts().is_current(f).await {
                return false;
            }
        }
        // Any stale caller is a chance to evict the retired value, never write it again.
        forget(app, &key).await;
        return true;
    }
    sanitize_percentages(&mut q, &key);
    let stale = if base.contains(':') { Vec::new() } else { stale_split_keys(app, host, base).await };
    // This write can also delete split keys; lock every touched key in sorted order so an old
    // in-flight split-key write cannot recreate a row after that deletion.
    let mut write_keys = vec![key.clone()];
    write_keys.extend(stale.iter().cloned());
    write_keys.sort();
    write_keys.dedup();
    let mut cache_writes = Vec::with_capacity(write_keys.len());
    for write_key in write_keys {
        cache_writes.push(cache_write_lock(&write_key).await);
    }
    // 撞限校正只看**這份讀數自己帶來的**窗；下面沿用的舊窗不是新證據。
    let brings_its_own_hit = q.limit_hit.is_some();
    let fresh = q.clone();
    let mut quotas = app.quotas().lock().await;
    if let Some(f) = fence {
        if !app.hosts().is_current(f).await {
            return false;
        }
    }
    if q.source == "statusline" {
        if let Some(previous) = quotas.get(&key) {
            if let Some((bucket, previous_reset, incoming_reset)) = guard_statusline_windows(previous, &mut q, chrono::Utc::now()) {
                tracing::debug!(host, key, bucket, previous_reset, incoming_reset, "discarded a stale Claude statusline quota snapshot");
                return true;
            }
        }
    }
    for k in &stale {
        if quotas.remove(k).is_some() {
            tracing::info!(host, stale = %k, bare = %key, "dropped a split quota key that now resolves to the bare key");
        }
    }
    // issue #475（i267 review）：**這一次真的帶進來的**窗蓋上觀測時間；下面沿用舊窗時原樣保留它。
    // 沒有這一步，被沿用的那一桶會跟著 `updated_at` 一直「看起來很新」，窗長到期永遠不成立。
    for w in [&mut q.five_hour, &mut q.seven_day, &mut q.fable].into_iter().flatten() {
        if w.observed_at.is_none() {
            w.observed_at = Some(if q.updated_at.trim().is_empty() { crate::db::now() } else { q.updated_at.clone() });
        }
    }
    // A window the new reading lacks keeps the previous value: statusLine has no Fable and
    // otherwise wipes the probe's F bar every few seconds.
    if q.fable.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.fable = prev.fable.clone();
        }
    }
    // 部分讀數沿用缺窗；codex app-server 是完整帳號讀數，缺窗表示方案沒有該窗。
    if q.source != "codex-app-server" && (q.five_hour.is_none() || q.seven_day.is_none()) {
        if let Some(prev) = quotas.get(&key) {
            if q.five_hour.is_none() {
                q.five_hour = prev.five_hour.clone();
            }
            if q.seven_day.is_none() {
                q.seven_day = prev.seven_day.clone();
            }
        }
    }
    // 重置券只有 app-server 讀得到，別的來源不該抹掉；但 app-server 已確認換了方案（兩邊的 planType 都知道且不同）
    // 時，舊方案的券不再屬於這個帳號，不沿用（#1508）。
    if q.reset_credits.is_none() {
        if let Some(prev) = quotas.get(&key) {
            let plan_changed = q.source == "codex-app-server" && q.plan.is_some() && prev.plan.is_some() && q.plan != prev.plan;
            if !plan_changed {
                q.reset_credits = prev.reset_credits.clone();
            }
        }
    }
    // 撞上限只有 CLI 橫幅看得到（§12.4），不沿用會被 app-server 輪詢洗回滿格（2026-09-12 使用者）。
    if q.limit_hit.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.limit_hit = prev.limit_hit.clone().filter(|hit| {
                let plan_changed = q.source == "codex-app-server"
                    && q.plan.is_some() && prev.plan.is_some() && q.plan != prev.plan
                    && codex_reading_after_hit(hit, &fresh);
                !plan_changed
            });
        }
    }
    if !brings_its_own_hit {
        // `fresh` 是這次帶進來的讀數，不看 stale 旗標（見 `grok_reading_clears_hit`）。
        q.limit_hit = q.limit_hit.take().and_then(|h| recalibrate_for_key(app, &key, h, &fresh, true));
    }
    if limit_hit_expired(q.limit_hit.as_ref()) {
        q.limit_hit = None;
        forget_grok_pct_clear(app, &key);
    }
    // CLI 狀態列沒有重置時間，沿用上一份，否則量表的「N 小時後重置」會消失。
    // **已經過去的不沿用**（issue #489）：對顯示沒有意義（畫面會寫「N 小時前重置」），而且它會被
    // `reset_passed` 讀成「這個窗重置過了」，把一筆剛讀到、真的見底的狀態列讀數放行。
    if let Some(prev) = quotas.get(&key) {
        for (now, old) in [(&mut q.five_hour, &prev.five_hour), (&mut q.seven_day, &prev.seven_day), (&mut q.fable, &prev.fable)] {
            if let (Some(w), Some(p)) = (now.as_mut(), old.as_ref()) {
                if w.resets_at.is_none() && !already_past(p.resets_at.as_deref()) {
                    w.resets_at = p.resets_at.clone();
                }
            }
        }
    }

    quotas.insert(key.clone(), q.clone());
    drop(quotas);
    {
        let mut stale_flags = app.quota_stale().lock().await;
        stale_flags.remove(&key);
        for old in &stale {
            stale_flags.remove(old);
        }
    }
    for old in &stale {
        delete_cache(app, old).await;
    }
    persist_cache_locked(app, &key, &q).await;
    // 寫進記憶體之後、寫快取列之前主機被換掉：換連線那一側已經清過這把 key，這裡剛寫的列會把舊機器的讀數留到下次開機。
    // 讓快取列回到記憶體現在的樣子（被清掉就刪，新連線已寫了就用它的）。
    if let Some(f) = fence {
        if !app.hosts().is_current(f).await {
            let now = app.quotas().lock().await.get(&key).cloned();
            match now {
                Some(cur) => persist_cache_locked(app, &key, &cur).await,
                None => delete_cache(app, &key).await,
            }
            return true;
        }
    }
    drop(cache_writes);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": quota_value(&q, false)})).await;
    true
}

/// 兩個 `resets_at` 差這麼多以內算同一個窗：`/usage` 的結構化時間帶毫秒（`…:00.594Z`），statusLine 是整秒。
/// 相鄰兩個窗的重置時間至少差 5 小時，幾分鐘的容忍不會把兩個窗併成一個。
const SAME_WINDOW_SLACK: chrono::Duration = chrono::Duration::minutes(5);

fn same_window(a: chrono::DateTime<chrono::Utc>, b: chrono::DateTime<chrono::Utc>) -> bool {
    (a - b).abs() <= SAME_WINDOW_SLACK
}

/// 這份 statusLine 讀數跟 cache 裡那份比，誰是比較新的 API 回應（#404）。
///
/// 同一個帳號的 5h 窗是一個單調的時鐘：`(resets_at, used_pct)` 只會往前走，每個 API 回應都帶，而 statusLine 報的是
/// **那顆 session 最後一次 API 回應**的數字——閒置的 session 一直重畫同一份舊快照。所以：
/// - cache 的 5h 窗還沒結束，這份卻沒有 5h（claude 不帶已經結束的窗＝它最後一次回合在更早的窗裡）、5h 是更早的窗、
///   或同窗但 5h 用量比較低 → `Less`（比較舊）。
/// - 5h 是更晚的窗、或同窗用量比較高 → `Greater`（比較新）。
/// - 同窗同用量 → `Equal`；兩邊都說不出有效的 5h 窗 → `None`（無從比較）。
fn statusline_freshness(existing: &Quota, incoming: &Quota, now: chrono::DateTime<chrono::Utc>) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering::{Greater, Less};
    let at = |w: &Window| w.resets_at.as_deref().and_then(parse_utc);
    let old = existing.five_hour.as_ref().and_then(|w| Some((w.used_pct, at(w)?)));
    let old_current = old.is_some_and(|(_, t)| t > now);
    let Some(new) = incoming.five_hour.as_ref() else {
        return old_current.then_some(Less);
    };
    let new_at = at(new)?;
    match old {
        Some((old_used, old_at)) if same_window(old_at, new_at) => new.used_pct.partial_cmp(&old_used),
        Some((_, old_at)) if new_at > old_at => Some(Greater),
        Some(_) => old_current.then_some(Less),
        None => (new_at > now).then_some(Greater),
    }
}

/// Compare Claude statusLine windows with the value already stored under a shared account key.
/// Other sources (especially the active `/usage` probe) bypass this guard.
fn guard_statusline_windows(
    existing: &Quota,
    incoming: &mut Quota,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(&'static str, String, String)> {
    let freshness = statusline_freshness(existing, incoming, now);
    if freshness == Some(std::cmp::Ordering::Less) {
        let reset = |q: &Quota| q.five_hour.as_ref().and_then(|w| w.resets_at.clone()).unwrap_or_else(|| "none".into());
        return Some(("five_hour", reset(existing), reset(incoming)));
    }
    for (bucket, old, new) in [
        ("five_hour", &existing.five_hour, &mut incoming.five_hour),
        ("seven_day", &existing.seven_day, &mut incoming.seven_day),
        ("fable", &existing.fable, &mut incoming.fable),
    ] {
        let (Some(old), Some(new)) = (old.as_ref(), new.as_mut()) else { continue };
        let (Some(old_at), Some(new_at)) = (
            old.resets_at.as_deref().and_then(parse_utc),
            new.resets_at.as_deref().and_then(parse_utc),
        ) else {
            continue;
        };
        if same_window(old_at, new_at) {
            // 分不出誰新時才同窗取大；5h 證明這份比較新就照寫，帳號在窗內被重置（resets_at 不變）時才降得下來（#404）。
            if freshness != Some(std::cmp::Ordering::Greater) {
                new.used_pct = new.used_pct.max(old.used_pct);
            }
        } else if old_at > now && new_at < old_at {
            return Some((bucket, old.resets_at.clone().unwrap(), new.resets_at.clone().unwrap()));
        }
    }
    None
}

pub fn parse_utc(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s.trim()).ok().map(|t| t.with_timezone(&chrono::Utc))
}

fn codex_reading_after_hit(hit: &LimitHit, fresh: &Quota) -> bool {
    fresh.source == "codex-app-server"
        && parse_utc(&fresh.updated_at).zip(parse_utc(&hit.at)).is_some_and(|(reading, at)| reading >= at)
}

/// grok 的權威讀數解除撞限（#1900）。四項證據缺一不可：
/// - 來源是 `grok-usage`，而且這份讀數是新的（[`reading_is_stale`]，不是開機回填或太久沒人更新的）；
/// - 讀數時間**嚴格晚於** `hit.at`（兩邊任一解不開就不清）；
/// - 撞限帶桶名，而且只認 `five_hour`／`seven_day`（`fable`／`opus`／`sonnet` 沒有自己的量表、無桶名的
///   credits 用完更不是窗的百分比說得準的）；
/// - 讀數裡那一桶的窗存在且不是 [`Window::critical`]。窗不在讀數裡不算「不見底」，不清。
///
/// 同 host、同 identity 由 [`set_inner`] 保證：撞限是從同一把 `quota_key` 的上一筆沿用來的。
///
/// 為什麼可以看百分比：`/usage` 的百分比是 grok 帳號端的計數，沒有證據證明它每一刻都即時反映撞限，
/// 所以這裡**不**宣稱它一定準。看百分比的理由是誤清只會多跑一回合：真的還在擋的話，下一個 turn 再撞限，
/// 橫幅會再記一次 `limit_hit`。但百分比跟擋人的計數不是同一個時（#1900 撞週限兩天後讀數才 55%），每 30 秒
/// 一份讀數就會「清掉 → 放行 → 重撞」一直循環（#1905），所以同一把 key 被百分比清過之後又重撞的，在那一桶
/// 窗長內不再看百分比（[`grok_pct_clear_allowed`]）；這個函式本身只回答「讀數說不說得出不見底」。
/// 這跟 claude 的 `/usage` 不同（見 [`recalibrate_limit_hit`]：撞限前開始、撞限後
/// 才回來的探測，百分比可能還沒到頂，而 claude 沒有橫幅之外的出口）。獨立於 codex 的 [`codex_reading_after_hit`]。
///
/// `reading_is_stale(fresh, false, now)` 的 `false`（不看 stale 旗標、只看讀數年齡）是故意的，兩個呼叫端各自負責：
/// - [`set_inner`]：`fresh` 是這次呼叫**帶進來的**讀數（`q.clone()`，在沿用舊欄位之前），不是表裡那一筆；stale 旗標
///   標的是表裡開機回填的那筆，跟這份新讀數無關（旗標要等寫入之後才在 `set_inner` 裡拿掉，判斷當下可能還掛著）。
///   開機回填走 [`load_cache`] 直接 insert 並掛旗標，不經過 `set_inner`。
/// - [`restore_limit_hit`]：拿來校正的是表裡現有的那筆，可能就是開機回填、旗標還掛著的；那邊先查旗標，掛著就
///   不准走百分比（[`recalibrate_limit_hit_at`] 的 `grok_pct`），這個函式不會看到那種讀數。
pub fn grok_reading_clears_hit(hit: &LimitHit, fresh: &Quota, now: chrono::DateTime<chrono::Utc>) -> bool {
    if fresh.source != "grok-usage" || reading_is_stale(fresh, false, now) {
        return false;
    }
    let Some((reading, at)) = parse_utc(&fresh.updated_at).zip(parse_utc(&hit.at)) else { return false };
    if reading <= at {
        return false;
    }
    let window = match hit.bucket.as_deref() {
        Some("five_hour") => fresh.five_hour.as_ref(),
        Some("seven_day") => fresh.seven_day.as_ref(),
        _ => None,
    };
    window.is_some_and(|w| !w.critical())
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
/// 沒有桶名（codex 的 credits 用完、開機回填的格子）原則上不動：那種撞限只有橫幅說得準。例外：新讀數的
/// **5h 與 7d 兩個窗都是撞限之後才開的**——兩桶同時重開只會是整個帳號重置（提早重置券、或週窗本身到期），
/// 撞限作廢（2026-09-27 使用者：codex 提早重置後額度全滿，還掛著「被擋、下午 02:14 恢復」）。
/// Codex app-server 的新完整讀數另可解除已消失或不再見底的桶，只有週窗時也適用；沒寫重置時間的（credits 用完）除外。
/// 上面「用窗的起點而不是百分比」是針對 claude 的 `/usage`；grok 的 `grok-usage` 另有一條看百分比的解除
/// （[`grok_reading_clears_hit`]，#1900），依據與限制寫在那個函式上；同一把 key 重撞後的退避（#1905）要知道 key，
/// 由 [`set_inner`]／[`restore_limit_hit`] 經 [`recalibrate_for_key`] 套上，這個純函式照舊准看百分比。
pub fn recalibrate_limit_hit(hit: LimitHit, fresh: &Quota) -> Option<LimitHit> {
    recalibrate_limit_hit_at(hit, fresh, chrono::Utc::now(), true)
}

/// [`recalibrate_limit_hit`] 的本體。`grok_pct` 為 `false` 時跳過 grok 看百分比那一條（其他規則照舊）；
/// codex、claude 的讀數本來就走不到那一條，這個參數對它們沒有作用。
fn recalibrate_limit_hit_at(mut hit: LimitHit, fresh: &Quota, now: chrono::DateTime<chrono::Utc>, grok_pct: bool) -> Option<LimitHit> {
    // credits 用完（codex 橫幅沒寫重置時間＝`until: None`，沒有桶名）：窗的讀數證明不了 credits 回來了，權威讀數不解除（#1576）。
    // 方案變了另由 `set` 的 plan_changed 分支處理。
    if hit.bucket.is_none() && hit.until.is_none() && codex_reading_after_hit(&hit, fresh) {
        return Some(hit);
    }
    // 完整的 Codex 帳號讀數可證明撞限的桶已消失或不再見底。
    // 舊探測、statusline 與其他供應商不具有這個權威性。
    if codex_reading_after_hit(&hit, fresh) {
        let full = match hit.bucket.as_deref() {
            None => [&fresh.five_hour, &fresh.seven_day].into_iter().flatten().any(Window::critical),
            Some("five_hour") => fresh.five_hour.as_ref().is_some_and(Window::critical),
            Some("seven_day") => fresh.seven_day.as_ref().is_some_and(Window::critical),
            _ => true,
        };
        if !full {
            return None;
        }
    }
    // grok：窗開在撞限之前時下面那條永遠清不掉（#1900），權威讀數說那一桶不見底就解除。
    if grok_pct && grok_reading_clears_hit(&hit, fresh, now) {
        return None;
    }
    if hit.bucket.is_none() {
        return if both_windows_opened_after(&hit, fresh) { None } else { Some(hit) };
    }
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

/// 5h 與 7d 的窗都在撞限之後才開（`resets_at − 窗長 ≥ hit.at`）。任一桶讀不到就不算。
fn both_windows_opened_after(hit: &LimitHit, fresh: &Quota) -> bool {
    let Some(at) = parse_utc(&hit.at) else { return false };
    let opened_after = |w: Option<&Window>, len: chrono::Duration| {
        w.and_then(|w| w.resets_at.as_deref()).and_then(parse_utc).is_some_and(|r| r - len >= at)
    };
    opened_after(fresh.five_hour.as_ref(), chrono::Duration::hours(5)) && opened_after(fresh.seven_day.as_ref(), chrono::Duration::days(7))
}

/// 沒寫時間的一律**不**過期，只能靠 [`clear_limit_hit`]。
pub fn limit_hit_expired(hit: Option<&LimitHit>) -> bool {
    let Some(until) = hit.and_then(|h| h.until.as_deref()) else { return false };
    // 解不開的 `until` **不**算過期：這裡的「不過期」是**繼續擋**（嚴格的那一邊），
    // 跟 `already_past` 的嚴格方向一致，不是 `reset_passed` 那種「放行」。
    match parse_utc(until) {
        Some(t) => chrono::Utc::now() >= t,
        None => false,
    }
}

pub fn cleared_at() -> &'static std::sync::Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>> {
    static M: std::sync::OnceLock<std::sync::Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>> =
        std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

pub fn cleared_at_key(app: &impl crate::capabilities::DataDir, key: &str) -> String {
    format!("{}\u{0}{key}", app.data_dir().display())
}

/// grok 撞限被權威讀數用百分比清掉的紀錄（#1905），每把 key（[`cleared_at_key`]）一筆。只活在記憶體：
/// daemon 重啟後是空的，最多再「清掉 → 重撞」一輪。
#[derive(Debug, Clone, PartialEq)]
pub struct GrokPctClear {
    /// 上一次被百分比清掉的時刻；窗長從這裡起算。
    pub cleared_at: chrono::DateTime<chrono::Utc>,
    /// 被清掉的那一筆撞限的桶（`five_hour`／`seven_day`）。
    pub bucket: String,
    /// 被清掉的那一筆撞限原本寫的 `until`；到期了紀錄跟著作廢（沒寫或解不開就只看窗長）。
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    /// 清掉之後又重撞、被擋下不准再看百分比的次數。
    pub rehits: u32,
    /// 已經算過一次重撞的撞限（`hit.at`）：每 30 秒一份讀數，同一筆重撞只算一次、只 warn 一次。
    pub counted_at: Option<String>,
}

/// 紀錄的總數上限。滿了時先清掉作廢的紀錄（[`grok_pct_clear_expired`]），清完還是滿，沒有紀錄的 key 才一律
/// 不准看百分比（保守：多擋，不無限長）。
pub const GROK_PCT_CLEARS_MAX: usize = 256;

pub fn grok_pct_clears() -> &'static std::sync::Mutex<HashMap<String, GrokPctClear>> {
    static M: std::sync::OnceLock<std::sync::Mutex<HashMap<String, GrokPctClear>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn grok_bucket_len(bucket: &str) -> Option<chrono::Duration> {
    match bucket {
        "five_hour" => Some(FIVE_HOUR_LEN),
        "seven_day" => Some(SEVEN_DAY_LEN),
        _ => None,
    }
}

/// 紀錄作廢：過了那一桶的窗長（從上次清掉起算），或被清掉那筆撞限的 `until` 已經到期。
fn grok_pct_clear_expired(m: &GrokPctClear, now: chrono::DateTime<chrono::Utc>) -> bool {
    grok_bucket_len(&m.bucket).map_or(true, |len| now - m.cleared_at >= len) || m.until.is_some_and(|u| now >= u)
}

/// 表滿時先清掉作廢的紀錄；回傳清完之後還有沒有空位。
fn grok_pct_clears_room(marks: &mut HashMap<String, GrokPctClear>, now: chrono::DateTime<chrono::Utc>) -> bool {
    if marks.len() >= GROK_PCT_CLEARS_MAX {
        marks.retain(|_, m| !grok_pct_clear_expired(m, now));
    }
    marks.len() < GROK_PCT_CLEARS_MAX
}

/// 這筆撞限能不能用百分比解除（#1905）。表裡那把 key 的紀錄作廢（[`grok_pct_clear_expired`]）就刪掉。
/// - 有紀錄、而且撞限是在上次清掉**之後**才撞的（`hit.at ≥ cleared_at`）＝清掉又重撞：不准，重撞次數加一（同一筆只加一次）；
/// - 有紀錄但撞限早於上次清掉（重啟後種回的舊撞限）：不是重撞，照舊准；
/// - 沒有紀錄：准；表滿時先清掉作廢的紀錄，清完還是滿就不准（保守）。撞限時間解不開也不准。
pub fn grok_pct_clear_allowed(marks: &mut HashMap<String, GrokPctClear>, mark: &str, hit: &LimitHit, now: chrono::DateTime<chrono::Utc>) -> bool {
    if marks.get(mark).is_some_and(|m| grok_pct_clear_expired(m, now)) {
        marks.remove(mark);
    }
    let Some(at) = parse_utc(&hit.at) else { return false };
    match marks.get_mut(mark) {
        Some(m) if at >= m.cleared_at => {
            if m.counted_at.as_deref() != Some(hit.at.as_str()) {
                m.rehits += 1;
                m.counted_at = Some(hit.at.clone());
                tracing::warn!(key = mark.rsplit('\u{0}').next().unwrap_or(mark), bucket = %m.bucket, rehits = m.rehits,
                    "grok limit hit again after a /usage percentage cleared it; percentage no longer clears it within the window");
            }
            false
        }
        Some(_) => true,
        None => grok_pct_clears_room(marks, now),
    }
}

/// 記下「這把 key 的撞限剛被百分比清掉」。已有紀錄的沿用重撞次數；表滿時先清掉作廢的，清完還是滿就不新增
/// （那種 key 根本不會被准許清）。
pub fn note_grok_pct_clear(marks: &mut HashMap<String, GrokPctClear>, mark: &str, hit: &LimitHit, now: chrono::DateTime<chrono::Utc>) {
    let Some(bucket) = hit.bucket.clone() else { return };
    let until = hit.until.as_deref().and_then(parse_utc);
    if let Some(m) = marks.get_mut(mark) {
        m.cleared_at = now;
        m.bucket = bucket;
        m.until = until;
    } else if grok_pct_clears_room(marks, now) {
        marks.insert(mark.to_string(), GrokPctClear { cleared_at: now, bucket, until, rehits: 0, counted_at: None });
    }
}

fn forget_grok_pct_clear(app: &impl crate::capabilities::DataDir, key: &str) {
    grok_pct_clears().lock().unwrap().remove(&cleared_at_key(app, key));
}

/// 帶 key 的撞限校正：grok 看百分比那一條先過 [`grok_pct_clear_allowed`]，清掉了就記一筆；其他規則同
/// [`recalibrate_limit_hit`]。`grok_pct` 為 `false`（呼叫端知道讀數不夠格）時那一條直接跳過、也不碰表。
/// codex、claude 的讀數走不到 [`grok_reading_clears_hit`]，所以也不碰表，行為與 [`recalibrate_limit_hit`] 相同。
fn recalibrate_for_key(app: &impl crate::capabilities::DataDir, key: &str, hit: LimitHit, fresh: &Quota, grok_pct: bool) -> Option<LimitHit> {
    let now = chrono::Utc::now();
    if grok_pct && grok_reading_clears_hit(&hit, fresh, now) {
        let mark = cleared_at_key(app, key);
        let mut marks = grok_pct_clears().lock().unwrap();
        if grok_pct_clear_allowed(&mut marks, &mark, &hit, now) {
            note_grok_pct_clear(&mut marks, &mark, &hit, now);
            return None;
        }
    }
    recalibrate_limit_hit_at(hit, fresh, now, false)
}

/// 一回合真的跑完就拿掉「撞上限」，不必等它自己寫的時間。
///
/// 不管這一格當下有沒有撞限都記下時刻：成功回合本身就是「這個帳號收得下工作」的證據，
/// 撞限可能在那之前已經自己過期、或是重啟後根本沒被回填。
pub async fn clear_limit_hit(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables), host: &str, base: &str) {
    let key = quota_key(host, base);
    let _cache_write = cache_write_lock(&key).await;
    cleared_at().lock().unwrap().insert(cleared_at_key(app, &key), chrono::Utc::now());
    forget_grok_pct_clear(app, &key);
    let mut quotas = app.quotas().lock().await;
    let Some(q) = quotas.get_mut(&key) else { return };
    if q.limit_hit.is_none() {
        return;
    }
    q.limit_hit = None;
    let out = q.clone();
    drop(quotas);
    let stale = app.quota_stale().lock().await.contains(&key);
    persist_cache_locked(app, &key, &out).await;
    drop(_cache_write);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": quota_value(&out, stale)})).await;
}

/// 只清 `success_at` **之前**就命中的撞限，給「成功請求的送達時間」當界線（已關聯的成功回合，daemon 時鐘）。
///
/// 撞限的 `at` 與 `success_at` 都要以時刻比較，不比字串：不同 offset 與小數位數照時刻算。任一時間解不開、
/// 沒有撞限、或撞限不早於 `success_at`，什麼都不動（不寫欄位、不寫 `cleared_at`），回 `false`。
/// 只有真的清掉舊撞限才把 `cleared_at` 推到 `max(既有, success_at)`，不用 now，也不倒退。
///
/// 讀撞限、比較、清除、持久化都在同一把 `cache_write_lock` 內；鎖外不讀撞限，所以晚到的舊快照寫入
/// 與同一把 key 的新撞限都排在這次判斷之後，不會被它蓋掉或被它清掉。
pub async fn clear_limit_hit_if_older_than(
    app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables),
    host: &str,
    base: &str,
    success_at: chrono::DateTime<chrono::Utc>,
) -> bool {
    let key = quota_key(host, base);
    let cache_write = cache_write_lock(&key).await;
    let mut quotas = app.quotas().lock().await;
    let Some(q) = quotas.get_mut(&key) else { return false };
    let Some(hit_at) = q.limit_hit.as_ref().and_then(|h| parse_utc(&h.at)) else { return false };
    if hit_at >= success_at {
        return false;
    }
    q.limit_hit = None;
    let out = q.clone();
    drop(quotas);
    forget_grok_pct_clear(app, &key);
    {
        let mut marks = cleared_at().lock().unwrap();
        let slot = marks.entry(cleared_at_key(app, &key)).or_insert(success_at);
        if *slot < success_at {
            *slot = success_at;
        }
    }
    let stale = app.quota_stale().lock().await.contains(&key);
    persist_cache_locked(app, &key, &out).await;
    drop(cache_write);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": quota_value(&out, stale)})).await;
    true
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
pub async fn seed_limit_hit(app: &(impl crate::capabilities::Db + crate::capabilities::Emit + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables), host: &str, base: &str, until: &str, message: &str, bucket: Option<String>) -> bool {
    let Some(t) = parse_utc(until) else { return false };
    if t <= chrono::Utc::now() {
        return false;
    }
    let key = quota_key(host, base);
    let _cache_write = cache_write_lock(&key).await;
    let mut quotas = app.quotas().lock().await;
    if let Some(hit) = quotas.get(&key).and_then(|q| q.limit_hit.as_ref()) {
        if !limit_hit_expired(Some(hit)) {
            // 沒寫時間的撞限永不過期（`limit_hit_expired`），一定比任何時間都「晚」。
            let keep = match hit.until.as_deref().and_then(parse_utc) {
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
    let stale = app.quota_stale().lock().await.contains(&key);
    persist_cache_locked(app, &key, &out).await;
    drop(_cache_write);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": quota_value(&out, stale)})).await;
    true
}

/// [`seed_limit_hit`] 的另一個來源：排著的 prompt 被擋下時自己記下的那筆撞限（`lifecycle::quota_hold`），
/// 重啟後**原樣**種回來——`at` 是當初撞限的時刻、帶著桶名，沒寫時間的（codex credits）照樣黏著。
///
/// `at` 要是原本那一刻，讀數才校正得準（[`recalibrate_limit_hit`] 靠它判斷窗是不是撞限之後才開的）；
/// 重啟之後、回填之前已經有新讀數進來的話，當場校正一次，不必等下一份。已經過期、被校正作廢、或這一格
/// 已有更晚（或黏著）的撞限時不寫。回傳有沒有真的寫進去。
pub async fn restore_limit_hit(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables), host: &str, base: &str, hit: LimitHit) -> bool {
    if limit_hit_expired(Some(&hit)) {
        return false;
    }
    let key = quota_key(host, base);
    let _cache_write = cache_write_lock(&key).await;
    let mut quotas = app.quotas().lock().await;
    // credits 撞限不由讀數解除（見 recalibrate_limit_hit）：權威讀數在它之後、格子卻已經沒有撞限，代表它已被方案變更或成功回合清掉，不寫回（#1576）。
    if quotas.get(&key).is_some_and(|q| q.limit_hit.is_none() && hit.bucket.is_none() && hit.until.is_none() && codex_reading_after_hit(&hit, q)) {
        return false;
    }
    // 表裡這筆可能是開機回填、還沒被真探測換掉的（stale 旗標還掛著）：那種讀數不准拿來看百分比。
    // 上鎖順序同 `load_cache`：quotas → quota_stale。
    let grok_pct = !app.quota_stale().lock().await.contains(&key);
    let hit = match quotas.get(&key) {
        Some(q) => match recalibrate_for_key(app, &key, hit, q, grok_pct) {
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
    let stale = app.quota_stale().lock().await.contains(&key);
    persist_cache_locked(app, &key, &out).await;
    drop(_cache_write);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": quota_value(&out, stale)})).await;
    true
}

/// Base kinds always present per host (empty bars before first report); orphan-host keys dropped.
pub async fn snapshot(app: &(impl crate::hosts::HostsAccess + crate::quota::QuotaStaleKeys + crate::quota::QuotaTables)) -> Value {
    let hosts = app.hosts().names().await;
    let q = app.quotas().lock().await.clone();
    let stale = app.quota_stale().lock().await.clone();
    let now = chrono::Utc::now();
    let is_stale = |key: &str, x: &Quota| reading_is_stale(x, stale.contains(key), now);
    let mut m = serde_json::Map::new();
    for h in &hosts {
        for k in crate::config::KINDS {
            let key = quota_key(h, k);
            m.insert(key.clone(), q.get(&key).map(|x| quota_value(x, is_stale(&key, x))).unwrap_or(Value::Null));
        }
    }
    for (k, v) in q.iter() {
        // Otherwise read as a local key downstream.
        let orphan = k.contains('/') && host_of_key(k, &hosts).0 == LOCAL_HOST;
        if !orphan {
            m.insert(k.clone(), quota_value(v, is_stale(k, v)));
        }
    }
    json!({"kinds": Value::Object(m)})
}

/// codex 狀態列的剩餘量；沒有 `resets_at`，交給 [`set`] 沿用。
pub fn quota_from_codex_status(q: &crate::codex_status::CodexStatusQuota, account: Option<&str>) -> Option<Quota> {
    let win = |left: Option<f64>| left.map(|l| Window { observed_at: None, used_pct: (100.0 - l).clamp(0.0, 100.0), resets_at: None });
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
pub enum Sighting {
    New,
    Changed,
    Same,
}

/// 記在行程裡就夠：daemon 重啟後第一次看到的畫面算 [`Sighting::New`]，年紀另外用回合時間判斷。
pub async fn status_line_sighting(host: &str, pane_id: &str, line: &str) -> Sighting {
    static SEEN: std::sync::OnceLock<tokio::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>> =
        std::sync::OnceLock::new();
    let mut map = SEEN.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new())).lock().await;
    note_sighting(&mut map, format!("{host}:{pane_id}"), line, std::time::Instant::now())
}

/// 一格 pane 這個鍵多久沒被讀到就忘掉：pane 活著時每輪都會更新時間；收掉的 pane 不再回來，不清的話每個開過的 pane 留一格。
/// 忘掉的 pane 下次看到只是回到 [`Sighting::New`]（年紀另外用回合時間判斷）。
pub const SIGHTING_KEEP: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

pub fn note_sighting(map: &mut std::collections::HashMap<String, (String, std::time::Instant)>, key: String, line: &str, now: std::time::Instant) -> Sighting {
    map.retain(|_, (_, at)| now.duration_since(*at) < SIGHTING_KEEP);
    match map.insert(key, (line.to_string(), now)) {
        None => Sighting::New,
        Some((prev, _)) if prev == line => Sighting::Same,
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
pub fn pane_window_used(
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







/// 每個 quota key 的最新額度快照。（欄位在 top state，由 composition 層實作這個窄能力。）
pub trait QuotaTables: Send + Sync {
    fn quotas(&self) -> &tokio::sync::Mutex<std::collections::BTreeMap<String, crate::quota::Quota>>;
}

/// 讀數已過期（stale）的 quota key。（欄位在 top state，由 composition 層實作這個窄能力。）
pub trait QuotaStaleKeys: Send + Sync {
    fn quota_stale(&self) -> &tokio::sync::Mutex<std::collections::BTreeSet<String>>;
}

/// 身分表查詢介面，供額度收斂計算身分設定
pub trait HostIdentities: Send + Sync {
    fn identity_for_host(&self, host: &str, name: &str) -> impl std::future::Future<Output = Option<crate::config::IdentityCfg>> + Send;
    fn host_tools_detected(&self, host: &str) -> impl std::future::Future<Output = bool> + Send;
}

#[cfg(test)]
mod cache_write_lock_tests {
    use super::*;
    use std::future::Future;
    use std::path::{Path, PathBuf};

    struct TestApp {
        db: sqlx::SqlitePool,
        data_dir: PathBuf,
        quotas: tokio::sync::Mutex<std::collections::BTreeMap<String, Quota>>,
        stale: tokio::sync::Mutex<std::collections::BTreeSet<String>>,
    }

    impl crate::capabilities::Db for TestApp {
        fn db(&self) -> &sqlx::SqlitePool {
            &self.db
        }
    }

    impl crate::capabilities::DataDir for TestApp {
        fn data_dir(&self) -> &Path {
            &self.data_dir
        }
    }

    impl crate::capabilities::Emit for TestApp {
        fn emit(&self, _kind: &str, _data: Value) -> impl Future<Output = ()> + Send {
            async {}
        }

        fn current_seq(&self) -> u64 {
            0
        }
    }

    impl QuotaTables for TestApp {
        fn quotas(&self) -> &tokio::sync::Mutex<std::collections::BTreeMap<String, Quota>> {
            &self.quotas
        }
    }

    impl QuotaStaleKeys for TestApp {
        fn quota_stale(&self) -> &tokio::sync::Mutex<std::collections::BTreeSet<String>> {
            &self.stale
        }
    }

    async fn test_app() -> TestApp {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE quota_cache (key TEXT PRIMARY KEY, quota_json TEXT NOT NULL, updated_at TEXT NOT NULL)")
            .execute(&db)
            .await
            .unwrap();
        TestApp {
            db,
            data_dir: std::env::temp_dir().join("quota-cache-write-lock-test"),
            quotas: tokio::sync::Mutex::new(Default::default()),
            stale: tokio::sync::Mutex::new(Default::default()),
        }
    }

    fn quota_with_hit() -> Quota {
        Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: Some(LimitHit {
                message: "You've hit your usage limit".into(),
                until: Some(crate::db::iso_at(chrono::Utc::now() + chrono::Duration::days(7))),
                at: crate::db::now(),
                bucket: Some("seven_day".into()),
            }),
            plan: None,
            updated_at: crate::db::now(),
            source: "test".into(),
            account: None,
            host: LOCAL_HOST.into(),
        }
    }

    /// #1825：A 已取出撞限快照但延遲寫快取時，B 的成功回合清除必須排在 A 後面，且重載不能復活撞限。
    #[tokio::test]
    async fn clearing_a_limit_hit_serializes_behind_an_in_flight_cache_snapshot() {
        let app = Arc::new(test_app().await);
        let base = "codex:cache-write-race-unit";
        let key = quota_key(LOCAL_HOST, base);
        let before_clear = quota_with_hit();
        app.quotas.lock().await.insert(key.clone(), before_clear.clone());
        persist_cache(&app, &key, &before_clear).await;

        // A 已複製舊快照、仍持有 key 的寫入鎖，尚未落地；B 不能先改記憶體並寫清除快照。
        let a_write = cache_write_lock(&key).await;
        let a_snapshot = app.quotas.lock().await[&key].clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let clear_app = app.clone();
        let mut clear = tokio::spawn(async move {
            let _ = started_tx.send(());
            clear_limit_hit(&clear_app, LOCAL_HOST, base).await;
        });
        started_rx.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut clear).await.is_err(),
            "clear must wait until the older snapshot write releases its key lock"
        );
        assert!(app.quotas.lock().await[&key].limit_hit.is_some(), "clear cannot mutate memory ahead of the older write");
        persist_cache_locked(&app, &key, &a_snapshot).await;
        drop(a_write);
        clear.await.unwrap();

        assert!(app.quotas.lock().await[&key].limit_hit.is_none(), "清除已更新記憶體狀態");
        let mut older_reading = a_snapshot;
        older_reading.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(1));
        persist_cache(&app, &key, &older_reading).await;
        let cached: String = sqlx::query_scalar("SELECT quota_json FROM quota_cache WHERE key = ?")
            .bind(&key)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(serde_json::from_str::<Quota>(&cached).unwrap().limit_hit.is_none(), "較舊讀數不能覆蓋清除後的快取");

        app.quotas.lock().await.remove(&key);
        assert_eq!(load_cache(&app).await.unwrap(), 1);
        assert!(app.quotas.lock().await[&key].limit_hit.is_none(), "重載快取不能復活撞限");
    }
}

/// [`clear_limit_hit_if_older_than`]：時間界線、鎖內序列化、`cleared_at` 不倒退、其他欄位不動。
#[cfg(test)]
mod clear_if_older_tests {
    use super::*;
    use std::future::Future;
    use std::path::{Path, PathBuf};

    struct CleanApp {
        db: sqlx::SqlitePool,
        data_dir: PathBuf,
        quotas: tokio::sync::Mutex<std::collections::BTreeMap<String, Quota>>,
        stale: tokio::sync::Mutex<std::collections::BTreeSet<String>>,
        emits: std::sync::atomic::AtomicUsize,
    }

    impl crate::capabilities::Db for CleanApp {
        fn db(&self) -> &sqlx::SqlitePool {
            &self.db
        }
    }

    impl crate::capabilities::DataDir for CleanApp {
        fn data_dir(&self) -> &Path {
            &self.data_dir
        }
    }

    impl crate::capabilities::Emit for CleanApp {
        fn emit(&self, _kind: &str, _data: Value) -> impl Future<Output = ()> + Send {
            self.emits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {}
        }

        fn current_seq(&self) -> u64 {
            0
        }
    }

    impl QuotaTables for CleanApp {
        fn quotas(&self) -> &tokio::sync::Mutex<std::collections::BTreeMap<String, Quota>> {
            &self.quotas
        }
    }

    impl QuotaStaleKeys for CleanApp {
        fn quota_stale(&self) -> &tokio::sync::Mutex<std::collections::BTreeSet<String>> {
            &self.stale
        }
    }

    async fn app() -> CleanApp {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE quota_cache (key TEXT PRIMARY KEY, quota_json TEXT NOT NULL, updated_at TEXT NOT NULL)")
            .execute(&db)
            .await
            .unwrap();
        CleanApp {
            db,
            data_dir: std::env::temp_dir().join("quota-clear-if-older-test"),
            quotas: tokio::sync::Mutex::new(Default::default()),
            stale: tokio::sync::Mutex::new(Default::default()),
            emits: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc)
    }

    /// 帶一顆撞限（`at`）與一些其他欄位的讀數；其他欄位用來驗證清除時沒有被動到。
    fn with_hit(at: &str) -> Quota {
        Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 40.0, resets_at: Some("2026-10-21T05:00:00Z".into()) }),
            seven_day: Some(Window { observed_at: None, used_pct: 100.0, resets_at: Some("2026-10-26T23:42:00Z".into()) }),
            fable: None,
            reset_credits: None,
            limit_hit: Some(LimitHit {
                message: "You hit your weekly limit.".into(),
                until: Some("2026-10-26T23:42:00Z".into()),
                at: at.into(),
                bucket: Some("seven_day".into()),
            }),
            plan: Some("super".into()),
            updated_at: "2026-10-20T00:17:12Z".into(),
            source: "grok-limit-hit".into(),
            account: Some("acct".into()),
            host: LOCAL_HOST.into(),
        }
    }

    async fn seed(app: &CleanApp, base: &str, q: Quota) -> String {
        let key = quota_key(LOCAL_HOST, base);
        app.quotas.lock().await.insert(key.clone(), q);
        key
    }

    fn marked(app: &CleanApp, key: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        cleared_at().lock().unwrap().get(&cleared_at_key(app, key)).copied()
    }

    #[tokio::test]
    async fn an_older_hit_is_cleared_and_only_the_limit_field_changes() {
        let app = app().await;
        let key = seed(&app, "grok:older-clears", with_hit("2026-10-20T00:17:11.218Z")).await;
        let before = app.quotas.lock().await[&key].clone();
        let success = t("2026-10-21T00:10:00Z");

        assert!(clear_limit_hit_if_older_than(&app, LOCAL_HOST, "grok:older-clears", success).await);

        let after = app.quotas.lock().await[&key].clone();
        assert!(after.limit_hit.is_none(), "早於成功送達的撞限要清掉");
        assert_eq!(after.five_hour, before.five_hour, "清撞限不動 5h 窗");
        assert_eq!(after.seven_day, before.seven_day, "清撞限不動 7d 窗（28% 讀數也不拿來清）");
        assert_eq!(after.plan, before.plan);
        assert_eq!(after.updated_at, before.updated_at, "清撞限不改讀數時間");
        assert_eq!(after.source, before.source);
        assert_eq!(after.account, before.account);
        assert_eq!(marked(&app, &key), Some(success), "cleared_at 取成功送達時間");
        let cached: String = sqlx::query_scalar("SELECT quota_json FROM quota_cache WHERE key = ?").bind(&key).fetch_one(&app.db).await.unwrap();
        assert!(serde_json::from_str::<Quota>(&cached).unwrap().limit_hit.is_none(), "清除要寫進快取");
    }

    #[tokio::test]
    async fn an_equal_or_later_hit_is_kept_and_cleared_at_is_not_touched() {
        let app = app().await;
        let success = t("2026-10-21T00:10:00Z");

        let equal = seed(&app, "grok:equal-kept", with_hit("2026-10-21T00:10:00.000Z")).await;
        assert!(!clear_limit_hit_if_older_than(&app, LOCAL_HOST, "grok:equal-kept", success).await, "相等不清");
        assert!(app.quotas.lock().await[&equal].limit_hit.is_some());
        assert_eq!(marked(&app, &equal), None, "沒清掉就不寫 cleared_at");

        let later = seed(&app, "grok:later-kept", with_hit("2026-10-21T00:30:00Z")).await;
        assert!(!clear_limit_hit_if_older_than(&app, LOCAL_HOST, "grok:later-kept", success).await, "較新的撞限不清");
        assert!(app.quotas.lock().await[&later].limit_hit.is_some());
        assert_eq!(marked(&app, &later), None);
    }

    #[tokio::test]
    async fn no_hit_or_an_unreadable_time_changes_nothing() {
        let app = app().await;
        let success = t("2026-10-21T00:10:00Z");

        let mut no_hit = with_hit("2026-10-20T00:17:11Z");
        no_hit.limit_hit = None;
        let key = seed(&app, "grok:no-hit", no_hit.clone()).await;
        assert!(!clear_limit_hit_if_older_than(&app, LOCAL_HOST, "grok:no-hit", success).await);
        assert_eq!(app.quotas.lock().await[&key], no_hit, "沒有撞限：不動任何欄位");
        assert_eq!(marked(&app, &key), None, "沒有撞限：不寫 cleared_at");

        let bad = seed(&app, "grok:bad-at", with_hit("not-a-time")).await;
        assert!(!clear_limit_hit_if_older_than(&app, LOCAL_HOST, "grok:bad-at", success).await, "撞限時間解不開：不清");
        assert!(app.quotas.lock().await[&bad].limit_hit.is_some());
        assert_eq!(marked(&app, &bad), None);

        assert!(!clear_limit_hit_if_older_than(&app, LOCAL_HOST, "grok:never-read", success).await, "沒有這一格的讀數");
    }

    #[tokio::test]
    async fn the_comparison_is_by_instant_not_by_text() {
        let app = app().await;
        // 同一時刻、不同 offset 與小數位數：相等，不清。
        seed(&app, "grok:offset-equal", with_hit("2026-10-20T02:17:11.218+02:00")).await;
        assert!(!clear_limit_hit_if_older_than(&app, LOCAL_HOST, "grok:offset-equal", t("2026-10-20T00:17:11.218Z")).await, "+02:00 的同一時刻要當相等");

        // 字串上 `08:00+09:00` 比 `00:00Z` 大，但實際是 23:00Z（較早）：要清。
        let earlier = seed(&app, "grok:offset-earlier", with_hit("2026-10-20T08:00:00+09:00")).await;
        assert!(clear_limit_hit_if_older_than(&app, LOCAL_HOST, "grok:offset-earlier", t("2026-10-20T00:00:00Z")).await, "應按時刻比，不按字串");
        assert!(app.quotas.lock().await[&earlier].limit_hit.is_none());

        // 小數位數：`.2185` 比 `.218` 晚 0.5 毫秒，不清。
        seed(&app, "grok:precision", with_hit("2026-10-20T00:17:11.2185Z")).await;
        assert!(!clear_limit_hit_if_older_than(&app, LOCAL_HOST, "grok:precision", t("2026-10-20T00:17:11.218Z")).await, "較晚的小數不能被當成更早");
    }

    #[tokio::test]
    async fn cleared_at_never_moves_backwards() {
        let app = app().await;
        let base = "grok:monotone";
        let key = seed(&app, base, with_hit("2026-10-20T00:17:11Z")).await;
        let late = t("2026-10-21T12:00:00Z");
        assert!(clear_limit_hit_if_older_than(&app, LOCAL_HOST, base, late).await);
        assert_eq!(marked(&app, &key), Some(late));

        // 新撞限在 `earlier` 之前就命中，`earlier` 這次成功比上一次早：清得掉，但 cleared_at 仍是 `late`。
        let earlier = t("2026-10-21T06:00:00Z");
        app.quotas.lock().await.insert(key.clone(), with_hit("2026-10-21T01:00:00Z"));
        assert!(clear_limit_hit_if_older_than(&app, LOCAL_HOST, base, earlier).await);
        assert_eq!(marked(&app, &key), Some(late), "cleared_at 只能往後推，不能倒退");
    }

    /// #1825 型競爭：舊快照寫入持有 key 鎖時，清除要等它；它寫進的新撞限必須在鎖內被看見、不被清掉。
    #[tokio::test]
    async fn a_newer_hit_written_under_the_lock_is_not_cleared_by_an_older_success() {
        let app = std::sync::Arc::new(app().await);
        let base = "grok:race-newer-hit";
        let key = seed(&app, base, with_hit("2026-10-20T00:17:11Z")).await;
        let success = t("2026-10-21T00:10:00Z");

        // 寫入端先拿鎖，把撞限換成比 `success` 還新的一顆，然後才放鎖。
        let writer_lock = cache_write_lock(&key).await;
        let mut newer = with_hit("2026-10-21T00:20:00Z");
        newer.source = "race-writer".into();
        app.quotas.lock().await.insert(key.clone(), newer.clone());

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let clear_app = app.clone();
        let mut clear = tokio::spawn(async move {
            let _ = started_tx.send(());
            clear_limit_hit_if_older_than(&*clear_app, LOCAL_HOST, base, success).await
        });
        started_rx.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut clear).await.is_err(),
            "判斷必須排在寫入端的鎖後面，不能在鎖外先讀到舊撞限"
        );
        drop(writer_lock);

        assert!(!clear.await.unwrap(), "鎖內看到的是較新的撞限，不能清");
        assert_eq!(app.quotas.lock().await[&key], newer, "新撞限與其他欄位原樣保留");
        assert_eq!(marked(&app, &key), None, "沒清掉就不推 cleared_at");
    }

    async fn db_row(app: &CleanApp, key: &str) -> (String, String) {
        sqlx::query_as("SELECT quota_json, updated_at FROM quota_cache WHERE key = ?").bind(key).fetch_one(&app.db).await.unwrap()
    }

    async fn db_limit_hit(app: &CleanApp, key: &str) -> Option<LimitHit> {
        serde_json::from_str::<Quota>(&db_row(app, key).await.0).unwrap().limit_hit
    }

    /// #1825 形狀：寫入端持鎖複製一份**有撞限**的舊快照 → 條件清除排隊等鎖 → 寫入端把舊快照落地後放鎖 → 清除完成。
    /// 清除之後 DB 與重載都不能復活撞限；較舊讀數晚到的持久化要被拒。
    #[tokio::test]
    async fn a_snapshot_copied_under_the_lock_cannot_resurrect_a_cleared_hit() {
        let app = std::sync::Arc::new(app().await);
        let base = "grok:snapshot-under-lock";
        let key = seed(&app, base, with_hit("2026-10-20T00:17:11Z")).await;
        let seeded = app.quotas.lock().await[&key].clone();
        persist_cache(&*app, &key, &seeded).await;
        let success = t("2026-10-21T00:10:00Z");

        let writer_lock = cache_write_lock(&key).await;
        let snapshot = app.quotas.lock().await[&key].clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let clear_app = app.clone();
        let mut clear = tokio::spawn(async move {
            let _ = started_tx.send(());
            clear_limit_hit_if_older_than(&*clear_app, LOCAL_HOST, base, success).await
        });
        started_rx.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut clear).await.is_err(),
            "清除必須等寫入端的鎖"
        );
        persist_cache_locked(&*app, &key, &snapshot).await;
        drop(writer_lock);

        assert!(clear.await.unwrap(), "舊撞限早於成功送達，要清");
        assert!(app.quotas.lock().await[&key].limit_hit.is_none(), "記憶體已清");
        assert!(db_limit_hit(&app, &key).await.is_none(), "DB 已清");
        assert_eq!(app.emits.load(std::sync::atomic::Ordering::SeqCst), 1, "清掉時只發一次 quota_updated");

        let mut older = snapshot.clone();
        older.updated_at = crate::db::iso_at(t("2026-10-20T00:10:00Z"));
        persist_cache(&*app, &key, &older).await;
        assert!(db_limit_hit(&app, &key).await.is_none(), "較舊讀數晚到的持久化要被拒，撞限不能寫回 DB");

        app.quotas.lock().await.remove(&key);
        assert_eq!(load_cache(&*app).await.unwrap(), 1);
        assert!(app.quotas.lock().await[&key].limit_hit.is_none(), "重載快取不能復活撞限");
    }

    /// 不清掉任何東西的情況（沒有撞限、撞限不早於成功、時間解不開）：不 emit、不寫 DB、不寫 cleared_at、不改記憶體。
    /// DB 用哨兵列確認；任何一次寫入都會把哨兵覆蓋掉。
    #[tokio::test]
    async fn a_call_that_clears_nothing_has_no_side_effects() {
        let app = app().await;
        let success = t("2026-10-21T00:10:00Z");
        let mut no_hit = with_hit("2026-10-20T00:17:11Z");
        no_hit.limit_hit = None;
        let cases = [
            ("grok:fx-no-hit", no_hit),
            ("grok:fx-equal", with_hit("2026-10-21T00:10:00.000Z")),
            ("grok:fx-later", with_hit("2026-10-21T00:30:00Z")),
            ("grok:fx-bad-at", with_hit("not-a-time")),
        ];
        for (base, q) in cases {
            let key = seed(&app, base, q.clone()).await;
            sqlx::query("INSERT OR REPLACE INTO quota_cache (key, quota_json, updated_at) VALUES (?, 'SENTINEL', 'SENTINEL')")
                .bind(&key)
                .execute(&app.db)
                .await
                .unwrap();
            let emits_before = app.emits.load(std::sync::atomic::Ordering::SeqCst);

            assert!(!clear_limit_hit_if_older_than(&app, LOCAL_HOST, base, success).await, "{base} 不該清");

            assert_eq!(app.emits.load(std::sync::atomic::Ordering::SeqCst), emits_before, "{base} 不該 emit");
            assert_eq!(db_row(&app, &key).await, ("SENTINEL".into(), "SENTINEL".into()), "{base} 不該寫 DB");
            assert_eq!(app.quotas.lock().await[&key], q, "{base} 不改記憶體");
            assert_eq!(marked(&app, &key), None, "{base} 不寫 cleared_at");
        }
    }
}
