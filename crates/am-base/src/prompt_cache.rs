//! `run.prompt_cache`：輸入框上方「送出的代價」提示用的精簡欄位（SPEC §6.5j、UI-DECISIONS）。
//!
//! 只做 claude 與 codex（grok 不帶、不畫）：
//! * claude：statusLine 的 `prompt_cache`（≥ 2.1.289）與 `context_window`，從 `runs.status_json` 挑精簡欄位，**不外送原文**；
//!   `source:"statusline"`。沒有這塊（舊版 claude）＝`null`，網頁退回 `last_api_at`＋`cache_ttl_secs` 推算。
//! * codex：沒有 statusLine，改讀它的 rollout（`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*-<session>.jsonl`）最近一筆
//!   `event_msg`／`token_count` 的 `info`：`last_token_usage.input_tokens`（＝這次請求讀進去的整段 context）、
//!   `cached_input_tokens`、`model_context_window`。到期時間＝那筆事件的 `timestamp` ＋ [`cache_clock::ttl_secs`]（推算，
//!   `source:"rollout_estimate"`）。`recache_tokens_if_cold` 用 `input_tokens` 近似。
//!
//! 檔案只讀新增的部分：記住每個 run 的路徑與位移，下一輪只讀位移之後的整行；第一次（沒位移）只讀檔尾
//! [`TAIL_BYTES`]。記憶體帳，daemon 重啟後第一輪重讀檔尾即可補回；run 結束由巡邏清掉（[`retain_runs`]）。

use crate::db;
use am_ports::{CodexRolloutAccess, DbContext, EventSink};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};


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
}

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn entry_for_test(session_id: Option<&str>, usage: Option<CodexUsage>) -> Entry {
    Entry { session_id: session_id.unwrap_or_default().to_string(), usage, ..Entry::default() }
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
    if !matches!(db::bot_host(database.pool(), &run.bot_id).await.as_deref(), Ok(host) if host == crate::config::LOCAL_HOST) {
        return;
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
    let Ok(Ok((text, new_offset))) = tokio::task::spawn_blocking(move || read_new(&p2, offset)).await else { return };
    let latest = last_token_count(&text);
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
            let (num, unit) = s.split_at(s.len().checked_sub(1)?);
            let n: i64 = num.trim().parse().ok()?;
            let mul = match unit {
                "s" => 1,
                "m" => 60,
                "h" => 3600,
                _ => return None,
            };
            (n > 0).then_some(n * mul)
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

/// codex：由 rollout 最後一筆 `token_count` 推算；到期＝事件時間＋TTL，`warm` 以 `now_ms` 為準。
pub fn from_codex_usage(u: &CodexUsage, ttl_secs: i64, now_ms: i64) -> Value {
    let expires_ms = u.at_ms + ttl_secs * 1000;
    json!({
        "source": "rollout_estimate",
        "warm": now_ms < expires_ms,
        "expires_at": expires_ms / 1000,
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
            usage.zip(crate::cache_clock::ttl_secs("codex")).map_or(Value::Null, |(u, ttl)| from_codex_usage(&u, ttl, now_ms))
        }
        _ => Value::Null,
    };
    o.insert("prompt_cache".into(), pc);
}
