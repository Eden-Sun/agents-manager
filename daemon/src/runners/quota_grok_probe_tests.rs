use crate::config::LOCAL_HOST;
use crate::identity_prefs::{identity_disabled, set_identity_disabled, DEFAULT_ACCOUNT};
use crate::testing as tt;

async fn set_grok(app: &std::sync::Arc<crate::state::App>, logged_in: Option<bool>) {
    let mut tools = std::collections::BTreeMap::new();
    tools.insert("grok".to_string(), crate::tools::ToolInfo { installed: true, path: Some("/bin/true".into()), version: None, logged_in });
    app.tools.lock().await.insert(
        LOCAL_HOST.into(),
        crate::tools::HostTools { tools, identities: Default::default(), shell_identities: vec![], utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now() },
    );
}

#[tokio::test]
async fn identity_disabled_defaults_false_and_reads_back() {
    let e = tt::env().await;
    assert!(!identity_disabled(&e.app.db, "m4p", "grok", DEFAULT_ACCOUNT).await);
    set_identity_disabled(&e.app.db, "m4p", "grok", DEFAULT_ACCOUNT, true).await.unwrap();
    assert!(identity_disabled(&e.app.db, "m4p", "grok", DEFAULT_ACCOUNT).await);
    assert!(!identity_disabled(&e.app.db, "local", "grok", DEFAULT_ACCOUNT).await);
    set_identity_disabled(&e.app.db, "m4p", "grok", DEFAULT_ACCOUNT, false).await.unwrap();
    assert!(!identity_disabled(&e.app.db, "m4p", "grok", DEFAULT_ACCOUNT).await);
}

#[tokio::test]
async fn a_logged_out_grok_never_opens_a_probe_pane() {
    let e = tt::env().await;
    let app = e.app.clone();
    set_grok(&app, Some(false)).await;
    assert_eq!(super::quota_grok::refresh_grok(&app, LOCAL_HOST).await.unwrap(), false);
    assert_eq!(super::quota_grok::refresh_grok_if_due(&app, LOCAL_HOST).await.unwrap(), None);
    set_grok(&app, None).await;
    assert_eq!(super::quota_grok::refresh_grok(&app, LOCAL_HOST).await.unwrap(), false);
    assert!(e.herdr.calls_to("workspace.create").is_empty());
}

#[tokio::test]
async fn a_disabled_default_grok_account_never_opens_a_probe_pane() {
    let e = tt::env().await;
    let app = e.app.clone();
    set_grok(&app, Some(true)).await;
    set_identity_disabled(&app.db, LOCAL_HOST, "grok", DEFAULT_ACCOUNT, true).await.unwrap();
    assert_eq!(super::quota_grok::refresh_grok(&app, LOCAL_HOST).await.unwrap(), false);
    assert_eq!(super::quota_grok::refresh_grok_if_due(&app, LOCAL_HOST).await.unwrap(), None);
    assert!(e.herdr.calls_to("workspace.create").is_empty());
}
