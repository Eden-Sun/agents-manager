//! `run.prompt_cache`：輸入框上方「送出的代價」提示用的精簡欄位（SPEC §6.5j、UI-DECISIONS）。
//!
//! 只做 claude 與 codex（grok 不帶、不畫）：
//! * claude：statusLine 的 `prompt_cache`（≥ 2.1.289）與 `context_window`，從 `runs.status_json` 挑精簡欄位，**不外送原文**；
//!   `source:"statusline"`。沒有這塊（舊版 claude）＝`null`，網頁退回 `last_api_at`＋`cache_ttl_secs` 推算。
//! * codex：沒有 statusLine，改讀它的 rollout（`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*-<session>.jsonl`）最近一筆
//!   `event_msg`／`token_count` 的 `info`：`last_token_usage.input_tokens`（＝這次請求讀進去的整段 context）、
//!   `cached_input_tokens`、`model_context_window`（推算，`source:"rollout_estimate"`）。codex 的 cache 不因閒置冷掉
//!   （[`cache_clock::ttl_secs`] 為 `None`）：`warm` 一直是 `true`、`expires_at` 為 `null`。`recache_tokens_if_cold` 用 `input_tokens` 近似。
//!
//! 檔案只讀新增的部分：記住每個 run 的路徑與位移，下一輪只讀位移之後的整行；第一次（沒位移）只讀檔尾
//! [`TAIL_BYTES`]，檔尾裡沒有 `token_count` 才往前多找 [`BACKWARD_BYTES`]。herdr 開的 codex child 沒有 `native_session_id`，
//! 由巡邏改用 herdr 綁在 agent 上的 session 找 rollout。遠端 codex（host 不是本機）改經 host 通道增量讀，每 run [`REMOTE_EVERY`] 一次（[`refresh_remote`]）。
//! 記憶體帳，daemon 重啟後第一輪重讀檔尾即可補回；run 結束由巡邏清掉（[`retain_runs`]）。

use crate::db;
use am_ports::{CodexRolloutAccess, DbContext, EventSink, RemoteRollout};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 遠端 codex 的 rollout 最多每這麼久讀一次（每 run）；讀失敗也等到下一個間隔，不在每輪巡邏打 host。
pub const REMOTE_EVERY: Duration = Duration::from_secs(20);

/// 第一次（或一輪新增太多）時只讀檔尾這麼多；rollout 一輪 token_count 約幾 KB，256 KB 足夠找到最後一筆。
pub const TAIL_BYTES: u64 = 256 * 1024;

/// rollout 一筆 `token_count` 取用的欄位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodexUsage {
    /// 事件時間（epoch 毫秒）。
    pub at_ms: i64,
    pub input: i64,
    pub cached: i64,
    pub window: i64,
}

#[derive(Debug, Default)]
pub struct Entry {
    session_id: String,
    path: Option<PathBuf>,
    offset: u64,
    usage: Option<CodexUsage>,
    /// 遠端 run 上次打 host 的時間（[`REMOTE_EVERY`] 節流）。位移與本機共用 `offset`，只放記憶體。
    remote_at: Option<Instant>,
}

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn entry_for_test(session_id: Option<&str>, usage: Option<CodexUsage>) -> Entry {
    Entry { session_id: session_id.unwrap_or_default().to_string(), usage, ..Entry::default() }
}

/// 測試用：把遠端 run 上次讀取的時間往回撥 [`REMOTE_EVERY`]（等同過了節流間隔）。
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn age_remote_for_test(run: &str) {
    if let Some(e) = store().lock().unwrap_or_else(|e| e.into_inner()).get_mut(run) {
        e.remote_at = e.remote_at.map(|at| at - REMOTE_EVERY);
    }
}

/// 測試用：這個 run 目前的位移與 context 讀數（input tokens）。
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn remote_state_for_test(run: &str) -> Option<(u64, Option<i64>)> {
    store().lock().unwrap_or_else(|e| e.into_inner()).get(run).map(|e| (e.offset, e.usage.map(|u| u.input)))
}

pub fn store() -> &'static Mutex<HashMap<String, Entry>> {
    static S: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// 不在 `active` 裡的 run（結束了）不留記錄。
#[cfg_attr(test, allow(dead_code))]
pub fn retain_runs(active: &[String]) {
    store().lock().unwrap_or_else(|e| e.into_inner()).retain(|id, _| active.contains(id));
}

/// rollout 的一行；不是 `token_count`、`info` 是 null（開頭常見）、缺欄位都回 `None`。
pub fn parse_token_count(line: &str) -> Option<CodexUsage> {
    if !line.contains("\"token_count\"") {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "event_msg" || v.pointer("/payload/type")?.as_str()? != "token_count" {
        return None;
    }
    let info = v.pointer("/payload/info")?;
    let last = info.get("last_token_usage")?;
    let input = last.get("input_tokens")?.as_i64()?;
    let cached = last.get("cached_input_tokens").and_then(Value::as_i64).unwrap_or(0);
    let window = info.get("model_context_window")?.as_i64()?;
    let at_ms = db::parse_ts(v.get("timestamp")?.as_str()?)?.timestamp_millis();
    Some(CodexUsage { at_ms, input, cached, window })
}

/// 一段（整行）rollout 文字裡最後一筆 `token_count`。
pub fn last_token_count(text: &str) -> Option<CodexUsage> {
    text.lines().rev().find_map(parse_token_count)
}

/// 讀 `path` 從 `offset` 起新增的**整行**：回 `(文字, 新位移)`。
/// 檔案縮小（被截斷／換檔）從頭來；沒位移或新增超過 [`TAIL_BYTES`] 只取檔尾（從下一個換行起，不吃半行）；
/// 結尾還沒寫完的半行不讀、位移停在它前面。
pub fn read_new(path: &Path, offset: u64) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let offset = if len < offset { 0 } else { offset };
    if len == offset {
        return Ok((String::new(), offset));
    }
    let (start, mid_line) = if len - offset > TAIL_BYTES { (len - TAIL_BYTES, true) } else { (offset, false) };
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.take(len - start).read_to_end(&mut buf)?;
    let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n') else { return Ok((String::new(), offset)) };
    let end = last_nl + 1;
    let from = if mid_line { buf.iter().position(|b| *b == b'\n').map_or(end, |i| i + 1) } else { 0 };
    let text = String::from_utf8_lossy(&buf[from.min(end)..end]).into_owned();
    Ok((text, start + end as u64))
}

/// 往前找的上限：一輪工具輸出再長也很少超過這麼多；只在還沒有任何讀數時讀一次。
pub const BACKWARD_BYTES: u64 = 32 * 1024 * 1024;

/// `path` 在 `end` 之前（往前最多 [`BACKWARD_BYTES`]）最後一筆 `token_count`。讀不到回 `None`。
pub fn last_token_count_before(path: &Path, end: u64) -> Option<CodexUsage> {
    let mut f = std::fs::File::open(path).ok()?;
    let start = end.saturating_sub(BACKWARD_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((end - start) as usize);
    f.take(end - start).read_to_end(&mut buf).ok()?;
    let from = if start > 0 { buf.iter().position(|b| *b == b'\n').map_or(buf.len(), |i| i + 1) } else { 0 };
    last_token_count(&String::from_utf8_lossy(&buf[from..]))
}

/// Run-scoped Codex cache observation. The App wrapper supplies only the database, local rollout
/// resolver, and bot-status event capability; parsing and incremental file reads stay here.
pub async fn refresh_codex_with_ports<R, E>(
    database: &DbContext<SqlitePool>,
    rollout: &R,
    events: &E,
    run: &db::Run,
) where
    R: CodexRolloutAccess,
    E: EventSink,
{
    let Some(session) = run.native_session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { return };
    let session_id = session.to_owned();
    match db::bot_host(database.pool(), &run.bot_id).await.as_deref() {
        Ok(host) if host == crate::config::LOCAL_HOST => {}
        // 遠端 codex：rollout 在那台主機，經 host 通道增量讀；讀數有變才投影。
        Ok(_) => {
            let changed = refresh_remote(&run.id, &session_id, |offset| rollout.remote_rollout_read(&run.id, &run.bot_id, &session_id, offset)).await;
            if changed {
                emit_bot_status(events, &run.bot_id).await;
            }
            return;
        }
        Err(_) => return,
    }
    let (known_path, offset) = {
        let mut s = store().lock().unwrap_or_else(|e| e.into_inner());
        let e = s.entry(run.id.clone()).or_default();
        if e.session_id != session_id {
            // 換了 session（/new、resume）：位移與路徑都作廢，舊讀數不留。
            *e = Entry { session_id: session_id.clone(), ..Default::default() };
        }
        (e.path.clone(), e.offset)
    };
    let path = match known_path {
        Some(p) => p,
        None => {
            let Some(p) = rollout.local_rollout_path(&run.bot_id, &session_id).await else { return };
            p
        }
    };
    let p2 = path.clone();
    let known = store().lock().unwrap_or_else(|e| e.into_inner()).get(&run.id).is_some_and(|e| e.usage.is_some());
    let Ok(Ok((text, new_offset, latest))) = tokio::task::spawn_blocking(move || {
        let (text, new_offset) = read_new(&p2, offset)?;
        let mut latest = last_token_count(&text);
        // 還沒有任何讀數（daemon 剛重啟、第一次看到這個 run），檔尾又剛好全是工具輸出：往前多找一段，
        // 不然冷掉之後永遠不知道冷前用了多少 context（使用者 2026-10-14）。
        if latest.is_none() && !known {
            latest = last_token_count_before(&p2, new_offset);
        }
        Ok::<_, std::io::Error>((text, new_offset, latest))
    })
    .await
    else {
        return;
    };
    let _ = text;
    let changed = {
        let mut s = store().lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = s.get_mut(&run.id).filter(|e| e.session_id == session_id) else { return };
        e.path = Some(path);
        e.offset = new_offset;
        let changed = latest.is_some() && latest != e.usage;
        if latest.is_some() {
            e.usage = latest;
        }
        changed
    };
    if changed {
        emit_bot_status(events, &run.bot_id).await;
    }
}

/// 遠端 codex 的一輪：距上次打 host 不到 [`REMOTE_EVERY`] 就不讀。位移與本機一樣只在記憶體（`Entry::offset`），
/// 第一次（位移為 0）傳 `None` 讓實作讀檔尾。讀失敗保留上一次讀數、不前進位移，仍記下這次時間（退避到下一個間隔）。
/// 回傳讀數是否變動（呼叫端據此投影）。
pub async fn refresh_remote<F, Fut>(run_id: &str, session: &str, read: F) -> bool
where
    F: FnOnce(Option<u64>) -> Fut,
    Fut: Future<Output = Option<RemoteRollout>>,
{
    let offset = {
        let mut s = store().lock().unwrap_or_else(|e| e.into_inner());
        let e = s.entry(run_id.to_owned()).or_default();
        if e.session_id != session {
            // 換了 session：位移與舊讀數都作廢，同本機。
            *e = Entry { session_id: session.to_owned(), ..Default::default() };
        }
        if e.remote_at.is_some_and(|at| at.elapsed() < REMOTE_EVERY) {
            return false;
        }
        e.remote_at = Some(Instant::now());
        (e.offset > 0).then_some(e.offset)
    };
    let Some(read) = read(offset).await else { return false };
    let latest = last_token_count(&read.text);
    let mut s = store().lock().unwrap_or_else(|e| e.into_inner());
    let Some(e) = s.get_mut(run_id).filter(|e| e.session_id == session) else { return false };
    e.offset = read.next;
    let changed = latest.is_some() && latest != e.usage;
    if latest.is_some() {
        e.usage = latest;
    }
    changed
}

async fn emit_bot_status<E: EventSink>(events: &E, bot_id: &str) {
    if let Err(error) = events.bot_status_changed(bot_id).await {
        tracing::warn!(bot = %bot_id, error = ?error, "prompt cache bot-status projection failed");
    }
}

fn pct1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

/// `"1h"`／`"5m"`／`"300s"`（或數字秒）→ 秒。
fn ttl_secs_of(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().filter(|n| *n > 0),
        Value::String(s) => {
            let s = s.trim();
            // 單位取最後一個字元：切位元組會切在非 ASCII 字元中間。乘法用 checked，溢位就當作不知道。
            let unit = s.chars().last()?;
            let n: i64 = s[..s.len() - unit.len_utf8()].trim().parse().ok()?;
            let mul = match unit {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                _ => return None,
            };
            (n > 0).then(|| n.checked_mul(mul)).flatten()
        }
        _ => None,
    }
}

/// claude：從 statusLine 原 JSON 挑精簡欄位。沒有 `prompt_cache` 這塊（舊版 claude）回 `Null`。
pub fn from_statusline(status_json: &str) -> Value {
    let Ok(v) = serde_json::from_str::<Value>(status_json) else { return Value::Null };
    let Some(pc) = v.get("prompt_cache").filter(|p| p.is_object()) else { return Value::Null };
    let ctx = v.get("context_window");
    let pct = ctx.and_then(|c| c.get("used_percentage")).and_then(Value::as_f64);
    let size = ctx.and_then(|c| c.get("context_window_size")).and_then(Value::as_i64);
    // 這次請求讀進去的整段 context：current_usage 三項加總；沒有就用百分比乘視窗大小。
    let usage_sum = ctx.and_then(|c| c.get("current_usage")).filter(|u| u.is_object()).map(|u| {
        ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
            .iter()
            .filter_map(|k| u.get(*k).and_then(Value::as_i64))
            .sum::<i64>()
    });
    let tokens = usage_sum.filter(|n| *n > 0).or_else(|| pct.zip(size).map(|(p, s)| (p / 100.0 * s as f64).round() as i64));
    json!({
        "source": "statusline",
        "warm": pc.get("warm").and_then(Value::as_bool),
        "expires_at": pc.get("expires_at").and_then(Value::as_i64),
        "ttl_secs": ttl_secs_of(pc.get("ttl")),
        "recache_tokens_if_cold": pc.get("recache_tokens_if_cold").and_then(Value::as_i64),
        "hit_ratio": pc.get("hit_ratio").and_then(Value::as_f64),
        "last_miss_cause": pc.get("last_miss_cause").filter(|c| c.is_string()),
        "caching_observed": pc.get("caching_observed").and_then(Value::as_bool),
        "context_used_pct": pct.map(pct1),
        "context_used_tokens": tokens,
        "context_size": size,
    })
}

/// codex：由 rollout 最後一筆 `token_count` 推算。有 TTL 時到期＝事件時間＋TTL、`warm` 以 `now_ms` 為準；
/// 沒有 TTL（[`crate::cache_clock::ttl_secs`]，codex 不因閒置冷掉）＝一直熱、`expires_at`／`ttl_secs` 為 `null`。
pub fn from_codex_usage(u: &CodexUsage, ttl_secs: Option<i64>, now_ms: i64) -> Value {
    let expires_ms = ttl_secs.map(|ttl| u.at_ms + ttl * 1000);
    json!({
        "source": "rollout_estimate",
        "warm": expires_ms.is_none_or(|exp| now_ms < exp),
        "expires_at": expires_ms.map(|exp| exp / 1000),
        "ttl_secs": ttl_secs,
        "recache_tokens_if_cold": u.input,
        "hit_ratio": (u.input > 0).then(|| (u.cached as f64 / u.input as f64 * 1000.0).round() / 1000.0),
        "last_miss_cause": Value::Null,
        "caching_observed": u.cached > 0,
        "context_used_pct": (u.window > 0).then(|| pct1(u.input as f64 / u.window as f64 * 100.0)),
        "context_used_tokens": u.input,
        "context_size": (u.window > 0).then_some(u.window),
    })
}

/// codex run 目前的 context 用量百分比（rollout 最近一筆 `token_count`：這次請求讀進去的 context ÷ 視窗）；還沒讀到／視窗不明回 `None`。
/// 保溫的熱壓門檻用（`primary_keep_warm::context_used_pct`）。
pub fn codex_context_pct(run_id: &str) -> Option<f64> {
    let u = store().lock().unwrap_or_else(|e| e.into_inner()).get(run_id).and_then(|e| e.usage)?;
    (u.window > 0).then(|| u.input as f64 / u.window as f64 * 100.0)
}

/// 在 run JSON 上補 `prompt_cache`（claude／codex；grok、沒有資料＝`null`）；沒有 run（`null`）不動。
pub fn annotate(run_json: &mut Value, kind: &str, now_ms: i64) {
    let Some(o) = run_json.as_object_mut() else { return };
    let pc = match kind {
        "claude" => o.get("status_json").and_then(Value::as_str).map_or(Value::Null, from_statusline),
        "codex" => {
            let usage = o.get("id").and_then(Value::as_str).and_then(|id| store().lock().unwrap_or_else(|e| e.into_inner()).get(id).and_then(|e| e.usage));
            usage.map_or(Value::Null, |u| from_codex_usage(&u, crate::cache_clock::ttl_secs("codex"), now_ms))
        }
        _ => Value::Null,
    };
    o.insert("prompt_cache".into(), pc);
}
