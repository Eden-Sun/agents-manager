
    use super::*;
    use crate::runners::models::list;

    /// 一台設定了、但連不上的遠端主機（沒有人在聽那個 port，connection refused，快速失敗）。
    async fn unreachable_host(e: &crate::testing::Env) -> &'static str {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let cfg = crate::config::HostCfg {
            shared_session: false,
            name: "unreachable-box".into(),
            ssh: "127.0.0.1".into(),
            ssh_port: port,
            ssh_opts: vec!["-o".into(), "ConnectTimeout=2".into()],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        e.app.hosts.apply_config(&e.app, &[cfg]).await;
        "unreachable-box"
    }

    #[tokio::test]
    async fn a_remote_model_identity_does_not_probe_without_home_and_recovers() {
        let e = crate::testing::env().await;
        let host = format!("model-home-616-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = e.app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        e.app.cfg.update(|cfg| {
            cfg.identities.push(crate::config::IdentityCfg {
                name: "cc1".into(),
                kind: "claude".into(),
                host: Some(host.clone()),
                env: [("CLAUDE_CONFIG_DIR".into(), "~/.claude-cc1".into())].into(),
                args: vec![],
            });
            Ok(())
        }).await.unwrap();
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Err(anyhow!("injected remote HOME read failure"))
        });

        let err = claude_default_effort(&e.app, &host, Some("cc1"), "opus").await.expect_err("do not read an identity's model settings without its HOME");
        assert!(err.to_string().contains("HOME"), "preserve the HOME failure reason: {err:#}");
        assert_eq!(calls.lock().unwrap().len(), 1, "settings-file probe must not follow failed HOME resolution");

        *conn.remote_home.lock().await = Some("/home/remote-model".into());
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Ok(r#"{"effortLevel":"medium"}"#.into())
        });
        assert_eq!(claude_default_effort(&e.app, &host, Some("cc1"), "opus").await.unwrap(), "medium");
        assert!(calls.lock().unwrap().last().unwrap().contains("'/home/remote-model/.claude-cc1'/settings.json"), "recovery uses the remote identity path: {:?}", calls.lock().unwrap());
    }

    /// #268：遠端 settings.json 讀不到（ssh 失敗）不是「沒設定」——以前讀成 `""`，預設 effort 變成內建的 `high`，
    /// 對帳把它記進子 bot，之後沒有人會再讀一次。
    #[tokio::test]
    async fn an_unreadable_remote_settings_file_is_an_error_not_the_builtin_default() {
        let e = crate::testing::env().await;
        let host = unreachable_host(&e).await;
        let r = tokio::time::timeout(Duration::from_secs(20), claude_default_effort(&e.app, host, None, "opus"))
            .await
            .expect("連不上要快速失敗");
        assert!(r.is_err(), "讀不到設定檔不能回內建預設：{r:?}");
        let listed = tokio::time::timeout(Duration::from_secs(20), list(&e.app, host, "claude", None, true)).await.unwrap();
        assert!(listed.is_err(), "模型清單不能拿內建預設充數、還快取十分鐘");
        assert!(e.app.models_cache.lock().await.is_empty(), "失敗的結果不能進快取");
        e.app.hosts.remove(&e.app, host).await;
    }

    /// #347：A 機的模型探測還在路上時同名主機改指到 B、B 的清單先進了快取；A 晚到的結果不能蓋掉它，也不能回給呼叫端當答案。
    #[tokio::test]
    async fn a_model_list_from_a_superseded_host_does_not_reach_the_cache() {
        let e = crate::testing::env().await;
        let host = "models-347";
        let cfg = |ssh: &str| crate::config::HostCfg {
            shared_session: false,
            name: host.into(),
            ssh: ssh.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        let answer = Arc::new(std::sync::Mutex::new(r#"{"effortLevel":"low"}"#.to_string()));
        let a2 = answer.clone();
        crate::hosts::set_ssh_fake(host, move |_| Ok(a2.lock().unwrap().clone()));
        crate::hosts::set_ssh_delay(host, Duration::from_millis(400));
        e.app.hosts.insert_remote_for_test(cfg("target-a")).await;
        let app = e.app.clone();
        let stale = tokio::spawn(async move { list(&app, host, "claude", None, true).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        e.app.hosts.replace_remote_for_test(&e.app, cfg("target-b")).await;
        *answer.lock().unwrap() = r#"{"effortLevel":"max"}"#.into();
        crate::hosts::set_ssh_delay(host, Duration::ZERO);
        let fresh = list(&e.app, host, "claude", None, true).await.expect("B 自己的清單");
        assert_eq!(fresh["models"][0]["default_effort"], "max");
        *answer.lock().unwrap() = r#"{"effortLevel":"low"}"#.into();

        let r = stale.await.unwrap();
        assert!(r.is_err(), "A 的結果作廢，不能當答案回：{r:?}");
        let cached = e.app.models_cache.lock().await.get(&format!("{host}/claude/")).map(|(_, v)| v.clone()).expect("B 的還在");
        assert_eq!(cached["models"][0]["default_effort"], "max", "快取要留著 B 的清單");
    }

    /// 缺檔是答案不是錯誤：本機回 `""`，遠端腳本要 exit 0（`ssh_exec` 看 exit code）。
    #[tokio::test]
    async fn a_missing_settings_file_is_an_empty_answer_not_a_failure() {
        let e = crate::testing::env().await;
        let local = read_optional_text(&e.app, LOCAL_HOST, "/nonexistent-am-dir/settings.json").await.unwrap();
        assert_eq!(local, "");
        let st = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(optional_cat_script("/nonexistent-am-dir/settings.json"))
            .status()
            .await
            .unwrap();
        assert!(st.success(), "缺檔時腳本要 exit 0，不然遠端缺檔會被 ssh_exec 當成失敗");
    }

    /// The argv shapes `herdr pane process-info` actually reported on this machine,
    /// 2026-09-07 (a claude child pane and a grok one).
    #[test]
    fn model_and_effort_are_read_off_a_running_cli() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--dangerously-skip-permissions", "--model", "opus"])),
            (Some("claude-opus-5-5".into()), None)
        );
        assert_eq!(
            model_effort_from_argv("grok", &argv(&["grok", "--always-approve", "-m", "grok-4.6", "--reasoning-effort", "high"])),
            (Some("grok-4.6".into()), Some("high".into()))
        );
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--model=sonnet", "--effort=xhigh"])),
            (Some("sonnet".into()), Some("xhigh".into()))
        );
        assert_eq!(
            model_effort_from_argv(
                "codex",
                &argv(&["codex", "-m", "gpt-5.6-sol", "-c", "model_reasoning_effort=\"max\"", "-c", "service_tier=\"priority\""])
            ),
            (Some("gpt-6-sol".into()), Some("max".into()))
        );
        assert_eq!(
            fast_from_argv("codex", &argv(&["codex", "-c", "service_tier=\"priority\""])),
            Some(true)
        );
        assert_eq!(fast_from_argv("codex", &argv(&["codex", "-c", "service_tier=\"\""])), Some(false));
        assert_eq!(fast_from_argv("claude", &argv(&["claude", "-c", "service_tier=\"priority\""])), None);
    }

    /// Nothing is guessed: a bare CLI stays unset, and an effort the kind does not accept
    /// (`max` is claude/codex only) is dropped rather than stored for grok.
    #[test]
    fn unparseable_argv_leaves_the_fields_unset() {
        let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(model_effort_from_argv("claude", &argv(&["claude", "--dangerously-skip-permissions"])), (None, None));
        assert_eq!(model_effort_from_argv("grok", &argv(&["grok", "--reasoning-effort", "max"])), (None, None));
        // A flag whose value is missing must not swallow the next flag.
        assert_eq!(
            model_effort_from_argv("claude", &argv(&["claude", "--model", "--effort", "high"])),
            (None, Some("high".into()))
        );
    }

    #[test]
    fn grok_falls_back_to_its_terminal_title() {
        assert_eq!(grok_title_model_effort("Grok 4.6 (xhigh)"), (Some("grok-4.6".into()), Some("xhigh".into())));
        assert_eq!(grok_title_model_effort("Grok 4.6"), (Some("grok-4.6".into()), None));
        // A title the agent has renamed to its task says nothing about the model.
        assert_eq!(grok_title_model_effort("遠端主機 gh 登入 API 與 UI - grok"), (None, None));
        assert_eq!(grok_title_model_effort("Grok Code Fast"), (None, None));
    }

    #[test]
    fn grok_effort_is_read_off_the_tui_footer() {
        let high = "  │ ❯                                        │\n  ╰──────────────── Grok 4.6 (high) · always-approve ─╯\n";
        assert_eq!(grok_effort_from_screen(high).as_deref(), Some("high"));
        let medium = "  ╰──────────────────────────────── Grok 4.6 (medium) · always-approve ─╯\n";
        assert_eq!(grok_effort_from_screen(medium).as_deref(), Some("medium"));
        assert_eq!(grok_effort_from_screen("claude composer, no grok footer"), None);
    }

    /// agy 的清單是寫死的 slug，effort 已在 slug 裡：每個模型的 `efforts` 都是空的（`--effort` 留給第二階段），有且只有一個預設。
    #[test]
    fn agy_models_are_static_slugs_without_separate_efforts() {
        let models = agy_static_models();
        assert_eq!(models.iter().filter(|m| m["is_default"] == true).count(), 1);
        for m in &models {
            assert_eq!(m["efforts"], json!([]));
        }
        assert_eq!(model_effort_from_argv("agy", &["agy".into(), "--model".into(), "gemini-3.8-flash-high".into()]).0.as_deref(), Some("gemini-3.8-flash-high"));
    }

    /// 2026-10-05：agy 只留 Gemini 3.8 Flash 三檔，預設還是 3.8 medium；拿掉的模型（3.7／3.6 Flash、3.1 Pro、claude、gpt-oss）一個都不在清單裡，
    /// 已存在的 bot 設了它們就換成 3.8 medium（啟動、寫設定、adopted 的 argv 都是同一支 `canonical_model`）。
    #[test]
    fn agy_models_are_gemini_38_flash_only_and_retired_ones_fall_back_to_the_default() {
        let ids: Vec<String> = agy_static_models().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["gemini-3.8-flash-medium", "gemini-3.8-flash-high", "gemini-3.8-flash-low"]);
        assert_eq!(agy_static_models()[0]["is_default"], true, "預設維持 3.8 medium");
        assert_eq!(AGY_DEFAULT_MODEL, ids[0]);
        for retired in AGY_RETIRED_MODELS {
            assert!(!ids.iter().any(|i| i == retired), "{retired} 不該還在清單裡");
            assert_eq!(canonical_model("agy", retired), AGY_DEFAULT_MODEL, "{retired}");
            assert_eq!(remap_deprecated_model("agy", retired), Some(AGY_DEFAULT_MODEL));
        }
        assert_eq!(AGY_RETIRED_MODELS.len(), 11, "3.7×3、3.6×3、3.1 Pro×2、claude×2、gpt-oss");
        // 清單裡的三檔與別的 kind 的同名字串、沒見過的新 slug 都原樣。
        for id in &ids {
            assert_eq!(canonical_model("agy", id), id);
        }
        assert_eq!(canonical_model("agy", "gemini-3.9-flash-high"), "gemini-3.9-flash-high", "使用者自己的選擇不動");
        assert_eq!(remap_deprecated_model("claude", "claude-sonnet-4-6"), None, "只對 agy");
        assert_eq!(model_effort_from_argv("agy", &["agy".into(), "--model".into(), "gemini-3.1-pro-high".into()]).0.as_deref(), Some("gemini-3.8-flash-medium"));
    }

    /// `/effort` 之後對話裡還留著上一則 `Switched to … (low effort)`。回讀必須用框底，不能用上面那則。
    #[test]
    fn grok_effort_readback_uses_the_composer_not_an_earlier_switch_line() {
        let screen = "\
  ⏺ Switched to Grok 4.7 (low effort)

  ╭────────────────────────────────────────────────╮
  │ ❯                                              │
  ╰────────────── Grok 4.7 (high) · always-approve ─╯
";
        assert_eq!(grok_effort_from_screen(screen).as_deref(), Some("high"));
    }

    /// Real `settings.json` shapes seen on this machine and on m4p, 2026-09-07: an account
    /// with a global default and one override, and a host with only an override (no global).
    #[test]
    fn claude_effort_hint_prefers_the_per_model_override() {
        let (global, per_model) = parse_claude_effort_settings(
            r#"{"effortLevel":"high","modelSettings":{"claude-opus-5":{"effortLevel":"low"}}}"#,
        );
        assert_eq!(global.as_deref(), Some("high"));
        assert_eq!(per_model.get("claude-opus-5").map(String::as_str), Some("low"));

        let models = claude_static_models(global.as_deref(), &per_model);
        let of = |id: &str| models.iter().find(|m| m["id"] == id).unwrap()["default_effort"].as_str().map(String::from);
        assert_eq!(of("opus"), Some("low".into()), "per-model override wins");
        assert_eq!(of("sonnet"), Some("high".into()), "falls back to the account default");

        // m4p: no top-level `effortLevel`, only a fable override.
        let (global2, per_model2) =
            parse_claude_effort_settings(r#"{"modelSettings":{"claude-fable-5-1":{"effortLevel":"low"}}}"#);
        assert_eq!(global2, None);
        let models2 = claude_static_models(global2.as_deref(), &per_model2);
        let of2 = |id: &str| models2.iter().find(|m| m["id"] == id).unwrap()["default_effort"].clone();
        assert_eq!(of2("fable"), json!("low"));
        // No override and no global: the CLI's own default (verified on a fresh cc2 identity).
        assert_eq!(of2("opus"), json!("high"));

        // Unreadable / not JSON: no override, no global — same built-in fallback.
        let (g3, m3) = parse_claude_effort_settings("");
        assert_eq!(g3, None);
        assert!(m3.is_empty());
        let models3 = claude_static_models(g3.as_deref(), &m3);
        assert_eq!(models3[0]["default_effort"], json!("high"));
    }

    #[test]
    fn grok_text_parses() {
        let t = "You are logged in with grok.com.\n\nDefault model: grok-4.5\n\nAvailable models:\n  - grok-4.6\n  * grok-4.5 (default)\n";
        let m = grok_models_from_text(t);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["id"], "grok-4.6");
        assert_eq!(m[0]["is_default"], false);
        assert_eq!(m[1]["id"], "grok-4.5");
        assert_eq!(m[1]["is_default"], true);
        assert_eq!(m[1]["efforts"][2], "high");
    }

    #[test]
    fn grok_cache_enriches_per_model_efforts() {
        let t = "Default model: grok-4.5\n\nAvailable models:\n  - grok-4.6\n  * grok-4.5 (default)\n";
        let m = grok_models_from_text(t);
        let cache = json!({
            "models": {
                "grok-4.6": {"info": {"reasoning_efforts": [
                    {"id": "xhigh", "value": "xhigh", "default": false},
                    {"id": "high", "value": "high", "default": true},
                    {"id": "medium", "value": "medium", "default": false},
                    {"id": "low", "value": "low", "default": false}
                ]}},
                "grok-4.5": {"info": {"reasoning_efforts": [
                    {"id": "high", "value": "high", "default": true},
                    {"id": "medium", "value": "medium", "default": false},
                    {"id": "low", "value": "low", "default": false}
                ]}}
            }
        });
        let m = enrich_grok_models(m, &cache.to_string(), "default_reasoning_effort = \"medium\"\n");
        assert_eq!(m[0]["efforts"], json!(["low", "medium", "high", "xhigh"]));
        assert_eq!(m[0]["default_effort"], "high");
        assert_eq!(m[1]["efforts"], json!(["low", "medium", "high"]));
        assert_eq!(m[1]["default_effort"], "high");
    }

    #[test]
    fn grok_config_effort_top_level_with_trailing_comment() {
        let cfg = "default_reasoning_effort = \"high\"  # 2026-09\n";
        assert_eq!(grok_default_effort_from_config(cfg).as_deref(), Some("high"));
    }

    #[test]
    fn grok_config_effort_models_table_and_single_quotes() {
        let cfg = "[models]\ndefault_reasoning_effort = 'medium' # note\n";
        assert_eq!(grok_default_effort_from_config(cfg).as_deref(), Some("medium"));
    }

    #[test]
    fn grok_config_effort_top_level_wins_over_models_table() {
        let cfg = "default_reasoning_effort = \"high\"\n[models]\ndefault_reasoning_effort = \"low\"\n";
        assert_eq!(grok_default_effort_from_config(cfg).as_deref(), Some("high"));
    }

    #[test]
    fn grok_config_effort_ignores_other_tables_and_junk() {
        // A same-named key under an unrelated table is not the global default.
        let cfg = "[models.\"grok-4.5\"]\ndefault_reasoning_effort = \"low\"\n";
        assert_eq!(grok_default_effort_from_config(cfg), None);
        assert_eq!(grok_default_effort_from_config("default_reasoning_effort = \"\"\n"), None);
        assert_eq!(grok_default_effort_from_config("default_reasoning_effort = 3\n"), None);
        assert_eq!(grok_default_effort_from_config("not toml = = =\n"), None);
        assert_eq!(grok_default_effort_from_config(""), None);
    }

    #[test]
    fn codex_rpc_result_maps() {
        let r = json!({"data": [{
            "id": "gpt-6-astra", "displayName": "GPT-6-Astra", "description": "d", "isDefault": true,
            "defaultReasoningEffort": "low",
            "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": ""}, {"reasoningEffort": "ultra", "description": ""}],
            "serviceTiers": [{"id": "priority", "name": "Fast", "description": "2x speed, increased usage"}]
        }]});
        let m = codex_models_from_rpc(&r);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["id"], "gpt-6-astra");
        assert_eq!(m[0]["default_effort"], "low");
        assert_eq!(m[0]["efforts"], json!(["low", "ultra"]));
        assert_eq!(m[0]["service_tiers"][0]["id"], "priority");
    }

    #[test]
    fn deprecated_models_are_remapped_only_on_exact_matches() {
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-sol"), Some("gpt-6-sol"));
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-terra"), Some("gpt-6-sol"));
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-luna"), Some("gpt-6-luna"));
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-sol-preview"), None);
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-terra-extra"), None);
        assert_eq!(remap_deprecated_model("codex", "gpt-5.6-luna-extra"), None);
        assert_eq!(remap_deprecated_model("claude", "opus"), Some("claude-opus-5-5"));
        assert_eq!(remap_deprecated_model("claude", "claude-opus-4-1"), None);
        assert_eq!(remap_deprecated_model("claude", "claude-opus-5-5"), None);
    }

    #[test]
    fn adopted_child_argv_model_is_canonicalized_only_for_retired_exact_names() {
        let argv = |model: &str| vec!["codex".into(), "-m".into(), model.into()];
        assert_eq!(model_effort_from_argv("codex", &argv("gpt-5.6-sol")).0.as_deref(), Some("gpt-6-sol"));
        assert_eq!(model_effort_from_argv("codex", &argv("gpt-5.6-terra")).0.as_deref(), Some("gpt-6-sol"));
        assert_eq!(model_effort_from_argv("codex", &argv("gpt-5.6-luna")).0.as_deref(), Some("gpt-6-luna"));
        let argv = vec!["claude".into(), "--model".into(), "opus".into()];
        assert_eq!(model_effort_from_argv("claude", &argv).0.as_deref(), Some("claude-opus-5-5"));
        let argv = vec!["claude".into(), "--model".into(), "claude-opus-4-1".into()];
        assert_eq!(model_effort_from_argv("claude", &argv).0.as_deref(), Some("claude-opus-4-1"));
    }

    #[test]
    fn codex_model_catalog_hides_all_deprecated_56_models_exactly() {
        let r = json!({"data": [
            {"id": "gpt-5.6-sol", "displayName": "Old Sol"},
            {"id": "gpt-5.6-terra", "displayName": "Old Terra"},
            {"id": "gpt-5.6-luna", "displayName": "Old Luna"},
            {"id": "gpt-5.6-sol-preview", "displayName": "Preview"},
            {"id": "gpt-5.6-luna-preview", "displayName": "Other"},
            {"id": "gpt-6-sol", "displayName": "New Sol"},
            {"id": "gpt-6-luna", "displayName": "New"}
        ]});
        let mapped = codex_models_from_rpc(&r);
        let ids: Vec<_> = mapped.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["gpt-5.6-sol-preview", "gpt-5.6-luna-preview", "gpt-6-sol", "gpt-6-luna"]);
    }

    #[test]
    fn find_response_skips_notifications() {
        let text = "{\"method\":\"remoteControl/status/changed\",\"params\":{}}\n{\"id\":1,\"result\":{}}\n{\"id\":2,\"result\":{\"data\":[]}}\n";
        let r = find_response(text, 2).unwrap().unwrap();
        assert_eq!(r["data"], json!([]));
        assert!(find_response(text, 9).is_none());
        let err = "{\"id\":2,\"error\":{\"code\":-1,\"message\":\"nope\"}}";
        assert!(find_response(err, 2).unwrap().is_err());
    }
