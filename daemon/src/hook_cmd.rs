//! `agents-managerd hook claude|codex` — the hook child process (SPEC §4.4).
//!
//! Contract (agreed interface — do not change the signatures):
//!   - synchronous, never panics, never writes to stdout, caller exits 0 afterwards.
//!   - POST http://127.0.0.1:<port>/hook/<provider> with header `X-AM-Bot-Token`
//!     and body {bot_id, provider, payload, received_at, truncated}.
//!   - on failure append that same body as one JSON line to
//!     ~/.config/agents-manager/bots/<bot_id>/hook-spool.jsonl
//!
//! Timing: this process sits in the agent's critical path (Claude blocks on the Stop
//! hook), so the whole thing is budgeted at well under the 3 s wall-clock ceiling:
//! stdin read ≤ 800 ms, HTTP connect ≤ 300 ms, HTTP total ≤ min(2 s, remaining budget).
//!
//! stdout is sacred: Claude parses a Stop hook's stdout as a *decision* object, so a
//! stray `println!` here can make the agent loop or abort. Errors go to stderr (one
//! short line) and to `hook.log`; never to stdout.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Total wall-clock budget. The hard contract is 3 s; we aim well under it so that
/// process startup + teardown still fit.
const TOTAL_BUDGET: Duration = Duration::from_millis(2_500);
/// stdin is closed by Claude right after it writes the payload; the cap only protects
/// us from a caller that keeps the pipe open.
const STDIN_BUDGET: Duration = Duration::from_millis(800);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const HTTP_TIMEOUT: Duration = Duration::from_millis(2_000);
/// SPEC §4.4.2 — 1 MiB stdin cap.
const MAX_PAYLOAD: usize = 1024 * 1024;

pub struct HookArgs {
    /// "claude" | "codex"
    pub provider: String,
    pub bot: String,
    pub token: String,
    pub port: u16,
    /// codex: the last argv JSON string; claude: None (payload comes from stdin)
    pub payload_arg: Option<String>,
}

/// Entry point. Never panics, never touches stdout. The caller exits 0 unconditionally.
pub fn run(args: HookArgs) {
    // A panic would print a multi-line backtrace-ish message to stderr; keep it to one line.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(std::io::stderr(), "agents-managerd hook: panic: {info}");
    }));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner(args)));
    std::panic::set_hook(prev);
    if r.is_err() {
        // Nothing more we can do — the caller still exits 0.
    }
}

fn inner(args: HookArgs) {
    let deadline = Instant::now() + TOTAL_BUDGET;

    let (text, truncated) = match args.payload_arg {
        // Codex: argv's last element is the JSON payload.
        Some(s) => {
            if s.len() > MAX_PAYLOAD {
                (truncate_utf8(&s, MAX_PAYLOAD).to_string(), true)
            } else {
                (s, false)
            }
        }
        // Claude: the payload arrives on stdin.
        None => read_stdin_capped(STDIN_BUDGET),
    };

    let payload = parse_payload(&text);
    let body = serde_json::json!({
        "bot_id": args.bot,
        "provider": args.provider,
        "payload": payload,
        "received_at": now_rfc3339(),
        "truncated": truncated,
    });

    let port = if args.port != 0 {
        args.port
    } else {
        // SPEC §4.4.5: env is only a fallback for a missing --port.
        std::env::var("AM_PORT").ok().and_then(|s| s.parse::<u16>().ok()).unwrap_or(7788)
    };

    match post(&body, &args.provider, &args.token, port, deadline) {
        Ok(()) => {}
        Err(e) => {
            log_line(&args.bot, &format!("post failed: {e}"));
            let _ = writeln!(std::io::stderr(), "agents-managerd hook: {e}; spooled");
            spool(&args.bot, &body);
        }
    }
}

/// Valid JSON *object* → used as-is. Anything else (invalid JSON, a bare scalar, an
/// array, a truncated blob) → `{"raw": "<text>"}` so the daemon still sees something.
fn parse_payload(text: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::json!({ "raw": text }),
    }
}

/// Read stdin up to `MAX_PAYLOAD`, giving up after `budget`.
///
/// The read runs on a helper thread so a caller that never closes the pipe cannot
/// blow the wall-clock contract; on timeout we return what the contract allows
/// (empty payload) and let the daemon classify it as a no-op.
fn read_stdin_capped(budget: Duration) -> (String, bool) {
    let (tx, rx) = std::sync::mpsc::channel::<(String, bool)>();
    std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(8192);
        // Read one byte past the cap so we can tell "exactly 1 MiB" from "truncated".
        let mut handle = std::io::stdin().lock().take((MAX_PAYLOAD + 1) as u64);
        let _ = handle.read_to_end(&mut buf);
        let truncated = buf.len() > MAX_PAYLOAD;
        if truncated {
            buf.truncate(MAX_PAYLOAD);
        }
        let s = String::from_utf8_lossy(&buf).into_owned();
        let s = if truncated { truncate_utf8(&s, MAX_PAYLOAD).to_string() } else { s };
        let _ = tx.send((s, truncated));
    });
    rx.recv_timeout(budget).unwrap_or_else(|_| (String::new(), false))
}

/// Truncate at a char boundary at or below `max` bytes.
fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// POST to the daemon. Hard-wired to IPv4 loopback and proxy-free (SPEC §4.4.3):
/// a user's `HTTP_PROXY` / `ALL_PROXY` must never intercept a local hook.
fn post(
    body: &serde_json::Value,
    provider: &str,
    token: &str,
    port: u16,
    deadline: Instant,
) -> Result<(), String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("out of time budget".into());
    }
    let total = HTTP_TIMEOUT.min(remaining);

    // reqwest is async-only in this build (no `blocking` feature), so we drive one
    // request on a single-threaded runtime and block here.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;

    let url = format!("http://127.0.0.1:{port}/hook/{provider}");
    let body = body.clone();
    let token = token.to_string();

    rt.block_on(async move {
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(total)
            .build()
            .map_err(|e| format!("client: {e}"))?;

        let resp = tokio::time::timeout(
            total,
            client.post(&url).header("X-AM-Bot-Token", token).json(&body).send(),
        )
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| format!("request: {e}"))?;

        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("http {}", status.as_u16()))
        }
    })
}

fn data_dir() -> PathBuf {
    // `AM_DATA_DIR` exists so the timing tests can run against a throwaway directory.
    if let Some(d) = std::env::var_os("AM_DATA_DIR") {
        return PathBuf::from(d);
    }
    match dirs::home_dir() {
        Some(h) => h.join(".config").join("agents-manager"),
        None => PathBuf::from(".agents-manager"),
    }
}

fn bot_dir(bot_id: &str) -> PathBuf {
    data_dir().join("bots").join(bot_id)
}

/// SPEC §4.4.4: one JSON line, `O_APPEND`, same body as the failed POST.
fn spool(bot_id: &str, body: &serde_json::Value) {
    let dir = bot_dir(bot_id);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log_line(bot_id, &format!("spool mkdir failed: {e}"));
        return;
    }
    let path = dir.join("hook-spool.jsonl");
    let line = match serde_json::to_string(body) {
        Ok(s) => s,
        Err(e) => {
            log_line(bot_id, &format!("spool encode failed: {e}"));
            return;
        }
    };
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(format!("{line}\n").as_bytes()) {
                log_line(bot_id, &format!("spool write failed: {e}"));
            }
        }
        Err(e) => log_line(bot_id, &format!("spool open failed: {e}")),
    }
}

/// Last resort: `hook.log`. If this fails too we stay silent — exit 0 is the contract.
fn log_line(bot_id: &str, msg: &str) {
    let dir = bot_dir(bot_id);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("hook.log")) {
        let _ = f.write_all(format!("{} {}\n", now_rfc3339(), msg).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_payload_passes_through() {
        let v = parse_payload(r#"{"a":1}"#);
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn non_object_json_becomes_raw() {
        assert_eq!(parse_payload("[1,2]")["raw"], "[1,2]");
        assert_eq!(parse_payload("not json")["raw"], "not json");
        assert_eq!(parse_payload("")["raw"], "");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let s = "aa\u{4f60}\u{597d}"; // 2 + 3 + 3 bytes
        assert_eq!(truncate_utf8(s, 4), "aa");
        assert_eq!(truncate_utf8(s, 5), "aa\u{4f60}");
        assert_eq!(truncate_utf8(s, 99), s);
    }
}
