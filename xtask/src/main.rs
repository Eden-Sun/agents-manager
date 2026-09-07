//! `cargo dev` / `cargo down` — start and stop the local dev stack
//! (agents-managerd + the Vite dev server) as background processes.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

fn dev(root: &Path) {
    if pid_file(root).exists() {
        eprintln!(
            "{} already exists — is the stack already running? run `cargo down` first.",
            pid_file(root).display()
        );
        std::process::exit(1);
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

    let daemon_log = fs::File::create(logs.join("daemon.log")).expect("create daemon.log");
    let daemon = Command::new(root.join("target/debug/agents-managerd"))
        .arg("serve")
        .current_dir(root)
        // Dev is reached from other devices on the LAN (`vite --host`), whose browsers send a
        // non-localhost Origin; the daemon's anti-CSRF check rejects that unless told to allow
        // it. `agents-managerd serve` run directly stays secure-by-default — this is dev-only.
        .env("AM_ALLOW_LAN_ORIGIN", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(daemon_log.try_clone().unwrap()))
        .stderr(Stdio::from(daemon_log))
        // Its own process group *and* detached stdin: without this, Ctrl-C on `cargo dev`'s
        // terminal, or the terminal/session closing, sends SIGINT/SIGHUP to the daemon along
        // with it, even though `cargo dev` itself already exited.
        .process_group(0)
        .spawn()
        .expect("spawn agents-managerd");

    let web_log = fs::File::create(logs.join("web.log")).expect("create web.log");
    let web = Command::new(root.join("web/node_modules/.bin/vite"))
        .arg("--host")
        .current_dir(root.join("web"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(web_log.try_clone().unwrap()))
        .stderr(Stdio::from(web_log))
        .process_group(0)
        .spawn()
        .expect("spawn vite dev server");

    fs::write(
        pid_file(root),
        format!("{}\n{}\n", daemon.id(), web.id()),
    )
    .expect("write pid file");

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

    for pid in contents.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
        // Kill the whole process group is overkill here; `cargo run` and `npm run dev`
        // both exec into the real child on macOS/Linux, so a plain kill is enough.
        let status = Command::new("kill").arg(pid.to_string()).status();
        if !matches!(status, Ok(s) if s.success()) {
            eprintln!("pid {pid} was already gone");
            continue;
        }
        // Wait for it to actually exit — a following `cargo dev` binding the same port
        // (e.g. the daemon's 7788) would otherwise race a socket the kernel hasn't freed yet.
        for _ in 0..50 {
            if Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|s| !s.success()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        println!("stopped pid {pid}");
    }

    fs::remove_file(&path).ok();
}
