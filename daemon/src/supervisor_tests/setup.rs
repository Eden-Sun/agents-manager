//! #1862 S1：從 `crates/am-supervisor/src/supervisor/setup.rs` 搬來的 daemon 整合測試（要 `state::App`／`testing::env`／runner）；
//! 測試名 `supervisor::setup::tests::…` → `supervisor_tests::setup::tests::…`。測試本文與斷言不變，只有 `use super::*` 改成 `use crate::supervisor::setup::*`。

#[allow(unused_imports)]
mod tests {
    use crate::supervisor::setup::*;
    // 接縫別名：測試本文照舊寫 `runtime_json(..)`／`claude_md(..)`。
    use crate::supervisor::setup::{claude_md_for_test as claude_md, runtime_json_for_test as runtime_json};
    use crate::config::{BotCfg, ProjectCfg};
    use crate::lifecycle::LcError;
    use crate::supervisor::ports::ConfigProjection;
    use sqlx::SqlitePool;
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use crate::supervisor::store;

    #[test]
    fn the_persona_body_drops_the_human_preamble() {
        let body = persona_body();
        assert!(body.starts_with("你是 AGM"), "persona body starts at the role line: {:?}", &body[..40.min(body.len())]);
        assert!(!body.contains("以下正文供 AGM bot"), "the note to the human reader is not part of the persona");
        assert!(body.contains("cc0/fable/low"), "the fixed candidate order survives the split");
    }

    #[test]
    fn candidates_stay_aliases_so_a_model_release_cannot_strand_the_supervisor() {
        assert_eq!(model_arg("fable"), "fable");
        assert_eq!(model_arg("sonnet"), "sonnet");
        assert_eq!(model_arg("bogus"), "fable");
        assert_eq!(model_arg("opus"), "claude-opus-5-5");
        assert_eq!(other_candidate("fable"), "opus");
        assert_eq!(other_candidate("opus"), "fable");
    }

    /// The one file the CLI reads on every call. A token leaking in here would end up in a
    /// directory the manager itself can read and quote back.
    #[test]
    fn the_runtime_file_names_the_manager_and_carries_no_secret() {
        let v = runtime_json(7788, "botULID", Some("respULID"), "/data");
        assert_eq!(v["daemon_url"], "http://127.0.0.1:7788");
        assert_eq!(v["manager_bot_id"], "botULID");
        assert_eq!(v["responder_bot_id"], "respULID", "巡檢目錄的 ops 腳本派工給協調者要讀得到");
        assert_eq!(runtime_json(7788, "botULID", None, "/data")["responder_bot_id"], Value::Null, "單角色安裝照實寫 null");
        assert_eq!(v["bot_id"], "botULID", "the older key stays, so either half can upgrade first");
        assert_eq!(v["role"], "patrol");
        assert_eq!(v["self_bot_id"], "botULID");
        let text = v.to_string().to_lowercase();
        assert!(!text.contains("token"), "no token, and no field that could hold one: {text}");
        assert!(v.get("remote_name").is_none(), "Remote Control 已廢（#1477），沒有讀者的欄位不留");
    }

    #[test]
    fn shared_project_instructions_do_not_assign_the_patrol_identity_to_other_bots() {
        let md = claude_md(Path::new("/patrol-only"), "patrol-only-id", 7799);
        assert!(!md.contains("/patrol-only"));
        assert!(!md.contains("patrol-only-id"));
        assert!(!md.contains("7799"));
        assert!(md.contains("AM_BOT_ID"));
        assert!(md.contains("runtime.json"));
        assert!(md.contains("子 agent 不因繼承這份文件取得 AGM 身份或權限"));
    }

    /// ops 腳本測試用的 runtime.json 必須是**真的** `runtime_json()` 寫出來的形狀：上一輪測試自己造了一份
    /// 正式安裝根本不存在的形狀，腳本在測試裡綠、在正式環境每一輪跳過（review2 sup #1）。
    /// 形狀一變這裡就紅；照訊息把 fixture 換成新的輸出，bash 測試才會跟著吃到。
    #[test]
    fn the_ops_script_fixture_is_what_setup_actually_writes() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("daemon/Cargo.toml").is_file())
            .expect("crate manifest directory is under the workspace");
        let path = repo.join("scripts/ops/fixtures/patrol-runtime.json");
        let want = runtime_json(7788, "bot-agm", Some("bot-resp"), "/data");
        let have: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture exists")).expect("fixture is JSON");
        assert_eq!(have, want, "scripts/ops/fixtures/patrol-runtime.json 要換成：\n{}", serde_json::to_string_pretty(&want).unwrap());
    }

    /// 協調者在巡檢之後才建立：巡檢目錄的 runtime.json 要補上它；巡檢還沒 setup 過就什麼都不寫。
    #[tokio::test]
    async fn the_patrol_runtime_file_learns_about_a_responder_set_up_later() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let file = agm_dir(&app).join("runtime.json");
        write_runtime_json(&app, "bot-agm", Some("bot-resp")).unwrap();
        assert!(!file.exists(), "巡檢目錄不存在時不替它建");
        std::fs::create_dir_all(agm_dir(&app)).unwrap();
        write_runtime_json(&app, "bot-agm", None).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["responder_bot_id"], Value::Null);
        write_runtime_json(&app, "bot-agm", Some("bot-resp")).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!((v["manager_bot_id"].as_str(), v["responder_bot_id"].as_str()), (Some("bot-agm"), Some("bot-resp")));
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// #1477：只移 `--remote-control <name>` 這一對；使用者的其他 args 原樣、順序不變。
    #[test]
    fn only_the_remote_control_pair_is_removed_from_args() {
        assert_eq!(without_remote_control(&strs(&["--remote-control", "AGM"])), Some(vec![]));
        assert_eq!(
            without_remote_control(&strs(&["--verbose", "--remote-control", "AGM", "--model", "x"])),
            Some(strs(&["--verbose", "--model", "x"]))
        );
        assert_eq!(without_remote_control(&strs(&["--remote-control=AGM", "--verbose"])), Some(strs(&["--verbose"])));
        assert_eq!(
            without_remote_control(&strs(&["--remote-control", "--verbose"])),
            Some(strs(&["--verbose"])),
            "旗標後面是另一個旗標：它沒有名字，只拿旗標本身"
        );
        assert_eq!(without_remote_control(&strs(&["--verbose"])), None, "沒有就是沒有：不重寫");
        assert_eq!(without_remote_control(&[]), None);
    }

    /// #1477：既有安裝的巡檢 bot 還帶著 `--remote-control AGM`。開機遷移與重跑 setup 都只拿掉那一對、其他 args 不動，
    /// 並且讓用舊設定起來的 run 變成「需重啟」（`launch_rev` 不同，既有機制）；新 setup 不再寫它。
    #[tokio::test]
    async fn an_existing_install_loses_only_the_remote_control_pair_and_is_marked_for_restart() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sup = store::get_or_init(&app.db).await.unwrap();
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![crate::config::IdentityCfg { name: sup.identity.clone(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] }],
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
            },
        );
        let args_in_cfg = |app: std::sync::Arc<crate::state::App>, id: String| async move {
            app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).find(|b| b.id.as_deref() == Some(id.as_str())).expect("bot in config").args.clone()
        };

        let (_, bot_id, _) = ensure_env(&app).await.map_err(|e| format!("{e:?}")).expect("新安裝 setup");
        assert!(args_in_cfg(app.clone(), bot_id.clone()).await.is_empty(), "新 setup 不再帶 --remote-control");
        assert_eq!(strip_remote_control_args(&app).await.unwrap(), 0, "沒有就不動");

        // 舊安裝：args 裡有 Remote Control 一對與使用者自己加的旗標；run 是用那份設定起來的。
        let id = bot_id.clone();
        app.update_and_project(move |cfg| {
            for b in cfg.projects.iter_mut().flat_map(|p| p.bots.iter_mut()) {
                if b.id.as_deref() == Some(id.as_str()) {
                    b.args = vec!["--remote-control".into(), "AGM".into(), "--verbose".into()];
                }
            }
            Ok(())
        })
        .await
        .unwrap();
        let run_id = crate::testing::fake_run(&app, &bot_id).await;
        let old_rev = crate::launch_rev::of(&crate::db::bot(&app.db, &bot_id).await.unwrap().unwrap());
        sqlx::query("UPDATE runs SET launch_rev=? WHERE id=?").bind(&old_rev).bind(&run_id).execute(&app.db).await.unwrap();
        let stale = |app: std::sync::Arc<crate::state::App>, id: String| async move {
            let bot = crate::db::bot(&app.db, &id).await.unwrap().unwrap();
            let run = crate::db::active_run(&app.db, &id).await.unwrap().expect("active run");
            crate::launch_rev::is_stale(&bot, &run)
        };
        assert!(!stale(app.clone(), bot_id.clone()).await, "前提：run 載入的就是現在的設定");

        assert_eq!(strip_remote_control_args(&app).await.unwrap(), 1);
        assert_eq!(args_in_cfg(app.clone(), bot_id.clone()).await, strs(&["--verbose"]), "只拿掉那一對");
        let projected = crate::db::bot(&app.db, &bot_id).await.unwrap().unwrap();
        assert_eq!(projected.args(), strs(&["--verbose"]), "投影進資料庫的也一樣");
        assert!(stale(app.clone(), bot_id.clone()).await, "舊 run 還拿著舊的 argv：要標需重啟，而不是被自動重啟");
        assert_eq!(strip_remote_control_args(&app).await.unwrap(), 0, "冪等：第二次什麼都不做");

        // 重跑 setup：不把它寫回去，使用者的旗標也不被蓋掉。
        let id = bot_id.clone();
        app.update_and_project(move |cfg| {
            for b in cfg.projects.iter_mut().flat_map(|p| p.bots.iter_mut()) {
                if b.id.as_deref() == Some(id.as_str()) {
                    b.args = vec!["--verbose".into(), "--remote-control".into(), "AGM".into()];
                }
            }
            Ok(())
        })
        .await
        .unwrap();
        ensure_env(&app).await.map_err(|e| format!("{e:?}")).expect("重跑 setup");
        assert_eq!(args_in_cfg(app.clone(), bot_id.clone()).await, strs(&["--verbose"]), "setup 也順手移掉舊的一對");
    }

    /// 設定 AGM 做到一半失敗：config 已經寫進 `AGM` 這顆 bot，之後部署檔案（或把 bot 記進 supervisor 列）那一步失敗。
    /// 再按一次設定要能接著做完。以前重跑時 supervisor 列還沒記 bot id，找到的是上一次自己寫進去的那顆 `AGM`，被當成
    /// 「別的 bot 已經叫 AGM」回 409 `name_taken`——之後永遠設定不起來，只能手動改 config（issue #181）。
    #[tokio::test]
    async fn a_setup_that_failed_halfway_can_be_run_again() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sup = store::get_or_init(&app.db).await.unwrap();
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![crate::config::IdentityCfg { name: sup.identity.clone(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] }],
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
            },
        );
        // 部署檔案那一步失敗：`bin` 被一個檔案佔住。
        let bin = agm_dir(&app).join("bin");
        std::fs::create_dir_all(agm_dir(&app)).unwrap();
        std::fs::write(&bin, "not a directory").unwrap();
        assert!(ensure_env(&app).await.is_err(), "前提：做到一半失敗");
        std::fs::remove_file(&bin).unwrap();

        let (_, bot_id, _) = ensure_env(&app).await.map_err(|e| format!("{e:?}")).expect("再跑一次要能做完");
        let agms: Vec<String> =
            app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).filter(|b| b.name == BOT_NAME).filter_map(|b| b.id.clone()).collect();
        assert_eq!(agms, vec![bot_id.clone()], "接著用上一次寫進去的那一顆，不另外長一顆");
        assert_eq!(store::get_or_init(&app.db).await.unwrap().bot_id.as_deref(), Some(bot_id.as_str()));
    }

    /// 名稱不當參照：做到一半失敗之後，那顆 `AGM` 被使用者改名，重跑仍靠 setup 留的記號找回它（不另長一顆）；
    ///
    #[tokio::test]
    async fn a_half_made_agm_is_found_by_its_mark_even_after_a_rename() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let sup = store::get_or_init(&app.db).await.unwrap();
        app.tools.lock().await.insert(
            crate::config::LOCAL_HOST.to_string(),
            crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: vec![crate::config::IdentityCfg { name: sup.identity.clone(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] }],
                utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
            },
        );
        let bin = agm_dir(&app).join("bin");
        std::fs::create_dir_all(agm_dir(&app)).unwrap();
        std::fs::write(&bin, "not a directory").unwrap();
        assert!(ensure_env(&app).await.is_err(), "前提：做到一半失敗");
        std::fs::remove_file(&bin).unwrap();

        let half_made = app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).find(|b| b.name == BOT_NAME).and_then(|b| b.id.clone()).expect("config 已寫進 AGM");
        let id = half_made.clone();
        app.cfg
            .update(move |cfg| {
                for b in cfg.projects.iter_mut().flat_map(|p| p.bots.iter_mut()) {
                    if b.id.as_deref() == Some(id.as_str()) {
                        b.name = "使用者改的名".into();
                    }
                }
                Ok(())
            })
            .await
            .unwrap();
        crate::projection::project_config(&app.cfg, &app.db).await.unwrap();

        let (_, bot_id, _) = ensure_env(&app).await.map_err(|e| format!("{e:?}")).expect("改名後重跑要能做完");
        assert_eq!(bot_id, half_made, "靠記號找回改名後的那一顆");
        let total: usize = app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).count();
        assert_eq!(total, 1, "沒有另外長一顆");
    }

    #[test]
    fn the_cli_is_compiled_in_not_read_off_the_build_machine() {
        assert!(AGM_CLI.contains("add_parser(\"assignments\""), "the deployed CLI is the real agm.py");
    }

    struct TestApp {
        dir: PathBuf,
    }
    impl crate::capabilities::DataDir for TestApp {
        fn data_dir(&self) -> &Path {
            &self.dir
        }
    }
    impl crate::capabilities::ListenPort for TestApp {
        fn port(&self) -> u16 {
            7788
        }
    }

    /// #919：deploy_files 重複跑（重寫每個檔）之後，目錄裡只有該有的檔，沒有暫存檔；權限是 0644，bin/agm 0755。
    #[test]
    fn deploy_files_leaves_no_temp_files_behind() {
        use std::os::unix::fs::PermissionsExt;
        let app = TestApp { dir: crate::test_dirs::scratch_dir("am-deploy") };
        for _ in 0..3 {
            deploy_files(&app, "botULID", Some("respULID"), "persona text").unwrap();
        }
        let dir = agm_dir(&app);
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["CLAUDE.md", "bin", "handoff.md", "persona.md", "runtime.json"], "不留暫存檔");
        let bin: Vec<String> = std::fs::read_dir(dir.join("bin")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(bin, vec!["agm"], "bin/ 也不留暫存檔");
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.join("runtime.json")), 0o644);
        assert_eq!(mode(dir.join("persona.md")), 0o644);
        assert_eq!(std::fs::read_to_string(dir.join("persona.md")).unwrap(), "persona text");
        let runtime: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("runtime.json")).unwrap()).unwrap();
        assert_eq!(runtime["manager_bot_id"], "botULID");
        std::fs::remove_dir_all(&app.dir).ok();
    }
}
