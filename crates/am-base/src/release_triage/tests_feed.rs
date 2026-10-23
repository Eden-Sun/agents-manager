//! agy／grok 的分診規則與帳本（issue #1928）：fixture 是實際抓下來的上游原文，不是手湊的字串。來源抓取與切段的測試在 `changelog.daemon-tests-tests.rs`。
use crate::changelog::Section;
use super::*;

const AGY_MD: &str = include_str!("fixtures/agy_1.4.4-1.4.6.md");
const GROK_MD: &str = include_str!("fixtures/grok_1.0.67.external.md");

async fn pool() -> SqlitePool {
    let p = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    ledger::migrate(&p).await.unwrap();
    p
}

fn counts(kind: &str, sections: &[Section], version: &str) -> (usize, usize, usize) {
    let es = build_entries(kind, sections.iter().find(|s| s.version == version).unwrap()).unwrap();
    let n = |b: Bucket| es.iter().filter(|e| e.bucket == b).count();
    (n(Bucket::Kept), n(Bucket::Unmatched), n(Bucket::Dropped))
}

// ───────────────────────── 切段與規則 ─────────────────────────

/// 規則層只決定「哪些條目要被認真看」：kept 才是 issue 候選，真正開幾張由模型的 guard／adopt verdict 決定，
/// 而且每版 ≤`issue::MAX_PER_VERSION`（4）張、每 24 小時 ≤`issue::MAX_PER_DAY`（8）張。所以這裡斷言的是候選上限。
#[test]
fn agy_rules_are_conservative_and_the_issue_candidates_are_few() {
    let all = source_sections("agy", AGY_MD);
    let table: Vec<(&str, (usize, usize, usize))> = ["1.4.6", "1.4.5", "1.4.4"].iter().map(|v| (*v, counts("agy", &all, v))).collect();
    // （kept, unmatched, dropped）：第一版沒有 drop 規則，其餘全進 unmatched。
    assert_eq!(table, vec![("1.4.6", (3, 8, 0)), ("1.4.5", (2, 10, 0)), ("1.4.4", (4, 14, 0))]);
    let kept: usize = table.iter().map(|(_, c)| c.0).sum();
    assert_eq!((kept, table.iter().map(|(_, c)| c.0 + c.1 + c.2).sum::<usize>()), (9, 41), "41 條裡只有 9 條是 issue 候選");
    let e = build_entries("agy", all.iter().find(|s| s.version == "1.4.6").unwrap()).unwrap();
    let permission = e.iter().find(|e| e.text.starts_with("Improved permission prompts")).unwrap();
    assert_eq!((permission.bucket, permission.categories.clone()), (Bucket::Kept, vec!["tui".to_string(), "subagent".to_string()]));
    // 不在上述五類的（plugin、sandbox、/diff…）一律 unmatched，不丟。
    let sandbox = e.iter().find(|e| e.text.starts_with("Improved terminal sandbox")).unwrap();
    assert_eq!(sandbox.bucket, Bucket::Unmatched);
}

#[test]
fn grok_rules_are_conservative_and_the_issue_candidates_are_few() {
    let all = source_sections("grok", GROK_MD);
    let table: Vec<(&str, (usize, usize, usize))> = ["1.0.67", "1.0.66", "1.0.65", "1.0.64", "1.0.63"].iter().map(|v| (*v, counts("grok", &all, v))).collect();
    assert_eq!(table, vec![("1.0.67", (2, 3, 0)), ("1.0.66", (0, 3, 0)), ("1.0.65", (2, 4, 0)), ("1.0.64", (5, 14, 0)), ("1.0.63", (3, 7, 0))]);
    let kept: usize = table.iter().map(|(_, c)| c.0).sum();
    assert_eq!((kept, table.iter().map(|(_, c)| c.0 + c.1 + c.2).sum::<usize>()), (12, 43), "43 條裡只有 12 條是 issue 候選");
    let e = build_entries("grok", all.iter().find(|s| s.version == "1.0.67").unwrap()).unwrap();
    let hooks = e.iter().find(|e| e.text.starts_with("**Quitting a session with hooks")).unwrap();
    assert_eq!((hooks.bucket, hooks.categories.clone()), (Bucket::Kept, vec!["hook".to_string()]));
}

// ───────────────────────── 帳本：同一個 stable 只分診一次 ─────────────────────────

#[tokio::test]
async fn a_grok_stable_window_lands_one_row_per_version_and_is_triaged_once() {
    let p = pool().await;
    let all = source_sections("grok", GROK_MD);
    let r = check(&p, "grok", &all, None, Some("1.0.62")).await.unwrap();
    assert_eq!((r.from.as_str(), r.to.as_str()), ("1.0.62", "1.0.67"));
    assert_eq!(r.pending.iter().map(|v| v.version.as_str()).collect::<Vec<_>>(), ["1.0.63", "1.0.64", "1.0.65", "1.0.66", "1.0.67"]);
    assert_eq!(ledger::list(&p, Some("grok"), None).await.unwrap().len(), 5);
    // 同一份 stable 再跑：帳本最大版本已是 1.0.67，不再插入、列也不變。
    let again = check(&p, "grok", &all, None, None).await.unwrap();
    assert_eq!((again.from.as_str(), again.to.as_str()), ("1.0.67", "1.0.67"));
    assert_eq!(ledger::list(&p, Some("grok"), None).await.unwrap().len(), 5);
    assert_eq!(again.pending.len(), 5, "還沒派工就仍是 pending，但沒有多出新的一列");
    // 錯過上一個 stable：最舊的段落跟 `from` 之間缺版 → 只記 log、不補抓，照常分診手上有的。
    let gap = pool().await;
    let r = check(&gap, "grok", &all, None, Some("1.0.60")).await.unwrap();
    assert_eq!(r.pending.len(), 5);
}

#[tokio::test]
async fn agy_first_run_baseline_and_new_version_flow() {
    let p = pool().await;
    let all = source_sections("agy", AGY_MD);
    let r = check(&p, "agy", &all, Some("1.4.4"), None).await.unwrap();
    assert!(r.pending.is_empty());
    let r = check(&p, "agy", &all, None, None).await.unwrap();
    assert_eq!(r.pending.iter().map(|v| v.version.as_str()).collect::<Vec<_>>(), ["1.4.5", "1.4.6"]);
    assert_eq!(r.pending[1].kept.len(), 3);
    assert!(check(&p, "herdr", &all, None, None).await.is_err());
}

#[test]
fn every_triage_kind_has_a_label() {
    let labels = issue::required_labels();
    for k in ["claude", "codex", "agy", "grok"] {
        assert!(labels.contains(&format!("upstream:{k}")), "{k}");
    }
}
