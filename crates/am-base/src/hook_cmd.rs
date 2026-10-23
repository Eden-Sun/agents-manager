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

/// POST 失敗的兩種：daemon 回了非 2xx（分得出狀態碼），或根本沒送到（連不上、逾時、建 client 失敗）。
enum PostError {
    Rejected(u16),
    Failed(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::Rejected(code) => write!(f, "http {code}"),
            PostError::Failed(msg) => f.write_str(msg),
        }
    }
}

/// daemon 明確、永久的拒絕：重放也一樣被丟，不進 spool。410＝bot 已刪（spool 還會把清掉的 bots/<id>/ 建回來）、
/// 409＝provider 不是這顆 bot 的 kind。401（token 不對／讀不到 bot）與 5xx 照舊 spool：那可能是暫時的。
pub fn worth_spooling(status: u16) -> bool {
    !matches!(status, 409 | 410)
}

/// 子 agent 的 pane（herdr shim 蓋的 `AM_CHILD_OF`）繼承的是母 bot 的 `AM_BOT_ID`／`AM_HOOK_TOKEN`／`AM_RUN_ID`：
/// 這裡送出去的 hook 會被記在母 bot 身上（同 kind 時收掉母 bot 的回合、改掉它的 session）。子 agent 沒有自己的 hook 身份
/// （§12.5／§12a.9 改讀對話檔），所以一律不送。
pub fn is_child_pane(child_of: Option<std::ffi::OsString>) -> bool {
    child_of.is_some_and(|v| !v.to_string_lossy().trim().is_empty())
}

fn inner(args: HookArgs) {
    let child = is_child_pane(std::env::var_os("AM_CHILD_OF"));
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

    // PreToolUse 是守門、不是事件（#1884）：擋廣域 kill，**不** POST、不 spool；只有要擋時才印 stdout（決策 JSON）。
    // 子 agent 的 pane 也要擋（事故就是子 bot 跑的），所以放在下面那個 child 判斷之前；解析不了一律放行。
    // 「判不出來所以放行」的原因除了 stderr（agy 的 dispatcher 會丟掉它）也記進 hook.log。
    let mut guard_log = |m: &str| {
        let _ = writeln!(std::io::stderr(), "agents-managerd hook: kill_guard: {m}");
        log_line_in(&data_dir, &args.bot, &format!("kill_guard: {m}"));
    };
    match crate::kill_guard::guard_logged(&args.provider, &args.event, &text, &mut guard_log) {
        crate::kill_guard::Guard::NotPreToolUse => {}
        crate::kill_guard::Guard::Pass => return,
        crate::kill_guard::Guard::Deny(decision) => {
            let _ = writeln!(std::io::stdout(), "{decision}");
            let _ = writeln!(std::io::stderr(), "agents-managerd hook: PreToolUse denied a broad kill");
            return;
        }
    }
    // 子 agent 的 hook 不送、也不 spool（#1004）：stdout 照舊空的。
    if child {
        return;
    }

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
        // 永久拒絕：重放也一定被丟，spool 只會把已刪 bot 的目錄建回來（#1005）。不寫 hook.log、不 spool。
        Err(PostError::Rejected(code)) if !worth_spooling(code) => {
            let _ = writeln!(std::io::stderr(), "agents-managerd hook: http {code}; dropped");
        }
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
pub fn hook_body(
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
pub fn enrich_agy_payload(payload: &mut serde_json::Value, event: &str) {
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
pub fn parse_payload(text: &str) -> serde_json::Value {
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

pub fn truncate_utf8(s: &str, max: usize) -> &str {
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
) -> Result<(), PostError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(PostError::Failed("out of time budget".into()));
    }
    let total = HTTP_TIMEOUT.min(remaining);

    // reqwest has no `blocking` feature in this build.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PostError::Failed(format!("runtime: {e}")))?;

    let url = format!("http://127.0.0.1:{port}/hook/{provider}");
    let body = body.clone();
    let token = token.to_string();

    rt.block_on(async move {
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(total)
            .build()
            .map_err(|e| PostError::Failed(format!("client: {e}")))?;

        let resp = tokio::time::timeout(
            total,
            client.post(&url).header("X-AM-Bot-Token", token).json(&body).send(),
        )
        .await
        .map_err(|_| PostError::Failed("timeout".to_string()))?
        .map_err(|e| PostError::Failed(format!("request: {e}")))?;

        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(PostError::Rejected(status.as_u16()))
        }
    })
}

/// spool 必須落在**啟動這顆 bot 的 daemon 的**資料目錄，否則隔離跑的 daemon 不會重播、
/// 正式 daemon 反而吃到它。順序：argv 的 `--data-dir`（daemon 寫死在 hook.sh 裡）> `AM_DATA_DIR`
/// （pane env）> 預設目錄。argv 優先是因為舊 pane 的 env 換不掉，但 hook.sh 每次啟動都重寫。
pub fn resolve_data_dir(arg: &str) -> PathBuf {
    if !arg.trim().is_empty() {
        return PathBuf::from(arg);
    }
    data_dir_from(std::env::var_os("AM_DATA_DIR"))
}

pub fn data_dir_from(env: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(d) = env.filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    match dirs::home_dir() {
        Some(h) => h.join(".config").join("agents-manager"),
        None => PathBuf::from(".agents-manager"),
    }
}

/// spool 總量上限（#1256）：一顆 bot 的 spool 目錄（`.replaying`、`.claim`、`hook-spool.jsonl` 加起來）不超過這個數。
/// 超過時從最舊的檔丟起；只剩最新那個檔還超過，就從它的最前面截掉（保留最新的一半，行邊界對齊）。
/// 單行上限在 daemon 那一側（`MAX_SPOOL_LINE`），這裡不管。
pub const SPOOL_CAP_BYTES: u64 = 64 * 1024 * 1024;

const SPOOL_NAME: &str = "hook-spool.jsonl";
const SPOOL_CLAIM: &str = "hook-spool.jsonl.claim";
const SPOOL_REPLAYING: &str = "hook-spool.jsonl.replaying";
/// 累計丟掉的紀錄數（十進位文字，一行）。daemon 重啟不歸零。
const SPOOL_DROPPED: &str = "hook-spool.dropped";

/// 一顆 bot 的 spool 目錄的排他鎖（`hook-spool.lock`，#1256）。hook 行程的寫入（含超量丟棄）與 daemon 的認領（rename）
/// 都要拿：不然丟棄、截斷會跟附加、認領撞在一起。放掉就是關掉檔案。
pub struct SpoolDirLock(std::fs::File);

pub fn lock_spool_dir(dir: &std::path::Path) -> std::io::Result<SpoolDirLock> {
    let f = crate::private_files::append_private(&dir.join("hook-spool.lock"))?;
    f.lock()?;
    Ok(SpoolDirLock(f))
}

/// SPEC §4.4.4: one JSON line, `O_APPEND`, same body as the failed POST.
pub fn spool_to(data_dir: &std::path::Path, bot_id: &str, body: &serde_json::Value) {
    let dir = data_dir.join("bots").join(bot_id);
    // 目錄 0700、spool 0600：裡面是完整的 hook payload（issue #494）。
    if let Err(e) = crate::private_files::create_private_dir(&dir) {
        log_line_in(data_dir, bot_id, &format!("spool mkdir failed: {e}"));
        return;
    }
    let path = dir.join(SPOOL_NAME);
    let line = match serde_json::to_string(body) {
        Ok(s) => s,
        Err(e) => {
            log_line_in(data_dir, bot_id, &format!("spool encode failed: {e}"));
            return;
        }
    };
    // #1256：超量檢查、丟棄、附加整段都在鎖裡做。鎖拿不到就照舊附加——寧可超一點，也不丟事件。
    let _lock = match lock_spool_dir(&dir) {
        Ok(l) => Some(l),
        Err(e) => {
            log_line_in(data_dir, bot_id, &format!("spool lock failed: {e}"));
            None
        }
    };
    enforce_spool_cap(data_dir, bot_id, &dir, line.len() as u64 + 1);
    match crate::private_files::append_private(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(format!("{line}\n").as_bytes()) {
                log_line_in(data_dir, bot_id, &format!("spool write failed: {e}"));
            }
        }
        Err(e) => log_line_in(data_dir, bot_id, &format!("spool open failed: {e}")),
    }
}

/// 超量就丟（#1256）。必須在 [`lock_spool_dir`] 之內呼叫。先整個丟最舊的檔，還超過再從最新檔的前面截。
fn enforce_spool_cap(data_dir: &std::path::Path, bot_id: &str, dir: &std::path::Path, incoming: u64) {
    let size = |name: &str| std::fs::metadata(dir.join(name)).map(|m| m.len()).unwrap_or(0);
    let mut total = size(SPOOL_REPLAYING) + size(SPOOL_CLAIM) + size(SPOOL_NAME);
    for name in [SPOOL_REPLAYING, SPOOL_CLAIM] {
        if total + incoming <= SPOOL_CAP_BYTES {
            return;
        }
        let bytes = size(name);
        if bytes == 0 {
            continue;
        }
        let path = dir.join(name);
        let records = records_in(&path, 0, bytes).unwrap_or(0);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                total -= bytes;
                record_drop(data_dir, bot_id, dir, name, bytes, records);
            }
            Err(e) => log_line_in(data_dir, bot_id, &format!("spool over cap; dropping {name} failed: {e}")),
        }
    }
    if total + incoming <= SPOOL_CAP_BYTES {
        return;
    }
    match trim_front(dir, SPOOL_CAP_BYTES / 2) {
        Ok((0, _)) => {}
        Ok((bytes, records)) => record_drop(data_dir, bot_id, dir, SPOOL_NAME, bytes, records),
        Err(e) => log_line_in(data_dir, bot_id, &format!("spool over cap; trimming {SPOOL_NAME} failed: {e}")),
    }
}

/// 只留最新的 `keep` 位元組（從行邊界開始）。回傳丟掉的位元組數與紀錄數。寫到暫存檔再 rename，寫到一半崩掉留下的還是完整的舊檔。
fn trim_front(dir: &std::path::Path, keep: u64) -> std::io::Result<(u64, u64)> {
    use std::io::{Seek, SeekFrom};
    let active = dir.join(SPOOL_NAME);
    let mut src = std::fs::File::open(&active)?;
    let len = src.metadata()?.len();
    if len <= keep {
        return Ok((0, 0));
    }
    // 從 `len - keep` 往後找第一個換行，截點就在它後面：不留半行。尾巴裡一個換行都沒有就全丟。
    let mut at = len - keep;
    src.seek(SeekFrom::Start(at))?;
    let mut buf = vec![0u8; 64 * 1024];
    let cut = loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break len;
        }
        if let Some(i) = buf[..n].iter().position(|&b| b == b'\n') {
            break at + i as u64 + 1;
        }
        at += n as u64;
    };
    let records = records_in(&active, 0, cut)?;
    let tmp = dir.join("hook-spool.jsonl.trim");
    let mut out = crate::private_files::create_private_truncate(&tmp)?;
    src.seek(SeekFrom::Start(cut))?;
    std::io::copy(&mut (&mut src).take(len - cut), &mut out)?;
    out.sync_all()?;
    std::fs::rename(&tmp, &active)?;
    Ok((cut, records))
}

/// `[from, to)` 裡的紀錄數：每個換行算一筆，結尾沒有換行的半行也算一筆。
fn records_in(path: &std::path::Path, from: u64, to: u64) -> std::io::Result<u64> {
    use std::io::{Seek, SeekFrom};
    if to <= from {
        return Ok(0);
    }
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut left = to - from;
    let mut buf = vec![0u8; 64 * 1024];
    let (mut n_records, mut last) = (0u64, b'\n');
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        n_records += buf[..n].iter().filter(|&&b| b == b'\n').count() as u64;
        last = buf[n - 1];
        left -= n as u64;
    }
    if last != b'\n' {
        n_records += 1;
    }
    Ok(n_records)
}

/// 每次丟棄記一行 warn 到 bot 的 `hook.log`，並把丟掉的紀錄數累加到 `hook-spool.dropped`（#1256）。
fn record_drop(data_dir: &std::path::Path, bot_id: &str, dir: &std::path::Path, what: &str, bytes: u64, records: u64) {
    let total = bump_dropped(dir, records);
    log_line_in(
        data_dir,
        bot_id,
        &format!("warn spool over cap: dropped {what} ({bytes} bytes, {records} records); dropped records so far: {total}"),
    );
}

fn bump_dropped(dir: &std::path::Path, add: u64) -> u64 {
    let path = dir.join(SPOOL_DROPPED);
    let before: u64 = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let total = before.saturating_add(add);
    if let Ok(mut f) = crate::private_files::create_private_truncate(&path) {
        let _ = f.write_all(format!("{total}\n").as_bytes());
    }
    total
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
