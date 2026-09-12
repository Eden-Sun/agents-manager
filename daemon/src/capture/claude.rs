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
            if s.chars().next().map(is_spinner_glyph).unwrap_or(false) || self.noise_line(s) {
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
            let rest = if is_spinner_glyph(first) {
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

fn is_noise(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.chars().next().unwrap_or(' ');
    if "▐▝▛▜█╭╮╰╯│▔⏵⚠✗✘".contains(first) || is_spinner_glyph(first) {
        return true;
    }
    if s.contains("Auto-update failed") {
        return true;
    }
    if s.chars()
        .all(|c| c == '─' || c == '━' || c == '-' || c == '=' || c == '_' || c == ' ')
    {
        return true;
    }
    if (s.contains(" | ") && (s.contains("5h:") || s.contains("7d:")))
        || (s.contains(" · ") && s.contains("left"))
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
}
