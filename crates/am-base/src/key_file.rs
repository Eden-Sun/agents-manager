//! API key 檔的讀寫（Jev 與 Claude API 共用；SPEC §4.3c／§4.3d）：600 權限、原子換檔、不回 key。
//! 這裡不認得任何一個用途，誰的 key 檔都能用。

use anyhow::{anyhow, Result};

/// 已寫好、fsync 過、還沒生效的新 key（0600 暫存檔）。`publish` 才原子換掉現行的 key 檔；
/// 沒 publish 就 drop 會把暫存檔刪掉。設定頁「token＋開關」一起存時，先暫存、config 寫成功才 publish，
/// config 失敗就不會留下一把已經生效的新 key（issue #342）。
pub struct StagedKey {
    tmp: std::path::PathBuf,
    dest: std::path::PathBuf,
    done: bool,
}

impl StagedKey {
    /// 暫存檔路徑，給 `key_status` 之類的檢查用（不含 key 內容）。
    pub fn staged_path(&self) -> &std::path::Path {
        &self.tmp
    }

    pub fn publish(mut self) -> Result<()> {
        std::fs::rename(&self.tmp, &self.dest)?;
        self.done = true;
        Ok(())
    }
}

impl Drop for StagedKey {
    fn drop(&mut self) {
        if !self.done {
            std::fs::remove_file(&self.tmp).ok();
        }
    }
}

pub fn stage_key(path: &str, key: &str) -> Result<StagedKey> {
    let key = key.trim();
    if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(anyhow!("key must be one line without spaces"));
    }
    let path = std::path::PathBuf::from(crate::config::expand_home(path, &std::env::var("HOME").unwrap_or_default()));
    let dir = path.parent().ok_or_else(|| anyhow!("key_file has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp", crate::db::ulid()));
    let written = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        let mut f = opts.open(&tmp)?;
        f.write_all(key.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        std::fs::remove_file(&tmp).ok();
        return Err(e);
    }
    Ok(StagedKey { tmp, dest: path, done: false })
}

/// key 檔現在能不能用；不能用的原因（不含 key 本身）。
pub fn key_status(path: &str) -> std::result::Result<(), String> {
    read_key(path).map(|_| ()).map_err(|e| e.to_string())
}

/// 檔案權限對 group／other 開著就拒用：key 不該是別人讀得到的。
pub fn read_key(path: &str) -> Result<String> {
    let path = crate::config::expand_home(path, &std::env::var("HOME").unwrap_or_default());
    let meta = std::fs::metadata(&path).map_err(|e| anyhow!("key file unreadable: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(anyhow!("key file is readable by others; chmod 600 it"));
        }
    }
    let _ = &meta;
    let key = std::fs::read_to_string(&path).map_err(|e| anyhow!("key file unreadable: {e}"))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("key file is empty"));
    }
    Ok(key.to_string())
}
