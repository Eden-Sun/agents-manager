//! Rate-limit quota per host + kind (`GET /api/quota`, WS `quota_updated`); sources: codex
//! app-server, claude statusLine + [`crate::quota_claude`] probe, grok [`crate::quota_grok`].
//! Keys are host-scoped (SPEC §14): bare on local, `<host>/…` remote — a remote bot's statusLine
//! must never land on the local row.

use crate::config::LOCAL_HOST;
use crate::state::App;
use anyhow::Result;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
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
    // 時間戳一律走 [`parse_utc`]（#518 收口：散落的 `parse_from_rfc3339` 收成一支，
    // 才不會有人再寫出一個方向不一樣的解析）。
    let future = |t: &Option<String>| t.as_deref().and_then(parse_utc).filter(|x| *x > now);
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
pub async fn load_cache(app: &Arc<App>) -> Result<usize> {
    let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT key, quota_json, updated_at FROM quota_cache")
        .fetch_all(&app.db)
        .await?;
    let mut restored = Vec::new();
    for (key, raw, updated_at) in rows {
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
    let mut quotas = app.quotas.lock().await;
    let mut stale = app.quota_stale.lock().await;
    for (key, q) in restored {
        quotas.insert(key.clone(), q);
        stale.insert(key);
    }
    Ok(count)
}

async fn persist_cache(app: &Arc<App>, key: &str, q: &Quota) {
    let raw = match serde_json::to_string(q) {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(key, error = %e, "cannot serialize quota cache");
            return;
        }
    };
    if let Err(e) = sqlx::query(
        "INSERT INTO quota_cache (key, quota_json, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET quota_json=excluded.quota_json, updated_at=excluded.updated_at
         WHERE excluded.updated_at >= quota_cache.updated_at",
    )
    .bind(key)
    .bind(raw)
    .bind(&q.updated_at)
    .execute(&app.db)
    .await
    {
        tracing::warn!(key, error = %e, "cannot persist quota cache");
    }
}

async fn delete_cache(app: &Arc<App>, key: &str) {
    if let Err(e) = sqlx::query("DELETE FROM quota_cache WHERE key = ?").bind(key).execute(&app.db).await {
        tracing::warn!(key, error = %e, "cannot delete quota cache");
    }
}

/// Remove a quota key that no longer belongs to a live identity/host, including its restart cache.
pub async fn forget(app: &Arc<App>, key: &str) {
    app.quotas.lock().await.remove(key);
    app.quota_stale.lock().await.remove(key);
    delete_cache(app, key).await;
}

pub(crate) fn quota_value(q: &Quota, stale: bool) -> Value {
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

fn unix_to_rfc3339(v: Option<&Value>) -> Option<String> {
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

/// 外部探測的結果只在探測開始時記下的 `fence` 仍是這台主機的權威時才發布（#347）：探測途中同名主機被重連／改指到
/// 另一台、或被移除，舊機器的額度就不能寫進新機器（或已移除主機）的 key。不是權威就回 `Err`、什麼都不寫。
pub async fn set_fenced(app: &Arc<App>, host: &str, base: &str, q: Quota, fence: &crate::hosts::HostFence) -> Result<()> {
    if !app.hosts.is_current(fence).await {
        anyhow::bail!("host `{host}` was reconnected/reconfigured during the quota probe; stale reading discarded");
    }
    set(app, host, base, q).await;
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

pub async fn set(app: &Arc<App>, host: &str, base: &str, mut q: Quota) {
    q.host = host.to_string();
    let key = quota_key(host, base);
    let stale = if base.contains(':') { Vec::new() } else { stale_split_keys(app, host, base).await };
    // 撞限校正只看**這份讀數自己帶來的**窗；下面沿用的舊窗不是新證據。
    let brings_its_own_hit = q.limit_hit.is_some();
    let fresh = q.clone();
    let mut quotas = app.quotas.lock().await;
    if q.source == "statusline" {
        if let Some(previous) = quotas.get(&key) {
            if let Some((bucket, previous_reset, incoming_reset)) = guard_statusline_windows(previous, &mut q, chrono::Utc::now()) {
                tracing::debug!(host, key, bucket, previous_reset, incoming_reset, "discarded a stale Claude statusline quota snapshot");
                return;
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
        let mut stale_flags = app.quota_stale.lock().await;
        stale_flags.remove(&key);
        for old in &stale {
            stale_flags.remove(old);
        }
    }
    for old in &stale {
        delete_cache(app, old).await;
    }
    persist_cache(app, &key, &q).await;
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": quota_value(&q, false)})).await;
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
    // 解不開的 `until` **不**算過期：這裡的「不過期」是**繼續擋**（嚴格的那一邊），
    // 跟 `already_past` 的嚴格方向一致，不是 `reset_passed` 那種「放行」。
    match parse_utc(until) {
        Some(t) => chrono::Utc::now() >= t,
        None => false,
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
    crate::judge::note_cleared(&app.db, &bot.id).await;
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
    let stale = app.quota_stale.lock().await.contains(&key);
    persist_cache(app, &key, &out).await;
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": quota_value(&out, stale)})).await;
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
    let Some(t) = parse_utc(until) else { return false };
    if t <= chrono::Utc::now() {
        return false;
    }
    let key = quota_key(host, base);
    let mut quotas = app.quotas.lock().await;
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
    let stale = app.quota_stale.lock().await.contains(&key);
    persist_cache(app, &key, &out).await;
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": quota_value(&out, stale)})).await;
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
    let stale = app.quota_stale.lock().await.contains(&key);
    persist_cache(app, &key, &out).await;
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": quota_value(&out, stale)})).await;
    true
}

/// Base kinds always present per host (empty bars before first report); orphan-host keys dropped.
pub async fn snapshot(app: &Arc<App>) -> Value {
    let hosts = app.hosts.names().await;
    let q = app.quotas.lock().await.clone();
    let stale = app.quota_stale.lock().await.clone();
    let mut m = serde_json::Map::new();
    for h in &hosts {
        for k in crate::config::KINDS {
            let key = quota_key(h, k);
            m.insert(key.clone(), q.get(&key).map(|x| quota_value(x, stale.contains(&key))).unwrap_or(Value::Null));
        }
    }
    for (k, v) in q.iter() {
        // Otherwise read as a local key downstream.
        let orphan = k.contains('/') && host_of_key(k, &hosts).0 == LOCAL_HOST;
        if !orphan {
            m.insert(k.clone(), quota_value(v, stale.contains(k)));
        }
    }
    json!({"kinds": Value::Object(m)})
}

/// codex 狀態列的剩餘量；沒有 `resets_at`，交給 [`set`] 沿用。
pub fn quota_from_codex_status(q: &crate::codex_live::CodexStatusQuota, account: Option<&str>) -> Option<Quota> {
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
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow::anyhow!("unknown host `{host}`"))?;
    let r = crate::models::codex_rpc(app, host, "account/rateLimits/read", json!({})).await;
    let r = match r {
        Ok(v) => v,
        Err(e) if e.to_string().contains("is not installed") => return Ok(false),
        Err(e) => return Err(e),
    };
    match quota_from_codex(&r) {
        Some(q) => {
            set_fenced(app, host, "codex", q, &fence).await?;
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

    /// issue #464（i407 review）：`Window::exhausted_at` 是三處共用的那一份判斷，
    /// 其中「解不開的時間戳當成已重置」沿用 `supervisor::policy::past` 的先例。
    #[test]
    fn a_window_is_exhausted_only_while_its_window_is_still_open() {
        let t = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc);
        let now = t("2026-09-13T12:00:00Z");
        let w = |used: f64, resets: Option<&str>| Window { observed_at: None, used_pct: used, resets_at: resets.map(String::from) };

        assert!(w(100.0, Some("2026-09-13T15:00:00Z")).exhausted_at(now), "見底、窗還沒到 → 用盡");
        assert!(!w(100.0, Some("2026-09-13T10:00:00Z")).exhausted_at(now), "見底但窗兩小時前就重置了 → 不算用盡");
        assert!(!w(10.0, Some("2026-09-13T15:00:00Z")).exhausted_at(now), "沒見底就不是用盡");
        // 解不開＝已重置（不永久擋）；沒有時間＝不知道（繼續擋）。
        assert!(!w(100.0, Some("not-a-timestamp")).exhausted_at(now), "壞掉的時間戳不該把身分永久排除");
        assert!(w(100.0, None).exhausted_at(now), "沒有重置時間就無從判斷，保守繼續擋");
        assert!(w(100.0, Some("not-a-timestamp")).reset_passed(now));
        assert!(!w(100.0, None).reset_passed(now));
    }

    /// issue #489（我 #464 帶出來的回歸）：走**真的 `set()` 路徑**。
    ///
    /// app-server 探測成功一次寫下未來的 `resets_at` → 時間跨過它、探測不再成功 → 之後只有 codex 狀態列
    /// 進來（建構時 `resets_at: None`、`used_pct` 見底）。只看「重置時刻在過去」的話，那筆繼承來的舊時刻
    /// 會把新鮮的見底讀數判成「已重置」→ 身分被當成有額度。
    #[tokio::test]
    async fn a_fresh_critical_statusline_is_not_excused_by_an_inherited_past_reset() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "codex");
        // 1. app-server：還有額度，重置時間在「一小時前」（模擬那次探測之後時間就跨過去了）。
        let mut probe = codex_q("codex-app-server", None);
        probe.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(3));
        probe.seven_day = Some(Window {
            used_pct: 20.0,
            resets_at: Some(crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(1))),
            observed_at: None,
        });
        set(&app, LOCAL_HOST, "codex", probe).await;

        // 2. 之後只有狀態列：真的見底、沒有 resets_at。這一刻才觀測到。
        let mut status = codex_q("codex-statusline", None);
        status.updated_at = crate::db::now();
        status.seven_day = Some(Window { used_pct: 97.0, resets_at: None, observed_at: None });
        set(&app, LOCAL_HOST, "codex", status).await;

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let now = chrono::Utc::now();
        assert_eq!(got.seven_day.as_ref().map(|w| w.used_pct), Some(97.0), "前提：新讀數真的進去了");
        assert!(
            got.exhausted(Bucket::SevenDay, now),
            "剛讀到的 97% 不能因為繼承了一個過去的重置時間就被放行：{:?}",
            got.seven_day
        );
        // 順帶：已經過去的重置時間本來就不該被沿用（對顯示也沒意義）。
        assert_eq!(got.seven_day.as_ref().and_then(|w| w.resets_at.clone()), None, "過去的 resets_at 不沿用");
    }

    /// i204 review（#489）：**解不開**的 `resets_at` 走的是票上那條原路，而且會一直黏著——
    /// `already_past` 第一版說它「沒過去」所以照樣沿用，`reset_passed` 又說它「已經跨過重置」，
    /// 於是那個身分從此永遠看起來有額度。用票上那個兩輪 `set()` 的重現釘住。
    #[tokio::test]
    async fn an_unparseable_reset_time_is_not_carried_forward_and_does_not_excuse_a_critical_reading() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "codex");

        // 第一輪：某個來源帶進一個解不開的 resets_at。
        let mut first = codex_q("codex-app-server", None);
        first.updated_at = crate::db::now();
        first.seven_day = Some(Window { used_pct: 20.0, resets_at: Some("not-a-timestamp".into()), observed_at: None });
        set(&app, LOCAL_HOST, "codex", first).await;

        // 第二輪：狀態列讀數（沒有自己的 resets_at）而且真的見底。
        let mut status = codex_q("codex-statusline", None);
        status.updated_at = crate::db::now();
        status.seven_day = Some(Window { used_pct: 97.0, resets_at: None, observed_at: None });
        set(&app, LOCAL_HOST, "codex", status).await;

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let w = got.seven_day.as_ref().unwrap();
        assert_eq!(w.used_pct, 97.0, "前提：新讀數進去了");
        assert_eq!(w.resets_at, None, "解不開的重置時間不該被沿用下去");
        assert!(got.exhausted(Bucket::SevenDay, chrono::Utc::now()), "97% 用掉不能因為一個解不開的時間戳就被放行");
    }

    /// i267 review（#518）：`resets_at` **與** `observed_at` 都解不開時，身分不可以被永久擋住。
    ///
    /// 這是前一顆的鏡像洞：`resets_at` 被丟成 `None` 之後就交給窗長規則收尾，但那條規則的輸入
    /// （`observed_at`，沒有就退回 `updated_at`）第一版對解不開的回 `false`＝「還很新」，
    /// 於是窗長永遠收不掉、`reset_passed` 回 false、`exhausted` 永遠 true——#475 標題那個問題原封不動回來。
    /// 既有的 `boot_drops_an_unparseable_reset_time_from_the_cache` 用的是**可解析**的 `observed_at`，走不到這裡。
    #[tokio::test]
    async fn a_reading_whose_every_timestamp_is_broken_does_not_block_forever() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let key = quota_key(LOCAL_HOST, "claude");
        let mut q = codex_q("boot", None);
        q.updated_at = crate::db::now();
        q.five_hour = Some(Window { used_pct: 100.0, resets_at: Some("garbage".into()), observed_at: Some("also-garbage".into()) });
        sqlx::query("INSERT INTO quota_cache (key, quota_json, updated_at) VALUES (?,?,?)")
            .bind(&key)
            .bind(serde_json::to_string(&q).unwrap())
            .bind("not-a-timestamp") // 連整筆的 updated_at 都壞掉
            .execute(&app.db)
            .await
            .unwrap();

        load_cache(&app).await.unwrap();
        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let five = got.five_hour.as_ref().expect("窗留著");
        assert_eq!(five.resets_at, None, "壞的重置時間丟掉");
        assert_eq!(five.observed_at, None, "壞的觀測時間也丟掉");
        // 三個時間戳全壞 → 這筆讀數說不出年齡，不能拿它永久擋住一個身分。
        assert!(!got.exhausted(Bucket::FiveHour, chrono::Utc::now()), "全壞的讀數不可以永久算用盡");
        assert!(got.usable_window(Bucket::FiveHour, chrono::Utc::now()).is_none(), "當成沒有讀數");
    }

    /// 年齡判斷對解不開的時間戳要當「已過期」，而不是「還很新」（#518 的純函式那一格）。
    #[test]
    fn an_unparseable_observation_time_counts_as_expired_not_fresh() {
        let now = chrono::Utc::now();
        let mk = |observed: Option<&str>, updated: &str| Quota {
            five_hour: Some(Window { used_pct: 100.0, resets_at: None, observed_at: observed.map(String::from) }),
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: updated.into(),
            source: "test".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        let fresh = crate::db::iso_at(now);
        // 正常：剛觀測到、見底 → 算用盡。
        assert!(mk(Some(&fresh), &fresh).exhausted(Bucket::FiveHour, now));
        // observed_at 解不開 → 退回 updated_at（還新）→ 仍算用盡。
        assert!(mk(Some("garbage"), &fresh).exhausted(Bucket::FiveHour, now));
        // 兩個都解不開 → 說不出年齡 → 不算用盡（不永久擋人）。
        assert!(!mk(Some("garbage"), "also-garbage").exhausted(Bucket::FiveHour, now));
        assert!(!mk(None, "also-garbage").exhausted(Bucket::FiveHour, now));
    }

    /// i264 review（#489）：`load_cache` 是裸的 `serde_json::from_str`，所以快取裡解不開的 `resets_at`
    /// 原樣載回來，`reset_passed` 讀成「已重置」→ 那個身分載回來就看起來有額度。載入時要驗一次。
    #[tokio::test]
    async fn boot_drops_an_unparseable_reset_time_from_the_cache() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let key = quota_key(LOCAL_HOST, "claude");
        let mut q = codex_q("boot", None);
        q.updated_at = crate::db::now();
        // 見底 ＋ 解不開的重置時間：載回來不能變成「有額度」。
        q.five_hour = Some(Window { used_pct: 100.0, resets_at: Some("not-a-timestamp".into()), observed_at: Some(crate::db::now()) });
        // 好的那個要留著。
        let good = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::days(3));
        q.seven_day = Some(Window { used_pct: 10.0, resets_at: Some(good.clone()), observed_at: Some(crate::db::now()) });
        sqlx::query("INSERT INTO quota_cache (key, quota_json, updated_at) VALUES (?,?,?)")
            .bind(&key)
            .bind(serde_json::to_string(&q).unwrap())
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();

        load_cache(&app).await.unwrap();
        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let five = got.five_hour.as_ref().expect("窗本身留著，只是沒有重置時間");
        assert_eq!(five.resets_at, None, "解不開的重置時間載回來時要丟掉");
        assert!(!five.reset_passed(chrono::Utc::now()), "沒有重置時間＝不知道，不是「已重置」");
        assert!(got.exhausted(Bucket::FiveHour, chrono::Utc::now()), "見底的讀數不能因為一個壞時間戳就被放行");
        assert_eq!(got.seven_day.as_ref().and_then(|w| w.resets_at.clone()), Some(good), "解得開的不動");
    }

    /// `unix_to_rfc3339` 的字串分支要驗過格式才放行（i204 review，#489）：
    /// 原本是原樣回傳，所以格式漂移時亂碼會直接變成 `resets_at`。
    #[test]
    fn a_reset_time_string_must_parse_before_it_is_accepted() {
        let at = |v: serde_json::Value| unix_to_rfc3339(Some(&v));
        // 正常的 RFC3339 照收，原樣留著。
        assert_eq!(at(json!("2026-09-10T00:26:40Z")), Some("2026-09-10T00:26:40Z".into()));
        // 帶時區位移的也收，原樣留著（下游一律比時刻，不比字串）。
        assert_eq!(at(json!("2026-09-10T08:26:40+08:00")), Some("2026-09-10T08:26:40+08:00".into()));
        // 解不開的丟掉，不要變成 resets_at。
        assert_eq!(at(json!("not-a-timestamp")), None);
        assert_eq!(at(json!("2026-13-99T99:99:99Z")), None);
        assert_eq!(at(json!("")), None);
        // 數字分支不受影響（秒與毫秒）。
        assert_eq!(at(json!(1_789_000_000)), Some("2026-09-10T00:26:40.000Z".into()));
        assert_eq!(at(json!(1_789_000_000_000i64)), Some("2026-09-10T00:26:40.000Z".into()));
    }

    /// #489 的第二條路：讀數**自己就帶著**一個剛過去的 `resets_at`（app-server 在窗剛翻過去時
    /// 回的就是這種），沿用那一段完全沒參與。這條專門釘 `reset_passed` 的觀測時間條件——
    /// 只靠「不沿用過期的 `resets_at`」擋不到它。
    ///
    /// （第一版我只寫了走沿用那條，結果變異「`reset_passed` 不看觀測時間」殺不掉它：
    /// 沿用那一半先把過期時刻丟了，根本走不到這個判斷。兩個守衛各自夠用，就得各自有測試。）
    #[tokio::test]
    async fn a_freshly_observed_critical_window_with_its_own_past_reset_still_blocks() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "codex");
        let mut probe = codex_q("codex-app-server", None);
        probe.updated_at = crate::db::now();
        // 窗剛翻過去一分鐘，而這一刻讀到的就是 97% 用掉。
        probe.seven_day = Some(Window {
            used_pct: 97.0,
            resets_at: Some(crate::db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(1))),
            observed_at: None,
        });
        set(&app, LOCAL_HOST, "codex", probe).await;

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let w = got.seven_day.as_ref().expect("讀數自己帶的 resets_at 不受沿用規則影響");
        assert!(w.resets_at.is_some(), "前提：這個過期時刻是讀數自己帶的，不是沿用來的");
        assert!(!w.reset_passed(chrono::Utc::now()), "觀測時間比重置新 → 不算跨過重置");
        assert!(got.exhausted(Bucket::SevenDay, chrono::Utc::now()), "剛讀到的 97% 要算用盡");
    }

    /// 反方向要照舊成立（#464 修的那個）：**觀測時間早於重置**的舊讀數仍然算「已重置」，不能永久擋人。
    #[test]
    fn a_reading_observed_before_the_reset_still_counts_as_reset() {
        let t = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc);
        let now = t("2026-09-13T12:00:00Z");
        let w = |observed: Option<&str>| Window {
            used_pct: 100.0,
            resets_at: Some("2026-09-13T10:00:00Z".into()),
            observed_at: observed.map(String::from),
        };
        // 觀測在重置之前 → 這筆讀數跨過了重置 → 不算用盡（#464）。
        assert!(w(Some("2026-09-13T09:00:00Z")).reset_passed(now));
        assert!(!w(Some("2026-09-13T09:00:00Z")).exhausted_at(now));
        // 剛好等於重置時刻也算跨過（邊界）。
        assert!(w(Some("2026-09-13T10:00:00Z")).reset_passed(now));
        // 觀測在重置之後 → 它已經反映重置後的狀態，說見底就是見底（#489）。
        assert!(!w(Some("2026-09-13T11:00:00Z")).reset_passed(now));
        assert!(w(Some("2026-09-13T11:00:00Z")).exhausted_at(now));
        // 完全不知道觀測時間 → 退回只看重置時刻（＝#464 的行為；舊快取列就是這種，本來就是舊讀數）。
        // 刻意不拿 `Quota::updated_at` 當備援，理由見 `reset_passed` 的註解。
        assert!(w(None).reset_passed(now));
        assert!(!w(None).exhausted_at(now));
        // 重置還沒到 → 無論觀測時間都不算跨過。
        let future = Window { used_pct: 100.0, resets_at: Some("2026-09-13T15:00:00Z".into()), observed_at: Some("2026-09-13T11:00:00Z".into()) };
        assert!(!future.reset_passed(now));
        assert!(future.exhausted_at(now));
    }

    /// issue #475（i267 review）：年齡要跟著**窗**走，不是跟著整筆讀數走。
    ///
    /// 走真的 `set()` 路徑：先送一筆「5h 見底、沒有 resets_at」，之後 statusline 一直只帶 7d
    /// （`set` 會把舊的 5h 原樣沿用，而 `updated_at` 蓋成現在）。只看 `updated_at` 的話那筆 5h
    /// 年齡永遠是 0，窗長到期永遠不成立——這條會紅。
    #[tokio::test]
    async fn a_carried_over_window_keeps_its_own_age_across_repeated_statuslines() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "claude");
        let win = |used: f64| Some(Window { used_pct: used, resets_at: None, observed_at: None });

        // 第一筆：5h 見底、7d 還有，兩個都沒有 resets_at。觀測時間是 6 小時前。
        let mut first = codex_q("statusline", None);
        first.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(6));
        first.five_hour = win(100.0);
        first.seven_day = win(10.0);
        set(&app, LOCAL_HOST, "claude", first).await;

        // 之後 statusline 只帶 7d（被截斷）：5h 被 `set` 沿用，`updated_at` 是現在。
        for _ in 0..3 {
            let mut later = codex_q("statusline", None);
            later.updated_at = crate::db::now();
            later.five_hour = None;
            later.seven_day = win(10.0);
            set(&app, LOCAL_HOST, "claude", later).await;
        }

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        assert_eq!(got.five_hour.as_ref().map(|w| w.used_pct), Some(100.0), "前提：5h 真的被沿用了");
        assert!(got.updated_at > crate::db::iso_at(chrono::Utc::now() - chrono::Duration::minutes(1)), "前提：整筆的 updated_at 是現在");
        let observed = got.five_hour.as_ref().and_then(|w| w.observed_at.clone()).expect("沿用的窗要帶著自己的觀測時間");
        assert!(observed < crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(5)), "觀測時間要留在 6 小時前：{observed}");

        let now = chrono::Utc::now();
        assert!(!got.exhausted(Bucket::FiveHour, now), "5h 的讀數 6 小時前觀測、又沒有 resets_at → 不算用盡");
        assert!(got.usable_window(Bucket::FiveHour, now).is_none(), "當成沒有讀數");
        // 7d 每次都真的帶進來，年齡是現在，照舊算數。
        assert!(got.usable_window(Bucket::SevenDay, now).is_some());
    }

    /// 撞限記錄（繞過 `set`、只改 `limit_hit` 與 `updated_at`）不該把窗的年齡重設。
    #[tokio::test]
    async fn recording_a_limit_hit_does_not_reset_a_windows_age() {
        let app = crate::testing::env().await.app.clone();
        let key = quota_key(LOCAL_HOST, "claude");
        let mut first = codex_q("statusline", None);
        first.updated_at = crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(6));
        first.five_hour = Some(Window { used_pct: 100.0, resets_at: None, observed_at: None });
        first.seven_day = Some(Window { used_pct: 10.0, resets_at: None, observed_at: None });
        set(&app, LOCAL_HOST, "claude", first).await;

        let until = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(1));
        seed_limit_hit(&app, LOCAL_HOST, "claude", &until, "You've reached your limit", Some("five_hour".into())).await;

        let got = app.quotas.lock().await.get(&key).cloned().unwrap();
        let observed = got.five_hour.as_ref().and_then(|w| w.observed_at.clone()).expect("窗還在，觀測時間也還在");
        assert!(observed < crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(5)), "撞限記錄不該把年齡重設：{observed}");
    }

    /// issue #475（i266 review）：`resets_at` 是 `None` 的見底讀數在 `exhausted_at` 眼裡永遠用盡，
    /// 沒有任何時間能讓它翻回來。這條規則以前只在 `load_cache` 開機跑一次，所以同一次 uptime 內
    /// 照樣永久卡住——而這台 daemon 常連跑好幾天。現在每次判斷都跑。
    #[test]
    fn a_reading_older_than_its_window_stops_counting_as_exhausted() {
        let now = chrono::Utc::now();
        let q = |age: chrono::Duration, resets: Option<String>| {
            let mut x = Quota {
                five_hour: Some(Window { observed_at: None, used_pct: 100.0, resets_at: resets }),
                seven_day: None,
                fable: None,
                reset_credits: None,
                limit_hit: None,
                plan: None,
                updated_at: crate::db::iso_at(now - age),
                source: "test".into(),
                account: None,
                host: LOCAL_HOST.into(),
            };
            x.seven_day = Some(Window { observed_at: None, used_pct: 10.0, resets_at: None });
            x
        };
        // 沒有 resets_at：窗長（5h）之內照舊算用盡，超過就不算。
        assert!(q(chrono::Duration::hours(1), None).exhausted(Bucket::FiveHour, now), "1 小時前的讀數還算數");
        assert!(!q(chrono::Duration::hours(6), None).exhausted(Bucket::FiveHour, now), "6 小時前＋沒有重置時間 → 必定跨過一次重置");
        // 回的是「沒有讀數」，不是「沒見底」：`responder` 靠這個分辨「不知道」與「有額度」。
        assert!(q(chrono::Duration::hours(6), None).usable_window(Bucket::FiveHour, now).is_none());
        assert!(q(chrono::Duration::hours(1), None).usable_window(Bucket::FiveHour, now).is_some());

        // 有 resets_at 就**不套**這條：7d 的讀數本來就可能好幾天前更新、窗卻還沒到。
        let future = Some(crate::db::iso_at(now + chrono::Duration::hours(2)));
        assert!(q(chrono::Duration::hours(6), future).exhausted(Bucket::FiveHour, now), "有重置時間就交給 reset_passed 判");
    }

    /// 窗長跟著桶走：同一筆 6 小時前的讀數，對 5h 桶算過期、對 7d／Fable 桶不算。
    #[test]
    fn the_window_length_follows_the_bucket() {
        let now = chrono::Utc::now();
        let full = Window { observed_at: None, used_pct: 100.0, resets_at: None };
        let q = Quota {
            five_hour: Some(full.clone()),
            seven_day: Some(full.clone()),
            fable: Some(full),
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::iso_at(now - chrono::Duration::hours(6)),
            source: "test".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        assert!(!q.exhausted(Bucket::FiveHour, now), "6 小時 > 5 小時窗");
        assert!(q.exhausted(Bucket::SevenDay, now), "6 小時 < 7 天窗");
        assert!(q.exhausted(Bucket::Fable, now), "Fable 跟 7d 同一個週期");
        assert_eq!(Bucket::FiveHour.len(), FIVE_HOUR_LEN);
        assert_eq!(Bucket::SevenDay.len(), SEVEN_DAY_LEN);
        assert_eq!(Bucket::Fable.len(), SEVEN_DAY_LEN);
    }

    /// **#518 刻意翻掉這條的方向。** 原本（#475）寫的是「`updated_at` 解不開就不拿年齡當理由」，
    /// 所以一筆「見底、沒有 `resets_at`、年齡又說不出來」的讀數會**永遠**算用盡——那正是 #475 標題
    /// 要修的「永久擋住一個身分」，只是換成走時間戳損毀那條路（i267 review）。
    /// 現在兩個時間戳都解不開就當成已過期＝這筆讀數不算數，跟其他四處「解不開＝已過去」同向。
    #[test]
    fn a_reading_with_no_usable_timestamp_stops_counting_as_exhausted() {
        let now = chrono::Utc::now();
        let mut q = codex_q("test", None);
        q.updated_at = "not-a-timestamp".into();
        q.five_hour = Some(Window { observed_at: None, used_pct: 100.0, resets_at: None });
        assert!(!q.exhausted(Bucket::FiveHour, now), "說不出年齡的讀數不可以永久算用盡");
        // 但 `updated_at` 讀得出來時照舊算數（這一半沒有變）。
        q.updated_at = crate::db::now();
        assert!(q.exhausted(Bucket::FiveHour, now), "年齡說得出來、又在窗長內 → 照舊算用盡");
    }

    /// issue #464 的**加固**（不是修 bug：目前沒有來源送毫秒）。1e12 秒是西元 33658 年，
    /// 所以超過門檻只可能是毫秒；不擋的話會安靜地算出一個永遠不會到的 `resets_at`。
    #[test]
    fn a_millisecond_timestamp_is_not_read_as_seconds() {
        let at = |v: serde_json::Value| unix_to_rfc3339(Some(&v));
        // 秒：照舊。
        assert_eq!(at(json!(1_789_000_000)), Some("2026-09-10T00:26:40.000Z".into()));
        // 毫秒：同一個時刻，不是西元五萬年。
        assert_eq!(at(json!(1_789_000_000_000i64)), Some("2026-09-10T00:26:40.000Z".into()));
        assert_eq!(at(json!(1_789_000_000_123i64)), Some("2026-09-10T00:26:40.123Z".into()));
        // 字串形式的毫秒一樣。
        assert_eq!(at(json!("1789000000000")), Some("2026-09-10T00:26:40.000Z".into()));
        // 已經是時間字串的原樣留著。
        assert_eq!(at(json!("2026-09-10T00:26:40Z")), Some("2026-09-10T00:26:40Z".into()));
        // 兩種都不該落在很遠的未來。
        for v in [json!(1_789_000_000_000i64), json!(1_789_000_000)] {
            let s = at(v).unwrap();
            assert!(s.starts_with("2026-"), "{s}");
        }
    }

    /// 多個 Claude session 共用一把帳號 key；較晚收到的舊窗狀態列不能蓋掉重置後的讀數。
    #[tokio::test]
    async fn an_old_statusline_snapshot_cannot_replace_newer_quota_windows() {
        let app = crate::testing::env().await.app.clone();
        let now = chrono::Utc::now();
        let window = |used_pct, resets_at| Window { observed_at: None, used_pct, resets_at: Some(iso(resets_at)) };
        let mut current = codex_q("statusline", None);
        current.five_hour = Some(window(0.0, now + chrono::Duration::hours(4)));
        current.seven_day = Some(window(0.0, now + chrono::Duration::days(6)));
        current.fable = Some(window(0.0, now + chrono::Duration::days(6)));
        current.source = "statusline".into();
        set(&app, LOCAL_HOST, "claude", current).await;

        let mut stale = codex_q("statusline", None);
        stale.five_hour = Some(window(99.0, now + chrono::Duration::hours(3)));
        stale.seven_day = Some(window(99.0, now + chrono::Duration::days(5)));
        stale.fable = Some(window(99.0, now + chrono::Duration::days(5)));
        stale.source = "statusline".into();
        set(&app, LOCAL_HOST, "claude", stale).await;

        let q = app.quotas.lock().await.get("claude").cloned().unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 0.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 0.0);
        assert_eq!(q.fable.as_ref().unwrap().used_pct, 0.0);
    }

    #[tokio::test]
    async fn a_same_window_statusline_cannot_decrease_usage_but_usage_probe_can_correct_it() {
        let app = crate::testing::env().await.app.clone();
        let reset = chrono::Utc::now() + chrono::Duration::hours(4);
        let mut current = codex_q("statusline", None);
        current.five_hour = Some(Window { observed_at: None, used_pct: 40.0, resets_at: Some(iso(reset)) });
        current.source = "statusline".into();
        set(&app, LOCAL_HOST, "claude", current).await;

        let mut stale = codex_q("statusline", None);
        stale.five_hour = Some(Window { observed_at: None, used_pct: 10.0, resets_at: Some(iso(reset)) });
        stale.source = "statusline".into();
        set(&app, LOCAL_HOST, "claude", stale).await;
        assert_eq!(app.quotas.lock().await.get("claude").unwrap().five_hour.as_ref().unwrap().used_pct, 40.0);

        let mut probe = codex_q("claude-usage", None);
        probe.five_hour = Some(Window { observed_at: None, used_pct: 12.0, resets_at: Some(iso(reset)) });
        set(&app, LOCAL_HOST, "claude", probe).await;
        let q = app.quotas.lock().await.get("claude").cloned().unwrap();
        assert_eq!(q.source, "claude-usage");
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 12.0);
    }

    /// 一顆 claude session 的 statusLine：`five` 是 5h 窗 (已用, 重置)，沒有就是那顆最後一次 API 回合落在已結束的窗裡。
    fn claude_statusline(five: Option<(f64, chrono::DateTime<chrono::Utc>)>, seven: (f64, chrono::DateTime<chrono::Utc>)) -> Quota {
        let mut q = codex_q("statusline", None);
        q.five_hour = five.map(|(used_pct, at)| Window { observed_at: None, used_pct, resets_at: Some(iso(at)) });
        q.seven_day = Some(Window { observed_at: None, used_pct: seven.0, resets_at: Some(iso(seven.1)) });
        q
    }

    async fn claude_seven_day(app: &Arc<App>) -> f64 {
        app.quotas.lock().await.get("claude").unwrap().seven_day.as_ref().unwrap().used_pct
    }

    /// #404（2026-09-23 14:04Z 誤報 critical）：cc0 的 17 個 session 報 7d 12%，一個閒置很久的 session 報 97%，
    /// **同一個** 7d resets_at。它的 payload 沒有 5h 窗（最後一次 API 回合在已結束的 5h 窗裡），#399 的「5h 較舊就丟」
    /// 比不到；同窗取 max 就把 97 鎖住，其餘 17 個永遠壓不回去。它先到、夾在中間、最後到都一樣要是 12。
    #[tokio::test]
    async fn one_idle_session_cannot_lock_the_seven_day_window_high() {
        let now = chrono::Utc::now();
        let five_reset = now + chrono::Duration::hours(3);
        let seven_reset = now + chrono::Duration::days(2);
        let fresh = || claude_statusline(Some((6.0, five_reset)), (12.0, seven_reset));
        for idle in [
            claude_statusline(None, (97.0, seven_reset)),
            // 同一個 5h 窗、但 5h 用量比較低：也是比較舊的回合。
            claude_statusline(Some((2.0, five_reset)), (97.0, seven_reset)),
        ] {
            for position in [0, 9, 17] {
                let app = crate::testing::env().await.app.clone();
                for i in 0..18 {
                    let q = if i == position { idle.clone() } else { fresh() };
                    set(&app, LOCAL_HOST, "claude", q).await;
                }
                assert_eq!(claude_seven_day(&app).await, 12.0, "閒置 session 排在第 {position} 個：{idle:?}");
            }
        }
    }

    /// 已經被鎖在高值（例如 5h 窗全過期時收進來的舊讀數）也不是永久的：任何一筆 5h 比較新的讀數就照實寫回來。
    #[tokio::test]
    async fn a_fresher_five_hour_reading_releases_a_high_seven_day_value() {
        let app = crate::testing::env().await.app.clone();
        let now = chrono::Utc::now();
        let seven_reset = now + chrono::Duration::days(2);
        let five_reset = now + chrono::Duration::hours(3);
        // 沒人用了 5 小時：沒有有效的 5h 窗可比，照舊收（同窗取大）。
        set(&app, LOCAL_HOST, "claude", claude_statusline(None, (97.0, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 97.0);
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((1.0, five_reset)), (12.0, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 12.0, "開了新 5h 窗的讀數比較新");
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((1.0, five_reset)), (13.0, seven_reset))).await;
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((1.0, five_reset)), (12.0, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 13.0, "5h 一樣新時分不出先後，同窗照舊取大");
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((3.0, five_reset)), (12.5, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 12.5, "5h 用量比較高＝比較新的回合，可以往下修");
    }

    /// `/usage` 的結構化 resets_at 帶毫秒（`…:00.594Z`），statusLine 是整秒：同一個窗，不能被當成「比較舊的窗」丟掉，
    /// 也不能把 statusLine 當成比較新的窗。探測之後，閒置 session 的舊讀數一樣不能把它蓋回高值。
    #[tokio::test]
    async fn a_usage_probe_and_the_statusline_agree_on_the_window_despite_millisecond_jitter() {
        let app = crate::testing::env().await.app.clone();
        let now = chrono::Utc::now();
        let five_reset = now + chrono::Duration::hours(3);
        let seven_reset = now + chrono::Duration::days(2);
        let jitter = chrono::Duration::milliseconds(594);
        set(&app, LOCAL_HOST, "claude", claude_statusline(None, (97.0, seven_reset))).await;
        let mut probe = codex_q("claude-usage", None);
        probe.five_hour = Some(Window { observed_at: None, used_pct: 6.0, resets_at: Some(iso(five_reset + jitter)) });
        probe.seven_day = Some(Window { observed_at: None, used_pct: 12.0, resets_at: Some(iso(seven_reset + jitter)) });
        set(&app, LOCAL_HOST, "claude", probe).await;
        assert_eq!(claude_seven_day(&app).await, 12.0, "探測直接覆寫");

        set(&app, LOCAL_HOST, "claude", claude_statusline(None, (97.0, seven_reset))).await;
        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((5.0, five_reset)), (97.0, seven_reset))).await;
        assert_eq!(claude_seven_day(&app).await, 12.0, "比探測舊的 statusLine 不能把 97 蓋回來");

        set(&app, LOCAL_HOST, "claude", claude_statusline(Some((7.0, five_reset)), (13.0, seven_reset))).await;
        let q = app.quotas.lock().await.get("claude").cloned().unwrap();
        assert_eq!(q.source, "statusline", "同窗、5h 比探測多：比較新的讀數要收");
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 13.0);
    }

    /// 狀態列是剩餘、存的是已用；不可洗掉 `resets_at`（2026-09-13 使用者：量表停在舊數字）。
    #[tokio::test]
    async fn the_status_line_updates_the_numbers_without_losing_the_reset_time() {
        let app = crate::testing::env().await.app.clone();
        // 重置時間用**還沒到**的相對時刻：原本寫死 2026-09-13／18，那兩個日期早就過去了，
        // 於是這條測試其實是在釘「連已經過去的重置時間也照樣沿用」——而那正是 #489 的破口
        // （繼承來的過期時刻會把新鮮的見底讀數判成已重置）。這裡的本意是「app-server 的重置時間
        // 不會被狀態列更新洗掉」，改成未來的時刻才測得到本意。
        let five_reset = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::hours(4));
        let seven_reset = crate::db::iso_at(chrono::Utc::now() + chrono::Duration::days(6));
        let from_server = Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 0.0, resets_at: Some(five_reset.clone()) }),
            seven_day: Some(Window { observed_at: None, used_pct: 50.0, resets_at: Some(seven_reset.clone()) }),
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
        assert_eq!(q.five_hour.as_ref().unwrap().resets_at.as_deref(), Some(five_reset.as_str()), "重置時間沿用");
        assert_eq!(q.seven_day.as_ref().unwrap().resets_at.as_deref(), Some(seven_reset.as_str()));
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
            q.five_hour = Some(Window { observed_at: None, used_pct: used, resets_at: Some(iso(resets)) });
            q.seven_day = Some(Window { observed_at: None, used_pct: 35.0, resets_at: Some(iso(now + chrono::Duration::days(3))) });
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
        let w = |used: f64, resets: chrono::DateTime<chrono::Utc>| Window { observed_at: None, used_pct: used, resets_at: Some(iso(resets)) };
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
        let bare = Window { observed_at: None, used_pct: 20.0, resets_at: None };
        assert_eq!(pane_window_used(Some(7.0), Some(&bare), len, Some(now), Sighting::New, now), Some(7.0));
        assert_eq!(pane_window_used(Some(7.0), Some(&bare), len, Some(now), Sighting::Same, now), None);
        assert_eq!(pane_window_used(None, Some(&cur), len, Some(now), Sighting::Changed, now), None);
    }

    /// 截斷只讀到 5h 時不可洗掉 7d（2026-09-13 使用者：header 的 codex 只剩一條）。
    #[tokio::test]
    async fn a_partial_reading_keeps_the_window_it_could_not_see() {
        let app = crate::testing::env().await.app.clone();
        let mut full = codex_q("codex-app-server", None);
        full.five_hour = Some(Window { observed_at: None, used_pct: 30.0, resets_at: Some("2026-09-13T19:22:00.000Z".into()) });
        full.seven_day = Some(Window { observed_at: None, used_pct: 76.0, resets_at: Some("2026-09-18T00:00:00.000Z".into()) });
        set(&app, LOCAL_HOST, "codex", full).await;

        let mut partial = codex_q("codex-statusline", None);
        partial.five_hour = Some(Window { observed_at: None, used_pct: 64.0, resets_at: None });
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
            q.five_hour = Some(Window { observed_at: None, used_pct: pct, resets_at: None });
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
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
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
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
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
        q.five_hour = Some(Window { observed_at: None, used_pct: 100.0, resets_at: None });
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
            primary_position: 0,
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
            five_hour: Some(Window { observed_at: None, used_pct: 10.0, resets_at: None }),
            seven_day: Some(Window { observed_at: None, used_pct: 20.0, resets_at: None }),
            fable: Some(Window { observed_at: None, used_pct: 66.0, resets_at: None }),
            reset_credits: None,
            limit_hit: None,
            plan: None, updated_at: crate::db::now(), source: "claude-usage".into(), account: Some("cc1".into()), host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:cc1", probe).await;
        let status = Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 11.0, resets_at: None }),
            seven_day: Some(Window { observed_at: None, used_pct: 21.0, resets_at: None }),
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
            five_hour: Some(Window { observed_at: None, used_pct: 0.0, resets_at: None }),
            seven_day: Some(Window { observed_at: None, used_pct: 0.0, resets_at: None }),
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
        usage.fable = Some(Window { observed_at: None, used_pct: 100.0, resets_at: Some(iso(tomorrow)) });
        set(&app, LOCAL_HOST, "claude:cc1", usage.clone()).await;
        assert_eq!(until(app.clone()).await, Some(iso(tomorrow)), "保底的 7 天要被那一桶自己的重置時間截短");

        // 不相干的桶（statusLine 只有 5h／7d）不算那一桶的讀數。
        let mut status = codex_q("statusline", None);
        status.five_hour = Some(Window { observed_at: None, used_pct: 3.0, resets_at: Some(iso(now + chrono::Duration::hours(4))) });
        set(&app, LOCAL_HOST, "claude:cc1", status).await;
        assert_eq!(until(app.clone()).await, Some(iso(tomorrow)));

        // 重置之後的讀數：窗的起點在撞限之後 → 撞限作廢。
        let mut after = codex_q("claude-usage", None);
        after.fable = Some(Window { observed_at: None, used_pct: 0.0, resets_at: Some(iso(at + chrono::Duration::days(7) + chrono::Duration::minutes(1))) });
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
        reading.five_hour = Some(Window { observed_at: None, used_pct: 94.0, resets_at: Some(iso(now + chrono::Duration::minutes(20))) });
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
        idle.five_hour = Some(Window { observed_at: None, used_pct: 0.0, resets_at: Some(iso(now - chrono::Duration::hours(1))) });
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
            five_hour: Some(Window { observed_at: None, used_pct: 10.0, resets_at: None }),
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

    /// issue #392：讀數寫入快取，重啟後先以 stale 回填；新的探測成功後才恢復 fresh，過期的舊撞限也不能卡住派送。
    #[tokio::test]
    async fn quota_cache_survives_restart_as_stale_until_a_fresh_reading_arrives() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let reading = |used_pct: f64, limit_hit: Option<LimitHit>| Quota {
            five_hour: Some(Window { observed_at: None, used_pct, resets_at: Some("2099-01-01T00:00:00Z".into()) }),
            seven_day: Some(Window { observed_at: None, used_pct: 20.0, resets_at: Some("2099-01-07T00:00:00Z".into()) }),
            fable: None,
            reset_credits: None,
            limit_hit,
            plan: Some("test".into()),
            updated_at: crate::db::now(),
            source: "test".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };

        set(&app, LOCAL_HOST, "claude", reading(41.0, None)).await;
        let restarted = crate::testing::restart_app(&env).await;
        assert_eq!(load_cache(&restarted).await.unwrap(), 1);
        assert_eq!(restarted.quotas.lock().await["claude"].five_hour.as_ref().unwrap().used_pct, 41.0);
        assert_eq!(snapshot(&restarted).await["kinds"]["claude"]["stale"], true);

        let fresh = reading(52.0, None);
        set(&restarted, LOCAL_HOST, "claude", fresh).await;
        assert_eq!(snapshot(&restarted).await["kinds"]["claude"]["stale"], false);
        assert_eq!(restarted.quotas.lock().await["claude"].five_hour.as_ref().unwrap().used_pct, 52.0);

        // 模擬快取裡還留著一筆已過 reset 的舊「用完了」；開機回填時照既有規則清掉，不能阻擋新的工作。
        let expired = reading(
            99.0,
            Some(LimitHit {
                message: "old limit".into(),
                until: Some("2000-01-01T00:00:00Z".into()),
                at: "1999-12-31T00:00:00Z".into(),
                bucket: None,
            }),
        );
        sqlx::query("UPDATE quota_cache SET quota_json = ?, updated_at = ? WHERE key = ?")
            .bind(serde_json::to_string(&expired).unwrap())
            .bind(&expired.updated_at)
            .bind("claude")
            .execute(&restarted.db)
            .await
            .unwrap();
        let after_expiry = crate::testing::restart_app(&env).await;
        load_cache(&after_expiry).await.unwrap();
        assert!(after_expiry.quotas.lock().await["claude"].limit_hit.is_none());
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
            five_hour: Some(Window { observed_at: None, used_pct: 3.0, resets_at: Some(resets) }),
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
        local.five_hour = Some(Window { observed_at: None, used_pct: 40.0, resets_at: Some(later(1)) });
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
            crate::tools::HostTools { tools: Default::default(), identities: Default::default(), shell_identities: vec![cc0], utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now() },
        );
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("cc0")).await.unwrap(), "claude", "偵測完：cc0 就是預設帳號");
        assert_eq!(resolve_quota_base(&app, LOCAL_HOST, "claude", Some("nobody")).await.unwrap(), "claude:nobody", "偵測完還查不到：照舊分開");
    }
    /// #347：探測途中同名主機被換掉，舊機器的額度不能寫進去（也不能把已移除主機的 key 種回來）。
    #[tokio::test]
    async fn a_quota_reading_from_a_superseded_host_probe_is_not_published() {
        let app = crate::testing::env().await.app.clone();
        let cfg = |ssh: &str| crate::config::HostCfg { name: "build1".into(), ssh: ssh.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new() };
        app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let fence = app.hosts.fence("build1").await.unwrap();
        let reading = || Quota {
            five_hour: Some(Window { observed_at: None, used_pct: 10.0, resets_at: None }), seven_day: None, fable: None, reset_credits: None,
            limit_hit: None, plan: None, updated_at: crate::db::now(), source: "test".into(), account: None, host: "build1".into(),
        };
        app.hosts.insert_remote_for_test(cfg("target-b")).await;
        assert!(set_fenced(&app, "build1", "codex", reading(), &fence).await.is_err());
        assert!(app.quotas.lock().await.get("build1/codex").is_none());
        let fresh = app.hosts.fence("build1").await.unwrap();
        set_fenced(&app, "build1", "codex", reading(), &fresh).await.unwrap();
        assert!(app.quotas.lock().await.get("build1/codex").is_some());
    }
}
