//! pane 的 `PATH` 上只能有**自己**那一份 shim（`<data>/bots/<id>/bin`）。
//!
//! 2026-09-18 兩次死鎖的來源：一顆 bot 的 pane 上疊了別顆 bot 的 shim 目錄。兩條路徑都會疊：
//!
//! * daemon 自己的 `PATH` 被複製進 pane env（`lifecycle::setup::pane_env`）——daemon 若是從某顆
//!   bot 的 pane 裡啟動的（部署就是這樣做的），它的 `PATH` 前面就掛著那顆 bot 的 shim；
//! * 子 agent 的 pane 是 `herdr pane split` 出來的，直接繼承父 pane 的 `PATH`（父的 shim 已經在
//!   前面），daemon 再把子自己的 shim 接上去——於是一條 `PATH` 上兩層 shim。
//!
//! 疊起來之後，一次 `cargo build` 會被一層一層的 shim 各拿一個 build slot：`max_concurrent=2`
//! 被自己的外層佔滿，最內層永遠等不到（16:45 與 18:40 各一次）。[`crate::cargo_shim`] 那邊已經
//! 認得出「PATH 上那個 cargo 是同一支 shim」並跳過，但那是下游的保險；這裡是源頭——**別顆 bot 的
//! shim 目錄根本不該出現在這顆 bot 的 `PATH` 上**。

/// 這個 `PATH` 元素是不是某顆 bot 的 shim 目錄。
///
/// 形狀是 `…/bots/<bot id>/bin`（`App::bot_dir` + `install_local`）。資料目錄可以被
/// `AM_DATA_DIR` 換掉，所以只認後面那三段，不寫死 `~/.config/agents-manager`。
pub fn is_bot_shim_dir(entry: &str) -> bool {
    let trimmed = entry.trim_end_matches('/');
    let mut parts = trimmed.rsplit('/');
    let (Some("bin"), Some(id), Some("bots")) = (parts.next(), parts.next(), parts.next()) else { return false };
    !id.is_empty()
}

/// 把 `path` 裡所有 bot shim 目錄拿掉，再把 `own` 放到最前面（只留一份）。
///
/// `own` 自己也先被清掉再放回去：重啟、restart、`start_inner` 可能已經加過一次，留兩份只會讓
/// 除錯的人多看一眼。
pub fn prepend_own_shim_dir(path: &str, own: &str) -> String {
    let own_trimmed = own.trim_end_matches('/');
    let kept: Vec<&str> = path
        .split(':')
        .filter(|e| !e.is_empty())
        .filter(|e| !is_bot_shim_dir(e))
        .filter(|e| e.trim_end_matches('/') != own_trimmed)
        .collect();
    if kept.is_empty() {
        return own.to_string();
    }
    format!("{own}:{}", kept.join(":"))
}

/// 在 pane 的 shell 裡做同一件事的那一行（`lifecycle::start` 會把它打進 pane）。
///
/// 全部走外部指令，不靠 shell 的字串切割：`for d in $PATH` 在 zsh 下**不會**照 `IFS` 拆開
/// （`SH_WORD_SPLIT` 預設是關的），而 pane 的登入 shell 多半就是 zsh。
pub fn pane_export_line(own_quoted: &str) -> String {
    format!(
        " export PATH=\"$(printf %s \"$PATH\" | tr ':' '\\n' | grep -v '/bots/[^/]*/bin$' | tr '\\n' ':' | sed 's/:$//')\"; export PATH={own_quoted}:\"$PATH\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bots_shim_dir_is_recognised_wherever_the_data_dir_is() {
        assert!(is_bot_shim_dir("/Users/m4p/.config/agents-manager/bots/01M2RYE4BQ/bin"));
        assert!(is_bot_shim_dir("/tmp/am-data/bots/01ABC/bin/"));
        // 不是 shim 目錄的一律留著。
        assert!(!is_bot_shim_dir("/opt/homebrew/bin"));
        assert!(!is_bot_shim_dir("/Users/m4p/.cargo/bin"));
        assert!(!is_bot_shim_dir("/x/bots/bin"));
        assert!(!is_bot_shim_dir("/x/bots//bin"));
        assert!(!is_bot_shim_dir(""));
    }

    /// 18:40 那顆 build child 的 `PATH`：自己的 shim 在最前面，別顆 bot 的又疊在中間。
    #[test]
    fn other_bots_shims_are_dropped_and_ours_ends_up_first_exactly_once() {
        let own = "/d/bots/SELF/bin";
        let path = "/d/bots/SELF/bin:/usr/bin:/d/bots/OTHER/bin:/bin:/d/bots/SELF/bin:/d/bots/THIRD/bin";
        assert_eq!(prepend_own_shim_dir(path, own), "/d/bots/SELF/bin:/usr/bin:/bin");

        // 本來就沒有 shim 的 PATH 只是被接上自己那一份。
        assert_eq!(prepend_own_shim_dir("/usr/bin:/bin", own), "/d/bots/SELF/bin:/usr/bin:/bin");
        // 整條都是別人的 shim：清完只剩自己。
        assert_eq!(prepend_own_shim_dir("/d/bots/A/bin:/d/bots/B/bin", own), own);
        assert_eq!(prepend_own_shim_dir("", own), own);
    }

    /// pane 那一行只用外部指令：zsh 不做 `IFS` 切割，用 shell 迴圈寫會在真的 pane 上默默失效。
    #[test]
    fn the_pane_line_filters_with_external_tools_only() {
        let line = pane_export_line("'/d/bots/SELF/bin'");
        assert!(line.contains("tr ':' '\\n'"), "{line}");
        assert!(line.contains("grep -v '/bots/[^/]*/bin$'"), "{line}");
        assert!(line.ends_with("export PATH='/d/bots/SELF/bin':\"$PATH\"\n"), "{line}");
        assert!(!line.contains("for d in"), "不要靠 shell 切字串：{line}");
    }
}
