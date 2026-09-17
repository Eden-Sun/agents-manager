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

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::io::{AsRawFd, FromRawFd as _};
use std::path::{Component, Path};
use std::time::{Duration, SystemTime};

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
    open_entry_in(&dir, name)
}

/// 打開 `dir`（呼叫端已經驗證、開好的目錄 fd，例如 [`read_dir_bound`] 列出來的一筆）底下的 `name`：
/// `O_NOFOLLOW`，並確認是一般檔案。跟 [`open_bound_file`] 的差別是這裡不重新從某個 `base` 逐層解一次
/// 路徑到 `dir`——呼叫端手上已經有驗證過的目錄 fd，只是要打開它底下**這一個**項目而已（issue #96：
/// `outbox::scan()` 判斷一個檔案要不要列出來得看內容開頭幾個位元組，這個判斷也要在同一個已驗證的
/// 目錄 fd 底下做，不能又用路徑重新 open 一次那個檔名）。
pub(crate) fn open_entry_in(dir: &File, name: &OsStr) -> io::Result<File> {
    let file = openat_raw(dir.as_raw_fd(), name, libc::O_RDONLY | libc::O_NOFOLLOW)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}

/// 一個目錄項目：名字、是不是一般檔案（符號連結／目錄／其他都是 `false`）、大小、mtime。
pub(crate) struct BoundEntry {
    pub name: OsString,
    pub is_file: bool,
    pub size: u64,
    pub modified: SystemTime,
}

/// 列舉 `dir`（已經是 [`open_bound_dir`] 驗證、打開好的目錄 fd）裡的項目，全程只用這一個 fd：
/// `fdopendir`／`readdir` 取檔名，`fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)` 取種類／大小／mtime——
/// 不符號連結、不重新用路徑解析這個目錄本身，也不對任何一個項目重新用路徑 open（issue #96：
/// 過去只有「這個目錄可不可信」的檢查是 fd-bound，列舉本身仍是路徑 `read_dir`，兩者中間有縫隙——
/// 雖然只到檔名／大小外洩，讀不到內容，這裡把那道縫隙也封掉）。
///
/// `fdopendir` 要接管一個獨立的 fd，不能直接把 `dir.as_raw_fd()` 或它的 `dup()` 交給它：`dup` 出來的
/// fd 跟原本的 fd 共用同一份 open file description，**含目錄的讀取位置**——讀到底再 `closedir`
/// 之後，呼叫端手上的 `dir` 也會被推到跟著到底，下一次再列什麼都讀不到（這裡曾經這樣寫，被
/// [`tests::the_directory_fd_stays_usable_after_listing`] 抓到）。改成對 `dir` 自己
/// `openat(dir, ".", O_DIRECTORY)`：對同一個已經驗證過的目錄 fd 重新開一次「自己」，拿到的是完全
/// 獨立的 open file description（位置各自獨立），`dir` 本身不管列幾次都不受影響；`"."` 是固定的
/// 自我參照，不是外部可控、可以被換掉的名字，這一步沒有引入新的路徑解析風險。
pub(crate) fn read_dir_bound(dir: &File) -> io::Result<Vec<BoundEntry>> {
    let reopened = openat_raw(dir.as_raw_fd(), OsStr::new("."), libc::O_RDONLY | libc::O_DIRECTORY)?;
    let raw = reopened.as_raw_fd();
    let dp = unsafe { libc::fdopendir(raw) };
    if dp.is_null() {
        return Err(io::Error::last_os_error()); // `reopened` 掉出作用域時正常關掉這個 fd，不會外洩。
    }
    // fdopendir 成功之後這個 fd 的關閉交給 closedir；`reopened` 不用再自己關一次。
    std::mem::forget(reopened);
    struct DirGuard(*mut libc::DIR);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let _guard = DirGuard(dp);

    let mut out = Vec::new();
    loop {
        // 這個 DIR* 只有這個函式自己用（剛從 fdopendir 拿到，沒有分享給別的執行緒），單純的
        // `readdir`（不是 `readdir_r`）沒有資料競爭的疑慮。
        let entry = unsafe { libc::readdir(dp) };
        if entry.is_null() {
            break; // 到底了；讀不出更多東西一律當作到底，跟 `std::fs::read_dir` 出錯時 `.flatten()` 跳過一樣寧可少列不要出錯。
        }
        let name_bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let name = OsString::from_vec(name_bytes.to_vec());
        let Ok(name_c) = cstr(&name) else { continue };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // AT_SYMLINK_NOFOLLOW：查符號連結本身，不跟著它走——符號連結不列是既有規則，這裡沿用，
        // 不會因為連結指到一個大檔案就把假的大小/mtime 交出去。
        let rc = unsafe { libc::fstatat(dir.as_raw_fd(), name_c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        if rc != 0 {
            continue; // 讀到名字之後、fstat 之前又被刪掉：跳過，不是錯誤。
        }
        let is_file = (st.st_mode & libc::S_IFMT) == libc::S_IFREG;
        let modified = SystemTime::UNIX_EPOCH + Duration::new(st.st_mtime.max(0) as u64, st.st_mtime_nsec.clamp(0, 999_999_999) as u32);
        out.push(BoundEntry { name, is_file, size: st.st_size.max(0) as u64, modified });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `open_entry_in` 只認已經打開的目錄 fd 底下那一個項目：一般檔案放行，符號連結（就算指到界線外的
    /// 一般檔案）跟目錄都拒絕，不會重新用路徑解析 `dir` 本身。
    #[test]
    fn open_entry_in_only_opens_a_regular_file_directly_under_the_given_fd() {
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
        assert!(open_entry_in(&dir, OsStr::new("link.txt")).is_err(), "符號連結不能開");
        assert!(open_entry_in(&dir, OsStr::new("sub")).is_err(), "目錄不是一般檔案");
        assert!(open_entry_in(&dir, OsStr::new("missing.txt")).is_err());
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
