//! 「有更新 · 重啟套用」之前先給使用者看新版改了什麼（2026-09-10 使用者需求）。
//! claude 更新提示不說版本：磁碟上的 `--version` 就是即將套用的版本，再切 CHANGELOG 的 from..to 段落。
//! 抓不到要明講（`found: false` + `error`），UI 不能靜默略過。版本探測不快取（要拿最新的）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::config::LOCAL_HOST;
use crate::state::App;

pub const CHANGELOG_URL: &str = "https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md";
/// codex 沒有真正的 CHANGELOG.md，改抓 releases；它在 TUI 當場問更新、新版未進磁碟，`to` 由呼叫端帶。
const CODEX_RELEASES_API: &str = "https://api.github.com/repos/openai/codex/releases?per_page=100";
const CODEX_RELEASES_URL: &str = "https://github.com/openai/codex/releases";
/// herdr 有真正的 CHANGELOG.md（Keep a Changelog 的 `## [x.y.z] - date`，[`parse_version`] 認得）。
pub const HERDR_CHANGELOG_URL: &str = "https://raw.githubusercontent.com/herdrdev/herdr/master/CHANGELOG.md";
const CACHE_TTL: Duration = Duration::from_secs(600);
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Default)]
pub struct ChangelogCache {
    inner: Mutex<std::collections::HashMap<String, (Instant, String)>>,
}

#[cfg(test)]
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
        if let Some(rest) = line.strip_prefix("## ") {
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
pub async fn installed_version(app: &Arc<App>, host: &str, kind: &str) -> Result<String> {
    let script = format!(
        "{}; [ -n \"$p\" ] && \"$p\" --version 2>/dev/null </dev/null | head -1 | tr -d '\\r'",
        crate::tools::login_abs_sh(kind).trim_end()
    );
    let out = if host == LOCAL_HOST {
        crate::hosts::sh_local_stdout(&script, VERSION_TIMEOUT, &format!("`{kind} --version`")).await?
    } else {
        let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
        // 睡著／斷線的主機：巡邏串行讀每台，不能每台都等滿 ssh 逾時（30 秒）才說讀不到。
        if !conn.is_connected() {
            return Err(anyhow!("host `{host}` 未連線，讀不到磁碟上的 `{kind}` 版本"));
        }
        conn.ssh_exec_path(&script).await?
    };
    let line = out.lines().next().unwrap_or("").trim();
    cli_version_string(line).ok_or_else(|| anyhow!("`{kind} --version` 回了「{line}」，看不出版本"))
}

/// 抓 feed 的網址與「是不是 codex releases JSON」。`release_triage` 的 CLI 是獨立行程，沒有 `App` 的快取可用。
pub(crate) fn feed_url(kind: &str) -> Option<(&'static str, bool)> {
    match kind {
        "claude" => Some((CHANGELOG_URL, false)),
        "codex" => Some((CODEX_RELEASES_API, true)),
        "herdr" => Some((HERDR_CHANGELOG_URL, false)),
        _ => None,
    }
}

pub fn source_url(kind: &str) -> &'static str {
    match kind {
        "codex" => CODEX_RELEASES_URL,
        "herdr" => HERDR_CHANGELOG_URL,
        _ => CHANGELOG_URL,
    }
}

/// releases JSON → 同 CHANGELOG 格式的 markdown。預覽版與草稿丟掉：使用者被問到的都是正式版。
pub(crate) fn codex_releases_to_md(json: &str) -> Result<String> {
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

pub(crate) async fn fetch_changelog(app: &Arc<App>, kind: &str) -> Result<String> {
    {
        let g = app.changelog.inner.lock().await;
        if let Some((at, text)) = g.get(kind) {
            if at.elapsed() < CACHE_TTL {
                return Ok(text.clone());
            }
        }
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
    app.changelog.inner.lock().await.insert(kind.to_string(), (Instant::now(), text.clone()));
    Ok(text)
}

/// 失敗都寫進回應不 panic。給了 `to` 就不探磁碟——codex 被問的當下新版還沒裝。
pub async fn lookup(app: &Arc<App>, host: &str, kind: &str, from: Option<&str>, to: Option<&str>) -> ChangelogReply {
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
    let all = parse_changelog(&md);
    let picked = pick_sections(&all, reply.from_version.as_deref(), &installed);
    if picked.is_empty() {
        reply.error = Some(format!("CHANGELOG 裡沒有 {installed} 這一版的段落"));
        return reply;
    }
    reply.found = true;
    reply.sections = picked;
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主機睡著／tailscale 斷線：背景巡邏（每 10 分鐘）讀磁碟版本不能對連不上的主機各等一趟 30 秒 ssh 逾時
    /// （每個 kind、每台主機各一次，串行），直接說連不上。
    #[tokio::test]
    async fn reading_the_disk_version_of_a_down_host_does_not_dial_ssh() {
        let host = "changelog-asleep";
        let env = crate::testing::env().await;
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        assert!(!conn.is_connected());
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        crate::hosts::set_ssh_fake(host, move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("2.1.5 (Claude Code)\n".into())
        });
        crate::hosts::set_ssh_delay(host, Duration::from_secs(3));
        let started = std::time::Instant::now();
        let err = installed_version(&env.app, host, "claude").await.unwrap_err();
        assert!(err.to_string().contains("未連線"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(1), "連不上的主機不能讓巡邏等：{:?}", started.elapsed());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "不該打任何 ssh");
    }

    const MD: &str = "# Changelog\n\n## 2.1.5\n\n- fixed a\n- fixed b\n\n## 2.1.4\n\n- thing\n\n## Unreleased\n\nnope\n\n## 2.1.3\n\n- old\n";

    #[test]
    fn splits_versions_and_skips_non_version_headings() {
        let s = parse_changelog(MD);
        assert_eq!(s.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["2.1.5", "2.1.4", "2.1.3"]);
        assert_eq!(s[0].body, "- fixed a\n- fixed b");
    }

    #[test]
    fn picks_the_range_newest_first() {
        let s = parse_changelog(MD);
        let p = pick_sections(&s, Some("2.1.3"), "2.1.5 (Claude Code)");
        assert_eq!(p.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["2.1.5", "2.1.4"]);
        let only = pick_sections(&s, None, "2.1.4");
        assert_eq!(only.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["2.1.4"]);
        assert!(pick_sections(&s, Some("2.1.5"), "2.1.5").is_empty());
        assert!(pick_sections(&s, None, "9.9.9").is_empty());
    }

    #[test]
    fn codex_releases_become_changelog_markdown() {
        let json = r#"[
          {"tag_name":"rust-v0.154.0","prerelease":false,"draft":false,"body":"New Features\n\n- thing"},
          {"tag_name":"rust-v0.154.0-alpha.6","prerelease":true,"draft":false,"body":"nope"},
          {"tag_name":"rust-v0.153.4","prerelease":false,"draft":false,"body":"- older"}
        ]"#;
        let md = codex_releases_to_md(json).unwrap();
        let s = parse_changelog(&md);
        assert_eq!(s.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["0.154.0", "0.153.4"]);
        let p = pick_sections(&s, Some("0.153.4"), "0.154.0");
        assert_eq!(p.len(), 1);
        assert!(p[0].body.contains("- thing"));
        assert!(codex_releases_to_md("[]").is_err());
    }

    #[test]
    fn version_parsing_tolerates_suffixes() {
        assert_eq!(parse_version("2.1.0 (Claude Code)"), Some(vec![2, 1, 0]));
        assert_eq!(parse_version("v1.0"), Some(vec![1, 0]));
        assert_eq!(parse_version("nope"), None);
    }

    /// herdr 的 CHANGELOG 是 Keep a Changelog 格式：`## [x.y.z] - date`，不是 Claude Code 那種
    /// 裸 `## x.y.z`。issue #66 整理 herdr 版本差異要吃得下這個格式，不然段落永遠抓不到。
    const HERDR_MD: &str = "# Changelog\n\n\
        ## Unreleased\n\n## [0.9.1] - 2026-09-16\n\n### Added\n- machine 遠端指令轉發\n\n\
        ## [0.9.0] - 2026-09-07\n\n### Changed\n- endpoint generation 1\n\n\
        ## [0.8.2] - 2026-08-01\n\n- 基準版\n";

    #[test]
    fn parses_keep_a_changelog_bracket_headings() {
        let s = parse_changelog(HERDR_MD);
        assert_eq!(s.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["0.9.1", "0.9.0", "0.8.2"], "帶中括號與日期的標題要能解析出版本號");
        assert!(s[0].body.contains("machine 遠端指令轉發"));
        let p = pick_sections(&s, Some("0.8.2"), "0.9.1");
        assert_eq!(p.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["0.9.1", "0.9.0"]);
    }

    /// `GET /api/changelog?kind=herdr`：herdr 有自己的 CHANGELOG 來源（Keep a Changelog），快取命中就不上網；
    /// 帶了 `to`（新版還沒裝）就不探磁碟。
    #[tokio::test]
    async fn herdr_lookup_reads_its_own_changelog_feed() {
        let e = crate::testing::env().await;
        e.app.changelog.seed("herdr", "# Changelog\n\n## Unreleased\n\n\
            ## [0.9.3] - 2026-09-29\n\n### Fixed\n- codex idle 判斷\n\n\
            ## [0.9.2] - 2026-09-24\n\n### Removed\n- `pane.graphics.*`\n\n\
            ## [0.9.1] - 2026-09-16\n\n- 基準\n").await;
        let r = lookup(&e.app, "local", "herdr", Some("0.9.1"), Some("0.9.3")).await;
        assert!(r.found, "{:?}", r.error);
        assert_eq!(r.sections.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["0.9.3", "0.9.2"]);
        assert_eq!(r.installed_version.as_deref(), Some("0.9.3"));
        assert_eq!(r.source_url, HERDR_CHANGELOG_URL);
        assert!(lookup(&e.app, "local", "grok", None, Some("1.0.0")).await.error.unwrap().contains("沒有 changelog 來源"));
    }
}
