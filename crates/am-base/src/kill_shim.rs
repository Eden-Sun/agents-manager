//! `pkill`／`killall` 的 PATH shim（issue #1884 第二階段）：bot 的 pane 打 `pkill …`／`killall …` 時一律擋下，
//! 訊息與 [`crate::kill_guard`] 的 PreToolUse hook 同一套政策（不呼叫真的 pkill／killall）。
//!
//! 為什麼要有 shim：PreToolUse hook 只蓋得到有接線的 CLI 的本機 bot（子 claude、遠端、codex、grok 都沒有），
//! 而 PATH 上的 shim 子 pane 繼承、遠端也有；不必解析指令字串。
//!
//! 內容是**一支 POSIX sh**（macOS 的 `/bin/sh` 與 Linux 的 dash 都跑得動；不呼叫 daemon、不連網路），
//! `pkill`、`killall` 兩個檔名同一份內容，以 `basename $0` 分派。替代做法（[`HOW_TO`]）與退出碼從 Rust 常數代入（[`shim_sh`]）。
//!
//! 限制（SPEC §4.4c）：`kill` 是 shell 內建，shim 擋不到 `kill -9 -1`／`kill 0`（留給 PreToolUse）；
//! `/usr/bin/pkill` 這種絕對路徑會繞過 shim。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// shim 的檔名（`pkill`、`killall` 同一份內容）。
pub const NAMES: [&str; 2] = ["pkill", "killall"];

/// 退出碼：被擋。不是 0／1——1 是 pkill 的「沒有行程符合」，腳本會把它當成正常。
pub const DENIED_EXIT: i32 = 2;

const TEMPLATE: &str = r####"#!/bin/sh
# agents-manager pkill/killall guard shim (issue #1884). Installed at the front of a managed pane's PATH,
# next to the herdr and cargo shims; the same file is installed as `pkill` and as `killall`.
# Policy (SPEC §4.4c): both tools are always denied; this shim never execs the real tool.
#
# AM_SHIM_MARKER: 這行讓其他 shim（例如 cargo）認得出「PATH 上那個 pkill 其實是 shim」。不要刪。

AM_HOW_TO='@AM_HOW_TO@'
AM_TOOL=$(basename "$0")
case $AM_TOOL in
    pkill) AM_WHY='pkill 一律不准用（不論 -f、-P、-x 或比對字串；窄的比對也會命中自己的 shell 或別的 agent）。' ;;
    killall) AM_WHY='killall 一律不准用（會殺掉同名的全部行程，包含其他 agent）。' ;;
    *) AM_WHY='一律不准用。' ;;
esac
printf 'agents-manager: 擋下 %s：%s\n%s\n' "$AM_TOOL" "$AM_WHY" "$AM_HOW_TO" >&2
exit @AM_DENIED_EXIT@
"####;

/// 代入 `kill_guard::HOW_TO` 與退出碼後的 shim 內容（`pkill`、`killall` 共用）。
pub fn shim_sh() -> &'static str {
    static SH: OnceLock<String> = OnceLock::new();
    SH.get_or_init(|| {
        debug_assert!(!crate::kill_guard::HOW_TO.contains('\''), "代入單引號字串的值不能含單引號");
        TEMPLATE.replace("@AM_HOW_TO@", crate::kill_guard::HOW_TO).replace("@AM_DENIED_EXIT@", &DENIED_EXIT.to_string())
    })
}

/// 本機：`<bot_dir>/bin/pkill`、`bin/killall`（原子寫入、內容一樣不重寫，跟其他 shim 一致）。
pub fn install_local(bot_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = crate::shim_refresh::bin_dir(bot_dir);
    std::fs::create_dir_all(&dir)?;
    for name in NAMES {
        crate::shim_refresh::write_atomic(&dir.join(name), shim_sh())?;
    }
    Ok(dir)
}

/// 遠端：同一支原子同步腳本（issue #124）。
pub async fn install_remote(conn: &crate::hosts::HostConn, remote_bot_dir: &str) -> anyhow::Result<String> {
    let r = crate::shim_refresh::sync_remote(conn, &[remote_bot_dir.to_string()], &NAMES, true).await?;
    if !r.failed.is_empty() {
        anyhow::bail!("remote kill shim install failed: {:?}", r.failed);
    }
    Ok(format!("{remote_bot_dir}/bin"))
}
