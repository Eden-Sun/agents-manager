
    use crate::app_ports_p3::install_via_bot;
    use crate::state::App;
    /// 現行 config.toml 的形狀（`[[identities]]` 不寫 host）在**本機**的行為一個字都不能變，
    /// 但不能再遮蔽遠端同名的 `ccN`——本機 cc1 與 m4p 的 cc1 是不同帳號（SPEC §16.2、review 2026-09-16）。
    /// 也不能因此讓遠端用不到它：codex／grok 身分只可能寫在 config 裡（review 2026-09-16 M6）。
    #[tokio::test]
    async fn a_config_identity_without_a_host_applies_everywhere_but_yields_to_that_hosts_own() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let cfg = |name: &str, kind: &str, host: Option<&str>, var: &str, dir: &str| crate::config::IdentityCfg {
            name: name.into(),
            kind: kind.into(),
            host: host.map(String::from),
            env: [(var.to_string(), dir.to_string())].into(),
            args: vec![],
        };
        app.cfg
            .update(|c| {
                c.identities = vec![
                    cfg("cc1", "claude", None, "CLAUDE_CONFIG_DIR", "/home/me/.claude-cc1"), // 現行形狀
                    cfg("cx2", "codex", None, "CODEX_HOME", "$HOME/.codex-cx2"),
                ];
                Ok(())
            })
            .await
            .unwrap();
        let dir_of = |i: Option<crate::config::IdentityCfg>, var: &str| i.and_then(|i| i.env.get(var).cloned());

        // 本機：照舊拿得到，env 也照舊。
        let local = identity_for_host(app, crate::config::LOCAL_HOST, "cc1").await;
        assert_eq!(dir_of(local, "CLAUDE_CONFIG_DIR").as_deref(), Some("/home/me/.claude-cc1"));

        // 遠端還沒偵測：codex 身分馬上能用（那台不可能有同名 shell 身分）；`ccN` 要等那台的 alias 讀過。
        assert_eq!(dir_of(identity_for_host(app, "m4p", "cx2").await, "CODEX_HOME").as_deref(), Some("$HOME/.codex-cx2"), "codex 身分只能寫在 config 裡，遠端要用得到");
        assert!(identity_for_host(app, "m4p", "cc1").await.is_none(), "還不知道 m4p 有沒有自己的 cc1");

        // m4p 偵測到自己的 cc1：那台的說了算，不被本機那筆遮蔽。
        let shell = |dir: Option<&str>| crate::tools::HostTools {
            tools: Default::default(),
            identities: Default::default(),
            shell_identities: dir.map(|d| vec![cfg("cc1", "claude", None, "CLAUDE_CONFIG_DIR", d)]).unwrap_or_default(),
            utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert("m4p".into(), shell(Some("$HOME/.claude-ccompany")));
        assert_eq!(dir_of(identity_for_host(app, "m4p", "cc1").await, "CLAUDE_CONFIG_DIR").as_deref(), Some("$HOME/.claude-ccompany"));
        // m4p 偵測完、沒有自己的 cc1：沒寫 host 的那筆就適用。
        app.tools.lock().await.insert("m4p".into(), shell(None));
        assert_eq!(dir_of(identity_for_host(app, "m4p", "cc1").await, "CLAUDE_CONFIG_DIR").as_deref(), Some("/home/me/.claude-cc1"));

        // 明寫 host 的最優先，而且只給那一台；`host = "local"` 就是只要本機。
        app.cfg
            .update(|c| {
                c.identities.push(cfg("cc1", "claude", Some("m4p"), "CLAUDE_CONFIG_DIR", "/home/m4p/.claude-ccompany"));
                c.identities.push(cfg("solo", "claude", Some("local"), "CLAUDE_CONFIG_DIR", "/home/me/.claude-solo"));
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(dir_of(identity_for_host(app, "m4p", "cc1").await, "CLAUDE_CONFIG_DIR").as_deref(), Some("/home/m4p/.claude-ccompany"));
        assert_eq!(dir_of(identity_for_host(app, crate::config::LOCAL_HOST, "cc1").await, "CLAUDE_CONFIG_DIR").as_deref(), Some("/home/me/.claude-cc1"));
        assert!(identity_for_host(app, "m4p", "solo").await.is_none(), "寫了 host = local 就只給本機");
        assert!(identity_for_host(app, crate::config::LOCAL_HOST, "solo").await.is_some());
    }

    /// 本機：沒寫 host 的 config 身分仍然蓋過同名的 shell alias（現行行為），偵測登入狀態用的清單跟啟動用的是同一份。
    #[test]
    fn detection_and_start_share_one_precedence() {
        let c = |name: &str, host: Option<&str>| crate::config::IdentityCfg { name: name.into(), kind: "claude".into(), host: host.map(String::from), env: Default::default(), args: vec![] };
        let config = vec![c("cc1", None), c("far", Some("m4p"))];
        let shell = vec![c("cc1", None), c("cc2", None)];
        let local = merge_identities(&config, crate::config::LOCAL_HOST, Some(&shell));
        assert_eq!(local.iter().map(|(i, s)| (i.name.as_str(), *s)).collect::<Vec<_>>(), [("cc1", SOURCE_CONFIG), ("cc2", SOURCE_SHELL)], "明寫給 m4p 的不出現在本機");
        let remote = merge_identities(&config, "m4p", Some(&shell));
        assert_eq!(remote.iter().map(|(i, s)| (i.name.as_str(), *s)).collect::<Vec<_>>(), [("far", SOURCE_CONFIG), ("cc1", SOURCE_SHELL), ("cc2", SOURCE_SHELL)]);
    }

    #[test]
    fn onboarding_flag_is_set_only_when_logged_in_and_missing() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-onboard-{}", ulid::Ulid::new())));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".claude.json");
        // No account: leave it alone (the TUI has to log in anyway).
        std::fs::write(&f, r#"{"theme":"dark"}"#).unwrap();
        assert!(!super::ensure_claude_onboarded(&dir));
        // Account but no flag: set it.
        std::fs::write(&f, r#"{"oauthAccount":{"emailAddress":"x@y"},"theme":"dark"}"#).unwrap();
        assert!(super::ensure_claude_onboarded(&dir));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["hasCompletedOnboarding"], true);
        assert_eq!(v["oauthAccount"]["emailAddress"], "x@y");
        // Already set: no rewrite.
        assert!(!super::ensure_claude_onboarded(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 帳號檔是 0600、走信任檔的鎖與原子寫：改完權限不變、目錄裡沒有暫存檔殘留（#1126）。
    #[test]
    fn onboarding_keeps_the_account_file_private_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-onboard-mode-{}", ulid::Ulid::new())));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".claude.json");
        std::fs::write(&f, r#"{"oauthAccount":{"emailAddress":"x@y"}}"#).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(super::ensure_claude_onboarded(&dir));
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600, "帳號檔不能被放寬");
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec![".claude.json".to_string()], "沒有留下暫存檔");
        // 沒有檔：不建。
        let empty = crate::testing::track(std::env::temp_dir().join(format!("am-onboard-none-{}", ulid::Ulid::new())));
        std::fs::create_dir_all(&empty).unwrap();
        assert!(!super::ensure_claude_onboarded(&empty));
        assert!(!empty.join(".claude.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&empty).unwrap();
    }

    use super::*;

    /// Real `alias` output from both machines (2026-09-06); m4p keys cc1 to `~/.claude-ccompany`.
    #[test]
    fn reads_ccn_aliases_off_the_shell() {
        let out = r#"
AM_PATH claude /opt/homebrew/bin/claude
AM_ALIAS cc='claude'
AM_ALIAS cc0='claude --dangerously-skip-permissions'
AM_ALIAS cc1='CLAUDE_CONFIG_DIR=$HOME/.claude-cc1 claude --dangerously-skip-permissions'
AM_ALIAS cc2='CLAUDE_CONFIG_DIR=$HOME/.claude-cc2 claude --dangerously-skip-permissions'
"#;
        let ids = parse_shell_identities(out);
        assert_eq!(ids.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["cc0", "cc1", "cc2"]);
        assert!(ids[0].env.is_empty(), "cc0 is the default account: no config dir");
        assert_eq!(ids[1].env["CLAUDE_CONFIG_DIR"], "$HOME/.claude-cc1");
        assert_eq!(ids[2].env["CLAUDE_CONFIG_DIR"], "$HOME/.claude-cc2");
        // The alias's own flags are never taken (the daemon owns those).
        assert!(ids.iter().all(|i| i.args.is_empty() && i.kind == "claude"));

        let remote = "AM_ALIAS cc1='CLAUDE_CONFIG_DIR=$HOME/.claude-ccompany claude --dangerously-skip-permissions'";
        assert_eq!(parse_shell_identities(remote)[0].env["CLAUDE_CONFIG_DIR"], "$HOME/.claude-ccompany");
    }

    #[test]
    fn ignores_aliases_that_are_not_ours() {
        // bash prints a leading `alias `; zsh does not. Both are read.
        let ids = parse_shell_identities("AM_ALIAS alias cc3=\"CLAUDE_CONFIG_DIR=~/.c3 claude\"");
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].env["CLAUDE_CONFIG_DIR"], "~/.c3");
        // Not claude, out of range, or the config dir set after the binary → skipped.
        for line in [
            "AM_ALIAS cc1='CLAUDE_CONFIG_DIR=$HOME/.x codex'",
            "AM_ALIAS cc7='CLAUDE_CONFIG_DIR=$HOME/.x claude'",
            "AM_ALIAS ccx='CLAUDE_CONFIG_DIR=$HOME/.x claude'",
            "AM_ALIAS cc1='claude --settings CLAUDE_CONFIG_DIR=$HOME/.x'",
            "AM_ALIAS cc2='env CLAUDE_CONFIG_DIR=$HOME/.x claude'",
        ] {
            let got = parse_shell_identities(line);
            // Never an empty-env identity that would run on the default account.
            assert!(got.is_empty(), "should not have made an identity from `{line}`: {got:?}");
        }
        // No config dir at all *is* the default account, and still counts.
        let ids = parse_shell_identities("AM_ALIAS cc0='claude --dangerously-skip-permissions'");
        assert_eq!(ids.len(), 1);
        assert!(ids[0].env.is_empty());
        // A later definition of the same name wins, the way the shell resolves it.
        let ids = parse_shell_identities(
            "AM_ALIAS cc1='CLAUDE_CONFIG_DIR=/a claude'\nAM_ALIAS cc1='CLAUDE_CONFIG_DIR=/b claude'",
        );
        assert_eq!(ids[0].env["CLAUDE_CONFIG_DIR"], "/b");
    }

    /// agy 沒有 `status` 指令，登入與否看憑證檔：探測腳本印 `AM_LOGIN agy 1／0`，解析要認得（額度那格的「未登入」靠它）。
    #[test]
    fn the_probe_script_asks_whether_agy_has_its_token_and_the_parser_reads_it() {
        assert!(PROBE_SH.contains("antigravity-cli/antigravity-oauth-token") && PROBE_SH.contains("AM_LOGIN agy 1") && PROBE_SH.contains("AM_LOGIN agy 0"));
        assert!(PROBE_SH.contains("security find-generic-password -s gemini -a antigravity"), "macOS 的 agy 憑證在 Keychain，不在檔案");
        let m = parse_probe("AM_PATH agy /h/.local/bin/agy\nAM_VER agy 1.2.16\nAM_LOGIN agy 0\n");
        assert_eq!(m["agy"].logged_in, Some(false));
        let m = parse_probe("AM_PATH agy /h/.local/bin/agy\nAM_LOGIN agy 1\n");
        assert_eq!(m["agy"].logged_in, Some(true));
        assert_eq!(parse_probe("AM_PATH agy \nAM_LOGIN agy 0\n")["agy"].logged_in, None, "沒裝＋沒憑證＝不知道，不是未登入");
    }

    #[test]
    fn parses_probe_output() {
        let out = "AM_PATH claude /opt/homebrew/bin/claude\nAM_VER claude 2.1.0 (Claude Code)\nAM_PATH codex /usr/local/bin/codex\nAM_VER codex codex-cli 0.120.0\nAM_PATH grok \nAM_LOGIN claude ?\nAM_LOGIN codex 1\nAM_LOGIN grok 0\n";
        let m = parse_probe(out);
        assert!(m["claude"].installed);
        assert_eq!(m["claude"].path.as_deref(), Some("/opt/homebrew/bin/claude"));
        assert_eq!(m["claude"].version.as_deref(), Some("2.1.0 (Claude Code)"));
        assert_eq!(m["claude"].logged_in, None);
        assert_eq!(m["codex"].logged_in, Some(true));
        assert!(!m["grok"].installed);
        assert!(m["grok"].path.is_none());
        // not installed + "no auth file" → unknown, not false
        assert_eq!(m["grok"].logged_in, None);
    }

    fn probe(name: &str, kind: &str, env: &[(&str, &str)]) -> IdentityProbe {
        IdentityProbe {
            name: name.into(),
            kind: kind.into(),
            bin: format!("/opt/homebrew/bin/{kind}"),
            env: env.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
            args: None,
        }
    }

    /// claude 只在本機問得到（ssh 讀不到 Keychain，會謊報 loggedIn:false）。
    #[test]
    fn claude_is_probed_locally_and_left_to_the_pane_when_remote() {
        assert_eq!(login_probe_args("claude", LOCAL_HOST), Some(CLAUDE_LOGIN_ARGS));
        assert_eq!(login_probe_args("claude", "m4p"), None);
        assert_eq!(login_probe_args("codex", "m4p"), Some(&["login", "status"][..]));
        // 腳本要用帶進來的 args，不是 kind 的預設（claude 的預設是 None，會整段被略過）。
        let mut it = probe("cc1", "claude", &[("CLAUDE_CONFIG_DIR", "/Users/m4p/.claude-ccompany")]);
        it.args = Some(CLAUDE_LOGIN_ARGS.to_vec());
        let sh = identity_probe_sh(&[it]);
        assert!(sh.contains("AM_IDENT_BEGIN %s\\n' 'cc1'"), "{sh}");
        assert!(sh.contains("'auth' 'status' '--json'"), "{sh}");
        assert!(sh.contains("CLAUDE_CONFIG_DIR='/Users/m4p/.claude-ccompany'"), "{sh}");
    }

    /// 這一輪問不到就沿用上一輪；真的登出（Some(false)）照樣覆蓋。
    #[test]
    fn a_pass_that_cannot_tell_keeps_the_last_known_answer() {
        let known = IdentityInfo {
            name: "cc1".into(),
            kind: "claude".into(),
            logged_in: Some(true),
            reason: Some("pane 探測".into()),
            account: Some("a@example.com".into()),
            plan: Some("team".into()),
            source: SOURCE_SHELL,
            config_dir: Some("/Users/m4p/.claude-ccompany".into()),
        };
        let mut unknown = IdentityInfo::unknown("cc1", "claude", SOURCE_SHELL, None);
        unknown.reason = Some("auth status 尚未取得結果".into());
        carry_over(&mut unknown, Some(&known));
        assert_eq!(unknown.logged_in, Some(true));
        assert_eq!(unknown.account.as_deref(), Some("a@example.com"));
        assert_eq!(unknown.plan.as_deref(), Some("team"));

        let mut logged_out = IdentityInfo::unknown("cc1", "claude", SOURCE_SHELL, None);
        logged_out.logged_in = Some(false);
        carry_over(&mut logged_out, Some(&known));
        assert_eq!(logged_out.logged_in, Some(false), "真的登出不能被舊答案蓋回去");
    }

    fn kinds(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(n, k)| ((*n).to_string(), (*k).to_string())).collect()
    }

    #[test]
    fn identity_script_exports_env_per_subshell() {
        let sh = identity_probe_sh(&[
            probe("gk0", "grok", &[]),
            probe("cx1", "codex", &[("CODEX_HOME", "/Users/m4p/.codex-alt")]),
        ]);
        assert!(sh.contains("AM_IDENT_BEGIN %s\\n' 'gk0'"));
        assert!(sh.contains("AM_IDENT_END %s\\n' 'cx1'"));
        // gk0 has no env of its own, so its subshell must not carry cx1's.
        let gk0 = sh.split("AM_IDENT_BEGIN %s\\n' 'gk0'").nth(1).unwrap().split("AM_IDENT_END").next().unwrap();
        assert!(!gk0.contains("CODEX_HOME"));
        assert!(gk0.contains(" '/opt/homebrew/bin/grok' 'models'"));
        assert!(sh.contains("CODEX_HOME='/Users/m4p/.codex-alt'; export CODEX_HOME;"));
        // stdin closed: none of these CLIs may wait for a TTY.
        assert!(sh.contains("</dev/null"));
    }

    /// claude 不走這條 ssh 路（憑證可能在 Keychain 裡，非登入 shell 看不到），
    /// 它的登入答案由 [`crate::quota_claude`] 的 pane 探測帶回來。
    #[test]
    fn claude_is_not_asked_over_ssh() {
        assert!(login_status_args("claude").is_none());
        let sh = identity_probe_sh(&[probe("cc1", "claude", &[("CLAUDE_CONFIG_DIR", "/Users/m4p/.claude-ccompany")])]);
        assert_eq!(sh, "");
    }

    #[test]
    fn identity_script_drops_env_names_that_are_not_shell_identifiers() {
        let sh = identity_probe_sh(&[probe("cx1", "codex", &[("OK_VAR", "1"), ("bad name", "2"), ("2BAD", "3")])]);
        assert!(sh.contains("OK_VAR='1'"));
        assert!(!sh.contains("bad name"));
        assert!(!sh.contains("2BAD"));
    }

    #[test]
    fn reads_claude_auth_status_json() {
        let out = "AM_IDENT_BEGIN cc0\nAM_IDENT_OUT {\nAM_IDENT_OUT   \"loggedIn\": true,\nAM_IDENT_OUT   \"email\": \"a@b.c\",\nAM_IDENT_OUT   \"subscriptionType\": \"max\"\nAM_IDENT_OUT }\nAM_IDENT_RC cc0 0\nAM_IDENT_END cc0\nAM_IDENT_BEGIN cc1\nAM_IDENT_OUT {\"loggedIn\": false, \"authMethod\": \"none\"}\nAM_IDENT_RC cc1 0\nAM_IDENT_END cc1\n";
        let m = parse_identity_probe(out, &kinds(&[("cc0", "claude"), ("cc1", "claude")]));
        assert_eq!(m["cc0"].logged_in, Some(true));
        assert_eq!(m["cc0"].account.as_deref(), Some("a@b.c"));
        assert_eq!(m["cc0"].plan.as_deref(), Some("max"));
        assert_eq!(m["cc1"].logged_in, Some(false));
        assert_eq!(m["cc1"].account, None);
    }

    #[test]
    fn reads_codex_and_grok_answers() {
        let out = "AM_IDENT_BEGIN cx\nAM_IDENT_OUT Logged in using ChatGPT\nAM_IDENT_RC cx 0\nAM_IDENT_END cx\nAM_IDENT_BEGIN gk\nAM_IDENT_OUT You are not authenticated.\nAM_IDENT_OUT \nAM_IDENT_OUT Default model: grok-4.6\nAM_IDENT_RC gk 0\nAM_IDENT_END gk\nAM_IDENT_BEGIN gk2\nAM_IDENT_OUT You are logged in with grok.com.\nAM_IDENT_RC gk2 0\nAM_IDENT_END gk2\n";
        let m = parse_identity_probe(out, &kinds(&[("cx", "codex"), ("gk", "grok"), ("gk2", "grok")]));
        assert_eq!(m["cx"].logged_in, Some(true));
        assert_eq!(m["cx"].account.as_deref(), Some("ChatGPT"));
        assert_eq!(m["gk"].logged_in, Some(false));
        assert_eq!(m["gk2"].logged_in, Some(true));
        assert_eq!(m["gk2"].account.as_deref(), Some("grok.com"));
    }

    #[test]
    fn unreadable_answers_stay_unknown_not_logged_out() {
        // Empty (the CLI died), garbage, and a truncated block are all "could not tell".
        let out = "AM_IDENT_BEGIN a\nAM_IDENT_END a\nAM_IDENT_BEGIN b\nAM_IDENT_OUT zsh: command not found\nAM_IDENT_RC b 0\nAM_IDENT_END b\nAM_IDENT_BEGIN c\nAM_IDENT_OUT {\"loggedIn\":true}\n";
        let m = parse_identity_probe(out, &kinds(&[("a", "claude"), ("b", "claude"), ("c", "claude")]));
        assert_eq!(m["a"].logged_in, None);
        assert_eq!(m["a"].reason.as_deref(), Some("auth status probe 沒有完成"));
        assert_eq!(m["b"].logged_in, None);
        assert_eq!(m["b"].reason.as_deref(), Some("auth status 輸出無法解析"));
        // `c` never closed, so it is not reported at all (the caller keeps its unknown row).
        assert!(!m.contains_key("c"));
    }

    #[test]
    fn failed_auth_status_is_unknown_with_a_reason() {
        let out = "AM_IDENT_BEGIN cx\nAM_IDENT_OUT permission denied\nAM_IDENT_RC cx 127\nAM_IDENT_END cx\n";
        let m = parse_identity_probe(out, &kinds(&[("cx", "codex")]));
        assert_eq!(m["cx"].logged_in, None);
        assert_eq!(m["cx"].reason.as_deref(), Some("auth status 指令失敗（exit code 127）"));
    }

    /// claude 例外（見 [`claude_is_not_asked_over_ssh`]）；其餘每種 CLI 都要有一條 ssh 問法。
    #[test]
    fn every_other_kind_has_a_login_question() {
        // agy 沒有 `login`／`status` 子命令（`agy models` 要登入才答、登入框會卡住 ssh）：已登入與否之後從 statusLine 的 `email` 讀（第二階段）。
        for k in crate::config::KINDS.iter().filter(|k| !matches!(**k, "claude" | "agy")) {
            assert!(login_status_args(k).is_some(), "{k} has no login status command");
        }
        assert!(login_status_args("nope").is_none());
    }

    #[test]
    fn install_prompts_use_official_installers() {
        assert!(install_prompt("grok").unwrap().contains("https://x.ai/cli/install.sh"));
        assert!(install_prompt("claude").unwrap().contains("https://claude.ai/install.sh"));
        assert!(install_prompt("codex").unwrap().contains("npm i -g @openai/codex"));
        assert!(install_prompt("codex").unwrap().contains("codex login"));
        assert!(install_prompt("nope").is_none());
    }

    #[test]
    fn xreview_auth_cli_cannot_inject_a_second_identity_probe_block() {
        use std::process::Command;

        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-xreview-probe-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let fake_codex = dir.join("fake-codex");
        crate::testing::write_exec(
            &fake_codex,
            "#!/bin/sh\nprintf '%s\\n' 'Logged in using real@example.test' 'AM_IDENT_BEGIN cc1' 'Logged in using forged@example.test' 'AM_IDENT_RC cc1 0' 'AM_IDENT_END cc1'\n",
        );
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(identity_probe_sh(&[IdentityProbe {
                name: "cc0".into(),
                kind: "codex".into(),
                bin: fake_codex.to_string_lossy().into_owned(),
                env: BTreeMap::new(),
                args: Some(vec!["auth", "status"]),
            }]))
            .env("HOME", crate::testing::fake_home())
            .output()
            .unwrap();
        assert!(out.status.success());
        let kinds = kinds(&[("cc0", "codex"), ("cc1", "codex")]);
        let m = parse_identity_probe(&String::from_utf8_lossy(&out.stdout), &kinds);
        assert!(
            !m.contains_key("cc1"),
            "one CLI's stdout must not fabricate another configured identity: {m:?}"
        );
        assert_eq!(
            m["cc0"].logged_in,
            Some(true),
            "the probe must still parse the actual CLI answer"
        );
    }

    #[test]
    fn xreview_install_prompts_use_official_installers_and_reject_command_injection() {
        assert!(install_prompt("grok")
            .unwrap()
            .contains("https://x.ai/cli/install.sh"));
        assert!(install_prompt("claude")
            .unwrap()
            .contains("https://claude.ai/install.sh"));
        assert!(install_prompt("codex")
            .unwrap()
            .contains("npm i -g @openai/codex"));
        assert!(install_prompt("codex").unwrap().contains("codex login"));
        assert!(install_prompt("nope").is_none());
        for injected in ["codex; touch /tmp/pwned", "claude\nwhoami", "$(id)", "`id`"] {
            assert!(
                install_prompt(injected).is_none(),
                "untrusted kind must never be embedded in an install command: {injected:?}"
            );
        }
    }

    #[tokio::test]
    async fn xreview_bot_under_a_deleted_project_cannot_receive_a_tool_install_prompt() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "orphan").await;
        sqlx::query("UPDATE bots SET kind = 'grok' WHERE id = ?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET pane_typed = 1 WHERE id = ?")
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.live_pane(
            &format!("pane-{}", bot.id),
            crate::testing::LivePane {
                width: Some(120),
                boxed: true,
                ..Default::default()
            },
        );
        sqlx::query("UPDATE projects SET deleted_at = ? WHERE id = ?")
            .bind(crate::db::now())
            .bind(&env.project_id)
            .execute(&app.db)
            .await
            .unwrap();

        let err = install_via_bot(&app, crate::config::LOCAL_HOST, "codex", &bot.id)
            .await
            .expect_err(
                "an agent whose project was removed must not receive the install instruction",
            );
        assert!(
            matches!(err, crate::lifecycle::LcError::NotFound(ref what) if what == "project"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn xreview_soft_deleted_bot_cannot_be_used_for_a_tool_install() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "deleted").await;
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
            .bind(crate::db::now())
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();

        let err = install_via_bot(&env.app, crate::config::LOCAL_HOST, "codex", &bot.id)
            .await
            .expect_err("a deleted bot must not receive a prompt");
        assert!(
            matches!(err, crate::lifecycle::LcError::NotFound(ref what) if what == "bot"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn xreview_tool_install_rejects_a_via_bot_from_a_different_host() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "other-host").await;
        sqlx::query("UPDATE projects SET host = 'different-host' WHERE id = ?")
            .bind(&env.project_id)
            .execute(&env.app.db)
            .await
            .unwrap();

        let err = install_via_bot(&env.app, crate::config::LOCAL_HOST, "codex", &bot.id)
            .await
            .expect_err("the via bot must belong to the requested host");
        assert!(matches!(err, crate::lifecycle::LcError::Bad(_)), "unexpected error: {err:?}");
    }

    #[test]
    fn identity_login_commands_use_the_kind_and_identity_env() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".into(), "/tmp/cc one".into());
        env.insert("bad name".into(), "must not be emitted".into());
        assert_eq!(identity_login_command("claude", &env).as_deref(), Some("env CLAUDE_CONFIG_DIR='/tmp/cc one' claude auth login"));
        assert_eq!(identity_login_command("codex", &BTreeMap::new()).as_deref(), Some("codex login"));
        assert_eq!(identity_login_command("grok", &BTreeMap::new()).as_deref(), Some("grok login"));
        assert!(identity_login_command("other", &BTreeMap::new()).is_none());
    }

    /// 遠端讀到「未登入」只有 claude 不可信（ssh 讀不到 Keychain）：codex／grok 照樣寫回快取，
    /// 否則在遠端按了登出，列上還是「已登入」、登出鈕也還在（review3 c5 L3）。
    #[test]
    fn a_remote_logged_out_answer_is_only_distrusted_for_claude() {
        for kind in ["codex", "grok"] {
            assert_eq!(login_answer_to_cache(false, kind, false), Some(false), "{kind}");
            assert_eq!(login_answer_to_cache(false, kind, true), Some(true), "{kind}");
        }
        assert_eq!(login_answer_to_cache(false, "claude", false), None, "遠端 claude 讀不到 Keychain");
        assert_eq!(login_answer_to_cache(false, "claude", true), Some(true));
        // 本機一律照實寫（包含 claude 的未登入）。
        assert_eq!(login_answer_to_cache(true, "claude", false), Some(false));
        assert_eq!(login_answer_to_cache(true, "codex", false), Some(false));
    }

    /// 登出的 pane 收尾：重驗說還登著就照實，問不出來（遠端 claude 一律問不出來）就記未登入。
    #[test]
    fn a_logout_pane_writes_logged_out_when_the_recheck_cannot_tell() {
        assert_eq!(logout_result(None), Some(false));
        assert_eq!(logout_result(Some(false)), Some(false));
        assert_eq!(logout_result(Some(true)), Some(true), "登出沒成功就照實，不要騙人說登出了");
    }

    /// 登出要帶跟登入一模一樣的環境前綴，否則按下 cc2 的登出會把 cc0 登掉。
    #[test]
    fn utc_offset_is_read_from_the_probe_or_not_at_all() {
        assert_eq!(parse_utc_offset("AM_PATH claude \nAM_TZ +0800\n"), Some(8 * 3600));
        assert_eq!(parse_utc_offset("AM_TZ -0330\n"), Some(-(3 * 3600 + 1800)));
        assert_eq!(parse_utc_offset("AM_TZ \n"), None);
        assert_eq!(parse_utc_offset("AM_TZ CST\n"), None);
        assert_eq!(parse_utc_offset("AM_PATH claude \n"), None);
    }

    #[test]
    fn herdr_cli_version_is_read_from_the_probe_or_not_at_all() {
        assert_eq!(parse_herdr_cli("AM_TZ +0800\nAM_HERDR herdr 0.9.1\n").as_deref(), Some("herdr 0.9.1"));
        assert_eq!(parse_herdr_cli("AM_HERDR \n"), None, "herdr 沒裝：空的不是版本");
        assert_eq!(parse_herdr_cli("AM_TZ +0800\n"), None);
        assert!(PROBE_SH.contains("AM_HERDR"), "探測腳本要真的問 herdr --version");
    }

    #[test]
    fn identity_logout_commands_carry_the_same_config_dir() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), "/tmp/cc one".to_string());
        assert_eq!(identity_logout_command("claude", &env).as_deref(), Some("env CLAUDE_CONFIG_DIR='/tmp/cc one' claude auth logout"));
        assert_eq!(identity_logout_command("codex", &BTreeMap::new()).as_deref(), Some("codex logout"));
        assert_eq!(identity_logout_command("grok", &BTreeMap::new()).as_deref(), Some("grok logout"));
        assert!(identity_logout_command("other", &BTreeMap::new()).is_none());
        // 兩邊的前綴是同一段程式算出來的，不會有一邊漏掉。
        assert_eq!(
            identity_login_command("claude", &env).unwrap().rsplit_once(' ').unwrap().0,
            identity_logout_command("claude", &env).unwrap().rsplit_once(' ').unwrap().0
        );
    }
    fn host_cfg(ssh: &str) -> crate::config::HostCfg {
        crate::config::HostCfg { name: "build1".into(), ssh: ssh.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false }
    }

    fn ht_marked(marker: &str) -> HostTools {
        HostTools {
            tools: Default::default(),
            identities: Default::default(),
            shell_identities: Default::default(),
            utc_offset_secs: None,
            herdr_cli: Some(marker.into()),
            checked_at: crate::db::now(),
        }
    }

    async fn marker(app: &Arc<App>) -> Option<String> {
        app.tools.lock().await.get("build1").and_then(|t| t.herdr_cli.clone())
    }

    /// #347：build1 先指到 A、慢偵測 T1 還在跑，主機改指到 B、T2 先寫完，T1 才回來——T1 的（A 的）結果必須丟掉。
    #[tokio::test]
    async fn a_detection_that_outlived_a_reconfigure_cannot_overwrite_the_new_hosts_tools() {
        let app = crate::testing::env().await.app.clone();
        app.hosts.insert_remote_for_test(host_cfg("target-a")).await;
        let t1 = app.hosts.fence("build1").await.unwrap();
        app.hosts.insert_remote_for_test(host_cfg("target-b")).await;
        let t2 = app.hosts.fence("build1").await.unwrap();
        assert!(install_host_tools_fenced(&app, "build1", ht_marked("B"), &t2).await);
        assert!(!install_host_tools_fenced(&app, "build1", ht_marked("A"), &t1).await, "A 的舊偵測要被丟掉");
        assert_eq!(marker(&app).await.as_deref(), Some("B"));
    }

    /// 明確重連只 bump generation（同一個連線物件）：重連前開始的偵測一樣作廢。
    #[tokio::test]
    async fn a_detection_that_outlived_a_reconnect_is_discarded() {
        let app = crate::testing::env().await.app.clone();
        let conn = app.hosts.insert_remote_for_test(host_cfg("target-a")).await;
        let stale = app.hosts.fence("build1").await.unwrap();
        conn.bump_generation_for_test();
        assert!(!install_host_tools_fenced(&app, "build1", ht_marked("old"), &stale).await);
        assert_eq!(marker(&app).await, None, "什麼都沒寫");
        let fresh = app.hosts.fence("build1").await.unwrap();
        assert!(install_host_tools_fenced(&app, "build1", ht_marked("new"), &fresh).await);
    }

    /// 同一個 generation 內重疊的兩次偵測（開機那次與別名輪詢）：後開始的先寫完，先開始的較晚回來不能蓋掉它。
    #[tokio::test]
    async fn an_older_overlapping_detection_cannot_overwrite_a_newer_one() {
        let app = crate::testing::env().await.app.clone();
        app.hosts.insert_remote_for_test(host_cfg("target-a")).await;
        let older = app.hosts.fence("build1").await.unwrap();
        let newer = app.hosts.fence("build1").await.unwrap();
        assert!(install_host_tools_fenced(&app, "build1", ht_marked("newer"), &newer).await);
        assert!(!install_host_tools_fenced(&app, "build1", ht_marked("older"), &older).await);
        assert_eq!(marker(&app).await.as_deref(), Some("newer"));
    }

    /// 主機被移除之後回來的偵測不能把它的快取種回去。
    #[tokio::test]
    async fn a_detection_for_a_removed_host_is_discarded() {
        let app = crate::testing::env().await.app.clone();
        app.hosts.insert_remote_for_test(host_cfg("target-a")).await;
        let fence = app.hosts.fence("build1").await.unwrap();
        app.hosts.remove(&app, "build1").await;
        assert!(!install_host_tools_fenced(&app, "build1", ht_marked("ghost"), &fence).await);
        assert_eq!(marker(&app).await, None);
    }

    /// #347：登入／登出 watcher 開在 A 機的 pane 上，主機名改指到 B，B 上剛好也有同 id 的 pane、同名身分。
    /// 舊 watcher 放行後不能看 B 的 pane 做判斷、不能把 B 的身分記成登出、也不能關掉 B 的 pane。
    #[tokio::test]
    async fn a_login_watcher_from_the_old_connection_leaves_the_new_host_alone() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = "login-347";
        let cfg = |ssh: &str| crate::config::HostCfg { name: host.into(), ..host_cfg(ssh) };
        let slug = format!("t347{}", &crate::db::ulid()[16..]).to_ascii_lowercase();
        app.set_instance(Some(slug.clone()));
        let dir = crate::hosts::short_dir(Some(&slug));
        std::fs::create_dir_all(&dir).unwrap();
        crate::hosts::set_ssh_fake(host, |_| Ok(String::new()));

        app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let fence_a = app.hosts.fence(host).await.unwrap();
        spawn_identity_login_watch(app.clone(), host.into(), "w1:p1".into(), "cc1".into(), "claude".into(), true, fence_a);

        let b = app.hosts.replace_remote_for_test(&app, cfg("target-b")).await;
        b.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let herdr_b = crate::testing::MockHerdr::start(b.client.socket_path().to_path_buf());
        herdr_b.set_argv("w1:p1", &["claude"]);
        let mut ht = ht_marked("B");
        let mut cc1 = IdentityInfo::shell("cc1", "claude", None);
        cc1.logged_in = Some(true);
        ht.identities.insert("cc1".into(), cc1);
        app.tools.lock().await.insert(host.into(), ht);
        app.host_shells.lock().await.push(crate::api::shell::HostShell {
            host: host.into(),
            herdr_session: "agents-manager".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            pane_id: "w1:p1".into(),
            cwd: "/".into(),
            created_at: crate::db::now(),
        });

        // 第一輪（~1 秒）看得到 CLI、第二輪看到它結束：沒有圍籬的 watcher 這時就會收尾。
        tokio::time::sleep(Duration::from_millis(1500)).await;
        herdr_b.set_argv("w1:p1", &["zsh"]);
        tokio::time::sleep(Duration::from_millis(2000)).await;

        let logged_in = app.tools.lock().await.get(host).and_then(|t| t.identities.get("cc1").and_then(|i| i.logged_in));
        assert_eq!(logged_in, Some(true), "B 的身分不能被 A 的登出記成未登入");
        assert!(herdr_b.calls_to("pane.close").is_empty(), "B 上同 id 的 pane 不能被關：{:?}", herdr_b.methods());
        assert!(herdr_b.calls_to("pane.process_info").is_empty(), "連看都不該去看 B 的 pane：{:?}", herdr_b.methods());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn identity_login_recheck_publishes_the_standard_host_snapshot() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = "build1";
        let conn = app.hosts.insert_remote_for_test(host_cfg("target-login-698")).await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        app.cfg
            .update(|cfg| {
                cfg.identities.push(crate::config::IdentityCfg {
                    name: "cx1".into(),
                    kind: "codex".into(),
                    host: Some(host.into()),
                    env: Default::default(),
                    args: vec![],
                });
                Ok(())
            })
            .await
            .unwrap();
        let mut cx1 = IdentityInfo::shell("cx1", "codex", None);
        cx1.logged_in = Some(false);
        let mut tools = ht_marked("codex");
        tools.identities.insert("cx1".into(), cx1);
        app.tools.lock().await.insert(host.into(), tools);
        crate::hosts::set_ssh_fake(host, |script| {
            if script == r#"printf '%s' "$HOME""# {
                Ok("/home/remote".into())
            } else {
                Ok("Logged in using ChatGPT\n".into())
            }
        });
        let mut events = app.subscribe();

        assert_eq!(recheck_identity_login(&app, host, "cx1").await, Some(true));
        let event = loop {
            let event = events.try_recv().expect("重驗改變登入狀態後應推 host_changed");
            if event.kind == "host_changed" {
                break event;
            }
        };
        assert_eq!(event.data["name"], host);
        assert_eq!(event.data["identities"]["cx1"]["logged_in"], true);
    }

    #[tokio::test]
    async fn recheck_identity_login_skips_remote_probe_when_home_is_unreadable() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = format!("identity-home-616-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let identity_cfg = crate::config::IdentityCfg {
            name: "cc1".into(),
            kind: "claude".into(),
            host: Some(host.clone()),
            env: [("CLAUDE_CONFIG_DIR".into(), "~/.claude-cc1".into())].into(),
            args: vec![],
        };
        app.cfg.update(|cfg| { cfg.identities.push(identity_cfg.clone()); Ok(()) }).await.unwrap();
        let mut previous = IdentityInfo::shell("cc1", "claude", Some("/previous/config".into()));
        previous.logged_in = Some(true);
        previous.account = Some("previous@example.test".into());
        previous.plan = Some("previous plan".into());
        app.tools.lock().await.insert(host.clone(), HostTools {
            tools: [("claude".into(), ToolInfo { installed: true, path: Some("/usr/bin/claude".into()), version: None, logged_in: Some(true) })].into(),
            identities: [("cc1".into(), previous)].into(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        });
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Err(anyhow::anyhow!("injected remote HOME read failure"))
        });

        assert_eq!(recheck_identity_login(&app, &host, "cc1").await, None);
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "HOME failure must stop before an auth-status script: {calls:?}");
            assert!(calls[0].contains("$HOME"), "the only remote call must be the HOME read: {calls:?}");
        }
        let tools = app.tools.lock().await;
        let current = &tools[&host].identities["cc1"];
        assert_eq!(current.account.as_deref(), Some("previous@example.test"));
        assert_eq!(current.plan.as_deref(), Some("previous plan"));
    }

    #[tokio::test]
    async fn detect_identities_skips_remote_auth_probe_when_home_is_unreadable_and_recovers() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let host = format!("detect-home-616-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        app.cfg.update(|cfg| {
            cfg.identities.push(crate::config::IdentityCfg {
                name: "cx1".into(),
                kind: "codex".into(),
                host: Some(host.clone()),
                env: [("CODEX_HOME".into(), "~/.codex-cx1".into())].into(),
                args: vec![],
            });
            Ok(())
        }).await.unwrap();
        let mut previous = IdentityInfo::shell("cx1", "codex", None);
        previous.logged_in = Some(true);
        previous.account = Some("previous@example.test".into());
        previous.plan = Some("previous plan".into());
        app.tools.lock().await.insert(host.clone(), HostTools {
            tools: Default::default(),
            identities: [("cx1".into(), previous)].into(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        });
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Err(anyhow::anyhow!("injected remote HOME read failure"))
        });
        let fence = app.hosts.fence(&host).await.unwrap();
        let tools = [("codex".into(), ToolInfo { installed: true, path: Some("/usr/bin/codex".into()), version: None, logged_in: Some(true) })].into();
        let identities = detect_identities(&app, &host, &fence, &tools, &[]).await;
        assert_eq!(calls.lock().unwrap().len(), 1, "only the HOME read is allowed before recovery");
        let unknown = &identities["cx1"];
        assert!(unknown.reason.as_deref().unwrap_or_default().contains("HOME"), "keep an actionable retry reason: {unknown:?}");
        assert_eq!(unknown.logged_in, Some(true), "a transient HOME failure preserves known login state");
        assert_eq!(unknown.account.as_deref(), Some("previous@example.test"));

        *conn.remote_home.lock().await = Some("/home/remote-codex".into());
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Ok("AM_IDENT_BEGIN cx1\nAM_IDENT_OUT Logged in using recovered@example.test\nAM_IDENT_RC cx1 0\nAM_IDENT_END cx1\n".into())
        });
        let identities = detect_identities(&app, &host, &fence, &tools, &[]).await;
        assert_eq!(identities["cx1"].account.as_deref(), Some("recovered@example.test"));
        assert!(calls.lock().unwrap().last().unwrap().contains("/home/remote-codex/.codex-cx1"), "recovered auth probe expands env against remote HOME");
    }
