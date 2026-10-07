
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn claude_store_follows_the_identity_config_dir() {
        assert_eq!(
            store_path("claude", &env(&[]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.claude.json")
        );
        assert_eq!(
            store_path("claude", &env(&[("CLAUDE_CONFIG_DIR", "$HOME/.claude-ccompany")]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.claude-ccompany/.claude.json")
        );
        assert_eq!(
            store_path("claude", &env(&[("CLAUDE_CONFIG_DIR", "   ")]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.claude.json")
        );
    }

    #[test]
    fn codex_store_follows_codex_home_and_other_kinds_have_no_gate() {
        assert_eq!(
            store_path("codex", &env(&[]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.codex/config.toml")
        );
        assert_eq!(
            store_path("codex", &env(&[("CODEX_HOME", "~/alt")]), "/home/u").unwrap(),
            PathBuf::from("/home/u/alt/config.toml")
        );
        assert!(store_path("shell", &env(&[]), "/home/u").is_none());
    }

    /// agy 的設定目錄只認 `$HOME`：身分／bot env 的任何變數都改不了它。
    #[test]
    fn agy_store_is_the_settings_json_under_home_and_ignores_env() {
        let want = PathBuf::from("/home/u/.gemini/antigravity-cli/settings.json");
        assert_eq!(store_path("agy", &env(&[]), "/home/u").unwrap(), want);
        assert_eq!(store_path("agy", &env(&[("GROK_HOME", "/x"), ("XDG_CONFIG_HOME", "/y")]), "/home/u").unwrap(), want);
    }

    #[test]
    fn marking_a_workspace_trusted_for_agy_merges_into_the_users_settings_and_is_idempotent() {
        let dir = crate::testing::scratch_dir("am-trust-agy");
        let store = dir.join(".gemini/antigravity-cli/settings.json");
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, r#"{"colorScheme":"light","statusLine":{"type":"command","command":"/x"}}"#).unwrap();
        assert!(mark_trusted("agy", &store, &["/w/a".into()]).unwrap());
        assert!(!mark_trusted("agy", &store, &["/w/a".into()]).unwrap(), "已信任：不重寫");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["trustedWorkspaces"], json!(["/w/a"]));
        assert_eq!(v["colorScheme"], "light");
        assert_eq!(v["statusLine"]["command"], "/x", "statusLine 是另一條路的事，信任寫入不碰");
        // 壞掉的檔不覆寫。
        std::fs::write(&store, "{ nope").unwrap();
        assert!(mark_trusted("agy", &store, &["/w/b".into()]).is_err());
        assert_eq!(std::fs::read_to_string(&store).unwrap(), "{ nope");
    }

    #[test]
    fn grok_merge_writes_what_grok_writes_and_leaves_a_trusted_folder_alone() {
        let existing = "[folders.\"/Users/m4p/project/agents-manager\"]\ntrusted = true\ndecided_at = 1789389148\n";
        let out = grok_merge(existing, &["/tmp/rt".into()], 42).unwrap().expect("new folder is written");
        let doc: toml_edit::DocumentMut = out.parse().unwrap();
        assert_eq!(doc["folders"]["/tmp/rt"]["trusted"].as_bool(), Some(true));
        assert_eq!(doc["folders"]["/tmp/rt"]["decided_at"].as_integer(), Some(42));
        assert_eq!(doc["folders"]["/Users/m4p/project/agents-manager"]["decided_at"].as_integer(), Some(1789389148));
        assert!(!out.contains("[folders]\n"), "no bare [folders] header: {out}");
        assert!(grok_merge(&out, &["/tmp/rt".into()], 99).unwrap().is_none(), "already trusted");
        assert!(grok_merge("folders = 3", &["/tmp/rt".into()], 1).is_err());
    }

    #[test]
    fn grok_store_follows_grok_home() {
        let mut env = BTreeMap::new();
        assert_eq!(store_path("grok", &env, "/h"), Some(PathBuf::from("/h/.grok/trusted_folders.toml")));
        env.insert("GROK_HOME".into(), "/x/g2".into());
        assert_eq!(store_path("grok", &env, "/h"), Some(PathBuf::from("/x/g2/trusted_folders.toml")));
    }

    #[test]
    fn claude_merge_keeps_every_other_field() {
        let before = r#"{
  "numStartups": 447,
  "oauthAccount": {"accountUuid": "abc", "emailAddress": "u@example.com"},
  "tipsHistory": {"new-user-warmup": 8},
  "projects": {
    "/home/u": {"hasTrustDialogAccepted": true, "lastCost": 1.25, "mcpServers": {}},
    "/home/u/other": {"allowedTools": ["Bash"]}
  },
  "autoUpdates": false
}"#;
        let after = claude_merge(before, &["/data/proj/main".into()]).unwrap().unwrap();
        let a: Value = serde_json::from_str(&after).unwrap();
        let b: Value = serde_json::from_str(before).unwrap();

        for (k, v) in b.as_object().unwrap() {
            if k == "projects" {
                continue;
            }
            assert_eq!(a.get(k), Some(v), "top-level `{k}` was lost or changed");
        }
        let bp = b["projects"].as_object().unwrap();
        for (k, v) in bp {
            assert_eq!(&a["projects"][k], v, "project `{k}` was lost or changed");
        }
        assert_eq!(a["projects"]["/home/u"]["lastCost"], json!(1.25));
        assert_eq!(a["projects"]["/home/u/other"]["allowedTools"], json!(["Bash"]));
        assert_eq!(a["projects"]["/data/proj/main"][CLAUDE_KEY], json!(true));
        assert_eq!(a["projects"].as_object().unwrap().len(), 3);
        assert!(after.ends_with("}\n"));
    }

    #[test]
    fn claude_merge_adds_projects_and_leaves_a_trusted_path_alone() {
        let out = claude_merge(r#"{"numStartups": 1}"#, &["/w".into()]).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["numStartups"], json!(1));
        assert_eq!(v["projects"]["/w"][CLAUDE_KEY], json!(true));

        let out = claude_merge("", &["/w".into()]).unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["projects"]["/w"][CLAUDE_KEY], json!(true));

        assert!(claude_merge(&out, &["/w".into()]).unwrap().is_none());
        for k in CLAUDE_EXTERNAL_KEYS {
            assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["projects"]["/w"][k], json!(true));
        }

        let out = claude_merge(r#"{"projects":{"/w":{"lastCost":3}}}"#, &["/w".into()]).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["projects"]["/w"]["lastCost"], json!(3));
        assert_eq!(v["projects"]["/w"][CLAUDE_KEY], json!(true));
    }

    #[test]
    fn claude_merge_refuses_a_file_it_does_not_understand() {
        assert!(claude_merge("not json", &["/w".into()]).is_err());
        assert!(claude_merge("[1,2]", &["/w".into()]).is_err());
        assert!(claude_merge(r#"{"projects": 7}"#, &["/w".into()]).is_err());
        assert!(claude_merge(r#"{"projects": {"/w": 7}}"#, &["/w".into()]).is_err());
    }

    #[test]
    fn codex_merge_keeps_comments_and_other_tables() {
        let before = r#"# my codex config
model = "gpt-5"

[tui]
theme = "dark"

[projects."/home/u/project"]
trust_level = "trusted"
"#;
        let out = codex_merge(before, &["/data/proj/dev-1".into()]).unwrap().unwrap();
        assert!(out.starts_with("# my codex config\n"), "comment lost:\n{out}");
        assert!(out.contains("[tui]\ntheme = \"dark\""), "table lost:\n{out}");
        assert!(out.contains("[projects.\"/home/u/project\"]"), "existing project lost:\n{out}");
        assert!(out.contains("[projects.\"/data/proj/dev-1\"]"), "new project missing:\n{out}");
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["model"].as_str(), Some("gpt-5"));
        assert_eq!(v["projects"]["/home/u/project"]["trust_level"].as_str(), Some("trusted"));
        assert_eq!(v["projects"]["/data/proj/dev-1"]["trust_level"].as_str(), Some("trusted"));

        assert!(codex_merge(&out, &["/data/proj/dev-1".into()]).unwrap().is_none());
        let fresh = codex_merge("", &["/w".into()]).unwrap().unwrap();
        assert!(fresh.contains("[projects.\"/w\"]"), "{fresh}");
        assert!(!fresh.contains("\n[projects]\n"), "bare [projects] header:\n{fresh}");
        assert!(codex_merge("nope = ", &["/w".into()]).is_err());
    }

    #[test]
    fn canonical_resolves_symlinks_and_survives_a_missing_directory() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-trust-canon-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let link = dir.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("real"), &link).unwrap();

        let got = canonical(&link.to_string_lossy());
        let want = std::fs::canonicalize(dir.join("real")).unwrap();
        assert_eq!(got, want.to_string_lossy());
        assert!(!got.contains("/link"), "symlink not resolved: {got}");

        assert_eq!(canonical("/no/such/dir/anywhere"), "/no/such/dir/anywhere");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 批次重啟時好幾顆 bot（不同 worktree、同一個 `~/.claude.json`）同時預先信任：
    /// 讀→合併→寫沒有互斥的話，後寫的蓋掉先寫的（那顆 bot 就跳出信任提示），暫存檔名又只有 pid，並行時還會撞檔。
    #[test]
    fn concurrent_pretrusts_of_one_store_lose_nothing() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-trust-race-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join(".claude.json");
        std::fs::write(&store, r#"{"numStartups": 9}"#).unwrap();
        for round in 0..20 {
            let n = 12;
            let barrier = Arc::new(std::sync::Barrier::new(n));
            let handles: Vec<_> = (0..n)
                .map(|i| {
                    let (store, barrier) = (store.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        mark_trusted("claude", &store, &[format!("/w/{round}/{i}")])
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap().expect("a concurrent pre-trust must not fail");
            }
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).expect("store stays valid JSON");
            for i in 0..n {
                assert_eq!(v["projects"][format!("/w/{round}/{i}")][CLAUDE_KEY], json!(true), "round {round}: /w/{round}/{i} was lost");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_trusted_is_atomic_and_idempotent_on_disk() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-trust-store-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join(".claude.json");
        std::fs::write(&store, r#"{"numStartups": 9, "projects": {"/keep": {"lastCost": 2}}}"#).unwrap();

        assert!(mark_trusted("claude", &store, &["/w1".into(), "/w2".into()]).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["numStartups"], json!(9));
        assert_eq!(v["projects"]["/keep"]["lastCost"], json!(2));
        assert_eq!(v["projects"]["/w1"][CLAUDE_KEY], json!(true));
        assert_eq!(v["projects"]["/w2"][CLAUDE_KEY], json!(true));

        let bytes = std::fs::read(&store).unwrap();
        assert!(!mark_trusted("claude", &store, &["/w1".into(), "/w2".into()]).unwrap());
        assert_eq!(std::fs::read(&store).unwrap(), bytes);
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left: {strays:?}");

        let none = dir.join("shell-nothing");
        assert!(!mark_trusted("shell", &none, &["/w1".into()]).unwrap());
        assert!(!none.exists());

        let cx = dir.join("sub").join("config.toml");
        assert!(mark_trusted("codex", &cx, &["/w1".into()]).unwrap());
        let v: toml::Value = toml::from_str(&std::fs::read_to_string(&cx).unwrap()).unwrap();
        assert_eq!(v["projects"]["/w1"]["trust_level"].as_str(), Some("trusted"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_trusted_keeps_the_original_file_mode() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = crate::testing::track(std::env::temp_dir().join(format!("am-trust-mode-{}", std::process::id())));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store = dir.join(".claude.json");
            std::fs::write(&store, "{}").unwrap();
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();

            assert!(mark_trusted("claude", &store, &["/w".into()]).unwrap());
            let mode = std::fs::metadata(&store).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode changed to {mode:o}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
