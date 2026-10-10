//! `cargo dev` / `cargo down` — start and stop the local dev stack
//! (agents-managerd + the Vite dev server) as background processes.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn repo_root() -> PathBuf {
    // xtask lives at <repo>/xtask, CARGO_MANIFEST_DIR is <repo>/xtask.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent dir")
        .to_path_buf()
}

fn pid_file(root: &Path) -> PathBuf {
    root.join("target/dev.pids")
}

fn log_dir(root: &Path) -> PathBuf {
    root.join("target/dev-logs")
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let root = repo_root();
    match cmd.as_str() {
        "dev" => dev(&root),
        "down" => down(&root),
        other => {
            eprintln!("usage: cargo dev | cargo down (got {other:?})");
            std::process::exit(1);
        }
    }
}

/// `dev.pids` 裡的一顆行程。pid 會被別的程式重用（重開機、行程自己掉了），所以同時記它的角色與
/// `ps lstart`（起來的時間）：`cargo down` 只殺 pid 現在仍然是當初那一顆的（#1075）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Daemon,
    Web,
}

impl Kind {
    /// 指令列裡一定會有的字：`target/debug/agents-managerd serve`、`node …/vite/bin/vite.js`。
    fn needle(self) -> &'static str {
        match self {
            Kind::Daemon => "agents-managerd",
            Kind::Web => "vite",
        }
    }
    fn tag(self) -> &'static str {
        match self {
            Kind::Daemon => "daemon",
            Kind::Web => "web",
        }
    }
    fn from_tag(s: &str) -> Option<Kind> {
        match s {
            "daemon" => Some(Kind::Daemon),
            "web" => Some(Kind::Web),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    pid: u32,
    kind: Kind,
    /// `ps -o lstart=` 的字串；沒有（舊格式）就只比指令列。
    started: Option<String>,
}

/// 每行 `<pid> <daemon|web> <lstart>`，沒有 lstart 寫 `-`。舊版是兩行純 pid（第一行 daemon、第二行 web），讀得進來、沒有 lstart。
fn encode(tracked: &[Tracked]) -> String {
    tracked
        .iter()
        .map(|t| format!("{} {} {}\n", t.pid, t.kind.tag(), t.started.as_deref().unwrap_or("-")))
        .collect()
}

fn decode(text: &str) -> Vec<Tracked> {
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let mut parts = line.trim().splitn(3, ' ');
            let pid: u32 = parts.next()?.parse().ok()?;
            match parts.next() {
                None => Some(Tracked {
                    pid,
                    kind: if i == 0 { Kind::Daemon } else { Kind::Web },
                    started: None,
                }),
                Some(tag) => Some(Tracked {
                    pid,
                    kind: Kind::from_tag(tag)?,
                    started: parts.next().map(str::trim).filter(|s| !s.is_empty() && *s != "-").map(str::to_string),
                }),
            }
        })
        .collect()
}

/// `ps` 看到的 pid 現在是什麼：（指令列，起來的時間）。pid 已經不在＝None。
fn process_info(pid: u32) -> Option<(String, String)> {
    let ps = |field: &str| -> Option<String> {
        let out = Command::new("ps").args(["-o", field, "-p", &pid.to_string()]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    Some((ps("command=")?, ps("lstart=")?))
}

/// 這個 pid 現在是不是當初 `cargo dev` 起的那一顆：角色的指令列字樣對得上，而且（有記的話）起來的時間也一樣。
fn is_ours(t: &Tracked, info: &(String, String)) -> bool {
    info.0.contains(t.kind.needle()) && t.started.as_deref().is_none_or(|s| s == info.1)
}

fn track(pid: u32, kind: Kind) -> Tracked {
    Tracked { pid, kind, started: process_info(pid).map(|i| i.1) }
}

/// 讀 pid 檔，只留下還是當初那一顆、現在仍在跑的行程。其餘（已不在、pid 已被別人用）都是舊資料。
fn live(root: &Path) -> Vec<Tracked> {
    let text = fs::read_to_string(pid_file(root)).unwrap_or_default();
    decode(&text)
        .into_iter()
        .filter(|t| process_info(t.pid).is_some_and(|info| is_ours(t, &info)))
        .collect()
}

fn write_pids(root: &Path, tracked: &[Tracked]) -> std::io::Result<()> {
    fs::write(pid_file(root), encode(tracked))
}

/// SIGTERM 並等它真的走（最多約 5 秒）。kill 本身失敗回 false。
fn terminate(pid: u32) -> bool {
    if !Command::new("kill").arg(pid.to_string()).status().is_ok_and(|s| s.success()) {
        return false;
    }
    // Wait for it to actually exit — a following `cargo dev` binding the same port
    // (e.g. the daemon's 7788) would otherwise race a socket the kernel hasn't freed yet.
    for _ in 0..50 {
        if process_info(pid).is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// 起到一半失敗：把已經起來的收掉、清掉 pid 檔，才不會留下沒有紀錄、佔著 port 的孤兒（#1075）。
fn abort(root: &Path, started: &[Tracked], why: &str) -> ! {
    eprintln!("{why}");
    for t in started {
        if !terminate(t.pid) {
            eprintln!("kill pid {} 失敗，請手動處理", t.pid);
        }
    }
    fs::remove_file(pid_file(root)).ok();
    std::process::exit(1);
}

fn dev(root: &Path) {
    let pids = pid_file(root);
    if pids.exists() {
        let running = live(root);
        if !running.is_empty() {
            eprintln!(
                "{} 裡還有在跑的 dev 行程（{}）；先跑 `cargo down`。",
                pids.display(),
                running.iter().map(|t| format!("{} pid={}", t.kind.tag(), t.pid)).collect::<Vec<_>>().join("、")
            );
            std::process::exit(1);
        }
        // 舊的 pid 檔（重開機後、行程自己掉了）：行程已經不在，或 pid 已被別的程式用掉。不是使用者要手動清的東西。
        println!("{} 是舊的（行程已不在，或 pid 已被別的程式用掉），清掉重來。", pids.display());
        fs::remove_file(&pids).ok();
    }

    let logs = log_dir(root);
    fs::create_dir_all(&logs).expect("create log dir");

    // Build first so `dev` doesn't return before the binary exists, and so we can
    // spawn the real binary directly (spawning through `cargo run`/`bun run` would
    // leave `cargo down` killing a wrapper process instead of the actual server).
    let build = Command::new("cargo")
        .args(["build", "--bin", "agents-managerd"])
        .current_dir(root)
        .status()
        .expect("run cargo build");
    if !build.success() {
        eprintln!("cargo build failed, aborting.");
        std::process::exit(1);
    }

    let install = Command::new("bun")
        .arg("install")
        .current_dir(root.join("web"))
        .status()
        .expect("run bun install (is bun on PATH?)");
    if !install.success() {
        eprintln!("bun install failed, aborting.");
        std::process::exit(1);
    }

    // 兩個 log 檔先開好：這之前什麼行程都還沒起，開檔失敗不會留下東西。
    let daemon_log = fs::File::create(logs.join("daemon.log")).expect("create daemon.log");
    let web_log = fs::File::create(logs.join("web.log")).expect("create web.log");

    let daemon: Child = Command::new(root.join("target/debug/agents-managerd"))
        .arg("serve")
        .current_dir(root)
        // Bind every interface and accept LAN peers/Origins — see main.rs/api.rs. A dev
        // binary does this on its own now; kept explicit so `cargo dev` never depends on how
        // the default is detected.
        .env("AM_DEV_LAN", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(daemon_log.try_clone().unwrap()))
        .stderr(Stdio::from(daemon_log))
        // Its own process group *and* detached stdin: without this, Ctrl-C on `cargo dev`'s
        // terminal, or the terminal/session closing, sends SIGINT/SIGHUP to the daemon along
        // with it, even though `cargo dev` itself already exited.
        .process_group(0)
        .spawn()
        .expect("spawn agents-managerd");
    let mut tracked = vec![track(daemon.id(), Kind::Daemon)];
    // daemon 一起來就先寫 pid 檔：之後 vite 起不來，`cargo down` 也找得到它（#1075）。
    if let Err(e) = write_pids(root, &tracked) {
        abort(root, &tracked, &format!("write {}: {e}", pid_file(root).display()));
    }

    let web = match Command::new(root.join("web/node_modules/.bin/vite"))
        .arg("--host")
        .current_dir(root.join("web"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(web_log.try_clone().unwrap()))
        .stderr(Stdio::from(web_log))
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => abort(
            root,
            &tracked,
            &format!("spawn vite dev server: {e}（web/node_modules 還在嗎？`cd web && bun install`）。已把剛起的 daemon 收掉。"),
        ),
    };
    tracked.push(track(web.id(), Kind::Web));
    if let Err(e) = write_pids(root, &tracked) {
        abort(root, &tracked, &format!("write {}: {e}", pid_file(root).display()));
    }

    println!("daemon pid={} log={}", daemon.id(), logs.join("daemon.log").display());
    println!("web    pid={} log={}", web.id(), logs.join("web.log").display());
    println!("run `cargo down` to stop both.");

    println!("\nlistening:");
    for (name, pid) in [("daemon", daemon.id()), ("web", web.id())] {
        // The daemon does a herdr handshake before it binds, so poll instead of a fixed
        // sleep — a flat delay long enough for that would just slow down the common case.
        let mut lines = Vec::new();
        for _ in 0..50 {
            let out = Command::new("lsof")
                .args(["-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN", "-P", "-n"])
                .output();
            if let Ok(o) = out {
                if !o.stdout.is_empty() {
                    lines = String::from_utf8_lossy(&o.stdout).lines().skip(1).map(str::to_string).collect();
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if lines.is_empty() {
            println!("  {name}: not listening yet (check {}/{name}.log)", logs.display());
        } else {
            for line in lines {
                println!("  {name}: {line}");
            }
        }
    }

    if let Some(url) = vite_local_url(&logs.join("web.log")) {
        println!("\nopen: {url}");
    }
}

/// Vite prints `➜  Local:   http://localhost:<port>/` once it's up; pull that out instead of
/// assuming 5173, since it picks the next free port when that one's taken.
fn vite_local_url(log: &Path) -> Option<String> {
    let text = fs::read_to_string(log).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.split("Local:").nth(1) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn down(root: &Path) {
    let path = pid_file(root);
    let contents = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("{} not found — nothing to stop.", path.display());
            std::process::exit(1);
        }
    };

    for t in decode(&contents) {
        match process_info(t.pid) {
            None => eprintln!("pid {} was already gone", t.pid),
            // pid 被別的程式用掉了（pid 檔是舊的）：不殺，那不是我們起的。
            Some(info) if !is_ours(&t, &info) => eprintln!(
                "pid {} 現在是別的行程（{}），不是 cargo dev 起的 {}；跳過，不殺。pid 檔是舊的。",
                t.pid,
                info.0,
                t.kind.tag()
            ),
            Some(_) => {
                if terminate(t.pid) {
                    println!("stopped pid {}", t.pid);
                } else {
                    eprintln!("kill pid {} 失敗", t.pid);
                }
            }
        }
    }

    fs::remove_file(&path).ok();
}


#[cfg(test)]
mod tests {
    use super::*;

    fn info(cmd: &str, started: &str) -> (String, String) {
        (cmd.into(), started.into())
    }

    #[test]
    fn pid_file_roundtrips_with_role_and_start_time() {
        let t = vec![
            Tracked { pid: 42, kind: Kind::Daemon, started: Some("Sat Oct 10 15:00:00 2026".into()) },
            Tracked { pid: 43, kind: Kind::Web, started: None },
        ];
        assert_eq!(decode(&encode(&t)), t);
    }

    #[test]
    fn legacy_two_line_pid_file_is_still_readable() {
        assert_eq!(
            decode("111\n222\n"),
            vec![
                Tracked { pid: 111, kind: Kind::Daemon, started: None },
                Tracked { pid: 222, kind: Kind::Web, started: None },
            ]
        );
    }

    #[test]
    fn garbage_lines_are_skipped() {
        assert!(decode("not-a-pid\n\n").is_empty());
    }

    #[test]
    fn our_daemon_and_vite_are_recognized() {
        let d = Tracked { pid: 1, kind: Kind::Daemon, started: Some("T".into()) };
        assert!(is_ours(&d, &info("/x/target/debug/agents-managerd serve", "T")));
        let w = Tracked { pid: 2, kind: Kind::Web, started: Some("T".into()) };
        assert!(is_ours(&w, &info("node /x/web/node_modules/vite/bin/vite.js --host", "T")));
    }

    #[test]
    fn reused_pid_of_another_program_is_not_ours() {
        let d = Tracked { pid: 1, kind: Kind::Daemon, started: Some("T".into()) };
        assert!(!is_ours(&d, &info("/Applications/Editor.app/Contents/MacOS/editor", "T")), "別的程式");
        let w = Tracked { pid: 2, kind: Kind::Web, started: Some("T".into()) };
        assert!(!is_ours(&w, &info("node /x/target/debug/agents-managerd", "T")), "角色對不上");
    }

    #[test]
    fn same_command_but_different_start_time_is_not_ours() {
        let d = Tracked { pid: 1, kind: Kind::Daemon, started: Some("Mon".into()) };
        assert!(!is_ours(&d, &info("/x/agents-managerd serve", "Tue")));
    }

    #[test]
    fn legacy_entry_without_start_time_matches_by_command_only() {
        let d = Tracked { pid: 1, kind: Kind::Daemon, started: None };
        assert!(is_ours(&d, &info("/x/agents-managerd serve", "whatever")));
    }
}
