//! `AM_IDENTITIES`（issue #1775）：這台主機看得到的身分清單，注入母 bot 的 pane。herdr shim 在那個 pane 裡查
//! `herdr agent start --identity <名>`，挑出子 pane 要用的帳號目錄；以前子 agent 只能吃母 pane 的帳號，要換身分就只剩自己起 CLI 這條路。

use crate::config::{expand_home, IdentityCfg};

/// 身分裡帶進清單的 key：只有帳號目錄（shim 的 `AM_RESERVED_ENV_KEYS` 也只認這三個）。其他 key 可能是憑證，不進 pane env。
const ACCOUNT_KEYS: [&str; 3] = ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "GROK_HOME"];

/// 名稱與 kind 不能含的字元：`/`（kind 與名稱的分界）、`:`（名稱與 KEY 的分界）、`;`（筆與筆）、`,`（KEY 與 KEY）。
const BAD_NAME: [char; 4] = ['/', ':', ';', ','];
/// 路徑值不能含的字元：`;` 與 `,`（同上）。`:` 與 `=` 在路徑裡合法，shim 會切在第一個。
const BAD_VALUE: [char; 2] = [';', ','];

/// 一台主機的身分清單，單行、`;` 分隔，格式 `<kind>/<名稱>[:KEY=路徑,KEY=路徑…]`。路徑已展開成那台的家目錄。
/// 含控制字元或分隔字元的身分整個略過：寧可不列，也不讓一筆壞資料把整串切錯。
pub fn identities_env(identities: &[IdentityCfg], home: &str) -> String {
    identities.iter().filter_map(|i| identity_entry(i, home)).collect::<Vec<_>>().join(";")
}

fn identity_entry(i: &IdentityCfg, home: &str) -> Option<String> {
    if !token_ok(&i.kind, &BAD_NAME) || !token_ok(&i.name, &BAD_NAME) {
        return None;
    }
    let mut kvs = Vec::new();
    for key in ACCOUNT_KEYS {
        let Some(raw) = i.env.get(key) else { continue };
        let value = expand_home(raw, home);
        if value.is_empty() {
            continue;
        }
        if !token_ok(&value, &BAD_VALUE) {
            return None;
        }
        kvs.push(format!("{key}={value}"));
    }
    let head = format!("{}/{}", i.kind, i.name);
    Some(if kvs.is_empty() { head } else { format!("{head}:{}", kvs.join(",")) })
}

fn token_ok(s: &str, bad: &[char]) -> bool {
    !s.is_empty() && s.chars().all(|c| !c.is_control() && !bad.contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(name: &str, kind: &str, env: &[(&str, &str)]) -> IdentityCfg {
        IdentityCfg {
            name: name.into(),
            kind: kind.into(),
            host: Some("box".into()),
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            args: vec![],
        }
    }

    /// 預設帳號（cc0）沒有目錄 key，只列名字；其他身分列出帳號目錄，`$HOME`／`~` 展開成那台的家目錄。
    #[test]
    fn each_identity_is_named_and_its_account_dir_is_expanded_on_that_host() {
        let list = [
            ident("cc0", "claude", &[]),
            ident("cc1", "claude", &[("CLAUDE_CONFIG_DIR", "$HOME/.claude-cc1")]),
            ident("cx1", "codex", &[("CODEX_HOME", "~/.codex-cx1")]),
        ];
        assert_eq!(
            identities_env(&list, "/home/remote"),
            "claude/cc0;claude/cc1:CLAUDE_CONFIG_DIR=/home/remote/.claude-cc1;codex/cx1:CODEX_HOME=/home/remote/.codex-cx1"
        );
    }

    /// 只帶帳號目錄那三個 key：其他 key（例如 API key）不進 pane env。
    #[test]
    fn only_the_account_dirs_are_listed_not_other_identity_env() {
        let list = [ident("cx1", "codex", &[("CODEX_HOME", "/h/.codex-cx1"), ("OPENAI_API_KEY", "sk-secret")])];
        assert_eq!(identities_env(&list, "/h"), "codex/cx1:CODEX_HOME=/h/.codex-cx1");
    }

    /// 名字或路徑含分隔字元、控制字元的那一筆整個略過，其他的照列。
    #[test]
    fn an_identity_that_would_break_the_line_is_dropped_and_the_rest_kept() {
        let list = [
            ident("bad;name", "claude", &[]),
            ident("cc1", "claude", &[("CLAUDE_CONFIG_DIR", "/h/a,b")]),
            ident("cc2", "claude", &[("CLAUDE_CONFIG_DIR", "/h/a\tb")]),
            ident("ok", "claude", &[]),
        ];
        assert_eq!(identities_env(&list, "/h"), "claude/ok");
    }

    /// 呼叫端（`pane_env_for_fence`）會拿這台 `identities_for_host` 的結果；沒有任何身分時是空字串，shim 就回「沒有這個身分」。
    #[test]
    fn no_identities_gives_an_empty_list() {
        assert_eq!(identities_env(&[], "/h"), "");
    }

    /// 規則文字：換身分要用 `--identity`、清單看 `$AM_IDENTITIES`，禁止自己 export／unset，也禁止用 pane run 換帳號。
    #[test]
    fn the_rules_switch_identity_with_the_flag_not_by_hand() {
        let rule = super::super::setup::child_agent_rules("proj-abc123");
        assert!(rule.contains("--identity <身分>"), "要說明換身分的旗標：{rule}");
        assert!(rule.contains("$AM_IDENTITIES"), "要說明這台可用的身分看哪裡");
        assert!(rule.contains("**禁止**自己 export／`env -u`／`unset`"), "禁止自己動帳號變數");
        assert!(rule.contains("**禁止**用 `herdr pane run <pane> \"claude …\"` 之類自己起 CLI 來換帳號"), "禁止用 pane run 換帳號");
        assert!(!rule.contains("禁止覆蓋"), "舊的「禁止覆蓋」規則要拿掉");
    }
}
