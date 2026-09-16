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
//! 刻意不做的事：不下載、不 `herdr update`、不 `server stop/restart`。0.8 → 0.9 這種跨 protocol 的
//! 升級要停 server、會殺掉正在跑的 pane，那是 AGM 走既有運維流程的決定，不是背景巡邏可以順手做的。
//!
//! 通知走既有 AGM 巡檢路由（`supervisor_inbox`）：一個 release 一筆合併事件，`event_key` 就是去重標記，
//! 所以 daemon 重啟不會對同一版再叫醒 AGM 一次，一台主機開幾個 pane 也不會變成幾次通知。

use std::collections::BTreeMap;
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
/// 手動 refresh 的最小間隔：連點只會打一次外部請求。
const MANUAL_MIN_GAP: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
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
    /// `from` 那一版在官方清單裡找得到 = 中間有哪些版是清楚的。
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
    view.complete = match &from_v {
        None => false,
        Some(_) => from.is_some_and(|f| m.releases.keys().any(|k| Ver::parse(k).as_ref() == Ver::parse(f).as_ref())),
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
    if view.gap.is_none() && !view.complete {
        view.gap = Some(match from {
            Some(f) => format!("官方清單裡沒有 {f} 這一版，{f} 到 {to} 之間可能還有其他版本沒列出來"),
            None => format!("不知道目前是哪一版，只列得出 {to} 這一版的內容"),
        });
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

/// 串著讀並在超過 [`MAX_BODY`] 時中止：`Content-Length` 可以騙人，不能只信它。
async fn fetch_body(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent("agents-manager")
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| anyhow!("http client: {e}"))?;
    let mut resp = client.get(url).send().await.map_err(|e| anyhow!("抓 {url} 失敗：{e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("抓 {url} 失敗：HTTP {}", resp.status()));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| anyhow!("讀 {url} 失敗：{e}"))? {
        if buf.len() + chunk.len() > MAX_BODY {
            return Err(anyhow!("{url} 超過 {MAX_BODY} bytes，不收"));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| anyhow!("{url} 不是 UTF-8"))
}

/// 一台主機的兩個版本。**不會**因為讀不到就填別的東西：讀不到就是 `None` + 一句原因。
async fn probe_host(app: &Arc<App>, conn: &Arc<crate::hosts::HostConn>) -> HostProbe {
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
    // 磁碟上的 CLI 走既有的版本探測（本機 `/bin/sh`、遠端 ssh），read-only。
    match crate::changelog::installed_version(app, &conn.name, "herdr").await {
        Ok(v) => p.disk_version = Some(v),
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

/// 真的落後的主機（**已知**落後；未知不算）才值得叫醒模型。
fn behind_hosts(hosts: &[Value]) -> Vec<String> {
    hosts
        .iter()
        .filter(|h| {
            ["server", "disk"].iter().any(|k| h.get(*k).and_then(|x| x.get("standing")).and_then(Value::as_str) == Some("behind"))
        })
        .filter_map(|h| h.get("host").and_then(Value::as_str).map(str::to_string))
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
        "note": "只追蹤與通知：沒有自動下載、herdr update 或 server restart。升級要停 server 會殺掉正在跑的 pane，走既有運維流程。",
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

fn stale(fetched_at: Option<&str>) -> bool {
    let Some(at) = fetched_at.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) else { return true };
    (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).to_std().map(|d| d > STALE_AFTER).unwrap_or(false)
}

fn host_json(row: &HostRow, connected: bool, latest: Option<&str>) -> Value {
    let server_standing = standing(row.server_version.as_deref(), latest);
    let disk_standing = standing(row.disk_version.as_deref(), latest);
    // 磁碟比跑著的新 = 新版已經裝好，換 server 才會生效。
    let restart_pending = match (row.disk_version.as_deref().and_then(Ver::parse), row.server_version.as_deref().and_then(Ver::parse)) {
        (Some(d), Some(s)) => d > s,
        _ => false,
    };
    json!({
        "host": row.host,
        "connected": connected,
        "checked_at": row.checked_at,
        "server": {
            "version": row.server_version,
            "protocol": row.server_protocol,
            "at": row.server_at,
            "error": row.server_error,
            "standing": server_standing,
        },
        "disk": { "version": row.disk_version, "at": row.disk_at, "error": row.disk_error, "standing": disk_standing },
        "restart_pending": restart_pending,
    })
}

/// 永遠 200；抓不到、問不到都寫在回應裡，UI 照實顯示，不會靜默變成「已是最新」。
pub async fn snapshot(app: &Arc<App>) -> Value {
    let row = load_latest(&app.db).await;
    let manifest = row.body_json.as_deref().and_then(|b| parse_manifest(b).ok());
    let latest = row.version.clone();
    let conns = app.hosts.list().await;
    let hosts: Vec<Value> = load_hosts(&app.db)
        .await
        .iter()
        .map(|r| {
            let connected = conns
                .iter()
                .find(|c| c.name == r.host)
                .map(|c| {
                    if c.is_local() {
                        app.connected.load(std::sync::atomic::Ordering::SeqCst)
                    } else {
                        c.is_connected()
                    }
                })
                .unwrap_or(false);
            host_json(r, connected, latest.as_deref())
        })
        .collect();
    // 版本差距從**所有主機裡最舊的已知版本**算起，才不會漏掉落後最多的那台。
    let oldest = hosts
        .iter()
        .flat_map(|h| ["server", "disk"].map(|k| h.get(k).and_then(|x| x.get("version")).and_then(Value::as_str).map(str::to_string)))
        .flatten()
        .filter_map(|v| Ver::parse(&v).map(|p| (p, v)))
        .min_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, v)| v);
    let unknown_hosts: Vec<String> = hosts
        .iter()
        .filter(|h| h.get("server").and_then(|s| s.get("version")).map(Value::is_null).unwrap_or(true))
        .filter_map(|h| h.get("host").and_then(Value::as_str).map(str::to_string))
        .collect();
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
            "stale": stale(row.fetched_at.as_deref()),
            "error": row.error,
            "source_url": LATEST_URL,
            "releases_url": RELEASES_URL,
        },
        "hosts": hosts,
        "behind_hosts": behind,
        "unknown_hosts": unknown_hosts,
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

/// 手動 refresh 的閘門：同時按兩次只會有一次外部請求，而且 60 秒內不重打。
///
/// 跟 [`crate::update_watch`] 的磁碟快取一樣用 process static，不進 `App`：這支只有自己會用。
fn gate() -> &'static tokio::sync::Mutex<Option<std::time::Instant>> {
    static G: std::sync::OnceLock<tokio::sync::Mutex<Option<std::time::Instant>>> = std::sync::OnceLock::new();
    G.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// 這次該不該真的打官方站。手動 refresh 在 [`MANUAL_MIN_GAP`] 內不重打；排程那輪永遠照跑。
fn throttled(last: Option<std::time::Instant>, manual: bool) -> bool {
    manual && last.map(|t| t.elapsed() < MANUAL_MIN_GAP).unwrap_or(false)
}

/// 查一輪：官方 manifest + 每台主機的兩個版本 + 需要時一筆 AGM 事件。
///
/// `manual = true` 時走 [`MANUAL_MIN_GAP`] 節流；第二個併發呼叫會在鎖上等前一個做完，然後看到剛更新的
/// 時間戳直接回傳快取，不會再打一次官方站。
pub async fn refresh(app: &Arc<App>, manual: bool) {
    let mut last = gate().lock().await;
    if throttled(*last, manual) {
        return;
    }
    *last = Some(std::time::Instant::now());
    match fetch_body(LATEST_URL).await.and_then(|b| parse_manifest(&b).map(|m| (m, b))) {
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
    for conn in app.hosts.list().await {
        let p = probe_host(app, &conn).await;
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
            refresh(&app, false).await;
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
    refresh(&app, true).await;
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

    fn v(s: &str) -> Ver {
        Ver::parse(s).unwrap_or_else(|| panic!("`{s}` 應該解得出來"))
    }

    #[test]
    fn versions_compare_numerically_including_ten_vs_nine() {
        // 字串比較會說 0.9.10 < 0.9.9。
        assert!(v("0.9.10") > v("0.9.9"));
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("0.9.0") > v("0.8.2"));
        // 位數不同補 0。
        assert_eq!(v("0.9"), v("0.9.0"));
        assert_eq!(v("1.0.0"), v("v1.0.0"));
        // 相同、降版。
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
        // 任一邊不知道 = unknown。這是整支功能的重點：不知道不能說「已是最新」。
        assert_eq!(standing(None, Some("0.9.0")), "unknown");
        assert_eq!(standing(Some("0.8.2"), None), "unknown");
        assert_eq!(standing(Some("nightly"), Some("0.9.0")), "unknown");
        assert_eq!(standing(None, None), "unknown");
    }

    #[test]
    fn version_comes_out_of_a_cli_banner() {
        // `herdr --version` 印的是 `herdr 0.8.2`。
        assert_eq!(v("herdr 0.8.2"), v("0.8.2"));
        assert_eq!(v("0.9.0+build5"), v("0.9.0"));
    }

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
        // 頂層有 stable、releases 缺了也還能用。
        assert_eq!(parse_manifest(r#"{"version":"0.9.0","notes":"x"}"#).unwrap().latest, "0.9.0");
    }

    #[test]
    fn notes_cover_the_gap_newest_first_and_never_borrow_another_versions_text() {
        let m = parse_manifest(FIXTURE).unwrap();
        let view = notes_between(&m, Some("0.8.0"), "0.9.0");
        assert!(view.complete, "0.8.0 在官方清單裡");
        assert_eq!(view.sections.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["0.9.0", "0.8.2"]);
        assert!(view.sections[0].notes.as_ref().unwrap().contains("Added"));
        assert!(view.gap.is_none());
        // 同一版 = 沒有差距。
        assert!(notes_between(&m, Some("0.9.0"), "0.9.0").sections.is_empty());
    }

    #[test]
    fn a_missing_middle_version_is_said_out_loud_not_papered_over() {
        let body = r#"{"version":"0.9.0","releases":{
            "0.9.0":{"notes":"new"},
            "0.8.5":{},
            "0.8.0":{"notes":"old"}}}"#;
        let m = parse_manifest(body).unwrap();
        // 目前跑 0.8.3——官方清單裡沒有這一版，所以中間有什麼並不確定。
        let unknown_from = notes_between(&m, Some("0.8.3"), "0.9.0");
        assert!(!unknown_from.complete);
        assert!(unknown_from.gap.as_ref().unwrap().contains("0.8.3"));
        assert_eq!(unknown_from.sections.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["0.9.0", "0.8.5"]);
        // 0.8.5 在清單裡但沒有 notes：明講它缺，不要拿 0.9.0 那段頂替。
        assert_eq!(unknown_from.missing_notes, ["0.8.5"]);
        assert!(unknown_from.sections.iter().find(|s| s.version == "0.8.5").unwrap().notes.is_none());
        // 連目前版本都不知道時，只給 `to` 那一段，並說清楚只有那一段。
        let no_from = notes_between(&m, None, "0.9.0");
        assert_eq!(no_from.sections.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["0.9.0"]);
        assert!(!no_from.complete);
        assert!(no_from.gap.as_ref().unwrap().contains("不知道目前是哪一版"));
    }

    // ---------------- DB 行為 ----------------

    async fn pool() -> SqlitePool {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        migrate(&p).await.unwrap();
        p
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
        assert!(after.checked_at > ok.checked_at || after.checked_at == ok.checked_at);
    }

    #[test]
    fn a_never_fetched_or_long_stale_latest_is_marked_stale() {
        assert!(stale(None), "沒抓過就是 stale");
        assert!(stale(Some("not a date")));
        let old = (chrono::Utc::now() - chrono::Duration::hours(20)).to_rfc3339();
        assert!(stale(Some(&old)));
        assert!(!stale(Some(&chrono::Utc::now().to_rfc3339())));
    }

    #[tokio::test]
    async fn an_offline_host_keeps_its_last_known_versions_and_is_never_given_the_local_ones() {
        let p = pool().await;
        save_host(
            &p,
            &HostProbe {
                host: "box".into(),
                server_version: Some("0.8.2".into()),
                server_protocol: Some(20),
                disk_version: Some("0.8.2".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let first = load_hosts(&p).await;
        assert_eq!(first[0].server_version.as_deref(), Some("0.8.2"));
        let stamp = first[0].server_at.clone();

        // 下一輪主機掛了：兩邊都讀不到。
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
        assert!(row.checked_at >= stamp.unwrap_or_default());
    }

    #[tokio::test]
    async fn hosts_are_tracked_apart() {
        let p = pool().await;
        save_host(&p, &HostProbe { host: "local".into(), server_version: Some("0.9.0".into()), disk_version: Some("0.9.0".into()), ..Default::default() }).await.unwrap();
        save_host(&p, &HostProbe { host: "box".into(), server_version: Some("0.8.2".into()), disk_version: Some("0.8.2".into()), ..Default::default() }).await.unwrap();
        let rows = load_hosts(&p).await;
        let by = |n: &str| rows.iter().find(|r| r.host == n).unwrap().clone();
        assert_eq!(standing(by("local").server_version.as_deref(), Some("0.9.0")), "latest");
        assert_eq!(standing(by("box").server_version.as_deref(), Some("0.9.0")), "behind");
        // 一台落後不會把另一台也說成落後。
        let json = [host_json(&by("local"), true, Some("0.9.0")), host_json(&by("box"), false, Some("0.9.0"))];
        assert_eq!(behind_hosts(&json), ["box"]);
    }

    #[tokio::test]
    async fn a_newer_cli_on_disk_than_the_running_server_is_waiting_for_a_server_swap() {
        let p = pool().await;
        save_host(&p, &HostProbe { host: "local".into(), server_version: Some("0.8.2".into()), disk_version: Some("0.9.0".into()), ..Default::default() }).await.unwrap();
        let row = load_hosts(&p).await.remove(0);
        let j = host_json(&row, true, Some("0.9.0"));
        assert_eq!(j["restart_pending"], true);
        assert_eq!(j["disk"]["standing"], "latest", "磁碟已是最新");
        assert_eq!(j["server"]["standing"], "behind", "但跑著的還是舊的");
        // 只有一邊知道版本時不能猜「待套用」。
        save_host(&p, &HostProbe { host: "half".into(), disk_version: Some("0.9.0".into()), server_error: Some("問不到".into()), ..Default::default() }).await.unwrap();
        let half = load_hosts(&p).await.into_iter().find(|r| r.host == "half").unwrap();
        assert_eq!(host_json(&half, true, Some("0.9.0"))["restart_pending"], false);
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

        let first = notify_if_new(app, "0.9.0", &hosts, true).await;
        assert!(first.is_some(), "第一次要寫進收件匣");
        // 同一台主機被掃了好幾次、或 daemon 重啟後又算了一輪：都是同一個 event_key。
        assert!(notify_if_new(app, "0.9.0", &hosts, true).await.is_none());
        let restarted = crate::testing::restart_app(&env).await;
        assert!(notify_if_new(&restarted, "0.9.0", &hosts, true).await.is_none(), "重啟不能對同一版再叫醒一次");
        let open = crate::supervisor::store::open_inbox(&app.db, 50).await.unwrap();
        let mine: Vec<_> = open.iter().filter(|e| e.kind == EVENT_KIND).collect();
        assert_eq!(mine.len(), 1, "一個 release 最多一筆合併事件");
        assert_eq!(mine[0].event_key, "herdr_update:0.9.0");
        let payload: Value = serde_json::from_str(&mine[0].payload_json).unwrap();
        assert_eq!(payload["behind_hosts"], json!(["local"]));

        // 下一版是新事件。
        assert!(notify_if_new(app, "0.9.1", &hosts, true).await.is_some());
        assert_eq!(
            crate::supervisor::store::open_inbox(&app.db, 50).await.unwrap().iter().filter(|e| e.kind == EVENT_KIND).count(),
            2
        );
        // 已通知的時間是從收件匣讀回來的，不另存一份可能對不上的狀態。
        assert!(notified_at(&app.db, "0.9.0").await.is_some());
        assert!(notified_at(&app.db, "0.9.2").await.is_none());
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
    }

    #[tokio::test]
    async fn the_snapshot_spans_hosts_from_the_oldest_known_version() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let m = parse_manifest(FIXTURE).unwrap();
        save_latest_ok(&app.db, &m, FIXTURE).await.unwrap();
        save_host(&app.db, &HostProbe { host: "local".into(), server_version: Some("0.8.2".into()), disk_version: Some("0.8.2".into()), ..Default::default() }).await.unwrap();
        save_host(&app.db, &HostProbe { host: "box".into(), server_version: Some("0.8.0".into()), disk_version: Some("0.8.0".into()), ..Default::default() }).await.unwrap();
        let snap = snapshot(app).await;
        assert_eq!(snap["latest"]["version"], "0.9.0");
        assert_eq!(snap["notes"]["from"], "0.8.0", "差距從最舊的那台算起");
        let versions: Vec<&str> =
            snap["notes"]["sections"].as_array().unwrap().iter().map(|s| s["version"].as_str().unwrap()).collect();
        assert_eq!(versions, ["0.9.0", "0.8.2"]);
        assert_eq!(snap["behind_hosts"], json!(["box", "local"]));
        assert_eq!(snap["unread"], true);
        mark_seen(&app.db, "0.9.0").await.unwrap();
        assert_eq!(snapshot(app).await["unread"], false, "看過就不再未讀");
    }

    /// 手動 refresh 的節流。併發的第二個呼叫會在 [`gate`] 的鎖上等第一個做完，然後拿到它剛寫下的
    /// 時間戳，於是走這條 `true`：兩次點擊只會有一次外部請求。
    #[test]
    fn a_manual_refresh_is_rate_limited_so_concurrent_clicks_are_one_request() {
        assert!(!throttled(None, true), "沒查過就該查");
        assert!(!throttled(Some(std::time::Instant::now()), false), "排程那輪不受手動節流影響");
        assert!(throttled(Some(std::time::Instant::now()), true), "剛查完的手動 refresh 不重打");
        assert!(
            !throttled(Some(std::time::Instant::now() - MANUAL_MIN_GAP - Duration::from_secs(1)), true),
            "過了間隔就可以再查"
        );
    }
}
