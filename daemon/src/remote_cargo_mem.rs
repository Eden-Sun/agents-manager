//! 外部 rustc／Cargo 主機（`[build.remote]`）的 CPU／RAM 取樣，給左上角徽章。
//!
//! 這台機器不在 herdr 樹裡，本機 `GET /api/mem` 的 `hosts[].total_bytes` 看不到它。
//! SSH 連線方式跟 issue #104 的 offload helper 同一套（密碼檔、askpass／sshpass）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

use crate::config::BuildRemoteCfg;
use crate::memstat::{parse_machine, MachineMem};
use crate::state::App;

const PASSWORD_FILE: &str = "remote-cargo-password";
const ASKPASS_FILE: &str = "remote-cargo-askpass.sh";
const ASKPASS_SH: &str = "#!/bin/sh\n# agents-manager: ssh 問密碼時回答它；密碼只從環境變數來。\nprintf '%s\\n' \"$AM_SSH_PASSWORD\"\n";

const MACHINE_MARK: &str = "__AM_MACHINE__";
const CPU_MARK: &str = "__AM_CPU__";
const RUSTC_MARK: &str = "__AM_RUSTC__";

const SAMPLE_TIMEOUT: Duration = Duration::from_secs(8);

const SAMPLE_SH: &str = r#"echo __AM_MACHINE__; cat /proc/meminfo 2>/dev/null || true; echo __AM_CPU__; nproc 2>/dev/null || echo 0; cat /proc/loadavg 2>/dev/null || true; grep '^cpu ' /proc/stat 2>/dev/null || true; echo __AM_RUSTC__; ps -eo rss=,pcpu=,comm= 2>/dev/null || ps -axo rss=,pcpu=,comm= 2>/dev/null || true"#;

static LAST_CPU: Mutex<Option<CpuTicks>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTicks {
    pub idle: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CargoRemoteMem {
    pub host: String,
    pub user: String,
    /// rustc／cargo／sccache 等編譯行程的 RSS 加總。
    pub rustc_bytes: u64,
    /// 那些行程的 `%cpu` 加總（一核＝100；多核可以超過 100）。第一次取樣可能是 `null`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rustc_cpu_pct: Option<f32>,
    pub rustc_processes: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<MachineMem>,
    /// 整機 CPU 0–100。第一次取樣沒有前一筆 `/proc/stat` 時是 `null`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nproc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load1: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn changed(prev: &Option<CargoRemoteMem>, next: &Option<CargoRemoteMem>) -> bool {
    match (prev, next) {
        (None, None) => false,
        (None, Some(_)) | (Some(_), None) => true,
        (Some(a), Some(b)) => {
            a.host != b.host
                || a.error.is_some() != b.error.is_some()
                || a.rustc_processes != b.rustc_processes
                || a.rustc_bytes.abs_diff(b.rustc_bytes) >= 8 * 1024 * 1024
                || pct_moved(a.cpu_pct, b.cpu_pct)
                || pct_moved(a.rustc_cpu_pct, b.rustc_cpu_pct)
                || match (&a.machine, &b.machine) {
                    (Some(x), Some(y)) => x.available_bytes.abs_diff(y.available_bytes) >= 64 * 1024 * 1024,
                    (x, y) => x.is_some() != y.is_some(),
                }
        }
    }
}

fn pct_moved(a: Option<f32>, b: Option<f32>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() >= 3.0,
        (x, y) => x.is_some() != y.is_some(),
    }
}

pub async fn sample(app: &App) -> Option<CargoRemoteMem> {
    let cfg = app.cfg.get().await;
    let remote = cfg.build.remote;
    if !remote.is_configured() {
        return None;
    }
    match tokio::time::timeout(SAMPLE_TIMEOUT, ssh_sample(&remote, &app.data_dir)).await {
        Ok(Ok(raw)) => Some(parse_sample(&remote, &raw, take_prev_cpu())),
        Ok(Err(e)) => Some(err_row(&remote, e)),
        Err(_) => Some(err_row(&remote, "ssh 逾時".into())),
    }
}

fn err_row(remote: &BuildRemoteCfg, error: String) -> CargoRemoteMem {
    CargoRemoteMem {
        host: remote.host.clone(),
        user: remote.user.clone(),
        rustc_bytes: 0,
        rustc_cpu_pct: None,
        rustc_processes: 0,
        machine: None,
        cpu_pct: None,
        nproc: None,
        load1: None,
        error: Some(error),
    }
}

fn take_prev_cpu() -> Option<CpuTicks> {
    LAST_CPU.lock().ok().and_then(|g| *g)
}

fn store_cpu(ticks: Option<CpuTicks>) {
    if let Ok(mut g) = LAST_CPU.lock() {
        *g = ticks;
    }
}

pub fn parse_sample(remote: &BuildRemoteCfg, out: &str, prev_cpu: Option<CpuTicks>) -> CargoRemoteMem {
    let machine_part = section(out, MACHINE_MARK, CPU_MARK);
    let cpu_part = section(out, CPU_MARK, RUSTC_MARK);
    let rustc_part = after(out, RUSTC_MARK);

    let machine = parse_machine(machine_part);
    let nproc = cpu_part.lines().map(str::trim).find_map(|l| l.parse::<u32>().ok().filter(|&n| n > 0 && n < 4096));
    let load1 = cpu_part.lines().find_map(parse_load1);
    let ticks = cpu_part.lines().find_map(parse_proc_stat);
    let cpu_pct = match (prev_cpu, ticks) {
        (Some(prev), Some(now)) => cpu_percent(prev, now),
        _ => None,
    };
    store_cpu(ticks);

    let (rustc_bytes, rustc_cpu_pct, rustc_processes) = sum_rustc(rustc_part);

    CargoRemoteMem {
        host: remote.host.clone(),
        user: remote.user.clone(),
        rustc_bytes,
        rustc_cpu_pct,
        rustc_processes,
        machine,
        cpu_pct,
        nproc,
        load1,
        error: None,
    }
}

fn section<'a>(out: &'a str, start: &str, end: &str) -> &'a str {
    let Some(s) = out.find(start) else { return "" };
    let body = &out[s + start.len()..];
    match body.find(end) {
        Some(e) => &body[..e],
        None => body,
    }
}

fn after<'a>(out: &'a str, start: &str) -> &'a str {
    match out.find(start) {
        Some(s) => &out[s + start.len()..],
        None => "",
    }
}

pub fn parse_load1(line: &str) -> Option<f32> {
    // `1.50 1.10 0.90 2/100 99`；不要把 `nproc` 的裸數字當成 load。
    let mut it = line.split_whitespace();
    let first = it.next()?;
    it.next()?;
    it.next()?;
    let n: f32 = first.parse().ok()?;
    (n >= 0.0 && n < 10_000.0).then_some(n)
}

/// `/proc/stat` 的 `cpu  user nice system idle iowait irq softirq steal guest guest_nice`
pub fn parse_proc_stat(line: &str) -> Option<CpuTicks> {
    let mut it = line.split_whitespace();
    if it.next() != Some("cpu") {
        return None;
    }
    let mut nums = [0u64; 10];
    let mut n = 0usize;
    for slot in &mut nums {
        let Some(raw) = it.next() else { break };
        let Ok(v) = raw.parse() else { break };
        *slot = v;
        n += 1;
    }
    if n < 4 {
        return None;
    }
    let idle = nums[3].saturating_add(if n > 4 { nums[4] } else { 0 }); // idle + iowait
    let total: u64 = nums[..n].iter().sum();
    (total > 0).then_some(CpuTicks { idle, total })
}

pub fn cpu_percent(prev: CpuTicks, now: CpuTicks) -> Option<f32> {
    let dt = now.total.saturating_sub(prev.total);
    if dt == 0 {
        return None;
    }
    let di = now.idle.saturating_sub(prev.idle);
    let busy = dt.saturating_sub(di) as f32 / dt as f32;
    Some((busy * 100.0).clamp(0.0, 100.0))
}

pub fn is_compiler_comm(comm: &str) -> bool {
    let name = comm.rsplit('/').next().unwrap_or(comm);
    matches!(name, "rustc" | "cargo" | "sccache" | "rustdoc" | "clippy-driver" | "rustc.exe" | "cargo.exe")
}

pub fn sum_rustc(out: &str) -> (u64, Option<f32>, u32) {
    let mut bytes = 0u64;
    let mut cpu = 0f32;
    let mut n = 0u32;
    for line in out.lines() {
        let mut it = line.split_whitespace();
        let (Some(rss), Some(pcpu), Some(comm)) = (it.next(), it.next(), it.next()) else { continue };
        if !is_compiler_comm(comm) {
            continue;
        }
        let Ok(kib) = rss.parse::<u64>() else { continue };
        let Ok(pct) = pcpu.parse::<f32>() else { continue };
        bytes += kib.saturating_mul(1024);
        cpu += pct.max(0.0);
        n += 1;
    }
    (bytes, (n > 0).then_some(cpu), n)
}

async fn ssh_sample(remote: &BuildRemoteCfg, data_dir: &Path) -> Result<String, String> {
    let password = read_password(data_dir).map_err(|e| e.to_string())?;
    let mut cmd = ssh_command(remote, password.as_deref(), data_dir).map_err(|e| e.to_string())?;
    cmd.arg(SAMPLE_SH).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let out = cmd.output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(if err.is_empty() { format!("ssh 結束 {}", out.status) } else { err.to_string() });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn read_password(data_dir: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(data_dir.join(PASSWORD_FILE)) {
        Ok(s) => {
            let s = s.trim_end_matches(['\r', '\n']).to_string();
            Ok((!s.is_empty()).then_some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn has_program(name: &str) -> bool {
    std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p).find(|d| {
                let p = d.join(name);
                p.is_file()
            })
        })
        .is_some()
}

fn ssh_supports_askpass_require() -> bool {
    let Ok(out) = std::process::Command::new("ssh").arg("-V").output() else { return false };
    let v = String::from_utf8_lossy(&out.stderr);
    let Some(rest) = v.split("OpenSSH_").nth(1) else { return false };
    openssh_at_least_8_4(rest)
}

pub fn openssh_at_least_8_4(rest: &str) -> bool {
    let head: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut it = head.split('.');
    let (Some(Ok(major)), minor) = (it.next().map(str::parse::<u32>), it.next().and_then(|m| m.parse::<u32>().ok())) else {
        return false;
    };
    major > 8 || (major == 8 && minor.unwrap_or(0) >= 4)
}

fn askpass_helper(data_dir: &Path) -> anyhow::Result<PathBuf> {
    let path = data_dir.join(ASKPASS_FILE);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(ASKPASS_SH) {
        std::fs::write(&path, ASKPASS_SH)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

fn ssh_command(remote: &BuildRemoteCfg, password: Option<&str>, data_dir: &Path) -> anyhow::Result<Command> {
    let mut cmd = if password.is_some() && has_program("sshpass") {
        let mut c = Command::new("sshpass");
        c.arg("-e").arg("ssh");
        if let Some(pw) = password {
            c.env("SSHPASS", pw);
        }
        c
    } else {
        let mut c = Command::new("ssh");
        if let Some(pw) = password {
            if !ssh_supports_askpass_require() {
                anyhow::bail!("沒有 sshpass，ssh 也太舊，無法用密碼連外部 rustc 主機");
            }
            let helper = askpass_helper(data_dir)?;
            c.env("AM_SSH_PASSWORD", pw)
                .env("SSH_ASKPASS", helper)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .arg("-o")
                .arg("NumberOfPasswordPrompts=1");
        }
        c
    };
    cmd.arg("-p")
        .arg(remote.ssh_port.to_string())
        .args(["-o", "ConnectTimeout=5", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2"])
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg(if password.is_some() { "BatchMode=no" } else { "BatchMode=yes" })
        .arg(format!("{}@{}", remote.user.trim(), remote.host.trim()));
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote() -> BuildRemoteCfg {
        BuildRemoteCfg { enabled: true, host: "192.168.1.46".into(), user: "ubuntu".into(), ssh_port: 22 }
    }

    #[test]
    fn proc_stat_percent_ignores_idle_and_iowait() {
        let a = parse_proc_stat("cpu  100 0 50 850 0 0 0 0").unwrap();
        let b = parse_proc_stat("cpu  200 0 150 1650 0 0 0 0").unwrap();
        // dt=1000, di=800 → busy 20%
        let pct = cpu_percent(a, b).unwrap();
        assert!((pct - 20.0).abs() < 0.01);
    }

    #[test]
    fn loadavg_takes_the_one_minute_number() {
        assert_eq!(parse_load1("4.21 3.10 1.02 2/812 44021"), Some(4.21));
        assert!(parse_load1("cpu  1 2 3 4").is_none());
    }

    #[test]
    fn rustc_and_cargo_rss_add_up_other_comms_do_not() {
        let out = "  102400  12.5 rustc\n    4096   0.5 cargo\n   99999  80.0 sshd\n  204800  40.0 /usr/bin/rustc\n";
        let (bytes, cpu, n) = sum_rustc(out);
        assert_eq!(n, 3);
        assert_eq!(bytes, (102400 + 4096 + 204800) * 1024);
        assert!((cpu.unwrap() - 53.0).abs() < 0.01);
    }

    #[test]
    fn a_full_blob_fills_machine_cpu_and_compiler_rows() {
        let blob = "\
__AM_MACHINE__
MemTotal:       32768000 kB
MemAvailable:   16384000 kB
__AM_CPU__
32
1.50 1.10 0.90 2/100 99
cpu  1000 0 500 8500 0 0 0 0
__AM_RUSTC__
  512000  80.0 rustc
    8192   1.0 cargo
";
        let prev = parse_proc_stat("cpu  500 0 200 4300 0 0 0 0");
        let row = parse_sample(&remote(), blob, prev);
        assert_eq!(row.host, "192.168.1.46");
        assert_eq!(row.nproc, Some(32));
        assert_eq!(row.load1, Some(1.5));
        assert_eq!(row.rustc_processes, 2);
        assert_eq!(row.rustc_bytes, (512000 + 8192) * 1024);
        assert!(row.machine.is_some());
        assert!(row.cpu_pct.unwrap() > 0.0);
        assert!(row.error.is_none());
    }

    #[test]
    fn openssh_version_compares_numbers() {
        assert!(openssh_at_least_8_4("8.4p1"));
        assert!(openssh_at_least_8_4("10.0p1"));
        assert!(!openssh_at_least_8_4("8.3p1"));
    }
}
