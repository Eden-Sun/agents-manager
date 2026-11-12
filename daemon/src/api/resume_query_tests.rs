use super::*;

#[test]
fn only_native_is_a_resume_mode_and_it_is_strict() {
    let q = |v: Option<&str>| StartQuery { resume: v.map(str::to_string), session: None };
    // 2026-10-02 起預設接回（接不回才開新的），`fresh` 才是開新對話。
    let default = resume_opts(&q(None)).unwrap();
    assert!(default.resume_native && !default.resume_required, "預設：能接就接，接不回照樣啟動");
    assert_eq!(resume_opts(&q(Some(""))).unwrap(), default);
    assert_eq!(resume_opts(&q(Some("fresh"))).unwrap(), lifecycle::StartOpts::default(), "fresh：開新對話");
    let native = resume_opts(&q(Some("native"))).unwrap();
    assert!(native.resume_native && native.resume_required, "native 一定是「接不回就不啟動」");
    assert!(matches!(resume_opts(&q(Some("bogus"))), Err(LcError::Bad(_))));
}

/// `session=<id>` 只跟 `resume=native` 一起收，而且只收乾淨的 id（救援路徑，2026-09-22）。
#[test]
fn an_explicit_session_rides_only_on_native_and_must_be_plain() {
    let q = |r: Option<&str>, s: Option<&str>| StartQuery { resume: r.map(str::to_string), session: s.map(str::to_string) };
    let o = resume_opts(&q(Some("native"), Some(" 246fcf93-af39-48d3-8041-b27df5a91958 "))).unwrap();
    assert_eq!(o.resume_session.as_deref(), Some("246fcf93-af39-48d3-8041-b27df5a91958"));
    assert!(o.resume_native && o.resume_required);
    assert!(resume_opts(&q(Some("native"), Some(""))).unwrap().resume_session.is_none());
    assert!(matches!(resume_opts(&q(None, Some("abc"))), Err(LcError::Bad(_))), "沒有 native 不收 session");
    assert!(matches!(resume_opts(&q(Some("native"), Some("a b"))), Err(LcError::Bad(_))));
    assert!(matches!(resume_opts(&q(Some("native"), Some("../x"))), Err(LcError::Bad(_))));
}

#[tokio::test]
async fn capabilities_advertise_resume_maintenance_and_service_auth() {
    let v = get_capabilities().await.0;
    let caps: Vec<&str> = v["capabilities"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(caps.contains(&"resume_native_start") && caps.contains(&"herdr_maintenance") && caps.contains(&"service_principals"), "{v}");
}
