//! issue 渲染與 gh publish（issue #204 §4、§5）。
//!
//! 這條管線唯一的外部寫入就是 `gh issue create`／`gh issue comment`。全部走可注入的 `gh` 路徑
//! （`[release_triage] gh_bin`），測試餵假腳本；`publish = false`（預設）時**完全不會**啟動 gh。
//!
//! 重試安全：開之前先查帳本、再用 `gh issue list --state all --search` 查隱藏標記——`--state all`，
//! 已關掉的（做完或判定不做）不復活；每開一張立刻寫回帳本，中途掛掉重跑不會開出第二張。
//! gh 失敗時列停在 `judged`（verdict 已存），下一輪只重試 publish、不重派模型。
//!
//! [`preflight`] 是唯一在 `publish = false` 時也會啟動 gh 的路徑：它由人明確觸發（`--dry-run`），
//! 只讀（`auth status`／`repo view`／`label list`／`issue list`），**不開 issue、不寫帳本**——
//! 要能在打開 `publish` 之前就看出「這一版會開哪幾張、標籤在不在、去重會不會命中」。

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
                // 逾時＝丟掉這個 future。沒有這一行的話子行程不會被殺，`gh issue create` 會在背景
                // 繼續把 issue 開出來，而呼叫端已經當它失敗、帳本裡沒有那張的紀錄（#456）。
                .kill_on_drop(true)
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

fn gh_for(cfg: &ReleaseTriageCfg) -> Gh {
    Gh {
        bin: PathBuf::from(cfg.gh_bin.clone().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| "gh".into())),
        repo: cfg.repo.clone().unwrap_or_default(),
    }
}

/// publish 會用到的所有標籤：`release-triage`、每個 kind 的 `upstream:<kind>`、`triage:guard`／`triage:adopt`。
/// `gh issue create --label` 對不存在的標籤是**硬失敗**，所以「auth 綠」不等於「開得出 issue」——
/// 少一個，第一次 publish 就會整版停在 `judged`。
pub fn required_labels() -> Vec<String> {
    let mut v = vec!["release-triage".to_string()];
    for k in crate::release_triage::rules::KINDS {
        v.push(format!("upstream:{k}"));
    }
    v.push("triage:guard".into());
    v.push("triage:adopt".into());
    v
}

/// gh 的唯讀健檢：`auth status` → `repo view`（設定的那個 repo 真的看得到、權限寫得進去嗎）
/// → `label list`（少標籤 `issue create` 會硬失敗）。四個欄位都填好回去，任何一步失敗就停在那一步。
async fn readonly_checks(gh: &Gh) -> serde_json::Value {
    let auth = gh.run(&["auth", "status"]).await;
    let mut out = serde_json::json!({
        "repo": gh.repo,
        "gh_auth_ok": auth.is_ok(),
        "gh_auth_error": auth.as_ref().err().cloned(),
        "repo_ok": false,
        "repo_error": serde_json::Value::Null,
        "viewer_permission": serde_json::Value::Null,
        "can_write": false,
        "issues_enabled": serde_json::Value::Null,
        "labels_missing": serde_json::Value::Null,
    });
    if auth.is_err() {
        return out;
    }
    if gh.repo.trim().is_empty() {
        out["repo_error"] = serde_json::json!("[release_triage] repo 沒設定（owner/name）");
        return out;
    }
    match gh.run(&["repo", "view", &gh.repo, "--json", "nameWithOwner,viewerPermission,hasIssuesEnabled"]).await {
        Err(e) => {
            out["repo_error"] = serde_json::json!(e);
            return out;
        }
        Ok(o) => match serde_json::from_str::<serde_json::Value>(o.trim()) {
            Err(e) => {
                out["repo_error"] = serde_json::json!(format!("gh repo view 回的不是 JSON：{e}"));
                return out;
            }
            Ok(v) => {
                let perm = v.get("viewerPermission").and_then(|p| p.as_str()).unwrap_or_default().to_string();
                out["repo_ok"] = serde_json::json!(true);
                out["can_write"] = serde_json::json!(matches!(perm.as_str(), "ADMIN" | "MAINTAIN" | "WRITE" | "TRIAGE"));
                out["viewer_permission"] = serde_json::json!(perm);
                out["issues_enabled"] = v.get("hasIssuesEnabled").cloned().unwrap_or(serde_json::Value::Null);
            }
        },
    }
    match gh.run(&["label", "list", "--repo", &gh.repo, "--json", "name", "-L", "200"]).await {
        Err(e) => out["repo_error"] = serde_json::json!(e),
        Ok(o) => match serde_json::from_str::<Vec<serde_json::Value>>(o.trim()) {
            Err(e) => out["repo_error"] = serde_json::json!(format!("gh label list 回的不是 JSON：{e}")),
            Ok(list) => {
                let have: Vec<&str> = list.iter().filter_map(|v| v.get("name").and_then(|n| n.as_str())).collect();
                let missing: Vec<String> = required_labels().into_iter().filter(|l| !have.contains(&l.as_str())).collect();
                out["labels_missing"] = serde_json::json!(missing);
            }
        },
    }
    out
}

/// `/api/supervisor/health` 的 `release_triage` 一格：`gh auth status` 失敗要看得到原因，
/// 不是等到有版本要開 issue 才在帳本的 `publish_error` 裡發現。`publish = false` 時不碰 gh、回 `None`。
/// auth 綠之後還要 repo 看得到、權限寫得進去、標籤齊——這三樣任何一樣不對，publish 一樣開不出 issue。
pub async fn health_probe(cfg: &ReleaseTriageCfg) -> Option<serde_json::Value> {
    if !cfg.publish {
        return None;
    }
    Some(readonly_checks(&gh_for(cfg)).await)
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

/// 標題＝`<kind> <version>: <一句話>（提防｜採用）`，前綴由 daemon 貼。
/// 模型常常自己也把 `claude 2.1.280:` 寫進 `title`（task.md 只要求「一句話」，但 2026-09-24 的正式帳本上
/// 8 個提案 8 個都這樣，其中一個還用全形冒號），照貼會變成 `claude 2.1.280: claude 2.1.280: …`——
/// 所以同一版的重複前綴在這裡剝掉（別版的不剝，那是模型真的在講另一版）。
pub fn title(kind: &str, version: &str, p: &StoredProposal) -> String {
    let prefix = format!("{kind} {version}");
    let one_line = p
        .title
        .trim()
        .strip_prefix(&prefix)
        .map(|rest| rest.trim_start().trim_start_matches([':', '：']).trim_start())
        .filter(|rest| !rest.is_empty())
        .unwrap_or_else(|| p.title.trim());
    format!("{prefix}: {one_line}（{}）", if p.triage == "guard" { "提防" } else { "採用" })
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

/// 帳本（含這一輪剛開的）已經有這個提案任一 entry 的 issue。**同一版兩個提案的 `entry_ids` 有交集時
/// 只會處理第一個**——`proposals_of` 只是把模型交來的 `verdicts.issues` 反序列化，沒有任何地方保證不重疊。
/// publish 與乾跑共用這一份，而且兩邊都要餵「會長大的」清單，否則預覽說 2 張、實際開 1 張（#204 review）。
fn already(p: &StoredProposal, issues: &[IssueRef]) -> bool {
    issues.iter().any(|i| i.entry_ids.iter().any(|id| p.entry_ids.contains(id)))
}

/// 每版／每日上限的即時計數。`publish_version` 與 [`preflight`] 共用，排序與上限的判斷只有一份。
struct Caps {
    /// 這一版已經**新開**幾張（`comment` 的不算，同 `publish_version` 原本的 `in_row`）。
    in_row: usize,
    /// 24 小時內已新開幾張（全部 kind、全部版本）。
    created_today: usize,
    /// 已經撞到 24 小時上限：`publish_version` 撞到就 `break`，所以之後一律 deferred。
    deferred_hit: bool,
}

impl Caps {
    /// 遠端查完之後該做什麼。`already` 要在呼叫這裡**之前**先擋掉（那一步不必問 gh）。
    fn plan(&self, p: &StoredProposal, found: Option<&(i64, String)>) -> PlanAction {
        if found.is_some() {
            return PlanAction::Existing;
        }
        // `duplicate_of` 只留言，不佔每版／每日的名額。
        if p.duplicate_of.is_some() {
            return PlanAction::Comment;
        }
        if self.deferred_hit {
            return PlanAction::DeferredDailyLimit;
        }
        if self.in_row >= MAX_PER_VERSION {
            return PlanAction::SkippedVersionLimit;
        }
        if self.created_today >= MAX_PER_DAY {
            return PlanAction::DeferredDailyLimit;
        }
        PlanAction::Create
    }

    fn note(&mut self, action: PlanAction) {
        match action {
            PlanAction::Create => {
                self.in_row += 1;
                self.created_today += 1;
            }
            PlanAction::DeferredDailyLimit => self.deferred_hit = true,
            _ => {}
        }
    }
}

/// 遠端已經有的 release-triage issue，一次抓完之後在本地比對。
///
/// **不走 `--search`**：那是 GitHub 的非同步搜尋索引，剛建立的 issue 要等一段時間才搜得到。
/// `gh issue create` 逾時（子行程雖然已加 `kill_on_drop`，仍可能在被殺之前就建好）或行程在
/// 寫回帳本之前掛掉時，重試就會因為「搜不到」而再開一張（#456）。`--label` 走的是 repo 的
/// issues 列表，對剛建立的 issue **立即一致**，而 `release-triage` 這個標籤是我們每次開 issue
/// 都一定會帶的（見 `publish_version` 的 `labels`）。`--state all`：已關掉的**不復活**。
struct Remote {
    listed: Vec<serde_json::Value>,
}

impl Remote {
    async fn load(gh: &Gh) -> Result<Self, String> {
        let out = gh
            .run(&["issue", "list", "--repo", &gh.repo, "--state", "all", "--label", "release-triage", "--json", "number,url,title,body", "-L", "200"])
            .await?;
        let listed: Vec<serde_json::Value> =
            serde_json::from_str(out.trim()).map_err(|e| format!("gh issue list 回的不是 JSON：{e}"))?;
        Ok(Self { listed })
    }

    /// 這個 marker（或這個標題）在遠端有沒有對應的 issue。
    ///
    /// 標題只在**那張 issue 的內文完全沒有 release-triage 標記**時才採用——內文被人編輯掉、
    /// 標記跟著不見的情況下還認得出來。不無條件用標題比對：同一版兩個提案的標題有可能撞在一起，
    /// 那樣會把第二張該開的 issue 誤判成已經存在。
    fn find(&self, mk: &str, title: &str) -> Option<(i64, String)> {
        let needle = format!("release-triage: {mk} -->");
        let pick = |v: &serde_json::Value| Some((v.get("number")?.as_i64()?, v.get("url")?.as_str()?.to_string()));
        let body_of = |v: &serde_json::Value| v.get("body").and_then(|b| b.as_str()).unwrap_or_default().to_string();
        if let Some(v) = self.listed.iter().find(|v| body_of(v).contains(&needle)) {
            return pick(v);
        }
        self.listed
            .iter()
            .find(|v| {
                v.get("title").and_then(|t| t.as_str()) == Some(title) && !body_of(v).contains(MARKER_PREFIX)
            })
            .and_then(pick)
    }
}

/// issue 內文結尾隱藏標記的前綴；`Remote::find` 用它判斷「內文還有沒有標記」。
const MARKER_PREFIX: &str = "release-triage:";

/// 遠端有沒有帶同一個隱藏標記的 issue。publish 與乾跑共用，兩邊的去重結論才不會漂。
/// 先看已經抓下來的列表（立即一致）；沒中才退回搜尋索引當補網——`--label` 被人拿掉、
/// 或 release-triage 的 issue 多到超過 `-L 200` 時還接得住。搜尋只會**多**認出東西，
/// 不會讓「其實有」變成「沒有」，所以加上它只有好處。
async fn find_existing(gh: &Gh, remote: &Remote, mk: &str, title: &str) -> Result<Option<(i64, String)>, String> {
    if let Some(hit) = remote.find(mk, title) {
        return Ok(Some(hit));
    }
    let search = format!("release-triage: {mk} in:body");
    let listed = gh
        .run(&["issue", "list", "--repo", &gh.repo, "--state", "all", "--search", &search, "--json", "number,url,body", "-L", "20"])
        .await?;
    let needle = format!("release-triage: {mk} -->");
    let parsed: Vec<serde_json::Value> =
        serde_json::from_str(listed.trim()).map_err(|e| format!("gh issue list 回的不是 JSON：{e}"))?;
    Ok(parsed
        .iter()
        .find(|v| v.get("body").and_then(|b| b.as_str()).is_some_and(|b| b.contains(&needle)))
        .and_then(|v| Some((v.get("number")?.as_i64()?, v.get("url")?.as_str()?.to_string()))))
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
    let gh = Gh { repo, ..gh_for(cfg) };
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
    if proposals.iter().all(|p| already(p, &issues)) {
        ledger::save_publish(pool, kind, version, &issues, Status::Published, None).await?;
        return Ok(Outcome::Published { created: 0, commented: 0, existing: 0, skipped: vec![] });
    }
    if let Err(e) = gh.run(&["auth", "status"]).await {
        return record_err(&issues, e).await;
    }
    // 遠端已有的 release-triage issue 抓一次就好（立即一致的列表，不是搜尋索引）。這一輪自己開出來的
    // 不必進這份快取：每開一張立刻寫回帳本，下一個提案由 `already` 擋掉。
    let remote = match Remote::load(&gh).await {
        Ok(r) => r,
        Err(e) => return record_err(&issues, e).await,
    };

    let (mut created, mut commented, mut existing) = (0usize, 0usize, 0usize);
    let mut skipped: Vec<String> = Vec::new();
    let mut deferred: Option<String> = None;
    // 上限用計數器而不是每輪重查：只有 `Create` 會增加「非 comment」的張數，跟原本每輪
    // `issues.iter().filter(!comment).count()` ＋ `created_in_last_day` 等價，而且與乾跑共用同一份判斷。
    let mut caps = Caps {
        in_row: issues.iter().filter(|i| !i.comment).count(),
        created_today: ledger::created_in_last_day(pool).await?,
        deferred_hit: false,
    };
    for p in &proposals {
        if already(p, &issues) {
            continue;
        }
        let mk = marker(kind, version, &p.entry_ids);
        // (a) 帳本查完了；(b) 遠端標記（`--state all`，已關的不復活）——乾跑走同一個函式。
        let found = match find_existing(&gh, &remote, &mk, &title(kind, version, p)).await {
            Ok(f) => f,
            Err(e) => return record_err(&issues, e).await,
        };
        // 排序、上限、`duplicate_of` 不佔名額的判斷全在 `Caps::plan`，乾跑叫的是同一份。
        let action = caps.plan(p, found.as_ref());
        caps.note(action);
        match action {
            PlanAction::Existing => {
                let (number, url) = found.expect("Existing 就是查到了");
                issues.push(IssueRef { marker: mk, entry_ids: p.entry_ids.clone(), number, url, created_at: ledger::now_ts(), comment: true });
                ledger::save_publish(pool, kind, version, &issues, Status::Judged, None).await?;
                existing += 1;
                continue;
            }
            PlanAction::Comment => {
                let dup = p.duplicate_of.expect("Comment 就是有 duplicate_of");
                let body = render_comment(kind, version, &row.entries, p);
                if let Err(e) = gh.run(&["issue", "comment", &dup.to_string(), "--repo", &gh.repo, "--body", &body]).await {
                    return record_err(&issues, e).await;
                }
                issues.push(IssueRef { marker: mk, entry_ids: p.entry_ids.clone(), number: dup, url: String::new(), created_at: ledger::now_ts(), comment: true });
                ledger::save_publish(pool, kind, version, &issues, Status::Judged, None).await?;
                commented += 1;
                continue;
            }
            PlanAction::SkippedVersionLimit => {
                skipped.push(mk);
                continue;
            }
            PlanAction::DeferredDailyLimit => {
                deferred = Some(format!("24 小時內已開 {MAX_PER_DAY} 張，{mk} 留待下一輪"));
                break;
            }
            // `already` 在迴圈開頭擋掉了；`RemoteUnknown` 只有乾跑會用（這裡查不到就已經 record_err 回去）。
            PlanAction::AlreadyLogged | PlanAction::RemoteUnknown => continue,
            PlanAction::Create => {}
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

// ───────────────────────── 乾跑（preflight） ─────────────────────────

/// 一個提案在乾跑裡的結論。`create`／`comment` 是「真的跑 publish 會做的事」，其餘是不會做的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    /// 會開一張新的。
    Create,
    /// `duplicate_of`：只到那張 issue 留言。
    Comment,
    /// 遠端已經有同一個標記（含已關的），只會記進帳本、不開。
    Existing,
    /// 帳本裡已經有這個 entry 的 issue，連 gh 都不會問。
    AlreadyLogged,
    /// 被每版 [`MAX_PER_VERSION`] 張擋下，之後也不會再開。
    SkippedVersionLimit,
    /// 被 24 小時 [`MAX_PER_DAY`] 張擋下，留待下一輪。
    DeferredDailyLimit,
    /// gh 檢查沒過，去重問不到遠端，所以只知道帳本裡還沒有。
    RemoteUnknown,
}

impl PlanAction {
    fn as_str(self) -> &'static str {
        match self {
            PlanAction::Create => "create",
            PlanAction::Comment => "comment",
            PlanAction::Existing => "existing",
            PlanAction::AlreadyLogged => "already_logged",
            PlanAction::SkippedVersionLimit => "skipped_version_limit",
            PlanAction::DeferredDailyLimit => "deferred_daily_limit",
            PlanAction::RemoteUnknown => "remote_unknown",
        }
    }
    /// 真的跑 publish 時會寫到 GitHub 的兩種。
    fn writes(self) -> bool {
        matches!(self, PlanAction::Create | PlanAction::Comment)
    }
}

/// 打開 `publish` 之前先看會發生什麼：**只讀**，一張 issue 都不開、帳本一個字都不寫。
///
/// - `publish` 是不是 true 都跑（唯一在 `publish = false` 時碰 gh 的路徑，由人明確觸發）。
/// - gh 檢查（auth／repo／標籤）過了才問遠端去重；沒過就照樣把 title／body 渲染出來，
///   讓人先看 verdict 品質，去重那欄記成 `remote_unknown`。
/// - 每版 4 張、24 小時 8 張的上限用跟 [`publish_version`] 同一組常數與同一個順序模擬。
pub async fn preflight(pool: &SqlitePool, cfg: &ReleaseTriageCfg, kind: Option<&str>, version: Option<&str>) -> Result<serde_json::Value> {
    let gh = gh_for(cfg);
    let mut checks = readonly_checks(&gh).await;
    // 門檻要跟 publish_version 一樣：它只跑 `gh auth status`（repo 沒設時提早回錯、不碰 gh），
    // 不查 repo view。乾跑若額外要求 repo_ok，repo 有問題時乾跑全報 remote_unknown、真跑照樣開（#204 review）。
    // repo 的問題仍然照實記在 `checks`，而且真的查不到時 find_existing 會回錯、落成 remote_unknown。
    let can_ask = checks["gh_auth_ok"] == serde_json::json!(true) && !gh.repo.trim().is_empty();
    // 遠端已有的 release-triage issue 只抓一次（立即一致的列表，不是搜尋索引），而且**真的需要問**
    // 才抓：全部提案都已經在帳本裡時一次 gh 都不叫（同 publish_version 的提早返回）。
    let mut remote: Option<Remote> = None;
    let mut remote_err: Option<String> = None;

    // **24 小時上限是跨版本的**：真跑每呼叫一次 publish_version 就重讀一次帳本，所以第 2 版看得到第 1 版
    // 剛開的那幾張。乾跑若每版都用同一個初始值重開 Caps，3 版以上就會說「每版都能開 4 張」（共 12），
    // 真跑第 9 張起 deferred（#440）。所以 `created_today` 與 `deferred_hit` 跨版留著，只有 `in_row` 每版重置。
    let mut caps = Caps { in_row: 0, created_today: ledger::created_in_last_day(pool).await?, deferred_hit: false };
    let mut versions = Vec::new();
    let (mut n_create, mut n_comment, mut n_existing, mut n_blocked) = (0usize, 0usize, 0usize, 0usize);
    for row in ledger::list(pool, kind, version).await?.into_iter().filter(|r| r.status == Status::Judged) {
        let mut proposals = proposals_of(&row);
        proposals.sort_by_key(|p| p.triage != "guard");
        // **會長大的清單**（同 publish_version）：existing／comment／create 都要 push 進去，
        // 否則同一版兩個提案的 entry_ids 有交集時，真跑跳過第二個、乾跑兩個都算 create（#204 review）。
        let mut issues = row.issues.clone();
        // 每版重置的只有「這一版開了幾張」。
        caps.in_row = issues.iter().filter(|i| !i.comment).count();
        let mut plans = Vec::new();
        for p in &proposals {
            let mk = marker(&row.kind, &row.version, &p.entry_ids);
            let mut plan = serde_json::json!({
                "marker": mk.clone(),
                "triage": p.triage,
                "entry_ids": p.entry_ids,
                "title": title(&row.kind, &row.version, p),
                "body": render_body(&row.kind, &row.version, &row.entries, p),
                "labels": ["release-triage".to_string(), format!("upstream:{}", row.kind), format!("triage:{}", p.triage)],
            });
            if can_ask && remote.is_none() && remote_err.is_none() && !already(p, &issues) {
                match Remote::load(&gh).await {
                    Ok(r) => remote = Some(r),
                    Err(e) => remote_err = Some(e),
                }
            }
            let action = if already(p, &issues) {
                PlanAction::AlreadyLogged
            } else if !can_ask {
                PlanAction::RemoteUnknown
            } else if let Some(e) = remote_err.clone() {
                plan["error"] = serde_json::json!(e);
                PlanAction::RemoteUnknown
            } else {
                let remote = remote.as_ref().expect("load 成功才會到這裡");
                match find_existing(&gh, remote, &mk, &title(&row.kind, &row.version, p)).await {
                    Err(e) => {
                        plan["error"] = serde_json::json!(e);
                        PlanAction::RemoteUnknown
                    }
                    Ok(found) => {
                        let a = caps.plan(p, found.as_ref());
                        match (&a, &found) {
                            (PlanAction::Existing, Some((number, url))) => {
                                plan["number"] = serde_json::json!(number);
                                plan["url"] = serde_json::json!(url);
                            }
                            (PlanAction::Comment, _) => plan["number"] = serde_json::json!(p.duplicate_of),
                            _ => {}
                        }
                        a
                    }
                }
            };
            caps.note(action);
            // 真跑會把 existing／comment／create 都寫進帳本的 issue 清單，下一個提案的 `already` 看得到它。
            if matches!(action, PlanAction::Existing | PlanAction::Comment | PlanAction::Create) {
                issues.push(IssueRef {
                    marker: mk.clone(),
                    entry_ids: p.entry_ids.clone(),
                    number: plan["number"].as_i64().unwrap_or(0),
                    url: String::new(),
                    created_at: ledger::now_ts(),
                    comment: action != PlanAction::Create,
                });
            }
            match action {
                PlanAction::Create => n_create += 1,
                PlanAction::Comment => n_comment += 1,
                PlanAction::Existing => n_existing += 1,
                PlanAction::SkippedVersionLimit | PlanAction::DeferredDailyLimit => n_blocked += 1,
                _ => {}
            }
            // `duplicate_of` 只留言，不佔每版／每日的名額（同 publish_version）。
            plan["action"] = serde_json::json!(action.as_str());
            plan["writes"] = serde_json::json!(action.writes());
            plans.push(plan);
        }
        versions.push(serde_json::json!({"kind": row.kind, "version": row.version, "proposals": plans}));
    }
    if let Some(e) = remote_err {
        checks["remote_list_error"] = serde_json::json!(e);
    }
    Ok(serde_json::json!({
        "dry_run": true,
        "publish_enabled": cfg.publish,
        "checks": checks,
        "would_create": n_create,
        "would_comment": n_comment,
        "existing": n_existing,
        "blocked_by_caps": n_blocked,
        "versions": versions,
    }))
}
