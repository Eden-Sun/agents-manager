//! claude／codex／herdr／grok「上游有新版可裝」（issue #707）：仿 [`crate::herdr_update`] 的「最新版 vs 本機版本＋`last_notified`
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
//! herdr（2026-10-01 一鍵更新）：上游是 GitHub releases 最大的正式版，磁碟版本直接讀工具探測快取的
//! `herdr_cli`（`herdr_version` 每 60 秒重探），不另跑 `--version`。
//!
//! 跟 #204（`release_triage`，分析 changelog 開 issue）不同：那邊回答「新版改了什麼、要不要處理」，這邊只回答
//! 「有沒有比磁碟新的版本可以裝」；兩邊都從同一份 releases 快取（`changelog::fetch_changelog`）發現 codex 新版。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::changelog::{self, cli_version_string, parse_version, version_string};

pub const NPM_CLAUDE_LATEST: &str = "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";
const NPM_CLAUDE_PAGE: &str = "https://www.npmjs.com/package/@anthropic-ai/claude-code";
const CODEX_RELEASES_PAGE: &str = "https://github.com/openai/codex/releases";
const HERDR_RELEASES_PAGE: &str = "https://github.com/herdrdev/herdr/releases";
pub const HERDR_RELEASES_API: &str = "https://api.github.com/repos/herdrdev/herdr/releases?per_page=30";
/// grok（issue #761）：xAI 自己的 installer 把 `~/.grok/bin/grok` 指到 `~/.grok/downloads/grok-<版本>-<平台>`，
/// 沒有 npm／GitHub releases；`grok update --check` 查的 stable 指標就是這個純文字檔（內容只有版本號，例如 `1.0.46`）。
pub const GROK_STABLE_URL: &str = "https://storage.googleapis.com/grok-build-public-artifacts/cli/stable";
/// grok 官方的升級指令（`--check` 只查不裝）；要在**那台主機**上跑，這裡只提示、不代跑。
pub const GROK_UPDATE_COMMAND: &str = "grok update";
pub const KINDS: [&str; 4] = ["claude", "codex", "herdr", "grok"];
/// 巡邏間隔：重比磁碟版本（claude 自己下載完之後通知要消失）。
pub const SWEEP: Duration = Duration::from_secs(600);
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
    match kind {
        "codex" => CODEX_RELEASES_PAGE,
        "herdr" => HERDR_RELEASES_PAGE,
        "grok" => GROK_STABLE_URL,
        _ => NPM_CLAUDE_PAGE,
    }
}

/// npm registry `…/latest` 的回應 → 版本號。`latest` dist-tag 只指向正式版；帶預發布後綴的也當看不懂，不拿來比。
pub fn npm_latest(json: &str) -> Result<String> {
    let v: Value = serde_json::from_str(json).map_err(|e| anyhow!("讀 npm registry 回應失敗：{e}"))?;
    let raw = v.get("version").and_then(Value::as_str).ok_or_else(|| anyhow!("npm registry 回應沒有 version"))?;
    version_string(raw).ok_or_else(|| anyhow!("npm registry 的 version 看不懂：「{raw}」"))
}

/// grok 的 stable 指標（一個版本號的純文字）→ 版本。HTML 錯誤頁、空內容、預發布（`-alpha.2`）與 build metadata（`+x`）
/// 一律當看不懂：抓到什麼就信什麼的話，一個 404 頁面會變成「上游版本 <!doctype」。
pub fn grok_latest(body: &str) -> Result<String> {
    let t = body.trim();
    let one_token = !t.is_empty() && t.split_whitespace().count() == 1;
    version_string(t)
        .filter(|v| one_token && v.contains('.') && v.as_str() == t.trim_start_matches('v'))
        .ok_or_else(|| anyhow!("grok 的 stable 指標看不懂：「{}」", t.chars().take(60).collect::<String>()))
}

/// `changelog::codex_releases_to_md` 整理過的 releases（草稿、預發布已經丟掉）→ 最大的正式版。
pub fn codex_latest(releases_md: &str) -> Result<String> {
    changelog::parse_changelog(releases_md)
        .into_iter()
        .map(|s| s.version)
        .max_by(|a, b| parse_version(a).cmp(&parse_version(b)))
        .ok_or_else(|| anyhow!("GitHub releases 裡沒有正式版"))
}

/// herdr 的 GitHub releases JSON → 最大的正式版（草稿、預發布、帶 `-rc` 之類後綴的 tag 都不算）。
pub fn herdr_latest(releases_json: &str) -> Result<String> {
    codex_latest(&changelog::codex_releases_to_md(releases_json)?)
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
    if status.kind == "herdr" {
        let behind: Vec<String> = status.hosts.iter().filter(|h| h.behind)
            .map(|h| format!("{} 是 {}", h.host, h.installed_version.as_deref().unwrap_or("?")))
            .collect();
        return format!("herdr 上游有新版 {latest}（{}）：更新會重啟 herdr server，所有 bot 中斷約 1 分鐘後自動接回", behind.join("、"));
    }
    if status.kind == "grok" {
        let behind: Vec<String> = status.hosts.iter().filter(|h| h.behind)
            .map(|h| format!("{} 磁碟上是 {}", h.host, h.installed_version.as_deref().unwrap_or("?")))
            .collect();
        return format!("grok 上游有新版 {latest}（{}）：到那台主機執行 `{GROK_UPDATE_COMMAND}`（官方升級指令，這裡不代裝）；跑著的 grok bot 要重啟才會換版", behind.join("、"));
    }
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
    let from = match status.kind.as_str() {
        "claude" => "npm registry",
        "grok" => "x.ai 的 grok 發佈位置",
        _ => "GitHub releases",
    };
    format!("查 {} 上游最新版失敗（{from}）：{}", status.kind, status.error.as_deref().unwrap_or("未知原因"))
}

pub fn load_last(path: &Path) -> HashMap<String, String> {
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
    pub upstream: Mutex<HashMap<String, (Instant, String, String)>>,
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

/// 有 Claude 安裝需要處理時，回傳這份快照允許安裝的共同目標。
pub async fn latest_target_for_host(app: &impl crate::upstream_update::UpstreamWatch, kind: &str, host: &str) -> Option<String> {
    let snapshots = app.upstream_watch().snapshot.lock().await;
    let status = snapshots.get(kind)?;
    (status.has_update && status.hosts.iter().any(|h| h.host == host))
        .then(|| status.target_version.clone())
        .flatten()
}

/// 快照裡這台落後時的目標版本（codex：沒有 run 帶「需安裝」通知時，安裝 API 用它核對確認框寫的那一版）。
pub async fn behind_target_for_host(app: &impl crate::upstream_update::UpstreamWatch, kind: &str, host: &str) -> Option<String> {
    let snapshots = app.upstream_watch().snapshot.lock().await;
    let status = snapshots.get(kind)?;
    (status.has_update && status.hosts.iter().any(|h| h.host == host && h.behind))
        .then(|| status.target_version.clone().or_else(|| status.latest_version.clone()))
        .flatten()
}

/// CLI 安裝成功後立即修正快照，讓 header 不必等下一輪 10 分鐘巡邏才收起警示。
pub async fn note_installed(app: &(impl crate::capabilities::Emit + crate::upstream_update::UpstreamWatch), kind: &str, host: &str, version: &str) {
    let updated = {
        let mut snapshots = app.upstream_watch().snapshot.lock().await;
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

#[cfg(any(test, feature = "test-hooks"))]
pub async fn set_snapshot_for_test(watch: &Watch, status: UpstreamStatus) {
    watch
        .snapshot
        .lock()
        .await
        .insert(status.kind.clone(), status);
}

pub fn last_path(app: &impl crate::capabilities::DataDir) -> PathBuf {
    app.data_dir().join(LAST_FILE)
}

/// 一輪：每個 kind 比一次，快照有變或要通知才推 `upstream_update`。回傳推出去的事件（測試用）。
pub async fn tick(app: &impl crate::capabilities::Emit, watch: &Watch, src: &dyn Sources, last_file: &Path) -> Vec<Value> {
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



/// 上游更新的觀察狀態。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait UpstreamWatch: Send + Sync {
    fn upstream_watch(&self) -> &crate::upstream_update::Watch;
}
