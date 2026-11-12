use super::within_fresh_window;
use chrono::{Duration, Utc};

/// #393：收編後 10 分鐘內才從 argv 補 fast；之後使用者改的設定不能被 argv 蓋回去。
#[test]
fn only_a_recently_adopted_run_may_take_fast_from_its_argv() {
    let now = Utc::now();
    let at = |mins: i64| (now - Duration::minutes(mins)).to_rfc3339();
    assert!(within_fresh_window(&at(1), now));
    assert!(within_fresh_window(&at(9), now));
    assert!(!within_fresh_window(&at(11), now));
    assert!(!within_fresh_window("not a time", now), "讀不懂就不補");
}
