
    use super::*;

    fn tmp() -> PathBuf {
        let d = crate::testing::track(std::env::temp_dir().join(format!("am-shim-refresh-{}", crate::db::ulid())));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed(data_dir: &Path, bot: &str, body: &str) -> PathBuf {
        let bin = data_dir.join("bots").join(bot).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["herdr", "cargo"] {
            std::fs::write(bin.join(name), body).unwrap();
        }
        bin
    }

    #[cfg(unix)]
    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    fn set_mode(p: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// 內容就是這顆 binary 帶的版本、權限也是 0755——一顆健康的 shim。
    fn seed_current(data_dir: &Path, bot: &str) -> PathBuf {
        let bin = data_dir.join("bots").join(bot).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, content) in shims() {
            std::fs::write(bin.join(name), content).unwrap();
            #[cfg(unix)]
            set_mode(&bin.join(name), 0o755);
        }
        bin
    }

    /// 2026-09-18：新版上線了，長跑 bot 手上還是舊 shim（沒有 `AM_SHIM_MARKER`），照樣巢狀死鎖。
    /// 開機掃描要把它換掉，不必等那顆 bot 重啟 pane。
    #[test]
    fn an_old_shim_is_replaced_without_touching_the_pane() {
        let data = tmp();
        let bin = seed(&data, "01OLD", "#!/bin/sh\n# 舊版，沒有 marker\nexec cargo \"$@\"\n");
        let changed = refresh_all(&data).changed;
        assert_eq!(changed.len(), 2, "{changed:?}");
        assert_eq!(std::fs::read_to_string(bin.join("cargo")).unwrap(), crate::cargo_shim::SHIM_SH);
        assert_eq!(std::fs::read_to_string(bin.join("herdr")).unwrap(), crate::herdr_shim::SHIM_SH);
        assert!(std::fs::read_to_string(bin.join("cargo")).unwrap().contains("AM_SHIM_MARKER"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(bin.join("cargo")).unwrap().permissions().mode() & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 內容一樣、權限也對：真的什麼都不做——mtime 不動，連 chmod 都不呼叫（ctime 也不動）。
    /// 每次重啟都重寫會把 mtime 洗掉，之後沒人分得出哪些 shim 真的換過版。
    #[cfg(unix)]
    #[test]
    fn a_shim_that_is_already_current_is_left_alone() {
        let data = tmp();
        let bin = seed_current(&data, "01NEW");
        let stat = |n: &str| {
            use std::os::unix::fs::MetadataExt as _;
            let m = std::fs::metadata(bin.join(n)).unwrap();
            (m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec(), m.ino())
        };
        let before = (stat("cargo"), stat("herdr"));

        assert!(refresh_all(&data).changed.is_empty());
        assert_eq!((stat("cargo"), stat("herdr")), before, "mtime／ctime／inode 都不能動");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// issue #126：內容已經是現行版、但**權限掉了**（舊版 `install_local` 寫完才 chmod，中間死掉就留下 0644）。
    /// 只比內容會當成「什麼都不用做」，pane 下一次打 `cargo` 就 permission denied——所以要把權限修回 0755，
    /// 而且**只 chmod**：內容沒變，不重寫（mtime、inode 都不動）。cargo／herdr 兩支都要。
    #[cfg(unix)]
    #[test]
    fn a_current_shim_that_lost_its_exec_bit_is_made_executable_again() {
        use std::os::unix::fs::MetadataExt as _;
        for broken in [0o644, 0o600, 0o664, 0o444] {
            let data = tmp();
            let bin = seed_current(&data, "01MODE");
            for (name, _) in shims() {
                set_mode(&bin.join(name), broken);
            }
            let before: Vec<_> = shims()
                .iter()
                .map(|(n, _)| {
                    let m = std::fs::metadata(bin.join(n)).unwrap();
                    (m.mtime(), m.mtime_nsec(), m.ino())
                })
                .collect();

            let mut changed = refresh_all(&data).changed;
            changed.sort();
            assert_eq!(changed, vec!["01MODE/cargo".to_string(), "01MODE/herdr".to_string()], "{broken:o} 要算修過：{changed:?}");
            for (i, (name, content)) in shims().iter().enumerate() {
                let p = bin.join(name);
                assert_eq!(mode_of(&p), 0o755, "{name} 從 {broken:o} 要修回 0755");
                assert_eq!(&std::fs::read_to_string(&p).unwrap(), content, "{name} 內容不該被動");
                let m = std::fs::metadata(&p).unwrap();
                assert_eq!((m.mtime(), m.mtime_nsec(), m.ino()), before[i], "{name}：只 chmod，不重寫內容（mtime／inode 不動）");
            }
            // 修完之後再跑就是真的 no-op（冪等）。
            assert!(refresh_all(&data).changed.is_empty());
            let _ = std::fs::remove_dir_all(&data);
        }
    }

    /// 直接呼叫 `write_atomic`（`install_local` 走的那條）：權限不對也要修，回 `Ok(true)`；健康的回 `Ok(false)`。
    #[cfg(unix)]
    #[test]
    fn write_atomic_reports_a_mode_repair_and_nothing_for_a_healthy_file() {
        let data = tmp();
        let p = data.join("cargo");
        assert!(write_atomic(&p, "#!/bin/sh\n:\n").unwrap(), "新檔案");
        assert_eq!(mode_of(&p), 0o755);
        assert!(!write_atomic(&p, "#!/bin/sh\n:\n").unwrap(), "健康：什麼都不做");
        set_mode(&p, 0o644);
        assert!(write_atomic(&p, "#!/bin/sh\n:\n").unwrap(), "同內容、掉了權限：修好了");
        assert_eq!(mode_of(&p), 0o755);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// A predictable temp name can already be a symlink; writing it must not truncate its target.
    #[cfg(unix)]
    #[test]
    fn write_atomic_does_not_follow_a_colliding_temporary_symlink() {
        use std::os::unix::fs::symlink;

        let data = tmp();
        let target = data.join("precious-file");
        let path = data.join("herdr");
        let temporary = data.join(format!(".herdr.tmp-{}", std::process::id()));
        std::fs::write(&target, "keep this content\n").unwrap();
        symlink(&target, &temporary).unwrap();

        assert!(write_atomic(&path, "#!/bin/sh\n# fresh shim\n").unwrap());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep this content\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "#!/bin/sh\n# fresh shim\n");
        assert!(!std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 內容不同：仍是暫存檔 + rename，**不是**就地截斷。舊 inode（正在跑那支 shim 的行程手上還開著）
    /// 內容原封不動，只有目錄項換掉；新的一份權限是 0755。
    #[cfg(unix)]
    #[test]
    fn a_different_shim_is_swapped_by_rename_and_the_old_inode_is_untouched() {
        use std::io::Read as _;
        let data = tmp();
        let bin = seed(&data, "01SWAP", "#!/bin/sh\n# 舊版\n");
        let mut old_handle = std::fs::File::open(bin.join("cargo")).unwrap();

        assert_eq!(refresh_all(&data).changed.len(), 2);

        let mut seen_by_old_process = String::new();
        old_handle.read_to_string(&mut seen_by_old_process).unwrap();
        assert_eq!(seen_by_old_process, "#!/bin/sh\n# 舊版\n", "就地截斷會讓正在跑的 shim 讀到新內容或空檔案");
        assert_eq!(std::fs::read_to_string(bin.join("cargo")).unwrap(), crate::cargo_shim::SHIM_SH);
        assert_eq!(mode_of(&bin.join("cargo")), 0o755);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 只補**已經有的**：沒有 bin／沒有那支 shim 的 bot 不生出新檔案，其他檔案也不碰。
    #[test]
    fn nothing_is_created_for_bots_that_never_had_a_shim() {
        let data = tmp();
        let bare = data.join("bots").join("01BARE");
        std::fs::create_dir_all(&bare).unwrap();
        let bin = data.join("bots").join("01HALF").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("cargo"), "#!/bin/sh\n# 舊\n").unwrap();
        std::fs::write(bin.join("something-else"), "keep me").unwrap();

        let changed = refresh_all(&data).changed;
        assert_eq!(changed, vec!["01HALF/cargo".to_string()], "{changed:?}");
        assert!(!bare.join("bin").exists(), "沒有 bin 的 bot 不該被生出目錄");
        assert!(!bin.join("herdr").exists(), "本來就沒有的 shim 不補");
        assert_eq!(std::fs::read_to_string(bin.join("something-else")).unwrap(), "keep me");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 換不動的 shim 要推 inbox：只記 warn 等於沒人知道，而它不會自己好（issue #533）。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_shim_that_cannot_be_replaced_is_reported_to_the_inbox() {
        use std::os::unix::fs::PermissionsExt as _;
        let e = crate::testing::env().await;
        let app = &e.app;
        let bin = app.data_dir.join("bots").join("01STUCK").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("cargo"), "#!/bin/sh\n# 舊\n").unwrap();
        let lock = || std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
        let unlock = || std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        lock();
        crate::runners::shim_refresh::refresh_at_startup(app).await; // 不 panic：開機照走
        unlock();
        assert_eq!(std::fs::read_to_string(bin.join("cargo")).unwrap(), "#!/bin/sh\n# 舊\n", "換不動時舊檔原封不動");

        let rows = |db: sqlx::SqlitePool| async move {
            sqlx::query_as::<_, (String, String)>("SELECT bot_id, payload_json FROM supervisor_inbox WHERE kind=?")
                .bind(SHIM_STALE_KIND)
                .fetch_all(&db)
                .await
                .unwrap()
        };
        let one = rows(app.db.clone()).await;
        assert_eq!(one.len(), 1, "{one:?}");
        assert_eq!(one[0].0, "01STUCK");
        assert!(one[0].1.contains("\"shim\":\"cargo\""), "{}", one[0].1);
        assert!(one[0].1.contains("01STUCK/bin/cargo"), "payload 要帶得出是哪個檔：{}", one[0].1);

        // 同一個版本再失敗一次不是新的一則（event_key 帶內容雜湊）。
        lock();
        crate::runners::shim_refresh::refresh_at_startup(app).await;
        unlock();
        assert_eq!(rows(app.db.clone()).await.len(), 1);

        // 修好之後照樣換得動。
        crate::runners::shim_refresh::refresh_at_startup(app).await;
        assert_eq!(std::fs::read_to_string(bin.join("cargo")).unwrap(), crate::cargo_shim::SHIM_SH);
    }

    /// 換版是 rename：暫存檔不留下來。
    #[test]
    fn the_swap_leaves_no_temporary_file_behind() {
        let data = tmp();
        let bin = seed(&data, "01TMP", "#!/bin/sh\n# 舊\n");
        refresh_all(&data);
        let leftovers: Vec<String> = std::fs::read_dir(&bin)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&data);
    }

    // ───────────── 遠端（issue #124）：腳本直接在本機的假「遠端」目錄上跑，不需要真的 ssh ─────────────

    /// 用 `sh -s` 跑 [`remote_sync_script`]（跟 `HostConn::ssh_exec` 送腳本的方式一樣：走 stdin）。
    #[cfg(unix)]
    fn run_script(script: &str, path: Option<&str>) -> String {
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("sh");
        cmd.arg("-s").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(p) = path {
            cmd.env("PATH", p);
        }
        let mut child = cmd.spawn().unwrap();
        // 收不完（腳本被截斷）時 shell 可能先結束，寫入端會 EPIPE——那正是要模擬的情況，不算錯。
        let _ = child.stdin.take().unwrap().write_all(script.as_bytes());
        String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
    }

    #[cfg(unix)]
    fn run_sync(dirs: &[&Path], names: &[&str], create_missing: bool) -> String {
        let dirs: Vec<String> = dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect();
        run_script(&remote_sync_script(&dirs, names, create_missing), None)
    }

    #[cfg(unix)]
    fn write_shim(path: &Path, body: &str, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        set_mode(path, mode);
    }

    #[cfg(unix)]
    fn touch_old(path: &Path) {
        assert!(std::process::Command::new("touch").args(["-t", "202601010000"]).arg(path).status().unwrap().success());
    }

    /// heredoc 要求內容以換行結尾，不然結尾標記黏在最後一行上、腳本就壞了。
    #[test]
    fn every_shim_ends_with_a_newline() {
        for (name, content) in shims() {
            assert!(content.ends_with('\n'), "{name}");
        }
    }

    /// issue #124：daemon 升級後，長跑的**遠端** bot 手上也還是舊 shim。重連之後盤點這台的 bot，把**已經存在**的
    /// shim 換成這顆 binary 帶的版本：暫存檔＋chmod＋rename（正在跑的舊 shim 沿用舊 inode）、內容一樣不重寫
    /// （不洗 mtime，只確保權限）、沒有 bin／沒有那支 shim 的 bot 不生出新檔案、上次斷線留下的殘留暫存檔掃掉、
    /// 重跑是冪等的。
    #[cfg(unix)]
    #[test]
    fn a_remote_sync_replaces_old_shims_atomically_and_only_where_they_already_exist() {
        use std::io::Read as _;
        let d = tmp();
        let (b1, b2, b3, b4) = (d.join("b1"), d.join("b2"), d.join("b3"), d.join("b4"));
        write_shim(&b1.join("bin/herdr"), "#!/bin/sh\n# 舊版 herdr\n", 0o755); // 舊內容
        write_shim(&b1.join("bin/cargo"), crate::cargo_shim::SHIM_SH, 0o644); // 內容對、權限掉了
        write_shim(&b2.join("bin/herdr"), crate::herdr_shim::SHIM_SH, 0o755); // 健康的現行版
        touch_old(&b2.join("bin/herdr"));
        // b2 沒有 cargo shim（從沒裝過）、b3 只有 bot 目錄沒有 bin、b4 根本不存在。
        std::fs::create_dir_all(&b3).unwrap();
        write_shim(&b1.join("bin/.herdr.tmp-999"), "殘留的半截暫存檔", 0o644);
        touch_old(&b1.join("bin/.herdr.tmp-999"));
        let mtime = |p: &Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let b2_mtime = mtime(&b2.join("bin/herdr"));
        let mut running_old = std::fs::File::open(b1.join("bin/herdr")).unwrap(); // 「正在執行」的舊 shim

        let out = run_sync(&[&b1, &b2, &b3, &b4], &["herdr", "cargo"], false);
        let r = parse_remote_sync(&out).unwrap();

        assert_eq!(r.updated, vec![format!("{} herdr", b1.display())], "只有內容真的不同的那一支：{out}");
        assert!(r.failed.is_empty(), "{out}");
        assert_eq!(std::fs::read_to_string(b1.join("bin/herdr")).unwrap(), crate::herdr_shim::SHIM_SH);
        assert_eq!(mode_of(&b1.join("bin/herdr")), 0o755);
        assert_eq!(mode_of(&b1.join("bin/cargo")), 0o755, "內容對、權限掉了：只 chmod");
        assert_eq!(std::fs::read_to_string(b1.join("bin/cargo")).unwrap(), crate::cargo_shim::SHIM_SH);
        let mut seen_by_old = String::new();
        running_old.read_to_string(&mut seen_by_old).unwrap();
        assert_eq!(seen_by_old, "#!/bin/sh\n# 舊版 herdr\n", "正在跑的舊 shim 沿用舊 inode，不能被就地截斷");
        assert_eq!(mtime(&b2.join("bin/herdr")), b2_mtime, "同內容不重寫：mtime 不能被洗掉");
        assert!(!b2.join("bin/cargo").exists(), "從沒裝過的 shim 不補");
        assert!(!b3.join("bin").exists(), "沒有 bin 的 bot 不生出目錄");
        assert!(!b4.exists(), "不存在的 bot 目錄不建立");
        let leftovers: Vec<String> = std::fs::read_dir(b1.join("bin")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with('.')).collect();
        assert!(leftovers.is_empty(), "殘留的暫存檔要掃掉：{leftovers:?}");

        // 冪等：再跑一次什麼都沒換。
        let again = parse_remote_sync(&run_sync(&[&b1, &b2, &b3, &b4], &["herdr", "cargo"], false)).unwrap();
        assert_eq!(again, RemoteSync::default());
        assert_eq!(mtime(&b2.join("bin/herdr")), b2_mtime);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Remote sync must not let a preexisting predictable temp symlink redirect `cp` into another file.
    #[cfg(unix)]
    #[test]
    fn remote_sync_does_not_follow_a_colliding_temporary_symlink() {
        let d = tmp();
        let bot = d.join("bot");
        let bin = bot.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let target = d.join("precious-file");
        let temporary = bin.join(".herdr.tmp-");
        std::fs::write(&target, "keep this content\n").unwrap();
        write_shim(&bin.join("herdr"), "#!/bin/sh\n# old shim\n", 0o755);

        let dirs = vec![bot.to_string_lossy().into_owned()];
        let script = remote_sync_script(&dirs, &["herdr"], false);
        let setup = format!(
            "tmp={}$$\nln -s {} \"$tmp\" || exit 77\ntest -L \"$tmp\" || exit 77\n{}",
            crate::hosts::sh_quote(&temporary.to_string_lossy()),
            crate::hosts::sh_quote(&target.to_string_lossy()),
            script
        );
        let out = run_script(&setup, None);
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "keep this content\n",
            "remote copy must not follow a temp symlink: {out}"
        );
        assert_eq!(std::fs::read_to_string(bin.join("herdr")).unwrap(), crate::herdr_shim::SHIM_SH);
        assert!(!std::fs::symlink_metadata(bin.join("herdr")).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 本機開機掃描與遠端同步讀的是**同一份**內容來源：同樣的舊狀態，兩邊跑完的檔案位元組與權限完全一樣。
    #[cfg(unix)]
    #[test]
    fn the_remote_sync_and_the_local_refresh_produce_the_same_files() {
        let d = tmp();
        let local = d.join("local");
        let remote = d.join("remote/bot");
        for bin in [local.join("bots/B/bin"), remote.join("bin")] {
            write_shim(&bin.join("herdr"), "#!/bin/sh\n# 舊\n", 0o755);
            write_shim(&bin.join("cargo"), crate::cargo_shim::SHIM_SH, 0o600);
        }
        refresh_all(&local);
        run_sync(&[&remote], &["herdr", "cargo"], false);
        for (name, content) in shims() {
            let l = local.join("bots/B/bin").join(name);
            let r = remote.join("bin").join(name);
            assert_eq!(std::fs::read(&l).unwrap(), std::fs::read(&r).unwrap(), "{name}");
            assert_eq!(std::fs::read_to_string(&r).unwrap(), content, "{name}");
            assert_eq!((mode_of(&l), mode_of(&r)), (0o755, 0o755), "{name}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 複製到一半失敗（磁碟滿、被砍）：舊檔原封不動、沒有留下半截的檔案，而且明說這一支失敗。
    #[cfg(unix)]
    #[test]
    fn a_copy_that_fails_midway_leaves_the_old_shim_untouched_and_no_partial_file() {
        let d = tmp();
        let bot = d.join("bot");
        write_shim(&bot.join("bin/herdr"), "#!/bin/sh\n# 舊版\n", 0o755);
        // 一支會寫出 10 個位元組就死掉的 `cp`，蓋在 PATH 最前面。
        let fake = d.join("fakebin");
        std::fs::create_dir_all(&fake).unwrap();
        write_shim(&fake.join("cp"), "#!/bin/sh\nhead -c 10 \"$1\" > \"$2\"\nexit 1\n", 0o755);

        let script = remote_sync_script(&[bot.to_string_lossy().into_owned()], &["herdr"], false);
        let out = run_script(&script, Some(&format!("{}:/usr/bin:/bin", fake.display())));
        let r = parse_remote_sync(&out).unwrap();

        assert_eq!(r.failed, vec![format!("{} herdr", bot.display())], "{out}");
        assert!(r.updated.is_empty());
        assert_eq!(std::fs::read_to_string(bot.join("bin/herdr")).unwrap(), "#!/bin/sh\n# 舊版\n", "舊檔不動");
        assert_eq!(mode_of(&bot.join("bin/herdr")), 0o755);
        let names: Vec<String> = std::fs::read_dir(bot.join("bin")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["herdr".to_string()], "沒有留下半截的暫存檔：{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// SSH 送到一半斷線：shell 收到的是被截斷的腳本。不管斷在哪——第一支 heredoc 中間、迴圈之前、迴圈中間——
    /// 都不能動任何 shim，也不能留下暫存檔；而且輸出沒有結尾標記，呼叫端不會把它當成功。
    #[cfg(unix)]
    #[test]
    fn a_script_cut_off_by_a_dropped_ssh_connection_changes_nothing() {
        let d = tmp();
        let bot = d.join("bot");
        write_shim(&bot.join("bin/herdr"), "#!/bin/sh\n# 舊版\n", 0o755);
        let script = remote_sync_script(&[bot.to_string_lossy().into_owned()], &["herdr", "cargo"], false);
        let loop_at = script.find("for D in").unwrap();
        let loop_end = loop_at + script[loop_at..].find("\ndone\necho").unwrap();
        let mut cuts = vec![loop_at / 2, loop_at - 5, loop_at + 30, loop_end - 3];
        cuts.iter_mut().for_each(|c| {
            while !script.is_char_boundary(*c) {
                *c -= 1;
            }
        });
        for cut in cuts {
            let out = run_script(&script[..cut], None);
            assert!(parse_remote_sync(&out).is_err(), "沒收完就不是成功（cut={cut}）：{out}");
            assert_eq!(std::fs::read_to_string(bot.join("bin/herdr")).unwrap(), "#!/bin/sh\n# 舊版\n", "cut={cut}");
            let names: Vec<String> = std::fs::read_dir(bot.join("bin")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            assert_eq!(names, vec!["herdr".to_string()], "cut={cut}：{names:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// bot 啟動時的安裝（`create_missing`）才會建目錄、建檔，而且只建被點名的那支；補版（重連）不建。
    #[cfg(unix)]
    #[test]
    fn a_remote_sync_creates_a_missing_shim_only_when_asked_to() {
        let d = tmp();
        let bot = d.join("bot");
        std::fs::create_dir_all(&bot).unwrap();
        run_sync(&[&bot], &["herdr", "cargo"], false);
        assert!(!bot.join("bin").exists(), "補版不生新檔案");

        let r = parse_remote_sync(&run_sync(&[&bot], &["herdr"], true)).unwrap();
        assert_eq!(r.updated, vec![format!("{} herdr", bot.display())]);
        assert_eq!(std::fs::read_to_string(bot.join("bin/herdr")).unwrap(), crate::herdr_shim::SHIM_SH);
        assert_eq!(mode_of(&bot.join("bin/herdr")), 0o755);
        assert!(!bot.join("bin/cargo").exists(), "只裝被點名的那支");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 沒看到結尾標記就不是成功；腳本自己放棄（暫存目錄建不起來、收到的位元組數不對）也是失敗。
    #[test]
    fn a_remote_sync_that_did_not_reach_its_end_is_not_a_success() {
        assert!(parse_remote_sync("").is_err());
        assert!(parse_remote_sync("AM_SHIM_UPDATED /b herdr\n").is_err(), "有換版但沒收尾：不確定，當失敗");
        assert!(parse_remote_sync("AM_SHIM_SYNC_FAILED truncated herdr\n").is_err());
        assert_eq!(parse_remote_sync("AM_SHIM_UPDATED /b herdr\nAM_SHIM_FAILED /c cargo\nAM_SHIM_SYNC_DONE\n").unwrap(), RemoteSync { updated: vec!["/b herdr".into()], failed: vec!["/c cargo".into()] });
    }
