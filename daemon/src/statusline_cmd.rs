//! `agents-managerd statusline --bot <id> --token <t> --port <p>` — Claude Code's statusLine
//! command for daemon-started claude bots (v4.0).
//!
//! Claude Code pipes a JSON object (`rate_limits`, `model`, `context_window`, `session_id`,
//! `cost`, `workspace`, …) to the statusLine command on every refresh. This process
//!   1. POSTs a slim copy (`hook_event_name = "StatusLine"`, no transcript-ish fields) to
//!      `/hook/claude` so the daemon can track the 5 h / 7 d quota, fire-and-forget (never
//!      spooled — the next refresh brings fresher numbers anyway), and
//!   2. runs the user's *own* statusLine command (from `$CLAUDE_CONFIG_DIR/settings.json`,
//!      default `~/.claude/settings.json`) on the same input and relays its stdout verbatim,
//!      so the pane's status bar looks exactly as it would without the daemon. No command
//!      configured → empty output.
//!
//! Budget ≤ 2 s; never exits non-zero; never panics past `run`.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TOTAL_BUDGET: Duration = Duration::from_millis(1_900);
const STDIN_BUDGET: Duration = Duration::from_millis(600);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_PAYLOAD: usize = 1024 * 1024;

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
    let input = read_stdin_capped(STDIN_BUDGET);

    // 1. report the quota (background thread; we do not wait for it past the deadline).
    let poster = slim_payload(&input).map(|payload| {
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
        let token = args.token.clone();
        std::thread::spawn(move || post(&body, &token, port, deadline))
    });

    // 2. the user's own status line, on the same input.
    if let Some(cmd) = user_statusline_command() {
        relay_user_command(&cmd, &input, deadline);
    }

    // Give the POST the rest of the budget, then leave regardless.
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

/// The fields the daemon cares about, plus the event name. `None` when stdin was not a JSON
/// object (nothing to report).
pub fn slim_payload(input: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    let o = v.as_object()?;
    let mut out = serde_json::Map::new();
    out.insert("hook_event_name".into(), serde_json::json!("StatusLine"));
    for k in ["session_id", "rate_limits", "model", "context_window", "version", "cwd"] {
        if let Some(x) = o.get(k) {
            out.insert(k.into(), x.clone());
        }
    }
    Some(serde_json::Value::Object(out))
}

/// `statusLine.command` from the user's settings.json (`type` must be `command` or absent).
/// Refuses to recurse into ourselves.
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
    if cmd.is_empty() || cmd.contains("agents-managerd statusline") || cmd.contains("hook.sh statusline") {
        return None;
    }
    Some(cmd)
}

/// Run `sh -c <cmd>` with `input` on stdin and copy its stdout to ours. Killed at the deadline.
fn relay_user_command(cmd: &str, input: &str, deadline: Instant) {
    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "agents-managerd statusline: spawn user command: {e}");
            return;
        }
    };
    if let Some(mut sin) = child.stdin.take() {
        let data = input.to_string();
        std::thread::spawn(move || {
            let _ = sin.write_all(data.as_bytes());
        });
    }
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(buf) => {
            let _ = child.wait();
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(&buf);
            let _ = out.flush();
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_stdin_capped(budget: Duration) -> String {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(8192);
        let mut handle = std::io::stdin().lock().take(MAX_PAYLOAD as u64);
        let _ = handle.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
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
        )
        .unwrap();
        assert_eq!(v["hook_event_name"], "StatusLine");
        assert_eq!(v["rate_limits"]["five_hour"]["used_percentage"], 3);
        assert!(v.get("transcript_path").is_none());
        assert!(v.get("cost").is_none());
        assert!(slim_payload("not json").is_none());
        assert!(slim_payload("[1]").is_none());
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
}
