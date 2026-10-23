//! 不借現有 bot、直接在主機 shell pane 裝 claude／codex／grok（`POST /api/hosts/{name}/tools/install` 省略 `via_bot_id`，
//! issue #1931；SPEC §16.3a 同一套開 pane／收尾模式）。
//!
//! 全新使用者一顆 bot 都沒有，借 bot 裝的路走不通。這裡開臨時 host-shell pane、把官方安裝指令打進去（指令寫死，
//! 不接受呼叫端傳入），pane 的終端快照就是 UI 看進度與輸出的唯一通道（不進 daemon log／事件）。
//! 安裝指令結束（pane 前景只剩 shell 自己）後重新偵測那台的 CLI、推 `host_changed`、關 pane；
//! 開不起來、等太久同樣走收尾。登入不在這裡——裝好之後 UI 走一般的登入流程。

use crate::hosts::HostFence;
use crate::lifecycle::{LcError, LcResult};
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// 安裝最久等多久：下載加 `npm i -g` 在慢網路上也不會超過；再久就是卡在互動提問或 sudo。
const DEADLINE: Duration = Duration::from_secs(15 * 60);
/// 指令打進去之後多久內要看到前景程序離開 shell；沒看到就當它瞬間跑完（例如 `command not found`）。
const STARTUP: Duration = Duration::from_secs(12);
const POLL: Duration = Duration::from_secs(1);
/// 連續幾次 `pane.process_info` 都失敗就當 pane 已經不在了（使用者自己關掉），不等滿 [`DEADLINE`]。
const PANE_GONE_AFTER: u32 = 3;

/// 官方安裝指令（API §12.7）；與借 bot 安裝的 [`crate::tools::install_prompt`] 共用 `tools::install_shell_command`。
pub fn install_command(kind: &str) -> Option<&'static str> {
    crate::tools::install_shell_command(kind)
}

fn running() -> &'static Mutex<HashSet<(String, String)>> {
    static R: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// 同一 `(host, kind)` 同時只裝一個；離開時放掉（含 panic）。
pub struct Slot(String, String);

impl Slot {
    pub fn take(host: &str, kind: &str) -> Option<Self> {
        running()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((host.to_string(), kind.to_string()))
            .then(|| Slot(host.to_string(), kind.to_string()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        running().lock().unwrap_or_else(|e| e.into_inner()).remove(&(self.0.clone(), self.1.clone()));
    }
}

fn is_shell_name(argv: &[String], argv0: Option<&str>) -> bool {
    argv.iter().map(String::as_str).chain(argv0).take(1).any(|a| {
        let base = std::path::Path::new(a).file_name().and_then(|v| v.to_str()).unwrap_or(a).trim_start_matches('-');
        matches!(base, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh")
    })
}

/// pane 前景有沒有 shell 以外的程序在跑：`shell_pid` 對得上的不算；herdr 沒給 `shell_pid` 時退回「看起來像 shell」。
pub fn installer_busy(shell: &am_base::herdr::PaneShell) -> bool {
    let Some(list) = shell.foreground_processes.as_ref() else { return false };
    list.iter().any(|p| match (shell.shell_pid, p.pid) {
        (Some(sp), Some(pid)) => pid != sp,
        _ => !is_shell_name(&p.argv, p.argv0.as_deref()),
    })
}

/// 開 pane、打指令、起收尾 watcher。回 `{pane_id, mode:"shell"}`。
pub async fn start(app: &Arc<crate::state::App>, host: &str, kind: &str) -> LcResult<serde_json::Value> {
    let command = install_command(kind).ok_or_else(|| {
        if kind == "agy" {
            LcError::Bad("agy 不走 tools/install，請用 POST /api/hosts/{name}/agy/install".into())
        } else {
            LcError::Bad("kind must be claude, codex or grok".into())
        }
    })?;
    let fence = app.hosts.fence(host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    let slot = Slot::take(host, kind)
        .ok_or_else(|| LcError::conflict("tool_install_in_progress", json!({"host": host, "kind": kind})))?;
    let (a, h) = (app.clone(), host.to_string());
    let opened = app
        .hosts
        .run_if_current(&fence, async move {
            let shell = crate::api::shell::open(&a, &h, None).await?;
            if let Err(e) = crate::api::shell::send_text(&a, &h, &shell.pane_id, command, true).await {
                let _ = crate::api::shell::close(&a, &h, &shell.pane_id).await;
                return Err(e);
            }
            Ok::<_, LcError>(shell)
        })
        .await;
    let shell = opened.ok_or_else(|| LcError::Upstream(format!("host `{host}` changed before the install started")))??;
    spawn_watch(app.clone(), host.to_string(), kind.to_string(), shell.pane_id.clone(), fence, slot);
    Ok(json!({"pane_id": shell.pane_id, "mode": "shell"}))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    Busy,
    Idle,
    /// `pane.process_info` 失敗。
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// 安裝指令跑完（或瞬間就結束）。
    Finished,
    /// pane 查不到：使用者關了它，或連線壞了。
    PaneGone,
    Timeout,
}

/// watcher 的判斷狀態：一次觀察一次 [`Tracker::step`]，回 `Some` 才收尾。
#[derive(Debug, Default)]
pub struct Tracker {
    saw_busy: bool,
    unreadable: u32,
}

impl Tracker {
    pub fn step(&mut self, seen: Observed, elapsed: Duration) -> Option<End> {
        match seen {
            Observed::Unreadable => {
                self.unreadable += 1;
                if self.unreadable >= PANE_GONE_AFTER {
                    return Some(End::PaneGone);
                }
            }
            Observed::Busy => {
                self.unreadable = 0;
                self.saw_busy = true;
            }
            Observed::Idle => {
                self.unreadable = 0;
                if self.saw_busy || elapsed >= STARTUP {
                    return Some(End::Finished);
                }
            }
        }
        (elapsed >= DEADLINE).then_some(End::Timeout)
    }
}

/// 結束後的收尾：重新偵測、推 `host_changed`，再推 `tool_install_done`。
/// 裝好了才關 pane；沒裝成（或偵測失敗）pane 留著，讓使用者在終端看到錯誤（API §12.7）。`slot` 活到這裡才放，收尾期間重複請求仍 409。
fn spawn_watch(app: Arc<crate::state::App>, host: String, kind: String, pane_id: String, fence: HostFence, slot: Slot) {
    tokio::spawn(async move {
        let _slot = slot;
        let started = tokio::time::Instant::now();
        let mut tracker = Tracker::default();
        let end = loop {
            tokio::time::sleep(POLL).await;
            if !app.hosts.is_current(&fence).await {
                tracing::info!(%host, %pane_id, "host was reconnected/reconfigured; install watcher gives up on the old connection");
                return;
            }
            let seen = match fence.conn().client.pane_shell(&pane_id).await {
                Ok(s) if installer_busy(&s) => Observed::Busy,
                Ok(_) => Observed::Idle,
                Err(_) => Observed::Unreadable,
            };
            if let Some(end) = tracker.step(seen, started.elapsed()) {
                break end;
            }
        };
        let installed = match crate::tools::detect_with_fence(&app, &host, &fence).await {
            Ok(ht) => {
                crate::state::emit_host_changed(&app, &fence).await;
                ht.tools.get(&kind).is_some_and(|t| t.installed)
            }
            Err(e) => {
                tracing::warn!(%host, error = %format!("{e:#}"), "re-detect after tool install failed");
                false
            }
        };
        let pane_exists = end != End::PaneGone;
        let kept = pane_exists && !installed;
        if pane_exists && installed && app.hosts.is_current(&fence).await {
            let _ = crate::api::shell::close(&app, &host, &pane_id).await;
        }
        app.emit(
            "tool_install_done",
            json!({"host": host, "kind": kind, "installed": installed, "pane_id": pane_id, "pane_kept": kept, "timed_out": end == End::Timeout}),
        )
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use am_base::herdr::{PaneShell, ProcessInfo};

    fn proc(pid: i64, argv: &[&str]) -> ProcessInfo {
        ProcessInfo { argv: argv.iter().map(|s| s.to_string()).collect(), argv0: None, pid: Some(pid) }
    }

    #[test]
    fn only_the_three_official_installers_exist() {
        assert_eq!(install_command("claude"), Some("curl -fsSL https://claude.ai/install.sh | bash"));
        let codex = install_command("codex").unwrap();
        assert!(codex.starts_with("sh -c '") && codex.ends_with('\''), "分支包在 sh -c 裡，使用者的 shell 是 fish／zsh 也能跑：{codex}");
        assert!(codex.find("npm i -g @openai/codex") < codex.find("brew install codex"), "npm 優先，其次 brew");
        assert!(codex.contains("exit 1"), "兩者都沒有要失敗");
        assert_eq!(install_command("grok"), Some("curl -fsSL https://x.ai/cli/install.sh | bash"));
        assert_eq!(install_command("agy"), None);
        assert_eq!(install_command("rm -rf /"), None);
    }

    #[test]
    fn the_installer_is_busy_until_only_the_shell_is_left_in_the_foreground() {
        let idle = PaneShell { shell_pid: Some(10), foreground_processes: Some(vec![proc(10, &["-zsh"])]) };
        let none = PaneShell { shell_pid: Some(10), foreground_processes: Some(vec![]) };
        let busy = PaneShell {
            shell_pid: Some(10),
            foreground_processes: Some(vec![proc(11, &["curl", "-fsSL", "x"]), proc(12, &["bash"])]),
        };
        assert!(!installer_busy(&idle));
        assert!(!installer_busy(&none));
        assert!(!installer_busy(&PaneShell::default()));
        assert!(installer_busy(&busy));
        // 沒有 shell_pid 時，installer 自己的 `bash` 不能被當成 shell 以外的程序漏看：curl 在就算忙。
        let unknown = PaneShell { shell_pid: None, foreground_processes: Some(vec![proc(11, &["npm", "i"])]) };
        let unknown_idle = PaneShell { shell_pid: None, foreground_processes: Some(vec![proc(10, &["-bash"])]) };
        assert!(installer_busy(&unknown));
        assert!(!installer_busy(&unknown_idle));
    }

    #[test]
    fn one_install_per_host_and_kind_and_the_slot_frees_on_drop() {
        let a = Slot::take("slot-host-a", "claude").expect("first");
        assert!(Slot::take("slot-host-a", "claude").is_none(), "same (host, kind) is refused");
        assert!(Slot::take("slot-host-a", "codex").is_some(), "other kind is independent");
        assert!(Slot::take("slot-host-b", "claude").is_some(), "other host is independent");
        drop(a);
        assert!(Slot::take("slot-host-a", "claude").is_some(), "released after the install ends");
    }

    #[test]
    fn a_pane_that_stays_unreadable_ends_the_watch_instead_of_waiting_for_the_deadline() {
        let mut t = Tracker::default();
        let sec = Duration::from_secs;
        assert_eq!(t.step(Observed::Busy, sec(5)), None);
        assert_eq!(t.step(Observed::Unreadable, sec(6)), None);
        assert_eq!(t.step(Observed::Unreadable, sec(7)), None);
        assert_eq!(t.step(Observed::Busy, sec(8)), None, "暫時錯誤不累積：讀到一次就歸零");
        assert_eq!(t.step(Observed::Unreadable, sec(9)), None);
        assert_eq!(t.step(Observed::Unreadable, sec(10)), None);
        assert_eq!(t.step(Observed::Unreadable, sec(11)), Some(End::PaneGone));
    }

    #[test]
    fn the_watch_ends_when_the_installer_leaves_the_foreground_or_never_shows_up() {
        let sec = Duration::from_secs;
        let mut t = Tracker::default();
        assert_eq!(t.step(Observed::Idle, sec(2)), None, "還在等指令起來");
        assert_eq!(t.step(Observed::Idle, STARTUP), Some(End::Finished), "指令瞬間就結束了");
        let mut t = Tracker::default();
        assert_eq!(t.step(Observed::Busy, sec(1)), None);
        assert_eq!(t.step(Observed::Idle, sec(60)), Some(End::Finished));
        let mut t = Tracker::default();
        assert_eq!(t.step(Observed::Busy, DEADLINE), Some(End::Timeout));
    }
}
