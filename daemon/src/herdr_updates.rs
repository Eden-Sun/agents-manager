//! Herdr 自己的版本追蹤（2026-09-16 使用者需求：「Herdr 版本也要追新功能與通知」）。
//!
//! 這支跟 [`crate::update_watch`] 沒有關係，不要混在一起看：那支追的是 **agent CLI**（claude）更新完
//! 等重啟的那句話，掛在 run 上；這支追的是**跑我們自己的 herdr**，掛在 host 上，而且只讀、只通知。
//!
//! 三個版本是三件不同的事，任何一個不知道都不能說「已是最新」：
//!
//! * **running server**：那台主機上正在跑的 herdr server（`ping` 的 `version`/`protocol`）。真正決定
//!   pane 行為的是它。
//! * **disk CLI**：那台主機磁碟上的 `herdr --version`。比 server 新 = 有人裝了新版但還沒換 server。
//! * **latest stable**：官方 <https://herdr.dev/latest.json>。prerelease 不推薦。
//!
//! 刻意不做的事：不下載、不 `herdr update`、不 `server stop/restart`。升級**可能**影響正在跑的 pane——
//! 0.9 起 client 更新可以不動相容的 server 與它底下的 pane，但 endpoint generation 比 1 舊的 server 需要一次
//! 性升級，那次才會動到 pane。是哪一種要看兩邊版本，所以交給 AGM 確認相容性並安排時間窗，不是背景巡邏
//! 可以順手做的。
//!
//! 通知走既有 AGM 巡檢路由（`supervisor_inbox`）：一個 release 一筆合併事件，`event_key` 就是去重標記，
//! 所以 daemon 重啟不會對同一版再叫醒 AGM 一次，一台主機開幾個 pane 也不會變成幾次通知。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::state::App;

/// 官方 stable manifest。固定寫死：這支會被 AGM 當成升級依據，來源不該被設定檔改掉。
pub const LATEST_URL: &str = "https://herdr.dev/latest.json";
/// 給 UI 的「官方 release 頁」連結。
pub const RELEASES_URL: &str = "https://github.com/herdrdev/herdr/releases";

/// 啟動查一次，之後每 6 小時。
const POLL: Duration = Duration::from_secs(6 * 60 * 60);
/// 兩次外部請求之間的最小間隔（手動與排程共用）：連點與併發只會打一次官方站。
const MIN_GAP: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// `herdr --version` 是 process spawn（遠端是 ssh）。
const DISK_TIMEOUT: Duration = Duration::from_secs(20);
/// 磁碟上那支 CLI 的名字。參數化只是為了讓測試能餵一支假的執行檔（`command -v` 認絕對路徑），
/// 正式呼叫端永遠是這個值。
const HERDR_BIN: &str = "herdr";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 2026-09-16 實測 latest.json 約 160 KB。留十幾倍餘裕，但不無上限地吃進記憶體。
const MAX_BODY: usize = 2 * 1024 * 1024;
/// 超過兩輪沒抓成功就標 `stale`：值照舊顯示（不吞快取），但不能假裝是剛確認過的。
const STALE_AFTER: Duration = Duration::from_secs(13 * 60 * 60);

// ---------------------------------------------------------------- semver

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PreId {
    /// semver：數字段永遠小於文字段。
    Num(u64),
    Text(String),
}

/// 比得對的版本號。`changelog::parse_version` 只吃純數字段（`0.9.0-rc.1` 會整個變 `None`，
/// 於是 prerelease 悄悄變成「看不出版本」），這裡要能認出 prerelease 才擋得住「推薦 rc 版」。
#[derive(Debug, Clone)]
pub struct Ver {
    nums: Vec<u64>,
    pre: Vec<PreId>,
}

impl Ver {
    /// `v0.9.0`、`0.9.0`、`herdr 0.8.2`、`0.9.0-rc.1+build5` 都吃。認不出回 `None`——
    /// 認不出要變成「未知」，不能變成 0.0.0 然後被說成落後或最新。
    pub fn parse(s: &str) -> Option<Self> {
        let tok = s.split_whitespace().find(|t| t.trim_start_matches('v').starts_with(|c: char| c.is_ascii_digit()))?;
        let tok = tok.trim_start_matches('v');
        // build metadata 不參與比較（semver §10）。
        let tok = tok.split('+').next()?;
        let (core, pre) = match tok.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (tok, None),
        };
        let nums: Vec<u64> = core.split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<_>>>()?;
        if nums.is_empty() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) if p.is_empty() => return None,
            Some(p) => p
                .split('.')
                .map(|id| match id.parse::<u64>() {
                    Ok(n) => PreId::Num(n),
                    Err(_) => PreId::Text(id.to_string()),
                })
                .collect(),
        };
        Some(Self { nums, pre })
    }

    /// 正式版。stable 追蹤只推薦這種。
    pub fn is_stable(&self) -> bool {
        self.pre.is_empty()
    }
}

impl Ord for Ver {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // 位數不同要補 0 再比：`0.9` 與 `0.9.0` 同版，`2.1.9 < 2.1.10`。
        let n = self.nums.len().max(other.nums.len());
        for i in 0..n {
            let (a, b) = (self.nums.get(i).copied().unwrap_or(0), other.nums.get(i).copied().unwrap_or(0));
            match a.cmp(&b) {
                std::cmp::Ordering::Equal => {}
                o => return o,
            }
        }
        // 同一組數字：有 prerelease 的比正式版舊（semver §11.3）。
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => self.pre.cmp(&other.pre),
        }
    }
}

impl PartialOrd for Ver {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// 相等就是 `cmp` 說相等，不是欄位長得一樣：`0.9` 與 `0.9.0` 是同一版，derive 出來的 `PartialEq`
/// 會說它們不同，於是「已是最新」跟「有更新」會給出互相矛盾的答案。
impl PartialEq for Ver {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for Ver {}

/// 招牌裡版本那一段的原文：`herdr 0.8.2` → `0.8.2`、`2.1.5 (Claude Code)` → `2.1.5`。
/// 保留 prerelease 與 build 後綴（`0.9.0-rc.1`），不像 `changelog::version_string` 只留數字段。
pub fn version_token(s: &str) -> Option<String> {
    let tok = s.split_whitespace().find(|t| Ver::parse(t).is_some())?;
    Some(tok.trim_start_matches('v').to_string())
}

/// `unknown`（任一邊不知道）/ `latest` / `behind` / `ahead`。
///
/// 不知道**不能**折成 `latest`：整支功能的重點就是不要謊報「已是最新」。
pub fn standing(current: Option<&str>, latest: Option<&str>) -> &'static str {
    let (Some(c), Some(l)) = (current.and_then(Ver::parse), latest.and_then(Ver::parse)) else {
        return "unknown";
    };
    match c.cmp(&l) {
        std::cmp::Ordering::Equal => "latest",
        std::cmp::Ordering::Less => "behind",
        std::cmp::Ordering::Greater => "ahead",
    }
}

// ---------------------------------------------------------------- manifest

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Release {
    pub notes: Option<String>,
    pub protocol: Option<u64>,
    pub endpoint_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// 最新 **stable**，不一定是檔案頂層那個 `version`。
    pub latest: String,
    pub protocol: Option<u64>,
    pub endpoint_generation: Option<u64>,
    /// 官方 `releases`：版本字串 → 該版資料。2026-09-16 實測 56 筆，含每版自己的 `notes`。
    pub releases: BTreeMap<String, Release>,
}

fn release_from(v: &Value) -> Release {
    Release {
        notes: v.get("notes").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
        protocol: v.get("protocol").and_then(Value::as_u64),
        endpoint_generation: v.get("endpoint_generation").and_then(Value::as_u64),
    }
}

/// 照 2026-09-16 實測的真實 schema：頂層 `version` / `protocol` / `endpoint_generation` / `notes`，
/// 加一份 `releases`（版本 → `{notes, protocol, endpoint_generation, …}`）。
///
/// 「最新」一律取 `releases` 與頂層 `version` 裡**最大的 stable**：頂層哪天推了 prerelease，這裡也不會
/// 把 rc 版說成該升上去的版本。完全沒有 stable 就是壞資料，回 `Err`（保留舊快取）。
pub fn parse_manifest(body: &str) -> Result<Manifest> {
    let v: Value = serde_json::from_str(body).map_err(|e| anyhow!("latest.json 不是合法 JSON：{e}"))?;
    let obj = v.as_object().ok_or_else(|| anyhow!("latest.json 不是物件"))?;
    let mut releases = BTreeMap::new();
    if let Some(map) = obj.get("releases").and_then(Value::as_object) {
        for (k, rv) in map {
            let Some(ver) = Ver::parse(k) else { continue };
            if !ver.is_stable() {
                continue;
            }
            releases.insert(k.trim().to_string(), release_from(rv));
        }
    }
    let top = obj.get("version").and_then(Value::as_str).map(str::trim).unwrap_or("");
    let top_stable = Ver::parse(top).filter(Ver::is_stable).map(|_| top.to_string());
    if let Some(t) = &top_stable {
        releases.entry(t.clone()).or_insert_with(|| Release {
            notes: obj.get("notes").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            protocol: obj.get("protocol").and_then(Value::as_u64),
            endpoint_generation: obj.get("endpoint_generation").and_then(Value::as_u64),
        });
    }
    let latest = releases
        .keys()
        .filter_map(|k| Ver::parse(k).map(|v| (v, k.clone())))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, k)| k)
        .ok_or_else(|| anyhow!("latest.json 裡沒有任何正式版"))?;
    let r = releases.get(&latest).cloned().unwrap_or_default();
    Ok(Manifest { latest, protocol: r.protocol, endpoint_generation: r.endpoint_generation, releases })
}

// ---------------------------------------------------------------- release notes

#[derive(Debug, Clone, PartialEq)]
pub struct NoteSection {
    pub version: String,
    /// `None` = 官方那筆沒帶 notes。不要拿別版的內容補。
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NotesView {
    pub from: Option<String>,
    pub to: String,
    /// 這份清單能不能當成「跨版差距的全部內容」。要同時滿足：`from` 那一版在官方清單裡（中間有哪些版是
    /// 清楚的）**且**每一段都真的有 notes。少一段內容就不是完整的差距，不能給 UI 一個會誤導的綠燈。
    pub complete: bool,
    /// 新的在前，只含 `(from, to]` 之間官方真的有資料的版本。
    pub sections: Vec<NoteSection>,
    /// 清單裡有、但那一版沒附 notes 的版本。
    pub missing_notes: Vec<String>,
    /// 為什麼這份清單可能不完整（`complete == false` 時給 UI 照抄的一句話）。
    pub gap: Option<String>,
}

/// `from`（不含）到 `to`（含）的官方 release notes，新的在前。
///
/// 只回官方清單裡真的有的版本，而且**每段都標自己的版本號**：不能把 `to` 那段說成整個跨版差距的內容。
/// `from` 不在清單裡（太舊、或本機是私有 build）時 `complete = false` + `gap` 明說，UI 照寫出來。
pub fn notes_between(m: &Manifest, from: Option<&str>, to: &str) -> NotesView {
    let mut view = NotesView { from: from.map(str::to_string), to: to.to_string(), ..Default::default() };
    let Some(to_v) = Ver::parse(to) else {
        view.gap = Some(format!("看不出「{to}」是哪一版，沒辦法列出版本差距"));
        return view;
    };
    let from_v = from.and_then(Ver::parse);
    let from_listed = match &from_v {
        None => false,
        Some(f) => m.releases.keys().any(|k| Ver::parse(k).as_ref() == Some(f)),
    };
    let mut picked: Vec<(Ver, NoteSection)> = m
        .releases
        .iter()
        .filter_map(|(k, r)| {
            let v = Ver::parse(k)?;
            if v > to_v {
                return None;
            }
            let keep = match &from_v {
                Some(f) => v > *f,
                // 起點不明就只給 `to` 那一段，不要假裝知道中間有哪些版。
                None => v == to_v,
            };
            keep.then(|| (v, NoteSection { version: k.clone(), notes: r.notes.clone() }))
        })
        .collect();
    picked.sort_by(|a, b| b.0.cmp(&a.0));
    view.missing_notes = picked.iter().filter(|(_, s)| s.notes.is_none()).map(|(_, s)| s.version.clone()).collect();
    view.sections = picked.into_iter().map(|(_, s)| s).collect();
    view.complete = from_listed && view.missing_notes.is_empty();
    if !view.complete {
        // 兩個原因可能同時成立，都要說；`gap` 是 UI 照抄的句子。
        let mut why: Vec<String> = Vec::new();
        match from {
            Some(f) if !from_listed => why.push(format!("官方清單裡沒有 {f} 這一版，{f} 到 {to} 之間可能還有其他版本沒列出來")),
            None => why.push(format!("不知道目前是哪一版，只列得出 {to} 這一版的內容")),
            _ => {}
        }
        if !view.missing_notes.is_empty() {
            why.push(format!("官方沒有附這幾版的說明：{}", view.missing_notes.join("、")));
        }
        if !why.is_empty() {
            view.gap = Some(why.join("；"));
        }
    }
    view
}

// ---------------------------------------------------------------- 資料表

/// `latest` 一列（`id = 1`）、每台主機一列、看過的版本一列。
///
/// 抓失敗只寫 `checked_at` / `error`，**不動** `body_json` / `version` / `fetched_at`：離線或官方站壞掉時
/// 舊快取要留著，不然一次網路抖動就把「有新版」變成「不知道」。
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    for stmt in [
        "CREATE TABLE IF NOT EXISTS herdr_latest (
           id INTEGER PRIMARY KEY CHECK (id = 1),
           version TEXT, protocol INTEGER, endpoint_generation INTEGER,
           body_json TEXT,
           fetched_at TEXT,
           checked_at TEXT,
           error TEXT
         )",
        "CREATE TABLE IF NOT EXISTS herdr_host_versions (
           host TEXT PRIMARY KEY,
           server_version TEXT, server_protocol INTEGER, server_at TEXT, server_error TEXT,
           disk_version TEXT, disk_at TEXT, disk_error TEXT,
           checked_at TEXT NOT NULL
         )",
        "CREATE TABLE IF NOT EXISTS herdr_update_seen (
           version TEXT PRIMARY KEY,
           seen_at TEXT NOT NULL
         )",
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default, sqlx::FromRow)]
struct LatestRow {
    version: Option<String>,
    protocol: Option<i64>,
    endpoint_generation: Option<i64>,
    body_json: Option<String>,
    fetched_at: Option<String>,
    checked_at: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct HostRow {
    host: String,
    server_version: Option<String>,
    server_protocol: Option<i64>,
    server_at: Option<String>,
    server_error: Option<String>,
    disk_version: Option<String>,
    disk_at: Option<String>,
    disk_error: Option<String>,
    checked_at: String,
}

async fn load_latest(pool: &SqlitePool) -> LatestRow {
    sqlx::query_as::<_, LatestRow>(
        "SELECT version, protocol, endpoint_generation, body_json, fetched_at, checked_at, error FROM herdr_latest WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

async fn save_latest_ok(pool: &SqlitePool, m: &Manifest, body: &str) -> Result<()> {
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO herdr_latest (id, version, protocol, endpoint_generation, body_json, fetched_at, checked_at, error)
         VALUES (1,?,?,?,?,?,?,NULL)
         ON CONFLICT(id) DO UPDATE SET
           version=excluded.version, protocol=excluded.protocol,
           endpoint_generation=excluded.endpoint_generation, body_json=excluded.body_json,
           fetched_at=excluded.fetched_at, checked_at=excluded.checked_at, error=NULL",
    )
    .bind(&m.latest)
    .bind(m.protocol.map(|p| p as i64))
    .bind(m.endpoint_generation.map(|p| p as i64))
    .bind(body)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// 只記「這次沒抓到、為什麼」。舊值原封不動。
async fn save_latest_err(pool: &SqlitePool, err: &str) -> Result<()> {
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO herdr_latest (id, checked_at, error) VALUES (1,?,?)
         ON CONFLICT(id) DO UPDATE SET checked_at=excluded.checked_at, error=excluded.error",
    )
    .bind(&now)
    .bind(err)
    .execute(pool)
    .await?;
    Ok(())
}

/// 一台主機這次探到什麼。`None` 的欄位代表「這次沒讀到」，寫入時不會覆蓋上次讀到的值。
#[derive(Debug, Clone, Default)]
struct HostProbe {
    host: String,
    server_version: Option<String>,
    server_protocol: Option<u64>,
    server_error: Option<String>,
    disk_version: Option<String>,
    disk_error: Option<String>,
}

/// 讀到的值才覆蓋（`COALESCE`），讀不到只換錯誤與時間：離線主機留著上次的版本並標示為舊資料，
/// 絕不拿本機版本冒充，也絕不因為一次 ssh 失敗就把已知版本清成未知。
async fn save_host(pool: &SqlitePool, p: &HostProbe) -> Result<()> {
    let now = crate::db::now();
    let server_at = p.server_version.as_ref().map(|_| now.clone());
    let disk_at = p.disk_version.as_ref().map(|_| now.clone());
    sqlx::query(
        "INSERT INTO herdr_host_versions
           (host, server_version, server_protocol, server_at, server_error, disk_version, disk_at, disk_error, checked_at)
         VALUES (?,?,?,?,?,?,?,?,?)
         ON CONFLICT(host) DO UPDATE SET
           server_version = COALESCE(excluded.server_version, herdr_host_versions.server_version),
           server_protocol = COALESCE(excluded.server_protocol, herdr_host_versions.server_protocol),
           server_at = COALESCE(excluded.server_at, herdr_host_versions.server_at),
           server_error = excluded.server_error,
           disk_version = COALESCE(excluded.disk_version, herdr_host_versions.disk_version),
           disk_at = COALESCE(excluded.disk_at, herdr_host_versions.disk_at),
           disk_error = excluded.disk_error,
           checked_at = excluded.checked_at",
    )
    .bind(&p.host)
    .bind(p.server_version.as_deref())
    .bind(p.server_protocol.map(|x| x as i64))
    .bind(server_at)
    .bind(p.server_error.as_deref())
    .bind(p.disk_version.as_deref())
    .bind(disk_at)
    .bind(p.disk_error.as_deref())
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

impl HostRow {
    /// 設定裡有、但這輪還沒巡到（剛加的主機、daemon 剛起來）。時間戳留空 = 一律未知，
    /// 不是「沒有問題」。
    fn never_probed(host: &str) -> Self {
        Self {
            host: host.to_string(),
            server_version: None,
            server_protocol: None,
            server_at: None,
            server_error: None,
            disk_version: None,
            disk_at: None,
            disk_error: None,
            checked_at: String::new(),
        }
    }
}

async fn load_hosts(pool: &SqlitePool) -> Vec<HostRow> {
    sqlx::query_as::<_, HostRow>("SELECT * FROM herdr_host_versions ORDER BY host")
        .fetch_all(pool)
        .await
        .unwrap_or_default()
}

/// 使用者按掉這一版的提示。只影響 UI 未讀，不影響 AGM 那筆事件。
pub async fn mark_seen(pool: &SqlitePool, version: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO herdr_update_seen (version, seen_at) VALUES (?,?)")
        .bind(version)
        .bind(crate::db::now())
        .execute(pool)
        .await?;
    Ok(())
}

async fn seen_at(pool: &SqlitePool, version: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT seen_at FROM herdr_update_seen WHERE version = ?")
        .bind(version)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

// ---------------------------------------------------------------- 探測

/// 串著讀並在超過 `max_body` 時中止：`Content-Length` 可以騙人，不能只信它。
///
/// timeout 與上限是參數而不是寫死的常數，測試才能用本機假 server 真的把逾時與超大 body 跑出來
/// （正式呼叫端一律帶 [`HTTP_TIMEOUT`] / [`MAX_BODY`]）。
async fn fetch_body(url: &str, timeout: Duration, max_body: usize) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("agents-manager")
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
        .build()
        .map_err(|e| anyhow!("http client: {e}"))?;
    let mut resp = client.get(url).send().await.map_err(|e| anyhow!("抓 {url} 失敗：{e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("抓 {url} 失敗：HTTP {}", resp.status()));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| anyhow!("讀 {url} 失敗：{e}"))? {
        if buf.len() + chunk.len() > max_body {
            return Err(anyhow!("{url} 超過 {max_body} bytes，不收"));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| anyhow!("{url} 不是 UTF-8"))
}

/// 正式一律是 [`LATEST_URL`]；測試把它指到本機假 server，才能把逾時／壞 JSON／503 真的跑過一次。
#[cfg(test)]
fn source_override() -> &'static std::sync::Mutex<Option<String>> {
    static S: std::sync::OnceLock<std::sync::Mutex<Option<String>>> = std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(None))
}

fn source_url() -> String {
    #[cfg(test)]
    if let Some(u) = source_override().lock().unwrap().clone() {
        return u;
    }
    LATEST_URL.to_string()
}

/// 一台主機的兩個版本。**不會**因為讀不到就填別的東西：讀不到就是 `None` + 一句原因。
async fn probe_host(app: &Arc<App>, conn: &Arc<crate::hosts::HostConn>, disk_bin: &str) -> HostProbe {
    let mut p = HostProbe { host: conn.name.clone(), ..Default::default() };
    let connected = if conn.is_local() {
        app.connected.load(std::sync::atomic::Ordering::SeqCst)
    } else {
        conn.is_connected()
    };
    if connected {
        match conn.client.ping().await {
            Ok(pong) => {
                p.server_version = Ver::parse(&pong.version).map(|_| pong.version.trim().to_string());
                if p.server_version.is_none() {
                    p.server_error = Some(format!("herdr server 回了「{}」，看不出版本", pong.version));
                }
                p.server_protocol = Some(pong.protocol as u64);
            }
            Err(e) => p.server_error = Some(format!("問不到跑著的 herdr server 版本：{e:#}")),
        }
    } else {
        p.server_error = Some("主機未連線，跑著的 herdr server 版本是上次讀到的".into());
    }
    // 磁碟上的 CLI：沿用既有的探測管道（本機 `/bin/sh`、遠端 ssh，read-only），但**自己解析**。
    // `changelog::installed_version` 用的 `version_string` 只認「版本在第一個 token」（claude 的
    // `2.1.5 (Claude Code)`）；herdr 印的是 `herdr 0.8.2`，第一個 token 是名字，套上去永遠回 Err，
    // 磁碟版本會永遠顯示未知。
    match crate::changelog::version_line(app, &conn.name, disk_bin, DISK_TIMEOUT).await {
        Ok(line) => match version_token(&line) {
            Some(v) => p.disk_version = Some(v),
            None => p.disk_error = Some(format!("`herdr --version` 回了「{line}」，看不出版本")),
        },
        Err(e) => p.disk_error = Some(format!("{e:#}")),
    }
    p
}

// ---------------------------------------------------------------- 通知

/// AGM 事件的 kind。`roles::route` 沒有特例的 kind 一律走巡檢 + 叫醒，正是這筆該去的地方。
pub const EVENT_KIND: &str = "herdr_update_available";

fn event_key(version: &str) -> String {
    format!("herdr_update:{version}")
}

async fn notified_at(pool: &SqlitePool, version: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT created_at FROM supervisor_inbox WHERE event_key = ? LIMIT 1")
        .bind(event_key(version))
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// 真的落後的主機才值得叫醒模型：`standing` 已經把「讀數過期」與「官方版本不明」折成 `unknown`，
/// 所以離線主機的歷史版本不會變成一次通知——不知道就不叫醒。
fn behind_hosts(hosts: &[Value]) -> Vec<String> {
    hosts
        .iter()
        .filter(|h| ["server", "disk"].iter().any(|k| side_standing(h, k) == "behind"))
        .map(host_name)
        .collect()
}

/// 一個 release 一筆合併事件。`supervisor_inbox` 的 `(supervisor_id, event_key)` 唯一索引就是去重標記，
/// 所以「寫進去」與「標記已通知」是同一個原子動作：
///
/// * 寫失敗 → 什麼都沒留下，下一輪再試（不會出現「沒送出卻永久 seen」）。
/// * daemon 重啟、或一台主機有十個 pane → 同一個 key，`INSERT OR IGNORE` 直接不動，不會再叫醒 AGM。
/// * 沒有落後的主機、或根本不知道版本 → 不寫、不叫醒。
async fn notify_if_new(app: &Arc<App>, latest: &str, hosts: &[Value], notes_complete: bool) -> Option<String> {
    let behind = behind_hosts(hosts);
    if behind.is_empty() {
        return None;
    }
    let payload = json!({
        "latest_version": latest,
        "behind_hosts": behind,
        "source_url": LATEST_URL,
        "releases_url": RELEASES_URL,
        "notes_complete": notes_complete,
        "note": "只追蹤與通知：沒有自動下載、herdr update 或 server restart。升級可能影響正在跑的 pane（0.9 起 client 更新可不動相容的 server；endpoint generation 較舊的 server 需要一次性升級，那次才會動到 pane），請確認版本相容性後安排時間窗。",
    });
    match crate::supervisor::store::push_inbox(&app.db, &event_key(latest), EVENT_KIND, None, None, None, &payload).await {
        Ok(Some(id)) => {
            tracing::info!(version = %latest, hosts = ?behind, "herdr 有新版，通知 AGM 巡檢");
            Some(id)
        }
        // 同一版已經通知過：這就是去重命中，不是錯誤。
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(version = %latest, error = %e, "herdr 新版通知寫不進 AGM 收件匣，下一輪再試");
            None
        }
    }
}

// ---------------------------------------------------------------- 對外快照

/// 這個時間戳是不是「還算數」。認不出、沒有，都算過期——不知道不能當成剛確認過。
fn expired(at: Option<&str>, after: Duration) -> bool {
    let Some(at) = at.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) else { return true };
    (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).to_std().map(|d| d > after).unwrap_or(false)
}

/// 一筆讀數現在還算不算數。
///
/// `save_host` 只在讀到值的時候覆蓋版本，而 `error` 永遠是**這一輪**的結果，所以
/// 「有值且這輪沒出錯」就等於「這個值是剛確認過的」。整列太久沒巡過（daemon 停過、主機一直連不上）
/// 則不管哪一邊都不算數。
fn side_fresh(version: Option<&str>, error: Option<&str>, row_fresh: bool) -> bool {
    row_fresh && version.is_some() && error.is_none()
}

/// 一台主機的兩個版本。
///
/// `standing` 是**現在**的判斷，`cached_standing` 是「上次讀到的那個版本對現在已知的最新」。兩者分開的
/// 理由：離線主機留著 0.8.0 這個歷史值可以顯示，但不能因此說它現在落後或現在最新——尤其不能綠燈。
/// 只要這邊的讀數不是剛確認過的、或官方最新版本身過期／不知道，`standing` 一律 `unknown`。
fn host_json(row: &HostRow, connected: bool, latest: Option<&str>, latest_fresh: bool) -> Value {
    let row_fresh = !expired(Some(row.checked_at.as_str()), STALE_AFTER);
    let s_fresh = side_fresh(row.server_version.as_deref(), row.server_error.as_deref(), row_fresh);
    let d_fresh = side_fresh(row.disk_version.as_deref(), row.disk_error.as_deref(), row_fresh);
    let judge = |fresh: bool, v: Option<&str>| if fresh && latest_fresh { standing(v, latest) } else { "unknown" };
    // 磁碟比跑著的新 = 新版已經裝好，換 server 才會生效。兩邊都要是剛確認過的才敢這樣說。
    let restart_pending = match (
        d_fresh.then(|| row.disk_version.as_deref().and_then(Ver::parse)).flatten(),
        s_fresh.then(|| row.server_version.as_deref().and_then(Ver::parse)).flatten(),
    ) {
        (Some(disk), Some(server)) => disk > server,
        _ => false,
    };
    json!({
        "host": row.host,
        "connected": connected,
        "checked_at": (!row.checked_at.is_empty()).then(|| row.checked_at.clone()),
        // 兩邊都是剛確認過的，這張卡才算「現在的實況」。
        "fresh": s_fresh && d_fresh,
        "server": {
            "version": row.server_version,
            "protocol": row.server_protocol,
            "at": row.server_at,
            "error": row.server_error,
            "fresh": s_fresh,
            "standing": judge(s_fresh, row.server_version.as_deref()),
            "cached_standing": standing(row.server_version.as_deref(), latest),
        },
        "disk": {
            "version": row.disk_version,
            "at": row.disk_at,
            "error": row.disk_error,
            "fresh": d_fresh,
            "standing": judge(d_fresh, row.disk_version.as_deref()),
            "cached_standing": standing(row.disk_version.as_deref(), latest),
        },
        "restart_pending": restart_pending,
    })
}

fn side_standing<'a>(h: &'a Value, side: &str) -> &'a str {
    h.get(side).and_then(|x| x.get("standing")).and_then(Value::as_str).unwrap_or("unknown")
}

fn host_name(h: &Value) -> String {
    h.get("host").and_then(Value::as_str).unwrap_or_default().to_string()
}

/// 任一邊**現在**未知（含資料過期、官方版本不明）的主機。這些絕不能被算進「都最新」。
fn unknown_hosts(hosts: &[Value]) -> Vec<String> {
    hosts
        .iter()
        .filter(|h| side_standing(h, "server") == "unknown" || side_standing(h, "disk") == "unknown")
        .map(host_name)
        .collect()
}

/// 讀數已經不算數的主機（離線、探測失敗、整列太久沒巡）。歷史版本照顯示，但不能當成現況。
fn stale_hosts(hosts: &[Value]) -> Vec<String> {
    hosts.iter().filter(|h| h.get("fresh").and_then(Value::as_bool) != Some(true)).map(host_name).collect()
}

/// 永遠 200；抓不到、問不到都寫在回應裡，UI 照實顯示，不會靜默變成「已是最新」。
///
/// 主機清單以 **`app.hosts`（設定檔）為準**，DB 只是快取：已經從設定移除的主機不再投影（否則它會永遠
/// 參與落後判斷與通知），剛加進來還沒巡到的主機則以「未知」出現，而不是整台消失。
pub async fn snapshot(app: &Arc<App>) -> Value {
    let row = load_latest(&app.db).await;
    let manifest = row.body_json.as_deref().and_then(|b| parse_manifest(b).ok());
    let latest = row.version.clone();
    let latest_stale = expired(row.fetched_at.as_deref(), STALE_AFTER);
    // 十幾個小時沒確認過的「最新版」不能拿來給任何人發綠燈。
    let latest_fresh = latest.is_some() && !latest_stale;
    let cached: HashMap<String, HostRow> = load_hosts(&app.db).await.into_iter().map(|r| (r.host.clone(), r)).collect();
    let mut hosts: Vec<Value> = Vec::new();
    for c in app.hosts.list().await {
        let connected = if c.is_local() { app.connected.load(std::sync::atomic::Ordering::SeqCst) } else { c.is_connected() };
        let row = cached.get(&c.name).cloned().unwrap_or_else(|| HostRow::never_probed(&c.name));
        hosts.push(host_json(&row, connected, latest.as_deref(), latest_fresh));
    }
    // 版本差距從**所有主機裡最舊的已知版本**算起，才不會漏掉落後最多的那台。
    let oldest = hosts
        .iter()
        .flat_map(|h| ["server", "disk"].map(|k| h.get(k).and_then(|x| x.get("version")).and_then(Value::as_str).map(str::to_string)))
        .flatten()
        .filter_map(|v| Ver::parse(&v).map(|p| (p, v)))
        .min_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, v)| v);
    let notes = match (&manifest, &latest) {
        (Some(m), Some(l)) => notes_between(m, oldest.as_deref(), l),
        _ => NotesView::default(),
    };
    let behind = behind_hosts(&hosts);
    let notice = match &latest {
        Some(l) => json!({
            "version": l,
            "notified_at": notified_at(&app.db, l).await,
            "seen_at": seen_at(&app.db, l).await,
        }),
        None => Value::Null,
    };
    let unread = match &latest {
        Some(l) => !behind.is_empty() && seen_at(&app.db, l).await.is_none(),
        None => false,
    };
    json!({
        "latest": {
            "version": latest,
            "protocol": row.protocol,
            "endpoint_generation": row.endpoint_generation,
            "fetched_at": row.fetched_at,
            "checked_at": row.checked_at,
            "stale": latest_stale,
            "error": row.error,
            "source_url": LATEST_URL,
            "releases_url": RELEASES_URL,
        },
        "hosts": hosts,
        "behind_hosts": behind,
        "unknown_hosts": unknown_hosts(&hosts),
        "stale_hosts": stale_hosts(&hosts),
        "notes": {
            "from": notes.from,
            "to": (!notes.to.is_empty()).then_some(notes.to.clone()),
            "complete": notes.complete,
            "gap": notes.gap,
            "missing_notes": notes.missing_notes,
            "sections": notes.sections.iter().map(|s| json!({"version": s.version, "notes": s.notes})).collect::<Vec<_>>(),
        },
        "notice": notice,
        "unread": unread,
        // read-only：UI 最多只有「重新檢查」與官方連結。
        "read_only": true,
    })
}

// ---------------------------------------------------------------- 排程

/// 查詢閘門：同時進來的呼叫排隊，而且任何來源在 [`MIN_GAP`] 內都不重打官方站。
///
/// 跟 [`crate::update_watch`] 的磁碟快取一樣用 process static，不進 `App`：這支只有自己會用。
fn gate() -> &'static tokio::sync::Mutex<Option<std::time::Instant>> {
    static G: std::sync::OnceLock<tokio::sync::Mutex<Option<std::time::Instant>>> = std::sync::OnceLock::new();
    G.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// 這次該不該真的打官方站。
///
/// **手動與排程一視同仁**：排程若不受約束，多一個呼叫端（或不小心起了兩個 watcher）就會連打官方站。
/// 排程自己是 6 小時一輪，永遠不會撞到這個下限；真正被擋下來的只有連點與併發。
fn throttled(last: Option<std::time::Instant>) -> bool {
    last.map(|t| t.elapsed() < MIN_GAP).unwrap_or(false)
}

/// 查一輪：官方 manifest + 每台主機的兩個版本 + 需要時一筆 AGM 事件。
///
/// 併發呼叫會在閘門的鎖上等前一個做完，然後看到它剛寫下的時間戳直接回頭——所以 N 個同時的
/// refresh 只會有一次外部請求。
pub async fn refresh(app: &Arc<App>) {
    let mut last = gate().lock().await;
    if throttled(*last) {
        return;
    }
    *last = Some(std::time::Instant::now());
    match fetch_body(&source_url(), HTTP_TIMEOUT, MAX_BODY).await.and_then(|b| parse_manifest(&b).map(|m| (m, b))) {
        Ok((m, body)) => {
            if let Err(e) = save_latest_ok(&app.db, &m, &body).await {
                tracing::warn!(error = %e, "herdr latest 寫不進 DB");
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            tracing::warn!(error = %msg, "查 herdr 最新版失敗，保留上次的結果");
            if let Err(e) = save_latest_err(&app.db, &msg).await {
                tracing::warn!(error = %e, "herdr latest 錯誤寫不進 DB");
            }
        }
    }
    // 各主機平行探：一台 ssh 逾時（最久 20 秒）不該把整輪拖成「主機數 × 20 秒」，
    // 手動 refresh 那條 HTTP 請求還等在後面。
    let mut probes = tokio::task::JoinSet::new();
    for conn in app.hosts.list().await {
        let app = app.clone();
        probes.spawn(async move { probe_host(&app, &conn, HERDR_BIN).await });
    }
    while let Some(done) = probes.join_next().await {
        let Ok(p) = done else { continue };
        if let Err(e) = save_host(&app.db, &p).await {
            tracing::warn!(host = %p.host, error = %e, "herdr 主機版本寫不進 DB");
        }
    }
    // 通知的判斷只看剛寫下的持久狀態，所以重啟後接著算也一樣。
    let snap = snapshot(app).await;
    let (Some(latest), Some(hosts)) = (
        snap.get("latest").and_then(|l| l.get("version")).and_then(Value::as_str),
        snap.get("hosts").and_then(Value::as_array),
    ) else {
        return;
    };
    let complete = snap.get("notes").and_then(|n| n.get("complete")).and_then(Value::as_bool).unwrap_or(false);
    notify_if_new(app, latest, hosts, complete).await;
}

/// 啟動查一次，之後每 6 小時一次。不吃 LLM、不碰 controller 的 10 秒迴圈。
pub fn spawn_watcher(app: Arc<App>) {
    tokio::spawn(async move {
        // 讓主機連線、herdr socket 先就位，第一輪才問得到跑著的版本。
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            refresh(&app).await;
            tokio::time::sleep(POLL).await;
        }
    });
}

// ---------------------------------------------------------------- HTTP

/// `GET /api/herdr/updates`：永遠 200，錯誤與「不知道」都寫在 payload 裡（docs/API.md）。
pub async fn http_get(State(app): State<Arc<App>>) -> Json<Value> {
    Json(snapshot(&app).await)
}

/// `POST /api/herdr/updates/refresh`：手動重查。節流與併發合流在 [`refresh`] 裡，
/// 被節流掉時照樣回目前快照（不是錯誤）。
pub async fn http_refresh(State(app): State<Arc<App>>) -> Json<Value> {
    refresh(&app).await;
    Json(snapshot(&app).await)
}

#[derive(serde::Deserialize)]
pub struct SeenBody {
    pub version: String,
}

/// `POST /api/herdr/updates/seen`：使用者按掉這一版的未讀提示。只動 UI 未讀，不影響 AGM 那筆事件。
pub async fn http_seen(State(app): State<Arc<App>>, Json(b): Json<SeenBody>) -> Result<Json<Value>, crate::lifecycle::LcError> {
    let v = b.version.trim();
    // 認不出版本就不寫：這張表是 UI 未讀的唯一依據，不收垃圾 key。
    if Ver::parse(v).is_none() {
        return Err(crate::lifecycle::LcError::Bad("version 看不出是哪一版".into()));
    }
    mark_seen(&app.db, v).await.map_err(|e| crate::lifecycle::LcError::Upstream(format!("{e:#}")))?;
    Ok(Json(snapshot(&app).await))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn v(s: &str) -> Ver {
        Ver::parse(s).unwrap_or_else(|| panic!("`{s}` 應該解得出來"))
    }

    /// 外部來源與查詢閘門都是 process 層的單例，測試之間要排隊，否則互相看到對方的時間戳。
    fn http_lock() -> &'static tokio::sync::Mutex<()> {
        static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(Default::default)
    }

    // ---------------- 版本比較 ----------------

    #[test]
    fn versions_compare_numerically_including_ten_vs_nine() {
        // 字串比較會說 0.9.10 < 0.9.9。
        assert!(v("0.9.10") > v("0.9.9"));
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("0.9.0") > v("0.8.2"));
        // 位數不同補 0。
        assert_eq!(v("0.9"), v("0.9.0"));
        assert_eq!(v("1.0.0"), v("v1.0.0"));
        assert_eq!(standing(Some("0.9.0"), Some("0.9.0")), "latest");
        assert_eq!(standing(Some("0.9.1"), Some("0.9.0")), "ahead");
        assert_eq!(standing(Some("0.8.2"), Some("0.9.0")), "behind");
    }

    #[test]
    fn prerelease_sorts_below_its_release_and_by_identifier() {
        assert!(v("0.9.0-rc.1") < v("0.9.0"));
        assert!(v("0.9.0-rc.2") > v("0.9.0-rc.1"));
        // 數字段 < 文字段（semver §11.4.3）。
        assert!(v("0.9.0-1") < v("0.9.0-alpha"));
        assert!(!v("0.9.0-rc.1").is_stable());
        assert!(v("0.9.0").is_stable());
    }

    #[test]
    fn unparsable_versions_are_unknown_never_up_to_date() {
        for bad in ["", "unknown", "nightly", "v", "0.9.0-", "x.y.z"] {
            assert!(Ver::parse(bad).is_none(), "`{bad}` 不該解出版本");
        }
        assert_eq!(standing(None, Some("0.9.0")), "unknown");
        assert_eq!(standing(Some("0.8.2"), None), "unknown");
        assert_eq!(standing(Some("nightly"), Some("0.9.0")), "unknown");
        assert_eq!(standing(None, None), "unknown");
    }

    #[test]
    fn version_comes_out_of_a_cli_banner() {
        // `herdr --version` 印的是 `herdr 0.8.2`：版本不在第一個 token。
        assert_eq!(v("herdr 0.8.2"), v("0.8.2"));
        assert_eq!(v("0.9.0+build5"), v("0.9.0"));
        assert_eq!(version_token("herdr 0.8.2").as_deref(), Some("0.8.2"));
        assert_eq!(version_token("herdr 0.9.0-rc.1").as_deref(), Some("0.9.0-rc.1"), "prerelease 後綴要留著");
        assert_eq!(version_token("2.1.5 (Claude Code)").as_deref(), Some("2.1.5"));
        assert_eq!(version_token("herdr"), None);
    }

    // ---------------- 磁碟探測（跑真正的 production helper）----------------

    /// 一個假的 CLI：`--version` 印 `line`，然後用 `code` 離開。`line` 空字串 = 什麼都不印。
    fn fake_cli(dir: &std::path::Path, name: &str, line: &str, code: i32, sleep_secs: u32) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        let body = format!(
            "#!/bin/sh\n[ {sleep_secs} -gt 0 ] && sleep {sleep_secs}\n[ -n '{line}' ] && echo '{line}'\nexit {code}\n"
        );
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().to_string()
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("am-herdr-cli-{}", crate::db::ulid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 2026-09-16 父 review 抓到的真 bug：磁碟探測走 `changelog::installed_version`，它末端的
    /// `version_string` 只認「版本在第一個 token」，`herdr 0.8.2` 的第一個 token 是 `herdr` →
    /// 永遠 `Err`，磁碟版本永遠顯示未知。這裡跑的是**正式那條探測管道**（`version_line`
    /// + 這個模組自己的解析），不是只測 `Ver::parse`。
    #[tokio::test]
    async fn the_real_disk_probe_reads_a_herdr_banner_instead_of_choking_on_the_name() {
        let env = crate::testing::env().await;
        let dir = tmpdir();

        let ok = fake_cli(&dir, "herdr-ok", "herdr 0.8.2", 0, 0);
        let line = crate::changelog::version_line(&env.app, "local", &ok, Duration::from_secs(10)).await.unwrap();
        assert_eq!(line, "herdr 0.8.2");
        assert_eq!(version_token(&line).as_deref(), Some("0.8.2"), "這就是以前拿不到的那個值");
        // 對照：舊的解析路徑（claude 用的那支）在同一行輸出上仍然是 Err，所以不能共用。
        assert!(crate::changelog::version_string(&line).is_none(), "version_string 吃不下 herdr 的招牌");

        let rc = fake_cli(&dir, "herdr-rc", "herdr 0.9.0-rc.1", 0, 0);
        let line = crate::changelog::version_line(&env.app, "local", &rc, Duration::from_secs(10)).await.unwrap();
        assert_eq!(version_token(&line).as_deref(), Some("0.9.0-rc.1"));
        assert!(!Ver::parse(&line).unwrap().is_stable(), "prerelease 認得出來，才不會被推薦成 stable");

        // claude 的招牌沒有被弄壞。
        let claude = fake_cli(&dir, "claude-ok", "2.1.5 (Claude Code)", 0, 0);
        assert_eq!(crate::changelog::installed_version(&env.app, "local", &claude).await.unwrap(), "2.1.5");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_disk_probe_that_fails_is_an_error_not_a_version() {
        let env = crate::testing::env().await;
        let dir = tmpdir();

        // 離開碼非 0 且沒有輸出。
        let dead = fake_cli(&dir, "herdr-dead", "", 3, 0);
        assert!(crate::changelog::version_line(&env.app, "local", &dead, Duration::from_secs(10)).await.is_err());

        // 跑得起來但什麼都不印。
        let silent = fake_cli(&dir, "herdr-silent", "", 0, 0);
        assert!(crate::changelog::version_line(&env.app, "local", &silent, Duration::from_secs(10)).await.is_err());

        // 根本沒有這支程式。
        let missing = dir.join("herdr-nope").to_string_lossy().to_string();
        assert!(crate::changelog::version_line(&env.app, "local", &missing, Duration::from_secs(10)).await.is_err());

        // 印了東西但看不出版本 → 探測本身成功，解析失敗；兩者要分得開。
        let junk = fake_cli(&dir, "herdr-junk", "herdr build unknown", 0, 0);
        let line = crate::changelog::version_line(&env.app, "local", &junk, Duration::from_secs(10)).await.unwrap();
        assert!(Ver::parse(&line).is_none());

        // 卡住不回 → 逾時（真的跑過逾時那條路，不是假裝）。
        let hang = fake_cli(&dir, "herdr-hang", "herdr 0.9.0", 0, 30);
        let e = crate::changelog::version_line(&env.app, "local", &hang, Duration::from_millis(600)).await.unwrap_err();
        assert!(format!("{e:#}").contains("timed out"), "要講得出是逾時：{e:#}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 正式那顆 `probe_host` 本身：磁碟那半要真的把 `herdr 0.8.2` 讀成 0.8.2。
    /// 只測 `Ver::parse` 抓不到這條接線——以前它接的是 `installed_version`，那支永遠回 Err。
    #[tokio::test]
    async fn probe_host_reads_the_disk_version_and_says_why_when_the_server_answer_is_unusable() {
        let env = crate::testing::env().await;
        let dir = tmpdir();
        let bin = fake_cli(&dir, "herdr", "herdr 0.8.2", 0, 0);
        let conn = env.app.hosts.get("local").await.unwrap();

        let p = probe_host(&env.app, &conn, &bin).await;
        assert_eq!(p.disk_version.as_deref(), Some("0.8.2"), "磁碟版本不能再是未知");
        assert!(p.disk_error.is_none());
        // mock herdr 的 ping 回 `version: "mock"`：認不出來就是認不出來，要講原因、不能猜。
        assert_eq!(p.server_version, None);
        assert!(p.server_error.as_deref().is_some_and(|e| e.contains("mock")), "{:?}", p.server_error);
        assert_eq!(p.server_protocol, Some(20), "protocol 還是讀得到");

        // 存進去之後，磁碟那一側是「剛確認過」的，server 那側不是。
        save_host(&env.app.db, &p).await.unwrap();
        let row = load_hosts(&env.app.db).await.remove(0);
        let j = host_json(&row, true, Some("0.9.0"), true);
        assert_eq!(j["disk"]["standing"], "behind", "0.8.2 對 0.9.0");
        assert_eq!(j["server"]["standing"], "unknown");

        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------- manifest ----------------

    /// 2026-09-16 實測的真實 schema（頂層 + `releases`），不是計畫書上猜的欄位名。
    const FIXTURE: &str = include_str!("../tests/fixtures/herdr-latest.json");

    #[test]
    fn the_real_manifest_parses_and_keeps_every_release_note() {
        let m = parse_manifest(FIXTURE).unwrap();
        assert_eq!(m.latest, "0.9.0");
        assert_eq!(m.protocol, Some(22));
        assert_eq!(m.endpoint_generation, Some(1));
        assert!(m.releases.len() > 40, "releases 應該有幾十筆，拿到 {}", m.releases.len());
        assert!(m.releases["0.8.2"].notes.as_ref().is_some_and(|n| !n.is_empty()));
        assert_eq!(m.releases["0.8.2"].protocol, Some(20));
    }

    #[test]
    fn a_prerelease_at_the_top_is_not_what_stable_gets_recommended() {
        let body = r#"{"version":"0.10.0-rc.1","protocol":23,"notes":"rc","releases":{
            "0.10.0-rc.1":{"notes":"rc","protocol":23},
            "0.9.0":{"notes":"stable","protocol":22},
            "0.8.2":{"notes":"older","protocol":20}}}"#;
        let m = parse_manifest(body).unwrap();
        assert_eq!(m.latest, "0.9.0", "stable 追蹤不推 rc");
        assert!(!m.releases.contains_key("0.10.0-rc.1"), "prerelease 不進清單");
        assert_eq!(m.protocol, Some(22), "protocol 要跟著那一版，不是頂層的");
    }

    #[test]
    fn broken_or_stableless_manifests_are_errors_so_the_cache_survives() {
        assert!(parse_manifest("").is_err());
        assert!(parse_manifest("{").is_err(), "截斷的 JSON");
        assert!(parse_manifest("[1,2]").is_err(), "不是物件");
        assert!(parse_manifest(r#"{"protocol":22}"#).is_err(), "沒有版本");
        assert!(parse_manifest(r#"{"version":"0.10.0-rc.1","releases":{}}"#).is_err(), "只有 prerelease");
        assert_eq!(parse_manifest(r#"{"version":"0.9.0","notes":"x"}"#).unwrap().latest, "0.9.0");
    }

    // ---------------- release notes ----------------

    #[test]
    fn notes_cover_the_gap_newest_first_and_never_borrow_another_versions_text() {
        let m = parse_manifest(FIXTURE).unwrap();
        let view = notes_between(&m, Some("0.8.0"), "0.9.0");
        assert!(view.complete, "0.8.0 在官方清單裡，而且每一段都有 notes");
        assert_eq!(view.sections.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["0.9.0", "0.8.2"]);
        assert!(view.sections[0].notes.as_ref().unwrap().contains("Added"));
        assert!(view.gap.is_none());
        assert!(notes_between(&m, Some("0.9.0"), "0.9.0").sections.is_empty());
    }

    #[test]
    fn a_missing_middle_version_is_said_out_loud_not_papered_over() {
        let body = r#"{"version":"0.9.0","releases":{
            "0.9.0":{"notes":"new"},
            "0.8.5":{},
            "0.8.0":{"notes":"old"}}}"#;
        let m = parse_manifest(body).unwrap();
        let unknown_from = notes_between(&m, Some("0.8.3"), "0.9.0");
        assert!(!unknown_from.complete);
        assert!(unknown_from.gap.as_ref().unwrap().contains("0.8.3"));
        assert_eq!(unknown_from.sections.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["0.9.0", "0.8.5"]);
        // 0.8.5 在清單裡但沒有 notes：明講它缺，不要拿 0.9.0 那段頂替。
        assert_eq!(unknown_from.missing_notes, ["0.8.5"]);
        assert!(unknown_from.sections.iter().find(|s| s.version == "0.8.5").unwrap().notes.is_none());
        assert!(unknown_from.gap.as_ref().unwrap().contains("0.8.5"), "兩個原因都要講：{:?}", unknown_from.gap);

        let no_from = notes_between(&m, None, "0.9.0");
        assert_eq!(no_from.sections.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["0.9.0"]);
        assert!(!no_from.complete);
        assert!(no_from.gap.as_ref().unwrap().contains("不知道目前是哪一版"));
    }

    /// 起點在清單裡、但中間某一版沒有 notes：`complete` 也要是 false。
    /// 只檢查「`from` 在不在」會給出一個誤導的完整旗標（2026-09-16 父 review）。
    #[test]
    fn a_listed_start_with_a_note_less_version_in_between_is_still_incomplete() {
        let body = r#"{"version":"0.9.0","releases":{
            "0.9.0":{"notes":"new"},
            "0.8.5":{},
            "0.8.0":{"notes":"start"}}}"#;
        let m = parse_manifest(body).unwrap();
        let view = notes_between(&m, Some("0.8.0"), "0.9.0");
        assert_eq!(view.missing_notes, ["0.8.5"]);
        assert!(!view.complete, "少一段內容就不是完整的跨版差距");
        assert!(view.gap.as_ref().unwrap().contains("0.8.5"));
        // 全部都有 notes 才算完整。
        let full = r#"{"version":"0.9.0","releases":{"0.9.0":{"notes":"a"},"0.8.5":{"notes":"b"},"0.8.0":{"notes":"c"}}}"#;
        let view = notes_between(&parse_manifest(full).unwrap(), Some("0.8.0"), "0.9.0");
        assert!(view.complete);
        assert!(view.gap.is_none());
    }

    // ---------------- DB ----------------

    async fn pool() -> SqlitePool {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        migrate(&p).await.unwrap();
        p
    }

    fn fresh_probe(host: &str, server: Option<&str>, disk: Option<&str>) -> HostProbe {
        HostProbe {
            host: host.to_string(),
            server_version: server.map(str::to_string),
            server_protocol: server.map(|_| 20),
            disk_version: disk.map(str::to_string),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_last_good_answer_and_still_records_why() {
        let p = pool().await;
        let m = parse_manifest(FIXTURE).unwrap();
        save_latest_ok(&p, &m, FIXTURE).await.unwrap();
        let ok = load_latest(&p).await;
        assert_eq!(ok.version.as_deref(), Some("0.9.0"));
        assert!(ok.error.is_none());

        save_latest_err(&p, "HTTP 503").await.unwrap();
        let after = load_latest(&p).await;
        assert_eq!(after.version.as_deref(), Some("0.9.0"), "壞掉的一輪不能吞掉舊快取");
        assert_eq!(after.body_json.as_deref(), Some(FIXTURE), "notes 也要留著");
        assert_eq!(after.fetched_at, ok.fetched_at, "上次成功的時間不動");
        assert_eq!(after.error.as_deref(), Some("HTTP 503"), "但要說得出這次為什麼沒查到");
    }

    #[test]
    fn a_never_fetched_or_long_stale_timestamp_is_expired() {
        assert!(expired(None, STALE_AFTER), "沒抓過就是過期");
        assert!(expired(Some("not a date"), STALE_AFTER));
        let old = (chrono::Utc::now() - chrono::Duration::hours(20)).to_rfc3339();
        assert!(expired(Some(&old), STALE_AFTER));
        assert!(!expired(Some(&chrono::Utc::now().to_rfc3339()), STALE_AFTER));
    }

    #[tokio::test]
    async fn an_offline_host_keeps_its_last_known_versions_and_is_never_given_the_local_ones() {
        let p = pool().await;
        save_host(&p, &fresh_probe("box", Some("0.8.2"), Some("0.8.2"))).await.unwrap();
        let first = load_hosts(&p).await;
        assert_eq!(first[0].server_version.as_deref(), Some("0.8.2"));
        let stamp = first[0].server_at.clone();

        save_host(
            &p,
            &HostProbe {
                host: "box".into(),
                server_error: Some("主機未連線".into()),
                disk_error: Some("ssh 失敗".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let row = &load_hosts(&p).await[0];
        assert_eq!(row.server_version.as_deref(), Some("0.8.2"), "上次讀到的值要留著");
        assert_eq!(row.disk_version.as_deref(), Some("0.8.2"));
        assert_eq!(row.server_at, stamp, "但版本的時間戳不能往前跳");
        assert_eq!(row.server_error.as_deref(), Some("主機未連線"));
    }

    // ---------------- 誠實度：過期／出錯的讀數不得發綠燈 ----------------

    #[tokio::test]
    async fn a_host_we_cannot_reach_is_unknown_now_even_though_we_remember_its_version() {
        let p = pool().await;
        save_host(&p, &fresh_probe("box", Some("0.9.0"), Some("0.9.0"))).await.unwrap();
        // 下一輪連不上：版本留著（0.9.0，剛好等於官方最新），但那是歷史。
        save_host(
            &p,
            &HostProbe { host: "box".into(), server_error: Some("主機未連線".into()), disk_error: Some("ssh 失敗".into()), ..Default::default() },
        )
        .await
        .unwrap();
        let row = load_hosts(&p).await.remove(0);
        let j = host_json(&row, false, Some("0.9.0"), true);
        assert_eq!(j["server"]["standing"], "unknown", "連不上就不能說它現在是最新的");
        assert_eq!(j["disk"]["standing"], "unknown");
        assert_eq!(j["fresh"], false);
        // 歷史值與它當時的比較結果仍看得到，UI 才能寫「上次讀到 0.9.0」。
        assert_eq!(j["server"]["version"], "0.9.0");
        assert_eq!(j["server"]["cached_standing"], "latest");
        assert_eq!(unknown_hosts(&[j.clone()]), ["box"]);
        assert_eq!(stale_hosts(&[j.clone()]), ["box"]);
        assert!(behind_hosts(&[j]).is_empty(), "不知道不叫醒");
    }

    #[tokio::test]
    async fn a_stale_latest_cannot_hand_out_green_lights_either() {
        let p = pool().await;
        save_host(&p, &fresh_probe("local", Some("0.9.0"), Some("0.9.0"))).await.unwrap();
        let row = load_hosts(&p).await.remove(0);
        // 主機讀數是新的，但官方最新版是十幾個小時前查到的 → 比較的基準本身不算數。
        let j = host_json(&row, true, Some("0.9.0"), false);
        assert_eq!(j["server"]["standing"], "unknown");
        assert_eq!(j["disk"]["standing"], "unknown");
        assert_eq!(j["server"]["fresh"], true, "主機那邊確實是剛讀到的");
        assert_eq!(unknown_hosts(&[j]), ["local"]);
    }

    #[tokio::test]
    async fn a_row_nobody_has_visited_for_hours_is_stale_even_without_an_error() {
        let p = pool().await;
        save_host(&p, &fresh_probe("box", Some("0.9.0"), Some("0.9.0"))).await.unwrap();
        // daemon 停了一天：沒有錯誤，但也沒人再確認過。
        let old = (chrono::Utc::now() - chrono::Duration::hours(30)).to_rfc3339();
        sqlx::query("UPDATE herdr_host_versions SET checked_at = ? WHERE host = 'box'").bind(&old).execute(&p).await.unwrap();
        let row = load_hosts(&p).await.remove(0);
        let j = host_json(&row, true, Some("0.9.0"), true);
        assert_eq!(j["server"]["standing"], "unknown", "沒有錯誤不代表資料還算數");
        assert_eq!(j["fresh"], false);
    }

    #[tokio::test]
    async fn hosts_are_tracked_apart() {
        let p = pool().await;
        save_host(&p, &fresh_probe("local", Some("0.9.0"), Some("0.9.0"))).await.unwrap();
        save_host(&p, &fresh_probe("box", Some("0.8.2"), Some("0.8.2"))).await.unwrap();
        let rows = load_hosts(&p).await;
        let by = |n: &str| rows.iter().find(|r| r.host == n).unwrap().clone();
        let json = [host_json(&by("local"), true, Some("0.9.0"), true), host_json(&by("box"), true, Some("0.9.0"), true)];
        assert_eq!(behind_hosts(&json), ["box"], "一台落後不會把另一台也說成落後");
        assert_eq!(json[0]["server"]["standing"], "latest");
        assert!(unknown_hosts(&json).is_empty());
    }

    #[tokio::test]
    async fn a_newer_cli_on_disk_than_the_running_server_is_waiting_for_a_server_swap() {
        let p = pool().await;
        save_host(&p, &fresh_probe("local", Some("0.8.2"), Some("0.9.0"))).await.unwrap();
        let row = load_hosts(&p).await.remove(0);
        let j = host_json(&row, true, Some("0.9.0"), true);
        assert_eq!(j["restart_pending"], true);
        assert_eq!(j["disk"]["standing"], "latest", "磁碟已是最新");
        assert_eq!(j["server"]["standing"], "behind", "但跑著的還是舊的");

        // 只有一邊知道版本時不猜。
        save_host(
            &p,
            &HostProbe { host: "half".into(), disk_version: Some("0.9.0".into()), server_error: Some("問不到".into()), ..Default::default() },
        )
        .await
        .unwrap();
        let half = load_hosts(&p).await.into_iter().find(|r| r.host == "half").unwrap();
        assert_eq!(host_json(&half, true, Some("0.9.0"), true)["restart_pending"], false);
    }

    #[tokio::test]
    async fn the_user_can_mark_a_version_seen_without_touching_the_agm_event() {
        let p = pool().await;
        assert!(seen_at(&p, "0.9.0").await.is_none());
        mark_seen(&p, "0.9.0").await.unwrap();
        let first = seen_at(&p, "0.9.0").await.unwrap();
        mark_seen(&p, "0.9.0").await.unwrap();
        assert_eq!(seen_at(&p, "0.9.0").await.as_deref(), Some(first.as_str()), "按第二次不改時間");
        assert!(seen_at(&p, "0.9.1").await.is_none(), "下一版又是未讀");
    }

    // ---------------- 通知 ----------------

    /// 這筆事件要走巡檢、而且要叫醒它——靠的是 `roles::route` 對沒列出來的 kind 的預設行為，
    /// 所以在這裡釘住：哪天預設改了，這個測試會先壞，而不是通知悄悄消失。
    #[test]
    fn the_event_goes_to_the_patrol_role_and_wakes_it() {
        let r = crate::supervisor::roles::route(EVENT_KIND, &json!({}), None);
        assert_eq!(r.role, crate::supervisor::roles::Role::Patrol);
        assert!(r.wake, "有新版要叫醒巡檢");
    }

    #[test]
    fn nothing_is_notified_when_no_host_is_known_to_be_behind() {
        let up_to_date = [json!({"host":"local","server":{"standing":"latest"},"disk":{"standing":"latest"}})];
        assert!(behind_hosts(&up_to_date).is_empty(), "沒更新不叫醒模型");
        let unknown = [json!({"host":"local","server":{"standing":"unknown"},"disk":{"standing":"unknown"}})];
        assert!(behind_hosts(&unknown).is_empty(), "不知道也不叫醒（但 UI 會顯示未知）");
        let one_side = [json!({"host":"local","server":{"standing":"latest"},"disk":{"standing":"behind"}})];
        assert_eq!(behind_hosts(&one_side), ["local"], "任一邊落後就算");
    }

    #[tokio::test]
    async fn one_release_is_one_event_across_restarts_and_many_panes() {
        let env = crate::testing::env().await;
        let app = &env.app;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let hosts = [json!({"host":"local","server":{"standing":"behind"},"disk":{"standing":"behind"}})];

        assert!(notify_if_new(app, "0.9.0", &hosts, true).await.is_some(), "第一次要寫進收件匣");
        assert!(notify_if_new(app, "0.9.0", &hosts, true).await.is_none());
        let restarted = crate::testing::restart_app(&env).await;
        assert!(notify_if_new(&restarted, "0.9.0", &hosts, true).await.is_none(), "重啟不能對同一版再叫醒一次");
        let open = crate::supervisor::store::open_inbox(&app.db, 50).await.unwrap();
        let mine: Vec<_> = open.iter().filter(|e| e.kind == EVENT_KIND).collect();
        assert_eq!(mine.len(), 1, "一個 release 最多一筆合併事件");
        assert_eq!(mine[0].event_key, "herdr_update:0.9.0");
        let payload: Value = serde_json::from_str(&mine[0].payload_json).unwrap();
        assert_eq!(payload["behind_hosts"], json!(["local"]));

        assert!(notify_if_new(app, "0.9.1", &hosts, true).await.is_some(), "下一版是新事件");
        assert!(notified_at(&app.db, "0.9.0").await.is_some());
        assert!(notified_at(&app.db, "0.9.2").await.is_none());
    }

    /// 通知寫失敗（這裡把收件匣整張表藏起來當故障注入）不能留下任何「已通知」的痕跡，
    /// 否則那一版就永遠不會再被送出。修好之後下一輪要補送。
    #[tokio::test]
    async fn a_notification_that_could_not_be_written_is_retried_not_silently_marked_sent() {
        let env = crate::testing::env().await;
        let app = &env.app;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let hosts = [json!({"host":"local","server":{"standing":"behind"},"disk":{"standing":"behind"}})];

        sqlx::query("ALTER TABLE supervisor_inbox RENAME TO supervisor_inbox_broken").execute(&app.db).await.unwrap();
        assert!(notify_if_new(app, "0.9.0", &hosts, true).await.is_none(), "寫不進去就是沒送出");
        assert!(notified_at(&app.db, "0.9.0").await.is_none(), "不能留下已通知的假痕跡");

        sqlx::query("ALTER TABLE supervisor_inbox_broken RENAME TO supervisor_inbox").execute(&app.db).await.unwrap();
        assert!(notify_if_new(app, "0.9.0", &hosts, true).await.is_some(), "下一輪要補送");
        assert!(notified_at(&app.db, "0.9.0").await.is_some());
        // 補送之後仍然只有一筆。
        assert!(notify_if_new(app, "0.9.0", &hosts, true).await.is_none());
        let n = crate::supervisor::store::open_inbox(&app.db, 50).await.unwrap().iter().filter(|e| e.kind == EVENT_KIND).count();
        assert_eq!(n, 1);
    }

    // ---------------- 快照：主機清單以設定為準 ----------------

    fn host_cfg(name: &str) -> crate::config::HostCfg {
        crate::config::HostCfg {
            name: name.to_string(),
            ssh: format!("nobody@{name}.invalid"),
            ssh_port: 22,
            ssh_opts: Vec::new(),
            herdr_session: "test".into(),
            remote_path: String::new(),
        }
    }

    fn names(snap: &Value) -> Vec<String> {
        snap["hosts"].as_array().unwrap().iter().map(host_name).collect()
    }

    #[tokio::test]
    async fn the_snapshot_reports_unknown_rather_than_up_to_date_when_nothing_has_been_fetched() {
        let env = crate::testing::env().await;
        let snap = snapshot(&env.app).await;
        assert!(snap["latest"]["version"].is_null());
        assert_eq!(snap["latest"]["stale"], true);
        assert_eq!(snap["unread"], false, "什麼都不知道時不要跳未讀");
        assert_eq!(snap["read_only"], true);
        assert_eq!(snap["behind_hosts"], json!([]));
        assert!(snap["notes"]["sections"].as_array().unwrap().is_empty());
        // 設定裡有 local，所以它要出現——以「未知」出現，不是整台消失。
        assert_eq!(names(&snap), ["local"]);
        assert_eq!(snap["hosts"][0]["server"]["standing"], "unknown");
        assert_eq!(snap["unknown_hosts"], json!(["local"]));
    }

    /// 主機清單以 `app.hosts`（設定檔）為準：DB 只是快取。
    #[tokio::test]
    async fn a_host_removed_from_config_stops_counting_and_a_new_one_shows_up_as_unknown() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let m = parse_manifest(FIXTURE).unwrap();
        save_latest_ok(&app.db, &m, FIXTURE).await.unwrap();
        app.hosts.insert_disconnected_for_test(host_cfg("box")).await;
        save_host(&app.db, &fresh_probe("local", Some("0.9.0"), Some("0.9.0"))).await.unwrap();
        save_host(&app.db, &fresh_probe("box", Some("0.8.0"), Some("0.8.0"))).await.unwrap();

        let snap = snapshot(app).await;
        assert_eq!(names(&snap), ["local", "box"], "local 先，其餘照名字");
        assert_eq!(snap["behind_hosts"], json!(["box"]));
        assert_eq!(snap["notes"]["from"], "0.8.0", "差距從最舊的那台算起");

        // 從設定移除：DB 那一列還在，但不能再投影，也不能再把任何人算成落後。
        app.hosts.remove_for_test("box").await;
        let snap = snapshot(app).await;
        assert_eq!(names(&snap), ["local"]);
        assert_eq!(snap["behind_hosts"], json!([]));
        assert_eq!(snap["notes"]["from"], "0.9.0", "只剩 local 這台的版本");
        assert!(!load_hosts(&app.db).await.iter().any(|r| r.host == "box" && r.server_version.is_none()), "歷史留在 DB");

        // 剛加進來、還沒巡到的主機：以未知出現。
        app.hosts.insert_disconnected_for_test(host_cfg("fresh")).await;
        let snap = snapshot(app).await;
        assert_eq!(names(&snap), ["local", "fresh"]);
        let new_host = &snap["hosts"][1];
        assert_eq!(new_host["server"]["version"], Value::Null);
        assert_eq!(new_host["server"]["standing"], "unknown");
        assert_eq!(new_host["checked_at"], Value::Null, "沒巡過就沒有時間戳");
        assert_eq!(snap["unknown_hosts"], json!(["fresh"]));
        assert_eq!(snap["behind_hosts"], json!([]), "沒問到的主機不算落後");
    }

    #[tokio::test]
    async fn the_snapshot_spans_hosts_from_the_oldest_known_version() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let m = parse_manifest(FIXTURE).unwrap();
        save_latest_ok(&app.db, &m, FIXTURE).await.unwrap();
        app.hosts.insert_disconnected_for_test(host_cfg("box")).await;
        save_host(&app.db, &fresh_probe("local", Some("0.8.2"), Some("0.8.2"))).await.unwrap();
        save_host(&app.db, &fresh_probe("box", Some("0.8.0"), Some("0.8.0"))).await.unwrap();

        let snap = snapshot(app).await;
        assert_eq!(snap["latest"]["version"], "0.9.0");
        assert_eq!(snap["notes"]["from"], "0.8.0");
        let versions: Vec<&str> =
            snap["notes"]["sections"].as_array().unwrap().iter().map(|s| s["version"].as_str().unwrap()).collect();
        assert_eq!(versions, ["0.9.0", "0.8.2"]);
        assert_eq!(snap["behind_hosts"], json!(["local", "box"]));
        assert_eq!(snap["unread"], true);
        mark_seen(&app.db, "0.9.0").await.unwrap();
        assert_eq!(snapshot(app).await["unread"], false, "看過就不再未讀");
    }

    // ---------------- HTTP 故障注入（本機假 server，不碰外網）----------------

    enum Reply {
        Body(&'static str),
        Owned(String),
        Status(u16),
        /// 收到請求後不回，用來跑逾時那條路。
        Hang,
    }

    struct FakeSite {
        url: String,
        hits: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeSite {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// 一個只說 HTTP/1.1 的假站。刻意不是 mock：`fetch_body` 的逾時、狀態碼與大小上限都要真的跑過。
    async fn fake_site(reply: Reply) -> FakeSite {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/latest.json", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let hang = matches!(reply, Reply::Hang);
        let status = match reply {
            Reply::Status(s) => s,
            _ => 200,
        };
        let body = match reply {
            Reply::Body(b) => Some(b.to_string()),
            Reply::Owned(b) => Some(b),
            Reply::Status(_) | Reply::Hang => None,
        };
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                counter.fetch_add(1, Ordering::SeqCst);
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    if hang {
                        // 連線留著不回應：客戶端只能靠自己的逾時脫身。
                        tokio::time::sleep(Duration::from_secs(120)).await;
                        return;
                    }
                    let body = body.unwrap_or_default();
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        FakeSite { url, hits, task }
    }

    #[tokio::test]
    async fn a_hanging_server_times_out_instead_of_hanging_the_daemon() {
        let site = fake_site(Reply::Hang).await;
        let started = std::time::Instant::now();
        let e = fetch_body(&site.url, Duration::from_millis(400), MAX_BODY).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "要靠逾時脫身，不是一直等");
        let msg = format!("{e:#}");
        assert!(msg.contains("抓") && msg.contains("失敗"), "逾時要說得出來：{msg}");
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_mid_stream() {
        let site = fake_site(Reply::Owned(format!("{{\"version\":\"0.9.0\",\"pad\":\"{}\"}}", "x".repeat(20_000)))).await;
        let e = fetch_body(&site.url, Duration::from_secs(5), 1024).await.unwrap_err();
        assert!(format!("{e:#}").contains("超過"), "{e:#}");
        // 同一個站在上限夠大的時候是好的，證明擋下來的是大小不是連線。
        assert!(fetch_body(&site.url, Duration::from_secs(5), MAX_BODY).await.is_ok());
    }

    #[tokio::test]
    async fn a_bad_gateway_or_broken_json_never_eats_the_good_cache() {
        let _guard = http_lock().lock().await;
        let env = crate::testing::env().await;
        let app = &env.app;

        // 先成功一次（真的走 HTTP）。
        let good = fake_site(Reply::Body(r#"{"version":"0.9.0","protocol":22,"endpoint_generation":1,"notes":"n",
            "releases":{"0.9.0":{"notes":"n","protocol":22},"0.8.2":{"notes":"o","protocol":20}}}"#))
        .await;
        *source_override().lock().unwrap() = Some(good.url.clone());
        *gate().lock().await = None;
        refresh(app).await;
        let ok = load_latest(&app.db).await;
        assert_eq!(ok.version.as_deref(), Some("0.9.0"));
        assert_eq!(good.hits.load(Ordering::SeqCst), 1);

        for (label, reply) in [("壞 JSON", Reply::Body("{\"version\": ")), ("HTTP 503", Reply::Status(503))] {
            let bad = fake_site(reply).await;
            *source_override().lock().unwrap() = Some(bad.url.clone());
            *gate().lock().await = None;
            refresh(app).await;
            let after = load_latest(&app.db).await;
            assert_eq!(after.version.as_deref(), Some("0.9.0"), "{label} 不能吞掉舊快取");
            assert_eq!(after.body_json, ok.body_json, "{label} 之後 release notes 還在");
            assert_eq!(after.fetched_at, ok.fetched_at, "{label} 不是一次成功的查詢");
            assert!(after.error.is_some(), "{label} 要記下原因");
            // 快照仍然給得出上次的答案，只是標成舊資料。
            let snap = snapshot(app).await;
            assert_eq!(snap["latest"]["version"], "0.9.0");
            assert!(snap["latest"]["error"].is_string());
        }
        *source_override().lock().unwrap() = None;
        *gate().lock().await = None;
    }

    /// 真的同時打 N 個 refresh（不是只驗那顆 bool）：外部請求只能有一次。
    #[tokio::test]
    async fn concurrent_refreshes_hit_the_official_site_exactly_once() {
        let _guard = http_lock().lock().await;
        let env = crate::testing::env().await;
        let site = fake_site(Reply::Body(r#"{"version":"0.9.0","releases":{"0.9.0":{"notes":"n"}}}"#)).await;
        *source_override().lock().unwrap() = Some(site.url.clone());
        *gate().lock().await = None;

        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let app = env.app.clone();
            set.spawn(async move { refresh(&app).await });
        }
        while set.join_next().await.is_some() {}
        assert_eq!(site.hits.load(Ordering::SeqCst), 1, "八個併發只該打一次");

        // 排程那輪也受同一個下限約束——否則多一個呼叫端就會連打官方站。
        refresh(&env.app).await;
        assert_eq!(site.hits.load(Ordering::SeqCst), 1, "60 秒內不重打，排程也一樣");
        assert!(throttled(Some(std::time::Instant::now())));
        assert!(!throttled(Some(std::time::Instant::now() - MIN_GAP - Duration::from_secs(1))));
        assert!(!throttled(None));

        *source_override().lock().unwrap() = None;
        *gate().lock().await = None;
    }

    /// 端到端：真的抓一次假站、寫進 DB、投影成快照、推一筆 AGM 事件。
    #[tokio::test]
    async fn a_full_round_writes_the_cache_and_notifies_once() {
        let _guard = http_lock().lock().await;
        let env = crate::testing::env().await;
        let app = &env.app;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        // 本機已知落後（herdr server 那邊由假資料給；這一輪只驗 latest 與通知）。
        save_host(&app.db, &fresh_probe("local", Some("0.8.2"), Some("0.8.2"))).await.unwrap();

        let site = fake_site(Reply::Body(r#"{"version":"0.9.0","protocol":22,"endpoint_generation":1,
            "releases":{"0.9.0":{"notes":"Added - x","protocol":22},"0.8.2":{"notes":"old","protocol":20}}}"#))
        .await;
        *source_override().lock().unwrap() = Some(site.url.clone());
        *gate().lock().await = None;
        refresh(app).await;

        let snap = snapshot(app).await;
        assert_eq!(snap["latest"]["version"], "0.9.0");
        assert_eq!(snap["latest"]["stale"], false);
        assert!(snap["latest"]["error"].is_null());
        assert_eq!(snap["notes"]["sections"][0]["version"], "0.9.0");
        let n = crate::supervisor::store::open_inbox(&app.db, 50).await.unwrap().iter().filter(|e| e.kind == EVENT_KIND).count();
        assert_eq!(n, 1, "一輪一筆");

        *source_override().lock().unwrap() = None;
        *gate().lock().await = None;
    }
}
