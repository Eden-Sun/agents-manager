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
        .replace("codex-key approval_policy 1", "codex-key approval_policy 0")
        .replace("codex-key features.hooks 1", "codex-key features.hooks 0");
    assert_eq!(ids(&out), vec!["codex.config.toml"]);
}

#[test]
fn codex_features_hooks_off_is_a_warning_and_default_mode_is_its_own_key() {
    let out = format!("{GOOD}AM_BL codex-key features.hooks 0\nAM_BL claude-key default defaultMode 0\n");
    assert!(ids(&out).contains(&"codex.config.toml:features.hooks".to_string()), "{out}");
    assert_eq!(sev(&out, "codex.config.toml:features.hooks"), WARN);
    assert!(ids(&out).contains(&"claude.default.settings.json:defaultMode".to_string()), "{out}");
    assert_eq!(sev(&out, "claude.default.settings.json:defaultMode"), WARN);
    let bare = out
        .replace("claude-file default settings.json 1", "claude-file default settings.json 0")
        .replace("claude-key default statusLine 1", "claude-key default statusLine 0")
        .replace("claude-key default hooks 1", "claude-key default hooks 0")
        .replace("claude-key default permissions 1", "claude-key default permissions 0");
    assert!(!ids(&bare).contains(&"claude.default.settings.json:defaultMode".to_string()), "{bare}");
}

/// 假的 $HOME：自己用 Drop 刪（#763：測試暫存目錄不能外洩）。
struct FakeHome(std::path::PathBuf);
impl FakeHome {
    fn new() -> Self {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-test-baseline-{}", crate::db::ulid())));
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
    run_probe_env(home, path, &[])
}

fn run_probe_env(home: &std::path::Path, path: &str, envs: &[(&str, &std::ffi::OsStr)]) -> String {
    // `am_abs` 是 PROBE_SH 的函式；BASELINE_SH 不依賴它（自己一次問完六個工具），這裡仍給一個替身，確保沒偷用。
    let script = format!("am_abs() {{ echo am_abs-called >&2; return 1; }}\n{BASELINE_SH}");
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env_clear()
        .env("HOME", home)
        .env("PATH", path)
        .envs(envs.iter().map(|(k, v)| (*k, *v)))
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

    // 外層 PATH 故意沒有 rtk，但登入 shell 會重讀使用者的 profile。有絕對路徑就是找到了，空的才是缺。
    let rtk = out.lines().find(|l| l.starts_with("AM_BL tool rtk ")).expect("rtk tool line");
    let rtk_path = &rtk["AM_BL tool rtk ".len()..];
    if rtk_path.is_empty() {
        assert!(ids.contains(&"tool.rtk".to_string()), "{ids:?}");
    } else {
        assert!(rtk_path.starts_with('/'), "{rtk}");
        assert!(!ids.contains(&"tool.rtk".to_string()), "{ids:?}");
    }
    assert!(ids.contains(&"claude.default.statusline-command.sh".to_string()), "{ids:?}");
    assert!(ids.contains(&"claude.default.settings.json:hooks".to_string()), "{ids:?}");
    assert!(!ids.contains(&"claude.default.settings.json:statusLine".to_string()), "{ids:?}");
    assert!(ids.contains(&"claude.cc2.settings.json:statusLine".to_string()), "{ids:?}");
    assert!(!ids.contains(&"codex.config.toml:approval_policy".to_string()), "{ids:?}");
    assert!(ids.contains(&"codex.config.toml:features.hooks".to_string()), "{ids:?}");
    assert!(ids.contains(&"claude.default.settings.json:defaultMode".to_string()), "{ids:?}");
    assert!(ids.contains(&"codex.hooks.json".to_string()), "{ids:?}");
    assert!(ids.contains(&"gitconfig.token".to_string()), "{ids:?}");
    assert!(!out.contains("s3cret"), "token must never leave the host: {out}");
    assert_eq!(snapshot(h), before, "the baseline probe is read-only");
}

#[test]
fn an_alias_outside_the_claude_cc_glob_is_still_checked() {
    // 票面：每個 claude 身分看它自己的 CLAUDE_CONFIG_DIR，不只有 ~/.claude 與 ~/.claude-cc<數字>。
    let home = FakeHome::new();
    let h = home.path();
    fs::create_dir_all(h.join(".claude-work")).unwrap();
    fs::write(h.join(".claude-work/settings.json"), "{}").unwrap();
    fs::write(h.join(".zshrc"), "alias cc5='CLAUDE_CONFIG_DIR=$HOME/.claude-work claude'\n").unwrap();
    let out = run_probe(h, "/usr/bin:/bin");
    let all = ids(&out);
    assert!(all.contains(&"claude.cc5.statusline-command.sh".to_string()), "{all:?}\n{out}");
    assert!(all.contains(&"claude.cc5.settings.json:statusLine".to_string()), "{all:?}");
    assert!(!out.contains(".claude-work"), "路徑不輸出");
}

#[test]
fn an_alias_using_braced_home_expansion_is_still_checked() {
    let home = FakeHome::new();
    let h = home.path();
    fs::create_dir_all(h.join(".claude-work")).unwrap();
    fs::write(h.join(".claude-work/settings.json"), "{}").unwrap();
    fs::write(h.join(".zshrc"), "alias cc5='CLAUDE_CONFIG_DIR=\"${HOME}/.claude-work\" claude'\n").unwrap();
    let out = run_probe(h, "/usr/bin:/bin");
    let all = ids(&out);
    assert!(all.contains(&"claude.cc5.settings.json:statusLine".to_string()), "${{HOME}} alias directory was skipped: {all:?}\n{out}");
    assert!(!out.contains(".claude-work"), "路徑不輸出");
}

#[test]
fn codex_features_hooks_true_under_a_features_table_is_present() {
    let home = FakeHome::new();
    let h = home.path();
    fs::create_dir_all(h.join(".codex")).unwrap();
    fs::write(h.join(".codex/config.toml"), "[features]\nhooks = true\napproval_policy = \"on-request\"\n").unwrap();
    fs::write(h.join(".codex/hooks.json"), "{}\n").unwrap();
    let out = run_probe(h, "/usr/bin:/bin");
    assert!(out.contains("AM_BL codex-key features.hooks 1"), "{out}");
    assert!(!ids(&out).contains(&"codex.config.toml:features.hooks".to_string()), "{out}");
    fs::write(h.join(".codex/config.toml"), "features.hooks = true\napproval_policy = \"on-request\"\n").unwrap();
    let dotted = run_probe(h, "/usr/bin:/bin");
    assert!(dotted.contains("AM_BL codex-key features.hooks 1"), "{dotted}");
    fs::write(h.join(".codex/config.toml"), "[features]\nhooks = false\n").unwrap();
    let off = run_probe(h, "/usr/bin:/bin");
    assert!(off.contains("AM_BL codex-key features.hooks 0"), "{off}");
}

#[test]
fn codex_hooks_in_another_table_or_with_a_non_boolean_value_are_not_compliant() {
    let home = FakeHome::new();
    let h = home.path();
    fs::create_dir_all(h.join(".codex")).unwrap();
    let config = h.join(".codex/config.toml");
    fs::write(&config, "[other]\nfeatures.hooks = true\n").unwrap();
    let nested = run_probe(h, "/usr/bin:/bin");
    assert!(nested.contains("AM_BL codex-key features.hooks 0"), "nested-table key must not satisfy root features.hooks: {nested}");
    fs::write(&config, "[features]\nhooks = trueish\n").unwrap();
    let malformed = run_probe(h, "/usr/bin:/bin");
    assert!(malformed.contains("AM_BL codex-key features.hooks 0"), "a non-boolean value must not satisfy hooks=true: {malformed}");
}

#[test]
fn claude_default_mode_in_an_unrelated_json_object_does_not_satisfy_permissions() {
    let home = FakeHome::new();
    let h = home.path();
    fs::create_dir_all(h.join(".claude")).unwrap();
    fs::write(h.join(".claude/settings.json"), r#"{"other":{"defaultMode":"default"}}"#).unwrap();
    let out = run_probe(h, "/usr/bin:/bin");
    assert!(out.contains("AM_BL claude-key default defaultMode 0"), "only permissions.defaultMode is the configured mode: {out}");
}

#[test]
fn a_host_with_no_claude_dir_at_all_reports_no_claude_identity() {
    let home = FakeHome::new();
    let out = run_probe(home.path(), "/usr/bin:/bin");
    assert!(!ids(&out).iter().any(|i| i.starts_with("claude.")), "{out}");
}

#[test]
fn the_same_claude_dir_reached_twice_is_reported_once() {
    // daemon 自己的環境若有 CLAUDE_CONFIG_DIR 指到 ~/.claude-ccN（或 ~/.claude），迴圈會走到同一個目錄兩次。
    let home = FakeHome::new();
    let h = home.path();
    fs::create_dir_all(h.join(".claude-cc2")).unwrap();
    fs::write(h.join(".claude-cc2/settings.json"), "{}").unwrap();
    let out = run_probe_env(h, "/usr/bin:/bin", &[("CLAUDE_CONFIG_DIR", h.join(".claude-cc2").as_os_str())]);
    let all = ids(&out);
    let mut uniq = all.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(all.len(), uniq.len(), "duplicate issue ids: {all:?}");
    assert!(all.contains(&"claude.cc2.settings.json:statusLine".to_string()), "{all:?}");
}

#[test]
fn evaluate_never_lists_the_same_issue_twice() {
    let dup = GOOD.replace(
        "AM_BL codex-file config.toml 1",
        "AM_BL claude-file default settings.json 1
AM_BL claude-file default RTK.md 0
AM_BL claude-file default RTK.md 0
AM_BL codex-file config.toml 1",
    );
    assert_eq!(ids(&dup), vec!["claude.default.RTK.md"]);
}

#[test]
fn a_hostile_directory_name_cannot_forge_probe_lines() {
    // `~/.claude-cc<數字>*` 的 glob 什麼名字都收：空白會讓欄位錯位、換行可以偽造 `AM_BL end` 或假的缺項。
    let home = FakeHome::new();
    let h = home.path();
    fs::create_dir_all(h.join(".claude-cc1 x")).unwrap();
    fs::create_dir_all(h.join(".claude-cc3\nAM_BL end")).unwrap();
    fs::create_dir_all(h.join(".claude-cc4\nAM_BL tool rtk")).unwrap();
    let out = run_probe(h, "/usr/bin:/bin");
    assert_eq!(out.lines().filter(|l| *l == "AM_BL end").count(), 1, "forged end marker: {out}");
    assert!(out.lines().all(|l| l.starts_with("AM_BL ")), "a line escaped the AM_BL framing: {out}");
    // 真的 rtk 可以是絕對路徑（登入 shell 的 Homebrew）。偽造是多一行，或路徑不是絕對路徑。
    let rtk: Vec<_> = out.lines().filter(|l| l.starts_with("AM_BL tool rtk")).collect();
    assert_eq!(rtk.len(), 1, "forged tool line: {out}");
    let rtk_path = &rtk[0]["AM_BL tool rtk ".len()..];
    assert!(rtk_path.is_empty() || rtk_path.starts_with('/'), "{out}");
    let all = ids(&out);
    assert!(!all.iter().any(|i| i.contains(' ') || i.contains('\n')), "{all:?}");
    assert!(!all.iter().any(|i| i.starts_with("claude.cc1") || i.starts_with("claude.cc3") || i.starts_with("claude.cc4")), "{all:?}");
}

#[test]
fn the_tools_are_looked_up_with_one_login_shell_not_six() {
    // 每次 `$SHELL -lic` 都要讀完整個 rc（nvm／conda 動輒數秒）；PROBE_SH 自己已經開了好幾次，
    // 再加六次會把整趟探測推過 30 秒上限，連原本的 tools 偵測都跟著失敗。
    let home = FakeHome::new();
    let h = home.path();
    let counter = h.join("shell-calls");
    let wrapper = h.join("fake-shell");
    fs::write(&wrapper, format!("#!/bin/sh\necho x >> {}\nexec /bin/sh \"$@\"\n", counter.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = run_probe_env(h, "/usr/bin:/bin", &[("SHELL", wrapper.as_os_str())]);
    assert!(out.contains("AM_BL end"), "{out}");
    let calls = fs::read_to_string(&counter).map_or(0, |c| c.lines().count());
    assert!(calls <= 1, "BASELINE_SH opened {calls} login shells");
}

#[test]
fn a_token_used_as_the_url_user_is_also_flagged_without_echoing_it() {
    let home = FakeHome::new();
    let h = home.path();
    fs::write(h.join(".gitconfig"), "[url \"https://ghp_abc123SECRET@github.com/\"]\n  insteadOf = https://github.com/\n").unwrap();
    let out = run_probe(h, "/usr/bin:/bin");
    assert!(ids(&out).contains(&"gitconfig.token".to_string()), "{out}");
    assert!(!out.contains("abc123SECRET"), "{out}");
    // 一般的 user@host（SSH 風格、沒有密碼）不算。
    fs::write(h.join(".gitconfig"), "[url \"https://git@github.com/\"]\n  insteadOf = https://github.com/\n").unwrap();
    assert!(!ids(&run_probe(h, "/usr/bin:/bin")).contains(&"gitconfig.token".to_string()));
}

fn snapshot(root: &std::path::Path) -> Vec<(String, Vec<u8>, u32, i64, i64)> {
    fn walk(p: &std::path::Path, root: &std::path::Path, acc: &mut Vec<(String, Vec<u8>, u32, i64, i64)>) {
        for e in fs::read_dir(p).unwrap().flatten() {
            let path = e.path();
            let meta = e.metadata().unwrap();
            use std::os::unix::fs::MetadataExt;
            let data = if meta.is_file() { fs::read(&path).unwrap() } else { vec![] };
            acc.push((
                path.strip_prefix(root).unwrap().display().to_string(),
                data,
                meta.mode(),
                meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
                meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
            ));
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

// ───────── 第二版（#719-b）：darwin-only、inbox 通知 ─────────

const MAC_PLUGINS_MISSING: &str = "AM_BL claude-plugin default imessage 0
AM_BL claude-plugin default discord 0
";

fn with_os(os: &str) -> String {
    GOOD.replace("AM_BL begin\n", &format!("AM_BL begin\nAM_BL os {os}\n"))
        .replace("AM_BL claude-key default permissions 1\n", &format!("AM_BL claude-key default permissions 1\n{MAC_PLUGINS_MISSING}"))
}

#[test]
fn mac_only_items_are_not_missing_on_a_linux_host() {
    let out = with_os("Linux");
    assert_eq!(os_of(&out).as_deref(), Some("Linux"));
    assert_eq!(evaluate(&out), Some(vec![]));
}

#[test]
fn mac_only_items_are_listed_on_a_mac() {
    let out = with_os("Darwin");
    assert_eq!(os_of(&out).as_deref(), Some("Darwin"));
    assert_eq!(ids(&out), vec!["claude.default.plugin:imessage", "claude.default.plugin:discord"]);
    assert_eq!(sev(&out, "claude.default.plugin:imessage"), WARN);
}

#[test]
fn an_unknown_os_never_counts_mac_only_items() {
    // 舊探測沒帶 `AM_BL os`：不知道是哪個系統就不能說缺 Mac 專用項。
    let out = GOOD.replace("AM_BL claude-key default permissions 1\n", &format!("AM_BL claude-key default permissions 1\n{MAC_PLUGINS_MISSING}"));
    assert_eq!(os_of(&out), None);
    assert_eq!(evaluate(&out), Some(vec![]));
}

#[test]
fn mac_plugins_are_not_asked_about_when_settings_json_is_missing() {
    let out = with_os("Darwin")
        .replace("claude-file default settings.json 1", "claude-file default settings.json 0")
        .replace("claude-key default statusLine 1", "claude-key default statusLine 0")
        .replace("claude-key default hooks 1", "claude-key default hooks 0")
        .replace("claude-key default permissions 1", "claude-key default permissions 0");
    assert_eq!(ids(&out), vec!["claude.default.settings.json"]);
}

#[test]
fn the_real_script_reports_the_os_and_checks_plugins_by_name() {
    let home = FakeHome::new();
    let h = home.path();
    fs::create_dir_all(h.join(".claude")).unwrap();
    fs::write(h.join(".claude/settings.json"), r#"{ "enabledPlugins": { "imessage@claude-plugins-official": true } }"#).unwrap();
    let out = run_probe(h, "/usr/bin:/bin");
    assert!(out.contains("AM_BL os "), "{out}");
    assert!(out.contains("AM_BL claude-plugin default imessage 1"), "{out}");
    assert!(out.contains("AM_BL claude-plugin default discord 0"), "{out}");
}

fn report(issues: Option<Vec<BaselineIssue>>) -> BaselineReport {
    BaselineReport { os: Some("Linux".into()), issues, checked_at: "2026-10-02T00:00:00.000Z".into(), failed_at: None, error: None, stale: false }
}

fn bi(id: &str, severity: &'static str) -> BaselineIssue {
    BaselineIssue { id: id.into(), severity, message: format!("{id} message") }
}

#[test]
fn nothing_is_pushed_for_a_consistent_or_unknown_host() {
    assert!(alert_for("ubuntu", &report(Some(vec![]))).is_none());
    assert!(alert_for("ubuntu", &report(None)).is_none(), "unknown is not a difference");
}

#[test]
fn the_same_difference_has_the_same_key_whatever_the_order_and_a_changed_one_does_not() {
    let a = alert_for("ubuntu", &report(Some(vec![bi("tool.rtk", CRITICAL), bi("claude.cc2.settings.json", CRITICAL)]))).unwrap();
    let b = alert_for("ubuntu", &report(Some(vec![bi("claude.cc2.settings.json", CRITICAL), bi("tool.rtk", CRITICAL)]))).unwrap();
    let c = alert_for("ubuntu", &report(Some(vec![bi("tool.rtk", CRITICAL)]))).unwrap();
    let other_host = alert_for("m4p", &report(Some(vec![bi("tool.rtk", CRITICAL)]))).unwrap();
    assert_eq!(a.0, b.0);
    assert_ne!(a.0, c.0);
    assert_ne!(c.0, other_host.0, "each host is its own difference");
    assert!(a.0.starts_with("ops_alert:daemon:host_baseline:ubuntu:"), "{}", a.0);
    assert_eq!(a.1["source"], "daemon");
    assert_eq!(a.1["reason"], "host_baseline");
    assert_eq!(a.1["subject"], "ubuntu");
    assert_eq!(a.1["critical"], 2);
    let listed = a.1["issues"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
}

async fn inbox_keys(app: &std::sync::Arc<crate::state::App>) -> Vec<String> {
    crate::supervisor::store::inbox(&app.db, 50).await.unwrap().into_iter().map(|e| e.event_key).collect()
}

#[tokio::test]
async fn notify_pushes_one_inbox_event_per_distinct_difference() {
    let env = crate::testing::env().await;
    let app = &env.app;
    let first = report(Some(vec![bi("tool.rtk", CRITICAL)]));
    notify(app, "ubuntu", &first).await;
    notify(app, "ubuntu", &first).await; // 重連、定期重量：同一份差異不再推
    assert_eq!(inbox_keys(app).await.len(), 1);

    notify(app, "ubuntu", &report(Some(vec![bi("tool.rtk", CRITICAL), bi("tool.zsh", CRITICAL)]))).await;
    assert_eq!(inbox_keys(app).await.len(), 2, "a changed difference is pushed again");

    notify(app, "ubuntu", &report(Some(vec![]))).await;
    notify(app, "ubuntu", &report(None)).await;
    assert_eq!(inbox_keys(app).await.len(), 2, "consistent / unknown pushes nothing");
}

#[tokio::test]
async fn a_handled_baseline_alert_keeps_its_permanent_dedupe_key_after_retention() {
    let env = crate::testing::env().await;
    let app = &env.app;
    let baseline = report(Some(vec![bi("tool.rtk", CRITICAL)]));
    notify(app, "ubuntu", &baseline).await;
    let key = inbox_keys(app).await.into_iter().next().expect("alert inserted");
    assert!(key.starts_with("ops_alert:daemon:host_baseline:ubuntu:"), "{key}");
    sqlx::query("UPDATE supervisor_inbox SET state='handled', updated_at=? WHERE event_key=?")
        .bind(crate::db::iso_in(-61 * 24 * 3600))
        .bind(&key)
        .execute(&app.db)
        .await
        .unwrap();

    let pruned = crate::supervisor::store::prune_handled_events(&app.db, crate::supervisor::store::PRUNE_HANDLED_AFTER_SECS)
        .await
        .unwrap();
    assert_eq!(pruned, 0, "baseline event_key is permanent even after it is handled");
    notify(app, "ubuntu", &baseline).await;
    assert_eq!(inbox_keys(app).await, vec![key], "a repaired-then-regressed difference must stay deduplicated");
}

#[test]
fn mac_only_plugins_match_the_script() {
    assert!(BASELINE_SH.contains(&format!("MAC_ONLY_PLUGINS=\"{}\"", MAC_ONLY_PLUGINS.join(" "))));
}

// ───────── 過期標記：偵測失敗或太久沒量，不能讓舊結果看起來像現在的 ─────────

fn at(hours_ago: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::hours(hours_ago)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn fresh(issues: Option<Vec<BaselineIssue>>, checked_at: String) -> BaselineReport {
    BaselineReport { checked_at, ..report(issues) }
}

#[test]
fn a_report_is_stale_after_a_failed_probe_or_after_more_than_one_recheck_cycle() {
    // 對齊到毫秒：時間戳一律走 db::iso_at（毫秒寬度），邊界才不會被截斷成「早一點點」。
    let now = chrono::Utc::now();
    let now = now - chrono::Duration::nanoseconds(i64::from(now.timestamp_subsec_nanos() % 1_000_000));
    assert!(!fresh(Some(vec![]), at(0)).snapshot(now).stale, "剛量完不算過期");
    assert!(!fresh(Some(vec![]), at(6)).snapshot(now).stale, "一個重量週期內（含偵測自己花的時間）不算過期");
    let boundary = now - chrono::Duration::minutes(375);
    assert!(!fresh(Some(vec![]), crate::db::iso_at(boundary)).snapshot(now).stale, "6h15m 邊界仍有效");
    assert!(fresh(Some(vec![]), crate::db::iso_at(boundary - chrono::Duration::milliseconds(1))).snapshot(now).stale, "超過 6h15m 才過期");
    assert!(fresh(Some(vec![]), at(7)).snapshot(now).stale, "超過一個重量週期");
    let mut failed = fresh(Some(vec![bi("tool.rtk", CRITICAL)]), at(0));
    failed.failed_at = Some(at(0));
    failed.error = Some("ssh timeout".into());
    let snap = failed.snapshot(now);
    assert!(snap.stale, "最後一次偵測失敗");
    assert_eq!(snap.issues.as_ref().map(Vec::len), Some(1), "舊結果照樣留著給人看，只是標過期");
    // 時間讀不懂：不賭它新。
    assert!(fresh(Some(vec![]), "garbage".into()).snapshot(now).stale);
}

#[test]
fn stale_is_part_of_the_json_the_web_reads() {
    let mut r = fresh(Some(vec![]), at(0));
    r.failed_at = Some(at(0));
    r.error = Some("ssh timeout".into());
    let v = serde_json::to_value(r.snapshot(chrono::Utc::now())).unwrap();
    assert_eq!(v["stale"], true);
    assert_eq!(v["error"], "ssh timeout");
    assert!(v["failed_at"].is_string() && v["checked_at"].is_string());
}

#[tokio::test]
async fn a_failed_detection_marks_the_kept_baseline_stale_and_a_good_one_clears_it() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let host = format!("stale-{}", crate::db::ulid().to_ascii_lowercase());
    let conn = app
        .hosts
        .insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        })
        .await;
    conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
    let ok = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let ok2 = ok.clone();
    crate::hosts::set_ssh_fake(&host, move |_| {
        if ok2.load(std::sync::atomic::Ordering::SeqCst) {
            Ok("AM_BL begin\nAM_BL os Linux\nAM_BL tool rtk \nAM_BL end\n".into())
        } else {
            Err(anyhow::anyhow!("ssh: connect to host timed out\nsecond line"))
        }
    });

    crate::tools::detect(&app, &host).await.unwrap();
    let first = app.host_baseline.lock().await.get(&host).cloned().unwrap();
    assert!(first.failed_at.is_none() && !first.snapshot(chrono::Utc::now()).stale);
    assert!(first.issues.as_ref().unwrap().iter().any(|i| i.id == "tool.rtk"));

    ok.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(crate::tools::detect(&app, &host).await.is_err());
    let kept = app.host_baseline.lock().await.get(&host).cloned().unwrap();
    assert_eq!(kept.checked_at, first.checked_at, "checked_at 是上一次成功的時間，不被失敗改寫");
    assert_eq!(kept.issues, first.issues, "舊結果留著");
    assert!(kept.failed_at.is_some());
    assert_eq!(kept.error.as_deref(), Some("ssh: connect to host timed out"), "只留第一行");
    assert!(kept.snapshot(chrono::Utc::now()).stale);

    ok.store(true, std::sync::atomic::Ordering::SeqCst);
    crate::tools::detect(&app, &host).await.unwrap();
    let healed = app.host_baseline.lock().await.get(&host).cloned().unwrap();
    assert!(healed.failed_at.is_none() && healed.error.is_none() && !healed.snapshot(chrono::Utc::now()).stale);
}

#[tokio::test]
async fn a_failed_detection_with_no_earlier_baseline_invents_none() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    note_failure(&app, "never-measured", &anyhow::anyhow!("boom")).await;
    assert!(app.host_baseline.lock().await.get("never-measured").is_none(), "沒量過就維持「尚未檢查」，不憑空造一份");
}
