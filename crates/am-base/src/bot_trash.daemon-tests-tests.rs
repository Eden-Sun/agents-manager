
    use super::*;

    #[test]
    fn a_trashed_dir_comes_back_on_restore_and_expires_after_keep() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-{}", crate::db::ulid())));
        let dir = data.join("bots").join("b1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "x").unwrap();

        let moved = move_in(&data, "b1", &dir).unwrap().expect("moved");
        assert!(!dir.exists() && moved.join("keep.txt").exists());
        assert_eq!(restore(&data, "b1", &dir).unwrap(), Some(moved));
        assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "x");

        move_in(&data, "b1", &dir).unwrap().unwrap();
        assert_eq!(gc(&data, std::time::Duration::from_secs(3600)), 0, "還沒過期");
        assert_eq!(gc(&data, std::time::Duration::ZERO), 1);
        assert_eq!(restore(&data, "b1", &dir).unwrap(), None, "過期清掉之後沒得還原");
        std::fs::remove_dir_all(data).unwrap();
    }

    /// `bots/<id>` 若是 symlink，`Path::exists` 會跟著走、把回收區留在原地。還原必須只拆連結，
    /// 把垃圾桶搬回原位，不能把檔案寫進連結指到的目錄。真的目錄仍不覆蓋。
    #[test]
    fn a_symlink_where_the_bot_dir_should_be_is_not_treated_as_a_rebuilt_directory() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-{}", crate::db::ulid())));
        let dir = data.join("bots").join("b1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "secret").unwrap();
        move_in(&data, "b1", &dir).unwrap().expect("moved");

        let outside = data.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.txt"), "keep-me").unwrap();
        std::os::unix::fs::symlink(&outside, &dir).unwrap();

        let restored = restore(&data, "b1", &dir).unwrap();
        assert!(restored.is_some(), "a symlink is not a rebuilt bot directory");
        assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "secret");
        assert_eq!(std::fs::read_to_string(outside.join("victim.txt")).unwrap(), "keep-me");
        assert!(!outside.join("keep.txt").exists(), "trash must not land inside the symlink target");
        assert!(std::fs::symlink_metadata(&dir).unwrap().is_dir());

        move_in(&data, "b1", &dir).unwrap().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fresh.txt"), "new").unwrap();
        assert_eq!(restore(&data, "b1", &dir).unwrap(), None, "a real directory is still not overwritten");
        assert_eq!(std::fs::read_to_string(dir.join("fresh.txt")).unwrap(), "new");
        std::fs::remove_dir_all(data).unwrap();
    }

    /// 在回收區放一份指定大小、指定「搬進來時刻」的目錄。
    fn seed(data: &Path, bot: &str, ms: u128, bytes: usize) -> PathBuf {
        let p = root(data).join(format!("{bot}.{ms}"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("blob"), vec![b'x'; bytes]).unwrap();
        p
    }

    /// review d77434c0 #2：時間上限擋不住「短時間刪掉一堆」。超過總量就從最舊的開始清，
    /// 最新那一份留著（剛刪掉的那顆才是最可能要還原的）。
    #[test]
    fn the_oldest_entries_go_first_once_the_trash_is_over_its_size_cap() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-cap-{}", crate::db::ulid())));
        let now = now_ms();
        let old = seed(&data, "b1", now - 3_000, 4_000);
        let mid = seed(&data, "b2", now - 2_000, 4_000);
        let new = seed(&data, "b3", now - 1_000, 4_000);

        // 沒超量就一個都不動（keep 很長，沒有東西過期）。
        assert_eq!(gc_with_cap(&data, Duration::from_secs(3600), 100_000), (0, 0));
        assert!(old.exists() && mid.exists() && new.exists());

        // 上限只容得下一份：最舊的兩份走，最新那份留著。
        assert_eq!(gc_with_cap(&data, Duration::from_secs(3600), 5_000), (0, 2));
        assert!(!old.exists() && !mid.exists(), "最舊的先清");
        assert!(new.exists(), "最新那一份永遠留著");
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// 過期的先清；清完還超量才輪到按時間淘汰，兩個數字分開回報。
    #[test]
    fn expiry_runs_before_the_size_cap() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-both-{}", crate::db::ulid())));
        let now = now_ms();
        seed(&data, "b1", now - 10_000, 4_000); // 過期
        let mid = seed(&data, "b2", now - 2_000, 4_000);
        let new = seed(&data, "b3", now - 1_000, 4_000);

        assert_eq!(gc_with_cap(&data, Duration::from_millis(5_000), 5_000), (1, 1));
        assert!(!mid.exists() && new.exists());
        assert_eq!(entries(&data).len(), 1);
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// 名字看不懂的（別人放進來的檔案、暫存）一律不碰，免得清理誤傷。
    #[test]
    fn entries_with_unparseable_names_are_never_touched() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-alien-{}", crate::db::ulid())));
        std::fs::create_dir_all(root(&data).join("not-a-trash-entry")).unwrap();
        std::fs::write(root(&data).join("README"), "x").unwrap();

        assert_eq!(gc_with_cap(&data, Duration::ZERO, 0), (0, 0));
        assert!(root(&data).join("not-a-trash-entry").exists() && root(&data).join("README").exists());
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn gc_does_not_follow_a_symlinked_trash_root_outside_the_data_dir() {
        use std::os::unix::fs::symlink;

        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-root-link-{}", crate::db::ulid())));
        let outside = crate::testing::track(std::env::temp_dir().join(format!("am-trash-outside-{}", crate::db::ulid())));
        let victim = outside.join("b1.1");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), "outside data").unwrap();
        std::fs::create_dir_all(&data).unwrap();
        symlink(&outside, root(&data)).unwrap();

        gc_with_cap(&data, Duration::ZERO, 0);

        assert!(victim.join("keep.txt").exists(), "GC must never traverse the trash root symlink");
    }

    /// #465：附件副本要跟 `bots/<id>/` 一起進回收區、受同一套過期與總量上限，還原時一起搬回來，
    /// 而且 `<id>.attachments.<ms>` 不能被當成 bot 目錄還原到 `bots/<id>/`。
    #[test]
    fn attachments_ride_the_same_trash_lifecycle_without_being_mistaken_for_the_bot_dir() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-att-{}", crate::db::ulid())));
        let bots = data.join("bots/B1");
        let att = attachments_dir(&data, "B1");
        std::fs::create_dir_all(&bots).unwrap();
        std::fs::create_dir_all(&att).unwrap();
        std::fs::write(bots.join("config"), "bot").unwrap();
        std::fs::write(att.join("a.png"), "img").unwrap();

        assert!(move_in(&data, "B1", &bots).unwrap().is_some());
        assert!(move_in_kind(&data, "B1", Some(ATTACHMENTS), &att).unwrap().is_some());
        assert!(!bots.exists() && !att.exists(), "兩個都搬走了");
        assert_eq!(entries(&data).len(), 2, "兩份都要被 GC 看得到（過期與總量上限一視同仁）");

        // 還原 bot 目錄時不能撈到 attachments 那份。
        restore(&data, "B1", &bots).unwrap();
        assert_eq!(std::fs::read_to_string(bots.join("config")).unwrap(), "bot");
        restore_kind(&data, "B1", Some(ATTACHMENTS), &att).unwrap();
        assert_eq!(std::fs::read_to_string(att.join("a.png")).unwrap(), "img");
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// 過期清理會把附件那份也收掉（以前 `attachments/<id>/` 永遠不會被任何人清）。
    #[test]
    fn expired_attachment_entries_are_collected_like_any_other() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-attgc-{}", crate::db::ulid())));
        let att = attachments_dir(&data, "B1");
        std::fs::create_dir_all(&att).unwrap();
        std::fs::write(att.join("a.png"), vec![b'x'; 32]).unwrap();
        let moved = move_in_kind(&data, "B1", Some(ATTACHMENTS), &att).unwrap().unwrap();
        assert!(moved.exists());
        let (expired, _) = gc_with_cap(&data, Duration::ZERO, u64::MAX);
        assert_eq!(expired, 1, "附件那份也要被過期清理收掉");
        assert!(!moved.exists());
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// **#513**：清理與還原不互斥時，`remove_dir_all` 是「先把裡面 unlink 光、最後才刪目錄」，
    /// 清到一半的那一份 `latest` 還看得到、還原的 `rename` 還會成功——使用者拿回一個正在被清空的目錄，
    /// 而還原回的是 `Ok(Some(..))`。現在清理的第一步是把它改名成 `*.deleting`：改名成功的那一刻起
    /// 回收區就看不到它，還原撈不到（`Ok(None)`），不會拿到一份注定被清空的目錄。
    #[test]
    fn an_entry_being_removed_leaves_the_trash_namespace_before_a_single_file_is_deleted() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-race-{}", crate::db::ulid())));
        let now = now_ms();
        let entry = seed(&data, "B1", now - 1_000, 32);
        assert_eq!(latest(&data, "B1", None).as_ref(), Some(&entry), "前提：還原撈得到它");

        // 清理的第一步（改名）做完、remove_dir_all 還沒跑：裡面的檔一個都還在。
        let root_dir = open_root(&data).unwrap();
        let staged = root(&data).join(take_aside(&root_dir, entry.file_name().unwrap()).expect("gc 拿到了這一份"));
        assert!(staged.join("blob").exists(), "還沒刪任何東西");
        assert_eq!(entries(&data).len(), 0, "回收區的帳看不到正在清的那一份");
        assert_eq!(latest(&data, "B1", None), None, "還原也撈不到");

        let dir = data.join("bots/B1");
        assert_eq!(restore(&data, "B1", &dir).unwrap(), None, "撈不到就是沒得還原，不會回 Ok(Some) 給半條命的目錄");
        assert!(!dir.exists());
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// 反過來：還原先搶到那一份，清理的改名就 `NotFound`——一個檔都不准動，還原回來的目錄完整。
    #[test]
    fn a_restore_that_wins_the_race_keeps_every_file_and_the_gc_removes_nothing() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-race2-{}", crate::db::ulid())));
        let now = now_ms();
        let entry = seed(&data, "B1", now - 1_000, 32);
        let dir = data.join("bots/B1");

        assert_eq!(restore(&data, "B1", &dir).unwrap(), Some(entry.clone()), "還原先到");
        let root_dir = open_root(&data).unwrap();
        assert!(!remove(&root_dir, entry.file_name().unwrap()), "清理沒拿到那一份");
        assert_eq!(std::fs::read(dir.join("blob")).unwrap().len(), 32, "還原回來的目錄一個位元組都沒少");
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// 上一輪在改名與 remove_dir_all 之間被砍掉留下的 `*.deleting`：兩道 gc 都看不到它，
    /// 沒人收就永遠佔著磁碟。每輪開頭收一次，而且不算進任何一個計數。
    #[test]
    fn leftover_deleting_entries_from_a_crashed_sweep_are_collected_on_the_next_gc() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-leftover-{}", crate::db::ulid())));
        let now = now_ms();
        let entry = seed(&data, "B1", now - 1_000, 16);
        let root_dir = open_root(&data).unwrap();
        let leftover = root(&data).join(take_aside(&root_dir, entry.file_name().unwrap()).expect("改名成功"));
        assert!(leftover.exists() && entries(&data).is_empty(), "前提：留下一份誰都看不到的殘骸");

        let keep = seed(&data, "B2", now - 500, 16);
        assert_eq!(gc_with_cap(&data, Duration::from_secs(3600), u64::MAX), (0, 0), "殘骸不算過期、也不算超量淘汰");
        assert!(!leftover.exists(), "殘骸收掉了");
        assert!(keep.exists(), "沒過期的照樣留著");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn dir_size_adds_up_nested_files() {
        let data = crate::testing::track(std::env::temp_dir().join(format!("am-trash-size-{}", crate::db::ulid())));
        let d = data.join("x");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/one"), vec![b'x'; 10]).unwrap();
        std::fs::write(d.join("a/b/two"), vec![b'x'; 5]).unwrap();
        assert_eq!(dir_size(&d), 15);
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn share_workspace_can_be_moved_to_trash_restored_and_counts_toward_cap() {
        let base = crate::testing::scratch_dir("am-trash-ws");
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let ws_parent = base.join("shared-bots");
        let ws = ws_parent.join("b1");
        std::fs::create_dir_all(ws.join("inbox")).unwrap();
        std::fs::write(ws.join("inbox/test.txt"), vec![b'a'; 100]).unwrap();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub/doc.txt"), vec![b'b'; 200]).unwrap();

        // 搬進 trash
        let moved = move_in_kind(&data, "b1", Some(SHARE_WORKSPACE), &ws).unwrap().expect("moved to trash");
        assert!(!ws.exists(), "original workspace is no longer live");
        assert!(moved.join("inbox/test.txt").exists());
        assert!(moved.join("sub/doc.txt").exists());

        // 計算大小
        let trash_entries = entries(&data);
        assert_eq!(trash_entries.len(), 1);
        assert_eq!(trash_entries[0].2, 300, "counts share workspace bytes");

        // 還原
        let restored = restore_kind(&data, "b1", Some(SHARE_WORKSPACE), &ws).unwrap().expect("restored");
        assert_eq!(restored, moved);
        assert!(ws.join("inbox/test.txt").exists());
        assert!(ws.join("sub/doc.txt").exists());
        assert_eq!(entries(&data).len(), 0);

        // 再次刪除 + GC 淘汰
        let moved2 = move_in_kind(&data, "b1", Some(SHARE_WORKSPACE), &ws).unwrap().expect("moved again");
        assert!(!ws.exists());
        assert_eq!(entries(&data).len(), 1);

        // GC with zero keep
        let (expired, _) = gc_with_cap(&data, Duration::ZERO, u64::MAX);
        assert_eq!(expired, 1);
        assert!(!moved2.exists());
        assert_eq!(entries(&data).len(), 0);

        // 過期後 restore 回傳 None
        assert_eq!(restore_kind(&data, "b1", Some(SHARE_WORKSPACE), &ws).unwrap(), None);
        std::fs::remove_dir_all(&base).unwrap();
    }
