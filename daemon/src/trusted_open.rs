//! 「先開、再用同一個 fd 驗」的共用邊界（issue #89）：outbox／local-image 過去都是先用路徑名字驗證
//! （canonicalize＋containment＋metadata），再用路徑名字重新 open 一次讀內容——兩次 open 之間，寫得到
//! 那個目錄的 process（bot 自己）可以把驗證通過的路徑換成指到界線外的符號連結，第二次 open 就讀到別的
//! 檔案：
//!
//! ```text
//! canonicalize(path) → 確認在界線內 → metadata(path) → read(path)  // 每個箭頭之間都能把 path 換掉
//! ```
//!
//! 這裡改成逐層 `openat(2)` 帶 `O_NOFOLLOW`：一段路徑是不是符號連結、打開的是哪個 inode，由同一個系統
//! 呼叫決定，不會有「查完才發現被換了」的窗口。拿到 [`std::fs::File`] 之後所有判斷（是不是一般檔案、
//! 擁有者、大小、內容開頭）都讀同一個 fd 的 `fstat`／內容，不再用路徑名字重新 open。
//!
//! 只支援 Unix（`libc::openat`／`O_NOFOLLOW`）——這個 daemon 只在 macOS 上跑，沒有另外做 Windows 的路。

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::io::{AsRawFd, FromRawFd as _};
use std::path::{Component, Path};

fn cstr(part: &OsStr) -> io::Result<CString> {
    CString::new(part.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path component has an embedded NUL"))
}

fn openat_raw(dirfd: i32, name: &OsStr, flags: i32) -> io::Result<File> {
    let c = cstr(name)?;
    // `O_CLOEXEC`：這個 fd 不該被 bot 自己的子行程繼承到。
    let fd = unsafe { libc::openat(dirfd, c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_dir(path: &Path) -> io::Result<File> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has an embedded NUL"))?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// `requested` 拆成一串一般 component：`..`、`.`、`/` 開頭（絕對路徑）、空字串一律 `None`。呼叫端要嘛已經
/// 把絕對路徑用字串比對砍成相對於信任邊界的殘餘，要嘛本來就是相對路徑——這裡不処理「怎麼變成相對的」，
/// 只確認拆出來的每一段都是普通檔名，不會被解讀成往上跳或跳到別的分支。
pub(crate) fn safe_relative_components(requested: &Path) -> Option<Vec<&OsStr>> {
    let mut out = Vec::new();
    for c in requested.components() {
        match c {
            Component::Normal(s) => out.push(s),
            _ => return None,
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 從 `base`（信任邊界本身——daemon 自己的 data_dir、或已經 canonicalize 過的 project 目錄；不要求它自己
/// 不是符號連結，那是註冊／啟動時就定下來的）開始，把 `components` 逐段打開成同一條鏈上的目錄 fd：
/// 每一段都 `O_NOFOLLOW`，是符號連結、不是目錄都直接失敗；`owner_uid` 給了值時，每一層的擁有者都要跟它
/// 一致（outbox 的兩段防護，見 902a85c——現在用 fd 自己的 `fstat` 查，不是重新 `stat` 路徑）。
pub(crate) fn open_bound_dir(base: &Path, components: &[&OsStr], owner_uid: Option<u32>) -> io::Result<File> {
    let mut dir = open_dir(base)?;
    for part in components {
        dir = openat_raw(dir.as_raw_fd(), part, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)?;
        if let Some(uid) = owner_uid {
            if dir.metadata()?.uid() != uid {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "directory owner mismatch"));
            }
        }
    }
    Ok(dir)
}

/// 同上，但最後一段是要讀的檔案：前面每一段當目錄逐層打開（同 [`open_bound_dir`]），最後一段
/// `O_NOFOLLOW` 打開、`fstat` 確認是一般檔案。回傳的 `File` 綁在真正打開當下的那個 inode，呼叫端之後所有
/// 檢查（大小、內容開頭）與真正的回應內容都要讀這個 fd，不能再用路徑名字重新 open。
pub(crate) fn open_bound_file(base: &Path, components: &[&OsStr], owner_uid: Option<u32>) -> io::Result<File> {
    let Some((name, dirs)) = components.split_last() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path"));
    };
    let dir = open_bound_dir(base, dirs, owner_uid)?;
    let file = openat_raw(dir.as_raw_fd(), name, libc::O_RDONLY | libc::O_NOFOLLOW)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("am-trusted-open-{tag}-{}", crate::db::ulid()));
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

    /// 最後一段是符號連結指到界線外：`O_NOFOLLOW` 直接失敗，不會先「查到是連結」才決定不讀——查跟開是
    /// 同一個系統呼叫。
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

    #[test]
    fn safe_relative_components_rejects_traversal_and_absolute_paths() {
        assert!(safe_relative_components(Path::new("a/b.txt")).is_some());
        assert!(safe_relative_components(Path::new("../outside.txt")).is_none());
        assert!(safe_relative_components(Path::new("a/../b.txt")).is_none());
        assert!(safe_relative_components(Path::new("/etc/passwd")).is_none());
        assert!(safe_relative_components(Path::new("")).is_none());
        assert!(safe_relative_components(Path::new(".")).is_none());
    }
}
