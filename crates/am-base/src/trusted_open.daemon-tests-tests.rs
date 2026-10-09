
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let base = crate::testing::track(std::env::temp_dir().join(format!("am-trusted-open-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn opens_a_plain_file_under_nested_directories() {
        let base = scratch("plain");
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::write(base.join("a/b/c.txt"), b"hi").unwrap();
        let mut f = open_bound_file(&base, &[OsStr::new("a"), OsStr::new("b"), OsStr::new("c.txt")], None).unwrap();
        let mut got = String::new();
        std::io::Read::read_to_string(&mut f, &mut got).unwrap();
        assert_eq!(got, "hi");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_local_bounded_read_stops_at_limit_plus_one_after_append() {
        let base = scratch("bounded-read-race");
        let path = base.join("growing.txt");
        std::fs::write(&path, b"a").unwrap();
        let file = File::open(&path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 1);

        let mut append = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut append, b"bcdefgh").unwrap();

        match read_limited(file, 4) {
            Err(BoundedReadError::TooLarge { observed }) => assert_eq!(observed, 5),
            Err(BoundedReadError::Io) => panic!("bounded read failed"),
            Ok(bytes) => panic!("growing file unexpectedly fit the limit: {} bytes", bytes.len()),
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 最後一段是符號連結指到界線外：`O_NOFOLLOW` 直接失敗，不會先「查到是連結」才決定不讀——查跟開是
    /// 同一個系統呼叫。
    /// 硬連結：`O_NOFOLLOW` 擋不住（它就是一般檔案）。把別處的檔案（`ui-token`、金鑰）`ln` 進 outbox／附件目錄、
    /// 改個無害的名字，名字黑名單就看不出來。這些目錄裡的檔案是「搬進來」的，連結數 > 1 一律不開。
    #[test]
    fn a_hard_linked_file_is_refused() {
        let dir = crate::testing::scratch_dir("am-hardlink");
        std::fs::write(dir.join("plain.txt"), b"ok").unwrap();
        std::fs::write(dir.join("secret"), b"token").unwrap();
        std::fs::hard_link(dir.join("secret"), dir.join("notes.txt")).unwrap();
        let handle = std::fs::File::open(&dir).unwrap();
        assert!(open_entry_in(&handle, OsStr::new("plain.txt")).is_ok());
        assert!(open_entry_in(&handle, OsStr::new("notes.txt")).is_err(), "硬連結不能當成 outbox／附件的檔案");
        assert!(open_entry_in(&handle, OsStr::new("secret")).is_err(), "另一端也是（連結數 2）");
    }

    #[test]
    fn a_directory_entry_name_cannot_escape_its_open_directory() {
        let base = scratch("entry-traversal");
        std::fs::create_dir_all(base.join("root")).unwrap();
        std::fs::write(base.join("secret.txt"), "outside").unwrap();
        let dir = File::open(base.join("root")).unwrap();
        assert!(open_entry_in(&dir, OsStr::new("../secret.txt")).is_err());
        assert!(write_new_file_in(&dir, OsStr::new("../escaped.txt"), b"no").is_err());
        assert!(create_new_file_in(&dir, OsStr::new("../escaped.txt"), 0o600).is_err());
        assert!(link_in(&dir, OsStr::new("secret.txt"), OsStr::new("../escaped.txt")).is_err());
        assert!(!base.join("escaped.txt").exists());
    }

    #[test]
    fn cleanup_unlinks_a_symlink_without_visiting_its_target() {
        let base = scratch("cleanup-link");
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();
        let target = base.join("outside");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("secret"), "keep").unwrap();
        std::os::unix::fs::symlink(&target, root.join("link")).unwrap();
        let dir = File::open(&root).unwrap();
        remove_tree_in(&dir, OsStr::new("link")).unwrap();
        assert!(!root.join("link").exists());
        assert_eq!(std::fs::read_to_string(target.join("secret")).unwrap(), "keep");
    }

    #[test]
    fn a_symlinked_final_component_is_refused() {
        let base = scratch("final-link");
        std::fs::write(base.join("outside.txt"), b"secret").unwrap();
        std::fs::create_dir_all(base.join("root")).unwrap();
        std::os::unix::fs::symlink(base.join("outside.txt"), base.join("root/link.txt")).unwrap();
        assert!(open_bound_file(&base.join("root"), &[OsStr::new("link.txt")], None).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 中間那一段目錄被換成符號連結：不能只在最後一段擋，逐層都要 `O_NOFOLLOW`。
    #[test]
    fn a_symlinked_intermediate_directory_is_refused() {
        let base = scratch("mid-link");
        std::fs::create_dir_all(base.join("root")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        std::fs::write(base.join("elsewhere/secret.txt"), b"nope").unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), base.join("root/sub")).unwrap();
        assert!(open_bound_file(&base.join("root"), &[OsStr::new("sub"), OsStr::new("secret.txt")], None).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 打開之後、我們讀它之前，名字被整段換掉（unlink + 重建同名）也不影響已經拿到手的那個 fd——fd
    /// 綁的是 inode，不是名字，這正是「先開再驗」要達成的效果。
    #[test]
    fn a_name_swapped_after_open_does_not_affect_the_already_opened_fd() {
        let base = scratch("swap-after-open");
        std::fs::write(base.join("report.txt"), b"real content").unwrap();
        let mut f = open_bound_file(&base, &[OsStr::new("report.txt")], None).unwrap();

        std::fs::remove_file(base.join("report.txt")).unwrap();
        std::fs::write(base.join("elsewhere.txt"), b"attacker content").unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere.txt"), base.join("report.txt")).unwrap();

        let mut got = String::new();
        std::io::Read::read_to_string(&mut f, &mut got).unwrap();
        assert_eq!(got, "real content", "已經打開的 fd 不該被之後的替換影響");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn owner_mismatch_is_refused_and_a_missing_directory_is_just_not_found() {
        let base = scratch("owner");
        std::fs::create_dir_all(base.join("outbox/kid")).unwrap();
        let me = std::fs::metadata(&base).unwrap().uid();
        assert!(open_bound_dir(&base, &[OsStr::new("outbox"), OsStr::new("kid")], Some(me)).is_ok());
        assert!(open_bound_dir(&base, &[OsStr::new("outbox"), OsStr::new("kid")], Some(me + 1)).is_err());
        assert_eq!(
            open_bound_dir(&base, &[OsStr::new("outbox"), OsStr::new("missing")], Some(me)).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_directory_is_not_a_regular_file() {
        let base = scratch("dir-as-file");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        assert!(open_bound_file(&base, &[OsStr::new("sub")], None).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn names_of(entries: &[BoundEntry]) -> Vec<String> {
        let mut v: Vec<String> = entries.iter().map(|e| e.name.to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    /// 一般檔案、目錄、指到界線外的符號連結混在一起：只有一般檔案 `is_file=true`，目錄與符號連結
    /// 都要列出來（呼叫端自己決定要不要顯示），但 `is_file` 要分得出來——outbox 的 `scan()` 靠這個
    /// 欄位把符號連結／子目錄濾掉。
    #[test]
    fn lists_regular_files_directories_and_symlinks_with_correct_kind() {
        let base = scratch("list-kinds");
        std::fs::create_dir_all(base.join("root/sub")).unwrap();
        std::fs::write(base.join("root/a.txt"), b"hello").unwrap();
        std::fs::write(base.join("outside.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(base.join("outside.txt"), base.join("root/link.txt")).unwrap();
        let dir = open_bound_dir(&base, &[OsStr::new("root")], None).unwrap();

        let entries = read_dir_bound(&dir).unwrap();
        assert_eq!(names_of(&entries), vec!["a.txt", "link.txt", "sub"]);
        let file = entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert!(file.is_file);
        assert_eq!(file.size, 5);
        assert!(file.modified > SystemTime::UNIX_EPOCH);
        let link = entries.iter().find(|e| e.name == "link.txt").unwrap();
        assert!(!link.is_file, "符號連結不是一般檔案，就算它指到的是一般檔案");
        let sub = entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(!sub.is_file);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// issue #96：先前只有「這個目錄可不可信」的檢查是 fd-bound，真正列舉那一步仍是路徑
    /// `read_dir`——檢查通過之後、列舉之前，這顆 bot 自己可以把整個目錄換成指到界線外的符號連結，
    /// 讓清單改列出界線外的檔名／大小（讀不到內容，但檔名本身就外洩了）。這裡重現那個窗口：先用
    /// `open_bound_dir`（模擬「檢查通過」那一刻）拿到 fd，然後把 `root` 這個名字換掉（原本那個目錄
    /// 整個改名挪走，`root` 這個名字改指到界線外——原本的目錄本身、裡面的 `a.txt` 完全沒被動過，
    /// 只是換了個名字掛著），確認 `read_dir_bound` 讀到的還是原本那個 fd 綁的內容，不會跟著
    /// 「`root` 現在指到哪裡」這個新狀態走。
    #[test]
    fn listing_follows_the_already_opened_fd_not_the_path_swapped_afterward() {
        let base = scratch("list-swap");
        std::fs::create_dir_all(base.join("root")).unwrap();
        std::fs::write(base.join("root/a.txt"), b"hi").unwrap();
        let dir = open_bound_dir(&base, &[OsStr::new("root")], None).unwrap();

        // 檢查通過之後、真正列舉之前：原本的目錄改名挪到旁邊（內容原封不動），`root` 這個名字
        // 讓給一個指到界線外的符號連結。
        std::fs::rename(base.join("root"), base.join("root-moved-aside")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        std::fs::write(base.join("elsewhere/secret.txt"), b"nope").unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), base.join("root")).unwrap();

        let entries = read_dir_bound(&dir).unwrap();
        assert_eq!(names_of(&entries), vec!["a.txt"], "拿到的是原本那個 fd 綁的目錄，不是換過去的 elsewhere");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// 讀完一輪之後 fd 還能再讀一次（沒有被 `fdopendir` 弄壞、也沒有把呼叫端的 fd 關掉）。
    #[test]
    fn the_directory_fd_stays_usable_after_listing() {
        let base = scratch("list-reuse");
        std::fs::create_dir_all(base.join("root")).unwrap();
        std::fs::write(base.join("root/a.txt"), b"hi").unwrap();
        let dir = open_bound_dir(&base, &[OsStr::new("root")], None).unwrap();
        assert_eq!(names_of(&read_dir_bound(&dir).unwrap()), vec!["a.txt"]);
        std::fs::write(base.join("root/b.txt"), b"there").unwrap();
        assert_eq!(names_of(&read_dir_bound(&dir).unwrap()), vec!["a.txt", "b.txt"], "同一個 fd 可以再列一次，看得到後來新增的檔案");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn directory_listing_can_stop_without_collecting_the_rest() {
        let base = scratch("bounded-dir-walk");
        for name in ["one", "two", "three"] {
            std::fs::write(base.join(name), b"x").unwrap();
        }
        let dir = File::open(&base).unwrap();
        let mut visited = 0;
        let complete = read_dir_bound_while(&dir, |_| {
            visited += 1;
            visited < 2
        })
        .unwrap();
        assert!(!complete, "consumer requested an early stop");
        assert_eq!(visited, 2, "entries after the limit were not materialized");
    }

    /// `open_entry_in` 只認已經打開的目錄 fd 底下那一個項目：一般檔案放行，符號連結（就算指到界線外的
    /// 一般檔案）跟目錄都拒絕，不會重新用路徑解析 `dir` 本身。
    #[test]
    fn macos_local_open_entry_in_only_opens_a_regular_file_directly_under_the_given_fd() {
        let base = scratch("entry-in");
        std::fs::create_dir_all(base.join("root/sub")).unwrap();
        std::fs::write(base.join("root/a.txt"), b"hi").unwrap();
        std::fs::write(base.join("outside.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(base.join("outside.txt"), base.join("root/link.txt")).unwrap();
        let dir = open_bound_dir(&base, &[OsStr::new("root")], None).unwrap();

        let mut f = open_entry_in(&dir, OsStr::new("a.txt")).unwrap();
        let mut got = String::new();
        std::io::Read::read_to_string(&mut f, &mut got).unwrap();
        assert_eq!(got, "hi");
        assert_eq!(unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK, 0, "一般檔案回傳前要清掉 O_NONBLOCK");
        assert!(open_entry_in(&dir, OsStr::new("link.txt")).is_err(), "符號連結不能開");
        assert!(open_entry_in(&dir, OsStr::new("sub")).is_err(), "目錄不是一般檔案");
        assert!(open_entry_in(&dir, OsStr::new("missing.txt")).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn macos_local_fifo_final_component_does_not_block() {
        const CHILD_ROOT: &str = "AM_TRUSTED_OPEN_FIFO_TEST_ROOT";
        const CHILD_MODE: &str = "AM_TRUSTED_OPEN_FIFO_TEST_MODE";

        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = Path::new(&root);
            let dir = open_bound_dir(root, &[OsStr::new("root")], None).unwrap();
            assert_eq!(std::env::var(CHILD_MODE).unwrap(), "replace");
            let result = open_entry_in_with(&dir, OsStr::new("race.png"), |dir, name, flags| {
                std::fs::remove_file(root.join("root/race.png")).unwrap();
                let fifo = CString::new(root.join("root/race.png").as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "replace with FIFO: {}", io::Error::last_os_error());
                openat_raw(dir.as_raw_fd(), name, flags)
            });
            assert!(result.is_err(), "a regular file replaced by a FIFO must be rejected after open");
            return;
        }

        use std::os::unix::ffi::OsStrExt as _;
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        let base = scratch("fifo-no-block");
        std::fs::create_dir_all(base.join("root")).unwrap();
        let fifo = CString::new(base.join("root/image.png").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "create FIFO: {}", io::Error::last_os_error());
        let dir = open_bound_dir(&base, &[OsStr::new("root")], None).unwrap();
        let mut open_called = false;
        let result = open_entry_in_with(&dir, OsStr::new("image.png"), |dir, name, flags| {
            open_called = true;
            openat_raw(dir.as_raw_fd(), name, flags)
        });
        assert!(result.is_err(), "FIFO is not a regular file");
        assert!(!open_called, "reject non-regular entries before calling openat");
        std::fs::write(base.join("root/race.png"), b"regular").unwrap();

        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("macos_local_fifo_final_component_does_not_block")
            .env(CHILD_ROOT, &base)
            .env(CHILD_MODE, "replace")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };

        std::fs::remove_dir_all(&base).unwrap();
        assert!(status.is_some(), "opening a FIFO after the type check must return promptly");
        assert!(status.unwrap().success(), "the replaced FIFO must be rejected as a non-regular file");
    }

    #[test]
    fn safe_relative_components_rejects_traversal_and_absolute_paths() {
        assert!(safe_relative_components(Path::new("a/b.txt")).is_some());
        assert!(safe_relative_components(Path::new("../outside.txt")).is_none());
        assert!(safe_relative_components(Path::new("a/../b.txt")).is_none());
        assert!(safe_relative_components(Path::new("/etc/passwd")).is_none());
        assert!(safe_relative_components(Path::new("")).is_none());
        assert!(safe_relative_components(Path::new(".")).is_none());
    }

    #[test]
    fn rename_noreplace_atomically_moves_and_rejects_existing() {
        use std::os::unix::fs::MetadataExt as _;
        let base = scratch("rename-noreplace");
        let dir = File::open(&base).unwrap();

        std::fs::write(base.join("src.txt"), b"source").unwrap();
        rename_noreplace_in(&dir, OsStr::new("src.txt"), OsStr::new("dst.txt")).unwrap();
        assert!(!base.join("src.txt").exists());
        assert_eq!(std::fs::read_to_string(base.join("dst.txt")).unwrap(), "source");
        assert_eq!(std::fs::metadata(base.join("dst.txt")).unwrap().nlink(), 1);

        std::fs::write(base.join("src2.txt"), b"source2").unwrap();
        let err = rename_noreplace_in(&dir, OsStr::new("src2.txt"), OsStr::new("dst.txt")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(base.join("src2.txt")).unwrap(), "source2");
        assert_eq!(std::fs::read_to_string(base.join("dst.txt")).unwrap(), "source");
        assert_eq!(std::fs::metadata(base.join("dst.txt")).unwrap().nlink(), 1);
        assert_eq!(std::fs::metadata(base.join("src2.txt")).unwrap().nlink(), 1);

        std::fs::remove_dir_all(&base).unwrap();
    }
