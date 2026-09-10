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

/// 目前只有 claude 有這種「裝好等重啟」的自動更新；codex／grok 不走這裡。
pub const CHANGELOG_URL: &str = "https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md";
const CACHE_TTL: Duration = Duration::from_secs(600);
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Default)]
pub struct ChangelogCache {
    inner: Mutex<Option<(Instant, String)>>,
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

fn version_string(s: &str) -> Option<String> {
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

async fn installed_version(app: &Arc<App>, host: &str, kind: &str) -> Result<String> {
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

async fn fetch_changelog(app: &Arc<App>) -> Result<String> {
    {
        let g = app.changelog.inner.lock().await;
        if let Some((at, text)) = g.as_ref() {
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
    let resp = client.get(CHANGELOG_URL).send().await.map_err(|e| anyhow!("抓 CHANGELOG 失敗：{e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("抓 CHANGELOG 失敗：HTTP {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| anyhow!("讀 CHANGELOG 失敗：{e}"))?;
    *app.changelog.inner.lock().await = Some((Instant::now(), text.clone()));
    Ok(text)
}

/// 主流程：版本探測與抓 changelog 各自失敗都不 panic，錯誤寫進回應。
pub async fn lookup(app: &Arc<App>, host: &str, kind: &str, from: Option<&str>) -> ChangelogReply {
    let mut reply = ChangelogReply {
        kind: kind.to_string(),
        host: host.to_string(),
        installed_version: None,
        from_version: from.and_then(version_string),
        found: false,
        sections: Vec::new(),
        source_url: CHANGELOG_URL.to_string(),
        error: None,
    };
    if kind != "claude" {
        reply.error = Some(format!("{kind} 沒有 changelog 來源"));
        return reply;
    }
    let installed = match installed_version(app, host, kind).await {
        Ok(v) => v,
        Err(e) => {
            reply.error = Some(format!("{e:#}"));
            return reply;
        }
    };
    reply.installed_version = Some(installed.clone());
    let md = match fetch_changelog(app).await {
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
    fn version_parsing_tolerates_suffixes() {
        assert_eq!(parse_version("2.1.0 (Claude Code)"), Some(vec![2, 1, 0]));
        assert_eq!(parse_version("v1.0"), Some(vec![1, 0]));
        assert_eq!(parse_version("nope"), None);
    }
}
