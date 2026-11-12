use super::*;

fn identity(name: &str, account: &str) -> crate::tools::IdentityInfo {
    crate::tools::IdentityInfo {
        name: name.to_string(),
        kind: "claude".to_string(),
        logged_in: Some(true),
        reason: None,
        account: Some(account.to_string()),
        plan: None,
        source: crate::tools::SOURCE_CONFIG,
        config_dir: None,
    }
}

fn host_tools(identity: crate::tools::IdentityInfo) -> crate::tools::HostTools {
    crate::tools::HostTools {
        tools: std::collections::BTreeMap::new(),
        identities: [(identity.name.clone(), identity)].into_iter().collect(),
        shell_identities: Vec::new(),
        utc_offset_secs: None, herdr_cli: None, checked_at: String::new(),
    }
}

#[test]
fn remote_bot_uses_remote_identity_account_not_local_account() {
    let mut tools = std::collections::HashMap::new();
    tools.insert("local".to_string(), host_tools(identity("cc1", "local@example.com")));
    tools.insert("remote".to_string(), host_tools(identity("cc1", "remote@example.com")));

    let (account, warning) = claude_account_from_tools(&tools, "remote", Some("cc1"));

    assert_eq!(account.as_deref(), Some("remote@example.com"));
    assert_eq!(warning, None);
}

#[test]
fn local_identity_probe_miss_does_not_fall_back_to_default_account() {
    let mut tools = std::collections::HashMap::new();
    tools.insert("local".to_string(), host_tools(identity("cc0", "stale@example.com")));

    let (account, warning) = claude_account_from_tools(&tools, "local", Some("cc1"));

    assert_eq!(account, None);
    assert_eq!(warning, None);
}

#[test]
fn default_bot_does_not_show_stale_account_when_host_says_logged_out() {
    let mut default_identity = identity("cc0", "stale@example.com");
    default_identity.logged_in = Some(false);
    let mut tools = std::collections::HashMap::new();
    tools.insert("local".to_string(), host_tools(default_identity));

    let (account, warning) = claude_account_from_tools(&tools, "local", None);

    assert_eq!(account, None);
    assert_eq!(warning, None);
}
