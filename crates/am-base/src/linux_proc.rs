//! 本機的 listen port 與 cwd（SPEC「Linux 主機」）。macOS 走 `lsof`；Linux 直接讀 `/proc`：Ubuntu server
//! 不保證裝了 `lsof`，沒裝時 `lsof` 起不來＝`None`（讀不到），打字前的複查與預覽就整片失明。
//!
//! Linux 這一側**產生跟 `lsof -Fpn` 同格式的文字**（`p<pid>` 一行、之後每個 `n<位址>` 一行），
//! 讓 `panes::parse_lsof`／`preview_bind::parse_listeners`／`preview::parse_lsof_cwd` 照舊吃，兩個平台共用一套解析。
//! 範圍跟 `lsof` 不帶 root 時一樣：讀得到 fd 的行程（同使用者）才算。

use std::collections::HashSet;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::Duration;

/// 全機（`pids` 為 `None`）或指定 pid 的 TCP LISTEN socket，`lsof -nP -iTCP -sTCP:LISTEN [-a -p …] -Fpn` 的格式。
/// `None`＝讀不到或逾時，跟 `lsof` 起不來同一個語意（**不是**「沒有 port」）。
pub async fn listen_fpn(pids: Option<&[i32]>, t: Duration) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let pids = pids.map(<[i32]>::to_vec);
        blocking(t, move || listen_fpn_in(Path::new("/proc"), pids.as_deref())).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let script = match pids {
            None => "lsof -nP -iTCP -sTCP:LISTEN -Fpn 2>/dev/null".to_string(),
            Some(p) => format!("lsof -nP -iTCP -sTCP:LISTEN -a -p {} -Fpn 2>/dev/null", join(p)),
        };
        let o = crate::hosts::sh_local(&script, t).await.ok().flatten()?;
        Some(String::from_utf8_lossy(&o.stdout).into_owned())
    }
}

/// 這些 pid 的 cwd，`lsof -nP -a -d cwd -p … -Fpn` 的格式。`None` 的語意同 [`listen_fpn`]。
pub async fn cwd_fpn(pids: &[i32], t: Duration) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let pids = pids.to_vec();
        blocking(t, move || cwd_fpn_in(Path::new("/proc"), &pids)).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let o = crate::hosts::sh_local(&format!("lsof -nP -a -d cwd -p {} -Fpn 2>/dev/null", join(pids)), t).await.ok().flatten()?;
        Some(String::from_utf8_lossy(&o.stdout).into_owned())
    }
}

#[cfg(not(target_os = "linux"))]
fn join(pids: &[i32]) -> String {
    pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",")
}

/// `/proc` 讀起來是毫秒級，但仍是阻塞 IO：丟到 blocking 執行緒，逾時照樣回 `None`，不卡 tokio worker。
#[cfg(target_os = "linux")]
async fn blocking(t: Duration, f: impl FnOnce() -> String + Send + 'static) -> Option<String> {
    tokio::time::timeout(t, tokio::task::spawn_blocking(f)).await.ok()?.ok()
}

/// `/proc/net/tcp`＋`tcp6` 的 LISTEN（st=`0A`）→ socket inode 對 `位址:port`。讀不到的檔當成空的
/// （例如核心關了 IPv6 就沒有 `tcp6`）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn listening_inodes(proc_root: &Path) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for (file, v6) in [("net/tcp", false), ("net/tcp6", true)] {
        let Ok(text) = std::fs::read_to_string(proc_root.join(file)) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != "0A" {
                continue;
            }
            let (Some(addr), Ok(inode)) = (endpoint(f[1], v6), f[9].parse::<u64>()) else { continue };
            if inode != 0 {
                out.push((inode, addr));
            }
        }
    }
    out
}

/// `0100007F:1F90` → `127.0.0.1:8080`；`00000000000000000000000001000000:0050` → `[::1]:80`。
/// 核心用 `%08X` 印**記憶體裡的網路序位元組當成本機序 u32**，所以每個 32 位元字用 `to_ne_bytes` 還原。
/// 萬用位址寫成 `*`，跟 `lsof` 一樣（`preview_bind::is_loopback` 把它當成對外）。
pub fn endpoint(field: &str, v6: bool) -> Option<String> {
    let (addr, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let words = |n: usize| -> Option<Vec<u8>> {
        if addr.len() != n * 8 {
            return None;
        }
        let mut b = Vec::with_capacity(n * 4);
        for i in 0..n {
            b.extend_from_slice(&u32::from_str_radix(&addr[i * 8..i * 8 + 8], 16).ok()?.to_ne_bytes());
        }
        Some(b)
    };
    let host = if v6 {
        let b: [u8; 16] = words(4)?.try_into().ok()?;
        let ip = Ipv6Addr::from(b);
        if ip.is_unspecified() { "*".to_string() } else { format!("[{ip}]") }
    } else {
        let b: [u8; 4] = words(1)?.try_into().ok()?;
        let ip = Ipv4Addr::from(b);
        if ip.is_unspecified() { "*".to_string() } else { ip.to_string() }
    };
    Some(format!("{host}:{port}"))
}

/// `/proc/<pid>/fd/*` 指到 `socket:[inode]` 的那些 inode。讀不到（別的使用者、行程剛結束）＝空。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn socket_inodes(proc_root: &Path, pid: i32) -> HashSet<u64> {
    let Ok(rd) = std::fs::read_dir(proc_root.join(pid.to_string()).join("fd")) else { return HashSet::new() };
    rd.filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter_map(|l| l.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok())
        .collect()
}

/// `proc_root` 底下的所有 pid（數字目錄）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn all_pids(proc_root: &Path) -> Vec<i32> {
    let Ok(rd) = std::fs::read_dir(proc_root) else { return Vec::new() };
    let mut v: Vec<i32> = rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect();
    v.sort_unstable();
    v
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn listen_fpn_in(proc_root: &Path, pids: Option<&[i32]>) -> String {
    let listening = listening_inodes(proc_root);
    let mut out = String::new();
    if listening.is_empty() {
        return out;
    }
    let pids = pids.map_or_else(|| all_pids(proc_root), <[i32]>::to_vec);
    for pid in pids {
        let mine = socket_inodes(proc_root, pid);
        let addrs: Vec<&str> = listening.iter().filter(|(ino, _)| mine.contains(ino)).map(|(_, a)| a.as_str()).collect();
        if addrs.is_empty() {
            continue;
        }
        let _ = writeln!(out, "p{pid}");
        for a in addrs {
            let _ = writeln!(out, "n{a}");
        }
    }
    out
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn cwd_fpn_in(proc_root: &Path, pids: &[i32]) -> String {
    let mut out = String::new();
    for &pid in pids {
        let Ok(cwd) = std::fs::read_link(proc_root.join(pid.to_string()).join("cwd")) else { continue };
        let Some(cwd) = cwd.to_str().filter(|c| !c.contains('\n')) else { continue };
        let _ = writeln!(out, "p{pid}\nn{cwd}");
    }
    out
}
