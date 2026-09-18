//! 開機時把所有現存 bot 的 shim 換成**這顆 binary 帶的版本**（SPEC §6.5b／§6.5g）。
//!
//! shim（`bots/<id>/bin/herdr`、`bots/<id>/bin/cargo`）以前只在 **bot 啟動**時寫一次。長跑的 bot
//! ——AGM、協調者、使用者自己的專案 bot——可以好幾天不重啟 pane，於是新版 daemon 上線之後，
//! 它們手上還是舊 shim：
//!
//! * 2026-09-18 21:xx 巡檢實測：`ee98f6cf` 已經上線，AGM 那顆的 `bin/cargo` 還是 14:00 寫的舊版
//!   （沒有 `AM_SHIM_MARKER`），跑 `cargo check` 十分鐘不動，行程鏈是
//!   AGM shim → AGM shim → build shim → build shim → AGM shim——正是 shim 互相把對方當成真 cargo
//!   的那個巢狀死鎖，修正明明已經在 binary 裡。
//!
//! shim 只是一個檔案，換掉它不需要動 pane：**下一次**在 pane 裡打 `cargo` 就會執行到新版。正在
//! 跑的舊 shim 也不受影響——rename 只換目錄項，已經開啟的行程沿用舊 inode。
//!
//! 所以寫入一律「暫存檔 + rename」（[`write_atomic`]），而且**內容一樣就不動**：每次重啟都重寫
//! 會把 mtime 洗掉，之後沒人分得出哪些 shim 真的換過版。

use std::path::{Path, PathBuf};

/// 一顆 bot 的 bin 目錄裡，我們負責的檔案與它現在該有的內容。
fn shims() -> [(&'static str, &'static str); 2] {
    [("herdr", crate::herdr_shim::SHIM_SH), ("cargo", crate::cargo_shim::SHIM_SH)]
}

/// 內容不同才寫，寫的時候先寫暫存檔再 rename（同一個目錄，所以 rename 是原子的）。
///
/// 回 `Ok(true)` ＝真的換了。權限每次都設：從舊版升上來的檔案可能是 0644（`install_local` 以前
/// 寫完才 chmod，中間死掉就留下不能執行的 shim）。
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<bool> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(false);
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp-{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("shim"), std::process::id()));
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(true),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 掃 `<data_dir>/bots/*/bin`，把已經存在的 shim 換成當前版本。回傳換掉的 `<bot id>/<檔名>`。
///
/// **只補已經有的**：沒有 `bin/` 或沒有那支 shim 的 bot 不生出新檔案——那種 bot（遠端、從沒啟動過）
/// 的 shim 由 `lifecycle::setup::install_shim` 在啟動時處理，這裡不替它決定。
pub fn refresh_all(data_dir: &Path) -> Vec<String> {
    let mut changed = Vec::new();
    let Ok(entries) = std::fs::read_dir(data_dir.join("bots")) else { return changed };
    for entry in entries.flatten() {
        let bot_dir = entry.path();
        if !bot_dir.is_dir() {
            continue;
        }
        let bin = bot_dir.join("bin");
        for (name, content) in shims() {
            let path = bin.join(name);
            if !path.exists() {
                continue;
            }
            match write_atomic(&path, content) {
                Ok(true) => changed.push(format!("{}/{name}", entry.file_name().to_string_lossy())),
                Ok(false) => {}
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "could not refresh this shim"),
            }
        }
    }
    changed
}

/// 開機時跑一次，只記一行 log。
pub fn refresh_at_startup(data_dir: &Path) {
    let changed = refresh_all(data_dir);
    if changed.is_empty() {
        tracing::debug!("every bot shim is already the version this binary carries");
    } else {
        tracing::info!(count = changed.len(), shims = ?changed, "refreshed bot shims to this binary's version (no pane restart needed)");
    }
}

/// `<data_dir>/bots/<id>/bin` 的路徑，給 `install_local` 共用。
pub fn bin_dir(bot_dir: &Path) -> PathBuf {
    bot_dir.join("bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("am-shim-refresh-{}", crate::db::ulid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn seed(data_dir: &Path, bot: &str, body: &str) -> PathBuf {
        let bin = data_dir.join("bots").join(bot).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["herdr", "cargo"] {
            std::fs::write(bin.join(name), body).unwrap();
        }
        bin
    }

    /// 2026-09-18：新版上線了，長跑 bot 手上還是舊 shim（沒有 `AM_SHIM_MARKER`），照樣巢狀死鎖。
    /// 開機掃描要把它換掉，不必等那顆 bot 重啟 pane。
    #[test]
    fn an_old_shim_is_replaced_without_touching_the_pane() {
        let data = tmp();
        let bin = seed(&data, "01OLD", "#!/bin/sh\n# 舊版，沒有 marker\nexec cargo \"$@\"\n");
        let changed = refresh_all(&data);
        assert_eq!(changed.len(), 2, "{changed:?}");
        assert_eq!(std::fs::read_to_string(bin.join("cargo")).unwrap(), crate::cargo_shim::SHIM_SH);
        assert_eq!(std::fs::read_to_string(bin.join("herdr")).unwrap(), crate::herdr_shim::SHIM_SH);
        assert!(std::fs::read_to_string(bin.join("cargo")).unwrap().contains("AM_SHIM_MARKER"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(bin.join("cargo")).unwrap().permissions().mode() & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 內容一樣就不要動：每次重啟都重寫會把 mtime 洗掉，之後沒人分得出哪些 shim 真的換過版。
    #[test]
    fn a_shim_that_is_already_current_is_left_alone() {
        let data = tmp();
        let bin = data.join("bots").join("01NEW").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("cargo"), crate::cargo_shim::SHIM_SH).unwrap();
        std::fs::write(bin.join("herdr"), crate::herdr_shim::SHIM_SH).unwrap();
        let before = std::fs::metadata(bin.join("cargo")).unwrap().modified().unwrap();

        assert!(refresh_all(&data).is_empty());
        assert_eq!(std::fs::metadata(bin.join("cargo")).unwrap().modified().unwrap(), before);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 只補**已經有的**：沒有 bin／沒有那支 shim 的 bot 不生出新檔案，其他檔案也不碰。
    #[test]
    fn nothing_is_created_for_bots_that_never_had_a_shim() {
        let data = tmp();
        let bare = data.join("bots").join("01BARE");
        std::fs::create_dir_all(&bare).unwrap();
        let bin = data.join("bots").join("01HALF").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("cargo"), "#!/bin/sh\n# 舊\n").unwrap();
        std::fs::write(bin.join("something-else"), "keep me").unwrap();

        let changed = refresh_all(&data);
        assert_eq!(changed, vec!["01HALF/cargo".to_string()], "{changed:?}");
        assert!(!bare.join("bin").exists(), "沒有 bin 的 bot 不該被生出目錄");
        assert!(!bin.join("herdr").exists(), "本來就沒有的 shim 不補");
        assert_eq!(std::fs::read_to_string(bin.join("something-else")).unwrap(), "keep me");
        let _ = std::fs::remove_dir_all(&data);
    }

    /// 換版是 rename：暫存檔不留下來。
    #[test]
    fn the_swap_leaves_no_temporary_file_behind() {
        let data = tmp();
        let bin = seed(&data, "01TMP", "#!/bin/sh\n# 舊\n");
        refresh_all(&data);
        let leftovers: Vec<String> = std::fs::read_dir(&bin)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&data);
    }
}
