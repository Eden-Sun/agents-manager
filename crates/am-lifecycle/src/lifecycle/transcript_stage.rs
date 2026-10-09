//! 換身分時把 claude 的 session 檔（transcript）複製到新身分的 `projects/` 底下（本機那一半；遠端是 `start.rs` 的 shell script）。
//!
//! 對話檔是整段對話內容，這裡的規則都是資料安全：
//! * 來源只收 `…/projects/<cwd 目錄>/<name>.jsonl` 形狀的**一般檔**（`transcript_path` 是 hook payload 記下來的字串，
//!   不能拿它當「複製任意檔案」的指令；符號連結也不跟）。
//! * 先寫到同目錄的暫存檔（`0600`）、`fsync`、再 `rename`：複製到一半失敗或 daemon 被殺，最終路徑上不會有半份檔被 `--resume` 誤用，
//!   暫存檔名以 `.` 開頭、不以 `.jsonl` 結尾，CLI 看不到。
//! * 目標已經有同名檔：一樣就不動；來源是它的前綴（目標那邊已經接著寫過）就**不蓋**；目標是來源的前綴就蓋；
//!   兩邊各自長出不同內容就先把目標改名留在旁邊（`<name>.jsonl.replaced-<ms>`）再放來源，不是直接消失。
//! * 新建的目錄 `0700`、複製出去的檔 `0600`，不管來源當初是什麼權限。

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Staged {
    /// 放好了（含目標只是來源的前綴、被換成較長的來源）。
    Copied,
    /// 目標一樣，什麼都不用做。
    Unchanged,
    /// 目標比來源長（來源是它的前綴）：目標那邊已經接著寫過，不動。
    KeptLongerDestination,
    /// 兩邊分岔：目標被改名留在旁邊，來源放上去了。
    Diverged { set_aside: PathBuf },
}

/// 來源的形狀與種類：`<projects>/<key>/<name>.jsonl`，一般檔、不是符號連結。
pub fn is_claude_transcript(src: &Path) -> bool {
    let Some(cwd_dir) = src.parent() else { return false };
    let Some(projects_dir) = cwd_dir.parent() else { return false };
    let real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir());
    src.extension() == Some(OsStr::new("jsonl"))
        && !src.components().any(|c| matches!(c, std::path::Component::ParentDir))
        && projects_dir.file_name() == Some(OsStr::new("projects"))
        && real_dir(projects_dir)
        && real_dir(cwd_dir)
        && std::fs::symlink_metadata(src).is_ok_and(|m| m.file_type().is_file() && m.nlink() == 1)
}

/// 遠端用的字串版（沒辦法 stat，只看形狀；符號連結由 script 自己擋）。
pub fn has_claude_transcript_shape(path: &str) -> bool {
    let p = Path::new(path);
    p.is_absolute()
        && !path.chars().any(char::is_control)
        && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
        && p.extension() == Some(OsStr::new("jsonl"))
        && p.parent().and_then(Path::parent).and_then(Path::file_name) == Some(OsStr::new("projects"))
}

/// `a` 的內容是不是 `b` 開頭那一段（`a` 比 `b` 短或一樣長）。
pub fn is_prefix_of(a: &std::fs::File, b: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::fs::FileExt as _;
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    let mut offset = 0u64;
    loop {
        let n = a.read_at(&mut ba, offset)?;
        if n == 0 {
            return Ok(true);
        }
        let mut got = 0;
        while got < n {
            let m = b.read_at(&mut bb[got..n], offset + got as u64)?;
            if m == 0 {
                return Ok(false);
            }
            got += m;
        }
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
        offset += n as u64;
    }
}

/// 把 `src` 放到 `dest_dir/fname`（規則見檔頭）。
pub fn stage_file(src: &Path, dest_dir: &Path, fname: &OsStr) -> std::io::Result<Staged> {
    stage_file_after_missing_check(src, dest_dir, fname, || {})
}

fn stage_file_after_missing_check(
    src: &Path,
    dest_dir: &Path,
    fname: &OsStr,
    after_missing_check: impl FnOnce(),
) -> std::io::Result<Staged> {
    if !is_claude_transcript(src) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "source is not a real Claude transcript"));
    }
    let Some(projects_dir) = dest_dir.parent().filter(|p| p.file_name() == Some(OsStr::new("projects"))) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "destination is not a Claude projects directory"));
    };
    let Some(config_dir) = projects_dir.parent() else { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "destination projects directory has no account root")) };
    let Some(cwd_key) = dest_dir.file_name() else { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "destination has no cwd key")) };
    let parts = [OsStr::new("projects"), cwd_key];
    let dir = crate::trusted_open::create_private_bound_dirs(config_dir, &parts)?;
    let mut from = crate::transcript_read::open_regular(src)?;
    let mut outcome = Staged::Copied;
    let mut preserve_old = false;
    let mut replace_old = false;
    let existing = match crate::trusted_open::open_entry_in(&dir, fname) {
        Ok(f) => Some(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if existing.is_none() {
        after_missing_check();
    }
    if let Some(existing) = existing {
        let (sm, dm) = (from.metadata()?, existing.metadata()?);
        if sm.dev() == dm.dev() && sm.ino() == dm.ino() { return Ok(Staged::Unchanged); }
        let (src_len, dest_len) = (sm.len(), dm.len());
        if dest_len == src_len && is_prefix_of(&from, &existing)? { return Ok(Staged::Unchanged); }
        if dest_len > src_len && is_prefix_of(&from, &existing)? { return Ok(Staged::KeptLongerDestination); }
        preserve_old = !(dest_len < src_len && is_prefix_of(&existing, &from)?);
        replace_old = true;
    }
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(fname);
    tmp_name.push(format!(".stage-{}", crate::db::ulid()));
    let done = (|| {
        use std::io::Write as _;
        let mut tmp = crate::trusted_open::create_new_file_in(&dir, &tmp_name, 0o600)?;
        std::io::copy(&mut from, &mut tmp)?;
        tmp.flush()?;
        tmp.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(e) = done {
        let _ = crate::trusted_open::unlink_in(&dir, &tmp_name);
        return Err(e);
    }
    let mut backup_name = None;
    if replace_old {
        let mut backup = if preserve_old { fname.to_os_string() } else { std::ffi::OsString::from(".stage-old-") };
        backup.push(if preserve_old { format!(".replaced-{}-{}", chrono::Utc::now().timestamp_millis(), crate::db::ulid()) } else { crate::db::ulid() });
        if let Err(e) = crate::trusted_open::rename_in(&dir, fname, &backup) {
            let _ = crate::trusted_open::unlink_in(&dir, &tmp_name);
            // Another stager may have moved the destination after our comparison. Re-read the
            // winner's state and apply the same prefix/divergence rules to that version.
            if e.kind() == std::io::ErrorKind::NotFound {
                return stage_file(src, dest_dir, fname);
            }
            return Err(e);
        }
        backup_name = Some(backup.clone());
        if preserve_old { outcome = Staged::Diverged { set_aside: dest_dir.join(&backup) }; }
    }
    if let Err(e) = crate::trusted_open::rename_noreplace_in(&dir, &tmp_name, fname) {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            let _ = crate::trusted_open::unlink_in(&dir, &tmp_name);
            // `rename_noreplace` publishes the completed, fsynced file atomically. A competing stager won
            // the same name between our lookup and publication; verify its contents before
            // accepting it, and retain any divergent destination according to the normal rules.
            let retry = stage_file(src, dest_dir, fname);
            if retry.is_ok() {
                if let Some(backup) = backup_name.as_deref().filter(|_| !preserve_old) {
                    crate::trusted_open::unlink_in(&dir, backup)?;
                }
            }
            return retry;
        }
        if let Some(backup) = backup_name.as_deref() {
            if crate::trusted_open::rename_noreplace_in(&dir, backup, fname).is_ok() { let _ = crate::trusted_open::unlink_in(&dir, backup); }
        }
        let _ = crate::trusted_open::unlink_in(&dir, &tmp_name);
        return Err(e);
    }
    if let Some(backup) = backup_name.as_deref().filter(|_| !preserve_old) { crate::trusted_open::unlink_in(&dir, backup)?; }
    Ok(outcome)
}

/// 附屬目錄整份複製：只收一般檔與目錄（符號連結不跟、不複製），檔 `0600`、目錄 `0700`。
pub fn copy_dir_private(src: &Path, dest: &Path) -> std::io::Result<()> {
    if !std::fs::symlink_metadata(src).is_ok_and(|m| m.file_type().is_dir()) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "companion source is not a real directory"));
    }
    let (src_config, src_parts) = companion_dir_parts(src)?;
    let (dest_config, dest_parts) = companion_dir_parts(dest)?;
    let src_fd = crate::trusted_open::open_bound_dir(&src_config, &src_parts, None)?;
    let dest_fd = crate::trusted_open::create_private_bound_dirs(&dest_config, &dest_parts)?;
    copy_bound_dir(&src_fd, &dest_fd)
}

fn companion_dir_parts(path: &Path) -> std::io::Result<(PathBuf, Vec<&OsStr>)> {
    let Some(cwd) = path.parent() else { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "companion directory has no cwd")) };
    let Some(projects) = cwd.parent().filter(|p| p.file_name() == Some(OsStr::new("projects"))) else { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "companion is outside a projects directory")) };
    let Some(config) = projects.parent() else { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "projects directory has no config root")) };
    Ok((config.to_path_buf(), vec![OsStr::new("projects"), cwd.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "cwd is missing"))?, path.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "companion name is missing"))?]))
}

fn copy_bound_dir(src: &std::fs::File, dest: &std::fs::File) -> std::io::Result<()> {
    for entry in crate::trusted_open::read_dir_bound(src)? {
        if entry.is_dir {
            let source_child = crate::trusted_open::open_dir_entry_in(src, &entry.name)?;
            let dest_child = crate::trusted_open::create_dir_entry_in(dest, &entry.name, 0o700)?;
            copy_bound_dir(&source_child, &dest_child)?;
        } else if entry.is_file {
            let mut from = crate::trusted_open::open_entry_in(src, &entry.name)?;
            let mut tmp_name = std::ffi::OsString::from(".");
            tmp_name.push(&entry.name);
            tmp_name.push(format!(".stage-{}", crate::db::ulid()));
            let result = (|| {
                let mut out = crate::trusted_open::create_new_file_in(dest, &tmp_name, 0o600)?;
                std::io::copy(&mut from, &mut out)?;
                out.flush()?;
                out.sync_all()?;
                crate::trusted_open::rename_noreplace_in(dest, &tmp_name, &entry.name)
            })();
            if let Err(e) = result { let _ = crate::trusted_open::unlink_in(dest, &tmp_name); return Err(e); }
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let p = crate::testing::track(std::env::temp_dir().join(format!("am-transcript-stage-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn source(root: &Path) -> PathBuf {
        let dir = root.join("projects/key");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sid.jsonl");
        std::fs::write(&path, "private conversation").unwrap();
        path
    }

    fn stage(src: &Path, config: &Path) -> std::io::Result<Staged> {
        stage_file(src, &config.join("projects/key"), OsStr::new("sid.jsonl"))
    }

    #[test]
    fn staging_creates_a_missing_identity_root_privately() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = root("missing-identity-root");
        let src = source(&base.join("old"));
        let config = base.join("new-identity");
        assert_eq!(stage(&src, &config).unwrap(), Staged::Copied);
        let dest = config.join("projects/key/sid.jsonl");
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "private conversation");
        assert_eq!(std::fs::metadata(&config).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn prefix_comparison_does_not_advance_the_open_source() {
        use std::io::Read as _;
        let base = root("prefix-position");
        let src = source(&base.join("old"));
        let dest = base.join("long-destination.jsonl");
        std::fs::write(&dest, "private conversation and later turns").unwrap();
        let mut a = crate::transcript_read::open_regular(&src).unwrap();
        let b = crate::transcript_read::open_regular(&dest).unwrap();
        assert!(is_prefix_of(&a, &b).unwrap());
        let mut actual = String::new();
        a.read_to_string(&mut actual).unwrap();
        assert_eq!(actual, "private conversation");
    }

    #[test]
    fn a_projects_root_symlink_is_not_a_transcript_source() {
        let base = root("projects-link");
        let real = base.join("real");
        let transcript = source(&real);
        let alias = base.join("alias");
        std::fs::create_dir_all(&alias).unwrap();
        std::os::unix::fs::symlink(real.join("projects"), alias.join("projects")).unwrap();
        assert!(stage(&alias.join("projects/key/sid.jsonl"), &base.join("new")).is_err());
        assert!(!base.join("new/projects/key/sid.jsonl").exists());
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), "private conversation");
    }

    #[test]
    fn a_source_cwd_symlink_is_not_followed() {
        let base = root("source-cwd-link");
        let outside = source(&base.join("outside"));
        let old = base.join("old/projects");
        std::fs::create_dir_all(&old).unwrap();
        std::os::unix::fs::symlink(outside.parent().unwrap(), old.join("key")).unwrap();
        assert!(stage(&old.join("key/sid.jsonl"), &base.join("new")).is_err());
        assert!(!base.join("new/projects/key/sid.jsonl").exists());
    }

    #[test]
    fn a_destination_cwd_symlink_is_rejected_without_writing_through_it() {
        let base = root("destination-cwd-link");
        let transcript = source(&base.join("old"));
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let projects = base.join("new/projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::os::unix::fs::symlink(&outside, projects.join("key")).unwrap();
        assert!(stage(&transcript, &base.join("new")).is_err());
        assert!(!outside.join("sid.jsonl").exists());
    }

    #[test]
    fn hard_linked_sources_and_destinations_are_rejected() {
        let base = root("hard-links");
        let src = source(&base.join("old"));
        let secret = base.join("secret");
        std::fs::write(&secret, "private conversation").unwrap();
        let linked_src = base.join("linked/projects/key/sid.jsonl");
        std::fs::create_dir_all(linked_src.parent().unwrap()).unwrap();
        std::fs::hard_link(&secret, &linked_src).unwrap();
        assert!(stage(&linked_src, &base.join("source-reject")).is_err());
        assert!(!base.join("source-reject/projects/key/sid.jsonl").exists());

        let linked_dest = base.join("new/projects/key/sid.jsonl");
        std::fs::create_dir_all(linked_dest.parent().unwrap()).unwrap();
        std::fs::hard_link(&secret, &linked_dest).unwrap();
        assert!(stage(&src, &base.join("new")).is_err());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "private conversation");
        assert_eq!(std::fs::read_to_string(linked_dest).unwrap(), "private conversation");
    }

    #[test]
    fn concurrent_first_stagers_both_accept_the_winning_copy() {
        let base = root("concurrent-first-stage");
        let src = source(&base.join("old"));
        let dest_dir = base.join("new/projects/key");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let results = std::thread::scope(|scope| {
            let threads = (0..2)
                .map(|_| {
                    let barrier = barrier.clone();
                    let src = src.clone();
                    let dest_dir = dest_dir.clone();
                    scope.spawn(move || {
                        stage_file_after_missing_check(
                            &src,
                            &dest_dir,
                            OsStr::new("sid.jsonl"),
                            || {
                                barrier.wait();
                            },
                        )
                    })
                })
                .collect::<Vec<_>>();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert!(
            results.iter().all(Result::is_ok),
            "concurrent staging results: {results:?}"
        );
        let final_file = dest_dir.join("sid.jsonl");
        assert_eq!(
            std::fs::read_to_string(&final_file).unwrap(),
            "private conversation"
        );
        assert_eq!(
            std::fs::read_dir(&dest_dir).unwrap().count(),
            1,
            "only the completed transcript should remain"
        );
    }
}
