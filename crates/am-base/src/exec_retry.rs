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
#[cfg(any(test, feature = "test-hooks"))]
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
#[cfg(any(test, feature = "test-hooks"))]
pub fn output(cmd: &mut std::process::Command) -> io::Result<std::process::Output> {
    retry(LIMIT, || cmd.output())
}

/// `std::process::Command::spawn`，遇到 `ETXTBSY` 重試。
#[cfg(any(test, feature = "test-hooks"))]
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
