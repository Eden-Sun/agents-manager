//! 上游新版分診（issue #204）：claude／codex 每出一版，把那一版的 changelog 逐條切開、用決定性規則分桶、
//! 記進帳本；語意判斷交給模型，結果再由 daemon 驗過才開 GitHub issue。
//!
//! A 這一半只做「抓 → 切條 → 分桶 → 記帳」，不做語意判斷、不碰 gh：
//! - [`split_entries`]：把 `changelog::Section` 的 body 切成逐條 entry。
//! - [`rules`]：`rules.toml` 的引擎。
//! - [`ledger`]：SQLite 帳本（狀態機 `pending|dispatched|judged|published|empty|failed`）。
//! - B：`verdict`（驗證模型交回的逐條 verdict）、`issue`（渲染＋gh publish＋去重＋上限）、`http`（`/api/release-triage/*`）。
//! - [`run_check`]：`agents-managerd release-triage-check` 背後的流程，輸出契約固定（見 [`CheckReport`]）。
//!
//! 版本比較、標題解析一律重用 `changelog.rs`（`parse_changelog`／`pick_sections`／`parse_version`），
//! 不另寫一套。

pub mod http;
pub mod issue;
pub mod ledger;
pub mod rules;
pub mod verdict;

use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::changelog::{self, Section};
pub use rules::Bucket;

/// 帳本裡一條 entry 的完整記錄（`entries_json` 的元素）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// `sha1(kind|version|空白正規化後的原文)` 前 10 碼（小寫十六進位）。
    pub id: String,
    /// 空白正規化後的原文（去掉行首 `- `，續行以單一空白接上）。**引用一律從這裡貼**，不用模型交回的文字。
    pub text: String,
    pub bucket: Bucket,
    pub categories: Vec<String>,
    /// 命中的規則名，事後能回答「這條當初為什麼沒開」。
    pub rules: Vec<String>,
}

/// SHA-1（RFC 3174）。只用來產生穩定的 entry id，不涉及安全性；不為它加依賴。
fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, b) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (hv, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *hv = hv.wrapping_add(v);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

/// 空白正規化：任何連續空白（含換行）壓成一個空格，頭尾去掉。
pub fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn entry_id(kind: &str, version: &str, text: &str) -> String {
    sha1_hex(format!("{kind}|{version}|{}", normalize(text)).as_bytes())[..10].to_string()
}

/// 把一個版本段落的 body 切成逐條原文：`- ` 開頭（行首）為一條，非空、非標題的後續行併入上一條
/// （縮排的子項也併進去，它跟母項是同一個改動）；空白行或標題結束目前這一條。
pub fn split_entry_texts(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut open = false;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("- ") {
            out.push(rest.to_string());
            open = true;
        } else if line.trim().is_empty() || line.starts_with('#') {
            open = false;
        } else if open {
            if let Some(last) = out.last_mut() {
                last.push(' ');
                last.push_str(line);
            }
        }
    }
    out.iter().map(|t| normalize(t)).filter(|t| !t.is_empty()).collect()
}

/// 一個版本段落 → 分好桶的 entry。`kind` 沒有規則 profile 時回錯。
pub fn build_entries(kind: &str, section: &Section) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for text in split_entry_texts(&section.body) {
        let c = rules::classify(kind, &text).ok_or_else(|| anyhow!("{kind} 沒有分診規則"))?;
        out.push(Entry {
            id: entry_id(kind, &section.version, &text),
            text,
            bucket: c.bucket,
            categories: c.categories,
            rules: c.rules,
        });
    }
    Ok(out)
}

/// codex 的 releases body 在 `## Changelog`（PR 流水帳，0.155.0 有一百多行）之前才是策展過的內容。
fn cut_codex_body(body: &str) -> String {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.trim() == "## Changelog" {
            break;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

/// `changelog::codex_releases_to_md` 的輸出 → 版本段落。
///
/// 不能直接用 `parse_changelog`：codex 的 body 自己就有 `## New Features`／`## Bug Fixes` 二級標題，
/// `parse_changelog` 遇到任何 `## ` 就結束目前段落，body 會被砍成空的。這裡只認「標題本身是版本號」的
/// `## x.y.z` 當段落起點（版本號解析仍是 `changelog::version_string`），其他 `## ` 留在 body 裡，
/// 最後再在 `## Changelog` 之前截斷。`GET /api/changelog` 走的 `parse_changelog` 不動。
pub fn codex_sections(md: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    let flush = |cur: &mut Option<(String, Vec<&str>)>, out: &mut Vec<Section>| {
        if let Some((v, body)) = cur.take() {
            out.push(Section { version: v, body: cut_codex_body(&body.join("\n")) });
        }
    };
    for line in md.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some(v) = changelog::version_string(rest.trim()) {
                flush(&mut cur, &mut out);
                cur = Some((v, Vec::new()));
                continue;
            }
        }
        if let Some((_, body)) = cur.as_mut() {
            body.push(line);
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// 上游 feed 的 markdown → 版本段落（claude 走 `parse_changelog`，codex 走 [`codex_sections`]）。
pub fn source_sections(kind: &str, md: &str) -> Vec<Section> {
    if kind == "codex" {
        codex_sections(md)
    } else {
        changelog::parse_changelog(md)
    }
}

/// 最新的正式版：feed 裡數值最大的那個。
fn latest_version(all: &[Section]) -> Option<String> {
    all.iter().filter_map(|s| changelog::parse_version(&s.version).map(|v| (v, s.version.clone()))).max().map(|(_, s)| s)
}

// ───────────────────────── check 的輸出契約（kick 腳本依賴，欄位名不要改） ─────────────────────────

#[derive(Debug, Serialize, PartialEq)]
pub struct KeptOut {
    pub id: String,
    pub text: String,
    pub categories: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct UnmatchedOut {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct PendingVersion {
    pub version: String,
    pub kept: Vec<KeptOut>,
    pub unmatched: Vec<UnmatchedOut>,
    pub dropped_count: usize,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct CheckReport {
    pub kind: String,
    pub from: String,
    pub to: String,
    pub pending: Vec<PendingVersion>,
}

fn pending_out(version: &str, entries: &[Entry]) -> PendingVersion {
    PendingVersion {
        version: version.to_string(),
        kept: entries
            .iter()
            .filter(|e| e.bucket == Bucket::Kept)
            .map(|e| KeptOut { id: e.id.clone(), text: e.text.clone(), categories: e.categories.clone() })
            .collect(),
        unmatched: entries.iter().filter(|e| e.bucket == Bucket::Unmatched).map(|e| UnmatchedOut { id: e.id.clone(), text: e.text.clone() }).collect(),
        dropped_count: entries.iter().filter(|e| e.bucket == Bucket::Dropped).count(),
    }
}

/// `release-triage-check` 的核心（不碰網路、不執行指令，測試直接餵 feed 內容）。
///
/// - `from`：`since` 明確給了就用它；否則是帳本裡已分診的最大版本；帳本是空的＝第一次跑，只把 `installed`
///   （磁碟上的版本）記成 `empty` 基準、不回 pending（不為安裝當下已經在的版本派工）。
/// - `to`：feed 裡最新的正式版。`(from, to]` 之間每一版各自一列；已在帳本的版本不重算（規則改了也不追溯，
///   要重來就明確給 `--since`）。
/// - `pending`：`(from, to]` 裡帳本狀態是 `pending` 的版本，舊的在前。
pub async fn check(pool: &SqlitePool, kind: &str, all: &[Section], installed: Option<&str>, since: Option<&str>) -> Result<CheckReport> {
    if !rules::supported(kind) {
        bail!("`{kind}` 沒有分診規則（目前只有 claude、codex）");
    }
    let to = latest_version(all).ok_or_else(|| anyhow!("{kind} 的 feed 裡沒有任何版本段落"))?;
    let since_v = match since {
        Some(s) => Some(changelog::version_string(s).ok_or_else(|| anyhow!("--since `{s}` 看不出版本"))?),
        None => None,
    };
    let ledger_max = ledger::max_version(pool, kind).await?;
    let from = match (since_v, ledger_max) {
        (Some(s), _) => s,
        (None, Some(m)) => m,
        (None, None) => {
            let inst = installed
                .and_then(changelog::version_string)
                .ok_or_else(|| anyhow!("帳本是空的，需要磁碟上的 {kind} 版本當基準，但讀不到（可用 --installed 指定）"))?;
            ledger::insert_baseline(pool, kind, &inst).await?;
            return Ok(CheckReport { kind: kind.to_string(), from: inst, to, pending: Vec::new() });
        }
    };
    ledger::requeue_stale(pool, kind).await?;
    for sec in changelog::pick_sections(all, Some(&from), &to) {
        let entries = build_entries(kind, &sec)?;
        ledger::insert_version(pool, kind, &sec.version, &entries).await?;
    }
    let mut pending: Vec<PendingVersion> = Vec::new();
    let from_v = changelog::parse_version(&from);
    let to_v = changelog::parse_version(&to);
    for row in ledger::rows_with_status(pool, kind, ledger::Status::Pending).await? {
        let v = changelog::parse_version(&row.version);
        if v > from_v && v <= to_v {
            pending.push(pending_out(&row.version, &row.entries));
        }
    }
    pending.sort_by_key(|p| changelog::parse_version(&p.version));
    Ok(CheckReport { kind: kind.to_string(), from, to, pending })
}

/// 磁碟上的版本（本機）：跟 `changelog::installed_version` 的本機分支同一段 login-shell 探測。
async fn local_installed_version(kind: &str) -> Result<String> {
    let script = format!(
        r#"p=$( "${{SHELL:-/bin/sh}}" -lic "command -v {kind}" 2>/dev/null | tail -1 ); [ -n "$p" ] || p=$(command -v {kind} 2>/dev/null); [ -n "$p" ] && "$p" --version 2>/dev/null </dev/null | head -1 | tr -d '\r'"#
    );
    let o = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new("/bin/sh").arg("-c").arg(&script).stdin(std::process::Stdio::null()).output(),
    )
    .await
    .map_err(|_| anyhow!("`{kind} --version` timed out"))??;
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    let line = out.lines().next().unwrap_or("").trim().to_string();
    changelog::version_string(&line).ok_or_else(|| anyhow!("`{kind} --version` 回了「{line}」，看不出版本"))
}

/// 抓上游 feed（沒有 daemon 的記憶體快取：CLI 是獨立行程，30 分鐘才跑一次）。抓不到就回錯——
/// 這一輪不做，不能當成「沒有新版」。
async fn fetch_feed(kind: &str) -> Result<String> {
    let (url, is_codex) = changelog::feed_url(kind).ok_or_else(|| anyhow!("{kind} 沒有 changelog 來源"))?;
    let client = reqwest::Client::builder()
        .user_agent("agents-manager")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| anyhow!("http client: {e}"))?;
    let resp = client.get(url).send().await.map_err(|e| anyhow!("抓 CHANGELOG 失敗：{e}"))?;
    if !resp.status().is_success() {
        bail!("抓 CHANGELOG 失敗：HTTP {}", resp.status());
    }
    let raw = resp.text().await.map_err(|e| anyhow!("讀 CHANGELOG 失敗：{e}"))?;
    if is_codex {
        changelog::codex_releases_to_md(&raw)
    } else {
        Ok(raw)
    }
}

/// 帳本所在的 SQLite：`--db` > `AM_DATA_DIR`／預設資料目錄底下的 `agents-manager.sqlite3`。
pub fn default_db_path() -> Result<std::path::PathBuf> {
    let dir = crate::startup::env_dir()?.unwrap_or_else(crate::startup::default_dir);
    Ok(dir.join("agents-manager.sqlite3"))
}

/// 只開帳本要的那一張表（不跑整套 `db::migrate`：daemon 正在用同一個檔案，CLI 不該順手升級全部 schema）。
async fn open_ledger_db(path: &Path) -> Result<SqlitePool> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(false)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(10));
    let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.map_err(|e| anyhow!("開不了 {}：{e}", path.display()))?;
    ledger::migrate(&pool).await?;
    Ok(pool)
}

pub struct CheckArgs {
    pub kind: String,
    pub since: Option<String>,
    pub installed: Option<String>,
    pub db: Option<std::path::PathBuf>,
    /// 測試用：不抓網路，直接讀這個檔案當 feed（claude＝CHANGELOG.md、codex＝`codex_releases_to_md` 的輸出）。
    pub feed_file: Option<std::path::PathBuf>,
}

/// CLI 入口：成功印 JSON、回 0；失敗（抓不到 feed、DB 開不了…）回錯，呼叫端 exit 非零。
pub async fn run_check(a: CheckArgs) -> Result<CheckReport> {
    let md = match &a.feed_file {
        Some(p) => std::fs::read_to_string(p).map_err(|e| anyhow!("讀不了 {}：{e}", p.display()))?,
        None => fetch_feed(&a.kind).await?,
    };
    let all = source_sections(&a.kind, &md);
    let db = match &a.db {
        Some(p) => p.clone(),
        None => default_db_path()?,
    };
    let pool = open_ledger_db(&db).await?;
    // 只有帳本是空的第一次才需要磁碟版本；避免每 30 分鐘白跑一次 login shell。
    let installed = match (&a.installed, ledger::max_version(&pool, &a.kind).await?, &a.since) {
        (Some(i), _, _) => Some(i.clone()),
        (None, None, None) => Some(local_installed_version(&a.kind).await?),
        _ => None,
    };
    let report = check(&pool, &a.kind, &all, installed.as_deref(), a.since.as_deref()).await;
    pool.close().await;
    report
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_publish;
