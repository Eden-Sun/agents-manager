use super::{Capture, ACTIVITY_MAX};

pub static PARSER: ClaudeCapture = ClaudeCapture;

pub struct ClaudeCapture;

impl Capture for ClaudeCapture {
    fn still_busy(&self, screen: &str) -> bool {
        let lines: Vec<&str> = screen.lines().collect();
        if let Some(cut) = composer_top(&lines) {
            let zone = status_zone_start(&lines, cut);
            return lines[zone..cut].iter().any(|l| is_zone_busy(l.trim()));
        }
        // 沒有輸入框（測試給的單行 spinner）：只認活動列形狀。`·`／`*` 要有 `(… tokens)` 那種形狀才算。
        lines.iter().any(|l| {
            let s = l.trim();
            is_live_spinner(s) || is_legacy_spinner(s)
        })
    }

    fn awaits_input(&self, screen: &str) -> bool {
        screen.lines().rev().take(12).any(|l| {
            // claude 2.1.285 的空框是 `❯` 接 U+00A0（2026-10-01 cf-ox-2）：空白一律 `is_whitespace`，不只空格與 tab。
            let mut chars = l.chars().filter(|c| !c.is_whitespace() && !"│┃╭╮╰╯─━".contains(*c));
            chars.next() == Some('❯') && chars.next().is_none()
        })
    }

    fn extract_reply(&self, text: &str) -> Option<String> {
        let marker = "⏺ ";
        let text = &dot_marker_as_record(text);
        let lines: Vec<&str> = text.lines().collect();
        let after_echo = after_last_prompt_echo(&lines);
        let last_marker = after_echo
            + lines[after_echo..]
                .iter()
                .rposition(|l| l.trim_start().starts_with(marker))?;
        // 回合結束時畫面最底的 `⏺` 常是工具呼叫（issue #753）：回覆是它上面最後一段文字，而且收在下一個工具呼叫之前。
        // 整回合只有工具呼叫（被中斷）就沒有更好的可取，照舊取最後一個 `⏺`。
        let last_text = (after_echo..=last_marker).rfind(|&i| lines[i].trim_start().starts_with(marker) && !is_tool_call_row(&lines, i));
        let start = last_text.unwrap_or(last_marker);
        let mut out: Vec<String> = Vec::new();
        // 回覆收到輸入框上緣為止。框的位置用真畫面：規則線夾著 `❯`（`claude-2.1.281-feedback-survey.txt`）。
        // 緊貼上緣的那幾行是狀態列（spinner／done），不是回覆。框以上的 `│`、`---`、⚠ 都是內容。
        let found_cut = composer_top(&lines).filter(|i| *i > start);
        // 打 slash 指令時框上緣貼著建議清單（2.1.290 起選中列是 `  ❯ /cmd  說明`，#875）：不是回覆，跟狀態列一樣切掉。
        let cut = found_cut.map_or(lines.len(), |c| suggestion_list_top(&lines, c).max(start + 1));
        let keep_until = status_zone_start(&lines, cut);
        for (i, line) in lines.iter().enumerate().take(keep_until).skip(start) {
            let t = line.trim_end();
            let s = t.trim_start();
            // 找不到輸入框（舊的圓角框 UI）時，框的上緣 `╭` 是回覆的終點。找得到就一路收到狀態列：助手自己畫的
            // 圓角方框圖（`╭──╮`／`╰──╯`）是內容，不能在第一個 `╭` 截掉（codex 那條同一個理由，`codex_reply_keeps_mermaid_box_drawing_lines`）。
            if found_cut.is_none() && (s.starts_with('╭') || s.starts_with('╰') || s.starts_with('▔')) {
                break;
            }
            if last_text.is_some() && s.starts_with(marker) && is_tool_call_row(&lines, i) {
                break;
            }
            if is_update_banner(s) || is_agents_md_notice(s) {
                continue;
            }
            let cleaned = s.strip_prefix(marker).unwrap_or(t).to_string();
            out.push(cleaned);
        }
        while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
            out.pop();
        }
        let joined = out.join("\n").trim().to_string();
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    }

    fn noise_line(&self, s: &str) -> bool {
        is_noise(s)
    }

    fn activity(&self, text: &str) -> Option<String> {
        let lines: Vec<&str> = text.lines().collect();
        let start = after_last_prompt_echo(&lines);
        let mut found: Option<String> = None;
        for line in &lines[start..] {
            let s = line.trim();
            let Some(first) = s.chars().next() else {
                continue;
            };
            let rest = if spinner_led(s) {
                s[first.len_utf8()..].trim()
            } else if super::is_activity_shape(s) {
                if first.is_alphanumeric() {
                    s
                } else {
                    s[first.len_utf8()..].trim()
                }
            } else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            found = Some(rest.to_string());
        }
        let s = found?;
        if s.chars().count() <= ACTIVITY_MAX {
            return Some(s);
        }
        let mut cut: String = s
            .chars()
            .take(ACTIVITY_MAX)
            .collect::<String>()
            .trim_end()
            .to_string();
        cut.push('…');
        Some(cut)
    }
}

/// claude 在 Linux 上把回覆／工具列的字頭畫成 `●`（U+25CF），macOS 才是 `⏺`（U+23FA）（2.1.287 Linux 真畫面
/// `claude-2.1.287-linux-*.txt`）。字頭一律在第 0 欄；縮排的 `●`（問卷、回覆裡自己寫的項目符號）不是字頭。
/// 後面的解析都認 `⏺`，進來先把第 0 欄的 `● ` 換過去。
fn dot_marker_as_record(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.lines().any(|l| l.starts_with("● ")) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match line.strip_prefix("● ") {
            Some(rest) => {
                out.push_str("⏺ ");
                out.push_str(rest);
            }
            None => out.push_str(line),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// `⏺` 開頭的這一行是工具呼叫（`⏺ Bash(herdr agent get …)`），不是助手的文字。認結構不認工具名：
/// 名字加 `(`、整行收在 `)`（長參數被畫成 `…)`），或下面第一行非空的就是 `⎿` 輸出。
/// 多行的命令（真畫面 `claude-2.1.281-background-shell.txt`）第一行不收在 `)`、下面也是命令的續行：
/// 名字加 `(` 開頭的，跳過續行（不含下一個 `⏺`）後接著 `⎿` 也算。
/// 回覆文字自己寫出 `parse(input) 會回傳…` 這幾個條件都不中。
fn is_tool_call_row(lines: &[&str], i: usize) -> bool {
    let s = lines[i].trim();
    let body = s.strip_prefix("⏺").unwrap_or(s).trim_start();
    let opens_call = body.split_once('(').is_some_and(|(name, _)| {
        name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
    });
    if opens_call && body.ends_with(')') {
        return true;
    }
    let mut rest = lines[i + 1..].iter().map(|l| l.trim());
    if opens_call {
        return rest.take_while(|l| !l.starts_with('⏺')).any(|l| l.starts_with('⎿'));
    }
    rest.find(|l| !l.is_empty()).is_some_and(|l| l.starts_with('⎿'))
}

/// 這一行是 spinner 開頭嗎。`*` 也是 spinner 的一格，但同時是回覆裡的 markdown 項目、程式碼區塊的 ` * 註解`：
/// `*` 開頭要有 spinner 該有的 `…` 才算（其他字頭照舊一律算，完成行沒有 `…`；`·` 不動：真畫面的 `· Run in another terminal…` 提示行靠它剝掉）。
fn spinner_led(s: &str) -> bool {
    match s.trim_start().chars().next() {
        Some(c) if is_spinner_glyph(c) => c != '*' || s.contains('…'),
        _ => false,
    }
}

pub fn is_spinner_glyph(c: char) -> bool {
    "✻✽✶✳✢✣✤✥✦✧✩✪✫✬✭✮✯✰✱✲✴✵✷✸✹✺✻✼✾❋·∗*".contains(c) || ('\u{2800}'..='\u{28FF}').contains(&c)
}

/// 輸入框上緣。只接受畫面底部那個框：`❯` 後面只剩規則線與狀態列
/// （`claude-2.1.281-feedback-survey.txt`：`────` / `❯` / `────` / 狀態列 / `⏵⏵`）。
/// 回覆上面的 `❯ 使用者句子` 後面還有內容，不是框。
fn composer_top(lines: &[&str]) -> Option<usize> {
    let prompt_at = lines.iter().rposition(|l| l.trim_start().starts_with('❯'))?;
    let footer = lines[prompt_at + 1..].iter().all(|l| {
        let s = l.trim();
        s.is_empty() || is_full_rule(s) || is_status_chrome(s)
    });
    if !footer {
        return None;
    }
    let mut i = prompt_at;
    while i > 0 && lines[i - 1].trim().is_empty() {
        i -= 1;
    }
    if i > 0 && is_full_rule(lines[i - 1]) {
        return Some(i - 1);
    }
    Some(prompt_at)
}

fn is_full_rule(s: &str) -> bool {
    let s = s.trim();
    s.chars().count() >= 3
        && s.chars()
            .all(|c| c == '─' || c == '━' || c == '-' || c == '=' || c == '_')
}

/// 緊貼輸入框上緣 `cut` 的 `/` 建議清單從哪一列開始；沒有清單就是 `cut`。
/// 真畫面（`claude-2.1.290-slash-*-suggestions.ansi`）：選中列縮排兩格 `  ❯ /rewind      說明`，其餘列縮排四格
/// `    /mobile      說明`，說明太長折到下一列（縮排更深）。只認整塊都是這個形狀、而且有選中列的——
/// 回覆裡縮排的文字不會剛好貼著框、又帶 `  ❯ /` 開頭的列。
fn suggestion_list_top(lines: &[&str], cut: usize) -> usize {
    let entry = |rest: &str| {
        rest.starts_with('/') && rest.split_once("  ").is_some_and(|(cmd, desc)| !cmd.contains(' ') && !desc.trim().is_empty())
    };
    let selected = |l: &str| l.strip_prefix("  ❯ ").is_some_and(entry);
    let other = |l: &str| l.strip_prefix("    ").is_some_and(|r| entry(r) || (r.starts_with("  ") && !r.trim().is_empty()));
    let mut top = cut;
    while top > 0 && (selected(lines[top - 1]) || other(lines[top - 1])) {
        top -= 1;
    }
    if lines[top..cut].iter().any(|l| selected(l)) { top } else { cut }
}

/// 緊貼輸入框上緣、中間沒有空行的狀態列。空行以上是回覆，不掃。
fn status_zone_start(lines: &[&str], cut: usize) -> usize {
    let mut zone = cut;
    // 真畫面的活動列／完成行跟輸入框上緣之間隔著空行（接在長對話後面是一行，短對話輸入框釘在畫面底、中間是一整段空白：
    // `claude-2.1.287-linux-working.txt`、`claude-2.1.281-background-shell.txt`）：這一段空白只容許一次，而且它上面要真的是狀態列；
    // 再往上的空行還是回覆的界線。
    let mut spacer_used = false;
    while zone > 0 {
        let s = lines[zone - 1].trim();
        if s.is_empty() {
            if spacer_used {
                break;
            }
            let mut first_blank = zone - 1;
            while first_blank > 0 && lines[first_blank - 1].trim().is_empty() {
                first_blank -= 1;
            }
            let above_is_status = first_blank > 0 && {
                let a = lines[first_blank - 1].trim();
                is_status_chrome(a) || is_zone_busy(a)
            };
            if !above_is_status {
                break;
            }
            spacer_used = true;
            zone = first_blank;
            continue;
        }
        if !(is_status_chrome(s) || is_zone_busy(s)) {
            break;
        }
        zone -= 1;
    }
    zone
}

fn is_done_row(s: &str) -> bool {
    let Some(c) = s.chars().next() else {
        return false;
    };
    is_spinner_glyph(c) && (s.contains("· done") || s.contains(" for "))
}

fn is_status_chrome(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    super::is_activity_shape(s)
        || is_done_row(s)
        || is_update_banner(s)
        || is_mode_row(s)
        || s.starts_with("Tip:")
        || s.starts_with("⎿")
        || s.contains("Auto-update failed")
        || (s.contains(" | ") && (s.contains("5h:") || s.contains("7d:")))
        || (s.contains(" · ") && s.contains("% left"))
}

/// 輸入框底下的權限模式列。判斷只有 [`crate::claude_mode::is_mode_row`] 一份（#788）。
pub(crate) fn is_mode_row(s: &str) -> bool {
    crate::claude_mode::is_mode_row(s)
}

fn is_zone_busy(s: &str) -> bool {
    is_live_spinner(s) || is_legacy_spinner(s)
}

/// 還在跑的活動列：`<Verb>… (3s · … tokens)`。`esc to interrupt` 單獨出現不算
/// （codex 的 `• Working (🤖 • esc to interrupt)` 不是經過時間，不能在這裡判忙）。
fn is_live_spinner(s: &str) -> bool {
    super::is_activity_shape(s)
}

/// 沒有輸入框時的舊 spinner（`⠦ Thinking… 52s`）。`·` 與 `*` 留給 [`is_live_spinner`] 的括號形狀。
fn is_legacy_spinner(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == '·' || first == '*' || first == '∗' || !is_spinner_glyph(first) {
        return false;
    }
    let rest = chars.as_str().trim_start();
    let Some(verb) = rest.split_whitespace().next() else {
        return false;
    };
    verb.ends_with('…') && !rest.contains("· done") && !rest.contains(" for ")
}

pub fn is_tool_progress(reply: &str) -> bool {
    let lines: Vec<&str> = reply
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    let running = |l: &str| l.starts_with("Running ") && l.ends_with('…');
    let noise = |l: &str| running(l) || l.starts_with("Tip: ") || l.contains("esc to interrupt");
    lines.iter().any(|l| running(l)) && lines.iter().all(|l| noise(l))
}

fn after_last_prompt_echo(lines: &[&str]) -> usize {
    prompt_echo_row(lines).map(|i| i + 1).unwrap_or(0)
}

/// 使用者這回合的回音行（`❯ <內容>`）在 `lines` 裡的位置（issue #762）。
///
/// claude 把使用者訊息的 `❯ ` 印在**第 0 欄**（真畫面 `claude-2.1.281-dangerous-rm.txt`、`claude-2.1.286-*` 的回音都是頂格、
/// ASCII 空白），續行才縮排兩格；助手回覆（`⏺` 之下）、使用者貼上的內文、工具輸出、排隊中的訊息一律縮排，
/// 所以其中引用的 `  ❯ npm test`、`  ❯ Switch to O…` 不是回音（`claude_live.rs` 認 `/model` 指令行也是同一條）。
/// 取**最後一個第 0 欄**的，縮排的一律不算：畫面上沒有第 0 欄回音（捲出去、輸入框裡的草稿用的是 `❯`＋U+00A0）就是「沒有回音」，
/// 呼叫端從頭看，不會把引用行當起點。內容要有字：`❯ ` 後面空的是輸入框，不是回音。
/// `lifecycle/screen.rs` 的 claude 分支共用這一支，不各寫一份。
pub(crate) fn prompt_echo_row(lines: &[&str]) -> Option<usize> {
    lines.iter().rposition(|l| l.strip_prefix("❯ ").is_some_and(|rest| !rest.trim().is_empty()))
}

fn is_codex_idle_prompt(s: &str) -> bool {
    let t = s.trim_start();
    let body = t.strip_prefix("› ").unwrap_or(t).trim();
    body.to_ascii_lowercase().starts_with("ask codex to do")
}

fn codex_usage_notice_line(line: &str) -> Option<String> {
    let body = line
        .trim()
        .strip_prefix('•')
        .or_else(|| line.trim().strip_prefix('■'))
        .map(str::trim_start)
        .unwrap_or_else(|| line.trim());
    let lower = body.to_ascii_lowercase();
    if lower.starts_with("you have ")
        && lower.contains("usage limit reset")
        && lower.contains("available")
        && lower.contains("run /usage")
    {
        Some(body.to_string())
    } else {
        None
    }
}

/// claude 在輸入框上方靠右印的版本列：`current: 2.1.276 · latest: 2.1.277 ✔ Update installed · Restart to update`
/// （也可能只有 `current … · latest …`，或只剩 `✔ Update installed …`）。回合結束後它還在畫面上，
/// 終端備援抓回覆時會被當成回覆的最後一行（2026-09-19 使用者截圖，AM-1-XH-2）。
/// 只認整行就是這條版本列，回覆裡**提到**這句話的不算。
fn is_update_banner(s: &str) -> bool {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("current: ") {
        let Some((cur, after)) = rest.split_once(" · latest: ") else { return false };
        let latest = after.split_whitespace().next().unwrap_or("");
        let tail = after[latest.len()..].trim();
        let is_ver = |v: &str| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c));
        return is_ver(cur) && is_ver(latest) && (tail.is_empty() || tail.starts_with('✔'));
    }
    s.starts_with("✔ Update installed")
}

fn is_noise(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.chars().next().unwrap_or(' ');
    if "▐▝▛▜█╭╮╰╯│▔⏵⚠✗✘".contains(first) {
        return true;
    }
    if spinner_led(s) {
        return true;
    }
    if s.contains("Auto-update failed") {
        return true;
    }
    if is_update_banner(s) || is_mode_row(s) {
        return true;
    }
    if s.chars()
        .all(|c| c == '─' || c == '━' || c == '-' || c == '=' || c == '_' || c == ' ')
    {
        return true;
    }
    if (s.contains(" | ") && (s.contains("5h:") || s.contains("7d:")))
        || (s.contains(" · ") && s.contains("% left"))
    {
        return true;
    }
    s.starts_with("Claude Code v")
        || s.starts_with("Tip:")
        || s.starts_with("Ask Codex")
        || s.contains("OpenAI Codex (v")
        || s.starts_with(">_ OpenAI Codex")
        || s.contains("Ask Codex to do")
        || s.contains("autocompletes slash commands")
        || s.contains("/model to change")
        || s.starts_with("directory:")
        || s.starts_with("permissions: YOLO")
        || (s.contains("Context ") && s.contains("% used"))
        || is_codex_idle_prompt(s)
        || codex_usage_notice_line(s).is_some()
        || is_agents_md_notice(s)
}

/// claude `agents-md` plugin 提示（issue #212 真機 2.1.277／2.1.278）：改讀 AGENTS.md 時畫在回覆槽，
/// 行首是 `⏺ agents-md: no CLAUDE.md found; AGENTS.md loaded: …`（跟助手回覆同一個 `⏺`）。
/// 2.1.276 的通知列 toast 這次沒重抓，舊字串仍當雜訊。
fn is_agents_md_notice(s: &str) -> bool {
    let t = s.trim_start().strip_prefix("⏺ ").unwrap_or(s.trim_start()).trim_start();
    t.starts_with("agents-md: no CLAUDE.md found; AGENTS.md loaded:")
        || t.starts_with("no CLAUDE.md found; AGENTS.md loaded:")
        || t.starts_with("This project has AGENTS.md but no CLAUDE.md")
}

#[cfg(test)]
mod loose_noise_tests {
    use super::*;

    /// #330：回覆裡剛好有「 · 」和 `left` 兩個字的一行（清單、進度）被當成 codex 狀態列剝掉——回覆少一行。
    #[test]
    fn a_reply_row_with_a_dot_and_the_word_left_is_not_chrome() {
        assert!(!is_noise("還剩 · 3 tasks left"));
        assert!(!is_noise("Time left · about 5 minutes"));
        let screen = "❯ 進度？\n⏺ 目前狀況：\n  build done · 3 tasks left\n  下一步跑測試\n";
        let reply = ClaudeCapture.extract_reply(screen).unwrap();
        assert!(reply.contains("3 tasks left"), "回覆中間那行不能被剝掉：{reply}");
        // 真的狀態列照舊是雜訊。
        assert!(is_noise("gpt-5.6-sol high · ~/p · Context 3% used · 5h 82% left · weekly 97% left"));
        assert!(is_noise("· 5h 82% left · weekly 97% left"));
    }

    /// #783：四種權限模式列（2.1.288 真機 shift+tab 輪一圈）都是輸入框底下的 chrome；回覆裡 `⏸` 開頭的句子不是。
    #[test]
    fn every_permission_mode_row_is_chrome_but_a_paused_reply_row_is_not() {
        for row in [
            "⏸ manual mode on · ← for agents",
            "⏸ manual mode on · ? for shortcuts",
            "⏸ plan mode on (shift+tab to cycle) · ← for agents",
            "⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
        ] {
            assert!(is_status_chrome(row) && is_noise(row), "{row}");
        }
        assert!(!is_mode_row("⏸ 暫停：等使用者決定 · mode on 的說明"));
        assert!(!is_noise("⏸ 暫停部署"));
        assert!(!is_noise("⏸ The manual mode on label is confusing here."));
        // 第一段要「以 mode on 結尾」（可再接 shift+tab），不是中間出現這幾個字。
        // `contains` 會把回覆「⏸ plan mode on the left…」整行當 chrome 剝掉。
        assert!(!is_mode_row("⏸ plan mode on the left is still default"));
        assert!(!is_noise("⏸ plan mode on the left is still default"));
        let quoted_status = "⏸ Quoted status: plan mode on";
        assert!(!is_mode_row(quoted_status), "句尾引用 mode on 也不是狀態列");
        assert!(!is_noise(quoted_status), "句尾引用 mode on 也不能當 chrome 剝除");
        let screen = "❯ 狀態？\n⏺ 畫面底下寫著：\n  ⏸ plan mode on the left is still default\n  下一步繼續\n";
        let reply = ClaudeCapture.extract_reply(screen).unwrap();
        assert!(reply.contains("plan mode on the left is still default"), "回覆提到 mode on 不能被剝：{reply}");

        let quoted_screen = "❯ 狀態？\n⏺ 引用了舊狀態列：\n  ⏸ Quoted status: plan mode on\n  下一步繼續\n";
        let quoted_reply = ClaudeCapture.extract_reply(quoted_screen).unwrap();
        assert!(quoted_reply.contains(quoted_status), "以 mode on 結尾的引用也不能被剝：{quoted_reply}");
    }

    /// #788：模式列只有 [`is_mode_row`] 一份，child_alerts 也呼叫它。四種模式（bypass／accept edits／plan／default manual）
    /// 連同各種尾巴都認；`child_alerts` 原本認的形式（沒有 `⏵⏵` 的 `bypass permissions on`、只剩 `shift+tab to cycle`
    /// 的折行、大小寫不同）不能退步。暫停中的回覆列、句子中間提到模式名稱的回覆都不是模式列。
    #[test]
    fn the_one_mode_row_check_covers_all_four_modes_and_not_a_paused_reply() {
        for row in [
            "⏵⏵ bypass permissions on (shift+tab to cycle)",
            "⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
            "⏵⏵ bypass permissions on · 1 shell · ← for agents",
            "  ⏵⏵ bypass permissions on  ",
            "⏵⏵ accept edits on (shift+tab to cycle) · ← for agents",
            "⏵⏵ accept edits on · ? for shortcuts",
            "⏸ plan mode on (shift+tab to cycle) · ← for agents",
            "⏸ plan mode on · ? for shortcuts",
            "⏸ manual mode on · ← for agents",
            "⏸ manual mode on · ? for shortcuts",
            "⏸ manual mode on",
            // child_alerts 原本就認的：沒有箭頭的 bypass 列、窄 pane 折下來的提示、大小寫。
            "bypass permissions on · ← for agents",
            "permissions on (shift+tab to cycle)",
            "(Shift+Tab to cycle) · ← for agents",
        ] {
            assert!(is_mode_row(row), "{row}");
        }
        for reply in [
            "⏸ 暫停：等使用者決定 · mode on 的說明",
            "⏸ 暫停部署",
            "⏸ Paused: waiting for the user · manual mode on is the default",
            "⏸ The manual mode on label is confusing here.",
            "我把 bypass permissions on 這個模式關掉了",
            "manual mode on 是 2.1.288 的預設",
            "",
        ] {
            assert!(!is_mode_row(reply), "{reply}");
        }
        // 2.1.288 default 模式真畫面：最底那一行就是模式列。
        let fixture = include_str!("../lifecycle/fixtures/claude-2.1.288-manual-mode-finished.txt");
        let last = fixture.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
        assert!(is_mode_row(last), "{last:?}");
    }

    /// #331：`*` 開頭的回覆行（markdown 項目、程式碼區塊的 ` * 註解`）被當 spinner 剝掉。
    #[test]
    fn a_reply_row_starting_with_star_or_dot_is_not_a_spinner() {
        assert!(!is_noise("* 第一點"));
        assert!(!is_noise("* Returns the count of items"));
        let screen = "❯ 寫註解\n⏺ 這樣：\n  /**\n   * Returns the count of items\n   */\n  fn count() {}\n";
        let reply = ClaudeCapture.extract_reply(screen).unwrap();
        assert!(reply.contains("* Returns the count of items"), "程式碼區塊的註解行不能被剝：{reply}");
        // 真的 spinner 行照舊是雜訊。
        assert!(is_noise("* Cooking… (3s · ↓ 1.0k tokens)"));
        assert!(is_noise("· Philosophising… (33m 33s · ↓ 94.9k tokens)"));
        assert!(is_noise("✻ Crunched for 9s · done 11:35 PM"));
    }
}

#[cfg(test)]
mod reply_boundary_tests {
    use super::*;

    /// 2.1.x 輸入框（`claude-2.1.281-feedback-survey.txt`）：規則線夾著空的 `❯`，底下才是狀態列。
    const COMPOSER: &str = "\
────────────────────────────────
❯
────────────────────────────────
  hunta | survey-cwd | HAI4.5 | 5h:80% | 7d:70%
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
";

    /// #661：markdown 表格的 `│` 列與回覆裡的 `---` 不是輸入框上緣。
    #[test]
    fn a_markdown_table_and_a_horizontal_rule_stay_in_the_reply() {
        let screen = format!(
            "❯ 分支狀態？\n⏺ 以下是分支狀態：\n\n  ┌──────────┬────────┐\n  │ 分支     │ 狀態   │\n  │ feat/a   │ 過期   │\n  └──────────┴────────┘\n\n  第一部分：結論\n  ---\n  第二部分：細節很重要\n建議刪掉 feat/a。\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("│ feat/a   │ 過期   │"), "表格列被截掉：{reply}");
        assert!(reply.contains("第二部分：細節很重要"), "水平線後面被截掉：{reply}");
        assert!(reply.contains("建議刪掉 feat/a。"), "結論被截掉：{reply}");
        assert!(!reply.contains("bypass permissions"), "輸入框以下的 chrome 不能進回覆：{reply}");
        assert!(!reply.contains("5h:"), "狀態列不能進回覆：{reply}");
    }

    /// #662：回覆區塊裡的警告符號與 `Tip:` 是內容。chrome 只認輸入框底下的真狀態列。
    #[test]
    fn warning_lines_inside_the_reply_are_kept() {
        let screen = format!(
            "❯ 可以 force push 嗎？\n⏺ 可以，但注意：\n  ⚠️ 這會刪掉所有未推送的 commit\n  Tip: 先備份\n  ✗ 不要用 --force\n  ✘ 遠端也會被改寫\n  ⏵ 先看 git status\n  其餘沒問題。\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("⚠️ 這會刪掉所有未推送的 commit"), "{reply}");
        assert!(reply.contains("Tip: 先備份"), "{reply}");
        assert!(reply.contains("✗ 不要用 --force"), "{reply}");
        assert!(reply.contains("✘ 遠端也會被改寫"), "{reply}");
        assert!(reply.contains("⏵ 先看 git status"), "{reply}");
        assert!(reply.contains("其餘沒問題。"), "{reply}");
        assert!(!reply.contains("bypass permissions"), "{reply}");
    }

    /// #663：舊回覆裡的 `·`／`*` 不是 spinner。活的 spinner 只認輸入框正上方的活動列。
    #[test]
    fn an_old_dot_line_above_a_finished_reply_is_not_busy() {
        let screen = format!(
            "❯ 下載？\n⏺ 先前：\n  · 下載中… 還沒好\n  * Loading…\n  已經好了。\n{COMPOSER}"
        );
        assert!(!ClaudeCapture.still_busy(&screen), "舊回覆的項目符號被當成還在跑");
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("· 下載中… 還沒好"), "{reply}");
        assert!(reply.contains("已經好了。"), "{reply}");

        let busy = format!("❯ 下載？\n⏺ 開始了\n· Philosophising… (33m 33s · ↓ 94.9k tokens)\n{COMPOSER}");
        assert!(ClaudeCapture.still_busy(&busy), "輸入框正上方的活動列應該算還在跑");
        let baking = "❯ Reply with PONG\n✢ Baking… (3s · esc to interrupt)\n──────\n❯\n";
        assert!(ClaudeCapture.still_busy(baking));
        // codex 的中斷提示沒有經過時間，不是 claude 的活動列。
        assert!(!ClaudeCapture.still_busy("• Working (🤖 • esc to interrupt)"));
    }

    /// issue #753：回合最後一段是工具呼叫（`⏺ Bash(…)`＋它的 `⎿` 輸出）時，備援以前抓「畫面上最後一個 `⏺`」，
    /// 存成回覆的是工具呼叫原文，真正的文字回答在上面一段、整段沒進網頁。取最後一段**文字**，並收在下一個工具呼叫之前。
    #[test]
    fn the_last_text_block_wins_over_a_trailing_tool_call() {
        let screen = format!(
            "❯ 現在部 demo\n⏺ Bash(herdr agent get cf-1)\n  ⎿  status: working\n\n⏺ 已經交給 memleak 排查，\n  先看這幾點。\n\n⏺ Bash(herdr agent get cf-2 --json)\n  ⎿  {{\"status\": \"idle\"}}\n     … +3 lines (ctrl+o to expand)\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert_eq!(reply, "已經交給 memleak 排查，\n  先看這幾點。", "{reply}");
    }

    /// 最後一段就是文字時照舊（前面的工具呼叫與它們的輸出不進回覆）；收在工具呼叫之前的規則不影響它。
    #[test]
    fn a_text_block_after_tool_calls_is_still_the_reply() {
        let screen = format!(
            "❯ 看一下\n⏺ Read(src/main.rs)\n  ⎿  Read 120 lines\n\n⏺ Update(src/main.rs)\n  ⎿  Updated\n\n⏺ 改好了，三處。\n{COMPOSER}"
        );
        assert_eq!(ClaudeCapture.extract_reply(&screen).unwrap(), "改好了，三處。");
    }

    /// 這一回合只有工具呼叫（沒有任何文字段，例如被中斷）：沒有更好的可取，維持原本的行為。
    #[test]
    fn a_turn_with_only_tool_calls_keeps_the_last_one() {
        let screen = format!("❯ 跑\n⏺ Bash(sleep 60)\n  ⎿  Running…\n{COMPOSER}");
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.starts_with("Bash(sleep 60)"), "{reply}");
    }

    /// 真畫面（`claude-2.1.281-background-shell.txt`）：多行的 Bash 呼叫第一行沒有收在 `)`、下面第一行也不是 `⎿`
    /// （是命令的第二行）。這種工具呼叫要當工具呼叫，回覆仍是它上面最後一段文字。
    #[test]
    fn a_multiline_tool_call_at_the_bottom_is_still_a_tool_call() {
        let screen = format!(
            "❯ 提交\n⏺ 好，我來提交。\n\n⏺ Bash(SP=/private/tmp/claude-501/x/scratchpad\n      python3 - <<'EOF'…)\n  ⎿  daemon/src/config.rs:262: 內容\n     … +6 lines (ctrl+o to expand)\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert_eq!(reply, "好，我來提交。", "{reply}");
    }

    /// 助手畫的圓角方框圖（`╭──╮`／`╰──╯`，LLM 很愛用）是回覆內容：輸入框的位置已經由 `composer_top` 找到時，
    /// 回覆一路收到狀態列為止，不能在第一個 `╭` 就被截掉（codex 那條有同一個 fixture，`codex_reply_keeps_mermaid_box_drawing_lines`）。
    #[test]
    fn a_rounded_box_diagram_in_the_reply_does_not_end_it() {
        let screen = format!(
            "❯ 畫流程\n⏺ 流程如下：\n\n  ╭────────╮\n  │ 收到請求 │\n  ╰────┬───╯\n       │\n  ╭────▼────╮\n  │ 判斷流程 │\n  ╰─────────╯\n\n圖後文字仍屬於回覆。\n{COMPOSER}"
        );
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("│ 判斷流程 │"), "圖被第一個 ╭ 截掉：{reply}");
        assert!(reply.contains("圖後文字仍屬於回覆。"), "圖後面的文字被截掉：{reply}");
        assert!(!reply.contains("bypass permissions"), "輸入框以下的 chrome 不能進回覆：{reply}");
    }

    /// #762：回覆裡引了 shell 提示符行（`  ❯ npm test`，縮排在 `⏺` 區塊裡）不是使用者回音。真回音是第 0 欄的 `❯ `：
    /// 以前取「最後一個 `❯ ` 開頭的行」，起點落在引用行之後，回覆整段 `None`（備援存不到回覆）。
    #[test]
    fn a_prompt_line_quoted_inside_the_reply_is_not_the_echo() {
        let screen = format!("❯ 怎麼跑測試\n⏺ 執行：\n  ❯ npm test\n  PASS\n結論：全綠。\n{COMPOSER}");
        assert_eq!(
            ClaudeCapture.extract_reply(&screen).as_deref(),
            Some("執行：\n  ❯ npm test\n  PASS\n結論：全綠。"),
        );
        // activity 也從真回音起算：引用行之後才有的字不能讓前面的活動列消失。
        let busy = "❯ 跑\n✻ Cooking… (3s · ↓ 1 tokens)\n⏺ 結果：\n  ❯ ls\n";
        assert_eq!(ClaudeCapture.activity(busy).as_deref(), Some("Cooking… (3s · ↓ 1 tokens)"));
    }

    /// 輸入框那一行只有 `❯` 加空白（有的版本／寬度會在後面補空白）：不是回音，不能把起點推到畫面最底、
    /// 讓上面真正的回音與回覆整段消失（#762 審查；註解寫「內容要有字」，條件以前只看長度，空白也算有字）。
    #[test]
    fn a_composer_row_of_only_spaces_after_the_marker_is_not_the_echo() {
        for composer in ["❯ ", "❯   ", "❯                                        "] {
            let screen = format!("❯ 問題\n⏺ 答案\n{composer}\n");
            assert_eq!(ClaudeCapture.extract_reply(&screen).as_deref(), Some("答案"), "composer={composer:?}");
        }
    }

    /// 縮排的 `❯ ` 一律不是回音（排隊中的訊息、引用）：沒有第 0 欄回音時從頭看，最後一個 `⏺` 照舊是回覆。
    #[test]
    fn an_indented_prompt_row_never_counts_as_the_echo() {
        let screen = format!("⏺ 舊回覆\n  ❯ 排隊的新問題\n⏺ 新回覆\n{COMPOSER}");
        assert_eq!(ClaudeCapture.extract_reply(&screen).as_deref(), Some("新回覆"));
    }

    /// 回覆文字自己長得像函式呼叫（沒有 `⎿` 輸出跟在後面）不是工具呼叫。
    #[test]
    fn a_reply_that_mentions_a_call_is_not_a_tool_call() {
        let screen = format!("❯ 怎麼寫\n⏺ parse(input) 會回傳 Option，\n  記得處理 None。\n{COMPOSER}");
        let reply = ClaudeCapture.extract_reply(&screen).unwrap();
        assert!(reply.contains("記得處理 None。"), "{reply}");
    }
}

/// claude 2.1.287 在 **Linux** 的真畫面（2026-10-02，tmux 120x40、`--dangerously-skip-permissions`）：回覆／工具列的字頭是
/// `●`（U+25CF），macOS 才是 `⏺`（U+23FA）；工具列也改成「描述 ＋ `⎿  $ 指令`」。本機 Linux 與遠端 Linux 主機的 claude bot
/// 備援（終端快照）都吃這個畫面。
#[cfg(test)]
mod linux_2_1_287_screen_tests {
    use super::*;

    const IDLE: &str = include_str!("../lifecycle/fixtures/claude-2.1.287-linux-idle.txt");
    const WORKING: &str = include_str!("../lifecycle/fixtures/claude-2.1.287-linux-working.txt");
    const FINISHED: &str = include_str!("../lifecycle/fixtures/claude-2.1.287-linux-finished.txt");

    #[test]
    fn busy_only_while_the_spinner_row_is_up() {
        assert!(ClaudeCapture.still_busy(WORKING));
        assert!(!ClaudeCapture.still_busy(FINISHED));
        assert!(!ClaudeCapture.still_busy(IDLE));
    }

    #[test]
    fn an_empty_composer_awaits_input() {
        assert!(ClaudeCapture.awaits_input(IDLE));
        assert!(ClaudeCapture.awaits_input(FINISHED));
    }

    /// 回覆是 `● PONG`；完成行 `✻ Churned for 9s · done 3:03 PM` 不進回覆。
    #[test]
    fn the_reply_is_read_after_a_dot_marker() {
        assert_eq!(ClaudeCapture.extract_reply(FINISHED).as_deref(), Some("PONG"));
    }

    /// 回合中畫面底下只有「● Sleeping for six seconds / ⎿  $ sleep 6」這個工具列（新的「描述＋指令」寫法，沒有 `Bash(…)`）：
    /// 照「整回合只有工具呼叫就取最後一個」的老規則，不會把 spinner 或輸入框 chrome 帶進來。
    #[test]
    fn a_running_tool_row_does_not_pull_in_the_spinner_or_the_composer() {
        let reply = ClaudeCapture.extract_reply(WORKING).unwrap();
        assert_eq!(reply, "Sleeping for six seconds\n  ⎿  $ sleep 6");
    }

    #[test]
    fn activity_is_the_spinner_row() {
        assert_eq!(ClaudeCapture.activity(WORKING).as_deref(), Some("Frosting… (2s · ↓ 77 tokens)"));
    }
}
