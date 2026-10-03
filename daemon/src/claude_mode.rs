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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_permission_mode_rows_match_and_a_paused_reply_does_not() {
        for row in [
            "⏸ manual mode on · ← for agents",
            "⏸ manual mode on · ? for shortcuts",
            "⏸ plan mode on (shift+tab to cycle) · ← for agents",
            "⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
            "bypass permissions on · ← for agents",
            "permissions on (shift+tab to cycle)",
            "(Shift+Tab to cycle) · ← for agents",
        ] {
            assert!(is_mode_row(row), "{row}");
        }
        for reply in [
            "⏸ 暫停：等使用者決定 · mode on 的說明",
            "⏸ 暫停部署",
            "⏸ plan mode on the left is still default",
            "⏸ Quoted status: plan mode on",
            "⏸ The manual mode on label is confusing here.",
            "The manual mode on label is confusing here.",
            "我把 bypass permissions on 這個模式關掉了",
        ] {
            assert!(!is_mode_row(reply), "{reply}");
        }
        let fixture = include_str!("lifecycle/fixtures/claude-2.1.288-manual-mode-finished.txt");
        let row = fixture.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
        assert!(is_mode_row(row), "{row}");
    }
}
