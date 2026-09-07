//! `cargo dev` / `cargo down` — start and stop the local dev stack
//! (agents-managerd + the Vite dev server) as background processes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    // spawn the real binary directly (spawning through `cargo run`/`npm run` would
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

    let daemon_log = fs::File::create(logs.join("daemon.log")).expect("create daemon.log");
    let daemon = Command::new(root.join("target/debug/agents-managerd"))
        .arg("serve")
        .current_dir(root)
        .stdout(Stdio::from(daemon_log.try_clone().unwrap()))
        .stderr(Stdio::from(daemon_log))
        .spawn()
        .expect("spawn agents-managerd");

    let web_log = fs::File::create(logs.join("web.log")).expect("create web.log");
    let web = Command::new(root.join("web/node_modules/.bin/vite"))
        .arg("--host")
        .current_dir(root.join("web"))
        .stdout(Stdio::from(web_log.try_clone().unwrap()))
        .stderr(Stdio::from(web_log))
        .spawn()
        .expect("spawn vite dev server (did you run `npm install` in web/?)");

    fs::write(
        pid_file(root),
        format!("{}\n{}\n", daemon.id(), web.id()),
    )
    .expect("write pid file");

    println!("daemon pid={} log={}", daemon.id(), logs.join("daemon.log").display());
    println!("web    pid={} log={}", web.id(), logs.join("web.log").display());
    println!("run `cargo down` to stop both.");
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
        match status {
            Ok(s) if s.success() => println!("stopped pid {pid}"),
            _ => eprintln!("pid {pid} was already gone"),
        }
    }

    fs::remove_file(&path).ok();
}
