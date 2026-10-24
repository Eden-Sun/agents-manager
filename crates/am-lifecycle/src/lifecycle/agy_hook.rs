//! agy 的全域設定（SPEC §agy 支援）：`~/.gemini/config/hooks.json` 的具名 hook ＋ `~/.gemini/antigravity-cli/settings.json` 的 `statusLine`，
//! 全都指向資料目錄裡同一支 dispatcher（`agy-hook.sh <事件>`）。
//!
//! agy 沒有「每次啟動指定 hook」的旗標，設定檔又是使用者（和 Gemini CLI）共用的 `~/.gemini`，所以：
//! * **只動我們那一項**：hooks.json 的 `agents-manager[-<實例>]` 鍵、settings.json 的 `statusLine`（使用者自己設的不碰）；其他全部原樣保留，
//!   讀不懂就不覆寫（回錯，由呼叫端記 warn、bot 照常啟動——hook 沒裝只是少了回報，畫面判讀與 transcript 還在）。
//! * **原子寫入、同一把鎖**：和信任清單共用 `trust::update_file`（settings.json 兩邊都寫）。
//! * **dispatcher 在非 AG Man 的 pane 裡什麼都不做**（沒有 `AM_BOT_ID`／`AM_HOOK_TOKEN`），使用者自己開的 agy 不受影響；而且**永遠 exit 0**，
//!   stdout 只給決策：hook 事件印 `{}`，PreToolUse 放行印空（1.4.7 的 `{}` 是拒絕，見 SPEC agy）。hook 是同步的、會卡住 agent loop，
//!   stdout 會被當成決策（`{"decision":"continue"}` 會擋住停止，所以絕不能印別的）。
//! * 一律走 [`crate::home::dir`]：測試的家目錄是假的，不會碰到真的 `~/.gemini`。

use std::path::{Path, PathBuf};

use crate::hosts::sh_quote;
use crate::agy_support as cfg;

/// dispatcher 的內容。`$1` 是事件名（`SessionStart`／`PreInvocation`／`Stop`／`state`）。
///
/// `state`（statusLine）的 stdout 會被當成狀態列文字，所以只有 hook 事件才印 `{}`；statusLine 什麼都不印（疊在 agy 內建的狀態列下面）。
pub(super) fn dispatch_sh(exe: &str, data_dir: &str, instance: Option<&str>) -> String {
    let gate = match instance {
        Some(slug) => format!("[ \"${{AM_INSTANCE:-}}\" = {} ] || {{ am_reply; exit 0; }}\n", sh_quote(slug)),
        None => "[ -z \"${AM_INSTANCE:-}\" ] || { am_reply; exit 0; }\n".to_string(),
    };
    format!(
        "#!/bin/sh\n\
         # agents-manager agy dispatcher (SPEC agy). Rewritten by the daemon on every agy bot start; no-op outside daemon panes.\n\
         # agy runs this synchronously and reads stdout as a decision: hook events answer `{{}}`, the status line prints nothing.\n\
         # PreToolUse 放行也是空的：1.4.7 把 `{{}}` 判成拒絕（tool call denied by pre-tool hook），空 stdout 才是「沒有決定」。\n\
         AM_EVENT=${{1:-}}\n\
         am_reply() {{ case $AM_EVENT in state|PreToolUse) ;; *) printf '{{}}\\n' ;; esac; }}\n\
         [ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || {{ am_reply; exit 0; }}\n\
         {gate}\
         if [ \"$AM_EVENT\" = PreToolUse ]; then\n\
         # 廣域 kill 守門（#1884）：只有「要擋」時 hook 才印決策 JSON（以 `{{` 開頭），原樣交給 agy；其餘一律 `{{}}`。\n\
         AM_OUT=$({exe} hook agy --bot \"$AM_BOT_ID\" --port \"${{AM_PORT:-7788}}\" --data-dir {data_dir} --event PreToolUse 2>/dev/null) || AM_OUT=\n\
         case $AM_OUT in '{{'*) printf '%s\\n' \"$AM_OUT\"; exit 0 ;; esac\n\
         am_reply; exit 0\n\
         fi\n\
         {exe} hook agy --bot \"$AM_BOT_ID\" --port \"${{AM_PORT:-7788}}\" --data-dir {data_dir} --event \"$AM_EVENT\" >/dev/null || true\n\
         am_reply\n\
         exit 0\n",
        exe = sh_quote(exe),
        data_dir = sh_quote(data_dir),
    )
}

fn dispatcher_text(app: &(impl crate::capabilities::DataDir + crate::capabilities::ExePath + crate::hosts::HostInstance)) -> String {
    dispatch_sh(&app.exe().to_string_lossy(), &app.data_dir().to_string_lossy(), app.instance().as_deref())
}

/// bot 啟動（本機）：裝／修 dispatcher 與兩個設定檔。回「有沒有改到東西」。
pub fn install_local(app: &(impl crate::capabilities::DataDir + crate::capabilities::ExePath + crate::hosts::HostInstance)) -> anyhow::Result<bool> {
    let home = crate::home::dir().ok_or_else(|| anyhow::anyhow!("no home dir; cannot install the agy hooks"))?;
    refuse_real_home(&home)?;
    install_at(app, &home)
}

/// 測試建置只准寫暫存目錄（比照 `daemon/src/testing.rs` 的 `scratch_leak_guard`）：所有 agy bot 共用的真的 `~/.gemini` 不能被測試蓋寫。
/// 正式建置（`TEST_BUILD` 為假）不擋。
pub(super) fn refuse_real_home(home: &Path) -> anyhow::Result<()> {
    if crate::home::TEST_BUILD && !crate::home::under_temp(home) {
        anyhow::bail!("test build refuses to write the real home {}", home.display());
    }
    Ok(())
}

pub fn install_at(app: &(impl crate::capabilities::DataDir + crate::capabilities::ExePath + crate::hosts::HostInstance), home: &Path) -> anyhow::Result<bool> {
    let dispatcher: PathBuf = app.data_dir().join(cfg::DISPATCH_SH);
    let text = dispatcher_text(app);
    let mut changed = false;
    if std::fs::read_to_string(&dispatcher).ok().as_deref() != Some(text.as_str()) {
        super::grok_hook::write_atomic(&dispatcher, &text, 0o700)?;
        changed = true;
    }
    let dispatcher = dispatcher.to_string_lossy().into_owned();
    let name = cfg::hook_name(app.instance().as_deref());
    changed |= crate::trust::update_file(&cfg::hooks_path(home), |existing| cfg::hooks_merge_guarded(existing, &name, &dispatcher))?;
    let statusline = format!("{} state", sh_quote(&dispatcher));
    changed |= crate::trust::update_file(&cfg::settings_path(home), |existing| cfg::statusline_merge(existing, &statusline))?;
    if changed {
        tracing::info!(dispatcher, home = %home.display(), "agy hooks installed");
    }
    Ok(changed)
}
