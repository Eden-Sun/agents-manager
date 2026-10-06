//! 使用者的家目錄。正式程式就是 `dirs::home_dir()`；**測試**拿到的是行程專屬的假家目錄。
//!
//! 起因（2026-10-02）：測試啟動 grok bot 時，hook 安裝、預先信任、herdr skill 都寫進 `dirs::home_dir()`——也就是跑測試那個人的
//! 真的 `~/.grok/hooks/agents-manager.json`（指到已經刪掉的 `/tmp/am-test-…/data/grok-hook.sh`）與 `~/.grok/trusted_folders.toml`
//! （累積 150 KB 的測試目錄）。會寫使用者家目錄的程式碼一律走這裡。

use std::path::PathBuf;

pub fn dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        Some(crate::testing::fake_home())
    }
    #[cfg(not(test))]
    {
        dirs::home_dir()
    }
}

/// 沒有 `--config` 也沒有 `AM_DATA_DIR` 時的資料目錄。
pub fn default_dir() -> PathBuf {
    dir().unwrap_or_else(|| PathBuf::from(".")).join(".config/agents-manager")
}

/// `AM_DATA_DIR`（空字串當沒設）。`hook_cmd.rs` 的 spool 也讀同一個變數。
pub fn env_dir() -> anyhow::Result<Option<PathBuf>> {
    std::env::var_os("AM_DATA_DIR")
        .map(PathBuf::from)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| normalize(&d))
        .transpose()
}

fn normalize(p: &std::path::Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    use std::path::Component;
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().context("current dir")?.join(p)
    };
    let mut out = PathBuf::from("/");
    let mut pending = 0usize;
    for comp in abs.components() {
        match comp {
            Component::Prefix(prefix) => out = PathBuf::from(prefix.as_os_str()),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
                pending = pending.saturating_sub(1);
            }
            Component::Normal(name) => {
                out.push(name);
                if pending > 0 {
                    pending += 1;
                    continue;
                }
                match std::fs::symlink_metadata(&out) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => pending = 1,
                    Err(e) => anyhow::bail!("無法解析路徑 {}：{e}", out.display()),
                    Ok(_) => match std::fs::canonicalize(&out) {
                        Ok(real) => out = real,
                        Err(e) => anyhow::bail!(
                            "{} 是解析不了的 symlink（{e}）：拒絕把它當成資料目錄的一段",
                            out.display()
                        ),
                    },
                }
            }
        }
    }
    Ok(out)
}
