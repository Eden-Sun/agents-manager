//! issue 渲染與 gh publish（issue #204 §4、§5）。
//!
//! 這條管線唯一的外部寫入就是 `gh issue create`／`gh issue comment`。全部走可注入的 `gh` 路徑
//! （`[release_triage] gh_bin`），測試餵假腳本；`publish = false`（預設）時**完全不會**啟動 gh。
//!
//! 重試安全：開之前先查帳本、再用 `gh issue list --state all --search` 查隱藏標記——`--state all`，
//! 已關掉的（做完或判定不做）不復活；每開一張立刻寫回帳本，中途掛掉重跑不會開出第二張。
//! gh 失敗時列停在 `judged`（verdict 已存），下一輪只重試 publish、不重派模型。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::process::Command;

use super::ledger::{self, IssueRef, Row, Status};
use super::verdict::StoredProposal;
use super::Entry;
use crate::config::ReleaseTriageCfg;

/// 每一版最多開幾張（超過的留在帳本，`guard` 優先）。
pub const MAX_PER_VERSION: usize = 4;
/// 每 24 小時最多開幾張（超過的留待下一輪）。
pub const MAX_PER_DAY: usize = 8;
const GH_TIMEOUT: Duration = Duration::from_secs(40);

/// 同一個行程裡同時只跑一個 publish：兩個請求同時進來，查標記與開 issue 之間不能交錯。
static PUBLISH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub struct Gh {
    bin: PathBuf,
    repo: String,
}

impl Gh {
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        // launchd 的 PATH 只有 /usr/bin:/bin；補上 Homebrew 與 ~/.local/bin（#66 留言點出的洞）。
        let home = std::env::var("HOME").unwrap_or_default();
        let path = format!("/opt/homebrew/bin:/usr/local/bin:{home}/.local/bin:{}", std::env::var("PATH").unwrap_or_default());
        let out = tokio::time::timeout(
            GH_TIMEOUT,
            Command::new(&self.bin)
                .args(args)
                .env("PATH", path)
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE")
                .env_remove("FORCE_COLOR")
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| format!("gh {} 逾時", args.first().copied().unwrap_or("")))?
        .map_err(|e| format!("gh 無法執行（{}）：{e}", self.bin.display()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(format!("gh {} 失敗：{}", args.iter().take(2).copied().collect::<Vec<_>>().join(" "), if err.is_empty() { out.status.to_string() } else { err }));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

pub fn marker(kind: &str, version: &str, ids: &[String]) -> String {
    format!("{kind}@{version}#{}", ids.join(","))
}

fn quote(entries: &[Entry], ids: &[String]) -> String {
    ids.iter()
        .filter_map(|id| entries.iter().find(|e| &e.id == id))
        .map(|e| format!("> {}", e.text))
        .collect::<Vec<_>>()
        .join("\n>\n")
}

pub fn title(kind: &str, version: &str, p: &StoredProposal) -> String {
    format!("{kind} {version}: {}（{}）", p.title, if p.triage == "guard" { "提防" } else { "採用" })
}

/// 照 #102 的格式渲染。`## 來源` 的引用來自帳本的 entry 原文，不是模型交回的文字。
pub fn render_body(kind: &str, version: &str, entries: &[Entry], p: &StoredProposal) -> String {
    format!(
        "## 來源\n{kind} {version} changelog（{url}）：\n\n{quote}\n\n## 目標\n{goal}\n\n## 建議\n{suggestion}\n\n## 驗收\n{acceptance}\n\n<!-- release-triage: {marker} -->\n",
        url = crate::changelog::source_url(kind),
        quote = quote(entries, &p.entry_ids),
        goal = p.goal,
        suggestion = p.suggestion,
        acceptance = p.acceptance,
        marker = marker(kind, version, &p.entry_ids),
    )
}

fn render_comment(kind: &str, version: &str, entries: &[Entry], p: &StoredProposal) -> String {
    format!(
        "release-triage 在 {kind} {version} 又提到同一件事：\n\n{quote}\n\n{goal}\n\n<!-- release-triage: {marker} -->\n",
        quote = quote(entries, &p.entry_ids),
        goal = p.goal,
        marker = marker(kind, version, &p.entry_ids),
    )
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// `publish = false`：只寫帳本，gh 一次都沒叫。
    Disabled,
    /// 這一版的提案都處理完了。
    Published { created: usize, commented: usize, existing: usize, skipped: Vec<String> },
    /// 24 小時上限擋下，留待下一輪。
    Deferred { reason: String },
    /// gh／設定出錯，列停在 `judged`。
    Failed { error: String },
}

fn proposals_of(row: &Row) -> Vec<StoredProposal> {
    row.verdicts
        .as_ref()
        .and_then(|v| v.get("issues"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

fn number_from_url(url: &str) -> Option<i64> {
    url.trim().lines().last()?.trim().rsplit('/').next()?.parse().ok()
}

/// 把一個 `judged` 版本的提案開成 issue。冪等，可重複呼叫。
pub async fn publish_version(pool: &SqlitePool, cfg: &ReleaseTriageCfg, kind: &str, version: &str) -> Result<Outcome> {
    if !cfg.publish {
        return Ok(Outcome::Disabled);
    }
    let _g = PUBLISH_LOCK.lock().await;
    let Some(row) = ledger::get(pool, kind, version).await? else {
        return Err(anyhow!("帳本沒有 {kind} {version}"));
    };
    if row.status != Status::Judged {
        return Err(anyhow!("{kind} {version} 的狀態是 {}，不是 judged", row.status.as_str()));
    }
    let fail = |e: String| Outcome::Failed { error: e };
    let Some(repo) = cfg.repo.clone().filter(|r| !r.trim().is_empty()) else {
        let e = "[release_triage] repo 沒設定（owner/name）".to_string();
        ledger::save_publish(pool, kind, version, &row.issues, Status::Judged, Some(&e)).await?;
        return Ok(fail(e));
    };
    let gh = Gh { bin: PathBuf::from(cfg.gh_bin.clone().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| "gh".into())), repo };
    let mut issues = row.issues.clone();
    let record_err = |issues: &[IssueRef], e: String| {
        let issues = issues.to_vec();
        let e2 = e.clone();
        async move {
            ledger::save_publish(pool, kind, version, &issues, Status::Judged, Some(&e2)).await?;
            Ok::<Outcome, anyhow::Error>(Outcome::Failed { error: e })
        }
    };

    let mut proposals = proposals_of(&row);
    // guard 優先（stable sort：同類保持模型交來的順序）。
    proposals.sort_by_key(|p| p.triage != "guard");

    // 有事可做才碰 gh：全部已在帳本就不必問 auth。
    let already = |p: &StoredProposal, issues: &[IssueRef]| issues.iter().any(|i| i.entry_ids.iter().any(|id| p.entry_ids.contains(id)));
    if proposals.iter().all(|p| already(p, &issues)) {
        ledger::save_publish(pool, kind, version, &issues, Status::Published, None).await?;
        return Ok(Outcome::Published { created: 0, commented: 0, existing: 0, skipped: vec![] });
    }
    if let Err(e) = gh.run(&["auth", "status"]).await {
        return record_err(&issues, e).await;
    }

    let (mut created, mut commented, mut existing) = (0usize, 0usize, 0usize);
    let mut skipped: Vec<String> = Vec::new();
    let mut deferred: Option<String> = None;
    for p in &proposals {
        if already(p, &issues) {
            continue;
        }
        let mk = marker(kind, version, &p.entry_ids);
        // (a) 帳本查完了；(b) 遠端標記。`--state all`：已關的不復活。
        let search = format!("release-triage: {mk} in:body");
        let listed = match gh
            .run(&["issue", "list", "--repo", &gh.repo, "--state", "all", "--search", &search, "--json", "number,url,body", "-L", "20"])
            .await
        {
            Ok(o) => o,
            Err(e) => return record_err(&issues, e).await,
        };
        let needle = format!("release-triage: {mk} -->");
        let found: Option<(i64, String)> = serde_json::from_str::<Vec<serde_json::Value>>(listed.trim())
            .map_err(|e| anyhow!("gh issue list 回的不是 JSON：{e}"))?
            .iter()
            .find(|v| v.get("body").and_then(|b| b.as_str()).is_some_and(|b| b.contains(&needle)))
            .and_then(|v| Some((v.get("number")?.as_i64()?, v.get("url")?.as_str()?.to_string())));
        if let Some((number, url)) = found {
            issues.push(IssueRef { marker: mk, entry_ids: p.entry_ids.clone(), number, url, created_at: ledger::now_ts(), comment: true });
            ledger::save_publish(pool, kind, version, &issues, Status::Judged, None).await?;
            existing += 1;
            continue;
        }
        if let Some(dup) = p.duplicate_of {
            let body = render_comment(kind, version, &row.entries, p);
            if let Err(e) = gh.run(&["issue", "comment", &dup.to_string(), "--repo", &gh.repo, "--body", &body]).await {
                return record_err(&issues, e).await;
            }
            issues.push(IssueRef { marker: mk, entry_ids: p.entry_ids.clone(), number: dup, url: String::new(), created_at: ledger::now_ts(), comment: true });
            ledger::save_publish(pool, kind, version, &issues, Status::Judged, None).await?;
            commented += 1;
            continue;
        }
        let in_row = issues.iter().filter(|i| !i.comment).count();
        if in_row >= MAX_PER_VERSION {
            skipped.push(mk);
            continue;
        }
        if ledger::created_in_last_day(pool).await? >= MAX_PER_DAY {
            deferred = Some(format!("24 小時內已開 {MAX_PER_DAY} 張，{mk} 留待下一輪"));
            break;
        }
        let t = title(kind, version, p);
        let body = render_body(kind, version, &row.entries, p);
        let labels = ["release-triage".to_string(), format!("upstream:{kind}"), format!("triage:{}", p.triage)];
        let mut args: Vec<&str> = vec!["issue", "create", "--repo", &gh.repo, "--title", &t, "--body", &body];
        for l in &labels {
            args.push("--label");
            args.push(l);
        }
        let out = match gh.run(&args).await {
            Ok(o) => o,
            Err(e) => return record_err(&issues, e).await,
        };
        let Some(number) = number_from_url(&out) else {
            return record_err(&issues, format!("gh issue create 沒回可解析的網址：{}", out.trim())).await;
        };
        issues.push(IssueRef {
            marker: mk,
            entry_ids: p.entry_ids.clone(),
            number,
            url: out.trim().lines().last().unwrap_or_default().to_string(),
            created_at: ledger::now_ts(),
            comment: false,
        });
        // 開一張就寫一次：之後任何一步掛掉，重跑也不會重開這張。
        ledger::save_publish(pool, kind, version, &issues, Status::Judged, None).await?;
        created += 1;
    }
    if let Some(reason) = deferred {
        ledger::save_publish(pool, kind, version, &issues, Status::Judged, Some(&reason)).await?;
        return Ok(Outcome::Deferred { reason });
    }
    let note = (!skipped.is_empty()).then(|| format!("每版上限 {MAX_PER_VERSION} 張，未開：{}", skipped.join("、")));
    ledger::save_publish(pool, kind, version, &issues, Status::Published, note.as_deref()).await?;
    Ok(Outcome::Published { created, commented, existing, skipped })
}
