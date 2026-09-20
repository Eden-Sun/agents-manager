//! B：verdict 驗證、issue 渲染、gh publish（假 gh 腳本，絕不打真的 GitHub）。

use super::issue::{self, Outcome};
use super::ledger::{self, Status};
use super::verdict::{self, EntryVerdict, Proposal, StoredProposal, Submission, Verdict};
use super::*;
use crate::config::ReleaseTriageCfg;
use crate::changelog::Section;

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
    assert_eq!(ok["gh_auth_ok"], true);
    gh.flag("fail_auth", true);
    let bad = issue::health_probe(&gh.cfg(true)).await.unwrap();
    assert_eq!(bad["gh_auth_ok"], false);
    assert!(bad["gh_auth_error"].as_str().unwrap().contains("not logged in"));
}
