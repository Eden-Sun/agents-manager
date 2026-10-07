//! 只給自己讀的檔案與目錄（issue #494）。
//!
//! bot 目錄裡放的是 spool（**完整的** hook payload：prompt、工具輸入、回覆）與 `claude-settings.json`。
//! 以前這些都交給那台機器的 umask 決定——Linux 預設 022 ＋ `~/.config` 0755 就是 0644，同一台機器上
//! 任何使用者都讀得到整段對話。權限不該靠環境剛好正確，所以建檔時就指定 0700／0600。
//!
//! 遠端那一半在 shell 腳本裡做同一件事（`lifecycle::setup` 的 `umask 077` ＋ `chmod`），沒辦法共用這裡的碼。

use std::path::Path;

/// `mkdir -p` 出 0700 的目錄；已經存在但太寬的（舊版留下的 0755）收回來。
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        // `create` 對已經存在的目錄不動權限，所以升級上來的那些要另外收。
        let loose = std::fs::metadata(dir).map(|m| m.permissions().mode() & 0o077 != 0).unwrap_or(false);
        if loose {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// `O_APPEND` 開檔；**建檔當下**就是 0600（不是寫完再 chmod：中間那一瞬是 umask 決定的）。
/// 已經存在的檔案不動權限——它可能正被另一個 hook 行程附加中。
pub fn append_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    o.open(path)
}
