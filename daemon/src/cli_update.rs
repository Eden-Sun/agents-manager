//! header 一鍵升級 codex（SPEC §6.9、API `POST /api/hosts/{name}/cli-update`，使用者 2026-09-25：「codex 的 upgrade 也和
//! claude 用一樣的方式出現在 header」）。
//!
//! claude 是 CLI 自己下載新版、重啟就換；codex 的新版**還沒安裝**（`codex_update.rs` 的「需安裝」通知），以前 header 只能切到那顆
//! bot 叫人自己去裝。這裡把「裝」補上：在那台主機跑**寫死的**官方安裝指令 → 重新讀 `codex --version` 確認真的升上去 →
//! 把那台 codex run 的「需安裝」通知改成「已安裝，重啟套用」→ 交給既有的一鍵重啟（`bulk_restart`，只收那台主機的 codex）。
//!
//! 刻意的邊界：
//! - 只給使用者在 UI 上按：帶 bot 身分的請求一律 403（同 `deploy_now::refuse_bot_caller`）——換掉的是所有 codex bot 共用的那顆 binary。
//! - 指令不接受呼叫端傳入；同一台同時只跑一個（409）；有逾時；輸出寫 `<data_dir>/cli-update.log`。
//! - 安裝失敗、讀不到版本、或裝完版本沒變：**一顆 bot 都不重啟**，`cli_update_done` 帶 `reason` 講清楚。
//! - 綁定使用者核准的版本（#569）：請求帶確認框寫的 `target_version`，要等於 daemon 眼中那台「需安裝」通知的目標（不然 409
//!   `stale_target`）；裝完要 `>= target` 才算成功，升了但沒到（CDN 還沒傳到、PATH 上是別顆）是 `target_not_reached`，不重啟。
//!   安裝前磁碟已經 `>= target` 就不再跑安裝指令，直接改通知、開重啟。
//! - 會動到機器的三件事（安裝、讀版本、開批次）都走 [`Runner`]，測試換成假的：測試裡絕對不能真的跑 `curl | sh`。

use crate::bulk_restart::Scope;
use crate::changelog::{cli_version_string, parse_version, version_string};
use crate::hosts::HostFence;
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write as _;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// codex 自己的升級提示寫的那一句（`Run sh -c '…' to update.`）。只有這一條，不收參數。
pub const CODEX_INSTALL: &str = "curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh";

/// 下載＋解壓一般半分鐘內；網路慢給到五分鐘，再久就是卡住了。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

pub const LOG_FILE: &str = "cli-update.log";

/// 失敗時回給畫面的輸出只留尾巴，全文在 log。
const TAIL_CHARS: usize = 1500;

/// 主機端的安裝鎖（#564）：`$HOME` 底下一條 symlink，目標是持鎖那支 shell 的 pid。
///
/// DB 那一列只擋得住**同一個資料目錄**的 daemon；daemon 被砍掉時遠端的 `curl | sh` 不會跟著停（逾時也只砍得掉本機的
/// ssh），隔離實例也會對同一台開安裝。所以安裝指令本身包在這把鎖裡：`ln -s` 是原子的，建立的同時就寫好了 pid，
/// 沒有「目錄建好了、pid 還沒寫」的空窗會被誤判成過期。已經有人拿著而且 pid 還活著就不裝、印 [`LOCKED_MARK`] 退出
/// [`LOCKED_EXIT`]；pid 死了（被 SIGKILL，沒跑到 trap）就當過期拿走。不分實例：換掉的是同一顆 codex。
pub const INSTALL_LOCK: &str = "$HOME/.agents-manager-codex-install.lock";
const LOCKED_EXIT: i32 = 75;
const LOCKED_MARK: &str = "AM_CODEX_INSTALL_LOCKED";

/// 把 `inner` 包進主機端的安裝鎖（POSIX sh：遠端是 `ssh … /bin/sh -s`，本機是 `/bin/sh -c`）。
/// 鎖被佔走的訊息寫到 stderr：遠端失敗時 `ssh_exec` 只帶 stderr 回來。
fn locked_script(lock: &str, inner: &str) -> String {
    format!(
        r#"L="{lock}"
if ! ln -s "$$" "$L" 2>/dev/null; then
  p=$(readlink "$L" 2>/dev/null)
  if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then echo "{LOCKED_MARK} pid=$p" >&2; exit {LOCKED_EXIT}; fi
  rm -f "$L"
  ln -s "$$" "$L" 2>/dev/null || {{ echo "{LOCKED_MARK} pid=?" >&2; exit {LOCKED_EXIT}; }}
fi
trap '[ "$(readlink "$L" 2>/dev/null)" = "$$" ] && rm -f "$L"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
{inner}
"#
    )
}

/// 那台的安裝鎖現在有沒有活著的主人：印 `busy <pid>` 或 `free`。
fn probe_script(lock: &str) -> String {
    format!(r#"L="{lock}"; p=$(readlink "$L" 2>/dev/null); if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then echo "busy $p"; else echo free; fi"#)
}

fn parse_probe(out: &str) -> anyhow::Result<bool> {
    match out.split_whitespace().next() {
        Some("busy") => Ok(true),
        Some("free") => Ok(false),
        _ => anyhow::bail!("看不懂安裝鎖的探測結果：{}", out.trim()),
    }
}

/// 安裝沒跑成的兩種原因：鎖被別人拿著（**沒有**跑安裝指令）跟真的跑了但失敗。
#[derive(Debug, Clone)]
pub enum InstallError {
    Locked(String),
    Failed(String),
}

fn classify_install_error(e: String) -> InstallError {
    if e.contains(LOCKED_MARK) { InstallError::Locked(e) } else { InstallError::Failed(e) }
}

/// 會動到機器的四件事。正式版是 [`Real`]；測試換成假的。
pub trait Runner: Send + Sync {
    /// 在主機端的安裝鎖裡跑安裝指令。`Ok` 是輸出，`Err` 是給人看的原因（含輸出尾巴）。遠端一律走 `fence` 記下的那條連線，
    /// 不用主機名重新解析——途中同名主機改指到另一台，安裝也不能跑到新機器上（#347）。
    fn install<'a>(&'a self, app: &'a Arc<App>, host: &'a str, fence: &'a HostFence) -> BoxFuture<'a, Result<String, InstallError>>;
    /// 那台的安裝鎖有沒有活著的主人（重啟後接手孤兒安裝用，#564）。
    fn installer_busy<'a>(&'a self, app: &'a Arc<App>, host: &'a str, fence: &'a HostFence) -> BoxFuture<'a, anyhow::Result<bool>>;
    /// 那台主機現在的 `codex --version` 原文。
    fn version<'a>(&'a self, app: &'a Arc<App>, host: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;
    /// 開那台主機 codex 的一鍵重啟，回計畫（`POST /api/bots/restart-idle` 同一份形狀）。
    fn restart<'a>(&'a self, app: &'a Arc<App>, scope: Scope) -> BoxFuture<'a, anyhow::Result<Value>>;
}

pub struct Real;

/// 本機跑一段 `/bin/sh -c`，回合併的輸出；非 0 退出是 `Err`（含輸出尾巴）。
async fn local_sh(script: &str, timeout: Duration) -> Result<String, String> {
    let child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("跑不起安裝指令：{e}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("安裝指令超過 {} 秒沒結束，已中止", timeout.as_secs()))?
        .map_err(|e| format!("等安裝指令結束失敗：{e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("安裝指令失敗（{}）：{}", out.status, tail(&text)))
    }
}

impl Runner for Real {
    fn install<'a>(&'a self, _app: &'a Arc<App>, host: &'a str, fence: &'a HostFence) -> BoxFuture<'a, Result<String, InstallError>> {
        Box::pin(async move {
            let script = locked_script(INSTALL_LOCK, CODEX_INSTALL);
            if host == crate::config::LOCAL_HOST {
                local_sh(&script, INSTALL_TIMEOUT).await.map_err(classify_install_error)
            } else {
                // 遠端逾時只砍得掉本機這條 ssh；那邊的安裝可能還在跑（鎖也還在它手上），所以錯誤要講清楚、請人去那台看。
                fence.conn().ssh_exec_path_timeout(&script, INSTALL_TIMEOUT).await.map_err(|e| {
                    classify_install_error(format!("在 {host} 安裝失敗（逾時的話那台的安裝可能還在跑）：{}", tail(&format!("{e:#}"))))
                })
            }
        })
    }

    fn installer_busy<'a>(&'a self, _app: &'a Arc<App>, host: &'a str, fence: &'a HostFence) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            let script = probe_script(INSTALL_LOCK);
            let out = if host == crate::config::LOCAL_HOST {
                local_sh(&script, Duration::from_secs(10)).await.map_err(|e| anyhow::anyhow!(e))?
            } else {
                fence.conn().ssh_exec_timeout(&script, Duration::from_secs(20)).await?
            };
            parse_probe(&out)
        })
    }

    fn version<'a>(&'a self, app: &'a Arc<App>, host: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(crate::changelog::installed_version(app, host, "codex"))
    }

    fn restart<'a>(&'a self, app: &'a Arc<App>, scope: Scope) -> BoxFuture<'a, anyhow::Result<Value>> {
        Box::pin(async move { crate::bulk_restart::spawn_scoped(app, Some(&scope)).await })
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

/// 開機時接手上一顆 daemon 沒跑完的安裝（#564）：多久看一次那台的安裝鎖、最多等多久。
const RECOVER_POLL: Duration = Duration::from_secs(5);
/// 那台的安裝不受 [`INSTALL_TIMEOUT`] 管（逾時只砍得掉本機的 ssh），所以給寬一點；再久就寫成「中斷」交給人。
const RECOVER_MAX: Duration = Duration::from_secs(15 * 60);

/// 這顆 daemon 行程的識別。`cli_updates.boot` 不是它的 `running` 列＝上一顆留下的孤兒（#564）。
fn boot() -> &'static str {
    static B: OnceLock<String> = OnceLock::new();
    B.get_or_init(crate::db::ulid)
}

/// 每一次安裝一列（#564）。以前「同一台只准一個」只記在行程內的 map：daemon 一重啟就忘了，遠端那個 `curl | sh`
/// 還在跑，使用者再按一次就疊出第二個。現在開跑前先寫進 DB，`status='running'` 的列每台最多一筆（部分唯一索引），
/// 收尾時先把結果寫進這一列才推 `cli_update_done`；重啟後 `boot` 不是自己的 `running` 列由 [`recover_at_startup`] 接手。
pub async fn migrate(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS cli_updates (
           id TEXT PRIMARY KEY,
           host TEXT NOT NULL,
           kind TEXT NOT NULL,
           target_version TEXT NOT NULL,
           status TEXT NOT NULL,
           phase TEXT NOT NULL,
           from_version TEXT,
           boot TEXT NOT NULL,
           result TEXT,
           started_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           finished_at TEXT
         )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS cli_updates_one_running ON cli_updates(host) WHERE status = 'running'")
        .execute(pool)
        .await?;
    Ok(())
}

/// `GET /api/state` 的 `cli_updates`：還沒收尾的安裝（含重啟後正在接手的那筆，帶 `recovered:true`）。
/// 進度只走 WS，`cli_update_done` 收不到時前端靠這一格對帳（同 #492 的 `restart_batch`）。讀不到 DB 回空清單。
pub async fn running_list(app: &App) -> Vec<Value> {
    let rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT id, host, kind, target_version, phase, boot FROM cli_updates WHERE status = 'running' ORDER BY started_at",
    )
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|(id, host, kind, target, phase, b)| {
            json!({"update_id": id, "host": host, "kind": kind, "target_version": target, "phase": phase, "recovered": b != boot()})
        })
        .collect()
}

async fn set_phase(app: &App, id: &str, phase: &str, from: Option<&str>) {
    let res = sqlx::query(
        "UPDATE cli_updates SET phase = ?, from_version = COALESCE(?, from_version), updated_at = ? WHERE id = ? AND status = 'running'",
    )
    .bind(phase)
    .bind(from)
    .bind(crate::db::now())
    .bind(id)
    .execute(&app.db)
    .await;
    if let Err(e) = res {
        tracing::warn!(update_id = id, error = %e, "could not record the cli-update phase");
    }
}

/// 結果寫進那一列（`running` → `done`／`failed`），之後這台才能再開一次。
async fn finish_row(app: &App, id: &str, v: &Value) {
    let status = if v["ok"] == json!(true) { "done" } else { "failed" };
    let now = crate::db::now();
    let res = sqlx::query(
        "UPDATE cli_updates SET status = ?, result = ?, updated_at = ?, finished_at = ? WHERE id = ? AND status = 'running'",
    )
    .bind(status)
    .bind(v.to_string())
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(&app.db)
    .await;
    if let Err(e) = res {
        tracing::error!(update_id = id, error = %e, "could not record the cli-update result; the host stays locked until the next restart reconciles it");
    }
}

/// 帶 bot 身分的請求一律拒絕：這一下會換掉所有 codex bot 共用的 binary、接著重啟它們，只給使用者按。
fn refuse_bot_caller(headers: &HeaderMap) -> Result<(), LcError> {
    if headers.contains_key("X-AM-Bot-Id") || headers.contains_key("X-AM-Bot-Token") {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "ui_only",
            "message": "CLI 升級只給使用者在 UI 上按；bot 要升級請照 SPEC §18.10 申請核准"})));
    }
    Ok(())
}

#[derive(Deserialize, Default)]
pub struct CliUpdateIn {
    pub kind: Option<String>,
    /// 確認框寫的「安裝 codex <target>」那一版（#569）。
    pub target_version: Option<String>,
}

/// `POST /api/hosts/{name}/cli-update`
pub async fn post_cli_update(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(host): Path<String>,
    body: Option<Json<CliUpdateIn>>,
) -> Result<Response, LcError> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let v = start(&app, &headers, &host, b.kind.as_deref(), b.target_version.as_deref(), Arc::new(Real)).await?;
    Ok((StatusCode::ACCEPTED, Json(v)).into_response())
}

/// 檢查、佔位、背景執行；只回 `update_id`，進度與結果走 WS。
pub async fn start(
    app: &Arc<App>,
    headers: &HeaderMap,
    host: &str,
    kind: Option<&str>,
    target: Option<&str>,
    runner: Arc<dyn Runner>,
) -> Result<Value, LcError> {
    refuse_bot_caller(headers)?;
    match kind.map(str::trim) {
        Some("codex") => {}
        Some("claude") => return Err(LcError::Bad("claude 會自己下載新版，重啟就套用，不需要安裝；用一鍵重啟（POST /api/bots/restart-idle）".into())),
        other => return Err(LcError::Bad(format!("kind 目前只收 codex，收到 `{}`", other.unwrap_or("")))),
    }
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    let Some(target) = target.and_then(|t| version_string(t.trim())) else {
        return Err(LcError::Bad("要帶 target_version（確認框寫的那一版）；重新整理頁面再按一次".into()));
    };
    // 使用者核准的版本要跟 daemon 眼中那台「需安裝」的目標是同一版：舊分頁、別人剛裝好、帳本又出了新版都擋下來重看。
    match pending_target(app, host).await {
        Some(t) if parse_version(&t) == parse_version(&target) => {}
        current => {
            let message = match &current {
                Some(t) => format!("{host} 的 codex 現在要裝的是 {t}，不是確認框寫的 {target}；重新開確認框再按一次"),
                None => format!("{host} 已經沒有等著安裝的 codex 新版（可能剛裝好了），這一下沒有安裝"),
            };
            return Err(LcError::conflict(
                "stale_target",
                json!({"host": host, "kind": "codex", "target_version": target, "current_target": current, "message": message}),
            ));
        }
    }
    let update_id = crate::db::ulid();
    // 先寫進 DB 才開跑（#564）：每台最多一筆 `running`，重啟之後也還在，由 [`recover_at_startup`] 接手。
    let now = crate::db::now();
    let inserted = sqlx::query(
        "INSERT INTO cli_updates (id, host, kind, target_version, status, phase, boot, started_at, updated_at)
         VALUES (?, ?, 'codex', ?, 'running', 'starting', ?, ?, ?)",
    )
    .bind(&update_id)
    .bind(host)
    .bind(&target)
    .bind(boot())
    .bind(&now)
    .bind(&now)
    .execute(&app.db)
    .await;
    if let Err(e) = inserted {
        if !e.as_database_error().is_some_and(|d| d.is_unique_violation()) {
            return Err(LcError::Upstream(format!("記不下這次安裝，沒有開始：{e}")));
        }
        let cur: Option<(String, String, String)> =
            sqlx::query_as("SELECT id, kind, boot FROM cli_updates WHERE host = ? AND status = 'running'")
                .bind(host)
                .fetch_optional(&app.db)
                .await
                .map_err(|e| LcError::Upstream(e.to_string()))?;
        let (id, kind, b) = cur.unwrap_or_else(|| (String::new(), "codex".into(), boot().to_string()));
        let recovered = b != boot();
        let message = if recovered {
            format!("{host} 上一次的 {kind} 安裝在 daemon 重啟前還沒結束，正在確認那台的安裝跑完了沒；這一下沒有再開一次")
        } else {
            format!("{host} 已經在安裝 {kind}，這一下沒有再開一次")
        };
        return Err(LcError::conflict(
            "cli_update_in_progress",
            json!({"host": host, "kind": kind, "update_id": id, "recovered": recovered, "message": message}),
        ));
    }
    let (app2, host2, id2, target2) = (app.clone(), host.to_string(), update_id.clone(), target.clone());
    tokio::spawn(async move {
        use futures::FutureExt as _;
        let res = std::panic::AssertUnwindSafe(run(&app2, runner.as_ref(), &host2, &id2, &target2)).catch_unwind().await;
        if res.is_err() {
            // `run` 只在收尾時寫結果；中途 panic 的話那一列還是 `running`，這台就再也開不了。
            finish_row(&app2, &id2, &json!({"ok": false, "reason": "internal_error", "error": "安裝流程中途異常結束，沒有重啟任何 bot；看 daemon.log"})).await;
        }
    });
    Ok(json!({"update_id": update_id, "host": host, "kind": "codex", "target_version": target, "started": true}))
}

/// 一次安裝的結果（也是 `cli_update_done` 的內容）。
///
/// 整段可能跑五分鐘，途中同名主機可能重連或改指到另一台（#347）：第一次讀版本之前記下 [`HostFence`]，每次讀完、
/// 改通知與開批次之前都確認它還是權威。不是就停在那裡回 `superseded`——升級前後的版本可能是兩台機器讀的，
/// 不能拿來判斷裝好了沒，更不能去改新機器的通知、重啟新機器的 bot。
pub async fn run(app: &Arc<App>, runner: &dyn Runner, host: &str, update_id: &str, target: &str) -> Value {
    let log_path = app.data_dir.join(LOG_FILE);
    let base = json!({"update_id": update_id, "host": host, "kind": "codex", "target_version": target, "log_path": log_path});
    let progress = |phase: &str, extra: Value| {
        let mut v = base.clone();
        v["phase"] = json!(phase);
        merge(&mut v, extra);
        v
    };
    let done = |ok: bool, extra: Value| {
        let mut v = base.clone();
        v["ok"] = json!(ok);
        merge(&mut v, extra);
        v
    };
    let finish = |v: Value| async move {
        // 結果先落進 DB 才推事件、才放這台的名額（#564）。
        finish_row(app, update_id, &v).await;
        if v["ok"] == json!(false) {
            tracing::warn!(host, reason = %v["reason"], error = %v["error"], "codex 升級沒有完成，沒有重啟任何 bot");
            log_line(app, &format!("[{update_id}] 結束：失敗 {} {}", v["reason"], v["error"]));
        }
        app.emit("cli_update_done", v.clone()).await;
        v
    };

    let Some(fence) = app.hosts.fence(host).await else {
        return finish(done(false, json!({"reason": "superseded", "error": format!("主機 {host} 已經不在設定裡，沒有安裝")}))).await;
    };
    let superseded = |phase: &str, extra: Value| {
        let mut v = done(false, json!({"reason": "superseded",
            "error": format!("主機 {host} 在{phase}時重連或改指到另一台，這次升級作廢：沒有改通知、沒有重啟任何 bot")}));
        merge(&mut v, extra);
        v
    };

    set_phase(app, update_id, "checking", None).await;
    app.emit("cli_update_progress", progress("checking", json!({}))).await;
    let before = runner.version(app, host).await;
    if !app.hosts.is_current(&fence).await {
        return finish(superseded("讀目前版本", json!({}))).await;
    }
    let before = match before.map(|raw| normalized(&raw)) {
        Ok(Some(v)) => v,
        Ok(None) | Err(_) => {
            return finish(done(false, json!({"reason": "version_unreadable", "error": "讀不到目前的 codex 版本，沒有安裝"}))).await;
        }
    };
    let target_v = parse_version(target);
    // 磁碟上已經是核准的那一版（手動裝過、巡邏還沒更新通知）：不再跑一次安裝指令，直接改通知、開重啟。
    let already = parse_version(&before) >= target_v;
    let after = if already {
        log_line(app, &format!("[{update_id}] {host}：codex 已經是 {before}（目標 {target}），不再安裝"));
        before.clone()
    } else {
        set_phase(app, update_id, "installing", Some(&before)).await;
        app.emit("cli_update_progress", progress("installing", json!({"from": before}))).await;
        log_line(app, &format!("[{update_id}] {host}：codex {before}，目標 {target}，執行 {CODEX_INSTALL}"));
        let installed = runner.install(app, host, &fence).await;
        if !app.hosts.is_current(&fence).await {
            log_line(app, &format!("[{update_id}] 安裝途中 {host} 換了連線，結果作廢"));
            return finish(superseded("安裝", json!({"from": before}))).await;
        }
        match installed {
            Ok(out) => log_line(app, &format!("[{update_id}] 安裝輸出：\n{}", out.trim_end())),
            // 那台已經有別的安裝在跑（上一顆 daemon 留下的、或別的實例開的）：這次**沒有**跑安裝指令（#564）。
            Err(InstallError::Locked(e)) => {
                log_line(app, &format!("[{update_id}] {host} 已經有另一個安裝在跑，這次沒有安裝：\n{e}"));
                return finish(done(false, json!({"reason": "already_running", "from": before,
                    "error": format!("{host} 已經有另一個 codex 安裝在跑（{}），這次沒有再裝一次；等它結束再按", e.trim())})))
                .await;
            }
            Err(InstallError::Failed(e)) => {
                log_line(app, &format!("[{update_id}] 安裝失敗：\n{e}"));
                return finish(done(false, json!({"reason": "install_failed", "error": e, "from": before}))).await;
            }
        }
        set_phase(app, update_id, "verifying", None).await;
        app.emit("cli_update_progress", progress("verifying", json!({"from": before}))).await;
        let after = runner.version(app, host).await;
        if !app.hosts.is_current(&fence).await {
            return finish(superseded("確認新版本", json!({"from": before}))).await;
        }
        let after = match after.map(|raw| normalized(&raw)) {
            Ok(Some(v)) => v,
            Ok(None) | Err(_) => {
                return finish(done(false, json!({"reason": "verify_failed", "from": before,
                    "error": "安裝指令跑完了，但讀不到 codex --version，沒有重啟任何 bot"})))
                .await;
            }
        };
        if parse_version(&after) <= parse_version(&before) {
            return finish(done(false, json!({"reason": "version_unchanged", "from": before, "to": after,
                "error": format!("安裝指令跑完了，codex 還是 {after}（原本 {before}），沒有重啟任何 bot")})))
            .await;
        }
        // 升了但沒到核准的那一版：不是使用者按的那個更新，通知維持「需安裝」、一顆都不重啟。
        if parse_version(&after) < target_v {
            return finish(done(false, json!({"reason": "target_not_reached", "from": before, "to": after,
                "error": format!("安裝指令跑完了，codex 是 {after}，還沒到確認的 {target}（原本 {before}），沒有重啟任何 bot")})))
            .await;
        }
        log_line(app, &format!("[{update_id}] 升級完成：{before} → {after}（目標 {target}）"));
        after
    };
    if !app.hosts.is_current(&fence).await {
        return finish(superseded("改通知", json!({"from": before, "to": after}))).await;
    }
    let notices = mark_installed(app, host, &before, &after).await;
    crate::update_watch::forget_disk_version(host, "codex").await;
    set_phase(app, update_id, "restarting", Some(&before)).await;
    app.emit("cli_update_progress", progress("restarting", json!({"from": before, "to": after}))).await;
    if !app.hosts.is_current(&fence).await {
        return finish(superseded("開重啟批次", json!({"from": before, "to": after, "notices_updated": notices}))).await;
    }
    let restart = runner.restart(app, Scope { kind: "codex".into(), host: host.to_string() }).await;
    let v = match restart {
        Ok(plan) => done(true, json!({"from": before, "to": after, "already_installed": already, "notices_updated": notices, "restart": plan})),
        // 裝好了但批次開不起來：新版已經在磁碟上，通知也改成「重啟套用」了，照一般的一鍵重啟再按一次就好。
        Err(e) => done(true, json!({"from": before, "to": after, "already_installed": already, "notices_updated": notices,
            "restart": null, "restart_error": format!("{e:#}")})),
    };
    finish(v).await
}

/// 開機時接手上一顆 daemon 沒收尾的安裝（#564）。那一列還是 `running`，所以這台照樣 409、`GET /api/state` 照樣列著；
/// 每一筆背景等那台的安裝鎖放掉，再讀一次版本收尾。
pub async fn recover_at_startup(app: &Arc<App>) {
    let rows: Vec<(String, String, String, Option<String>)> = match sqlx::query_as(
        "SELECT id, host, target_version, from_version FROM cli_updates WHERE status = 'running' AND boot != ?",
    )
    .bind(boot())
    .fetch_all(&app.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not read unfinished cli updates; they stay locked until the next restart");
            return;
        }
    };
    for (id, host, target, from) in rows {
        tracing::warn!(update_id = %id, host = %host, "codex 升級在 daemon 重啟前還沒收尾，接手確認那台的安裝");
        let app = app.clone();
        tokio::spawn(async move {
            recover(&app, &Real, &id, &host, &target, from.as_deref(), RECOVER_POLL, RECOVER_MAX).await;
        });
    }
}

/// 接手一筆孤兒安裝：等那台的安裝鎖沒有活著的主人（上一顆 daemon 開的 `curl | sh` 跑完或死了），讀版本收尾。
///
/// - 到了核准的版本：改通知成「已安裝，重啟套用」，`ok:true`＋`recovered:true`。**不自動開重啟**——按下去的那一刻跟現在
///   隔了一次 daemon 重啟，哪些 bot 閒著已經不是當時那份；`restart:null`＋`restart_error` 請人按一般的重啟。
/// - 沒到（或讀不到）：`reason:"interrupted"`，一顆都不重啟、通知不動；使用者再按一次會照常重來（安裝前先讀版本）。
/// - 等到 `max` 鎖還在：一樣收成 `interrupted` 放掉這一列；那台的安裝真的還在跑的話，下一次按會被主機端的鎖擋成 `already_running`。
///
/// 絕對不跑安裝指令。
#[allow(clippy::too_many_arguments)]
pub async fn recover(app: &Arc<App>, runner: &dyn Runner, update_id: &str, host: &str, target: &str, from: Option<&str>, poll: Duration, max: Duration) -> Value {
    let log_path = app.data_dir.join(LOG_FILE);
    let base = json!({"update_id": update_id, "host": host, "kind": "codex", "target_version": target, "log_path": log_path, "recovered": true});
    let done = |ok: bool, extra: Value| {
        let mut v = base.clone();
        v["ok"] = json!(ok);
        if let Some(f) = from {
            v["from"] = json!(f);
        }
        merge(&mut v, extra);
        v
    };
    let finish = |v: Value| async move {
        finish_row(app, update_id, &v).await;
        log_line(app, &format!("[{update_id}] 重啟後接手收尾：ok={} {} {}", v["ok"], v["reason"], v["error"]));
        app.emit("cli_update_done", v.clone()).await;
        v
    };
    set_phase(app, update_id, "recovering", None).await;
    let mut progress = base.clone();
    progress["phase"] = json!("installing");
    app.emit("cli_update_progress", progress).await;

    let deadline = tokio::time::Instant::now() + max;
    loop {
        let Some(fence) = app.hosts.fence(host).await else {
            return finish(done(false, json!({"reason": "superseded", "error": format!("主機 {host} 已經不在設定裡；上一次的安裝有沒有跑完要去那台看")}))).await;
        };
        match runner.installer_busy(app, host, &fence).await {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => tracing::debug!(host, error = %e, "could not probe the codex install lock yet"),
        }
        if tokio::time::Instant::now() >= deadline {
            return finish(done(false, json!({"reason": "interrupted",
                "error": format!("daemon 重啟前開始的安裝，等了 {} 分鐘 {host} 上的安裝還沒結束（或連不上）；沒有重啟任何 bot。那台的安裝還在跑的話再按會被擋下", max.as_secs() / 60)})))
            .await;
        }
        tokio::time::sleep(poll).await;
    }
    let after = runner.version(app, host).await.ok().and_then(|raw| normalized(&raw));
    let Some(after) = after.clone().filter(|a| parse_version(a) >= parse_version(target)) else {
        return finish(done(false, json!({"reason": "interrupted", "to": after,
            "error": format!("daemon 在安裝途中重啟，{host} 的 codex 現在是 {}，沒到確認的 {target}；沒有重啟任何 bot，要裝就再按一次", after.as_deref().unwrap_or("（讀不到）"))})))
        .await;
    };
    // 跑著的版本優先取通知寫的起點（`mark_installed` 自己會找）；這裡的 `from` 只是最後的退路。
    let notices = mark_installed(app, host, from.unwrap_or(""), &after).await;
    crate::update_watch::forget_disk_version(host, "codex").await;
    finish(done(true, json!({"to": after, "notices_updated": notices, "restart": null,
        "restart_error": "daemon 在安裝途中重啟過，這次沒有自動重啟 bot；按一般的 ⌃⌃ 重啟套用"})))
    .await
}

/// 那台主機上還寫著「需安裝」的 codex run 與它的通知。
async fn pending_runs(app: &Arc<App>, host: &str) -> Vec<(crate::db::Run, String)> {
    let mut out = Vec::new();
    for run in crate::db::all_active_runs(&app.db).await.unwrap_or_default() {
        let Some(notice) = run.update_notice.clone().filter(|t| t.contains("需安裝")) else { continue };
        match crate::db::bot(&app.db, &run.bot_id).await {
            Ok(Some(b)) if b.kind == "codex" => {}
            _ => continue,
        }
        if crate::db::bot_host(&app.db, &run.bot_id).await.ok().as_deref() != Some(host) {
            continue;
        }
        out.push((run, notice));
    }
    out
}

/// daemon 眼中那台現在要裝的版本：「需安裝」通知裡最新的目標（確認框取同一個，`codexInstallPlan`）。沒有就是 `None`。
async fn pending_target(app: &Arc<App>, host: &str) -> Option<String> {
    pending_runs(app, host)
        .await
        .iter()
        .filter_map(|(_, n)| crate::codex_update::pending_to(n))
        .max_by(|a, b| parse_version(a).cmp(&parse_version(b)))
}

fn merge(v: &mut Value, extra: Value) {
    if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
        o.extend(e);
    }
}

/// `codex-cli 0.157.0` → `0.157.0`；看不出版本是 `None`。
fn normalized(raw: &str) -> Option<String> {
    cli_version_string(raw).or_else(|| version_string(raw.trim()))
}

fn log_line(app: &App, line: &str) {
    let path = app.data_dir.join(LOG_FILE);
    let res = std::fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| writeln!(f, "{} {line}", crate::db::now()));
    if let Err(e) = res {
        tracing::warn!(path = %path.display(), error = %e, "could not write the cli-update log");
    }
}

/// 那台主機上還寫著「需安裝」的 codex run 改成「已安裝，重啟套用」（批次只收這一種）。回改了幾筆。
/// 跑著的版本：記憶體裡看過的 → 通知寫的起點 → 安裝前的磁碟版本（這個 process 是裝之前起的，不會比它新）。
async fn mark_installed(app: &Arc<App>, host: &str, before: &str, after: &str) -> usize {
    let mut n = 0;
    for (run, notice) in pending_runs(app, host).await {
        let running = crate::codex_update::remember_running(&run.id, "", None)
            .or_else(|| crate::codex_update::pending_from(&notice))
            .unwrap_or_else(|| before.to_string());
        let Some(text) = crate::codex_update::installed_text(after, &running) else { continue };
        // 只換掉讀到的那一句：這之間巡邏改過就讓巡邏的為準。
        let res = sqlx::query("UPDATE runs SET update_notice = ? WHERE id = ? AND update_notice = ?")
            .bind(&text)
            .bind(&run.id)
            .bind(&notice)
            .execute(&app.db)
            .await;
        if res.is_ok_and(|r| r.rows_affected() == 1) {
            n += 1;
            app.emit_bot_status(&run.bot_id).await;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::sync::Mutex;

    /// 假執行器：版本照順序吐、安裝照設定成功或失敗、批次只記下被叫了幾次。**不碰真的 codex**。
    struct Fake {
        versions: Mutex<Vec<anyhow::Result<String>>>,
        install: Result<String, InstallError>,
        /// `installer_busy` 照順序吐；吐完就是「沒人拿著」。
        busy: Mutex<Vec<bool>>,
        probes: Mutex<usize>,
        installs: Mutex<usize>,
        restarts: Mutex<Vec<(String, String)>>,
        /// 安裝卡住多久（測並發用）。
        hold: Duration,
        /// 有的話，安裝等到它放行才回（測途中換主機用）。
        gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl Fake {
        fn new(versions: &[&str], install: Result<&str, &str>) -> Arc<Self> {
            Arc::new(Self {
                versions: Mutex::new(versions.iter().map(|v| Ok(v.to_string())).collect()),
                install: install.map(str::to_string).map_err(|e| InstallError::Failed(e.to_string())),
                busy: Mutex::new(Vec::new()),
                probes: Mutex::new(0),
                installs: Mutex::new(0),
                restarts: Mutex::new(Vec::new()),
                hold: Duration::ZERO,
                gate: Mutex::new(None),
            })
        }
        fn restarts(&self) -> Vec<(String, String)> {
            self.restarts.lock().unwrap().clone()
        }
        fn installs(&self) -> usize {
            *self.installs.lock().unwrap()
        }
    }

    impl Runner for Fake {
        fn installer_busy<'a>(&'a self, _app: &'a Arc<App>, _host: &'a str, _fence: &'a HostFence) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async move {
                *self.probes.lock().unwrap() += 1;
                let mut b = self.busy.lock().unwrap();
                Ok(if b.is_empty() { false } else { b.remove(0) })
            })
        }
        fn install<'a>(&'a self, _app: &'a Arc<App>, _host: &'a str, _fence: &'a HostFence) -> BoxFuture<'a, Result<String, InstallError>> {
            Box::pin(async move {
                *self.installs.lock().unwrap() += 1;
                if !self.hold.is_zero() {
                    tokio::time::sleep(self.hold).await;
                }
                let gate = self.gate.lock().unwrap().take();
                if let Some(gate) = gate {
                    gate.await.ok();
                }
                self.install.clone()
            })
        }
        fn version<'a>(&'a self, _app: &'a Arc<App>, _host: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
            Box::pin(async move {
                let mut v = self.versions.lock().unwrap();
                if v.is_empty() {
                    Err(anyhow::anyhow!("no more versions"))
                } else {
                    v.remove(0)
                }
            })
        }
        fn restart<'a>(&'a self, _app: &'a Arc<App>, scope: Scope) -> BoxFuture<'a, anyhow::Result<Value>> {
            Box::pin(async move {
                self.restarts.lock().unwrap().push((scope.kind, scope.host));
                Ok(json!({"batch_id": "b-1", "total": 1, "planned": [{"bot_id": "x", "name": "cx"}], "skipped": []}))
            })
        }
    }

    async fn codex_bot_with_notice(env: &crate::testing::Env, name: &str, notice: &str) -> (String, String) {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET update_notice=? WHERE id=?").bind(notice).bind(&run).execute(&env.app.db).await.unwrap();
        (bot.id, run)
    }

    async fn notice_of(app: &Arc<App>, run: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>("SELECT update_notice FROM runs WHERE id=?").bind(run).fetch_one(&app.db).await.unwrap()
    }

    fn done_events(rx: &mut tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if ev.kind == "cli_update_done" {
                out.push(ev.data);
            }
        }
        out
    }

    #[tokio::test]
    async fn a_successful_install_flips_the_notice_and_starts_the_codex_batch_for_that_host() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let fake = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("installed"));
        let mut rx = env.app.subscribe();

        let v = super::run(&env.app, fake.as_ref(), "local", "u-1", "0.157.0").await;

        assert_eq!(v["ok"], true, "{v}");
        assert_eq!((v["from"].as_str(), v["to"].as_str()), (Some("0.155.1"), Some("0.157.0")));
        assert_eq!(fake.restarts(), [("codex".to_string(), "local".to_string())], "裝好要接著開那台 codex 的批次");
        assert_eq!(v["restart"]["batch_id"], "b-1");
        let n = notice_of(&env.app, &run).await.unwrap();
        assert!(n.contains("已安裝") && n.contains("0.157.0") && !n.contains("需安裝"), "批次只收「已安裝」：{n}");
        assert_eq!(v["notices_updated"], 1);
        assert_eq!(done_events(&mut rx).len(), 1, "結果要推事件");
        let log = std::fs::read_to_string(env.app.data_dir.join(LOG_FILE)).unwrap();
        assert!(log.contains("installed") && log.contains(CODEX_INSTALL), "輸出寫 log：{log}");
    }

    #[tokio::test]
    async fn a_failed_install_restarts_nothing_and_says_why() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let fake = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Err("curl: (6) Could not resolve host"));
        let mut rx = env.app.subscribe();

        let v = super::run(&env.app, fake.as_ref(), "local", "u-2", "0.157.0").await;

        assert_eq!(v["ok"], false);
        assert_eq!(v["reason"], "install_failed");
        assert!(v["error"].as_str().unwrap().contains("Could not resolve host"), "{v}");
        assert!(fake.restarts().is_empty(), "安裝失敗不能重啟任何 bot");
        assert_eq!(notice_of(&env.app, &run).await, Some(pending), "通知不動");
        assert_eq!(done_events(&mut rx)[0]["reason"], "install_failed");
    }

    #[tokio::test]
    async fn an_install_that_leaves_the_version_unchanged_restarts_nothing() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let fake = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.155.1"], Ok("ok"));

        let v = super::run(&env.app, fake.as_ref(), "local", "u-3", "0.157.0").await;

        assert_eq!(v["reason"], "version_unchanged", "{v}");
        assert!(fake.restarts().is_empty(), "版本沒變不能重啟");
        assert_eq!(notice_of(&env.app, &run).await, Some(pending));
    }

    #[tokio::test]
    async fn unreadable_versions_before_or_after_restart_nothing() {
        let env = crate::testing::env().await;
        let fake = Fake::new(&[], Ok("ok"));
        let v = super::run(&env.app, fake.as_ref(), "local", "u-4", "0.157.0").await;
        assert_eq!(v["reason"], "version_unreadable");
        assert_eq!(fake.installs(), 0, "不知道現在的版本就不裝");

        let fake = Fake::new(&["codex-cli 0.155.1"], Ok("ok"));
        let v = super::run(&env.app, fake.as_ref(), "local", "u-5", "0.157.0").await;
        assert_eq!(v["reason"], "verify_failed");
        assert!(fake.restarts().is_empty());
    }

    /// 別台主機的 codex、claude、已經不是「需安裝」的通知都不動。
    #[tokio::test]
    async fn only_codex_runs_on_that_host_that_still_need_the_install_are_flipped() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_a, here) = codex_bot_with_notice(&env, "cx-here", &pending).await;
        let (claude_bot, claude_run) = codex_bot_with_notice(&env, "cl", "Update installed · Restart to update").await;
        sqlx::query("UPDATE bots SET kind='claude' WHERE id=?").bind(&claude_bot).execute(&env.app.db).await.unwrap();
        let pid = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/x', 'far', 'far', ?)")
            .bind(&pid)
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let (far_bot, far_run) = codex_bot_with_notice(&env, "cx-far", &pending).await;
        sqlx::query("UPDATE bots SET project_id=? WHERE id=?").bind(&pid).bind(&far_bot).execute(&env.app.db).await.unwrap();

        assert_eq!(mark_installed(&env.app, "local", "0.155.1", "0.157.0").await, 1);
        assert!(notice_of(&env.app, &here).await.unwrap().contains("已安裝"));
        assert_eq!(notice_of(&env.app, &far_run).await, Some(pending), "別台主機的 binary 沒換");
        assert_eq!(notice_of(&env.app, &claude_run).await.as_deref(), Some("Update installed · Restart to update"));
    }

    /// #347：A 機安裝還在跑時同名主機改指到 B（`?confirm=repoint`）。A 裝完之後不能讀 B 的版本當成「裝好了」、
    /// 不能改 B 的通知、不能重啟 B 的 codex，結果要講明作廢。
    #[tokio::test]
    async fn an_update_whose_host_was_repointed_mid_install_touches_nothing_on_the_new_target() {
        let env = crate::testing::env().await;
        let host = "cx-347";
        let cfg = |ssh: &str| crate::config::HostCfg {
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        env.app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let pid = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/x', 'b', ?, ?)")
            .bind(&pid)
            .bind(host)
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (bot, run) = codex_bot_with_notice(&env, "cx-b", &pending).await;
        sqlx::query("UPDATE bots SET project_id=? WHERE id=?").bind(&pid).bind(&bot).execute(&env.app.db).await.unwrap();
        // 第二次讀版本讀到的是 B 已經比較新的 codex：沒有圍籬的話就會被當成「A 升上去了」。
        let fake = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("installed on A"));
        let (release, gate) = tokio::sync::oneshot::channel();
        *fake.gate.lock().unwrap() = Some(gate);

        let (app, f2) = (env.app.clone(), fake.clone());
        let task = tokio::spawn(async move { super::run(&app, f2.as_ref(), host, "u-347", "0.157.0").await });
        assert!(crate::testing::eventually!(fake.installs() == 1), "安裝要先開始");
        env.app.hosts.replace_remote_for_test(&env.app, cfg("target-b")).await;
        release.send(()).unwrap();
        let v = task.await.unwrap();

        assert_eq!(v["ok"], false, "{v}");
        assert_eq!(v["reason"], "superseded", "{v}");
        assert!(fake.restarts().is_empty(), "不能重啟 B 的 codex");
        assert_eq!(notice_of(&env.app, &run).await, Some(pending), "B 的通知不動");
    }

    #[tokio::test]
    async fn a_bot_caller_is_refused_and_only_codex_on_a_known_host_is_accepted() {
        let env = crate::testing::env().await;
        let fake = Fake::new(&[], Ok("ok"));
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Id", "b1".parse().unwrap());
        let r = start(&env.app, &h, "local", Some("codex"), Some("0.157.0"), fake.clone()).await;
        assert!(matches!(r, Err(LcError::Forbidden(ref v)) if v["reason"] == "ui_only"), "帶 bot 身分一律 403");
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Token", "tok".parse().unwrap());
        assert!(matches!(start(&env.app, &h, "local", Some("codex"), Some("0.157.0"), fake.clone()).await, Err(LcError::Forbidden(_))));

        let ui = HeaderMap::new();
        assert!(matches!(start(&env.app, &ui, "local", Some("claude"), Some("0.157.0"), fake.clone()).await, Err(LcError::Bad(_))));
        assert!(matches!(start(&env.app, &ui, "local", None, Some("0.157.0"), fake.clone()).await, Err(LcError::Bad(_))));
        assert!(matches!(start(&env.app, &ui, "nowhere", Some("codex"), Some("0.157.0"), fake.clone()).await, Err(LcError::NotFound(_))));
        assert_eq!(fake.installs(), 0, "被拒絕的請求什麼都不跑");
    }

    #[tokio::test]
    async fn a_second_install_on_the_same_host_is_a_409_until_the_first_finishes() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let slow = Arc::new(Fake {
            hold: Duration::from_millis(300),
            ..Arc::try_unwrap(Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("ok"))).ok().unwrap()
        });
        let ui = HeaderMap::new();
        let first = start(&env.app, &ui, "local", Some("codex"), Some("0.157.0"), slow.clone()).await.expect("第一個開得起來");
        assert!(running_list(&env.app).await.iter().any(|r| r["update_id"] == first["update_id"]), "state 看得到在跑的那一個");

        let second = start(&env.app, &ui, "local", Some("codex"), Some("0.157.0"), Fake::new(&[], Ok("ok"))).await;
        match second {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v["reason"], "cli_update_in_progress", "{v}");
                assert_eq!(v["update_id"], first["update_id"]);
            }
            other => panic!("同一台同時只准一個：{other:?}"),
        }
        assert!(crate::testing::eventually!(running_list(&env.app).await.is_empty()), "跑完就放掉");
        assert_eq!(slow.restarts().len(), 1);
        let id = first["update_id"].as_str().unwrap();
        let (st, res) = row_status(&env.app, id).await;
        assert_eq!((st.as_str(), res["ok"].clone(), res["to"].clone()), ("done", json!(true), json!("0.157.0")), "結果寫進那一列：{res}");
        // 第一次裝好會把通知改成「已安裝」；再開一次要有新的「需安裝」目標。
        sqlx::query("UPDATE runs SET update_notice=? WHERE id=?").bind(&pending).bind(&run).execute(&env.app.db).await.unwrap();
        start(&env.app, &ui, "local", Some("codex"), Some("0.157.0"), Fake::new(&[], Ok("ok"))).await.expect("放掉之後可以再開");
    }

    /// #569：升了但沒到確認框寫的那一版，不是使用者核准的更新——不改通知、一顆都不重啟。
    #[tokio::test]
    async fn an_install_below_the_approved_target_restarts_nothing() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.0"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let fake = Fake::new(&["codex-cli 0.155.0", "codex-cli 0.156.0"], Ok("ok"));
        let mut rx = env.app.subscribe();

        let v = super::run(&env.app, fake.as_ref(), "local", "u-6", "0.157.0").await;

        assert_eq!(v["ok"], false, "{v}");
        assert_eq!(v["reason"], "target_not_reached", "{v}");
        assert_eq!((v["from"].as_str(), v["to"].as_str(), v["target_version"].as_str()), (Some("0.155.0"), Some("0.156.0"), Some("0.157.0")));
        assert_eq!(fake.installs(), 1);
        assert!(fake.restarts().is_empty(), "沒到目標不能重啟");
        assert_eq!(notice_of(&env.app, &run).await, Some(pending), "通知維持「需安裝」");
        assert_eq!(done_events(&mut rx)[0]["reason"], "target_not_reached");
    }

    /// 裝到比目標還新（安裝指令裝的是最新版，帳本還沒追上）：已經涵蓋使用者核准的那一版，照成功走。
    #[tokio::test]
    async fn an_install_past_the_target_is_accepted() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.0"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let fake = Fake::new(&["codex-cli 0.155.0", "codex-cli 0.158.0"], Ok("ok"));

        let v = super::run(&env.app, fake.as_ref(), "local", "u-7", "0.157.0").await;

        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["to"], "0.158.0");
        assert_eq!(fake.restarts().len(), 1);
        assert!(notice_of(&env.app, &run).await.unwrap().contains("已安裝"));
    }

    /// 磁碟上已經是目標版本：不再跑安裝指令，只改通知、開重啟。
    #[tokio::test]
    async fn a_disk_already_at_the_target_skips_the_installer() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.0"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let fake = Fake::new(&["codex-cli 0.157.0"], Ok("ok"));

        let v = super::run(&env.app, fake.as_ref(), "local", "u-8", "0.157.0").await;

        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["already_installed"], true);
        assert_eq!(fake.installs(), 0, "已經裝好就不再跑 curl | sh");
        assert_eq!(fake.restarts().len(), 1, "跑著的還是舊版，照樣要重啟套用");
        assert!(notice_of(&env.app, &run).await.unwrap().contains("已安裝"));
    }

    /// 請求帶的版本要等於 daemon 眼中那台的目標：沒帶 400、不一樣或已經沒有「需安裝」都是 409 `stale_target`，什麼都不跑。
    #[tokio::test]
    async fn a_target_that_does_not_match_the_pending_notice_is_refused() {
        let env = crate::testing::env().await;
        let ui = HeaderMap::new();
        let fake = Fake::new(&["codex-cli 0.155.0", "codex-cli 0.157.0"], Ok("ok"));

        let none = start(&env.app, &ui, "local", Some("codex"), Some("0.157.0"), fake.clone()).await;
        assert!(matches!(none, Err(LcError::Conflict(ref v)) if v["reason"] == "stale_target" && v["current_target"].is_null()), "{none:?}");

        let older = crate::codex_update::pending_text(Some("0.155.0"), "0.156.1");
        codex_bot_with_notice(&env, "cx-a", &older).await;
        let newer = crate::codex_update::pending_text(Some("0.155.0"), "0.157.0");
        codex_bot_with_notice(&env, "cx-b", &newer).await;

        assert!(matches!(start(&env.app, &ui, "local", Some("codex"), None, fake.clone()).await, Err(LcError::Bad(_))), "沒帶目標 400");
        match start(&env.app, &ui, "local", Some("codex"), Some("0.156.1"), fake.clone()).await {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v["reason"], "stale_target", "{v}");
                assert_eq!(v["current_target"], "0.157.0", "目標取那台「需安裝」裡最新的");
            }
            other => panic!("舊的目標要擋：{other:?}"),
        }
        assert_eq!(fake.installs(), 0, "被拒絕的請求什麼都不跑");

        let ok = start(&env.app, &ui, "local", Some("codex"), Some("0.157.0"), fake.clone()).await.expect("跟最新目標一致就開");
        assert_eq!(ok["target_version"], "0.157.0");
        assert!(crate::testing::eventually!(running_list(&env.app).await.is_empty()));
        assert_eq!(fake.installs(), 1);
    }

    /// 上一顆 daemon 寫下、還沒收尾的那一列（它的行程已經不在了，`boot` 不是現在這顆）。
    async fn orphan_row(app: &Arc<App>, host: &str, target: &str, phase: &str, from: Option<&str>) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO cli_updates (id, host, kind, target_version, status, phase, from_version, boot, started_at, updated_at)
             VALUES (?, ?, 'codex', ?, 'running', ?, ?, 'dead-daemon', ?, ?)",
        )
        .bind(&id)
        .bind(host)
        .bind(target)
        .bind(phase)
        .bind(from)
        .bind(db::now())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        id
    }

    async fn row_status(app: &Arc<App>, id: &str) -> (String, Value) {
        let (st, res): (String, Option<String>) =
            sqlx::query_as("SELECT status, result FROM cli_updates WHERE id = ?").bind(id).fetch_one(&app.db).await.unwrap();
        (st, res.map(|r| serde_json::from_str(&r).unwrap()).unwrap_or(Value::Null))
    }

    /// #564 回歸：安裝跑到一半 daemon 被砍（行程內的狀態全沒了）、遠端的 `curl | sh` 還在跑。新的 daemon 開在同一個 DB 上：
    /// 再按一次不能開第二個安裝、state 要列得出那一筆；那台的安裝結束後接手收成終局，之後刻意再裝一次照常可以。
    #[tokio::test]
    async fn a_daemon_restart_mid_install_launches_no_second_installer_and_reconciles() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let orphan = orphan_row(&env.app, "local", "0.157.0", "installing", Some("0.155.1")).await;
        let ui = HeaderMap::new();

        let retry = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("ok"));
        match start(&env.app, &ui, "local", Some("codex"), Some("0.157.0"), retry.clone()).await {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v["reason"], "cli_update_in_progress", "{v}");
                assert_eq!(v["update_id"], orphan.as_str());
                assert_eq!(v["recovered"], true, "{v}");
            }
            other => panic!("重啟前的安裝還沒收尾，不能再開一次：{other:?}"),
        }
        assert_eq!(retry.installs(), 0, "不能再發第二個安裝指令");
        let listed = running_list(&env.app).await;
        assert!(listed.iter().any(|r| r["update_id"] == orphan.as_str() && r["recovered"] == true), "state 要看得到接手中的那筆：{listed:?}");

        // 那台的安裝鎖還被上一個安裝拿著兩輪，第三輪放掉；那時磁碟上已經是新版。
        let rec = Fake::new(&["codex-cli 0.157.0"], Ok("should not run"));
        *rec.busy.lock().unwrap() = vec![true, true];
        let mut rx = env.app.subscribe();
        let v = recover(&env.app, rec.as_ref(), &orphan, "local", "0.157.0", Some("0.155.1"), Duration::from_millis(1), Duration::from_secs(5)).await;

        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["recovered"], true);
        assert_eq!(*rec.probes.lock().unwrap(), 3, "鎖放掉之前一直等");
        assert_eq!(rec.installs(), 0, "接手絕對不跑安裝指令");
        assert!(rec.restarts().is_empty(), "隔了一次重啟，不自動開重啟");
        assert!(notice_of(&env.app, &run).await.unwrap().contains("已安裝"), "裝好了要改通知，一般的重啟才收得到");
        let (st, res) = row_status(&env.app, &orphan).await;
        assert_eq!((st.as_str(), res["ok"].clone()), ("done", json!(true)), "終局寫進 DB：{res}");
        assert_eq!(done_events(&mut rx).len(), 1);
        assert!(running_list(&env.app).await.is_empty(), "收尾後放掉這台");

        // 之後刻意再裝一次（新的目標）照常開。
        let next = crate::codex_update::pending_text(Some("0.157.0"), "0.158.0");
        sqlx::query("UPDATE runs SET update_notice=? WHERE id=?").bind(&next).bind(&run).execute(&env.app.db).await.unwrap();
        let again = Fake::new(&["codex-cli 0.157.0", "codex-cli 0.158.0"], Ok("ok"));
        start(&env.app, &ui, "local", Some("codex"), Some("0.158.0"), again.clone()).await.expect("收尾之後可以再裝");
        assert!(crate::testing::eventually!(running_list(&env.app).await.is_empty()));
        assert_eq!(again.installs(), 1);
    }

    /// #564：裝好之後、確認版本／重啟之前 daemon 死掉。接手時讀到已經是目標版本：直接認，不先重跑一次安裝。
    #[tokio::test]
    async fn a_crash_after_the_install_succeeded_is_recognised_without_reinstalling() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let orphan = orphan_row(&env.app, "local", "0.157.0", "verifying", Some("0.155.1")).await;
        let rec = Fake::new(&["codex-cli 0.157.0"], Ok("should not run"));

        let v = recover(&env.app, rec.as_ref(), &orphan, "local", "0.157.0", Some("0.155.1"), Duration::from_millis(1), Duration::from_secs(5)).await;

        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["to"], "0.157.0");
        assert_eq!(rec.installs(), 0);
        assert!(notice_of(&env.app, &run).await.unwrap().contains("已安裝"));
        assert_eq!(row_status(&env.app, &orphan).await.0, "done");
    }

    /// 接手時版本沒到目標、或等到上限鎖還在：收成 `interrupted`，通知不動、一顆都不重啟，這台放掉讓人再按。
    #[tokio::test]
    async fn an_orphan_that_did_not_reach_the_target_or_never_released_is_interrupted() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;

        let orphan = orphan_row(&env.app, "local", "0.157.0", "installing", Some("0.155.1")).await;
        let rec = Fake::new(&["codex-cli 0.155.1"], Ok("x"));
        let v = recover(&env.app, rec.as_ref(), &orphan, "local", "0.157.0", Some("0.155.1"), Duration::from_millis(1), Duration::from_secs(5)).await;
        assert_eq!((v["ok"].clone(), v["reason"].clone()), (json!(false), json!("interrupted")), "{v}");
        assert_eq!(notice_of(&env.app, &run).await, Some(pending.clone()), "通知不動");
        let (st, res) = row_status(&env.app, &orphan).await;
        assert_eq!((st.as_str(), res["reason"].as_str()), ("failed", Some("interrupted")));

        let stuck = orphan_row(&env.app, "local", "0.157.0", "installing", None).await;
        let rec = Fake::new(&["codex-cli 0.157.0"], Ok("x"));
        *rec.busy.lock().unwrap() = vec![true; 1000];
        let v = recover(&env.app, rec.as_ref(), &stuck, "local", "0.157.0", None, Duration::from_millis(1), Duration::from_millis(30)).await;
        assert_eq!(v["reason"], "interrupted", "{v}");
        assert!(v["error"].as_str().unwrap().contains("還沒結束"), "{v}");
        assert_eq!(rec.installs() + rec.restarts().len(), 0);
        assert_eq!(notice_of(&env.app, &run).await, Some(pending), "等不到就什麼都不改");
        assert!(running_list(&env.app).await.is_empty(), "放掉這台：真的還在跑的話由主機端的鎖擋");
    }

    /// 主機端的鎖被別人拿著（別的實例、或上一顆 daemon 開的安裝還在跑）：這次沒有跑安裝，`already_running`、一顆都不重啟。
    #[tokio::test]
    async fn a_host_lock_held_by_another_installer_is_already_running_not_install_failed() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        let fake = Arc::new(Fake {
            install: Err(classify_install_error(format!("安裝指令失敗（exit status: 75）：{LOCKED_MARK} pid=4242"))),
            ..Arc::try_unwrap(Fake::new(&["codex-cli 0.155.1"], Ok("x"))).ok().unwrap()
        });

        let v = super::run(&env.app, fake.as_ref(), "local", "u-9", "0.157.0").await;

        assert_eq!(v["reason"], "already_running", "{v}");
        assert!(v["error"].as_str().unwrap().contains("pid=4242"), "{v}");
        assert!(fake.restarts().is_empty());
        assert_eq!(notice_of(&env.app, &run).await, Some(pending));
    }

    /// 真的 `/bin/sh` 跑鎖的腳本（裡面的指令換成 sleep／echo，絕不跑 curl）：拿著的時候第二個不跑、退 75；
    /// 探測看得到；跑完放掉；主人死掉留下的過期鎖會被拿走；裡面指令的失敗照樣傳出來。
    #[tokio::test]
    async fn the_host_lock_script_excludes_a_second_installer_and_recovers_a_stale_lock() {
        let dir = std::env::temp_dir().join(format!("am-cli-lock-{}", db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("codex-install.lock").display().to_string();
        let marker = dir.join("second-ran");

        let first = tokio::process::Command::new("/bin/sh").arg("-c").arg(locked_script(&lock, "sleep 1")).spawn().unwrap();
        assert!(crate::testing::eventually!(std::fs::symlink_metadata(&lock).is_ok()), "第一個要先拿到鎖");
        assert!(parse_probe(&local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap()).unwrap(), "拿著的時候探測是 busy");

        let second = local_sh(&locked_script(&lock, &format!("touch '{}'", marker.display())), Duration::from_secs(5)).await;
        let err = second.expect_err("鎖被拿著，第二個不能跑");
        assert!(err.contains(LOCKED_MARK) && err.contains("75"), "{err}");
        assert!(matches!(classify_install_error(err), InstallError::Locked(_)));
        assert!(!marker.exists(), "第二個的指令一行都沒跑");

        assert!(first.wait_with_output().await.unwrap().status.success());
        assert!(std::fs::symlink_metadata(&lock).is_err(), "跑完放掉鎖");
        assert!(!parse_probe(&local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap()).unwrap());

        // 主人被 SIGKILL（trap 沒跑到）留下的鎖：pid 已經不在，當過期拿走。
        std::os::unix::fs::symlink("99999999", &lock).unwrap();
        local_sh(&locked_script(&lock, &format!("touch '{}'", marker.display())), Duration::from_secs(5)).await.expect("過期的鎖要拿得走");
        assert!(marker.exists());
        assert!(std::fs::symlink_metadata(&lock).is_err());

        let failed = local_sh(&locked_script(&lock, "echo boom >&2; exit 6"), Duration::from_secs(5)).await.expect_err("裡面失敗要傳出來");
        assert!(failed.contains("boom") && !failed.contains(LOCKED_MARK), "{failed}");
        assert!(matches!(classify_install_error(failed), InstallError::Failed(_)));
        assert!(std::fs::symlink_metadata(&lock).is_err(), "失敗也放掉鎖");
        std::fs::remove_dir_all(&dir).ok();
    }
}
