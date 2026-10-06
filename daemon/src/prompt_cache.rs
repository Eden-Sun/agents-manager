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
use crate::state::App;
use am_ports::{CodexRolloutAccess, DbContext, EventSink};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use sqlx::SqlitePool;

/// 第一次（或一輪新增太多）時只讀檔尾這麼多；rollout 一輪 token_count 約幾 KB，256 KB 足夠找到最後一筆。
const TAIL_BYTES: u64 = 256 * 1024;

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
struct Entry {
    session_id: String,
    path: Option<PathBuf>,
    offset: u64,
    usage: Option<CodexUsage>,
}

fn store() -> &'static Mutex<HashMap<String, Entry>> {
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
fn read_new(path: &Path, offset: u64) -> std::io::Result<(String, u64)> {
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

/// 巡邏每輪對每顆 codex run 叫一次：讀 rollout 新增的部分，最後一筆 `token_count` 變了就推 `bot_status`。
/// 找不到 session／rollout、遠端主機、讀失敗都靜靜跳過（提示而已，不影響任何流程）。
pub async fn refresh_codex(app: &Arc<App>, run: &db::Run) {
    crate::app_ports_r2a8::refresh_codex(app, run).await;
}

/// Run-scoped Codex cache observation. The App wrapper supplies only the database, local rollout
/// resolver, and bot-status event capability; parsing and incremental file reads stay here.
pub(crate) async fn refresh_codex_with_ports<R, E>(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 真實格式（codex 0.15x rollout，2026-09-28 取樣）：第一筆 `info:null`、其後每筆 `last_token_usage` 是這一次請求。
    const ROLLOUT: &str = concat!(
        r#"{"timestamp":"2026-09-28T14:35:12.000Z","ordinal":3,"type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#, "\n",
        r#"{"timestamp":"2026-09-28T14:35:13.100Z","ordinal":4,"type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex"}}}"#, "\n",
        r#"{"timestamp":"2026-09-28T14:35:32.249Z","ordinal":18,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":13547,"cached_input_tokens":11008,"cache_write_input_tokens":0,"output_tokens":267,"reasoning_output_tokens":126,"total_tokens":13814},"last_token_usage":{"input_tokens":13547,"cached_input_tokens":11008,"cache_write_input_tokens":0,"output_tokens":267,"reasoning_output_tokens":126,"total_tokens":13814},"model_context_window":258400},"rate_limits":{"limit_id":"codex","primary":{"used_percent":8.0,"window_minutes":300,"resets_at":1790622294}}}}"#, "\n",
        r#"{"timestamp":"2026-09-28T14:35:40.856Z","ordinal":25,"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":31214,"cached_input_tokens":24064,"cache_write_input_tokens":0,"output_tokens":552,"reasoning_output_tokens":322,"total_tokens":31766},"last_token_usage":{"input_tokens":17667,"cached_input_tokens":13056,"cache_write_input_tokens":0,"output_tokens":285,"reasoning_output_tokens":196,"total_tokens":17952},"model_context_window":258400},"rate_limits":{"limit_id":"codex"}}}"#, "\n",
        r#"{"timestamp":"2026-09-28T14:35:41.000Z","ordinal":26,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"token_count 不是這行"}]}}"#, "\n",
    );

    fn ms(s: &str) -> i64 {
        db::parse_ts(s).unwrap().timestamp_millis()
    }

    #[test]
    fn the_last_token_count_in_a_real_rollout_is_read() {
        let u = last_token_count(ROLLOUT).unwrap();
        assert_eq!(u, CodexUsage { at_ms: ms("2026-09-28T14:35:40.856Z"), input: 17667, cached: 13056, window: 258400 });
        // `info:null`、其他事件、半行都不是讀數。
        assert!(last_token_count(r#"{"timestamp":"2026-09-28T14:35:13.100Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#).is_none());
        assert!(parse_token_count(r#"{"timestamp":"2026-09-28T14:35:32.249Z","type":"event_msg","payload":{"type":"token_cou"#).is_none());
        assert!(last_token_count("").is_none());
    }

    /// 保溫的熱壓門檻讀它：codex 的 context 用量＝最近一筆 `token_count` 的 input ÷ 視窗。
    #[test]
    fn codex_context_pct_is_the_last_request_over_the_window() {
        let run = "r-codex-ctx-pct";
        assert_eq!(codex_context_pct(run), None, "還沒讀到 rollout");
        let u = last_token_count(ROLLOUT).unwrap();
        store().lock().unwrap().insert(run.into(), Entry { usage: Some(u), ..Default::default() });
        let pct = codex_context_pct(run).unwrap();
        assert!((pct - 17667.0 / 258400.0 * 100.0).abs() < 1e-9, "{pct}");
        store().lock().unwrap().insert(run.into(), Entry { usage: Some(CodexUsage { window: 0, ..u }), ..Default::default() });
        assert_eq!(codex_context_pct(run), None, "視窗大小不明不猜");
        store().lock().unwrap().remove(run);
    }

    #[test]
    fn codex_usage_becomes_an_estimate_with_context_and_hit_ratio() {
        let u = last_token_count(ROLLOUT).unwrap();
        let hot = from_codex_usage(&u, 3600, u.at_ms + 10 * 60_000);
        assert_eq!(hot["source"], "rollout_estimate");
        assert_eq!(hot["warm"], true);
        assert_eq!(hot["expires_at"], u.at_ms / 1000 + 3600);
        assert_eq!(hot["recache_tokens_if_cold"], 17667);
        assert_eq!(hot["hit_ratio"], 0.739);
        assert_eq!(hot["context_used_pct"], 6.8);
        assert_eq!(hot["context_used_tokens"], 17667);
        assert_eq!(hot["context_size"], 258400);
        assert_eq!(from_codex_usage(&u, 3600, u.at_ms + 61 * 60_000)["warm"], false);
    }

    /// claude 2.1.289 statusLine 實例（取用欄位以外的原文不外送）。
    const STATUS: &str = r#"{"version":"2.1.289","cwd":"/secret/path","transcript_path":"/x",
        "context_window":{"used_percentage":45,"context_window_size":1000000,"total_input_tokens":9999999,
          "current_usage":{"input_tokens":1000,"cache_creation_input_tokens":8000,"cache_read_input_tokens":441000}},
        "prompt_cache":{"warm":true,"expires_at":1791119812,"ttl":"1h","hit_ratio":0.994,"recache_tokens_if_cold":459258,"misses":0,
          "miss_causes":{},"last_miss_cause":null,"cache_write_tokens":434165,"requests":213,"caching_observed":true}}"#;

    #[test]
    fn claude_statusline_is_slimmed_down_without_the_raw_json() {
        let v = from_statusline(STATUS);
        assert_eq!(v["source"], "statusline");
        assert_eq!(v["warm"], true);
        assert_eq!(v["expires_at"], 1791119812);
        assert_eq!(v["ttl_secs"], 3600);
        assert_eq!(v["recache_tokens_if_cold"], 459258);
        assert_eq!(v["hit_ratio"], 0.994);
        assert_eq!(v["context_used_pct"], 45.0);
        assert_eq!(v["context_used_tokens"], 450000);
        assert_eq!(v["context_size"], 1000000);
        let s = v.to_string();
        assert!(!s.contains("/secret/path") && !s.contains("cache_write_tokens") && !s.contains("miss_causes"));
    }

    #[test]
    fn claude_without_prompt_cache_has_none() {
        assert!(from_statusline(r#"{"version":"2.1.280","context_window":{"used_percentage":5}}"#).is_null());
        assert!(from_statusline("not json").is_null());
        let mut run = json!({"id": "r-old", "status_json": r#"{"context_window":{"used_percentage":5}}"#});
        annotate(&mut run, "claude", 0);
        assert!(run["prompt_cache"].is_null());
        let mut run = json!({"id": "r-new", "status_json": STATUS});
        annotate(&mut run, "claude", 0);
        assert_eq!(run["prompt_cache"]["warm"], true);
    }

    #[test]
    fn grok_and_no_run_carry_nothing() {
        let mut run = json!({"id": "r-grok", "status_json": STATUS});
        annotate(&mut run, "grok", 0);
        assert!(run["prompt_cache"].is_null());
        let mut none = Value::Null;
        annotate(&mut none, "claude", 0);
        assert!(none.is_null());
    }

    #[test]
    fn codex_annotate_uses_the_remembered_rollout_reading() {
        let u = last_token_count(ROLLOUT).unwrap();
        store().lock().unwrap().insert("r-codex-annotate".into(), Entry { session_id: "s".into(), usage: Some(u), ..Default::default() });
        let mut run = json!({"id": "r-codex-annotate"});
        annotate(&mut run, "codex", u.at_ms + 60_000);
        assert_eq!(run["prompt_cache"]["source"], "rollout_estimate");
        assert_eq!(run["prompt_cache"]["warm"], true);
        // 還沒讀到任何 token_count：null。
        let mut fresh = json!({"id": "r-codex-unseen"});
        annotate(&mut fresh, "codex", 0);
        assert!(fresh["prompt_cache"].is_null());
        store().lock().unwrap().remove("r-codex-annotate");
    }

    #[test]
    fn rollout_is_read_incrementally_and_never_from_the_middle_of_a_line() {
        let dir = crate::testing::scratch_dir("am-pc");
        let path = dir.join("rollout-x.jsonl");
        let lines: Vec<&str> = ROLLOUT.lines().collect();
        // 第一輪：前三行＋第四行只寫一半。
        let half = &lines[3][..40];
        std::fs::write(&path, format!("{}\n{}\n{}\n{half}", lines[0], lines[1], lines[2])).unwrap();
        let (t1, off1) = read_new(&path, 0).unwrap();
        assert_eq!(last_token_count(&t1).unwrap().input, 13547);
        assert_eq!(off1 as usize, lines[0].len() + lines[1].len() + lines[2].len() + 3);
        // 沒新增：空字串、位移不動。
        assert_eq!(read_new(&path, off1).unwrap(), (String::new(), off1));
        // 補完那一行：只讀到新增的整行，不重讀前面。
        std::fs::write(&path, format!("{}\n{}\n{}\n{}\n", lines[0], lines[1], lines[2], lines[3])).unwrap();
        let (t2, off2) = read_new(&path, off1).unwrap();
        assert_eq!(t2.lines().count(), 1);
        assert_eq!(last_token_count(&t2).unwrap().input, 17667);
        assert_eq!(off2, std::fs::metadata(&path).unwrap().len());
        // 檔案縮小（換檔）：從頭來。
        std::fs::write(&path, format!("{}\n", lines[2])).unwrap();
        assert_eq!(last_token_count(&read_new(&path, off2).unwrap().0).unwrap().input, 13547);
        // 新增很大：只取檔尾、第一個（可能是半行的）片段丟掉。
        let mut big = String::new();
        while (big.len() as u64) < TAIL_BYTES * 2 {
            big.push_str(lines[0]);
            big.push('\n');
        }
        big.push_str(lines[3]);
        big.push('\n');
        std::fs::write(&path, &big).unwrap();
        let (t3, off3) = read_new(&path, 0).unwrap();
        assert!(t3.len() as u64 <= TAIL_BYTES);
        assert_eq!(last_token_count(&t3).unwrap().input, 17667);
        assert_eq!(off3, big.len() as u64);
    }

    #[test]
    fn retain_runs_forgets_finished_runs() {
        store().lock().unwrap().insert("r-retain-pc".into(), Entry::default());
        retain_runs(&["other".to_string()]);
        assert!(!store().lock().unwrap().contains_key("r-retain-pc"));
    }
}
