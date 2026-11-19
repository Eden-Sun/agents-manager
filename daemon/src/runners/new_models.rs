//! 新模型上線推播（使用者 2026-11-19）。
//!
//! 每 [`SCAN_EVERY`] 問一次模型清單（啟動後約 [`SCAN_FIRST`] 第一次）：
//! - codex：每台已連線 host 各問一次 `models::list(.., "codex", None, refresh=true)`，id 取聯集；某台失敗就跳過那台。
//! - claude：Anthropic Models API `GET /v1/models`（分頁），金鑰讀 `[claude_api] key_file`；沒有金鑰就略過 claude。
//!
//! 狀態存 `{data_dir}/models-seen.json`（`{"claude":[..],"codex":[..]}`）。某 kind **第一次**成功抓到＝只記錄、不推；
//! 之後出現新 id → 每個 kind 推一則。seen 只增不減：某台暫時抓不到、清單短一截，之後補回來不會被當成新模型重推。
//! 抓取失敗（或整個 kind 都沒抓到）不動 seen。推播開關 `new_models` 關掉時仍更新 seen。
//!
//! 金鑰只經過 `read_key` 與 HTTP header，不寫進 log、錯誤訊息或測試輸出。

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Once};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::state::App;

/// 掃描週期：6 小時。
pub const SCAN_EVERY: Duration = Duration::from_secs(6 * 3600);
/// daemon 啟動後第一次掃描的等待時間（等 host 連線起來）。
pub const SCAN_FIRST: Duration = Duration::from_secs(120);
const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const PAGE_LIMIT: u32 = 1000;
/// 分頁上限：避免 API 異常時無限續抓。
const MAX_PAGES: usize = 20;
const SEEN_FILE: &str = "models-seen.json";
const SWITCH_KEY: &str = "new_models";

/// kind（"claude"／"codex"）→ 已見過的 model id。沒有這個 kind 的鍵＝還沒成功抓過一次。
pub type Seen = BTreeMap<String, BTreeSet<String>>;

/// 一趟掃描的抓取結果；`None`＝這個 kind 這趟抓取失敗（或沒有金鑰），seen 不動。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Fetched {
    pub codex: Option<BTreeSet<String>>,
    pub claude: Option<BTreeSet<String>>,
}

/// Anthropic Models API 的一頁。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ModelsPage {
    pub ids: Vec<String>,
    pub has_more: bool,
    pub last_id: Option<String>,
}

pub fn load_seen(dir: &Path) -> Seen {
    std::fs::read_to_string(dir.join(SEEN_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<Seen>(&t).ok())
        .unwrap_or_default()
}

fn save_seen(dir: &Path, seen: &Seen) {
    if let Ok(text) = serde_json::to_string(seen) {
        let _ = crate::webpush::write_private(&dir.join(SEEN_FILE), text.as_bytes());
    }
}

/// 把這趟抓到的 id 併進 seen，回傳新出現的 id（排序）。第一次見到這個 kind 只記錄、回傳空。
pub fn absorb(seen: &mut Seen, kind: &str, ids: &BTreeSet<String>) -> Vec<String> {
    match seen.get_mut(kind) {
        None => {
            seen.insert(kind.to_string(), ids.clone());
            Vec::new()
        }
        Some(known) => {
            let new: Vec<String> = ids.difference(known).cloned().collect();
            known.extend(ids.iter().cloned());
            new
        }
    }
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "codex" => "Codex",
        "claude" => "Claude",
        other => other,
    }
}

/// 一則新模型推播的 payload（`send_plain` 的格式）。`new` 必須非空、已排序。
pub fn notice(kind: &str, new: &[String]) -> Value {
    json!({
        "title": "新模型上線",
        "body": format!("{}：{}", kind_label(kind), new.join("、")),
        "tag": format!("new-model:{kind}:{}", new.join(",")),
        "url": "/",
    })
}

/// 純邏輯：更新 seen，回傳要推的 payload。`push_enabled=false` 時只更新 seen。
pub fn scan(seen: &mut Seen, fetched: &Fetched, push_enabled: bool) -> Vec<Value> {
    let mut out = Vec::new();
    for (kind, ids) in [("codex", &fetched.codex), ("claude", &fetched.claude)] {
        let Some(ids) = ids else { continue };
        let new = absorb(seen, kind, ids);
        if push_enabled && !new.is_empty() {
            out.push(notice(kind, &new));
        }
    }
    out
}

/// 單趟 codex：各 host 的 list 結果取聯集；失敗的 host 跳過。全部失敗（或沒有 host）＝None。
pub async fn codex_union<F, Fut>(hosts: Vec<String>, mut list: F) -> Option<BTreeSet<String>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let mut ids = BTreeSet::new();
    let mut any_ok = false;
    for host in hosts {
        match list(host.clone()).await {
            Ok(v) => {
                any_ok = true;
                ids.extend(model_ids(&v));
            }
            Err(e) => tracing::warn!(host = %host, "新模型掃描：codex 清單抓取失敗，跳過這台：{e:#}"),
        }
    }
    any_ok.then_some(ids)
}

/// `models::list` 的回傳：`models[].id`。
pub fn model_ids(v: &Value) -> BTreeSet<String> {
    v["models"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str()).map(str::to_string).collect()
}

/// 分頁收集 Anthropic models。`get(after_id)` 抓一頁；`has_more` 為真就帶 `last_id` 續抓。
pub async fn collect_claude_pages<F, Fut>(mut get: F) -> Result<BTreeSet<String>>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<ModelsPage>>,
{
    let mut ids = BTreeSet::new();
    let mut after: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let page = get(after.clone()).await?;
        ids.extend(page.ids);
        if !page.has_more {
            return Ok(ids);
        }
        match page.last_id {
            Some(last) if Some(&last) != after.as_ref() => after = Some(last),
            _ => bail!("models 分頁沒有前進（has_more 但缺 last_id）"),
        }
    }
    bail!("models 分頁超過 {MAX_PAGES} 頁")
}

pub fn parse_page(v: &Value) -> Result<ModelsPage> {
    let data = v["data"].as_array().ok_or_else(|| anyhow!("models 回應缺 data"))?;
    Ok(ModelsPage {
        ids: data.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect(),
        has_more: v["has_more"].as_bool().unwrap_or(false),
        last_id: v["last_id"].as_str().map(str::to_string),
    })
}

async fn get_claude_page(key: &str, after: Option<&str>) -> Result<ModelsPage> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| anyhow!("無法建立 HTTP client"))?;
    let mut req = client
        .get(ANTHROPIC_MODELS_URL)
        .query(&[("limit", PAGE_LIMIT.to_string())])
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION);
    if let Some(a) = after {
        req = req.query(&[("after_id", a)]);
    }
    let res = req.send().await.map_err(|e| anyhow!("models 請求失敗：{}", e.without_url()))?;
    let status = res.status();
    if !status.is_success() {
        bail!("models API 回 HTTP {}", status.as_u16());
    }
    let v: Value = res.json().await.map_err(|_| anyhow!("models 回應不是 JSON"))?;
    parse_page(&v)
}

/// 讀金鑰；沒有（或讀不了）就回 None，並只記一次 info。錯誤訊息不含金鑰內容。
pub fn key_or_skip(key_file: &str) -> Option<String> {
    match crate::key_file::read_key(key_file) {
        Ok(k) => Some(k),
        Err(e) => {
            static ONCE: Once = Once::new();
            ONCE.call_once(|| tracing::info!("新模型掃描：沒有可用的 Anthropic 金鑰，略過 claude（{e}）"));
            None
        }
    }
}

async fn fetch_claude(app: &Arc<App>) -> Option<BTreeSet<String>> {
    let key_file = app.cfg.get().await.claude_api.key_file.clone();
    let key = key_or_skip(&key_file)?;
    match collect_claude_pages(|after| {
        let key = key.clone();
        async move { get_claude_page(&key, after.as_deref()).await }
    })
    .await
    {
        Ok(ids) => Some(ids),
        Err(e) => {
            tracing::warn!("新模型掃描：claude 清單抓取失敗：{e:#}");
            None
        }
    }
}

async fn fetch_codex(app: &Arc<App>) -> Option<BTreeSet<String>> {
    let hosts = app.hosts.names().await;
    codex_union(hosts, |host| async move { crate::runners::models::list(app, &host, "codex", None, true).await }).await
}

/// 一趟掃描：抓取、更新 seen（有抓到才寫檔）、依開關推播。
pub async fn run(app: &Arc<App>) {
    let fetched = Fetched { codex: fetch_codex(app).await, claude: fetch_claude(app).await };
    let enabled = crate::webpush::setting_enabled(&app.data_dir, SWITCH_KEY);
    let mut seen = load_seen(&app.data_dir);
    let payloads = scan(&mut seen, &fetched, enabled);
    if fetched.codex.is_some() || fetched.claude.is_some() {
        save_seen(&app.data_dir, &seen);
    }
    for p in payloads {
        crate::webpush::send_plain(app, p).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// 走 `testing::scratch_dir`：建好並登記，測試行程結束時會被掃掉（scratch leak guard 要求）。
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        crate::testing::scratch_dir(&format!("new-models-{tag}"))
    }

    #[test]
    fn first_success_only_records_then_a_new_id_pushes_once() {
        let mut seen = Seen::new();
        let f1 = Fetched { codex: Some(set(&["gpt-6.2", "gpt-6.2-mini"])), claude: None };
        assert!(scan(&mut seen, &f1, true).is_empty(), "第一次成功抓到只記錄、不推");
        assert_eq!(seen["codex"], set(&["gpt-6.2", "gpt-6.2-mini"]));

        let f2 = Fetched { codex: Some(set(&["gpt-6.2", "gpt-6.2-mini", "gpt-6.3-sol"])), claude: None };
        let pushes = scan(&mut seen, &f2, true);
        assert_eq!(pushes.len(), 1, "新 id 推一則");
        assert_eq!(pushes[0]["body"], "Codex：gpt-6.3-sol");
        assert_eq!(pushes[0]["title"], "新模型上線");
        assert_eq!(pushes[0]["tag"], "new-model:codex:gpt-6.3-sol");
        assert_eq!(pushes[0]["url"], "/");

        assert!(scan(&mut seen, &f2, true).is_empty(), "同一批 id 不重推");
    }

    #[test]
    fn each_kind_pushes_its_own_notice_and_the_body_joins_ids() {
        let mut seen = Seen::new();
        scan(&mut seen, &Fetched { codex: Some(set(&["a"])), claude: Some(set(&["claude-opus-5-5"])) }, true);
        let f = Fetched { codex: Some(set(&["a", "b", "c"])), claude: Some(set(&["claude-opus-5-5", "claude-opus-5-6"])) };
        let pushes = scan(&mut seen, &f, true);
        assert_eq!(pushes.len(), 2, "每個 kind 一則");
        assert_eq!(pushes[0]["body"], "Codex：b、c");
        assert_eq!(pushes[0]["tag"], "new-model:codex:b,c");
        assert_eq!(pushes[1]["body"], "Claude：claude-opus-5-6");
        assert_eq!(pushes[1]["tag"], "new-model:claude:claude-opus-5-6");
    }

    #[test]
    fn failed_fetch_leaves_seen_untouched() {
        let mut seen = Seen::new();
        scan(&mut seen, &Fetched { codex: Some(set(&["a"])), claude: None }, true);
        let before = seen.clone();
        // 抓取失敗：None 不動 seen，也不推。
        assert!(scan(&mut seen, &Fetched { codex: None, claude: None }, true).is_empty());
        assert_eq!(seen, before);
        // 失敗之後恢復：仍以原本的 seen 比對，只推真正新的 id。
        let pushes = scan(&mut seen, &Fetched { codex: Some(set(&["a", "z"])), claude: None }, true);
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0]["body"], "Codex：z");
    }

    #[test]
    fn switch_off_pushes_nothing_but_still_updates_seen() {
        let mut seen = Seen::new();
        scan(&mut seen, &Fetched { codex: Some(set(&["a"])), claude: None }, false);
        let pushes = scan(&mut seen, &Fetched { codex: Some(set(&["a", "new1"])), claude: None }, false);
        assert!(pushes.is_empty(), "開關關掉不推");
        assert_eq!(seen["codex"], set(&["a", "new1"]), "但 seen 仍更新");
        // 之後再打開：不會把 new1 當成新的補推。
        assert!(scan(&mut seen, &Fetched { codex: Some(set(&["a", "new1"])), claude: None }, true).is_empty());
    }

    #[test]
    fn ids_that_disappear_are_kept_so_they_do_not_repush_when_back() {
        let mut seen = Seen::new();
        scan(&mut seen, &Fetched { codex: Some(set(&["a", "b"])), claude: None }, true);
        // 某台暫時抓不到，清單短一截：不移除，也不推。
        assert!(scan(&mut seen, &Fetched { codex: Some(set(&["a"])), claude: None }, true).is_empty());
        assert_eq!(seen["codex"], set(&["a", "b"]));
        // 補回來：b 已見過，不推。
        assert!(scan(&mut seen, &Fetched { codex: Some(set(&["a", "b"])), claude: None }, true).is_empty());
    }

    #[test]
    fn seen_roundtrips_through_the_private_file() {
        let dir = tmp_dir("roundtrip");
        assert!(load_seen(&dir).is_empty(), "沒有檔＝空");
        let mut seen = Seen::new();
        seen.insert("claude".into(), set(&["claude-opus-5-5"]));
        save_seen(&dir, &seen);
        let text = std::fs::read_to_string(dir.join(SEEN_FILE)).unwrap();
        assert!(text.contains(r#""claude":["claude-opus-5-5"]"#), "{text}");
        assert_eq!(load_seen(&dir), seen);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_union_skips_failing_hosts_and_none_when_all_fail() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let ok = |ids: &'static [&'static str]| {
            json!({"models": ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()})
        };
        let got = rt.block_on(codex_union(vec!["local".into(), "m4p".into(), "bad".into()], |host| async move {
            match host.as_str() {
                "local" => Ok(ok(&["gpt-6.2", "gpt-6.3-sol"])),
                "m4p" => Ok(ok(&["gpt-6.3-sol", "gpt-6.4"])),
                _ => Err(anyhow!("ssh 不通")),
            }
        }));
        assert_eq!(got, Some(set(&["gpt-6.2", "gpt-6.3-sol", "gpt-6.4"])), "聯集，失敗那台跳過");
        let none = rt.block_on(codex_union(vec!["bad".into()], |_| async { Err(anyhow!("ssh 不通")) }));
        assert_eq!(none, None, "全部失敗＝None（不當成模型消失）");
        let no_hosts = rt.block_on(codex_union(vec![], |_| async { Ok(json!({})) }));
        assert_eq!(no_hosts, None);
    }

    #[test]
    fn claude_pages_follow_has_more_with_after_id() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let mut calls: Vec<Option<String>> = Vec::new();
        let got = rt.block_on(collect_claude_pages(|after| {
            calls.push(after.clone());
            async move {
                match after.as_deref() {
                    None => Ok(ModelsPage { ids: vec!["claude-a".into()], has_more: true, last_id: Some("claude-a".into()) }),
                    Some("claude-a") => Ok(ModelsPage { ids: vec!["claude-b".into()], has_more: false, last_id: Some("claude-b".into()) }),
                    other => panic!("unexpected cursor {other:?}"),
                }
            }
        }));
        assert_eq!(got.unwrap(), set(&["claude-a", "claude-b"]));
        assert_eq!(calls, vec![None, Some("claude-a".to_string())]);
    }

    #[test]
    fn claude_pages_stop_on_a_stuck_cursor_and_on_too_many_pages() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let stuck = rt.block_on(collect_claude_pages(|_| async { Ok(ModelsPage { ids: vec!["x".into()], has_more: true, last_id: None }) }));
        assert!(stuck.is_err(), "has_more 但沒有 last_id 直接失敗");
        let forever = rt.block_on(collect_claude_pages(|_| async {
            Ok(ModelsPage { ids: vec![], has_more: true, last_id: Some("same".into()) })
        }));
        assert!(forever.is_err(), "游標不前進就失敗");
        let mut n = 0;
        let many = rt.block_on(collect_claude_pages(|_| {
            n += 1;
            let id = format!("p{n}");
            async move { Ok(ModelsPage { ids: vec![], has_more: true, last_id: Some(id) }) }
        }));
        assert!(many.is_err(), "超過分頁上限就失敗");
    }

    #[test]
    fn parse_page_reads_data_has_more_and_last_id() {
        let p = parse_page(&json!({"data": [{"id": "claude-x"}, {"nope": 1}], "has_more": true, "last_id": "claude-x"})).unwrap();
        assert_eq!(p, ModelsPage { ids: vec!["claude-x".into()], has_more: true, last_id: Some("claude-x".into()) });
        assert!(parse_page(&json!({"has_more": false})).is_err(), "缺 data 是錯誤");
    }

    #[test]
    fn no_key_file_means_claude_is_skipped_without_error() {
        let dir = tmp_dir("nokey");
        let missing = dir.join("anthropic-api-key");
        assert_eq!(key_or_skip(missing.to_str().unwrap()), None, "沒有金鑰＝略過");
        std::fs::write(&missing, "sk-test-123\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&missing, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(key_or_skip(missing.to_str().unwrap()).as_deref(), Some("sk-test-123"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_ids_reads_models_array_only() {
        assert_eq!(model_ids(&json!({"models": [{"id": "a"}, {"x": 1}, {"id": "b"}]})), set(&["a", "b"]));
        assert!(model_ids(&json!({})).is_empty());
    }
}
