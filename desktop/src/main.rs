//! Desktop shell for agents-manager.
//!
//! Deliberately thin: `agents-managerd` (shipped as a Tauri sidecar) still serves the whole
//! UI over loopback, so the webview only has to point at it. The shell's jobs are
//! (1) hand the daemon a real login-shell PATH — a `.app` launched from Finder inherits
//! launchd's bare `/usr/bin:/bin:/usr/sbin:/sbin`, which would hide `herdr`, `claude`,
//! `codex`, … — (2) wait for the listener, and (3) stop the daemon on quit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::Manager;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// Matches `externalBin` in tauri.conf.json (the `-<triple>` suffix is stripped when bundled).
const SIDECAR: &str = "agents-managerd";
/// How long the daemon gets to bind its port before the splash turns into an error.
const BOOT_TIMEOUT: Duration = Duration::from_secs(45);
/// A login shell that stalls (rc waiting on input, slow version manager) must not stall the app.
const PATH_PROBE_TIMEOUT: Duration = Duration::from_secs(6);
/// Quit must feel instant, so the daemon gets this long to honour SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_millis(1500);
/// The splash page may still be loading when an error arrives, so the shell re-publishes it
/// for this long; the page also picks the state up itself on load.
const REPORT_WINDOW: Duration = Duration::from_secs(5);

/// Without these the daemon cannot get off the ground, so the shell says so up front instead
/// of letting the boot time out with nothing to go on. `(binary, how to install it)`.
const REQUIRED_CLIS: &[(&str, &str)] = &[("herdr", "brew install herdr")];
/// At least one of these is needed to actually run a bot, but the daemon starts fine without
/// them and the UI reports what it found — a missing one is a warning, not a stop.
const AGENT_CLIS: &[&str] = &["claude", "codex", "grok"];

/// The sidecar we spawned, so `RunEvent::Exit` can stop it. Stays `None` when the user
/// already had a daemon running in a terminal — that one is not ours to kill.
static DAEMON: Mutex<Option<CommandChild>> = Mutex::new(None);
/// Set on quit. The boot thread checks it around `spawn()`, whose window is otherwise long
/// enough for a quit to miss the child entirely and leave it orphaned.
static EXITING: AtomicBool = AtomicBool::new(false);

/// What is (or is not) sitting on the daemon's port.
#[derive(PartialEq)]
enum Port {
    Free,
    /// Answers `GET /api/session` the way agents-managerd does.
    Daemon,
    /// Something is listening, but it is not ours — attaching would show a stranger's page.
    Foreign,
}

fn main() {
    let addr = daemon_addr();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(move |app| {
            let handle = app.handle().clone();
            // Everything below can block (login shell probe, daemon boot), so it runs off the
            // main thread and the splash window paints immediately.
            std::thread::spawn(move || boot(handle, addr));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("build tauri app")
        .run(|_app, event| {
            if let tauri::RunEvent::Exit = event {
                EXITING.store(true, Ordering::SeqCst);
                if let Some(child) = DAEMON.lock().unwrap().take() {
                    stop_daemon(child);
                }
            }
        });
}

fn boot(handle: tauri::AppHandle, addr: SocketAddr) {
    // A daemon already running in a terminal wins: attach to it, skip the preflight (it
    // evidently found what it needed), and leave it alone on quit. The check has to identify
    // the daemon, not just find a listener — otherwise any process holding the port would
    // have the window navigated to it.
    let running = match identify(addr) {
        Port::Daemon => true,
        Port::Free => false,
        Port::Foreign => {
            report(&handle, format!("{{kind:\"taken\",addr:{:?}}}", addr.to_string()));
            return;
        }
    };
    if !running {
        let env = login_env();
        let path = env
            .get("PATH")
            .cloned()
            .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());

        let missing: Vec<&(&str, &str)> =
            REQUIRED_CLIS.iter().filter(|(bin, _)| resolve(&path, bin).is_none()).collect();
        if !missing.is_empty() {
            report_missing(&handle, &missing);
            return;
        }
        if !AGENT_CLIS.iter().any(|bin| resolve(&path, bin).is_some()) {
            eprintln!("warning: none of {AGENT_CLIS:?} is on PATH; no bot will be able to start");
        }

        if let Err(e) = spawn_daemon(&handle, env) {
            eprintln!("could not start agents-managerd: {e}");
            fail(&handle, addr);
            return;
        }
    }
    let deadline = Instant::now() + BOOT_TIMEOUT;
    while identify(addr) != Port::Daemon {
        if EXITING.load(Ordering::SeqCst) {
            return;
        }
        if Instant::now() >= deadline {
            fail(&handle, addr);
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // ATS is happier with the `localhost` exception domain than a bare IP, and the daemon
    // accepts either as a `Host` (docs/API.md §0).
    show(&handle, &format!("http://localhost:{}/", addr.port()));
}

/// Point the (already visible) splash window at `url`.
fn show(handle: &tauri::AppHandle, url: &str) {
    let Some(w) = handle.get_webview_window("main") else { return };
    match url.parse() {
        Ok(u) => {
            if let Err(e) = w.navigate(u) {
                eprintln!("navigate {url}: {e}");
            }
        }
        Err(e) => eprintln!("bad url {url}: {e}"),
    }
}

/// The daemon never bound its port. Rewrite the splash copy in place rather than navigating,
/// so no second page has to be bundled.
fn fail(handle: &tauri::AppHandle, addr: SocketAddr) {
    eprintln!("agents-managerd did not come up on {addr}");
    // `{:?}` on the String gives a quoted, escaped JS literal.
    report(handle, format!("{{kind:\"failed\",addr:{:?}}}", addr.to_string()));
}

/// Publish an error state to the splash page. A preflight can finish before the page's
/// `<script>` has run, and a single `eval` then vanishes silently — leaving the spinner up
/// forever — so park the state in `window.amPending` (which the page also reads on load) and
/// keep re-publishing it for a bounded while in case the document itself was not there yet.
fn report(handle: &tauri::AppHandle, state: String) {
    let Some(w) = handle.get_webview_window("main") else { return };
    let js = format!("window.amPending = {state}; window.amRender && window.amRender();");
    let deadline = Instant::now() + REPORT_WINDOW;
    loop {
        if let Err(e) = w.eval(&js) {
            eprintln!("eval failed: {e}");
            return;
        }
        if Instant::now() >= deadline || EXITING.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// SIGTERM, then SIGKILL. Only the graceful path closes the ssh ControlMasters
/// (SPEC §11.3.5), and `CommandChild::kill` is a bare SIGKILL.
fn stop_daemon(child: CommandChild) {
    #[cfg(unix)]
    {
        let pid = child.pid() as libc::pid_t;
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            let deadline = Instant::now() + TERM_GRACE;
            while Instant::now() < deadline {
                // ESRCH once the shell plugin's reader thread has reaped it.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let _ = child.kill();
}

fn spawn_daemon(
    handle: &tauri::AppHandle,
    env: HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // The quit handler can only stop what it can see, so bracket the spawn with the flag:
    // a quit arriving mid-`spawn()` would otherwise find `DAEMON` still empty.
    if EXITING.load(Ordering::SeqCst) {
        return Ok(());
    }
    let (mut rx, child) = handle.shell().sidecar(SIDECAR)?.args(["serve"]).envs(env).spawn()?;
    if EXITING.load(Ordering::SeqCst) {
        stop_daemon(child);
        return Ok(());
    }
    *DAEMON.lock().unwrap() = Some(child);
    // The daemon's tracing output is the only diagnostic a packaged build has; forward it to
    // our stderr so running the bundle's binary from a terminal shows it (docs/PACKAGING.md §3).
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                CommandEvent::Stdout(b) | CommandEvent::Stderr(b) => {
                    eprint!("{}", String::from_utf8_lossy(&b))
                }
                CommandEvent::Terminated(t) => eprintln!("agents-managerd exited: {t:?}"),
                _ => {}
            }
        }
    });
    Ok(())
}

/// A required CLI is not installed (or not on the login shell's PATH). Name it, and say how
/// to get it, rather than letting the daemon fail to start behind a generic timeout.
fn report_missing(handle: &tauri::AppHandle, missing: &[&(&str, &str)]) {
    for (bin, hint) in missing {
        eprintln!("required command `{bin}` not found on PATH — install it with: {hint}");
    }
    let list = missing
        .iter()
        .map(|(bin, hint)| format!("[{bin:?},{hint:?}]"))
        .collect::<Vec<_>>()
        .join(",");
    report(handle, format!("{{kind:\"missing\",missing:[{list}]}}"));
}

/// `which`, against a PATH we were handed rather than our own environment.
fn resolve(path: &str, bin: &str) -> Option<std::path::PathBuf> {
    path.split(':').filter(|d| !d.is_empty()).map(|d| std::path::Path::new(d).join(bin)).find(
        |c| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(c).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                c.is_file()
            }
        },
    )
}

/// Who owns `addr`? A bare TCP connect is not enough: whatever holds the port would be taken
/// for the daemon and shown in the window, so ask it for `GET /api/session` and insist on the
/// answer agents-managerd gives (`daemon/src/api.rs`). Hand-rolled to keep the shell's
/// dependency list at "tauri plus two small crates".
fn identify(addr: SocketAddr) -> Port {
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) else {
        return Port::Free;
    };
    let io = Duration::from_millis(1500);
    let _ = s.set_read_timeout(Some(io));
    let _ = s.set_write_timeout(Some(io));
    // `/api/session` is rejected unless `Host` is loopback; when the daemon listens on a real
    // interface it answers 403 instead, which still only agents-managerd does at this path.
    let host = if addr.ip().is_loopback() || addr.ip().is_unspecified() {
        format!("127.0.0.1:{}", addr.port())
    } else {
        addr.to_string()
    };
    let req = format!("GET /api/session HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return Port::Foreign;
    }
    let mut buf = Vec::new();
    if s.take(64 * 1024).read_to_end(&mut buf).is_err() && buf.is_empty() {
        return Port::Foreign;
    }
    let text = String::from_utf8_lossy(&buf);
    let ok = text.starts_with("HTTP/1.1 200") && text.contains("\"token\"") && text.contains("\"port\"");
    let non_local = text.starts_with("HTTP/1.1 403") && text.contains("non-local request");
    if ok || non_local {
        Port::Daemon
    } else {
        Port::Foreign
    }
}

/// `[server] listen` from the daemon's config, falling back to SPEC §5's default.
fn daemon_addr() -> SocketAddr {
    const DEFAULT: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 7788);
    configured_listen().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT)
}

fn configured_listen() -> Option<String> {
    let dir = match std::env::var_os("AM_DATA_DIR").filter(|d| !d.is_empty()) {
        Some(d) => std::path::PathBuf::from(d),
        None => dirs::home_dir()?.join(".config/agents-manager"),
    };
    let text = std::fs::read_to_string(dir.join("config.toml")).ok()?;
    let value: toml::Value = text.parse().ok()?;
    Some(value.get("server")?.get("listen")?.as_str()?.to_string())
}

/// Ask the user's login shell for the PATH it would give a terminal, and hand that to the
/// daemon — `herdr` usually lives in `~/.cargo/bin` or `/opt/homebrew/bin`, neither of which
/// is on a Finder-launched app's PATH. `-i` is included because PATH is conventionally set in
/// `.zshrc`/`.bashrc` rather than the profile.
fn login_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(path) = shell_path() {
        env.insert("PATH".to_string(), path);
    }
    if let Some(home) = dirs::home_dir() {
        env.insert("HOME".to_string(), home.to_string_lossy().to_string());
    }
    env
}

fn shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = std::process::Command::new(&shell)
        .args(["-lic", "printf %s \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let (tx, rx) = std::sync::mpsc::channel();
    let mut out = child.stdout.take()?;
    std::thread::spawn(move || {
        let mut buf = String::new();
        use std::io::Read;
        let _ = out.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    let text = match rx.recv_timeout(PATH_PROBE_TIMEOUT) {
        Ok(t) => {
            let _ = child.wait();
            t
        }
        Err(_) => {
            eprintln!("login shell PATH probe timed out; falling back to the inherited PATH");
            let _ = child.kill();
            return None;
        }
    };
    // An interactive rc may print its own banner first; the PATH is the last line.
    text.lines().last().map(str::trim).filter(|p| p.contains('/')).map(str::to_string)
}
