//! `agents-managerd hook claude|codex|grok|agy` — the hook child process (SPEC §4.4, §12).
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
    /// 這顆 hook 屬於哪一顆 daemon 的資料目錄。空字串＝回頭看 `AM_DATA_DIR`，再沒有才是預設目錄。
    /// 寫死在 argv 裡：pane env 只保護「這顆 daemon 新開的 pane」，舊 pane 沒有新 env（sol 複審二輪）。
    pub data_dir: String,
    /// agy：事件名（`SessionStart`／`PreInvocation`／`Stop`／`state`），payload 自己不帶。其他 provider 忽略。
    pub event: String,
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
    let data_dir = resolve_data_dir(&args.data_dir);

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

    let mut payload = parse_payload(&text);
    if args.provider == "agy" {
        enrich_agy_payload(&mut payload, &args.event);
    }
    let run_id = std::env::var("AM_RUN_ID").ok();
    let body = hook_body(&args.bot, &args.provider, payload, &now_rfc3339(), truncated, run_id.as_deref());

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
            log_line_in(&data_dir, &args.bot, &format!("post failed: {e}"));
            let _ = writeln!(std::io::stderr(), "agents-managerd hook: {e}; spooled");
            spool_to(&data_dir, &args.bot, &body);
        }
    }
}

/// 送給 daemon 的那一份 body（POST 與 spool 同一份）。
///
/// `run_id` 是這個 CLI 行程啟動時 pane env 的 `AM_RUN_ID`：`--resume` 接回同一段對話時，新舊兩個行程
/// 回報的是**同一個** session id，只看 session 分不出一則遲到的 hook 是哪個行程送的（issue #92）。
/// 沒有值就不帶這個鍵——daemon 照舊只看 session（舊行程、手動跑的 hook）。
fn hook_body(
    bot: &str,
    provider: &str,
    payload: serde_json::Value,
    received_at: &str,
    truncated: bool,
    run_id: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "bot_id": bot,
        "provider": provider,
        "payload": payload,
        "received_at": received_at,
        "truncated": truncated,
    });
    if let Some(rid) = run_id.map(str::trim).filter(|r| !r.is_empty()) {
        body["run_id"] = serde_json::json!(rid);
    }
    body
}

/// agy 的 payload 沒有事件名（`hooks.json` 的指令參數才有），`Stop` 也沒有助理回覆文字——回覆在 `transcriptPath`（`transcript_full.jsonl`）。
/// hook 子行程跟 transcript 在同一台機器上，所以這裡讀（只讀尾端、同步、很小），把事件名與最近一回合的問答放進 payload；
/// 讀不到就不放，daemon 照樣收得到事件（回合照收，只是沒有回覆文字）。其他事件只補事件名。
fn enrich_agy_payload(payload: &mut serde_json::Value, event: &str) {
    const TAIL_BYTES: u64 = 2 * 1024 * 1024;
    let Some(obj) = payload.as_object_mut() else { return };
    let event = event.trim();
    if !event.is_empty() {
        obj.insert("hookEventName".into(), serde_json::json!(event));
    }
    if event != "Stop" {
        return;
    }
    let Some(path) = obj.get("transcriptPath").and_then(|v| v.as_str()).filter(|p| p.ends_with(".jsonl")) else { return };
    let Ok(tail) = crate::agy_support::read_tail(std::path::Path::new(path), TAIL_BYTES) else { return };
    if let Some(n) = crate::agy_support::last_input_tokens(&tail) {
        obj.insert("lastInputTokens".into(), serde_json::json!(n));
    }
    let ex = crate::agy_support::last_exchange(&tail);
    if let Some(a) = ex.assistant {
        obj.insert("lastAssistantMessage".into(), serde_json::json!(a));
    }
    if let Some(u) = ex.user {
        obj.insert("lastUserMessage".into(), serde_json::json!(u));
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

/// spool 必須落在**啟動這顆 bot 的 daemon 的**資料目錄，否則隔離跑的 daemon 不會重播、
/// 正式 daemon 反而吃到它。順序：argv 的 `--data-dir`（daemon 寫死在 hook.sh 裡）> `AM_DATA_DIR`
/// （pane env）> 預設目錄。argv 優先是因為舊 pane 的 env 換不掉，但 hook.sh 每次啟動都重寫。
fn resolve_data_dir(arg: &str) -> PathBuf {
    if !arg.trim().is_empty() {
        return PathBuf::from(arg);
    }
    data_dir_from(std::env::var_os("AM_DATA_DIR"))
}

fn data_dir_from(env: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(d) = env.filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    match dirs::home_dir() {
        Some(h) => h.join(".config").join("agents-manager"),
        None => PathBuf::from(".agents-manager"),
    }
}

/// SPEC §4.4.4: one JSON line, `O_APPEND`, same body as the failed POST.
fn spool_to(data_dir: &std::path::Path, bot_id: &str, body: &serde_json::Value) {
    let dir = data_dir.join("bots").join(bot_id);
    // 目錄 0700、spool 0600：裡面是完整的 hook payload（issue #494）。
    if let Err(e) = crate::private_files::create_private_dir(&dir) {
        log_line_in(data_dir, bot_id, &format!("spool mkdir failed: {e}"));
        return;
    }
    let path = dir.join("hook-spool.jsonl");
    let line = match serde_json::to_string(body) {
        Ok(s) => s,
        Err(e) => {
            log_line_in(data_dir, bot_id, &format!("spool encode failed: {e}"));
            return;
        }
    };
    match crate::private_files::append_private(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(format!("{line}\n").as_bytes()) {
                log_line_in(data_dir, bot_id, &format!("spool write failed: {e}"));
            }
        }
        Err(e) => log_line_in(data_dir, bot_id, &format!("spool open failed: {e}")),
    }
}

/// Last resort; if this fails too we stay silent.
fn log_line_in(data_dir: &std::path::Path, bot_id: &str, msg: &str) {
    let dir = data_dir.join("bots").join(bot_id);
    if crate::private_files::create_private_dir(&dir).is_err() {
        return;
    }
    if let Ok(mut f) = crate::private_files::append_private(&dir.join("hook.log")) {
        let _ = f.write_all(format!("{} {}\n", now_rfc3339(), msg).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 隔離跑的 daemon 會把自己的資料目錄注入 pane env；spool 一定要落在那裡，
    /// 否則隔離的 daemon 不會重播、正式 daemon 反而吃到它（2026-09-14 事故）。
    #[test]
    fn the_spool_follows_the_injected_data_dir() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-hook-spool-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir);
        spool_to(&dir, "b1", &serde_json::json!({"event": "Stop"}));

        let line = std::fs::read_to_string(dir.join("bots/b1/hook-spool.jsonl")).unwrap();
        assert!(line.contains("\"event\":\"Stop\""), "{line}");
        let home_spool = data_dir_from(None).join("bots/b1/hook-spool.jsonl");
        assert!(!home_spool.starts_with(&dir), "沒注入時才回到預設目錄");

        assert_eq!(data_dir_from(Some("/tmp/am-iso".into())), PathBuf::from("/tmp/am-iso"));
        assert_eq!(data_dir_from(Some("".into())), data_dir_from(None), "空字串當沒設");

        // argv 贏過 env：daemon 重啟前就開著的 pane 換不掉 env，但 hook.sh 每次啟動都重寫。
        assert_eq!(resolve_data_dir("/tmp/am-iso"), PathBuf::from("/tmp/am-iso"));
        assert_eq!(resolve_data_dir("  "), data_dir_from(std::env::var_os("AM_DATA_DIR")), "沒給才看 env");
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// issue #92：body 帶上這個行程自己的 run id，daemon 才分得出「同一個 session、不同行程」的遲到 hook。
    /// 沒有值（舊 pane、手動跑）就整個不帶，不送一個空字串進去。
    #[test]
    fn the_body_names_the_run_its_process_was_started_for() {
        let with = hook_body("b1", "claude", serde_json::json!({"a": 1}), "2026-09-18T00:00:00.000Z", false, Some("01RUN"));
        assert_eq!(with["run_id"], "01RUN");
        assert_eq!((with["bot_id"].as_str(), with["payload"]["a"].as_i64()), (Some("b1"), Some(1)));
        let parsed: crate::hookrecv::HookBody = serde_json::from_value(with).unwrap();
        assert_eq!(parsed.run_id.as_deref(), Some("01RUN"), "daemon 那一側讀得回來");
        for none in [None, Some(""), Some("  ")] {
            let without = hook_body("b1", "claude", serde_json::json!({}), "t", false, none);
            assert!(without.get("run_id").is_none(), "{none:?} → {without}");
        }
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

    /// agy：payload 沒有事件名、`Stop` 沒有回覆文字——hook 子行程補事件名，並從 transcript 讀最近一回合的問答放進 payload。
    #[test]
    fn an_agy_stop_gets_its_event_name_and_the_reply_read_from_the_transcript() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-hook-agy-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("transcript_full.jsonl");
        std::fs::write(
            &t,
            concat!(
                r#"{"step_index":0,"type":"USER_INPUT","content":"<USER_REQUEST>\nsay OK\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"}"#, "\n",
                r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"OK","input_tokens":11824}"#, "\n",
            ),
        )
        .unwrap();
        let mut p = serde_json::json!({"conversationId": "c-1", "transcriptPath": t, "fullyIdle": true});
        enrich_agy_payload(&mut p, "Stop");
        assert_eq!(p["hookEventName"], "Stop");
        assert_eq!(p["lastAssistantMessage"], "OK");
        assert_eq!(p["lastUserMessage"], "say OK");
        assert_eq!(p["lastInputTokens"], 11824, "context 大小＝最後一筆回覆讀進去的 token 數");

        // 其他事件只補事件名，不碰 transcript。
        let mut p = serde_json::json!({"conversationId": "c-1", "transcriptPath": t});
        enrich_agy_payload(&mut p, "PreInvocation");
        assert_eq!(p["hookEventName"], "PreInvocation");
        assert!(p.get("lastAssistantMessage").is_none());

        // 讀不到（路徑是占位、不是 .jsonl）：事件照樣送，只是沒有回覆文字。
        let mut p = serde_json::json!({"transcriptPath": "/no/such/transcript_full.jsonl"});
        enrich_agy_payload(&mut p, "Stop");
        assert_eq!(p["hookEventName"], "Stop");
        assert!(p.get("lastAssistantMessage").is_none());
        let mut p = serde_json::json!({"transcriptPath": "/etc/passwd"});
        enrich_agy_payload(&mut p, "Stop");
        assert!(p.get("lastAssistantMessage").is_none(), "只讀 .jsonl");
        // 非物件 payload（`{"raw": …}` 以外的怪東西）不會 panic。
        let mut p = serde_json::json!([1, 2]);
        enrich_agy_payload(&mut p, "Stop");
    }
}
