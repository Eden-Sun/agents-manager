//! 原子寫檔：暫存檔寫好、`fsync`、`rename` 進去，讀的人只會看到完整的舊內容或完整的新內容（#919，#520 同形）。
//!
//! `std::fs::write` 是 `O_TRUNC` 之後才寫：中間讀到的是空檔或半截。`runtime.json` 每次 `bin/agm` 執行都讀，
//! 半截就是 `bad_runtime`。順序跟 am-config 的 `write_atomic` 一樣：
//! 同目錄暫存檔（`create_new` ＋ 一開始就 0600，內容不會先以 umask 的 0644 露出來）→ 寫 → `sync_all` →
//! 套權限 → `rename` → 目錄 `fsync` → 失敗刪暫存。

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// 寫完之後檔案的權限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 固定這個權限（例如 0644）。
    Exact(u32),
    /// 沿用原檔的權限（使用者自己 chmod 過的不能被悄悄放寬或收窄）；原檔不存在就維持 0600。
    Preserve,
}

/// 把 `data` 原子地寫成 `path`。目錄必須已經存在。
pub fn write(path: &Path, data: &[u8], mode: Mode) -> std::io::Result<()> {
    // 同一個行程裡並行的兩次寫入不能共用暫存檔：pid 之外再加遞增號。
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{} 沒有檔名", path.display())))?;
    let tmp = dir.join(format!(".{name}.am-tmp.{}.{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
        match mode {
            Mode::Exact(m) => f.set_permissions(std::fs::Permissions::from_mode(m))?,
            Mode::Preserve => {
                if let Ok(md) = std::fs::metadata(path) {
                    f.set_permissions(md.permissions())?;
                }
            }
        }
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // 目錄項目的更動也要落盤；有些檔案系統不支援對目錄 fsync，那就算了（檔案本身已經 sync 過）。
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("am-atomic-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn leftovers(dir: &Path, keep: &str) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != keep)
            .collect()
    }

    /// 讀者執行緒一直讀、寫者反覆換內容：每次讀到的都必須是完整的舊版或完整的新版。
    #[test]
    fn a_write_never_exposes_a_truncated_file() {
        let dir = scratch("trunc");
        let path = dir.join("runtime.json");
        let old = "a".repeat(2_000_000);
        let new = "b".repeat(2_000_000);
        write(&path, old.as_bytes(), Mode::Exact(0o644)).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (path, stop, old, new) = (path.clone(), stop.clone(), old.clone(), new.clone());
            std::thread::spawn(move || {
                let mut bad = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(got) = std::fs::read_to_string(&path) {
                        if got != old && got != new {
                            bad.push(got.len());
                        }
                    }
                }
                bad
            })
        };
        for i in 0..40 {
            write(&path, if i % 2 == 0 { new.as_bytes() } else { old.as_bytes() }, Mode::Exact(0o644)).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        let bad = reader.join().unwrap();
        assert!(bad.is_empty(), "讀到了不是舊也不是新的內容（位元組數）：{bad:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 暫存檔一開始就是 0600（內容不會先以 0644 露出來）；結果的權限：`Exact` 固定、`Preserve` 沿用原檔、原檔不存在則 0600。
    #[test]
    fn the_temp_file_starts_private_and_the_result_keeps_the_old_mode() {
        let dir = scratch("mode");
        let path = dir.join("f.txt");

        // 觀察者：寫的過程中出現在目錄裡的其他檔案（暫存檔）都不能有 group／other 權限。
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (dir, stop) = (dir.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut loose = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    for e in std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()) {
                        if e.file_name().to_string_lossy().starts_with(".f.txt.am-tmp") {
                            if let Ok(md) = e.metadata() {
                                if md.permissions().mode() & 0o077 != 0 {
                                    loose.push(md.permissions().mode() & 0o777);
                                }
                            }
                        }
                    }
                }
                loose
            })
        };
        // 結果要 0600 的寫入：暫存檔從建立到 rename 都不能比 0600 寬（`Exact(0o644)` 的暫存檔在寫完內容、rename 之前
        // 本來就會變 0644，那是它最後要的權限，不在這裡觀察）。
        let big = vec![b'x'; 4_000_000];
        for _ in 0..20 {
            write(&path, &big, Mode::Exact(0o600)).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        let loose = watcher.join().unwrap();
        assert!(loose.is_empty(), "暫存檔曾經以較寬的權限出現：{loose:?}");
        assert_eq!(mode_of(&path), 0o600);
        write(&path, b"first", Mode::Exact(0o644)).unwrap();
        assert_eq!(mode_of(&path), 0o644);

        // Preserve：沿用原檔（0640），不被放寬成 0644／收窄成 0600。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write(&path, b"second", Mode::Preserve).unwrap();
        assert_eq!(mode_of(&path), 0o640);
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        // Preserve + 原檔不存在 → 0600。
        let fresh = dir.join("fresh.txt");
        write(&fresh, b"x", Mode::Preserve).unwrap();
        assert_eq!(mode_of(&fresh), 0o600);
        // Exact 蓋過原檔的權限。
        write(&path, b"third", Mode::Exact(0o600)).unwrap();
        assert_eq!(mode_of(&path), 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 寫失敗（目標是個目錄，rename 不過）：不留暫存檔。
    #[test]
    fn a_failed_write_leaves_no_temp_file() {
        let dir = scratch("fail");
        let target = dir.join("target");
        std::fs::create_dir(&target).unwrap();
        assert!(write(&target, b"data", Mode::Exact(0o644)).is_err());
        assert_eq!(leftovers(&dir, "target"), Vec::<String>::new());
        // 目錄不存在：開不了暫存檔，也不該留東西。
        assert!(write(&dir.join("no-such-dir").join("f"), b"data", Mode::Preserve).is_err());
        assert_eq!(leftovers(&dir, "target"), Vec::<String>::new());
        std::fs::remove_dir_all(&dir).ok();
    }
}
