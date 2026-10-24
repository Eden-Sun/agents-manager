//! 外部 cargo 的多台建置主機與本機執行器（#1956）。`remote_cargo` 的子模組：守門、租約、同步都在本體，
//! 這裡只放「本機執行器怎麼起 shell」與「多台時挑哪一台」。

use super::*;

/// 本機執行器帶過去的環境變數：跟 ssh 進遠端一樣是乾淨環境，pane 的 `AM_*`、shim 的 PATH、`TMPDIR` 不漏進 cargo 與測試。
pub(super) const LOCAL_ENV_KEEP: [&str; 9] = ["USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "CARGO_HOME", "RUSTUP_HOME"];
/// 同 sshd 給非互動 session 的 PATH；`~/.cargo/bin` 由各段腳本自己接在前面（同 `PROBE_SH`）。
pub(super) const LOCAL_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub(super) fn local_home() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    {
        if let Some(home) = TEST_LOCAL_HOME.with(|h| h.borrow().clone()) {
            return Ok(home);
        }
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("本機執行器要 $HOME（remote_root 相對於它）"))
}

/// 本機執行器的一條「連線」：cwd＝`$HOME`（相對的 `remote_root` 跟 ssh 進去時同一個意思），自成一個 process group——
/// 跟 sshd 一樣一條連線一組，守門的 `reap` 收得到這次的 cargo，又不會打到 helper 自己（守門會拒收自己那組）。
pub(super) fn local_shell(script: &str) -> anyhow::Result<Command> {
    use std::os::unix::process::CommandExt as _;
    let home = local_home()?;
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).current_dir(&home).env_clear();
    for key in LOCAL_ENV_KEEP {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("HOME", &home).env("PATH", LOCAL_PATH).process_group(0);
    Ok(cmd)
}

/// 本機執行器的負載上限（#1956）：設定的 `max_load` > 呼叫端的 `CHECK_MAX_LOAD` > 核心數 × 1.25（同 `scripts/wait-load.sh`）。
pub(super) fn local_max_load(remote: &BuildRemoteCfg, check_max_load: Option<&str>) -> f64 {
    if remote.max_load > 0.0 {
        return remote.max_load;
    }
    if let Some(v) = check_max_load.and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| *v > 0.0) {
        return v;
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) as f64 * 1.25
}

/// 試用守門（[`Admission::try_only`]）沒拿到：共用目錄或名額被佔著、或負載太高（#1956）。`waiters`＝那台正在排隊的人數。
#[derive(Debug)]
pub(super) struct Busy {
    pub(super) waiters: usize,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "build host busy ({} waiting)", self.waiters)
    }
}

impl std::error::Error for Busy {}

/// 一台展開後的設定能不能用（[`decide_offload`]）。本機執行器只在 Linux：守門腳本要 flock 與 /proc。
pub(super) fn check_target(h: &BuildRemoteCfg) -> Result<(), &'static str> {
    if h.is_local() {
        if !cfg!(target_os = "linux") {
            return Err("本機執行器（host = \"local\"）只支援 Linux：守門要 flock 與 /proc");
        }
        return validate_target("", "", h.remote_root.trim());
    }
    if h.host.trim().is_empty() || h.user.trim().is_empty() {
        return Err("host／user 是空的");
    }
    validate_target(h.host.trim(), h.user.trim(), h.remote_root.trim())?;
    if h.ssh_port == 0 {
        return Err("ssh_port 不能是 0");
    }
    Ok(())
}

/// `remote-cargo --list-hosts`：照設定順序印每台建置主機的名字（[`BuildRemoteCfg::target_label`]），一行一台；
/// 沒啟用就什麼都不印。`scripts/ops/remote-cargo-prewarm.sh` 用它逐台預熱（#1956）。
pub fn list_hosts_cli(config_path: &Path) -> i32 {
    match decide_offload(config_path, &["check".to_string()], Vec::<(String, String)>::new()) {
        Offload::Go(hosts) => {
            for h in hosts {
                println!("{}", h.target_label());
            }
            0
        }
        Offload::Local(None) => 0,
        Offload::Local(Some(why)) => {
            eprintln!("agents-manager: {why}");
            2
        }
    }
}

/// 挑一台建置主機並拿到它的租約（#1956）。只有一台時就是那台的正常守門（排隊規則跟以前一樣）。
///
/// 多台時：
/// 1. 照設定順序逐台**試用**（[`Admission::try_from_cfg`]：一秒都不等、本機另看負載）——拿到就是這台，試用的租約直接拿來用。
/// 2. 都忙就挑排隊人數最少的那台（同分照設定順序，[`queue_order`]），用正常守門排隊；那台這時連不上就換下一名。
///
/// 連不上的每一台都在這裡講原因、記健康檔；全部連不上回最後一個 [`Unreachable`]。設定錯之類的其他錯誤照舊直接失敗。
pub(super) fn acquire(hosts: &[BuildRemoteCfg], data_dir: &Path, root: &Path, ctl: &Deadline) -> anyhow::Result<(usize, Lease)> {
    let multi = hosts.len() > 1;
    let mut last_unreachable: Option<anyhow::Error> = None;
    let mut unreachable = |i: usize, e: anyhow::Error, more: bool| {
        let h = &hosts[i];
        let why = e.to_string();
        let then = if more { "改試下一台" } else { "這次 cargo 改在本機跑（要排本機名額）" };
        eprintln!("{}", unreachable_note_then(h, &auth_desc(h, data_dir), &why, then));
        note_health(h, data_dir, false, Some(why));
        last_unreachable = Some(e);
    };
    if !multi {
        return match open_lease(&hosts[0], data_dir, root, Admission::from_cfg(&hosts[0]), ctl) {
            Ok(lease) => Ok((0, lease)),
            Err(e) if e.downcast_ref::<Unreachable>().is_some() => {
                unreachable(0, e, false);
                Err(last_unreachable.take().expect("just recorded"))
            }
            Err(e) => Err(e),
        };
    }
    let mut busy = Vec::new();
    for (i, h) in hosts.iter().enumerate() {
        match open_lease(h, data_dir, root, Admission::try_from_cfg(h), ctl) {
            Ok(lease) => {
                eprintln!("agents-manager: 建置主機 {} 有空槽，這次在那裡編", h.target_label());
                return Ok((i, lease));
            }
            Err(e) => match e.downcast_ref::<Busy>() {
                Some(b) => {
                    note_health(h, data_dir, true, None);
                    busy.push((i, b.waiters));
                }
                None if e.downcast_ref::<Unreachable>().is_some() => unreachable(i, e, i + 1 < hosts.len()),
                None => return Err(e),
            },
        }
    }
    let order = queue_order(&busy);
    for (n, &i) in order.iter().enumerate() {
        let h = &hosts[i];
        let waiters = busy.iter().find(|(j, _)| *j == i).map(|(_, w)| *w).unwrap_or(0);
        eprintln!("agents-manager: 建置主機都滿了，排 {}（前面約 {waiters} 個在等）", h.target_label());
        match open_lease(h, data_dir, root, Admission::from_cfg(h), ctl) {
            Ok(lease) => return Ok((i, lease)),
            Err(e) if e.downcast_ref::<Unreachable>().is_some() => unreachable(i, e, n + 1 < order.len()),
            Err(e) => return Err(e),
        }
    }
    Err(last_unreachable.unwrap_or_else(|| anyhow::Error::new(Unreachable { why: "沒有連得上的建置主機".into() })))
}

/// 都忙時排哪台（#1956）：排隊人數少的先，同分照設定順序（`busy` 的 index）。
pub(super) fn queue_order(busy: &[(usize, usize)]) -> Vec<usize> {
    let mut v = busy.to_vec();
    v.sort_by_key(|&(i, waiters)| (waiters, i));
    v.into_iter().map(|(i, _)| i).collect()
}
