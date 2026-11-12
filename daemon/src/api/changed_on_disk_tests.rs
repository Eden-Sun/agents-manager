use super::*;

/// #1647：寫之前磁碟被手改是「這次沒寫、可以重送」，要回 409（`config_written:false`），不能是 502 upstream。
#[test]
fn a_config_changed_on_disk_is_a_retryable_conflict_not_an_upstream_failure() {
    match cfg_err(anyhow::Error::new(crate::config::ConfigChangedOnDisk)) {
        LcError::Conflict(v) => {
            assert_eq!(v["reason"], "config_changed_on_disk");
            assert_eq!(v["config_written"], false);
            assert_eq!(v["retryable"], true);
        }
        other => panic!("{other:?}"),
    }
}
