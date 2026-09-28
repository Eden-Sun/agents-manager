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
fn endpoint(field: &str, v6: bool) -> Option<String> {
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
fn listen_fpn_in(proc_root: &Path, pids: Option<&[i32]>) -> String {
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
fn cwd_fpn_in(proc_root: &Path, pids: &[i32]) -> String {
    let mut out = String::new();
    for &pid in pids {
        let Ok(cwd) = std::fs::read_link(proc_root.join(pid.to_string()).join("cwd")) else { continue };
        let Some(cwd) = cwd.to_str().filter(|c| !c.contains('\n')) else { continue };
        let _ = writeln!(out, "p{pid}\nn{cwd}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// 假的 `/proc`：net/tcp、net/tcp6 照核心的欄位排，fd 是指到 `socket:[inode]` 的懸空符號連結（跟真的一樣）。
    fn fake_proc(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("am-linux-proc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("net")).unwrap();
        let hdr = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        let le = cfg!(target_endian = "little");
        // 127.0.0.1:5173 LISTEN、0.0.0.0:8080 LISTEN、127.0.0.1:9999 ESTABLISHED（不算）。
        let lo = if le { "0100007F" } else { "7F000001" };
        std::fs::write(
            root.join("net/tcp"),
            format!(
                "{hdr}   0: {lo}:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0 100 0 0 10 0\n\
                    1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 222 1 0 100 0 0 10 0\n\
                    2: {lo}:270F {lo}:D431 01 00000000:00000000 00:00000000 00000000  1000        0 333 1 0 20 4 30 10 -1\n"
            ),
        )
        .unwrap();
        // [::1]:3000 LISTEN、[::]:4000 LISTEN。
        let one = if le { "01000000" } else { "00000001" };
        std::fs::write(
            root.join("net/tcp6"),
            format!(
                "{hdr}   0: 000000000000000000000000{one}:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 444 1 0 100 0 0 10 0\n\
                    1: 00000000000000000000000000000000:0FA0 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 555 1 0 100 0 0 10 0\n"
            ),
        )
        .unwrap();
        let fds = |pid: i32, links: &[&str]| {
            let d = root.join(pid.to_string()).join("fd");
            std::fs::create_dir_all(&d).unwrap();
            for (i, l) in links.iter().enumerate() {
                symlink(l, d.join(i.to_string())).unwrap();
            }
        };
        fds(10, &["/dev/null", "socket:[111]", "socket:[333]", "pipe:[9]"]);
        fds(20, &["socket:[222]", "socket:[444]"]);
        fds(30, &["socket:[555]"]);
        fds(40, &["socket:[999]"]);
        symlink("/work/app", root.join("10/cwd")).unwrap();
        symlink("/work/other", root.join("20/cwd")).unwrap();
        root
    }

    #[test]
    fn listeners_come_out_in_lsof_fpn_format_and_parse_like_lsof() {
        let root = fake_proc("listen");
        let all = listen_fpn_in(&root, None);
        assert_eq!(all, "p10\nn127.0.0.1:5173\np20\nn*:8080\nn[::1]:3000\np30\nn*:4000\n", "ESTABLISHED 與沒 listen 的 pid 不列");
        let by_pid = crate::panes::parse_lsof(&all);
        assert_eq!(by_pid[&10], vec![5173]);
        assert_eq!(by_pid[&20], vec![3000, 8080]);
        assert!(!by_pid.contains_key(&40));
        // 預覽的綁定判斷：`*` 跟 `[::1]` 要分得出對外與 loopback。
        let l = crate::preview_bind::parse_listeners(&listen_fpn_in(&root, Some(&[20])));
        assert_eq!(l, vec![("*".to_string(), 8080), ("[::1]".to_string(), 3000)]);
        assert_eq!(crate::preview_bind::exposed_addr(&l), Some("*"));
        assert_eq!(listen_fpn_in(&root, Some(&[10, 99])), "p10\nn127.0.0.1:5173\n", "不存在的 pid 略過");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cwd_comes_out_in_lsof_fpn_format() {
        let root = fake_proc("cwd");
        let out = cwd_fpn_in(&root, &[10, 20, 30]);
        assert_eq!(out, "p10\nn/work/app\np20\nn/work/other\n", "讀不到 cwd 的 pid 略過");
        let m = crate::preview::parse_lsof_cwd(&out);
        assert_eq!(m[&10], "/work/app");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn endpoint_decodes_kernel_hex() {
        let le = cfg!(target_endian = "little");
        assert_eq!(endpoint(if le { "0100007F:0050" } else { "7F000001:0050" }, false).as_deref(), Some("127.0.0.1:80"));
        assert_eq!(endpoint(if le { "0101A8C0:1F90" } else { "C0A80101:1F90" }, false).as_deref(), Some("192.168.1.1:8080"));
        assert_eq!(endpoint("00000000:0016", false).as_deref(), Some("*:22"));
        assert_eq!(endpoint("0100007F", false), None, "沒有 port");
        assert_eq!(endpoint("XYZ:0050", false), None);
        assert_eq!(endpoint("0100007F:0050", true), None, "長度不對的 v6");
    }

    /// 真的正式路徑看自己剛 bind 的 port 與 cwd：Linux 驗核心的 `/proc` 格式（外部編譯主機），macOS 驗 `lsof`
    /// （`check.sh macos-local`）。Linux 的 `/proc` 一定讀得到；macOS 的 `lsof` 起不來或逾時就略過並印原因，比照 shell.rs 的煙霧測試。
    #[tokio::test]
    async fn macos_local_real_listeners_and_cwd_see_this_process() {
        let t = Duration::from_secs(10);
        let me = i32::try_from(std::process::id()).unwrap();
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        let Some(out) = listen_fpn(Some(&[me]), t).await else {
            if cfg!(target_os = "linux") {
                panic!("Linux 的 /proc 一定讀得到");
            }
            return eprintln!("略過：`lsof` 起不來或逾時");
        };
        let l = crate::preview_bind::parse_listeners(&out);
        assert!(l.contains(&("127.0.0.1".to_string(), port)), "{out}");
        // 之後幾趟讀不到：Linux 算錯、macOS 照樣略過那一項。
        let read = |o: Option<String>, what: &str| {
            assert!(o.is_some() || !cfg!(target_os = "linux"), "Linux 的 /proc 讀不到：{what}");
            o
        };
        if let Some(out) = read(listen_fpn(None, t).await, "全機") {
            assert!(crate::panes::parse_lsof(&out).get(&me).is_some_and(|p| p.contains(&port)), "全機掃描也要看得到自己");
        }
        drop(v4);
        if let Some(out) = read(listen_fpn(Some(&[me]), t).await, "關掉之後") {
            assert!(!crate::preview_bind::parse_listeners(&out).iter().any(|(_, p)| *p == port), "關掉之後不再列：{out}");
        }
        if let Some(out) = read(cwd_fpn(&[me], t).await, "cwd") {
            let want = std::env::current_dir().unwrap().canonicalize().unwrap();
            let got = crate::preview::parse_lsof_cwd(&out).get(&me).map(|c| std::path::PathBuf::from(c).canonicalize().unwrap());
            assert_eq!(got, Some(want), "{out}");
        }
    }
}
