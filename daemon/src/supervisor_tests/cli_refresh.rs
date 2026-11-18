//! #1862 S1：從 `crates/am-supervisor/src/supervisor/cli_refresh.rs` 搬來的 daemon 整合測試（要 `state::App`／`testing::env`／runner）；
//! 測試名 `supervisor::cli_refresh::tests::…` → `supervisor_tests::cli_refresh::tests::…`。測試本文與斷言不變，只有 `use super::*` 改成 `use crate::supervisor::cli_refresh::*`。

#[allow(unused_imports)]
mod tests {
    use crate::supervisor::cli_refresh::*;
    use axum::extract::State;
    use axum::Json;
    use serde_json::{json, Value};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use crate::supervisor::roles::{self, Role};
    use crate::supervisor::store;
    use crate::supervisor_inbox::short_hash;

    /// issue #520：`bin/agm` 是一直有人在 exec 的 python 腳本。以前 `deploy_files` 用 `fs::write`
    /// （`O_TRUNC` 之後才寫），讀的人會看到空的或半截。這條真的開一個讀者執行緒一直讀，一邊反覆換版：
    /// **每一次讀到的內容都必須是完整的舊版或完整的新版**，不能有第三種。
    #[test]
    fn a_reader_only_ever_sees_a_whole_0755_executable() {
        let dir = tmpdir();
        let bin = dir.join("agm");
        // 夠大才看得出截斷：小檔案有機會在一次 write 裡寫完，測不到那個窗口。
        let old: String = "# old\n".repeat(20_000);
        let new: String = "# new\n".repeat(20_000);
        install_executable(&bin, old.as_bytes()).unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let reader = {
            let (bin, stop, seen) = (bin.clone(), stop.clone(), seen.clone());
            let (old, new) = (old.clone(), new.clone());
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // 權限跟內容一起看：檔案只要出現在最終路徑，就該已經是 0755。
                    // 對路徑 chmod 的寫法在「建檔」與「補 +x」之間會有一瞬是 umask 的 0644。
                    if let Ok(md) = std::fs::metadata(&bin) {
                        let mode = md.permissions().mode() & 0o777;
                        if mode != 0o755 {
                            seen.lock().unwrap().push(format!("權限 {mode:o}"));
                        }
                    }
                    if let Ok(got) = std::fs::read_to_string(&bin) {
                        reads += 1;
                        // 只記「不是舊也不是新」的那種，記字數就夠指認是不是半截。
                        if got != *"" && got.as_str() != old.as_str() && got.as_str() != new.as_str() {
                            seen.lock().unwrap().push(format!("{} 位元組", got.len()));
                        }
                    }
                }
                reads
            })
        };
        for i in 0..60 {
            let body = if i % 2 == 0 { &new } else { &old };
            // 每隔幾輪先刪掉：這樣才會走到「從無到有建檔」那條，權限窗口只在那裡出現。
            if i % 10 == 0 {
                let _ = std::fs::remove_file(&bin);
            }
            install_executable(&bin, body.as_bytes()).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let reads = reader.join().unwrap();

        let torn = seen.lock().unwrap().clone();
        assert!(torn.is_empty(), "讀到半截的內容、或權限不是 0755 的瞬間：{torn:?}");
        assert!(reads > 0, "讀者一次都沒讀到，這條沒有真的驗到東西");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// issue #520：以前是 `fs::write` 建檔（umask，通常 0644）之後才對**路徑** chmod 0755，
    /// 中間那一瞬檔案已經在那裡但還不能執行。`fchmod` 在暫存檔上設好再 rename，
    /// 所以那個檔一出現在最終路徑就是 0755——連第一次建立都沒有例外。
    #[test]
    fn the_executable_is_0755_the_moment_it_appears() {
        let dir = tmpdir();
        let bin = dir.join("agm");
        assert!(!bin.exists(), "前提：這是第一次建立");
        install_executable(&bin, b"#!/usr/bin/env python3\n").unwrap();
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "建出來就該是 0755，實際 {mode:o}");

        // 覆寫既有檔也要維持 0755：先把它改成 0644，再裝一次。
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        install_executable(&bin, b"#!/usr/bin/env python3\nprint(1)\n").unwrap();
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "覆寫之後也要是 0755，實際 {mode:o}");

        // 暫存檔不留下。
        let strays: Vec<String> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-")).collect();
        assert!(strays.is_empty(), "暫存檔沒清掉：{strays:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tmpdir() -> PathBuf {
        crate::testing::scratch_dir("am-cli-refresh")
    }

    fn with_bin(content: &str) -> PathBuf {
        let d = tmpdir();
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(d.join("bin/agm"), content).unwrap();
        for (f, body) in [("CLAUDE.md", "claude"), ("persona.md", "persona"), ("runtime.json", "{\"model\":\"x\"}")] {
            std::fs::write(d.join(f), body).unwrap();
        }
        d
    }

    fn baks(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d.join("bin"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("agm.bak-"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn same_content_is_not_rewritten() {
        let d = with_bin("NEW");
        let bin = d.join("bin/agm");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let before = std::fs::metadata(&bin).unwrap().modified().unwrap();
        assert_eq!(refresh_cli(&d, "NEW").unwrap(), Outcome::Unchanged);
        assert_eq!(std::fs::metadata(&bin).unwrap().modified().unwrap(), before);
        assert!(baks(&d).is_empty());
    }

    #[tokio::test]
    async fn same_content_non_executable_cli_is_reported_stale_and_repaired() {
        let env = crate::testing::env().await;
        let d = with_bin("NEW");
        let bin = d.join("bin/agm");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        store::get_or_init(&env.app.db).await.unwrap();
        store::set_env(&env.app.db, "patrol-bot", &env.project_id, &d.to_string_lossy()).await.unwrap();

        let before = status(&env.app, "NEW").await;
        assert_eq!(before["roles"][0]["state"], "stale", "same bytes without execute permission are unusable: {before}");

        let result = refresh_with(&env.app, "NEW").await;
        assert_eq!(result[0]["outcome"], "refreshed", "{result:?}");
        assert_eq!(std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(status(&env.app, "NEW").await["roles"][0]["state"], "ok");
    }

    #[test]
    fn different_content_is_backed_up_and_replaced_executable_and_nothing_else_moves() {
        let d = with_bin("OLD");
        let out = refresh_cli(&d, "NEW").unwrap();
        let name = format!("agm.bak-{}", short_hash(b"OLD"));
        assert_eq!(
            out,
            Outcome::Refreshed { old_hash: Some(short_hash(b"OLD")), new_hash: short_hash(b"NEW"), backup: Some(name.clone()) }
        );
        assert_eq!(std::fs::read_to_string(d.join("bin/agm")).unwrap(), "NEW");
        assert_eq!(std::fs::read_to_string(d.join("bin").join(&name)).unwrap(), "OLD");
        assert_eq!(std::fs::metadata(d.join("bin/agm")).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::read_to_string(d.join("CLAUDE.md")).unwrap(), "claude");
        assert_eq!(std::fs::read_to_string(d.join("persona.md")).unwrap(), "persona");
        assert_eq!(std::fs::read_to_string(d.join("runtime.json")).unwrap(), "{\"model\":\"x\"}");
        assert!(!std::fs::read_dir(d.join("bin")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".agm.tmp")));
    }

    #[test]
    fn the_same_old_version_is_backed_up_once() {
        let d = with_bin("OLD");
        refresh_cli(&d, "NEW").unwrap();
        let bak = d.join("bin").join(format!("agm.bak-{}", short_hash(b"OLD")));
        let stamp = std::fs::metadata(&bak).unwrap().modified().unwrap();
        // 有人手動裝回舊版，下一次開機又換掉：備份不重複、也不被覆寫。
        std::fs::write(d.join("bin/agm"), "OLD").unwrap();
        refresh_cli(&d, "NEWER").unwrap();
        assert_eq!(baks(&d), vec![format!("agm.bak-{}", short_hash(b"OLD"))]);
        assert_eq!(std::fs::metadata(&bak).unwrap().modified().unwrap(), stamp);
        assert_eq!(std::fs::read_to_string(d.join("bin/agm")).unwrap(), "NEWER");
    }

    #[test]
    fn a_missing_directory_is_skipped_not_created() {
        let d = tmpdir().join("never-set-up");
        assert_eq!(refresh_cli(&d, "NEW").unwrap(), Outcome::Skipped);
        assert!(!d.exists());
        let bare = tmpdir();
        assert_eq!(refresh_cli(&bare, "NEW").unwrap(), Outcome::Skipped);
        assert!(!bare.join("bin").exists());
    }

    #[test]
    fn the_hash_is_stable_across_builds() {
        assert_eq!(short_hash(b""), "cbf29ce48422");
        assert_eq!(short_hash(b"a"), "af63dc4c8601");
    }

    /// #532：安裝端落後看得見，而且不必重啟 daemon 就修得掉。
    #[tokio::test]
    async fn the_api_reports_a_stale_cli_and_can_replace_it_without_a_restart() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let patrol = with_bin("OLD");
        store::get_or_init(&app.db).await.unwrap();
        store::set_env(&app.db, "patrol-bot", &e.project_id, &patrol.to_string_lossy()).await.unwrap();

        let st = status(app, "NEW").await;
        assert_eq!(st["embedded_hash"], short_hash(b"NEW"));
        assert_eq!(st["roles"][0]["role"], "patrol");
        assert_eq!(st["roles"][0]["state"], "stale", "{st}");
        assert_eq!(st["roles"][0]["installed_hash"], short_hash(b"OLD"));

        refresh_with(app, "NEW").await; // POST 走的就是這一支
        let st = status(app, "NEW").await;
        assert_eq!(st["roles"][0]["state"], "ok", "{st}");
        assert_eq!(std::fs::read_to_string(patrol.join("bin/agm")).unwrap(), "NEW");
        assert_eq!(std::fs::read_to_string(patrol.join("persona.md")).unwrap(), "persona", "只動 bin/agm");

        // GET 報的是這顆 binary 真的內嵌的那份（ops-sync 要拿它跟 repo 比）。
        let Json(v) = get_cli(State(app.clone())).await;
        assert_eq!(v["embedded_hash"], short_hash(crate::supervisor::setup::AGM_CLI.as_bytes()));

        // 沒有 bin/ 的角色目錄：not_set_up，不代建。
        let bare = tmpdir();
        roles::set_env(&app.db, Role::Responder, "responder-bot", &e.project_id, &bare.to_string_lossy()).await.unwrap();
        let st = status(app, "NEW").await;
        let responder = st["roles"].as_array().unwrap().iter().find(|r| r["role"] == "responder").unwrap().clone();
        assert_eq!(responder["state"], "not_set_up", "{st}");
        assert!(!bare.join("bin").exists());
    }

    #[tokio::test]
    async fn startup_refreshes_configured_roles_only_and_a_write_failure_does_not_stop_boot() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let patrol = with_bin("OLD");
        let responder = with_bin("OLD");
        store::get_or_init(&app.db).await.unwrap();
        store::set_env(&app.db, "patrol-bot", &e.project_id, &patrol.to_string_lossy()).await.unwrap();
        // 協調者沒設定：它的目錄就算在也不動。
        refresh_with(app, "NEW").await;
        assert_eq!(std::fs::read_to_string(patrol.join("bin/agm")).unwrap(), "NEW");
        assert_eq!(std::fs::read_to_string(responder.join("bin/agm")).unwrap(), "OLD");

        roles::set_env(&app.db, Role::Responder, "responder-bot", &e.project_id, &responder.to_string_lossy()).await.unwrap();
        let before = roles::get(&app.db, Role::Responder).await.unwrap();
        let bin = responder.join("bin");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
        refresh_with(app, "NEW").await; // 不 panic、不回錯：開機照走
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(std::fs::read_to_string(bin.join("agm")).unwrap(), "OLD");
        assert_eq!(std::fs::read_to_string(responder.join("persona.md")).unwrap(), "persona");

        let kinds: Vec<(String, String)> = sqlx::query_as("SELECT kind, payload_json FROM supervisor_inbox WHERE kind='agm_cli_stale'")
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(kinds.len(), 1, "{kinds:?}");
        assert!(kinds[0].1.contains("\"role\":\"responder\""), "{}", kinds[0].1);
        let after = roles::get(&app.db, Role::Responder).await.unwrap();
        assert_eq!((after.identity, after.model, after.effort), (before.identity, before.model, before.effort));
    }

    /// #971：POST 的回應要講清楚每個角色這次怎麼了；失敗的原因不能只留在 log。
    #[tokio::test]
    async fn refresh_reports_each_roles_outcome_and_the_failure_reason() {
        let e = crate::testing::env().await;
        let app = &e.app;
        let patrol = with_bin("OLD");
        // 協調者的 bin/agm 是目錄：讀檔會失敗（不靠 chmod，root 也擋得住）。
        let responder = with_bin("OLD");
        std::fs::remove_file(responder.join("bin/agm")).unwrap();
        std::fs::create_dir_all(responder.join("bin/agm")).unwrap();
        store::get_or_init(&app.db).await.unwrap();
        store::set_env(&app.db, "patrol-bot", &e.project_id, &patrol.to_string_lossy()).await.unwrap();
        roles::set_env(&app.db, Role::Responder, "responder-bot", &e.project_id, &responder.to_string_lossy()).await.unwrap();

        let r = refresh_with(app, "NEW").await;
        let by = |role: &str| r.iter().find(|x| x["role"] == role).unwrap().clone();
        let p = by("patrol");
        assert_eq!(p["outcome"], "refreshed", "{r:?}");
        assert!(p.get("error").is_none(), "{r:?}");
        let c = by("responder");
        assert_eq!(c["outcome"], "failed", "{r:?}");
        assert!(c["error"].as_str().is_some_and(|s| !s.is_empty()), "{r:?}");

        // 再跑一次：巡檢已經是新版 → unchanged；協調者還是壞的 → 仍 failed。
        let r = refresh_with(app, "NEW").await;
        let by = |role: &str| r.iter().find(|x| x["role"] == role).unwrap().clone();
        assert_eq!(by("patrol")["outcome"], "unchanged", "{r:?}");
        assert_eq!(by("responder")["outcome"], "failed", "{r:?}");
    }
}
