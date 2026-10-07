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
    pub program: String,
    args: Vec<String>,
    pub env: Vec<(&'static str, String)>,
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
    pub fn systemd(uid: u32, has_runtime_dir: bool, session: &str) -> Option<Self> {
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
    reap_in_background(cmd.spawn()?);
    Ok("spawn")
}

/// 丟掉 `Child` 不會 wait：行程結束後在 daemon 存活期間留 zombie（#287）。另起 thread 等它，結束就收掉。
/// （跟 `state::reap_in_background` 同一招；這裡留一份自己的，herdr_unit 才不必依賴 `state`。）
fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        if let Err(error) = child.wait() {
            tracing::warn!(?error, "failed waiting for spawned child");
        }
    });
}
