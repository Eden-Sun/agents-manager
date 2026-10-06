//! 左上角「立即部署」（使用者 2026-09-25；2026-09-29 隨例行部署簡化）。
//!
//! 例行更新是 systemd timer／launchd 每 5 分鐘跑 `daemon-update-kick.sh`，腳本自己找最新一顆
//! `ubuntu-ci` 綠燈的 main commit、建置、換版，不經 LLM、不要核准（SPEC §18.2）。這裡只做
//! 「使用者按下去」那一段，**不另寫一套 build＋swap**：
//!
//! 1. `GET /api/deploy/status`：線上 binary（`build_info::BUILD_SHA`）落後 `origin/main` 幾個 commit、
//!    有沒有會進 binary 的差異（路徑同 kick 的 `build-inputs`），以及現在有沒有部署在跑。
//! 2. `POST /api/deploy/now`：寫 `supervisor/AGM/daemon-update.now.json`（要部署的 sha），再
//!    `launchctl kickstart` 同一個 launchd job（Linux 是 `systemctl --user start --no-block` 同一個
//!    systemd unit，issue #677）。kick 看到這個檔就直接部署那顆 sha，不等 ubuntu-ci（使用者按下就是裁示），
//!    做完刪檔；沒人 working／送達中才換版等安全條件照舊。
//!
//! 為什麼走排程器而不是 daemon 自己 fork 一支 kick：kick 的 `PATH` 只寫在 plist 的
//! `EnvironmentVariables`（unit 的 `Environment=`），daemon 這邊拿不到；而且 launchd／systemd 都保證同一個
//! job 同時只有一個，跟排程那一輪不會疊。kick 正在跑的那一刻 kickstart／start 不會再起一個，請求檔留著，
//! 下一輪（最多 5 分鐘）就吃到。
use crate::lifecycle::LcError;
use crate::state::App;
use crate::supervisor::store;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 使用者的請求：kick 讀到就走「立即」模式，部署做完（或那顆 sha 已經不能部署）才刪。
pub const REQUEST_FILE: &str = "daemon-update.now.json";
pub const LOG_FILE: &str = "daemon-update.log";
pub const KICK_SCRIPT: &str = "bin/daemon-update-kick.sh";
pub const LAUNCHD_LABEL: &str = "com.agm.daemon-update";
/// Linux 上同一個 job 的 systemd user unit（`scripts/ops/systemd/`，issue #677）。
pub const SYSTEMD_UNIT: &str = "com.agm.daemon-update.service";
/// 確認框最多列幾個 commit；落後更多時只列最新的這幾個，總數照實寫。
const COMMIT_LIST_MAX: usize = 30;
/// `GET /status` 最多多久 fetch 一次（背景跑，不擋回應）。
const FETCH_EVERY: Duration = Duration::from_secs(300);
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// 使用者按下之後由誰把 kick 叫起來。正式是 [`SchedulerKick::for_this_host`]；測試換成假的，不碰 launchd／systemd。
pub trait KickLauncher: Send + Sync {
    fn kick(&self) -> Result<(), String>;
}

/// 叫排程器「現在跑一輪」的那一條指令：macOS `launchctl kickstart`，Linux `systemctl --user start`（issue #677）。
pub struct SchedulerKick {
    program: String,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
}

impl SchedulerKick {
    pub fn for_this_host() -> Self {
        // SAFETY: getuid 沒有前置條件，也不會失敗。
        let uid = unsafe { libc::getuid() };
        if cfg!(target_os = "linux") {
            Self::systemd(uid, std::env::var_os("XDG_RUNTIME_DIR").is_some())
        } else {
            Self::launchd(uid)
        }
    }

    /// 不帶 -k：正在跑的那一輪不能被砍掉（它可能正拿著租約派工）；沒在跑就起一輪。
    fn launchd(uid: u32) -> Self {
        SchedulerKick { program: "launchctl".into(), args: vec!["kickstart".into(), format!("gui/{uid}/{LAUNCHD_LABEL}")], env: Vec::new() }
    }

    /// `--no-block`：oneshot unit 的 start 會一路等到 kick 跑完（可能好幾分鐘），這一下只要排進去就好。
    /// 正在跑的那一輪不受影響：start job 併進正在進行的那一個，不會再起一個（同 kickstart 不帶 -k）。
    /// daemon 不是從登入 session 起的（沒有 pam_systemd）就沒有 `XDG_RUNTIME_DIR`，`systemctl --user`
    /// 連不到 user bus，所以補上 `/run/user/<uid>`。
    fn systemd(uid: u32, has_runtime_dir: bool) -> Self {
        let env = if has_runtime_dir { Vec::new() } else { vec![("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))] };
        SchedulerKick {
            program: "systemctl".into(),
            args: ["--user", "start", "--no-block", SYSTEMD_UNIT].map(String::from).to_vec(),
            env,
        }
    }
}

impl KickLauncher for SchedulerKick {
    fn kick(&self) -> Result<(), String> {
        let out = std::process::Command::new(&self.program)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .map_err(|e| format!("{}: {e}", self.program))?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if err.is_empty() { format!("{} {} rc={:?}", self.program, self.args.join(" "), out.status.code()) } else { err })
        }
    }
}

/// 這一趟要用到的位置。正式從 `App` 推出來；測試直接給暫存 repo 與暫存 AGM 目錄。
pub struct Ctx {
    pub repo: PathBuf,
    pub agm_dir: PathBuf,
    pub live_sha: String,
}

impl Ctx {
    pub fn of(app: &(impl crate::capabilities::DataDir + crate::capabilities::ExePath)) -> Self {
        Ctx { repo: repo_dir(app), agm_dir: crate::supervisor::setup::agm_dir(app), live_sha: crate::build_info::BUILD_SHA.to_string() }
    }
}

/// daemon 從哪個 repo 建出來的：`<repo>/target/release/agents-managerd` 往上找第一個同時有 `.git`
/// 與 `daemon/Cargo.toml` 的目錄。找不到（binary 被搬走）才退回 kick 的預設 `~/project/agents-manager`。
pub fn repo_dir(app: &impl crate::capabilities::ExePath) -> PathBuf {
    if let Some(found) = app.exe().ancestors().find(|d| d.join(".git").exists() && d.join("daemon/Cargo.toml").is_file()) {
        return found.to_path_buf();
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("project/agents-manager")
}

async fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(repo).args(args).kill_on_drop(true);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    match tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => Ok(o),
        Ok(Err(e)) => Err(format!("git {}: {e}", args.join(" "))),
        Err(_) => Err(format!("git {} timed out", args.join(" "))),
    }
}

async fn git_text(repo: &Path, args: &[&str]) -> Result<String, String> {
    let o = git(repo, args).await?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// 一個 commit 的完整 sha；不是 commit（或不在這個 repo）回 `None`。
async fn commit_sha(repo: &Path, rev: &str) -> Option<String> {
    if rev.is_empty() || rev.starts_with('-') {
        return None;
    }
    git_text(repo, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]).await.ok().filter(|s| !s.is_empty())
}

/// `from..to` 之間有沒有會進 binary 的差異。`git diff --quiet` 的 1 是「有差」，其他非 0 是讀不到。
async fn code_changed(repo: &Path, from: &str, to: &str) -> Result<bool, String> {
    let mut args = vec!["diff", "--quiet", from, to, "--"];
    args.extend(crate::supervisor::persona::BUILD_INPUTS);
    let o = git(repo, &args).await?;
    match o.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(format!("git diff {from} {to}: {}", String::from_utf8_lossy(&o.stderr).trim())),
    }
}

/// 線上那顆落後多少。`Err` 是「說不出來」（sha 是 `unknown`、不在 repo、沒有 origin/main），
/// 不是「沒落後」——按鈕不出現，但狀態會說原因。
pub async fn behind(repo: &Path, live: &str) -> Result<Value, String> {
    if live.is_empty() || live == "unknown" {
        return Err("這顆 binary 建置時拿不到 git，說不出是哪一版".into());
    }
    let live_full = commit_sha(repo, live).await.ok_or_else(|| format!("線上的 {live} 不在 {}", repo.display()))?;
    let target = commit_sha(repo, "origin/main").await.ok_or_else(|| format!("{} 讀不到 origin/main", repo.display()))?;
    let range = format!("{live_full}..{target}");
    let count = |s: String| s.parse::<u64>().unwrap_or(0);
    let total = count(git_text(repo, &["rev-list", "--count", &range]).await?);
    let mut code_args = vec!["rev-list", "--count", &range, "--"];
    code_args.extend(crate::supervisor::persona::BUILD_INPUTS);
    let code_commits = count(git_text(repo, &code_args).await?);
    let changed = code_changed(repo, &live_full, &target).await?;
    let log = git_text(repo, &["log", &format!("-{COMMIT_LIST_MAX}"), "--format=%h%x1f%s", &range]).await?;
    let commits: Vec<Value> = log
        .lines()
        .filter_map(|l| l.split_once('\u{1f}'))
        .map(|(sha, subject)| json!({"sha": sha, "subject": subject}))
        .collect();
    Ok(json!({
        "live_sha": live,
        "live_full": live_full,
        "target_sha": target,
        "target_short": target.chars().take(8).collect::<String>(),
        "behind": total,
        "code_commits": code_commits,
        "code_changed": changed,
        "commits": commits,
        "commits_truncated": total as usize > commits.len(),
    }))
}

/// 現在有沒有部署在跑；有就回是哪一種、誰。兩種都算：
/// * 使用者上一下的請求檔還在（kick 還沒吃到、正在建置，或在等安全窗口）；
/// * 有人握著 rebuild／restart 租約（kick 正在換版，或別顆 bot 自己在重建）。
pub async fn in_progress(app: &impl crate::capabilities::Db, agm_dir: &Path) -> Result<Option<Value>, LcError> {
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    if let Some(req) = read_request(agm_dir) {
        return Ok(Some(json!({"kind": "requested", "sha": req.get("sha"), "requested_at": req.get("requested_at")})));
    }
    let now = crate::db::now();
    for resource in crate::supervisor::maintenance::RESOURCES {
        if let Some(l) = store::lease(app.db(), resource).await.map_err(up)?.filter(|l| l.held_at(&now)) {
            return Ok(Some(json!({"kind": "lease", "resource": resource, "owner": l.owner, "expires_at": l.expires_at})));
        }
    }
    Ok(None)
}

/// 請求檔還在、讀得懂才算。壞掉的檔 kick 會自己清掉並記 log，這裡不把它當成有部署在跑。
fn read_request(agm_dir: &Path) -> Option<Value> {
    let txt = std::fs::read_to_string(agm_dir.join(REQUEST_FILE)).ok()?;
    serde_json::from_str::<Value>(&txt).ok().filter(|v| v.get("sha").and_then(Value::as_str).is_some())
}

/// 正在 `working` 的 bot（確認框列出來：換 binary 要等它們，不是按了就砍）。
async fn working_bots(app: &(impl crate::capabilities::Db + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView)) -> Vec<Value> {
    match crate::supervisor::maintenance::safety(app, &[]).await {
        Ok(v) => v.get("working").and_then(Value::as_array).cloned().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 裝好的 kick 認不認得立即模式。舊版不會讀請求檔：核准開了、檔寫了，卻永遠沒人動，
/// 之後每一下都 409 `deploy_in_progress`——寧可現在就講清楚要先 install。
fn kick_ready(agm_dir: &Path) -> Result<(), Value> {
    let path = agm_dir.join(KICK_SCRIPT);
    match std::fs::read_to_string(&path) {
        Ok(s) if s.contains(REQUEST_FILE) => Ok(()),
        Ok(_) => Err(json!({"reason": "kick_outdated", "path": path,
            "message": "裝好的 daemon-update-kick.sh 還不認得「立即部署」：先照 scripts/ops/README.md install 新版"})),
        Err(e) => Err(json!({"reason": "kick_not_installed", "path": path,
            "message": format!("找不到 {}（{e}）：例行更新沒有裝，立即部署也沒有東西可以叫", path.display())})),
    }
}

static FETCHED_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// 背景 fetch，最多每 5 分鐘一次；這一次的回應用的是 fetch 之前的 origin/main。
fn maybe_fetch(repo: &Path) {
    {
        let mut at = FETCHED_AT.lock().unwrap_or_else(|p| p.into_inner());
        if at.is_some_and(|t| t.elapsed() < FETCH_EVERY) {
            return;
        }
        *at = Some(Instant::now());
    }
    let repo = repo.to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = git(&repo, &["fetch", "-q", "origin", "main"]).await.and_then(|o| {
            o.status.success().then_some(()).ok_or_else(|| String::from_utf8_lossy(&o.stderr).trim().to_string())
        }) {
            tracing::warn!(repo = %repo.display(), error = %e, "deploy status: git fetch failed; using the local origin/main");
        }
    });
}

pub async fn status(app: &(impl crate::capabilities::Db + crate::supervisor::ports::HostProbes + crate::supervisor::ports::LocalAccountView), ctx: &Ctx) -> Result<Value, LcError> {
    let diff = behind(&ctx.repo, &ctx.live_sha).await;
    let running = in_progress(app, &ctx.agm_dir).await?;
    let mut out = match diff {
        Ok(v) => v,
        Err(e) => json!({"live_sha": ctx.live_sha, "code_changed": false, "behind": 0, "commits": [], "error": e}),
    };
    out["running"] = running.unwrap_or(Value::Null);
    out["working"] = json!(working_bots(app).await);
    out["log_path"] = json!(ctx.agm_dir.join(LOG_FILE));
    out["kick_ready"] = json!(kick_ready(&ctx.agm_dir).is_ok());
    Ok(out)
}

pub async fn get_status(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let ctx = Ctx::of(&app);
    maybe_fetch(&ctx.repo);
    Ok(Json(status(&app, &ctx).await?))
}

#[derive(Deserialize, Default)]
pub struct NowIn {
    /// 確認框上顯示的那個 commit。部署的就是它，不是按下去那一刻的 origin/main
    /// （main 約 5 分鐘一個 push，使用者確認的是框裡看到的範圍）。
    #[serde(default)]
    pub sha: Option<String>,
}

/// 同一時間只受理一次（連點兩下、兩個分頁同時按）：檢查「有沒有在跑」到寫出請求檔之間不能插隊。
static START_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 只給 UI：帶任一 bot 身分標頭的一律 403，包括缺值、錯 token 或只帶一半，不得降級成使用者（#339）。
/// #556 裁示接受共用 UI token 持有者視為使用者；兩個 bot 標頭都沒帶時照此政策放行，不代表 daemon 證明了真人。
pub fn refuse_bot_caller(headers: &HeaderMap) -> Result<(), LcError> {
    if headers.contains_key("X-AM-Bot-Id") || headers.contains_key("X-AM-Bot-Token") {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "ui_only",
            "message": "立即部署只給使用者；帶 bot 身分標頭的呼叫端不接受，bot 要重建請照 SPEC §18.10 申請核准"})));
    }
    Ok(())
}

pub async fn post_now(State(app): State<Arc<App>>, headers: HeaderMap, body: Option<Json<NowIn>>) -> Result<Json<Value>, LcError> {
    refuse_bot_caller(&headers)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    Ok(Json(start(&app, &Ctx::of(&app), b.sha.as_deref(), &SchedulerKick::for_this_host()).await?))
}

pub async fn start(app: &(impl crate::capabilities::Db + crate::capabilities::Emit), ctx: &Ctx, sha: Option<&str>, launcher: &dyn KickLauncher) -> Result<Value, LcError> {
    let _g = START_LOCK.lock().await;
    if let Some(running) = in_progress(app, &ctx.agm_dir).await? {
        return Err(LcError::conflict("deploy_in_progress", json!({"running": running,
            "message": "已經有部署在跑（或在等安全窗口），這一下沒有再開一趟", "log_path": ctx.agm_dir.join(LOG_FILE)})));
    }
    kick_ready(&ctx.agm_dir).map_err(LcError::Unavailable)?;
    let diff = behind(&ctx.repo, &ctx.live_sha)
        .await
        .map_err(|e| LcError::conflict("status_unknown", json!({"message": e})))?;
    let live_full = diff["live_full"].as_str().unwrap_or_default().to_string();
    let head = diff["target_sha"].as_str().unwrap_or_default().to_string();
    let target = match sha.map(str::trim).filter(|s| !s.is_empty()) {
        None => head.clone(),
        Some(s) => commit_sha(&ctx.repo, s)
            .await
            .ok_or_else(|| LcError::conflict("unknown_commit", json!({"sha": s, "message": format!("{s} 不是這個 repo 的 commit")})))?,
    };
    // 只部署 main 上的東西：`origin/main` 的祖先（或本身）。分支上的 commit 沒經過 CI 與 review。
    let on_main = git(&ctx.repo, &["merge-base", "--is-ancestor", &target, &head]).await.map_err(LcError::Upstream)?;
    if !on_main.status.success() {
        return Err(LcError::conflict("target_not_on_main", json!({"sha": target, "origin_main": head,
            "message": "要部署的 commit 不在 origin/main 上"})));
    }
    // 舊分頁可能在 daemon 更新後才按下確認。線上版本必須是 target 的祖先或同一顆，才不會降版。
    let live_is_ancestor = git(&ctx.repo, &["merge-base", "--is-ancestor", &live_full, &target]).await.map_err(LcError::Upstream)?;
    match live_is_ancestor.status.code() {
        Some(0) => {}
        Some(1) => {
            return Err(LcError::conflict("target_older_than_live", json!({"sha": target, "live_sha": ctx.live_sha,
                "message": format!("線上版本 {live_full} 不是要部署的 {target} 的祖先或同一顆，無法保證不是降版；請重新載入後確認。")})));
        }
        _ => {
            return Err(LcError::Upstream(format!("git merge-base --is-ancestor {live_full} {target}: {}",
                String::from_utf8_lossy(&live_is_ancestor.stderr).trim())));
        }
    }
    if !code_changed(&ctx.repo, &live_full, &target).await.map_err(LcError::Upstream)? {
        return Err(LcError::conflict("nothing_to_deploy", json!({"sha": target, "live_sha": ctx.live_sha,
            "message": "線上那顆到這個 commit 之間只動到不進 binary 的檔，不用重建"})));
    }
    let short: String = target.chars().take(8).collect();
    let request = json!({"sha": target, "live_sha": ctx.live_sha, "requested_at": crate::db::now(), "requested_by": "ui"});
    if let Err(e) = write_request(&ctx.agm_dir, &request) {
        return Err(LcError::Unavailable(json!({"reason": "request_write_failed", "message": e, "retryable": true})));
    }
    if let Err(e) = launcher.kick() {
        undo(&ctx.agm_dir, &format!("叫不起 {LAUNCHD_LABEL}：{e}"));
        return Err(LcError::Unavailable(json!({"reason": "kick_start_failed", "message": format!("叫不起例行更新（{LAUNCHD_LABEL}）：{e}"),
            "retryable": true})));
    }
    let _ = store::add_note(app.db(), "deploy_now", &json!({"sha": target, "live_sha": ctx.live_sha})).await;
    app.emit("supervisor_changed", json!({"deploy_now": {"sha": target}})).await;
    tracing::info!(sha = %target, live = %ctx.live_sha, "deploy now requested from the UI");
    Ok(json!({
        "started": true,
        "sha": target,
        "short": short,
        "live_sha": ctx.live_sha,
        "log_path": ctx.agm_dir.join(LOG_FILE),
        "request_path": ctx.agm_dir.join(REQUEST_FILE),
    }))
}

fn write_request(agm_dir: &Path, v: &Value) -> Result<(), String> {
    let path = agm_dir.join(REQUEST_FILE);
    let tmp = agm_dir.join(format!("{REQUEST_FILE}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, v.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// 叫不起 kick 就收回請求檔：留著的話之後每一下都 409，而且沒有人會去吃它。
fn undo(agm_dir: &Path, why: &str) {
    let _ = std::fs::remove_file(agm_dir.join(REQUEST_FILE));
    tracing::warn!(why, "deploy now did not start");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::git::run;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeKick {
        calls: AtomicUsize,
        fail: Option<&'static str>,
    }

    impl KickLauncher for FakeKick {
        fn kick(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.fail.map_or(Ok(()), |e| Err(e.to_string()))
        }
    }

    fn fake(fail: Option<&'static str>) -> FakeKick {
        FakeKick { calls: AtomicUsize::new(0), fail }
    }

    fn commit(repo: &Path, file: &str, msg: &str) -> String {
        let p = repo.join(file);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("{msg}\n")).unwrap();
        run(repo, &["add", "-A"]);
        run(repo, &["commit", "-q", "-m", msg]);
        run(repo, &["rev-parse", "HEAD"])
    }

    /// base（線上）→ docs → daemon → docs。`origin/main` 指到最後一個。
    struct Fixture {
        env: crate::testing::Env,
        ctx: Ctx,
        live: String,
        docs_only: String,
        code: String,
        head: String,
    }

    async fn fixture() -> Fixture {
        let env = crate::testing::env().await;
        let repo = env.repo.clone();
        let live = commit(&repo, "daemon/src/a.rs", "live");
        let docs_only = commit(&repo, "docs/x.md", "docs one");
        let code = commit(&repo, "daemon/src/a.rs", "fix the daemon");
        let head = commit(&repo, "docs/y.md", "docs two");
        run(&repo, &["update-ref", "refs/remotes/origin/main", &head]);
        let agm_dir = env.dir.join("agm");
        std::fs::create_dir_all(agm_dir.join("bin")).unwrap();
        std::fs::write(agm_dir.join(KICK_SCRIPT), format!("#!/bin/bash\n# reads {REQUEST_FILE}\n")).unwrap();
        let ctx = Ctx { repo, agm_dir, live_sha: live[..8].to_string() };
        Fixture { env, ctx, live, docs_only, code, head }
    }

    #[tokio::test]
    async fn behind_counts_every_commit_and_flags_code_changes() {
        let f = fixture().await;
        let v = behind(&f.ctx.repo, &f.ctx.live_sha).await.unwrap();
        assert_eq!(v["behind"], 3);
        assert_eq!(v["code_commits"], 1);
        assert_eq!(v["code_changed"], true);
        assert_eq!(v["target_sha"], f.head);
        let subjects: Vec<&str> = v["commits"].as_array().unwrap().iter().map(|c| c["subject"].as_str().unwrap()).collect();
        assert_eq!(subjects, ["docs two", "fix the daemon", "docs one"]);
        // docs-only 的落後不算要部署（kick 同一條規則）。
        let v = behind(&f.ctx.repo, &f.code).await.unwrap();
        assert_eq!(v["behind"], 1);
        assert_eq!(v["code_changed"], false);
        assert!(behind(&f.ctx.repo, "unknown").await.is_err(), "沒有 git 的建置說不出落後多少");
        let _ = &f.live;
    }

    #[tokio::test]
    async fn start_writes_the_request_and_kicks_once() {
        let f = fixture().await;
        let kick = fake(None);
        let out = start(&f.env.app, &f.ctx, Some(&f.code), &kick).await.unwrap();
        assert_eq!(out["started"], true);
        assert_eq!(out["sha"], f.code, "部署確認框上的 commit，不是 origin/main HEAD");
        assert_eq!(kick.calls.load(Ordering::SeqCst), 1);
        assert!(store::approvals(&f.env.app.db, 100).await.unwrap().is_empty(), "不再開核准單");
        let req: Value = serde_json::from_str(&std::fs::read_to_string(f.ctx.agm_dir.join(REQUEST_FILE)).unwrap()).unwrap();
        assert_eq!(req["sha"], f.code);

        // 第二下：請求還沒被 kick 吃掉＝有部署在跑，409，不再 kick。
        let e = start(&f.env.app, &f.ctx, None, &kick).await.unwrap_err();
        assert!(matches!(&e, LcError::Conflict(v) if v["reason"] == "deploy_in_progress" && v["running"]["kind"] == "requested"), "{e:?}");
        assert_eq!(kick.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn start_refuses_while_someone_holds_a_rebuild_or_restart_window() {
        for resource in crate::supervisor::maintenance::RESOURCES {
            let f = fixture().await;
            let kick = fake(None);
            let held = store::acquire_lease(&f.env.app.db, resource, "fixer-bot", None, None, &crate::db::iso_in(600), false, None, &json!({}))
                .await
                .unwrap();
            assert!(held.is_some());
            let e = start(&f.env.app, &f.ctx, None, &kick).await.unwrap_err();
            assert!(
                matches!(&e, LcError::Conflict(v) if v["running"]["kind"] == "lease" && v["running"]["resource"] == resource),
                "{resource}: {e:?}"
            );
            assert_eq!(kick.calls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn start_refuses_docs_only_and_commits_off_main() {
        let f = fixture().await;
        let kick = fake(None);
        let e = start(&f.env.app, &f.ctx, Some(&f.docs_only), &kick).await.unwrap_err();
        assert!(matches!(&e, LcError::Conflict(v) if v["reason"] == "nothing_to_deploy"), "{e:?}");
        run(&f.ctx.repo, &["checkout", "-q", "-b", "side", &f.live]);
        let side = commit(&f.ctx.repo, "daemon/src/b.rs", "unreviewed");
        let e = start(&f.env.app, &f.ctx, Some(&side), &kick).await.unwrap_err();
        assert!(matches!(&e, LcError::Conflict(v) if v["reason"] == "target_not_on_main"), "{e:?}");
        assert_eq!(kick.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn start_refuses_a_target_older_than_the_live_binary() {
        let mut f = fixture().await;
        // An old page confirms `code` after a newer code commit is already live.
        let newer = commit(&f.ctx.repo, "daemon/src/b.rs", "newer live");
        run(&f.ctx.repo, &["update-ref", "refs/remotes/origin/main", &newer]);
        f.ctx.live_sha = newer[..8].to_string();
        let kick = fake(None);
        let e = start(&f.env.app, &f.ctx, Some(&f.code), &kick).await.unwrap_err();
        assert!(matches!(&e, LcError::Conflict(v) if v["reason"] == "target_older_than_live"), "{e:?}");
        assert!(matches!(&e, LcError::Conflict(v) if v["message"].as_str().unwrap_or("").contains("降版")), "{e:?}");
        assert_eq!(kick.calls.load(Ordering::SeqCst), 0);
        assert!(!f.ctx.agm_dir.join(REQUEST_FILE).exists());
    }

    #[tokio::test]
    async fn a_kick_that_cannot_start_rolls_the_request_back() {
        let f = fixture().await;
        let kick = fake(Some("Could not find service"));
        let e = start(&f.env.app, &f.ctx, None, &kick).await.unwrap_err();
        assert!(matches!(&e, LcError::Unavailable(v) if v["reason"] == "kick_start_failed"), "{e:?}");
        assert!(!f.ctx.agm_dir.join(REQUEST_FILE).exists(), "留著的話之後每一下都 409");
        // 收回之後可以再按。
        assert!(in_progress(&f.env.app, &f.ctx.agm_dir).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_old_kick_is_named_before_anything_is_written() {
        let f = fixture().await;
        std::fs::write(f.ctx.agm_dir.join(KICK_SCRIPT), "#!/bin/bash\n# old\n").unwrap();
        let kick = fake(None);
        let e = start(&f.env.app, &f.ctx, None, &kick).await.unwrap_err();
        assert!(matches!(&e, LcError::Unavailable(v) if v["reason"] == "kick_outdated"), "{e:?}");
        assert!(!f.ctx.agm_dir.join(REQUEST_FILE).exists());
        assert_eq!(kick.calls.load(Ordering::SeqCst), 0);
    }

    /// 寫一支假的排程器指令（`body` 接在 `#!/bin/sh` 之後）並確定它已經可以被 exec（issue #189）：見 [`crate::testing::write_exec`]。
    fn write_exec(path: &Path, body: &str) {
        crate::testing::write_exec(path, format!("#!/bin/sh\n{body}"))
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp() -> Tmp {
        let d = crate::testing::track(std::env::temp_dir().join(format!("am-deploy-kick-{}", crate::db::ulid())));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    /// 假 `systemctl`：把 argv 與它看到的 `XDG_RUNTIME_DIR` 記下來，照 `rc` 離開（issue #677）。
    fn fake_systemctl(dir: &Path, rc: i32, stderr: &str) -> (SchedulerKick, PathBuf) {
        let log = dir.join("systemctl.log");
        let bin = dir.join("systemctl");
        write_exec(&bin, &format!(
            "printf '%s\\n' \"$*\" \"XDG_RUNTIME_DIR=${{XDG_RUNTIME_DIR:-}}\" >> '{}'\nprintf '%s' '{stderr}' >&2\nexit {rc}\n",
            log.display()
        ));
        let mut k = SchedulerKick::systemd(4242, false);
        k.program = bin.to_string_lossy().into_owned();
        (k, log)
    }

    #[test]
    fn linux_kick_starts_the_systemd_user_unit_without_blocking() {
        let dir = tmp();
        let (k, log) = fake_systemctl(&dir.0, 0, "");
        k.kick().unwrap();
        // --no-block：oneshot 的 start 預設會等 kick 整輪跑完，UI 那一下會卡住好幾分鐘。
        // 沒有 XDG_RUNTIME_DIR 時要補 /run/user/<uid>，否則 systemctl --user 連不到 user bus。
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "--user start --no-block com.agm.daemon-update.service\nXDG_RUNTIME_DIR=/run/user/4242\n"
        );
    }

    #[test]
    fn linux_kick_keeps_an_existing_runtime_dir_and_reports_systemctl_errors() {
        let dir = tmp();
        let (mut k, log) = fake_systemctl(&dir.0, 5, "Unit com.agm.daemon-update.service not found.");
        k.env = SchedulerKick::systemd(4242, true).env;
        assert_eq!(k.kick().unwrap_err(), "Unit com.agm.daemon-update.service not found.");
        // 已經有值就不蓋：假 systemctl 看到的是繼承下來的原值（本機沒設就是空的，遠端編譯主機上有）。
        let inherited = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        assert!(std::fs::read_to_string(&log).unwrap().ends_with(&format!("XDG_RUNTIME_DIR={inherited}\n")));
        let (k, _) = fake_systemctl(&dir.0, 5, "");
        let err = k.kick().unwrap_err();
        assert!(err.ends_with("systemctl --user start --no-block com.agm.daemon-update.service rc=Some(5)"), "{err}");
    }

    #[test]
    fn each_host_kicks_its_own_scheduler() {
        // 只看組出來的指令，不真的執行 launchctl（destructive-canary 的規則）。
        let mac = SchedulerKick::launchd(501);
        assert_eq!((mac.program.as_str(), mac.args.as_slice()), ("launchctl", ["kickstart".to_string(), "gui/501/com.agm.daemon-update".into()].as_slice()));
        assert!(mac.env.is_empty());
        let want = if cfg!(target_os = "linux") { "systemctl" } else { "launchctl" };
        assert_eq!(SchedulerKick::for_this_host().program, want);
    }

    #[test]
    fn only_unmarked_ui_token_callers_can_press_the_button() {
        // #556 裁示：目前持有共用 UI token 就視為使用者，接受 LAN／本機行程風險。
        assert!(refuse_bot_caller(&HeaderMap::new()).is_ok());

        // 只帶半套、帶錯 token 或完整宣告 bot 身分，都不能降級成使用者。
        let cases: &[&[(&str, &[u8])]] = &[
            &[("X-AM-Bot-Id", b"b1")],
            &[("X-AM-Bot-Token", b"not-the-token")],
            &[("X-AM-Bot-Id", b"b1"), ("X-AM-Bot-Token", b"not-the-token")],
            &[("X-AM-Bot-Token", b"")],
            &[("X-AM-Bot-Token", b"\xff")],
        ];
        for pairs in cases {
            let mut headers = HeaderMap::new();
            for (name, value) in *pairs {
                headers.insert(*name, axum::http::HeaderValue::from_bytes(value).unwrap());
            }
            assert!(
                matches!(refuse_bot_caller(&headers), Err(LcError::Forbidden(v)) if v["reason"] == "ui_only"),
                "headers {pairs:?} must not fall back to the user principal");
        }
    }
}
