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

impl Admission {
    /// 多台挑主機的第一輪試用：同一台、同一組名額，只是一秒都不等；本機另看負載（[`local_max_load`]）。
    pub(super) fn try_from_cfg(remote: &BuildRemoteCfg) -> Self {
        Admission {
            shared_wait_secs: 0,
            queue_wait_secs: 0,
            try_only: true,
            max_load: remote.is_local().then(|| local_max_load(remote, std::env::var("CHECK_MAX_LOAD").ok().as_deref())),
            ..Self::from_cfg(remote)
        }
    }
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

/// 把設定錯的那幾台挑掉（[`check_target`]）：回 (能用的, 每台略過的原因)。一台設錯不拖累整組——
/// 例如額外主機少寫 user、Mac 上設了 `host = "local"`；全部都錯時由呼叫端退回本機。
pub(super) fn usable_targets(hosts: Vec<BuildRemoteCfg>) -> (Vec<BuildRemoteCfg>, Vec<String>) {
    let mut skipped = Vec::new();
    let ok = hosts
        .into_iter()
        .filter(|h| match check_target(h) {
            Ok(()) => true,
            Err(why) => {
                skipped.push(format!("設定檔 [build.remote] 的 {} 不合法（{why}）", h.host));
                false
            }
        })
        .collect();
    (ok, skipped)
}

/// `remote-cargo --host <名字>`：只留這一台、不分流（預熱逐台跑）；設定裡沒有這台回 `None`。
pub(super) fn only_host(hosts: Vec<BuildRemoteCfg>, only: Option<&str>) -> Option<Vec<BuildRemoteCfg>> {
    match only {
        None => Some(hosts),
        Some(name) => hosts.into_iter().find(|h| h.target_label() == name).map(|h| vec![h]),
    }
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
            Err(e) => Err(queue_full_noted(&hosts[0], data_dir, e)),
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
            Err(e) => return Err(queue_full_noted(h, data_dir, e)),
        }
    }
    Err(last_unreachable.unwrap_or_else(|| anyhow::Error::new(Unreachable { why: "沒有連得上的建置主機".into() })))
}

/// 排名額排到逾時（[`QueueFull`]）＝守門有回話，那台連得上、只是滿了：記在**排的那台**名下（不是第一台，#1956 review）。
fn queue_full_noted(h: &BuildRemoteCfg, data_dir: &Path, mut e: anyhow::Error) -> anyhow::Error {
    if let Some(qf) = e.downcast_mut::<QueueFull>() {
        if qf.host.is_empty() {
            qf.host = h.host.clone();
        }
        note_health(h, data_dir, true, None);
    }
    e
}

/// 都忙時排哪台（#1956）：排隊人數少的先，同分照設定順序（`busy` 的 index）。
pub(super) fn queue_order(busy: &[(usize, usize)]) -> Vec<usize> {
    let mut v = busy.to_vec();
    v.sort_by_key(|&(i, waiters)| (waiters, i));
    v.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::super::guard_tests::{base, fake_remote, guard_cmd, guard_with, with_fake_transport, with_sealed_waits, write_exec, HASH, QUICK};
    use super::*;
    use std::time::{Duration, Instant};

    /// 本機執行器的一台：`$HOME`（假 cargo 在 `.cargo/bin`）在沙盒，`remote_root` 是沙盒裡的絕對路徑。回傳 (設定, cwd, data_dir)。
    fn local_host(base: &Path, root: &str, cargo_body: &str) -> (BuildRemoteCfg, PathBuf, PathBuf) {
        let home = base.join("home");
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        write_exec(&home.join(".cargo/bin/cargo"), cargo_body);
        let cwd = base.join("work");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\n").unwrap();
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let remote = BuildRemoteCfg {
            enabled: true,
            host: crate::config::LOCAL_BUILD_HOST.into(),
            remote_root: base.join(root).display().to_string(),
            cargo_jobs: 1,
            test_threads: 0,
            max_concurrent: 1,
            timeout_secs: 60,
            ..Default::default()
        };
        (remote, cwd, data)
    }

    fn with_local_home<T>(base: &Path, f: impl FnOnce() -> T) -> T {
        TEST_LOCAL_HOME.with(|h| *h.borrow_mut() = Some(base.join("home")));
        let out = with_sealed_waits(base, f);
        TEST_LOCAL_HOME.with(|h| *h.borrow_mut() = None);
        out
    }

    /// 本機執行器：不經 ssh，同一套守門／共用 target／rsync 在本機跑；cargo 在租到的 `shared/` 裡跑、環境是乾淨的（pane 的 `AM_*` 不漏進去），
    /// 健康檔記在 `local` 這台名下。
    #[test]
    fn the_local_executor_runs_cargo_in_the_shared_target_without_ssh() {
        let base = base();
        let (remote, cwd, data) = local_host(&base, "rc", "pwd > \"$HOME/ran\"; env > \"$HOME/env\"; exit 0\n");
        std::fs::write(cwd.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        // ssh／rsync-over-ssh 一律不准碰：換成會失敗的假程式，被叫到就紅。
        let bin = base.join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(&bin.join("ssh"), "echo 'ssh must not run' >&2; exit 255\n");
        TEST_PROGRAMS.with(|p| *p.borrow_mut() = Some((bin.join("ssh").display().to_string(), "rsync".into())));
        let rc = with_local_home(&base, || run_offload(&remote, &data, &cwd, &["test".to_string()], &Deadline::new(60)));
        TEST_PROGRAMS.with(|p| *p.borrow_mut() = None);
        assert_eq!(rc, 0);
        let ran = std::fs::read_to_string(base.join("home/ran")).unwrap();
        assert!(ran.trim().starts_with(&base.join("rc").display().to_string()) && ran.trim().ends_with("/shared"), "{ran}");
        assert!(Path::new(ran.trim()).join("src/lib.rs").is_file(), "來源同步到本機的共用目錄");
        let env = std::fs::read_to_string(base.join("home/env")).unwrap();
        assert!(!env.lines().any(|l| l.starts_with("AM_") || l.starts_with("HERDR_")), "pane 的環境不漏進去：{env}");
        assert!(env.lines().any(|l| l == format!("HOME={}", base.join("home").display())), "{env}");
        let h = crate::remote_health::read(&data, "local").expect("健康檔記在 local 名下");
        assert!(h.reachable);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 試用守門：池與名額都被佔著就不等、不改走冷目錄，回 `BUSY\t<排隊人數>`；排隊人數＝正在等鎖的守門數。
    #[test]
    fn a_trial_guard_reports_busy_with_the_number_of_waiters() {
        let base = base();
        let one = Admission { max: 1, pool: 0, ..QUICK };
        let mut holder = guard_with(&base, HASH, "job-1-1", one);
        let trial = Admission { try_only: true, shared_wait_secs: 0, queue_wait_secs: 0, ..one };
        let token = LeaseToken::new();
        let err = Lease::start(guard_cmd(&base, HASH, "job-2-2", &token, trial), token, &Deadline::new(0)).err().expect("busy");
        assert_eq!(err.downcast_ref::<Busy>().map(|b| b.waiters), Some(0), "{err:#}");
        assert!(!base.join(format!("rc/{HASH}/job-2-2")).exists(), "試用不改走冷目錄");

        // 一個正常守門在排隊：試用數得到它。
        let b2 = base.clone();
        let waiter = std::thread::spawn(move || guard_with(&b2, HASH, "job-3-3", Admission { shared_wait_secs: 30, ..one }));
        let t0 = Instant::now();
        let waiters = loop {
            let token = LeaseToken::new();
            let err = Lease::start(guard_cmd(&base, HASH, "job-4-4", &token, trial), token, &Deadline::new(0)).err().expect("busy");
            let n = err.downcast_ref::<Busy>().map(|b| b.waiters).unwrap();
            if n > 0 || t0.elapsed() > Duration::from_secs(10) {
                break n;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(waiters, 1);
        holder.finish();
        waiter.join().unwrap().finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 多台：第一台（照設定順序）的池全被佔著就用第二台，不排第一台的隊。
    #[test]
    fn with_several_hosts_a_busy_first_host_hands_the_call_to_a_free_second() {
        let base = base();
        let (a, cwd, data) = local_host(&base, "rc", "pwd > \"$HOME/ran\"; exit 0\n");
        let b = BuildRemoteCfg { remote_root: base.join("rc-b").display().to_string(), ..a.clone() };
        let hash = cache_hash(&source_root(&cwd).0);
        let mut holder = guard_with(&base, &hash, "job-1-1", Admission { max: 1, pool: 0, ..QUICK });
        let rc = with_local_home(&base, || run_offload_on(&[a, b], &data, &cwd, &["check".to_string()], &Deadline::new(60)));
        assert_eq!(rc, 0);
        let ran = std::fs::read_to_string(base.join("home/ran")).unwrap();
        assert!(ran.trim().starts_with(&base.join("rc-b").display().to_string()), "第一台忙，用第二台：{ran}");
        holder.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// 都忙時排排隊人數最少的那台，同分照設定順序。
    #[test]
    fn when_every_host_is_busy_the_shortest_queue_wins_and_ties_keep_config_order() {
        assert_eq!(queue_order(&[(0, 2), (1, 0), (2, 0)]), vec![1, 2, 0]);
        assert_eq!(queue_order(&[(0, 1), (1, 1)]), vec![0, 1]);
        assert!(queue_order(&[]).is_empty());
    }

    /// 本機執行器的負載門檻：設定 > 呼叫端的 CHECK_MAX_LOAD > 核心數 × 1.25（同 wait-load.sh）。
    #[test]
    fn the_local_load_cap_prefers_config_then_check_max_load_then_cores() {
        let cfg = BuildRemoteCfg { host: "local".into(), max_load: 7.5, ..Default::default() };
        assert_eq!(local_max_load(&cfg, Some("40")), 7.5);
        let auto = BuildRemoteCfg { max_load: 0.0, ..cfg };
        assert_eq!(local_max_load(&auto, Some("40")), 40.0);
        assert_eq!(local_max_load(&auto, Some("junk")), std::thread::available_parallelism().unwrap().get() as f64 * 1.25);
        assert!(Admission::try_from_cfg(&auto).max_load.is_some(), "本機試用看負載");
        assert_eq!(Admission::try_from_cfg(&BuildRemoteCfg { host: "r7".into(), ..auto }).max_load, None, "ssh 主機不看");
    }

    /// #1971：本機試用時負載超過上限就是沒有空槽（回 `Busy`，不排隊、不編）；負載低就拿得到。
    /// 注入的是守門讀的負載檔（[`TEST_LOADAVG`]），不是真機器的 loadavg——CI 機負載 >20 時，這條不能跟著紅。
    #[test]
    fn an_overloaded_local_host_has_no_free_slot_on_trial() {
        let base = base();
        let (local, cwd, _data) = local_host(&base, "rc", "exit 0\n");
        let hash = cache_hash(&source_root(&cwd).0);
        let trial = Admission::try_from_cfg(&local);
        let file = base.join("loadavg");
        TEST_LOADAVG.with(|p| *p.borrow_mut() = Some(file.clone()));
        std::fs::write(&file, "999.00 999.00 999.00 1/1 1\n").unwrap();
        let token = LeaseToken::new();
        let err = Lease::start(guard_cmd(&base, &hash, "job-1-1", &token, trial), token, &Deadline::new(0)).err().expect("負載超標就不給");
        assert!(err.downcast_ref::<Busy>().is_some(), "回 Busy：{err:#}");
        std::fs::write(&file, "0.10 0.10 0.10 1/1 1\n").unwrap();
        let token = LeaseToken::new();
        let mut lease = Lease::start(guard_cmd(&base, &hash, "job-2-2", &token, trial), token, &Deadline::new(0)).expect("低負載拿得到空槽");
        lease.finish();
        TEST_LOADAVG.with(|p| *p.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(base);
    }

    /// 設定：舊的單一 `[build.remote]` 展開成一台；`[[build.remote.hosts]]` 照順序接在後面、沒寫的沿用第一台、
    /// `enabled = false` 的略過、只支援金鑰；`host = "local"` 不用 user。設錯的那台只略過（其他台照用），全部都錯才退回本機並講是哪台。
    #[test]
    fn several_build_hosts_are_read_in_order_and_an_old_single_host_config_is_unchanged() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-hosts-cfg-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let args = vec!["test".to_string()];
        let decide = |body: &str| {
            let p = dir.join("c.toml");
            std::fs::write(&p, body).unwrap();
            decide_offload(&p, &args, Vec::<(String, String)>::new())
        };
        let Offload::Go(old) = decide("[build.remote]\nenabled = true\nhost = \"r7\"\nuser = \"r7\"\ncargo_jobs = 8\n") else { panic!("舊設定照轉") };
        assert_eq!(old.len(), 1);
        assert_eq!((old[0].target_label(), old[0].password_id.clone()), ("r7@r7:22".to_string(), None), "舊設定的密碼檔規則不變");
        let multi = "[build.remote]\nenabled = true\nhost = \"local\"\ncargo_jobs = 12\ntest_threads = 6\n\
                     [[build.remote.hosts]]\nhost = \"r7\"\nuser = \"r7\"\nmax_concurrent = 1\n\
                     [[build.remote.hosts]]\nhost = \"off\"\nuser = \"x\"\nenabled = false\n";
        let Offload::Go(hosts) = decide(multi) else { panic!("多台照轉") };
        assert_eq!(hosts.iter().map(BuildRemoteCfg::target_label).collect::<Vec<_>>(), vec!["local", "r7@r7:22"]);
        assert_eq!((hosts[1].cargo_jobs, hosts[1].test_threads, hosts[1].max_concurrent), (12, 6, 1), "沒寫的沿用第一台");
        assert_eq!(hosts[1].password_id.as_deref(), Some(""), "之後的主機只走金鑰／agent");
        // 額外主機設錯（主機名不合法、沒寫 user 又沿用到本機執行器的空 user）：只略過那台。
        let bad_extra = "[build.remote]\nenabled = true\nhost = \"local\"\n\
                         [[build.remote.hosts]]\nhost = \"h;id\"\nuser = \"u\"\n[[build.remote.hosts]]\nhost = \"r9\"\n";
        let Offload::Go(hosts) = decide(bad_extra) else { panic!("一台設錯不拖累整組") };
        assert_eq!(hosts.iter().map(BuildRemoteCfg::target_label).collect::<Vec<_>>(), vec!["local"]);
        let (ok, skipped) = usable_targets(BuildRemoteCfg { host: "r7".into(), user: String::new(), ..hosts[0].clone() }.targets());
        assert!(ok.is_empty() && skipped.len() == 1 && skipped[0].contains("r7"), "{skipped:?}");
        // 全部都錯：退回本機並講是哪台。
        match decide("[build.remote]\nenabled = true\nhost = \"h;id\"\nuser = \"u\"\n") {
            Offload::Local(Some(why)) => assert!(why.contains("h;id"), "{why}"),
            _ => panic!("壞掉的那台要講"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 一台連不上的 ssh 主機：假 ssh 一律回 255（沒進去）；rsync 用真的（本機執行器同步到本機路徑）。
    fn with_dead_ssh<T>(base: &Path, f: impl FnOnce() -> T) -> T {
        let bin = base.join("deadbin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(&bin.join("ssh"), "echo 'ssh: connect to host r7 port 22: No route to host' >&2; exit 255\n");
        TEST_PROGRAMS.with(|p| *p.borrow_mut() = Some((bin.join("ssh").display().to_string(), "rsync".into())));
        let out = with_local_home(base, f);
        TEST_PROGRAMS.with(|p| *p.borrow_mut() = None);
        out
    }

    fn dead_r7(base: &Path, local: &BuildRemoteCfg) -> BuildRemoteCfg {
        BuildRemoteCfg {
            host: "r7".into(),
            user: "r7".into(),
            ssh_port: 22,
            remote_root: base.join("rc-r7").display().to_string(),
            ..local.clone()
        }
    }

    /// 連不上的那台換下一台：第一台 ssh 進不去就用第二台編，連不上的那台記成連不上。
    #[test]
    fn an_unreachable_first_host_hands_the_call_to_the_next_one() {
        let base = base();
        let (local, cwd, data) = local_host(&base, "rc", "pwd > \"$HOME/ran\"; exit 0\n");
        let r7 = dead_r7(&base, &local);
        let rc = with_dead_ssh(&base, || run_offload_on(&[r7, local], &data, &cwd, &["check".to_string()], &Deadline::new(60)));
        assert_eq!(rc, 0);
        let ran = std::fs::read_to_string(base.join("home/ran")).unwrap();
        assert!(ran.trim().starts_with(&base.join("rc").display().to_string()), "改在第二台（本機）編：{ran}");
        assert!(!crate::remote_health::read(&data, "r7@r7:22").expect("記下 r7").reachable);
        assert!(crate::remote_health::read(&data, "local").expect("記下 local").reachable);
        let _ = std::fs::remove_dir_all(base);
    }

    /// review #1956-1：r7 連不上、本機名額滿到逾時而退回本機（QueueFull）時，健康檔記在**排的那台**（local）——
    /// r7 仍是連不上，不會被蓋成連得上。
    #[test]
    fn a_queue_full_fallback_marks_the_queued_host_not_the_first_one() {
        let base = base();
        let (local, cwd, data) = local_host(&base, "rc", "echo must-not-run >&2; exit 99\n");
        let r7 = dead_r7(&base, &local);
        // 別的 repo 佔著本機唯一的名額（池是空的，只有名額滿）。
        let mut holder = guard_with(&base, "00000000000000a1", "job-1-1", Admission { max: 1, ..QUICK });
        TEST_QUEUE_WAIT_SECS.with(|w| w.set(Some(1)));
        let rc = with_dead_ssh(&base, || run_offload_on(&[r7, local], &data, &cwd, &["check".to_string()], &Deadline::new(60)));
        TEST_QUEUE_WAIT_SECS.with(|w| w.set(None));
        assert_eq!(rc, EXIT_RUN_LOCALLY);
        assert!(!crate::remote_health::read(&data, "r7@r7:22").unwrap().reachable, "r7 還是連不上");
        assert!(crate::remote_health::read(&data, "local").unwrap().reachable, "本機守門有回話＝連得上");
        holder.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// review #1956-3：試用時先看名額再播種——名額滿了就回忙，不先把共用 target 拷一份出來。
    #[test]
    fn a_trial_on_a_full_host_does_not_seed_a_new_pool_copy_first() {
        let base = base();
        let adm = Admission { max: 1, pool: 2, ..QUICK };
        let mut holder = guard_with(&base, HASH, "job-1-1", adm);
        assert!(holder.hs.dir.ends_with("/shared"), "{:?}", holder.hs);
        std::fs::create_dir_all(Path::new(&holder.hs.dir).join("target/debug/deps")).unwrap();
        std::fs::write(Path::new(&holder.hs.dir).join("target/debug/deps/big"), "x").unwrap();
        let trial = Admission { try_only: true, shared_wait_secs: 0, queue_wait_secs: 0, ..adm };
        let token = LeaseToken::new();
        let err = Lease::start(guard_cmd(&base, HASH, "job-2-2", &token, trial), token, &Deadline::new(0)).err().expect("busy");
        assert!(err.downcast_ref::<Busy>().is_some(), "{err:#}");
        let second = base.join(format!("rc/{HASH}/shared-2"));
        assert!(!second.exists() && !base.join(format!("rc/{HASH}/shared-2.seed")).exists(), "名額滿了不先播種 {}", second.display());
        holder.finish();
        let _ = std::fs::remove_dir_all(base);
    }

    /// review #1956-2：試用輪遇到「鎖檔在 open 與 flock 之間被回收」（守門回 76）時回報忙碌、讓 helper 換下一台，
    /// 不是讓整次失敗。用一個對鎖檔路徑回假 inode 的 `stat` 重現（`/proc/<pid>/fd` 那邊照真的）。
    #[test]
    fn a_trial_whose_lock_file_was_recycled_reports_busy() {
        let base = base();
        let bin = base.join("statbin");
        std::fs::create_dir_all(&bin).unwrap();
        let real = String::from_utf8(Command::new("sh").args(["-c", "command -v stat"]).output().unwrap().stdout).unwrap();
        write_exec(
            &bin.join("stat"),
            &format!("for a; do last=$a; done\ncase \"$*\" in *%d:%i*) case \"$last\" in /proc/*) ;; *.lock) echo 0:0; exit 0 ;; esac ;; esac\nexec {} \"$@\"\n", real.trim()),
        );
        let trial = Admission { max: 2, pool: 0, try_only: true, shared_wait_secs: 0, queue_wait_secs: 0, ..QUICK };
        let token = LeaseToken::new();
        let mut cmd = guard_cmd(&base, HASH, "job-1-1", &token, trial);
        cmd.env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
        let err = Lease::start(cmd, token, &Deadline::new(0)).err().expect("busy");
        assert!(err.downcast_ref::<Busy>().is_some(), "回 76 要當成忙碌：{err:#}");
        let _ = std::fs::remove_dir_all(base);
    }

    /// 合流條件（#1956）：舊的單一 `[build.remote]`（沒有 `hosts`）走的跟改動前完全一樣——
    /// 一台、沒有略過的設錯主機（不印新的 ⚠️）、一個正常守門（`try=0`，沒有試用輪、不看負載），
    /// ssh 次數跟 main（2026-10-24，改動前）實測相同：冷的 5 次（守門、讀 manifest ×2、寫 manifest、跑 cargo）、熱的 4 次。
    #[test]
    fn an_old_single_host_config_takes_exactly_the_old_path() {
        let base = base();
        let (remote, cwd, data) = fake_remote(&base, 60, "exit 0\n");
        std::fs::write(cwd.join("src/lib.rs"), "").unwrap();
        let cfg = base.join("c.toml");
        std::fs::write(
            &cfg,
            format!("[build.remote]\nenabled = true\nhost = \"fake\"\nuser = \"me\"\nremote_root = \"{}\"\ncargo_jobs = 1\ntimeout_secs = 60\n", remote.remote_root),
        )
        .unwrap();
        let Offload::Go(hosts) = decide_offload(&cfg, &["test".to_string()], Vec::<(String, String)>::new()) else { panic!("舊設定照轉") };
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].target_label(), remote.target_label());
        assert!(usable_targets(hosts.clone()).1.is_empty(), "沒有略過的主機＝沒有新的警告");
        // 記下每次 ssh 跑的腳本。
        let bin = base.join("fakebin");
        std::fs::rename(bin.join("ssh"), bin.join("ssh-real")).unwrap();
        let log = base.join("ssh.log");
        write_exec(
            &bin.join("ssh"),
            &format!(
                "for a; do last=$a; done\nprintf '%s\\n=====\\n' \"$last\" >> '{}'\nexec '{}' \"$@\"\n",
                log.display(),
                bin.join("ssh-real").display()
            ),
        );
        for (round, want) in [(0, 5), (1, 4)] {
            let rc = with_fake_transport(&base, || run_offload_on(&hosts, &data, &cwd, &["test".to_string()], &Deadline::new(60)));
            assert_eq!(rc, 0);
            let l = std::fs::read_to_string(&log).unwrap();
            let calls: Vec<&str> = l.split("\n=====\n").filter(|c| !c.trim().is_empty()).collect();
            assert_eq!(calls.len(), want, "第 {round} 次的 ssh 次數跟改動前不同：{calls:#?}");
            let guards: Vec<&&str> = calls.iter().filter(|c| c.contains("HUP PIPE")).collect();
            assert_eq!(guards.len(), 1, "只開一次守門");
            assert!(guards[0].contains("try=0;") && guards[0].contains("maxload=0"), "正常守門，不是試用、不看負載");
            std::fs::remove_file(&log).unwrap();
        }
        let _ = std::fs::remove_dir_all(base);
    }
}
