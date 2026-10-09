//! header 一鍵安裝 Claude／Codex CLI 更新（SPEC §6.9、API `POST /api/hosts/{name}/cli-update`）。
//!
//! 上游新版還沒安裝時，header 讓使用者確認目標版本；在主機跑**寫死的**安裝指令 → 重新讀該 CLI 版本確認到達目標 →
//! 把該主機 run 的「需安裝」通知改成「已安裝，重啟套用」。Codex 延續既有 scoped restart；Claude 留給使用者再確認一般重啟。
//!
//! 刻意的邊界：
//! - 只給使用者在 UI 上按：帶 bot 身分的請求一律 403（同 `deploy_now::refuse_bot_caller`）。
//! - 指令不接受呼叫端傳入；同一台同時只跑一個（409）；有逾時；輸出寫 `<data_dir>/cli-update.log`。
//! - 安裝失敗、讀不到版本、或裝完版本沒變：**一顆 bot 都不重啟**，`cli_update_done` 帶 `reason` 講清楚。
//! - 綁定使用者核准的版本（#569）：請求帶確認框寫的 `target_version`，要等於 daemon 眼中那台「需安裝」通知的目標（不然 409
//!   `stale_target`）；Codex 裝完要 `>= target`、Claude 要精確等於 `target`，否則是 `target_not_reached`。
//!   安裝前磁碟已經符合目標就不再跑安裝指令；Claude 不會自動重啟。
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
use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// codex 自己的升級提示寫的那一句（`Run sh -c '…' to update.`）。只有這一條，不收參數。
pub const CODEX_INSTALL: &str = "curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh";
pub const CODEX_INSTALL_LOCK: &str = "$HOME/.agents-manager-codex-install.lock";
pub const CLAUDE_INSTALL_LOCK: &str = "$HOME/.agents-manager-claude-install.lock";

/// 下載＋解壓一般半分鐘內；網路慢給到五分鐘，再久就是卡住了。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

pub const LOG_FILE: &str = "cli-update.log";

/// 失敗時回給畫面的輸出只留尾巴，全文在 log。
const TAIL_CHARS: usize = 1500;

/// 主機端的安裝鎖（#564、#580）：`$HOME` 底下一條 symlink，目標是專用安裝 process group 的 id 與唯一 nonce。
///
/// DB 那一列只擋得住**同一個資料目錄**的 daemon；daemon 被砍掉時遠端的 `curl | sh` 不會跟著停（逾時也只砍得掉本機的
/// ssh），隔離實例也會對同一台開安裝。所以安裝 helper 在主機端建立專用 process group 並持鎖、等待完整安裝指令；外層 SSH
/// wrapper 離開不會帶走安裝。`ln -s` 原子地寫入 `process-group-id:nonce`；probe 查該 group 的存活行程是否仍帶同一個 nonce，
/// 因此 helper 被 SIGKILL 後，只要 installer 子行程還活著就仍算忙，也不會把重用的 PID 誤認成 owner。沒有 nonce 的舊版 PID 鎖
/// 無法驗明 owner，視為過期回收。鎖忙時不裝、印 [`LOCKED_MARK`] 退出 [`LOCKED_EXIT`]；group 內已無 owner 時可回收。
/// 不分實例：換掉的是同一顆 codex。

const LOCKED_EXIT: i32 = 75;
pub(crate) const LOCKED_MARK: &str = "AM_CODEX_INSTALL_LOCKED";

/// 識別新式 `process-group-id:nonce` owner；舊版單 PID symlink 沒有足夠資料驗明身份，視為過期。
/// 比對 command line 上的 nonce，因為 macOS 的 `ps` 不會用 Linux 的 `ps e` 方式輸出環境變數。
fn lock_owner_check() -> &'static str {
    r#"lock_owner_alive() {
  owner=$1
  case "$owner" in *:*) pgid=${owner%%:*}; nonce=${owner#*:} ;; *) return 1 ;; esac
  case "$nonce" in ''|*:* ) return 1 ;; esac
  case "$pgid" in ''|0|*[!0-9]*) return 1 ;; esac
  kill -0 "-$pgid" 2>/dev/null || return 1
  members=$(ps -A -o pid= -o pgid= 2>/dev/null | awk -v pgid="$pgid" '$2 == pgid { print $1 }') || return 1
  for member in $members; do
    cmd=$(ps -ww -p "$member" -o command= 2>/dev/null) || continue
    case "$cmd" in *"AM_CODEX_INSTALL_OWNER=$nonce"*) return 0 ;; esac
  done
  return 1
}"#
}

fn shell_lock_path(lock: &str) -> String {
    match lock.strip_prefix("$HOME/") {
        Some(path) => format!("\"$HOME/{}\"", path.replace('\\', "\\\\").replace('$', "\\$").replace('`', "\\`").replace('"', "\\\"")),
        None => crate::hosts::sh_quote(lock),
    }
}

/// 把 `inner` 包進主機端的安裝鎖（POSIX sh：遠端是 `ssh … /bin/sh -s`，本機是 `/bin/sh -c`）。
/// 鎖被佔走的訊息寫到 stderr：遠端失敗時 `ssh_exec` 只帶 stderr 回來。
pub(crate) fn locked_script(lock: &str, inner: &str) -> String {
    let nonce = crate::db::ulid();
    let lock = shell_lock_path(lock);
    let inner = crate::hosts::sh_quote(inner);
    let worker = format!(
        r#"LOCK_NONCE='{nonce}'
L={lock}
OUT=$2
PARENT_PID=$1
GROUP_ID=$(ps -p "$$" -o pgid= 2>/dev/null | tr -d '[:space:]')
[ "$GROUP_ID" = "$$" ] || {{ echo "Could not isolate installer process group" >&2; exit 70; }}
AM_CODEX_INSTALL_OWNER={nonce}
export AM_CODEX_INSTALL_OWNER
{owner_check}
cleanup() {{
  [ "$(readlink "$L" 2>/dev/null)" = "$GROUP_ID:$LOCK_NONCE" ] && rm -f "$L"
  kill -0 "$PARENT_PID" 2>/dev/null || rm -f "$OUT"
}}
trap cleanup EXIT
trap '' HUP INT TERM
if ! ln -s "$GROUP_ID:$LOCK_NONCE" "$L" 2>/dev/null; then
  p=$(readlink "$L" 2>/dev/null)
  if [ -n "$p" ] && lock_owner_alive "$p"; then echo "{LOCKED_MARK} pid=${{p%%:*}}" >&2; exit {LOCKED_EXIT}; fi
  rm -f "$L"
  ln -s "$GROUP_ID:$LOCK_NONCE" "$L" 2>/dev/null || {{ p=$(readlink "$L" 2>/dev/null); echo "{LOCKED_MARK} pid=${{p%%:*}}" >&2; exit {LOCKED_EXIT}; }}
fi
AM_CODEX_INSTALL_OWNER="$LOCK_NONCE" /bin/sh -c 'trap ":" EXIT; eval "$1"' "AM_CODEX_INSTALL_OWNER=$LOCK_NONCE" {inner}
"#,
        nonce = nonce,
        owner_check = lock_owner_check(),
    );
    let worker = crate::hosts::sh_quote(&worker);
    format!(
        r#"L={lock}
LOCK_NONCE='{nonce}'
OUT=$(mktemp "${{TMPDIR:-/tmp}}/am-codex-install.XXXXXX") || exit 70
umask 077
# Perl forks a child, puts it in its own process group, then execs the lock worker.
AM_CODEX_INSTALL_OWNER="$LOCK_NONCE" nohup perl -e 'my $pid = fork(); die "fork: $!" unless defined $pid; if ($pid) {{ waitpid($pid, 0); my $status = $?; exit(($status & 127) ? 128 + ($status & 127) : $status >> 8); }} setpgrp(0, 0) or die "setpgrp: $!"; exec @ARGV or die "exec: $!";' /bin/sh -c {worker} am-codex-install "$$" "$OUT" > "$OUT" 2>&1 < /dev/null &
worker_pid=$!
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
wait "$worker_pid"
status=$?
cat "$OUT"
rm -f "$OUT"
exit "$status"
"#,
        lock = lock,
        worker = worker,
    )
}

/// 那台的安裝鎖現在有沒有活著的主人：印 `busy <pid>` 或 `free`。
fn probe_script(lock: &str) -> String {
    format!(
        r#"L={lock}; {owner_check}; p=$(readlink "$L" 2>/dev/null); if [ -n "$p" ] && lock_owner_alive "$p"; then echo "busy ${{p%%:*}}"; else echo free; fi"#,
        lock = shell_lock_path(lock),
        owner_check = lock_owner_check(),
    )
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
    if e.contains(LOCKED_MARK) { InstallError::Locked(e) } else { InstallError::Failed(e)
    }
}

fn install_lock(kind: &str) -> Option<&'static str> {
    match kind {
        "codex" => Some(CODEX_INSTALL_LOCK),
        "claude" => Some(CLAUDE_INSTALL_LOCK),
        _ => None,
    }
}

fn claude_install_command(target: &str) -> Option<String> {
    let target = version_string(target)?;
    Some(format!("claude install {target}"))
}

fn install_command(kind: &str, target: &str) -> Option<String> {
    match kind {
        "codex" => Some(CODEX_INSTALL.to_string()),
        "claude" => claude_install_command(target),
        _ => None,
    }
}

fn supported_kind(kind: &str) -> bool {
    matches!(kind, "codex" | "claude")
}

fn verify_durable_kind(
    expected: &str,
    stored: Result<Option<String>, sqlx::Error>,
    update_id: &str,
) -> Result<String, String> {
    if !supported_kind(expected) {
        return Err(format!("cli-update {update_id} 的啟動 kind 無效：{expected:?}"));
    }
    match stored {
        Err(error) => Err(format!("讀取 cli-update {update_id} 的 kind 失敗：{error}")),
        Ok(None) => Err(format!("cli-update {update_id} 的 durable job 列已不存在")),
        Ok(Some(stored)) if stored == expected => Ok(stored),
        Ok(Some(stored)) => Err(format!(
            "cli-update {update_id} 的 kind 不一致：啟動時是 {expected}，durable 列是 {stored}"
        )),
    }
}

/// 會動到機器的四件事。正式版是 [`Real`]；測試換成假的。
pub trait Runner: Send + Sync {
    /// 在主機端的安裝鎖裡跑安裝指令。`Ok` 是輸出，`Err` 是給人看的原因（含輸出尾巴）。遠端一律走 `fence` 記下的那條連線，
    /// 不用主機名重新解析——途中同名主機改指到另一台，安裝也不能跑到新機器上（#347）。
    fn install<'a>(&'a self, app: &'a Arc<App>, host: &'a str,
        kind: &'a str,
        target: &'a str,
        fence: &'a HostFence) -> BoxFuture<'a, Result<String, InstallError>>;
    /// 那台的安裝鎖有沒有活著的主人（重啟後接手孤兒安裝用，#564）。
    fn installer_busy<'a>(&'a self, app: &'a Arc<App>, host: &'a str,
        kind: &'a str,
        fence: &'a HostFence) -> BoxFuture<'a, anyhow::Result<bool>>;
    /// 那台主機現在該 kind 的 `--version` 原文。
    fn version<'a>(&'a self, app: &'a Arc<App>, host: &'a str,
        kind: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<String>>;
    /// 開那台主機該 kind 的一鍵重啟，回計畫（`POST /api/bots/restart-idle` 同一份形狀）。
    fn restart<'a>(&'a self, app: &'a Arc<App>, scope: Scope, fence: &'a HostFence) -> BoxFuture<'a, anyhow::Result<Value>>;
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
    fn install<'a>(&'a self, _app: &'a Arc<App>, host: &'a str,
        kind: &'a str,
        target: &'a str,
        fence: &'a HostFence) -> BoxFuture<'a, Result<String, InstallError>> {
        Box::pin(async move {
            let lock = install_lock(kind)
                .ok_or_else(|| InstallError::Failed(format!("不支援安裝 {kind}")))?;
            let command = install_command(kind, target).ok_or_else(|| {
                InstallError::Failed(format!("{kind} 安裝目標版本不合法：{target}"))
            })?;
            let script = locked_script(lock, &command);
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

    fn installer_busy<'a>(&'a self, _app: &'a Arc<App>, host: &'a str,
        kind: &'a str,
        fence: &'a HostFence) -> BoxFuture<'a, anyhow::Result<bool>> {
        Box::pin(async move {
            let lock = install_lock(kind).ok_or_else(|| anyhow::anyhow!("不支援安裝 {kind}"))?;
            let script = probe_script(lock);
            let out = if host == crate::config::LOCAL_HOST {
                local_sh(&script, Duration::from_secs(10)).await.map_err(|e| anyhow::anyhow!(e))?
            } else {
                fence.conn().ssh_exec_timeout(&script, Duration::from_secs(20)).await?
            };
            parse_probe(&out)
        })
    }

    fn version<'a>(&'a self, app: &'a Arc<App>, host: &'a str,
        kind: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(crate::changelog::installed_version(app, host, kind))
    }

    fn restart<'a>(&'a self, app: &'a Arc<App>, scope: Scope, fence: &'a HostFence) -> BoxFuture<'a, anyhow::Result<Value>> {
        Box::pin(async move { crate::runners::bulk_restart::spawn_scoped_fenced(app, &scope, fence).await })
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
/// SQLite 啟動時暫時讀不到時先快重試，連續失敗再逐步拉長。
const RECOVER_ENUM_RETRY_INITIAL: Duration = Duration::from_secs(1);
const RECOVER_ENUM_RETRY_MAX: Duration = Duration::from_secs(60);
/// DB 恢復後仍低頻巡查，讓稍後可讀的未收尾列也能在這次 daemon 執行期間被接手。
const RECOVER_ENUM_SWEEP: Duration = Duration::from_secs(5 * 60);
/// 終態 DB 寫入暫時失敗時保留 host slot，在目前 daemon 內持續補交。
const FINISH_RETRY_INITIAL: Duration = Duration::from_secs(1);
const FINISH_RETRY_MAX: Duration = Duration::from_secs(60);
const MARK_INSTALLED_RETRY_INITIAL: Duration = Duration::from_secs(1);
const MARK_INSTALLED_RETRY_MAX: Duration = Duration::from_secs(60);

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
           finished_at TEXT,
           host_target TEXT
         )",
    )
    .execute(pool)
    .await?;
    if !crate::db::has_column(pool, "cli_updates", "host_target").await? {
        sqlx::query("ALTER TABLE cli_updates ADD COLUMN host_target TEXT")
            .execute(pool)
            .await?;
    }
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS cli_updates_one_running ON cli_updates(host) WHERE status = 'running'")
        .execute(pool)
        .await?;
    Ok(())
}

/// Stable identity for a machine across daemon restarts. The same fields define a host repoint in
/// `api::repoints_host` and durable quota holds; reconnect generations remain process-local fences.
fn host_target_for_conn(conn: &crate::hosts::HostConn) -> String {
    match conn.cfg.as_ref() {
        None => crate::config::LOCAL_HOST.to_string(),
        Some(cfg) => format!("{}:{}/{}", cfg.ssh, cfg.ssh_port, cfg.herdr_session),
    }
}

/// `GET /api/state` 的 `cli_updates`：還沒收尾的安裝（含重啟後正在接手的那筆，帶 `recovered:true`）。
/// 進度只走 WS，`cli_update_done` 收不到時前端靠這一格對帳（同 #492 的 `restart_batch`）。讀不到 DB 回空清單。
pub async fn running_list(app: &impl crate::capabilities::Db) -> Vec<Value> {
    let rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT id, host, kind, target_version, phase, boot FROM cli_updates WHERE status = 'running' ORDER BY started_at",
    )
    .fetch_all(app.db())
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|(id, host, kind, target, phase, b)| {
            json!({"update_id": id, "host": host, "kind": kind, "target_version": target, "phase": phase, "recovered": b != boot()})
        })
        .collect()
}

async fn set_phase(app: &impl crate::capabilities::Db, id: &str, phase: &str, from: Option<&str>) {
    let res = sqlx::query(
        "UPDATE cli_updates SET phase = ?, from_version = COALESCE(?, from_version), updated_at = ? WHERE id = ? AND status = 'running'",
    )
    .bind(phase)
    .bind(from)
    .bind(crate::db::now())
    .bind(id)
    .execute(app.db())
    .await;
    if let Err(e) = res {
        tracing::warn!(update_id = id, error = %e, "could not record the cli-update phase");
    }
}

/// 結果寫進那一列（`running` → `done`／`failed`），之後這台才能再開一次。
/// DB 暫時不可寫時保持 `running`／`finishing` 並在本行程內退避重試；不會先發終態事件或放掉 host slot。
async fn finish_row(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db), id: &str, v: &Value) -> Option<Value> {
    let status = if v["ok"] == json!(true) { "done" } else { "failed" };
    let serialized = v.to_string();
    let mut retry = FINISH_RETRY_INITIAL;
    loop {
        set_phase(app, id, "finishing", None).await;
        let now = crate::db::now();
        let res = sqlx::query(
            "UPDATE cli_updates SET status = ?, result = ?, updated_at = ?, finished_at = ? WHERE id = ? AND status = 'running'",
        )
        .bind(status)
        .bind(&serialized)
        .bind(&now)
        .bind(&now)
        .bind(id)
        .execute(app.db())
        .await;
        match res {
            Ok(result) if result.rows_affected() == 1 => return Some(v.clone()),
            Ok(result) => {
                let stored: Result<Option<(String, Option<String>)>, sqlx::Error> = sqlx::query_as(
                    "SELECT status, result FROM cli_updates WHERE id = ?",
                )
                .bind(id)
                .fetch_optional(app.db())
                .await;
                match stored {
                    Ok(Some((stored_status, Some(stored_result)))) if stored_status == "done" || stored_status == "failed" => {
                        return match serde_json::from_str(&stored_result) {
                            Ok(value) => Some(value),
                            Err(error) => {
                                tracing::error!(update_id = id, error = %error, "the competing cli-update terminal result is unreadable; suppressing the local result");
                                None
                            }
                        };
                    }
                    Ok(Some((stored_status, _))) if stored_status == "running" => {
                        tracing::error!(update_id = id, rows_affected = result.rows_affected(), "cli-update terminal CAS made no change while the row is still running; retaining the host slot and retrying");
                    }
                    Ok(_) => {
                        tracing::warn!(update_id = id, "cli-update row was removed or superseded before terminal commit; suppressing the local result");
                        return None;
                    }
                    Err(error) => {
                        tracing::error!(update_id = id, error = %error, "could not reread cli-update terminal state; retaining the host slot and retrying");
                    }
                }
            }
            Err(error) => {
                tracing::error!(update_id = id, error = %error, "could not record the cli-update result; retaining the host slot and retrying");
            }
        }
        tokio::time::sleep(retry).await;
        retry = retry.saturating_mul(2).min(FINISH_RETRY_MAX);
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
    let kind = match kind.map(str::trim) {
        Some("codex") => "codex",
        Some("claude") => "claude",
        other => {
            return Err(LcError::Bad(format!(
                "kind 目前只收 claude 或 codex，收到 `{}`",
        other.unwrap_or(""))))
        }
    };
    let Some(operation_fence) = app.hosts.fence(host).await else {
        return Err(LcError::NotFound("host".into()));
    };
    let operation_host_target = host_target_for_conn(operation_fence.conn());
    let Some(target) = target.and_then(|t| version_string(t.trim())) else {
        return Err(LcError::Bad("要帶 target_version（確認框寫的那一版）；重新整理頁面再按一次".into()));
    };
    // 使用者核准的版本要跟 daemon 眼中那台「需安裝」的目標是同一版：舊分頁、別人剛裝好、帳本又出了新版都擋下來重看。
    let current_target = match kind {
        "claude" => crate::upstream_update::latest_target_for_host(app, kind, host).await,
        // 只有成功確認沒有 pending notice 才退回快照；讀取失敗時拒絕，避免舊確認框改用另一個目標。
        _ => match pending_target(app, host).await {
            Ok(Some(t)) => Some(t),
            Ok(None) => crate::upstream_update::behind_target_for_host(app, kind, host).await,
            Err(error) => {
                tracing::warn!(host, error = %error, "could not determine the pending Codex install target");
                return Err(LcError::Unavailable(json!({
                    "error": "pending_target_unavailable",
                    "reason": "pending_target_unavailable",
                    "retryable": true,
                    "retry_after_secs": 5,
                    "message": format!("讀取 {host} 的 Codex 安裝通知失敗，沒有開始安裝；稍後再試。"),
                    "detail": format!("{error:#}"),
                })));
            }
        },
    };
    match current_target {
        Some(t) if parse_version(&t) == parse_version(&target) => {}
        current => {
            let message = match &current {
                Some(t) => format!("{host} 的 {kind} 現在要裝的是 {t}，不是確認框寫的 {target}；重新開確認框再按一次"),
                None => format!("{host} 已經沒有等著安裝的 {kind} 新版（可能剛裝好了），這一下沒有安裝"),
            };
            return Err(LcError::conflict(
                "stale_target",
                json!({"host": host, "kind": kind, "target_version": target, "current_target": current, "message": message}),
            ));
        }
    }
    if !app.hosts.is_current(&operation_fence).await {
        return Err(LcError::conflict(
            "host_changed",
            json!({"host": host, "message": format!("{host} 在建立安裝紀錄前換了連線；沒有開始安裝，請重新整理後再按一次")}),
        ));
    }
    let update_id = crate::db::ulid();
    // 先寫進 DB 才開跑（#564）：每台最多一筆 `running`，重啟之後也還在，由 [`recover_at_startup`] 接手。
    let now = crate::db::now();
    let inserted = sqlx::query(
        "INSERT INTO cli_updates (id, host, kind, target_version, status, phase, boot, started_at, updated_at, host_target)
         VALUES (?, ?, ?, ?, 'running', 'starting', ?, ?, ?, ?)",
    )
    .bind(&update_id)
    .bind(host)
    .bind(kind)
    .bind(&target)
    .bind(boot())
    .bind(&now)
    .bind(&now)
    .bind(&operation_host_target)
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
        let (id, running_kind, b) =
            cur.unwrap_or_else(|| (String::new(), kind.to_string(), boot().to_string()));
        let recovered = b != boot();
        let message = if recovered {
            format!("{host} 上一次的 {running_kind} 安裝在 daemon 重啟前還沒結束，正在確認那台的安裝跑完了沒；這一下沒有再開一次")
        } else {
            format!("{host} 已經在安裝 {running_kind}，這一下沒有再開一次")
        };
        return Err(LcError::conflict(
            "cli_update_in_progress",
            json!({"host": host, "kind": running_kind, "update_id": id, "recovered": recovered, "message": message}),
        ));
    }
    // herdr 一鍵更新在跑（server 要重啟、所有 pane 會消失）：同一台不能同時裝 CLI 再 scoped 重啟 bot。
    // 先寫自己的列、再看那邊（herdr_upgrade 反過來：先佔位、再看這邊的列）；被擋就把剛寫的列拿掉，不然這台從此被自己的殘列卡住。
    if host == crate::config::LOCAL_HOST && crate::herdr_upgrade::is_running(app) {
        let _ = sqlx::query("DELETE FROM cli_updates WHERE id = ?").bind(&update_id).execute(&app.db).await;
        return Err(LcError::conflict(
            "herdr_update_in_progress",
            json!({"host": host, "message": "herdr 正在更新（server 會重啟、所有 pane 都會消失），這一下沒有安裝；等它結束再試"}),
        ));
    }
    let (app2, host2, id2, target2, kind2, fence2) = (
        app.clone(),
        host.to_string(),
        update_id.clone(),
        target.clone(),
        kind.to_string(), operation_fence);
    tokio::spawn(async move {
        use futures::FutureExt as _;
        let res = std::panic::AssertUnwindSafe(run_with_fence(&app2, runner.as_ref(), &host2, &id2, &target2, &kind2, Some(fence2))).catch_unwind().await;
        if res.is_err() {
            // 中途 panic 也要等終態寫進 DB 後才告訴 UI；finish_row 會在目前行程內保留 retry debt。
            let failed = json!({"update_id": id2, "host": host2, "kind": kind2, "target_version": target2,
                "ok": false, "reason": "internal_error", "error": "安裝流程中途異常結束，沒有重啟任何 bot；看 daemon.log"});
            if let Some(durable) = finish_row(&app2, &id2, &failed).await {
                app2.emit("cli_update_done", durable).await;
            }
        }
    });
    Ok(json!({"update_id": update_id, "host": host, "kind": kind, "target_version": target, "started": true}))
}

/// 一次安裝的結果（也是 `cli_update_done` 的內容）。
///
/// 整段可能跑五分鐘，途中同名主機可能重連或改指到另一台（#347）：第一次讀版本之前記下 [`HostFence`]，每次讀完、
/// 改通知與開批次之前都確認它還是權威。不是就停在那裡回 `superseded`——升級前後的版本可能是兩台機器讀的，
/// 不能拿來判斷裝好了沒，更不能去改新機器的通知、重啟新機器的 bot。
#[cfg(test)]
pub async fn run(app: &Arc<App>, runner: &dyn Runner, host: &str, update_id: &str, target: &str) -> Value {
    let stored_kind = read_durable_kind(app, update_id).await;
    let expected_kind = stored_kind
        .as_ref()
        .ok()
        .and_then(Option::as_deref)
        .unwrap_or("")
        .to_string();
    let fence = app.hosts.fence(host).await;
    run_with_kind_result(app, runner, host, update_id, target, &expected_kind, stored_kind, fence).await
}

async fn run_with_fence(
    app: &Arc<App>,
    runner: &dyn Runner,
    host: &str,
    update_id: &str,
    target: &str,
    expected_kind: &str,
    fence: Option<HostFence>,
) -> Value {
    let stored_kind = read_durable_kind(app, update_id).await;
    run_with_kind_result(app, runner, host, update_id, target, expected_kind, stored_kind, fence).await
}

async fn read_durable_kind(app: &impl crate::capabilities::Db, update_id: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT kind FROM cli_updates WHERE id=?")
        .bind(update_id)
        .fetch_optional(app.db())
        .await
}

async fn run_with_kind_result(
    app: &Arc<App>,
    runner: &dyn Runner,
    host: &str,
    update_id: &str,
    target: &str,
    expected_kind: &str,
    stored_kind: Result<Option<String>, sqlx::Error>,
    fence: Option<HostFence>,
) -> Value {
    let kind = expected_kind.to_string();
    let log_path = app.data_dir.join(LOG_FILE);
    let base = json!({"update_id": update_id, "host": host, "kind": kind, "target_version": target, "log_path": log_path});
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
    let finish_kind = kind.clone();
    let finish_log_kind = kind.clone();
    let finish = |v: Value| {
        let finish_kind = finish_kind.clone();
        let finish_log_kind = finish_log_kind.clone();
        async move {
        // 結果先落進 DB 才推事件、才放這台的名額（#564、#577）。
        let Some(v) = finish_row(app, update_id, &v).await else {
            return json!({"update_id": update_id, "host": host, "kind": finish_kind, "target_version": target,
                "ok": false, "reason": "superseded", "error": "終態列已不存在或已被其他流程取代；沒有發布本地結果"});
        };
        if v["ok"] == json!(false) {
            tracing::warn!(host, kind = %finish_log_kind,reason = %v["reason"], error = %v["error"], "CLI 升級沒有完成，沒有重啟任何 bot");
            log_line(app, &format!("[{update_id}] 結束：失敗 {} {}", v["reason"], v["error"]));
        }
        app.emit("cli_update_done", v.clone()).await;
        v
    }
    };

    let kind = match verify_durable_kind(expected_kind, stored_kind, update_id) {
        Ok(kind) => kind,
        Err(error) => return finish(done(false, json!({"reason": "internal_error", "error": error}))).await,
    };

    let Some(fence) = fence else {
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
    let before = runner.version(app, host, &kind).await;
    if !app.hosts.is_current(&fence).await {
        return finish(superseded("讀目前版本", json!({}))).await;
    }
    let before = match before.map(|raw| normalized(&raw)) {
        Ok(Some(v)) => v,
        Ok(None) | Err(_) => {
            return finish(done(false, json!({"reason": "version_unreadable", "error": format!("讀不到目前的 {kind} 版本，沒有安裝")}))).await;
        }
    };
    let target_v = parse_version(target);
    // Codex 的安裝器裝最新版，已到或超過目標就可直接套用。Claude 只會在低於目標時執行指定版本安裝；
    // 快照落後而磁碟已超前目標時也當作已安裝，避免照舊快照把 CLI 降版。
    let already = if kind == "claude" {
        parse_version(&before)
            .zip(target_v.as_ref())
            .is_some_and(|(before, target)| &before >= target)
    } else {
        parse_version(&before) >= target_v
    };
    let after = if already {
        log_line(app, &format!("[{update_id}] {host}：{kind} 已經是 {before}（目標 {target}），不再安裝"));
        before.clone()
    } else {
        set_phase(app, update_id, "installing", Some(&before)).await;
        app.emit("cli_update_progress", progress("installing", json!({"from": before}))).await;
        let command = install_command(&kind, target).unwrap_or_default();
        log_line(app, &format!("[{update_id}] {host}：{kind} {before}，目標 {target}，執行 {command}"));
        let installed = runner.install(app, host, &kind, target, &fence).await;
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
                    "error": format!("{host} 已經有另一個 {kind} 安裝在跑（{}），這次沒有再裝一次；等它結束再按", e.trim())})))
                .await;
            }
            Err(InstallError::Failed(e)) => {
                log_line(app, &format!("[{update_id}] 安裝失敗：\n{e}"));
                return finish(done(false, json!({"reason": "install_failed", "error": e, "from": before}))).await;
            }
        }
        set_phase(app, update_id, "verifying", None).await;
        app.emit("cli_update_progress", progress("verifying", json!({"from": before}))).await;
        let after = runner.version(app, host, &kind).await;
        if !app.hosts.is_current(&fence).await {
            return finish(superseded("確認新版本", json!({"from": before}))).await;
        }
        let after = match after.map(|raw| normalized(&raw)) {
            Ok(Some(v)) => v,
            Ok(None) | Err(_) => {
                return finish(done(false, json!({"reason": "verify_failed", "from": before,
                    "error": format!("安裝指令跑完了，但讀不到 {kind} --version，沒有重啟任何 bot")})))
                .await;
            }
        };
        if kind == "codex" && parse_version(&after) <= parse_version(&before) {
            return finish(done(false, json!({"reason": "version_unchanged", "from": before, "to": after,
                "error": format!("安裝指令跑完了，codex 還是 {after}（原本 {before}），沒有重啟任何 bot")})))
            .await;
        }
        // Codex 裝最新版，至少要到核准版本；Claude 指定版本必須完全相同。失敗時一顆都不重啟。
        if if kind == "claude" {
            parse_version(&after) != target_v
        } else {
            parse_version(&after) < target_v
        } {
            let reached = if kind == "claude" {
                "沒有精確到"
            } else {
                "還沒到"
            };
            return finish(done(false, json!({"reason": "target_not_reached", "from": before, "to": after,
                "error": format!("安裝指令跑完了，{kind} 是 {after}，{reached}確認的 {target}（原本 {before}），沒有重啟任何 bot")})))
            .await;
        }
        log_line(app, &format!("[{update_id}] {kind} 升級完成：{before} → {after}（目標 {target}）"));
        after
    };
    if !app.hosts.is_current(&fence).await {
        return finish(superseded("改通知", json!({"from": before, "to": after}))).await;
    }
    let Some(notices) = mark_installed_until_durable(app, host, &kind, &before, &after, &fence).await else {
        return finish(superseded("改通知", json!({"from": before, "to": after}))).await;
    };
    if !app.hosts.is_current(&fence).await {
        return finish(superseded("確認通知持久化", json!({"from": before, "to": after, "notices_updated": notices}))).await;
    }
    crate::runners::update_watch::forget_disk_version(app, host, &kind).await;
    if app
        .hosts
        .run_if_current(
            &fence,
            crate::upstream_update::note_installed(app, &kind, host, &after),
        )
        .await
        .is_none()
    {
        return finish(superseded(
            "更新上游快照",
            json!({"from": before, "to": after, "notices_updated": notices}),
        ))
        .await;
    }
    if kind == "claude" {
        // Claude 的安裝與套用是兩次明確操作；改好持續通知後交給一般重啟 chip，不背景替使用者重啟。
        return finish(done(
            true,
            json!({
                "from": before,
                "to": after,
                "already_installed": already,
                "notices_updated": notices,
                "restart": null,
                "restart_required": true,
                "restart_status": "manual",
            }),
        ))
        .await;
    }
    set_phase(app, update_id, "restarting", Some(&before)).await;
    app.emit(
        "cli_update_progress",
        progress("restarting", json!({"from": before, "to": after})),
    )
    .await;
    if !app.hosts.is_current(&fence).await {
        return finish(superseded(
            "開重啟批次", json!({"from": before, "to": after, "notices_updated": notices}))).await;
    }
    let restart = runner.restart(app, Scope { kind: kind.clone(), host: host.to_string() }, &fence).await;
    let v = match restart {
        // `restart_status` 照批次那邊講的（#566）：`started` 開了這台 codex 的一批；`already_covered` 正在跑的那一批
        // 確實還排著這台每一顆該重啟的 codex；`deferred` 那一批沒涵蓋，排在它後面、它結束時自己接著開。
        // 不能再把「剛好有一批在跑」（`already_running`）當成重啟已經交出去。
        Ok(plan) => {
            let status = plan["restart_status"].as_str().unwrap_or("started").to_string();
            if status == "superseded" {
                let error = format!("主機 {host} 的設定已改變；這次安裝留在原主機，但沒有把任何 bot 的重啟改投到新主機");
                tracing::warn!(host, update_id,
                    kind,
                    "CLI install succeeded but its scoped restart authority was superseded"
                );
                log_line(app, &format!("[{update_id}] 重啟作廢：{error}"));
                return finish(done(true, json!({
                    "from": before,
                    "to": after,
                    "already_installed": already,
                    "notices_updated": notices,
                    "restart": null,
                    "restart_status": status,
                    "restart_error": error,
                })))
                .await;
            }
            log_line(app, &format!("[{update_id}] 重啟：{status} {}", plan["batch_id"]));
            done(true, json!({"from": before, "to": after, "already_installed": already, "notices_updated": notices,
                "restart": plan, "restart_status": status}))
        }
        // 裝好了但批次開不起來：新版已經在磁碟上，通知也改成「重啟套用」了，照一般的一鍵重啟再按一次就好。
        Err(e) => done(true, json!({"from": before, "to": after, "already_installed": already, "notices_updated": notices,
            "restart": null, "restart_status": "error", "restart_error": format!("{e:#}")})),
    };
    finish(v).await
}

type RecoveryRow = (String, String, String,
    String,
    Option<String>, Option<String>,
);

/// 同一列可能在多輪掃描中一直是 `running`。Processed tombstones 必須保留到 daemon 結束：某一輪掃描
/// 可能在 worker 終結前讀到舊 row，稍後才把那份結果送來 admission；只記錄 active worker 會重新啟動它。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryWorkerState {
    Active,
    Processed,
}

static ACTIVE_RECOVERIES: OnceLock<Mutex<HashMap<String, RecoveryWorkerState>>> = OnceLock::new();

fn active_recoveries() -> &'static Mutex<HashMap<String, RecoveryWorkerState>> {
    ACTIVE_RECOVERIES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn recovery_worker_active(id: &str) -> bool {
    active_recoveries()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        == Some(&RecoveryWorkerState::Active)
}

struct RecoveryWorkerGuard(String);

impl RecoveryWorkerGuard {
    fn mark_processed(&self) {
        let mut recoveries = active_recoveries()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if recoveries.get(&self.0) == Some(&RecoveryWorkerState::Active) {
            recoveries.insert(self.0.clone(), RecoveryWorkerState::Processed);
        }
    }
}

impl Drop for RecoveryWorkerGuard {
    fn drop(&mut self) {
        let mut recoveries = active_recoveries()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if recoveries.get(&self.0) == Some(&RecoveryWorkerState::Active) {
            recoveries.remove(&self.0);
        }
    }
}

fn spawn_recovery_worker(app: &Arc<App>, runner: &Arc<dyn Runner>, row: RecoveryRow) -> bool {
    let (id, host, kind, target, from, host_target) = row;
    let claimed = {
        let mut recoveries = active_recoveries()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if recoveries.contains_key(&id) {
            false
        } else {
            recoveries.insert(id.clone(), RecoveryWorkerState::Active);
            true
        }
    };
    if !claimed {
        return false;
    }
    let (app, runner, id) = (app.clone(), runner.clone(), id.clone());
    let guard = RecoveryWorkerGuard(id.clone());
    tokio::spawn(async move {
        let _guard = guard;
        tracing::warn!(update_id = %id, host = %host, kind, "CLI 升級在 daemon 重啟前還沒收尾，接手確認那台的安裝");
        recover_with_registry(
            &app,
            runner.as_ref(),
            &id,
            &host,
            &kind,
            &target,
            from.as_deref(),
            RECOVER_POLL,
            RECOVER_MAX,
            host_target.as_deref(),
            Some(&_guard),
        )
        .await;
    });
    true
}

async fn unfinished_updates(app: &impl crate::capabilities::Db) -> Result<Vec<RecoveryRow>, sqlx::Error> {
    sqlx::query_as("SELECT id, host, kind, target_version, from_version, host_target FROM cli_updates WHERE status = 'running' AND boot != ?")
        .bind(boot())
        .fetch_all(app.db())
        .await
}

/// 啟動時立即掃描；不論成功與否都保留低頻掃描器。讀取錯誤先以有上限的退避重試，成功後每隔一段時間繼續巡查。
async fn start_recovery_sweeper(
    app: &Arc<App>,
    runner: Arc<dyn Runner>,
    retry_initial: Duration,
    retry_max: Duration,
    sweep_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let retry_initial = retry_initial.min(retry_max);
    let (mut wait, mut retry_delay) = match unfinished_updates(app).await {
        Ok(rows) => {
            for row in rows {
                spawn_recovery_worker(app, &runner, row);
            }
            (sweep_interval, retry_initial)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not read unfinished cli updates; startup recovery will retry");
            (retry_initial, retry_initial.saturating_mul(2).min(retry_max))
        }
    };
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(wait).await;
            match unfinished_updates(&app).await {
                Ok(rows) => {
                    for row in rows {
                        spawn_recovery_worker(&app, &runner, row);
                    }
                    wait = sweep_interval;
                    retry_delay = retry_initial;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not read unfinished cli updates; startup recovery will retry");
                    wait = retry_delay;
                    retry_delay = retry_delay.saturating_mul(2).min(retry_max);
                }
            }
        }
    })
}

/// 開機時接手上一顆 daemon 沒收尾的安裝（#564）。那一列還是 `running`，所以這台照樣 409、`GET /api/state` 照樣列著；
/// 每一筆背景等那台的安裝鎖放掉，再讀一次版本收尾。啟動讀取失敗不會丟掉接手工作：退避重試後持續低頻掃描，
/// 同一 update id 在這顆 daemon 裡最多只有一個 recovery worker。
pub async fn recover_at_startup(app: &Arc<App>) {
    let _sweeper = start_recovery_sweeper(app, Arc::new(Real), RECOVER_ENUM_RETRY_INITIAL, RECOVER_ENUM_RETRY_MAX, RECOVER_ENUM_SWEEP).await;
}

/// 接手一筆孤兒安裝：等那台的安裝鎖沒有活著的主人（上一顆 daemon 開的 `curl | sh` 跑完或死了），讀版本收尾。
///
/// - 到了核准的版本：改通知成「已安裝，重啟套用」，`ok:true`＋`recovered:true`。**不自動開重啟**——按下去的那一刻跟現在
///   隔了一次 daemon 重啟，哪些 bot 閒著已經不是當時那份；`restart:null`＋`restart_error` 請人按一般的重啟。
/// - 沒到（或讀不到）：`reason:"interrupted"`，一顆都不重啟、通知不動；使用者再按一次會照常重來（安裝前先讀版本）。
/// - 等到 `max` 鎖還在：一樣收成 `interrupted` 放掉這一列；那台的安裝真的還在跑的話，下一次按會被主機端的鎖擋成 `already_running`。
///
/// 絕對不跑安裝指令。
#[cfg(test)]
async fn fail_recovery_kind(
    app: &App,
    update_id: &str,
    host: &str,
    target: &str,
    kind: &str,
    error: String,
) -> Value {
    let failed = json!({
        "update_id": update_id,
        "host": host,
        "kind": kind,
        "target_version": target,
        "recovered": true,
        "ok": false,
        "reason": "internal_error",
        "error": error,
    });
    match finish_row(app, update_id, &failed).await {
        Some(result) => {
            app.emit("cli_update_done", result.clone()).await;
            result
        }
        None => json!({
            "update_id": update_id,
            "host": host,
            "kind": kind,
            "target_version": target,
            "recovered": true,
            "ok": false,
            "reason": "superseded",
            "error": "終態列已不存在或已被其他流程取代；沒有發布本地結果",
        }),
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub async fn recover(
    app: &Arc<App>,
    runner: &dyn Runner,
    update_id: &str,
    host: &str,
    target: &str,
    from: Option<&str>,
    poll: Duration,
    max: Duration,
) -> Value {
    let kind = match read_durable_kind(app, update_id).await {
        Ok(Some(kind)) if supported_kind(&kind) => kind,
        Ok(Some(kind)) => return fail_recovery_kind(app, update_id, host, target, &kind,
            format!("持久 cli-update 列的 kind 不支援：{kind}")).await,
        Ok(None) => return fail_recovery_kind(app, update_id, host, target, "",
            "持久 cli-update 列已不存在，不能接手".to_string()).await,
        Err(error) => return fail_recovery_kind(app, update_id, host, target, "",
            format!("讀取持久 cli-update kind 失敗：{error}")).await,
    };
    let host_target = sqlx::query_scalar::<_, Option<String>>("SELECT host_target FROM cli_updates WHERE id = ?")
        .bind(update_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .flatten();
    recover_with_registry(app, runner, update_id, host,
        &kind,
        target, from, poll, max, host_target.as_deref(), None).await
}

#[allow(clippy::too_many_arguments)]
async fn recover_with_registry(
    app: &Arc<App>,
    runner: &dyn Runner,
    update_id: &str,
    host: &str,
    kind: &str,
    target: &str,
    from: Option<&str>,
    poll: Duration,
    max: Duration,
    host_target: Option<&str>,
    registry_guard: Option<&RecoveryWorkerGuard>,
) -> Value {
    let log_path = app.data_dir.join(LOG_FILE);
    let base = json!({"update_id": update_id, "host": host, "kind": kind, "target_version": target, "log_path": log_path, "recovered": true});
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
        let Some(v) = finish_row(app, update_id, &v).await else {
            if let Some(guard) = registry_guard {
                guard.mark_processed();
            }
            return json!({"update_id": update_id, "host": host, "kind": kind, "target_version": target,
                "recovered": true, "ok": false, "reason": "superseded", "error": "終態列已不存在或已被其他流程取代；沒有發布本地結果"});
        };
        if let Some(guard) = registry_guard {
            guard.mark_processed();
        }
        log_line(app, &format!("[{update_id}] 重啟後接手收尾：ok={} {} {}", v["ok"], v["reason"], v["error"]));
        app.emit("cli_update_done", v.clone()).await;
        v
    };
    let changed_authority = |message: String| done(false, json!({"reason": "host_changed", "error": message}));
    let superseded = |message: String| done(false, json!({"reason": "superseded", "error": message}));
    if let Err(error) = verify_durable_kind(kind, read_durable_kind(app, update_id).await, update_id) {
        return finish(done(false, json!({"reason": "internal_error", "error": error}))).await;
    }
    set_phase(app, update_id, "recovering", None).await;
    let mut progress = base.clone();
    progress["phase"] = json!("installing");
    app.emit("cli_update_progress", progress).await;

    let Some(host_target) = host_target else {
        return finish(changed_authority(format!(
            "這筆 {host} 的 cli-update 沒有保存啟動時的主機目標；不能把目前同名主機當成接手對象"
        )))
        .await;
    };

    let deadline = tokio::time::Instant::now() + max;
    let fence = loop {
        let Some(fence) = app.hosts.fence(host).await else {
            return finish(changed_authority(format!("主機 {host} 已經不在設定裡；上一次的安裝有沒有跑完要去原本那台看"))).await;
        };
        if host_target_for_conn(fence.conn()) != host_target {
            return finish(changed_authority(format!(
                "{host} 現在指向另一個主機目標；這筆上次的安裝不會接手或改目前主機的通知"
            )))
            .await;
        }
        let probe = runner.installer_busy(app, host, kind, &fence).await;
        if !app.hosts.is_current(&fence).await {
            return finish(superseded(format!("{host} 在安裝鎖探測期間重連或改設定；沒有讀版本、改通知或發布成功"))).await;
        }
        match probe {
            Ok(false) => break fence,
            Ok(true) => {}
            Err(e) => tracing::debug!(host, error = %e, "could not probe the codex install lock yet"),
        }
        if tokio::time::Instant::now() >= deadline {
            return finish(done(false, json!({"reason": "interrupted",
                "error": format!("daemon 重啟前開始的安裝，等了 {} 分鐘 {host} 上的安裝還沒結束（或連不上）；沒有重啟任何 bot。那台的安裝還在跑的話再按會被擋下", max.as_secs() / 60)})))
            .await;
        }
        tokio::time::sleep(poll).await;
    };
    if !app.hosts.is_current(&fence).await {
        return finish(superseded(format!("{host} 的安裝鎖已放開，但連線在版本核對前改變；沒有改通知"))).await;
    }
    let after = runner.version(app, host, kind).await.ok().and_then(|raw| normalized(&raw));
    if !app.hosts.is_current(&fence).await {
        return finish(superseded(format!("{host} 在版本核對期間重連或改設定；沒有改通知"))).await;
    }
    let reached = |a: &str| {
        if kind == "claude" {
            parse_version(a) == parse_version(target)
        } else {
            parse_version(a) >= parse_version(target)
        }
    };
    let Some(after) = after.clone().filter(|a| reached(a)) else {
        return finish(done(false, json!({"reason": "interrupted", "to": after,
            "error": format!("daemon 在安裝途中重啟，{host} 的 {kind} 現在是 {}，沒符合確認的 {target}；沒有重啟任何 bot，要裝就再按一次", after.as_deref().unwrap_or("（讀不到）"))})))
        .await;
    };
    // 跑著的版本優先取通知寫的起點（`mark_installed` 自己會找）；這裡的 `from` 只是最後的退路。
    if !app.hosts.is_current(&fence).await || host_target_for_conn(fence.conn()) != host_target {
        return finish(superseded(format!("{host} 在改通知前重連或改設定；沒有發布接手成功"))).await;
    }
    let Some(notices) = mark_installed_until_durable(app, host, kind, from.unwrap_or(""), &after, &fence).await else {
        return finish(superseded(format!("{host} 在改通知期間重連或改設定；沒有發布接手成功"))).await;
    };
    if !app.hosts.is_current(&fence).await || host_target_for_conn(fence.conn()) != host_target {
        return finish(superseded(format!("{host} 在改通知持久化時重連或改設定；沒有發布接手成功"))).await;
    }
    crate::runners::update_watch::forget_disk_version(app, host, kind).await;
    if kind == "claude" {
        if app
            .hosts
            .run_if_current(
                &fence,
                crate::upstream_update::note_installed(app, kind, host, &after),
            )
            .await
            .is_none()
        {
            return finish(superseded(format!(
                "{host} 在更新上游快照時重連或改設定；沒有發布接手成功"
            )))
            .await;
        }
        finish(done(true, json!({"to": after, "notices_updated": notices, "restart": null, "restart_required": true,
            "restart_status": "manual"}))).await
    } else {
        finish(done(true, json!({"to": after, "notices_updated": notices, "restart": null,
            "restart_error": "daemon 在安裝途中重啟過，這次沒有自動重啟 bot；按一般的 ⌃⌃ 重啟套用"}))).await
    }
}

/// 那台主機上還寫著「需安裝」的 codex run 與它的通知。
fn pending_notice_target(kind: &str, notice: &str) -> Option<String> {
    match kind {
        "codex" => crate::codex_update::pending_to(notice),
        "claude" => crate::upstream_update::claude_pending_to(notice),
        _ => None,
    }
}

async fn pending_runs(app: &impl crate::capabilities::Db, host: &str, kind: &str) -> anyhow::Result<Vec<(crate::db::Run, String)>> {
    let mut out = Vec::new();
    let runs = crate::db::all_active_runs(app.db())
        .await
        .map_err(|error| anyhow::anyhow!("enumerate active runs: {error:#}"))?;
    for run in runs {
        let Some(notice) = run
            .update_notice
            .clone()
            .filter(|t| pending_notice_target(kind, t).is_some())
        else {
            continue;
        };
        let Some(bot) = crate::db::bot(app.db(), &run.bot_id)
            .await
            .map_err(|error| anyhow::anyhow!("read bot {}: {error:#}", run.bot_id))?
        else {
            continue;
        };
        if bot.kind != kind {
            continue;
        }
        let bot_host = crate::db::bot_host(app.db(), &run.bot_id)
            .await
            .map_err(|error| anyhow::anyhow!("read host for bot {}: {error:#}", run.bot_id))?;
        if bot_host != host {
            continue;
        }
        out.push((run, notice));
    }
    Ok(out)
}

/// daemon 眼中那台現在要裝的版本。Claude 以 fleet 上游快照綁定共同目標，Codex 沿用 run 通知。
async fn pending_target(app: &impl crate::capabilities::Db, host: &str) -> anyhow::Result<Option<String>> {
    Ok(pending_runs(app, host, "codex")
        .await?
        .iter()
        .filter_map(|(_, n)| pending_notice_target("codex", n))
        .max_by(|a, b| parse_version(a).cmp(&parse_version(b))))
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

fn log_line(app: &impl crate::capabilities::DataDir, line: &str) {
    let path = app.data_dir().join(LOG_FILE);
    let res = std::fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| writeln!(f, "{} {line}", crate::db::now()));
    if let Err(e) = res {
        tracing::warn!(path = %path.display(), error = %e, "could not write the cli-update log");
    }
}

fn notice_versions(notice: &str) -> Vec<String> {
    notice
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter_map(crate::changelog::version_string)
        .collect()
}

fn notice_needs_this_install(kind: &str, notice: &str, after: &str) -> bool {
    let prefix = match kind {
        "codex" => crate::codex_update::NOTICE_PREFIX,
        "claude" => crate::upstream_update::CLAUDE_NOTICE_PREFIX,
        _ => return false,
    };
    if !notice.starts_with(prefix) {
        return false;
    }
    if notice.contains("需安裝") {
        return pending_notice_target(kind, notice)
            .and_then(|v| parse_version(&v))
            .zip(parse_version(after))
            .is_none_or(|(target, installed)| target <= installed);
    }
    if notice.contains("已安裝") {
        return notice_versions(notice)
            .first()
            .and_then(|v| parse_version(v))
            .zip(parse_version(after))
            .is_none_or(|(recorded, installed)| recorded < installed);
    }
    false
}

async fn mark_installed_change_is_already_durable(app: &impl crate::capabilities::Db, host: &str,
    kind: &str, run_id: &str, after: &str) -> anyhow::Result<bool> {
    let Some(run) = crate::db::run(app.db(), run_id).await? else {
        return Ok(true);
    };
    if !matches!(run.state.as_str(), "starting" | "running" | "stopping") {
        return Ok(true);
    }
    let Some(bot) = crate::db::bot(app.db(), &run.bot_id).await? else {
        return Ok(true);
    };
    if bot.kind != kind || crate::db::bot_host(app.db(), &run.bot_id).await? != host {
        return Ok(true);
    }
    let Some(notice) = run.update_notice.as_deref() else {
        return Ok(true);
    };
    let prefix = if kind == "claude" {
        crate::upstream_update::CLAUDE_NOTICE_PREFIX
    } else {
        crate::codex_update::NOTICE_PREFIX
    };
    if notice.starts_with(prefix) && notice.contains("需安裝") {
        let Some(target) = pending_notice_target(kind, notice).and_then(|v| parse_version(&v)) else {
            anyhow::bail!("active {kind} run {run_id} still has an unreadable install notice after a lost CAS");
        };
        let Some(installed) = parse_version(after) else {
            anyhow::bail!("installed {kind} version {after:?} is not parseable after a lost CAS");
        };
        return Ok(target > installed);
    }
    if notice.starts_with(prefix) && notice.contains("已安裝") {
        let Some(recorded) = notice_versions(notice).first().and_then(|v| parse_version(v)) else {
            anyhow::bail!(
                "active {kind} run {run_id} has an unreadable installed notice after a lost CAS"
            );
        };
        let Some(installed) = parse_version(after) else {
            anyhow::bail!("installed {kind} version {after:?} is not parseable after a lost CAS");
        };
        return Ok(recorded >= installed);
    }
    // The run no longer carries this CLI install handoff, so it cannot be skipped by this operation's restart.
    Ok(true)
}

/// 只將通知改成安裝完成；列舉、權威讀取、CAS 寫入或 CAS 衝突驗證失敗時回錯，不能把錯誤當成空名單。
/// 確認沒有 relevant row 時回 `Ok(0)`，只有這個成功證明的空結果可視為無操作。
/// 跑著的版本：記憶體裡看過的 → 通知寫的起點 → 安裝前的磁碟版本（這個 process 是裝之前起的，不會比它新）。
async fn mark_installed_with_fence(
    app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::Db + crate::hosts::HostsAccess),
    host: &str,
    kind: &str,
    before: &str,
    after: &str,
    fence: Option<&HostFence>,
) -> anyhow::Result<Option<usize>> {
    let mut n = 0;
    for run in crate::db::all_active_runs(app.db()).await? {
        if let Some(fence) = fence {
            if !app.hosts().is_current(fence).await {
                return Ok(None);
            }
        }
        let Some(notice) = run.update_notice.as_deref() else { continue };
        if !notice_needs_this_install(kind, notice, after) {
            continue;
        }
        let Some(bot) = crate::db::bot(app.db(), &run.bot_id).await? else { continue };
        let bot_host = crate::db::bot_host(app.db(), &run.bot_id).await?;
        if bot.kind != kind || bot_host != host {
            continue;
        }
        if let Some(fence) = fence {
            if !app.hosts().is_current(fence).await {
                return Ok(None);
            }
        }
        let running = if kind == "codex" {
            crate::codex_update::remember_running(&run.id, "", None)
            .or_else(|| crate::codex_update::pending_from(notice))
            .or_else(|| notice_versions(notice).get(1).cloned())
        } else {
            crate::update_watch::running_version(run.status_json.as_deref())
                .or_else(|| crate::upstream_update::claude_pending_from(notice))
                .or_else(|| notice_versions(notice).get(1).cloned())
        }
        .unwrap_or_else(|| before.to_string());
        let text = if kind == "codex" {
            crate::codex_update::installed_text(after, &running)
        } else if parse_version(&running).zip(parse_version(after)).is_some_and(|(r, a)| r >= a) {
            None
        } else {
            Some(crate::upstream_update::claude_installed_text(
                after, &running,
            ))
        };
        if text.is_none() && kind != "claude" {
            if parse_version(&running)
                .zip(parse_version(after))
                .is_some_and(|(r, a)| r >= a)
            {
                continue;
            }
            anyhow::bail!(
                "could not prove {kind} run {} no longer needs install notice transition", run.id);
        }
        if let Some(fence) = fence {
            if !app.hosts().is_current(fence).await {
                return Ok(None);
            }
            #[cfg(test)]
            crate::app_ports_p12::race_point::hit("cli_update_before_notice_cas", &run.id).await;
        }
        let write_notice = async {
            sqlx::query("UPDATE runs SET update_notice = ? WHERE id = ? AND update_notice = ?")
                .bind(text.as_deref())
                .bind(&run.id)
                .bind(notice)
                .execute(app.db())
                .await
        };
        // Host replacement takes the same connection's write gate. Validate under its read side
        // and keep that gate through the durable CAS, so repoint either wins first and refuses
        // this write or waits until the notice transition has committed on its captured authority.
        let result = match fence {
            Some(fence) => match app.hosts().run_if_current(fence, write_notice).await {
                Some(result) => result?,
                None => return Ok(None),
            },
            None => write_notice.await?,
        };
        if result.rows_affected() == 1 {
            n += 1;
            app.emit_bot_status(&run.bot_id).await;
        } else {
            if let Some(fence) = fence {
                if !app.hosts().is_current(fence).await {
                    return Ok(None);
                }
            }
            if !mark_installed_change_is_already_durable(app, host, kind, &run.id, after).await? {
                anyhow::bail!(
                    "{kind} run {} still needs the installed notice transition after a lost CAS", run.id);
            }
            if let Some(fence) = fence {
                if !app.hosts().is_current(fence).await {
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some(n))
}

/// DB 錯誤時維持 cli_updates 的 running 列並退避重試；run/recover 都要等通知轉換可證明持久化後才收尾。
/// fence 失效就停止重試並拒絕發布成功，防止重試期間改寫同名新主機的通知。
async fn mark_installed_until_durable(
    app: &(impl crate::capabilities::BotStatusEmit + crate::capabilities::DataDir + crate::capabilities::Db + crate::hosts::HostsAccess),
    host: &str,
    kind: &str,
    before: &str,
    after: &str,
    fence: &HostFence,
) -> Option<usize> {
    let mut retry = MARK_INSTALLED_RETRY_INITIAL;
    loop {
        if !app.hosts().is_current(fence).await {
            return None;
        }
        match mark_installed_with_fence(app, host, kind, before, after, Some(fence)).await {
            Ok(Some(n)) => return Some(n),
            Ok(None) => return None,
            Err(error) => {
                if !app.hosts().is_current(fence).await {
                    return None;
                }
                tracing::warn!(host, kind,error = %error, "could not durably mark CLI update notices installed; keeping update running and retrying");
                log_line(app, &format!("{kind} notice transition for {host} failed; update remains running: {error:#}"));
                tokio::time::sleep(retry).await;
                retry = retry.saturating_mul(2).min(MARK_INSTALLED_RETRY_MAX);
            }
        }
    }
}

#[cfg(test)]
async fn mark_installed(app: &Arc<App>, host: &str, before: &str, after: &str) -> anyhow::Result<usize> {
    Ok(mark_installed_with_fence(app, host, "codex", before, after, None).await?.unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::sync::Mutex;

    /// 假執行器：版本照順序吐、安裝照設定成功或失敗、批次只記下被叫了幾次。**不碰真的 codex**。
    struct Fake {
        versions: Mutex<Vec<anyhow::Result<String>>>,
        version_reads: Mutex<usize>,
        install: Result<String, InstallError>,
        /// `installer_busy` 照順序吐；吐完就是「沒人拿著」。
        busy: Mutex<Vec<bool>>,
        probes: Mutex<usize>,
        probe_hold: Duration,
        installs: Mutex<usize>,
        restarts: Mutex<Vec<(String, String)>>,
        /// 安裝卡住多久（測並發用）。
        hold: Duration,
        /// 有的話，安裝等到它放行才回（測途中換主機用）。
        gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        /// 有的話，重啟請求記錄後先停住，讓測試在終局寫入前安排 DB 競態。
        restart_gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        /// 批次走真的 `bulk_restart::spawn_scoped`（#566：要測跟正在跑的那一批怎麼互動）。
        real_restart: bool,
    }

    impl Fake {
        fn new(versions: &[&str], install: Result<&str, &str>) -> Arc<Self> {
            Arc::new(Self {
                versions: Mutex::new(versions.iter().map(|v| Ok(v.to_string())).collect()),
                version_reads: Mutex::new(0),
                install: install.map(str::to_string).map_err(|e| InstallError::Failed(e.to_string())),
                busy: Mutex::new(Vec::new()),
                probes: Mutex::new(0),
                probe_hold: Duration::ZERO,
                installs: Mutex::new(0),
                restarts: Mutex::new(Vec::new()),
                hold: Duration::ZERO,
                gate: Mutex::new(None),
                restart_gate: Mutex::new(None),
                real_restart: false,
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
        fn installer_busy<'a>(&'a self, _app: &'a Arc<App>, _host: &'a str,
            _kind: &'a str,
            _fence: &'a HostFence) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async move {
                *self.probes.lock().unwrap() += 1;
                if !self.probe_hold.is_zero() {
                    tokio::time::sleep(self.probe_hold).await;
                }
                let mut b = self.busy.lock().unwrap();
                Ok(if b.is_empty() { false } else { b.remove(0) })
            })
        }
        fn install<'a>(&'a self, _app: &'a Arc<App>, _host: &'a str,
            _kind: &'a str,
            _target: &'a str,
            _fence: &'a HostFence) -> BoxFuture<'a, Result<String, InstallError>> {
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
        fn version<'a>(&'a self, _app: &'a Arc<App>, _host: &'a str,
            _kind: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
            Box::pin(async move {
                *self.version_reads.lock().unwrap() += 1;
                let mut v = self.versions.lock().unwrap();
                if v.is_empty() {
                    Err(anyhow::anyhow!("no more versions"))
                } else {
                    v.remove(0)
                }
            })
        }
        fn restart<'a>(&'a self, app: &'a Arc<App>, scope: Scope, fence: &'a HostFence) -> BoxFuture<'a, anyhow::Result<Value>> {
            Box::pin(async move {
                self.restarts.lock().unwrap().push((scope.kind.clone(), scope.host.clone()));
                let gate = self.restart_gate.lock().unwrap().take();
                if let Some(gate) = gate {
                    gate.await.ok();
                }
                if self.real_restart {
                    return crate::runners::bulk_restart::spawn_scoped_fenced(app, &scope, fence).await;
                }
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

    async fn seed_running(app: &Arc<App>, id: &str, host: &str, target: &str) {
        let now = db::now();
        sqlx::query(
            "INSERT INTO cli_updates (id, host, kind, target_version, status, phase, boot, started_at, updated_at)
             VALUES (?, ?, 'codex', ?, 'running', 'starting', ?, ?, ?)",
        )
        .bind(id)
        .bind(host)
        .bind(target)
        .bind(boot())
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .unwrap();
    }

    async fn fail_terminal_updates(app: &Arc<App>) {
        sqlx::query(
            "CREATE TRIGGER fail_cli_update_terminal BEFORE UPDATE OF status ON cli_updates
             WHEN NEW.status IN ('done', 'failed')
             BEGIN SELECT RAISE(ABORT, 'transient terminal write failure'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
    }

    async fn restore_terminal_updates(app: &Arc<App>) {
        sqlx::query("DROP TRIGGER fail_cli_update_terminal")
            .execute(&app.db)
            .await
            .unwrap();
    }

    #[derive(Clone, Copy)]
    enum MarkInstalledFailure {
        RunEnumeration,
        BotRead,
        BotHostRead,
        NoticeWrite,
        CompareAndSwapLost,
    }

    #[derive(Clone, Copy)]
    enum PendingLookupFailure {
        ActiveRuns,
        Bot,
        BotHost,
    }

    fn test_host_cfg(host: &str) -> crate::config::HostCfg {
        crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: format!("target-{host}"),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }
    }

    async fn configure_codex_target(
        env: &crate::testing::Env,
        host: &str,
        notice_target: &str,
        snapshot_target: &str,
    ) -> (String, String) {
        env.app.hosts.insert_remote_for_test(test_host_cfg(host)).await;
        let notice = crate::codex_update::pending_text(Some("0.158.0"), notice_target);
        let (bot, run) = codex_bot_with_notice(env, host, &notice).await;
        sqlx::query("UPDATE projects SET host=? WHERE id=(SELECT project_id FROM bots WHERE id=?)")
            .bind(host)
            .bind(&bot)
            .execute(&env.app.db)
            .await
            .unwrap();
        let status = crate::upstream_update::build_status(
            "codex",
            &Ok(snapshot_target.to_string()),
            &[(host.to_string(), Ok("codex-cli 0.158.0".to_string()))],
            None,
        );
        crate::upstream_update::set_snapshot_for_test(&env.app.upstream_watch, status).await;
        (bot, run)
    }

    async fn fail_pending_target_lookup(
        env: &crate::testing::Env,
        failure: PendingLookupFailure,
    ) {
        let (table, unavailable) = match failure {
            PendingLookupFailure::ActiveRuns => ("runs", "runs_unavailable"),
            PendingLookupFailure::Bot => ("bots", "bots_unavailable"),
            PendingLookupFailure::BotHost => ("projects", "projects_unavailable"),
        };
        sqlx::query(&format!("ALTER TABLE {table} RENAME TO {unavailable}"))
            .execute(&env.app.db)
            .await
            .unwrap();
    }

    async fn restore_pending_target_lookup(
        env: &crate::testing::Env,
        failure: PendingLookupFailure,
    ) {
        let (table, unavailable) = match failure {
            PendingLookupFailure::ActiveRuns => ("runs", "runs_unavailable"),
            PendingLookupFailure::Bot => ("bots", "bots_unavailable"),
            PendingLookupFailure::BotHost => ("projects", "projects_unavailable"),
        };
        sqlx::query(&format!("ALTER TABLE {unavailable} RENAME TO {table}"))
            .execute(&env.app.db)
            .await
            .unwrap();
    }

    async fn assert_pending_lookup_failure_fails_closed(
        host: &str,
        failure: PendingLookupFailure,
    ) {
        let env = crate::testing::env().await;
        configure_codex_target(&env, host, "0.160.0", "0.159.0").await;
        fail_pending_target_lookup(&env, failure).await;
        let fake = Fake::new(&["codex-cli 0.158.0", "codex-cli 0.159.0"], Ok("ok"));

        let result = start(
            &env.app,
            &HeaderMap::new(),
            host,
            Some("codex"),
            Some("0.159.0"),
            fake.clone(),
        )
        .await;
        restore_pending_target_lookup(&env, failure).await;

        let Err(LcError::Unavailable(body)) = result else {
            panic!("pending notice lookup failure must return retryable 503 before the snapshot fallback: {result:?}");
        };
        assert_eq!(body["reason"], "pending_target_unavailable", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(fake.installs(), 0, "an unreadable pending notice must never start the installer");
    }

    #[tokio::test]
    async fn an_active_runs_read_error_does_not_fall_back_to_the_snapshot_target() {
        assert_pending_lookup_failure_fails_closed("cli-update-740-shared", PendingLookupFailure::ActiveRuns).await;
    }

    #[tokio::test]
    async fn a_bot_read_error_does_not_fall_back_to_the_snapshot_target() {
        assert_pending_lookup_failure_fails_closed("cli-update-740-shared", PendingLookupFailure::Bot).await;
    }

    #[tokio::test]
    async fn a_bot_host_read_error_does_not_fall_back_to_the_snapshot_target() {
        assert_pending_lookup_failure_fails_closed("cli-update-740-shared", PendingLookupFailure::BotHost).await;
    }

    #[tokio::test]
    async fn no_pending_notice_can_still_use_the_behind_snapshot_target() {
        let env = crate::testing::env().await;
        let host = "cli-update-740-shared";
        env.app.hosts.insert_remote_for_test(test_host_cfg(host)).await;
        let status = crate::upstream_update::build_status(
            "codex",
            &Ok("0.159.0".to_string()),
            &[(host.to_string(), Ok("codex-cli 0.158.0".to_string()))],
            None,
        );
        crate::upstream_update::set_snapshot_for_test(&env.app.upstream_watch, status).await;
        let fake = Fake::new(&["codex-cli 0.158.0", "codex-cli 0.159.0"], Ok("ok"));

        let result = start(
            &env.app,
            &HeaderMap::new(),
            host,
            Some("codex"),
            Some("0.159.0"),
            fake.clone(),
        )
        .await;

        assert!(result.is_ok(), "successful empty lookup still accepts an out-of-date host snapshot: {result:?}");
        assert!(crate::testing::eventually!(running_list(&env.app).await.is_empty()));
        assert_eq!(fake.installs(), 1);
    }

    /// H is repointed after run_with_fence's last pre-dispatch check, while the restart runner is gated.
    /// The real scoped dispatcher must reject A's stale fence and return an explicit superseded result.
    #[tokio::test]
    async fn a_repoint_between_restart_check_and_dispatch_is_reported_superseded() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = "cli-update-repoint";
        let host_cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        app.hosts.insert_remote_for_test(host_cfg("target-a")).await;
        let fence = app.hosts.fence(host).await.expect("A is configured");
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (bot, run) = codex_bot_with_notice(&env, "cx-repoint", &pending).await;
        sqlx::query("UPDATE projects SET host=? WHERE id=(SELECT project_id FROM bots WHERE id=?)")
            .bind(host)
            .bind(&bot)
            .execute(&app.db)
            .await
            .unwrap();
        seed_running(&app, "u-repoint", host, "0.157.0").await;

        let fake = Arc::new(Fake {
            real_restart: true,
            ..Arc::try_unwrap(Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("installed on A"))).ok().unwrap()
        });
        let (release, gate) = tokio::sync::oneshot::channel();
        *fake.restart_gate.lock().unwrap() = Some(gate);
        let mut rx = app.subscribe();
        let (app_task, runner, host_task, fence_task) = (app.clone(), fake.clone(), host.to_string(), fence.clone());
        let task = tokio::spawn(async move {
            run_with_fence(&app_task, runner.as_ref(), &host_task, "u-repoint", "0.157.0", "codex", Some(fence_task)).await
        });
        assert!(crate::testing::eventually!(fake.restarts().len() == 1), "the dispatch gate is after the final pre-dispatch fence check");

        app.hosts.replace_remote_for_test(&app, host_cfg("target-b")).await;
        release.send(()).unwrap();
        let result = task.await.unwrap();

        assert_eq!(result["ok"], true, "the install on A succeeded: {result}");
        assert_eq!(result["restart_status"], "superseded", "the stale restart must not be reported as handed to B: {result}");
        assert!(result["restart"].is_null(), "no batch belongs to B: {result}");
        assert!(result["restart_error"].as_str().unwrap().contains("沒有把任何 bot 的重啟改投到新主機"), "explain the skipped handoff: {result}");
        assert_eq!(db::active_run(&app.db, &bot).await.unwrap().map(|r| r.id), Some(run), "B's bot stays running");
        assert_eq!(done_events(&mut rx).len(), 1, "the durable update result remains visible");
    }

    async fn install_waits_for_mark_installed_recovery(failure: MarkInstalledFailure) {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx-mark-retry", &pending).await;
        seed_running(&env.app, "u-mark-retry", "local", "0.157.0").await;
        let fake = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("ok"));
        let mut rx = env.app.subscribe();

        match failure {
            MarkInstalledFailure::RunEnumeration => {
                sqlx::query("ALTER TABLE runs RENAME TO runs_unavailable")
                    .execute(&env.app.db)
                    .await
                    .unwrap();
            }
            MarkInstalledFailure::BotRead => {
                sqlx::query("ALTER TABLE bots RENAME TO bots_unavailable")
                    .execute(&env.app.db)
                    .await
                    .unwrap();
            }
            MarkInstalledFailure::BotHostRead => {
                sqlx::query("ALTER TABLE projects RENAME TO projects_unavailable")
                    .execute(&env.app.db)
                    .await
                    .unwrap();
            }
            MarkInstalledFailure::CompareAndSwapLost => {
                sqlx::query(
                    "CREATE TRIGGER ignore_update_notice BEFORE UPDATE OF update_notice ON runs
                     BEGIN SELECT RAISE(IGNORE); END",
                )
                .execute(&env.app.db)
                .await
                .unwrap();
            }
            MarkInstalledFailure::NoticeWrite => {
                sqlx::query(
                    "CREATE TRIGGER fail_update_notice BEFORE UPDATE OF update_notice ON runs
                     BEGIN SELECT RAISE(ABORT, 'transient notice update failure'); END",
                )
                .execute(&env.app.db)
                .await
                .unwrap();
            }
        }

        let app = env.app.clone();
        let runner = fake.clone();
        let task = tokio::spawn(async move { super::run(&app, runner.as_ref(), "local", "u-mark-retry", "0.157.0").await });
        tokio::time::sleep(Duration::from_millis(80)).await;

        let status_before_recovery = row_status(&env.app, "u-mark-retry").await.0;
        let restarts_before_recovery = fake.restarts();
        let done_before_recovery = done_events(&mut rx);

        match failure {
            MarkInstalledFailure::RunEnumeration => {
                sqlx::query("ALTER TABLE runs_unavailable RENAME TO runs")
                    .execute(&env.app.db)
                    .await
                    .unwrap();
            }
            MarkInstalledFailure::BotRead => {
                sqlx::query("ALTER TABLE bots_unavailable RENAME TO bots")
                    .execute(&env.app.db)
                    .await
                    .unwrap();
            }
            MarkInstalledFailure::BotHostRead => {
                sqlx::query("ALTER TABLE projects_unavailable RENAME TO projects")
                    .execute(&env.app.db)
                    .await
                    .unwrap();
            }
            MarkInstalledFailure::CompareAndSwapLost => {
                sqlx::query("DROP TRIGGER ignore_update_notice")
                    .execute(&env.app.db)
                    .await
                    .unwrap();
            }
            MarkInstalledFailure::NoticeWrite => {
                sqlx::query("DROP TRIGGER fail_update_notice")
                    .execute(&env.app.db)
                    .await
                    .unwrap();
            }
        }

        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the install should continue after SQLite recovers")
            .expect("the install task should not panic");
        assert_eq!(status_before_recovery, "running", "DB failure cannot terminalize the update before notice durability");
        assert!(restarts_before_recovery.is_empty(), "DB failure cannot hand off a restart before notice durability");
        assert!(done_before_recovery.is_empty(), "DB failure cannot publish success before notice durability");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["notices_updated"], 1, "{result}");
        assert_eq!(fake.restarts().len(), 1);
        assert!(notice_of(&env.app, &run).await.unwrap().contains("已安裝"));
        assert_eq!(row_status(&env.app, "u-mark-retry").await.0, "done");
        assert_eq!(done_events(&mut rx).len(), 1);
    }

    #[tokio::test]
    async fn a_mark_installed_run_enumeration_failure_waits_for_database_recovery() {
        install_waits_for_mark_installed_recovery(MarkInstalledFailure::RunEnumeration).await;
    }

    #[tokio::test]
    async fn a_mark_installed_bot_read_failure_waits_for_database_recovery() {
        install_waits_for_mark_installed_recovery(MarkInstalledFailure::BotRead).await;
    }

    #[tokio::test]
    async fn a_mark_installed_bot_host_read_failure_waits_for_database_recovery() {
        install_waits_for_mark_installed_recovery(MarkInstalledFailure::BotHostRead).await;
    }

    #[tokio::test]
    async fn a_mark_installed_notice_write_failure_waits_for_database_recovery() {
        install_waits_for_mark_installed_recovery(MarkInstalledFailure::NoticeWrite).await;
    }

    #[tokio::test]
    async fn a_mark_installed_lost_cas_waits_until_the_notice_transition_is_durable() {
        install_waits_for_mark_installed_recovery(MarkInstalledFailure::CompareAndSwapLost).await;
    }

    #[tokio::test]
    async fn recovery_keeps_its_durable_debt_until_mark_installed_succeeds() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx-recover-mark", &pending).await;
        let orphan = orphan_row(&env.app, "local", "0.157.0", "verifying", Some("0.155.1")).await;
        let fake = Fake::new(&["codex-cli 0.157.0"], Ok("unused"));
        let mut rx = env.app.subscribe();
        sqlx::query(
            "CREATE TRIGGER fail_update_notice BEFORE UPDATE OF update_notice ON runs
             BEGIN SELECT RAISE(ABORT, 'transient notice update failure'); END",
        )
        .execute(&env.app.db)
        .await
        .unwrap();

        let app = env.app.clone();
        let runner = fake.clone();
        let orphan_for_task = orphan.clone();
        let task = tokio::spawn(async move {
            recover(
                &app,
                runner.as_ref(),
                &orphan_for_task,
                "local",
                "0.157.0",
                Some("0.155.1"),
                Duration::from_millis(1),
                Duration::from_secs(5),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(80)).await;

        let status_before_recovery = row_status(&env.app, &orphan).await.0;
        let notice_before_recovery = notice_of(&env.app, &run).await.unwrap();
        let done_before_recovery = done_events(&mut rx);
        sqlx::query("DROP TRIGGER fail_update_notice")
            .execute(&env.app.db)
            .await
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("startup recovery should resume when notice writes recover")
            .expect("the recovery worker should not panic");
        assert_eq!(status_before_recovery, "running", "recovery debt must stay durable during the DB fault");
        assert_eq!(notice_before_recovery, pending, "failed notice writes must leave the pending notice intact");
        assert!(done_before_recovery.is_empty(), "recovery cannot publish success before the notice transition");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["recovered"], true);
        assert!(notice_of(&env.app, &run).await.unwrap().contains("已安裝"));
        assert_eq!(row_status(&env.app, &orphan).await.0, "done");
        assert_eq!(done_events(&mut rx).len(), 1);
    }

    async fn assert_slot_is_held(app: &Arc<App>, host: &str) {
        let now = db::now();
        let err = sqlx::query(
            "INSERT INTO cli_updates (id, host, kind, target_version, status, phase, boot, started_at, updated_at)
             VALUES ('slot-probe', ?, 'codex', '0.157.0', 'running', 'starting', ?, ?, ?)",
        )
        .bind(host)
        .bind(boot())
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .expect_err("the running update must retain the host slot until its terminal result commits");
        assert!(err.as_database_error().is_some_and(|e| e.is_unique_violation()), "{err}");
    }

    async fn assert_slot_is_released(app: &Arc<App>, host: &str) {
        let now = db::now();
        sqlx::query(
            "INSERT INTO cli_updates (id, host, kind, target_version, status, phase, boot, started_at, updated_at)
             VALUES ('slot-probe', ?, 'codex', '0.157.0', 'running', 'starting', ?, ?, ?)",
        )
        .bind(host)
        .bind(boot())
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .expect("terminal commit must release the host slot");
    }

    async fn row_phase(app: &Arc<App>, id: &str) -> String {
        sqlx::query_scalar("SELECT phase FROM cli_updates WHERE id = ?")
            .bind(id)
            .fetch_one(&app.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_successful_terminal_write_is_retried_before_done_and_releases_the_slot() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, _run) = codex_bot_with_notice(&env, "cx", &pending).await;
        seed_running(&env.app, "u-finish-ok", "local", "0.157.0").await;
        let fake = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("installed"));
        let (release, gate) = tokio::sync::oneshot::channel();
        *fake.restart_gate.lock().unwrap() = Some(gate);
        let mut rx = env.app.subscribe();
        let (app, f) = (env.app.clone(), fake.clone());
        let task = tokio::spawn(async move {
            super::run(&app, f.as_ref(), "local", "u-finish-ok", "0.157.0").await
        });

        assert!(crate::testing::eventually!(fake.restarts().len() == 1), "run must reach the terminal path");
        fail_terminal_updates(&env.app).await;
        release.send(()).unwrap();
        // 等它真的走到 finish_row，不賭固定 100ms：整樹平行跑負載高時還停在前一個 phase（#759，d753af7e）。
        assert!(
            crate::testing::eventually!(row_phase(&env.app, "u-finish-ok").await == "finishing"),
            "run must reach the owed terminal write"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let done_before_commit = done_events(&mut rx);
        let was_running = row_status(&env.app, "u-finish-ok").await.0 == "running";
        let phase_while_owed = row_phase(&env.app, "u-finish-ok").await;
        assert_slot_is_held(&env.app, "local").await;

        restore_terminal_updates(&env.app).await;
        assert!(crate::testing::eventually!(task.is_finished()), "terminal debt should settle after DB recovery");
        let result = task.await.unwrap();
        let (status, stored) = row_status(&env.app, "u-finish-ok").await;
        let done_after_commit = done_events(&mut rx);
        assert!(done_before_commit.is_empty(), "done must wait for the DB commit: {done_before_commit:?}");
        assert!(was_running, "the row must remain running while the terminal write is owed");
        assert_eq!(phase_while_owed, "finishing", "the owed terminal write should be visible in API state");
        assert_eq!(status, "done");
        assert_eq!(stored, result);
        assert_eq!(done_after_commit, [result]);
        assert_slot_is_released(&env.app, "local").await;
    }

    #[tokio::test]
    async fn a_failed_terminal_write_is_retried_before_done_and_releases_the_slot() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        seed_running(&env.app, "u-finish-failed", "local", "0.157.0").await;
        let fake = Fake::new(&["codex-cli 0.155.1"], Err("curl: (6) Could not resolve host"));
        let (release, gate) = tokio::sync::oneshot::channel();
        *fake.gate.lock().unwrap() = Some(gate);
        let mut rx = env.app.subscribe();
        let (app, f) = (env.app.clone(), fake.clone());
        let task = tokio::spawn(async move {
            super::run(&app, f.as_ref(), "local", "u-finish-failed", "0.157.0").await
        });

        assert!(crate::testing::eventually!(fake.installs() == 1), "installer must start before injecting the failure");
        fail_terminal_updates(&env.app).await;
        release.send(()).unwrap();
        // 等它真的走到 finish_row，不賭固定 100ms：整樹平行跑負載高時還停在前一個 phase（#759，d753af7e）。
        assert!(
            crate::testing::eventually!(row_phase(&env.app, "u-finish-failed").await == "finishing"),
            "run must reach the owed terminal write"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let done_before_commit = done_events(&mut rx);
        let was_running = row_status(&env.app, "u-finish-failed").await.0 == "running";
        let phase_while_owed = row_phase(&env.app, "u-finish-failed").await;
        assert_slot_is_held(&env.app, "local").await;

        restore_terminal_updates(&env.app).await;
        assert!(crate::testing::eventually!(task.is_finished()), "failed terminal debt should settle after DB recovery");
        let result = task.await.unwrap();
        let (status, stored) = row_status(&env.app, "u-finish-failed").await;
        let done_after_commit = done_events(&mut rx);
        assert!(done_before_commit.is_empty(), "failure must wait for the DB commit: {done_before_commit:?}");
        assert!(was_running, "the row must remain running while the terminal write is owed");
        assert_eq!(phase_while_owed, "finishing", "the owed terminal write should be visible in API state");
        assert_eq!(result["reason"], "install_failed");
        assert_eq!(status, "failed");
        assert_eq!(stored, result);
        assert_eq!(done_after_commit, [result]);
        assert_eq!(notice_of(&env.app, &run).await, Some(pending));
        assert_slot_is_released(&env.app, "local").await;
    }

    #[tokio::test]
    async fn a_terminal_cas_loss_publishes_only_the_durable_result() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, _run) = codex_bot_with_notice(&env, "cx", &pending).await;
        seed_running(&env.app, "u-finish-cas", "local", "0.157.0").await;
        let fake = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("installed"));
        let (release, gate) = tokio::sync::oneshot::channel();
        *fake.restart_gate.lock().unwrap() = Some(gate);
        let mut rx = env.app.subscribe();
        let (app, f) = (env.app.clone(), fake.clone());
        let task = tokio::spawn(async move {
            super::run(&app, f.as_ref(), "local", "u-finish-cas", "0.157.0").await
        });

        assert!(crate::testing::eventually!(fake.restarts().len() == 1), "run must reach the terminal path");
        let durable = json!({"update_id": "u-finish-cas", "host": "local", "kind": "codex",
            "target_version": "0.157.0", "ok": false, "reason": "other_terminalizer", "error": "stored result wins"});
        let now = db::now();
        assert_eq!(sqlx::query("UPDATE cli_updates SET status='failed', result=?, updated_at=?, finished_at=? WHERE id=? AND status='running'")
            .bind(durable.to_string()).bind(&now).bind(&now).bind("u-finish-cas")
            .execute(&env.app.db).await.unwrap().rows_affected(), 1);
        release.send(()).unwrap();
        assert!(crate::testing::eventually!(task.is_finished()), "the terminal CAS result should be reconciled");
        let result = task.await.unwrap();

        assert_eq!(result, durable, "a lost CAS must return the durable winner, not the local success");
        let events = done_events(&mut rx);
        assert_eq!(events.len(), 1, "only one terminal event should be published");
        assert_eq!(events[0], durable, "only the durable terminal result may be published");
        let (status, stored) = row_status(&env.app, "u-finish-cas").await;
        assert_eq!(status, "failed");
        assert_eq!(stored, durable);
    }

    #[tokio::test]
    async fn a_successful_install_flips_the_notice_and_starts_the_codex_batch_for_that_host() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        seed_running(&env.app, "u-1", "local", "0.157.0").await;
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
    async fn a_missing_durable_update_row_runs_no_cli_commands() {
        let env = crate::testing::env().await;
        let fake = Fake::new(&["codex-cli 0.155.1"], Ok("should not install"));

        let result = super::run(&env.app, fake.as_ref(), "local", "missing-update", "0.157.0").await;

        assert_eq!(result["reason"], "superseded", "a missing durable job must stop before using its kind: {result}");
        assert_eq!(*fake.version_reads.lock().unwrap(), 0, "a missing job must not query any CLI version");
        assert_eq!(fake.installs(), 0, "a missing job must not run any installer");
    }

    #[tokio::test]
    async fn a_claude_kind_read_error_fails_the_job_without_running_either_installer() {
        let env = crate::testing::env().await;
        seed_running(&env.app, "u-kind-read-error", "local", "2.1.284").await;
        sqlx::query("UPDATE cli_updates SET kind='claude' WHERE id=?")
            .bind("u-kind-read-error")
            .execute(&env.app.db)
            .await
            .unwrap();
        let fake = Fake::new(&["2.1.281"], Ok("should not install"));
        let fence = env.app.hosts.fence("local").await;

        let result = super::run_with_kind_result(
            &env.app,
            fake.as_ref(),
            "local",
            "u-kind-read-error",
            "2.1.284",
            "claude",
            Err(sqlx::Error::Protocol("injected kind read failure".into())),
            fence,
        )
        .await;

        assert_eq!(result["kind"], "claude");
        assert_eq!(result["reason"], "internal_error", "a transient kind read error is retryable: {result}");
        assert!(result["error"].as_str().unwrap().contains("injected kind read failure"));
        assert_eq!(*fake.version_reads.lock().unwrap(), 0, "kind must be verified before any CLI version command");
        assert_eq!(fake.installs(), 0, "neither the Codex nor Claude installer may run");
        let (status, durable) = row_status(&env.app, "u-kind-read-error").await;
        assert_eq!(status, "failed");
        assert_eq!(durable["reason"], "internal_error");
    }

    #[tokio::test]
    async fn a_durable_kind_mismatch_fails_before_any_cli_command() {
        let env = crate::testing::env().await;
        seed_running(&env.app, "u-kind-mismatch", "local", "2.1.284").await;
        let fake = Fake::new(&["2.1.281"], Ok("should not install"));
        let fence = env.app.hosts.fence("local").await;

        let result = super::run_with_kind_result(
            &env.app,
            fake.as_ref(),
            "local",
            "u-kind-mismatch",
            "2.1.284",
            "claude",
            Ok(Some("codex".to_string())),
            fence,
        )
        .await;

        assert_eq!(result["reason"], "internal_error", "a changed kind must be reported clearly: {result}");
        assert!(result["error"].as_str().unwrap().contains("kind 不一致"));
        assert_eq!(*fake.version_reads.lock().unwrap(), 0, "kind mismatch must be rejected before CLI commands");
        assert_eq!(fake.installs(), 0);
        assert_eq!(row_status(&env.app, "u-kind-mismatch").await.0, "failed");
    }

    #[tokio::test]
    async fn a_failed_install_restarts_nothing_and_says_why() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx", &pending).await;
        seed_running(&env.app, "u-2", "local", "0.157.0").await;
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
        seed_running(&env.app, "u-3", "local", "0.157.0").await;
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
        seed_running(&env.app, "u-4", "local", "0.157.0").await;
        let v = super::run(&env.app, fake.as_ref(), "local", "u-4", "0.157.0").await;
        assert_eq!(v["reason"], "version_unreadable");
        assert_eq!(fake.installs(), 0, "不知道現在的版本就不裝");

        let fake = Fake::new(&["codex-cli 0.155.1"], Ok("ok"));
        seed_running(&env.app, "u-5", "local", "0.157.0").await;
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

        assert_eq!(mark_installed(&env.app, "local", "0.155.1", "0.157.0").await.unwrap(), 1);
        assert!(notice_of(&env.app, &here).await.unwrap().contains("已安裝"));
        assert_eq!(notice_of(&env.app, &far_run).await, Some(pending), "別台主機的 binary 沒換");
        assert_eq!(notice_of(&env.app, &claude_run).await.as_deref(), Some("Update installed · Restart to update"));
    }

    #[tokio::test]
    async fn a_successfully_proven_empty_mark_installed_set_is_a_valid_noop() {
        let env = crate::testing::env().await;
        let (_bot, run) = codex_bot_with_notice(&env, "cx-no-pending-update", "Codex is up to date").await;

        assert_eq!(mark_installed(&env.app, "local", "0.155.1", "0.157.0").await.unwrap(), 0);
        assert_eq!(notice_of(&env.app, &run).await.as_deref(), Some("Codex is up to date"));
    }

    /// #347：A 機安裝還在跑時同名主機改指到 B（`?confirm=repoint`）。A 裝完之後不能讀 B 的版本當成「裝好了」、
    /// 不能改 B 的通知、不能重啟 B 的 codex，結果要講明作廢。
    #[tokio::test]
    async fn an_update_whose_host_was_repointed_mid_install_touches_nothing_on_the_new_target() {
        let env = crate::testing::env().await;
        let host = "cx-347";
        let cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
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
        seed_running(&env.app, "u-347", host, "0.157.0").await;
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

    /// #605：the last standalone fence check succeeds, then H is repointed before the notice CAS.
    /// The CAS must share authority serialization with the repoint so A cannot mutate the H row after B wins.
    #[tokio::test]
    async fn a_repoint_after_notice_fence_check_does_not_commit_the_old_authoritys_notice() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = "cli-update-notice-repoint";
        let cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (bot, run) = codex_bot_with_notice(&env, "cx-notice-repoint", &pending).await;
        sqlx::query("UPDATE projects SET host=? WHERE id=(SELECT project_id FROM bots WHERE id=?)")
            .bind(host)
            .bind(&bot)
            .execute(&app.db)
            .await
            .unwrap();
        seed_running(&app, "u-notice-repoint", host, "0.157.0").await;

        let app_to_repoint = app.clone();
        let replacement = cfg("target-b");
        crate::app_ports_p12::race_point::arm("cli_update_before_notice_cas", &run, move || async move {
            app_to_repoint.hosts.replace_remote_for_test(&app_to_repoint, replacement).await;
        });

        let fake = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("installed on A"));
        let mut rx = app.subscribe();
        let result = super::run(&app, fake.as_ref(), host, "u-notice-repoint", "0.157.0").await;

        assert_eq!(result["ok"], false, "a stale install must not report success: {result}");
        assert_eq!(result["reason"], "superseded", "a stale host authority must fail closed: {result}");
        assert_eq!(notice_of(&app, &run).await, Some(pending), "A must not commit its installed notice after H points to B");
        assert!(fake.restarts().is_empty(), "a stale install must not restart bots on B");
        let events = done_events(&mut rx);
        assert_eq!(events.len(), 1, "publish one terminal result after the stale CAS is refused");
        assert_eq!(events[0]["reason"], "superseded");
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
        assert!(matches!(start(&env.app, &ui, "local", Some("claude"), Some("0.157.0"), fake.clone()).await, Err(LcError::Conflict(ref v)) if v["reason"] == "stale_target")
        );
        assert!(matches!(
            start(&env.app, &ui, "local", None, Some("0.157.0"), fake.clone()).await,
            Err(LcError::Bad(_))));
        assert!(matches!(start(&env.app, &ui,
                "nowhere",
                Some("codex"),
                Some("0.157.0"),
                fake.clone()
            )
            .await,
            Err(LcError::NotFound(_))
        ));
        assert_eq!(fake.installs(), 0, "被拒絕的請求什麼都不跑");
    }

    #[tokio::test]
    async fn claude_install_uses_one_pinned_target_and_waits_for_a_separate_restart_confirmation() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "claude-install").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        let pending = "claude 有新版 2.1.281 → 2.1.284，需安裝後重啟";
        sqlx::query("UPDATE runs SET update_notice=?, status_json=? WHERE id=?")
            .bind(pending)
            .bind(r#"{"version":"2.1.281 (Claude Code)"}"#)
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();
        crate::upstream_update::set_snapshot_for_test(&env.app.upstream_watch, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[("local".into(), Ok("2.1.281 (Claude Code)".into()))], None,
        ))
        .await;
        let fake = Fake::new(
            &["2.1.281 (Claude Code)", "2.1.284 (Claude Code)"],
            Ok("stub installer"),
        );

        let started = start(
            &env.app,
            &HeaderMap::new(),
            "local",
            Some("claude"),
            Some("2.1.284"), fake.clone(),
        )
        .await
        .expect("Claude 指定版本安裝要被接受");
        assert_eq!(started["kind"], "claude");
        assert!(
            crate::testing::eventually!(running_list(&env.app).await.is_empty()),
            "安裝流程完成"
        );
        let kind: String = sqlx::query_scalar("SELECT kind FROM cli_updates WHERE id=?")
            .bind(started["update_id"].as_str().unwrap())
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(kind, "claude", "持久列要保留 CLI 種類");
        assert!(fake.restarts().is_empty(), "Claude 安裝成功不得自動重啟");
        let notice = notice_of(&env.app, &run).await.unwrap();
        assert!(
            notice.contains("已安裝") && notice.contains("重啟套用"),
            "{notice}"
        );
        let result = row_status(&env.app, started["update_id"].as_str().unwrap())
            .await
            .1;
        assert_eq!(result["restart_required"], true);
        assert!(result["restart"].is_null());
    }

    #[tokio::test]
    async fn claude_install_does_not_downgrade_a_host_that_overtook_the_snapshot_target() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "claude-newer-than-snapshot").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        let pending = crate::upstream_update::claude_pending_text(Some("2.1.281"), "2.1.284");
        sqlx::query("UPDATE runs SET update_notice=?, status_json=? WHERE id=?")
            .bind(&pending)
            .bind(r#"{"version":"2.1.281 (Claude Code)"}"#)
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();
        crate::upstream_update::set_snapshot_for_test(&env.app.upstream_watch, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[("local".into(), Ok("2.1.281".into()))],
            None,
        ))
        .await;
        let fake = Fake::new(&["2.1.285"], Ok("must not run"));

        let started = start(
            &env.app,
            &HeaderMap::new(),
            "local",
            Some("claude"),
            Some("2.1.284"),
            fake.clone(),
        )
        .await
        .unwrap();
        assert!(crate::testing::eventually!(running_list(&env.app).await.is_empty()));

        assert_eq!(fake.installs(), 0, "快照落後時不可把已安裝的較新 Claude 降版");
        let result = row_status(&env.app, started["update_id"].as_str().unwrap())
            .await
            .1;
        assert_eq!(result["ok"], true, "已在目標以上就只記錄實際版本：{result}");
        assert_eq!(result["already_installed"], true);
        assert_eq!(result["to"], "2.1.285");
        let notice = notice_of(&env.app, &run).await.unwrap();
        assert!(notice.contains("已安裝") && notice.contains("重啟套用"), "{notice}");
        assert!(
            crate::upstream_update::latest_target_for_host(&env.app, "claude", "local")
                .await
                .is_none(),
            "套用後快照要反映主機已超前原目標"
        );
    }

    #[tokio::test]
    async fn claude_install_keeps_restart_notice_when_pending_notice_had_no_from_version() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "claude-install-no-from").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        let pending = crate::upstream_update::claude_pending_text(None, "2.1.284");
        sqlx::query("UPDATE runs SET update_notice=? WHERE id=?")
            .bind(&pending)
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();

        let updated = mark_installed_with_fence(
            &env.app,
            "local",
            "claude",
            "2.1.281",
            "2.1.284",
            None,
        )
        .await
        .unwrap();

        assert_eq!(updated, Some(1));
        let notice = notice_of(&env.app, &run)
            .await
            .expect("安裝成功後仍要留下需要重啟套用的持續提示");
        assert!(
            notice.contains("2.1.281") && notice.contains("已安裝") && notice.contains("重啟套用"),
            "{notice}"
        );
    }

    #[tokio::test]
    async fn claude_install_rejects_a_version_above_the_exact_confirmed_target() {
        let env = crate::testing::env().await;
        let bot =
            crate::testing::claude_bot(&env.app, &env.project_id, "claude-exact-target").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        let pending = crate::upstream_update::claude_pending_text(Some("2.1.281"), "2.1.284");
        sqlx::query("UPDATE runs SET update_notice=? WHERE id=?")
            .bind(&pending)
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();
        crate::upstream_update::set_snapshot_for_test(&env.app.upstream_watch, crate::upstream_update::build_status(
            "claude",
            &Ok("2.1.284".into()),
            &[("local".into(), Ok("2.1.281".into()))],
            None,
        ))
        .await;
        let fake = Fake::new(&["2.1.281", "2.1.285"], Ok("stub installer"));
        let started = start(
            &env.app,
            &HeaderMap::new(),
            "local",
            Some("claude"),
            Some("2.1.284"),
            fake.clone(),
        )
        .await
        .unwrap();
        assert!(crate::testing::eventually!(running_list(&env.app)
            .await
            .is_empty()));
        let result = row_status(&env.app, started["update_id"].as_str().unwrap())
            .await
            .1;
        assert_eq!(
            result["reason"], "target_not_reached",
            "Claude target must match exactly: {result}"
        );
        assert!(fake.restarts().is_empty());
        assert_eq!(
            notice_of(&env.app, &run).await.as_deref(),
            Some(pending.as_str())
        );
    }

    #[test]
    fn claude_install_command_runs_only_a_path_stub_with_the_exact_target() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-claude-install-stub-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("claude");
        let captured = dir.join("args");
        std::fs::write(
            &bin,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAPTURE_ARGS\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let command =
            claude_install_command("2.1.284").expect("valid version has a pinned command");
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env("PATH", &dir)
            .env("CAPTURE_ARGS", &captured)
            .output()
            .unwrap();
        let args = std::fs::read_to_string(&captured).unwrap_or_default();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            output.status.success(),
            "stub command failed: {}",
            String::from_utf8_lossy(&output.stderr));
        assert_eq!(args, "install\n2.1.284\n");
        assert!(
            claude_install_command("2.1.284; touch /tmp/unwanted").is_none(),
            "target cannot inject shell syntax"
        );
    }

    #[tokio::test]
    async fn a_started_update_persists_its_host_target_for_recovery() {
        let env = crate::testing::env().await;
        let host = "start-authority-347";
        let cfg = crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: "target-a".into(),
            ssh_port: 2222,
            ssh_opts: vec![],
            herdr_session: "codex-work".into(),
            remote_path: String::new(),
        };
        env.app.hosts.insert_remote_for_test(cfg).await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (bot, _) = codex_bot_with_notice(&env, "cx-start-authority-347", &pending).await;
        sqlx::query("UPDATE projects SET host=? WHERE id=(SELECT project_id FROM bots WHERE id=?)")
            .bind(host)
            .bind(&bot)
            .execute(&env.app.db)
            .await
            .unwrap();

        let started = start(
            &env.app,
            &HeaderMap::new(),
            host,
            Some("codex"),
            Some("0.157.0"),
            Fake::new(&["codex-cli 0.157.0"], Ok("already installed")),
        )
        .await
        .unwrap();
        let host_target: Option<String> = sqlx::query_scalar("SELECT host_target FROM cli_updates WHERE id=?")
            .bind(started["update_id"].as_str().unwrap())
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(host_target.as_deref(), Some("target-a:2222/codex-work"));
    }

    /// herdr 一鍵更新在跑（server 要重啟、所有 pane 會消失）：同一台不能同時裝 CLI 再 scoped 重啟 bot。
    /// 被擋的不留 `running` 那一列（不然這台的 cli-update 從此被自己的殘列卡住）。
    #[tokio::test]
    async fn a_running_herdr_update_blocks_a_cli_install_on_that_host_and_leaves_no_row() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let _ = codex_bot_with_notice(&env, "cx", &pending).await;
        let _held = crate::herdr_upgrade::hold_slot_for_test(&env.app);
        let fake = Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("ok"));
        let err = start(&env.app, &HeaderMap::new(), "local", Some("codex"), Some("0.157.0"), fake.clone()).await.unwrap_err();
        assert!(matches!(&err, LcError::Conflict(v) if v["reason"] == "herdr_update_in_progress"), "{err:?}");
        assert_eq!(fake.installs(), 0);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cli_updates WHERE host = 'local'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(rows, 0, "被擋的不留列");
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
        seed_running(&env.app, "u-6", "local", "0.157.0").await;
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
        seed_running(&env.app, "u-7", "local", "0.157.0").await;
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
        seed_running(&env.app, "u-8", "local", "0.157.0").await;
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
        let host_target = app.hosts.get(host).await.map(|conn| host_target_for_conn(&conn));
        sqlx::query(
            "INSERT INTO cli_updates (id, host, kind, target_version, status, phase, from_version, boot, started_at, updated_at, host_target)
             VALUES (?, ?, 'codex', ?, 'running', ?, ?, 'dead-daemon', ?, ?, ?)",
        )
        .bind(&id)
        .bind(host)
        .bind(target)
        .bind(phase)
        .bind(from)
        .bind(db::now())
        .bind(db::now())
        .bind(host_target)
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

    /// #347: startup recovery must compare the durable operation authority before trusting the current same-name host.
    #[tokio::test]
    async fn recovery_does_not_adopt_a_repointed_host_that_already_has_the_target_version() {
        let env = crate::testing::env().await;
        let host = "recover-347";
        let cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        env.app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let orphan = orphan_row(&env.app, host, "0.157.0", "installing", Some("0.155.1")).await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (bot, run) = codex_bot_with_notice(&env, "cx-recover-347", &pending).await;
        sqlx::query("UPDATE projects SET host=? WHERE id=(SELECT project_id FROM bots WHERE id=?)")
            .bind(host)
            .bind(&bot)
            .execute(&env.app.db)
            .await
            .unwrap();

        env.app.hosts.replace_remote_for_test(&env.app, cfg("target-b")).await;
        let fake = Fake::new(&["codex-cli 0.157.0"], Ok("already installed on B"));
        let v = recover(&env.app, fake.as_ref(), &orphan, host, "0.157.0", Some("0.155.1"), Duration::ZERO, Duration::from_secs(5)).await;

        assert_eq!(v["ok"], false, "recovery cannot adopt B's installed version: {v}");
        assert_eq!(v["reason"], "host_changed", "the result must identify the durable authority mismatch: {v}");
        assert_eq!(*fake.probes.lock().unwrap(), 0, "do not inspect the current host after the recorded target changed");
        assert_eq!(*fake.version_reads.lock().unwrap(), 0, "do not read B's version as A's result");
        assert_eq!(notice_of(&env.app, &run).await, Some(pending), "B's notice must remain untouched");
        let (status, result) = row_status(&env.app, &orphan).await;
        assert_eq!(status, "failed");
        assert_eq!(result["reason"], "host_changed");
    }

    #[tokio::test]
    async fn recovery_fails_closed_when_a_legacy_row_has_no_durable_host_target() {
        let env = crate::testing::env().await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (_bot, run) = codex_bot_with_notice(&env, "cx-recover-legacy-347", &pending).await;
        let orphan = orphan_row(&env.app, "local", "0.157.0", "installing", Some("0.155.1")).await;
        sqlx::query("UPDATE cli_updates SET host_target=NULL WHERE id=?")
            .bind(&orphan)
            .execute(&env.app.db)
            .await
            .unwrap();
        let fake = Fake::new(&["codex-cli 0.157.0"], Ok("already installed"));

        let v = recover(&env.app, fake.as_ref(), &orphan, "local", "0.157.0", Some("0.155.1"), Duration::ZERO, Duration::from_secs(5)).await;

        assert_eq!(v["ok"], false, "missing durable authority must not be inferred from the current host: {v}");
        assert_eq!(v["reason"], "host_changed");
        assert_eq!(*fake.probes.lock().unwrap(), 0);
        assert_eq!(*fake.version_reads.lock().unwrap(), 0);
        assert_eq!(notice_of(&env.app, &run).await, Some(pending));
    }

    #[tokio::test]
    async fn recovery_rejects_an_unsupported_durable_kind_before_any_host_probe() {
        let env = crate::testing::env().await;
        let orphan = orphan_row(&env.app, "local", "0.157.0", "installing", Some("0.155.1")).await;
        sqlx::query("UPDATE cli_updates SET kind='unknown' WHERE id=?")
            .bind(&orphan)
            .execute(&env.app.db)
            .await
            .unwrap();
        let fake = Fake::new(&["codex-cli 0.157.0"], Ok("must not install"));

        let result = recover(
            &env.app,
            fake.as_ref(),
            &orphan,
            "local",
            "0.157.0",
            Some("0.155.1"),
            Duration::ZERO,
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(result["kind"], "unknown");
        assert_eq!(result["reason"], "internal_error", "recovery must reject unknown durable kinds: {result}");
        assert_eq!(*fake.probes.lock().unwrap(), 0, "recovery must validate kind before probing host install locks");
        assert_eq!(*fake.version_reads.lock().unwrap(), 0);
        assert_eq!(fake.installs(), 0);
        assert_eq!(row_status(&env.app, &orphan).await.0, "failed");
    }

    #[tokio::test]
    async fn recovery_rejects_a_kind_changed_after_startup_scan() {
        let env = crate::testing::env().await;
        let orphan = orphan_row(&env.app, "local", "2.1.284", "installing", Some("2.1.281")).await;
        let host_target: Option<String> = sqlx::query_scalar("SELECT host_target FROM cli_updates WHERE id=?")
            .bind(&orphan)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        let fake = Fake::new(&["2.1.284"], Ok("must not install"));

        let result = recover_with_registry(
            &env.app,
            fake.as_ref(),
            &orphan,
            "local",
            "claude",
            "2.1.284",
            Some("2.1.281"),
            Duration::ZERO,
            Duration::from_secs(5),
            host_target.as_deref(),
            None,
        )
        .await;

        assert_eq!(result["reason"], "internal_error", "recovery must match the startup row kind: {result}");
        assert!(result["error"].as_str().unwrap().contains("kind 不一致"));
        assert_eq!(*fake.probes.lock().unwrap(), 0, "kind mismatch must be rejected before host probes");
        assert_eq!(*fake.version_reads.lock().unwrap(), 0);
        assert_eq!(row_status(&env.app, &orphan).await.0, "failed");
    }

    #[tokio::test]
    async fn recovery_does_not_probe_a_row_removed_after_startup_scan() {
        let env = crate::testing::env().await;
        let orphan = orphan_row(&env.app, "local", "0.157.0", "installing", Some("0.155.1")).await;
        let host_target: Option<String> = sqlx::query_scalar("SELECT host_target FROM cli_updates WHERE id=?")
            .bind(&orphan)
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM cli_updates WHERE id=?")
            .bind(&orphan)
            .execute(&env.app.db)
            .await
            .unwrap();
        let fake = Fake::new(&["codex-cli 0.157.0"], Ok("must not install"));

        let result = recover_with_registry(
            &env.app,
            fake.as_ref(),
            &orphan,
            "local",
            "codex",
            "0.157.0",
            Some("0.155.1"),
            Duration::ZERO,
            Duration::from_secs(5),
            host_target.as_deref(),
            None,
        )
        .await;

        assert_eq!(result["reason"], "superseded", "a deleted recovery row must not be adopted: {result}");
        assert_eq!(*fake.probes.lock().unwrap(), 0, "a missing row must stop before host probes");
        assert_eq!(*fake.version_reads.lock().unwrap(), 0);
        assert_eq!(fake.installs(), 0);
    }

    #[tokio::test]
    async fn claude_recovery_requires_the_exact_target_and_never_restarts() {
        let env = crate::testing::env().await;
        let pending = crate::upstream_update::claude_pending_text(Some("2.1.281"), "2.1.284");
        let bot =
            crate::testing::claude_bot(&env.app, &env.project_id, "claude-recover-exact").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET update_notice=? WHERE id=?")
            .bind(&pending)
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();
        let orphan = orphan_row(&env.app, "local", "2.1.284", "verifying", Some("2.1.281")).await;
        sqlx::query("UPDATE cli_updates SET kind='claude' WHERE id=?")
            .bind(&orphan)
            .execute(&env.app.db)
            .await
            .unwrap();
        let fake = Fake::new(&["2.1.285"], Ok("unused"));
        let result = recover(
            &env.app,
            fake.as_ref(),
            &orphan,
            "local",
            "2.1.284",
            Some("2.1.281"),
            Duration::ZERO,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            result["reason"], "interrupted",
            "recovery must reject a non-exact Claude version: {result}"
        );
        assert!(fake.restarts().is_empty());
        assert_eq!(
            notice_of(&env.app, &run).await.as_deref(),
            Some(pending.as_str())
        );
    }

    /// #564 reopened: a transient failure during the first startup enumeration must not strand the durable row until another restart.
    #[tokio::test]
    async fn a_failed_startup_enumeration_is_retried_after_the_db_recovers() {
        let env = crate::testing::env().await;
        let orphan = orphan_row(&env.app, "local", "0.157.0", "installing", None).await;
        let mut rx = env.app.subscribe();
        let fake = Arc::new(Fake {
            probe_hold: Duration::from_millis(80),
            ..Arc::try_unwrap(Fake::new(&["codex-cli 0.157.0"], Ok("already installed"))).ok().unwrap()
        });

        // Make the startup SELECT fail while keeping the row intact, then restore the table without restarting the app.
        sqlx::query("ALTER TABLE cli_updates RENAME TO cli_updates_unavailable")
            .execute(&env.app.db)
            .await
            .unwrap();
        let sweeper = start_recovery_sweeper(
            &env.app,
            fake.clone(),
            Duration::from_millis(2),
            Duration::from_millis(8),
            Duration::from_millis(2),
        )
        .await;
        sqlx::query("ALTER TABLE cli_updates_unavailable RENAME TO cli_updates")
            .execute(&env.app.db)
            .await
            .unwrap();

        let terminal = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if row_status(&env.app, &orphan).await.0 != "running" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        sweeper.abort();
        assert!(terminal.is_ok(), "a recovered database must be re-enumerated in the same daemon boot");
        assert_ne!(row_status(&env.app, &orphan).await.0, "running");
        assert_eq!(done_events(&mut rx).len(), 1, "one update id must have exactly one recovery worker");
        assert_eq!(*fake.probes.lock().unwrap(), 1, "overlapping sweeps must not start duplicate recovery workers");
        assert_eq!(*fake.version_reads.lock().unwrap(), 1, "the recovered update must only be reconciled once");
        assert!(running_list(&env.app).await.is_empty(), "terminal recovery must release the host slot");
        let now = db::now();
        sqlx::query(
            "INSERT INTO cli_updates (id, host, kind, target_version, status, phase, boot, started_at, updated_at)
             VALUES (?, 'local', 'codex', '0.158.0', 'running', 'starting', ?, ?, ?)",
        )
        .bind(db::ulid())
        .bind(boot())
        .bind(&now)
        .bind(&now)
        .execute(&env.app.db)
        .await
        .expect("the host slot must accept a later update after recovery commits");
    }

    struct ProbeBarrier {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    struct ProbeGatedRunner {
        inner: Arc<Fake>,
        barrier: Arc<ProbeBarrier>,
    }

    impl Runner for ProbeGatedRunner {
        fn installer_busy<'a>(
            &'a self,
            _app: &'a Arc<App>,
            _host: &'a str,
            _kind: &'a str,
            _fence: &'a HostFence,
        ) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async move {
                let probe = {
                    let mut probes = self.inner.probes.lock().unwrap();
                    *probes += 1;
                    *probes
                };
                if probe == 1 {
                    self.barrier.entered.notify_one();
                    self.barrier.release.notified().await;
                }
                let mut busy = self.inner.busy.lock().unwrap();
                Ok(if busy.is_empty() {
                    false
                } else {
                    busy.remove(0)
                })
            })
        }

        fn install<'a>(
            &'a self,
            app: &'a Arc<App>,
            host: &'a str,
            kind: &'a str,
            target: &'a str,
            fence: &'a HostFence,
        ) -> BoxFuture<'a, Result<String, InstallError>> {
            self.inner.install(app, host, kind, target, fence)
        }

        fn version<'a>(
            &'a self,
            app: &'a Arc<App>,
            host: &'a str,
            kind: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<String>> {
            self.inner.version(app, host, kind)
        }

        fn restart<'a>(
            &'a self,
            app: &'a Arc<App>,
            scope: Scope,
            fence: &'a HostFence,
        ) -> BoxFuture<'a, anyhow::Result<Value>> {
            self.inner.restart(app, scope, fence)
        }
    }

    #[tokio::test]
    async fn recovery_keeps_the_free_lock_fence_through_version_and_notice_publish() {
        let env = crate::testing::env().await;
        let host = "recover-fence-347";
        let cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        env.app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let orphan = orphan_row(&env.app, host, "0.157.0", "installing", Some("0.155.1")).await;
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (bot, run) = codex_bot_with_notice(&env, "cx-recover-fence-347", &pending).await;
        sqlx::query("UPDATE projects SET host=? WHERE id=(SELECT project_id FROM bots WHERE id=?)")
            .bind(host)
            .bind(&bot)
            .execute(&env.app.db)
            .await
            .unwrap();

        let barrier = Arc::new(ProbeBarrier { entered: tokio::sync::Notify::new(), release: tokio::sync::Notify::new() });
        let fake = Fake::new(&["codex-cli 0.157.0"], Ok("already installed on A"));
        let runner: Arc<dyn Runner> = Arc::new(ProbeGatedRunner { inner: fake.clone(), barrier: barrier.clone() });
        let (app, fake_host, fake_id, runner) = (env.app.clone(), host.to_string(), orphan.clone(), runner.clone());
        let task = tokio::spawn(async move {
            recover(&app, runner.as_ref(), &fake_id, &fake_host, "0.157.0", Some("0.155.1"), Duration::ZERO, Duration::from_secs(5)).await
        });
        tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
            .await
            .expect("recovery must reach the lock probe on A");
        env.app.hosts.replace_remote_for_test(&env.app, cfg("target-b")).await;
        barrier.release.notify_one();
        let v = task.await.unwrap();

        assert_eq!(v["ok"], false, "the free-lock result from A is stale after replacement: {v}");
        assert_eq!(v["reason"], "superseded", "the recovery fence changed before version verification: {v}");
        assert_eq!(*fake.probes.lock().unwrap(), 1);
        assert_eq!(*fake.version_reads.lock().unwrap(), 0, "a stale final lock result must not trigger a version read");
        assert_eq!(notice_of(&env.app, &run).await, Some(pending), "B's notice must remain untouched");
    }

    #[tokio::test]
    async fn a_stale_recovery_enumeration_cannot_spawn_after_its_worker_finishes() {
        let env = crate::testing::env().await;
        let orphan = orphan_row(&env.app, "local", "0.157.0", "installing", None).await;
        let mut rx = env.app.subscribe();
        let barrier = Arc::new(ProbeBarrier {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let fake = Fake::new(&["codex-cli 0.157.0"], Ok("already installed"));
        let runner: Arc<dyn Runner> = Arc::new(ProbeGatedRunner {
            inner: fake.clone(),
            barrier: barrier.clone(),
        });
        let first_row = unfinished_updates(&env.app)
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(spawn_recovery_worker(&env.app, &runner, first_row));
        tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
            .await
            .expect("the first worker must reach its controlled probe");

        // This is the exact result a sweep can hold after SELECT returns and before it dispatches its rows.
        let stale_row = unfinished_updates(&env.app)
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(stale_row.0, orphan);
        barrier.release.notify_one();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let terminal = row_status(&env.app, &orphan).await.0 != "running";
                let active = recovery_worker_active(&orphan);
                if terminal && !active {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the first worker must finish and release its active claim");

        let stale_dispatch_started_worker = spawn_recovery_worker(&env.app, &runner, stale_row);
        assert!(
            !stale_dispatch_started_worker,
            "a stale enumeration must not restart a worker after the row has been processed"
        );
        assert_eq!(
            *fake.probes.lock().unwrap(),
            1,
            "one update id must only be probed once"
        );
        assert_eq!(
            *fake.version_reads.lock().unwrap(),
            1,
            "one update id must only be reconciled once"
        );
        assert_eq!(
            done_events(&mut rx).len(),
            1,
            "a stale sweep must not emit a second completion"
        );
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
        seed_running(&env.app, "u-9", "local", "0.157.0").await;
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

    /// `echo $! > file` creates the file before writing the PID, so wait for parseable content, not just existence.
    async fn wait_for_pid_file(path: &std::path::Path) -> Option<u32> {
        // 30 秒上限（issue #952）：原本 2 秒在整樹平行跑、負載高時會先到期。
        for _ in 0..3000 {
            if let Some(pid) = std::fs::read_to_string(path).ok().and_then(|text| text.trim().parse().ok()) {
                return Some(pid);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }

    #[test]
    fn the_host_lock_path_keeps_home_expansion_in_the_remote_shell() {
        assert_eq!(shell_lock_path(CODEX_INSTALL_LOCK), "\"$HOME/.agents-manager-codex-install.lock\"");
        assert_eq!(shell_lock_path("/tmp/a'b"), "'/tmp/a'\\''b'");
    }

    /// The `/bin/sh` process here stands in for the remote command shell started by SSH.
    /// Killing that control process must not release the host lock while its fake installer lives.
    #[tokio::test]
    async fn remote_install_lock_survives_control_disconnect_until_installer_exits() {
        use tokio::io::AsyncWriteExt as _;

        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-cli-lock-disconnect-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("codex-install.lock").display().to_string();
        let installer_pid_file = dir.join("installer.pid");
        let second_ran = dir.join("second-ran");
        let release_file = dir.join("release-installer");
        let inner = format!(
            "(while [ ! -e '{}' ]; do sleep 0.05; done) & echo $! > '{}'; wait",
            release_file.display(),
            installer_pid_file.display()
        );

        let mut control = tokio::process::Command::new("/bin/sh")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut input = control.stdin.take().unwrap();
        input.write_all(locked_script(&lock, &inner).as_bytes()).await.unwrap();
        drop(input);
        let installer_pid = wait_for_pid_file(&installer_pid_file).await.expect("fake installer started");

        control.start_kill().unwrap(); // Unix `start_kill` uses SIGKILL; do not run the wrapper's EXIT trap.
        control.wait().await.unwrap();
        assert_eq!(unsafe { libc::kill(installer_pid as i32, 0) }, 0, "the fake installer outlives the SSH control shell");

        let probe = local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap();
        assert!(parse_probe(&probe).unwrap(), "a dropped SSH control process must not make the live installer lock stale: {probe}");
        let second = local_sh(&locked_script(&lock, &format!("touch '{}'", second_ran.display())), Duration::from_secs(5)).await;
        let err = second.expect_err("a second install stays excluded while the fake remote installer lives");
        assert!(err.contains(LOCKED_MARK) && err.contains("75"), "{err}");
        assert!(!second_ran.exists(), "the contender must not enter its install section");

        std::fs::write(&release_file, "release").unwrap();
        crate::testing::eventually!(unsafe { libc::kill(installer_pid as i32, 0) } != 0);
        assert_ne!(unsafe { libc::kill(installer_pid as i32, 0) }, 0, "fake installer should finish");
        crate::testing::eventually!(!parse_probe(&local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap()).unwrap());
        assert!(!parse_probe(&local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap()).unwrap(), "lock becomes recoverable after the installer exits");
        local_sh(&locked_script(&lock, &format!("touch '{}'", second_ran.display())), Duration::from_secs(5)).await.expect("a completed installer must release the lock");
        assert!(second_ran.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Killing the process that created the lock must not release it while an installer child survives.
    #[tokio::test]
    async fn remote_install_lock_survives_helper_sigkill_until_installer_exits() {
        use tokio::io::AsyncWriteExt as _;

        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-cli-lock-helper-kill-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("codex-install.lock").display().to_string();
        let installer_pid_file = dir.join("installer.pid");
        let second_ran = dir.join("second-ran");
        let release_file = dir.join("release-installer");
        let inner = format!(
            "(while [ ! -e '{}' ]; do sleep 0.05; done) & echo $! > '{}'; wait",
            release_file.display(),
            installer_pid_file.display()
        );

        let mut control = tokio::process::Command::new("/bin/sh")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = control.stdin.take().unwrap();
        input.write_all(locked_script(&lock, &inner).as_bytes()).await.unwrap();
        drop(input);
        let installer_pid = match wait_for_pid_file(&installer_pid_file).await {
            Some(pid) => pid,
            None => {
                let error = std::fs::read_to_string(&installer_pid_file).map(|text| format!("pid file content {text:?}")).unwrap_or_else(|error| error.to_string());
                let output = control.wait_with_output().await.unwrap();
                panic!("fake installer did not start: {error}; stdout={:?}; stderr={:?}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            }
        };
        let owner = std::fs::read_link(&lock).unwrap().to_string_lossy().into_owned();
        let helper_pid: i32 = owner.split(':').next().unwrap().parse().unwrap();
        assert_ne!(helper_pid, control.id().unwrap() as i32, "the lock helper is separate from the SSH control shell");

        assert_eq!(unsafe { libc::kill(installer_pid as i32, 0) }, 0, "fake installer is alive before killing the helper");
        assert_eq!(unsafe { libc::kill(helper_pid, libc::SIGKILL) }, 0, "kill the actual lock helper, not the outer control shell");
        control.wait().await.unwrap();
        let installer_survived = unsafe { libc::kill(installer_pid as i32, 0) } == 0;

        let probe = local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap();
        let busy = parse_probe(&probe).unwrap();
        let second = local_sh(&locked_script(&lock, &format!("touch '{}'", second_ran.display())), Duration::from_secs(5)).await;
        let second_was_excluded = second.as_ref().is_err_and(|err| err.contains(LOCKED_MARK) && err.contains("75"));
        let second_did_not_run = !second_ran.exists();

        std::fs::write(&release_file, "release").unwrap();
        crate::testing::eventually!(unsafe { libc::kill(installer_pid as i32, 0) } != 0);
        let installer_finished = unsafe { libc::kill(installer_pid as i32, 0) } != 0;
        let released = !parse_probe(&local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap()).unwrap();
        let reacquired = local_sh(&locked_script(&lock, &format!("touch '{}'", second_ran.display())), Duration::from_secs(5)).await.is_ok();
        let second_ran_after_release = second_ran.exists();
        std::fs::remove_dir_all(&dir).ok();

        assert!(installer_survived, "SIGKILL of the lock helper must leave the installer child alive");
        assert!(busy, "helper death must not make the live installer's lock stale: {probe}");
        assert!(second_was_excluded, "a contender must stay out while the installer survives: {second:?}");
        assert!(second_did_not_run, "the contender must not enter its install section");
        assert!(installer_finished, "the fake installer should eventually finish");
        assert!(released, "a dead install group must make its abandoned lock stale");
        assert!(reacquired && second_ran_after_release, "a new installer can acquire the lock after the old installer exits");
    }

    /// A stale PID can be reused by an unrelated process. PID liveness without the lock owner's identity is not enough.
    #[tokio::test]
    async fn a_reused_pid_does_not_keep_a_stale_host_install_lock_busy() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-cli-lock-pid-reuse-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("codex-install.lock").display().to_string();
        let marker = dir.join("installer-ran");

        let mut unrelated = tokio::process::Command::new("/bin/sh")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let unrelated_pid = unrelated.id().unwrap();
        let unrelated_stdin = unrelated.stdin.take().unwrap();
        std::os::unix::fs::symlink(unrelated_pid.to_string(), &lock).unwrap();
        let probe = local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap();
        assert!(!parse_probe(&probe).unwrap(), "an unrelated live `sh -s` reusing a legacy PID must not pin the lock: {probe}");
        local_sh(&locked_script(&lock, &format!("touch '{}'", marker.display())), Duration::from_secs(5)).await.expect("an unrelated process reusing the old PID must not pin the stale lock");
        assert!(marker.exists());
        assert!(std::fs::symlink_metadata(&lock).is_err());
        drop(unrelated_stdin);
        unrelated.wait().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_reused_process_group_id_does_not_keep_a_stale_host_install_lock_busy() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-cli-lock-pgid-reuse-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("codex-install.lock").display().to_string();
        let marker = dir.join("installer-ran");
        let pid_file = dir.join("unrelated-sleep.pid");

        let mut unrelated = tokio::process::Command::new("perl")
            .arg("-e")
            .arg("my $pid = fork(); die \"fork: $!\" unless defined $pid; if ($pid) { waitpid($pid, 0); my $status = $?; exit(($status & 127) ? 128 + ($status & 127) : $status >> 8); } setpgrp(0, 0) or die \"setpgrp: $!\"; exec @ARGV or die \"exec: $!\";")
            .arg("/bin/sh")
            .arg("-c")
            .arg(format!("echo $$ > '{}'; exec /bin/sleep 30", pid_file.display()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let unrelated_pid = match wait_for_pid_file(&pid_file).await {
            Some(pid) => pid as i32,
            None => {
                let error = std::fs::read_to_string(&pid_file).map(|text| format!("pid file content {text:?}")).unwrap_or_else(|error| error.to_string());
                let _ = unrelated.start_kill();
                let _ = unrelated.wait().await;
                panic!("unrelated process group did not start: {error}");
            }
        };
        let unrelated_pgid = unsafe { libc::getpgid(unrelated_pid) };
        if unrelated_pgid <= 0 {
            unsafe { libc::kill(unrelated_pid, libc::SIGKILL) };
            let _ = unrelated.wait().await;
            panic!("the unrelated process group must still be alive");
        }
        if unrelated_pgid == unsafe { libc::getpgrp() } {
            unsafe { libc::kill(unrelated_pid, libc::SIGKILL) };
            let _ = unrelated.wait().await;
            panic!("Perl setpgrp must isolate the unrelated process group");
        }
        std::os::unix::fs::symlink(format!("{unrelated_pgid}:old-owner-nonce"), &lock).unwrap();

        let probe = local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap();
        let busy = parse_probe(&probe).unwrap();
        let contender = local_sh(&locked_script(&lock, &format!("touch '{}'", marker.display())), Duration::from_secs(5)).await;
        let acquired = contender.is_ok();
        let installer_ran = marker.exists();

        unsafe { libc::kill(unrelated_pid, libc::SIGKILL) };
        unrelated.wait().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert!(!busy, "a live but unrelated process group must not match an abandoned lock's nonce: {probe}");
        assert!(acquired, "a stale group ID must be reclaimable: {contender:?}");
        assert!(installer_ran, "the replacement installer should run");
    }

    /// 真的 `/bin/sh` 跑鎖的腳本（裡面的指令換成 sleep／echo，絕不跑 curl）：拿著的時候第二個不跑、退 75；
    /// 探測看得到；跑完放掉；主人死掉留下的過期鎖會被拿走；裡面指令的失敗照樣傳出來。
    #[tokio::test]
    async fn the_host_lock_script_excludes_a_second_installer_and_recovers_a_stale_lock() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-cli-lock-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("codex-install.lock").display().to_string();
        let marker = dir.join("second-ran");
        let release_file = dir.join("release-installer");

        let inner = format!("while [ ! -e '{}' ]; do sleep 0.05; done", release_file.display());
        let first = tokio::process::Command::new("/bin/sh").arg("-c").arg(locked_script(&lock, &inner)).spawn().unwrap();
        assert!(crate::testing::eventually!(std::fs::symlink_metadata(&lock).is_ok()), "第一個要先拿到鎖");
        let probe = local_sh(&probe_script(&lock), Duration::from_secs(5)).await.unwrap();
        assert!(parse_probe(&probe).unwrap(), "拿著的時候探測是 busy: {probe}");

        let second = local_sh(&locked_script(&lock, &format!("touch '{}'", marker.display())), Duration::from_secs(5)).await;
        let err = second.expect_err("鎖被拿著，第二個不能跑");
        assert!(err.contains(LOCKED_MARK) && err.contains("75"), "{err}");
        assert!(matches!(classify_install_error(err), InstallError::Locked(_)));
        assert!(!marker.exists(), "第二個的指令一行都沒跑");

        std::fs::write(&release_file, "release").unwrap();
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

    /// #566：一批不相干的 claude 全域重啟正在跑（清單在 codex 裝好之前就定了）。codex 在 local 裝好之後，
    /// 結果不能只因為「有一批在跑」就報成重啟已經交出去、還指向那一批；要講 `deferred`，而且那批一結束，
    /// 這台的 codex 自己接著跑一批——不用使用者再按一次。
    #[tokio::test]
    async fn an_install_behind_an_unrelated_running_batch_reports_deferred_and_restarts_codex_after_it() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let cl = crate::testing::claude_bot(&app, &env.project_id, "cl").await;
        let cl_run = crate::testing::fake_run(&app, &cl.id).await;
        sqlx::query("UPDATE runs SET update_notice='Update installed · Restart to update' WHERE id=?").bind(&cl_run).execute(&app.db).await.unwrap();
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let (cx, _) = codex_bot_with_notice(&env, "cx", &pending).await;
        seed_running(&app, "u-566", "local", "0.157.0").await;
        // 這台不認得的身分：重啟在 start 那一步被擋，不真的開 pane。
        sqlx::query("UPDATE bots SET identity='nope' WHERE id IN (?, ?)").bind(&cl.id).bind(&cx).execute(&app.db).await.unwrap();
        let seen = Arc::new(Mutex::new(Vec::<crate::state::WsEvent>::new()));
        let (mut rx, out) = (app.subscribe(), seen.clone());
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                out.lock().unwrap().push(ev);
            }
        });
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        crate::app_ports_p12::race_point::arm("bulk_restart_before_lookup", &cl.id, move || async move {
            gate.await.ok();
        });
        let first = crate::runners::bulk_restart::spawn(&app).await.unwrap();
        assert_eq!(first["planned"].as_array().unwrap().len(), 1, "第一批只有 claude（codex 還是「需安裝」）：{first}");

        let fake = Arc::new(Fake { real_restart: true, ..Arc::try_unwrap(Fake::new(&["codex-cli 0.155.1", "codex-cli 0.157.0"], Ok("ok"))).ok().unwrap() });
        let v = super::run(&app, fake.as_ref(), "local", "u-566", "0.157.0").await;

        assert_eq!(v["ok"], true, "新版確實裝好了：{v}");
        assert_eq!(v["restart_status"], "deferred", "那一批沒涵蓋這台的 codex：{v}");
        assert_eq!(v["restart"]["behind_batch_id"], first["batch_id"], "{v}");
        assert_ne!(v["restart"]["batch_id"], first["batch_id"], "進度不能掛到不相干的那一批：{v}");
        release.send(()).unwrap();
        let codex_restarted_later = || {
            seen.lock().unwrap().iter().any(|e| e.kind == "bots_restart_progress" && e.data["bot_id"] == cx.as_str() && e.data["batch_id"] != first["batch_id"])
        };
        assert!(crate::testing::eventually!(codex_restarted_later()), "那批結束後這台的 codex 要自己接著重啟");
    }
}
