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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn a_private_dir_is_0700_even_when_it_already_existed_wide_open() {
        let root = std::env::temp_dir().join(format!("am-private-{}", crate::db::ulid()));
        let deep = root.join("bots").join("b1");
        create_private_dir(&deep).unwrap();
        assert_eq!(mode_of(&deep), 0o700, "新建的就是 0700");
        assert_eq!(mode_of(&root.join("bots")), 0o700, "中間層也是");

        std::fs::set_permissions(&deep, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&deep).unwrap();
        assert_eq!(mode_of(&deep), 0o700, "舊版留下的 0755 要收回來");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 建檔當下就要是 0600：先寫再 chmod 的話，中間那一瞬是 umask 決定的。
    #[test]
    fn an_appended_file_is_created_0600() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("am-private-{}", crate::db::ulid()));
        create_private_dir(&dir).unwrap();
        let p = dir.join("spool.jsonl");
        append_private(&p).unwrap().write_all(b"a\n").unwrap();
        assert_eq!(mode_of(&p), 0o600);
        append_private(&p).unwrap().write_all(b"b\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n", "第二次是附加，不是覆蓋");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
