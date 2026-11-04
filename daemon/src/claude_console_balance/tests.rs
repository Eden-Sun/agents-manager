use super::*;
use std::sync::Mutex as StdMutex;

const EN_PAGE: &str = "Console\nSettings\nBilling\n\nCredit balance\nRemaining balance for API usage\n\n$0.00\n$96.99 promotional credit available\n\nBuy credits\n\nPromotional credits\nRemaining balance    Applies to\n    Expires on    Date granted\n$96.99 of $100.00\nAgent SDK, API, Batch API, Playground, Managed Agents\n    11/15/2026    10/14/2026\n";

fn ok(o: ParseOutcome) -> ConsoleBalanceData {
    match o {
        ParseOutcome::Ok(d) => d,
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn parses_english_page() {
    let d = ok(parse_console_billing(EN_PAGE));
    assert_eq!(d.purchased_usd, 0.0);
    assert_eq!(d.promotional_usd, 96.99);
    assert!((d.total_usd() - 96.99).abs() < 1e-9);
    assert_eq!(d.promotions, vec![Promotion { remaining_usd: 96.99, total_usd: 100.0, expires_on: Some("2026-11-15".into()) }]);
}

#[test]
fn parses_purchased_plus_multiple_promotions() {
    let t = "Credit balance\n$1,234.50\n$30.00 promotional credit available\nPromotional credits\n$10.00 of $20.00\nAPI\n12/01/2026\n10/01/2026\n$20.00 of $50.00\nAPI\n01/31/2027 10/20/2026\n";
    let d = ok(parse_console_billing(t));
    assert_eq!(d.purchased_usd, 1234.5);
    assert_eq!(d.promotional_usd, 30.0);
    assert!((d.total_usd() - 1264.5).abs() < 1e-9);
    assert_eq!(d.promotions.len(), 2);
    assert_eq!(d.promotions[0].expires_on.as_deref(), Some("2026-12-01"));
    assert_eq!(d.promotions[1].remaining_usd, 20.0);
    assert_eq!(d.promotions[1].total_usd, 50.0);
    assert_eq!(d.promotions[1].expires_on.as_deref(), Some("2027-01-31"));
}

#[test]
fn parses_chinese_page() {
    let t = "額度餘額\n\n$12.34\n$96.99 促銷額度可用\n\n促銷額度\n剩餘餘額    適用於\n到期日    發放日期\n$96.99 / $100.00\nAgent SDK、API\n2026年11月15日    2026年10月14日\n";
    let d = ok(parse_console_billing(t));
    assert_eq!(d.purchased_usd, 12.34);
    assert_eq!(d.promotional_usd, 96.99);
    assert_eq!(d.promotions, vec![Promotion { remaining_usd: 96.99, total_usd: 100.0, expires_on: Some("2026-11-15".into()) }]);
}

#[test]
fn chinese_iso_dates_and_swapped_columns() {
    // 欄位順序反了（發放日在前）也取較晚的當到期日。
    let t = "信用額度餘額\n$0.00\n$5.00 優惠額度可用\n$5.00 共 $10.00\n2026-10-14   2026-11-15\n";
    let d = ok(parse_console_billing(t));
    assert_eq!(d.promotions[0].expires_on.as_deref(), Some("2026-11-15"));
}

#[test]
fn promotional_falls_back_to_sum_when_summary_line_missing() {
    let t = "Credit balance\n$3.00\n$1.50 of $2.00\n11/15/2026 10/14/2026\n$2.50 of $5.00\n12/15/2026 10/14/2026\n";
    let d = ok(parse_console_billing(t));
    assert_eq!(d.promotional_usd, 4.0);
}

#[test]
fn no_promotions_is_zero() {
    let d = ok(parse_console_billing("Credit balance\n$7.25\nBuy credits\n"));
    assert_eq!(d.purchased_usd, 7.25);
    assert_eq!(d.promotional_usd, 0.0);
    assert!(d.promotions.is_empty());
}

#[test]
fn english_month_dates() {
    assert_eq!(find_dates("Nov 15, 2026  October 14 2026"), vec![NaiveDate::from_ymd_opt(2026, 11, 15).unwrap(), NaiveDate::from_ymd_opt(2026, 10, 14).unwrap()]);
    // 第一段 > 12 當 DD/MM。
    assert_eq!(find_dates("25/12/2026"), vec![NaiveDate::from_ymd_opt(2026, 12, 25).unwrap()]);
    assert!(find_dates("$96.99 of $100.00").is_empty());
}

#[test]
fn logged_out_pages() {
    assert_eq!(parse_console_billing("Welcome back\nContinue with Google\nContinue with email\n"), ParseOutcome::LoggedOut);
    assert_eq!(parse_console_billing("歡迎回來\n使用 Google 繼續\n使用電子郵件繼續\n"), ParseOutcome::LoggedOut);
}

#[test]
fn unrecognised_page_is_parse_failed() {
    assert_eq!(parse_console_billing("Something completely different\n$5.00\n"), ParseOutcome::ParseFailed);
    assert_eq!(parse_console_billing(""), ParseOutcome::ParseFailed);
    // 有標籤但後面沒有金額。
    assert_eq!(parse_console_billing("Credit balance\nloading...\n"), ParseOutcome::ParseFailed);
}

#[test]
fn console_json_shapes() {
    assert_eq!(console_json(None, false), json!({"status": "not_configured"}));
    assert_eq!(console_json(None, true)["status"], "error");
    let snap = ConsoleSnapshot { status: Status::Ok, data: Some(ok(parse_console_billing(EN_PAGE))), fetched_at: "2026-11-04T10:00:00.000Z".into(), error: None };
    let v = console_json(Some(&snap), true);
    assert_eq!(v["status"], "ok");
    assert_eq!(v["purchased_usd"], 0.0);
    assert_eq!(v["promotional_usd"], 96.99);
    assert_eq!(v["total_usd"], 96.99);
    assert_eq!(v["promotions"][0]["expires_on"], "2026-11-15");
    assert_eq!(v["fetched_at"], "2026-11-04T10:00:00.000Z");
    let out = ConsoleSnapshot { status: Status::LoggedOut, data: None, fetched_at: "t".into(), error: None };
    let v = console_json(Some(&out), true);
    assert_eq!(v["status"], "logged_out");
    assert!(v.get("total_usd").is_none());
}

// ── 流程：假 runner ──

struct FakeRunner {
    calls: StdMutex<Vec<Vec<String>>>,
    text: Result<String, String>,
    open_err: Option<String>,
    /// 依序消耗：每次 open 先從這裡取一個錯誤回；空了才看 `open_err`。
    open_errs_once: StdMutex<Vec<String>>,
    saved: Option<String>,
}

impl FakeRunner {
    fn new(text: Result<String, String>) -> Self {
        Self { calls: StdMutex::new(Vec::new()), text, open_err: None, open_errs_once: StdMutex::new(Vec::new()), saved: Some(r#"{"cookies":[{"name":"fresh"}]}"#.into()) }
    }
    fn count(&self, word: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|c| c.iter().any(|a| a == word)).count()
    }
}

impl BrowserRunner for FakeRunner {
    fn run(&self, a: Vec<String>, _t: Duration) -> impl Future<Output = Result<String, String>> + Send {
        self.calls.lock().unwrap().push(a.clone());
        let res = if a.iter().any(|x| x == "open") {
            let once = { let mut q = self.open_errs_once.lock().unwrap(); if q.is_empty() { None } else { Some(q.remove(0)) } };
            match once {
                Some(e) => Err(e),
                None => self.open_err.clone().map_or(Ok(String::new()), Err),
            }
        } else if a.iter().any(|x| x == "text") {
            self.text.clone()
        } else if a.iter().any(|x| x == "save") {
            match &self.saved {
                Some(s) => std::fs::write(a.last().unwrap(), s).map(|_| String::new()).map_err(|e| e.to_string()),
                None => Err("save failed".into()),
            }
        } else {
            Ok(String::new())
        };
        async move { res }
    }
}

fn state_dir(initial: &str) -> ((), PathBuf) {
    let dir = crate::testing::scratch_dir("console-balance");
    let p = dir.join("state.json");
    std::fs::write(&p, initial).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    ((), p)
}

#[tokio::test(start_paused = true)]
async fn fetch_ok_closes_and_writes_state_back() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, p) = state_dir(r#"{"cookies":[{"name":"old"}]}"#);
    let r = FakeRunner::new(Ok(EN_PAGE.into()));
    let snap = fetch_once(&r, &p).await;
    assert_eq!(snap.status, Status::Ok);
    assert_eq!(snap.data.unwrap().total_usd(), 96.99);
    assert_eq!(r.count("close"), 2, "先收殘留 session、讀完再收一次");
    assert!(std::fs::read_to_string(&p).unwrap().contains("fresh"));
    assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(leftovers, vec!["state.json".to_string()], "暫存檔要清掉");
}

#[tokio::test(start_paused = true)]
async fn logged_out_does_not_touch_state_but_closes() {
    let (_d, p) = state_dir(r#"{"cookies":[{"name":"old"}]}"#);
    let r = FakeRunner::new(Ok("Continue with Google".into()));
    let snap = fetch_once(&r, &p).await;
    assert_eq!(snap.status, Status::LoggedOut);
    assert_eq!(r.count("save"), 0);
    assert!(std::fs::read_to_string(&p).unwrap().contains("old"));
    assert_eq!(r.count("close"), 2);
}

#[tokio::test(start_paused = true)]
async fn open_failure_still_closes() {
    let (_d, p) = state_dir("{}");
    let mut r = FakeRunner::new(Ok(String::new()));
    r.open_err = Some("boom".into());
    let snap = fetch_once(&r, &p).await;
    assert_eq!(snap.status, Status::Error);
    assert_eq!(snap.error.as_deref(), Some("boom"));
    assert_eq!(r.count("close"), 2);
}

const CONNECT_ERR: &str = "✗ Could not configure browser: Failed to connect: No such file or directory (os error 2)";

#[tokio::test(start_paused = true)]
async fn open_retries_on_failed_to_connect_then_succeeds() {
    let (_d, p) = state_dir(r#"{"cookies":[{"name":"old"}]}"#);
    let r = FakeRunner::new(Ok(EN_PAGE.into()));
    r.open_errs_once.lock().unwrap().push(CONNECT_ERR.into());
    let snap = fetch_once(&r, &p).await;
    assert_eq!(snap.status, Status::Ok);
    assert_eq!(r.count("open"), 2);
    assert_eq!(r.count("close"), 2);
}

#[tokio::test(start_paused = true)]
async fn open_connect_failures_exhaust_retries_and_still_close() {
    let (_d, p) = state_dir("{}");
    let mut r = FakeRunner::new(Ok(EN_PAGE.into()));
    r.open_err = Some(CONNECT_ERR.into());
    let snap = fetch_once(&r, &p).await;
    assert_eq!(snap.status, Status::Error);
    assert!(snap.error.unwrap().contains("Failed to connect"));
    assert_eq!(r.count("open"), 1 + OPEN_RETRIES);
    assert_eq!(r.count("close"), 2);
}

#[tokio::test(start_paused = true)]
async fn open_non_connect_failure_is_not_retried() {
    let (_d, p) = state_dir("{}");
    let mut r = FakeRunner::new(Ok(String::new()));
    r.open_err = Some("boom".into());
    let _ = fetch_once(&r, &p).await;
    assert_eq!(r.count("open"), 1);
}

#[tokio::test(start_paused = true)]
async fn get_text_failure_still_closes() {
    let (_d, p) = state_dir("{}");
    let r = FakeRunner::new(Err("get failed".into()));
    let snap = fetch_once(&r, &p).await;
    assert_eq!(snap.status, Status::Error);
    assert_eq!(r.count("close"), 2);
}

#[tokio::test(start_paused = true)]
async fn unparseable_page_polls_then_parse_failed() {
    let (_d, p) = state_dir("{}");
    let r = FakeRunner::new(Ok("nothing useful".into()));
    let snap = fetch_once(&r, &p).await;
    assert_eq!(snap.status, Status::ParseFailed);
    assert_eq!(r.count("text"), MAX_POLLS);
    assert_eq!(r.count("close"), 2);
}

#[tokio::test(start_paused = true)]
async fn bad_saved_state_keeps_original_file() {
    let (_d, p) = state_dir(r#"{"cookies":[{"name":"old"}]}"#);
    let mut r = FakeRunner::new(Ok(EN_PAGE.into()));
    r.saved = Some(r#"{"cookies":[]}"#.into());
    let snap = fetch_once(&r, &p).await;
    assert_eq!(snap.status, Status::Ok, "寫回失敗不影響讀到的餘額");
    assert!(std::fs::read_to_string(&p).unwrap().contains("old"));
    assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    // save 指令本身失敗也一樣。
    r.saved = None;
    let snap = fetch_once(&r, &p).await;
    assert_eq!(snap.status, Status::Ok);
    assert!(std::fs::read_to_string(&p).unwrap().contains("old"));
}

#[test]
fn child_path_puts_chain_dirs_first() {
    let orig = std::env::join_paths(["/usr/bin", "/bin"]).unwrap();
    let chain = vec![PathBuf::from("/a/bin"), PathBuf::from("/opt/node/bin")];
    let p = child_path(&chain, Some(&orig)).unwrap();
    let v: Vec<PathBuf> = std::env::split_paths(&p).collect();
    assert_eq!(v, vec![PathBuf::from("/a/bin"), PathBuf::from("/opt/node/bin"), PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
}

#[test]
fn child_path_dedups_and_handles_missing() {
    let orig = std::env::join_paths(["/usr/bin", "/opt/node/bin"]).unwrap();
    let chain = vec![PathBuf::from("/opt/node/bin"), PathBuf::from("/opt/node/bin")];
    let p = child_path(&chain, Some(&orig)).unwrap();
    let v: Vec<PathBuf> = std::env::split_paths(&p).collect();
    assert_eq!(v, vec![PathBuf::from("/opt/node/bin"), PathBuf::from("/usr/bin")]);
    assert_eq!(std::env::split_paths(&child_path(&[PathBuf::from("/x")], None).unwrap()).count(), 1);
    assert!(child_path(&[], Some(&orig)).is_none());
}

#[cfg(unix)]
#[test]
fn exe_chain_dirs_follows_two_hop_symlink_chain() {
    use std::os::unix::fs::symlink;
    let root = crate::testing::scratch_dir("agm-chain");
    let (local, node_bin, pkg) = (root.join("local/bin"), root.join("node/bin"), root.join("node/lib/pkg/bin"));
    for d in [&local, &node_bin, &pkg] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(pkg.join("real"), "").unwrap();
    // 第一跳用相對路徑、第二跳用絕對路徑，兩種都要涵蓋。
    symlink("../lib/pkg/bin/real", node_bin.join("agent-browser")).unwrap();
    symlink(node_bin.join("agent-browser"), local.join("agent-browser")).unwrap();
    let dirs = exe_chain_dirs(&local.join("agent-browser"));
    assert_eq!(dirs, vec![local.clone(), node_bin.clone(), node_bin.join("../lib/pkg/bin")]);
    assert!(dirs.contains(&node_bin), "中間層（node 所在）必須在 PATH 內");
    // 非 symlink、相對單檔名都不炸。
    assert_eq!(exe_chain_dirs(&pkg.join("real")), vec![pkg.clone()]);
    assert!(exe_chain_dirs(Path::new("agent-browser")).is_empty());
}
