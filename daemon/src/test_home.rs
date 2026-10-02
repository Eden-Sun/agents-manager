//! 測試行程的 `$HOME`（資料安全審查，2026-10-02）。
//!
//! 很多程式碼用 `dirs::home_dir()` 找身分目錄、信任清單、CLI 設定（`~/.claude*`、`~/.claude.json`、`~/.codex`、`~/.grok`）。
//! 測試以前直接用跑測試那個人的真 HOME：這台的 `~/.claude.json` 累積了 3 萬多筆 `/tmp/am-test-…` 專案（6.5 MB，每次 claude
//! 啟動都要讀）、`~/.codex/config.toml` 與 `~/.grok/trusted_folders.toml` 各上千筆，`~/.claude/projects/` 有兩千多個 `am-test-*` 目錄，
//! 更糟的是 `~/.grok/hooks/agents-manager.json`（使用者真的 grok 在用的 hook）被改成指到已經不存在的測試目錄。
//!
//! 所以測試行程**開始前**（`.init_array`／`__mod_init_func`，在 `main` 與任何執行緒之前）就把 `HOME` 換成一個拋棄式目錄，
//! 行程結束時刪掉。測試要驗「寫到哪裡」的，改看 [`dir`]。
//!
//! 守護測試在最下面：HOME 真的換了、預設身分目錄與信任檔真的落在裡面。

use std::sync::OnceLock;

static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();

/// 這個測試行程的拋棄式 HOME。
pub fn dir() -> &'static std::path::Path {
    HOME.get().map(|p| p.as_path()).expect("test_home::redirect 沒跑到（.init_array 沒執行？）")
}

extern "C" fn redirect() {
    // `track`：行程結束時刪掉（同 `testing::scratch_leak_guard` 的規定）。
    let home = crate::testing::track(std::env::temp_dir().join(format!("am-test-home-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&home);
    if std::fs::create_dir_all(&home).is_err() {
        return;
    }
    // SAFETY（執行緒）：在 `main` 之前、單執行緒的時候呼叫。
    std::env::set_var("HOME", &home);
    let _ = HOME.set(home);
}

#[used]
#[cfg_attr(target_os = "linux", link_section = ".init_array")]
#[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
static REDIRECT_HOME: extern "C" fn() = redirect;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LOCAL_HOST;

    /// 測試行程的 HOME 是拋棄式目錄，`dirs::home_dir()` 看得到它。
    #[test]
    fn the_test_process_runs_with_a_throwaway_home() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(home, dir(), "dirs::home_dir() 還是真的 HOME");
        assert!(home.is_absolute(), "{}", home.display());
        assert!(home.file_name().unwrap().to_string_lossy().starts_with("am-test-home-"));
        assert_eq!(std::env::var_os("HOME").as_deref(), Some(home.as_os_str()));
    }

    /// 沒設 `CLAUDE_CONFIG_DIR` 的預設身分目錄、以及預先信任寫的檔，都落在拋棄式 HOME 裡，不是真的 `~`。
    #[tokio::test]
    async fn default_identity_dir_and_trust_stores_never_point_at_the_real_home() {
        let e = crate::testing::env().await;
        let dir_default = crate::lifecycle::identity_config_dir(&e.app, LOCAL_HOST, None).await.unwrap();
        assert!(std::path::Path::new(&dir_default).starts_with(dir()), "{dir_default}");

        let bot = crate::testing::claude_bot(&e.app, &e.project_id, "home-guard").await;
        let cwd = e.dir.join("repo-guard");
        std::fs::create_dir_all(&cwd).unwrap();
        let errors = crate::trust::pretrust_for_start(&e.app, &bot, LOCAL_HOST, cwd.to_str().unwrap()).await;
        assert!(errors.is_empty(), "{errors:?}");
        let written = std::fs::read_to_string(dir().join(".claude.json")).expect("信任檔要寫在拋棄式 HOME 的 .claude.json");
        assert!(written.contains("repo-guard"), "{written}");
    }
}
