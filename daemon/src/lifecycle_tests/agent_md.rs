#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle::agent_md::test_seams::*;
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::agent_md::*;
    /// 專案那份跟 repo 放在一起：遠端專案到那台主機上讀，`~` 用那台的 HOME 展開；讀不到要講出主機名。
    #[tokio::test]
    async fn a_remote_projects_file_is_read_on_its_host() {
        let host = "agent-md-box";
        let env = crate::testing::env().await;
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), herdr_session: "agents-manager".into(), ..Default::default() };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some("/Users/x".into());
        crate::hosts::set_ssh_fake(host, |script| {
            Ok(if script.contains("F='/Users/x/repo/CLAUDE.md'") { "AM_MD_OK\nREMOTE RULES\n".into() } else { "AM_MD_MISSING\n".into() })
        });
        assert_eq!(read_remote(&conn, "~/repo/CLAUDE.md").await.unwrap().trim(), "REMOTE RULES");
        let err = read_remote(&conn, "/nope.md").await.unwrap_err();
        assert!(err.contains("/nope.md") && err.contains(host), "{err}");
    }
    mod profile_tests {
        use super::*;
        use crate::capabilities::Db as _;
        use base64::Engine as _;
        use std::sync::{Arc, Mutex};

        const INITIAL: &str = "# retain header\ndeveloper_instructions = \"old\" # retain inline\nmodel = \"fixture-model\"\n\n# retain table\n[hooks.state.\"file:fixture:session_start:0:0\"]\ntrusted_hash = \"fixture-trust-no-secret\"\nenabled = true\n# retain trailer\n\n";
        const PLACEHOLDER: &str = "/__AM_PROFILE_FIXTURE__/profile.toml";

        fn framed(text: &[u8]) -> String {
            format!(
                "AM_PROFILE_PRESENT\n{}\nAM_PROFILE_END\n",
                base64::engine::general_purpose::STANDARD.encode(text)
            )
        }
        fn shell(script: &str) -> String {
            let out = std::process::Command::new("sh")
                .args(["-c", script])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "profile script failed with {:?}",
                out.status.code()
            );
            String::from_utf8(out.stdout).unwrap()
        }

        #[test]
        fn profile_patch_changes_only_the_instruction_value_preserving_all_other_bytes() {
            let updated = patch_codex_profile("fixture.toml", INITIAL, "new").unwrap();
            assert_eq!(
                updated,
                INITIAL.replace(
                    "developer_instructions = \"old\"",
                    "developer_instructions = \"new\""
                )
            );
            assert_eq!(
                patch_codex_profile("fixture.toml", INITIAL, "old").unwrap(),
                INITIAL
            );
            for original in [INITIAL.replace('\n', "\r\n"), INITIAL.trim_end().to_owned()] {
                let expected = original.replace(
                    "developer_instructions = \"old\"",
                    "developer_instructions = \"new\"",
                );
                assert_eq!(
                    patch_codex_profile("fixture.toml", &original, "new").unwrap(),
                    expected
                );
            }

            let new = patch_codex_profile(
                "fixture.toml",
                "# header\nmodel = \"fixture\"\n",
                "line 'one'\nline \\\"two\"",
            )
            .unwrap();
            let doc: toml_edit::DocumentMut = new.parse().unwrap();
            assert_eq!(
                doc["developer_instructions"].as_str(),
                Some("line 'one'\nline \\\"two\"")
            );
            assert!(new.contains("# header\nmodel = \"fixture\"\n"));
            assert!(
                doc.get("hooks").is_none(),
                "daemon must not generate hooks.state"
            );
            assert!(!new.contains("trusted_hash"));
        }

        #[test]
        fn profile_parse_errors_name_the_path_and_line_but_never_echo_source() {
            let bad = "developer_instructions = \"old\"\n# trusted_hash = \"FIXTURE_DO_NOT_ECHO\"\n[broken =\n";
            let error = patch_codex_profile("/fixture/profile.toml", bad, "new").unwrap_err();
            let display = error.to_string();
            assert!(display.contains("/fixture/profile.toml"));
            assert!(display.contains("第 3 行"), "{display}");
            assert!(display.contains("修好這個檔或刪掉它，就會重新產生"));
            assert!(!format!("{error:?} {error}").contains("FIXTURE_DO_NOT_ECHO"));
            for bad in [
                "developer_instructions = 7\n",
                "[developer_instructions]\nold = true\n",
            ] {
                assert!(patch_codex_profile("fixture.toml", bad, "new").is_err());
            }
        }

        #[tokio::test]
        async fn profile_local_install_preserves_trust_and_keeps_invalid_files_unchanged() {
            let env = crate::testing::env().await;
            let mut bot = crate::testing::claude_bot(&env.app, &env.project_id, "profile-local").await;
            bot.kind = "codex".into();
            let project = db::project(env.app.db(), &env.project_id)
                .await
                .unwrap()
                .unwrap();
            let home = env.dir.join("codex-profile");
            std::fs::create_dir_all(&home).unwrap();
            let path = home.join(format!("{}.config.toml", codex_profile_name(&bot.id)));
            std::fs::write(&path, INITIAL).unwrap();
            let name = install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                .await
                .unwrap();
            assert_eq!(name, codex_profile_name(&bot.id));
            let expected = patch_codex_profile(path.to_str().unwrap(), INITIAL, "new").unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                .await
                .unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                modified
            );
            let broken = "# FIXTURE_DO_NOT_ECHO\n[broken =\n";
            std::fs::write(&path, broken).unwrap();
            let error = install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("FIXTURE_DO_NOT_ECHO"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
            std::fs::write(&path, [0xff, 0xfe]).unwrap();
            assert!(
                install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), vec![0xff, 0xfe]);
        }

        #[tokio::test]
        async fn profile_local_missing_is_created_but_other_read_errors_are_not_missing() {
            let env = crate::testing::env().await;
            let bot = crate::testing::claude_bot(&env.app, &env.project_id, "profile-missing").await;
            let project = db::project(env.app.db(), &env.project_id)
                .await
                .unwrap()
                .unwrap();
            let home = env.dir.join("new-codex");
            install_codex_profile(&*env.app, &bot, &project, home.to_str().unwrap(), "new")
                .await
                .unwrap();
            let path = home.join(format!("{}.config.toml", codex_profile_name(&bot.id)));
            let contents = std::fs::read_to_string(&path).unwrap();
            assert_eq!(contents, patch_codex_profile("fresh", "", "new").unwrap());
            assert!(!contents.contains("hooks") && !contents.contains("trusted_hash"));
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(
                local_profile_snapshot(&home).is_err(),
                "directory read error is not NotFound"
            );
        }

        #[test]
        fn profile_remote_envelope_requires_explicit_missing_or_complete_valid_utf8() {
            assert_eq!(
                decode_remote_profile("fixture", "AM_PROFILE_MISSING\n").unwrap(),
                ""
            );
            assert_eq!(
                decode_remote_profile("fixture", &framed(INITIAL.as_bytes())).unwrap(),
                INITIAL
            );
            for reply in [
                "",
                "AM_PROFILE_MISSING",
                "AM_PROFILE_MISSING\nextra",
                "AM_PROFILE_READ_ERROR\n",
                "AM_PROFILE_PRESENT\n",
                "AM_PROFILE_PRESENT\nnot-base64\nAM_PROFILE_END\n",
            ] {
                assert!(decode_remote_profile("fixture", reply).is_err());
            }
            assert!(decode_remote_profile("fixture", &framed(&[0xff, 0xfe])).is_err());
        }

        #[tokio::test]
        async fn profile_remote_failures_never_publish_or_become_missing() {
            let env = crate::testing::env().await;
            let bot =
                crate::testing::claude_bot(&env.app, &env.project_id, "profile-remote-failure").await;
            let mut project = db::project(env.app.db(), &env.project_id)
                .await
                .unwrap()
                .unwrap();
            let host = "profile-remote-failure";
            project.host = host.into();
            let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), herdr_session: "agents-manager".into(), ..Default::default() };
            env.app.hosts.insert_remote_for_test(cfg).await;
            for failure in [
                None,
                Some("AM_PROFILE_READ_ERROR\n".into()),
                Some("AM_PROFILE_PRESENT\nYWJj".into()),
                Some(framed(b"# FIXTURE_DO_NOT_ECHO\n[broken =\n")),
            ] {
                let writes = Arc::new(Mutex::new(0));
                let observed = writes.clone();
                crate::hosts::set_ssh_fake(host, move |script| {
                    if script.contains("AM_PROFILE_WRITTEN") {
                        *observed.lock().unwrap() += 1;
                        return Ok("AM_PROFILE_WRITTEN\n".into());
                    }
                    match &failure {
                        Some(reply) => Ok(reply.clone()),
                        None => Err(anyhow::anyhow!("FIXTURE_DO_NOT_ECHO transport")),
                    }
                });
                let error = install_codex_profile(&*env.app, &bot, &project, "/fixture/codex", "new")
                    .await
                    .unwrap_err();
                assert!(!format!("{error:?} {error}").contains("FIXTURE_DO_NOT_ECHO"));
                assert!(error
                    .to_string()
                    .contains("profile-remote-failure:./am-bot-"));
                assert!(!error.to_string().contains("/fixture/codex"));
                assert!(!error.to_string().contains("trusted_hash"));
                assert!(!error.to_string().contains("hooks.state"));
                assert_eq!(*writes.lock().unwrap(), 0);
            }
        }

        #[tokio::test]
        async fn profile_remote_existing_and_explicit_missing_take_distinct_safe_paths() {
            let env = crate::testing::env().await;
            let bot =
                crate::testing::claude_bot(&env.app, &env.project_id, "profile-remote-success").await;
            let mut project = db::project(env.app.db(), &env.project_id)
                .await
                .unwrap()
                .unwrap();
            let host = "profile-remote-success";
            project.host = host.into();
            let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), herdr_session: "agents-manager".into(), ..Default::default() };
            env.app.hosts.insert_remote_for_test(cfg).await;
            for previous in [None, Some(INITIAL)] {
                let scripts = Arc::new(Mutex::new(Vec::new()));
                let observed = scripts.clone();
                crate::hosts::set_ssh_fake(host, move |script| {
                    observed.lock().unwrap().push(script.to_owned());
                    if script.contains("AM_PROFILE_WRITTEN") {
                        Ok("AM_PROFILE_WRITTEN\n".into())
                    } else {
                        Ok(previous
                            .map(|s| framed(s.as_bytes()))
                            .unwrap_or_else(|| "AM_PROFILE_MISSING\n".into()))
                    }
                });
                install_codex_profile(&*env.app, &bot, &project, "/fixture/codex", "new")
                    .await
                    .unwrap();
                let calls = scripts.lock().unwrap();
                assert_eq!(calls.len(), 2);
                assert!(calls[0].contains("AM_PROFILE_PRESENT"));
                assert!(calls[1].contains("AM_PROFILE_WRITTEN"));
                assert_eq!(
                    calls[1].contains("fixture-trust-no-secret"),
                    previous.is_some()
                );
                if previous.is_none() {
                    assert!(!calls[1].contains("hooks.state") && !calls[1].contains("trusted_hash"));
                }
            }
        }

        #[tokio::test]
        async fn macos_local_profile_scripts_preserve_exact_bytes_and_use_portable_tools() {
            let env = crate::testing::env().await;
            let path = env.dir.join("profile ' spaced.toml");
            let path = path.to_str().unwrap();
            assert_eq!(
                shell(&remote_profile_read_script(path)),
                "AM_PROFILE_MISSING\n"
            );
            let updated = patch_codex_profile(path, INITIAL, "new\nAM_AGENT_MD_EOF\nend").unwrap();
            assert_eq!(
                shell(&remote_profile_write_script(path, &updated)),
                "AM_PROFILE_WRITTEN\n"
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), updated);
            assert_eq!(
                decode_remote_profile(path, &shell(&remote_profile_read_script(path))).unwrap(),
                updated
            );
            let modified = std::fs::metadata(path).unwrap().modified().unwrap();
            shell(&remote_profile_write_script(path, &updated));
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                modified
            );
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let blocked = env.dir.join("directory.toml");
            std::fs::create_dir(&blocked).unwrap();
            assert_eq!(
                shell(&remote_profile_read_script(blocked.to_str().unwrap())),
                "AM_PROFILE_READ_ERROR\n"
            );
            assert!(std::fs::read_dir(&env.dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")));
        }

        #[test]
        fn profile_portable_verification_fixture_is_from_the_production_helpers() {
            let updated = patch_codex_profile(PLACEHOLDER, INITIAL, "new").unwrap();
            let fresh = patch_codex_profile(PLACEHOLDER, "", "new").unwrap();
            let fixture = serde_json::json!({
                "placeholder": PLACEHOLDER, "initial": INITIAL, "updated": updated, "fresh": fresh,
                "read_script": remote_profile_read_script(PLACEHOLDER),
                "write_script": remote_profile_write_script(PLACEHOLDER, &updated),
                "fresh_script": remote_profile_write_script(PLACEHOLDER, &fresh),
            });
            println!("PROFILE_VERIFY_FIXTURE={fixture}");
            assert!(!fixture["fresh"].as_str().unwrap().contains("trusted_hash"));
        }
    }
}
