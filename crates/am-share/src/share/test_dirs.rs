//! 測試用暫存目錄（不依賴 daemon 的 `testing`，讓 am-share 自己的測試也跑得起來）：登記後在行程結束時刪掉。
use std::sync::{Mutex, Once};

fn remove_at_exit(path: &std::path::Path) {
    static LIST: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());
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

/// `$TMPDIR/<prefix>-<ulid>`，已建好並登記。
pub fn scratch_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", crate::db::ulid()));
    std::fs::create_dir_all(&dir).unwrap();
    remove_at_exit(&dir);
    dir
}

/// 把自己組的暫存路徑登記成「行程結束時刪掉」，原樣回傳、不建立。
pub fn track(path: std::path::PathBuf) -> std::path::PathBuf {
    remove_at_exit(&path);
    path
}
