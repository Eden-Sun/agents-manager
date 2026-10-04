//! Claude Code statusLine command for daemon-started claude bots (token via `$AM_HOOK_TOKEN`).
//! POSTs the payload to `/hook/claude` fire-and-forget (never spooled: the next refresh is fresher),
//! and relays the user's *own* statusLine command verbatim so the pane looks unchanged.
//!
//! Budget ≤ 2 s; never exits non-zero; never panics past `run`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{io, thread};

use std::os::unix::process::CommandExt;

const TOTAL_BUDGET: Duration = Duration::from_millis(1_900);
const STDIN_BUDGET: Duration = Duration::from_millis(600);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_PAYLOAD: usize = 1024 * 1024;
const MAX_STATUSLINE_OUTPUT: usize = 64 * 1024;

pub struct StatuslineArgs {
    pub bot: String,
    pub token: String,
    pub port: u16,
}

pub fn run(args: StatuslineArgs) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(std::io::stderr(), "agents-managerd statusline: panic: {info}");
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner(args)));
    std::panic::set_hook(prev);
}

fn inner(args: StatuslineArgs) {
    let deadline = Instant::now() + TOTAL_BUDGET;
    let (input, truncated) = read_stdin_capped(STDIN_BUDGET);

    // Before the POST so the text can ride along; it is the pane's own output, so no delay.
    let status_line = user_statusline_command().and_then(|cmd| relay_user_command(&cmd, &input, deadline));

    let poster = if truncated { None } else { slim_payload(&input, status_line.as_deref()) }.map(|payload| {
        let body = serde_json::json!({
            "bot_id": args.bot,
            "provider": "claude",
            "payload": payload,
            "received_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "truncated": false,
        });
        let port = if args.port != 0 {
            args.port
        } else {
            std::env::var("AM_PORT").ok().and_then(|s| s.parse::<u16>().ok()).unwrap_or(7788)
        };
        let token = crate::hook_cmd::hook_token(&args.token);
        std::thread::spawn(move || post(&body, &token, port, deadline))
    });

    if let Some(h) = poster {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = h.join();
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(remaining);
    }
}

pub fn slim_payload(input: &str, status_line: Option<&str>) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let o = v.as_object()?;
    let mut out = serde_json::Map::new();
    out.insert("hook_event_name".into(), serde_json::json!("StatusLine"));
    // The daemon tracks the transcript path elsewhere.
    for (k, v) in o {
        if k == "transcript_path" {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    if let Some(t) = status_line.map(str::trim).filter(|t| !t.is_empty()) {
        out.insert("status_line".into(), serde_json::json!(t));
    }
    Some(serde_json::Value::Object(out))
}

pub fn user_statusline_command() -> Option<String> {
    let cfg_dir = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
    let text = std::fs::read_to_string(cfg_dir.join("settings.json")).ok()?;
    statusline_command_from_settings(&text)
}

pub fn statusline_command_from_settings(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let sl = v.get("statusLine")?;
    if let Some(t) = sl.get("type").and_then(|t| t.as_str()) {
        if t != "command" {
            return None;
        }
    }
    let cmd = sl.get("command")?.as_str()?.trim().to_string();
    // Refuses to recurse into ourselves.
    if cmd.is_empty() || cmd.contains("agents-managerd statusline") || cmd.contains("hook.sh statusline") {
        return None;
    }
    Some(cmd)
}

struct CommandOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Run in a process group so timing out also terminates ordinary child processes started by the
/// user's shell command. The reader always drains stdout, while retaining only a bounded prefix.
fn run_user_command(cmd: &str, input: &str, deadline: Instant) -> Option<CommandOutput> {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "agents-managerd statusline: spawn user command: {e}");
            return None;
        }
    };
    if let Some(mut sin) = child.stdin.take() {
        let data = input.to_string();
        std::thread::spawn(move || {
            let _ = sin.write_all(data.as_bytes());
        });
    }
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel::<io::Result<CommandOutput>>();
    thread::spawn(move || {
        let mut buf = Vec::with_capacity(MAX_STATUSLINE_OUTPUT);
        let mut truncated = false;
        let mut chunk = [0u8; 8192];
        if let Some(mut s) = stdout {
            loop {
                match s.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let keep = (MAX_STATUSLINE_OUTPUT - buf.len()).min(n);
                        buf.extend_from_slice(&chunk[..keep]);
                        truncated |= keep < n;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
        }
        let _ = tx.send(Ok(CommandOutput {
            bytes: buf,
            truncated,
        }));
    });
    let pgid = child.id() as i32;
    let mut child_exited = false;
    let mut output = None;
    loop {
        if !child_exited {
            match child.try_wait() {
                Ok(Some(_)) => {
                    child_exited = true;
                    // The shell can exit while a background descendant still holds stdout open.
                    // Reap the whole group before waiting for the reader's EOF.
                    kill_process_group(pgid);
                }
                Ok(None) => {}
                Err(_) => {
                    kill_process_group(pgid);
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        if output.is_none() {
            match rx.try_recv() {
                Ok(Ok(buf)) => output = Some(buf),
                Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    kill_process_group(pgid);
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if child_exited {
            if let Some(buf) = output {
                return Some(buf);
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            kill_process_group(pgid);
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}

fn kill_process_group(pgid: i32) {
    // SAFETY: `pgid` is the pid of the child spawned with `process_group(0)` above.
    let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

fn relay_user_command(cmd: &str, input: &str, deadline: Instant) -> Option<String> {
    let output = run_user_command(cmd, input, deadline)?;
    {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&output.bytes);
        let _ = out.flush();
    }
    if output.truncated {
        return None;
    }
    let text = crate::github::strip_ansi(&String::from_utf8_lossy(&output.bytes));
    let text = text.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn read_capped<R: Read>(reader: R, max: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::with_capacity(max.min(8192));
    reader
        .take((max as u64).saturating_add(1))
        .read_to_end(&mut buf)?;
    let truncated = buf.len() > max;
    buf.truncate(max);
    Ok((buf, truncated))
}

fn read_stdin_capped(budget: Duration) -> (String, bool) {
    let (tx, rx) = std::sync::mpsc::channel::<(String, bool)>();
    thread::spawn(move || {
        let input = read_capped(std::io::stdin().lock(), MAX_PAYLOAD)
            .map(|(buf, truncated)| (String::from_utf8_lossy(&buf).into_owned(), truncated))
            .unwrap_or_default();
        let _ = tx.send(input);
    });
    rx.recv_timeout(budget).unwrap_or_default()
}

fn post(body: &serde_json::Value, token: &str, port: u16, deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return;
    }
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
    let url = format!("http://127.0.0.1:{port}/hook/claude");
    let body = body.clone();
    let token = token.to_string();
    rt.block_on(async move {
        let Ok(client) = reqwest::Client::builder().no_proxy().connect_timeout(CONNECT_TIMEOUT).timeout(remaining).build()
        else {
            return;
        };
        let _ = tokio::time::timeout(remaining, client.post(&url).header("X-AM-Bot-Token", token).json(&body).send()).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slim_payload_keeps_only_quota_fields() {
        let v = slim_payload(
            r#"{"session_id":"s","transcript_path":"/x","rate_limits":{"five_hour":{"used_percentage":3,"resets_at":1}},"model":{"id":"m"},"context_window":{"used_percentage":40},"cost":{"total_cost_usd":1}}"#,
            Some("me | proj | 5h:12%"),
        )
        .unwrap();
        assert_eq!(v["hook_event_name"], "StatusLine");
        assert_eq!(v["rate_limits"]["five_hour"]["used_percentage"], 3);
        assert!(v.get("transcript_path").is_none());
        assert_eq!(v["cost"]["total_cost_usd"], 1);
        assert_eq!(v["status_line"], "me | proj | 5h:12%");
        assert!(slim_payload("not json", None).is_none());
        assert!(slim_payload("[1]", None).is_none());
        // Blank / whitespace-only output is not a status line.
        assert!(slim_payload(r#"{"session_id":"s"}"#, Some("  ")).unwrap().get("status_line").is_none());
    }

    #[test]
    fn user_command_from_settings() {
        assert_eq!(
            statusline_command_from_settings(r#"{"statusLine":{"type":"command","command":"sh /x/statusline-command.sh"}}"#),
            Some("sh /x/statusline-command.sh".into())
        );
        assert_eq!(statusline_command_from_settings(r#"{"statusLine":{"type":"static","command":"x"}}"#), None);
        assert_eq!(statusline_command_from_settings(r#"{}"#), None);
        assert_eq!(
            statusline_command_from_settings(r#"{"statusLine":{"command":"/a/agents-managerd statusline --bot b"}}"#),
            None
        );
    }

    #[test]
    fn a_user_command_cannot_escape_the_deadline_after_closing_stdout() {
        let deadline = Instant::now() + Duration::from_millis(100);
        let started = Instant::now();
        let _ = relay_user_command("exec 1>&-; sleep 2", "{}", deadline);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "statusline waited for a child after stdout closed"
        );
    }

    #[test]
    fn user_command_output_is_bounded_while_stdout_is_still_drained() {
        let output = run_user_command(
            "head -c 131072 /dev/zero | tr '\\000' x",
            "{}",
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(output.bytes.len(), MAX_STATUSLINE_OUTPUT);
        assert!(output.truncated);
    }

    #[test]
    fn oversized_stdin_is_detected_and_only_the_prefix_is_kept() {
        let input = vec![b'x'; 32];
        let (kept, truncated) = read_capped(input.as_slice(), 16).unwrap();
        assert_eq!(kept, vec![b'x'; 16]);
        assert!(truncated);
        let (kept, truncated) = read_capped(input.as_slice(), 32).unwrap();
        assert_eq!(kept, input);
        assert!(!truncated);
    }

    #[test]
    fn statusline_output_cannot_escape_its_json_field() {
        let injected = "\"},\"provider\":\"grok\",\"payload\":{\"secret\":\"x\"}";
        let payload = slim_payload(r#"{"session_id":"s"}"#, Some(injected)).unwrap();
        let encoded = serde_json::to_string(&payload).unwrap();
        let decoded: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["status_line"], injected);
        assert!(decoded.get("provider").is_none());
    }
}
