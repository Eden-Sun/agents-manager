//! 測試用暫存目錄與輔助（不依賴 daemon 的 `testing`，讓 am-config 測試跑得起來）。
use super::*;
use std::sync::{Mutex, Once};

pub fn ulid() -> String {
    ulid::Ulid::new().to_string()
}

fn remove_at_exit(path: &std::path::Path) {
    static LIST: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static HOOK: Once = Once::new();
    extern "C" fn sweep() {
        for p in LIST.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            if std::fs::remove_dir_all(&p).is_err() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    LIST.lock().unwrap_or_else(|e| e.into_inner()).push(path.to_path_buf());
    HOOK.call_once(|| {
        // SAFETY: `sweep` 是沒有參數的 `extern "C"` 函式，整個行程生命週期內都有效。
        unsafe { libc::atexit(sweep) };
    });
}

/// `$TMPDIR/<prefix>-<ulid>`，已建好並登記。基底先 canonicalize，理由同 `testing::scratch_dir`（#1964）。
pub fn scratch_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().canonicalize().unwrap().join(format!("{prefix}-{}", ulid()));
    std::fs::create_dir_all(&dir).unwrap();
    remove_at_exit(&dir);
    dir
}

/// 把自己組的暫存路徑登記成「行程結束時刪掉」，原樣回傳、不建立。
#[allow(dead_code)]
pub fn track(path: PathBuf) -> PathBuf {
    remove_at_exit(&path);
    path
}

pub struct NoHooks;
impl ConfigChangeHooks for NoHooks {
    fn validate_projection(&self, _next: &ConfigFile) -> Result<()> {
        Ok(())
    }
    fn audit_external_change(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
    fn audit_reload_unchanged(&self, _at: &'static std::panic::Location<'static>, _path: &Path) {}
    fn audit_write(&self, _at: &'static std::panic::Location<'static>, _path: &Path, _old: &ConfigFile, _new: &ConfigFile) {}
}
