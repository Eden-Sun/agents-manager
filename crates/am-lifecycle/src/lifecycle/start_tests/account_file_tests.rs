use std::collections::BTreeMap;

/// #1125：身份沒設 `CLAUDE_CONFIG_DIR` 時，`.claude.json` 在家目錄（不是 `~/.claude/`）；設了就在那個目錄裡。
#[test]
fn the_account_file_of_an_identity_without_a_config_dir_is_in_home() {
    let none = BTreeMap::new();
    assert_eq!(super::claude_account_file_dir(&none, "/home/u"), "/home/u");
    let set: BTreeMap<String, String> = [("CLAUDE_CONFIG_DIR".to_string(), "~/.claude-cc1".to_string())].into();
    assert_eq!(super::claude_account_file_dir(&set, "/home/u"), "/home/u/.claude-cc1");
    let blank: BTreeMap<String, String> = [("CLAUDE_CONFIG_DIR".to_string(), "  ".to_string())].into();
    assert_eq!(super::claude_account_file_dir(&blank, "/home/u"), "/home/u");
}
