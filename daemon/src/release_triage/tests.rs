use super::*;
use crate::changelog::Section;

const CLAUDE_MD: &str = include_str!("fixtures/claude_2.1.276-278.md");
const CODEX_JSON: &str = include_str!("fixtures/codex_releases_0.155.json");

async fn pool() -> SqlitePool {
    let p = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    ledger::migrate(&p).await.unwrap();
    p
}

fn claude_sections() -> Vec<Section> {
    source_sections("claude", CLAUDE_MD)
}

fn codex_sections_fixture() -> Vec<Section> {
    source_sections("codex", &changelog::codex_releases_to_md(CODEX_JSON).unwrap())
}

fn entries(kind: &str, sections: &[Section], version: &str) -> Vec<Entry> {
    build_entries(kind, sections.iter().find(|s| s.version == version).unwrap()).unwrap()
}

fn find<'a>(es: &'a [Entry], needle: &str) -> &'a Entry {
    let hit: Vec<&Entry> = es.iter().filter(|e| e.text.contains(needle)).collect();
    assert_eq!(hit.len(), 1, "「{needle}」應該剛好命中一條，實際 {}", hit.len());
    hit[0]
}

#[test]
fn sha1_matches_known_vectors() {
    assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(sha1_hex(&[b'a'; 1000]), "291e9a6c66994949b57ba5e650361e98fc36b1ba");
}

#[test]
fn entry_splitting_joins_continuations_and_stops_at_blank_or_heading() {
    let body = "intro line\n- first\n  wraps here\n  - nested\n\n  orphan after blank\n- second\n## New Features\n- third   spaced";
    assert_eq!(split_entry_texts(body), ["first wraps here - nested", "second", "third spaced"]);
}

#[test]
fn ids_are_stable_and_match_the_dedupe_markers_already_in_use() {
    let claude = claude_sections();
    let e277 = entries("claude", &claude, "2.1.277");
    let codex = codex_sections_fixture();
    let e0155 = entries("codex", &codex, "0.155.0");
    let e0155_1 = entries("codex", &codex, "0.155.1");
    // #205–#207 的去重標記已經用了這些值：對不上就是正規化寫法變了。
    assert_eq!(find(&e277, "invisible Unicode formatting").id, "b61f2b664d");
    assert_eq!(find(&e277, "Added AGENTS.md support").id, "4257013a6a");
    assert_eq!(find(&e277, "terminal color codes").id, "c9928fd626");
    assert_eq!(find(&e0155, "status row and completion timestamps").id, "9f7b1914a9");
    assert_eq!(find(&e0155_1, "reasoning summaries disabled by default").id, "abc15d424e");
    assert_eq!(entry_id("claude", "2.1.277", "  a   b\n c "), entry_id("claude", "2.1.277", "a b c"));
    assert_ne!(entry_id("claude", "2.1.277", "a"), entry_id("codex", "2.1.277", "a"));
}

#[test]
fn claude_fixture_splits_into_90_entries_and_dropped_ones_name_their_rule() {
    let all = claude_sections();
    assert_eq!(all.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["2.1.278", "2.1.277", "2.1.276"]);
    let mut total = 0;
    for s in &all {
        let es = build_entries("claude", s).unwrap();
        total += es.len();
        for e in &es {
            let is_surface = ["[VSCode]", "[Claude Tag]", "[Claude Code on the web]"].iter().any(|t| e.text.starts_with(t));
            if is_surface || e.text.to_lowercase().contains("gateway") {
                assert_eq!(e.bucket, Bucket::Dropped, "{}", e.text);
            }
            if e.bucket == Bucket::Dropped {
                assert!(!e.rules.is_empty(), "dropped 一定要記規則名：{}", e.text);
            }
            if e.bucket == Bucket::Kept {
                assert!(!e.categories.is_empty(), "kept 一定要有類別：{}", e.text);
            }
        }
    }
    assert_eq!(total, 90);
    let e277 = entries("claude", &all, "2.1.277");
    let surface = e277.iter().filter(|e| e.text.starts_with("[VSCode]") || e.text.starts_with("[Claude Tag]") || e.text.starts_with("[Claude Code on the web]")).count();
    assert!(surface >= 10, "fixture 裡應有一批標籤條目：{surface}");
}

#[test]
fn claude_fixture_keeps_the_entries_that_matter() {
    let e277 = entries("claude", &claude_sections(), "2.1.277");
    let unicode = find(&e277, "invisible Unicode formatting");
    assert_eq!(unicode.bucket, Bucket::Kept);
    assert!(unicode.categories.contains(&"tui".to_string()), "prompt 輸入框語意：{:?}", unicode.categories);
    // 軟 drop 的回歸測試：括號裡提到 Bedrock／Vertex／Foundry 不能讓整條被丟掉。
    let agents = find(&e277, "Added AGENTS.md support");
    assert_eq!(agents.bucket, Bucket::Kept);
    assert!(agents.categories.contains(&"instructions".to_string()));
    assert!(!agents.rules.iter().any(|r| r == "cloud-provider"));
    let color = find(&e277, "terminal color codes");
    assert_eq!(color.bucket, Bucket::Kept);
}

#[test]
fn version_2_1_276_with_only_a_gateway_entry_is_empty() {
    let e276 = entries("claude", &claude_sections(), "2.1.276");
    assert_eq!(e276.len(), 1);
    assert_eq!(e276[0].bucket, Bucket::Dropped);
    assert_eq!(e276[0].rules, ["gateway"]);
}

#[test]
fn codex_body_is_cut_at_changelog_heading_and_headings_do_not_truncate_sections() {
    let all = codex_sections_fixture();
    assert_eq!(all.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["0.155.1", "0.155.0"]);
    let e0155 = entries("codex", &all, "0.155.0");
    assert_eq!(e0155.len(), 13, "策展過的段落 6+6+1；`## Changelog` 以下的 PR 流水帳不產生 entry");
    assert!(!e0155.iter().any(|e| e.text.starts_with("#43521") || e.text.contains("@copyberry")));
    let status = find(&e0155, "status row and completion timestamps");
    assert_eq!(status.bucket, Bucket::Kept);
    assert!(status.categories.contains(&"tui".to_string()));
    assert_eq!(find(&e0155, "/voice").bucket, Bucket::Dropped);
    assert_eq!(find(&e0155, "Touch ID").bucket, Bucket::Dropped);
    assert_eq!(find(&e0155, "Python SDK").bucket, Bucket::Dropped);
    assert_eq!(entries("codex", &all, "0.155.1").len(), 1);
    // `GET /api/changelog` 用的 parse_changelog 行為不變：body 遇到 `## New Features` 就結束。
    let md = changelog::codex_releases_to_md(CODEX_JSON).unwrap();
    assert!(changelog::parse_changelog(&md).iter().all(|s| !s.body.contains("Touch ID")));
}

#[tokio::test]
async fn first_run_records_the_installed_baseline_and_returns_nothing_pending() {
    let p = pool().await;
    let all = claude_sections();
    let r = check(&p, "claude", &all, Some("2.1.278 (Claude Code)"), None).await.unwrap();
    assert_eq!((r.from.as_str(), r.to.as_str()), ("2.1.278", "2.1.278"));
    assert!(r.pending.is_empty());
    let row = ledger::get(&p, "claude", "2.1.278").await.unwrap().unwrap();
    assert_eq!(row.status, ledger::Status::Empty);
    // 帳本已有基準：同一份 feed 再跑，沒有新版。
    assert!(check(&p, "claude", &all, None, None).await.unwrap().pending.is_empty());
    // 沒有基準又讀不到磁碟版本 → 明確報錯，不猜。
    let fresh = pool().await;
    assert!(check(&fresh, "claude", &all, None, None).await.is_err());
}

#[tokio::test]
async fn replay_2_1_275_to_278_gives_a_row_per_version_and_276_is_empty() {
    let p = pool().await;
    let all = claude_sections();
    let r = check(&p, "claude", &all, None, Some("2.1.275")).await.unwrap();
    assert_eq!((r.from.as_str(), r.to.as_str()), ("2.1.275", "2.1.278"));
    // 三版各一列；276 全被丟掉 → empty，不在 pending。
    assert_eq!(ledger::list(&p, Some("claude"), None).await.unwrap().len(), 3);
    assert_eq!(ledger::get(&p, "claude", "2.1.276").await.unwrap().unwrap().status, ledger::Status::Empty);
    // 契約：pending 舊版在前、新版在後（kick 截斷時 request-id 取該批最後一版，靠這個順序）。
    assert_eq!(r.pending.iter().map(|v| v.version.as_str()).collect::<Vec<_>>(), ["2.1.277", "2.1.278"], "升冪；278 的 `/status` 那條進 kept");
    let v277 = &r.pending[0];
    assert!(v277.kept.iter().any(|k| k.id == "b61f2b664d" && !k.categories.is_empty()));
    assert!(v277.kept.iter().any(|k| k.id == "4257013a6a"));
    assert!(v277.dropped_count > 20 && v277.kept.len() + v277.unmatched.len() + v277.dropped_count == 87);
    // 輸出契約：欄位名固定。
    let j = serde_json::to_value(&r).unwrap();
    assert!(j.get("kind").is_some() && j.get("from").is_some() && j.get("to").is_some());
    let pv = &j["pending"][0];
    assert!(pv["version"].is_string() && pv["kept"][0]["id"].is_string() && pv["kept"][0]["text"].is_string() && pv["kept"][0]["categories"].is_array());
    assert!(pv["unmatched"].is_array() && pv["dropped_count"].is_number());
}

#[tokio::test]
async fn rerun_is_idempotent_and_ledger_max_version_becomes_the_new_from() {
    let p = pool().await;
    let all = claude_sections();
    let first = check(&p, "claude", &all, None, Some("2.1.275")).await.unwrap();
    let second = check(&p, "claude", &all, None, None).await.unwrap();
    assert_eq!(second.from, "2.1.278", "from＝帳本已分診的最大版本");
    assert!(second.pending.is_empty(), "from 之後沒有新版");
    // 派出去之後就不再是 pending；重跑 --since 也不會重複派。
    let both = ["2.1.277".to_string(), "2.1.278".to_string()];
    assert_eq!(ledger::mark_dispatched(&p, "claude", &both).await.unwrap(), 2);
    assert_eq!(ledger::mark_dispatched(&p, "claude", &both).await.unwrap(), 0, "CAS：已 dispatched 的不再動");
    let again = check(&p, "claude", &all, None, Some("2.1.275")).await.unwrap();
    assert!(again.pending.is_empty());
    assert_eq!(first.pending.len(), 2);
}

#[tokio::test]
async fn a_new_upstream_version_after_the_baseline_is_the_only_pending() {
    let p = pool().await;
    let old: Vec<Section> = claude_sections().into_iter().filter(|s| s.version != "2.1.278").collect();
    check(&p, "claude", &old, Some("2.1.277"), None).await.unwrap();
    let r = check(&p, "claude", &claude_sections(), None, None).await.unwrap();
    assert_eq!((r.from.as_str(), r.to.as_str()), ("2.1.277", "2.1.278"));
    assert_eq!(r.pending.iter().map(|v| v.version.as_str()).collect::<Vec<_>>(), ["2.1.278"]);
    assert!(r.pending[0].kept.iter().any(|k| k.text.contains("Auto mode server")), "`/status` 那條要進 kept");
}

#[tokio::test]
async fn stale_dispatch_goes_back_to_pending_and_fails_after_three_attempts() {
    let p = pool().await;
    let es = entries("claude", &claude_sections(), "2.1.277");
    ledger::insert_version(&p, "claude", "2.1.277", &es).await.unwrap();
    for round in 1..=3 {
        ledger::mark_dispatched(&p, "claude", &["2.1.277".into()]).await.unwrap();
        // 剛派出去：還沒過期，不動。
        assert!(ledger::requeue_stale(&p, "claude").await.unwrap().is_empty());
        let old = (chrono::Utc::now() - chrono::Duration::hours(7)).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        sqlx::query("UPDATE release_triage SET dispatched_at = ? WHERE kind='claude' AND version='2.1.277'").bind(old).execute(&p).await.unwrap();
        let moved = ledger::requeue_stale(&p, "claude").await.unwrap();
        let want = if round == 3 { ledger::Status::Failed } else { ledger::Status::Pending };
        assert_eq!(moved, [("2.1.277".to_string(), want)], "第 {round} 次");
    }
    let row = ledger::get(&p, "claude", "2.1.277").await.unwrap().unwrap();
    assert_eq!((row.status, row.attempts), (ledger::Status::Failed, 3));
}

#[tokio::test]
async fn ledger_max_version_compares_numerically() {
    let p = pool().await;
    for v in ["0.9.0", "0.10.0", "0.2.5"] {
        ledger::insert_baseline(&p, "codex", v).await.unwrap();
    }
    assert_eq!(ledger::max_version(&p, "codex").await.unwrap().as_deref(), Some("0.10.0"));
    assert_eq!(ledger::max_version(&p, "claude").await.unwrap(), None);
}

#[tokio::test]
async fn unsupported_kind_is_an_error() {
    let p = pool().await;
    assert!(check(&p, "herdr", &claude_sections(), Some("1.0.0"), None).await.is_err());
}

/// 整段 CLI 流程（feed 檔＋DB 檔）：不碰網路、不碰正式 DB。
#[tokio::test]
async fn run_check_reads_a_feed_file_and_a_private_db() {
    let dir = std::env::temp_dir().join(format!("am-rt-{}-{}", std::process::id(), crate::release_triage::ledger::now_ts().replace(':', "")));
    std::fs::create_dir_all(&dir).unwrap();
    let feed = dir.join("CHANGELOG.md");
    std::fs::write(&feed, CLAUDE_MD).unwrap();
    let db = dir.join("t.sqlite3");
    std::fs::write(&db, b"").unwrap();
    let mk = |since: Option<&str>| CheckArgs {
        kind: "claude".into(),
        since: since.map(String::from),
        installed: Some("2.1.276".into()),
        db: Some(db.clone()),
        feed_file: Some(feed.clone()),
    };
    let first = run_check(mk(None)).await.unwrap();
    assert!(first.pending.is_empty() && first.from == "2.1.276");
    let backfill = run_check(mk(Some("2.1.276"))).await.unwrap();
    assert_eq!(backfill.pending.iter().map(|v| v.version.as_str()).collect::<Vec<_>>(), ["2.1.277", "2.1.278"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 契約：pending 一律升冪，包含 `0.9.x` 與 `0.10.x` 這種字串排序會錯的版本。
#[tokio::test]
async fn pending_is_sorted_oldest_first_numerically() {
    let p = pool().await;
    let mk = |v: &str| Section { version: v.into(), body: "- Fixed `--resume` hanging".into() };
    let all: Vec<Section> = ["0.10.0", "0.9.1", "0.9.0", "0.2.5"].iter().map(|v| mk(v)).collect();
    let r = check(&p, "claude", &all, None, Some("0.2.5")).await.unwrap();
    assert_eq!(r.pending.iter().map(|v| v.version.as_str()).collect::<Vec<_>>(), ["0.9.0", "0.9.1", "0.10.0"]);
}

#[test]
fn cli_version_output_with_a_name_prefix_is_still_a_version() {
    // AGM 2026-09-19 實測：`codex --version` 印 `codex-cli 0.154.0`，以前整個 check 掛掉、kick 每輪跳過 codex。
    for (line, want) in [
        ("codex-cli 0.154.0", Some("0.154.0")),
        ("2.1.278 (Claude Code)", Some("2.1.278")),
        ("herdr 0.8.2", Some("0.8.2")),
        // 前面有帶點的非版本 token（檔名、網址）不能讓後面真的版本被略過。
        ("claude.real 2.1.278", Some("2.1.278")),
        ("herdr.exe 0.9.1 (build 2026.09.10)", Some("0.9.1")),
        ("codex-cli", None),
        ("", None),
    ] {
        assert_eq!(changelog::cli_version_string(line).as_deref(), want, "{line}");
    }
}
