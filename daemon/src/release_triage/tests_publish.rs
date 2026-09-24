//! B：verdict 驗證、issue 渲染、gh publish（假 gh 腳本，絕不打真的 GitHub）。

use super::issue::{self, Outcome};
use super::ledger::{self, Status};
use super::verdict::{self, EntryVerdict, Proposal, StoredProposal, Submission, Verdict};
use super::*;
use crate::config::ReleaseTriageCfg;
use crate::changelog::Section;
use serde_json::json;

const CLAUDE_MD: &str = include_str!("fixtures/claude_2.1.276-278.md");

async fn pool() -> SqlitePool {
    let p = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    ledger::migrate(&p).await.unwrap();
    p
}

fn entries277() -> Vec<Entry> {
    let all: Vec<Section> = source_sections("claude", CLAUDE_MD);
    build_entries("claude", all.iter().find(|s| s.version == "2.1.277").unwrap()).unwrap()
}

fn judged(es: &[Entry]) -> Vec<&Entry> {
    es.iter().filter(|e| e.bucket != Bucket::Dropped).collect()
}

fn verdicts_all(es: &[Entry], v: Verdict) -> Vec<EntryVerdict> {
    judged(es).iter().map(|e| EntryVerdict { entry_id: e.id.clone(), verdict: v, reason: "r".into(), module: "m".into() }).collect()
}

fn proposal(ids: &[&str]) -> Proposal {
    Proposal {
        entry_ids: ids.iter().map(|s| s.to_string()).collect(),
        verdict: None,
        title: "把鎖補上".into(),
        goal: "目標文字".into(),
        suggestion: "建議文字".into(),
        acceptance: "驗收文字".into(),
        duplicate_of: None,
    }
}

fn sub(es: &[Entry], v: Verdict, issues: Vec<Proposal>) -> Submission {
    Submission { kind: "claude".into(), version: "2.1.277".into(), verdicts: verdicts_all(es, v), issues }
}

#[test]
fn validation_accepts_a_complete_submission_and_derives_the_triage_kind() {
    let es = entries277();
    let ids: Vec<String> = judged(&es).iter().take(2).map(|e| e.id.clone()).collect();
    let mut s = sub(&es, Verdict::None, vec![]);
    s.verdicts[0].verdict = Verdict::Adopt;
    s.verdicts[1].verdict = Verdict::Guard;
    s.issues = vec![proposal(&[&ids[0], &ids[1]])];
    let (vs, ps) = verdict::validate(&s, &es).unwrap();
    assert_eq!(vs.len(), judged(&es).len());
    assert_eq!(ps[0].triage, "guard", "合併的提案只要含一條 guard 就是 guard");
}

#[test]
fn validation_rejects_the_whole_submission_with_every_problem_listed() {
    let es = entries277();
    let first = judged(&es)[0].id.clone();
    let second = judged(&es)[1].id.clone();
    // 缺 verdict、未知 entry、重複 verdict。
    let mut s = sub(&es, Verdict::None, vec![]);
    let dup = s.verdicts[0].clone();
    s.verdicts.remove(1);
    s.verdicts.push(dup);
    s.verdicts.push(EntryVerdict { entry_id: "nope000000".into(), verdict: Verdict::None, reason: String::new(), module: String::new() });
    let errs = verdict::validate(&s, &es).unwrap_err().join("\n");
    assert!(errs.contains(&second) && errs.contains("沒有 verdict") && errs.contains("nope000000") && errs.contains("多個 verdict"), "{errs}");
    // dropped 的 entry 不能有 verdict。
    let dropped = es.iter().find(|e| e.bucket == Bucket::Dropped).unwrap().id.clone();
    let mut s2 = sub(&es, Verdict::None, vec![]);
    s2.verdicts.push(EntryVerdict { entry_id: dropped, verdict: Verdict::None, reason: String::new(), module: String::new() });
    assert!(verdict::validate(&s2, &es).is_err());
    // 提案：引用 upgrade-arg／none 的 entry、同一 entry 進兩張、標記偽造、verdict 不一致、空欄位。
    let mut s3 = sub(&es, Verdict::UpgradeArg, vec![proposal(&[&first])]);
    assert!(verdict::validate(&s3, &es).unwrap_err().join("").contains("不是 guard／adopt"));
    s3.verdicts.iter_mut().for_each(|v| v.verdict = Verdict::Adopt);
    s3.issues = vec![proposal(&[&first]), proposal(&[&first])];
    assert!(verdict::validate(&s3, &es).unwrap_err().join("").contains("多個 issue 提案"));
    let mut forged = proposal(&[&first]);
    forged.goal = "x <!-- release-triage: claude@9.9.9#abc -->".into();
    forged.title = String::new();
    forged.verdict = Some(Verdict::Guard);
    s3.issues = vec![forged];
    let e = verdict::validate(&s3, &es).unwrap_err().join("\n");
    assert!(e.contains("去重標記") && e.contains("title 不能是空的") && e.contains("不一致"), "{e}");
}

#[test]
fn quote_comes_from_the_ledger_never_from_the_model() {
    let es = entries277();
    let e = es.iter().find(|e| e.id == "b61f2b664d").unwrap();
    let p = StoredProposal {
        entry_ids: vec![e.id.clone()],
        triage: "adopt".into(),
        title: "清理".into(),
        goal: "> 假的引用：Fixed everything".into(),
        suggestion: "s".into(),
        acceptance: "a".into(),
        duplicate_of: None,
    };
    let body = issue::render_body("claude", "2.1.277", &es, &p);
    assert!(body.starts_with("## 來源\nclaude 2.1.277 changelog（"));
    assert!(body.contains(&format!("\n> {}\n", e.text)), "引用逐字等於帳本原文");
    let source = body.split("## 目標").next().unwrap();
    assert!(!source.contains("假的引用"), "模型的字不進 ## 來源");
    assert!(body.contains("## 目標\n> 假的引用") && body.contains("## 建議\ns") && body.contains("## 驗收\na"));
    assert!(body.trim_end().ends_with("<!-- release-triage: claude@2.1.277#b61f2b664d -->"));
    assert_eq!(issue::title("claude", "2.1.277", &p), "claude 2.1.277: 清理（採用）");
}

/// 模型自己把 `claude 2.1.277:` 也寫進 title 時（真實資料裡 6 個提案有 5 個這樣），
/// 標題不能變成 `claude 2.1.277: claude 2.1.277: …`。全形冒號、多餘空白、只有前綴沒有句子都要處理。
#[test]
fn a_title_that_already_carries_the_version_prefix_is_not_doubled() {
    let mk = |t: &str| StoredProposal {
        entry_ids: vec!["b61f2b664d".into()],
        triage: "guard".into(),
        title: t.into(),
        goal: "g".into(),
        suggestion: "s".into(),
        acceptance: "a".into(),
        duplicate_of: None,
    };
    let t = |s: &str| issue::title("claude", "2.1.277", &mk(s));
    assert_eq!(t("claude 2.1.277: resume 會多起一個回合"), "claude 2.1.277: resume 會多起一個回合（提防）");
    assert_eq!(t("claude 2.1.277：resume 會多起一個回合"), "claude 2.1.277: resume 會多起一個回合（提防）");
    assert_eq!(t("  claude 2.1.277:  resume 會多起一個回合 "), "claude 2.1.277: resume 會多起一個回合（提防）");
    assert_eq!(t("resume 會多起一個回合"), "claude 2.1.277: resume 會多起一個回合（提防）", "沒有前綴的照舊");
    assert_eq!(t("claude 2.1.277"), "claude 2.1.277: claude 2.1.277（提防）", "只有前綴沒有句子：不吃掉，讓人看得出模型沒寫");
    assert_eq!(t("claude 2.1.278: 另一版的標題"), "claude 2.1.277: claude 2.1.278: 另一版的標題（提防）", "別版的前綴不剝");
}

// ───────────────────────── 假 gh ─────────────────────────

struct FakeGh {
    dir: std::path::PathBuf,
}

impl FakeGh {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("am-fakegh-{tag}-{}-{}", std::process::id(), ledger::now_ts().replace(':', "")));
        std::fs::create_dir_all(&dir).unwrap();
        let script = r#"#!/bin/sh
D=$(dirname "$0")
echo "$1 $2" >> "$D/calls.log"
case "$1 $2" in
  "auth status") [ -f "$D/fail_auth" ] && { echo "not logged in" >&2; exit 1; }; exit 0;;
  "repo view")
    [ -f "$D/fail_repo" ] && { echo "Could not resolve to a Repository" >&2; exit 1; }
    cat "$D/repo.json" 2>/dev/null || echo '{"nameWithOwner":"o/r","viewerPermission":"ADMIN","hasIssuesEnabled":true}';;
  "label list") cat "$D/labels.json" 2>/dev/null || echo '[{"name":"release-triage"},{"name":"upstream:claude"},{"name":"upstream:codex"},{"name":"triage:guard"},{"name":"triage:adopt"}]';;
  "issue list") if [ -f "$D/list.json" ]; then cat "$D/list.json"; else echo "[]"; fi;;
  "issue create")
    [ -f "$D/fail_create" ] && { echo "API rate limit exceeded" >&2; exit 1; }
    printf '%s\n' "$@" > "$D/last_create.txt"
    n=$(cat "$D/n" 2>/dev/null || echo 100); n=$((n+1)); echo $n > "$D/n"
    echo "https://github.com/o/r/issues/$n";;
  "issue comment") printf '%s\n' "$@" > "$D/last_comment.txt"; exit 0;;
  *) exit 2;;
esac
"#;
        let p = dir.join("gh");
        std::fs::write(&p, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }
    fn cfg(&self, publish: bool) -> ReleaseTriageCfg {
        ReleaseTriageCfg { publish, gh_bin: Some(self.dir.join("gh").display().to_string()), repo: Some("o/r".into()) }
    }
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls.log")).map(|s| s.lines().map(String::from).collect()).unwrap_or_default()
    }
    fn count(&self, what: &str) -> usize {
        self.calls().iter().filter(|c| *c == what).count()
    }
    fn flag(&self, name: &str, on: bool) {
        let p = self.dir.join(name);
        if on {
            std::fs::write(p, "1").unwrap();
        } else {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 帳本裡放一個 judged 的 2.1.277，提案由 `(triage, [entry index])` 決定（index 指 kept／unmatched 的第幾條）。
async fn seed(p: &SqlitePool, props: &[(&str, &[usize], Option<i64>)]) -> Vec<Entry> {
    let es = entries277();
    ledger::insert_version(p, "claude", "2.1.277", &es).await.unwrap();
    let j = judged(&es);
    let stored: Vec<StoredProposal> = props
        .iter()
        .enumerate()
        .map(|(n, (triage, idx, dup))| StoredProposal {
            entry_ids: idx.iter().map(|i| j[*i].id.clone()).collect(),
            triage: triage.to_string(),
            title: format!("提案{n}"),
            goal: "g".into(),
            suggestion: "s".into(),
            acceptance: "a".into(),
            duplicate_of: *dup,
        })
        .collect();
    let v = serde_json::json!({"verdicts": [], "issues": stored});
    assert!(ledger::save_verdicts(p, "claude", "2.1.277", &v, Status::Judged).await.unwrap());
    es
}

async fn row(p: &SqlitePool) -> ledger::Row {
    ledger::get(p, "claude", "2.1.277").await.unwrap().unwrap()
}

#[tokio::test]
async fn publish_false_never_starts_gh() {
    let p = pool().await;
    let gh = FakeGh::new("off");
    seed(&p, &[("guard", &[0], None)]).await;
    let o = issue::publish_version(&p, &gh.cfg(false), "claude", "2.1.277").await.unwrap();
    assert_eq!(o, Outcome::Disabled);
    assert!(gh.calls().is_empty(), "publish=false 時完全沒叫 gh：{:?}", gh.calls());
    let r = row(&p).await;
    assert_eq!((r.status, r.issues.len()), (Status::Judged, 0));
}

#[tokio::test]
async fn running_twice_opens_exactly_one_issue_with_labels_and_marker() {
    let p = pool().await;
    let gh = FakeGh::new("twice");
    let es = seed(&p, &[("guard", &[0], None)]).await;
    let cfg = gh.cfg(true);
    let first = issue::publish_version(&p, &cfg, "claude", "2.1.277").await.unwrap();
    assert!(matches!(first, Outcome::Published { created: 1, .. }), "{first:?}");
    assert_eq!(row(&p).await.status, Status::Published);
    // 第二次：狀態已是 published → 不該再進 publish（呼叫端只重試 judged）；直接呼叫也不會多開。
    assert!(issue::publish_version(&p, &cfg, "claude", "2.1.277").await.is_err());
    assert_eq!(gh.count("issue create"), 1);
    let args = std::fs::read_to_string(gh.dir.join("last_create.txt")).unwrap();
    for want in ["--label\nrelease-triage", "--label\nupstream:claude", "--label\ntriage:guard", "--repo\no/r"] {
        assert!(args.contains(want), "缺 {want}：{args}");
    }
    assert!(args.contains(&format!("release-triage: claude@2.1.277#{} -->", judged(&es)[0].id)));
    assert!(args.contains("（提防）"));
    let r = row(&p).await;
    assert_eq!((r.issues.len(), r.issues[0].number, r.issues[0].comment), (1, 101, false));
}

#[tokio::test]
async fn a_closed_issue_is_never_reopened_even_when_the_ledger_forgot_it() {
    let p = pool().await;
    let gh = FakeGh::new("closed");
    let es = seed(&p, &[("adopt", &[0], None)]).await;
    let mk = issue::marker("claude", "2.1.277", &[judged(&es)[0].id.clone()]);
    std::fs::write(
        gh.dir.join("list.json"),
        serde_json::json!([
            {"number": 7, "url": "https://github.com/o/r/issues/7", "body": "unrelated <!-- release-triage: claude@2.1.277#zzzzzzzzzz -->"},
            {"number": 42, "url": "https://github.com/o/r/issues/42", "body": format!("x\n<!-- release-triage: {mk} -->\n")}
        ])
        .to_string(),
    )
    .unwrap();
    let o = issue::publish_version(&p, &gh.cfg(true), "claude", "2.1.277").await.unwrap();
    assert!(matches!(o, Outcome::Published { created: 0, existing: 1, .. }), "{o:?}");
    assert_eq!(gh.count("issue create"), 0);
    assert!(gh.calls().iter().any(|c| c == "issue list"));
    let r = row(&p).await;
    assert_eq!((r.status, r.issues[0].number), (Status::Published, 42));
}

#[tokio::test]
async fn a_non_json_issue_list_is_recorded_as_a_publish_error_not_thrown() {
    let p = pool().await;
    let gh = FakeGh::new("notjson");
    seed(&p, &[("adopt", &[0], None)]).await;
    std::fs::write(gh.dir.join("list.json"), "<html>502 Bad Gateway</html>").unwrap();
    let o = issue::publish_version(&p, &gh.cfg(true), "claude", "2.1.277").await.unwrap();
    assert!(matches!(&o, Outcome::Failed { error } if error.contains("不是 JSON")), "{o:?}");
    let r = row(&p).await;
    assert_eq!(r.status, Status::Judged);
    assert!(r.publish_error.unwrap().contains("不是 JSON"), "health 要看得到原因");
    assert_eq!(gh.count("issue create"), 0);
}

#[tokio::test]
async fn gh_failure_leaves_the_row_judged_and_the_retry_only_reruns_publish() {
    let p = pool().await;
    let gh = FakeGh::new("fail");
    seed(&p, &[("guard", &[0], None), ("adopt", &[1], None)]).await;
    let cfg = gh.cfg(true);
    gh.flag("fail_create", true);
    let o = issue::publish_version(&p, &cfg, "claude", "2.1.277").await.unwrap();
    assert!(matches!(&o, Outcome::Failed { error } if error.contains("rate limit")), "{o:?}");
    let r = row(&p).await;
    assert_eq!(r.status, Status::Judged);
    assert!(r.publish_error.unwrap().contains("rate limit"));
    assert!(r.verdicts.is_some(), "verdict 還在，不必重派模型");
    gh.flag("fail_create", false);
    let o = issue::publish_version(&p, &cfg, "claude", "2.1.277").await.unwrap();
    assert!(matches!(o, Outcome::Published { created: 2, .. }), "{o:?}");
    let r = row(&p).await;
    assert_eq!((r.status, r.publish_error, r.issues.len()), (Status::Published, None, 2));
    // auth 失敗同樣停在 judged。
    let p2 = pool().await;
    seed(&p2, &[("guard", &[0], None)]).await;
    gh.flag("fail_auth", true);
    assert!(matches!(issue::publish_version(&p2, &cfg, "claude", "2.1.277").await.unwrap(), Outcome::Failed { .. }));
    assert_eq!(row(&p2).await.status, Status::Judged);
}

#[tokio::test]
async fn caps_guard_first_four_per_version_and_eight_per_day() {
    // 每版 4：五張 adopt＋一張 guard（排最後）→ guard 一定開出來，最後一張 adopt 被擋。
    let p = pool().await;
    let gh = FakeGh::new("cap");
    seed(&p, &[("adopt", &[0], None), ("adopt", &[1], None), ("adopt", &[2], None), ("adopt", &[3], None), ("adopt", &[4], None), ("guard", &[5], None)]).await;
    let o = issue::publish_version(&p, &gh.cfg(true), "claude", "2.1.277").await.unwrap();
    let Outcome::Published { created, skipped, .. } = o else { panic!("{o:?}") };
    assert_eq!((created, skipped.len()), (4, 2));
    assert_eq!(gh.count("issue create"), 4);
    let r = row(&p).await;
    assert!(r.issues.iter().any(|i| i.entry_ids == [judged(&entries277())[5].id.clone()]), "guard 優先");
    assert!(r.publish_error.unwrap().contains("每版上限"));
    assert_eq!(r.status, Status::Published);

    // 每 24 小時 8：別的版本已經開了 8 張 → 這版留在 judged 待下一輪。
    let p = pool().await;
    let gh = FakeGh::new("day");
    seed(&p, &[("guard", &[0], None)]).await;
    let refs: Vec<ledger::IssueRef> = (0..8)
        .map(|n| ledger::IssueRef { marker: format!("claude@2.1.270#{n}"), entry_ids: vec![format!("e{n}")], number: n, url: String::new(), created_at: ledger::now_ts(), comment: false })
        .collect();
    ledger::insert_version(&p, "claude", "2.1.270", &entries277()).await.unwrap();
    sqlx::query("UPDATE release_triage SET status='judged', issue_numbers_json=? WHERE version='2.1.270'").bind(serde_json::to_string(&refs).unwrap()).execute(&p).await.unwrap();
    let o = issue::publish_version(&p, &gh.cfg(true), "claude", "2.1.277").await.unwrap();
    assert!(matches!(o, Outcome::Deferred { .. }), "{o:?}");
    assert_eq!((gh.count("issue create"), row(&p).await.status), (0, Status::Judged));
}

#[tokio::test]
async fn duplicate_of_only_comments_and_does_not_count_against_caps() {
    let p = pool().await;
    let gh = FakeGh::new("dup");
    seed(&p, &[("guard", &[0], Some(102))]).await;
    let o = issue::publish_version(&p, &gh.cfg(true), "claude", "2.1.277").await.unwrap();
    assert!(matches!(o, Outcome::Published { created: 0, commented: 1, .. }), "{o:?}");
    assert_eq!((gh.count("issue create"), gh.count("issue comment")), (0, 1));
    let args = std::fs::read_to_string(gh.dir.join("last_comment.txt")).unwrap();
    assert!(args.contains("102") && args.contains("--repo\no/r"));
    assert_eq!(ledger::created_in_last_day(&p).await.unwrap(), 0);
    let r = row(&p).await;
    assert_eq!((r.issues[0].number, r.issues[0].comment), (102, true));
}

#[tokio::test]
async fn repo_missing_is_a_recorded_failure_not_a_gh_call() {
    let p = pool().await;
    let gh = FakeGh::new("norepo");
    seed(&p, &[("guard", &[0], None)]).await;
    let mut cfg = gh.cfg(true);
    cfg.repo = None;
    assert!(matches!(issue::publish_version(&p, &cfg, "claude", "2.1.277").await.unwrap(), Outcome::Failed { .. }));
    assert!(gh.calls().is_empty());
}

#[tokio::test]
async fn health_probe_reports_gh_auth_failure_and_stays_silent_when_publish_is_off() {
    let gh = FakeGh::new("health");
    assert_eq!(issue::health_probe(&gh.cfg(false)).await, None);
    assert_eq!(gh.calls().len(), 0, "publish=false 不碰 gh");
    let ok = issue::health_probe(&gh.cfg(true)).await.unwrap();
    assert_eq!((&ok["gh_auth_ok"], &ok["repo_ok"], &ok["can_write"]), (&json!(true), &json!(true), &json!(true)));
    assert_eq!(ok["viewer_permission"], "ADMIN");
    assert_eq!(ok["labels_missing"], json!([]), "標籤齊");
    // auth 綠但 repo 看不到／權限只有 READ／標籤少一個——三種都會讓 `gh issue create` 失敗，健檢要分得出來。
    std::fs::write(gh.dir.join("repo.json"), r#"{"nameWithOwner":"o/r","viewerPermission":"READ","hasIssuesEnabled":false}"#).unwrap();
    std::fs::write(gh.dir.join("labels.json"), r#"[{"name":"release-triage"},{"name":"upstream:claude"}]"#).unwrap();
    let ro = issue::health_probe(&gh.cfg(true)).await.unwrap();
    assert_eq!((&ro["repo_ok"], &ro["can_write"], &ro["issues_enabled"]), (&json!(true), &json!(false), &json!(false)));
    assert_eq!(ro["labels_missing"], json!(["upstream:codex", "triage:guard", "triage:adopt"]));
    gh.flag("fail_repo", true);
    let nr = issue::health_probe(&gh.cfg(true)).await.unwrap();
    assert_eq!((&nr["gh_auth_ok"], &nr["repo_ok"]), (&json!(true), &json!(false)));
    assert!(nr["repo_error"].as_str().unwrap().contains("Could not resolve"), "{nr}");
    gh.flag("fail_auth", true);
    let bad = issue::health_probe(&gh.cfg(true)).await.unwrap();
    assert_eq!(bad["gh_auth_ok"], false);
    assert!(bad["gh_auth_error"].as_str().unwrap().contains("not logged in"));
    assert_eq!(bad["repo_ok"], false, "auth 沒過就停在那一步，不再問 repo");
}

// ───────────────────────── 乾跑（打開 publish 之前） ─────────────────────────

fn actions(v: &serde_json::Value) -> Vec<String> {
    v["versions"][0]["proposals"].as_array().unwrap().iter().map(|p| p["action"].as_str().unwrap().to_string()).collect()
}

/// #204 的 close condition：要能在 `publish = false` 的狀態下證明「這一版會開幾張、內文長什麼樣」，
/// 而且乾跑本身一張 issue 都不開、帳本一個字都不改。
#[tokio::test]
async fn a_dry_run_plans_the_issues_without_creating_any_or_touching_the_ledger() {
    let p = pool().await;
    let gh = FakeGh::new("dry");
    let es = seed(&p, &[("adopt", &[1], None), ("guard", &[0], None)]).await;
    let before = row(&p).await;
    let out = issue::preflight(&p, &gh.cfg(false), None, None).await.unwrap();
    assert_eq!(out["publish_enabled"], false, "publish 還是關著");
    assert_eq!((&out["would_create"], &out["would_comment"], &out["existing"]), (&json!(2), &json!(0), &json!(0)));
    assert_eq!(actions(&out), ["create", "create"]);
    // guard 優先：排序跟真的 publish 同一套。
    assert_eq!(out["versions"][0]["proposals"][0]["triage"], "guard");
    let first = &out["versions"][0]["proposals"][0];
    assert_eq!(first["title"], "claude 2.1.277: 提案1（提防）");
    assert!(first["body"].as_str().unwrap().contains(&format!("> {}", judged(&es)[0].text)), "內文先看得到引用");
    assert!(first["body"].as_str().unwrap().contains(&format!("release-triage: claude@2.1.277#{} -->", judged(&es)[0].id)));
    assert_eq!(first["labels"], json!(["release-triage", "upstream:claude", "triage:guard"]));
    // gh 只被讀過：auth／repo／label／issue list，沒有 create 也沒有 comment。
    assert_eq!((gh.count("issue create"), gh.count("issue comment")), (0, 0), "{:?}", gh.calls());
    assert!(gh.count("issue list") >= 2, "每個提案都真的問過遠端去重：{:?}", gh.calls());
    let after = row(&p).await;
    assert_eq!((after.status, after.issues.len(), after.publish_error.clone()), (before.status, 0, None));
    assert_eq!(after.updated_at, before.updated_at, "帳本一個字都沒動");
}

/// 遠端已經有同一個標記（含已關的）→ 乾跑就看得出「重跑不會開第二張」，不必真的開一張來試。
#[tokio::test]
async fn a_dry_run_reports_the_existing_remote_issue_instead_of_planning_a_duplicate() {
    let p = pool().await;
    let gh = FakeGh::new("dryexist");
    let es = seed(&p, &[("guard", &[0], None)]).await;
    let mk = issue::marker("claude", "2.1.277", &[judged(&es)[0].id.clone()]);
    std::fs::write(
        gh.dir.join("list.json"),
        json!([{"number": 42, "url": "https://github.com/o/r/issues/42", "body": format!("x\n<!-- release-triage: {mk} -->\n")}]).to_string(),
    )
    .unwrap();
    let out = issue::preflight(&p, &gh.cfg(true), Some("claude"), None).await.unwrap();
    assert_eq!(actions(&out), ["existing"]);
    assert_eq!((&out["would_create"], &out["existing"]), (&json!(0), &json!(1)));
    assert_eq!(out["versions"][0]["proposals"][0]["number"], 42);
    assert_eq!(row(&p).await.issues.len(), 0, "乾跑不把它記進帳本");
}

/// gh 檢查沒過（auth 壞／標籤少）時乾跑不能假裝算得出來：去重那欄是 `remote_unknown`，
/// 但 title／body 照樣渲染，人還是能先看 verdict 品質。
#[tokio::test]
async fn a_dry_run_that_cannot_reach_github_says_so_instead_of_guessing() {
    let p = pool().await;
    let gh = FakeGh::new("dryblocked");
    seed(&p, &[("guard", &[0], None)]).await;
    std::fs::write(gh.dir.join("labels.json"), r#"[{"name":"release-triage"}]"#).unwrap();
    let ok = issue::preflight(&p, &gh.cfg(true), None, None).await.unwrap();
    assert_eq!(actions(&ok), ["create"]);
    assert_eq!(ok["checks"]["labels_missing"], json!(["upstream:claude", "upstream:codex", "triage:guard", "triage:adopt"]));
    gh.flag("fail_auth", true);
    let blocked = issue::preflight(&p, &gh.cfg(true), None, None).await.unwrap();
    assert_eq!(actions(&blocked), ["remote_unknown"]);
    assert_eq!(blocked["would_create"], 0);
    assert!(!blocked["versions"][0]["proposals"][0]["body"].as_str().unwrap().is_empty(), "內文照樣看得到");
    assert_eq!(blocked["versions"][0]["proposals"][0]["writes"], false);
}

/// #204 review 抓到的分岔：乾跑與真跑各寫一遍上限／already，數字會對不上。
/// 同一版兩個提案的 `entry_ids` 有交集時（`already` 用 `.any(contains)`，交集就算，而 `proposals_of`
/// 對重疊沒有任何保證），真跑第一個 create 之後就會跳過第二個——乾跑若讀的是不會長大的 `row.issues`，
/// 就會說 2 張、實際只開 1 張。這條先乾跑再真跑，斷言兩邊逐項相等。
#[tokio::test]
async fn a_dry_run_and_the_real_publish_agree_even_when_two_proposals_share_an_entry() {
    let p = pool().await;
    let gh = FakeGh::new("equiv");
    let es = entries277();
    ledger::insert_version(&p, "claude", "2.1.277", &es).await.unwrap();
    let j = judged(&es);
    // 兩個提案共用 j[0]：第二個還多帶一條 j[1]，所以不是同一組 entry_ids。
    let stored = vec![
        StoredProposal {
            entry_ids: vec![j[0].id.clone()],
            triage: "guard".into(),
            title: "第一張".into(),
            goal: "g".into(),
            suggestion: "s".into(),
            acceptance: "a".into(),
            duplicate_of: None,
        },
        StoredProposal {
            entry_ids: vec![j[0].id.clone(), j[1].id.clone()],
            triage: "guard".into(),
            title: "跟第一張重疊".into(),
            goal: "g".into(),
            suggestion: "s".into(),
            acceptance: "a".into(),
            duplicate_of: None,
        },
    ];
    let v = serde_json::json!({"verdicts": [], "issues": stored});
    assert!(ledger::save_verdicts(&p, "claude", "2.1.277", &v, Status::Judged).await.unwrap());

    let cfg = gh.cfg(true);
    let dry = issue::preflight(&p, &cfg, None, None).await.unwrap();
    assert_eq!(actions(&dry), ["create", "already_logged"], "第二個跟第一個重疊，乾跑就要說不會開");
    assert_eq!(row(&p).await.issues.len(), 0, "乾跑沒動帳本");

    let real = issue::publish_version(&p, &cfg, "claude", "2.1.277").await.unwrap();
    let Outcome::Published { created, commented, existing, .. } = real else { panic!("{real:?}") };
    assert_eq!(
        (dry["would_create"].as_u64().unwrap(), dry["would_comment"].as_u64().unwrap(), dry["existing"].as_u64().unwrap()),
        (created as u64, commented as u64, existing as u64),
        "乾跑預告與真跑結果必須逐項相等：dry={dry}"
    );
    assert_eq!(gh.count("issue create"), 1, "真的只開一張");
}

/// 上限也要對得上：5 個提案、每版上限 4 張——乾跑與真跑要同樣說 4 開 1 擋。
#[tokio::test]
async fn a_dry_run_and_the_real_publish_agree_on_the_per_version_cap() {
    let p = pool().await;
    let gh = FakeGh::new("equivcap");
    seed(&p, &[("guard", &[0], None), ("guard", &[1], None), ("guard", &[2], None), ("guard", &[3], None), ("guard", &[4], None)]).await;
    let cfg = gh.cfg(true);
    let dry = issue::preflight(&p, &cfg, None, None).await.unwrap();
    assert_eq!(actions(&dry), ["create", "create", "create", "create", "skipped_version_limit"]);
    let real = issue::publish_version(&p, &cfg, "claude", "2.1.277").await.unwrap();
    let Outcome::Published { created, skipped, .. } = real else { panic!("{real:?}") };
    assert_eq!(dry["would_create"], json!(created), "乾跑說幾張就是幾張");
    assert_eq!((created, skipped.len()), (4, 1));
}

/// 帳本已經有這個 entry 的 issue（上一輪開好了）→ 連 gh 都不必問。
#[tokio::test]
async fn a_dry_run_skips_github_for_proposals_already_in_the_ledger() {
    let p = pool().await;
    let gh = FakeGh::new("drylogged");
    let es = seed(&p, &[("guard", &[0], None)]).await;
    let id = judged(&es)[0].id.clone();
    let mk = issue::marker("claude", "2.1.277", &[id.clone()]);
    let refs = vec![ledger::IssueRef { marker: mk, entry_ids: vec![id], number: 7, url: "u".into(), created_at: ledger::now_ts(), comment: false }];
    assert!(ledger::save_publish(&p, "claude", "2.1.277", &refs, Status::Judged, None).await.unwrap());
    let out = issue::preflight(&p, &gh.cfg(true), None, None).await.unwrap();
    assert_eq!(actions(&out), ["already_logged"]);
    assert_eq!(gh.count("issue list"), 0, "帳本有了就不問遠端：{:?}", gh.calls());
}
