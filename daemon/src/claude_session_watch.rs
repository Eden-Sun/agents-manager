//! claude 子 agent 的 session 來源（SPEC §12.5b）：各身分 config 目錄的 herdr claude hook 是**必要條件**。
//!
//! 沒有 hook 的 claude 子 agent（`herdr agent start` 開的）靠 herdr 的 `agent_session` 找對話檔，而 herdr 是靠 claude
//! 的 SessionStart hook（`<CLAUDE_CONFIG_DIR>/settings.json` 裡的 `herdr-agent-state.sh session`）綁 session 的。
//! hook 只裝在預設的 `~/.claude`、各身分自己的 config 目錄（`~/.claude-cc1` 之類）沒裝時，那個身分的子 agent 永遠沒有
//! session：`runs.native_session_id`／`transcript_path` 空、每個回合都是 `completed_fallback`。
//!
//! 每一輪（[`sweep`]）對每台連得上的主機、daemon 偵測到的每個 claude 身分（含預設 `~/.claude`；**不掃 `~/.claude-*`**）：
//! 1. 讀 `settings.json`，缺 SessionStart hook 就自動補裝（[`ensure_dir`]：先備份一次、`CLAUDE_CONFIG_DIR=<dir> herdr integration install claude`
//!    只加不改，裝完重讀驗證）；補裝失敗 log＋`ops_alert`（`claude_identity_hook_install_failed`），失敗後 1 小時內不重試。
//! 2. `running`、`adopted`、無 hook 的 claude run 超過 [`NO_SESSION_AFTER`] 仍沒 session：log＋`ops_alert`（`claude_child_no_session`），
//!    指名它的身分目錄是補裝了、補裝失敗或 hook 本來就在。
//!
//! 啟動前（[`preflight`]，`ShimInstallPort::claude_hook_preflight`）與 child 的 `herdr agent start`（shim 的 `am_ensure_claude_hook`）
//! 也各補裝一次；補不上就在啟動回應／stderr 明講「這顆不會有 session」。

use crate::state::App;
use futures::future::BoxFuture;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SWEEP: Duration = Duration::from_secs(300);
/// run 起來多久還沒有 session 才算「接不上」（herdr 綁 session 與 daemon 讀檔各要一兩輪）。
pub const NO_SESSION_AFTER: Duration = Duration::from_secs(180);
/// 補裝失敗後多久內不再自動重試（啟動前的 [`preflight`] 不受限）。
pub const RETRY_AFTER: Duration = Duration::from_secs(3600);
const EXEC_TIMEOUT: Duration = Duration::from_secs(40);
/// herdr 的 claude hook 腳本名；`settings.json` 裡 SessionStart 的 command 含它就算裝了。
pub const HOOK_MARK: &str = "herdr-agent-state";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookState {
    Present,
    /// settings.json 沒有那條 SessionStart hook（檔案不存在、或讀得懂但沒有）。
    Missing,
}

/// `settings.json` 全文（檔案不存在＝`None`）裡有沒有 herdr 的 SessionStart hook。讀不懂的 JSON 當成沒有。
pub fn settings_hook_state(settings: Option<&str>) -> HookState {
    let present = settings
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .and_then(|v| v.pointer("/hooks/SessionStart").cloned())
        .and_then(|v| v.as_array().cloned())
        .is_some_and(|entries| {
            entries.iter().any(|e| {
                e.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
                    hs.iter().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains(HOOK_MARK)))
                })
            })
        });
    if present { HookState::Present } else { HookState::Missing }
}

/// 一個 claude config 目錄，以及指到它的身分名（空字串＝預設；`cc0` 沒設 `CLAUDE_CONFIG_DIR` 時跟預設是同一個目錄）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDir {
    pub identities: Vec<String>,
    pub dir: String,
}

impl ConfigDir {
    pub fn label(&self) -> String {
        let names: Vec<&str> = self.identities.iter().map(|i| if i.is_empty() { "預設" } else { i.as_str() }).collect();
        format!("{}（{}）", names.join("／"), self.dir)
    }

    /// 手動補裝的等價指令（寫進建議；daemon 自己跑的是 [`Probe::install`]）。
    pub fn install_command(&self) -> String {
        format!("CLAUDE_CONFIG_DIR={} herdr integration install claude", crate::hosts::sh_quote(&self.dir))
    }
}

/// 會碰機器的三件事；測試換成假的。`Err` 是給人看的原因。
pub trait Probe: Send + Sync {
    fn config_dirs<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Vec<ConfigDir>>;
    /// 檔案不存在＝`Ok(None)`；目錄不存在＝`Err`（還沒登入過的身分不算缺 hook）。
    fn read_settings<'a>(&'a self, host: &'a str, dir: &'a str) -> BoxFuture<'a, Result<Option<String>, String>>;
    /// `CLAUDE_CONFIG_DIR=<dir> herdr integration install claude`；寫前備份一次 settings.json。
    fn install<'a>(&'a self, host: &'a str, dir: &'a str) -> BoxFuture<'a, Result<(), String>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 本來就有。
    Present,
    /// 這次補裝成功（裝完重讀驗證過）。
    Installed,
    Failed(String),
}

/// 確保一個目錄有 hook：缺就補裝並驗證。讀不到 settings.json（斷線、目錄不存在）回 `None`：不拿「讀不到」當「沒有」。
pub async fn ensure_dir(probe: &dyn Probe, host: &str, cd: &ConfigDir) -> Option<Outcome> {
    match probe.read_settings(host, &cd.dir).await {
        Ok(text) if settings_hook_state(text.as_deref()) == HookState::Present => return Some(Outcome::Present),
        Ok(_) => {}
        Err(e) => {
            tracing::debug!(host, dir = %cd.dir, error = %e, "could not read the claude settings.json to check the herdr hook");
            return None;
        }
    }
    if let Err(e) = probe.install(host, &cd.dir).await {
        return Some(Outcome::Failed(e));
    }
    Some(match probe.read_settings(host, &cd.dir).await {
        Ok(text) if settings_hook_state(text.as_deref()) == HookState::Present => Outcome::Installed,
        Ok(_) => Outcome::Failed("herdr integration install 回報成功，但 settings.json 仍沒有 SessionStart hook".to_string()),
        Err(e) => Outcome::Failed(format!("裝完重讀 settings.json 失敗：{e}")),
    })
}

/// 補裝失敗的記憶（主機＋目錄 → 時間與原因），讓背景巡檢不每輪都重試同一個壞掉的目錄。
#[derive(Default)]
pub struct Attempts(Mutex<HashMap<(String, String), (Instant, String)>>);

impl Attempts {
    fn recent_failure(&self, host: &str, dir: &str) -> Option<String> {
        let g = self.0.lock().unwrap();
        g.get(&(host.to_string(), dir.to_string())).filter(|(at, _)| at.elapsed() < RETRY_AFTER).map(|(_, why)| why.clone())
    }

    fn note(&self, host: &str, dir: &str, outcome: &Outcome) {
        let mut g = self.0.lock().unwrap();
        match outcome {
            Outcome::Failed(why) => {
                g.insert((host.to_string(), dir.to_string()), (Instant::now(), why.clone()));
            }
            _ => {
                g.remove(&(host.to_string(), dir.to_string()));
            }
        }
    }
}

/// 一個目錄這一輪的結果（只列有事的：補裝成功或失敗）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handled {
    pub host: String,
    pub dir: ConfigDir,
    pub outcome: Outcome,
}

/// 這台主機所有身分目錄：缺的補裝。回傳補裝成功／失敗的（本來就有的不列）。
pub async fn ensure_host(probe: &dyn Probe, attempts: &Attempts, host: &str) -> Vec<Handled> {
    let mut out = Vec::new();
    for cd in probe.config_dirs(host).await {
        let outcome = match attempts.recent_failure(host, &cd.dir) {
            Some(why) => Outcome::Failed(why),
            None => match ensure_dir(probe, host, &cd).await {
                Some(o) => o,
                None => continue,
            },
        };
        attempts.note(host, &cd.dir, &outcome);
        if outcome != Outcome::Present {
            out.push(Handled { host: host.to_string(), dir: cd, outcome });
        }
    }
    out
}

/// 在跑、無 hook、超過門檻還沒有 session 的 claude run。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StuckRun {
    pub run_id: String,
    pub bot_id: String,
    pub bot_name: String,
    pub identity: String,
    pub host: String,
}

pub async fn stuck_runs(pool: &SqlitePool, started_before: &str) -> sqlx::Result<Vec<StuckRun>> {
    sqlx::query_as(
        "SELECT r.id AS run_id, r.bot_id AS bot_id, b.name AS bot_name, COALESCE(NULLIF(r.runtime_identity, ''), b.identity, '') AS identity, p.host AS host \
         FROM runs r JOIN bots b ON b.id = r.bot_id JOIN projects p ON p.id = b.project_id \
         WHERE r.state = 'running' AND b.kind = 'claude' AND r.adopted = 1 AND b.inject_hooks = 0 AND b.deleted_at IS NULL \
           AND (r.native_session_id IS NULL OR r.native_session_id = '') AND r.started_at < ?",
    )
    .bind(started_before)
    .fetch_all(pool)
    .await
}

async fn alert(pool: &SqlitePool, key: &str, bot_id: Option<&str>, payload: Value) {
    if let Err(e) = crate::supervisor::store::push_inbox(pool, key, "ops_alert", None, bot_id, None, &payload).await {
        tracing::warn!(key, error = %e, "could not queue the claude session ops_alert");
    }
}

/// 補裝的結果：成功 log；失敗 log＋ops_alert（同主機同目錄只推一次）。
pub async fn report_handled(pool: &SqlitePool, handled: &[Handled]) {
    for h in handled {
        match &h.outcome {
            Outcome::Installed => {
                tracing::info!(host = %h.host, dir = %h.dir.dir, "installed the herdr claude hook into this identity's config dir (settings.json backed up once as settings.json.am-bak-herdr)");
            }
            Outcome::Failed(why) => {
                tracing::warn!(host = %h.host, dir = %h.dir.dir, error = %why, "could not install the herdr claude hook: child agents on this identity get no session and fall back to screen scraping");
                let key = format!("ops_alert:daemon:claude_identity_hook_install_failed:{}:{}", h.host, h.dir.dir);
                let payload = json!({
                    "source": "daemon",
                    "reason": "claude_identity_hook_install_failed",
                    "subject": format!("{}:{}", h.host, h.dir.dir),
                    "detail": format!("{} 的 claude config 目錄 {} 缺 herdr 的 SessionStart hook（{HOOK_MARK}），daemon 自動補裝失敗：{why}。用這個身分開的 claude 子 agent 綁不到 session、回合只能靠畫面快照（completed_fallback）。", h.host, h.dir.label()),
                    "action": format!("在 {} 上手動跑 `{}`，確認 settings.json 的 hooks.SessionStart 有 {HOOK_MARK}。daemon 約 1 小時後會再試一次。", h.host, h.dir.install_command()),
                });
                alert(pool, &key, None, payload).await;
            }
            Outcome::Present => {}
        }
    }
}

/// 無 session 的 run：log＋ops_alert（每個 run 只推一次），原因指名身分目錄。
pub async fn report_stuck(pool: &SqlitePool, stuck: &[StuckRun], handled: &[Handled]) {
    for s in stuck {
        let hit = handled.iter().find(|h| h.host == s.host && h.dir.identities.contains(&s.identity));
        let cause = match hit.map(|h| (&h.outcome, &h.dir)) {
            Some((Outcome::Failed(why), cd)) => format!("原因：身分目錄 {} 缺 herdr 的 SessionStart hook（{HOOK_MARK}），自動補裝失敗：{why}。手動補裝：`{}`", cd.label(), cd.install_command()),
            Some((_, cd)) => format!("身分目錄 {} 缺 hook，daemon 剛補裝好；這顆要重開才會綁上 session", cd.label()),
            None => "身分目錄的 herdr hook 在；查 `herdr agent get` 的 agent_session、`herdr integration status` 是不是過期".to_string(),
        };
        tracing::warn!(run = %s.run_id, bot = %s.bot_name, host = %s.host, identity = %s.identity, "claude child has been running without a session: transcript is not readable, turns will complete as completed_fallback");
        let key = format!("ops_alert:daemon:claude_child_no_session:{}", s.run_id);
        let payload = json!({
            "source": "daemon",
            "reason": "claude_child_no_session",
            "subject": s.bot_name,
            "bot_id": s.bot_id,
            "bot_name": s.bot_name,
            "run_id": s.run_id,
            "detail": format!("claude 子 agent `{}`（{}，身分 {}）已經在跑超過 {} 秒，還是沒有 session 登記：讀不到對話檔，每個回合都只能靠畫面快照。{cause}", s.bot_name, s.host, if s.identity.is_empty() { "預設" } else { &s.identity }, NO_SESSION_AFTER.as_secs()),
            "action": "hook 到位後這顆 agent 要重開（/exit 後 `herdr agent start … -- --resume <session>`）才會綁上 session。",
        });
        alert(pool, &key, Some(&s.bot_id), payload).await;
    }
}

/// 一輪：`hosts` 是 (主機名, 現在連得上)。
pub async fn sweep(pool: &SqlitePool, probe: &dyn Probe, attempts: &Attempts, hosts: &[(String, bool)], started_before: &str) {
    let mut handled = Vec::new();
    for (host, connected) in hosts {
        if *connected {
            handled.extend(ensure_host(probe, attempts, host).await);
        }
    }
    report_handled(pool, &handled).await;
    match stuck_runs(pool, started_before).await {
        Ok(stuck) => report_stuck(pool, &stuck, &handled).await,
        Err(e) => tracing::warn!(error = %e, "could not list claude runs without a session"),
    }
}

/// 啟動前：這顆 claude bot 的身分目錄補裝 hook。補不上回一句給人看的警告（呼叫端寫進啟動回應，仍照常啟動）。
pub async fn preflight(probe: &dyn Probe, host: &str, identity: Option<&str>) -> Option<String> {
    let want = identity.map(str::trim).unwrap_or("");
    let cd = probe.config_dirs(host).await.into_iter().find(|c| c.identities.iter().any(|i| i == want))?;
    match ensure_dir(probe, host, &cd).await? {
        Outcome::Failed(why) => Some(format!(
            "身分目錄 {} 缺 herdr 的 SessionStart hook（{HOOK_MARK}），啟動前自動補裝失敗：{why}。這顆 claude 不會有 session（讀不到對話檔、回合只能靠畫面快照）。手動補裝：`{}`",
            cd.label(),
            cd.install_command()
        )),
        _ => None,
    }
}

pub struct Real<'a>(pub &'a App);

impl Real<'_> {
    async fn exec(&self, host: &str, script: &str) -> Result<String, String> {
        if host == crate::config::LOCAL_HOST {
            let o = crate::hosts::sh_local(script, EXEC_TIMEOUT).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| "逾時".to_string())?;
            Ok(format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
        } else {
            let fence = self.0.hosts.fence(host).await.ok_or_else(|| format!("找不到主機 {host}"))?;
            fence.conn().ssh_exec_path_timeout(script, EXEC_TIMEOUT).await.map_err(|e| format!("{e:#}"))
        }
    }
}

impl Probe for Real<'_> {
    fn config_dirs<'a>(&'a self, host: &'a str) -> BoxFuture<'a, Vec<ConfigDir>> {
        Box::pin(async move {
            // 身分清單以 daemon 偵測到的為準（shell alias／設定檔），再加預設；不掃 `~/.claude-*`。
            let mut names: Vec<String> = vec![String::new()];
            names.extend(crate::tools::identities_for_host_ref(self.0, host).await.into_iter().filter(|i| i.kind == "claude").map(|i| i.name));
            let mut out: Vec<ConfigDir> = Vec::new();
            for name in names {
                let ident = Some(name.as_str()).filter(|n| !n.is_empty());
                let Ok(dir) = crate::lifecycle::identity_config_dir(self.0, host, ident).await else { continue };
                match out.iter_mut().find(|c| c.dir == dir) {
                    Some(c) => c.identities.push(name),
                    None => out.push(ConfigDir { identities: vec![name], dir }),
                }
            }
            out
        })
    }

    fn read_settings<'a>(&'a self, host: &'a str, dir: &'a str) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let d = crate::hosts::sh_quote(dir.trim_end_matches('/'));
            let script = format!("if [ ! -d {d} ]; then echo AM_NODIR; elif [ -f {d}/settings.json ]; then echo AM_FOUND; cat {d}/settings.json; else echo AM_NONE; fi");
            let out = self.exec(host, &script).await?;
            match out.split_once('\n') {
                Some(("AM_FOUND", body)) => Ok(Some(body.to_string())),
                _ if out.trim() == "AM_NONE" => Ok(None),
                _ if out.trim() == "AM_NODIR" => Err("目錄不存在".to_string()),
                _ => Err(format!("讀 settings.json 的輸出不認得：{}", out.chars().take(80).collect::<String>())),
            }
        })
    }

    fn install<'a>(&'a self, host: &'a str, dir: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let d = crate::hosts::sh_quote(dir.trim_end_matches('/'));
            // 只加不改：herdr 把 SessionStart 併進既有 settings.json；仍先備份一次（已有備份不覆蓋）。
            let inner = format!(
                "d={d}; [ -d \"$d\" ] || {{ echo 'config 目錄不存在'; exit 3; }}\n\
                 [ ! -f \"$d/settings.json\" ] || [ -e \"$d/settings.json.am-bak-herdr\" ] || cp -p \"$d/settings.json\" \"$d/settings.json.am-bak-herdr\" || {{ echo '備份 settings.json 失敗，未安裝'; exit 4; }}\n\
                 CLAUDE_CONFIG_DIR=\"$d\" \"$p\" integration install claude 2>&1"
            );
            let script = format!("{}\n[ -n \"$p\" ] || {{ echo 'AM_NO_HERDR'; exit 0; }}\n{inner}\n", crate::tools::login_abs_sh("herdr"));
            let out = self.exec(host, &script).await?;
            if out.lines().any(|l| l.trim() == "AM_NO_HERDR") {
                return Err("這台找不到 herdr".to_string());
            }
            // 成功與否以重讀 settings.json 為準（[`ensure_dir`]）；這裡只擋明確的失敗輸出。
            if out.contains("備份 settings.json 失敗") || out.contains("config 目錄不存在") {
                return Err(out.trim().chars().take(300).collect());
            }
            Ok(())
        })
    }
}

fn attempts() -> &'static Attempts {
    static A: std::sync::OnceLock<Attempts> = std::sync::OnceLock::new();
    A.get_or_init(Attempts::default)
}

pub fn spawn(app: Arc<App>) {
    crate::background_loop::spawn_periodic(&app, "claude session watcher", SWEEP, Duration::from_secs(90), |app| async move {
        let hosts: Vec<(String, bool)> = app.hosts.list().await.iter().map(|c| (c.name.clone(), c.is_local() || c.is_connected())).collect();
        let cutoff = (chrono::Utc::now() - chrono::Duration::from_std(NO_SESSION_AFTER).unwrap_or_default())
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sweep(&app.db, &Real(&app), attempts(), &hosts, &cutoff).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_HOOK: &str = r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"bash '/Users/m4p/.claude/hooks/herdr-agent-state.sh' session"}]}],"PreToolUse":[]}}"#;
    const RTK_ONLY: &str = r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"rtk hook claude"}]}]}}"#;

    #[test]
    fn only_a_session_start_hook_naming_the_herdr_script_counts() {
        assert_eq!(settings_hook_state(Some(WITH_HOOK)), HookState::Present);
        assert_eq!(settings_hook_state(Some(RTK_ONLY)), HookState::Missing, "m4p 的 .claude-cc1 就是這個形狀");
        assert_eq!(settings_hook_state(None), HookState::Missing);
        assert_eq!(settings_hook_state(Some("not json")), HookState::Missing);
        let wrong_event = WITH_HOOK.replace("SessionStart", "Stop");
        assert_eq!(settings_hook_state(Some(&wrong_event)), HookState::Missing, "掛在別的事件不算");
    }

    /// 假機器：每個目錄一份 settings.json；`install` 成功時把 hook 寫進去（`installs_ok=false` 則失敗、`lies=true` 則回成功卻不寫）。
    struct Fake {
        dirs: Vec<ConfigDir>,
        files: Mutex<HashMap<String, Result<Option<String>, String>>>,
        install_ok: bool,
        lies: bool,
        installed: Mutex<Vec<String>>,
    }

    impl Probe for Fake {
        fn config_dirs<'a>(&'a self, _host: &'a str) -> BoxFuture<'a, Vec<ConfigDir>> {
            Box::pin(async move { self.dirs.clone() })
        }
        fn read_settings<'a>(&'a self, _host: &'a str, dir: &'a str) -> BoxFuture<'a, Result<Option<String>, String>> {
            Box::pin(async move { self.files.lock().unwrap().get(dir).cloned().unwrap_or(Ok(None)) })
        }
        fn install<'a>(&'a self, _host: &'a str, dir: &'a str) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.installed.lock().unwrap().push(dir.to_string());
                if !self.install_ok {
                    return Err("herdr 回非 0".to_string());
                }
                if !self.lies {
                    self.files.lock().unwrap().insert(dir.to_string(), Ok(Some(WITH_HOOK.to_string())));
                }
                Ok(())
            })
        }
    }

    fn cd(ids: &[&str], d: &str) -> ConfigDir {
        ConfigDir { identities: ids.iter().map(|s| s.to_string()).collect(), dir: d.into() }
    }

    /// 現在 m4p 的樣子：cc0＝預設 `~/.claude`（有 hook）、cc1＝`~/.claude-cc1`（只有 rtk）。另一個目錄讀不到（ssh 斷）。
    fn m4p_like(install_ok: bool, lies: bool) -> Fake {
        Fake {
            dirs: vec![cd(&["", "cc0"], "/Users/m4p/.claude"), cd(&["cc1"], "/Users/m4p/.claude-cc1"), cd(&["cc3"], "/Users/m4p/.claude-cc3")],
            files: Mutex::new(HashMap::from([
                ("/Users/m4p/.claude".to_string(), Ok(Some(WITH_HOOK.to_string()))),
                ("/Users/m4p/.claude-cc1".to_string(), Ok(Some(RTK_ONLY.to_string()))),
                ("/Users/m4p/.claude-cc3".to_string(), Err("目錄不存在".to_string())),
            ])),
            install_ok,
            lies,
            installed: Mutex::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn a_missing_hook_is_installed_and_verified_while_present_and_unreadable_dirs_are_left_alone() {
        let fake = m4p_like(true, false);
        let handled = ensure_host(&fake, &Attempts::default(), "m4p").await;
        assert_eq!(handled.len(), 1, "{handled:?}");
        assert_eq!((handled[0].dir.dir.as_str(), &handled[0].outcome), ("/Users/m4p/.claude-cc1", &Outcome::Installed));
        assert_eq!(*fake.installed.lock().unwrap(), vec!["/Users/m4p/.claude-cc1".to_string()], "只動缺的那個，不碰已有 hook 的與不存在的目錄");
        assert!(ensure_host(&fake, &Attempts::default(), "m4p").await.is_empty(), "冪等：裝好後第二輪什麼都不做");
        assert_eq!(fake.installed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_install_that_claims_success_but_leaves_no_hook_is_a_failure_and_is_not_retried_within_the_backoff() {
        let fake = m4p_like(true, true);
        let attempts = Attempts::default();
        let handled = ensure_host(&fake, &attempts, "m4p").await;
        assert!(matches!(&handled[0].outcome, Outcome::Failed(w) if w.contains("仍沒有")), "{handled:?}");
        let again = ensure_host(&fake, &attempts, "m4p").await;
        assert!(matches!(again[0].outcome, Outcome::Failed(_)), "仍回報失敗");
        assert_eq!(fake.installed.lock().unwrap().len(), 1, "退避期內不重跑安裝");
    }

    async fn pool_with_runs() -> (std::path::PathBuf, SqlitePool) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-claude-session-watch-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::app_ports_p1::open(&dir.join("db.sqlite3")).await.unwrap();
        sqlx::query("INSERT INTO projects(id,path,label,host,created_at) VALUES('p1','/x','x','m4p','t')").execute(&pool).await.unwrap();
        // (bot, identity, inject_hooks, session, adopted)
        for (id, ident, inject, session, adopted) in [
            ("b-cc1", "cc1", 0, "", 1),
            ("b-cc0", "cc0", 0, "sess-1", 1),
            ("b-hooked", "cc1", 1, "", 1),
            ("b-manual", "cc1", 0, "", 0),
        ] {
            sqlx::query("INSERT INTO bots(id,project_id,name,kind,identity,inject_hooks,effort,fast,hook_token,created_at) VALUES(?,'p1',?,'claude',?,?,'high',0,'t','t')")
                .bind(id).bind(id).bind(ident).bind(inject).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO runs(id,bot_id,state,adopted,native_session_id,started_at) VALUES(?,?,'running',?,?,'2026-10-30T06:00:00.000Z')")
                .bind(format!("r-{id}")).bind(id).bind(adopted).bind(session).execute(&pool).await.unwrap();
        }
        (dir, pool)
    }

    async fn alerts(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE kind = 'ops_alert' ORDER BY event_key").fetch_all(pool).await.unwrap()
    }

    #[tokio::test]
    async fn a_failed_install_and_the_stuck_child_behind_it_each_raise_one_ops_alert_naming_the_dir() {
        let (dir, pool) = pool_with_runs().await;
        let hosts = vec![("m4p".to_string(), true)];
        let (fake, attempts) = (m4p_like(false, false), Attempts::default());
        sweep(&pool, &fake, &attempts, &hosts, "2026-10-30T06:05:00.000Z").await;
        sweep(&pool, &fake, &attempts, &hosts, "2026-10-30T06:05:00.000Z").await;
        let a = alerts(&pool).await;
        assert_eq!(a.len(), 2, "目錄一則、run 一則；第二輪不重推：{a:?}");
        let dir_alert = a.iter().find(|p| p.contains("claude_identity_hook_install_failed")).expect("install alert");
        assert!(dir_alert.contains("/Users/m4p/.claude-cc1") && dir_alert.contains("herdr 回非 0"), "{dir_alert}");
        let run_alert = a.iter().find(|p| p.contains("claude_child_no_session")).expect("run alert");
        assert!(run_alert.contains("b-cc1") && run_alert.contains("自動補裝失敗"), "{run_alert}");
        assert!(!a.iter().any(|p| p.contains("b-cc0") || p.contains("b-hooked") || p.contains("b-manual")), "有 session／有 hook／不是領養的都不報：{a:?}");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_successful_install_raises_no_install_alert_but_still_tells_the_stuck_child_to_restart() {
        let (dir, pool) = pool_with_runs().await;
        sweep(&pool, &m4p_like(true, false), &Attempts::default(), &[("m4p".to_string(), true)], "2026-10-30T06:05:00.000Z").await;
        let a = alerts(&pool).await;
        assert!(!a.iter().any(|p| p.contains("hook_install_failed")), "{a:?}");
        let run_alert = a.iter().find(|p| p.contains("claude_child_no_session")).expect("run alert");
        assert!(run_alert.contains("剛補裝好") && run_alert.contains("重開"), "{run_alert}");
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_run_younger_than_the_threshold_is_not_flagged() {
        let (dir, pool) = pool_with_runs().await;
        sweep(&pool, &m4p_like(true, false), &Attempts::default(), &[("m4p".to_string(), true)], "2026-10-30T05:59:00.000Z").await;
        assert!(!alerts(&pool).await.iter().any(|p| p.contains("claude_child_no_session")));
        pool.close().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn preflight_is_silent_when_the_identity_is_fine_or_gets_fixed_and_warns_when_it_cannot() {
        assert_eq!(preflight(&m4p_like(true, false), "m4p", Some("cc0")).await, None, "cc0 與預設是同一個目錄，本來就有");
        assert_eq!(preflight(&m4p_like(true, false), "m4p", Some("cc1")).await, None, "缺但補上了");
        let warn = preflight(&m4p_like(false, false), "m4p", Some("cc1")).await.expect("補不上要警告");
        assert!(warn.contains("/Users/m4p/.claude-cc1") && warn.contains("不會有 session"), "{warn}");
        assert_eq!(preflight(&m4p_like(false, false), "m4p", Some("nope")).await, None, "不認得的身分不在這裡報");
    }
}
