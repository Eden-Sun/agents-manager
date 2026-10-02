use super::*;
use std::fs;
use std::process::Command;

fn ids(out: &str) -> Vec<String> {
    evaluate(out).expect("probe finished").into_iter().map(|i| i.id).collect()
}

fn sev(out: &str, id: &str) -> &'static str {
    evaluate(out).unwrap().into_iter().find(|i| i.id == id).unwrap_or_else(|| panic!("no issue {id}")).severity
}

const GOOD: &str = "AM_BL begin
AM_BL tool herdr /u/herdr
AM_BL tool rtk /u/rtk
AM_BL tool zsh /u/zsh
AM_BL tool bun /u/bun
AM_BL tool jq /u/jq
AM_BL tool gh /u/gh
AM_BL claude-dir default /h/.claude
AM_BL claude-file default settings.json 1
AM_BL claude-file default statusline-command.sh 1
AM_BL claude-file default CLAUDE.md 1
AM_BL claude-file default RTK.md 1
AM_BL claude-key default statusLine 1
AM_BL claude-key default hooks 1
AM_BL claude-key default permissions 1
AM_BL codex-file config.toml 1
AM_BL codex-file hooks.json 1
AM_BL codex-key approval_policy 1
AM_BL grok-file config.toml 1
AM_BL herdr-file config.toml 1
AM_BL gitconfig-token 0
AM_BL end
";

#[test]
fn a_host_that_matches_the_baseline_has_no_issues() {
    assert_eq!(evaluate(GOOD), Some(vec![]));
}

#[test]
fn a_probe_cut_short_is_unknown_not_everything_missing() {
    // ssh 逾時、被截斷：沒有 `AM_BL end`。一次逾時不能在每台主機上喊一排缺漏。
    let cut = GOOD.replace("AM_BL end\n", "");
    assert_eq!(evaluate(&cut), None);
    assert_eq!(evaluate(""), None);
}

#[test]
fn a_missing_tool_is_critical_and_named() {
    let out = GOOD.replace("AM_BL tool rtk /u/rtk", "AM_BL tool rtk ");
    assert_eq!(ids(&out), vec!["tool.rtk"]);
    assert_eq!(sev(&out, "tool.rtk"), CRITICAL);
}

#[test]
fn every_claude_identity_is_checked_on_its_own() {
    let out = GOOD.replace(
        "AM_BL codex-file config.toml 1",
        "AM_BL claude-dir cc1 /h/.claude-cc1
AM_BL claude-file cc1 settings.json 1
AM_BL claude-file cc1 statusline-command.sh 0
AM_BL claude-file cc1 CLAUDE.md 1
AM_BL claude-file cc1 RTK.md 0
AM_BL claude-key cc1 statusLine 0
AM_BL claude-key cc1 hooks 1
AM_BL claude-key cc1 permissions 1
AM_BL codex-file config.toml 1",
    );
    assert_eq!(
        ids(&out),
        vec!["claude.cc1.statusline-command.sh", "claude.cc1.RTK.md", "claude.cc1.settings.json:statusLine"]
    );
    assert_eq!(sev(&out, "claude.cc1.statusline-command.sh"), CRITICAL);
    assert_eq!(sev(&out, "claude.cc1.settings.json:statusLine"), CRITICAL);
    assert_eq!(sev(&out, "claude.cc1.RTK.md"), WARN);
}

#[test]
fn a_missing_settings_file_does_not_also_list_every_key_in_it() {
    let out = GOOD
        .replace("claude-file default settings.json 1", "claude-file default settings.json 0")
        .replace("claude-key default statusLine 1", "claude-key default statusLine 0")
        .replace("claude-key default hooks 1", "claude-key default hooks 0")
        .replace("claude-key default permissions 1", "claude-key default permissions 0");
    assert_eq!(ids(&out), vec!["claude.default.settings.json"]);
}

#[test]
fn codex_gaps_and_a_token_in_gitconfig_are_warnings() {
    let out = GOOD
        .replace("codex-key approval_policy 1", "codex-key approval_policy 0")
        .replace("codex-file hooks.json 1", "codex-file hooks.json 0")
        .replace("grok-file config.toml 1", "grok-file config.toml 0")
        .replace("herdr-file config.toml 1", "herdr-file config.toml 0")
        .replace("gitconfig-token 0", "gitconfig-token 1");
    assert_eq!(
        ids(&out),
        vec!["codex.hooks.json", "codex.config.toml:approval_policy", "grok.config.toml", "herdr.config.toml", "gitconfig.token"]
    );
    for id in ids(&out) {
        assert_eq!(sev(&out, &id), WARN, "{id}");
    }
    // 只報有，不能把 token 本身帶進訊息。
    assert!(!format!("{:?}", evaluate(&out)).contains("ghp_"));
}

#[test]
fn a_missing_codex_config_does_not_also_list_its_keys() {
    let out = GOOD
        .replace("codex-file config.toml 1", "codex-file config.toml 0")
        .replace("codex-key approval_policy 1", "codex-key approval_policy 0");
    assert_eq!(ids(&out), vec!["codex.config.toml"]);
}

/// 假的 $HOME：自己用 Drop 刪（#763：測試暫存目錄不能外洩）。
struct FakeHome(std::path::PathBuf);
impl FakeHome {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("am-test-baseline-{}", crate::db::ulid()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for FakeHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_probe(home: &std::path::Path, path: &str) -> String {
    // `am_abs` 是 PROBE_SH 的函式；這裡用 `command -v` 代替，只量 BASELINE_SH 本身。
    let script = format!("am_abs() {{ command -v \"$1\" 2>/dev/null; }}\n{BASELINE_SH}");
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env_clear()
        .env("HOME", home)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn the_real_script_reads_a_fake_home_and_writes_nothing() {
    let home = FakeHome::new();
    let h = home.path();
    fs::create_dir_all(h.join(".claude")).unwrap();
    fs::write(h.join(".claude/settings.json"), r#"{ "statusLine": {"type":"command"}, "permissions": {} }"#).unwrap();
    fs::write(h.join(".claude/CLAUDE.md"), "x").unwrap();
    fs::create_dir_all(h.join(".claude-cc2")).unwrap();
    fs::write(h.join(".claude-cc2/settings.json"), "{}").unwrap();
    fs::create_dir_all(h.join(".codex")).unwrap();
    fs::write(h.join(".codex/config.toml"), "approval_policy = \"never\"\n").unwrap();
    fs::write(h.join(".gitconfig"), "[url \"https://bot:s3cret@github.com/\"]\n  insteadOf = https://github.com/\n").unwrap();
    let before = snapshot(h);

    let out = run_probe(h, "/usr/bin:/bin");
    let ids = ids(&out);

    assert!(ids.contains(&"tool.rtk".to_string()), "{ids:?}");
    assert!(ids.contains(&"claude.default.statusline-command.sh".to_string()), "{ids:?}");
    assert!(ids.contains(&"claude.default.settings.json:hooks".to_string()), "{ids:?}");
    assert!(!ids.contains(&"claude.default.settings.json:statusLine".to_string()), "{ids:?}");
    assert!(ids.contains(&"claude.cc2.settings.json:statusLine".to_string()), "{ids:?}");
    assert!(!ids.contains(&"codex.config.toml:approval_policy".to_string()), "{ids:?}");
    assert!(ids.contains(&"codex.hooks.json".to_string()), "{ids:?}");
    assert!(ids.contains(&"gitconfig.token".to_string()), "{ids:?}");
    assert!(!out.contains("s3cret"), "token must never leave the host: {out}");
    assert_eq!(snapshot(h), before, "the baseline probe is read-only");
}

#[test]
fn a_host_with_no_claude_dir_at_all_reports_no_claude_identity() {
    let home = FakeHome::new();
    let out = run_probe(home.path(), "/usr/bin:/bin");
    assert!(!ids(&out).iter().any(|i| i.starts_with("claude.")), "{out}");
}

fn snapshot(root: &std::path::Path) -> Vec<(String, u64)> {
    fn walk(p: &std::path::Path, root: &std::path::Path, acc: &mut Vec<(String, u64)>) {
        for e in fs::read_dir(p).unwrap().flatten() {
            let path = e.path();
            let meta = e.metadata().unwrap();
            acc.push((path.strip_prefix(root).unwrap().display().to_string(), meta.len()));
            if meta.is_dir() {
                walk(&path, root, acc);
            }
        }
    }
    let mut acc = Vec::new();
    walk(root, root, &mut acc);
    acc.sort();
    acc
}
