//! herdr integration（claude／codex／antigravity-cli／grok 的 hook 腳本）的更新總覽（SPEC §6.9c、API `GET /api/integration-updates`，issue #1772）。
//!
//! herdr 升版後，各 agent 的 hook 腳本版本會跟不上（`herdr integration status` 顯示 `outdated (v8 < v11)`），
//! 以前沒有任何畫面提醒。這裡每一輪（跟 `upstream_update` 同節奏）在每台主機跑**唯讀**的 `herdr integration status`，
//! 只留我們用得到的四家，快照給設定頁「更新檢查」。
//!
//! 刻意的邊界：
//! - 巡邏只讀。`herdr integration install <name>` 手動只在使用者按設定頁的按鈕時跑（帶 bot 身分的請求一律 403），
//!   而且只准 [`TRACKED`] 裡的名字、`not installed` 的手動不裝（只顯示）。它會改那台的 `~/.claude`／`~/.codex` 設定。
//! - 唯一的自動安裝（#1871）：該主機 grok／agy CLI 已登入、那一家 integration 是 `not_installed` 時，[`auto_install`] 在巡檢那一輪裝一次
//!   （每主機每家每個 daemon 行程最多一次，失敗不重試）。claude／codex 的 outdated 照舊只顯示。
//! - 同一台同時只跑一個安裝（409）；有逾時。
//! - 會動到機器的兩件事（讀狀態、安裝）都走 [`Ops`]，測試換成假的：測試裡絕對不能真的跑 `herdr integration install`。

use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path as AxPath, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 設定頁要看的四家（`herdr integration status` 的名字；grok／antigravity-cli 沒裝時也列出來，只顯示）。
pub const TRACKED: [&str; 4] = ["claude", "codex", "antigravity-cli", "grok"];

/// 自動安裝的兩家：(integration 名, 工具探測的 kind)。「已登入」看 `app.tools[host].tools[kind]` 的 `installed && logged_in == Some(true)`。
pub const AUTO_INSTALL: [(&str, &str); 2] = [("grok", "grok"), ("antigravity-cli", "agy")];

/// 跟 `cli_updates`／`upstream_update` 同一個節奏。
pub const SWEEP: Duration = Duration::from_secs(600);
const STATUS_TIMEOUT: Duration = Duration::from_secs(40);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(60);
/// 失敗時回給畫面的輸出只留尾巴。
const TAIL_CHARS: usize = 800;

/// `current`｜`outdated`｜`not_installed`｜`unknown`（herdr 印了我們不認得的狀態；`detail` 留原文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub state: &'static str,
    /// hook 腳本現在的版本號（`v8` 的 8）；沒裝或沒印＝None。
    pub installed_version: Option<u32>,
    /// herdr 要求的版本（`outdated (v8 < v11)` 的 11；`current (v11)` 就是 11）。
    pub required_version: Option<u32>,
    pub path: Option<String>,
    /// `unknown` 時 herdr 印的原文，其他狀態是 None。
    pub detail: Option<String>,
}

impl Row {
    fn json(&self) -> Value {
        json!({"name": self.name, "state": self.state, "installed_version": self.installed_version,
               "required_version": self.required_version, "path": self.path, "detail": self.detail})
    }
}

/// 括號裡的 `v8` ／ `v8 < v11`：回 (installed, required)。`current (v11)` 兩個都是 11。
fn parse_versions(rest: &str) -> (Option<u32>, Option<u32>) {
    let Some(open) = rest.find('(') else { return (None, None) };
    let Some(close) = rest[open..].find(')') else { return (None, None) };
    let inner = &rest[open + 1..open + close];
    let num = |s: &str| s.trim().strip_prefix('v').and_then(|d| d.parse::<u32>().ok());
    match inner.split_once('<') {
        Some((a, b)) => (num(a), num(b)),
        None => match num(inner) {
            Some(v) => (Some(v), Some(v)),
            None => (None, None),
        },
    }
}

/// 解析 `herdr integration status` 的輸出，只留 [`TRACKED`]，順序固定。沒印到的名字不列。
///
/// 每行：`<name>[ (experimental)]: <狀態>[ (v8 < v11)] (<路徑>)`；狀態是 `current`／`outdated`／`not installed`。
pub fn parse_status(out: &str) -> Vec<Row> {
    let mut found: BTreeMap<String, Row> = BTreeMap::new();
    for line in out.lines() {
        let Some((head, rest)) = line.trim().split_once(':') else { continue };
        let name = head.split_whitespace().next().unwrap_or("");
        if !TRACKED.contains(&name) || head.contains("(experimental)") {
            continue;
        }
        let rest = rest.trim();
        let path = rest.rsplit_once('(').and_then(|(_, p)| p.strip_suffix(')')).filter(|p| p.contains('/')).map(str::to_string);
        // 版本在狀態字後面的第一組括號；路徑那組不含 `v<數字>`，所以先砍掉路徑再讀。
        let without_path = match &path {
            Some(p) => rest.strip_suffix(&format!("({p})")).unwrap_or(rest).trim_end(),
            None => rest,
        };
        let (state, installed, required, detail) = if without_path.starts_with("not installed") {
            ("not_installed", None, None, None)
        } else if without_path.starts_with("outdated") {
            let (a, b) = parse_versions(without_path);
            ("outdated", a, b, None)
        } else if without_path.starts_with("current") || without_path.starts_with("up to date") || without_path.starts_with("already up to date") {
            let (a, b) = parse_versions(without_path);
            ("current", a, b, None)
        } else {
            ("unknown", None, None, Some(rest.to_string()))
        };
        found.insert(
            name.to_string(),
            Row { name: name.to_string(), state, installed_version: installed, required_version: required, path, detail },
        );
    }
    TRACKED.iter().filter_map(|n| found.remove(*n)).collect()
}

/// 會動到機器的兩件事。正式版是 [`Real`]；測試換成假的。`Err` 是給人看的原因。
pub trait Ops: Send + Sync {
    /// 那台 `herdr integration status` 的原文。
    fn status<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Result<String, String>>;
    /// 那台跑 `herdr integration install <name>`（`name` 已驗過在 [`TRACKED`]）。
    fn install<'a>(&'a self, host: &'a str, name: &'a str) -> BoxFuture<'a, Result<String, String>>;
}

#[derive(Debug, Clone, Default)]
struct HostSnap {
    checked_at: String,
    /// 這一輪讀失敗的原因；`items` 還是上一次成功的（不把「讀不到」顯示成「沒有」）。
    error: Option<String>,
    items: Vec<Row>,
}

/// 每台主機最近一次的結果＋「正在安裝」的名單。只在記憶體：重啟後第一輪就補回來。
#[derive(Default)]
pub struct Watch {
    hosts: Mutex<BTreeMap<String, HostSnap>>,
    installing: Mutex<HashSet<String>>,
    /// 自動裝試過的 (主機, integration 名) 與結果：整個 daemon 行程只試一次，失敗也不重試（#1871）。
    auto: Mutex<BTreeMap<(String, String), AutoOutcome>>,
}

/// 一次自動安裝的結果（給設定頁與通知）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoOutcome {
    pub host: String,
    pub name: String,
    pub ok: bool,
    /// 裝完重讀到的狀態；讀不到＝None。
    pub state: Option<&'static str>,
    /// 失敗原因（給人看的一句）；成功是 None。
    pub error: Option<String>,
    pub at: String,
}

impl AutoOutcome {
    fn json(&self) -> Value {
        json!({"ok": self.ok, "state": self.state, "error": self.error, "at": self.at})
    }
}

impl Watch {
    fn record(&self, host: &str, result: Result<Vec<Row>, String>) {
        let mut g = self.hosts.lock().unwrap();
        let snap = g.entry(host.to_string()).or_default();
        snap.checked_at = crate::db::now();
        match result {
            Ok(items) => {
                snap.items = items;
                snap.error = None;
            }
            Err(e) => snap.error = Some(e),
        }
    }

    /// 已不存在的主機整筆丟掉。
    fn retain(&self, names: &[String]) {
        self.hosts.lock().unwrap().retain(|h, _| names.contains(h));
    }

    pub fn snapshot(&self) -> Vec<Value> {
        self.hosts
            .lock()
            .unwrap()
            .iter()
            .map(|(h, s)| {
                json!({"host": h, "checked_at": s.checked_at, "error": s.error,
                       "items": s.items.iter().map(|r| self.item_json(h, r)).collect::<Vec<_>>()})
            })
            .collect()
    }

    /// 一列的 JSON；自動裝過的多一個 `auto_install`（沒試過就沒有這個欄位）。
    fn item_json(&self, host: &str, row: &Row) -> Value {
        let mut v = row.json();
        if let Some(o) = self.auto.lock().unwrap().get(&(host.to_string(), row.name.clone())) {
            v["auto_install"] = o.json();
        }
        v
    }

    /// 這一輪成功讀到的列；讀失敗（`error` 有值，`items` 是上次的）回空：不拿舊資料決定要不要裝。
    fn fresh_items(&self, host: &str) -> Vec<Row> {
        self.hosts.lock().unwrap().get(host).filter(|s| s.error.is_none()).map(|s| s.items.clone()).unwrap_or_default()
    }

    /// 這台最近一次讀狀態失敗的原因；最近一次成功＝None。
    fn read_error(&self, host: &str) -> Option<String> {
        self.hosts.lock().unwrap().get(host).and_then(|s| s.error.clone())
    }

    fn items(&self, host: &str) -> Vec<Row> {
        self.hosts.lock().unwrap().get(host).map(|s| s.items.clone()).unwrap_or_default()
    }

    /// 讀一台，寫進快照。
    pub async fn refresh_host(&self, ops: &dyn Ops, host: &str) {
        let result = ops.status(host).await.map(|out| parse_status(&out));
        self.record(host, result);
    }
}

/// 一輪：`hosts` 是 (主機名, 現在連得上)；連不上的不嘗試、記「未連線」。
pub async fn sweep(watch: &Watch, ops: &dyn Ops, hosts: &[(String, bool)]) {
    let names: Vec<String> = hosts.iter().map(|(h, _)| h.clone()).collect();
    watch.retain(&names);
    for (host, connected) in hosts {
        if *connected {
            watch.refresh_host(ops, host).await;
        } else {
            watch.record(host, Err("主機未連線".to_string()));
        }
    }
}

fn tail(s: &str) -> String {
    let t = s.trim();
    let n = t.chars().count();
    if n <= TAIL_CHARS {
        t.to_string()
    } else {
        format!("…{}", t.chars().skip(n - TAIL_CHARS).collect::<String>())
    }
}

pub fn refuse_bot_caller(headers: &HeaderMap) -> Result<(), LcError> {
    if headers.contains_key("X-AM-Bot-Id") || headers.contains_key("X-AM-Bot-Token") {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "ui_only",
            "message": "herdr integration 更新會改那台的 agent 設定，只給使用者在設定頁按"})));
    }
    Ok(())
}

/// 使用者按「更新」：在 `host` 跑 `herdr integration install <name>`，再讀一次狀態。
/// 只准 [`TRACKED`] 的名字；`not installed` 的 409 `not_installed`（例外：grok／antigravity-cli 且那台那家已登入才裝）；已是 current 的 409 `already_current`；
/// 同一台同時只跑一個（409 `install_in_progress`）。快照裡沒有這個名字（還沒巡過）一律當成不能裝。
pub async fn install(
    watch: &Watch,
    ops: &dyn Ops,
    host: &str,
    name: &str,
    logged_in: &HashSet<(String, String)>,
) -> Result<Value, LcError> {
    if !TRACKED.contains(&name) {
        return Err(LcError::Bad(format!("不支援的 integration：{name}（只有 {}）", TRACKED.join("、"))));
    }
    let state = watch.items(host).into_iter().find(|r| r.name == name).map(|r| r.state);
    match state {
        Some("outdated") => {}
        // 只有 grok／antigravity-cli、而且那台那家已登入（跟自動安裝同一個判斷）才讓使用者手動裝；其餘照舊 409。
        Some("not_installed") => match AUTO_INSTALL.iter().find(|(n, _)| *n == name) {
            Some((_, tool)) if logged_in.contains(&(host.to_string(), tool.to_string())) => {}
            Some((_, tool)) => {
                return Err(LcError::conflict("not_installed", json!({"host": host, "name": name, "logged_in": false,
                    "message": format!("{name} 的 integration 還沒安裝，而且 {host} 上的 {tool} 沒有登入（或探測問不到登入狀態）；登入之後才能裝")})));
            }
            None => {
                return Err(LcError::conflict("not_installed", json!({"host": host, "name": name,
                    "message": format!("{name} 的 integration 還沒安裝；這裡只顯示（只有 grok／antigravity-cli 已登入時才能安裝）")})));
            }
        },
        Some("current") => {
            return Err(LcError::conflict("already_current", json!({"host": host, "name": name,
                "message": format!("{name} 的 integration 已是最新")})));
        }
        _ => {
            return Err(LcError::conflict("state_unknown", json!({"host": host, "name": name,
                "message": "還沒讀到這台的 integration 狀態，請稍後再試"})));
        }
    }
    run_install(watch, ops, host, name).await
}

/// 佔住這台、跑安裝、再讀一次狀態（手動與自動共用，所以 `install_in_progress` 互斥是同一把）。
async fn run_install(watch: &Watch, ops: &dyn Ops, host: &str, name: &str) -> Result<Value, LcError> {
    if !watch.installing.lock().unwrap().insert(host.to_string()) {
        return Err(LcError::conflict("install_in_progress", json!({"host": host,
            "message": format!("{host} 正在更新 integration，等它做完")})));
    }
    let outcome = ops.install(host, name).await;
    watch.installing.lock().unwrap().remove(host);
    let output = outcome.map_err(|e| LcError::Upstream(format!("在 {host} 更新 {name} integration 失敗：{}", tail(&e))))?;
    // 裝完再讀一次：回給畫面的是實際狀態，不是「指令沒報錯」。
    watch.refresh_host(ops, host).await;
    // 重讀失敗（`items` 還是裝之前的舊資料）時不拿舊的當結果：state／integration 是 null，原因放 `reread_error`。
    if let Some(e) = watch.read_error(host) {
        return Ok(json!({"host": host, "name": name, "state": null, "integration": null,
                         "reread_error": e, "output": tail(&output)}));
    }
    let row = watch.items(host).into_iter().find(|r| r.name == name);
    Ok(json!({"host": host, "name": name, "state": row.as_ref().map(|r| r.state),
              "integration": row.map(|r| r.json()), "output": tail(&output)}))
}

/// 自動安裝（#1871）：巡檢（[`sweep`]）之後跑。三條件同時成立才裝——這一輪讀到的狀態是 `not_installed`、`logged_in` 含 (主機, 工具 kind)
/// （grok／agy CLI 在那台已裝且已登入）、而且那台那一家這個 daemon 行程還沒試過。每個 (主機, 家) 只試一次，成功失敗都記下來、不重試
/// （再試靠重啟 daemon）。那台正被別的安裝佔著（`install_in_progress`）不算試過，下一輪再來。回傳這一輪實際跑過的結果，給呼叫端記 log 與通知。
pub async fn auto_install(
    watch: &Watch,
    ops: &dyn Ops,
    hosts: &[(String, bool)],
    logged_in: &HashSet<(String, String)>,
) -> Vec<AutoOutcome> {
    let mut done = vec![];
    for (host, connected) in hosts {
        if !*connected {
            continue;
        }
        let rows = watch.fresh_items(host);
        for (name, tool) in AUTO_INSTALL {
            let missing = rows.iter().any(|r| r.name == name && r.state == "not_installed");
            if !missing || !logged_in.contains(&(host.clone(), tool.to_string())) {
                continue;
            }
            let key = (host.clone(), name.to_string());
            if watch.auto.lock().unwrap().contains_key(&key) {
                continue;
            }
            let result = run_install(watch, ops, host, name).await;
            if matches!(&result, Err(LcError::Conflict(v)) if v["reason"] == "install_in_progress") {
                continue;
            }
            let (state, error) = match &result {
                Ok(v) => {
                    let st = STATE_NAMES.iter().copied().find(|n| v["state"] == *n);
                    let error = match (v["state"].as_str(), v["reread_error"].as_str()) {
                        (Some("current"), _) => None,
                        (Some(other), _) => Some(format!("裝完狀態是 {other}，不是最新")),
                        (None, Some(e)) => Some(format!("裝完重讀狀態失敗：{e}")),
                        (None, None) => Some("裝完讀不到這一家的狀態".to_string()),
                    };
                    (st, error)
                }
                Err(e) => (None, Some(lc_text(e))),
            };
            let outcome = AutoOutcome { host: host.clone(), name: name.to_string(), ok: error.is_none(), state, error, at: crate::db::now() };
            match &outcome.error {
                None => tracing::info!(host = %host, integration = name, "auto-installed the herdr integration (agent is logged in there)"),
                Some(e) => tracing::warn!(host = %host, integration = name, error = %e, "auto-install of the herdr integration failed; it is not retried until the daemon restarts"),
            }
            watch.auto.lock().unwrap().insert(key, outcome.clone());
            done.push(outcome);
        }
    }
    done
}

const STATE_NAMES: [&str; 4] = ["current", "outdated", "not_installed", "unknown"];

fn lc_text(e: &LcError) -> String {
    match e {
        LcError::Upstream(m) | LcError::Bad(m) => m.clone(),
        other => format!("{other:?}"),
    }
}

/// 正式版：本機 `/bin/sh -c`，遠端走那台主機的 ssh 連線（同 `cli_update::Real`）。
pub struct Real(pub Arc<App>);

fn script(inner: &str) -> String {
    format!(
        "{}\n[ -n \"$p\" ] || {{ echo 'AM_NO_HERDR'; exit 0; }}\n{inner}\n",
        crate::tools::login_abs_sh("herdr")
    )
}

impl Real {
    async fn run(&self, host: &str, script: &str, timeout: Duration) -> Result<String, String> {
        let out = if host == crate::config::LOCAL_HOST {
            let o = crate::hosts::sh_local(script, timeout)
                .await
                .map_err(|e| format!("{e:#}"))?
                .ok_or_else(|| format!("超過 {} 秒沒結束，已中止", timeout.as_secs()))?;
            let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            if !o.status.success() {
                return Err(format!("退出 {}：{}", o.status, tail(&text)));
            }
            text
        } else {
            let fence = self.0.hosts.fence(host).await.ok_or_else(|| format!("找不到主機 {host}"))?;
            fence.conn().ssh_exec_path_timeout(script, timeout).await.map_err(|e| format!("{e:#}"))?
        };
        if out.lines().any(|l| l.trim() == "AM_NO_HERDR") {
            return Err("這台找不到 herdr".to_string());
        }
        Ok(out)
    }
}

impl Ops for Real {
    fn status<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move { self.run(host, &script("\"$p\" integration status 2>&1"), STATUS_TIMEOUT).await })
    }

    fn install<'a>(&'a self, host: &'a str, name: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            // `name` 呼叫端已驗過在 TRACKED，這裡再擋一次，不讓任何字串進到 shell。
            if !TRACKED.contains(&name) {
                return Err(format!("不支援的 integration：{name}"));
            }
            self.run(host, &script(&format!("\"$p\" integration install {name} 2>&1")), INSTALL_TIMEOUT).await
        })
    }
}

fn watch() -> &'static Watch {
    static W: OnceLock<Watch> = OnceLock::new();
    W.get_or_init(Watch::default)
}

pub fn spawn(app: Arc<App>) {
    // 開機先等工具探測（第一輪延後 60 秒），同 upstream_update。
    crate::background_loop::spawn_periodic(&app, "herdr integration watcher", SWEEP, Duration::from_secs(60), |app| async move {
        let hosts: Vec<(String, bool)> =
            app.hosts.list().await.iter().map(|c| (c.name.clone(), c.is_local() || c.is_connected())).collect();
        sweep(watch(), &Real(app.clone()), &hosts).await;
        let logged_in = logged_in_agents(&app).await;
        for o in auto_install(watch(), &Real(app.clone()), &hosts, &logged_in).await {
            app.emit(
                "herdr_integration_auto_install",
                json!({"host": o.host, "name": o.name, "ok": o.ok, "state": o.state, "error": o.error}),
            )
            .await;
        }
    });
}

/// 工具探測說「已裝且已登入」的 (主機, kind)；只收 [`AUTO_INSTALL`] 用得到的兩個 kind。
async fn logged_in_agents(app: &Arc<App>) -> HashSet<(String, String)> {
    let tools = app.tools.lock().await;
    let mut out = HashSet::new();
    for (host, ht) in tools.iter() {
        for (_, kind) in AUTO_INSTALL {
            if ht.tools.get(kind).is_some_and(|t| t.installed && t.logged_in == Some(true)) {
                out.insert((host.clone(), kind.to_string()));
            }
        }
    }
    out
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/integration-updates", get(get_status))
        .route("/hosts/{name}/herdr-integration", axum::routing::post(post_install))
}

/// `GET /api/integration-updates`：最近一輪的快照，不觸發抓取。第一輪還沒跑完是空陣列。
async fn get_status() -> Json<Value> {
    Json(json!({ "hosts": watch().snapshot() }))
}

#[derive(Deserialize, Default)]
struct InstallIn {
    /// 沒帶＝空字串，交給 [`install`] 的白名單擋成 400；不讓 extractor 的 422 搶在 bot 的 403 前面。
    #[serde(default)]
    name: String,
}

/// `POST /api/hosts/{name}/herdr-integration {name}`：使用者按「更新」才跑。
async fn post_install(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    AxPath(host): AxPath<String>,
    body: Option<Json<InstallIn>>,
) -> Result<Json<Value>, LcError> {
    refuse_bot_caller(&headers)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    if app.hosts.get(&host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let logged_in = logged_in_agents(&app).await;
    Ok(Json(install(watch(), &Real(app), &host, &b.name, &logged_in).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
pi: not installed (/home/u/.pi/agent/extensions/herdr-agent-state.ts)
claude: outdated (v8 < v11) (/home/u/.claude/hooks/herdr-agent-state.sh)
codex: outdated (v8 < v10) (/home/u/.codex/herdr-agent-state.sh)
antigravity-cli: not installed (/home/u/.gemini/config/hooks/herdr-agent-state.sh)
grok: not installed (/home/u/.grok/hooks/herdr-agent-state.sh)
letta (experimental): not installed (/home/u/.letta/hooks/herdr-agent-session.sh)
";

    #[test]
    fn parse_keeps_only_tracked_in_fixed_order_with_versions_and_paths() {
        let rows = parse_status(SAMPLE);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["claude", "codex", "antigravity-cli", "grok"], "pi／letta 不列，順序固定");
        assert_eq!(rows[0].state, "outdated");
        assert_eq!((rows[0].installed_version, rows[0].required_version), (Some(8), Some(11)));
        assert_eq!(rows[0].path.as_deref(), Some("/home/u/.claude/hooks/herdr-agent-state.sh"));
        assert_eq!((rows[1].installed_version, rows[1].required_version), (Some(8), Some(10)));
        assert_eq!(rows[2].state, "not_installed");
        assert_eq!((rows[2].installed_version, rows[2].required_version), (None, None));
        assert_eq!(rows[3].state, "not_installed");
    }

    #[test]
    fn parse_reads_current_and_flags_unrecognised_state_as_unknown_not_current() {
        let rows = parse_status("claude: current (v11) (/h/.claude/hooks/x.sh)\ncodex: wedged somehow (/h/.codex/x.sh)\n");
        assert_eq!(rows[0].state, "current");
        assert_eq!((rows[0].installed_version, rows[0].required_version), (Some(11), Some(11)));
        assert_eq!(rows[1].state, "unknown", "不認得的狀態不能當成最新");
        assert_eq!(rows[1].detail.as_deref(), Some("wedged somehow (/h/.codex/x.sh)"));
        assert!(parse_status("").is_empty());
        assert!(parse_status("garbage without colon\n").is_empty());
    }

    struct Fake {
        /// host → 狀態輸出；沒有的主機讀取失敗。
        status: Mutex<BTreeMap<String, String>>,
        installs: Mutex<Vec<(String, String)>>,
        /// 安裝成功後該主機的新輸出。
        after_install: Mutex<Option<String>>,
        fail_install: bool,
    }

    impl Fake {
        fn new(host: &str, out: &str) -> Self {
            Self {
                status: Mutex::new([(host.to_string(), out.to_string())].into()),
                installs: Mutex::new(vec![]),
                after_install: Mutex::new(None),
                fail_install: false,
            }
        }
    }

    impl Ops for Fake {
        fn status<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move { self.status.lock().unwrap().get(host).cloned().ok_or_else(|| "ssh 逾時".to_string()) })
        }
        fn install<'a>(&'a self, host: &'a str, name: &'a str) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                self.installs.lock().unwrap().push((host.to_string(), name.to_string()));
                if self.fail_install {
                    return Err("permission denied".to_string());
                }
                if let Some(next) = self.after_install.lock().unwrap().take() {
                    self.status.lock().unwrap().insert(host.to_string(), next);
                }
                Ok("installed".to_string())
            })
        }
    }

    #[tokio::test]
    async fn sweep_records_each_host_keeps_old_items_on_failure_and_drops_removed_hosts() {
        let watch = Watch::default();
        let fake = Fake::new("local", SAMPLE);
        sweep(&watch, &fake, &[("local".into(), true), ("m4p".into(), false), ("gone".into(), true)]).await;
        let snap = watch.snapshot();
        let by = |h: &str| snap.iter().find(|s| s["host"] == h).cloned().unwrap();
        assert_eq!(by("local")["items"].as_array().unwrap().len(), 4);
        assert!(by("local")["error"].is_null());
        assert_eq!(by("m4p")["error"], "主機未連線");
        assert_eq!(by("m4p")["items"].as_array().unwrap().len(), 0);
        assert_eq!(by("gone")["error"], "ssh 逾時", "讀取失敗是錯誤，不是「沒有 integration」");

        // 第二輪：local 讀失敗，上一次的 items 還在、只多一個 error；gone 已不在主機清單就整筆丟掉。
        fake.status.lock().unwrap().clear();
        sweep(&watch, &fake, &[("local".into(), true), ("m4p".into(), false)]).await;
        let snap = watch.snapshot();
        assert_eq!(snap.len(), 2);
        let local = snap.iter().find(|s| s["host"] == "local").unwrap();
        assert_eq!(local["error"], "ssh 逾時");
        assert_eq!(local["items"].as_array().unwrap().len(), 4, "失敗那一輪不清掉上次的結果");
    }

    #[test]
    fn bot_callers_are_refused_but_the_ui_is_not() {
        assert!(refuse_bot_caller(&HeaderMap::new()).is_ok());
        for h in ["X-AM-Bot-Id", "X-AM-Bot-Token"] {
            let mut m = HeaderMap::new();
            m.insert(h, "x".parse().unwrap());
            assert!(matches!(refuse_bot_caller(&m), Err(LcError::Forbidden(v)) if v["reason"] == "ui_only"));
        }
    }

    #[tokio::test]
    async fn install_runs_only_for_an_outdated_tracked_name_then_rereads_the_real_state() {
        let watch = Watch::default();
        let fake = Fake::new("local", SAMPLE);
        *fake.after_install.lock().unwrap() = Some(
            "claude: current (v11) (/home/u/.claude/hooks/herdr-agent-state.sh)\ncodex: outdated (v8 < v10) (/home/u/.codex/x.sh)\n".into(),
        );
        sweep(&watch, &fake, &[("local".into(), true)]).await;

        let r = install(&watch, &fake, "local", "claude", &who(&[])).await.unwrap();
        assert_eq!(r["state"], "current", "回的是裝完重讀的實際狀態");
        assert_eq!(*fake.installs.lock().unwrap(), vec![("local".to_string(), "claude".to_string())]);
        assert!(watch.installing.lock().unwrap().is_empty(), "做完要放掉這台的佔位");
        // 快照也跟著更新，codex 沒被動到。
        let items = watch.items("local");
        assert_eq!(items.iter().find(|r| r.name == "claude").unwrap().state, "current");
        assert_eq!(items.iter().find(|r| r.name == "codex").unwrap().state, "outdated");
    }

    #[tokio::test]
    async fn install_refuses_untracked_not_installed_current_and_never_seen_without_running_anything() {
        let watch = Watch::default();
        let fake = Fake::new("local", &format!("{SAMPLE}claude: current (v11) (/x/y.sh)\n"));
        // 沒巡過：不知道狀態就不裝。
        assert!(matches!(install(&watch, &fake, "local", "codex", &who(&[])).await, Err(LcError::Conflict(v)) if v["reason"] == "state_unknown"));
        sweep(&watch, &fake, &[("local".into(), true)]).await;
        // 不在名單：400，包含會被當成 shell 片段的字串。
        for bad in ["pi", "claude; rm -rf ~", "", "../x"] {
            assert!(matches!(install(&watch, &fake, "local", bad, &who(&[])).await, Err(LcError::Bad(_))), "{bad:?}");
        }
        assert!(matches!(install(&watch, &fake, "local", "grok", &who(&[])).await, Err(LcError::Conflict(v)) if v["reason"] == "not_installed"));
        assert!(matches!(install(&watch, &fake, "local", "claude", &who(&[])).await, Err(LcError::Conflict(v)) if v["reason"] == "already_current"));
        assert!(fake.installs.lock().unwrap().is_empty(), "被拒的一個指令都不跑");
    }

    #[tokio::test]
    async fn install_failure_is_an_error_releases_the_slot_and_second_install_on_a_busy_host_is_409() {
        let watch = Watch::default();
        let mut fake = Fake::new("local", SAMPLE);
        fake.fail_install = true;
        sweep(&watch, &fake, &[("local".into(), true)]).await;
        let e = install(&watch, &fake, "local", "codex", &who(&[])).await.unwrap_err();
        assert!(matches!(&e, LcError::Upstream(m) if m.contains("permission denied")), "{e:?}");
        assert!(watch.installing.lock().unwrap().is_empty(), "失敗也要放掉佔位，不然那台永遠裝不了");
        assert_eq!(watch.items("local").iter().find(|r| r.name == "codex").unwrap().state, "outdated", "失敗不改快照");

        watch.installing.lock().unwrap().insert("local".to_string());
        let busy = install(&watch, &fake, "local", "codex", &who(&[])).await.unwrap_err();
        assert!(matches!(busy, LcError::Conflict(v) if v["reason"] == "install_in_progress"));
    }

    fn who(pairs: &[(&str, &str)]) -> HashSet<(String, String)> {
        pairs.iter().map(|(h, k)| (h.to_string(), k.to_string())).collect()
    }

    const AFTER_GROK: &str = "grok: current (v3) (/home/u/.grok/hooks/herdr-agent-state.sh)\nantigravity-cli: not installed (/home/u/.gemini/config/hooks/herdr-agent-state.sh)\nclaude: outdated (v8 < v11) (/c)\ncodex: outdated (v8 < v10) (/x)\n";

    #[tokio::test]
    async fn auto_install_runs_only_for_a_logged_in_grok_or_agy_that_is_not_installed() {
        let watch = Watch::default();
        let fake = Fake::new("local", SAMPLE);
        *fake.after_install.lock().unwrap() = Some(AFTER_GROK.into());
        let hosts = [("local".to_string(), true)];
        sweep(&watch, &fake, &hosts).await;

        // grok 已登入、agy 沒登入；claude／codex（outdated）就算登入也不自動升。
        let out = auto_install(&watch, &fake, &hosts, &who(&[("local", "grok"), ("local", "claude"), ("local", "codex")])).await;
        assert_eq!(*fake.installs.lock().unwrap(), vec![("local".to_string(), "grok".to_string())]);
        assert_eq!(out.len(), 1);
        assert!(out[0].ok && out[0].error.is_none() && out[0].state == Some("current"), "{out:?}");
        assert!(watch.installing.lock().unwrap().is_empty());
        assert_eq!(watch.items("local").iter().find(|r| r.name == "grok").unwrap().state, "current", "裝完立刻重讀");

        // 設定頁快照帶出自動裝的結果；沒試過的列沒有這個欄位。
        let snap = watch.snapshot();
        let items = snap[0]["items"].as_array().unwrap();
        let by = |n: &str| items.iter().find(|i| i["name"] == n).unwrap();
        assert_eq!(by("grok")["auto_install"]["ok"], true);
        assert!(by("antigravity-cli").get("auto_install").is_none());
    }

    #[tokio::test]
    async fn auto_install_without_a_login_or_a_fresh_status_or_a_connection_runs_nothing() {
        let watch = Watch::default();
        let fake = Fake::new("local", SAMPLE);
        let hosts = [("local".to_string(), true), ("m4p".to_string(), false)];
        sweep(&watch, &fake, &hosts).await;
        // 沒登入（或探測問不到）。
        assert!(auto_install(&watch, &fake, &hosts, &who(&[])).await.is_empty());
        // 主機沒連線：就算探測快取留著登入也不裝。
        assert!(auto_install(&watch, &fake, &hosts, &who(&[("m4p", "grok")])).await.is_empty());
        // 這一輪讀狀態失敗（items 是上次的）：不拿舊資料決定。
        fake.status.lock().unwrap().clear();
        sweep(&watch, &fake, &[("local".to_string(), true)]).await;
        assert!(auto_install(&watch, &fake, &[("local".to_string(), true)], &who(&[("local", "grok")])).await.is_empty());
        assert!(fake.installs.lock().unwrap().is_empty());
        assert!(watch.snapshot().iter().all(|h| h["items"].as_array().unwrap().iter().all(|i| i.get("auto_install").is_none())));
    }

    #[tokio::test]
    async fn auto_install_tries_each_host_and_family_once_and_does_not_retry_a_failure() {
        let watch = Watch::default();
        let mut fake = Fake::new("local", SAMPLE);
        fake.fail_install = true;
        let hosts = [("local".to_string(), true)];
        sweep(&watch, &fake, &hosts).await;
        let logged = who(&[("local", "grok"), ("local", "agy")]);

        let first = auto_install(&watch, &fake, &hosts, &logged).await;
        assert_eq!(first.len(), 2, "grok 與 agy 各試一次");
        assert!(first.iter().all(|o| !o.ok && o.error.as_deref().is_some_and(|e| e.contains("permission denied"))), "{first:?}");
        assert!(watch.installing.lock().unwrap().is_empty(), "失敗也放掉佔位");
        // 下一輪、再下一輪：仍是 not_installed 也不再裝。
        sweep(&watch, &fake, &hosts).await;
        assert!(auto_install(&watch, &fake, &hosts, &logged).await.is_empty());
        assert_eq!(fake.installs.lock().unwrap().len(), 2);
        // 失敗記在快照裡給設定頁看。
        let snap = watch.snapshot();
        let grok = snap[0]["items"].as_array().unwrap().iter().find(|i| i["name"] == "grok").unwrap();
        assert_eq!(grok["auto_install"]["ok"], false);
        assert!(grok["auto_install"]["error"].as_str().unwrap().contains("permission denied"));
    }

    #[tokio::test]
    async fn auto_install_leaves_a_busy_host_for_the_next_round_instead_of_using_up_its_one_try() {
        let watch = Watch::default();
        let fake = Fake::new("local", SAMPLE);
        let hosts = [("local".to_string(), true)];
        sweep(&watch, &fake, &hosts).await;
        watch.installing.lock().unwrap().insert("local".to_string());
        assert!(auto_install(&watch, &fake, &hosts, &who(&[("local", "grok")])).await.is_empty());
        assert!(fake.installs.lock().unwrap().is_empty(), "手動安裝佔著時不插隊");
        watch.installing.lock().unwrap().remove("local");
        assert_eq!(auto_install(&watch, &fake, &hosts, &who(&[("local", "grok")])).await.len(), 1, "放掉之後那一次還在");
    }

    #[tokio::test]
    async fn manual_install_of_a_missing_integration_is_allowed_only_for_a_logged_in_grok_or_agy() {
        let watch = Watch::default();
        let fake = Fake::new("local", SAMPLE);
        *fake.after_install.lock().unwrap() = Some(AFTER_GROK.into());
        sweep(&watch, &fake, &[("local".to_string(), true)]).await;
        let conflict = |r: Result<Value, LcError>| match r {
            Err(LcError::Conflict(v)) => v,
            other => panic!("要 409：{other:?}"),
        };

        // 沒登入：409，原因寫在回應裡，什麼都沒跑。
        let v = conflict(install(&watch, &fake, "local", "grok", &who(&[])).await);
        assert_eq!(v["reason"], "not_installed");
        assert_eq!(v["logged_in"], false);
        assert!(v["message"].as_str().unwrap().contains("沒有登入"), "{v}");
        // 別台登入不算這台。
        conflict(install(&watch, &fake, "local", "antigravity-cli", &who(&[("m4p", "agy")])).await);
        assert!(fake.installs.lock().unwrap().is_empty());

        // 已登入：可以裝，回的是裝完重讀的實際狀態，也沿用同一把互斥。
        watch.installing.lock().unwrap().insert("local".to_string());
        let busy = conflict(install(&watch, &fake, "local", "grok", &who(&[("local", "grok")])).await);
        assert_eq!(busy["reason"], "install_in_progress");
        watch.installing.lock().unwrap().remove("local");
        let r = install(&watch, &fake, "local", "grok", &who(&[("local", "grok")])).await.unwrap();
        assert_eq!(r["state"], "current");
        assert_eq!(*fake.installs.lock().unwrap(), vec![("local".to_string(), "grok".to_string())]);
    }

    #[tokio::test]
    async fn manual_install_of_a_missing_claude_or_codex_stays_409_even_when_logged_in() {
        let watch = Watch::default();
        let fake = Fake::new("local", "claude: not installed (/c)\ncodex: not installed (/x)\n");
        sweep(&watch, &fake, &[("local".to_string(), true)]).await;
        let logged = who(&[("local", "claude"), ("local", "codex")]);
        for name in ["claude", "codex"] {
            let r = install(&watch, &fake, "local", name, &logged).await;
            assert!(matches!(&r, Err(LcError::Conflict(v)) if v["reason"] == "not_installed" && v.get("logged_in").is_none()), "{name}: {r:?}");
        }
        assert!(fake.installs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_reread_after_install_reports_no_state_and_the_real_reason_not_the_old_row() {
        let watch = Watch::default();
        let fake = Fake::new("local", SAMPLE);
        let hosts = [("local".to_string(), true)];
        sweep(&watch, &fake, &hosts).await;
        // 裝的當下成功，但接著的重讀失敗（ssh 逾時）：不能把舊的 not_installed 當成裝完的結果。
        fake.status.lock().unwrap().clear();
        let out = auto_install(&watch, &fake, &hosts, &who(&[("local", "grok")])).await;
        assert_eq!(out.len(), 1);
        assert!(!out[0].ok && out[0].state.is_none(), "{out:?}");
        let e = out[0].error.as_deref().unwrap();
        assert!(e.contains("重讀狀態失敗") && e.contains("ssh 逾時"), "{e}");

        // 手動路徑一樣：200 但 state／integration 是 null，帶 reread_error。
        let watch = Watch::default();
        let fake = Fake::new("local", SAMPLE);
        sweep(&watch, &fake, &hosts).await;
        fake.status.lock().unwrap().clear();
        let r = install(&watch, &fake, "local", "grok", &who(&[("local", "grok")])).await.unwrap();
        assert!(r["state"].is_null() && r["integration"].is_null(), "{r}");
        assert!(r["reread_error"].as_str().unwrap().contains("ssh 逾時"));
    }
}
