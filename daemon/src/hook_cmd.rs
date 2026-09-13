//! `agents-managerd hook claude|codex|grok` — the hook child process (SPEC §4.4, §12).
//!
//! Contract (do not change the signatures): synchronous, never panics, caller exits 0; POST
//! with `X-AM-Bot-Token`, spool the same body to `bots/<bot_id>/hook-spool.jsonl` on failure.
//! Claude blocks on the Stop hook, so everything fits well under 3 s.
//!
//! stdout is sacred: Claude parses a Stop hook's stdout as a *decision* object, so a stray
//! `println!` can make the agent loop or abort. Errors go to stderr and `hook.log` only.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Hard contract is 3 s; leave room for process startup + teardown.
const TOTAL_BUDGET: Duration = Duration::from_millis(2_500);
/// Only guards against a caller that keeps the pipe open.
const STDIN_BUDGET: Duration = Duration::from_millis(800);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const HTTP_TIMEOUT: Duration = Duration::from_millis(2_000);
/// SPEC §4.4.2 — 1 MiB stdin cap.
const MAX_PAYLOAD: usize = 1024 * 1024;

pub struct HookArgs {
    pub provider: String,
    pub bot: String,
    pub token: String,
    pub port: u16,
    /// codex: the last argv JSON string; claude / grok: None (payload comes from stdin)
    pub payload_arg: Option<String>,
}

/// `--token`, else `$AM_HOOK_TOKEN` — kept off the command line where `ps` exposes it (issue #43).
pub fn hook_token(arg: &str) -> String {
    if !arg.is_empty() {
        arg.to_string()
    } else {
        std::env::var("AM_HOOK_TOKEN").unwrap_or_default()
    }
}

/// Never panics, never touches stdout.
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
        Some(s) => {
            if s.len() > MAX_PAYLOAD {
                (truncate_utf8(&s, MAX_PAYLOAD).to_string(), true)
            } else {
                (s, false)
            }
        }
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

    let token = hook_token(&args.token);
    match post(&body, &args.provider, &token, port, deadline) {
        Ok(()) => {}
        Err(e) => {
            log_line(&args.bot, &format!("post failed: {e}"));
            let _ = writeln!(std::io::stderr(), "agents-managerd hook: {e}; spooled");
            spool(&args.bot, &body);
        }
    }
}

/// Non-object → `{"raw": "<text>"}` so the daemon still sees something.
fn parse_payload(text: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::json!({ "raw": text }),
    }
}

/// Helper thread so a never-closed pipe cannot blow the budget; timeout → empty payload.
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

/// IPv4 loopback, proxy-free (SPEC §4.4.3): `HTTP_PROXY` must never intercept a local hook.
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

    // reqwest has no `blocking` feature in this build.
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
    // For tests.
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

/// Last resort; if this fails too we stay silent.
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
    fn explicit_token_wins_over_env() {
        assert_eq!(hook_token("cli"), "cli");
    }

    #[test]
    fn empty_token_reads_env_or_stays_empty() {
        // Env-dependent, so only the empty branch is asserted deterministically.
        let from_env = std::env::var("AM_HOOK_TOKEN").unwrap_or_default();
        assert_eq!(hook_token(""), from_env);
    }

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
