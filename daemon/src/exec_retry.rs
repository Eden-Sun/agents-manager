//! `ETXTBSY`（Text file busy）重試（issue #189）。
//!
//! 行程剛寫完的可執行檔立刻 `exec`，可能撞上 `ETXTBSY`：同一個行程裡別的執行緒在這個檔案開著寫入的那一刻 `fork`，
//! 子行程短暫繼承了寫入 fd，直到它自己 `exec` 為止；這段時間內 exec 那個檔案，kernel 都回 `ETXTBSY`。
//! 這不是被執行的程式的問題，過一下就好，所以只在**這個錯誤**時重試——等的是條件，不是固定睡一段時間，
//! 其他任何錯誤（不存在、沒有執行權限…）原樣回傳。
//!
//! 正式碼（daemon 自己複製／寫完 binary 後去驗它）與測試碼（[`crate::testing::write_exec`] 寫假腳本）共用這一份。

use std::io;
use std::time::{Duration, Instant};

/// 重試的總上限：fork 到 exec 之間是毫秒級，30 秒表示出了別的事，這時把 `ETXTBSY` 原樣交出去。
pub const LIMIT: Duration = Duration::from_secs(30);
const STEP: Duration = Duration::from_millis(2);

pub fn is_text_busy(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ETXTBSY)
}

/// 同步版：`f` 回 `ETXTBSY` 就重試到 `limit`，其他結果（成功或別的錯誤）直接回。
pub fn retry<T>(limit: Duration, mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let started = Instant::now();
    loop {
        match f() {
            Err(e) if is_text_busy(&e) && started.elapsed() < limit => std::thread::sleep(STEP),
            r => return r,
        }
    }
}

/// `std::process::Command::output`，遇到 `ETXTBSY` 重試。
pub fn output(cmd: &mut std::process::Command) -> io::Result<std::process::Output> {
    retry(LIMIT, || cmd.output())
}

/// `std::process::Command::spawn`，遇到 `ETXTBSY` 重試。
pub fn spawn(cmd: &mut std::process::Command) -> io::Result<std::process::Child> {
    retry(LIMIT, || cmd.spawn())
}

/// tokio 版的 `output`：睡的是 `tokio::time::sleep`，不佔住執行緒。
pub async fn output_async(cmd: &mut tokio::process::Command) -> io::Result<std::process::Output> {
    let started = Instant::now();
    loop {
        match cmd.output().await {
            Err(e) if is_text_busy(&e) && started.elapsed() < LIMIT => tokio::time::sleep(STEP).await,
            r => return r,
        }
    }
}

#[cfg(test)]
mod tests {
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
}
