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
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// codex 自己的升級提示寫的那一句（`Run sh -c '…' to update.`）。只有這一條，不收參數。
pub const CODEX_INSTALL: &str = "curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh";

/// 下載＋解壓一般半分鐘內；網路慢給到五分鐘，再久就是卡住了。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

pub const LOG_FILE: &str = "cli-update.log";

/// 失敗時回給畫面的輸出只留尾巴，全文在 log。
const TAIL_CHARS: usize = 1500;

/// 會動到機器的三件事。正式版是 [`Real`]；測試換成假的。
pub trait Runner: Send + Sync {
    /// 跑安裝指令。`Ok` 是輸出，`Err` 是給人看的原因（含輸出尾巴）。
    fn install<'a>(&'a self, app: &'a Arc<App>, host: &'a str) -> BoxFuture<'a, Result<String, String>>;
    /// 那台主機現在的 `codex --version` 原文。
    fn version<'a>(&'a self, app: &'a Arc<App>, host: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;
    /// 開那台主機 codex 的一鍵重啟，回計畫（`POST /api/bots/restart-idle` 同一份形狀）。
    fn restart<'a>(&'a self, app: &'a Arc<App>, scope: Scope) -> BoxFuture<'a, anyhow::Result<Value>>;
}

pub struct Real;

impl Runner for Real {
    fn install<'a>(&'a self, app: &'a Arc<App>, host: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if host == crate::config::LOCAL_HOST {
                let child = tokio::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(CODEX_INSTALL)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("跑不起安裝指令：{e}"))?;
                let out = tokio::time::timeout(INSTALL_TIMEOUT, child.wait_with_output())
                    .await
                    .map_err(|_| format!("安裝指令超過 {} 秒沒結束，已中止", INSTALL_TIMEOUT.as_secs()))?
                    .map_err(|e| format!("等安裝指令結束失敗：{e}"))?;
                let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                if out.status.success() {
                    Ok(text)
                } else {
                    Err(format!("安裝指令失敗（{}）：{}", out.status, tail(&text)))
                }
            } else {
                let conn = app.hosts.get(host).await.ok_or_else(|| format!("不認得主機 `{host}`"))?;
                // 遠端逾時只砍得掉本機這條 ssh；那邊的安裝可能還在跑，所以錯誤要講清楚、請人去那台看。
                conn.ssh_exec_path_timeout(CODEX_INSTALL, INSTALL_TIMEOUT)
                    .await
                    .map_err(|e| format!("在 {host} 安裝失敗（逾時的話那台的安裝可能還在跑）：{}", tail(&format!("{e:#}"))))
            }
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

#[derive(Debug, Clone)]
struct Running {
    update_id: String,
    host: String,
    kind: String,
    target: String,
}

/// 鍵是 `<data_dir>|<host>`：同一台同時只准一個安裝（兩個 `curl | sh` 疊在一起會互相覆蓋 `current` 連結）。
fn running() -> &'static Mutex<HashMap<String, Running>> {
    static M: OnceLock<Mutex<HashMap<String, Running>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn slot_key(app: &App, host: &str) -> String {
    format!("{}|{host}", app.data_dir.display())
}

/// `GET /api/state` 的 `cli_updates`：現在在跑的安裝。進度只走 WS，`cli_update_done` 收不到時前端靠這一格對帳（同 #492 的 `restart_batch`）。
pub fn running_list(data_dir: &std::path::Path) -> Vec<Value> {
    let prefix = format!("{}|", data_dir.display());
    let Ok(m) = running().lock() else { return Vec::new() };
    m.iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(_, r)| json!({"update_id": r.update_id, "host": r.host, "kind": r.kind, "target_version": r.target}))
        .collect()
}

/// 安裝結束（含 panic）就放掉。
struct Slot(String);

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut m) = running().lock() {
            m.remove(&self.0);
        }
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
    let key = slot_key(app, host);
    let update_id = crate::db::ulid();
    {
        let mut m = running().lock().unwrap();
        if let Some(r) = m.get(&key) {
            return Err(LcError::conflict(
                "cli_update_in_progress",
                json!({"host": host, "kind": r.kind, "update_id": r.update_id, "message": format!("{host} 已經在安裝 {}，這一下沒有再開一次", r.kind)}),
            ));
        }
        m.insert(key.clone(), Running { update_id: update_id.clone(), host: host.to_string(), kind: "codex".into(), target: target.clone() });
    }
    let slot = Slot(key);
    let (app2, host2, id2, target2) = (app.clone(), host.to_string(), update_id.clone(), target.clone());
    tokio::spawn(async move {
        let _slot = slot;
        run(&app2, runner.as_ref(), &host2, &id2, &target2).await;
    });
    Ok(json!({"update_id": update_id, "host": host, "kind": "codex", "target_version": target, "started": true}))
}

/// 一次安裝的結果（也是 `cli_update_done` 的內容）。
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
        if v["ok"] == json!(false) {
            tracing::warn!(host, reason = %v["reason"], error = %v["error"], "codex 升級沒有完成，沒有重啟任何 bot");
            log_line(app, &format!("[{update_id}] 結束：失敗 {} {}", v["reason"], v["error"]));
        }
        app.emit("cli_update_done", v.clone()).await;
        v
    };

    app.emit("cli_update_progress", progress("checking", json!({}))).await;
    let before = match runner.version(app, host).await.map(|raw| normalized(&raw)) {
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
        app.emit("cli_update_progress", progress("installing", json!({"from": before}))).await;
        log_line(app, &format!("[{update_id}] {host}：codex {before}，目標 {target}，執行 {CODEX_INSTALL}"));
        match runner.install(app, host).await {
            Ok(out) => log_line(app, &format!("[{update_id}] 安裝輸出：\n{}", out.trim_end())),
            Err(e) => {
                log_line(app, &format!("[{update_id}] 安裝失敗：\n{e}"));
                return finish(done(false, json!({"reason": "install_failed", "error": e, "from": before}))).await;
            }
        }
        app.emit("cli_update_progress", progress("verifying", json!({"from": before}))).await;
        let after = match runner.version(app, host).await.map(|raw| normalized(&raw)) {
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
    let notices = mark_installed(app, host, &before, &after).await;
    crate::update_watch::forget_disk_version(host, "codex").await;
    app.emit("cli_update_progress", progress("restarting", json!({"from": before, "to": after}))).await;
    let restart = runner.restart(app, Scope { kind: "codex".into(), host: host.to_string() }).await;
    let v = match restart {
        Ok(plan) => done(true, json!({"from": before, "to": after, "already_installed": already, "notices_updated": notices, "restart": plan})),
        // 裝好了但批次開不起來：新版已經在磁碟上，通知也改成「重啟套用」了，照一般的一鍵重啟再按一次就好。
        Err(e) => done(true, json!({"from": before, "to": after, "already_installed": already, "notices_updated": notices,
            "restart": null, "restart_error": format!("{e:#}")})),
    };
    finish(v).await
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

    /// 假執行器：版本照順序吐、安裝照設定成功或失敗、批次只記下被叫了幾次。**不碰真的 codex**。
    struct Fake {
        versions: Mutex<Vec<anyhow::Result<String>>>,
        install: Result<String, String>,
        installs: Mutex<usize>,
        restarts: Mutex<Vec<(String, String)>>,
        /// 安裝卡住多久（測並發用）。
        hold: Duration,
    }

    impl Fake {
        fn new(versions: &[&str], install: Result<&str, &str>) -> Arc<Self> {
            Arc::new(Self {
                versions: Mutex::new(versions.iter().map(|v| Ok(v.to_string())).collect()),
                install: install.map(str::to_string).map_err(str::to_string),
                installs: Mutex::new(0),
                restarts: Mutex::new(Vec::new()),
                hold: Duration::ZERO,
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
        fn install<'a>(&'a self, _app: &'a Arc<App>, _host: &'a str) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                *self.installs.lock().unwrap() += 1;
                if !self.hold.is_zero() {
                    tokio::time::sleep(self.hold).await;
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
        assert!(running_list(&env.app.data_dir).iter().any(|r| r["update_id"] == first["update_id"]), "state 看得到在跑的那一個");

        let second = start(&env.app, &ui, "local", Some("codex"), Some("0.157.0"), Fake::new(&[], Ok("ok"))).await;
        match second {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v["reason"], "cli_update_in_progress", "{v}");
                assert_eq!(v["update_id"], first["update_id"]);
            }
            other => panic!("同一台同時只准一個：{other:?}"),
        }
        assert!(crate::testing::eventually!(running_list(&env.app.data_dir).is_empty()), "跑完就放掉");
        assert_eq!(slow.restarts().len(), 1);
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
        assert!(crate::testing::eventually!(running_list(&env.app.data_dir).is_empty()));
        assert_eq!(fake.installs(), 1);
    }
}
