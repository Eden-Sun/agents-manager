//! claude 輸入框底下的權限模式列。全 daemon 只認這一份（#788）：
//! capture、screen、child_alerts、tui_prompts 都呼叫 [`is_mode_row`]。
//!
//! bypass／accept edits 是 `⏵⏵ … on (shift+tab to cycle)`，plan 是 `⏸ plan mode on (shift+tab to cycle)`；
//! default 是 `⏸ manual mode on · ← for agents`（2.1.288 沒有 `shift+tab`，#783）。
//! 尾巴（` · ← for agents`、` · ? for shortcuts`、` · 1 shell`）隨狀態在變。

/// 這一行是不是權限模式列。
///
/// `⏵⏵` 開頭一律算。其餘看第一段（` · ` 之前、去掉可選的 `⏸`）：整段以 `(shift+tab to cycle)` 結尾
///（窄 pane 折下來的提示），或完全等於 `manual mode on`／`plan mode on`／`accept edits on`／`bypass permissions on`。
/// 回覆裡中間的 `mode on`、句尾引用 `plan mode on`、句子中間的 `bypass permissions on` 都不是。
/// 沒有箭頭、第一段剛好是 `bypass permissions on` 的也算（`child_alerts` 原本就認這種）。
pub fn is_mode_row(s: &str) -> bool {
    let s = s.trim();
    if s.starts_with("⏵⏵") {
        return true;
    }
    let low = s.to_ascii_lowercase();
    let rest = low.strip_prefix('⏸').unwrap_or(&low);
    let seg = rest.split(" · ").next().unwrap_or(rest).trim();
    if seg.ends_with("(shift+tab to cycle)") {
        return true;
    }
    matches!(seg, "manual mode on" | "plan mode on" | "accept edits on" | "bypass permissions on")
}
