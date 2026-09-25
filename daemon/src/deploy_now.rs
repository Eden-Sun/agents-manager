//! 左上角「立即部署」（使用者 2026-09-25：「agm排以外，要我可以在左上角直接點立即部署」）。
//!
//! 例行更新是 launchd `com.agm.daemon-update` 每 5 分鐘跑 `daemon-update-kick.sh`，由它自己判斷
//! 整點／申請門檻／等太久才往下走，而 rebuild 核准要等 AGM 裁示（SPEC §18.2、§18.10）。這裡只做
//! 「使用者按下去」那一段，**不另寫一套 build＋swap**：
//!
//! 1. `GET /api/deploy/status`：線上 binary（`build_info::BUILD_SHA`）落後 `origin/main` 幾個 commit、
//!    有沒有會進 binary 的差異（路徑同 kick 的 `build-inputs`），以及現在有沒有部署在跑。
//! 2. `POST /api/deploy/now`：開一筆 `requester=daemon-update-kick` 的 rebuild 核准並**以使用者的名義核准**
//!    （這一下就是使用者的裁示，不再等 AGM），寫 `supervisor/AGM/daemon-update.now.json`，再
//!    `launchctl kickstart` 同一個 launchd job。kick 看到這個檔就略過觸發閘，其餘安全條件照舊
//!    （乾淨 HEAD worktree、整樹測試、沒人 working 才換、租約、`.bak`、驗證失敗回滾）。
//!
//! 為什麼走 launchd 而不是 daemon 自己 fork 一支 kick：kick 的 `AGM_BUILD_BOT`、`PATH` 只寫在 plist 的
//! `EnvironmentVariables`，daemon 這邊拿不到；而且 launchd 保證同一個 job 同時只有一個，跟排程那一輪
//! 不會疊。kick 正在跑的那一刻 kickstart 不會再起一個，請求檔留著，下一輪（最多 5 分鐘）就吃到。
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

/// kick 申請 rebuild 用的名字（plist 沒設 `AM_AGENT_NAME`）。核准的 `requester` 要跟 kick 之後
/// `lease acquire --owner` 逐字相等，否則 409 `approval_owner_mismatch`。
pub const KICK_OWNER: &str = "daemon-update-kick";
/// 使用者在 UI 按「立即部署」時記在核准上的 actor。
pub const USER_ACTOR: &str = "user(立即部署)";
/// 使用者的請求：kick 讀到就走「立即」模式，派工成功才刪。
pub const REQUEST_FILE: &str = "daemon-update.now.json";
pub const LOG_FILE: &str = "daemon-update.log";
pub const KICK_SCRIPT: &str = "bin/daemon-update-kick.sh";
pub const LAUNCHD_LABEL: &str = "com.agm.daemon-update";
/// kick 派出的更新交辦都用這個前綴（`agm-daemon-update-<sha>`，立即模式多一段 `-now-<核准>`）。
pub const ASSIGNMENT_PREFIX: &str = "agm-daemon-update-";
/// 確認框最多列幾個 commit；落後更多時只列最新的這幾個，總數照實寫。
const COMMIT_LIST_MAX: usize = 30;
/// 核准有效期同 kick 的 `--expires-in`（issue #421）：等安全窗口可能要好幾個整點。
const APPROVAL_TTL_SECS: i64 = 21600;
/// `GET /status` 最多多久 fetch 一次（背景跑，不擋回應）。
const FETCH_EVERY: Duration = Duration::from_secs(300);
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// 使用者按下之後由誰把 kick 叫起來。正式是 `launchctl kickstart`；測試換成假的，不碰 launchd。
pub trait KickLauncher: Send + Sync {
    fn kick(&self) -> Result<(), String>;
}

pub struct Launchctl;

impl KickLauncher for Launchctl {
    fn kick(&self) -> Result<(), String> {
        // SAFETY: getuid 沒有前置條件，也不會失敗。
        let uid = unsafe { libc::getuid() };
        let target = format!("gui/{uid}/{LAUNCHD_LABEL}");
        // 不帶 -k：正在跑的那一輪不能被砍掉（它可能正拿著租約派工）；沒在跑就起一輪。
        let out = std::process::Command::new("launchctl").args(["kickstart", &target]).output().map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if err.is_empty() { format!("launchctl kickstart {target} rc={:?}", out.status.code()) } else { err })
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
    pub fn of(app: &Arc<App>) -> Self {
        Ctx { repo: repo_dir(app), agm_dir: crate::supervisor::setup::agm_dir(app), live_sha: crate::build_info::BUILD_SHA.to_string() }
    }
}

/// daemon 從哪個 repo 建出來的：`<repo>/target/release/agents-managerd` 往上找第一個同時有 `.git`
/// 與 `daemon/Cargo.toml` 的目錄。找不到（binary 被搬走）才退回 kick 的預設 `~/project/agents-manager`。
pub fn repo_dir(app: &Arc<App>) -> PathBuf {
    if let Some(found) = app.exe.ancestors().find(|d| d.join(".git").exists() && d.join("daemon/Cargo.toml").is_file()) {
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

/// 現在有沒有部署在跑；有就回是哪一種、誰。三種都算：
/// * 使用者上一下的請求檔還在（kick 還沒派出去，或在等安全窗口）；
/// * 有人握著 rebuild／restart 租約（kick 正在派、建置 child 正在換、或別顆 bot 自己在重建）；
/// * `agm-daemon-update-*` 交辦還沒結案（含等驗收，同 kick 的 `assignments --open`）。
pub async fn in_progress(app: &Arc<App>, agm_dir: &Path) -> Result<Option<Value>, LcError> {
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    if let Some(req) = read_request(agm_dir) {
        return Ok(Some(json!({"kind": "requested", "sha": req.get("sha"), "requested_at": req.get("requested_at"),
                              "approval_id": req.get("approval_id")})));
    }
    let now = crate::db::now();
    for resource in crate::supervisor::maintenance::RESOURCES {
        if let Some(l) = store::lease(&app.db, resource).await.map_err(up)?.filter(|l| l.held_at(&now)) {
            return Ok(Some(json!({"kind": "lease", "resource": resource, "owner": l.owner, "expires_at": l.expires_at})));
        }
    }
    let open = store::unsettled_assignments(&app.db).await.map_err(up)?;
    if let Some(a) = open.iter().find(|a| a.client_request_id.starts_with(ASSIGNMENT_PREFIX)) {
        return Ok(Some(json!({"kind": "assignment", "client_request_id": a.client_request_id, "status": a.status,
                              "bot_id": a.target_bot_id})));
    }
    Ok(None)
}

/// kick 在立即模式替建置 child 申請 restart 時用的 request id 前綴，後面接那筆 rebuild 核准的 id。
pub const RESTART_REQUEST_PREFIX: &str = "deploy-now-restart-";

/// 立即部署的 restart 核准由 daemon 在建立當下核准（#447 之後 `decide` 的 approve 要驗過的 AGM 角色，
/// launchd 跑的 kick 沒有角色身分，打 HTTP decide 會 403）。只在全部對得上時才核准，否則照舊留給 AGM：
/// 請求檔還在、request id 是 `deploy-now-restart-<請求檔的 rebuild 核准>`、那筆 rebuild 是使用者在 UI 核准的
/// 且還有效、commit 一致、這筆 restart 還是 pending。回傳核准後的那筆。
pub async fn preapprove_restart(app: &Arc<App>, a: &store::Approval) -> Option<store::Approval> {
    preapprove_restart_in(app, &Ctx::of(app).agm_dir, a).await
}

async fn preapprove_restart_in(app: &Arc<App>, agm_dir: &Path, a: &store::Approval) -> Option<store::Approval> {
    if a.purpose != "restart" || a.status != "pending" {
        return None;
    }
    let rebuild_id = a.client_request_id.as_deref()?.strip_prefix(RESTART_REQUEST_PREFIX)?;
    let req = read_request(agm_dir)?;
    if req.get("approval_id").and_then(Value::as_str) != Some(rebuild_id) {
        return None;
    }
    let rebuild = store::approval(&app.db, rebuild_id).await.ok()??;
    let now = crate::db::now();
    let live = rebuild.purpose == "rebuild"
        && rebuild.status == "approved"
        && rebuild.decided_by.as_deref() == Some(USER_ACTOR)
        && rebuild.expires_at.as_deref().is_none_or(|e| crate::db::cmp_ts(e, &now).is_gt());
    let same_commit = match (rebuild.target_commit.as_deref(), a.target_commit.as_deref()) {
        (Some(r), Some(t)) if !t.is_empty() => r.starts_with(t) || t.starts_with(r),
        _ => false,
    };
    if !live || !same_commit {
        return None;
    }
    let reason = format!("使用者在 UI 按「立即部署」（rebuild 核准 {rebuild_id}）");
    match store::decide_approval_from(&app.db, &a.id, "pending", "approved", USER_ACTOR, Some(&reason), None).await {
        Ok(Some((decided, _))) => {
            tracing::info!(approval = %a.id, rebuild = rebuild_id, "deploy now: restart approval pre-approved by the daemon");
            Some(decided)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(approval = %a.id, error = %e, "deploy now: could not pre-approve the restart");
            None
        }
    }
}

/// 請求檔還在、讀得懂才算。壞掉的檔 kick 會自己清掉並記 log，這裡不把它當成有部署在跑。
fn read_request(agm_dir: &Path) -> Option<Value> {
    let txt = std::fs::read_to_string(agm_dir.join(REQUEST_FILE)).ok()?;
    serde_json::from_str::<Value>(&txt).ok().filter(|v| v.get("approval_id").and_then(Value::as_str).is_some())
}

/// 正在 `working` 的 bot（確認框列出來：換 binary 要等它們，不是按了就砍）。
async fn working_bots(app: &Arc<App>) -> Vec<Value> {
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

pub async fn status(app: &Arc<App>, ctx: &Ctx) -> Result<Value, LcError> {
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

/// 只給 UI：帶 bot 身分來的一律 403。bot 要重建照 §18.10 申請核准，不能替使用者按這顆鍵。
/// （共用 UI token 的前提下 daemon 分不出人與不報身分的 bot，SPEC §18.10 同一個取捨。）
pub fn refuse_bot_caller(headers: &HeaderMap) -> Result<(), LcError> {
    if headers.contains_key("X-AM-Bot-Id") {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "ui_only",
            "message": "立即部署只給使用者在 UI 上按；bot 要重建請照 SPEC §18.10 申請核准"})));
    }
    Ok(())
}

pub async fn post_now(State(app): State<Arc<App>>, headers: HeaderMap, body: Option<Json<NowIn>>) -> Result<Json<Value>, LcError> {
    refuse_bot_caller(&headers)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    Ok(Json(start(&app, &Ctx::of(&app), b.sha.as_deref(), &Launchctl).await?))
}

pub async fn start(app: &Arc<App>, ctx: &Ctx, sha: Option<&str>, launcher: &dyn KickLauncher) -> Result<Value, LcError> {
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
    if !code_changed(&ctx.repo, &live_full, &target).await.map_err(LcError::Upstream)? {
        return Err(LcError::conflict("nothing_to_deploy", json!({"sha": target, "live_sha": ctx.live_sha,
            "message": "線上那顆到這個 commit 之間只動到不進 binary 的檔，不用重建"})));
    }
    let short: String = target.chars().take(8).collect();
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    let expires = crate::db::iso_in(APPROVAL_TTL_SECS);
    let request_id = format!("deploy-now-{short}-{}", crate::db::ulid());
    // 不推 `approval_requested`：這筆不是在等誰裁示，下一行就由使用者核准。
    let created = store::create_approval_superseding(
        &app.db,
        KICK_OWNER,
        "rebuild",
        &format!("立即部署 {}..{short}（使用者在 UI 按下）；restart 由 daemon 在 kick 申請時以同一個授權核准", ctx.live_sha),
        Some(&target),
        Some(&expires),
        Some(&request_id),
        None,
        Some("使用者在左上角按「立即部署」"),
    )
    .await
    .map_err(up)?;
    let id = created.approval.id.clone();
    let decided = store::decide_approval_from(&app.db, &id, "pending", "approved", USER_ACTOR, Some("使用者在 UI 按「立即部署」"), None)
        .await
        .map_err(up)?;
    if decided.is_none() {
        return Err(LcError::conflict("approval_not_pending", json!({"approval_id": id, "message": "剛開的核准已經不是 pending，這趟不動"})));
    }
    let request = json!({"approval_id": id, "sha": target, "live_sha": ctx.live_sha, "requested_at": crate::db::now(), "requested_by": "ui"});
    if let Err(e) = write_request(&ctx.agm_dir, &request) {
        undo(app, &ctx.agm_dir, &id, &format!("寫不進請求檔：{e}")).await;
        return Err(LcError::Unavailable(json!({"reason": "request_write_failed", "message": e, "retryable": true})));
    }
    if let Err(e) = launcher.kick() {
        undo(app, &ctx.agm_dir, &id, &format!("叫不起 {LAUNCHD_LABEL}：{e}")).await;
        return Err(LcError::Unavailable(json!({"reason": "kick_start_failed", "message": format!("叫不起例行更新（{LAUNCHD_LABEL}）：{e}"),
            "retryable": true})));
    }
    let _ = store::add_note(&app.db, "deploy_now", &json!({"approval_id": id, "sha": target, "live_sha": ctx.live_sha})).await;
    app.emit("supervisor_changed", json!({"approval": decided.map(|(a, _)| a.to_json()), "deploy_now": {"sha": target}})).await;
    tracing::info!(sha = %target, live = %ctx.live_sha, approval = %id, "deploy now requested from the UI");
    Ok(json!({
        "started": true,
        "sha": target,
        "short": short,
        "live_sha": ctx.live_sha,
        "approval_id": id,
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

/// 叫不起 kick 就收回：請求檔刪掉、核准撤銷。留著的話之後每一下都 409，而且沒有人會去用那筆核准。
async fn undo(app: &Arc<App>, agm_dir: &Path, approval_id: &str, why: &str) {
    let _ = std::fs::remove_file(agm_dir.join(REQUEST_FILE));
    if let Err(e) = store::decide_approval_from(&app.db, approval_id, "approved", "revoked", USER_ACTOR, Some(why), None).await {
        tracing::warn!(approval = %approval_id, error = %e, "deploy now: could not revoke the approval after a failed start");
    }
    tracing::warn!(approval = %approval_id, why, "deploy now did not start");
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

    async fn restart_request(f: &Fixture, rebuild_id: &str, commit: &str) -> store::Approval {
        store::create_approval_superseding(&f.env.app.db, "bot-build", "restart", "daemon 重啟", Some(commit), None,
            Some(&format!("{RESTART_REQUEST_PREFIX}{rebuild_id}")), None, None)
        .await
        .unwrap()
        .approval
    }

    async fn started(f: &Fixture) -> String {
        start(&f.env.app, &f.ctx, Some(&f.code), &fake(None)).await.unwrap()["approval_id"].as_str().unwrap().to_string()
    }

    /// #447：kick 沒有角色身分打不了 decide，restart 改由 daemon 在建立當下核准——但只在全部對得上時。
    #[tokio::test]
    async fn restart_for_a_live_deploy_now_is_approved_by_the_daemon() {
        let f = fixture().await;
        let rebuild = started(&f).await;
        let r = restart_request(&f, &rebuild, &f.code[..8]).await;
        let ok = preapprove_restart_in(&f.env.app, &f.ctx.agm_dir, &r).await.expect("對得上就核准");
        assert_eq!((ok.status.as_str(), ok.decided_by.as_deref()), ("approved", Some(USER_ACTOR)));
    }

    #[tokio::test]
    async fn a_restart_for_another_commit_is_left_to_agm() {
        let f = fixture().await;
        let rebuild = started(&f).await;
        let r = restart_request(&f, &rebuild, &f.head).await;
        assert!(preapprove_restart_in(&f.env.app, &f.ctx.agm_dir, &r).await.is_none(), "commit 對不上不能核准");
        assert_eq!(store::approval(&f.env.app.db, &r.id).await.unwrap().unwrap().status, "pending", "留給 AGM");
    }

    #[tokio::test]
    async fn a_restart_naming_another_rebuild_or_after_the_request_is_gone_is_left_to_agm() {
        let f = fixture().await;
        let rebuild = started(&f).await;
        // 另一筆同樣由使用者核准、同一個 commit 的 rebuild，但請求檔指的不是它。
        let db = &f.env.app.db;
        let other = store::create_approval_superseding(db, KICK_OWNER, "rebuild", "另一趟", Some(&f.code), None, Some("other-rebuild"), None, None)
            .await
            .unwrap()
            .approval;
        store::decide_approval_from(db, &other.id, "pending", "approved", USER_ACTOR, None, None).await.unwrap();
        let stranger = restart_request(&f, &other.id, &f.code).await;
        assert!(preapprove_restart_in(&f.env.app, &f.ctx.agm_dir, &stranger).await.is_none(), "不是請求檔那筆 rebuild");
        std::fs::remove_file(f.ctx.agm_dir.join(REQUEST_FILE)).unwrap();
        let late = restart_request(&f, &rebuild, &f.code).await;
        assert!(preapprove_restart_in(&f.env.app, &f.ctx.agm_dir, &late).await.is_none(), "請求檔不在就不核准");
    }

    #[tokio::test]
    async fn a_revoked_deploy_now_does_not_preapprove_the_restart() {
        let f = fixture().await;
        let rebuild = started(&f).await;
        store::decide_approval_from(&f.env.app.db, &rebuild, "approved", "revoked", "agm", Some("撤回"), None).await.unwrap();
        let r = restart_request(&f, &rebuild, &f.code).await;
        assert!(preapprove_restart_in(&f.env.app, &f.ctx.agm_dir, &r).await.is_none(), "rebuild 已撤銷就不核准 restart");
    }

    #[tokio::test]
    async fn start_approves_as_the_user_writes_the_request_and_kicks_once() {
        let f = fixture().await;
        let kick = fake(None);
        let out = start(&f.env.app, &f.ctx, Some(&f.code), &kick).await.unwrap();
        assert_eq!(out["started"], true);
        assert_eq!(out["sha"], f.code, "部署確認框上的 commit，不是 origin/main HEAD");
        assert_eq!(kick.calls.load(Ordering::SeqCst), 1);
        let id = out["approval_id"].as_str().unwrap();
        let a = store::approval(&f.env.app.db, id).await.unwrap().unwrap();
        assert_eq!((a.status.as_str(), a.purpose.as_str(), a.requester.as_str()), ("approved", "rebuild", KICK_OWNER));
        assert_eq!(a.target_commit.as_deref(), Some(f.code.as_str()));
        assert_eq!(a.decided_by.as_deref(), Some("user(立即部署)"));
        let req: Value = serde_json::from_str(&std::fs::read_to_string(f.ctx.agm_dir.join(REQUEST_FILE)).unwrap()).unwrap();
        assert_eq!(req["approval_id"], id);
        assert_eq!(req["sha"], f.code);

        // 第二下：請求還沒被 kick 吃掉＝有部署在跑，409，不再開核准也不再 kick。
        let before = store::approvals(&f.env.app.db, 100).await.unwrap().len();
        let e = start(&f.env.app, &f.ctx, None, &kick).await.unwrap_err();
        assert!(matches!(&e, LcError::Conflict(v) if v["reason"] == "deploy_in_progress" && v["running"]["kind"] == "requested"), "{e:?}");
        assert_eq!(kick.calls.load(Ordering::SeqCst), 1);
        assert_eq!(store::approvals(&f.env.app.db, 100).await.unwrap().len(), before);
    }

    #[tokio::test]
    async fn start_refuses_while_an_update_assignment_is_open() {
        let f = fixture().await;
        let kick = fake(None);
        // 例行更新的交辦還沒結案（含等驗收）。
        sqlx::query(
            "INSERT INTO supervisor_assignments (id, supervisor_id, target_bot_id, client_request_id, text, status, attempts, created_at, updated_at)
             VALUES ('as1', ?, 'b1', ?, 't', 'delivered', 0, ?, ?)",
        )
        .bind(store::SUPERVISOR_ID)
        .bind(format!("{ASSIGNMENT_PREFIX}{}", f.head))
        .bind(crate::db::now())
        .bind(crate::db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
        let e = start(&f.env.app, &f.ctx, None, &kick).await.unwrap_err();
        assert!(matches!(&e, LcError::Conflict(v) if v["running"]["kind"] == "assignment"), "{e:?}");
        assert_eq!(kick.calls.load(Ordering::SeqCst), 0);
        assert!(store::approvals(&f.env.app.db, 100).await.unwrap().is_empty(), "擋下來就不開核准");
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
        assert!(store::approvals(&f.env.app.db, 100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_kick_that_cannot_start_rolls_the_approval_and_request_back() {
        let f = fixture().await;
        let kick = fake(Some("Could not find service"));
        let e = start(&f.env.app, &f.ctx, None, &kick).await.unwrap_err();
        assert!(matches!(&e, LcError::Unavailable(v) if v["reason"] == "kick_start_failed"), "{e:?}");
        assert!(!f.ctx.agm_dir.join(REQUEST_FILE).exists(), "留著的話之後每一下都 409");
        let rows = store::approvals(&f.env.app.db, 100).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "revoked");
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
        assert!(store::approvals(&f.env.app.db, 100).await.unwrap().is_empty());
        assert!(!f.ctx.agm_dir.join(REQUEST_FILE).exists());
        assert_eq!(kick.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn bots_cannot_press_the_button() {
        let mut h = HeaderMap::new();
        assert!(refuse_bot_caller(&h).is_ok());
        h.insert("X-AM-Bot-Id", "b1".parse().unwrap());
        assert!(matches!(refuse_bot_caller(&h), Err(LcError::Forbidden(v)) if v["reason"] == "ui_only"));
    }
}
