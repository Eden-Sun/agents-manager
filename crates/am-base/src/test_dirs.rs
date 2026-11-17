//! 測試用暫存目錄（不依賴 daemon 的 `testing`，讓 am-base 自己的測試也跑得起來）：登記後在行程結束時刪掉。
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

/// `$TMPDIR/<prefix>-<ulid>`，已建好並登記。基底先 canonicalize，理由同 `testing::scratch_dir`（#1964）。
pub fn scratch_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().canonicalize().unwrap().join(format!("{prefix}-{}", crate::db::ulid()));
    std::fs::create_dir_all(&dir).unwrap();
    remove_at_exit(&dir);
    dir
}

/// 把自己組的暫存路徑登記成「行程結束時刪掉」，原樣回傳、不建立。
pub fn track(path: std::path::PathBuf) -> std::path::PathBuf {
    remove_at_exit(&path);
    path
}

/// 寫一支測試用的假腳本（`content` 要以 `#!` 開頭）並確定它**已經可以被 exec**（issue #189）：並行的別條測試在別的執行緒
/// `fork` 時會短暫繼承這個檔案的寫入 fd，這段時間 exec 它回 `ETXTBSY`。shell 腳本在第一行後面插一行 `AM_TEST_EXEC_PROBE` 的守衛
/// （有設就 `exit 0`），寫完用它 exec 一次、`ETXTBSY` 就重試（[`crate::exec_retry`]）。不是 shell 的腳本只寫檔加 chmod。
/// 與 daemon `testing::write_exec` 同一份行為（#1862 遷移 B：讓不依賴 daemon 的 crate-only 測試共用）。
#[cfg(unix)]
pub fn write_exec(path: impl AsRef<std::path::Path>, content: impl AsRef<str>) {
    use std::os::unix::fs::PermissionsExt as _;
    let (path, content) = (path.as_ref(), content.as_ref());
    let (shebang, rest) = content.split_once('\n').unwrap_or((content, ""));
    assert!(shebang.starts_with("#!"), "script needs a shebang line: {shebang}");
    let is_shell = shebang.split_whitespace().any(|w| matches!(w.rsplit('/').next(), Some("sh" | "bash" | "zsh" | "dash")));
    let body = if is_shell { format!("{shebang}\n[ -z \"${{AM_TEST_EXEC_PROBE:-}}\" ] || exit 0\n{rest}") } else { content.to_string() };
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    if is_shell {
        let out = crate::exec_retry::output(std::process::Command::new(path).env("AM_TEST_EXEC_PROBE", "1")).unwrap();
        assert!(out.status.success(), "{}: {:?}", path.display(), out.status);
    }
}
