//! 「有更新 · 重啟套用」之前先給使用者看新版改了什麼（2026-09-10 使用者需求）。
//!
//! claude 自動更新只在 pane 底下印一句 `Update installed · Restart to update`，沒說是哪一版、
//! 改了什麼。這裡做兩件事：
//! 1. 在那台主機上再跑一次 `claude --version`——磁碟上已經是新版（跑著的 process 還是舊的），
//!    這就是「即將套用」的版本；
//! 2. 抓 Claude Code 的 `CHANGELOG.md`（GitHub raw），切出 `from`（跑著的版本）到新版之間的段落。
//!
//! 抓不到就在回應裡明講（`found: false` + `error`），UI 要寫「找不到 changelog」，不能靜默略過。
//! CHANGELOG 全文快取 10 分鐘；版本探測不快取（就是要拿最新的）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::config::LOCAL_HOST;
use crate::state::App;

/// claude：裝好等重啟，版本從磁碟上的 `claude --version` 探。
pub const CHANGELOG_URL: &str = "https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md";
/// codex：沒有 CHANGELOG.md（repo 裡那份只寫「去看 releases」），改抓 releases API。
/// 而且它是 TUI 裡當場問「Update now / Skip」，新版還沒進磁碟——版本要由呼叫端從畫面帶 `to` 進來。
const CODEX_RELEASES_API: &str = "https://api.github.com/repos/openai/codex/releases?per_page=100";
const CODEX_RELEASES_URL: &str = "https://github.com/openai/codex/releases";
const CACHE_TTL: Duration = Duration::from_secs(600);
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

/// 一個 kind 一份全文快取（claude 是 CHANGELOG.md，codex 是 releases 併成的同格式 markdown）。
#[derive(Default)]
pub struct ChangelogCache {
    inner: Mutex<std::collections::HashMap<String, (Instant, String)>>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Section {
    pub version: String,
    /// 該版本標題底下的原文（markdown），不含 `## x.y.z` 那行。
    pub body: String,
}

#[derive(Serialize, Debug)]
pub struct ChangelogReply {
    pub kind: String,
    pub host: String,
    /// 磁碟上（即將套用）的版本；`claude --version` 拿不到就是 null。
    pub installed_version: Option<String>,
    /// 呼叫端說的「現在跑著」的版本（原樣回去，方便 UI 寫 `1.2.3 → 1.2.5`）。
    pub from_version: Option<String>,
    /// 是否真的找到了對應版本的段落。false 時 `sections` 為空、`error` 說明原因。
    pub found: bool,
    pub sections: Vec<Section>,
    pub source_url: String,
    pub error: Option<String>,
}

/// `x.y.z` 之外的字（`(Claude Code)`、前綴 `v`）都丟掉。
pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    let tok = s.split_whitespace().next()?.trim_start_matches('v');
    let parts: Vec<u64> = tok.split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then_some(parts)
}

/// `2.1.269 (Claude Code)` → `2.1.269`。認不出來就是 `None`。
pub fn version_string(s: &str) -> Option<String> {
    parse_version(s).map(|v| v.iter().map(|n| n.to_string()).collect::<Vec<_>>().join("."))
}

/// 把 CHANGELOG.md 切成一段一版。認 `## 1.2.3` 這種二級標題（Claude Code 的格式）。
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

/// 那台主機**磁碟上**的版本（`<kind> --version`）。跑著的 process 可能還是舊的——
/// 這正是 `update_watch` 用來判斷「重啟就會換新版」的那一半。
pub async fn installed_version(app: &Arc<App>, host: &str, kind: &str) -> Result<String> {
    let script = format!(
        r#"p=$( "${{SHELL:-/bin/sh}}" -lic "command -v {kind}" 2>/dev/null | tail -1 ); [ -n "$p" ] || p=$(command -v {kind} 2>/dev/null); [ -n "$p" ] && "$p" --version 2>/dev/null </dev/null | head -1 | tr -d '\r'"#
    );
    let out = if host == LOCAL_HOST {
        let o = tokio::time::timeout(
            VERSION_TIMEOUT,
            tokio::process::Command::new("/bin/sh").arg("-c").arg(&script).stdin(std::process::Stdio::null()).output(),
        )
        .await
        .map_err(|_| anyhow!("`{kind} --version` timed out"))??;
        String::from_utf8_lossy(&o.stdout).to_string()
    } else {
        let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
        conn.ssh_exec_path(&script).await?
    };
    let line = out.lines().next().unwrap_or("").trim();
    version_string(line).ok_or_else(|| anyhow!("`{kind} --version` 回了「{line}」，看不出版本"))
}

/// UI 上「完整 CHANGELOG ↗」要連去的地方。
pub fn source_url(kind: &str) -> &'static str {
    if kind == "codex" {
        CODEX_RELEASES_URL
    } else {
        CHANGELOG_URL
    }
}

/// codex 的 releases JSON → 跟 CHANGELOG.md 同格式的 markdown，後面就能共用 `parse_changelog`。
/// 預覽版（`0.154.0-alpha.6`）與草稿一律丟掉：使用者被問到的都是正式版。
fn codex_releases_to_md(json: &str) -> Result<String> {
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
        // `0.154.0-alpha.6` 的 `version_string` 會失敗（parse 不了 `0-alpha`），這裡再擋一次帶後綴的。
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

async fn fetch_changelog(app: &Arc<App>, kind: &str) -> Result<String> {
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
    let url = if kind == "codex" { CODEX_RELEASES_API } else { CHANGELOG_URL };
    let resp = client.get(url).send().await.map_err(|e| anyhow!("抓 CHANGELOG 失敗：{e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("抓 CHANGELOG 失敗：HTTP {}", resp.status()));
    }
    let raw = resp.text().await.map_err(|e| anyhow!("讀 CHANGELOG 失敗：{e}"))?;
    let text = if kind == "codex" { codex_releases_to_md(&raw)? } else { raw };
    app.changelog.inner.lock().await.insert(kind.to_string(), (Instant::now(), text.clone()));
    Ok(text)
}

/// 主流程：版本探測與抓 changelog 各自失敗都不 panic，錯誤寫進回應。
/// `to`：呼叫端已經知道的目標版本（codex 是從 TUI 那句 `0.153.4 -> 0.154.0` 讀來的）。
/// 給了就不再探磁碟——codex 被問的當下新版根本還沒裝。
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
    if kind != "claude" && kind != "codex" {
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
}
