//! claude／codex「上游有新版可裝」（issue #707）：仿 [`crate::herdr_update`] 的「最新版 vs 本機版本＋`last_notified`
//! 去重」，但這裡真的去問上游——claude 問 npm registry 的 `latest`，codex 沿用 [`crate::changelog`] 抓的 GitHub releases。
//!
//! 跟 [`crate::update_watch`] 的「重啟套用」是**兩件事、並存**：
//! - 這裡：上游有、**磁碟上還沒有**（claude 還沒自己下載、codex 還沒裝）——重啟換不到任何東西。
//! - `update_watch`：磁碟上已經是新版、跑著的 run 還是舊的——重啟就換（`runs.update_notice`）。
//!
//! 2026-09-28 使用者回報：npm 已是 2.1.283、磁碟停在 2.1.281，AG Man 什麼都沒說——因為 claude 的提示只看磁碟。
//!
//! 通知：每一種 kind 的每一個新上游版本只推一次 `upstream_update`（`notify: "update"`）；上次推過的版本記在
//! `<data_dir>/upstream-update.last.json`（同 herdr 的 `herdr-update.last`，不為一個字串加表）。上游抓不到**不是**
//! 「沒有新版」：快照帶 `error`，從正常變成抓不到的那一輪推 `notify: "error"`，不靜默。上游結果有 TTL 快取，
//! 巡邏每 10 分鐘只重比磁碟版本，不會每輪打 npm／GitHub。
//!
//! 跟 #204（`release_triage`，分析 changelog 開 issue）不同：那邊回答「新版改了什麼、要不要處理」，這邊只回答
//! 「有沒有比磁碟新的版本可以裝」；兩邊都從同一份 releases 快取（`changelog::fetch_changelog`）發現 codex 新版。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::changelog::{self, cli_version_string, parse_version, version_string};
use crate::state::App;

pub const NPM_CLAUDE_LATEST: &str = "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";
const NPM_CLAUDE_PAGE: &str = "https://www.npmjs.com/package/@anthropic-ai/claude-code";
const CODEX_RELEASES_PAGE: &str = "https://github.com/openai/codex/releases";
pub const KINDS: [&str; 2] = ["claude", "codex"];
/// 巡邏間隔：重比磁碟版本（claude 自己下載完之後通知要消失）。
const SWEEP: Duration = Duration::from_secs(600);
/// 上游結果的有效期；抓失敗的不快取，下一輪（10 分鐘後）再試。
const UPSTREAM_TTL: Duration = Duration::from_secs(3600);
pub const LAST_FILE: &str = "upstream-update.last.json";

pub const CLAUDE_NOTICE_PREFIX: &str = "claude 有新版";

/// Claude 的安裝提示掛在 run 上，讓一般重啟流程知道新版尚未安裝。
pub fn claude_pending_text(from: Option<&str>, to: &str) -> String {
    match from {
        Some(from) => format!("{CLAUDE_NOTICE_PREFIX} {from} → {to}，需安裝後重啟"),
        None => format!("{CLAUDE_NOTICE_PREFIX} {to}，需安裝後重啟"),
    }
}

pub fn claude_pending_from(notice: &str) -> Option<String> {
    if !notice.starts_with(CLAUDE_NOTICE_PREFIX) || !notice.contains("需安裝") {
        return None;
    }
    let versions = notice_versions(notice);
    (versions.len() >= 2).then(|| versions[0].clone())
}

pub fn claude_pending_to(notice: &str) -> Option<String> {
    if !notice.starts_with(CLAUDE_NOTICE_PREFIX) || !notice.contains("需安裝") {
        return None;
    }
    notice_versions(notice).into_iter().last()
}

/// Claude 安裝完成提示裡記錄的磁碟版本。
pub fn claude_installed_to(notice: &str) -> Option<String> {
    if !notice.starts_with(CLAUDE_NOTICE_PREFIX) || !notice.contains("已安裝") {
        return None;
    }
    notice_versions(notice).into_iter().next()
}

fn notice_versions(notice: &str) -> Vec<String> {
    notice
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter_map(version_string)
        .collect()
}

pub fn claude_installed_text(disk: &str, running: &str) -> String {
    format!("claude 有新版 {disk}（這個 run 跑的是 {running}），已安裝，重啟套用")
}

pub fn source_url(kind: &str) -> &'static str {
    if kind == "codex" {
        CODEX_RELEASES_PAGE
    } else {
        NPM_CLAUDE_PAGE
    }
}

/// npm registry `…/latest` 的回應 → 版本號。`latest` dist-tag 只指向正式版；帶預發布後綴的也當看不懂，不拿來比。
pub fn npm_latest(json: &str) -> Result<String> {
    let v: Value = serde_json::from_str(json).map_err(|e| anyhow!("讀 npm registry 回應失敗：{e}"))?;
    let raw = v.get("version").and_then(Value::as_str).ok_or_else(|| anyhow!("npm registry 回應沒有 version"))?;
    version_string(raw).ok_or_else(|| anyhow!("npm registry 的 version 看不懂：「{raw}」"))
}

/// `changelog::codex_releases_to_md` 整理過的 releases（草稿、預發布已經丟掉）→ 最大的正式版。
pub fn codex_latest(releases_md: &str) -> Result<String> {
    changelog::parse_changelog(releases_md)
        .into_iter()
        .map(|s| s.version)
        .max_by(|a, b| parse_version(a).cmp(&parse_version(b)))
        .ok_or_else(|| anyhow!("GitHub releases 裡沒有正式版"))
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct HostDisk {
    pub host: String,
    /// 磁碟上的版本（`<kind> --version`）；讀不到是 `None`，原因在 `error`。
    pub installed_version: Option<String>,
    pub error: Option<String>,
    /// 上游比這台磁碟上的新。
    pub behind: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct UpstreamStatus {
    pub kind: String,
    /// 上游最新正式版；抓不到是 `None`（原因在 `error`），**不是**「沒有新版」。
    pub latest_version: Option<String>,
    /// 這次允許安裝的共同目標。Claude 會取上游版本與各主機已安裝版本的最大值；Codex 等於上游版本。
    pub target_version: Option<String>,
    pub source_url: String,
    /// 上游那份結果是什麼時候抓的。
    pub checked_at: Option<String>,
    pub error: Option<String>,
    pub hosts: Vec<HostDisk>,
    /// 至少一台主機的磁碟比上游舊。
    pub has_update: bool,
    /// 上次真的推過通知的上游版本（去重用）。
    pub notified_version: Option<String>,
}

/// 純函式：上游結果＋每台主機的磁碟版本 → 快照。版本比較一律用數值（`0.9.9 < 0.9.10`）。
pub fn build_status(kind: &str, upstream: &Result<String, String>, disks: &[(String, Result<String, String>)], checked_at: Option<String>) -> UpstreamStatus {
    let latest_version = upstream.as_ref().ok().and_then(|v| version_string(v));
    let mut hosts: Vec<HostDisk> = disks
        .iter()
        .map(|(host, r)| {
            let (installed_version, error) = match r {
                Ok(line) => match cli_version_string(line).or_else(|| version_string(line)) {
                    Some(v) => (Some(v), None),
                    None => (None, Some(format!("`{kind} --version` 回了「{line}」，看不出版本")),
                    ),
                },
                Err(e) => (None, Some(e.clone())),
            };
            HostDisk {
                host: host.clone(),
                installed_version,
                error,
                behind: false,
            }
        })
        .collect();
    let target_version = if kind == "claude" {
        hosts
            .iter()
            .filter_map(|h| h.installed_version.as_deref())
            .chain(latest_version.as_deref())
            .max_by(|a, b| parse_version(a).cmp(&parse_version(b)))
            .map(str::to_string)
    } else {
        latest_version.clone()
    };
    for host in &mut hosts {
        host.behind = match (
            target_version.as_deref().and_then(parse_version),
            host.installed_version.as_deref().and_then(parse_version),
        ) {
            (Some(target), Some(installed)) => installed < target,
            (Some(_), None) => kind == "claude",
            (None, _) => false,
        };
    }
    let claude_versions_differ = kind == "claude"
        && hosts
            .iter()
            .filter_map(|h| h.installed_version.as_deref().and_then(parse_version))
            .any(|v| {
                target_version
                    .as_deref()
                    .and_then(parse_version)
                    .is_some_and(|target| v != target)
            });
    UpstreamStatus {
        kind: kind.to_string(),
        has_update: hosts.iter().any(|h| h.behind) || claude_versions_differ,
        latest_version,
        target_version,
        source_url: source_url(kind).to_string(),
        checked_at,
        error: upstream.as_ref().err().cloned(),
        hosts,
        notified_version: None,
    }
}

/// 同一個上游版本只通知一次。只在上游比上次通知的**更新**時才推：npm 的 `latest` 被退回舊版再推回來，不重報。
pub fn should_notify(status: &UpstreamStatus, last_notified: Option<&str>) -> bool {
    let Some(latest) = status.latest_version.as_deref().and_then(parse_version) else { return false };
    status.has_update && last_notified.and_then(parse_version).is_none_or(|last| latest > last)
}

/// 給使用者看的那一句：要跟「重啟套用」分得清——這是上游有、磁碟上還沒有。
pub fn notice_text(status: &UpstreamStatus) -> String {
    let latest = status.latest_version.as_deref().unwrap_or("?");
    if status.kind != "claude" {
        let behind: Vec<String> = status.hosts.iter().filter(|h| h.behind)
            .map(|h| format!("{} 磁碟上是 {}", h.host, h.installed_version.as_deref().unwrap_or("?")))
            .collect();
        return format!("{} 上游有新版 {latest}（{}）：需先安裝，裝好才會出現「重啟套用」", status.kind, behind.join("、"));
    }

    let target = status.target_version.as_deref().unwrap_or(latest);
    let hosts: Vec<String> = status.hosts.iter().map(|h| {
        let installed = h.installed_version.as_deref().unwrap_or_else(|| h.error.as_deref().unwrap_or("讀取失敗"));
        format!("{}：{} → {target}", h.host, installed)
    }).collect();
    format!("claude 需安裝 {target}（{}）：需安裝到共同版本，裝到每台都相同後才會出現「重啟套用」。指令：`claude install {target}`", hosts.join("；"))
}

/// 快照＋畫面要顯示的那一句：有新版是 [`notice_text`]、抓不到是 [`error_text`]、其他 `null`。
pub fn item_json(status: &UpstreamStatus) -> Value {
    let text = if status.has_update {
        Some(notice_text(status))
    } else {
        status.error.as_ref().map(|_| error_text(status))
    };
    let mut v = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
    v["text"] = json!(text);
    v
}

pub fn error_text(status: &UpstreamStatus) -> String {
    let from = if status.kind == "codex" { "GitHub releases" } else { "npm registry" };
    format!("查 {} 上游最新版失敗（{from}）：{}", status.kind, status.error.as_deref().unwrap_or("未知原因"))
}

fn load_last(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_last(path: &Path, last: &HashMap<String, String>) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(last)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 上游與磁碟兩個來源；正式跑是 [`Live`]，測試換成假的，不上網、不跑 `--version`。
pub trait Sources: Send + Sync {
    fn upstream<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Result<String>>;
    /// 這個 kind 裝在哪些主機上（沒裝的主機不列，否則每台都掛一條「讀不到版本」）。
    fn hosts<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Vec<String>>;
    fn installed<'a>(&'a self, host: &'a str, kind: &'a str) -> BoxFuture<'a, Result<String>>;
}

#[derive(Default)]
pub struct Watch {
    upstream: Mutex<HashMap<String, (Instant, String, String)>>,
    snapshot: Mutex<HashMap<String, UpstreamStatus>>,
}

impl Watch {
    pub async fn snapshot(&self) -> Vec<UpstreamStatus> {
        let g = self.snapshot.lock().await;
        KINDS.iter().filter_map(|k| g.get(*k).cloned()).collect()
    }

    /// TTL 內的成功結果直接用；過期或上次失敗才問上游。回 `(結果, 抓的時間)`。
    async fn upstream(&self, src: &dyn Sources, kind: &str) -> (Result<String, String>, String) {
        if let Some((at, v, ts)) = self.upstream.lock().await.get(kind) {
            if at.elapsed() < UPSTREAM_TTL {
                return (Ok(v.clone()), ts.clone());
            }
        }
        let ts = crate::db::now();
        match src.upstream(kind).await {
            Ok(v) => {
                self.upstream.lock().await.insert(kind.to_string(), (Instant::now(), v.clone(), ts.clone()));
                (Ok(v), ts)
            }
            Err(e) => (Err(format!("{e:#}")), ts),
        }
    }
}

pub fn watch() -> &'static Watch {
    static W: OnceLock<Watch> = OnceLock::new();
    W.get_or_init(Watch::default)
}

/// 有 Claude 安裝需要處理時，回傳這份快照允許安裝的共同目標。
pub async fn latest_target_for_host(kind: &str, host: &str) -> Option<String> {
    let snapshots = watch().snapshot.lock().await;
    let status = snapshots.get(kind)?;
    (status.has_update && status.hosts.iter().any(|h| h.host == host))
        .then(|| status.target_version.clone())
        .flatten()
}

/// 快照裡這台落後時的目標版本（codex：沒有 run 帶「需安裝」通知時，安裝 API 用它核對確認框寫的那一版）。
pub async fn behind_target_for_host(kind: &str, host: &str) -> Option<String> {
    let snapshots = watch().snapshot.lock().await;
    let status = snapshots.get(kind)?;
    (status.has_update && status.hosts.iter().any(|h| h.host == host && h.behind))
        .then(|| status.target_version.clone().or_else(|| status.latest_version.clone()))
        .flatten()
}

/// CLI 安裝成功後立即修正快照，讓 header 不必等下一輪 10 分鐘巡邏才收起警示。
pub async fn note_installed(app: &App, kind: &str, host: &str, version: &str) {
    let updated = {
        let mut snapshots = watch().snapshot.lock().await;
        let Some(status) = snapshots.get_mut(kind) else {
            return;
        };
        let Some(disk) = status.hosts.iter_mut().find(|h| h.host == host) else {
            return;
        };
        disk.installed_version = Some(version.to_string());
        disk.error = None;
        status.target_version = if kind == "claude" {
            status
                .hosts
                .iter()
                .filter_map(|h| h.installed_version.as_deref())
                .chain(status.latest_version.as_deref())
                .max_by(|a, b| parse_version(a).cmp(&parse_version(b)))
                .map(str::to_string)
        } else {
            status.latest_version.clone()
        };
        for h in &mut status.hosts {
            h.behind = match (
                status.target_version.as_deref().and_then(parse_version),
                h.installed_version.as_deref().and_then(parse_version),
            ) {
                (Some(target), Some(installed)) => installed < target,
                (Some(_), None) => kind == "claude",
                (None, _) => false,
            };
        }
        let versions_differ = kind == "claude"
            && status
                .hosts
                .iter()
                .filter_map(|h| h.installed_version.as_deref().and_then(parse_version))
                .any(|v| {
                    status
                        .target_version
                        .as_deref()
                        .and_then(parse_version)
                        .is_some_and(|target| v != target)
                });
        status.has_update = status.hosts.iter().any(|h| h.behind) || versions_differ;
        status.checked_at = Some(crate::db::now());
        item_json(status)
    };
    let mut event = updated;
    event["notify"] = json!(null);
    app.emit("upstream_update", event).await;
}

#[cfg(test)]
pub(crate) async fn set_snapshot_for_test(status: UpstreamStatus) {
    watch()
        .snapshot
        .lock()
        .await
        .insert(status.kind.clone(), status);
}

pub fn last_path(app: &App) -> PathBuf {
    app.data_dir.join(LAST_FILE)
}

/// 一輪：每個 kind 比一次，快照有變或要通知才推 `upstream_update`。回傳推出去的事件（測試用）。
pub async fn tick(app: &Arc<App>, watch: &Watch, src: &dyn Sources, last_file: &Path) -> Vec<Value> {
    let mut emitted = Vec::new();
    for kind in KINDS {
        let (upstream, checked_at) = watch.upstream(src, kind).await;
        let mut disks = Vec::new();
        for host in src.hosts(kind).await {
            let r = src.installed(&host, kind).await.map_err(|e| format!("{e:#}"));
            disks.push((host, r));
        }
        let mut status = build_status(kind, &upstream, &disks, Some(checked_at));
        let mut last = load_last(last_file);
        let prev = watch.snapshot.lock().await.get(kind).cloned();
        let notify = if should_notify(&status, last.get(kind).map(String::as_str)) {
            let v = status.latest_version.clone().unwrap_or_default();
            last.insert(kind.to_string(), v);
            if let Err(e) = save_last(last_file, &last) {
                // 記不下來下一輪會再推一次；寧可重複也不要漏。
                tracing::warn!(kind, error = %e, "upstream_update: 寫不進 last_notified");
            }
            Some("update")
        } else if status.error.is_some() && prev.as_ref().is_none_or(|p| p.error.is_none()) {
            Some("error")
        } else {
            None
        };
        status.notified_version = last.get(kind).cloned();
        if let Some(e) = &status.error {
            tracing::warn!(kind, error = %e, "upstream_update: 查不到上游最新版");
        }
        // `checked_at` 不算變化：抓不到時每輪都是新的嘗試時間，算進去就會每 10 分鐘重推一次同樣的錯誤。
        let changed = prev.as_ref().map(|p| UpstreamStatus { checked_at: status.checked_at.clone(), ..p.clone() }).as_ref() != Some(&status);
        watch.snapshot.lock().await.insert(kind.to_string(), status.clone());
        if !changed && notify.is_none() {
            continue;
        }
        let mut ev = item_json(&status);
        ev["notify"] = json!(notify);
        if notify == Some("update") {
            tracing::info!(kind, latest = ?status.latest_version, "上游有新版可裝");
        }
        app.emit("upstream_update", ev.clone()).await;
        emitted.push(ev);
    }
    emitted
}

pub struct Live(pub Arc<App>);

impl Sources for Live {
    fn upstream<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            if kind == "codex" {
                return codex_latest(&changelog::fetch_changelog(&self.0, "codex").await?);
            }
            let client = reqwest::Client::builder()
                .user_agent("agents-manager")
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|e| anyhow!("http client: {e}"))?;
            let resp = client.get(NPM_CLAUDE_LATEST).send().await.map_err(|e| anyhow!("連不上 npm registry：{e}"))?;
            if !resp.status().is_success() {
                return Err(anyhow!("npm registry 回 HTTP {}", resp.status()));
            }
            npm_latest(&resp.text().await.map_err(|e| anyhow!("讀 npm registry 回應失敗：{e}"))?)
        })
    }

    fn hosts<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move {
            // 先拿主機清單再鎖 tools：不要拿著 tools 鎖去等別的鎖。
            let conns = self.0.hosts.list().await;
            let tools = self.0.tools.lock().await;
            let mut out = Vec::new();
            for c in conns {
                if tools.get(&c.name).and_then(|t| t.tools.get(kind)).is_some_and(|t| t.installed) {
                    out.push(c.name.clone());
                }
            }
            out
        })
    }

    fn installed<'a>(&'a self, host: &'a str, kind: &'a str) -> BoxFuture<'a, Result<String>> {
        Box::pin(changelog::installed_version(&self.0, host, kind))
    }
}

pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        // 開機先等工具探測跑完，不然第一輪一台主機都沒有。
        tokio::time::sleep(Duration::from_secs(60)).await;
        let src = Live(app.clone());
        let path = last_path(&app);
        loop {
            tick(&app, watch(), &src, &path).await;
            tokio::time::sleep(SWEEP).await;
        }
    });
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/upstream-updates", get(get_status))
}

/// `GET /api/upstream-updates`：最近一輪的快照，不觸發抓取。第一輪還沒跑完是空陣列。
async fn get_status(State(_app): State<Arc<App>>) -> Json<Value> {
    let items: Vec<Value> = watch().snapshot().await.iter().map(item_json).collect();
    Json(json!({ "items": items }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn ok(v: &str) -> Result<String, String> {
        Ok(v.to_string())
    }

    fn disk(host: &str, line: &str) -> (String, Result<String, String>) {
        (host.to_string(), Ok(line.to_string()))
    }

    #[test]
    fn claude_pending_from_requires_a_source_and_target_pair() {
        let without_source = claude_pending_text(None, "2.1.284");
        assert_eq!(claude_pending_from(&without_source), None);

        let with_source = claude_pending_text(Some("2.1.281"), "2.1.284");
        assert_eq!(claude_pending_from(&with_source).as_deref(), Some("2.1.281"));
    }

    // ── 純函式 ──

    /// 使用者 2026-09-28 那一刻：npm 2.1.283、磁碟 2.1.281。
    #[test]
    fn npm_newer_than_disk_is_an_update() {
        let s = build_status("claude", &ok("2.1.283"), &[disk("local", "2.1.281 (Claude Code)")], None);
        assert!(s.has_update && s.hosts[0].behind);
        assert_eq!(s.latest_version.as_deref(), Some("2.1.283"));
        assert_eq!(s.hosts[0].installed_version.as_deref(), Some("2.1.281"));
        assert!(should_notify(&s, None));
        let t = notice_text(&s);
        assert!(t.contains("2.1.283") && t.contains("local：2.1.281 → 2.1.283") && t.contains("裝到每台都相同"),
            "{t}"
        );
        assert!(
            t.contains("claude install 2.1.283"),
            "提示要給出指定版本的安裝方式：{t}"
        );
        assert!(
            !t.contains("背景更新") && !t.contains("claude update"),
            "停用自動更新時不可叫人繼續等：{t}"
        );
    }

    #[test]
    fn same_or_older_upstream_is_not_an_update() {
        let same = build_status("claude", &ok("2.1.283"), &[disk("local", "2.1.283 (Claude Code)")], None);
        assert!(!same.has_update && !should_notify(&same, None));
        // 磁碟比 npm 新（npm 還沒同步、或裝了別的通道）：不叫人去裝。
        let older = build_status("claude", &ok("2.1.281"), &[disk("local", "2.1.283 (Claude Code)")], None);
        assert!(!older.has_update && !should_notify(&older, None));
    }

    #[test]
    fn claude_host_version_mismatch_uses_one_fleet_target_and_lists_every_host() {
        let disks = [
            disk("local", "2.1.284"),
            disk("m4p", "2.1.281"),
            ("offline".into(), Err("timeout".into())),
        ];
        let s = build_status("claude", &ok("2.1.283"), &disks, None);
        assert!(s.has_update, "版本不一致也要持續提示");
        assert_eq!(s.latest_version.as_deref(), Some("2.1.283"));
        assert_eq!(
            s.target_version.as_deref(),
            Some("2.1.284"),
            "最高已安裝版成為共同目標"
        );
        assert_eq!(
            s.hosts.iter().map(|h| h.behind).collect::<Vec<_>>(),
            [false, true, true]
        );
        let text = notice_text(&s);
        assert!(text.contains("local：2.1.284 → 2.1.284"), "{text}");
        assert!(text.contains("m4p：2.1.281 → 2.1.284"), "{text}");
        assert!(text.contains("offline：timeout → 2.1.284"), "{text}");
        assert!(text.contains("claude install 2.1.284"), "{text}");
    }

    #[test]
    fn versions_compare_numerically_not_as_strings() {
        assert!(build_status("codex", &ok("0.9.10"), &[disk("local", "codex-cli 0.9.9")], None).has_update);
        assert!(!build_status("codex", &ok("0.9.9"), &[disk("local", "codex-cli 0.9.10")], None).has_update);
    }

    /// 只要有一台落後就算；通知列出落後的那幾台。
    #[test]
    fn any_host_behind_is_an_update_and_the_notice_names_it() {
        let s = build_status("codex", &ok("0.157.0"), &[disk("local", "codex-cli 0.157.0"), disk("m2", "codex-cli 0.156.1")], None);
        assert!(s.has_update && !s.hosts[0].behind && s.hosts[1].behind);
        let t = notice_text(&s);
        assert!(t.contains("m2 磁碟上是 0.156.1") && !t.contains("local") && t.contains("需先安裝"), "{t}");
    }

    #[test]
    fn dedup_notifies_each_upstream_version_once() {
        let s = build_status("claude", &ok("2.1.283"), &[disk("local", "2.1.281")], None);
        assert!(should_notify(&s, None), "還沒通知過");
        assert!(should_notify(&s, Some("2.1.282")), "上次通知的是更舊的版本");
        assert!(!should_notify(&s, Some("2.1.283")), "這一版已經通知過");
        assert!(!should_notify(&s, Some("2.1.284")), "npm latest 被退回舊版，不重報");
    }

    /// 抓不到上游不是「沒有新版」：`latest_version` 空、`error` 講原因，不會被當成有更新或沒更新而默默帶過。
    #[test]
    fn an_unreachable_upstream_is_an_explicit_error() {
        let s = build_status("claude", &Err("連不上 npm registry：timeout".into()), &[disk("local", "2.1.281")], None);
        assert!(s.latest_version.is_none() && !s.has_update && !should_notify(&s, None));
        assert_eq!(s.error.as_deref(), Some("連不上 npm registry：timeout"));
        let t = error_text(&s);
        assert!(t.contains("claude") && t.contains("npm registry") && t.contains("timeout"), "{t}");
    }

    #[test]
    fn npm_latest_reads_the_version_field() {
        assert_eq!(npm_latest(r#"{"name":"@anthropic-ai/claude-code","version":"2.1.283","dist":{}}"#).unwrap(), "2.1.283");
        assert!(npm_latest(r#"{"name":"x"}"#).is_err());
        assert!(npm_latest("<html>rate limited</html>").is_err());
        assert!(npm_latest(r#"{"version":"2.2.0-beta.1"}"#).is_err(), "預發布不拿來比");
    }

    /// codex 走 releases：沿用 `codex_releases_to_md`（丟草稿／預發布），取最大的正式版，不是第一個。
    #[test]
    fn codex_latest_is_the_highest_stable_release() {
        let json = r#"[
          {"tag_name":"rust-v0.157.0-alpha.2","prerelease":true,"draft":false,"body":"nope"},
          {"tag_name":"rust-v0.156.1","prerelease":false,"draft":false,"body":"- older"},
          {"tag_name":"rust-v0.157.0","prerelease":false,"draft":false,"body":"- newest"},
          {"tag_name":"rust-v0.99.0","prerelease":false,"draft":false,"body":"- ancient"}
        ]"#;
        let md = changelog::codex_releases_to_md(json).unwrap();
        assert_eq!(codex_latest(&md).unwrap(), "0.157.0");
        assert!(codex_latest("").is_err());
    }

    // ── 整輪：假的上游與磁碟 ──

    struct Fake {
        upstream: StdMutex<HashMap<String, Result<String, String>>>,
        disk: StdMutex<HashMap<String, String>>,
        calls: StdMutex<usize>,
    }

    impl Fake {
        fn new(claude: Result<&str, &str>, codex: Result<&str, &str>, claude_disk: &str, codex_disk: &str) -> Self {
            let up = [("claude", claude), ("codex", codex)]
                .into_iter()
                .map(|(k, r)| (k.to_string(), r.map(str::to_string).map_err(str::to_string)))
                .collect();
            let d = [("claude".to_string(), claude_disk.to_string()), ("codex".to_string(), codex_disk.to_string())].into_iter().collect();
            Fake { upstream: StdMutex::new(up), disk: StdMutex::new(d), calls: StdMutex::new(0) }
        }
        fn set_disk(&self, kind: &str, v: &str) {
            self.disk.lock().unwrap().insert(kind.into(), v.into());
        }
        fn set_upstream(&self, kind: &str, r: Result<&str, &str>) {
            self.upstream.lock().unwrap().insert(kind.into(), r.map(str::to_string).map_err(str::to_string));
        }
    }

    impl Sources for Fake {
        fn upstream<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Result<String>> {
            *self.calls.lock().unwrap() += 1;
            let r = self.upstream.lock().unwrap().get(kind).cloned().unwrap();
            Box::pin(async move { r.map_err(|e| anyhow!(e)) })
        }
        fn hosts<'a>(&'a self, _kind: &'a str) -> BoxFuture<'a, Vec<String>> {
            Box::pin(async { vec!["local".to_string()] })
        }
        fn installed<'a>(&'a self, _host: &'a str, kind: &'a str) -> BoxFuture<'a, Result<String>> {
            let v = self.disk.lock().unwrap().get(kind).cloned().unwrap();
            Box::pin(async move { Ok(v) })
        }
    }

    fn of<'a>(evs: &'a [Value], kind: &str) -> Option<&'a Value> {
        evs.iter().find(|e| e["kind"] == kind)
    }

    #[tokio::test]
    async fn claude_via_npm_notifies_once_then_clears_when_the_disk_catches_up() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Ok("2.1.283"), Ok("0.157.0"), "2.1.281 (Claude Code)", "codex-cli 0.157.0");

        let evs = tick(&e.app, &w, &src, &last).await;
        let c = of(&evs, "claude").expect("第一輪要推");
        assert_eq!(c["notify"], "update");
        assert!(c["text"].as_str().unwrap().contains("2.1.283"));
        assert_eq!(c["notified_version"], "2.1.283");
        // codex 同版：推快照但不通知、沒有要顯示的字。
        assert!(of(&evs, "codex").unwrap()["notify"].is_null());
        assert!(of(&evs, "codex").unwrap()["text"].is_null());

        // 同一版第二輪：什麼都不推（快照沒變、已通知過）。
        assert!(tick(&e.app, &w, &src, &last).await.is_empty(), "同一版不重複通知");
        // daemon 重啟（新的 Watch）也不重報：去重記在檔案。
        let w2 = Watch::default();
        let evs = tick(&e.app, &w2, &src, &last).await;
        assert!(evs.iter().all(|e| e["notify"].is_null()), "重啟後同一版不重報：{evs:?}");

        // claude 自己下載好了：快照改成沒有更新，推一次（web 收掉），不通知。
        src.set_disk("claude", "2.1.283 (Claude Code)");
        let evs = tick(&e.app, &w2, &src, &last).await;
        let c = of(&evs, "claude").unwrap();
        assert_eq!(c["has_update"], false);
        assert!(c["notify"].is_null());
        assert_eq!(w2.snapshot().await.len(), 2);
    }

    #[tokio::test]
    async fn codex_via_releases_notifies_a_newer_release_and_each_later_one() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Ok("2.1.281"), Ok("0.157.0"), "2.1.281", "codex-cli 0.156.1");
        let evs = tick(&e.app, &w, &src, &last).await;
        let c = of(&evs, "codex").unwrap();
        assert_eq!(c["notify"], "update");
        assert!(c["text"].as_str().unwrap().contains("需先安裝"));
        // 上游又出了下一版：TTL 內用快取，不會馬上看到——清掉快取模擬過期。
        src.set_upstream("codex", Ok("0.158.0"));
        w.upstream.lock().await.clear();
        let evs = tick(&e.app, &w, &src, &last).await;
        assert_eq!(of(&evs, "codex").unwrap()["notify"], "update", "新的一版要再通知");
        assert_eq!(of(&evs, "codex").unwrap()["latest_version"], "0.158.0");
    }

    /// TTL：一輪內或下一輪都不重打上游；失敗不快取，下一輪再試。
    #[tokio::test]
    async fn upstream_results_are_cached_but_failures_are_retried() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Ok("2.1.283"), Err("GitHub HTTP 403 rate limit"), "2.1.283", "codex-cli 0.157.0");
        tick(&e.app, &w, &src, &last).await;
        assert_eq!(*src.calls.lock().unwrap(), 2);
        tick(&e.app, &w, &src, &last).await;
        assert_eq!(*src.calls.lock().unwrap(), 3, "claude 用快取，只有失敗的 codex 重問");
    }

    /// 抓不到要講：從正常變成抓不到推一次 `error`，一直抓不到不刷屏，恢復後沒事。
    #[tokio::test]
    async fn an_unreachable_upstream_is_announced_once_not_silently_skipped() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Err("連不上 npm registry：dns error"), Ok("0.157.0"), "2.1.281", "codex-cli 0.157.0");
        let evs = tick(&e.app, &w, &src, &last).await;
        let c = of(&evs, "claude").unwrap();
        assert_eq!(c["notify"], "error");
        assert!(c["text"].as_str().unwrap().contains("dns error"));
        assert!(c["latest_version"].is_null() && c["has_update"] == false);
        // 下一輪的嘗試時間一定不同（毫秒精度）：時間戳變了不算快照變了。
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(of(&tick(&e.app, &w, &src, &last).await, "claude").is_none(), "持續抓不到不重複推");
        src.set_upstream("claude", Ok("2.1.283"));
        let c = of(&tick(&e.app, &w, &src, &last).await, "claude").cloned().unwrap();
        assert_eq!(c["notify"], "update", "恢復之後照常通知新版");
        assert!(c["error"].is_null());
    }

    /// codex 沒有 run 帶「需安裝」時，安裝 API 靠這個核對：那台落後才給目標，沒落後、沒新版都是 None。
    /// 用不存在的主機名，不去動別的測試會讀的 `local`（快照是 process 全域）。
    #[tokio::test]
    async fn behind_target_is_only_given_for_a_host_that_is_behind() {
        set_snapshot_for_test(build_status(
            "codex-behind-test",
            &Ok("0.159.0".into()),
            &[("bt-old".into(), Ok("codex-cli 0.157.1".into())), ("bt-new".into(), Ok("codex-cli 0.159.0".into()))],
            None,
        ))
        .await;
        assert_eq!(behind_target_for_host("codex-behind-test", "bt-old").await.as_deref(), Some("0.159.0"));
        assert_eq!(behind_target_for_host("codex-behind-test", "bt-new").await, None, "已經是新版");
        assert_eq!(behind_target_for_host("codex-behind-test", "bt-missing").await, None);
        assert_eq!(behind_target_for_host("no-such-kind", "bt-old").await, None);
    }
}
