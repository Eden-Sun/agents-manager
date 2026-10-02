//! Linux：本機的 herdr server 交給 systemd user unit `herdr@<session>.service` 看管
//!（`scripts/ops/systemd/herdr@.service`，由 `agm ops-sync` 對照安裝；issue #677，SPEC §19）。
//!
//! 以前 daemon 在 socket 連不到時直接 spawn `herdr --session <s> server`。在 Linux 上那顆 server 與每個 pane
//! 的 agent 都落在 daemon 所在的 cgroup：daemon 若以一般的 systemd service 跑，停一次 daemon 就全部陪葬；
//! 另外裝一個會自動重啟的 unit 而 daemon 照樣自己 spawn，herdr 一掛兩邊會各起一顆搶同一個 socket。
//! 所以看管者只留 systemd 一個：daemon 要起 server 就 `systemctl --user start`，叫不到 user bus、
//! 沒裝這個 unit（`Unit … not found`）才退回直接 spawn。macOS 不走這裡（沒有 systemd）。

/// 起 herdr server 的那一條 `systemctl` 指令。
pub struct UnitStart {
    pub(crate) program: String,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
}

/// systemd 的實例名要原樣可用：`systemd-escape` 會改寫的字元（`/`、空白、非 ASCII…）一律不走 unit，
/// 退回直接 spawn，免得 unit 起的是另一個名字的 session。
fn instance_name_ok(session: &str) -> bool {
    !session.is_empty()
        && !session.starts_with('.')
        && session.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl UnitStart {
    /// 這台該不該、能不能交給 systemd。非 Linux 一律 `None`。
    pub fn for_this_host(session: &str) -> Option<Self> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        // SAFETY: getuid 沒有前置條件，也不會失敗。
        let uid = unsafe { libc::getuid() };
        Self::systemd(uid, std::env::var_os("XDG_RUNTIME_DIR").is_some(), session)
    }

    /// daemon 不是從登入 session 起的（沒有 pam_systemd）就沒有 `XDG_RUNTIME_DIR`，`systemctl --user`
    /// 連不到 user bus，補上 `/run/user/<uid>`（同 `deploy_now::SchedulerKick`）。
    /// 不帶 `--no-block`：要等 unit 真的進到 active（`Type=simple` 在 exec 之後就算），失敗才看得到錯誤。
    pub(crate) fn systemd(uid: u32, has_runtime_dir: bool, session: &str) -> Option<Self> {
        if !instance_name_ok(session) {
            return None;
        }
        let env = if has_runtime_dir { Vec::new() } else { vec![("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))] };
        Some(UnitStart {
            program: "systemctl".into(),
            args: vec!["--user".into(), "start".into(), format!("herdr@{session}.service")],
            env,
        })
    }

    pub fn run(&self) -> Result<(), String> {
        let out = std::process::Command::new(&self.program)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
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

/// 起 `session` 的 herdr server，回傳用的是哪條路（寫進 log／錯誤訊息）。`unit` 有值就先交給 systemd；
/// 它失敗（沒有 user bus、沒裝 unit）才直接 spawn `herdr`。unit 成功就**不再** spawn：socket 慢一點起來
/// 是 systemd 那顆還在開，再起一顆就是兩顆搶同一個 socket。
pub fn start_server(session: &str, log_dir: &std::path::Path, unit: Option<&UnitStart>, herdr: &str) -> anyhow::Result<&'static str> {
    if let Some(u) = unit {
        match u.run() {
            Ok(()) => {
                tracing::info!(session, "herdr server handed to systemd user unit herdr@{session}.service");
                return Ok("systemd");
            }
            Err(error) => tracing::warn!(session, %error, "systemctl --user start herdr@… failed; spawning herdr directly"),
        }
    }
    std::fs::create_dir_all(log_dir).ok();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("herdr-server.log"))?;
    let errlog = log.try_clone()?;
    let mut cmd = std::process::Command::new(herdr);
    cmd.args(["--session", session, "server"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(errlog));
    // Detach: the herdr server outlives the daemon and is not part of its terminal's
    // foreground process group.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    crate::state::reap_in_background(cmd.spawn()?);
    Ok("spawn")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    pub(crate) struct Tmp(pub PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    pub(crate) fn tmp() -> Tmp {
        let d = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-unit-{}", crate::db::ulid())));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    /// Linux：剛寫好的假執行檔，別的測試執行緒 fork 的瞬間可能還握著寫入的 fd，這時 exec 會回
    /// ETXTBSY（Text file busy）。那不是被測的行為，重試幾次。
    fn busy_retry<T, E: std::fmt::Display>(mut f: impl FnMut() -> Result<T, E>) -> Result<T, E> {
        for _ in 0..50 {
            match f() {
                Err(e) if e.to_string().contains("Text file busy") => std::thread::sleep(std::time::Duration::from_millis(20)),
                r => return r,
            }
        }
        f()
    }

    /// 假 `systemctl`：記下 argv 與它看到的 `XDG_RUNTIME_DIR`，照 `rc` 離開。
    pub(crate) fn fake_systemctl(dir: &Path, rc: i32, stderr: &str) -> (UnitStart, PathBuf) {
        let log = dir.join("systemctl.log");
        let bin = dir.join("systemctl");
        crate::testing::write_exec(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" \"XDG_RUNTIME_DIR=${{XDG_RUNTIME_DIR:-}}\" >> '{}'\nprintf '%s' '{stderr}' >&2\nexit {rc}\n",
                log.display()
            ),
        );
        let mut u = UnitStart::systemd(4242, false, "agents-manager").unwrap();
        u.program = bin.to_string_lossy().into_owned();
        (u, log)
    }

    #[test]
    fn starts_the_session_instance_and_supplies_the_runtime_dir() {
        let dir = tmp();
        let (u, log) = fake_systemctl(&dir.0, 0, "");
        busy_retry(|| u.run()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "--user start herdr@agents-manager.service\nXDG_RUNTIME_DIR=/run/user/4242\n"
        );
        assert!(UnitStart::systemd(4242, true, "agents-manager").unwrap().env.is_empty(), "已經有值就不蓋");
    }

    #[test]
    fn reports_why_the_unit_did_not_start() {
        let dir = tmp();
        let (u, _) = fake_systemctl(&dir.0, 5, "Unit herdr@agents-manager.service not found.");
        assert_eq!(busy_retry(|| u.run()).unwrap_err(), "Unit herdr@agents-manager.service not found.");
        let (u, _) = fake_systemctl(&dir.0, 1, "");
        assert!(busy_retry(|| u.run()).unwrap_err().ends_with("start herdr@agents-manager.service rc=Some(1)"));
    }

    #[test]
    fn session_names_systemd_would_rewrite_are_not_sent_to_a_unit() {
        for bad in ["", "a/b", "a b", ".hidden", "中文", "a\\x2d"] {
            assert!(UnitStart::systemd(1, true, bad).is_none(), "{bad:?}");
        }
        for ok in ["agents-manager", "am-attach-remote", "s_1.2"] {
            assert!(UnitStart::systemd(1, true, ok).is_some(), "{ok:?}");
        }
    }

    /// 假 `herdr`：記下 argv 就離開（不起任何 server）。
    fn fake_herdr(dir: &Path) -> (String, PathBuf) {
        let log = dir.join("herdr.log");
        let bin = dir.join("herdr");
        crate::testing::write_exec(&bin, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()));
        (bin.to_string_lossy().into_owned(), log)
    }

    /// 等背景 spawn 的假 herdr 寫完（它由 reap thread 收，不在這條執行緒上）。
    fn read_eventually(p: &Path) -> String {
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(p) {
                if !s.is_empty() {
                    return s;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        String::new()
    }

    #[test]
    fn a_started_unit_is_the_only_server() {
        let dir = tmp();
        let (u, sys_log) = fake_systemctl(&dir.0, 0, "");
        let (herdr, herdr_log) = fake_herdr(&dir.0);
        assert_eq!(busy_retry(|| start_server("agents-manager", &dir.0.join("logs"), Some(&u), &herdr)).unwrap(), "systemd");
        assert!(std::fs::read_to_string(&sys_log).unwrap().starts_with("--user start herdr@agents-manager.service\n"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!herdr_log.exists(), "unit 起了就不能再自己 spawn 一顆");
    }

    #[test]
    fn falls_back_to_spawning_when_the_unit_cannot_start() {
        let dir = tmp();
        let (u, sys_log) = fake_systemctl(&dir.0, 1, "Failed to connect to bus: No medium found");
        let (herdr, herdr_log) = fake_herdr(&dir.0);
        assert_eq!(busy_retry(|| start_server("agents-manager", &dir.0.join("logs"), Some(&u), &herdr)).unwrap(), "spawn");
        assert!(sys_log.exists(), "有 unit 就要先試");
        assert_eq!(read_eventually(&herdr_log), "--session agents-manager server\n");
        // 沒有 unit（macOS、名字不能當實例名）直接 spawn。
        std::fs::remove_file(&herdr_log).unwrap();
        assert_eq!(busy_retry(|| start_server("x", &dir.0.join("logs"), None, &herdr)).unwrap(), "spawn");
        assert_eq!(read_eventually(&herdr_log), "--session x server\n");
    }

    #[test]
    fn only_linux_hands_the_server_to_systemd() {
        let got = UnitStart::for_this_host("agents-manager").map(|u| u.program);
        assert_eq!(got.as_deref(), if cfg!(target_os = "linux") { Some("systemctl") } else { None });
    }
}
