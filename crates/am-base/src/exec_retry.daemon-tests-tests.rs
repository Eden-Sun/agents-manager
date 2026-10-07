
    use super::*;
    use std::cell::Cell;

    fn busy() -> io::Error {
        io::Error::from_raw_os_error(libc::ETXTBSY)
    }

    #[test]
    fn a_busy_text_file_is_retried_until_it_is_free() {
        let calls = Cell::new(0);
        let out = retry(LIMIT, || {
            calls.set(calls.get() + 1);
            if calls.get() < 4 { Err(busy()) } else { Ok("ran") }
        });
        assert_eq!(out.unwrap(), "ran");
        assert_eq!(calls.get(), 4, "前三次 ETXTBSY、第四次成功");
    }

    #[test]
    fn any_other_error_is_returned_at_once() {
        let calls = Cell::new(0);
        let err = retry(LIMIT, || -> io::Result<()> {
            calls.set(calls.get() + 1);
            Err(io::Error::from(io::ErrorKind::NotFound))
        })
        .unwrap_err();
        assert_eq!((err.kind(), calls.get()), (io::ErrorKind::NotFound, 1), "不存在不是 ETXTBSY：不重試");
    }

    #[test]
    fn a_file_that_stays_busy_past_the_limit_hands_the_error_back() {
        let calls = Cell::new(0);
        let err = retry(Duration::from_millis(20), || -> io::Result<()> {
            calls.set(calls.get() + 1);
            Err(busy())
        })
        .unwrap_err();
        assert!(is_text_busy(&err));
        assert!(calls.get() > 1, "到期之前有重試過");
    }

    /// 真的 `exec`：腳本剛寫好、寫入 fd 還開著（模擬別的執行緒 fork 時繼承的那個）時 exec 會回 ETXTBSY，
    /// 放掉之後同一個 `output` 呼叫就成功了。
    #[test]
    fn output_waits_for_a_script_whose_write_fd_is_still_open() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::testing::scratch_dir("am-etxtbsy");
        let path = dir.join("stub.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // 寫入 fd 還開著：直接 exec 會是 ETXTBSY（前提）。
        assert!(is_text_busy(&std::process::Command::new(&path).output().unwrap_err()), "前提：寫入 fd 開著時 exec 回 ETXTBSY");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(f);
        });
        let out = output(&mut std::process::Command::new(&path)).unwrap();
        releaser.join().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }
