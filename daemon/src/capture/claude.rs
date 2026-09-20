use super::{Capture, ACTIVITY_MAX};

pub static PARSER: ClaudeCapture = ClaudeCapture;

pub struct ClaudeCapture;

impl Capture for ClaudeCapture {
    fn still_busy(&self, screen: &str) -> bool {
        screen.lines().any(is_spinner_line)
    }

    fn awaits_input(&self, screen: &str) -> bool {
        screen.lines().rev().take(12).any(|l| {
            let mut chars = l.chars().filter(|c| !"│┃╭╮╰╯─━ \t".contains(*c));
            chars.next() == Some('❯') && chars.next().is_none()
        })
    }

    fn extract_reply(&self, text: &str) -> Option<String> {
        let marker = "⏺ ";
        let lines: Vec<&str> = text.lines().collect();
        let after_echo = after_last_prompt_echo(&lines);
        let start = after_echo
            + lines[after_echo..]
                .iter()
                .rposition(|l| l.trim_start().starts_with(marker))?;
        let mut out: Vec<String> = Vec::new();
        for line in &lines[start..] {
            let t = line.trim_end();
            let s = t.trim_start();
            if s.starts_with('╭') || s.starts_with('│') || s.starts_with('╰') || s.starts_with('▔')
            {
                break;
            }
            if !s.is_empty()
                && s.chars()
                    .all(|c| c == '─' || c == '━' || c == '-' || c == '=' || c == '_')
            {
                break;
            }
            if spinner_led(s) || self.noise_line(s) {
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

fn is_spinner_line(s: &str) -> bool {
    let s = s.trim();
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_spinner_glyph(first) {
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
    lines
        .iter()
        .rposition(|l| {
            let t = l.trim_start();
            t.starts_with("❯ ") && t.len() > 3
        })
        .map(|i| i + 1)
        .unwrap_or(0)
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
    if is_update_banner(s) {
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
        || s.contains("shift+tab to cycle")
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
