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
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
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
    let shaped = src.extension() == Some(OsStr::new("jsonl"))
        && src.parent().and_then(Path::parent).and_then(Path::file_name) == Some(OsStr::new("projects"));
    shaped && std::fs::symlink_metadata(src).is_ok_and(|m| m.file_type().is_file())
}

/// 遠端用的字串版（沒辦法 stat，只看形狀；符號連結由 script 自己擋）。
pub fn has_claude_transcript_shape(path: &str) -> bool {
    let p = Path::new(path);
    p.extension() == Some(OsStr::new("jsonl")) && p.parent().and_then(Path::parent).and_then(Path::file_name) == Some(OsStr::new("projects"))
}

/// 一路建到 `dir`，這次新建的每一層都是 `0700`。
pub fn private_create_dir_all(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// `a` 的內容是不是 `b` 開頭那一段（`a` 比 `b` 短或一樣長）。
fn is_prefix_of(a: &Path, b: &Path) -> std::io::Result<bool> {
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        let mut got = 0;
        while got < n {
            let m = fb.read(&mut bb[got..n])?;
            if m == 0 {
                return Ok(false);
            }
            got += m;
        }
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

pub(crate) fn write_private_copy(src: &Path, tmp: &Path) -> std::io::Result<()> {
    let mut from = std::fs::File::open(src)?;
    let mut to = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(tmp)?;
    std::io::copy(&mut from, &mut to)?;
    to.flush()?;
    to.sync_all()
}

/// 把 `src` 放到 `dest_dir/fname`（規則見檔頭）。
pub fn stage_file(src: &Path, dest_dir: &Path, fname: &OsStr) -> std::io::Result<Staged> {
    private_create_dir_all(dest_dir)?;
    let dest = dest_dir.join(fname);
    let mut outcome = Staged::Copied;
    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        if meta.file_type().is_file() {
            let (src_len, dest_len) = (std::fs::metadata(src)?.len(), meta.len());
            if dest_len == src_len && is_prefix_of(src, &dest)? {
                return Ok(Staged::Unchanged);
            }
            if dest_len > src_len && is_prefix_of(src, &dest)? {
                return Ok(Staged::KeptLongerDestination);
            }
            if !(dest_len < src_len && is_prefix_of(&dest, src)?) {
                let mut aside = fname.to_os_string();
                aside.push(format!(".replaced-{}", chrono::Utc::now().timestamp_millis()));
                let aside = dest_dir.join(aside);
                std::fs::rename(&dest, &aside)?;
                outcome = Staged::Diverged { set_aside: aside };
            }
        }
    }
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(fname);
    tmp_name.push(format!(".stage-{}", crate::db::ulid()));
    let tmp = dest_dir.join(tmp_name);
    let done = write_private_copy(src, &tmp).and_then(|()| std::fs::rename(&tmp, &dest));
    if let Err(e) = done {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(outcome)
}

/// 附屬目錄整份複製：只收一般檔與目錄（符號連結不跟、不複製），檔 `0600`、目錄 `0700`。
pub fn copy_dir_private(src: &Path, dest: &Path) -> std::io::Result<()> {
    private_create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let to = dest.join(entry.file_name());
        if kind.is_dir() {
            copy_dir_private(&entry.path(), &to)?;
        } else if kind.is_file() {
            let mut from = std::fs::File::open(entry.path())?;
            let mut out = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&to)?;
            std::io::copy(&mut from, &mut out)?;
        }
    }
    Ok(())
}
