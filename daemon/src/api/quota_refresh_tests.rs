/// 逾時就先回，背景的探測不能被取消（它跑完照樣寫進 quota、推 WS）。
///
/// 不靠牆鐘（以前 `elapsed < 250ms`＋`sleep(500ms)` 在慢 runner 上會掛）：慢的那個工作卡在閘門上，
/// 閘門在「已經逾時先回」之後才放開——所以「不等慢的那一個」是結構上成立的，不是量出來的；放開後用輪詢等它跑完。
#[tokio::test]
async fn a_slow_refresh_answers_early_and_keeps_running() {
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (g, d) = (gate.clone(), done.clone());
    let slow = tokio::spawn(async move {
        g.notified().await;
        d.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    assert!(!super::finished_within(std::time::Duration::from_millis(20), slow).await, "卡在閘門上：逾時先回");
    assert!(!done.load(std::sync::atomic::Ordering::SeqCst), "先回的時候慢的還沒做完");
    gate.notify_one();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !done.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "逾時後背景沒有跑完（被取消了？）");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let quick = tokio::spawn(async {});
    assert!(super::finished_within(std::time::Duration::from_secs(30), quick).await);
}
