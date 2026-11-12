use super::*;

#[test]
fn a_superseded_tool_detection_is_a_retryable_409_not_a_502() {
    let e = anyhow::Error::new(crate::tools::Superseded { host: "mac_builder".into() });
    let LcError::Conflict(v) = detect_error("mac_builder", e) else { panic!("要是 409") };
    assert_eq!(v["reason"], "host_superseded");
    assert_eq!(v["retryable"], true);
    assert_eq!(v["host"], "mac_builder");
    assert!(matches!(detect_error("mac_builder", anyhow::anyhow!("ssh: connect timed out")), LcError::Upstream(_)), "其餘照舊 502");
}
