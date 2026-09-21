//! 預覽模式（issue #253，SPEC §6.12）：頂層 bot 的專案起一顆 vite dev server，UI 把它內嵌在右半面板。
//!
//! 一顆預覽＝該 bot 自己那個 tab 裡的一顆 service pane（`pane.split`）＋ `bot_previews` 一列。
//! 行程與 port 的查詢全走 [`PreviewEnv`]：正式是 herdr＋本機 TCP，測試換成決定性的假貨，
//! 不碰真 herdr、真行程（#211：那種測試只會在 CI 上紅）。
//!
//! 狀態轉移（[`next_status`]）、偵測（[`detect_dir`]）、挑 port（[`pick_port`]）都是純函式。
//! 所有會動 pane／DB 的入口共用一把全域鎖（[`gate`]）：預覽操作很短，而且 stop／delete bot
//! 是在 bot 鎖裡呼叫進來的，所以這裡**絕不**再拿 bot 鎖。

use crate::db;
use crate::herdr::HerdrClient;
use crate::lifecycle::{LcError, LcResult};
use crate::state::App;
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 5173 留給人手開的 `npx vite`。
pub const PORT_START: u16 = 5180;
/// 往上找多少顆；同時預覽二十顆頂層 bot 也夠。
pub const PORT_SPAN: u16 = 100;
/// port 開始 listen 之前最多等多久，逾時轉 `failed`。
pub const START_TIMEOUT_SECS: i64 = 60;
/// 起動期間（`starting`）與已經在跑（`running`）的監看間隔：後者只是確認還活著，不必每秒打一輪 TCP／herdr。
#[cfg(not(test))]
const POLL: Duration = Duration::from_secs(1);
#[cfg(test)]
const POLL: Duration = Duration::from_millis(50);
#[cfg(not(test))]
const RUNNING_POLL: Duration = Duration::from_secs(5);
#[cfg(test)]
const RUNNING_POLL: Duration = Duration::from_millis(100);
const TAIL_LINES: u32 = 40;
pub const SOURCE_SPAWNED: &str = "spawned";
pub const SOURCE_ATTACHED: &str = "attached";
const CONFIG_NAMES: [&str; 4] = ["vite.config.ts", "vite.config.mts", "vite.config.js", "vite.config.mjs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Off,
    Starting,
    Running,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Off => "off",
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Failed => "failed",
        }
    }
    fn parse(s: &str) -> Status {
        match s {
            "starting" => Status::Starting,
            "running" => Status::Running,
            "failed" => Status::Failed,
            _ => Status::Off,
        }
    }
    /// 這一列還佔著 port 與 pane。
    fn is_live(self) -> bool {
        matches!(self, Status::Starting | Status::Running)
    }
}

/// 外面世界的四個問題。`None`＝問不到（不是「沒有」）：呼叫端一律當「沒變」。
pub trait PreviewEnv: Send + Sync {
    /// 在 `target_pane` 所在的 tab 裡開一顆 pane（cwd＝`cwd`），送出 `cmd`；回新 pane 的 id。
    fn spawn<'a>(&'a self, target_pane: &'a str, cwd: &'a str, cmd: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;
    fn pane_alive<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<bool>>;
    fn pane_tail<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, String>;
    /// 關 pane；那個 tab 因此空了就一起關。回 `true`＝關掉了或確定本來就不在；`false`＝關不掉／問不到（不可當成已關）。
    fn close_pane<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, bool>;
    /// 測試用：模擬「這顆 bot 的 herdr session 現在拿不到」。
    #[cfg(test)]
    fn resolvable(&self) -> bool {
        true
    }
    /// 本機這個 port 現在有沒有人在 listen。
    fn port_listening(&self, port: u16) -> BoxFuture<'_, bool>;
    /// 本機上在 listen 的 dev server：命令列對得上已知的 dev server，或行程 cwd 落在 `roots`（AG Man 認得的專案路徑）底下；
    /// `None`＝掃不到（不是「沒有」）。
    fn scan_servers<'a>(&'a self, roots: &'a [String]) -> BoxFuture<'a, Option<Vec<ViteProc>>>;
    /// 這顆 pane 的行程樹現在 listen 的 port（昇冪）；`None`＝問不到。dev script 起的 server 自己挑 port，靠這個觀察。
    fn pane_ports<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<Vec<u16>>>;
    /// 這個目錄屬於哪個 git repo；不是 repo 或讀不到＝`None`。
    fn repo_key<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Option<RepoKey>>;
}

/// 判斷「同一個 repo」用：git common dir（同一個 repo 的所有 worktree 共用）與 origin URL（各自 clone 的同一個 repo）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoKey {
    pub common: String,
    pub origin: Option<String>,
}

/// 兩個目錄是不是同一個 repo：common dir 相同，或兩邊都有 origin 而且相同。
pub fn same_repo(a: &RepoKey, b: &RepoKey) -> bool {
    a.common == b.common || matches!((&a.origin, &b.origin), (Some(x), Some(y)) if x == y)
}

/// `git rev-parse --git-common-dir`（可能是相對於 `dir` 的路徑）與 `git config remote.origin.url` 的輸出。
pub fn parse_repo_key(dir: &str, common_out: &str, origin_out: &str) -> Option<RepoKey> {
    let common = common_out.trim();
    if common.is_empty() {
        return None;
    }
    let p = Path::new(common);
    let abs = if p.is_absolute() { p.to_path_buf() } else { Path::new(dir).join(p) };
    let common = std::fs::canonicalize(&abs).unwrap_or(abs).to_string_lossy().into_owned();
    let origin = Some(origin_out.trim().to_string()).filter(|o| !o.is_empty());
    Some(RepoKey { common, origin })
}

/// 一顆已經在跑、而且有 TCP listen 的 dev server（名字沿用 v2，不只 vite）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViteProc {
    pub pid: i32,
    pub port: u16,
    pub cwd: String,
    /// `vite`／`next`／`webpack`／…／`unknown`（命令列認不得、只因為 cwd 在專案底下才列進來）。
    pub kind: String,
}

/// herdr＋本機 TCP。
pub struct RealEnv {
    pub client: HerdrClient,
    pub app: Arc<App>,
}

impl PreviewEnv for RealEnv {
    fn spawn<'a>(&'a self, target_pane: &'a str, cwd: &'a str, cmd: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            // 往下切：只吃高度，不像左右切會把 agent 的 TUI 擠到 ~31 欄以下而亂排。
            let pane = self.client.pane_split(target_pane, "down", cwd, json!({})).await?;
            let typed = async {
                self.client.pane_send_text(&pane.pane_id, cmd).await?;
                self.client.pane_send_keys(&pane.pane_id, &["enter"]).await
            }
            .await;
            if let Err(e) = typed {
                let _ = self.client.pane_close(&pane.pane_id).await;
                return Err(e);
            }
            Ok(pane.pane_id)
        })
    }
    fn pane_alive<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move { self.client.pane_get(pane_id).await.ok().map(|p| p.is_some()) })
    }
    fn pane_tail<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, String> {
        Box::pin(async move {
            self.client.pane_read(pane_id, "recent", TAIL_LINES).await.map(|r| r.text).unwrap_or_default()
        })
    }
    fn close_pane<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            match self.client.pane_get(pane_id).await {
                Err(_) => false,
                Ok(None) => true,
                Ok(Some(p)) => {
                    crate::lifecycle::close_pane_and_tab(&self.client, Some(&p.workspace_id), Some(&p.tab_id), pane_id).await;
                    // 關指令的結果被吞掉了：再問一次，確定不在才算關掉。
                    matches!(self.client.pane_get(pane_id).await, Ok(None))
                }
            }
        })
    }
    fn port_listening(&self, port: u16) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let c = tokio::net::TcpStream::connect(("127.0.0.1", port));
            matches!(tokio::time::timeout(Duration::from_millis(400), c).await, Ok(Ok(_)))
        })
    }
    fn scan_servers<'a>(&'a self, roots: &'a [String]) -> BoxFuture<'a, Option<Vec<ViteProc>>> {
        Box::pin(scan_real(roots))
    }
    fn pane_ports<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<Vec<u16>>> {
        Box::pin(async move {
            let local = crate::config::LOCAL_HOST;
            let probe = self.app.probe();
            let shell = self.client.pane_shell(pane_id).await.ok()?;
            let dump = probe.dump(&self.app, local).await.ok()?;
            let facts = crate::panes::facts_from(&shell, &dump, pane_id)?;
            probe.listen_ports(local, &facts.pids).await
        })
    }
    fn repo_key<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Option<RepoKey>> {
        Box::pin(async move {
            let q = crate::hosts::sh_quote(dir);
            let t = Duration::from_secs(5);
            let common = crate::hosts::sh_local(&format!("git -C {q} rev-parse --git-common-dir 2>/dev/null"), t).await.ok().flatten()?;
            if !common.status.success() {
                return None;
            }
            let origin = crate::hosts::sh_local(&format!("git -C {q} config --get remote.origin.url 2>/dev/null"), t).await.ok().flatten()?;
            parse_repo_key(dir, &String::from_utf8_lossy(&common.stdout), &String::from_utf8_lossy(&origin.stdout))
        })
    }
}

/// 三次查詢：`ps`（每個 pid 的命令列）、`lsof`（全機 listen 的 port）、`lsof`（那些 pid 的 cwd），再交給 [`join_servers`] 篩。
async fn scan_real(roots: &[String]) -> Option<Vec<ViteProc>> {
    let t = Duration::from_secs(10);
    let ps = crate::hosts::sh_local("ps -axo pid=,command=", t).await.ok().flatten()?;
    let cmds = parse_ps_commands(&String::from_utf8_lossy(&ps.stdout));
    let ports = crate::hosts::sh_local("lsof -nP -iTCP -sTCP:LISTEN -Fpn 2>/dev/null", t).await.ok().flatten()?;
    let ports = crate::panes::parse_lsof(&String::from_utf8_lossy(&ports.stdout));
    if ports.is_empty() {
        return Some(Vec::new());
    }
    let list = ports.keys().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let cwds = crate::hosts::sh_local(&format!("lsof -nP -a -d cwd -p {list} -Fpn 2>/dev/null"), t).await.ok().flatten()?;
    Some(join_servers(&ports, &parse_lsof_cwd(&String::from_utf8_lossy(&cwds.stdout)), &cmds, roots, std::process::id() as i32))
}

#[cfg(test)]
pub(crate) type EnvOverride = std::sync::Mutex<Option<Arc<dyn PreviewEnv>>>;

/// 這顆 bot 的 run 用的 env：測試可以在 `App::preview_env` 塞假貨。
async fn env_for(app: &Arc<App>, run: &db::Run) -> LcResult<Arc<dyn PreviewEnv>> {
    #[cfg(test)]
    if let Some(e) = app.preview_env.lock().unwrap().clone() {
        return if e.resolvable() { Ok(e) } else { Err(LcError::Upstream("no Herdr session is available (test)".into())) };
    }
    let client = app
        .herdr_for_run(run)
        .await
        .ok_or_else(|| LcError::Upstream(format!("no Herdr session is available for run `{}`", run.id)))?;
    Ok(Arc::new(RealEnv { client, app: app.clone() }))
}

/// 沒有 run 可問（bot 已停）時關舊 pane 用的 env：本機的管理 session。
fn fallback_env(app: &Arc<App>) -> Arc<dyn PreviewEnv> {
    #[cfg(test)]
    if let Some(e) = app.preview_env.lock().unwrap().clone() {
        return e;
    }
    Arc::new(RealEnv { client: app.herdr.clone(), app: app.clone() })
}

/// 這顆 bot 沒有 active run 時關舊 pane 用的 env：bot 自己設定的 session（不是猜管理 session）。
/// 拿不到（bot／session 讀不到、herdr 沒連）＝`None`：不知道 pane 在哪個 session，呼叫端不可動它。
async fn env_for_bot(app: &Arc<App>, bot_id: &str) -> Option<Arc<dyn PreviewEnv>> {
    #[cfg(test)]
    if let Some(e) = app.preview_env.lock().unwrap().clone() {
        return e.resolvable().then_some(e);
    }
    let bot = db::bot(&app.db, bot_id).await.ok()??;
    let session = app.session_for_bot(&bot, crate::config::LOCAL_HOST).await?;
    let client = app.herdr_for_session(crate::config::LOCAL_HOST, &session).await?;
    Some(Arc::new(RealEnv { client, app: app.clone() }))
}

// ───────────────────────────── 純函式 ─────────────────────────────

/// 往下找幾層（`<cwd>` 是第 0 層）。
pub const SEARCH_DEPTH: usize = 3;
/// 最多走過幾個目錄、最多列幾個候選：專案目錄底下可能是整個 monorepo 或一堆 worktree，不能無限掃。
pub const SEARCH_MAX_DIRS: usize = 2000;
pub const MAX_CANDIDATES: usize = 20;
/// 不往裡面找的目錄名：相依套件、建置產物、worktree 目錄（那是別份 checkout，不是這顆 bot 的工作樹）。隱藏目錄（`.git`、`.claude`…）一律略過。
const SKIP_DIRS: [&str; 6] = ["node_modules", "target", "dist", "build", "worktrees", "vendor"];

fn skipped(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

/// 候選目錄：在 `<cwd>` 底下有界地找（最多 [`SEARCH_DEPTH`] 層、走過 [`SEARCH_MAX_DIRS`] 個目錄、留 [`MAX_CANDIDATES`] 個），
/// 有 `vite.config.{ts,mts,js,mjs}`，或 `package.json` 帶 `dev` script 的全列。順序：`<cwd>` 本身、`<cwd>/web`，其餘照（層數、路徑）排序。
/// 找到 vite 設定的目錄不再往裡面找。全都沒有回試過的路徑。
pub fn detect_dirs(
    cwd: &Path,
    exists: impl Fn(&Path) -> bool,
    subdirs: impl Fn(&Path) -> Vec<PathBuf>,
    has_dev_script: impl Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, Vec<String>> {
    let has_config = |dir: &Path| CONFIG_NAMES.iter().any(|n| exists(&dir.join(n)));
    let mut found: Vec<(usize, PathBuf)> = Vec::new();
    let mut level = vec![cwd.to_path_buf()];
    let mut visited = 0usize;
    for depth in 0..=SEARCH_DEPTH {
        let mut next = Vec::new();
        for dir in level {
            if visited >= SEARCH_MAX_DIRS {
                break;
            }
            visited += 1;
            if has_config(&dir) {
                found.push((depth, dir));
                continue;
            }
            // 有 `dev` script 的也是候選，但不擋住往下找：monorepo 根目錄常有一個 `turbo run dev`，真正的 app 在裡面。
            if has_dev_script(&dir) {
                found.push((depth, dir.clone()));
            }
            if depth == SEARCH_DEPTH {
                continue;
            }
            let mut subs: Vec<PathBuf> = subdirs(&dir)
                .into_iter()
                .filter(|p| p.file_name().is_some_and(|n| !skipped(&n.to_string_lossy())))
                .collect();
            subs.sort();
            next.extend(subs);
        }
        level = next;
    }
    let web = cwd.join("web");
    found.sort_by(|(da, a), (db, b)| {
        let rank = |d: &PathBuf, depth: usize| if d == cwd { 0 } else if *d == web { 1 } else { 2 + depth };
        rank(a, *da).cmp(&rank(b, *db)).then_with(|| a.cmp(b))
    });
    found.truncate(MAX_CANDIDATES);
    if !found.is_empty() {
        return Ok(found.into_iter().map(|(_, d)| d).collect());
    }
    let mut tried = Vec::new();
    for dir in [cwd.to_path_buf(), cwd.join("web")] {
        tried.extend(CONFIG_NAMES.iter().map(|n| dir.join(n).to_string_lossy().into_owned()));
    }
    tried.push(cwd.join("**").join("vite.config.*").to_string_lossy().into_owned() + &format!("、package.json 的 dev script（最多 {SEARCH_DEPTH} 層）"));
    Err(tried)
}

/// 命令列對得上哪種已知的 dev server。看的是命令列裡每個字的檔名部分（`vitest`、`vitepress` 之類不算）。
pub fn dev_kind(cmd: &str) -> Option<&'static str> {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let has = |w: &str| toks.contains(&w);
    for t in &toks {
        let base = t.rsplit('/').next().unwrap_or(t);
        let kind = match base {
            "vite" | "vite.js" => "vite",
            "next-server" => "next",
            "next" if has("dev") || has("start") => "next",
            "webpack" | "webpack-dev-server" => "webpack",
            "astro" => "astro",
            "remix" | "remix-serve" => "remix",
            "storybook" | "start-storybook" => "storybook",
            "nuxt" | "nuxi" => "nuxt",
            "rsbuild" => "rsbuild",
            "parcel" => "parcel",
            "ng" if has("serve") => "angular",
            "react-scripts" => "react-scripts",
            "bun" if has("--hot") => "bun",
            _ => continue,
        };
        return Some(kind);
    }
    None
}

/// 不是 dev server、只是碰巧 cwd 在專案底下的常駐程式：資料庫、ssh、AG Man 自己與 herdr 不列。
fn is_infrastructure(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first).trim_start_matches('-').trim_end_matches(':');
    ["ssh", "sshd", "postgres", "mysqld", "mongod", "redis-server", "agents-managerd", "herdr", "launchd"].contains(&base)
}

/// cwd 是不是落在某個專案路徑底下（含正好等於）。
pub fn under_any(cwd: &str, roots: &[String]) -> bool {
    let c = norm(cwd);
    roots.iter().any(|r| {
        let r = norm(r);
        c == r || (c.len() > r.len() && c.starts_with(r) && c.as_bytes()[r.len()] == b'/')
    })
}

/// 一個在 listen 的行程要不要列：命令列是已知 dev server，或 cwd 在專案底下（而且不是常駐的基礎設施）。
pub fn classify_listener(cmd: &str, cwd: &str, roots: &[String]) -> Option<&'static str> {
    if let Some(k) = dev_kind(cmd) {
        return Some(k);
    }
    (under_any(cwd, roots) && !is_infrastructure(cmd)).then_some("unknown")
}

/// `ps -axo pid=,command=` → pid 到命令列。
pub fn parse_ps_commands(out: &str) -> HashMap<i32, String> {
    out.lines()
        .filter_map(|l| {
            let (pid, cmd) = l.trim_start().split_once(char::is_whitespace)?;
            Some((pid.parse().ok()?, cmd.trim().to_string()))
        })
        .collect()
}

/// `lsof -a -d cwd -Fpn`：`p<pid>` 之後的 `n<path>` 是那個 pid 的 cwd。
pub fn parse_lsof_cwd(out: &str) -> HashMap<i32, String> {
    let mut cur = None;
    let mut m = HashMap::new();
    for l in out.lines() {
        if let Some(r) = l.strip_prefix('p') {
            cur = r.trim().parse().ok();
        } else if let (Some(r), Some(pid)) = (l.strip_prefix('n'), cur) {
            m.insert(pid, r.to_string());
        }
    }
    m
}

/// 有 listen port、查得到 cwd、又符合 [`classify_listener`] 的才列（一顆行程多個 port 就一個 port 一筆；`own_pid` 自己不列）。
pub fn join_servers(
    ports: &HashMap<i32, Vec<u16>>,
    cwds: &HashMap<i32, String>,
    cmds: &HashMap<i32, String>,
    roots: &[String],
    own_pid: i32,
) -> Vec<ViteProc> {
    let mut v = Vec::new();
    for (pid, ps) in ports {
        let (Some(cwd), true) = (cwds.get(pid), *pid != own_pid) else { continue };
        let cmd = cmds.get(pid).map(String::as_str).unwrap_or("");
        let Some(kind) = classify_listener(cmd, cwd, roots) else { continue };
        v.extend(ps.iter().map(|port| ViteProc { pid: *pid, port: *port, cwd: cwd.clone(), kind: kind.into() }));
    }
    v.sort_by_key(|p| (p.port, p.pid));
    v
}

/// 一顆已經在跑的 vite 跟這顆 bot 的關係；排序就是列出來的順序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relation {
    /// cwd 正好是這顆 bot 的候選目錄。
    SameDir,
    /// 同一個 repo 的別份 checkout／worktree。
    SameRepo,
    /// 別的專案，或判不出來。
    Other,
}

impl Relation {
    pub fn as_str(self) -> &'static str {
        match self {
            Relation::SameDir => "same_dir",
            Relation::SameRepo => "same_repo",
            Relation::Other => "other",
        }
    }
}

/// 判不出來（bot 或那顆 vite 的 repo 讀不到）退成 `Other`。
pub fn relation_of(p: &ViteProc, cands: &[PathBuf], mine: Option<&RepoKey>, theirs: Option<&RepoKey>) -> Relation {
    if cands.iter().any(|c| norm(&c.to_string_lossy()) == norm(&p.cwd)) {
        return Relation::SameDir;
    }
    match (mine, theirs) {
        (Some(a), Some(b)) if same_repo(a, b) => Relation::SameRepo,
        _ => Relation::Other,
    }
}

/// 給 UI 分組顯示的 repo 名：common dir 是 `<repo>/.git` 就取 `<repo>` 的目錄名，bare 之類取它自己的名字；
/// 不是 git 就用目錄名。
pub fn repo_name(key: Option<&RepoKey>, dir: &str) -> String {
    let base = |p: &str| Path::new(norm(p)).file_name().map(|n| n.to_string_lossy().into_owned());
    if let Some(k) = key {
        let common = Path::new(norm(&k.common));
        let named = if common.file_name().is_some_and(|n| n == ".git") { common.parent().and_then(|p| base(&p.to_string_lossy())) } else { base(&k.common) };
        if let Some(n) = named {
            return n;
        }
    }
    base(dir).unwrap_or_else(|| dir.to_string())
}

fn norm(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    if t.is_empty() { "/" } else { t }
}

/// 掃到的 vite 分兩類：cwd 正好是候選目錄的（同一份 checkout，`attach`；候選順序在前的優先），
/// 其餘全部是 `others`（別份 checkout 或別的專案：畫面上看到的不是這顆 bot 工作樹裡的程式碼，不自動接）。
pub fn classify(procs: &[ViteProc], candidates: &[PathBuf]) -> (Option<ViteProc>, Vec<ViteProc>) {
    let hit = candidates
        .iter()
        .find_map(|c| procs.iter().find(|p| norm(&p.cwd) == norm(&c.to_string_lossy())))
        .cloned();
    let others = procs.iter().filter(|p| Some(*p) != hit.as_ref()).cloned().collect();
    (hit, others)
}

/// 從 [`PORT_START`] 起往上找第一顆沒被別的預覽佔著、也沒人在 listen 的。
pub fn pick_port(taken: &HashSet<u16>, listening: &HashSet<u16>) -> Option<u16> {
    (PORT_START..PORT_START + PORT_SPAN).find(|p| !taken.contains(p) && !listening.contains(p))
}

/// `package.json` 的 `scripts.dev`（非空字串）；沒有或壞掉＝`None`。
pub fn parse_dev_script(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let dev = v.get("scripts")?.get("dev")?.as_str()?.trim();
    (!dev.is_empty()).then(|| dev.to_string())
}

fn read_dev_script(dir: &Path) -> Option<String> {
    parse_dev_script(&std::fs::read_to_string(dir.join("package.json")).ok()?)
}

/// 實際要跑的那一行：目錄有 `dev` script 就 `bun run dev`（不硬塞 port，起來之後觀察它實際 listen 的 port），
/// 沒有才退回 `bunx vite`（port 由 daemon 挑好）。
pub fn run_command(dev_script: Option<&str>, allow_lan: bool, port: u16) -> String {
    match dev_script {
        Some(_) => "bun run dev".to_string(),
        None => command(allow_lan, port),
    }
}

/// `allow_lan` 開著（dev 的區網／Tailscale 存取）才綁全部介面。
pub fn command(allow_lan: bool, port: u16) -> String {
    let bind = if allow_lan { "0.0.0.0" } else { "127.0.0.1" };
    format!("bunx vite --host {bind} --port {port} --strictPort")
}

/// 一次觀察到的事實。`pane_alive` 為 `None`＝herdr 沒回答，當作還在。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub pane_alive: Option<bool>,
    pub listening: bool,
}

/// 轉移結果；`Failed` 帶原因（呼叫端再補 pane 尾巴）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Stay,
    To(Status, Option<&'static str>),
}

/// `off`／`failed` 是靜止的；`starting` 等 port 出現（pane 沒了或逾時＝失敗）；`running` 在 pane 被關時回 `off`
/// （使用者自己關的，不是錯），port 不見但 pane 還在＝vite 掛了，`failed`。
pub fn next_status(cur: Status, attached: bool, obs: Observed, elapsed_secs: i64) -> Next {
    let gone = obs.pane_alive == Some(false);
    match cur {
        Status::Off | Status::Failed => Next::Stay,
        Status::Starting if obs.listening => Next::To(Status::Running, None),
        Status::Starting if gone => Next::To(Status::Failed, Some("pane 在 vite 起來之前就被關了")),
        Status::Starting if elapsed_secs >= START_TIMEOUT_SECS => {
            Next::To(Status::Failed, Some("60 秒內沒有偵測到 listen 的 port"))
        }
        Status::Starting => Next::Stay,
        Status::Running if gone => Next::To(Status::Off, None),
        // 接上的是別人的 server：它結束了就是斷開，不算我們的失敗。
        Status::Running if attached && !obs.listening => Next::To(Status::Off, None),
        Status::Running if !obs.listening => Next::To(Status::Failed, Some("vite 已經停了（port 不再 listen）")),
        Status::Running => Next::Stay,
    }
}

// ───────────────────────────── DB ─────────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Row {
    pub bot_id: String,
    pub host: String,
    pub pane_id: Option<String>,
    pub port: Option<i64>,
    pub dir: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: String,
    /// `spawned`（AG Man 起的，有 pane）或 `attached`（接上既有的 vite，沒有 pane；只斷開、不殺）。
    pub source: String,
    pub pid: Option<i64>,
    /// 實際跑的那一行（`attached` 是 `None`：那是別人開的）。
    pub command: Option<String>,
    /// dev server 種類（`vite`／`next`／…／`unknown`）；`spawned` 由命令決定。
    pub kind: Option<String>,
}

impl Row {
    fn status(&self) -> Status {
        Status::parse(&self.status)
    }
    fn attached(&self) -> bool {
        self.source == SOURCE_ATTACHED
    }
    /// `GET/POST/DELETE` 的回應形狀。
    pub fn body(&self) -> Value {
        if self.status() == Status::Off {
            return off_body();
        }
        json!({
            "status": self.status,
            "port": self.port,
            "dir": self.dir,
            "pane_id": self.pane_id,
            "error": self.error,
            "started_at": self.started_at,
            "source": self.source,
            "pid": self.pid,
            "command": self.command,
            "kind": self.kind,
        })
    }
}

fn off_body() -> Value {
    json!({"status": "off"})
}

pub async fn row(pool: &SqlitePool, bot_id: &str) -> anyhow::Result<Option<Row>> {
    Ok(sqlx::query_as::<_, Row>("SELECT * FROM bot_previews WHERE bot_id = ?").bind(bot_id).fetch_optional(pool).await?)
}

async fn live_rows(pool: &SqlitePool) -> anyhow::Result<Vec<Row>> {
    Ok(sqlx::query_as::<_, Row>("SELECT * FROM bot_previews WHERE status IN ('starting','running')").fetch_all(pool).await?)
}

async fn taken_ports(pool: &SqlitePool) -> anyhow::Result<HashSet<u16>> {
    Ok(live_rows(pool).await?.into_iter().filter_map(|r| r.port).filter_map(|p| u16::try_from(p).ok()).collect())
}

async fn put(pool: &SqlitePool, r: &Row) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO bot_previews (bot_id, host, pane_id, port, dir, status, error, started_at, updated_at, source, pid, command, kind)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)
         ON CONFLICT(bot_id) DO UPDATE SET host=excluded.host, pane_id=excluded.pane_id, port=excluded.port,
           dir=excluded.dir, status=excluded.status, error=excluded.error, started_at=excluded.started_at,
           updated_at=excluded.updated_at, source=excluded.source, pid=excluded.pid,
           command=excluded.command, kind=excluded.kind",
    )
    .bind(&r.bot_id)
    .bind(&r.host)
    .bind(&r.pane_id)
    .bind(r.port)
    .bind(&r.dir)
    .bind(&r.status)
    .bind(&r.error)
    .bind(&r.started_at)
    .bind(&r.updated_at)
    .bind(&r.source)
    .bind(r.pid)
    .bind(&r.command)
    .bind(&r.kind)
    .execute(pool)
    .await?;
    Ok(())
}

/// `/api/state` 每顆 bot 的 `preview`：`{"status","port"}`，沒開過或 `off` 是 `null`。
pub async fn state_map(pool: &SqlitePool) -> anyhow::Result<HashMap<String, Value>> {
    let rows = sqlx::query_as::<_, Row>("SELECT * FROM bot_previews WHERE status != 'off'").fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| (r.bot_id.clone(), json!({"status": r.status, "port": r.port, "source": r.source}))).collect())
}

async fn emit_changed(app: &Arc<App>, r: &Row) {
    app.emit("preview_changed", json!({"bot_id": r.bot_id, "status": r.status, "port": r.port, "source": r.source})).await;
}

// ───────────────────────────── 入口 ─────────────────────────────

/// 預覽操作的全域鎖：見模組說明。
fn gate() -> &'static tokio::sync::Mutex<()> {
    static G: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    G.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

fn elapsed_secs(started_at: Option<&str>) -> i64 {
    started_at
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(0)
}

async fn top_level_bot(app: &Arc<App>, bot_id: &str) -> LcResult<db::Bot> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    Ok(bot)
}

/// 這顆 bot 的 vite 候選目錄（[`detect_dirs`] 對真的檔案系統）。
/// 這顆 bot 的工作目錄：`bots.cwd`，沒有就是專案路徑。找 vite 與判 repo 都從這裡出發。
async fn base_dir(app: &Arc<App>, bot: &db::Bot) -> LcResult<String> {
    let project = db::project(&app.db, &bot.project_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    Ok(bot.cwd.clone().filter(|c| !c.trim().is_empty()).unwrap_or(project.path))
}

async fn candidates_of(app: &Arc<App>, bot: &db::Bot) -> LcResult<Result<Vec<PathBuf>, Vec<String>>> {
    let cwd = base_dir(app, bot).await?;
    // 不跟 symlink：免得繞圈。
    let subdirs = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .map(|it| it.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect())
            .unwrap_or_default()
    };
    Ok(detect_dirs(Path::new(&cwd), |p| p.is_file(), subdirs, |d| read_dev_script(d).is_some()))
}

/// AG Man 認得的本機專案路徑：行程 cwd 落在其中一個底下就算「跟專案有關」。
async fn local_project_roots(app: &Arc<App>) -> Vec<String> {
    db::live_projects(&app.db)
        .await
        .map(|ps| ps.into_iter().filter(|p| p.host == crate::config::LOCAL_HOST).map(|p| p.path).collect())
        .unwrap_or_default()
}

/// 回應在 [`Row::body`] 之外多幾欄：`candidates`（可以起 dev server 的目錄）與 `candidate_info`（各自會跑的那一行）、
/// 沒在用的狀態下另有 `command`（預設候選會跑的那一行）與 `others`（本機所有 dev server，各帶 `kind`／`relation`／`repo`）。
async fn decorated(app: &Arc<App>, bot: &db::Bot, mut body: Value, live: bool) -> LcResult<Value> {
    let cands = candidates_of(app, bot).await?.unwrap_or_default();
    let mut others: Vec<(Relation, String, ViteProc)> = Vec::new();
    let env = match db::active_run(&app.db, &bot.id).await {
        Ok(Some(run)) => env_for(app, &run).await.unwrap_or_else(|_| fallback_env(app)),
        _ => fallback_env(app),
    };
    // 各候選會跑的那一行；退回 `bunx vite` 的要先知道會挑到哪個 port。
    let devs: Vec<Option<String>> = cands.iter().map(|c| read_dev_script(c)).collect();
    let mut port = PORT_START;
    if devs.iter().any(Option::is_none) {
        let taken = taken_ports(&app.db).await.unwrap_or_default();
        port = pick_port(&taken, &listening_window(env.as_ref()).await).unwrap_or(PORT_START);
    }
    let info: Vec<Value> = cands
        .iter()
        .zip(&devs)
        .map(|(c, d)| json!({"dir": c.to_string_lossy(), "command": run_command(d.as_deref(), app.allow_lan, port)}))
        .collect();
    if !live {
        let base = base_dir(app, bot).await?;
        // 掃不到就當沒有：這只是「順便列出來」，不是決定。
        let scan = env.scan_servers(&local_project_roots(app).await).await.unwrap_or_default();
        // 這顆 bot 屬於哪個 repo、什麼算「同一個目錄」都看它自己的工作目錄（專案路徑），跟找沒找到設定檔無關。
        let mine = env.repo_key(&base).await;
        let mut same_dirs = cands.clone();
        same_dirs.push(PathBuf::from(&base));
        for p in scan {
            let key = env.repo_key(&p.cwd).await;
            let relation = relation_of(&p, &same_dirs, mine.as_ref(), key.as_ref());
            let repo = repo_name(key.as_ref(), &p.cwd);
            others.push((relation, repo, p));
        }
        others.sort_by_key(|(r, _, p)| (*r, p.port));
    }
    if let Some(o) = body.as_object_mut() {
        o.insert("candidates".into(), json!(cands.iter().map(|c| c.to_string_lossy()).collect::<Vec<_>>()));
        if !live {
            o.insert("command".into(), info.first().map(|i| i["command"].clone()).unwrap_or(Value::Null));
        }
        o.insert("candidate_info".into(), json!(info));
        o.insert("others".into(), json!(others
            .iter()
            .map(|(rel, repo, p)| json!({"port": p.port, "dir": p.cwd, "pid": p.pid, "kind": p.kind, "relation": rel.as_str(), "repo": repo}))
            .collect::<Vec<_>>()));
    }
    Ok(body)
}

/// `GET`：先對一次帳（pane 還在嗎、port 還在 listen 嗎），再回。
pub async fn get(app: &Arc<App>, bot_id: &str) -> LcResult<Value> {
    let bot = top_level_bot(app, bot_id).await?;
    let _g = gate().lock().await;
    let r = refresh_locked(app, bot_id).await;
    let live = r.as_ref().is_some_and(|r| r.status().is_live());
    // 舊 daemon 留下的、或監看掛掉的列：GET 補上（已經有就不會多一個）。
    #[cfg(not(test))]
    if live {
        spawn_watcher(app.clone(), bot_id.to_string());
    }
    decorated(app, &bot, r.map(|r| r.body()).unwrap_or_else(off_body), live).await
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct StartReq {
    /// `auto`（預設：同目錄有在跑的就接、沒有就起）／`attach`（要帶 `port`）／`spawn`。
    pub mode: Option<String>,
    pub port: Option<u16>,
    pub dir: Option<String>,
    /// `mode=attach`：使用者挑的那顆的 pid。帶了就要對得上，否則 port 已經換人了（409 `stale_selection`）。
    pub pid: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Attach,
    Spawn,
}

/// `POST`：冪等啟動。
pub async fn start(app: &Arc<App>, bot_id: &str, req: StartReq) -> LcResult<Value> {
    let bot = top_level_bot(app, bot_id).await?;
    if bot.parent_bot_id.is_some() || bot.managed_by != "user" {
        return Err(LcError::conflict("not_top_level", json!({"bot_id": bot_id})));
    }
    crate::lifecycle::refuse_default_session(&bot)?;
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    if host != crate::config::LOCAL_HOST {
        // iframe 連的是瀏覽器所在那台的 port；遠端主機上的 vite 連不到。
        return Err(LcError::conflict("remote_host", json!({"bot_id": bot_id, "host": host})));
    }
    let mode = match req.mode.as_deref().unwrap_or("auto") {
        "auto" => Mode::Auto,
        "attach" => Mode::Attach,
        "spawn" => Mode::Spawn,
        other => return Err(LcError::Bad(format!("mode 只能是 auto／attach／spawn，收到 `{other}`"))),
    };
    if mode == Mode::Attach && req.port.is_none() {
        return Err(LcError::Bad("mode=attach 必須帶 port".into()));
    }
    let explicit = mode != Mode::Auto || req.port.is_some() || req.dir.is_some();
    let _g = gate().lock().await;
    // 已經在跑（或在起）而且對得上帳：原樣回；明確指定了別的（換目錄、換接哪顆）才先斷開再來。
    if let Some(r) = refresh_locked(app, bot_id).await {
        if r.status().is_live() {
            if !explicit {
                #[cfg(not(test))]
                spawn_watcher(app.clone(), bot_id.to_string());
                return decorated(app, &bot, r.body(), true).await;
            }
            match disconnect_locked(app, r).await {
                Some(stopped) if !stopped.status().is_live() => {}
                _ => {
                    // 明確切換不能把舊 pane 關掉時，不能繼續起第二顆，否則會留下孤兒 server。
                    return Err(LcError::conflict("preview_stop_failed", json!({"bot_id": bot_id})));
                }
            }
        }
    }
    let cands = candidates_of(app, &bot).await?;
    let no_config = |tried: Vec<String>| LcError::conflict("no_vite_config", json!({"bot_id": bot_id, "tried": tried}));
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?;
    let env = match &run {
        Some(run) => env_for(app, run).await?,
        None => fallback_env(app),
    };
    // 掃不到只影響「自動接」，不擋 spawn；明確要求 attach 時掃不到就是失敗。
    let roots = local_project_roots(app).await;
    let scan = env.scan_servers(&roots).await;

    let attach_to: Option<ViteProc> = match mode {
        Mode::Attach => {
            let port = req.port.unwrap_or_default();
            let hit = scan.as_deref().unwrap_or_default().iter().find(|p| p.port == port).cloned();
            let hit = hit.ok_or_else(|| LcError::conflict("not_vite", json!({"bot_id": bot_id, "port": port})))?;
            // 使用者挑的是清單上「那一顆」：GET 到 POST 之間 port 被別的行程接手時，失敗，不默默接到別人。
            if req.pid.is_some_and(|pid| pid != hit.pid) || req.dir.as_deref().is_some_and(|d| norm(d) != norm(&hit.cwd)) {
                return Err(LcError::conflict(
                    "stale_selection",
                    json!({"bot_id": bot_id, "port": port, "pid": hit.pid, "dir": hit.cwd}),
                ));
            }
            Some(hit)
        }
        Mode::Auto => {
            let all = cands.clone().unwrap_or_default();
            let pool = match req.dir.as_deref() {
                Some(d) => vec![all.iter().find(|c| norm(&c.to_string_lossy()) == norm(d)).cloned().ok_or_else(|| {
                    LcError::Bad(format!("dir `{d}` 不是這顆 bot 的 dev server 候選目錄"))
                })?],
                // 候選目錄之外，bot 自己的工作目錄也算「同一個目錄」（那顆 server 可能沒有 vite 設定，例如 next）。
                None => all.into_iter().chain([PathBuf::from(base_dir(app, &bot).await?)]).collect(),
            };
            classify(scan.as_deref().unwrap_or_default(), &pool).0
        }
        Mode::Spawn => None,
    };
    if let Some(p) = attach_to {
        let now = db::now();
        let r = Row {
            bot_id: bot_id.to_string(),
            host,
            pane_id: None,
            port: Some(p.port as i64),
            dir: Some(p.cwd),
            status: Status::Running.as_str().into(),
            error: None,
            started_at: Some(now.clone()),
            updated_at: now,
            source: SOURCE_ATTACHED.into(),
            pid: Some(p.pid as i64),
            command: None,
            kind: Some(p.kind),
        };
        put(&app.db, &r).await.map_err(up)?;
        emit_changed(app, &r).await;
        #[cfg(not(test))]
        spawn_watcher(app.clone(), bot_id.to_string());
        return decorated(app, &bot, r.body(), true).await;
    }

    let cands = cands.map_err(no_config)?;
    let dir = match req.dir.as_deref() {
        Some(d) => cands
            .iter()
            .find(|c| norm(&c.to_string_lossy()) == norm(d))
            .cloned()
            .ok_or_else(|| LcError::Bad(format!("dir `{d}` 不是這顆 bot 的 dev server 候選目錄")))?,
        None => cands[0].clone(),
    }
    .to_string_lossy()
    .into_owned();
    let Some(run) = run.filter(|r| r.state == "running" && r.pane_id.is_some()) else {
        return Err(LcError::conflict("bot_not_running", json!({"bot_id": bot_id})));
    };
    // 有 dev script 就用它，不硬塞 port（起來之後觀察實際 listen 的 port）；沒有才退回 `bunx vite` 並由 daemon 挑 port。
    let dev = read_dev_script(Path::new(&dir));
    let port = if dev.is_some() {
        None
    } else {
        let taken = taken_ports(&app.db).await.map_err(up)?;
        let listening = listening_window(env.as_ref()).await;
        Some(
            pick_port(&taken, &listening)
                .ok_or_else(|| LcError::conflict("no_free_port", json!({"from": PORT_START, "span": PORT_SPAN})))?,
        )
    };
    let cmd = run_command(dev.as_deref(), app.allow_lan, port.unwrap_or(PORT_START));
    let pane = run.pane_id.clone().unwrap_or_default();
    let pane_id = env.spawn(&pane, &dir, &cmd).await.map_err(up)?;
    let now = db::now();
    let r = Row {
        bot_id: bot_id.to_string(),
        host,
        pane_id: Some(pane_id.clone()),
        port: port.map(i64::from),
        dir: Some(dir),
        status: Status::Starting.as_str().into(),
        error: None,
        started_at: Some(now.clone()),
        updated_at: now,
        source: SOURCE_SPAWNED.into(),
        pid: None,
        kind: Some(dev_kind(&cmd).unwrap_or(if dev.is_some() { "unknown" } else { "vite" }).into()),
        command: Some(cmd),
    };
    if let Err(e) = put(&app.db, &r).await {
        // 記不下來就不留孤兒 pane。
        env.close_pane(&pane_id).await;
        return Err(up(e));
    }
    emit_changed(app, &r).await;
    // 測試自己決定什麼時候看（不然背景那一拍會跟測試的手動轉移搶）；watcher 另有測試。
    #[cfg(not(test))]
    spawn_watcher(app.clone(), bot_id.to_string());
    decorated(app, &bot, r.body(), true).await
}

/// 斷開一列：`spawned` 關 pane，`attached` 什麼都不動（那顆 vite 是別人的），然後標 `off`。要在 [`gate`] 裡呼叫。
async fn disconnect_locked(app: &Arc<App>, r: Row) -> Option<Row> {
    if !close_row_pane(app, &r).await {
        // 不知道那顆 pane 屬於哪個 session（讀 run 失敗）：不關、也不標 off，留著下次再看。
        tracing::warn!(bot = %r.bot_id, "preview: cannot tell which herdr session owns the pane; leaving the preview as is");
        return Some(r);
    }
    let off = Row { pane_id: None, status: Status::Off.as_str().into(), error: None, updated_at: db::now(), ..r };
    match put(&app.db, &off).await {
        Ok(()) => {
            emit_changed(app, &off).await;
            Some(off)
        }
        Err(e) => {
            tracing::warn!(bot = %off.bot_id, error = %e, "preview: cannot mark the preview off");
            None
        }
    }
}

/// `DELETE`：關 pane、放掉 port。
pub async fn stop(app: &Arc<App>, bot_id: &str) -> LcResult<Value> {
    top_level_bot(app, bot_id).await?;
    stop_for_bot(app, bot_id).await;
    Ok(off_body())
}

/// bot 被停止／刪除／閒置收掉時一起收預覽。盡力而為：讀不到就記 log，不擋 bot 的停機。
pub async fn stop_for_bot(app: &Arc<App>, bot_id: &str) {
    let _g = gate().lock().await;
    let r = match row(&app.db, bot_id).await {
        Ok(Some(r)) if r.status() != Status::Off => r,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(bot = bot_id, error = %e, "preview: cannot read the preview row while stopping the bot");
            return;
        }
    };
    disconnect_locked(app, r).await;
}

/// 關這一列的 pane；回 `false`＝**沒關掉也沒確認它不在**（讀不到 run、拿不到那個 session 的 client、關指令失敗），呼叫端不可標 off。
/// 讀 run 失敗（`Err`）不等於「bot 已停」：`Ok(None)` 才是確定沒有 run，才退回管理 session。
async fn close_row_pane(app: &Arc<App>, r: &Row) -> bool {
    // 接上的那顆是別人開的 server：只斷開，絕不動它。
    if r.attached() {
        return true;
    }
    let Some(pane) = r.pane_id.as_deref() else { return true };
    let env = match db::active_run(&app.db, &r.bot_id).await {
        Ok(Some(run)) => env_for(app, &run).await.ok(),
        Ok(None) => env_for_bot(app, &r.bot_id).await,
        Err(e) => {
            tracing::warn!(bot = %r.bot_id, error = %e, "preview: cannot read the active run while closing the preview pane");
            return false;
        }
    };
    let Some(env) = env else {
        tracing::warn!(bot = %r.bot_id, "preview: no herdr session available for the pane's owner; not closing through another session");
        return false;
    };
    env.close_pane(pane).await
}

/// 開機對帳：`starting`／`running` 的列拿「pane 還在不在」＋「port 有沒有在 listen」對回去；還在 `starting` 的補上 watcher。
pub async fn reconcile_all(app: &Arc<App>) {
    let rows = match live_rows(&app.db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "preview: cannot read bot_previews at startup");
            return;
        }
    };
    for r in rows {
        let _g = gate().lock().await;
        if let Some(now) = refresh_locked(app, &r.bot_id).await {
            if now.status().is_live() {
                spawn_watcher(app.clone(), r.bot_id.clone());
            }
        }
    }
}

// ───────────────────────────── 對帳 ─────────────────────────────

/// 一次觀察＋轉移＋寫回＋通知；回最新的列（沒有這列回 `None`）。要在 [`gate`] 裡呼叫。
async fn refresh_locked(app: &Arc<App>, bot_id: &str) -> Option<Row> {
    let r = match row(&app.db, bot_id).await {
        Ok(r) => r?,
        Err(e) => {
            tracing::warn!(bot = bot_id, error = %e, "preview: cannot read the preview row");
            return None;
        }
    };
    if !r.status().is_live() {
        return Some(r);
    }
    let attached = r.attached();
    // 讀不到 run（`Err`）不是「沒有 run」：那是不知道，什麼都不動、下次再看（關 pane 是破壞性的）。
    let run = match db::active_run(&app.db, bot_id).await {
        Ok(run) => run,
        Err(e) if !attached => {
            tracing::warn!(bot = bot_id, error = %e, "preview: cannot read the active run; leaving the preview as is");
            return Some(r);
        }
        Err(_) => None,
    };
    // bot 自己掛了或被停了：預覽跟著收，不留一顆沒人管的 vite。接上的只看它自己的 port，不看 bot。
    if run.is_none() && !attached {
        return disconnect_locked(app, r).await;
    }
    let env = match (&run, attached) {
        (Some(run), false) => match env_for(app, run).await {
            Ok(env) => env,
            Err(e) => {
                // 暫時拿不到 herdr：不知道，列原樣留著、下次再看（不是「沒有這列」，watcher 不可因此結束）。
                tracing::warn!(bot = bot_id, error = ?e, "preview: no herdr session for the run; leaving the preview as is");
                return Some(r);
            }
        },
        _ => fallback_env(app),
    };
    let mut port = r.port;
    // 接上的 server 換了行程（同 port 被別的行程接手）：見 [`attached_identity`]。
    let mut rebind: Option<i64> = None;
    let (pane_alive, listening) = match (&r.pane_id, r.port) {
        (Some(p), Some(port)) => (env.pane_alive(p).await, env.port_listening(port as u16).await),
        // dev script 起的 server 自己挑 port：看這顆 pane 的行程樹實際 listen 到哪個（多個取最小的）。
        (Some(p), None) if !attached => {
            let alive = env.pane_alive(p).await;
            let ports = env.pane_ports(p).await;
            // pane_ports 的 None 是「問不到」，不能當成空集合讓 starting 超時失敗；pane_alive 已知關閉仍照常失敗。
            if ports.is_none() && alive != Some(false) {
                return Some(r);
            }
            port = ports.unwrap_or_default().first().map(|x| i64::from(*x));
            (alive, port.is_some())
        }
        (None, Some(port)) if attached => {
            let mut up = env.port_listening(port as u16).await;
            if up && r.status() == Status::Running {
                match attached_identity(app, env.as_ref(), &r, port as u16).await {
                    Identity::Lost => up = false,
                    Identity::Rebound(pid) => rebind = Some(pid),
                    Identity::Same | Identity::Unknown => {}
                }
            }
            (None, up)
        }
        (_, Some(port)) => (Some(false), env.port_listening(port as u16).await),
        _ => (Some(false), false),
    };
    let next = next_status(r.status(), attached, Observed { pane_alive, listening }, elapsed_secs(r.started_at.as_deref()));
    let Next::To(to, why) = next else {
        let Some(pid) = rebind else { return Some(r) };
        let updated = Row { pid: Some(pid), updated_at: db::now(), ..r.clone() };
        return match put(&app.db, &updated).await {
            Ok(()) => {
                emit_changed(app, &updated).await;
                Some(updated)
            }
            Err(e) => {
                tracing::warn!(bot = bot_id, error = %e, "preview: cannot record the re-attached pid");
                Some(r)
            }
        };
    };
    let error = match (to, why) {
        (Status::Failed, Some(why)) => {
            let tail = match &r.pane_id {
                Some(p) if pane_alive != Some(false) => env.pane_tail(p).await,
                _ => String::new(),
            };
            Some(if tail.trim().is_empty() { why.to_string() } else { format!("{why}\n{}", tail.trim_end()) })
        }
        _ => None,
    };
    if to == Status::Off && !close_row_pane(app, &r).await {
        return Some(r);
    }
    let pane_id = if to == Status::Off { None } else { r.pane_id.clone() };
    let updated = Row { status: to.as_str().into(), error, pane_id, port, updated_at: db::now(), ..r.clone() };
    match put(&app.db, &updated).await {
        Ok(()) => {
            emit_changed(app, &updated).await;
            Some(updated)
        }
        Err(e) => {
            tracing::warn!(bot = bot_id, error = %e, "preview: cannot record the new preview status");
            Some(r)
        }
    }
}

/// 接上的 server 還是不是當初接的那一顆。
enum Identity {
    Same,
    /// 同 port、同 cwd、仍是 dev server，只是 pid 變了（同一個 app 重啟）：沿用這一列，換記新 pid。
    Rebound(i64),
    /// port 現在是別的行程：不是 dev server，或 cwd 不同。不可默默跟著它，轉 `off`。
    Lost,
    /// 問不到（掃描失敗、列上沒有記 pid／dir）：當「沒變」。
    Unknown,
}

async fn attached_identity(app: &Arc<App>, env: &dyn PreviewEnv, r: &Row, port: u16) -> Identity {
    let (Some(pid), Some(dir)) = (r.pid, r.dir.as_deref()) else { return Identity::Unknown };
    let Some(all) = env.scan_servers(&local_project_roots(app).await).await else { return Identity::Unknown };
    match all.iter().find(|p| p.port == port) {
        None => Identity::Lost,
        Some(p) if i64::from(p.pid) == pid => Identity::Same,
        Some(p) if norm(&p.cwd) == norm(dir) => Identity::Rebound(i64::from(p.pid)),
        Some(_) => Identity::Lost,
    }
}

/// 已經有監看的 bot（一顆 bot 只有一個）。登記與註銷都在 [`gate`] 裡做：結束的那一拍決定「不用看了」到註銷之間
/// 沒有人能插進來開新預覽，新預覽的 `spawn_watcher` 不會被舊的登記擋掉。
fn watched() -> &'static std::sync::Mutex<HashSet<String>> {
    static W: OnceLock<std::sync::Mutex<HashSet<String>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// 這顆 bot 的預覽現在有沒有人監看（測試用）。
#[cfg(test)]
fn is_watched(bot_id: &str) -> bool {
    watched().lock().unwrap().contains(bot_id)
}

/// 監看一顆 `starting`／`running` 的預覽直到它離開這兩個狀態（`off`／`failed`／這列沒了）：`starting` 每秒、`running` 每 5 秒對一次帳
/// （pane 還在嗎、port 還在 listen 嗎），server 半路掛掉不必等人 GET 才發現、前端也收得到 `preview_changed`。
/// 同一顆 bot 只有一個；已經有就回 `false`。要在 [`gate`] 裡呼叫。
fn spawn_watcher(app: Arc<App>, bot_id: String) -> bool {
    if !watched().lock().unwrap().insert(bot_id.clone()) {
        return false;
    }
    tokio::spawn(async move {
        struct Unwatch(Option<String>);
        impl Drop for Unwatch {
            fn drop(&mut self) {
                if let Some(id) = &self.0 {
                    watched().lock().unwrap().remove(id);
                }
            }
        }
        // 只在 panic 之類沒走到下面明確註銷的情況兜底；正常結束在 gate 裡註銷後解除，免得誤刪之後才登記的新監看。
        let mut guard = Unwatch(Some(bot_id.clone()));
        let mut wait = POLL;
        loop {
            tokio::time::sleep(wait).await;
            let _g = gate().lock().await;
            #[cfg(test)]
            tick(&bot_id);
            // 只讀**一次**決定要不要結束：以前是 refresh 讀不到之後再讀一次確認，兩次之間 DB 恢復了就把暫時性的錯誤誤判成
            // 「這列沒了」、監看無聲結束（整套平行跑時偶發，`the_starting_watcher_survives_an_unreadable_preview_row`）。
            let next = match row(&app.db, &bot_id).await {
                // 讀不到這一列（暫時性）不是「這列沒了」：只有確定沒有這一列、或它已經不是在監看的狀態才結束。
                Err(_) => Some(Status::Starting),
                Ok(None) => None,
                Ok(Some(r)) if !r.status().is_live() => None,
                Ok(Some(seen)) => match refresh_locked(&app, &bot_id).await {
                    Some(r) if r.status().is_live() => Some(r.status()),
                    Some(_) => None,
                    // 剛看到它是活的：refresh 讀不到或寫不進去（暫時性），下一輪再看；這列不會消失。
                    None => Some(seen.status()),
                },
            };
            match next {
                Some(Status::Running) => wait = RUNNING_POLL,
                Some(_) => wait = POLL,
                None => {
                    watched().lock().unwrap().remove(&bot_id);
                    guard.0 = None;
                    return;
                }
            }
        }
    });
    true
}

/// 測試用：監看迴圈每一輪記一次，讓測試等「它真的輪過幾次」而不是睡一段固定時間。
#[cfg(test)]
fn ticks() -> &'static std::sync::Mutex<HashMap<String, u64>> {
    static T: OnceLock<std::sync::Mutex<HashMap<String, u64>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

#[cfg(test)]
fn tick(bot_id: &str) {
    *ticks().lock().unwrap().entry(bot_id.to_string()).or_default() += 1;
}

#[cfg(test)]
pub(crate) fn watcher_ticks(bot_id: &str) -> u64 {
    ticks().lock().unwrap().get(bot_id).copied().unwrap_or(0)
}

/// 挑 port 前一次問完整個窗口。
async fn listening_window(env: &dyn PreviewEnv) -> HashSet<u16> {
    let checks = (PORT_START..PORT_START + PORT_SPAN).map(|p| async move { (p, env.port_listening(p).await) });
    futures::future::join_all(checks).await.into_iter().filter(|(_, l)| *l).map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests;
