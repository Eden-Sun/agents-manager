//! 「有更新 · 重啟套用」之前先給使用者看新版改了什麼（2026-09-10 使用者需求）。
//! claude 更新提示不說版本：磁碟上的 `--version` 就是即將套用的版本，再切 CHANGELOG 的 from..to 段落。
//! 抓不到要明講（`found: false` + `error`），UI 不能靜默略過。版本探測不快取（要拿最新的）。

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::config::LOCAL_HOST;

pub const CHANGELOG_URL: &str = "https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md";
/// codex 沒有真正的 CHANGELOG.md，改抓 releases；它在 TUI 當場問更新、新版未進磁碟，`to` 由呼叫端帶。
const CODEX_RELEASES_API: &str = "https://api.github.com/repos/openai/codex/releases?per_page=100";
const CODEX_RELEASES_URL: &str = "https://github.com/openai/codex/releases";
/// herdr 有真正的 CHANGELOG.md（Keep a Changelog 的 `## [x.y.z] - date`，[`parse_version`] 認得）。
pub const HERDR_CHANGELOG_URL: &str = "https://raw.githubusercontent.com/herdrdev/herdr/master/CHANGELOG.md";
/// agy（issue #1928）：官方 repo 的 CHANGELOG.md（`## x.y.z` ＋ `- ` 單行條目）；抓不到才退同 repo 的 GitHub releases。
pub const AGY_CHANGELOG_URL: &str = "https://raw.githubusercontent.com/google-antigravity/antigravity-cli/refs/heads/main/CHANGELOG.md";
pub const AGY_RELEASES_API: &str = "https://api.github.com/repos/google-antigravity/antigravity-cli/releases?per_page=100";
const AGY_CHANGELOG_PAGE: &str = "https://github.com/google-antigravity/antigravity-cli/blob/main/CHANGELOG.md";
/// grok：CDN 的 `<版>.external.md`（`# x.y.z — 日期` 一段一版，stable 的檔涵蓋上一個 stable 之後的所有版本）。
/// 版本只能取自 stable 指標（[`crate::upstream_update::GROK_STABLE_URL`]），見 [`grok_feed_url`]。
pub const GROK_CHANGELOG_BASE: &str = "https://storage.googleapis.com/grok-build-public-artifacts/cli/changelogs";
const CACHE_TTL: Duration = Duration::from_secs(600);
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Default)]
pub struct ChangelogCache {
    inner: Mutex<std::collections::HashMap<String, (Instant, String)>>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl ChangelogCache {
    /// 測試直接塞一份 CHANGELOG 進快取，不上網。
    pub async fn seed(&self, kind: &str, text: &str) {
        self.inner.lock().await.insert(kind.to_string(), (Instant::now(), text.to_string()));
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Section {
    pub version: String,
    /// 不含 `## x.y.z` 那行。
    pub body: String,
}

#[derive(Serialize, Debug)]
pub struct ChangelogReply {
    pub kind: String,
    pub host: String,
    /// 磁碟上（即將套用）的版本。
    pub installed_version: Option<String>,
    pub from_version: Option<String>,
    /// false 時 `sections` 為空、`error` 說明原因。
    pub found: bool,
    pub sections: Vec<Section>,
    pub source_url: String,
    pub error: Option<String>,
}

pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    // `[0.9.0] - 2026-09-07`（herdr 的 CHANGELOG 用 Keep a Changelog 的 `## [x.y.z] - date` 標題，
    // 不是 Claude Code 那種裸 `## x.y.z`）：先取第一個空白分隔的 token 再拆掉包住版本號的中括號，
    // 日期本來就在下一個 token，split_whitespace 早就丟掉了。
    let tok = s.split_whitespace().next()?;
    let tok = tok.trim_start_matches('[').trim_end_matches(']').trim_start_matches('v');
    let parts: Vec<u64> = tok.split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then_some(parts)
}

/// `2.1.269 (Claude Code)` → `2.1.269`；`[0.9.0] - 2026-09-07` → `0.9.0`。
pub fn version_string(s: &str) -> Option<String> {
    parse_version(s).map(|v| v.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("."))
}

/// `--version` 的輸出：`2.1.278 (Claude Code)`、`codex-cli 0.154.0`、`herdr 0.8.2`。版本不一定在第一個 token，
/// 取第一個看得出版本的 token（至少 `x.y` 兩段，避免把 `codex-cli` 之類當成版本）。
pub fn cli_version_string(line: &str) -> Option<String> {
    line.split_whitespace().filter(|t| t.contains('.')).find_map(version_string)
}

/// 認 `## 1.2.3` 二級標題（Claude Code 的格式）。
pub fn parse_changelog(md: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    for line in md.lines() {
        // agy 的 1.1.4 標題前面真的多一個空白（` ## 1.1.4`）：縮排的 `## ` 只有後面是版本號才算標題，
        // 否則（例如 body 裡縮排的 `## foo`）照舊當內文，不截斷段落。
        let heading = line.strip_prefix("## ").or_else(|| line.trim_start().strip_prefix("## ").filter(|r| version_string(r.trim()).is_some()));
        if let Some(rest) = heading {
            if let Some((v, body)) = cur.take() {
                out.push(Section { version: v, body: body.join("\n").trim().to_string() });
            }
            match version_string(rest.trim()) {
                Some(v) => cur = Some((v, Vec::new())),
                None => cur = None,
            }
            continue;
        }
        if let Some((_, body)) = cur.as_mut() {
            body.push(line);
        }
    }
    if let Some((v, body)) = cur.take() {
        out.push(Section { version: v, body: body.join("\n").trim().to_string() });
    }
    out
}

/// grok 的 `.external.md`（`# x.y.z — 日期` 一段一版）→ 版本段落。`## Features`／`## Bug Fixes` 等小標題留在 body 裡
/// （[`parse_changelog`] 遇到任何 `## ` 就結束段落，會把 body 砍掉），只有一級標題且後面是版本號才開新段落。
pub fn grok_sections(md: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    for line in md.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            if let Some((v, body)) = cur.take() {
                out.push(Section { version: v, body: body.join("\n").trim().to_string() });
            }
            cur = version_string(rest.trim()).map(|v| (v, Vec::new()));
            continue;
        }
        if let Some((_, body)) = cur.as_mut() {
            body.push(line);
        }
    }
    if let Some((v, body)) = cur.take() {
        out.push(Section { version: v, body: body.join("\n").trim().to_string() });
    }
    out
}

/// `from`（不含）到 `to`（含）之間的段落，新的在前。`from` 不明就只給 `to` 那一段。
pub fn pick_sections(all: &[Section], from: Option<&str>, to: &str) -> Vec<Section> {
    let Some(to_v) = parse_version(to) else { return Vec::new() };
    let from_v = from.and_then(parse_version);
    let mut picked: Vec<Section> = all
        .iter()
        .filter(|s| {
            let Some(v) = parse_version(&s.version) else { return false };
            if v > to_v {
                return false;
            }
            match &from_v {
                Some(f) => v > *f,
                None => v == to_v,
            }
        })
        .cloned()
        .collect();
    picked.sort_by(|a, b| parse_version(&b.version).cmp(&parse_version(&a.version)));
    picked
}

/// **磁碟上**的版本；跑著的 process 可能還是舊的，`update_watch` 靠這個差判斷有更新。
pub async fn installed_version(app: &impl crate::hosts::HostsAccess, host: &str, kind: &str) -> Result<String> {
    let script = format!(
        "{}; printf 'AM_VER=%s\\n' \"$([ -n \"$p\" ] && \"$p\" --version 2>/dev/null </dev/null | head -1 | tr -d '\\r')\"",
        crate::tools::login_abs_sh(kind).trim_end()
    );
    let out = if host == LOCAL_HOST {
        crate::hosts::sh_local_stdout(&script, VERSION_TIMEOUT, &format!("`{kind} --version`")).await?
    } else {
        let conn = app.hosts().get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
        // 睡著／斷線的主機：巡邏串行讀每台，不能每台都等滿 ssh 逾時（30 秒）才說讀不到。
        if !conn.is_connected() {
            return Err(anyhow!("host `{host}` 未連線，讀不到磁碟上的 `{kind}` 版本"));
        }
        conn.ssh_exec_path(&script).await?
    };
    // 只認腳本自己印的 `AM_VER=` 那一行：遠端 shell 的 banner、motd 夾在前面（或只剩 banner）時，第一行不是 CLI 的版本。
    let line = out.lines().find_map(|l| l.split_once("AM_VER=").map(|(_, v)| v.trim())).unwrap_or("");
    cli_version_string(line).ok_or_else(|| anyhow!("`{kind} --version` 回了「{}」，看不出版本", out.trim()))
}

/// 抓 feed 的網址與「是不是 codex releases JSON」。`release_triage` 的 CLI 是獨立行程，沒有 `App` 的快取可用。
pub fn feed_url(kind: &str) -> Option<(&'static str, bool)> {
    match kind {
        "claude" => Some((CHANGELOG_URL, false)),
        "codex" => Some((CODEX_RELEASES_API, true)),
        "herdr" => Some((HERDR_CHANGELOG_URL, false)),
        "agy" => Some((AGY_CHANGELOG_URL, false)),
        // grok 的網址要帶版本：這裡回的是 stable 指標，實際的 changelog 網址由 [`grok_feed_url`] 組。
        "grok" => Some((crate::upstream_update::GROK_STABLE_URL, false)),
        _ => None,
    }
}

/// grok 某一版的 changelog 網址。版本號只收 `x.y.z` 純數字加點（stable 指標的內容不可信，不能帶出 `/`、`:`、`?`、`#`）。
pub fn grok_feed_url(base: &str, version: &str) -> Result<String> {
    let strict = !version.is_empty() && version.split('.').count() == 3 && version.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !strict {
        return Err(anyhow!("grok 版本看不懂：「{}」", version.chars().take(60).collect::<String>()));
    }
    Ok(format!("{base}/{version}.external.md"))
}

pub fn source_url(kind: &str) -> &'static str {
    match kind {
        "codex" => CODEX_RELEASES_URL,
        "herdr" => HERDR_CHANGELOG_URL,
        "agy" => AGY_CHANGELOG_PAGE,
        "grok" => crate::upstream_update::GROK_STABLE_URL,
        _ => CHANGELOG_URL,
    }
}

/// 「這一版的 changelog 在哪」的連結：grok 指到該版的 `.external.md`（每一版各有一個檔），其他同 [`source_url`]。
pub fn source_url_for(kind: &str, version: &str) -> String {
    if kind == "grok" {
        if let Ok(u) = grok_feed_url(GROK_CHANGELOG_BASE, version) {
            return u;
        }
    }
    source_url(kind).to_string()
}

/// releases JSON → 同 CHANGELOG 格式的 markdown。預覽版與草稿丟掉：使用者被問到的都是正式版。
pub fn codex_releases_to_md(json: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| anyhow!("讀 releases 失敗：{e}"))?;
    let arr = v.as_array().ok_or_else(|| anyhow!("releases 不是陣列"))?;
    let mut out = String::new();
    for r in arr {
        if r.get("draft").and_then(|b| b.as_bool()).unwrap_or(false) || r.get("prerelease").and_then(|b| b.as_bool()).unwrap_or(false) {
            continue;
        }
        let tag = r.get("tag_name").and_then(|t| t.as_str()).unwrap_or("");
        let tag = tag.trim_start_matches("rust-");
        let Some(ver) = version_string(tag) else { continue };
        // 保險：再擋一次帶 `-alpha` 之類後綴的 tag。
        if tag.trim_start_matches('v').contains('-') {
            continue;
        }
        let body = r.get("body").and_then(|b| b.as_str()).unwrap_or("").trim();
        out.push_str(&format!("## {ver}\n\n{body}\n\n"));
    }
    if out.is_empty() {
        return Err(anyhow!("releases 裡沒有正式版"));
    }
    Ok(out)
}

/// agy／grok 的抓取來源。正式環境用 [`FeedSource::official`]；測試換成本機伺服器（`allowed` 也跟著換）。
///
/// 抓回來的一切都是**資料**：網址全寫死成常數，只允許三個主機（`raw.githubusercontent.com`／`api.github.com` 的
/// `google-antigravity/antigravity-cli`、`storage.googleapis.com` 的 `grok-build-public-artifacts`）；唯一由回應組出來的網址是
/// grok 的 `<stable 版本>.external.md`，版本號先過 [`grok_feed_url`] 的嚴格檢查；不跟隨轉址；每個請求出門前再比一次允許清單。
#[derive(Debug, Clone)]
pub struct FeedSource {
    pub agy_changelog: String,
    pub agy_releases: String,
    pub grok_stable: String,
    pub grok_base: String,
    pub allowed: Vec<String>,
}

/// 允許請求的網址前綴（含路徑，不只是主機）。
const ALLOWED_FEED_PREFIXES: [&str; 4] = [
    "https://raw.githubusercontent.com/google-antigravity/antigravity-cli/",
    "https://api.github.com/repos/google-antigravity/antigravity-cli/",
    "https://storage.googleapis.com/grok-build-public-artifacts/cli/changelogs/",
    crate::upstream_update::GROK_STABLE_URL,
];
/// 一份 changelog 的大小上限：最大的 agy CHANGELOG.md 約 174 KB，給一個數量級的餘裕；stable 指標只有一個版本號。
const MAX_FEED_BYTES: usize = 2 * 1024 * 1024;
const MAX_POINTER_BYTES: usize = 256;

impl FeedSource {
    pub fn official() -> Self {
        Self {
            agy_changelog: AGY_CHANGELOG_URL.into(),
            agy_releases: AGY_RELEASES_API.into(),
            grok_stable: crate::upstream_update::GROK_STABLE_URL.into(),
            grok_base: GROK_CHANGELOG_BASE.into(),
            allowed: ALLOWED_FEED_PREFIXES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// 這個網址在不在正式允許清單裡。
pub fn is_official_feed_url(url: &str) -> bool {
    ALLOWED_FEED_PREFIXES.iter().any(|p| url.starts_with(p))
}

async fn get_pinned(c: &reqwest::Client, src: &FeedSource, url: &str, max: usize, what: &str) -> Result<String> {
    if !src.allowed.iter().any(|p| url.starts_with(p.as_str())) {
        return Err(anyhow!("{what}：{url} 不在允許的來源清單裡"));
    }
    let mut resp = c.get(url).send().await.map_err(|e| anyhow!("抓 {what} 失敗：{e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("抓 {what} 失敗：HTTP {}", resp.status()));
    }
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(anyhow!("抓 {what} 失敗：內容超過 {max} 位元組"));
    }
    // 沒有 Content-Length 的 chunked 回應也要擋：邊讀邊算，超過就停（不是整份讀完才檢查）。
    let mut raw: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| anyhow!("讀 {what} 失敗：{e}"))? {
        if raw.len() + chunk.len() > max {
            return Err(anyhow!("抓 {what} 失敗：內容超過 {max} 位元組"));
        }
        raw.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// agy：raw 的 CHANGELOG.md，失敗才退 GitHub releases。grok：stable 指標 → 該版的 `.external.md`。
/// 逾時同 claude／codex（連線 10 秒、整體 20 秒）；轉址一律當失敗。
pub async fn fetch_pinned_feed(kind: &str, src: &FeedSource) -> Result<String> {
    let c = reqwest::Client::builder()
        .user_agent("agents-manager")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| anyhow!("http client: {e}"))?;
    match kind {
        "agy" => match get_pinned(&c, src, &src.agy_changelog, MAX_FEED_BYTES, "agy CHANGELOG.md").await {
            Ok(md) => Ok(md),
            Err(first) => {
                let json = get_pinned(&c, src, &src.agy_releases, MAX_FEED_BYTES, "agy releases")
                    .await
                    .map_err(|second| anyhow!("{first}；備援也失敗：{second}"))?;
                codex_releases_to_md(&json)
            }
        },
        "grok" => {
            let pointer = get_pinned(&c, src, &src.grok_stable, MAX_POINTER_BYTES, "grok stable 指標").await?;
            let version = crate::upstream_update::grok_latest(&pointer)?;
            let url = grok_feed_url(&src.grok_base, &version)?;
            get_pinned(&c, src, &url, MAX_FEED_BYTES, "grok changelog").await
        }
        other => Err(anyhow!("{other} 不走這個抓取路徑")),
    }
}

pub async fn fetch_changelog(app: &impl crate::changelog::ChangelogState, kind: &str) -> Result<String> {
    {
        let g = app.changelog().inner.lock().await;
        if let Some((at, text)) = g.get(kind) {
            if at.elapsed() < CACHE_TTL {
                return Ok(text.clone());
            }
        }
    }
    if matches!(kind, "agy" | "grok") {
        let text = fetch_pinned_feed(kind, &FeedSource::official()).await?;
        app.changelog().inner.lock().await.insert(kind.to_string(), (Instant::now(), text.clone()));
        return Ok(text);
    }
    let client = reqwest::Client::builder()
        .user_agent("agents-manager")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| anyhow!("http client: {e}"))?;
    let (url, is_codex) = feed_url(kind).ok_or_else(|| anyhow!("{kind} 沒有 changelog 來源"))?;
    let resp = client.get(url).send().await.map_err(|e| anyhow!("抓 CHANGELOG 失敗：{e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("抓 CHANGELOG 失敗：HTTP {}", resp.status()));
    }
    let raw = resp.text().await.map_err(|e| anyhow!("讀 CHANGELOG 失敗：{e}"))?;
    let text = if is_codex { codex_releases_to_md(&raw)? } else { raw };
    app.changelog().inner.lock().await.insert(kind.to_string(), (Instant::now(), text.clone()));
    Ok(text)
}

/// 失敗都寫進回應不 panic。給了 `to` 就不探磁碟——codex 被問的當下新版還沒裝。
pub async fn lookup(app: &(impl crate::changelog::ChangelogState + crate::hosts::HostsAccess), host: &str, kind: &str, from: Option<&str>, to: Option<&str>) -> ChangelogReply {
    let mut reply = ChangelogReply {
        kind: kind.to_string(),
        host: host.to_string(),
        installed_version: None,
        from_version: from.and_then(version_string),
        found: false,
        sections: Vec::new(),
        source_url: source_url(kind).to_string(),
        error: None,
    };
    if feed_url(kind).is_none() {
        reply.error = Some(format!("{kind} 沒有 changelog 來源"));
        return reply;
    }
    let installed = match to.and_then(version_string) {
        Some(v) => v,
        None => match installed_version(app, host, kind).await {
            Ok(v) => v,
            Err(e) => {
                reply.error = Some(format!("{e:#}"));
                return reply;
            }
        },
    };
    reply.installed_version = Some(installed.clone());
    let md = match fetch_changelog(app, kind).await {
        Ok(t) => t,
        Err(e) => {
            reply.error = Some(format!("{e:#}"));
            return reply;
        }
    };
    let all = if kind == "grok" { grok_sections(&md) } else { crate::release_triage::source_sections(kind, &md) };
    if kind == "grok" {
        // 抓到的是 stable 那一版的檔：它的版本就是 feed 裡最大的那一段。
        if let Some(newest) = all.iter().filter_map(|s| parse_version(&s.version).map(|v| (v, s.version.as_str()))).max() {
            reply.source_url = source_url_for(kind, newest.1);
        }
    }
    let picked = pick_sections(&all, reply.from_version.as_deref(), &installed);
    if picked.is_empty() {
        reply.error = Some(format!("CHANGELOG 裡沒有 {installed} 這一版的段落"));
        return reply;
    }
    reply.found = true;
    reply.sections = picked;
    reply
}



/// changelog 快取。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait ChangelogState: Send + Sync {
    fn changelog(&self) -> &crate::changelog::ChangelogCache;
}
