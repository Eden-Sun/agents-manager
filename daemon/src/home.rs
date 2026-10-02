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
